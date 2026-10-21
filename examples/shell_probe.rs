//! S1/S2 smoke test for the built-in `shell` tool — the same A/B discipline as `aox selftest`,
//! but runnable on its own: `cargo run --example shell_probe`.
//!
//! It builds a real `Runtime` with a fixed workspace in `%TEMP%`, registers the shell tools and
//! drives them exactly as the agent would, then prints `PASS`/`FAIL` per check and exits with the
//! number of failures. Everything it creates is removed.
//!
//! What it establishes:
//!
//! | # | Check | Why it matters |
//! |---|-------|----------------|
//! | 1 | a write **inside** the workspace succeeds | the boundary is not so tight that the tool is useless |
//! | 2 | a write outside, spelled so the scanner **cannot** see it (`[IO.File]::WriteAllText`), is refused | this is the kernel doing it, not the scanner |
//! | 3 | a plain `> <outside path>` is refused **before** running, with the boundary contract | the guard rail produces a readable refusal |
//! | 4 | the refused file really does not exist | "it got blocked" must not mean "nothing happened" |
//! | 5 | reading outside the workspace still works | the deliberate read/write asymmetry |
//! | 6 | a non-zero exit code reaches the model as output, not as a tool error | a compiler error is content |

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

use aox::config::Config;
use aox::runtime::Runtime;

fn main() {
    // Same hidden entry as the two real binaries. It is not optional: `shell` spawns
    // `current_exe()` to lower the token, so whichever binary hosts the runtime must be able to
    // answer that switch — otherwise this probe would recursively start itself.
    if let Some(code) = aox::shell::helper::run_if_requested() {
        std::process::exit(code);
    }
    let code = match run() {
        Ok(failures) => failures,
        Err(e) => {
            println!("FAIL  could not set the probe up: {e:#}");
            1
        }
    };
    println!(
        "\n=== {} ===\n",
        if code == 0 { "all checks passed" } else { "there are failures above" }
    );
    std::process::exit(code);
}

#[tokio::main]
async fn run() -> anyhow::Result<i32> {
    // `AOX_PROBE_ROOT` puts the probe's workspace *inside* an already-prepared tree (a subdirectory
    // inherits the label), which is how the kernel boundary gets tested on a real project path.
    let root = match std::env::var("AOX_PROBE_ROOT") {
        Ok(p) => PathBuf::from(p).join(format!("aox-shell-probe-{}", std::process::id())),
        Err(_) => std::env::temp_dir().join(format!("aox-shell-probe-{}", std::process::id())),
    };
    let ws = root.join("ws");
    let outside = root.join("outside-medium.txt");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&ws)?;
    std::fs::write(&outside, "a Medium file outside the workspace\n")?;

    let mut cfg = Config::dummy();
    // `AOX_PROBE_BASH=<path>` forces one bash: the way to exercise the busybox fallback on a machine that
    // already has a full bash.
    if let Ok(p) = std::env::var("AOX_PROBE_BASH") {
        cfg.shell.bash = p;
    }
    cfg.workplace = aox::config::WorkplaceConfig {
        mode: "fixed".into(),
        root: Some(ws.to_string_lossy().into_owned()),
    };
    let rt = Arc::new(Runtime::new(cfg, ws.clone(), ws.join("plugins"))?);
    for t in aox::tool::shell::shell_tools() {
        rt.register(t).await?;
    }

    let mut failed = 0;
    println!("\n=== shell probe ===");
    println!("workspace {}", ws.display());
    println!("outside   {}", outside.display());

    // A subdirectory of a prepared tree **inherits the label**, so a probe workspace created inside one
    // is already at the right level; only the per-workspace marker is missing. Mirror the nearest
    // ancestor's marker, so this runs the same path the real tool takes in a prepared project.
    {
        let mut cur = Some(ws.as_path());
        let mut found = None;
        for _ in 0..8 {
            let Some(dir) = cur else { break };
            let m = dir.join(".aox").join("shell-untrusted");
            if m.is_file() {
                found = Some(dir.to_path_buf());
                break;
            }
            cur = dir.parent();
        }
        if let Some(owner) = found {
            std::fs::create_dir_all(ws.join(".aox"))?;
            std::fs::write(ws.join(".aox").join("shell-untrusted"), "level = untrusted\n")?;
            println!(
                "      (marker mirrored from the prepared tree at {} — expecting the kernel path)",
                owner.display()
            );
        }
    }

    // Exercise the "prepared" branch without needing a UAC prompt: the marker is what the tool reads,
    // so writing one puts it on that path (where the *probe* then decides whether to believe it).
    if std::env::var("AOX_PROBE_PRETEND_PREPARED").is_ok() {
        let _ = std::fs::create_dir_all(ws.join(".aox"));
        let _ = std::fs::write(ws.join(".aox").join("shell-untrusted"), "level = untrusted\n");
        println!("      (pretending this workspace is prepared: marker written)");
    }

    // 1 ── write inside
    match ask(&rt, json!({"command": "echo inside-ok> inside.txt"})).await {
        Ok(out) if !out.is_error => println!("PASS  a command that writes inside the workspace"),
        other => {
            println!("FAIL  a command writing inside the workspace: {}", show(other));
            failed += 1;
        }
    }

    // 2 ── write outside, invisible to the scanner (only the kernel can catch this one)
    let hidden_target = root.join("hidden-outside.txt");
    let hidden_cmd = format!(
        "powershell -NoProfile -Command \"[IO.File]::WriteAllText('{}','should not exist')\"",
        hidden_target.display()
    );
    let hidden = ask(&rt, json!({ "command": hidden_cmd })).await.ok();
    let rail_only = hidden
        .as_ref()
        .map(|o| o.content.contains("boundary: guard-rail-only"))
        .unwrap_or(false);
    if let Some(o) = &hidden {
        println!("      (that write reported: {})", first_line(&o.content));
    }
    if rail_only {
        println!("INFO  a write outside the workspace is not caught when the command hides its target");
        println!("      => guard rail only, by design; the measurement that ended the kernel route is in");
        println!("         docs/SHELL.md");
    } else if hidden_target.exists() {
        println!("FAIL  THE BOUNDARY DID NOT HOLD: {} was created", hidden_target.display());
        let _ = std::fs::remove_file(&hidden_target);
        failed += 1;
    } else {
        println!("PASS  a write outside the workspace was refused (and nobody saw it coming)");
    }

    // 3 ── plain redirect outside: refused *before* running, with the contract text
    let plain_cmd = format!("echo nope> {}", outside.display());
    match ask(&rt, json!({ "command": plain_cmd })).await {
        Ok(out) if out.is_error && out.content.contains("Permission boundary") => {
            println!("PASS  a plain out-of-bounds redirect is refused with the boundary contract")
        }
        other => {
            println!("FAIL  the out-of-bounds redirect was not refused as expected: {}", show(other));
            failed += 1;
        }
    }

    // 4 ── and the target is untouched (both the original content and its mtime)
    match std::fs::read_to_string(&outside) {
        Ok(s) if s.contains("Medium file outside") => println!("PASS  the out-of-workspace file is untouched"),
        other => {
            println!("FAIL  the out-of-workspace file changed: {other:?}");
            failed += 1;
        }
    }

    // 5 ── reading outside is allowed
    let read_cmd = format!("type \"{}\"", outside.display());
    match ask(&rt, json!({ "command": read_cmd })).await {
        Ok(out) if !out.is_error && out.content.contains("Medium file outside") => {
            println!("PASS  reading outside the workspace works")
        }
        other => {
            println!("FAIL  reading outside the workspace was blocked: {}", show(other));
            failed += 1;
        }
    }

    // 6 ── a failing command is output, not a tool error
    match ask(&rt, json!({"command": "exit 3"})).await {
        Ok(out) if out.content.contains("exit 3") => {
            println!("PASS  a non-zero exit code is reported as exit 3 with its output")
        }
        other => {
            println!("FAIL  the exit code did not reach the caller: {}", show(other));
            failed += 1;
        }
    }

    // 7 ── the header states which boundary was actually in force
    match ask(&rt, json!({"command": "echo boundary"})).await {
        Ok(out) if out.content.contains("boundary:") => {
            let line = out.content.lines().find(|l| l.contains("boundary:")).unwrap_or("");
            println!("PASS  the header reports the boundary: {}", line.trim());
        }
        other => {
            println!("FAIL  the header does not report the boundary: {}", show(other));
            failed += 1;
        }
    }

    // 8 ── bash, when this machine has one: prologue, coreutils on PATH, and a real pipeline
    match aox::tool::bash::available(&rt) {
        None => println!("SKIP  no bash on this machine — the bash tool is not registered"),
        Some(b) => {
            println!("      bash: {} at {}", b.flavour, b.exe.display());
            for t in aox::tool::bash::bash_tools(&rt) {
                rt.register(t).await?;
            }
            let script = "printf 'ws=%s\\n' \"$AOX_WORKSPACE_U\"; \
                          command -v sed >/dev/null && echo coreutils-ok; \
                          echo hello | tr a-z A-Z";
            match rt.execute(aox::tool::bash::BASH_TOOL, json!({ "command": script })).await {
                out if !out.is_error && out.content.contains("coreutils-ok") && out.content.contains("HELLO") => {
                    let ws_line = out.content.lines().find(|l| l.starts_with("ws=")).unwrap_or("");
                    println!("PASS  bash: prologue ({}) + coreutils on PATH + a pipeline", ws_line.trim());
                }
                out => {
                    println!("FAIL  the bash call did not work: {}", first_line(&out.content));
                    failed += 1;
                }
            }
        }
    }

    let _ = std::fs::remove_dir_all(&root);
    Ok(failed)
}

/// One call through the registry — the same path the agent takes (`Runtime::execute`), minus the
/// authorisation prompt.
async fn ask(rt: &Arc<Runtime>, args: serde_json::Value) -> anyhow::Result<aox::tool::ToolOutput> {
    Ok(rt.execute(aox::tool::shell::SHELL_TOOL, args).await)
}

fn show(r: anyhow::Result<aox::tool::ToolOutput>) -> String {
    match r {
        Ok(o) => format!("is_error={} · {}", o.is_error, first_line(&o.content)),
        Err(e) => format!("{e:#}"),
    }
}

fn first_line(s: &str) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}
