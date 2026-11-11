use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::attach::{self, Attachment};

/// Conversation messages, aligned directly with OpenAI chat.completions' wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    /// When an assistant initiates a tool call, `content` may be null, so this field is always serialized.
    pub content: Option<Value>,
    /// Reasoning content returned by thinking mode (DeepSeek thinking, e.g. deepseek-flash / deepseek-reasoner).
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
    /// The streamed accumulation also goes through here, so that the "streaming" and "non-streaming" paths produce...
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
    pub fn text(&self) -> Option<&str> {
        self.content.as_ref()?.as_str().map(str::trim).filter(|t| !t.is_empty())
    }

    /// History rendering, `.jsonl` re-reading, and UI bubbles all go through it: a parts array concatenates the `text`...
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

/// Whether a message list is a legal request body for an OpenAI-compatible endpoint.
pub fn is_legal_body(msgs: &[Message]) -> bool {
    msgs.iter().enumerate().all(|(i, m)| {
        if m.role != "assistant" {
            return true;
        }
        let Some(calls) = m.tool_calls.as_ref().filter(|c| !c.is_empty()) else {
            return true;
        };
        calls.iter().all(|c| {
            msgs[i + 1..]
                .iter()
                .any(|r| r.role == "tool" && r.tool_call_id.as_deref() == Some(c.id.as_str()))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool call with the given id, for building bodies by hand.
    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall { name: "fs_read".into(), arguments: "{}".into() },
        }
    }

    /// Pin the one definition of a legal boundary against the real shapes this code produces (see `agent_loop.rs`)...
    #[test]
    fn a_tool_call_without_its_result_makes_the_body_illegal() {
        let sys = Message::system("sys");
        let user = Message::user("do it");

        let done = vec![
            sys.clone(),
            user.clone(),
            Message::assistant(None, None, vec![call("c1")]),
            Message::tool_result("c1", "fs_read", "ok".into()),
        ];
        assert!(is_legal_body(&done), "a finished batch is a legal body");

        let dangling =
            vec![sys.clone(), user.clone(), Message::assistant(None, None, vec![call("c1")])];
        assert!(!is_legal_body(&dangling), "an unanswered tool call is not");

        let half = vec![
            sys.clone(),
            user.clone(),
            Message::assistant(None, None, vec![call("c1"), call("c2")]),
            Message::tool_result("c1", "fs_read", "ok".into()),
        ];
        assert!(!is_legal_body(&half), "one id out of two answered is still illegal");

        assert!(is_legal_body(&[sys, user]), "no tool call anywhere is trivially legal");
    }

    /// The key zero-drift assertion: with no attachment, a serialised user message must be byte-for-byte identical...
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
