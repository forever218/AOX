//! CLI surface for recovery. The GUI uses the same types through its bridge.

use std::sync::Arc;

use crate::runtime::Runtime;

use super::model::{CoverageState, PointKind, RecoveryPoint, RestorePlan, RestoreReport, SCHEMA};
use super::restore::{self, TargetSpec};
use super::store::RecoveryStore;
use super::RecoveryError;

#[derive(Clone, Debug)]
pub enum RecoveryCmd {
    List { all: bool },
    Checkpoint { label: String },
    Rollback {
        target: Option<String>,
        yes: bool,
        force: bool,
        allow_partial: bool,
    },
    Redo,
    Status,
    Verify,
    Prune,
    /// Drop every point in this workspace (into the recycle bin).
    Clear,
}

#[derive(Clone, Debug, Default)]
pub struct CommandReport {
    pub text: String,
    pub restore: Option<RestoreReport>,
}

pub fn is_cmd(line: &str) -> bool {
    let l = line.trim_start();
    l == "/checkpoints"
        || l.starts_with("/checkpoints ")
        || l == "/checkpoint"
        || l.starts_with("/checkpoint ")
        || l == "/rollback"
        || l.starts_with("/rollback ")
        || l == "/redo"
        || l == "/backup"
        || l.starts_with("/backup ")
}

pub fn parse(line: &str) -> Result<RecoveryCmd, String> {
    let mut parts = line.split_whitespace();
    let head = parts.next().unwrap_or("");
    match head {
        "/checkpoints" => {
            let mut all = false;
            for p in parts {
                if p != "--all" {
                    return Err(format!("unknown /checkpoints option {p}"));
                }
                all = true;
            }
            Ok(RecoveryCmd::List { all })
        }
        "/checkpoint" => {
            let label = parts.collect::<Vec<_>>().join(" ");
            if label.trim().is_empty() {
                return Err("usage: /checkpoint <label>".into());
            }
            Ok(RecoveryCmd::Checkpoint { label })
        }
        "/rollback" => {
            let mut target = None;
            let mut yes = false;
            let mut force = false;
            let mut allow_partial = false;
            for p in parts {
                match p {
                    "--yes" => yes = true,
                    "--force" => {
                        force = true;
                        yes = true;
                    }
                    "--allow-partial" => allow_partial = true,
                    other if other.starts_with('-') => {
                        return Err(format!("unknown /rollback option {other}"))
                    }
                    other if target.is_none() => target = Some(other.to_string()),
                    other => return Err(format!("unexpected extra argument {other}")),
                }
            }
            Ok(RecoveryCmd::Rollback { target, yes, force, allow_partial })
        }
        "/redo" => Ok(RecoveryCmd::Redo),
        "/backup" => match parts.next() {
            None | Some("status") => Ok(RecoveryCmd::Status),
            Some("verify") => Ok(RecoveryCmd::Verify),
            Some("prune") => Ok(RecoveryCmd::Prune),
            Some(other) => Err(format!("unknown /backup action {other}; use status / verify / prune")),
        },
        other => Err(format!("not a recovery command: {other}")),
    }
}

pub fn run(rt: &Arc<Runtime>, cmd: RecoveryCmd) -> Result<CommandReport, String> {
    let root = command_workspace(rt)?;
    let cfg = rt.config.recovery.clone();
    if !cfg.enabled {
        return Err("task recovery is disabled ([recovery] enabled = false)".into());
    }
    match cmd {
        RecoveryCmd::List { all } => list(&root, &cfg, all),
        RecoveryCmd::Checkpoint { label } => checkpoint(&root, &cfg, &label),
        RecoveryCmd::Status => status(&root, &cfg),
        RecoveryCmd::Verify => verify(&root, &cfg),
        RecoveryCmd::Prune => prune(&root, &cfg),
        RecoveryCmd::Clear => clear(&root, &cfg),
        RecoveryCmd::Rollback { target, yes, force, allow_partial } => {
            rollback(&root, &cfg, target.as_deref(), yes, force, allow_partial)
        }
        RecoveryCmd::Redo => redo(&root, &cfg),
    }
}

/// Resolve the workspace for a recovery command even before the conversation's first message.
pub(super) fn command_workspace(rt: &Arc<Runtime>) -> Result<std::path::PathBuf, String> {
    if let Some(root) = rt.workplace.active_root() {
        return Ok(root);
    }
    let (mode, _) = rt.workplace.snapshot();
    if matches!(mode, crate::workplace::Mode::Full) {
        return Err("full access has no protected workspace; switch to default / a chosen folder to use /rollback".into());
    }
    rt.workplace
        .ensure_started(&rt.root)
        .map_err(|e| format!("could not establish the workspace: {e:#}"))?;
    rt.workplace
        .active_root()
        .ok_or_else(|| "no protected workspace is active".to_string())
}

pub(super) fn open(root: &std::path::Path, cfg: &crate::config::RecoveryConfig, create: bool) -> Result<Arc<RecoveryStore>, String> {
    let store = RecoveryStore::open(root, cfg, create).map(Arc::new).map_err(|e| match e {
        RecoveryError::NoStore(_) => "no recovery points in this workspace yet".to_string(),
        other => other.to_string(),
    })?;
    // Resolve any crash-left journal before showing a list or planning a restore. The lock is held
    // only for this reconciliation; `apply_rollback` re-checks the cursor after taking its own.
    {
        let _lock = store.lock().map_err(|e| e.to_string())?;
        restore::recover_pending(&store, cfg).map_err(|e| e.to_string())?;
    }
    Ok(store)
}

fn list(root: &std::path::Path, cfg: &crate::config::RecoveryConfig, all: bool) -> Result<CommandReport, String> {
    let store = match open(root, cfg, false) {
        Ok(s) => s,
        Err(e) if e.starts_with("no recovery points") => return Ok(CommandReport { text: e, restore: None }),
        Err(e) => return Err(e),
    };
    let cursor = store.cursor().map_err(|e| e.to_string())?;
    // Chain order, not timestamp order: two points can share a second, and "newest first" that is
    // really "whatever the file names sort to" would make the numbering in `/rollback n` a lie.
    let nodes = super::view::ordered_nodes(&store);
    let mut out = String::new();
    out.push_str(&crate::tf!(
        "{} recovery point(s); cursor: {}",
        nodes.len(),
        cursor.head.as_deref().unwrap_or("genesis")
    ));
    if cursor.truncated {
        out.push_str(" (older history truncated)");
    }
    for n in nodes.iter().take(if all { usize::MAX } else { 20 }) {
        out.push_str(&format!(
            "\n  {}  {}  {}  {}  {} path(s)  {}  {}",
            n.id,
            local_time(n.created_at),
            kind_label(n.kind),
            status_label(n.status),
            n.ops.len(),
            coverage_label(n.coverage.state),
            if n.label.is_empty() { "-" } else { &n.label }
        ));
    }
    if let Some(h) = &cursor.head {
        if !nodes.iter().any(|n| &n.id == h) {
            out.push_str(&format!("\n  warning: cursor head {h} has no node file"));
        }
    }
    Ok(CommandReport { text: out, restore: None })
}

fn checkpoint(root: &std::path::Path, cfg: &crate::config::RecoveryConfig, label: &str) -> Result<CommandReport, String> {
    let store = open(root, cfg, true)?;
    let _lock = store.lock().map_err(|e| e.to_string())?;
    let mut cursor = store.cursor().map_err(|e| e.to_string())?;
    let node = RecoveryPoint {
        schema: SCHEMA,
        id: super::store::new_id("rp"),
        parent: cursor.head.clone(),
        kind: PointKind::Manual,
        status: super::model::PointStatus::Completed,
        created_at: crate::history::unix_now(),
        finished_at: crate::history::unix_now(),
        conversation: None,
        turn: None,
        label: label.chars().take(80).collect(),
        ops: Vec::new(),
        coverage: super::model::Coverage::complete(),
        redoable: true,
    };
    store.save_node(&node).map_err(|e| e.to_string())?;
    cursor.head = Some(node.id.clone());
    cursor.redo.clear();
    cursor.updated_at = crate::history::unix_now();
    store.save_cursor(&cursor).map_err(|e| e.to_string())?;
    Ok(CommandReport {
        text: crate::tf!(
            "saved checkpoint {} ({})\nit sits inside the same retention window as everything else: [recovery] keep = {} bounds how many tasks stay undoable, so this anchor is dropped once {} newer tasks have been recorded",
            node.id,
            node.label,
            cfg.keep,
            cfg.keep
        ),
        restore: None,
    })
}

fn status(root: &std::path::Path, cfg: &crate::config::RecoveryConfig) -> Result<CommandReport, String> {
    let store = match open(root, cfg, false) {
        Ok(s) => s,
        Err(e) if e.starts_with("no recovery points") => {
            return Ok(CommandReport { text: e, restore: None })
        }
        Err(e) => return Err(e),
    };
    let cursor = store.cursor().map_err(|e| e.to_string())?;
    let nodes = store.list_nodes().map_err(|e| e.to_string())?;
    let bytes = store.disk_usage();
    let mut out = crate::tf!(
        "workspace: {}\nhead: {}\npoints: {}   size: {}\nredo depth: {}",
        root.display(),
        cursor.head.as_deref().unwrap_or("genesis"),
        nodes.len(),
        fmt_bytes(bytes),
        cursor.redo.len()
    );
    if cursor.truncated {
        out.push_str("\nolder history: truncated");
    }
    // The baseline is not a checkpoint and does not roll back to anything, so it is easy to mistake
    // for waste. Saying what it is answers that in one line.
    let baseline = store.load_baseline();
    if baseline.is_empty() {
        out.push_str(&crate::tf!(
            "\nshell guard: {}   baseline: none yet (the first shell command builds one)",
            guard_label(cfg)
        ));
    } else {
        let hashed: u64 = baseline.values().map(|b| b.size).sum();
        out.push_str(&crate::tf!(
            "\nshell guard: {}   baseline: {} file(s), {} of content cached for the next command",
            guard_label(cfg),
            baseline.len(),
            fmt_bytes(hashed)
        ));
    }
    // "How far back can I go" is the question this line answers, and it is not the same number as
    // "how many points exist" — `keep` is what bounds the undo depth.
    out.push_str(&crate::tf!(
        "\nretention: keep = {} turn(s){}",
        cfg.keep,
        if cursor.truncated { "; older points were already dropped" } else { "" }
    ));
    Ok(CommandReport { text: out, restore: None })
}

fn guard_label(cfg: &crate::config::RecoveryConfig) -> &'static str {
    match cfg.shell_guard {
        crate::config::ShellGuard::Strict => "strict (scan before and after, command refuses if the scan cannot finish)",
        crate::config::ShellGuard::BestEffort => {
            "best effort (scan only what changed; the recovery point is marked partial)"
        }
        crate::config::ShellGuard::Off => "off (shell writes are not covered)",
    }
}

fn verify(root: &std::path::Path, cfg: &crate::config::RecoveryConfig) -> Result<CommandReport, String> {
    let store = match open(root, cfg, false) {
        Ok(s) => s,
        Err(e) if e.starts_with("no recovery points") => {
            return Ok(CommandReport { text: e, restore: None })
        }
        Err(e) => return Err(e),
    };
    let nodes = store.list_nodes().map_err(|e| e.to_string())?;
    let mut checked = 0;
    let mut bad = Vec::new();
    for n in nodes {
        for op in n.ops {
            for b in [op.before.as_ref(), op.after.as_ref()].into_iter().flatten() {
                if !b.stored {
                    continue;
                }
                checked += 1;
                if let Err(e) = store.verify_blob(b) {
                    bad.push(format!("{}: {e}", op.path));
                }
            }
        }
    }
    Ok(CommandReport {
        text: if bad.is_empty() {
            crate::tf!("verified {} object reference(s); no missing or corrupt blob", checked)
        } else {
            format!("verified {checked} object reference(s); {} problem(s):\n  {}", bad.len(), bad.join("\n  "))
        },
        restore: None,
    })
}

/// `/backup prune`: apply retention and sweep unreachable objects.
fn clear(root: &std::path::Path, cfg: &crate::config::RecoveryConfig) -> Result<CommandReport, String> {
    let store = match open(root, cfg, false) {
        Ok(s) => s,
        Err(e) if e.starts_with("no recovery points") => {
            return Ok(CommandReport { text: e, restore: None })
        }
        Err(e) => return Err(e),
    };
    let _lock = store.lock().map_err(|e| e.to_string())?;
    let (points, files) = super::gc::clear_all(&store).map_err(|e| format!("{e:#}"))?;
    Ok(CommandReport {
        text: crate::tf!(
            "Cleared {} recovery point(s) and {} stored file(s). Both are in the system recycle bin and can be restored from there.",
            points,
            files
        ),
        restore: None,
    })
}

fn prune(root: &std::path::Path, cfg: &crate::config::RecoveryConfig) -> Result<CommandReport, String> {
    let store = match open(root, cfg, false) {
        Ok(s) => s,
        Err(e) if e.starts_with("no recovery points") => {
            return Ok(CommandReport { text: e, restore: None })
        }
        Err(e) => return Err(e),
    };
    let _lock = store.lock().map_err(|e| e.to_string())?;
    let before = store.disk_usage();
    let report = super::gc::prune(&store, cfg.keep).map_err(|e| e.to_string())?;
    let after = store.disk_usage();
    if report.is_empty() {
        return Ok(CommandReport {
            text: crate::tf!(
                "nothing to prune: {} point(s) within keep = {}, {} on disk",
                store.list_nodes().map(|n| n.len()).unwrap_or(0),
                cfg.keep,
                fmt_bytes(after)
            ),
            restore: None,
        });
    }
    let mut text = crate::tf!(
        "pruned {} point(s), {} object(s), {} safety snapshot(s): {} → {} (freed {})",
        report.nodes_dropped,
        report.objects_dropped,
        report.safety_dropped,
        fmt_bytes(before),
        fmt_bytes(after),
        fmt_bytes(before.saturating_sub(after))
    );
    if report.truncated {
        text.push_str(&crate::tf!(
            "\nthe oldest part of the branch is gone; /rollback now reaches at most {} step(s) back",
            cfg.keep
        ));
    }
    Ok(CommandReport { text, restore: None })
}

fn rollback(
    root: &std::path::Path,
    cfg: &crate::config::RecoveryConfig,
    target: Option<&str>,
    yes: bool,
    force: bool,
    allow_partial: bool,
) -> Result<CommandReport, String> {
    let store = match open(root, cfg, false) {
        Ok(s) => s,
        Err(e) => return Err(e),
    };
    let spec = match target {
        None => TargetSpec::Count(1),
        Some(s) if s.chars().all(|c| c.is_ascii_digit()) => TargetSpec::Count(
            s.parse::<usize>().map_err(|_| format!("bad rollback count {s}"))?,
        ),
        Some(s) => TargetSpec::Id(s.to_string()),
    };
    rollback_spec_with(&store, cfg, spec, yes, force, allow_partial)
}

/// The shared body of both entry points: the CLI resolves a count or an id, the panel resolves TargetSpec::Before...
pub(super) fn rollback_spec(
    root: &std::path::Path,
    cfg: &crate::config::RecoveryConfig,
    spec: TargetSpec,
    force: bool,
    allow_partial: bool,
) -> Result<CommandReport, String> {
    let store = match open(root, cfg, false) {
        Ok(s) => s,
        Err(e) => return Err(e),
    };
    rollback_spec_with(&store, cfg, spec, true, force, allow_partial)
}

fn rollback_spec_with(
    store: &Arc<RecoveryStore>,
    cfg: &crate::config::RecoveryConfig,
    spec: TargetSpec,
    yes: bool,
    force: bool,
    allow_partial: bool,
) -> Result<CommandReport, String> {
    let t = restore::resolve(store, &spec).map_err(|e| e.to_string())?;
    let plan = restore::build_plan(store, cfg, &t).map_err(|e| e.to_string())?;
    let mut text = preview(&plan);
    if !yes {
        text.push_str("\nThis was a preview. Add --yes to apply.");
        return Ok(CommandReport { text, restore: None });
    }
    if !plan.conflicts.is_empty() && !force {
        return Err(format!(
            "{}\nNothing was written. Save the external changes, or use --force to overwrite them (a safety snapshot is saved first).",
            text
        ));
    }
    if plan.partial && !allow_partial {
        return Err(format!(
            "{}\nThis rollback crosses incomplete coverage. Add --allow-partial only if you accept that.",
            text
        ));
    }
    let report = restore::apply_rollback(&store, cfg, plan, force, allow_partial)
        .map_err(|e| format!("{text}\n\n{e}"))?;
    text.push_str(&format!(
        "\nRestored {} path(s), removed {} path(s). safety={}",
        report.restored,
        report.removed,
        report.safety_id.as_deref().unwrap_or("-")
    ));
    if !report.warnings.is_empty() {
        text.push_str("\nwarnings:\n  ");
        text.push_str(&report.warnings.join("\n  "));
    }
    Ok(CommandReport { text, restore: Some(report) })
}

fn redo(root: &std::path::Path, cfg: &crate::config::RecoveryConfig) -> Result<CommandReport, String> {
    let store = match open(root, cfg, false) {
        Ok(s) => s,
        Err(e) => return Err(e),
    };
    let report = restore::apply_redo(&store, cfg).map_err(|e| e.to_string())?;
    Ok(CommandReport {
        text: crate::tf!(
            "redo complete: restored {} path(s), removed {} path(s)",
            report.restored,
            report.removed
        ),
        restore: Some(report),
    })
}

fn preview(plan: &RestorePlan) -> String {
    let mut out = crate::tf!(
        "rollback {} -> {}",
        plan.from.as_deref().unwrap_or("genesis"),
        plan.target.as_deref().unwrap_or("genesis")
    );
    if plan.actions.is_empty() {
        out.push_str("\n(no file changes)");
    } else {
        out.push_str(&crate::tf!("\n{} path(s):", plan.actions.len()));
        for a in &plan.actions {
            let tag = match (&a.current, &a.restore) {
                (None, Some(_)) => "A",
                (Some(_), None) => "D",
                _ => "M",
            };
            out.push_str(&format!(
                "\n  {tag}  {}  {} -> {}",
                a.path,
                a.current.as_ref().map(|b| fmt_bytes(b.size)).unwrap_or_else(|| "(absent)".into()),
                a.restore.as_ref().map(|b| fmt_bytes(b.size)).unwrap_or_else(|| "(removed)".into())
            ));
        }
    }
    if !plan.conflicts.is_empty() {
        out.push_str(&crate::tf!("\nconflicts ({}):", plan.conflicts.len()));
        for c in &plan.conflicts {
            out.push_str(&format!("\n  ! {c}"));
        }
    }
    if plan.partial {
        out.push_str("\nincomplete coverage: this rollback cannot be guaranteed for every write");
    }
    if plan.truncated {
        out.push_str("\nolder history is truncated; only the retained chain can be rolled back");
    }
    for w in &plan.warnings {
        out.push_str(&format!("\n  note: {w}"));
    }
    out
}

fn coverage_label(c: CoverageState) -> &'static str {
    match c {
        CoverageState::Complete => "complete",
        CoverageState::Partial => "partial",
        CoverageState::Unknown => "unknown",
    }
}

fn kind_label(k: PointKind) -> &'static str {
    match k {
        PointKind::Turn => "task",
        PointKind::External => "external",
        PointKind::Manual => "manual",
    }
}

fn status_label(s: super::model::PointStatus) -> &'static str {
    use super::model::PointStatus::*;
    match s {
        Completed => "ok",
        Failed => "failed",
        Interrupted => "interrupted",
        Recovered => "recovered",
    }
}

fn local_time(ts: u64) -> String {
    let (y, mo, d, h, mi, _) = crate::history::civil_from_unix(ts);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC")
}

fn fmt_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_recovery_commands() {
        assert!(matches!(parse("/checkpoints").unwrap(), RecoveryCmd::List { all: false }));
        assert!(matches!(parse("/checkpoints --all").unwrap(), RecoveryCmd::List { all: true }));
        assert!(matches!(parse("/rollback").unwrap(), RecoveryCmd::Rollback { yes: false, .. }));
        assert!(matches!(
            parse("/rollback 2 --yes").unwrap(),
            RecoveryCmd::Rollback { yes: true, target: Some(ref t), .. } if t == "2"
        ));
        assert!(matches!(parse("/backup status").unwrap(), RecoveryCmd::Status));
        assert!(parse("/backup nope").is_err());
    }

    #[test]
    fn command_detection_does_not_match_a_model_message() {
        assert!(!is_cmd("please rollback the last change"));
        assert!(is_cmd("/rollback"));
        assert!(is_cmd("/checkpoints --all"));
    }
}
