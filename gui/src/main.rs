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
//   --plugins <path>  指定插件目录
//   --prompt <text>   启动后自动发送第一条指令
//   --demo            演示模式：不连模型，只预览界面排布与事件流

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bridge;
mod markdown;
mod theme;

use tokio::sync::mpsc;

use bridge::{FromAgent, SessionFlags, ToAgent};

/// 内嵌窗口图标：由 `assets/aox.ico` 预转换为 128×128 RGBA 原始数据，
/// 这样无需引入 PNG/ICO 解码依赖。
const ICON_RGBA: &[u8] = include_bytes!("../assets/icon.rgba");
const ICON_SIZE: u32 = 128;

fn window_icon() -> egui::IconData {
    egui::IconData {
        rgba: ICON_RGBA.to_vec(),
        width: ICON_SIZE,
        height: ICON_SIZE,
    }
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let demo = args.iter().any(|a| a == "--demo");
    let prompt = aox::paths::arg_value(&args, "--prompt");

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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AOX")
            .with_icon(window_icon())
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([1000.0, 640.0]),
        ..Default::default()
    };

    eframe::run_native(
        "AOX",
        options,
        Box::new(move |cc| {
            let app = app::App::new(cc, to_agent_tx, from_agent_rx, flags, driver, demo, prompt);
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
}
