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
    /// What this call is **for**, in a few words — the model's own `purpose` argument when it gave
    /// one, otherwise the command itself for `bash`/`shell`.
    ///
    /// The row used to name the tool and nothing else, which for the two shell tools is the whole
    /// problem: a row reading `bash` says nothing about whether it is compiling, searching or
    /// reading a log. Like [`Self::subject`], this is parsed out of the arguments while they are
    /// still arriving, so the label appears with the row rather than after the call.
    purpose: Option<String>,
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
    /// The tool call running right now, if any — its own clock and the two numbers published for it.
    ///
    /// The header used to show only the **turn** clock, which is why "Running bash 471.5s" told the
    /// user nothing they could act on: 471 s of a turn may be 40 s of thinking plus a 430 s command,
    /// and there was no way to see either the call's own time or its ceiling.
    call: Option<RunningCall>,
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

/// The call the live row is reporting on right now.
///
/// Three numbers, and each answers a different question: `started` = "how long has it been going",
/// `cap` = "when will the host give up on it", `expect` = "how long did the model say it would take".
/// Without all three, a long call is indistinguishable from a wedged one.
struct RunningCall {
    /// Activity-clock reading when `ToolRunning` arrived.
    started: f32,
    /// This call's ceiling, in seconds (the shell tools compute it per call).
    cap: u64,
    /// The model's own estimate, when it chose to give one.
    expect: Option<u64>,
}

struct AuthCard {
    tool: String,
    args: String,
}/// A multiple-choice question waiting on the user (`ask_user`). The question and the
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
    /// target state. **Not persisted**: every launch starts folded (see `uistate`).
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

/// What the plan card draws, cached when a checkpoint arrives.
///
/// Cached rather than read from `TaskState` every frame: the paint path runs at 60 Hz and a plan is
/// a few hundred bytes of strings — cloning that per frame to satisfy the borrow checker would be
/// paying for nothing. Building it once, in the event arm, also keeps every "what is the current
/// step" decision in one place.
struct PlanView {
    done: usize,
    total: usize,
    /// The first open step — what the model says it is working on (see `TaskState::steps`).
    current: Option<String>,
    steps: Vec<(String, bool)>,
}

impl PlanView {
    /// `None` when the checkpoint has no plan: a card that always says "0/0" is noise.
    fn of(st: &aox::task::TaskState) -> Option<Self> {
        if st.steps.is_empty() {
            return None;
        }
        let steps: Vec<(String, bool)> = st.steps.iter().map(|s| (s.text.clone(), s.done)).collect();
        Some(Self {
            done: st.steps.len() - st.pending_steps(),
            total: st.steps.len(),
            current: steps.iter().find(|(_, d)| !d).map(|(t, _)| t.clone()),
            steps,
        })
    }
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
/// The two file writers — `write_file` (whole-file) and `edit_file` (in-place snippets) — both take a
/// `path` and both produce a diff, so both rows should say which file they are about. Adding a read or
/// a delete here is a one-line change the day the same is wanted for them; nothing else keys off this
/// list.
const SUBJECT_TOOLS: &[&str] = &[WRITE_FILE_TOOL, "edit_file"];

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

/// Longest `purpose` shown on a call row. Long enough for "fix the 471 s shell hang", short enough
/// that the row still fits beside the tool name and the file path.
const PURPOSE_MAX: usize = 48;

/// What a call is **for**, read off its arguments: `(purpose, subject)`.
///
/// | tool | purpose | subject |
/// |------|---------|---------|
/// | anything with a `purpose` argument | the model's words | — |
/// | `bash` / `shell` | the **command**, elided at the tail | — |
/// | `write_file` / `edit_file` | the model's words, if any | the file path |
///
/// The command is used as a *fallback*, never paraphrased: `bash` + `cargo build --release` tells the
/// user more than any four-word summary could, and it cannot be wrong. A purpose the model *did* write
/// wins, because "why" is the thing the command line does not say.
fn purpose_from_value(name: &str, args: &serde_json::Value) -> Option<String> {
    if let Some(p) = args.get(aox::tool::PURPOSE_ARG).and_then(|v| v.as_str()) {
        let p = flat_line(p);
        if !p.is_empty() {
            return Some(clip_head(&p, PURPOSE_MAX));
        }
    }
    if matches!(name, "shell" | "bash") {
        let cmd = flat_line(args.get("command").and_then(|v| v.as_str())?);
        return (!cmd.is_empty()).then(|| clip_head(&cmd, PURPOSE_MAX));
    }
    None
}

/// The same, from the arguments as text (a row rebuilt from `history/*.jsonl`).
fn purpose_from_json(name: &str, args: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    purpose_from_value(name, &v)
}

/// Collapse a multi-line script into one line: a `bash` row must stay one row.
fn flat_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut at the **tail** — unlike [`clip_path`], the beginning of a command is the identifying half
/// (the program being run), so the ellipsis belongs at the end.
fn clip_head(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
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
            show_history: false,
            hist_t: 0.0,
            hist_groups: Vec::new(),
            hist_open: HashSet::new(),
            hist_closed: HashSet::new(),
            hist_query: String::new(),
            hist_fold_loaded: false,
            name_dialog: None,
            // The drawer starts collapsed on a fresh install: the full width goes to the transcript,
            // and you click the top-left icon when you want it. From then on the position is read
            // back from `.aox/ui.json` (see `refresh_history`).
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
            attempt_mark: None,
            task: None,
            plan: None,
            plan_open: false,
            plan_t: 0.0,
            plan_anim: None,
            composer_base_h: COMPOSER_RESERVE,
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

    // ── event pump: land the agent thread's messages into local state ──
    fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                FromAgent::Boot(b) => {
                    self.model = b.model.clone();
                    self.config_path = b.config_path.clone();
                    // The ring's denominator. Zero would mean "not configured": fall back to the
                    // default rather than dividing by it.
                    self.context_window = if b.context_window > 0 {
                        b.context_window
                    } else {
                        DEFAULT_CONTEXT_WINDOW
                    };
                    // The UI-side history backend is shipped along by the bridge: the same
                    // implementation the agent thread uses.
                    self.store = Some(b.store.clone());
                    if let Some(note) = &b.note {
                        self.log_error = Some(note.clone());
                        self.items.push(Item::Error(note.clone()));
                    }
                    self.status = if b.provider_ready { aox::i18n::t("Ready").to_string() } else { aox::i18n::t("No model configured").to_string() };
                    let ready = b.provider_ready;
                    // The boot info has to be in place **before** the first refresh: `refresh_history`
                    // reads the app root out of it (`App::app_root`) to find the space registry, and a
                    // refresh that landed first would build an empty history tree.
                    self.boot = Some(*b);
                    self.refresh_history();
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
                        call: None,
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
                    // A new turn rewrites the context from scratch: whatever the last compaction
                    // left on the gauge is no longer the reading anyone needs (and if the previous
                    // turn died mid-compaction, the "compacting…" hint must not survive it either).
                    self.ctx_reset = None;
                    self.compacting = None;
                    // Defensive: a call still marked as "being written" from the previous turn would
                    // otherwise be adopted by a call of the same name in this one.
                    self.stream_calls.clear();
                    // Last turn's writes stop being "this turn's". The workspace view lists what the
                    // turn in flight is touching; the transcript already keeps every earlier edit,
                    // with its own diff, where it happened.
                    wsview::begin_turn(&self.wsview);
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
                        // The cache-hit window reads the same record, but only for what the record
                        // alone can say: the lifetime totals and the fallback reading for endpoints
                        // that report nothing per call. Its grid was already fed by `CallUsage`
                        // above -- a turn's `usage` is a *sum* over its calls, so letting it
                        // overwrite the grid would paint blocks the last request never sent.
                        cache::lock(&self.cache).observe_turn(&rec);
                        // The breakdown goes right after this turn's reply (by now the turn has
                        // ended and the tail of `items` is exactly that), and drag the view to the
                        // bottom while we're at it -- nobody goes looking for a row that appeared
                        // out of nowhere.
                        self.items.push(Item::Usage(usage_line(&rec)));
                        self.usage_log.push(*rec);
                        // A real reading is in: the post-compaction placeholder has done its job.
                        self.ctx_reset = None;
                        self.force_follow = true;
                    }
                }
                FromAgent::CallUsage { usage, last_prompt } => {
                    // One finished model call, mid-turn. This is what makes the cache map move: the
                    // turn's aggregate only arrives once the whole turn is over, which for a tool
                    // loop can be a dozen calls later.
                    cache::lock(&self.cache).observe_call(&usage, last_prompt);
                }
                FromAgent::UsageLog(log) => {
                    // The whole ledger the agent read back from file after a conversation switch.
                    // The per-turn breakdowns have to go back into the transcript -- when an old
                    // conversation is opened, each turn's bill should be stuck back where it was,
                    // not heaped up at the top.
                    self.usage_log = log;
                    // The gauge belongs to the conversation on screen; a switch replaces the reading
                    // wholesale, and a leftover override from the previous one would be a lie.
                    self.ctx_reset = None;
                    self.compacting = None;
                    self.rebuild_usage_rows();
                    // Rebuild the cache map from that ledger. The map is a history — one tile per
                    // recorded round — so it is replayed whole rather than seeded with the last
                    // line: opening a conversation with 30 turns should show 30 tiles, not one.
                    // Only sums survive in the file, which is why a replayed tile stands for a
                    // round and says so in its tooltip.
                    let mut cv = cache::lock(&self.cache);
                    cv.reset();
                    cv.replay(&self.usage_log);
                }
                FromAgent::ContextMap(segs) => {
                    // The section breakdown of the request just sent. Converted into the window's own
                    // `Seg` here rather than over there: the message crosses from the core crate, and
                    // the window's type belongs to the GUI crate.
                    cache::lock(&self.cache).observe_context(
                        segs.into_iter().map(|(label, tokens)| cache::Seg { label, tokens }).collect(),
                    );
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
                FromAgent::Attempt { n, delay_ms } => {
                    if n == 0 {
                        // A request is starting. Remember where its rows begin so a retry can undo
                        // exactly this attempt and nothing before it.
                        self.attempt_mark = self.live.as_ref().map(|l| l.steps.len());
                    } else {
                        // The abandoned attempt is being thrown away: its half-typed body text, the
                        // tool rows its streamed fragments opened, and the clock it was holding.
                        if let Some(l) = self.live.as_mut() {
                            l.streaming.clear();
                            if let Some(mark) = self.attempt_mark {
                                l.steps.truncate(mark);
                            }
                            l.call = None;
                        }
                        self.stream_calls.clear();
                        self.status = aox::tf!("Retrying in {} s", format!("{:.1}", delay_ms as f32 / 1000.0));
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
        // Read the registry and hand its workspace roots to the store **before** listing. `list()`
        // only scans the workspaces the store has been told about, so doing this afterwards would
        // make the first refresh after a workspace is added (or removed) draw the **old** tree --
        // and in the add case the new workspace's conversations would be missing from it.
        //
        // The registry is re-read every time (one small JSON file): a workspace can change while the
        // app runs, and a cached copy would keep drawing a tree that no longer matches the disk.
        let root = self.app_root();
        // Fold state, read **once** — the first refresh that knows the root (see
        // `App::hist_fold_loaded`). Doing it here rather than in the boot arm means the `--demo`
        // constructor, which sets up its own synthetic boot and then refreshes, is covered too.
        //
        // Only the folds come from the file. The drawer's own position is deliberately **not**
        // remembered (user request, 2026-10-18): every launch starts folded, snapped rather than
        // animated — a sidebar sliding open by itself on launch reads as a glitch, not as a welcome.
        if !self.hist_fold_loaded {
            if let Some(root) = root.as_deref() {
                let fs = uistate::FoldState::load(root);
                self.hist_open = fs.open;
                self.hist_closed = fs.closed;
                self.hist_fold_loaded = true;
            }
        }
        let reg = root.as_deref().map(aox::space::Registry::load);
        if let (Some(root), Some(reg)) = (root.as_deref(), reg.as_ref()) {
            store.set_workspaces(&reg.workspace_roots(root));
        }
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
        // Group the (already newest-first) list into workspaces, for the drawer's tree.
        //
        // Done here rather than in the drawer: the drawer redraws at frame rate, and grouping
        // walks and re-sorts the whole list. There is a useful side effect of the pinning above --
        // `group` orders groups by their newest item's position in the list it is handed, so the
        // workspace holding the current conversation comes out first among the user's workspaces,
        // and the current conversation is first inside its group. That is the two-level pin for free.
        self.hist_groups = match (root.as_deref(), reg.as_ref()) {
            (Some(root), Some(reg)) => aox::space::group(root, reg, items.clone()),
            _ => Vec::new(),
        };
        self.history = items;
    }

    /// The application root, as the bridge resolved it.
    ///
    /// Taken from the boot info rather than re-derived from `store.location()`: the bridge is the
    /// one place that decides the root (it honours `--config`-style overrides), and a second
    /// derivation would silently disagree with it the day that logic changes.
    fn app_root(&self) -> Option<PathBuf> {
        self.boot.as_ref().map(|b| PathBuf::from(&b.paths.root))
    }

    /// Drop a workspace from AOX: **registration only, nothing on disk is touched**.
    ///
    /// Writes the `hidden` mark into the registry so the folder stops being listed *and* stops being
    /// scanned -- dropping it from the spaces alone would not do, because `group()` invents a space
    /// for any unknown root it meets, so the row would be back on the very next refresh.
    ///
    /// A failed write is swallowed on purpose: the visible result of "the file could not be saved" is
    /// the row still being there, which is the same thing the user sees if they cancel. There is no
    /// separate error surface for this, and a toast for a UI-organisation file would be noise.
    fn forget_workspace(&mut self, root: &Path) {
        let Some(app_root) = self.app_root() else {
            return;
        };
        if let Err(e) = (|| -> anyhow::Result<()> {
            let mut reg = aox::space::Registry::load(&app_root);
            reg.remove_workspace(root);
            reg.save(&app_root)
        })() {
            // The row staying on screen *is* the failure report; put the reason in the log where a
            // developer can find it (`log_error` shows on screen, which this does not deserve).
            eprintln!("aox-gui: could not save the space registry: {e:#}");
            return;
        }
        // The fold state is deliberately **not** reset here. The removed group's id stops being drawn
        // (`hist_groups` is rebuilt without it), so whatever it left in the two sets is inert -- and
        // clearing them would mean "I dropped one workspace and all my other groups jumped open",
        // which is a side effect on places the user did not touch.
        self.refresh_history();
    }

    /// Create a workspace, register it, and move this conversation into it.
    ///
    /// Three steps, in this order and each for its own reason:
    ///
    /// 1. **The folder**, under `<app root>/workplace/` — the layout convention the system prompt
    ///    already describes to the model, so "make yourself a project folder" and this button agree
    ///    about where projects live. An **existing** folder is adopted instead of refused: putting a
    ///    directory you already have back into the tree is the same gesture as making a new one, and
    ///    `create_dir_all` is a no-op when it is already there. Nothing is ever overwritten.
    /// 2. **The registry**, which is what makes the group exist at all. The tree is rebuilt from that
    ///    file on every refresh, so a folder nobody registered is not a workspace, it is just a folder.
    /// 3. **The conversation's own workspace**, over the bridge: the root only decides where writes
    ///    may land once the agent thread has applied it (`ToAgent::Workplace`), because the tier
    ///    belongs to the conversation, not to the window. The bridge answers with a `Workplace` push,
    ///    which is what renames the chip at the bottom of the input box -- this call is a request.
    ///
    /// Returns the reason it failed, for the dialog to show. Both are things the user can act on (a
    /// name that cannot be a folder, a directory that cannot be written), which is why the message
    /// belongs in the dialog rather than in a toast that has already faded.
    fn create_workspace(&mut self, raw: &str, app_root: Option<&Path>) -> Result<(), String> {
        let Some(app_root) = app_root else {
            return Err(aox::i18n::t("AOX does not know its own application root").to_string());
        };
        let name = check_ws_name(raw)?;
        let dir = app_root.join(aox::workplace::WORKPLACE_DIR).join(&name);
        std::fs::create_dir_all(&dir)
            .map_err(|e| aox::tf!("Could not make the workspace: {}", e))?;

        let mut reg = aox::space::Registry::load(app_root);
        let id = reg.add_workspace(&dir, Some(name));
        reg.save(app_root)
            .map_err(|e| aox::tf!("Could not make the workspace: {}", format!("{e:#}")))?;

        // The tree is rebuilt from the registry, so the new group is on screen from this frame on.
        self.refresh_history();
        // Unfold it, so the group the button just made is visibly there instead of being one more
        // folded header among the others. The stale "explicitly folded" mark is dropped first --
        // re-adding a folder that had been removed lands on the same id, and a group the user once
        // folded must not come back folded when they have just asked for it by name.
        self.hist_closed.remove(&id);
        self.hist_open.insert(id);
        // The unfold is a fold-state change like any other, so it is written through the same path --
        // otherwise "create a workspace, restart" would show the new group folded again.
        self.save_fold_state();
        // ...and move this conversation in, so the next thing written lands in the new folder. The
        // bridge answers with a `Workplace` push, which is what renames the chip at the bottom of the
        // input box -- this call is a request, not a state change.
        self.send_ws(Mode::Fixed(dir));
        Ok(())
    }

    /// Open the rename dialog for a workspace, pre-filled with the name the tree is showing.
    ///
    /// The current name is read from the **drawn group** rather than from a fresh registry read:
    /// they are the same value, and this way the name in the field is exactly the one the user just
    /// right-clicked, with no chance of the two disagreeing mid-frame.
    fn start_rename_workspace(&mut self, root: &Path) {
        let current = self
            .hist_groups
            .iter()
            .find(|g| g.root.as_deref() == Some(root))
            .map(|g| g.name.clone())
            .unwrap_or_default();
        self.name_dialog = Some(NameDialog::renaming(root.to_path_buf(), current));
    }

    /// Rename a workspace's **label**, and nothing else. See [`aox::space::Registry::rename_workspace`]
    /// for why the folder is not moved; from here it matters that this is a registry edit only, so it
    /// cannot disturb the conversation being chatted in, and it does not go near the bridge.
    ///
    /// `None` for `raw` (or a blank name) means "show the folder's own name again" — the rename
    /// dialog's working undo, since a label equal to the folder name is what "no name" already
    /// displays as. Anything else goes through **the same** validation as creating: the name lands in
    /// the same file and the same tree, and a looser rule here would just be a second way to write a
    /// name the create dialog refuses.
    fn rename_workspace(&mut self, root: &Path, raw: Option<&str>) -> Result<(), String> {
        let Some(app_root) = self.app_root() else {
            return Err(aox::i18n::t("AOX does not know its own application root").to_string());
        };
        let name = match raw.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => Some(check_ws_name(n)?),
            None => None,
        };
        let mut reg = aox::space::Registry::load(&app_root);
        reg.rename_workspace(root, name);
        reg.save(&app_root)
            .map_err(|e| aox::tf!("Could not rename the workspace: {}", format!("{e:#}")))?;
        // The fold state is deliberately **not** touched: a group's header is on screen whether it is
        // folded or not, so the new name is visible either way — and forcing the group open would be
        // an unasked-for change to a place the user did not click. (Creating a workspace does unfold,
        // because there the group is brand new and folded is how it would arrive.)
        self.refresh_history();
        Ok(())
    }

    /// Open the rename dialog for a conversation, pre-filled with the title the row is showing.
    ///
    /// Read from the **drawn row** for the same reason as the workspace case: it is the exact string
    /// the user just right-clicked, with no chance of the dialog and the row disagreeing.
    fn start_rename_conversation(&mut self, id: &str, current: String) {
        self.name_dialog = Some(NameDialog::renaming_conversation(id.to_string(), current));
    }

    /// Rename one conversation's **label**, and nothing else: the id (the filename), the workspace it
    /// is bound to and every message in it are all untouched. A `kind:"title"` row is appended to the
    /// conversation's own file (see [`aox::store::Session::append_title`]) — which is what makes this
    /// survive a rollback, and why the newest row is the one that counts.
    ///
    /// `None` (or a blank name) means "show the automatic title again" — the first user message. That
    /// is the dialog's working undo, and it needs the store's nested `Option` to be distinguishable
    /// from "never renamed": a cleared label is a row with no `title` key, not the absence of a row.
    ///
    /// This goes **over the bridge** rather than touching the file here. The conversation being
    /// chatted in has its file owned by the agent thread (that is where the append handle lives), so
    /// writing it from the UI thread would be a second writer on the same file; and for any other
    /// conversation the agent is the one that owns the store handle. So this is a request — the tree
    /// is rebuilt when the bridge answers with a fresh history list.
    fn rename_conversation(&mut self, id: &str, raw: Option<&str>) -> Result<(), String> {
        let title = match raw.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => Some(check_conv_title(t)?),
            None => None,
        };
        let _ = self.tx.send(ToAgent::RenameConversation {
            id: id.to_string(),
            title,
        });
        Ok(())
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

    /// Start a **new conversation bound to a particular workspace** — the "＋" on a group header, and
    /// the same row in its right-click menu.
    ///
    /// Two messages, and the **order is the whole implementation**: `NewConversation` first, then
    /// `Workplace(Fixed(root))`. The agent's `new_conversation` resets the workspace tier to `Default`,
    /// so sending the root first would have it wiped a moment later and the new thread would land back
    /// in the app root — the exact opposite of what the row promised. Both go down one channel, which
    /// the agent drains in order, so "first" here really is first there.
    ///
    /// The early return for an already-empty conversation is deliberately **not** copied from
    /// [`App::new_conversation`]: there, doing nothing is right, because the point is "give me a fresh
    /// one" and a fresh one is already on screen. Here the point is **where the next thing gets
    /// written**, and a blank conversation that is still bound to the app root is not in the workspace
    /// the user just clicked — so the `Workplace` half has to go through in that case too.
    ///
    /// Like every other conversation switch this is a request: the UI moves when the bridge answers
    /// (`FromAgent::Switched`, `FromAgent::Workplace`).
    fn new_conversation_in(&mut self, root: &Path) {
        if self.busy || self.switching {
            return;
        }
        if self.demo {
            // No agent thread to answer, so put the one thing the demo can show -- the workspace the
            // chip at the bottom of the input box names -- into place the same way `send_ws` does.
            self.send_ws(Mode::Fixed(root.to_path_buf()));
            return;
        }
        self.switching = true;
        let _ = self.tx.send(ToAgent::NewConversation);
        let _ = self.tx.send(ToAgent::Workplace(Mode::Fixed(root.to_path_buf())));
    }

    /// Re-file a conversation into another group — **a registry edit only**; nothing on disk moves.
    ///
    /// See [`aox::space::Registry::place_conversation`] for what "placement" does and does not mean
    /// (the file stays put, the write boundary is untouched). From here the interesting part is the
    /// target: the menu hands back a **group id**, which may be an `auto:` id for an unregistered root,
    /// so the root is read off the drawn tree rather than assumed to be in the registry.
    ///
    /// The destination is unfolded on the way, so the moved row is visibly there instead of vanishing
    /// into a folded group — the same gesture `create_workspace` makes for the group it just made. A
    /// move whose destination you cannot see is the one outcome worth spending a fold on.
    fn move_conversation(&mut self, conv_id: &str, target_group_id: &str) {
        let Some(app_root) = self.app_root() else {
            return;
        };
        // The root comes off the **drawn** group, which is what lets an auto group (a root the
        // registry has never heard of) be registered by the placement itself.
        let target_root = self
            .hist_groups
            .iter()
            .find(|g| g.id == target_group_id)
            .and_then(|g| g.root.clone());
        let mut reg = aox::space::Registry::load(&app_root);
        let id = reg.place_conversation(conv_id, target_group_id, target_root.as_deref());
        if let Err(e) = reg.save(&app_root) {
            // As with the other registry edits: the row not moving *is* the failure report.
            eprintln!("aox-gui: could not save the space registry: {e:#}");
            return;
        }
        // Rebuilding the tree is what actually moves the row between groups.
        self.refresh_history();
        // Unfold the destination -- under the id it ended up with, which differs from the target's when
        // the target was an auto group that the placement just registered.
        self.hist_closed.remove(&id);
        self.hist_open.insert(id);
        self.save_fold_state();
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
        // The cache map is swapped with the rest: it describes "the conversation I am in", and the
        // ledger it is fed from has just been cleared. The window itself stays open (it is a lens on
        // whatever conversation is current, not a property of one), and its position is left alone.
        cache::lock(&self.cache).reset();
        // The workspace view's edit ledger belongs to the conversation too: it lists what **this**
        // conversation's current turn touched, and the turn count restarts here. The window stays
        // open (it is a lens on the current workspace, not a property of one conversation).
        wsview::reset(&self.wsview);
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

    /// The workspace drawer: a **resident left column that pushes the transcript aside**.
    ///
    /// It used to be a card floating over the transcript. That read as a pop-up, and a pop-up is
    /// something you dismiss; this is a place you work from, so it takes part in the layout instead:
    /// `SidePanel::left` shrinks the central column by exactly its width and gives it back when the
    /// drawer is put away. Nothing is covered, no scrim is needed, and the transcript is never
    /// drawn underneath a panel it cannot be read through.
    ///
    /// # How the push is done
    ///
    /// The **panel width** is what animates, so the eased value *is* the push: at any frame the
    /// transcript has given up exactly as much room as the drawer occupies. The content is laid out
    /// to that same width rather than at a fixed 300, which means it squeezes on the way in exactly
    /// like a sidebar being dragged wider — the alternative (fixed-width content riding the panel's
    /// edge) would let the content's min-rect overflow the panel, and egui would then hand the
    /// central column the *content's* width instead of the panel's, killing the animation.
    ///
    /// The horizontal inset eases with the width for the same reason: a 12px margin inside a 3px
    /// panel is a negative content rect.
    ///
    /// # Where the closing affordances went
    ///
    /// There is no "click outside" and no Esc-to-close any more. A resident column that disappears
    /// because the pointer landed on the transcript is a column you stop trusting; the drawer's head
    /// carries the one control that puts it away ([`sidebar_button`]), and Esc belongs to the
    /// search box (it clears the query).
    fn drawer(&mut self, ctx: &egui::Context) {
        if self.hist_t <= 0.0 {
            // Fully closed: drop the query. It is done here rather than at each door that closes
            // the drawer (the collapse button, a conversation switch) because there is exactly one
            // state that means "not showing", and a stale filter would greet the next open with a
            // list that looks like it lost its rows.
            self.hist_query.clear();
            return;
        }
        let ease = ease_out_cubic(self.hist_t);
        // Clicks are refused while a switch request is in flight or a turn is running: a switch
        // started during either of those takes effect only later in the queue, and all the
        // clicker sees is the UI "hesitating before it jumps".
        let locked = self.busy || self.switching;

        // ── geometry: a column that takes part in the layout ──
        //
        // The eased width **is** the push: whatever the drawer takes, the transcript gives up, and
        // both numbers come from the same expression. A panel narrower than its own margin would
        // hand its content a negative rect, so the inset eases with it instead of being a constant.
        let w = (HISTORY_W * ease).max(1.0);
        let inset = (12.0 * ease).round() as i8;
        let pw = (w - 2.0 * inset as f32).max(8.0);
        // The window's height comes from **the largest of two independent reports**, because they
        // can disagree: `content_rect` is what the rest of this file uses for coordinates, but on a
        // window whose DPI the backend reports differently it can come back at a fraction of the
        // real client area. Taking the max is safe in both directions: the value only ever bounds
        // a height.
        //
        // ⚠️ Compare **heights**, not `bottom()`s: `viewport().inner_rect` is in *screen*
        // coordinates (its `min` is the window's position on the desktop), so its `bottom()` is
        // where the window ends on the **monitor**, not how tall it is.
        let win_h = ctx.content_rect().height().max(
            ctx.input(|i| i.viewport().inner_rect.map_or(0.0, |r| r.height())),
        );
        // What the scrolling list gets: the window, minus the head and the foot, minus the frame's
        // bottom margin. Both constants are plain sums of what is drawn (see their docs), because
        // the drawer places every gap by hand instead of letting `item_spacing` do it.
        let list_h = (win_h - DRAWER_HEAD - DRAWER_FOOT - 12.0).max(120.0);

        let mut want_new_ws = false;
        let mut want_open_ws = false;
        let mut want_new_conv = false;
        let mut want_settings = false;
        let mut want_close = false;
        // Read once, for the foot row that opens `<app root>/workplace/`: "is there a root at all"
        // is also what decides whether that row is clickable.
        let app_root = self.app_root();
        // The search box's text, lower-cased **once**: `hist_tree` asks "does this row match" for
        // every row it draws, and folding the case inside that loop would be the same work N times.
        let query = self.hist_query.trim().to_lowercase();
        let mut open: Option<String> = None;
        // Group folds requested this frame. Collected rather than applied in place: the tree is
        // drawn inside the `Area` closure, so `&mut self` edits have to wait until after it returns
        // (the same shape `open` already used for switching).
        let mut toggles: Vec<String> = Vec::new();
        // Row / header requests, collected for the same reason as `toggles`. One slot per
        // destination rather than one enum because each lands somewhere different afterwards (a
        // message to the agent / a registry edit / a shell open / a dialog), and a single click can
        // only ever produce one of them.
        let mut recycle: Vec<PathBuf> = Vec::new();
        let mut forget_ws: Option<PathBuf> = None;
        let mut reveal_ws: Option<PathBuf> = None;
        let mut rename_ws: Option<PathBuf> = None;
        // "Start a conversation in this workspace" -- fed by the header's hover "＋" and by the
        // workspace menu's first row. One slot for both because they are one action with two doors.
        let mut new_conv_ws: Option<PathBuf> = None;
        // "Rename this conversation", as `(id, the title currently shown)`. The second half is what
        // pre-fills the dialog, and it carries the **title** rather than a path because a conversation
        // rename never touches the filesystem (see `NameDialogMode::RenameConversation`).
        let mut rename_conv: Option<(String, String)> = None;
        // "Re-file this conversation into that group", as `(conversation id, group id)`. The group id
        // is the id the tree drew -- possibly an `auto:` one -- and the lookup back to its root happens
        // in `move_conversation`, which has the drawn tree in hand.
        let mut move_conv: Option<(String, String)> = None;

        let panel = egui::SidePanel::left("aox-drawer")
            // Not resizable: the width is the animation's own output, and a drag handle would be
            // fighting it for the same number.
            .resizable(false)
            .exact_width(w)
            // The drawer's only chrome is the hairline drawn below. The built-in separator is the
            // same line in a theme colour that is not this one, and it would also be drawn for a
            // panel that is 3px wide mid-animation.
            .show_separator_line(false)
            .frame(
                Frame::NONE
                    .fill(P::surface())
                    .inner_margin(Margin { left: inset, right: inset, top: 12, bottom: 12 }),
            )
            .show(ctx, |ui| {
                ui.set_width(pw);
                // Every gap in this panel is placed by hand. `item_spacing` is a theme-wide 10, and
                // a column this dense cannot afford ten unaccounted pixels between every widget --
                // it is also what lets `DRAWER_HEAD` / `DRAWER_FOOT` be exact sums instead of
                // "measured, and re-measured whenever a row moves".
                ui.spacing_mut().item_spacing = Vec2::ZERO;

                // ── head: collapse, then settings at the far end ──
                ui.horizontal(|ui| {
                    // The **same** button that opened the drawer (see `sidebar_button`): one
                    // control, two places, so it reads as a toggle instead of two unrelated icons.
                    if sidebar_button(ui, true, false).clicked() {
                        want_close = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if gear_button(ui).clicked() {
                            want_settings = true;
                        }
                        // The empty gap between the two icons is the window's drag handle. The
                        // title bar (`App::titlebar`) starts at this column's right edge, so
                        // without this the top-left corner of the window would be the one patch
                        // you cannot move it by — and it reads as the bar, so it has to behave
                        // like the bar: drag to move, double-click to maximize.
                        let gap = ui.available_rect_before_wrap();
                        let resp =
                            ui.interact(gap, ui.id().with("head-drag"), Sense::click_and_drag());
                        if resp.drag_started() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                        }
                        if resp.double_clicked() {
                            let maxed = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maxed));
                        }
                    });
                });
                ui.add_space(10.0);
                if new_chat_row(ui, pw, !locked).clicked() && !locked {
                    want_new_conv = true;
                }
                ui.add_space(10.0);
                search_row(ui, pw, &mut self.hist_query);
                ui.add_space(18.0);
                section_head(
                    ui,
                    pw,
                    aox::i18n::t("Workspaces"),
                    &aox::tf!("{}", self.hist_groups.len()),
                );
                ui.add_space(7.0);

                // Is there anything in the tree other than the two built-ins? A registered
                // workspace counts as "something" **even while it is empty** -- that is exactly
                // the state the "new workspace" button leaves behind -- so the friendly
                // first-run screen below is reserved for a genuinely empty install.
                let custom_group = self.hist_groups.iter().any(|g| g.builtin.is_none());

                // `auto_shrink([false, false])` is the load-bearing part: the scroll area
                // takes the height we decided instead of collapsing back onto its content,
                // which is what stops the panel resizing itself whenever a group is folded.
                //
                // `min_scrolled_height` is the *other* half, and it is not redundant: a
                // `ScrollArea` sizes itself from `ui.available_rect_before_wrap()`, so
                // "take all available space" is only as good as that number. Forcing the
                // inner height is what actually makes `list_h` real.
                ScrollArea::vertical()
                    .max_height(list_h)
                    .min_scrolled_height(list_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Rows sit 3 apart; the tree adds its own gaps between groups.
                        ui.spacing_mut().item_spacing.y = 3.0;
                        // The empty state is drawn **inside** the scroll area rather than
                        // short-circuiting the panel. It has to be: "no conversations yet" is
                        // not the same statement as "no workspaces yet", and returning early
                        // here would hide the very group a workspace just created shows up as
                        // -- and the button that made it, which sits below the list.
                        if self.history.is_empty() && !custom_group {
                            ui.label(
                                RichText::new(
                                    aox::i18n::t("No history yet.\nAfter your first message, the conversation is saved here automatically."),
                                )
                                .color(P::text_faint())
                                .size(12.5),
                            );
                            ui.add_space(10.0);
                            // A directory path can be long; use truncate so it closes itself
                            // off within the panel width instead of being cut by the panel
                            // boundary.
                            ui.add(
                                Label::new(
                                    RichText::new(
                                        self.store
                                            .as_ref()
                                            .map(|s| s.location().display().to_string())
                                            .unwrap_or_default(),
                                    )
                                    .color(P::text_faint())
                                    .size(11.0),
                                )
                                .truncate(),
                            );
                        } else if !query.is_empty()
                            && !self.hist_groups.iter().any(|g| group_matches(g, &query))
                        {
                            // Said out loud rather than drawn as an empty column: with a
                            // query in the box, "nothing here" and "the list is broken"
                            // look identical otherwise.
                            ui.add_space(2.0);
                            ui.label(
                                RichText::new(aox::i18n::t("No matches"))
                                    .color(P::text_faint())
                                    .size(12.0),
                            );
                        } else {
                            self.hist_tree(
                                ui,
                                pw,
                                locked,
                                &query,
                                &mut open,
                                &mut toggles,
                                &mut recycle,
                                &mut forget_ws,
                                &mut reveal_ws,
                                &mut rename_ws,
                                &mut new_conv_ws,
                                &mut rename_conv,
                                &mut move_conv,
                            );
                        }
                    });

                // The foot: the actions that are not about a row. Pinned outside the
                // scroll area, so they cannot be scrolled away by a workspace with eighty
                // conversations in it.
                ui.add_space(8.0);
                let x0 = ui.cursor().left();
                let y0 = ui.cursor().top();
                ui.painter()
                    .hline(x0..=(x0 + pw), y0, Stroke::new(1.0, P::border()));
                ui.add_space(8.0);
                // "New workspace" only goes live when no turn is running: it hands this
                // conversation to a new root, which is a workspace switch like any other.
                if foot_row(ui, pw, !locked, FootIcon::Plus, aox::i18n::t("New workspace"))
                    .clicked()
                {
                    want_new_ws = true;
                }
                ui.add_space(2.0);
                // ...whereas opening the folder touches nothing at all, so this row stays
                // live during a turn. Disabled only when the app root is unknown -- which is
                // the one case with no folder to name and no path to hand the shell.
                if foot_row(
                    ui,
                    pw,
                    app_root.is_some(),
                    FootIcon::Folder,
                    aox::i18n::t("Open workspace folder"),
                )
                .clicked()
                {
                    want_open_ws = true;
                }

                // Resize hot-zones for the parts of the window's border that live in this column:
                // the whole left edge and the bottom-left corner (the drawer is a full-height
                // column, and the top row is an `Area` that stops at this column's right edge).
                // `ui.interact` is clipped to the panel's rect, so this call and the two in
                // `titlebar` / the central panel add up to exactly the eight zones -- see
                // `titlebar`.
                gwin::resize_handles(ui, ctx, ctx.content_rect());
            });
        // The drawer's only chrome: one hairline down the full height, painted from the panel's own
        // rect (so it lands on the moving edge for the whole animation) and **after** the content,
        // so nothing can draw over it.
        let r = panel.response.rect;
        ctx.layer_painter(panel.response.layer_id)
            .vline(r.right() - 0.5, r.top()..=r.bottom(), Stroke::new(1.0, P::border()));

        // Esc belongs to the search box: with a query typed, "get me out of this" means "show me
        // everything again". It deliberately does **not** close the drawer -- a resident column that
        // vanishes on a stray Esc is a column you stop trusting, and the head has the one control
        // that puts it away on purpose.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !self.hist_query.is_empty() {
            self.hist_query.clear();
        }

        for id in toggles {
            self.toggle_group(&id);
        }
        // The order here is deliberate: the workspace edits come before the refresh, so the refresh
        // sees the registry as it is *after* the edit (see `refresh_history`, which re-reads it).
        if let Some(root) = reveal_ws {
            reveal_in_file_manager(&root);
        }
        if let Some(root) = forget_ws {
            self.forget_workspace(&root);
        }
        if let Some(root) = rename_ws {
            // Opens the dialog; nothing is written until the name is confirmed. Unlike the two above
            // this is not an edit at all yet, which is why it is here rather than among them.
            self.start_rename_workspace(&root);
        }
        if let Some((id, current)) = rename_conv {
            // Same shape as the workspace rename: the dialog is opened, and nothing is written until
            // "Save". The pre-fill is the title the row was showing, so an empty field visibly means
            // "back to the first message".
            self.start_rename_conversation(&id, current);
        }
        if let Some((conv, target)) = move_conv {
            // A registry edit, done here (after the `Area` closure) because it needs `&mut self`.
            // Unlike the rename above it writes immediately -- moving a row has no dialog to confirm.
            self.move_conversation(&conv, &target);
        }
        if let Some(root) = new_conv_ws {
            // New conversation **bound to that workspace** -- two messages down the queue, in an order
            // that matters (see `new_conversation_in`). The panel stays open: the new thread is empty
            // and its row only appears once something is written, so closing the panel here would make
            // the click look like it did nothing.
            self.new_conversation_in(&root);
        }
        if !recycle.is_empty() {
            // Handed to the agent thread rather than recycled here: the move is a blocking shell
            // call, and it also keeps one code path for "a conversation was deleted" -- the toast
            // and the list refresh then come back through the same `AgentEvent::Recycled` the
            // model's own deletions use.
            let _ = self.tx.send(ToAgent::Recycle(recycle));
        }
        if want_new_ws {
            // Just opens the dialog. The folder is not touched until the user has named it and hit
            // "Create" -- this is the one action in the panel that writes to disk, so it gets a step
            // of its own rather than happening under the pointer.
            self.name_dialog = Some(NameDialog::new());
        }
        if want_open_ws {
            if let Some(root) = app_root.as_deref() {
                let dir = root.join(aox::workplace::WORKPLACE_DIR);
                // Made if it is not there yet: `ensure_started` normally creates it, but the panel can
                // be opened before the agent thread has run, and handing `explorer` a path that does
                // not exist opens nothing at all -- a button that silently does nothing.
                let _ = std::fs::create_dir_all(&dir);
                reveal_in_file_manager(&dir);
            }
        }
        if want_new_conv {
            // Same path the toolbar's "+" used to take. Refused while a turn runs for the same
            // reason `new_chat_row` is drawn disabled then: the switch would only land after the
            // turn, and the click would look like it did nothing.
            self.new_conversation();
        }
        if want_settings {
            self.settings_open = true;
        }
        if want_close {
            // Not persisted: the drawer always starts folded (see the fold-state load in
            // `refresh_history`). Closing it is not a preference, it is "done with it for now".
            self.show_history = false;
        }
        if let Some(id) = open {
            self.switch_history(&id);
        }
    }

    /// Draw the workspace-grouped conversation tree inside the drawer's scroll area.
    ///
    /// Takes `&self` -- the drawer's `SidePanel` closure only holds an immutable borrow -- and hands
    /// everything it wants to change back to the caller: `open` (switch to this conversation),
    /// `toggles` (fold / unfold this group), and the seven right-click requests `recycle` /
    /// `forget_ws` / `reveal_ws` / `rename_ws` / `new_conv_ws` / `rename_conv` / `move_conv` (delete
    /// this conversation / drop this workspace / open its folder / rename it / start a conversation in
    /// an existing workspace / rename a conversation / re-file a conversation into another group).
    ///
    /// The groups arrive from [`aox::space::group`] in a **fixed order** (Default, Full access, then
    /// the user's workspaces, most recently active first). Exactly one adjustment is made on top:
    /// the workspace holding the current conversation is pulled to the very top, so the place you
    /// are in is the first thing in the panel -- the tree's counterpart of the old flat list pinning
    /// the current row. It is applied as an index order, so the groups themselves are never touched.
    #[allow(clippy::too_many_arguments)] // one `&mut` slot per thing the tree can ask for; see above
    fn hist_tree(
        &self,
        ui: &mut Ui,
        width: f32,
        locked: bool,
        query: &str,
        open: &mut Option<String>,
        toggles: &mut Vec<String>,
        recycle: &mut Vec<PathBuf>,
        forget_ws: &mut Option<PathBuf>,
        reveal_ws: &mut Option<PathBuf>,
        rename_ws: &mut Option<PathBuf>,
        new_conv_ws: &mut Option<PathBuf>,
        rename_conv: &mut Option<(String, String)>,
        move_conv: &mut Option<(String, String)>,
    ) {
        let mut order: Vec<usize> = (0..self.hist_groups.len()).collect();
        if let Some(cur) = self.current_group_id() {
            if let Some(i) = self.hist_groups.iter().position(|g| g.id == cur) {
                order.retain(|&j| j != i);
                order.insert(0, i);
            }
        }

        let cur = self.current_history.as_deref();
        // With a query in the box the tree is **flattened open**. Matching a conversation that sits
        // inside a folded workspace and then showing nothing would be a filter that hides its own
        // results; the fold state is left alone on disk, it just does not apply while filtering.
        let filtering = !query.is_empty();
        let mut drawn = 0usize;
        for &gi in order.iter() {
            let g = &self.hist_groups[gi];
            // A workspace whose **name** matches keeps all of its conversations (you searched for
            // the place, not for a title); otherwise only the rows whose title matches survive.
            let name_hit = !filtering || g.name.to_lowercase().contains(query);
            let rows: Vec<&aox::history::Summary> = if name_hit {
                g.items.iter().collect()
            } else {
                g.items.iter().filter(|s| s.title.to_lowercase().contains(query)).collect()
            };
            if filtering && !name_hit && rows.is_empty() {
                continue;
            }
            if drawn > 0 {
                // A breath between groups; without it the last row of one group sits flush against
                // the next group's header and the whole thing reads as one undifferentiated list.
                ui.add_space(3.0);
            }
            drawn += 1;
            let unfolded = filtering || self.group_is_open(&g.id);
            let (head, plus) = history_group_header(ui, g, width, rows.len(), unfolded);
            if head.clicked() && !filtering {
                toggles.push(g.id.clone());
            }
            // The header's own "＋" and its menu row say the same thing, so they feed the same slot.
            // A group with no root has no "＋" (see `history_group_header`), so the root here is a
            // formality -- but it is still read from `g` rather than assumed.
            if plus {
                if let Some(root) = g.root.clone() {
                    *new_conv_ws = Some(root);
                }
            }
            // Only a real folder gets a menu. `Default` and `Full access` have no root of their own
            // (and are not the user's to remove), so neither "new conversation", "rename", "open in
            // the file manager" nor "remove from AOX" has anything to act on -- a menu whose every
            // row is dead is worse than no menu at all.
            if let Some(root) = g.root.clone() {
                head.context_menu(|ui| {
                    // First row, because it is the one thing this menu can do that the header cannot
                    // do without hovering: make a conversation here. Not gated on `locked` -- the
                    // click goes down the same queue as the "＋", which refuses a switch while a turn
                    // runs by doing nothing, and graying the row out here would only be a second,
                    // silent copy of that rule.
                    if menu_item(ui, aox::i18n::t("New conversation"), MENU_W, true, false).clicked() {
                        *new_conv_ws = Some(root.clone());
                        ui.close();
                    }
                    // Renaming here moves **the label, never the folder**: the conversations record
                    // the folder they were written to, so a folder move would strand every one of
                    // them under a root the registry no longer knows (see
                    // `Registry::rename_workspace`). The dialog says which part is being renamed --
                    // a menu row that has to explain itself is a menu row nobody reads.
                    if menu_item(ui, aox::i18n::t("Rename"), MENU_W, true, false).clicked() {
                        *rename_ws = Some(root.clone());
                        ui.close();
                    }
                    if menu_item(ui, aox::i18n::t("Open in file manager"), MENU_W, true, false)
                        .clicked()
                    {
                        *reveal_ws = Some(root.clone());
                        ui.close();
                    }
                    // Deliberately not styled as a danger action and not confirmed: nothing is
                    // deleted. The folder, its conversations and everything inside stay exactly
                    // where they are; this only drops the row from the tree, and re-adding the
                    // folder is a working undo (see `Registry`'s `hidden` mark).
                    if menu_item(ui, aox::i18n::t("Remove from AOX"), MENU_W, true, false).clicked() {
                        *forget_ws = Some(root.clone());
                        ui.close();
                    }
                });
            }
            if !unfolded {
                continue;
            }
            // The "Move to" targets: every drawn group except this one. Built once per group rather
            // than per row, since the list is identical for every row inside it. It is a pair of
            // (group id, display name): the id is what the move records, the name is what the menu
            // shows -- and the name is never re-derived from the id, because for `Default`/`Full
            // access` and for a renamed workspace they have nothing to do with each other.
            let move_targets: Vec<(String, String)> = self
                .hist_groups
                .iter()
                .filter(|x| x.id != g.id)
                .map(|x| (x.id.clone(), x.name.clone()))
                .collect();
            for s in rows {
                let current = Some(s.id.as_str()) == cur;
                let (row, more) = history_row(ui, s, current, locked, width, HIST_INDENT);
                if row.clicked() && !locked {
                    *open = Some(s.id.clone());
                }
                // A row's menu is offered **even while a turn runs**: deleting an old conversation
                // touches nothing the running turn holds (the move happens on the agent thread).
                // Only *switching* has to wait for the turn, and that is the `locked` gate above.
                //
                // Two doors onto **one** menu: the row's right-click, and the "⋯" that appears on
                // hover. The contents live in `conversation_menu` rather than in a closure so both
                // doors can hand it the same `&mut` slots (a closure could only be consumed once).
                let path = s.path.clone();
                let id = s.id.clone();
                // What the dialog's field is pre-filled with. The **title**, not the id: the id is a
                // timestamp, and "edit the timestamp" is not the thing anybody right-clicked.
                let title = s.title.clone();
                let mut menu = |ui: &mut Ui| {
                    conversation_menu(
                        ui,
                        &id,
                        &title,
                        &path,
                        current,
                        &move_targets,
                        rename_conv,
                        move_conv,
                        recycle,
                    );
                };
                row.context_menu(&mut menu);
                // The "⋯" only exists on a hovered (or current) row, so this popup can only ever be
                // opened from a visible affordance -- but it is registered every frame regardless,
                // because a popup that is only *created* on the frame of the click has no id to
                // remember being open by.
                egui::Popup::menu(&more).show(&mut menu);
            }
        }
    }

    /// Whether the workspace group `id` is drawn unfolded (see [`App::hist_open`] / [`App::hist_closed`]).
    fn group_is_open(&self, id: &str) -> bool {
        if self.hist_closed.contains(id) {
            return false;
        }
        if self.hist_open.contains(id) {
            return true;
        }
        match self.current_group_id() {
            Some(cur) => cur == id,
            // Nothing is on screen yet (a brand-new conversation that has not reached disk), so
            // there is no "where you are" to point at. Unfold everything rather than greet the user
            // with a wall of headers and nothing underneath.
            None => true,
        }
    }

    /// Fold / unfold a workspace group.
    ///
    /// Flips relative to **what is on screen right now** rather than inserting into a chosen set:
    /// the effective state is the two explicit sets plus the default, so a caller holding only the
    /// id would have to re-derive all three just to know which set to write to.
    fn toggle_group(&mut self, id: &str) {
        let now = self.group_is_open(id);
        self.hist_open.remove(id);
        self.hist_closed.remove(id);
        if now {
            self.hist_closed.insert(id.to_string());
        } else {
            self.hist_open.insert(id.to_string());
        }
        // Persisted right away: a fold is a deliberate click, it is cheap to write (one tiny file),
        // and deferring it would mean choosing a moment to flush that always turns out to be the
        // wrong one (the app being killed is exactly when nothing gets flushed).
        self.save_fold_state();
    }

    /// Write the two fold sets to `<app root>/.aox/ui.json`.
    ///
    /// Best-effort, like the registry: a failure is logged, never surfaced. The tree keeps working
    /// with the in-memory sets either way, so a failure only costs "the fold is not remembered next
    /// launch" — not worth an error the user has to dismiss.
    fn save_fold_state(&self) {
        let Some(root) = self.app_root() else {
            return;
        };
        let fs = uistate::FoldState {
            open: self.hist_open.clone(),
            closed: self.hist_closed.clone(),
        };
        if let Err(e) = fs.save(&root) {
            eprintln!("aox-gui: could not save the fold state: {e:#}");
        }
    }

    /// The id of the group holding the conversation being chatted in, if any.
    fn current_group_id(&self) -> Option<&str> {
        let cur = self.current_history.as_deref()?;
        self.hist_groups
            .iter()
            .find(|g| g.items.iter().any(|s| s.id == cur))
            .map(|g| g.id.as_str())
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
        // Same reasoning for the purpose: `purpose` is a short string and `command` is the first
        // argument of a shell call, so either is normally complete within a fragment or two. Taking
        // only whole values keeps a half-arrived command from being drawn as if it were the command.
        let purpose = match aox::llm::partial_json_string(
            &self.stream_calls[slot].args,
            aox::tool::PURPOSE_ARG,
        ) {
            Some(p) => Some(clip_head(&flat_line(&p), PURPOSE_MAX)),
            None if matches!(call_name.as_str(), "shell" | "bash") => {
                aox::llm::partial_json_string(&self.stream_calls[slot].args, "command")
                    .map(|c| clip_head(&flat_line(&c), PURPOSE_MAX))
            }
            None => None,
        };
        let fresh = self.stream_calls[slot].step == usize::MAX;
        let step = self.stream_calls[slot].step;
        let plugin = self.plugin_of(&call_name);
        let Some(l) = self.live.as_mut() else { return };
        let step = if fresh {
            l.steps.push(StepView {
                name: call_name.clone(),
                plugin,
                purpose: purpose.clone(),
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
            if purpose.is_some() {
                st.purpose = purpose;
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
        // A tool cannot be running while the model is writing, so this is also where the call's own
        // clock retires. Cleared unconditionally: leaving it behind would show a frozen "0:47 / 2:00"
        // next to a "Thinking" label.
        if let Some(l) = self.live.as_mut() {
            l.call = None;
        }
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
                let purpose = purpose_from_value(&name, &args);
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
                            if purpose.is_some() {
                                st.purpose = purpose.clone();
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
                            purpose,
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
            AgentEvent::ToolRunning { name, timeout_secs, expect_secs } => {
                self.status = aox::tf!("Running {}", name);
                let started = self.now();
                if let Some(l) = self.live.as_mut() {
                    l.call = Some(RunningCall { started, cap: timeout_secs, expect: expect_secs });
                }
                if live {
                    activity::lock(&self.act).tool_note(&aox::tf!("Timeout limit {}s", timeout_secs));
                }
            }
            AgentEvent::ToolResult { name, is_error, preview, diff, diff_detail, path } => {
                // A file write is also a row in the workspace view: that window answers "what did
                // this turn touch", which is the one thing this event knows and the transcript can
                // only say in prose. Recorded before the step is adopted, so the list and the
                // "+N / −N" on the transcript row appear on the same frame.
                if let Some(p) = path {
                    wsview::note_edit(&self.wsview, p, diff, diff_detail.clone());
                }
                if let Some(l) = self.live.as_mut() {
                    // The call is over: retire its clock before the row shows its result.
                    l.call = None;
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
            AgentEvent::Task(t) => {
                // Nothing is drawn here: the bar above the input reads it on every frame (see
                // `plan_bar`). Storing it is enough, and it must survive the end of the turn --
                // a plan outlives the work that wrote it.
                self.plan = PlanView::of(&t);
                self.task = Some(*t);
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
                // The tree is redrawn only when something asks for it, and a recycled conversation is
                // exactly such a thing: without this, a row right-clicked away would sit in the list
                // until the next turn ended. Done **here** rather than at the point the delete was
                // requested -- the move happens on the agent thread, so a refresh issued then would
                // race it and put the row straight back on screen.
                if self.show_history {
                    self.refresh_history();
                }
            }
            // ── Context compaction (`aox::compact`) ──
            // Two events, because the user sees two different things: while the summary call is in
            // flight the input box shows "compacting…" (the one moment the turn genuinely pauses),
            // and when it lands the gauge drops to the new figure instead of staying pinned at full
            // until the next request comes back.
            AgentEvent::ContextCompacting { used, budget } => {
                self.compacting = Some(std::time::Instant::now());
                self.note_act(
                    ActKind::Info,
                    aox::i18n::t("Context"),
                    &aox::tf!("compacting ~{} of {} tokens", used, budget),
                );
            }
            AgentEvent::ContextCompacted { before, after, folded, elided } => {
                self.compacting = None;
                self.ctx_reset = Some(after);
                self.note_act(
                    ActKind::Info,
                    aox::i18n::t("Context"),
                    &aox::tf!(
                        "compacted ~{} → ~{} tokens ({} messages folded, {} tool results stubbed)",
                        before,
                        after,
                        folded,
                        elided
                    ),
                );
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
        // An aborted turn can be aborted *during* a compaction; the hint must not outlive the turn.
        self.compacting = None;
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
    /// The task plan: a small **card** above the input, collapsed by default.
    ///
    /// Collapsed it answers the one question worth the space while work is running — "which step is
    /// it on, and how far in". One click opens the whole list, ticked and unticked, which is what a
    /// long task needs when the model's own account of progress is worth checking.
    ///
    /// (First version was a bare dim line that turned out to be unreadable against the page — the
    /// text was `text_dim` on no background at all. A card was the user's call, and it is the right
    /// one: a persistent, clickable reading is a panel, not a whisper.)
    /// Height of the **rows** part of the plan block, interpolated continuously from `plan_t`.
    ///
    /// Continuous, not `rows × t` rounded to whole rows: stepping one 20 px row at a time is what
    /// made the disclosure feel like a stutter (reported 2026-10-23) — the card, its shadow and the
    /// space reserved in the transcript all jumped together, several times per click. A fractional
    /// height is clipped instead, so the last row slides into view.
    ///
    /// The reserve and the drawing read this same number, in the same frame: two independent floats
    /// is exactly how the transcript's last line ends up behind the card for a frame.
    fn plan_body_height(&self) -> f32 {
        self.plan_rows_height() + self.plan_gap_height() + if self.plan.is_some() { PLAN_DIV_H } else { 0.0 }
    }

    /// Height of the **viewport** the rows scroll in.
    ///
    /// Exactly `rows × PLAN_ROW_H` — not a pixel more. A viewport that is 9 px taller than the rows
    /// it shows displays the top strip of the next row, which reads as a sliced line of text at the
    /// bottom of the list (2026-10-23). The breathing room beneath belongs to the card, not to the
    /// scrolled area, so it is counted separately (see `plan_gap_height`).
    fn plan_rows_height(&self) -> f32 {
        let Some(v) = self.plan.as_ref() else { return 0.0 };
        let rows = v.steps.len().min(PLAN_ROWS) as f32;
        rows * PLAN_ROW_H * self.plan_t
    }

    /// Breathing room under the last row; animated with the rows, and **outside** the scroll area.
    fn plan_gap_height(&self) -> f32 {
        if self.plan.is_none() || self.plan_t <= 0.0 {
            return 0.0;
        }
        PLAN_GAP * self.plan_t
    }

    /// The whole plan block's height for this frame.
    ///
    /// The header is there whenever a plan exists — only the rows animate. (Returning 0 at
    /// `plan_t == 0` was the first version's bug: the collapsed block reserved no space and the last
    /// line of the conversation sat underneath it.)
    fn plan_block_height(&self) -> f32 {
        if self.plan.is_none() {
            return 0.0;
        }
        PLAN_HEAD_H + self.plan_body_height()
    }

    fn plan_bar(&mut self, ui: &mut Ui) {
        let rows_h = self.plan_rows_height();
        let gap = self.plan_gap_height();
        // Scrolling only exists once the disclosure has finished: a scroll area whose viewport is
        // still growing shows a scrollbar for a fifth of a second and then takes it away.
        let full = self.plan_anim.is_none() && self.plan_t >= 1.0;
        let Some(view) = self.plan.as_ref() else { return };
        plan_card(ui, view, &mut self.plan_open, rows_h, gap, full);
    }

    fn choice_bar(&mut self, ui: &mut Ui) {
        // Clone rather than drawing through `self.choice`: the last row needs
        // `&mut self.choice_text`, which together with "read the question and options" is two
        // borrows of the same self -- the struct is tiny, so cloning is the least trouble.
        let Some(card) = self.choice.clone() else { return };
        let mut picked: Option<usize> = None;
        let mut typed: Option<String> = None;

        ui.add_space(6.0);
        // The question the model is waiting on: a **raised** card, unlike everything else in the
        // transcript. It is the one block that wants an answer, so it is allowed to sit above the
        // page instead of in it — that is what the shadow and the brighter edge are saying.
        theme::raised(theme::card_radius())
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

    /// The window's top row: four controls that **float on the content**, not on a bar.
    ///
    /// # Why the bar is ours
    ///
    /// Until 2026-10-18 the top of this window was two strips stacked on each other: the native
    /// caption (empty since 2026-10-04 — no title, only the system buttons, dyed to `P::bg()` so it
    /// read as background) and, inside the content area, a row of floating controls. Both were
    /// background-coloured, neither was quite "the window bar", and the independent windows
    /// (`activity`, `cache`, `wsview`) meanwhile drew a better shape: a bar that is *part of* the
    /// window rather than a strip above it.
    ///
    /// So the caption is gone (`with_decorations(false)` in `main.rs`) and the row is drawn here,
    /// to the same measurements as [`gwin::header`]: same 32x28 buttons, same hover language, same
    /// drag and double-click-to-maximize, same 1px outline ([`gwin::outline`]) and same DWM rounded
    /// corners (`theme::round_corners`). The one thing it does **not** carry is the pin: the main
    /// window is not something you keep on top of anything.
    ///
    /// # Why it is an `Area` and not a panel
    ///
    /// A `TopBottomPanel` reserves a full-width strip and paints it. On the plain background that is
    /// invisible; on the **first screen's background image** it cuts the picture off 52 points down,
    /// and the image then reads as "content under a bar" instead of as the window's own paper. An
    /// `Area` on the `Foreground` layer reserves nothing: the picture runs to the top edge and the
    /// four controls sit on small translucent plates ([`gwin::float_plate`]) that hold their own
    /// against any photograph.
    ///
    /// Nothing is covered by that: the transcript already starts 76 points down (see
    /// [`App::transcript`]), so the plates never sit on a line of text — which is also why the whole
    /// strip can stay the drag handle.
    ///
    /// # What is on it
    ///
    /// - Left: the drawer toggle, **only while the drawer is shut** — the drawer's head carries the
    ///   same control while it is open ([`sidebar_button`]: one drawing, two places).
    /// - Then a status pill (dot + text), **only while a turn runs**. It is a pill rather than bare
    ///   text because it now sits on a picture, and text alone would be unreadable on some of them.
    /// - Right: minimize / maximize / close, on one plate.
    ///
    /// No title **on screen**. The caption lost its "AOX" on 2026-10-04 — the user wanted the top
    /// edge to be the window's own edge, not a label — and putting the name back would reopen
    /// exactly that. The *system* title is "AOX" (see `main.rs`): the window is undecorated, so it
    /// is never painted, but it is what the taskbar and Alt-Tab call this window.
    ///
    /// "New conversation" and "settings" are still **not** here: both live inside the drawer, the
    /// only place they can be reached from once the drawer owns that corner.
    fn titlebar(&mut self, ctx: &egui::Context) {
        let mut toggle = false;
        let full = ctx.content_rect();
        // The row starts where the drawer ends — on the same eased clock the panel itself runs on,
        // so it travels with the column instead of jumping when the animation lands. While the
        // drawer is shut the offset is zero and the row spans the window.
        let bar_x = HISTORY_W * ease_out_cubic(self.hist_t);
        let strip = Rect::from_min_max(
            Pos2::new(full.left() + bar_x, full.top()),
            Pos2::new(full.right(), full.top() + TITLEBAR_H),
        );
        // Mid-animation the column can be a few points wide; a row that cannot hold two plates is
        // not worth drawing (and would paint its plates off the right edge).
        if strip.width() < 4.0 * PLATE {
            return;
        }

        egui::Area::new(egui::Id::new("aox-titlebar"))
            .order(egui::Order::Foreground)
            .movable(false)
            .fixed_pos(strip.min)
            .show(ctx, |ui| {
                // Everything this ui *touches* must stay inside the strip, input included. The
                // eight resize zones are computed from the window rect, and this layer wins the hit
                // test over the panels below it — unclipped, its two zones would claim the whole
                // left edge and the whole bottom edge as well.
                ui.set_clip_rect(strip);
                // The whole strip is the window's drag handle. The plates are declared after it,
                // and later-declared widgets win the hit test, so they still answer the pointer.
                let (rect, resp) = ui.allocate_exact_size(strip.size(), Sense::click_and_drag());
                // `StartDrag` has to go out on the **very frame** of the press, or the window lags
                // behind the pointer for the whole drag.
                if resp.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                let maxed = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                // Double-clicking a title bar maximizes — the system did that for us until the
                // caption went away. (It is also why this strip senses clicks, not just drags.)
                if resp.double_clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maxed));
                }

                // ── left: the drawer toggle, only while the drawer is shut ──
                if self.hist_t <= 0.0 {
                    let plate = Rect::from_center_size(
                        Pos2::new(rect.left() + TITLEBAR_INSET + PLATE * 0.5, rect.center().y),
                        Vec2::splat(PLATE),
                    );
                    gwin::float_plate(ui.painter(), plate, gwin::FLOAT_R, false);
                    // A child ui rather than `ui.scope_builder`: the strip has already been
                    // allocated in full, and a scope would allocate its own button a second time —
                    // below the strip, outside the Area.
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(plate.shrink(1.0)));
                    // `floating = false`: the plate is the background now, and the button's own
                    // hover fill (a step above the plate) is the whole hover language here.
                    if sidebar_button(&mut child, false, false).clicked() {
                        toggle = true;
                    }
                }

                // ── the running status: a readout, not a control ──
                if self.busy {
                    // Working time, same basis as every other readout (see [`Clock`]) -- otherwise
                    // the number here would keep climbing while the model sits waiting on the user.
                    let secs = self
                        .live
                        .as_ref()
                        .map_or(0.0, |l| (self.now() - l.turn_base).max(0.0));
                    let galley = ui.painter().layout_no_wrap(
                        format!("{} · {:.1}s", self.status, secs),
                        FontId::proportional(13.0),
                        P::text_dim(),
                    );
                    let pill = Rect::from_min_size(
                        Pos2::new(rect.left() + STATUS_X, rect.center().y - STATUS_H * 0.5),
                        Vec2::new(STATUS_PAD * 2.0 + STATUS_DOT_GAP + galley.size().x, STATUS_H),
                    );
                    gwin::float_plate(ui.painter(), pill, 9, false);
                    let pulse = ((self.spin * 2.0) as i32) % 2 == 0;
                    let dot = Pos2::new(pill.left() + STATUS_PAD + 4.0, pill.center().y);
                    ui.painter().circle_filled(
                        dot,
                        4.0,
                        if pulse { P::ink() } else { P::ink_soft() },
                    );
                    ui.painter().galley(
                        Pos2::new(dot.x + STATUS_DOT_GAP, pill.center().y - galley.size().y * 0.5),
                        galley,
                        P::text_dim(),
                    );
                }

                // ── right: minimize / maximize / close, one plate each ──
                //
                // Laid out right-to-left exactly as [`gwin::header`] does it, so the two windows
                // put their buttons in the same places. **Three plates, not one**: a single plate
                // holding all three reads as a toolbar of segmented buttons, which is a heavier
                // object than this row is -- each of these is one button, and at this size the
                // separate chips are also easier to hit.
                let plate_at = |cx: f32| {
                    Rect::from_center_size(Pos2::new(cx, rect.center().y), Vec2::splat(PLATE))
                };
                let mut cx = rect.right() - TITLEBAR_INSET - PLATE * 0.5;
                let close = plate_at(cx);
                gwin::float_plate(ui.painter(), close, gwin::FLOAT_R, false);
                if gwin::win_button(ui, close.shrink(1.0), "close", gwin::Glyph::Close) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                cx -= PLATE + WBTN_GAP;
                let max = plate_at(cx);
                gwin::float_plate(ui.painter(), max, gwin::FLOAT_R, false);
                if gwin::win_button(ui, max.shrink(1.0), "max", gwin::Glyph::Max { on: maxed }) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maxed));
                }
                cx -= PLATE + WBTN_GAP;
                let min = plate_at(cx);
                gwin::float_plate(ui.painter(), min, gwin::FLOAT_R, false);
                if gwin::win_button(ui, min.shrink(1.0), "min", gwin::Glyph::Min) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }

                // Resize hot-zones: the top edge and the two upper corners live in this ui's clip
                // rect. The drawer and the central column call this too, with the same full-window
                // rect — each `ui.interact` is clipped to its own ui, so the three calls together
                // cover all eight zones.
                gwin::resize_handles(ui, ctx, full);
            });

        if toggle {
            self.show_history = !self.show_history;
            if self.show_history {
                self.refresh_history();
            }
            // Nothing is written here: the drawer is not a persisted preference (see
            // `save_fold_state`), so opening it costs no disk write at all.
            //
            // The drawer has already been drawn by the time this row runs (it is declared first),
            // so the flip needs one more pass to show — and an idle window doesn't repaint on its
            // own. One request is enough: from the next pass on, the drawer's own animation keeps
            // the frames coming.
            ctx.request_repaint();
        }
    }

    /// The token count in the input box's bottom-right corner: **click** to unfold the breakdown
    /// above it.
    ///
    /// Click, not hover, and that is a direct consequence of the move: the chip now sits in the
    /// corner the pointer crosses on its way to the send button, so a hover-opened panel would
    /// flash open every time someone sent a message. The panel stays up until the chip is clicked
    /// again, or until something else is clicked (see the dismiss test below).
    ///
    /// The number is **live**: completed turns are exact, the running turn is the stream's own
    /// estimate, so the total climbs during a turn instead of sitting still until it ends. The
    /// `~` marks the estimate half.
    fn usage_chip(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let base = self.usage_sum().total.unwrap_or(0);
        let est = self.live.as_ref().map_or(0, |l| l.out.tokens());
        let shown = base + est;
        // Nothing has been spent and nothing is being spent: no chip at all, rather than a
        // "Σ 0 tok" that would read as a measurement.
        if shown == 0 && !self.busy {
            return;
        }
        let text = if est > 0 {
            format!("Σ ~{} tok", thousands(shown))
        } else {
            format!("Σ {} tok", thousands(shown))
        };
        // Drawn by hand rather than as a `Label`: the hover plate has to go **under** the text, and
        // a widget's own paint has already happened by the time its response says "hovered". The
        // same reason the drawer's icons allocate a rect and then paint into it.
        //
        // The width follows the number, which is why the ring is added *before* this in the
        // right-to-left row: the chip grows to the left and the ring keeps its place in the corner.
        let galley = ui
            .painter()
            .layout_no_wrap(text.clone(), FontId::proportional(13.0), P::text_dim());
        let (rect, resp) =
            ui.allocate_exact_size(galley.size() + egui::vec2(12.0, 6.0), Sense::click());
        let hov = pointer_if(ui, &resp);
        let lit = hov || self.usage_open;
        if lit {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(7), P::hover_fill());
        }
        let fg = if lit { P::ink() } else { P::text_dim() };
        let galley = ui
            .painter()
            .layout_no_wrap(text, FontId::proportional(13.0), fg);
        ui.painter()
            .galley(rect.min + egui::vec2(6.0, 3.0), galley, fg);
        if resp.clicked() {
            self.usage_open = !self.usage_open;
        }
        if !self.usage_open {
            return;
        }
        // The panel unfolds **upwards**: the chip is in the window's bottom-right corner, so
        // "below it" is off-screen. `pivot(LEFT_BOTTOM)` puts the panel's own bottom-left corner
        // on the anchor, which is what makes it grow up instead of down.
        //
        // ⚠️ **Do not convert coordinates here**: `Area::fixed_pos` and `ui.allocate_*` are both in
        // **client-area local coordinates** (`Area::begin` uses `ctx.content_rect()` as the
        // constraint). This once had a "viewport origin" term, `viewport().inner_rect.left_top()`,
        // added -- that is **screen (monitor) coordinates**, a completely different system, and the
        // conversion is meaningless; the result was the panel shifting wholesale with the
        // window's position on the monitor (measured ~(154, 14) logical pixels off when the
        // window wasn't at (0,0)). The same mistake was made in `workplace_ui::menu` and has been
        // fixed there too.
        let anchor = resp.rect.left_top() - egui::vec2(0.0, 6.0);
        let log = &self.usage_log;
        let panel_rect = egui::Area::new(egui::Id::new("aox-usage-panel"))
            .fixed_pos(anchor)
            .pivot(Align2::LEFT_BOTTOM)
            .order(egui::Order::Foreground)
            .show(&ctx, |ui| {
                Frame::popup(ui.style())
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(12))
                    .stroke(Stroke::new(1.0, P::edge_raised()))
                    .inner_margin(Margin::symmetric(16, 13))
                    .show(ui, |ui| usage_detail(ui, log))
                    .response
                    .rect
            })
            .inner;
        // Dismissed by a click anywhere else. Tested after the panel has been drawn, so the click
        // that opens it (which lands on the chip) can't immediately close it again.
        let inside = resp.hovered() || panel_rect.contains(ui.ctx().pointer_hover_pos().unwrap_or_default());
        if ctx.input(|i| i.pointer.any_click()) && !inside {
            self.usage_open = false;
        }
    }

    /// The **totals** across this conversation's turns (summed field by field).
    ///
    /// Both the corner chip and the expanded panel use it -- one number says "how much was
    /// spent", the whole set says "where it went".
    fn usage_sum(&self) -> Usage {
        sum_usage(&self.usage_log)
    }

    /// How much of the model's context window this conversation currently occupies -- the ring's
    /// numerator.
    ///
    /// `last_prompt` on the newest record is exactly that number: it is the size of the **last
    /// request**, i.e. everything the model was handed (system prompt + the whole history + the new
    /// message). The running turn's output is added on top, because it is about to become part of
    /// the next request -- without it the ring would sit perfectly still while a long reply is
    /// being written, which is the one moment someone is likely to be watching it.
    fn context_used(&self) -> u64 {
        let live = self.live.as_ref().map_or(0, |l| l.out.tokens());
        // Right after a compaction the true figure is only known locally (the compacted request's
        // size); the endpoint's own number arrives with the next call. Showing the stale "full"
        // reading in between is exactly the moment the user is watching, so the reset wins until a
        // real number lands (see `ctx_reset`).
        if let Some(after) = self.ctx_reset {
            return after + live;
        }
        let base = self
            .usage_log
            .iter()
            .rev()
            .find_map(|r| r.last_prompt)
            .unwrap_or(0);
        base + live
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
        // What a **relative** path in a reply is relative to, for the two leaf consumers that are
        // several layers of painting code away from here (`markdown::scan`'s inline-code paths and
        // the file name on a tool-call row). The workspace root, not the app root: that is the
        // directory the agent actually writes into, and the one the user thinks in.
        crate::open::set_workspace_root(
            &self.ws.view().and_then(|v| v.root.clone()).unwrap_or_else(aox::paths::app_root),
        );
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
                        ui.add_space(COMPOSER_RESERVE + self.plan_block_height());
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
                    .stroke(Stroke::new(1.0, P::edge_raised()))
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
            // The **call's** clock while a call is running, the turn's otherwise. A running call is
            // the one case where "how long has this been going" has an answer the user can act on,
            // and it needs its ceiling right next to it: `0:47 / 2:00` says "fine", `1:58 / 2:00`
            // says "about to be cut off".
            match &l.call {
                Some(c) => {
                    ui.label(
                        RichText::new(span(now - c.started, c.cap as f32))
                            .color(P::text_faint())
                            .size(13.0),
                    );
                    if let Some(e) = c.expect {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(aox::tf!("expected ~{}", clock(e as f32)))
                                .color(P::text_faint())
                                .size(13.0),
                        );
                    }
                }
                None => {
                    ui.label(
                        RichText::new(format!("{:.1}s", (now - l.turn_base).max(0.0)))
                            .color(P::text_faint())
                            .size(13.0),
                    );
                }
            }
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

    /// Write a folder the user has just chosen into the registry, so it **survives a restart**.
    ///
    /// This is the missing half of the directory picker. `ToAgent::Workplace` only tells the agent
    /// thread where the next conversation may write; it says nothing to the registry, and the
    /// registry is what `workspace_roots` — and therefore the store's scan — is built from. So a
    /// folder picked from the list switched the writes correctly and then **vanished on the next
    /// launch**: the group was drawn from an `auto:` fallback for as long as a conversation lived
    /// there, and the conversations themselves were never listed again because their directory was
    /// never scanned.
    ///
    /// Called from [`App::send_ws`] rather than from the picker so that every door onto "switch to
    /// this folder" — the tier menu, the full-access confirmation, the picker — passes through it.
    /// The app root and folders already in the registry are refused there (see
    /// [`aox::space::Registry::adopt_workspace`]), which is what keeps a repeat click from
    /// rewriting the file and keeps `Default`'s conversations in `Default`.
    fn adopt_workspace(&mut self, root: &Path) {
        let Some(app_root) = self.app_root() else {
            return;
        };
        let mut reg = aox::space::Registry::load(&app_root);
        let Some(id) = reg.adopt_workspace(root, &app_root) else {
            return;
        };
        if let Err(e) = reg.save(&app_root) {
            // Same policy as every other registry write: logged, not surfaced. The conversation
            // still goes where the user asked; what is lost is only "the folder is still there next
            // launch", and a modal for that would interrupt the thing they were doing.
            eprintln!("aox-gui: could not save the space registry: {e:#}");
            return;
        }
        // The tree is rebuilt from the registry, so the group is on screen from this frame on --
        // and unfolded, exactly as `create_workspace` leaves the group it makes. An empty new space
        // is not drawn yet (`group()` drops a group with no conversations unless it is built-in), so
        // this mostly matters for the frame the first conversation lands in it.
        self.refresh_history();
        self.hist_closed.remove(&id);
        self.hist_open.insert(id);
        self.save_fold_state();
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
        // A folder is a workspace only if the registry says so (see `adopt_workspace`); the agent
        // message below moves the write boundary, and this is the other half of the same click.
        if let Mode::Fixed(root) = &mode {
            self.adopt_workspace(root);
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
                            P::edge_raised()
                        } else {
                            P::edge()
                        },
                    ))
                    .shadow(theme::card_shadow())
                    .inner_margin(Margin::symmetric(22, 15))
                    .show(ui, |ui| {
                        let w = ui.available_width();
                        ui.set_width(w);
                        ui.set_max_width(w);
                        // The plan lives **inside** the input card, above a hairline divider: as a
                        // card of its own it left a seam against the box, and a card floating over
                        // the transcript hid the last line of the conversation behind it (both
                        // reported 2026-10-23).
                        self.plan_bar(ui);
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
                                .hint_text(RichText::new(hint).color(P::quiet()).size(16.0));
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

                        // Second row: the workplace button at the bottom-left, the usage readouts
                        // at the bottom-right.
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
                        //
                        // The right-hand pair is the "what has this cost" corner: the token total
                        // (click → the breakdown unfolds upwards) and the context ring next to it.
                        // They moved here from the top bar because the top bar is where you look
                        // when you want to **act**, and this is where the hand already is when you
                        // want to **know**.
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            workplace_ui::button(ui, &mut self.ws, self.demo || !blocked);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                // While the host is folding old turns into a summary, the gauge has no
                                // honest reading (the numbers only come back a round trip later), so
                                // the ring becomes the spinner and a line of text says why. That is
                                // the **only** moment the input box stops looking idle while a turn
                                // is still running.
                                let spin = self.compacting.map(|t| t.elapsed().as_secs_f32());
                                context_ring(ui, self.context_used(), self.context_window, spin);
                                if spin.is_some() {
                                    ui.add_space(6.0);
                                    ui.label(
                                        RichText::new(aox::i18n::t("Compacting context…"))
                                            .size(11.0)
                                            .color(P::text_dim()),
                                    );
                                    // Nothing else in this row animates, so the repaint has to be
                                    // asked for by hand; without it the arc would jump only when the
                                    // pointer moves.
                                    ui.ctx().request_repaint();
                                }
                                ui.add_space(10.0);
                                self.usage_chip(ui);
                            });
                        });
                    });
            });
        });
    }

    /// The name dialog ("new workspace" from the panel's foot row, "rename workspace" from a
    /// group's right-click menu, or "rename conversation" from a conversation row's).
    ///
    /// The state is `take()`n out of `self.name_dialog` and put back at the end, rather than borrowed
    /// across the draw: the body needs `&mut self` (the app root, and on confirm the registry), and a
    /// `&mut` into `App::name_dialog` held over that is exactly the double borrow the compiler refuses.
    /// Taking it keeps one owner at a time without cloning the whole struct every frame.
    fn name_dialog_overlay(&mut self, ctx: &egui::Context) {
        let Some(mut st) = self.name_dialog.take() else {
            return;
        };
        let root = self.app_root();
        let action = name_dialog_modal(ctx, &mut st, root.as_deref());
        match action {
            // Still open: put the state back, so the next frame draws the same text and keeps the
            // caret where the user left it. (Anything else would make the field feel like it resets
            // itself every frame.)
            None => self.name_dialog = Some(st),
            // Cancelled -- backdrop click, Esc, or the button. Nothing was written, so there is
            // nothing to clean up.
            Some(DialogAction::Cancel) => {}
            Some(DialogAction::Confirm) => {
                let res = match &st.mode {
                    NameDialogMode::New => self.create_workspace(&st.name, root.as_deref()),
                    // Blank is not refused here, it is the undo: the dialog says the folder's own name
                    // is what an empty field means, so it has to be what an empty field does.
                    NameDialogMode::Rename { root: ws } => {
                        self.rename_workspace(ws, Some(st.name.as_str()))
                    }
                    // Same shape as the workspace rename: whatever is in the field is the answer,
                    // because a **blank** field is the real answer here too ("go back to the first
                    // message"), and the caller is the one that reads the blank.
                    NameDialogMode::RenameConversation { id } => {
                        self.rename_conversation(id, Some(st.name.as_str()))
                    }
                };
                // Refused: the dialog **stays open** with the message, so the name can be fixed
                // without typing it all again. On success it closes and the field stays `None`.
                if let Err(e) = res {
                    st.error = Some(e);
                    self.name_dialog = Some(st);
                }
            }
        }
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
            .backdrop_color(P::scrim())
            .frame(
                theme::raised(theme::card_radius())
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

                    // No vertical rule between the two columns.
                    //
                    // The rule was there to say "picking on the left, reading on the right". The
                    // two columns are already told apart by their size, their colour and the
                    // selected row's marker; a line on top of that is one more line to look at, and
                    // the card reads calmer without it. The gap does the same job.
                    ui.add_space(SETTINGS_W - SETTINGS_NAV_W - SETTINGS_RIGHT_W);

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
        //
        // No caption line under either button. "Open the activity stream" already says what it
        // opens; a grey sentence underneath only repeats it in smaller type and adds a second
        // thing to read on a screen that holds two buttons.
        let on = activity::lock(&self.act).is_open();
        if activity_open_button(ui, on).clicked() {
            activity::open(ui.ctx(), &self.act);
        }

        ui.add_space(2.0);
        let on = cache::lock(&self.cache).is_open();
        if cache_open_button(ui, on).clicked() {
            cache::open(ui.ctx(), &self.cache);
        }

        ui.add_space(2.0);
        let on = wsview::is_open(&self.wsview);
        if wsview_open_button(ui, on).clicked() {
            wsview::open(ui.ctx(), &self.wsview);
        }
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

        // Every row is **collected** before anything is drawn: the name column has to be as wide as
        // the widest name in it (see `kv_key_w`), and that cannot be known while drawing row by row.
        let mut rows: Vec<(&str, String, Color32)> = Vec::new();

        rows.push((aox::i18n::t("System info"), p.host.clone(), P::text_faint()));

        // Which command-line tool the model reaches for first. Above the language runtimes on purpose:
        // those are for *plugins*, this is for the model's own hands. The value names the flavour
        // (`git-bash` vs `busybox`), because that is the difference between a full bash and an ash —
        // and the exact executable, because "which one did it pick" is the next question.
        if !p.command_line.is_empty() {
            rows.push((aox::i18n::t("Command line"), p.command_line.clone(), P::text_faint()));
        }

        // Runtime environment: **bundled or system PATH** -- one row per language, with the
        // source written straight into brackets. Only the first row carries the name; the rest are
        // continuations and simply get the same (measured) column.
        // No ✓/✗ drawn: those two glyphs aren't guaranteed to exist in the hand-picked font (the
        // two in the call stream are hand-drawn), and the word "not found" already states the
        // conclusion clearly.
        for (i, it) in self.runtimes.items.iter().enumerate() {
            rows.push((
                if i == 0 { aox::i18n::t("Runtime environment") } else { "" },
                format!("{}  {}  [{}]", it.lang, it.command, it.origin),
                if it.ok { P::text_faint() } else { P::warn() },
            ));
        }

        if !self.model.is_empty() {
            rows.push((aox::i18n::t("Model in use"), self.model.clone(), P::text_faint()));
        }
        if !p.config.is_empty() {
            rows.push((aox::i18n::t("Config file"), p.config.clone(), P::text_faint()));
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
            rows.push((aox::i18n::t("Config layers"), layers.join(" → "), P::text_faint()));
        }
        if !p.root.is_empty() {
            rows.push((aox::i18n::t("App root"), p.root.clone(), P::text_faint()));
        }
        rows.push((aox::i18n::t("Workspace"), self.workplace_line(), P::text_faint()));
        if !p.history.is_empty() {
            rows.push((aox::i18n::t("History"), p.history.clone(), P::text_faint()));
        }
        if !p.plugins.is_empty() {
            rows.push((aox::i18n::t("Plugins directory"), p.plugins.clone(), P::text_faint()));
        }
        if !p.runtime.is_empty() {
            // "if it's bundled" -- whether the directory is there tells you at a glance whether
            // the bundled runtime is in use
            let note = if std::path::Path::new(&p.runtime).is_dir() {
                ""
            } else {
                aox::i18n::t(" (built-in runtime not installed)")
            };
            rows.push((
                aox::i18n::t("Runtime directory"),
                format!("{}{note}", p.runtime),
                P::text_faint(),
            ));
        }

        let keys: Vec<&str> = rows.iter().map(|r| r.0).collect();
        let key_w = kv_key_w(ui, &keys);
        for (k, v, c) in &rows {
            kv_row(ui, key_w, k, v, *c);
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
        ui.add_space(12.0);
        // No rule under the masthead, and no "Full ISC license text" label above the box: the
        // masthead is already the only large type on the column, and the box below it is plainly
        // the licence. A label saying "here is the licence" is the definition of a line of text
        // that earns nothing.
        ui.label(
            RichText::new(aox::i18n::t("AOX GUI, written in Rust. Licensed under the open-source ISC license."))
                .color(P::text_dim())
                .size(14.0),
        );

        ui.add_space(14.0);
        // The license text sits in a filled box. **Selectable**: when quoting a clause you can
        // copy it directly instead of retyping it off the screen.
        Frame::NONE
            .fill(P::code_bg())
            .corner_radius(CornerRadius::same(12))
            .stroke(Stroke::new(1.0, P::edge()))
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
        // The two links are one column, so they share one measured width — "Author's blog" is the
        // wider of the two and would otherwise print over its own URL.
        let link_keys = [aox::i18n::t("Author's blog"), aox::i18n::t("Project home")];
        let link_key_w = kv_key_w(ui, &link_keys);
        link_row(ui, link_key_w, link_keys[0], "https://2am.top");
        link_row(ui, link_key_w, link_keys[1], "https://github.com/forever218/AOX");
    }

    fn auth_overlay(&mut self, ctx: &egui::Context) {
        let Some(card) = self.auth.take() else { return };
        let mut decision: Option<AuthDecision> = None;
        egui::Modal::new(egui::Id::new("aox-auth"))
            .backdrop_color(P::scrim())
            .show(ctx, |ui| {
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
            theme::well(8)
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
/// Hand-drawn like the other glyphs in this file (`sidebar_button` / `gear_button`): the
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
/// How many rows fit in the card before the list starts **scrolling** inside it.
///
/// The card sits above the input box, so an unbounded list would walk the composer off a short
/// window — and a 30-step plan is legal (see `task::STEPS_MAX`). Scrolling lives inside the card
/// rather than in the "show the first 8 and a `+k`" version this replaces: a plan you cannot read
/// past row 8 is not a plan you can check your work against.
const PLAN_ROWS: usize = 8;

/// Draw the plan card.
///
/// A free function rather than a method on purpose: the caller lends `&self.plan` and
/// `&mut self.plan_open` at the same time, and two disjoint fields are fine while a method call on
/// `&mut self` would be rejected.
///
/// Markers carry the state, colour carries the attention: `✓` done (dim), `▸` the step being worked
/// on (full text colour), `·` still to come (dim). Only one row is bright, which is the whole point
/// of a progress list — the rest is context.
fn plan_card(ui: &mut Ui, view: &PlanView, open: &mut bool, rows_h: f32, gap: f32, full: bool) {
    let w = ui.available_width();
    ui.set_width(w);

    // ── Header: always visible, one line ──
    let header = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            // "3/5" first — it is the number the eye looks for, and on its own it already justifies
            // the space the collapsed block takes.
            ui.label(
                RichText::new(format!("{}/{}", view.done, view.total))
                    .color(P::text_dim())
                    .size(12.0),
            );
            match &view.current {
                Some(cur) => {
                    ui.add(Label::new(RichText::new(cur).color(P::text()).size(12.5)).truncate());
                }
                None => {
                    ui.label(
                        RichText::new(aox::i18n::t("every step is ticked"))
                            .color(P::text_dim())
                            .size(12.0),
                    );
                }
            }
            // Painted, never typed: `▸` / `▾` have no glyph in the bundled font, so both came out as
            // tofu boxes (2026-10-23). Three lines cannot go missing.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                chevron(ui, *open, P::text_dim());
            });
        })
        .response;
    let hit = ui.interact(header.rect, ui.id().with("aox-plan-toggle"), Sense::click());
    if hit.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if hit.clicked() {
        *open = !*open;
    }

    // ── The rule, then the rows sliding out from under it ──
    //
    // The rule's band is part of the card from the start (only its ink fades in), and the rows are
    // clipped **below** it. An earlier version put the rule at the bottom of the animated block, so
    // while opening it travelled down across the half-revealed row — a line through the text, read
    // as "overlapping" (2026-10-23).
    let block_h = PLAN_DIV_H + rows_h;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, block_h), Sense::hover());
    let line_y = rect.top() + 4.0;
    ui.painter().hline(
        rect.x_range(),
        line_y,
        Stroke::new(1.0, P::edge().gamma_multiply(plan_rule_alpha(block_h))),
    );

    let rows_rect = Rect::from_min_max(
        Pos2::new(rect.left(), line_y + 1.0 + 4.0),
        Pos2::new(rect.right(), rect.bottom()),
    );
    if rows_rect.height() <= 0.5 {
        ui.add_space(gap);
        return;
    }
    let mut area = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rows_rect)
            .layout(Layout::top_down(Align::Min)),
    );
    area.set_clip_rect(rows_rect.intersect(ui.clip_rect()));
    if full {
        // A plan longer than the visible window scrolls **inside the card** (user request,
        // 2026-10-23): the alternative — showing the first eight and a "+k" — means the tail of the
        // plan is unreachable, and the tail is exactly where "what is left" lives.
        egui::ScrollArea::vertical()
            .id_salt("aox-plan-rows")
            .max_height(rows_rect.height())
            .auto_shrink([false, true])
            .show(&mut area, |ui| plan_rows_into(ui, view));
    } else {
        plan_rows_into(&mut area, view);
    }
    ui.add_space(gap);
}

/// The rows themselves.
///
/// Shared by the plain path and the scrolling path so the two can never drift apart — and each row
/// is allocated at **exactly** `PLAN_ROW_H`: the block's height is computed from that constant
/// before a single row is drawn (the composer's position depends on it), so a row that lays itself
/// out naturally would make the arithmetic a guess. It was a guess once: five rows reported a
/// height of three.
fn plan_rows_into(ui: &mut Ui, view: &PlanView) {
    let w = ui.available_width();
    // **No inter-row spacing**: `PLAN_ROW_H` is the row's full pitch, and the block's height was
    // computed from it before anything was drawn. Leaving egui's default 4 px between rows added
    // 20 px to five rows, which pushed the first row out of the viewport and produced a scrollbar
    // for a plan that fits (2026-10-23).
    ui.spacing_mut().item_spacing.y = 0.0;
    for (text, done) in view.steps.iter() {
        let current = !*done && view.current.as_deref() == Some(text.as_str());
        // Done vs to come is told by the **mark**, not by the text colour: one row is always the
        // working one, and dimming the rest of them would bury the plan.
        let (color, size) = if current {
            (P::text(), 12.5)
        } else if *done {
            (P::text_dim(), 12.0)
        } else {
            (P::text_dim(), 12.0)
        };
        ui.allocate_ui_with_layout(
            Vec2::new(w, PLAN_ROW_H),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                step_mark(ui, *done);
                ui.add(Label::new(RichText::new(text).color(color).size(size)).truncate());
            },
        );
    }
}

/// One step's marker: a hairline box while it is still to come, a low-saturation green tick once it
/// is done.
///
/// Painted rather than typed, for the same reason as the chevron: the bundled fonts have no `✓`
/// glyph and it came out as an empty rectangle. Painting also lets the box and the tick share one
/// optical size — no two font glyphs ever do, and at 10 px that shows.
///
/// The green is `P::add_green()` (the diff's "lines added" colour), deliberately low-saturation: a
/// plan is a quiet list, and a saturated tick would out-shout the one row the user is looking at.
fn step_mark(ui: &mut Ui, done: bool) -> Rect {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
    let c = rect.center();
    let painter = ui.painter();
    if done {
        // Two strokes, corner below-left of centre: that is what makes it read as a tick instead of
        // a "v". 1.6 px at this size matches the visual weight of the box outline.
        let s = Stroke::new(1.6, P::add_green());
        painter.line_segment([c + egui::vec2(-3.7, 0.2), c + egui::vec2(-1.2, 2.7)], s);
        painter.line_segment([c + egui::vec2(-1.2, 2.7), c + egui::vec2(3.9, -2.9)], s);
    } else {
        painter.rect_stroke(
            Rect::from_center_size(c, Vec2::splat(8.4)),
            CornerRadius::same(2),
            Stroke::new(1.0, P::text_dim()),
            egui::StrokeKind::Inside,
        );
    }
    rect
}

/// How visible the rule is, given how much of the block is revealed.
///
/// Only a fade — the geometry never depends on it, so a frame in the middle of the disclosure cannot
/// produce a line in the wrong place. It reaches full strength once the rows have started arriving.
fn plan_rule_alpha(body_h: f32) -> f32 {
    ((body_h - 6.0) / 6.0).clamp(0.0, 1.0)
}

/// A small disclosure triangle, painted instead of typed (see `plan_card`).
fn chevron(ui: &mut Ui, open: bool, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
    let c = rect.center();
    let pts = if open {
        vec![c + egui::vec2(-3.5, -1.5), c + egui::vec2(3.5, -1.5), c + egui::vec2(0.0, 2.5)]
    } else {
        vec![c + egui::vec2(-1.5, -3.5), c + egui::vec2(-1.5, 3.5), c + egui::vec2(2.5, 0.0)]
    };
    ui.painter().add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

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
    // The same wiring a real boot does: `--demo` should show the conversations that are actually on
    // this machine, including the ones made in other workspaces. Without this the preview would look
    // poorer than the real thing for no reason.
    let spaces = aox::space::Registry::load(&root);
    store.set_workspaces(&spaces.workspace_roots(&root));
    app.boot = Some(BootInfo {
        config_path: app.config_path.clone(),
        // The demo data gets a realistic layer chain too, so that row in the "About" window is
        // visible under --demo.
        config_layers: vec!["bundle:core".into(), "config.toml".into()],
        model: app.model.clone(),
        context_window: DEFAULT_CONTEXT_WINDOW,
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
            // `--demo` never loads the config, so the shell row uses the defaults (`[shell] bash =
            // "auto"`). Detection itself only stats files, so this is a real answer for this machine,
            // not a placeholder.
            &aox::config::ShellConfig::default(),
        ),
        store: store.clone(),
    });
    app.store = Some(store);
    app.refresh_history();
    // A plan for the bar above the input: `--demo` is how the interface is verified without a
    // model, and a bar nobody can see in the demo is a bar nobody checks.
    let demo_task = aox::task::TaskState::steps_demo();
    // Both: the bar draws from `plan`, and `task` is what `/task`-style reporting would read.
    app.plan = PlanView::of(&demo_task);
    app.task = Some(demo_task);

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
                purpose: Some(aox::i18n::t("see what the host already has").into()),
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
                purpose: Some(aox::i18n::t("make the filestat plugin").into()),
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
                purpose: Some(aox::i18n::t("write the plugin").into()),
                // A file that is **really there**: this row is the demo of "click a tool call's file
                // name and it opens in the file manager", and a path that resolves to nothing would
                // demonstrate the opposite (`plugins/filestat/main.py` only exists if you built the
                // demo plugin). See `open::resolve_relative`.
                subject: Some("plugins/sdk/plugin_sdk.py".into()),
                args: r#"{"path":"plugins/sdk/plugin_sdk.py"}"#.into(),
                result: Some((false, aox::i18n::t("Overwrote plugins/sdk/plugin_sdk.py (2.4 KB)").into())),
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
                purpose: Some(aox::i18n::t("count the files here").into()),
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
    //
    // The bare path is **this binary's own path**, not a made-up one: the whole point of that line is
    // that clicking it opens a real Explorer window, and a placeholder that resolves to nothing would
    // only ever show the "path not found" tooltip — which is the opposite of a demo.
    let self_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "E:\\rustapp\\dist\\aox-gui.exe".into());
    app.items.push(Item::Assistant(
        "## Over-wide content\n\n```json\n{\"very_long_key_that_never_wraps\":\"C:/Users/someone/AppData/Local/Temp/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json\",\"n\":1}\n```\n\nThe path `E:/rustapp/plugins/filestat/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.py` is very long.\n\nA bare path in prose, and a URL: {SELF} · https://github.com/forever218/AOX — both are clickable (click the path: Explorer opens with the file selected; click the URL: it opens in your browser).\n\n### Connectivity test\n\n- `httpfetch_raw` → requested bilibili.com, got HTTP 200 and fetched the page text (183539 chars, truncated); network and plugin-process communication are fine.\n\n- `bilibili_video_info` → fetched BV1GJ411x7h7 successfully with full metadata (title \"Never Gonna Give You Up (Official MV)\", publisher \"Sony Music China\", 106M plays).\n\n| Plugin | Language | Tools |\n| --- | --- | --- |\n| bilibili | python | bilibili_video_info (video metadata), bilibili_danmaku (bullet comments), bilibili_comments (comments), bilibili_subtitle (subtitles), bilibili_related (related videos) |\n| httpfetch | python | httpfetch_raw (generic GET for API debugging / web page fetching) |\n\nConclusion: no new or fixed plugins needed -- existing capabilities already cover Bilibili video info fetching + generic web requests, ready to take the job. File I/O, command execution, data export (CSV/Excel) and similar can be added later as needed.\n\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .replace("{SELF}", &self_path),
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

    // The cache-hit map gets the same treatment: `--demo` must show it too, because "what it looks
    // like while it is working" is most of what a preview is for, and the window is worthless to
    // judge from an empty grid.
    //
    // There is one clock now, so the demo cannot pre-fill an array — it has to **run the front**, and
    // the front only walks while a turn is in flight. So the demo declares itself "in flight" for the
    // length of this block: it is standing in for exactly the one situation that ever fills this map.
    //
    // Two hundred-odd readings, a shade over one lap, so the preview opens on a **full** square
    // instead of one that stops a third of the way round. Each reading holds for one or two tiles, so
    // the map has bands rather than a single dot in the corner.
    //
    // Deterministic rather than random (a fixed pattern, not a generator), so two launches of
    // `--demo` show the same picture and a screenshot can be compared against the previous one.
    //
    // The last tile is written by the call below, so the frame lands on the current reading, and the
    // final numbers are the ones a live window would show: a 31K context of which 24.4K came back
    // from the cache, with the four sections adding up to exactly that context size.
    //
    // Fed through the **real** sequence rather than the shortcuts: sections, then the per-call
    // figures, then the turn's ledger — the same three messages a live turn sends, in the same
    // order. A preview that skipped a step would stop being evidence that the live path works.
    {
        use aox::llm::Usage;
        let mut cv = cache::lock(&app.cache);
        cv.gw.force_open();
        cv.gw.busy = true;
        for k in 0..200u32 {
            let prompt = 6_000 + u64::from(k) * 140;
            // The mix is a *warm* conversation, which is what a long one looks like: most calls land
            // at 93% cached — a hit band with a thin brick strip, the case where the old picture
            // showed nothing at all — a few at 55% (a message appended that the cached prefix could
            // not absorb), and the odd 2% cold start.
            let share = match k % 12 {
                3 | 8 => 0.55,
                5 => 0.02,
                _ => 0.93,
            };
            let cached = (prompt as f64 * share) as u64;
            cv.observe_call(
                &Usage { prompt: Some(prompt), cached: Some(cached), ..Default::default() },
                None,
            );
            // 0.1 s is the largest step `advance` accepts from one frame, and at 10.2 tiles a
            // second it is one tile — so this is "the reading held for 98 ms".
            cv.advance(0.1);
            // Every fifth reading held twice as long: without a few wider bands the strip reads as
            // noise instead of as something that stayed put.
            if k % 5 == 0 {
                cv.advance(0.1);
            }
        }
        cv.observe_context(vec![
            cache::Seg { label: "System prompt", tokens: 5_200 },
            cache::Seg { label: "Tool definitions", tokens: 4_100 },
            cache::Seg { label: "Conversation history", tokens: 20_100 },
            cache::Seg { label: "Your input", tokens: 1_800 },
        ]);
        cv.observe_call(
            &Usage {
                prompt: Some(31_200),
                completion: Some(940),
                total: Some(32_140),
                reasoning: Some(310),
                cached: Some(24_400),
                cache_miss: Some(6_800),
            },
            Some(31_200),
        );
        cv.observe_turn(&UsageRecord {
            round: 3,
            at: 0,
            calls: 2,
            usage: Usage {
                prompt: Some(31_200),
                completion: Some(940),
                total: Some(32_140),
                reasoning: Some(310),
                cached: Some(24_400),
                cache_miss: Some(6_800),
            },
            last_prompt: Some(31_200),
            session_total: Some(74_600),
            session_calls: 7,
        });
        // One step for the live reading, so the frame lands on **it**: the seeded loop ends on a 93%
        // call, and a preview whose newest tile is a hit would show neither the 22%-missed strip nor
        // the partial band the last call really produced.
        cv.advance(0.1);
        // The launch lands with the square all but full and the frame on the live reading. It then
        // **stops**, because nothing is in flight in demo mode — the same rule a real window follows
        // when the turn ends.
        cv.gw.busy = false;
    }

    // The workspace view is seeded too: a file browser previewed on an empty directory would prove
    // nothing about the layout. The root is the **current directory** rather than the app root,
    // because in demo mode the app root is `target/release` — which contains no source to browse —
    // while the current directory is where `--demo` is normally launched from.
    //
    // The two edits are illustrative: the counts and the diff lines are written out here rather
    // than produced by a turn, because in demo mode no turn ever ran. The **paths are real**, so the
    // "File" half of the pane shows actual code from this repository.
    {
        let root = std::env::current_dir().unwrap_or_default();
        wsview::force_open(&app.wsview);
        wsview::set_root(&app.wsview, Some(root.clone()));
        wsview::note_edit(
            &app.wsview,
            root.join("src").join("tool").join("hostfs.rs"),
            Some((4, 2)),
            Some(
                "⋯\n     let existed = path.exists();\n-    let previous = if existed { std::fs::read_to_string(&path).ok() } else { None };\n+    // Read the previous version **before** overwriting it — this is the only moment both\n+    // versions are in hand.\n+    let previous = if existed { std::fs::read_to_string(&path).ok() } else { Some(String::new()) };\n     match std::fs::write(&path, content.as_bytes()) {\n⋯"
                    .to_string(),
            ),
        );
        wsview::note_edit(
            &app.wsview,
            root.join("gui").join("src").join("wsview.rs"),
            Some((6, 0)),
            Some(
                "⋯\n     fn rebuild(&mut self) {\n         self.rows.clear();\n+        let Some(root) = self.root.clone() else {\n+            return;\n+        };\n+        let edited: BTreeSet<String> = self.files.iter().map(|f| key(&f.path)).collect();\n+        walk(&root, 0, &self.expanded, &edited, &mut self.rows);\n     }\n⋯"
                    .to_string(),
            ),
        );
        // Two directories unfolded, so the tree shows its shape instead of a single row.
        wsview::expand(&app.wsview, root.join("src"));
        wsview::expand(&app.wsview, root.join("gui").join("src"));
        // Land on the diff: the window's whole point is "what did this turn change", and an empty
        // right pane would demo the frame without demoing the feature.
        wsview::select(
            &app.wsview,
            root.join("src").join("tool").join("hostfs.rs"),
            wsview::Mode::Diff,
        );
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
            let plan_h = self.plan_block_height();
            let composer_h = self.composer_base_h + plan_h;
            let origin = Pos2::new(column_rect.left(), column_rect.bottom() - 18.0 - composer_h);
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

        // Before the settings dialog, because it belongs to the history panel: the two are never
        // open at once, and a modal drawn later wins any overlap.
        self.name_dialog_overlay(ctx);
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

/// The plan block inside the input card: header row, one row per step, and the gap under the
/// divider.
///
/// These are **reserve** numbers, not drawing numbers: the block lays itself out naturally, and the
/// transcript reserves at the end of its content what these add up to. Deliberately a couple of
/// points generous — under-reserving is the reported bug (the last line of the conversation hides
/// behind the card), while over-reserving is a few blank pixels nobody notices.
const PLAN_HEAD_H: f32 = 24.0;
const PLAN_ROW_H: f32 = 22.0;
/// Breathing room under the last step row.
const PLAN_GAP: f32 = 9.0;
/// The hairline between the plan and the text box, plus its margins. **Constant**, never animated:
/// a rule that slides with the rows crosses the half-revealed text on the way down (that was the
/// "重叠" seen while opening, 2026-10-23).
const PLAN_DIV_H: f32 = 10.0;
/// Disclosure clock. Short enough to read as "opened", long enough not to be a jump cut.
const PLAN_ANIM_SECS: f32 = 0.2;

/// How many conversations the history overlay lists at most.
///
/// No pagination: a few dozen is already plenty for "scroll back and find the one from just now",
/// whereas listing everything would mean scanning the whole directory on every refresh. To really
/// go further back, both `/history` and the `history/` directory are there.
const HISTORY_LIST_LIMIT: usize = 80;

// ────────────── geometry of the title bar and the history drawer ──────────────
//
// These numbers used to be scattered across two places: the buttons' landing spots lived in
// `top_bar`'s `exact_height(50)` + `Margin::symmetric(20, 0)`, and the history column's width
// lived in `SidePanel::default_width(266)`. With both the top bar and the side column removed they
// have no "container" to lean on, so they are pinned explicitly here.

/// The denominator for the ring when the host reports no window at all (it normally reports one:
/// `[provider] context_window` has a default now).
///
/// The same constant the agent compacts against (`aox::config::DEFAULT_CONTEXT_WINDOW`), not a second
/// number that happens to agree today: a ring showing "40% full" while compaction runs at a different
/// threshold is a display that lies.
const DEFAULT_CONTEXT_WINDOW: u64 = aox::config::DEFAULT_CONTEXT_WINDOW as u64;
/// The top-left button's hot zone (and the drawer head's, which shares the same drawing).
///
/// 32 rather than the 36 this used to be: the drawer's head icons are 32 so that the whole row
/// lines up with the "new conversation" row below it, and one number for both places is the point
/// of [`sidebar_button`] — the floating button and the in-panel one have to look like the same
/// control.
const BAR_BTN: f32 = 32.0;
/// Height of the top row's strip — the band that drags the window and holds the plates.
///
/// It is **not** a reserved panel: nothing is cut out of the column, the background image simply
/// runs underneath. 52 keeps the row on the same line the old bar had, and the transcript's own
/// 76-point top padding (see [`App::transcript`]) is what keeps text out of it.
const TITLEBAR_H: f32 = 52.0;
/// Distance from the strip's edge to its first plate — the same 10 [`gwin::header`] leaves at the
/// far right, so the two windows are framed by the same margin.
const TITLEBAR_INSET: f32 = 10.0;
/// The square plate a single button floats on: [`BAR_BTN`] plus one point of breathing on each
/// side, so the button's hover fill never touches the plate's hairline.
const PLATE: f32 = 34.0;
/// Gap between two **separate** floating window buttons: wide enough that they read as three
/// buttons rather than as one segmented strip, narrow enough that they still belong together.
const WBTN_GAP: f32 = 6.0;
/// Left edge of the status pill, measured from the strip's left edge.
///
/// Fixed rather than "after whatever was drawn before": the pill is drawn whether or not the drawer
/// toggle occupies the slot before it, so opening the drawer can't make the readout jump.
const STATUS_X: f32 = 52.0;
/// Height of the status pill — [`PLATE`] minus 6, so a readout is visibly lighter than a control.
const STATUS_H: f32 = 28.0;
/// Breathing room inside the status pill.
const STATUS_PAD: f32 = 12.0;
/// From the busy dot's centre to the status text's left edge — the same 9 [`gwin::header`] leaves
/// between its dot and its title.
const STATUS_DOT_GAP: f32 = 9.0;
/// Width of the history drawer.
///
/// 300: the drawer is a **full-height column**, not a card with a list in it, so it is sized like a
/// sidebar (a share of the window, not "as wide as the titles happen to need"). Conversation titles
/// are elided against what is left -- at this width that is ~24 Han characters, and the row's
/// second line already carries the time and the message count.
const HISTORY_W: f32 = 300.0;
/// Left inset of a conversation row under its workspace row.
///
/// 14 rather than the old 18: the row no longer leads with a dot (the nesting is read from the
/// indent plus the folder above it), so it only has to clear the workspace row's icon column.
const HIST_INDENT: f32 = 14.0;
/// Width of a right-click menu's rows.
///
/// Fixed rather than "as wide as the longest label": the two menus (a conversation's and a
/// workspace's) would otherwise come out different widths, and a menu that resizes itself when the
/// pointer crosses rows reads as the UI twitching under the hand.
///
/// `pub(crate)`: the transcript's link menu (`markdown::link_menu`) draws its rows with the same
/// widget and the same width, so a menu opened on a path is indistinguishable from one opened on a
/// conversation.
pub(crate) const MENU_W: f32 = 178.0;

/// Height of the drawer's head: icon row, "new conversation", search, section title.
///
/// **Exact, not measured**: the drawer zeroes `item_spacing` and places every gap by hand (see
/// [`App::drawer`]), so this is a plain sum -- 12 (top margin) + 32 (icon row) + 10 + 40 (new
/// conversation) + 10 + 34 (search) + 18 + 16 (section title) + 7. If one of those gaps is edited
/// below, this number has to follow, or the list gets the wrong allowance.
const DRAWER_HEAD: f32 = 179.0;
/// Height of the drawer's foot: the rule above it plus its two rows.
///
/// 8 (gap) + 1 (rule) + 8 + 32 + 2 + 32. Same rule as [`DRAWER_HEAD`]: a plain sum of what is drawn.
const DRAWER_FOOT: f32 = 83.0;
/// How long the drawer takes to take its width / give it back (seconds).
///
/// Shorter than the 0.24 this used to be: the drawer now **pushes** the transcript, so every frame
/// of the animation is a frame in which the text re-wraps. 0.18 still reads as motion and gets the
/// reflow over with before the eye starts following the words.
const HIST_ANIM_SECS: f32 = 0.18;

/// Ease-out curve (cubic ease-out): quick off the mark, slow to settle.
///
/// Used instead of the built-in `ctx.animate_bool_with_time` (which interpolates **linearly**):
/// constant-speed motion makes hard corners at both ends -- it kicks off as if flicked and stops
/// as if it hit a wall.
fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

/// The sidebar toggle — **one control in two places**: the button in the window's title bar while
/// the drawer is shut, and the same button in the drawer's head while it is open.
///
/// Sharing the drawing is the point. The two used to be different glyphs (a rule with three lines
/// floating over the transcript, a panel outline inside the panel) at different sizes with
/// different resting backdrops, so "the thing that opens the sidebar" and "the thing that closes
/// it" didn't look related at all — the user's complaint. Now: same 32px hot zone, same panel
/// outline with the divider down its left edge, same 1.3px stroke, same hover language.
///
/// The only thing that still differs is the **resting backdrop**, and it has to: the floating copy
/// sits straight on the transcript text and is illegible without a plate under it, while the one
/// in the drawer's head already sits on the drawer's own surface, where a plate would be a second
/// box drawn on a box.
fn sidebar_button(ui: &mut Ui, open: bool, floating: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(BAR_BTN), Sense::click());
    let hov = pointer_if(ui, &resp);
    let fg = if hov {
        P::ink()
    } else if open {
        P::text_dim()
    } else {
        P::text_faint()
    };

    let painter = ui.painter();
    if floating || hov {
        painter.rect_filled(
            rect,
            CornerRadius::same(9),
            if hov { P::surface_hi() } else { P::panel() },
        );
    }
    // The glyph: a panel with a divider down its left edge — "there is a column over there".
    let r = Rect::from_center_size(rect.center(), Vec2::new(14.0, 12.0));
    painter.rect_stroke(
        r,
        CornerRadius::same(2),
        Stroke::new(1.3, fg),
        egui::StrokeKind::Inside,
    );
    painter.line_segment(
        [Pos2::new(r.left() + 4.6, r.top()), Pos2::new(r.left() + 4.6, r.bottom())],
        Stroke::new(1.3, fg),
    );
    resp.on_hover_text(if open {
        aox::i18n::t("Hide history")
    } else {
        aox::i18n::t("Show history")
    })
}

/// The context gauge: a ring that fills as the conversation eats the model's context window.
///
/// `used / limit` is the whole reading, and it is deliberately tiny and unlabelled — it answers
/// "am I about to run out of room" at a glance, and the exact figures are one hover away. Colour is
/// the only alarm, and it borrows the three-band language the cache-hit map already taught this UI:
/// quiet ink below 75%, amber past it, brick past 92%.
///
/// Drawn as a hand-built polyline rather than a real arc: egui has no arc primitive, and at 16px a
/// 40-segment `PathShape` is pixel-identical to one. Zero arc → no fill at all, so a fresh
/// conversation shows the empty track and nothing else.
///
/// `spin` is `Some(seconds since the compaction started)` while the host is folding history into a
/// summary. In that state the ring **stops being a reading**: the numbers will only be known after
/// the next request, so filling it with the old (full) value would say "still full" at the exact
/// moment the problem is being solved. Instead a short arc travels around the track — the one
/// animation this row has, and the honest shape for "working, no measurement yet".
fn context_ring(ui: &mut Ui, used: u64, limit: u64, spin: Option<f32>) -> egui::Response {
    let d = 16.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(d), Sense::hover());
    let c = rect.center();
    let r = d * 0.5 - 1.3;
    let p = ui.painter();
    p.circle_stroke(c, r, Stroke::new(2.0, P::border()));
    if let Some(t) = spin {
        // One turn per second, a 90° tail: slow enough to read as "working", fast enough that a
        // one-second summary does not look frozen.
        let sweep = std::f32::consts::FRAC_PI_2;
        let a0 = -std::f32::consts::FRAC_PI_2 + t * std::f32::consts::TAU;
        let n = 10usize;
        let pts: Vec<Pos2> = (0..=n)
            .map(|i| {
                let a = a0 + sweep * (i as f32 / n as f32);
                Pos2::new(c.x + r * a.cos(), c.y + r * a.sin())
            })
            .collect();
        p.add(egui::Shape::line(pts, Stroke::new(2.0, P::text_dim())));
        return resp.on_hover_text(aox::i18n::t("Compacting the context…"));
    }
    let frac = if limit == 0 {
        0.0
    } else {
        (used as f32 / limit as f32).clamp(0.0, 1.0)
    };
    if frac > 0.0 {
        let col = if frac >= 0.92 {
            P::danger()
        } else if frac >= 0.75 {
            P::warn()
        } else {
            P::text_dim()
        };
        let n = ((frac * 40.0).ceil() as usize).max(2);
        let start = -std::f32::consts::FRAC_PI_2; // 12 o'clock, clockwise from there
        let pts: Vec<Pos2> = (0..=n)
            .map(|i| {
                let a = start + std::f32::consts::TAU * frac * (i as f32 / n as f32);
                Pos2::new(c.x + r * a.cos(), c.y + r * a.sin())
            })
            .collect();
        p.add(egui::Shape::line(pts, Stroke::new(2.0, col)));
    }
    let left = limit.saturating_sub(used);
    resp.on_hover_text(aox::tf!(
        "Context: {} of {} used, {} left",
        thousands(used),
        thousands(limit),
        thousands(left)
    ))
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

/// One workspace row in the drawer: a folder, the name, how many conversations are in it, and a
/// disclosure chevron on the right. The whole row is the click target for folding / unfolding.
///
/// The count is drawn whether or not the group is folded: `list()` scans every workspace in one go
/// (see the store's multi-directory layout), so the number is already in hand -- a "not counted yet"
/// placeholder would be a promise the panel has no reason to break. While a search is running it is
/// the number of **matches**, not of conversations, because that is what is under the row.
///
/// Returns the row's own response (the fold toggle) plus whether the hovered "＋" was clicked. Two
/// answers rather than one because they are two different gestures on one rect: the "＋" is a
/// **child** click target nested inside the row, and it has to be the one that wins the pointer or
/// "new conversation here" would also fold the group the user is pointing at.
fn history_group_header(
    ui: &mut Ui,
    g: &aox::space::Group,
    width: f32,
    count: usize,
    open: bool,
) -> (egui::Response, bool) {
    let w = (width - 6.0).max(60.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click());
    // Pointer over the row -- asked of the **rect**, not `resp.hovered()`. The "＋" below is a child
    // interact target nested inside this rect, so on the frames the pointer is on it the row is no
    // longer the *hovered widget*; `resp.hovered()` would flip to false and take the row's fill --
    // and, since the "＋" is only drawn while the row reads as hovered, the glyph with it -- which is
    // a stable-looking half-frame flicker: draw, erase, draw. `rect_contains_pointer` is the one
    // reading that does not depend on which of the two widgets won the hit test.
    let hov = ui.rect_contains_pointer(rect);
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().rect_filled(rect, CornerRadius::same(8), P::hover_fill());
    }
    let fg = if hov { P::text() } else { P::text_dim() };
    let cy = rect.center().y;
    let icon_x = rect.left() + 11.0;

    let count = aox::tf!("{}", count);
    // Three gutters on the right, always taken off the name's width whether or not the glyphs are
    // drawn right now: the "＋" (hover only), the count, and the chevron. If the width only shrank
    // on hover, the name would jump sideways the instant the pointer arrived -- the one thing a
    // hover affordance must never do, since it moves the ground out from under the click that is
    // about to happen.
    let chev_w = 20.0;
    let count_w = 22.0;
    let plus_w = 22.0;
    // Far enough right that the name clears the folder instead of sitting on its corner.
    let name_x = icon_x + 15.0;
    let name_w = (rect.right() - 6.0 - chev_w - count_w - plus_w - name_x).max(24.0);
    // Bold marks the header as a heading rather than a row (same treatment as markdown titles and
    // the activity stream's headings).
    let name = elide(ui, &g.name, theme::bold_font(12.5), fg, name_w);
    let cnt = elide(ui, &count, theme::body_font(11.0), P::text_faint(), count_w);

    let painter = ui.painter();
    // A folder, where this used to be a disclosure triangle. What it adds is the one thing a
    // header could never show: that this row is a *directory*, which is also what tells a workspace
    // apart from a conversation at a glance. Same 13x11 box the workplace picker's rows use, so the
    // two trees draw the same folder. The folded-or-not signal is the chevron on the right.
    workplace_ui::paint_folder(
        painter,
        Rect::from_center_size(Pos2::new(icon_x, cy), Vec2::new(13.0, 11.0)),
        fg,
    );
    painter.galley(Pos2::new(name_x, cy - name.size().y * 0.5), name, fg);
    let cnt_x = rect.right() - 6.0 - chev_w - cnt.size().x;
    painter.galley(Pos2::new(cnt_x, cy - cnt.size().y * 0.5), cnt, P::text_faint());

    // ── the chevron ──
    // Points down when the group is open, right when it is folded. Drawn as two strokes rather than
    // a glyph: a font's "v" changes shape with whichever face the build picked, and this one has to
    // sit on the same optical centre as the folder.
    let chev_x = rect.right() - 6.0 - chev_w * 0.5;
    let s = 3.6;
    let stroke = Stroke::new(1.4, if hov { P::text_dim() } else { P::text_faint() });
    let chev = if open {
        vec![
            Pos2::new(chev_x - s, cy - s * 0.5),
            Pos2::new(chev_x, cy + s * 0.6),
            Pos2::new(chev_x + s, cy - s * 0.5),
        ]
    } else {
        vec![
            Pos2::new(chev_x - s * 0.5, cy - s),
            Pos2::new(chev_x + s * 0.6, cy),
            Pos2::new(chev_x - s * 0.5, cy + s),
        ]
    };
    painter.add(egui::Shape::line(chev, stroke));

    // ── the "＋": new conversation *in this workspace* ──
    //
    // Drawn only for a group with a real folder (`g.root.is_some()`), matching the context menu
    // below: `Default` and `Full access` are not directories, so there is nowhere to put a
    // conversation and the glyph would be a lie. Drawn only on hover for the same reason it is
    // hidden at rest -- at rest the header is a *label*, and five of them at once wearing a "＋"
    // would turn a list of workspaces into a row of buttons.
    //
    // It sits **before the count** rather than at the far right: the count is the row's own data and
    // it should not be pushed around by a control, and "＋ 3" reads as "add one to these three",
    // which is exactly what the click does.
    let mut plus_clicked = false;
    if g.root.is_some() && hov {
        let plus_rect = Rect::from_center_size(
            Pos2::new(rect.right() - 6.0 - chev_w - count_w - plus_w * 0.5, cy),
            Vec2::splat(20.0),
        );
        let click = ui.interact(plus_rect, resp.id.with("new-conv"), Sense::click());
        let on = pointer_if(ui, &click);
        if on {
            // One more step than the row's own fill, so a second hoverable thing inside a hovered
            // row is still readable as "this, specifically, is the target".
            ui.painter().rect_filled(plus_rect, CornerRadius::same(6), P::active_fill());
        }
        let pfg = if on { P::ink() } else { P::text_dim() };
        let s = 3.8;
        let p = ui.painter();
        p.line_segment(
            [Pos2::new(plus_rect.center().x - s, cy), Pos2::new(plus_rect.center().x + s, cy)],
            Stroke::new(1.5, pfg),
        );
        p.line_segment(
            [Pos2::new(plus_rect.center().x, cy - s), Pos2::new(plus_rect.center().x, cy + s)],
            Stroke::new(1.5, pfg),
        );
        plus_clicked = click.clicked();
        if click.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }

    // The workspace's directory is the one thing a header can't show (a path doesn't fit, and for
    // the built-ins there is nothing to show anyway), so it rides on the tooltip: with several
    // workspaces listed, "which folder is this one again" is exactly the question the tree raises.
    let resp = match g.root.as_deref() {
        Some(root) => resp.on_hover_text(root.display().to_string()),
        None => resp,
    };
    (resp, plus_clicked)
}

/// The name dialog's state while it is open (see [`App::name_dialog`]).
struct NameDialog {
    /// Which question is being asked. The shapes below are shared by all three.
    mode: NameDialogMode,
    /// What the user has typed, bound straight to the text field.
    name: String,
    /// Why the last attempt was refused. Cleared as soon as the name changes, so the message can
    /// never outlive the input it was about.
    error: Option<String>,
    /// Whether the field has been handed the caret yet. It has to be done **once**, on the frame the
    /// dialog appears: asking every frame would fight the user from the moment they clicked anywhere
    /// else in the dialog, and `request_focus` on an already-focused field is not idempotent in the
    /// way that would fix it -- it would just keep pulling the caret back.
    focused: bool,
}

/// Which question the shared shell is asking.
///
/// The type is named after none of its three jobs on purpose: it asks about a workspace's folder, a
/// workspace's label and a conversation's label, and a name taken from one of them is a name that
/// gets read instead of the code.
enum NameDialogMode {
    /// Make a folder under `<app root>/workplace/` and register it.
    New,
    /// Re-label an **existing** folder. The folder itself is never moved — see
    /// [`aox::space::Registry::rename_workspace`] — which is why the path below is read from here
    /// rather than computed from the typed name.
    Rename { root: PathBuf },
    /// Re-label one **conversation**. Nothing on disk is renamed: the id (which is the filename) is
    /// left alone and a row carrying the new title is appended to the conversation's own file. So
    /// there is no path to spell out here, and the folder-name rule does not apply either — a title
    /// is only a label, so `notes/2026` and `v1.2.` are both fine (see [`check_conv_title`]).
    RenameConversation { id: String },
}

impl NameDialogMode {
    /// A stable tag for the `Modal` id. Three questions share one body, and a `Modal` keeps its state
    /// under its id — one id for all three would let a rename inherit whatever the previous question
    /// left in the field.
    fn tag(&self) -> u8 {
        match self {
            Self::New => 0,
            Self::Rename { .. } => 1,
            Self::RenameConversation { .. } => 2,
        }
    }
}

impl NameDialog {
    fn new() -> Self {
        Self { mode: NameDialogMode::New, name: String::new(), error: None, focused: false }
    }

    /// Open for a workspace rename, with the field **pre-filled**: the current name is the thing being
    /// edited, so an empty field would make the common case (fix one character) the awkward one.
    fn renaming(root: PathBuf, current: String) -> Self {
        Self { mode: NameDialogMode::Rename { root }, name: current, error: None, focused: false }
    }

    /// Open for a conversation rename. Pre-filled for the same reason as the workspace case — and with
    /// the title the row is *showing*, so clearing it and saving is visibly "back to the first
    /// message" rather than "back to something invisible".
    fn renaming_conversation(id: String, current: String) -> Self {
        Self {
            mode: NameDialogMode::RenameConversation { id },
            name: current,
            error: None,
            focused: false,
        }
    }

    /// The folder being renamed, or `None` in the other two modes.
    fn rename_root(&self) -> Option<&Path> {
        match &self.mode {
            NameDialogMode::Rename { root } => Some(root),
            _ => None,
        }
    }

    /// The conversation being renamed, or `None` in the other two modes.
    fn conv_id(&self) -> Option<&str> {
        match &self.mode {
            NameDialogMode::RenameConversation { id } => Some(id),
            _ => None,
        }
    }
}

/// What the user did with the workspace dialog this frame.
enum DialogAction {
    /// "Create" in the new-workspace mode, "Save" in the rename mode.
    Confirm,
    Cancel,
}

/// Draw the name dialog — "new workspace", "rename workspace" or "rename conversation". Returns
/// what the user asked for, if anything.
///
/// A free function taking the state by `&mut` rather than a method on `App`: the body needs no
/// `App` state at all (`app_root` comes in as a parameter), so keeping it out of the `impl` is what
/// lets the caller hold the dialog's state apart from `self` while it is open.
///
/// The three modes share one body on purpose. They ask the same kind of question — "what should
/// this thing be called?" — and differ only in the words around the field (the title, the one-line
/// blurb, the button's verb) and in **which rule validates it and what the answer goes to**. Three
/// dialogs would duplicate the input handling, the Enter key, the caret handoff and the error slot —
/// i.e. every part of this that can have a bug in it.
fn name_dialog_modal(
    ctx: &egui::Context,
    st: &mut NameDialog,
    app_root: Option<&Path>,
) -> Option<DialogAction> {
    // Which of the three questions this is, read once here instead of five times below. Booleans and
    // a `u8` rather than a held reference to the mode: the body needs `&mut st.name` a few lines down,
    // and a borrow of `st` living across that is exactly what the compiler refuses.
    let creating = matches!(st.mode, NameDialogMode::New);
    let renaming_conversation = st.conv_id().is_some();
    let mut action = None;
    // A separate id per question: all three share one body, and a `Modal` keeps its state under this
    // id, so one id would let a rename inherit whatever the previous question left in the field.
    let resp = egui::Modal::new(egui::Id::new(("aox-name", st.mode.tag())))
        .backdrop_color(P::scrim())
        .frame(
            theme::raised(theme::card_radius())
                .inner_margin(Margin::symmetric(22, 18)),
        )
        .show(ctx, |ui| {
            ui.set_width(430.0);
            // The words, the field's hint and the one sentence of explanation — the only things the
            // three questions do not share. Everything else (the field, the caret handoff, Enter, the
            // error slot, the buttons) is one body for a reason: it is all the part that can have a
            // bug in it.
            let (title, blurb, hint) = match &st.mode {
                NameDialogMode::New => (
                    "New workspace",
                    aox::i18n::t("A folder of its own, with its own conversations. Writes and deletes stay inside it."),
                    aox::i18n::t("Workspace name"),
                ),
                NameDialogMode::Rename { .. } => (
                    "Rename workspace",
                    aox::i18n::t("Only this label changes — the folder stays where it is. Clear the name to use the folder's own name."),
                    aox::i18n::t("Workspace name"),
                ),
                // The one thing worth saying here is that nothing on disk is renamed: the *row* is a
                // label, the file behind it keeps its name, and clearing the field is how you get the
                // automatic title back.
                NameDialogMode::RenameConversation { .. } => (
                    "Rename conversation",
                    aox::i18n::t("Only this row's label changes; the file keeps its name on disk. Clear it to go back to the first message."),
                    aox::i18n::t("Conversation name"),
                ),
            };
            ui.label(RichText::new(aox::i18n::t(title)).color(P::text()).size(16.0).strong());
            ui.add_space(5.0);
            // One line, and it says what the thing *is* rather than where it goes: in the workspace
            // modes the concrete path below answers "where" far better than prose can, and this screen
            // has room for exactly one sentence.
            //
            // In the workspace-rename case the sentence is doing real work rather than setting a tone.
            // The field holds a *name* and the box below holds a *folder*, and without saying so it is
            // perfectly reasonable to read this screen as "these two are kept in step" — which is the
            // one thing it does not do.
            ui.label(RichText::new(blurb).color(P::text_faint()).size(12.5));
            ui.add_space(12.0);

            // The field's height comes from **this margin**, not from a minimum size. A `TextEdit`'s
            // box is its text plus its margin, and singleline text is drawn aligned inside whatever
            // box it is handed -- so padding is the only way to get a taller field without the text
            // ending up stuck to the top edge. The default (`Margin::symmetric(4, 2)`) makes a ~23px
            // strip, which reads as cramped next to this dialog's 16px title and its buttons.
            let edit = ui.add(
                TextEdit::singleline(&mut st.name)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(11, 10))
                    .font(FontId::proportional(14.0))
                    .hint_text(
                        RichText::new(hint).color(P::quiet()).size(13.5),
                    ),
            );
            if !st.focused {
                edit.request_focus();
                // ...and put the caret at the **end of the pre-filled name**, explicitly.
                //
                // `request_focus` alone does not promise *where in the text* the caret lands.
                // `TextEdit` only falls back to "end of text" when it has no stored cursor to
                // resume from (`default_cursor_range`, consulted in egui's own `events`), and this
                // field keeps its state under one id across every opening of the dialog. So the
                // second time it is opened, the index a previous rename left behind is what the
                // caret resumes at -- and when that is 0 the caret sits *before* the pre-filled
                // name, which makes backspace do nothing and typing prepend: "rename this" turns
                // into "rename thisOld name". Written out here, the common case (adjust the tail of
                // the name) works on the first try, every time the dialog is opened.
                let mut state =
                    egui::TextEdit::load_state(ui.ctx(), edit.id).unwrap_or_default();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(st.name.chars().count()),
                )));
                state.store(ui.ctx(), edit.id);
                st.focused = true;
            }
            if edit.changed() {
                st.error = None;
            }

            // The path, spelled out. It means a different thing in each of the two modes that have
            // one -- the folder that *is* there vs. the folder that *will* be -- which is why the
            // arms stay separate instead of one expression with a flag inside it. The conversation
            // rename is the third state and has no path at all.
            ui.add_space(8.0);
            match (renaming_conversation, st.rename_root(), app_root) {
                // **Renaming a conversation**: nothing on disk moves -- the id keeps its filename and
                // the new label is a row inside the file -- so there is no directory to spell out
                // here. The same vertical space is reserved anyway, so the buttons sit in the same
                // place as in the other two questions.
                (true, _, _) => ui.add_space(31.0),
                // **Renaming**: the folder that is already there. Tied to no typed text, so the box
                // does not flicker as the name is edited — and it is shown even while the field is
                // blank, which is the state that means "use the folder's own name".
                (_, Some(ws), _) => {
                    let path = ws.display().to_string();
                    theme::well(8)
                        .inner_margin(Margin::symmetric(10, 7))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new(workplace_ui::elide_path(&path, 56))
                                    .font(theme::mono_font(12.5))
                                    .color(P::text_dim()),
                            );
                        });
                }
                // **Creating**: where the folder *will* land. The user is about to let the model write
                // inside this directory, so "which directory" is the one thing this screen must not
                // leave implied -- and the name typed above is only half of the answer.
                //
                // Drawn for an empty or unusable name too, showing just the parent: `workplace/`. The
                // space below the field is reserved in every case anyway (see the catch-all arm), and
                // an empty box was the one state where that reservation read as a hole in the dialog.
                // A box that is always there is also a box that never jumps.
                (_, None, Some(root)) => {
                    let base = root.join(aox::workplace::WORKPLACE_DIR);
                    let path = match check_ws_name(&st.name) {
                        Ok(name) => base.join(name),
                        Err(_) => base,
                    };
                    theme::well(8)
                        .inner_margin(Margin::symmetric(10, 7))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new(workplace_ui::elide_path(&path.display().to_string(), 56))
                                    .font(theme::mono_font(12.5))
                                    .color(P::text_dim()),
                            );
                        });
                }
                // The only case left: no known app root, so there is no path to spell out. The same
                // vertical space is reserved anyway, so the buttons sit in the same place in every
                // state this dialog can be in.
                _ => ui.add_space(31.0),
            }

            // Why the button would not do anything, shown **as it is typed** rather than only after
            // a submit. The alternative is a confirm button that silently ignores the click, and "I
            // pressed it and nothing happened" is the one thing this dialog must never do. The
            // submit-time failure (a registry that cannot be written) rides in the same slot: it is
            // about the same text, and it is dropped the moment that text changes.
            //
            // Nothing is said about an **empty** field: it starts that way, and a dialog that opens
            // complaining has already spent the user's attention on nothing. (An empty field is not
            // merely tolerated either -- in the rename mode it is a real answer, which is why nothing
            // here refuses it and the caller is left to read the meaning.)
            // A conversation title is validated by a **different rule** to a folder name: a title is
            // only a label, so separators and dots are all perfectly good in it. The one thing it
            // cannot be is longer than the row can show, and that is what `check_conv_title` says.
            let live = if st.name.trim().is_empty() {
                None
            } else if renaming_conversation {
                check_conv_title(&st.name).err()
            } else {
                check_ws_name(&st.name).err()
            };
            if let Some(e) = live.or_else(|| st.error.clone()) {
                ui.add_space(8.0);
                ui.label(RichText::new(e).color(P::text()).size(13.0));
            }

            ui.add_space(16.0);
            ui.horizontal(|ui| {
                // Enter in the name field does the same as the button. The standard test for it on a
                // single-line field: it **lost focus** this frame, and Enter is what took the focus
                // away (a bare `key_pressed` would also fire while the field still has it).
                //
                // The caret is handed straight back when that happens, **unconditionally**: a
                // single-line field surrenders focus on Enter, so after a refusal the obvious next
                // move -- fix the name, press Enter again -- would otherwise be typed into nothing.
                // When the confirm does succeed this is a no-op; the dialog is gone by the next frame.
                let entered = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if entered {
                    edit.request_focus();
                    action = Some(DialogAction::Confirm);
                }
                // The button is deliberately **not** disabled while the name is invalid: a control
                // that looks live and silently does nothing is the worst of both worlds, while
                // pressing it just runs the validation and says why. The live message above is what
                // makes that feel like an answer rather than a refusal.
                let verb = if creating { "Create" } else { "Save" };
                if primary_button(ui, aox::i18n::t(verb)).clicked() {
                    action = Some(DialogAction::Confirm);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if flat_button(ui, aox::i18n::t("Cancel")).clicked() {
                        action = Some(DialogAction::Cancel);
                    }
                });
            });
        });

    // Clicking the backdrop or pressing Esc closes it -- the standard exit `egui::Modal` provides --
    // and that has to mean **cancel**: nothing was asked for, so nothing is written.
    if resp.should_close() {
        action.get_or_insert(DialogAction::Cancel);
    }
    action
}

/// Validate a typed workspace name and hand back the trimmed form.
///
/// The only place in the GUI that has to think about what a filename may contain, and it matters
/// more than it looks: the name becomes a **path segment**, so a separator or a `..` here would
/// silently point the whole workspace -- and every write the model makes under it -- somewhere else
/// than the dialog's path preview promised. The reserved characters are rejected on **every**
/// platform rather than only on Windows: "works on my machine" is a worse outcome than one slightly
/// stricter rule, and the registry has to stay readable by every build.
fn check_ws_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(aox::i18n::t("Please give the workspace a name.").to_string());
    }
    // Structural, not "a character we dislike": a leading dot makes a hidden directory, and Windows
    // silently drops a **trailing dot**, so `"notes."` and `"notes"` would be one folder wearing two
    // different rows in the tree. Both get the same message -- the user's fix is identical in either
    // case, which is to type a plain folder name.
    //
    // A trailing **space** is deliberately not in this list: the trim above already dealt with it,
    // and refusing what `"notes "` becomes (`"notes"`, a perfectly good name) would be a puzzle
    // rather than a rule.
    let structural = name.starts_with('.') || name.ends_with('.');
    let illegal = name
        .chars()
        .any(|c| matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control());
    if structural || illegal {
        return Err(aox::i18n::t("That name cannot be used for a folder.").to_string());
    }
    Ok(name.to_string())
}

/// Validate a typed conversation title and hand back the trimmed form.
///
/// Deliberately **not** [`check_ws_name`]. A title never becomes a path: it is one row inside the
/// conversation's own file, so the separators, `?`, `"` and even a trailing dot are all perfectly
/// good in it — a thread called `notes/2026` or `v1.2.` is fine, and refusing those would just be a
/// folder rule leaking onto a label. What actually matters here is two other things:
///
/// - **Length.** The row draws the title on one line, and the store clamps what it reads back to
///   [`aox::store::TITLE_MAX_CHARS`] — so a longer title would be silently cut, and "what I typed
///   is not what got saved" is exactly the kind of quiet edit this dialog exists to prevent.
///   Refusing it here states the limit instead of applying it behind the user's back.
/// - **Control characters.** A tab or a stray `\r` would land in the persisted row and then in the
///   row's layout. A single-line field cannot be *typed* into one, but a paste can carry one.
///
/// An **empty** string is `Ok`, not an error: for a title, blank is a real answer — "go back to the
/// automatic title, the first message" — so refusing it is not this function's business. (The
/// caller is what reads a blank as that meaning; see [`App::rename_conversation`].)
fn check_conv_title(raw: &str) -> Result<String, String> {
    let title = raw.trim();
    if title.chars().any(char::is_control) {
        return Err(aox::i18n::t("A conversation name cannot contain control characters.").to_string());
    }
    if title.chars().count() > aox::store::TITLE_MAX_CHARS {
        return Err(aox::tf!(
            "A conversation name can be at most {} characters.",
            aox::store::TITLE_MAX_CHARS
        ));
    }
    Ok(title.to_string())
}

/// The settings button in the drawer's head: same 32px hot zone and same hover language as
/// [`sidebar_button`] (they sit side by side, and two different box sizes on one row is exactly the
/// kind of thing that reads as sloppy), **no backdrop at rest** — the drawer's own surface is the
/// backdrop.
fn gear_button(ui: &mut Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(BAR_BTN), Sense::click());
    let hov = pointer_if(ui, &resp);
    if hov {
        ui.painter().rect_filled(rect, CornerRadius::same(9), P::hover_fill());
    }
    let fg = if hov { P::ink() } else { P::text_dim() };
    draw_gear(ui.painter(), rect.center(), 7.8, 5.5, 1.3, fg);
    resp.on_hover_text(aox::i18n::t("Settings"))
}

/// The gear, drawn as a closed polyline of teeth plus a centre hole.
///
/// One drawing for both sizes (the drawer head and the settings dialog's rows): two copies
/// would drift apart the first time either is touched, and the difference is only three numbers.
fn draw_gear(
    painter: &egui::Painter,
    c: Pos2,
    r_out: f32,
    r_in: f32,
    width: f32,
    fg: Color32,
) {
    const TEETH: usize = 8;
    // The circle is divided into TEETH segments, and each one holds four points:
    // "tip -> valley -> valley -> tip".
    let step = std::f32::consts::TAU / TEETH as f32;
    let tooth = step * 0.22; // the tooth tip's share of the angle (the rest is the valley)
    let mut pts: Vec<Pos2> = Vec::with_capacity(TEETH * 4 + 1);
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
    painter.add(egui::Shape::line(pts, Stroke::new(width, fg)));
    // The centre hole -- with it, it looks like a gear; without it, it's an eight-pointed star
    painter.circle_stroke(c, r_out * 0.26, Stroke::new(width, fg));
}

/// The drawer's one **button**: full width, 40 high, an outline rather than a filled block.
///
/// A reversed (ink) block would shout louder than anything else in the drawer, and "new
/// conversation" is the one action here that is also reachable from the keyboard — so it is drawn
/// as the quietest thing that still reads as pressable. `enabled == false` (a turn is running)
/// keeps the shape and only dims it: a button that disappears mid-turn is worse than one that says
/// "not now".
fn new_chat_row(ui: &mut Ui, width: f32, enabled: bool) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 40.0), sense);
    let hov = pointer_if(ui, &resp) && enabled;
    let p = ui.painter();
    p.rect_filled(
        rect,
        CornerRadius::same(9),
        if hov { P::active_fill() } else { P::hover_fill() },
    );
    p.rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0, P::border()),
        egui::StrokeKind::Inside,
    );
    let fg = if enabled { P::text() } else { P::quiet() };
    let cy = rect.center().y;
    let cx = rect.left() + 16.0;
    let s = 4.2;
    p.line_segment([Pos2::new(cx - s, cy), Pos2::new(cx + s, cy)], Stroke::new(1.5, fg));
    p.line_segment([Pos2::new(cx, cy - s), Pos2::new(cx, cy + s)], Stroke::new(1.5, fg));
    let label = p.layout_no_wrap(
        aox::i18n::t("New conversation").to_string(),
        theme::body_font(13.0),
        Color32::PLACEHOLDER,
    );
    p.galley(Pos2::new(rect.left() + 32.0, cy - label.size().y * 0.5), label, fg);
    // The shortcut hint is drawn, not wired: there is no global key handler for it yet, and a hint
    // for a key that does nothing is exactly the kind of small lie that makes a UI feel unreliable.
    // It is here because the row is the drawer's one action and the reference puts it in this
    // corner; the binding lands with the rest of the keyboard work.
    let hint = p.layout_no_wrap(
        "Ctrl K".to_string(),
        theme::body_font(10.5),
        Color32::PLACEHOLDER,
    );
    p.galley(
        Pos2::new(rect.right() - 12.0 - hint.size().x, cy - hint.size().y * 0.5),
        hint,
        P::text_faint(),
    );
    resp
}

/// The drawer's search field: a sunken well with a magnifier, filled in place.
///
/// The `TextEdit` is placed **inside** an already-allocated rect (`ui.put`) rather than laid out,
/// so the well, the glyph and the field share one box; `frame(false)` keeps the widget from drawing
/// a second background inside the first. Focus is read from memory *before* the field is put down,
/// so the border is right on the frame the click lands rather than one frame later.
fn search_row(ui: &mut Ui, width: f32, query: &mut String) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::hover());
    let id = egui::Id::new("aox-hist-search");
    let focused = ui.memory(|m| m.has_focus(id));
    let p = ui.painter();
    // `bg`, not `code_bg`: on the drawer's `surface` this is the same one-step sunken well the
    // reference has, and it is already a token that exists in both themes.
    p.rect_filled(rect, CornerRadius::same(9), P::bg());
    if focused {
        p.rect_stroke(
            rect,
            CornerRadius::same(9),
            Stroke::new(1.0, P::border_strong()),
            egui::StrokeKind::Inside,
        );
    }
    let c = Pos2::new(rect.left() + 15.0, rect.center().y);
    p.circle_stroke(
        Pos2::new(c.x - 1.0, c.y - 1.0),
        4.2,
        Stroke::new(1.4, P::text_faint()),
    );
    p.line_segment(
        [Pos2::new(c.x + 2.0, c.y + 2.0), Pos2::new(c.x + 5.2, c.y + 5.2)],
        Stroke::new(1.4, P::text_faint()),
    );
    let inner = Rect::from_min_max(
        Pos2::new(rect.left() + 28.0, rect.top() + 7.0),
        Pos2::new(rect.right() - 8.0, rect.bottom() - 7.0),
    );
    ui.put(
        inner,
        egui::TextEdit::singleline(query)
            .id(id)
            .frame(false)
            .margin(Margin::ZERO)
            .hint_text(
                RichText::new(aox::i18n::t("Search workspaces")).color(P::quiet()).size(12.0),
            )
            .font(theme::body_font(12.0))
            .desired_width(inner.width()),
    );
}

/// A section title: small, faint, **letter-spaced**, with an optional count on the right.
///
/// The tracking is drawn by hand (one glyph at a time) because `RichText` has no letter-spacing and
/// this is the one place in the panel where the spacing *is* the design: at 10px, three Han
/// characters set solid read as a stray word, and the same three with 1.3px between them read as a
/// label over a list.
fn section_head(ui: &mut Ui, width: f32, label: &str, count: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 16.0), Sense::hover());
    let p = ui.painter();
    let font = theme::body_font(10.0);
    let cy = rect.center().y;
    let mut x = rect.left() + 2.0;
    for ch in label.chars() {
        let g = p.layout_no_wrap(ch.to_string(), font.clone(), Color32::PLACEHOLDER);
        p.galley(Pos2::new(x, cy - g.size().y * 0.5), g.clone(), P::text_faint());
        x += g.size().x + 1.3;
    }
    let c = p.layout_no_wrap(count.to_string(), font, Color32::PLACEHOLDER);
    p.galley(
        Pos2::new(rect.right() - 2.0 - c.size().x, cy - c.size().y * 0.5),
        c,
        P::text_faint(),
    );
}

/// Does this workspace survive the current search? A workspace matches by **name** (which keeps all
/// of its conversations) or by having at least one conversation whose title matches.
fn group_matches(g: &aox::space::Group, query: &str) -> bool {
    g.name.to_lowercase().contains(query)
        || g.items.iter().any(|s| s.title.to_lowercase().contains(query))
}

/// The contents of a conversation's menu — shared by the row's right-click and its "⋯" button.
///
/// A free function rather than a closure: the two doors would otherwise each need their own copy of
/// the body, and the slots they write to are `&mut` borrows that a single closure could only hand
/// out once.
fn conversation_menu(
    ui: &mut Ui,
    id: &str,
    title: &str,
    path: &Path,
    current: bool,
    move_targets: &[(String, String)],
    rename_conv: &mut Option<(String, String)>,
    move_conv: &mut Option<(String, String)>,
    recycle: &mut Vec<PathBuf>,
) {
    // Renaming is offered on **every** row, the current one included (unlike Delete below): the
    // current conversation is the one whose file the agent already has open, and the append is
    // exactly what makes the rename work there.
    if menu_item(ui, aox::i18n::t("Rename"), MENU_W, true, false).clicked() {
        *rename_conv = Some((id.to_string(), title.to_string()));
        ui.close();
    }
    // Move to → another group. A submenu rather than one flat row per group: the target list grows
    // with the user's workspaces, and a menu that keeps getting taller is worse than one click
    // deeper. Offered on every row including the current one -- re-filing is a registry edit, so it
    // touches neither the open file nor the running turn. Hidden when there is only one group, so it
    // is never a row that opens onto nothing.
    if !move_targets.is_empty() {
        ui.menu_button(aox::i18n::t("Move to"), |ui| {
            for (tid, tname) in move_targets {
                if menu_item(ui, tname, MENU_W, true, false).clicked() {
                    *move_conv = Some((id.to_string(), tid.clone()));
                    ui.close();
                }
            }
        });
    }
    // The conversation being chatted in is refused rather than hidden: the agent has it open and
    // would keep appending to a file that is no longer there. The row stays visible but greyed, with
    // the reason on hover, so the menu looks the same everywhere and the missing row is never a
    // mystery.
    let del = menu_item(ui, aox::i18n::t("Delete"), MENU_W, !current, true);
    if current {
        del.on_hover_text(aox::i18n::t("Cannot delete the conversation you are in"));
    } else if del.clicked() {
        recycle.push(path.to_path_buf());
        ui.close();
    }
}

/// Which glyph a history-panel foot row leads with.
enum FootIcon {
    /// A hand-drawn plus: "make one".
    Plus,
    /// The folder from [`workplace_ui::paint_folder`] — the same drawing a group header now carries:
    /// "the directory they all live in".
    Folder,
}

/// One row of the history panel's foot: a glyph, then a label.
///
/// Both rows go through here rather than each drawing itself: the hover fill, the three-way colour
/// rule and the click sense are the parts with anything to get wrong, and they have to be identical
/// or the two rows would answer the same pointer differently.
///
/// Hand-drawn like every other icon in this file (the folder in `workplace_ui`): a font's `+` would
/// change shape and weight with whichever font the build picked. `enabled == false` swaps the sense
/// to hover, so the row is **visible but not clickable** rather than absent -- the panel's shape does
/// not depend on whether a turn happens to be running.
fn foot_row(ui: &mut Ui, width: f32, enabled: bool, icon: FootIcon, label: &str) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new((width - 6.0).max(60.0), 32.0), sense);
    let hov = pointer_if(ui, &resp) && enabled;
    if hov {
        // The same one-step fill the list rows use, so the foot reads as part of the same list
        // rather than as a second kind of control.
        ui.painter().rect_filled(rect, CornerRadius::same(8), P::hover_fill());
    }
    let fg = if !enabled {
        P::quiet()
    } else if hov {
        P::text()
    } else {
        P::text_dim()
    };
    // Both glyphs sit on the same 11px gutter as a group header's folder, so the panel's left edge
    // reads as one column from the list all the way down to the foot.
    let cy = rect.center().y;
    let cx = rect.left() + 11.0;
    let painter = ui.painter();
    match icon {
        FootIcon::Plus => {
            let s = 4.0;
            let stroke = Stroke::new(1.5, fg);
            painter.line_segment([Pos2::new(cx - s, cy), Pos2::new(cx + s, cy)], stroke);
            painter.line_segment([Pos2::new(cx, cy - s), Pos2::new(cx, cy + s)], stroke);
        }
        FootIcon::Folder => {
            // The same 13x11 box the workplace picker's rows and the group headers use.
            workplace_ui::paint_folder(
                painter,
                Rect::from_center_size(Pos2::new(cx, cy), Vec2::new(13.0, 11.0)),
                fg,
            );
        }
    }
    let g = painter.layout_no_wrap(label.to_string(), theme::body_font(13.0), Color32::PLACEHOLDER);
    painter.galley(Pos2::new(rect.left() + 24.0, cy - g.size().y * 0.5), g, fg);
    resp
}

/// One row of a right-click menu.
///
/// Full width and left-aligned, unlike [`flat_button`] (which hugs its text): the whole row should be
/// the click target, so the pointer never has to land on the glyphs. `enabled == false` draws the
/// label faint and swallows the click -- the row stays **visible** rather than being dropped, so a
/// menu is never a different shape depending on which row it was opened over (the reason then rides
/// on the tooltip, which the caller attaches).
///
/// `strong` marks the consequential row ("Delete"): it is emphasised by **contrast, not hue**, which
/// is this palette's own rule -- hue is reserved for [`P::danger`], and that is a deep brick red the
/// theme docs forbid spending on anything recoverable. A deletion that lands in the recycle bin is
/// exactly that, so the row reads stronger rather than louder.
pub(crate) fn menu_item(ui: &mut Ui, label: &str, w: f32, enabled: bool, strong: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 30.0), Sense::click());
    let hov = pointer_if(ui, &resp) && enabled;
    if hov {
        // Hover raises the surface one step -- the same language as list rows and the top-bar icons.
        ui.painter().rect_filled(rect, CornerRadius::same(7), P::surface_hi());
    }
    let fg = if !enabled {
        P::quiet()
    } else if strong {
        P::text()
    } else if hov {
        P::text()
    } else {
        P::text_dim()
    };
    let galley =
        ui.painter().layout_no_wrap(label.to_owned(), theme::body_font(13.5), Color32::PLACEHOLDER);
    ui.painter().galley(
        Pos2::new(rect.left() + 10.0, rect.center().y - galley.size().y * 0.5),
        galley,
        fg,
    );
    resp
}

/// Show `dir` in the OS file manager.
///
/// Delegates to [`crate::open`]: there is now a second caller (a clickable path in a reply) that
/// needs exactly this behaviour, and two copies of "how this OS shows a folder" is one copy too
/// many — the copy that gets fixed is never the copy that is used.
fn reveal_in_file_manager(dir: &Path) {
    crate::open::reveal(dir);
}

/// One conversation row in the drawer: title on the first line, "last activity · message count" on
/// the second, and a "⋯" on the right once the pointer is on it.
///
/// `current` = the one being chatted in right now. It is marked by **fill only** — no dot, no left
/// bar: at rest the list is plain text on paper, and the row you are in is one shade darker than the
/// rest, which is the same signal a hover gives. `locked` = a switch request is in flight or a turn
/// is running, in which case clicks are refused and the whole row is dimmed one step. `indent`
/// shifts the text right so the row sits nested under its workspace row (the fill still spans the
/// full width, so the clickable strip doesn't shrink with the depth).
///
/// Returns the row's response plus the "⋯" child response — the second is the anchor for the row's
/// menu (`Popup::menu`), so the same menu can be opened by right-clicking the row or by clicking the
/// glyph. It is only a *click* target while the glyph is drawn; the rest of the time it senses hover,
/// so it can never swallow a click meant for the row.
fn history_row(
    ui: &mut Ui,
    s: &aox::history::Summary,
    current: bool,
    locked: bool,
    width: f32,
    indent: f32,
) -> (egui::Response, egui::Response) {
    let w = (width - 6.0).max(60.0); // leave room for the scroll bar
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 48.0), Sense::click());
    // Asked of the rect rather than of `resp`: the "⋯" below is a child interact target, and on the
    // frames the pointer is over it the row is no longer the *hovered widget* -- which would make
    // the fill (and, since the glyph is only drawn while the row reads as hovered, the glyph with
    // it) flicker off. Same reasoning as the workspace row.
    let hov = !locked && ui.rect_contains_pointer(rect);
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // The current row is filled **whether or not** it is hovered; a hovered row that is not the
    // current one gets the lighter step. Both are one fill, no border, no marker.
    let fill = if current {
        Some(P::active_fill())
    } else if hov {
        Some(P::hover_fill())
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    }

    let x = rect.left() + 10.0 + indent;
    let title_color = if locked {
        P::text_faint()
    } else if hov || current {
        P::text()
    } else {
        P::text_dim()
    };
    // The "⋯" gutter is always taken off the title's width, so the title does not reflow the moment
    // the pointer arrives and the glyph appears.
    let more_w = 22.0;
    let max_w = (rect.right() - 6.0 - more_w - x).max(24.0);
    let title = elide(ui, &s.title, theme::body_font(13.5), title_color, max_w);
    let sub = elide(
        ui,
        &aox::tf!("{} · {} msgs", s.time, s.messages),
        theme::body_font(10.5),
        P::text_faint(),
        max_w,
    );
    let painter = ui.painter();
    // A galley is positioned by its top-left, so half its own height is subtracted here to get the
    // "vertically centred on its line" placement. Two lines at 15 and 32 in a 48-high row: the block
    // reads as centred without either line touching the fill's edge.
    painter.galley(Pos2::new(x, rect.top() + 16.0 - title.size().y * 0.5), title, title_color);
    painter.galley(Pos2::new(x, rect.top() + 33.0 - sub.size().y * 0.5), sub, P::text_faint());

    // ── the "⋯" ──
    // Drawn on hover and on the current row, and never at rest: twenty rows each wearing three dots
    // would read as a column of buttons rather than a list of titles. Three 2px dots, hand-drawn --
    // a font's "…" sits on the baseline and would be off-centre against the title's optical middle.
    let show_more = hov || current;
    let more_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 6.0 - more_w * 0.5, rect.top() + 16.0),
        Vec2::splat(20.0),
    );
    let more = ui.interact(
        more_rect,
        resp.id.with("more"),
        if show_more { Sense::click() } else { Sense::hover() },
    );
    if show_more {
        let on = pointer_if(ui, &more);
        if on {
            ui.painter().rect_filled(more_rect, CornerRadius::same(6), P::active_fill());
        }
        let p = ui.painter();
        let fg = if on { P::text() } else { P::text_faint() };
        for dx in [-3.0_f32, 0.0, 3.0] {
            p.circle_filled(Pos2::new(more_rect.center().x + dx, more_rect.center().y), 1.0, fg);
        }
    }

    if locked {
        return (resp.on_hover_text(format!("{}\n{}", s.title, s.id)), more);
    }
    (
        resp.on_hover_text(aox::tf!("{}\n{}\nClick to switch to this conversation", s.title, s.id)),
        more,
    )
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
                            purpose: purpose_from_json(
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
///
/// Buttons come in exactly **three heights** (see [`flat_button`], [`primary_button`],
/// [`danger_button`]); the height is a field rather than "text + padding" so that two buttons
/// standing side by side always line up, whatever their labels are.
#[derive(Clone, Copy)]
struct Btn {
    size: f32,
    /// Horizontal padding (the vertical one is implied by `h`: the label is centred).
    pad: Vec2,
    /// Fixed height of the hot zone.
    h: f32,
    /// Lower bound on the width, so one-word buttons don't turn into tiny pills.
    min_w: f32,
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
    let w = (galley.size().x + b.pad.x * 2.0).max(b.min_w);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, b.h), Sense::click());
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
    // recolour doesn't have to re-lay-out the text. Centred in both axes: with a fixed height the
    // baseline can no longer be derived from the padding.
    let pos = Pos2::new(
        rect.center().x - galley.size().x * 0.5,
        rect.center().y - galley.size().y * 0.5,
    );
    painter.galley(pos, galley, if hov { b.fg_hover } else { b.fg });

    resp
}

/// A secondary button (e.g. "Close" in dialogs, "Refresh" in the history panel).
///
/// Transparent at rest and **one fill step** on hover — the same hover language as a list row, a
/// menu row and a toolbar icon, so "the pointer is on something clickable" always looks identical.
/// 32 high: big enough to hit without aiming, which is what "the button is too small" was about.
pub(crate) fn flat_button(ui: &mut Ui, text: &str) -> egui::Response {
    text_button(
        ui,
        text,
        Btn {
            size: 14.0,
            pad: Vec2::new(12.0, 0.0),
            h: 32.0,
            min_w: 0.0,
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
            pad: Vec2::new(20.0, 0.0),
            h: 38.0,
            min_w: 88.0,
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
            pad: Vec2::new(20.0, 0.0),
            h: 38.0,
            min_w: 88.0,
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
                RichText::new(aox::i18n::t("You can also type here…")).color(P::quiet()).size(13.0),
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
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    p.rect_filled(
        rect,
        CornerRadius::same(10),
        if resp.hovered() { P::surface_hi() } else { P::surface() },
    );
    // A stroke on hover, but `edge_raised` — a full-ink ring made the row look selected rather than
    // hovered, which is a different thing (the row is not selected until it is picked).
    if resp.hovered() {
        p.rect_stroke(
            rect,
            CornerRadius::same(10),
            Stroke::new(1.0, P::edge_raised()),
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
        .stroke(Stroke::new(1.0, P::edge()))
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
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(17.0), Sense::click());
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

/// A duration as a clock: `0:47`, `2:00`.
///
/// `m:ss` for every size rather than a decimal below a minute: this readout's whole job is
/// "elapsed over ceiling", and two different formats side by side (`47.0s / 2:00`) reads as a unit
/// mistake. Seconds are truncated, not rounded, so the number never claims a second that has not
/// passed.
fn clock(secs: f32) -> String {
    let s = secs.max(0.0) as u32;
    format!("{}:{:02}", s / 60, s % 60)
}

/// `elapsed / cap` — the one readout that answers "is this still fine".
fn span(elapsed: f32, cap: f32) -> String {
    format!("{} / {}", clock(elapsed), clock(cap))
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
            .stroke(Stroke::new(1.0, P::edge()))
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
        .stroke(Stroke::new(1.0, P::edge()))
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
        .stroke(Stroke::new(1.0, P::edge_raised()))
        .shadow(theme::card_shadow())
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
    // The file name, if it resolves to something on disk. Collected inside the row and used *after*
    // the row's own click is registered -- see below.
    let mut name_hit: Option<(egui::Rect, std::path::PathBuf)> = None;
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
            // Why this call exists, between the name and the file it works on: "why, then where" is
            // the order people read them, and without it a row reading `bash` said nothing at all
            // about what was about to run. Dimmer than the name so rows stay scannable by tool name.
            if let Some(why) = &st.purpose {
                ui.add_space(6.0);
                ui.label(RichText::new(why).color(P::text_dim()).size(13.5));
            }
            // The file this call works on, straight after its name (user request, 2026-10-11): a row
            // reading only "write_file" leaves you counting back to find out which file it was.
            // Monospaced so two paths line up, and shortened from the middle -- the tail is the half
            // that identifies a file.
            if let Some(sub) = &st.subject {
                ui.add_space(6.0);
                // Clickable when it resolves inside the workspace: "which file was that" is one
                // click from "show me". The rect is collected here and re-registered after the row's
                // own click below -- see the note there.
                let path = crate::open::resolve_relative(sub);
                let resp = ui.add(
                    Label::new(
                        RichText::new(clip_path(sub))
                            .font(theme::mono_font(13.0))
                            .color(P::text_dim()),
                    )
                    .selectable(false)
                    .sense(Sense::click()),
                );
                if path.is_some() {
                    pointer_if(ui, &resp);
                }
                if let Some(p) = path {
                    name_hit = Some((resp.rect, p));
                }
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
        // ...and for the same reason the file name has to be registered *after that* to win in turn:
        // on a foldable row a click on the path must reveal the file, not fold the diff shut.
        if let Some((rect, path)) = name_hit {
            let name = ui
                .interact(rect, ui.id().with("aox-step-file"), Sense::click())
                .on_hover_text(path.display().to_string());
            if name.clicked() {
                crate::open::reveal(&path);
            }
        }
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
        .stroke(Stroke::new(1.0, P::edge()))
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
/// Right column width = total - left column - the gap between the two.
const SETTINGS_RIGHT_W: f32 = SETTINGS_W - SETTINGS_NAV_W - 24.0;
/// The height the bottom "Close" row takes up (including the gap between it and the content).
const SETTINGS_FOOT_H: f32 = 44.0;
/// Floor for the name column in a "name + value" row.
///
/// The column is **measured**, not fixed — see `kv_key_w` for why. This is only the lower bound, so
/// that a column of short names ("History", "App root") still looks deliberate instead of hugging
/// the values. It is also what the Chinese column lands on, so the zh layout is unchanged.
const KV_KEY_MIN: f32 = 84.0;
/// Air between a name and its value: the value's first glyph is what the eye lines up on.
const KV_KEY_GAP: f32 = 18.0;

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
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let fg = if selected { P::ink() } else { P::text_dim() };

    let painter = ui.painter();
    // Selected and hovered share the **same** fill; only the marker on the left tells them apart.
    // Two different greys for "you are here" and "the pointer is here" read as two different
    // states of the same thing, which they are not.
    if selected || resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(10), P::surface_hi());
    }
    if selected {
        painter.rect_filled(
            Rect::from_min_size(
                Pos2::new(rect.left() + 6.0, rect.center().y - 8.0),
                Vec2::new(3.0, 16.0),
            ),
            CornerRadius::same(2),
            fg,
        );
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
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(10), P::surface_hi());
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
    entry_row(ui, on, EntryMark::Flow, aox::i18n::t("Open activity stream"))
}

/// Which mark an entry row paints: the activity stream's flow line, the cache map's grid, or the
/// workspace view's tree.
#[derive(Clone, Copy)]
enum EntryMark {
    Flow,
    Grid,
    Tree,
}

/// One "opens a window" row on the settings page.
///
/// A row, not a bordered button. As a full-width filled block with an outline it read as a
/// **disabled input field** — which is roughly what it looked like. The page is a column of
/// settings, so the row obeys the same rules as every other row in the app: nothing at rest, one
/// fill step on hover, full ink for the label. The chevron on the right says "this goes somewhere
/// else" without a word of explanation under it.
fn entry_row(ui: &mut Ui, on: bool, mark: EntryMark, label: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 48.0), Sense::click());
    let hot = resp.hovered() && !on;
    if hot {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // Already open: the row stays readable but stops responding. Dimming it all the way to
    // `text_faint` made it look like a broken/disabled entry rather than "the window is already up".
    let fg = if on { P::text_dim() } else { P::text() };

    let painter = ui.painter();
    if hot {
        painter.rect_filled(rect, CornerRadius::same(10), P::surface_hi());
    }
    let icon =
        Rect::from_center_size(Pos2::new(rect.left() + 20.0, rect.center().y), Vec2::splat(20.0));
    match mark {
        EntryMark::Flow => draw_flow_curve(painter, icon, fg),
        EntryMark::Grid => draw_grid_mark(painter, icon, fg),
        EntryMark::Tree => draw_tree_mark(painter, icon, fg),
    }
    let g = painter.layout_no_wrap(label.to_string(), egui::FontId::proportional(15.0), fg);
    painter.galley(Pos2::new(rect.left() + 48.0, rect.center().y - g.size().y * 0.5), g, fg);

    let c = Pos2::new(rect.right() - 20.0, rect.center().y);
    let s = 3.4;
    let st = Stroke::new(1.4, if hot { P::text() } else { P::text_faint() });
    painter.line_segment([c + Vec2::new(-s * 0.45, -s), c + Vec2::new(s * 0.65, 0.0)], st);
    painter.line_segment([c + Vec2::new(s * 0.65, 0.0), c + Vec2::new(-s * 0.45, s)], st);
    resp
}

/// The "open cache hit map" button: the same card as [`activity_open_button`], with the grid as its
/// mark.
///
/// Sharing the card is the point, not a shortcut: both rows open "a window that observes the agent",
/// and giving the second one a different height, radius or hover treatment would make the column
/// look assembled from two different programs. What differs is the glyph — the flow line is the
/// activity stream's identity, 4×4 cells are this one's.
///
/// The mark is a miniature of the window itself (hit cells filled, the rest outlined), so the two
/// read as the same object at two scales.
fn cache_open_button(ui: &mut Ui, on: bool) -> egui::Response {
    entry_row(ui, on, EntryMark::Grid, aox::i18n::t("Open cache hit map"))
}

/// The "open workspace view" button: the same card again, with a file tree as its mark.
///
/// Third of the trio, and deliberately the third: the flow line and the grid both observe the
/// agent, whereas this one observes **the files it touched** — a different question, so a different
/// glyph, but the same row, because all three open a window and the column should not start
/// changing shape halfway down.
fn wsview_open_button(ui: &mut Ui, on: bool) -> egui::Response {
    entry_row(ui, on, EntryMark::Tree, aox::i18n::t("Open workspace view"))
}

/// The tree mark: a root, two branches, two leaves — the smallest shape that reads as "a directory
/// of files" at 20 points. Drawn with the same 1.4px stroke as the other two marks so the three
/// icons sit at the same weight.
fn draw_tree_mark(p: &egui::Painter, r: Rect, fg: Color32) {
    let st = Stroke::new(1.4, fg);
    let x0 = r.left() + 2.5;
    let cy = r.center().y;
    // The trunk, and the two rows hanging off it.
    let y1 = r.top() + 4.5;
    let y2 = r.bottom() - 4.5;
    p.line_segment([Pos2::new(x0, y1), Pos2::new(x0, y2)], st);
    // Top branch: trunk -> elbow -> leaf
    p.line_segment([Pos2::new(x0, y1 + 3.0), Pos2::new(x0 + 4.0, y1 + 3.0)], st);
    p.circle_filled(Pos2::new(x0 + 7.0, y1 + 3.0), 1.9, fg);
    // Bottom branch
    p.line_segment([Pos2::new(x0, y2 - 3.0), Pos2::new(x0 + 4.0, y2 - 3.0)], st);
    p.circle_filled(Pos2::new(x0 + 7.0, y2 - 3.0), 1.9, fg);
    // A folder at the root end: a small open rectangle, which is what separates "a tree" from "a
    // fork in the road".
    let f = Rect::from_min_size(Pos2::new(x0 + 4.5, cy - 2.6), Vec2::new(8.0, 5.6));
    p.rect_stroke(f, CornerRadius::same(1), Stroke::new(1.2, fg), egui::StrokeKind::Inside);
}

/// The 4×4 mark for the cache-hit button: ten cells filled, the rest outlined.
///
/// The count is not decoration — ten of sixteen is the "mostly cached" reading the window exists to
/// make visible, and an all-filled or half-filled square would say nothing. It is drawn at 18 points,
/// so 4×4 is the largest grid that still resolves; a literal 16×16 would be a smudge.
fn draw_grid_mark(p: &egui::Painter, r: Rect, fg: Color32) {
    const N: i32 = 4;
    let gap = 2.0;
    let cell = (r.width() - (N as f32 - 1.0) * gap) / N as f32;
    for y in 0..N {
        for x in 0..N {
            let at = Pos2::new(r.left() + x as f32 * (cell + gap), r.top() + y as f32 * (cell + gap));
            let cr = Rect::from_min_size(at, Vec2::splat(cell));
            if y * N + x < 10 {
                p.rect_filled(cr, CornerRadius::same(1), fg);
            } else {
                p.rect_stroke(
                    cr,
                    CornerRadius::same(1),
                    Stroke::new(1.0, fg.gamma_multiply(0.45)),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
}

/// The font every name in a "name + value" row is set in.
///
/// Shared with [`kv_key_w`] on purpose: the column is measured with the same font it is painted
/// with, and a second literal here would let the two drift apart silently.
fn kv_key_font() -> egui::FontId {
    egui::FontId::proportional(13.5)
}

/// Width of the name column for a **whole** column of rows: the widest name actually drawn.
///
/// A fixed width was the wrong tool. `84` had been measured against the Chinese label ("运行时环境");
/// English names run ~40% wider — "Runtime environment" is ~118 pt — and the name is painted
/// straight onto the painter, which neither clips nor takes part in layout, so an overlong one does
/// not push anything aside: it prints **on top of** the value. That was the whole of "英文排版混乱".
///
/// It has to be the width of the widest name in the column, not per row: a per-row width would let
/// each value start wherever, and a column that lines up is the entire point of these rows. So the
/// caller collects its rows first and measures once. The cost is one layout call per name.
fn kv_key_w(ui: &Ui, keys: &[&str]) -> f32 {
    let mut w = KV_KEY_MIN;
    for k in keys {
        if k.is_empty() {
            continue;
        }
        // The same entry point the drawing code uses, so the measured width is the painted width.
        let g = ui.painter().layout_no_wrap((*k).to_owned(), kv_key_font(), P::text_dim());
        w = w.max(g.size().x + KV_KEY_GAP);
    }
    // Rounded up to a whole point: the values' x would otherwise land on a fraction, and every glyph
    // after the first in a monospace value would be nudged off the pixel grid.
    w.ceil()
}

/// A "name + value" row (the "System" column is all rows like this).
///
/// `key_w` comes from [`kv_key_w`] and must be the same for every row of a column. The colour is a
/// parameter rather than a second function: the runtime rows use it to say "not found", and one
/// entry point is one place to change.
fn kv_row(ui: &mut Ui, key_w: f32, key: &str, val: &str, color: Color32) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(key_w, 19.0), Sense::hover());
        if !key.is_empty() {
            let p = ui.painter();
            let g = p.layout_no_wrap(key.to_string(), kv_key_font(), P::text_dim());
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

/// A "name + clickable link" row (used by the "About" column). `key_w` as in [`kv_row`].
fn link_row(ui: &mut Ui, key_w: f32, key: &str, url: &str) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(key_w, 19.0), Sense::hover());
        let p = ui.painter();
        let g = p.layout_no_wrap(key.to_string(), kv_key_font(), P::text_dim());
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

    /// The readout that makes a long call legible: elapsed over ceiling, plus the model's estimate.
    ///
    /// Truncating rather than rounding is deliberate — a clock that says `1:59` while the kill fires
    /// would be the one number the user is watching, so it must never run ahead of the truth.
    #[test]
    fn the_running_call_reads_elapsed_over_ceiling() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(59.9), "0:59");
        assert_eq!(clock(120.0), "2:00");
        assert_eq!(clock(-3.0), "0:00", "a clock cannot run backwards");
        assert_eq!(span(47.2, 120.0), "0:47 / 2:00");
        assert_eq!(span(5.0, 600.0), "0:05 / 10:00");
    }

    /// What a call row says about the call — the three shapes it can arrive in, and the one case
    /// where it must stay silent.
    #[test]
    fn a_row_says_what_the_call_is_for() {
        use serde_json::json;

        // The model's own words win: the command line cannot say *why*.
        assert_eq!(
            purpose_from_value("edit_file", &json!({"path": "a.rs", "purpose": "fix the 471 s hang"})),
            Some("fix the 471 s hang".into())
        );
        // `bash` has no purpose of its own here, so the command is the label — real, not paraphrased.
        assert_eq!(
            purpose_from_value("bash", &json!({"command": "cargo build --release"})),
            Some("cargo build --release".into())
        );
        // A multi-line script must not turn one row into five.
        assert_eq!(
            purpose_from_value("shell", &json!({"command": "cd a\n  && cargo test\n"})),
            Some("cd a && cargo test".into())
        );
        // A long command keeps its **head** (the program identifies it) and gets one ellipsis.
        let long = purpose_from_value("bash", &json!({"command": "x".repeat(200)})).unwrap();
        assert!(long.ends_with('…'), "{long}");
        assert_eq!(long.chars().count(), PURPOSE_MAX);
        // Nothing to say, and nothing invented.
        assert_eq!(purpose_from_value("read_file", &json!({"path": "a.rs"})), None);
        // Whitespace-only purpose is not a purpose.
        assert_eq!(purpose_from_value("bash", &json!({"purpose": "   ", "command": "dir"})), Some("dir".into()));
    }

    /// Why `step_row` re-registers the file name **after** the row's own click, instead of just
    /// adding a clickable label inside it.
    ///
    /// Two widgets laid over the same rect: egui gives the click to the **last** one registered, which
    /// is the opposite of what "the row's own click wins" (the older comment in `step_row_inner`) is
    /// usually read to mean. Both facts are true at once — the row wins over the labels built *inside*
    /// it because it is registered after them, and it loses to the name because the name is registered
    /// after the row. That ordering is the whole feature ("click a tool call's file name to reveal it,
    /// instead of folding the diff"), and it is invisible in the source: deleting the second
    /// `ui.interact` would leave a file name that looks clickable and folds the row instead.
    ///
    /// A test rather than a manual click, because a click here has to open a real Explorer window.
    #[test]
    fn a_click_goes_to_the_widget_registered_last_on_the_same_rect() {
        use egui::{Event, PointerButton, Sense};

        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_size(egui::pos2(20.0, 20.0), egui::vec2(240.0, 22.0));
        let (row, name) = (egui::Id::new("aox-test-row"), egui::Id::new("aox-test-name"));
        let mut clicks = (false, false);

        // The hit test for clicks uses **last frame's** widget rects, so this needs a couple of idle
        // frames before the press/release, or nothing is registered yet and neither widget is hit.
        let mut frame = |events: Vec<Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 200.0))),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::Area::new(egui::Id::new("aox-test-area")).show(ctx, |ui| {
                    let row_resp = ui.interact(rect, row, Sense::click());
                    // ...and this is the line `step_row_inner` adds, in the same position.
                    let name_resp = ui.interact(rect, name, Sense::click());
                    clicks = (row_resp.clicked(), name_resp.clicked());
                });
            });
        };

        frame(vec![]);
        frame(vec![Event::PointerMoved(rect.center())]);
        frame(vec![Event::PointerButton {
            pos: rect.center(),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        }]);
        frame(vec![Event::PointerButton {
            pos: rect.center(),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        }]);

        assert_eq!(
            clicks,
            (false, true),
            "the widget registered last must own the click; if this flips, `step_row_inner` has to \
             register the file name before the row (and then the row would swallow it instead)"
        );
    }

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

    /// What a workspace name may be, pinned down as a table.
    ///
    /// Worth its own test because this is the **only** place in the GUI that decides what a
    /// filename may contain, and it feeds a path straight into `create_dir_all` under the app root:
    /// a separator or a `..` slipping through would not fail loudly, it would quietly point the
    /// whole workspace (and every write the model makes under it) somewhere else than the dialog's
    /// path preview promised. The refusals are therefore asserted individually rather than by a
    /// generic "some are rejected".
    #[test]
    fn workspace_names_are_stripped_down_to_one_safe_segment() {
        // Accepted, in the form the dialog previews it.
        for (typed, want) in [
            ("notes", "notes"),
            // Surrounding whitespace is trimmed rather than refused: `"notes "` *is* `"notes"` as far
            // as the filesystem is concerned, so the only sensible reading is that the user meant the
            // name they typed.
            ("  notes  ", "notes"),
            ("notes ", "notes"),
            ("My Notes 2", "My Notes 2"),
            ("中文 工作区", "中文 工作区"),
        ] {
            assert_eq!(check_ws_name(typed).as_deref(), Ok(want), "typed {typed:?}");
        }

        for typed in [
            "",
            "   ",
            ".",
            "..",
            ".hidden",        // a leading dot is a hidden directory, not a name we want by accident
            "notes.",         // a trailing dot is dropped by Windows -> one folder, two rows
            "a/b",
            "a\\b",
            "C:notes",
            "a:b",
            "a*b",
            "a?b",
            "a\"b",
            "a<b",
            "a>b",
            "a|b",
            "a\nb",           // control characters would end up literally in the folder name
        ] {
            assert!(check_ws_name(typed).is_err(), "{typed:?} must be refused");
        }
    }

    /// The names the "System" column can put in the name slot — the English source literals, which
    /// are also the keys of `locales/zh.toml`. Adding a row without adding its label here leaves
    /// that label unmeasured (and that is exactly how the bug below got in).
    const SYSTEM_EN: [&str; 11] = [
        "System info",
        "Command line",
        "Runtime environment",
        "Model in use",
        "Config file",
        "Config layers",
        "App root",
        "Workspace",
        "History",
        "Plugins directory",
        "Runtime directory",
    ];
    const SYSTEM_ZH: [&str; 11] = [
        "系统信息",
        "命令行",
        "运行时环境",
        "所用模型",
        "配置文件",
        "配置层",
        "应用根目录",
        "工作区",
        "历史记录",
        "插件目录",
        "运行时目录",
    ];
    const ABOUT_EN: [&str; 2] = ["Author's blog", "Project home"];
    const ABOUT_ZH: [&str; 2] = ["作者博客", "项目地址"];

    /// The name column must be wide enough for **every** name it can show, in **every** language.
    ///
    /// This is "设置-系统界面英文排版混乱" as an executable statement. The column used to be a
    /// constant (`84`) measured against the Chinese labels; English runs ~40% wider ("Runtime
    /// environment" ≈ 118 pt) and the name is painted straight onto the painter — no clipping, no
    /// participation in layout — so the overlong name printed **on top of** its value. The fix was
    /// to measure the widest name in the column (`kv_key_w`); this states the property that fix has
    /// to keep: `column >= widest name + gap`.
    ///
    /// The second assertion is the other half of the contract: the Chinese layout must not move.
    /// `KV_KEY_MIN` exists so that a column of short names still looks deliberate — if this ever
    /// fails, the minimum was changed and the zh dialog now has a gap where there wasn't one.
    #[test]
    fn the_name_column_fits_every_name_it_can_show() {
        let ctx = egui::Context::default();
        // Fonts are per-Context while `theme`'s "bold available" flags are process-global: a Context
        // that lays out text installs its own, or it measures whatever a sibling test left behind.
        crate::theme::install_fonts(&ctx);

        /// Widest of these names, painted the way `kv_row` paints them.
        fn widest(ui: &Ui, keys: &[&str]) -> f32 {
            keys.iter()
                .map(|k| {
                    ui.painter()
                        .layout_no_wrap((*k).to_owned(), kv_key_font(), P::text_dim())
                        .size()
                        .x
                })
                .fold(0.0f32, f32::max)
        }

        let cases: [(&str, &[&str], &[&str]); 2] =
            [("System", &SYSTEM_EN, &SYSTEM_ZH), ("About", &ABOUT_EN, &ABOUT_ZH)];
        // (column, key_w for English, key_w for Chinese, widest English name, widest Chinese name)
        let mut measured: Vec<(&str, f32, f32, f32, f32)> = Vec::new();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                for (col, en, zh) in cases {
                    measured.push((
                        col,
                        kv_key_w(ui, en),
                        kv_key_w(ui, zh),
                        widest(ui, en),
                        widest(ui, zh),
                    ));
                }
            });
        });

        for (col, key_en, key_zh, widest_en, widest_zh) in measured {
            println!(
                "{col}: en column={key_en:.1} (widest {widest_en:.1}) · \
                 zh column={key_zh:.1} (widest {widest_zh:.1})"
            );
            assert!(
                key_en + 0.01 >= widest_en + KV_KEY_GAP,
                "{col}: the column is {key_en:.1} pt but the widest English name needs \
                 {:.1} — it will print over its value",
                widest_en + KV_KEY_GAP
            );
            assert!(
                key_zh + 0.01 >= widest_zh + KV_KEY_GAP,
                "{col}: the column is {key_zh:.1} pt but the widest Chinese name needs {:.1}",
                widest_zh + KV_KEY_GAP
            );
            // Chinese is the language the old constant was tuned for. It may settle a hair above
            // the floor — 67.5 + the 18 pt gap is 85.5, so it lands on 86 rather than 84 — but it
            // must not *jump*: the floor is there precisely so the zh dialog keeps the column it
            // has always had, and 2 pt is invisible while 60 would not be.
            assert!(
                key_zh <= KV_KEY_MIN + 4.0,
                "{col}: the Chinese column jumped from {KV_KEY_MIN} to {key_zh:.1}"
            );
            // The values are paths; a name column that eats the panel would wrap them for nothing.
            assert!(
                key_en <= SETTINGS_RIGHT_W - 240.0,
                "{col}: a {key_en:.1} pt name column leaves too little room for values \
                 (panel {SETTINGS_RIGHT_W:.0} pt)"
            );
        }
    }
}
