//! The `bash` tool: a POSIX shell for the model, with `shell` as the fallback.
//!
//! # Why it exists separately from `shell`
//!
//! Measured on this machine: `bash -c true` costs ~180 ms over a native-process floor, while
//! `powershell -NoProfile -Command exit` costs ~1100 ms. So a POSIX shell is the cheap option *and* the
//! dense one — one line of `find … | xargs … | sed …` replaces three tool round trips, and every round
//! trip is a model request.
//!
//! It is a **separate tool name** rather than a value of `shell`'s `shell` argument because the model
//! picks its syntax from the tool's name: behind `shell` it reaches for `cmd`, while a tool called
//! `bash` gets POSIX. The cost is one extra schema in the request.
//!
//! # Where the boundary is
//!
//! Identical to `shell`: the same `cwd` check, the same command scan, the same Job Object, the same
//! timeout and output handling — this file is a thin front door onto
//! [`crate::tool::shell::run_shell`]. That is deliberate: a second execution path would be a second
//! boundary to keep correct.
//!
//! # Registration
//!
//! [`bash_tools`] returns **nothing** when no bash is found: a tool that can only fail would cost the
//! model a round trip to learn what `[shell] bash` already knows. The lookup is a handful of file
//! stats — the `uname -s` validation happens on the first real call instead (see `shell::bash`).

use std::sync::Arc;
use std::time::Duration;

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
    vec![Arc::new(BashTool)]
}

/// Whether a bash was found — used by `/shell` and the prompt section to describe the machine truthfully.
pub fn available(rt: &Runtime) -> Option<crate::shell::bash::Bash> {
    crate::shell::bash::detect(&rt.config.shell, &rt.root)
}

pub(crate) struct BashTool;

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
                                    Default from config (120), hard cap 600."
                },
                "env": {
                    "type": "object",
                    "description": "Extra environment variables for this call only."
                }
            },
            "required": ["command"]
        })
    }

    fn timeout_hint(&self, args: &Value) -> Option<Duration> {
        let secs = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(600);
        Some(Duration::from_secs(secs.min(600) + 5))
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        match crate::tool::shell::run_shell(&args, rt, Some(Dialect::Bash)).await {
            Ok(out) => out,
            Err(text) => ToolOutput::err(text),
        }
    }
}
