//! The "you already asked that" guard.
//! The expensive failure mode this exists for: a model that cannot make progress, so it reads the same file again...
//! # Why the rule is not "same tool + same args".

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::message::Message;
use crate::runtime::Runtime;
use crate::tool::ToolKind;
use crate::workplace::Access;

/// One recorded read.
struct Seen {
    /// The result as it was pushed into the conversation (already capped), for the "is it still
    /// there" check.
    content: String,
    /// [`Guard::state`] at the moment it was recorded
    state: u64,
}

/// Per-turn state. Rebuilt for every turn: "repeated" means "within this turn", never across turns
/// (the user may well ask the same question twice on purpose).
#[derive(Default)]
pub struct Guard {
    seen: HashMap<(String, String), Seen>,
    /// Bumped by every call that is **not** a read
    state: u64,
}

impl Guard {
    /// Is this call a read-only host tool?
    pub async fn eligible(rt: &Arc<Runtime>, name: &str) -> bool {
        // `web_fetch` is a read by the workplace's definition (it touches no model-chosen path), but "the same URL twice...
        if name == crate::tool::web::WEB_TOOL {
            return false;
        }
        matches!(rt.tool_kind(name).await, Some(ToolKind::Host)) && rt.tool_access(name).await == Access::Read
    }

    /// Would running this call again give back exactly what the conversation already holds?
    pub fn repeats(&self, name: &str, args: &Value, msgs: &[Message]) -> bool {
        let Some(prev) = self.seen.get(&key(name, args)) else {
            return false;
        };
        if prev.state != self.state {
            return false; // the world moved: a write, a delete, a command
        }
        // The pointer is only honest while the result is really still in the request.
        let needle = prev.content.trim();
        msgs.iter().any(|m| m.role == "tool" && m.text() == Some(needle))
    }

    /// Record a read's result (the text that actually went into the conversation).
    pub fn record(&mut self, name: &str, args: &Value, content: &str) {
        self.seen.insert(key(name, args), Seen { content: content.to_string(), state: self.state });
    }

    /// Something that is not a read ran — its side effects are unknown, so nothing recorded before
    /// it can be trusted to still describe the world.
    pub fn touched(&mut self) {
        self.state += 1;
    }
}

fn key(name: &str, args: &Value) -> (String, String) {
    // `to_string` on a `Value` normalises key order (serde_json's map is a BTreeMap), so two calls
    // that differ only in the order the model wrote its arguments still compare equal.
    (name.to_string(), serde_json::to_string(args).unwrap_or_default())
}

/// What the model gets instead of the repeated output.
pub fn notice() -> String {
    crate::i18n::t(
        "identical to the call above (same tool, same arguments) and nothing has been written since, so the result has not changed — it is already in this conversation. Do not call it again; if you were retrying a call that failed, say what failed instead of repeating it.",
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(name: &str, content: &str) -> Message {
        Message::tool_result("call-1", name, content.to_string())
    }

    /// The conversation holding the previous result — the guard's third condition.
    fn msgs(content: &str) -> Vec<Message> {
        vec![Message::user("go"), read("read_file", content)]
    }

    const RESULT: &str = "fn main() {}\n";

    #[test]
    fn a_read_repeated_with_nothing_in_between_is_caught() {
        let args = json!({"path": "a.txt"});
        let mut g = Guard::default();
        g.record("read_file", &args, RESULT);
        assert!(g.repeats("read_file", &args, &msgs(RESULT)));
    }

    #[test]
    fn a_write_in_between_clears_the_verdict() {
        // This is the measured case: re-running an identical command after the file it reads was
        // edited. The arguments are the same; the answer is not.
        let args = json!({"path": "a.txt"});
        let mut g = Guard::default();
        g.record("read_file", &args, RESULT);
        g.touched();
        assert!(!g.repeats("read_file", &args, &msgs(RESULT)));
    }

    #[test]
    fn a_result_that_was_trimmed_away_clears_the_verdict() {
        // Trimming/compaction can drop the earlier result between the two calls; pointing at "the
        // result above" would then be false.
        let args = json!({"path": "a.txt"});
        let mut g = Guard::default();
        g.record("read_file", &args, RESULT);
        assert!(!g.repeats("read_file", &args, &msgs("something else entirely")));
    }

    #[test]
    fn different_arguments_or_a_different_tool_are_not_a_repeat() {
        let mut g = Guard::default();
        g.record("read_file", &json!({"path": "a.txt"}), RESULT);
        assert!(!g.repeats("read_file", &json!({"path": "b.txt"}), &msgs(RESULT)));
        assert!(!g.repeats("list_dir", &json!({"path": "a.txt"}), &msgs(RESULT)));
    }

    #[test]
    fn argument_order_does_not_make_a_different_call() {
        let mut g = Guard::default();
        g.record("read_file", &json!({"path": "a.txt", "start": 1}), RESULT);
        assert!(g.repeats("read_file", &json!({"start": 1, "path": "a.txt"}), &msgs(RESULT)));
    }
}
