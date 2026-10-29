//! GUI ↔ Agent bridge layer.
//!
//! Design point: **fully reuse the existing architecture**.
//! - A background thread runs a tokio runtime, driving the task with `aox::agent::Agent`'s loop unchanged;
//! - `GuiUi` implements the `aox::ui::Ui` trait, forwarding `AgentEvent`, status and the streaming
//!   reasoning process to the egui main thread;
//! - user input, authorization decisions and abort signals travel back through channels.
//!
//! As a result the CLI and GUI share one Agent / Provider / plugin-host implementation, with no logic fork.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use aox::agent::Agent;
use aox::history::UsageRecord;
use aox::llm::{StreamSink, Usage};
use aox::runtime::Runtime;
use aox::ui::{AgentEvent, AuthDecision, ChoiceReply, Ui};

/// Main thread → Agent thread
pub enum ToAgent {
    /// One user instruction plus the attachments it carries (local paths).
    ///
    /// Attachments only travel with **this one** message (one-shot): if the model needs that file in
    /// a later turn, it reads it itself (via `read_file`) following the path referenced in the
    /// message — keeping the attachment bytes in the context would cost tokens for it every turn.
    Input(String, Vec<PathBuf>),
    /// Switch to some past conversation (`id` is the file-name stem).
    ///
    /// Switching "replaces the context in place", so it must happen back on the Agent's thread; the
    /// main thread cannot read the file itself and hand it over.
    Resume(String),
    /// Start a new conversation. The old one needs no wrap-up — it was written to `history/` as it went.
    NewConversation,
    /// This turn's activity-stream records, appended to the same conversation file (`kind:"act"` lines).
    ///
    /// This goes through the queue rather than letting the main thread open the file itself: the
    /// handle is in the Agent thread's hands, and "new conversation" and "abort rollback" also happen
    /// on that thread — the queue naturally preserves write order.
    Act(Vec<Value>),
    /// Rename a conversation (the history panel's conversation menu).
    ///
    /// Goes through the queue for the same reason `Act` does: the file handle for the conversation
    /// being chatted in is in this thread's hands, and the row we append to ends up in that same file.
    /// `title: None` = "show the automatic title again".
    RenameConversation { id: String, title: Option<String> },
    /// Workplace: switch tier (the button at the bottom-left of the input box, see `gui/src/workplace_ui.rs`).
    ///
    /// Like `Input`, this can only be done **back on the Agent's thread** — `Workplace` lives inside
    /// `Runtime`, and only this thread can reach it; the UI cannot call `set_mode` itself and can only
    /// send the request over.
    ///
    /// It carries only "which tier to switch to": the UI need not ask for the current state, because on
    /// startup / at the end of every turn / after every tier switch the bridge proactively pushes a
    /// [`FromAgent::Workplace`].
    Workplace(aox::workplace::Mode),
    /// The recovery panel: list points, preview a rollback, apply one, redo, verify, prune.
    ///
    /// Every variant is answered on **this** thread ([`recovery_request`]): the store lives inside
    /// the workspace and the recovery API is synchronous, so the UI must never run it — a 100 MB
    /// tree walk on the main thread is a frozen window, not a slow one.
    Recovery(RecoveryReq),
    /// Move these paths to the system recycle bin and report the total.
    ///
    /// The main thread cannot do this itself: `recycle` is async and can block for a while on the
    /// shell, and the "N files moved to the recycle bin" notice is an [`AgentEvent`] — routing it
    /// through the **same event** the model's own file deletion produces means both deletions look
    /// identical to the user (one toast, same wording), instead of the GUI growing a second, subtly
    /// different notion of "delete".
    Recycle(Vec<PathBuf>),
    /// The Model tab / the input-box switcher: put this configuration into force.
    ///
    /// Handled **on this thread**, for two reasons. The provider lives in `Runtime`, and only the agent
    /// thread owns it — the UI physically cannot swap it. And the write to `config.toml` has to agree
    /// with what is in memory: doing it here keeps "construct → apply → save" in one place and in the
    /// right order, instead of the main thread writing a file the host may reject.
    ApplyProvider(Box<ProviderEdit>),
    /// Ask the endpoint which models it serves (`GET …/models`). Answered with [`FromAgent::Models`].
    ///
    /// It runs here rather than on the UI thread because it is a network call with a timeout, and the
    /// UI thread draws the window: a slow endpoint there is a frozen window, not a slow panel.
    ScanModels { base_url: String, api_key: Option<String> },
    /// "Test connection": one tiny chat request to this endpoint + key + model (see
    /// `aox::llm::models::probe`). Answered with [`FromAgent::TestResult`].
    ///
    /// A different question from a scan, which is why it is a different message: a key can list models
    /// and still fail to chat, and a model name can be wrong in a way no list reveals.
    TestModel { base_url: String, api_key: Option<String>, model: String },
    /// Exit: close the plugin child processes and end the driver thread
    Shutdown,
}

/// What the Model tab can change, as it arrives from the UI.
///
/// A struct of plain values rather than "the new `ProviderConfig`": the UI has no business knowing
/// about `stream`, `api_key_env`, or which of the two key sources wins. It says what the person typed;
/// the bridge merges that onto what is already in force (see [`edit_to_config`]).
#[derive(Clone, Debug)]
pub struct ProviderEdit {
    pub base_url: String,
    pub model: String,
    /// `Some` = replace the stored key. `None` = **leave it alone** — the panel never displays the
    /// stored key, so an empty field means "unchanged", not "delete it".
    pub api_key: Option<String>,
    pub context_window: u32,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub thinking: aox::config::Thinking,
    pub thinking_wire: aox::config::ThinkingWire,
    /// Request by SSE streaming. Per model, because it is a property of the endpoint.
    pub stream: bool,
}

/// Everything a front end shows about the model in force, pushed after every change.
#[derive(Clone, Debug)]
pub struct ProviderView {
    pub base_url: String,
    pub model: String,
    pub context_window: u32,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub thinking: aox::config::Thinking,
    pub ready: bool,
    /// Why the running model is not usable, when it is not — the one line the settings page shows at
    /// the bottom when `ready` is false.
    pub error: Option<String>,
}

/// The answer to [`ToAgent::ScanModels`].
#[derive(Clone, Debug)]
pub struct ModelsView {
    pub base_url: String,
    /// `(model id, window when the endpoint reported one)`.
    pub entries: Vec<(String, Option<u32>)>,
    /// The URL that answered — remembered so the next scan asks once.
    pub url: Option<String>,
    /// A scan that failed says why here; `entries` is then empty. Failure is **not** an error state for
    /// the app: the model field stays hand-editable, which is the escape hatch for an endpoint that
    /// cannot list at all.
    pub error: Option<String>,
}

/// Move every path to the system recycle bin, reporting the total through `AgentEvent::Recycled`.
///
/// A failure is reported per path rather than swallowed: "I deleted three and only two went" is
/// exactly the case where silence is worst, because the user believes the file is safely recoverable.
async fn recycle_all(paths: Vec<PathBuf>, tx: &mpsc::UnboundedSender<FromAgent>) {
    let mut moved = 0usize;
    for p in paths {
        match aox::recycle::recycle(&p).await {
            Ok(()) => moved += 1,
            Err(e) => {
                let _ = tx.send(FromAgent::Event(AgentEvent::Warn(aox::tf!(
                    "Could not move {} to the recycle bin: {}",
                    p.display(),
                    format!("{e:#}")
                ))));
            }
        }
    }
    if moved > 0 {
        let _ = tx.send(FromAgent::Event(AgentEvent::Recycled(moved)));
    }
}

/// Agent thread → main thread
pub enum FromAgent {
    /// Startup info (config / model / paths / whether usable)
    Boot(Box<BootInfo>),
    /// A model request is **starting**: `n == 0` is the first attempt, `n >= 1` is a retry after a
    /// failure, and `delay_ms` is how long the loop waited before it (see
    /// [`aox::llm::StreamSink::on_attempt`]).
    ///
    /// A retry needs its own event rather than a `Warn`, because the UI has to *undo* work: a stream
    /// that was cut has already drawn half an answer, and the retyped one must replace it, not
    /// appear beneath it.
    Attempt { n: u32, delay_ms: u64 },
    /// The core event stream (fully shared with the CLI)
    Event(AgentEvent),
    /// A turn starts
    TurnStart,
    /// A turn ends (normally)
    TurnEnd,
    /// This turn's token ledger — exactly the line written into the conversation file (sent after `TurnEnd`)
    Usage(Box<UsageRecord>),
    /// The usage ledger for the whole conversation (resent after switching / creating a conversation).
    ///
    /// **Must be sent after `Switched`**: on receiving `Switched` the main thread clears the ledger;
    /// if the order were reversed, the just-restored data would be wiped by itself.
    UsageLog(Vec<UsageRecord>),
    /// What the request about to be sent is made of: sections in body order, each with a rough token
    /// estimate. Sent before every model call (see `Ui::context_map`), and used by the cache-hit
    /// window to attribute blocks to the sections they came from.
    ContextMap(Vec<(&'static str, u64)>),
    /// One model call finished **inside** the current turn (see `Ui::call_usage`).
    ///
    /// Distinct from [`FromAgent::Usage`], which carries the turn's **aggregate** and only arrives
    /// after the turn is over. This one is what lets the cache-hit map move while the model is still
    /// working: `UsageRecord::usage` is a sum over calls, so a mid-turn update cannot be recovered
    /// from it — the last call's figures have to be sent on their own.
    CallUsage { usage: Usage, last_prompt: Option<u64> },
    /// A turn was aborted
    TurnAborted,
    /// Status-bar text (thinking / executing xx)
    Status(String),
    /// Streaming reasoning delta
    Reasoning(String),
    /// The reasoning stream of one response is over (it disappears immediately)
    ReasoningEnd,
    /// Streaming content delta. The content itself goes through `Event(AssistantText)`; this one is
    /// **only for live token counting** — the main thread needs to know "how many more characters just
    /// came out this instant" for the number to keep climbing.
    Content(String),
    /// A fragment of a tool call the model is **still writing** (its arguments are arriving in
    /// pieces) — see `StreamSink::on_tool_call`.
    ///
    /// This is the one signal that arrives *before* a tool runs, and it is what lets the main thread
    /// put the call's row on screen and keep a live reading on it while the model writes, instead of
    /// showing nothing until the whole call has come back. `index` identifies the call within the
    /// response; `name` is filled in only on the fragment that first carries it.
    ToolDelta { index: u64, name: String, delta: String },
    /// Request authorization (blocks waiting for the main thread to fill in)
    AuthRequest { tool: String, args: String, reply: oneshot::Sender<AuthDecision> },
    /// A "multiple-choice question" (`ask_user`): likewise blocks waiting for the main thread to fill in.
    ///
    /// It is a separate variant from `AuthRequest` rather than merged into one: their **UI shapes** and
    /// **answer types** differ (authorization is an "allow/deny" confirmation box, a choice is a row of
    /// "pick one" buttons), and merging would force both sides to decide "which kind is this" from the
    /// parameters — which is exactly where mistakes are most likely.
    ChoiceRequest {
        question: String,
        options: Vec<String>,
        reply: oneshot::Sender<ChoiceReply>,
    },
    /// Tool / plugin snapshot (refreshed periodically, for the right panel to show "self-growth" live)
    Snapshot(Snapshot),
    /// Plugin runtime status (for the "About" window); resent once assembly completes
    Runtimes(RuntimeView),
    /// The id of the conversation file being written this session (`None` before a conversation starts).
    ///
    /// The main thread uses it to mark "current" in the history list and to rescan the directory when
    /// needed — the history files themselves are read from disk by the main thread directly, not
    /// through this channel (see `aox::history`).
    HistoryCurrent(Option<String>),
    /// The session was switched (opened a history conversation / created a new one).
    ///
    /// `opened` is the conversation id after switching (`None` when creating a new one); a non-empty
    /// `error` means loading failed — in that case the Agent's context **was not touched** and the main
    /// thread should change nothing.
    ///
    /// It carries only the id, not the messages: the loaded content is re-read from disk by the main
    /// thread itself (see the note above).
    Switched { opened: Option<String>, error: Option<String> },
    /// The current workplace (tier + the root in effect).
    ///
    /// Sent as a receipt on startup, at the end of every turn, and after any tier switch: the UI needs
    /// it for two things — showing the current tier name on the button, and ticking the current tier in
    /// the three-way list. A struct rather than a line of text: ticking requires comparing tiers with
    /// `PartialEq`, and parsing a string like "Picked workspace" is too brittle (a copy change silently breaks
    /// it), for the same reason as [`RuntimeView`].
    Workplace(Box<WorkplaceView>),
    /// An answer for the recovery panel (see [`ToAgent::Recovery`]).
    Recovery(Box<RecoveryView>),
    /// The model in force changed (startup, and after every successful apply).
    ///
    /// Pushed rather than pulled: the ring's denominator, the "Model in use" row and the panel's form
    /// all have to agree, and the authoritative values live on the agent thread. A UI that kept its
    /// own copy would drift the first time an apply failed halfway.
    Provider(Box<ProviderView>),
    /// The answer to a scan (see [`ToAgent::ScanModels`]).
    Models(Box<ModelsView>),
    /// The answer to [`ToAgent::TestModel`]. `model` comes back so a slow reply cannot land in the
    /// wrong dialog (the user may have moved on to another model by then).
    TestResult { base_url: String, model: String, ok: bool, text: String },
    /// One line about a model operation, for the panel's status line.
    ///
    /// Separate from [`AgentEvent`] on purpose: an apply failure is not a conversation event, and
    /// putting it in the activity stream would file it under "things the model did".
    ModelNotice { ok: bool, text: String },
    /// An unrecoverable error occurred this turn
    Fatal(String),
}

/// The current workplace (see [`FromAgent::Workplace`]).
#[derive(Debug, Clone)]
pub struct WorkplaceView {
    pub mode: aox::workplace::Mode,
    /// The currently effective root. `None` in the `full` tier, and in the `default` tier when the
    /// **ground has not been broken yet** — the latter is not "unrestricted" but "this conversation has
    /// not begun"; the UI must not present the two as the same thing.
    pub root: Option<PathBuf>,
}

/// Send the current workplace state to the UI.
///
/// What the recovery panel asks the agent thread to do.
#[derive(Clone, Debug)]
pub enum RecoveryReq {
    /// The list, plus how far back it can go. Sent when the panel opens and after every change.
    List,
    /// Resolve one point and describe what rolling back to it would do. Read-only.
    Preview { target: String },
    /// Apply it. `force` overwrites files changed outside AOX; `allow_partial` crosses points with
    /// incomplete coverage. They are separate consents because they are separate questions.
    Rollback { target: String, force: bool, allow_partial: bool },
    Redo,
    Verify,
    Prune,
    /// Drop every point in this workspace (into the recycle bin).
    Clear,
}

/// What comes back.
#[derive(Clone, Debug)]
pub enum RecoveryView {
    Points(Box<aox::recovery::view::PointsView>),
    Preview(Box<aox::recovery::view::PreviewView>),
    /// Something to say inside the panel: the outcome of a rollback / verify / prune, or a refusal.
    /// `ok == false` is shown in the danger colour and **is not a modal** — the panel is already
    /// the place the user is looking.
    Notice { text: String, ok: bool },
}

/// One lock acquisition takes both tier and root ([`aox::workplace::Workplace::snapshot`]), reading the
/// same state as the CLI's `/workplace` — the two frontends do not each keep their own "what tier is it
/// now" judgment.
fn sync_workplace(tx: &mpsc::UnboundedSender<FromAgent>, rt: &Runtime) {
    let (mode, root) = rt.workplace.snapshot();
    let _ = tx.send(FromAgent::Workplace(Box::new(WorkplaceView { mode, root })));
}

/// Handle a workplace request from the UI (the button at the bottom-left of the input box, see `gui/src/workplace_ui.rs`).
///
/// It is the **same implementation** as the CLI's `/workplace` ([`aox::workplace::Workplace::set_mode`]);
/// here it only decides "where the result goes": a successful/failed switch goes into the activity
/// stream (feedback the user can see), and afterwards a state receipt is sent regardless of outcome —
/// so the tier name on the button and the tick in the three-way list catch up immediately, without
/// waiting for the next turn to end.
fn set_workplace(ui: &mut GuiUi, rt: &Arc<Runtime>, mode: aox::workplace::Mode) {
    // Take the name first: `set_mode` consumes `mode`, and the error text needs it.
    let label = mode.label();
    match rt.workplace.set_mode(mode, &rt.root) {
        Ok(msg) => ui.emit(AgentEvent::Info(msg)),
        Err(e) => ui.emit(AgentEvent::Error(aox::tf!("failed to switch workplace ({}): {}", label, format!("{e:#}")))),
    }
    sync_workplace(&ui.tx, rt);
}

pub struct BootInfo {
    pub config_path: String,
    /// The layers used to compose this config (low → high, later ones override earlier ones).
    ///
    /// Only used to show one line in the "About" window; the full source tree goes through the CLI's
    /// `--dump-config` — that table does not fit in a 380px card, and squeezing it in would just be
    /// noise nobody reads.
    pub config_layers: Vec<String>,
    pub model: String,
    /// The model's context window, for the ring in the input box. `0` = not configured.
    pub context_window: u64,
    pub provider_ready: bool,
    /// Whether task recovery is on (`[recovery] enabled`). The panel's chip is **not drawn** when it
    /// is off: a control for a feature that does not exist is a lie with a hover state.
    pub recovery: bool,
    /// This launch **created** `config.toml` (there was none before).
    ///
    /// Only then does the window open on Settings → Model by itself. Not "whenever the model is not
    /// ready": someone whose key expired would get a dialog in the face on every launch, which is the
    /// kind of help that becomes an obstacle.
    pub first_run: bool,
    pub note: Option<String>,
    /// The paths and host info the "System" tab needs (see [`BootPaths`]).
    pub paths: BootPaths,
    /// The storage instance the UI side reads history with (read-only use).
    ///
    /// The **same backend implementation** as the agent thread, pointing at the same data — the UI does
    /// not build paths itself, nor decide the persistence format (see `aox::store`). On a normal start
    /// it is a clone of `rt.store`; when the config cannot be read, the default backend is a fallback, so
    /// at least old conversations in the sidebar remain visible.
    pub store: Arc<dyn aox::store::Store>,
}

/// The paths and host info in the "System" tab.
///
/// Why one struct rather than adding seven or eight more `String` fields to `BootInfo`: these values
/// live and die together (all come from the same `Runtime`), and **more will be added** (e.g. a log
/// directory). Bundling them keeps `boot()`'s signature from growing with "one more displayed row", and
/// the three call sites need change in only one construction.
#[derive(Debug, Clone, Default)]
pub struct BootPaths {
    /// Application root directory (config / plugins / history / runtime all hang off it).
    pub root: String,
    /// The config file (the last layer of the composition chain).
    pub config: String,
    /// The plugins directory.
    pub plugins: String,
    /// The history directory (`<root>/history`, reported by the storage backend itself; the UI doesn't build it).
    pub history: String,
    /// The bundled runtime directory (`<root>/runtime`) — it exists even when no bundled runtime is installed, just empty.
    pub runtime: String,
    /// Host info, e.g. `windows x86_64 · 16 cores · 31.7 GB RAM`.
    pub host: String,
    /// Which command-line tool the model gets first, e.g. `bash · git-bash — C://Program Files\Git\bin\bash.exe`.
    ///
    /// Computed by the core (`aox::shell::command_line_summary`) so the row cannot drift from what is
    /// actually registered: the sentence is a claim about behaviour, not decoration.
    pub command_line: String,
}

impl BootPaths {
    /// Assemble one from several paths (host info asks the core library itself).
    ///
    /// All three startup branches go through it — including the two degraded branches "config cannot be
    /// read" and "model not ready": those branches cannot get a full `Runtime`, but **the paths are
    /// known**, and "where the config file actually is" is exactly what should be shown then (the user
    /// came to find out why it did not load).
    pub fn new(
        root: &std::path::Path,
        config: &std::path::Path,
        plugins: &std::path::Path,
        history: std::path::PathBuf,
        shell: &aox::config::ShellConfig,
    ) -> Self {
        Self {
            root: root.display().to_string(),
            config: config.display().to_string(),
            plugins: plugins.display().to_string(),
            history: history.display().to_string(),
            runtime: root.join("runtime").display().to_string(),
            host: aox::sysinfo::host_summary(),
            command_line: aox::shell::command_line_summary(shell, root),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub plugins: Vec<String>,
}

/// The status of one plugin runtime language (a row in the "System" tab).
///
/// Structured rather than a pre-joined string: the CLI wants one compact line (`runtimes::one_line`),
/// while the settings page wants three columns "language / resolved command / origin" — the same data
/// laid out two ways. Joining into a string and splitting it back is asking for trouble, and this line
/// is meant for human eyes anyway, so it cannot be split.
#[derive(Debug, Clone, Default)]
pub struct RuntimeItem {
    pub lang: String,
    /// The command finally used (absolute path or bare command)
    pub command: String,
    /// Origin: `specified in config.toml` / `bundled in app dir` / `PATH` / `not found`
    pub origin: String,
    pub ok: bool,
}

/// The runtime status view.
///
/// Deliberately a struct rather than a few lines of string: the UI decides "whether to show an install
/// button" from it, and judging by parsing text like `✗ python` is too brittle (a copy change silently
/// breaks it).
#[derive(Debug, Clone, Default)]
pub struct RuntimeView {
    /// One row per language (structured, see [`RuntimeItem`]).
    pub items: Vec<RuntimeItem>,
    /// Whether Python is missing — decides whether the settings page shows the one-click install entry
    pub python_missing: bool,
}

/// A cross-thread abort flag. Polling rather than `Notify`, to avoid the stale-permit problem of "notification before wait".
pub struct SessionFlags {
    abort: AtomicBool,
}

impl SessionFlags {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { abort: AtomicBool::new(false) })
    }

    pub fn request_abort(&self) {
        self.abort.store(true, Ordering::SeqCst);
    }

    fn clear(&self) {
        self.abort.store(false, Ordering::SeqCst);
    }
}

async fn wait_abort(flags: Arc<SessionFlags>) {
    loop {
        if flags.abort.swap(false, Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Forward the Provider's streaming reasoning to the GUI.
struct ChanSink {
    tx: mpsc::UnboundedSender<FromAgent>,
}

impl StreamSink for ChanSink {
    fn on_reasoning(&self, delta: &str) {
        let _ = self.tx.send(FromAgent::Reasoning(delta.to_string()));
    }

    fn on_content(&self, delta: &str) {
        let _ = self.tx.send(FromAgent::Content(delta.to_string()));
    }

    fn on_tool_call(&self, index: u64, name: &str, delta: &str) {
        let _ = self.tx.send(FromAgent::ToolDelta {
            index,
            name: name.to_string(),
            delta: delta.to_string(),
        });
    }

    fn on_finish(&self) {
        let _ = self.tx.send(FromAgent::ReasoningEnd);
    }

    fn on_attempt(&self, attempt: u32, delay: std::time::Duration) {
        let _ = self.tx.send(FromAgent::Attempt { n: attempt, delay_ms: delay.as_millis() as u64 });
    }
}

/// The graphical implementation of `aox::ui::Ui`.
pub struct GuiUi {
    tx: mpsc::UnboundedSender<FromAgent>,
    rx: mpsc::UnboundedReceiver<ToAgent>,
}

#[async_trait]
impl Ui for GuiUi {
    /// The GUI's instruction path is `next_command` (which also handles "switch session" in place), so
    /// this is in fact never called; the implementation is kept only to satisfy the trait.
    async fn read_user_input(&mut self) -> Result<Option<String>> {
        loop {
            match self.rx.recv().await {
                Some(ToAgent::Input(s, files)) => {
                    // The GUI's real instruction path is `next_command` (see its docs); this trait
                    // implementation exists only to satisfy the signature. If execution really reaches
                    // here, **speak up** — silently dropping attachments would one day, when this path
                    // comes alive, yield a ghost bug of "the image was clearly dragged but not sent".
                    if !files.is_empty() {
                        let _ = self.tx.send(FromAgent::Event(AgentEvent::Warn(aox::tf!(
                            "internal error: this path does not support attachments; {} attachment(s) were not sent with the message.",
                            files.len()
                        ))));
                    }
                    return Ok(Some(s));
                }
                Some(ToAgent::Shutdown) | None => return Ok(None),
                Some(ToAgent::Resume(_))
                | Some(ToAgent::NewConversation)
                | Some(ToAgent::Act(_))
                | Some(ToAgent::RenameConversation { .. })
                | Some(ToAgent::Recycle(_))
                | Some(ToAgent::Workplace(_))
                | Some(ToAgent::Recovery(_))
                // The model seam is handled by `next_command` / `idle_until_shutdown`, never here; a
                // message arriving in this inert stub is simply left to them.
                | Some(ToAgent::ApplyProvider(_))
                | Some(ToAgent::ScanModels { .. })
                | Some(ToAgent::TestModel { .. }) => continue,
            }
        }
    }

    fn emit(&mut self, event: AgentEvent) {
        let _ = self.tx.send(FromAgent::Event(event));
    }

    async fn authorize(&mut self, tool_name: &str, args: &Value) -> AuthDecision {
        let (reply, rx) = oneshot::channel();
        let pretty = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
        let sent = self.tx.send(FromAgent::AuthRequest {
            tool: tool_name.to_string(),
            args: pretty,
            reply,
        });
        if sent.is_err() {
            return AuthDecision::Deny;
        }
        rx.await.unwrap_or(AuthDecision::Deny)
    }

    /// The graphical version of `ask_user`: send the question and options to the main thread, draw them
    /// as the row of buttons above the input box, and wait to be filled in.
    ///
    /// All three failure paths (cannot send / main thread did not reply / reply channel broken) return
    /// `Cancelled`: that is far safer than "default to the first option" — the former merely leaves it
    /// unanswered and the model proceeds in its own words and explains, whereas the latter **makes a
    /// choice for the user that they never made**, and nobody would notice.
    async fn choose(&mut self, question: &str, options: &[String]) -> ChoiceReply {
        let (reply, rx) = oneshot::channel();
        let sent = self.tx.send(FromAgent::ChoiceRequest {
            question: question.to_string(),
            options: options.to_vec(),
            reply,
        });
        if sent.is_err() {
            return ChoiceReply::Cancelled;
        }
        rx.await.unwrap_or(ChoiceReply::Cancelled)
    }

    async fn turn_start(&mut self) {
        let _ = self.tx.send(FromAgent::TurnStart);
    }

    async fn turn_end(&mut self) {
        let _ = self.tx.send(FromAgent::TurnEnd);
    }

    async fn turn_usage(&mut self, rec: &UsageRecord) {
        let _ = self.tx.send(FromAgent::Usage(Box::new(rec.clone())));
    }

    async fn context_map(&mut self, segs: &[(&'static str, u64)]) {
        // A snapshot, not a borrow: the caller is mid-turn and will move on the moment this returns.
        let _ = self.tx.send(FromAgent::ContextMap(segs.to_vec()));
    }

    async fn call_usage(&mut self, usage: &Usage, last_prompt: Option<u64>) {
        let _ = self.tx.send(FromAgent::CallUsage { usage: usage.clone(), last_prompt });
    }

    async fn set_status(&mut self, label: &str) {
        let _ = self.tx.send(FromAgent::Status(label.to_string()));
    }

    fn stream_sink(&self) -> Option<Box<dyn StreamSink + Send + Sync + '_>> {
        Some(Box::new(ChanSink { tx: self.tx.clone() }))
    }
}

/// Send the runtime status to the "About" window.
///
/// Pass the **raw** `config.runtimes` (not a resolved snapshot), otherwise the "origin" would be
/// misjudged as "specified in config.toml" — see the notes in `aox::runtimes::status`.
fn send_runtime_status(tx: &mpsc::UnboundedSender<FromAgent>, rt: &Runtime) {
    let st = aox::runtimes::status(&rt.root, &rt.config.runtimes);
    let view = RuntimeView {
        items: st
            .iter()
            .map(|s| RuntimeItem {
                lang: s.lang.key().to_string(),
                command: s.command.clone(),
                origin: s.origin.label().to_string(),
                ok: s.ok(),
            })
            .collect(),
        python_missing: st
            .iter()
            .any(|s| s.lang == aox::runtimes::Lang::Python && !s.ok()),
    };
    let _ = tx.send(FromAgent::Runtimes(view));
}

/// The GUI implementation of the recovery commands. Parsing and the actual work are shared with
/// the CLI; the GUI only routes the report to its event stream and tells the Agent to invalidate
/// stale task evidence after a successful restore.
fn recovery_command(ui: &mut GuiUi, agent: &mut Agent, line: &str) {
    let rt = agent.rt().clone();
    let parsed = match aox::recovery::command::parse(line) {
        Ok(c) => c,
        Err(e) => {
            ui.emit(AgentEvent::Error(e));
            return;
        }
    };
    match aox::recovery::command::run(&rt, parsed) {
        Ok(rep) => {
            ui.emit(AgentEvent::Info(rep.text));
            if let Some(report) = rep.restore {
                agent.note_workspace_restored(&report);
            }
        }
        Err(e) => ui.emit(AgentEvent::Error(e)),
    }
}

/// The recovery panel's backend: one request in, one view out.
///
/// The store work itself is [`aox::recovery::command`] — the same functions the CLI's `/checkpoints`,
/// `/rollback` and `/backup` call. The GUI deliberately owns **no** recovery logic: it decides what
/// it looks like, never what it means.
fn recovery_request(agent: &mut Agent, tx: &mpsc::UnboundedSender<FromAgent>, req: RecoveryReq) {
    use aox::recovery::command as cmd;
    use aox::recovery::view;
    let rt = agent.rt().clone();
    let view = match req {
        // `history_id()` is the conversation whose turn is running (or the one just opened): it is what
        // makes "this point is mine" answerable. Without it every row would look foreign in a new
        // conversation, and a row that belongs to someone else would look like the user's own work.
        RecoveryReq::List => match view::points_view(&rt, agent.history_id()) {
            Ok(v) => RecoveryView::Points(Box::new(v)),
            Err(e) => RecoveryView::Notice { text: e, ok: false },
        },
        RecoveryReq::Preview { target } => match view::preview_view(&rt, &target) {
            Ok(v) => RecoveryView::Preview(Box::new(v)),
            Err(e) => RecoveryView::Notice { text: e, ok: false },
        },
        RecoveryReq::Rollback { target, force, allow_partial } => {
            match view::rollback_to(&rt, &target, force, allow_partial) {
                Ok(rep) => {
                    // Same bookkeeping as `/rollback` typed into the input box: the task checkpoint's
                    // evidence is stale after a restore, and the transcript gets a host note. Routing
                    // both doors through this one call is what keeps them from diverging.
                    if let Some(report) = &rep.restore {
                        agent.note_workspace_restored(report);
                    }
                    RecoveryView::Notice { text: rep.text, ok: true }
                }
                Err(e) => RecoveryView::Notice { text: e, ok: false },
            }
        }
        RecoveryReq::Redo => notice_from(cmd::run(&rt, cmd::RecoveryCmd::Redo)),
        RecoveryReq::Verify => notice_from(cmd::run(&rt, cmd::RecoveryCmd::Verify)),
        RecoveryReq::Prune => notice_from(cmd::run(&rt, cmd::RecoveryCmd::Prune)),
        RecoveryReq::Clear => notice_from(cmd::run(&rt, cmd::RecoveryCmd::Clear)),
    };
    // A preview never changes the list; everything else might, so its answer is followed by a fresh
    // one. Decided **before** the view is moved into the channel.
    let refresh = !matches!(view, RecoveryView::Preview(_));
    let _ = tx.send(FromAgent::Recovery(Box::new(view)));
    if refresh {
        if let Ok(v) = view::points_view(&rt, agent.history_id()) {
            let _ = tx.send(FromAgent::Recovery(Box::new(RecoveryView::Points(Box::new(v)))));
        }
    }
}

fn notice_from(result: Result<aox::recovery::command::CommandReport, String>) -> RecoveryView {
    match result {
        Ok(rep) => RecoveryView::Notice { text: rep.text, ok: true },
        Err(e) => RecoveryView::Notice { text: e, ok: false },
    }
}

/// The GUI implementation of `/runtime`.
///
/// Parsing, provisioning and copy all reuse `aox::runtimes`, shared with the CLI; here it only decides
/// "where the output goes": progress and results go into the activity stream, so a 10 MB download does
/// not make the UI look frozen.
async fn runtime_command(ui: &mut GuiUi, rt: &Arc<Runtime>, line: &str) {
    let parsed = match aox::runtimes::parse_cmd(line) {
        Ok(c) => c,
        Err(e) => {
            ui.emit(AgentEvent::Error(format!("{e:#}")));
            return;
        }
    };
    match parsed {
        aox::runtimes::Cmd::Status => {
            ui.emit(AgentEvent::Info(aox::runtimes::status_report(
                &rt.root,
                &rt.config.runtimes,
            )));
        }
        aox::runtimes::Cmd::Dir => {
            ui.emit(AgentEvent::Info(rt.root.join("runtime").display().to_string()));
        }
        aox::runtimes::Cmd::Install(lang) => {
            let tx = ui.tx.clone();
            let mut sink = |m: &str| {
                let _ = tx.send(FromAgent::Event(AgentEvent::Info(m.to_string())));
            };
            match aox::runtimes::install(rt, lang, &mut sink).await {
                Ok(msg) => {
                    ui.emit(AgentEvent::Info(msg));
                    send_runtime_status(&ui.tx, rt);
                }
                Err(e) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
            }
        }
    }
}

/// The configured context window, or `0` for "the config didn't say".
///
/// A tiny wrapper so the two call sites that have a `Runtime` in hand don't each repeat the
/// `Option → 0` dance; the GUI turns `0` into its own default (see `DEFAULT_CONTEXT_WINDOW`).
fn ctx_window(rt: &Arc<Runtime>) -> u64 {
    // Read from the **live** provider, not from the config snapshot taken at startup: after a model
    // switch the ring's denominator has to follow, and this is the one place that answers "how big is
    // the window right now".
    u64::from(rt.provider.info().context_window)
}

// ─────────────────────────── the model seam ───────────────────────────

/// Push the model in force to the UI. One call site for "something about the model changed".
fn push_provider(tx: &mpsc::UnboundedSender<FromAgent>, rt: &Runtime) {
    let i = rt.provider.info();
    let _ = tx.send(FromAgent::Provider(Box::new(ProviderView {
        base_url: i.base_url,
        model: i.model,
        context_window: i.context_window,
        max_tokens: i.max_tokens,
        temperature: i.temperature,
        thinking: i.thinking,
        ready: i.ready,
        error: i.error,
    })));
}

/// Merge what the panel typed onto what is already in force.
///
/// The fields the UI cannot see are **carried over** rather than defaulted: `stream` is a config-only
/// preference, and `api_key_env` must survive as long as no new key was typed (an install that keeps
/// its key in the environment must not lose that by pressing "apply" on the model name).
fn edit_to_config(edit: &ProviderEdit, prev: &aox::config::ProviderConfig) -> aox::config::ProviderConfig {
    let mut cfg = prev.clone();
    cfg.base_url = edit.base_url.trim().to_string();
    cfg.model = edit.model.trim().to_string();
    if let Some(k) = edit.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        cfg.api_key = Some(k.to_string());
        // The inline key wins in `resolve_key`, so leaving the variable in place would put a line in
        // `config.toml` that no longer does anything.
        cfg.api_key_env = None;
    }
    cfg.context_window = Some(edit.context_window.max(1));
    cfg.max_tokens = Some(edit.max_tokens.max(1));
    cfg.temperature = edit.temperature;
    cfg.thinking = edit.thinking;
    cfg.thinking_wire = edit.thinking_wire;
    cfg.stream = edit.stream;
    cfg
}

/// Put an edit into force **and** remember it, in that order.
///
/// The order is the contract: a configuration that cannot be built (empty key, unusable URL) must never
/// reach the file, or the next launch starts from a config that cannot run. So: construct → swap →
/// save. If the save fails after a successful swap, the session keeps working and says so — the
/// alternative (roll the live provider back because a file write failed) would be strictly worse.
fn apply_provider(
    rt: &Runtime,
    edit: &ProviderEdit,
    tx: &mpsc::UnboundedSender<FromAgent>,
) {
    let prev = rt.provider.cfg();
    let cfg = edit_to_config(edit, &prev);
    if let Err(e) = rt.provider.apply(cfg.clone()) {
        let _ = tx.send(FromAgent::ModelNotice {
            ok: false,
            text: aox::tf!("Could not use this configuration: {}", format!("{e:#}")),
        });
        return;
    }
    // Applied. The UI hears about the new state first, so the panel is correct even if the disk is not.
    push_provider(tx, rt);
    // Remember it: since 2026-10-28 the book's active record **is** the model configuration, so this
    // replaces the old `config.toml` write. Same contract as before — the swap has already happened,
    // so a failed save costs the memory of the choice and says so, rather than rolling the session back.
    match remember_model(&rt.root, &cfg) {
        Ok(()) => {
            let _ = tx.send(FromAgent::ModelNotice {
                ok: true,
                text: aox::tf!("In effect · next request uses {}", cfg.model),
            });
        }
        Err(e) => {
            let _ = tx.send(FromAgent::ModelNotice {
                ok: false,
                text: aox::tf!(
                    "In effect for this session, but saving the model list failed: {}",
                    format!("{e:#}")
                ),
            });
        }
    }
}

/// Fold a configuration into `.aox/models.json` and mark it in force.
///
/// The GUI's half of `aox::modelbook::Book::remember`: loading and saving live here so the runtime
/// does not have to hold a mutable copy of a file the panel also edits.
fn remember_model(app_root: &std::path::Path, cfg: &aox::config::ProviderConfig) -> anyhow::Result<()> {
    let mut book = aox::modelbook::Book::load(app_root);
    book.remember(cfg);
    book.save(app_root)
}

/// Answer a scan request. Never fails the app: a scan error is a line in the panel, and the model
/// field stays hand-editable — that is the escape hatch for an endpoint that cannot list at all.
/// Answer a "test connection" request.
async fn test_model(
    tx: &mpsc::UnboundedSender<FromAgent>,
    base_url: String,
    api_key: Option<String>,
    model: String,
) {
    let (ok, text) = match aox::llm::models::probe(&base_url, api_key.as_deref(), &model).await {
        Ok(msg) => (true, msg),
        Err(e) => (false, format!("{e:#}")),
    };
    let _ = tx.send(FromAgent::TestResult { base_url, model, ok, text });
}

async fn scan_models(
    tx: &mpsc::UnboundedSender<FromAgent>,
    base_url: String,
    api_key: Option<String>,
) {
    let mut view = ModelsView { base_url, entries: Vec::new(), url: None, error: None };
    match aox::llm::models::list(&view.base_url, api_key.as_deref()).await {
        Ok(scan) => {
            view.url = Some(scan.url);
            view.entries = scan.models.into_iter().map(|m| (m.id, m.window)).collect();
        }
        Err(e) => view.error = Some(format!("{e:#}")),
    }
    let _ = tx.send(FromAgent::Models(Box::new(view)));
}

fn boot(
    tx: &mpsc::UnboundedSender<FromAgent>,
    config_path: &std::path::Path,
    model: &str,
    // The model's context window (`[provider] context_window`), or 0 when unknown -- the ring in
    // the input box needs a denominator, and "unknown" is the honest answer for a config that
    // didn't say.
    context_window: u64,
    ready: bool,
    recovery: bool,
    note: Option<String>,
    store: Arc<dyn aox::store::Store>,
    // Which layers this config was composed from (low → high). An empty list when the config cannot be read — do not invent layers that don't exist.
    layers: Vec<String>,
    paths: BootPaths,
    // A fresh install: `config.toml` was written by this launch (see `BootInfo::first_run`).
    first_run: bool,
) {
    let _ = tx.send(FromAgent::Boot(Box::new(BootInfo {
        config_path: config_path.display().to_string(),
        config_layers: layers,
        model: model.to_string(),
        context_window,
        provider_ready: ready,
        recovery,
        first_run,
        note,
        paths,
        store,
    })));
}

/// The path list all three startup branches need.
///
/// The history directory is asked of the storage backend (`Store::location`) — the UI does not build
/// `<root>/history` itself: after a backend change (the very point of the storage seam existing), a
/// built path would be fake, whereas this follows along.
fn boot_paths(
    root: &std::path::Path,
    config: &std::path::Path,
    plugins: &std::path::Path,
    store: &Arc<dyn aox::store::Store>,
    shell: &aox::config::ShellConfig,
) -> BootPaths {
    BootPaths::new(root, config, plugins, store.location().to_path_buf(), shell)
}

/// Restore persisted plugins and report everything about them the user needs to know.
///
/// Split out of `boot_and_run` for two reasons. Both startup branches need it — the normal one and
/// the "model not configured" one, where the plugin panel should still show what is on disk — and it
/// is the **one startup step that can block**: a plugin whose metadata cache is cold or stale has to
/// be started to ask it its own tool list, and a broken one makes that wait out `INIT_TIMEOUT` (20 s)
/// per plugin. Callers therefore run it **after** the session has been handed to the UI via `boot`,
/// never before: the history sidebar lives off the store that `boot` carries, so anything scheduled
/// ahead of `boot` silently blanks it for as long as it blocks.
async fn restore_plugins(ui: &mut GuiUi, rt: &Arc<Runtime>) {
    // Success stays silent — startup should not flush a screenful of "plugin xxx restored" into the
    // activity stream, pushing the information you really need out of view. Only failure speaks up.
    for (name, result) in aox::plugin::load_all(rt).await {
        if let Err(e) = result {
            ui.emit(AgentEvent::Warn(aox::tf!("plugin {} failed to load: {}", name, format!("{e:#}"))));
        }
    }
    // Plugins without a fence must speak up too (sharing the same judgment and copy as the CLI).
    // A fence failure is not a fatal error, but "thinking there is protection when there is none"
    // deserves a look.
    let unfenced = aox::sandbox::unfenced(rt).await;
    if !unfenced.is_empty() {
        ui.emit(AgentEvent::Warn(aox::tf!(
            "These plugins are not wrapped in a process fence; they are not subject to the process / memory / UI limits: {}",
            unfenced.join(aox::i18n::t("; "))
        )));
    }
}

/// Tell the main thread "which conversation this session is writing".
///
/// Called once after each turn: the first time the id appears, the main thread adds the new
/// conversation to the sidebar; repeated notifications with the same id then tell it "this one's content
/// changed, the count should refresh".
fn send_history_current(tx: &mpsc::UnboundedSender<FromAgent>, agent: &Agent) {
    let _ = tx.send(FromAgent::HistoryCurrent(agent.history_id().map(str::to_string)));
}

/// Why this loop stopped waiting.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// The window is closing.
    Shutdown,
    /// A model configuration arrived and was saved. The caller should re-check the provider.
    Configured,
}

/// The "startup is thwarted" loop: keep answering the channel until either the window closes or the
/// Model tab supplies a configuration that works.
///
/// Two very different states land here, and both must stay *usable* rather than exit:
///
/// * the config could not be read at all (`rt == None`) — the settings panel is still on screen, but
///   there is no `Runtime` to apply anything to, so a save only writes the file and the caller starts
///   over;
/// * the config reads but has no usable `[provider]` (`rt == Some`) — the save is applied in place.
///
/// It must also answer switch-session requests: after sending one the main thread greys out the list
/// and waits for a receipt to unlock it, while those messages are doomed to do nothing in this state —
/// silence would freeze the sidebar forever.
async fn idle_until_shutdown(
    rx: &mut mpsc::UnboundedReceiver<ToAgent>,
    tx: &mpsc::UnboundedSender<FromAgent>,
    rt: Option<&Arc<Runtime>>,
) -> Wait {
    while let Some(msg) = rx.recv().await {
        match msg {
            ToAgent::Shutdown => return Wait::Shutdown,
            // ── the repair path ──
            //
            // This is why the state exists at all: a fresh install opens with nothing configured, and
            // filling the panel in has to *work* rather than tell the user to restart.
            ToAgent::ApplyProvider(edit) => {
                match rt {
                    Some(rt) => {
                        apply_provider(rt, &edit, tx);
                        if rt.provider.ready() {
                            return Wait::Configured;
                        }
                    }
                    None => {
                        // No runtime to swap, so validate and persist only; the caller re-reads the
                        // file and boots again.
                        let cfg = edit_to_config(&edit, &blank_provider_config());
                        match aox::llm::openai::OpenAiProvider::new(&cfg) {
                            Ok(_) => match remember_model(&aox::paths::app_root(), &cfg) {
                                Ok(()) => return Wait::Configured,
                                Err(e) => {
                                    let _ = tx.send(FromAgent::ModelNotice {
                                        ok: false,
                                        text: aox::tf!(
                                            "Could not save the model list: {}",
                                            format!("{e:#}")
                                        ),
                                    });
                                }
                            },
                            Err(e) => {
                                let _ = tx.send(FromAgent::ModelNotice {
                                    ok: false,
                                    text: aox::tf!(
                                        "Could not use this configuration: {}",
                                        format!("{e:#}")
                                    ),
                                });
                            }
                        }
                    }
                }
            }
            // Scanning works in this state too — it is exactly when a person wants it, and it touches
            // neither the provider nor the conversation.
            ToAgent::ScanModels { base_url, api_key } => scan_models(tx, base_url, api_key).await,
            ToAgent::TestModel { base_url, api_key, model } => {
                test_model(tx, base_url, api_key, model).await
            }
            ToAgent::Resume(_) | ToAgent::NewConversation => {
                let _ = tx.send(FromAgent::Switched {
                    opened: None,
                    error: Some(aox::i18n::t("model not ready; cannot switch conversation right now").into()),
                });
            }
            ToAgent::Input(..) => {
                let _ = tx.send(FromAgent::Event(AgentEvent::Warn(
                    aox::i18n::t("model not ready; this instruction was not sent.").into(),
                )));
            }
            // There is no log to write in this stretch (the Agent never started); drop it honestly
            ToAgent::Act(_) => {}
            // Recycling needs no Runtime at all (it is a shell call on a path), so unlike the arms
            // around it this one really does the work.
            ToAgent::Recycle(paths) => recycle_all(paths, tx).await,
            // A rename does need the storage layer, and the storage layer only exists inside
            // `Runtime` — which never started in this stretch. Say so rather than let the menu row
            // look like it did nothing.
            ToAgent::RenameConversation { .. } => {
                let _ = tx.send(FromAgent::Event(AgentEvent::Warn(
                    aox::i18n::t("model not ready; the conversation cannot be renamed right now.")
                        .into(),
                )));
            }
            // The workplace button is also dead here — `Workplace` lives in `Runtime`, and the Runtime
            // never started. Say something rather than let the button look "unresponsive when clicked"
            // (the same reason as the `Input` arm above).
            ToAgent::Workplace(_) => {
                let _ = tx.send(FromAgent::Event(AgentEvent::Warn(
                    aox::i18n::t("model not ready; the workplace cannot be switched right now.").into(),
                )));
            }
            // Recovery reads the workspace store, and the store lives inside the Runtime that never
            // started. Answer with a view rather than silence: the panel would otherwise sit on its
            // skeleton with no way to learn why.
            ToAgent::Recovery(_) => {
                let _ = tx.send(FromAgent::Recovery(Box::new(RecoveryView::Notice {
                    text: aox::i18n::t("The model is not ready, so the recovery store cannot be read yet.").into(),
                    ok: false,
                })));
            }
        }
    }
    Wait::Shutdown
}

/// A `[provider]` with nothing filled in — the "previous" side of an edit made when no `Runtime` exists
/// yet. The defaults are the same ones the config parser uses, so the edit's own values are what land.
fn blank_provider_config() -> aox::config::ProviderConfig {
    aox::config::ProviderConfig {
        base_url: String::new(),
        model: String::new(),
        api_key: None,
        api_key_env: None,
        max_tokens: Some(aox::config::DEFAULT_MAX_TOKENS),
        temperature: None,
        stream: true,
        context_window: Some(aox::config::DEFAULT_CONTEXT_WINDOW),
        thinking: aox::config::Thinking::default(),
        thinking_wire: aox::config::ThinkingWire::default(),
    }
}

/// Take the next user instruction, handling control messages like "switch session" along the way.
///
/// This does not go through `Ui::read_user_input()`: that method can only return user text, whereas
/// switching a session must **modify `agent` in place**, which only this thread can reach. The channel is
/// still the same one, so the order naturally matches the user's order of operations.
///
/// The return value carries the attachment paths: attachments must be assembled into the message **on the
/// Agent thread** (`Message::user_with_files`); the UI side must not build it itself — otherwise the
/// "in memory but not in the history file" divergence comes right back.
async fn next_command(
    ui: &mut GuiUi,
    agent: &mut Agent,
    tx: &mpsc::UnboundedSender<FromAgent>,
) -> Result<Option<(String, Vec<PathBuf>)>> {
    loop {
        match ui.rx.recv().await {
            Some(ToAgent::Input(s, files)) => return Ok(Some((s, files))),
            Some(ToAgent::Resume(id)) => {
                // Let the storage seam open that conversation by id — the UI side does not build paths (see `src/store.rs`).
                let msg = match agent.resume(&id) {
                    Ok(_) => FromAgent::Switched { opened: Some(id), error: None },
                    Err(e) => FromAgent::Switched { opened: None, error: Some(format!("{e:#}")) },
                };
                let _ = tx.send(msg);
                // The ledger is sent right after Switched: on receiving Switched the main thread first
                // clears the UI ledger, so a reversed order would wipe the just-restored data by itself.
                let _ = tx.send(FromAgent::UsageLog(agent.usage_log().to_vec()));
                // The workplace must be pushed along too: each conversation has its own tier and root
                // (`resume` has already restored the persisted one). Without it the button would keep
                // showing **the previous conversation's** tier — which is the "open an old conversation,
                // the workplace is still the last one" that was hit on a real run.
                sync_workplace(&tx, agent.rt());
            }
            Some(ToAgent::NewConversation) => {
                agent.new_conversation();
                let _ = tx.send(FromAgent::Switched { opened: None, error: None });
                let _ = tx.send(FromAgent::UsageLog(Vec::new()));
                // A new conversation always returns to the "default" tier (`new_conversation` already
                // switched it); the UI must catch up immediately, or the button keeps showing the last
                // conversation's tier.
                sync_workplace(&tx, agent.rt());
            }
            // Activity-stream persistence: done in place, without entering the next turn — the order is
            // exactly the user's order of operations, so "this turn's timeline" always lands in this
            // turn's conversation file.
            Some(ToAgent::Act(acts)) => agent.append_acts(&acts),
            // Renaming a conversation: one appended row, written in place, without entering the next
            // turn — nothing about the model's context changes, so there is nothing to re-read.
            //
            // The receipt is `HistoryCurrent`: its meaning is already "a conversation file changed on
            // disk, rescan the directory", which is exactly what the list needs to show the new name
            // (and it is the only thing that refreshes the panel). A failed rename has to speak up —
            // the user typed a name and would otherwise be left thinking it took.
            Some(ToAgent::RenameConversation { id, title }) => {
                match agent.rename_conversation(&id, title.as_deref()) {
                    Ok(()) => send_history_current(tx, agent),
                    Err(e) => ui.emit(AgentEvent::Warn(aox::tf!(
                        "Could not rename the conversation: {}",
                        format!("{e:#}")
                    ))),
                }
            }
            // Deleting a conversation from the history list. Nothing about it needs the Agent: the
            // file is moved to the recycle bin and the count comes back as an `AgentEvent`, exactly
            // as if a tool had deleted it.
            Some(ToAgent::Recycle(paths)) => recycle_all(paths, tx).await,
            // ── the Model tab, while the session is running ──
            //
            // Deliberately **in place**: no rebuild of the Runtime, no new Agent. Changing the model
            // mid-conversation keeps the messages, the history file and the token ledger exactly where
            // they are; only the next request goes to a different model. Rebuilding here would look
            // identical on screen and silently drop the conversation.
            Some(ToAgent::ApplyProvider(edit)) => {
                {
                    let rt = agent.rt();
                    apply_provider(rt, &edit, tx);
                }
                // The folded-history cache was computed against the **old** budget. Keeping it would
                // mean one round still being requested at the previous window's size, which is either a
                // 400 (the window shrank) or a needless re-fold (it grew). Dropping it costs one
                // recomputation and removes the only way the two can disagree.
                agent.set_view_cache(None);
            }
            Some(ToAgent::ScanModels { base_url, api_key }) => scan_models(tx, base_url, api_key).await,
            Some(ToAgent::TestModel { base_url, api_key, model }) => {
                test_model(tx, base_url, api_key, model).await
            }
            // Workplace button: switch tier / query state. Done in place — `Workplace` lives inside
            // `Runtime`, and this thread is the only place that can reach the Agent (and through it its
            // `rt`). It does not enter the next turn: a tier switch is a pure host action, unrelated to
            // the model conversation.
            Some(ToAgent::Workplace(mode)) => {
                let rt = agent.rt().clone();
                set_workplace(ui, &rt, mode);
                // A tier switch must also be recorded in **this conversation's** archive: the workplace
                // follows the conversation, and without recording it, opening this conversation next time
                // would fall back to the default ground. `persist_workplace` compares for itself and does
                // not write when nothing changed, so neither failure nor no-change leaves junk lines.
                agent.persist_workplace();
            }
            // The recovery panel. Nothing here enters the model conversation: like the workplace
            // button, these are host actions about the workspace, not instructions to the model.
            Some(ToAgent::Recovery(req)) => recovery_request(agent, tx, req),
            Some(ToAgent::Shutdown) | None => return Ok(None),
        }
    }
}

/// Start it on a dedicated thread: build the tokio runtime → load the runtime → run the driver loop.
pub fn spawn(
    tx: mpsc::UnboundedSender<FromAgent>,
    rx: mpsc::UnboundedReceiver<ToAgent>,
    flags: Arc<SessionFlags>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("aox-agent".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(FromAgent::Fatal(aox::tf!("failed to create async runtime: {}", e)));
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(e) = boot_and_run(tx.clone(), rx, flags).await {
                    let _ = tx.send(FromAgent::Fatal(format!("{e:#}")));
                }
            });
        })
        .expect(aox::i18n::t("failed to start the agent thread"))
}

async fn boot_and_run(
    tx: mpsc::UnboundedSender<FromAgent>,
    mut rx: mpsc::UnboundedReceiver<ToAgent>,
    flags: Arc<SessionFlags>,
) -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (root, default_plugins) = aox::paths::resolve_workplace();
    let plugins_dir = aox::paths::arg_value(&args, "--plugins")
        .map(PathBuf::from)
        .unwrap_or(default_plugins);
    let config_path = aox::paths::resolve_config_path(&args);

    if let Err(e) = aox::plugin::ensure_sdk(&plugins_dir) {
        let _ = tx.send(FromAgent::Event(AgentEvent::Warn(aox::tf!(
            "failed to write the plugin SDK ({}): plugin features may be unavailable",
            format!("{e:#}")
        ))));
    }

    // ── a fresh install has no config.toml ────────────────────────────────────────────────
    //
    // The installer ships `config.example.toml` only, on purpose: no personal key is ever baked into
    // an installer. The CLI has always written the template on its first run; the GUI never did, so a
    // fresh install opened onto "config file not found" and the only way forward was to leave the
    // program and edit a file by hand — exactly the friction the Model tab exists to remove. Now the
    // template lands first (documentation included), the panel opens on top of it, and the two things
    // that were missing get filled in there.
    // "Nothing has been chosen yet" rather than "there is no config.toml": since the model
    // configuration moved into `.aox/models.json`, a fresh install can perfectly well have a config
    // file and still need the panel (and the other way round).
    //
    // `mut` because it is **spent** by the first `boot` below: the "not configured" branch announces
    // itself, and the `boot` that follows a successful save must not announce it a second time (see
    // the reset after that first boot).
    let mut first_run = aox::modelbook::Book::load(&root).active.is_none();
    if first_run {
        match std::fs::write(&config_path, aox::config::TEMPLATE) {
            Ok(()) => {
                let _ = tx.send(FromAgent::Event(AgentEvent::Info(aox::tf!(
                    "Created a configuration template at {}. Fill in the endpoint and the key in Settings → Model; no restart needed.",
                    config_path.display()
                ))));
            }
            Err(e) => {
                let _ = tx.send(FromAgent::Event(AgentEvent::Warn(aox::tf!(
                    "No config file at {} and it could not be created ({}). Settings → Model still works for this session, but nothing can be saved until that directory is writable.",
                    config_path.display(),
                    e
                ))));
            }
        }
    }

    // Config goes through **layered composition** (stage 5): profile → bundle → patch → config.toml.
    // The same `aox::config::layer::resolve` as the CLI — the two frontends do not each keep a set of composition rules.
    let resolved = match aox::config::layer::resolve(
        &root,
        &config_path,
        aox::paths::resolve_manifest_path(&args).as_deref(),
        aox::paths::resolve_profile(&args).as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => {
            // Config missing/invalid: the GUI still opens, showing guidance, and the input area is
            // disabled. The history-reading backend uses the default one (the config is not read yet),
            // so the sidebar doesn't go empty along with it.
            let store = aox::store::default_store(&root, 0);
            // Feed the workspaces even here: the registry only needs `.aox/spaces.json`, not the
            // config, so a broken config must not also cost the user the conversations made in other
            // workspaces. Exactly the call `Runtime::new` makes.
            let spaces = aox::space::Registry::load(&root);
            store.set_workspaces(&spaces.workspace_roots(&root));
            // Config could not be read: the row uses the defaults (`[shell] bash = "auto"`), which is
            // the honest thing to show — the reason the config failed is shown right above it.
            let paths = boot_paths(
                &root,
                &config_path,
                &plugins_dir,
                &store,
                &aox::config::ShellConfig::default(),
            );
            boot(
                &tx,
                &config_path,
                "",
                0,
                false,
                false,
                Some(format!("{e:#}")),
                store,
                Vec::new(),
                paths,
                false,
            );
            // Config unreadable, and there is **no Runtime to apply anything to**. The panel is still
            // on screen (a save lands in `config.toml` and reports success or failure through
            // `ModelNotice`), but nothing can be applied in this process: the layers that failed are
            // read once, at startup. So the window stays open rather than vanishing — the user has to
            // see the reason and the file it points at.
            let _ = idle_until_shutdown(&mut rx, &tx, None).await;
            return Ok(());
        }
    };
    let config_layers = resolved.composed.layer_labels();
    let config = resolved.config;

    let rt = Runtime::new(config, root.clone(), plugins_dir.clone())?;
    for t in aox::tool::meta::meta_tools() {
        rt.register(t).await?;
    }
    // Host file tools (the workspace boundary, see src/workplace.rs and src/tool/hostfs.rs).
    // The same group and the same implementation registered as the CLI; with the default
    // `[workplace] mode = "full"` they behave no differently from ordinary read/write tools — the
    // boundary takes effect only after you tighten the tier.
    for t in aox::tool::hostfs::host_file_tools() {
        rt.register(t).await?;
    }
    // Interaction tools (`ask_user`): let the model hand a "multiple-choice question" to the UI, rather
    // than writing it in the body and waiting for the user to type. Registering them here matters because
    // it **makes the model see them in the tools list**.
    for t in aox::tool::ask::ask_tools() {
        rt.register(t).await?;
    }
    // Tool discovery (`aox::tool::discover`). Same story as the CLI: visible to the model, executed by
    // the agent loop (it changes the conversation's advertised tool set, which `Tool::call` can't reach).
    for t in aox::tool::discover::find_tools() {
        rt.register(t).await?;
    }
    // Task checkpoints (`aox::task`). Same story as the CLI: registration makes it visible to the
    // model, execution is intercepted by the agent loop.
    for t in aox::tool::task::task_tools() {
        rt.register(t).await?;
    }
    // Verification (`aox::tool::verify`). Same story: visible to the model, executed by the agent loop.
    // The built-in command line (`aox::tool::shell`): same tool, same boundary, same executor as in
    // the CLI. Registering it here is what makes "the GUI has no shell" stop being true.
    if rt.config.shell.enabled {
        for t in aox::tool::shell::shell_tools(&rt.config.shell) {
            rt.register(t).await?;
        }
        for t in aox::tool::bash::bash_tools(&rt) {
            rt.register(t).await?;
        }
        // Long-lived jobs (`shell_job`): same host-owned Job fence as a one-shot call, but the
        // runtime keeps it, so a dev server survives the calls that use it.
        for t in aox::tool::job::job_tools() {
            rt.register(t).await?;
        }
    }
    for t in aox::tool::verify::verify_tools() {
        rt.register(t).await?;
    }

    let mut ui = GuiUi { tx: tx.clone(), rx };

    // Runtime status: one line to the "About" window, one missing-interpreter hint into the activity
    // stream (the same copy as the CLI). So the user knows immediately in the GUI that "this machine
    // lacks python, click here to install", rather than discovering it only when the model fails to
    // build a plugin.
    send_runtime_status(&tx, &rt);
    if let Some(hint) = aox::runtimes::missing_hint(&rt.root, &rt.config.runtimes) {
        ui.emit(AgentEvent::Warn(hint));
    }

    // ── model not configured yet: stay up, and wait for the Model tab ─────────────────────
    //
    // This used to be a dead end: report "LLM not ready", disable the input box, and idle until the
    // window closed — with the fix being "edit config.toml and restart". On a fresh install that was
    // the *first* thing a user met. The window is now fully usable in this state (history, plugins,
    // settings), and a save from the Model tab comes back through the same channel, is applied here,
    // and the session simply carries on into the code below.
    let mut plugins_restored = false;
    if let Some(reason) = rt.provider.error() {
        ui.emit(AgentEvent::Error(aox::tf!("LLM not ready: {}", reason)));
        ui.emit(AgentEvent::Info(
            aox::i18n::t("Open Settings → Model, fill in the endpoint and the key, and the session starts right away — no restart.").into(),
        ));
        let paths = boot_paths(&root, &config_path, &plugins_dir, &rt.store, &rt.config.shell);
        boot(
            &tx,
            &config_path,
            &rt.provider.cfg().model,
            ctx_window(&rt),
            false,
            rt.config.recovery.enabled,
            Some(reason),
            rt.store.clone(),
            config_layers.clone(),
            paths,
            first_run,
        );
        // The "add your model" prompt has been handed to the UI. Spending the flag here is what
        // stops the *second* `boot` (below, after the save makes the provider usable) from asking
        // again — the "saved it, and now there is another one" report of 2026-10-28.
        first_run = false;
        // Even in the "not ready" state, fill the plugin panel with what is on disk.
        restore_plugins(&mut ui, &rt).await;
        plugins_restored = true;
        // Still in "not ready" state, but send the workplace state anyway: if the button is visible, at least it shows the real tier.
        sync_workplace(&tx, &rt);
        push_provider(&tx, &rt);
        // One iteration is the normal case; the loop is here so that a save which fails to make the
        // provider usable (wrong key, unreachable host) leaves the window waiting instead of starting a
        // session that cannot answer.
        loop {
            match idle_until_shutdown(&mut ui.rx, &tx, Some(&rt)).await {
                Wait::Shutdown => return Ok(()),
                Wait::Configured => {
                    if rt.provider.ready() {
                        break;
                    }
                }
            }
        }
    }

    // The project rules file (RULES.md) is now read centrally by `aox::prompt::build`, no longer once
    // each in the GUI and CLI (see src/prompt.rs).
    let mut agent = Agent::new(rt.clone());
    let paths = boot_paths(&root, &config_path, &plugins_dir, &rt.store, &rt.config.shell);
    // Hand the session to the UI **before** touching plugins. The history sidebar reads off the store
    // that `boot` carries, so restoring plugins first used to blank it for the whole wait — up to
    // `INIT_TIMEOUT` (20 s) per cold-or-broken plugin. This order is the point of the fix.
    boot(
        &tx,
        &config_path,
        &rt.provider.cfg().model,
        ctx_window(&rt),
        true,
        rt.config.recovery.enabled,
        None,
        rt.store.clone(),
        config_layers,
        paths,
        first_run,
    );
    push_provider(&tx, &rt);
    send_history_current(&tx, &agent);
    // Now restore persisted plugins (the same path as the CLI). It is the one startup step that can
    // block, so it runs here — after the UI already has the session — and finishes before the driver
    // loop below, so the very first turn still sees every plugin tool.
    //
    // Skipped when the "not configured yet" branch already did it: `load_all` starts every plugin
    // process, and running it twice would start each one twice for no reason.
    if !plugins_restored {
        restore_plugins(&mut ui, &rt).await;
    }
    // Send the workplace state right at startup: both the button's initial "Workplace · <tier>" and which
    // item to tick in the three-way list depend on it.
    sync_workplace(&tx, &rt);

    // Broadcast plugin snapshots periodically: every new plugin the model creates becomes visible in the
    // right panel immediately. (Tool names are no longer broadcast — the top-bar tool chips were removed,
    // so there is no consumer.)
    let snap_tx = tx.clone();
    let snap_rt = rt.clone();
    let snap_task = tokio::spawn(async move {
        loop {
            let plugins = {
                let p = snap_rt.plugins.lock().await;
                let mut v: Vec<String> = p.keys().cloned().collect();
                v.sort();
                v
            };
            if snap_tx.send(FromAgent::Snapshot(Snapshot { plugins })).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
    });

    // ---- Driver loop: one-to-one with the CLI's REPL
    loop {
        let Some((input, files)) = next_command(&mut ui, &mut agent, &tx).await? else { break };
        let line = input.trim().to_string();
        // Only attachments with no text is also a legal message (drag an image and send directly)
        if line.is_empty() && files.is_empty() {
            continue;
        }
        // Local commands: these are handled by the host itself, not sent to the model (same as the CLI).
        if aox::recovery::command::is_cmd(&line) {
            recovery_command(&mut ui, &mut agent, &line);
            continue;
        }
        if aox::runtimes::is_cmd(&line) {
            runtime_command(&mut ui, &rt, &line).await;
            continue;
        }
        flags.clear();
        let mark = agent.messages_len();
        enum Outcome {
            Done(Result<()>),
            Aborted,
        }
        let outcome = {
            let turn = agent.run_turn(&mut ui, &line, &files);
            tokio::pin!(turn);
            tokio::select! {
                r = &mut turn => Outcome::Done(r),
                _ = wait_abort(flags.clone()) => Outcome::Aborted,
            }
        };
        match outcome {
            Outcome::Done(Ok(())) => {}
            Outcome::Done(Err(e)) => {
                let _ = ui.tx.send(FromAgent::Fatal(format!("{e:#}")));
            }
            Outcome::Aborted => {
                // Roll back to the last **legal** boundary, not to the start of the turn: the
                // user's instruction and every completed step stay in the model's context, and only
                // the in-flight step is dropped. Exactly the same call as the CLI's — the
                // granularity decision lives in `Agent::interrupt_rollback_point`, so the two front
                // ends cannot drift apart.
                //
                // The attachment tray needs nothing here: the GUI clears it at send time
                // (`app.rs`), which is already consistent with "this message was delivered".
                agent.rollback_interrupted_turn(mark);
                // The future was dropped, so `run_turn` cannot close the recovery transaction.
                let rec = agent.finish_recovery_turn(aox::recovery::model::TurnStatus::Interrupted);
                for w in rec.warnings {
                    ui.emit(AgentEvent::Warn(aox::tf!("recovery: {}", w)));
                }
                if let Some(e) = rec.error {
                    ui.emit(AgentEvent::Warn(aox::tf!("recovery checkpoint was not committed: {}", e)));
                }
                let _ = ui.tx.send(FromAgent::TurnAborted);
            }
        }
        // Sync "current conversation" once after each turn: the first turn creates the file (a new entry
        // appears in the sidebar), and repeated notifications with the same id make the sidebar refresh the count.
        send_history_current(&tx, &agent);
        // Sync the workplace along the way: the `default` tier's root is created **on the first message**,
        // so under the "default" tier the directory behind the button has a value only at the end of this
        // turn — without resending, the UI would keep showing "root: not decided yet".
        sync_workplace(&tx, &rt);
    }

    snap_task.abort();
    // Close all plugin child processes (the same as the CLI's exit path). Under lazy loading most
    // slots are dormant and `stop` is a no-op for them.
    let handles: Vec<_> = rt.plugins.lock().await.values().cloned().collect();
    for h in handles {
        h.stop().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aox::config::{ProviderConfig, Thinking, ThinkingWire};

    /// The configuration already in force, with the key coming from the environment.
    fn prev() -> ProviderConfig {
        ProviderConfig {
            base_url: "https://old.example".into(),
            model: "old-model".into(),
            api_key: None,
            api_key_env: Some("MY_KEY".into()),
            max_tokens: Some(8_000),
            temperature: Some(0.2),
            // A per-model switch: whatever this held, an edit replaces it with the panel's answer.
            stream: false,
            context_window: Some(128_000),
            thinking: Thinking::Off,
            thinking_wire: ThinkingWire::Auto,
        }
    }

    fn edit(key: Option<&str>) -> ProviderEdit {
        ProviderEdit {
            base_url: " https://api.deepseek.com ".into(),
            model: " deepseek-flash ".into(),
            api_key: key.map(str::to_string),
            context_window: 1_000_000,
            max_tokens: 32_000,
            temperature: Some(0.3),
            thinking: Thinking::High,
            thinking_wire: ThinkingWire::Deepseek,
            stream: true,
        }
    }

    /// The panel never shows the stored key, so an untouched key field must mean "keep using the
    /// environment variable" — dropping it would silently break every install that keeps its key in
    /// the environment, on the first press of any control.
    #[test]
    fn an_edit_without_a_key_keeps_the_environment_variable() {
        let c = edit_to_config(&edit(None), &prev());
        assert_eq!(c.api_key_env.as_deref(), Some("MY_KEY"));
        assert!(c.api_key.is_none());
        // And the visible fields did change, or the merge is doing nothing.
        assert_eq!(c.model, "deepseek-flash");
        assert_eq!(c.base_url, "https://api.deepseek.com", "whitespace is trimmed");
    }

    /// A typed key wins and takes the variable out with it: `resolve_key` prefers the inline value, so
    /// leaving `api_key_env` set would leave a config line that no longer has any effect.
    #[test]
    fn a_typed_key_replaces_the_environment_variable() {
        let c = edit_to_config(&edit(Some(" sk-new ")), &prev());
        assert_eq!(c.api_key.as_deref(), Some("sk-new"));
        assert!(c.api_key_env.is_none());
    }

    /// Streaming used to be a config-only preference that had to be carried over by hand. It is a
    /// switch in the dialog now (a per-model field), so the edit **replaces** it — in both directions,
    /// which is what makes "turn it off for this gateway" survivable: the setting belongs to the record,
    /// not to the session.
    #[test]
    fn the_streaming_switch_comes_from_the_edit() {
        let mut e = edit(None);
        e.stream = true;
        assert!(edit_to_config(&e, &prev()).stream, "on, even though the previous value was off");
        e.stream = false;
        assert!(!edit_to_config(&e, &prev()).stream, "and off, even starting from off");
    }

    /// A zero window or cap cannot work (and would divide by zero in the ring), so it is clamped rather
    /// than written to the file.
    #[test]
    fn a_zero_is_clamped_before_it_reaches_the_file() {
        let mut e = edit(None);
        e.context_window = 0;
        e.max_tokens = 0;
        let c = edit_to_config(&e, &prev());
        assert_eq!(c.context_window, Some(1));
        assert_eq!(c.max_tokens, Some(1));
    }

    /// The "no Runtime yet" side of an edit starts from the code defaults, so what the user typed is
    /// exactly what lands — no leftover from a config that could not be read.
    #[test]
    fn the_blank_side_of_a_first_run_edit_carries_no_leftovers() {
        let b = blank_provider_config();
        assert!(b.base_url.is_empty());
        let c = edit_to_config(&edit(Some("sk-x")), &b);
        assert_eq!(c.base_url, "https://api.deepseek.com");
        assert_eq!(c.max_tokens, Some(32_000));
        assert!(c.api_key_env.is_none());
    }
}
