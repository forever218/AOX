//! Plugin host: each plugin is a separate child process, exchanging "one JSON per line" RPC messages over stdio.
//!
//! Protocol:
//!   1. After starting a plugin, the host sends {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
//!      The plugin replies with result: {"name":..., "tools":[{"name","description","parameters","access"}]}
//!      `access` is optional: writing "read" means this tool is **read-only** (the workspace boundary is judged as a read — reads are unrestricted),
//!      everything else is judged as "may write to disk". See [`ToolDef::access`].
//!   2. The host sends {"jsonrpc":"2.0","id":N,"method":"tools/call","params":{"name","arguments"}}
//!      The plugin replies with result: {"content":"...","is_error":false}
//!   3. The host may send the notification {"jsonrpc":"2.0","method":"shutdown"}; the plugin should exit on its own.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::config::RuntimesConfig;
use crate::i18n::t;
use crate::tf;
use crate::runtime::Runtime;

const INIT_TIMEOUT: Duration = Duration::from_secs(20);
const RUST_BUILD_TIMEOUT: Duration = Duration::from_secs(180);

/// Suppress the console black window that child processes pop up on Windows.
///
/// **Creation flags** for child processes.
///
/// Two things share this one spot:
///
/// ① **`CREATE_NO_WINDOW`** — a plugin process's stdin/stdout are pipes and stderr goes to
///    `stderr.log`, so it **never needs a console**. But in the GUI build the host is compiled as the `windows` subsystem
///    (with no console of its own); launching console programs like `python.exe` / `node.exe` / `rustc.exe` at that point
///    makes Windows allocate a fresh console just for it — what the user sees is
///    "black windows popping up out of nowhere". Adding this flag everywhere makes CLI and GUI behave the same.
/// ② **The fence's `CREATE_SUSPENDED`** (`src/sandbox.rs`) — freeze the process first so the host
///    can stuff it into a Job Object before it runs its first line of code.
///
/// There are **two** kinds of `Command`: plugin child processes use `tokio::process::Command` (async),
/// while the runtime-provisioning chain (certutil / tar / powershell) uses `std::process::Command` (sync).
/// Both implement `creation_flags`, so a small trait unifies them here,
/// avoiding a copy of the platform details at every call site.
pub(crate) trait ProcessFlags {
    /// Only requires "no console window pops up". That is exactly what the provisioning call sites need.
    fn hide_console(&mut self) {
        self.process_flags(0);
    }

    /// Set the creation flags: `CREATE_NO_WINDOW` **OR'd with** `extra`.
    ///
    /// ⚠️ The underlying call **overwrites** rather than appends, so everything must be given at once — that is also why `extra`
    /// comes in as a parameter instead of letting the caller call `creation_flags` again (which would wipe
    /// out `CREATE_NO_WINDOW` and bring the black window back).
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
///
/// All fields are optional — an old plugin's `plugin.toml` has no `[contributes]` at all,
/// so it deserializes to "contributes nothing", byte-identical in behaviour to today.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Contributes {
    /// Which hook points to attach to. **Only the five names in `hook::POINTS` are recognized** (they are the trait method names);
    /// a single misspelling means **activation fails** — the same principle as `[store] backend` / `[loop] impl`:
    /// a setting written but never taking effect is the hardest class of bug to track down; better to argue loudly on the spot.
    pub hooks: Vec<String>,
    /// Prompt-segment ids to contribute. **The body is not written here** — it is fetched with `prompt/segment` at activation,
    /// so the prompt content is not stored in two places and the body forgotten when the manifest is edited.
    pub prompt_segments: Vec<String>,
    /// Whether to contribute a theme patch (fetched with `theme/patch` at activation).
    ///
    /// This cycle only does "fetch + validate + display"; merging in the GUI is **deferred**: the UI theme is fixed **before**
    /// the window is created (`gui/src/main.rs`'s `load_theme` runs on the main thread while plugin loading runs on a background thread),
    /// so a component patch is inherently later than the theme. Set up the protocol now to avoid redoing the RPC later.
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

    /// Effective value = manifest ← overridden by `initialize` (**last writer wins**, the same idea as the stage-5 layer stack).
    ///
    /// Why not recognize only one place: the manifest is an **offline declaration** (visible without starting the plugin, and the model writes against it),
    /// while `initialize` is the **live process state** (the plugin knows better, e.g. it decides which points to attach to from environment variables).
    /// When both are present the process wins; an old plugin that only writes the manifest still works.
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

    /// Validate and normalize: trim, drop empties, dedupe; hook points are then sorted into [`crate::hook::POINTS`]'s fixed order.
    ///
    /// An unrecognized hook point is a hard error (see the field docs). The sort also makes the question of what order hooks run in
    /// depend only on the declared content, not on the order written — the same manifest yields the same execution order every time.
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
    /// What it contributes besides tools (stage 6 · 6b). Default = tools only, same as today.
    ///
    /// `skip_serializing_if`: `save()` does not write an empty
    /// `[contributes]` section for a plugin that "contributes nothing" — an old plugin re-persisted still matches the old format.
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
    /// The **declared intent** of this tool: read-only, or possibly writing to disk.
    ///
    /// A plugin declares it with `"access": "read"` in the `initialize` tool object (default = write).
    /// The host uses it for exactly one thing: which tier to apply when judging the **workspace boundary** — reads are unrestricted, writes/deletes must be inside.
    ///
    /// Why the plugin itself must say: a plugin is a **black box**, the host cannot see what it does with that path,
    /// and can only scan argument names for "path-like" strings (`workplace::scan_paths`). That coarse net is right for writes
    /// (conservative), but for reads it is collateral damage — a read-only tool that scans the D drive gets blocked as a write.
    /// Declaring this bit fills in the information "the host cannot guess".
    ///
    /// ⚠️ It is a **declaration, not enforcement**: a plugin can still declare `read` and then write. This host-side boundary guards against
    /// "the model casually writing outside the area", not against a deliberately malicious plugin process (that is a different threat model,
    /// see `docs/SANDBOX.md`). So no validation is done here, only faithful acceptance.
    pub access: crate::workplace::Access,
}

struct RpcChannel {
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    next_id: u64,
}

impl RpcChannel {
    /// Read one line. Tolerates non-UTF-8 bytes (lossy decode); returning None means child EOF.
    ///
    /// If a child process mixes GBK/binary bytes into a protocol line, the old implementation would report
    /// "stream did not contain valid UTF-8" and break the whole RPC; here we decode on a best-effort basis.
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

/// A live plugin process
pub struct PluginHandle {
    pub name: String,
    pub language: String,
    pub description: String,
    pub tool_defs: Vec<ToolDef>,
    /// The effective component declaration (`plugin.toml [contributes]` ← the result after `initialize` overrides).
    pub contributes: Contributes,
    /// The theme patch fetched and validated at activation (same shape as `config.toml [theme]`).
    /// This cycle only **stores and displays** it; GUI merging is deferred (see [`Contributes::theme`]).
    pub theme_patch: Option<Box<crate::config::ThemeConfig>>,
    /// The failure ledger for hook RPC — **shares the same Arc pair** with [`crate::hook::ComponentHook`]:
    /// the hook side only reads (display), this side only writes (accounting). Hooks may not throw (throwing would break the main loop),
    /// so failure can only be recorded this way — degrade silently, but never vanish silently.
    pub hook_failures: Arc<std::sync::atomic::AtomicU64>,
    pub hook_last_error: Arc<std::sync::Mutex<Option<String>>>,
    cfg: RuntimesConfig,
    /// This plugin process's **fence** (Job Object, see `src/sandbox.rs`).
    ///
    /// It lives here rather than on `Runtime` so its lifetime **equals the process's lifetime**:
    /// `Fence::drop` closes the Job handle, and `KILL_ON_JOB_CLOSE` then takes the plugin away
    /// together with its descendants. So "stop the plugin" has two safeguards — `child.kill()` and closing the fence.
    ///
    /// The `Mutex` wrapper is for one thing only: `respawn` must swap it out through `&self` (atomic replacement;
    /// the old fence is Dropped at that moment ⇒ the old process is carried off by the Job, even if `kill()` did not wait cleanly).
    pub fence: std::sync::Mutex<crate::sandbox::Fence>,
    /// Config for building the fence (used by `respawn` to apply it afresh).
    sandbox: crate::sandbox::Sandbox,
    /// Application root: the plugin process's working directory. Relative paths inside the plugin therefore match the host's view.
    root: PathBuf,
    dir: PathBuf,
    manifest: PluginManifest,
    rpc: Mutex<RpcChannel>,
    child: Mutex<Child>,
}

impl PluginHandle {
    /// Send one RPC and wait for the response with a matching id (other lines are ignored).
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let mut ch = self.rpc.lock().await;
        ch.next_id += 1;
        let id = ch.next_id;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        ch.stdin.write_all(line.as_bytes()).await.context(t("failed to write to the plugin's stdin"))?;
        ch.stdin.flush().await?;
        loop {
            let line = ch
                .next_line()
                .await?
                .ok_or_else(|| anyhow!(tf!("plugin {} process has exited", self.name)))?;
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue; // a non-protocol line printed by the plugin; ignore it
            };
            if v.get("id").and_then(|x| x.as_u64()) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                bail!(tf!("plugin {} returned an error: {}", self.name, err));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    pub async fn shutdown(&self) {
        // Notify on a best-effort basis, then force it to end
        let _ = self.rpc("shutdown", Value::Null).await;
        let _ = self.child.lock().await.kill().await;
    }

    /// Kill the current process, respawn it, and re-handshake. Used for self-healing after the plugin hangs/crashes.
    pub async fn respawn(&self) -> Result<()> {
        {
            let mut c = self.child.lock().await;
            let _ = c.kill().await;
        }
        let mut child = spawn_command(&self.cfg, &self.root, &self.dir, &self.manifest, &self.sandbox)?
            .spawn()
            .context(t("failed to restart the plugin process"))?;
        // The new process must be **fenced afresh**, for the same reason as the first launch: it too comes out frozen.
        // The old fence is replaced only after this line (its Job holds only the old, already-dead process,
        // so whether it closes hardly matters, but fencing the new one first is safer).
        let fence = self.sandbox.contain(&child);
        let stdin = child.stdin.take().ok_or_else(|| anyhow!(t("cannot take the plugin's stdin")))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!(t("cannot take the plugin's stdout")))?;
        {
            let mut ch = self.rpc.lock().await;
            *ch = RpcChannel { stdin, out: BufReader::new(stdout), next_id: 0 };
        }
        *self.child.lock().await = child;
        if let Ok(mut f) = self.fence.lock() {
            *f = fence;
        }
        // Handshake to confirm the new process is usable
        self.rpc("initialize", json!({})).await?;
        Ok(())
    }

    /// A one-line short text of fence status (used by `/plugins`).
    pub fn fence_status(&self) -> String {
        match self.fence.lock() {
            Ok(f) => f.status_text(),
            Err(_) => t("fenced ?").to_string(),
        }
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

        let first = tokio::time::timeout(timeout, self.handle.rpc("tools/call", params.clone())).await;
        let first_err = match first {
            Ok(Ok(v)) => return parse_tool_result(v),
            Ok(Err(e)) => tf!("plugin {} communication failed: {}", self.handle.name, format!("{e:#}")),
            Err(_) => tf!("plugin {} response timed out ({} s)", self.handle.name, timeout.as_secs()),
        };

        // Plugin unresponsive/crashed: respawn the process and retry once
        match self.handle.respawn().await {
            Ok(()) => {
                match tokio::time::timeout(timeout, self.handle.rpc("tools/call", params)).await {
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
    // `"attach": ["<absolute path>", …]` — files **produced by the plugin**, which the host attaches for the model to see.
    //
    // This is the landing spot for "plugins can upload too": a plugin is a separate process and cannot call the host's `attach_file` tool,
    // but it can **declare** the produced files in its own return value. What the host gets is a path, going through exactly the same
    // check as a user drag-and-drop (`Attachment::probe`) — so "plugin screenshots → the model really sees that image" holds,
    // rather than just stuffing the path into the context as a string. Protocol shape in `prompt::BUILTIN_PROMPT`.
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
    crate::tool::ToolOutput { content, is_error, attach }
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
            // -X utf8 + PYTHONUTF8/PYTHONIOENCODING: force the child's stdin/stdout through UTF-8.
            // This is the key, from the host side, to curing "Windows' default GBK output breaks the JSON protocol",
            // no longer relying on the model hand-writing an encoding patch in every plugin.
            // -u: disable buffering so protocol lines are visible immediately.
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

/// Start the plugin process and complete the initialize handshake
async fn spawn_and_init(
    cfg: &RuntimesConfig,
    root: &Path,
    dir: &Path,
    manifest: &PluginManifest,
    sandbox: &crate::sandbox::Sandbox,
) -> Result<PluginHandle> {
    let mut child = spawn_command(cfg, root, dir, manifest, sandbox)?
        .spawn()
        .with_context(|| tf!("failed to start plugin {} (check whether {} is in PATH)", manifest.name, cfg.python))?;
    // Fence it immediately: the process comes out frozen, and this is the **only** place guaranteeing that, before it runs its first line of code,
    // it is already in the Job. Once inside `PluginHandle` it lives and dies with the process.
    let fence = sandbox.contain(&child);
    let stdin = child.stdin.take().ok_or_else(|| anyhow!(t("cannot take the plugin's stdin")))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!(t("cannot take the plugin's stdout")))?;

    let handle = PluginHandle {
        name: manifest.name.clone(),
        language: manifest.language.clone(),
        description: manifest.description.clone(),
        tool_defs: vec![],
        contributes: Contributes::default(),
        theme_patch: None,
        hook_failures: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        hook_last_error: Arc::new(std::sync::Mutex::new(None)),
        fence: std::sync::Mutex::new(fence),
        sandbox: sandbox.clone(),
        cfg: cfg.clone(),
        root: root.to_path_buf(),
        dir: dir.to_path_buf(),
        manifest: manifest.clone(),
        rpc: Mutex::new(RpcChannel { stdin, out: BufReader::new(stdout), next_id: 0 }),
        child: Mutex::new(child),
    };

    let result = tokio::time::timeout(INIT_TIMEOUT, handle.rpc("initialize", json!({})))
        .await
        .map_err(|_| anyhow!(tf!("plugin {} initialize handshake timed out ({} s)", manifest.name, INIT_TIMEOUT.as_secs())))??;

    // ---- Tools (**not a single field of the old shape was changed**) ----
    // A missing `tools` is treated as empty: a component that only contributes hooks/prompt segments and
    // provides no tools has no reason to build an empty array. If it is meant to be an array but is not (wrong type), it still errors on the spot.
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
    let live = match result.get("contributes") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            serde_json::from_value::<Contributes>(v.clone())
                .with_context(|| tf!("failed to parse plugin {}'s initialize.contributes", manifest.name))?,
        ),
    };
    let contributes = manifest
        .contributes
        .clone()
        .overridden_by(live)
        .normalize()
        .with_context(|| tf!("plugin {}'s contribution declaration is malformed", manifest.name))?;

    // **The one thing stage 6 · 6b loosened**: previously "declared no tools at all" was a straight failure, so
    // a component that "only contributes hooks, provides no tools" had no path to life at all. Now it takes the union —
    // there must be **at least one** of tools and contributions; having neither is a genuine config error (it does nothing).
    if defs.is_empty() && contributes.is_empty() {
        bail!(tf!(
            "plugin {} declares neither tools nor any [contributes] — it would do nothing",
            manifest.name
        ));
    }

    Ok(PluginHandle { tool_defs: defs, contributes, ..handle })
}

/// Extract the "workspace intent" from a tool declaration.
///
/// **Only `"read"` is recognized** (case-insensitive, trimming tolerated); everything else — writing `"write"`, misspelling `"ReadOnly"`,
/// typing it as a number, or simply having no such field — all fall through to "write". This asymmetry is deliberate:
/// the two consequences of a wrong direction differ by orders of magnitude — **a misspelling only blocks the tool one extra time (the user sees it, the model can fix it),
/// whereas getting it wrong lets an out-of-bounds write land directly on disk** (irreversible). So better to recognize too little than too much.
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
///
/// It goes to stderr rather than a UI event: activation happens **while the Runtime is being assembled** (the CLI has not entered the REPL,
/// the GUI has not built its window), and at that moment there is no ui to send to. Runtime accidents have
/// [`crate::hook::ComponentHook`]'s failure ledger to fall back on (visible in `/hook`).
fn component_warn(plugin: &str, why: impl std::fmt::Display) {
    eprintln!("{}", tf!("[component] {}: {}", plugin, why));
}

/// Register the plugin's **tools and contributions** into the Runtime and bring the handle under management.
///
/// Order: **fetch contribution content first (needs RPC) → then register tools → attach contributions last**.
///
/// The two error-handling lines are deliberately separate:
/// - **tool registration failure = the whole plugin fails to install** (same as today, rolling back what was registered);
/// - **failure to fetch contribution content = warn about that one only** (degrade). Failing to fetch one prompt segment
///   must not scrap tools the plugin can already use.
async fn activate(rt: &Arc<Runtime>, handle: PluginHandle) -> Result<Vec<String>> {
    // ---- ① Fetch the contributions that need RPC (prompt segment bodies / theme patch) ----
    let mut seg_texts: Vec<(String, String)> = Vec::new();
    for id in &handle.contributes.prompt_segments {
        match handle.rpc("prompt/segment", json!({ "id": id })).await {
            Ok(v) => {
                let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                if text.trim().is_empty() {
                    component_warn(&handle.name, tf!("prompt segment {}'s body is empty; skipping", id));
                } else {
                    seg_texts.push((id.clone(), text));
                }
            }
            Err(e) => component_warn(&handle.name, tf!("failed to fetch prompt segment {}: {}", id, format!("{e:#}"))),
        }
    }

    let mut theme_patch: Option<Box<crate::config::ThemeConfig>> = None;
    if handle.contributes.theme {
        match handle.rpc("theme/patch", Value::Null).await {
            Ok(v) => match serde_json::from_value::<crate::config::ThemeConfig>(v) {
                Ok(t) => theme_patch = Some(Box::new(t)),
                Err(e) => component_warn(
                    &handle.name,
                    tf!("the theme patch is not [theme]-shaped; ignored: {}", format!("{e:#}")),
                ),
            },
            Err(e) => component_warn(&handle.name, tf!("failed to fetch the theme patch: {}", format!("{e:#}"))),
        }
    }

    let handle = Arc::new(PluginHandle { theme_patch, ..handle });

    // ---- ② Tools: registration failure rolls back the whole thing (same as today) ----
    let mut registered: Vec<String> = vec![];
    for def in &handle.tool_defs {
        let tool = Arc::new(PluginTool { handle: handle.clone(), def: def.clone() });
        if let Err(e) = rt.register(tool).await {
            for n in &registered {
                rt.unregister(n).await;
            }
            handle.shutdown().await;
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

    // ---- ④ Contributions: prompt segments (bodies already fetched in ①; here they only enter the registry) ----
    for (id, body) in seg_texts {
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
    // Only a "currently running plugin" needs unloading first; a leftover directory (from a previous failed creation) is auto-cleaned and rebuilt,
    // avoiding the dead loop of "creation says it already exists, removal says it does not exist".
    if rt.plugins.lock().await.contains_key(name) {
        bail!(tf!("plugin {} is running; remove_plugin first if you want to rebuild it", name));
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
        let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
        // Use the resolved rustc (possibly the one bundled in the app directory), not the raw value from config
        let runtimes = rt.runtimes();
        let mut cmd = Command::new(&runtimes.rustc);
        cmd.arg("--edition=2021").arg("-O");
        // Statically link the CRT, consistent with the host's own build (see .cargo/config.toml).
        // Only added on windows: on other platforms +crt-static has different semantics (it affects libc's static/dynamic linking),
        // whereas the goal here is exactly one thing: "stop the plugin exe depending on VCRUNTIME140.dll".
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
        entry = exe;
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

    let runtimes = rt.runtimes();
    let handle = spawn_and_init(&runtimes, &rt.root, dir, &manifest, &rt.sandbox).await?;
    activate(rt, handle).await
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
        for def in &handle.tool_defs {
            rt.unregister(&def.name).await;
            removed.push(def.name.clone());
        }
        handle.shutdown().await;
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

/// On startup, scan the plugins/ directory and restore all persisted plugins.
/// Returns a list of (plugin name, result); failures do not block startup.
pub async fn load_all(rt: &Arc<Runtime>) -> Vec<(String, Result<Vec<String>>)> {
    let mut out = vec![];
    let Ok(entries) = std::fs::read_dir(&rt.plugins_dir) else {
        return out;
    };
    // The resolved interpreter config, shared across the whole scan (never rewritten midway)
    let runtimes = rt.runtimes();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || dir.file_name().map(|n| n == "sdk").unwrap_or(false) {
            continue;
        }
        let manifest = match PluginManifest::load(&dir) {
            Ok(m) => m,
            Err(e) => {
                out.push((
                    dir.file_name().unwrap_or_default().to_string_lossy().to_string(),
                    Err(anyhow!(tf!("failed to read the manifest: {}", format!("{e:#}")))),
                ));
                continue;
            }
        };
        let name = manifest.name.clone();
        let result = match spawn_and_init(&runtimes, &rt.root, &dir, &manifest, &rt.sandbox).await {
            Ok(handle) => activate(rt, handle).await,
            Err(e) => Err(e),
        };
        out.push((name, result));
    }
    out
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

        // initialize said nothing → use the manifest entirely
        assert_eq!(manifest.clone().overridden_by(None).hooks, vec!["before_tool"]);

        // initialize spoke → the fields it names are its call, the rest stay with the manifest
        let live = Contributes { hooks: vec!["on_error".into()], theme: true, ..Default::default() };
        let eff = manifest.overridden_by(Some(live));
        assert_eq!(eff.hooks, vec!["on_error"], "the process's live answer overrides the manifest");
        assert!(eff.theme, "the manifest does not mention theme; take initialize's");
        assert_eq!(eff.caps, vec!["fs_read"], "initialize does not mention caps; keep the manifest's");
    }

    /// How the declared **read intent** is parsed.
    ///
    /// This pins an **asymmetry**: the two consequences of a wrong direction differ by orders of magnitude — failing to recognize a read
    /// blocks the tool one extra time (the user sees it, the model can fix it); recognizing a write as a read lets an out-of-bounds write
    /// land directly on disk (irreversible). So only `"read"` is recognized; everything else falls to the most conservative "write".
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
