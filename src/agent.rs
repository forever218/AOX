use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::attach;
use crate::auth::AuthLevel;
use crate::history::UsageRecord;
use crate::llm::{LlmProvider, Usage};
use crate::message::Message;
use crate::runtime::Runtime;
use crate::tool::{ToolKind, ToolOutput};
use crate::ui::{AgentEvent, AuthDecision, Ui};

/// 一轮任务的用量累加器。
///
/// 它是 `Agent` 的字段而不是某个函数的局部变量，原因有两条：
///
/// ① **已经花掉的 token 是收不回来的**：`AgentLoop::run` 可能中途 `bail!`（模型连续返回
///    空回复等），账留在 `Agent` 手上，出错时 `run_turn` 照样能如实报给用户，
///    而不是因为一个 `Err` 就把账丢了。
/// ② 循环已经搬到 `AgentLoop` 里去（阶段 6 的 6a），「谁在跑循环」与「账记在谁身上」
///    必须解耦 —— 循环通过 [`Agent::accrue_call`] 记一笔，由 [`Agent::finish_turn`] 收口。
#[derive(Default)]
struct TurnAcc {
    calls: u32,
    total: Usage,
    /// 最后一次请求的输入规模 = 当前上下文占用
    last_prompt: Option<u64>,
}

pub struct Agent {
    llm: Arc<dyn LlmProvider>,
    rt: Arc<Runtime>,
    messages: Vec<Message>,
    /// 本次对话的落盘器（存储缝的**写入端**）。
    ///
    /// 惰性创建：只有真正产生了用户消息才建文件，所以「启动 → /help → 退出」
    /// 这种会话不会在 history/ 里留下一堆空文件。
    ///
    /// 具体落到哪个文件、叫什么名字，这里一概不管 —— 那是 `self.rt.store` 的
    /// 实现细节（见 `src/store.rs`）。本类型只按 **id** 与它对话。
    log: Option<Box<dyn crate::store::Session>>,
    /// 落盘过程中的错误只报一次，避免每轮都刷同一句警告。
    log_error_reported: bool,
    /// 本次对话到目前的累计用量（切对话 / 新建对话时从文件接回或归零）
    session_usage: Usage,
    session_calls: u32,
    /// 当前对话**逐轮**的用量记录，与对话文件里的 `kind:"usage"` 行一一对应。
    ///
    /// 内存里留一份，是为了让界面立刻能答「这段对话一共花了多少、哪一轮最贵」，
    /// 而不必每帧读盘；文件那边才是权威 —— 重启后从这里接回来。
    usage_log: Vec<UsageRecord>,
    /// **本对话已经落盘过**的工作区记录（档位 + 根 + 本对话自建地）；`None` = 还没写过。
    ///
    /// 有了它才能「只在真的变了的时候补一条」：每轮无脑追加会让文件里堆满重复行，
    /// 而读的时候是取最后一条 —— 那种文件后来者根本看不出哪几行才算数。
    /// 新建对话 / 打开旧对话时重置，对齐那一刻文件里的实际情况。
    workplace_logged: Option<crate::workplace::Record>,
    /// 本轮的用量累加器（由 `AgentLoop` 通过 [`Agent::accrue_call`] 驱动）。
    ///
    /// 放在 `Agent` 里而不是循环的局部变量里：循环是可替换的，账不行 ——
    /// 换一个实现也得记在同一本账上（见 [`TurnAcc`] 的注释）。
    turn: TurnAcc,
}

impl Agent {
    /// 组装一个 Agent。
    ///
    /// 系统提示词由 [`crate::prompt::build`] 从「段」拼出来：内置段、运行环境、
    /// 项目规则文件、以及用户在 `config.toml [prompt]` 里的增删改，全都在那里合成。
    /// 这里**不再读 `RULES.md`** —— 那份「取规则文件」的逻辑原先在 CLI（`src/main.rs`）
    /// 与 GUI（`gui/src/bridge.rs`）各写了一遍，现在归到 `prompt` 一处。
    pub fn new(llm: Arc<dyn LlmProvider>, rt: Arc<Runtime>) -> Self {
        let sys = crate::prompt::build(&rt);
        Self {
            llm,
            rt,
            messages: vec![Message::system(sys)],
            log: None,
            log_error_reported: false,
            session_usage: Usage::default(),
            session_calls: 0,
            usage_log: Vec::new(),
            workplace_logged: None,
            turn: TurnAcc::default(),
        }
    }

    /// 本次对话的历史 id（还没开始对话时为 None）。
    /// id 由存储后端在新建时定下（jsonl 后端就是文件名主干），这里只是转述。
    pub fn history_id(&self) -> Option<&str> {
        self.log.as_ref().map(|l| l.id())
    }

    /// 把当前工作区状态记进**本对话的存档**（只在真的变了的时候写）。
    ///
    /// 调用点三处：每轮开头一次（首条消息把刚建好的那块地记下来）、CLI 的 `/workplace`
    /// 切档后、GUI 的工作区按钮切档后。
    ///
    /// **没有对话文件时直接返回** —— 那种情况等 `ensure_log` 建好文件、下一轮的
    /// 这一步再记，不会丢。
    pub fn persist_workplace(&mut self) {
        let Some(log) = self.log.as_mut() else { return };
        let cur = self.rt.workplace.persisted();
        if self.workplace_logged.as_ref() == Some(&cur) {
            return;
        }
        log.append_workplace(&cur);
        self.workplace_logged = Some(cur);
    }

    /// 确保本次对话已有落盘文件。失败只警告一次，不打断对话。
    fn ensure_log(&mut self, ui: &mut dyn Ui) {
        if self.log.is_some() {
            return;
        }
        let cfg = &self.rt.config.history;
        if !cfg.enabled {
            return;
        }
        match self.rt.store.create(Some(&self.rt.config.provider.model)) {
            Ok(log) => self.log = Some(log),
            Err(e) if !self.log_error_reported => {
                self.log_error_reported = true;
                ui.emit(AgentEvent::Warn(format!(
                    "对话历史无法写入 {}（{e:#}）：本次对话不会被保存",
                    self.rt.store.location().display()
                )));
            }
            Err(_) => {}
        }
    }

    /// 把一条用户输入（+ 附件路径）变成消息。
    ///
    /// 附件在这里、也**只在这里**成型：读盘、base64、组装 parts 全交给
    /// [`Message::user_with_files`]。GUI 与 CLI 都只把路径递过来，谁也不自己拼 ——
    /// 否则「内存里有、历史文件里没有」那种错位会从另一个入口回来。
    ///
    /// 附件出问题（读不到 / 超限 / 目录）时**不吞也不中断**：能挂的照挂，挂不上的
    /// 把那句理由拼在正文末尾。这样「附件少了一个」是**对话里看得见**的一件事 ——
    /// 只闪一个界面提示的话，模型与后翻历史的用户都不知道当时少了东西。
    fn user_message(&self, input: &str, files: &[PathBuf]) -> Message {
        if files.is_empty() {
            return Message::user(input);
        }
        let (items, problems) = attach::probe_all(files);
        let text = if problems.is_empty() {
            input.to_string()
        } else {
            format!(
                "{input}\n\n（宿主提示）以下附件没能附上：\n{}",
                problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n")
            )
        };
        match Message::user_with_files(&text, &items) {
            Ok(m) => m,
            Err(e) => Message::user(format!("{text}\n\n（宿主提示）附件没能附上：{e}")),
        }
    }

    /// 推入一条消息，并同步落盘。
    ///
    /// **所有**对 `messages` 的写入都必须走这里 —— 落盘与内存只要有一处不同步，
    /// 历史文件就不再是这段对话的忠实记录。这条约束连 `AgentLoop` 也不例外：
    /// 它是 `pub` 的，而 `messages` 只给只读视图（见下面的受控原语一节）。
    pub fn push_message(&mut self, msg: Message, is_error: Option<bool>) {
        if let Some(log) = self.log.as_mut() {
            log.append(&msg, is_error);
        }
        self.messages.push(msg);
    }

    /// 当前对话历史长度（用于中断时回滚）
    pub fn messages_len(&self) -> usize {
        self.messages.len()
    }

    /// 当前对话逐轮的用量记录（供界面显示「每轮各花了多少」）
    pub fn usage_log(&self) -> &[UsageRecord] {
        &self.usage_log
    }

    /// 追加上一批**活动流**记录（界面侧产生，写进同一份对话文件）。
    ///
    /// 为什么由这里写、而不是界面自己去开文件：文件句柄只有这个线程握着，
    /// 而且「新建对话」「中断回滚」也发生在这里 —— 多一个写入方就要多一套
    /// 跨线程的互斥与顺序保证，而这些本来靠线程内的一条队列就白拿了。
    ///
    /// `log` 为 `None` 时直接丢弃。正常情况下不会发生：活动流只在「一轮任务」
    /// 期间产生，而那一轮一开始就会 `ensure_log`。唯一的例外是配置里关掉了
    /// 历史（`[history] enabled = false`）—— 那本来就不该落盘。
    ///
    /// `allow(dead_code)`：`src/main.rs` 与 `src/lib.rs` 各自编译同一批模块
    /// （见 lib.rs 开头），而 CLI 那一份根本没有活动流，所以它眼里的这个方法是
    /// 死的。GUI 那一份在用。
    #[allow(dead_code)]
    pub fn append_acts(&mut self, acts: &[Value]) {
        let Some(log) = self.log.as_mut() else { return };
        for a in acts {
            log.append_act(a);
        }
    }

    /// 切换到一段已存档的对话（按 **id**）：读回消息载入上下文，并把落盘器接回那次对话。
    ///
    /// 返回载入的消息条数（不含系统提示）。载入失败时上下文**保持原样** ——
    /// 半途清空再报错，用户会既丢了眼前的对话、又没拿到想要的那段。
    ///
    /// 载入的是全部历史。这让「接着上次聊」是真的接着聊，代价是对话越长
    /// 上下文越大；想开新话题就按「+」，那是另一条路径。
    pub fn resume(&mut self, id: &str) -> Result<usize> {
        let items = self.rt.store.load(id)?;
        let log = self.rt.store.open(id)?;
        // 只保留系统提示（第 0 条），其余全部换成存档里的消息。
        // 系统提示不入档是有意的（每轮都在变），所以它只能由这里保住。
        self.messages.truncate(1);
        self.messages.extend(items.iter().map(|i| i.msg.clone()));
        self.log = Some(log);
        self.log_error_reported = false;
        // 用量账本从存储层接回来。不接的话，重启一次或切一次对话，
        // 之前那几轮的 token 就凭空消失了 —— 而「这段对话一共花了多少」
        // 恰恰是最该跨会话保留的数字。
        self.usage_log = self.rt.store.load_usage(id);
        self.session_usage = Usage::default();
        self.session_calls = 0;
        for r in &self.usage_log {
            self.session_usage.accrue(&r.usage);
            self.session_calls += r.calls;
        }
        // 工作区跟着**对话**走，不是跟着进程走：把这段对话存档里的那条还回来，
        // 并把「已落盘」对齐到它。于是重开这段对话会落在**它自己**那块地里，
        // 而不是「当前这块」—— 原先正是后者，于是同一个对话每次进去工作区都不一样。
        //
        // 旧对话文件没有这条记录（那时还没这个功能）：什么都不动，等它下一次落盘时
        // 由 `persist_workplace` 把当前状态记上，从此这段对话也就有了自己的地。
        self.workplace_logged = match self.rt.store.load_workplace(id) {
            Some(rec) => {
                self.rt.workplace.restore(&rec);
                Some(rec)
            }
            None => None,
        };
        Ok(items.len())
    }

    /// 开始一段新对话。
    ///
    /// 旧对话**不需要任何「保存」动作**：它是边聊边追加的，此刻已经完整地
    /// 躺在 `history/` 里了。这里只把落盘器置空 —— 下一轮会惰性新建一个文件，
    /// 于是「上一段对话」自然就归入历史。
    pub fn new_conversation(&mut self) {
        self.messages.truncate(1);
        self.log = None;
        self.log_error_reported = false;
        self.session_usage = Usage::default();
        self.session_calls = 0;
        self.usage_log.clear();
        // 工作区也跟着翻页：**一律回到「默认」档**（用户 2026-10-04 定的语义）——
        // 上一段对话里临时切过的档不继承；下一轮的首条消息会在 `workplace/` 下开一块新地。
        // 不清的话，「+」之后模型还写在上一次对话挑的那块地里，那就不是「新对话」了。
        self.rt.workplace.reset_for_new_conversation();
        // 新对话的文件还没建，落盘记录等首条消息时由 `persist_workplace` 补上；
        // 这里先清掉「已落盘」，免得拿上一段对话的记录去比对、把首条消息那次写漏。
        self.workplace_logged = None;
    }

    /// 中断当前任务后回滚对话历史，避免遗留「有 tool_calls 但无结果」的半截消息。
    ///
    /// 落盘文件同步回滚：内存里丢掉了什么，文件里也必须跟着丢掉，
    /// 否则会留下一段无法作为请求体回放的残记录。
    pub fn truncate_messages(&mut self, len: usize) {
        self.messages.truncate(len);
        if let Some(log) = self.log.as_mut() {
            log.rewrite(&self.messages);
        }
    }

    /// 丢掉正文里最老的一段：保留系统提示，以及 `messages[cut..]`（`cut` 起的都留下）。
    ///
    /// ⚠️ **与 `truncate_messages` 方向相反**，所以名字刻意区分开：
    /// `Vec::truncate(n)` 的语义是「**留前** n 条」，中断回滚要的正是这个（回到任务前）；
    /// 而 `[loop] max_rounds` 要的是「**留后**几轮」。两者混用的后果是**静默**裁错方向 ——
    /// 表现为「本轮刚说的话没了、最老的上下文还在」，且内存与文件倒是「一致地错」，
    /// 所以单看落盘内容并不容易发现。这条注释就是为此存在的。
    ///
    /// 落盘同样对称：内存丢了什么，文件里也跟着丢（走 `Session::rewrite`）。
    pub fn drop_oldest(&mut self, cut: usize) {
        if cut <= 1 || cut >= self.messages.len() {
            return;
        }
        self.messages.drain(1..cut);
        if let Some(log) = self.log.as_mut() {
            log.rewrite(&self.messages);
        }
    }

    /// 处理一轮用户输入：循环「问模型 → 执行工具 → 回填结果」直到模型不再调用工具。
    ///
    /// **循环本身不在这里** —— 它由 `Runtime.loop_impl`（循环缝，见 `src/agent_loop.rs`）
    /// 提供；本方法只负责「开一轮、开账、把账收口」这三件事。换一个 `[loop] impl`
    /// 就是换一套执行模型，而记账与收尾仍然走同一条路。
    ///
    /// `files` 是这一条消息带的附件（本机路径）。**只附在这一条上**（一次性）：
    /// 后续轮次模型要用就顺着消息里的路径引用自己读 —— 附件的字节留在上下文里，
    /// 每一轮都要为它再付一次 token。
    pub async fn run_turn(&mut self, ui: &mut dyn Ui, input: &str, files: &[PathBuf]) -> Result<()> {
        // 工作区：`default` 档位在**首条消息**时开一块新地（`started` 保证一段对话只开一次）。
        //
        // 放在整轮最前面（早于 `push_message` 与循环），是为了让**本轮任何工具调用之前**
        // 根就已经定好。否则第一条消息里模型要写文件时会被判成「工作区还没建立」而拒掉，
        // 用户会觉得莫名其妙 —— 那是实现顺序造成的假故障，不是权限边界。
        match self.rt.workplace.ensure_started(&self.rt.root) {
            Ok(Some(dir)) => ui.emit(AgentEvent::Info(format!(
                // 措辞按新语义（读不设限、写删受限）：说「只会读写文件」会让用户以为
                // 连读也被关住了，于是白白去提权 —— 那是 2026-10-04 改语义时漏掉的一句。
                "本对话的工作区：{}\n  写入与删除都被限制在这个目录里（读取不受限，设备上任何文件都能读）。\
                 要放宽：`/workplace`，或输入框左下角的「工作区」。",
                dir.display()
            ))),
            Ok(None) => {}
            // 建目录都失败了（权限 / 磁盘），照实说但不打断对话：此后文件写入会被边界拒绝。
            Err(e) => ui.emit(AgentEvent::Warn(format!("创建工作区失败：{e:#}"))),
        }
        self.ensure_log(ui);
        // 首条消息这一趟把「本对话在哪块地干活」记进存档（`ensure_started` 刚定下根，
        // 所以这条记录里带的是那块地的真实路径）。之后只在切档时才再写。
        self.persist_workplace();
        self.push_message(self.user_message(input, files), None);
        // 包住整轮：UI 借此显示等待动画，并在结束时把中间过程折叠成一行
        ui.turn_start().await;
        self.begin_turn();
        // 先把 `Arc` 取出来：`run` 要 `&mut self`，不能再从 `self` 上借 `loop_impl`。
        let lp = self.rt.loop_impl.clone();
        let result = lp.run(self, ui).await;
        ui.turn_end().await;
        // 用量排在动画停掉之后再报（见 `Ui::turn_usage` 的注释）。
        // 出错也报：token 是真花出去了，不能因为任务失败就装作没花。
        if let Some(rec) = self.finish_turn() {
            ui.turn_usage(&rec).await;
        }
        result
    }

    // ───────── 给 `AgentLoop` 的受控原语（见 `src/agent_loop.rs` 的模块文档）─────────
    //
    // 循环搬到 `AgentLoop` 之后成了 `Agent` 的**同层协作者**，所以下面这组方法必须可见。
    // 但写入口仍然只有 `push_message` 一个：循环拿不到裸 `Vec<Message>`，
    // 也就造不出「内存里有、历史文件里没有」的错位 —— 那条线没有因为抽缝而放松。

    /// 运行时（工具表 / 插件表 / 配置 / 钩子链都挂在它上面）。
    pub fn rt(&self) -> &Arc<Runtime> {
        &self.rt
    }

    /// LLM Provider。
    pub fn llm(&self) -> &Arc<dyn LlmProvider> {
        &self.llm
    }

    /// 当前上下文的只读视图（含下标 0 的系统提示）。**只读** —— 要改走 `push_message`。
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// 开一轮新的账。`run_turn` 在把控制权交给循环之前调用。
    pub fn begin_turn(&mut self) {
        self.turn = TurnAcc::default();
    }

    /// 记一笔 LLM 调用（次数 + 各项 token）。端点不给 usage 时 `accrue` 会安静跳过。
    pub fn accrue_call(&mut self, usage: &Usage) {
        self.turn.calls += 1;
        self.turn.total.accrue(usage);
        if usage.prompt.is_some() {
            self.turn.last_prompt = usage.prompt;
        }
    }

    /// 收口本轮：把账并入会话累计、记进 `usage_log`、落进对话文件。
    ///
    /// 一次调用都没发生（循环一开始就失败）时返回 `None` —— 空账不值得记一行。
    ///
    /// 落盘的与交给界面的是**同一个对象** —— 界面显示的账与文件里存的账因此不可能
    /// 对不上（这类「界面上一个数、文件里另一个数」的坑不值得踩第二次）。
    pub fn finish_turn(&mut self) -> Option<UsageRecord> {
        if self.turn.calls == 0 {
            return None;
        }
        self.session_usage.accrue(&self.turn.total);
        self.session_calls += self.turn.calls;
        let rec = UsageRecord {
            round: self.usage_log.len() as u32 + 1,
            at: crate::history::unix_now(),
            calls: self.turn.calls,
            usage: self.turn.total.clone(),
            last_prompt: self.turn.last_prompt,
            session_total: self.session_usage.total,
            session_calls: self.session_calls,
        };
        if let Some(log) = self.log.as_mut() {
            log.append_usage(&rec);
        }
        self.usage_log.push(rec.clone());
        Some(rec)
    }

    /// 授权 → 执行一次工具调用。属于给 `AgentLoop` 的受控原语：它把「问策略、问用户、
    /// 记白名单、执行并兜超时」这一整套收在一个入口里，循环不需要、也不应该自己拼。
    pub async fn authorize_and_execute(&mut self, ui: &mut dyn Ui, name: &str, args: Value) -> ToolOutput {
        let kind = self.rt.tool_kind(name).await.unwrap_or(ToolKind::Meta);
        // ── 工作区边界：**早于授权**判定 ──
        //
        // 放在授权之前有两个理由：
        // ① 越界调用不必先弹一次授权框、再告诉用户「其实那个位置根本碰不到」；
        // ② 这里拿得到 `Ui`，能给用户发一条 `Warn`（工具自己的 `call` 拿不到 Ui，
        //    所以工具内那层只能把契约塞回给模型 —— 两层是纵深，不是重复）。
        //
        // 这次调用算「读」还是「落盘」**由工具自己说**（`Tool::access`：宿主文件工具按
        // 名字判，插件工具按插件声明的 access 判，没声明就是落盘）。
        //
        // `full` 档位在 `workplace_reject` 里**整个短路**：不扫参数、不 `canonicalize`。
        // 早先这里是「照样调 `check`，反正它返回 `Ok`」—— 而调用方只取 `.err()`，
        // 于是 `normalize` 的结果被丢掉，65~224 µs 的系统调用白花；这条路径又是
        // **每次工具调用**都要走的。
        // ⚠️ 2026-10-05 起配置默认是受限档（`default`），所以这条短路只对**显式**选了
        // 完全访问的用户生效 —— 别再用「大多数用户走这条」给短路当理由。
        let access = self.rt.tool_access(name).await;
        if let Some(reject) = self.workplace_reject(&kind, access, &args) {
            ui.emit(AgentEvent::Warn(reject.user_text()));
            return ToolOutput::err(reject_text(&kind, &reject));
        }
        // 能力位（阶段 6 · 6b 起真的填上）：插件在 `[contributes] caps` 里声明要什么能力，
        // 这里原样转给策略 —— 声明了能力的插件，它的工具调用一律走 `Ask`
        // （见 `auth::Policy::level_for`）。元工具、以及没声明能力的插件拿到的都是空切片，
        // 判定与阶段 4 逐条一致。
        let caps = self.rt.caps_for(&kind).await;
        let level = self.rt.auth.level_for(&kind, name, &caps);
        let allowed = match level {
            AuthLevel::Auto => true,
            AuthLevel::Deny => false,
            AuthLevel::Ask => {
                if self.rt.session_allowed.read().await.contains(name) {
                    true
                } else {
                    match ui.authorize(name, &args).await {
                        AuthDecision::Allow => true,
                        AuthDecision::AllowSession => {
                            self.rt.session_allowed.write().await.insert(name.to_string());
                            true
                        }
                        AuthDecision::Deny => false,
                    }
                }
            }
        };
        if allowed {
            ui.emit(AgentEvent::ToolRunning {
                name: name.to_string(),
                timeout_secs: self.rt.config.agent.tool_timeout_secs,
            });
            self.rt.execute(name, args).await
        } else {
            // 「被配置挡住」与「用户当场拒绝」是两件事，给模型的措辞也必须不同：
            // 前者是宿主写死的规则（`[auth]` 里的 deny，阶段 4 起支持 `fs_*` 通配），
            // 后者才是人。以前两者共用同一句话，模型会去「说服用户」，方向从一开始就错了。
            match level {
                AuthLevel::Deny => ToolOutput::err(format!(
                    "工具 {name} 被宿主策略禁止（config.toml 的 [auth]）。\
                     不要再尝试调用它，改用其它工具或换一条实现路径。"
                )),
                _ => ToolOutput::err(
                    "用户拒绝了这次工具调用。请改用其他方式完成任务，或向用户说明原因。",
                ),
            }
        }
    }

    /// 判定一次工具调用是否触到工作区边界。`None` = 放行。
    ///
    /// 按工具类别分三条路，精度不同，都是刻意的：
    ///
    /// - `ToolKind::Meta`（`create_plugin` / `list_plugins` / `remove_plugin`）：
    ///   不声明路径参数，直接放行。
    ///
    /// - `ToolKind::Host`（宿主文件工具）：schema 是我们自己定义的，直接读 `path` 参数。
    ///   读（`read_file` / `list_dir` / `attach_file`）不受工作区限制 ⇒ 这条路恒等于
    ///   放行；只有写（`write_file`）与删（`remove`）才真正过边界。`list_dir` 允许
    ///   **不传** `path`（那时指的是工作区根，位置由宿主自己给），所以缺参不是错误、
    ///   不放行的是「模型指定的位置」。
    ///
    /// - `ToolKind::Plugin`：插件是黑盒，宿主只能按参数名与值形态扫一遍「像路径」的
    ///   字符串（见 `workplace::scan_paths`）。这些参数按**插件声明的** access 判
    ///   （[`crate::tool::Tool::access`]）：声明了 `"read"` 的按读判（读不设限 ⇒
    ///   一律放行），没声明的仍按「落盘」—— 宿主看不出它的意图，而「少挡一次写」的
    ///   代价远大于「多挡一次读」，保守方向就是把每个像路径的参数都当成潜在的写。
    ///   同时它**宁漏勿误伤**：漏判只是少一层粗网，误判会直接拒掉一次合法调用。
    ///
    /// 三条路用的是**同一套** `Workplace` 判定，所以不存在「两套规则」。
    fn workplace_reject(
        &self,
        kind: &ToolKind,
        access: crate::workplace::Access,
        args: &Value,
    ) -> Option<crate::workplace::Reject> {
        // `full` 档：**一切放行**。放在 `match` 之前，是为了连
        // `scan_paths` 那张粗网都不跑 —— 见调用点那一段注释。
        if self.rt.workplace.is_unrestricted() {
            return None;
        }
        match kind {
            ToolKind::Meta => None,
            ToolKind::Host => {
                let raw = args.get(crate::workplace::PATH_ARG).and_then(|v| v.as_str())?;
                self.rt.workplace.check_access(raw, access).err()
            }
            ToolKind::Plugin(_) => {
                for (_key, raw) in crate::workplace::scan_paths(args) {
                    // 走 `check_access` 而不是 `check`：粗网本身不预设读或写，
                    // 由调用方给的 `access` 说了算。传 `Read` 时它恒为 `Ok`，
                    // 于是「只读工具可以碰工作区外的路径」这件事**自动**成立，
                    // 与 `read_file` / `list_dir` 用的是同一条判定。
                    if let Err(r) = self.rt.workplace.check_access(&raw, access) {
                        return Some(r);
                    }
                }
                None
            }
        }
    }
}

/// 一次越界该给模型看的完整文本。
///
/// `kind` 决定要不要附上插件那条**出路**（[`PLUGIN_READ_HINT`]）：只有插件工具需要，
/// 宿主文件工具不该看到它 —— 那段话会把模型的注意力引到一条对它并不存在的路上
///（宿主工具没有「声明只读」这回事，它的档位写死在宿主手里）。
fn reject_text(kind: &ToolKind, reject: &crate::workplace::Reject) -> String {
    match kind {
        ToolKind::Plugin(_) => format!("{}\n\n{PLUGIN_READ_HINT}", reject.model_text()),
        _ => reject.model_text(),
    }
}

/// 插件工具的路径参数撞上边界时，多附给模型的那一段。
///
/// 只加在 `ToolKind::Plugin` 那一种情形。存在的理由：粗网按写入拦下一个**只读**工具
/// 是**误伤**，而模型此刻手里只有那份通用报错（「不要重试、要提权就找用户」）——
/// 于是它要么放弃，要么去请求提权。而这件事根本不需要动边界：**插件补一行声明就行**。
/// 不给它这条路，一次误伤就会换来一次不必要的权限放宽。
///
/// 措辞上要**明确压过**通用报错里的「不要重试」：那一条是给「真的想越界写」准备的，
/// 这里是另一回事（工具本来就只读），不说清的话模型会照着前一句放弃。
const PLUGIN_READ_HINT: &str = "【如果这道工具只读，上面那句「不要重试」不适用】\n\
     如果这道插件工具本来就只是**读**（扫描 / 统计 / 列目录 / 解析文件这类），\
     那么拦住你的不是权限，而是插件没声明自己是只读的。\n\
     请让插件在工具声明里补上第 4 位 `\"read\"`，重建插件后即可照常访问工作区外的路径。\
     Python SDK 的写法是把三元组写成四元组：\n  \
     \"<工具名>\": (\"<说明>\", <JSON Schema>, <handler>, \"read\")\n\
     宿主随后就按「读」判 —— 与 read_file / list_dir 同一档，**不需要向用户提权**。\
     反过来，会写文件 / 删东西的工具**不要**加这一位。";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    /// 一道**假**插件工具：类别报 Plugin，`access` 由测试给。
    ///
    /// 用它而不是真起一个插件子进程：这条测试要钉的是「边界怎么用声明的 access」，
    /// 与「插件进程怎么跑起来」无关 —— 后者另有真插件的自检（`selftest`）。
    struct FakeTool {
        name: String,
        access: crate::workplace::Access,
    }

    #[async_trait::async_trait]
    impl crate::tool::Tool for FakeTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> ToolKind {
            // 插件名随便给：`workplace_reject` 只看「是不是插件」，不看是哪一个。
            ToolKind::Plugin("fake".into())
        }
        fn description(&self) -> &str {
            "test double"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn access(&self) -> crate::workplace::Access {
            self.access
        }
        async fn call(&self, _args: Value, _rt: &Arc<Runtime>) -> ToolOutput {
            ToolOutput::ok("test double")
        }
    }

    /// 一个工作区固定在 `ws` 的 Agent，并注册一道叫 `tool`、意图为 `access` 的工具。
    async fn agent_with(ws: &Path, tool: &str, access: crate::workplace::Access) -> Agent {
        let mut cfg = crate::config::Config::dummy();
        cfg.workplace = crate::config::WorkplaceConfig {
            mode: "fixed".into(),
            root: Some(ws.to_string_lossy().into_owned()),
        };
        let rt =
            Runtime::new(cfg, ws.to_path_buf(), ws.join("plugins")).expect("运行时应当能建起来");
        rt.register(Arc::new(FakeTool { name: tool.to_string(), access }))
            .await
            .expect("注册假工具应当成功");
        let llm: Arc<dyn LlmProvider> =
            Arc::new(crate::llm::UnavailableProvider { reason: "test".into() });
        Agent::new(llm, rt)
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aox-agent-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// **A/B 对拍**：同一道工具、同一个工作区**外**的路径，只差一个声明。
    ///
    /// 为什么必须两边都断言：只测「声明了 read 就放行」，把整个判定删掉（恒放行）也能过；
    /// 只测「没声明就拦」，把 access 硬编码成 `Write`、无视声明也能过。
    /// 两条一起，才证明「这一档**确实由声明决定**」—— 而这正是本次改动的全部内容。
    #[tokio::test]
    async fn plugin_read_declaration_controls_the_workplace_gate() {
        use crate::workplace::Access;
        let ws = tmp("access");
        // 工作区**外面**的一个路径。不必真实存在：判定只做路径比较，不碰磁盘。
        let outside = ws.parent().unwrap().join("aox-agent-outside").join("x.bin");
        let args = json!({ "path": outside.to_string_lossy(), "limit": 10 });
        let kind = ToolKind::Plugin("fake".into());

        // ① 声明了只读 ⇒ 放行（与 read_file / list_dir 同一档）
        let a_read = agent_with(&ws, "fake_scan", Access::Read).await;
        let access = a_read.rt().tool_access("fake_scan").await;
        assert_eq!(access, Access::Read, "`tool_access` 要把工具声明的意图透出来");
        assert!(
            a_read.workplace_reject(&kind, access, &args).is_none(),
            "只读工具访问工作区外的路径必须放行 —— 这就是「插件的读也放开」那条需求"
        );

        // ② 没声明（= 落盘）⇒ 同一个路径被拦
        let a_write = agent_with(&ws, "fake_scan", Access::Write).await;
        let access = a_write.rt().tool_access("fake_scan").await;
        assert_eq!(access, Access::Write, "没声明就是落盘，这是默认档");
        let rej = a_write
            .workplace_reject(&kind, access, &args)
            .expect("没声明只读的插件工具，带区外路径必须被拦下");
        assert!(rej.model_text().contains("权限边界"), "{}", rej.model_text());

        // 宿主文件工具那条路没被这次改动影响：同一个区外路径仍然被拦（写档）
        let host_write = a_write
            .workplace_reject(&ToolKind::Host, Access::Write, &args)
            .expect("宿主写工具的区外路径必须被拦");
        assert!(host_write.model_text().contains("权限边界"));
        // 而读档照旧放行 —— 与 2026-10-04 定的「读不设限」一致
        assert!(a_write.workplace_reject(&ToolKind::Host, Access::Read, &args).is_none());

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// 越界文本里那条**插件专用出路**：插件要看得见，宿主文件工具不该看见。
    ///
    /// 两边都断言的道理与上一条相同：只断言「插件有」，把提示无条件加到所有工具上也能过
    /// —— 而那会让宿主文件工具收到一份指向**不存在机制**的建议（宿主工具没有「声明只读」
    /// 这回事，它的档位写死在宿主手里），把模型的注意力引到岔路上。
    #[test]
    fn the_read_hint_is_attached_only_to_plugin_rejects() {
        let rej = crate::workplace::Reject::NotStarted;
        let base = rej.model_text();

        let plugin = reject_text(&ToolKind::Plugin("x".into()), &rej);
        assert!(plugin.starts_with(&base), "通用部分要原样在前：{plugin}");
        assert!(plugin.contains(PLUGIN_READ_HINT), "插件那条出路必须附上");
        assert!(plugin.contains("\"read\""), "要写清补的是哪个字面量：{plugin}");

        let host = reject_text(&ToolKind::Host, &rej);
        assert_eq!(host, base, "宿主工具不该收到那段插件专用的出路");
    }
}
