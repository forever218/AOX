//! GUI main window state and painting.
//!
//! Layout (modelled on the mainstream AI coding tools):
//! ```text
//! ┌───────────────────────────────────────────────────────────┐
//! │ [History] [+] Status · Σ tok   [Activity] [Theme] [About] │  ← floating buttons, no top bar
//! │┌─────────────────┐                                        │
//! ││ History (slide) │   user bubble / assistant Markdown     │
//! ││  · this thread  │             ┌────────────────────┐     │
//! ││  · that thread  │             │ Floating input box │     │
//! │└─────────────────┘             └────────────────────┘     │
//! └───────────────────────────────────────────────────────────┘
//! ```
//!
//! **There is no top bar** (user request, 2026-10-04): the old `TopBottomPanel` was removed
//! and the five icons became standalone buttons floating above the transcript, each with a
//! backdrop in the old top bar's colour ([`P::panel`]). The positions are restored per the
//! table below: 20/7 at the top-left, 20 in from the right as well -- **pixel-identical** to
//! what the old top bar's `Margin::symmetric(20, 0)` + 36px hot zone worked out to (button
//! centre y = 7 + 18 = 25 = half of the old bar's 50px height).
//!
//! **The left column is a `SidePanel` again, but it is not the old one**: it is a **resident
//! drawer** that takes part in the layout -- opening it pushes the transcript to the right, closing
//! it gives the width back (see [`App::drawer`]). It is a place you work from, not a pop-up you
//! dismiss, so it has no scrim, no "click outside to close", and its position survives a restart.
//! The same `hist_t` progress value and [`HIST_ANIM_SECS`] drive its width.
//!
//! The activity stream is **not in the main window**: clicking that icon opens it in its own
//! window (see `activity.rs`).
//!
//! There is no separate "input area" row at the bottom: the input box is a card floating
//! above the transcript, and scrolling can carry it all the way under it.

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

// ─────────────────────────── child modules ───────────────────────────
//
// `app` was one 9 000-line file until 2026-10-25; it is now a directory of screens. The rule for the
// split is "one screen (or one layer) per file, behaviour unchanged": every module below is a
// straight move, so a diff against the old file shows blocks leaving and the same blocks arriving,
// never rewritten code. (Checked by the split: 0 code lines changed, 0 comment lines lost.)
//
// Visibility follows from Rust's own rule -- a child module can see its ancestor's private items, so
// **no field of `App` had to be made public** for the screens to touch it. The other direction is not
// free: a method defined in a child is private *to that child*, hence `pub(super)` on everything its
// siblings or `mod.rs` call -- and on the fields of the types in `model` / `plan`, because a sibling
// cannot see another sibling's private fields.
//
// The modules that export *items* are glob-imported here, and every module picks them up in turn
// through `use super::*`. `settings`, `store` and `events` export only `impl App` blocks, so there is
// nothing to import from them -- they are listed for completeness.
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

// ─────────────────────────── data model ───────────────────────────

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
    /// The activity stream's shared state. It lives in its own window (`activity.rs`); the
    /// main window only writes into it.
    act: activity::Shared,
    /// The cache-hit map's shared state — a second independent window (`cache.rs`), fed from the
    /// same two sources as the activity stream: per-turn usage records, and a per-frame time base.
    cache: cache::Shared,
    /// The workspace view's shared state — a third independent window (`wsview.rs`). It is fed the
    /// workspace root every frame and this turn's writes as the tool results arrive; the window
    /// itself owns the tree, the file contents and the diff display.
    wsview: wsview::Shared,
    auth: Option<AuthCard>,
    auth_reply: Option<oneshot::Sender<AuthDecision>>,
    /// A multiple-choice question waiting on the user (asked via `ask_user`). `None` = none.
    choice: Option<ChoiceCard>,
    /// Contents of the **inline input box** on the last row of the option slot.
    ///
    /// Keeping it separate from the main input box `input` is deliberate: they belong to
    /// different things -- this one applies only to the current question and is cleared
    /// once answered. Sharing one would cause two bugs: swallowing a draft the user typed
    /// elsewhere while answering, or leaving that draft stuck in the box afterwards.
    choice_text: String,
    /// The reply channel. **Same pattern as `auth_reply`**: while it is `Some` the agent
    /// thread is blocked in `choose()`, so every path that dismisses the card has to fill
    /// it in -- miss one and the UI looks fine while the model waits forever.
    choice_reply: Option<oneshot::Sender<ChoiceReply>>,
    /// Decoded / decoding images. The key is the **local path** (see [`Tex`]).
    tex: HashMap<PathBuf, Tex>,

    /// The first screen's background (one pool for light, one for dark -- see [`crate::brand`]).
    ///
    /// Stored separately from `tex`: `tex` is keyed by path and driven by the conversation,
    /// whereas this is **two pools of artwork** unrelated to it. Mixing them in would only blur
    /// what the map is keyed on.
    brand: brand::Art,

    /// Why the background couldn't be shown, if it couldn't.
    ///
    /// The image is painted from inside `App::update` (it has to be laid down **before** the
    /// transcript, so it sits behind the content), but the place with room for a line of text is
    /// `empty_state`, which runs later -- so the reason is carried across in one field rather than
    /// plumbed through `transcript` as a parameter.
    brand_err: Option<String>,

    input: String,
    /// Attachments in the pending-upload bar (dragged in by the user / attached via
    /// `attach_file`).
    ///
    /// What is stored is [`aox::attach::Attachment`], **not the bytes**: a 30 MB screenshot
    /// can sit in that bar for ages, and UI state shouldn't carry it the whole time -- the
    /// content is read only at the moment it is actually sent (inside
    /// `Message::user_with_files`).
    pending: Vec<aox::attach::Attachment>,
    /// The pointer is dragging files over the window. Used only to brighten the input
    /// card's border by one step -- the user needs to know "where it will land if I let
    /// go", otherwise letting go at the top-right corner works too and reads as a mistake.
    dragging_files: bool,
    focus_input: bool,
    busy: bool,
    waiting_auth: bool,
    status: String,
    spin: f32,
    /// The current conversation's token ledger, **turn by turn**.
    ///
    /// Read back from the file when an old conversation is opened (`FromAgent::UsageLog`),
    /// then appended to at the end of each turn. If it is empty, or has no numbers at all
    /// (the endpoint returns no usage), the whole block is hidden -- better nothing than a
    /// row of zeros pretending to be statistics.
    usage_log: Vec<UsageRecord>,
    /// Whether the "Σ total tokens" breakdown is unfolded above the input box.
    ///
    /// A **click** toggle (the chip sits in the input card's bottom-right corner, where a
    /// hover-opened panel would fire every time the pointer crossed the corner on its way to the
    /// send button). It stays open until it is clicked again or something else is clicked.
    usage_open: bool,
    /// The model's context window in tokens -- the denominator of the ring in the input box.
    ///
    /// Comes from `[provider] context_window` (which now has a default, see
    /// [`aox::config::DEFAULT_CONTEXT_WINDOW`]); the fallback here only covers a host that reports 0.
    /// See [`App::context_used`] for the numerator.
    context_window: u64,
    /// When the host is compacting the context right now (the instant the event arrived), so the
    /// input box can show "compacting…" with an animation instead of a gauge that is pinned at full.
    ///
    /// A `std::time::Instant` rather than `self.clock`: the animation has to keep moving even in
    /// `--demo`, where the simulated clock only advances while something is "running".
    compacting: Option<std::time::Instant>,
    /// The ring's numerator right after a compaction (`ContextCompacted::after`).
    ///
    /// The real figure only comes back with the **next** request's usage, which is a whole model
    /// round trip away — and that round trip is exactly what the user is watching for, so leaving the
    /// gauge sitting at "full" (or red) for those seconds reads as "it didn't work". Cleared as soon
    /// as a real number arrives (a turn's `Usage`, or a new turn starting).
    ctx_reset: Option<u64>,

    model: String,
    config_path: String,
    plugins: Vec<String>,
    /// Plugin runtime status (one row per language, sent by the bridge).
    /// It lives in the "About" window -- this kind of "one glance is enough" diagnostic
    /// data shouldn't take up the main UI.
    runtimes: RuntimeView,

    /// Store instance used for reading history (`aox::store`: another handle onto the same
    /// backend the agent thread uses).
    ///
    /// The GUI reads from disk directly instead of going through the agent thread --
    /// reading history is pure file I/O with no concurrency issues, so there is no reason
    /// to open another channel for it. `None` means "not finished starting up", and nothing
    /// is read then.
    store: Option<Arc<dyn aox::store::Store>>,
    /// The conversation list (newest first). Refreshed only when the sidebar is opened or a
    /// turn ends.
    history: Vec<aox::history::Summary>,
    /// The id of the conversation we are currently in (`None` for a new conversation that
    /// hasn't hit disk yet).
    ///
    /// The sidebar uses it to mark "current" -- the sidebar **is a conversation switcher**:
    /// click one and you carry on there, so this naturally follows.
    current_history: Option<String>,
    /// A switch request waiting on the agent's acknowledgement (the list is greyed out
    /// first so double-clicks can't bounce back and forth)
    switching: bool,
    /// The on/off state of the "history" button at the top-left. It is the drawer's
    /// target state.
    ///
    /// Persisted since 2026-10-25 (`.aox/ui.json`, see [`uistate::FoldState::drawer`]): a launch
    /// starts with the drawer wherever the last session left it. Before that it was always folded,
    /// and every session began with the same click to open it again.
    show_history: bool,
    /// Slide-out animation progress: `0` = fully collapsed, `1` = fully expanded.
    ///
    /// `App::update` steps it toward `show_history` every frame, and `toolbar` / `drawer` both
    /// derive their geometry from it. **Do not use the boolean directly**: that gives you only
    /// "there" and "not there", with nowhere to stand for the two-hundred-odd milliseconds in
    /// between, and the push degrades into a jump cut.
    hist_t: f32,
    /// The conversation list **grouped into workspaces**, newest group first.
    ///
    /// Built once per [`App::refresh_history`] and read every frame the drawer is open: grouping
    /// walks the whole flat list, and doing that per frame would re-sort and re-allocate eighty
    /// entries sixty times a second for a list that only changes when a turn ends.
    /// Empty until the first refresh (when `history` is empty too, so nothing draws it).
    hist_groups: Vec<aox::space::Group>,
    /// Workspace groups the user has **explicitly unfolded** in the history overlay, by
    /// [`aox::space::Group::id`].
    ///
    /// Two sets rather than one "collapsed" flag, because the **default** is the load-bearing part:
    /// a group nobody has touched is drawn open iff it holds the conversation being chatted in, so
    /// opening History shows you where you are without a click while the other workspaces stay
    /// folded and the panel stays short. A click writes to one of the two sets, and an explicit
    /// choice always outranks the default.
    hist_open: HashSet<String>,
    /// Workspace groups the user has **explicitly folded** (see [`App::hist_open`]).
    hist_closed: HashSet<String>,
    /// What is typed into the drawer's search field.
    ///
    /// Kept on `App` rather than in egui memory: the field is rebuilt from scratch every frame (the
    /// drawer has no persistent widget state), and a query that lived in a temporary would be wiped
    /// by the next frame's `TextEdit`. It is **not** persisted either -- a search is a momentary
    /// question, and coming back to a list that is silently filtered by something typed yesterday
    /// reads as "my conversations are gone".
    hist_query: String,
    /// Whether the two sets above have been read from disk yet.
    ///
    /// Loaded **once**, lazily, on the first refresh that knows the app root — not on every refresh:
    /// re-reading would overwrite the in-memory sets (the ones the user is toggling) with whatever was
    /// last written, so any choice not yet saved would be silently reverted. The flag keeps "load" a
    /// one-shot inside a function that already has `app_root` in hand, so both the real boot path and
    /// the `--demo` constructor get it for free.
    hist_fold_loaded: bool,
    /// The name dialog's in-progress state — "new workspace", "rename workspace" or "rename
    /// conversation"; `None` while it is closed.
    ///
    /// One `Option` rather than loose fields on `App`: "a typed name with no dialog" (or an error
    /// message with no name) is not a state this screen can be in, and splitting it would mean
    /// remembering to reset both on every close path. The mode lives **inside** the struct rather
    /// than in a second `Option` beside it, so "open, but for what?" is answered in one place.
    name_dialog: Option<NameDialog>,
    /// A destructive action held back until the user says yes — see [`Confirm`].
    ///
    /// `None` while no question is pending. The **action itself** is in here (the path, the root),
    /// not just a flag: the tree can be refreshed, re-sorted and re-folded between the click and the
    /// answer, and a confirmation must carry out the thing its sentence named.
    confirm: Option<Confirm>,

    /// Demo mode (--demo): no LLM attached, just shows the layout and the shape of the
    /// event stream
    demo: bool,
    /// Whether the transcript auto-follows to the bottom
    follow: bool,
    /// A **lock** that holds the transcript at the bottom until it gets there.
    ///
    /// Set when the view must end up at the end no matter where it was: the user just sent a
    /// message, a conversation was switched, the model asked something. It is **not** cleared on
    /// the next frame — the scroll is animated and the content keeps growing while it runs (see
    /// `App::transcript`), so a one-frame flag would be released half-way up. It clears itself once
    /// the view is actually at the bottom, or as soon as the user pulls up, whichever comes first.
    force_follow: bool,
    /// Where the view sat last frame, to notice "the user pulled up" (see [`Self::force_follow`]).
    last_scroll_y: f32,
    /// The "add your model" dialog has been offered once this launch.
    ///
    /// A second `Boot` carrying `first_run` (there are two on the not-configured path: one before the
    /// save and one after it) must not open a second dialog. The bridge already spends the flag
    /// before the second one; this is the local guarantee, so that the dialog cannot be stacked even
    /// if the host starts sending it again.
    setup_asked: bool,
    /// Whether the activity stream auto-follows
    /// Whether the settings dialog is open (the top bar's three buttons -- activity stream /
    /// theme / about -- have all been folded into it).
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
    ///
    /// A viewport command issued while the window is being created can be dropped before
    /// the front-end event loop comes up, so we record it here to guarantee it really is
    /// delivered at least once from `update`.
    native_theme: Option<theme::Mode>,

    /// State of the "workplace" button at the bottom-left of the input box (button /
    /// pop-up list / confirmation / directory picker).
    ///
    /// The state as a whole lives inside `workplace_ui`; here we hold only a handle --
    /// otherwise four booleans would be scattered over `App`, each of which would still
    /// have to remember the mutual exclusions (see the "open one, close two" bit in
    /// `workplace_ui::button`).
    ws: workplace_ui::WorkplaceUi,
    /// State of the model chip at the bottom-left of the input box, the list it opens, and the
    /// Settings → Model form (see `crate::model_ui`).
    ///
    /// Same reasoning as `ws`: one handle rather than half a dozen loose booleans and strings, and the
    /// address book lives with the UI that owns it.
    mdl: crate::model_ui::ModelUi,
    /// The recovery panel's state and the rows it was last given (see `crate::recovery_ui`).
    rec: crate::recovery_ui::RecoveryUi,

    /// The transient toast currently on screen: how many files it accounts for, and the moment it
    /// stops being shown.
    ///
    /// Timed on the **wall clock** (`Instant`), deliberately not on [`Clock`]: the activity clock
    /// stops whenever the program is idle, and the moment a toast appears the program usually *is*
    /// idle — a toast measured on it would sit there until the user happened to do something.
    ///
    /// A **count**, not a finished sentence: a `remove` call takes one path, so deleting two files
    /// arrives as two events — and the count is what lets the second one merge into the toast
    /// already on screen instead of overwriting it. The sentence is built at draw time via
    /// [`aox::recycle::notice`].
    toast: Option<(usize, Instant)>,

    /// The tool calls the model is writing **right now** (see [`StreamCall`]).
    ///
    /// Cleared at every turn boundary. A call leaves this list the moment its `ToolCall` event
    /// arrives and the row becomes an ordinary one; anything still here when a turn ends never ran
    /// and its row is dropped.
    stream_calls: Vec<StreamCall>,

    /// How many rows `live.steps` held when the **current request** started.
    ///
    /// A retried request (`FromAgent::Attempt` with `n >= 1`) has to roll its own rows back: the
    /// abandoned attempt may already have opened tool rows from streamed argument fragments, and
    /// keeping them would leave the transcript showing work that never happened.
    attempt_mark: Option<usize>,

    /// The latest task checkpoint of this conversation, if the model recorded one.
    ///
    /// Until 2026-10-23 nothing outside the model's context ever saw this: a checkpoint went into the
    /// conversation file and into the prompt, and the interface showed nothing. What is worth a line
    /// is the **plan** — "4 of 6 done, next is run the tests" is the one reading that says whether a
    /// long task is actually moving.
    task: Option<aox::task::TaskState>,
    /// What the plan card draws (derived from `task`, rebuilt only when a checkpoint arrives).
    plan: Option<PlanView>,
    /// Whether the plan card is expanded. **Collapsed by default** (user request, 2026-10-23): while
    /// work is running the only thing worth the space is "which step, and how far in".
    plan_open: bool,
    /// Expand/collapse position of the plan block: 0 = closed, 1 = fully open.
    plan_t: f32,
    /// `(started, from, to)` while the disclosure is in flight.
    ///
    /// Driven by the **wall clock**, not by `dt`. The frame-by-frame stepping used by `hist_t` reads
    /// `dt` from the input state, which while idle reports a stale, much larger value than the real
    /// frame time — so a 0.18 s animation collapsed into four jumps of 28% each, which is the
    /// stutter reported on 2026-10-23. Elapsed time is immune to both a stale `dt` and a low frame
    /// rate.
    plan_anim: Option<(Instant, f32, f32)>,
    /// Height of the composer **excluding** the plan block, measured last frame.
    ///
    /// The composer is a floating area pinned by an explicit position (see `show`'s note): the plan
    /// block's height is known in advance from `plan_t`, and this is the only part that has to be
    /// measured — which is fine, because it only changes when the text in the input grows a line.
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
            act: activity::new_shared(),
            cache: cache::new_shared(),
            wsview: wsview::new_shared(),
            auth: None,
            auth_reply: None,
            choice: None,
            choice_text: String::new(),
            choice_reply: None,
            tex: HashMap::new(),
            brand: brand::Art::new(),
            brand_err: None,
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
            // Both start folded and are replaced by what `.aox/ui.json` says on the first refresh
            // that knows the app root (see `App::hist_fold_loaded`): a fresh install — no file yet
            // — keeps the drawer shut and gives the full width to the transcript.
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
    ///
    /// Derived from the flags rather than latched at each transition on purpose: every place that
    /// could start or stop a wait would otherwise have to remember to call in (and the ones that
    /// *clear* a wait — a turn ending mid-question, a fatal error, a conversation switch — are
    /// exactly the ones easiest to forget). Deriving it every frame can't be missed, and being a
    /// frame late at a transition is worth nothing on a chart whose bars are seconds wide.
    ///
    /// Called at the *top* of the frame, before [`App::pump`], so the events handled this frame
    /// read a clock that already reflects the state they are about to change.
    fn sync_clock(&mut self) {
        self.clock.set_running(self.running());
    }

    /// Whether anything is actually progressing: a turn in flight, not parked on a person.
    ///
    /// The three ways a turn stops progressing are all waits on the user, and all three must look
    /// the same to anything that measures *time*: the agent clock (whose bars would otherwise grow
    /// while nobody was running) and the cache map's front (which would otherwise paint the same
    /// reading across a square that is waiting for a click). Kept in one place so a fourth such
    /// wait, when it appears, has one obvious home.
    fn running(&self) -> bool {
        self.busy && !self.waiting_auth && self.choice_reply.is_none()
    }

    /// On the first frame, converge the window into the screen and place it explicitly.
    ///
    /// Two problems to solve:
    ///
    /// 1. **Size**: startup asks for 1380x900 logical points. On a small display with high
    ///    DPI scaling, the client area together with the window decorations exceeds the
    ///    screen and the bottom half of the window (input box included) is cut off.
    /// 2. **Position**: without an explicit position the OS decides, and Windows
    ///    "cascades" new windows toward the bottom-right, putting the window's bottom edge
    ///    past the screen -- the user can't see the input box. The black band in the
    ///    screenshots is the off-screen region that `CopyFromScreen` captured as black.
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

        // The window decorations (title bar + border) sit outside the client area. The
        // first frame usually can't measure them yet, so fall back to a logical-point
        // floor: about 30 for the title bar and 8 for the border, so 46 covers 100%-200%
        // scaling.
        let chrome = (outer - inner).max(Vec2::new(18.0, 46.0));

        let want = default_window_size(mon, chrome);

        if (inner.x - want.x).abs() > 8.0 || (inner.y - want.y).abs() > 8.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(want));
        }

        // Centre it (outer frame = client area + decorations) so the bottom edge never
        // crosses the taskbar
        let outer_size = want + chrome;
        // The visible height (taskbar excluded) is likewise computed only once, in
        // default_window_size; here we derive it from "default height + decorations" so the
        // taskbar height isn't computed in two places.
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

    /// Record a system event into the activity stream (a short bar on the timeline's
    /// "system" row).
    ///
    /// **Only recorded while a turn is running**: the activity stream means "how this turn
    /// ran", so nothing extra should appear on it while the program is idle.
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

    // ─────────────────────── history ───────────────────────

    /// The application root, as the bridge resolved it.
    ///
    /// Taken from the boot info rather than re-derived from `store.location()`: the bridge is the
    /// one place that decides the root (it honours `--config`-style overrides), and a second
    /// derivation would silently disagree with it the day that logic changes.
    fn app_root(&self) -> Option<PathBuf> {
        self.boot.as_ref().map(|b| PathBuf::from(&b.paths.root))
    }

    // ── input area / keyboard ──

    // ─────────────────────────── painting ───────────────────────────

}

/// The main window's default size: computed from the monitor size, minus the borders and the
/// taskbar, then clamped to a range a person can actually look at.
///
/// The activity-stream window has to be "the same size as the main window", so this formula is
/// written once only -- computing it on both sides would inevitably drift apart.
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
        // First statement of the first frame: the appearance install that `App::new` could not do
        // yet (the system theme only becomes readable inside a pass). Idempotent, free after the
        // first call. See `theme::settle`.
        theme::settle(ctx);
        self.fit_window(ctx);
        // Before `pump`: the clock's run/stop state has to be settled from the *current* flags
        // before this frame's events are timestamped, so nothing records a moment that belongs to
        // a stretch that shouldn't be counted (see [`Clock`]).
        self.sync_clock();
        self.pump();
        // Drag and drop: anywhere in the window accepts (see `take_dropped`). Placed after
        // `pump` and before painting -- an attachment received this frame has to appear in the
        // pending-upload bar this same frame.
        self.take_dropped(ctx);

        // Image decoding: finishing (attaching textures) and queueing (starting background
        // threads). Once per frame, idempotent.
        self.poll_images(ctx);
        // The empty-state illustration gets a push here too: it likewise needs a background
        // thread to decode and likewise can't be drawn until that finishes.
        // Both images are decoded **from the start** (rather than decoding whichever one is in
        // use), which is why this isn't tied to the theme.
        self.brand.poll(ctx);

        // The every-frame recheck for "follow system": when the system theme changes, follow it
        // (egui puts the current system theme in `ctx` each frame). For why this isn't computed
        // once at the moment of switching, see the comment on `theme::follow_system` -- that way
        // "follow system" would degrade into "decide once at startup by copying the system".
        theme::follow_system(ctx);

        // The native title bar keeps only the window title itself: no icon is set, and the model
        // name is no longer appended, so a line like "AOX · xxx" isn't crowding the top-left. For
        // the colour sync see sync_native_theme below.
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        // `switching` needs a repaint too: the switch acknowledgement comes back from the agent
        // thread asynchronously, and an idle window doesn't repaint by itself, so without this it
        // looks like "I clicked and nothing happened" until the mouse moves next.
        if self.busy || self.waiting_auth || self.live.is_some() || self.switching {
            self.spin += dt;
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        // Advance the slide-out animation: step `hist_t` toward the toggle state until it gets
        // there.
        //
        // Stepping frame by frame (rather than "remember the start time + consult an easing
        // table") is what lets a mid-flight reversal connect properly: double-clicking the toggle
        // walks the progress back from **where it currently is**, instead of jumping to 0 and
        // starting over.
        let target = if self.show_history { 1.0 } else { 0.0 };
        if self.hist_t != target {
            let step = dt.min(0.05) / HIST_ANIM_SECS;
            self.hist_t = if self.hist_t < target {
                (self.hist_t + step).min(target)
            } else {
                (self.hist_t - step).max(target)
            };
            // During the animation a repaint has to be requested explicitly: an idle window
            // doesn't repaint on its own in egui, and without this the slide-out becomes a series
            // of stuttering jumps.
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

        // A recovery request is answered on a channel from the agent thread, and an idle window does
        // not repaint on its own — so without this the panel sits on "reading…" until the pointer
        // happens to move. Polling at ~30 ms is invisible next to the frame the answer arrives on,
        // and it stops the moment `busy` clears.
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

        // The drawer has to be declared **before** the central panel: a side panel is what tells the
        // central column how much room is left, so declaring it afterwards would push nothing (the
        // column would already have taken the whole window). It also means the drawer is drawn
        // first, which is why nothing has to be layered on top of it.
        //
        // Once `hist_t` reaches zero it stops drawing itself, so **don't** additionally test
        // `show_history` here -- the collapsing has to keep being drawn, and testing the toggle
        // would make the panel vanish mid-way.
        //
        // **Before the title bar too** (2026-10-18). A `TopBottomPanel` declared first would take a
        // full-width strip off the top, and the drawer would start 52 points down -- the column's
        // top edge visibly cut off by a bar that is the same colour as the background, which is
        // exactly what the user saw. Declared first, the drawer keeps the window's full height and
        // the bar only spans the column to its right; the two together still read as one row.
        self.drawer(ctx);
        // The bar is declared after the drawer, so it owns the top strip of what is left of the
        // window -- the drawer's own head is the left end of the same row.
        self.titlebar(ctx);
        // The activity stream lives in its own window: the main window syncs "what is happening
        // right now" over every frame and re-declares that viewport (egui viewports are
        // declarative; one that isn't declared gets reclaimed).
        activity::lock(&self.act).tick(self.now(), &self.status, self.busy);
        // The cache-hit map is fed the same way: one `observe` per finished model call, plus the
        // per-frame time base. It is handed `running`, not `busy`: the strip freezes whenever the
        // next reading is blocked on a person (an idle window, a question waiting for a choice, a
        // plugin waiting for a permission), and `running` is exactly that distinction — see
        // `sync_clock`.
        cache::lock(&self.cache).tick(self.now(), self.running());
        // Hand the child windows this window's native handle, once: they need it to keep this
        // window painting while a turn runs (a child takes the foreground when it opens, and a
        // frozen pass loop here is a frozen chart there — see `gwin::poke_main`).
        gwin::remember_main_window(frame);
        activity::sync(ctx, &self.act);
        cache::sync(ctx, &self.cache);
        // The workspace view is fed the same way, and for the same reason: the window is a pure
        // function of what it is handed. The root comes from the workplace receipt (it changes when
        // the tier is switched), and `running` rather than `busy` for the dot — a turn waiting on a
        // person is not editing anything, so the file being watched should stop being re-read.
        wsview::set_root(&self.wsview, self.ws.view().and_then(|v| v.root.clone()));
        wsview::tick(&self.wsview, self.running());
        wsview::sync(ctx, &self.wsview);
        let frame = Frame::NONE.fill(P::bg());
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let column_rect = ui.max_rect();
            // The first screen's background goes down **first**: the transcript and the floating
            // input card (a Foreground `Area`) are then painted on top of it for free.
            //
            // It is painted from here rather than from inside `transcript` because that one lives in
            // a scroll area: its coordinates are offset by the scroll, so a rect taken from there
            // would land in the wrong place the moment the content is taller than the viewport.
            //
            // Only the first screen gets it. Once something has been said the column goes back to
            // the plain background, exactly as before -- the picture belongs to the *empty* state.
            self.brand_err = if self.on_empty_state() {
                self.brand.show_background(ui, column_rect)
            } else {
                None
            };
            // One height, two consumers: the composer is positioned from it, and the transcript
            // ends there. Computing either independently is how the last line (and the hit-rate
            // block) ended up behind the card. The plan's height is exact for this frame because
            // `plan_t` drives it; the rest of the card was measured on the previous frame.
            let plan_h = self.plan_block_height();
            let composer_h = self.composer_base_h + plan_h;
            let composer_top =
                (column_rect.bottom() - COMPOSER_BOTTOM_GAP - composer_h).max(column_rect.top());

            self.transcript(ui, composer_top);

            // The input box became a floating card (a Foreground layer) ending exactly at the
            // boundary the transcript stopped at. Why not put it in a `TopBottomPanel::bottom`:
            // that would permanently eat a bottom bar, and since the panel's fill matches the
            // background it just looks like an empty region at the bottom.
            // `constrain_to` does two jobs here: it confines the clip region to this column (so it
            // can't cover the content of the layer below), and it makes the anchor's centring
            // reference frame exactly this column.
            let clip = Rect::from_min_max(
                column_rect.min,
                Pos2::new(column_rect.right(), column_rect.bottom()),
            );
            // The anchor is measured from the **window's** centre, while the card has to sit on the
            // column's centre -- and the column starts wherever the drawer ends. Half the drawer's
            // width is exactly the difference, on the same eased clock the panel itself uses, so the
            // card travels with the column instead of being clamped into it after the fact.
            // **Pin the composer by its top-left, not by an anchor.**
            //
            // `Area::anchor` positions an area using the size measured on the *previous* frame. The
            // plan block changes height during its disclosure, so the anchor was applied to a stale
            // height: the card's bottom moved by exactly that frame's delta and snapped back — "the
            // input box bobs up and down" (reported 2026-10-23).
            //
            // Instead the plan block's height is known *exactly* in advance (`plan_t` drives it), and
            // the rest of the composer is measured once per frame. Both together give the total
            // height now, so `bottom - height` is a real bottom: the card grows upwards and its
            // bottom stays put.
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

            // The last of the three calls that together bury the eight border hot-zones: this one
            // owns the right edge, the bottom edge and the two right-hand corners. Declared after
            // everything else in the column so it beats the transcript and the scrollbar at the
            // outermost few pixels.
            gwin::resize_handles(ui, ctx, ctx.content_rect());
        });

        // The workplace's three overlays: the three-way pop-up list, the full-access
        // confirmation, and the directory picker. All are scheduled after the CentralPanel --
        // they have to sit above the transcript and the input card.
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
        // Scheduled **after** the workplace's three overlays so that if both were somehow open (a
        // frame where the click order crossed), the model list is the one on top — it is anchored to
        // the leftmost chip, so it covers less of the row.
        let model_req = crate::model_ui::menu(ctx, &mut self.mdl);
        self.send_requests(model_req);
        // The add/edit dialog. Drawn here rather than from the settings tab: a modal inside the
        // settings card's scroll area would be clipped by it.
        let dialog_req = crate::model_ui::dialog(ctx, &mut self.mdl);
        self.send_requests(dialog_req);
        // The recovery panel, drawn above the transcript for the same reason the menus above are:
        // it overlaps the input card. Its confirmation dialog is a real modal, so it comes with the
        // other dialog overlays below.
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
        // The other question the same panel can ask (clear every point). Drawn after the rollback
        // one so it wins if both were somehow open; they never are, since each closes the other's
        // trigger.
        if let Some(req) = self.rec.clear_confirm(ctx) {
            self.send_recovery(req);
        }
        self.settings_overlay(ctx);
        self.auth_overlay(ctx);
        // Last, so the toast sits above everything: it reports something that has **already**
        // happened, and being hidden behind a card that is asking a question would lose exactly the
        // moment it was needed.
        self.toast_overlay(ctx);

        // The window has no system frame any more, so the 1px seam against the desktop is ours to
        // draw — the same stroke the independent windows use (`gwin::outline`). It has to come
        // **after** every panel: the central panel's fill would otherwise cover the left and top
        // edges, and it has to be a layer of its own because each panel clips its own painting.
        let full = ctx.content_rect();
        gwin::outline_painter(
            &ctx.layer_painter(egui::LayerId::new(
                egui::Order::Debug,
                egui::Id::new("aox-window-frame"),
            )),
            full,
        );
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

// ─────────────────────────── widgets ───────────────────────────

/// The denominator for the ring when the host reports no window at all (it normally reports one:
/// `[provider] context_window` has a default now).
///
/// The same constant the agent compacts against (`aox::config::DEFAULT_CONTEXT_WINDOW`), not a second
/// number that happens to agree today: a ring showing "40% full" while compaction runs at a different
/// threshold is a display that lies.
const DEFAULT_CONTEXT_WINDOW: u64 = aox::config::DEFAULT_CONTEXT_WINDOW as u64;
