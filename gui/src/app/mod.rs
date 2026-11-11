//! GUI main window state and painting.
//! Layout (modelled on the mainstream AI coding tools).
//! There is no top bar: the old `TopBottomPanel` was removed and the five icons became standalone buttons floating...

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
use crate::brand;
use crate::bridge::{
    BootInfo, BootPaths, FromAgent, RuntimeView, SessionFlags, Snapshot, ToAgent, WorkplaceView,
};
use crate::cache;
use crate::gwin;
use crate::markdown;
use crate::theme::{self, P};
use crate::uistate;
use crate::workplace_ui;
use crate::wsview;

// ─────────────────────────── child modules ───────────────────────────.
mod chrome;
mod composer;
mod demo;
mod drawer;
mod events;
mod model;
mod overlays;
mod plan;
mod settings;
mod store;
mod transcript;
mod widgets;

use chrome::*;
use composer::*;
use demo::*;
use drawer::*;
use model::*;
use overlays::*;
use plan::*;
use transcript::*;
use widgets::*;

// The names other modules already reach in through `crate::app::`.
pub(crate) use widgets::{
    button, button_ex, button_sized, column, danger_button, dialog_footer, ease_out_cubic, elide,
    flat_button, menu_item, middle_elide, pointer_if, primary_button, primary_plus_button,
    paint_button, button_size, Weight,
    BTN_H, MENU_W,
};

// data model

pub struct App {
    tx: UnboundedSender<ToAgent>,
    rx: UnboundedReceiver<FromAgent>,
    flags: Arc<SessionFlags>,
    driver: Option<std::thread::JoinHandle<()>>,

    /// The single time base for everything the UI times (see [`Clock`]).
    clock: Clock,
    boot: Option<BootInfo>,

    items: Vec<Item>,
    live: Option<Live>,
    /// State for the Remote page (pairing, start/stop, logs); the bridge runs on its own thread.
    remote: crate::remote_ui::RemoteState,

    /// The activity stream's shared state. It lives in its own window (`activity.rs`); the
    /// main window only writes into it.
    act: activity::Shared,
    /// The cache-hit map's shared state — a second independent window (`cache.rs`), fed from the
    /// same two sources as the activity stream: per-turn usage records, and a per-frame time base.
    cache: cache::Shared,
    /// The workspace view's shared state — a third independent window (`wsview.rs`).
    wsview: wsview::Shared,
    auth: Option<AuthCard>,
    auth_reply: Option<oneshot::Sender<AuthDecision>>,
    /// A multiple-choice question waiting on the user (asked via `ask_user`). `None` = none.
    choice: Option<ChoiceCard>,
    /// Contents of the inline input box on the last row of the option slot.
    choice_text: String,
    /// The reply channel. Same pattern as `auth_reply`: while it is `Some` the agent thread is blocked in `choose()`...
    choice_reply: Option<oneshot::Sender<ChoiceReply>>,
    /// Decoded / decoding images. The key is the **local path** (see [`Tex`]).
    tex: HashMap<PathBuf, Tex>,

    /// The first screen's background (one pool for light, one for dark -- see crate::brand).
    brand: brand::Art,

    /// Why the background couldn't be shown, if it couldn't.
    brand_err: Option<String>,

    /// The welcome line on the first screen (see crate::welcome): which sentence, in which colour, typed or not.
    welcome: crate::welcome::Stage,

    input: String,
    /// Attachments in the pending-upload bar (dragged in by the user / attached via `attach_file`).
    pending: Vec<aox::attach::Attachment>,
    /// The pointer is dragging files over the window.
    dragging_files: bool,
    focus_input: bool,
    busy: bool,
    waiting_auth: bool,
    status: String,
    spin: f32,
    /// The current conversation's token ledger, turn by turn.
    usage_log: Vec<UsageRecord>,
    /// Whether the "Σ total tokens" breakdown is unfolded above the input box.
    usage_open: bool,
    /// The model's context window in tokens -- the denominator of the ring in the input box.
    context_window: u64,
    /// When the host is compacting the context right now (the instant the event arrived), so the input box can show...
    compacting: Option<std::time::Instant>,
    /// The ring's numerator right after a compaction (`ContextCompacted::after`).
    ctx_reset: Option<u64>,

    model: String,
    config_path: String,
    plugins: Vec<String>,
    /// Plugin runtime status (one row per language, sent by the bridge).
    runtimes: RuntimeView,

    /// Store instance used for reading history (`aox::store`: another handle onto the same backend the agent thread...
    store: Option<Arc<dyn aox::store::Store>>,
    /// The conversation list (newest first). Refreshed only when the sidebar is opened or a
    /// turn ends.
    history: Vec<aox::history::Summary>,
    /// The id of the conversation we are currently in (`None` for a new conversation that hasn't hit disk yet).
    current_history: Option<String>,
    /// A switch request waiting on the agent's acknowledgement (the list is greyed out
    /// first so double-clicks can't bounce back and forth)
    switching: bool,
    /// The on/off state of the "history" button at the top-left.
    show_history: bool,
    /// Slide-out animation progress: `0` = fully collapsed, `1` = fully expanded.
    hist_t: f32,
    /// The conversation list grouped into workspaces, newest group first.
    hist_groups: Vec<aox::space::Group>,
    /// Workspace groups the user has explicitly unfolded in the history overlay, by aox::space::Group::id.
    hist_open: HashSet<String>,
    /// Workspace groups the user has **explicitly folded** (see [`App::hist_open`]).
    hist_closed: HashSet<String>,
    /// What is typed into the drawer's search field.
    hist_query: String,
    /// Whether the two sets above have been read from disk yet.
    hist_fold_loaded: bool,
    /// The name dialog's in-progress state — "new workspace", "rename workspace" or "rename conversation"; `None`...
    name_dialog: Option<NameDialog>,
    /// A destructive action held back until the user says yes — see Confirm.
    confirm: Option<Confirm>,

    /// Demo mode (--demo): no LLM attached, just shows the layout and the shape of the
    /// event stream
    demo: bool,
    /// Whether the transcript auto-follows to the bottom
    follow: bool,
    /// A lock that holds the transcript at the bottom until it gets there.
    force_follow: bool,
    /// Where the view sat last frame, to notice "the user pulled up" (see [`Self::force_follow`]).
    last_scroll_y: f32,
    /// Where the transcript should be scrolled to on the next frame — a content-space y, i.e. the reply's first line...
    jump: Option<f32>,
    /// How far the conversation rail has slid in: `0.0` = absent, `1.0` = fully out.
    rail_t: f32,
    /// The "add your model" dialog has been offered once this launch.
    setup_asked: bool,
    /// Whether the activity stream auto-follows Whether the settings dialog is open (the top bar's three buttons ...
    settings_open: bool,
    /// Which column is selected in the settings dialog.
    settings_tab: SettingsTab,
    log_error: Option<String>,
    /// The initial instruction passed via `--prompt`: sent automatically once the model is
    /// ready
    pending_prompt: Option<String>,
    /// The moment the authorization dialog appeared (used to debounce the hotkeys, so a
    /// freshly-shown dialog can't be triggered by a stray keypress)
    auth_shown_at: Instant,
    /// First-frame window-size convergence: 0 = not tried yet, >0 = number of frames tried
    fit_tries: u8,
    /// The native title-bar theme last pushed to the OS. `None` means it never has been.
    native_theme: Option<theme::Mode>,

    /// State of the "workplace" button at the bottom-left of the input box (button / pop-up list / confirmation /...
    ws: workplace_ui::WorkplaceUi,
    /// State of the model chip at the bottom-left of the input box, the list it opens, and the.
    mdl: crate::model_ui::ModelUi,
    /// The recovery panel's state and the rows it was last given (see `crate::recovery_ui`).
    rec: crate::recovery_ui::RecoveryUi,

    /// The transient toast currently on screen: how many files it accounts for, and the moment it stops being shown.
    toast: Option<(usize, Instant)>,

    /// The tool calls the model is writing right now (see StreamCall).
    stream_calls: Vec<StreamCall>,

    /// How many rows `live.steps` held when the current request started.
    attempt_mark: Option<usize>,

    /// The latest task checkpoint of this conversation, if the model recorded one.
    task: Option<aox::task::TaskState>,
    /// What the plan card draws (derived from `task`, rebuilt only when a checkpoint arrives).
    plan: Option<PlanView>,
    /// Whether the plan card is expanded. **Collapsed by default** (user request, 2026-10-23): while
    /// work is running the only thing worth the space is "which step, and how far in".
    plan_open: bool,
    /// Expand/collapse position of the plan block: 0 = closed, 1 = fully open.
    plan_t: f32,
    /// `(started, from, to)` while the disclosure is in flight.
    plan_anim: Option<(Instant, f32, f32)>,
    /// Height of the composer excluding the plan block, measured last frame.
    composer_base_h: f32,
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
        // All appearance comes from `[theme]`; light/dark prefers the record of the last
        // manual switch (see theme::apply).
        theme::apply(&cc.egui_ctx, &theme_cfg);
        // The first screen's preferences are read before the window exists: the background switch decides whether...
        let welcome_prefs = crate::welcome::Prefs::load(&aox::paths::app_root());
        let mut brand = brand::Art::new();
        brand.set_enabled(welcome_prefs.background);
        let mut app = Self {
            tx,
            rx,
            flags,
            driver,
            demo,
            clock: Clock::new(),
            boot: None,
            items: Vec::new(),
            live: None,
            remote: crate::remote_ui::RemoteState::default(),
            act: activity::new_shared(),
            cache: cache::new_shared(),
            wsview: wsview::new_shared(),
            auth: None,
            auth_reply: None,
            choice: None,
            choice_text: String::new(),
            choice_reply: None,
            tex: HashMap::new(),
            brand,
            brand_err: None,
            welcome: crate::welcome::Stage::new(welcome_prefs),
            input: String::new(),
            pending: Vec::new(),
            dragging_files: false,
            focus_input: false,
            busy: false,
            waiting_auth: false,
            status: aox::i18n::t("Ready").into(),
            spin: 0.0,
            usage_log: Vec::new(),
            usage_open: false,
            context_window: DEFAULT_CONTEXT_WINDOW,
            compacting: None,
            ctx_reset: None,
            model: String::new(),
            config_path: String::new(),
            plugins: Vec::new(),
            runtimes: RuntimeView::default(),
            store: None,
            history: Vec::new(),
            current_history: None,
            switching: false,
            // Both start folded and are replaced by what `.aox/ui.json` says on the first refresh that knows the app root...
            show_history: false,
            hist_t: 0.0,
            hist_groups: Vec::new(),
            hist_open: HashSet::new(),
            hist_closed: HashSet::new(),
            hist_query: String::new(),
            hist_fold_loaded: false,
            name_dialog: None,
            confirm: None,
            follow: true,
            force_follow: true,
            last_scroll_y: 0.0,
            jump: None,
            rail_t: 0.0,
            setup_asked: false,
            settings_open: false,
            settings_tab: SettingsTab::Extra,
            log_error: None,
            pending_prompt: if demo { None } else { prompt },
            auth_shown_at: Instant::now(),
            fit_tries: 0,
            native_theme: None,
            ws: workplace_ui::WorkplaceUi::new(),
            mdl: crate::model_ui::ModelUi::new(),
            rec: crate::recovery_ui::RecoveryUi::new(),
            toast: None,
            stream_calls: Vec::new(),
            attempt_mark: None,
            task: None,
            plan: None,
            plan_open: false,
            plan_t: 0.0,
            plan_anim: None,
            composer_base_h: COMPOSER_FIRST_FRAME_H,
        };
        if demo {
            seed_demo(&mut app);
        }
        app
    }

    /// Current reading of the activity clock (seconds of work since launch, pauses excluded).
    fn now(&self) -> f32 {
        self.clock.now()
    }

    /// Let the clock run only while a turn is actually working.
    fn sync_clock(&mut self) {
        self.clock.set_running(self.running());
    }

    /// Whether anything is actually progressing: a turn in flight, not parked on a person.
    fn running(&self) -> bool {
        self.busy && !self.waiting_auth && self.choice_reply.is_none()
    }

    /// Turn a [`crate::welcome::Ask`] into a message for the agent thread, which owns every network
    /// call in this program (the UI thread draws; it does not wait on sockets).
    fn request_hello(&self, ask: crate::welcome::Ask) {
        let msg = match ask {
            crate::welcome::Ask::Api(url) => ToAgent::HelloFromApi(url),
            crate::welcome::Ask::Model => ToAgent::HelloFromModel,
        };
        let _ = self.tx.send(msg);
    }

    /// On the first frame, converge the window into the screen and place it explicitly.
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
        // None of this is available before the window really exists; try again next frame
        let (Some(mon), Some(inner), Some(outer)) = (mon, inner, outer) else {
            return;
        };

        // The window decorations (title bar + border) sit outside the client area.
        let chrome = (outer - inner).max(Vec2::new(18.0, 46.0));

        let want = default_window_size(mon, chrome);

        if (inner.x - want.x).abs() > 8.0 || (inner.y - want.y).abs() > 8.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(want));
        }

        // Centre it (outer frame = client area + decorations) so the bottom edge never
        // crosses the taskbar
        let outer_size = want + chrome;
        // The visible height (taskbar excluded) is likewise computed only once, in default_window_size; here we derive...
        let taskbar = (mon.y * 0.06).clamp(36.0, 72.0);
        let usable_h = (mon.y - taskbar).max(1.0);
        let pos = Vec2::new(
            ((mon.x - outer_size.x) * 0.5).max(6.0),
            ((usable_h - outer_size.y) * 0.5).max(6.0),
        );
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::Pos2::new(
            pos.x, pos.y,
        )));

        // Once converged there is nothing left to compute
        self.fit_tries = TRIES;
    }

    /// Record a system event into the activity stream (a short bar on the timeline's "system" row).
    fn note_act(&self, kind: ActKind, title: &str, body: &str) {
        if self.live.is_none() {
            return;
        }
        activity::lock(&self.act).note(kind, title, body, self.now());
    }

    fn plugin_of(&self, tool: &str) -> Option<String> {
        // Convention: tool name = "<plugin>_<feature>"; match the prefix against the list
        // of known plugins
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

    // history

    /// The application root, as the bridge resolved it.
    fn app_root(&self) -> Option<PathBuf> {
        self.boot.as_ref().map(|b| PathBuf::from(&b.paths.root))
    }

    // input area / keyboard

    // painting

}

/// The main window's default size: computed from the monitor size, minus the borders and the taskbar, then clamped...
pub(crate) fn default_window_size(mon: Vec2, chrome: Vec2) -> Vec2 {
    // There is no API for the taskbar height, so take a conservative fraction of the monitor
    // height (about 48 logical points by default)
    let taskbar = (mon.y * 0.06).clamp(36.0, 72.0);
    // A little breathing room from the edges
    let margin = Vec2::new(80.0, 28.0);
    Vec2::new(
        (mon.x - margin.x - chrome.x).min(1380.0).max(1000.0),
        (mon.y - taskbar - margin.y - chrome.y).min(900.0).max(620.0),
    )
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // First statement of the first frame: the appearance install that `App::new` could not do yet (the system theme...
        theme::settle(ctx);
        self.remote.poll();
        // Remote takeover swallows keyboard events but keeps the mouse so the Reclaim control button stays clickable.
        if crate::remote_lock::current().is_some() {
            ctx.input_mut(|i| {
                i.events.retain(|e| {
                    !matches!(
                        e,
                        egui::Event::Key { .. }
                            | egui::Event::Text(_)
                            | egui::Event::Paste(_)
                            | egui::Event::Copy
                            | egui::Event::Cut
                    )
                });
            });
        }

        self.fit_window(ctx);
        // Before `pump`: the clock's run/stop state has to be settled from the *current* flags before this frame's events...
        self.sync_clock();
        self.pump();
        // Drag and drop: anywhere in the window accepts (see `take_dropped`).
        self.take_dropped(ctx);

        // Image decoding: finishing (attaching textures) and queueing (starting background
        // threads). Once per frame, idempotent.
        self.poll_images(ctx);
        // The empty-state illustration gets a push here too: it likewise needs a background thread to decode and likewise...
        self.brand.poll(ctx);

        // The every-frame recheck for "follow system": when the system theme changes, follow it (egui puts the current...
        theme::follow_system(ctx);

        // The first screen's welcome line: enter/leave edges, the grace clock, and the request when one is due.
        let on_first_screen = self.on_empty_state();
        if let Some(ask) = self.welcome.sync(on_first_screen) {
            self.request_hello(ask);
        }
        self.welcome.step(ctx);

        // The native title bar keeps only the window title itself: no icon is set, and the model name is no longer...
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        // `switching` needs a repaint too: the switch acknowledgement comes back from the agent thread asynchronously...
        if self.busy || self.waiting_auth || self.live.is_some() || self.switching {
            self.spin += dt;
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        // Advance the slide-out animation: step `hist_t` toward the toggle state until it gets there.
        let target = if self.show_history { 1.0 } else { 0.0 };
        if self.hist_t != target {
            let step = dt.min(0.05) / HIST_ANIM_SECS;
            self.hist_t = if self.hist_t < target {
                (self.hist_t + step).min(target)
            } else {
                (self.hist_t - step).max(target)
            };
            // During the animation a repaint has to be requested explicitly: an idle window doesn't repaint on its own...
            ctx.request_repaint();
        }

        // The plan block's disclosure clock — the same frame-by-frame stepping as `hist_t`, for the
        // same reason: reversing mid-flight continues from where it is instead of jumping to 0.
        let plan_target = if self.plan_open { 1.0 } else { 0.0 };
        match self.plan_anim {
            // Reversed mid-flight: carry on from wherever it is, never from 0.
            Some((_, _, to)) if to != plan_target => {
                self.plan_anim = Some((Instant::now(), self.plan_t, plan_target));
                ctx.request_repaint();
            }
            Some((started, from, to)) => {
                let k = (started.elapsed().as_secs_f32() / PLAN_ANIM_SECS).clamp(0.0, 1.0);
                self.plan_t = from + (to - from) * ease_out_cubic(k);
                if k >= 1.0 {
                    self.plan_t = to;
                    self.plan_anim = None;
                }
                ctx.request_repaint();
            }
            None if self.plan_t != plan_target => {
                self.plan_anim = Some((Instant::now(), self.plan_t, plan_target));
                ctx.request_repaint();
            }
            None => {}
        }

        // A recovery request is answered on a channel from the agent thread, and an idle window does not repaint...
        if self.rec.is_waiting() {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }

        if self.native_theme != Some(theme::mode()) {
            self.native_theme = Some(theme::mode());
            theme::sync_native_theme(ctx);
            theme::sync_caption(frame);
            // The caption used to hand us rounded corners for free; with it gone the system has to
            // be asked, or the window goes square (and loses its shadow with the corners).
            theme::round_corners(frame);
        }

        // The drawer has to be declared before the central panel: a side panel is what tells the central column how much...
        self.drawer(ctx);
        // The bar is declared after the drawer, so it owns the top strip of what is left of the
        // window -- the drawer's own head is the left end of the same row.
        self.titlebar(ctx);
        // The activity stream lives in its own window: the main window syncs "what is happening right now" over every...
        activity::lock(&self.act).tick(self.now(), &self.status, self.busy);
        // The cache-hit map is fed the same way: one `observe` per finished model call, plus the per-frame time base.
        cache::lock(&self.cache).tick(self.now(), self.running());
        // Hand the child windows this window's native handle, once: they need it to keep this window painting...
        gwin::remember_main_window(frame);
        activity::sync(ctx, &self.act);
        cache::sync(ctx, &self.cache);
        // The workspace view is fed the same way, and for the same reason: the window is a pure function of what...
        wsview::set_root(&self.wsview, self.ws.view().and_then(|v| v.root.clone()));
        wsview::tick(&self.wsview, self.running());
        wsview::sync(ctx, &self.wsview);
        let frame = Frame::NONE.fill(P::bg());
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let column_rect = ui.max_rect();
            // The first screen's background goes down first: the transcript and the floating input card (a Foreground `Area`)...
            self.brand_err = if self.on_empty_state() {
                self.brand.show_background(ui, column_rect)
            } else {
                None
            };
            // One height, two consumers: the composer is positioned from it, and the transcript ends there.
            let plan_h = self.plan_block_height();
            let composer_h = self.composer_base_h + plan_h;
            let composer_top =
                (column_rect.bottom() - COMPOSER_BOTTOM_GAP - composer_h).max(column_rect.top());

            self.transcript(ui, composer_top);

            // The welcome line, on top of everything the transcript drew.
            if self.on_empty_state() && self.log_error.is_none() {
                let stage =
                    Rect::from_min_max(column_rect.min, Pos2::new(column_rect.right(), composer_top));
                self.welcome.draw(ui, stage);
            }

            // The input box became a floating card (a Foreground layer) ending exactly at the boundary the transcript stopped...
            let clip = Rect::from_min_max(
                column_rect.min,
                Pos2::new(column_rect.right(), column_rect.bottom()),
            );
            // The anchor is measured from the window's centre, while the card has to sit on the column's centre ...
            let origin = Pos2::new(column_rect.left(), composer_top);
            egui::Area::new(egui::Id::new("aox-composer"))
                .order(egui::Order::Foreground)
                .movable(false)
                .constrain_to(clip)
                .fixed_pos(origin)
                .show(ctx, |ui| {
                    ui.set_width(column_rect.width());
                    self.composer(ui);
                    // Measured for **next** frame's position: see the note where `top` is computed.
                    let total = ui.min_rect().height();
                    self.composer_base_h = (total - plan_h).max(0.0);
                });

            // The last of the three calls that together bury the eight border hot-zones: this one owns the right edge...
            gwin::resize_handles(ui, ctx, ctx.content_rect());
        });

        // The workplace's three overlays: the three-way pop-up list, the full-access confirmation, and the directory...
        if let Some(mode) = workplace_ui::menu(ctx, &mut self.ws) {
            self.send_ws(mode);
        }
        if let Some(mode) = workplace_ui::confirm(ctx, &mut self.ws) {
            self.send_ws(mode);
        }
        if let Some(mode) = workplace_ui::picker(ctx, &mut self.ws) {
            self.send_ws(mode);
        }
        // The model list, drawn in the same layer for the same reason: it overlaps the input card.
        let model_req = crate::model_ui::menu(ctx, &mut self.mdl);
        self.send_requests(model_req);
        // The add/edit dialog. Drawn here rather than from the settings tab: a modal inside the
        // settings card's scroll area would be clipped by it.
        let dialog_req = crate::model_ui::dialog(ctx, &mut self.mdl);
        self.send_requests(dialog_req);
        // The recovery panel, drawn above the transcript for the same reason the menus above are: it overlaps the input...
        if let Some(req) = self.rec.panel(ctx) {
            self.send_recovery(req);
        }

        // Before the settings dialog, because it belongs to the history panel: the two are never
        // open at once, and a modal drawn later wins any overlap.
        self.name_dialog_overlay(ctx);
        // Sits beside the name dialog: same layer, same "nothing happens until you answer" rule.
        self.confirm_overlay(ctx);
        if let Some(req) = self.rec.confirm(ctx) {
            self.send_recovery(req);
        }
        // The other question the same panel can ask (clear every point).
        if let Some(req) = self.rec.clear_confirm(ctx) {
            self.send_recovery(req);
        }
        self.settings_overlay(ctx);
        self.auth_overlay(ctx);
        // Last, so the toast sits above everything: it reports something that has already happened, and being hidden...
        self.toast_overlay(ctx);

        // The window has no system frame any more, so the 1px seam against the desktop is ours to draw — the same stroke...
        let full = ctx.content_rect();
        gwin::outline_painter(
            &ctx.layer_painter(egui::LayerId::new(
                egui::Order::Debug,
                egui::Id::new("aox-window-frame"),
            )),
            full,
        );
        // Remote takeover overlay drawn last, above all layers; it clears when the bridge's heartbeat expires.
        if let Some(info) = crate::remote_lock::current() {
            crate::remote_lock::takeover(ctx, &info);
            ctx.request_repaint_after(std::time::Duration::from_millis(800));
        }

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
            // Bounded wait: give plugin subprocesses a moment to exit gracefully, but never stall
            // the UI's own exit
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            std::thread::spawn(move || {
                let _ = h.join();
                let _ = done_tx.send(());
            });
            let _ = done_rx.recv_timeout(Duration::from_millis(1500));
        }
    }
}

// widgets

/// The denominator for the ring when the host reports no window at all (it normally reports one: `provider...
const DEFAULT_CONTEXT_WINDOW: u64 = aox::config::DEFAULT_CONTEXT_WINDOW as u64;
