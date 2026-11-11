//! Retention and reachability: what may be deleted, and in what order.
//! Two different questions live here, and mixing them is how a recovery system eats its own data.
//! * trim — the branch is longer than `keep`. Cut the oldest *segment*. This is a structural decision about...

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::model::{BlobRef, PointKind, RecoveryPoint, SafetySnapshot};
use super::store::RecoveryStore;
use super::{RecoveryError, RecoveryResult};

/// How long an abandoned temp file is kept before it is considered garbage.
const TMP_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// At or above this share of `max_total_mb`, a finished turn also runs a full sweep.
const SWEEP_AT_PERCENT: u64 = 80;

#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Node files deleted (they were older than the `keep` window, or unreachable).
    pub nodes_dropped: usize,
    /// Content objects deleted.
    pub objects_dropped: usize,
    pub bytes_freed: u64,
    /// Unreferenced `redo/` safety snapshots deleted
    pub safety_dropped: usize,
    pub tmp_dropped: usize,
    /// This pass cut the oldest part of the branch, so `/checkpoints` will say so.
    pub truncated: bool,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.nodes_dropped == 0
            && self.objects_dropped == 0
            && self.safety_dropped == 0
            && self.tmp_dropped == 0
    }

    pub fn merge(&mut self, other: Report) {
        self.nodes_dropped += other.nodes_dropped;
        self.objects_dropped += other.objects_dropped;
        self.bytes_freed += other.bytes_freed;
        self.safety_dropped += other.safety_dropped;
        self.tmp_dropped += other.tmp_dropped;
        self.truncated |= other.truncated;
    }
}

/// Cut the oldest part of the branch so at most `keep` turn nodes stay undoable.
pub fn trim(store: &RecoveryStore, keep: usize) -> RecoveryResult<Report> {
    let mut report = Report::default();
    if keep == 0 {
        return Ok(report);
    }
    let mut cursor = store.cursor()?;
    let nodes = chain(store, cursor.head.as_deref());

    // The window is a prefix: newest first, keep everything up to and including the `keep`-th turn node.
    let mut turns = 0usize;
    let mut last_kept = None;
    for (i, n) in nodes.iter().enumerate() {
        if n.kind == PointKind::Turn {
            turns += 1;
            if turns == keep {
                last_kept = Some(i);
                break;
            }
        }
    }
    let Some(last_kept) = last_kept else { return Ok(report) };
    if last_kept + 1 >= nodes.len() {
        return Ok(report);
    }

    // ① the boundary forgets its parent, so the remaining chain is self-consistent without the
    //    nodes that are about to disappear.
    let mut boundary = nodes[last_kept].clone();
    if boundary.parent.is_some() {
        boundary.parent = None;
        store.save_node(&boundary)?;
        report.truncated = true;
    }
    // ② the cursor says so before anything is deleted.
    if !cursor.truncated {
        cursor.truncated = true;
        cursor.updated_at = crate::history::unix_now();
        store.save_cursor(&cursor)?;
    }
    // ③ only now remove the files.
    for n in &nodes[last_kept + 1..] {
        let path = store.node_path(&n.id);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                report.nodes_dropped += 1;
                report.bytes_freed += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(RecoveryError::BackupUnavailable(format!(
                    "could not remove {}: {e}",
                    path.display()
                )))
            }
        }
    }
    Ok(report)
}

/// Mark from every root, then sweep `objects/`.
pub fn collect(store: &RecoveryStore) -> RecoveryResult<Report> {
    let mut report = Report::default();
    let mut rooted: BTreeSet<String> = BTreeSet::new();

    let cursor = store.cursor()?;
    let nodes = chain(store, cursor.head.as_deref());
    let reachable: BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    for n in &nodes {
        for op in &n.ops {
            push(&mut rooted, op.before.as_ref());
            push(&mut rooted, op.after.as_ref());
        }
    }
    for b in store.load_baseline().values() {
        if b.stored {
            rooted.insert(b.hash.clone());
        }
    }
    for (id, snap) in safety_snapshots(store) {
        for p in snap.states.iter().chain(snap.expected.iter()) {
            push(&mut rooted, p.state.as_ref());
        }
        // A safety that no redo entry points at is finished: its blobs stop being roots once the
        // snapshot itself is gone, which is why the snapshot is deleted here and not later.
        if !cursor.redo.iter().any(|r| r == &id) {
            store.remove_safety(&id);
            report.safety_dropped += 1;
        }
    }
    for path in store.journal_paths()? {
        if let Ok(text) = std::fs::read_to_string(&path) {
            for hash in hex_tokens(&text) {
                rooted.insert(hash);
            }
        }
    }

    // Unreachable node files: a crash between "write the node" and "move the cursor" leaves one behind, and nothing...
    if let Ok(entries) = std::fs::read_dir(store.dir().join("nodes")) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            if reachable.contains(id) {
                continue;
            }
            if store.load_node(id).is_err() {
                continue;
            }
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(&path).is_ok() {
                report.nodes_dropped += 1;
                report.bytes_freed += size;
            }
        }
    }

    let objects = store.dir().join("objects");
    sweep_objects(&objects, &rooted, &mut report);

    report.tmp_dropped = sweep_tmp(store, TMP_GRACE);
    Ok(report)
}

/// Trim, then sweep — what `/backup prune` means, and what a turn does when the store is filling up.
pub fn clear_all(store: &RecoveryStore) -> anyhow::Result<(usize, usize)> {
    let dir = store.dir();
    let points = std::fs::read_dir(dir.join("nodes"))
        .map(|d| d.filter_map(|e| e.ok()).filter(|e| e.path().is_file()).count())
        .unwrap_or(0);
    let mut files = 0usize;
    for sub in ["nodes", "objects", "baseline"] {
        let p = dir.join(sub);
        if !p.exists() {
            continue;
        }
        files += crate::recycle::count_files(&p);
        crate::recycle::recycle_blocking(&p)?;
        std::fs::create_dir_all(&p)?;
    }
    // The head must forget itself too, or the next read follows an id whose node is in the bin.
    let mut cursor = store.cursor().map_err(|e| anyhow::anyhow!("{e}"))?;
    cursor.head = None;
    store.save_cursor(&cursor).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((points, files))
}

pub fn prune(store: &RecoveryStore, keep: usize) -> RecoveryResult<Report> {
    let mut report = trim(store, keep)?;
    let mut swept = collect(store)?;
    swept.truncated |= report.truncated;
    report.merge(Report { truncated: false, ..swept });
    Ok(report)
}

/// The cheap end-of-turn pass.
/// Trimming is structural and cheap (a walk of the chain, which is bounded by `keep`).
pub fn maintain(store: &RecoveryStore, keep: usize, max_total_mb: u64) -> Vec<String> {
    let mut out = Vec::new();
    match trim(store, keep) {
        Ok(r) if r.truncated => out.push(format!(
            "retention: {} older recovery point(s) dropped ([recovery] keep = {keep})",
            r.nodes_dropped
        )),
        Ok(_) => {}
        Err(e) => out.push(format!("retention: could not trim older recovery points: {e}")),
    }
    let cap = max_total_mb.saturating_mul(1024 * 1024);
    if cap > 0 {
        let used = store.total_bytes();
        if used.saturating_mul(100) >= cap.saturating_mul(SWEEP_AT_PERCENT) {
            match collect(store) {
                Ok(r) if !r.is_empty() => out.push(format!(
                    "recovery storage was {used} B of {cap} B; pruned {} unused object(s)",
                    r.objects_dropped
                )),
                Ok(_) => {}
                Err(e) => out.push(format!("recovery cleanup could not finish: {e}")),
            }
        }
    }
    let tmp = sweep_tmp(store, TMP_GRACE);
    if tmp > 0 {
        out.push(format!("recovery: removed {tmp} abandoned temp file(s)"));
    }
    out
}

/// The cursor's chain, newest first. Stops at the first unreadable node: everything older is
/// unreachable anyway, and a corrupt file must not turn a listing into an error.
fn chain(store: &RecoveryStore, head: Option<&str>) -> Vec<RecoveryPoint> {
    let mut out = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut cur = head.map(str::to_string);
    while let Some(id) = cur {
        // A damaged node that points at itself would otherwise hang every command.
        if !seen.insert(id.clone()) {
            break;
        }
        match store.load_node(&id) {
            Ok(n) => {
                cur = n.parent.clone();
                out.push(n);
            }
            Err(_) => break,
        }
    }
    out
}

fn push(into: &mut BTreeSet<String>, blob: Option<&BlobRef>) {
    if let Some(b) = blob {
        into.insert(b.hash.clone());
    }
}

fn safety_snapshots(store: &RecoveryStore) -> Vec<(String, SafetySnapshot)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(store.dir().join("redo")) else { return out };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        match store.load_safety(id) {
            Ok(s) => out.push((id.to_string(), s)),
            // An unreadable snapshot is not a root and cannot be used by redo either; leave the file
            // for `/backup verify` to report rather than deleting evidence here.
            Err(_) => continue,
        }
    }
    out
}

/// Every 64-character lowercase hex run in the text. Journals only ever carry hashes in that shape.
fn hex_tokens(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_hexdigit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
            if i - start == 64 {
                out.push(text[start..i].to_ascii_lowercase());
            }
        } else {
            i += 1;
        }
    }
    out
}

fn sweep_objects(dir: &Path, rooted: &BTreeSet<String>, report: &mut Report) {
    let fans = match std::fs::read_dir(dir) {
        Ok(d) => d,
        Err(_) => return,
    };
    for fan in fans.flatten() {
        let Ok(sub) = std::fs::read_dir(fan.path()) else { continue };
        for entry in sub.flatten() {
            let Ok(deeper) = std::fs::read_dir(entry.path()) else { continue };
            for obj in deeper.flatten() {
                let path = obj.path();
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
                if rooted.contains(name) {
                    continue;
                }
                let size = obj.metadata().map(|m| m.len()).unwrap_or(0);
                if std::fs::remove_file(&path).is_ok() {
                    report.objects_dropped += 1;
                    report.bytes_freed += size;
                }
            }
        }
    }
}

fn sweep_tmp(store: &RecoveryStore, grace: Duration) -> usize {
    let dir = store.dir().join("tmp");
    let Ok(entries) = std::fs::read_dir(&dir) else { return 0 };
    let now = SystemTime::now();
    let mut removed = 0;
    for e in entries.flatten() {
        let path = e.path();
        let old = match std::fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(t) => now.duration_since(t).map(|d| d > grace).unwrap_or(false),
            Err(_) => false,
        };
        if old && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RecoveryConfig;
    use crate::recovery::model::{FileOp, FileOpKind, PointStatus, TurnStatus, SCHEMA};
    use crate::recovery::RecoveryManager;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aox-recovery-gc-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn store_for(d: &Path) -> RecoveryStore {
        RecoveryStore::open(d, &RecoveryConfig::default(), true).unwrap()
    }

    /// Commit `n` turns, each changing one distinct file, and return the store.
    fn history(d: &Path, n: usize) -> RecoveryManager {
        let m = RecoveryManager::new(RecoveryConfig::default(), d.to_path_buf());
        for i in 0..n {
            let f = d.join(format!("f{i}.txt"));
            m.begin_turn(d, None, &format!("turn {i}")).unwrap();
            let cap = m.capture_before(&f).unwrap();
            std::fs::write(&f, format!("content {i}")).unwrap();
            m.capture_after(cap);
            m.finish_turn(TurnStatus::Completed).point.expect("a node");
        }
        m
    }

    #[test]
    fn keep_cuts_the_oldest_segment_and_the_boundary_forgets_its_parent() {
        let d = tmp("trim");
        history(&d, 5);
        let store = store_for(&d);
        let before = store.list_nodes().unwrap();

        let report = trim(&store, 2).unwrap();
        assert!(report.truncated);
        assert_eq!(report.nodes_dropped, 3, "5 commits - keep 2");
        let after = store.list_nodes().unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(before.len(), 5);

        // The two survivors are the newest two, and the older one is now the root.
        let cursor = store.cursor().unwrap();
        assert!(cursor.truncated);
        let head = store.load_node(cursor.head.as_ref().unwrap()).unwrap();
        let root = store.load_node(&head.parent.clone().unwrap()).unwrap();
        assert!(root.parent.is_none(), "the boundary must be a root now");
        assert_eq!(root.label, "turn 3");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn keep_is_a_no_op_while_the_branch_fits() {
        let d = tmp("trim-fits");
        history(&d, 3);
        let store = store_for(&d);
        let report = trim(&store, 30).unwrap();
        assert!(report.is_empty(), "{report:?}");
        assert_eq!(store.list_nodes().unwrap().len(), 3);
        assert!(!store.cursor().unwrap().truncated);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn keep_zero_means_no_limit_rather_than_delete_everything() {
        let d = tmp("trim-zero");
        history(&d, 3);
        let store = store_for(&d);
        let report = trim(&store, 0).unwrap();
        assert!(report.is_empty());
        assert_eq!(store.list_nodes().unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_object_survives_while_any_node_still_points_at_it() {
        let d = tmp("shared");
        let m = RecoveryManager::new(RecoveryConfig::default(), d.clone());
        let a = d.join("a.txt");
        std::fs::write(&a, "v0").unwrap();
        // turn 0: v0 -> v1, turn 1: v1 -> v2, turn 2: v2 -> v3
        for i in 0..3 {
            m.begin_turn(&d, None, &format!("t{i}")).unwrap();
            let cap = m.capture_before(&a).unwrap();
            std::fs::write(&a, format!("v{}", i + 1)).unwrap();
            m.capture_after(cap);
            m.finish_turn(TurnStatus::Completed).point.expect("a node");
        }
        let store = store_for(&d);
        let h = |s: &str| blake3::hash(s.as_bytes()).to_hex().to_string();
        for v in ["v0", "v1", "v2", "v3"] {
            assert!(store.has_object(&h(v)), "{v} should be stored");
        }

        // Keep only the newest turn. Its before-image is v2 — produced by the node that is about to
        // be dropped, and still needed. That is the whole point of marking instead of counting refs.
        trim(&store, 1).unwrap();
        let report = collect(&store).unwrap();
        assert_eq!(report.objects_dropped, 2, "v0 and v1 are unreachable now");
        assert!(!store.has_object(&h("v1")));
        assert!(store.has_object(&h("v2")), "a live before-image always wins");
        assert!(store.has_object(&h("v3")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_baseline_is_a_root_so_shell_before_images_are_never_swept() {
        let d = tmp("baseline-root");
        std::fs::write(d.join("keep.txt"), "kept by the baseline").unwrap();
        let m = RecoveryManager::new(RecoveryConfig::default(), d.clone());
        m.begin_turn(&d, None, "shell").unwrap();
        let g = m.begin_opaque(&d, "shell").unwrap().unwrap();
        std::fs::write(d.join("other.txt"), "x").unwrap();
        m.finish_opaque(g);
        m.finish_turn(TurnStatus::Completed);
        let store = store_for(&d);
        let kept = blake3::hash(b"kept by the baseline").to_hex().to_string();
        assert!(store.has_object(&kept));

        let report = collect(&store).unwrap();
        assert_eq!(report.objects_dropped, 0, "nothing is unreachable yet");
        assert!(store.has_object(&kept));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_node_the_cursor_never_reached_is_swept() {
        let d = tmp("orphan");
        history(&d, 2);
        let store = store_for(&d);
        // Simulate a crash after the node write and before the cursor move.
        let orphan = RecoveryPoint {
            schema: SCHEMA,
            id: "rp-orphan".into(),
            parent: None,
            kind: PointKind::Turn,
            status: PointStatus::Completed,
            created_at: 1,
            finished_at: 1,
            conversation: None,
            turn: None,
            label: "lost".into(),
            ops: vec![FileOp {
                path: "ghost.txt".into(),
                kind: FileOpKind::Create,
                before: None,
                after: None,
            }],
            coverage: crate::recovery::model::Coverage::complete(),
            redoable: false,
        };
        store.save_node(&orphan).unwrap();
        assert!(store.node_path("rp-orphan").exists());

        let report = collect(&store).unwrap();
        assert_eq!(report.nodes_dropped, 1);
        assert!(!store.node_path("rp-orphan").exists());
        // The two committed nodes are untouched.
        assert_eq!(store.list_nodes().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_abandoned_journal_keeps_its_objects_alive() {
        let d = tmp("journal-root");
        let store = store_for(&d);
        let blob = store.put_reader(&mut "journal-only".as_bytes(), None, 1024, false).unwrap();
        let mut j = store.new_journal().unwrap();
        j.append(
            &serde_json::json!({"t":"intent","path":"a.txt","before":blob.hash}),
            true,
        )
        .unwrap();
        drop(j); // the process "died" here: the journal file stays

        let report = collect(&store).unwrap();
        assert_eq!(report.objects_dropped, 0);
        assert!(store.has_object(&blob.hash), "a journal is a root until it is resolved");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_finished_safety_snapshot_is_dropped_but_a_live_redo_entry_is_kept() {
        let d = tmp("safety");
        history(&d, 2);
        let store = store_for(&d);
        let mut cursor = store.cursor().unwrap();
        // A safety the cursor does not reference (a redo branch that was abandoned).
        let dead = crate::recovery::model::SafetySnapshot {
            schema: SCHEMA,
            id: "s-dead".into(),
            from_cursor: None,
            target_cursor: None,
            created_at: 1,
            states: Vec::new(),
            expected: Vec::new(),
        };
        store.save_safety(&dead).unwrap();
        assert!(store.dir().join("redo/s-dead.json").exists());
        cursor.redo = vec!["s-live".into()];
        store.save_cursor(&cursor).unwrap();
        let live = crate::recovery::model::SafetySnapshot { id: "s-live".into(), ..dead };
        store.save_safety(&live).unwrap();

        let report = collect(&store).unwrap();
        assert_eq!(report.safety_dropped, 1);
        assert!(!store.dir().join("redo/s-dead.json").exists());
        assert!(store.dir().join("redo/s-live.json").exists(), "a live redo branch must survive");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn hex_tokens_finds_hashes_and_ignores_everything_else() {
        let h = "a".repeat(64);
        let text = format!("{{\"before\":\"{h}\",\"n\":\"{}\"}}", "b".repeat(63));
        assert_eq!(hex_tokens(&text), vec![h]);
    }

    #[test]
    fn maintain_reports_a_trim_but_never_fails_the_turn() {
        let d = tmp("maintain");
        history(&d, 3);
        let store = store_for(&d);
        let warnings = maintain(&store, 1, 512);
        assert!(warnings.iter().any(|w| w.contains("keep = 1")), "{warnings:?}");
        assert_eq!(store.list_nodes().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
