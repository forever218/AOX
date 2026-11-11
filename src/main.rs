//! The command-line front end.
//! Silenced at the crate root rather than item by item, because the two targets are the same source: anything...
#![allow(dead_code)]

mod bootstrap;
mod agent;
mod agent_loop;
mod attach;
mod auth;
mod checks;
mod compact;
mod config;
mod history;
mod hook;
mod i18n;
mod jobs;
mod llm;
mod message;
mod modelbook;
mod notes;
mod plugin;
mod prompt;
mod provider_live;
mod recovery;
mod recycle;
// Host side of AOX Remote. Declared here as well as in `lib.rs` for the usual reason: this bin
// compiles its own copy of every module.
mod remote;
mod repeat;
mod runtime;
mod runtimes;
mod sandbox;
mod selftest;
// The built-in command line (`shell`): the executor, the low-integrity helper entry, and the integrity-level...
mod shell;
// The space registry's tree-building half (`group`, `Group`, the writers) exists for the GUI's history panel...
mod space;
mod store;
// Task checkpoints (`src/task.rs`): the CLI has no surface that shows one, but the agent writes and restores them...
mod task;
mod tool;
mod ui;
mod web;
mod workplace;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::agent::Agent;
use crate::runtime::Runtime;
use crate::ui::cli::CliUi;
use crate::ui::{AgentEvent, Ui};

/// When run by double-clicking the exe (no terminal), wait for a keypress after an
/// error before exiting, so the window doesn't flash by.
fn pause_before_exit() {
    use std::io::Write as _;
    print!("{}", crate::i18n::t("\nPress Enter to exit…"));
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
}

/// Whether stdin is a TTY; the wizard is only for a person present, since prompting a pipe would hang scripts.
fn stdin_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Ask one line, empty input takes the default; EOF returns an empty string, which the caller treats as skip.
fn ask(label: &str, default: &str) -> String {
    use std::io::Write as _;
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default}]: ");
    }
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    if std::io::stdin().read_line(&mut buf).unwrap_or(0) == 0 {
        return String::new();
    }
    let v = buf.trim();
    if v.is_empty() {
        default.to_string()
    } else {
        v.to_string()
    }
}

/// First-run wizard: endpoint/key/model to .aox/models.json (also GUI's); empty key skips, startup unblocked.
fn first_run_setup(app_root: &Path) -> Result<()> {
    println!();
    println!(
        "{}",
        crate::i18n::t(
            "AOX needs a model before it can answer. Three questions (all editable later with /model)."
        )
    );
    println!();

    let base_url = ask(crate::i18n::t("API endpoint").as_ref(), "https://api.deepseek.com");
    if base_url.trim().is_empty() {
        println!(
            "{}",
            crate::i18n::t("No endpoint given; skipping. You can add one later with /model.")
        );
        return Ok(());
    }
    let key = ask(crate::i18n::t("API key (visible as you type)").as_ref(), "");
    if key.trim().is_empty() {
        println!(
            "{}",
            crate::i18n::t("No key given; skipping model setup. Add one later with /model.")
        );
        return Ok(());
    }
    let model = ask(crate::i18n::t("Model name").as_ref(), "deepseek-chat");
    if model.trim().is_empty() {
        println!("{}", crate::i18n::t("No model name given; skipping."));
        return Ok(());
    }

    let id = format!(
        "m{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let mut rec = crate::modelbook::ModelRecord::blank(id.clone());
    rec.name = model.trim().to_string();
    rec.base_url = base_url.trim().to_string();
    rec.api_key = key.trim().to_string();
    rec.model = model.trim().to_string();

    let mut book = crate::modelbook::Book::load(app_root);
    book.upsert(rec);
    book.set_active(&id);
    book.save(app_root)?;

    println!();
    println!("{}", crate::tf!("Saved: {} @ {}", model.trim(), base_url.trim()));
    println!(
        "{}",
        crate::tf!("Stored in {}", app_root.join(".aox").join("models.json").display())
    );
    println!();
    Ok(())
}

/// Resolve the "application root".
fn resolve_app_root(exe_dir: Option<&Path>) -> PathBuf {
    let Some(exe_dir) = exe_dir else {
        return PathBuf::from(".");
    };
    let mut cur: Option<&Path> = Some(exe_dir);
    while let Some(d) = cur {
        if d.join("Cargo.toml").exists() {
            return d.to_path_buf();
        }
        cur = d.parent();
    }
    exe_dir.to_path_buf()
}

/// Migrate plugins from the old location (under the exe directory) to the canonical plugins directory...
fn migrate_plugins(from: &Path, to: &Path) {
    if from == to || !from.is_dir() {
        return;
    }
    let mut failed = 0usize;
    if let Ok(entries) = std::fs::read_dir(from) {
        for e in entries.flatten() {
            let src = e.path();
            if !src.is_dir() {
                continue;
            }
            if e.file_name() == "sdk" {
                continue; // sdk is rewritten by the host on every startup; don't migrate it
            }
            // Only migrate structurally complete plugins (those with plugin.toml);
            // skip empty directories left behind by a previous failure
            if !src.join("plugin.toml").is_file() {
                continue;
            }
            let dst = to.join(e.file_name());
            if dst.exists() {
                continue;
            }
            if copy_dir(&src, &dst).is_err() {
                failed += 1;
            }
        }
    }
    let _ = std::fs::create_dir_all(to);
    if failed == 0 && to.is_dir() && from.components().any(|c| c.as_os_str() == "target") {
        let _ = std::fs::remove_dir_all(from);
    }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&s, &d)?;
        } else if ty.is_file() {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

/// Whether a directory is writable (determined with a probe file).
fn is_writable_dir(dir: &Path) -> bool {
    let probe = dir.join(".aox-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Ensure the application root is writable.
fn ensure_writable_root(root: PathBuf) -> PathBuf {
    if is_writable_dir(&root) {
        return root;
    }
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return root;
    };
    let alt = PathBuf::from(local).join("AOX");
    if std::fs::create_dir_all(&alt).is_ok() && is_writable_dir(&alt) {
        println!("{}", crate::tf!("Hint: {} is not writable, switching to {}", root.display(), alt.display()));
        alt
    } else {
        root
    }
}

/// The `/runtime` subcommand: inspect status / install a runtime.
async fn runtimes_command(rt: &Arc<Runtime>, line: &str) -> Result<String> {
    match runtimes::parse_cmd(line)? {
        runtimes::Cmd::Status => Ok(runtimes::status_report(&rt.root, &rt.config.runtimes)),
        runtimes::Cmd::Dir => Ok(rt.root.join("runtime").display().to_string()),
        runtimes::Cmd::Install(lang) => {
            // Print progress straight to the screen (installation may download 10 MB;
            // we can't stay silent)
            let mut sink = |m: &str| println!("  {}", ui::style::dim(m));
            runtimes::install(rt, lang, &mut sink).await
        }
    }
}

/// The `/history` subcommand: inspect / switch / delete historical conversations.
fn history_command(agent: &mut Agent, line: &str, store: &dyn store::Store) -> Result<String> {
    Ok(match history::parse_cmd(line)? {
        history::Cmd::List => history::list_text(
            &store.list(history::DEFAULT_LIST_LIMIT),
            store.location(),
            agent.history_id(),
        ),
        // Say which backend is in use as well: once sqlite really exists, seeing this line tells the user whether...
        history::Cmd::Dir => crate::tf!(
            "{} (backend {}; every conversation of this install lives here)",
            store.location().display(),
            store.backend()
        ),
        history::Cmd::Show(key) => {
            let s = store.resolve(&key)?;
            let items = store.load(&s.id)?;
            crate::tf!(
                "{}  {} ({} messages)\n{}",
                s.time,
                s.title,
                s.messages,
                history::transcript_text(&items)
            )
        }
        history::Cmd::Open(key) => {
            let s = store.resolve(&key)?;
            let n = agent.resume(&s.id)?;
            crate::tf!(
                "Switched to \"{}\" ({} messages). Further turns will keep appending to the same file.\n  {}",
                s.title,
                n,
                s.path.display()
            )
        }
        history::Cmd::Remove(key) => {
            let s = store.remove(&key)?;
            crate::tf!("Deleted: {}  {}", s.time, s.title)
        }
    })
}

/// Handle to the REPL's background remote service (stop/wait only); details live in remote::serve_headless.
struct RemoteJob {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Hidden shell-helper entry: must be first ---- The host spawns itself with `--aox-shell-exec` to run one command...
    if let Some(code) = shell::helper::run_if_requested() {
        std::process::exit(code);
    }
    let args: Vec<String> = std::env::args().collect();
    ui::style::init(); // Probe terminal capabilities: enable ANSI colours + UTF-8

    // Application root: `cargo run` locates the directory holding Cargo.toml; double-clicking the exe uses the exe...
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let app_root = ensure_writable_root(resolve_app_root(exe_dir.as_deref()));
    let plugins_dir = args
        .windows(2)
        .find(|w| w[0] == "--plugins")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| app_root.join("plugins"));
    if let Some(d) = exe_dir.as_ref() {
        migrate_plugins(&d.join("plugins"), &plugins_dir);
    }
    plugin::ensure_sdk(&plugins_dir)?;

    // ---- selftest subcommand: verify the plugin pipeline without depending on the LLM
    if args.iter().any(|a| a == "selftest") {
        return selftest::run(app_root, plugins_dir).await;
    }

    // Load config (anchored to the app root, so a different CWD can't make it read/write an unexpected location...
    let config_path = args
        .windows(2)
        .find(|w| w[0] == "--config")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| app_root.join("config.toml"));

    // Composition manifest (phase 5): the config is composed from.
    let manifest_arg = args
        .windows(2)
        .find(|w| w[0] == "--manifest")
        .map(|w| PathBuf::from(&w[1]));
    let profile_arg = args.windows(2).find(|w| w[0] == "--profile").map(|w| w[1].clone());

    // `--dump-config`: just print the layer stack and the composed tree, then exit.
    if args.iter().any(|a| a == "--dump-config") {
        match config::layer::compose(
            &app_root,
            &config_path,
            manifest_arg.as_deref(),
            profile_arg.as_deref(),
        ) {
            Ok(c) => println!("{}", c.dump_text()),
            Err(e) => println!("{}", crate::tf!("[config error] {}", format!("{e:#}"))),
        }
        return Ok(());
    }

    // --setup forces the wizard (no JSON edits, automation-testable); placed first for the first-run branch.
    let force_setup = args.iter().any(|a| a == "--setup");

    let resolved = match config::layer::resolve(
        &app_root,
        &config_path,
        manifest_arg.as_deref(),
        profile_arg.as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => {
            if !config_path.exists() {
                // First run: auto-generate the template, so double-clicking the exe
                // doesn't just flash a window
                match std::fs::write(&config_path, config::TEMPLATE) {
                    Ok(()) => {
                        println!("{}", crate::tf!("No config file found; created a template for you: {}", config_path.display()));
                        if !stdin_is_tty() && !force_setup {
                            // For scripts and pipes, keep the old behavior: never touch the input, just state the next step clearly.
                            println!("{}", crate::i18n::t("Open it in an editor, fill in base_url and api_key (or api_key_env), then restart this program."));
                            pause_before_exit();
                            return Ok(());
                        }
                        // Interactive terminal: ask endpoint/key/model into .aox/models.json, not an editor (not in config.toml).
                        println!("{}", crate::i18n::t("The template has no model section any more; let's fill one in here."));
                    }
                    Err(werr) => {
                        println!("{}", crate::tf!("Could not create config file {}: {}", config_path.display(), werr));
                        println!("{}", crate::i18n::t("Make sure the directory is writable, or use --config <path> to point at a writable location."));
                        pause_before_exit();
                        return Ok(());
                    }
                }
            } else {
                println!("{}", crate::tf!("[config error] {}", format!("{e:#}")));
                pause_before_exit();
                return Ok(());
            }
            // Re-parse right after writing the template (valid by construction; if not, handle it as a config error).
            match config::layer::resolve(
                &app_root,
                &config_path,
                manifest_arg.as_deref(),
                profile_arg.as_deref(),
            ) {
                Ok(r) => r,
                Err(e) => {
                    println!("{}", crate::tf!("[config error] {}", format!("{e:#}")));
                    pause_before_exit();
                    return Ok(());
                }
            }
        }
    };
    // `/config` must show people "which layers composed this config", so keep the short
    // summary around first — the config itself is taken by Runtime right after.
    let layer_summary = resolved.composed.layer_summary();
    let config = resolved.config;

    // First-run setup: ask for endpoint / key / model, write `.aox/models.json`.
    if force_setup
        || (stdin_is_tty()
            && crate::modelbook::Book::load(&app_root).active_record().is_none()
            && !config.provider.is_configured())
    {
        first_run_setup(&app_root)?;
    }

    // Assemble the runtime: meta tools + restore persisted plugins A wrong `store backend` fails here.
    let rt = match Runtime::new(config, app_root.clone(), plugins_dir) {
        Ok(rt) => rt,
        Err(e) => {
            println!("{}", crate::tf!("[config error] {}", format!("{e:#}")));
            pause_before_exit();
            return Ok(());
        }
    };
    bootstrap::register_tools(&rt).await?;

    // `aox remote...`: the host side of AOX Remote ---- Placed after every tool is registered, so a remote turn has...
    if args.get(1).map(String::as_str) == Some("remote") {
        return crate::remote::cli(rt.clone(), app_root.clone(), &args[2..], true).await;
    }

    let mut ui = CliUi::new();

    // Print the banner before restoring plugins.
    println!("\n{}", ui::style::logo());
    println!(
        "  {} {}",
        ui::style::dim(crate::i18n::t("Model")),
        // The live provider, not `rt.config.provider`: since the model is chosen in `.aox/models.json`, the config...
        ui::style::bold(&rt.provider.cfg().model)
    );
    ui.emit(AgentEvent::Info(crate::tf!(
        "Config {}   plugins dir {}",
        config_path.display(),
        rt.plugins_dir.display()
    )));
    ui.emit(AgentEvent::Info(crate::i18n::t("Type /help to see all commands").into()));
    ui.emit(AgentEvent::Info(crate::tf!(
        "Project home {}",
        // OSC 8 hyperlinks: clickable in Windows Terminal / VS Code etc.;
        // old conhost doesn't support it and falls back to a plain-text URL
        ui::style::link(ui::style::PROJECT_URL, ui::style::PROJECT_URL)
    )));

    // Restore persisted plugins. Silent on success (the banner above is already on screen),
    // speak up only on failure. To see loaded plugins, use /plugins.
    let load_results = plugin::load_all(&rt).await;
    for (name, result) in &load_results {
        if let Err(e) = result {
            ui.emit(AgentEvent::Warn(crate::tf!("plugin {} failed to load: {}", name, format!("{e:#}"))));
        }
    }
    // Plugins whose fence isn't fitted must speak up — this is the classic "thinking there's protection when there...
    let unfenced = sandbox::unfenced(&rt).await;
    if !unfenced.is_empty() {
        ui.emit(AgentEvent::Warn(format!(
            "These plugins are not wrapped in a process fence; they are not subject to the process / memory / UI limits: {}",
            unfenced.join(crate::i18n::t("; "))
        )));
    }

    // The provider was built by `Runtime::new` and lives on the runtime (see `src/provider_live.rs`), so "not ready"...
    if let Some(reason) = rt.provider.error() {
        ui.emit(AgentEvent::Error(crate::tf!("LLM not ready: {}", reason)));
        ui.emit(AgentEvent::Info(
            crate::i18n::t("Set base_url / api_key in config.toml (or point api_key_env at an environment variable), or configure it in the GUI's Settings -> Model; local commands /tools /plugins /demo still work.")
                .into(),
        ));
    }

    // The project rules file (RULES.md) is no longer read here: it is now the `rules` segment of the prompt, taken...
    let mut agent = Agent::new(rt.clone());

    // Runtime hint: only an extra line when Python is missing; when runtimes are
    // complete the startup output stays clean.
    if let Some(hint) = runtimes::missing_hint(&rt.root, &rt.config.runtimes) {
        ui.emit(AgentEvent::Warn(hint));
    }

    // REPL.
    // `pending` is the command-line version of the pending-upload tray: `/attach <path>` attaches, and the next...
    let mut pending: Vec<PathBuf> = Vec::new();
    // Service from /remote start lives outside the loop so exit can ask it to disconnect; a raw drop cuts the line.
    let mut remote: Option<RemoteJob> = None;
    loop {
        let Some(line) = ui.read_user_input().await? else { break };
        let line = line.trim();
        // Attachments alone, with no text, still count as a message (consistent with the GUI)
        if line.is_empty() && pending.is_empty() {
            continue;
        }
        if line == "/attach" || line.starts_with("/attach ") {
            let rest = line["/attach".len()..].trim();
            attach_command(&mut pending, rest, &mut ui);
            continue;
        }
        // Subcommands match aox remote; only start is backgrounded in the REPL (blocking would surrender the terminal).
        if line == "/remote" || line.starts_with("/remote ") {
            let rest = line["/remote".len()..].trim();
            let args: Vec<String> = rest.split_whitespace().map(String::from).collect();
            match args.first().map(String::as_str) {
                None | Some("status") => {
                    // Running comes from the task: Some only means started; it may have exited on network loss or GUI reclaim.
                    let running = remote.as_ref().is_some_and(|j| !j.handle.is_finished());
                    ui.emit(AgentEvent::Info(crate::remote::brief(&app_root, running).into()));
                }
                Some("start") | Some("serve") => {
                    if remote.as_ref().is_some_and(|j| !j.handle.is_finished()) {
                        ui.emit(AgentEvent::Info(
                            "The remote service is already running (run /remote stop first)".into(),
                        ));
                    } else {
                        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let handle = tokio::spawn({
                            let rt = rt.clone();
                            let root = app_root.clone();
                            let stop = stop.clone();
                            async move {
                                if let Err(e) = crate::remote::serve_headless(rt, root, stop).await {
                                    eprintln!("{}", crate::tf!("remote service exited: {}", format!("{e:#}")));
                                }
                            }
                        });
                        remote = Some(RemoteJob { stop, handle });
                        ui.emit(AgentEvent::Info(
                            "Remote service started in the background. /remote stop stops it.".into(),
                        ));
                    }
                }
                Some("stop") => {
                    let live = remote.as_ref().is_some_and(|j| !j.handle.is_finished());
                    if !live {
                        remote = None;
                        ui.emit(AgentEvent::Info("The remote service is not running.".into()));
                    } else if let Some(job) = remote.take() {
                        job.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        ui.emit(AgentEvent::Info(
                            "Stop requested (it disconnects within 5 seconds)".into(),
                        ));
                    }
                }
                // Pairing prompts read stdin and would fight the line editor, so the REPL must supply all args at once.
                Some("pair") if args.len() < 3 => {
                    ui.emit(AgentEvent::Info(
                        "In the REPL, give everything at once: /remote pair <server address> <pairing code> [agent name]"
                            .into(),
                    ));
                }
                // All other subcommands reuse aox remote directly; duplicating them in two places would only drift apart.
                Some(_) => {
                    if let Err(e) = crate::remote::cli(rt.clone(), app_root.clone(), &args, false).await {
                        ui.emit(AgentEvent::Error(format!("{e:#}")));
                    }
                }
            }
            continue;
        }
        match line {
            "/quit" | "/exit" => break,
            "/tools" => {
                ui.emit(AgentEvent::Info(crate::tf!("Tools: {}", rt.tool_names().await.join(", "))));
            }
            "/plugins" => {
                // Snapshot the slots, then drop the plugin table lock: `stat` takes each slot's own
                // lock and awaits, and the table has to stay usable while this prints.
                let slots: Vec<(String, std::sync::Arc<crate::plugin::PluginHandle>)> = {
                    let plugins = rt.plugins.lock().await;
                    let mut v: Vec<_> = plugins.iter().map(|(n, h)| (n.clone(), h.clone())).collect();
                    v.sort_by(|a, b| a.0.cmp(&b.0));
                    v
                };
                if slots.is_empty() {
                    ui.emit(AgentEvent::Info(crate::i18n::t("(no plugins loaded)").into()));
                } else {
                    // Each slot prints which of its two states it is in.
                    let mut running = 0usize;
                    let listed: Vec<String> = {
                        let mut v = Vec::new();
                        for (name, h) in &slots {
                            let st = h.stat().await;
                            if st.running {
                                running += 1;
                            }
                            v.push(crate::tf!("{} ({})", name, st.label()));
                        }
                        v
                    };
                    ui.emit(AgentEvent::Info(crate::tf!("Plugins: {}", listed.join(", "))));
                    // The mode line: without it, "everything says dormant" is ambiguous — it could be
                    // lazy loading working, or a reaper that never ran.
                    let mode = if rt.config.plugins.lazy {
                        crate::tf!(
                            "lazy loading on; processes are closed after {}s idle",
                            rt.config.plugins.idle_timeout_secs
                        )
                    } else {
                        crate::i18n::t("lazy loading off; plugin processes stay resident").to_string()
                    };
                    ui.emit(AgentEvent::Info(crate::tf!(
                        "Plugin slots: {} · running: {} · {}",
                        slots.len(),
                        running,
                        mode
                    )));
                    // Component contributions (phase 6 · 6b): only list those that have contributions — old plugins that only provide...
                    let mut extra: Vec<String> = Vec::new();
                    for (n, h) in &slots {
                        let c = &h.contributes;
                        if c.is_empty() {
                            continue;
                        }
                        let mut parts: Vec<String> = Vec::new();
                        if !c.hooks.is_empty() {
                            parts.push(crate::tf!("hooks {}", c.hooks.join("/")));
                        }
                        if !c.prompt_segments.is_empty() {
                            parts.push(crate::tf!("prompt segments {}", c.prompt_segments.join("/")));
                        }
                        if !c.caps.is_empty() {
                            parts.push(crate::tf!("requested caps {}", c.caps.join("/")));
                        }
                        if c.theme {
                            // Also read the patch itself: declaring it but being None
                            // means retrieval or validation didn't pass
                            parts.push(if h.theme_patch.is_some() {
                                crate::i18n::t("theme patch ✓ (GUI merge not wired up yet)").to_string()
                            } else {
                                crate::i18n::t("theme patch (not retrieved)").to_string()
                            });
                        }
                        extra.push(format!("  {n} → {}", parts.join(crate::i18n::t("; "))));
                    }
                    if !extra.is_empty() {
                        ui.emit(AgentEvent::Info(crate::tf!("Component contributions:\n{}", extra.join("\n"))));
                    }
                }
            }
            "/demo" => {
                crate::ui::cli::print_demo();
                ui.turn_start().await;
                ui.set_status(crate::i18n::t("Thinking")).await;
                tokio::time::sleep(std::time::Duration::from_millis(900)).await;
                ui.emit(AgentEvent::ToolCall {
                    name: "fs_read".into(),
                    args: serde_json::json!({"path": "config.toml"}),
                });
                ui.emit(AgentEvent::ToolRunning {
                    name: "fs_read".into(),
                    timeout_secs: 120,
                    expect_secs: Some(1),
                });
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                ui.emit(AgentEvent::ToolResult {
                    name: "fs_read".into(),
                    is_error: false,
                    preview: "base_url = \"https://api.deepseek.com/v1\"".into(),
                    diff: None,
                    diff_detail: None,
                    path: None,
                });
                ui.emit(AgentEvent::AssistantText(
                    crate::i18n::t("Demo finished: the tool-call trace above has been collapsed; type **/expand** to expand it.").into(),
                ));
                ui.turn_end().await;
            }
            "/expand" => ui.expand_last_turn(),
            "/usage" => ui.show_usage(agent.usage_log()),
            "/prompt" => ui.emit(AgentEvent::Info(prompt::report(&rt))),
            "/hook" => ui.emit(AgentEvent::Info(hook::report(&rt))),
            "/loop" => ui.emit(AgentEvent::Info(agent_loop::report(&rt))),
            // The checkpoint is the one thing the CLI had no window onto at all (see the note at the
            // top of this file). It lives on the `Agent`, not in config, so it is read from there.
            "/task" => match agent.task() {
                Some(st) => ui.emit(AgentEvent::Info(crate::task::report(st))),
                None => ui.emit(AgentEvent::Info(crate::i18n::t(
                    "no task checkpoint in this conversation yet — the model writes one with task_state on long tasks (goal, plan, acceptance).",
                ).to_string())),
            },
            _ if recovery::command::is_cmd(line) => {
                match recovery::command::parse(line)
                    .map_err(anyhow::Error::msg)
                    .and_then(|cmd| recovery::command::run(&rt, cmd).map_err(anyhow::Error::msg))
                {
                    Ok(rep) => {
                        ui.emit(AgentEvent::Info(rep.text));
                        if let Some(report) = rep.restore {
                            agent.note_workspace_restored(&report);
                        }
                    }
                    Err(e) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
                }
            }
            "/sandbox" => ui.emit(AgentEvent::Info(sandbox::report(&rt))),
            // Workplace: `/workplace` shows the current state, `/workplace full|default|<dir>` switches tiers.
            _ if workplace::is_cmd(line) => match workplace::command(&rt, line) {
                Ok(text) => {
                    ui.emit(AgentEvent::Info(text));
                    // The tier must be recorded into this conversation's save too.
                    agent.persist_workplace();
                }
                Err(e) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
            },
            "/config" => ui.emit(AgentEvent::Info(layer_summary.clone())),
            // `/model` — the saved models and switching between them.
            _ if modelbook::is_cmd(line) => match modelbook::command(&rt, line) {
                Ok(text) => ui.emit(AgentEvent::Info(text)),
                Err(e) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
            },
            "/shell" => ui.emit(AgentEvent::Info(tool::shell::report(&rt))),
            "/web" => ui.emit(AgentEvent::Info(tool::web::report(&rt))),
            "/new" => {
                // The same path as the "+" button on the GUI top bar.
                agent.new_conversation();
                ui.emit(AgentEvent::Info(
                    crate::i18n::t("Started a new conversation. The previous one has been saved under history/ (see /history).").into(),
                ));
            }
            _ if runtimes::is_cmd(line) => {
                match runtimes_command(&rt, line).await {
                    Ok(text) => ui.emit(AgentEvent::Info(text)),
                    Err(e) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
                }
            }
            _ if history::is_cmd(line) => {
                match history_command(&mut agent, line, &*rt.store) {
                    Ok(text) => ui.emit(AgentEvent::Info(text)),
                    Err(e) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
                }
            }
            "/help" => {
                println!();
                for (cmd, desc) in [
                    ("/help", crate::i18n::t("Show this help")),
                    ("/tools", crate::i18n::t("List all currently available tools")),
                    ("/plugins", crate::i18n::t("List loaded plugins and what each contributes (hooks / prompt segments / capabilities)")),
                    ("/runtime", crate::i18n::t("Show plugin runtime status; /runtime install python installs a bundled runtime")),
                    (
                        "/history",
                        crate::i18n::t("Historical conversations; list to list, show <n> to read in full, open <n> to continue, rm <n> to delete"),
                    ),
                    ("/new", crate::i18n::t("Start a new conversation (the current one is saved to history automatically)")),
                    (
                        "/attach",
                        crate::i18n::t("Stage a local file in the pending-upload tray to send with the next message; /attach clear removes all"),
                    ),
                    ("/demo", crate::i18n::t("Preview the terminal layout and the collapsed-trace effect")),
                    ("/expand", crate::i18n::t("Expand the most recent collapsed tool-call trace")),
                    ("/usage", crate::i18n::t("Review the token usage breakdown of the last task")),
                    ("/prompt", crate::i18n::t("Show how the system prompt is assembled from segments (config.toml's [prompt] can reorder / disable / add)")),
                    ("/hook", crate::i18n::t("Show the active loop hooks (from config.toml's [hook] and plugins' [contributes])")),
                    (
                        "/loop",
                        crate::i18n::t("Show the main-loop implementation, context trimming and the per-turn request ceiling (config.toml's [loop]: impl swaps the implementation, max_rounds caps how much context is kept, max_iters_per_turn caps how many requests one turn may spend)"),
                    ),
                    (
                        "/task",
                        crate::i18n::t("Show this conversation's task checkpoint: the goal, the plan with its ticks, and the acceptance criteria"),
                    ),
                    (
                        "/checkpoints",
                        crate::i18n::t("List task-level file recovery points in this workspace"),
                    ),
                    (
                        "/rollback",
                        crate::i18n::t("Preview the previous task's file changes; add --yes to restore, or a count / checkpoint id before the flags"),
                    ),
                    (
                        "/redo",
                        crate::i18n::t("Undo the most recent file rollback using its safety snapshot"),
                    ),
                    (
                        "/backup",
                        crate::i18n::t("Recovery status: /backup status | verify | prune (apply retention and free unused space)"),
                    ),
                    (
                        "/sandbox",
                        crate::i18n::t("Show the fence around plugin subprocesses (config.toml's [sandbox]: process / memory / UI limits; it also spells out what it does not cover)"),
                    ),
                    (
                        "/shell",
                        crate::i18n::t("Show the built-in command line: dialects, boundary, limits (config.toml's [shell])"),
                    ),
                    (
                        "/web",
                        crate::i18n::t("Show the built-in fetch tool: reach, budget, timeout (config.toml's [web])"),
                    ),
                    (
                        "/workplace",
                        crate::i18n::t("Show / switch the model's file boundary; /workplace full|default|<dir> changes the tier (config.toml's [workplace])"),
                    ),
                    (
                        "/config",
                        crate::i18n::t("Show which layers this config is composed of (aox.toml under the app root can customize it; --dump-config shows where each value comes from)"),
                    ),
                    (
                        "/model",
                        crate::i18n::t("List the saved models; /model <number|name> switches to one (the same list the GUI edits; effective on the next request)"),
                    ),
                    (
                        "/remote",
                        crate::i18n::t("Phone bridge in this session: /remote start|stop; list / use / pair / policy / remove work like `aox remote`"),
                    ),
                    ("/quit", crate::i18n::t("Quit the program (/exit is equivalent)")),
                ] {
                    println!(
                        "  {}  {}",
                        ui::style::cyan(&format!("{cmd:<10}")),
                        ui::style::dim(desc)
                    );
                }
                println!(
                    "  {}",
                    ui::style::dim(crate::i18n::t("Anything else is sent straight to the model; press Ctrl+C to interrupt a running task"))
                );
                println!();
            }
            _ => {
                let mark = agent.messages_len();
                enum TurnOutcome {
                    Done(Result<()>),
                    Interrupted,
                }
                // Inside a select! branch we can't touch the agent/ui borrowed by the
                // turn; collect the result first, then handle it
                let outcome = {
                    let turn = agent.run_turn(&mut ui, line, &pending);
                    tokio::pin!(turn);
                    tokio::select! {
                        r = &mut turn => TurnOutcome::Done(r),
                        _ = tokio::signal::ctrl_c() => TurnOutcome::Interrupted,
                    }
                };
                match outcome {
                    // Clear on send (one-shot): the attachments have gone into the history with
                    // this message, and must not ride along a second time with the next one.
                    TurnOutcome::Done(_) => pending.clear(),
                    // On interrupt this now hinges on a single fact: did this turn's user message actually make it into the history?...
                    TurnOutcome::Interrupted => {
                        if agent.messages_len() > mark {
                            pending.clear();
                        }
                    }
                }
                match outcome {
                    TurnOutcome::Done(Ok(())) => {}
                    TurnOutcome::Done(Err(e)) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
                    TurnOutcome::Interrupted => {
                        ui.abort_turn().await;
                        // Roll back to the last legal boundary, not to the start of the turn: the user's instruction and every completed...
                        agent.rollback_interrupted_turn(mark);
                        ui.emit(AgentEvent::Warn(
                            crate::i18n::t("Interrupted the current task (only the step that was still running was dropped; everything already done stays in context)").into(),
                        ));
                    }
                }
            }
        }
    }

    // On shutdown ask the service to disconnect, wait at most 6s, then give up: the user already quit.
    if let Some(job) = remote.take() {
        job.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(6), job.handle).await;
    }

    // Shut down all plugin processes before exiting.
    let handles: Vec<_> = rt.plugins.lock().await.values().cloned().collect();
    for h in handles {
        h.stop().await;
    }
    Ok(())
}

/// `/attach` — the command-line version of the pending-upload tray.
fn attach_command(pending: &mut Vec<PathBuf>, rest: &str, ui: &mut CliUi) {
    if rest.is_empty() {
        if pending.is_empty() {
            ui.emit(AgentEvent::Info(
                crate::i18n::t("The pending-upload tray is empty. Usage: /attach <file path> [more paths…] (quote paths containing spaces in full)").into(),
            ));
            return;
        }
        let list =
            pending.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n  ");
        ui.emit(AgentEvent::Info(crate::tf!(
            "Pending upload ({}):\n  {}\nThe next message will carry them.",
            pending.len(),
            list
        )));
        return;
    }
    if rest == "clear" {
        pending.clear();
        ui.emit(AgentEvent::Info(crate::i18n::t("The pending-upload tray has been cleared.").into()));
        return;
    }
    // When the whole string is an existing path, treat it as one file (otherwise a path with spaces gets broken...
    let raw: Vec<String> = if std::path::Path::new(rest).exists() {
        vec![rest.to_string()]
    } else {
        rest.split_whitespace().map(str::to_string).collect()
    };
    if raw.is_empty() {
        ui.emit(AgentEvent::Warn(crate::i18n::t("No path given. Usage: /attach <file path>").into()));
        return;
    }
    let mut added = 0usize;
    let mut problems: Vec<String> = Vec::new();
    for r in raw {
        // Also strip the quoting layer the shell adds automatically when dragging into a terminal
        let r = r.trim_matches(|c| c == '"' || c == '\'').to_string();
        let path = PathBuf::from(&r);
        if pending.iter().any(|p| *p == path) {
            continue;
        }
        match attach::Attachment::probe(path.clone()) {
            Ok(_) => {
                pending.push(path);
                added += 1;
            }
            Err(e) => problems.push(e),
        }
    }
    if added > 0 {
        ui.emit(AgentEvent::Info(crate::tf!("Attached {} file(s) ({} total); they will be sent with the next message.", added, pending.len())));
    }
    for p in problems {
        ui.emit(AgentEvent::Warn(p));
    }
}
