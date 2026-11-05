//! The command-line front end. See `src/lib.rs`: **both crate roots compile the same modules**, and
//! this one does not depend on the library, so every item that only the GUI or the tests use is
//! unreachable *here* — which is what `dead_code` reports, thirty times over, on a clean build.
//!
//! Silenced at the crate root rather than item by item, because the two targets are the same source:
//! anything *genuinely* dead (a private function nobody calls) is still dead in the **library** build,
//! which reports it. So the signal is not lost, it just moves to the target that can see it. The
//! alternative — an `#[allow]` on each of the GUI-only APIs — needs a new one every time a GUI feature
//! lands, and rots into a lie ("why is this allowed?").
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
// The built-in command line (`shell`): the executor, the low-integrity helper entry, and the
// integrity-level primitives. Declared here as well as in `lib.rs` because this bin compiles its
// own copy of every module (see the trap note in TRAPS.md).
mod shell;
// The space registry's tree-building half (`group`, `Group`, the writers) exists for the GUI's
// history panel; this CLI links the module only to call `Registry::load` / `workspace_roots` when it
// sets up storage. Covered by the crate-root `allow(dead_code)` above — the library build keeps the
// signal (see the note at the top of `src/space.rs`).
mod space;
mod store;
// Task checkpoints (`src/task.rs`): the CLI has no surface that shows one, but the agent writes and
// restores them regardless — "the task was interrupted, continue it" is exactly as true in a
// terminal as it is in the GUI.
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

/// 当前进程的 stdin 是不是真终端。引导只对"人坐在前面"的情况开放——
/// 在管道里问问题等于让脚本挂住。
fn stdin_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// 问一行，空输入取默认值。EOF 返回空串，调用方按"跳过"处理。
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

/// 首次运行的引导：端点 / Key / 模型名 → `.aox/models.json`。
///
/// 写的是和 GUI「设置 → 模型」同一份文件，所以两边看到的是同一个模型；之后随时
/// `/model` 可以换或加。Key 空 = 跳过，不拦着启动：本地命令和 `/remote` 都不需要模型。
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
///
/// Rule: from the exe's directory, walk upward to the nearest Cargo.toml; if found,
/// use its directory (development mode), otherwise use the exe's directory
/// (distribution mode). This way `cargo run` (exe under target/debug) and
/// double-clicking the exe both normalize to the same root, eliminating the path
/// confusion where target/debug/plugins/ and the source plugins/ coexist and the
/// model sees two plugins directories in the filesystem.
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

/// Migrate plugins from the old location (under the exe directory) to the canonical
/// plugins directory, so historical plugins don't "vanish into thin air". Only fills
/// in what's missing, never overwrites existing files; once everything migrates
/// successfully, clean up the leftover build-artifact directory (one that contains a
/// `target` path component), otherwise the next startup would "resurrect" plugins the
/// user already deleted.
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

/// Ensure the application root is writable. At runtime AOX writes config.toml and
/// plugins/ under the root; if it's installed to a read-only location like Program
/// Files, fall back to the user-writable %LOCALAPPDATA%\AOX.
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
///
/// Parsing and installation both live in `crate::runtimes`, with the CLI and GUI
/// sharing one implementation and one set of wording; this only decides "where the
/// progress and result get printed".
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
///
/// Parsing and wording both live in `crate::history`, with the CLI and GUI sharing one
/// implementation. Data is always requested from the **storage seam**
/// (`crate::store::Store`) — no file paths are assembled here anymore, so switching
/// backends (e.g. one database holding all conversations) requires no change to this
/// code. Only `open` needs the `Agent` — it swaps the context in place, the one
/// subcommand that isn't a pure query.
fn history_command(agent: &mut Agent, line: &str, store: &dyn store::Store) -> Result<String> {
    Ok(match history::parse_cmd(line)? {
        history::Cmd::List => history::list_text(
            &store.list(history::DEFAULT_LIST_LIMIT),
            store.location(),
            agent.history_id(),
        ),
        // Say which backend is in use as well: once sqlite really exists, seeing this
        // line tells the user whether their config change took effect, instead of
        // having to guess.
        //
        // The path named is the **default workspace's** directory. Conversations live per workspace
        // since 2026-10-14, so printing one path without saying so would read as "this is all there
        // is" — and send the user looking in the wrong folder for a conversation made elsewhere.
        history::Cmd::Dir => crate::tf!(
            "{} (backend {}; each workspace keeps its own conversations under <workspace>/.aox/history)",
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

/// REPL 后台跑的远程服务。只有"怎么停"和"等它停"两件事——
/// 具体协议、重连、心跳都在 `remote::serve_headless` 里，这里不重复。
struct RemoteJob {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // ---- hidden shell-helper entry: **must be first** ----
    // The host spawns itself with `--aox-shell-exec` to run one command (see `src/shell/helper.rs`:
    // lowering an integrity level only works from inside the process that owns the token). Nothing
    // else may run before this — the helper must not touch the terminal, the config or the plugins.
    if let Some(code) = shell::helper::run_if_requested() {
        std::process::exit(code);
    }
    let args: Vec<String> = std::env::args().collect();
    ui::style::init(); // Probe terminal capabilities: enable ANSI colours + UTF-8

    // Application root: `cargo run` locates the directory holding Cargo.toml;
    // double-clicking the exe uses the exe directory.
    // The plugins directory is fixed at <root>/plugins and can be overridden with
    // `--plugins <path>`.
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

    // ---- Load config (anchored to the app root, so a different CWD can't make it
    //      read/write an unexpected location; it previously used a CWD-relative path,
    //      and the installed build would write config.toml to C:\ when double-clicked,
    //      causing an "access denied" crash)
    let config_path = args
        .windows(2)
        .find(|w| w[0] == "--config")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| app_root.join("config.toml"));

    // Composition manifest (phase 5): the config is composed from
    // `profile → bundle → patch → config.toml`, and **config.toml is always the last
    // layer**, so not one word of previously written config needs changing. When there
    // is no aox.toml under the app root, the layer stack degrades to "just
    // config.toml" — exactly equivalent to before.
    let manifest_arg = args
        .windows(2)
        .find(|w| w[0] == "--manifest")
        .map(|w| PathBuf::from(&w[1]));
    let profile_arg = args.windows(2).find(|w| w[0] == "--profile").map(|w| w[1].clone());

    // `--dump-config`: just print the layer stack and the composed tree, then exit.
    // Deliberately goes through `compose` (without parsing Config) — when the config
    // is broken, that's exactly when this table is most needed.
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

    // `--setup` 强制走一遍模型引导：换机器、换 key 时不必手改 JSON，也让自动化
    // （管道喂三个值）能测到引导本身。放在最前面是因为首次运行的模板分支要用它：
    // 非 TTY 时那个分支默认"说完就退"，但带了 --setup 就不该退。
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
                            // 脚本/管道：保持"不碰你的输入、说清楚下一步"的老行为。
                            println!("{}", crate::i18n::t("Open it in an editor, fill in base_url and api_key (or api_key_env), then restart this program."));
                            pause_before_exit();
                            return Ok(());
                        }
                        // 交互终端：不要让人去编辑器里改文件——模型本来就不在 config.toml 里，
                        // 下面直接问端点 / Key / 模型名，写进 .aox/models.json。
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
            // 模板刚写下去，重新解析一次（它一定是合法的；真坏了就按配置错误处理）。
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

    // ---- First-run setup: ask for endpoint / key / model, write `.aox/models.json`
    //
    // The GUI pops an "add a model" dialog on first launch; the CLI used to answer "edit
    // config.toml and restart", which post-2026-10-28 is wrong twice over: the template has no
    // `[provider]` any more, and a double-clicked exe closes before anyone can edit anything.
    //
    // Triggers: no active record **and** no usable `[provider]` (old configs are left alone), or
    // an explicit `--setup` (re-run it without hand-editing JSON). Non-interactive runs skip the
    // questions — a piped script should be able to start the REPL without waiting on stdin.
    if force_setup
        || (stdin_is_tty()
            && crate::modelbook::Book::load(&app_root).active_record().is_none()
            && !config.provider.is_configured())
    {
        first_run_setup(&app_root)?;
    }

    // ---- Assemble the runtime: meta tools + restore persisted plugins
    // A wrong `[store] backend` fails here. Treated as a "config error": friendly
    // message + wait for a keypress, the same path as a TOML parse failure above —
    // so a user double-clicking the exe never sees only a flash.
    let rt = match Runtime::new(config, app_root.clone(), plugins_dir) {
        Ok(rt) => rt,
        Err(e) => {
            println!("{}", crate::tf!("[config error] {}", format!("{e:#}")));
            pause_before_exit();
            return Ok(());
        }
    };
    bootstrap::register_tools(&rt).await?;

    // ---- `aox remote ...`: the host side of AOX Remote ----
    // Placed **after** every tool is registered, so a remote turn has exactly the same tool set as
    // a local one, and before the banner/REPL, which a daemon has no use for.
    if args.get(1).map(String::as_str) == Some("remote") {
        return crate::remote::cli(rt.clone(), app_root.clone(), &args[2..], true).await;
    }

    let mut ui = CliUi::new();

    // Print the banner **before** restoring plugins. Restore is the one startup step that can
    // block — up to `INIT_TIMEOUT` (20 s) per cold-or-broken plugin — and it stays silent on
    // success, so running it first would leave the terminal blank for that whole wait. The REPL
    // below does not open until after restore, so nothing is skipped by moving this up.
    println!("\n{}", ui::style::logo());
    println!(
        "  {} {}",
        ui::style::dim(crate::i18n::t("Model")),
        // The live provider, not `rt.config.provider`: since the model is chosen in
        // `.aox/models.json`, the config snapshot in the runtime is only the fallback and is empty on
        // a fresh install — which is how the banner came to print "Model" and nothing after it.
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
    // Plugins whose fence isn't fitted must speak up — this is the classic "thinking
    // there's protection when there isn't" scenario, the same class as "plugin failed
    // to load": the user must know, but startup shouldn't be interrupted. The decision
    // and wording come from `sandbox::unfenced` (the GUI shares the same copy).
    let unfenced = sandbox::unfenced(&rt).await;
    if !unfenced.is_empty() {
        ui.emit(AgentEvent::Warn(format!(
            "These plugins are not wrapped in a process fence; they are not subject to the process / memory / UI limits: {}",
            unfenced.join(crate::i18n::t("; "))
        )));
    }

    // The provider was built by `Runtime::new` and lives on the runtime (see `src/provider_live.rs`),
    // so "not ready" is a state to report, not a value to construct here. A missing key is no longer
    // fatal either: the session still runs (local commands, history, plugins), and `/model` or the
    // GUI's Model tab can fill it in without a restart.
    if let Some(reason) = rt.provider.error() {
        ui.emit(AgentEvent::Error(crate::tf!("LLM not ready: {}", reason)));
        ui.emit(AgentEvent::Info(
            crate::i18n::t("Set base_url / api_key in config.toml (or point api_key_env at an environment variable), or configure it in the GUI's Settings -> Model; local commands /tools /plugins /demo still work.")
                .into(),
        ));
    }

    // The project rules file (RULES.md) is no longer read here: it is now the `rules`
    // segment of the prompt, taken uniformly from `config.agent.rules_file` by
    // `prompt::build` — the CLI and GUI share one implementation.
    let mut agent = Agent::new(rt.clone());

    // Runtime hint: only an extra line when Python is missing; when runtimes are
    // complete the startup output stays clean.
    if let Some(hint) = runtimes::missing_hint(&rt.root, &rt.config.runtimes) {
        ui.emit(AgentEvent::Warn(hint));
    }

    // ---- REPL
    //
    // `pending` is the **command-line version of the pending-upload tray**: `/attach
    // <path>` attaches, and the next message sends it along. It's the same path as the
    // GUI's drag-and-drop (both go through `Attachment::probe`, then take shape via
    // `Agent::run_turn`'s `files` argument), so the two ends agree exactly on "what can
    // be attached, what gets blocked" — which is also why the decision was gathered
    // into one implementation in `src/attach.rs` in the first place.
    let mut pending: Vec<PathBuf> = Vec::new();
    // `/remote start` 起的后台服务。放在循环外：退出时要先请它自己断开，
    // 直接 drop 掉任务等于掐线，服务端得等心跳超时才发现这台机器没了。
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
        // 远程服务。子命令和 `aox remote` 完全一致，区别只有 start：在 REPL 里它是**后台**的
        // （阻塞住就等于把终端交给守护进程，那是外部的 `aox remote serve`）。
        if line == "/remote" || line.starts_with("/remote ") {
            let rest = line["/remote".len()..].trim();
            let args: Vec<String> = rest.split_whitespace().map(String::from).collect();
            match args.first().map(String::as_str) {
                None | Some("status") => {
                    // "在跑"要看任务本身：`remote` 是 Some 只说明起过，可能已经因为
                    // 断网/被 GUI 取回控制权退出了。
                    let running = remote.as_ref().is_some_and(|j| !j.handle.is_finished());
                    ui.emit(AgentEvent::Info(crate::remote::brief(&app_root, running).into()));
                }
                Some("start") | Some("serve") => {
                    if remote.as_ref().is_some_and(|j| !j.handle.is_finished()) {
                        ui.emit(AgentEvent::Info(
                            "远程服务已经在跑了（先 /remote stop）".into(),
                        ));
                    } else {
                        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let handle = tokio::spawn({
                            let rt = rt.clone();
                            let root = app_root.clone();
                            let stop = stop.clone();
                            async move {
                                if let Err(e) = crate::remote::serve_headless(rt, root, stop).await {
                                    eprintln!("{}", crate::tf!("远程服务退出：{}", format!("{e:#}")));
                                }
                            }
                        });
                        remote = Some(RemoteJob { stop, handle });
                        ui.emit(AgentEvent::Info(
                            "远程服务已启动（后台）。/remote stop 停止。".into(),
                        ));
                    }
                }
                Some("stop") => {
                    let live = remote.as_ref().is_some_and(|j| !j.handle.is_finished());
                    if !live {
                        remote = None;
                        ui.emit(AgentEvent::Info("远程服务没在跑。".into()));
                    } else if let Some(job) = remote.take() {
                        job.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        ui.emit(AgentEvent::Info(
                            "已请求停止（最多 5 秒内断开）".into(),
                        ));
                    }
                }
                // 配对的交互式追问要读 stdin，会和行编辑抢：在 REPL 里必须一次把参数给全。
                Some("pair") if args.len() < 3 => {
                    ui.emit(AgentEvent::Info(
                        "在 REPL 里请一次给全：/remote pair <服务端地址> <配对码> [Agent 名称]"
                            .into(),
                    ));
                }
                // 其余子命令直接复用 `aox remote`：两处各写一遍只会慢慢走偏。
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
                    // Each slot prints which of its two states it is in. **Dormant is normal**, not a
                    // problem: the plugin's tools are registered and callable, it just has no process
                    // yet. A live one also shows its **fence status** — `fence ✗` means that process
                    // never entered a Job (the sandbox is off, or the outer Job doesn't allow
                    // nesting). It isn't a fatal error, but "thinking there's protection when there
                    // isn't" deserves a glance.
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
                    // Component contributions (phase 6 · 6b): only list those that **have
                    // contributions** — old plugins that only provide tools don't reappear
                    // here (the line above already mentioned them).
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
                // As a bonus demo: the waiting animation → the tool process gets
                // collapsed → /expand expands it
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
            // Workplace: `/workplace` shows the current state, `/workplace
            // full|default|<dir>` switches tiers. Both decision and execution live in
            // the `workplace` module (one implementation, shared by CLI and GUI).
            _ if workplace::is_cmd(line) => match workplace::command(&rt, line) {
                Ok(text) => {
                    ui.emit(AgentEvent::Info(text));
                    // The tier must be recorded into this conversation's save too. The
                    // "show current state" case also reaches here, but
                    // `persist_workplace` compares for itself and writes nothing if
                    // unchanged.
                    agent.persist_workplace();
                }
                Err(e) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
            },
            "/config" => ui.emit(AgentEvent::Info(layer_summary.clone())),
            // `/model` — the saved models and switching between them. The file is
            // `.aox/models.json` (the GUI's Model tab edits the same one), and the switch is the same
            // path the GUI uses: mark the record active, then hand it to `LiveProvider::apply`. No
            // restart, no new conversation.
            _ if modelbook::is_cmd(line) => match modelbook::command(&rt, line) {
                Ok(text) => ui.emit(AgentEvent::Info(text)),
                Err(e) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
            },
            "/shell" => ui.emit(AgentEvent::Info(tool::shell::report(&rt))),
            "/web" => ui.emit(AgentEvent::Info(tool::web::report(&rt))),
            "/new" => {
                // The same path as the "+" button on the GUI top bar. The old
                // conversation needs no save action — it has been appended to all
                // along and is already lying in history/ by now.
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
                    // On interrupt this now hinges on a single fact: did this turn's user message
                    // actually make it into the history? Rolling back to the last **legal** boundary
                    // (`Agent::interrupt_rollback_point`) keeps it in every real case, so the
                    // attachments are already in context and must be cleared too. The old code kept
                    // them because its rollback went back to the *turn* start, which dropped the
                    // message — a reason that no longer exists. The one case that still keeps them
                    // is "interrupted before `run_turn` had pushed the message at all": nothing was
                    // sent, and swallowing the attachments would only make the user attach them all
                    // over again.
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
                        // Roll back to the last **legal** boundary, not to the start of the turn:
                        // the user's instruction and every completed step stay in context, and only
                        // the in-flight (unreplayable) step is dropped. The granularity decision
                        // lives in `Agent::interrupt_rollback_point`, shared with the GUI.
                        agent.rollback_interrupted_turn(mark);
                        ui.emit(AgentEvent::Warn(
                            crate::i18n::t("Interrupted the current task (only the step that was still running was dropped; everything already done stays in context)").into(),
                        ));
                    }
                }
            }
        }
    }

    // 后台远程服务：先请它自己断开（心跳 5 秒一轮），最多等 6 秒。
    // 超时就直接放弃——用户已经按了退出，不该被一条连接拖住。
    if let Some(job) = remote.take() {
        job.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(6), job.handle).await;
    }

    // ---- Shut down all plugin processes before exiting
    //
    // Only the slots that actually have a process do any work here — under lazy loading that is
    // usually none of them, and `stop` decides that itself (the dormant case is a no-op rather than
    // a branch to keep in sync in two front ends).
    let handles: Vec<_> = rt.plugins.lock().await.values().cloned().collect();
    for h in handles {
        h.stop().await;
    }
    Ok(())
}

/// `/attach` — the command-line version of the pending-upload tray.
///
/// | Form | Meaning |
/// |---|---|
/// | `/attach` | show what's attached right now |
/// | `/attach clear` | detach everything |
/// | `/attach <path> [more…]` | attach (sent along with the next message) |
///
/// Both the decision and the interception go through the one implementation in
/// [`aox::attach`] (the same function the GUI drag-and-drop uses), so the reason a
/// 40 MiB file is turned away from the command line is **the same sentence** as the
/// reason it's turned away when dropped onto the UI.
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
    // When the whole string is an existing path, treat it as **one** file (otherwise a
    // path with spaces gets broken apart); otherwise split on whitespace (the most
    // common form, `/attach a.png b.pdf`).
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
