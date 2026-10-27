//! Bounded, symmetric workspace scanning for shell commands (the "strict" half of `shell_guard`).
//!
//! # Why this exists
//!
//! `write_file` / `edit_file` / `remove` tell the host which path they touch, so their capture is
//! exact and costs one read. A shell command tells the host nothing, so the only honest way to learn
//! what it changed is to look before and look after. This module is that look.
//!
//! # The three rules that matter more than speed
//!
//! 1. **Symmetry.** The before walk and the after walk use the same exclusions, the same depth and
//!    the same file-type rules. One side skipping what the other visits invents files that appeared
//!    or vanished.
//! 2. **Content, not metadata.** Strict mode hashes every regular file, because the case metadata
//!    cannot see is exactly the interesting one: a same-size rewrite (`sed -i`, a formatter, an
//!    in-place editor save that preserves mtime). Metadata is never allowed to *prove* "unchanged";
//!    it only orders the work.
//! 3. **Bounded.** Entry count, stored bytes and wall clock each have a ceiling. Reaching one stops
//!    the walk and records why. The caller then decides what that means — refuse the command
//!    (strict) or mark the recovery point partial (best effort). It is never silently ignored.
//!
//! # What "before image" means here
//!
//! A `Scan` records, per file, the content hash *and* whether the bytes are in the store. The
//! second half is what makes rollback possible: a hash proves what changed, an object restores it.
//! [`scan`] fills it by asking the store whether the object exists (a `stat`), and stores the file
//! only when it does not and the scope asks for it — so the first strict command in a workspace pays
//! a full copy, and every later one pays only for files it has never seen.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::model::BlobRef;
use super::store::RecoveryStore;

/// Estimated in-memory cost of one index entry: the map node, the path `String` and the `BlobRef`.
///
/// Deliberately generous. `metadata_memory_mb` is a ceiling meant to keep a scan from OOM-ing the
/// agent, not an accounting target, so over-estimating turns the budget into a *smaller* tree rather
/// than a surprise allocation.
pub const ENTRY_COST: u64 = 256;

/// How much content a scan is allowed to put into the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreScope {
    /// Store every file whose object is missing (strict: full pre-image, first call copies the tree).
    All,
    /// Store only files whose hash differs from the baseline (best effort: suspects only).
    OnlyChanged,
}

/// The ceilings a single scan runs under.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum entries (files + directories) visited. `0` = unlimited.
    pub max_entries: u64,
    /// Maximum bytes this scan may add to the store. `0` = unlimited.
    pub max_bytes: u64,
    /// Wall-clock ceiling for one scan. `0` = unlimited.
    pub deadline: Duration,
}

impl Limits {
    #[cfg(test)]
    pub fn unlimited() -> Self {
        Self { max_entries: 0, max_bytes: 0, deadline: Duration::ZERO }
    }
}

/// What the tree looked like at one instant.
#[derive(Clone, Debug, Default)]
pub struct Scan {
    /// Workspace-relative path → content state. Only regular files appear here.
    pub files: BTreeMap<String, BlobRef>,
    /// Relative paths of directories (no entries of their own; used to tell "empty dir" from "gone").
    pub dirs: BTreeSet<String>,
    /// Entries visited, files and directories together.
    pub scanned: u64,
    /// Bytes this scan added to the store.
    pub stored_bytes: u64,
    /// `Some(why)` when a ceiling stopped the walk early.
    pub stopped: Option<String>,
    /// Symlinks / junctions / sockets: recorded, never followed.
    pub skipped: Vec<String>,
    /// Paths whose content could not be read (locked, vanished mid-walk, over `max_file_mb`).
    pub unreadable: Vec<String>,
    pub elapsed: Duration,
}

impl Scan {
    /// Every entry the walk was supposed to see was seen.
    pub fn complete(&self) -> bool {
        self.stopped.is_none()
    }

}

/// One path whose content differs between two scans.
#[derive(Clone, Debug)]
pub struct Change {
    pub path: String,
    /// `None` = the path did not exist before.
    pub before: Option<BlobRef>,
    /// `None` = the path does not exist now.
    pub after: Option<BlobRef>,
}

impl Change {
    /// A change we can undo (the old bytes are in the store) and redo (the new bytes are too).
    pub fn reversible(&self) -> bool {
        self.before.as_ref().map(|b| b.stored).unwrap_or(true)
            && self.after.as_ref().map(|a| a.stored).unwrap_or(true)
    }
}

/// Walk `root`, hash every regular file, and store content according to `scope`.
///
/// Never returns `Err`: a path that cannot be read, a subtree that is a reparse point and a budget
/// that ran out are all *facts about the scan*, and the caller needs them to decide whether the
/// command may run at all. Turning them into an error here would throw away the partial knowledge
/// that `best_effort` mode is allowed to work with.
pub fn scan(
    store: &RecoveryStore,
    root: &Path,
    baseline: &BTreeMap<String, BlobRef>,
    scope: StoreScope,
    limits: &Limits,
    max_file: u64,
) -> Scan {
    let started = Instant::now();
    let mut out = Scan::default();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        if let Some(why) = over_budget(&out, limits, started) {
            out.stopped.get_or_insert(why);
            break;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                out.unreadable.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        // Sorted so two scans of an unchanged tree produce the same order, which keeps the "first
        // ceiling hit" deterministic and the tests meaningful.
        let mut items: Vec<PathBuf> = Vec::new();
        for e in entries {
            match e {
                Ok(e) => items.push(e.path()),
                Err(err) => out.unreadable.push(format!("{}: {err}", dir.display())),
            }
        }
        items.sort();

        for path in items {
            out.scanned += 1;
            if let Some(why) = over_budget(&out, limits, started) {
                out.stopped.get_or_insert(why);
                break;
            }
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                // Gone between `read_dir` and `symlink_metadata`. Not an error: the after walk sees
                // the same thing a real `rm -rf` race produces, and the diff reconciles it.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    out.unreadable.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            if meta.file_type().is_symlink() {
                // Never followed: a link can point outside the workspace, and following it would
                // attribute someone else's bytes to this command.
                out.skipped.push(rel_hint(root, &path));
                continue;
            }
            let rel = match store.relative(&path) {
                Ok(Some(rel)) => rel,
                // Excluded subtree (`target/`, `.git/`, …): skipped before descending, on both walks.
                Ok(None) => continue,
                Err(e) => {
                    out.skipped.push(format!("{} ({e})", path.display()));
                    continue;
                }
            };
            if meta.is_dir() {
                out.dirs.insert(rel);
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                // A device, socket or fifo. Nothing to restore, and reading one can block forever.
                out.skipped.push(rel);
                continue;
            }
            let known = baseline.get(&rel).map(|b| b.hash.clone());
            let entry = observe(store, &path, &rel, known.as_deref(), scope, limits, max_file, &mut out);
            if let Some(entry) = entry {
                out.files.insert(rel, entry);
            }
        }
        if out.stopped.is_some() {
            break;
        }
    }

    out.elapsed = started.elapsed();
    out
}

/// Read one regular file once, decide whether its bytes must be kept, and describe it.
fn observe(
    store: &RecoveryStore,
    path: &Path,
    rel: &str,
    known_hash: Option<&str>,
    scope: StoreScope,
    limits: &Limits,
    max_file: u64,
    out: &mut Scan,
) -> Option<BlobRef> {
    let mut blob = match store.hash_file_only(path) {
        Ok(b) => b,
        Err(e) => {
            out.unreadable.push(format!("{rel}: {e}"));
            return None;
        }
    };

    // "The bytes are available for a rollback" is a fact about the store, not about this scan. Asking
    // the store (a `stat`) is what lets a file that was stored by an earlier command — or by the
    // baseline — be used as a before-image without reading it again.
    blob.stored = store.has_object(&blob.hash);

    let changed = known_hash != Some(blob.hash.as_str());
    let want = match scope {
        StoreScope::All => true,
        StoreScope::OnlyChanged => changed,
    };
    if want && !blob.stored {
        if blob.size > max_file && max_file > 0 {
            out.unreadable.push(format!(
                "{rel} is {} bytes, above the {} byte per-file cap: hashed but not stored",
                blob.size, max_file
            ));
        } else if limits.max_bytes > 0 && out.stored_bytes.saturating_add(blob.size) > limits.max_bytes {
            out.stopped.get_or_insert_with(|| {
                format!(
                    "storing {rel} would take this scan past its {} MiB content budget",
                    limits.max_bytes / (1024 * 1024)
                )
            });
        } else {
            match store.put_file(path, max_file) {
                Ok(stored) => {
                    out.stored_bytes = out.stored_bytes.saturating_add(stored.size);
                    blob.stored = true;
                }
                Err(e) => out.unreadable.push(format!("{rel}: {e}")),
            }
        }
    }

    Some(blob)
}

fn over_budget(out: &Scan, limits: &Limits, started: Instant) -> Option<String> {
    if limits.max_entries > 0 && out.scanned >= limits.max_entries {
        return Some(format!(
            "the walk visited {} entries, its ceiling (raise [recovery] metadata_memory_mb, or exclude a directory)",
            limits.max_entries
        ));
    }
    if !limits.deadline.is_zero() && started.elapsed() > limits.deadline {
        return Some(format!(
            "the walk took longer than {} s (exclude a large directory, or use shell_guard = \"best_effort\")",
            limits.deadline.as_secs()
        ));
    }
    None
}

/// Best-effort relative form for a path [`RecoveryStore::relative`] refuses (a symlink, say): the
/// diagnostic is worth more than the refusal, and `strip_prefix` is purely textual.
fn rel_hint(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(p) => p.to_string_lossy().replace('\\', "/"),
        Err(_) => path.display().to_string(),
    }
}

/// Paths whose content differs, as `(before, after)` pairs.
///
/// Comparison is by content hash only. A file whose mtime moved but whose bytes did not is **not** a
/// change — otherwise every `touch`, every build-system timestamp probe and every editor save with no
/// edit would produce a rollback entry, and the real changes would be lost in the noise.
///
/// A path that was a directory and is now a file (or the reverse) shows up as two entries, a delete of
/// the subtree and a create of the file, which is exactly what restoring it requires. Nothing here
/// needs a third "type changed" case.
pub fn diff(before: &Scan, after: &Scan) -> Vec<Change> {
    let mut out = Vec::new();
    for (path, b) in &before.files {
        match after.files.get(path) {
            Some(a) if a.hash == b.hash => {}
            Some(a) => out.push(Change {
                path: path.clone(),
                before: Some(b.clone()),
                after: Some(a.clone()),
            }),
            None => out.push(Change {
                path: path.clone(),
                before: Some(b.clone()),
                after: None,
            }),
        }
    }
    for (path, a) in &after.files {
        if !before.files.contains_key(path) {
            out.push(Change {
                path: path.clone(),
                before: None,
                after: Some(a.clone()),
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RecoveryConfig;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aox-recovery-scan-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn store_for(d: &Path) -> RecoveryStore {
        let mut cfg = RecoveryConfig::default();
        cfg.max_file_mb = 4;
        RecoveryStore::open(d, &cfg, true).unwrap()
    }

    fn write(d: &Path, rel: &str, body: &str) {
        let p = d.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn a_scan_sees_files_recursively_and_skips_excluded_subtrees() {
        let d = tmp("walk");
        write(&d, "a.txt", "a");
        write(&d, "src/b.txt", "b");
        write(&d, "target/c.txt", "should be invisible");
        write(&d, "node_modules/x/y.js", "invisible");
        let store = store_for(&d);

        let s = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &Limits::unlimited(), 4 << 20);
        assert!(s.complete(), "{:?}", s.stopped);
        let paths: Vec<&str> = s.files.keys().map(String::as_str).collect();
        assert_eq!(paths, vec!["a.txt", "src/b.txt"]);
        assert!(s.dirs.contains("src"));
        assert!(!s.dirs.iter().any(|p| p.starts_with("target")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_same_size_rewrite_is_a_change_and_an_untouched_file_is_not() {
        let d = tmp("same-size");
        write(&d, "a.txt", "aaaa");
        write(&d, "b.txt", "bbbb");
        let store = store_for(&d);
        let before = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &Limits::unlimited(), 4 << 20);

        // Same length, same mtime is impossible to fake cleanly through the OS, so only content is
        // asserted: this is the case a metadata-only scanner gets wrong.
        write(&d, "a.txt", "zzzz");
        let after = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &Limits::unlimited(), 4 << 20);

        let changes = diff(&before, &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].path, "a.txt");
        assert!(changes[0].reversible());
        assert_ne!(
            changes[0].before.as_ref().unwrap().hash,
            changes[0].after.as_ref().unwrap().hash
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn deleting_and_recreating_a_file_with_the_same_content_is_not_a_change() {
        let d = tmp("delete-recreate");
        write(&d, "a.txt", "same");
        let store = store_for(&d);
        let before = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &Limits::unlimited(), 4 << 20);
        std::fs::remove_file(d.join("a.txt")).unwrap();
        write(&d, "a.txt", "same");
        let after = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &Limits::unlimited(), 4 << 20);
        assert!(diff(&before, &after).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_baseline_lets_a_second_scan_reuse_objects_instead_of_rewriting_them() {
        let d = tmp("baseline");
        write(&d, "a.txt", "one");
        let store = store_for(&d);
        let first = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &Limits::unlimited(), 4 << 20);
        let baseline: BTreeMap<String, BlobRef> =
            first.files.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

        let second = scan(&store, &d, &baseline, StoreScope::All, &Limits::unlimited(), 4 << 20);
        assert_eq!(second.stored_bytes, 0, "nothing new to store");
        assert!(second.files["a.txt"].stored, "the object is still available");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn best_effort_only_stores_a_file_that_changed_since_the_baseline() {
        let d = tmp("best-effort");
        write(&d, "kept.txt", "kept");
        write(&d, "edited.txt", "old");
        let store = store_for(&d);
        let first = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &Limits::unlimited(), 4 << 20);
        let baseline: BTreeMap<String, BlobRef> =
            first.files.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

        // Changed outside AOX since the baseline: a suspect, so its *current* bytes become the
        // before-image of whatever the command is about to do.
        write(&d, "edited.txt", "new");
        let s = scan(&store, &d, &baseline, StoreScope::OnlyChanged, &Limits::unlimited(), 4 << 20);
        assert_eq!(s.stored_bytes, 3, "only the changed file's content was new");
        assert!(s.files["kept.txt"].stored, "already in the store from the baseline");
        assert!(s.files["edited.txt"].stored);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_entry_ceiling_stops_the_walk_and_says_why() {
        let d = tmp("budget");
        for i in 0..20 {
            write(&d, &format!("f{i}.txt"), "x");
        }
        let store = store_for(&d);
        let limits = Limits { max_entries: 5, max_bytes: 0, deadline: Duration::ZERO };
        let s = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &limits, 4 << 20);
        assert!(!s.complete());
        assert!(s.files.len() < 20);
        let why = s.stopped.unwrap();
        assert!(why.contains("ceiling"), "{why}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_time_ceiling_stops_the_walk() {
        let d = tmp("deadline");
        write(&d, "a.txt", "a");
        let store = store_for(&d);
        let limits = Limits { max_entries: 0, max_bytes: 0, deadline: Duration::from_nanos(1) };
        let s = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &limits, 4 << 20);
        assert!(!s.complete());
        assert!(s.stopped.unwrap().contains("longer than"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_over_the_per_file_cap_is_hashed_but_not_stored() {
        let d = tmp("cap");
        write(&d, "big.bin", "0123456789");
        let store = store_for(&d);
        let s = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &Limits::unlimited(), 4);
        let entry = &s.files["big.bin"];
        assert!(!entry.stored, "over the cap, so only the hash is known");
        assert_eq!(entry.size, 10);
        assert!(s.unreadable.iter().any(|u| u.contains("per-file cap")), "{:?}", s.unreadable);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_directory_replaced_by_a_file_reads_as_a_delete_plus_a_create() {
        let d = tmp("type-change");
        write(&d, "thing/inner.txt", "inner");
        let store = store_for(&d);
        let before = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &Limits::unlimited(), 4 << 20);

        std::fs::remove_dir_all(d.join("thing")).unwrap();
        write(&d, "thing", "now a file");
        let after = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &Limits::unlimited(), 4 << 20);

        let changes = diff(&before, &after);
        let delete = changes.iter().find(|c| c.path == "thing/inner.txt").expect("delete");
        assert!(delete.after.is_none());
        let create = changes.iter().find(|c| c.path == "thing").expect("create");
        assert!(create.before.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_recorded_and_never_followed() {
        let d = tmp("symlink");
        write(&d, "real.txt", "real");
        std::os::unix::fs::symlink(d.join("real.txt"), d.join("link.txt")).unwrap();
        let store = store_for(&d);
        let s = scan(&store, &d, &BTreeMap::new(), StoreScope::All, &Limits::unlimited(), 4 << 20);
        assert!(s.files.contains_key("real.txt"));
        assert!(!s.files.contains_key("link.txt"));
        assert!(s.skipped.iter().any(|p| p == "link.txt"), "{:?}", s.skipped);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Perf probe, not a test: `AOX_SCAN_PROBE=<dir> cargo test --release --lib a_scan_of_a_real_tree -- --ignored --nocapture`.
    ///
    /// It exists because "how much does a strict shell command cost" is a question with a number for
    /// an answer, and the number depends on the machine. Print it instead of guessing in the docs.
    #[test]
    #[ignore]
    fn a_scan_of_a_real_tree() {
        let Some(dir) = std::env::var_os("AOX_SCAN_PROBE").map(PathBuf::from) else {
            panic!("set AOX_SCAN_PROBE to a directory");
        };
        let store = store_for(&dir);
        let first = scan(&store, &dir, &BTreeMap::new(), StoreScope::All, &Limits::unlimited(), 64 << 20);
        let baseline: BTreeMap<String, BlobRef> =
            first.files.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let second = scan(&store, &dir, &baseline, StoreScope::All, &Limits::unlimited(), 64 << 20);
        let bytes: u64 = first.files.values().map(|f| f.size).sum();
        println!(
            "{} file(s), {} dir(s), {} of content\n  first scan (cold, stores): {:?}, {} stored\n  second scan (warm): {:?}, {} stored\n  {:?}",
            first.files.len(),
            first.dirs.len(),
            bytes,
            first.elapsed,
            first.stored_bytes,
            second.elapsed,
            second.stored_bytes,
            first.stopped
        );
    }
}
