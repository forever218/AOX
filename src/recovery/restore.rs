//! Restore planning, preflight, safety snapshots, and redo.
//! Nothing is written until every affected path has been checked.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::json;

use super::model::{
    BlobRef, CoverageState, FileOp, PointKind, RecoveryPoint, RestorePath, RestorePlan, RestoreReport,
    SafetyPath, SafetySnapshot, SCHEMA,
};
use super::store::{Journal, RecoveryStore};
use super::{RecoveryError, RecoveryResult};
use crate::config::RecoveryConfig;

#[derive(Clone, Debug)]
pub enum TargetSpec {
    /// Undo this many task checkpoints from the cursor (the CLI's `/rollback n`).
    Count(usize),
    /// Restore **to** the state recorded at this checkpoint (the CLI's `/rollback <id>`).
    Id(String),
    /// Undo this checkpoint and everything after it (the GUI's "roll back to before this point").
    Before(String),
}

pub struct Target {
    pub id: Option<String>,
}

pub fn resolve(store: &RecoveryStore, spec: &TargetSpec) -> RecoveryResult<Target> {
    let cursor = store.cursor()?;
    match spec {
        TargetSpec::Count(n) => {
            if *n == 0 {
                return Err(RecoveryError::BackupUnavailable(
                    "rollback count must be at least 1".into(),
                ));
            }
            let mut cur = cursor.head.clone();
            let mut seen = 0usize;
            while let Some(id) = cur {
                let node = store.load_node(&id)?;
                if node.kind == PointKind::Turn {
                    seen += 1;
                    if seen == *n {
                        return Ok(Target { id: node.parent.clone() });
                    }
                }
                cur = node.parent;
            }
            Err(RecoveryError::BackupUnavailable(format!(
                "only {seen} task checkpoint(s) are available from the current cursor"
            )))
        }
        TargetSpec::Id(id) => {
            let node = store.load_node(id)?;
            Ok(Target { id: Some(node.id) })
        }
        TargetSpec::Before(id) => {
            let node = store.load_node(id)?;
            Ok(Target { id: node.parent })
        }
    }
}

pub fn build_plan(
    store: &RecoveryStore,
    _cfg: &RecoveryConfig,
    target: &Target,
) -> RecoveryResult<RestorePlan> {
    let cursor = store.cursor()?;
    let mut plan = RestorePlan {
        from: cursor.head.clone(),
        target: target.id.clone(),
        truncated: cursor.truncated,
        ..Default::default()
    };
    let mut cur = cursor.head.clone();
    let mut nodes: Vec<RecoveryPoint> = Vec::new();
    while let Some(ref id) = cur {
        if Some(id) == target.id.as_ref() {
            break;
        }
        let node = store.load_node(id)?;
        if node.coverage.state != CoverageState::Complete {
            plan.partial = true;
            plan.warnings.push(format!(
                "{} has incomplete coverage: {}",
                node.id,
                node.coverage.reasons.join("; ")
            ));
        }
        cur = node.parent.clone();
        nodes.push(node);
    }
    if cur != target.id {
        return Err(RecoveryError::BackupUnavailable(
            "the requested checkpoint is not an ancestor of the current cursor".into(),
        ));
    }
    if nodes.is_empty() {
        plan.warnings.push("already at that checkpoint".into());
        return Ok(plan);
    }

    // Newest first gives the state expected on disk; the oldest before-image is the final state.
    let mut current: BTreeMap<String, Option<BlobRef>> = BTreeMap::new();
    let mut restore: BTreeMap<String, Option<BlobRef>> = BTreeMap::new();
    for node in &nodes {
        for op in &node.ops {
            current.entry(op.path.clone()).or_insert_with(|| op.after.clone());
            restore.insert(op.path.clone(), op.before.clone());
        }
    }

    for (rel, want_restore) in restore {
        if let Some(b) = &want_restore {
            if !b.stored {
                plan.conflicts.push(format!("{rel}: the version to restore was never stored"));
            } else if let Err(e) = store.verify_blob(b) {
                plan.conflicts.push(format!("{rel}: {e}"));
            }
        }
        let abs = store.absolute(&rel)?;
        let actual = match std::fs::symlink_metadata(&abs) {
            Ok(meta) if meta.file_type().is_symlink() => {
                plan.conflicts.push(format!("{rel}: is now a symlink/reparse point"));
                None
            }
            Ok(meta) if meta.is_file() => Some(store.hash_file_only(&abs)?),
            Ok(_) => {
                plan.conflicts.push(format!("{rel}: is now a directory or special file"));
                None
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                plan.conflicts.push(format!("{rel}: {e}"));
                None
            }
        };
        let expected = current.remove(&rel).flatten();
        match (&expected, &actual) {
            (None, None) => {}
            (Some(e), Some(a)) if e.hash == a.hash => {
                if e.readonly != a.readonly {
                    plan.warnings
                        .push(format!("{rel}: permissions changed outside AOX; restore will use the saved mode"));
                }
            }
            (None, Some(_)) => {
                plan.conflicts.push(format!("{rel}: a file exists that the checkpoint expected to be absent"));
            }
            (Some(_), None) => {
                plan.conflicts.push(format!("{rel}: the checkpoint expected a file, but it is missing"));
            }
            (Some(e), Some(a)) => {
                plan.conflicts.push(format!(
                    "{rel}: changed outside AOX (expected {}, found {})",
                    short(&e.hash),
                    short(&a.hash)
                ));
            }
        }
        plan.actions.push(RestorePath {
            path: rel,
            current: actual,
            restore: want_restore,
        });
    }
    plan.actions.retain(|a| match (&a.current, &a.restore) {
        (None, None) => false,
        (Some(x), Some(y)) => x.hash != y.hash || x.readonly != y.readonly,
        _ => true,
    });
    Ok(plan)
}

pub fn apply_rollback(
    store: &RecoveryStore,
    cfg: &RecoveryConfig,
    mut plan: RestorePlan,
    force: bool,
    allow_partial: bool,
) -> RecoveryResult<RestoreReport> {
    if plan.partial && !allow_partial {
        return Err(RecoveryError::BackupUnavailable(
            "this rollback crosses a checkpoint with incomplete coverage; pass --allow-partial to continue".into(),
        ));
    }
    if !plan.conflicts.is_empty() && !force {
        return Err(RecoveryError::BackupUnavailable(format!(
            "{} conflict(s); nothing was written. Use --force only after saving the external changes.",
            plan.conflicts.len()
        )));
    }
    let _lock = store.lock()?;
    let cursor = store.cursor()?;
    if cursor.head != plan.from {
        return Err(RecoveryError::BackupUnavailable(
            "the cursor moved since the preview; run the preview again".into(),
        ));
    }
    // Re-check the expected current state immediately before saving safety. This shrinks (but does
    // not eliminate) the TOCTOU window against an external editor.
    for a in &plan.actions {
        let abs = store.absolute(&a.path)?;
        let actual = match std::fs::symlink_metadata(&abs) {
            Ok(m) if m.is_file() => Some(store.hash_file_only(&abs)?),
            Ok(_) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(RecoveryError::BackupUnavailable(format!("{}: {e}", a.path))),
        };
        let ok = match (&a.current, &actual) {
            (None, None) => true,
            (Some(x), Some(y)) => x.hash == y.hash,
            _ => false,
        };
        if !ok {
            return Err(RecoveryError::BackupUnavailable(format!(
                "{} changed while preparing the restore; nothing was written",
                a.path
            )));
        }
    }

    let safety_id = super::store::new_id("redo");
    let mut states = Vec::new();
    for a in &plan.actions {
        let abs = store.absolute(&a.path)?;
        let state = match std::fs::symlink_metadata(&abs) {
            Ok(m) if m.is_file() => Some(store.put_file(&abs, mb(cfg.max_file_mb))?),
            Ok(_) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(RecoveryError::BackupUnavailable(format!("{}: {e}", a.path))),
        };
        states.push(SafetyPath { path: a.path.clone(), state });
    }
    let safety = SafetySnapshot {
        schema: SCHEMA,
        id: safety_id.clone(),
        from_cursor: plan.from.clone(),
        target_cursor: plan.target.clone(),
        created_at: crate::history::unix_now(),
        states,
        expected: plan
            .actions
            .iter()
            .map(|a| SafetyPath { path: a.path.clone(), state: a.restore.clone() })
            .collect(),
    };
    store.save_safety(&safety)?;
    plan.safety_id = Some(safety_id.clone());

    let quarantine = store.dir().join("quarantine").join(&safety_id);
    let mut journal = store.new_restore_journal()?;
    journal
        .append(
            &json!({
                "t":"restore_begin",
                "mode":"rollback",
                "safety":safety_id,
                "from":plan.from,
                "target":plan.target,
                "at":crate::history::unix_now()
            }),
            true,
        )
        .map_err(|e| RecoveryError::BackupUnavailable(e.to_string()))?;
    let report = write_actions(store, &mut plan.actions, &mut journal, &quarantine);
    match report {
        Ok((restored, removed, mut warnings)) => {
            journal.append(
                &json!({"t":"applied_all","at":crate::history::unix_now()}),
                true,
            )?;
            let mut cursor = store.cursor()?;
            cursor.head = plan.target.clone();
            cursor.redo.push(safety_id.clone());
            while cursor.redo.len() > 8 {
                if let Some(old) = cursor.redo.first().cloned() {
                    store.remove_safety(&old);
                }
                cursor.redo.remove(0);
            }
            cursor.updated_at = crate::history::unix_now();
            store.save_cursor(&cursor)?;
            let _ = journal.append(
                &json!({"t":"restore_committed","target":plan.target,"at":crate::history::unix_now()}),
                true,
            );
            journal.remove();
            let _ = std::fs::remove_dir_all(&quarantine);
            warnings.extend(plan.warnings);
            Ok(RestoreReport {
                from: plan.from,
                target: plan.target,
                restored,
                removed,
                safety_id: Some(safety_id),
                warnings,
            })
        }
        Err(e) => {
            // Best effort: put every applied path back to the pre-restore state. Safety is already
            // durable, so the same operation can be retried after fixing a lock/permission issue.
            let mut warnings = vec![format!("restore failed: {e}")];
            let applied = read_applied(journal.path());
            for path in applied.iter().rev() {
                if let Some(st) = safety.states.iter().find(|s| &s.path == path) {
                    if let Err(e) = put_state(store, &st.path, &st.state) {
                        warnings.push(format!("{}: could not revert after failure: {e}", st.path));
                    }
                }
            }
            warnings.push(format!("safety snapshot retained as {}", safety_id));
            Err(RecoveryError::BackupUnavailable(warnings.join("; ")))
        }
    }
}

fn write_actions(
    store: &RecoveryStore,
    actions: &mut [RestorePath],
    journal: &mut Journal,
    quarantine: &Path,
) -> RecoveryResult<(usize, usize, Vec<String>)> {
    let mut restored = 0usize;
    let mut removed = 0usize;
    for a in actions {
        let abs = store.absolute(&a.path)?;
        match &a.restore {
            Some(blob) => {
                store.materialize(blob, &abs)?;
                restored += 1;
            }
            None => {
                if abs.exists() {
                    let q = quarantine.join(&a.path);
                    if let Some(parent) = q.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            RecoveryError::BackupUnavailable(format!("{}: {e}", parent.display()))
                        })?;
                    }
                    std::fs::rename(&abs, &q).map_err(|e| {
                        RecoveryError::BackupUnavailable(format!(
                            "cannot move {} aside: {e}",
                            abs.display()
                        ))
                    })?;
                    removed += 1;
                }
            }
        }
        journal.append(
            &json!({"t":"applied","path":a.path,"at":crate::history::unix_now()}),
            true,
        )?;
    }
    Ok((restored, removed, Vec::new()))
}

pub fn apply_redo(store: &RecoveryStore, cfg: &RecoveryConfig) -> RecoveryResult<RestoreReport> {
    let _lock = store.lock()?;
    let mut cursor = store.cursor()?;
    let Some(safety_id) = cursor.redo.last().cloned() else {
        return Err(RecoveryError::BackupUnavailable("nothing to redo".into()));
    };
    let safety = store.load_safety(&safety_id)?;
    if safety.target_cursor != cursor.head {
        return Err(RecoveryError::BackupUnavailable(
            "the cursor moved since that rollback; redo is no longer applicable".into(),
        ));
    }
    let mut actions: Vec<RestorePath> = safety
        .states
        .iter()
        .map(|s| RestorePath {
            path: s.path.clone(),
            current: safety
                .expected
                .iter()
                .find(|e| e.path == s.path)
                .and_then(|e| e.state.clone()),
            restore: s.state.clone(),
        })
        .collect();
    for a in &actions {
        let abs = store.absolute(&a.path)?;
        let actual = match std::fs::symlink_metadata(&abs) {
            Ok(m) if m.is_file() => Some(store.hash_file_only(&abs)?),
            Ok(_) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(RecoveryError::BackupUnavailable(format!("{}: {e}", a.path))),
        };
        let ok = match (&a.current, &actual) {
            (None, None) => true,
            (Some(x), Some(y)) => x.hash == y.hash,
            _ => false,
        };
        if !ok {
            return Err(RecoveryError::BackupUnavailable(format!(
                "{} changed outside AOX; redo refused",
                a.path
            )));
        }
        if let Some(b) = &a.restore {
            if !b.stored {
                return Err(RecoveryError::BackupUnavailable(format!(
                    "{}: the redo version was never stored",
                    a.path
                )));
            }
        }
    }
    let quarantine = store.dir().join("quarantine").join(format!("redo-{safety_id}"));
    let mut journal = store.new_restore_journal()?;
    journal.append(
        &json!({
            "t":"restore_begin",
            "mode":"redo",
            "safety":safety_id,
            "from":safety.target_cursor,
            "target":safety.from_cursor,
            "at":crate::history::unix_now()
        }),
        true,
    )?;
    let (restored, removed, mut warnings) =
        write_actions(store, &mut actions, &mut journal, &quarantine)?;
    journal.append(&json!({"t":"applied_all","at":crate::history::unix_now()}), true)?;
    cursor.head = safety.from_cursor.clone();
    cursor.redo.pop();
    cursor.updated_at = crate::history::unix_now();
    store.save_cursor(&cursor)?;
    let _ = journal.append(
        &json!({"t":"redo_committed","target":cursor.head,"at":crate::history::unix_now()}),
        true,
    );
    journal.remove();
    let _ = std::fs::remove_dir_all(&quarantine);
    store.remove_safety(&safety_id);
    warnings.push(format!("restored from redo {}", short(&safety_id)));
    let _ = cfg;
    Ok(RestoreReport {
        from: safety.target_cursor,
        target: safety.from_cursor,
        restored,
        removed,
        safety_id: Some(safety_id),
        warnings,
    })
}

fn put_state(store: &RecoveryStore, rel: &str, state: &Option<BlobRef>) -> RecoveryResult<()> {
    let abs = store.absolute(rel)?;
    match state {
        Some(b) => store.materialize(b, &abs),
        None => {
            if abs.exists() {
                let q = store.dir().join("quarantine").join("revert").join(rel);
                if let Some(parent) = q.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        RecoveryError::BackupUnavailable(format!("{}: {e}", parent.display()))
                    })?;
                }
                std::fs::rename(&abs, &q)
                    .map_err(|e| RecoveryError::BackupUnavailable(format!("{}: {e}", abs.display())))?;
            }
            Ok(())
        }
    }
}

fn read_applied(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("t").and_then(|x| x.as_str()) == Some("applied"))
        .filter_map(|v| v.get("path").and_then(|x| x.as_str()).map(str::to_string))
        .collect()
}

/// Resolve journals left by a crash.
pub fn recover_pending(store: &RecoveryStore, cfg: &RecoveryConfig) -> RecoveryResult<()> {
    for path in store.journal_paths()? {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let events = read_events(&path)?;
        let result = if name.starts_with("restore_") {
            recover_restore_journal(store, &path, &events)
        } else if name.starts_with("tx_") {
            recover_capture_journal(store, cfg, &path, &events)
        } else {
            Ok(())
        };
        result?;
    }
    Ok(())
}

fn recover_capture_journal(
    store: &RecoveryStore,
    cfg: &RecoveryConfig,
    path: &Path,
    events: &[serde_json::Value],
) -> RecoveryResult<()> {
    let mut parent: Option<String> = None;
    let mut conversation: Option<String> = None;
    let mut intents: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut finish: Option<String> = None;
    let mut partial: Vec<String> = Vec::new();
    for v in events {
        match v.get("t").and_then(|x| x.as_str()).unwrap_or("") {
            "begin" => {
                parent = v.get("parent").and_then(|x| x.as_str()).map(str::to_string);
                conversation = v.get("conversation").and_then(|x| x.as_str()).map(str::to_string);
            }
            "intent" => {
                if let Some(p) = v.get("path").and_then(|x| x.as_str()) {
                    intents.insert(
                        p.to_string(),
                        v.get("before").and_then(|x| x.as_str()).map(str::to_string),
                    );
                }
            }
            "partial" => {
                if let Some(r) = v.get("reason").and_then(|x| x.as_str()) {
                    partial.push(r.to_string());
                }
            }
            "finish" => finish = v.get("node").and_then(|x| x.as_str()).map(str::to_string),
            _ => {}
        }
    }
    if let Some(id) = finish {
        if let Ok(node) = store.load_node(&id) {
            let mut cursor = store.cursor()?;
            if cursor.head == node.parent {
                cursor.head = Some(node.id);
                cursor.redo.clear();
                cursor.updated_at = crate::history::unix_now();
                store.save_cursor(&cursor)?;
            }
        }
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    if intents.is_empty() {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    let mut cursor = store.cursor()?;
    if cursor.head != parent {
        // Crash after the cursor was advanced but before the journal was removed: the committed node is the new head...
        if let Some(head) = cursor.head.as_deref() {
            if store
                .load_node(head)
                .map(|n| n.parent == parent)
                .unwrap_or(false)
            {
                let _ = std::fs::remove_file(path);
                return Ok(());
            }
        }
        // Another branch moved the cursor. Leave the journal alone: inventing a node on a
        // different branch would make `/rollback` lie about ancestry.
        return Ok(());
    }
    let mut ops = Vec::new();
    for (rel, before_hash) in intents {
        let before = match before_hash {
            Some(h) => Some(store.blob_by_hash(&h)?),
            None => None,
        };
        let abs = store.absolute(&rel)?;
        let actual = match std::fs::symlink_metadata(&abs) {
            Ok(m) if m.is_file() => match store.observe_file(&abs, mb(cfg.max_file_mb)) {
                Ok(b) => Some(b),
                Err(e) => {
                    partial.push(format!("{rel}: {e}"));
                    continue;
                }
            },
            Ok(_) => {
                partial.push(format!("{rel}: no longer a regular file"));
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                partial.push(format!("{rel}: {e}"));
                continue;
            }
        };
        if crate::recovery::same_state(&before, &actual) {
            continue;
        }
        let mut op = FileOp {
            path: rel,
            kind: super::model::FileOpKind::Modify,
            before,
            after: actual,
        };
        op.finish_kind();
        ops.push(op);
    }
    if ops.is_empty() {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    let node = RecoveryPoint {
        schema: SCHEMA,
        id: super::store::new_id("rp"),
        parent: parent.clone(),
        kind: PointKind::Turn,
        status: super::model::PointStatus::Recovered,
        created_at: crate::history::unix_now(),
        finished_at: crate::history::unix_now(),
        conversation,
        turn: None,
        label: "recovered interrupted task".into(),
        redoable: ops.iter().all(|o| o.after.as_ref().map(|b| b.stored).unwrap_or(true)),
        ops,
        coverage: super::model::Coverage {
            state: CoverageState::Partial,
            scanned: 0,
            excluded_roots: cfg.exclude.clone(),
            reasons: {
                let mut r = vec!["recovered from an interrupted journal".to_string()];
                r.extend(partial);
                r
            },
        },
    };
    store.save_node(&node)?;
    cursor.head = Some(node.id);
    cursor.redo.clear();
    cursor.updated_at = crate::history::unix_now();
    store.save_cursor(&cursor)?;
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn recover_restore_journal(
    store: &RecoveryStore,
    path: &Path,
    events: &[serde_json::Value],
) -> RecoveryResult<()> {
    let mut mode = "";
    let mut safety_id: Option<String> = None;
    let mut from: Option<String> = None;
    let mut target: Option<String> = None;
    let mut applied: Vec<String> = Vec::new();
    let mut applied_all = false;
    let mut committed = false;
    for v in events {
        match v.get("t").and_then(|x| x.as_str()).unwrap_or("") {
            "restore_begin" => {
                mode = v.get("mode").and_then(|x| x.as_str()).unwrap_or("");
                safety_id = v.get("safety").and_then(|x| x.as_str()).map(str::to_string);
                from = v.get("from").and_then(|x| x.as_str()).map(str::to_string);
                target = v.get("target").and_then(|x| x.as_str()).map(str::to_string);
            }
            "applied" => {
                if let Some(p) = v.get("path").and_then(|x| x.as_str()) {
                    applied.push(p.to_string());
                }
            }
            "applied_all" => applied_all = true,
            "restore_committed" | "redo_committed" => committed = true,
            _ => {}
        }
    }
    if committed {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    if applied.is_empty() {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    let Some(sid) = safety_id else {
        return Err(RecoveryError::Corrupt {
            path: path.to_path_buf(),
            reason: "restore journal has no safety id".into(),
        });
    };
    if applied_all {
        let mut cursor = store.cursor()?;
        match mode {
            "rollback" => {
                cursor.head = target.clone();
                if !cursor.redo.contains(&sid) {
                    cursor.redo.push(sid.clone());
                }
            }
            "redo" => {
                cursor.head = from.clone();
                cursor.redo.retain(|x| x != &sid);
                store.remove_safety(&sid);
            }
            _ => {
                return Err(RecoveryError::Corrupt {
                    path: path.to_path_buf(),
                    reason: format!("unknown restore mode {mode:?}"),
                })
            }
        }
        cursor.updated_at = crate::history::unix_now();
        store.save_cursor(&cursor)?;
        let _ = std::fs::remove_file(path);
        return Ok(());
    }

    let safety = store.load_safety(&sid)?;
    let states = if mode == "redo" { &safety.expected } else { &safety.states };
    for rel in applied.iter().rev() {
        if let Some(st) = states.iter().find(|s| &s.path == rel) {
            put_state(store, &st.path, &st.state)?;
        }
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn read_events(path: &Path) -> RecoveryResult<Vec<serde_json::Value>> {
    let text = std::fs::read_to_string(path).map_err(|e| RecoveryError::Corrupt {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => out.push(v),
            // A crash may truncate the final line.
            Err(_) => break,
        }
    }
    Ok(out)
}

fn mb(n: u64) -> u64 {
    n.saturating_mul(1024 * 1024)
}

fn short(s: &str) -> String {
    s.chars().take(10).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::model::TurnStatus;
    use crate::recovery::RecoveryManager;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aox-recovery-restore-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rollback_one_and_redo() {
        let d = tmp("roundtrip");
        let cfg = RecoveryConfig::default();
        let m = RecoveryManager::new(cfg.clone(), d.clone());
        let f = d.join("a.txt");
        std::fs::write(&f, "one").unwrap();
        m.begin_turn(&d, None, "first").unwrap();
        let c = m.capture_before(&f).unwrap();
        std::fs::write(&f, "two").unwrap();
        m.capture_after(c);
        assert!(m.finish_turn(TurnStatus::Completed).point.is_some());

        let store = m.open_store(&d, false).unwrap();
        let t = resolve(&store, &TargetSpec::Count(1)).unwrap();
        let plan = build_plan(&store, &cfg, &t).unwrap();
        assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
        let rep = apply_rollback(&store, &cfg, plan, false, false).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one");
        assert_eq!(rep.restored, 1);
        apply_redo(&store, &cfg).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "two");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn drift_refuses_without_writing() {
        let d = tmp("drift");
        let cfg = RecoveryConfig::default();
        let m = RecoveryManager::new(cfg.clone(), d.clone());
        let f = d.join("a.txt");
        std::fs::write(&f, "one").unwrap();
        m.begin_turn(&d, None, "first").unwrap();
        let c = m.capture_before(&f).unwrap();
        std::fs::write(&f, "two").unwrap();
        m.capture_after(c);
        m.finish_turn(TurnStatus::Completed);
        std::fs::write(&f, "outside").unwrap();
        let store = m.open_store(&d, false).unwrap();
        let t = resolve(&store, &TargetSpec::Count(1)).unwrap();
        let plan = build_plan(&store, &cfg, &t).unwrap();
        assert!(!plan.conflicts.is_empty());
        assert!(apply_rollback(&store, &cfg, plan, false, false).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "outside");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn delete_is_restored_to_the_same_bytes() {
        let d = tmp("delete");
        let cfg = RecoveryConfig::default();
        let m = RecoveryManager::new(cfg.clone(), d.clone());
        let f = d.join("gone.txt");
        std::fs::write(&f, "keep me").unwrap();
        m.begin_turn(&d, None, "delete").unwrap();
        let c = m.capture_tree_before(&f).unwrap();
        std::fs::remove_file(&f).unwrap();
        m.finish_tree_after(c);
        m.finish_turn(TurnStatus::Completed);
        let store = m.open_store(&d, false).unwrap();
        let t = resolve(&store, &TargetSpec::Count(1)).unwrap();
        let plan = build_plan(&store, &cfg, &t).unwrap();
        apply_rollback(&store, &cfg, plan, false, false).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "keep me");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_external_change_gets_its_own_bridge_node() {
        let d = tmp("external");
        let cfg = RecoveryConfig::default();
        let m = RecoveryManager::new(cfg.clone(), d.clone());
        let f = d.join("a.txt");
        std::fs::write(&f, "one").unwrap();
        m.begin_turn(&d, None, "first").unwrap();
        let c = m.capture_before(&f).unwrap();
        std::fs::write(&f, "two").unwrap();
        m.capture_after(c);
        let first = m.finish_turn(TurnStatus::Completed).point.unwrap();

        std::fs::write(&f, "external").unwrap();
        m.begin_turn(&d, None, "second").unwrap();
        let c = m.capture_before(&f).unwrap();
        std::fs::write(&f, "four").unwrap();
        m.capture_after(c);
        let second = m.finish_turn(TurnStatus::Completed).point.unwrap();
        let store = m.open_store(&d, false).unwrap();
        let task = store.load_node(&second).unwrap();
        let bridge = store.load_node(task.parent.as_deref().unwrap()).unwrap();
        assert_eq!(bridge.kind, PointKind::External);
        assert_eq!(bridge.parent.as_deref(), Some(first.as_str()));
        let target = resolve(&store, &TargetSpec::Count(1)).unwrap();
        assert_eq!(target.id.as_deref(), Some(bridge.id.as_str()));
        let plan = build_plan(&store, &cfg, &target).unwrap();
        apply_rollback(&store, &cfg, plan, false, false).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "external");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_capture_journal_is_recovered_after_a_simulated_crash() {
        let d = tmp("crash-capture");
        let cfg = RecoveryConfig::default();
        let f = d.join("a.txt");
        std::fs::write(&f, "before").unwrap();
        {
            let m = RecoveryManager::new(cfg.clone(), d.clone());
            m.begin_turn(&d, Some("c1"), "crash").unwrap();
            let c = m.capture_before(&f).unwrap();
            std::fs::write(&f, "after").unwrap();
            m.capture_after(c);
            // No finish: drop closes the journal and releases the lock, like a process crash.
        }
        let store = RecoveryManager::new(cfg.clone(), d.clone())
            .open_store(&d, false)
            .unwrap();
        let _l = store.lock().unwrap();
        recover_pending(&store, &cfg).unwrap();
        let cursor = store.cursor().unwrap();
        let head = cursor.head.expect("recovered node");
        let node = store.load_node(&head).unwrap();
        assert_eq!(node.status, crate::recovery::model::PointStatus::Recovered);
        assert_eq!(node.ops.len(), 1);
        assert!(node.ops[0].before.is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_tree_delete_is_recoverable_after_a_crash() {
        let d = tmp("crash-tree");
        let cfg = RecoveryConfig::default();
        let sub = d.join("tree");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "a").unwrap();
        std::fs::write(sub.join("b.txt"), "b").unwrap();
        {
            let m = RecoveryManager::new(cfg.clone(), d.clone());
            m.begin_turn(&d, None, "delete tree").unwrap();
            let _cap = m.capture_tree_before(&sub).unwrap();
            std::fs::remove_dir_all(&sub).unwrap();
        }
        let store = RecoveryManager::new(cfg.clone(), d.clone())
            .open_store(&d, false)
            .unwrap();
        let _l = store.lock().unwrap();
        recover_pending(&store, &cfg).unwrap();
        let head = store.cursor().unwrap().head.unwrap();
        let node = store.load_node(&head).unwrap();
        assert_eq!(node.ops.len(), 2);
        assert!(node.ops.iter().all(|o| o.after.is_none()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_partially_applied_restore_is_reverted_from_safety() {
        let d = tmp("crash-restore");
        let cfg = RecoveryConfig::default();
        let store = RecoveryStore::open(&d, &cfg, true).unwrap();
        let f = d.join("a.txt");
        std::fs::write(&f, "one").unwrap();
        let one = store.put_file(&f, 1024).unwrap();
        let safety = SafetySnapshot {
            schema: SCHEMA,
            id: "redo_testpart".into(),
            from_cursor: None,
            target_cursor: None,
            created_at: 1,
            states: vec![SafetyPath { path: "a.txt".into(), state: Some(one) }],
            expected: vec![SafetyPath { path: "a.txt".into(), state: None }],
        };
        store.save_safety(&safety).unwrap();
        std::fs::write(&f, "two").unwrap();
        let mut j = store.new_restore_journal().unwrap();
        j.append(&serde_json::json!({
            "t":"restore_begin","mode":"rollback","safety":"redo_testpart",
            "from":null,"target":null,"at":1
        }), true).unwrap();
        j.append(&serde_json::json!({"t":"applied","path":"a.txt","at":2}), true).unwrap();
        drop(j);
        recover_pending(&store, &cfg).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one");
        assert!(store.journal_paths().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_target_resolver_never_invents_a_parent() {
        let d = tmp("target");
        let cfg = RecoveryConfig::default();
        let m = RecoveryManager::new(cfg.clone(), d.clone());
        m.begin_turn(&d, None, "x").unwrap();
        std::fs::write(d.join("a"), "x").unwrap();
        let c = m.capture_before(&d.join("a")).unwrap();
        std::fs::write(d.join("a"), "y").unwrap();
        m.capture_after(c);
        m.finish_turn(TurnStatus::Completed);
        let store = m.open_store(&d, false).unwrap();
        assert!(resolve(&store, &TargetSpec::Count(2)).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

}
