//! AOX 核心库。
//!
//! 这个 crate 同时提供两种产物：
//! - **lib `aox`**（本文件）：把 Agent 主循环、LLM Provider、插件宿主、运行时与
//!   `Ui` 抽象层暴露出来，供 `gui/`（`aox-gui`）等其它前端复用。
//! - **bin `aox`**（`src/main.rs`）：原有的命令行版本，行为与加入 GUI 之前完全一致。
//!
//! 两个 crate root 各自编译同一批模块；CLI 不引用本 lib，因此它的行为不受影响。
//! 凡是「前端无关」的能力都应只依赖这里的公开接口，而不要复制实现。

pub mod agent;
pub mod auth;
pub mod config;
pub mod llm;
pub mod message;
pub mod paths;
pub mod plugin;
pub mod runtime;
pub mod runtimes;
pub mod selftest;
pub mod tool;
pub mod ui;
