use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub provider: ProviderConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub runtimes: RuntimesConfig,
    #[serde(default)]
    pub history: HistoryConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// 是否以 SSE 流式方式请求（默认 true）。
    /// 开启后思考模型的推理过程可以实时回显；若某个端点不接受
    /// stream / stream_options，设为 false 即回退为等完整响应。
    #[serde(default = "default_true")]
    pub stream: bool,
}

fn default_true() -> bool {
    true
}

impl ProviderConfig {
    pub fn resolve_key(&self) -> Result<String> {
        if let Some(k) = &self.api_key {
            if !k.trim().is_empty() {
                return Ok(k.clone());
            }
        }
        if let Some(env_name) = &self.api_key_env {
            let v = std::env::var(env_name)
                .with_context(|| format!("环境变量 {env_name} 未设置，无法获取 API key"))?;
            if !v.trim().is_empty() {
                return Ok(v);
            }
        }
        Err(anyhow!(
            "未配置 API key：请在 config.toml 的 [provider] 中设置 api_key 或 api_key_env"
        ))
    }

    pub fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// ask | auto | deny
    pub meta_tools: String,
    /// ask | auto | deny
    pub plugin_tools: String,
    #[serde(default)]
    pub overrides: HashMap<String, String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self { meta_tools: "ask".into(), plugin_tools: "auto".into(), overrides: HashMap::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub rules_file: Option<String>,
    pub tool_timeout_secs: u64,
    pub max_result_chars: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { rules_file: Some("RULES.md".into()), tool_timeout_secs: 60, max_result_chars: 20000 }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimesConfig {
    /// 值写成**裸命令**（`python` / `node` / `rustc`）时，宿主会依次尝试
    /// 「应用目录内置运行时 `<app_root>/runtime/<lang>/<exe>`」→「PATH」；
    /// 写成**完整路径**则直接采用，不做任何替换。
    pub python: String,
    pub node: String,
    pub rustc: String,
    /// 覆盖 Python 运行时下载源（默认 CPython 官方嵌入式包，见 `runtimes::DEFAULT_PYTHON_URL`）。
    #[serde(default)]
    pub python_url: Option<String>,
    /// 期望的 SHA-256。留空时：默认源用内置的官方校验值，自定义源则不校验。
    #[serde(default)]
    pub python_sha256: Option<String>,
}

impl Default for RuntimesConfig {
    fn default() -> Self {
        Self {
            python: "python".into(),
            node: "node".into(),
            rustc: "rustc".into(),
            python_url: None,
            python_sha256: None,
        }
    }
}

/// 对话历史落盘。
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryConfig {
    /// 是否把对话写入 `<应用根>/history/`。关掉后不再新开文件，
    /// 但已有的历史仍然可读可查看（`/history`、GUI 侧边栏照常工作）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 最多保留多少个对话（按时间，新的优先）；0 = 不限制。
    /// 只在新建对话时裁剪，所以不会删掉正在写的那个。
    #[serde(default = "default_history_keep")]
    pub keep: usize,
}

fn default_history_keep() -> usize {
    200
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self { enabled: true, keep: default_history_keep() }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "找不到配置文件 {}。请复制 config.example.toml 为 config.toml 并填写 API 配置。",
                path.display()
            )
        })?;
        let cfg: Config = toml::from_str(&text).with_context(|| format!("配置文件 {} 解析失败", path.display()))?;
        Ok(cfg)
    }

    /// selftest 使用的占位配置（不需要真实 provider）
    pub fn dummy() -> Self {
        Self {
            provider: ProviderConfig {
                base_url: String::new(),
                model: String::new(),
                api_key: None,
                api_key_env: None,
                max_tokens: None,
                temperature: None,
                stream: false,
            },
            auth: AuthConfig::default(),
            agent: AgentConfig::default(),
            runtimes: RuntimesConfig::default(),
            history: HistoryConfig::default(),
        }
    }
}
