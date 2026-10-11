//! Loop hooks (the first shape of the Loop seam).
//!
//! Five hook points hang off the loop skeleton **without touching the skeleton itself**: before a
//! request, after a reply, before a tool call, after a tool result, and when a request errors.
//! These five points are the 80% solution for "change system behaviour" — the remaining 20%
//! (replacing the whole loop) belongs to `AgentLoop` in [`crate::agent_loop`] (phase 6 · 6a is
//! already in place: `StandardLoop` is the skeleton those points used to live in).
//!
//! | Hook point | Where | What it can do | On rejection |
//! |---|---|---|---|
//! | [`Hook::before_request`] | before the request | swap the tool list, append messages | abort this round |
//! | [`Hook::after_reply`] | after the reply (**after accounting**) | edit/clear the reply | treated as an invalid reply; takes the empty-reply retry path |
//! | [`Hook::before_tool`] | before the tool call | edit `args` | the tool is not run; the reason is fed back as the tool result |
//! | [`Hook::after_tool`] | after the tool result | edit the result | the whole block becomes an error result carrying the reason |
//! | [`Hook::on_error`] | where `llm.chat` fails | degrade and retry | — |
//!
//! # With no hooks, behaviour is exactly as it was before hooks existed
//!
//! All five [`Hook`] methods **have default empty implementations**; when `config.toml [hook]` is
//! empty, `Runtime.hooks` is an empty chain. Every call on an empty chain degenerates into
//! "check the length and return", so every loop path is **equivalent** to before hooks existed —
//! that is this phase's hard acceptance criterion.
//!
//! # Why hooks don't get `&mut Vec<Message>`
//!
//! `before_request` can change things, but it **cannot** get `&mut messages`: it can only put
//! what it wants to say into [`Request::inject`], and the loop enqueues it via
//! `Agent::push_message`.
//!
//! The reason: the only write path for `messages` is `Agent::push_message`, which **also** writes
//! the file; rollback goes through `truncate_messages` + `rewrite`. Handing a raw `Vec` to a hook
//! is opening a back door where "it's in memory but not in the history file" — that isn't a maybe,
//! it's a certainty one minute later.
//!
//! The "trim / rewrite history" hole deliberately left open at the time **has since been filled by
//! `[loop] max_rounds` in phase 6 · 6a**: trimming naturally belongs to the loop layer (it must
//! touch memory and the conversation file at once, and rebuild the request after trimming). The
//! hook layer still only exposes "append".
//!
//! # Two sources: config-driven + component contributions (phase 6 · 6b)
//!
//! - [`ConfigHook`] — from `config.toml [hook]`, fixed at startup;
//! - [`ComponentHook`] — from a plugin's `plugin.toml [contributes] hooks`, attached when the
//!   plugin activates.
//!
//! The latter rides on [the existing subprocess JSON-RPC protocol](crate::plugin): a subprocess
//! plugin is already in the cost class of "one RPC per tool call", so it **can afford hooks** —
//! no need to stuff a scripting engine into the host for this. That judgement is the biggest
//! divergence between phase 6's roadmap and the original plan (the original assumed "hooks must be
//! in-process", which the goal itself does not require).
//!
//! A component subprocess dying / timing out / returning garbage = that component **fails
//! silently**: log one failure and let it through, and the main loop runs to completion as usual.
//! "Must be degradable" is achieved by giving every exit of [`ComponentHook`] a fallback value.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::config::HookConfig;
use crate::llm::ChatReply;
use crate::message::Message;
use crate::runtime::Runtime;
use crate::tool::ToolOutput;

/// A hook's ruling on "the step about to happen".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Let it through — the default, and the result when "there are no hooks at all".
    Pass,
    /// Stop this step. `reason` must always be fit for human eyes: it is fed back to the model, or
    /// used as this round's abort reason.
    Reject(String),
}

/// The part of the request that `before_request` can see and change.
pub struct Request {
    /// The tool schema list sent to the model this round (default = `Runtime::tool_schemas()`).
    /// A hook may replace it wholesale — e.g. pulling a few tools out of this round by policy.
    pub tools: Vec<Value>,
    /// Messages the hook asks to **append**. The loop enqueues them via `Agent::push_message`
    /// (see the module docs).
    ///
    /// Offering only "append" rather than "rewrite" is deliberate: appending is the only change
    /// that can be persisted without `rewrite`.
    pub inject: Vec<Message>,
}

/// What a hook can do about a failed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorAction {
    /// Throw it as-is (the default) — exactly as if there were no hook.
    Propagate,
    /// Swallow this error: inject `note` into the conversation as a host prompt, then retry.
    ///
    /// Guarding against infinite loops is **the emitter's own responsibility** (e.g. degrade the
    /// same error only once). The host side has another config fallback
    /// (`[hook] max_llm_retries`) — retries work without writing a hook or installing a component.
    ///
    /// The real constructor (from phase 6 · 6b on): a component's `hooks/on_error` returns
    /// `{"action":"retry","note":"…"}`, which [`ComponentHook`] maps here.
    Retry(String),
}

/// The names of the five hook points — exactly the five method names of [`Hook`], **no more and no
/// less**.
///
/// These are the strings a component writes in `plugin.toml [contributes] hooks`. Deliberately no
/// "alias table": one extra mapping is one more place to get wrong, and getting it wrong shows up
/// as "the hook doesn't take effect" — precisely the hardest class to track down.
pub const POINTS: [&str; 5] = [
    "before_request",
    "after_reply",
    "before_tool",
    "after_tool",
    "on_error",
];

/// A loop hook. All five methods are "do nothing by default".
///
/// Why the methods are all `async`: hooks often need to query the system's current state (tool
/// list, plugin table), and those queries are themselves async. Same convention as
/// [`crate::tool::Tool`] / [`crate::ui::Ui`].
#[async_trait]
pub trait Hook: Send + Sync {
    /// The name (used by the `/hook` report and diagnostics)
    fn name(&self) -> &str;

    /// One sentence of description for the `/hook` report (falls back to the name)
    fn describe(&self) -> String {
        self.name().to_string()
    }

    /// ① Before the request: may replace this round's tool list, may ask to append messages, and
    /// may **abort this round**.
    async fn before_request(&self, _rt: &Arc<Runtime>, _req: &mut Request) -> Verdict {
        Verdict::Pass
    }

    /// ② After the reply: may edit this reply (text / tool calls), and may **reject** it.
    ///
    /// Reject = treat it as one invalid reply, taking the same retry path as "the model returned
    /// an empty reply". Note this point sits **after accounting**: the tokens of a rejected call
    /// really were spent, and cannot go unaccounted just because the reply wasn't used.
    async fn after_reply(&self, _rt: &Arc<Runtime>, _reply: &mut ChatReply) -> Verdict {
        Verdict::Pass
    }

    /// ③ Before the tool call: may edit `args`, and may stop this call (stopping means the tool is
    /// **not executed**).
    ///
    /// ⚠️ At this moment the assistant message carrying `tool_calls` **is already on disk**; what
    /// the file holds is the model's original arguments. Changes here only affect this execution
    /// and are **not written back to history** — history must faithfully record "what the model
    /// asked for", whereas hooks are a host-side execution policy.
    async fn before_tool(&self, _rt: &Arc<Runtime>, _name: &str, _args: &mut Value) -> Verdict {
        Verdict::Pass
    }

    /// ④ After the tool result: may edit the result, and may stop it (stopping = replacing the
    /// whole block with an error result carrying `reason`).
    async fn after_tool(&self, _rt: &Arc<Runtime>, _name: &str, _out: &mut ToolOutput) -> Verdict {
        Verdict::Pass
    }

    /// ⑤ Request failed: may handle it degradably.
    ///
    /// **Only errors of the model request itself reach here**. Tool execution failures, and the
    /// `bail!` for "the model returned empty replies repeatedly", don't count — those are model
    /// behaviour, not infrastructure faults, and each already has its own diagnostics written.
    async fn on_error(&self, _rt: &Arc<Runtime>, _err: &anyhow::Error) -> ErrorAction {
        ErrorAction::Propagate
    }
}

/// A set of hooks plus the semantics of "run in registration order, short-circuit on rejection".
///
/// `run_loop` only ever talks to this and doesn't know how many hooks there are or who they are —
/// the same shape as [`crate::store::Store`] / [`crate::auth::Policy`] (principle P2).
///
/// # Why the chain is mutable (phase 6 · 6b)
///
/// Config-driven hooks are fixed in `Runtime::new`, but **component-contributed hooks can't be** —
/// plugins are only pulled up by `load_all` **after** `Runtime::new`. For "a component's
/// declaration to take effect immediately", the chain must support runtime appends, so it went
/// from a `Vec` to a `RwLock<Vec<_>>` internally.
///
/// The hard criterion "installing no components = byte-for-byte identical to today" is unaffected:
/// with 0 hooks in the chain, each of the five hook points loops 0 times over **an empty vec** —
/// equivalent path by path to before the change (`empty_chain_never_intervenes` pins exactly this).
#[derive(Default)]
pub struct HookChain {
    hooks: std::sync::RwLock<Vec<Arc<dyn Hook>>>,
}

impl HookChain {
    pub fn new(hooks: Vec<Arc<dyn Hook>>) -> Self {
        Self { hooks: std::sync::RwLock::new(hooks) }
    }

    /// Take a snapshot of the hooks (cloning `Arc`s, N reference-count bumps).
    ///
    /// Why not iterate while holding the lock: every hook-point call has to `await` (hooks often
    /// query async state), and a `std::sync` read guard **is not `Send`** — holding it across an
    /// `await` makes the whole future non-`Send`, and `tokio::spawn` simply won't compile. So
    /// honestly clone a copy and drop the lock.
    ///
    /// Cloning an empty chain allocates nothing (`Vec::clone` doesn't build a heap for an empty
    /// vec), so under the default "no hooks at all" configuration this snapshot path is zero-cost.
    fn snapshot(&self) -> Vec<Arc<dyn Hook>> {
        self.hooks.read().unwrap().clone()
    }

    /// Append a hook at runtime (component-contributed hooks come in this way).
    pub fn push(&self, hook: Arc<dyn Hook>) {
        self.hooks.write().unwrap().push(hook);
    }

    /// Drop every hook that belongs to `plugin`.
    ///
    /// This is the counterpart of [`HookChain::push`] that used to be unnecessary: with no way to
    /// replace a live component, a hook could only ever stop by the plugin disappearing from the
    /// `plugins` table (which `ComponentHook::ask` notices on its own). A **hot swap** breaks that
    /// assumption — the plugin stays in the table under the same name, so the old hook would sit in
    /// the chain as a ghost, firing against the new process with the previous declaration. Called
    /// from `activate_slot` before the new contributions are pushed, it is a harmless no-op on the
    /// create path (nothing was there yet) and the fix on the update path.
    ///
    /// Matching is by [`Hook::name`], which a `ComponentHook` returns as its plugin name.
    pub fn remove_by_plugin(&self, plugin: &str) {
        self.hooks.write().unwrap().retain(|h| h.name() != plugin);
    }

    /// No hooks at all — in this case the path `run_loop` takes is equivalent, path by path, to
    /// before the "Loop seam".
    pub fn is_empty(&self) -> bool {
        self.hooks.read().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.hooks.read().unwrap().len()
    }

    pub async fn before_request(&self, rt: &Arc<Runtime>, req: &mut Request) -> Verdict {
        for h in &self.snapshot() {
            if let Verdict::Reject(why) = h.before_request(rt, req).await {
                return Verdict::Reject(why);
            }
        }
        Verdict::Pass
    }

    pub async fn after_reply(&self, rt: &Arc<Runtime>, reply: &mut ChatReply) -> Verdict {
        for h in &self.snapshot() {
            if let Verdict::Reject(why) = h.after_reply(rt, reply).await {
                return Verdict::Reject(why);
            }
        }
        Verdict::Pass
    }

    pub async fn before_tool(&self, rt: &Arc<Runtime>, name: &str, args: &mut Value) -> Verdict {
        for h in &self.snapshot() {
            if let Verdict::Reject(why) = h.before_tool(rt, name, args).await {
                return Verdict::Reject(why);
            }
        }
        Verdict::Pass
    }

    pub async fn after_tool(&self, rt: &Arc<Runtime>, name: &str, out: &mut ToolOutput) -> Verdict {
        for h in &self.snapshot() {
            if let Verdict::Reject(why) = h.after_tool(rt, name, out).await {
                return Verdict::Reject(why);
            }
        }
        Verdict::Pass
    }

    /// Ask each hook in turn; **the first one offering a degradation wins** (if all return
    /// `Propagate`, it is thrown as-is).
    pub async fn on_error(&self, rt: &Arc<Runtime>, err: &anyhow::Error) -> ErrorAction {
        for h in &self.snapshot() {
            match h.on_error(rt, err).await {
                ErrorAction::Propagate => {}
                other => return other,
            }
        }
        ErrorAction::Propagate
    }

    /// The `/hook` report: which hooks are currently registered and what each does.
    ///
    /// Like `/prompt`, it deliberately **does not align columns**: names are ASCII while
    /// descriptions may be non-ASCII, and padding with `{:<12}` only gets knocked crooked by
    /// double-width characters.
    pub fn report(&self) -> String {
        if self.is_empty() {
            return crate::i18n::t("Loop hooks: 0 registered — behavior is identical to the built-in loop.\n  Two sources: config.toml's [hook] (config-driven, fixed at startup); plugin plugin.toml's [contributes] hooks (component contributions, attached on activation).")
                .to_string();
        }
        let mut out = crate::tf!("Loop hooks: {} registered · run in registration order\n", self.len());
        for (i, h) in self.snapshot().iter().enumerate() {
            out.push_str(&format!("\n  {}. {}\n     {}\n", i + 1, h.name(), h.describe()));
        }
        out.push_str(
            crate::i18n::t("\n  Sources: config.toml's [hook] / plugin [contributes] hooks. Config changes need a restart; components follow plugin activation/unload."),
        );
        out
    }
}

// ─────────────────────────── The config-driven implementation ───────────────────────────

/// A hook driven by `config.toml [hook]`.
///
/// In this round `[hook]` does exactly one thing: **fill in default arguments** for named tools.
///
/// "Forbidding a class of tools" **is not here** — that's an authorization question and belongs to
/// `[auth]` (since phase 4 its override keys support wildcards like `fs_*`). The two mechanisms
/// govern two different things: `[auth]` decides "let it through or not", `[hook]` decides
/// "after letting it through, should the arguments change".
pub struct ConfigHook {
    tool_args_default: HashMap<String, Value>,
}

impl ConfigHook {
    /// `Ok(None)` = nothing was written in the config, so there's no need to register this hook
    /// (preserving "empty config = empty chain").
    pub fn from_config(cfg: &HookConfig) -> Result<Option<Self>> {
        if cfg.tool_args_default.is_empty() {
            return Ok(None);
        }
        let mut map = HashMap::with_capacity(cfg.tool_args_default.len());
        for (tool, defaults) in &cfg.tool_args_default {
            let tool = tool.trim();
            if tool.is_empty() {
                continue;
            }
            // toml::Value → serde_json::Value: the model-side arguments are JSON values to begin
            // with, and the "fill only missing keys" merge must happen on one shared type, not on
            // two separate representations.
            let v = serde_json::to_value(defaults)
                .with_context(|| crate::tf!("[hook.tool_args_default.{}] cannot be converted to JSON", tool))?;
            map.insert(tool.to_string(), v);
        }
        if map.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self { tool_args_default: map }))
    }
}

#[async_trait]
impl Hook for ConfigHook {
    fn name(&self) -> &str {
        "config"
    }

    fn describe(&self) -> String {
        let mut names: Vec<&String> = self.tool_args_default.keys().collect();
        names.sort();
        let items: Vec<String> = names
            .iter()
            .map(|n| {
                let keys = match self.tool_args_default.get(*n) {
                    Some(Value::Object(m)) => {
                        let mut k: Vec<&String> = m.keys().collect();
                        k.sort();
                        k.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(crate::i18n::t(", "))
                    }
                    _ => crate::i18n::t("(not an object; ignored)").to_string(),
                };
                format!("{n}({keys})")
            })
            .collect();
        crate::tf!("Fills in default arguments (only keys the model omitted): {}", items.join(crate::i18n::t(", ")))
    }

    async fn before_tool(&self, _rt: &Arc<Runtime>, name: &str, args: &mut Value) -> Verdict {
        let Some(defaults) = self.tool_args_default.get(name) else {
            return Verdict::Pass;
        };
        let (Some(dst), Some(src)) = (args.as_object_mut(), defaults.as_object()) else {
            // The arguments aren't an object (or the defaults were written as a scalar) — nothing
            // to merge, pass quietly, don't guess.
            return Verdict::Pass;
        };
        // **Fill missing keys only**: keys the model explicitly gave are never touched.
        // Overriding the model's explicit intent would create the worst failure mode of all,
        // "the model said A but the tool received B" — far harder to track down than "a default is
        // missing".
        for (k, v) in src {
            dst.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Verdict::Pass
    }
}

// ─────────────────────── Component-contributed hooks (phase 6 · 6b) ───────────────────────

/// The wait cap for one hook RPC.
///
/// Why hooks get their own cap far smaller than `agent.tool_timeout_secs` (120 seconds by default):
/// a stuck hook blocks **the entire main loop** (the user watches a spinner), whereas a stuck tool
/// only blocks that one call. A local script takes well under 10 seconds to make one judgement;
/// anything beyond is "this component is broken" — treat it as degradable, which is far more
/// meaningful than making the user wait two minutes.
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// A hook declared by a plugin's `[contributes] hooks`.
///
/// It **contains no policy of its own**; it does three things: turn a hook point into one RPC, map
/// the result into [`Verdict`] / [`ErrorAction`], and on failure let it through while booking the
/// failure.
///
/// # Why look the handle up by "plugin name" on demand instead of holding an `Arc<PluginHandle>`
///
/// ① To avoid a cycle — `Runtime.plugins` already stores the handle, and the hook holding another
/// copy would make it a two-way reference;
/// ② You get "uninstalling the component disables it" for free: not found ⇒ pass, with no
/// deregistration logic needed;
/// ③ Consistent with [`crate::plugin::PluginTool`] — it too holds a name and looks it up when
/// used.
///
/// The cost is one extra `plugins` lock per hook point — five points per round, negligible.
pub struct ComponentHook {
    plugin: String,
    /// The points the component declared (already validated; can only come from [`POINTS`])
    points: Vec<String>,
    /// The capabilities the component requested (shown in the `/hook` report; the authorization
    /// decision itself goes through [`crate::auth::Policy`])
    caps: Vec<String>,
    /// **Shares the same pair of counters** as `PluginHandle`, so the `/hook` report can read the
    /// failure tally in sync.
    failures: Arc<AtomicU64>,
    last_error: Arc<std::sync::Mutex<Option<String>>>,
}

impl ComponentHook {
    /// Build one from its "parts".
    ///
    /// It deliberately **does not take a `PluginHandle`**: the `hook` layer doesn't need to know
    /// what a plugin handle looks like, only "plugin name + declaration". One less coupling, and
    /// the degradation path becomes easier to test in isolation (just pass a nonexistent plugin
    /// name, as this module's tests do).
    pub fn new(
        plugin: String,
        points: Vec<String>,
        caps: Vec<String>,
        failures: Arc<AtomicU64>,
        last_error: Arc<std::sync::Mutex<Option<String>>>,
    ) -> Self {
        Self { plugin, points, caps, failures, last_error }
    }

    fn has(&self, point: &str) -> bool {
        self.points.iter().any(|p| p == point)
    }

    fn note_failure(&self, why: String) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = Some(why);
    }

    /// Send one hook RPC. **Every** abnormal path (component absent, timeout, error) collapses
    /// into `None`, and the caller lets it through accordingly — nothing on the hook path is
    /// allowed to throw an error back into the main loop.
    ///
    /// The RPC goes through `rpc_ready`, not a raw `rpc`: under lazy loading a hook-only component
    /// is normally **dormant**, and this is what starts it. A component that fires on every turn and
    /// should not pay for that start each time belongs in `[plugins] eager` — see the config docs.
    async fn ask(&self, rt: &Arc<Runtime>, point: &str, params: Value) -> Option<Value> {
        let handle = rt.plugins.lock().await.get(&self.plugin).cloned();
        let Some(handle) = handle else {
            self.note_failure(crate::tf!("component {} is not running (unloaded or failed to load)", self.plugin));
            return None;
        };
        let method = format!("hooks/{point}");
        match tokio::time::timeout(HOOK_TIMEOUT, handle.rpc_ready(rt, &method, params)).await {
            Ok(Ok(v)) => Some(v),
            Ok(Err(e)) => {
                self.note_failure(format!("{method}: {e:#}"));
                None
            }
            Err(_) => {
                self.note_failure(crate::tf!("{}: timed out ({} s)", method, HOOK_TIMEOUT.as_secs()));
                None
            }
        }
    }
}

/// Map a component's returned `{"verdict": …}` into a [`Verdict`].
///
/// Any unrecognised value (misspelled, wrong type, simply absent) counts as `Pass` — this is the
/// **execution path**, and throwing here would let one bad component kill the whole round, which
/// directly conflicts with "components must be degradable". Problems that deserve to be seen go
/// into the `note_failure` tally, not here.
fn verdict_of(v: &Value) -> Verdict {
    match v.get("verdict").and_then(|x| x.as_str()) {
        Some("reject") => Verdict::Reject(
            v.get("reason")
                .and_then(|x| x.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(crate::i18n::t("the component asked to block this step but gave no reason"))
                .to_string(),
        ),
        _ => Verdict::Pass,
    }
}

#[async_trait]
impl Hook for ComponentHook {
    fn name(&self) -> &str {
        &self.plugin
    }

    fn describe(&self) -> String {
        let mut s = crate::tf!("component contribution · hooks into {}", self.points.join(crate::i18n::t(", ")));
        if !self.caps.is_empty() {
            s.push_str(&crate::tf!(" · requires caps {}", self.caps.join(crate::i18n::t(", "))));
        }
        let n = self.failures.load(Ordering::Relaxed);
        if n > 0 {
            let last = self
                .last_error
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| crate::i18n::t("(no details)").to_string());
            s.push_str(&crate::tf!("\n     ⚠ failed {} time(s) (treated as degradable; most recent: {})", n, last));
        }
        s
    }

    async fn before_request(&self, rt: &Arc<Runtime>, req: &mut Request) -> Verdict {
        if !self.has("before_request") {
            return Verdict::Pass;
        }
        // The tool list is only **sent out** for the component to inspect (so it can judge); this
        // round does not accept a component replacing the whole tool table — that would hand "what
        // the model can see this round" entirely to the component, a risk out of proportion to the
        // benefit.
        let params = json!({ "tools": req.tools });
        let Some(v) = self.ask(rt, "before_request", params).await else {
            return Verdict::Pass;
        };
        // Injection only accepts "user text": this hook layer only exposes "append" (see the first
        // point in the module docs).
        if let Some(arr) = v.get("inject").and_then(|x| x.as_array()) {
            for t in arr {
                if let Some(s) = t.as_str().filter(|s| !s.trim().is_empty()) {
                    req.inject.push(Message::user(s));
                }
            }
        }
        verdict_of(&v)
    }

    async fn after_reply(&self, rt: &Arc<Runtime>, reply: &mut ChatReply) -> Verdict {
        if !self.has("after_reply") {
            return Verdict::Pass;
        }
        // Only the **count** is sent, not the plaintext of the tool calls — this round's component
        // has no way to change them anyway, and sending them would only tempt people into thinking
        // they could.
        let params = json!({
            "text": reply.message.text(),
            "tool_calls": reply.message.tool_calls.as_ref().map_or(0, |c| c.len()),
        });
        let Some(v) = self.ask(rt, "after_reply", params).await else {
            return Verdict::Pass;
        };
        // Only the **text** may be changed. Tool calls are not editable: that is the record of
        // "what the model wants to do", and letting a component change it amounts to forging model
        // intent — both the request body and the persisted history would then drift.
        if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
            reply.message.set_text(t);
        }
        verdict_of(&v)
    }

    async fn before_tool(&self, rt: &Arc<Runtime>, name: &str, args: &mut Value) -> Verdict {
        if !self.has("before_tool") {
            return Verdict::Pass;
        }
        let params = json!({ "tool": name, "args": args });
        let Some(v) = self.ask(rt, "before_tool", params).await else {
            return Verdict::Pass;
        };
        if let Some(a) = v.get("args") {
            if !a.is_null() {
                *args = a.clone();
            }
        }
        verdict_of(&v)
    }

    async fn after_tool(&self, rt: &Arc<Runtime>, name: &str, out: &mut ToolOutput) -> Verdict {
        if !self.has("after_tool") {
            return Verdict::Pass;
        }
        let params = json!({ "tool": name, "content": out.content, "is_error": out.is_error });
        let Some(v) = self.ask(rt, "after_tool", params).await else {
            return Verdict::Pass;
        };
        if let Some(c) = v.get("content").and_then(|x| x.as_str()) {
            out.content = c.to_string();
        }
        if let Some(b) = v.get("is_error").and_then(|x| x.as_bool()) {
            out.is_error = b;
        }
        verdict_of(&v)
    }

    async fn on_error(&self, rt: &Arc<Runtime>, err: &anyhow::Error) -> ErrorAction {
        if !self.has("on_error") {
            return ErrorAction::Propagate;
        }
        let params = json!({ "error": format!("{err:#}") });
        let Some(v) = self.ask(rt, "on_error", params).await else {
            return ErrorAction::Propagate;
        };
        match v.get("action").and_then(|x| x.as_str()) {
            Some("retry") => ErrorAction::Retry(
                v.get("note")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(crate::i18n::t("(component suggested a retry but gave no hint)"))
                    .to_string(),
            ),
            _ => ErrorAction::Propagate,
        }
    }
}

/// Build the **config-driven** hook chain for `Runtime::new`: currently `[hook]` is the only
/// implementation.
///
/// Component-contributed hooks don't come from here — they are attached via [`HookChain::push`]
/// when a plugin activates (plugins are loaded after `Runtime::new`).
///
/// Returning a `Vec` rather than the chain itself is so that `Runtime` can see at a glance that
/// "empty config = empty chain". Anything in the config that can't work (say, a default that
/// isn't an object) **errors at startup** rather than being silently skipped — the same principle
/// as `[store] backend`: written but ineffective is the hardest class of problem to track down.
pub fn registry_from_config(cfg: &HookConfig) -> Result<Vec<Arc<dyn Hook>>> {
    let mut hooks: Vec<Arc<dyn Hook>> = Vec::new();
    if let Some(h) = ConfigHook::from_config(cfg)? {
        hooks.push(Arc::new(h));
    }
    Ok(hooks)
}

/// The body of the `/hook` command.
pub fn report(rt: &Runtime) -> String {
    rt.hooks.report()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RuntimesConfig};
    use serde_json::json;
    use std::sync::Mutex;

    /// Build a runtime that never touches real directories (`Config::dummy()` + interpreters that
    /// don't exist in this context).
    fn rt() -> Arc<Runtime> {
        let mut cfg = Config::dummy();
        cfg.runtimes = RuntimesConfig {
            python: "__none__.exe".into(),
            node: "__none__.exe".into(),
            rustc: "__none__.exe".into(),
            python_url: None,
            python_sha256: None,
        };
        let root = std::env::temp_dir().join("aox-hook-test");
        Runtime::new(cfg, root.clone(), root.join("plugins")).expect("the test runtime should not fail")
    }

    /// A fake hook that records "I was called".
    struct Rec {
        tag: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
        verdict: Verdict,
    }

    #[async_trait]
    impl Hook for Rec {
        fn name(&self) -> &str {
            self.tag
        }
        async fn before_tool(&self, _rt: &Arc<Runtime>, _name: &str, _args: &mut Value) -> Verdict {
            self.log.lock().unwrap().push(self.tag);
            self.verdict.clone()
        }
    }

    fn rec(tag: &'static str, log: &Arc<Mutex<Vec<&'static str>>>, verdict: Verdict) -> Arc<dyn Hook> {
        Arc::new(Rec { tag, log: log.clone(), verdict })
    }

    fn cfg_with(tool: &str, keys: &[(&str, &str)]) -> HookConfig {
        let mut inner = HashMap::new();
        for (k, v) in keys {
            inner.insert((*k).to_string(), toml::Value::String((*v).to_string()));
        }
        HookConfig {
            tool_args_default: [(tool.to_string(), inner)].into_iter().collect(),
            ..Default::default()
        }
    }

    /// All five points of an empty chain are "do nothing" — this is the mechanism guaranteeing
    /// "no hooks = unchanged behaviour".
    #[tokio::test]
    async fn empty_chain_never_intervenes() {
        let rt = rt();
        let chain = HookChain::default();
        assert!(chain.is_empty() && chain.len() == 0);

        let mut req = Request { tools: vec![json!({"a": 1})], inject: Vec::new() };
        assert_eq!(chain.before_request(&rt, &mut req).await, Verdict::Pass);
        assert_eq!(req.tools.len(), 1, "an empty chain must not touch tools");
        assert!(req.inject.is_empty(), "an empty chain must not inject messages");

        let mut reply = ChatReply {
            message: Message::assistant(Some("hi".into()), None, Vec::new()),
            finish_reason: Some("stop".into()),
            usage: Default::default(),
        };
        assert_eq!(chain.after_reply(&rt, &mut reply).await, Verdict::Pass);
        assert_eq!(reply.message.text(), Some("hi"), "an empty chain must not edit the reply");

        let mut args = json!({"path": "a"});
        assert_eq!(chain.before_tool(&rt, "t", &mut args).await, Verdict::Pass);
        assert_eq!(args, json!({"path": "a"}), "an empty chain must not edit the arguments");

        let mut out = ToolOutput::ok("result");
        assert_eq!(chain.after_tool(&rt, "t", &mut out).await, Verdict::Pass);
        assert_eq!(out.content, "result");

        let e = anyhow::anyhow!("boom");
        assert_eq!(chain.on_error(&rt, &e).await, ErrorAction::Propagate);
    }

    /// Run in registration order; short-circuit at the first `Reject`, and later hooks must not be
    /// called.
    #[tokio::test]
    async fn chain_runs_in_order_and_stops_at_first_reject() {
        let rt = rt();
        let log = Arc::new(Mutex::new(Vec::new()));
        let chain = HookChain::new(vec![
            rec("a", &log, Verdict::Pass),
            rec("b", &log, Verdict::Reject("b says no".into())),
            rec("c", &log, Verdict::Pass),
        ]);
        assert_eq!(chain.len(), 3);

        let v = chain.before_tool(&rt, "t", &mut json!({})).await;
        assert_eq!(v, Verdict::Reject("b says no".into()));
        assert_eq!(*log.lock().unwrap(), vec!["a", "b"], "later hooks must not be asked after a reject");
    }

    /// The `args` that `before_tool` receives are **writable**: after a hook edits them, later
    /// hooks and the loop see the edited value.
    #[tokio::test]
    async fn before_tool_edits_are_visible_to_later_hooks() {
        let rt = rt();
        let log = Arc::new(Mutex::new(Vec::new()));

        struct Fill;
        #[async_trait]
        impl Hook for Fill {
            fn name(&self) -> &str {
                "fill"
            }
            async fn before_tool(&self, _rt: &Arc<Runtime>, _n: &str, args: &mut Value) -> Verdict {
                args["filled"] = json!(true);
                Verdict::Pass
            }
        }

        struct Check(Arc<Mutex<Vec<&'static str>>>);
        #[async_trait]
        impl Hook for Check {
            fn name(&self) -> &str {
                "check"
            }
            async fn before_tool(&self, _rt: &Arc<Runtime>, _n: &str, args: &mut Value) -> Verdict {
                assert_eq!(args["filled"], json!(true), "a later hook must see the earlier hook's edit");
                self.0.lock().unwrap().push("check");
                Verdict::Pass
            }
        }

        let chain = HookChain::new(vec![Arc::new(Fill), Arc::new(Check(log.clone()))]);
        let mut args = json!({});
        assert_eq!(chain.before_tool(&rt, "t", &mut args).await, Verdict::Pass);
        assert_eq!(args, json!({"filled": true}));
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    /// Messages injected by `before_request` are handed back to the loop as-is (persisting them is
    /// the loop's job, via `push_message`).
    #[tokio::test]
    async fn before_request_hands_injected_messages_back() {
        let rt = rt();
        struct Inject;
        #[async_trait]
        impl Hook for Inject {
            fn name(&self) -> &str {
                "inject"
            }
            async fn before_request(&self, _rt: &Arc<Runtime>, req: &mut Request) -> Verdict {
                req.inject.push(Message::user("a host addendum"));
                Verdict::Pass
            }
        }
        let chain = HookChain::new(vec![Arc::new(Inject)]);
        let mut req = Request { tools: Vec::new(), inject: Vec::new() };
        assert_eq!(chain.before_request(&rt, &mut req).await, Verdict::Pass);
        assert_eq!(req.inject.len(), 1);
        assert_eq!(req.inject[0].text(), Some("a host addendum"));
    }

    /// **Fill missing keys only**: a value the model explicitly gave is never overwritten by a
    /// default.
    #[tokio::test]
    async fn args_default_only_fills_missing_keys() {
        let rt = rt();
        let hook = ConfigHook::from_config(&cfg_with(
            "fs_read_file",
            &[("path", "config.toml"), ("encoding", "utf-8")],
        ))
        .unwrap()
        .expect("a configured hook must be built");

        // The model gave path, not encoding
        let mut args = json!({ "path": "my own path" });
        assert_eq!(hook.before_tool(&rt, "fs_read_file", &mut args).await, Verdict::Pass);
        assert_eq!(args["path"], json!("my own path"), "an explicit value must not be overridden by a default");
        assert_eq!(args["encoding"], json!("utf-8"), "the missing one gets filled in");

        // Other tools are left entirely alone
        let mut other = json!({});
        assert_eq!(hook.before_tool(&rt, "fs_write_file", &mut other).await, Verdict::Pass);
        assert_eq!(other, json!({}), "unconfigured tools must not be touched");
    }

    /// When the arguments aren't an object, pass quietly — if the intent can't be guessed, don't
    /// touch it.
    #[tokio::test]
    async fn args_default_leaves_non_object_args_alone() {
        let rt = rt();
        let hook = ConfigHook::from_config(&cfg_with("t", &[("k", "v")])).unwrap().unwrap();
        for mut args in [json!("a string"), json!([1, 2]), json!(null)] {
            let before = args.clone();
            assert_eq!(hook.before_tool(&rt, "t", &mut args).await, Verdict::Pass);
            assert_eq!(args, before);
        }
    }

    /// `[hook]` entirely empty → register no hook (the entry-point guarantee of "empty config =
    /// empty chain").
    #[test]
    fn empty_config_registers_no_hook() {
        let hooks = registry_from_config(&HookConfig::default()).unwrap();
        assert!(hooks.is_empty(), "the default config must not build a hook");
        // An empty table (a `[hook]` header written with no content) likewise builds nothing
        let empty = HookConfig { tool_args_default: HashMap::new(), ..Default::default() };
        assert!(registry_from_config(&empty).unwrap().is_empty());
    }

    /// The config → runtime chain must really connect: 0 by default, 1 once configured.
    #[test]
    fn runtime_builds_chain_from_config() {
        let mut cfg = Config::dummy();
        cfg.runtimes = RuntimesConfig {
            python: "__none__.exe".into(),
            node: "__none__.exe".into(),
            rustc: "__none__.exe".into(),
            python_url: None,
            python_sha256: None,
        };
        let root = std::env::temp_dir().join("aox-hook-test-rt");

        let off = Runtime::new(cfg.clone(), root.clone(), root.join("plugins")).unwrap();
        assert!(off.hooks.is_empty(), "default config: an empty chain");
        assert!(off.hooks.report().contains("0 registered"));

        cfg.hook = cfg_with("fs_read_file", &[("encoding", "utf-8")]);
        let on = Runtime::new(cfg, root.clone(), root.join("plugins")).unwrap();
        assert_eq!(on.hooks.len(), 1, "once configured it must be attached");
        let rep = on.hooks.report();
        assert!(rep.contains("config"), "the report must carry the hook name: {rep}");
        assert!(rep.contains("encoding"), "the report must say which key was filled: {rep}");
    }

    // ─────────────── Phase 6 · 6b: runtime appends + component contributions ───────────────

    /// Once an empty chain is `push`ed, the hook points **immediately** start asking it — this is
    /// the mechanism behind "a component's declaration takes effect immediately".
    #[tokio::test]
    async fn push_adds_a_hook_to_a_live_chain() {
        let rt = rt();
        let chain = HookChain::default();
        assert!(chain.is_empty(), "it must start as an empty chain (the premise of zero drift)");

        let log = Arc::new(Mutex::new(Vec::new()));
        chain.push(rec("late", &log, Verdict::Reject("no".into())));
        assert_eq!(chain.len(), 1);
        assert!(!chain.is_empty());

        let v = chain.before_tool(&rt, "t", &mut json!({})).await;
        assert_eq!(v, Verdict::Reject("no".into()));
        assert_eq!(*log.lock().unwrap(), vec!["late"], "a pushed hook must actually be called");
    }

    /// Response mapping: only an **explicit** `"reject"` counts as a rejection; everything else
    /// (misspelled, missing field, wrong type) counts as passing — this is the execution path, and
    /// it's better to miss one interception than let one bad component kill the whole round.
    #[test]
    fn verdict_only_rejects_on_an_explicit_word() {
        assert_eq!(verdict_of(&json!({ "verdict": "pass" })), Verdict::Pass);
        assert_eq!(verdict_of(&json!({})), Verdict::Pass);
        assert_eq!(verdict_of(&json!(null)), Verdict::Pass);
        assert_eq!(verdict_of(&json!({ "verdict": "REJECT" })), Verdict::Pass, "case is not recognised");
        assert_eq!(verdict_of(&json!({ "verdict": 1 })), Verdict::Pass, "a wrong type is not recognised");
        assert_eq!(
            verdict_of(&json!({ "verdict": "reject", "reason": "   " })),
            Verdict::Reject("the component asked to block this step but gave no reason".into()),
            "an empty reason must still produce a presentable sentence"
        );
        assert_eq!(
            verdict_of(&json!({ "verdict": "reject", "reason": "out of bounds" })),
            Verdict::Reject("out of bounds".into())
        );
    }

    /// Component absent (not installed / uninstalled / failed to load) → pass + book a failure,
    /// with not one argument touched. This is the minimal evidence for "components must be
    /// degradable".
    #[tokio::test]
    async fn component_hook_degrades_when_the_plugin_is_gone() {
        let rt = rt(); // there is not a single plugin in this runtime
        let failures = Arc::new(AtomicU64::new(0));
        let last = Arc::new(std::sync::Mutex::new(None));
        let hook = ComponentHook::new(
            "ghost".into(),
            vec!["before_tool".into()],
            Vec::new(),
            failures.clone(),
            last.clone(),
        );

        let mut args = json!({ "a": 1 });
        assert_eq!(hook.before_tool(&rt, "t", &mut args).await, Verdict::Pass);
        assert_eq!(args, json!({ "a": 1 }), "arguments must not be changed when the component is absent");
        assert_eq!(failures.load(Ordering::Relaxed), 1, "a failure must be booked");
        assert!(
            last.lock().unwrap().as_deref().unwrap_or("").contains("ghost"),
            "the ledger must say which component: {:?}",
            last.lock().unwrap()
        );
        // The failure tally must be visible from the report (otherwise it's "silently vanishing",
        // not "silently degrading")
        assert!(hook.describe().contains("failed 1 time(s)"), "the failure must be visible in the report: {}", hook.describe());
    }

    /// An undeclared point: the host must not send even one RPC, let alone book a failure.
    #[tokio::test]
    async fn undeclared_points_are_never_asked() {
        let rt = rt();
        let failures = Arc::new(AtomicU64::new(0));
        let hook = ComponentHook::new(
            "ghost".into(),
            vec!["before_tool".into()],
            Vec::new(),
            failures.clone(),
            Arc::new(std::sync::Mutex::new(None)),
        );

        let e = anyhow::anyhow!("boom");
        assert_eq!(hook.on_error(&rt, &e).await, ErrorAction::Propagate);

        let mut reply = ChatReply {
            message: Message::assistant(Some("hi".into()), None, Vec::new()),
            finish_reason: Some("stop".into()),
            usage: Default::default(),
        };
        assert_eq!(hook.after_reply(&rt, &mut reply).await, Verdict::Pass);
        assert_eq!(reply.message.text(), Some("hi"), "an undeclared point must not edit the reply");

        assert_eq!(failures.load(Ordering::Relaxed), 0, "an undeclared point must not book a failure");
    }
}
