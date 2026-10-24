//! The host's built-in **command line** (`shell`), and the guard rail in front of it.
//!
//! # What it is
//!
//! One tool, four dialects (`cmd` / `powershell` / `pwsh` / `git`), executed by
//! [`crate::shell::runner`]. Its reason to exist is efficiency: an agent that can pipe, glob and
//! chain finishes in one call what would otherwise be five tool round trips — and every round trip
//! is a model request.
//!
//! # Where the boundary is (read this before touching the scanner)
//!
//! | Layer | What it really does |
//! |-------|---------------------|
//! | [`crate::workplace`] | the `cwd` must be an absolute path inside the workspace, so a *relative* write is inside by construction |
//! | [`scan`] | refuses commands whose **explicit write targets** point outside — a **guard rail**: it produces a readable refusal before half a command has run, and it is the only thing `boundary = "scan"` has |
//! | Low integrity (`crate::shell::lowil`) | the actual boundary: the command runs at Low and the workspace is labelled Low, so `no-write-up` makes any out-of-workspace write a kernel refusal — including the ones the scanner cannot see (`powershell -e <base64>`, a script file, a child process) |
//!
//! ⚠️ **Never describe the scanner as the boundary.** It is bypassable by construction; on Windows
//! the kernel is what holds, and `/shell` says which of the two is currently in force.
//!
//! # Reads are not restricted
//!
//! Deliberately the same asymmetry as [`crate::workplace::Access`]: `type C:\logs\x.log` works,
//! `> C:\logs\x.log` does not. Reading takes information; writing changes the device.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::runtime::Runtime;
use crate::shell::runner::{self, Dialect, Failure, Outcome, Spec, Stop};
use crate::tool::{Tool, ToolKind, ToolOutput};

/// The tool's name. Also the key `[auth] shell` governs (see `crate::auth::SHELL_TOOLS`), which is
/// why the constant lives here: one spelling, two readers.
pub const SHELL_TOOL: &str = "shell";

/// The `cwd` parameter (the tool's own name for it — [`PATH_ARG`] is the *file* tools' name).
const CWD_ARG: &str = "cwd";

pub fn shell_tools(cfg: &crate::config::ShellConfig) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ShellTool {
        timeout: cfg.timeout_secs,
        max_timeout: cfg.max_timeout_secs,
    })]
}

pub(crate) struct ShellTool {
    /// `[shell] timeout_secs` / `max_timeout_secs`, snapshotted at registration.
    ///
    /// The tool needs them in [`Tool::timeout_hint`], which is asked **before** `call` and gets no
    /// `Runtime` — and the outer guard in `Runtime::execute` is armed from that answer, so "I don't
    /// know my own budget yet" is not an option. Config is read once at startup; reading it again
    /// here would be the same numbers.
    timeout: u64,
    max_timeout: u64,
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        SHELL_TOOL
    }

    fn kind(&self) -> ToolKind {
        // `Host`, not `Meta`: this is "make the host do a job", the same category as the file tools.
        // Its authorisation tier is separate anyway (`[auth] shell`), not the `Meta` one.
        ToolKind::Host
    }

    fn description(&self) -> &str {
        "Run a command on the user's machine: **cmd.exe by default** — fast (~87 ms to start) and \
         `C:\\` paths and `/flag` arguments reach the program verbatim — or PowerShell / git by passing \
         `shell`. Work inside the workspace: the working directory must be inside it, and **writes and \
         deletes outside it are refused**. Reading anywhere is allowed. Use it for a Windows-native \
         command, a cmd builtin, `git`, or any CLI tool instead of writing a plugin for it. Output: \
         exit code + stdout + stderr (long output is truncated to its head and tail). \
         If you expect a call to take a while (a build, a big search), say so in your reply and pass \
         `expect_secs` so the user can see \"expected ~N s\" instead of guessing. A call that prints \
         nothing for 30 s is treated as wedged and killed — raise `idle_secs` for a command that is \
         legitimately silent."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The full command line, verbatim (pipes, quotes and && all work)."
                },
                CWD_ARG: {
                    "type": "string",
                    "description": "Absolute directory to run in; must be inside the workspace. \
                                    Defaults to the workspace root. Relative paths are refused."
                },
                "shell": {
                    "type": "string",
                    "enum": ["cmd", "powershell", "pwsh", "git"],
                    "description": "Which program interprets the command. Default \"cmd\". \
                                    Use \"git\" for a plain git subcommand (no shell syntax)."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Give up after this many seconds and kill the whole process tree. \
                                    Default from config (60), hard cap 600."
                },
                "idle_secs": {
                    "type": "integer",
                    "description": IDLE_DESC
                },
                "expect_secs": {
                    "type": "integer",
                    "description": EXPECT_DESC
                },
                crate::tool::PURPOSE_ARG: {
                    "type": "string",
                    "description": crate::tool::PURPOSE_DESC
                },
                "env": {
                    "type": "object",
                    "description": "Extra environment variables for this call only."
                }
            },
            "required": ["command"]
        })
    }

    fn cap_hint(&self, args: &Value) -> Option<u64> {
        // Tell `Runtime::execute` to stay out of the way: this call may legitimately run for
        // minutes, and the tool has its own, more precise timeout (which kills the tree).
        hint_cap(args, self.timeout, self.max_timeout)
    }

    fn expect_hint(&self, args: &Value) -> Option<u64> {
        hint_expected(args, self.max_timeout)
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        match run_shell(&args, rt, None).await {
            Ok(out) => out,
            Err(text) => ToolOutput::err(text),
        }
    }
}

/// Everything one call does, in order. Split out of [`Tool::call`] so the order is readable in one
/// screen and the tests can drive it directly.
pub(crate) async fn run_shell(
    args: &Value,
    rt: &Arc<Runtime>,
    forced: Option<Dialect>,
) -> Result<ToolOutput, String> {
    let cfg = &rt.config.shell;
    let prepared = prepare(args, rt, forced)?;
    let dialect = prepared.dialect;
    let cwd = prepared.cwd.clone();
    let ws = prepared.workspace.clone();
    let boundary_note = prepared.boundary;

    let started = Instant::now();
    let spec = Spec {
        dialect,
        command: prepared.script,
        cwd: cwd.clone(),
        timeout: Duration::from_secs(call_timeout(args, rt)),
        idle: call_idle(args, rt),
        env: prepared.env,
        // Always false: see the note in `prepare`. The helper still supports it, and the measurement
        // that made it useless lives in `docs/SHELL.md`.
        lowil: false,
    };

    let outcome = match runner::run(rt, &spec).await {
        Ok(o) => o,
        // `NoBoundary` cannot happen while `lowil` is false, but the helper is a separate binary and a
        // mismatch between the two would otherwise surface as "no such variant" far from here.
        Err(Failure::NoBoundary(why)) | Err(Failure::Spawn(why)) => return Err(why),
    };

    let mut text = format_outcome(&outcome, dialect, &cwd, boundary_note);
    if cfg.track_changes && outcome.stopped.is_none() {
        let changed = changed_since(&ws, started);
        if !changed.is_empty() {
            text.push_str(&crate::tf!("\nchanged: {} file(s)\n", changed.len()));
            for p in changed.iter().take(40) {
                text.push_str(&format!("  {}\n", p.display()));
            }
            if changed.len() > 40 {
                text.push_str(&crate::tf!("  … and {} more\n", changed.len() - 40));
            }
        }
    }

    let failed = outcome.stopped.is_some() || outcome.code != Some(0);
    let mut out = ToolOutput::ok(text);
    out.is_error = failed;
    Ok(out)
}

/// Everything a command needs before it is spawned, after policy has been applied.
///
/// Shared by the one-shot path and `shell_job`: a background command must pass the **same** cwd
/// check, command scan, bash prologue and environment setup as a foreground one. Keeping this in
/// one function is what stops "background" from becoming a second, weaker execution path.
pub(crate) struct Prepared {
    pub dialect: Dialect,
    /// The command's script after the bash prologue (for `cmd` / PowerShell: unchanged).
    pub script: String,
    /// The model's own line, for display.
    pub command: String,
    pub cwd: PathBuf,
    pub workspace: PathBuf,
    pub env: Vec<(String, String)>,
    pub boundary: Boundary,
}

/// Parse and validate a command against the workplace, producing a spawn-ready description.
pub(crate) fn prepare(
    args: &Value,
    rt: &Arc<Runtime>,
    forced: Option<Dialect>,
) -> Result<Prepared, String> {
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| crate::i18n::t("missing parameter \"command\"").to_string())?;

    // `forced` is how the `bash` tool reuses this whole path: same boundary, same Job, same output — one
    // execution path only.
    let dialect = match forced {
        Some(d) => d,
        None => match args.get("shell") {
            None | Some(Value::Null) => Dialect::default_for_platform(),
            Some(v) => {
                let raw = v.as_str().unwrap_or("");
                Dialect::parse(raw).ok_or_else(|| {
                    crate::tf!(
                        "{} is not a shell this tool has. Valid values: cmd, bash, pwsh, powershell, git.",
                        format!("{raw:?}")
                    )
                })?
            }
        },
    };

    // ── ① tier + cwd: the only *structural* guarantee in this file ──
    let (ws, unrestricted) = match rt.workplace.snapshot() {
        (crate::workplace::Mode::Full, _) => {
            (rt.workplace.active_root().unwrap_or_else(|| rt.root.clone()), true)
        }
        (_, Some(root)) => (root, false),
        (_, None) => {
            // Confined tier, root not decided yet (the `Pending` state): there is no "inside" to
            // write to, so this must be refused rather than run somewhere arbitrary.
            return Err(crate::i18n::t(
                "This conversation's workspace is not established yet (it is fixed when the user \
                 sends the first message), so no command can be run against it yet.",
            )
            .to_string());
        }
    };
    let raw_cwd = args.get(CWD_ARG).and_then(|v| v.as_str()).unwrap_or("");
    let cwd = if raw_cwd.trim().is_empty() {
        ws.clone()
    } else {
        rt.workplace
            .check_access(raw_cwd, crate::workplace::Access::Write)
            .map_err(|r| r.model_text())?
    };
    if !cwd.is_dir() {
        return Err(crate::tf!("the working directory {} does not exist", cwd.display()));
    }

    // ── ② the guard rail ──
    if !unrestricted {
        if let Some(why) = scan(command, dialect, &cwd, &ws) {
            return Err(why);
        }
    }

    // ── ③ the boundary this call actually gets ──
    //
    // One layer, and it is the one that works: the guard rail above, plus the `cwd` check in ①.
    //
    // A kernel-enforced boundary was implemented three separate ways (a lowered token inherited by the
    // child, the same token set on a suspended child, and `CreateProcessAsUser` with duplicated and
    // restricted tokens) and measured **not to hold** on Windows 11 26200: the child reports
    // `IL=0x1000 (Low)` yet the kernel refuses its writes even to a Low-labelled directory, so writes
    // inside the workspace fail too. `docs/SHELL.md` records the measurements and what would have to be
    // different for it to work. Shipping a `boundary = "kernel"` switch would promise something the
    // program cannot deliver, so there is no switch.
    let boundary = if unrestricted { Boundary::Unrestricted } else { Boundary::GuardRail };

    // A bash script gets the host's two exports in front of it (workspace in both path forms). It is a
    // prefix rather than a file on purpose: no dotfiles, nothing to clean up, and the model's own script
    // stays byte-identical after those two lines.
    let script = if dialect == Dialect::Bash {
        format!("{}{}", crate::shell::bash::prologue(&ws), command)
    } else {
        command.to_string()
    };

    Ok(Prepared {
        dialect,
        script,
        command: command.to_string(),
        env: child_env(rt, &ws, boundary),
        cwd,
        workspace: ws,
        boundary,
    })
}

/// Which boundary this call actually got — reported in the header, because "the write was refused"
/// means something different in each case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Boundary {
    /// The command scan only. A guard rail: it stops the obvious out-of-bounds writes and produces a
    /// readable refusal, and it is bypassable by construction (`-EncodedCommand`, a script file, a
    /// child process). The name says "only" so nobody mistakes it for a kernel boundary.
    GuardRail,
    /// `full` tier: nothing is restricted.
    Unrestricted,
}

impl Boundary {
    fn label(&self) -> &'static str {
        match self {
            Boundary::GuardRail => "guard-rail-only",
            Boundary::Unrestricted => "unrestricted",
        }
    }
}

/// The two call-level knobs the `shell` and `bash` schemas share, described **once**: two copies of a
/// paragraph drift the first time one of them is reworded, and the model would then be reading two
/// different stories about the same number.
pub(crate) const IDLE_DESC: &str = "Kill the call if it writes nothing to stdout or stderr for \
     this many seconds (default 30; 0 disables it for this call). Raise it for a command that is \
     legitimately silent for a long time (a `sleep`, a download with no progress output). A build or \
     a compile prints constantly, so the default suits those.";

pub(crate) const EXPECT_DESC: &str = "Your own estimate of how long this will take, in seconds. \
     Informational only — nothing enforces it. The user sees \"expected ~N s\" next to the running \
     call, which is how they tell \"still working\" from \"wedged\", so fill it in for anything you \
     expect to take more than a few seconds.";

/// This call's ceiling, in seconds — the number the UI shows and the outer guard is built from.
///
/// Shared by both shell tools. A flat constant here (it used to be `600 + 5`) meant the backstop was
/// five times wider than the timeout it backs up — so when the inner kill path failed, the user's only
/// exit was 615 s away.
pub(crate) fn hint_cap(args: &Value, timeout: u64, max: u64) -> Option<u64> {
    Some(call_timeout_with(args, timeout, max))
}

/// The model's own estimate, clamped so a typo cannot put "expected 999999 s" on the user's screen.
pub(crate) fn hint_expected(args: &Value, max: u64) -> Option<u64> {
    args.get("expect_secs")
        .and_then(|v| v.as_u64())
        .filter(|s| *s > 0)
        .map(|s| s.min(max.max(1)))
}

fn call_timeout(args: &Value, rt: &Arc<Runtime>) -> u64 {
    let cfg = &rt.config.shell;
    call_timeout_with(args, cfg.timeout_secs, cfg.max_timeout_secs)
}

/// The total budget for one call: what the model asked for, else the configured default, clamped to
/// the ceiling. Shared by the two readers that must never disagree — the spec that runs the command
/// and the hint that keeps the outer guard out of the way.
fn call_timeout_with(args: &Value, default: u64, max: u64) -> u64 {
    let asked = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(default);
    asked.clamp(1, max.max(1))
}

/// The idle budget for one call, or `None` when the watchdog is off.
fn call_idle(args: &Value, rt: &Arc<Runtime>) -> Option<Duration> {
    let cfg = &rt.config.shell;
    let secs = args.get("idle_secs").and_then(|v| v.as_u64()).unwrap_or(cfg.idle_timeout_secs);
    // A model can raise it for a command that is legitimately silent, but not past the total budget:
    // an idle budget larger than the call's own is a number that can never fire.
    let secs = secs.min(cfg.max_timeout_secs.max(1));
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// The environment the command sees.
///
/// Two jobs, both about the Low token:
/// 1. `TMP`/`TEMP`/`TMPDIR` and the build caches point **into the workspace**, because a Low process
///    cannot write `%TEMP%` or `%USERPROFILE%\.cargo` (both Medium). Without this a `cargo build`
///    fails inside its own cache with "access denied" — a failure that looks exactly like the
///    boundary misfiring.
/// 2. `AOX_WORKSPACE` / `AOX_SHELL_BOUNDARY` tell a script (and the model) where it is and which
///    boundary is in force, without having to infer it from a failed write.
fn child_env(rt: &Arc<Runtime>, ws: &Path, boundary: Boundary) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let tmp = ws.join(crate::workplace::AOX_DIR).join(crate::workplace::TMP_DIR);
    let _ = std::fs::create_dir_all(&tmp);
    for key in ["TMP", "TEMP", "TMPDIR"] {
        env.push((key.to_string(), tmp.display().to_string()));
    }
    if rt.config.shell.env_cache {
        let cache = ws.join(crate::workplace::AOX_DIR).join("toolcache");
        for (key, sub) in [
            ("CARGO_HOME", "cargo"),
            ("NPM_CONFIG_CACHE", "npm"),
            ("PIP_CACHE_DIR", "pip"),
            ("GOPATH", "go"),
        ] {
            let dir = cache.join(sub);
            let _ = std::fs::create_dir_all(&dir);
            env.push((key.to_string(), dir.display().to_string()));
        }
    }
    env.push(("AOX_WORKSPACE".into(), ws.display().to_string()));
    env.push(("AOX_SHELL_BOUNDARY".into(), boundary.label().to_string()));
    env
}

fn format_outcome(
    o: &Outcome,
    dialect: Dialect,
    cwd: &Path,
    boundary: Boundary,
) -> String {
    let mut s = String::new();
    match o.stopped {
        Some(Stop::Timeout(budget)) => s.push_str(&crate::tf!(
            "TIMED OUT after {} s (the call's budget was {} s) — the whole process tree was \
             terminated. A server never exits, so a larger timeout cannot help; use \
             shell_job {action:\"start\"} for a long-lived process.\n",
            format!("{:.0}", o.elapsed.as_secs_f64()),
            budget.as_secs()
        )),
        // The idle case carries the "what to do about it" line because the model is the only one who
        // knows whether silence was expected. Without it the natural next move is to retry the same
        // call and be killed again.
        Some(Stop::Idle(idle)) => s.push_str(&crate::tf!(
            "KILLED after {} s: it wrote nothing to stdout or stderr for {} s, which is how a \
             wedged process looks. The whole tree was terminated; whatever it printed before that is \
             below. If the silence was expected — a `sleep`, a download with no progress output — \
             retry with a larger `idle_secs` (0 turns the watchdog off for that call). If this is a \
             server or watcher, do not raise the budget: use shell_job {action:\"start\"} so it can \
             outlive the call.\n",
            format!("{:.0}", o.elapsed.as_secs_f64()),
            idle.as_secs()
        )),
        None => {}
    }
    // The command detached something and the fence is about to take it away. Without this sentence
    // the model sees only "exit 0, no output, port closed" and retries the same command — which is
    // exactly the loop this feature exists to end.
    if o.stopped.is_none() {
        if let Some(n) = o.left_behind.filter(|n| *n > 0) {
            s.push_str(&crate::tf!(
                "host: this call returned, but {} background process(es) it had started were \
                 terminated with its Job. A shell call cannot leave a server running. For a \
                 long-lived process use shell_job (action \"start\") instead of `&`, `nohup` or \
                 `start /b`.\n",
                n
            ));
        }
    }
    s.push_str(&crate::tf!(
        "exit {} · {} s · {} · boundary: {} · cwd {}\n",
        o.code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
        format!("{:.2}", o.elapsed.as_secs_f64()),
        dialect.label(),
        boundary.label(),
        cwd.display()
    ));
    for (name, cap) in [("stdout", &o.out), ("stderr", &o.err)] {
        if cap.text.trim().is_empty() {
            continue;
        }
        s.push_str(&crate::tf!("\n--- {} ({}) ---\n", name, human_bytes(cap.total)));
        s.push_str(cap.text.trim_end());
        s.push('\n');
        if cap.truncated() {
            s.push_str(&crate::tf!(
                "[{} was {}; kept the first quarter and the last three quarters]\n",
                name,
                human_bytes(cap.total)
            ));
        }
    }
    s
}

fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

// ─────────────────────────── the guard rail ───────────────────────────

/// Refuse a command whose **explicit write targets** land outside the workspace.
///
/// Returns the text for the model (`None` = let it run).
///
/// The rule is narrow on purpose: **only** absolute paths and `..` segments are judged, because the
/// cwd is already inside the workspace, so every relative write is inside by construction. That keeps
/// the false-positive rate near zero (`--output-format json` is not a path, `type C:\x.log` is a
/// read), which matters more than coverage: this layer exists to produce a *good refusal*, and a
/// refusal that also blocks legitimate work is worse than no layer at all.
fn scan(command: &str, dialect: Dialect, cwd: &Path, ws: &Path) -> Option<String> {
    let tokens = tokenize(command);
    let words: Vec<String> = tokens.iter().map(|t| t.to_ascii_lowercase()).collect();
    let is_pwsh = matches!(dialect, Dialect::PowerShell | Dialect::Pwsh);

    let mut suspects: Vec<&str> = Vec::new();

    // ① redirect targets: the most common way a shell writes a file.
    for (i, w) in words.iter().enumerate() {
        // The tokenizer has already split `>` / `>>` into their own tokens, so the write target is
        // simply whatever follows.
        if w == ">" || w == ">>" {
            if let Some(next) = tokens.get(i + 1) {
                if !is_operator(next) {
                    suspects.push(next.as_str());
                }
            }
        }
    }

    // ② directory changes: leaving the workspace is a write-position change even though `cd` itself
    //    writes nothing — everything after it would.
    let cd_commands = ["cd", "chdir", "pushd", "set-location", "sl"];
    for (i, w) in words.iter().enumerate() {
        let base = basename(w);
        if cd_commands.contains(&base) {
            if let Some(target) = tokens.iter().skip(i + 1).find(|t| !t.starts_with('-') && !t.starts_with('/')) {
                suspects.push(target.as_str());
            }
        }
    }

    // ③ commands whose *arguments* are destinations.
    if let Some(first) = words.first() {
        let base = basename(first);
        let writes = WRITE_PROGRAMS.contains(&base)
            || (is_pwsh && powsh_writer(&words))
            || (base == "sed" && words.iter().any(|w| w == "-i" || w.starts_with("-i")))
            || (base == "git" && words.get(1).map(|w| w == "clone").unwrap_or(false));
        if writes {
            for (i, t) in tokens.iter().enumerate() {
                if i == 0 || t.starts_with('-') || is_operator(t) {
                    continue;
                }
                suspects.push(t.as_str());
            }
        }
    }

    for cand in suspects {
        let cand = cand.trim_matches(|c| c == '"' || c == '\'' || c == ',');
        if cand.is_empty() || !looks_like_path(cand) {
            continue;
        }
        let resolved = resolve(cand, cwd);
        let inside_root = is_inside(ws, &resolved);
        if !inside_root {
            return Some(out_of_bounds_text(cand, &resolved, ws));
        }
    }
    None
}

/// Programs whose positional arguments are write destinations. Deliberately **not** exhaustive — a
/// miss here costs nothing on Windows (the kernel still refuses) and costs the boundary in `scan`
/// mode, which `/shell` says out loud.
const WRITE_PROGRAMS: &[&str] = &[
    "del", "erase", "rm", "rmdir", "rd", "mkdir", "md", "move", "mv", "copy", "cp", "xcopy",
    "robocopy", "ren", "rename", "touch", "truncate", "tee", "dd", "mklink", "ln", "tar", "unzip",
    "7z", "expand", "certutil",
];

/// The PowerShell cmdlets that write. Matched on the whole lowercased word (they carry hyphens, so
/// they cannot collide with the short names above).
fn powsh_writer(words: &[String]) -> bool {
    const WRITERS: &[&str] = &[
        "out-file",
        "set-content",
        "add-content",
        "clear-content",
        "new-item",
        "copy-item",
        "move-item",
        "remove-item",
        "rename-item",
        "new-itemproperty",
        "set-itemproperty",
        "export-csv",
        "invoke-webrequest",
        "start-bitstransfer",
    ];
    words.iter().any(|w| WRITERS.contains(&basename(w)))
}

fn basename(w: &str) -> &str {
    let w = w.trim_matches('"');
    w.rsplit(['\\', '/']).next().unwrap_or(w)
}

fn is_operator(t: &str) -> bool {
    matches!(t, "|" | "||" | "&" | "&&" | ";" | ">" | ">>" | "<" | "2>" | "2>>")
}

/// Does this token look like a path at all?
///
/// Only absolute-looking things and `..` chains: a bare word is a flag or an argument, and treating
/// those as paths is how a scanner starts rejecting `git log --graph`. A URL is **not** a path —
/// `https://…` matches "letter, colon, slash" and would otherwise be judged as drive `h:`.
fn looks_like_path(t: &str) -> bool {
    if t.contains("://") || t.starts_with("git@") {
        return false;
    }
    let b = t.as_bytes();
    if t == ".." || t.starts_with("../") || t.starts_with("..\\") || t.contains("\\..\\") || t.contains("/../") {
        return true;
    }
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        return true;
    }
    t.starts_with("\\\\") || t.starts_with("//")
}

fn tokenize(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = command.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '|' | '&' | ';' | '<' | '>' => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    let mut op = c.to_string();
                    if c == '|' && chars.peek() == Some(&'|') {
                        chars.next();
                        op.push('|');
                    }
                    if c == '&' && chars.peek() == Some(&'&') {
                        chars.next();
                        op.push('&');
                    }
                    if c == '>' && chars.peek() == Some(&'>') {
                        chars.next();
                        op.push('>');
                    }
                    out.push(op);
                }
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            },
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Resolve a token against the cwd, purely lexically (nothing is touched on disk).
fn resolve(t: &str, cwd: &Path) -> PathBuf {
    let p = Path::new(t);
    let joined = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    // Fold `.` and `..` textually: a path does not have to exist to be judged, and asking the
    // filesystem would let a not-yet-created target pass this check.
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn is_inside(root: &Path, p: &Path) -> bool {
    // ASCII case-insensitive: Windows paths are, and this must not depend on the filesystem.
    let r = root.display().to_string().replace('/', "\\").to_ascii_lowercase();
    let q = p.display().to_string().replace('/', "\\").to_ascii_lowercase();
    q == r || q.starts_with(&format!("{r}\\"))
}

/// The same three-part contract as [`crate::workplace::Reject::model_text`]: what happened, "this is
/// a boundary and not a broken tool, do not retry", and what the user can do about it.
fn out_of_bounds_text(raw: &str, resolved: &Path, ws: &Path) -> String {
    crate::tf!(
        "[Permission boundary] {} points outside the workspace ({} → {}), so this command was \
         not run.\n\n\
         This is not a broken command or a wrong argument — the command line may only **write and \
         delete** inside the workspace. **Do not retry it, and do not look for another way around \
         it** (the same target is refused however it is spelled). Reading outside the workspace is \
         fine: only writes and deletes are refused.\n\n\
         If the task really needs to write there, tell the user they can raise the boundary: the \
         \"Workspace\" button at the bottom-left of the input box, `/workplace full|<dir>`, or \
         `[workplace]` in config.toml. Otherwise do that part inside the workspace.",
        format!("{raw:?}"),
        ws.display(),
        resolved.display()
    )
}

// ─────────────────────────── what changed ───────────────────────────

/// Files under the workspace whose mtime is at or after `since`.
///
/// Bounded on purpose (depth, entry count, skipped directories): this runs after every command, and
/// `node_modules` alone would otherwise cost more than the command did.
fn changed_since(ws: &Path, since: Instant) -> Vec<PathBuf> {
    /// A little slack: filesystems round mtime, and a write that lands exactly on the boundary must
    /// not be lost.
    const SLACK: Duration = Duration::from_secs(2);
    const MAX_DEPTH: usize = 8;
    const MAX_ENTRIES: usize = 4000;
    const SKIP: &[&str] = &[".git", "node_modules", "target", "__pycache__", ".venv", "venv", "dist"];
    // `since` is an `Instant` (monotonic — the right thing to measure a call with) but file mtimes
    // are `SystemTime`, so the cutoff has to be expressed in the wall clock: "now minus how long ago
    // the call started, minus the slack".
    let cutoff = std::time::SystemTime::now()
        .checked_sub(since.elapsed() + SLACK)
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut out = Vec::new();
    let mut seen = 0usize;
    let mut stack = vec![(ws.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH || seen > MAX_ENTRIES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                break;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let path = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                if SKIP.contains(&name.as_str()) || (depth == 0 && name == crate::workplace::AOX_DIR) {
                    continue;
                }
                stack.push((path, depth + 1));
            } else if meta.modified().map(|m| m >= cutoff).unwrap_or(false) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The body of `/shell`: what the command line may do right now, and what it may not.
///
/// The second half is written on purpose. The most common misuse of this feature is assuming the
/// boundary covers more than it does — printing the limits next to the guarantees is far more
/// effective than documenting them.
pub fn report(rt: &Arc<Runtime>) -> String {
    let c = &rt.config.shell;
    let mut s = String::from(crate::i18n::t("Shell (config.toml's [shell])\n\n"));
    if !c.enabled {
        s.push_str(&crate::i18n::t("  status: off — the tools are not registered, the model never sees them\n"));
        return s;
    }
    let (mode, root) = rt.workplace.snapshot();
    let ws = root.unwrap_or_else(|| rt.root.clone());
    let tier = match mode {
        crate::workplace::Mode::Full => crate::i18n::t("full access — nothing is restricted"),
        crate::workplace::Mode::Fixed(_) => crate::i18n::t("picked workspace (confined)"),
        crate::workplace::Mode::Default => crate::i18n::t("default (confined)"),
    };
    s.push_str(&crate::tf!(
        "  tools: bash · shell (cmd / powershell / pwsh / git)\n  \
         workspace: {} — {}\n  \
         boundary: guard rail only (the command scan) — see the limits below\n  \
         timeout: {} s per call (hard cap {}), output {} per stream (head + tail)\n  \
         processes: at most {} per call; the whole tree dies on timeout (one Job Object per call)\n  \
         caches: {}\n",
        ws.display(),
        tier,
        c.timeout_secs,
        c.max_timeout_secs,
        human_bytes(c.max_output_bytes as u64),
        c.max_processes,
        if c.env_cache {
            crate::i18n::t("CARGO_HOME / NPM_CONFIG_CACHE / PIP_CACHE_DIR / TMP point into <workspace>\\.aox\\")
        } else {
            crate::i18n::t("inherited (unchanged environment)")
        }
    ));
    match crate::tool::bash::available(rt) {
        Some(b) => s.push_str(&crate::tf!("  bash: {} ({})\n", b.flavour, b.exe.display())),
        None => s.push_str(&crate::i18n::t(
            "  bash: not found — [shell] bash, or a Git for Windows install, would enable the bash tool\n",
        )),
    }
    // What the model is actually told to reach for first — `[shell] prefer` is an instruction, and
    // this line exists so the user can check it without reading the prompt.
    s.push_str(&if crate::shell::prefers_bash(c) {
        crate::i18n::t("  order: bash first, then shell (cmd), then powershell — [shell] prefer\n")
    } else {
        crate::i18n::t("  order: shell (cmd) first, then bash, then powershell — [shell] prefer\n")
    });
    s.push_str(&crate::i18n::t(
        "\n  ✓ the working directory must be inside the workspace, so a relative write lands inside\n  \
         ✓ obvious out-of-bounds writes and deletes are refused before the command runs, with an \
         explanation the model can act on\n  \
         ✓ reads anywhere — deliberately unrestricted, exactly like read_file\n  \
         ✗ **this is a guard rail, not a boundary**: a command that hides its target \
         (encoded command, a script file, a child process) walks straight through it\n  \
         ✗ network: a command can download and upload; there is no network boundary here\n\n  \
         A kernel-enforced version was built and measured not to hold on Windows 11 26200 — the \
         numbers are in docs/SHELL.md. The header of every call states which layer it got.\n",
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The three knobs, at the boundary values a model actually produces: nothing, a sensible ask,
    /// an impossible ask, and a zero (which must not mean "no timeout").
    #[test]
    fn the_call_budget_is_what_was_asked_for_clamped_to_the_ceiling() {
        assert_eq!(call_timeout_with(&json!({}), 120, 600), 120);
        assert_eq!(call_timeout_with(&json!({"timeout_secs": 30}), 120, 600), 30);
        assert_eq!(call_timeout_with(&json!({"timeout_secs": 9999}), 120, 600), 600);
        assert_eq!(call_timeout_with(&json!({"timeout_secs": 0}), 120, 600), 1);
        // What the UI shows must be the budget itself, not the outer leash wrapped around it.
        assert_eq!(hint_cap(&json!({"timeout_secs": 30}), 120, 600), Some(30));
    }

    #[test]
    fn the_estimate_is_clamped_and_zero_means_silence() {
        assert_eq!(hint_expected(&json!({}), 600), None);
        assert_eq!(hint_expected(&json!({"expect_secs": 0}), 600), None);
        assert_eq!(hint_expected(&json!({"expect_secs": 90}), 600), Some(90));
        assert_eq!(hint_expected(&json!({"expect_secs": 99_999}), 600), Some(600));
    }

    fn ws() -> PathBuf {
        PathBuf::from(r"E:\rustapp\workplace\demo")
    }
    fn cwd() -> PathBuf {
        ws()
    }

    fn refuses(cmd: &str) -> bool {
        scan(cmd, Dialect::Cmd, &cwd(), &ws()).is_some()
    }

    #[test]
    fn redirects_outside_are_refused() {
        assert!(refuses(r"echo hi > C:\Windows\Temp\x.txt"));
        assert!(refuses(r"echo hi > ..\..\escape.txt"));
        assert!(refuses("dir > D:/other/log.txt"));
    }

    #[test]
    fn redirects_inside_are_allowed() {
        assert!(!refuses("echo hi > out.txt"));
        assert!(!refuses(r"echo hi > E:\rustapp\workplace\demo\out.txt"));
        assert!(!refuses("../sibling.txt"));
        assert!(!refuses(r"cargo build > build.log 2>&1"));
    }

    #[test]
    fn reads_outside_are_allowed() {
        // The whole point of the asymmetry: reading anywhere is legitimate.
        assert!(!refuses(r"type C:\Windows\win.ini"));
        assert!(!refuses(r"dir C:\Users"));
        assert!(!refuses(r"git -C D:\other\repo status"));
    }

    #[test]
    fn destructive_commands_outside_are_refused() {
        assert!(refuses(r"del C:\Users\me\Documents\a.docx"));
        assert!(refuses(r"rmdir /s /q D:\other"));
        assert!(refuses(r"copy a.txt C:\Windows\Temp\a.txt"));
        assert!(refuses(r"robocopy . D:\other /E"));
    }

    #[test]
    fn destructive_commands_inside_are_allowed() {
        assert!(!refuses(r"del out.txt"));
        assert!(!refuses(r"rmdir /s /q E:\rustapp\workplace\demo\old"));
        assert!(!refuses(r"copy a.txt b.txt"));
    }

    #[test]
    fn leaving_the_workspace_is_refused() {
        assert!(refuses(r"cd D:\other && del x"));
        assert!(refuses("pushd C:\\Windows"));
        assert!(!refuses("cd sub && del x"));
    }

    #[test]
    fn powershell_writers_outside_are_refused() {
        let pw = Dialect::PowerShell;
        assert!(scan(r"Set-Content -Path C:\x.txt -Value 1", pw, &cwd(), &ws()).is_some());
        assert!(scan(r"Out-File D:\a.txt", pw, &cwd(), &ws()).is_some());
        assert!(scan(r"Get-Content C:\x.txt", pw, &cwd(), &ws()).is_none());
    }

    #[test]
    fn git_clone_outside_is_refused() {
        assert!(refuses(r"git clone https://example.com/x.git D:\other\repo"));
        assert!(!refuses(r"git clone https://example.com/x.git repo"));
        assert!(!refuses("git status -s"));
    }

    #[test]
    fn flags_are_not_mistaken_for_paths() {
        assert!(!refuses("git log --format=%H --max-count=5"));
        assert!(!refuses("cargo build --release --target-dir target"));
        assert!(!refuses(r#"findstr /s /i "error" *.log"#));
    }

    #[test]
    fn sed_in_place_outside_is_refused() {
        assert!(refuses(r"sed -i s/a/b/ C:\x.txt"));
        assert!(!refuses("sed -i s/a/b/ x.txt"));
        assert!(!refuses(r"sed -n 1p C:\x.txt"), "sed without -i reads, which is allowed");
    }

    #[test]
    fn path_folding_sees_through_dotdot() {
        let got = resolve(r"E:\rustapp\workplace\demo\..\..\secret.txt", &cwd());
        assert_eq!(got, PathBuf::from(r"E:\rustapp\secret.txt"));
        assert!(!is_inside(&ws(), &got));
    }

    #[test]
    fn case_and_separators_do_not_escape_the_comparison() {
        assert!(is_inside(&ws(), Path::new(r"E:\RUSTAPP\workplace\demo\a.txt")));
        assert!(is_inside(&ws(), Path::new("E:/rustapp/workplace/demo/a.txt")));
        // A sibling whose name merely starts with the same letters is *not* inside.
        assert!(!is_inside(&ws(), Path::new(r"E:\rustapp\workplace\demo2\a.txt")));
    }

    #[test]
    fn tokenizer_splits_operators_and_keeps_quotes() {
        assert_eq!(tokenize(r#"echo "a b" | more"#), vec!["echo", "a b", "|", "more"]);
        assert_eq!(tokenize("a && b || c"), vec!["a", "&&", "b", "||", "c"]);
        assert_eq!(tokenize(r"x > out.txt"), vec!["x", ">", "out.txt"]);
    }

    /// A one-shot call that detaches a child must say so. Without this sentence the model sees a
    /// successful, silent command followed by a closed port, and its only apparent option is to run
    /// the same thing again.
    #[tokio::test]
    async fn a_detached_child_is_reported_as_terminated() {
        if !cfg!(windows) {
            return;
        }
        let host = ["target/debug/aox.exe", "target/release/aox.exe"]
            .iter()
            .map(|rel| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
            .find(|p| p.is_file());
        let Some(host) = host else { return };
        crate::shell::runner::use_test_host(host);

        let dir = std::env::temp_dir().join("aox-shell-detached-child");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = crate::config::Config::dummy();
        cfg.workplace = crate::config::WorkplaceConfig {
            mode: "fixed".into(),
            root: Some(dir.to_string_lossy().into_owned()),
        };
        let rt = crate::runtime::Runtime::new(cfg.clone(), dir.clone(), dir.join("plugins"))
            .expect("runtime");
        let tool = shell_tools(&cfg.shell).into_iter().next().expect("shell tool");

        let out = tool
            .call(
                json!({
                    "command": "start /b ping -n 8 127.0.0.1",
                    "purpose": "detached-child probe"
                }),
                &rt,
            )
            .await;
        assert!(
            out.content.contains("background process(es)"),
            "a detached child must be reported, not silently killed:\n{}",
            out.content
        );
        assert!(out.content.contains("shell_job"), "{}", out.content);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
