//! The storage seam (the `Store` seam).
//! It takes "where conversation history lives, in what format it is written, and how it is read back" out...
//! Store: backend-level (one directory / one database). `Runtime` holds an `Arc<dyn Store>`. - Session: one open...

pub mod jsonl;

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::history::{Loaded, Summary, UsageRecord};
use crate::message::Message;

/// One open conversation (the write side).
pub trait Session: Send {
    /// The conversation id (== the filename stem)
    fn id(&self) -> &str;
    /// Where it lands on disk. For display only — callers must not use it to build other paths.
    #[allow(dead_code)]
    fn path(&self) -> &Path;
    /// Append a message (`role == "system"` is skipped; the system prompt changes every round).
    fn append(&mut self, msg: &Message, is_error: Option<bool>);
    /// Append an activity-stream record
    fn append_act(&mut self, act: &Value);
    /// Append one round's token ledger.
    fn append_usage(&mut self, rec: &UsageRecord);
    /// Append a "this conversation's workplace" record (the current mode + bound root + the plot this conversation's...
    fn append_workplace(&mut self, rec: &crate::workplace::Record);
    /// Append "this conversation is called X now" (`None` = drop the user's name and go back to the automatic...
    #[allow(dead_code)]
    fn append_title(&mut self, title: Option<&str>);
    /// Append a task checkpoint (`kind:"task"`), the long-running task's goal / constraints / progress / next steps...
    fn append_task(&mut self, task: &Value);
    /// Rewrite the whole file with the given message list (interrupt rollback);
    /// the activity stream and usage are preserved as-is.
    fn rewrite(&mut self, msgs: &[Message]);
}

/// Birth metadata of a new session: grouping info in first-line meta only, not location; defaults to local.
#[derive(Debug, Clone, Default)]
pub struct Birth {
    pub origin: crate::history::Origin,
    /// Id of the originating space; remote is builtin:remote, local is empty and grouped via workplace.
    pub space: Option<String>,
    /// The remote room (id, name).
    pub room: Option<(String, String)>,
}

impl Birth {
    /// A new session started from the phone: fixed to the Remote space and carries the room name.
    pub fn remote(room_id: &str, room_name: &str) -> Self {
        Self {
            origin: crate::history::Origin::Remote,
            space: Some(crate::space::ID_REMOTE.to_string()),
            room: Some((room_id.to_string(), room_name.to_string())),
        }
    }
}

/// The storage backend (directory-level)
pub trait Store: Send + Sync {
    /// The backend name, i.e. the value of `config.toml [store] backend`.
    fn backend(&self) -> &'static str;
    /// Where conversations are kept — what the UI shows under "history lives here", and what the CLI's `/history dir`...
    fn location(&self) -> &Path;
    /// Create a new conversation and write the header record.
    fn create(&self, model: Option<&str>, birth: &Birth) -> Result<Box<dyn Session>>;
    /// Continue an existing conversation (keep writing by id).
    fn open(&self, id: &str) -> Result<Box<dyn Session>>;
    /// List conversations, **most recently used first
    fn list(&self, limit: usize) -> Vec<Summary>;
    /// Resolve "the index in the list" or "id / id prefix" into one entry.
    fn resolve(&self, key: &str) -> Result<Summary>;
    /// Read the whole conversation's messages (bad lines skipped).
    fn load(&self, id: &str) -> Result<Vec<Loaded>>;
    /// Read the activity-stream records (payload returned as-is).
    #[allow(dead_code)]
    fn load_acts(&self, id: &str) -> Vec<Value>;
    /// Read the per-round token usage
    fn load_usage(&self, id: &str) -> Vec<UsageRecord>;
    /// Read this conversation's last workplace record; `None` if one was never written (old conversation files have...
    fn load_workplace(&self, id: &str) -> Option<crate::workplace::Record>;
    /// Read this conversation's last task checkpoint (`kind:"task"`), as the raw payload.
    fn load_task(&self, id: &str) -> Option<Value>;
    /// Delete a conversation and return the entry that was removed.
    fn remove(&self, key: &str) -> Result<Summary>;
    /// Keep only the newest `keep` conversations (`0` = no limit) and return the number deleted.
    #[allow(dead_code)]
    fn prune(&self, keep: usize) -> usize;
}

/// The known backend names. When adding a backend, update here and [`backend`] together.
pub const BACKENDS: [&str; 1] = ["jsonl"];

/// How long a conversation title may be, in characters (not bytes, so CJK is never cut in half).
pub const TITLE_MAX_CHARS: usize = 46;

/// The default backend name — used when `config.toml` does not set `[store] backend`.
pub fn default_backend() -> &'static str {
    BACKENDS[0]
}

/// Build a default backend instance — cannot fail (the backend name is hard-coded to default_backend).
pub fn default_store(root: &Path, keep: usize) -> Arc<dyn Store> {
    Arc::new(jsonl::JsonlStore::new(root, keep))
}

/// Build a backend by name.
/// An unrecognized name errors outright rather than silently falling back to jsonl: starting up as usual despite...
pub fn backend(name: &str, root: &Path, keep: usize) -> Result<Arc<dyn Store>> {
    let name = name.trim();
    // Empty = not configured = use the default backend; it's not "written wrong",
    // so don't classify it as unknown.
    if name.is_empty() || name == default_backend() {
        return Ok(default_store(root, keep));
    }
    bail!(crate::tf!(
        "unknown storage backend: {} (currently available: {})\n\
         check [store] backend in config.toml, or delete it to use the default.",
        name,
        BACKENDS.join(" / ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// An unrecognized backend name must error, not silently fall back to jsonl.
    #[test]
    fn unknown_backend_is_an_error() {
        let root = std::env::temp_dir();
        // Use `.err()` instead of `.unwrap_err()`: `Arc<dyn Store>` is not `Debug`.
        let e = backend("sqlite", &root, 0)
            .err()
            .expect("an unknown backend must error")
            .to_string();
        assert!(e.contains("sqlite"), "the error must name the wrong one: {e}");
        assert!(e.contains("jsonl"), "the error must tell the user what is available: {e}");
    }

    /// Both leaving it empty (unconfigured) and explicitly writing the default
    /// name should resolve to the jsonl backend, located at `<root>/.aox/conversations`.
    #[test]
    fn default_backend_resolves_to_jsonl() {
        let root: PathBuf = std::env::temp_dir().join("aox-store-default-test");
        assert_eq!(default_backend(), "jsonl");
        for name in ["", "jsonl", "  jsonl  "] {
            let s = backend(name, &root, 0).unwrap_or_else(|e| panic!("{name:?} should be usable: {e:#}"));
            assert_eq!(s.backend(), "jsonl");
            assert_eq!(
                s.location(),
                root.join(".aox").join("conversations"),
                "the single conversation store"
            );
        }
        assert_eq!(default_store(&root, 0).backend(), "jsonl");
    }
}
