//! Task-level file checkpoints and rollback.
//! The manager is hosted on `Runtime`, not inside `run_turn`: the GUI aborts a turn by dropping its future...

pub mod command;
pub mod gc;
pub mod model;
pub mod restore;
pub mod scan;
pub mod store;
pub mod view;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::config::{RecoveryConfig, ShellGuard};
use model::{
    BlobRef, Coverage, CoverageState, FileOp, PointKind, PointStatus, RecoveryPoint, TurnStatus, SCHEMA,
};
use store::{FileLock, Journal, RecoveryStore};

static TURN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Wall-clock ceiling for one workspace scan.
const SCAN_DEADLINE: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub enum RecoveryError {
    NoStore(PathBuf),
    OutsideWorkspace(String),
    UnsupportedEntry(String),
    LimitExceeded { path: PathBuf, size: u64, cap: u64 },
    BackupUnavailable(String),
    Locked(String),
    Corrupt { path: PathBuf, reason: String },
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoStore(p) => write!(f, "no recovery store at {}", p.display()),
            Self::OutsideWorkspace(s) => write!(f, "{s}"),
            Self::UnsupportedEntry(s) => write!(f, "{s}"),
            Self::LimitExceeded { path, size, cap } => write!(
                f,
                "{} is {size} bytes, beyond the recovery cap of {cap} bytes",
                path.display()
            ),
            Self::BackupUnavailable(s) => write!(f, "{s}"),
            Self::Locked(s) => write!(f, "{s}"),
            Self::Corrupt { path, reason } => {
                write!(f, "recovery data {} is unusable: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for RecoveryError {}

pub type RecoveryResult<T> = Result<T, RecoveryError>;

#[derive(Clone, Debug)]
pub struct RecoveryBlock {
    #[allow(dead_code)] // Kept separate for a future user-facing toast; tools only return `model`.
    pub user: String,
    pub model: String,
}

impl RecoveryBlock {
    fn from_error(e: &RecoveryError) -> Self {
        let text = e.to_string();
        Self {
            user: text.clone(),
            model: format!(
                "the host refused this write because its previous contents could not be saved for rollback: {text}\nThe file was not changed. Do not retry the same write blindly; report the reason to the user (free space, file size, or a symlink/unsupported entry)."
            ),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RecoveryFinish {
    #[allow(dead_code)] // Read by tests and available to front ends; the turn only needs warnings.
    pub point: Option<String>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct FileCapture {
    token: Option<CaptureToken>,
    pub warning: Option<String>,
}

#[derive(Clone, Debug)]
struct CaptureToken {
    turn: u64,
    rel: String,
}

#[derive(Clone, Debug, Default)]
pub struct TreeCapture {
    token: Option<TreeToken>,
    pub warning: Option<String>,
}

#[derive(Clone, Debug)]
struct TreeToken {
    turn: u64,
    rels: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct OpaqueSummary {
    /// Workspace-relative paths whose content this command changed
    pub changed: Vec<String>,
    /// Why the coverage is not complete (truncated walk, unstored content, unreadable path).
    pub notes: Vec<String>,
    /// The recovery point must be recorded as `partial`.
    pub partial: bool,
}

impl OpaqueSummary {
    /// The line tools append to their output, or `None` when there is nothing to say.
    pub fn note_text(&self) -> Option<String> {
        if self.notes.is_empty() {
            return None;
        }
        Some(format!("recovery: {}", self.notes.join("; ")))
    }
}

/// One shell command's window: the before-image index, kept until the follow-up scan.
pub struct OpaqueGuard {
    before: Option<scan::Scan>,
    active: Arc<ActiveTurn>,
    /// Something the *before* scan wants to say even if the command turns out to change nothing —
    /// today, that caching this workspace's before-images cost a lot of disk.
    note: Option<String>,
}

/// Above this, the first before-image pass of a workspace is worth mentioning.
const BASELINE_WARN_BYTES: u64 = 8 * 1024 * 1024;

impl Drop for OpaqueGuard {
    fn drop(&mut self) {
        if self.before.is_none() {
            return;
        }
        let mut st = self.active.state.lock().unwrap_or_else(|e| e.into_inner());
        st.partial.push(
            "a shell command ended without its follow-up scan (the turn was interrupted), so whatever it wrote is not covered by rollback".into(),
        );
    }
}

impl fmt::Debug for OpaqueGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpaqueGuard").field("open", &self.before.is_some()).finish()
    }
}

#[derive(Clone, Debug)]
pub struct TurnStart {
    #[allow(dead_code)]
    pub tx: String,
}

struct ActiveTurn {
    turn: u64,
    root: PathBuf,
    conversation: Option<String>,
    label: String,
    started_at: u64,
    state: Mutex<ActiveState>,
}

struct ActiveState {
    store: Option<Arc<RecoveryStore>>,
    _lock: Option<FileLock>,
    journal: Option<Journal>,
    parent: Option<String>,
    ops: BTreeMap<String, FileOp>,
    partial: Vec<String>,
    finished: bool,
    /// Store bytes at transaction start; used to enforce `max_turn_mb`.
    start_bytes: u64,
}

impl ActiveState {
    fn empty() -> Self {
        Self {
            store: None,
            _lock: None,
            journal: None,
            parent: None,
            ops: BTreeMap::new(),
            partial: Vec::new(),
            finished: false,
            start_bytes: 0,
        }
    }
}

pub struct RecoveryManager {
    cfg: RecoveryConfig,
    #[allow(dead_code)]
    app_root: PathBuf,
    active: Mutex<Option<Arc<ActiveTurn>>>,
}

impl RecoveryManager {
    pub fn new(cfg: RecoveryConfig, app_root: PathBuf) -> Self {
        Self { cfg, app_root, active: Mutex::new(None) }
    }

    #[allow(dead_code)] // Used by the recovery unit tests; command.rs owns the runtime-facing path.
    pub fn open_store(&self, root: &Path, create: bool) -> RecoveryResult<Arc<RecoveryStore>> {
        RecoveryStore::open(root, &self.cfg, create).map(Arc::new)
    }

    pub fn begin_turn(
        &self,
        root: &Path,
        conversation: Option<&str>,
        label: &str,
    ) -> RecoveryResult<Option<TurnStart>> {
        if !self.cfg.enabled {
            return Ok(None);
        }
        let mut slot = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Err(RecoveryError::BackupUnavailable(
                "the previous recovery transaction is still open; finish or abort it first".into(),
            ));
        }
        let turn = TURN_SEQ.fetch_add(1, Ordering::Relaxed);
        let label = clamp_chars(label.trim(), 80);
        let active = Arc::new(ActiveTurn {
            turn,
            root: root.to_path_buf(),
            conversation: conversation.map(str::to_string),
            label,
            started_at: crate::history::unix_now(),
            state: Mutex::new(ActiveState::empty()),
        });
        *slot = Some(active);
        Ok(Some(TurnStart { tx: format!("{turn}") }))
    }

    fn current(&self) -> Option<Arc<ActiveTurn>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn capture_before(&self, path: &Path) -> Result<FileCapture, RecoveryBlock> {
        let Some(active) = self.current() else {
            return Ok(FileCapture::default());
        };
        let mut st = active.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.finished {
            return Ok(FileCapture::default());
        }
        let store = match self.ensure_store(&active, &mut st) {
            Ok(s) => s,
            Err(e) => return Err(RecoveryBlock::from_error(&e)),
        };
        let rel = match store.relative(path) {
            Ok(Some(rel)) => rel,
            Ok(None) => {
                return Ok(FileCapture {
                    token: None,
                    warning: Some(format!(
                        "{} is in an excluded recovery path; this write is not covered by rollback",
                        path.display()
                    )),
                })
            }
            Err(RecoveryError::OutsideWorkspace(reason)) => {
                return Ok(FileCapture {
                    token: None,
                    warning: Some(format!(
                        "{reason}; this write is outside the protected workspace and not covered by rollback"
                    )),
                })
            }
            Err(e) => return Err(RecoveryBlock::from_error(&e)),
        };

        let before = match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(RecoveryBlock::from_error(&RecoveryError::UnsupportedEntry(format!(
                    "{} is a symlink/reparse point; refusing to track its write",
                    path.display()
                ))))
            }
            Ok(meta) if meta.is_file() => {
                if !within_turn_budget(&st, &store, meta.len(), self.cfg.max_turn_mb) {
                    return Err(RecoveryBlock::from_error(&RecoveryError::BackupUnavailable(
                        format!(
                            "this task has reached its {} MiB recovery budget; the write was refused",
                            self.cfg.max_turn_mb
                        ),
                    )));
                }
                match store.put_file(path, mb(self.cfg.max_file_mb)) {
                    Ok(b) => Some(b),
                    Err(e) => return Err(RecoveryBlock::from_error(&e)),
                }
            }
            Ok(_) => {
                return Err(RecoveryBlock::from_error(&RecoveryError::UnsupportedEntry(format!(
                    "{} is not a regular file",
                    path.display()
                ))))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(RecoveryBlock::from_error(&RecoveryError::BackupUnavailable(
                    format!("{}: {e}", path.display()),
                )))
            }
        };

        st.ops.entry(rel.clone()).or_insert_with(|| FileOp {
            path: rel.clone(),
            kind: model::FileOpKind::Modify,
            before: before.clone(),
            after: before.clone(),
        });
        if let Some(journal) = st.journal.as_mut() {
            let _ = journal.append(
                &json!({"t":"intent","path":rel,"before":before.as_ref().map(|b| &b.hash),"at":crate::history::unix_now()}),
                true,
            );
        }
        Ok(FileCapture {
            token: Some(CaptureToken { turn: active.turn, rel }),
            warning: None,
        })
    }

    pub fn capture_after(&self, cap: FileCapture) -> Option<String> {
        let token = cap.token?;
        let Some(active) = self.current() else {
            return Some("the recovery transaction ended before this write could be recorded".into());
        };
        if active.turn != token.turn {
            return Some("this write belongs to an older recovery transaction".into());
        }
        let mut st = active.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(store) = st.store.clone() else {
            return Some("the recovery store was not open after the write".into());
        };
        let abs = match store.absolute(&token.rel) {
            Ok(p) => p,
            Err(e) => return Some(format!("could not map {} back to disk: {e}", token.rel)),
        };
        let (after, warn) = match std::fs::symlink_metadata(&abs) {
            Ok(meta) if meta.is_file() => {
                let fits_turn = within_turn_budget(&st, &store, meta.len(), self.cfg.max_turn_mb);
                let observed = if fits_turn {
                    store.observe_file(&abs, mb(self.cfg.max_file_mb))
                } else {
                    store.hash_file_only(&abs)
                };
                match observed {
                    Ok(b) => {
                        let warn = if !fits_turn {
                            Some(format!(
                                "{} pushed this task over its recovery budget; rollback is available but redo is not",
                                token.rel
                            ))
                        } else {
                            None
                        };
                        (Some(b), warn)
                    }
                    Err(e) => (None, Some(e.to_string())),
                }
            }
            Ok(_) => (None, Some(format!("{} is no longer a regular file", token.rel))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, None),
            Err(e) => (None, Some(format!("{}: {e}", token.rel))),
        };
        if let Some(op) = st.ops.get_mut(&token.rel) {
            op.after = after.clone();
        }
        if let Some(op) = st.ops.get(&token.rel) {
            if op.is_noop() {
                st.ops.remove(&token.rel);
            }
        }
        if let Some(journal) = st.journal.as_mut() {
            let _ = journal.append(
                &json!({"t":"after","path":token.rel,"after":after.as_ref().map(|b| &b.hash),"at":crate::history::unix_now()}),
                true,
            );
        }
        cap.warning.or(warn)
    }

    pub fn capture_tree_before(&self, path: &Path) -> Result<TreeCapture, RecoveryBlock> {
        let Some(active) = self.current() else {
            return Ok(TreeCapture::default());
        };
        let mut st = active.state.lock().unwrap_or_else(|e| e.into_inner());
        let store = match self.ensure_store(&active, &mut st) {
            Ok(s) => s,
            Err(e) => return Err(RecoveryBlock::from_error(&e)),
        };
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                return Err(RecoveryBlock::from_error(&RecoveryError::BackupUnavailable(
                    format!("{}: {e}", path.display()),
                )))
            }
        };
        if meta.is_dir() {
            match store.relative(path) {
                Ok(None) => {
                    return Ok(TreeCapture {
                        token: None,
                        warning: Some("this directory is in an excluded recovery path; deletion is not covered".into()),
                    })
                }
                Err(RecoveryError::OutsideWorkspace(reason)) => {
                    return Ok(TreeCapture {
                        token: None,
                        warning: Some(format!("{reason}; deletion is not covered by rollback")),
                    })
                }
                Err(e) => return Err(RecoveryBlock::from_error(&e)),
                Ok(Some(_)) => {}
            }
        }
        let mut captured: Vec<(String, Option<BlobRef>)> = Vec::new();
        if meta.is_file() {
            let rel = match store.relative(path) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return Ok(TreeCapture {
                        token: None,
                        warning: Some("excluded recovery path; deletion is not covered".into()),
                    })
                }
                Err(RecoveryError::OutsideWorkspace(reason)) => {
                    return Ok(TreeCapture {
                        token: None,
                        warning: Some(format!("{reason}; deletion is not covered by rollback")),
                    })
                }
                Err(e) => return Err(RecoveryBlock::from_error(&e)),
            };
            let b = store
                .put_file(path, mb(self.cfg.max_file_mb))
                .map_err(|e| RecoveryBlock::from_error(&e))?;
            captured.push((rel, Some(b)));
        } else if meta.is_dir() {
            self.walk_tree(&store, path, &mut captured, st.start_bytes)
                .map_err(|e| RecoveryBlock::from_error(&e))?;
        } else {
            return Err(RecoveryBlock::from_error(&RecoveryError::UnsupportedEntry(format!(
                "{} is not a regular file or directory",
                path.display()
            ))));
        }
        // Journal every before-image **before** the delete. A crash after the recycle-bin call but
        // before `finish_tree_after` must still leave enough evidence to reconstruct the deletion.
        if let Some(journal) = st.journal.as_mut() {
            for (rel, before) in &captured {
                journal
                    .append(
                        &json!({"t":"intent","path":rel,"before":before.as_ref().map(|b| &b.hash),"at":crate::history::unix_now()}),
                        true,
                    )
                    .map_err(|e| RecoveryBlock::from_error(&e))?;
            }
        }
        let rels: Vec<String> = captured.iter().map(|(r, _)| r.clone()).collect();
        for (rel, before) in captured {
            st.ops.entry(rel.clone()).or_insert_with(|| FileOp {
                path: rel,
                kind: model::FileOpKind::Modify,
                before,
                after: None,
            });
        }
        Ok(TreeCapture {
            token: Some(TreeToken { turn: active.turn, rels }),
            warning: None,
        })
    }

    pub fn finish_tree_after(&self, cap: TreeCapture) -> Option<String> {
        let token = cap.token?;
        let Some(active) = self.current() else {
            return Some("the recovery transaction ended before this deletion could be recorded".into());
        };
        if active.turn != token.turn {
            return Some("this deletion belongs to an older recovery transaction".into());
        }
        let mut st = active.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(store) = st.store.clone() else {
            return Some("the recovery store was not open after the deletion".into());
        };
        let mut warnings = Vec::new();
        for rel in token.rels {
            let abs = match store.absolute(&rel) {
                Ok(p) => p,
                Err(e) => {
                    warnings.push(e.to_string());
                    continue;
                }
            };
            let after = match std::fs::symlink_metadata(&abs) {
                Ok(m) if m.is_file() => {
                    let fits = within_turn_budget(&st, &store, m.len(), self.cfg.max_turn_mb);
                    let observed = if fits {
                        store.observe_file(&abs, mb(self.cfg.max_file_mb))
                    } else {
                        store.hash_file_only(&abs)
                    };
                    match observed {
                        Ok(b) => Some(b),
                        Err(e) => {
                            warnings.push(e.to_string());
                            None
                        }
                    }
                }
                Ok(_) => None,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    warnings.push(format!("{rel}: {e}"));
                    None
                }
            };
            if let Some(op) = st.ops.get_mut(&rel) {
                op.after = after;
                if op.is_noop() {
                    st.ops.remove(&rel);
                }
            }
        }
        cap.warning.or_else(|| (!warnings.is_empty()).then(|| warnings.join("; ")))
    }

    fn walk_tree(
        &self,
        store: &RecoveryStore,
        dir: &Path,
        out: &mut Vec<(String, Option<BlobRef>)>,
        start_bytes: u64,
    ) -> RecoveryResult<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| RecoveryError::BackupUnavailable(format!("{}: {e}", dir.display())))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| RecoveryError::BackupUnavailable(e.to_string()))?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let p = entry.path();
            let meta = std::fs::symlink_metadata(&p)
                .map_err(|e| RecoveryError::BackupUnavailable(format!("{}: {e}", p.display())))?;
            if meta.file_type().is_symlink() {
                return Err(RecoveryError::UnsupportedEntry(format!(
                    "{} is a symlink/reparse point; refusing to back up only part of the tree",
                    p.display()
                )));
            }
            if meta.is_dir() {
                // Skip an excluded subtree before walking it; otherwise a large `target/` would be
                // traversed for nothing and a link inside it could block an unrelated delete.
                if store.relative(&p)?.is_none() {
                    continue;
                }
                self.walk_tree(store, &p, out, start_bytes)?;
            } else if meta.is_file() {
                let used = store.total_bytes().saturating_sub(start_bytes);
                let cap = mb(self.cfg.max_turn_mb);
                if cap > 0 && used.saturating_add(meta.len()) > cap {
                    return Err(RecoveryError::BackupUnavailable(format!(
                        "this task would exceed its {} MiB recovery budget while backing up a directory",
                        self.cfg.max_turn_mb
                    )));
                }
                let Some(rel) = store.relative(&p)? else { continue };
                let b = store.put_file(&p, mb(self.cfg.max_file_mb))?;
                out.push((rel, Some(b)));
            } else {
                return Err(RecoveryError::UnsupportedEntry(format!(
                    "{} is not a regular file",
                    p.display()
                )));
            }
        }
        Ok(())
    }

    /// Open the window around one shell command: scan the workspace *before* it runs.
    pub fn begin_opaque(
        &self,
        root: &Path,
        reason: &str,
    ) -> Result<Option<OpaqueGuard>, RecoveryBlock> {
        let Some(active) = self.current() else {
            return Ok(None);
        };
        let strict = match self.cfg.shell_guard {
            ShellGuard::Off => return Ok(None),
            ShellGuard::Strict => true,
            ShellGuard::BestEffort => false,
        };
        let (store, baseline, limits) = {
            let mut st = active.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.finished {
                return Ok(None);
            }
            let store = match self.ensure_store(&active, &mut st) {
                Ok(s) => s,
                // A store the host cannot open is a failure of *recovery*, not a reason to stop the user's work: `.aox/backup`...
                Err(e) => {
                    drop(st);
                    self.mark_partial(format!(
                        "{reason}: the recovery store is unavailable ({e}), so writes from this command are not covered"
                    ));
                    return Ok(None);
                }
            };
            let baseline = store.load_baseline();
            let limits = self.scan_limits(&store, &st);
            (store, baseline, limits)
        };

        let scope = if strict { scan::StoreScope::All } else { scan::StoreScope::OnlyChanged };
        let before = scan::scan(&store, root, &baseline, scope, &limits, mb(self.cfg.max_file_mb));

        if !before.complete() {
            let why = before.stopped.clone().unwrap_or_else(|| "the scan did not finish".into());
            if strict {
                return Err(RecoveryBlock::from_error(&RecoveryError::BackupUnavailable(format!(
                    "{reason}: the workspace scan did not finish — {why}. This call was **not** \
                     started, because anything it wrote could not be recorded for rollback. Either \
                     make the tree smaller (exclude a directory in [recovery] exclude, or raise \
                     metadata_memory_mb) or set [recovery] shell_guard = \"best_effort\" to run it \
                     with a recovery point that is explicitly marked partial."
                ))));
            }
            self.mark_partial(format!("{reason}: the workspace scan did not finish — {why}"));
        }
        if !before.skipped.is_empty() {
            self.mark_partial(format!(
                "{} symlink(s)/special file(s) were not followed (e.g. {}), so writes through them are not covered",
                before.skipped.len(),
                before.skipped.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !before.unreadable.is_empty() {
            self.mark_partial(format!(
                "{} path(s) could not be read before the command (e.g. {}), so their previous contents are unknown",
                before.unreadable.len(),
                before.unreadable.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        let note = (before.stored_bytes >= BASELINE_WARN_BYTES).then(|| {
            format!(
                "this workspace's before-images took {:.1} MiB ({} file(s)); large folders such as a \
                 toolchain or a build output belong in [recovery] exclude to keep that small",
                before.stored_bytes as f64 / (1024.0 * 1024.0),
                before.files.len()
            )
        });
        Ok(Some(OpaqueGuard { before: Some(before), active, note }))
    }

    /// Close the window: scan again, diff, and record what the command actually changed.
    pub fn finish_opaque(&self, mut guard: OpaqueGuard) -> OpaqueSummary {
        let Some(before) = guard.before.take() else {
            return OpaqueSummary::default();
        };
        let mut summary = OpaqueSummary::default();
        if let Some(note) = guard.note.take() {
            summary.notes.push(note);
        }
        let mut st = guard.active.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(store) = st.store.clone() else {
            summary.partial = true;
            summary.notes.push("the recovery store was not open after the command".into());
            return summary;
        };
        let root = guard.active.root.clone();
        let limits = self.scan_limits(&store, &st);
        let baseline_before = before.files.clone();
        // Strict keeps a full post-image; best effort keeps only what the command touched — otherwise
        // it would silently become strict, which is the one thing it promises not to do.
        let scope = if self.cfg.shell_guard == ShellGuard::Strict {
            scan::StoreScope::All
        } else {
            scan::StoreScope::OnlyChanged
        };
        let after =
            scan::scan(&store, &root, &baseline_before, scope, &limits, mb(self.cfg.max_file_mb));

        if !after.complete() {
            summary.partial = true;
            summary.notes.push(format!(
                "the follow-up scan did not finish ({}), so this command's coverage is partial",
                after.stopped.as_deref().unwrap_or("unknown reason")
            ));
        }

        let changes = scan::diff(&before, &after);
        summary.changed = changes.iter().map(|c| c.path.clone()).collect();
        let unstored = changes.iter().filter(|c| !c.reversible()).count();
        if unstored > 0 {
            summary.partial = true;
            summary.notes.push(format!(
                "{unstored} changed file(s) could not be stored (over the per-file or size cap): their hash is \
                 known, but restoring them is not possible"
            ));
        }

        for c in &changes {
            let rel = c.path.clone();
            if let Some(op) = st.ops.get_mut(&rel) {
                op.after = c.after.clone();
                let noop = op.is_noop();
                if noop {
                    st.ops.remove(&rel);
                }
            } else {
                st.ops.insert(
                    rel.clone(),
                    FileOp {
                        path: rel.clone(),
                        kind: model::FileOpKind::Modify,
                        before: c.before.clone(),
                        after: c.after.clone(),
                    },
                );
            }
            if let Some(journal) = st.journal.as_mut() {
                let _ = journal.append(
                    &json!({"t":"intent","path":rel,"before":c.before.as_ref().map(|b| &b.hash),"at":crate::history::unix_now()}),
                    true,
                );
            }
        }

        // A command can also put a path *back*: `edit_file` writes, then the command runs `git checkout`. The diff has...
        let changed_set: BTreeMap<&str, ()> =
            summary.changed.iter().map(|p| (p.as_str(), ())).collect();
        let tracked: Vec<String> = st.ops.keys().cloned().collect();
        for rel in tracked {
            if changed_set.contains_key(rel.as_str()) {
                continue;
            }
            if after.skipped.iter().any(|s| s == &rel)
                || after.unreadable.iter().any(|u| u.starts_with(&format!("{rel}:")))
            {
                continue;
            }
            let now = after.files.get(&rel).cloned();
            if let Some(op) = st.ops.get_mut(&rel) {
                if same_state(&op.after, &now) {
                    continue;
                }
                op.after = now;
                let noop = op.is_noop();
                if noop {
                    st.ops.remove(&rel);
                }
            }
        }

        // The baseline is the next command's before-image cache.
        if after.complete() {
            if let Err(e) = store.save_baseline(&after.files) {
                summary.notes.push(format!("the shell baseline could not be updated: {e}"));
            }
        }
        if summary.partial {
            let text = summary.notes.join("; ");
            st.partial.push(text);
        }
        summary
    }

    /// The ceilings one scan runs under, derived from the same quotas everything else uses.
    fn scan_limits(&self, store: &RecoveryStore, st: &ActiveState) -> scan::Limits {
        let max_entries = if self.cfg.metadata_memory_mb == 0 {
            0
        } else {
            self.cfg.metadata_memory_mb * 1024 * 1024 / scan::ENTRY_COST
        };
        // Content budget: whatever this task has left of `max_turn_mb` **and** whatever the store has
        // left of `max_total_mb`. Either one is enough to make a before-image unstorable.
        let turn_cap = mb(self.cfg.max_turn_mb);
        let turn_left = if turn_cap == 0 {
            0
        } else {
            turn_cap.saturating_sub(store.total_bytes().saturating_sub(st.start_bytes))
        };
        let total_cap = mb(self.cfg.max_total_mb);
        let total_left = if total_cap == 0 {
            0
        } else {
            total_cap.saturating_sub(store.total_bytes())
        };
        let max_bytes = match (turn_left, total_left) {
            (0, 0) => 0,
            (0, t) => t,
            (t, 0) => t,
            (a, b) => a.min(b),
        };
        scan::Limits { max_entries, max_bytes, deadline: SCAN_DEADLINE }
    }

    /// Record that this task's coverage is incomplete, with the reason the user will see.
    pub fn mark_partial(&self, reason: impl Into<String>) {
        let Some(active) = self.current() else { return };
        let mut st = active.state.lock().unwrap_or_else(|e| e.into_inner());
        st.partial.push(reason.into());
    }

    /// A background job writes whenever it wants, so no scan bracket can contain it: the recovery point is marked...
    pub fn note_background_job(&self, id: &str) {
        self.mark_partial(format!(
            "background job {id} is running, and its writes happen outside any scan, so files it changes are not covered; stop the job before relying on /rollback"
        ));
    }

    pub fn finish_turn(&self, status: TurnStatus) -> RecoveryFinish {
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(active) = active else {
            return RecoveryFinish::default();
        };
        let mut st = active.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.finished {
            return RecoveryFinish::default();
        }
        st.finished = true;
        let Some(store) = st.store.clone() else {
            return RecoveryFinish::default();
        };
        st.ops.retain(|_, op| {
            op.finish_kind();
            !op.is_noop()
        });
        if st.ops.is_empty() {
            if let Some(j) = st.journal.take() {
                j.remove();
            }
            return RecoveryFinish {
                warnings: st.partial.clone(),
                ..Default::default()
            };
        }

        let mut warnings = st.partial.clone();
        let coverage = if st.partial.is_empty() {
            Coverage::complete()
        } else {
            Coverage {
                state: CoverageState::Partial,
                scanned: 0,
                excluded_roots: self.cfg.exclude.clone(),
                reasons: st.partial.clone(),
            }
        };
        // If a touched path was changed outside AOX since the previous checkpoint, record that change as its own...
        let mut external_ops = Vec::new();
        for op in st.ops.values() {
            if op.is_noop() {
                continue;
            }
            match last_state_before(&store, st.parent.as_deref(), &op.path) {
                Ok(Some(prev)) if !same_state(&prev, &op.before) => {
                    let mut ext = FileOp {
                        path: op.path.clone(),
                        kind: model::FileOpKind::Modify,
                        before: prev,
                        after: op.before.clone(),
                    };
                    ext.finish_kind();
                    external_ops.push(ext);
                }
                Ok(_) => {}
                Err(e) => warnings.push(format!("could not check external drift for {}: {e}", op.path)),
            }
        }
        let mut parent = st.parent.clone();
        if !external_ops.is_empty() {
            let ext = RecoveryPoint {
                schema: SCHEMA,
                id: store::new_id("rp"),
                parent: parent.clone(),
                kind: PointKind::External,
                status: PointStatus::Completed,
                created_at: active.started_at,
                finished_at: crate::history::unix_now(),
                conversation: active.conversation.clone(),
                turn: None,
                label: "external changes before this task".into(),
                ops: external_ops,
                coverage: Coverage::complete(),
                redoable: true,
            };
            if let Err(e) = store.save_node(&ext) {
                warnings.push(format!("could not record external changes: {e}"));
            } else {
                parent = Some(ext.id);
            }
        }

        let id = store::new_id("rp");
        let ops: Vec<FileOp> = std::mem::take(&mut st.ops).into_values().collect();
        let redoable = ops
            .iter()
            .all(|op| op.after.as_ref().map(|b| b.stored).unwrap_or(true));
        let node = RecoveryPoint {
            schema: SCHEMA,
            id: id.clone(),
            parent,
            kind: PointKind::Turn,
            status: status.as_status(),
            created_at: active.started_at,
            finished_at: crate::history::unix_now(),
            conversation: active.conversation.clone(),
            turn: Some(active.turn),
            label: active.label.clone(),
            ops,
            coverage,
            redoable,
        };
        if let Err(e) = store.save_node(&node) {
            warnings.push(e.to_string());
            return RecoveryFinish { point: None, warnings, error: Some(e.to_string()) };
        }
        let mut cursor = match store.cursor() {
            Ok(c) => c,
            Err(e) => {
                warnings.push(e.to_string());
                return RecoveryFinish { point: Some(id), warnings, error: Some(e.to_string()) };
            }
        };
        cursor.head = Some(node.id.clone());
        cursor.redo.clear();
        cursor.updated_at = crate::history::unix_now();
        if let Err(e) = store.save_cursor(&cursor) {
            warnings.push(e.to_string());
            return RecoveryFinish { point: Some(id), warnings, error: Some(e.to_string()) };
        }
        if let Some(journal) = st.journal.as_mut() {
            let _ = journal.append(
                &json!({"t":"finish","node":node.id,"at":crate::history::unix_now()}),
                true,
            );
        }
        if let Some(j) = st.journal.take() {
            j.remove();
        }
        // Housekeeping runs after the commit and cannot fail it: the task's files are already on disk and its recovery...
        warnings.extend(gc::maintain(&store, self.cfg.keep, self.cfg.max_total_mb));
        RecoveryFinish { point: Some(id), warnings, error: None }
    }

    pub fn abort_turn(&self) -> RecoveryFinish {
        self.finish_turn(TurnStatus::Interrupted)
    }

    fn ensure_store(
        &self,
        active: &ActiveTurn,
        st: &mut ActiveState,
    ) -> RecoveryResult<Arc<RecoveryStore>> {
        if let Some(s) = &st.store {
            return Ok(s.clone());
        }
        let store = Arc::new(RecoveryStore::open(&active.root, &self.cfg, true)?);
        // Lock before inspecting pending journals: recovery may itself write files or the cursor.
        let lock = store.lock()?;
        restore::recover_pending(&store, &self.cfg)?;
        st.start_bytes = store.total_bytes();
        let cursor = store.cursor()?;
        let mut journal = store.new_journal()?;
        journal.append(
            &json!({
                "t":"begin",
                "tx":journal.path().file_stem().and_then(|s| s.to_str()).unwrap_or("tx"),
                "parent":cursor.head,
                "conversation":active.conversation,
                "at":crate::history::unix_now()
            }),
            true,
        )?;
        st.parent = cursor.head;
        st.journal = Some(journal);
        st._lock = Some(lock);
        st.store = Some(store.clone());
        Ok(store)
    }
}

fn mb(n: u64) -> u64 {
    n.saturating_mul(1024 * 1024)
}

fn within_turn_budget(
    st: &ActiveState,
    store: &RecoveryStore,
    additional: u64,
    max_turn_mb: u64,
) -> bool {
    let cap = mb(max_turn_mb);
    if cap == 0 {
        return true;
    }
    let used = store.total_bytes().saturating_sub(st.start_bytes);
    used.saturating_add(additional) <= cap
}

fn same_state(a: &Option<BlobRef>, b: &Option<BlobRef>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.hash == y.hash && x.readonly == y.readonly,
        _ => false,
    }
}

/// Find the last recorded state of `path` at or before `parent`. `None` means this path was never
/// tracked, not that it was absent (`Some(None)` means tracked as absent).
fn last_state_before(
    store: &RecoveryStore,
    parent: Option<&str>,
    path: &str,
) -> RecoveryResult<Option<Option<BlobRef>>> {
    let mut cur = parent.map(str::to_string);
    while let Some(id) = cur {
        let node = match store.load_node(&id) {
            Ok(n) => n,
            Err(RecoveryError::Corrupt { .. }) => break,
            Err(e) => return Err(e),
        };
        if let Some(op) = node.ops.iter().find(|op| op.path == path) {
            return Ok(Some(op.after.clone()));
        }
        cur = node.parent;
    }
    Ok(None)
}

fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aox-recovery-mgr-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_inactive_manager_never_touches_disk() {
        let d = tmp("inactive");
        let cfg = RecoveryConfig::default();
        let m = RecoveryManager::new(cfg, d.clone());
        let f = d.join("a.txt");
        std::fs::write(&f, "x").unwrap();
        let cap = m.capture_before(&f).unwrap();
        assert!(cap.token.is_none());
        assert!(!d.join(".aox").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_write_creates_one_node_and_rollback_material_is_present() {
        let d = tmp("node");
        let mut cfg = RecoveryConfig::default();
        cfg.max_file_mb = 4;
        let m = RecoveryManager::new(cfg, d.clone());
        let f = d.join("a.txt");
        std::fs::write(&f, "before").unwrap();
        m.begin_turn(&d, Some("c1"), "test").unwrap();
        let cap = m.capture_before(&f).unwrap();
        std::fs::write(&f, "after").unwrap();
        m.capture_after(cap);
        let fin = m.finish_turn(TurnStatus::Completed);
        assert!(fin.error.is_none(), "{:?}", fin);
        let point = fin.point.expect("a node");
        let store = m.open_store(&d, false).unwrap();
        let node = store.load_node(&point).unwrap();
        assert_eq!(node.ops.len(), 1);
        assert_eq!(node.ops[0].path, "a.txt");
        assert!(node.ops[0].before.as_ref().unwrap().stored);
        assert!(node.ops[0].after.as_ref().unwrap().stored);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_partial_reason_survives_into_the_node() {
        let d = tmp("partial");
        let cfg = RecoveryConfig::default();
        let m = RecoveryManager::new(cfg, d.clone());
        let f = d.join("a.txt");
        std::fs::write(&f, "before").unwrap();
        m.begin_turn(&d, None, "partial").unwrap();
        let c = m.capture_before(&f).unwrap();
        std::fs::write(&f, "after").unwrap();
        m.capture_after(c);
        m.mark_partial("shell writes are not captured");
        let id = m.finish_turn(TurnStatus::Completed).point.unwrap();
        let store = m.open_store(&d, false).unwrap();
        let node = store.load_node(&id).unwrap();
        assert_eq!(node.coverage.state, CoverageState::Partial);
        assert!(node.coverage.reasons.iter().any(|r| r.contains("shell")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_noop_write_produces_no_node() {
        let d = tmp("noop");
        let m = RecoveryManager::new(RecoveryConfig::default(), d.clone());
        let f = d.join("a.txt");
        std::fs::write(&f, "same").unwrap();
        m.begin_turn(&d, None, "noop").unwrap();
        let cap = m.capture_before(&f).unwrap();
        std::fs::write(&f, "same").unwrap();
        m.capture_after(cap);
        let fin = m.finish_turn(TurnStatus::Completed);
        assert!(fin.point.is_none());
        assert!(m.open_store(&d, false).unwrap().list_nodes().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    // W5: the shell window

    #[test]
    fn a_shell_command_is_captured_by_the_before_and_after_scans() {
        let d = tmp("opaque-roundtrip");
        let cfg = RecoveryConfig::default();
        let m = RecoveryManager::new(cfg.clone(), d.clone());
        std::fs::write(d.join("kept.txt"), "kept").unwrap();
        std::fs::write(d.join("edited.txt"), "one").unwrap();
        m.begin_turn(&d, None, "shell").unwrap();

        let guard = m.begin_opaque(&d, "shell").expect("the scan fits").expect("a window");
        // What a command would do, with no tool involved: rewrite one file, create another, delete a
        // third. This is exactly the case the host cannot see any other way.
        std::fs::write(d.join("edited.txt"), "two").unwrap();
        std::fs::write(d.join("created.txt"), "new").unwrap();
        std::fs::remove_file(d.join("kept.txt")).unwrap();

        let summary = m.finish_opaque(guard);
        assert!(summary.changed.contains(&"edited.txt".to_string()), "{:?}", summary.changed);
        assert!(summary.changed.contains(&"created.txt".to_string()));
        assert!(summary.changed.contains(&"kept.txt".to_string()));
        assert!(!summary.partial, "{:?}", summary.notes);

        let id = m.finish_turn(TurnStatus::Completed).point.expect("a node");
        let store = m.open_store(&d, false).unwrap();
        let node = store.load_node(&id).unwrap();
        assert_eq!(node.coverage.state, CoverageState::Complete);
        let edited = node.ops.iter().find(|o| o.path == "edited.txt").unwrap();
        assert!(edited.before.as_ref().unwrap().stored, "the old bytes must be restorable");
        assert!(edited.after.as_ref().unwrap().stored, "redo needs the new bytes too");
        let created = node.ops.iter().find(|o| o.path == "created.txt").unwrap();
        assert!(created.before.is_none());
        let deleted = node.ops.iter().find(|o| o.path == "kept.txt").unwrap();
        assert!(deleted.after.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_shell_write_can_be_rolled_back_and_redone() {
        let d = tmp("opaque-rollback");
        let cfg = RecoveryConfig::default();
        let m = RecoveryManager::new(cfg.clone(), d.clone());
        std::fs::write(d.join("a.txt"), "before").unwrap();
        m.begin_turn(&d, None, "shell").unwrap();
        let guard = m.begin_opaque(&d, "shell").unwrap().unwrap();
        std::fs::write(d.join("a.txt"), "after").unwrap();
        m.finish_opaque(guard);
        m.finish_turn(TurnStatus::Completed).point.expect("a node");

        let store = m.open_store(&d, false).unwrap();
        let target = restore::resolve(&store, &restore::TargetSpec::Count(1)).unwrap();
        let plan = restore::build_plan(&store, &cfg, &target).unwrap();
        assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
        restore::apply_rollback(&store, &cfg, plan, false, false).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "before");
        restore::apply_redo(&store, &cfg).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "after");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_scan_that_cannot_store_the_tree_refuses_a_strict_command() {
        let d = tmp("opaque-refuse");
        let mut cfg = RecoveryConfig::default();
        // A 2 MiB file against a 1 MiB per-task content budget: the before-image cannot be stored, so
        // "roll back to before this command" would be a lie.
        cfg.max_turn_mb = 1;
        cfg.max_file_mb = 8;
        let m = RecoveryManager::new(cfg.clone(), d.clone());
        std::fs::write(d.join("big.bin"), vec![7u8; 2 * 1024 * 1024]).unwrap();
        m.begin_turn(&d, None, "shell").unwrap();
        let err = m.begin_opaque(&d, "shell").expect_err("the command must be refused");
        assert!(err.model.contains("not** started"), "{}", err.model);
        assert!(err.model.contains("best_effort"), "{}", err.model);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn best_effort_runs_the_same_command_and_marks_the_point_partial() {
        let d = tmp("opaque-best-effort");
        let mut cfg = RecoveryConfig::default();
        cfg.max_turn_mb = 1;
        cfg.max_file_mb = 8;
        cfg.shell_guard = ShellGuard::BestEffort;
        let m = RecoveryManager::new(cfg.clone(), d.clone());
        std::fs::write(d.join("big.bin"), vec![7u8; 2 * 1024 * 1024]).unwrap();
        m.begin_turn(&d, None, "shell").unwrap();
        let guard = m.begin_opaque(&d, "shell").expect("best effort never refuses").expect("a window");
        std::fs::write(d.join("small.txt"), "s").unwrap();
        let summary = m.finish_opaque(guard);
        assert!(summary.changed.contains(&"small.txt".to_string()), "{:?}", summary.changed);
        // The refusal lives on the node, not in the summary: `begin_opaque` already recorded why the
        // before-image is incomplete, and the node is where the user sees coverage.
        let id = m.finish_turn(TurnStatus::Completed).point.expect("a node");
        let store = m.open_store(&d, false).unwrap();
        let node = store.load_node(&id).unwrap();
        assert_eq!(node.coverage.state, CoverageState::Partial);
        assert!(
            node.coverage.reasons.iter().any(|r| r.contains("did not finish")),
            "{:?}",
            node.coverage.reasons
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_window_dropped_before_its_second_scan_marks_the_turn_partial() {
        let d = tmp("opaque-dropped");
        let m = RecoveryManager::new(RecoveryConfig::default(), d.clone());
        let a = d.join("a.txt");
        let b = d.join("b.txt");
        std::fs::write(&a, "one").unwrap();
        std::fs::write(&b, "b0").unwrap();
        m.begin_turn(&d, None, "aborted").unwrap();
        // A host tool records one path, so the turn is not empty…
        let cap = m.capture_before(&b).unwrap();
        std::fs::write(&b, "b1").unwrap();
        m.capture_after(cap);
        {
            let _guard = m.begin_opaque(&d, "shell").unwrap().expect("a window");
            // …then a command writes a path the host never learns about, and the GUI drops the turn
            // future here, so the follow-up scan never runs.
            std::fs::write(&a, "two").unwrap();
        }
        let id = m.finish_turn(TurnStatus::Interrupted).point.expect("the b.txt op forms a node");
        let store = m.open_store(&d, false).unwrap();
        let node = store.load_node(&id).unwrap();
        assert_eq!(node.coverage.state, CoverageState::Partial);
        assert!(
            node.coverage.reasons.iter().any(|r| r.contains("follow-up scan")),
            "{:?}",
            node.coverage.reasons
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_second_command_reuses_the_baseline_instead_of_copying_the_tree_again() {
        let d = tmp("opaque-baseline");
        let cfg = RecoveryConfig::default();
        let m = RecoveryManager::new(cfg.clone(), d.clone());
        std::fs::write(d.join("a.txt"), "one").unwrap();
        std::fs::write(d.join("b.txt"), "two").unwrap();
        m.begin_turn(&d, None, "first").unwrap();
        let g = m.begin_opaque(&d, "shell").unwrap().unwrap();
        std::fs::write(d.join("a.txt"), "ONE").unwrap();
        m.finish_opaque(g);
        m.finish_turn(TurnStatus::Completed);

        let store = m.open_store(&d, false).unwrap();
        let size_after_first = store.total_bytes();
        let baseline = store.load_baseline();
        assert!(baseline.contains_key("a.txt"), "the baseline records the whole tree");
        assert!(baseline.contains_key("b.txt"));

        m.begin_turn(&d, None, "second").unwrap();
        let g = m.begin_opaque(&d, "shell").unwrap().unwrap();
        std::fs::write(d.join("b.txt"), "TWO").unwrap();
        let summary = m.finish_opaque(g);
        m.finish_turn(TurnStatus::Completed);
        assert_eq!(summary.changed, vec!["b.txt".to_string()]);
        // Only b.txt's new content is new; nothing was recopied.
        assert!(
            store.total_bytes() <= size_after_first + 4,
            "{} → {}",
            size_after_first,
            store.total_bytes()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_command_that_puts_a_file_back_leaves_no_op_behind() {
        let d = tmp("opaque-revert");
        let m = RecoveryManager::new(RecoveryConfig::default(), d.clone());
        let f = d.join("a.txt");
        std::fs::write(&f, "original").unwrap();
        m.begin_turn(&d, None, "edit then revert").unwrap();
        // A host tool edits the file…
        let cap = m.capture_before(&f).unwrap();
        std::fs::write(&f, "edited").unwrap();
        m.capture_after(cap);
        // …and the command reverts it to the original bytes.
        let g = m.begin_opaque(&d, "shell").unwrap().unwrap();
        std::fs::write(&f, "original").unwrap();
        let summary = m.finish_opaque(g);
        // The command *did* change the file — from the edit back to the original bytes.
        assert_eq!(summary.changed, vec!["a.txt".to_string()]);
        // But the task has no net effect, so the node must describe the disk (unchanged) rather than
        // the edit that no longer exists.
        let fin = m.finish_turn(TurnStatus::Completed);
        assert!(fin.point.is_none(), "a fully reverted task is not a checkpoint");
        let _ = std::fs::remove_dir_all(&d);
    }
}
