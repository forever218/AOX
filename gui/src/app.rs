//! GUI 主界面状态与绘制。
//!
//! 布局（参考主流 AI 编程软件）：
//! ```text
//! ┌───────────────────────────────────────────────┬──────────────┐
//! │ 顶栏：AOX · 状态 · 模型/工具/插件 · [活动]     │  活动流面板   │
//! ├───────────────────────────────────────────────┤ （可折叠）    │
//! │ 对话流：用户气泡 / 助手 Markdown / 折叠的过程  │  状态 + 实时  │
//! │                                                │  思考 + 工具  │
//! ├───────────────────────────────────────────────┤  调用记录     │
//! │ 输入区：多行输入 + 发送 / 停止                 │              │
//! └───────────────────────────────────────────────┴──────────────┘
//! ```

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{
    Align, Align2, Color32, CornerRadius, Frame, Label, Layout, Margin, Pos2, RichText,
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
    fn color(self) -> Color32 {
        match self {
            NoteKind::Info => P::TEXT_DIM,
            NoteKind::Warn => P::WARN,
            NoteKind::Error => P::ERR,
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
    fn color(self) -> Color32 {
        match self {
            ActKind::Turn => P::ACCENT,
            ActKind::Think => P::TEXT_FAINT,
            ActKind::Call => P::WARN,
            ActKind::Run => P::TEXT_FAINT,
            ActKind::Ok => P::OK,
            ActKind::Err => P::ERR,
            ActKind::Info => P::TEXT_DIM,
            ActKind::Warn => P::WARN,
            ActKind::Error => P::ERR,
        }
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
    root: String,
    config_path: String,
    tools: Vec<String>,
    plugins: Vec<String>,

    show_activity: bool,
    /// 演示模式（--demo）：不连 LLM，只展示界面与事件流的排布
    demo: bool,
    /// 对话流是否自动跟随到底部
    follow: bool,
    /// 本条消息强制滚到底（用户刚发指令时）
    force_follow: bool,
    /// 活动流是否自动跟随
    follow_acts: bool,
    log_error: Option<String>,
    /// `--prompt` 传入的首条指令：等模型就绪后自动发送
    pending_prompt: Option<String>,
    /// 授权弹窗出现的时刻（用于快捷键去抖，避免弹窗刚出现就被误触）
    auth_shown_at: Instant,
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
            root: String::new(),
            config_path: String::new(),
            tools: Vec::new(),
            plugins: Vec::new(),
            show_activity: true,
            follow: true,
            force_follow: true,
            follow_acts: true,
            log_error: None,
            pending_prompt: if demo { None } else { prompt },
            auth_shown_at: Instant::now(),
        };
        if demo {
            seed_demo(&mut app);
        }
        app
    }

    fn now(&self) -> f32 {
        self.started.elapsed().as_secs_f32()
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
                    self.root = b.root.clone();
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
        let plugins_changed = s.plugins != self.plugins;
        self.tools = s.tools;
        self.plugins = s.plugins;
        if plugins_changed && !self.plugins.is_empty() {
            }
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
            .fill(P::PANEL)
            .inner_margin(Margin::symmetric(14, 0));
        egui::TopBottomPanel::top("aox-top")
            .exact_height(44.0)
            .frame(frame)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // 品牌
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(9.0, 9.0), Sense::hover());
                    ui.painter().circle_filled(
                        rect.center(),
                        4.0,
                        if self.busy { P::ACCENT } else { P::ACCENT_DIM },
                    );
                    ui.add_space(7.0);
                    ui.label(RichText::new("AOX").color(P::TEXT).size(15.0).strong());
                    ui.add_space(9.0);
                    ui.label(
                        RichText::new(if self.status.is_empty() { "就绪" } else { &self.status })
                            .color(P::TEXT_FAINT)
                            .size(12.0),
                    );
                    if self.busy {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(format!("{:.1}s", self.live.as_ref().map_or(0.0, |l| {
                                l.started.elapsed().as_secs_f32()
                            })))
                            .color(P::TEXT_FAINT)
                            .size(11.5),
                        );
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if toggle_chip(ui, "活动流", self.show_activity).clicked() {
                            self.show_activity = !self.show_activity;
                        }
                        if self.demo {
                            ui.add_space(6.0);
                            chip(ui, "演示", P::WARN);
                        }
                        ui.add_space(6.0);
                        if !self.plugins.is_empty() {
                            chip(ui, &format!("插件 {}", self.plugins.len()), P::TEXT_DIM);
                        }
                        chip(ui, &format!("工具 {}", self.tools.len()), P::TEXT_DIM);
                        if !self.model.is_empty() {
                            chip(ui, &self.model, P::ACCENT_DIM);
                        }
                    });
                });
            });
    }

    /// 右侧活动流：实时展示「信息流到哪了、调了什么工具」。
    fn activity_panel(&mut self, ctx: &egui::Context) {
        let frame = Frame::side_top_panel(&ctx.style())
            .fill(P::PANEL)
            .inner_margin(Margin::symmetric(12, 10));
        egui::SidePanel::right("aox-activity")
            .resizable(true)
            .default_width(340.0)
            .width_range(240.0..=560.0)
            .frame(frame)
            .show(ctx, |ui| {
                // 面板标题
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 8.0), Sense::hover());
                    let pulsing = self.busy
                        && ((self.spin * 2.0) as i32 % 2 == 0);
                    ui.painter().circle_filled(
                        rect.center(),
                        3.5,
                        if self.busy {
                            if pulsing { P::ACCENT } else { P::ACCENT_DIM }
                        } else {
                            P::TEXT_FAINT
                        },
                    );
                    ui.add_space(5.0);
                    ui.label(RichText::new("活动流").color(P::TEXT).size(12.5).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if flat_button(ui, "清空").clicked() {
                            self.acts.clear();
                        }
                    });
                });
                ui.add_space(8.0);

                // 当前状态
                let (status, elapsed) = match self.live.as_ref() {
                    Some(l) => (l.status.clone(), l.started.elapsed().as_secs_f32()),
                    None => (self.status.clone(), 0.0),
                };
                Frame::NONE
                    .fill(P::SURFACE.gamma_multiply(0.75))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(if status.is_empty() { "就绪" } else { &status })
                                    .color(if self.busy { P::ACCENT } else { P::TEXT_DIM })
                                    .size(12.0),
                            );
                            if self.busy {
                                ui.label(
                                    RichText::new(format!("{elapsed:.1}s"))
                                        .color(P::TEXT_FAINT)
                                        .size(11.0),
                                );
                            }
                        });
                    });
                ui.add_space(8.0);

                let out = ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(self.follow_acts)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        if self.acts.is_empty() {
                            ui.add_space(12.0);
                            ui.label(RichText::new("等待指令…").color(P::TEXT_FAINT).size(12.0));
                        }
                        for a in &self.acts {
                            act_row(ui, a);
                        }
                    });
                // 「贴近底部才自动跟随」：用户往上翻历史时不会被新事件拽回去
                let max = (out.content_size.y - out.inner_rect.height()).max(0.0);
                self.follow_acts = out.state.offset.y >= max - 32.0;
            });
    }

    fn transcript(&mut self, ui: &mut Ui) {
        let width = ui.available_width();
        let inner = (width - 64.0).max(240.0);
        let out = ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(self.follow || self.force_follow)
            .show(ui, |ui| {
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    ui.add_space(28.0);
                    ui.vertical(|ui| {
                        ui.set_max_width(inner);
                        ui.set_min_width(inner);
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
                                Item::User(t) => user_bubble(ui, t, inner * 0.78),
                                Item::Assistant(t) => assistant_block(ui, t, inner),
                                Item::Process(p) => process_block(ui, p, inner),
                                Item::Note(k, t) => note_row(ui, *k, t),
                                Item::Error(t) => error_card(ui, t, inner),
                            }
                        }
                        if let Some(l) = &self.live {
                            self.live_block(ui, l, inner);
                        }
                        ui.add_space(14.0);
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
        ui.add_space(64.0);
        ui.vertical_centered(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(220.0, 46.0), Sense::hover());
            ui.painter().text(
                Pos2::new(rect.center().x, rect.center().y),
                Align2::CENTER_CENTER,
                "AOX",
                theme::bold_font(34.0),
                P::TEXT,
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("自我增长的 AI 编程代理 · 初始无工具，自己造插件")
                    .color(P::TEXT_FAINT)
                    .size(12.5),
            );
            ui.add_space(26.0);

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
                ui.add_space(6.0);
            }

            if let Some(err) = &self.log_error {
                ui.add_space(26.0);
                Frame::NONE
                    .fill(P::SURFACE.gamma_multiply(0.7))
                    .corner_radius(CornerRadius::same(10))
                    .stroke(Stroke::new(1.0, P::ERR.gamma_multiply(0.35)))
                    .inner_margin(Margin::symmetric(14, 12))
                    .show(ui, |ui| {
                        ui.set_max_width(560.0);
                        ui.label(RichText::new("模型未就绪").color(P::ERR).size(13.0).strong());
                        ui.add_space(4.0);
                        ui.label(RichText::new(err).color(P::TEXT_DIM).size(12.0));
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(format!("配置文件：{}", self.config_path))
                                .color(P::TEXT_FAINT)
                                .size(11.5),
                        );
                        ui.label(
                            RichText::new("填写 [provider] 的 base_url / api_key 后重新启动。")
                                .color(P::TEXT_FAINT)
                                .size(11.5),
                        );
                    });
            }
        });
    }

    fn live_block(&self, ui: &mut Ui, l: &Live, width: f32) {
        // 进行中的工具调用
        if !l.steps.is_empty() {
            Frame::NONE
                .fill(P::SURFACE.gamma_multiply(0.55))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.set_width(width);
                    for (i, st) in l.steps.iter().enumerate() {
                        step_row(ui, i + 1, st, width - 24.0);
                    }
                });
            ui.add_space(10.0);
        }

        // 等待动画 + 状态
        ui.horizontal(|ui| {
            spinner_dot(ui, self.spin, P::ACCENT);
            ui.add_space(4.0);
            ui.label(
                RichText::new(if l.status.is_empty() { "思考中" } else { &l.status })
                    .color(P::TEXT_DIM)
                    .size(12.5),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("{:.1}s", l.started.elapsed().as_secs_f32()))
                    .color(P::TEXT_FAINT)
                    .size(11.0),
            );
        });

        // 实时思考过程（尾部几行，响应结束即消失）
        if l.reasoning_live && !l.reasoning.is_empty() {
            ui.add_space(8.0);
            let tail = tail_lines(&l.reasoning, 4);
            Frame::NONE
                .fill(P::CODE_BG.gamma_multiply(0.65))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width((width - 20.0).max(120.0));
                    let mut job = egui::text::LayoutJob::default();
                    job.wrap.max_width = (width - 28.0).max(100.0);
                    job.wrap.break_anywhere = true;
                    job.append(
                        &tail,
                        0.0,
                        egui::TextFormat {
                            font_id: theme::body_font(12.0),
                            color: P::TEXT_FAINT,
                            ..Default::default()
                        },
                    );
                    ui.add(Label::new(job));
                });
        }
        ui.add_space(10.0);
    }

    fn composer(&mut self, ui: &mut Ui) {
        let width = ui.available_width();
        let blocked = self.demo || self.boot.as_ref().is_some_and(|b| !b.provider_ready);
        let can_send = !self.busy && !blocked && !self.waiting_auth && !self.input.trim().is_empty();

        Frame::NONE
            .fill(P::SURFACE)
            .corner_radius(CornerRadius::same(12))
            .stroke(Stroke::new(1.0, if self.busy { P::BORDER } else { P::BORDER }))
            .inner_margin(Margin::symmetric(12, 9))
            .show(ui, |ui| {
                ui.set_width(width - 58.0);
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
                        "给 AOX 下达指令…（Enter 发送 · Shift+Enter 换行）"
                    };
                    let text_edit = TextEdit::multiline(&mut self.input)
                        .desired_rows(1)
                        .desired_width(f32::INFINITY)
                        .frame(false)
                        .hint_text(RichText::new(hint).color(P::TEXT_FAINT).size(13.5));
                    let resp = ui.add(text_edit);
                    if self.focus_input {
                        resp.request_focus();
                        self.focus_input = false;
                    }

                    let ime = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Ime(_))));
                    let enter = ui.input(|i| {
                        i.key_pressed(egui::Key::Enter) && !i.modifiers.shift && !i.modifiers.alt
                    });
                    if resp.has_focus() && enter && !ime {
                        if can_send {
                            self.send();
                        }
                    }

                    ui.add_space(4.0);
                    if self.busy {
                        if stop_button(ui).clicked() {
                            self.stop();
                        }
                    } else if send_button(ui, can_send).clicked() && can_send {
                        self.send();
                    }
                });
            });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if self.root.is_empty() { String::new() } else { self.root.clone() })
                    .color(P::TEXT_FAINT)
                    .size(10.5),
            );
        });
    }

    fn auth_overlay(&mut self, ctx: &egui::Context) {
        let Some(card) = self.auth.take() else { return };
        let mut decision: Option<AuthDecision> = None;
        egui::Modal::new(egui::Id::new("aox-auth")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.label(
                RichText::new("工具调用需要授权")
                    .color(P::TEXT)
                    .size(14.5)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("模型想调用一个会改变系统自身的元工具：")
                    .color(P::TEXT_FAINT)
                    .size(12.0),
            );
            ui.add_space(8.0);
            ui.label(RichText::new(&card.tool).color(P::ACCENT).size(13.5));
            ui.add_space(8.0);
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = 440.0;
            job.wrap.break_anywhere = true;
            job.append(
                &clamp_str(&card.args, 1600),
                0.0,
                egui::TextFormat {
                    font_id: theme::mono_font(12.0),
                    color: P::TEXT_DIM,
                    ..Default::default()
                },
            );
            Frame::NONE
                .fill(P::CODE_BG)
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
                    .color(P::TEXT_FAINT)
                    .size(10.5),
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
    app.root = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    app.config_path = "config.toml".into();
    app.tools = ["create_plugin", "filestat_count", "list_plugins", "remove_plugin"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    app.plugins = vec!["filestat".to_string()];
    app.status = "就绪".into();
    app.boot = Some(BootInfo {
        root: app.root.clone(),
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

impl eframe::App for App {    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();

        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        if self.busy || self.waiting_auth || self.live.is_some() {
            self.spin += dt;
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        self.top_bar(ctx);
        if self.show_activity {
            self.activity_panel(ctx);
        }
        let frame = Frame::NONE.fill(P::BG);
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            egui::TopBottomPanel::bottom("aox-composer")
                .min_height(70.0)
                .frame(
                    Frame::NONE
                        .fill(P::BG)
                        .inner_margin(Margin::symmetric(24, 12)),
                )
                .show_inside(ui, |ui| self.composer(ui));
            self.transcript(ui);
        });

        self.auth_overlay(ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let c = P::BG;
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

fn chip(ui: &mut Ui, text: &str, color: Color32) {
    Frame::NONE
        .fill(P::SURFACE.gamma_multiply(0.85))
        .corner_radius(CornerRadius::same(20))
        .inner_margin(Margin::symmetric(9, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(color).size(11.0));
        });
}

fn toggle_chip(ui: &mut Ui, text: &str, on: bool) -> egui::Response {
    let fill = if on { P::ACCENT.gamma_multiply(0.16) } else { P::SURFACE };
    let stroke = if on { Stroke::new(1.0, P::ACCENT_DIM) } else { Stroke::new(1.0, P::BORDER) };
    let text_color = if on { P::ACCENT } else { P::TEXT_DIM };
    Frame::NONE
        .fill(fill)
        .corner_radius(CornerRadius::same(20))
        .stroke(stroke)
        .inner_margin(Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(text_color).size(11.5));
        })
        .response
        .interact(Sense::click())
}

fn flat_button(ui: &mut Ui, text: &str) -> egui::Response {
    let label = ui.label(RichText::new(text).color(P::TEXT_DIM).size(11.5));
    let resp = label.interact(Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(
            resp.rect.expand2(Vec2::new(6.0, 3.0)),
            CornerRadius::same(6),
            P::SURFACE_HI,
        );
    }
    resp
}

fn primary_button(ui: &mut Ui, text: &str) -> egui::Response {
    let resp = Frame::NONE
        .fill(P::ACCENT.gamma_multiply(0.18))
        .corner_radius(CornerRadius::same(8))
        .stroke(Stroke::new(1.0, P::ACCENT_DIM))
        .inner_margin(Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(P::ACCENT).size(12.5));
        })
        .response
        .interact(Sense::click());
    resp
}

fn example_chip(ui: &mut Ui, text: &str) -> egui::Response {
    let resp = Frame::NONE
        .fill(P::SURFACE)
        .corner_radius(CornerRadius::same(20))
        .stroke(Stroke::new(1.0, P::BORDER))
        .inner_margin(Margin::symmetric(12, 5))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(P::TEXT_DIM).size(12.0));
        })
        .response
        .interact(Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(
            resp.rect,
            CornerRadius::same(20),
            P::ACCENT.gamma_multiply(0.10),
        );
    }
    resp
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
    let hov = enabled && resp.hovered();
    let fill = if !enabled {
        P::SURFACE
    } else if hov {
        hover
    } else {
        base
    };
    let painter = ui.painter();
    if enabled {
        painter.circle_filled(rect.center(), diameter * 0.5, fill);
    } else {
        painter.circle_stroke(rect.center(), diameter * 0.5, Stroke::new(1.0, P::BORDER));
    }
    let fg = if !enabled {
        P::TEXT_FAINT
    } else if hov {
        P::BG
    } else {
        P::TEXT
    };
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
    icon_button(ui, 30.0, P::ACCENT, P::ACCENT.gamma_multiply(1.15), enabled, paint_arrow_up)
}

fn stop_button(ui: &mut Ui) -> egui::Response {
    icon_button(ui, 30.0, P::ERR.gamma_multiply(0.85), P::ERR, true, paint_square)
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
    let (rect, _) = ui.allocate_exact_size(Vec2::new(12.0, 12.0), Sense::hover());
    let c = rect.center();
    let painter = ui.painter();
    for i in 0..8 {
        let phase = (spin * 7.0) as i32 + i;
        let alpha = if phase % 8 == 0 { 1.0 } else { 0.22 };
        let ang = std::f32::consts::TAU * (i as f32) / 8.0;
        let p = Pos2::new(c.x + ang.cos() * 4.0, c.y + ang.sin() * 4.0);
        painter.circle_filled(p, 1.3, color.gamma_multiply(alpha));
    }
}

fn user_bubble(ui: &mut Ui, text: &str, max_w: f32) {
    ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
        Frame::NONE
            .fill(P::USER_BG)
            .corner_radius(CornerRadius::same(12))
            .stroke(Stroke::new(1.0, P::ACCENT.gamma_multiply(0.20)))
            .inner_margin(Margin::symmetric(13, 10))
            .show(ui, |ui| {
                ui.set_max_width(max_w);
                ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                    let mut job = egui::text::LayoutJob::default();
                    job.wrap.max_width = max_w;
                    job.append(
                        text,
                        0.0,
                        egui::TextFormat {
                            font_id: theme::body_font(13.5),
                            color: P::TEXT,
                            ..Default::default()
                        },
                    );
                    ui.add(Label::new(job).selectable(true));
                });
            });
    });
    ui.add_space(12.0);
}

fn assistant_block(ui: &mut Ui, text: &str, width: f32) {
    markdown::render(ui, text, P::TEXT);
    let _ = width;
}

fn note_row(ui: &mut Ui, kind: NoteKind, text: &str) {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 16.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 2.5, kind.color());
        ui.label(RichText::new(text).color(kind.color()).size(11.5));
    });
    ui.add_space(6.0);
}

fn error_card(ui: &mut Ui, text: &str, width: f32) {
    Frame::NONE
        .fill(P::ERR.gamma_multiply(0.08))
        .corner_radius(CornerRadius::same(10))
        .stroke(Stroke::new(1.0, P::ERR.gamma_multiply(0.30)))
        .inner_margin(Margin::symmetric(13, 11))
        .show(ui, |ui| {
            ui.set_max_width(width);
            ui.label(RichText::new("出错").color(P::ERR).size(12.5).strong());
            ui.add_space(3.0);
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = width - 30.0;
            job.wrap.break_anywhere = true;
            job.append(
                &clamp_str(text, 2000),
                0.0,
                egui::TextFormat {
                    font_id: theme::body_font(12.0),
                    color: P::TEXT_DIM,
                    ..Default::default()
                },
            );
            ui.add(Label::new(job).selectable(true));
        });
    ui.add_space(12.0);
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

    let row_w = width.min(560.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(row_w, 22.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(7), P::SURFACE);
    }
    paint_tri(
        ui.painter(),
        Pos2::new(rect.left() + 8.0, rect.center().y),
        4.0,
        p.expanded,
        P::TEXT_FAINT,
    );
    ui.painter().text(
        Pos2::new(rect.left() + 20.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::body_font(11.5),
        P::TEXT_FAINT,
    );
    if resp.clicked() {
        p.expanded = !p.expanded;
    }
    ui.add_space(4.0);

    if p.expanded {
        Frame::NONE
            .fill(P::SURFACE.gamma_multiply(0.5))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(width - 4.0);
                for (i, st) in p.steps.iter().enumerate() {
                    step_row(ui, i + 1, st, width - 40.0);
                }
                if !p.interim.is_empty() {
                    ui.add_space(4.0);
                    for t in &p.interim {
                        ui.label(
                            RichText::new(clamp_str(t, 400))
                                .color(P::TEXT_FAINT)
                                .size(11.5),
                        );
                    }
                }
                if !p.reasoning.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new("思考过程").color(P::TEXT_FAINT).size(10.5));
                    ui.add_space(2.0);
                    let mut job = egui::text::LayoutJob::default();
                    job.wrap.max_width = width - 40.0;
                    job.wrap.break_anywhere = true;
                    job.append(
                        &clamp_str(&p.reasoning, 3000),
                        0.0,
                        egui::TextFormat {
                            font_id: theme::body_font(11.5),
                            color: P::TEXT_FAINT,
                            ..Default::default()
                        },
                    );
                    ui.add(Label::new(job).selectable(true));
                }
            });
        ui.add_space(10.0);
    }
}

fn step_row(ui: &mut Ui, idx: usize, st: &StepView, width: f32) {
    ui.horizontal_top(|ui| {
        ui.label(
            RichText::new(format!("{idx:>2}"))
                .color(P::TEXT_FAINT)
                .size(11.0)
                .monospace(),
        );
        ui.add_space(2.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 16.0), Sense::hover());
        let color = match &st.result {
            Some((true, _)) => P::ERR,
            Some((false, _)) => P::OK,
            None => P::ACCENT,
        };
        ui.painter().circle_filled(rect.center(), 3.0, color);
        ui.add_space(2.0);
        ui.label(RichText::new(&st.name).color(P::TEXT).size(12.0));
        if let Some(pl) = &st.plugin {
            Frame::NONE
                .fill(P::ACCENT.gamma_multiply(0.12))
                .corner_radius(CornerRadius::same(20))
                .inner_margin(Margin::symmetric(6, 1))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(if pl == "meta" { "meta" } else { pl.as_str() })
                            .color(P::ACCENT_DIM)
                            .size(10.0),
                    );
                });
        }
        if !st.args.is_empty() && st.args != "{}" {
            ui.label(
                RichText::new(one_line(&st.args, 88))
                    .color(P::TEXT_FAINT)
                    .size(11.0)
                    .monospace(),
            );
        }
    });
    if let Some((err, preview)) = &st.result {
        ui.horizontal_top(|ui| {
            ui.add_space(26.0);
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = (width - 30.0).max(80.0);
            job.wrap.break_anywhere = true;
            job.append(
                &one_line(preview, 220),
                0.0,
                egui::TextFormat {
                    font_id: theme::body_font(11.0),
                    color: if *err { P::ERR.gamma_multiply(0.85) } else { P::TEXT_FAINT },
                    ..Default::default()
                },
            );
            ui.add(Label::new(job));
        });
    }
    ui.add_space(5.0);
}

fn act_row(ui: &mut Ui, a: &Act) {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 14.0), Sense::hover());
        let color = a.kind.color();
        if a.live {
            let pulse = ((a.at * 2.0) as i32) % 2 == 0;
            ui.painter().circle_filled(rect.center(), 3.0, if pulse { color } else { P::TEXT_FAINT });
        } else {
            ui.painter().circle_filled(rect.center(), 2.5, color.gamma_multiply(0.85));
        }
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&a.title).color(color).size(11.5));
                ui.label(
                    RichText::new(format!("+{:.1}s", a.at))
                        .color(P::TEXT_FAINT)
                        .size(9.5),
                );
            });
            if !a.body.trim().is_empty() {
                let body = if a.detail {
                    let b = clamp_str(&a.body, 900);
                    if a.kind.mono() { b } else { collapse_ws(&b) }
                } else {
                    collapse_ws(&clamp_str(&a.body, 220))
                };
                let mut job = egui::text::LayoutJob::default();
                job.wrap.max_width = (ui.available_width() - 6.0).max(80.0);
                job.wrap.break_anywhere = a.kind.mono();
                job.append(
                    &body,
                    0.0,
                    egui::TextFormat {
                        font_id: if a.kind.mono() {
                            theme::mono_font(10.5)
                        } else {
                            theme::body_font(11.0)
                        },
                        color: P::TEXT_FAINT,
                        ..Default::default()
                    },
                );
                ui.add(Label::new(job).selectable(true));
            }
        });
    });
    ui.add_space(7.0);
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
