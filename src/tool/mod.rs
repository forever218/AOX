pub mod ask;
pub mod bash;
pub mod discover;
pub mod edit;
pub mod fingerprint;
pub mod hostfs;
pub mod job;
pub mod meta;
pub mod scope;
pub mod shell;
pub mod task;
pub mod verify;
pub mod web;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::runtime::Runtime;

/// The optional `purpose` argument: one short line saying what a call is for.
pub const PURPOSE_ARG: &str = "purpose";

/// What `purpose` is for. See PURPOSE_ARG.
pub const PURPOSE_DESC: &str = "One short line (a few words, not a sentence) saying what this \
     call is for — e.g. \"fix the title save\", \"run the test suite\", \"count files by type\". The \
     UI shows it next to the tool name, so the user can tell calls apart without opening the \
     arguments. Optional, but fill it in whenever the tool name alone does not say what you are \
     doing — which is almost always for `bash`/`shell`, whose name says nothing about the command.";

/// Names of the host's built-in file tools (same order as `hostfs::host_file_tools()`).
pub(crate) const HOST_FILE_TOOLS: [&str; 7] = [
    "read_file",
    "write_file",
    "list_dir",
    "remove",
    "attach_file",
    "edit_file",
    PUBLISH_TOOL,
];

/// `publish_file`: hands an existing file to the room instead of to the model.
pub(crate) const PUBLISH_TOOL: &str = "publish_file";

/// Which entries in that table are read-only (by index) — reads are unrestricted, only writes and deletes go...
const READ_ONLY_TOOLS: [usize; 3] = [0 /* read_file */, 2 /* list_dir */, 4 /* attach_file */];

/// The access intent of a host file tool: read-only (read a file / list a directory / attach a file) or write...
pub(crate) fn host_tool_access(name: &str) -> crate::workplace::Access {
    if READ_ONLY_TOOLS.iter().any(|i| name == HOST_FILE_TOOLS[*i]) {
        crate::workplace::Access::Read
    } else {
        crate::workplace::Access::Write
    }
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Files produced by this tool (local paths).
    pub attach: Vec<std::path::PathBuf>,
    /// How many files this call moved to the system recycle bin (0 = "this call wasn't a delete").
    pub recycled: usize,
    /// Lines added / removed by this call, when it edited a text file — `(added, removed)`.
    pub diff: Option<(usize, usize)>,
    /// The diff itself, ready to draw — one line per entry, each prefixed with `+`, `-` or a space (`⋯` marks elided...
    pub diff_detail: Option<String>,
    /// The file this call wrote, when it was a file edit (`None` for everything else).
    pub path: Option<std::path::PathBuf>,
}

impl ToolOutput {
    pub fn ok(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            is_error: false,
            attach: Vec::new(),
            recycled: 0,
            diff: None,
            diff_detail: None,
            path: None,
        }
    }

    pub fn err(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            is_error: true,
            attach: Vec::new(),
            recycled: 0,
            diff: None,
            diff_detail: None,
            path: None,
        }
    }

    /// Carry the files this tool produced. Chain it after `ok(..)` / `err(..)`.
    pub fn with_attach(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.attach = paths;
        self
    }

    /// Report that this call sent `n` files to the recycle bin. Chain it after `ok(..)`.
    pub fn with_recycled(mut self, n: usize) -> Self {
        self.recycled = n;
        self
    }

    /// Report how many lines this call **added / removed**. Chain it after `ok(..)`.
    pub fn with_diff(mut self, added: usize, removed: usize) -> Self {
        self.diff = Some((added, removed));
        self
    }

    /// Carry the diff to draw when the row is unfolded. Chain it after `with_diff(..)`.
    pub fn with_diff_detail(mut self, detail: String) -> Self {
        self.diff_detail = Some(detail);
        self
    }

    /// Name the file this call wrote.
    pub fn with_path(mut self, path: std::path::PathBuf) -> Self {
        self.path = Some(path);
        self
    }
}

#[derive(Debug, Clone)]
pub enum ToolKind {
    /// Host-provided meta tools (`create_plugin` / `update_plugin` / `list_plugins` / `remove_plugin`).
    Meta,
    /// Host-provided file tools (`read_file` / `write_file` / `list_dir` / `remove`, see `hostfs`) — the concrete form...
    Host,
    /// A tool provided by some plugin.
    Plugin(String),
}

/// The tool abstraction. Every tool (meta / plugin) is transparent to the agent loop.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn kind(&self) -> ToolKind;
    fn description(&self) -> &str;
    /// JSON Schema for the parameters (an object type).
    fn parameters(&self) -> Value;

    /// Does one call of this tool count as a read or a write (write / delete)? Only used for the workplace boundary...
    fn access(&self) -> crate::workplace::Access {
        crate::workplace::Access::Write
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": self.description(),
                "parameters": self.parameters(),
            }
        })
    }

    /// How long this call is allowed to run, in seconds — the number that gets shown to the user and that the outer...
    fn cap_hint(&self, _args: &Value) -> Option<u64> {
        None
    }

    /// The leash the outer guard in `Runtime::execute` should use.
    fn timeout_hint(&self, args: &Value) -> Option<std::time::Duration> {
        self.cap_hint(args).map(|s| std::time::Duration::from_secs(s + 5))
    }

    /// How long the model expects this call to take, in seconds — its own announcement ("this is a compile, give me...
    fn expect_hint(&self, _args: &Value) -> Option<u64> {
        None
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many host tools every doc comment in the repo quotes.
    #[test]
    fn the_quoted_host_tool_count_matches_reality() {
        const DOCUMENTED: usize = 18;
        let n = meta::meta_tools().len()
            + hostfs::host_file_tools().len()
            + ask::ask_tools().len()
            + discover::find_tools().len()
            + task::task_tools().len()
            + verify::verify_tools().len()
            + shell::shell_tools(&crate::config::ShellConfig::default()).len()
            + job::job_tools().len()
            + web::web_tools(&crate::config::WebConfig::default()).len();
        assert_eq!(n, DOCUMENTED, "update every doc that quotes the host tool count");
    }
}
