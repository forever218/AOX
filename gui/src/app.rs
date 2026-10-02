//! GUI 主界面状态与绘制。
//!
//! 布局（参考主流 AI 编程软件）：
//! ```text
//! ┌───────────────────────────────────────────────┬──────────────┐
//! │ 顶栏：AOX · 状态 · 模型 · [明暗] [活动流]      │  活动流面板   │
//! ├───────────────────────────────────────────────┤ （可折叠）    │
//! │ 对话流：用户气泡 / 助手 Markdown / 折叠的过程  │  状态 + 实时  │
//! │                                                │  思考 + 工具  │
//! │                  ┌──────────────────┐          │  调用记录     │
//! │                  │ 悬浮输入框（覆盖）│          │              │
//! └──────────────────┴──────────────────┴──────────┴──────────────┘
//! ```
//!
//! 底部没有独立的「输入区」栏位：输入框是一枚浮在对话内容之上的卡片，
//! 滚动时可以一直滑到它下面。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{
    Align, Align2, Color32, CornerRadius, Frame, Label, Layout, Margin, Pos2, Rect, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Ui, Vec2,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use aox::ui::{AgentEvent, AuthDecision};

use crate::bridge::{BootInfo, FromAgent, SessionFlags, Snapshot, ToAgent};
use crate::markdown;
use crate::theme::{self, P};

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
    elapsed: f32,
    expanded: bool,
    aborted: bool,
}

/// 对话流中的一条记录。
enum Item {
    User(String),
    Assistant(String),
    Process(Process),
    Note(NoteKind, String),
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActKind {
    Turn,
    Think,
    Call,
    Run,
    Ok,
    Err,
    Info,
    Warn,
    Error,
}

impl ActKind {
    /// 单色方案下不得不用明度分层，所以这里定义的是「强调程度」而不是颜色：
    /// 关键节点（任务开始/工具调用/失败）用正文色，完成类用次要色，过程类最弱。
    fn color(self) -> Color32 {
        match self {
            ActKind::Turn => P::text(),
            ActKind::Call => P::text(),
            ActKind::Err => P::text(),
            ActKind::Error => P::text(),
            ActKind::Ok => P::text_dim(),
            ActKind::Info => P::text_dim(),
            ActKind::Warn => P::text_dim(),
            ActKind::Think => P::text_faint(),
            ActKind::Run => P::text_faint(),
        }
    }

    /// 失败类节点额外画一个叉，免得只靠明度差看不出来。
    fn is_failure(self) -> bool {
        matches!(self, ActKind::Err | ActKind::Error)
    }

    fn mono(self) -> bool {
        matches!(self, ActKind::Call | ActKind::Run | ActKind::Ok | ActKind::Err | ActKind::Think)
    }
}

/// 活动流中的一行。
struct Act {
    kind: ActKind,
    at: f32,
    title: String,
    body: String,
    live: bool,
    detail: bool,
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
    acts: VecDeque<Act>,
    auth: Option<AuthCard>,
    auth_reply: Option<oneshot::Sender<AuthDecision>>,

    input: String,
    focus_input: bool,
    busy: bool,
    waiting_auth: bool,
    status: String,
    spin: f32,

    model: String,
    config_path: String,
    plugins: Vec<String>,
    /// 上一次写进窗口标题的模型名。原生标题栏与界面融为一体后，
    /// 「AOX · <模型>」放这里最自然，也就不必在顶栏再堆一个模型胶囊。
    title_model: String,

    show_activity: bool,
    /// 演示模式（--demo）：不连 LLM，只展示界面与事件流的排布
    demo: bool,
    /// 对话流是否自动跟随到底部
    follow: bool,
    /// 本条消息强制滚到底（用户刚发指令时）
    force_follow: bool,
    /// 活动流是否自动跟随
    follow_acts: bool,
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
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        tx: UnboundedSender<ToAgent>,
        rx: UnboundedReceiver<FromAgent>,
        flags: Arc<SessionFlags>,
        driver: Option<std::thread::JoinHandle<()>>,
        demo: bool,
        prompt: Option<String>,
    ) -> Self {
        theme::load_pref();
        theme::apply(&cc.egui_ctx);
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
            acts: VecDeque::new(),
            auth: None,
            auth_reply: None,
            input: String::new(),
            focus_input: false,
            busy: false,
            waiting_auth: false,
            status: "就绪".into(),
            spin: 0.0,
            model: String::new(),
            config_path: String::new(),
            plugins: Vec::new(),
            title_model: String::new(),
            show_activity: true,
            follow: true,
            force_follow: true,
            follow_acts: true,
            about_open: false,
            log_error: None,
            pending_prompt: if demo { None } else { prompt },
            auth_shown_at: Instant::now(),
            fit_tries: 0,
            native_theme: None,
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

        // 任务栏高度没有 API 可查，按显示器高度取一个保守比例（默认约 48 逻辑点）
        let taskbar = (mon.y * 0.06).clamp(36.0, 72.0);
        // 贴边留一点呼吸空间
        let margin = Vec2::new(80.0, 28.0);

        let want = Vec2::new(
            (mon.x - margin.x - chrome.x).min(1380.0).max(1000.0),
            (mon.y - taskbar - margin.y - chrome.y).min(900.0).max(620.0),
        );

        if (inner.x - want.x).abs() > 8.0 || (inner.y - want.y).abs() > 8.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(want));
        }

        // 居中摆放（外框 = 客户区 + 装饰），保证下沿不越过任务栏
        let outer_size = want + chrome;
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

    fn push_act(&mut self, kind: ActKind, title: impl Into<String>, body: impl Into<String>, detail: bool) {
        let at = self.now();
        self.acts.push_back(Act {
            kind,
            at,
            title: title.into(),
            body: body.into(),
            live: false,
            detail,
        });
        while self.acts.len() > 500 {
            self.acts.pop_front();
        }
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
                FromAgent::TurnStart => {
                    self.live = Some(Live {
                        status: "思考中".into(),
                        reasoning: String::new(),
                        reasoning_live: false,
                        steps: Vec::new(),
                        interim: Vec::new(),
                        pending_final: None,
                        started: Instant::now(),
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
                FromAgent::Reasoning(d) => {
                    if let Some(l) = self.live.as_mut() {
                        if !l.reasoning_live {
                            l.reasoning_live = true;
                            let at = self.started.elapsed().as_secs_f32();
                            self.acts.push_back(Act {
                                kind: ActKind::Think,
                                at,
                                title: "思考过程".into(),
                                body: String::new(),
                                live: true,
                                detail: true,
                            });
                        }
                        if l.reasoning.len() < 120_000 {
                            l.reasoning.push_str(&d);
                        }
                        match self.acts.back_mut() {
                            Some(a) if a.kind == ActKind::Think && a.live => {
                                a.body.push_str(&d);
                                clamp_tail(&mut a.body, 6000);
                            }
                            _ => {}
                        }
                    }
                            }
                FromAgent::ReasoningEnd => {
                    if let Some(l) = self.live.as_mut() {
                        l.reasoning_live = false;
                    }
                    if let Some(a) = self.acts.back_mut() {
                        if a.kind == ActKind::Think && a.live {
                            a.live = false;
                        }
                    }
                }
                FromAgent::Event(ev) => self.on_event(ev),
                FromAgent::TurnEnd => self.finish_turn(false),
                FromAgent::TurnAborted => {
                    self.push_act(ActKind::Err, "任务已中断", "", false);
                    self.finish_turn(true);
                }
                FromAgent::AuthRequest { tool, args, reply } => {
                    self.auth = Some(AuthCard { tool: tool.clone(), args: args.clone() });
                    self.auth_reply = Some(reply);
                    self.waiting_auth = true;
                    self.auth_shown_at = Instant::now();
                    self.push_act(ActKind::Call, format!("请求授权 · {tool}"), args, true);
                }
                FromAgent::Fatal(e) => {
                    self.items.push(Item::Error(e.clone()));
                    self.push_act(ActKind::Error, "错误", e, true);
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

    fn on_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::AssistantText(t) => {
                match self.live.as_mut() {
                    Some(l) => {
                        if let Some(prev) = l.pending_final.replace(t) {
                            l.interim.push(prev);
                        }
                    }
                    None => self.items.push(Item::Assistant(t)),
                }
            }
            AgentEvent::InterimText(t) => {
                if let Some(l) = self.live.as_mut() {
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
                let title = match &plugin {
                    Some(p) if p != "meta" => format!("调用 {name}"),
                    _ => format!("调用 {name}"),
                };
                self.push_act(ActKind::Call, title, args, true);
            }
            AgentEvent::ToolRunning { name, timeout_secs } => {
                self.status = format!("执行 {name}");
                self.push_act(ActKind::Run, format!("执行中 {name}"), format!("超时 {timeout_secs}s"), false);
            }
            AgentEvent::ToolResult { name, is_error, preview } => {
                if let Some(l) = self.live.as_mut() {
                    if let Some(st) =
                        l.steps.iter_mut().rev().find(|s| s.name == name && s.result.is_none())
                    {
                        st.result = Some((is_error, preview.clone()));
                    }
                }
                let kind = if is_error { ActKind::Err } else { ActKind::Ok };
                self.push_act(kind, format!("{}{name}", if is_error { "失败 " } else { "完成 " }), preview, true);
            }
            AgentEvent::Info(m) => {
                self.push_act(ActKind::Info, "信息", m.clone(), false);
                if self.live.is_none() {
                    self.items.push(Item::Note(NoteKind::Info, m));
                }
            }
            AgentEvent::Warn(m) => {
                self.push_act(ActKind::Warn, "警告", m.clone(), false);
                if self.live.is_none() {
                    self.items.push(Item::Note(NoteKind::Warn, m));
                }
            }
            AgentEvent::Error(m) => {
                self.push_act(ActKind::Error, "错误", m.clone(), true);
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
                elapsed,
                expanded: false,
                aborted,
            }));
        }
        if let Some(f) = l.pending_final {
            self.items.push(Item::Assistant(f));
        }
        self.push_act(
            if aborted { ActKind::Err } else { ActKind::Turn },
            if aborted { "任务中断" } else { "任务结束" },
            format!("耗时 {elapsed:.1}s"),
            false,
        );
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
        self.push_act(ActKind::Turn, "新指令", text.clone(), true);
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
                    // 品牌名交给原生标题栏（它本来就写着窗口标题），这里只留状态，
                    // 否则同一个「AOX」会在紧挨着的两行里出现两次。
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), Sense::hover());
                    ui.painter().circle_filled(
                        rect.center(),
                        4.5,
                        if self.busy { P::ink() } else { P::ink_soft() },
                    );
                    ui.add_space(9.0);
                    ui.label(
                        RichText::new(if self.status.is_empty() { "就绪" } else { &self.status })
                            .color(if self.busy { P::text() } else { P::text_dim() })
                            .size(15.0),
                    );
                    if self.busy {
                        ui.add_space(7.0);
                        ui.label(
                            RichText::new(format!("{:.1}s", self.live.as_ref().map_or(0.0, |l| {
                                l.started.elapsed().as_secs_f32()
                            })))
                            .color(P::text_faint())
                            .size(13.5),
                        );
                    }

                    // 右上角只留三个图标：活动流开关 + 明暗切换 + 关于。
                    // 原来那排「模型 / 工具 N / 插件 N / 演示」的胶囊全部去掉——
                    // 这些信息要么在活动流里能看到，要么属于看一眼就够的诊断数据。
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if theme_toggle(ui, theme::mode() == theme::Mode::Dark).clicked() {
                            want_toggle = true;
                        }
                        ui.add_space(2.0);
                        if flow_toggle(ui, self.show_activity).clicked() {
                            self.show_activity = !self.show_activity;
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

    /// 右侧活动流：实时展示「信息流到哪了、调了什么工具」。
    fn activity_panel(&mut self, ctx: &egui::Context) {
        let frame = Frame::side_top_panel(&ctx.style())
            .fill(P::panel())
            .inner_margin(Margin::symmetric(20, 18));
        egui::SidePanel::right("aox-activity")
            .resizable(true)
            .default_width(410.0)
            .width_range(300.0..=700.0)
            .frame(frame)
            .show(ctx, |ui| {
                // 把面板内容的宽度钉死在「当前面板宽度」上。
                //
                // 面板宽度并不是独立保存的：egui 每帧把「内容矩形」写回 PanelState，
                // 下一帧再拿它当面板宽度。于是内容只要比面板窄一点点——滚动条预留的
                // 几像素、某一帧 right_to_left 布局没吃满、或一行里漏算的 item_spacing——
                // 宽度就会一帧帧缩水，直到 width_range 的下限；反过来则会一路涨到上限。
                // 表现都是同一句话：「拖完自己弹回去」。
                // 钉死之后，面板宽度只由「用户拖动 + default_width」决定。
                let pw = ui.available_width();
                ui.set_min_width(pw);
                ui.set_max_width(pw);
                // 面板标题
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), Sense::hover());
                    let pulsing = self.busy && ((self.spin * 2.0) as i32 % 2 == 0);
                    ui.painter().circle_filled(
                        rect.center(),
                        4.0,
                        if self.busy {
                            if pulsing { P::ink() } else { P::ink_soft() }
                        } else {
                            P::text_faint()
                        },
                    );
                    ui.add_space(8.0);
                    ui.label(RichText::new("活动流").color(P::text()).size(15.5).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if flat_button(ui, "清空").clicked() {
                            self.acts.clear();
                        }
                    });
                });
                ui.add_space(14.0);

                // 当前状态
                let (status, elapsed) = match self.live.as_ref() {
                    Some(l) => (l.status.clone(), l.started.elapsed().as_secs_f32()),
                    None => (self.status.clone(), 0.0),
                };
                Frame::NONE
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(12))
                    .stroke(Stroke::new(1.0, P::border()))
                    .inner_margin(Margin::symmetric(16, 13))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(if status.is_empty() { "就绪" } else { &status })
                                    .color(if self.busy { P::ink() } else { P::text_dim() })
                                    .size(14.5),
                            );
                            if self.busy {
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(format!("{elapsed:.1}s"))
                                        .color(P::text_faint())
                                        .size(13.0),
                                );
                            }
                        });
                    });
                ui.add_space(14.0);

                let out = ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(self.follow_acts)
                    .show(ui, |ui| {
                        // 锁死内容宽度 = 面板宽度。否则内部卡片会按「文本理想宽度」撑开，
                        // 超出部分被面板裁掉——这就是之前卡片看起来被切的原因。
                        let w = ui.available_width();
                        ui.set_width(w);
                        ui.set_max_width(w);
                        // 同上：活动流里的标题/正文也都在水平布局内，统一强制换行
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                        if self.acts.is_empty() {
                            ui.add_space(16.0);
                            ui.label(RichText::new("等待指令…").color(P::text_faint()).size(14.5));
                        }
                        for a in &self.acts {
                            act_row(ui, a, w);
                        }
                    });
                // 「贴近底部才自动跟随」：用户往上翻历史时不会被新事件拽回去
                let max = (out.content_size.y - out.inner_rect.height()).max(0.0);
                self.follow_acts = out.state.offset.y >= max - 32.0;
            });
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
                        // 只要还没开始真正的对话，就先展示品牌区（此时 items 里可能已有加载/错误提示）
                        let started = self
                            .items
                            .iter()
                            .any(|i| matches!(i, Item::User(_) | Item::Assistant(_)));
                        if !started && self.live.is_none() {
                            self.empty_state(ui);
                        }
                        for i in 0..self.items.len() {
                            match &mut self.items[i] {
                                Item::User(t) => user_bubble(ui, t, inner * 0.80),
                                Item::Assistant(t) => assistant_block(ui, t, inner),
                                Item::Process(p) => process_block(ui, p, inner),
                                Item::Note(k, t) => note_row(ui, *k, t),
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
            // 品牌字：用衬线体，配一条细分隔线，像书页的刊头
            let (rect, _) = ui.allocate_exact_size(Vec2::new(320.0, 62.0), Sense::hover());
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "AOX",
                theme::serif_font(46.0),
                P::text(),
            );
            ui.add_space(4.0);
            let (rule, _) = ui.allocate_exact_size(Vec2::new(230.0, 1.0), Sense::hover());
            ui.painter().hline(
                (rule.center().x - 115.0)..=(rule.center().x + 115.0),
                rule.center().y,
                Stroke::new(1.0, P::border_strong()),
            );
            ui.add_space(16.0);
            ui.label(
                RichText::new("自我增长的 AI 编程代理 · 初始无工具，自己造插件")
                    .color(P::text_dim())
                    .size(15.0),
            );
            ui.add_space(40.0);

            let examples = [
                "看看现在有哪些插件和工具",
                "写一个插件，统计当前目录下各类文件的数量",
                "写一个插件读取一个网页的标题并总结内容",
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
                    .corner_radius(CornerRadius::same(theme::CARD_RADIUS))
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
                .corner_radius(CornerRadius::same(theme::CARD_RADIUS))
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
                    .corner_radius(CornerRadius::same(theme::CARD_RADIUS))
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
                self.push_act(
                    ActKind::Info,
                    "授权结果",
                    match d {
                        AuthDecision::Allow => "允许一次",
                        AuthDecision::AllowSession => "本会话始终允许",
                        AuthDecision::Deny => "拒绝",
                    },
                    false,
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
    app.boot = Some(BootInfo {
        config_path: app.config_path.clone(),
        model: app.model.clone(),
        provider_ready: true,
        note: None,
    });

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
        elapsed: 7.4,
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

    // 活动流：按时间顺序铺一遍典型信息流
    let rows: &[(ActKind, f32, &str, &str, bool)] = &[
        (ActKind::Turn, 0.0, "新指令", "做一个插件，统计当前目录下各类文件的数量", true),
        (ActKind::Think, 0.3, "思考过程", "需要先确认宿主已有能力，再决定是否造插件。", true),
        (ActKind::Call, 1.1, "调用 list_plugins", "{}", true),
        (ActKind::Ok, 1.4, "完成 list_plugins", "已加载插件: probe_screen, fs", true),
        (ActKind::Call, 2.0, "调用 create_plugin", r#"{"name":"filestat","language":"python"}"#, true),
        (ActKind::Info, 4.6, "授权结果", "允许一次", false),
        (ActKind::Ok, 6.2, "完成 create_plugin", "插件 filestat 已创建、注册并持久化（1 个工具）", true),
        (ActKind::Call, 6.5, "调用 filestat_count", r#"{"path":".","ext":null}"#, true),
        (ActKind::Run, 6.5, "执行中 filestat_count", "超时 120s", false),
        (ActKind::Ok, 7.3, "完成 filestat_count", ".rs 14 · .py 9 · .toml 4 · .md 2 · .lock 1", true),
        (ActKind::Turn, 7.4, "任务结束", "耗时 7.4s", false),
    ];
    for (kind, at, title, body, detail) in rows {
        app.acts.push_back(Act {
            kind: *kind,
            at: *at,
            title: (*title).to_string(),
            body: (*body).to_string(),
            live: false,
            detail: *detail,
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.fit_window(ctx);
        self.pump();

        // 原生标题栏：底色与界面完全一致（DWM 属性），标题文字带上当前模型。
        // 放在 top_bar 之后，这样刚点的明暗切换同一帧就能反映到标题栏上。
        if self.title_model != self.model {
            self.title_model = self.model.clone();
            let title = if self.model.is_empty() {
                "AOX".to_string()
            } else {
                format!("AOX · {}", self.model)
            };
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        }

        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        if self.busy || self.waiting_auth || self.live.is_some() {
            self.spin += dt;
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        self.top_bar(ctx);

        if self.native_theme != Some(theme::mode()) {
            self.native_theme = Some(theme::mode());
            theme::sync_native_theme(ctx);
            theme::sync_caption(frame);
        }

        if self.show_activity {
            self.activity_panel(ctx);
        }
        let frame = Frame::NONE.fill(P::bg());
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let column_rect = ui.max_rect();
            // 对话区铺满整个中央面板，不再给输入框让出底栏
            self.transcript(ui);

            // 输入框改为一枚悬浮卡片（Foreground 层），直接压在对话内容之上。
            // 之所以不放在 TopBottomPanel::bottom 里：那会永久占掉一条底栏，
            // 面板底色与背景同色，看上去就是底部空出一片区域。
            // constrain_to 在这里有两个作用：把裁切区域限制在本栏内（不会盖到右侧活动流），
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
fn flat_button(ui: &mut Ui, text: &str) -> egui::Response {
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
fn primary_button(ui: &mut Ui, text: &str) -> egui::Response {
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
                            font_id: theme::body_font(theme::BODY),
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
        .corner_radius(CornerRadius::same(theme::CARD_RADIUS))
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
    let label = if p.aborted {
        format!("任务已中断 · {n} 步 · {:.1}s", p.elapsed)
    } else if n == 0 {
        format!("处理过程 · {:.1}s", p.elapsed)
    } else {
        format!("{n} 步工具调用 · {:.1}s", p.elapsed)
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
            .corner_radius(CornerRadius::same(theme::CARD_RADIUS))
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

fn act_row(ui: &mut Ui, a: &Act, panel_w: f32) {
    // 时间轴：一条淡竖线 + 节点圆点，比单纯列点更像「进度流」
    let rail_w = 20.0;
    // 必须把 horizontal 布局的 item_spacing 也扣掉。
    //
    // 面板宽度是从「上一帧的内容矩形」反推出来的（egui 的 PanelState.rect = 内容矩形），
    // 所以内容一旦比面板宽，面板下一帧就会跟着变宽——是个自激的反馈环：
    // 每帧涨「溢出量」，几帧内就顶到 width_range 上限，此后无论怎么拖都会被弹回最大宽度。
    // 这里漏掉一次 item_spacing（10px）就正好触发它。
    let gap = ui.spacing().item_spacing.x;
    let body_w = (panel_w - rail_w - gap - 2.0).max(120.0);

    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(rail_w, 22.0), Sense::hover());
        let color = a.kind.color();
        let c = rect.center();
        let painter = ui.painter();
        // 竖线贯穿整段：上下都多伸出去一点，让相邻节点之间连成一条线
        painter.line_segment(
            [
                Pos2::new(c.x, rect.top() - 18.0),
                Pos2::new(c.x, rect.bottom() + 18.0),
            ],
            Stroke::new(1.0, P::border()),
        );
        if a.live {
            let pulse = ((a.at * 2.0) as i32) % 2 == 0;
            painter.circle_filled(c, 5.0, if pulse { color } else { P::text_faint() });
        } else if a.kind.is_failure() {
            // 失败：实心大点 + 叉，单色下也一眼可辨
            painter.circle_filled(c, 6.0, color);
            let s = 2.6;
            let stroke = Stroke::new(1.6, P::panel());
            painter.line_segment(
                [Pos2::new(c.x - s, c.y - s), Pos2::new(c.x + s, c.y + s)],
                stroke,
            );
            painter.line_segment(
                [Pos2::new(c.x - s, c.y + s), Pos2::new(c.x + s, c.y - s)],
                stroke,
            );
        } else {
            painter.circle_filled(c, 3.6, color);
            // 用面板底色在内圈开个孔，形成圆环，区分「进行中/已完成」
            painter.circle_filled(c, 1.6, P::panel());
        }

        ui.vertical(|ui| {
            ui.set_width(body_w);
            ui.set_max_width(body_w);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&a.title).color(color).size(14.5));
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("+{:.1}s", a.at))
                        .color(P::text_faint())
                        .size(12.0),
                );
            });
            if !a.body.trim().is_empty() {
                let body = if a.detail {
                    let b = clamp_str(&a.body, 900);
                    if a.kind.mono() { b } else { collapse_ws(&b) }
                } else {
                    collapse_ws(&clamp_str(&a.body, 220))
                };
                ui.add_space(5.0);
                // 参数/结果放进一张小卡片里，和标题分开，长内容也不会顶出面板
                let card_w = (body_w - 26.0).max(100.0);
                Frame::NONE
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(10))
                    .stroke(Stroke::new(1.0, P::border()))
                    .inner_margin(Margin::symmetric(13, 10))
                    .show(ui, |ui| {
                        ui.set_width(card_w);
                        ui.set_max_width(card_w);
                        let mut job = egui::text::LayoutJob::default();
                        job.wrap.max_width = card_w;
                        job.wrap.break_anywhere = a.kind.mono();
                        job.append(
                            &body,
                            0.0,
                            egui::TextFormat {
                                font_id: if a.kind.mono() {
                                    theme::mono_font(13.0)
                                } else {
                                    theme::body_font(13.5)
                                },
                                color: P::text_dim(),
                                ..Default::default()
                            },
                        );
                        ui.add(Label::new(job).selectable(true).wrap_mode(egui::TextWrapMode::Wrap));
                    });
            }
        });
    });
    ui.add_space(14.0);
}

// ─────────────────────────── 文本工具 ───────────────────────────

fn one_line(s: &str, max: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    clamp_str(&joined, max)
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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

fn clamp_tail(s: &mut String, max: usize) {
    let len = s.chars().count();
    if len <= max {
        return;
    }
    let skip = len - max;
    *s = s.chars().skip(skip).collect();
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
