//! Workspace: where the model's file capabilities may be confined.
//! This is the host side of sandbox approach "take file capability back to the host". It answers one question: "is...
//! # Three tiers.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::config::WorkplaceConfig;
use crate::i18n::t;
use crate::tf;

/// Subdirectory under the app root where **durable deliverables** go, one folder per project
/// (`<app root>/workplace/<project>/`).
pub const WORKPLACE_DIR: &str = "workplace";

/// Scratch subdirectory of AOX_DIR (`<workspace>/.aox/tmp/`) —— screenshots, caches, one-off output.
pub const TMP_DIR: &str = "tmp";

/// How old a scratch file has to be before a turn-end sweep removes it.
pub const SCRATCH_GRACE: std::time::Duration = std::time::Duration::from_secs(600);

/// How old a scratch file has to be before a turn-start sweep removes it.
pub const SCRATCH_STALE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// What a scratch sweep removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
}

impl Swept {
    pub fn any(&self) -> bool {
        self.files + self.dirs > 0
    }
}

/// Empty the workspace's scratch directory of everything older than `min_age`.
pub fn sweep_scratch(ws: &Path, min_age: std::time::Duration) -> Swept {
    let dir = ws.join(AOX_DIR).join(TMP_DIR);
    let mut out = Swept::default();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        // No scratch directory: nothing to do (a workspace that was never started).
        return out;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or_default();
        if age < min_age {
            continue;
        }
        if meta.is_dir() {
            // Recursive: `__pycache__`, `pip-xxxx`, a scratch folder the model made for itself.
            if std::fs::remove_dir_all(&path).is_ok() {
                out.dirs += 1;
            }
        } else if meta.is_file() && std::fs::remove_file(&path).is_ok() {
            out.files += 1;
            out.bytes += meta.len();
        }
    }
    out
}

/// Subdirectory of AOX_DIR holding conversations (`<workspace>/.aox/history/`).
pub const AOX_DIR: &str = ".aox";

/// Subdirectory of [`AOX_DIR`] holding the AI's own dated markdown notes
/// (`<root>/.aox/memory/<YYYY-MM-DD>.md`). Created alongside [`AOX_DIR`] on first use.
pub const MEMORY_DIR: &str = "memory";

/// The key in the parameters of the host's built-in file tools that denotes "path".
pub const PATH_ARG: &str = "path";

/// When a parameter name hits one of these words, its value is treated as a path (only used for the coarse scan...
const PATH_KEYS: &[&str] = &[
    "path", "paths", "file", "files", "filename", "filepath", "file_path", "dir", "dirs", "directory",
    "folder", "root", "src", "dst", "dest", "destination", "target", "cwd", "workdir", "working_dir",
    "save_to", "input_path", "output_path",
];

/// The runtime tier. The GUI / CLI can switch it, taking effect immediately (the next tool call
/// is judged under the new tier).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Confined to the app root; every new conversation starts there again.
    Default,
    /// A fixed plot.
    Fixed(PathBuf),
    /// Unrestricted.
    Full,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Default => t("Default"),
            Mode::Fixed(_) => t("Picked workspace"),
            Mode::Full => t("Full access"),
        }
    }
}

/// The intent of a path access —— decides whether the workspace boundary governs it under the confined tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read-only (`read_file` / `list_dir`): **the whole device can be read**, not limited by the
    /// workspace.
    Read,
    /// Write-to-disk (`write_file` / `remove`): **must land inside the workspace** (except the
    /// `full` tier).
    Write,
}

/// The currently effective state.
#[derive(Debug, Clone)]
enum Active {
    /// Unrestricted (config is `full`).
    Full,
    /// Confined, but the root is not yet decided: `default` tier, this conversation hasn't sent a message yet.
    Pending,
    /// Confined, root already decided.
    At(PathBuf),
}

/// A conversation's workspace record (the `kind:"workplace"` line in `.jsonl`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Record {
    /// `"default"` / `"fixed"` / `"full"`. Empty string = unrecognized (caller should treat as no
    /// record).
    pub mode: String,
    /// The root bound to the current tier; `full` has no root.
    pub root: Option<PathBuf>,
    /// This conversation's own root under the "default" tier (is `None` if not yet established).
    pub own: Option<PathBuf>,
}

impl Record {
    /// Serialize into one `.jsonl` line (`kind` is filled in by the storage layer, see `store::jsonl`).
    pub fn to_value(&self) -> serde_json::Value {
        let mut v = serde_json::json!({ "mode": self.mode });
        let mut put = |key: &str, p: &Option<PathBuf>| {
            if let Some(p) = p {
                v[key] = serde_json::json!(p.display().to_string());
            }
        };
        put("root", &self.root);
        put("own", &self.own);
        v
    }

    /// Read back from one line; **unrecognized lines return `None`** (missing `mode`, or value
    /// not a string).
    pub fn from_value(v: &serde_json::Value) -> Option<Self> {
        let mode = v.get("mode").and_then(|m| m.as_str())?.trim().to_lowercase();
        if mode.is_empty() {
            return None;
        }
        let path = |key: &str| {
            v.get(key)
                .and_then(|s| s.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
        };
        Some(Self { mode, root: path("root"), own: path("own") })
    }
}

/// The runtime tier and root.
/// Does not store config (once stored a `cfg`): the tier's criterion is `picked` in the state, not config ——...
pub struct Workplace {
    /// `std::sync::Mutex`: only read/written in synchronous context, **never held across an
    /// `await`** (the check runs on the tool-call path, and that future must stay `Send`).
    state: std::sync::Mutex<State>,
}

#[derive(Debug)]
struct State {
    active: Active,
    /// Whether this conversation has "started" (= has already decided a root).
    started: bool,
    /// This conversation's own root under the "default" tier; `None` = not yet established.
    own: Option<PathBuf>,
    /// Is the root in `active` one the user explicitly picked (`fixed`, or a runtime tier switch), or one handed out...
    picked: bool,
}

impl Workplace {
    /// Construct from config. An unrecognized `mode` errors at startup, it does not silently fall back to `full` ——...
    pub fn new(cfg: WorkplaceConfig, app_root: &Path) -> anyhow::Result<Self> {
        let mode = cfg.mode.trim().to_ascii_lowercase();
        let active = match mode.as_str() {
            "full" => Active::Full,
            "default" => Active::Pending,
            "fixed" => {
                let raw = cfg.root.as_deref().unwrap_or("").trim();
                if raw.is_empty() {
                    anyhow::bail!(
                        t("[workplace] mode = \"fixed\" also needs root = \"<dir>\"\
                         (for the confined default tier —— the app root —— use mode = \"default\")")
                    );
                }
                let p = PathBuf::from(raw);
                let p = if p.is_absolute() { p } else { app_root.join(p) };
                // A fixed workspace is built right now: the user named a plot in config, so that plot should exist.
                let p = normalize(&p);
                std::fs::create_dir_all(&p)
                    .map_err(|e| anyhow::anyhow!("{}", tf!("[workplace] failed to create root = {}: {}", p.display(), e)))?;
                Active::At(p)
            }
            other => anyhow::bail!(tf!(
                "{} is not a recognised [workplace] mode. Valid values: \"default\" (confined to the app root, the default), \
                 \"full\" (unrestricted), \"fixed\" (together with root: a fixed directory)",
                format!("{other:?}")
            )),
        };
        Ok(Self {
            state: std::sync::Mutex::new(State {
                active,
                started: matches!(mode.as_str(), "full" | "fixed"),
                // At startup there is no "self-built plot for this conversation" yet —— the
                // conversation itself hasn't even started.
                own: None,
                // Only the `fixed` tier is "a plot the user explicitly picked"; `full` has no root,
                // and the `default` tier's plot is built on the fly.
                picked: mode == "fixed",
            }),
        })
    }

    /// The currently effective root; `None` = unrestricted
    pub fn active_root(&self) -> Option<PathBuf> {
        match &self.lock().active {
            Active::At(p) => Some(p.clone()),
            _ => None,
        }
    }

    /// Snapshot for the UI: lock once while grabbing both the "tier" and the "currently effective root".
    pub fn snapshot(&self) -> (Mode, Option<PathBuf>) {
        Self::snapshot_of(&self.lock())
    }

    /// The **single** source of the "tier + root" mapping. Both `snapshot` and `persisted` read
    /// it —— so they can't drift apart later (one says "default", the other stores `fixed`).
    fn snapshot_of(st: &State) -> (Mode, Option<PathBuf>) {
        let mode = match &st.active {
            Active::Full => Mode::Full,
            Active::Pending => Mode::Default,
            // ️ The criterion is `picked` in the state, not `cfg.mode` in config.
            Active::At(_) if !st.picked => Mode::Default,
            Active::At(p) => Mode::Fixed(p.clone()),
        };
        let root = match &st.active {
            Active::At(p) => Some(p.clone()),
            _ => None,
        };
        (mode, root)
    }

    /// The current tier. Equivalent to `self.snapshot().0`.
    pub fn mode(&self) -> Mode {
        self.snapshot().0
    }

    /// Compress the current state into a persistable form (see Record).
    pub fn persisted(&self) -> Record {
        let st = self.lock();
        let (mode, root) = Self::snapshot_of(&st);
        let mode = match mode {
            Mode::Default => "default",
            Mode::Fixed(_) => "fixed",
            Mode::Full => "full",
        };
        Record { mode: mode.to_string(), root, own: st.own.clone() }
    }

    /// Restore the workspace from a conversation record (used when opening an old conversation).
    pub fn restore(&self, rec: &Record) {
        let mut st = self.lock();
        st.own = rec.own.clone();
        match (rec.mode.as_str(), rec.root.clone()) {
            ("full", _) => {
                st.active = Active::Full;
                st.started = true;
                st.picked = false; // no root, so "explicitly picked" doesn't apply
            }
            // A plot the user named back then ⇒ the UI reports "choose workspace".
            ("fixed", Some(p)) => {
                st.active = Active::At(normalize(&p));
                st.started = true;
                st.picked = true;
            }
            // A `default`-tier plot: reuse the saved root, but the tier is still "default".
            ("default", Some(p)) => {
                let p = normalize(&p);
                st.active = Active::At(p.clone());
                st.started = true;
                st.picked = false;
                // Old-format records have no `own` field: `root` is that self-built plot, use it
                // as the fallback.
                if st.own.is_none() {
                    st.own = Some(p);
                }
            }
            // `default` gave no root, or the tier name is unrecognized: back to "not yet
            // decided", fixed to the app root on the first message.
            _ => {
                st.active = Active::Pending;
                st.started = false;
                st.picked = false;
            }
        }
    }

    /// Start a new conversation: always return to the "default" tier, and clear "started".
    pub fn reset_for_new_conversation(&self) {
        let mut st = self.lock();
        st.started = false;
        st.active = Active::Pending;
        st.picked = false;
        // A new conversation starts over: clear "this conversation's own root" too, so a later
        // `ensure_started` re-decides it from scratch instead of reusing a stale one.
        st.own = None;
    }

    /// Ensure the root is decided before this conversation starts.
    pub fn ensure_started(&self, app_root: &Path) -> anyhow::Result<Option<PathBuf>> {
        let mut st = self.lock();
        if st.started {
            return Ok(None);
        }
        // `Pending`: the only branch that does real work; pulled out on its own —— so we don't write `st.active`...
        if matches!(st.active, Active::Pending) {
            // The default workspace is the app root: no per-conversation `<timestamp>/` plot any more.
            let dir = normalize(app_root);
            std::fs::create_dir_all(&dir)
                .map_err(|e| anyhow::anyhow!("{}", tf!("failed to create the workspace ({}): {}", dir.display(), e)))?;
            // Best-effort: if this fails the model can still create the directory itself
            // (`write_file` creates missing parents), so it must not fail the turn.
            let _ = std::fs::create_dir_all(dir.join(WORKPLACE_DIR));
            for sub in [TMP_DIR, MEMORY_DIR] {
                let _ = std::fs::create_dir_all(dir.join(AOX_DIR).join(sub));
            }
            st.active = Active::At(dir.clone());
            // A root handed out by the `default` tier ⇒ reports "default" not "choose workspace"
            // (criterion is `picked`, see `State`).
            st.picked = false;
            // Record it as "this conversation's own root": afterwards, no matter which tier you
            // switch to and back to default, it's this same root.
            st.own = Some(dir.clone());
            st.started = true;
            return Ok(Some(dir));
        }
        // `Full` / `At`: first time this conversation decides its root, just record it.
        let root = match &st.active {
            Active::At(p) => Some(p.clone()),
            _ => None,
        };
        st.started = true;
        Ok(root)
    }

    /// Switch tier at runtime (CLI's `/workplace`, GUI's button). Returns a message for the
    /// user.
    pub fn set_mode(&self, mode: Mode, app_root: &Path) -> anyhow::Result<String> {
        // When switching to "choose workspace", build immediately: the user explicitly picked a plot, so that plot should...
        let dir = match mode {
            Mode::Fixed(ref p) => {
                // A relative path is resolved against the **app root** (same set as the `fixed`
                // tier in `new()`).
                let p = if p.is_absolute() { p.clone() } else { app_root.join(p) };
                Some(normalize(&p))
            }
            _ => None,
        };
        if let Some(d) = dir.as_ref() {
            std::fs::create_dir_all(d)
                .map_err(|e| anyhow::anyhow!("{}", tf!("directory {} is unusable: {}", d.display(), e)))?;
            if !d.is_dir() {
                anyhow::bail!("{}", tf!("{} is not a directory", d.display()));
            }
            // First use of a user-picked workspace: lay down the same `.aox/` layout the `default` tier sets up...
            for sub in [TMP_DIR, MEMORY_DIR] {
                let _ = std::fs::create_dir_all(d.join(AOX_DIR).join(sub));
            }
        }
        let mut st = self.lock();
        match mode {
            Mode::Full => {
                st.active = Active::Full;
                st.started = true;
                st.picked = false; // no root, so "explicitly picked" doesn't apply
                Ok(t("Switched to \"Full access\": AOX can read and write any file on the machine (including outside the workspace).").into())
            }
            Mode::Fixed(_) => {
                let d = dir.expect("computed above just now");
                st.active = Active::At(d.clone());
                st.started = true;
                st.picked = true; // a plot the user named ⇒ the UI reports "choose workspace"
                Ok(tf!("Switched to \"Picked workspace\": {}", d.display()))
            }
            Mode::Default => {
                st.picked = false;
                let _ = app_root;
                // This conversation has already decided its "default" root ⇒ go back to that one, don't decide another.
                match st.own.clone() {
                    Some(p) => {
                        st.active = Active::At(p.clone());
                        st.started = true;
                        Ok(tf!("Switched to \"Default\": this conversation stays in {}.", p.display()))
                    }
                    None => {
                        st.active = Active::Pending;
                        st.started = false;
                        Ok(t("Switched to \"Default\": the next message will work in the app root.")
                            .into())
                    }
                }
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // The check happens on the tool-call path; the lock is held for one path comparison —— it can't be poisoned.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the current state is unrestricted (`full` tier = everything allowed).
    pub fn is_unrestricted(&self) -> bool {
        matches!(self.lock().active, Active::Full)
    }

    /// Judge a write-to-disk path (write / delete). `Ok` = the normalized absolute path, ready to hand to the file API.
    pub fn check(&self, raw: &str) -> Result<PathBuf, Reject> {
        self.check_access(raw, Access::Write)
    }

    /// Judge a read-only path (read file / list directory).
    pub fn check_read(&self, raw: &str) -> Result<PathBuf, Reject> {
        self.check_access(raw, Access::Read)
    }

    /// The single implementation for both intents.
    pub fn check_access(&self, raw: &str, access: Access) -> Result<PathBuf, Reject> {
        if access == Access::Read {
            return Ok(normalize(Path::new(raw)));
        }
        let root = match &self.lock().active {
            Active::Full => return Ok(normalize(Path::new(raw))),
            Active::Pending => return Err(Reject::NotStarted),
            Active::At(r) => r.clone(),
        };
        let p = Path::new(raw);
        if !p.is_absolute() {
            // See module doc "① absolute paths only".
            return Err(Reject::Relative { root, raw: raw.to_string() });
        }
        let resolved = normalize(p);
        if !inside(&root, &resolved) {
            return Err(Reject::Outside { root, raw: raw.to_string(), resolved });
        }
        Ok(resolved)
    }

    /// The body of `/workplace`
    pub fn report(&self, app_root: &Path) -> String {
        // ️ The order is deliberately reversed; don't casually "optimize" it into "lock first, then compute the tier...
        let label = self.mode().label();
        let active = self.lock().active.clone();
        let mut out = String::from(t("Workspace\n\n"));
        match &active {
            Active::Full => {
                out.push_str(t(
                    "  Tier: Full access\n  \
                     AOX can read and write any file on the machine, with no boundary.\n\n  \
                     This was chosen **explicitly** (mode = \"full\" in config.toml, or switched in the UI) — \
                     the config default is the \"Default\" tier (confined).\n  To tighten: /workplace default or /workplace <dir>\n",
                ));
            }
            Active::Pending => {
                out.push_str(&tf!(
                    "  Tier: Default (confined)\n  \
                     Root: not decided yet — sending the next message will work in the app root ({}).\n",
                    app_root.display()
                ));
            }
            Active::At(p) => {
                out.push_str(&tf!(
                    "  Tier: {} (confined)\n  Root: {}\n  \
                     Writes and deletes are confined to this directory; **reads are unrestricted** (the whole machine is readable).\n",
                    label,
                    p.display()
                ));
            }
        }
        out.push_str(t(
            "\n  ⚠️ The boundary only governs \"host-provided capabilities\": the host's built-in file tools, and the path arguments of plugin tools.\n     \
             it cannot stop a misbehaving plugin from open()ing files by itself — that needs OS-level isolation (AppContainer).\n",
        ));
        out
    }
}

// The /workplace command

/// The command's two names: the first is the canonical name, the rest are legacy aliases.
const CMD_NAMES: [&str; 2] = ["/workplace", "/workspace"];

/// The predicate for the `/workplace` command family (used by the CLI router, see the `_ if...::is_cmd(line)`...
pub fn is_cmd(line: &str) -> bool {
    cmd_arg(line).is_some()
}

/// Strip the argument after the command; `None` = this line is not the command at all.
fn cmd_arg(line: &str) -> Option<&str> {
    for name in CMD_NAMES {
        if line == name {
            return Some("");
        }
        if let Some(rest) = line.strip_prefix(name) {
            // Only accept the form "command name + space", to avoid `/workplaceX` being treated
            // as `/workplace X`.
            if let Some(arg) = rest.strip_prefix(' ') {
                return Some(arg.trim());
            }
        }
    }
    None
}

/// Execute the `/workplace` command.
pub fn command(rt: &std::sync::Arc<crate::runtime::Runtime>, line: &str) -> anyhow::Result<String> {
    // `is_cmd` already guarantees this line is the command, so `None` won't happen; if it
    // somehow does, treat it as "no argument".
    let arg = cmd_arg(line).unwrap_or("");
    if arg.is_empty() {
        return Ok(rt.workplace.report(&rt.root));
    }
    let mode = if arg.eq_ignore_ascii_case("full") {
        Mode::Full
    } else if arg.eq_ignore_ascii_case("default") {
        Mode::Default
    } else {
        Mode::Fixed(PathBuf::from(arg))
    };
    rt.workplace.set_mode(mode, &rt.root)
}

// The out-of-bounds contract

/// A single out-of-bounds event.
#[derive(Debug, Clone)]
pub enum Reject {
    /// A relative path was given under the confined tier.
    Relative { root: PathBuf, raw: String },
    /// The path falls outside the workspace.
    Outside { root: PathBuf, raw: String, resolved: PathBuf },
    /// Confined tier, but this conversation hasn't started yet (root not decided).
    NotStarted,
}

impl Reject {
    /// Text shown to the model. Its three parts are hard requirements.
    pub fn model_text(&self) -> String {
        // Every branch must **carry the root itself**: this text is all the model gets, and it has
        // nowhere else to look it up.
        let what = match self {
            Reject::Relative { root, raw } => tf!(
                "You passed a relative path {} — this tier does not accept relative paths.\
                 Rewrite it as an **absolute path inside the workspace**; the workspace root is: {}.\
                 Why: the host and the plugin subprocess do not share a working directory, so the same relative path resolves to\
                 two different files on the two sides — that inconsistency is far more dangerous than a few extra characters.",
                format!("{raw:?}"),
                root.display()
            ),
            Reject::Outside { root, raw, resolved } => tf!(
                "{} resolves to {}, which lies outside the workspace (workspace root: {}).",
                format!("{raw:?}"),
                resolved.display(),
                root.display()
            ),
            Reject::NotStarted => {
                t("The workspace for this conversation is not decided yet in this tier (it is fixed when the user sends the first message),\
                 so nothing can be written into it yet.")
                    .to_string()
            }
        };
        // ️ What follows is operating instructions for the model, and it will be relayed to the user verbatim ——...
        tf!(
            "[Permission boundary] {}\n\n\
             This is not a broken tool or a wrong argument — it is the current workspace's permission boundary.\
             **Do not retry this call, and do not try to sneak around it another way** (the same out-of-bounds target will be refused no matter how you phrase it).\n\n\
             Please **relay this to the user**: the current workspace only allows **writes and deletes** inside the boundary,\
             so you cannot modify that location. Note: **reading files is not restricted by the workspace** — \
             if you only need to read something outside the boundary, just read it; no escalation needed.\
             If the user really needs to write or delete there, tell them the permission can be raised like this:\n  \
             - UI: click \"Workspace\" at the bottom-left of the input box, choose \"Default / Picked workspace / Full access\"\n  \
             - CLI: `/workplace default` (confined to the app root), `/workplace <dir>` (a fixed directory)\
             or `/workplace full` (full access — AOX can read and write any file on the machine)\n  \
             - or edit `[workplace]` in config.toml (mode = \"full\" / \"default\" / \"fixed\") and restart\n\n\
             Until the user changes the permission, fall back to what is possible inside the workspace for the rest,\
             or tell the user clearly that this step cannot be done and what is missing.",
            what
        )
    }

    /// The one line for the user (the caller sends it with a `Warn` event).
    pub fn user_text(&self) -> String {
        match self {
            Reject::Relative { root, raw } => tf!(
                "Blocked an out-of-bounds access: relative path {} (workspace root {}).\
                 This tier only accepts absolute paths inside the workspace.",
                format!("{raw:?}"),
                root.display()
            ),
            Reject::Outside { root, raw, resolved } => tf!(
                "Blocked an out-of-bounds access: {} → {}, workspace is {}.\
                 The call was not executed. (To loosen: `/workplace default|<dir>|full`,\
                 or edit [workplace] in config.toml)",
                format!("{raw:?}"),
                resolved.display(),
                root.display()
            ),
            Reject::NotStarted => {
                t("This conversation's workspace isn't established yet; this write/delete was blocked — send a message and it will work.").to_string()
            }
        }
    }
}

// Argument scanning (the coarse net)

/// Pick the "path-like" strings out of one call's arguments, returning `(parameter name, value)`.
pub fn scan_paths(args: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk(args, "", &mut out);
    out
}

fn walk(v: &Value, prefix: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                match val {
                    Value::String(s) => {
                        if looks_like_path(k, s) {
                            out.push((key, s.clone()));
                        }
                    }
                    // Arrays of strings: a form like `paths = ["a", "b"]` is very common, so judge each item.
                    Value::Array(items) => {
                        for item in items {
                            if let Value::String(s) = item {
                                if looks_like_path(k, s) {
                                    out.push((key.clone(), s.clone()));
                                }
                            } else {
                                walk(item, &key, out);
                            }
                        }
                    }
                    _ => walk(val, &key, out),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, prefix, out);
            }
        }
        _ => {}
    }
}

/// Once a value exceeds this length, or contains a newline, it is no longer treated as a path (see...
const MAX_PATH_CHARS: usize = 260;

fn looks_like_path(key: &str, val: &str) -> bool {
    if val.trim().is_empty() {
        return false;
    }
    // The parameter name looks like a path —— the most reliable rule.
    if PATH_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
        return true;
    }
    // When the key name says nothing, we can only guess from the shape of the value —— and that starts by ruling out...
    if val.contains('\n') || val.contains('\r') || val.chars().count() > MAX_PATH_CHARS {
        return false;
    }
    // ③ The value itself is obviously an absolute path (`/x`, `\x`, `X:\x`, `\\server\share`, `~/x`)
    let b = val.as_bytes();
    if b[0] == b'/' || b[0] == b'\\' || val.starts_with("~/") {
        return true;
    }
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return true;
    }
    // ③ Contains a `..` component —— that token appears almost only in paths, and it is precisely
    //    the common way to escape the boundary
    val.split(['/', '\\']).any(|seg| seg == "..")
}

// Path normalization and containment

/// Normalize: resolve symlinks / 8.3 short names / case, and cancel `.` and cancellable `..`.
fn normalize(p: &Path) -> PathBuf {
    let lex = lexical(p);
    let mut tail: Vec<OsString> = Vec::new();
    let mut cur = lex.clone();
    loop {
        if let Ok(c) = cur.canonicalize() {
            // On Windows `canonicalize` returns a verbatim path (`\\?\C:\…`) —— that is the one after "resolving symlinks /...
            let mut out = strip_verbatim(c);
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return lexical(&out);
        }
        let Some(name) = cur.file_name().map(|s| s.to_os_string()) else { break };
        tail.push(name);
        match cur.parent() {
            // `parent()` returning empty / equal to itself ⇒ we've reached the top (a drive root,
            // or the start of a relative path)
            Some(parent) if !parent.as_os_str().is_empty() && parent != cur => {
                cur = parent.to_path_buf();
            }
            _ => break,
        }
    }
    lex
}

/// Called only once, at normalize's exit.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

/// Purely lexical normalization: cancel only `.` and cancellable `..`, **without touching the
/// filesystem** (and without resolving symlinks).
fn lexical(p: &Path) -> PathBuf {
    let mut prefix = PathBuf::new();
    let mut stack: Vec<OsString> = Vec::new();
    for c in p.components() {
        match c {
            // A drive letter and the root land at the head of the result, and cannot be popped by `..` —— otherwise...
            Component::Prefix(_) | Component::RootDir => prefix.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.pop().is_none() {
                    stack.push("..".into());
                }
            }
            Component::Normal(s) => stack.push(s.to_os_string()),
        }
    }
    let mut out = prefix;
    for s in stack {
        out.push(s);
    }
    out
}

/// Is `p` inside `root` (including being equal to root)?
fn inside(root: &Path, p: &Path) -> bool {
    let mut r = root.components();
    let mut q = p.components();
    loop {
        match (r.next(), q.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(a), Some(b)) => {
                let (a, b) = (a.as_os_str().to_string_lossy(), b.as_os_str().to_string_lossy());
                let same = if cfg!(windows) { a.eq_ignore_ascii_case(&b) } else { a == b };
                if !same {
                    return false;
                }
            }
        }
    }
}

// ─────────────────────────── (no timestamped plots any more) ─────────────────────────── Until 2026-10-08...

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkplaceConfig;

    fn ws(mode: &str, root: Option<&str>) -> Workplace {
        let app = std::env::temp_dir().join("aox-ws-test");
        Workplace::new(WorkplaceConfig { mode: mode.into(), root: root.map(str::to_string) }, &app)
            .expect("test config should be valid")
    }

    fn fixed(dir: &str) -> Workplace {
        ws("fixed", Some(dir))
    }

    /// The command's two names: `/workplace` is the canonical name, `/workspace` is the legacy name.
    #[test]
    fn command_answers_to_both_names_and_parses_args() {
        for name in ["/workplace", "/workspace"] {
            assert!(is_cmd(name), "{name} should be recognised as the command");
            assert_eq!(cmd_arg(name), Some(""), "{name} with no argument should yield an empty arg");
            assert_eq!(cmd_arg(&format!("{name} default")), Some("default"));
            assert_eq!(cmd_arg(&format!("{name}   C:\\x ")), Some("C:\\x"), "extra spaces must be eaten");
        }
        assert_eq!(cmd_arg("/workplace /workplace"), Some("/workplace"), "a dir named like the command must strip exactly one");
        assert_eq!(cmd_arg("/workspace /workspace"), Some("/workspace"));
        for other in ["/workplaceX", "/workplaces", "/work", "workplace", ""] {
            assert!(!is_cmd(other), "{other:?} must not be taken for this command");
        }
    }

    #[test]
    fn unknown_mode_is_an_error() {
        let app = std::env::temp_dir();
        let r = Workplace::new(WorkplaceConfig { mode: "sandbox".into(), root: None }, &app);
        assert!(r.is_err(), "an unknown mode must fail at startup, not silently fall back to full");
    }

    #[test]
    fn fixed_without_root_is_an_error() {
        let app = std::env::temp_dir();
        let r = Workplace::new(WorkplaceConfig { mode: "fixed".into(), root: None }, &app);
        assert!(r.is_err(), "fixed without root must error");
    }

    #[test]
    fn full_lets_everything_through() {
        let w = ws("full", None);
        assert!(w.check("C:\\Windows\\System32").is_ok());
        assert!(w.check("relative.txt").is_ok(), "the unrestricted tier must not nitpick relative paths");
        assert_eq!(w.active_root(), None);
    }

    /// The confined tier: **relative paths are always rejected**, and the text must give the root
    /// (the model relies on it to rewrite).
    #[test]
    fn confined_rejects_relative_and_tells_the_root() {
        let base = std::env::temp_dir().join("aox-ws-rel");
        std::fs::create_dir_all(&base).unwrap();
        let w = fixed(&base.to_string_lossy());
        let e = w.check("config.toml").expect_err("a relative path must be rejected");
        let t = e.model_text();
        assert!(t.contains("relative path"), "{t}");
        assert!(t.contains(&normalize(&base).display().to_string()), "{t}");
        assert!(t.contains("relay this to the user"), "the contract must tell the model to relay: {t}");
        assert!(t.contains("Do not retry"), "the contract must say don't retry: {t}");
    }

    #[test]
    fn confined_allows_inside_and_rejects_outside() {
        let base = std::env::temp_dir().join("aox-ws-in");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        let w = fixed(&base.to_string_lossy());

        let inside = base.join("sub").join("a.txt");
        assert!(w.check(&inside.to_string_lossy()).is_ok(), "an absolute path inside the workspace must pass");

        let outside = base.parent().unwrap().join("aox-ws-outside.txt");
        let e = w.check(&outside.to_string_lossy()).expect_err("outside the workspace must be rejected");
        match e {
            Reject::Outside { .. } => {}
            other => panic!("expected Outside, got {other:?}"),
        }
    }

    /// `..` escape: lexically it looks inside, but after normalization it must land outside.
    #[test]
    fn dotdot_escape_is_caught() {
        let base = std::env::temp_dir().join("aox-ws-dotdot");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        let w = fixed(&base.to_string_lossy());

        let escape = base.join("sub").join("..").join("..").join("evil.txt");
        let e = w.check(&escape.to_string_lossy()).expect_err("a `..` escape must be rejected");
        assert!(matches!(e, Reject::Outside { .. }), "{e:?}");

        // A `..` that doesn't escape must be allowed (don't reject `sub/../ok.txt` along with it)
        let ok = base.join("sub").join("..").join("ok.txt");
        assert!(w.check(&ok.to_string_lossy()).is_ok(), "a `..` staying inside the workspace must pass");
    }

    /// Case-insensitive (Windows semantics): a differing case in the path prefix is not
    /// out-of-bounds.
    #[test]
    fn prefix_compare_ignores_ascii_case() {
        if !cfg!(windows) {
            return;
        }
        let base = std::env::temp_dir().join("AOX-WS-CASE");
        std::fs::create_dir_all(&base).unwrap();
        let w = fixed(&base.to_string_lossy());
        let shouty = base.join("A.TXT");
        let lower = PathBuf::from(shouty.to_string_lossy().to_ascii_lowercase());
        assert!(w.check(&lower.to_string_lossy()).is_ok(), "a case difference must not count as out-of-bounds");
    }

    /// The `default` tier: **everything is rejected before the root is decided**, and it must not
    /// be silently allowed just because "it isn't built yet".
    #[test]
    fn pending_is_rejected_not_silently_allowed() {
        let app = std::env::temp_dir().join("aox-ws-pending");
        // Pre-create the app root: in production it always exists, and this makes `normalize`
        // below canonicalize on both sides instead of one side falling back to `lexical`.
        std::fs::create_dir_all(&app).unwrap();
        let w = ws("default", None);
        let e = w.check("C:\\Windows").expect_err("with the root undecided it must reject, not pass");
        assert!(matches!(e, Reject::NotStarted), "{e:?}");

        let dir = w.ensure_started(&app).unwrap().expect("the default root should be established");
        assert!(dir.is_dir(), "the directory should really exist");
        // The default workspace **is the app root** (changed 2026-10-08) — not a `<timestamp>/`
        // plot under `workplace/`.
        assert_eq!(dir, normalize(&app), "the default root must be the app root itself");
        // The conventional subdirs are made up front.
        assert!(app.join(WORKPLACE_DIR).is_dir(), "workplace/ should be created");
        // …and so is AOX's own per-workspace state, under one hidden folder (2026-10-14):
        // scratch files, conversations, and the AI's notes.
        assert!(app.join(AOX_DIR).join(TMP_DIR).is_dir(), ".aox/tmp/ should be created");
        assert!(app.join(AOX_DIR).join(MEMORY_DIR).is_dir(), ".aox/memory/ should be created");
        // Conversations are store-wide, so a workspace must **not** grow one of its own.
        assert!(!app.join(AOX_DIR).join("history").exists(), "no per-workspace history any more");
        assert!(w.check(&dir.join("x.txt").to_string_lossy()).is_ok());
        // A conversation establishes its root only once
        assert!(w.ensure_started(&app).unwrap().is_none(), "the same conversation must not re-establish the root");
        // A new conversation goes back to the default tier — same app root again.
        w.reset_for_new_conversation();
        let dir2 = w.ensure_started(&app).unwrap().unwrap();
        assert_eq!(dir, dir2, "every conversation's default root is the app root");
        let _ = std::fs::remove_dir_all(&app);
    }

    /// The AI's own notes folder is laid down when a **user-picked** workspace is first used (the
    /// `set_mode(Fixed)` path —— `/workplace <dir>` and the GUI picker), matching the `default` tier.
    #[test]
    fn picking_a_workspace_lays_down_its_notes_folder() {
        let app = std::env::temp_dir().join("aox-ws-pick-notes");
        std::fs::create_dir_all(&app).unwrap();
        let w = ws("default", None);

        // A relative pick resolves under the app root (same rule as the `fixed` tier in `new()`).
        w.set_mode(Mode::Fixed(PathBuf::from("picked-here")), &app).unwrap();
        assert!(
            app.join("picked-here").join(AOX_DIR).join(MEMORY_DIR).is_dir(),
            ".aox/memory/ should be created on a relative pick"
        );
        // A picked workspace gets the same whole layout, not just the notes folder — otherwise
        // history would land there (the backend creates it) but scratch files would have nowhere to go.
        assert!(
            app.join("picked-here").join(AOX_DIR).join(TMP_DIR).is_dir(),
            ".aox/tmp/ should be created on a relative pick"
        );

        // An absolute pick works the same way.
        let abs = app.join("abs-pick");
        std::fs::create_dir_all(&abs).unwrap();
        w.set_mode(Mode::Fixed(abs.clone()), &app).unwrap();
        assert!(
            abs.join(AOX_DIR).join(MEMORY_DIR).is_dir(),
            ".aox/memory/ should be created under an absolute pick"
        );
        let _ = std::fs::remove_dir_all(&app);
    }

    #[test]
    fn scan_paths_picks_path_like_values_only() {
        let args = serde_json::json!({
            "path": "config.toml",
            "text": "this has a / slash but is not a path",
            "pattern": "*.rs",
            "abs": "/etc/passwd",
            "win": "D:\\data\\x.csv",
            "escape": "sub/../../etc/passwd",
            "nested": {"dir": "plugins"},
            "paths": ["a.txt", "b/c.txt"],
            "url": "https://example.com/x",
            "n": 3
        });
        let hits: Vec<String> = scan_paths(&args).into_iter().map(|(_, v)| v).collect();
        assert!(hits.contains(&"config.toml".to_string()), "the path argument must be caught");
        assert!(hits.contains(&"/etc/passwd".to_string()), "an absolute path must be caught");
        assert!(hits.contains(&"D:\\data\\x.csv".to_string()), "a Windows absolute path must be caught");
        assert!(hits.contains(&"plugins".to_string()), "a nested dir must be caught");
        // A relative name inside an array is caught by the key name (`paths`) —— if the key name weren't on the list...
        assert!(hits.contains(&"a.txt".to_string()), "items in a paths array must be caught");
        assert!(hits.contains(&"b/c.txt".to_string()), "same as above");
        assert!(!hits.contains(&"*.rs".to_string()), "a glob is not a path");
        assert!(!hits.contains(&"3".to_string()));
        // Contains `/` but isn't a path: a URL must never be caught —— a misfire would leave a
        // plugin unable to even fetch data under the confined tier
        assert!(!hits.contains(&"https://example.com/x".to_string()), "a URL is not a file path");
        // The key name doesn't look like a path and the value carries spaces and prose ⇒ not
        // judged (the coarse net would rather miss than misfire)
        assert!(!hits.iter().any(|h| h.contains("slash")), "prose must not be taken for a path");
    }

    /// The user-side text must make clear "what it was trying to do, and what blocked it", rather
    /// than copying the model's copy of it.
    #[test]
    fn user_text_differs_from_model_text() {
        let w = fixed(&std::env::temp_dir().to_string_lossy());
        let e = w.check("relative.txt").unwrap_err();
        let (m, u) = (e.model_text(), e.user_text());
        assert!(u.contains("Blocked an out-of-bounds access"), "{u}");
        assert!(!u.contains("Do not retry"), "the user-facing line must not carry model-speak like don't-retry");
        assert!(m.len() > u.len(), "the model contract should be longer and more complete than the user line");
    }

    /// Regression: `report()` once called `self.mode()` while holding the state lock (it locks...
    #[test]
    fn report_after_switching_does_not_deadlock() {
        let base = std::env::temp_dir().join("aox-ws-report");
        std::fs::create_dir_all(&base).unwrap();
        let w = fixed(&base.to_string_lossy());
        let app = std::env::temp_dir();

        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = tx.send(w.report(&app));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(text) => {
                handle.join().unwrap();
                assert!(text.contains("Picked workspace"), "the fixed tier must report Picked workspace: {text}");
                assert!(
                    text.contains(&normalize(&base).display().to_string()),
                    "the body should contain the root: {text}"
                );
            }
            Err(_) => panic!("report() did not return within 5s — the state lock was reentered (deadlock)"),
        }

        // After the `default` tier decides its root, **the tier is still "default"** —— it must not
        // be called "choose workspace" just because the root was built on the fly
        let app2 = std::env::temp_dir().join("aox-ws-report-default");
        let w2 = ws("default", None);
        w2.ensure_started(&app2).unwrap().expect("the default root should be established");
        let text = w2.report(&app2);
        assert!(text.contains("Default"), "{text}");
        assert!(!text.contains("Picked workspace"), "the default tier must not be reported as Picked workspace: {text}");
        let _ = std::fs::remove_dir_all(&app2);
    }

    /// Regression: a whole block of source code must not be taken for a path.
    #[test]
    fn multiline_source_is_not_treated_as_a_path() {
        let src = "import os\nsys.path.insert(0, os.path.join(d, \"..\"))\n# a/b/c\n".repeat(30);
        let args = serde_json::json!({
            "name": "demo",
            "source": src,
            "code": "x = a/b",
        });
        let hits = scan_paths(&args);
        assert!(
            !hits.iter().any(|(_, v)| v.contains("import os")),
            "the whole source was taken for a path: {:?}",
            hits.iter().map(|(k, v)| (k.clone(), v.len())).collect::<Vec<_>>()
        );
        assert!(hits.is_empty(), "neither of these two arguments should be caught: {hits:?}");

        // But the key name is authoritative: the same value hung under the `path` key is judged however long it is (it...
        let long_path = format!("C:\\{}", "x".repeat(300));
        let hits = scan_paths(&serde_json::json!({ "path": long_path }));
        assert_eq!(hits.len(), 1, "a long value under the path key is a path; length must not excuse it");
    }

    /// `is_unrestricted()` is the only basis for the check point's short-circuit (under the `full` tier the whole...
    #[test]
    fn is_unrestricted_agrees_with_check() {
        let full = ws("full", None);
        assert!(full.is_unrestricted(), "the full tier must report unrestricted");
        for p in ["relative.txt", "C:\\Windows\\System32", "/etc/passwd", "a/../../b"] {
            assert!(full.check(p).is_ok(), "the full tier must pass {p:?}");
        }

        let base = std::env::temp_dir().join("aox-ws-unrestricted");
        std::fs::create_dir_all(&base).unwrap();
        let w = fixed(&base.to_string_lossy());
        assert!(!w.is_unrestricted(), "a confined tier must not report unrestricted — otherwise the check is skipped entirely");
        w.set_mode(Mode::Full, &base).unwrap();
        assert!(w.is_unrestricted(), "after switching to full at runtime it must short-circuit too");

        // `Pending` (default tier, no message sent yet) **rejects everything**, and must never be
        // reported as unrestricted.
        assert!(!ws("default", None).is_unrestricted(), "Pending rejects everything and must never report unrestricted");
    }

    /// The new semantics: reads unrestricted / writes and deletes confined.
    #[test]
    fn read_is_never_restricted_but_write_is() {
        let base = std::env::temp_dir().join("aox-ws-rw");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        let outside = base.parent().unwrap().join("aox-ws-rw-outside.txt");
        let outside = outside.to_string_lossy().into_owned();
        let inside = base.join("sub").join("a.txt").to_string_lossy().into_owned();

        // ① The fixed tier: an out-of-bounds path —— write rejects, read passes; an in-plot path
        //    passes both intents
        let w = fixed(&base.to_string_lossy());
        assert!(
            matches!(w.check(&outside), Err(Reject::Outside { .. })),
            "an out-of-bounds **write** must be rejected"
        );
        assert!(w.check_read(&outside).is_ok(), "an out-of-bounds **read** must pass (the new semantics)");
        assert!(w.check(&inside).is_ok());
        assert!(w.check_read(&inside).is_ok());

        // Relative path: write rejects (the host and plugin have different cwds, and that ambiguity is a risk only writes...
        assert!(matches!(w.check("config.toml"), Err(Reject::Relative { .. })));
        assert!(w.check_read("config.toml").is_ok(), "a read must not be rejected for being relative");

        // ③ `default` with no plot opened yet (Pending): writes all rejected, reads still pass ——
        //    otherwise "nothing can be read before the first message"
        let p = ws("default", None);
        assert!(matches!(p.check("C:\\Windows"), Err(Reject::NotStarted)));
        assert!(p.check_read("C:\\Windows").is_ok(), "reads must not be blocked while the plot is not started yet");

        // ④ The full tier: both intents pass (the read branch and the full write branch are two
        //    separate pieces of code, and both must be pinned)
        let f = ws("full", None);
        assert!(f.check(&outside).is_ok());
        assert!(f.check_read(&outside).is_ok());
    }

    /// `snapshot()` is the UI's only state-fetching entry point: one lock acquisition grabs both the "tier"...
    #[test]
    fn snapshot_agrees_with_mode_and_root() {
        // full: tier Full, root None (not "not yet decided" but "unrestricted")
        let full = ws("full", None);
        assert_eq!(full.snapshot(), (Mode::Full, None));
        assert_eq!(full.snapshot().0, full.mode());

        // default before building: tier Default, root None (**not** unrestricted)
        let app = std::env::temp_dir().join("aox-ws-snapshot");
        let d = ws("default", None);
        assert_eq!(d.snapshot(), (Mode::Default, None));

        // default after establishing: the tier is still Default, but the root now exists
        let dir = d.ensure_started(&app).unwrap().expect("the default root should be established");
        let (m, r) = d.snapshot();
        assert_eq!(m, Mode::Default, "after the default root is established the mode must not become Picked workspace");
        assert_eq!(r.as_deref(), Some(dir.as_path()));
        assert_eq!(d.mode(), m, "snapshot and mode must agree");
        assert_eq!(d.active_root(), r, "snapshot and active_root must agree");
        let _ = std::fs::remove_dir_all(&app);

        // fixed: the tier carries a path, and the root is itself
        let base = std::env::temp_dir().join("aox-ws-snapshot-fixed");
        std::fs::create_dir_all(&base).unwrap();
        let f = fixed(&base.to_string_lossy());
        let (m, r) = f.snapshot();
        assert!(matches!(m, Mode::Fixed(_)), "{m:?}");
        assert_eq!(r, Some(normalize(&base)));
        assert_eq!(f.mode(), m);
        assert_eq!(f.active_root(), r);
    }

    /// The tier's criterion is how it came about at runtime, not what the config says.
    #[test]
    fn snapshot_follows_the_runtime_origin_not_the_config() {
        let base = std::env::temp_dir().join("aox-ws-origin");
        std::fs::create_dir_all(base.join("picked")).unwrap();

        // Config `default` + a fixed plot picked at runtime ⇒ must report "choose workspace"
        let w = ws("default", None);
        let d = base.join("picked");
        w.set_mode(Mode::Fixed(d.clone()), &base).unwrap();
        let (m, r) = w.snapshot();
        assert!(matches!(m, Mode::Fixed(_)), "a runtime-picked plot must report Picked workspace, got {m:?}");
        assert_eq!(r, Some(normalize(&d)));
        assert_eq!(w.persisted().mode, "fixed", "the record must say fixed, otherwise the next open falls back to default");

        // Whereas a root handed out by the `default` tier still reports "default" (picked=false)
        let w2 = ws("default", None);
        let app2 = std::env::temp_dir().join("aox-ws-origin-default");
        w2.ensure_started(&app2).unwrap().expect("the default root should be established");
        assert_eq!(w2.snapshot().0, Mode::Default);
        assert_eq!(w2.persisted().mode, "default");
        let _ = std::fs::remove_dir_all(&app2);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A new conversation always returns to the "default" tier.
    #[test]
    fn new_conversation_always_returns_to_default() {
        let base = std::env::temp_dir().join("aox-ws-newconv");
        std::fs::create_dir_all(base.join("picked")).unwrap();
        let app = std::env::temp_dir().join("aox-ws-newconv-app");
        // The app root exists in production; pre-create it so `normalize` is stable on both sides.
        std::fs::create_dir_all(&app).unwrap();

        // Likewise with the config saying `fixed`: after turning the page it's no longer that fixed
        // plot
        let w = fixed(&base.join("picked").to_string_lossy());
        assert!(matches!(w.snapshot().0, Mode::Fixed(_)));
        w.reset_for_new_conversation();
        assert_eq!(w.snapshot(), (Mode::Default, None), "a new conversation must return to Default with the root undecided");

        // After switching to "full access" and turning the page, likewise back to "default" (not
        // inheriting full)
        w.set_mode(Mode::Full, &base).unwrap();
        assert_eq!(w.snapshot().0, Mode::Full);
        w.reset_for_new_conversation();
        assert_eq!(w.snapshot(), (Mode::Default, None));

        // The "root not decided" semantics must really hold: the next message fixes it to the app
        // root (not a `<timestamp>/` plot).
        let built = w.ensure_started(&app).unwrap().expect("the default root should be established on the spot");
        assert_eq!(built, normalize(&app), "the new conversation's root must be the app root");
        let _ = std::fs::remove_dir_all(&app);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `persisted()` and `restore()` must be inverses of each other —— opening an old conversation is just putting...
    #[test]
    fn persisted_and_restore_round_trip() {
        let base = std::env::temp_dir().join("aox-ws-persist");
        std::fs::create_dir_all(base.join("picked")).unwrap();
        let app = std::env::temp_dir().join("aox-ws-persist-app");
        std::fs::create_dir_all(&app).unwrap(); // the app root exists in production

        // ① default: the root is the app root → the archive has mode=default, and both root
        //    and own are the app root
        let w = ws("default", None);
        let dir = w.ensure_started(&app).unwrap().expect("the default root should be established");
        let saved = w.persisted();
        assert_eq!(saved.mode, "default");
        assert_eq!(saved.root.as_ref(), Some(&normalize(&dir)));
        assert_eq!(saved.own.as_ref(), Some(&normalize(&dir)), "the self-decided root must be recorded too");
        let fresh = ws("full", None); // deliberately start from another tier —— restore must override it
        fresh.restore(&saved);
        assert_eq!(fresh.snapshot(), (Mode::Default, Some(normalize(&dir))));
        assert_eq!(fresh.persisted(), saved, "a round-trip must be identical");

        // ② fixed: a named directory, `own` empty
        let w = fixed(&base.join("picked").to_string_lossy());
        let saved = w.persisted();
        assert_eq!(saved.mode, "fixed");
        assert_eq!(saved.own, None);
        let fresh = ws("full", None);
        fresh.restore(&saved);
        let (m, r) = fresh.snapshot();
        assert!(matches!(m, Mode::Fixed(_)), "{m:?}");
        assert_eq!(r, saved.root);

        // Full: the current tier has no root, but the own root must stay —— switching back to default relies on it.
        let own = normalize(&app.join(WORKPLACE_DIR).join("20260101000000"));
        let saved = Record { mode: "full".into(), root: None, own: Some(own.clone()) };
        let fresh = fixed(&base.join("picked").to_string_lossy());
        fresh.restore(&saved);
        assert_eq!(fresh.snapshot(), (Mode::Full, None));
        assert_eq!(fresh.persisted().own.as_ref(), Some(&own), "even under full the own root must be kept");
        fresh.set_mode(Mode::Default, &base).unwrap();
        assert_eq!(fresh.snapshot(), (Mode::Default, Some(own)), "switching back to default must return to the recorded root");

        // ④ An unrecognized tier name (or `default` with no root) ⇒ back to "not yet decided",
        //    letting the first message decide
        let fresh = ws("full", None);
        fresh.restore(&Record { mode: "nonsense".into(), root: None, own: None });
        assert_eq!(fresh.snapshot(), (Mode::Default, None));

        let _ = std::fs::remove_dir_all(&app);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Switching away and back to "default" must return to this conversation's original root, not pick a different one.
    #[test]
    fn switching_back_to_default_reuses_the_conversation_own_root() {
        let app = std::env::temp_dir().join("aox-ws-own");
        let base = std::env::temp_dir().join("aox-ws-own-base");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&app).unwrap(); // the app root exists in production

        let w = ws("default", None);
        let first = w.ensure_started(&app).unwrap().expect("the first message should establish the root");

        // Switch back and forth a few times: each time it must be the same root, and no new
        // directory is built
        for _ in 0..3 {
            w.set_mode(Mode::Full, &base).unwrap();
            assert_eq!(w.snapshot(), (Mode::Full, None));
            let msg = w.set_mode(Mode::Default, &base).unwrap();
            assert_eq!(
                w.snapshot(),
                (Mode::Default, Some(normalize(&first))),
                "switching back to default must return to this conversation's root"
            );
            assert!(msg.contains("stays in"), "the message should say it stays in the same root, got: {msg}");
            // The first-message step must not run again (`started` is still true ⇒ returns None)
            assert!(w.ensure_started(&app).unwrap().is_none(), "the root must not be re-established");
        }

        // No per-conversation directories accumulate under workplace/ any more.
        let made = std::fs::read_dir(app.join(WORKPLACE_DIR)).map(|d| d.count()).unwrap_or(0);
        assert_eq!(made, 0, "the default tier must not create per-conversation dirs, got {made}");

        // A **new conversation** re-establishes its root — the same app root.
        w.reset_for_new_conversation();
        assert_eq!(w.snapshot(), (Mode::Default, None));
        let second = w.ensure_started(&app).unwrap().expect("a new conversation should establish the root again");
        assert_eq!(second, first, "every conversation's default root is the same app root");

        let _ = std::fs::remove_dir_all(&app);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The scratch sweep removes what the model left, and only that.
    #[test]
    fn the_scratch_sweep_empties_tmp_and_nothing_else() {
        let root = std::env::temp_dir().join(format!("aox-sweep-{}", std::process::id()));
        let aox = root.join(AOX_DIR);
        let tmp = aox.join(TMP_DIR);
        std::fs::create_dir_all(tmp.join("__pycache__")).unwrap();
        std::fs::create_dir_all(aox.join("other")).unwrap();
        std::fs::create_dir_all(aox.join(MEMORY_DIR)).unwrap();
        std::fs::write(tmp.join("fix1.py"), "print(1)").unwrap();
        std::fs::write(tmp.join("__pycache__").join("x.pyc"), "junk").unwrap();
        std::fs::write(aox.join("other").join("kept.txt"), "{}").unwrap();
        std::fs::write(aox.join(MEMORY_DIR).join("2026-10-23.md"), "# note").unwrap();
        std::fs::write(root.join("deliverable.txt"), "keep me").unwrap();

        // A window longer than the files are old must leave everything alone.
        let none = sweep_scratch(&root, std::time::Duration::from_secs(3600));
        assert_eq!(none, Swept::default());
        assert!(tmp.join("fix1.py").is_file());

        // No window: the scratch directory is emptied, recursively
        let all = sweep_scratch(&root, std::time::Duration::ZERO);
        assert_eq!(all.files, 1, "{all:?}");
        assert_eq!(all.dirs, 1, "the __pycache__ directory counts as one, not one per file");
        assert!(all.bytes > 0);
        assert!(!tmp.join("fix1.py").exists());
        assert!(!tmp.join("__pycache__").exists());
        // ...and the directory itself survives, because the host pre-creates it per turn.
        assert!(tmp.is_dir());
        // The rest of `.aox` (conversations, notes) and the workspace's own files are untouched —
        // the sweep must never be a reason to lose history.
        assert!(aox.join("other").join("kept.txt").is_file());
        assert!(aox.join(MEMORY_DIR).join("2026-10-23.md").is_file());
        assert!(root.join("deliverable.txt").is_file());

        // A workspace that was never started has no tmp dir: a sweep is a no-op, not an error.
        let bare = std::env::temp_dir().join(format!("aox-sweep-bare-{}", std::process::id()));
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(sweep_scratch(&bare, std::time::Duration::ZERO), Swept::default());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bare);
    }
}
