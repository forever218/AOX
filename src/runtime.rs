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

/// Shared runtime state: the tool registry + plugin management + the session-level
/// authorization allowlist.
/// Meta tools use it to "grow" the system itself.
pub struct Runtime {
    pub config: Config,
    /// The authorization policy (the Policy seam, see `src/auth.rs`).
    ///
    /// Holds a trait object rather than a concrete type: `Agent` only asks "does this call
    /// get through", and doesn't care whether the decision was read from
    /// `config.toml [auth]` or computed elsewhere.
    /// `Send + Sync` is guaranteed by the `Policy` bound — it is shared between the agent
    /// thread and the GUI main thread.
    pub auth: Arc<dyn Policy>,
    /// The session storage backend (the Store seam, see `src/store.rs`).
    ///
    /// Fixed by `config.toml [store] backend` in `Runtime::new`; the agent thread uses it
    /// to create/open conversation files, and the UI side builds a separate read-only
    /// instance of the same backend (`location` points at the same place).
    pub store: Arc<dyn crate::store::Store>,
    /// The application root. Plugin subprocesses use it as their working directory, so
    /// relative paths inside plugins match the host's view.
    pub root: PathBuf,
    pub plugins_dir: PathBuf,
    /// The interpreter resolution result (a snapshot of `config.runtimes` after the
    /// three-tier resolution).
    ///
    /// Kept as a separate copy instead of reading `config.runtimes` directly so that
    /// "installing a runtime takes effect immediately" becomes possible: refresh this in
    /// place after installation and the next plugin subprocess spawn uses the new
    /// interpreter, with no process restart needed. A tiny struct, so clone cost is
    /// negligible.
    ///
    /// Uses `std::sync::RwLock` rather than tokio's: reads/writes happen only in
    /// synchronous contexts and the lock is never held across an `await` —
    /// `spawn_command` is a synchronous function and must be able to take it directly.
    runtimes: std::sync::RwLock<RuntimesConfig>,
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    pub plugins: Mutex<HashMap<String, Arc<PluginHandle>>>,
    /// Tool names the user chose "always allow" for in this session
    pub session_allowed: RwLock<HashSet<String>>,
    /// The system prompt's **segment registry** (`src/prompt.rs`).
    ///
    /// The built-in `core` / `protocol` segments and the appended segments in
    /// `config.toml [[prompt.segment]]` are all registered in `Runtime::new`; the run-env
    /// segment and the project-rules segment are generated on demand by the `prompt`
    /// module (they depend on `root` and the filesystem, so they don't belong baked into
    /// the registry).
    ///
    /// Uses `std::sync::RwLock` rather than tokio's: reads/writes happen only in
    /// synchronous contexts (`Agent::new` → `prompt::build`), and the lock is never held
    /// across an `await`.
    prompt_segments: std::sync::RwLock<Vec<PromptSegment>>,
    /// The loop hook chain (the Loop seam, see `src/hook.rs`).
    ///
    /// Fixed by `config.toml [hook]` in `Runtime::new`. Under the default config it is an
    /// **empty chain**, in which case `Agent::run_loop`'s five hook points all degrade to
    /// "check the length and return" — that is how this phase achieves "no hooks = the
    /// behaviour is equivalent to today, item by item".
    ///
    /// Like `store` / `auth` it holds a concrete type rather than adding another layer of
    /// trait object: the chain itself is that indirection (what it holds inside is the
    /// `Vec<Arc<dyn Hook>>`), and wrapping another ABC around it would be pointless.
    pub hooks: crate::hook::HookChain,
    /// The main-loop implementation (the Loop seam's second form, see `src/agent_loop.rs`).
    ///
    /// Fixed by `config.toml [loop] impl` in `Runtime::new`; **an unrecognized name errors
    /// at startup**, with no silent fallback to `standard`. `Agent::run_turn` takes a loop
    /// from here to run — so "swap in a different execution model" is a config change, not
    /// a code change.
    pub loop_impl: Arc<dyn crate::agent_loop::AgentLoop>,
    /// The plugin subprocess **fence** (Job Object, see `src/sandbox.rs`).
    ///
    /// Fixed by `config.toml [sandbox]` in `Runtime::new`. It is itself only a holder of
    /// config; the real kernel object is created fresh on every spawn (one Job per
    /// plugin) — so changing this **while a plugin is running** does not affect an
    /// already-started process, only the next spawn / restart.
    pub sandbox: crate::sandbox::Sandbox,
    /// The model's **file boundary** (the workplace, see `src/workplace.rs`).
    ///
    /// Sits beside `sandbox` but manages something else: `sandbox` restricts how plugin
    /// subprocesses run, while `workplace` restricts "which files can be touched this
    /// time". Under `full` it is unrestricted; under `default` / `fixed` writes and
    /// deletes may only land in the designated plot (reads are unrestricted).
    /// **The config default is `default` (restricted)** — changed from `full` on
    /// 2026-10-05, honouring the user's "restricted by default on every startup". So
    /// omitting `[workplace]` now **no longer** means "behaviour unchanged".
    ///
    /// Internally a `std::sync::Mutex`, read/written only in synchronous contexts (the
    /// decision runs on the tool-call path) — the same reason as `runtimes` /
    /// `prompt_segments`: the lock is never held across an `await`.
    pub workplace: crate::workplace::Workplace,
    /// The **space registry** (see `src/space.rs`): which workspaces exist, what they are called, and
    /// which conversation sits where.
    ///
    /// Lives on the runtime rather than in each front end so the agent thread and the UI read the same
    /// list. A `RwLock` because the editing phase (creating / removing a workspace) writes it at
    /// runtime; today it is only read. The lock is never held across an `await` — the same rule as
    /// `runtimes` / `prompt_segments`.
    ///
    /// `allow(dead_code)`: written at startup and read by the overlay that groups history by workspace,
    /// which lands in the next phase — the phase that ships this field only needs it to exist and to
    /// have fed `Store::set_workspaces`.
    #[allow(dead_code)]
    pub spaces: std::sync::RwLock<crate::space::Registry>,
}

/// One tool as the **discovery search** sees it (see `crate::tool::discover`).
///
/// A borrowed-free snapshot rather than a reference into the registry: the search is `async` and
/// must not hold the tools lock while it formats a result the model will read.
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
    /// Assemble the runtime. It returns `Result` so that "a wrong storage backend name"
    /// errors **at startup** (an unrecognized `[store] backend` value fails outright
    /// rather than silently falling back to jsonl — see `store::backend`).
    pub fn new(config: Config, root: PathBuf, plugins_dir: PathBuf) -> Result<Arc<Self>> {
        // Three-tier interpreter resolution: explicit config path > built-in runtime under
        // the app directory > PATH.
        // Done here rather than at each entry point so the CLI / GUI / selftest all follow
        // the same path, with no logic fork like "the GUI runs but the CLI doesn't".
        //
        // Note: `config` itself is **left untouched**. The resolution result goes only
        // into the `runtimes` field — the presentation layer (/runtime report, system
        // prompt) needs the raw value to explain "where the interpreter came from", and
        // once an absolute path is written back into config, every origin gets misreported
        // as "specified in config.toml".
        let mut resolved = config.runtimes.clone();
        crate::runtimes::resolve(&mut resolved, &root);
        let auth: Arc<dyn Policy> = Arc::new(ConfigPolicy::from_config(&config.auth));
        // Compute the prompt registry before `config` is moved into Self.
        // Built-in segments are registered in the prompt module; config segments go in
        // here as well (same id overrides built-in).
        let prompt_segments = crate::prompt::registry_from_config(&config.prompt);
        // Fix the storage backend before `config` is moved into Self (it needs config.store / config.history).
        let store = crate::store::backend(&config.store.backend, &root, config.history.keep)?;
        // The space registry, read before `root` is moved into Self. Its only job at startup is to
        // tell the store **which workspaces to visit**: history is per workspace
        // (`<workspace>/.aox/history`), so listing every conversation needs the list of workspaces —
        // this call is the entire coupling between the registry and the storage seam.
        let spaces = crate::space::Registry::load(&root);
        store.set_workspaces(&spaces.workspace_roots(&root));
        // The loop hook chain is likewise fixed before `config` is moved.
        // `?` is used here: something unintelligible in `[hook]` (not an object, a value
        // that won't convert to JSON) errors **at startup** rather than quietly not taking
        // effect — the same principle as `[store] backend`.
        let hooks = crate::hook::HookChain::new(crate::hook::registry_from_config(&config.hook)?);
        // The main-loop implementation is likewise fixed before `config` is moved.
        // `?` is used here: an unrecognized name in `[loop] impl` errors **at startup**
        // rather than silently falling back to standard — the same principle as
        // `[store] backend`.
        let loop_impl = crate::agent_loop::build(&config.loop_config)?;
        // Take the fence config before `config` is moved into Self (`Sandbox` is just a cheap config holder).
        let sandbox = crate::sandbox::Sandbox::new(config.sandbox.clone());
        // The workplace is likewise fixed before `config` is moved. **`?` is used here**:
        // an unrecognized `[workplace] mode` value, or `fixed` without a `root`, both error
        // **at startup** — the same principle as `[store] backend` / `[loop] impl`
        // (something configured but not taking effect is the hardest to diagnose).
        let workplace = crate::workplace::Workplace::new(config.workplace.clone(), &root)?;
        Ok(Arc::new(Self {
            config,
            auth,
            store,
            root,
            plugins_dir,
            runtimes: std::sync::RwLock::new(resolved),
            tools: RwLock::new(HashMap::new()),
            plugins: Mutex::new(HashMap::new()),
            session_allowed: RwLock::new(HashSet::new()),
            prompt_segments: std::sync::RwLock::new(prompt_segments),
            hooks,
            loop_impl,
            sandbox,
            workplace,
            spaces: std::sync::RwLock::new(spaces),
        }))
    }

    /// A snapshot of the space registry.
    ///
    /// Cloned rather than borrowed: a registry is small (a handful of spaces), and handing out a
    /// reference would mean holding the lock across whatever the caller does next — the same hazard
    /// `Workplace::report` documents ("take a snapshot first, then release the lock").
    #[allow(dead_code)] // read by the history overlay in the next phase; see the `spaces` field
    pub fn spaces(&self) -> crate::space::Registry {
        self.spaces.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The currently registered prompt segments (a clone; the caller may sort/filter freely).
    pub fn prompt_segments(&self) -> Vec<PromptSegment> {
        self.prompt_segments.read().unwrap().clone()
    }

    /// Append a prompt segment. When the id already exists it **replaces** it wholesale —
    /// the same semantics as `[[prompt.segment]]` overriding a built-in segment in
    /// `config.toml` (last registered wins).
    ///
    /// Caller (since phase 6 · 6b): when a plugin activates it attaches the prompt segment
    /// contributed by the component — the body is fetched from the subprocess by
    /// `prompt/segment`, see `plugin::activate`.
    pub fn add_prompt_segment(&self, seg: PromptSegment) {
        let mut g = self.prompt_segments.write().unwrap();
        match g.iter_mut().find(|s| s.id == seg.id) {
            Some(slot) => *slot = seg,
            None => g.push(seg),
        }
    }

    /// Drop every prompt segment contributed by `plugin`.
    ///
    /// Segment ids are prefixed with the owning plugin (`<plugin>:<id>`, see `activate_slot`), which
    /// is the only thing that makes this decidable. Needed for the same reason as
    /// [`crate::hook::HookChain::remove_by_plugin`]: a hot swap must not leave behind a segment the
    /// new version no longer declares. On the create path nothing matches and this is a no-op.
    pub fn remove_prompt_segments_by_plugin(&self, plugin: &str) {
        let prefix = format!("{plugin}:");
        self.prompt_segments.write().unwrap().retain(|s| !s.id.starts_with(&prefix));
    }

    /// Append a loop hook at runtime (component-contributed hooks come in here).
    ///
    /// Why "at runtime" is needed: plugins are loaded **after** `Runtime::new`, whereas
    /// config-driven hooks are fixed inside `new`. A hook point takes a snapshot before
    /// iterating, so a hook added here takes effect immediately for the **next** hook-point
    /// call (see `hook::HookChain::snapshot`).
    ///
    /// The reverse direction (uninstall) needs no counterpart: `ComponentHook` looks up
    /// its handle by **plugin name** each time, so it stops working on its own the moment
    /// the plugin disappears from `plugins` (see the comment on `hook::ComponentHook`).
    pub fn add_hook(&self, hook: Arc<dyn crate::hook::Hook>) {
        self.hooks.push(hook);
    }

    /// The currently effective interpreter config (a clone).
    pub fn runtimes(&self) -> RuntimesConfig {
        self.runtimes.read().unwrap().clone()
    }

    /// Redo the three-tier resolution.
    /// Call it after installing a runtime, or after the user edits `config.runtimes` by
    /// hand, to make the change take effect immediately.
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

    /// Regenerated every round — a plugin tool the model just injected is visible in the
    /// very next round.
    /// ⚠️ **The request path no longer calls this** — `AgentLoop` asks for
    /// [`Runtime::tool_schemas_scoped`]. What remains here is the **unscoped baseline** (every
    /// registered tool), which the scoping tests compare against to prove that `inline_max = 0` is
    /// byte-identical to the behaviour before the feature existed.
    #[allow(dead_code)]
    pub async fn tool_schemas(&self) -> Vec<serde_json::Value> {
        let tools = self.tools.read().await;
        let mut v: Vec<_> = tools.values().map(|t| t.schema()).collect();
        v.sort_by(|a, b| {
            a["function"]["name"].as_str().unwrap_or("").cmp(b["function"]["name"].as_str().unwrap_or(""))
        });
        v
    }

    /// The same list as [`Runtime::tool_schemas`], minus the tools this conversation is not
    /// advertising right now (see `crate::tool::scope`).
    ///
    /// ⚠️ **Hiding is advertising only.** Name resolution — `execute`, `tool_kind`, `tool_access`,
    /// `caps_for` — always goes through the registry, so a tool that is hidden here still runs if
    /// the model calls it. That is deliberate: hiding must not create a new failure mode.
    ///
    /// The decision is re-made on **every** call from the live registry size, so "a plugin was
    /// installed / removed" needs no bookkeeping: the next round lands on the right side of the
    /// threshold on its own.
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

    /// A snapshot of the registry for the tool-discovery search: name, whether it belongs to a
    /// plugin, and the full schema (which carries the description the search matches against).
    ///
    /// Sorted by name so the same query always renders in the same order — the output goes into the
    /// context, and an arbitrary order would make the prompt cache miss for no reason.
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

    /// Every tool registered by one plugin (by plugin name). Empty when the plugin isn't installed.
    ///
    /// Used right after `create_plugin` / `update_plugin`: a tool the model just built has to be
    /// **visible in the next request** — making it search for its own creation first would be a
    /// pointless round trip, and it is exactly the moment the model is most likely to give up on
    /// the idea of using it.
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

    /// Whether one call of this tool counts as "read" or "writes to disk" — this is what
    /// the workplace boundary uses (see [`crate::tool::Tool::access`] and
    /// `agent::Agent::workplace_reject`).
    ///
    /// Returns **writes to disk** when the tool isn't in the table: callers always look up
    /// the name first, so if this really happens it means the registry and the caller
    /// disagree, and the conservative direction is still "block one time too many".
    pub async fn tool_access(&self, name: &str) -> crate::workplace::Access {
        match self.tools.read().await.get(name) {
            Some(t) => t.access(),
            None => crate::workplace::Access::Write,
        }
    }

    /// The **capabilities requested** by this call (from a plugin's `[contributes] caps`,
    /// since phase 6 · 6b).
    ///
    /// Only plugin tools can have them — meta tools belong to the host and request
    /// capabilities on nobody's behalf.
    /// Returns empty when the plugin isn't there (not installed / uninstalled / failed to
    /// load): **the capability request vanishes together with the component**, leaving no
    /// half-state of "the plugin is gone but the restriction remains".
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
    ///
    /// Lives here rather than in `Agent` because the tool registry is private to `Runtime`, and
    /// because these are the same hooks [`Self::execute`] builds its outer guard from — one reader,
    /// one answer. Either half is `None` when the tool has no opinion (which is every tool except the
    /// shell ones).
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
        // Fallback timeout: plugin tools already have a tool_timeout_secs timeout +
        // restart-and-retry internally; add 30 seconds of headroom here to guard against a
        // permanent block on any path.
        //
        // A tool may ask for a **longer** leash than the flat rule allows (the shell tools do: a
        // build can legitimately take minutes) — see `Tool::timeout_hint`. Without it the outer
        // guard would fire first and report "the tool timed out" while the tool's own, more precise
        // timeout never ran.
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
