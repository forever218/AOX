//! `shell_job` — the host-managed form of "a command that must stay running".
//!
//! # Why not just `bash … &`
//!
//! Every one-shot shell call owns a Job Object and drops it when the call returns. That is the
//! feature, not a bug: it guarantees a build, a test run or a `grep` cannot leave anything behind.
//! It also means a detached server dies exactly when the shell that started it exits. `nohup`,
//! `start /b` and `disown` only change the process's relationship to the *shell*; Job membership is
//! inherited and cannot be talked out of.
//!
//! A `shell_job` is the same fence, owned by the **runtime** instead. It survives calls, is visible
//! through `status`, writes to a log file, and dies on `stop`, on the lifetime cap, or when AOX
//! exits.
//!
//! # Actions
//!
//! | action | meaning |
//! |---|---|
//! | `start` | launch one command under a managed Job; returns an id and a log path |
//! | `status` | one job (with `id`) or all of them |
//! | `logs` | tail of the job's combined stdout/stderr |
//! | `stop` | terminate the whole tree now |
//!
//! There is deliberately no `restart`: a restart is "stop then start", and hiding those two facts
//! behind one word would make a port conflict impossible to see.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::runtime::Runtime;
use crate::tool::{Tool, ToolKind, ToolOutput};

/// Tool name; also the key `[auth] shell` governs.
pub const JOB_TOOL: &str = "shell_job";

pub fn job_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ShellJobTool)]
}

struct ShellJobTool;

#[async_trait]
impl Tool for ShellJobTool {
    fn name(&self) -> &str {
        JOB_TOOL
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }

    fn description(&self) -> &str {
        "Run a **long-lived** command (a dev server, watcher, preview process) that must survive after \
         the launching call returns. Use `start` once, then keep working with ordinary `bash`/`shell` \
         (curl the port, take screenshots), read output with `logs`, and call `stop` when done. \
         Background jobs are still fenced Job Objects: `stop`, the lifetime cap, or AOX exiting kills \
         the whole tree. Do NOT use `&`, `nohup`, `disown` or `start /b` in a normal shell call — its \
         Job is closed when the call returns and those descendants are terminated."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["start", "status", "logs", "stop"],
                    "description": "start a job / list running jobs / read its log / stop its whole process tree"
                },
                "command": {
                    "type": "string",
                    "description": "start: the command line to keep running (no trailing `&`). Required for start."
                },
                "cwd": {
                    "type": "string",
                    "description": "start: absolute working directory inside the workspace; defaults to its root."
                },
                "shell": {
                    "type": "string",
                    "enum": ["cmd", "bash", "powershell", "pwsh", "git"],
                    "description": "start: interpreter for the command; defaults to cmd. Use \"bash\" for POSIX syntax, pipes, globs and `$(…)`."
                },
                "name": {
                    "type": "string",
                    "description": "start: a short label for the job id and log file, e.g. \"vite\". Optional."
                },
                "id": {
                    "type": "string",
                    "description": "status / logs / stop: the id returned by start."
                },
                "max_bytes": {
                    "type": "integer",
                    "description": "logs: how many bytes from the end to return (default 20000, capped at 200000)."
                },
                crate::tool::PURPOSE_ARG: {
                    "type": "string",
                    "description": crate::tool::PURPOSE_DESC
                }
            },
            "required": ["action"]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        match dispatch(&args, rt).await {
            Ok(s) => ToolOutput::ok(s),
            Err(e) => ToolOutput::err(e),
        }
    }
}

async fn dispatch(args: &Value, rt: &Arc<Runtime>) -> Result<String, String> {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match action.as_str() {
        "start" => start(args, rt).await,
        "status" => status(args, rt).await,
        "logs" => logs(args, rt).await,
        "stop" => stop(args, rt).await,
        other => Err(crate::tf!(
            "unknown action {:?}; use start / status / logs / stop",
            other
        )),
    }
}

async fn start(args: &Value, rt: &Arc<Runtime>) -> Result<String, String> {
    if !rt.config.shell.enabled {
        return Err(crate::i18n::t("shell tools are disabled in config.toml ([shell] enabled = false)").into());
    }
    let cfg = &rt.config.shell;
    if cfg.background_max_jobs == 0 {
        return Err(crate::i18n::t(
            "background jobs are disabled ([shell] background_max_jobs = 0)",
        )
        .into());
    }
    if cfg.background_max_processes == 0 {
        return Err(crate::i18n::t(
            "background jobs need a positive process allowance ([shell] background_max_processes)",
        )
        .into());
    }

    // Reuse the one-shot policy: cwd, command scan, bash prologue and environment all come from
    // the same function, so a background server cannot slip past a boundary a `bash` call obeys.
    let prepared = crate::tool::shell::prepare(args, rt, None)?;
    let spec = crate::shell::runner::Spec {
        dialect: prepared.dialect,
        command: prepared.script.clone(),
        cwd: prepared.cwd.clone(),
        // Ignored by `spawn_background`; a server has no meaningful total/idle budget.
        timeout: Duration::ZERO,
        idle: None,
        env: prepared.env.clone(),
        lowil: false,
    };

    let name = crate::jobs::clean_name(args.get("name").and_then(|v| v.as_str()));
    let dir = crate::jobs::run_dir(&prepared.workspace);
    std::fs::create_dir_all(&dir)
        .map_err(|e| crate::tf!("could not create the job log directory {}: {}", dir.display(), e))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let log = dir.join(format!("{}-{}.log", name, stamp));

    // Fast pre-check (and a chance to free slots) before doing the expensive parts. Because the
    // startup probe awaits, the registry is **not** held across the spawn; capacity is checked
    // again under the lock below, and a late arrival is killed rather than silently overfilling.
    let mut jobs = rt.jobs.lock().await;
    let expired = jobs.expire(cfg.background_max_hours);
    jobs.make_room();
    if !jobs.has_room(cfg.background_max_jobs) {
        let ids = jobs.ids();
        return Err(crate::tf!(
            "no room for another background job (limit {}). Stop one first: {}. Expired: {}",
            cfg.background_max_jobs,
            if ids.is_empty() { "none running".to_string() } else { ids.join(", ") },
            if expired.is_empty() { "none".to_string() } else { expired.join(", ") }
        ));
    }
    drop(jobs);

    let mut bg = crate::shell::runner::spawn_background(
        rt,
        &spec,
        &log,
        cfg.background_max_processes,
    )
    .map_err(|e| match e {
        crate::shell::runner::Failure::Spawn(s)
        | crate::shell::runner::Failure::NoBoundary(s) => s,
    })?;

    // A very short startup probe catches the common failures (missing binary, bad cwd, port already
    // in use) while the log is still small and the model can act on the message immediately. A
    // server that is still running after this is reported as started.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let early_exit = bg.child.try_wait().ok().flatten();
    if let Some(status) = early_exit {
        if bg.fence.active_processes().unwrap_or(0) == 0 {
            let (tail, _, _) = crate::jobs::read_log_tail(&log, 4000).unwrap_or_default();
            let tail = tail.trim();
            return Err(crate::tf!(
                "the background command exited immediately ({}).{}",
                status
                    .code()
                    .map(|c| crate::tf!("exit {}", c))
                    .unwrap_or_else(|| crate::i18n::t("no exit code").to_string()),
                if tail.is_empty() {
                    String::new()
                } else {
                    crate::tf!("\n--- log ---\n{}", tail)
                }
            ));
        }
    }

    // Re-lock for the insert; the startup probe must not hold the registry lock across an await.
    // Another `start` may have taken the last slot in the meantime — kill this one instead of
    // exceeding the configured cap.
    let mut jobs = rt.jobs.lock().await;
    let expired = jobs.expire(cfg.background_max_hours);
    jobs.make_room();
    if !jobs.has_room(cfg.background_max_jobs) {
        bg.fence.terminate();
        return Err(crate::tf!(
            "the job limit ({}) was reached while this command was starting; it was stopped again. Expired: {}",
            cfg.background_max_jobs,
            if expired.is_empty() { "none".to_string() } else { expired.join(", ") }
        ));
    }
    let info = jobs.insert(
        &name,
        prepared.workspace.clone(),
        prepared.command.clone(),
        log.clone(),
        bg,
    );
    Ok(crate::tf!(
        "started background job {} (pid {}, {}).\nlog: {}\n\nUse shell_job {{\"action\":\"status\",\"id\":\"{}\"}} for state, \
         {{\"action\":\"logs\",\"id\":\"{}\"}} for output, and {{\"action\":\"stop\",\"id\":\"{}\"}} when finished.\n\
         Keep working normally (curl, screenshots); the job survives those calls.",
        info.id,
        info.pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "?".to_string()),
        info.state_label(),
        info.log.display(),
        info.id,
        info.id,
        info.id
    ))
}

async fn status(args: &Value, rt: &Arc<Runtime>) -> Result<String, String> {
    let id = args.get("id").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let mut jobs = rt.jobs.lock().await;
    let expired = jobs.expire(rt.config.shell.background_max_hours);
    let infos = jobs.status(id)?;
    drop(jobs);
    Ok(dropped_notice(&expired) + &format_jobs(&infos))
}

async fn logs(args: &Value, rt: &Arc<Runtime>) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| crate::i18n::t("logs needs the job id returned by start").to_string())?;
    let max = args
        .get("max_bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(20_000)
        .min(200_000) as usize;
    let mut jobs = rt.jobs.lock().await;
    let expired = jobs.expire(rt.config.shell.background_max_hours);
    let (info, path) = jobs.log_path(id)?;
    drop(jobs);

    let (tail, truncated, total) = crate::jobs::read_log_tail(&path, max)?;
    let mut out = dropped_notice(&expired);
    out.push_str(&crate::tf!(
        "job {} · {} · {} s · log: {}\nlog size: {} bytes{}\n",
        info.id,
        info.state_label(),
        info.age_secs,
        info.log.display(),
        total,
        if truncated { " (showing the tail)" } else { "" }
    ));
    if tail.trim().is_empty() {
        out.push_str(crate::i18n::t("(the log is empty so far)"));
    } else {
        out.push_str("--- log ---\n");
        out.push_str(tail.trim_end());
    }
    Ok(out)
}

async fn stop(args: &Value, rt: &Arc<Runtime>) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| crate::i18n::t("stop needs the job id returned by start").to_string())?;
    let mut jobs = rt.jobs.lock().await;
    let expired = jobs.expire(rt.config.shell.background_max_hours);
    let info = jobs.stop(id)?;
    Ok(dropped_notice(&expired)
        + &crate::tf!(
            "stopped job {} ({}); its whole process tree was terminated. Log kept at {}",
            info.id,
            info.command,
            info.log.display()
        ))
}

fn format_jobs(infos: &[crate::jobs::JobInfo]) -> String {
    if infos.is_empty() {
        return crate::i18n::t("no background jobs").to_string();
    }
    let mut s = crate::tf!("{} background job(s):\n", infos.len());
    for i in infos {
        s.push_str(&crate::tf!(
            "- {}  {}  {} s  pid {}\n  cmd: {}\n  log: {}\n",
            i.id,
            i.state_label(),
            i.age_secs,
            i.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
            i.command,
            i.log.display()
        ));
    }
    s
}

fn dropped_notice(expired: &[String]) -> String {
    if expired.is_empty() {
        String::new()
    } else {
        crate::tf!(
            "note: lifetime limit reached; stopped {}.\n",
            expired.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_host() -> bool {
        for rel in ["target/debug/aox.exe", "target/release/aox.exe"] {
            let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
            if p.is_file() {
                crate::shell::runner::use_test_host(p);
                return true;
            }
        }
        false
    }

    async fn runtime(dir: &std::path::Path) -> Arc<Runtime> {
        let mut cfg = crate::config::Config::dummy();
        cfg.workplace = crate::config::WorkplaceConfig {
            mode: "fixed".into(),
            root: Some(dir.to_string_lossy().into_owned()),
        };
        Runtime::new(cfg, dir.to_path_buf(), dir.join("plugins")).expect("runtime")
    }

    /// The whole point of the feature: a process survives the `start` call, its log is readable,
    /// and `stop` removes the record.
    #[tokio::test]
    async fn a_background_job_outlives_its_start_call_and_stop_ends_it() {
        if !cfg!(windows) || !test_host() {
            return;
        }
        let dir = std::env::temp_dir().join("aox-shell-job-start-stop");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rt = runtime(&dir).await;

        let out = dispatch(
            &json!({
                "action": "start",
                "name": "probe",
                "command": "ping -n 20 127.0.0.1"
            }),
            &rt,
        )
        .await
        .expect("the background command should start");
        assert!(out.contains("started background job"), "{out}");

        let (id, pid) = {
            let mut jobs = rt.jobs.lock().await;
            let infos = jobs.status(None).expect("status");
            assert_eq!(infos.len(), 1);
            assert!(infos[0].running, "{}", infos[0].state_label());
            (infos[0].id.clone(), infos[0].pid)
        };
        assert!(pid.is_some(), "a started job must expose its pid");

        // Let it actually write, then read the log through the tool.
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let logs = dispatch(&json!({"action": "logs", "id": id, "max_bytes": 4000}), &rt)
            .await
            .expect("logs");
        assert!(logs.contains("--- log ---"), "{logs}");
        assert!(logs.contains(&id), "{logs}");

        // A separate one-shot shell call runs its own Job and must not disturb this one. This is
        // the exact sequence the model needs: start server, then curl/echo in later calls.
        let shell = crate::tool::shell::shell_tools(&rt.config.shell)[0].clone();
        let one_shot = shell
            .call(json!({"command": "echo while-the-job-runs"}), &rt)
            .await;
        assert!(!one_shot.is_error, "{}", one_shot.content);
        assert!(
            rt.jobs
                .lock()
                .await
                .status(Some(&id))
                .expect("still registered")[0]
                .running,
            "the background job must survive an unrelated one-shot call"
        );

        let stopped = dispatch(&json!({"action": "stop", "id": id}), &rt)
            .await
            .expect("stop");
        assert!(stopped.contains("stopped job"), "{stopped}");
        assert!(rt.jobs.lock().await.status(None).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real HTTP server: the port answers through ordinary tool calls, and `stop` really closes
    /// it. This is the Vite-shaped case (a process that starts, listens, and never exits).
    #[tokio::test]
    async fn a_background_http_server_serves_until_stopped() {
        use std::net::{TcpListener, TcpStream};
        use std::time::Instant;

        if !cfg!(windows) || !test_host() {
            return;
        }
        if std::process::Command::new("python")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        // Ask the OS for a free port, then release it for Python to bind.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dir = std::env::temp_dir().join("aox-shell-job-http");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rt = runtime(&dir).await;

        let started = dispatch(
            &json!({
                "action": "start",
                "name": "http",
                "command": format!("python -m http.server {} --bind 127.0.0.1", port)
            }),
            &rt,
        )
        .await
        .expect("the HTTP server should start");
        assert!(started.contains("started background job"), "{started}");

        let live = |deadline: Instant| {
            let mut ok = false;
            while Instant::now() < deadline {
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    ok = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            ok
        };
        assert!(
            live(Instant::now() + Duration::from_secs(4)),
            "the server should be listening on {port}"
        );

        let id = rt.jobs.lock().await.ids().into_iter().next().unwrap();
        dispatch(&json!({"action": "stop", "id": id}), &rt)
            .await
            .expect("stop");
        let mut closed = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_err() {
                closed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(closed, "stop must close the listening socket");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A command that exits immediately is not a server. The start call must say why (including the
    /// log) instead of handing back an id for a dead job.
    #[tokio::test]
    async fn a_command_that_exits_at_once_is_reported_as_a_failure() {
        if !cfg!(windows) || !test_host() {
            return;
        }
        let dir = std::env::temp_dir().join("aox-shell-job-early-exit");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rt = runtime(&dir).await;

        let e = dispatch(
            &json!({"action": "start", "command": "echo one-shot"}),
            &rt,
        )
        .await
        .expect_err("an immediately exiting command is not a background job");
        assert!(e.contains("exited immediately"), "{e}");
        assert!(rt.jobs.lock().await.status(None).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
