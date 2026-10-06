//! GUI 主界面状态与绘制。
//!
//! 布局（参考主流 AI 编程软件）：
//! ```text
//! ┌───────────────────────────────────────────────────────────┐
//! │ [历史] [+] 状态 · Σ tok              [活动流] [明暗] [关于] │  ← 浮动按钮，无顶栏
//! │ ┌───────────────┐                                         │
//! │ │ 历史记录（滑出）│  对话流：用户气泡 / 助手 Markdown         │
//! │ │  · 这一段      │                ┌──────────────────┐     │
//! │ │  · 那一段      │                │ 悬浮输入框（覆盖）│     │
//! │ └───────────────┘                └──────────────────┘     │
//! └───────────────────────────────────────────────────────────┘
//! ```
//!
//! **没有顶栏**（2026-10-04 用户要求）：原先那条 `TopBottomPanel` 拆掉了，五枚图标变成
//! 浮在对话内容之上的独立按钮，各带一块与旧顶栏同色的底（[`P::panel`]）。位置按下表复原：
//! 左上 20/7、右上同样内缩 20 —— 与旧顶栏里 `Margin::symmetric(20, 0)` + 36px 热区
//! 算出来的落点**逐像素相同**（按钮中心 y = 7 + 18 = 25 = 旧顶栏 50 高的一半）。
//!
//! **左侧历史栏也不再是布局里的一条 `SidePanel`**，改成从左上按钮**向下滑出**的浮层
//! （见 [`App::history_overlay`]）；同时「新建对话」那枚「+」从历史按钮**背后向右滑出**
//! （默认藏起来，见 [`App::toolbar`]）。两者共用同一个进度 `hist_t`，动画时长
//! [`HIST_ANIM_SECS`]。
//!
//! 活动流**不在主窗口里**，点那枚图标会以独立窗口打开（见 `activity.rs`）。
//!
//! 底部没有独立的「输入区」栏位：输入框是一枚浮在对话内容之上的卡片，
//! 滚动时可以一直滑到它下面。

use std::collections::HashMap;
use std::path::PathBuf;
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
use aox::ui::{AgentEvent, AuthDecision, ChoiceReply};
use aox::workplace::Mode;

use crate::activity::{self, ActKind};
use crate::bridge::{
    BootInfo, BootPaths, FromAgent, RuntimeView, SessionFlags, Snapshot, ToAgent, WorkplaceView,
};
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
    /// 调用参数（JSON 串）。**界面上不显示** —— 行上只留工具名，见 `step_row`；
    /// 留字段是为了排错时能一眼对上 `history/*.jsonl` 里那一行。
    #[allow(dead_code)]
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
    /// 用户说的话 + 那条消息带的附件**展示名**。
    ///
    /// 只留名字不留字节：气泡里要画的是一排小胶囊（文件名.后缀），不是图片本身；
    /// 真正的附件内容在 `.jsonl` 里只有一条路径引用。
    User(String, Vec<String>),
    Assistant(String),
    Process(Process),
    Note(NoteKind, String),
    /// 上一条消息里带的**图片**，单独成块显示在它下面。
    ///
    /// 为什么单开一个 Item 而不是给 `User` / `Note` 加个字段：图片有两个来源
    /// （用户拖进来的、工具/插件产出的），它们分别落在 `User` 与 `Note` 上；
    /// 而**画法完全一样**。单开一支之后，渲染只写一遍，也不会因为将来又多一个
    /// 「带图的消息类型」而再改一次那两个变体（改变体要动所有构造点）。
    Images(Vec<PathBuf>),
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
    /// `interim[i]` 说出时**已经跑过多少步调用** —— 用来把实时区切成
    /// 「调用流 → 阶段回复 → 调用流 → …」的交替段（用户 2026-10-05 提的：原来所有调用
    /// 挤在顶上、所有回复挤在中间，看不出哪几步属于哪个阶段）。
    ///
    /// 存「切点」而不是把 `steps` / `interim` 换成 `Vec<Segment>`：这两样在一轮结束时
    /// 要原样交给过程卡片，保持扁平就不用动那条提交路径。
    cuts: Vec<usize>,
    pending_final: Option<String>,
    /// **正在打字的正文**（这一瞬间还没写完的那一句）。
    ///
    /// 与 `pending_final` 的分工：`pending_final` 是「本次响应已经确定的完整正文」，
    /// 到一轮结束才落成一条消息；`streaming` 是让它在落盘之前就**看得见**（用户 2026-10-05
    /// 提的「回复只在全部完成后才瞬间出现、之前对话是黑的」）。
    /// 两者在 `AgentEvent::AssistantText` / `InterimText` 到达时对齐 —— 那时增量已经吐完，
    /// 用权威文本覆盖一遍，避免打字过程中因为丢帧而与真文本差几个字。
    streaming: String,
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

/// 一道等用户作答的选择题（`ask_user`）。问题与选项都是模型给的原文。
///
/// 带 `Clone`：作答那一行要用 `&mut self.choice_text`，而同一帧还要读题目与选项 ——
/// 克隆一份小的（一句话 + 最多 6 个短串）比在借用上做体操划算得多。
#[derive(Clone)]
struct ChoiceCard {
    question: String,
    options: Vec<String>,
}

/// 一张图片当前处于哪个阶段。
///
/// 三个阶段各占一支、而不是用 `Option<TextureHandle>` + 另一个错误表：界面对这三者的
/// 画法本来就不同（图 / 「正在解码…」/ 一句失败原因），合成一个只会让渲染那边
/// 再拆回来。
enum Tex {
    /// 后台线程正在解码
    Loading(std::sync::mpsc::Receiver<Result<aox::image::Bitmap, String>>),
    Ready(egui::TextureHandle),
    Failed(String),
}

/// 设置弹窗左侧那几栏。**声明顺序就是列表里从上到下的顺序**。
///
/// 「额外」放第一个是因为它装的不是设置项（是一个入口），越靠边越不干扰
/// 真正要找设置的人 —— 但用户明确要求它排在最前，那就排最前，不替他改顺序。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettingsTab {
    /// 额外：活动流入口这类「不属于某一项设置」的东西
    Extra,
    /// 外观：明暗 / 跟随系统
    Appearance,
    /// 系统：路径与运行时诊断
    System,
    /// 关于：版本、协议、链接
    About,
}

impl SettingsTab {
    const ALL: [SettingsTab; 4] =
        [SettingsTab::Extra, SettingsTab::Appearance, SettingsTab::System, SettingsTab::About];

    fn label(self) -> &'static str {
        match self {
            SettingsTab::Extra => "额外",
            SettingsTab::Appearance => "外观",
            SettingsTab::System => "系统",
            SettingsTab::About => "关于",
        }
    }
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
    /// 正在等用户作答的一道选择题（`ask_user` 问出来的）。`None` = 没有。
    choice: Option<ChoiceCard>,
    /// 选项槽最后那一行**行内输入框**的内容。
    ///
    /// 与主输入框 `input` 分开是刻意的：两者的归属不同 —— 这份只对当前这道题有效，
    /// 作答完就清空。共用一个会出两种 bug：作答时把用户在别处打的草稿吞掉，
    /// 或者作答完草稿还赖在框里。
    choice_text: String,
    /// 回执通道。**与 `auth_reply` 同一套路**：`Some` 期间 agent 线程正阻塞在
    /// `choose()` 上，所以任何一条收起卡片的路径都必须把它回填掉 ——
    /// 漏了就会「界面看着没事，模型那边永远等下去」。
    choice_reply: Option<oneshot::Sender<ChoiceReply>>,
    /// 已解码 / 正在解码的图片。键是**本机路径**（见 [`Tex`]）。
    tex: HashMap<PathBuf, Tex>,

    input: String,
    /// 待上传栏里的附件（用户拖进来 / `attach_file` 挂上来的）。
    ///
    /// 存的是 [`aox::attach::Attachment`] 而**不是字节**：一个 30 MB 的截图在栏里
    /// 挂上半天，界面状态不该一直背着它 —— 内容只在真正发出去的那一刻读
    /// （`Message::user_with_files` 内部做）。
    pending: Vec<aox::attach::Attachment>,
    /// 指针正把文件拖在窗口上方。只用来把输入卡片描边提亮一档 —— 用户得知道
    /// 「松手会放在哪」，否则拖到窗口右上角松手也生效，反而像是点错了。
    dragging_files: bool,
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
    /// 左上角那枚「历史记录」按钮的开关。浮层的目标状态就是它。
    show_history: bool,
    /// 滑出动画进度：`0` = 完全收起，`1` = 完全展开。
    ///
    /// 由 `App::update` 每帧朝 `show_history` 逼近，`toolbar` / `history_overlay`
    /// 都按它算位移。**不要直接用那个布尔量**：那样只有「在」与「不在」两态，
    /// 中间那两百多毫秒就没有落脚点，滑出会退化成瞬移。
    hist_t: f32,
    /// 历史浮层**上一帧**的实际高度。
    ///
    /// 滑入距离要等于面板自身高度（整块从上方滑进来），而高度由内容（有几条记录）
    /// 决定 —— 只有画完才知道。于是记一帧：本帧拿它算位移，画完再记下新的那个。
    hist_h: f32,
    /// 左上角历史按钮的矩形。只用来回答一个问题：这一下点击算不算「点在浮层外面」。
    ///
    /// 不记的话，点按钮那一帧会同时命中「按钮点击」与「点外部」两条路径 ——
    /// 打开的动作当场被自己关掉，表现为「点了没反应」。
    bar_hist_rect: Option<Rect>,

    /// 演示模式（--demo）：不连 LLM，只展示界面与事件流的排布
    demo: bool,
    /// 对话流是否自动跟随到底部
    follow: bool,
    /// 本条消息强制滚到底（用户刚发指令时）
    force_follow: bool,
    /// 活动流是否自动跟随
    /// 设置弹窗是否展开（顶栏那三枚按钮（活动流 / 明暗 / 关于）已全部收进它）。
    settings_open: bool,
    /// 设置弹窗里正选中哪一栏。
    settings_tab: SettingsTab,
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
            choice: None,
            choice_text: String::new(),
            choice_reply: None,
            tex: HashMap::new(),
            input: String::new(),
            pending: Vec::new(),
            dragging_files: false,
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
            hist_t: 0.0,
            // 面板高度给一个「几条记录的窗口」的初值：第一帧还没画过，没有实测值可用，
            // 而这一帧的滑入位移必须马上有个数。之后每帧都会被真实高度覆盖。
            hist_h: 320.0,
            bar_hist_rect: None,
            // 历史浮层默认收起：主区宽度全部让给对话，需要时再点左上角那枚图标。
            follow: true,
            force_follow: true,
            settings_open: false,
            settings_tab: SettingsTab::Extra,
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
                        cuts: Vec::new(),
                        pending_final: None,
                        streaming: String::new(),
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
                    // 正文增量有两个用途：① 让 token 数字一直涨；② **即时显示** ——
                    // 追加到 `streaming`，界面就能一个字一个字地打出来（用户 2026-10-05
                    // 提的「回复要等全部完成才瞬间出现、在那之前对话是黑的」）。
                    // 活动流的模型段仍由 `Event(AssistantText)` 推进：那里再插一遍会让
                    // 同一段文字在时间轴上出现两次。
                    if let Some(l) = self.live.as_mut() {
                        l.out.add(&d);
                        l.streaming.push_str(&d);
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
                // `ask_user` 问出来的一道选择题。与授权请求同一套路，但**不阻塞输入**
                //（用户要能自己打字作答），所以这里不动 `input` / `can_send` 那套开关，
                // 只把卡片挂上、状态改掉。
                FromAgent::ChoiceRequest { question, options, reply } => {
                    self.status = "等你选择".into();
                    self.choice = Some(ChoiceCard { question: question.clone(), options });
                    // 上一道题残留的字不能带过来：那会造成「我还没打字，框里却有内容」
                    self.choice_text.clear();
                    self.choice_reply = Some(reply);
                    // 选项槽在输入框上方，用户此刻多半正看着对话末尾 —— 拉到最下面
                    // 才看得见它。不拉的话表现是「模型问了一句，然后就没反应了」。
                    self.force_follow = true;
                    let at = self.now();
                    activity::lock(&self.act).note(ActKind::Info, "等你选择", &question, at);
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
        // 选项槽跟着一起收。**回执通道丢掉就够了**：对端 `rx.await` 会立刻报错，
        // 而 `GuiUi::choose` 把「通道断了」映射成 `Cancelled`（见 bridge 里那条注释）——
        // 于是 agent 线程不会卡在一个永远等不到答案的 `choose()` 上。
        self.choice = None;
        self.choice_reply = None;
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

    /// 从左上按钮**向下滑出**的历史浮层（会话切换器）。
    ///
    /// 它顶替了原先那条 `SidePanel::left`：内容照搬侧边栏（标题 / 刷新 / 分隔线 /
    /// 列表 / 空状态），外壳从「布局里的一条栏」换成「浮在对话之上的一层」。
    ///
    /// # 滑出是怎么做出来的
    ///
    /// 两块拼起来 —— **位置**（整块内容从「目标位置 − 自身高度」滑到目标位置）
    /// 加上**裁剪**（可视区钉在目标位置上沿）。只做位移的话，面板会先在按钮
    /// **脸上**冒出来再往下走；有了裁剪，它就是从按钮下沿一点点长出来的。
    ///
    /// `Ui::set_clip_rect` 顺带把交互也管住了：egui 给控件的 `interact_rect` 正是
    /// `clip_rect ∩ rect`，被裁掉的那半截点不到。否则滑出途中鼠标扫过「还没长出来」
    /// 的区域也会命中一行，凭空切走一段对话。
    fn history_overlay(&mut self, ctx: &egui::Context) {
        if self.hist_t <= 0.0 {
            return;
        }
        let ease = ease_out_cubic(self.hist_t);
        // 切换请求在飞行中、或正在跑任务时不接受点击：这两段时间里发起切换，
        // 结果会排在后面才生效，点的人只会看到界面「迟疑一下才跳」。
        let locked = self.busy || self.switching;

        // 与左上那枚按钮同一条左边线；顶边 = 按钮下沿 + 8。
        let x = BAR_LEFT;
        let top = BAR_TOP + BAR_BTN + 8.0;
        let w = HISTORY_W;
        // 面板高度由内容（有几条记录）定，这里只封顶：超过窗口下沿就会被窗口边界
        // 硬切一刀。留 22px 给底部的圆角与阴影。
        let max_h = (ctx.content_rect().bottom() - top - 22.0).clamp(150.0, 620.0);
        let slide = -(1.0 - ease) * self.hist_h;

        let mut want_refresh = false;
        let mut open: Option<String> = None;

        let area = egui::Area::new(egui::Id::new("aox-history"))
            .order(egui::Order::Foreground)
            .fixed_pos(Pos2::new(x, top + slide))
            // 滑出途中位置在目标位置**上方**（`top + slide` 会跑到按钮上面去），
            // 而 Area 默认会把越界的位置拉回屏幕内 —— 位移当场被抹平、动画失效。
            // 这里放开约束：位置说了算。
            .constrain_to(loose_rect())
            .show(ctx, |ui| {
                // 可视区比面板四周各松一圈，免得把圆角描边和投影切出硬边。
                ui.set_clip_rect(Rect::from_min_max(
                    Pos2::new(x - 26.0, top - 2.0),
                    Pos2::new(x + w + 26.0, top + max_h + 30.0),
                ));
                Frame::NONE
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(12))
                    .stroke(Stroke::new(1.0, P::border()))
                    .shadow(egui::Shadow {
                        offset: [0, 8],
                        blur: 22,
                        spread: 0,
                        color: P::shadow(),
                    })
                    .inner_margin(Margin::symmetric(14, 12))
                    .show(ui, |ui| {
                        ui.set_width(w - 28.0);
                        let pw = ui.available_width();

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
                                RichText::new(
                                    "还没有历史记录。\n发出第一条指令后，这段对话会自动保存到这里。",
                                )
                                .color(P::text_faint())
                                .size(13.0),
                            );
                            ui.add_space(10.0);
                            // 目录路径可能很长，用 truncate 让它自己在面板宽度内收口，
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

                        // 高度**贴合内容**（`auto_shrink` 的第二个分量），只封一个上限：
                        // 记录少的时候面板就矮，最后一条下面不会拖着一片空。
                        //
                        // ⚠️ 别改成「按条数算高度」（`rows × 行高`）。上一版就是那么写的，
                        // 但那个乘积只是**行本身**的高度，滚动区还会另算自己的边距 ——
                        // 估出来的数对不上面板实际的长相，一条记录的面板底下会多出
                        // 二十几个逻辑像素的空白（截图比对才看出来）。
                        ScrollArea::vertical()
                            .max_height((max_h - HIST_HEAD).max(60.0))
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                for s in &self.history {
                                    let current =
                                        Some(s.id.as_str()) == self.current_history.as_deref();
                                    if history_row(ui, s, current, locked, pw).clicked() && !locked {
                                        open = Some(s.id.clone());
                                    }
                                }
                            });
                    });
            });
        let panel = area.response.rect;
        // 记下这一帧的实际高度 —— 下一帧的滑入距离就是它（见字段 `hist_h`）。
        // 这里**不**按 `hist_t` 判断：位置偏移不影响 `rect` 的高度，随时记都是准的。
        self.hist_h = panel.height().max(96.0);

        // 点面板外面、或按 Esc → 收起。按钮自己那一下不算「外面」（见 `bar_hist_rect`）。
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let clicked_outside = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer.interact_pos().is_some_and(|p| {
                    !panel.contains(p) && !self.bar_hist_rect.is_some_and(|r| r.contains(p))
                })
        });
        if esc || clicked_outside {
            self.show_history = false;
        }

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
                        if let Some(prev) = l.pending_final.replace(t.clone()) {
                            l.interim.push(prev);
                        }
                        // 增量已经吐完 ⇒ 拿权威文本对齐一次：打字过程中万一丢帧，
                        // 屏幕上的字会与真文本差几个字，最后一刻必须抹平。
                        l.streaming = t;
                    }
                    None => self.items.push(Item::Assistant(t)),
                }
            }
            AgentEvent::InterimText(t) => {
                if let Some(l) = self.live.as_mut() {
                    let at = self.started.elapsed().as_secs_f32();
                    activity::lock(&self.act).model_begin(at);
                    // 阶段说明到此定稿：收进 `interim`（它会留在实时区，并在结束后进过程卡片），
                    // `streaming` 清空 —— 下一段正文从新的一行重新开始打。
                    l.streaming.clear();
                    l.interim.push(t);
                    // 记下「这句话之前跑了几步」：实时区据此把它前面那段调用单独画成一张卡。
                    l.cuts.push(l.steps.len());
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
            AgentEvent::Images(paths) => {
                // 工具产出的图片：**当场画出来**。
                //
                // 在此之前这套流程有个说不通的结局：模型说「图出来了，直接看上面那张」，
                // 而人这边什么都没有 —— 图进的是**发给模型的那条请求**，界面只把随行的
                // 那句话画成了一个小圆点。于是模型看得见、人看不见，而两种前端里
                // 只有图形界面有这个问题。
                self.items.push(Item::Images(paths));
                self.force_follow = true;
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
        // 一轮结束时理论上不会有挂着的选项槽（模型已经往下走了）。真有就是界面自己
        // 没清干净 —— 那也一起丢掉：此刻对端已经不在等（那一轮的 `choose()` 早返回了），
        // `tx.send` 会静默失败。留着它反而危险：下一次 `ChoiceRequest` 之前若有人
        // 调 `answer_choice`，那一下会打到一个已经没人听的通道上，而界面看起来像
        // 「我点了、但它没反应」。
        self.choice = None;
        self.choice_reply = None;
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

    /// 收下这一帧拖进窗口的文件。
    ///
    /// **整个窗口任意位置都接**（用户 2026-10-05 明确要求）—— 所以从 `ctx` 上取，
    /// 而不是挂在某个控件上：挂控件就只有那一小块热区生效，其余地方拖进来毫无反应。
    ///
    /// egui 每帧把「这一帧落下的文件」报一次（`dropped_files`），读完即走，
    /// 不会重复计数；`hovered_files` 用来把输入卡片描边提亮一档。
    fn take_dropped(&mut self, ctx: &egui::Context) {
        let (dropped, hovering) = ctx.input(|i| (i.raw.dropped_files.clone(), !i.raw.hovered_files.is_empty()));
        self.dragging_files = hovering;
        if dropped.is_empty() {
            return;
        }
        for f in dropped {
            // 没有本机路径的（某些平台拖的是内存里的数据）只能放弃 —— 附件是按路径读的
            let Some(path) = f.path else {
                self.items.push(Item::Note(
                    NoteKind::Warn,
                    format!("拖入的「{}」没有本机路径，无法作为附件。", f.name),
                ));
                continue;
            };
            match aox::attach::Attachment::probe(&path) {
                // 同一个文件拖两次 = 一个附件（不要出现两枚一模一样的胶囊）
                Ok(a) if self.pending.iter().any(|p| p.path == a.path) => {}
                Ok(a) => self.pending.push(a),
                // 拦下来的理由照实说（目录 / 读不到 / 图片超过 32 MiB），别只说「失败」
                Err(e) => self.items.push(Item::Note(NoteKind::Warn, e)),
            }
        }
    }

    /// 待上传栏 —— 输入卡片**上方**那一排小胶囊，每个附件一枚。
    ///
    /// 名字过长**中间省略**（`很长的名字…截图.png`）：后缀是这条信息里最有用的一位
    /// （一眼分得出是图还是文档），所以砍中间、留住尾巴；路径一律不显示 ——
    /// 一个 `E:\…\…\…` 的前缀会把胶囊撑爆，而用户自己清楚它是从哪拖来的。
    /// 输入框上方那排可点选项（模型用 `ask_user` 问出来的）。
    ///
    /// 与「待上传栏」同一处、同一套语言（小圆角块 + 细描边），因为它们回答的是同一类
    /// 问题：「用户现在要给我什么」。区别只在于一个给的是文件，一个给的是决定。
    ///
    /// **竖排**：A / B / C … 一行一个，最后一个永远是「自己输入」。
    /// 第一版是横排，实机一测就露馅：选项一多，最后一个被挤成 28px 宽的竖条
    ///（文字一个字一行），而且**点上去没反应** —— 命中区跟着被压缩的内容变了形，
    /// 画出来的那块与能点的那块对不上（用户 2026-10-06 两个问题一起报的）。
    /// 竖排还顺带解决了「一眼扫完」：选项本来就是读一个选一个，横排反而要左右找。
    fn choice_bar(&mut self, ui: &mut Ui) {
        // 克隆一份而不是借着 `self.choice` 画：最后那一行要用 `&mut self.choice_text`，
        // 与「读题目和选项」是同一个 self 的两处借用 —— 结构体很小，克隆最省事。
        let Some(card) = self.choice.clone() else { return };
        let mut picked: Option<usize> = None;
        let mut typed: Option<String> = None;

        ui.add_space(6.0);
        Frame::NONE
            .fill(P::panel())
            .corner_radius(CornerRadius::same(14))
            .stroke(Stroke::new(1.0, P::border()))
            .inner_margin(Margin::symmetric(16, 12))
            .show(ui, |ui| {
                let w = ui.available_width();
                ui.set_width(w);
                // 问题原文照搬：改写了就不再是模型问的那句，而用户要回答的正是它
                ui.label(RichText::new(&card.question).color(P::text()).size(14.5));
                ui.add_space(9.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    for (i, o) in card.options.iter().enumerate() {
                        if ask_row(ui, w, i, o) {
                            picked = Some(i);
                        }
                    }
                    // 最后一行是**输入框本身**（见 `ask_input_row`）：作答的地方就在
                    // 题目旁边，眼睛不用在上下两个框之间来回跑。
                    typed = ask_input_row(ui, w, &mut self.choice_text);
                });
            });
        ui.add_space(6.0);

        if let Some(i) = picked {
            self.answer_choice(ChoiceReply::Picked(i));
        } else if let Some(t) = typed {
            self.choice_text.clear();
            self.answer_choice(ChoiceReply::Text(t));
        }
    }

    fn attach_bar(&mut self, ui: &mut Ui) {
        if self.pending.is_empty() {
            return;
        }
        let mut remove: Option<usize> = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(7.0, 7.0);
            for (i, a) in self.pending.iter().enumerate() {
                if attach_chip(ui, &a.name) {
                    remove = Some(i);
                }
            }
        });

        // 挂的是图片、而当前模型多半不支持视觉时，给一句**能忽略**的提示。
        //
        // 刻意不拦：用户要的是「照发，由端点报错、模型转述」（见 `attach` 模块文档）。
        // 这句话只是把「等一会儿会莫名其妙收到一个 400」提前成「现在就能看出来」。
        if self.pending.iter().any(|a| a.kind.is_some()) && !self.model.to_ascii_lowercase().contains("flash") {
            ui.add_space(5.0);
            ui.label(
                RichText::new(format!(
                    "当前模型 {}：DeepSeek 侧只有 deepseek-flash 支持图片输入（照发也行，端点会报错）。",
                    self.model
                ))
                .color(P::warn())
                .size(12.5),
            );
        }
        // 删要等循环走完：`iter()` 还借着 `self.pending`
        if let Some(i) = remove {
            self.pending.remove(i);
        }
        ui.add_space(8.0);
    }

    fn send(&mut self) {
        if self.demo {
            return;
        }
        let text = self.input.trim().to_string();

        // ── 有选项槽挂着：这一句是对那道题的**作答**，不是新一轮任务 ──
        //
        // 不说清这一点会很难查：用户打了字按发送，而 agent 线程此刻正阻塞在
        // `choose()` 上 —— 那条 `Input` 只会排在通道里，表现就是「我打字了它不理我，
        // 选项槽还挂在那儿」。作答之后输入框清空、选项槽收起，那一轮接着往下走。
        if self.choice.is_some() {
            if !text.is_empty() {
                self.input.clear();
                self.answer_choice(ChoiceReply::Text(text));
            }
            return;
        }

        // 只有附件、没有文字也算一条消息 —— 拖一张图进来直接回车是最常见的用法。
        if (text.is_empty() && self.pending.is_empty()) || self.busy || self.waiting_auth {
            return;
        }
        if self.boot.as_ref().is_some_and(|b| !b.provider_ready) {
            return;
        }
        let files: Vec<PathBuf> = self.pending.iter().map(|a| a.path.clone()).collect();
        let names: Vec<String> = self.pending.iter().map(|a| a.name.clone()).collect();
        // 时间轴上只记「发了几个附件」：那是给人扫一眼的流水账，不是文件清单
        //（附件的真实记录在对话文件里，见 `attach::to_content` 的路径清单）。
        let label = if names.is_empty() {
            text.clone()
        } else if text.is_empty() {
            format!("（{} 个附件）", names.len())
        } else {
            format!("{text}（{} 个附件）", names.len())
        };
        self.items.push(Item::User(text.clone(), names));
        // 自己拖进来的图**当场**显示，不等宿主那条回执绕一圈回来。
        let shown: Vec<PathBuf> = files.iter().filter(|p| looks_like_image(p)).cloned().collect();
        if !shown.is_empty() {
            self.items.push(Item::Images(shown));
        }
        self.input.clear();
        self.pending.clear();
        self.force_follow = true;
        // 开一轮：时间轴上最上面那条「输入」从这一刻起
        activity::lock(&self.act).round_begin(&label, self.now());
        let _ = self.tx.send(ToAgent::Input(text, files));
    }

    fn stop(&mut self) {
        if self.busy {
            self.flags.request_abort();
            self.status = "正在中断…".into();
        }
    }

    /// 把用户的回答交回 agent 线程，并收起选项槽。
    ///
    /// 这是**唯一**一处把 `ChoiceReply` 送出去的地方：三条作答路径（点选项、
    /// 打字发送、以后若加的快捷键）都走它，才不会出现「某条路上忘了回执，
    /// 于是模型永远等下去」。
    fn answer_choice(&mut self, reply: ChoiceReply) {
        if let Some(tx) = self.choice_reply.take() {
            let _ = tx.send(reply);
        }
        self.choice = None;
        self.choice_text.clear();
        // 刚作答完，那一轮还在跑（模型要接着做）—— 状态回到「思考中」，
        // 而不是留在「等你选择」上让人以为还在等他。
        self.status = "思考中".into();
    }

    /// 图片解码的收尾与排队：每帧一次。
    ///
    /// 排在这里而不是「创建 Item 的那一刻」有两个原因：
    /// ① Item 有两个来源（历史回放与实时事件），而 `items_from_history` 是个不用
    ///    `self` 的自由函数，两边各排一次就是把同一件事写两遍；
    /// ② 扫一遍是**幂等**的 —— 已经有条目的路径不会重复排队，于是不需要「在哪一步
    ///    之后必须记得调一次」这种约定。
    fn poll_images(&mut self, ctx: &egui::Context) {
        // ① 收：解码完的挂成纹理（`try_recv` 不会阻塞界面）
        let done: Vec<(PathBuf, Result<aox::image::Bitmap, String>)> = self
            .tex
            .iter_mut()
            .filter_map(|(p, t)| match t {
                Tex::Loading(rx) => rx.try_recv().ok().map(|r| (p.clone(), r)),
                _ => None,
            })
            .collect();
        for (p, r) in done {
            let entry = match r {
                Ok(bm) => Tex::Ready(ctx.load_texture(
                    // 纹理名只用文件名：它只是调试时的标识，而整条路径又长又含反斜杠
                    format!("aox-img:{}", p.file_name().unwrap_or_default().to_string_lossy()),
                    egui::ColorImage::from_rgba_unmultiplied([bm.width, bm.height], &bm.rgba),
                    egui::TextureOptions::LINEAR,
                )),
                // 失败也缓存：否则每帧都会为同一张坏图重开一次解码线程
                Err(e) => Tex::Failed(e),
            };
            self.tex.insert(p, entry);
        }

        // ② 排：还没排过的进队列
        let mut want: Vec<PathBuf> = Vec::new();
        for it in &self.items {
            if let Item::Images(paths) = it {
                for p in paths {
                    // 去重（同一张图可能在一条消息里被列两次），且只挑「像图片」的
                    if looks_like_image(p) && !self.tex.contains_key(p) && !want.contains(p) {
                        want.push(p.clone());
                    }
                }
            }
        }
        // 一帧最多起 4 条解码线程：一屏二十张图不该一次起二十个线程、更不该同时
        // 摊开二十份解码后的像素（那可能上 GB）。剩下的下一帧接着排 —— 这不影响
        // 观感，因为用户本来就一次只看一两张。
        let queued = want.len();
        for p in want.into_iter().take(4) {
            let (tx, rx) = std::sync::mpsc::channel();
            let path = p.clone();
            // 解码是同步的、且可能几十毫秒（GDI+ 见 `aox::image`），绝不能占着界面线程
            std::thread::spawn(move || {
                let _ = tx.send(aox::image::decode(&path));
            });
            self.tex.insert(p, Tex::Loading(rx));
        }

        // ③ 还有在解 / 还没排完的，就要求下一帧 —— 否则窗口安静下来之后，
        //    那些图会永远停在「正在解码…」上（egui 只在有输入时才重绘）。
        if queued > 0 || self.tex.values().any(|t| matches!(t, Tex::Loading(_))) {
            ctx.request_repaint();
        }
    }

    // ─────────────────────────── 绘制 ───────────────────────────

    /// 浮在对话之上的一排按钮 —— 原来的顶栏（2026-10-04 拆掉了面板本身）。
    ///
    /// 位置逐像素照搬旧顶栏算出来的落点：左上 `(20, 7)`、右上同样内缩 20，
    /// 配 36px 热区 ⇒ 按钮中心 y = 25 = 旧顶栏 50px 高的中线。
    ///
    /// 每枚按钮自带一块底色（[`P::panel`]，就是旧顶栏的填充色），所以它们浮在
    /// 对话文字上也不会糊成一片。悬停时底色升一档（`surface_hi`），
    /// 与列表行、菜单行的悬停语言一致。
    ///
    /// 两处与旧顶栏不同的：
    /// - 「新建对话」与「设置」默认**都不显示**，随历史浮层一起从历史按钮背后向右滑出；
    /// - 状态与 token 从「顶栏左侧的一块空位」变成一枚跟着按钮走的胶囊
    ///   （顶栏没了，它们总得有个落脚处，否则任务进度与 token 账本就无处可看）。
    ///
    /// **右上那三枚（活动流 / 明暗 / 关于）已全部删掉**（用户 2026-10-05 要求），
    /// 各自搬进了设置弹窗：活动流 → 「额外」栏，明暗 → 「外观」栏，关于 → 「关于」栏。
    fn toolbar(&mut self, ctx: &egui::Context) {
        let mut want_new = false;
        let mut want_settings = false;
        let mut toggle_hist = false;
        let ease = ease_out_cubic(self.hist_t);
        let locked = self.busy || self.switching;

        // ── 滑出组：「+」在里、「设置」在外 ──
        //
        // 每个按钮各自一个 `Area`：滑出裁剪要按各自的右边界切，共用一个 Area 做不到。
        // `x_end - slide` 是「从历史按钮背后钻出来」的位移；**裁剪左边界固定在自己的
        // 终点位**，于是位移没走完时露出来的部分不足一整个按钮，读起来就是「还在后面」。
        if self.hist_t > 0.0 {
            let slide = (BAR_BTN + 10.0) * (1.0 - ease);
            for (id, slot, icon) in [
                ("aox-bar-new", 0.0_f32, SlideIcon::Plus),
                ("aox-bar-settings", 1.0_f32, SlideIcon::Gear),
            ] {
                // slot 0 = 紧挨历史按钮；slot 1 = 再往右一格。38 = 36 + 2 的间隙。
                let x_end = BAR_LEFT + BAR_BTN + 2.0 + slot * (BAR_BTN + 2.0);
                egui::Area::new(egui::Id::new(id))
                    .order(egui::Order::Foreground)
                    .fixed_pos(Pos2::new(x_end - slide, BAR_TOP))
                    // 滑出途中位置会压到左边的按钮上，别让 Area 把它拉回屏幕内。
                    .constrain_to(loose_rect())
                    .show(ctx, |ui| {
                        ui.set_clip_rect(Rect::from_min_max(
                            Pos2::new(x_end, BAR_TOP - 6.0),
                            Pos2::new(x_end + BAR_BTN + 6.0, BAR_TOP + BAR_BTN + 6.0),
                        ));
                        match icon {
                            SlideIcon::Plus => {
                                if plus_icon(ui, locked).clicked() && !locked {
                                    want_new = true;
                                }
                            }
                            SlideIcon::Gear => {
                                if gear_icon(ui).clicked() {
                                    want_settings = true;
                                }
                            }
                        }
                    });
            }
        }

        // ── 左上：历史按钮（位置不变）＋ 状态 / token 胶囊 ──
        egui::Area::new(egui::Id::new("aox-bar-left"))
            .order(egui::Order::Foreground)
            .fixed_pos(Pos2::new(BAR_LEFT, BAR_TOP))
            .constrain_to(loose_rect())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if history_icon(ui, self.show_history).clicked() {
                        toggle_hist = true;
                    }
                    // 「+」与「设置」住在另外两个 `Area` 里（各要单独做滑出裁剪），这里替
                    // 它们把位置让出来：展开时各撑开 38px，收起时不占位。于是那枚 token
                    // 胶囊会被「挤」到右边去 —— 正好是「按钮从左边钻出来」该有的连带效果。
                    //
                    // 38 = 按钮的 36 + 与它之间留的 2。少了这段，胶囊会直接压在按钮身上：
                    // 两者底色同色，实机上看起来就是「按钮和胶囊共用一块底」，于是那枚
                    // 按钮像是没长出来（截图比对才发现，肉眼扫一眼容易放过）。
                    ui.add_space(8.0 + 38.0 * 2.0 * ease);
                    // 一个 token 都没有、又没在跑任务时，这枚胶囊连壳都不该出现
                    //（`token_chip` 此时什么都不画，硬套一层 Frame 会剩个空框）。
                    if self.busy || self.usage_sum().total.is_some() {
                        Frame::NONE
                            .fill(P::panel())
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(12, 6))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    if self.busy {
                                        ui.label(
                                            RichText::new(&self.status).color(P::text()).size(14.0),
                                        );
                                        ui.add_space(7.0);
                                        let secs = self
                                            .live
                                            .as_ref()
                                            .map_or(0.0, |l| l.started.elapsed().as_secs_f32());
                                        ui.label(
                                            RichText::new(format!("{secs:.1}s"))
                                                .color(P::text_faint())
                                                .size(13.5),
                                        );
                                        // 累计 token 实时滚动 = 已完成各轮（精确）＋ 本轮估算。
                                        // 所以整个对话的账在任务执行期间也一路上涨，而不是每轮归零。
                                        // 带 `~` 是因为后半截还在估；一轮结束后由那枚精确的总数接手。
                                        let base = self.usage_sum().total.unwrap_or(0);
                                        let est =
                                            self.live.as_ref().map_or(0, |l| l.out.tokens());
                                        let shown = base + est;
                                        if shown > 0 {
                                            ui.add_space(7.0);
                                            ui.label(
                                                RichText::new(format!(
                                                    "Σ ~{} tok",
                                                    thousands(shown)
                                                ))
                                                .color(P::text_faint())
                                                .size(13.5),
                                            );
                                        }
                                    } else {
                                        // 空闲时只剩一个总 token，鼠标移上去就有明细
                                        self.token_chip(ui, ctx);
                                    }
                                });
                            });
                    }
                });
            });
        // 这一栏按钮的矩形记一笔：历史浮层要靠它排除「点在按钮上」那一下
        //（不然按钮的开关动作会被浮层自己的「点外面就收起」当场抵消）。
        self.bar_hist_rect =
            Some(Rect::from_min_size(Pos2::new(BAR_LEFT, BAR_TOP), Vec2::splat(BAR_BTN)));

        if toggle_hist {
            self.show_history = !self.show_history;
            if self.show_history {
                self.refresh_history();
            }
        }
        if want_new {
            self.new_conversation();
        }
        if want_settings {
            self.settings_open = true;
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
            if matches!(it, Item::User(..)) {
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
                // 顶部留白 = 旧顶栏那条（50）＋ 原来的 26。顶栏拆掉之后按钮改成浮在
                // 内容之上，不留白的话第一行字直接顶到按钮底下。
                ui.add_space(76.0);
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
                            .any(|i| matches!(i, Item::User(..) | Item::Assistant(_)));
                        if !started && self.live.is_none() {
                            self.empty_state(ui);
                        }
                        // 图片渲染要查纹理缓存。`self.items` 与 `self.tex` 是**两个不相干
                        // 的字段**，所以先取一个只借用 `tex` 的引用，循环里借用
                        // `items` 就不冲突了（整块 `&mut self` 是不行的）。
                        let tex = &self.tex;
                        for item in &mut self.items {
                            match item {
                                Item::User(t, files) => user_bubble(ui, t, files, inner * 0.80),
                                Item::Assistant(t) => assistant_block(ui, t, inner),
                                Item::Process(p) => process_block(ui, p, inner),
                                Item::Note(k, t) => note_row(ui, *k, t),
                                Item::Images(paths) => images_block(ui, paths, inner, tex),
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
        // 实时区按**阶段**切开：一段调用流 → 一句阶段回复 → 再一段调用流 → …
        // （用户 2026-10-05 提的：原来所有调用挤在顶上、所有回复挤在中间，
        // 看不出哪几步属于哪个阶段。）
        //
        // `cuts[i]` = 第 i 句阶段回复说出时已经跑过的步数 ⇒ 第 i 段调用就是
        // `steps[cuts[i-1] .. cuts[i]]`，最后一段一直画到 `steps.len()`。
        for seg in 0..=l.interim.len() {
            let from = if seg == 0 { 0 } else { l.cuts.get(seg - 1).copied().unwrap_or(0) };
            let to = l.cuts.get(seg).copied().unwrap_or(l.steps.len()).min(l.steps.len());
            if from < to {
                calls_card(ui, &l.steps[from..to]);
                ui.add_space(14.0);
            }
            if let Some(say) = l.interim.get(seg) {
                live_plain_text(ui, say, P::text_dim());
                ui.add_space(14.0);
            }
        }
        // 正在打的那一句接在最后：它还没归属到任何阶段（说完才知道后面还有没有调用）。
        if !l.streaming.trim().is_empty() {
            live_plain_text(ui, &l.streaming, P::text());
            ui.add_space(14.0);
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

        // 思考框：**固定高度、且常驻**（用户 2026-10-05：「底下的思考框会时不时上下伸缩，
        // 导致屏幕上下震动」）。伸缩有两个来源，这里一次治掉：
        //   ① 流式过程中它从一行涨到四行；
        //   ② 每次响应结束 `reasoning_live` 变 false、文本被清空，整块忽隐忽现。
        // 所以框只画一次、高度写死，文字画在框内并按框裁掉。代价是非思考模型也会看到
        // 这个空框 —— 用「一块稳定的空白」换「整块上下跳」，稳定更值。
        const THINK_LINES: usize = 4;
        const THINK_LINE_H: f32 = 20.0;
        ui.add_space(12.0);
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(w, THINK_LINE_H * THINK_LINES as f32 + 26.0),
            Sense::hover(),
        );
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(12), P::code_bg());
        painter.rect_stroke(
            rect,
            CornerRadius::same(12),
            Stroke::new(1.0, P::border()),
            egui::StrokeKind::Inside,
        );
        if !l.reasoning.is_empty() {
            let inner = rect.shrink2(Vec2::new(16.0, 13.0));
            let tail = tail_lines(&l.reasoning, THINK_LINES);
            // 先裁再画：文字超过四行时被切掉，而不是把框顶高。
            let p = painter.with_clip_rect(inner);
            let g = p.layout(tail, theme::body_font(14.5), P::text_faint(), inner.width());
            p.galley(inner.left_top(), g, P::text_faint());
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
        // 有选项槽挂着时，**发送的含义变了**：它是「这就是我的回答」，而不是开新一轮
        //（那一轮正在跑，`busy` 为真）。所以这里要放行 —— 否则用户看着一排选项、
        // 想自己打字说清楚，而发送键是灰的，只能去点某个都不太对的选项。
        let choosing = self.choice.is_some();
        // 只有附件、没有文字也允许发送（拖一张图进来按发送是最常见的用法）。
        // 作答时不吃这一条：那时必须真的打字，光挂个附件不算回答。
        let can_send = !blocked
            && !self.waiting_auth
            && (!self.busy || choosing)
            && (!self.input.trim().is_empty()
                || (!choosing && !self.pending.is_empty()));

        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.vertical(|ui| {
                ui.set_width(inner);
                ui.set_max_width(inner);

                // 选项槽：模型用 `ask_user` 问出来的选择题。放在待上传栏**之上** ——
                // 它比「我这次要发什么」更急（用户不答，这一轮就不动）。
                self.choice_bar(ui);

                // 待上传栏：输入卡片**上方**那一排小胶囊（用户 2026-10-05 明确要求的位置）。
                self.attach_bar(ui);

                // 悬浮输入框：没有分隔线、四周也不铺底色，靠大圆角 + 一圈细描边 + 一层淡投影
                // 浮在对话区之上（这就是「背景掏空」的观感来源）。
                Frame::NONE
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(20))
                    .stroke(Stroke::new(
                        if self.dragging_files { 1.5 } else { 1.0 },
                        // 文件正拖在窗口上方时描边提亮一档：松手会落在哪，得看得出来
                        if self.dragging_files {
                            P::ink()
                        } else if self.busy || self.waiting_auth {
                            P::border_strong()
                        } else {
                            P::border()
                        },
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
                            } else if choosing {
                                // 指向**上面那一行**行内输入框：作答的地方在题目旁边，
                                // 不要把人往下面这个框引（那是用户 2026-10-06 明确要的）
                                "在上面那排选项里点一个，或直接在那一行输入…"
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

    /// 设置：一枚居中的大卡片，左栏选、右栏看。
    ///
    /// 取代了原来右上那三枚按钮（活动流 / 明暗 / 关于）与那枚 380px 的「关于」小卡片
    /// ——「把三个按钮全删掉，收进一个设置弹窗」是用户 2026-10-05 的要求。
    /// 所以四个栏目里三个都是**搬家**：「额外」收活动流入口、「外观」收明暗切换、
    /// 「关于」收版本与协议；只有「系统」是新写的（那些路径以前半藏在关于卡片里，
    /// 现在成体系地列全）。
    ///
    /// 点背景、按 Esc 或点「关闭」都收起（前两条由 `Modal` 自带）。
    fn settings_overlay(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let resp = egui::Modal::new(egui::Id::new("aox-settings"))
            .frame(
                Frame::NONE
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(theme::card_radius()))
                    .stroke(Stroke::new(1.0, P::border()))
                    .inner_margin(Margin::symmetric(20, 18)),
            )
            .show(ctx, |ui| {
                // 高度写死：两栏的切换不该让卡片跟着内容忽大忽小 ——
                // 从「系统」（一大列路径）切到「额外」（一个按钮）时整块缩水很难看。
                ui.set_width(SETTINGS_W);
                ui.set_height(SETTINGS_H);
                ui.horizontal_top(|ui| {
                    // 这一排的横向间距**一律由下面的 `add_space` 手写**。
                    //
                    // 不归零的话 `item_spacing`（主题里是 8）会叠加在每次 allocate 之间：
                    // 左栏与竖线之间多 8、竖线与会话之间又多 8 —— 总宽悄悄变成 716，
                    // 比 `SETTINGS_W` 多 16。于是卡片比设计宽 16px，而「右栏到底多宽」
                    // 从可算变成了要猜（这正是第一版被量出来的 758.7 而不是 740）。
                    ui.spacing_mut().item_spacing.x = 0.0;

                    // ── 左：栏目 ──
                    ui.vertical(|ui| {
                        ui.set_width(SETTINGS_NAV_W);
                        ui.set_height(SETTINGS_H);
                        for tab in SettingsTab::ALL {
                            if tab_row(ui, tab, self.settings_tab == tab).clicked() {
                                self.settings_tab = tab;
                            }
                        }
                    });

                    // 一条竖线把「选什么」与「看什么」分开。与「关于」里那条横线
                    // 同一套语汇（1px、border 色），只是转了个方向。
                    //
                    // 高度**写常量**而不是 `ui.available_height()`：在水平布局里后者会
                    // 随左边那一栏已经占掉的高度变化，竖线于是时高时矮。常量还顺带
                    // 把「这条线要贯穿整张卡片」这件事写明。
                    let (rule, _) =
                        ui.allocate_exact_size(Vec2::new(1.0, SETTINGS_H), Sense::hover());
                    ui.painter().vline(
                        rule.center().x,
                        rule.y_range(),
                        Stroke::new(1.0, P::border()),
                    );
                    ui.add_space(18.0);

                    // ── 右：内容 ──
                    ui.vertical(|ui| {
                        ui.set_width(SETTINGS_RIGHT_W);
                        // 高度也写死。理由：`ScrollArea` 取的是「可用高度 ∩ max_height」，
                        // 而在这个水平布局里，「可用高度」会被同一排的其它子项影响
                        // （竖线那一下 allocate 就把它吃掉过一次）。写死之后
                        // 「内容 + 间隙 + 底部按钮」三段的高度都可算，卡片高度就不会
                        // 随栏目内容变化 —— 那是「切栏目时卡片忽大忽小」的来源。
                        ui.set_height(SETTINGS_H);
                        // 底部那 SETTINGS_FOOT_H 留给「关闭」：不扣掉的话内容一长
                        // 就把按钮顶出卡片
                        ScrollArea::vertical()
                            .id_salt("aox-settings-body")
                            .max_height(SETTINGS_H - SETTINGS_FOOT_H)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                // ⚠️ 这一行是**整个弹窗稳不稳的关键**，别删。
                                //
                                // `ScrollArea` 的内宽 = 外宽 − 滚动条占位，而那个占位是
                                // **逐帧动画**的（`animate_bool_responsive` ×
                                // `spacing.scroll.allocated_width()`，本项目是 6px）。
                                // 只要内容宽度还跟着 `available_width()` 走，滚动条每呼吸
                                // 一次，行宽就变一次、换行的文字就重新折一次行 —— 界面表现
                                // 就是「整个面板在抖、字发虚」，而且**只在有重绘时发生**
                                // （鼠标一动就重绘，所以看起来是「鼠标一移就抖」）。
                                //
                                // 钉死之后：内容宽度是常量，折行稳定，滚动条只影响它自己
                                // 那 6px 的浮动条（浮在内容右侧，不压字）。
                                let content_w =
                                    SETTINGS_RIGHT_W - ui.spacing().scroll.allocated_width();
                                ui.set_width(content_w);
                                match self.settings_tab {
                                    SettingsTab::Extra => self.tab_extra(ui),
                                    SettingsTab::Appearance => self.tab_appearance(ui),
                                    SettingsTab::System => self.tab_system(ui),
                                    SettingsTab::About => self.tab_about(ui),
                                }
                            });
                        ui.add_space(6.0);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if flat_button(ui, "关闭").clicked() {
                                open = false;
                            }
                        });
                    });
                });
            });
        // 点背景、按 Esc 也算关闭
        if resp.should_close() || !open {
            self.settings_open = false;
        }
    }

    /// 「额外」栏：活动流入口。
    ///
    /// 这一栏刻意只有**一个入口、没有设置项** —— 它排在最前是因为用户这么要求，
    /// 但里面确实只有「打开活动流」这一件事（原先它占着右上角一枚图标）。
    fn tab_extra(&mut self, ui: &mut Ui) {
        // 先把锁收掉再判：`is_open()` 这一下要拿锁，而点击分支里的 `open()` 自己还会
        // 再拿一次 —— 两把锁叠在同一句里就是自锁（顶栏那版踩过）。
        let on = activity::lock(&self.act).is_open();
        if activity_open_button(ui, on).clicked() {
            activity::open(ui.ctx(), &self.act);
        }
        ui.add_space(9.0);
        ui.label(RichText::new("观测Agent内部循环逻辑").color(P::text_faint()).size(13.0));
    }

    /// 「外观」栏：明暗三档（含跟随系统）。
    ///
    /// 选哪一档只改 `theme::Pref`；「跟随系统」的实际取值由 `theme::effective` 从系统
    /// 主题算出来，并且**每帧复查**（见 `theme::follow_system`）——
    /// 不复查的话，「跟随系统」就只是「启动时照系统的样子定一次」。
    fn tab_appearance(&mut self, ui: &mut Ui) {
        let cur = theme::pref();
        for p in theme::Pref::ALL {
            if choice_row(ui, p.label(), cur == p).clicked() {
                theme::apply_pref(ui.ctx(), p);
            }
        }
    }

    /// 「系统」栏：这台机器、这份配置、这些目录。
    ///
    /// 顺序照用户点的单子排：系统信息 → 运行时环境 → 模型 → 配置 → 工作空间 →
    /// 历史 → 插件 → 运行时目录。
    fn tab_system(&self, ui: &mut Ui) {
        let p = self.boot.as_ref().map(|b| b.paths.clone()).unwrap_or_default();

        kv_row(ui, "系统信息", &p.host);

        // 运行时环境：**内置还是系统 PATH** —— 逐语言一行，来源直接写在方括号里。
        // 不画 ✓/✗：手写字体里那两个字形不一定有（调用流那两枚就是手绘的），
        // 而「未找到」这个词本身已经把结论说清楚了。
        for (i, it) in self.runtimes.items.iter().enumerate() {
            kv_row_colored(
                ui,
                if i == 0 { "运行时环境" } else { "" },
                &format!("{}  {}  [{}]", it.lang, it.command, it.origin),
                if it.ok { P::text_faint() } else { P::warn() },
            );
        }

        if !self.model.is_empty() {
            kv_row(ui, "所用模型", &self.model);
        }
        if !p.config.is_empty() {
            kv_row(ui, "配置文件", &p.config);
        }
        // 这份配置由哪几层合出来：`core → patch → config.toml`。
        //
        // 只显示名字链、不铺来源树（完整的那张表走 CLI 的 `aox --dump-config`）；
        // **只有一层时不显示** —— 说「就一层」等于没说话。这条判据是从原来那张
        // 「关于」小卡片里搬过来的，一字未改。
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
            kv_row(ui, "配置层", &layers.join(" → "));
        }
        if !p.root.is_empty() {
            kv_row(ui, "应用根目录", &p.root);
        }
        kv_row(ui, "工作空间", &self.workplace_line());
        if !p.history.is_empty() {
            kv_row(ui, "历史记录", &p.history);
        }
        if !p.plugins.is_empty() {
            kv_row(ui, "插件目录", &p.plugins);
        }
        if !p.runtime.is_empty() {
            // 「如果是内置的话」——目录在不在，一眼看得出来用没用内置运行时
            let note = if std::path::Path::new(&p.runtime).is_dir() {
                ""
            } else {
                "（尚未装配内置运行时）"
            };
            kv_row(ui, "运行时目录", &format!("{}{note}", p.runtime));
        }

        // 本机没有 Python 时给一个一键安装入口。原来它长在「关于」卡片里，
        // 而现在「关于」只管版本与协议 —— 装运行时是系统层面的事，放这一栏才对。
        if self.runtimes.python_missing {
            ui.add_space(14.0);
            ui.label(
                RichText::new("本机没有 Python，文件读写、系统信息等能力不可用。")
                    .color(P::text_dim())
                    .size(13.5),
            );
            ui.add_space(8.0);
            if primary_button(ui, "安装 Python 运行时（约 10 MB）").clicked() {
                // 走与 CLI 完全相同的命令路径（宿主自己处理，不发给模型）
                let _ = self.tx.send(ToAgent::Input("/runtime install python".into(), Vec::new()));
            }
        }
    }

    /// 工作空间那一行：「路径 + 档位」。
    ///
    /// 工作空间是**跟着对话走**的（`/new` 一律回默认档），所以这里显示的是**当前这段
    /// 对话**的那块地 —— 这也是为什么它能显示而不用等磁盘扫描：现状由 bridge 主动推。
    fn workplace_line(&self) -> String {
        use aox::workplace::Mode;
        let Some(v) = self.ws.view() else {
            // 刚启动的头一两帧还没收到回执。不猜一个目录，如实说还不知道。
            return "（还没收到回执）".into();
        };
        let name = match &v.mode {
            Mode::Default => "默认档".to_string(),
            Mode::Full => "完全访问档（写入不受限）".to_string(),
            Mode::Fixed(p) => format!("固定档 {}", p.display()),
        };
        match &v.root {
            Some(r) => format!("{}（{name}）", r.display()),
            // `default` 档要等第一条消息才开地。这里刻意与「不受限」分开说 —— 两件事。
            None => format!("（{name} · 还没开地）"),
        }
    }

    /// 「关于」栏：版本、协议、链接。
    ///
    /// 协议正文**直接嵌入仓库里那份 `LICENSE`**，不手抄一行 —— 手抄的那份一旦与文件
    /// 不一致，「关于」里写的就不是真正生效的那份协议了，而这种错没人会去核对。
    fn tab_about(&self, ui: &mut Ui) {
        // 刊头：衬线体的 AOX GUI + 版本号，底下一条细线（与首屏空状态同一个语汇）
        ui.horizontal(|ui| {
            ui.label(RichText::new("AOX GUI").font(theme::serif_font(24.0)).color(P::text()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .color(P::text_faint())
                        .size(13.5),
                );
            });
        });
        ui.add_space(9.0);
        let (rule, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
        ui.painter().hline(rule.x_range(), rule.center().y, Stroke::new(1.0, P::border()));

        ui.add_space(12.0);
        ui.label(
            RichText::new("基于 Rust 编写的 AOX 图形化应用，遵循开源协议 ISC。")
                .color(P::text_dim())
                .size(14.0),
        );

        ui.add_space(16.0);
        ui.label(RichText::new("ISC 协议全文").color(P::text_faint()).size(12.5));
        ui.add_space(6.0);
        // 协议正文装在一个带底色的框里。**可选中**：要引用某一条款时能直接复制，
        // 而不是照着屏幕重打一遍。
        Frame::NONE
            .fill(P::code_bg())
            .corner_radius(CornerRadius::same(12))
            .stroke(Stroke::new(1.0, P::border()))
            .inner_margin(Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.add(
                    Label::new(
                        RichText::new(LICENSE_TEXT.trim_end())
                            .font(theme::mono_font(12.0))
                            .color(P::text_faint()),
                    )
                    .selectable(true)
                    .wrap_mode(egui::TextWrapMode::Wrap),
                );
            });

        ui.add_space(16.0);
        link_row(ui, "作者博客", "https://2am.top");
        link_row(ui, "项目地址", "https://github.com/forever218/AOX");
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
        // 演示模式也给一份真实的路径清单：设置页的「系统」栏本来就是「看一眼就够」
        // 的诊断数据，用假路径预览毫无意义（而真路径在演示模式里同样读得到）。
        paths: BootPaths::new(
            &root,
            std::path::Path::new(&app.config_path),
            &root.join("plugins"),
            store.location().to_path_buf(),
        ),
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

    app.items.push(Item::User("做一个插件，统计当前目录下各类文件的数量".into(), Vec::new()));
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
        // 拖放：整窗口任意位置都接（见 `take_dropped`）。放在 `pump` 之后、绘制之前
        // —— 这一帧刚收下的附件要能在这一帧就出现在待上传栏里。
        self.take_dropped(ctx);

        // 图片解码：收尾（挂纹理）与排队（起后台线程）。每帧一次，幂等。
        self.poll_images(ctx);

        // 「跟随系统」的每帧复查：系统换了主题就跟着换（egui 每帧把当前系统主题放在
        // `ctx` 里）。不放在切换那一刻算一次的原因见 `theme::follow_system` 的注释 ——
        // 那样「跟随系统」会退化成「启动时照系统的样子定一次」。
        theme::follow_system(ctx);

        // 原生标题栏只留窗口标题本身：图标不设、标题里也不再缀模型名，
        // 免得左上角挤着一行「AOX · xxx」。底色同步见下面 sync_native_theme。
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        // `switching` 也要重绘：切换的回执从 agent 线程异步回来，空闲窗口
        // 默认不会自己重画，少了这一条就会「点了没反应」，直到下次鼠标一动。
        if self.busy || self.waiting_auth || self.live.is_some() || self.switching {
            self.spin += dt;
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        // 滑出动画推进：把 `hist_t` 朝开关状态挪一格，挪到为止。
        //
        // 逐帧推进（而不是「记开始时刻 + 查缓动表」）是为了让中途反向也接得上：
        // 连点两下开关时，进度从**当前位置**往回走，不会先跳回 0 再重开。
        let target = if self.show_history { 1.0 } else { 0.0 };
        if self.hist_t != target {
            let step = dt.min(0.05) / HIST_ANIM_SECS;
            self.hist_t = if self.hist_t < target {
                (self.hist_t + step).min(target)
            } else {
                (self.hist_t - step).max(target)
            };
            // 动画期间必须自己喊重绘：窗口空闲时 egui 不会自己刷帧，
            // 少了这一条，滑出会变成一顿一顿的跳格。
            ctx.request_repaint();
        }

        self.toolbar(ctx);

        if self.native_theme != Some(theme::mode()) {
            self.native_theme = Some(theme::mode());
            theme::sync_native_theme(ctx);
            theme::sync_caption(frame);
        }

        // 历史浮层：`hist_t` 归零后它自己就不画了，这里**不要**再判 `show_history`
        // —— 收起的过程也得接着画，判开关会让面板在半路上凭空消失。
        self.history_overlay(ctx);
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

        self.settings_overlay(ctx);
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

/// 历史浮层一次最多列出多少个历史对话。
///
/// 不做分页：几十条已经够「翻回去找刚才那段」了，而列全部意味着每次刷新
/// 都要扫完整个目录。真要翻更早的，`/history` 与 `history/` 目录都在。
const HISTORY_LIST_LIMIT: usize = 80;

// ────────────── 浮动按钮条与历史浮层的几何 ──────────────
//
// 这几个数原本散在两处：按钮的落点在 `top_bar` 的 `exact_height(50)` +
// `Margin::symmetric(20, 0)` 里，历史栏的宽度在 `SidePanel::default_width(266)` 里。
// 顶栏与侧栏都被拆掉之后，它们没有「容器」可依了，于是显式钉在这里 ——
// 位置照搬拆栏前的结果，逐像素不变。

/// 按钮热区边长（与三枚图标函数里的 `d` 是同一个数）。
const BAR_BTN: f32 = 36.0;
/// 按钮条距窗口**左**边。
const BAR_LEFT: f32 = 20.0;
/// 按钮条距窗口上边。`7 + 18 = 25`，正好是旧顶栏 50px 高的一半 ——
/// 按钮中心线与拆栏之前完全重合。
const BAR_TOP: f32 = 7.0;
/// 历史浮层的宽度。
const HISTORY_W: f32 = 300.0;
/// 历史浮层里「列表以上」那部分的高度（内边距 + 标题行 + 分隔线）。
///
/// 只用来算列表最多能占多高，取整即可 —— 面板的实际高度由内容自己决定，不靠它。
const HIST_HEAD: f32 = 74.0;
/// 滑出 / 收起的动画时长（秒）。
///
/// 0.24 是「看得出来在动、又不用等」的一档：更短就退回瞬移，更长会觉得
/// 点完之后界面在迟疑。
const HIST_ANIM_SECS: f32 = 0.24;

/// 顶栏滑出组里的两枚按钮 —— 只有它俩要做「从历史按钮背后钻出来」的裁剪。
///
/// 用一个枚举而不是写两段几乎一样的 `Area`：两枚按钮的位移、裁剪、留白
/// 必须逐像素一致，抄成两份的话下次改一处就会两边不齐（那种错很难在看代码时发现，
/// 只有截图对比才看得出来）。
#[derive(Clone, Copy)]
enum SlideIcon {
    /// 新建对话
    Plus,
    /// 设置
    Gear,
}

/// 缓出曲线（cubic ease-out）：起步快、收尾慢。
///
/// 用它而不是官方的 `ctx.animate_bool_with_time`（那是**线性**插值）：
/// 匀速位移在起点与终点都是硬拐弯 —— 起手像被弹了一下，停下像撞了墙。
fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

/// 一个「比窗口大得多」的约束矩形：给需要滑到窗口外的 `Area` 用。
///
/// `Area` 默认会把自己的位置拉回约束矩形内，滑出动画的位移会被当场抹平，
/// 所以那几处要放开。**但不要用 `Rect::EVERYTHING`** —— 里边是 ±∞，
/// 一旦参与「锚点对齐」之类的运算，位置就变成 NaN 或无穷，Area 直接消失
///（右上那排按钮栽过一次）。给个有限的宽松框，效果一样，还不会算出怪数。
fn loose_rect() -> Rect {
    Rect::from_min_max(Pos2::new(-10_000.0, -10_000.0), Pos2::new(10_000.0, 10_000.0))
}

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
    // 常态就铺一块底色（= 旧顶栏的填充色）：顶栏拆掉之后按钮直接浮在对话文字上，
    // 没有这块底，图标会和正文糊在一起。悬停升一档，与列表行、菜单行同一套语言。
    painter.rect_filled(
        rect,
        CornerRadius::same(10),
        if hov { P::surface_hi() } else { P::panel() },
    );
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
    painter.rect_filled(
        rect,
        CornerRadius::same(10),
        if hov { P::surface_hi() } else { P::panel() },
    );
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
                // `flat_text` 而不是 `content.clone()`：用户消息可能带附件，那时 content
                // 是 parts 数组 —— 直接 clone 一个字都显示不出来。宿主自己加的附件清单块
                // 由 `flat_text` 滤掉（见 `attach::ATTACH_MARK`）。
                let text = m.flat_text();
                // 胶囊的名字、以及要**显示**的图片路径，都从清单块里还原：
                // `image_url` 块里没有文件名字段（见 `attach::files_from_content`）。
                let files = m
                    .content
                    .as_ref()
                    .map(aox::attach::files_from_content)
                    .unwrap_or_default();
                if text.trim_start().starts_with("（宿主提示）") {
                    out.push(Item::Note(NoteKind::Warn, text.trim().to_string()));
                } else {
                    out.push(Item::User(
                        text,
                        files.iter().map(|(n, _)| n.clone()).collect(),
                    ));
                }
                let paths: Vec<PathBuf> = files.into_iter().map(|(_, p)| p).collect();
                if !paths.is_empty() {
                    out.push(Item::Images(paths));
                }
            }
            "assistant" => {
                let calls = m.tool_calls.clone().unwrap_or_default();
                let text = m.flat_text();
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
                let body = m.flat_text();
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

/// 设置：一个齿轮（36px 热区、单色描边、悬停换色 —— 与历史 / 「+」同一套画法）。
///
/// 齿轮画成 **8 齿的多边形 + 中间一个孔**，而不是「圆圈 + 八条放射线」：
/// 后者与 [`theme_toggle`] 画太阳的手法一模一样，缩在 36px 里两者会读成同一个图形
/// （都是「圆 + 八条线」）。齿用**折线**描出来才有轮廓感，一眼分得开。
///
/// 齿廓自己算：外圈齿顶与齿谷交替取点，绕一圈闭合。8 齿 × 4 点 = 32 个点。
fn gear_icon(ui: &mut Ui) -> egui::Response {
    let d = 36.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(d), Sense::click());
    let hov = pointer_if(ui, &resp);
    let fg = if hov { P::ink() } else { P::text_dim() };

    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(10),
        if hov { P::surface_hi() } else { P::panel() },
    );

    const TEETH: usize = 8;
    let c = rect.center();
    let r_out = 9.6_f32;
    let r_in = 6.9_f32;
    // 一圈分成 TEETH 段，每段里「齿顶 → 齿谷 → 齿谷 → 齿顶」四点。
    let step = std::f32::consts::TAU / TEETH as f32;
    let tooth = step * 0.22; // 齿顶占的那段角（其余是齿谷）
    let mut pts: Vec<Pos2> = Vec::with_capacity(TEETH * 4);
    for k in 0..TEETH {
        let a0 = step * k as f32;
        for (ang, r) in [
            (a0 - tooth, r_in),
            (a0, r_out),
            (a0 + tooth, r_out),
            (a0 + tooth * 2.0, r_in),
        ] {
            let (s, co) = ang.sin_cos();
            pts.push(Pos2::new(c.x + co * r, c.y + s * r));
        }
    }
    pts.push(pts[0]); // 闭合
    painter.add(egui::Shape::line(pts, Stroke::new(1.5, fg)));
    // 中心孔 —— 有它才像齿轮，没有就是个八角星
    painter.circle_stroke(c, 2.5, Stroke::new(1.5, fg));

    resp.on_hover_text("设置")
}

/// 那条「信息从这里流到那里」的 S 形流线。
///
/// 从原来顶栏那枚 `flow_toggle` 里**抽出来**的：那枚按钮已经删掉（活动流改从设置页
/// 打开），但这条曲线还要在设置页的「打开活动流」按钮上出现 —— 同一个图形只该有一份画法。
fn draw_flow_curve(painter: &egui::Painter, rect: Rect, fg: Color32) {
    // 控制点一上一下，得到一条平滑的 S 形
    let a = Pos2::new(rect.left(), rect.bottom());
    let b = Pos2::new(rect.left(), rect.top() + rect.height() * 0.12);
    let c = Pos2::new(rect.right(), rect.bottom() - rect.height() * 0.12);
    let e = Pos2::new(rect.right(), rect.top());

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
        vec![e + dir * 5.2, e - dir * 2.2 + n * 4.0, e - dir * 2.2 - n * 4.0],
        fg,
        Stroke::NONE,
    ));
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
/// 一枚附件胶囊（待上传栏里的一格）。返回 `true` 表示用户点了右边的 ×。
///
/// 整枚胶囊**不可点**，只有 × 是热区：这个胶囊没有「点开」的去处（附件不是链接），
/// 让整枚都可点就等于「点一下删一个」，太容易手滑。
/// 选项槽最后那一行：**行内输入**（回车或点右侧箭头作答）。返回用户打的字。
///
/// 第一版它只是个「自己输入…」的按钮，点了把焦点交给下面那个大输入框。用户的反馈
/// （2026-10-06）是「不用下面那个大的主输入框」—— 作答的地方就该在题目旁边，
/// 眼睛不必在上下两个框之间来回跑；而「点了按钮之后还得自己去找另一个框」这件事
/// 本身就很容易被当成「点了没反应」。
///
/// 回车与发送钮两条路都通：单行输入习惯回车，但鼠标用户不该被逼着碰键盘。
fn ask_input_row(ui: &mut Ui, w: f32, text: &mut String) -> Option<String> {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(10), P::surface());

    // 右边那枚发送钮（与主输入框那枚同一套画法，只是小一号）
    let btn = Rect::from_center_size(
        Pos2::new(rect.right() - 19.0, rect.center().y),
        Vec2::splat(26.0),
    );
    // 左边空出编号列（12 内缩 + 26 列宽），文字与上面的 A / B / C 左对齐
    let edit_rect = Rect::from_min_max(
        Pos2::new(rect.left() + 38.0, rect.top() + 7.0),
        Pos2::new(btn.left() - 6.0, rect.bottom() - 7.0),
    );

    let resp = ui.put(
        edit_rect,
        TextEdit::singleline(text)
            .frame(false)
            .desired_width(edit_rect.width())
            .font(FontId::proportional(13.5))
            .hint_text(
                RichText::new("也可以在这里直接输入…").color(P::text_faint()).size(13.0),
            ),
    );
    // 单行 TextEdit 的「回车」标准判据：这一帧**失去焦点**且按下了回车。
    // 只看 `key_pressed` 不行 —— 那样打字时按回车会误触（它不会失焦）。
    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

    let typed = text.trim().to_string();
    let can = !typed.is_empty();
    let bresp = ui.interact(btn, ui.id().with("aox-ask-send"), Sense::click());
    let hot = can && pointer_if(ui, &bresp);
    let p = ui.painter();
    if can {
        p.circle_filled(
            btn.center(),
            btn.width() * 0.5,
            if hot { P::ink().gamma_multiply(0.75) } else { P::ink() },
        );
    } else {
        // 空着的时候画个空心圈：让人看得出「这里有个按钮，只是先得写点什么」
        p.circle_stroke(btn.center(), btn.width() * 0.5, Stroke::new(1.0, P::border()));
    }
    paint_arrow_up(p, btn, if can { P::bg() } else { P::text_faint() });

    (can && (enter || bresp.clicked())).then_some(typed)
}

/// 选项槽里的一行选项：`A` + 正文。返回是否被点。
fn ask_row(ui: &mut Ui, w: f32, idx: usize, text: &str) -> bool {
    ask_chip(ui, w, idx, text)
}

/// 选项槽里的一行：`A` + 正文。
///
/// 自绘 + `allocate_exact_size(.., Sense::click())`，**刻意不用 `Frame::show` 之后再
/// `interact()`**：后者在窄宽度下命中区会跟着内容一起被压扁（第一版就是这样 ——
/// 画出来是条 28px 宽的竖条，点上去毫无反应）。自绘的话命中区就是这一整行，
/// 宽度确定、与画出来的完全重合。
///
/// 编号列占固定宽度，于是 A / B / C 的文字左边**对得齐** —— 选项槽是一列，不是一堆。
fn ask_chip(ui: &mut Ui, w: f32, idx: usize, text: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 32.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    p.rect_filled(
        rect,
        CornerRadius::same(10),
        if resp.hovered() { P::surface_hi() } else { P::surface() },
    );
    // 悬停时再描一圈：光靠底色那一档，在浅色主题下几乎看不出鼠标停在哪一行
    if resp.hovered() {
        p.rect_stroke(
            rect,
            CornerRadius::same(10),
            Stroke::new(1.0, P::ink()),
            egui::StrokeKind::Inside,
        );
    }

    const NUM_W: f32 = 26.0;
    let g = p.layout_no_wrap(
        aox::tool::ask::letter(idx).to_string(),
        FontId::proportional(13.5),
        P::text_faint(),
    );
    p.galley(
        Pos2::new(rect.left() + 12.0, rect.center().y - g.size().y * 0.5),
        g,
        P::text_faint(),
    );
    let color = P::text();
    // 按可用宽度估一个上限再省略：中文按 ~13px、西文按 ~7px 折中取 11px，
    // 估不准也只是多留或少留一点白 —— 而**不省略**的话长选项会直接冲出这一行。
    let keep = (((w - NUM_W - 24.0) / 11.0).max(12.0)) as usize;
    let g = p.layout_no_wrap(middle_elide(text, keep), FontId::proportional(13.5), color);
    p.galley(
        Pos2::new(rect.left() + NUM_W + 8.0, rect.center().y - g.size().y * 0.5),
        g,
        color,
    );
    resp.clicked()
}

fn attach_chip(ui: &mut Ui, name: &str) -> bool {
    let mut removed = false;
    Frame::NONE
        .fill(P::surface_hi())
        .corner_radius(CornerRadius::same(13))
        .stroke(Stroke::new(1.0, P::border()))
        .inner_margin(Margin::symmetric(11, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                ui.label(RichText::new(middle_elide(name, 30)).color(P::text_dim()).size(13.0));
                removed = close_x(ui);
            });
        });
    removed
}

/// 胶囊右边的 ×。返回 `true` 表示被点了。
///
/// 用两条线段手绘而不是打 `×` 字符 —— 与调用流里的 ✓/✗ 同一个理由：
/// 不赌字体里有没有这个字形（缺字形时画出来是个豆腐块，比不画更糟）。
fn close_x(ui: &mut Ui) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(15.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("移除");
    let col = if resp.hovered() { P::text() } else { P::text_faint() };
    let c = rect.center();
    let s = 3.4;
    let stroke = Stroke::new(1.4, col);
    ui.painter().line_segment([c + Vec2::new(-s, -s), c + Vec2::new(s, s)], stroke);
    ui.painter().line_segment([c + Vec2::new(s, -s), c + Vec2::new(-s, s)], stroke);
    resp.clicked()
}

/// 过长时**中间省略**：留住头尾，砍中间。
///
/// 为什么砍中间不砍尾巴：这条信息里最有用的一位是**后缀**（`.png` 还是 `.pdf`，
/// 决定用户要不要先压一下），而尾巴正好是它。`keep` 是保留的字符数（含省略号）。
fn middle_elide(s: &str, keep: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    // 太短就不用省；`keep` 小到放不下「头 + 省略号 + 尾」时也别硬省（会显示成一团）
    if chars.len() <= keep || keep < 9 {
        return s.to_string();
    }
    let head = keep / 2;
    let tail = keep - head - 1; // 减掉的 1 是省略号自己占的那位
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
}

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

fn user_bubble(ui: &mut Ui, text: &str, files: &[String], max_w: f32) {
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
                    // 附件胶囊排在正文**上面**：与待上传栏里的位置一致（发出前后同一个观感），
                    // 而且空正文时（只发了文件）气泡里也有东西可看。
                    if !files.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(7.0, 7.0);
                            for name in files {
                                sent_chip(ui, name);
                            }
                        });
                        if !text.trim().is_empty() {
                            ui.add_space(8.0);
                        }
                    }
                    if text.trim().is_empty() {
                        return;
                    }
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

/// 已经发出去的消息气泡里的附件胶囊 —— 只读（没有 ×）。
///
/// 与待上传栏那枚**同一个形状、同一套省略规则**，只少一个 ×：这样「发出去」这个动作
/// 在视觉上就是「胶囊淡下去、×消失了」，不需要另一种标记去解释。
fn sent_chip(ui: &mut Ui, name: &str) {
    Frame::NONE
        .fill(P::surface())
        .corner_radius(CornerRadius::same(13))
        .stroke(Stroke::new(1.0, P::border()))
        .inner_margin(Margin::symmetric(11, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(middle_elide(name, 30)).color(P::text_dim()).size(13.0));
        });
}

fn assistant_block(ui: &mut Ui, text: &str, width: f32) {
    markdown::render(ui, text, P::text());
    let _ = width;
}

/// 看起来像图片的后缀。**只用来决定「值不值得试着解一下」**，不用来判定格式 ——
/// 真正说了算的是解码器本身（`aox::image::decode`）。所以后缀不在表里、内容其实是
/// PNG 的文件不会显示成图，这是这条捷径的代价；而反过来「后缀像图片但内容不是」
/// 会被解码器拒绝，界面按「显示失败」如实说一句，不会假装成功。
fn looks_like_image(p: &std::path::Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| {
            matches!(
                e.as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff"
            )
        })
}

/// 一条消息带的图片。按版心宽度缩放、保持长宽比。
///
/// 三态各自有各自的画法（见 [`Tex`]）：图 / 「正在解码…」/ 一句失败原因。
/// **不放大**（`scale` 上限 1.0）：把一张 200×80 的示意图拉满版心只会糊成马赛克，
/// 而用户看到「它就是这么大」比看到一张虚掉的大图有用。
fn images_block(ui: &mut Ui, paths: &[PathBuf], inner: f32, tex: &HashMap<PathBuf, Tex>) {
    for p in paths {
        if !looks_like_image(p) {
            // 不是图片（.txt / .zip 之类）：本来就不该有画面，安静跳过。
            // 它的名字在上面的气泡 / 提示行里已经有了。
            continue;
        }
        ui.add_space(6.0);
        match tex.get(p) {
            Some(Tex::Ready(h)) => {
                let size = h.size_vec2();
                let scale = (inner.min(760.0) / size.x).min(460.0 / size.y).min(1.0);
                let show = size * scale.max(0.05);
                ui.add(egui::Image::new(egui::load::SizedTexture::new(h.id(), show)).corner_radius(12.0));
            }
            Some(Tex::Failed(e)) => {
                ui.label(
                    RichText::new(format!("显示不了这张图：{e}"))
                        .color(P::text_faint())
                        .size(12.5),
                );
            }
            // 排队中 / 正在解码。两者对用户是一回事，不必分开说。
            _ => {
                ui.label(RichText::new("正在解码图片…").color(P::text_faint()).size(12.5));
            }
        }
        ui.add_space(2.0);
    }
    ui.add_space(4.0);
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
                for st in &p.steps {
                    step_row(ui, st);
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

/// 实时区里的一段「模型正在说的话」。
///
/// 故意用**普通正文**渲染、不加 Markdown、不加气泡边框：它是同一段文字在落盘之前的临时
/// 形态，落盘后会换成带 Markdown 的气泡。中途另起一套样式，两段看着就不像同一句话，
/// 切换时会「跳」一下。`break_anywhere` 开着是为了防止答案里的长路径把版心顶破 ——
/// 这里只求不溢出，好看与否交给最终那条气泡。
fn live_plain_text(ui: &mut Ui, text: &str, color: Color32) {
    let w = ui.available_width();
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = w;
    job.wrap.break_anywhere = true;
    job.append(
        text,
        0.0,
        egui::TextFormat { font_id: theme::body_font(15.0), color, ..Default::default() },
    );
    ui.add(Label::new(job));
}

fn step_row(ui: &mut Ui, st: &StepView) {
    ui.horizontal_top(|ui| {
        // 状态标记（用户 2026-10-05 定的形态）：进行中 = 三点跑马灯（**刻意与思考中那个
        // 8 点绕圈的 spinner 不同形**）、成功 = ✓、失败 = ✗。
        // 也不再编号 —— 编号会把「一个阶段的几步调用」读成一份清单，这里要的是
        // 「一个接着下一个」。
        match &st.result {
            None => call_marquee(ui, P::text_dim()),
            Some((is_err, _)) if *is_err => call_glyph(ui, false, P::ink()),
            Some(_) => call_glyph(ui, true, P::ink()),
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
        // 参数**不在这里显示**（用户 2026-10-05 提的）：一串 JSON 会把这一行撑成好几行，
        // 展开后更是一眼看不到底。行上只留「工具名 + 归属插件」，结果在下一行给一行摘要。
        // 真要看参数，`history/*.jsonl` 里原样存着。
    });
    ui.add_space(5.0);
}

/// 实时区里的一段调用流（某句阶段回复之前的那些调用）。
///
/// 实时区**一段一张卡**（见 `live_block` 的切段循环），过程卡片那边则是一张卡装全部 ——
/// 行渲染共用同一套，只是分组不同。
fn calls_card(ui: &mut Ui, steps: &[StepView]) {
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
            for st in steps {
                step_row(ui, st);
            }
        });
}

/// 调用流里的「进行中」：三个点，亮点依次后移。
///
/// 与 [`spinner_dot`] 那个 8 点绕圈**刻意不同形**（用户要求「有区别于思考中的加载动画」）：
/// 一眼就能分出「是在调工具」还是「是在想」。
fn call_marquee(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(16.0, 20.0), Sense::hover());
    // 相位取 `ui.input(|i| i.time)`（egui 自增的秒数）：任何位置都能直接调它，
    // 不必把 `App` 的 `spin` 一路传进来。
    let t = ui.input(|i| i.time) as f32;
    let active = ((t * 3.0) as usize) % 3;
    let painter = ui.painter();
    for i in 0..3 {
        let hot = i == active;
        let p = Pos2::new(rect.left() + 3.0 + i as f32 * 5.0, rect.center().y);
        painter.circle_filled(
            p,
            if hot { 2.4 } else { 1.8 },
            color.gamma_multiply(if hot { 1.0 } else { 0.22 }),
        );
    }
}

/// ✓ / ✗。用两条线段手绘 —— 不赌字体里有没有这两个字形（与 `paint_folder` 同一个理由）。
fn call_glyph(ui: &mut Ui, ok: bool, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(16.0, 20.0), Sense::hover());
    let c = rect.center();
    let s = 4.4;
    let stroke = Stroke::new(1.8, color);
    let painter = ui.painter();
    if ok {
        painter.line_segment(
            [Pos2::new(c.x - s, c.y + 0.6), Pos2::new(c.x - 1.4, c.y + s)],
            stroke,
        );
        painter.line_segment([Pos2::new(c.x - 1.4, c.y + s), Pos2::new(c.x + s, c.y - s)], stroke);
    } else {
        painter.line_segment([Pos2::new(c.x - s, c.y - s), Pos2::new(c.x + s, c.y + s)], stroke);
        painter.line_segment([Pos2::new(c.x + s, c.y - s), Pos2::new(c.x - s, c.y + s)], stroke);
    }
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

// ────────────── 设置弹窗 ──────────────

/// 设置弹窗的**内容区**尺寸（不含内边距 20/18）。
///
/// 比原来那枚 380px 的「关于」小卡片大一圈：它要装一张两栏的表，右栏还是路径清单
/// （`E:\rustapp\workplace\2026-10-05-…` 这种值短不了）。
const SETTINGS_W: f32 = 700.0;
/// 高度写死，理由是「切栏目时卡片不该忽大忽小」——见 `settings_overlay` 的注释。
const SETTINGS_H: f32 = 470.0;
/// 左栏（栏目列表）宽度。
const SETTINGS_NAV_W: f32 = 132.0;
/// 右栏宽度 = 总宽 − 左栏 − 竖线 1px − 间距 18px。
const SETTINGS_RIGHT_W: f32 = SETTINGS_W - SETTINGS_NAV_W - 19.0;
/// 底部「关闭」那一行占掉的高度（含它与内容之间的间隙）。
const SETTINGS_FOOT_H: f32 = 44.0;
/// 「名字 + 值」那一行里左列（名字）占的宽度。固定住值才会左右对齐成一条线。
const KV_KEY_W: f32 = 84.0;

/// 开源协议正文。**直接嵌入仓库里那份 `LICENSE`** —— 不手抄：手抄的那份迟早与文件
/// 不一致，而「关于」里写的必须是真正生效的那一份。
const LICENSE_TEXT: &str = include_str!("../../LICENSE");

/// 左栏里的一行栏目。
///
/// 选中态用两件事说：「底升一档 + 文字转主色」加左侧一小段竖线。只靠底色的话，
/// 在这个色阶很窄的单色方案里（`surface_hi` 与 `surface` 只差一档）几乎看不出来。
fn tab_row(ui: &mut Ui, tab: SettingsTab, selected: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let fg = if selected { P::ink() } else { P::text_dim() };

    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, CornerRadius::same(9), P::surface_hi());
        painter.rect_filled(
            Rect::from_min_size(
                Pos2::new(rect.left() + 6.0, rect.center().y - 7.0),
                Vec2::new(2.0, 14.0),
            ),
            CornerRadius::same(1),
            fg,
        );
    } else if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(9), P::panel());
    }

    let g = painter.layout_no_wrap(tab.label().to_string(), egui::FontId::proportional(14.5), fg);
    painter.galley(Pos2::new(rect.left() + 18.0, rect.center().y - g.size().y * 0.5), g, fg);
    resp
}

/// 单选行（「外观」那一栏用）：右侧一个圆点，选中的是实心。
///
/// 圆点而不是勾：勾的形状在这个界面里已经被「工作区档位」用掉了（那是「当前生效的
/// 档位」），再用一次会读成同一件事。实心 / 空心是「选中 / 未选中」最省解释的画法。
fn choice_row(ui: &mut Ui, label: &str, selected: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(9), P::surface_hi());
    }
    let fg = if selected { P::text() } else { P::text_dim() };
    let g = painter.layout_no_wrap(label.to_string(), egui::FontId::proportional(14.5), fg);
    painter.galley(Pos2::new(rect.left() + 4.0, rect.center().y - g.size().y * 0.5), g, fg);

    let dot = Pos2::new(rect.right() - 12.0, rect.center().y);
    if selected {
        painter.circle_filled(dot, 5.0, P::ink());
    } else {
        painter.circle_stroke(dot, 5.0, Stroke::new(1.5, P::border_strong()));
    }
    resp
}

/// 「打开活动流」按钮：左边那条流线 + 右边文字。
///
/// 为什么不是一枚纯文字按钮：那条流线是活动流**唯一的视觉身份**（原来顶栏那枚图标就是
/// 它），丢掉之后这一栏就只剩四个中文字，得先读一遍才知道点开是什么。
///
/// 已经开着时整枚按钮淡一档：一个写着「打开」的按钮再点一下**不该把窗口关掉**，
/// 所以那时它不可点（`activity::open` 里也是这么做的，见它的注释）。
fn activity_open_button(ui: &mut Ui, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(196.0, 42.0), Sense::click());
    let hot = resp.hovered() && !on;
    if hot {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let fg = if on { P::text_faint() } else { P::text() };

    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(12),
        if hot { P::surface_hi() } else { P::panel() },
    );
    painter.rect_stroke(
        rect,
        CornerRadius::same(12),
        Stroke::new(1.0, P::border()),
        egui::StrokeKind::Inside,
    );
    // 图标：18px 见方的流线，靠左内缩 22
    let icon =
        Rect::from_center_size(Pos2::new(rect.left() + 22.0, rect.center().y), Vec2::splat(18.0));
    draw_flow_curve(painter, icon, fg);
    let g = painter.layout_no_wrap("打开活动流".to_string(), egui::FontId::proportional(14.5), fg);
    painter.galley(Pos2::new(rect.left() + 42.0, rect.center().y - g.size().y * 0.5), g, fg);
    resp
}

/// 「名字 + 值」一行（「系统」那一栏全是这种行）。
fn kv_row(ui: &mut Ui, key: &str, val: &str) {
    kv_row_colored(ui, key, val, P::text_faint());
}

/// 同上，但值可以换个颜色（「运行时环境」用它把「未找到」标出来）。
fn kv_row_colored(ui: &mut Ui, key: &str, val: &str, color: Color32) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(KV_KEY_W, 19.0), Sense::hover());
        if !key.is_empty() {
            let p = ui.painter();
            let g = p.layout_no_wrap(key.to_string(), egui::FontId::proportional(13.5), P::text_dim());
            p.galley(Pos2::new(r.left(), r.top() + 1.0), g, P::text_dim());
        }
        // 值一律等宽：它们是路径、命令、版本号，不是散文。可选中 —— 路径经常是要
        // 复制到别处去用的（终端、资源管理器），只读的显示等于让人照着敲一遍。
        ui.add(
            Label::new(RichText::new(val).font(theme::mono_font(12.5)).color(color))
                .selectable(true)
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    });
    ui.add_space(4.0);
}

/// 「名字 + 可点链接」一行（「关于」那一栏用）。
fn link_row(ui: &mut Ui, key: &str, url: &str) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(KV_KEY_W, 19.0), Sense::hover());
        let p = ui.painter();
        let g = p.layout_no_wrap(key.to_string(), egui::FontId::proportional(13.5), P::text_dim());
        p.galley(Pos2::new(r.left(), r.top() + 1.0), g, P::text_dim());

        let resp = ui.add(
            Label::new(RichText::new(url).color(P::text()).size(13.5).underline())
                .sense(Sense::click())
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
    });
    ui.add_space(4.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把「设置弹窗为什么必须**钉死**内容宽度」这件事钉成一条可执行的说明。
    ///
    /// 这一条测的是 **egui 的一个行为**，不是我们的代码：`ScrollArea` 会按
    /// `animate_bool_responsive` **逐帧动画**地给滚动条分配宽度，于是它内部的内容宽度
    /// 会连续变化。凡是拿 `available_width()` 当尺寸的控件都会跟着变宽变窄、换行的文字
    /// 会每帧重新折行 —— 这正是 2026-10-05 那次「鼠标一动，设置面板就轻微震动、
    /// 字发虚」的根因（没有重绘动画就不推进，所以它只在鼠标移动时才现形）。
    ///
    /// 为什么值得单独钉住：那个行为在源码里只体现为两行减法
    /// （`current_bar_use = 动画量 × allocated_width()`），翻代码完全看不出它会牵动布局；
    /// 而一旦有人把 `settings_overlay` 里的 `ui.set_width(content_w)` 当成多余的删掉，
    /// 抖动会原样回来 —— 手动测又未必碰得到。所以这条测试的**失败信息**里写清了
    /// 该去哪一段注释核对。
    #[test]
    fn egui_animates_the_scroll_bar_allocation() {
        let ctx = egui::Context::default();
        // 与本项目 `theme::install_style` 一致：浮动样式 + 占位宽度（thin() 给 6px）
        ctx.style_mut(|s| s.spacing.scroll = egui::style::ScrollStyle::thin());
        assert!(
            ctx.style().spacing.scroll.allocated_width() > 0.0,
            "前提：样式里滚动条要占位，否则这条测试什么也证明不了"
        );

        let mut widths: Vec<f32> = Vec::new();
        for frame in 0..8 {
            let input = egui::RawInput {
                // 动画靠 `time` 推进：不给时间变化，`animate_bool` 就永远停在起点
                time: Some(frame as f64 * 0.05),
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 200.0))),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ScrollArea::vertical()
                        .id_salt("aox-test-scroll")
                        .max_height(80.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            widths.push(ui.available_width());
                            // 内容明显高于视口，逼出滚动条
                            for _ in 0..40 {
                                ui.label("x");
                            }
                        });
                });
            });
        }

        let first = widths[0];
        let changed = widths.iter().any(|w| (w - first).abs() > 0.5);
        assert!(
            changed,
            "egui 不再逐帧改滚动条占位了？那 `App::settings_overlay` 里 \
             `ui.set_width(content_w)` 那一段的**理由注释**要跟着更新 —— \
             它存在的唯一目的就是躲开这个动画（不是多余的）。实测宽度序列：{widths:?}"
        );
    }
}
