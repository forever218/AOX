pub mod ask;
pub mod hostfs;
pub mod meta;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::runtime::Runtime;

/// Names of the host's built-in **file tools** (same order as `hostfs::host_file_tools()`).
///
/// This is a constant table rather than a literal repeated in four `name()` methods because
/// the names have to be recognised in **two** places: ① what each tool in `hostfs` calls
/// itself, and ② `agent::authorize_and_execute` deciding "is this call a host file tool?".
/// Both sides referring to the same constant rules out the nastiest kind of drift — rename
/// a tool and the boundary check silently stops applying (the tool still works, it just has
/// no boundary any more).
pub(crate) const HOST_FILE_TOOLS: [&str; 5] =
    ["read_file", "write_file", "list_dir", "remove", "attach_file"];

/// Which entries in that table are **read-only** (by index) — reads are unrestricted, only
/// writes and deletes go through the workplace boundary (the 2026-10-04 semantics).
///
/// Split out of `host_tool_access` into its own constant so that "which ones are reads" has
/// exactly **one** statement of record: `attach_file` counts as a read (attaching only hands
/// the file over, nothing is written). Adding a read-only tool later means changing this line
/// and nothing else — the `if` below cannot fall out of sync.
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

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput;
}
