//! `ask_user` — lets the model hand a "multiple-choice question" to the interface, instead of writing...
//! # Why it is needed.
//! When the model wants the user to make a decision, it naturally writes a line like "do you pick 1 or 2?"...

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolKind, ToolOutput};
use crate::runtime::Runtime;
use crate::ui::{ChoiceReply, Ui};

/// The tool name. The Agent's special case and this spot reference the same constant — so a rename cannot produce...
pub const ASK_TOOL: &str = "ask_user";

/// The maximum number of options at once.
pub const MAX_OPTIONS: usize = 6;

pub fn ask_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(AskUser)]
}

pub(crate) struct AskUser;

#[async_trait]
impl Tool for AskUser {
    fn name(&self) -> &str {
        ASK_TOOL
    }
    /// Report `Host`: it is a capability built into the host (the same class as the file tools), not a plugin...
    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }
    fn description(&self) -> &str {
        crate::i18n::t("Ask the user to pick one of a few options and **wait for the answer** (this pauses the current round). Use it when the user must make a decision — writing \"do you choose 1 or 2?\" in the body text is useless: that is plain text, the user has to type it themselves, and nothing guarantees what they type matches your numbering. Options are numbered automatically, and the UI adds a \"type your own\" entry. The return value is the option the user picked, or a paragraph they typed themselves (that is a valid answer too — do not ask the same question again because of it).")
    }
    fn parameters(&self) -> Value {
        parameters()
    }
    async fn call(&self, _args: Value, _rt: &Arc<Runtime>) -> ToolOutput {
        // Under normal circumstances this is unreachable (see the module docs).
        ToolOutput::err(crate::tf!(
            "{} must be handled by the host's main loop (it needs the UI handed in) and cannot be executed directly",
            ASK_TOOL
        ))
    }
}

fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "question": {
                "type": "string",
                "description": crate::i18n::t("One sentence to ask the user. Make clear what is being chosen (e.g. \"which basis should this report be computed on?\")")
            },
            "options": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 2,
                "maxItems": MAX_OPTIONS,
                "description": crate::i18n::t("Candidate answers, 2-6 of them. **Write the body only, do not number them yourself** — the host adds A/B/C…. Each option must stand on its own; do not write \"same as above\" or \"pick the first one\"")
            }
        },
        "required": ["question", "options"]
    })
}

/// Split `ask_user`'s arguments into `(question, options)`. Errors are phrased for the model to read.
pub fn parse(args: &Value) -> Result<(String, Vec<String>), String> {
    let question = args
        .get("question")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| crate::tf!("{} is missing question (give the user a question they can understand)", ASK_TOOL))?
        .to_string();

    let raw = args
        .get("options")
        .and_then(|v| v.as_array())
        .ok_or_else(|| crate::tf!("{}'s options must be an array of strings", ASK_TOOL))?;

    let mut options: Vec<String> = Vec::with_capacity(raw.len());
    for o in raw {
        let s = o
            .as_str()
            .ok_or_else(|| crate::tf!("{}'s options contains a non-string entry: {}", ASK_TOOL, o))?
            .trim()
            .to_string();
        if s.is_empty() {
            return Err(crate::tf!("{}'s options contains an empty entry — remove it and call again", ASK_TOOL));
        }
        options.push(s);
    }
    if options.len() < 2 {
        return Err(crate::tf!(
            "{} needs at least two options ({} given). If there is only one possibility there is nothing to ask — just do it",
            ASK_TOOL,
            options.len()
        ));
    }
    if options.len() > MAX_OPTIONS {
        // Truncate rather than error: a few extra options aren't a mistake, they just don't fit on screen.
        options.truncate(MAX_OPTIONS);
    }
    Ok((question, options))
}

/// Letters rather than digits: the model often writes its own numbers in the body ("1. 2. 3."), and when the user...
pub fn letter(i: usize) -> char {
    (b'A' + (i % 26) as u8) as char
}

/// Read a line the user typed as "which option". Returns `None` when it can't be recognised.
pub fn as_choice(s: &str, n: usize) -> Option<usize> {
    let t = s.trim().trim_end_matches(['.', '。', ')', '）', '、', ':']);
    if t.chars().count() != 1 {
        return None;
    }
    let c = t.chars().next()?;
    if let Some(i) = letter_index(c) {
        return (i < n).then_some(i);
    }
    let d = c.to_digit(10)? as usize;
    // Digits are read as **1-based** (what the user sees is "1. 2. 3."); out of range means
    // unrecognised, and it goes down the free-form-answer path
    (d >= 1 && d <= n).then(|| d - 1)
}

fn letter_index(c: char) -> Option<usize> {
    let u = c.to_ascii_uppercase();
    u.is_ascii_uppercase().then(|| (u as u8 - b'A') as usize)
}

/// Ask the user once. Called by the Agent wherever it has a `Ui` (see the module docs).
pub async fn interact(args: Value, ui: &mut dyn Ui) -> ToolOutput {
    let (question, options) = match parse(&args) {
        Ok(v) => v,
        Err(e) => return ToolOutput::err(e),
    };
    match ui.choose(&question, &options).await {
        ChoiceReply::Picked(i) => match options.get(i) {
            // Hand back "label + original text" together: when the model later echoes the choice, the
            // user can match it up.
            Some(text) => ToolOutput::ok(crate::tf!("the user chose {}. {}", letter(i), text)),
            None => ToolOutput::err(crate::i18n::t("the UI returned a non-existent option index (internal error); ask again")),
        },
        ChoiceReply::Text(t) => {
            // It deliberately spells out "this is a valid answer too": the model easily reads "the user didn't click...
            ToolOutput::ok(crate::tf!("the user did not click an option and answered directly: {}", t))
        }
        ChoiceReply::Cancelled => ToolOutput::err(
            crate::i18n::t("The user gave no answer (they closed the options, or the UI dropped). Do not ask the same question again: either continue with your own default judgment and state clearly that you went with X, or leave the decision to the user's next message."),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_normal_question() {
        let args = json!({"question": "  Which basis for the stats? ", "options": [" left ", "right"]});
        let (q, o) = parse(&args).unwrap();
        assert_eq!(q, "Which basis for the stats?", "leading/trailing whitespace must be trimmed");
        assert_eq!(o, vec!["left".to_string(), "right".to_string()]);
    }

    /// An empty option must be an error, never silently swallowed: after swallowing, the model receives an option slot...
    #[test]
    fn empty_or_non_string_options_are_rejected_not_swallowed() {
        let e = parse(&json!({"question": "q", "options": ["a", "  "]})).unwrap_err();
        assert!(e.contains("an empty entry"), "{e}");
        let e = parse(&json!({"question": "q", "options": ["a", 1]})).unwrap_err();
        assert!(e.contains("a non-string entry"), "{e}");
    }

    #[test]
    fn a_single_option_is_not_a_question() {
        let e = parse(&json!({"question": "q", "options": ["the only one"]})).unwrap_err();
        assert!(e.contains("at least two options"), "{e}");
    }

    #[test]
    fn missing_question_is_an_error() {
        let e = parse(&json!({"options": ["a", "b"]})).unwrap_err();
        assert!(e.contains("question"), "{e}");
    }

    /// Exceeding the limit is **truncation**, not an error: a few extra ones aren't a mistake, they
    /// just don't fit on screen.
    #[test]
    fn too_many_options_are_truncated_not_rejected() {
        let opts: Vec<String> = (0..9).map(|i| format!("option {i}")).collect();
        let (_, o) = parse(&json!({"question": "q", "options": opts})).unwrap();
        assert_eq!(o.len(), MAX_OPTIONS);
        assert_eq!(o[0], "option 0", "truncation must keep the first few, not drop them randomly");
    }

    #[test]
    fn letters_start_at_a() {
        assert_eq!(letter(0), 'A');
        assert_eq!(letter(1), 'B');
        assert_eq!(letter(5), 'F');
    }

    /// Recognising a label must be **precise**, never greedy: recognising wrongly costs the model
    /// carrying on with a choice the user never made.
    #[test]
    fn choice_parsing_accepts_letters_and_numbers_but_nothing_fuzzy() {
        let n = 3;
        for (input, want) in [
            ("A", 0usize),
            ("a", 0),
            ("b", 1),
            ("C", 2),
            ("B.", 1),
            ("c)", 2),
            ("  1  ", 0),
            ("2.", 1),
            ("3、", 2),
        ] {
            assert_eq!(as_choice(input, n), Some(want), "{input:?} should be recognised as item {want}");
        }
        // These **should not** be recognized as options — they should be handed back to the model verbatim as free-form answers
        for input in ["", "ab", "A4 paper", "the 1st", "A B", "yes", "0", "4"] {
            assert_eq!(as_choice(input, n), None, "{input:?} must not be recognised as an option");
        }
    }
}
