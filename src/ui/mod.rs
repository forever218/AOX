pub mod cli;
pub mod spinner;
pub mod style;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::history::UsageRecord;
use crate::llm::{StreamSink, Usage};

/// Events flowing from the Agent to the UI. A GUI implementation can subscribe to this event stream
/// and render directly.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// The assistant's **final reply**: text after which no more tools are called and the task ends.
    AssistantText(String),
    /// Interim text the assistant emits while it "still has tools to call" (e.g. "let me see which
    /// plugins there are").
    /// It is process narration, so it must be folded into the process area and never shown as the
    /// final answer.
    InterimText(String),
    /// About to execute a tool call
    ToolCall { name: String, args: Value },
    /// The tool passed authorisation and is starting to run (may take a while)
    ToolRunning { name: String, timeout_secs: u64 },
    /// The tool call finished. `diff` is the edit statistic — `(added, removed)` lines, or `None`
    /// when the call was not an edit; the GUI shows it as "+N / −N" on the call's own row. The same
    /// row can be unfolded, and `diff_detail` is what it then draws — the diff itself, one line per
    /// entry, each prefixed with `+`, `-` or a space (see `ToolOutput::diff_detail`).
    ///
    /// `#[allow(dead_code)]` for the same reason `Images` carries it: `src/main.rs` (the CLI bin)
    /// compiles these modules a second time, and there the variant is only ever **written**.
    #[allow(dead_code)]
    ToolResult {
        name: String,
        is_error: bool,
        preview: String,
        diff: Option<(usize, usize)>,
        diff_detail: Option<String>,
        /// The file this call wrote, when it was a file edit (see `ToolOutput::path`). The
        /// workspace view lists a turn's edits by this field and nothing else — `content` above is
        /// prose for the model, so a front end must never try to recover the name from it.
        path: Option<std::path::PathBuf>,
    },
    Info(String),
    Warn(String),
    Error(String),
    /// The host has just attached **an image produced by a tool** to the conversation (see
    /// `crate::attach`).
    ///
    /// A separate event rather than stuffing paths into that `Warn`: `Warn` is **a sentence for a
    /// human to read**, whereas this is **a picture for a human to see**. The GUI takes it and
    /// renders the image; the command line has nowhere to render, so it can simply ignore it (the
    /// filenames are already listed in that sentence).
    ///
    /// Why `#[allow(dead_code)]`: `src/main.rs` (the CLI bin) compiles the same batch of modules a
    /// second time, and there it only ever writes `Images(_)` and never reads the paths inside it —
    /// so that field is judged dead code in the bin, even though the `lib` side is plainly using it.
    /// **Do not** touch the module structure for this (same cause as the "bin and lib each compile
    /// the same-named modules" entry in `TRAPS.md`).
    #[allow(dead_code)]
    Images(Vec<std::path::PathBuf>),
    /// **N files were moved to the system recycle bin** by a host file tool.
    ///
    /// A variant of its own rather than a sentence inside `Info`, for the same split as `Images`:
    /// the CLI prints one dim line, whereas the graphical front end shows a **transient toast**
    /// that fades away by itself. `Info` is the shape for neither — a note is a permanent line in
    /// the transcript, which is the wrong weight for "something is in the bin now". It also
    /// carries a **number, not a sentence**: the wording belongs to the front end.
    ///
    /// Emitted only when the count is non-zero, so "no event" means "nothing was deleted" — a
    /// front end never has to decide whether a zero is worth mentioning.
    Recycled(usize),
}

/// The outcome of an authorisation prompt
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    Allow,
    AllowSession,
    Deny,
}

/// The user's answer to one "multiple-choice question" (`ask_user`'s return-value source).
///
/// The three variants correspond to three **genuinely distinct** situations, with different wording too (see `tool::ask::interact`):
/// picked an option / didn't pick, but typed a line / didn't answer at all. Collapsing them into `Option<String>`
/// would merge "typed something" with "picked an option" into one thing — yet the model handles the two differently
/// (for the former it must ask "is that what you meant?", for the latter it just complies).
#[derive(Debug, Clone)]
pub enum ChoiceReply {
    /// Picked option n (0-based)
    Picked(usize),
    /// Didn't pick an option, just typed a line
    Text(String),
    /// No answer (the interface was closed, or the connection dropped)
    Cancelled,
}

/// The UI abstraction — the interface kept in reserve for a future graphical front end.
/// The Agent main loop depends only on this trait; the CLI is one implementation, and a future
/// egui/Tauri interface is another.
#[async_trait]
pub trait Ui: Send {
    /// Read one line of user input; Ok(None) means exit (EOF or the user quitting)
    async fn read_user_input(&mut self) -> Result<Option<String>>;
    /// Show an Agent event
    fn emit(&mut self, event: AgentEvent);
    /// Ask the user for authorisation for one tool call
    async fn authorize(&mut self, tool_name: &str, args: &Value) -> AuthDecision;

    /// Ask the user one "multiple-choice question" and wait for the answer (the landing point of
    /// `ask_user`).
    ///
    /// The default implementation returns [`ChoiceReply::Cancelled`]: a UI that doesn't override it
    /// won't crash for lack of the capability, it just leaves `ask_user` with "the user didn't
    /// answer" — which is the truth (this interface really can't ask). Both front ends (CLI / GUI)
    /// implement it.
    async fn choose(&mut self, _question: &str, _options: &[String]) -> ChoiceReply {
        ChoiceReply::Cancelled
    }

    /// A turn starts (the user issued an instruction, the model + tool loop begins).
    /// The UI may start its waiting animation here and get ready to receive this turn's events.
    async fn turn_start(&mut self) {}
    /// A turn ends (the model stops calling tools).
    /// The UI may stop the spinner here and fold the middle process into a one-line summary.
    async fn turn_end(&mut self) {}
    /// The token account for one turn.
    ///
    /// The argument is **the very line that lands in the conversation file** (`kind:"usage"`), so
    /// the account shown in the interface can never disagree with the one on disk. It holds the turn
    /// number, the timestamp, this turn's items, and the conversation total up to this turn.
    ///
    /// It is deliberately called **after** `turn_end`: by then the waiting animation has been torn
    /// down, so the CLI can print the detail cleanly on the cursor's current line instead of being
    /// painted over by a spinner redraw.
    ///
    /// When the endpoint doesn't support usage, `rec.is_empty()` is true and the UI can simply skip
    /// this display.
    async fn turn_usage(&mut self, _rec: &UsageRecord) {}
    /// The account for **one model call inside the current turn** — the same figures `turn_usage`
    /// reports, but per call and arriving while the turn is still running.
    ///
    /// It is a separate callback rather than a field on `turn_usage` because the two answer different
    /// questions. `turn_usage` gives the turn's aggregate (`UsageRecord::usage` is the **sum** over
    /// every call the turn made) and lands once the turn is over; a live view — the cache-hit map is
    /// the only user — needs "what is the context right now", which is the *last* call, and it needs
    /// it before the turn ends. Deriving one from the other is impossible: the sum of the cache hits
    /// over a turn's calls can easily exceed the size of its last request.
    ///
    /// `last_prompt` is the accumulator's current input size, offered as a fallback for endpoints
    /// whose per-call `usage.prompt` is absent. The default implementation does nothing, so a front
    /// end with no live view (the CLI) is unaffected.
    async fn call_usage(&mut self, _usage: &Usage, _last_prompt: Option<u64>) {}
    /// What the request **about to be sent** is made of: one entry per section, in body order, with
    /// a rough token estimate for each.
    ///
    /// Called immediately before every model call — **before**, not after, because the point is to
    /// attribute the reply's usage figures to the sections that produced them, and by the time the
    /// reply arrives the request body is gone.
    ///
    /// Empty default: a UI with nothing to say about request composition (the CLI, the spinner)
    /// simply does not implement it, and the cost of that is one function that isn't there.
    async fn context_map(&mut self, _segs: &[(&'static str, u64)]) {}
    /// Update the in-progress status label (e.g. "Thinking", "Running fs_read").
    /// An empty default implementation; the GUI can map it to status-bar text.
    async fn set_status(&mut self, _label: &str) {}

    /// Provide a "streaming delta sink": while streaming, the Provider pushes the model's reasoning
    /// process into it and the UI can show it live (the CLI scrolls it in the waiting area, and it
    /// disappears once shown).
    ///
    /// Returning None means this UI doesn't show the streaming process. The default implementation
    /// is None, so when the GUI wants to show the reasoning stream it only needs to override this one
    /// method, with no change to the core loop.
    fn stream_sink(&self) -> Option<Box<dyn StreamSink + Send + Sync + '_>> {
        None
    }
}
