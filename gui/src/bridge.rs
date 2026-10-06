//! GUI ↔ Agent 桥接层。
//!
//! 设计要点：**完全复用既有架构**。
//! - 后台线程跑一个 tokio 运行时，用 `aox::agent::Agent` 原封不动的循环驱动任务；
//! - `GuiUi` 实现 `aox::ui::Ui` trait，把 `AgentEvent`、状态、流式思考过程转发给 egui 主线程；
//! - 用户输入、授权决定、中断信号通过 channel 反向传回。
//!
//! 因此 CLI 与 GUI 共享同一套 Agent / Provider / 插件宿主实现，没有任何逻辑分叉。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use aox::agent::Agent;
use aox::history::UsageRecord;
use aox::llm::{LlmProvider, StreamSink};
use aox::runtime::Runtime;
use aox::ui::{AgentEvent, AuthDecision, ChoiceReply, Ui};

/// 主线程 → Agent 线程
pub enum ToAgent {
    /// 一条用户指令 + 这条指令带的附件（本机路径）。
    ///
    /// 附件只跟着**这一条**消息走（一次性）：后续轮次里模型如果还需要那份文件，
    /// 顺着消息里的路径引用自己读（`read_file`）—— 把附件的字节留在上下文里，
    /// 每一轮都要为它付一次 token。
    Input(String, Vec<PathBuf>),
    /// 切换到某段历史对话（`id` 即文件名主干）。
    ///
    /// 切换是「就地换掉上下文」，所以必须回到 Agent 所在的线程做，
    /// 不能由主线程自己读了文件再塞过来。
    Resume(String),
    /// 开一段新对话。旧对话不需要任何收尾动作 —— 它已经边聊边写进 `history/` 了。
    NewConversation,
    /// 本轮产生的活动流记录，追加进同一个对话文件（`kind:"act"` 行）。
    ///
    /// 走这条队列而不是让主线程自己开文件：句柄在 Agent 线程手里，而且「新建对话」
    /// 与「中断回滚」也在那个线程里发生 —— 队列天然保住了写入顺序。
    Act(Vec<Value>),
    /// 工作区：切档位（输入框左下角那枚按钮，见 `gui/src/workplace_ui.rs`）。
    ///
    /// 与 `Input` 一样必须**回到 Agent 所在线程**才做得了 —— `Workplace` 住在 `Runtime`
    /// 里，只有这个线程拿得到它；界面不能自己调 `set_mode`，只能把请求发过来。
    ///
    /// 只带「要切成哪一档」：界面不需要主动问现状，因为启动 / 每轮结束 / 每次切档后
    /// bridge 都会主动推一份 [`FromAgent::Workplace`]。
    Workplace(aox::workplace::Mode),
    /// 退出：关闭插件子进程并结束驱动线程
    Shutdown,
}

/// Agent 线程 → 主线程
pub enum FromAgent {
    /// 启动信息（配置 / 模型 / 路径 / 是否可用）
    Boot(Box<BootInfo>),
    /// 核心事件流（与 CLI 完全同源）
    Event(AgentEvent),
    /// 一轮任务开始
    TurnStart,
    /// 一轮任务结束（正常）
    TurnEnd,
    /// 一轮任务的 token 账 —— 就是落进对话文件的那一行（排在 `TurnEnd` 之后）
    Usage(Box<UsageRecord>),
    /// 整段对话的用量账本（切对话 / 新建对话后重发）。
    ///
    /// **必须排在 `Switched` 之后发**：主线程收到 `Switched` 会把账本清空，
    /// 顺序反了的话，刚接回来的数据会被自己抹掉。
    UsageLog(Vec<UsageRecord>),
    /// 一轮任务被中断
    TurnAborted,
    /// 状态栏文字（思考中 / 执行 xx）
    Status(String),
    /// 流式思考增量
    Reasoning(String),
    /// 一次响应的思考流结束（即时消失）
    ReasoningEnd,
    /// 流式正文增量。正文本身走 `Event(AssistantText)`，这条**只为实时 token 计数** ——
    /// 主线程需要知道「这一瞬间又吐出来多少字」，才能让数字一直在涨。
    Content(String),
    /// 请求授权（阻塞等待主线程回填）
    AuthRequest { tool: String, args: String, reply: oneshot::Sender<AuthDecision> },
    /// 一道「选择题」（`ask_user`）：同样阻塞等主线程回填。
    ///
    /// 与 `AuthRequest` 分成两条而不是合成一条：它们的**界面形态**与**回答类型**都不同
    ///（授权是「允许/拒绝」的确认框，选项是「挑一个」的一排按钮），合并只会让两边
    /// 都要在参数里判断「这次是哪种」，而那正是最容易出错的地方。
    ChoiceRequest {
        question: String,
        options: Vec<String>,
        reply: oneshot::Sender<ChoiceReply>,
    },
    /// 工具 / 插件快照（周期性刷新，用于右侧面板实时展示「自我增长」）
    Snapshot(Snapshot),
    /// 插件运行时状态（供「关于」窗口展示）；装配完成后会重发
    Runtimes(RuntimeView),
    /// 本次会话正在写的那个对话文件 id（还没开始对话时为 None）。
    ///
    /// 主线程据此在历史列表里标出「当前」，并在需要时重新扫描目录 ——
    /// 历史文件本身由主线程直接读盘，不经过这条 channel（见 `aox::history`）。
    HistoryCurrent(Option<String>),
    /// 会话已切换（打开历史对话 / 新建对话）。
    ///
    /// `opened` 是切换后的对话 id（新建对话时为 `None`）；`error` 非空表示
    /// 载入失败 —— 此时 Agent 的上下文**没有被动过**，主线程应当什么都不改。
    ///
    /// 只带 id 不带消息：载入后的内容由主线程自己读盘重建（见上一条说明）。
    Switched { opened: Option<String>, error: Option<String> },
    /// 工作区现状（档位 + 当前生效的根）。
    ///
    /// 启动、每轮结束、以及任何一次切档之后都会回执一份：界面要它做两件事 ——
    /// 按钮上显示当前档位名、三选一列表里给当前档位打勾。
    /// 用结构体而不是一行文本：打勾要靠 `PartialEq` 比档位，解析「选择工作区」这种
    /// 字符串太脆（文案一改就静默失效），理由与 [`RuntimeView`] 一致。
    Workplace(Box<WorkplaceView>),
    /// 本轮出现不可恢复错误
    Fatal(String),
}

/// 工作区现状（见 [`FromAgent::Workplace`]）。
#[derive(Debug, Clone)]
pub struct WorkplaceView {
    pub mode: aox::workplace::Mode,
    /// 当前生效的根。`full` 档、以及 `default` 档**还没开地**时都是 `None`
    /// —— 后者不是「不受限」，是「这条对话还没开始」，界面别把两者说成一回事。
    pub root: Option<PathBuf>,
}

/// 把当前工作区现状发给界面。
///
/// 一次取锁同时拿档位与根（[`aox::workplace::Workplace::snapshot`]），与 CLI 的
/// `/workplace` 读的是同一份状态 —— 两个前端不各留一套「现在是什么档」的判断。
fn sync_workplace(tx: &mpsc::UnboundedSender<FromAgent>, rt: &Runtime) {
    let (mode, root) = rt.workplace.snapshot();
    let _ = tx.send(FromAgent::Workplace(Box::new(WorkplaceView { mode, root })));
}

/// 处理界面发来的工作区请求（输入框左下角那枚按钮，见 `gui/src/workplace_ui.rs`）。
///
/// 与 CLI 的 `/workplace` 是**同一条实现**（[`aox::workplace::Workplace::set_mode`]），
/// 这里只决定「结果写到哪」：切档成功/失败进活动流（用户看得见的反馈），
/// 随后无论成败都补一份现状回执 —— 按钮上的档位名与三选一列表里的勾立即跟上，
/// 不用等下一轮结束。
fn set_workplace(ui: &mut GuiUi, rt: &Arc<Runtime>, mode: aox::workplace::Mode) {
    // 先取名字：`set_mode` 会把 `mode` 收走，而报错文案里要用它。
    let label = mode.label();
    match rt.workplace.set_mode(mode, &rt.root) {
        Ok(msg) => ui.emit(AgentEvent::Info(msg)),
        Err(e) => ui.emit(AgentEvent::Error(format!("切换工作区失败（{label}）: {e:#}"))),
    }
    sync_workplace(&ui.tx, rt);
}

pub struct BootInfo {
    pub config_path: String,
    /// 合成这份配置用到的层（低 → 高，后面的覆盖前面的）。
    ///
    /// 只给「关于」窗口显示一行用；完整的来源树走 CLI 的 `--dump-config` ——
    /// 380px 的小卡片塞不下那张表，塞进去也只是没人看的噪音。
    pub config_layers: Vec<String>,
    pub model: String,
    pub provider_ready: bool,
    pub note: Option<String>,
    /// 「系统」选项卡要的那几处路径与主机信息（见 [`BootPaths`]）。
    pub paths: BootPaths,
    /// 界面侧读历史用的存储实例（只读用途）。
    ///
    /// 与 agent 线程**同一个后端实现**、指向同一处数据 —— 界面不自己拼路径，
    /// 也不自己判断落盘格式（见 `aox::store`）。正常启动时就是 `rt.store` 的克隆；
    /// 配置读不出来时用默认后端兜底，至少让侧边栏里的旧对话还看得见。
    pub store: Arc<dyn aox::store::Store>,
}

/// 「系统」选项卡里那几处路径与主机信息。
///
/// 为什么合成一个结构体而不是给 `BootInfo` 再加七八个 `String` 字段：这些值同生共死
/// （都来自同一个 `Runtime`），而且**将来还会加**（比如日志目录）。打包之后
/// `boot()` 的签名不会随着「再加一行显示项」继续变长，三个调用点也只需改一处构造。
#[derive(Debug, Clone, Default)]
pub struct BootPaths {
    /// 应用根目录（配置 / 插件 / 历史 / 运行时都挂在它下面）。
    pub root: String,
    /// 配置文件（合成链的最后一层）。
    pub config: String,
    /// 插件目录。
    pub plugins: String,
    /// 历史记录目录（`<root>/history`，由存储后端自己报，界面不拼）。
    pub history: String,
    /// 内置运行时目录（`<root>/runtime`）—— 没装任何内置运行时时它也在，只是空的。
    pub runtime: String,
    /// 主机信息，例如 `windows x86_64 · 16 核 · 31.7 GB 内存`。
    pub host: String,
}

impl BootPaths {
    /// 从几处路径装配一份（主机信息自己去问核心库）。
    ///
    /// 三个启动分支都走它 —— 包括「配置读不出来」与「模型没就绪」两个降级分支：
    /// 那些分支拿不到完整 `Runtime`，但**路径是知道的**，而「配置文件到底在哪」
    /// 恰恰是那时候最该显示的东西（用户就是来排查它为什么没读起来）。
    pub fn new(
        root: &std::path::Path,
        config: &std::path::Path,
        plugins: &std::path::Path,
        history: std::path::PathBuf,
    ) -> Self {
        Self {
            root: root.display().to_string(),
            config: config.display().to_string(),
            plugins: plugins.display().to_string(),
            history: history.display().to_string(),
            runtime: root.join("runtime").display().to_string(),
            host: aox::sysinfo::host_summary(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub plugins: Vec<String>,
}

/// 一门插件运行时的状态（「系统」选项卡里的一行）。
///
/// 结构化而不是预先拼好的字符串：CLI 要的是一行紧凑文本（`runtimes::one_line`），
/// 而设置页要的是「语言 / 解析到的命令 / 来源」三列 —— 同一份数据两种排法。
/// 拼成字符串再拆回来是自找麻烦，而这行文字本来就是给人看的，拆不动。
#[derive(Debug, Clone, Default)]
pub struct RuntimeItem {
    pub lang: String,
    /// 最终使用的命令（绝对路径或裸命令）
    pub command: String,
    /// 来源：`config.toml 指定` / `应用目录内置` / `PATH` / `未找到`
    pub origin: String,
    pub ok: bool,
}

/// 运行时状态视图。
///
/// 刻意做成结构体而不是几行字符串：界面要据此决定「要不要给安装按钮」，
/// 靠解析 `✗ python` 这种文本判断太脆（文案一改就静默失效）。
#[derive(Debug, Clone, Default)]
pub struct RuntimeView {
    /// 每个语言一行（结构化，见 [`RuntimeItem`]）。
    pub items: Vec<RuntimeItem>,
    /// Python 是否缺失——决定设置页是否显示一键安装入口
    pub python_missing: bool,
}

/// 跨线程的中断标志。用轮询而不是 Notify，避免「通知早于等待」导致的陈旧许可问题。
pub struct SessionFlags {
    abort: AtomicBool,
}

impl SessionFlags {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { abort: AtomicBool::new(false) })
    }

    pub fn request_abort(&self) {
        self.abort.store(true, Ordering::SeqCst);
    }

    fn clear(&self) {
        self.abort.store(false, Ordering::SeqCst);
    }
}

async fn wait_abort(flags: Arc<SessionFlags>) {
    loop {
        if flags.abort.swap(false, Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 把 Provider 的流式思考过程转发给 GUI。
struct ChanSink {
    tx: mpsc::UnboundedSender<FromAgent>,
}

impl StreamSink for ChanSink {
    fn on_reasoning(&self, delta: &str) {
        let _ = self.tx.send(FromAgent::Reasoning(delta.to_string()));
    }

    fn on_content(&self, delta: &str) {
        let _ = self.tx.send(FromAgent::Content(delta.to_string()));
    }

    fn on_finish(&self) {
        let _ = self.tx.send(FromAgent::ReasoningEnd);
    }
}

/// `aox::ui::Ui` 的图形实现。
pub struct GuiUi {
    tx: mpsc::UnboundedSender<FromAgent>,
    rx: mpsc::UnboundedReceiver<ToAgent>,
}

#[async_trait]
impl Ui for GuiUi {
    /// GUI 的取指令路径是 `next_command`（它还要就地处理「切换会话」），
    /// 所以这里实际上不会被调到；保留实现只为满足 trait。
    async fn read_user_input(&mut self) -> Result<Option<String>> {
        loop {
            match self.rx.recv().await {
                Some(ToAgent::Input(s, files)) => {
                    // GUI 真正取指令的是 `next_command`（见它的文档），这条 trait 实现只
                    // 为满足签名而存在。真走到这里就**出声**——静默丢掉附件，等哪天这条路
                    // 变活了会得到一个「图明明拖了却没发出去」的幽灵 bug。
                    if !files.is_empty() {
                        let _ = self.tx.send(FromAgent::Event(AgentEvent::Warn(format!(
                            "内部错误：这条路径不支持附件，{} 个附件未随消息发出。",
                            files.len()
                        ))));
                    }
                    return Ok(Some(s));
                }
                Some(ToAgent::Shutdown) | None => return Ok(None),
                Some(ToAgent::Resume(_))
                | Some(ToAgent::NewConversation)
                | Some(ToAgent::Act(_))
                | Some(ToAgent::Workplace(_)) => continue,
            }
        }
    }

    fn emit(&mut self, event: AgentEvent) {
        let _ = self.tx.send(FromAgent::Event(event));
    }

    async fn authorize(&mut self, tool_name: &str, args: &Value) -> AuthDecision {
        let (reply, rx) = oneshot::channel();
        let pretty = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
        let sent = self.tx.send(FromAgent::AuthRequest {
            tool: tool_name.to_string(),
            args: pretty,
            reply,
        });
        if sent.is_err() {
            return AuthDecision::Deny;
        }
        rx.await.unwrap_or(AuthDecision::Deny)
    }

    /// `ask_user` 的图形版：把问题与选项发给主线程，画成输入框上方那排按钮，等回填。
    ///
    /// 三条失败路径（发不出去 / 主线程没回 / 回执通道断了）一律回 `Cancelled`：
    /// 那比「默认选第一个」安全得多 —— 前者只是没答，模型会按它自己的话往下走并说明，
    /// 后者是**替用户做了一个他没做过的选择**，而谁都不会发现。
    async fn choose(&mut self, question: &str, options: &[String]) -> ChoiceReply {
        let (reply, rx) = oneshot::channel();
        let sent = self.tx.send(FromAgent::ChoiceRequest {
            question: question.to_string(),
            options: options.to_vec(),
            reply,
        });
        if sent.is_err() {
            return ChoiceReply::Cancelled;
        }
        rx.await.unwrap_or(ChoiceReply::Cancelled)
    }

    async fn turn_start(&mut self) {
        let _ = self.tx.send(FromAgent::TurnStart);
    }

    async fn turn_end(&mut self) {
        let _ = self.tx.send(FromAgent::TurnEnd);
    }

    async fn turn_usage(&mut self, rec: &UsageRecord) {
        let _ = self.tx.send(FromAgent::Usage(Box::new(rec.clone())));
    }

    async fn set_status(&mut self, label: &str) {
        let _ = self.tx.send(FromAgent::Status(label.to_string()));
    }

    fn stream_sink(&self) -> Option<Box<dyn StreamSink + Send + Sync + '_>> {
        Some(Box::new(ChanSink { tx: self.tx.clone() }))
    }
}

/// 把运行时状态发给「关于」窗口。
///
/// 传**原始** `config.runtimes`（而不是解析后的快照），否则「来源」会被误判成
/// 「config.toml 指定」——见 `aox::runtimes::status` 的说明。
fn send_runtime_status(tx: &mpsc::UnboundedSender<FromAgent>, rt: &Runtime) {
    let st = aox::runtimes::status(&rt.root, &rt.config.runtimes);
    let view = RuntimeView {
        items: st
            .iter()
            .map(|s| RuntimeItem {
                lang: s.lang.key().to_string(),
                command: s.command.clone(),
                origin: s.origin.label().to_string(),
                ok: s.ok(),
            })
            .collect(),
        python_missing: st
            .iter()
            .any(|s| s.lang == aox::runtimes::Lang::Python && !s.ok()),
    };
    let _ = tx.send(FromAgent::Runtimes(view));
}

/// `/runtime` 的 GUI 实现。
///
/// 解析、装配、文案全部复用 `aox::runtimes`，与 CLI 同源；这里只决定「写到哪」：
/// 进度与结果都进活动流，这样 10 MB 的下载过程不会让界面看起来卡死。
async fn runtime_command(ui: &mut GuiUi, rt: &Arc<Runtime>, line: &str) {
    let parsed = match aox::runtimes::parse_cmd(line) {
        Ok(c) => c,
        Err(e) => {
            ui.emit(AgentEvent::Error(format!("{e:#}")));
            return;
        }
    };
    match parsed {
        aox::runtimes::Cmd::Status => {
            ui.emit(AgentEvent::Info(aox::runtimes::status_report(
                &rt.root,
                &rt.config.runtimes,
            )));
        }
        aox::runtimes::Cmd::Dir => {
            ui.emit(AgentEvent::Info(rt.root.join("runtime").display().to_string()));
        }
        aox::runtimes::Cmd::Install(lang) => {
            let tx = ui.tx.clone();
            let mut sink = |m: &str| {
                let _ = tx.send(FromAgent::Event(AgentEvent::Info(m.to_string())));
            };
            match aox::runtimes::install(rt, lang, &mut sink).await {
                Ok(msg) => {
                    ui.emit(AgentEvent::Info(msg));
                    send_runtime_status(&ui.tx, rt);
                }
                Err(e) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
            }
        }
    }
}

fn boot(
    tx: &mpsc::UnboundedSender<FromAgent>,
    config_path: &std::path::Path,
    model: &str,
    ready: bool,
    note: Option<String>,
    store: Arc<dyn aox::store::Store>,
    // 这份配置由哪几层合成（低 → 高）。读不出配置时给空表 —— 不编造不存在的层。
    layers: Vec<String>,
    paths: BootPaths,
) {
    let _ = tx.send(FromAgent::Boot(Box::new(BootInfo {
        config_path: config_path.display().to_string(),
        config_layers: layers,
        model: model.to_string(),
        provider_ready: ready,
        note,
        paths,
        store,
    })));
}

/// 三个启动分支都要的那份路径清单。
///
/// 历史目录问存储后端要（`Store::location`）—— 界面不自己拼 `<root>/history`：
/// 换了后端（这是存储缝存在的意义）之后，拼出来的路径就是假的，而这里会跟着变。
fn boot_paths(
    root: &std::path::Path,
    config: &std::path::Path,
    plugins: &std::path::Path,
    store: &Arc<dyn aox::store::Store>,
) -> BootPaths {
    BootPaths::new(root, config, plugins, store.location().to_path_buf())
}

/// 把「本次会话正在写的对话」告诉主线程。
///
/// 每轮结束后调一次：id 第一次出现时主线程会把新对话加进侧边栏，
/// 之后同一 id 的重复通知则让主线程知道「这一条的内容变了，条数该刷新了」。
fn send_history_current(tx: &mpsc::UnboundedSender<FromAgent>, agent: &Agent) {
    let _ = tx.send(FromAgent::HistoryCurrent(agent.history_id().map(str::to_string)));
}

/// 启动受挫时（配置读不出来 / 模型没就绪）的「只等退出」循环。
///
/// 必须回应切换会话的请求：主线程发出请求后会先把列表锁灰、等回执解锁，
/// 而这几条消息在这个状态下注定做不成事 —— 沉默就等于把侧边栏永久冻住。
async fn idle_until_shutdown(
    rx: &mut mpsc::UnboundedReceiver<ToAgent>,
    tx: &mpsc::UnboundedSender<FromAgent>,
) {
    while let Some(msg) = rx.recv().await {
        match msg {
            ToAgent::Shutdown => break,
            ToAgent::Resume(_) | ToAgent::NewConversation => {
                let _ = tx.send(FromAgent::Switched {
                    opened: None,
                    error: Some("模型未就绪，暂时无法切换对话".into()),
                });
            }
            ToAgent::Input(..) => {
                let _ = tx.send(FromAgent::Event(AgentEvent::Warn(
                    "模型未就绪，这条指令没有发出。".into(),
                )));
            }
            // 这一段里没有 log 可写（Agent 根本没起来），如实丢掉
            ToAgent::Act(_) => {}
            // 工作区按钮这时也按不动 —— `Workplace` 住在 `Runtime` 里，而 Runtime 没起来。
            // 出声回一句，别让按钮看起来「点了没反应」（与上面 Input 那条同一个理由）。
            ToAgent::Workplace(_) => {
                let _ = tx.send(FromAgent::Event(AgentEvent::Warn(
                    "模型未就绪，工作区暂时无法切换。".into(),
                )));
            }
        }
    }
}

/// 取下一个用户指令，期间顺手处理「切换会话」这类控制消息。
///
/// 这里没有走 `Ui::read_user_input()`：那个方法只能吐回用户文本，而切换会话
/// 必须**就地**改动 `agent`，只有这个线程拿得到它。通道还是同一条，
/// 顺序自然也就和用户的操作顺序一致。
///
/// 返回值带上附件路径：附件要在**Agent 线程**里组进消息（`Message::user_with_files`），
/// 界面侧不能自己拼 —— 否则「内存里有、历史文件里没有」那种错位就回来了。
async fn next_command(
    ui: &mut GuiUi,
    agent: &mut Agent,
    tx: &mpsc::UnboundedSender<FromAgent>,
) -> Result<Option<(String, Vec<PathBuf>)>> {
    loop {
        match ui.rx.recv().await {
            Some(ToAgent::Input(s, files)) => return Ok(Some((s, files))),
            Some(ToAgent::Resume(id)) => {
                // 按 id 让存储缝去开那次对话 —— 界面侧不拼路径（见 `src/store.rs`）。
                let msg = match agent.resume(&id) {
                    Ok(_) => FromAgent::Switched { opened: Some(id), error: None },
                    Err(e) => FromAgent::Switched { opened: None, error: Some(format!("{e:#}")) },
                };
                let _ = tx.send(msg);
                // 账本紧跟 Switched 发出：主线程收到 Switched 会先把界面上的账清空，
                // 顺序反了的话刚接回来的数据会被自己抹掉。
                let _ = tx.send(FromAgent::UsageLog(agent.usage_log().to_vec()));
                // 工作区也要跟着推：每段对话有自己的档位与根（`resume` 已经把存档里的那份
                // 还原了）。不推的话按钮上会一直挂着**上一段对话**的档位 —— 这就是实机撞出来的
                // 「打开旧对话，工作区还是上一个」。
                sync_workplace(&tx, agent.rt());
            }
            Some(ToAgent::NewConversation) => {
                agent.new_conversation();
                let _ = tx.send(FromAgent::Switched { opened: None, error: None });
                let _ = tx.send(FromAgent::UsageLog(Vec::new()));
                // 新对话一律回到「默认」档（`new_conversation` 里已经切了），界面必须立刻跟上，
                // 否则按钮继续显示上一段对话的档位。
                sync_workplace(&tx, agent.rt());
            }
            // 活动流落盘：就地进行，不进下一轮 —— 顺序与用户的操作完全一致，
            // 所以「这一轮的时间轴」永远落在这一轮的对话文件里。
            Some(ToAgent::Act(acts)) => agent.append_acts(&acts),
            // 工作区按钮：切档 / 查现状。就地做完 —— `Workplace` 住在 `Runtime` 里，
            // 而这个线程是唯一拿得到 Agent（进而拿得到它的 `rt`）的地方。
            // 不进下一轮：切档是纯宿主动作，与模型对话无关。
            Some(ToAgent::Workplace(mode)) => {
                let rt = agent.rt().clone();
                set_workplace(ui, &rt, mode);
                // 切档也要记进**本对话**的存档：工作区是跟着对话走的，切完不记的话
                // 下次打开这段对话就回到默认那块地了。`persist_workplace` 自己比对过、
                // 没变就不写，所以失败/无变化时都不会留下垃圾行。
                agent.persist_workplace();
            }
            Some(ToAgent::Shutdown) | None => return Ok(None),
        }
    }
}

/// 在独立线程里启动：建 tokio 运行时 → 装载运行时 → 跑驱动循环。
pub fn spawn(
    tx: mpsc::UnboundedSender<FromAgent>,
    rx: mpsc::UnboundedReceiver<ToAgent>,
    flags: Arc<SessionFlags>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("aox-agent".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(FromAgent::Fatal(format!("无法创建异步运行时: {e}")));
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(e) = boot_and_run(tx.clone(), rx, flags).await {
                    let _ = tx.send(FromAgent::Fatal(format!("{e:#}")));
                }
            });
        })
        .expect("无法启动 agent 线程")
}

async fn boot_and_run(
    tx: mpsc::UnboundedSender<FromAgent>,
    mut rx: mpsc::UnboundedReceiver<ToAgent>,
    flags: Arc<SessionFlags>,
) -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (root, default_plugins) = aox::paths::resolve_workplace();
    let plugins_dir = aox::paths::arg_value(&args, "--plugins")
        .map(PathBuf::from)
        .unwrap_or(default_plugins);
    let config_path = aox::paths::resolve_config_path(&args);

    if let Err(e) = aox::plugin::ensure_sdk(&plugins_dir) {
        let _ = tx.send(FromAgent::Event(AgentEvent::Warn(format!(
            "插件 SDK 写入失败（{:#}）：插件功能可能不可用",
            e
        ))));
    }

    // 配置走**分层合成**（阶段 5）：profile → bundle → patch → config.toml。
    // 与 CLI 是同一条 `aox::config::layer::resolve` —— 两个前端不各留一套合成规则。
    let resolved = match aox::config::layer::resolve(
        &root,
        &config_path,
        aox::paths::resolve_manifest_path(&args).as_deref(),
        aox::paths::resolve_profile(&args).as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => {
            // 配置缺失/无效：GUI 照常打开，展示引导信息，输入区会被禁用。
            // 读历史的后端用默认的（此时还没读到配置），免得侧边栏跟着一起空掉。
            let store = aox::store::default_store(&root, 0);
            let paths = boot_paths(&root, &config_path, &plugins_dir, &store);
            boot(
                &tx,
                &config_path,
                "",
                false,
                Some(format!("{e:#}")),
                store,
                Vec::new(),
                paths,
            );
            idle_until_shutdown(&mut rx, &tx).await;
            return Ok(());
        }
    };
    let config_layers = resolved.composed.layer_labels();
    let config = resolved.config;

    let model = config.provider.model.clone();
    let rt = Runtime::new(config, root.clone(), plugins_dir.clone())?;
    for t in aox::tool::meta::meta_tools() {
        rt.register(t).await?;
    }
    // 宿主文件工具（工作区边界，见 src/workplace.rs 与 src/tool/hostfs.rs）。
    // 与 CLI 注册的是同一组、同一份实现；默认 `[workplace] mode = "full"` 时
    // 它们的行为与普通读写工具无异 —— 边界只在你收紧档位之后才生效。
    for t in aox::tool::hostfs::host_file_tools() {
        rt.register(t).await?;
    }
    // 交互工具（`ask_user`）：让模型把「选择题」交给界面，而不是写在正文里
    // 等用户打字。注册在这里的意义是**让模型在 tools 列表里看得见它**。
    for t in aox::tool::ask::ask_tools() {
        rt.register(t).await?;
    }

    let mut ui = GuiUi { tx: tx.clone(), rx };

    // 运行时状态：一条给「关于」窗口，一条缺失提示进活动流（与 CLI 同一份文案）。
    // 这样用户在 GUI 里也能立刻知道「本机缺 python、该点哪里装」，而不是等到
    // 模型建插件失败才发现。
    send_runtime_status(&tx, &rt);
    if let Some(hint) = aox::runtimes::missing_hint(&rt.root, &rt.config.runtimes) {
        ui.emit(AgentEvent::Warn(hint));
    }

    // 恢复持久化插件（与 CLI 同一路径）。
    // 成功保持静默——启动时不该往活动流里刷一屏「插件 xxx 已恢复」，
    // 那会把真正要看的信息挤下去。只有失败才出声。
    for (name, result) in aox::plugin::load_all(&rt).await {
        if let Err(e) = result {
            ui.emit(AgentEvent::Warn(format!("插件 {name} 加载失败: {e:#}")));
        }
    }
    // 围栏没套上的插件也要出声（与 CLI 共用同一份判定与文案）。
    // `围栏 ✗` 不是致命错误，但「以为有保护、其实没有」值得用户看一眼。
    let unfenced = aox::sandbox::unfenced(&rt).await;
    if !unfenced.is_empty() {
        ui.emit(AgentEvent::Warn(format!(
            "以下插件没套上进程围栏，它们不受进程数 / 内存 / UI 限制：{}",
            unfenced.join("；")
        )));
    }

    let llm: Arc<dyn LlmProvider> = match aox::llm::openai::OpenAiProvider::new(&rt.config.provider) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            ui.emit(AgentEvent::Error(format!("LLM 未就绪: {e:#}")));
            ui.emit(AgentEvent::Info(
                "请在 config.toml 的 [provider] 填写 base_url 与 api_key（或 api_key_env）后重新启动。".into(),
            ));
            let paths = boot_paths(&root, &config_path, &plugins_dir, &rt.store);
            boot(
                &tx,
                &config_path,
                &model,
                false,
                Some(format!("{e:#}")),
                rt.store.clone(),
                config_layers.clone(),
                paths,
            );
            // 仍是「未就绪」状态，但工作区现状照样发一份：按钮若可见，至少显示的是真档位。
            sync_workplace(&tx, &rt);
            idle_until_shutdown(&mut ui.rx, &tx).await;
            return Ok(());
        }
    };

    // 项目规则文件（RULES.md）现在由 `aox::prompt::build` 统一读取，
    // 不再是 GUI 与 CLI 各读一遍（见 src/prompt.rs）。
    let mut agent = Agent::new(llm, rt.clone());
    let paths = boot_paths(&root, &config_path, &plugins_dir, &rt.store);
    boot(&tx, &config_path, &model, true, None, rt.store.clone(), config_layers, paths);
    send_history_current(&tx, &agent);
    // 启动就把工作区现状发过去：按钮上「工作区 · <档位>」的初值，以及三选一列表里
    // 该给哪一项打勾，都靠它。
    sync_workplace(&tx, &rt);

    // 周期性广播插件快照：模型每造出一个新插件，右侧面板立刻可见。
    // （工具名不再广播——顶栏的工具胶囊已去掉，没有消费方。）
    let snap_tx = tx.clone();
    let snap_rt = rt.clone();
    let snap_task = tokio::spawn(async move {
        loop {
            let plugins = {
                let p = snap_rt.plugins.lock().await;
                let mut v: Vec<String> = p.keys().cloned().collect();
                v.sort();
                v
            };
            if snap_tx.send(FromAgent::Snapshot(Snapshot { plugins })).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
    });

    // ---- 驱动循环：与 CLI 的 REPL 一一对应
    loop {
        let Some((input, files)) = next_command(&mut ui, &mut agent, &tx).await? else { break };
        let line = input.trim().to_string();
        // 只有附件、没有文字也是合法的一条消息（拖张图直接发送）
        if line.is_empty() && files.is_empty() {
            continue;
        }
        // 本地命令：/runtime 由宿主自己处理，不发给模型（与 CLI 一致）
        if aox::runtimes::is_cmd(&line) {
            runtime_command(&mut ui, &rt, &line).await;
            continue;
        }
        flags.clear();
        let mark = agent.messages_len();
        enum Outcome {
            Done(Result<()>),
            Aborted,
        }
        let outcome = {
            let turn = agent.run_turn(&mut ui, &line, &files);
            tokio::pin!(turn);
            tokio::select! {
                r = &mut turn => Outcome::Done(r),
                _ = wait_abort(flags.clone()) => Outcome::Aborted,
            }
        };
        match outcome {
            Outcome::Done(Ok(())) => {}
            Outcome::Done(Err(e)) => {
                let _ = ui.tx.send(FromAgent::Fatal(format!("{e:#}")));
            }
            Outcome::Aborted => {
                agent.truncate_messages(mark);
                let _ = ui.tx.send(FromAgent::TurnAborted);
            }
        }
        // 每轮结束后同步一次「当前对话」：第一轮会新建文件（侧边栏出现新条目），
        // 之后同一 id 的重复通知让侧边栏刷新条数。
        send_history_current(&tx, &agent);
        // 工作区也顺手同步一次：`default` 档的根是**首条消息时**才建的，所以
        // 「默认」档下按钮背后的目录是到这一轮结束才有值 —— 不重发的话界面会一直显示
        // 「根：还没定」。
        sync_workplace(&tx, &rt);
    }

    snap_task.abort();
    // 关闭所有插件子进程（与 CLI 退出路径一致）
    let handles: Vec<_> = rt.plugins.lock().await.values().cloned().collect();
    for h in handles {
        h.shutdown().await;
    }
    Ok(())
}
