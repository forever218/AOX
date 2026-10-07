use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::attach::{self, Attachment};

/// Conversation messages, aligned directly with OpenAI chat.completions' wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    /// When an assistant initiates a tool call, `content` may be null, so this field is always serialized.
    ///
    /// **The type is `Value`, not `String`** (changed on 2026-10-05 for attachments): in the
    /// OpenAI-compatible protocol plain text is a JSON string while attachments make it a parts
    /// array, so the same field carries both shapes. Changing just this one spot is enough —
    /// `openai.rs`'s `build_body` already passes `messages` through untouched and lets serde do
    /// the serialization, so the protocol layer needs zero changes.
    ///
    /// ⚠️ **Plain text still serializes as a JSON string** (that is how [`Self::plain`] builds it),
    /// so old conversation files, old endpoints, and the no-image path stay **byte-for-byte
    /// identical** — this zero-drift guarantee is a hard requirement for this change and is pinned
    /// by `plain_text_stays_a_json_string`.
    pub content: Option<Value>,
    /// Reasoning content returned by thinking mode (DeepSeek thinking, e.g. deepseek-flash /
    /// deepseek-reasoner).
    ///
    /// ⚠️ Must be passed back to the API verbatim: as long as a "assistant message with tool_calls"
    /// exists in the history, DeepSeek requires it to be sent back together with reasoning_content,
    /// otherwise the whole request is rejected with 400:
    /// "The `reasoning_content` in the thinking mode must be passed back to the API."
    /// This is exactly what makes multi-step tasks silently break midway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// In the OpenAI protocol arguments are a JSON string
    pub arguments: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain("system", content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::plain("user", content)
    }

    /// A user message with attachments.
    ///
    /// **Attachments may only go into user messages** — the endpoint returns 400 on the spot if it
    /// sees an image in a `system` / `assistant` message (explicitly stated in the official "usage
    /// limits"). This is not a style choice but a hard protocol constraint, so this function is the
    /// sole entry point for attachments: if you want to sneak an image elsewhere, the function
    /// signature blocks it first.
    ///
    /// With no attachments it degrades to [`Self::user`] (a plain string), keeping the old path
    /// byte-for-byte identical.
    pub fn user_with_files(text: &str, files: &[Attachment]) -> Result<Self, String> {
        if files.is_empty() {
            return Ok(Self::user(text));
        }
        let body = attach::to_content(text, files)?;
        let mut m = Self::plain("user", String::new());
        m.content = Some(body);
        Ok(m)
    }

    fn plain(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(Value::String(content.into())),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn tool_result(call_id: &str, tool_name: &str, content: String) -> Self {
        Self {
            role: "tool".into(),
            content: Some(Value::String(content)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(call_id.to_string()),
            name: Some(tool_name.to_string()),
        }
    }

    /// Build an assistant message.
    ///
    /// The streamed accumulation also goes through here, so that the "streaming" and
    /// "non-streaming" paths produce exactly the same fields (in particular
    /// `reasoning_content`, which thinking models must pass back).
    pub fn assistant(
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: "assistant".into(),
            content: content.map(Value::String),
            reasoning_content: reasoning_content.filter(|r| !r.trim().is_empty()),
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            tool_call_id: None,
            name: None,
        }
    }

    /// The trimmed body text; all-whitespace counts as "no content".
    ///
    /// **Only plain strings are recognized**: a parts array from an attachment returns `None` — when
    /// a call site wants to show "what the user said" it should use [`Self::flat_text`], whose
    /// semantics are "one line of text for a human to read" rather than "the model's body". This
    /// strict semantics is kept because the main loop uses it to decide "did the model say nothing":
    /// an `assistant` response is always a string, and loosening it would treat empty parts as
    /// having said something.
    pub fn text(&self) -> Option<&str> {
        self.content.as_ref()?.as_str().map(str::trim).filter(|t| !t.is_empty())
    }

    /// Any shape of content → "a piece of text for a human to read".
    ///
    /// History rendering, `.jsonl` re-reading, and UI bubbles all go through it: a parts array
    /// concatenates the `text` blocks, and image/file blocks are swapped for placeholders like
    /// `[image]` / `[file: name]` (the attachment body itself isn't in the content; the placeholder
    /// is what that slice of history actually keeps).
    pub fn flat_text(&self) -> String {
        let Some(v) = &self.content else {
            return String::new();
        };
        if let Some(s) = v.as_str() {
            return s.to_string();
        }
        let Some(parts) = v.as_array() else {
            // Neither a string nor an array: emit the JSON as-is rather than silently showing blank
            return v.to_string();
        };
        let mut out = String::new();
        for p in parts {
            match p.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    let Some(t) = p.get("text").and_then(|t| t.as_str()) else { continue };
                    // The attachment list is added by the host, not something the user said — see `attach::ATTACH_MARK`
                    if t.starts_with(attach::ATTACH_MARK) {
                        continue;
                    }
                    out.push_str(t);
                }
                Some("image_url") => out.push_str(crate::i18n::t("[image]")),
                Some("file") => {
                    let name = p.get("filename").and_then(|f| f.as_str()).unwrap_or(crate::i18n::t("attachment"));
                    out.push_str(&crate::tf!("[file: {}]", name));
                }
                _ => {}
            }
        }
        out.trim().to_string()
    }

    /// Replaces the body text (plain text). Used when a `hook` rewrites the model's reply.
    pub fn set_text(&mut self, s: impl Into<String>) {
        self.content = Some(Value::String(s.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The key zero-drift assertion**: with no attachment, a serialised user message must be
    /// byte-for-byte identical to before the type changed — `content` is a JSON string, not
    /// `[{...}]`.
    #[test]
    fn plain_text_stays_a_json_string() {
        let v = serde_json::to_value(Message::user("hi")).unwrap();
        assert_eq!(v["content"], Value::String("hi".into()));

        let v = serde_json::to_value(Message::tool_result("c1", "read_file", "ok".into())).unwrap();
        assert_eq!(v["content"], Value::String("ok".into()));
    }

    #[test]
    fn text_is_strict_but_flat_text_is_forgiving() {
        let d = std::env::temp_dir().join("aox-msg-flattext");
        let _ = std::fs::create_dir_all(&d);
        let png = d.join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();

        let a = Attachment::probe(&png).unwrap();
        let m = Message::user_with_files("look at this", &[a]).unwrap();

        assert!(m.text().is_none(), "a parts array must not be taken for the model's text");
        let flat = m.flat_text();
        assert!(flat.contains("look at this"), "{flat}");
        assert!(flat.contains("[image]"), "{flat}");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn user_with_files_without_files_is_the_old_message() {
        let a = Message::user_with_files("hello", &[]).unwrap();
        let b = Message::user("hello");
        assert_eq!(serde_json::to_value(&a).unwrap(), serde_json::to_value(&b).unwrap());
    }
}
