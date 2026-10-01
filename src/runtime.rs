use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{Mutex, RwLock};

use crate::auth::AuthPolicy;
use crate::config::Config;
use crate::plugin::PluginHandle;
use crate::tool::{Tool, ToolKind, ToolOutput};

/// 运行时共享状态：工具注册表 + 插件管理 + 会话级授权白名单。
/// 元工具通过它对系统自身进行「生长」。
pub struct Runtime {
    pub config: Config,
    pub auth: AuthPolicy,
    /// 应用根目录。插件子进程以此为工作目录，保证插件内的相对路径与宿主视图一致。
    pub root: PathBuf,
    pub plugins_dir: PathBuf,
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    pub plugins: Mutex<HashMap<String, Arc<PluginHandle>>>,
    /// 用户在本会话中选择「总是允许」的工具名
    pub session_allowed: RwLock<HashSet<String>>,
}

impl Runtime {
    pub fn new(config: Config, root: PathBuf, plugins_dir: PathBuf) -> Arc<Self> {
        let auth = AuthPolicy::from_config(&config.auth);
        Arc::new(Self {
            config,
            auth,
            root,
            plugins_dir,
            tools: RwLock::new(HashMap::new()),
            plugins: Mutex::new(HashMap::new()),
            session_allowed: RwLock::new(HashSet::new()),
        })
    }

    pub async fn register(&self, tool: Arc<dyn Tool>) -> Result<()> {
        let mut tools = self.tools.write().await;
        let name = tool.name().to_string();
        if tools.contains_key(&name) {
            anyhow::bail!("工具名冲突: {name}（工具名全局唯一，请用「插件名_功能」命名）");
        }
        tools.insert(name, tool);
        Ok(())
    }

    pub async fn unregister(&self, name: &str) {
        self.tools.write().await.remove(name);
    }

    /// 每轮对话重新生成——模型刚注入的插件工具下一轮立即可见。
    pub async fn tool_schemas(&self) -> Vec<serde_json::Value> {
        let tools = self.tools.read().await;
        let mut v: Vec<_> = tools.values().map(|t| t.schema()).collect();
        v.sort_by(|a, b| {
            a["function"]["name"].as_str().unwrap_or("").cmp(b["function"]["name"].as_str().unwrap_or(""))
        });
        v
    }

    pub async fn tool_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tools.read().await.keys().cloned().collect();
        v.sort();
        v
    }

    pub async fn tool_kind(&self, name: &str) -> Option<ToolKind> {
        self.tools.read().await.get(name).map(|t| t.kind())
    }

    pub async fn execute(self: &Arc<Self>, name: &str, args: serde_json::Value) -> ToolOutput {
        let tool = { self.tools.read().await.get(name).cloned() };
        let Some(tool) = tool else {
            return ToolOutput::err(format!("工具不存在: {name}"));
        };
        // 兜底超时：插件工具内部已有 tool_timeout_secs 超时 + 重启重试，
        // 这里额外加 30 秒余量，防任何路径上的永久阻塞。
        let timeout = Duration::from_secs(self.config.agent.tool_timeout_secs + 30);
        match tokio::time::timeout(timeout, tool.call(args, self)).await {
            Ok(out) => out,
            Err(_) => ToolOutput::err(format!("工具 {name} 执行超时（{} 秒）", timeout.as_secs())),
        }
    }
}
