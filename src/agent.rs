use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

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
            turn: TurnAcc::default(),
        }
    }

    /// 本次对话的历史 id（还没开始对话时为 None）。
    /// id 由存储后端在新建时定下（jsonl 后端就是文件名主干），这里只是转述。
    pub fn history_id(&self) -> Option<&str> {
        self.log.as_ref().map(|l| l.id())
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
    pub async fn run_turn(&mut self, ui: &mut dyn Ui, input: &str) -> Result<()> {
        self.ensure_log(ui);
        self.push_message(Message::user(input), None);
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
}
