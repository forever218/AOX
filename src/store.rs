//! The storage seam (the `Store` seam).
//!
//! It takes "where conversation history lives, in what format it is written, and how it is
//! read back" out of the caller's hands. Callers (`Agent` / CLI / GUI) deal with only two
//! traits and no longer build paths themselves:
//!
//! - [`Store`]: backend-level (one directory / one database). `Runtime` holds an `Arc<dyn Store>`.
//! - [`Session`]: one **open conversation** (the write side), produced by [`Store::create`] /
//!   [`Store::open`].
//!
//! **Paths are deliberately absent from the interface**: mappings like `dir` / `file_of`
//! ("id ↔ filename") are implementation details of the jsonl backend (see [`jsonl`]). The moment a
//! path parameter appears on a trait, switching backends later (e.g. sqlite holding all
//! conversations in one database) becomes impossible — that is the easiest way for a "seam" to go
//! wrong. So callers always speak in terms of **id**.
//!
//! Only the jsonl backend exists for now, but the `config.toml [store] backend` entry is set up
//! early so that "switching backends" later means changing config, not code.

pub mod jsonl;

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::history::{Loaded, Summary, UsageRecord};
use crate::message::Message;

/// One open conversation (the write side).
///
/// Its methods correspond one-to-one with the old `history::Log` and have exactly
/// the same semantics: a failed append does not error (history must not break a
/// running conversation), and `rewrite` must preserve the activity stream and
/// usage lines.
pub trait Session: Send {
    /// The conversation id (== the filename stem).
    fn id(&self) -> &str;
    /// Where it lands on disk. **For display only** — callers must not use it to
    /// build other paths.
    ///
    /// `allow(dead_code)`: `src/main.rs` and `src/lib.rs` each compile the same set
    /// of modules (see the top of lib.rs); the CLI build unit speaks only in ids
    /// and has no use for it. It is kept for "show where this conversation lands"
    /// and for test assertions (the same situation as `Runtime::add_prompt_segment`).
    #[allow(dead_code)]
    fn path(&self) -> &Path;
    /// Append a message (`role == "system"` is skipped; the system prompt changes every round).
    fn append(&mut self, msg: &Message, is_error: Option<bool>);
    /// Append an activity-stream record.
    fn append_act(&mut self, act: &Value);
    /// Append one round's token ledger.
    fn append_usage(&mut self, rec: &UsageRecord);
    /// Append a "this conversation's workplace" record (the current mode + bound
    /// root + the plot this conversation's default tier created for itself).
    ///
    /// One record is written for the first message of each conversation and
    /// another on every mode switch; when reading, **take the last one**. It is
    /// stored for two reasons: ① restore that plot when reopening an old
    /// conversation; ② so that "switch to full access and back to default" need
    /// not create another plot (see [`crate::workplace::Record`]).
    fn append_workplace(&mut self, rec: &crate::workplace::Record);
    /// Rewrite the whole file with the given message list (interrupt rollback);
    /// the activity stream and usage are preserved as-is.
    fn rewrite(&mut self, msgs: &[Message]);
}

/// The storage backend (directory-level).
pub trait Store: Send + Sync {
    /// The backend name, i.e. the value of `config.toml [store] backend`.
    fn backend(&self) -> &'static str;
    /// The data location (for the UI to show "where history is kept").
    fn location(&self) -> &Path;
    /// Create a new conversation and write the header record.
    fn create(&self, model: Option<&str>) -> Result<Box<dyn Session>>;
    /// Continue an existing conversation (keep writing by id).
    fn open(&self, id: &str) -> Result<Box<dyn Session>>;
    /// List conversations, **most recently used first**.
    fn list(&self, limit: usize) -> Vec<Summary>;
    /// Resolve "the index in the list" or "id / id prefix" into one entry.
    fn resolve(&self, key: &str) -> Result<Summary>;
    /// Read the whole conversation's messages (bad lines skipped).
    fn load(&self, id: &str) -> Result<Vec<Loaded>>;
    /// Read the activity-stream records (payload returned as-is).
    ///
    /// `allow(dead_code)`: the timeline is read by the GUI (a consumer of the lib); the CLI build
    /// unit has no caller for it (see the top of lib.rs for the two-build explanation).
    #[allow(dead_code)]
    fn load_acts(&self, id: &str) -> Vec<Value>;
    /// Read the per-round token usage.
    fn load_usage(&self, id: &str) -> Vec<UsageRecord>;
    /// Read this conversation's **last** workplace record; `None` if one was
    /// never written (old conversation files have none).
    ///
    /// What `None` means is up to the caller: `Agent::resume` chooses to **leave**
    /// the current state alone — if we don't know which plot that conversation
    /// used, we should not guess on the user's behalf.
    fn load_workplace(&self, id: &str) -> Option<crate::workplace::Record>;
    /// Delete a conversation and return the entry that was removed.
    fn remove(&self, key: &str) -> Result<Summary>;
    /// Keep only the newest `keep` conversations (`0` = no limit) and return the
    /// number deleted.
    ///
    /// The everyday path does not need a caller to invoke it — creating a new
    /// conversation makes the backend trim automatically per `config.toml
    /// [history] keep`. It stays on the trait as a slot reserved for entries like
    /// "manual cleanup", and as a guarantee that "switching backends preserves the
    /// same semantics". `allow(dead_code)`: there really is no caller right now.
    #[allow(dead_code)]
    fn prune(&self, keep: usize) -> usize;
}

/// The known backend names. When adding a backend, update here and [`backend`] together.
pub const BACKENDS: [&str; 1] = ["jsonl"];

/// The default backend name — used when `config.toml` does not set `[store] backend`.
pub fn default_backend() -> &'static str {
    BACKENDS[0]
}

/// Build a default backend instance — **cannot fail** (the backend name is hard-coded to
/// [`default_backend`]).
///
/// For the cases "config not yet read" and "just want to read history": the GUI must still show
/// history in the sidebar even when config reading fails, and at that point it has no `Runtime`
/// and thus no backend name from config — reading history must never disappear just because
/// something else broke.
pub fn default_store(root: &Path, keep: usize) -> Arc<dyn Store> {
    Arc::new(jsonl::JsonlStore::new(root, keep))
}

/// Build a backend by name.
///
/// **An unrecognized name errors outright** rather than silently falling back to
/// jsonl: starting up as usual despite a wrong config shows up as "I clearly
/// configured a different backend, why is it still writing jsonl" — the hardest
/// class of problem to diagnose. (Phase 2 already got burned once by "the file
/// was written but didn't take effect"; see the `--theme` entry.)
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

    /// An unrecognized backend name must **error**, not silently fall back to jsonl.
    ///
    /// This guards the hardest-to-diagnose trap of "config set but not taking effect": once it
    /// silently falls back, the user sees "I clearly configured a different backend, why is it
    /// still writing jsonl", with not a single hint in the startup log.
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
    /// name should resolve to the jsonl backend, located at `<root>/history`.
    #[test]
    fn default_backend_resolves_to_jsonl() {
        let root: PathBuf = std::env::temp_dir().join("aox-store-default-test");
        assert_eq!(default_backend(), "jsonl");
        for name in ["", "jsonl", "  jsonl  "] {
            let s = backend(name, &root, 0).unwrap_or_else(|e| panic!("{name:?} should be usable: {e:#}"));
            assert_eq!(s.backend(), "jsonl");
            assert!(s.location().ends_with("history"), "location: {}", s.location().display());
        }
        assert_eq!(default_store(&root, 0).backend(), "jsonl");
    }
}
