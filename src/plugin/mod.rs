//! Plugin host: each plugin is a separate child process, exchanging "one JSON per line" RPC messages over stdio.
//! Protocol: 1. After starting a plugin, the host sends {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}...
//! # Slot model: metadata is permanent, the process is not.

/// The on-disk metadata cache that makes a lazy start possible: `plugins/<name>/plugin.cache.json`.
pub mod cache;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::config::{PluginsConfig, RuntimesConfig};
use crate::i18n::t;
use crate::tf;
use crate::runtime::Runtime;

const INIT_TIMEOUT: Duration = Duration::from_secs(20);
const RUST_BUILD_TIMEOUT: Duration = Duration::from_secs(180);

/// How many plugins may be probed at once during startup (`load_all`).
const PROBE_CONCURRENCY: usize = 4;

/// Suppress the console black window that child processes pop up on Windows.
pub(crate) trait ProcessFlags {
    /// Only requires "no console window pops up". That is exactly what the provisioning call sites need.
    fn hide_console(&mut self) {
        self.process_flags(0);
    }

    /// Set the creation flags: `CREATE_NO_WINDOW` OR'd with `extra`.
    fn process_flags(&mut self, extra: u32);
}

/// Do not create a console window (`winbase.h`).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
impl ProcessFlags for Command {
    fn process_flags(&mut self, extra: u32) {
        self.creation_flags(CREATE_NO_WINDOW | extra);
    }
}

#[cfg(windows)]
impl ProcessFlags for std::process::Command {
    fn process_flags(&mut self, extra: u32) {
        use std::os::windows::process::CommandExt as _;
        self.creation_flags(CREATE_NO_WINDOW | extra);
    }
}

#[cfg(not(windows))]
impl ProcessFlags for Command {
    fn process_flags(&mut self, _extra: u32) {}
}

#[cfg(not(windows))]
impl ProcessFlags for std::process::Command {
    fn process_flags(&mut self, _extra: u32) {}
}

/// What a plugin contributes besides tools (stage 6 · 6b).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Contributes {
    /// Which hook points to attach to.
    pub hooks: Vec<String>,
    /// Prompt-segment ids to contribute. **The body is not written here** — it is fetched with `prompt/segment` at activation,
    /// so the prompt content is not stored in two places and the body forgotten when the manifest is edited.
    pub prompt_segments: Vec<String>,
    /// Whether to contribute a theme patch (fetched with `theme/patch` at activation).
    pub theme: bool,
    /// Capability requests. When non-empty, **this plugin's tool calls all go through `Ask`** (see `auth::Policy::level_for`):
    /// having declared what capabilities it wants, a human must nod once; it may not silently ride on `[auth] plugin_tools`.
    pub caps: Vec<String>,
}

impl Contributes {
    /// Contributes nothing — used to decide "is this plugin down to just tools".
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
            && self.prompt_segments.is_empty()
            && !self.theme
            && self.caps.is_empty()
    }

    /// Why not recognize only one place: the manifest is an offline declaration (visible without starting the plugin...
    pub fn overridden_by(self, live: Option<Self>) -> Self {
        let Some(live) = live else { return self };
        Self {
            hooks: if live.hooks.is_empty() { self.hooks } else { live.hooks },
            prompt_segments: if live.prompt_segments.is_empty() {
                self.prompt_segments
            } else {
                live.prompt_segments
            },
            theme: live.theme || self.theme,
            caps: if live.caps.is_empty() { self.caps } else { live.caps },
        }
    }

    /// Validate and normalize: trim, drop empties, dedupe; hook points are then sorted into crate::hook::POINTS's...
    pub fn normalize(mut self) -> Result<Self> {
        let mut hooks: Vec<String> = Vec::new();
        for p in self.hooks.drain(..) {
            let p = p.trim().to_string();
            if p.is_empty() {
                continue;
            }
            if !crate::hook::POINTS.contains(&p.as_str()) {
                bail!(tf!(
                    "[contributes] hooks has an unknown hook point: {} (the five valid ones: {})",
                    p,
                    crate::hook::POINTS.join(" / ")
                ));
            }
            if !hooks.contains(&p) {
                hooks.push(p);
            }
        }
        hooks.sort_by_key(|p| {
            crate::hook::POINTS.iter().position(|x| x == p).unwrap_or(usize::MAX)
        });
        self.hooks = hooks;

        let mut segs: Vec<String> = Vec::new();
        for s in self.prompt_segments.drain(..) {
            let s = s.trim().to_string();
            if !s.is_empty() && !segs.contains(&s) {
                segs.push(s);
            }
        }
        self.prompt_segments = segs;
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub language: String,
    pub description: String,
    /// For a script plugin this is the entry file name (main.py); for a rust plugin it is the compiled artifact's file name
    pub entry: String,
    /// What it contributes besides tools (stage 6 · 6b).
    #[serde(default, skip_serializing_if = "Contributes::is_empty")]
    pub contributes: Contributes,
}

impl PluginManifest {
    pub fn load(dir: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(dir.join("plugin.toml"))?;
        Ok(toml::from_str(&text)?)
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::write(dir.join("plugin.toml"), toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// The declared intent of this tool: read-only, or possibly writing to disk.
    pub access: crate::workplace::Access,
}

struct RpcChannel {
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    next_id: u64,
}

impl RpcChannel {
    /// Read one line. Tolerates non-UTF-8 bytes (lossy decode); returning None means child EOF.
    async fn next_line(&mut self) -> Result<Option<String>> {
        let mut buf: Vec<u8> = Vec::new();
        let n = self.out.read_until(b'\n', &mut buf).await.context(t("failed to read from the plugin's stdout"))?;
        if n == 0 {
            return Ok(None);
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }
}

/// A live plugin process and everything whose lifetime is tied to it.
struct LiveProc {
    rpc: RpcChannel,
    child: Child,
    /// This process's fence (Job Object, see `src/sandbox.rs`) — one fresh Job per start.
    fence: crate::sandbox::Fence,
    /// When an RPC last ran on this process. The reaper's only clock.
    last_used: Instant,
}

/// A read-only snapshot of a slot's process state, for `/plugins`, `/sandbox` and the exit path.
pub struct SlotStat {
    /// Whether a process is up right now.
    pub running: bool,
    /// Seconds since this process last served an RPC (0 when dormant).
    pub idle_secs: u64,
    /// Whether the fence is really fitted (meaningless while dormant).
    pub fence_active: bool,
    /// The fence's own one-line status text
    pub fence_text: String,
}

impl SlotStat {
    /// The human-facing one-liner: which of the two states the slot is in, and — when a process is up — how long...
    pub fn label(&self) -> String {
        if !self.running {
            return t("dormant (no process)").to_string();
        }
        tf!("active · idle {}s · {}", self.idle_secs, self.fence_text)
    }
}

/// A plugin slot: the permanent metadata, plus an optional process.
pub struct PluginHandle {
    pub name: String,
    pub language: String,
    pub description: String,
    /// The tool table.
    /// Behind a lock rather than a plain field for one reason: at start time the live `initialize` answer is compared...
    pub tool_defs: std::sync::RwLock<Vec<ToolDef>>,
    /// The effective component declaration (`plugin.toml [contributes]` ← the result after `initialize` overrides)
    pub contributes: Contributes,
    /// The theme patch fetched and validated at activation (same shape as `config.toml [theme]`).
    /// This cycle only **stores and displays** it; GUI merging is deferred (see [`Contributes::theme`]).
    pub theme_patch: Option<Box<crate::config::ThemeConfig>>,
    /// The failure ledger for hook RPC — shares the same Arc pair with crate::hook::ComponentHook: the hook side only...
    pub hook_failures: Arc<AtomicU64>,
    pub hook_last_error: Arc<std::sync::Mutex<Option<String>>>,
    /// Config for building the fence (a fresh Job is created on every start, so this is only a template).
    sandbox: crate::sandbox::Sandbox,
    /// Application root: the plugin process's working directory. Relative paths inside the plugin therefore match the host's view.
    root: PathBuf,
    dir: PathBuf,
    manifest: PluginManifest,
    /// The process slot. `None` = dormant: no process, and everything about the plugin still works — its tools...
    proc: Mutex<Option<LiveProc>>,
}

impl PluginHandle {
    /// The tool table as it stands right now.
    pub fn tools(&self) -> Vec<ToolDef> {
        self.tool_defs.read().unwrap().clone()
    }

    /// Whether a process is up right now.
    pub async fn is_running(&self) -> bool {
        self.proc.lock().await.is_some()
    }

    /// A read-only view of the slot, for the human-facing listings
    pub async fn stat(&self) -> SlotStat {
        let g = self.proc.lock().await;
        match g.as_ref() {
            Some(live) => SlotStat {
                running: true,
                idle_secs: live.last_used.elapsed().as_secs(),
                fence_active: live.fence.active,
                fence_text: live.fence.status_text(),
            },
            None => SlotStat { running: false, idle_secs: 0, fence_active: false, fence_text: String::new() },
        }
    }

    /// One RPC, starting the process first if it is dormant.
    pub async fn rpc_ready(self: &Arc<Self>, rt: &Arc<Runtime>, method: &str, params: Value) -> Result<Value> {
        // Make room *before* taking our own lock: this walks the other slots and takes their locks, and doing...
        if !self.is_running().await {
            reap_over_cap(rt, rt.config.plugins.max_resident).await;
        }

        let mut g = self.proc.lock().await;
        if g.is_none() {
            *g = Some(self.start(rt).await?);
        }
        // Holding this lock *is* the "in flight" marker: the reaper only ever closes a process it can
        // `try_lock`, so a long tool call (up to `tool_timeout_secs`) can never be cut off halfway.
        let live = g.as_mut().expect("the slot was filled just above");
        let res = rpc_inner(&self.name, live, method, params).await;
        live.last_used = Instant::now();
        res
    }

    /// Start the process and bring the slot in line with what it actually reports.
    async fn start(self: &Arc<Self>, rt: &Arc<Runtime>) -> Result<LiveProc> {
        let (mut live, init) =
            spawn_and_init(&rt.runtimes(), &self.root, &self.dir, &self.manifest, &self.sandbox).await?;
        self.reconcile(rt, &init).await;
        live.last_used = Instant::now();
        Ok(live)
    }

    /// Correct the slot against the live `initialize` answer.
    async fn reconcile(self: &Arc<Self>, rt: &Arc<Runtime>, init: &InitInfo) {
        let assumed = self.tools();
        if !same_tools(&assumed, &init.tools) {
            // Drop the old names first: a name that survived may have changed shape, and
            // `Runtime::register` refuses a name that is already taken.
            for d in &assumed {
                rt.unregister(&d.name).await;
            }
            for d in &init.tools {
                let tool = Arc::new(PluginTool { handle: self.clone(), def: d.clone() });
                if let Err(e) = rt.register(tool).await {
                    plugin_warn(&self.name, tf!("failed to re-register tool {}: {}", d.name, format!("{e:#}")));
                }
            }
            *self.tool_defs.write().unwrap() = init.tools.clone();
            plugin_warn(
                &self.name,
                tf!("its tool table did not match the cache; refreshed to {} tool(s)", init.tools.len()),
            );
        }
        if init.contributes != self.contributes {
            plugin_warn(&self.name, t("its [contributes] declaration changed; the change takes effect after a restart"));
        }
        // Always rewrite: the cache is keyed by a fingerprint of the plugin's own files, so a
        // fingerprint that moved while the answer did not would otherwise re-probe on every start.
        write_cache(&self.dir, &self.manifest, init);
    }

    /// Force the process down and back up. Used for self-healing after the plugin hangs or crashes.
    async fn respawn(self: &Arc<Self>, rt: &Arc<Runtime>) -> Result<()> {
        self.stop().await;
        let live = self.start(rt).await?;
        *self.proc.lock().await = Some(live);
        Ok(())
    }

    /// Close the process, if there is one.
    pub async fn stop(&self) {
        let live = { self.proc.lock().await.take() };
        if let Some(live) = live {
            self.close(live).await;
        }
    }

    /// Close one process politely, then make sure it is gone.
    async fn close(&self, mut live: LiveProc) {
        // Ask first, so the plugin can leave on its own terms (flush, clean up) — but only for a moment: a plugin...
        if notify(&mut live, "shutdown", Value::Null).await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_millis(300), live.rpc.next_line()).await;
        }
        let _ = live.child.kill().await;
        // Dropping the fence closes the Job; `KILL_ON_JOB_CLOSE` takes the rest of the tree with it.
        drop(live);
    }
}

/// Whether two tool tables describe the same tools.
pub(crate) fn same_tools(a: &[ToolDef], b: &[ToolDef]) -> bool {
    a.len() == b.len()
        && b.iter().all(|d| {
            a.iter().any(|x| {
                x.name == d.name
                    && x.description == d.description
                    && x.parameters == d.parameters
                    && x.access == d.access
            })
        })
}

/// Send a notification: no id, no reply expected (the protocol's `shutdown` is one).
async fn notify(live: &mut LiveProc, method: &str, params: Value) -> Result<()> {
    let req = json!({"jsonrpc": "2.0", "method": method, "params": params});
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    live.rpc.stdin.write_all(line.as_bytes()).await?;
    live.rpc.stdin.flush().await?;
    Ok(())
}

/// One request/response exchange on an already started process.
async fn rpc_inner(name: &str, live: &mut LiveProc, method: &str, params: Value) -> Result<Value> {
    live.rpc.next_id += 1;
    let id = live.rpc.next_id;
    let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    live.rpc.stdin.write_all(line.as_bytes()).await.context(t("failed to write to the plugin's stdin"))?;
    live.rpc.stdin.flush().await?;
    loop {
        let line = live
            .rpc
            .next_line()
            .await?
            .ok_or_else(|| anyhow!(tf!("plugin {} process has exited", name)))?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue; // a non-protocol line printed by the plugin; ignore it
        };
        if v.get("id").and_then(|x| x.as_u64()) != Some(id) {
            continue;
        }
        if let Some(err) = v.get("error") {
            bail!(tf!("plugin {} returned an error: {}", name, err));
        }
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    }
}

// Idle reaping

/// How often the reaper looks at the slots.
fn reap_tick(cfg: &PluginsConfig) -> Duration {
    Duration::from_secs((cfg.idle_timeout_secs / 4).clamp(1, 5))
}

/// Start the background reaper.
fn spawn_reaper(rt: &Arc<Runtime>) {
    let cfg = rt.config.plugins.clone();
    if !cfg.lazy {
        return; // the escape hatch: everything stays resident, exactly like before this feature
    }
    if cfg.idle_timeout_secs == 0 && cfg.max_resident == 0 {
        return; // neither rule can ever fire, so do not run a timer that only sleeps
    }
    let weak = Arc::downgrade(rt);
    let tick = reap_tick(&cfg);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tick).await;
            let Some(rt) = weak.upgrade() else { return };
            // Snapshot the slots under the lock and release it before touching any of them: a slot's `stop` can await...
            let slots: Vec<Arc<PluginHandle>> = rt.plugins.lock().await.values().cloned().collect();
            for h in slots {
                if cfg.eager.iter().any(|n| n == &h.name) {
                    continue; // "always resident" — the user asked for this one to stay
                }
                if cfg.idle_timeout_secs > 0 {
                    reap_slot(&h, Duration::from_secs(cfg.idle_timeout_secs)).await;
                }
            }
            reap_over_cap(&rt, cfg.max_resident).await;
        }
    });
}

/// Close a slot's process if it has been idle for at least `min_idle`. Returns whether it closed one.
async fn reap_slot(h: &Arc<PluginHandle>, min_idle: Duration) -> bool {
    let mut g = match h.proc.try_lock() {
        Ok(g) => g,
        Err(_) => return false, // busy or mid-start — leave it alone
    };
    match g.as_ref() {
        Some(live) if live.last_used.elapsed() >= min_idle => {}
        _ => return false,
    }
    let live = g.take().expect("checked just above");
    h.close(live).await;
    true
}

/// The slot's idle seconds, or `None` when it is dormant or currently busy.
fn idle_secs_if_free(h: &Arc<PluginHandle>) -> Option<u64> {
    let g = h.proc.try_lock().ok()?;
    g.as_ref().map(|l| l.last_used.elapsed().as_secs())
}

/// Keep at most `cap` plugin processes alive, giving up the longest-idle ones first. `0` = no cap.
async fn reap_over_cap(rt: &Arc<Runtime>, cap: usize) {
    if cap == 0 {
        return;
    }
    let eager = &rt.config.plugins.eager;
    let slots: Vec<Arc<PluginHandle>> = rt.plugins.lock().await.values().cloned().collect();
    let mut alive: Vec<(Arc<PluginHandle>, u64)> = slots
        .into_iter()
        .filter(|h| !eager.iter().any(|n| n == &h.name))
        .filter_map(|h| idle_secs_if_free(&h).map(|secs| (h, secs)))
        .collect();
    if alive.len() <= cap {
        return;
    }
    // Oldest first: the slot nobody has touched for longest is the one to give up.
    alive.sort_by(|a, b| b.1.cmp(&a.1));
    let excess = alive.len() - cap;
    for (h, _) in alive.into_iter().take(excess) {
        // `Duration::ZERO`: making room overrides "give it a grace period" — the cap is a hard budget.
        reap_slot(&h, Duration::ZERO).await;
    }
}

/// A tool provided by a plugin (registered with ToolRegistry)
pub struct PluginTool {
    handle: Arc<PluginHandle>,
    def: ToolDef,
}

#[async_trait::async_trait]
impl crate::tool::Tool for PluginTool {
    fn name(&self) -> &str {
        &self.def.name
    }
    fn kind(&self) -> crate::tool::ToolKind {
        crate::tool::ToolKind::Plugin(self.handle.name.clone())
    }
    fn description(&self) -> &str {
        &self.def.description
    }
    fn parameters(&self) -> Value {
        self.def.parameters.clone()
    }
    /// Judged by the **plugin-declared** intent (see [`ToolDef::access`]) — undeclared means write.
    fn access(&self) -> crate::workplace::Access {
        self.def.access
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> crate::tool::ToolOutput {
        use crate::tool::ToolOutput;
        let timeout = Duration::from_secs(rt.config.agent.tool_timeout_secs);
        let params = json!({"name": self.def.name, "arguments": args});
        // Recorded before the call, because it decides what a failure *means* (see below).
        let was_running = self.handle.is_running().await;

        let first = tokio::time::timeout(timeout, self.handle.rpc_ready(rt, "tools/call", params.clone())).await;
        let first_err = match first {
            Ok(Ok(v)) => return parse_tool_result(v),
            Ok(Err(e)) => tf!("plugin {} communication failed: {}", self.handle.name, format!("{e:#}")),
            Err(_) => tf!("plugin {} response timed out ({} s)", self.handle.name, timeout.as_secs()),
        };

        // A failure on a process we had to start is a start failure (no interpreter, a plugin that cannot import itself).
        if !was_running {
            return ToolOutput::err(first_err);
        }

        // Plugin unresponsive/crashed: respawn the process and retry once
        match self.handle.respawn(rt).await {
            Ok(()) => {
                match tokio::time::timeout(timeout, self.handle.rpc_ready(rt, "tools/call", params)).await {
                    Ok(Ok(v)) => parse_tool_result(v),
                    Ok(Err(e)) => ToolOutput::err(tf!("{}; the retry after restarting the plugin failed too: {}", first_err, format!("{e:#}"))),
                    Err(_) => ToolOutput::err(tf!("{}; the retry after restarting the plugin timed out too", first_err)),
                }
            }
            Err(re) => ToolOutput::err(tf!("{}; and restarting the plugin failed: {}", first_err, format!("{re:#}"))),
        }
    }
}

fn parse_tool_result(v: Value) -> crate::tool::ToolOutput {
    let content = v
        .get("content")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| v.to_string());
    let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
    // `"attach": ["<absolute path>", …]` — files produced by the plugin, which the host attaches for the model to see.
    let attach = v
        .get("attach")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.as_str())
                .map(std::path::PathBuf::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    crate::tool::ToolOutput { content, is_error, attach, recycled: 0, diff: None, diff_detail: None, path: None }
}

fn spawn_command(
    cfg: &RuntimesConfig,
    root: &Path,
    dir: &Path,
    manifest: &PluginManifest,
    sandbox: &crate::sandbox::Sandbox,
) -> Result<Command> {
    let stderr_log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("stderr.log"))?;
    // Entry scripts always use absolute paths, so even with the working directory set to the app root the wrong file is never found.
    let entry = dir.join(&manifest.entry);
    let mut cmd = match manifest.language.as_str() {
        "python" => {
            let mut c = Command::new(&cfg.python);
            // X utf8 + PYTHONUTF8/PYTHONIOENCODING: force the child's stdin/stdout through UTF-8.
            c.args(["-X", "utf8", "-u"]).arg(&entry);
            c.env("PYTHONUTF8", "1").env("PYTHONIOENCODING", "utf-8");
            c
        }
        "node" => {
            let mut c = Command::new(&cfg.node);
            c.arg(&entry);
            c.env("NODE_OPTIONS", "--no-warnings");
            c
        }
        "rust" => Command::new(&entry),
        other => bail!(tf!("unknown plugin language: {}", other)),
    };
    // The fence requires "freeze first" (`CREATE_SUSPENDED`); `spawn_and_init` resumes it after it joins the Job.
    // With the sandbox off this item is 0, and behaviour is exactly as before the fence.
    cmd.process_flags(sandbox.create_flags());
    cmd.current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_log))
        .kill_on_drop(true);
    Ok(cmd)
}

/// Start the process (no handshake yet) and put it under its fence.
fn spawn_process(
    cfg: &RuntimesConfig,
    root: &Path,
    dir: &Path,
    manifest: &PluginManifest,
    sandbox: &crate::sandbox::Sandbox,
) -> Result<LiveProc> {
    let mut child = spawn_command(cfg, root, dir, manifest, sandbox)?
        .spawn()
        .with_context(|| tf!("failed to start plugin {} (check whether {} is in PATH)", manifest.name, cfg.python))?;
    let fence = sandbox.contain(&child);
    let stdin = child.stdin.take().ok_or_else(|| anyhow!(t("cannot take the plugin's stdin")))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!(t("cannot take the plugin's stdout")))?;
    Ok(LiveProc {
        rpc: RpcChannel { stdin, out: BufReader::new(stdout), next_id: 0 },
        child,
        fence,
        last_used: Instant::now(),
    })
}

/// What one `initialize` handshake told us, plus the contribution content fetched with it.
struct InitInfo {
    tools: Vec<ToolDef>,
    /// The effective contribution declaration (manifest ← overridden by `initialize`)
    contributes: Contributes,
    /// Prompt-segment bodies, as returned by `prompt/segment`
    prompt_segments: BTreeMap<String, String>,
    /// The raw `theme/patch` result, validated later by [`theme_of`].
    theme_patch: Option<Value>,
}

/// Start the plugin process, complete the initialize handshake, and fetch whatever the declared contributions...
async fn spawn_and_init(
    cfg: &RuntimesConfig,
    root: &Path,
    dir: &Path,
    manifest: &PluginManifest,
    sandbox: &crate::sandbox::Sandbox,
) -> Result<(LiveProc, InitInfo)> {
    let mut live = spawn_process(cfg, root, dir, manifest, sandbox)?;

    let result = tokio::time::timeout(INIT_TIMEOUT, rpc_inner(&manifest.name, &mut live, "initialize", json!({})))
        .await
        .map_err(|_| anyhow!(tf!("plugin {} initialize handshake timed out ({} s)", manifest.name, INIT_TIMEOUT.as_secs())))??;

    // Tools (not a single field of the old shape was changed) ---- A missing `tools` is treated as empty: a component...
    let tools: Vec<Value> = match result.get("tools") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => v
            .as_array()
            .cloned()
            .ok_or_else(|| anyhow!(tf!("plugin {}'s initialize.tools is not an array", manifest.name)))?,
    };
    let mut defs = vec![];
    for t in &tools {
        let name = t
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| anyhow!(tf!("a tool of plugin {} lacks name", manifest.name)))?
            .to_string();
        let description = t.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string();
        let parameters = t.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}));
        let access = parse_access(t);
        defs.push(ToolDef { name, description, parameters, access });
    }

    // ---- Contribution declaration: manifest ← overridden by `initialize` (stage 6 · 6b) ----
    // Not returning `contributes` is the old shape and just uses the manifest as before (for most plugins that one is empty).
    let live_decl = match result.get("contributes") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            serde_json::from_value::<Contributes>(v.clone())
                .with_context(|| tf!("failed to parse plugin {}'s initialize.contributes", manifest.name))?,
        ),
    };
    let contributes = manifest
        .contributes
        .clone()
        .overridden_by(live_decl)
        .normalize()
        .with_context(|| tf!("plugin {}'s contribution declaration is malformed", manifest.name))?;

    // The one thing stage 6 · 6b loosened: previously "declared no tools at all" was a straight failure...
    if defs.is_empty() && contributes.is_empty() {
        bail!(tf!(
            "plugin {} declares neither tools nor any [contributes] — it would do nothing",
            manifest.name
        ));
    }

    // Fetch the contributions that need RPC (prompt segment bodies / theme patch) ---- They are fetched here...
    let mut prompt_segments: BTreeMap<String, String> = BTreeMap::new();
    for id in &contributes.prompt_segments {
        match rpc_inner(&manifest.name, &mut live, "prompt/segment", json!({ "id": id })).await {
            Ok(v) => {
                let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                if text.trim().is_empty() {
                    component_warn(&manifest.name, tf!("prompt segment {}'s body is empty; skipping", id));
                } else {
                    prompt_segments.insert(id.clone(), text);
                }
            }
            Err(e) => component_warn(&manifest.name, tf!("failed to fetch prompt segment {}: {}", id, format!("{e:#}"))),
        }
    }

    let mut theme_patch: Option<Value> = None;
    if contributes.theme {
        match rpc_inner(&manifest.name, &mut live, "theme/patch", Value::Null).await {
            Ok(v) => theme_patch = Some(v),
            Err(e) => component_warn(&manifest.name, tf!("failed to fetch the theme patch: {}", format!("{e:#}"))),
        }
    }

    Ok((live, InitInfo { tools: defs, contributes, prompt_segments, theme_patch }))
}

/// Validate a raw `theme/patch` result into `theme` shape.
fn theme_of(plugin: &str, raw: Option<Value>) -> Option<Box<crate::config::ThemeConfig>> {
    let raw = raw?;
    match serde_json::from_value::<crate::config::ThemeConfig>(raw) {
        Ok(t) => Some(Box::new(t)),
        Err(e) => {
            component_warn(plugin, tf!("the theme patch is not [theme]-shaped; ignored: {}", format!("{e:#}")));
            None
        }
    }
}

/// Extract the "workspace intent" from a tool declaration.
fn parse_access(t: &Value) -> crate::workplace::Access {
    match t.get("access").and_then(|a| a.as_str()).map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case("read") => crate::workplace::Access::Read,
        _ => crate::workplace::Access::Write,
    }
}

/// Component-contributed prompt segments default to **after all segments** (built-in core=10 / protocol=20 /
/// env=30 / rules=40, config segments default 50). A component is "one more layer" and should not jump ahead of the project rules.
const COMPONENT_SEGMENT_ORDER: i32 = 60;

/// A notice for when a component contribution goes wrong.
fn component_warn(plugin: &str, why: impl std::fmt::Display) {
    eprintln!("{}", tf!("[component] {}: {}", plugin, why));
}

/// A notice about a plugin going wrong outside activation.
fn plugin_warn(plugin: &str, why: impl std::fmt::Display) {
    eprintln!("{}", tf!("[plugin] {}: {}", plugin, why));
}

/// The parts a slot is assembled from, once its metadata is known.
struct SlotParts {
    defs: Vec<ToolDef>,
    contributes: Contributes,
    theme_patch: Option<Box<crate::config::ThemeConfig>>,
    /// The process, when one has already been started.
    live: Option<LiveProc>,
}

/// Build a slot. Nothing is started here, and no I/O happens at all — keeping the constructor inert is what makes...
fn assemble(
    dir: &Path,
    root: &Path,
    manifest: &PluginManifest,
    sandbox: &crate::sandbox::Sandbox,
    parts: SlotParts,
) -> Arc<PluginHandle> {
    Arc::new(PluginHandle {
        name: manifest.name.clone(),
        language: manifest.language.clone(),
        description: manifest.description.clone(),
        tool_defs: std::sync::RwLock::new(parts.defs),
        contributes: parts.contributes,
        theme_patch: parts.theme_patch,
        hook_failures: Arc::new(AtomicU64::new(0)),
        hook_last_error: Arc::new(std::sync::Mutex::new(None)),
        sandbox: sandbox.clone(),
        root: root.to_path_buf(),
        dir: dir.to_path_buf(),
        manifest: manifest.clone(),
        proc: Mutex::new(parts.live),
    })
}

/// Register the slot's tools and contributions into the Runtime and bring it under management.
async fn activate_slot(rt: &Arc<Runtime>, handle: Arc<PluginHandle>, segments: Vec<(String, String)>) -> Result<Vec<String>> {
    // Clear this plugin's previous *non-tool* contributions ---- On the create path nothing is there and both...
    rt.hooks.remove_by_plugin(&handle.name);
    rt.remove_prompt_segments_by_plugin(&handle.name);

    // ---- ② Tools: registration failure rolls back the whole thing (same as today) ----
    let mut registered: Vec<String> = vec![];
    for def in &handle.tools() {
        let tool = Arc::new(PluginTool { handle: handle.clone(), def: def.clone() });
        if let Err(e) = rt.register(tool).await {
            for n in &registered {
                rt.unregister(n).await;
            }
            handle.stop().await;
            return Err(e.context(tf!("failed to activate plugin {}", handle.name)));
        }
        registered.push(def.name.clone());
    }

    // ---- ③ Contributions: hooks (purely local registration, no RPC) ----
    // The failure ledger and the handle **share the same Arc pair**, so what `/hook` reports is exactly what is recorded here.
    if !handle.contributes.hooks.is_empty() {
        rt.add_hook(Arc::new(crate::hook::ComponentHook::new(
            handle.name.clone(),
            handle.contributes.hooks.clone(),
            handle.contributes.caps.clone(),
            handle.hook_failures.clone(),
            handle.hook_last_error.clone(),
        )));
    }

    // ---- ④ Contributions: prompt segments (bodies were fetched with the handshake; here they only enter the registry) ----
    for (id, body) in segments {
        rt.add_prompt_segment(crate::prompt::PromptSegment {
            // Segment id is prefixed with the plugin name: segment ids are globally unique, and two components each writing an `extra_rules`
            // must not clobber each other (which is exactly what "last registrant wins" would do).
            id: format!("{}:{id}", handle.name),
            title: tf!("{} ({})", id, handle.name),
            body,
            order: COMPONENT_SEGMENT_ORDER,
            source: crate::prompt::Source::Plugin,
        });
    }

    rt.plugins.lock().await.insert(handle.name.clone(), handle);
    Ok(registered)
}

/// Implementation of the create_plugin meta-tool: write source → (compile if rust) → startup handshake → register tools.
/// On success returns the list of registered tool names.
pub async fn create_plugin(
    rt: &Arc<Runtime>,
    name: &str,
    language: &str,
    description: &str,
    source: &str,
) -> Result<Vec<String>> {
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        || name.is_empty()
        || name.len() > 40
    {
        bail!(tf!("plugin names may contain only lowercase letters/digits/underscores, length 1-40: {}", format!("{name:?}")));
    }
    let entry = match language {
        "python" => "main.py",
        "node" => "main.js",
        "rust" => "main.rs",
        other => bail!(tf!("language must be python / node / rust, got: {}", other)),
    };
    let dir = rt.plugins_dir.join(name);
    // A name that is already live is no longer a hard error: rebuilding in place is exactly what a `update_plugin`...
    if rt.plugins.lock().await.contains_key(name) {
        return update_plugin(rt, name, source, None).await;
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| tf!("failed to clean up the leftover plugin directory: {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(entry), source)?;

    let result = create_plugin_inner(rt, name, language, description, entry, &dir).await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir); // leave no debris behind on failure
    }
    result
}

/// Implementation of the `update_plugin` meta-tool: replace an existing plugin's source in place, retiring the old...
pub async fn update_plugin(
    rt: &Arc<Runtime>,
    name: &str,
    source: &str,
    description: Option<&str>,
) -> Result<Vec<String>> {
    // Snapshot the old handle up front (its manifest/entry/language are what we keep), then release
    // the table lock — the whole body below does I/O and RPC and must not hold it.
    let old = { rt.plugins.lock().await.get(name).cloned() };
    let Some(old) = old else {
        bail!(tf!(
            "no such plugin: {} — use create_plugin to make a new one (update_plugin only changes plugins that already exist)",
            name
        ));
    };

    let language = old.language.clone();
    let entry = old.manifest.entry.clone();
    let description = description.unwrap_or(&old.description).to_string();
    let dir = old.dir.clone();

    // The manifest keeps its language/entry but takes the new description; `[contributes]` starts
    // from the on-disk value and is overridden by whatever the new `initialize` returns.
    let manifest = PluginManifest {
        name: name.to_string(),
        language: language.clone(),
        description,
        entry: entry.clone(),
        contributes: old.manifest.contributes.clone(),
    };

    // ---- Write the new source, remembering the old one so a failure can be rolled back ----
    let entry_path = dir.join(&entry);
    let previous = std::fs::read_to_string(&entry_path).ok();
    std::fs::write(&entry_path, source).with_context(|| tf!("failed to write plugin source: {}", entry_path.display()))?;

    // rust: recompile before anything is dismantled — a compile failure must not touch the running
    // plugin at all, so it is treated like the write above: fail here, roll back the source, done.
    if language == "rust" {
        if let Err(e) = compile_rust_plugin(rt, name, &dir).await {
            restore(&entry_path, previous.as_deref(), rt, name, &language, &dir).await;
            return Err(e);
        }
    }

    // Handshake the new version before dismantling the old one.
    let handshake = spawn_and_init(&rt.runtimes(), &rt.root, &dir, &manifest, &rt.sandbox).await;
    let (live, init) = match handshake {
        Ok(v) => v,
        Err(e) => {
            restore(&entry_path, previous.as_deref(), rt, name, &language, &dir).await;
            return Err(e);
        }
    };

    // Dismantle the old slot (the new version has handshaken, so this is the safe moment) ---- Unregister the old...
    for def in &old.tools() {
        rt.unregister(&def.name).await;
    }
    // Close the old process. Otherwise `rpc_ready` on the stale handle would keep a second interpreter alive behind...
    old.stop().await;
    rt.plugins.lock().await.remove(name);

    // Install the new slot
    let theme_patch = theme_of(&manifest.name, init.theme_patch.clone());
    write_cache(&dir, &manifest, &init);
    let segments: Vec<(String, String)> = init.prompt_segments.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let handle = assemble(
        &dir,
        &rt.root,
        &manifest,
        &rt.sandbox,
        SlotParts { defs: init.tools, contributes: init.contributes, theme_patch, live: Some(live) },
    );
    match activate_slot(rt, handle, segments).await {
        Ok(tools) => Ok(tools),
        Err(e) => {
            // Registration itself failed. The old process is already gone, so the caller's rollback
            // restores the source; make sure no half-registered new slot is left behind either.
            rt.plugins.lock().await.remove(name);
            restore(&entry_path, previous.as_deref(), rt, name, &language, &dir).await;
            Err(e)
        }
    }
}

/// Put the previous source back after a failed update, and recompile it for a rust plugin.
async fn restore(entry_path: &Path, previous: Option<&str>, rt: &Arc<Runtime>, name: &str, language: &str, dir: &Path) {
    match previous {
        Some(text) => {
            if std::fs::write(entry_path, text).is_err() {
                plugin_warn(name, t("failed to roll back the plugin source after a failed update"));
            } else if language == "rust" {
                let _ = compile_rust_plugin(rt, name, dir).await;
            }
        }
        // The entry file did not exist before (a half-built plugin). Removing what we wrote returns
        // the directory to the state `create_plugin`'s leftover-cleanup expects.
        None => {
            let _ = std::fs::remove_file(entry_path);
        }
    }
}

async fn create_plugin_inner(
    rt: &Arc<Runtime>,
    name: &str,
    language: &str,
    description: &str,
    entry: &str,
    dir: &Path,
) -> Result<Vec<String>> {
    let mut entry = entry.to_string();

    if language == "rust" {
        compile_rust_plugin(rt, name, dir).await?;
        entry = rust_entry(name);
    }

    let manifest = PluginManifest {
        name: name.to_string(),
        language: language.to_string(),
        description: description.to_string(),
        entry,
        // A plugin created by `create_plugin` provides tools only by default; to declare contributions the model later edits
        // `plugin.toml` (or has `initialize` return `contributes` directly) — both routes are recognized.
        contributes: Contributes::default(),
    };
    manifest.save(dir)?;

    // A just-created plugin is started now, not deferred: the model almost always calls it on the very next step...
    let (live, init) = spawn_and_init(&rt.runtimes(), &rt.root, dir, &manifest, &rt.sandbox).await?;
    let theme_patch = theme_of(&manifest.name, init.theme_patch.clone());
    write_cache(dir, &manifest, &init);
    let segments: Vec<(String, String)> = init.prompt_segments.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let handle = assemble(
        dir,
        &rt.root,
        &manifest,
        &rt.sandbox,
        SlotParts { defs: init.tools, contributes: init.contributes, theme_patch, live: Some(live) },
    );
    activate_slot(rt, handle, segments).await
}

/// The compiled artifact's name for a rust plugin (`main.rs` → `<name>[.exe]`)
fn rust_entry(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Compile a rust plugin's `main.rs` in place, writing `rust_entry(name)` next to it.
async fn compile_rust_plugin(rt: &Arc<Runtime>, name: &str, dir: &Path) -> Result<()> {
    let exe = rust_entry(name);
    // Use the resolved rustc (possibly the one bundled in the app directory), not the raw value from config
    let runtimes = rt.runtimes();
    let mut cmd = Command::new(&runtimes.rustc);
    cmd.arg("--edition=2021").arg("-O");
    // Statically link the CRT, consistent with the host's own build (see.cargo/config.toml).
    if cfg!(windows) {
        cmd.args(["-C", "target-feature=+crt-static"]);
    }
    cmd.arg("main.rs").arg("-o").arg(&exe).current_dir(dir);
    cmd.hide_console();
    let build = tokio::time::timeout(RUST_BUILD_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow!(t("rustc compile timed out")))?
        .context(t("cannot start rustc (rust plugins need the Rust toolchain in PATH)"))?;
    if !build.status.success() {
        bail!(tf!("rustc compile failed:\n{}", String::from_utf8_lossy(&build.stderr)));
    }
    Ok(())
}
/// A failure is a warning, never an error: the cache is an optimisation, and the plugin it describes works exactly...
fn write_cache(dir: &Path, manifest: &PluginManifest, init: &InitInfo) {
    let captured = cache::Cached::capture(
        dir,
        &manifest.entry,
        &init.tools,
        init.contributes.clone(),
        init.prompt_segments.clone(),
        init.theme_patch.clone(),
    );
    if let Err(e) = cache::save(dir, &captured) {
        plugin_warn(&manifest.name, tf!("failed to write the metadata cache: {}", format!("{e:#}")));
    }
}

/// Implementation of the remove_plugin meta-tool.
/// Whether or not the plugin is loaded, try to clean up its directory, guaranteeing "deleted means deleted".
pub async fn remove_plugin(rt: &Arc<Runtime>, name: &str) -> Result<Vec<String>> {
    let handle = {
        let mut plugins = rt.plugins.lock().await;
        plugins.remove(name)
    };
    let mut removed = vec![];
    if let Some(handle) = handle {
        for def in &handle.tools() {
            rt.unregister(&def.name).await;
            removed.push(def.name.clone());
        }
        // `stop` covers both states, so there is no "was it running?" branch to get wrong — and the
        // directory removal below takes the metadata cache with it, so nothing survives the delete.
        handle.stop().await;
        drop(handle);
    }
    let dir = rt.plugins_dir.join(name);
    let dir_existed = dir.exists();
    if dir_existed {
        std::fs::remove_dir_all(&dir).with_context(|| tf!("failed to delete the plugin directory: {}", dir.display()))?;
    }
    if removed.is_empty() && !dir_existed {
        bail!(tf!("no such plugin: {}", name));
    }
    Ok(removed)
}

/// On startup, scan the plugins/ directory and bring every persisted plugin under management.
pub async fn load_all(rt: &Arc<Runtime>) -> Vec<(String, Result<Vec<String>>)> {
    let Ok(entries) = std::fs::read_dir(&rt.plugins_dir) else {
        return vec![];
    };

    // ── Phase 1: scan. Cheap and synchronous, so it stays sequential and keeps the on-disk order. A directory whose...
    enum Job {
        Broken(anyhow::Error),
        Ready(PathBuf, PluginManifest),
    }
    let mut jobs: Vec<(String, Job)> = vec![];
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || dir.file_name().map(|n| n == "sdk").unwrap_or(false) {
            continue;
        }
        match PluginManifest::load(&dir) {
            Ok(m) => {
                let name = m.name.clone();
                jobs.push((name, Job::Ready(dir, m)));
            }
            Err(e) => {
                let name = dir.file_name().unwrap_or_default().to_string_lossy().to_string();
                jobs.push((
                    name,
                    Job::Broken(anyhow!(tf!("failed to read the manifest: {}", format!("{e:#}")))),
                ));
            }
        }
    }

    // ── Phase 2: probe + register, a few at a time.
    enum Slot {
        Immediate(Result<Vec<String>>),
        Task(tokio::task::JoinHandle<Result<Vec<String>>>),
    }
    let gate = Arc::new(tokio::sync::Semaphore::new(PROBE_CONCURRENCY));
    let mut slots: Vec<(String, Slot)> = Vec::with_capacity(jobs.len());
    for (name, job) in jobs {
        match job {
            Job::Broken(e) => slots.push((name, Slot::Immediate(Err(e)))),
            Job::Ready(dir, manifest) => {
                let rt = rt.clone();
                let gate = gate.clone();
                slots.push((
                    name,
                    Slot::Task(tokio::spawn(async move {
                        // Held for the whole probe; dropped when this task ends.
                        let _permit = gate.acquire_owned().await;
                        load_one(&rt, &dir, &manifest).await
                    })),
                ));
            }
        }
    }

    let mut out: Vec<(String, Result<Vec<String>>)> = Vec::with_capacity(slots.len());
    for (name, slot) in slots {
        let result = match slot {
            Slot::Immediate(r) => r,
            // A join error means the task panicked. `load_one` is written to return errors rather than panic, but if one ever...
            Slot::Task(h) => h.await.unwrap_or_else(|e| Err(anyhow!(tf!("loading panicked: {}", e)))),
        };
        out.push((name, result));
    }

    // Start the idle reaper once per Runtime. It is a no-op when `[plugins] lazy = false`, which is
    // what keeps the escape hatch exactly equivalent to the old model.
    spawn_reaper(rt);
    out
}

/// Bring one plugin under management, without (usually) starting anything.
async fn load_one(rt: &Arc<Runtime>, dir: &Path, manifest: &PluginManifest) -> Result<Vec<String>> {
    if let Some(cached) = cache::load(dir, &manifest.entry) {
        let contributes = cached
            .contributes
            .clone()
            .normalize()
            .with_context(|| tf!("plugin {}'s contribution declaration is malformed", manifest.name))?;
        let theme_patch = theme_of(&manifest.name, cached.theme_patch.clone());
        let segments: Vec<(String, String)> = cached.prompt_segments.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let handle = assemble(
            dir,
            &rt.root,
            manifest,
            &rt.sandbox,
            SlotParts { defs: cached.tool_defs(), contributes, theme_patch, live: None },
        );
        if handle.tools().is_empty() && handle.contributes.is_empty() {
            bail!(tf!(
                "plugin {} declares neither tools nor any [contributes] — it would do nothing",
                manifest.name
            ));
        }
        return activate_slot(rt, handle, segments).await;
    }

    let (live, init) = spawn_and_init(&rt.runtimes(), &rt.root, dir, manifest, &rt.sandbox).await?;
    let theme_patch = theme_of(&manifest.name, init.theme_patch.clone());
    write_cache(dir, manifest, &init);
    let segments: Vec<(String, String)> = init.prompt_segments.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let handle = assemble(
        dir,
        &rt.root,
        manifest,
        &rt.sandbox,
        SlotParts { defs: init.tools, contributes: init.contributes, theme_patch, live: Some(live) },
    );
    // The process existed only to report its own tool list, and the cache now holds that answer (both paths reach...
    handle.stop().await;
    activate_slot(rt, handle, segments).await
}

/// Ensure plugins/sdk/ exists (Python SDK + protocol doc) for plugins the model writes to reference.
/// These two files are wholly owned by the host and overwritten on every startup, guaranteeing they match the host version.
pub fn ensure_sdk(plugins_dir: &Path) -> Result<()> {
    let sdk = plugins_dir.join("sdk");
    std::fs::create_dir_all(&sdk)?;
    std::fs::write(sdk.join("plugin_sdk.py"), t(PYTHON_SDK))?;
    std::fs::write(sdk.join("PROTOCOL.md"), t(PROTOCOL_DOC))?;
    Ok(())
}

pub const PROTOCOL_DOC: &str = r#"# Plugin protocol (host <-> plugin subprocess)

Every message is "one line of JSON" on stdout/stdin. stdout carries protocol messages only; logs always go to stderr.

## 1. Handshake

The host sends:
   {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
The plugin replies with one line:
   {"jsonrpc":"2.0","id":1,"result":{
      "name":"<plugin name>",
      "tools":[{"name":"<tool name>","description":"<description>","parameters":<JSON Schema>,"access":"<read|write>"}],
      "contributes":{ ... }          // optional, see §4
   }}

`tools` may be omitted (a component that only contributes, without offering tools, need not write it).
Omitting `contributes` means the old shape — a tools-only plugin changes not one field.

`access` may also be omitted; omitted = `"write"`:

  - `"read"`  — this tool is **read-only** (scanning / stats / listing / parsing files).
                The host's workplace boundary judges it by "read", and **reads are unrestricted**: it can
                reach paths outside the workspace, the same tier as the host's own read_file / list_dir,
                no user elevation needed.
  - `"write"` — may touch the disk (the default). Path-bearing arguments must stay inside this
                conversation's workspace, otherwise the whole call is blocked (the host can only scan
                argument values for strings that "look like paths" and cannot tell whether you read or
                write with them, so anything undeclared is handled at the most conservative tier).

Only tools that **really write no files and delete nothing** should declare `"read"`. A false
declaration is not caught by the host (it cannot see inside the plugin process), but it amounts to
tearing down that tool's workplace boundary yourself.

## 2. Tool call

Host: {"jsonrpc":"2.0","id":N,"method":"tools/call","params":{"name":"<tool name>","arguments":{...}}}
Plugin: {"jsonrpc":"2.0","id":N,"result":{"content":"<string result>","is_error":false}}

## 3. Shutdown

The host sends {"jsonrpc":"2.0","method":"shutdown"} (no id); the plugin should exit.

## 4. Component contributions (optional, stage 6 · 6b)

Declared in plugin.toml:

    [contributes]
    hooks = ["before_tool", "on_error"]   # only the five names below are accepted; a typo fails activation
    prompt_segments = ["extra_rules"]     # segment ids; fetch the body with prompt/segment
    theme = true                          # whether to contribute a theme patch
    caps = ["fs_write"]                   # capability request: this plugin's tool calls always go through "ask"

Alternatively return them from initialize's `contributes` above — **when both exist, initialize wins**
(the manifest is an offline declaration; the process's live answer is more accurate).

**Hook implementations must not error**: the host treats timeouts (10 s), communication failures and
unrecognised return values all as "pass", and books the failure (`/hook` shows it). So hooks just
return what they mean to say — no need to backstop anything — but **don't** do slow work in them:
they block the main loop.

### 4.1 Hooks

Host: {"jsonrpc":"2.0","id":N,"method":"hooks/<point>","params":{...}}
Plugin: {"jsonrpc":"2.0","id":N,"result":{...}}

point            params                              result
before_request   {"tools":[...]}                     {"verdict":"pass"|"reject","reason":"…","inject":["extra text"]}
after_reply      {"text":…,"tool_calls":<count>}      {"verdict":…,"text":"rewritten text"}
before_tool      {"tool":"name","args":{...}}         {"verdict":…,"args":{changed args}}
after_tool       {"tool":…,"content":…,"is_error":…}  {"verdict":…,"content":…,"is_error":…}
on_error         {"error":"…"}                        {"action":"propagate"|"retry","note":"hint for the model"}

`inject` accepts **user text only** (an array of strings) — this layer opens up "appending" only,
not "rewriting history" (that is `[loop] max_rounds`'s business).
`after_reply` can only change the text, not `tool_calls` (changing those would forge the model's intent).

### 4.2 Prompt segments

Host: {"jsonrpc":"2.0","id":N,"method":"prompt/segment","params":{"id":"extra_rules"}}
Plugin: {"jsonrpc":"2.0","id":N,"result":{"text":"the segment's body"}}

### 4.3 Theme patch

Host: {"jsonrpc":"2.0","id":N,"method":"theme/patch","params":null}
Plugin: {"jsonrpc":"2.0","id":N,"result":{ ...same shape as config.toml's [theme]... }}

For now the host only **fetches and validates** theme patches (GUI merging is not wired yet — see
the comments in `plugin/mod.rs`).

Python plugins should use the plugin_sdk.py in the same directory directly."#;

pub const PYTHON_SDK: &str = r#"""""
AOX - Python plugin SDK

A minimal plugin (tools only):
    import sys, os
    sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
    from plugin_sdk import serve

    def my_tool(args):
        return "result string"

    serve("plugin name", {
        "my_tool": ("what the tool does", {"type": "object", "properties": {}}, my_tool),
    })

Components (hooks / prompt segments / theme on top of tools):
    def before_tool(params):
        # params = {"tool": "tool name", "args": {...}}
        return {"verdict": "pass", "args": params["args"]}

    serve(
        "plugin name",
        {},
        contributes={"hooks": ["before_tool"], "prompt_segments": ["note"], "caps": ["fs_write"]},
        hooks={"before_tool": before_tool},
        prompt_segments={"note": "Think about what to write before writing any file."},
    )

Exceptions thrown inside a tool or a hook never take the host down: a tool exception becomes an
is_error result; a hook exception goes to stderr and returns as "pass".
"""
import sys
import json


def _resolve(value):
    # accepts either a plain value or a zero-arg callable (the latter is computed each time)
    return value() if callable(value) else value


def _send(msg):
    sys.stdout.write(json.dumps(msg, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def _tool_entry(name, spec):
    """One tool declaration -> the protocol's JSON.

    spec is (description, JSON Schema, handler), with an **optional 4th slot** for access:

      "read"  -> this tool is **read-only**. The host's workplace boundary judges by "read" —
                 reads are unrestricted, so scanning / stats / listing can reach paths outside
                 the workspace (the same tier as the host's own read_file / list_dir; no user
                 elevation needed).
      omitted / other -> may touch the disk: path-bearing arguments must stay inside this
                 conversation's workspace.

    Only tools that **really write no files and delete nothing** should write "read". A false
    declaration is not caught by the host (it cannot see inside the plugin process), but it
    amounts to tearing down that tool's workplace boundary yourself.
    """
    return {
        "name": name,
        "description": spec[0],
        "parameters": spec[1],
        "access": spec[3] if len(spec) > 3 else "write",
    }


def serve(plugin_name, tools, contributes=None, hooks=None, prompt_segments=None, theme=None):
    """Serve. Every parameter is optional; write only the ones you use.

    tools:           {tool name: (description, JSON Schema, handler[, "read"])},
                     handler(dict) -> result string.
                     A 4th slot "read" = the tool is read-only (see _tool_entry)
    contributes:     contribution dict (hooks / prompt_segments / theme / caps),
                     equivalent to [contributes] in plugin.toml (when both exist, this wins)
    hooks:           {point name: fn(params) -> dict}; point names are the five in PROTOCOL.md
    prompt_segments: {segment id: body}; the body may also be fn() -> body
    theme:           theme patch dict; may also be fn() -> dict
    """
    tools = tools or {}
    hooks = hooks or {}
    prompt_segments = prompt_segments or {}
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except Exception:
            continue
        mid = req.get("id")
        method = req.get("method")
        params = req.get("params") or {}
        if method == "initialize":
            result = {
                "name": plugin_name,
                "tools": [_tool_entry(n, spec) for n, spec in tools.items()],
            }
            if contributes:
                result["contributes"] = contributes
            _send({"jsonrpc": "2.0", "id": mid, "result": result})
        elif method == "tools/call":
            tname = params.get("name")
            args = params.get("arguments") or {}
            try:
                if tname not in tools:
                    raise KeyError(f"unknown tool: {tname}")
                handler = tools[tname][2]
                _send({"jsonrpc": "2.0", "id": mid,
                       "result": {"content": str(handler(args)), "is_error": False}})
            except Exception as e:
                _send({"jsonrpc": "2.0", "id": mid,
                       "result": {"content": f"{type(e).__name__}: {e}", "is_error": True}})
        elif method.startswith("hooks/"):
            point = method[len("hooks/"):]
            handler = hooks.get(point)
            if handler is None:
                # this point is not declared: "pass" straight away, the host does not even book a failure
                _send({"jsonrpc": "2.0", "id": mid, "result": {}})
            else:
                try:
                    _send({"jsonrpc": "2.0", "id": mid, "result": handler(params) or {}})
                except Exception as e:
                    # a hook exception never takes the host down: trace to stderr, return as "pass"
                    sys.stderr.write(f"[hook {point}] {type(e).__name__}: {e}\n")
                    sys.stderr.flush()
                    _send({"jsonrpc": "2.0", "id": mid, "result": {}})
        elif method == "prompt/segment":
            sid = params.get("id")
            if sid in prompt_segments:
                _send({"jsonrpc": "2.0", "id": mid,
                       "result": {"text": str(_resolve(prompt_segments[sid]))}})
            else:
                _send({"jsonrpc": "2.0", "id": mid, "result": {"text": ""}})
        elif method == "theme/patch":
            _send({"jsonrpc": "2.0", "id": mid, "result": _resolve(theme) or {}})
        elif method == "shutdown":
            break"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// A hot swap must leave no ghost of the previous version behind.
    #[test]
    fn swapping_a_plugin_drops_only_its_own_contributions() {
        use crate::hook::{Hook, HookChain};
        use crate::prompt::{PromptSegment, Source};

        // A minimal hook that only reports a name — enough for `remove_by_plugin`, which matches by it.
        struct Named(String);
        #[async_trait::async_trait]
        impl Hook for Named {
            fn name(&self) -> &str {
                &self.0
            }
            fn describe(&self) -> String {
                String::new()
            }
        }

        let chain = HookChain::new(vec![
            Arc::new(Named("alpha".into())) as Arc<dyn Hook>,
            Arc::new(Named("beta".into())) as Arc<dyn Hook>,
        ]);
        chain.remove_by_plugin("alpha");
        assert_eq!(chain.len(), 1, "only the swapped plugin's hook is dropped");

        let rt = rt_for_contrib_tests();
        let seg = |id: &str| PromptSegment {
            id: id.into(),
            title: String::new(),
            body: "b".into(),
            order: 60,
            source: Source::Plugin,
        };
        rt.add_prompt_segment(seg("alpha:note"));
        rt.add_prompt_segment(seg("beta:note"));
        rt.remove_prompt_segments_by_plugin("alpha");
        let ids: Vec<String> = rt.prompt_segments().into_iter().map(|s| s.id).collect();
        assert!(ids.contains(&"beta:note".to_string()), "another plugin's segment is untouched");
        assert!(!ids.contains(&"alpha:note".to_string()), "the swapped plugin's segment is gone");

        // No-op when nothing matches — this is what every `create_plugin` runs into.
        chain.remove_by_plugin("gamma");
        rt.remove_prompt_segments_by_plugin("gamma");
        assert_eq!(chain.len(), 1);
    }

    /// Build a throwaway Runtime for the contribution-cleanup test (no plugins, no I/O).
    fn rt_for_contrib_tests() -> Arc<Runtime> {
        let root = std::env::temp_dir().join("aox-plugin-contrib-test");
        let _ = std::fs::create_dir_all(&root);
        // The only required section is `[provider]`; everything else defaults.
        let cfg: crate::config::Config = toml::from_str(
            "[provider]\nbase_url = \"https://example.com/v1\"\nmodel = \"m\"\napi_key = \"k\"\n",
        )
        .expect("the minimal test config must parse");
        Runtime::new(cfg, root.clone(), root.join("plugins")).expect("the test runtime must build")
    }

    /// An old manifest (no `[contributes]`) must still parse as-is — backward compatibility is a hard requirement of this stage.
    #[test]
    fn old_manifest_without_contributes_still_parses() {
        let m: PluginManifest = toml::from_str(
            "name = \"fs\"\nlanguage = \"python\"\ndescription = \"file read/write\"\nentry = \"main.py\"\n",
        )
        .unwrap();
        assert!(m.contributes.is_empty(), "an old manifest must not grow contributions out of nowhere");
    }

    /// A new manifest can parse the four kinds of declaration.
    #[test]
    fn manifest_parses_the_contributes_section() {
        let m: PluginManifest = toml::from_str(
            "name = \"comp\"\nlanguage = \"python\"\ndescription = \"component\"\nentry = \"main.py\"\n\n\
             [contributes]\n\
             hooks = [\"before_tool\", \"on_error\"]\n\
             prompt_segments = [\"note\"]\n\
             theme = true\n\
             caps = [\"fs_write\"]\n",
        )
        .unwrap();
        assert_eq!(m.contributes.hooks, vec!["before_tool", "on_error"]);
        assert_eq!(m.contributes.prompt_segments, vec!["note"]);
        assert!(m.contributes.theme);
        assert_eq!(m.contributes.caps, vec!["fs_write"]);
        assert!(!m.contributes.is_empty());
    }

    /// An empty contribution section must not be **written out** — an old plugin re-persisted still matches the old format.
    #[test]
    fn save_omits_an_empty_contributes_section() {
        let dir = std::env::temp_dir().join("aox-manifest-save-test");
        std::fs::create_dir_all(&dir).unwrap();
        let m = PluginManifest {
            name: "fs".into(),
            language: "python".into(),
            description: "d".into(),
            entry: "main.py".into(),
            contributes: Contributes::default(),
        };
        m.save(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join("plugin.toml")).unwrap();
        assert!(!text.contains("contributes"), "an empty contributes section must not be written: {text}");
        let _ = std::fs::remove_file(dir.join("plugin.toml"));
    }

    /// An unrecognized hook point name = **activation failure**, and the error must give both "which one" and "which are available".
    #[test]
    fn normalize_rejects_an_unknown_hook_point() {
        let c = Contributes {
            hooks: vec!["before_tool".into(), "before_evrything".into()],
            ..Default::default()
        };
        let e = c.normalize().unwrap_err().to_string();
        assert!(e.contains("before_evrything"), "the error must name the offender: {e}");
        assert!(e.contains("before_request"), "the error must list the five valid ones: {e}");
    }

    /// Normalization: trim, drop empties, dedupe; hook point order is fixed by `POINTS`, independent of the order written.
    #[test]
    fn normalize_dedups_and_fixes_the_order() {
        let c = Contributes {
            hooks: vec![" on_error ".into(), "before_tool".into(), "on_error".into(), "  ".into()],
            prompt_segments: vec![" note ".into(), "note".into(), "".into()],
            ..Default::default()
        };
        let c = c.normalize().unwrap();
        assert_eq!(c.hooks, vec!["before_tool", "on_error"], "in POINTS' fixed order");
        assert_eq!(c.prompt_segments, vec!["note"]);
    }

    /// `initialize` overrides the manifest (**last writer wins**), but a field "present in the manifest and unmentioned by initialize" stays with the manifest.
    #[test]
    fn initialize_contributes_wins_over_the_manifest() {
        let manifest = Contributes {
            hooks: vec!["before_tool".into()],
            caps: vec!["fs_read".into()],
            ..Default::default()
        };

        assert_eq!(manifest.clone().overridden_by(None).hooks, vec!["before_tool"]);

        let live = Contributes { hooks: vec!["on_error".into()], theme: true, ..Default::default() };
        let eff = manifest.overridden_by(Some(live));
        assert_eq!(eff.hooks, vec!["on_error"], "the process's live answer overrides the manifest");
        assert!(eff.theme, "the manifest does not mention theme; take initialize's");
        assert_eq!(eff.caps, vec!["fs_read"], "initialize does not mention caps; keep the manifest's");
    }

    /// How the declared read intent is parsed.
    #[test]
    fn access_declaration_is_read_only_when_it_exactly_says_so() {
        use crate::workplace::Access;
        let read = |v: serde_json::Value| parse_access(&json!({"access": v}));

        assert_eq!(read(json!("read")), Access::Read);
        // Case and surrounding-whitespace tolerant: this is "the same word written crookedly", and should not be punished
        assert_eq!(read(json!(" read ")), Access::Read);
        assert_eq!(read(json!("READ")), Access::Read);

        // Everything else falls to write — each could be a plugin author's slip, and one extra block does no harm
        assert_eq!(read(json!("write")), Access::Write);
        assert_eq!(read(json!("readonly")), Access::Write, "only the word read is recognised");
        assert_eq!(read(json!("read_only")), Access::Write);
        assert_eq!(read(json!(1)), Access::Write, "a wrong type is treated conservatively too");
        assert_eq!(read(json!(null)), Access::Write);
        assert_eq!(parse_access(&json!({})), Access::Write, "missing field = old plugin, unchanged behavior");
    }
}
