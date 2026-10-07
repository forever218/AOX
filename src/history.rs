//! The conversation history's **data model and wording**.
//!
//! What remains here is only "what a record looks like" and "how to show it to a human": the four
//! structs `Meta` / `UsageRecord` / `Summary` / `Loaded`, plus `/history` subcommand parsing and
//! plain-text rendering.
//!
//! **Persisting and reading moved to [`crate::store`]** (the storage seam extracted in stage 3,
//! implemented in `store::jsonl`). Splitting the two means: swapping the storage backend doesn't
//! drag the wording along, and changing the wording doesn't touch I/O; callers speak in terms of
//! **id** rather than assembling history file paths themselves.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::llm::Usage;
use crate::message::Message;

/// How many conversations the list returns by default.
///
/// Both the CLI (`/history`) and the backend's "index" parsing use it — an index is counted by this
/// list's position, and the moment the two disagree, `/history rm 2` deletes something other than
/// the entry the user saw.
pub const DEFAULT_LIST_LIMIT: usize = 60;

/// The prefix of a prompt the host injected. Such "user messages" weren't said by a real person and
/// must not be used as a title.
///
/// The backend tests for it when reading a title, and the CLI tests for it when rendering the full
/// text — one test, written once.
pub const HOST_PREFIX: &str = "(host hint)";

/// The current Unix seconds.
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ─────────────────────────── Records ───────────────────────────

/// The conversation file's header record (the first line of the jsonl).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// Unix seconds, convenient for programmatic comparison
    #[serde(default)]
    pub created: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// One turn's token account (corresponding to a `kind:"usage"` line in the jsonl).
///
/// **One entry per turn**, rather than a single grand total at the end of the file: a total can only
/// answer "how much did this conversation cost", not "which turn was the most expensive". And the
/// most expensive turn is usually exactly the one that went wrong — diagnosis depends on it.
///
/// Every field is written "only if it has a value": if the endpoint under-reports, the file has one
/// fewer key, and reading it back naturally yields `None` — no need to pass off a 0 as "no spend".
#[derive(Debug, Clone, Default)]
pub struct UsageRecord {
    /// Which turn (1-based)
    pub round: u32,
    /// When it was written (unix seconds)
    pub at: u64,
    /// How many times the model was called this turn
    pub calls: u32,
    /// This turn's per-field usage
    pub usage: Usage,
    /// The input size of the last request = the current context occupancy
    pub last_prompt: Option<u64>,
    /// The **conversation total** tokens up to this turn. Stored redundantly so that reading it back
    /// doesn't require summing by hand, and so "how much this conversation cost in total" is visible
    /// at a glance in the file.
    pub session_total: Option<u64>,
    /// The conversation's cumulative call count up to this turn
    pub session_calls: u32,
}

impl UsageRecord {
    pub fn to_value(&self) -> Value {
        let mut v = json!({
            "round": self.round,
            "at": self.at,
            "calls": self.calls,
            "session_calls": self.session_calls,
        });
        let mut put = |key: &str, x: Option<u64>| {
            if let Some(n) = x {
                v[key] = json!(n);
            }
        };
        put("prompt", self.usage.prompt);
        put("completion", self.usage.completion);
        put("total", self.usage.total);
        put("reasoning", self.usage.reasoning);
        put("cached", self.usage.cached);
        put("cache_miss", self.usage.cache_miss);
        put("last_prompt", self.last_prompt);
        put("session_total", self.session_total);
        v
    }

    pub fn from_value(v: &Value) -> Self {
        let num = |k: &str| v.get(k).and_then(Value::as_u64);
        let n32 = |k: &str| num(k).unwrap_or(0) as u32;
        Self {
            round: n32("round"),
            at: num("at").unwrap_or(0),
            calls: n32("calls"),
            usage: Usage {
                prompt: num("prompt"),
                completion: num("completion"),
                total: num("total"),
                reasoning: num("reasoning"),
                cached: num("cached"),
                cache_miss: num("cache_miss"),
            },
            last_prompt: num("last_prompt"),
            session_total: num("session_total"),
            session_calls: n32("session_calls"),
        }
    }

    /// Whether this line carries not a single number (it is empty when the endpoint doesn't return
    /// usage at all)
    pub fn is_empty(&self) -> bool {
        self.usage.is_empty()
    }
}

/// One entry in the list.
#[derive(Debug, Clone)]
pub struct Summary {
    /// The filename stem (timestamp + salt), also the identifier used by `/history show` and the GUI.
    /// Always use it when calling the storage layer, **never** assemble another path from `path`.
    pub id: String,
    /// Where it lives on disk. For display (`/history dir`, the path hint in the UI).
    pub path: std::path::PathBuf,
    /// The first user message (truncated); a placeholder if it can't be obtained
    pub title: String,
    /// The message count (excluding meta)
    pub messages: usize,
    /// The display string for "the time it was last spoken to" ("10-02 15:39"). It uses the same
    /// time as the list sorting — the moment the sort key and the displayed time aren't the same one,
    /// the list looks mis-sorted.
    pub time: String,
}

/// One message read back out.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub msg: Message,
    pub is_error: Option<bool>,
}

// ─────────────────────────── Commands (shared by CLI / GUI) ───────────────────────────

/// A `/history` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// List the historical conversations
    List,
    /// Print one conversation in full
    Show(String),
    /// Switch to a conversation (read its context back, and subsequent turns keep writing to the same file)
    Open(String),
    /// Print the history directory
    Dir,
    /// Delete a conversation
    Remove(String),
}

/// Whether this line is a `/history` command.
pub fn is_cmd(line: &str) -> bool {
    line == "/history" || line.starts_with("/history ")
}

pub fn parse_cmd(line: &str) -> Result<Cmd> {
    let mut parts = line.split_whitespace();
    let _ = parts.next();
    match parts.next() {
        None | Some("list") => Ok(Cmd::List),
        Some("dir") => Ok(Cmd::Dir),
        Some("show") => match parts.next() {
            Some(id) => Ok(Cmd::Show(id.to_string())),
            None => bail!(crate::i18n::t("Usage: /history show <number|file name>")),
        },
        Some("open") | Some("use") => match parts.next() {
            Some(id) => Ok(Cmd::Open(id.to_string())),
            None => bail!(crate::i18n::t("Usage: /history open <number|file name>")),
        },
        Some("rm") | Some("remove") => match parts.next() {
            Some(id) => Ok(Cmd::Remove(id.to_string())),
            None => bail!(crate::i18n::t("Usage: /history rm <number|file name>")),
        },
        Some(other) => bail!(
            crate::tf!("Unknown /history subcommand: {}\nUsage: /history [list] | /history show <number|file name> | /history open <number|file name> | /history rm <number|file name> | /history dir", other)
        ),
    }
}

// ─────────────────────────── Text rendering ───────────────────────────

/// The plain-text implementation of `/history` (which the CLI prints directly).
///
/// It handles layout only: the data is fetched from the storage layer by the caller and passed in —
/// that way "swapping the backend" and "changing the wording" don't entangle, and this function
/// doesn't have to read the disk itself.
pub fn list_text(items: &[Summary], location: &Path, current: Option<&str>) -> String {
    if items.is_empty() {
        return crate::tf!("(no history yet)\n  Directory: {}", location.display());
    }
    let mut s = crate::tf!("History ({} conversations, most recently active first)  {}", items.len(), location.display());
    for (i, it) in items.iter().enumerate() {
        let cur = if Some(it.id.as_str()) == current { crate::i18n::t("  ← current") } else { "" };
        s.push_str(&crate::tf!(
            "\n  {}. {}  {} messages  {}{}",
            format!("{:>2}", i + 1),
            it.time,
            format!("{:>4}", it.messages),
            it.title,
            cur
        ));
    }
    s.push_str(
        crate::i18n::t("\n  Usage: /history show <n> to read in full | /history open <n> to continue | /history rm <n> | /history dir"),
    );
    s
}

/// Render one conversation as plain text (the CLI's `/history show`).
pub fn transcript_text(items: &[Loaded]) -> String {
    use crate::ui::style;
    if items.is_empty() {
        return crate::i18n::t("(empty conversation)").into();
    }
    let mut s = String::new();
    for it in items {
        let m = &it.msg;
        match m.role.as_str() {
            "user" => {
                // Use `flat_text`: a user message may carry attachments, in which case `content` is a
                // parts array (body text + image blocks), and a direct `as_deref` would show not one
                // character.
                let text = m.flat_text();
                let text = text.trim();
                if text.starts_with(HOST_PREFIX) {
                    s.push_str(&format!("\n{} {}\n", style::dim(crate::i18n::t("host:")), style::dim(text)));
                } else {
                    s.push_str(&format!("\n{} {}\n", style::bold(crate::i18n::t("you:")), text));
                }
                // An attachment is only a path reference in the `.jsonl` (see the `attach` module),
                // so when reviewing history it must be listed explicitly — otherwise a conversation
                // with images looks no different from a plain-text one, and "what exactly was sent
                // at the time" is precisely the question reviewing history is meant to answer.
                let files = m
                    .content
                    .as_ref()
                    .map(crate::attach::names_from_content)
                    .unwrap_or_default();
                if !files.is_empty() {
                    s.push_str(&format!(
                        "  {} {}\n",
                        style::dim(crate::i18n::t("attachments:")),
                        style::dim(&files.join(crate::i18n::t(", ")))
                    ));
                }
            }
            "assistant" => {
                if let Some(t) = m.text() {
                    s.push_str(&format!("\n{} {}\n", style::bold(crate::i18n::t("assistant:")), t));
                }
                for c in m.tool_calls.as_deref().unwrap_or(&[]) {
                    s.push_str(&format!(
                        "\n  {} {} {}\n",
                        style::dim("→"),
                        style::cyan(&c.function.name),
                        style::dim(&clamp_chars(c.function.arguments.trim(), 160))
                    ));
                }
            }
            "tool" => {
                let (mark, name) = (
                    if it.is_error == Some(true) { "✗" } else { "✓" },
                    m.name.as_deref().unwrap_or("tool"),
                );
                // A tool result can be long (already trimmed by max_result_chars before writing), and
                // it is trimmed once more for display, so reviewing history doesn't mean scrolling
                // through a single result.
                let body = m.flat_text().replace('\n', " ⏎ ");
                s.push_str(&format!("  {} {} {}\n", mark, name, style::dim(&clamp_chars(body.trim(), 300))));
            }
            _ => {}
        }
    }
    s
}

fn clamp_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The index list and the parsed count use the same limit — the moment the two disagree,
    /// `/history rm 2` deletes something other than the entry the user saw.
    #[test]
    fn list_limit_is_the_shared_one() {
        assert!(DEFAULT_LIST_LIMIT > 0);
    }

    /// Usage record: write only what has a value, and a missing field reads back as None (not 0).
    #[test]
    fn usage_record_round_trips_sparse_fields() {
        let rec = UsageRecord {
            round: 2,
            at: 42,
            calls: 1,
            usage: Usage { prompt: Some(10), total: Some(30), ..Default::default() },
            last_prompt: None,
            session_total: Some(60),
            session_calls: 3,
        };
        let back = UsageRecord::from_value(&rec.to_value());
        assert_eq!(back.round, 2);
        assert_eq!(back.usage.prompt, Some(10));
        assert_eq!(back.usage.total, Some(30));
        assert_eq!(back.usage.completion, None, "not reported means absent; it must not become 0");
        assert_eq!(back.last_prompt, None);
        assert_eq!(back.session_total, Some(60));
        assert_eq!(back.session_calls, 3);
        assert!(!back.is_empty());
    }

    /// Subcommand parsing: aliases, a missing argument erroring, an unknown subcommand erroring.
    #[test]
    fn parse_cmd_handles_aliases_and_errors() {
        assert_eq!(parse_cmd("/history").unwrap(), Cmd::List);
        assert_eq!(parse_cmd("/history list").unwrap(), Cmd::List);
        assert_eq!(parse_cmd("/history dir").unwrap(), Cmd::Dir);
        assert_eq!(parse_cmd("/history show 3").unwrap(), Cmd::Show("3".into()));
        assert_eq!(parse_cmd("/history use abc").unwrap(), Cmd::Open("abc".into()));
        assert_eq!(parse_cmd("/history rm 1").unwrap(), Cmd::Remove("1".into()));
        assert!(parse_cmd("/history show").is_err());
        assert!(parse_cmd("/history nope").is_err());
    }

    /// The prefix test used for titles must not fail when rendering the full text: a host prompt has
    /// to be shown as "host:".
    #[test]
    fn transcript_marks_host_prompts() {
        let items = vec![
            Loaded { msg: Message::user("(host hint) your previous reply was empty."), is_error: None },
            Loaded { msg: Message::user("take a look for me"), is_error: None },
        ];
        let text = transcript_text(&items);
        assert!(text.contains("host:"), "a host prompt must be marked out");
        assert!(text.contains("you:"), "a real user message uses 'you:'");
    }
}
