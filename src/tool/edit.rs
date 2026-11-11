//! `edit_file` — edit a text file locally, by exact `old_text` matches, instead of overwriting it.
//! The "rewrite the whole thing" action is the most expensive error source here: the model has to emit the entire...
//! # Five hard rules (each one stands for a class of real accident).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::fingerprint;
use super::hostfs::{diff_text, line_diff};
use super::{host_tool_access, Tool, ToolKind, ToolOutput, HOST_FILE_TOOLS};
use crate::runtime::Runtime;
use crate::workplace::PATH_ARG;

/// How many edits one call may carry.
const MAX_EDITS: usize = 64;

/// The largest file `edit_file` will handle.
const EDIT_MAX_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        HOST_FILE_TOOLS[5]
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }
    /// Write: it really changes what is on disk.
    fn access(&self) -> crate::workplace::Access {
        host_tool_access(self.name())
    }
    fn description(&self) -> &str {
        crate::i18n::t("Edit an existing text file in place by replacing exact snippets (provided by the host; subject to the workspace). Each old_text must match EXACTLY ONCE in the file — zero matches or several matches reject the whole call, so include enough surrounding context to be unique. All edits are validated against the same original snapshot and applied together, or not at all. Prefer this over write_file for changing part of a file: no reformatting, no accidental drift in the lines you did not mean to touch.")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                PATH_ARG: {"type": "string", "description": crate::i18n::t("Path of the file to edit (absolute, inside the workspace)")},
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": crate::i18n::t("Edits to apply, all against the same original file content. Order does not matter; overlapping edits are rejected."),
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": {"type": "string", "description": crate::i18n::t("Exact text to replace, copied verbatim from the file (indentation included). Must occur exactly once.")},
                            "new_text": {"type": "string", "description": crate::i18n::t("Replacement text. An empty string deletes the match.")}
                        },
                        "required": ["old_text", "new_text"]
                    }
                },
                "expected_hash": {"type": "string", "description": crate::i18n::t("Optional. The hash shown in read_file's header for the version you are editing. If the file has changed since, the edit is refused instead of landing on a version you never saw.")},
                crate::tool::PURPOSE_ARG: {"type": "string", "description": crate::i18n::t(crate::tool::PURPOSE_DESC)}
            },
            "required": [PATH_ARG, "edits"]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let raw = match super::hostfs::path_arg(&args) {
            Ok(p) => p,
            Err(e) => return e,
        };
        // Write tier: goes through the workplace boundary (the same decision as write_file, with no
        // side door).
        let path = match super::hostfs::guard(rt, raw) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let edits = match parse_edits(&args) {
            Ok(e) => e,
            Err(e) => return e,
        };

        if path.is_dir() {
            return ToolOutput::err(crate::tf!(
                "{} is a directory. edit_file edits one text file; use list_dir to see what is inside it.",
                path.display()
            ));
        }
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutput::err(crate::tf!(
                    "{} does not exist. edit_file only edits existing files; use write_file to create one.",
                    path.display()
                ))
            }
            Err(e) => return ToolOutput::err(crate::tf!("cannot access {}: {}", path.display(), e)),
        };
        if meta.len() > EDIT_MAX_BYTES {
            return ToolOutput::err(crate::tf!(
                "{} is {} bytes, which is beyond edit_file's {} byte limit. Rewriting a file this large is not something a single edit should do — use write_file, or split the work.",
                path.display(),
                meta.len(),
                EDIT_MAX_BYTES
            ));
        }

        // ── Read the original: it must be UTF-8 text ── Reject strictly instead of decoding lossily: lossy turns invalid...
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(crate::tf!("failed to read {}: {}", path.display(), e)),
        };
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return ToolOutput::err(crate::tf!(
                "{} looks like a binary file (contains NUL bytes); edit_file handles UTF-8 text only.",
                path.display()
            ));
        }
        let original = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => {
                let at = e.utf8_error().valid_up_to();
                return ToolOutput::err(crate::tf!(
                    "{} is not valid UTF-8 text (first invalid byte at offset {}). edit_file will not rewrite it, because a lossy round-trip would silently destroy those bytes; convert the file to UTF-8 first, or use a plugin that handles its encoding.",
                    path.display(),
                    at
                ));
            }
        };

        // Optional version check: content only, never mtime
        let current_hash = fingerprint::of_bytes(original.as_bytes());
        if let Some(expected) = args.get("expected_hash").and_then(|v| v.as_str()) {
            let want = fingerprint::normalize(expected);
            if !want.is_empty() && want != current_hash {
                return ToolOutput::err(crate::tf!(
                    "expected_hash does not match the file's current content: you expected {}, the file is {}. The file changed since you read it, so this edit was refused; call read_file again and redo the edit against the current text.",
                    want,
                    current_hash
                ));
            }
        }

        // Validate, then splice the final content in memory
        let updated = match apply_edits(&original, &edits) {
            Ok(s) => s,
            Err(e) => {
                return ToolOutput::err(crate::tf!(
                    "no change was written to {}: {}\nThe file is untouched.",
                    path.display(),
                    e.message()
                ))
            }
        };
        if updated == original {
            // Semantically this is a successful call in which nothing happened (e.g. new_text is identical to old_text).
            return ToolOutput::ok(crate::tf!(
                "no change: {} already contains the requested text, nothing was written (hash {}).",
                path.display(),
                current_hash
            ))
            .with_path(path);
        }

        // One write: temp file in the same directory, then rename
        let capture = match rt.recovery.capture_before(&path) {
            Ok(c) => c,
            Err(b) => return ToolOutput::err(b.model),
        };
        if let Err(e) = write_atomically(&path, updated.as_bytes()) {
            let note = rt.recovery.capture_after(capture);
            return ToolOutput::err(crate::tf!(
                "failed to replace {}: {}. Nothing was written; the original file is intact.{}",
                path.display(),
                e,
                note.map(|n| format!("\nrecovery: {n}")).unwrap_or_default()
            ));
        }
        let recovery_note = rt.recovery.capture_after(capture);

        let (added, removed) = line_diff(&original, &updated);
        let new_hash = fingerprint::of_bytes(updated.as_bytes());
        let mut out = crate::tf!(
            "edited {}: {} edit(s) applied (+{} −{} lines).\nnew hash {} — pass it as expected_hash if you edit this file again.",
            path.display(),
            edits.len(),
            added,
            removed,
            new_hash
        );
        for (i, (old, new)) in edits.iter().enumerate() {
            // Validation already guaranteed `old` occurs exactly once, so searching again here is
            // safe (it only exists to report a line number).
            let line = original.find(old.as_str()).map(|at| line_of(&original, at)).unwrap_or(1);
            let old_lines = old.lines().count().max(1);
            let new_lines = new.lines().count();
            out.push_str(&crate::tf!(
                "\n  #{} at line {}: {} line(s) → {} line(s)",
                i + 1,
                line,
                old_lines,
                new_lines
            ));
        }
        if let Some(note) = recovery_note {
            out.push_str(&format!("\nrecovery: {note}"));
        }
        // Reuses `write_file`'s diff rendering, so the workspace view and the tool row display it
        // with no changes on their side.
        ToolOutput::ok(out)
            .with_diff(added, removed)
            .with_diff_detail(diff_text(&original, &updated))
            .with_path(path)
    }
}

/// An edit that has been **located**: the matched range plus the replacement text.
struct LocatedEdit {
    at: usize,
    end: usize,
    new: String,
}

/// Parse the `edits` argument. A shape error must name the entry and the field, otherwise the
/// model can only retry the whole batch.
fn parse_edits(args: &Value) -> Result<Vec<(String, String)>, ToolOutput> {
    let Some(arr) = args.get("edits").and_then(|v| v.as_array()) else {
        return Err(ToolOutput::err(crate::i18n::t(
            "missing parameter edits (must be an array of {old_text, new_text} objects)",
        )));
    };
    if arr.is_empty() {
        return Err(ToolOutput::err(crate::i18n::t(
            "edits is empty: there is nothing to change. If you meant to create or replace the whole file, use write_file.",
        )));
    }
    if arr.len() > MAX_EDITS {
        return Err(ToolOutput::err(crate::tf!(
            "too many edits in one call: {} (the limit is {}). Split them into several calls.",
            arr.len(),
            MAX_EDITS
        )));
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let n = i + 1;
        let Some(old) = item.get("old_text").and_then(|v| v.as_str()) else {
            return Err(ToolOutput::err(crate::tf!(
                "edit #{} has no old_text (it must be a string containing the exact existing text)",
                n
            )));
        };
        if old.is_empty() {
            return Err(ToolOutput::err(crate::tf!(
                "edit #{} has an empty old_text. An empty string matches everywhere, so it cannot identify a position; give the exact text to replace.",
                n
            )));
        }
        let Some(new) = item.get("new_text").and_then(|v| v.as_str()) else {
            return Err(ToolOutput::err(crate::tf!(
                "edit #{} has no new_text (use an empty string to delete the matched text)",
                n
            )));
        };
        out.push((old.to_string(), new.to_string()));
    }
    Ok(out)
}

/// Why one edit was rejected — every variant carries "what to do next", because this text is all
/// the model gets to see.
enum EditError {
    /// Edit n: zero matches
    NotFound(usize, String),
    /// Edit n: more than one match, with the line numbers that matched
    Ambiguous(usize, usize, Vec<usize>),
    /// Edits n and m have overlapping ranges
    Overlap(usize, usize),
}

impl EditError {
    fn message(&self) -> String {
        match self {
            Self::NotFound(n, sample) => crate::tf!(
                "edit #{}: old_text was not found in the file.\n  searched for: {}\nRead the file again and copy the exact text (indentation and line breaks included) — do not guess or paraphrase.",
                n,
                sample
            ),
            Self::Ambiguous(n, count, lines) => {
                let at = lines
                    .iter()
                    .take(5)
                    .map(|l| l.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                crate::tf!(
                    "edit #{}: old_text matches {} places (lines {}), so the position is ambiguous.\nInclude more surrounding lines so it matches exactly once. Nothing was written.",
                    n,
                    count,
                    at
                )
            }
            Self::Overlap(a, b) => crate::tf!(
                "edits #{} and #{} overlap: their matched ranges intersect, so the result would depend on which one is applied first. Merge them into one edit, or make them target disjoint text.",
                a,
                b
            ),
        }
    }
}

/// Validate the whole batch, then splice.
fn apply_edits(original: &str, edits: &[(String, String)]) -> Result<String, EditError> {
    let mut located: Vec<LocatedEdit> = Vec::with_capacity(edits.len());

    for (i, (old, new)) in edits.iter().enumerate() {
        let n = i + 1;
        // `match_indices` yields **non-overlapping** hits (left to right), which is the usual
        // meaning of "how many times does this occur".
        let hits: Vec<usize> = original.match_indices(old.as_str()).map(|(at, _)| at).collect();
        match hits.len() {
            0 => {
                return Err(EditError::NotFound(n, clip(old, 120)));
            }
            1 => located.push(LocatedEdit {
                at: hits[0],
                end: hits[0] + old.len(),
                new: new.clone(),
            }),
            count => {
                let lines = hits.iter().take(5).map(|at| line_of(original, *at)).collect();
                return Err(EditError::Ambiguous(n, count, lines));
            }
        }
    }

    // Overlapping ranges = the result depends on the order => reject outright rather than deciding on the model's...
    let mut order: Vec<usize> = (0..located.len()).collect();
    order.sort_by_key(|i| located[*i].at);
    for w in order.windows(2) {
        let (a, b) = (w[0], w[1]);
        if located[a].end > located[b].at {
            return Err(EditError::Overlap(a + 1, b + 1));
        }
    }

    // Replace from the back: earlier offsets are unaffected by later ones, so one pass suffices.
    let mut out = original.to_string();
    for i in order.into_iter().rev() {
        let e = &located[i];
        out.replace_range(e.at..e.end, &e.new);
    }
    Ok(out)
}

/// Byte offset -> 1-based line number (for error text and results only, never for locating).
fn line_of(src: &str, at: usize) -> usize {
    src.as_bytes()[..at.min(src.len())].iter().filter(|b| **b == b'\n').count() + 1
}

/// Flatten text to one line and cut it at `n` characters — pasting a whole source block into an
/// error only buries the point.
fn clip(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    for _ in 0..n {
        match chars.next() {
            Some(c) if c == '\n' => out.push_str("\\n"),
            Some(c) if c == '\r' => out.push_str("\\r"),
            Some(c) if c == '\t' => out.push_str("\\t"),
            Some(c) => out.push(c),
            None => return out,
        }
    }
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// Atomic replace: temp file in the same directory -> `rename` over the target.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".aox-edit-{}.tmp", std::process::id()));
    let tmp = dir.join(name);

    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // Keep the original permission bits where possible (meaningful on Unix; a harmless no-op on
    // Windows).
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// For the tests and `host_file_tools()`: install the tools into a Runtime with a fixed workplace.
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aox-edit-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// One replacement: only the matched bytes move, nothing else does (including CRLF and a
    /// missing trailing newline).
    #[test]
    fn a_single_edit_touches_only_the_match() {
        let src = "fn main() {\r\n    let value = 1;\r\n}\r\n";
        let out = apply_edits(
            src,
            &[("let value = 1;".to_string(), "let value = 2;".to_string())],
        )
        .ok()
        .expect("must match exactly once");
        assert_eq!(out, "fn main() {\r\n    let value = 2;\r\n}\r\n");
        assert!(out.contains("\r\n"), "CRLF must survive: no newline normalisation");
    }

    /// Several edits in one call, in **any order** — each is located against the same original
    /// snapshot.
    #[test]
    fn several_edits_share_one_snapshot() {
        let src = "alpha\nbeta\ngamma\n";
        let edits = vec![
            ("gamma".to_string(), "GAMMA".to_string()),
            ("alpha".to_string(), "ALPHA".to_string()),
        ];
        let out = apply_edits(src, &edits).ok().unwrap();
        assert_eq!(out, "ALPHA\nbeta\nGAMMA\n");
    }

    /// The key rule: zero matches is an error, not "skip this one"
    #[test]
    fn a_missing_snippet_is_an_error_not_a_skip() {
        let e = apply_edits("abc", &[("zzz".into(), "y".into())]).err().unwrap();
        assert!(matches!(e, EditError::NotFound(1, _)));
        assert!(e.message().contains("not found"), "{}", e.message());
    }

    /// The same `old_text` twice => ambiguous; the error must carry line numbers, or the model has
    /// no idea how much more context to add.
    #[test]
    fn two_matches_are_ambiguous_and_the_lines_are_named() {
        let src = "x = 1;\ny = 2;\nx = 1;\n";
        let e = apply_edits(src, &[("x = 1;".into(), "x = 9;".into())]).err().unwrap();
        match &e {
            EditError::Ambiguous(n, count, lines) => {
                assert_eq!((*n, *count), (1, 2));
                assert_eq!(lines, &vec![1, 3]);
            }
            _ => panic!("should be ambiguous"),
        }
        assert!(e.message().contains("ambiguous"), "{}", e.message());
    }

    /// Overlapping ranges => the result depends on the order => reject the batch and name the two
    /// edits involved.
    #[test]
    fn overlapping_edits_are_refused() {
        let src = "abcdef";
        let edits = vec![
            ("abcde".to_string(), "X".to_string()),
            ("cde".to_string(), "Y".to_string()),
        ];
        let e = apply_edits(src, &edits).err().unwrap();
        match e {
            EditError::Overlap(a, b) => assert_eq!((a, b), (1, 2)),
            other => panic!("expected overlap, got {}", other.message()),
        }
    }

    /// Adjacent but non-overlapping ranges are fine (`end == start` is not an overlap).
    #[test]
    fn touching_ranges_are_not_overlapping() {
        let out = apply_edits(
            "abc",
            &[("ab".to_string(), "X".to_string()), ("c".to_string(), "Y".to_string())],
        )
        .ok()
        .unwrap();
        assert_eq!(out, "XY");
    }

    /// An empty `new_text` deletes; an empty `old_text` is rejected at the argument layer (it
    /// matches everywhere).
    #[test]
    fn deletion_is_an_empty_replacement() {
        let out = apply_edits("a\nb\nc", &[("b\n".into(), "".into())]).ok().unwrap();
        assert_eq!(out, "a\nc");
        let e = parse_edits(&json!({"edits": [{"old_text": "", "new_text": "x"}]})).err().unwrap();
        assert!(e.content.contains("empty old_text"), "{}", e.content);
    }

    /// The same edit submitted twice: the second one matches the same place => rejected as an
    /// overlap (rather than silently applied once).
    #[test]
    fn submitting_the_same_edit_twice_is_an_overlap() {
        let e = apply_edits(
            "only",
            &[("only".to_string(), "a".to_string()), ("only".to_string(), "b".to_string())],
        )
        .err()
        .unwrap();
        assert!(matches!(e, EditError::Overlap(1, 2)));
    }

    /// Multi-byte characters: offsets are bytes, and the splice must land on character boundaries
    /// (`&str` slicing semantics guarantee that by themselves).
    #[test]
    fn multibyte_text_can_be_edited() {
        let out = apply_edits("héllo title\nbody\n", &[("héllo".into(), "hello".into())])
            .ok()
            .unwrap();
        assert_eq!(out, "hello title\nbody\n");
    }

    // Tool layer (real files)

    async fn rt_with_ws(dir: &Path) -> Arc<Runtime> {
        let mut cfg = crate::config::Config::dummy();
        cfg.workplace = crate::config::WorkplaceConfig {
            mode: "fixed".into(),
            root: Some(dir.to_string_lossy().into()),
        };
        let rt = Runtime::new(cfg, dir.to_path_buf(), dir.join("plugins")).unwrap();
        for t in super::super::hostfs::host_file_tools() {
            rt.register(t).await.unwrap();
        }
        rt
    }

    #[tokio::test]
    async fn edit_then_read_round_trip_and_diff_metadata() {
        let d = tmp("roundtrip");
        let f = d.join("a.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let rt = rt_with_ws(&d).await;

        let out = rt
            .execute(
                HOST_FILE_TOOLS[5],
                json!({PATH_ARG: f.to_string_lossy(), "edits": [{"old_text": "two", "new_text": "TWO"}]}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one\nTWO\nthree\n");
        // Same shape as write_file: counts, an expandable diff, the path that changed — none missing
        assert_eq!(out.diff, Some((1, 1)));
        assert!(out.diff_detail.as_deref().unwrap_or_default().contains("-two"), "{:?}", out.diff_detail);
        assert_eq!(out.path.as_deref(), Some(f.as_path()));
        assert!(out.content.contains("new hash"), "{}", out.content);
        // No temp file may be left behind in the directory
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("aox-edit"))
            .collect();
        assert!(leftovers.is_empty(), "temporary file left behind: {leftovers:?}");
    }

    /// `expected_hash` is the only way to say "change the version I actually read": a mismatch must
    /// be **rejected**, with a hint to read the file again.
    #[tokio::test]
    async fn a_stale_expected_hash_refuses_the_write() {
        let d = tmp("stale");
        let f = d.join("a.txt");
        std::fs::write(&f, "original\n").unwrap();
        let old_hash = fingerprint::of_bytes(b"original\n");
        // Someone else changed the file after it was read
        std::fs::write(&f, "changed by someone else\n").unwrap();
        let rt = rt_with_ws(&d).await;

        let out = rt
            .execute(
                HOST_FILE_TOOLS[5],
                json!({
                    PATH_ARG: f.to_string_lossy(),
                    "edits": [{"old_text": "original", "new_text": "mine"}],
                    "expected_hash": old_hash,
                }),
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("read_file again"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "changed by someone else\n",
            "a refused edit must leave the file untouched"
        );
    }

    /// Write only when the whole batch passes: if the second edit fails, the first must not land
    /// either. This is the only defence against a half-edited file.
    #[tokio::test]
    async fn one_bad_edit_leaves_the_whole_file_untouched() {
        let d = tmp("allornothing");
        let f = d.join("a.txt");
        std::fs::write(&f, "keep me\n").unwrap();
        let rt = rt_with_ws(&d).await;

        let out = rt
            .execute(
                HOST_FILE_TOOLS[5],
                json!({
                    PATH_ARG: f.to_string_lossy(),
                    "edits": [
                        {"old_text": "keep me", "new_text": "changed"},
                        {"old_text": "not in the file", "new_text": "x"}
                    ],
                }),
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("edit #2"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "keep me\n");
    }

    #[tokio::test]
    async fn a_missing_file_says_so_instead_of_creating_one() {
        let d = tmp("missing");
        let rt = rt_with_ws(&d).await;
        let f = d.join("nope.txt");
        let out = rt
            .execute(
                HOST_FILE_TOOLS[5],
                json!({PATH_ARG: f.to_string_lossy(), "edits": [{"old_text": "a", "new_text": "b"}]}),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("write_file"), "the fix should be named: {}", out.content);
        assert!(!f.exists());
    }

    /// Invalid UTF-8 must be **rejected**, not round-tripped lossily: that would burn the bad bytes
    /// into U+FFFD for good.
    #[tokio::test]
    async fn invalid_utf8_is_refused_rather_than_corrupted() {
        let d = tmp("utf8");
        let f = d.join("gbk.txt");
        std::fs::write(&f, [0xC4, 0xE3, 0xBA, 0xC3, b'\n']).unwrap();
        let rt = rt_with_ws(&d).await;
        let out = rt
            .execute(
                HOST_FILE_TOOLS[5],
                json!({PATH_ARG: f.to_string_lossy(), "edits": [{"old_text": "a", "new_text": "b"}]}),
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("UTF-8"), "{}", out.content);
        assert_eq!(std::fs::read(&f).unwrap(), vec![0xC4, 0xE3, 0xBA, 0xC3, b'\n']);
    }

    /// The same boundary as write_file: a write outside the workspace is blocked, and the file is
    /// genuinely untouched.
    #[tokio::test]
    async fn an_out_of_workspace_edit_is_refused() {
        let d = tmp("outside");
        let rt = rt_with_ws(&d).await;
        let outside = d.parent().unwrap().join("aox-edit-evil.txt");
        std::fs::write(&outside, "before\n").unwrap();
        let out = rt
            .execute(
                HOST_FILE_TOOLS[5],
                json!({PATH_ARG: outside.to_string_lossy(), "edits": [{"old_text": "before", "new_text": "after"}]}),
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("permission boundary"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "before\n");
        let _ = std::fs::remove_file(&outside);
    }

    /// When nothing changes (old_text == new_text): no write, no +0/−0 row, but still a success.
    #[tokio::test]
    async fn a_noop_edit_writes_nothing() {
        let d = tmp("noop");
        let f = d.join("a.txt");
        std::fs::write(&f, "same\n").unwrap();
        let before = std::fs::metadata(&f).unwrap().modified().unwrap();
        let rt = rt_with_ws(&d).await;
        let out = rt
            .execute(
                HOST_FILE_TOOLS[5],
                json!({PATH_ARG: f.to_string_lossy(), "edits": [{"old_text": "same", "new_text": "same"}]}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.diff, None, "a no-op must not claim +0/−0");
        assert_eq!(out.path.as_deref(), Some(f.as_path()));
        assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), before);
    }
}
