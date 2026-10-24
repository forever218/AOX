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
use aox::llm::{LlmProvider, StreamSink, Usage};
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
    /// Move these paths to the system recycle bin and report the total.
    ///
    /// The main thread cannot do this itself: `recycle` is async and can block for a while on the
    /// shell, and the "N files moved to the recycle bin" notice is an [`AgentEvent`] — routing it
    /// through the **same event** the model's own file deletion produces means both deletions look
    /// identical to the user (one toast, same wording), instead of the GUI growing a second, subtly
    /// different notion of "delete".
    Recycle(Vec<PathBuf>),
    /// Exit: close the plugin child processes and end the driver thread
    Shutdown,
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
                | Some(ToAgent::Workplace(_)) => continue,
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
    rt.config.provider.context_window.map(u64::from).unwrap_or(0)
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
    note: Option<String>,
    store: Arc<dyn aox::store::Store>,
    // Which layers this config was composed from (low → high). An empty list when the config cannot be read — do not invent layers that don't exist.
    layers: Vec<String>,
    paths: BootPaths,
) {
    let _ = tx.send(FromAgent::Boot(Box::new(BootInfo {
        config_path: config_path.display().to_string(),
        config_layers: layers,
        model: model.to_string(),
        context_window,
        provider_ready: ready,
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

/// The "only wait for shutdown" loop when startup is thwarted (config unreadable / model not ready).
///
/// It must answer switch-session requests: after sending one the main thread greys out the list and
/// waits for a receipt to unlock it, while those messages are doomed to do nothing in this state —
/// silence would freeze the sidebar forever.
async fn idle_until_shutdown(
    rx: &mut mpsc::UnboundedReceiver<ToAgent>,
    tx: &mpsc::UnboundedSender<FromAgent>,
) {
    while let Some(msg) = rx.recv().await {
        match msg {
            ToAgent::Shutdown => break,
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
        }
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
                Some(format!("{e:#}")),
                store,
                Vec::new(),
                paths,
            );
            idle_until_shutdown(&mut rx, &tx).await;
            return Ok(());
        }
    };
    let config_layers = resolved.composed.layer_labels();
    let config = resolved.config;

    let model = config.provider.model.clone();
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

    let llm: Arc<dyn LlmProvider> = match aox::llm::openai::OpenAiProvider::new(&rt.config.provider) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            ui.emit(AgentEvent::Error(aox::tf!("LLM not ready: {}", format!("{e:#}"))));
            ui.emit(AgentEvent::Info(
                aox::i18n::t("Fill in base_url and api_key (or api_key_env) under [provider] in config.toml, then restart.").into(),
            ));
            let paths = boot_paths(&root, &config_path, &plugins_dir, &rt.store, &rt.config.shell);
            boot(
                &tx,
                &config_path,
                &model,
                ctx_window(&rt),
                false,
                Some(format!("{e:#}")),
                rt.store.clone(),
                config_layers.clone(),
                paths,
            );
            // Even in the "not ready" state, fill the plugin panel with what is on disk.
            restore_plugins(&mut ui, &rt).await;
            // Still in "not ready" state, but send the workplace state anyway: if the button is visible, at least it shows the real tier.
            sync_workplace(&tx, &rt);
            idle_until_shutdown(&mut ui.rx, &tx).await;
            return Ok(());
        }
    };

    // The project rules file (RULES.md) is now read centrally by `aox::prompt::build`, no longer once
    // each in the GUI and CLI (see src/prompt.rs).
    let mut agent = Agent::new(llm, rt.clone());
    let paths = boot_paths(&root, &config_path, &plugins_dir, &rt.store, &rt.config.shell);
    // Hand the session to the UI **before** touching plugins. The history sidebar reads off the store
    // that `boot` carries, so restoring plugins first used to blank it for the whole wait — up to
    // `INIT_TIMEOUT` (20 s) per cold-or-broken plugin. This order is the point of the fix.
    boot(&tx, &config_path, &model, ctx_window(&rt), true, None, rt.store.clone(), config_layers, paths);
    send_history_current(&tx, &agent);
    // Now restore persisted plugins (the same path as the CLI). It is the one startup step that can
    // block, so it runs here — after the UI already has the session — and finishes before the driver
    // loop below, so the very first turn still sees every plugin tool.
    restore_plugins(&mut ui, &rt).await;
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
        // Local command: /runtime is handled by the host itself, not sent to the model (same as the CLI)
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
