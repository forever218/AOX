//! The `bash` tool: a POSIX shell for the model, with `shell` as the fallback.
//! # Why it exists separately from `shell`.
//! Measured on this machine: `bash -c true` costs ~180 ms over a native-process floor, while `powershell...

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::runtime::Runtime;
use crate::shell::runner::Dialect;
use crate::tool::{Tool, ToolKind, ToolOutput};

/// The tool's name (also what `[auth] shell` governs, together with `shell`).
pub const BASH_TOOL: &str = "bash";

const CWD_ARG: &str = "cwd";

/// The tools to register: one `bash`, or none at all when this machine has no functional one.
pub fn bash_tools(rt: &Runtime) -> Vec<Arc<dyn Tool>> {
    if crate::shell::bash::detect(&rt.config.shell, &rt.root).is_none() {
        return Vec::new();
    }
    vec![Arc::new(BashTool {
        timeout: rt.config.shell.timeout_secs,
        max_timeout: rt.config.shell.max_timeout_secs,
    })]
}

/// Whether a bash was found — used by `/shell` and the prompt section to describe the machine truthfully.
pub fn available(rt: &Runtime) -> Option<crate::shell::bash::Bash> {
    crate::shell::bash::detect(&rt.config.shell, &rt.root)
}

pub(crate) struct BashTool {
    /// See [`crate::tool::shell::ShellTool`] — same numbers, same reason (`timeout_hint` runs before
    /// `call` and never sees a `Runtime`).
    timeout: u64,
    max_timeout: u64,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        BASH_TOOL
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }

    fn description(&self) -> &str {
        "Run a POSIX shell command (bash) on the user's machine: pipes, globs, `$(…)`, `&&`, and \
         sed/grep/find/awk are all available, so one call does what several would. Works inside the \
         workspace: the working directory must be inside it, and writes/deletes outside it are refused \
         (reading anywhere is fine). Reach for `shell` when the command is Windows-native — a cmd \
         builtin, the PowerShell object model — or when a `C:\\` path or `/flag` must not be rewritten \
         (cmd passes both verbatim). Paths: relative to the workspace, or `E:/dir` with forward \
         slashes — that form works for the shell's own tools, for native .exe programs, and in both \
         bash flavours."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The script to run (POSIX sh/bash syntax). Multi-line is fine."
                },
                CWD_ARG: {
                    "type": "string",
                    "description": "Absolute directory to run in; must be inside the workspace. \
                                    Defaults to the workspace root."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Give up after this many seconds and kill the whole process tree. \
                                    Default from config (60), hard cap 600."
                },
                "idle_secs": {
                    "type": "integer",
                    "description": crate::tool::shell::IDLE_DESC
                },
                "expect_secs": {
                    "type": "integer",
                    "description": crate::tool::shell::EXPECT_DESC
                },
                crate::tool::PURPOSE_ARG: {
                    "type": "string",
                    "description": crate::tool::PURPOSE_DESC
                },
                "env": {
                    "type": "object",
                    "description": "Extra environment variables for this call only."
                }
            },
            "required": ["command"]
        })
    }

    fn cap_hint(&self, args: &Value) -> Option<u64> {
        crate::tool::shell::hint_cap(args, self.timeout, self.max_timeout)
    }

    fn expect_hint(&self, args: &Value) -> Option<u64> {
        crate::tool::shell::hint_expected(args, self.max_timeout)
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        match crate::tool::shell::run_shell(&args, rt, Some(Dialect::Bash)).await {
            Ok(out) => out,
            Err(text) => ToolOutput::err(text),
        }
    }
}
