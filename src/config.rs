use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Layered composition manifest (stage 5): turn "read one config" into "compose a layer stack".
pub mod layer;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// The fallback model configuration, from the layer stack (bundle / patch / `config.toml`).
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub runtimes: RuntimesConfig,
    #[serde(default)]
    pub history: HistoryConfig,
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub prompt: PromptConfig,
    #[serde(default)]
    pub hook: HookConfig,
    /// The `retry` section of `config.toml` (what to do when a model request fails, see RetryConfig...
    #[serde(default)]
    pub retry: RetryConfig,
    /// The `loop` section of `config.toml` (the loop seam's second form, see `src/agent_loop.rs`).
    #[serde(default, rename = "loop")]
    pub loop_config: LoopConfig,
    /// The `[sandbox]` section of `config.toml` (the plugin child-process fence, see `src/sandbox.rs`).
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// The `shell` section of `config.toml` (the built-in command line, see `src/tool/shell.rs`).
    #[serde(default)]
    pub shell: ShellConfig,
    /// The `web` section of `config.toml` (the built-in fetch tool, see `src/tool/web.rs`).
    #[serde(default)]
    pub web: WebConfig,
    /// The `plugins` section of `config.toml` (lazy loading of plugin subprocesses, see `src/plugin/mod.rs`).
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// The `tools` section of `config.toml` (which tool schemas are advertised, see ToolsConfig).
    #[serde(default)]
    pub tools: ToolsConfig,
    /// The `workplace` section of `config.toml` (the model's file boundary, see `src/workplace.rs`).
    #[serde(default, rename = "workplace", alias = "workspace")]
    pub workplace: WorkplaceConfig,
    /// The `[recovery]` section: task-level file checkpoints and rollback (see `src/recovery`).
    #[serde(default)]
    pub recovery: RecoveryConfig,
    /// GUI-only appearance config. The CLI bin target compiles this file too, but cannot read this field.
    #[allow(dead_code)]
    #[serde(default)]
    pub theme: ThemeConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    /// Empty = not configured yet.
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    /// Cap on the reply's tokens.
    /// It is also the amount reserved inside the context budget (`compact::Policy`), so the reply does not get cut off...
    #[serde(default = "default_max_tokens")]
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Whether to request via SSE streaming (default true).
    #[serde(default = "default_true")]
    pub stream: bool,
    /// The model's context window, in tokens.
    #[serde(default = "default_context_window")]
    pub context_window: Option<u32>,
    /// How hard the model should think before answering (the Model tab's "thinking strength").
    #[serde(default)]
    pub thinking: Thinking,
    /// Which wire format the thinking fields are written in.
    #[serde(default)]
    pub thinking_wire: ThinkingWire,
}

impl Default for ProviderConfig {
    /// Mirrors the serde defaults exactly (a `provider` with no keys at all and a `ProviderConfig::default()` have...
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            api_key: None,
            api_key_env: None,
            max_tokens: Some(DEFAULT_MAX_TOKENS),
            temperature: None,
            stream: true,
            context_window: Some(DEFAULT_CONTEXT_WINDOW),
            thinking: Thinking::default(),
            thinking_wire: ThinkingWire::default(),
        }
    }
}

/// Thinking strength, as the user states it (not as any one endpoint spells it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    /// Send nothing; the endpoint decides (DeepSeek's own default today is on, at high effort).
    #[default]
    Default,
    /// Turn thinking off. Worth having as its own word: DeepSeek ships with it **on**, so "off" is a
    /// real cost decision, not a nicety.
    Off,
    Low,
    High,
    Max,
}

impl Thinking {
    pub const ALL: [Thinking; 5] =
        [Thinking::Default, Thinking::Off, Thinking::Low, Thinking::High, Thinking::Max];

    /// The word written in `config.toml` and the one the GUI compares against.
    pub fn as_str(self) -> &'static str {
        match self {
            Thinking::Default => "default",
            Thinking::Off => "off",
            Thinking::Low => "low",
            Thinking::High => "high",
            Thinking::Max => "max",
        }
    }

    /// The value sent as `reasoning_effort`. `Max` degrades to `high` on wires that have no such
    /// word (OpenAI), which is stated in the GUI next to the choice instead of being hidden.
    pub fn effort(self) -> Option<&'static str> {
        match self {
            Thinking::Low => Some("low"),
            Thinking::High => Some("high"),
            Thinking::Max => Some("max"),
            Thinking::Off | Thinking::Default => None,
        }
    }
}

/// Which vendor dialect the thinking fields follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingWire {
    /// Infer from the host (see [`ThinkingWire::resolve`])
    #[default]
    Auto,
    /// `thinking: {type: "enabled"|"disabled"}` + `reasoning_effort` (DeepSeek, and the many
    /// gateways that copy it).
    Deepseek,
    /// `reasoning_effort` only; there is no way to switch thinking off.
    Openai,
    /// `enable_thinking: true|false` (DashScope / Qwen)
    Qwen,
    /// Send nothing at all.
    None,
}

impl ThinkingWire {
    pub const ALL: [ThinkingWire; 5] = [
        ThinkingWire::Auto,
        ThinkingWire::Deepseek,
        ThinkingWire::Openai,
        ThinkingWire::Qwen,
        ThinkingWire::None,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingWire::Auto => "auto",
            ThinkingWire::Deepseek => "deepseek",
            ThinkingWire::Openai => "openai",
            ThinkingWire::Qwen => "qwen",
            ThinkingWire::None => "none",
        }
    }

    /// Turn `auto` into a concrete wire, from the endpoint's host alone.
    pub fn resolve(self, base_url: &str) -> ThinkingWire {
        if self != ThinkingWire::Auto {
            return self;
        }
        let host = host_of(base_url);
        if host.contains("deepseek") {
            ThinkingWire::Deepseek
        } else if host.contains("dashscope") || host.contains("aliyun") {
            ThinkingWire::Qwen
        } else if host.contains("openai.com") {
            ThinkingWire::Openai
        } else {
            ThinkingWire::None
        }
    }

    /// Write this level's fields into a request body.
    pub fn apply(self, level: Thinking, body: &mut serde_json::Value) {
        use serde_json::json;
        if level == Thinking::Default || self == ThinkingWire::None {
            return;
        }
        match (self, level) {
            (ThinkingWire::Deepseek, Thinking::Off) => {
                body["thinking"] = json!({ "type": "disabled" });
            }
            (ThinkingWire::Deepseek, l) => {
                body["thinking"] = json!({ "type": "enabled" });
                if let Some(e) = l.effort() {
                    body["reasoning_effort"] = json!(e);
                }
            }
            // No way to disable it on this wire → `off` sends nothing, and the GUI says so
            (ThinkingWire::Openai, Thinking::Off) => {}
            (ThinkingWire::Openai, l) => {
                // `max` has no OpenAI equivalent; `high` is its ceiling.
                let e = if l == Thinking::Max { "high" } else { l.effort().unwrap_or("high") };
                body["reasoning_effort"] = json!(e);
            }
            (ThinkingWire::Qwen, Thinking::Off) => body["enable_thinking"] = json!(false),
            (ThinkingWire::Qwen, _) => body["enable_thinking"] = json!(true),
            (ThinkingWire::None | ThinkingWire::Auto, _) => {}
        }
    }
}

/// The bare host+port of a base URL, lowercased. Hand-rolled because `reqwest::Url` is overkill for
/// "which vendor is this" and would drag a parse failure into a decision that must never fail.
fn host_of(base_url: &str) -> String {
    let s = base_url.trim();
    let s = s.split_once("://").map_or(s, |(_, rest)| rest);
    let s = s.split('/').next().unwrap_or(s);
    s.to_ascii_lowercase()
}


/// Default reply-token cap: generous enough that a normal answer, a diff or a file write fits in one
/// call, far below what any current endpoint considers "too many".
pub const DEFAULT_MAX_TOKENS: u32 = 32_000;

/// Default context window assumed when the config says nothing.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 600_000;

fn default_max_tokens() -> Option<u32> {
    Some(DEFAULT_MAX_TOKENS)
}

fn default_context_window() -> Option<u32> {
    Some(DEFAULT_CONTEXT_WINDOW)
}

fn default_true() -> bool {
    true
}

/// The shipped configuration template, embedded once.
pub const TEMPLATE: &str = include_str!("../config.example.toml");

impl ProviderConfig {
    pub fn resolve_key(&self) -> Result<String> {
        if let Some(k) = &self.api_key {
            if !k.trim().is_empty() {
                return Ok(k.clone());
            }
        }
        if let Some(env_name) = &self.api_key_env {
            let v = std::env::var(env_name)
                .with_context(|| crate::tf!("environment variable {} is not set, cannot get the API key", env_name))?;
            if !v.trim().is_empty() {
                return Ok(v);
            }
        }
        Err(anyhow!(
            crate::i18n::t("no API key configured for the model in force: fill it in under Settings → Model (it is stored in .aox/models.json)")
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

    /// The `…/models` URLs to try when scanning, in order.
    pub fn models_urls(&self) -> Vec<String> {
        let mut base = self.base_url.trim().trim_end_matches('/').to_string();
        if let Some(b) = base.strip_suffix("/chat/completions") {
            base = b.trim_end_matches('/').to_string();
        }
        if let Some(b) = base.strip_suffix("/completions") {
            base = b.trim_end_matches('/').to_string();
        }
        let versioned = base
            .rsplit('/')
            .next()
            .is_some_and(|last| last.strip_prefix('v').is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())));
        if versioned {
            vec![format!("{base}/models")]
        } else {
            vec![format!("{base}/v1/models"), format!("{base}/models")]
        }
    }

    /// The wire this config's thinking fields follow, with `auto` already resolved.
    pub fn wire(&self) -> ThinkingWire {
        self.thinking_wire.resolve(&self.base_url)
    }

    /// Whether this config looks filled in.
    pub fn is_configured(&self) -> bool {
        if !self.base_url.trim().is_empty() && is_placeholder(self.base_url.trim()) {
            return false;
        }
        if !self.model.trim().is_empty() && is_placeholder(self.model.trim()) {
            return false;
        }
        self.key_state() != KeyState::Missing
    }

    /// Where the key comes from, for both the readiness check and the panel's hint line.
    pub fn key_state(&self) -> KeyState {
        if let Some(k) = &self.api_key {
            let k = k.trim();
            if !k.is_empty() && !is_placeholder(k) {
                return KeyState::Inline;
            }
        }
        if let Some(name) = &self.api_key_env {
            match std::env::var(name) {
                Ok(v) if !v.trim().is_empty() => return KeyState::Env(name.clone()),
                _ => return KeyState::EnvUnset(name.clone()),
            }
        }
        KeyState::Missing
    }
}

/// Where the API key in force comes from (see [`ProviderConfig::key_state`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    /// Written in `config.toml` (or supplied by the panel).
    Inline,
    /// Taken from this environment variable, which is set and non-empty.
    Env(String),
    /// The variable is named but not set — worth saying out loud, because the config *looks* done.
    EnvUnset(String),
    Missing,
}

/// Values that mean "nobody has filled this in yet".
fn is_placeholder(s: &str) -> bool {
    let l = s.trim().to_ascii_lowercase();
    l.is_empty()
        || l.contains("...")
        || l.contains('<')
        || matches!(l.as_str(), "sk-" | "your-key" | "your_api_key" | "changeme" | "none" | "todo")
}

#[derive(Debug, Clone, Deserialize)]
// The whole table is optional: when `auth` only sets `meta_tools`, `plugin_tools` takes its code default.
#[serde(default)]
pub struct AuthConfig {
    /// One of: ask | auto | deny
    pub meta_tools: String,
    /// ask | auto | deny
    pub plugin_tools: String,
    /// Ask | auto | deny — the shell tools (`shell`, and `bash` once it lands) only.
    pub shell: String,
    /// Ask | auto | deny — the web tools (`web_fetch`, and anything else in `NET_TOOLS`).
    pub web: String,
    #[serde(default)]
    pub overrides: HashMap<String, String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            // Everything is allowed unless the user says otherwise: a tool call that stops to ask is a tool call the model...
            meta_tools: "auto".into(),
            plugin_tools: "auto".into(),
            shell: "auto".into(),
            web: "auto".into(),
            overrides: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
// Same as above: `agent` is written in parts by two bundles (tools writes the timeout and truncation, prompt-core...
#[serde(default)]
pub struct AgentConfig {
    pub rules_file: Option<String>,
    pub tool_timeout_secs: u64,
    pub max_result_chars: usize,
    /// The input token budget for one request; `> 0` turns compaction on and overrides everything else.
    pub context_budget_tokens: u64,
    /// How much of the model's context window may be used for input, before the reply's reserve is
    /// taken off. Only read when `context_budget_tokens == 0` and `context_window` is set.
    pub compact_ratio: f32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            rules_file: Some("RULES.md".into()),
            tool_timeout_secs: 60,
            max_result_chars: 20000,
            context_budget_tokens: 0,
            compact_ratio: 0.8,
        }
    }
}

/// The `tools` section: how much of the tool registry is advertised to the model.
#[derive(Debug, Clone, Deserialize)]
// Same as above: a section that sets only one key leaves the other at its default.
#[serde(default)]
pub struct ToolsConfig {
    /// Registry size at or below which the whole list is sent (old behaviour). `0` = never hide plugin tools, ever —...
    pub inline_max: usize,
    /// Tools that stay visible regardless of `inline_max` (plugin tools included).
    pub enabled: Vec<String>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        // ⚠️ 0 would be a legitimate value here, but **not** a good default: it means "always inline",
        // which is exactly what this section exists to escape once a registry grows.
        Self { inline_max: 24, enabled: Vec::new() }
    }
}

// Same as above: when `[runtimes]` sets only `python`, `node` / `rustc` take their code defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RuntimesConfig {
    /// When the value is a bare command (`python` / `node` / `rustc`), the host tries in order.
    pub python: String,
    pub node: String,
    pub rustc: String,
    /// Which side wins when both exist: `system` (default) or `builtin`.
    #[serde(default = "default_prefer")]
    pub prefer: String,
    /// Override the Python runtime download source (default: the official CPython embeddable package, see `runtimes::DEFAULT_PYTHON_URL`).
    #[serde(default)]
    pub python_url: Option<String>,
    /// The expected SHA-256. When empty: the default source uses the built-in official checksum, while a custom source is not verified.
    #[serde(default)]
    pub python_sha256: Option<String>,
}

/// `prefer` defaults to the system interpreter; see [`RuntimesConfig::prefer`].
fn default_prefer() -> String {
    "system".to_string()
}

impl Default for RuntimesConfig {
    fn default() -> Self {
        Self {
            python: "python".into(),
            node: "node".into(),
            rustc: "rustc".into(),
            prefer: default_prefer(),
            python_url: None,
            python_sha256: None,
        }
    }
}

/// Conversation history persistence.
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryConfig {
    /// Whether to write conversations into `<app root>/history/`. Once off, no new files are opened,
    /// but existing history remains readable and viewable (`/history`, the GUI sidebar still work).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How many conversations to keep at most (by time, newest first); 0 = unlimited.
    /// Trimming happens only when a new conversation is created, so the one being written is never deleted.
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

/// The storage backend for conversation history (the storage seam extracted in stage 3, see `src/store.rs`).
#[derive(Debug, Clone, Deserialize)]
pub struct StoreConfig {
    #[serde(default = "default_store_backend")]
    pub backend: String,
}

    fn default_store_backend() -> String {
        // The default backend name is written once, in the `store` module, and referenced from the config side — so the two don't each spell out a literal.
        crate::store::default_backend().to_string()
    }

impl Default for StoreConfig {
    fn default() -> Self {
        Self { backend: default_store_backend() }
    }
}

/// How the system prompt is assembled (reordering / disabling / appending segments).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PromptConfig {
    /// Explicit order: the listed segment ids are sorted to the front in this order, and the unlisted ones keep...
    #[serde(default)]
    pub order: Vec<String>,
    /// Segment ids to disable (built-in segments may use `core` / `protocol` / `env` / `rules`).
    #[serde(default)]
    pub disable: Vec<String>,
    /// Segments to append (or to override a built-in segment by the same id).
    #[serde(default)]
    pub segment: Vec<PromptSegmentConfig>,
}

/// A custom prompt segment in `config.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct PromptSegmentConfig {
    /// Segment id; when it matches a built-in segment (`core` / `protocol`) the **whole segment replaces** the built-in one.
    pub id: String,
    /// Display name; defaults to the id.
    #[serde(default)]
    pub title: Option<String>,
    pub body: String,
    /// Sort key, default 50 (after the built-in segments and the runtime environment).
    #[serde(default)]
    pub order: Option<i32>,
}

/// Loop hooks (the stage-4 Loop seam, implemented in `src/hook.rs`).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HookConfig {
    /// Auto-fill arguments for the given tools, written like.
    #[serde(default)]
    pub tool_args_default: HashMap<String, HashMap<String, toml::Value>>,
}

/// The `retry` section: how many times a failed model request is sent again, and how long to wait between attempts.
#[derive(Debug, Clone, Deserialize)]
pub struct RetryConfig {
    /// Extra attempts after the first one (`0` = never retry).
    #[serde(default = "default_retry_attempts")]
    pub attempts: usize,
    /// The first wait, in milliseconds. It doubles per attempt: `base`, `2×base`, `4×base` …
    #[serde(default = "default_retry_base_ms")]
    pub base_ms: u64,
    /// Ceiling for one wait, in milliseconds. It also caps a `Retry-After` the server sends: an
    /// endpoint asking for an hour would otherwise leave the turn silent for an hour.
    #[serde(default = "default_retry_max_ms")]
    pub max_ms: u64,
}

fn default_retry_attempts() -> usize {
    2
}

fn default_retry_base_ms() -> u64 {
    1_000
}

fn default_retry_max_ms() -> u64 {
    30_000
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            attempts: default_retry_attempts(),
            base_ms: default_retry_base_ms(),
            max_ms: default_retry_max_ms(),
        }
    }
}

/// The main loop (the loop seam's second form from stage 6 · 6a, implemented in `src/agent_loop.rs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoopConfig {
    /// Which loop implementation to use.
    #[serde(rename = "impl")]
    pub implementor: String,
    /// How many "user turns" to keep at most in the context; `0` (default) = no trimming.
    pub max_rounds: usize,
    /// How many model requests one turn may make before the host stops and hands control back. `0` (default) =...
    #[serde(default)]
    pub max_iters_per_turn: usize,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self { implementor: default_loop_impl(), max_rounds: 0, max_iters_per_turn: 0 }
    }
}

fn default_loop_impl() -> String {
    // The default implementation name is written once, in the `agent_loop` module, and referenced from the config side — so the two don't each spell out a literal.
    crate::agent_loop::default_impl().to_string()
}

/// The plugin child-process fence (Job Object, see `src/sandbox.rs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    /// Master switch. Once off, no Job is built and no `CREATE_SUSPENDED` is added, byte-identical to before the fence.
    pub enabled: bool,
    /// How many child processes each plugin may open at most (not counting the plugin itself).
    pub max_processes: u32,
    /// Commit-memory cap for each plugin process, in MB. `0` (default) = unlimited.
    pub max_memory_mb: usize,
    /// Forbid the plugin from using the host process's USER object (windows / clipboard / desktop / global atoms).
    /// A plugin process doesn't need these anyway — all its communication goes through the stdin/stdout pipes.
    pub ui_limits: bool,
}

impl SandboxConfig {
    /// The `ActiveProcessLimit` actually written into the Job.
    pub fn job_process_limit(&self) -> u32 {
        self.max_processes.saturating_add(1)
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self { enabled: true, max_processes: 8, max_memory_mb: 0, ui_limits: true }
    }
}

/// The built-in shell tools (`config.toml`'s `shell`, see `src/tool/shell.rs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    /// Master switch. `false` = the tools are never registered, so the model never sees them.
    pub enabled: bool,
    /// Which command line the model should reach for first: `bash` (default) or `cmd`.
    #[serde(default = "default_shell_prefer")]
    pub prefer: String,
    // NOTE: there is deliberately no `boundary` switch.
    pub bash: String,
    /// Whether `<app root>\runtime\bash\bash.exe` (or `busybox.exe`) is considered by `auto`.
    pub bash_pack: String,
    /// Default per-call timeout, in seconds.
    pub timeout_secs: u64,
    /// Hard ceiling for `timeout_secs`. A model that asks for an hour waits an hour otherwise,
    /// while holding one tool slot the whole time.
    pub max_timeout_secs: u64,
    /// Kill a call that has written nothing to either stream for this many seconds.
    pub idle_timeout_secs: u64,
    /// How much of a command's output to keep (stdout and stderr counted separately), in bytes.
    pub max_output_bytes: usize,
    /// Process allowance for one call (a Job Object per call, the same mechanism as plugins).
    pub max_processes: u32,
    /// How many long-lived `shell_job` trees may exist at once. `0` disables the tool.
    pub background_max_jobs: usize,
    /// Process allowance inside one background Job.
    pub background_max_processes: u32,
    /// Maximum lifetime of one background job, in hours. `0` = no expiry (stop explicitly instead).
    pub background_max_hours: u64,
    /// Point build caches at the workspace (`CARGO_HOME`, `NPM_CONFIG_CACHE`, `PIP_CACHE_DIR`, `TMP`/`TEMP`) instead...
    pub env_cache: bool,
    /// Report which workspace files changed during the call (a bounded mtime scan afterwards).
    pub track_changes: bool,
}

/// `prefer` defaults to bash; see [`ShellConfig::prefer`]
fn default_shell_prefer() -> String {
    "bash".to_string()
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            prefer: default_shell_prefer(),
            bash: "auto".into(),
            bash_pack: "runtime".into(),
            timeout_secs: 60,
            max_timeout_secs: 600,
            idle_timeout_secs: 30,
            max_output_bytes: 128 * 1024,
            max_processes: 16,
            background_max_jobs: 4,
            background_max_processes: 64,
            background_max_hours: 2,
            env_cache: true,
            track_changes: true,
        }
    }
}

/// The built-in web tool (`config.toml`'s `web`, see `src/tool/web.rs` + `src/web/`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    /// Master switch. `false` = the tool is never registered, so the model never sees it.
    pub enabled: bool,
    /// Which hosts may be reached: `public` (default) | `any` | `allowlist`.
    pub egress: String,
    /// Exceptions (`public`) or the whole list (`allowlist`).
    pub hosts: Vec<String>,
    /// Default per-result character budget
    pub max_chars: usize,
    /// Ceiling for an explicit `max_chars` in the call
    pub hard_max_chars: usize,
    /// Body cap for text-ish responses, counted **after** decompression.
    pub max_bytes: usize,
    /// Body cap for `save = true` (PDFs, images, archives)
    pub max_save_bytes: usize,
    /// Default total deadline for one call, in seconds.
    pub timeout_secs: u64,
    /// Ceiling for the call's own `timeout_secs`
    pub max_timeout_secs: u64,
    /// How many redirect hops are followed before giving up.
    pub max_redirects: u32,
    /// The `User-Agent` sent with every request. Empty = the built-in default.
    pub user_agent: String,
    /// What to try when nothing declares an encoding: `gb18030` (default) | `windows-1252` | `none`.
    pub charset_fallback: String,
    /// Extra PEM roots for an intranet CA. `rustls` uses the bundled webpki roots, not the Windows
    /// certificate store, so a corporate root has to be pointed at explicitly.
    pub ca_bundle: String,
    /// Accept invalid certificates. A last resort, and documented as one.
    pub insecure_tls: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            egress: "public".into(),
            hosts: Vec::new(),
            max_chars: 12_000,
            hard_max_chars: 48_000,
            max_bytes: 4 * 1024 * 1024,
            max_save_bytes: 32 * 1024 * 1024,
            timeout_secs: 30,
            max_timeout_secs: 60,
            max_redirects: 5,
            user_agent: String::new(),
            charset_fallback: "gb18030".into(),
            ca_bundle: String::new(),
            insecure_tls: false,
        }
    }
}

/// Lazy loading of plugin subprocesses (`config.toml`'s `plugins`, see `src/plugin/mod.rs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// Master switch. `true` (default) = metadata-only at boot, start on demand, reap when idle.
    pub lazy: bool,
    /// How many seconds a plugin process may sit unused before it is shut down.
    pub idle_timeout_secs: u64,
    /// Upper bound on simultaneously live plugin processes. `0` (default) = unlimited.
    pub max_resident: usize,
    /// Plugin names that always stay resident even when idle.
    pub eager: Vec<String>,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self { lazy: true, idle_timeout_secs: 60, max_resident: 0, eager: Vec::new() }
    }
}

/// The workspace boundary (the config surface of sandbox option "take the file capability back to the host", see...
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorkplaceConfig {
    /// `"default"` (default: confined to the app root — the workspace itself) / `"full"` (unrestricted) / `"fixed"`...
    pub mode: String,
    /// The fixed workspace directory when `mode = "fixed"`. A relative path is interpreted relative to the **app root**.
    pub root: Option<String>,
}

impl Default for WorkplaceConfig {
    fn default() -> Self {
        // ⚠️ The factory default is `default` (confined), not `full` — see the `WorkplaceConfig` comments above.
        Self { mode: "default".into(), root: None }
    }
}

// Task recovery (checkpoints / rollback)

/// How thoroughly an opaque tool call (`shell`, a writing plugin) is guarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellGuard {
    /// Hash the whole scoped tree before and after; refuse the call if that cannot be done.
    Strict,
    /// Use metadata as a shortcut; a write discovered only afterwards leaves the node `partial`.
    BestEffort,
    /// Do not capture opaque writes at all.
    Off,
}

/// Task-level file checkpoints under `<workspace>/.aox/backup`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RecoveryConfig {
    pub enabled: bool,
    /// How many `turn` nodes must remain undoable from the cursor.
    pub keep: usize,
    pub max_total_mb: u64,
    pub max_file_mb: u64,
    pub max_turn_mb: u64,
    pub metadata_memory_mb: u64,
    pub shell_guard: ShellGuard,
    pub protect_plugins: bool,
    /// Workspace-relative path patterns. A trailing `/` means a directory prefix; `*.ext` matches a suffix.
    pub exclude: Vec<String>,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep: 30,
            max_total_mb: 512,
            max_file_mb: 64,
            max_turn_mb: 256,
            metadata_memory_mb: 32,
            shell_guard: ShellGuard::Strict,
            protect_plugins: true,
            exclude: default_recovery_exclude(),
        }
    }
}

fn default_recovery_exclude() -> Vec<String> {
    [
        ".aox/",
        ".git/",
        ".hg/",
        ".svn/",
        "target/",
        "node_modules/",
        ".venv/",
        "venv/",
        "__pycache__/",
        ".pytest_cache/",
        // Downloaded toolchains and vendored runtimes: generated, large, and never the thing a rollback is for.
        "runtime/",
        // Agent housekeeping (this program's sibling tooling, session logs, blobs). Not the user's
        // source, and it grows on its own.
        ".workbuddy/",
        "dist/",
        "build/",
        "out/",
        // Executable images and their symbols.
        "*.exe",
        "*.dll",
        "*.pdb",
        "*.msi",
        "*.apk",
        "*.tmp",
        "*.swp",
        "*.lock",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

// ─────────────────────────── Appearance (GUI theme) ───────────────────────────.

/// A colour value.
/// Accepts these forms (usable in both `config.toml` and theme files): - `"#RRGGBB"` opaque - `"#RRGGBBAA"`...
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl RgbColor {
    fn parse_hex(s: &str) -> Option<Self> {
        let t = s.trim().trim_start_matches('#');
        // Note `get` checks character boundaries: should non-ASCII sneak in, this is judged invalid rather than panicking.
        let byte = |i: usize| u8::from_str_radix(t.get(i..i + 2)?, 16).ok();
        match t.len() {
            6 => Some(Self { r: byte(0)?, g: byte(2)?, b: byte(4)?, a: 255 }),
            8 => Some(Self { r: byte(0)?, g: byte(2)?, b: byte(4)?, a: byte(6)? }),
            3 => {
                let nib =
                    |i: usize| -> Option<u8> { Some(u8::from_str_radix(t.get(i..i + 1)?, 16).ok()? * 17) };
                Some(Self { r: nib(0)?, g: nib(1)?, b: nib(2)?, a: 255 })
            }
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for RgbColor {
    fn deserialize<D>(d: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V;

        impl<'de> serde::de::Visitor<'de> for V {
            type Value = RgbColor;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(crate::i18n::t("color: \"#RRGGBB\" / \"#RRGGBBAA\" / \"#RGB\" or [r, g, b(, a)]"))
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<RgbColor, E> {
                RgbColor::parse_hex(s).ok_or_else(|| {
                    E::custom(crate::tf!("cannot parse the color {}: expected #RRGGBB / #RRGGBBAA / #RGB", format!("{s:?}")))
                })
            }

            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<RgbColor, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut v: [u8; 4] = [0, 0, 0, 255];
                let mut n = 0usize;
                while n < 4 {
                    match seq.next_element::<u8>()? {
                        Some(x) => {
                            v[n] = x;
                            n += 1;
                        }
                        None => break,
                    }
                }
                match n {
                    3 | 4 => Ok(RgbColor { r: v[0], g: v[1], b: v[2], a: v[3] }),
                    _ => Err(serde::de::Error::custom(crate::i18n::t("a color array needs 3 or 4 components (each 0-255)"))),
                }
            }
        }

        d.deserialize_any(V)
    }
}

/// A per-field override of one palette.
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
pub struct PaletteConfig {
    pub bg: Option<RgbColor>,
    pub panel: Option<RgbColor>,
    pub surface: Option<RgbColor>,
    pub surface_hi: Option<RgbColor>,
    pub border: Option<RgbColor>,
    pub border_strong: Option<RgbColor>,
    pub text: Option<RgbColor>,
    pub text_dim: Option<RgbColor>,
    pub text_faint: Option<RgbColor>,
    /// The accent colour in the monochrome scheme (almost all black in light mode, almost all white in dark mode)
    pub ink: Option<RgbColor>,
    pub ink_soft: Option<RgbColor>,
    pub user_bg: Option<RgbColor>,
    pub code_bg: Option<RgbColor>,
    /// The "chip" background for inline code (deliberately a different shade from `code_bg`)
    pub code_chip: Option<RgbColor>,
    pub quote_bar: Option<RgbColor>,
    /// Background of the reversed block (error notices)
    pub reverse_bg: Option<RgbColor>,
    pub on_reverse: Option<RgbColor>,
    /// Shadow colour of floating cards
    pub shadow: Option<RgbColor>,
    /// The only hue in the whole palette: dangerous actions (currently used only for "full access").
    pub danger: Option<RgbColor>,
}

/// The UI's font sizes and scale. Every item has a built-in default, so you need only write the ones you want to change.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Body font size, the baseline for the whole UI
    pub body_size: f32,
    /// Headings
    pub heading_size: f32,
    /// Button text
    pub button_size: f32,
    /// Small text
    pub small_size: f32,
    /// Monospace (code blocks)
    pub mono_size: f32,
    /// Card corner radius
    pub card_radius: u8,
    /// Item spacing `[horizontal, vertical]`
    pub item_spacing: [f32; 2],
    /// Button padding `[horizontal, vertical]`
    pub button_padding: [f32; 2],
    /// Minimum height of interactive controls
    pub interact_height: f32,
    /// Indent
    pub indent: f32,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            body_size: 16.0,
            heading_size: 21.0,
            button_size: 15.0,
            small_size: 13.0,
            mono_size: 14.5,
            card_radius: 12,
            item_spacing: [10.0, 10.0],
            button_padding: [14.0, 9.0],
            interact_height: 30.0,
            indent: 22.0,
        }
    }
}

/// Font file paths. When empty, continue auto-searching in the order "environment variable → built-in candidate table"
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
pub struct FontsConfig {
    /// Body / CJK fallback (equivalent to the environment variable `AOX_GUI_FONT`)
    pub cjk: Option<String>,
    /// Bold (`AOX_GUI_FONT_BOLD`)
    pub bold: Option<String>,
    /// Serif, used for subheadings (`AOX_GUI_FONT_SERIF`)
    pub serif: Option<String>,
    /// Brand font, used for the first-screen big greeting (`AOX_GUI_FONT_BRAND`)
    pub brand: Option<String>,
    /// Monospace, used for code blocks (`AOX_GUI_FONT_MONO`)
    pub mono: Option<String>,
}

/// GUI appearance config: `config.toml`'s `theme`, or a whole theme file given with `--theme <file>`.
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
pub struct ThemeConfig {
    /// Initial light/dark: `"light"` / `"dark"`.
    pub mode: Option<String>,
    /// Overrides for the "paper" (light) set
    #[serde(default)]
    pub light: PaletteConfig,
    /// Overrides for the "ink" (dark) set
    #[serde(default)]
    pub dark: PaletteConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub fonts: FontsConfig,
}

#[allow(dead_code)] // Only the GUI calls it (`--theme <file>`); the CLI doesn't accept this argument, but the bin target compiles this file too
impl ThemeConfig {
    /// Parse a theme from a TOML text.
    fn from_toml_text(text: &str) -> Result<Self> {
        let value: toml::Value = toml::from_str(text)?;
        // Writing `mode` at the top of the file (i.e. outside `theme`) is very natural, and TOML puts it on the document...
        let root_mode = value.get("mode").and_then(|v| v.as_str()).map(str::to_string);
        // Use `[theme]` if present, otherwise treat the whole file as the theme
        let node = value.get("theme").cloned().unwrap_or(value);
        let mut cfg = <ThemeConfig as Deserialize>::deserialize(node)?;
        if cfg.mode.is_none() {
            cfg.mode = root_mode;
        }
        Ok(cfg)
    }

    /// Load a standalone theme file (`--theme <path>`)
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| crate::tf!("theme file not found: {}", path.display()))?;
        Self::from_toml_text(&text)
            .with_context(|| crate::tf!("theme file {} cannot be used as a theme", path.display()))
    }
}

impl Config {
    /// Read a single TOML into `Config` (no layered composition done to it).
    #[allow(dead_code)] // No caller in the bin target; in the lib target it is a public API
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            crate::tf!(
                "config file {} not found. Copy config.example.toml to config.toml and fill in your API settings.",
                path.display()
            )
        })?;
        let cfg: Config = toml::from_str(&text).with_context(|| crate::tf!("failed to parse config file {}", path.display()))?;
        Ok(cfg)
    }

    /// Placeholder config for selftest (no real provider needed)
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
                context_window: None,
                thinking: Thinking::default(),
                thinking_wire: ThinkingWire::default(),
            },
            auth: AuthConfig::default(),
            agent: AgentConfig::default(),
            runtimes: RuntimesConfig::default(),
            history: HistoryConfig::default(),
            store: StoreConfig::default(),
            prompt: PromptConfig::default(),
            hook: HookConfig::default(),
            retry: RetryConfig::default(),
            loop_config: LoopConfig::default(),
            sandbox: SandboxConfig::default(),
            shell: ShellConfig::default(),
            web: WebConfig::default(),
            plugins: PluginsConfig::default(),
            tools: ToolsConfig::default(),
            workplace: WorkplaceConfig::default(),
            recovery: RecoveryConfig::default(),
            theme: ThemeConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap before parsing: a TOML document cannot be a bare value, and a colour can only appear as a value of some key.
    #[derive(Deserialize)]
    struct W {
        c: RgbColor,
    }

    fn color(src: &str) -> Option<RgbColor> {
        toml::from_str::<W>(&format!("c = {src}")).ok().map(|w| w.c)
    }

    /// A plausible-looking provider configuration, for the tests that only ask about parsing.
    fn provider() -> ProviderConfig {
        ProviderConfig {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-flash".into(),
            api_key: Some("sk-test".into()),
            api_key_env: None,
            max_tokens: Some(32_000),
            temperature: Some(0.3),
            stream: true,
            context_window: Some(1_000_000),
            thinking: Thinking::High,
            thinking_wire: ThinkingWire::Deepseek,
        }
    }

    /// The placeholder the installer ships (`sk-...`) is **not** a configured key: treating it as one is
    /// what turns "not set up yet" into a 401 on the first message.
    #[test]
    fn the_shipped_placeholder_does_not_count_as_configured() {
        let mut p = provider();
        p.api_key = Some("sk-...".into());
        assert!(!p.is_configured());
        p.api_key = Some("   ".into());
        assert!(!p.is_configured());
        p.api_key = Some("sk-real-looking".into());
        assert!(p.is_configured());
    }

    #[test]
    fn rgb_color_accepts_documented_forms() {
        let a = RgbColor { r: 0x1A, g: 0x2B, b: 0x3C, a: 255 };
        assert_eq!(color("\"#1A2B3C\""), Some(a));
        assert_eq!(color("\"1a2b3c\""), Some(a), "a leading # is optional, and case does not matter");
        assert_eq!(
            color("\"#1A2B3C80\""),
            Some(RgbColor { r: 0x1A, g: 0x2B, b: 0x3C, a: 0x80 })
        );
        assert_eq!(color("\"#abc\""), Some(RgbColor { r: 0xAA, g: 0xBB, b: 0xCC, a: 255 }));
        assert_eq!(color("[26, 43, 60]"), Some(a));
        assert_eq!(
            color("[26, 43, 60, 128]"),
            Some(RgbColor { r: 26, g: 43, b: 60, a: 128 })
        );
    }

    #[test]
    fn rgb_color_rejects_garbage() {
        for bad in ["\"#12345\"", "\"#GGGGGG\"", "\"red\"", "[1, 2]", "[]"] {
            assert!(color(bad).is_none(), "{bad} must not be accepted");
        }
    }

    /// The default ThemeConfig must be a zero override, and the metric defaults must equal the constants hard-coded...
    #[test]
    fn default_theme_is_zero_override() {
        let t = ThemeConfig::default();
        assert!(t.mode.is_none(), "the default does not pick light or dark; that is left to the stored preference / built-in default");

        let all_none = |p: &PaletteConfig| {
            p.bg.is_none()
                && p.panel.is_none()
                && p.surface.is_none()
                && p.surface_hi.is_none()
                && p.border.is_none()
                && p.border_strong.is_none()
                && p.text.is_none()
                && p.text_dim.is_none()
                && p.text_faint.is_none()
                && p.ink.is_none()
                && p.ink_soft.is_none()
                && p.user_bg.is_none()
                && p.code_bg.is_none()
                && p.code_chip.is_none()
                && p.quote_bar.is_none()
                && p.reverse_bg.is_none()
                && p.on_reverse.is_none()
                && p.shadow.is_none()
        };
        assert!(all_none(&t.light));
        assert!(all_none(&t.dark));

        let m = &t.metrics;
        assert_eq!(m.body_size, 16.0);
        assert_eq!(m.heading_size, 21.0);
        assert_eq!(m.button_size, 15.0);
        assert_eq!(m.small_size, 13.0);
        assert_eq!(m.mono_size, 14.5);
        assert_eq!(m.card_radius, 12);
        assert_eq!(m.item_spacing, [10.0, 10.0]);
        assert_eq!(m.button_padding, [14.0, 9.0]);
        assert_eq!(m.interact_height, 30.0);
        assert_eq!(m.indent, 22.0);

        assert!(t.fonts.cjk.is_none());
        assert!(t.fonts.bold.is_none());
        assert!(t.fonts.serif.is_none());
        assert!(t.fonts.brand.is_none());
        assert!(t.fonts.mono.is_none());
    }

    #[test]
    fn partial_overrides_only_touch_listed_fields() {
        let t: ThemeConfig = toml::from_str(
            r##"
            mode = "light"

            [metrics]
            body_size = 18.0
            card_radius = 4

            [dark]
            text = "#FF0000"

            [fonts]
            cjk = 'C:\Windows\Fonts\simkai.ttf'
            "##,
        )
        .expect("the theme section should parse");

        assert_eq!(t.mode.as_deref(), Some("light"));
        assert_eq!(t.metrics.body_size, 18.0);
        assert_eq!(t.metrics.card_radius, 4);
        assert_eq!(t.metrics.heading_size, 21.0, "metrics that were not written keep their defaults");
        assert_eq!(t.dark.text, Some(RgbColor { r: 0xFF, g: 0, b: 0, a: 255 }));
        assert!(t.dark.bg.is_none(), "only text was changed; the other 17 entries are unaffected");
        assert!(t.light.text.is_none(), "editing dark must not touch light");
        assert_eq!(t.fonts.cjk.as_deref(), Some(r"C:\Windows\Fonts\simkai.ttf"));
        assert!(t.fonts.mono.is_none());
    }

    #[test]
    fn config_theme_section_is_nested_under_theme() {
        let c: Config = toml::from_str(
            r##"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"

            [theme]
            mode = "light"

            [theme.metrics]
            body_size = 20.0

            [theme.dark]
            text = "#123456"
            "##,
        )
        .expect("the whole config should parse");

        assert_eq!(c.theme.mode.as_deref(), Some("light"));
        assert_eq!(c.theme.metrics.body_size, 20.0);
        assert_eq!(c.theme.metrics.indent, 22.0);
        assert_eq!(c.theme.dark.text, Some(RgbColor { r: 0x12, g: 0x34, b: 0x56, a: 255 }));
        assert!(c.theme.light.text.is_none());
    }

    /// A `--theme` file must accept both forms: bare theme / with a `[theme]` table.
    /// (`themes/*.toml` uses the latter, so the whole block can be pasted straight into `config.toml`.)
    #[test]
    fn theme_file_accepts_bare_and_wrapped_shapes() {
        let bare = ThemeConfig::from_toml_text(
            r##"
            mode = "dark"

            [metrics]
            body_size = 19.0

            [dark]
            text = "#00FF00"
            "##,
        )
        .expect("a bare theme should parse");
        assert_eq!(bare.mode.as_deref(), Some("dark"));
        assert_eq!(bare.metrics.body_size, 19.0);
        assert_eq!(bare.dark.text, Some(RgbColor { r: 0, g: 0xFF, b: 0, a: 255 }));

        let wrapped = ThemeConfig::from_toml_text(
            r##"
            [theme]
            mode = "light"

            [theme.metrics]
            body_size = 19.0

            [theme.light]
            text = "#00FF00"
            "##,
        )
        .expect("a theme file with a [theme] table should parse");
        assert_eq!(wrapped.mode.as_deref(), Some("light"));
        assert_eq!(wrapped.metrics.body_size, 19.0);
        assert_eq!(wrapped.light.text, Some(RgbColor { r: 0, g: 0xFF, b: 0, a: 255 }));
        assert!(wrapped.dark.text.is_none());
    }

    /// Shipped example theme files must really be loadable — this test is aimed exactly at the bug of
    /// "the example file uses `[theme]` nesting while the loader only accepts a bare theme".
    #[test]
    fn shipped_theme_files_load() {
        for f in ["themes/paper.toml", "themes/ink.toml", "themes/high-contrast.toml"] {
            let t = ThemeConfig::load(Path::new(f)).unwrap_or_else(|e| panic!("{f} should load: {e:#}"));
            assert!(t.mode.is_some(), "{f} should declare mode");
        }

        // paper / ink are complete copies of the built-in palettes, all 18 fields should be covered
        let paper = ThemeConfig::load(Path::new("themes/paper.toml")).unwrap();
        assert!(paper.light.bg.is_some() && paper.light.text.is_some());
        assert!(paper.light.on_reverse.is_some() && paper.light.shadow.is_some());

        let ink = ThemeConfig::load(Path::new("themes/ink.toml")).unwrap();
        assert!(ink.dark.bg.is_some() && ink.dark.text.is_some());
        assert!(ink.dark.on_reverse.is_some() && ink.dark.shadow.is_some());

        // high-contrast defines both light and dark (otherwise the other half falls back to the built-in palette and looks mixed)
        let hc = ThemeConfig::load(Path::new("themes/high-contrast.toml")).unwrap();
        assert!(hc.light.bg.is_some() && hc.dark.bg.is_some());
        assert_eq!(hc.metrics.body_size, 17.0);
    }

    /// An old config (with no `[theme]` at all) must still parse — stage 2 must not break existing config.toml.
    #[test]
    fn config_without_theme_section_still_parses() {        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"
            "#,
        )
        .expect("an old config without [theme] must still parse");

        assert!(c.theme.mode.is_none());
        assert!(c.theme.dark.text.is_none());
        assert_eq!(c.theme.metrics.body_size, 16.0);
    }

    /// The nested form `[hook.tool_args_default.<tool name>]` must parse into "tool → which keys to fill"
    #[test]
    fn hook_section_parses_nested_tool_args_default() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"

            [hook.tool_args_default.fs_read_file]
            encoding = "utf-8"

            [hook.tool_args_default.fs_write_file]
            create_dirs = true
            "#,
        )
        .expect("a config with [hook] should parse");

        let d = &c.hook.tool_args_default;
        assert_eq!(d.len(), 2, "one entry per tool");
        assert_eq!(d["fs_read_file"]["encoding"].as_str(), Some("utf-8"));
        assert_eq!(d["fs_write_file"]["create_dirs"].as_bool(), Some(true));
    }

    /// `retry` — the retry budget lives here since 2026-10-23 (it used to be `hook max_llm_retries`, which was only...
    #[test]
    fn retry_section_defaults_to_two_attempts_and_parses_overrides() {
        let p = "[provider]\nbase_url = \"https://example.com/v1\"\nmodel = \"m\"\napi_key = \"k\"\n";

        let d: Config = toml::from_str(p).expect("omitting [retry] should parse");
        assert_eq!(d.retry.attempts, 2, "two retries by default: one 429 used to cost the whole turn");
        assert_eq!(d.retry.base_ms, 1_000);
        assert_eq!(d.retry.max_ms, 30_000);

        let c: Config = toml::from_str(&format!("{p}\n[retry]\nattempts = 0\n")).expect("should parse");
        assert_eq!(c.retry.attempts, 0, "0 = the old behaviour, for anyone who wants it");
        assert_eq!(c.retry.base_ms, 1_000, "a partial table keeps the other defaults");

        let c: Config =
            toml::from_str(&format!("{p}\n[retry]\nattempts = 5\nbase_ms = 200\nmax_ms = 900\n"))
                .expect("should parse");
        assert_eq!((c.retry.attempts, c.retry.base_ms, c.retry.max_ms), (5, 200, 900));
    }

    /// `workplace` is the official section name, `workspace` is the old name.
    #[test]
    fn workplace_section_accepts_the_legacy_name() {
        const BASE: &str = r#"
[provider]
base_url = "https://example.com/v1"
model = "m"
api_key = "k"
"#;
        let new: Config = toml::from_str(&format!("{BASE}\n[workplace]\nmode = \"default\"\n"))
            .expect("[workplace] should parse");
        let old: Config = toml::from_str(&format!("{BASE}\n[workspace]\nmode = \"default\"\n"))
            .expect("the old section name [workspace] must still parse");
        assert_eq!(old.workplace, new.workplace, "the old section name must read back exactly like the new one");
        assert_eq!(new.workplace.mode, "default", "the value must really be read in, not fall back to the default");

        // Both names written (which theoretically should not happen): better to error than silently pick one — picking...
        let both =
            format!("{BASE}\n[workplace]\nmode = \"default\"\n[workspace]\nmode = \"full\"\n");
        assert!(toml::from_str::<Config>(&both).is_err(), "writing both names for the same section should error");
    }

    /// An old config (with no `hook` at all) must still parse, and the hook table must be empty — stage 4, like stage...
    #[test]
    fn factory_default_is_the_confined_mode() {
        assert_eq!(WorkplaceConfig::default().mode, "default");
        let c: Config = toml::from_str(
            r#"
[provider]
base_url = "https://example.com/v1"
model = "m"
api_key = "k"
"#,
        )
        .expect("omitting [workplace] should parse");
        assert_eq!(c.workplace.mode, "default", "omitting this section = factory default = restricted tier");
        assert_eq!(c.workplace.root, None);
    }

    #[test]
    fn config_without_hook_section_still_parses() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"
            "#,
        )
        .expect("an old config without [hook] must still parse");
        assert!(c.hook.tool_args_default.is_empty());
    }

    /// The defaults of `[loop]`: standard + no trimming.
    /// This is the data-layer guarantee that "no `[loop]` ⇒ behaviour byte-identical to stage 5".
    #[test]
    fn loop_defaults_to_standard_without_trimming() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"
            "#,
        )
        .expect("an old config without [loop] must still parse");
        assert_eq!(c.loop_config.implementor, "standard");
        assert_eq!(c.loop_config.max_rounds, 0, "the context is not trimmed by default");
        assert_eq!(LoopConfig::default().implementor, "standard");
    }

    /// Both keys of the `[loop]` section must be recognized — and, incidentally, pin "the TOML key name `impl` must not be dragged off course by the Rust keyword issue".
    #[test]
    fn loop_section_parses_impl_and_max_rounds() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"

            [loop]
            impl = "standard"
            max_rounds = 6
            "#,
        )
        .expect("a config with [loop] should parse");
        assert_eq!(c.loop_config.implementor, "standard");
        assert_eq!(c.loop_config.max_rounds, 6);
    }

    /// `loop max_iters_per_turn` — the work ceiling, added 2026-10-23 next to the context ceiling.
    #[test]
    fn the_per_turn_request_ceiling_is_separate_from_the_context_cap() {
        let p = "[provider]\nbase_url = \"https://example.com/v1\"\nmodel = \"m\"\napi_key = \"k\"\n";

        let d: Config = toml::from_str(p).expect("omitting [loop] should parse");
        assert_eq!(d.loop_config.max_iters_per_turn, 0, "default 0 = no ceiling");

        let c: Config = toml::from_str(&format!("{p}\n[loop]\nmax_iters_per_turn = 50\n"))
            .expect("should parse");
        assert_eq!(c.loop_config.max_iters_per_turn, 50);
        assert_eq!(c.loop_config.max_rounds, 0, "setting one must not touch the other");

        let c: Config = toml::from_str(&format!("{p}\n[loop]\nmax_rounds = 3\n"))
            .expect("should parse");
        assert_eq!(c.loop_config.max_iters_per_turn, 0);
        assert_eq!(c.loop_config.max_rounds, 3);
    }

    /// With only `max_rounds` written, `impl` takes the default — like the other tables, "a whole table may omit fields".
    #[test]
    fn loop_section_partial_still_parses() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"

            [loop]
            max_rounds = 3
            "#,
        )
        .expect("[loop] with only one key should still parse");
        assert_eq!(c.loop_config.implementor, "standard");
        assert_eq!(c.loop_config.max_rounds, 3);
    }

    /// The defaults of `sandbox`: on, and exactly the set with "zero impact on legitimate plugins".
    #[test]
    fn sandbox_defaults_are_on_and_conservative() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"
            "#,
        )
        .expect("an old config without [sandbox] must still parse");

        assert!(c.sandbox.enabled, "the fence is on by default");
        assert_eq!(c.sandbox.max_processes, 8, "the default must allow child processes (0 is what locks it down)");
        assert_eq!(c.sandbox.max_memory_mb, 0, "the memory cap is off by default (exceeding it kills the process, so it must not default on)");
        assert!(c.sandbox.ui_limits);

        let d = SandboxConfig::default();
        assert!(d.enabled && d.max_processes == 8 && d.max_memory_mb == 0 && d.ui_limits);
        // The `+1` is the **platform convention** (`ACTIVE_PROCESS` includes the plugin itself). Pin that it
        // is added only once, and that 0 really means "not a single child process allowed", not "unlimited".
        assert_eq!(d.job_process_limit(), 9, "configured 8 => 9 slots in the Job (including the plugin itself)");
        assert_eq!(
            SandboxConfig { max_processes: 0, ..SandboxConfig::default() }.job_process_limit(),
            1,
            "0 = only the plugin itself => not one child process can start"
        );
        assert_eq!(
            SandboxConfig { max_processes: u32::MAX, ..SandboxConfig::default() }.job_process_limit(),
            u32::MAX,
            "an extreme value must not wrap to 0 — inside a Job, 0 means \"unlimited\", the exact opposite"
        );
    }

    /// `plugins` defaults: lazy on, 60s idle, uncapped resident count, no eager list — and an old config that has...
    #[test]
    fn plugins_default_to_lazy_loading() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"
            "#,
        )
        .expect("an old config without [plugins] must still parse");
        assert!(c.plugins.lazy, "lazy loading is the default (starting every plugin at boot is the thing being fixed)");
        assert_eq!(c.plugins.idle_timeout_secs, 60);
        assert_eq!(c.plugins.max_resident, 0, "0 = no cap on simultaneously live processes");
        assert!(c.plugins.eager.is_empty());

        // A partial table keeps the code default for every key it omits, the same as [sandbox]/[auth].
        let c2: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"

            [plugins]
            idle_timeout_secs = 0
            eager = ["comp"]
            "#,
        )
        .unwrap();
        assert_eq!(c2.plugins.idle_timeout_secs, 0, "0 = never reap");
        assert_eq!(c2.plugins.eager, vec!["comp"]);
        assert!(c2.plugins.lazy, "a key the section does not mention keeps its default");
    }

    /// With only one key written the rest take defaults (like the other tables, "a whole table may omit fields").
    #[test]
    fn sandbox_section_partial_still_parses() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"

            [sandbox]
            max_processes = 4
            max_memory_mb = 512
            "#,
        )
        .expect("[sandbox] with only two keys should still parse");

        assert_eq!(c.sandbox.max_processes, 4);
        assert_eq!(c.sandbox.max_memory_mb, 512);
        assert!(c.sandbox.enabled, "keys that were not written keep their defaults");
        assert!(c.sandbox.ui_limits);
    }

    /// Background jobs are on by default, but bounded: four trees, 64 processes each, two hours.
    #[test]
    fn background_job_defaults_are_bounded() {
        let d = ShellConfig::default();
        assert_eq!(d.background_max_jobs, 4);
        assert_eq!(d.background_max_processes, 64);
        assert_eq!(d.background_max_hours, 2);
    }

    /// The path to turn the fence off must really exist (it is the only way back).
    #[test]
    fn sandbox_can_be_turned_off() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"

            [sandbox]
            enabled = false
            "#,
        )
        .expect("should parse");
        assert!(!c.sandbox.enabled);
    }

    /// The two provider defaults, checked through serde rather than by reading the struct: the defaults live...
    #[test]
    fn a_minimal_config_gets_the_documented_provider_defaults() {
        let c: Config = toml::from_str(
            "[provider]\nbase_url = \"https://example.invalid\"\nmodel = \"m\"\n",
        )
        .expect("a config with only the required provider keys must parse");
        assert_eq!(c.provider.max_tokens, Some(DEFAULT_MAX_TOKENS));
        assert_eq!(c.provider.context_window, Some(DEFAULT_CONTEXT_WINDOW));

        // ...and a default config therefore DOES compact: 600000 x 0.8 - 32000.
        let p = crate::compact::Policy::from_config(&c).expect("the defaults give a budget");
        assert_eq!(p.budget, 448_000);

        // An explicit window still wins on its own terms (10k x 0.8 - 32000 would go negative, so
        // the budget floors at SMALLEST_BUDGET rather than at zero).
        let mut small = c.clone();
        small.provider.context_window = Some(10_000);
        assert_eq!(
            crate::compact::Policy::from_config(&small).unwrap().budget,
            crate::compact::SMALLEST_BUDGET
        );
    }

    #[derive(Deserialize)]
    struct RecoveryWrap {
        #[serde(default)]
        recovery: RecoveryConfig,
    }

    #[test]
    fn recovery_defaults_are_the_documented_ones() {
        let w: RecoveryWrap = toml::from_str("").unwrap();
        assert!(w.recovery.enabled);
        assert_eq!(w.recovery.keep, 30);
        assert_eq!(w.recovery.max_total_mb, 512);
        assert_eq!(w.recovery.max_file_mb, 64);
        assert_eq!(w.recovery.shell_guard, ShellGuard::Strict);
        assert!(w.recovery.exclude.iter().any(|p| p == ".aox/"));
        assert!(w.recovery.exclude.iter().any(|p| p == "*.exe"), "binaries are not source");
    }

    #[test]
    fn a_bad_shell_guard_is_a_parse_error() {
        let e = toml::from_str::<RecoveryWrap>("[recovery]\nshell_guard = \"mostly\"")
            .err()
            .expect("an unknown guard must not silently fall back");
        assert!(format!("{e}").contains("shell_guard"), "{e}");
    }
}
