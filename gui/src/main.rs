// AOX GUI — a minimal graphical front-end.
// It shares the same core library with the CLI build (`aox`) (`aox::agent` / `aox::llm` / `aox::plugin` /...
// Architecture: egui main thread (this thread) <--channel--> background tokio thread (drives aox::agent::Agent)...

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod activity;
mod app;
mod brand;
mod bridge;
mod cache;
mod gwin;
mod markdown;
mod model_ui;
mod open;
mod recovery_ui;
mod remote_lock;
mod remote_ui;
mod syntax;
mod svg;
mod tex;
mod theme;
mod uistate;
mod welcome;
mod workplace_ui;
mod wsview;

use tokio::sync::mpsc;

use bridge::{FromAgent, SessionFlags, ToAgent};

/// Read appearance config: `--theme <file>` wins (wholly replaces `theme`), otherwise read the composed `theme`.
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
    // Hidden shell-helper entry: must be first ---- The GUI is a `windows`-subsystem binary...
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

    // No system decorations: the caption used to be a *second* top strip — an empty, `P::bg()`-dyed native bar...
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
