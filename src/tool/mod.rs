pub mod ask;
pub mod bash;
pub mod discover;
pub mod edit;
pub mod fingerprint;
pub mod hostfs;
pub mod job;
pub mod meta;
pub mod scope;
pub mod shell;
pub mod task;
pub mod verify;
pub mod web;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::runtime::Runtime;

/// The optional `purpose` argument: one short line saying what a call is **for**.
///
/// Declared here rather than in each schema because four tools carry it (`shell`, `bash`,
/// `write_file`, `edit_file`) and a reworded copy in three of them is exactly how tool descriptions
/// drift apart. It is model-facing text, so the wording is the whole feature: a purpose that reads
/// "do the thing" is worse than none.
pub const PURPOSE_ARG: &str = "purpose";

/// What `purpose` is for. See [`PURPOSE_ARG`].
///
/// The last sentence is the part that earns its tokens: without it the model writes purposes for the
/// trivial calls and skips the interesting ones ("bash" is exactly the row that needs a label).
pub const PURPOSE_DESC: &str = "One short line (a few words, not a sentence) saying what this \
     call is for — e.g. \"fix the title save\", \"run the test suite\", \"count files by type\". The \
     UI shows it next to the tool name, so the user can tell calls apart without opening the \
     arguments. Optional, but fill it in whenever the tool name alone does not say what you are \
     doing — which is almost always for `bash`/`shell`, whose name says nothing about the command.";

/// Names of the host's built-in **file tools** (same order as `hostfs::host_file_tools()`).
///
/// This is a constant table rather than a literal repeated in four `name()` methods because
/// the names have to be recognised in **two** places: ① what each tool in `hostfs` calls
/// itself, and ② `agent::authorize_and_execute` deciding "is this call a host file tool?".
/// Both sides referring to the same constant rules out the nastiest kind of drift — rename
/// a tool and the boundary check silently stops applying (the tool still works, it just has
/// no boundary any more).
///
/// ⚠️ **Append only.** Entries are addressed by index ([`READ_ONLY_TOOLS`], and every
/// `HOST_FILE_TOOLS[n]` in `hostfs` / `edit` / the tests), so inserting a name in the middle
/// silently re-points the read/write split at a different tool. `edit_file` was appended as
/// index 5 for exactly that reason.
pub(crate) const HOST_FILE_TOOLS: [&str; 6] =
    ["read_file", "write_file", "list_dir", "remove", "attach_file", "edit_file"];

/// Which entries in that table are **read-only** (by index) — reads are unrestricted, only
/// writes and deletes go through the workplace boundary (the 2026-10-04 semantics).
///
/// Split out of `host_tool_access` into its own constant so that "which ones are reads" has
/// exactly **one** statement of record: `attach_file` counts as a read (attaching only hands
/// the file over, nothing is written). Adding a read-only tool later means changing this line
/// and nothing else — the `if` below cannot fall out of sync.
///
/// `edit_file` is deliberately **absent**: it rewrites bytes on disk, so it is a write, and the
/// workspace boundary must judge it exactly as it judges `write_file`.
const READ_ONLY_TOOLS: [usize; 3] = [0 /* read_file */, 2 /* list_dir */, 4 /* attach_file */];

/// The **access intent** of a host file tool: read-only (read a file / list a directory /
/// attach a file) or write (write / delete).
///
/// Same source of truth as [`HOST_FILE_TOOLS`] (names are looked up by index into one table),
/// so renaming cannot silently disable the decision — same reasoning as that table's comment.
/// If a name does not match (which should not happen) it is treated as a **write**: the
/// conservative direction is always "block one time too many, never one time too few".
pub(crate) fn host_tool_access(name: &str) -> crate::workplace::Access {
    if READ_ONLY_TOOLS.iter().any(|i| name == HOST_FILE_TOOLS[*i]) {
        crate::workplace::Access::Read
    } else {
        crate::workplace::Access::Write
    }
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// **Files produced** by this tool (local paths). The host hands them to the model as
    /// images — see `crate::attach`.
    ///
    /// Why hang them off the result instead of keeping a global staging area: semantically
    /// these are "this step's output", so travelling with the result gives them the right
    /// owner and the right order for free. Both routes fill this field — host tools
    /// (`attach_file`) and plugins (writing `"attach": ["<path>", …]` into the returned
    /// JSON) — which is what makes "the plugin took a screenshot and the model actually saw
    /// it" true, rather than stuffing a path string into the context.
    ///
    /// ⚠️ Images may **only** be sent as `user` messages (an endpoint seeing an image in a
    /// system or assistant message rejects it with a 400), so these paths never go into the
    /// `tool` result. The main loop injects them as a separate user message instead (see the
    /// closing section of `agent_loop`).
    pub attach: Vec<std::path::PathBuf>,
    /// How many files this call **moved to the system recycle bin** (0 = "this call wasn't a
    /// delete").
    ///
    /// It rides on the result for exactly the same reason `attach` does: it is *this step's*
    /// outcome, so travelling with the result gives it the right owner and the right order for
    /// free. Two things make a field the only workable shape here — the count has to be taken
    /// **before** the move (afterwards the files are gone and there is nothing left to count), and
    /// the UI needs a **number**, not prose: the `content` string below is written for the model,
    /// and making a front end parse a sentence back out of it to feed a toast would be a needless
    /// round trip through text. The front end does the wording (see `AgentEvent::Recycled`).
    pub recycled: usize,
    /// **Lines added / removed** by this call, when it edited a text file — `(added, removed)`.
    ///
    /// `None` means "this call wasn't an edit" (a read, a listing…), which is deliberately not the
    /// same as `Some((0, 0))`: the latter is a real edit that happened to change nothing, and the UI
    /// should say something different (nothing) for the two.
    ///
    /// Like `recycled`, it rides on the result because it is *this step's* outcome, and the UI wants
    /// two **numbers** rather than a sentence: the diff has to be computed while both versions are
    /// still in hand (see `write_file`), and having a front end re-parse prose to find them would be
    /// a needless round trip through text.
    pub diff: Option<(usize, usize)>,
    /// The diff itself, ready to draw — one line per entry, each prefixed with `+`, `-` or a space
    /// (`⋯` marks elided stretches). `None` when the call was not an edit.
    ///
    /// It travels beside the two counts rather than instead of them: the counts are what the row
    /// shows at a glance, this is what the row unfolds into. Rendered **here**, in the tool, for the
    /// same reason the counts are computed here — both versions of the file exist for exactly one
    /// moment, and only this side can still see them.
    pub diff_detail: Option<String>,
    /// The **file this call wrote**, when it was a file edit (`None` for everything else).
    ///
    /// The counts and the diff above say "how much changed" but never "of what"; the workspace view
    /// (`gui/src/wsview.rs`) needs the *name* to put the change in a list and to open that file's
    /// diff when the row is clicked. The tool already has the path in hand at the moment it computes
    /// the diff, so carrying it costs one `Option` and saves the front end from re-parsing
    /// `content` — which is prose written for the model, not a record.
    ///
    /// Absolute, and already through the workplace boundary (see `guard` in `hostfs`), so a front
    /// end can read it directly without deciding again whether it is allowed to.
    pub path: Option<std::path::PathBuf>,
}

impl ToolOutput {
    pub fn ok(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            is_error: false,
            attach: Vec::new(),
            recycled: 0,
            diff: None,
            diff_detail: None,
            path: None,
        }
    }

    pub fn err(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            is_error: true,
            attach: Vec::new(),
            recycled: 0,
            diff: None,
            diff_detail: None,
            path: None,
        }
    }

    /// Carry the files this tool produced. Chain it after `ok(..)` / `err(..)`.
    pub fn with_attach(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.attach = paths;
        self
    }

    /// Report that this call sent `n` files to the recycle bin. Chain it after `ok(..)`.
    pub fn with_recycled(mut self, n: usize) -> Self {
        self.recycled = n;
        self
    }

    /// Report how many lines this call **added / removed**. Chain it after `ok(..)`.
    pub fn with_diff(mut self, added: usize, removed: usize) -> Self {
        self.diff = Some((added, removed));
        self
    }

    /// Carry the diff to draw when the row is unfolded. Chain it after `with_diff(..)`.
    pub fn with_diff_detail(mut self, detail: String) -> Self {
        self.diff_detail = Some(detail);
        self
    }

    /// Name the file this call wrote. Chain it after `with_diff(..)` — a path without a diff is a
    /// call that touched a file but changed nothing measurable, and the workspace view treats the
    /// two separately (a row with no numbers, rather than a row claiming "+0 / −0").
    pub fn with_path(mut self, path: std::path::PathBuf) -> Self {
        self.path = Some(path);
        self
    }
}

#[derive(Debug, Clone)]
pub enum ToolKind {
    /// Host-provided meta tools (`create_plugin` / `update_plugin` / `list_plugins` / `remove_plugin`).
    /// They change **the host itself** (adding, replacing and removing plugins), so by default they
    /// ask first (`[auth] meta_tools`).
    Meta,
    /// Host-provided **file tools** (`read_file` / `write_file` / `list_dir` / `remove`,
    /// see `hostfs`) — the concrete form of sandbox option ③: the arguments and the
    /// implementation both live in the host, so "a path must pass the workplace check first"
    /// cannot be circumvented.
    ///
    /// **Its own kind rather than folded into `Meta`**, because the two have different
    /// authorisation semantics: meta tools change the host itself, whereas file tools are
    /// "make the host do a job" — the same category as plugin tools. Folding them into `Meta`
    /// would pop an authorisation prompt on every read and write (`meta_tools` defaults to
    /// `ask`), while the `fs_*` plugin tools the model has are `auto` — one action with two
    /// behaviours, changing with whichever tool the model picks. That is impossible to explain.
    Host,
    /// A tool provided by some plugin.
    Plugin(String),
}

/// The tool abstraction. Every tool (meta / plugin) is transparent to the agent loop.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn kind(&self) -> ToolKind;
    fn description(&self) -> &str;
    /// JSON Schema for the parameters (an object type).
    fn parameters(&self) -> Value;

    /// Does one call of this tool count as a **read** or a **write** (write / delete)?
    /// Only used for the **workplace boundary** decision (see `agent::Agent::workplace_reject`).
    ///
    /// Defaults to **write**, which is deliberately the conservative direction: the host
    /// cannot guess whether a tool will read or write with a path, and missing a blocked
    /// write costs far more than blocking a read one time too many. Both categories give a
    /// more precise answer where they can:
    ///
    /// - **Host file tools** decide by name (the schema is ours, see [`host_tool_access`]);
    /// - **Plugin tools** decide by the access the **plugin itself declares**
    ///   (see `plugin::ToolDef::access`) — only a declared `"read"` opens things up;
    ///   without a declaration it still counts as a write.
    ///
    /// Declaring is **the plugin author's responsibility**; the host will not guess on their
    /// behalf. A read-only tool (scanning / counting / listing a directory) that adds the
    /// declaration gets the same access to paths outside the workplace as `read_file`; without
    /// it, the coarse net blocks it as a write — and the error the model gets spells out how
    /// to add the declaration (see `agent::PLUGIN_READ_HINT`).
    fn access(&self) -> crate::workplace::Access {
        crate::workplace::Access::Write
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": self.description(),
                "parameters": self.parameters(),
            }
        })
    }

    /// How long **this call** is allowed to run, in seconds — the number that gets shown to the user
    /// and that the outer guard is built from.
    ///
    /// Only the shell tools answer this: their ceiling is per call (`timeout_secs`, clamped to
    /// `[shell] max_timeout_secs`), not the flat `[agent] tool_timeout_secs` everything else gets.
    /// Kept separate from [`Tool::timeout_hint`] because a *budget* and the *leash wrapped around it*
    /// are different numbers, and a screen that says "cap 125 s" for a 120 s budget is a small lie
    /// that invites a bug report.
    fn cap_hint(&self, _args: &Value) -> Option<u64> {
        None
    }

    /// The leash the **outer** guard in `Runtime::execute` should use.
    ///
    /// A command that asked for 300 s would otherwise be cut off at
    /// `[agent] tool_timeout_secs + 30 s` — before its own timeout could fire — and the error would
    /// blame "the tool" while the process tree was left to the Job's kill-on-close. A hint here is how
    /// that outer guard learns to stay out of the way.
    ///
    /// Default: derived from [`Tool::cap_hint`] plus a few seconds of slack, so the two can never
    /// disagree. `None` = "I have no opinion", and the outer guard uses its flat rule.
    fn timeout_hint(&self, args: &Value) -> Option<std::time::Duration> {
        self.cap_hint(args).map(|s| std::time::Duration::from_secs(s + 5))
    }

    /// How long the model **expects** this call to take, in seconds — its own announcement
    /// ("this is a compile, give me two minutes").
    ///
    /// Only the shell tools answer this, and only when the model chose to say so. It is not a limit
    /// and nothing enforces it: it exists so the UI can show "expected ~2 min" next to the elapsed
    /// clock, which is the difference between "working" and "wedged" from the user's chair. A tool
    /// that has no idea simply does not answer, and the UI shows the cap instead.
    fn expect_hint(&self, _args: &Value) -> Option<u64> {
        None
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many host tools every doc comment in the repo quotes.
    ///
    /// Pinned against the **actual constructors** because the number has already drifted once: the
    /// docs said "12" long after `find_tools` made it 13, and nothing failed — a doc number nobody
    /// executes is a comment, not a fact. Adding a host tool now breaks this test first.
    #[test]
    fn the_quoted_host_tool_count_matches_reality() {
        const DOCUMENTED: usize = 17;
        let n = meta::meta_tools().len()
            + hostfs::host_file_tools().len()
            + ask::ask_tools().len()
            + discover::find_tools().len()
            + task::task_tools().len()
            + verify::verify_tools().len()
            + shell::shell_tools(&crate::config::ShellConfig::default()).len()
            + job::job_tools().len()
            + web::web_tools(&crate::config::WebConfig::default()).len();
        assert_eq!(n, DOCUMENTED, "update every doc that quotes the host tool count");
    }
}
