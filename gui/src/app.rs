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
//! **The left history column is no longer a `SidePanel` in the layout either**: it became an
//! overlay that **slides down** from the top-left button (see [`App::history_overlay`]); and
//! the "new conversation" "+" **slides out to the right from behind** the history button
//! (hidden by default, see [`App::toolbar`]). The two share one progress value `hist_t` and
//! one animation duration [`HIST_ANIM_SECS`].
//!
//! The activity stream is **not in the main window**: clicking that icon opens it in its own
//! window (see `activity.rs`).
//!
//! There is no separate "input area" row at the bottom: the input box is a card floating
//! above the transcript, and scrolling can carry it all the way under it.

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
use crate::brand;
use crate::bridge::{
    BootInfo, BootPaths, FromAgent, RuntimeView, SessionFlags, Snapshot, ToAgent, WorkplaceView,
};
use crate::markdown;
use crate::theme::{self, P};
use crate::workplace_ui;

// ─────────────────────────── data model ───────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    Info,
    Warn,
    Error,
}

impl NoteKind {
    /// A monochrome scheme has no red/yellow/green left to spend, so we use lightness
    /// instead: body colour / secondary / weaker.
    fn color(self) -> Color32 {
        match self {
            NoteKind::Info => P::text_dim(),
            NoteKind::Warn => P::text_dim(),
            NoteKind::Error => P::text(),
        }
    }
}

/// One tool call (for the collapsed display).
#[derive(Clone)]
struct StepView {
    name: String,
    plugin: Option<String>,
    /// The file this call is *about*, when naming it is worth a glance — `write_file`'s `path`, read
    /// off the arguments. `None` for every other tool (see `SUBJECT_TOOLS`).
    ///
    /// A field of its own rather than a read of `args` below: while the model is still *writing* the
    /// call the row is already on screen and the arguments are not in yet, and this is what lets the
    /// row name the file from its very first fragment.
    subject: Option<String>,
    /// Call arguments (a JSON string). **Not shown in the UI** -- the row keeps only the
    /// tool name, see `step_row`; the field is kept so that when debugging you can line it
    /// up with the matching row in `history/*.jsonl` at a glance.
    #[allow(dead_code)]
    args: String,
    result: Option<(bool, String)>,
    /// `(added, removed)` lines when this call edited a text file (see `ToolOutput::diff`), drawn as
    /// "+N / −N" after the tool name. `None` on every non-edit call — and on edits restored from an
    /// old `history/*.jsonl`, which never carried the number.
    diff: Option<(usize, usize)>,
    /// The edit itself, one line per entry, each prefixed with `+`, `-` or a space (see
    /// `ToolOutput::diff_detail`). This is what the row unfolds into; `None` wherever `diff` is.
    diff_detail: Option<String>,
    /// Whether the row is unfolded. Clicking it flips this — possible only when there is a
    /// `diff_detail` to unfold, which is also exactly when the row shows its caret.
    expanded: bool,
}

/// A tool call the model is **still writing** — its arguments are arriving in fragments.
///
/// This is what keeps a call from being invisible until it is over. The loop only learns about tool
/// calls once the whole response has landed, and by then a `write_file` is usually done, so on its
/// own the conversation sits still through the longest part of the job and then shows a finished
/// row. A `StreamCall` is opened on the first fragment (`StreamSink::on_tool_call`), which puts the
/// row and its waiting marquee on screen straight away, and carries the fragments in `args` so the
/// reading next to the name can grow while the model writes.
///
/// `step` is the row's slot in `Live::steps`. `usize::MAX` means "the row is not drawn yet" — a
/// provider is free to send arguments before it sends the name, and a row with no name has nothing
/// to show.
struct StreamCall {
    index: u64,
    name: String,
    args: String,
    step: usize,
}

/// The "process" of one turn (collapsed into a single row once it ends).
struct Process {
    steps: Vec<StepView>,
    interim: Vec<String>,
    reasoning: String,
    /// Elapsed time. History files carry no timing information, so `None` means "don't
    /// show a duration".
    elapsed: Option<f32>,
    expanded: bool,
    aborted: bool,
}

/// One entry in the transcript.
enum Item {
    /// What the user said + the **display names** of the attachments on that message.
    ///
    /// Names only, no bytes: what the bubble draws is a row of small chips (`name.ext`),
    /// not the images themselves; the real attachment content is nothing but a path
    /// reference in the `.jsonl`.
    User(String, Vec<String>),
    Assistant(String),
    Process(Process),
    Note(NoteKind, String),
    /// An **image** carried by the previous message, drawn as its own block below it.
    ///
    /// Why a separate Item rather than a field on `User` / `Note`: images have two sources
    /// (dragged in by the user, produced by a tool/plugin) which land on `User` and `Note`
    /// respectively, yet **the drawing is exactly the same**. With a variant of its own the
    /// rendering is written once, and it won't need changing again when a future
    /// "message type with images" shows up (changing a variant touches every construction
    /// site).
    Images(Vec<PathBuf>),
    /// One turn's token breakdown. It sits right under that turn's AI reply -- the bill
    /// belongs next to where it was spent, not piled into a top bar for the user to match
    /// up with turns by themselves.
    Usage(String),
    /// Error card (unrecoverable)
    Error(String),
}

/// A turn in progress.
struct Live {
    status: String,
    reasoning: String,
    reasoning_live: bool,
    steps: Vec<StepView>,
    interim: Vec<String>,
    /// How many call steps had **already run** when `interim[i]` was spoken -- used to
    /// split the live area into alternating segments "call stream -> interim reply ->
    /// call stream -> ..." (user request, 2026-10-05: previously all the calls were crammed
    /// at the top and all the replies in the middle, so you couldn't tell which steps
    /// belonged to which stage).
    ///
    /// We store "cut points" rather than replacing `steps` / `interim` with
    /// `Vec<Segment>`: at the end of a turn these two have to be handed to the process card
    /// as-is, and keeping them flat means that commit path doesn't have to change.
    cuts: Vec<usize>,
    pending_final: Option<String>,
    /// **The body text being typed** (the sentence that isn't finished yet at this
    /// instant).
    ///
    /// Division of labour with `pending_final`: `pending_final` is "the complete body text
    /// this response has settled on", which only turns into a message at the end of a turn;
    /// `streaming` is what makes it **visible** before it hits disk (user request,
    /// 2026-10-05: the reply only popped in all at once when everything was finished, and
    /// until then the conversation was blank).
    /// The two are reconciled when `AgentEvent::AssistantText` / `InterimText` arrive --
    /// by then the deltas are done, so the authoritative text overwrites once and a few
    /// dropped frames can't leave the screen a few characters off.
    streaming: String,
    /// The activity clock's reading when this turn started. The live "12.3s" readouts subtract it
    /// from the clock's current reading, so they show the turn's **working** time — the turn
    /// doesn't look like it took four minutes just because the user pondered a question for that
    /// long (see [`Clock`]).
    turn_base: f32,
    /// Characters received on the output side (reasoning + body), used to scroll a live
    /// token count that keeps climbing.
    ///
    /// During streaming the server hasn't reported yet, so this can only be an estimate;
    /// at the end of a turn the exact value from `TurnUsage` replaces it.
    out: TokenEstimate,
}

struct AuthCard {
    tool: String,
    args: String,
}

/// A multiple-choice question waiting on the user (`ask_user`). The question and the
/// options are the model's own words.
///
/// `Clone` is derived because the answer row needs `&mut self.choice_text` while the same
/// frame still has to read the question and the options -- cloning something small (one
/// sentence plus at most 6 short strings) beats gymnastics with borrows.
#[derive(Clone)]
struct ChoiceCard {
    question: String,
    options: Vec<String>,
}

/// Which stage an image is currently in.
///
/// The three stages each get a variant rather than one `Option<TextureHandle>` plus a
/// separate error table: the UI already draws all three differently (the image /
/// "decoding..." / a one-line failure reason), and merging them would only make the
/// renderer split them apart again.
enum Tex {
    /// A background thread is decoding
    Loading(std::sync::mpsc::Receiver<Result<aox::image::Bitmap, String>>),
    Ready(egui::TextureHandle),
    Failed(String),
}

/// The columns down the left of the settings dialog. **Declaration order is the order
/// they appear in the list, top to bottom.**
///
/// "Extra" comes first because what it holds isn't a setting (it's an entrance), and the
/// further from the middle it sits the less it gets in the way of someone actually
/// looking for settings -- but the user explicitly asked for it first, so first it is; we
/// don't reorder on their behalf.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettingsTab {
    /// Extra: things like the activity-stream entrance that "belong to no setting"
    Extra,
    /// Appearance: light/dark / follow system
    Appearance,
    /// System: paths and runtime diagnostics
    System,
    /// About: version, license, links
    About,
}

impl SettingsTab {
    const ALL: [SettingsTab; 4] =
        [SettingsTab::Extra, SettingsTab::Appearance, SettingsTab::System, SettingsTab::About];

    fn label(self) -> &'static str {
        match self {
            SettingsTab::Extra => aox::i18n::t("Extra"),
            SettingsTab::Appearance => aox::i18n::t("Appearance"),
            SettingsTab::System => aox::i18n::t("System"),
            SettingsTab::About => aox::i18n::t("About"),
        }
    }
}

/// The activity clock: seconds of *work* since launch, with the stretches where nothing was
/// running **cut out**.
///
/// Everything timed in this program is timed against this clock rather than the wall, because the
/// wall is the wrong unit for "how long did this take". Two stretches must not count:
///
/// * **Idle** — after a turn ends and before the next instruction. The user reading a reply or
///   typing has nothing to do with how long the work took, and (worse) it is usually minutes, so
///   leaving it in stretched one round away from the next and buried the actual work in a mostly
///   empty timeline. Cutting it is also what makes consecutive rounds sit flush: the next round's
///   first record lands exactly where the previous round's last one stopped.
/// * **Waiting on the user** — an authorization prompt or an `ask_user` question. While the
///   question is on screen the model is blocked on a person, not working, so that time belongs to
///   neither side's duration. This is why a "waiting for your choice" bar no longer reads as if
///   the tool ran for four minutes.
///
/// Summing instead of subtracting a start point (`Instant::now()` would give only the running
/// stretch, losing every earlier one) is what makes it survive many pause/resume cycles: each
/// stopped stretch is folded into `paused` once, on the way out.
struct Clock {
    started: Instant,
    /// Seconds already accumulated as "not running"
    paused: f32,
    /// When the current pause began (`None` = running right now)
    since: Option<Instant>,
}

impl Clock {
    fn new() -> Self {
        Self { started: Instant::now(), paused: 0.0, since: None }
    }

    fn now(&self) -> f32 {
        let paused = self.paused + self.since.map_or(0.0, |t: Instant| t.elapsed().as_secs_f32());
        (self.started.elapsed().as_secs_f32() - paused).max(0.0)
    }

    /// Run or stop the clock. Idempotent, so it is safe to call every frame with a derived
    /// condition: only the *change* folds time into `paused` (and `set_running` is the one place
    /// that does, so no stretch can be counted twice or lost).
    fn set_running(&mut self, run: bool) {
        match (run, self.since) {
            (false, None) => self.since = Some(Instant::now()),
            (true, Some(t)) => {
                self.paused += t.elapsed().as_secs_f32();
                self.since = None;
            }
            _ => {}
        }
    }
}

// ─────────────────────────── App ───────────────────────────

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
    /// Whether the "Σ total tokens" panel is currently being pressed by the pointer.
    ///
    /// `resp.hovered()` alone breaks the instant the pointer travels from the chip to the
    /// panel (there is a gap between them) and the panel flashes away. Remembering a
    /// cross-frame flag holds it steady.
    usage_hover: bool,

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
    /// The on/off state of the "history" button at the top-left. It is the overlay's
    /// target state.
    show_history: bool,
    /// Slide-out animation progress: `0` = fully collapsed, `1` = fully expanded.
    ///
    /// `App::update` steps it toward `show_history` every frame, and `toolbar` /
    /// `history_overlay` both derive their offsets from it. **Do not use the boolean
    /// directly**: that gives you only "there" and "not there", with nowhere to stand for
    /// the two-hundred-odd milliseconds in between, and the slide-out degrades into a jump
    /// cut.
    hist_t: f32,
    /// The history overlay's actual height **on the previous frame**.
    ///
    /// The slide-in distance has to equal the panel's own height (the whole block slides in
    /// from above), and that height is set by the content (how many records there are) --
    /// which is only known once it has been drawn. So we remember one frame: this frame
    /// uses it for the offset, and after drawing we record the new one.
    hist_h: f32,
    /// The rect of the top-left history button. It answers exactly one question: does this
    /// click count as "outside the overlay"?
    ///
    /// Without it, the frame in which the button is clicked hits both the "button clicked"
    /// and the "clicked outside" paths -- the opening action is cancelled by itself on the
    /// spot, which shows up as "I clicked and nothing happened".
    bar_hist_rect: Option<Rect>,

    /// Demo mode (--demo): no LLM attached, just shows the layout and the shape of the
    /// event stream
    demo: bool,
    /// Whether the transcript auto-follows to the bottom
    follow: bool,
    /// Force a scroll to the bottom for this message (right after the user sends a command)
    force_follow: bool,
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
}

/// How long a toast stays up, in seconds. Also the window in which a second delete merges into it.
const TOAST_SECS: f32 = 6.0;
/// The last stretch of that, during which it fades instead of vanishing mid-read.
const TOAST_FADE_SECS: f32 = 0.6;
/// Corner radius of the toast. Deliberately larger than a card's: the notice is a small rounded
/// capsule, not a piece of page furniture.
const TOAST_RADIUS: u8 = 14;

/// The host's file-writing tool, by name.
///
/// The GUI needs it to know which call is carrying text it can count line by line
/// (`StreamCall::args` → `aox::llm::partial_json_lines`). It is written out here because the core's
/// table (`tool::HOST_FILE_TOOLS`) is `pub(crate)` and unreachable from this crate — same reason
/// `plugin_of` spells out the three `meta` tool names.
const WRITE_FILE_TOOL: &str = "write_file";

/// Tools whose row **names the file it works on**.
///
/// Only the file writer for now (user request, 2026-10-11: "also show the file being edited"). Adding
/// a read or a delete here is a one-line change the day the same is wanted for them — nothing else
/// keys off this list.
const SUBJECT_TOOLS: &[&str] = &[WRITE_FILE_TOOL];

/// Height cap of the unfolded diff panel. A peephole into the edit, not a second editor — the point
/// is "which lines moved", and the panel scrolls for the rest.
///
/// **Not draggable** (the grab strip was built and then dropped, user decision 2026-10-11: "forget
/// the resizing, just double the height"): a fixed cap is predictable, and the panel still shrinks to
/// fit a short diff, so a two-line edit is not buried in empty background.
const DIFF_PANEL_H: f32 = 480.0;

/// Longest path shown on a call row before it is shortened from the middle. The tail is the half
/// that identifies a file, so a shortened path keeps it.
const SUBJECT_MAX: usize = 56;

/// The file a call is about, straight off its **parsed** arguments.
fn subject_from_value(name: &str, args: &serde_json::Value) -> Option<String> {
    if !SUBJECT_TOOLS.contains(&name) {
        return None;
    }
    args.get("path").and_then(|v| v.as_str()).map(str::to_string)
}

/// The same, from the arguments as text (a row rebuilt from `history/*.jsonl`, which keeps them as
/// the model wrote them).
fn subject_from_json(name: &str, args: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    subject_from_value(name, &v)
}

/// Shorten a path from the middle: `keep/both/ends.rs`. See `SUBJECT_MAX`.
fn clip_path(s: &str) -> String {
    let n = s.chars().count();
    if n <= SUBJECT_MAX {
        return s.to_string();
    }
    let keep_head = SUBJECT_MAX / 4;
    let keep_tail = SUBJECT_MAX - keep_head - 1;
    let head: String = s.chars().take(keep_head).collect();
    let tail: String = s.chars().skip(n - keep_tail).collect();
    format!("{head}…{tail}")
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
            // Seed the panel height with "a window of a few records": the first frame has
            // never been drawn, so there is no measured value, yet this frame's slide-in
            // offset needs a number right now. Every later frame overwrites it with the
            // real height.
            hist_h: 320.0,
            bar_hist_rect: None,
            // The history overlay starts collapsed: the full width goes to the transcript,
            // and you click the top-left icon when you want it.
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
            toast: None,
            stream_calls: Vec::new(),
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
        let running = self.busy && !self.waiting_auth && self.choice_reply.is_none();
        self.clock.set_running(running);
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

    // ── event pump: land the agent thread's messages into local state ──
    fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                FromAgent::Boot(b) => {
                    self.model = b.model.clone();
                    self.config_path = b.config_path.clone();
                    // The UI-side history backend is shipped along by the bridge: the same
                    // implementation the agent thread uses.
                    self.store = Some(b.store.clone());
                    self.refresh_history();
                    if let Some(note) = &b.note {
                        self.log_error = Some(note.clone());
                        self.items.push(Item::Error(note.clone()));
                    }
                    self.status = if b.provider_ready { aox::i18n::t("Ready").to_string() } else { aox::i18n::t("No model configured").to_string() };
                    let ready = b.provider_ready;
                    self.boot = Some(*b);
                    if ready {
                        // --prompt: send the first instruction automatically once the model is ready
                        if let Some(p) = self.pending_prompt.take() {
                            self.input = p;
                            self.send();
                        }
                    }
                }
                FromAgent::Snapshot(s) => self.apply_snapshot(s),
                FromAgent::Runtimes(v) => self.runtimes = v,
                // Current workplace state: the tier name on the button and the ticked row
                // in the list both come from it. The bridge re-sends one at startup, at the
                // end of every turn, and after every tier change.
                FromAgent::Workplace(v) => self.ws.set_view(*v),
                FromAgent::HistoryCurrent(id) => {
                    // Keep counts/titles fresh while the sidebar is open; while it is closed
                    // only "the conversation file changed" is worth a directory scan
                    // (otherwise every turn re-reads the disk for nothing).
                    let changed = self.current_history != id;
                    self.current_history = id;
                    if changed || self.show_history {
                        self.refresh_history();
                    }
                }
                FromAgent::Switched { opened, error } => {
                    self.switching = false;
                    if let Some(e) = error {
                        // When loading fails the agent's context was never touched, so change
                        // nothing here either -- otherwise the UI has already "jumped" while
                        // the model is still in the old conversation.
                        self.items.push(Item::Error(e));
                        continue;
                    }
                    self.current_history = opened.clone();
                    self.adopt_conversation(opened.as_deref());
                    self.refresh_history();
                }
                FromAgent::TurnStart => {
                    // Read the clock before `busy` is set: the turn's own time starts where the
                    // instruction landed, and the turn's readouts then measure from here.
                    let base = self.now();
                    self.live = Some(Live {
                        status: aox::i18n::t("Thinking").into(),
                        reasoning: String::new(),
                        reasoning_live: false,
                        steps: Vec::new(),
                        interim: Vec::new(),
                        cuts: Vec::new(),
                        pending_final: None,
                        streaming: String::new(),
                        turn_base: base,
                        out: TokenEstimate::default(),
                    });
                    self.busy = true;
                    self.status = aox::i18n::t("Thinking").into();
                    // Defensive: a call still marked as "being written" from the previous turn would
                    // otherwise be adopted by a call of the same name in this one.
                    self.stream_calls.clear();
                }
                FromAgent::Status(s) => {
                    self.status = s.clone();
                    if let Some(l) = self.live.as_mut() {
                        l.status = s;
                    }
                }
                FromAgent::Content(d) => {
                    // The body delta serves two purposes: (1) keep the token counter climbing;
                    // (2) **show it immediately** -- appending to `streaming` lets the UI type it
                    // out character by character (user request, 2026-10-05: the reply only popped
                    // in once everything was finished, and until then the conversation was
                    // blank).
                    // The activity stream's model segment is still advanced by
                    // `Event(AssistantText)`: inserting it again here would put the same stretch
                    // of text on the timeline twice.
                    if let Some(l) = self.live.as_mut() {
                        l.out.add(&d);
                        l.streaming.push_str(&d);
                    }
                    // Body text means the model is past any call it was writing, so the status must
                    // stop claiming a tool is running (`stream_tool_delta` makes the opposite switch).
                    self.status_back_to_thinking();
                }
                FromAgent::ToolDelta { index, name, delta } => {
                    self.stream_tool_delta(index, name, delta)
                }
                FromAgent::Usage(rec) => {
                    // If the endpoint reports no numbers, hide the whole block -- don't fob it off
                    // with 0s
                    if !rec.is_empty() {
                        // The breakdown goes right after this turn's reply (by now the turn has
                        // ended and the tail of `items` is exactly that), and drag the view to the
                        // bottom while we're at it -- nobody goes looking for a row that appeared
                        // out of nowhere.
                        self.items.push(Item::Usage(usage_line(&rec)));
                        self.usage_log.push(*rec);
                        self.force_follow = true;
                    }
                }
                FromAgent::UsageLog(log) => {
                    // The whole ledger the agent read back from file after a conversation switch.
                    // The per-turn breakdowns have to go back into the transcript -- when an old
                    // conversation is opened, each turn's bill should be stuck back where it was,
                    // not heaped up at the top.
                    self.usage_log = log;
                    self.rebuild_usage_rows();
                }
                FromAgent::Reasoning(d) => {
                    if let Some(l) = self.live.as_mut() {
                        // `self.now()` cannot be used here (it borrows all of self), while `l` is
                        // holding a mutable borrow of `self.live`. `self.clock` is a *different*
                        // field, so borrowing it alone is fine — and it has to be this clock, not
                        // `Instant::now()`, or the model segment would start on the wall clock and
                        // drift away from every other record the moment a pause happened.
                        let at = self.clock.now();
                        if !l.reasoning_live {
                            l.reasoning_live = true;
                            activity::lock(&self.act).model_begin(at);
                        }
                        if l.reasoning.len() < 120_000 {
                            l.reasoning.push_str(&d);
                        }
                        // Reasoning counts as output too: the server's completion_tokens already
                        // includes it, so counting only the body would come out noticeably low.
                        l.out.add(&d);
                        activity::lock(&self.act).model_push(&d);
                    }
                    // Same switch as the body-text arm: thinking again means no tool is running.
                    self.status_back_to_thinking();
                            }
                FromAgent::ReasoningEnd => {
                    // Reasoning ending does not mean the model's segment has ended: it may go on
                    // to emit body text or to call a tool, and either of those closes this segment
                    // on its own.
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
                        &aox::tf!("Request authorization · {}", tool),
                        &args,
                        at,
                    );
                }
                // A multiple-choice question asked via `ask_user`. Same pattern as an
                // authorization request, but it **does not block input** (the user has to be able
                // to type an answer), so the `input` / `can_send` switches are left alone here --
                // only the card is hung up and the state changed.
                FromAgent::ChoiceRequest { question, options, reply } => {
                    self.status = aox::i18n::t("Waiting for your choice").into();
                    self.choice = Some(ChoiceCard { question: question.clone(), options });
                    // Left-over text from the previous question must not be carried over: that
                    // produces "I haven't typed anything yet, but there's text in the box"
                    self.choice_text.clear();
                    self.choice_reply = Some(reply);
                    // The option slot sits above the input box and the user is probably looking at
                    // the end of the transcript right now -- it is only visible once you scroll to
                    // the bottom. Without the scroll it reads as "the model asked something and then
                    // nothing happened".
                    self.force_follow = true;
                    let at = self.now();
                    activity::lock(&self.act).note(ActKind::Info, aox::i18n::t("Waiting for your choice"), &question, at);
                }
                FromAgent::Fatal(e) => {
                    self.items.push(Item::Error(e.clone()));
                    self.note_act(ActKind::Error, aox::i18n::t("Fatal error"), &e);
                    if self.live.is_some() {
                        self.finish_turn(false);
                    }
                }
            }
        }
    }

    fn apply_snapshot(&mut self, s: Snapshot) {
        // The top bar no longer has a "plugins N / tools N" chip, so only the data itself is
        // kept here; plugin changes already show up in the bridge's event stream and don't need
        // another entry here.
        self.plugins = s.plugins;
    }

    // ─────────────────────── history ───────────────────────

    /// Rescan the history directory.
    ///
    /// Done only when the sidebar opens, a turn ends, or a new conversation is started, rather
    /// than every frame -- scanning every frame needlessly interrupts the UI's smoothness, and
    /// the history list doesn't need to be that fresh anyway.
    fn refresh_history(&mut self) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let mut items = store.list(HISTORY_LIST_LIMIT);
        // The conversation being chatted in is always pinned to the top.
        //
        // The on-disk sort key is "time of the last utterance", and the conversation just
        // switched into may not have been touched for three days, so it lands far down the
        // list -- yet that is the one the user is looking for right now. Pinning is a purely
        // presentational adjustment: it doesn't touch file times and doesn't affect the order
        // `list()` gives the CLI.
        if let Some(cur) = self.current_history.as_deref() {
            if let Some(i) = items.iter().position(|s| s.id == cur) {
                let it = items.remove(i);
                items.insert(0, it);
            }
        }
        self.history = items;
    }

    /// Switch to a history conversation.
    ///
    /// Send the request and return: the actual context swap happens on the agent thread, and
    /// the UI only moves once `FromAgent::Switched` comes back -- that way a failed switch
    /// doesn't have the UI jump ahead and leave the UI on one conversation while the model is
    /// on another.
    fn switch_history(&mut self, id: &str) {
        if self.busy || self.switching || Some(id) == self.current_history.as_deref() {
            return;
        }
        self.switching = true;
        let _ = self.tx.send(ToAgent::Resume(id.to_string()));
    }

    /// Start a new conversation.
    ///
    /// The old conversation **needs no wrap-up at all**: it was appended to as we chatted, so
    /// it already sits complete in `history/`. So all this does is let go on the agent side,
    /// with the UI clearing along with it.
    fn new_conversation(&mut self) {
        if self.busy || self.switching || (self.current_history.is_none() && self.items.is_empty()) {
            return;
        }
        self.switching = true;
        let _ = self.tx.send(ToAgent::NewConversation);
    }

    /// Move the UI to the conversation with id `id` (`None` = a brand-new empty one).
    ///
    /// The content is rebuilt by reading the disk directly rather than having the agent send
    /// the messages over: the history file is local anyway, and passing only the id down the
    /// channel keeps the responsibilities cleaner (the agent owns context, the UI owns
    /// presentation).
    fn adopt_conversation(&mut self, id: Option<&str>) {
        self.items = match (id, self.store.as_ref()) {
            (Some(id), Some(store)) => store
                .load(id)
                .map(|l| items_from_history(&l))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        // A brand-new conversation draws a fresh background (the user's ask: "pick one at startup
        // and on every new conversation"). Opening an existing conversation keeps whatever is
        // showing -- only `None` means "nothing has been said yet", which is the one state the
        // background belongs to. Note that this re-rolls even when startup already rolled once:
        // the second roll is the one that lands on screen, so there is nothing to reconcile.
        if id.is_none() {
            self.brand.reroll();
        }
        // Swap out the previous conversation's in-progress state along with the activity
        // stream, otherwise the right side would show the new conversation while scrolling the
        // old conversation's tool calls.
        self.live = None;
        self.waiting_auth = false;
        self.auth = None;
        self.auth_reply = None;
        // The option slot is folded away too. **Dropping the reply channel is enough**: the
        // peer's `rx.await` errors out immediately, and `GuiUi::choose` maps "channel closed" to
        // `Cancelled` (see the comment in bridge) -- so the agent thread doesn't get stuck in a
        // `choose()` that will never see an answer.
        self.choice = None;
        self.choice_reply = None;
        // The usage ledger is swapped along with the messages and the activity stream: the
        // agent side has read its ledger back too, and the `UsageLog` that follows will send
        // this conversation's records over.
        self.usage_log.clear();
        // The activity stream is swapped too. It lives in the **same file** as the messages, so
        // "open this conversation" should show the timeline as it was then; a new conversation
        // clears it.
        //
        // Fetch the records before taking the lock: reading the store goes through `self.store`,
        // while the lock guard holds `self.act` for a long time.
        let rows = match (id, self.store.as_ref()) {
            (Some(id), Some(store)) => store.load_acts(id),
            _ => Vec::new(),
        };
        // Compute `now` before taking the lock: `self.now()` borrows all of `self`, which
        // tangles easily with the lock guard.
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

    /// The history overlay (a conversation switcher) that **slides down** from the top-left
    /// button.
    ///
    /// It replaces what used to be a `SidePanel::left`: the content is lifted from the sidebar
    /// (title / refresh / separator / list / empty state) while the shell changes from "a column
    /// in the layout" to "a layer floating above the transcript".
    ///
    /// # How the slide-out is done
    ///
    /// Two pieces bolted together -- **position** (the whole block travels from "target
    /// position - its own height" to the target position) plus **clipping** (the visible region
    /// pinned to the target position's top edge). With translation alone the panel would first
    /// pop out **over the button's face** and then travel down; with clipping it grows out of
    /// the button's bottom edge bit by bit.
    ///
    /// `Ui::set_clip_rect` takes care of interaction along the way: the `interact_rect` egui
    /// gives a widget is exactly `clip_rect ∩ rect`, so the clipped-away half can't be clicked.
    /// Without it, the mouse sweeping over the "not grown out yet" region during the slide-out
    /// would still hit a row and switch conversations out of nowhere.
    fn history_overlay(&mut self, ctx: &egui::Context) {
        if self.hist_t <= 0.0 {
            return;
        }
        let ease = ease_out_cubic(self.hist_t);
        // Clicks are refused while a switch request is in flight or a turn is running: a switch
        // started during either of those takes effect only later in the queue, and all the
        // clicker sees is the UI "hesitating before it jumps".
        let locked = self.busy || self.switching;

        // Same left edge as the top-left button; top edge = the button's bottom edge + 8.
        let x = BAR_LEFT;
        let top = BAR_TOP + BAR_BTN + 8.0;
        let w = HISTORY_W;
        // The panel height is set by the content (how many records there are); this only caps
        // it, since going past the window's lower edge gets sliced off hard by the window
        // boundary. 22px is left for the bottom corner radius and the shadow.
        let max_h = (ctx.content_rect().bottom() - top - 22.0).clamp(150.0, 620.0);
        let slide = -(1.0 - ease) * self.hist_h;

        let mut want_refresh = false;
        let mut open: Option<String> = None;

        let area = egui::Area::new(egui::Id::new("aox-history"))
            .order(egui::Order::Foreground)
            .fixed_pos(Pos2::new(x, top + slide))
            // Mid-slide the position is **above** the target (`top + slide` ends up over the
            // button), and an Area pulls out-of-bounds positions back on screen by default --
            // which flattens the offset on the spot and kills the animation. So the constraint is
            // released here: the position is in charge.
            .constrain_to(loose_rect())
            .show(ctx, |ui| {
                // The visible region is loosened a ring beyond the panel on each side, so the
                // corner stroke and the shadow don't get cut into hard edges.
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
                            ui.label(RichText::new(aox::i18n::t("History")).color(P::text_dim()).size(13.0));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if flat_button(ui, aox::i18n::t("Refresh")).clicked() {
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
                                    aox::i18n::t("No history yet.\nAfter your first message, the conversation is saved here automatically."),
                                )
                                .color(P::text_faint())
                                .size(13.0),
                            );
                            ui.add_space(10.0);
                            // A directory path can be long; use truncate so it closes itself off
                            // within the panel width instead of being cut by the panel boundary.
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

                        // Height **hugs the content** (the second component of `auto_shrink`), with
                        // only an upper cap: with few records the panel is short, and no empty
                        // stretch trails below the last row.
                        //
                        // ⚠️ Don't change this to "height from the row count" (`rows * row
                        // height`). The previous version did exactly that, but that product is the
                        // height of **the rows alone**, while the scroll area computes its own
                        // margins on top -- the estimate doesn't match what the panel really looks
                        // like, and a one-record panel ends up with twenty-odd logical pixels of
                        // blank underneath it (only a screenshot comparison showed it).
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
        // Record this frame's actual height -- that is next frame's slide-in distance (see the
        // `hist_h` field). This is **not** gated on `hist_t`: the position offset doesn't affect
        // the `rect`'s height, so recording it at any time is accurate.
        self.hist_h = panel.height().max(96.0);

        // Clicking outside the panel, or pressing Esc, collapses it. The button's own click
        // doesn't count as "outside" (see `bar_hist_rect`).
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

    /// A fragment of a tool call the model is **still writing**: open its row once, then keep the
    /// reading next to it climbing as the fragments arrive.
    ///
    /// Without this the entire stretch in which the model writes a call is invisible — the tool
    /// itself has not started, so there is nothing on screen to look at, and for `write_file`
    /// (whose arguments *are* the file) that stretch is the whole job.
    fn stream_tool_delta(&mut self, index: u64, name: String, delta: String) {
        if self.live.is_none() {
            return;
        }
        // One entry per call, keyed by the API's own call index: several calls may be in flight at
        // once and their fragments interleave.
        let slot = match self.stream_calls.iter().position(|c| c.index == index) {
            Some(i) => i,
            None => {
                self.stream_calls.push(StreamCall {
                    index,
                    name: String::new(),
                    args: String::new(),
                    step: usize::MAX,
                });
                self.stream_calls.len() - 1
            }
        };
        // The name only rides on the first fragment of a call; the fragments after it have to
        // inherit it, or the row would be nameless.
        if !name.is_empty() {
            self.stream_calls[slot].name = name;
        }
        self.stream_calls[slot].args.push_str(&delta);

        let call_name = self.stream_calls[slot].name.clone();
        if call_name.is_empty() {
            return; // No name yet, so there is no row to draw -- wait for the fragment that has it.
        }
        // Only the host's file writer has a reading to show: its arguments carry the text itself,
        // so "how many lines have arrived" is meaningful. Everything else gets a bare marquee.
        let lines = if call_name == WRITE_FILE_TOOL {
            aox::llm::partial_json_lines(&self.stream_calls[slot].args, "content")
        } else {
            0
        };
        // The path is the first thing in the arguments, so it is normally in hand within a fragment
        // or two — and naming the file is most of what this row has to say. Only the whole value is
        // taken: a half-typed `E:/exam` would read as a glitch rather than as progress.
        let subject = if SUBJECT_TOOLS.contains(&call_name.as_str()) {
            aox::llm::partial_json_string(&self.stream_calls[slot].args, "path")
        } else {
            None
        };
        let fresh = self.stream_calls[slot].step == usize::MAX;
        let step = self.stream_calls[slot].step;
        let plugin = self.plugin_of(&call_name);
        let Some(l) = self.live.as_mut() else { return };
        let step = if fresh {
            l.steps.push(StepView {
                name: call_name.clone(),
                plugin,
                subject: subject.clone(),
                // Left empty until the call really runs: the row shows no arguments (see
                // `step_row`), and what the model eventually sends is filled in by the `ToolCall`
                // arm, which adopts this row.
                args: String::new(),
                result: None,
                diff: None,
                // Not known until the call returns, and there is nothing to unfold before that.
                diff_detail: None,
                expanded: false,
            });
            l.steps.len() - 1
        } else {
            step
        };
        self.stream_calls[slot].step = step;
        if let Some(st) = l.steps.get_mut(step) {
            // Either reading may land a fragment after the row does — keep the row in step with
            // whatever has arrived.
            if subject.is_some() {
                st.subject = subject;
            }
            // Only ever the added side: how many lines are *disappearing* cannot be known until the
            // old file is read, which happens when the call runs.
            if lines > 0 {
                st.diff = Some((lines, 0));
            }
        }
        if fresh {
            // Name the tool that is working. "Thinking" would be plainly wrong once the model is
            // visibly writing a call.
            let want = aox::tf!("Running {}", call_name);
            l.status = want.clone();
            self.status = want;
        }
    }

    /// The model is producing text again, so no tool can be running: put the status line back to
    /// "Thinking".
    ///
    /// Called on every delta, hence the comparison before anything is built — this costs a string
    /// compare in the common case and allocates nothing.
    fn status_back_to_thinking(&mut self) {
        let want = aox::i18n::t("Thinking");
        if self.status != want {
            self.status = want.to_string();
            if let Some(l) = self.live.as_mut() {
                l.status = want.to_string();
            }
        }
    }

    fn on_event(&mut self, ev: AgentEvent) {
        // The activity stream records only "how a turn ran": events outside a turn (switching
        // conversations, the tail left behind after a fatal error, ...) never go on the
        // timeline.
        let live = self.live.is_some();
        match ev {
            AgentEvent::AssistantText(t) => {
                match self.live.as_mut() {
                    Some(l) => {
                        let at = self.clock.now();
                        // The model starting to emit body text = the "model" segment starts (if it
                        // hasn't already)
                        activity::lock(&self.act).model_begin(at);
                        if let Some(prev) = l.pending_final.replace(t.clone()) {
                            l.interim.push(prev);
                        }
                        // The deltas are done, so reconcile against the authoritative text once: if a
                        // frame is dropped while typing, the characters on screen end up a few off the
                        // real text, and that has to be smoothed out at the very end.
                        l.streaming = t;
                    }
                    None => self.items.push(Item::Assistant(t)),
                }
            }
            AgentEvent::InterimText(t) => {
                if let Some(l) = self.live.as_mut() {
                    let at = self.clock.now();
                    activity::lock(&self.act).model_begin(at);
                    // The stage note is final here: it goes into `interim` (which stays in the live
                    // area and ends up in the process card afterwards) and `streaming` is cleared --
                    // the next stretch of body text starts typing from a fresh line.
                    l.streaming.clear();
                    l.interim.push(t);
                    // Record "how many steps ran before this sentence": the live area uses it to
                    // draw the preceding calls as their own card.
                    l.cuts.push(l.steps.len());
                }
            }
            AgentEvent::ToolCall { name, args } => {
                let plugin = self.plugin_of(&name);
                // Read the file name off the parsed arguments *before* they are flattened to text --
                // this is the authoritative reading, and the streaming one (which has to decode a
                // half-arrived string) is only ever a preview of it.
                let subject = subject_from_value(&name, &args);
                let args = compact_json(&args);
                // The model has just finished writing this call, so its row is normally already on
                // screen (opened by the streaming fragments — see `stream_tool_delta`). Adopt that
                // row rather than pushing a second one for the same call; `position` takes the first
                // unclaimed entry, which is the right one when a response calls the same tool twice.
                let mut adopted = false;
                if let Some(i) = self
                    .stream_calls
                    .iter()
                    .position(|c| c.name == name && c.step != usize::MAX)
                {
                    let step = self.stream_calls[i].step;
                    if let Some(l) = self.live.as_mut() {
                        if let Some(st) = l.steps.get_mut(step) {
                            st.args = args.clone();
                            // The streaming reading of the path is a preview; this one wins.
                            if subject.is_some() {
                                st.subject = subject.clone();
                            }
                            adopted = true;
                        }
                    }
                    // The live reading (`diff`) is deliberately left in place: there is no reason to
                    // blank it for the moment between "model finished writing" and "tool returned".
                    if adopted {
                        self.stream_calls.remove(i);
                    }
                }
                if !adopted {
                    if let Some(l) = self.live.as_mut() {
                        l.steps.push(StepView {
                            name: name.clone(),
                            plugin: plugin.clone(),
                            subject,
                            args: args.clone(),
                            result: None,
                            // Unknown until the call returns; filled in by the `ToolResult` arm.
                            diff: None,
                            diff_detail: None,
                            expanded: false,
                        });
                    }
                }
                if live {
                    // Only one bar is opened on the activity stream: call -> execute -> return is a
                    // single thing, and splitting it into three makes it impossible to see how long
                    // the step actually took.
                    activity::lock(&self.act).tool_begin(
                        &name,
                        plugin.as_deref().filter(|p| *p != "meta"),
                        &args,
                        self.now(),
                    );
                }
            }
            AgentEvent::ToolRunning { name, timeout_secs } => {
                self.status = aox::tf!("Running {}", name);
                if live {
                    activity::lock(&self.act).tool_note(&aox::tf!("Timeout limit {}s", timeout_secs));
                }
            }
            AgentEvent::ToolResult { name, is_error, preview, diff, diff_detail } => {
                if let Some(l) = self.live.as_mut() {
                    if let Some(st) =
                        l.steps.iter_mut().rev().find(|s| s.name == name && s.result.is_none())
                    {
                        st.result = Some((is_error, preview.clone()));
                        st.diff = diff;
                        // What the row can be unfolded into. The row is drawable-with-a-caret from
                        // this instant on, which is the same instant the "+N / −N" lands.
                        st.diff_detail = diff_detail;
                    }
                }
                if live {
                    activity::lock(&self.act).tool_end(is_error, &preview, self.now());
                }
            }
            AgentEvent::Info(m) => {
                self.note_act(ActKind::Info, aox::i18n::t("Info"), &m);
                if self.live.is_none() {
                    self.items.push(Item::Note(NoteKind::Info, m));
                }
            }
            AgentEvent::Warn(m) => {
                self.note_act(ActKind::Warn, aox::i18n::t("Warning"), &m);
                if self.live.is_none() {
                    self.items.push(Item::Note(NoteKind::Warn, m));
                }
            }
            AgentEvent::Recycled(n) => {
                // Two audiences, two shapes (the same split as `Images` below): the activity stream
                // gets a bar — "how this turn ran" — and the human gets a **toast** that fades on
                // its own.
                //
                // The toast is set unconditionally, not only while a turn is running: a delete the
                // user asked for can easily finish after the turn that ordered it has ended, and
                // "where did my file go" is precisely the moment this has to be visible.
                self.note_act(ActKind::Info, aox::i18n::t("Recycle bin"), &aox::recycle::notice(n));
                // One `remove` call takes one path, so "delete two files" arrives as two events.
                // Merge the new one into the total already on screen — two flashes of "1 file"
                // read as a bug, and the number is the part the user is actually looking at.
                // Restarting the timer keeps the merged toast up long enough to be read.
                match &mut self.toast {
                    Some((count, shown_at)) if shown_at.elapsed().as_secs_f32() < TOAST_SECS => {
                        *count += n;
                        *shown_at = Instant::now();
                    }
                    _ => self.toast = Some((n, Instant::now())),
                }
            }
            AgentEvent::Images(paths) => {
                // Images produced by a tool: **drawn right here**.
                //
                // Before this, the flow had an ending that made no sense: the model would say "the
                // image is ready, just look at the one above", and on the human side there was
                // nothing -- the image went into **the request sent to the model**, and the UI only
                // drew the accompanying sentence as a tiny dot. So the model could see it and the
                // person couldn't; of the two front ends, only the graphical one had this problem.
                self.items.push(Item::Images(paths));
                self.force_follow = true;
            }
            AgentEvent::Error(m) => {
                self.note_act(ActKind::Error, aox::i18n::t("Error"), &m);
                if self.live.is_none() {
                    self.items.push(Item::Note(NoteKind::Error, m));
                }
            }
        }
    }

    /// A turn ends: collapse the process into one row, then paste the final answer beneath it.
    fn finish_turn(&mut self, aborted: bool) {
        self.busy = false;
        self.waiting_auth = false;
        self.auth = None;
        self.auth_reply = None;
        // At the end of a turn there shouldn't be an option slot hanging around (the model has
        // moved on). If there is, the UI failed to clean up -- so drop it too: the peer isn't
        // waiting any more (that turn's `choose()` returned long ago) and `tx.send` fails
        // silently. Keeping it is actually dangerous: if someone calls `answer_choice` before the
        // next `ChoiceRequest`, that lands on a channel nobody is listening to, and the UI looks
        // like "I clicked it and nothing happened".
        self.choice = None;
        self.choice_reply = None;
        self.status = aox::i18n::t("Ready").into();
        let Some(mut l) = self.live.take() else {
            self.stream_calls.clear();
            return;
        };
        // A call the model was still writing when the turn ended never ran — the response was cut
        // off, or the turn was aborted. Its row has to go, otherwise the process card keeps a
        // marquee spinning forever for a step that never happened.
        //
        // They are always the *trailing* rows (a streaming row is appended at the tail, and a row
        // stops streaming once its `ToolCall` arrives), so one truncation removes exactly them.
        if !self.stream_calls.is_empty() {
            let first = self
                .stream_calls
                .iter()
                .map(|c| c.step)
                .filter(|s| *s != usize::MAX)
                .min();
            if let Some(first) = first {
                l.steps.truncate(first);
            }
            self.stream_calls.clear();
        }
        // Working time, not wall time: a turn the user sat on for ten minutes while answering a
        // question is not a ten-minute turn (see [`Clock`]).
        let elapsed = (self.now() - l.turn_base).max(0.0);

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
            // A turn ends: close every segment still open (don't leave a bar that keeps growing
            // forever, longer and longer).
            act.end_round(at);
            if aborted {
                act.note(ActKind::Warn, aox::i18n::t("Task interrupted"), &aox::tf!("Ran for {}s", format!("{elapsed:.1}")), at);
            }
            // Once closed, take this turn's new records and hand them to the agent thread to
            // write into **this conversation's file**. Hand them over rather than opening the file
            // from the UI: the handle is on the agent side, and "new conversation / rollback on
            // interrupt" also happens on that thread; this message goes to the back of the queue,
            // so the ordering is naturally right -- it can't leak into the next conversation.
            act.export_acts()
        };
        if !pending.is_empty() {
            let _ = self.tx.send(ToAgent::Act(pending));
        }
        self.force_follow = true;
    }

    // ── input area / keyboard ──

    /// Take the files dropped into the window this frame.
    ///
    /// **Anywhere in the window accepts them** (explicit user request, 2026-10-05) -- hence
    /// reading from `ctx` rather than hanging it off a widget: hanging it off a widget means
    /// only that one small hot zone works, and dropping anywhere else does nothing at all.
    ///
    /// egui reports "files dropped this frame" once per frame (`dropped_files`); reading it
    /// drains it, so nothing is double-counted. `hovered_files` is what brightens the input
    /// card's border by one step.
    fn take_dropped(&mut self, ctx: &egui::Context) {
        let (dropped, hovering) = ctx.input(|i| (i.raw.dropped_files.clone(), !i.raw.hovered_files.is_empty()));
        self.dragging_files = hovering;
        if dropped.is_empty() {
            return;
        }
        for f in dropped {
            // Ones with no local path (some platforms drag in-memory data) have to be given up --
            // attachments are read by path
            let Some(path) = f.path else {
                self.items.push(Item::Note(
                    NoteKind::Warn,
                    aox::tf!("Dropped \"{}\" has no local path and cannot be used as an attachment.", f.name),
                ));
                continue;
            };
            match aox::attach::Attachment::probe(&path) {
                // The same file dropped twice = one attachment (don't show two identical chips)
                Ok(a) if self.pending.iter().any(|p| p.path == a.path) => {}
                Ok(a) => self.pending.push(a),
                // State the real reason it was rejected (directory / unreadable / image over
                // 32 MiB), don't just say "failed"
                Err(e) => self.items.push(Item::Note(NoteKind::Warn, e)),
            }
        }
    }

    /// The pending-upload bar -- the row of small chips **above** the input card, one per
    /// attachment.
    ///
    /// Over-long names are **elided in the middle** (`a very long name...shot.png`): the
    /// extension is the single most useful bit here (one glance tells you image or document), so
    /// cut the middle and keep the tail. Paths are never shown -- an `E:\...\...\...` prefix
    /// would blow the chip apart, and the user knows where they dragged it from.
    /// The row of clickable options above the input box (asked by the model via `ask_user`).
    ///
    /// Same place and same language as the pending-upload bar (small rounded blocks + a thin
    /// stroke), because they answer the same class of question: "what is the user about to give
    /// me". The only difference is that one gives files and the other gives a decision.
    ///
    /// **Vertical**: A / B / C ... one per row, and the last one is always "type your own".
    /// The first version was horizontal, and one run on real hardware gave it away: with many
    /// options the last one got squashed into a 28px-wide sliver (one character per line) and
    /// **clicking it did nothing** -- the hit zone deformed along with the squashed content, so
    /// what was drawn and what could be clicked no longer matched (the user reported both
    /// problems together on 2026-10-06).
    /// Vertical also settles "read it in one sweep": you read an option and pick it, whereas
    /// horizontal makes you hunt left and right.
    fn choice_bar(&mut self, ui: &mut Ui) {
        // Clone rather than drawing through `self.choice`: the last row needs
        // `&mut self.choice_text`, which together with "read the question and options" is two
        // borrows of the same self -- the struct is tiny, so cloning is the least trouble.
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
                // The question is copied verbatim: rewording it means it is no longer what the
                // model asked, and that is precisely what the user has to answer
                ui.label(RichText::new(&card.question).color(P::text()).size(14.5));
                ui.add_space(9.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    for (i, o) in card.options.iter().enumerate() {
                        if ask_row(ui, w, i, o) {
                            picked = Some(i);
                        }
                    }
                    // The last row is **the input box itself** (see `ask_input_row`): the place to
                    // answer is right beside the question, so the eyes don't have to run back and
                    // forth between two boxes.
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

        // When an image is attached and the current model probably has no vision, give a hint
        // that **can be ignored**.
        //
        // Deliberately not blocked: what the user wants is "send it anyway, let the endpoint
        // error and the model relay that" (see the `attach` module docs). This sentence only
        // moves "you'll mysteriously get a 400 in a while" forward to "you can see it right
        // now".
        if self.pending.iter().any(|a| a.kind.is_some()) && !self.model.to_ascii_lowercase().contains("flash") {
            ui.add_space(5.0);
            ui.label(
                RichText::new(aox::tf!(
                    "Current model {}: on the DeepSeek side only deepseek-flash supports image input (sending it anyway is fine; the endpoint will report an error).",
                    self.model
                ))
                .color(P::warn())
                .size(12.5),
            );
        }
        // Deletion waits until the loop is done: `iter()` still borrows `self.pending`
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

        // ── an option slot is up: this sentence is the **answer** to that question, not a new
        // turn ──
        //
        // Getting this wrong is hard to debug: the user types and hits send while the agent
        // thread is blocked in `choose()` -- the `Input` just queues up in the channel and it
        // looks like "I typed and it ignored me, and the option slot is still sitting there".
        // After answering, the input box clears and the option slot folds away, and that turn
        // carries on.
        if self.choice.is_some() {
            if !text.is_empty() {
                self.input.clear();
                self.answer_choice(ChoiceReply::Text(text));
            }
            return;
        }

        // Attachments with no text still count as a message -- dragging an image in and hitting
        // enter is the most common usage.
        if (text.is_empty() && self.pending.is_empty()) || self.busy || self.waiting_auth {
            return;
        }
        if self.boot.as_ref().is_some_and(|b| !b.provider_ready) {
            return;
        }
        let files: Vec<PathBuf> = self.pending.iter().map(|a| a.path.clone()).collect();
        let names: Vec<String> = self.pending.iter().map(|a| a.name.clone()).collect();
        // The timeline records only "how many attachments were sent": that is a glanceable
        // ledger, not a file listing (the real record of attachments is in the conversation
        // file, see the path list in `attach::to_content`).
        let label = if names.is_empty() {
            text.clone()
        } else if text.is_empty() {
            aox::tf!("({} attachments)", names.len())
        } else {
            aox::tf!("{} ({} attachments)", text, names.len())
        };
        self.items.push(Item::User(text.clone(), names));
        // Images dragged in by the user are shown **on the spot**, without waiting for the
        // host's acknowledgement to make a round trip.
        let shown: Vec<PathBuf> = files.iter().filter(|p| looks_like_image(p)).cloned().collect();
        if !shown.is_empty() {
            self.items.push(Item::Images(shown));
        }
        self.input.clear();
        self.pending.clear();
        self.force_follow = true;
        // Open a turn: the top "input" row on the timeline starts from this moment
        activity::lock(&self.act).round_begin(&label, self.now());
        let _ = self.tx.send(ToAgent::Input(text, files));
    }

    fn stop(&mut self) {
        if self.busy {
            self.flags.request_abort();
            self.status = aox::i18n::t("Interrupting…").into();
        }
    }

    /// Hand the user's answer back to the agent thread and fold the option slot away.
    ///
    /// This is the **only** place that sends a `ChoiceReply`: all three answering paths
    /// (clicking an option, typing and sending, and any hotkey added later) go through it, so
    /// there can be no "one path forgot the reply and the model waits forever".
    fn answer_choice(&mut self, reply: ChoiceReply) {
        if let Some(tx) = self.choice_reply.take() {
            let _ = tx.send(reply);
        }
        self.choice = None;
        self.choice_text.clear();
        // The answer was just given and that turn is still running (the model has more to do),
        // so the state goes back to "thinking" rather than staying on "waiting for you" and
        // making it look like it is still waiting.
        self.status = aox::i18n::t("Thinking").into();
    }

    /// Finishing and queueing image decoding: once per frame.
    ///
    /// It lives here rather than "at the moment an Item is created" for two reasons:
    /// (1) Items come from two sources (history replay and live events) and
    ///     `items_from_history` is a free function with no `self`, so queueing on both sides
    ///     would mean writing the same thing twice;
    /// (2) one sweep is **idempotent** -- a path that already has an entry isn't queued again,
    ///     so no convention like "remember to call this after step X" is needed.
    fn poll_images(&mut self, ctx: &egui::Context) {
        // ① Collect: finished decodes become textures (`try_recv` doesn't block the UI)
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
                // Upload parameters live in one place, `crate::tex` (which also spells out why
                // getting them wrong doesn't raise an error)
                Ok(bm) => Tex::Ready(crate::tex::upload(
                    ctx,
                    // The texture name uses only the file name: it is a debugging label, and a
                    // whole path is long and full of backslashes
                    format!("aox-img:{}", p.file_name().unwrap_or_default().to_string_lossy()),
                    &bm,
                )),
                // Failures are cached too: otherwise every frame would spin up another decode
                // thread for the same broken image
                Err(e) => Tex::Failed(e),
            };
            self.tex.insert(p, entry);
        }

        // ② Queue: ones not yet queued go into the queue
        let mut want: Vec<PathBuf> = Vec::new();
        for it in &self.items {
            if let Item::Images(paths) = it {
                for p in paths {
                    // Deduplicate (the same image can be listed twice in one message), and pick
                    // only the ones that look like images
                    if looks_like_image(p) && !self.tex.contains_key(p) && !want.contains(p) {
                        want.push(p.clone());
                    }
                }
            }
        }
        // At most 4 decode threads per frame: a screen with twenty images shouldn't spin up
        // twenty threads at once, and even less should it unfurl twenty decoded bitmaps
        // simultaneously (that can run to gigabytes). The rest are queued on the next frame --
        // which costs nothing visually, since the user only looks at one or two at a time anyway.
        let queued = want.len();
        for p in want.into_iter().take(4) {
            let (tx, rx) = std::sync::mpsc::channel();
            let path = p.clone();
            // Decoding is synchronous and can take tens of milliseconds (GDI+, see `aox::image`),
            // so it must never occupy the UI thread
            std::thread::spawn(move || {
                let _ = tx.send(aox::image::decode(&path));
            });
            self.tex.insert(p, Tex::Loading(rx));
        }

        // ③ Anything still decoding or not yet queued asks for another frame -- otherwise, once
        //    the window goes quiet those images sit on "decoding..." forever (egui only repaints
        //    when there is input).
        if queued > 0 || self.tex.values().any(|t| matches!(t, Tex::Loading(_))) {
            ctx.request_repaint();
        }
    }

    // ─────────────────────────── painting ───────────────────────────

    /// The row of buttons floating above the transcript -- the old top bar (the panel itself
    /// was removed on 2026-10-04).
    ///
    /// The positions are lifted pixel for pixel from what the old top bar worked out to:
    /// `(20, 7)` at the top-left, 20 in from the right as well, with a 36px hot zone, so the
    /// button centre is y = 25 = the midline of the old 50px bar.
    ///
    /// Each button carries its own backdrop ([`P::panel`], which is the old top bar's fill), so
    /// they don't smear into the transcript text behind them. Hover raises the backdrop one step
    /// (`surface_hi`), the same hover language as list rows and menu rows.
    ///
    /// Two things differ from the old top bar:
    /// - "New conversation" and "Settings" are **both hidden by default**, sliding out to the
    ///   right from behind the history button along with the history overlay;
    /// - status and tokens changed from "an empty spot on the left of the top bar" into a chip
    ///   that travels with the buttons (with the bar gone they had to land somewhere, otherwise
    ///   task progress and the token ledger would have nowhere to be seen).
    ///
    /// **The three on the top-right (activity stream / theme / about) have all been removed**
    /// (user request, 2026-10-05); each moved into the settings dialog: activity stream ->
    /// "Extra", theme -> "Appearance", about -> "About".
    fn toolbar(&mut self, ctx: &egui::Context) {
        let mut want_new = false;
        let mut want_settings = false;
        let mut toggle_hist = false;
        let ease = ease_out_cubic(self.hist_t);
        let locked = self.busy || self.switching;

        // ── the slide-out group: "+" innermost, "Settings" outermost ──
        //
        // Each button gets its own `Area`: the slide-out clip has to cut at each button's own
        // right edge, which a shared Area can't do. `x_end - slide` is the offset that brings it
        // "out from behind the history button"; the **clip's left edge is pinned at its own end
        // position**, so while the offset is short of complete only part of a button is exposed,
        // which reads as "still behind".
        if self.hist_t > 0.0 {
            let slide = (BAR_BTN + 10.0) * (1.0 - ease);
            for (id, slot, icon) in [
                ("aox-bar-new", 0.0_f32, SlideIcon::Plus),
                ("aox-bar-settings", 1.0_f32, SlideIcon::Gear),
            ] {
                // slot 0 = right next to the history button; slot 1 = one more to the right.
                // 38 = the 36 button plus a 2px gap.
                let x_end = BAR_LEFT + BAR_BTN + 2.0 + slot * (BAR_BTN + 2.0);
                egui::Area::new(egui::Id::new(id))
                    .order(egui::Order::Foreground)
                    .fixed_pos(Pos2::new(x_end - slide, BAR_TOP))
                    // Mid-slide the position overlaps the button to its left; don't let the Area
                    // pull it back on screen.
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

        // ── top-left: the history button (position unchanged) + the status / token chip ──
        egui::Area::new(egui::Id::new("aox-bar-left"))
            .order(egui::Order::Foreground)
            .fixed_pos(Pos2::new(BAR_LEFT, BAR_TOP))
            .constrain_to(loose_rect())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if history_icon(ui, self.show_history).clicked() {
                        toggle_hist = true;
                    }
                    // "+" and "Settings" live in two other `Area`s (each needs its own slide-out
                    // clip), so room is made for them here: 38px each while expanded, nothing while
                    // collapsed. That "pushes" the token chip to the right -- exactly the knock-on
                    // effect you want from "buttons emerging from the left".
                    //
                    // 38 = the button's 36 plus the 2px gap between them. Without that stretch the
                    // chip sits right on top of the button: they share a backdrop colour, so on real
                    // hardware it looks like the button and the chip share one fill, and the button
                    // appears not to have grown out at all (only a screenshot comparison caught it;
                    // a glance by eye lets it through).
                    ui.add_space(8.0 + 38.0 * 2.0 * ease);
                    // With not a single token and no turn running, even the chip's shell shouldn't
                    // appear (`token_chip` draws nothing then, and forcing a Frame around it would
                    // leave an empty box).
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
                                        // Working time, same basis as every other readout (see
                                        // [`Clock`]) -- otherwise the number here would keep
                                        // climbing while the model sits waiting on the user.
                                        let secs = self
                                            .live
                                            .as_ref()
                                            .map_or(0.0, |l| (self.now() - l.turn_base).max(0.0));
                                        ui.label(
                                            RichText::new(format!("{secs:.1}s"))
                                                .color(P::text_faint())
                                                .size(13.5),
                                        );
                                        // The running total = completed turns (exact) + this turn
                                        // (estimated). So the whole conversation's bill climbs during a
                                        // turn instead of resetting each turn. The `~` is there because
                                        // the second half is still an estimate; once the turn ends the
                                        // exact total takes over.
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
                                        // When idle only the total token count remains; hover for the
                                        // breakdown
                                        self.token_chip(ui, ctx);
                                    }
                                });
                            });
                    }
                });
            });
        // Record the rect of this row of buttons: the history overlay needs it to rule out "the
        // click landed on a button" (otherwise the button's toggle is cancelled on the spot by
        // the overlay's own "click outside collapses it").
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

    /// The "total tokens" chip: **hovering** expands the total, no click needed.
    ///
    /// Hover rather than click, because this number is a glance-at thing by nature: whoever wants
    /// to check just moves the mouse over it, whereas a click would first require them to realise
    /// "this is clickable".
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
        // The panel sits 6px directly below the chip: as the pointer travels from chip to panel
        // it is always over one of the two, so no frame falls through the gap and nothing
        // flickers.
        //
        // ⚠️ **Do not convert coordinates here**: `Area::fixed_pos` and `ui.allocate_*` are both
        // in **client-area local coordinates** (`Area::begin` uses `ctx.content_rect()` as the
        // constraint). This once had a "viewport origin" term, `viewport().inner_rect.left_top()`,
        // added -- that is **screen (monitor) coordinates**, a completely different system, and
        // the conversion is meaningless; the result was the panel shifting wholesale with the
        // window's position on the monitor (measured ~(154, 14) logical pixels off when the
        // window wasn't at (0,0)). The same mistake was made in `workplace_ui::menu` and has been
        // fixed there too.
        let anchor = resp.rect.left_bottom() + egui::vec2(0.0, 6.0);
        // Separate borrows: the closure writes hover state and reads the ledger
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

    /// The cumulative token count for the whole conversation (the number the Σ shows).
    ///
    /// Taken straight from `session_total` in the last record -- that is a running total written
    /// at save time, so the whole ledger doesn't have to be recomputed each time.
    /// The **totals** across this conversation's turns (summed field by field).
    ///
    /// Both the top bar and the expanded panel use it -- one number says "how much was spent",
    /// the whole set says "where it went".
    fn usage_sum(&self) -> Usage {
        sum_usage(&self.usage_log)
    }

    /// Put each turn's breakdown from the ledger back into the transcript, after that turn's AI
    /// reply.
    ///
    /// Location relies on the natural boundary "every user message starts a turn": turn N's
    /// breakdown falls between the Nth user message and the next one (or at the end if there
    /// isn't one). That way, opening an old conversation puts every turn's bill back where it was,
    /// instead of heaping them all at the top.
    fn rebuild_usage_rows(&mut self) {
        self.items.retain(|it| !matches!(it, Item::Usage(_)));
        if self.usage_log.is_empty() {
            return;
        }
        // First work out the end index of each turn's content
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
        // Insert in reverse: later turns first, so earlier turns' indexes don't get skewed
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
                // Top padding = the old top bar's 50 + the original 26. With the bar gone the
                // buttons float above the content, and without the padding the first line of text
                // butts right up under them.
                ui.add_space(76.0);
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    ui.vertical(|ui| {
                        ui.set_max_width(inner);
                        ui.set_min_width(inner);
                        // The measure's left/right bounds: width = inner, wrapping automatically at
                        // the right edge.
                        //
                        // egui's `Ui::wrap_mode()` returns `Extend` inside a "non-wrapping horizontal
                        // layout" and `Label` inherits that by default -- so long text runs to the
                        // end of the line and gets clipped past the right edge. Bubbles, list items
                        // and tool-call rows are all such horizontal layouts internally, so it is
                        // nailed down once at the column root and every descendant Ui (including
                        // inside a Frame) inherits `Wrap`.
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                        // Until the real conversation starts, show the first screen (there may
                        // already be loading / error notices in `items` at this point). The same
                        // predicate decides whether the background gets painted, in `App::update`
                        // -- one function, so "the image is up" and "the first screen is up" cannot
                        // drift apart.
                        if self.on_empty_state() {
                            self.empty_state(ui);
                        }
                        // Image rendering has to consult the texture cache. `self.items` and
                        // `self.tex` are **two unrelated fields**, so take a reference that borrows
                        // only `tex` first, and borrowing `items` in the loop no longer conflicts
                        // (borrowing all of `&mut self` would not work).
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
                        if self.live.is_some() {
                            self.live_block(ui, inner);
                        }
                        // Reserve the floating input box's height at the end, so the last message
                        // can scroll above it instead of being permanently half-covered.
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

    /// Is the first screen (nothing said yet) the thing on show?
    ///
    /// This decides two things that must agree: whether the transcript draws the first screen, and
    /// whether the background image is painted behind it. Notices that arrived before any message
    /// (a load failure, "model not ready") do **not** end the first screen -- they are the first
    /// screen's content, not a conversation.
    fn on_empty_state(&self) -> bool {
        self.live.is_none()
            && !self
                .items
                .iter()
                .any(|i| matches!(i, Item::User(..) | Item::Assistant(_)))
    }

    fn empty_state(&mut self, ui: &mut Ui) {
        // What used to be here was the illustration itself. It is now a **full-bleed background**
        // painted behind the whole column (`App::update` -> `brand::show_background`), so there is
        // nothing left to reserve space for or centre: the empty state is just the "model not
        // ready" card, or nothing at all. The old "Hi there" line and the preset examples beneath
        // it were removed at the user's request (2026-10-06) -- the empty state no longer presumes
        // to think of what the user might say.
        //
        // A background that failed to load is reported *here* rather than where it was painted:
        // this is where there is room for a line of text in the column, and "a file in the folder
        // isn't really a PNG" is otherwise invisible -- the first screen would simply be plain, at
        // random, which reads as "the feature is flaky".
        if self.log_error.is_none() {
            if let Some(e) = self.brand_err.clone() {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(aox::tf!("The background image could not be loaded: {}", e))
                            .color(P::text_faint())
                            .size(13.0),
                    );
                });
            }
            return;
        }

        // With an error card (model not configured) the card is **not** centred: the single most
        // important thing on screen then is that error, and centring it would push it down out of
        // sight on a short window. (This 8.0 is what the illustrated version used in the same case,
        // so the card has not moved.)
        ui.add_space(8.0);
        ui.vertical_centered(|ui| {
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
                                    RichText::new(aox::i18n::t("Model not ready"))
                                        .color(P::on_reverse())
                                        .size(14.0),
                                );
                            });
                        ui.add_space(10.0);
                        ui.label(RichText::new(err).color(P::text_dim()).size(14.5));
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(aox::tf!("Config file: {}", self.config_path))
                                .color(P::text_faint())
                                .size(13.5),
                        );
                        ui.label(
                            RichText::new(aox::i18n::t("Fill in base_url / api_key under [provider], then restart."))
                                .color(P::text_faint())
                                .size(13.5),
                        );
                    });
            }
        });
    }

    fn live_block(&mut self, ui: &mut Ui, width: f32) {
        let _ = width;
        // Taken off `self` before the live state is borrowed below: a call row can now be unfolded,
        // which needs the step list `&mut`, and `self.now()` would be a second, conflicting borrow of
        // the whole struct.
        let spin = self.spin;
        let now = self.now();
        let Some(l) = self.live.as_mut() else { return };
        // The live area is cut by **stage**: a stretch of call stream -> one interim reply ->
        // another stretch of call stream -> ...
        // (user request, 2026-10-05: previously all the calls were crammed at the top and all
        // the replies in the middle, so you couldn't tell which steps belonged to which stage.)
        //
        // `cuts[i]` = the number of steps already run when interim reply i was spoken, so call
        // stretch i is `steps[cuts[i-1] .. cuts[i]]`, and the last one runs to `steps.len()`.
        for seg in 0..=l.interim.len() {
            let from = if seg == 0 { 0 } else { l.cuts.get(seg - 1).copied().unwrap_or(0) };
            let to = l.cuts.get(seg).copied().unwrap_or(l.steps.len()).min(l.steps.len());
            if from < to {
                calls_card(ui, &mut l.steps[from..to]);
                ui.add_space(14.0);
            }
            if let Some(say) = l.interim.get(seg) {
                live_plain_text(ui, say, P::text_dim());
                ui.add_space(14.0);
            }
        }
        // The sentence being typed goes last: it doesn't belong to any stage yet (only once
        // it's finished do you know whether more calls follow).
        if !l.streaming.trim().is_empty() {
            live_plain_text(ui, &l.streaming, P::text());
            ui.add_space(14.0);
        }

        // waiting animation + status
        ui.horizontal(|ui| {
            spinner_dot(ui, spin, P::ink());
            ui.add_space(8.0);
            ui.label(
                RichText::new(if l.status.is_empty() { aox::i18n::t("Thinking") } else { &l.status })
                    .color(P::text_dim())
                    .size(15.0),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!("{:.1}s", (now - l.turn_base).max(0.0)))
                    .color(P::text_faint())
                    .size(13.0),
            );
            // The output token count scrolls live. The `~` is because the server hasn't reported
            // during streaming and this value is estimated from characters; once the turn ends the
            // exact total takes over.
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

        // The reasoning box: **fixed height and always present** (user, 2026-10-05: "the
        // reasoning box at the bottom keeps growing and shrinking, which makes the whole screen
        // jump up and down"). There were two sources of the resizing, both cured at once here:
        //   ① during streaming it grew from one line to four;
        //   ② at the end of every response `reasoning_live` went false and the text was cleared,
        //      so the whole block flickered in and out.
        // So the box is drawn once with a hard-coded height, and the text is drawn inside it and
        // clipped to the box. The price is that non-reasoning models see this empty box too --
        // trading "a stable patch of blank" for "the whole block bouncing up and down"; stable
        // is worth more.
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
            // Clip first, then draw: text past four lines gets cut off rather than pushing the
            // box taller.
            let p = painter.with_clip_rect(inner);
            let g = p.layout(tail, theme::body_font(14.5), P::text_faint(), inner.width());
            p.galley(inner.left_top(), g, P::text_faint());
        }
        ui.add_space(16.0);
    }

    /// Send a request produced by the workplace button to the agent thread.
    ///
    /// The UI can't change tiers by itself: `Workplace` lives inside `Runtime`, and only the
    /// agent thread can get at `Runtime` (see `ToAgent::Workplace` in `bridge.rs`). So all this
    /// does is "send it out"; the real result and the new tier are driven by the bridge's
    /// acknowledgement (`FromAgent::Workplace`).
    fn send_ws(&mut self, mode: Mode) {
        if self.demo {
            // Demo mode has no agent thread to acknowledge, so simulate it in place -- that way
            // `--demo` can also show "switch tier -> button renames + the tick in the list
            // follows". UI only, no disk involved.
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
        // While an option slot is up, **the meaning of send changes**: it means "this is my
        // answer" rather than starting a new turn (that turn is running, so `busy` is true). So it
        // has to be let through here -- otherwise the user is looking at a row of options, wants
        // to type their own clearer answer, and finds the send key greyed out, leaving only
        // options that all miss the point.
        let choosing = self.choice.is_some();
        // Attachments with no text may be sent too (dragging an image in and pressing send is the
        // most common usage).
        // This does not apply while answering: then you really have to type; a mere attachment is
        // not an answer.
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

                // The option slot: a multiple-choice question the model asked via `ask_user`. It
                // goes **above** the pending-upload bar -- it is more urgent than "what am I sending
                // this time" (until the user answers, this turn doesn't move).
                self.choice_bar(ui);

                // The pending-upload bar: the row of small chips **above** the input card (the
                // position the user explicitly asked for on 2026-10-05).
                self.attach_bar(ui);

                // The floating input box: no separator line and no fill around it, floating above
                // the transcript on a large corner radius + a thin stroke + a faint shadow (which is
                // where the "hollowed-out background" look comes from).
                Frame::NONE
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(20))
                    .stroke(Stroke::new(
                        if self.dragging_files { 1.5 } else { 1.0 },
                        // While files are being dragged over the window, brighten the stroke one step:
                        // where it will land on release has to be visible
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
                        // Note: ui.horizontal_centered() can't be used here -- it requests "the full
                        // available size" in one go, which makes the auto-height bottom bar panel keep
                        // inflating (measured: it swallows the whole window). ui.horizontal centres on
                        // the cross axis just the same, but its initial height is a single row, so
                        // there is no feedback loop.
                        ui.horizontal(|ui| {
                            let hint = if self.demo {
                                aox::i18n::t("Demo mode: layout preview only (start with --demo; run normally to chat)")
                            } else if blocked {
                                aox::i18n::t("Model not configured; chat unavailable")
                            } else if self.waiting_auth {
                                aox::i18n::t("Waiting for authorization…")
                            } else if choosing {
                                // Point at the inline input box **on the row above**: the place to
                                // answer is beside the question, and people shouldn't be directed to the
                                // box below (explicit user request, 2026-10-06)
                                aox::i18n::t("Click one of the options above, or type in that row…")
                            } else if self.busy {
                                aox::i18n::t("Running; click Stop on the right")
                            } else {
                                aox::i18n::t("Tell AOX what to do…")
                            };
                            // Explicitly subtract the send button's width so the input box can't push
                            // the button out of the card
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

                        // Second row: the workplace button at the input box's **bottom-left**.
                        //
                        // On its own row rather than squeezed into the previous one: it says "under
                        // what permissions this conversation runs", which is a different thing from
                        // "type + send"; squeezing them together narrows the input box, and this
                        // button sitting right next to the round send button also invites misclicks
                        // (one merely "sends a message", the other "drops all restrictions").
                        //
                        // In demo mode it **can be clicked** (`self.demo || ...`): the list /
                        // confirmation / directory picker it opens are pure UI, so simulating them in
                        // place is enough to show the layout (see `send_ws`).
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            workplace_ui::button(ui, &mut self.ws, self.demo || !blocked);
                        });
                    });
            });
        });
    }

    /// Settings: one large centred card, pick on the left, read on the right.
    ///
    /// It replaces the three buttons that used to be at the top-right (activity stream / theme /
    /// about) and that 380px "about" card -- "delete all three buttons and fold them into one
    /// settings dialog" was the user's request on 2026-10-05.
    /// So three of the four columns are **relocations**: "Extra" takes the activity-stream
    /// entrance, "Appearance" takes the light/dark switch, "About" takes the version and the
    /// license; only "System" is newly written (those paths used to be half-hidden in the about
    /// card and are now listed properly).
    ///
    /// Clicking the background, pressing Esc, or clicking "Close" all dismiss it (the first two
    /// come from `Modal`).
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
                // Height hard-coded: switching columns shouldn't make the card grow and shrink with
                // the content -- going from "System" (a long column of paths) to "Extra" (a single
                // button) shrinking the whole block looks bad.
                ui.set_width(SETTINGS_W);
                ui.set_height(SETTINGS_H);
                ui.horizontal_top(|ui| {
                    // The horizontal spacing in this row is **entirely hand-written by the
                    // `add_space` calls below**.
                    //
                    // Without zeroing it, `item_spacing` (8 in this theme) stacks between every
                    // allocate: 8 extra between the left column and the vertical rule, and another 8
                    // between the rule and the content -- the total width quietly becomes 716, 16
                    // more than `SETTINGS_W`. So the card is 16px wider than designed, and "how wide
                    // is the right column really" goes from computable to guesswork (which is exactly
                    // why the first version measured 758.7 instead of 740).
                    ui.spacing_mut().item_spacing.x = 0.0;

                    // ── left: the columns ──
                    ui.vertical(|ui| {
                        ui.set_width(SETTINGS_NAV_W);
                        ui.set_height(SETTINGS_H);
                        for tab in SettingsTab::ALL {
                            if tab_row(ui, tab, self.settings_tab == tab).clicked() {
                                self.settings_tab = tab;
                            }
                        }
                    });

                    // A vertical rule separates "what you pick" from "what you read". Same
                    // vocabulary as the horizontal rule in "About" (1px, border colour), just turned
                    // a quarter turn.
                    //
                    // The height is **a constant** rather than `ui.available_height()`: inside a
                    // horizontal layout the latter varies with how much the left column has already
                    // taken up, so the rule would be tall and short by turns. A constant also states
                    // plainly that "this line runs the full height of the card".
                    let (rule, _) =
                        ui.allocate_exact_size(Vec2::new(1.0, SETTINGS_H), Sense::hover());
                    ui.painter().vline(
                        rule.center().x,
                        rule.y_range(),
                        Stroke::new(1.0, P::border()),
                    );
                    ui.add_space(18.0);

                    // ── right: the content ──
                    ui.vertical(|ui| {
                        ui.set_width(SETTINGS_RIGHT_W);
                        // The height is hard-coded too. The reason: `ScrollArea` takes "available
                        // height ∩ max_height", and in this horizontal layout the available height is
                        // affected by the other children on the same row (the vertical rule's allocate
                        // ate into it once already). With it hard-coded, the heights of "content + gap
                        // + bottom button" are all computable, so the card height can't vary with the
                        // column's content -- and that was the source of "the card grows and shrinks
                        // when you switch columns".
                        ui.set_height(SETTINGS_H);
                        // The `SETTINGS_FOOT_H` at the bottom is reserved for "Close": without
                        // subtracting it, long content pushes the button out of the card
                        ScrollArea::vertical()
                            .id_salt("aox-settings-body")
                            .max_height(SETTINGS_H - SETTINGS_FOOT_H)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                // ⚠️ This line is **what makes the whole dialog stable**; don't delete
                                // it.
                                //
                                // A `ScrollArea`'s inner width = outer width - the scrollbar's
                                // reservation, and that reservation is **animated frame by frame**
                                // (`animate_bool_responsive` x `spacing.scroll.allocated_width()`, 6px
                                // in this project). As long as the content width still follows
                                // `available_width()`, every time the scrollbar breathes the row width
                                // changes and wrapped text re-wraps -- which shows up as "the whole panel
                                // is jittering and the text looks blurry", and it **only happens when
                                // there is a repaint** (moving the mouse repaints, so it looks like "it
                                // jitters the moment I move the mouse").
                                //
                                // Pinned down: the content width is a constant, wrapping is stable, and
                                // the scrollbar only affects its own floating 6px bar (which floats to
                                // the right of the content and doesn't press on the text).
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
                            if flat_button(ui, aox::i18n::t("Close")).clicked() {
                                open = false;
                            }
                        });
                    });
                });
            });
        // Clicking the background or pressing Esc also counts as closing
        if resp.should_close() || !open {
            self.settings_open = false;
        }
    }

    /// The "Extra" column: the activity-stream entrance.
    ///
    /// This column deliberately holds **one entrance and no settings** -- it comes first only
    /// because the user asked for that, but it really does contain just the one thing, "open the
    /// activity stream" (which used to occupy an icon in the top-right).
    fn tab_extra(&mut self, ui: &mut Ui) {
        // Drop the lock first, then test: `is_open()` takes the lock, and `open()` in the click
        // branch takes it again -- two locks stacked in one statement is a self-deadlock (the
        // top-bar version ran into this).
        let on = activity::lock(&self.act).is_open();
        if activity_open_button(ui, on).clicked() {
            activity::open(ui.ctx(), &self.act);
        }
        ui.add_space(9.0);
        ui.label(RichText::new(aox::i18n::t("Observe the agent's inner loop")).color(P::text_faint()).size(13.0));
    }

    /// The "Appearance" column: three light/dark options (including follow system).
    ///
    /// Picking one only changes `theme::Pref`; what "follow system" actually resolves to is
    /// computed from the system theme by `theme::effective` and **rechecked every frame** (see
    /// `theme::follow_system`) -- without the recheck, "follow system" would amount to "decide
    /// once at startup by copying the system".
    fn tab_appearance(&mut self, ui: &mut Ui) {
        let cur = theme::pref();
        for p in theme::Pref::ALL {
            if choice_row(ui, p.label(), cur == p).clicked() {
                theme::apply_pref(ui.ctx(), p);
            }
        }
    }

    /// The "System" column: this machine, this config, these directories.
    ///
    /// The order follows the list the user asked for: system info -> runtime environment ->
    /// model -> config -> workplace -> history -> plugins -> runtime directory.
    fn tab_system(&self, ui: &mut Ui) {
        let p = self.boot.as_ref().map(|b| b.paths.clone()).unwrap_or_default();

        kv_row(ui, aox::i18n::t("System info"), &p.host);

        // Runtime environment: **bundled or system PATH** -- one row per language, with the
        // source written straight into brackets.
        // No ✓/✗ drawn: those two glyphs aren't guaranteed to exist in the hand-picked font (the
        // two in the call stream are hand-drawn), and the word "not found" already states the
        // conclusion clearly.
        for (i, it) in self.runtimes.items.iter().enumerate() {
            kv_row_colored(
                ui,
                if i == 0 { aox::i18n::t("Runtime environment") } else { "" },
                &format!("{}  {}  [{}]", it.lang, it.command, it.origin),
                if it.ok { P::text_faint() } else { P::warn() },
            );
        }

        if !self.model.is_empty() {
            kv_row(ui, aox::i18n::t("Model in use"), &self.model);
        }
        if !p.config.is_empty() {
            kv_row(ui, aox::i18n::t("Config file"), &p.config);
        }
        // Which layers this config is merged from: `core -> patch -> config.toml`.
        //
        // Only the name chain is shown, not the full provenance tree (for the complete table use
        // the CLI's `aox --dump-config`); **with only one layer nothing is shown** -- saying
        // "there's just one layer" amounts to saying nothing. This rule was lifted from the old
        // "about" card, not a word changed.
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
            kv_row(ui, aox::i18n::t("Config layers"), &layers.join(" → "));
        }
        if !p.root.is_empty() {
            kv_row(ui, aox::i18n::t("App root"), &p.root);
        }
        kv_row(ui, aox::i18n::t("Workspace"), &self.workplace_line());
        if !p.history.is_empty() {
            kv_row(ui, aox::i18n::t("History"), &p.history);
        }
        if !p.plugins.is_empty() {
            kv_row(ui, aox::i18n::t("Plugins directory"), &p.plugins);
        }
        if !p.runtime.is_empty() {
            // "if it's bundled" -- whether the directory is there tells you at a glance whether
            // the bundled runtime is in use
            let note = if std::path::Path::new(&p.runtime).is_dir() {
                ""
            } else {
                aox::i18n::t(" (built-in runtime not installed)")
            };
            kv_row(ui, aox::i18n::t("Runtime directory"), &format!("{}{note}", p.runtime));
        }

        // When there is no Python on this machine, offer a one-click install. It used to live in
        // the "about" card, but "About" now only covers version and license -- installing a
        // runtime is a system-level matter, so this is the right column for it.
        if self.runtimes.python_missing {
            ui.add_space(14.0);
            ui.label(
                RichText::new(aox::i18n::t("Python is missing on this machine; file I/O, system info and similar capabilities are unavailable."))
                    .color(P::text_dim())
                    .size(13.5),
            );
            ui.add_space(8.0);
            if primary_button(ui, aox::i18n::t("Install Python runtime (~10 MB)")).clicked() {
                // Goes down exactly the same command path as the CLI (handled by the host itself,
                // not sent to the model)
                let _ = self.tx.send(ToAgent::Input("/runtime install python".into(), Vec::new()));
            }
        }
    }

    /// The workplace row: "path + tier".
    ///
    /// The workplace **follows the conversation** (`/new` always goes back to the default tier),
    /// so what is shown here is **this conversation's** plot of land -- which is also why it can
    /// be shown without waiting for a disk scan: the bridge pushes the current state.
    fn workplace_line(&self) -> String {
        use aox::workplace::Mode;
        let Some(v) = self.ws.view() else {
            // The first frame or two after startup hasn't received an acknowledgement yet. Don't
            // guess a directory; say honestly that it isn't known yet.
            return aox::i18n::t("(no acknowledgement yet)").into();
        };
        let name = match &v.mode {
            Mode::Default => aox::i18n::t("Default tier").to_string(),
            Mode::Full => aox::i18n::t("Full access tier (writes unrestricted)").to_string(),
            Mode::Fixed(p) => aox::tf!("Fixed tier {}", p.display()),
        };
        match &v.root {
            Some(r) => aox::tf!("{} ({})", r.display(), name),
            // The `default` tier only opens its plot once the first message arrives. This is
            // deliberately stated separately from "unrestricted" -- two different things.
            None => aox::tf!("({} · plot not opened yet)", name),
        }
    }

    /// The "About" column: version, license, links.
    ///
    /// The license text is **the repo's own `LICENSE`, embedded directly**, not copied out by
    /// hand -- a hand-copied version that drifts from the file means what "About" shows is no
    /// longer the license actually in force, and nobody ever double-checks that kind of error.
    fn tab_about(&self, ui: &mut Ui) {
        // Masthead: AOX GUI in a serif face + the version, with a thin rule underneath (the same
        // vocabulary as the empty state)
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
            RichText::new(aox::i18n::t("AOX GUI, written in Rust. Licensed under the open-source ISC license."))
                .color(P::text_dim())
                .size(14.0),
        );

        ui.add_space(16.0);
        ui.label(RichText::new(aox::i18n::t("Full ISC license text")).color(P::text_faint()).size(12.5));
        ui.add_space(6.0);
        // The license text sits in a filled box. **Selectable**: when quoting a clause you can
        // copy it directly instead of retyping it off the screen.
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
        link_row(ui, aox::i18n::t("Author's blog"), "https://2am.top");
        link_row(ui, aox::i18n::t("Project home"), "https://github.com/forever218/AOX");
    }

    fn auth_overlay(&mut self, ctx: &egui::Context) {
        let Some(card) = self.auth.take() else { return };
        let mut decision: Option<AuthDecision> = None;
        egui::Modal::new(egui::Id::new("aox-auth")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.label(
                RichText::new(aox::i18n::t("Tool call needs authorization"))
                    .color(P::text())
                    .size(17.0)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(aox::i18n::t("The model wants to call a meta-tool that changes the system itself:"))
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
            // The keyboard shortcuts are identical to the CLI's: y allows once / a always allows
            // for this session / n denies.
            // A 250ms debounce is added: keys are ignored right after the dialog appears, so
            // residue from the previous message (its Enter, say) can't trigger it by accident.
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
                if primary_button(ui, aox::i18n::t("Allow once")).clicked() {
                    decision = Some(AuthDecision::Allow);
                }
                if flat_button(ui, aox::i18n::t("Always allow this session")).clicked() {
                    decision = Some(AuthDecision::AllowSession);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if flat_button(ui, aox::i18n::t("Deny")).clicked() {
                        decision = Some(AuthDecision::Deny);
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(aox::i18n::t("Shortcuts: y allow once · a always for this session · n / Esc deny"))
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
                    aox::i18n::t("Authorization result"),
                    match d {
                        AuthDecision::Allow => aox::i18n::t("Allow once"),
                        AuthDecision::AllowSession => aox::i18n::t("Always allow this session"),
                        AuthDecision::Deny => aox::i18n::t("Deny"),
                    },
                );
            }
            None => self.auth = Some(card),
        }
    }
    /// The transient toast: one line about something that just happened, gone a few seconds later.
    ///
    /// Why an overlay rather than an `Item` in the transcript: the transcript is a **record** — it
    /// goes to `history/*.jsonl` and is replayed when a conversation is resumed — whereas this is a
    /// passing notice that should not outlive its moment. Being able to sit *above* the transcript
    /// is the other half of the reason it is drawn from `update()` next to the other overlays.
    fn toast_overlay(&mut self, ctx: &egui::Context) {
        let Some((count, shown_at)) = self.toast else { return };
        let age = shown_at.elapsed().as_secs_f32();
        if age >= TOAST_SECS {
            self.toast = None;
            return;
        }
        // A toast has to leave **on its own**, and an idle egui window doesn't repaint by itself:
        // without this it would sit there until the mouse happened to move.
        ctx.request_repaint_after(Duration::from_millis(50));
        // Only the tail fades, so the text is fully legible for as long as it is up and then leaves
        // without a snap. The whole card fades (fill, border, shadow and text together) — fading the
        // text alone leaves an empty box hanging in the air.
        let alpha = ((TOAST_SECS - age) / TOAST_FADE_SECS).clamp(0.0, 1.0);
        // Built here, not stored: the count may still grow while the toast is on screen.
        let text = aox::recycle::notice(count);
        egui::Area::new(egui::Id::new("aox-toast"))
            .order(egui::Order::Tooltip)
            .movable(false)
            .interactable(false)
            .anchor(Align2::CENTER_TOP, Vec2::new(0.0, 18.0))
            .show(ctx, |ui| {
                Frame::NONE
                    .fill(P::toast_bg().gamma_multiply(alpha))
                    .stroke(Stroke::new(1.0, P::toast_border().gamma_multiply(alpha)))
                    .corner_radius(CornerRadius::same(TOAST_RADIUS))
                    .shadow(egui::Shadow {
                        offset: [0, 6],
                        blur: 20,
                        spread: 0,
                        color: P::shadow().gamma_multiply(alpha),
                    })
                    .inner_margin(Margin::symmetric(18, 13))
                    .show(ui, |ui| {
                        // The icon and the sentence are one row, vertically centred. The whole
                        // foreground (icon + text) is the deep red, so the notice reads as one object.
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            let ink = P::toast_text().gamma_multiply(alpha);
                            trash_glyph(ui, 20.0, ink);
                            ui.add_space(10.0);
                            ui.label(RichText::new(&text).color(ink).size(14.5));
                        });
                    });
            });
    }
}

/// A stroke-drawn trash can, sized to sit at the left of a toast line.
///
/// Hand-drawn like the other glyphs in this file (`history_icon` / `plus_icon` / `gear_icon`): the
/// project carries no icon font, and a dozen line segments on a 20×20 grid cost less than a dependency.
fn trash_glyph(ui: &mut Ui, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    // Everything below is laid out on a 20×20 grid, then scaled to `size`.
    let s = size / 20.0;
    let o = rect.min.to_vec2();
    let p = |x: f32, y: f32| Pos2::new(o.x + x * s, o.y + y * s);
    let w = Stroke::new((1.5 * s).max(1.0), color);
    let painter = ui.painter();
    // Handle, then lid.
    painter.line_segment([p(8.0, 3.0), p(12.0, 3.0)], w);
    painter.line_segment([p(3.0, 6.0), p(17.0, 6.0)], w);
    // Body: two slightly tapered sides and the base.
    painter.line_segment([p(5.2, 6.0), p(6.4, 17.0)], w);
    painter.line_segment([p(14.8, 6.0), p(13.6, 17.0)], w);
    painter.line_segment([p(6.4, 17.0), p(13.6, 17.0)], w);
    // Two ribs.
    painter.line_segment([p(8.9, 9.0), p(8.9, 14.4)], w);
    painter.line_segment([p(11.1, 9.0), p(11.1, 14.4)], w);
}

// ─────────────────────────── demo data (--demo) ───────────────────────────

/// The counterpart of the CLI's `/demo`: preview the layout and the shape of the event stream
/// without connecting a real model.
fn seed_demo(app: &mut App) {
    app.model = "deepseek-chat".into();
    app.config_path = "config.toml".into();
    app.plugins = vec!["filestat".to_string()];
    app.status = aox::i18n::t("Ready").into();
    // Demo mode reads history from the real application root too: a preview should look real,
    // and making up a few fake conversations would just make people think the sidebar is
    // ornamental.
    let root = aox::paths::resolve_workplace().0;
    let store = aox::store::default_store(&root, 0);
    app.boot = Some(BootInfo {
        config_path: app.config_path.clone(),
        // The demo data gets a realistic layer chain too, so that row in the "About" window is
        // visible under --demo.
        config_layers: vec!["bundle:core".into(), "config.toml".into()],
        model: app.model.clone(),
        provider_ready: true,
        note: None,
        // Demo mode gets a real path listing as well: the settings page's "System" column is
        // exactly the kind of "one glance is enough" diagnostic data, so previewing it with fake
        // paths is pointless (and the real paths are readable in demo mode anyway).
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

    // The workplace button's initial value: in demo mode the bridge won't send
    // `FromAgent::Workplace`, so one is stuffed in locally, letting `--demo` show the tier name
    // on the button and the ticked row in the list.
    app.ws.set_view(WorkplaceView { mode: Mode::Default, root: None });

    // The "Σ usage" chip **is only drawn when there is a ledger**, and `--demo` has no real
    // calls -- so two records are added, letting the preview show it and the breakdown panel it
    // expands on hover (which is also the only way to verify the panel's position).
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

    app.items.push(Item::User(aox::i18n::t("Make a plugin that counts files by type in the current directory").into(), Vec::new()));
    app.items.push(Item::Process(Process {
        steps: vec![
            StepView {
                name: "list_plugins".into(),
                plugin: Some("meta".into()),
                subject: None,
                args: "{}".into(),
                result: Some((false, aox::i18n::t("Plugins loaded: probe_screen, fs").into())),
                diff: None,
                diff_detail: None,
                expanded: false,
            },
            StepView {
                name: "create_plugin".into(),
                plugin: Some("meta".into()),
                subject: None,
                args: r#"{"name":"filestat","language":"python"}"#.into(),
                result: Some((false, aox::i18n::t("Plugin filestat created, registered and persisted (1 tool)").into())),
                diff: None,
                diff_detail: None,
                expanded: false,
            },
            StepView {
                name: "write_file".into(),
                plugin: None,
                subject: Some("plugins/filestat/main.py".into()),
                args: r#"{"path":"plugins/filestat/main.py"}"#.into(),
                result: Some((false, aox::i18n::t("Overwrote plugins/filestat/main.py (2.4 KB)").into())),
                // Demo data for the edit statistic — the row draws it as "+46 / −2", and unfolds
                // into a diff in the very shape `diff_text` produces.
                diff: Some((46, 2)),
                diff_detail: Some(
                    "⋯\n import os\n \n def count(path):\n     out = {}\n-    for f in os.listdir(path):\n+    for f in sorted(os.listdir(path)):\n         ext = os.path.splitext(f)[1]\n         out[ext] = out.get(ext, 0) + 1\n+    return out\n⋯"
                        .into(),
                ),
                expanded: false,
            },
            StepView {
                name: "filestat_count".into(),
                plugin: Some("filestat".into()),
                subject: None,
                args: r#"{"path":".","ext":null}"#.into(),
                result: Some((false, ".rs 14 · .py 9 · .toml 4 · .md 2 · .lock 1".into())),
                diff: None,
                diff_detail: None,
                expanded: false,
            },
        ],
        interim: vec![aox::i18n::t("Check for existing capabilities to reuse first.").into()],
        reasoning: "Counting files by type: the host has no such capability; create a filestat plugin with create_plugin first, then call it."
            .into(),
        elapsed: Some(7.4),
        expanded: false,
        aborted: false,
    }));
    app.items.push(Item::Assistant(
        "Your `filestat` plugin is ready; it provides the tool **filestat_count**.\n\n## Usage\n\n```json\n{ \"path\": \".\", \"ext\": null }\n```\n\nLeave `ext` empty to count all extensions. Result for the current directory:\n\n| Ext | Count |\n|------|------|\n| .rs | 14 |\n| .py | 9 |\n| .toml | 4 |\n| .md | 2 |\n\n> The plugin is persisted under `plugins/filestat/`; it loads automatically on the next start -- no need to rebuild."
            .into(),
    ));
    // ▼▼▼ diagnostics: over-wide content (long code lines / long inline code / long words /
    //      lists / tables) -- to verify that content wider than the measure wraps at the right
    //      edge instead of being clipped.
    app.items.push(Item::Assistant(
        "## Over-wide content\n\n```json\n{\"very_long_key_that_never_wraps\":\"C:/Users/someone/AppData/Local/Temp/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json\",\"n\":1}\n```\n\nThe path `E:/rustapp/plugins/filestat/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.py` is very long.\n\n### Connectivity test\n\n- `httpfetch_raw` → requested bilibili.com, got HTTP 200 and fetched the page text (183539 chars, truncated); network and plugin-process communication are fine.\n\n- `bilibili_video_info` → fetched BV1GJ411x7h7 successfully with full metadata (title \"Never Gonna Give You Up (Official MV)\", publisher \"Sony Music China\", 106M plays).\n\n| Plugin | Language | Tools |\n| --- | --- | --- |\n| bilibili | python | bilibili_video_info (video metadata), bilibili_danmaku (bullet comments), bilibili_comments (comments), bilibili_subtitle (subtitles), bilibili_related (related videos) |\n| httpfetch | python | httpfetch_raw (generic GET for API debugging / web page fetching) |\n\nConclusion: no new or fixed plugins needed -- existing capabilities already cover Bilibili video info fetching + generic web requests, ready to take the job. File I/O, command execution, data export (CSV/Excel) and similar can be added later as needed.\n\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .into(),
    ));
    // ▲▲▲ diagnostics
    app.items.push(Item::Note(
        NoteKind::Info,
        aox::i18n::t("Plugin filestat restored (1 tool)").into(),
    ));

    // The activity stream: lay out a typical sequence of events in time order (horizontal
    // timeline: bar width = real elapsed time). Demo mode opens the activity-stream window as
    // well -- it is a big part of the UI already, so a preview should show it.
    {
        let mut a = activity::lock(&app.act);
        a.force_open();
        a.round_begin(aox::i18n::t("Make a plugin that counts files by type in the current directory"), 0.0);
        a.model_begin(0.5);
        a.model_push(aox::i18n::t("Counting files by type: the host has no such capability;"));
        a.model_push(aox::i18n::t("create a filestat plugin with create_plugin first, then call it."));
        a.tool_begin("list_plugins", Some("meta"), "{}", 1.7);
        a.tool_end(false, aox::i18n::t("Plugins loaded: probe_screen, fs"), 2.0);
        a.model_begin(2.1);
        a.model_push(aox::i18n::t("Check for existing capabilities to reuse first."));
        a.tool_begin(
            "create_plugin",
            Some("meta"),
            r#"{"name":"filestat","language":"python"}"#,
            2.8,
        );
        a.tool_note(aox::i18n::t("Timeout limit 120s"));
        a.note(ActKind::Auth, aox::i18n::t("Request authorization · create_plugin"), "{\"name\":\"filestat\"}", 3.6);
        a.note(ActKind::Info, aox::i18n::t("Authorization result"), aox::i18n::t("Allow once"), 5.8);
        a.tool_end(false, aox::i18n::t("Plugin filestat created, registered and persisted (1 tool)"), 6.4);
        a.model_begin(6.5);
        a.tool_begin("filestat_count", Some("filestat"), r#"{"path":".","ext":null}"#, 7.4);
        a.tool_end(false, ".rs 14 · .py 9 · .toml 4 · .md 2 · .lock 1", 8.6);
        a.model_begin(8.7);
        a.model_push(aox::i18n::t("Result in hand; assembling it into a table for the user."));
        a.end_round(11.9);
        a.tick(11.9, aox::i18n::t("Ready"), false);

        // Second turn: the cross-turn, cross-plugin connector effect is demonstrated too.
        //
        // It starts at 11.9, exactly where the first turn ended. That is not a cosmetic choice:
        // the clock stops between turns (`Clock`), so in a real session the gap between one round's
        // last record and the next round's instruction is always zero. Seeding an idle gap here
        // would make the preview show a layout the program can no longer produce.
        a.round_begin(aox::i18n::t("Write the plugin's usage into the README"), 11.9);
        a.model_begin(12.5);
        a.model_push(aox::i18n::t("Just add a \"filestat usage\" section to the README."));
        a.tool_begin("fs_write", Some("fs"), r#"{"path":"README.md"}"#, 13.3);
        a.tool_end(false, aox::i18n::t("Wrote 1.2 KB"), 14.1);
        a.model_begin(14.2);
        a.model_push(aox::i18n::t("Done."));
        a.end_round(16.8);
        a.tick(16.8, aox::i18n::t("Ready"), false);
    }
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

        self.toolbar(ctx);

        if self.native_theme != Some(theme::mode()) {
            self.native_theme = Some(theme::mode());
            theme::sync_native_theme(ctx);
            theme::sync_caption(frame);
        }

        // The history overlay: once `hist_t` reaches zero it stops drawing itself, so **don't**
        // additionally test `show_history` here -- the collapsing has to keep being drawn, and
        // testing the toggle would make the panel vanish mid-way.
        self.history_overlay(ctx);
        // The activity stream lives in its own window: the main window syncs "what is happening
        // right now" over every frame and re-declares that viewport (egui viewports are
        // declarative; one that isn't declared gets reclaimed).
        activity::lock(&self.act).tick(self.now(), &self.status, self.busy);
        // Hand the stream window this window's native handle, once: it needs it to keep this window
        // painting while a turn runs (the stream window takes the foreground when it opens, and a
        // frozen pass loop here is a frozen timeline there — see `activity::poke_main`).
        activity::remember_main_window(&self.act, frame);
        activity::sync(ctx, &self.act);
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
            // The transcript fills the whole central panel; no bottom bar is given up for the
            // input box any more
            self.transcript(ui);

            // The input box became a floating card (a Foreground layer) pressed right on top of
            // the transcript. Why not put it in a `TopBottomPanel::bottom`: that would permanently
            // eat a bottom bar, and since the panel's fill matches the background it just looks
            // like an empty region at the bottom.
            // `constrain_to` does two jobs here: it confines the clip region to this column (so it
            // can't cover the content of the layer below), and it makes the anchor's centring
            // reference frame exactly this column.
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

        self.settings_overlay(ctx);
        self.auth_overlay(ctx);
        // Last, so the toast sits above everything: it reports something that has **already**
        // happened, and being hidden behind a card that is asking a question would lose exactly the
        // moment it was needed.
        self.toast_overlay(ctx);
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

/// The transcript column's measure: returns (content width, left/right padding).
/// Over-wide lines are hard to read, so there is an upper bound and it is centred in the
/// window.
pub(crate) fn column(available: f32) -> (f32, f32) {
    let inner = (available - 96.0).min(880.0).max(280.0);
    let pad = ((available - inner) * 0.5).max(18.0);
    (inner, pad)
}

/// The vertical space the floating input box takes up (card height + bottom margin + room for the
/// shadow).
/// That much blank is reserved at the end of the transcript, so the last message can't be pressed
/// under the card.
const COMPOSER_RESERVE: f32 = 118.0;

/// How many conversations the history overlay lists at most.
///
/// No pagination: a few dozen is already plenty for "scroll back and find the one from just now",
/// whereas listing everything would mean scanning the whole directory on every refresh. To really
/// go further back, both `/history` and the `history/` directory are there.
const HISTORY_LIST_LIMIT: usize = 80;

// ────────────── geometry of the floating button row and the history overlay ──────────────
//
// These numbers used to be scattered across two places: the buttons' landing spots lived in
// `top_bar`'s `exact_height(50)` + `Margin::symmetric(20, 0)`, and the history column's width
// lived in `SidePanel::default_width(266)`. With both the top bar and the side column removed they
// have no "container" to lean on, so they are pinned explicitly here -- the positions are lifted
// from the pre-removal result, pixel for pixel unchanged.

/// Side length of a button's hot zone (the same number as `d` in the three icon functions).
const BAR_BTN: f32 = 36.0;
/// Distance from the button row to the window's **left** edge.
const BAR_LEFT: f32 = 20.0;
/// Distance from the button row to the window's top edge. `7 + 18 = 25`, exactly half of the old
/// 50px top bar -- the buttons' centre line coincides exactly with the pre-removal one.
const BAR_TOP: f32 = 7.0;
/// Width of the history overlay.
const HISTORY_W: f32 = 300.0;
/// Height of the "above the list" part of the history overlay (padding + title row + separator).
///
/// Used only to work out how tall the list may be at most; rounding is fine -- the panel's real
/// height is decided by its content, not by this.
const HIST_HEAD: f32 = 74.0;
/// The slide-out / collapse animation duration (seconds).
///
/// 0.24 is the "you can see it moving, but you don't have to wait" setting: shorter falls back to
/// an instant jump, longer feels like the UI is hesitating after the click.
const HIST_ANIM_SECS: f32 = 0.24;

/// The two buttons in the slide-out group -- these are the only ones that need the "emerge from
/// behind the history button" clipping.
///
/// An enum rather than two nearly identical `Area` blocks: the two buttons' offsets, clipping and
/// padding have to match pixel for pixel, and with two copies any future change to one of them
/// leaves the two misaligned (an error that is very hard to spot by reading the code -- only a
/// screenshot comparison shows it).
#[derive(Clone, Copy)]
enum SlideIcon {
    /// New conversation
    Plus,
    /// Settings
    Gear,
}

/// Ease-out curve (cubic ease-out): quick off the mark, slow to settle.
///
/// Used instead of the built-in `ctx.animate_bool_with_time` (which interpolates **linearly**):
/// constant-speed motion makes hard corners at both ends -- it kicks off as if flicked and stops
/// as if it hit a wall.
fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

/// A constraint rect that is "much bigger than the window": for the `Area`s that need to slide
/// outside it.
///
/// An `Area` pulls its position back inside the constraint rect by default, which flattens the
/// slide-out offset on the spot, so those places have to release it. **But don't use
/// `Rect::EVERYTHING`** -- it is ±infinity inside, and as soon as it takes part in something like
/// anchor alignment the position becomes NaN or infinite and the Area disappears entirely (the
/// top-right button row ran into this once). Hand it a finite, roomy box: same effect, and no
/// absurd numbers come out of it.
fn loose_rect() -> Rect {
    Rect::from_min_max(Pos2::new(-10_000.0, -10_000.0), Pos2::new(10_000.0, 10_000.0))
}

/// The history button: a vertical rule on the left (standing for the sidebar) plus three short
/// horizontal lines.
///
/// Drawn the same way as the two other top-bar icons (36px hot zone, single-colour stroke, hover
/// recolour).
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
    // A backdrop is laid down even in the resting state (= the old top bar's fill): with the bar
    // gone the buttons float straight on the transcript text, and without that backdrop the icons
    // would smear into the body text. Hover raises it one step, the same language as list rows and
    // menu rows.
    painter.rect_filled(
        rect,
        CornerRadius::same(10),
        if hov { P::surface_hi() } else { P::panel() },
    );
    let r = rect.shrink(10.0);
    // One short vertical line on the left = the sidebar itself
    painter.line_segment(
        [Pos2::new(r.left() + 0.8, r.top()), Pos2::new(r.left() + 0.8, r.bottom())],
        Stroke::new(1.6, fg),
    );
    // Three horizontal lines on the right = the list entries
    for i in 0..3 {
        let y = r.top() + r.height() * (0.16 + 0.34 * i as f32);
        painter.line_segment(
            [Pos2::new(r.left() + 4.6, y), Pos2::new(r.right(), y)],
            Stroke::new(1.6, fg),
        );
    }
    resp.on_hover_text(if on { aox::i18n::t("Hide history") } else { aox::i18n::t("Show history") })
}

/// The "new conversation" button: a "+".
///
/// Drawn the same way as the history icon next to it (36px hot zone, single-colour stroke, hover
/// recolour); side by side the two read as "browse a thread / start a thread". While a turn is
/// running it is dimmed and ignores clicks -- switching conversations then would only take effect
/// after the turn, and the clicker would just think the UI had frozen.
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
        return resp.on_hover_text(aox::i18n::t("A task is running; start a new conversation later"));
    }
    resp.on_hover_text(aox::i18n::t("New conversation"))
}

/// A single line of text, closed off by **pixels**: anything past `max_w` is elided.
///
/// Truncating by character count (`clamp_str`) doesn't work: 34 Han characters are ~476px, twice
/// the sidebar's width, and the excess draws right out of the panel and gets sliced off hard by
/// the sidebar / main-transcript divider -- which looks like the title escaping its bounds. Let
/// `LayoutJob` decide where to cut from the actual rendered width, and it follows automatically
/// when the panel is dragged wider or narrower.
fn elide(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat { font_id: font, color, ..Default::default() },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: max_w,
        max_rows: 1,
        // Chinese has no word boundaries; without this, "the whole run doesn't fit" results in no
        // truncation at all
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

/// One history conversation row in the sidebar: title + "last activity · message count".
///
/// `current` = the one being chatted in right now (a filled dot on the right); `locked` = a switch
/// request is in flight or a turn is running, in which case clicks are refused and the whole row
/// is dimmed one step.
fn history_row(
    ui: &mut Ui,
    s: &aox::history::Summary,
    current: bool,
    locked: bool,
    width: f32,
) -> egui::Response {
    let w = (width - 6.0).max(60.0); // leave room for the scroll bar
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
    // The text's right bound is set 10px to the left of the dot: neither the "current" dot on the
    // right nor the safety margin eats into the title's width, so both lines close off at the same
    // width and the title can never draw outside this row.
    let dot_x = rect.right() - 10.0;
    let max_w = (dot_x - 10.0 - x).max(24.0);
    let title = elide(ui, &s.title, theme::body_font(14.0), title_color, max_w);
    let sub = elide(
        ui,
        &aox::tf!("{} · {} msgs", s.time, s.messages),
        theme::body_font(12.0),
        P::text_faint(),
        max_w,
    );
    let painter = ui.painter();
    // A galley is positioned by its top-left, so half its own height is subtracted here to get the
    // previous "vertically centred" placement
    painter.galley(Pos2::new(x, rect.top() + 15.0 - title.size().y * 0.5), title, title_color);
    painter.galley(Pos2::new(x, rect.top() + 32.0 - sub.size().y * 0.5), sub, P::text_faint());
    if current {
        // The current conversation is marked with a filled dot, which doesn't eat into the title's
        // width
        painter.circle_filled(Pos2::new(dot_x, rect.top() + 15.0), 3.2, P::ink_soft());
    }
    if locked {
        return resp.on_hover_text(format!("{}\n{}", s.title, s.id));
    }
    resp.on_hover_text(aox::tf!("{}\n{}\nClick to switch to this conversation", s.title, s.id))
}

/// Flatten the history messages read from disk into the transcript's turn-by-turn records.
///
/// The only difference from live rendering is how the "process" is accumulated: live accumulates
/// as events arrive, whereas here it is reconstructed from the message sequence -- an assistant's
/// tool_calls open a process, and the tool results right after it are filled back into the
/// matching step by name.
fn items_from_history(loaded: &[aox::history::Loaded]) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    // The process currently being accumulated (settled and pushed into `out` when a new user
    // message or a final answer appears)
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
                // `flat_text` rather than `content.clone()`: a user message may carry attachments,
                // in which case content is a parts array -- cloning it directly shows not a single
                // character. The attachment manifest block the host adds itself is filtered out by
                // `flat_text` (see `attach::ATTACH_MARK`).
                let text = m.flat_text();
                // Both the chip's name and the image paths to **display** are recovered from the
                // manifest block: the `image_url` block has no file-name field (see
                // `attach::files_from_content`).
                let files = m
                    .content
                    .as_ref()
                    .map(aox::attach::files_from_content)
                    .unwrap_or_default();
                if text.trim_start().starts_with(aox::history::HOST_PREFIX) {
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
                        // History has no elapsed time (that is a runtime-only fact), so it isn't shown
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
                            // The file name still comes from the arguments, which the history file
                            // does keep — only the diff itself was never written down.
                            subject: subject_from_json(
                                &c.function.name,
                                &c.function.arguments,
                            ),
                            args: c.function.arguments.trim().to_string(),
                            result: None,
                            // Not persisted, so a resumed turn shows no figure on its edit rows.
                            diff: None,
                            diff_detail: None,
                            expanded: false,
                        });
                    }
                }
            }
            "tool" => {
                let name = m.name.clone().unwrap_or_default();
                let body = m.flat_text();
                if let Some(p) = pending.as_mut() {
                    // The same step may have several calls of the same name; fill back into "the last
                    // same-named step that has no result yet"
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

/// Settings: a gear (36px hot zone, single-colour stroke, hover recolour -- the same drawing as
/// history / "+").
///
/// The gear is drawn as **an 8-tooth polygon plus a hole in the middle**, not as "a circle plus
/// eight spokes": the latter is exactly how [`theme_toggle`] draws a sun, and shrunk into 36px the
/// two would read as the same shape (both being "a circle + eight lines"). Drawing the teeth as
/// **a polyline** gives them an outline and keeps the two instantly distinguishable.
///
/// The tooth profile is computed by hand: alternate points on the tips and the valleys, closing
/// the loop. 8 teeth x 4 points = 32 points.
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
    // The circle is divided into TEETH segments, and each one holds four points:
    // "tip -> valley -> valley -> tip".
    let step = std::f32::consts::TAU / TEETH as f32;
    let tooth = step * 0.22; // the tooth tip's share of the angle (the rest is the valley)
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
    pts.push(pts[0]); // close the loop
    painter.add(egui::Shape::line(pts, Stroke::new(1.5, fg)));
    // The centre hole -- with it, it looks like a gear; without it, it's an eight-pointed star
    painter.circle_stroke(c, 2.5, Stroke::new(1.5, fg));

    resp.on_hover_text(aox::i18n::t("Settings"))
}

/// That S-shaped flow line meaning "information flows from here to there".
///
/// **Extracted** from the old top bar's `flow_toggle`: that button has been removed (the activity
/// stream is opened from the settings page now), but the curve still appears on the settings
/// page's "open activity stream" button -- one shape should have only one drawing.
fn draw_flow_curve(painter: &egui::Painter, rect: Rect, fg: Color32) {
    // One control point above and one below gives a smooth S
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

    // start node
    painter.circle_filled(a, 2.1, fg);

    // end arrow: along the tangent of the curve's last segment
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

/// Switch to the "hand" cursor while hovering a clickable area, and return whether it is hovered.
///
/// egui doesn't change the cursor for hand-drawn widgets automatically:
/// `allocate_exact_size(.., Sense::click())` only registers an interaction area, and the cursor
/// stays the default arrow. Together with `selectable_labels` (on by default) the label inside a
/// button even turns the cursor into the text-selection I-beam -- giving you "an input cursor on
/// top of a button". Route everything through this function and a clickable place is always a
/// hand.
fn pointer_if(ui: &Ui, resp: &egui::Response) -> bool {
    let hov = resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    hov
}

/// The skin for text buttons: sizing plus a colour pair for resting / hover.
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

/// A hand-drawn text button.
///
/// The key is the **order of drawing**: measure the text first -> allocate a hot zone from that
/// size -> lay down the backdrop -> draw the text last.
///
/// The empty-state example buttons used to do it the other way round: `ui.label()` painted the
/// text first and the hover backdrop was then laid over the hot zone -- landing above the text
/// layer, so the moment the mouse arrived the text vanished. Here it is backdrop first, text last,
/// so in both themes and every state they naturally can't occlude each other.
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
    // The layout is done with PLACEHOLDER and the real colours are applied here -- so a hover
    // recolour doesn't have to re-lay-out the text
    painter.galley(rect.min + b.pad, galley, if hov { b.fg_hover } else { b.fg });

    resp
}

/// A secondary button with a transparent backdrop (e.g. "Close" in dialogs).
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

/// The primary action: a reversed block (ink-black with white text in the light theme), which
/// carries more weight than an outline.
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

/// The dangerous-action button (currently only used for the "full access" confirmation).
///
/// [`P::danger`] is the backdrop and the text colour is still [`P::on_reverse`] -- not a
/// coincidence: `danger` is a deep brick red in the light theme (needing light text) and a pale
/// brick red in the dark theme (needing dark text), which matches the reversed block's light/dark
/// direction exactly, so no separate foreground colour has to be computed for it.
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

/// The example-question chip in the empty state.
/// One attachment chip (a cell in the pending-upload bar). Returns `true` if the user clicked the
/// × on the right.
///
/// The chip as a whole is **not clickable**; only the × is a hot zone: there is nowhere for this
/// chip to "open" to (an attachment is not a link), and making the whole thing clickable amounts
/// to "one click, one deletion", far too easy to fumble.
/// The last row of the option slot: **inline input** (answer with Enter or by clicking the arrow on
/// the right). Returns what the user typed.
///
/// The first version was just a "type your own..." button that handed focus to the big input box
/// below. The user's feedback (2026-10-06) was "don't use that big main input box below" -- the
/// place to answer should be beside the question, with no need for the eyes to run back and forth
/// between two boxes; and "after clicking the button you still have to go find another box" is
/// itself easily taken for "I clicked it and nothing happened".
///
/// Both Enter and the send button work: single-line input naturally wants Enter, but mouse users
/// shouldn't be forced to touch the keyboard.
fn ask_input_row(ui: &mut Ui, w: f32, text: &mut String) -> Option<String> {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(10), P::surface());

    // The send button on the right (same drawing as the one on the main input box, one size down)
    let btn = Rect::from_center_size(
        Pos2::new(rect.right() - 19.0, rect.center().y),
        Vec2::splat(26.0),
    );
    // Leave the number column free on the left (12 padding + 26 column width) so the text is
    // left-aligned with the A / B / C above it
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
                RichText::new(aox::i18n::t("You can also type here…")).color(P::text_faint()).size(13.0),
            ),
    );
    // The standard test for "Enter" on a single-line TextEdit: it **lost focus** this frame and
    // Enter was pressed.
    // `key_pressed` alone won't do -- that would misfire on Enter while typing (it doesn't lose
    // focus).
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
        // Draw a hollow circle while it's empty, so it's clear "there's a button here, you just have
        // to write something first"
        p.circle_stroke(btn.center(), btn.width() * 0.5, Stroke::new(1.0, P::border()));
    }
    paint_arrow_up(p, btn, if can { P::bg() } else { P::text_faint() });

    (can && (enter || bresp.clicked())).then_some(typed)
}

/// One option row in the option slot: `A` + the body text. Returns whether it was clicked.
fn ask_row(ui: &mut Ui, w: f32, idx: usize, text: &str) -> bool {
    ask_chip(ui, w, idx, text)
}

/// One row in the option slot: `A` + the body text.
///
/// Hand-drawn + `allocate_exact_size(.., Sense::click())`, and **deliberately not `Frame::show`
/// followed by `interact()`**: at narrow widths the latter's hit zone gets squashed along with the
/// content (that is exactly what the first version did -- it drew as a 28px-wide sliver and
/// clicking it did nothing at all). Hand-drawn, the hit zone is the whole row: a definite width,
/// exactly coinciding with what is drawn.
///
/// The number column takes a fixed width, so the left edges of A / B / C's text **line up** -- the
/// option slot is a column, not a heap.
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
    // Add a stroke on hover: with the backdrop step alone, in the light theme you can barely tell
    // which row the mouse is on
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
    // Estimate an upper bound from the available width and elide: splitting the difference between
    // ~13px for Han and ~7px for Latin gives 11px. Estimating loosely only leaves a bit too much or
    // too little blank -- **not eliding** would send a long option straight out of the row.
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

/// The × on the right of a chip. Returns `true` if it was clicked.
///
/// Hand-drawn from two line segments rather than typesetting the `×` character -- the same reason
/// as the ✓/✗ in the call stream: no betting on whether the font has the glyph (a missing glyph
/// draws as a tofu box, which is worse than not drawing it).
fn close_x(ui: &mut Ui) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(15.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(aox::i18n::t("Remove"));
    let col = if resp.hovered() { P::text() } else { P::text_faint() };
    let c = rect.center();
    let s = 3.4;
    let stroke = Stroke::new(1.4, col);
    ui.painter().line_segment([c + Vec2::new(-s, -s), c + Vec2::new(s, s)], stroke);
    ui.painter().line_segment([c + Vec2::new(s, -s), c + Vec2::new(-s, s)], stroke);
    resp.clicked()
}

/// When too long, **elide in the middle**: keep both ends, cut the middle out.
///
/// Why cut the middle rather than the tail: the most useful bit in this information is the
/// **extension** (`.png` or `.pdf`, which decides whether the user has to compress it first), and
/// the tail is exactly that. `keep` is the number of characters retained (ellipsis included).
fn middle_elide(s: &str, keep: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    // Too short, no need to elide; and when `keep` is too small to fit "head + ellipsis + tail",
    // don't force it either (it renders as a smear)
    if chars.len() <= keep || keep < 9 {
        return s.to_string();
    }
    let head = keep / 2;
    let tail = keep - head - 1; // the subtracted 1 is the ellipsis's own slot
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
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
    // Inside a solid-fill button the glyph has to use the page colour to be visible (in a
    // monochrome scheme `fill` is the "ink")
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

/// Accumulate a conversation's totals turn by turn.
fn sum_usage(log: &[UsageRecord]) -> Usage {
    let mut u = Usage::default();
    for r in log {
        u.accrue(&r.usage);
    }
    u
}

/// The text of a turn breakdown's row in the transcript.
///
/// "Lay every field out directly" is deliberate: this row is buried right next to the body text,
/// and whoever reads it is most likely checking the books -- making them open a second-level panel
/// is torment. Missing fields simply don't appear -- no padding out with 0s.
fn usage_line(r: &UsageRecord) -> String {
    let mut parts = vec![aox::tf!("#{} · {} calls", r.round, r.calls)];
    let mut put = |k: &str, v: Option<u64>| {
        if let Some(v) = v {
            parts.push(format!("{k} {}", thousands(v)));
        }
    };
    put(aox::i18n::t("Input"), r.usage.prompt);
    put(aox::i18n::t("Output"), r.usage.completion);
    put(aox::i18n::t("Total"), r.usage.total);
    put(aox::i18n::t("Reasoning"), r.usage.reasoning);
    put(aox::i18n::t("Cache hit"), r.usage.cached);
    put(aox::i18n::t("Cache miss"), r.usage.cache_miss);
    parts.join(" · ")
}

/// The token breakdown line in the transcript. Deliberately lighter than the body text: it is a
/// footnote, not content, so it isn't even given a bullet -- a row of numbers says what it is on
/// its own.
fn usage_row(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).color(P::text_faint()).size(12.5));
    ui.add_space(10.0);
}

/// The breakdown unfolded from the "Σ total tokens" chip: **this conversation's totals**.
///
/// The turn-by-turn bills are not here -- each of those rows sits with its own AI reply in the
/// transcript (see `usage_line`). What this answers is a different question: "how much did this
/// whole conversation cost, and where did it go".
fn usage_detail(ui: &mut Ui, log: &[UsageRecord]) {
    // Tighten the line spacing. This is "a column of numbers", and the default item spacing
    // stretches it out into something like a loose card, making it harder to see at a glance which
    // rows belong to the same entry.
    ui.spacing_mut().item_spacing.y = 1.0;
    let rows: Vec<&UsageRecord> = log.iter().filter(|r| !r.is_empty()).collect();
    if rows.is_empty() {
        ui.label(
            RichText::new(aox::i18n::t("No usage records for this conversation yet"))
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

    /// One "label ... number" row. A `v` of None skips the row entirely.
    ///
    /// Allocate this row's rect directly rather than using `horizontal` + `with_layout` -- the
    /// latter stretches the row height to the content area's leftover in a vertical container, so a
    /// column of numbers comes out as loose as an empty card.
    /// The numbers use a monospace face so that right-aligned they line up vertically; reading the
    /// numbers matters more than reading the labels.
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
        RichText::new(aox::tf!("This conversation · {} rounds · {} calls", rows.len(), calls))
            .color(P::text())
            .size(13.5),
    );
    ui.add_space(6.0);

    row(ui, aox::i18n::t("Input"), sum.prompt);
    row(ui, aox::i18n::t("Output"), sum.completion);
    row(ui, aox::i18n::t("Total"), sum.total);
    // The sub-items are indented one level under the "totals": reasoning is part of output and
    // cache is part of input
    row(ui, aox::i18n::t("  of which reasoning"), sum.reasoning);
    row(ui, aox::i18n::t("  cache hit"), sum.cached);
    row(ui, aox::i18n::t("  cache miss"), sum.cache_miss);

    // Last input != total input: the former is "how big is the current context", the latter is
    // "how much was read in across all these turns". Showing both is what keeps "the context is
    // nearly full" from being misread as "one turn was too expensive".
    if let Some(v) = rows.last().and_then(|r| r.last_prompt) {
        let (rule, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
        ui.painter()
            .hline(rule.x_range(), rule.center().y, Stroke::new(1.0, P::border()));
        ui.add_space(9.0);
        row(ui, aox::i18n::t("Current context"), Some(v));
    }
}

fn user_bubble(ui: &mut Ui, text: &str, files: &[String], max_w: f32) {
    // The padding takes up width on top, so the body text's available width has to be subtracted
    // first; otherwise the bubble sticks out of the column and gets clipped
    let inner_w = (max_w - 44.0).max(140.0);
    let body = text.trim();

    // Lay the body text out once at `inner_w` first and **measure the width actually used**: short
    // sentences narrow, long ones run full.
    //
    // ⚠️ This step can't be skipped. `Frame::show` computes its own rect from the content's
    // `min_rect`, and that is stretched to "available width" from the start -- writing only
    // `ui.set_max_width(..)` **clamps that width down** rather than letting it shrink with the
    // content. The result: however long the message, the bubble is the same width (which is exactly
    // what the user reported on 2026-10-06).
    let galley = (!body.is_empty()).then(|| {
        ui.painter().layout(
            body.to_owned(),
            theme::body_font(theme::body_size()),
            P::text(),
            inner_w,
        )
    });
    let text_w = galley.as_ref().map_or(0.0, |g| g.size().x);
    // With attachments the bubble has to fit at least one chip (the widest one is used as the lower
    // bound; if it really doesn't fit, let it wrap)
    let chips_w = files.iter().map(|n| chip_width(ui, n)).fold(0.0_f32, f32::max);
    let want = text_w.max(chips_w).min(inner_w);

    ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
        Frame::NONE
            .fill(P::user_bg())
            .corner_radius(CornerRadius::same(16))
            .stroke(Stroke::new(1.0, P::border()))
            .inner_margin(Margin::symmetric(20, 15))
            .show(ui, |ui| {
                // The width is **pinned** (min and max both set to the same value), so the content's
                // `min_rect` is `want` and the bubble closes around the content -- a narrow strip for a
                // short sentence, the full measure for a long one.
                ui.set_min_width(want);
                ui.set_max_width(want);
                ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                    // The attachment chips go **above** the body text: consistent with their position
                    // in the pending-upload bar (the same look before and after sending), and with an
                    // empty body (only files sent) the bubble still has something to show.
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

/// How wide a sent attachment chip is (**estimated**).
///
/// Used only to give the bubble a width **lower bound** -- measuring it properly would mean drawing
/// it once first, which isn't worth it; estimating loosely only leaves the bubble a hair wider or
/// narrower than the chip, and `horizontal_wrapped` wraps to catch it.
/// ⚠️ The font size / padding / stroke must match the ones in [`sent_chip`]; changing that side
/// means changing this one too.
fn chip_width(ui: &Ui, name: &str) -> f32 {
    const PAD_X: f32 = 11.0;
    const STROKE: f32 = 1.0;
    let g = ui.painter().layout_no_wrap(
        middle_elide(name, 30),
        FontId::proportional(13.0),
        Color32::PLACEHOLDER,
    );
    g.size().x + PAD_X * 2.0 + STROKE * 2.0
}

/// The attachment chip inside an already-sent message bubble -- read-only (no ×).
///
/// **The same shape and the same elision rules** as the pending-upload one, just missing an ×: that
/// way the act of sending is visually "the chip dims and the × disappears", with no other marker
/// needed to explain it.
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

/// Extensions that look like images. **Only used to decide "is it worth trying to decode"**, not
/// to determine the format -- the decoder itself (`aox::image::decode`) is what actually decides.
/// So a file whose extension isn't in the table but whose content is really a PNG won't be shown as
/// an image, which is the price of this shortcut; and conversely "the extension looks like an image
/// but the content isn't" gets rejected by the decoder, and the UI says "display failed" honestly
/// rather than pretending it worked.
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

/// An image carried by a message. Scaled to the measure's width, aspect ratio preserved.
///
/// Each of the three states is drawn its own way (see [`Tex`]): the image / "decoding..." / a
/// one-line failure reason.
/// **Never upscaled** (`scale` capped at 1.0): blowing a 200x80 diagram up to the full measure only
/// smears it into mosaic, and the user seeing "this is how big it is" is more useful than seeing a
/// blurred large version.
fn images_block(ui: &mut Ui, paths: &[PathBuf], inner: f32, tex: &HashMap<PathBuf, Tex>) {
    for p in paths {
        if !looks_like_image(p) {
            // Not an image (.txt / .zip and the like): there was never meant to be a picture, so
            // skip quietly. Its name is already in the bubble / notice line above.
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
                    RichText::new(aox::tf!("Cannot display this image: {}", e))
                        .color(P::text_faint())
                        .size(12.5),
                );
            }
            // Queued / decoding. To the user these are the same thing; no need to say them
            // separately.
            _ => {
                ui.label(RichText::new(aox::i18n::t("Decoding image…")).color(P::text_faint()).size(12.5));
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
    // In a monochrome scheme, errors use a "reversed block" rather than red text -- the highest
    // contrast, and the least dependent on hue
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
                    ui.label(RichText::new(aox::i18n::t("Failed")).color(P::on_reverse()).size(14.0));
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
    // History has no elapsed-time information (that only exists at runtime), so only the step
    // count is reported here.
    let tail = match p.elapsed {
        Some(s) => format!(" · {s:.1}s"),
        None => String::new(),
    };
    let label = if p.aborted {
        aox::tf!("Task interrupted · {} steps{}", n, tail)
    } else if n == 0 {
        aox::tf!("Processing{}", tail)
    } else {
        aox::tf!("{} tool calls{}", n, tail)
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
                // Note: the available width **inside the frame** has to be used here.
                // Reusing the outer width would make the content wider than the frame's inner area,
                // and the whole card would be pushed past the column width and clipped.
                let w = ui.available_width();
                ui.set_width(w);
                ui.set_max_width(w);
                for (i, st) in p.steps.iter_mut().enumerate() {
                    step_row(ui, st, i);
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
                    ui.label(RichText::new(aox::i18n::t("Reasoning")).color(P::text_faint()).size(13.0));
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

/// A stretch of "what the model is saying right now" in the live area.
///
/// Deliberately rendered as **plain body text**, with no Markdown and no bubble border: it is a
/// temporary form of the same text before it hits disk, and once saved it becomes a bubble with
/// Markdown. Introducing a different style midway would make the two look like different sentences
/// and "jump" on the switch. `break_anywhere` is on to stop long paths in the answer from bursting
/// the measure -- all that matters here is not overflowing; whether it looks good is left to the
/// final bubble.
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

/// One call row, plus the diff panel it can unfold into.
///
/// `idx` is the row's slot in its container and is used **only** to give the row an id of its own.
/// Two rows in one card can both be unfolded, and each panel is *its own* scroll area and *its own*
/// draggable height — with a shared id they fight over one offset and one size (user report,
/// 2026-10-11: "unfold two diff panels and scrolling one scrolls the other too").
fn step_row(ui: &mut Ui, st: &mut StepView, idx: usize) {
    ui.push_id(idx, |ui| step_row_inner(ui, st));
}

fn step_row_inner(ui: &mut Ui, st: &mut StepView) {
    // A row with a diff behind it is **foldable** (user request, 2026-10-11: "the file being edited,
    // and clicking it opens the change"). Every other row stays plain and inert.
    let foldable = st.diff_detail.is_some();
    let unfolded = st.expanded;
    let row = ui
        .horizontal_top(|ui| {
            // Status markers (the form settled on with the user, 2026-10-05): in progress = a three-dot
            // marquee (**deliberately a different shape from the 8-dot orbiting spinner used for
            // reasoning**), success = ✓, failure = ✗.
            // They are no longer numbered either -- numbering reads "a few calls in one stage" as a
            // checklist, whereas what is wanted here is "one after another".
            match &st.result {
                None => call_marquee(ui, P::text_dim()),
                Some((is_err, _)) if *is_err => call_glyph(ui, false, P::ink()),
                Some(_) => call_glyph(ui, true, P::ink()),
            }
            ui.add_space(4.0);
            ui.label(RichText::new(&st.name).color(P::text()).size(14.5));
            // The file this call works on, straight after its name (user request, 2026-10-11): a row
            // reading only "write_file" leaves you counting back to find out which file it was.
            // Monospaced so two paths line up, and shortened from the middle -- the tail is the half
            // that identifies a file.
            if let Some(sub) = &st.subject {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(clip_path(sub)).font(theme::mono_font(13.0)).color(P::text_dim()),
                );
            }
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
            // Edit statistic, right where the eye already is — next to the call it belongs to. Green
            // for added lines, red for removed, and **nothing at all** when the call changed no lines:
            // a "0 / 0" would just be noise on every read-only call.
            if let Some((added, removed)) = st.diff {
                if added > 0 {
                    ui.add_space(6.0);
                    ui.label(RichText::new(format!("+{added}")).color(P::add_green()).size(13.0));
                }
                if removed > 0 {
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("-{removed}")).color(P::del_red()).size(13.0));
                }
            }
            // The caret sits at the far right edge of the row. It is also what makes the **whole
            // row** the click target: "there is something to open here".
            if foldable {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(14.0, 18.0), Sense::hover());
                    paint_tri(ui.painter(), rect.center(), 4.0, unfolded, P::text_faint());
                });
            }
            // Arguments are **not shown here** (user request, 2026-10-05): a JSON blob would stretch
            // this row into several lines, and expanded you can't see the bottom of it. The row keeps
            // only "tool name + owning plugin", and the next row gives a one-line summary of the result.
            // To really see the arguments, they are stored verbatim in `history/*.jsonl`.
        })
        .response;
    if foldable {
        // Re-register the row's own rect with a click sense, **after** the row has been built: egui
        // hands a click to the topmost widget that wants it, and the labels inside the row are added
        // before this, so the row wins.
        let resp = row.interact(Sense::click());
        if pointer_if(ui, &resp) && resp.clicked() {
            st.expanded = !st.expanded;
        }
    }
    if st.expanded {
        // Borrowed, never cloned: the transcript is laid out again on every repaint, and this string
        // is the length of the diff.
        if let Some(diff) = st.diff_detail.as_deref() {
            step_diff(ui, diff);
        }
    }
    ui.add_space(5.0);
}

/// The unfolded edit: every line of the diff, tinted by what the line is.
///
/// A **short** panel that scrolls (user request, 2026-10-11). The question it answers is "which lines
/// moved", and a panel that grew with the file would push the conversation off screen. Its cap is
/// [`DIFF_PANEL_H`], and a diff shorter than that draws a shorter panel — a small edit leaves no
/// empty background behind.
fn step_diff(ui: &mut Ui, diff: &str) {
    ui.add_space(6.0);
    Frame::NONE
        .fill(P::code_bg())
        .corner_radius(CornerRadius::same(8))
        .stroke(Stroke::new(1.0, P::border()))
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            // Inside the frame, so the panel can never be wider than the card holding it.
            let w = ui.available_width();
            ui.set_width(w);
            ui.set_max_width(w);
            // Claim the panel's height back from egui before building it.
            //
            // egui gives a nested `ScrollArea` only the height left in its parent's `max_rect`, and
            // inside the transcript **every** parent's rect is one screen tall, however far down the
            // page the row sits. A panel opened near the bottom of a long conversation was therefore
            // handed a few dozen pixels and stayed that size: `max_height(DIFF_PANEL_H)` is a cap, not
            // a request, and nothing could reach it (user report, 2026-10-11: "still too short", with
            // a three-line panel under a +353 diff). `set_max_height` grows the rect from the cursor
            // downwards, so asking for the full panel height here is exactly the fix.
            ui.set_max_height(ui.available_height().max(DIFF_PANEL_H));
            // The first character is the whole contract (see `diff_text`): `+` added, `-` removed, a
            // space for context, and anything else is a marker — an elided stretch, or where a diff
            // too large to draw was cut off — drawn faintest of all.
            ScrollArea::vertical()
                .id_salt("aox-step-diff")
                .max_height(DIFF_PANEL_H)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for line in diff.lines() {
                        let color = match line.chars().next() {
                            Some('+') => P::add_green(),
                            Some('-') => P::del_red(),
                            Some(' ') => P::text_dim(),
                            _ => P::text_faint(),
                        };
                        ui.label(RichText::new(line).font(theme::mono_font(12.5)).color(color));
                    }
                });
        });
}

/// A stretch of call stream in the live area (the calls preceding one interim reply).
///
/// The live area gets **one card per stretch** (see the segmentation loop in `live_block`), whereas
/// the process card puts everything in a single card -- the row rendering is shared, only the
/// grouping differs.
fn calls_card(ui: &mut Ui, steps: &mut [StepView]) {
    Frame::NONE
        .fill(P::surface())
        .corner_radius(CornerRadius::same(theme::card_radius()))
        .stroke(Stroke::new(1.0, P::border()))
        .inner_margin(Margin::symmetric(20, 16))
        .show(ui, |ui| {
            // Use the available width inside the frame, so the content can't push the card out of the
            // column width
            let w = ui.available_width();
            ui.set_width(w);
            ui.set_max_width(w);
            for (i, st) in steps.iter_mut().enumerate() {
                // Each row ids itself off its slot (see `step_row`), so panels never share a scroll
                // offset or a height.
                step_row(ui, st, i);
            }
        });
}

/// The "in progress" marker in the call stream: three dots with the lit one moving along.
///
/// **Deliberately a different shape** from [`spinner_dot`]'s 8-dot orbit (the user asked for
/// "something distinct from the reasoning spinner"): one glance tells you whether it is calling a
/// tool or thinking.
fn call_marquee(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(16.0, 20.0), Sense::hover());
    // The phase comes from `ui.input(|i| i.time)` (egui's self-incrementing seconds count):
    // callable anywhere directly, with no need to thread `App`'s `spin` all the way down.
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

/// ✓ / ✗. Hand-drawn from two line segments -- no betting on whether the font has these two
/// glyphs (the same reason as `paint_folder`).
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

// ────────────── settings dialog ──────────────

/// The settings dialog's **content area** size (padding 20/18 excluded).
///
/// A size up from the old 380px "about" card: it has to hold a two-column table, and the right
/// column is a list of paths (`E:\rustapp\workplace\2026-10-05-...`, values that can't be short).
const SETTINGS_W: f32 = 700.0;
/// Height hard-coded, the reason being "the card shouldn't grow and shrink when switching columns"
/// -- see the comment on `settings_overlay`.
const SETTINGS_H: f32 = 470.0;
/// Width of the left column (the list of columns).
const SETTINGS_NAV_W: f32 = 132.0;
/// Right column width = total - left column - 1px rule - 18px spacing.
const SETTINGS_RIGHT_W: f32 = SETTINGS_W - SETTINGS_NAV_W - 19.0;
/// The height the bottom "Close" row takes up (including the gap between it and the content).
const SETTINGS_FOOT_H: f32 = 44.0;
/// Width taken by the left column (the name) in a "name + value" row. Fixing it is what makes the
/// values line up in a column.
const KV_KEY_W: f32 = 84.0;

/// The open-source license text. **The repo's own `LICENSE`, embedded directly** -- not copied by
/// hand: a hand copy will sooner or later drift from the file, and what "About" shows has to be the
/// one actually in force.
const LICENSE_TEXT: &str = include_str!("../../LICENSE");

/// One column row in the left column.
///
/// The selected state is stated in two ways: "backdrop up one step + text in the primary colour"
/// plus a short vertical rule on the left. With the backdrop alone it would be nearly invisible in
/// this monochrome scheme's narrow range (`surface_hi` and `surface` are one step apart).
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

/// A radio row (used by the "Appearance" column): a dot on the right, filled when selected.
///
/// A dot rather than a tick: the tick's shape is already taken in this UI by the "workplace tier"
/// (meaning "the tier currently in force"), and using it again would read as the same thing.
/// Filled / hollow is the drawing that needs the least explaining for "selected / not selected".
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

/// The "open activity stream" button: the flow line on the left + text on the right.
///
/// Why not a plain text button: that flow line is the activity stream's **only visual identity**
/// (the old top-bar icon was exactly this), and dropping it would leave this column with just a few
/// words, so you'd have to read them before knowing what opens.
///
/// When it is already open the whole button dims one step: a button labelled "open"
/// **shouldn't close the window** when clicked again, so it isn't clickable then (which is what
/// `activity::open` does too -- see its comment).
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
    // Icon: an 18px square flow line, inset 22 from the left
    let icon =
        Rect::from_center_size(Pos2::new(rect.left() + 22.0, rect.center().y), Vec2::splat(18.0));
    draw_flow_curve(painter, icon, fg);
    let g = painter.layout_no_wrap(aox::i18n::t("Open activity stream").to_string(), egui::FontId::proportional(14.5), fg);
    painter.galley(Pos2::new(rect.left() + 42.0, rect.center().y - g.size().y * 0.5), g, fg);
    resp
}

/// A "name + value" row (the "System" column is all rows like this).
fn kv_row(ui: &mut Ui, key: &str, val: &str) {
    kv_row_colored(ui, key, val, P::text_faint());
}

/// As above, but the value may be coloured ("runtime environment" uses this to flag "not found").
fn kv_row_colored(ui: &mut Ui, key: &str, val: &str, color: Color32) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(KV_KEY_W, 19.0), Sense::hover());
        if !key.is_empty() {
            let p = ui.painter();
            let g = p.layout_no_wrap(key.to_string(), egui::FontId::proportional(13.5), P::text_dim());
            p.galley(Pos2::new(r.left(), r.top() + 1.0), g, P::text_dim());
        }
        // Values are always monospace: they are paths, commands, version numbers, not prose.
        // Selectable -- paths often need copying elsewhere (a terminal, a file manager), and a
        // read-only display amounts to making people retype them.
        ui.add(
            Label::new(RichText::new(val).font(theme::mono_font(12.5)).color(color))
                .selectable(true)
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    });
    ui.add_space(4.0);
}

/// A "name + clickable link" row (used by the "About" column).
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

    /// Nail down, as an executable statement, why the settings dialog **has to pin** its content
    /// width.
    ///
    /// This tests **a behaviour of egui**, not our code: a `ScrollArea` allocates the scrollbar's
    /// width **animated frame by frame** via `animate_bool_responsive`, so the content width inside
    /// it keeps changing. Any widget that sizes itself from `available_width()` widens and narrows
    /// along with it, and wrapped text re-wraps every frame -- which is exactly the root cause of
    /// the 2026-10-05 "move the mouse and the settings panel shivers slightly with blurry text"
    /// (the animation doesn't advance without a repaint, so it only shows up while the mouse is
    /// moving).
    ///
    /// Why it is worth pinning separately: in the source that behaviour amounts to two lines of
    /// subtraction (`current_bar_use = animated amount x allocated_width()`), and reading the code
    /// gives no hint that it moves the layout around; and the moment someone deletes
    /// `ui.set_width(content_w)` from `settings_overlay` as redundant, the jitter comes back
    /// unchanged -- while manual testing may well never run into it. So this test's **failure
    /// message** says exactly which comment to go and check.
    #[test]
    fn egui_animates_the_scroll_bar_allocation() {
        let ctx = egui::Context::default();
        // Consistent with this project's `theme::install_style`: floating style + reserved width
        // (thin() gives 6px)
        ctx.style_mut(|s| s.spacing.scroll = egui::style::ScrollStyle::thin());
        assert!(
            ctx.style().spacing.scroll.allocated_width() > 0.0,
            "precondition: the style must reserve scroll-bar width, otherwise this test proves nothing"
        );

        let mut widths: Vec<f32> = Vec::new();
        for frame in 0..8 {
            let input = egui::RawInput {
                // The animation advances on `time`: with no change in time, `animate_bool` sits at
                // its starting point forever
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
                            // Content clearly taller than the viewport, forcing a scrollbar
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
                        "egui no longer changes scroll-bar allocation per frame? Then the **rationale comment** for the `ui.set_width(content_w)` block in `App::settings_overlay` must be updated -- its only purpose is to dodge this animation (it is not redundant). Measured width sequence: {widths:?}"
        );
    }
}
