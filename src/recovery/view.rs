//! The recovery store, described as **data** for a UI to draw.
//!
//! The CLI prints text; the panel in `gui/` lays the same facts out itself. Both read the same store
//! through the same helpers — a UI that parsed the CLI's text would be a second implementation of
//! every rule, and the first one to drift would be the one users act on.
//!
//! # Why this is its own file
//!
//! These items are called by exactly one consumer, the GUI crate. `src/main.rs` compiles this module
//! tree a second time for the `aox` binary, which never calls them — so without the allow below, a
//! release build of the CLI reports fifteen "never used" warnings that are not defects. Keeping them
//! in one file means the `allow` covers one clearly-scoped module instead of being sprinkled over
//! `command.rs`, where it would also hide a genuinely dead CLI helper.
#![allow(dead_code)]

use std::sync::Arc;

use crate::runtime::Runtime;

use super::command::{command_workspace, rollback_spec, CommandReport};
use super::model::{PointKind, RecoveryPoint};
use super::restore::{self, TargetSpec};
use super::store::RecoveryStore;
use super::RecoveryError;

/// One row of the recovery list.
#[derive(Clone, Debug, Default)]
pub struct PointRow {
    pub id: String,
    pub label: String,
    /// `task` / `external` / `manual` — a stable code, **not** localized text: the GUI picks the word.
    pub kind: &'static str,
    /// `complete` / `partial` / `unknown`.
    pub status: &'static str,
    pub paths: usize,
    pub created_at: u64,
    /// This is the cursor's head, i.e. where the next rollback would start from.
    pub head: bool,
    /// The conversation that recorded this point, when it recorded one.
    pub conversation: Option<String>,
    /// Whether it was **this** conversation.
    ///
    /// The store is per workspace, so the chain is shared: opening a new conversation in the same
    /// folder still sees the points the previous one left. That is correct (the files are the same
    /// files, and undoing them from anywhere is legitimate) but it must not **look** like the new
    /// conversation did that work. This flag is what lets the panel say whose point it is.
    ///
    /// `true` also covers "cannot tell": a point with no recorded conversation (a `/checkpoint` from
    /// the CLI, a test) is not accused of belonging to someone else.
    pub mine: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PointsView {
    pub root: String,
    pub rows: Vec<PointRow>,
    pub keep: usize,
    pub truncated: bool,
    /// A rollback is undoable right now (a safety snapshot is waiting).
    pub redoable: bool,
    /// Node files the cursor cannot reach (a crash leftover, or points rolled past). They are **not**
    /// in `rows`; this is only so the panel can say they exist and will be cleaned up.
    pub orphans: usize,
}

/// One path in a rollback preview.
#[derive(Clone, Debug, Default)]
pub struct PathLine {
    pub path: String,
    /// `A` (will be created) / `D` (will be removed) / `M` (will be rewritten).
    pub kind: char,
    pub before: Option<u64>,
    pub after: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct PreviewView {
    /// The point the caller **asked about** — echoed back unchanged.
    ///
    /// This is what any UI matches its answer against. It is deliberately *not* the resolved node:
    /// resolving "undo this point" walks to the parent, so the two ids differ by one step, and a
    /// panel that compared the resolved id against the row it clicked would silently drop every
    /// answer except the one row whose parent happens to be genesis. (That was a real bug.)
    pub requested: String,
    /// The node the restore would actually walk back to; `None` = genesis (the beginning of the
    /// retained chain). Shown in diagnostics, never matched against.
    pub resolved: Option<String>,
    pub lines: Vec<PathLine>,
    pub modified: usize,
    pub added: usize,
    pub removed: usize,
    /// Paths changed outside AOX since the point; rolling back would overwrite them.
    pub conflicts: Vec<String>,
    /// The point (or something it crosses) has incomplete coverage.
    pub partial: bool,
    pub truncated: bool,
    pub warnings: Vec<String>,
}

fn kind_code(k: PointKind) -> &'static str {
    match k {
        PointKind::Turn => "task",
        PointKind::External => "external",
        PointKind::Manual => "manual",
    }
}

fn coverage_code(c: super::model::CoverageState) -> &'static str {
    use super::model::CoverageState::*;
    match c {
        Complete => "complete",
        Partial => "partial",
        Unknown => "unknown",
    }
}

/// The chain the cursor can actually walk: newest first, ending at the root.
///
/// This is the only set a rollback can reach. Everything else on disk is unreachable by definition —
/// either a node written by a crash before the cursor moved, or a point left behind by rolling back
/// past it. The panel lists **this**, because a row you cannot roll back to is a promise you cannot
/// keep.
pub(super) fn chain_nodes(store: &Arc<RecoveryStore>) -> Vec<RecoveryPoint> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut cur = store.cursor().ok().and_then(|c| c.head);
    while let Some(id) = cur {
        if !seen.insert(id.clone()) {
            break; // a corrupt node that points at itself must not hang a listing
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

/// How many node files exist that the cursor cannot reach.
pub(super) fn orphan_count(store: &Arc<RecoveryStore>) -> usize {
    let reachable: std::collections::BTreeSet<String> =
        chain_nodes(store).into_iter().map(|n| n.id).collect();
    store
        .list_nodes()
        .map(|all| all.into_iter().filter(|n| !reachable.contains(&n.id)).count())
        .unwrap_or(0)
}

/// The points in **chain order, newest first**, with any orphan appended. Used by `/checkpoints`,
/// which is a report about what is *stored* — the panel's list is a report about what can be *undone*
/// and therefore filters the orphans out (see [`chain_nodes`]).
///
/// Sorting by `created_at` alone looks right until two points share a second — which they routinely
/// do, because a turn can finish in under a second — and then the order is whatever the file names
/// happen to sort to. The cursor chain is the authoritative answer to "what can I undo, in what
/// order", so that is what this returns. Orphans (a node written by a crash that never published a
/// cursor) come last: they cannot be rolled back to, but they exist on disk until GC sweeps them.
pub(super) fn ordered_nodes(store: &Arc<RecoveryStore>) -> Vec<RecoveryPoint> {
    let mut out = chain_nodes(store);
    let seen: std::collections::BTreeSet<String> = out.iter().map(|n| n.id.clone()).collect();
    if let Ok(mut rest) = store.list_nodes() {
        rest.retain(|n| !seen.contains(&n.id));
        rest.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out.extend(rest);
    }
    out
}

/// Open the store without creating one. `None` = this workspace has never recorded a point.
///
/// Deliberately not routed through [`open`]: that helper turns "no store" into an error message
/// meant for a person reading a terminal, whereas the panel needs to tell "nothing to show yet"
/// apart from "something went wrong" — they look completely different on screen.
fn open_soft(
    root: &std::path::Path,
    cfg: &crate::config::RecoveryConfig,
) -> Result<Option<Arc<RecoveryStore>>, String> {
    let store = match RecoveryStore::open(root, cfg, false) {
        Ok(s) => Arc::new(s),
        Err(RecoveryError::NoStore(_)) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    {
        let _lock = store.lock().map_err(|e| e.to_string())?;
        restore::recover_pending(&store, cfg).map_err(|e| e.to_string())?;
    }
    Ok(Some(store))
}

/// Everything the recovery panel needs to draw its list.
pub fn points_view(rt: &Arc<Runtime>, current: Option<&str>) -> Result<PointsView, String> {
    let root = command_workspace(rt)?;
    let cfg = rt.config.recovery.clone();
    let mut view = PointsView { root: root.display().to_string(), keep: cfg.keep, ..Default::default() };
    if !cfg.enabled {
        return Ok(view);
    }
    let Some(store) = open_soft(&root, &cfg)? else { return Ok(view) };
    let cursor = store.cursor().map_err(|e| e.to_string())?;
    let nodes = chain_nodes(&store);
    view.orphans = orphan_count(&store);
    view.truncated = cursor.truncated;
    view.redoable = !cursor.redo.is_empty();
    view.rows = nodes
        .into_iter()
        .map(|n| {
            let mine = match (current, n.conversation.as_deref()) {
                (Some(cur), Some(conv)) => cur == conv,
                // Unknown on either side: do not attribute it to another conversation.
                _ => true,
            };
            PointRow {
                head: cursor.head.as_deref() == Some(n.id.as_str()),
                id: n.id,
                label: n.label,
                kind: kind_code(n.kind),
                status: coverage_code(n.coverage.state),
                paths: n.ops.len(),
                created_at: n.created_at,
                conversation: n.conversation,
                mine,
            }
        })
        .collect();
    Ok(view)
}

/// Resolve `target` and describe what a rollback would do — read-only, writes nothing.
pub fn preview_view(rt: &Arc<Runtime>, target: &str) -> Result<PreviewView, String> {
    let root = command_workspace(rt)?;
    let cfg = rt.config.recovery.clone();
    if !cfg.enabled {
        return Err("task recovery is disabled ([recovery] enabled = false)".into());
    }
    let store = open_soft(&root, &cfg)?
        .ok_or_else(|| "no recovery points in this workspace yet".to_string())?;
    // The GUI's row means "undo this point", not "stand on it" — see `TargetSpec::Before`.
    let t = restore::resolve(&store, &TargetSpec::Before(target.to_string()))
        .map_err(|e| e.to_string())?;
    let plan = restore::build_plan(&store, &cfg, &t).map_err(|e| e.to_string())?;
    let mut view = PreviewView {
        requested: target.to_string(),
        resolved: plan.target.clone(),
        conflicts: plan.conflicts.clone(),
        partial: plan.partial,
        truncated: plan.truncated,
        warnings: plan.warnings.clone(),
        ..Default::default()
    };
    for a in &plan.actions {
        let kind = match (&a.current, &a.restore) {
            (None, Some(_)) => 'A',
            (Some(_), None) => 'D',
            _ => 'M',
        };
        match kind {
            'A' => view.added += 1,
            'D' => view.removed += 1,
            _ => view.modified += 1,
        }
        view.lines.push(PathLine {
            path: a.path.clone(),
            kind,
            before: a.current.as_ref().map(|b| b.size),
            after: a.restore.as_ref().map(|b| b.size),
        });
    }
    Ok(view)
}

/// Apply a rollback to `target`. `force` overwrites files changed outside AOX; `allow_partial`
/// crosses points whose coverage is incomplete. Both are **separate** consents, exactly as in the
/// CLI: they answer different questions and a UI that merged them would be asking for one and
/// getting the other.
pub fn rollback_to(
    rt: &Arc<Runtime>,
    target: &str,
    force: bool,
    allow_partial: bool,
) -> Result<CommandReport, String> {
    let root = command_workspace(rt)?;
    let cfg = rt.config.recovery.clone();
    if !cfg.enabled {
        return Err("task recovery is disabled ([recovery] enabled = false)".into());
    }
    rollback_spec(&root, &cfg, TargetSpec::Before(target.to_string()), force, allow_partial)
}


#[cfg(test)]
mod tests {
    //! These read a real store through a real `Runtime`, because that is the whole contract: the
    //! panel shows whatever these two functions say, and a bug here is a bug the user acts on.

    use super::*;
    use crate::recovery::RecoveryManager;

    fn rt_for(dir: &std::path::Path) -> Arc<Runtime> {
        let mut cfg = crate::config::Config::dummy();
        cfg.workplace = crate::config::WorkplaceConfig {
            mode: "fixed".into(),
            root: Some(dir.to_string_lossy().into_owned()),
        };
        let rt = Runtime::new(cfg, dir.to_path_buf(), dir.join("plugins")).expect("runtime");
        rt.workplace.ensure_started(dir).expect("workspace");
        rt
    }

    #[test]
    fn points_view_is_newest_first_and_marks_the_head() {
        let d = std::env::temp_dir().join("aox-view-points");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let m = RecoveryManager::new(crate::config::RecoveryConfig::default(), d.clone());
        for i in 0..3 {
            let f = d.join(format!("f{i}.txt"));
            m.begin_turn(&d, None, &format!("turn {i}")).unwrap();
            let cap = m.capture_before(&f).unwrap();
            std::fs::write(&f, format!("v{i}")).unwrap();
            m.capture_after(cap);
            assert!(m.finish_turn(crate::recovery::model::TurnStatus::Completed).point.is_some());
        }
        let rt = rt_for(&d);
        let view = points_view(&rt, None).unwrap();
        assert_eq!(view.rows.len(), 3);
        assert_eq!(view.rows[0].label, "turn 2", "newest first");
        assert!(view.rows[0].head, "the newest point is where a rollback starts");
        assert!(!view.rows[1].head);
        assert_eq!(view.rows[0].kind, "task");
        assert_eq!(view.rows[0].status, "complete");
        assert_eq!(view.rows[0].paths, 1);
        assert!(!view.truncated);
        assert!(!view.redoable);
        assert!(view.root.contains("aox-view-points"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_point_says_whether_this_conversation_recorded_it() {
        let d = std::env::temp_dir().join("aox-view-owner");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let m = RecoveryManager::new(crate::config::RecoveryConfig::default(), d.clone());
        // Two tasks recorded by conversation "a", one by "b", and one with no conversation at all.
        for (i, conv) in [(0, Some("a")), (1, Some("a")), (2, Some("b")), (3, None)] {
            m.begin_turn(&d, conv, &format!("task {i}")).unwrap();
            let f = d.join(format!("f{i}.txt"));
            let cap = m.capture_before(&f).unwrap();
            std::fs::write(&f, format!("v{i}")).unwrap();
            m.capture_after(cap);
            m.finish_turn(crate::recovery::model::TurnStatus::Completed).point.expect("a node");
        }
        let rt = rt_for(&d);

        let view = points_view(&rt, Some("a")).unwrap();
        let flags: Vec<(&str, bool)> =
            view.rows.iter().map(|r| (r.label.as_str(), r.mine)).collect();
        assert_eq!(
            flags,
            vec![("task 3", true), ("task 2", false), ("task 1", true), ("task 0", true)],
            "the one recorded by another conversation is marked; the unrecorded one is not accused"
        );

        // A conversation that has recorded nothing still sees the chain — the files are shared — but
        // every attributed row says it is not theirs.
        let view = points_view(&rt, Some("c")).unwrap();
        // Three points carry someone else's id (two from "a", one from "b"); the unrecorded one stays
        // unlabelled.
        assert_eq!(view.rows.iter().filter(|r| !r.mine).count(), 3);

        // Nobody to attribute to: keep it quiet rather than label everything foreign.
        let view = points_view(&rt, None).unwrap();
        assert!(view.rows.iter().all(|r| r.mine));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_rollback_leaves_a_redo_the_panel_can_offer() {
        let d = std::env::temp_dir().join("aox-view-redo");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let m = RecoveryManager::new(crate::config::RecoveryConfig::default(), d.clone());
        m.begin_turn(&d, None, "one task").unwrap();
        let cap = m.capture_before(&d.join("a.txt")).unwrap();
        std::fs::write(d.join("a.txt"), "written").unwrap();
        m.capture_after(cap);
        m.finish_turn(crate::recovery::model::TurnStatus::Completed).point.expect("a node");

        let rt = rt_for(&d);
        let view = points_view(&rt, None).unwrap();
        assert!(!view.redoable, "nothing has been rolled back yet");
        let head = view.rows[0].id.clone();
        assert!(view.rows[0].head);

        rollback_to(&rt, &head, false, false).unwrap();
        let view = points_view(&rt, None).unwrap();
        assert!(view.redoable, "the safety snapshot is exactly what `/redo` needs");
        assert!(!d.join("a.txt").exists(), "the file really went away");
        // Rolling back past the only point leaves the cursor at genesis: nothing is undoable, and the
        // node that described the vanished tree is **unreachable**. It must not stay in the list —
        // a row nobody can roll back to is a promise nobody can keep. `/checkpoints` still reports it,
        // because that command is about what is stored.
        assert!(view.rows.is_empty(), "{:?}", view.rows.iter().map(|r| &r.id).collect::<Vec<_>>());
        assert_eq!(view.orphans, 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rolling_back_the_newest_point_leaves_the_rest_listed_and_undoable() {
        let d = std::env::temp_dir().join("aox-view-middle");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let m = RecoveryManager::new(crate::config::RecoveryConfig::default(), d.clone());
        for i in 0..3 {
            m.begin_turn(&d, None, &format!("task {i}")).unwrap();
            let f = d.join(format!("f{i}.txt"));
            let cap = m.capture_before(&f).unwrap();
            std::fs::write(&f, format!("v{i}")).unwrap();
            m.capture_after(cap);
            m.finish_turn(crate::recovery::model::TurnStatus::Completed).point.expect("a node");
        }
        let rt = rt_for(&d);
        let view = points_view(&rt, None).unwrap();
        assert_eq!(view.rows.len(), 3);
        let newest = view.rows[0].id.clone();

        rollback_to(&rt, &newest, false, false).unwrap();
        let view = points_view(&rt, None).unwrap();
        assert_eq!(view.rows.len(), 2, "the two older points are still undoable");
        assert_eq!(view.rows[0].label, "task 1", "and `task 1` is now the head");
        assert!(view.rows[0].head);
        assert_eq!(view.orphans, 1, "the undone point is now unreachable");
        assert!(view.redoable);
        assert!(!d.join("f2.txt").exists());

        // The row that is left still previews — this is the case that was broken in a real screenshot
        // (the answer was matched against the resolved parent instead of the clicked row).
        let next = view.rows[0].id.clone();
        let p = preview_view(&rt, &next).unwrap();
        assert_eq!(p.requested, next);
        assert!(!p.lines.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn preview_view_says_what_would_change_and_notices_drift() {
        let d = std::env::temp_dir().join("aox-view-preview");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let m = RecoveryManager::new(crate::config::RecoveryConfig::default(), d.clone());
        let a = d.join("a.txt");
        std::fs::write(&a, "one").unwrap();
        m.begin_turn(&d, None, "edit and create").unwrap();
        let cap = m.capture_before(&a).unwrap();
        std::fs::write(&a, "two").unwrap();
        m.capture_after(cap);
        let c = m.capture_before(&d.join("new.txt")).unwrap();
        std::fs::write(d.join("new.txt"), "fresh").unwrap();
        m.capture_after(c);
        let id = m.finish_turn(crate::recovery::model::TurnStatus::Completed).point.expect("a node");

        let rt = rt_for(&d);
        let view = preview_view(&rt, &id).unwrap();
        // The echo, and the step the restore would actually take. These two ids are **not** the same,
        // and a UI that matched on the resolved one would drop this answer.
        assert_eq!(view.requested, id);
        assert_ne!(view.resolved.as_deref(), Some(id.as_str()));
        assert!(view.resolved.is_none(), "one node deep, so the restore walks to genesis");
        assert_eq!(view.modified, 1, "a.txt is rewritten back");
        assert_eq!(view.removed, 1, "new.txt did not exist before this point, so it goes");
        assert_eq!(view.added, 0);
        assert!(view.conflicts.is_empty());
        let mut kinds: Vec<char> = view.lines.iter().map(|l| l.kind).collect();
        kinds.sort();
        assert_eq!(kinds, vec!['D', 'M']);

        // Change the file behind the point's back: the preview must say so *before* the user clicks,
        // because rolling back would silently swallow that edit.
        std::fs::write(&a, "edited outside").unwrap();
        let view = preview_view(&rt, &id).unwrap();
        assert_eq!(view.conflicts.len(), 1, "{:?}", view.conflicts);

        // An unknown id is an error, not an empty plan: the panel shows the message instead of a
        // "0 paths" preview that would look like a no-op rollback.
        assert!(preview_view(&rt, "rp-does-not-exist").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

}
