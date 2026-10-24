//! `task_state` — the checkpoint entry point for long-horizon tasks.
//!
//! # Why a tool, and not something the host generates on its own
//!
//! The columns that carry the value — goal, constraints, acceptance criteria, why a decision was
//! made — are **known only to the model**: the user's intent lives in the conversation, but "this
//! constraint must not be broken" is the model's reading of the context. All the host can know by
//! itself is "which files were touched" (from real tool results), so that one column is written by
//! the host alone; every other column has to be submitted explicitly by the model.
//!
//! The host also has to audit what the model says about itself: `modified_files` — and the
//! verification status that lands with the verification work — cannot be set from here; passing
//! them is an error. With one self-reported column, a checkpoint degrades from "the facts" to "the
//! facts as the model believes them", and that is the most dangerous thing at restore time.
//!
//! # Why ordinary Q&A should never touch it
//!
//! A single question, or a single-file edit, has zero restore value; writing a line of state for it
//! only buys one more tool round trip and one more row on disk. So the tool's **own description**
//! says "only for multi-stage / multi-file / interruptible or compacted work", and the host never
//! creates a checkpoint by itself.
//!
//! # Execution path
//!
//! This tool cannot go through `Runtime::execute`: the checkpoint hangs off `Agent` (it belongs to
//! **this conversation**, not to the tool table). So, like `ask_user`, it is intercepted by
//! `Agent::authorize_and_execute` **after authorization** (see `agent::Agent::task_tool`).
//! Registering it in the tool table exists purely so "the model can see it in its tool list" —
//! nobody calls a tool they cannot see.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolKind, ToolOutput};
use crate::runtime::Runtime;

/// The tool name. `Agent::authorize_and_execute` and this module share one constant, so neither
/// side can drift onto its own literal.
pub const TASK_TOOL: &str = "task_state";

/// The host's task-checkpoint tool.
pub fn task_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(TaskStateTool)]
}

pub(crate) struct TaskStateTool;

#[async_trait]
impl Tool for TaskStateTool {
    fn name(&self) -> &str {
        TASK_TOOL
    }
    fn kind(&self) -> ToolKind {
        // `Host`: it changes nothing outside this conversation (no paths, no files), so it belongs to
        // the "let the host do a job" tier rather than the "change the host itself" one. Concretely it
        // follows `[auth] plugin_tools` (auto by default) — a checkpoint write must never pop an
        // authorization dialog, or the model learns to skip checkpoints.
        ToolKind::Host
    }
    fn description(&self) -> &str {
        crate::i18n::t("Record or update a checkpoint for a LONG, multi-step task (several stages, several files, or work that may be interrupted or have its context compacted). Write the goal, the constraints that must not be broken, the acceptance criteria, what is done, what is in progress, key decisions, what is unresolved, and the next step. Arrays REPLACE the previous value, so send the full current list each time. Do not use this for a single question or a one-file edit — a checkpoint there is pure overhead. Keep the plan live: when a step is genuinely finished, tick it immediately with `tick`, before the next step's tools; put that tick call first in the same response rather than saving several ticks for the final answer. modified_files and the verification record are maintained by the host from real tool results: gather evidence with verify, and status \"done\" is accepted only when every acceptance item has a current passing check.")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string", "description": crate::i18n::t("One sentence: what is this task achieving? Required the first time you record a checkpoint for a task.")},
                "constraints": {"type": "array", "items": {"type": "string"}, "description": crate::i18n::t("Hard rules that must not be broken (interfaces to keep, files not to touch, style or dependency limits).")},
                "acceptance": {"type": "array", "items": {"type": "string"}, "description": crate::i18n::t("How this task counts as done, in checkable terms (e.g. \"cargo test passes\", \"API unchanged\"). Each item can be proven by verify; replacing this list clears the evidence collected for the old one.")},
                "steps": {
                    "type": "array",
                    "description": crate::i18n::t("Your own plan: the ordered steps you intend to take, each ticked as you finish it. This is the only part of the checkpoint that answers \"what was I about to do next\" after the conversation is compacted, so keep it current — mark a step done in the same update as the work it describes. Replaced wholesale like every other array; a plain string means an unfinished step."),
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string", "description": crate::i18n::t("One concrete step (what you will do, not how you feel about it).")},
                            "done": {"type": "boolean", "description": crate::i18n::t("true once this step is genuinely finished.")},
                        },
                        "required": ["text"],
                    },
                },
                "tick": {
                    "type": "array",
                    "items": {"type": "integer"},
                    "description": crate::i18n::t("Mark these steps (1-based numbers) as done. This is the cheap way to record progress — one number instead of the whole list. Use it the moment a step is finished and put it first in the same response as the next step's tools; never defer several ticks to the end. A single number is accepted too."),
                },
                "untick": {
                    "type": "array",
                    "items": {"type": "integer"},
                    "description": crate::i18n::t("Mark these steps (1-based numbers) as NOT done, for a step that turned out not to be finished after all."),
                },
                "completed": {"type": "array", "items": {"type": "string"}, "description": crate::i18n::t("Stages that are genuinely finished.")},
                "in_progress": {"type": "array", "items": {"type": "string"}, "description": crate::i18n::t("What is being worked on right now.")},
                "decisions": {"type": "array", "items": {"type": "string"}, "description": crate::i18n::t("Important decisions and why (the reason is what a later you needs).")},
                "unresolved": {"type": "array", "items": {"type": "string"}, "description": crate::i18n::t("Open questions, known obstacles, things that did not work.")},
                "next_steps": {"type": "array", "items": {"type": "string"}, "description": crate::i18n::t("What to do next, concretely.")},
                "status": {"type": "string", "enum": ["done", "open"], "description": crate::i18n::t("\"done\" asks the host to accept the task; it is refused unless every acceptance item has a current passing check (see verify). \"open\" reopens a task that turned out not to be finished.")},
                "reset": {"type": "boolean", "description": crate::i18n::t("Discard the previous checkpoint and start a new task (requires goal).")}
            },
            "required": []
        })
    }

    async fn call(&self, _args: Value, _rt: &Arc<Runtime>) -> ToolOutput {
        // Reached only when something bypassed the agent loop (`Runtime::execute` called directly).
        // There is no conversation to attach a checkpoint to in that case, and silently succeeding
        // would be the worst answer: the caller would believe a checkpoint exists.
        ToolOutput::err(crate::i18n::t(
            "task_state is executed by the agent loop, not by the tool registry: a checkpoint belongs to a conversation, and this path has none.",
        ))
    }
}
