//! One shell call: from a [`Spec`] to captured output — spawn, fence, timeout, decode, cap.
//!
//! Everything a *dialect* decides is "which program, with which arguments"; everything else here is
//! identical for every dialect, which is the whole point of this module: `cmd`, PowerShell, Git Bash
//! and a future `bash` all go through one spawn path, one Job, one timeout, one output format. A
//! second implementation would be a second boundary to keep correct.
//!
//! # The two processes
//!
//! ```text
//! host ──spawn──▶ helper (same exe, --aox-shell-exec) ──spawn──▶ the real command
//!   │                    │ lowers its own token to Low (when the kernel boundary is on)
//!   └── Job Object ──────┴─ the command and every descendant inherit the Job and the token
//! ```
//!
//! The helper exists because a Medium process cannot simply *ask* for a Low child on Windows without
//! possibly needing privileges it does not have — see [`crate::shell`]'s module docs.
//!
//! # Two failure classes, kept apart
//!
//! [`Failure`] means "the call never produced a command outcome": the boundary could not be applied
//! ([`Failure::NoBoundary`]) or the process could not start ([`Failure::Spawn`]). A command that ran
//! and exited non-zero is **not** a failure — it is an [`Outcome`] with `code = Some(1)`, because the
//! model needs to see the compiler's own error text, not our summary of it.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::helper::{self, HelperSpec};
use crate::plugin::ProcessFlags;
use crate::runtime::Runtime;

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

    /// The default when the model does not say. `cmd` on Windows (always present), `bash` on other
    /// platforms — but this build only ships the Windows dialects, so non-Windows callers must pass
    /// one explicitly.
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
    ///
    /// The quoting rules are the interesting part:
    /// - `cmd /D /S /C` strips exactly one pair of outer quotes, which is what makes a whole command
    ///   line with its own quotes survive being passed as a single argument.
    /// - PowerShell takes the command as one argument; Rust's argument quoting already produces the
    ///   right thing, so no manual wrapping.
    /// - `git` is not a shell: its "command" is split into arguments (see [`split_args`]).
    fn command_line(&self, command: &str, bash: Option<&super::bash::Bash>) -> CmdLine {
        match self {
            // ⚠️ `cmd` is the one dialect whose command must **not** be passed as an ordinary
            // argument: Rust quotes arguments with the MSVC rules (`"` becomes `\"`) and `cmd` does
            // not understand that escape — it sees a literal backslash-quote and answers "the syntax
            // of the command is incorrect". Measured: `type "C:\a b\c.txt"` failed exactly that way.
            // Handing the tail over verbatim inside one pair of quotes — which `/S` then strips — is
            // the form `cmd` actually parses.
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
            // `command` is a **script** here (the tool has already prepended the prologue). It goes in as
            // one ordinary argument: MSYS parses argv the same way the C runtime does, so Rust's quoting
            // produces exactly what bash expects — unlike `cmd`, which needs the verbatim tail.
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
///
/// Hand-rolled rather than `CommandLineToArgvW` so the behaviour is the same on every platform and
/// can be unit-tested without a Windows process. The one thing it does *not* implement is the
/// `\"` escape (rare in `git` commands, and a wrong guess would be worse than a documented limit).
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
    /// The call was stopped by its own timeout (the tree was killed).
    pub timed_out: bool,
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

/// Run one command. The caller has already made every policy decision — this only executes.
pub async fn run(rt: &Arc<Runtime>, spec: &Spec) -> Result<Outcome, Failure> {
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

    let exe = std::env::current_exe()
        .map_err(|e| Failure::Spawn(crate::tf!("could not locate my own exe: {}", e)))?;

    // The fence is created **regardless of `[sandbox] enabled`**: a timeout must kill the whole
    // tree, and that is a correctness need, not a hardening option.
    let sandbox = crate::sandbox::Sandbox::for_shell(rt.config.shell.max_processes);

    let mut cmd = Command::new(&exe);
    cmd.arg(helper::FLAG)
        .env(helper::SPEC_ENV, encoded)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
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
    let out_task = tokio::spawn(read_capped(stdout, cap));
    let err_task = tokio::spawn(read_capped(stderr, cap));

    let (code, timed_out) = match tokio::time::timeout(spec.timeout, child.wait()).await {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(e)) => {
            fence.terminate();
            return Err(Failure::Spawn(crate::tf!("the shell helper could not be waited on: {}", e)));
        }
        Err(_) => {
            // The whole tree, not just the child we hold: `cargo` leaves `rustc` behind otherwise.
            fence.terminate();
            let _ = child.wait().await;
            (None, true)
        }
    };
    let elapsed = started.elapsed();

    // After `terminate` the pipes reach EOF as the tree dies; a short grace period is enough, and a
    // pipe that still does not close must not hang the tool call.
    let out = join_capture(out_task).await;
    let err = join_capture(err_task).await;

    // The helper's own signals travel on stderr, marked with a `\x01` prefix that a normal program
    // will not produce. Strip them from what the model reads, and turn them into a `Failure` — the
    // model must not be told "the command failed" when the truth is "the boundary did not apply".
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

    Ok(Outcome { code, elapsed, out, err, timed_out })
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
///
/// Keeping both ends is deliberate: a build log puts the *reason* at the top ("error: linking with
/// `cc` failed") and the *list of failures* at the bottom. A plain "first N bytes" would show a
/// screen of `Compiling …` and none of the answer.
async fn read_capped<R>(reader: Option<R>, cap: usize) -> Capture
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
}
