// AOX GUI — a minimal graphical front-end.
//
// It shares the same core library with the CLI build (`aox`) (`aox::agent` / `aox::llm` /
// `aox::plugin` / `aox::ui`): the agent main loop, plugin host, provider and authorization policy
// are all reused, and the GUI only handles "presentation + input".
//
// Architecture:
//   egui main thread (this thread)  <--channel-->  background tokio thread (drives aox::agent::Agent)
//   `App` implements `eframe::App`; `bridge::GuiUi` implements `aox::ui::Ui`.
//
// Common arguments (same as the CLI):
//   --config <path>   specify the config file
//   --theme  <path>   specify a theme file (wholly replaces the `[theme]` in config.toml)
//   --plugins <path>  specify the plugin directory
//   --prompt <text>   automatically send the first instruction after startup
//   --demo            demo mode: don't connect to the model, just preview the layout and event flow

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod activity;
mod app;
mod brand;
mod bridge;
mod cache;
mod gwin;
mod markdown;
mod open;
mod recovery_ui;
mod syntax;
mod svg;
mod tex;
mod theme;
mod uistate;
mod workplace_ui;
mod wsview;

use tokio::sync::mpsc;

use bridge::{FromAgent, SessionFlags, ToAgent};

/// Read appearance config: `--theme <file>` wins (**wholly replaces** `[theme]`), otherwise read
/// the composed `[theme]`.
///
/// Deliberately **no error, no exit** here: when config is missing or broken the window still opens,
/// just using the built-in appearance. "Config has a problem" is reported uniformly by the
/// background thread; the main thread's job is to not let the window fail to open.
///
/// Reading config goes through the **same** layered composition as the background thread
/// (profile → bundle → patch → config.toml) — otherwise a `[theme]` written in some patch would be
/// missed at this "before the window is built" step, showing up as "I changed the color in a patch
/// but the UI didn't change".
fn load_theme(args: &[String]) -> aox::config::ThemeConfig {
    if let Some(p) = aox::paths::arg_value(args, "--theme") {
        match aox::config::ThemeConfig::load(std::path::Path::new(&p)) {
            Ok(c) => return c,
            Err(e) => eprintln!("{}", aox::tf!("[theme] {}; falling back to the [theme] in config", format!("{e:#}"))),
        }
    }
    let path = aox::paths::resolve_config_path(args);
    let manifest = aox::paths::resolve_manifest_path(args);
    let profile = aox::paths::resolve_profile(args);
    aox::config::layer::resolve(
        &aox::paths::app_root(),
        &path,
        manifest.as_deref(),
        profile.as_deref(),
    )
    .map(|r| r.config.theme)
    .unwrap_or_default()
}

fn main() -> eframe::Result<()> {
    // ---- hidden shell-helper entry: **must be first** ----
    // The GUI is a `windows`-subsystem binary, so it is the one the agent's commands run through when
    // the user started the GUI rather than the CLI. The helper is the same code either way
    // (`aox::shell::helper`): lower this process's token, start the command, pass the exit code back.
    if let Some(code) = aox::shell::helper::run_if_requested() {
        std::process::exit(code);
    }
    let args: Vec<String> = std::env::args().collect();
    let demo = args.iter().any(|a| a == "--demo");
    let prompt = aox::paths::arg_value(&args, "--prompt");
    // Appearance must be computed before building the window: `theme::apply` runs inside
    // `App::new`, before the first frame.
    let theme_cfg = load_theme(&args);

    // Bidirectional channels: main thread ↔ agent driver thread
    let (to_agent_tx, to_agent_rx) = mpsc::unbounded_channel::<ToAgent>();
    let (from_agent_tx, from_agent_rx) = mpsc::unbounded_channel::<FromAgent>();
    let flags = SessionFlags::new();

    // Background thread: build the tokio runtime → load runtimes and plugins → drive the agent loop.
    // Demo mode doesn't start it, to avoid spinning idle.
    let driver = if demo {
        None
    } else {
        Some(bridge::spawn(from_agent_tx, to_agent_rx, flags.clone()))
    };

    // **No system decorations** (2026-10-18): the caption used to be a *second* top strip — an
    // empty, `P::bg()`-dyed native bar holding only the system buttons, sitting above a floating
    // row of our own controls inside the content area. Two bars, both the colour of the background,
    // neither quite "the window bar". Now the main window draws its own bar exactly the way the
    // independent windows do (`gwin`): drag, minimize / maximize / close, 1px outline, DWM rounded
    // corners — all rebuilt in `App::titlebar`. See the note there for the full story.
    //
    // The **system title** is "AOX" even though the bar shows no label: the window is undecorated,
    // so the string is never drawn anywhere — it is what the taskbar, Alt-Tab, the window list and
    // any accessibility tool call this window by, and an empty title there left the main window as
    // the one nameless entry next to "AOX Activity Stream" / "AOX Workspace". (The on-screen bar
    // still carries no "AOX": that part of the 2026-10-04 request is unchanged — the bar *is* the
    // app's top edge, not a label.)
    //
    // The icon must be **given explicit pixels** (`brand::window_icon` has the full story): without
    // it eframe installs its own "e", and the route of "pass an empty icon and let Windows use the
    // exe resource" doesn't work either (winit installs the empty icon for real, and the taskbar /
    // Alt-Tab would paint a system generic icon).
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AOX")
            .with_icon(brand::window_icon())
            .with_decorations(false)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([1000.0, 640.0]),
        ..Default::default()
    };

    eframe::run_native(
        "AOX",
        options,
        Box::new(move |cc| {
            let app = app::App::new(
                cc,
                theme_cfg,
                to_agent_tx,
                from_agent_rx,
                flags,
                driver,
                demo,
                prompt,
            );
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
}
