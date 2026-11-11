//! Assemble a usable Runtime: constructor plus every host tool, extracted from main.rs.
//! /cwd rebuilds the Runtime with a new [workplace]; the rebuilt one must carry the exact same tool set.
//! Duplicating the registrations lets them drift; one missing web_tools drops a whole remote capability.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::runtime::Runtime;
use crate::tool::{ask, bash, discover, hostfs, job, meta, shell, task, verify, web};

pub async fn build(config: Config, root: PathBuf, plugins_dir: PathBuf) -> Result<Arc<Runtime>> {
    let rt = Runtime::new(config, root, plugins_dir)?;
    register_tools(&rt).await?;
    // A /cwd-rebuilt Runtime must load user plugins too, or tools vanish on directory switch; failure is non-fatal.
    let _ = crate::plugin::load_all(&rt).await;
    Ok(rt)
}

/// Register built-in tools; the caller owns Runtime::new so the CLI can take its press-any-key exit on error.
pub async fn register_tools(rt: &Arc<Runtime>) -> Result<()> {
    for t in meta::meta_tools() {
        rt.register(t).await?;
    }
    // Host file tools (sandbox approach ③: take file capability back into the host).
    for t in hostfs::host_file_tools() {
        rt.register(t).await?;
    }
    // ask_user, task_state and verify are registered as visible only; their real calls are intercepted by Agent.
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
