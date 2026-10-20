//! The host's built-in **file tools** — the concrete form of sandbox approach ③.
//!
//! Why the host provides them itself instead of letting the model keep writing
//! `fs` plugins:
//!
//! A file plugin the model wrote itself **can only be self-disciplined inside its
//! own code**; the host cannot see what path it used to do what. These tools, by
//! contrast, keep both parameters and implementation in the host's hands — so
//! "the path must first pass the workplace check" **cannot be bypassed** (unless
//! the model simply doesn't use them and writes its own plugin instead, which is
//! a different class of risk; see the threat model in `docs/SANDBOX.md`).
//!
//! The check happens in two places, deliberately in depth:
//! 1. `Agent::authorize_and_execute` — the single funnel for all tool calls,
//!    **before authorization**. Intercepting there has the bonus of being able to
//!    send the user a `Warn` (tools have no access to `Ui`).
//! 2. The start of each tool's own `call` — guards against someone bypassing the
//!    loop and calling `Runtime::execute` directly (`selftest` does exactly that).
//!    Without this layer the check would be empty on that path.
//!
//! Both places go through the **same check** in the `workplace` module (reading
//! uses `check_read`, writing/removing use `check`; the split is described in
//! [`crate::workplace::Access`]) and the same wording
//! ([`crate::workplace::Reject`]), so there is no "two sets of rules".
//!
//! **Reading is unrestricted, writing/removing are not** (semantics settled on
//! 2026-10-04): `read_file` / `list_dir` / `attach_file` only normalize the path
//! and open the file — the whole device is readable; only `write_file` / `remove`
//! actually cross the workplace boundary.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::edit::EditFileTool;
use super::fingerprint;
use super::host_tool_access;
use super::Tool;
use super::ToolKind;
use super::ToolOutput;
use super::HOST_FILE_TOOLS;
use crate::runtime::Runtime;
use crate::workplace::PATH_ARG;

/// The host file tools.
///
/// `edit_file` is last because [`HOST_FILE_TOOLS`] is addressed by index (see the warning there);
/// putting a new tool at the end leaves every existing index — and therefore the read/write split —
/// exactly where it was.
pub fn host_file_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFileTool),
        Arc::new(WriteFileTool),
        Arc::new(ListDirTool),
        Arc::new(RemoveTool),
        Arc::new(AttachFileTool),
        Arc::new(EditFileTool),
    ]
}

/// Per-call byte cap when reading a file. Beyond it, truncate and **say so
/// explicitly** — silently truncating would make the model draw conclusions from
/// partial content, the worst kind of "silent failure".
const READ_LIMIT: usize = 200_000;
/// Entry cap when listing a directory, for the same reason.
const LIST_LIMIT: usize = 300;

/// Check a `path` argument for a **write to disk** (write / remove / edit), and unify
/// "out of bounds" into the contract text handed to the model.
pub(crate) fn guard(rt: &Arc<Runtime>, raw: &str) -> Result<std::path::PathBuf, ToolOutput> {
    rt.workplace.check(raw).map_err(|r| ToolOutput::err(r.model_text()))
}

/// Check a `path` argument for a **read-only** operation (read file / list dir).
///
/// Per [`crate::workplace::Access::Read`], reading is **not restricted by the
/// workplace**, so this never returns an error — it just normalizes the path and
/// hands it back. **Deliberately written separately from [`guard`]**: it makes
/// "read and write are not the same tier" visible at a glance at the call site —
/// if the two shared one `guard`, the day someone puts a boundary on reads too,
/// the code would show no change at all.
fn guard_read(rt: &Arc<Runtime>, raw: &str) -> std::path::PathBuf {
    // `check_read` is always `Ok` (reads are never refused); and even an unexpected
    // failure must not panic, so fall back to the raw path.
    rt.workplace.check_read(raw).unwrap_or_else(|_| std::path::PathBuf::from(raw))
}

/// Fetch the required `path` argument.
pub(crate) fn path_arg(args: &Value) -> Result<&str, ToolOutput> {
    args.get(PATH_ARG)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ToolOutput::err(crate::tf!("missing parameter {} (must be a string path)", PATH_ARG)))
}

// ---------------------------------------------------------------- read_file

pub(crate) struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        HOST_FILE_TOOLS[0]
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }
    /// Read / write are split by **name** (see `host_tool_access`) — these tools'
    /// schemas are ones we wrote ourselves, so the split is decidable at a glance;
    /// the plugin path cannot do that and must rely on the plugin's declaration
    /// (see `Tool::access`).
    fn access(&self) -> crate::workplace::Access {
        host_tool_access(self.name())
    }
    fn description(&self) -> &str {
        crate::i18n::t("Read a text file and return its content (provided by the host). Reads are not restricted by the workspace — any file on the device can be read. For a large file, pass start_line / max_lines to read just one slice (the file is streamed, never loaded whole); the header line carries the file's hash, which edit_file accepts as expected_hash.")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                PATH_ARG: {"type": "string", "description": crate::i18n::t("Path of the file to read (absolute; reads are not restricted by the workspace)")},
                "start_line": {"type": "integer", "description": crate::i18n::t("1-based line to start at (omit to start from the beginning). Passing this or max_lines switches to a line-numbered slice.")},
                "max_lines": {"type": "integer", "description": crate::i18n::t("How many lines to return at most (omit to read to the end, still subject to max_bytes)")},
                "max_bytes": {"type": "integer", "description": crate::i18n::t("Maximum bytes to read; default 200000")}
            },
            "required": [PATH_ARG]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let raw = match path_arg(&args) {
            Ok(p) => p,
            Err(e) => return e,
        };
        // Reads go through `guard_read`: **not restricted by the workplace**
        // (2026-10-04 semantics).
        let path = guard_read(rt, raw);
        if path.is_dir() {
            return ToolOutput::err(crate::tf!(
                "{} is a directory, not a file. Use list_dir to see what is inside.",
                path.display()
            ));
        }

        // ── Parameters: a range is the **optional second shape**; without one this is the same
        //    whole-file read as before ──
        let limit = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(READ_LIMIT)
            .min(READ_LIMIT);
        let start_line = match args.get("start_line").and_then(|v| v.as_u64()) {
            Some(0) => {
                return ToolOutput::err(crate::i18n::t(
                    "start_line is 1-based: line 1 is the first line of the file (0 is not a valid line number)",
                ))
            }
            Some(n) => Some(n as usize),
            None => None,
        };
        let max_lines = match args.get("max_lines").and_then(|v| v.as_u64()) {
            Some(0) => {
                return ToolOutput::err(crate::i18n::t(
                    "max_lines must be at least 1; omit it to read to the end of the file",
                ))
            }
            Some(n) => Some(n as usize),
            None => None,
        };
        let ranged = start_line.is_some() || max_lines.is_some();

        let size = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(e) => return ToolOutput::err(crate::tf!("failed to read {}: {}", path.display(), e)),
        };
        // Binary pre-check: only the first 8 KB, the same test as before — it just no longer
        // requires pulling the whole file into memory.
        match looks_binary(&path) {
            Ok(true) => {
                return ToolOutput::err(crate::tf!(
                    "{} looks like a binary file (contains NUL bytes); this tool reads text only.",
                    path.display()
                ))
            }
            Ok(false) => {}
            Err(e) => return ToolOutput::err(crate::tf!("failed to read {}: {}", path.display(), e)),
        }

        // The fingerprint is taken **before** reading: it describes *this version of the file*, and
        // the slice being read is only one window into it.
        let hash = fingerprint::of_file(&path).unwrap_or_else(|| fingerprint::unknown().to_string());

        let start = start_line.unwrap_or(1);
        let cap = max_lines;
        let slice = match read_slice(&path, start, cap, limit, ranged) {
            Ok(s) => s,
            Err(e) => return e,
        };

        // ── Header line ──
        // A whole-file read keeps its old shape (path + byte count) plus two new fields; a slice
        // spells out the range and how to continue, so the model knows to resume from `start_line`
        // instead of starting over.
        let mut out = crate::tf!("{} ({} bytes, hash {})\n", path.display(), size, hash);
        if ranged {
            if slice.last == 0 {
                out.push_str(&crate::tf!("lines {}-{}: no lines in this range\n", start, start));
            } else {
                out.push_str(&crate::tf!("lines {}-{}", slice.first, slice.last));
                if slice.has_more {
                    out.push_str(&crate::tf!(
                        " (more lines follow — call again with start_line={})",
                        slice.last + 1
                    ));
                } else {
                    out.push_str(crate::i18n::t(" (end of file)"));
                }
                out.push('\n');
            }
        }
        if slice.byte_capped {
            if ranged {
                out.push_str(&crate::tf!(
                    "⚠️ stopped at the {} byte cap; the text below is not the whole range — continue with start_line={}.\n",
                    limit,
                    slice.last.max(start)
                ));
            } else {
                out.push_str(&crate::tf!(
                    "⚠️ the file is larger than the {} byte cap; **the text below is only its first {} bytes**, not the whole file. Pass start_line / max_lines to read a specific slice.\n",
                    limit,
                    limit
                ));
            }
        }
        out.push_str(&slice.body);
        if slice.invalid_utf8 {
            out.push_str(crate::i18n::t(
                "\n⚠️ some bytes in this range are not valid UTF-8 and were shown as U+FFFD; the file on disk is unchanged. edit_file refuses such files.",
            ));
        }
        ToolOutput::ok(out)
    }
}

/// Is the first 8 KB of the file free of NUL bytes?
///
/// Only the head is inspected — the same rule the whole-file version used, kept deliberately: a NUL
/// deep inside a file is usually legitimate data (a length prefix, a padded field), whereas a NUL in
/// the first block means "this was never text".
fn looks_binary(path: &std::path::Path) -> std::io::Result<bool> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 8192];
    let n = f.read(&mut buf)?;
    Ok(buf[..n].contains(&0))
}

/// One slice of a text file, already decoded for display.
struct Slice {
    body: String,
    first: usize,
    last: usize,
    has_more: bool,
    /// The byte cap stopped us before the requested range was finished.
    byte_capped: bool,
    invalid_utf8: bool,
}

/// Read lines `[start, start+cap)` **by streaming** — memory never exceeds one line plus the byte cap.
///
/// This is the whole point of the range form: a 200 MB log can be read at line 180 000 without ever
/// materialising the file. The full-file form goes through the same code with `start = 1`, `cap = None`,
/// which is also what removes the old "read the entire file, then cut the output" behaviour.
///
/// `numbered` only controls the display (line numbers are added in range mode); the reader is the same.
fn read_slice(
    path: &std::path::Path,
    start: usize,
    cap: Option<usize>,
    max_bytes: usize,
    numbered: bool,
) -> Result<Slice, ToolOutput> {
    use std::io::{BufRead, BufReader};

    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return Err(ToolOutput::err(crate::tf!("failed to read {}: {}", path.display(), e))),
    };
    let mut r = BufReader::new(f);
    let mut buf: Vec<u8> = Vec::new();
    let mut body = String::new();
    let mut line_no = 0usize;
    let mut emitted = 0usize;
    let mut bytes = 0usize;
    let mut slice = Slice {
        body: String::new(),
        first: 0,
        last: 0,
        has_more: false,
        byte_capped: false,
        invalid_utf8: false,
    };

    loop {
        buf.clear();
        let n = match r.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(e) => return Err(ToolOutput::err(crate::tf!("failed to read {}: {}", path.display(), e))),
        };
        if n == 0 {
            break; // EOF
        }
        line_no += 1;
        if line_no < start {
            continue; // skipping the prefix does not even decode it
        }
        if let Some(cap) = cap {
            if emitted >= cap {
                slice.has_more = true; // one line past the range => there really is more
                break;
            }
        }
        let allow = max_bytes.saturating_sub(bytes);
        if allow == 0 {
            slice.has_more = true;
            slice.byte_capped = true;
            break;
        }
        let take = n.min(allow);
        let chunk = &buf[..take];
        if std::str::from_utf8(chunk).is_err() {
            slice.invalid_utf8 = true;
        }
        // Truncation can land in the middle of a multi-byte character => lossy, rather than failing
        // the whole call.
        let mut text = String::from_utf8_lossy(chunk).into_owned();
        let complete = take == n;
        if complete {
            if text.ends_with('\n') {
                text.pop();
                // `\r` is stripped only in the line-numbered view: there it is part of the line
                // terminator and would just show up as a stray control character. The plain form
                // must stay **byte-for-byte** what is on disk (a CRLF file must read back as CRLF),
                // so the `\r` is kept and the `\n` pushed back below restores the pair.
                if numbered && text.ends_with('\r') {
                    text.pop();
                }
            }
        }
        if numbered {
            body.push_str(&format!("{:>6}\t", line_no));
        }
        body.push_str(&text);
        body.push('\n');
        bytes += take;
        emitted += 1;
        if slice.first == 0 {
            slice.first = line_no;
        }
        slice.last = line_no;
        if !complete {
            // The byte budget ran out mid-line: the content is still given, but the line is marked
            // incomplete.
            slice.has_more = true;
            slice.byte_capped = true;
            break;
        }
    }

    // start_line past the end: say how many lines the file actually has, rather than returning an
    // empty string that makes the model believe the file is empty.
    if slice.first == 0 && start > 1 {
        return Err(ToolOutput::err(crate::tf!(
            "start_line {} is past the end of {}: the file has only {} line(s).",
            start,
            path.display(),
            line_no
        )));
    }
    if slice.first == 0 && line_no > 0 {
        // start = 1 and not a single line came out: only possible with a zero byte budget, which is
        // handled above — this is just the backstop.
        slice.has_more = true;
    }
    slice.body = body;
    Ok(slice)
}

// ---------------------------------------------------------------- write_file

pub(crate) struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        HOST_FILE_TOOLS[1]
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }
    /// Read / write are split by **name** (see `host_tool_access`) — these tools'
    /// schemas are ones we wrote ourselves, so the split is decidable at a glance;
    /// the plugin path cannot do that and must rely on the plugin's declaration
    /// (see `Tool::access`).
    fn access(&self) -> crate::workplace::Access {
        host_tool_access(self.name())
    }
    fn description(&self) -> &str {
        crate::i18n::t("Write a string into a text file (overwrite; provided by the host; subject to the workspace). The path must be an absolute path inside the workspace; parent directories are created automatically by default. For changing part of an existing file, use edit_file instead: an overwrite rewrites every unchanged line too, so any drift in them lands on disk as well.")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                PATH_ARG: {"type": "string", "description": crate::i18n::t("Path of the file to write (absolute, inside the workspace)")},
                "content": {"type": "string", "description": crate::i18n::t("File content (full overwrite)")},
                "create_dirs": {"type": "boolean", "description": crate::i18n::t("Whether to create missing parent directories automatically; default true")}
            },
            "required": [PATH_ARG, "content"]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let raw = match path_arg(&args) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let path = match guard(rt, raw) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ToolOutput::err(crate::i18n::t("missing parameter content (must be a string)")),
        };
        if path.is_dir() {
            return ToolOutput::err(crate::tf!("{} is a directory and cannot be written as a file.", path.display()));
        }
        let create_dirs = args.get("create_dirs").and_then(|v| v.as_bool()).unwrap_or(true);
        if create_dirs {
            if let Some(parent) = path.parent() {
                if !parent.exists() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        return ToolOutput::err(crate::tf!("failed to create parent directory {}: {}", parent.display(), e));
                    }
                }
            }
        } else if let Some(parent) = path.parent() {
            if !parent.is_dir() {
                return ToolOutput::err(crate::tf!(
                    "parent directory {} does not exist (create_dirs = false, so it was not created automatically)",
                    parent.display()
                ));
            }
        }
        let existed = path.exists();
        // Read the previous version **before** overwriting it — this is the only moment both
        // versions are in hand, and the "+N / −N" the UI shows comes from comparing them.
        // A file that is missing starts from empty (everything counts as added); one that is not
        // valid UTF-8 text yields `None`, and "no number" is the honest answer in that case.
        let previous = if existed { std::fs::read_to_string(&path).ok() } else { Some(String::new()) };
        match std::fs::write(&path, content.as_bytes()) {
            Ok(()) => {
                let out = ToolOutput::ok(crate::tf!(
                    "{} {} ({} bytes)",
                    if existed { crate::i18n::t("Overwrote") } else { crate::i18n::t("Created") },
                    path.display(),
                    content.len()
                ));
                match &previous {
                    Some(old) => {
                        let (added, removed) = line_diff(old, content);
                        out.with_diff(added, removed)
                            .with_diff_detail(diff_text(old, content))
                            .with_path(path)
                    }
                    // A previous version that was not UTF-8 text: no diff is possible, but the file
                    // **was** written — so it still carries its path, and the workspace view lists
                    // it as a changed file with no numbers rather than hiding it.
                    None => out.with_path(path),
                }
            }
            Err(e) => ToolOutput::err(crate::tf!("failed to write {}: {}", path.display(), e)),
        }
    }
}

/// Beyond this many (post-trim) lines the LCS table is skipped, see below.
const DIFF_MAX_LINES: usize = 2000;

/// Lines added / removed between `old` and `new`, as a **line-level** LCS — not a character diff.
///
/// Written by hand rather than pulled from a crate: this is the project's only diff, the inputs are
/// source-code sized, and answering "how much did that write change" should not cost a dependency.
/// A common head/tail is trimmed first (edits are usually local, and that is what keeps the table
/// small in the normal case); the LCS itself uses two rolling rows, so memory stays `O(min(n, m))`
/// instead of `O(n·m)`. Past `DIFF_MAX_LINES` the table is skipped entirely and a blunt line-by-line
/// comparison is used — it never *under*-reports a change, which is the direction that matters for a
/// number a human reads at a glance.
pub(crate) fn line_diff(old: &str, new: &str) -> (usize, usize) {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    let mut head = 0;
    while head < a.len() && head < b.len() && a[head] == b[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < a.len() - head && tail < b.len() - head && a[a.len() - 1 - tail] == b[b.len() - 1 - tail]
    {
        tail += 1;
    }
    let a = &a[head..a.len() - tail];
    let b = &b[head..b.len() - tail];

    if a.len() > DIFF_MAX_LINES || b.len() > DIFF_MAX_LINES {
        let common = a.iter().zip(b.iter()).filter(|(x, y)| x == y).count();
        return (b.len() - common, a.len() - common);
    }

    // LCS length, computed bottom-up with only two rows alive at once.
    let mut prev = vec![0u32; b.len() + 1];
    let mut cur = vec![0u32; b.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            cur[j] = if a[i] == b[j] { prev[j + 1] + 1 } else { prev[j].max(cur[j + 1]) };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let lcs = prev[0] as usize;
    // Everything not in the LCS was added (on the new side) or removed (on the old side).
    (b.len() - lcs, a.len() - lcs)
}

/// Unchanged lines kept around each change in the unfolded diff.
const DIFF_CONTEXT: usize = 3;
/// Most lines the unfolded diff carries; past this it is cut off and says so.
const DIFF_SHOW_MAX: usize = 400;
/// Longest line drawn in the unfolded diff, in characters — a minified file is one enormous line.
const DIFF_LINE_MAX: usize = 200;
/// Above this many cells the alignment table is skipped and the two middles are drawn as one block of
/// removals followed by one block of additions (see [`diff_text`]). ~1 MB of `u32`.
const DIFF_TABLE_CELLS: usize = 250_000;
/// The marker for a stretch of the file left out of the unfolded diff.
const ELISION: &str = "⋯";

/// Render the difference between two versions of a file, ready to be drawn line by line.
///
/// Each output line starts with `+` (added), `-` (removed) or a space (context); a line that is only
/// `⋯` stands in for a stretch left out. That is the classic shape on purpose: a front end can tell
/// what a line is from its first character alone, with no extra structure to pass around.
///
/// Alignment is the same head/tail trim + LCS as [`line_diff`], and is only attempted while the
/// changed middles are small enough to hold a table; past [`DIFF_TABLE_CELLS`] the two middles are
/// drawn as one block of removals followed by one block of additions. That fallback degrades the
/// *interleaving*, never the information — and a file written from scratch is all additions anyway.
pub(crate) fn diff_text(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    let mut head = 0;
    while head < a.len() && head < b.len() && a[head] == b[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < a.len() - head && tail < b.len() - head && a[a.len() - 1 - tail] == b[b.len() - 1 - tail]
    {
        tail += 1;
    }
    let mid_a = &a[head..a.len() - tail];
    let mid_b = &b[head..b.len() - tail];

    // The changes, as (prefix, text). Nothing is trimmed off the ends any more, so the first and the
    // last entry are always a real change.
    let mut ops: Vec<(char, &str)> = Vec::new();
    if mid_a.len().saturating_mul(mid_b.len()) <= DIFF_TABLE_CELLS {
        // The full table, so walking it back can tell an insertion from a deletion properly (the
        // two-rolling-row trick `line_diff` uses saves memory by throwing that information away).
        let cols = mid_b.len() + 1;
        let mut dp = vec![0u32; (mid_a.len() + 1) * cols];
        for i in (0..mid_a.len()).rev() {
            for j in (0..mid_b.len()).rev() {
                dp[i * cols + j] = if mid_a[i] == mid_b[j] {
                    dp[(i + 1) * cols + j + 1] + 1
                } else {
                    dp[(i + 1) * cols + j].max(dp[i * cols + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0usize, 0usize);
        while i < mid_a.len() && j < mid_b.len() {
            if mid_a[i] == mid_b[j] {
                ops.push((' ', mid_a[i]));
                i += 1;
                j += 1;
            } else if dp[(i + 1) * cols + j] >= dp[i * cols + j + 1] {
                ops.push(('-', mid_a[i]));
                i += 1;
            } else {
                ops.push(('+', mid_b[j]));
                j += 1;
            }
        }
        // One side ran out first; whatever is left of the other is a plain tail.
        for l in &mid_a[i..] {
            ops.push(('-', l));
        }
        for l in &mid_b[j..] {
            ops.push(('+', l));
        }
    } else {
        for l in mid_a {
            ops.push(('-', l));
        }
        for l in mid_b {
            ops.push(('+', l));
        }
    }

    let mut out: Vec<String> = Vec::new();
    // Context above the change, and an elision marker when there is more file above that.
    if head > 0 {
        let start = head.saturating_sub(DIFF_CONTEXT);
        if start > 0 {
            out.push(ELISION.to_string());
        }
        for l in &a[start..head] {
            out.push(format!(" {}", clip_line(l)));
        }
    }
    // Unchanged runs *inside* the change are elided too: one edited line in the middle of a long
    // block should not drag the whole block onto the screen.
    let mut k = 0;
    while k < ops.len() {
        if ops[k].0 != ' ' {
            out.push(format!("{}{}", ops[k].0, clip_line(ops[k].1)));
            k += 1;
            continue;
        }
        let run = k;
        while k < ops.len() && ops[k].0 == ' ' {
            k += 1;
        }
        if k - run <= DIFF_CONTEXT * 2 + 1 {
            for op in &ops[run..k] {
                out.push(format!(" {}", clip_line(op.1)));
            }
        } else {
            for op in &ops[run..run + DIFF_CONTEXT] {
                out.push(format!(" {}", clip_line(op.1)));
            }
            out.push(ELISION.to_string());
            for op in &ops[k - DIFF_CONTEXT..k] {
                out.push(format!(" {}", clip_line(op.1)));
            }
        }
    }
    if tail > 0 {
        let shown = DIFF_CONTEXT.min(tail);
        for l in &a[a.len() - tail..a.len() - tail + shown] {
            out.push(format!(" {}", clip_line(l)));
        }
        if tail > shown {
            out.push(ELISION.to_string());
        }
    }

    if out.len() > DIFF_SHOW_MAX {
        let rest = out.len() - DIFF_SHOW_MAX;
        out.truncate(DIFF_SHOW_MAX);
        out.push(crate::tf!("… {} more lines", rest));
    }
    out.join("\n")
}

/// One line of the unfolded diff, clipped — a single minified line must not smuggle a whole file in.
fn clip_line(s: &str) -> String {
    let mut out = String::new();
    for (n, c) in s.chars().enumerate() {
        if n >= DIFF_LINE_MAX {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------- list_dir

pub(crate) struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        HOST_FILE_TOOLS[2]
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }
    /// Read / write are split by **name** (see `host_tool_access`) — these tools'
    /// schemas are ones we wrote ourselves, so the split is decidable at a glance;
    /// the plugin path cannot do that and must rely on the plugin's declaration
    /// (see `Tool::access`).
    fn access(&self) -> crate::workplace::Access {
        host_tool_access(self.name())
    }
    fn description(&self) -> &str {
        crate::i18n::t("List a directory's contents (provided by the host). Listing is not restricted by the workspace. With no `path`, the current workspace root is listed. Directory names carry a trailing `/`.")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                PATH_ARG: {"type": "string", "description": crate::i18n::t("Directory to list (absolute path); omitted = workspace root")}
            },
            "required": []
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        // No path = the workspace root (under an unrestricted tier this degrades to the app root).
        // On this path the **model specified no location at all**, so no check is needed — the root is given by the host itself.
        let dir = match args.get(PATH_ARG).and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) {
            // Listing is "read-only" too: go through `guard_read`, not restricted
            // by the workplace.
            Some(raw) => guard_read(rt, raw),
            None => match rt.workplace.active_root() {
                Some(p) => p,
                None => rt.root.clone(),
            },
        };
        if !dir.exists() {
            return ToolOutput::err(crate::tf!("{} does not exist", dir.display()));
        }
        if !dir.is_dir() {
            return ToolOutput::err(crate::tf!("{} is not a directory. Use read_file to read a file.", dir.display()));
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => return ToolOutput::err(crate::tf!("failed to list {}: {}", dir.display(), e)),
        };
        // Directories first, then same kind sorted by name — so it reads with
        // structure, rather than in filesystem-return order
        let mut items: Vec<(bool, String, u64)> = Vec::new();
        for e in entries.flatten() {
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            items.push((is_dir, e.file_name().to_string_lossy().into_owned(), size));
        }
        items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let total = items.len();
        let mut out = crate::tf!("{} ({} entries)\n", dir.display(), total);
        for (is_dir, name, size) in items.iter().take(LIST_LIMIT) {
            if *is_dir {
                out.push_str(&format!("  {name}/\n"));
            } else {
                out.push_str(&crate::tf!("  {}  ({} bytes)\n", name, size));
            }
        }
        if total > LIST_LIMIT {
            out.push_str(&crate::tf!("  …{} more entries not listed (limit {})\n", total - LIST_LIMIT, LIST_LIMIT));
        }
        ToolOutput::ok(out)
    }
}

// ---------------------------------------------------------------- attach_file

/// **Hand a local file to the model to look at** — an image really enters the
/// next request and the model can see its content.
///
/// Why it is needed: `read_file` only reads text (binaries are refused outright),
/// so the "screenshot → analyze" path would only ever carry a path and the model
/// would be stuck with a string of characters. The only legal landing spot for an
/// image is a `user` message (the endpoint returns 400 outright when it sees an
/// image in system / assistant), whereas tool results travel under the `tool`
/// role — so this tool **does not stuff the image into its return value**; it only
/// reports the path, and the main loop spins up a separate user message to inject
/// it (see the closing section of `agent_loop`, `crate::attach`).
///
/// The access intent is **read-only** (see `tool::host_tool_access`): attaching
/// just hands the file out and writes nothing to disk, so there is no risk of
/// "breaking something elsewhere" — the same tier as `read_file`, not restricted
/// by the workplace. That too is deliberate: a plugin writing screenshots into the
/// workplace is the norm, but a user wanting to hand the model a picture on their
/// desktop should also work.
pub(crate) struct AttachFileTool;

#[async_trait]
impl Tool for AttachFileTool {
    fn name(&self) -> &str {
        HOST_FILE_TOOLS[4]
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }
    /// Read / write are split by **name** (see `host_tool_access`) — these tools'
    /// schemas are ones we wrote ourselves, so the split is decidable at a glance;
    /// the plugin path cannot do that and must rely on the plugin's declaration
    /// (see `Tool::access`).
    fn access(&self) -> crate::workplace::Access {
        host_tool_access(self.name())
    }
    fn description(&self) -> &str {
        crate::i18n::t("Hand a local file to the model so it can see it (provided by the host). Images (JPEG/PNG/GIF/WebP) are **really sent to the model** and the model can see the content; other formats are sent too, and the endpoint decides whether to accept them. Use this for things that must be *looked at* — screenshots / charts / scans; do not use read_file (that one reads text only).")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                PATH_ARG: {"type": "string", "description": crate::i18n::t("Path of the file to attach (absolute; not restricted by the workspace)")}
            },
            "required": [PATH_ARG]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let raw = match path_arg(&args) {
            Ok(p) => p,
            Err(e) => return e,
        };
        // Same tier as read_file: read-only, only normalizes the path, does not
        // cross the workplace boundary.
        let path = guard_read(rt, raw);
        match crate::attach::Attachment::probe(path.clone()) {
            Ok(a) => {
                let what = match a.kind {
                    Some(k) => crate::tf!("image ({})", k.mime()),
                    None => crate::i18n::t("file (not an image; the endpoint may not support this format)").to_string(),
                };
                // State truthfully in the result "what was attached and roughly how
                // big": the model thus knows whether to expect "an image it can see"
                // next or "possibly an unsupported-format error".
                ToolOutput::ok(crate::tf!(
                    "Attached {}: {} ({} bytes). Its content will appear in your next request.",
                    what,
                    path.display(),
                    a.size
                ))
                // The real payload is handed to the main loop here — **not stuffed
                // into the return value**
                .with_attach(vec![path])
            }
            Err(e) => ToolOutput::err(e),
        }
    }
}

// ---------------------------------------------------------------- remove

pub(crate) struct RemoveTool;

#[async_trait]
impl Tool for RemoveTool {
    fn name(&self) -> &str {
        HOST_FILE_TOOLS[3]
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }
    /// Read / write are split by **name** (see `host_tool_access`) — these tools'
    /// schemas are ones we wrote ourselves, so the split is decidable at a glance;
    /// the plugin path cannot do that and must rely on the plugin's declaration
    /// (see `Tool::access`).
    fn access(&self) -> crate::workplace::Access {
        host_tool_access(self.name())
    }
    fn description(&self) -> &str {
        crate::i18n::t("Move a file or an empty directory to the system recycle bin (provided by the host; subject to the workspace) — nothing is erased, and the user can still restore it. Moving a non-empty directory as a whole requires an explicit recursive = true.")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                PATH_ARG: {"type": "string", "description": crate::i18n::t("Path to move to the recycle bin (absolute, inside the workspace)")},
                "recursive": {"type": "boolean", "description": crate::i18n::t("Whether to move a non-empty directory to the recycle bin as a whole; default false")}
            },
            "required": [PATH_ARG]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let raw = match path_arg(&args) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let path = match guard(rt, raw) {
            Ok(p) => p,
            Err(e) => return e,
        };
        // Don't delete the workplace root itself — that would leave every remaining
        // file operation in this conversation with nowhere to go. This has nothing
        // to do with permissions; it's purely a "delete it and you can't keep
        // working" protection.
        if rt.workplace.active_root().as_deref() == Some(path.as_path()) {
            return ToolOutput::err(crate::tf!(
                "{} is the current workspace root and cannot delete itself. To clean up its contents, delete the entries inside it.",
                path.display()
            ));
        }
        if !path.exists() {
            return ToolOutput::err(crate::tf!("{} does not exist", path.display()));
        }
        let recursive = args.get("recursive").and_then(|v| v.as_bool()).unwrap_or(false);
        // A **non-empty directory** is the only case that has to ask permission out loud: it is the
        // one shape of call that can send a whole tree to the bin on a single slip. A file, and an
        // empty directory, go without a second thought.
        if !recursive && path.is_dir() {
            let non_empty = std::fs::read_dir(&path).map(|mut it| it.next().is_some()).unwrap_or(false);
            if non_empty {
                return ToolOutput::err(crate::tf!(
                    "{} is a non-empty directory. To move it to the recycle bin as a whole, pass recursive = true explicitly.",
                    path.display()
                ));
            }
        }
        // Count **before** the move: once the shell has taken the files there is nothing left to
        // walk, and the shell does not tell us how much it took (see `recycle::count_files`). That
        // number is what the UI turns into "N files moved to the recycle bin".
        let count = crate::recycle::count_files(&path);
        // The delete goes to the **recycle bin**, not to oblivion — see the module note in
        // `crate::recycle` for why it is the shell's own delete rather than `remove_file`, and for
        // the two cases where the shell can still erase outright.
        match crate::recycle::recycle(&path).await {
            Ok(()) => ToolOutput::ok(crate::tf!("Moved {} to the system recycle bin", path.display()))
                .with_recycled(count),
            Err(e) => ToolOutput::err(crate::tf!(
                "failed to move {} to the system recycle bin: {}",
                path.display(),
                format!("{e:#}")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, WorkplaceConfig};
    use crate::runtime::Runtime;

    #[test]
    fn line_diff_counts_insertions_and_deletions() {
        // A file appearing from nothing is all additions; emptying one is all removals.
        assert_eq!(line_diff("", "a\nb"), (2, 0));
        assert_eq!(line_diff("a\nb", ""), (0, 2));
        // Neither side changed.
        assert_eq!(line_diff("a\nb\nc", "a\nb\nc"), (0, 0));
    }

    #[test]
    fn line_diff_leaves_shared_lines_out_of_the_tally() {
        // One middle line swapped: exactly 1 added and 1 removed, neighbours untouched.
        assert_eq!(line_diff("a\nb\nc\n", "a\nB\nc\n"), (1, 1));
        // A pure append is 1 added, 0 removed.
        assert_eq!(line_diff("a\nb\n", "a\nb\nc\n"), (1, 0));
    }

    /// The unfolded diff. The prefixes are the whole contract with the front end, and the elisions
    /// are what keep a one-line edit from dragging an entire file onto the screen.
    #[test]
    fn diff_text_marks_every_line_and_elides_the_unchanged() {
        // A file appearing from nothing is additions and nothing else.
        assert_eq!(diff_text("", "a\nb"), "+a\n+b");
        // Nothing changed at all: no change lines to show.
        assert_eq!(diff_text("a\nb\n", "a\nb\n"), " a\n b");
        // One edited line inside a long file: the change, three lines of context on each side, and an
        // elision marker wherever the rest of the file was left out.
        let old: String = (0..12).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line5\n", "LINE5\n");
        assert_eq!(
            diff_text(&old, &new),
            "⋯\n line2\n line3\n line4\n-line5\n+LINE5\n line6\n line7\n line8\n⋯"
        );
        // An unchanged stretch *between* two changes is elided the same way.
        let a = "a\nX\nc1\nc2\nc3\nc4\nc5\nc6\nc7\nc8\nc9\nc10\nY\nz";
        let b = "a\nX2\nc1\nc2\nc3\nc4\nc5\nc6\nc7\nc8\nc9\nc10\nY2\nz";
        let d = diff_text(a, b);
        assert!(d.contains("-X\n+X2\n"), "{d}");
        assert!(d.contains("⋯"), "the middle stretch should be elided: {d}");
        // Past the size the panel may grow to, the diff is cut off — and says so rather than
        // pretending the file ended there.
        let big: String = (0..DIFF_SHOW_MAX + 50).map(|i| format!("l{i}\n")).collect();
        let d = diff_text("", &big);
        assert_eq!(d.lines().count(), DIFF_SHOW_MAX + 1);
        assert!(d.ends_with(" more lines"), "{d}");
    }

    /// Build a runtime with a "fixed workplace", to verify the tools really are
    /// bound by the boundary.
    ///
    /// The tools must genuinely be registered: `Runtime::execute` looks people up
    /// by registry, and bypassing the registry leaves nothing to test. This is
    /// exactly the division of labour between this module and the `workplace`
    /// unit tests — that side tests the decision, this side tests that "the
    /// decision really is wired into the tool-call path".
    async fn rt_with_ws(dir: &std::path::Path) -> Arc<Runtime> {
        let mut cfg = Config::dummy();
        cfg.workplace = WorkplaceConfig { mode: "fixed".into(), root: Some(dir.to_string_lossy().into()) };
        // It doesn't matter if interpreter resolution fails: this module's tests
        // don't start plugins
        let rt = Runtime::new(cfg, dir.to_path_buf(), dir.join("plugins")).expect("the runtime should build");
        for t in host_file_tools() {
            rt.register(t).await.expect("registering host file tools should succeed");
        }
        rt
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aox-hostfs-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let d = tmp("rw");
        let rt = rt_with_ws(&d).await;
        let f = d.join("sub").join("a.txt");

        let w = rt.execute(HOST_FILE_TOOLS[1], json!({PATH_ARG: f.to_string_lossy(), "content": "héllo\n"})).await;
        assert!(!w.is_error, "{}", w.content);
        assert!(f.exists(), "the parent directory should be created automatically");

        let r = rt.execute(HOST_FILE_TOOLS[0], json!({PATH_ARG: f.to_string_lossy()})).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("héllo"), "the content should read back: {}", r.content);
    }

    /// An out-of-bounds **write** must be blocked, and the copy handed to the
    /// model must carry the three-part contract.
    ///
    /// Note it's "write": reads are not restricted by the workplace (see
    /// `read_outside_is_allowed_but_remove_is_not`), so this can only hang off
    /// `write_file` — hanging it off `read_file` would make it a permanently green
    /// fake test.
    #[tokio::test]
    async fn outside_write_is_refused_with_the_contract() {
        let d = tmp("outside");
        let rt = rt_with_ws(&d).await;
        let bad = d.parent().unwrap().join("aox-hostfs-evil.txt");
        let out = rt.execute(HOST_FILE_TOOLS[1], json!({PATH_ARG: bad.to_string_lossy(), "content": "x"})).await;
        assert!(out.is_error);
        assert!(!bad.exists(), "an out-of-bounds write must never actually happen");
        assert!(out.content.contains("permission boundary"), "{}", out.content);
        assert!(out.content.contains("Do not retry"), "{}", out.content);
        assert!(out.content.contains("relay this to the user"), "{}", out.content);
    }

    /// New semantics (2026-10-04): **reading is not restricted by the workplace**
    /// — out-of-bounds files are readable and out-of-bounds directories listable;
    /// but **removal** is still restricted. Pin both together, so nobody later
    /// "unifies" them back into one tier (pinning only one side means "read passed"
    /// could just be that reading never went through the check at all — that's a
    /// leak, not correct semantics).
    #[tokio::test]
    async fn read_outside_is_allowed_but_remove_is_not() {
        let d = tmp("rdout");
        let rt = rt_with_ws(&d).await;
        // A file and a directory **outside** the workplace
        let out_dir = d.parent().unwrap().join("aox-hostfs-outside-dir");
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();
        let f = out_dir.join("note.txt");
        std::fs::write(&f, "outside content").unwrap();

        // Read: allowed
        let r = rt.execute(HOST_FILE_TOOLS[0], json!({PATH_ARG: f.to_string_lossy()})).await;
        assert!(!r.is_error, "reading a file outside the workspace must be allowed (new semantics): {}", r.content);
        assert!(r.content.contains("outside content"), "{}", r.content);

        // List directory: equally allowed
        let l = rt.execute(HOST_FILE_TOOLS[2], json!({PATH_ARG: out_dir.to_string_lossy()})).await;
        assert!(!l.is_error, "listing a directory outside the workspace must be allowed: {}", l.content);
        assert!(l.content.contains("note.txt"), "{}", l.content);

        // Remove: still blocked, and the file must still be there
        let rm = rt.execute(HOST_FILE_TOOLS[3], json!({PATH_ARG: f.to_string_lossy()})).await;
        assert!(rm.is_error, "deleting a file outside the workspace must be intercepted: {}", rm.content);
        assert!(f.exists(), "an out-of-bounds delete must never actually happen");
        assert!(rm.content.contains("permission boundary"), "{}", rm.content);

        let _ = std::fs::remove_dir_all(&out_dir);
    }

    #[tokio::test]
    async fn non_empty_dir_needs_recursive() {
        let d = tmp("rm");
        let rt = rt_with_ws(&d).await;
        let sub = d.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("inner.txt"), "x").unwrap();

        let denied = rt.execute(HOST_FILE_TOOLS[3], json!({PATH_ARG: sub.to_string_lossy()})).await;
        assert!(denied.is_error, "a non-empty directory must not be deleted by a single slip");
        assert!(sub.exists());

        let forced = rt
            .execute(HOST_FILE_TOOLS[3], json!({PATH_ARG: sub.to_string_lossy(), "recursive": true}))
            .await;
        assert!(!forced.is_error, "{}", forced.content);
        assert!(!sub.exists());
    }

    #[tokio::test]
    async fn workplace_root_itself_is_not_removable() {
        let d = tmp("root");
        let rt = rt_with_ws(&d).await;
        let out = rt.execute(HOST_FILE_TOOLS[3], json!({PATH_ARG: d.to_string_lossy(), "recursive": true})).await;
        assert!(out.is_error, "the workspace root must not be deletable");
        assert!(d.exists());
    }

    #[tokio::test]
    async fn list_dir_defaults_to_the_workplace_root() {
        let d = tmp("ls");
        std::fs::write(d.join("a.txt"), "12345").unwrap();
        std::fs::create_dir_all(d.join("folder")).unwrap();
        let rt = rt_with_ws(&d).await;
        let out = rt.execute(HOST_FILE_TOOLS[2], json!({})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("folder/"), "directories should carry a trailing /: {}", out.content);
        assert!(out.content.contains("a.txt"), "{}", out.content);
    }

    // ───────────────────────── range reads ─────────────────────────

    /// Pull the fingerprint out of a read_file header — the exact string a model would copy into
    /// `expected_hash`. Parsing it here (rather than recomputing it) is the point: it proves the
    /// value printed is the value edit_file accepts.
    fn hash_in(content: &str) -> String {
        let head = content.lines().next().unwrap_or_default();
        let at = head.find("hash ").expect("the header must carry a hash");
        head[at + 5..].chars().take(16).collect()
    }

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line{i}\n")).collect()
    }

    #[tokio::test]
    async fn a_range_read_returns_only_that_slice_with_line_numbers() {
        let d = tmp("range");
        std::fs::write(d.join("log.txt"), numbered(10)).unwrap();
        let rt = rt_with_ws(&d).await;

        let out = rt
            .execute(
                HOST_FILE_TOOLS[0],
                json!({PATH_ARG: d.join("log.txt").to_string_lossy(), "start_line": 3, "max_lines": 2}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("lines 3-4"), "{}", out.content);
        assert!(out.content.contains("more lines follow"), "{}", out.content);
        assert!(out.content.contains("start_line=5"), "the next call should be spelled out: {}", out.content);
        // Exactly these two lines: neither the lines before them nor the ones after may appear
        assert!(out.content.contains("line3"), "{}", out.content);
        assert!(out.content.contains("line4"), "{}", out.content);
        assert!(!out.content.contains("line2\n"), "{}", out.content);
        assert!(!out.content.contains("line5"), "{}", out.content);
        // Line numbers are 1-based and form their own column
        assert!(out.content.contains("     3\tline3"), "{:?}", out.content);
    }

    #[tokio::test]
    async fn the_last_range_says_it_reached_the_end() {
        let d = tmp("range-end");
        std::fs::write(d.join("f.txt"), numbered(5)).unwrap();
        let rt = rt_with_ws(&d).await;
        let out = rt
            .execute(
                HOST_FILE_TOOLS[0],
                json!({PATH_ARG: d.join("f.txt").to_string_lossy(), "start_line": 4}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("lines 4-5"), "{}", out.content);
        assert!(out.content.contains("end of file"), "{}", out.content);
        assert!(!out.content.contains("more lines follow"), "{}", out.content);
    }

    /// start_line out of range: it must say how many lines the file has. An empty string is the
    /// worst kind of "looks like it worked".
    #[tokio::test]
    async fn a_start_line_past_the_end_is_an_error_with_the_real_length() {
        let d = tmp("range-past");
        std::fs::write(d.join("f.txt"), numbered(3)).unwrap();
        let rt = rt_with_ws(&d).await;
        let out = rt
            .execute(
                HOST_FILE_TOOLS[0],
                json!({PATH_ARG: d.join("f.txt").to_string_lossy(), "start_line": 9}),
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("only 3 line"), "{}", out.content);
    }

    #[tokio::test]
    async fn zero_valued_range_arguments_are_rejected() {
        let d = tmp("range-zero");
        std::fs::write(d.join("f.txt"), "a\nb\n").unwrap();
        let rt = rt_with_ws(&d).await;
        for args in [
            json!({PATH_ARG: d.join("f.txt").to_string_lossy(), "start_line": 0}),
            json!({PATH_ARG: d.join("f.txt").to_string_lossy(), "max_lines": 0}),
        ] {
            let out = rt.execute(HOST_FILE_TOOLS[0], args.clone()).await;
            assert!(out.is_error, "{args} should be rejected: {}", out.content);
        }
    }

    /// No range = the old behaviour: the whole content, no line-number column, header gains only
    /// `hash`.
    #[tokio::test]
    async fn the_plain_read_keeps_its_old_shape() {
        let d = tmp("plain");
        std::fs::write(d.join("f.txt"), "alpha\nbeta\n").unwrap();
        let rt = rt_with_ws(&d).await;
        let out = rt
            .execute(HOST_FILE_TOOLS[0], json!({PATH_ARG: d.join("f.txt").to_string_lossy()}))
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("(11 bytes, hash "), "{}", out.content);
        assert!(!out.content.contains("     1\t"), "no line-number column in the plain form: {}", out.content);
        assert!(out.content.ends_with("alpha\nbeta\n"), "{:?}", out.content);
    }

    /// The hash in the header must be **exactly** what `edit_file` accepts as `expected_hash` — the
    /// only contract between the two tools. Let the algorithm or the formatting differ and every edit
    /// the model makes with a hash gets rejected as "the file changed".
    #[tokio::test]
    async fn the_hash_printed_by_read_is_accepted_by_edit() {
        let d = tmp("hash-loop");
        let f = d.join("f.txt");
        std::fs::write(&f, "before\n").unwrap();
        let rt = rt_with_ws(&d).await;

        let read = rt.execute(HOST_FILE_TOOLS[0], json!({PATH_ARG: f.to_string_lossy()})).await;
        let hash = hash_in(&read.content);
        let edit = rt
            .execute(
                HOST_FILE_TOOLS[5],
                json!({
                    PATH_ARG: f.to_string_lossy(),
                    "edits": [{"old_text": "before", "new_text": "after"}],
                    "expected_hash": hash,
                }),
            )
            .await;
        assert!(!edit.is_error, "the printed hash must be accepted verbatim: {}", edit.content);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "after\n");
    }

    /// CRLF files: the line-number column must not drag `\r` into the content, and a whole-file
    /// read must return the body byte-for-byte.
    #[tokio::test]
    async fn crlf_files_read_cleanly_in_both_forms() {
        let d = tmp("crlf");
        let f = d.join("win.txt");
        std::fs::write(&f, "a\r\nb\r\nc\r\n").unwrap();
        let rt = rt_with_ws(&d).await;

        let ranged = rt
            .execute(
                HOST_FILE_TOOLS[0],
                json!({PATH_ARG: f.to_string_lossy(), "start_line": 2, "max_lines": 1}),
            )
            .await;
        assert!(!ranged.is_error, "{}", ranged.content);
        assert!(ranged.content.contains("     2\tb\n"), "{:?}", ranged.content);
        assert!(!ranged.content.contains('\r'), "{:?}", ranged.content);

        let plain = rt.execute(HOST_FILE_TOOLS[0], json!({PATH_ARG: f.to_string_lossy()})).await;
        assert!(plain.content.ends_with("a\r\nb\r\nc\r\n"), "{:?}", plain.content);
    }

    /// The byte budget ran out mid-line: the content is given, and the line is marked incomplete
    /// with more to follow.
    #[tokio::test]
    async fn the_byte_cap_reports_a_partial_stop() {
        let d = tmp("bytecap");
        std::fs::write(d.join("f.txt"), numbered(40)).unwrap();
        let rt = rt_with_ws(&d).await;
        let out = rt
            .execute(
                HOST_FILE_TOOLS[0],
                json!({PATH_ARG: d.join("f.txt").to_string_lossy(), "max_bytes": 100}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("byte cap"), "{}", out.content);
        assert!(out.content.contains("not the whole file"), "{}", out.content);
        assert!(out.content.len() < 400, "the cap must actually bound the payload: {}", out.content.len());
    }

    // ───────────────────────── the index table ─────────────────────────

    /// `HOST_FILE_TOOLS` is addressed **by index** (the read/write split, and every `[n]` above), so
    /// the list and the table have to agree position by position. A future tool inserted in the middle
    /// would silently point the read-only exemption at some other tool; this fails instead.
    #[test]
    fn the_tool_list_matches_the_name_table_in_order() {
        let tools = host_file_tools();
        assert_eq!(tools.len(), HOST_FILE_TOOLS.len());
        for (i, t) in tools.iter().enumerate() {
            assert_eq!(t.name(), HOST_FILE_TOOLS[i], "index {i} drifted out of the table");
        }
        // The new tool is a write: it must be judged by the workspace boundary exactly as write_file is.
        assert_eq!(host_tool_access("edit_file"), crate::workplace::Access::Write);
        assert_eq!(host_tool_access("read_file"), crate::workplace::Access::Read);
    }

    /// The schema the model actually receives has to name the two required parameters — a tool whose
    /// schema forgot `edits` would be called with `{}` and fail every time.
    #[test]
    fn the_edit_schema_requires_path_and_edits() {
        let tools = host_file_tools();
        let edit = tools.iter().find(|t| t.name() == "edit_file").expect("edit_file must be registered");
        let params = edit.parameters();
        let required: Vec<&str> = params["required"]
            .as_array()
            .expect("required must be an array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(required.contains(&PATH_ARG), "{required:?}");
        assert!(required.contains(&"edits"), "{required:?}");
        assert_eq!(params["properties"]["edits"]["type"], "array");
        assert_eq!(params["properties"]["edits"]["items"]["type"], "object");
    }
}
