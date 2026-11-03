//! 把「一个可用的 Runtime」装起来：构造函数 + 全部宿主工具。
//!
//! 从 `main.rs` 抽出来的原因很具体：`aox remote` 支持 `/cwd`，换工作空间意味着用新的
//! `[workplace]` 重建 Runtime，而重建出来的 Runtime 和启动时那个必须拥有**同一套工具**。
//! 复制一份注册代码，两份迟早会漂移（漏掉一个 `web_tools` 就会让远程少一整个能力）。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::runtime::Runtime;
use crate::tool::{ask, bash, discover, hostfs, job, meta, shell, task, verify, web};

pub async fn build(config: Config, root: PathBuf, plugins_dir: PathBuf) -> Result<Arc<Runtime>> {
    let rt = Runtime::new(config, root, plugins_dir)?;
    register_tools(&rt).await?;
    // `/cwd` 重建出来的 Runtime 必须和启动时一样带上用户插件，否则换一次目录
    // 第三方工具就悄悄消失了。失败不致命（插件坏了不该让整个 Runtime 起不来）。
    let _ = crate::plugin::load_all(&rt).await;
    Ok(rt)
}

/// 注册宿主内置工具。调用方负责 `Runtime::new`（CLI 要在出错时走"按任意键退出"的分支）。
pub async fn register_tools(rt: &Arc<Runtime>) -> Result<()> {
    for t in meta::meta_tools() {
        rt.register(t).await?;
    }
    // Host file tools (sandbox approach ③: take file capability back into the host).
    for t in hostfs::host_file_tools() {
        rt.register(t).await?;
    }
    // `ask_user` 与 `task_state` / `verify` 只注册"可见"，真正调用被 Agent 拦截。
    for t in ask::ask_tools() {
        rt.register(t).await?;
    }
    for t in discover::find_tools() {
        rt.register(t).await?;
    }
    for t in task::task_tools() {
        rt.register(t).await?;
    }
    for t in verify::verify_tools() {
        rt.register(t).await?;
    }
    if rt.config.shell.enabled {
        for t in shell::shell_tools(&rt.config.shell) {
            rt.register(t).await?;
        }
        for t in bash::bash_tools(rt) {
            rt.register(t).await?;
        }
        for t in job::job_tools() {
            rt.register(t).await?;
        }
    }
    if rt.config.web.enabled {
        for t in web::web_tools(&rt.config.web) {
            rt.register(t).await?;
        }
    }
    Ok(())
}
