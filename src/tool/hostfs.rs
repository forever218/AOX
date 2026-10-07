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

use super::{host_tool_access, Tool, ToolKind, ToolOutput, HOST_FILE_TOOLS};
use crate::runtime::Runtime;
use crate::workplace::PATH_ARG;

/// The five host file tools.
pub fn host_file_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFileTool),
        Arc::new(WriteFileTool),
        Arc::new(ListDirTool),
        Arc::new(RemoveTool),
        Arc::new(AttachFileTool),
    ]
}

/// Per-call byte cap when reading a file. Beyond it, truncate and **say so
/// explicitly** — silently truncating would make the model draw conclusions from
/// partial content, the worst kind of "silent failure".
const READ_LIMIT: usize = 200_000;
/// Entry cap when listing a directory, for the same reason.
const LIST_LIMIT: usize = 300;

/// Check a `path` argument for a **write to disk** (write / remove), and unify
/// "out of bounds" into the contract text handed to the model.
fn guard(rt: &Arc<Runtime>, raw: &str) -> Result<std::path::PathBuf, ToolOutput> {
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
fn path_arg(args: &Value) -> Result<&str, ToolOutput> {
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
        crate::i18n::t("Read a text file and return its content (provided by the host). Reads are not restricted by the workspace — any file on the device can be read.")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                PATH_ARG: {"type": "string", "description": crate::i18n::t("Path of the file to read (absolute; reads are not restricted by the workspace)")},
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
        let limit = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(READ_LIMIT)
            .min(READ_LIMIT);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(crate::tf!("failed to read {}: {}", path.display(), e)),
        };
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return ToolOutput::err(crate::tf!(
                "{} looks like a binary file (contains NUL bytes); this tool reads text only.",
                path.display()
            ));
        }
        let truncated = bytes.len() > limit;
        // Truncation may land in the middle of a multi-byte character, so use
        // lossy instead of failing outright with from_utf8
        let body = String::from_utf8_lossy(&bytes[..bytes.len().min(limit)]).into_owned();
        let mut out = crate::tf!("{} ({} bytes)\n", path.display(), bytes.len());
        if truncated {
            out.push_str(&crate::tf!(
                "⚠️ the file exceeds {} bytes; **the text below is only the first {} bytes**, not the whole file.\n",
                limit,
                limit
            ));
        }
        out.push_str(&body);
        ToolOutput::ok(out)
    }
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
        crate::i18n::t("Write a string into a text file (overwrite; provided by the host; subject to the workspace). The path must be an absolute path inside the workspace; parent directories are created automatically by default.")
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
        match std::fs::write(&path, content.as_bytes()) {
            Ok(()) => ToolOutput::ok(crate::tf!(
                "{} {} ({} bytes)",
                if existed { crate::i18n::t("Overwrote") } else { crate::i18n::t("Created") },
                path.display(),
                content.len()
            )),
            Err(e) => ToolOutput::err(crate::tf!("failed to write {}: {}", path.display(), e)),
        }
    }
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
        crate::i18n::t("Delete a file or an empty directory (provided by the host; subject to the workspace). Deleting a non-empty directory requires an explicit recursive = true.")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                PATH_ARG: {"type": "string", "description": crate::i18n::t("Path to delete (absolute, inside the workspace)")},
                "recursive": {"type": "boolean", "description": crate::i18n::t("Whether to delete a non-empty directory recursively; default false")}
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
        let is_dir = path.is_dir();
        if is_dir {
            if !recursive {
                // A non-empty directory won't be deleted: this is the only gate
                // against "one slip wipes out a whole tree"
                let non_empty = std::fs::read_dir(&path).map(|mut it| it.next().is_some()).unwrap_or(false);
                if non_empty {
                    return ToolOutput::err(crate::tf!(
                        "{} is a non-empty directory. To really delete it recursively, pass recursive = true explicitly.",
                        path.display()
                    ));
                }
            }
            let r = if recursive { std::fs::remove_dir_all(&path) } else { std::fs::remove_dir(&path) };
            match r {
                Ok(()) => ToolOutput::ok(crate::tf!(
                    "Deleted directory {} ({})",
                    path.display(),
                    if recursive { crate::i18n::t("recursive") } else { crate::i18n::t("empty dir") }
                )),
                Err(e) => ToolOutput::err(crate::tf!("failed to delete directory {}: {}", path.display(), e)),
            }
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => ToolOutput::ok(crate::tf!("Deleted file {}", path.display())),
                Err(e) => ToolOutput::err(crate::tf!("failed to delete file {}: {}", path.display(), e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, WorkplaceConfig};
    use crate::runtime::Runtime;

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

        let w = rt.execute(HOST_FILE_TOOLS[1], json!({PATH_ARG: f.to_string_lossy(), "content": "你好\n"})).await;
        assert!(!w.is_error, "{}", w.content);
        assert!(f.exists(), "the parent directory should be created automatically");

        let r = rt.execute(HOST_FILE_TOOLS[0], json!({PATH_ARG: f.to_string_lossy()})).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("你好"), "the content should read back: {}", r.content);
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
        std::fs::write(&f, "外面的内容").unwrap();

        // Read: allowed
        let r = rt.execute(HOST_FILE_TOOLS[0], json!({PATH_ARG: f.to_string_lossy()})).await;
        assert!(!r.is_error, "reading a file outside the workspace must be allowed (new semantics): {}", r.content);
        assert!(r.content.contains("外面的内容"), "{}", r.content);

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
}
