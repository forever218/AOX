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
    /// Interim text the assistant emits while it "still has tools to call" (e.g. "let me see which plugins there are").
    InterimText(String),
    /// About to execute a tool call
    ToolCall { name: String, args: Value },
    /// The tool passed authorisation and is starting to run (may take a while).
    ToolRunning { name: String, timeout_secs: u64, expect_secs: Option<u64> },
    /// The tool call finished. `diff` is the edit statistic — `(added, removed)` lines, or `None` when the call was...
    #[allow(dead_code)]
    ToolResult {
        name: String,
        is_error: bool,
        preview: String,
        diff: Option<(usize, usize)>,
        diff_detail: Option<String>,
        /// The file this call wrote, when it was a file edit (see `ToolOutput::path`).
        path: Option<std::path::PathBuf>,
    },
    Info(String),
    Warn(String),
    Error(String),
    /// The host has just attached images produced by one tool call to the conversation (see `crate::attach`).
    #[allow(dead_code)]
    ToolImages {
        tool: String,
        paths: Vec<std::path::PathBuf>,
    },
    /// The task checkpoint was just written (see crate::task) — the front end should show the plan it carries.
    Task(Box<crate::task::TaskState>),
    /// N files were moved to the system recycle bin by a host file tool.
    Recycled(usize),
    /// The host is compacting the context right now (it is about to ask the model for a summary of the oldest turns...
    ContextCompacting { used: u64, budget: u64 },
    /// The context was compacted; the request that follows is the smaller one.
    ContextCompacted { before: u64, after: u64, folded: usize, elided: usize },
}

/// The outcome of an authorisation prompt
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    Allow,
    AllowSession,
    Deny,
}

/// The user's answer to one "multiple-choice question" (`ask_user`'s return-value source).
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
#[async_trait]
pub trait Ui: Send {
    /// Read one line of user input; Ok(None) means exit (EOF or the user quitting)
    async fn read_user_input(&mut self) -> Result<Option<String>>;
    /// Show an Agent event
    fn emit(&mut self, event: AgentEvent);
    /// Ask the user for authorisation for one tool call
    async fn authorize(&mut self, tool_name: &str, args: &Value) -> AuthDecision;

    /// Ask the user one "multiple-choice question" and wait for the answer (the landing point of `ask_user`).
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
    async fn turn_usage(&mut self, _rec: &UsageRecord) {}
    /// The account for one model call inside the current turn — the same figures `turn_usage` reports, but per call...
    async fn call_usage(&mut self, _usage: &Usage, _last_prompt: Option<u64>) {}
    /// What the request about to be sent is made of: one entry per section, in body order, with a rough token estimate...
    async fn context_map(&mut self, _segs: &[(&'static str, u64)]) {}
    /// Update the in-progress status label (e.g. "Thinking", "Running fs_read").
    /// An empty default implementation; the GUI can map it to status-bar text.
    async fn set_status(&mut self, _label: &str) {}

    /// Provide a "streaming delta sink": while streaming, the Provider pushes the model's reasoning process...
    fn stream_sink(&self) -> Option<Box<dyn StreamSink + Send + Sync + '_>> {
        None
    }
}
