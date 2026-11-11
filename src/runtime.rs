use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{Mutex, RwLock};

use crate::auth::{ConfigPolicy, Policy};
use crate::config::{Config, RuntimesConfig};
use crate::plugin::PluginHandle;
use crate::prompt::PromptSegment;
use crate::tool::{Tool, ToolKind, ToolOutput};

/// Shared runtime state: the tool registry + plugin management + the session-level authorization allowlist.
pub struct Runtime {
    pub config: Config,
    /// The authorization policy (the Policy seam, see `src/auth.rs`).
    pub auth: Arc<dyn Policy>,
    /// The session storage backend (the Store seam, see `src/store.rs`).
    pub store: Arc<dyn crate::store::Store>,
    /// The application root. Plugin subprocesses use it as their working directory, so
    /// relative paths inside plugins match the host's view.
    pub root: PathBuf,
    pub plugins_dir: PathBuf,
    /// The interpreter resolution result (a snapshot of `config.runtimes` after the three-tier resolution).
    runtimes: std::sync::RwLock<RuntimesConfig>,
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    pub plugins: Mutex<HashMap<String, Arc<PluginHandle>>>,
    /// Long-lived shell jobs (`shell_job`, see `src/jobs.rs`).
    pub jobs: Mutex<crate::jobs::Jobs>,
    /// Tool names the user chose "always allow" for in this session
    pub session_allowed: RwLock<HashSet<String>>,
    /// The system prompt's segment registry (`src/prompt.rs`).
    prompt_segments: std::sync::RwLock<Vec<PromptSegment>>,
    /// The loop hook chain (the Loop seam, see `src/hook.rs`).
    pub hooks: crate::hook::HookChain,
    /// The main-loop implementation (the Loop seam's second form, see `src/agent_loop.rs`).
    pub loop_impl: Arc<dyn crate::agent_loop::AgentLoop>,
    /// The plugin subprocess fence (Job Object, see `src/sandbox.rs`).
    pub sandbox: crate::sandbox::Sandbox,
    /// The model's file boundary (the workplace, see `src/workplace.rs`).
    pub workplace: crate::workplace::Workplace,
    /// Task-level file checkpoints and rollback (see `src/recovery`).
    pub recovery: crate::recovery::RecoveryManager,
    /// The live model provider (see `src/provider_live.rs`).
    pub provider: Arc<crate::provider_live::LiveProvider>,

    /// The space registry (see `src/space.rs`): which workspaces exist, what they are called, and which conversation...
    #[allow(dead_code)]
    pub spaces: std::sync::RwLock<crate::space::Registry>,
}

/// One tool as the discovery search sees it (see `crate::tool::discover`).
pub struct ToolEntry {
    pub name: String,
    /// `true` when the tool belongs to a plugin — the only kind that may be held back from the
    /// request (see `crate::tool::scope` for why the line is drawn there).
    pub plugin: bool,
    pub description: String,
    /// The whole `{"type":"function","function":{…}}` object: the search prints `function.parameters`
    /// so the model can call the tool on the very next round, without a second lookup.
    pub schema: serde_json::Value,
}

impl Runtime {
    /// Assemble the runtime. It returns `Result` so that "a wrong storage backend name" errors at startup (an...
    pub fn new(config: Config, root: PathBuf, plugins_dir: PathBuf) -> Result<Arc<Self>> {
        // Three-tier interpreter resolution: explicit config path > built-in runtime under the app directory > PATH. Done...
        let mut resolved = config.runtimes.clone();
        crate::runtimes::resolve(&mut resolved, &root);
        let auth: Arc<dyn Policy> = Arc::new(ConfigPolicy::from_config(&config.auth));
        // Compute the prompt registry before `config` is moved into Self.
        let prompt_segments = crate::prompt::registry_from_config(&config.prompt);
        // Fix the storage backend before `config` is moved into Self (it needs config.store / config.history).
        let store = crate::store::backend(&config.store.backend, &root, config.history.keep)?;
        // The space registry, read before `root` is moved into Self.
        let spaces = crate::space::Registry::load(&root);
        // The loop hook chain is likewise fixed before `config` is moved. `?` is used here: something unintelligible...
        let hooks = crate::hook::HookChain::new(crate::hook::registry_from_config(&config.hook)?);
        // The main-loop implementation is likewise fixed before `config` is moved. `?` is used here: an unrecognized name...
        let loop_impl = crate::agent_loop::build(&config.loop_config)?;
        // Take the fence config before `config` is moved into Self (`Sandbox` is just a cheap config holder).
        let sandbox = crate::sandbox::Sandbox::new(config.sandbox.clone());
        // The workplace is likewise fixed before `config` is moved. `?` is used here: an unrecognized `workplace mode`...
        let workplace = crate::workplace::Workplace::new(config.workplace.clone(), &root)?;
        let recovery = crate::recovery::RecoveryManager::new(config.recovery.clone(), root.clone());
        // Before `config` is moved: the live provider keeps its own snapshot of `provider`, and it is built here (never...
        let mut book = crate::modelbook::Book::load(&root);
        book.adopt(&config.provider, &root);
        let provider_cfg = book.active_config().unwrap_or_else(|| config.provider.clone());
        let provider = crate::provider_live::LiveProvider::new(provider_cfg);
        let rt = Arc::new(Self {
            config,
            auth,
            store,
            root,
            plugins_dir,
            runtimes: std::sync::RwLock::new(resolved),
            tools: RwLock::new(HashMap::new()),
            plugins: Mutex::new(HashMap::new()),
            jobs: Mutex::new(crate::jobs::Jobs::new()),
            session_allowed: RwLock::new(HashSet::new()),
            prompt_segments: std::sync::RwLock::new(prompt_segments),
            hooks,
            loop_impl,
            sandbox,
            workplace,
            recovery,
            provider,
            spaces: std::sync::RwLock::new(spaces),
        });
        rt.start_job_reaper();
        Ok(rt)
    }

    /// Terminate `shell_job` trees that outlive `shell background_max_hours`, even when nobody is talking to the model.
    fn start_job_reaper(self: &Arc<Self>) {
        let hours = self.config.shell.background_max_hours;
        if hours == 0 {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak = Arc::downgrade(self);
        handle.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let Some(rt) = weak.upgrade() else { break };
                let _ = rt.jobs.lock().await.expire(hours);
            }
        });
    }

    /// A snapshot of the space registry.
    #[allow(dead_code)] // read by the history overlay in the next phase; see the `spaces` field
    pub fn spaces(&self) -> crate::space::Registry {
        self.spaces.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The currently registered prompt segments (a clone; the caller may sort/filter freely).
    pub fn prompt_segments(&self) -> Vec<PromptSegment> {
        self.prompt_segments.read().unwrap().clone()
    }

    /// Append a prompt segment. When the id already exists it replaces it wholesale — the same semantics...
    pub fn add_prompt_segment(&self, seg: PromptSegment) {
        let mut g = self.prompt_segments.write().unwrap();
        match g.iter_mut().find(|s| s.id == seg.id) {
            Some(slot) => *slot = seg,
            None => g.push(seg),
        }
    }

    /// Drop every prompt segment contributed by `plugin`.
    pub fn remove_prompt_segments_by_plugin(&self, plugin: &str) {
        let prefix = format!("{plugin}:");
        self.prompt_segments.write().unwrap().retain(|s| !s.id.starts_with(&prefix));
    }

    /// Append a loop hook at runtime (component-contributed hooks come in here).
    pub fn add_hook(&self, hook: Arc<dyn crate::hook::Hook>) {
        self.hooks.push(hook);
    }

    /// The currently effective interpreter config (a clone).
    pub fn runtimes(&self) -> RuntimesConfig {
        self.runtimes.read().unwrap().clone()
    }

    /// Redo the three-tier resolution.
    pub fn refresh_runtimes(&self) {
        let mut g = self.runtimes.write().unwrap();
        *g = self.config.runtimes.clone();
        let root = self.root.clone();
        crate::runtimes::resolve(&mut g, &root);
    }

    pub async fn register(&self, tool: Arc<dyn Tool>) -> Result<()> {
        let mut tools = self.tools.write().await;
        let name = tool.name().to_string();
        if tools.contains_key(&name) {
            anyhow::bail!(crate::tf!("tool name conflict: {} (tool names are globally unique; name them \"<plugin>_<function>\")", name));
        }
        tools.insert(name, tool);
        Ok(())
    }

    pub async fn unregister(&self, name: &str) {
        self.tools.write().await.remove(name);
    }

    /// Regenerated every round — a plugin tool the model just injected is visible in the very next round. ️...
    #[allow(dead_code)]
    pub async fn tool_schemas(&self) -> Vec<serde_json::Value> {
        let tools = self.tools.read().await;
        let mut v: Vec<_> = tools.values().map(|t| t.schema()).collect();
        v.sort_by(|a, b| {
            a["function"]["name"].as_str().unwrap_or("").cmp(b["function"]["name"].as_str().unwrap_or(""))
        });
        v
    }

    /// The same list as Runtime::tool_schemas, minus the tools this conversation is not advertising right now (see...
    pub async fn tool_schemas_scoped(&self, scope: &crate::tool::scope::Scope) -> Vec<serde_json::Value> {
        let tools = self.tools.read().await;
        let hide = scope.hides(tools.len());
        let mut v: Vec<_> = tools
            .values()
            .filter(|t| {
                !hide || scope.advertises(t.name(), matches!(t.kind(), ToolKind::Plugin(_)))
            })
            .map(|t| t.schema())
            .collect();
        v.sort_by(|a, b| {
            a["function"]["name"].as_str().unwrap_or("").cmp(b["function"]["name"].as_str().unwrap_or(""))
        });
        v
    }

    /// A snapshot of the registry for the tool-discovery search: name, whether it belongs to a plugin, and the full...
    pub async fn tool_entries(&self) -> Vec<ToolEntry> {
        let tools = self.tools.read().await;
        let mut v: Vec<ToolEntry> = tools
            .values()
            .map(|t| ToolEntry {
                name: t.name().to_string(),
                plugin: matches!(t.kind(), ToolKind::Plugin(_)),
                description: t.description().to_string(),
                schema: t.schema(),
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Every tool registered by one plugin (by plugin name).
    pub async fn plugin_tool_names(&self, plugin: &str) -> Vec<String> {
        let tools = self.tools.read().await;
        let mut v: Vec<String> = tools
            .values()
            .filter(|t| matches!(t.kind(), ToolKind::Plugin(p) if p == plugin))
            .map(|t| t.name().to_string())
            .collect();
        v.sort();
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

    /// Whether one call of this tool counts as "read" or "writes to disk" — this is what the workplace boundary uses...
    pub async fn tool_access(&self, name: &str) -> crate::workplace::Access {
        match self.tools.read().await.get(name) {
            Some(t) => t.access(),
            None => crate::workplace::Access::Write,
        }
    }

    /// The capabilities requested by this call (from a plugin's `contributes caps`, since phase 6 · 6b).
    pub async fn caps_for(&self, kind: &ToolKind) -> Vec<String> {
        let ToolKind::Plugin(name) = kind else {
            return Vec::new();
        };
        self.plugins
            .lock()
            .await
            .get(name)
            .map(|h| h.contributes.caps.clone())
            .unwrap_or_default()
    }

    /// The two numbers a running call reports to the UI: `(cap_secs, expect_secs)`.
    pub async fn call_hints(&self, name: &str, args: &serde_json::Value) -> (Option<u64>, Option<u64>) {
        let tool = { self.tools.read().await.get(name).cloned() };
        match tool {
            Some(t) => (t.cap_hint(args), t.expect_hint(args)),
            None => (None, None),
        }
    }

    pub async fn execute(self: &Arc<Self>, name: &str, args: serde_json::Value) -> ToolOutput {
        let tool = { self.tools.read().await.get(name).cloned() };
        let Some(tool) = tool else {
            return ToolOutput::err(crate::tf!("no such tool: {}", name));
        };
        // Fallback timeout: plugin tools already have a tool_timeout_secs timeout + restart-and-retry internally; add 30...
        let timeout = match tool.timeout_hint(&args) {
            Some(d) => d + Duration::from_secs(10),
            None => Duration::from_secs(self.config.agent.tool_timeout_secs + 30),
        };
        match tokio::time::timeout(timeout, tool.call(args, self)).await {
            Ok(out) => out,
            Err(_) => ToolOutput::err(crate::tf!("tool {} timed out ({} s)", name, timeout.as_secs())),
        }
    }
}
