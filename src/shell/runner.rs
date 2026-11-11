//! One shell call: from a Spec to captured output — spawn, fence, timeout, decode, cap.
//! Everything a *dialect* decides is "which program, with which arguments"; everything else here is identical...
//! # The two processes.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use super::helper::{self, HelperSpec};
use crate::plugin::ProcessFlags;
use crate::runtime::Runtime;

/// Test-only override for "which executable re-enters as the shell helper".
#[cfg(test)]
static TEST_HOST: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn use_test_host(path: std::path::PathBuf) {
    *TEST_HOST.lock().unwrap() = Some(path);
}

fn host_exe() -> Result<PathBuf, Failure> {
    #[cfg(test)]
    if let Some(p) = TEST_HOST.lock().unwrap().clone() {
        return Ok(p);
    }
    std::env::current_exe()
        .map_err(|e| Failure::Spawn(crate::tf!("could not locate my own exe: {}", e)))
}

/// Which interpreter the command is handed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// `cmd.exe /D /S /C "<command>"`
    Cmd,
    /// `powershell.exe -NoProfile -NonInteractive -Command "<command>"`
    PowerShell,
    /// `pwsh.exe` (PowerShell 7+), same flags as [`Dialect::PowerShell`].
    Pwsh,
    /// `git.exe` with the command split into arguments — a convenience, not a shell.
    Git,
    /// A POSIX shell (`bash`, or `busybox sh`): the command is a **script**, optionally preceded by the
    /// host's prologue (see `shell::bash::prologue`).
    Bash,
}

impl Dialect {
    /// Parse the tool's `shell` argument. `None` = not a dialect we accept (the caller turns that
    /// into a tool error listing the valid values, rather than silently picking one).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cmd" => Some(Dialect::Cmd),
            "powershell" | "ps" => Some(Dialect::PowerShell),
            "pwsh" => Some(Dialect::Pwsh),
            "git" => Some(Dialect::Git),
            "bash" | "sh" => Some(Dialect::Bash),
            _ => None,
        }
    }

    /// The default when the model does not say. `cmd` on Windows (always present), `bash` on other platforms —...
    pub fn default_for_platform() -> Self {
        Dialect::Cmd
    }

    /// A stable word for the output header — the model must be able to tell what ran the command.
    pub fn label(&self) -> &'static str {
        match self {
            Dialect::Cmd => "cmd",
            Dialect::PowerShell => "powershell",
            Dialect::Pwsh => "pwsh",
            Dialect::Git => "git",
            Dialect::Bash => "bash",
        }
    }

    /// The program and arguments this dialect runs for `command`.
    fn command_line(&self, command: &str, bash: Option<&super::bash::Bash>) -> CmdLine {
        match self {
            // ️ `cmd` is the one dialect whose command must not be passed as an ordinary argument: Rust quotes arguments...
            Dialect::Cmd => CmdLine {
                program: "cmd.exe".to_string(),
                args: vec!["/D".into(), "/S".into(), "/C".into()],
                raw_tail: Some(format!("\"{command}\"")),
            },
            Dialect::PowerShell => CmdLine {
                program: "powershell.exe".to_string(),
                args: vec![
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-ExecutionPolicy".into(),
                    "Bypass".into(),
                    "-Command".into(),
                    command.to_string(),
                ],
                raw_tail: None,
            },
            Dialect::Pwsh => CmdLine {
                program: "pwsh.exe".to_string(),
                args: vec![
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    command.to_string(),
                ],
                raw_tail: None,
            },
            Dialect::Git => CmdLine {
                program: "git.exe".to_string(),
                args: split_args(command),
                raw_tail: None,
            },
            // `command` is a script here (the tool has already prepended the prologue).
            Dialect::Bash => match bash {
                Some(b) => {
                    let (program, args) = b.command_line(command);
                    CmdLine { program, args, raw_tail: None }
                }
                // Unreachable in practice (`run` resolves the bash before getting here), but a plain
                // name is a better failure than a panic: the OS error will say "not found".
                None => CmdLine {
                    program: "bash.exe".to_string(),
                    args: vec!["-c".into(), command.to_string()],
                    raw_tail: None,
                },
            },
        }
    }
}

/// The pieces of a command line: program, ordinary arguments, and an optional **verbatim tail**
/// appended without quoting (see [`Dialect::command_line`] for why `cmd` needs one).
#[derive(Debug, Clone)]
pub struct CmdLine {
    pub program: String,
    pub args: Vec<String>,
    pub raw_tail: Option<String>,
}

/// Split a command line into arguments, honouring double quotes.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Everything one call needs, fully resolved by the tool layer (no policy decisions here).
#[derive(Debug, Clone)]
pub struct Spec {
    pub dialect: Dialect,
    /// The command line, verbatim as the model wrote it.
    pub command: String,
    /// Already checked against the workplace boundary; the helper also sets it as the child's cwd.
    pub cwd: PathBuf,
    pub timeout: Duration,
    /// Kill the call when it has written nothing to either stream for this long. `None` = off.
    pub idle: Option<Duration>,
    /// Extra environment entries, on top of the inherited environment.
    pub env: Vec<(String, String)>,
    /// Run at Low integrity (the kernel boundary). `false` = scan-only mode.
    pub lowil: bool,
}

/// One stream's worth of output, after decoding and capping.
#[derive(Debug, Default, Clone)]
pub struct Capture {
    pub text: String,
    /// Bytes the command actually wrote, before capping — so "truncated" can be stated honestly.
    pub total: u64,
}

impl Capture {
    pub fn truncated(&self) -> bool {
        self.total > self.text.len() as u64
    }
}

/// What a call produced.
#[derive(Debug)]
pub struct Outcome {
    /// The command's exit code; `None` when the platform could not report one.
    pub code: Option<i32>,
    pub elapsed: Duration,
    pub out: Capture,
    pub err: Capture,
    /// The call was **stopped by us** (the whole tree was killed), and why.
    pub stopped: Option<Stop>,
    /// Processes still alive in the Job when the shell finished, before the fence was dropped.
    pub left_behind: Option<u32>,
}

/// Why a call was stopped early. Both are the host's decision, never the command's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// It ran out of its total budget.
    Timeout(Duration),
    /// It had written nothing to either stream for this long. Carries the idle budget, not the
    /// actual silence — same number, and the one the message needs to quote.
    Idle(Duration),
}

/// The call never became an outcome.
#[derive(Debug)]
pub enum Failure {
    /// The kernel boundary could not be applied. `auto` mode retries without it; `kernel` mode
    /// refuses the call.
    NoBoundary(String),
    /// The helper (or the command) could not be started at all.
    Spawn(String),
}

/// Build the host-side helper command: program, helper argv, env and cwd.
fn command_for(rt: &Arc<Runtime>, spec: &Spec) -> Result<Command, Failure> {
    // A bash has to be found *before* the command line is built (its path is the program), and its
    // `PATH` must be rewritten: MSYS would otherwise treat every `C:\…` entry as a relative POSIX path.
    let bash = match spec.dialect {
        Dialect::Bash => Some(super::bash::validated(&rt.config.shell, &rt.root).map_err(Failure::Spawn)?),
        _ => None,
    };
    let mut env = spec.env.clone();
    if let Some(b) = &bash {
        let inherited = std::env::var("PATH").unwrap_or_default();
        env.retain(|(k, _)| !k.eq_ignore_ascii_case("PATH"));
        env.push(("PATH".to_string(), super::bash::path_env(b, &inherited)));
    }

    let line = spec.dialect.command_line(&spec.command, bash.as_ref());
    let helper_spec = HelperSpec {
        program: line.program,
        args: line.args,
        raw_tail: line.raw_tail,
        cwd: spec.cwd.display().to_string(),
        env,
        lowil: spec.lowil,
    };
    let encoded = serde_json::to_string(&helper_spec)
        .map_err(|e| Failure::Spawn(crate::tf!("could not encode the shell spec: {}", e)))?;
    let exe = host_exe()?;

    let mut cmd = Command::new(&exe);
    cmd.arg(helper::FLAG)
        .env(helper::SPEC_ENV, encoded)
        .current_dir(&spec.cwd);
    Ok(cmd)
}

/// A shell process that is deliberately allowed to outlive its call.
pub struct Background {
    pub child: Child,
    pub fence: crate::sandbox::Fence,
    pub started: Instant,
}

/// Start a command and keep its process tree alive under a caller-owned Job.
pub fn spawn_background(
    rt: &Arc<Runtime>,
    spec: &Spec,
    log: &std::path::Path,
    max_processes: u32,
) -> Result<Background, Failure> {
    use std::fs::OpenOptions;

    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| Failure::Spawn(crate::tf!("could not open the job log {}: {}", log.display(), e)))?;
    let err = out
        .try_clone()
        .map_err(|e| Failure::Spawn(crate::tf!("could not duplicate the job log handle: {}", e)))?;

    let sandbox = crate::sandbox::Sandbox::for_shell(max_processes);
    let mut cmd = command_for(rt, spec)?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .process_flags(sandbox.create_flags());

    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| Failure::Spawn(crate::tf!("could not start the background command: {}", e)))?;
    let fence = sandbox.contain(&child);
    if !fence.active {
        let why = fence
            .reason
            .clone()
            .unwrap_or_else(|| crate::i18n::t("the Job Object could not be fitted").to_string());
        let _ = child.start_kill();
        return Err(Failure::Spawn(crate::tf!(
            "refusing to run an untracked background process: {}",
            why
        )));
    }
    Ok(Background { child, fence, started })
}

/// Run one command. The caller has already made every policy decision — this only executes.
pub async fn run(rt: &Arc<Runtime>, spec: &Spec) -> Result<Outcome, Failure> {
    // The fence is created **regardless of `[sandbox] enabled`**: a timeout must kill the whole
    // tree, and that is a correctness need, not a hardening option.
    let sandbox = crate::sandbox::Sandbox::for_shell(rt.config.shell.max_processes);

    let mut cmd = command_for(rt, spec)?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `CREATE_SUSPENDED` when the fence is on, so the process cannot run one line before it is in
    // the Job — exactly the same discipline as a plugin child (`plugin::spawn_command`).
    cmd.process_flags(sandbox.create_flags());

    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| Failure::Spawn(crate::tf!("could not start the shell helper: {}", e)))?;
    let fence = sandbox.contain(&child);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let cap = rt.config.shell.max_output_bytes;
    // Milliseconds since `started` at which the last byte arrived, one shared stamp for both streams: a command...
    let activity = Arc::new(AtomicU64::new(0));
    let out_task = tokio::spawn(read_capped(stdout, cap, activity.clone(), started));
    let err_task = tokio::spawn(read_capped(stderr, cap, activity.clone(), started));

    // Poll rather than `timeout(child.wait())`: two conditions can end the call (the total budget, and "no output...
    let deadline = started + spec.timeout;
    let mut stopped: Option<Stop> = None;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {}
            Err(e) => {
                fence.terminate();
                return Err(Failure::Spawn(crate::tf!("the shell helper could not be waited on: {}", e)));
            }
        }
        let now = Instant::now();
        if let Some(stop) =
            budget_spent(now, deadline, started, activity.load(Ordering::Relaxed), spec.idle)
        {
            stopped = Some(stop);
            break None;
        }
        tokio::time::sleep(TICK).await;
    };
    // Measured **before** the kill: "TIMED OUT after 120 s" must mean the budget, not the budget plus
    // however long the tree took to die.
    let elapsed = started.elapsed();
    if stopped.is_some() {
        kill_tree(&fence, &mut child).await;
    }

    // After `terminate` the pipes reach EOF as the tree dies; a short grace period is enough, and a
    // pipe that still does not close must not hang the tool call.
    let out = join_capture(out_task).await;
    let err = join_capture(err_task).await;

    // The helper's own signals travel on stderr, marked with a `\x01` prefix that a normal program will not produce.
    let (err_text, signal) = helper::strip_signals(&err.text);
    let err = Capture { text: err_text, total: err.total };

    if code == Some(helper::EXIT_LOWIL_FAIL) {
        let why = match signal {
            Some(helper::Signal::LowIl(reason)) => reason,
            _ => crate::i18n::t("the integrity boundary could not be applied").to_string(),
        };
        return Err(Failure::NoBoundary(why));
    }
    if code == Some(helper::EXIT_SPAWN_FAIL) {
        let why = match signal {
            Some(helper::Signal::Spawn(reason)) => reason,
            _ => crate::i18n::t("the command could not be started").to_string(),
        };
        return Err(Failure::Spawn(why));
    }

    // Read the tree's size after the shell itself has been reaped and before this function returns.
    let left_behind = if stopped.is_some() { Some(0) } else { fence.active_processes() };
    if left_behind.unwrap_or(0) > 0 {
        fence.terminate();
    }

    Ok(Outcome { code, elapsed, out, err, stopped, left_behind })
}

/// How often the wait loop looks at the child. Also the resolution of both deadlines — a 30 s idle
/// budget does not need a millisecond-accurate clock, and 10 wake-ups a second is not a cost.
const TICK: Duration = Duration::from_millis(100);

/// Which budget, if any, has run out — the whole policy of the wait loop, in one testable place.
fn budget_spent(
    now: Instant,
    deadline: Instant,
    started: Instant,
    last_activity_ms: u64,
    idle: Option<Duration>,
) -> Option<Stop> {
    if now >= deadline {
        return Some(Stop::Timeout(deadline.saturating_duration_since(started)));
    }
    let idle = idle?;
    let ran = now.saturating_duration_since(started);
    let quiet = ran.saturating_sub(Duration::from_millis(last_activity_ms));
    (quiet >= idle).then_some(Stop::Idle(idle))
}

/// How long the tree gets to die after `TerminateJobObject`, before we stop believing in the fence.
const GRACE: Duration = Duration::from_secs(5);

/// How long the process we hold gets after a direct `kill`, before the call gives up on it.
const GRACE_HARD: Duration = Duration::from_secs(2);

/// Stop the whole tree, and bound how long we wait for it to die.
async fn kill_tree(fence: &crate::sandbox::Fence, child: &mut Child) {
    fence.terminate();
    if died_within(child, GRACE).await {
        return;
    }
    // The fence did not take it (no Job, or the helper escaped one).
    let _ = child.start_kill();
    let _ = died_within(child, GRACE_HARD).await;
}

/// `true` when the child has exited (and been reaped) within `d`.
async fn died_within(child: &mut Child, d: Duration) -> bool {
    let until = Instant::now() + d;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            // The handle is gone; nothing left to wait for.
            Err(_) => return true,
        }
        if Instant::now() >= until {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn join_capture(
    task: tokio::task::JoinHandle<Capture>,
) -> Capture {
    match tokio::time::timeout(Duration::from_secs(3), task).await {
        Ok(Ok(c)) => c,
        _ => Capture::default(),
    }
}

/// Read one stream to EOF, keeping the head and the tail and counting everything.
async fn read_capped<R>(
    reader: Option<R>,
    cap: usize,
    activity: Arc<AtomicU64>,
    started: Instant,
) -> Capture
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(mut reader) = reader else { return Capture::default() };
    let head_cap = cap / 4;
    let tail_cap = cap.saturating_sub(head_cap);
    let mut head: Vec<u8> = Vec::new();
    let mut tail: VecDeque<u8> = VecDeque::new();
    let mut total: u64 = 0;
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                // The idle watchdog reads this. Stamped on **any** byte, not on a "useful" one:
                // the question it answers is "did the process say anything at all".
                activity.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                let mut chunk = &buf[..n];
                if head.len() < head_cap {
                    let take = (head_cap - head.len()).min(chunk.len());
                    head.extend_from_slice(&chunk[..take]);
                    chunk = &chunk[take..];
                }
                for b in chunk {
                    if tail.len() == tail_cap {
                        tail.pop_front();
                    }
                    tail.push_back(*b);
                }
            }
            Err(_) => break,
        }
    }
    let mut bytes = head;
    if total as usize > bytes.len() + tail.len() {
        bytes.extend_from_slice(crate::i18n::t("\n… [output elided] …\n").as_bytes());
    }
    bytes.extend(tail.iter().copied());
    Capture { text: super::decode(&bytes), total }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_args_honours_quotes() {
        assert_eq!(split_args("status -s"), vec!["status", "-s"]);
        assert_eq!(split_args("commit -m \"a b\""), vec!["commit", "-m", "a b"]);
        assert_eq!(split_args("  log   --oneline "), vec!["log", "--oneline"]);
        // An empty quoted argument is still an argument — `git commit -m ""` must not become `commit -m`.
        assert_eq!(split_args("commit -m \"\""), vec!["commit", "-m", ""]);
    }

    #[test]
    fn dialects_map_to_a_command_line() {
        // `cmd` hands the command over as a **verbatim tail**: quoting it like a normal argument is
        // what broke `type "C:\a b\c.txt"` (Rust escapes the inner quotes, cmd does not unescape).
        let c = Dialect::Cmd.command_line("echo hi", None);
        assert_eq!(c.program, "cmd.exe");
        assert!(c.args.iter().any(|x| x == "/C"));
        assert_eq!(c.raw_tail.as_deref(), Some("\"echo hi\""));

        let c = Dialect::Git.command_line("status -s", None);
        assert_eq!(c.program, "git.exe");
        assert_eq!(c.args, vec!["status", "-s"]);
        assert!(c.raw_tail.is_none(), "git takes real arguments, so it must not get a raw tail");

        // The command is one argument, not re-split: a pipeline must arrive intact.
        let c = Dialect::PowerShell.command_line("Get-ChildItem | Select-Object Name", None);
        assert_eq!(c.args.last().unwrap(), "Get-ChildItem | Select-Object Name");
    }

    #[test]
    fn the_bash_dialect_takes_a_script_not_a_command_line() {
        let bash = super::super::bash::Bash {
            exe: std::path::PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
            prefix: Vec::new(),
            flavour: "git-bash",
            posix_root: Some("/c/Program Files/Git".to_string()),
        };
        let c = Dialect::Bash.command_line("echo $AOX_WORKSPACE", Some(&bash));
        assert_eq!(c.program, r"C:\Program Files\Git\bin\bash.exe");
        // No dotfiles, and the script is the last argument rather than a `raw_tail` (which is a `cmd`
        // requirement, not a bash one).
        assert_eq!(c.args[..2], ["--noprofile", "--norc"]);
        assert_eq!(c.args.last().unwrap(), "echo $AOX_WORKSPACE");
        assert!(c.raw_tail.is_none());
    }

    #[test]
    fn parse_rejects_unknown_dialects() {
        assert_eq!(Dialect::parse("CMD"), Some(Dialect::Cmd));
        assert_eq!(Dialect::parse("git"), Some(Dialect::Git));
        assert_eq!(Dialect::parse("bash"), Some(Dialect::Bash));
        assert_eq!(Dialect::parse("sh"), Some(Dialect::Bash));
        assert_eq!(Dialect::parse("fish"), None);
    }

    /// The two deadlines, without waiting for either.
    #[test]
    fn the_budget_is_spent_on_time_or_on_silence() {
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let timeout = Duration::from_secs(120);
        let deadline = at(120);
        let idle = Some(Duration::from_secs(30));

        // Running, printing: nothing to do
        assert_eq!(budget_spent(at(10), deadline, start, 9_000, idle), None);
        // Silent since the start, 30 s in ⇒ idle.
        assert_eq!(budget_spent(at(30), deadline, start, 0, idle), Some(Stop::Idle(Duration::from_secs(30))));
        // Printed at 25 s, now 40 s: only 15 s of silence, still fine
        assert_eq!(budget_spent(at(40), deadline, start, 25_000, idle), None);
        // ...and at 56 s the same print is 31 s old ⇒ idle fires.
        assert_eq!(budget_spent(at(56), deadline, start, 25_000, idle), Some(Stop::Idle(Duration::from_secs(30))));
        // The total budget wins over a call that kept printing (a build that never ends).
        assert_eq!(budget_spent(at(120), deadline, start, 119_000, idle), Some(Stop::Timeout(timeout)));
        // Watchdog off: silence is not a reason to kill anything
        assert_eq!(budget_spent(at(119), deadline, start, 0, None), None);
    }

    /// The wait after a kill must be bounded.
    #[tokio::test]
    async fn waiting_for_a_killed_child_gives_up_instead_of_hanging() {
        let mut child = Command::new("cmd.exe")
            .args(["/C", "ping -n 30 127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cmd.exe must exist on Windows");

        // Still alive: the grace period must expire and say so, not block.
        assert!(!died_within(&mut child, Duration::from_millis(150)).await);

        // Killed directly (no Job here): it must be reaped well inside the grace.
        let _ = child.start_kill();
        assert!(
            died_within(&mut child, Duration::from_secs(3)).await,
            "a killed child must be reaped; hanging here is what the timeout path used to do"
        );
    }
}
