//! `verify` — **host-collected evidence**, so "done" stops being a claim.
//!
//! # Why this exists
//!
//! The checkpoint (`crate::task`) records what the model believes. The one belief that matters most
//! and is most often wrong is "it works now". A model that has just written a file will happily
//! report "tests pass" without having run them, and the next session has no way to tell.
//!
//! So the host keeps its own ledger. This module runs checks the host can genuinely perform, and
//! [`crate::task::TaskState::gate`] only accepts `done` when every acceptance criterion is covered
//! by a passing, **current** check.
//!
//! # The four kinds, and why exactly these
//!
//! | Kind | Proves | Cost |
//! |---|---|---|
//! | `file_hash` | the file on disk is exactly the version you reviewed | one streamed read |
//! | `file_contains` | a specific line/text really is in the file | one bounded read |
//! | `path_exists` | a file or directory is really there | one `stat` |
//! | `output_contains` | the **last tool result** really said this | a string search |
//!
//! `output_contains` is the degraded path for command-level verification: AOX ships no shell tool,
//! so "the tests pass" can only be evidenced through whatever command plugin the user installed. The
//! host cannot run `cargo test` itself, but it *can* read the output the plugin returned — the model
//! picks the needle, the text is not the model's.
//!
//! # What this is not
//!
//! Not a test runner, and not a judge of whether the *right* thing was checked. It answers "did
//! something real happen, recently, that supports this criterion". Choosing good criteria is the
//! model's job; reviewing them is the user's.
//!
//! # Execution path
//!
//! Like `task_state`, the call is intercepted by `Agent::authorize_and_execute` **after**
//! authorization: the evidence has to be attached to *this conversation's* checkpoint, and
//! `Tool::call` only ever sees `&Runtime`. Registering it in the tool table exists solely so the
//! model can see the entry point.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::fingerprint;
use super::{Tool, ToolKind, ToolOutput};
use crate::runtime::Runtime;
use crate::task::{Check, DETAIL_MAX};
use crate::workplace::Access;

/// The tool name. Shared with `Agent::authorize_and_execute` so neither side can drift onto its own
/// literal.
pub const VERIFY_TOOL: &str = "verify";

/// The check kinds. **Machine values** — they are never translated, and they are what ends up in the
/// checkpoint row.
pub const KINDS: [&str; 4] = ["file_hash", "file_contains", "path_exists", "output_contains"];

/// `file_contains` reads the whole file, so it needs a cap. Past this the model is told to use
/// `file_hash` instead: a substring search over a file this large is not something a single check
/// should be doing, and the bot would be reading a file the model never meant to inspect.
pub const SCAN_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Occurrence counts are reported to reassure ("it is there"), not to measure — cap before counting
/// gets expensive on a pathological file.
const COUNT_MAX: usize = 999;

pub fn verify_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(VerifyTool)]
}

pub(crate) struct VerifyTool;

#[async_trait]
impl Tool for VerifyTool {
    fn name(&self) -> &str {
        VERIFY_TOOL
    }

    fn kind(&self) -> ToolKind {
        // `Host`: it changes nothing in the project (it only reads), so it belongs to the "let the
        // host do a job" tier. Concretely it follows `[auth] plugin_tools` (auto by default) — an
        // authorization dialog in the middle of "prove it works" would teach the model to skip
        // verification entirely, which is the opposite of the point.
        ToolKind::Host
    }

    fn access(&self) -> Access {
        // Read: this tool never writes, and the workplace boundary must not treat it as a write (or
        // a check of a file the model can already `read_file` would be blocked for no reason).
        Access::Read
    }

    fn description(&self) -> &str {
        crate::i18n::t("Run ONE check and record it as evidence for the task checkpoint — this is how \"done\" becomes verifiable instead of a claim. file_hash: the file is exactly the version you reviewed. file_contains: a specific text really is in the file. path_exists: a file or directory is really there. output_contains: the LAST tool result really said this (use it after a test or command tool). The host runs the check itself and records the result — you cannot write the result. criterion names the acceptance item this proves (1 = the first one). Evidence expires as soon as a file is written afterwards, so run the checks after your last edit.")
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "check": {
                    "type": "string",
                    "enum": KINDS,
                    "description": crate::i18n::t("Which check to run. file_hash needs path + expected_hash; file_contains needs path + text; path_exists needs path; output_contains needs text.")
                },
                "criterion": {
                    "type": "integer",
                    "minimum": 1,
                    "description": crate::i18n::t("The 1-based index of the acceptance criterion this check proves (1 = the first item in acceptance).")
                },
                "path": {
                    "type": "string",
                    "description": crate::i18n::t("Absolute path to check (file for file_hash / file_contains, file or directory for path_exists).")
                },
                "text": {
                    "type": "string",
                    "description": crate::i18n::t("Exact text to look for (file_contains searches the file; output_contains searches the last tool result). Case-sensitive.")
                },
                "expected_hash": {
                    "type": "string",
                    "description": crate::i18n::t("The hash read_file printed in its header for the version you reviewed.")
                }
            },
            "required": ["check", "criterion"]
        })
    }

    async fn call(&self, _args: Value, _rt: &Arc<Runtime>) -> ToolOutput {
        // Reached only when something bypassed the agent loop. Failing loudly beats pretending: a
        // caller that believes evidence was recorded would go on to trust a gate that has none.
        ToolOutput::err(crate::i18n::t(
            "verify is executed by the agent loop, not by the tool registry: the evidence belongs to a conversation's checkpoint, and this path has no conversation.",
        ))
    }
}

/// Run one check and return the piece of evidence.
///
/// `last_output` is the most recent tool result the host has seen (its tail), used by
/// `output_contains`. `epoch` is the checkpoint's current workspace epoch — the caller fills it in,
/// because only `Agent` knows which task this evidence belongs to.
///
/// `Err` means "the check could not run" (bad arguments, unreadable file). A check that ran and
/// **failed** is `Ok(Check { ok: false, .. })` — that distinction is what lets the gate say "the
/// latest check failed" instead of "there is no evidence".
pub fn run(args: &Value, last_output: Option<&str>, epoch: u64) -> Result<Check, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| crate::i18n::t("verify expects an object of arguments").to_string())?;
    let kind = obj.get("check").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if !KINDS.contains(&kind.as_str()) {
        return Err(crate::tf!(
            "check must be one of: {}",
            KINDS.join(" / ")
        ));
    }
    let criterion = obj.get("criterion").and_then(Value::as_u64).unwrap_or(0) as usize;
    if criterion == 0 {
        return Err(crate::i18n::t(
            "criterion must be the 1-based index of the acceptance item this check proves (1 = the first one) — add acceptance to the checkpoint first if you have not",
        )
        .to_string());
    }

    let at = crate::history::unix_now();
    let (target, ok, detail) = match kind.as_str() {
        "file_hash" => check_hash(obj)?,
        "file_contains" => check_contains(obj)?,
        "path_exists" => check_exists(obj)?,
        "output_contains" => check_output(obj, last_output)?,
        // `KINDS` is exhaustive, so this is unreachable; the arm exists so adding a kind forces a
        // decision here instead of silently falling through.
        _ => return Err(crate::i18n::t("unknown check").to_string()),
    };
    Ok(Check {
        criterion,
        // Left empty here on purpose: only the caller (`Agent`) can see the checkpoint's acceptance
        // list, and echoing that text is half the value of recording evidence at all.
        claim: String::new(),
        kind,
        target,
        ok,
        detail: clip(&detail, DETAIL_MAX),
        at,
        epoch,
    })
}

/// `file_hash`: is the file on disk still the version the model read?
fn check_hash(obj: &serde_json::Map<String, Value>) -> Result<(String, bool, String), String> {
    let path = arg(obj, "path")?;
    let expected = arg(obj, "expected_hash")?;
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err(crate::tf!("{} is not a file (or does not exist)", path));
    }
    let Some(actual) = fingerprint::of_file(&p) else {
        return Err(crate::tf!(
            "could not fingerprint {}: it is unreadable or larger than {} bytes",
            path,
            fingerprint::MAX_FINGERPRINT_BYTES
        ));
    };
    let want = fingerprint::normalize(&expected);
    let ok = actual == want;
    let detail = if ok {
        crate::tf!("the file on disk hashes to {}, exactly the version you reviewed", actual)
    } else {
        crate::tf!("the file on disk hashes to {}, but you expected {}", actual, want)
    };
    Ok((path, ok, detail))
}

/// `file_contains`: is this text really in the file?
fn check_contains(obj: &serde_json::Map<String, Value>) -> Result<(String, bool, String), String> {
    let path = arg(obj, "path")?;
    let text = arg(obj, "text")?;
    let p = PathBuf::from(&path);
    let meta = std::fs::metadata(&p).map_err(|e| crate::tf!("cannot read {}: {}", path, e))?;
    if !meta.is_file() {
        return Err(crate::tf!("{} is not a file", path));
    }
    if meta.len() > SCAN_MAX_BYTES {
        return Err(crate::tf!(
            "{} is {} bytes, beyond the {} byte scan limit — use file_hash for a file this large",
            path,
            meta.len(),
            SCAN_MAX_BYTES
        ));
    }
    let bytes = std::fs::read(&p).map_err(|e| crate::tf!("cannot read {}: {}", path, e))?;
    // Lossy on purpose: the check is "is this text in there", and a file with one invalid byte
    // should not turn into "the check could not run".
    let hay = String::from_utf8_lossy(&bytes);
    let n = hay.matches(&text).count();
    let ok = n > 0;
    let detail = if ok {
        crate::tf!("found {} time(s) in the file", n.min(COUNT_MAX))
    } else {
        crate::i18n::t("the text is not in the file").to_string()
    };
    Ok((path, ok, detail))
}

/// `path_exists`: is it there? A missing path is a **failed check**, not a failed run.
fn check_exists(obj: &serde_json::Map<String, Value>) -> Result<(String, bool, String), String> {
    let path = arg(obj, "path")?;
    let p = PathBuf::from(&path);
    match std::fs::metadata(&p) {
        Ok(m) if m.is_dir() => Ok((path, true, crate::i18n::t("directory exists").to_string())),
        Ok(m) => Ok((
            path,
            true,
            crate::tf!("file exists, {} bytes", m.len()),
        )),
        Err(_) => Ok((path, false, crate::i18n::t("nothing exists at this path").to_string())),
    }
}

/// `output_contains`: did the **last tool result** really say this?
///
/// The text comes from a tool the host ran, so it cannot be invented by the model — that is the only
/// reason this counts as evidence at all.
fn check_output(
    obj: &serde_json::Map<String, Value>,
    last_output: Option<&str>,
) -> Result<(String, bool, String), String> {
    let text = arg(obj, "text")?;
    let Some(out) = last_output else {
        return Err(crate::i18n::t(
            "no tool has produced output in this conversation yet — run the command first, then verify its output",
        )
        .to_string());
    };
    let ok = out.contains(&text);
    let detail = if ok {
        crate::i18n::t("the last tool result contains this text").to_string()
    } else {
        crate::i18n::t("the last tool result does NOT contain this text").to_string()
    };
    Ok((crate::i18n::t("the last tool result").to_string(), ok, detail))
}

/// One required string argument, trimmed. The error is written for the model, so it names the key.
fn arg(obj: &serde_json::Map<String, Value>, key: &str) -> Result<String, String> {
    match obj.get(key).and_then(Value::as_str).map(str::trim) {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(crate::tf!("{} is required for this check (a non-empty string)", key)),
    }
}

/// Truncate by **characters**, appending an ellipsis when it actually cut something.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aox-verify-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The strongest form: the bytes on disk are the bytes that were reviewed.
    #[test]
    fn file_hash_matches_and_mismatches() {
        let d = scratch("hash");
        let f = d.join("a.txt");
        std::fs::write(&f, "hello\n").unwrap();
        let h = fingerprint::of_bytes(b"hello\n");

        let c = run(
            &json!({"check": "file_hash", "criterion": 1, "path": f.to_string_lossy(), "expected_hash": h}),
            None,
            3,
        )
        .unwrap();
        assert!(c.ok, "{}", c.detail);
        assert_eq!(c.epoch, 3, "the caller's epoch must be carried through");
        assert_eq!(c.kind, "file_hash");

        let c = run(
            &json!({"check": "file_hash", "criterion": 1, "path": f.to_string_lossy(), "expected_hash": "0".repeat(16)}),
            None,
            0,
        )
        .unwrap();
        assert!(!c.ok, "a different hash is a failed check, not a failed run");
        assert!(c.detail.contains("expected"), "{}", c.detail);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn file_contains_counts_and_reports_a_miss() {
        let d = scratch("contains");
        let f = d.join("b.rs");
        std::fs::write(&f, "fn main() {}\nfn gate() {}\n").unwrap();
        let p = f.to_string_lossy().to_string();

        let c = run(&json!({"check": "file_contains", "criterion": 2, "path": p, "text": "fn gate"}), None, 0).unwrap();
        assert!(c.ok);
        assert_eq!(c.criterion, 2);
        let c = run(&json!({"check": "file_contains", "criterion": 1, "path": f.to_string_lossy(), "text": "fn nope"}), None, 0).unwrap();
        assert!(!c.ok);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A missing path is a real answer ("no"), so it must come back as a failed check rather than an
    /// error — otherwise the gate could not tell "it is not there" from "the check never ran".
    #[test]
    fn a_missing_path_is_a_failed_check_not_an_error() {
        let c = run(
            &json!({"check": "path_exists", "criterion": 1, "path": "Z:/definitely/not/here"}),
            None,
            0,
        )
        .unwrap();
        assert!(!c.ok);
    }

    #[test]
    fn output_contains_reads_only_what_a_tool_really_said() {
        let c = run(&json!({"check": "output_contains", "criterion": 1, "text": "test result: ok"}), Some("... test result: ok. 299 passed ..."), 0).unwrap();
        assert!(c.ok);
        let c = run(&json!({"check": "output_contains", "criterion": 1, "text": "test result: ok"}), Some("error: could not compile"), 0).unwrap();
        assert!(!c.ok);
        let e = run(&json!({"check": "output_contains", "criterion": 1, "text": "x"}), None, 0).err().unwrap();
        assert!(e.contains("no tool"), "{e}");
    }

    #[test]
    fn bad_arguments_are_explained_not_guessed() {
        let e = run(&json!({"check": "file_contains", "criterion": 1, "text": "x"}), None, 0).err().unwrap();
        assert!(e.contains("path"), "{e}");
        let e = run(&json!({"check": "nope", "criterion": 1}), None, 0).err().unwrap();
        assert!(e.contains("file_hash"), "the error must list the valid kinds: {e}");
        let e = run(&json!({"check": "path_exists"}), None, 0).err().unwrap();
        assert!(e.contains("criterion"), "{e}");
    }
}
