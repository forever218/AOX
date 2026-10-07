pub mod cli;
pub mod spinner;
pub mod style;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::history::UsageRecord;
use crate::llm::StreamSink;

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
    /// The tool call finished
    ToolResult { name: String, is_error: bool, preview: String },
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
