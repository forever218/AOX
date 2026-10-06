// AOX GUI —— 极简图形界面前端。
//
// 与 CLI 版本（`aox`）共用同一套核心库（`aox::agent` / `aox::llm` / `aox::plugin` /
// `aox::ui`），Agent 主循环、插件宿主、Provider、授权策略全部复用，GUI 只负责「呈现 + 输入」。
//
// 架构：
//   egui 主线程（本线程）  <--channel-->  后台 tokio 线程（驱动 aox::agent::Agent）
//   `App` 实现 `eframe::App`；`bridge::GuiUi` 实现 `aox::ui::Ui`。
//
// 常用参数（与 CLI 一致）：
//   --config <path>   指定配置文件
//   --theme  <path>   指定主题文件（整份取代 config.toml 的 [theme]）
//   --plugins <path>  指定插件目录
//   --prompt <text>   启动后自动发送第一条指令
//   --demo            演示模式：不连模型，只预览界面排布与事件流

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod activity;
mod app;
mod brand;
mod bridge;
mod markdown;
mod syntax;
mod svg;
mod tex;
mod theme;
mod workplace_ui;

use tokio::sync::mpsc;

use bridge::{FromAgent, SessionFlags, ToAgent};

/// 取外观配置：`--theme <file>` 优先（**整份取代** `[theme]`），否则读合成后的 `[theme]`。
///
/// 这里刻意**不报错、不退出**：配置缺失或写坏时窗口照常打开，只用内置外观。
/// 「配置有问题」由后台线程统一提示；主线程的职责是别让窗口打不开。
///
/// 读配置走的是与后台线程**同一条**分层合成（profile → bundle → patch → config.toml）——
/// 否则某个 patch 里写的 `[theme]` 会在「建窗口之前」这一步被漏掉，
/// 表现成「我在 patch 里改了颜色，界面却没变」。
fn load_theme(args: &[String]) -> aox::config::ThemeConfig {
    if let Some(p) = aox::paths::arg_value(args, "--theme") {
        match aox::config::ThemeConfig::load(std::path::Path::new(&p)) {
            Ok(c) => return c,
            Err(e) => eprintln!("[theme] {e:#}；改用配置里的 [theme]"),
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
    let args: Vec<String> = std::env::args().collect();
    let demo = args.iter().any(|a| a == "--demo");
    let prompt = aox::paths::arg_value(&args, "--prompt");
    // 外观必须在建窗口之前算好：`theme::apply` 在 `App::new` 里、第一帧之前跑。
    let theme_cfg = load_theme(&args);

    // 双向 channel：主线程 ↔ Agent 驱动线程
    let (to_agent_tx, to_agent_rx) = mpsc::unbounded_channel::<ToAgent>();
    let (from_agent_tx, from_agent_rx) = mpsc::unbounded_channel::<FromAgent>();
    let flags = SessionFlags::new();

    // 后台线程：建 tokio 运行时 → 装载运行时与插件 → 驱动 Agent 循环。
    // 演示模式不启动，避免空转。
    let driver = if demo {
        None
    } else {
        Some(bridge::spawn(from_agent_tx, to_agent_rx, flags.clone()))
    };

    // 标题留空（2026-10-04 用户要求）：原生标题栏不再写「AOX」，标题栏就只剩系统按钮
    // —— 与主界面一块底色，认不出「栏」。
    //
    // 图标必须**显式给像素**（`brand::window_icon` 里有完整的来龙去脉）：不给的话
    // eframe 会装它自带的那个「e」，而「给个空图标、让 Windows 用 exe 资源」那条路
    // 也不通（winit 会把空图标当真装上，标题栏画出系统通用图标）。
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("")
            .with_icon(brand::window_icon())
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
