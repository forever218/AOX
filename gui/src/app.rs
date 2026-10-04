//! GUI 主界面状态与绘制。
//!
//! 布局（参考主流 AI 编程软件）：
//! ```text
//! ┌───────────────────────────────────────────────────────────┐
//! │ 顶栏：状态 · [历史] [新建]              [活动流] [明暗] [关于] │
//! ├───────────────────────────────────────────────────────────┤
//! │ 对话流：用户气泡 / 助手 Markdown / 折叠的过程                 │
//! │                  ┌──────────────────┐                      │
//! │                  │ 悬浮输入框（覆盖）│                      │
//! └──────────────────┴──────────────────┴──────────────────────┘
//! ```
//!
//! 活动流**不在主窗口里**，点顶栏那枚图标会以独立窗口打开（见 `activity.rs`）；
//! 左侧历史栏也默认折叠。所以主窗口只有一栏内容。
//!
//! 底部没有独立的「输入区」栏位：输入框是一枚浮在对话内容之上的卡片，
//! 滚动时可以一直滑到它下面。

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Frame, Label, Layout, Margin, Pos2, Rect,
    RichText, ScrollArea, Sense, Stroke, TextEdit, Ui, Vec2,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use aox::history::UsageRecord;
use aox::llm::{thousands, TokenEstimate, Usage};
use aox::ui::{AgentEvent, AuthDecision};
use aox::workplace::Mode;

use crate::activity::{self, ActKind};
use crate::bridge::{BootInfo, FromAgent, RuntimeView, SessionFlags, Snapshot, ToAgent, WorkplaceView};
use crate::markdown;
use crate::theme::{self, P};
use crate::workplace_ui;

// ─────────────────────────── 数据模型 ───────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    Info,
    Warn,
    Error,
}

impl NoteKind {
    /// 单色方案下没有红黄绿可用了，改用明度层次：正文色 / 次要色 / 更弱色。
    fn color(self) -> Color32 {
        match self {
            NoteKind::Info => P::text_dim(),
            NoteKind::Warn => P::text_dim(),
            NoteKind::Error => P::text(),
        }
    }
}

/// 一次工具调用（用于折叠展示）。
#[derive(Clone)]
struct StepView {
    name: String,
    plugin: Option<String>,
    args: String,
    result: Option<(bool, String)>,
}

/// 一轮任务的「过程」（结束后折叠成一行）。
struct Process {
    steps: Vec<StepView>,
    interim: Vec<String>,
    reasoning: String,
    /// 耗时。历史记录里没有时间信息，用 `None` 表示「不显示耗时」。
    elapsed: Option<f32>,
    expanded: bool,
    aborted: bool,
}

/// 对话流中的一条记录。
enum Item {
    User(String),
    Assistant(String),
    Process(Process),
    Note(NoteKind, String),
    /// 一轮任务的 token 明细。紧跟在那轮 AI 答复下面 —— 账要贴着它花掉的地方，
    /// 而不是攒到顶栏让人自己去对轮次。
    Usage(String),
    /// 错误卡片（不可恢复）
    Error(String),
}

/// 进行中的一轮任务。
struct Live {
    status: String,
    reasoning: String,
    reasoning_live: bool,
    steps: Vec<StepView>,
    interim: Vec<String>,
    pending_final: Option<String>,
    started: Instant,
    /// 输出侧（思考 + 正文）收到的字符数，用于实时滚动一个在涨的 token 数。
    ///
    /// 流式过程中服务端还没报数，只能估；一轮结束时会用 `TurnUsage` 里的精确值顶掉。
    out: TokenEstimate,
}

struct AuthCard {
    tool: String,
    args: String,
}

// ─────────────────────────── App ───────────────────────────

pub struct App {
    tx: UnboundedSender<ToAgent>,
    rx: UnboundedReceiver<FromAgent>,
    flags: Arc<SessionFlags>,
    driver: Option<std::thread::JoinHandle<()>>,

    started: Instant,
    boot: Option<BootInfo>,

    items: Vec<Item>,
    live: Option<Live>,
    /// 活动流的共享状态。住在独立窗口里（`activity.rs`），主窗口只管往里写。
    act: activity::Shared,
    auth: Option<AuthCard>,
    auth_reply: Option<oneshot::Sender<AuthDecision>>,

    input: String,
    focus_input: bool,
    busy: bool,
    waiting_auth: bool,
    status: String,
    spin: f32,
    /// 当前对话**逐轮**的 token 账。
    ///
    /// 打开一段旧对话时从文件读回来（`FromAgent::UsageLog`），每轮结束再追加一条。
    /// 空的或全无数字的（端点不返回 usage）就整块不显示 —— 宁可没有，
    /// 也不要拿一串 0 假装在统计。
    usage_log: Vec<UsageRecord>,
    /// 顶栏那枚「Σ 总 token」的面板是否正被指针压着。
    ///
    /// 单靠 `resp.hovered()` 会在指针从胶囊移向面板的那一瞬断开（两者之间有缝），
    /// 于是面板一闪就没了。记一个跨帧的状态就稳住了。
    usage_hover: bool,

    model: String,
    config_path: String,
    plugins: Vec<String>,
    /// 插件运行时状态（每语言一行，由 bridge 发来）。
    /// 放在「关于」窗口里——这类「看一眼就够」的诊断数据不该占用主界面。
    runtimes: RuntimeView,

    /// 读历史用的存储实例（`aox::store`，与 agent 线程同一个后端的另一个句柄）。
    ///
    /// GUI 直接读盘，不经过 agent 线程 —— 读历史是纯文件操作，没有并发问题，
    /// 也就不必为它再开一条 channel。`None` 表示「还没启动完成」，此时不读。
    store: Option<Arc<dyn aox::store::Store>>,
    /// 历史对话列表（最新的在前）。只在展开侧边栏、或本轮结束后刷新。
    history: Vec<aox::history::Summary>,
    /// 当前所在的这段对话的 id（尚未落盘的新对话为 None）。
    ///
    /// 侧边栏据此标「当前」—— 侧边栏是**会话切换器**：点某一条就切过去接着聊，
    /// 这条自然也就跟着变了。
    current_history: Option<String>,
    /// 正在等 Agent 回执的切换请求（切换前先灰掉列表，避免连点造成来回跳）
    switching: bool,
    /// 左侧历史侧边栏是否展开
    show_history: bool,

    /// 演示模式（--demo）：不连 LLM，只展示界面与事件流的排布
    demo: bool,
    /// 对话流是否自动跟随到底部
    follow: bool,
    /// 本条消息强制滚到底（用户刚发指令时）
    force_follow: bool,
    /// 活动流是否自动跟随
    /// 关于窗口是否展开
    about_open: bool,
    log_error: Option<String>,
    /// `--prompt` 传入的首条指令：等模型就绪后自动发送
    pending_prompt: Option<String>,
    /// 授权弹窗出现的时刻（用于快捷键去抖，避免弹窗刚出现就被误触）
    auth_shown_at: Instant,
    /// 首帧窗口尺寸收敛：0=还没试过，>0 表示已尝试的帧数
    fit_tries: u8,
    /// 上次下发给操作系统的原生标题栏主题。`None` 表示还没下发过。
    ///
    /// 创建窗口时下发的 viewport command 有可能在前端事件循环起来之前就被丢掉，
    /// 所以这里记一笔，保证至少从 `update` 里真正下发一次。
    native_theme: Option<theme::Mode>,

    /// 输入框左下角那枚「工作区」按钮的状态（按钮 / 向上弹出列表 / 确认框 / 目录选择器）。
    ///
    /// 状态整体住在 `workplace_ui` 里，这里只持有一个句柄 —— 免得四个布尔量散落在
    /// `App` 上、各自还要记得互斥关系（见 `workplace_ui::button` 里那段「开一个收两个」）。
    ws: workplace_ui::WorkplaceUi,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        theme_cfg: aox::config::ThemeConfig,
        tx: UnboundedSender<ToAgent>,
        rx: UnboundedReceiver<FromAgent>,
        flags: Arc<SessionFlags>,
        driver: Option<std::thread::JoinHandle<()>>,
        demo: bool,
        prompt: Option<String>,
    ) -> Self {
        // 外观全部来自 `[theme]`；明暗优先取上次手动切换的记录（见 theme::apply）。
        theme::apply(&cc.egui_ctx, &theme_cfg);
        let mut app = Self {
            tx,
            rx,
            flags,
            driver,
            demo,
            started: Instant::now(),
            boot: None,
            items: Vec::new(),
            live: None,
            act: activity::new_shared(),
            auth: None,
            auth_reply: None,
            input: String::new(),
            focus_input: false,
            busy: false,
            waiting_auth: false,
            status: "就绪".into(),
            spin: 0.0,
            usage_log: Vec::new(),
            usage_hover: false,
            model: String::new(),
            config_path: String::new(),
            plugins: Vec::new(),
            runtimes: RuntimeView::default(),
            store: None,
            history: Vec::new(),
            current_history: None,
            switching: false,
            show_history: false,
            // 左右两个侧栏默认都收起：主区宽度全部让给对话，需要时再点顶栏那两枚图标。
            follow: true,
            force_follow: true,
            about_open: false,
            log_error: None,
            pending_prompt: if demo { None } else { prompt },
            auth_shown_at: Instant::now(),
            fit_tries: 0,
            native_theme: None,
            ws: workplace_ui::WorkplaceUi::new(),
        };
        if demo {
            seed_demo(&mut app);
        }
        app
    }

    fn now(&self) -> f32 {
        self.started.elapsed().as_secs_f32()
    }

    /// 首帧把窗口收敛进屏幕，并显式摆正位置。
    ///
    /// 要解决两件事：
    ///
    /// 1. **尺寸**：启动时请求 1380×900 逻辑点，在高 DPI 缩放的小屏上，客户区连同
    ///    窗口装饰会超出屏幕，窗口下半截（连同输入框）被切掉。
    /// 2. **位置**：不指定位置就只能听操作系统安排，而 Windows 会把新窗口「级联」
    ///    到右下角，窗口下沿越过屏幕底边——用户看不到输入框，截图里那条黑带就是
    ///    屏幕外区域被 `CopyFromScreen` 抓成了黑色。
    fn fit_window(&mut self, ctx: &egui::Context) {
        const TRIES: u8 = 60;
        if self.fit_tries >= TRIES {
            return;
        }
        self.fit_tries += 1;

        let (mon, inner, outer) = ctx.input(|i| {
            let v = i.viewport();
            (
                v.monitor_size,
                v.inner_rect.map(|r| r.size()),
                v.outer_rect.map(|r| r.size()),
            )
        });
        // 窗口真正创建之前拿不到这些，下一帧再试
        let (Some(mon), Some(inner), Some(outer)) = (mon, inner, outer) else {
            return;
        };

        // 窗口装饰（标题栏 + 边框）占在客户区之外。首帧往往还量不准，给个按逻辑点算
        // 的下限兜底：标题栏约 30、边框约 8，46 足以覆盖 100%~200% 的缩放。
        let chrome = (outer - inner).max(Vec2::new(18.0, 46.0));

        let want = default_window_size(mon, chrome);

        if (inner.x - want.x).abs() > 8.0 || (inner.y - want.y).abs() > 8.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(want));
        }

        // 居中摆放（外框 = 客户区 + 装饰），保证下沿不越过任务栏
        let outer_size = want + chrome;
        // 可见高度（去掉任务栏）同样只在 default_window_size 里算过一次，
        // 这里用「默认高度 + 装饰」反推，避免两处各算一遍任务栏高度。
        let taskbar = (mon.y * 0.06).clamp(36.0, 72.0);
        let usable_h = (mon.y - taskbar).max(1.0);
        let pos = Vec2::new(
            ((mon.x - outer_size.x) * 0.5).max(6.0),
            ((usable_h - outer_size.y) * 0.5).max(6.0),
        );
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::Pos2::new(
            pos.x, pos.y,
        )));

        // 收敛完就不必再算
        self.fit_tries = TRIES;
    }

    /// 往活动流里记一条系统事件（时间轴「系统」行上的一枚短色块）。
    ///
    /// **只有一轮任务执行期间才记**：活动流的语义是「这一轮是怎么跑的」，
    /// 程序闲着的时候它上面不该多出任何东西。
    fn note_act(&self, kind: ActKind, title: &str, body: &str) {
        if self.live.is_none() {
            return;
        }
        activity::lock(&self.act).note(kind, title, body, self.now());
    }

    fn plugin_of(&self, tool: &str) -> Option<String> {
        // 约定：工具名 = "<插件名>_<功能>"；用已知插件列表做前缀匹配
        for p in &self.plugins {
            if tool.starts_with(&format!("{p}_")) {
                return Some(p.clone());
            }
        }
        if matches!(tool, "create_plugin" | "list_plugins" | "remove_plugin") {
            return Some("meta".into());
        }
        None
    }

    // ── 事件泵：把 Agent 线程的消息落到本地状态 ──
    fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                FromAgent::Boot(b) => {
                    self.model = b.model.clone();
                    self.config_path = b.config_path.clone();
                    // 界面侧读历史的后端由 bridge 一并送来：与 agent 线程同一个实现。
                    self.store = Some(b.store.clone());
                    self.refresh_history();
                    if let Some(note) = &b.note {
                        self.log_error = Some(note.clone());
                        self.items.push(Item::Error(note.clone()));
                    }
                    self.status = if b.provider_ready { "就绪".to_string() } else { "未配置模型".to_string() };
                    let ready = b.provider_ready;
                    self.boot = Some(*b);
                    if ready {
                        // --prompt：模型就绪后自动发首条指令
                        if let Some(p) = self.pending_prompt.take() {
                            self.input = p;
                            self.send();
                        }
                    }
                }
                FromAgent::Snapshot(s) => self.apply_snapshot(s),
                FromAgent::Runtimes(v) => self.runtimes = v,
                // 工作区现状：按钮上的档位名与列表里打勾的那一项都靠它。
                // 启动、每轮结束、每次切档后 bridge 都会重发一份。
                FromAgent::Workplace(v) => self.ws.set_view(*v),
                FromAgent::HistoryCurrent(id) => {
                    // 侧边栏开着时要让条数/标题保持最新；关着时只有「换了对话文件」
                    // 才值得扫一次目录（否则每轮都白读一遍磁盘）。
                    let changed = self.current_history != id;
                    self.current_history = id;
                    if changed || self.show_history {
                        self.refresh_history();
                    }
                }
                FromAgent::Switched { opened, error } => {
                    self.switching = false;
                    if let Some(e) = error {
                        // 载入失败时 Agent 的上下文没被动过，所以这里也什么都不改，
                        // 免得界面已经「跳」过去了、模型却还在原来那段对话里。
                        self.items.push(Item::Error(e));
                        continue;
                    }
                    self.current_history = opened.clone();
                    self.adopt_conversation(opened.as_deref());
                    self.refresh_history();
                }
                FromAgent::TurnStart => {
                    self.live = Some(Live {
                        status: "思考中".into(),
                        reasoning: String::new(),
                        reasoning_live: false,
                        steps: Vec::new(),
                        interim: Vec::new(),
                        pending_final: None,
                        started: Instant::now(),
                        out: TokenEstimate::default(),
                    });
                    self.busy = true;
                    self.status = "思考中".into();
                }
                FromAgent::Status(s) => {
                    self.status = s.clone();
                    if let Some(l) = self.live.as_mut() {
                        l.status = s;
                    }
                }
                FromAgent::Content(d) => {
                    // 正文增量只为让 token 数字继续涨。正文本身走
                    // `Event(AssistantText)` 一次性送达，活动流的模型段也由那里推进 ——
                    // 这里再插一遍会让同一段文字在活动流里出现两次。
                    if let Some(l) = self.live.as_mut() {
                        l.out.add(&d);
                    }
                }
                FromAgent::Usage(rec) => {
                    // 端点没报数就整块不显示，别拿 0 糊弄
                    if !rec.is_empty() {
                        // 明细紧跟在这轮答复后面（此时 turn 已结束，items 末尾正是它），
                        // 顺带把视图拽到底 —— 凭空多出来的一行没人会自己去找
                        self.items.push(Item::Usage(usage_line(&rec)));
                        self.usage_log.push(*rec);
                        self.force_follow = true;
                    }
                }
                FromAgent::UsageLog(log) => {
                    // 切换对话后由 agent 从文件接回来的整段账本。
                    // 逐轮明细要重新插回对话流 —— 打开旧对话时，每轮的账该贴回
                    // 它原来待着的位置，而不是在顶上堆成一坨。
                    self.usage_log = log;
                    self.rebuild_usage_rows();
                }
                FromAgent::Reasoning(d) => {
                    if let Some(l) = self.live.as_mut() {
                        // 注意：这里不能用 `self.now()`（它借整个 self），
                        // 而 `l` 正握着 `self.live` 的可变借用
                        let at = self.started.elapsed().as_secs_f32();
                        if !l.reasoning_live {
                            l.reasoning_live = true;
                            activity::lock(&self.act).model_begin(at);
                        }
                        if l.reasoning.len() < 120_000 {
                            l.reasoning.push_str(&d);
                        }
                        // 推理也计入输出：服务端的 completion_tokens 本来就含它，
                        // 只数正文会明显偏低
                        l.out.add(&d);
                        activity::lock(&self.act).model_push(&d);
                    }
                            }
                FromAgent::ReasoningEnd => {
                    // 思考结束不等于模型这一段结束：它可能接着吐正文、也可能去调工具，
                    // 那两件事各自会把这一段收掉。
                    if let Some(l) = self.live.as_mut() {
                        l.reasoning_live = false;
                    }
                }
                FromAgent::Event(ev) => self.on_event(ev),
                FromAgent::TurnEnd => self.finish_turn(false),
                FromAgent::TurnAborted => {
                    self.finish_turn(true);
                }
                FromAgent::AuthRequest { tool, args, reply } => {
                    self.auth = Some(AuthCard { tool: tool.clone(), args: args.clone() });
                    self.auth_reply = Some(reply);
                    self.waiting_auth = true;
                    self.auth_shown_at = Instant::now();
                    let at = self.now();
                    activity::lock(&self.act).note(
                        ActKind::Auth,
                        &format!("请求授权 · {tool}"),
                        &args,
                        at,
                    );
                }
                FromAgent::Fatal(e) => {
                    self.items.push(Item::Error(e.clone()));
                    self.note_act(ActKind::Error, "致命错误", &e);
                    if self.live.is_some() {
                        self.finish_turn(false);
                    }
                }
            }
        }
    }

    fn apply_snapshot(&mut self, s: Snapshot) {
        // 顶栏不再有「插件 N / 工具 N」的胶囊，这里只保留数据本身；
        // 插件变动由 bridge 的事件流体现，不需要在这里再补一条。
        self.plugins = s.plugins;
    }

    // ─────────────────────── 历史记录 ───────────────────────

    /// 重新扫描历史目录。
    ///
    /// 只在「展开侧边栏」「本轮结束」「切到新对话」时做，而不是每帧 ——
    /// 每帧扫目录会白白打断 UI 的流畅度，而历史列表本来就不需要那么新。
    fn refresh_history(&mut self) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let mut items = store.list(HISTORY_LIST_LIMIT);
        // 正在聊的这一段永远置顶。
        //
        // 盘上的排序键是「最后一次说话的时间」，而刚切进来的这段对话可能三天没动过，
        // 于是会排在很下面 —— 可人此刻要找的就是它。置顶是纯展示层的调整，
        // 不改动文件时间，也不影响 `list()` 给 CLI 的顺序。
        if let Some(cur) = self.current_history.as_deref() {
            if let Some(i) = items.iter().position(|s| s.id == cur) {
                let it = items.remove(i);
                items.insert(0, it);
            }
        }
        self.history = items;
    }

    /// 切换到某段历史对话。
    ///
    /// 只发请求就返回：真正换上下文发生在 Agent 那个线程里，等
    /// `FromAgent::Switched` 回来再动界面 —— 这样「切换失败」时界面不会
    /// 抢先跳过去，留下「界面在这段、模型在那段」的错位。
    fn switch_history(&mut self, id: &str) {
        if self.busy || self.switching || Some(id) == self.current_history.as_deref() {
            return;
        }
        self.switching = true;
        let _ = self.tx.send(ToAgent::Resume(id.to_string()));
    }

    /// 开一段新对话。
    ///
    /// 旧对话**不需要任何收尾动作**：它是边聊边追加的，此刻已经完整地躺在
    /// `history/` 里了。所以这里只是让 Agent 松手，界面跟着清空。
    fn new_conversation(&mut self) {
        if self.busy || self.switching || (self.current_history.is_none() && self.items.is_empty()) {
            return;
        }
        self.switching = true;
        let _ = self.tx.send(ToAgent::NewConversation);
    }

    /// 把界面切到 `id` 那段对话（`None` = 全新的空对话）。
    ///
    /// 内容自己读盘重建，而不是让 Agent 把消息发过来：历史文件本来就在本地，
    /// 通道里只传 id 能让职责更干净（Agent 管上下文，界面管展示）。
    fn adopt_conversation(&mut self, id: Option<&str>) {
        self.items = match (id, self.store.as_ref()) {
            (Some(id), Some(store)) => store
                .load(id)
                .map(|l| items_from_history(&l))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        // 上一段对话的进行中状态与活动流一起换掉，否则右侧会一边显示新对话，
        // 一边滚着旧对话的工具调用。
        self.live = None;
        self.waiting_auth = false;
        self.auth = None;
        self.auth_reply = None;
        // 用量账本跟消息、活动流一起换掉：Agent 那边也把账重新接回来了，
        // 紧接着的 `UsageLog` 会把这一段对话的记录送过来。
        self.usage_log.clear();
        // 活动流同样换掉。它和消息躺在**同一个文件**里，所以「打开这段对话」
        // 就该看到它当时的那条时间轴；新建对话则清空。
        //
        // 记录先取好再上锁：读存储要走 `self.store`，而锁守卫会长期持有 `self.act`。
        let rows = match (id, self.store.as_ref()) {
            (Some(id), Some(store)) => store.load_acts(id),
            _ => Vec::new(),
        };
        // `now` 先算好再上锁：`self.now()` 借整个 `self`，读起来容易和锁守卫纠缠。
        let now = self.now();
        let mut act = activity::lock(&self.act);
        match id {
            Some(_) => act.load_acts(&rows, now),
            None => act.reset(),
        }
        drop(act);
        self.follow = true;
        self.force_follow = true;
    }

    /// 左侧历史侧边栏（会话切换器）。
    fn history_panel(&mut self, ctx: &egui::Context) {
        let frame = Frame::side_top_panel(&ctx.style())
            .fill(P::panel())
            .inner_margin(Margin::symmetric(16, 16));
        let mut want_refresh = false;
        let mut open: Option<String> = None;
        // 切换请求在飞行中、或正在跑任务时不接受点击：这两段时间里发起切换，
        // 结果会排在后面才生效，点的人只会看到界面「迟疑一下才跳」。
        let locked = self.busy || self.switching;
        egui::SidePanel::left("aox-history")
            .resizable(true)
            .default_width(266.0)
            .width_range(206.0..=430.0)
            .frame(frame)
            .show(ctx, |ui| {
                // 与活动流窗口同样的理由：把内容宽度钉死在面板宽度上，
                // 否则内容比面板窄一点点，面板宽度会一帧帧缩到下限。
                let pw = ui.available_width();
                ui.set_min_width(pw);
                ui.set_max_width(pw);

                ui.horizontal(|ui| {
                    ui.label(RichText::new("历史记录").color(P::text_dim()).size(13.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if flat_button(ui, "刷新").clicked() {
                            want_refresh = true;
                        }
                    });
                });
                ui.add_space(7.0);
                let (rule, _) = ui.allocate_exact_size(Vec2::new(pw, 1.0), Sense::hover());
                ui.painter().hline(
                    rule.left()..=rule.right(),
                    rule.center().y,
                    Stroke::new(1.0, P::border()),
                );
                ui.add_space(9.0);

                if self.history.is_empty() {
                    ui.label(
                        RichText::new("还没有历史记录。\n发出第一条指令后，这段对话会自动保存到这里。")
                            .color(P::text_faint())
                            .size(13.0),
                    );
                    ui.add_space(10.0);
                    // 目录路径可能很长，用 truncate 让它在面板宽度内收口，
                    // 而不是靠面板边界去切。
                    ui.add(
                        Label::new(
                            RichText::new(
                                self.store
                                    .as_ref()
                                    .map(|s| s.location().display().to_string())
                                    .unwrap_or_default(),
                            )
                            .color(P::text_faint())
                            .size(11.5),
                        )
                        .truncate(),
                    );
                    return;
                }

                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for s in &self.history {
                        let current = Some(s.id.as_str()) == self.current_history.as_deref();
                        if history_row(ui, s, current, locked, pw).clicked() && !locked {
                            open = Some(s.id.clone());
                        }
                    }
                    // 面板里**不放**「新建对话」：顶栏那枚「+」离侧边栏开关只有 38px，
                    // 两处入口挨得太近，反而要停下来分辨该点哪个。列表就只做一件事 ——
                    // 翻哪段对话。
                });
            });
        if want_refresh {
            self.refresh_history();
        }
        if let Some(id) = open {
            self.switch_history(&id);
        }
    }

    fn on_event(&mut self, ev: AgentEvent) {
        // 活动流只记「一轮任务怎么跑的」：不在任务里的事件（切换对话、致命错误之后
        // 残留的尾巴……）一律不进时间轴。
        let live = self.live.is_some();
        match ev {
            AgentEvent::AssistantText(t) => {
                match self.live.as_mut() {
                    Some(l) => {
                        let at = self.started.elapsed().as_secs_f32();
                        // 模型开始吐正文 = 「模型」那一段开始（若还没开）
                        activity::lock(&self.act).model_begin(at);
                        if let Some(prev) = l.pending_final.replace(t) {
                            l.interim.push(prev);
                        }
                    }
                    None => self.items.push(Item::Assistant(t)),
                }
            }
            AgentEvent::InterimText(t) => {
                if let Some(l) = self.live.as_mut() {
                    let at = self.started.elapsed().as_secs_f32();
                    activity::lock(&self.act).model_begin(at);
                    l.interim.push(t);
                }
            }
            AgentEvent::ToolCall { name, args } => {
                let plugin = self.plugin_of(&name);
                let args = compact_json(&args);
                if let Some(l) = self.live.as_mut() {
                    l.steps.push(StepView {
                        name: name.clone(),
                        plugin: plugin.clone(),
                        args: args.clone(),
                        result: None,
                    });
                }
                if live {
                    // 活动流上只开一段色块：调用 → 执行 → 返回 是同一件事，
                    // 拆成三条就看不出「这一步到底花了多久」。
                    activity::lock(&self.act).tool_begin(
                        &name,
                        plugin.as_deref().filter(|p| *p != "meta"),
                        &args,
                        self.now(),
                    );
                }
            }
            AgentEvent::ToolRunning { name, timeout_secs } => {
                self.status = format!("执行 {name}");
                if live {
                    activity::lock(&self.act).tool_note(&format!("超时上限 {timeout_secs}s"));
                }
            }
            AgentEvent::ToolResult { name, is_error, preview } => {
                if let Some(l) = self.live.as_mut() {
                    if let Some(st) =
                        l.steps.iter_mut().rev().find(|s| s.name == name && s.result.is_none())
                    {
                        st.result = Some((is_error, preview.clone()));
                    }
                }
                if live {
                    activity::lock(&self.act).tool_end(is_error, &preview, self.now());
                }
            }
            AgentEvent::Info(m) => {
                self.note_act(ActKind::Info, "信息", &m);
                if self.live.is_none() {
                    self.items.push(Item::Note(NoteKind::Info, m));
                }
            }
            AgentEvent::Warn(m) => {
                self.note_act(ActKind::Warn, "警告", &m);
                if self.live.is_none() {
                    self.items.push(Item::Note(NoteKind::Warn, m));
                }
            }
            AgentEvent::Error(m) => {
                self.note_act(ActKind::Error, "错误", &m);
                if self.live.is_none() {
                    self.items.push(Item::Note(NoteKind::Error, m));
                }
            }
        }
    }

    /// 一轮结束：把过程折叠成一行，再把最终答复贴到下面。
    fn finish_turn(&mut self, aborted: bool) {
        self.busy = false;
        self.waiting_auth = false;
        self.auth = None;
        self.auth_reply = None;
        self.status = "就绪".into();
        let Some(l) = self.live.take() else { return };
        let elapsed = l.started.elapsed().as_secs_f32();

        if !l.steps.is_empty() {
            self.items.push(Item::Process(Process {
                steps: l.steps,
                interim: l.interim,
                reasoning: l.reasoning,
                elapsed: Some(elapsed),
                expanded: false,
                aborted,
            }));
        }
        if let Some(f) = l.pending_final {
            self.items.push(Item::Assistant(f));
        }
        let at = self.now();
        let pending = {
            let mut act = activity::lock(&self.act);
            // 一轮结束：把所有还开着的段收掉（不留下一条永远在长、越拖越长的色块）
            act.end_round(at);
            if aborted {
                act.note(ActKind::Warn, "任务中断", &format!("已执行 {elapsed:.1}s"), at);
            }
            // 收完就取走这一轮新增的记录，交给 Agent 线程写进**这段对话的文件**。
            // 交给它而不是界面自己开文件：句柄在它手里，而且「新建对话 / 中断回滚」
            // 也在那个线程里发生；这条消息排在队尾，所以顺序天然是对的 ——
            // 不会串到下一段对话里去。
            act.export_acts()
        };
        if !pending.is_empty() {
            let _ = self.tx.send(ToAgent::Act(pending));
        }
        self.force_follow = true;
    }

    // ── 输入区 / 键盘 ──
    fn send(&mut self) {
        if self.demo {
            return;
        }
        let text = self.input.trim().to_string();
        if text.is_empty() || self.busy || self.waiting_auth {
            return;
        }
        if self.boot.as_ref().is_some_and(|b| !b.provider_ready) {
            return;
        }
        self.items.push(Item::User(text.clone()));
        self.input.clear();
        self.force_follow = true;
        // 开一轮：时间轴上最上面那条「输入」从这一刻起
        activity::lock(&self.act).round_begin(&text, self.now());
        let _ = self.tx.send(ToAgent::Input(text));
    }

    fn stop(&mut self) {
        if self.busy {
            self.flags.request_abort();
            self.status = "正在中断…".into();
        }
    }

    // ─────────────────────────── 绘制 ───────────────────────────

    fn top_bar(&mut self, ctx: &egui::Context) {
        let frame = Frame::side_top_panel(&ctx.style())
            .fill(P::panel())
            .inner_margin(Margin::symmetric(20, 0));
        let mut want_toggle = false;
        egui::TopBottomPanel::top("aox-top")
            .exact_height(50.0)
            .frame(frame)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // 历史侧边栏的开关放在最左：面板就在左边，开关也跟着在左边，
                    // 不必让眼睛横跨整个顶栏去找它。
                    if history_icon(ui, self.show_history).clicked() {
                        self.show_history = !self.show_history;
                        if self.show_history {
                            self.refresh_history();
                        }
                    }
                    ui.add_space(2.0);
                    // 「+」紧挨着列表图标：一个管「翻哪段」，一个管「开一段」，
                    // 放在一起才是完整的一组。
                    let locked = self.busy || self.switching;
                    if plus_icon(ui, locked).clicked() && !locked {
                        self.new_conversation();
                    }
                    // 左侧到这里为止 —— 原先那枚状态圆点与「就绪」字样都去掉了：
                    // 空闲时它只是常驻噪音，忙碌时活动流里其实写得更细。
                    // 唯一保留的是任务执行中的那几秒，那会儿进度确实值得一个位置。
                    // （品牌名也不在这里：原生标题栏本来就写着「AOX · 模型」，
                    // 紧挨着两行再写一次是重复。）
                    if self.busy {
                        ui.add_space(12.0);
                        ui.label(RichText::new(&self.status).color(P::text()).size(14.0));
                        ui.add_space(7.0);
                        ui.label(
                            RichText::new(format!("{:.1}s", self.live.as_ref().map_or(0.0, |l| {
                                l.started.elapsed().as_secs_f32()
                            })))
                            .color(P::text_faint())
                            .size(13.5),
                        );
                        // 累计 token 实时滚动 = 已完成各轮（精确）＋ 本轮进行中的估算。
                        // 所以整个对话的账在任务执行期间也是一路在涨的，而不是
                        // 每轮归零。带 `~` 是因为后半截还在估，一轮结束后由顶栏
                        // 那枚精确的总数接手。
                        let base = self.usage_sum().total.unwrap_or(0);
                        let est = self.live.as_ref().map_or(0, |l| l.out.tokens());
                        let shown = base + est;
                        if shown > 0 {
                            ui.add_space(7.0);
                            ui.label(
                                RichText::new(format!("Σ ~{} tok", thousands(shown)))
                                    .color(P::text_faint())
                                    .size(13.5),
                            );
                        }
                    } else {
                        // 空闲时只剩一个总 token，鼠标移上去就有明细
                        self.token_chip(ui, ctx);
                    }

                    // 右上角只留三个图标：活动流开关 + 明暗切换 + 关于。
                    // 原来那排「模型 / 工具 N / 插件 N / 演示」的胶囊全部去掉——
                    // 这些信息要么在活动流里能看到，要么属于看一眼就够的诊断数据。
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if theme_toggle(ui, theme::mode() == theme::Mode::Dark).clicked() {
                            want_toggle = true;
                        }
                        ui.add_space(2.0);
                        if flow_toggle(ui, activity::lock(&self.act).is_open()).clicked() {
                            activity::toggle(ctx, &self.act);
                        }
                        ui.add_space(2.0);
                        if about_icon(ui).clicked() {
                            self.about_open = true;
                        }
                    });
                });
            });
        if want_toggle {
            theme::apply_mode(ctx, theme::toggle_mode());
        }
    }

    /// 顶栏左侧那枚「总 token」胶囊：**悬停**即展开合计，不必点。
    ///
    /// 用悬停而不是点击，是因为这个数字本来就是「瞟一眼」的东西：想确认的人
    /// 鼠标移过去就有了，而点击反而要求他先认出「这里可以点」。
    fn token_chip(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let Some(total) = self.usage_sum().total else { return };
        let resp = ui.add(
            Label::new(
                RichText::new(format!("Σ {} tok", thousands(total)))
                    .color(P::text_dim())
                    .size(13.5),
            )
            .sense(Sense::hover()),
        );
        if resp.hovered() {
            self.usage_hover = true;
        }
        if !self.usage_hover {
            return;
        }
        // 面板就贴在胶囊正下方 6px：指针从胶囊走到面板的路上始终压着其中之一，
        // 中间不会有一帧落空，也就不会闪。
        //
        // ⚠️ **坐标不要做任何换算**：`Area::fixed_pos` 与 `ui.allocate_*` 同处
        // **客户区局部坐标**（`Area::begin` 拿 `ctx.content_rect()` 当约束）。
        // 这里曾经加过一次「视口原点」`viewport().inner_rect.left_top()` ——
        // 那是**屏幕（显示器）坐标**，两者根本不在一个坐标系，换算不来；
        // 结果是面板随窗口在显示器上的位置整体平移（窗口不在 (0,0) 时实测偏
        // ~(154,14) 逻辑像素）。同一处错误在 `workplace_ui::menu` 也犯过，已一并修掉。
        let anchor = resp.rect.left_bottom() + egui::vec2(0.0, 6.0);
        // 分开借：闭包里要写 hover 状态、又要读账本
        let log = &self.usage_log;
        let hovering = &mut self.usage_hover;
        egui::Area::new(egui::Id::new("aox-usage-panel"))
            .fixed_pos(anchor)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let frame = Frame::popup(ui.style())
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(10))
                    .stroke(Stroke::new(1.0, P::border()))
                    .inner_margin(Margin::symmetric(16, 13))
                    .show(ui, |ui| usage_detail(ui, log));
                *hovering = resp.hovered() || ui.rect_contains_pointer(frame.response.rect);
            });
    }

    /// 整段对话的累计 token（顶栏那枚 Σ 显示的数）。
    ///
    /// 直接取最后一条记录里的 `session_total` —— 那是落盘时就写好的累计值，
    /// 不用每次把整本账重算一遍。
    /// 当前对话各轮的**合计**（逐项累加）。
    ///
    /// 顶栏与展开面板都用它 —— 一个数说「花了多少」，一整套说「花在哪」。
    fn usage_sum(&self) -> Usage {
        sum_usage(&self.usage_log)
    }

    /// 把账本里的每一轮明细插回对话流，插在那一轮 AI 答复之后。
    ///
    /// 定位靠「每条用户消息开启一轮」这个天然分界：第 N 轮的明细落在第 N 条用户
    /// 消息与下一条之间（没有下一条就落末尾）。这样打开一段旧对话时，每一轮的账
    /// 都贴回它原来待着的位置，而不是在顶上堆成一坨。
    fn rebuild_usage_rows(&mut self) {
        self.items.retain(|it| !matches!(it, Item::Usage(_)));
        if self.usage_log.is_empty() {
            return;
        }
        // 先算出每一轮内容的结束下标
        let mut ends: Vec<usize> = Vec::new();
        for (i, it) in self.items.iter().enumerate() {
            if matches!(it, Item::User(_)) {
                if let Some(prev) = ends.len().checked_sub(1) {
                    ends[prev] = i.saturating_sub(1);
                }
                ends.push(i);
            }
        }
        if let Some(last) = ends.last_mut() {
            *last = self.items.len().saturating_sub(1);
        }
        // 倒序插入：后面的轮次先插，前面轮次的下标才不会被带偏
        for rec in self.usage_log.iter().rev() {
            let Some(&end) = ends.get(rec.round.saturating_sub(1) as usize) else {
                continue;
            };
            self.items.insert(end + 1, Item::Usage(usage_line(rec)));
        }
    }

    fn transcript(&mut self, ui: &mut Ui) {
        let avail = ui.available_width();
        let (inner, pad) = column(avail);
        let out = ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(self.follow || self.force_follow)
            .show(ui, |ui| {
                ui.add_space(26.0);
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    ui.vertical(|ui| {
                        ui.set_max_width(inner);
                        ui.set_min_width(inner);
                        // 版心左右边界：宽 = inner，右端到顶自动换行。
                        //
                        // egui 的 `Ui::wrap_mode()` 在「非换行水平布局」里返回 `Extend`，
                        // 而 `Label` 默认会继承它 —— 结果是长文本一行到底、冲出右边界被裁。
                        // 气泡 / 列表项 / 工具调用行内部全是这种水平布局，所以在栏根上一把锁死，
                        // 后代 Ui（含 Frame 内部）一律继承 `Wrap`。
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                        // 只要还没开始真正的对话，就先展示品牌区
                        // （此时 items 里可能已有加载/错误提示）
                        let started = self
                            .items
                            .iter()
                            .any(|i| matches!(i, Item::User(_) | Item::Assistant(_)));
                        if !started && self.live.is_none() {
                            self.empty_state(ui);
                        }
                        for item in &mut self.items {
                            match item {
                                Item::User(t) => user_bubble(ui, t, inner * 0.80),
                                Item::Assistant(t) => assistant_block(ui, t, inner),
                                Item::Process(p) => process_block(ui, p, inner),
                                Item::Note(k, t) => note_row(ui, *k, t),
                                Item::Usage(t) => usage_row(ui, t),
                                Item::Error(t) => error_card(ui, t, inner),
                            }
                        }
                        if let Some(l) = &self.live {
                            self.live_block(ui, l, inner);
                        }
                        // 末尾留出悬浮输入框的高度，最后一条消息才能滚到它上方，
                        // 而不是永远被压住半截。
                        ui.add_space(COMPOSER_RESERVE);
                    });
                });
            });
        let max = (out.content_size.y - out.inner_rect.height()).max(0.0);
        self.follow = out.state.offset.y >= max - 48.0;
        if self.force_follow {
            self.follow = true;
            self.force_follow = false;
        }
    }

    fn empty_state(&mut self, ui: &mut Ui) {
        ui.add_space(52.0);
        ui.vertical_centered(|ui| {
            // 首屏只有一句招呼：艺术体大字，不加分隔线、不加副标题。
            let (rect, _) = ui.allocate_exact_size(Vec2::new(320.0, 62.0), Sense::hover());
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Hi there",
                theme::brand_font(46.0),
                P::text(),
            );
            // 原有「细横线 + 副标题」的位置整块留白（4 + 1 + 16 + 21 + 40），
            // 行去掉了但高度照旧 —— 否则下面的预设会整体往上跳一截。
            ui.add_space(82.0);

            let examples = [
                "看看现在有哪些插件和工具",
                "写一个插件，统计当前目录下各类文件的数量",
                "在电脑桌面建一个.docx文档介绍一下你",
            ];
            for ex in examples {
                if example_chip(ui, ex).clicked() {
                    self.input = ex.to_string();
                    self.focus_input = true;
                }
                ui.add_space(10.0);
            }

            if let Some(err) = &self.log_error {
                ui.add_space(38.0);
                let w = 640.0_f32.min(ui.available_width() - 8.0);
                Frame::NONE
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(theme::card_radius()))
                    .stroke(Stroke::new(1.0, P::border_strong()))
                    .inner_margin(Margin::symmetric(22, 18))
                    .show(ui, |ui| {
                        ui.set_max_width(w);
                        Frame::NONE
                            .fill(P::reverse_bg())
                            .corner_radius(CornerRadius::same(7))
                            .inner_margin(Margin::symmetric(11, 4))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new("模型未就绪")
                                        .color(P::on_reverse())
                                        .size(14.0),
                                );
                            });
                        ui.add_space(10.0);
                        ui.label(RichText::new(err).color(P::text_dim()).size(14.5));
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(format!("配置文件：{}", self.config_path))
                                .color(P::text_faint())
                                .size(13.5),
                        );
                        ui.label(
                            RichText::new("填写 [provider] 的 base_url / api_key 后重新启动。")
                                .color(P::text_faint())
                                .size(13.5),
                        );
                    });
            }
        });
    }

    fn live_block(&self, ui: &mut Ui, l: &Live, width: f32) {
        let _ = width;
        // 进行中的工具调用
        if !l.steps.is_empty() {
            Frame::NONE
                .fill(P::surface())
                .corner_radius(CornerRadius::same(theme::card_radius()))
                .stroke(Stroke::new(1.0, P::border()))
                .inner_margin(Margin::symmetric(20, 16))
                .show(ui, |ui| {
                    // 用框架内部的可用宽度，避免内容把卡片顶出栏宽
                    let w = ui.available_width();
                    ui.set_width(w);
                    ui.set_max_width(w);
                    for (i, st) in l.steps.iter().enumerate() {
                        step_row(ui, i + 1, st, w);
                    }
                });
            ui.add_space(16.0);
        }

        // 等待动画 + 状态
        ui.horizontal(|ui| {
            spinner_dot(ui, self.spin, P::ink());
            ui.add_space(8.0);
            ui.label(
                RichText::new(if l.status.is_empty() { "思考中" } else { &l.status })
                    .color(P::text_dim())
                    .size(15.0),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!("{:.1}s", l.started.elapsed().as_secs_f32()))
                    .color(P::text_faint())
                    .size(13.0),
            );
            // 输出 token 实时滚动。带 `~` 是因为流式过程中服务端还没报数，
            // 这个值按字符估；任务结束后顶栏会换成精确的总数。
            let est = l.out.tokens();
            if est > 0 {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("~{} tok", thousands(est)))
                        .color(P::text_faint())
                        .size(13.0),
                );
            }
        });

        // 实时思考过程（尾部几行，响应结束即消失）
        if l.reasoning_live && !l.reasoning.is_empty() {
            ui.add_space(12.0);
            let tail = tail_lines(&l.reasoning, 4);
            Frame::NONE
                .fill(P::code_bg())
                .corner_radius(CornerRadius::same(12))
                .stroke(Stroke::new(1.0, P::border()))
                .inner_margin(Margin::symmetric(16, 13))
                .show(ui, |ui| {
                    let w = ui.available_width();
                    ui.set_width(w);
                    ui.set_max_width(w);
                    let mut job = egui::text::LayoutJob::default();
                    job.wrap.max_width = w;
                    job.wrap.break_anywhere = true;
                    job.append(
                        &tail,
                        0.0,
                        egui::TextFormat {
                            font_id: theme::body_font(14.5),
                            color: P::text_faint(),
                            ..Default::default()
                        },
                    );
                    ui.add(Label::new(job));
                });
        }
        ui.add_space(16.0);
    }

    /// 把工作区按钮产生的一条请求发给 Agent 线程。
    ///
    /// 界面自己切不了档：`Workplace` 住在 `Runtime` 里，而 `Runtime` 只有 Agent 那个
    /// 线程拿得到（见 `bridge.rs` 的 `ToAgent::Workplace`）。所以这里只负责「发出去」，
    /// 真正的结果与新档位由 bridge 回执（`FromAgent::Workplace`）驱动。
    fn send_ws(&mut self, mode: Mode) {
        if self.demo {
            // 演示模式没有 Agent 线程来回执，就地模拟一下 —— 好让 `--demo` 也能把
            // 「切档 → 按钮改名 + 列表打勾跟着走」演出来。只影响界面，不碰磁盘。
            let (view_mode, root) = match &mode {
                Mode::Default => (Mode::Default, None),
                Mode::Fixed(p) => (Mode::Fixed(p.clone()), Some(p.clone())),
                Mode::Full => (Mode::Full, None),
            };
            self.ws.set_view(WorkplaceView { mode: view_mode, root });
            return;
        }
        let _ = self.tx.send(ToAgent::Workplace(mode));
    }

    fn composer(&mut self, ui: &mut Ui) {
        let (inner, pad) = column(ui.available_width());
        let blocked = self.demo || self.boot.as_ref().is_some_and(|b| !b.provider_ready);
        let can_send = !self.busy && !blocked && !self.waiting_auth && !self.input.trim().is_empty();

        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.vertical(|ui| {
                ui.set_width(inner);
                ui.set_max_width(inner);

                // 悬浮输入框：没有分隔线、四周也不铺底色，靠大圆角 + 一圈细描边 + 一层淡投影
                // 浮在对话区之上（这就是「背景掏空」的观感来源）。
                Frame::NONE
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(20))
                    .stroke(Stroke::new(
                        1.0,
                        if self.busy || self.waiting_auth { P::border_strong() } else { P::border() },
                    ))
                    .shadow(egui::Shadow {
                        offset: [0, 5],
                        blur: 18,
                        spread: 0,
                        color: P::shadow(),
                    })
                    .inner_margin(Margin::symmetric(22, 15))
                    .show(ui, |ui| {
                        let w = ui.available_width();
                        ui.set_width(w);
                        ui.set_max_width(w);
                        // 注意：这里不能用 ui.horizontal_centered()——它会一次性申请「全部可用尺寸」，
                        // 让自动定高的底栏面板不断膨胀（实测会把整个窗口吃掉）。ui.horizontal 同样
                        // 会做交叉轴居中，但初始高度只有一行，不会反馈放大。
                        ui.horizontal(|ui| {
                            let hint = if self.demo {
                                "演示模式：仅预览界面排布（以 --demo 启动；正常启动即可对话）"
                            } else if blocked {
                                "模型未配置，无法对话"
                            } else if self.waiting_auth {
                                "等待授权确认…"
                            } else if self.busy {
                                "正在执行，可点击右侧停止"
                            } else {
                                "给 AOX 下达指令…"
                            };
                            // 显式扣掉发送按钮的宽度，避免输入框把按钮挤出卡片
                            let edit_w = (w - 62.0).max(120.0);
                            let text_edit = TextEdit::multiline(&mut self.input)
                                .desired_rows(1)
                                .desired_width(edit_w)
                                .frame(false)
                                .hint_text(RichText::new(hint).color(P::text_faint()).size(16.0));
                            let resp = ui.add(text_edit);
                            if self.focus_input {
                                resp.request_focus();
                                self.focus_input = false;
                            }

                            let ime =
                                ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Ime(_))));
                            let enter = ui.input(|i| {
                                i.key_pressed(egui::Key::Enter) && !i.modifiers.shift && !i.modifiers.alt
                            });
                            if resp.has_focus() && enter && !ime && can_send {
                                self.send();
                            }

                            ui.add_space(8.0);
                            if self.busy {
                                if stop_button(ui).clicked() {
                                    self.stop();
                                }
                            } else if send_button(ui, can_send).clicked() && can_send {
                                self.send();
                            }
                        });

                        // 第二行：输入框**左下角**的工作区按钮。
                        //
                        // 单独占一行而不是挤进上一行：它说的是「这段对话跑在什么权限下」，
                        // 与「输入 + 发送」是两件事；挤在一起会把输入框挤窄，而且这枚按钮
                        // 紧挨着发送圆钮也容易被误点（一个只是「发消息」，一个是「放开所有权限」）。
                        //
                        // 演示模式下**允许点**（`self.demo || ...`）：它弹出来的列表 / 确认框 /
                        // 目录选择器全是纯界面，就地模拟一下就能把版式演完（见 `send_ws`）。
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            workplace_ui::button(ui, &mut self.ws, self.demo || !blocked);
                        });
                    });
            });
        });
    }

    /// 关于：一枚居中的小卡片。点背景、按 Esc 或点「关闭」都收起。
    ///
    /// 刻意不写版本史、构建时间、依赖清单这些看起来「专业」的噪音，
    /// 只留版本号、一句话定位和项目地址。
    fn about_overlay(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let mut open = true;
        let resp = egui::Modal::new(egui::Id::new("aox-about"))
            .frame(
                Frame::NONE
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(theme::card_radius()))
                    .stroke(Stroke::new(1.0, P::border()))
                    .inner_margin(Margin::symmetric(26, 22)),
            )
            .show(ctx, |ui| {
                ui.set_width(380.0);

                // 刊头：衬线体的 AOX + 版本号，底下一条细线，和空状态同一个语汇
                ui.horizontal(|ui| {
                    ui.label(RichText::new("AOX GUI").font(theme::serif_font(26.0)).color(P::text()));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .color(P::text_faint())
                                .size(13.5),
                        );
                    });
                });
                ui.add_space(9.0);
                let (rule, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
                ui.painter()
                    .hline(rule.x_range(), rule.center().y, Stroke::new(1.0, P::border()));

                ui.add_space(13.0);
                ui.label(
                    RichText::new("AOX 的图形化应用，基于 Rust 编写。")
                        .color(P::text_dim())
                        .size(15.0),
                );

                ui.add_space(14.0);
                let link = ui.add(
                    Label::new(
                        RichText::new(aox::ui::style::PROJECT_URL)
                            .color(P::text())
                            .size(14.5)
                            .underline(),
                    )
                    .sense(Sense::click()),
                );
                if link.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if link.clicked() {
                    ui.ctx()
                        .open_url(egui::OpenUrl::new_tab(aox::ui::style::PROJECT_URL));
                }

                // 当前模型：原先缀在窗口标题里，标题去掉「· xxx」之后挪到这儿，
                // 和下面的运行时信息同属「看一眼就够」的诊断数据。
                if !self.model.is_empty() {
                    ui.add_space(18.0);
                    ui.label(
                        RichText::new(format!("模型  {}", self.model))
                            .font(theme::mono_font(12.5))
                            .color(P::text_faint()),
                    );
                }

                // 配置层：这份配置是「合」出来的，还是只有 config.toml 一份。
                // 只显示名字链（去掉 bundle: 前缀），不在这里铺来源树 —— 完整来源树走
                // CLI 的 `aox --dump-config`。只有一层时不显示，说了等于没说。
                let layers: Vec<String> = self
                    .boot
                    .as_ref()
                    .map(|b| {
                        b.config_layers
                            .iter()
                            .map(|l| l.strip_prefix("bundle:").unwrap_or(l).to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                if layers.len() > 1 {
                    ui.add_space(18.0);
                    ui.label(
                        RichText::new(format!("配置层  {}", layers.join(" → ")))
                            .font(theme::mono_font(12.5))
                            .color(P::text_faint()),
                    );
                }

                // 插件运行时：这类「看一眼就够」的诊断信息放在关于窗口里，
                // 主界面不额外加胶囊。缺 Python 时才多出一个安装入口。
                if !self.runtimes.lines.is_empty() {
                    ui.add_space(18.0);
                    let (rule, _) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), 1.0),
                        Sense::hover(),
                    );
                    ui.painter()
                        .hline(rule.x_range(), rule.center().y, Stroke::new(1.0, P::border()));

                    ui.add_space(12.0);
                    ui.label(RichText::new("插件运行时").color(P::text_dim()).size(14.0));
                    ui.add_space(6.0);
                    for l in &self.runtimes.lines {
                        ui.label(
                            RichText::new(l)
                                .font(theme::mono_font(12.5))
                                .color(P::text_faint()),
                        );
                    }
                    if self.runtimes.python_missing {
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new("本机没有 Python，文件读写、系统信息等能力不可用。")
                                .color(P::text_dim())
                                .size(13.5),
                        );
                        ui.add_space(8.0);
                        if primary_button(ui, "安装 Python 运行时（约 10 MB）").clicked() {
                            // 走与 CLI 完全相同的命令路径（宿主自己处理，不发给模型）
                            let _ = self.tx.send(ToAgent::Input("/runtime install python".into()));
                            open = false;
                        }
                    }
                }

                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if flat_button(ui, "关闭").clicked() {
                            open = false;
                        }
                    });
                });
            });
        // 点背景、按 Esc 也算关闭
        if resp.should_close() || !open {
            self.about_open = false;
        }
    }

    fn auth_overlay(&mut self, ctx: &egui::Context) {
        let Some(card) = self.auth.take() else { return };
        let mut decision: Option<AuthDecision> = None;
        egui::Modal::new(egui::Id::new("aox-auth")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.label(
                RichText::new("工具调用需要授权")
                    .color(P::text())
                    .size(17.0)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("模型想调用一个会改变系统自身的元工具：")
                    .color(P::text_faint())
                    .size(14.0),
            );
            ui.add_space(8.0);
            ui.label(RichText::new(&card.tool).color(P::ink()).size(16.0));
            ui.add_space(8.0);
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = 440.0;
            job.wrap.break_anywhere = true;
            job.append(
                &clamp_str(&card.args, 1600),
                0.0,
                egui::TextFormat {
                    font_id: theme::mono_font(14.0),
                    color: P::text_dim(),
                    ..Default::default()
                },
            );
            Frame::NONE
                .fill(P::code_bg())
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.add(Label::new(job));
                });
            ui.add_space(12.0);
            // 键盘快捷键与 CLI 完全一致：y 允许一次 / a 本会话始终允许 / n 拒绝。
            // 加 250ms 去抖：弹窗刚出现时忽略按键，避免上一条消息的 Enter 等残留事件误触。
            let settled = self.auth_shown_at.elapsed() > Duration::from_millis(250);
            if settled {
                if ui.input(|i| i.key_pressed(egui::Key::Y)) {
                    decision = Some(AuthDecision::Allow);
                }
                if ui.input(|i| i.key_pressed(egui::Key::A)) {
                    decision = Some(AuthDecision::AllowSession);
                }
                if ui.input(|i| i.key_pressed(egui::Key::N) || i.key_pressed(egui::Key::Escape)) {
                    decision = Some(AuthDecision::Deny);
                }
            }
            ui.horizontal(|ui| {
                if primary_button(ui, "允许一次").clicked() {
                    decision = Some(AuthDecision::Allow);
                }
                if flat_button(ui, "本会话始终允许").clicked() {
                    decision = Some(AuthDecision::AllowSession);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if flat_button(ui, "拒绝").clicked() {
                        decision = Some(AuthDecision::Deny);
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new("快捷键：y 允许一次 · a 本会话始终允许 · n / Esc 拒绝")
                    .color(P::text_faint())
                    .size(12.5),
            );
        });

        match decision {
            Some(d) => {
                if let Some(tx) = self.auth_reply.take() {
                    let _ = tx.send(d);
                }
                self.waiting_auth = false;
                self.note_act(
                    ActKind::Info,
                    "授权结果",
                    match d {
                        AuthDecision::Allow => "允许一次",
                        AuthDecision::AllowSession => "本会话始终允许",
                        AuthDecision::Deny => "拒绝",
                    },
                );
            }
            None => self.auth = Some(card),
        }
    }
}

// ─────────────────────────── 演示数据（--demo） ───────────────────────────

/// 与 CLI 的 `/demo` 对应：不连真实模型也能预览界面排布与事件流的形态。
fn seed_demo(app: &mut App) {
    app.model = "deepseek-chat".into();
    app.config_path = "config.toml".into();
    app.plugins = vec!["filestat".to_string()];
    app.status = "就绪".into();
    // 演示模式也从真实的应用根读历史：预览本来就该是真实的样子，
    // 编几条假对话反而会让人以为侧边栏是装饰品。
    let root = aox::paths::resolve_workplace().0;
    let store = aox::store::default_store(&root, 0);
    app.boot = Some(BootInfo {
        config_path: app.config_path.clone(),
        // 演示数据也照真实形态给一条层链，好让「关于」窗口那一行在 --demo 里也看得见。
        config_layers: vec!["bundle:core".into(), "config.toml".into()],
        model: app.model.clone(),
        provider_ready: true,
        note: None,
        store: store.clone(),
    });
    app.store = Some(store);
    app.refresh_history();

    // 工作区按钮的初值：演示模式下 bridge 不会发 `FromAgent::Workplace`，
    // 就地塞一份，好让 `--demo` 也能把按钮上的档位名与列表里打勾的那一项演出来。
    app.ws.set_view(WorkplaceView { mode: Mode::Default, root: None });

    // 顶栏那枚「Σ 用量」胶囊**只在有账本时才画**，而 `--demo` 没有真实调用 ——
    // 补两条账，好让预览里也能看到它与被悬停展开的明细面板（也才验得了面板的位置）。
    app.usage_log = vec![
        UsageRecord {
            round: 1,
            at: 0,
            calls: 2,
            usage: Usage {
                prompt: Some(9_312),
                completion: Some(1_204),
                total: Some(10_516),
                reasoning: Some(486),
                cached: Some(2_048),
                cache_miss: Some(7_264),
            },
            last_prompt: Some(9_312),
            session_total: Some(10_516),
            session_calls: 2,
        },
        UsageRecord {
            round: 2,
            at: 0,
            calls: 4,
            usage: Usage {
                prompt: Some(24_608),
                completion: Some(2_174),
                total: Some(26_782),
                reasoning: Some(1_130),
                cached: Some(6_144),
                cache_miss: Some(18_464),
            },
            last_prompt: Some(24_608),
            session_total: Some(37_298),
            session_calls: 6,
        },
    ];

    app.items.push(Item::User("做一个插件，统计当前目录下各类文件的数量".into()));
    app.items.push(Item::Process(Process {
        steps: vec![
            StepView {
                name: "list_plugins".into(),
                plugin: Some("meta".into()),
                args: "{}".into(),
                result: Some((false, "已加载插件: probe_screen, fs".into())),
            },
            StepView {
                name: "create_plugin".into(),
                plugin: Some("meta".into()),
                args: r#"{"name":"filestat","language":"python"}"#.into(),
                result: Some((false, "插件 filestat 已创建、注册并持久化（1 个工具）".into())),
            },
            StepView {
                name: "filestat_count".into(),
                plugin: Some("filestat".into()),
                args: r#"{"path":".","ext":null}"#.into(),
                result: Some((false, ".rs 14 · .py 9 · .toml 4 · .md 2 · .lock 1".into())),
            },
        ],
        interim: vec!["先看看有没有现成的能力可以复用。".into()],
        reasoning: "用户要统计文件数量：可以读目录 + 按后缀聚合。宿主里没有这个能力，\
                    需要先 create_plugin 造一个 filestat 插件，再调用它。"
            .into(),
        elapsed: Some(7.4),
        expanded: false,
        aborted: false,
    }));
    app.items.push(Item::Assistant(
        "已经为你造好了 `filestat` 插件，它提供工具 **filestat_count**。\n\n\
         ## 用法\n\n\
         ```json\n{ \"path\": \".\", \"ext\": null }\n```\n\n\
         `ext` 留空表示统计全部后缀。当前目录的结果：\n\n\
         | 后缀 | 数量 |\n|------|------|\n| .rs | 14 |\n| .py | 9 |\n| .toml | 4 |\n| .md | 2 |\n\n\
         > 插件已持久化到 `plugins/filestat/`，下次启动会自动加载，无需重建。"
            .into(),
    ));
    // ▼▼▼ 诊断用：超宽内容（长代码行 / 长内联代码 / 长词 / 列表 / 表格）
    //      —— 验证「内容比版心宽」时是否会在右端边界处换行，而不是被裁切。
    app.items.push(Item::Assistant(
        "## 超宽内容\n\n\
         ```json\n{\"very_long_key_that_never_wraps\":\"C:/Users/someone/AppData/Local/Temp/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json\",\"n\":1}\n```\n\n\
         路径 `E:/rustapp/plugins/filestat/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.py` 很长。\n\n\
         ### 连通性实测\n\n\
         - `httpfetch_raw` → 请求 bilibili.com 返回 HTTP 200，成功抓取到页面文本（共 183539 字符，已截断），网络与插件进程通信正常。\n\n\
         - `bilibili_video_info` → 拉取 BV1GJ411x7h7 成功，返回完整元数据（标题「【官方 MV】Never Gonna Give You Up - Rick Astley」、UP 主「索尼音乐中国」、播放 1.06 亿）。\n\n\
         | 插件 | 语言 | 提供工具 |\n| --- | --- | --- |\n\
         | bilibili | python | bilibili_video_info（视频元数据）、bilibili_danmaku（弹幕）、bilibili_comments（评论）、bilibili_subtitle（字幕）、bilibili_related（相关推荐） |\n\
         | httpfetch | python | httpfetch_raw（通用 GET 请求，用于接口调试/抓取网页） |\n\n\
         结论：无需新建或修复任何插件，现有能力已覆盖 B 站视频信息抓取 + 通用网页请求两类需求，可直接接活。若后续需要文件读写、命令执行、数据导出（如 CSV/Excel）等能力，再按需扩展。\n\n\
         aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .into(),
    ));
    // ▲▲▲ 诊断用
    app.items.push(Item::Note(
        NoteKind::Info,
        "插件 filestat 已恢复（1 个工具）".into(),
    ));

    // 活动流：按时间顺序铺一遍典型信息流（横向时间轴：色块宽度 = 真实耗时）。
    // 演示模式把活动流窗口一并打开 —— 它已经是界面的一大块，预览就该看到它。
    {
        let mut a = activity::lock(&app.act);
        a.force_open();
        a.round_begin("做一个插件，统计当前目录下各类文件的数量", 0.0);
        a.model_begin(0.5);
        a.model_push("用户要统计文件数量：可以读目录 + 按后缀聚合。宿主里没有这个能力，");
        a.model_push("需要先 create_plugin 造一个 filestat 插件，再调用它。");
        a.tool_begin("list_plugins", Some("meta"), "{}", 1.7);
        a.tool_end(false, "已加载插件: probe_screen, fs", 2.0);
        a.model_begin(2.1);
        a.model_push("先看看有没有现成的能力可以复用。");
        a.tool_begin(
            "create_plugin",
            Some("meta"),
            r#"{"name":"filestat","language":"python"}"#,
            2.8,
        );
        a.tool_note("超时上限 120s");
        a.note(ActKind::Auth, "请求授权 · create_plugin", "{\"name\":\"filestat\"}", 3.6);
        a.note(ActKind::Info, "授权结果", "允许一次", 5.8);
        a.tool_end(false, "插件 filestat 已创建、注册并持久化（1 个工具）", 6.4);
        a.model_begin(6.5);
        a.tool_begin("filestat_count", Some("filestat"), r#"{"path":".","ext":null}"#, 7.4);
        a.tool_end(false, ".rs 14 · .py 9 · .toml 4 · .md 2 · .lock 1", 8.6);
        a.model_begin(8.7);
        a.model_push("结果已经拿到，整理成一张表给用户。");
        a.end_round(11.9);
        a.tick(11.9, "就绪", false);

        // 第二轮：跨轮次、跨插件的连线效果也一并演示
        a.round_begin("把刚才那个插件的用法写进 README", 13.2);
        a.model_begin(13.8);
        a.model_push("README 里加一节「filestat 用法」即可。");
        a.tool_begin("fs_write", Some("fs"), r#"{"path":"README.md"}"#, 14.6);
        a.tool_end(false, "已写入 1.2 KB", 15.4);
        a.model_begin(15.5);
        a.model_push("写完了。");
        a.end_round(18.1);
        a.tick(18.1, "就绪", false);
    }
}

/// 主窗口的默认尺寸：按显示器大小算，留出边框与任务栏，再夹到人能看的区间。
///
/// 活动流窗口要「和主窗口一样大」，所以公式只写这一份 —— 两边各算一次必然漂移。
pub(crate) fn default_window_size(mon: Vec2, chrome: Vec2) -> Vec2 {
    // 任务栏高度没有 API 可查，按显示器高度取一个保守比例（默认约 48 逻辑点）
    let taskbar = (mon.y * 0.06).clamp(36.0, 72.0);
    // 贴边留一点呼吸空间
    let margin = Vec2::new(80.0, 28.0);
    Vec2::new(
        (mon.x - margin.x - chrome.x).min(1380.0).max(1000.0),
        (mon.y - taskbar - margin.y - chrome.y).min(900.0).max(620.0),
    )
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.fit_window(ctx);
        self.pump();

        // 原生标题栏只留窗口标题本身：图标不设、标题里也不再缀模型名，
        // 免得左上角挤着一行「AOX · xxx」。底色同步见下面 sync_native_theme。
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        // `switching` 也要重绘：切换的回执从 agent 线程异步回来，空闲窗口
        // 默认不会自己重画，少了这一条就会「点了没反应」，直到下次鼠标一动。
        if self.busy || self.waiting_auth || self.live.is_some() || self.switching {
            self.spin += dt;
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        self.top_bar(ctx);

        if self.native_theme != Some(theme::mode()) {
            self.native_theme = Some(theme::mode());
            theme::sync_native_theme(ctx);
            theme::sync_caption(frame);
        }

        if self.show_history {
            self.history_panel(ctx);
        }
        // 活动流在独立窗口里：主窗口每帧把「现在在干什么」同步过去，并重新声明
        // 那个视口（egui 的视口是声明式的，不声明就会被回收）。
        activity::lock(&self.act).tick(self.now(), &self.status, self.busy);
        activity::sync(ctx, &self.act);
        let frame = Frame::NONE.fill(P::bg());
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let column_rect = ui.max_rect();
            // 对话区铺满整个中央面板，不再给输入框让出底栏
            self.transcript(ui);

            // 输入框改为一枚悬浮卡片（Foreground 层），直接压在对话内容之上。
            // 之所以不放在 TopBottomPanel::bottom 里：那会永久占掉一条底栏，
            // 面板底色与背景同色，看上去就是底部空出一片区域。
            // constrain_to 在这里有两个作用：把裁切区域限制在本栏内（不会盖到下一层内容），
            // 以及让 anchor 的居中参考系正好是本栏。
            let clip = Rect::from_min_max(
                column_rect.min,
                Pos2::new(column_rect.right(), column_rect.bottom()),
            );
            egui::Area::new(egui::Id::new("aox-composer"))
                .order(egui::Order::Foreground)
                .movable(false)
                .constrain_to(clip)
                .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, -18.0))
                .show(ctx, |ui| {
                    ui.set_width(column_rect.width());
                    self.composer(ui);
                });
        });

        // 工作区的三个浮层：向上弹出的三选一列表、完全访问的确认框、目录选择器。
        // 都排在 CentralPanel 之后 —— 它们要盖在对话内容与输入卡片之上。
        if let Some(mode) = workplace_ui::menu(ctx, &mut self.ws) {
            self.send_ws(mode);
        }
        if let Some(mode) = workplace_ui::confirm(ctx, &mut self.ws) {
            self.send_ws(mode);
        }
        if let Some(mode) = workplace_ui::picker(ctx, &mut self.ws) {
            self.send_ws(mode);
        }

        self.about_overlay(ctx);
        self.auth_overlay(ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let c = P::bg();
        [
            c.r() as f32 / 255.0,
            c.g() as f32 / 255.0,
            c.b() as f32 / 255.0,
            1.0,
        ]
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.flags.request_abort();
        let _ = self.tx.send(ToAgent::Shutdown);
        if let Some(h) = self.driver.take() {
            // 有界等待：给插件子进程一点时间优雅退出，但绝不卡死界面退出
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            std::thread::spawn(move || {
                let _ = h.join();
                let _ = done_tx.send(());
            });
            let _ = done_rx.recv_timeout(Duration::from_millis(1500));
        }
    }
}

// ─────────────────────────── 小组件 ───────────────────────────

/// 对话栏的版心：返回 (内容宽度, 左右留白)。
/// 行宽过宽会很难读，所以设了上限并在窗口里居中。
pub(crate) fn column(available: f32) -> (f32, f32) {
    let inner = (available - 96.0).min(880.0).max(280.0);
    let pad = ((available - inner) * 0.5).max(18.0);
    (inner, pad)
}

/// 悬浮输入框在纵向占用的空间（卡片高度 + 下边距 + 投影余量）。
/// 对话流末尾留出这段空白，保证最后一条消息不会被卡片压住。
const COMPOSER_RESERVE: f32 = 118.0;

/// 侧边栏一次最多列出多少个历史对话。
///
/// 不做分页：几十条已经够「翻回去找刚才那段」了，而列全部意味着每次刷新
/// 都要扫完整个目录。真要翻更早的，`/history` 与 `history/` 目录都在。
const HISTORY_LIST_LIMIT: usize = 80;

/// 历史记录按钮：左侧一条竖线（代表侧边栏）+ 三条短横线。
///
/// 与另外两个顶栏图标同一套画法（36px 热区、单色描边、悬停换色）。
fn history_icon(ui: &mut Ui, on: bool) -> egui::Response {
    let d = 36.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(d), Sense::click());
    let hov = pointer_if(ui, &resp);
    let fg = if hov {
        P::ink()
    } else if on {
        P::text()
    } else {
        P::text_faint()
    };

    let painter = ui.painter();
    if hov {
        painter.circle_filled(rect.center(), d * 0.5, P::surface_hi());
    }
    let r = rect.shrink(10.0);
    // 左边一条短竖线 = 侧边栏本身
    painter.line_segment(
        [Pos2::new(r.left() + 0.8, r.top()), Pos2::new(r.left() + 0.8, r.bottom())],
        Stroke::new(1.6, fg),
    );
    // 右侧三条横线 = 列表条目
    for i in 0..3 {
        let y = r.top() + r.height() * (0.16 + 0.34 * i as f32);
        painter.line_segment(
            [Pos2::new(r.left() + 4.6, y), Pos2::new(r.right(), y)],
            Stroke::new(1.6, fg),
        );
    }
    resp.on_hover_text(if on { "隐藏历史记录" } else { "显示历史记录" })
}

/// 「新建对话」按钮：一个「+」。
///
/// 与紧挨着的历史图标同一套画法（36px 热区、单色描边、悬停换色），
/// 两枚并排读作「翻哪段 / 开一段」。任务执行中压暗且不响应点击 ——
/// 那会儿换对话会让回执排到任务后面才生效，点的人只会觉得界面卡住了。
fn plus_icon(ui: &mut Ui, locked: bool) -> egui::Response {
    let d = 36.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(d), Sense::click());
    let hov = pointer_if(ui, &resp) && !locked;
    let fg = if locked {
        P::text_faint()
    } else if hov {
        P::ink()
    } else {
        P::text_dim()
    };

    let painter = ui.painter();
    if hov {
        painter.circle_filled(rect.center(), d * 0.5, P::surface_hi());
    }
    let r = rect.shrink(13.0);
    let c = r.center();
    painter.line_segment(
        [Pos2::new(c.x, r.top()), Pos2::new(c.x, r.bottom())],
        Stroke::new(1.6, fg),
    );
    painter.line_segment(
        [Pos2::new(r.left(), c.y), Pos2::new(r.right(), c.y)],
        Stroke::new(1.6, fg),
    );
    if locked {
        return resp.on_hover_text("正在执行任务，稍后再新建对话");
    }
    resp.on_hover_text("新建对话")
}

/// 单行文本，按**像素**收口：超出 `max_w` 就省略号截断。
///
/// 不能按字数截（`clamp_str`）：34 个汉字 ≈ 476px，比侧边栏还宽一倍，多出来的
/// 部分会一路画到面板外面，被「侧边栏 / 主对话区」那条分界线硬切一刀 ——
/// 看起来就是标题越了界。交给 `LayoutJob` 按实际渲染宽度决定截在哪，
/// 面板拖宽拖窄都会自动跟着变。
fn elide(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat { font_id: font, color, ..Default::default() },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: max_w,
        max_rows: 1,
        // 中文没有词边界，不开这个的话「整段放不下」时会直接不截
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

/// 侧边栏里的一行历史对话：标题 + 「最后活动 · 条数」。
///
/// `current` = 眼下正在聊的这一段（右侧一个实心点），`locked` = 切换请求在飞行中
/// 或任务正在执行，此时不接受点击，整行也压暗一档。
fn history_row(
    ui: &mut Ui,
    s: &aox::history::Summary,
    current: bool,
    locked: bool,
    width: f32,
) -> egui::Response {
    let w = (width - 6.0).max(60.0); // 给滚动条留一点
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 46.0), Sense::click());
    let hov = pointer_if(ui, &resp) && !locked;
    if hov {
        ui.painter().rect_filled(rect, CornerRadius::same(8), P::surface());
    }
    let x = rect.left() + 10.0;
    let title_color = if locked {
        P::text_faint()
    } else if hov || current {
        P::text()
    } else {
        P::text_dim()
    };
    // 文字右边界定在圆点左边再留 10px：右侧那枚「当前」圆点与安全边距都不占标题的
    // 宽度，两行都按同一个宽度收口，标题就再也画不出这一行之外。
    let dot_x = rect.right() - 10.0;
    let max_w = (dot_x - 10.0 - x).max(24.0);
    let title = elide(ui, &s.title, theme::body_font(14.0), title_color, max_w);
    let sub = elide(
        ui,
        &format!("{} · {} 条", s.time, s.messages),
        theme::body_font(12.0),
        P::text_faint(),
        max_w,
    );
    let painter = ui.painter();
    // galley 以左上角定位，这里减掉自身高度的一半换算成原来的「垂直居中」
    painter.galley(Pos2::new(x, rect.top() + 15.0 - title.size().y * 0.5), title, title_color);
    painter.galley(Pos2::new(x, rect.top() + 32.0 - sub.size().y * 0.5), sub, P::text_faint());
    if current {
        // 当前会话用一个实心点标出来，不占用标题宽度
        painter.circle_filled(Pos2::new(dot_x, rect.top() + 15.0), 3.2, P::ink_soft());
    }
    if locked {
        return resp.on_hover_text(format!("{}\n{}", s.title, s.id));
    }
    resp.on_hover_text(format!("{}\n{}\n点击切换到这段对话", s.title, s.id))
}

/// 把读出来的历史消息摊成对话流里的一轮轮记录。
///
/// 与实时渲染的唯一差别是「过程」的攒法：实时是边收事件边攒，
/// 这里是从消息序列里回推 —— assistant 的 tool_calls 开一段过程，
/// 紧随其后的 tool 结果按名字回填到对应那一步上。
fn items_from_history(loaded: &[aox::history::Loaded]) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    // 正在攒的那段过程（出现新的用户消息或最终答复时结算并压入 out）
    let mut pending: Option<Process> = None;

    let flush = |out: &mut Vec<Item>, pending: &mut Option<Process>| {
        if let Some(p) = pending.take() {
            out.push(Item::Process(p));
        }
    };

    for it in loaded {
        let m = &it.msg;
        match m.role.as_str() {
            "user" => {
                flush(&mut out, &mut pending);
                let text = m.content.clone().unwrap_or_default();
                if text.trim_start().starts_with("（宿主提示）") {
                    out.push(Item::Note(NoteKind::Warn, text.trim().to_string()));
                } else {
                    out.push(Item::User(text));
                }
            }
            "assistant" => {
                let calls = m.tool_calls.clone().unwrap_or_default();
                let text = m.content.clone().unwrap_or_default();
                if calls.is_empty() {
                    flush(&mut out, &mut pending);
                    if !text.trim().is_empty() {
                        out.push(Item::Assistant(text));
                    }
                } else {
                    let p = pending.get_or_insert_with(|| Process {
                        steps: Vec::new(),
                        interim: Vec::new(),
                        reasoning: String::new(),
                        // 历史里没有耗时（那是运行期才有的事实），所以不显示
                        elapsed: None,
                        expanded: false,
                        aborted: false,
                    });
                    if let Some(r) = m.reasoning_content.as_deref() {
                        p.reasoning.push_str(r);
                    }
                    if !text.trim().is_empty() {
                        p.interim.push(text);
                    }
                    for c in &calls {
                        p.steps.push(StepView {
                            name: c.function.name.clone(),
                            plugin: None,
                            args: c.function.arguments.trim().to_string(),
                            result: None,
                        });
                    }
                }
            }
            "tool" => {
                let name = m.name.clone().unwrap_or_default();
                let body = m.content.clone().unwrap_or_default();
                if let Some(p) = pending.as_mut() {
                    // 同一步可能有多次同名调用，回填到「最后一个还没有结果的同名步骤」
                    if let Some(st) = p
                        .steps
                        .iter_mut()
                        .rev()
                        .find(|s| s.name == name && s.result.is_none())
                    {
                        st.result = Some((it.is_error == Some(true), body));
                    }
                }
            }
            _ => {}
        }
    }
    flush(&mut out, &mut pending);
    out
}

/// 明暗切换按钮：暗色模式显示太阳（点了变亮），亮色模式显示月亮。
fn theme_toggle(ui: &mut Ui, is_dark: bool) -> egui::Response {
    let d = 36.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(d), Sense::click());
    let hov = pointer_if(ui, &resp);
    // 未悬停时按钮是「透明」的，所以月牙的缺口要用它背后的面板色来盖
    let behind = if hov { P::surface_hi() } else { P::panel() };
    let fg = if hov { P::text() } else { P::text_dim() };

    let painter = ui.painter();
    if hov {
        painter.circle_filled(rect.center(), d * 0.5, behind);
    }
    let c = rect.center();
    if is_dark {
        // 太阳：圆 + 八条光芒
        painter.circle_stroke(c, 5.2, Stroke::new(1.7, fg));
        for i in 0..8 {
            let a = std::f32::consts::TAU * i as f32 / 8.0;
            let (s, co) = a.sin_cos();
            painter.line_segment(
                [
                    Pos2::new(c.x + co * 8.2, c.y + s * 8.2),
                    Pos2::new(c.x + co * 11.2, c.y + s * 11.2),
                ],
                Stroke::new(1.7, fg),
            );
        }
    } else {
        // 月亮：实心圆，再用背景色盖掉右上角形成月牙
        painter.circle_filled(c, 8.6, fg);
        painter.circle_filled(Pos2::new(c.x + 4.8, c.y - 3.8), 7.6, behind);
    }
    resp.on_hover_text(if is_dark { "切换到纸色（亮）" } else { "切换到墨色（暗）" })
}

/// 活动流开关：一条「流程图」式的流线箭头。
///
/// 从右下绕到左上的一条 S 形曲线，起点一个节点圆、终点一个箭头——
/// 就是「信息从这里流到那里」的图形说法。面板开着时是实的墨色，关掉时退成浅灰，
/// 不用看文字也知道现在是什么状态。
fn flow_toggle(ui: &mut Ui, on: bool) -> egui::Response {
    let d = 36.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(d), Sense::click());
    let hov = pointer_if(ui, &resp);
    let fg = if hov {
        P::ink()
    } else if on {
        P::text()
    } else {
        P::text_faint()
    };

    let painter = ui.painter();
    if hov {
        painter.circle_filled(rect.center(), d * 0.5, P::surface_hi());
    }

    let r = rect.shrink(9.5);
    // 三次贝塞尔：控制点一上一下，得到一条平滑的 S 形
    let a = Pos2::new(r.left(), r.bottom());
    let b = Pos2::new(r.left(), r.top() + r.height() * 0.12);
    let c = Pos2::new(r.right(), r.bottom() - r.height() * 0.12);
    let e = Pos2::new(r.right(), r.top());

    let at = |t: f32| {
        let u = 1.0 - t;
        Pos2::new(
            u * u * u * a.x + 3.0 * u * u * t * b.x + 3.0 * u * t * t * c.x + t * t * t * e.x,
            u * u * u * a.y + 3.0 * u * u * t * b.y + 3.0 * u * t * t * c.y + t * t * t * e.y,
        )
    };

    const N: usize = 24;
    let pts: Vec<Pos2> = (0..=N).map(|i| at(i as f32 / N as f32)).collect();
    painter.add(egui::Shape::line(pts.clone(), Stroke::new(1.8, fg)));

    // 起点节点
    painter.circle_filled(a, 2.1, fg);

    // 终点箭头：沿曲线末段切向
    let dir = {
        let v = e - pts[N - 3];
        let l = v.length();
        if l > 0.001 {
            v / l
        } else {
            Vec2::new(0.0, -1.0)
        }
    };
    let n = Vec2::new(-dir.y, dir.x);
    painter.add(egui::Shape::convex_polygon(
        vec![
            e + dir * 5.2,
            e - dir * 2.2 + n * 4.0,
            e - dir * 2.2 - n * 4.0,
        ],
        fg,
        Stroke::NONE,
    ));

    resp.on_hover_text(if on { "隐藏活动流" } else { "显示活动流" })
}

/// 关于：一个 ⓘ 圈。和另外两个图标同一套画法（36px 热区、单色描边）。
fn about_icon(ui: &mut Ui) -> egui::Response {
    let d = 36.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(d), Sense::click());
    let hov = pointer_if(ui, &resp);
    let fg = if hov { P::ink() } else { P::text_dim() };

    let painter = ui.painter();
    if hov {
        painter.circle_filled(rect.center(), d * 0.5, P::surface_hi());
    }
    let c = rect.center();
    painter.circle_stroke(c, 8.4, Stroke::new(1.6, fg));
    painter.circle_filled(Pos2::new(c.x, c.y - 3.7), 1.35, fg);
    painter.line_segment(
        [Pos2::new(c.x, c.y - 0.3), Pos2::new(c.x, c.y + 4.4)],
        Stroke::new(1.8, fg),
    );
    resp.on_hover_text("关于 AOX GUI")
}

/// 悬停在可点区域上时切到「手型」光标，并返回是否悬停。
///
/// egui 不会给自绘组件自动换光标：`allocate_exact_size(.., Sense::click())` 只注册
/// 交互区域，光标仍是默认箭头。配合 `selectable_labels`（默认开）时，按钮里的
/// label 还会把光标改成文本选择的 I 形——于是「按钮上顶着输入光标」。
/// 统一走这个函数，可点的地方就一定是手型。
fn pointer_if(ui: &Ui, resp: &egui::Response) -> bool {
    let hov = resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    hov
}

/// 文字按钮的皮肤：尺寸 + 常态/悬停两套配色。
#[derive(Clone, Copy)]
struct Btn {
    size: f32,
    pad: Vec2,
    radius: u8,
    stroke: Stroke,
    fg: Color32,
    fg_hover: Color32,
    fill: Color32,
    fill_hover: Color32,
}

/// 自绘文字按钮。
///
/// 关键在于**绘制顺序**：先量出文字尺寸 → 按尺寸分配热区 → 铺底色 → 最后画字。
///
/// 之前 `flat_button` / `example_chip` 是反过来的：`ui.label()` 先把字画上去，
/// 悬停时再补一块底色盖住热区——底色落在文字图层之上，鼠标一放上去字就没了。
/// 这里改成先底后字，两套主题、所有状态都天然不会互相遮挡。
fn text_button(ui: &mut Ui, text: &str, b: Btn) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::proportional(b.size),
        Color32::PLACEHOLDER,
    );
    let (rect, resp) = ui.allocate_exact_size(galley.size() + b.pad * 2.0, Sense::click());
    let hov = pointer_if(ui, &resp);

    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(b.radius),
        if hov { b.fill_hover } else { b.fill },
    );
    if b.stroke != Stroke::NONE {
        painter.rect_stroke(
            rect,
            CornerRadius::same(b.radius),
            b.stroke,
            egui::StrokeKind::Inside,
        );
    }
    // 版面用 PLACEHOLDER 排，实际颜色在这里给——于是悬停换色不用重排文字
    painter.galley(rect.min + b.pad, galley, if hov { b.fg_hover } else { b.fg });

    resp
}

/// 透明底的次要按钮（如弹窗里的「关闭」）。
pub(crate) fn flat_button(ui: &mut Ui, text: &str) -> egui::Response {
    text_button(
        ui,
        text,
        Btn {
            size: 13.5,
            pad: Vec2::new(9.0, 5.0),
            radius: 8,
            stroke: Stroke::NONE,
            fg: P::text_dim(),
            fg_hover: P::text(),
            fill: Color32::TRANSPARENT,
            fill_hover: P::surface_hi(),
        },
    )
}

/// 主要动作：反白块（亮色下是墨黑底白字），比描边更有分量。
pub(crate) fn primary_button(ui: &mut Ui, text: &str) -> egui::Response {
    text_button(
        ui,
        text,
        Btn {
            size: 14.5,
            pad: Vec2::new(18.0, 9.0),
            radius: 10,
            stroke: Stroke::NONE,
            fg: P::on_reverse(),
            fg_hover: P::on_reverse(),
            fill: P::reverse_bg(),
            fill_hover: P::ink_soft(),
        },
    )
}

/// 危险动作按钮（目前只有「完全访问」的二次确认用）。
///
/// 用 [`P::danger`] 当底色、文字色仍取 [`P::on_reverse`] —— 这不是巧合：danger 在
/// 亮色下是深砖红（要浅字）、暗色下是浅砖红（要深字），与「反白块」的明暗方向正好
/// 一致，所以不必再为它单独算一次前景色。
pub(crate) fn danger_button(ui: &mut Ui, text: &str) -> egui::Response {
    text_button(
        ui,
        text,
        Btn {
            size: 14.5,
            pad: Vec2::new(18.0, 9.0),
            radius: 10,
            stroke: Stroke::NONE,
            fg: P::on_reverse(),
            fg_hover: P::on_reverse(),
            fill: P::danger(),
            fill_hover: P::danger().gamma_multiply(0.82),
        },
    )
}

/// 空状态里的示例问题胶囊。
fn example_chip(ui: &mut Ui, text: &str) -> egui::Response {
    text_button(
        ui,
        text,
        Btn {
            size: 14.5,
            pad: Vec2::new(18.0, 9.0),
            radius: 22,
            stroke: Stroke::new(1.0, P::border()),
            fg: P::text_dim(),
            fg_hover: P::text(),
            fill: P::surface(),
            fill_hover: P::surface_hi(),
        },
    )
}

fn icon_button(
    ui: &mut Ui,
    diameter: f32,
    base: Color32,
    hover: Color32,
    enabled: bool,
    draw: impl Fn(&egui::Painter, egui::Rect, Color32),
) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(diameter), sense);
    let hov = enabled && pointer_if(ui, &resp);
    let fill = if !enabled {
        P::surface()
    } else if hov {
        hover
    } else {
        base
    };
    let painter = ui.painter();
    if enabled {
        painter.circle_filled(rect.center(), diameter * 0.5, fill);
    } else {
        painter.circle_stroke(rect.center(), diameter * 0.5, Stroke::new(1.0, P::border()));
    }
    // 实心底的按钮里，字形必须用页面底色才看得见（单色方案下 fill 就是「墨」）
    let fg = if !enabled { P::text_faint() } else { P::bg() };
    draw(painter, rect, fg);
    resp
}

fn paint_arrow_up(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let c = rect.center();
    let s = rect.width() * 0.30;
    painter.line_segment(
        [Pos2::new(c.x, c.y - s * 0.9), Pos2::new(c.x, c.y + s * 0.75)],
        Stroke::new(1.6, color),
    );
    painter.line_segment(
        [Pos2::new(c.x - s * 0.62, c.y - s * 0.22), Pos2::new(c.x, c.y - s * 0.9)],
        Stroke::new(1.6, color),
    );
    painter.line_segment(
        [Pos2::new(c.x + s * 0.62, c.y - s * 0.22), Pos2::new(c.x, c.y - s * 0.9)],
        Stroke::new(1.6, color),
    );
}

fn paint_square(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let c = rect.center();
    let s = rect.width() * 0.20;
    painter.rect_filled(
        egui::Rect::from_center_size(c, Vec2::splat(s * 1.5)),
        CornerRadius::same(2),
        color,
    );
}

fn send_button(ui: &mut Ui, enabled: bool) -> egui::Response {
    icon_button(ui, 38.0, P::ink(), P::ink().gamma_multiply(0.75), enabled, paint_arrow_up)
}

fn stop_button(ui: &mut Ui) -> egui::Response {
    icon_button(ui, 38.0, P::ink_soft(), P::ink_soft().gamma_multiply(0.8), true, paint_square)
}

fn paint_tri(painter: &egui::Painter, center: Pos2, size: f32, down: bool, color: Color32) {
    let pts = if down {
        vec![
            Pos2::new(center.x - size, center.y - size * 0.5),
            Pos2::new(center.x + size, center.y - size * 0.5),
            Pos2::new(center.x, center.y + size * 0.6),
        ]
    } else {
        vec![
            Pos2::new(center.x - size * 0.5, center.y - size),
            Pos2::new(center.x - size * 0.5, center.y + size),
            Pos2::new(center.x + size * 0.6, center.y),
        ]
    };
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

fn spinner_dot(ui: &mut Ui, spin: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(16.0, 16.0), Sense::hover());
    let c = rect.center();
    let painter = ui.painter();
    for i in 0..8 {
        let phase = (spin * 7.0) as i32 + i;
        let alpha = if phase % 8 == 0 { 1.0 } else { 0.22 };
        let ang = std::f32::consts::TAU * (i as f32) / 8.0;
        let p = Pos2::new(c.x + ang.cos() * 5.4, c.y + ang.sin() * 5.4);
        painter.circle_filled(p, 1.7, color.gamma_multiply(alpha));
    }
}

/// 逐轮累加出一段对话的合计。
fn sum_usage(log: &[UsageRecord]) -> Usage {
    let mut u = Usage::default();
    for r in log {
        u.accrue(&r.usage);
    }
    u
}

/// 一轮明细在对话流里那一行的文字。
///
/// 「直接摊开所有项」是刻意的：这一行就埋在正文旁边，看它的人多半是为了查账，
/// 让他再点开二级面板反而是折磨。缺的那几项不出现 —— 不拿 0 充数。
fn usage_line(r: &UsageRecord) -> String {
    let mut parts = vec![format!("#{} · {} 次调用", r.round, r.calls)];
    let mut put = |k: &str, v: Option<u64>| {
        if let Some(v) = v {
            parts.push(format!("{k} {}", thousands(v)));
        }
    };
    put("输入", r.usage.prompt);
    put("输出", r.usage.completion);
    put("合计", r.usage.total);
    put("推理", r.usage.reasoning);
    put("缓存命中", r.usage.cached);
    put("缓存未命中", r.usage.cache_miss);
    parts.join(" · ")
}

/// 对话流里那行 token 明细。刻意做得比正文更轻：它是附注，不是内容，
/// 所以连标记点都不给 —— 一行数字自己就说明它是什么了。
fn usage_row(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).color(P::text_faint()).size(12.5));
    ui.add_space(10.0);
}

/// 顶栏那枚「Σ 总 token」点开后的明细：**这段对话的合计**。
///
/// 逐轮的账不放在这儿 —— 那一行行各自贴着它的 AI 答复写在对话流里（见 `usage_line`）。
/// 这里回答的是另一个问题：「这一整段对话一共花了多少、花在哪」。
fn usage_detail(ui: &mut Ui, log: &[UsageRecord]) {
    // 行距压紧。这里是「一列数字」，默认的组块间距会把它撑得像一张松垮的卡片，
    // 反而更难一眼对出哪几行属于同一项。
    ui.spacing_mut().item_spacing.y = 1.0;
    let rows: Vec<&UsageRecord> = log.iter().filter(|r| !r.is_empty()).collect();
    if rows.is_empty() {
        ui.label(
            RichText::new("这段对话还没有用量记录")
                .color(P::text_faint())
                .size(13.0),
        );
        return;
    }
    let mut sum = Usage::default();
    for r in &rows {
        sum.accrue(&r.usage);
    }
    let calls: u32 = rows.iter().map(|r| r.calls).sum();

    /// 一行「标签 …… 数字」。`v` 为 None 时整行跳过。
    ///
    /// 自己分配这一行的矩形，而不是 `horizontal` + `with_layout` —— 后者在
    /// 竖向容器里会把行高撑到内容区的富余高度，一列数字 于是松得像张空卡片。
    /// 数字用等宽字体，右对齐后能上下对齐，看数字比看标签重要。
    fn row(ui: &mut Ui, k: &str, v: Option<u64>) {
        let Some(v) = v else { return };
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 21.0), Sense::hover());
        let p = ui.painter();
        p.text(
            Pos2::new(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            k,
            FontId::proportional(13.0),
            P::text_faint(),
        );
        p.text(
            Pos2::new(rect.right(), rect.center().y),
            Align2::RIGHT_CENTER,
            thousands(v),
            theme::mono_font(13.5),
            P::text(),
        );
    }

    ui.label(
        RichText::new(format!("本次对话 · {} 轮 · {} 次调用", rows.len(), calls))
            .color(P::text())
            .size(13.5),
    );
    ui.add_space(6.0);

    row(ui, "输入", sum.prompt);
    row(ui, "输出", sum.completion);
    row(ui, "合计", sum.total);
    // 细项缩进一格挂在「合计」下面：推理是输出的一部分，缓存是输入的一部分
    row(ui, "　其中推理", sum.reasoning);
    row(ui, "　缓存命中", sum.cached);
    row(ui, "　缓存未命中", sum.cache_miss);

    // 末次输入 ≠ 输入合计：前者是「当前上下文多大」，后者是「这些轮一共读进去多少」。
    // 两个数都摆出来，才不至于把「上下文快满了」误判成「某一轮太贵」。
    if let Some(v) = rows.last().and_then(|r| r.last_prompt) {
        let (rule, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
        ui.painter()
            .hline(rule.x_range(), rule.center().y, Stroke::new(1.0, P::border()));
        ui.add_space(9.0);
        row(ui, "当前上下文", Some(v));
    }
}

fn user_bubble(ui: &mut Ui, text: &str, max_w: f32) {
    // 内边距会额外占宽度，所以要先把正文的可用宽度扣出来，否则气泡会顶出栏宽被裁
    let inner_w = (max_w - 44.0).max(140.0);
    ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
        Frame::NONE
            .fill(P::user_bg())
            .corner_radius(CornerRadius::same(16))
            .stroke(Stroke::new(1.0, P::border()))
            .inner_margin(Margin::symmetric(20, 15))
            .show(ui, |ui| {
                ui.set_max_width(inner_w);
                ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                    let mut job = egui::text::LayoutJob::default();
                    job.wrap.max_width = inner_w;
                    job.append(
                        text,
                        0.0,
                        egui::TextFormat {
                            font_id: theme::body_font(theme::body_size()),
                            color: P::text(),
                            ..Default::default()
                        },
                    );
                    ui.add(Label::new(job).selectable(true).wrap_mode(egui::TextWrapMode::Wrap));
                });
            });
    });
    ui.add_space(18.0);
}

fn assistant_block(ui: &mut Ui, text: &str, width: f32) {
    markdown::render(ui, text, P::text());
    let _ = width;
}

fn note_row(ui: &mut Ui, kind: NoteKind, text: &str) {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(12.0, 20.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.5, kind.color());
        ui.add_space(4.0);
        ui.label(RichText::new(text).color(kind.color()).size(14.0));
    });
    ui.add_space(9.0);
}

fn error_card(ui: &mut Ui, text: &str, width: f32) {
    // 单色方案里，错误用「反白块」而不是红字——对比度最高，也最不依赖色相
    let inner_w = (width - 40.0).max(140.0);
    Frame::NONE
        .fill(P::surface())
        .corner_radius(CornerRadius::same(theme::card_radius()))
        .stroke(Stroke::new(1.0, P::border_strong()))
        .inner_margin(Margin::symmetric(20, 18))
        .show(ui, |ui| {
            ui.set_width(inner_w);
            ui.set_max_width(inner_w);
            Frame::NONE
                .fill(P::reverse_bg())
                .corner_radius(CornerRadius::same(7))
                .inner_margin(Margin::symmetric(11, 4))
                .show(ui, |ui| {
                    ui.label(RichText::new("出错").color(P::on_reverse()).size(14.0));
                });
            ui.add_space(10.0);
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = inner_w;
            job.wrap.break_anywhere = true;
            job.append(
                &clamp_str(text, 2000),
                0.0,
                egui::TextFormat {
                    font_id: theme::body_font(15.0),
                    color: P::text_dim(),
                    ..Default::default()
                },
            );
            ui.add(Label::new(job).selectable(true));
        });
    ui.add_space(18.0);
}

fn process_block(ui: &mut Ui, p: &mut Process, width: f32) {
    let n = p.steps.len();
    // 历史记录里没有耗时信息（那是运行期才有的），所以这一刻只报步数。
    let tail = match p.elapsed {
        Some(s) => format!(" · {s:.1}s"),
        None => String::new(),
    };
    let label = if p.aborted {
        format!("任务已中断 · {n} 步{tail}")
    } else if n == 0 {
        format!("处理过程{tail}")
    } else {
        format!("{n} 步工具调用{tail}")
    };

    if n == 0 && p.interim.is_empty() && p.reasoning.is_empty() {
        return;
    }

    let row_w = width.min(620.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(row_w, 30.0), Sense::click());
    if pointer_if(ui, &resp) {
        ui.painter().rect_filled(rect, CornerRadius::same(9), P::surface());
    }
    paint_tri(
        ui.painter(),
        Pos2::new(rect.left() + 11.0, rect.center().y),
        4.6,
        p.expanded,
        P::text_faint(),
    );
    ui.painter().text(
        Pos2::new(rect.left() + 26.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::body_font(14.0),
        P::text_faint(),
    );
    if resp.clicked() {
        p.expanded = !p.expanded;
    }
    ui.add_space(7.0);

    if p.expanded {
        Frame::NONE
            .fill(P::surface())
            .corner_radius(CornerRadius::same(theme::card_radius()))
            .stroke(Stroke::new(1.0, P::border()))
            .inner_margin(Margin::symmetric(20, 18))
            .show(ui, |ui| {
                // 注意：这里必须用「框架内部」的可用宽度。
                // 沿用外面的 width 会让内容比框架内区还宽，整个卡片被挤出栏宽后裁掉。
                let w = ui.available_width();
                ui.set_width(w);
                ui.set_max_width(w);
                for (i, st) in p.steps.iter().enumerate() {
                    step_row(ui, i + 1, st, w);
                }
                if !p.interim.is_empty() {
                    ui.add_space(8.0);
                    for t in &p.interim {
                        ui.label(
                            RichText::new(clamp_str(t, 400))
                                .color(P::text_faint())
                                .size(14.0),
                        );
                    }
                }
                if !p.reasoning.is_empty() {
                    ui.add_space(12.0);
                    ui.label(RichText::new("思考过程").color(P::text_faint()).size(13.0));
                    ui.add_space(5.0);
                    let mut job = egui::text::LayoutJob::default();
                    job.wrap.max_width = w;
                    job.wrap.break_anywhere = true;
                    job.append(
                        &clamp_str(&p.reasoning, 3000),
                        0.0,
                        egui::TextFormat {
                            font_id: theme::body_font(14.0),
                            color: P::text_faint(),
                            ..Default::default()
                        },
                    );
                    ui.add(Label::new(job).selectable(true).wrap_mode(egui::TextWrapMode::Wrap));
                }
            });
        ui.add_space(16.0);
    }
}

fn step_row(ui: &mut Ui, idx: usize, st: &StepView, width: f32) {
    let w = width.max(140.0);
    ui.horizontal_top(|ui| {
        ui.label(
            RichText::new(format!("{idx:>2}"))
                .color(P::text_faint())
                .size(13.0)
                .monospace(),
        );
        ui.add_space(5.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(12.0, 20.0), Sense::hover());
        // 单色方案下的状态区分：成功=实心墨点；失败=空心圆环（一眼能看出「不一样」）
        let (err, ok) = match &st.result {
            Some((e, _)) => (*e, !*e),
            None => (false, false),
        };
        if err {
            ui.painter()
                .circle_stroke(rect.center(), 4.5, Stroke::new(2.0, P::ink()));
        } else if ok {
            ui.painter().circle_filled(rect.center(), 4.0, P::ink());
        } else {
            ui.painter()
                .circle_stroke(rect.center(), 3.5, Stroke::new(1.5, P::text_faint()));
        }
        ui.add_space(4.0);
        ui.label(RichText::new(&st.name).color(P::text()).size(14.5));
        if let Some(pl) = &st.plugin {
            ui.add_space(2.0);
            Frame::NONE
                .fill(P::surface_hi())
                .corner_radius(CornerRadius::same(20))
                .inner_margin(Margin::symmetric(9, 2))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(if pl == "meta" { "meta" } else { pl.as_str() })
                            .color(P::text_dim())
                            .size(12.0),
                    );
                });
        }
        if !st.args.is_empty() && st.args != "{}" {
            let room = (ui.available_width() - 10.0).max(60.0);
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = room;
            job.wrap.break_anywhere = true;
            job.append(
                &st.args,
                0.0,
                egui::TextFormat {
                    font_id: theme::mono_font(13.0),
                    color: P::text_faint(),
                    ..Default::default()
                },
            );
            ui.add(Label::new(job));
        }
    });
    if let Some((err, preview)) = &st.result {
        ui.horizontal_top(|ui| {
            ui.add_space(38.0);
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = (w - 44.0).max(80.0);
            job.wrap.break_anywhere = true;
            job.append(
                &one_line(preview, 400),
                0.0,
                egui::TextFormat {
                    font_id: theme::body_font(13.5),
                    color: if *err { P::ink() } else { P::text_faint() },
                    ..Default::default()
                },
            );
            ui.add(Label::new(job));
        });
    }
    ui.add_space(5.0);
}

fn one_line(s: &str, max: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    clamp_str(&joined, max)
}
fn clamp_str(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut n = 0usize;
    for ch in s.chars() {
        if n >= max {
            out.push('…');
            break;
        }
        out.push(ch);
        n += 1;
    }
    out
}
fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.trim_end().lines().collect();
    let start = lines.len().saturating_sub(n);
    let tail = lines[start..].join("\n");
    clamp_str(&tail, 900)
}

fn compact_json(v: &serde_json::Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_else(|_| v.to_string());
    clamp_str(&s, 300)
}
