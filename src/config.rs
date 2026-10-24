use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Layered composition manifest (stage 5): turn "read one config" into "compose a layer stack".
///
/// What comes out is still an ordinary TOML tree — not a single field of the `Config` shape in
/// this file changed, so "the config model" and "the config source" stay two independent things
/// (design principle P1).
pub mod layer;

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
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub prompt: PromptConfig,
    #[serde(default)]
    pub hook: HookConfig,
    /// The `[retry]` section of `config.toml` (what to do when a model request fails, see
    /// [`RetryConfig`] and `src/llm/failure.rs`).
    ///
    /// It lives next to `[hook]` on purpose: until 2026-10-23 the retry budget was
    /// `[hook] max_llm_retries`, a key that only existed because a *hook* was the first thing able
    /// to ask for a retry. The budget never belonged to the hook seam — it belongs to the loop — and
    /// it now travels with the delay curve it has to agree with.
    #[serde(default)]
    pub retry: RetryConfig,
    /// The `[loop]` section of `config.toml` (the loop seam's second form, see `src/agent_loop.rs`).
    ///
    /// The Rust field name carries an underscore because `loop` is a keyword — **the TOML key is
    /// still `loop`** (matched via `rename`), so what you see in the config file is `[loop]`.
    #[serde(default, rename = "loop")]
    pub loop_config: LoopConfig,
    /// The `[sandbox]` section of `config.toml` (the plugin child-process fence, see `src/sandbox.rs`).
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// The `[shell]` section of `config.toml` (the built-in command line, see `src/tool/shell.rs`).
    ///
    /// A fifth, again distinct, "how much": `[sandbox]` limits how a plugin process runs,
    /// `[workplace]` limits which files may be touched, `[plugins]` limits how long a process lives,
    /// `[tools]` limits what the model is told exists, and **this one grants a capability the model
    /// did not have before** — a command line, confined to the workspace by the host's command check.
    #[serde(default)]
    pub shell: ShellConfig,
    /// The `[plugins]` section of `config.toml` (lazy loading of plugin subprocesses, see
    /// `src/plugin/mod.rs`).
    ///
    /// It governs a third thing, again distinct from both of the above: `[sandbox]` limits how a
    /// plugin process runs, `[workplace]` limits which files may be touched, and **this one limits
    /// how long a plugin process exists at all**.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// The `[tools]` section of `config.toml` (which tool schemas are advertised, see
    /// [`ToolsConfig`]).
    ///
    /// A fourth, again distinct, "how much": `[sandbox]` limits how a plugin process runs,
    /// `[workplace]` limits which files may be touched, `[plugins]` limits how long a process lives,
    /// and **this one limits what the model is told exists**.
    #[serde(default)]
    pub tools: ToolsConfig,
    /// The `[workplace]` section of `config.toml` (the model's file boundary, see `src/workplace.rs`).
    ///
    /// Two layers apart from `[sandbox]`: this one governs "which files may be touched", not
    /// "how child processes run".
    ///
    /// ⚠️ **The TOML section name is `[workplace]`; the old name `[workspace]` is still accepted**
    /// (`alias`). Naming was unified once on 2026-10-04: **the word is always `workplace`** — the
    /// directory name `<app root>/workplace/`, the CLI command `/workplace`, the config section
    /// `[workplace]`; the second pass also renamed the Rust-side module and types
    /// (`src/workplace.rs` / `Workplace` / `WorkplaceConfig`), so there is no second spelling left.
    ///
    /// **Do not delete that `alias`**: without it, an already-written `config.toml` **silently**
    /// falls back to `full` (serde ignores sections it doesn't recognize without erroring) — exactly
    /// the "written but not in effect" class of problem that is hardest to track down.
    #[serde(default, rename = "workplace", alias = "workspace")]
    pub workplace: WorkplaceConfig,
    /// GUI-only appearance config. The CLI bin target compiles this file too, but cannot read this field.
    #[allow(dead_code)]
    #[serde(default)]
    pub theme: ThemeConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    /// Cap on the reply's tokens. Omitted ⇒ [`DEFAULT_MAX_TOKENS`].
    ///
    /// It is also the amount **reserved** inside the context budget (`compact::Policy`), so the reply
    /// does not get cut off by the input having filled the window: raising it lowers the input budget
    /// by the same number.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Whether to request via SSE streaming (default true).
    /// With it on, a thinking model's reasoning can be echoed in real time; if some endpoint does
    /// not accept stream / stream_options, set it false to fall back to waiting for the full response.
    #[serde(default = "default_true")]
    pub stream: bool,
    /// The model's context window, in tokens. Omitted ⇒ [`DEFAULT_CONTEXT_WINDOW`].
    ///
    /// **Two readers, and the second one is not cosmetic** (this doc said otherwise until
    /// 2026-10-23, and the stale half was the important one):
    ///
    /// 1. the GUI's context ring — "how full is the window" needs to know what full means;
    /// 2. **`compact::Policy`**, which derives the compaction budget as
    ///    `context_window × compact_ratio − max_tokens`. Set it too high and compaction never
    ///    triggers, so an over-long conversation goes to the endpoint and comes back as a 400; set it
    ///    too low and context that would still fit gets folded away.
    ///
    /// So this must be the **model's real window**, not an aspiration. `[agent]
    /// context_budget_tokens` overrides the derivation outright if a single number is easier to
    /// reason about.
    #[serde(default = "default_context_window")]
    pub context_window: Option<u32>,
}

/// Default reply-token cap: generous enough that a normal answer, a diff or a file write fits in one
/// call, far below what any current endpoint considers "too many".
pub const DEFAULT_MAX_TOKENS: u32 = 32_000;

/// Default context window assumed when the config says nothing.
///
/// ⚠️ It feeds the compaction budget (see [`ProviderConfig::context_window`]), so it has to match the
/// model actually in use. When a model is smaller than this, **set the real number**: an oversized
/// assumption is not "extra headroom", it is "compaction never runs".
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
            crate::i18n::t("no API key configured: set api_key or api_key_env under [provider] in config.toml")
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
// The whole table is optional: when `[auth]` only sets `meta_tools`, `plugin_tools` takes its
// code default. This matches the long-standing behavior of `[prompt]` / `[history]` / `[store]` /
// `[theme.metrics]`. Stage-5 layered composition can produce "a slice of one table" (a single
// layer changing just one key); requiring the whole table would make that layer fail to parse, and
// the error (missing field) would point at the composed result, leaving the user completely lost.
#[serde(default)]
pub struct AuthConfig {
    /// One of: ask | auto | deny
    pub meta_tools: String,
    /// ask | auto | deny
    pub plugin_tools: String,
    /// ask | auto | deny — the **shell tools** (`shell`, and `bash` once it lands) only.
    ///
    /// Its own key rather than riding `plugin_tools`, because it is a different kind of capability:
    /// a plugin tool does one designed job, whereas a command line runs arbitrary programs (and can
    /// read every file the user can read). Default `ask`, which after one "allow for this session"
    /// stops nagging — see the authorisation policy in `src/auth.rs`.
    pub shell: String,
    #[serde(default)]
    pub overrides: HashMap<String, String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            meta_tools: "ask".into(),
            plugin_tools: "auto".into(),
            shell: "ask".into(),
            overrides: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
// Same as above: `[agent]` is written in parts by two bundles (tools writes the timeout and
// truncation, prompt-core writes the rules file); requiring the whole table would make that
// composition fail outright.
#[serde(default)]
pub struct AgentConfig {
    pub rules_file: Option<String>,
    pub tool_timeout_secs: u64,
    pub max_result_chars: usize,
    /// The **input** token budget for one request; `> 0` turns compaction on and overrides everything
    /// else. Use it when the window is known to be smaller than `context_window` says (or when only
    /// part of the window is usable).
    ///
    /// 0 = "derive it from `[provider] context_window × compact_ratio`". Since 2026-10-23 both of
    /// those have defaults (600k / 0.8), so **0 means "compact at ~448k input tokens"**, not "off".
    /// Set this explicitly when a single number is easier to reason about than the product.
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

/// The `[tools]` section: **how much of the tool registry is advertised to the model**.
///
/// Two different questions live here, and they are deliberately kept apart instead of being merged
/// into one "on/off":
///
/// - `inline_max` answers "is the list still small enough to send whole?" A registry at or under it
///   is sent **exactly as before** — every tool, every round, byte-identical request. That is the
///   zero-drift default: a light install sees no change at all.
/// - `enabled` answers "which tools must be visible no matter what", for the case where the user runs
///   a plugin they always want at hand.
///
/// The line for "may be held back" is **not a name list** but the tool's own
/// [`crate::tool::ToolKind`]: the host's own tools (`Meta` / `Host`) are always advertised, and only
/// `Plugin(_)` tools can be hidden. A model-built plugin therefore can never displace the host's own
/// vocabulary — see [`crate::tool::scope`] for the rule and the reasoning.
///
/// ⚠️ Hiding is **advertising only**, not execution: `Runtime::execute` (and `Agent`'s
/// `authorize_and_execute`) resolve a name against the registry, not against this list, so a tool
/// the model remembers but cannot see still runs. Hidden ≠ unavailable.
#[derive(Debug, Clone, Deserialize)]
// Same as above: a section that sets only one key leaves the other at its default.
#[serde(default)]
pub struct ToolsConfig {
    /// Registry size at or below which the whole list is sent (old behaviour). `0` = never hide
    /// plugin tools, ever — the permanent escape hatch.
    ///
    /// The default (24) is picked so that the 16 host tools plus a handful of plugins stay fully
    /// inline: discovery costs a round trip, and paying it to save a few hundred tokens is a bad
    /// trade. It starts paying when a registry holds dozens of tools.
    pub inline_max: usize,
    /// Tools that stay visible regardless of `inline_max` (plugin tools included). Names must match
    /// the registry exactly; unknown names are ignored, so deleting a plugin does not leave a broken
    /// config behind.
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
    /// When the value is a **bare command** (`python` / `node` / `rustc`), the host tries in order:
    /// **`PATH`** → "runtime bundled in the app directory `<app_root>/runtime/<lang>/<exe>`";
    /// when it is a **full path** it is used directly, with no substitution.
    ///
    /// `PATH` comes first because a complete interpreter (with pip, venv, headers and the full stdlib) is
    /// better than the embeddable runtime this project can ship — see [`prefer`](RuntimesConfig::prefer)
    /// for the way out. A `PATH` candidate is only accepted when it is a real file: the Windows "app
    /// execution alias" (`%LOCALAPPDATA%\Microsoft\WindowsApps\python.exe`, 0 bytes, a reparse point
    /// that opens the Microsoft Store) is skipped.
    pub python: String,
    pub node: String,
    pub rustc: String,
    /// Which side wins when both exist: `system` (default) or `builtin`.
    ///
    /// `system` = `PATH` first, bundled runtime as the fallback. `builtin` = ignore `PATH` for bare
    /// commands and use `<app_root>/runtime/<lang>/` whenever it is installed — the escape hatch for
    /// machines whose `PATH` python is broken in a way that is not detectable by looking at the file.
    /// An explicit path in `python` / `node` / `rustc` always wins over both.
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

/// The **storage backend** for conversation history (the storage seam extracted in stage 3, see `src/store.rs`).
///
/// This cycle has only one implementation, `"jsonl"`: one file per conversation, under
/// `<app root>/history/`. **An unrecognized value errors out at startup**, rather than silently
/// falling back to jsonl — a mistyped config that still starts shows up as "I clearly set another
/// backend, why is it still writing jsonl?", and that class of problem is the hardest to track down.
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
///
/// The segments themselves and the assembly logic live in `src/prompt.rs`; this is just the user's
/// override in `config.toml`. All fields empty by default = follow the built-in order exactly,
/// **byte-identical** to the historical hard-coded prompt.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PromptConfig {
    /// Explicit order: the listed segment ids are sorted to the front in this order, and the
    /// unlisted ones keep their default order after them.
    ///
    /// So "move only env to the front" needs just `order = ["env"]`, no need to list all four.
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
///
/// **This whole section empty = not a single hook registered**, and `run_loop` takes exactly the
/// same path as before the Loop seam.
///
/// Note the division of labour: this governs "whether to change things **after releasing**", not
/// "whether to release at all" — blocking a tool is an authorization concern and belongs to
/// `[auth]` (whose override keys support wildcards like `fs_*`).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HookConfig {
    /// Auto-fill arguments for the given tools, written like:
    ///
    /// ```toml
    /// [hook.tool_args_default.fs_read_file]
    /// encoding = "utf-8"
    /// ```
    ///
    /// **Only fills keys the model did not give**; values the model passed explicitly are never
    /// touched — overriding the model's explicit intent creates the hardest-to-track failure mode
    /// of "the model said A, the tool received B".
    #[serde(default)]
    pub tool_args_default: HashMap<String, HashMap<String, toml::Value>>,
}

/// The `[retry]` section: how many times a failed **model request** is sent again, and how long to
/// wait between attempts.
///
/// Scope is exactly "the request that never produced a reply". Tool calls are not retried here and
/// never will be — a tool can have side effects, so the host may report a failure but must never
/// run it a second time on its own. Requests, by contrast, are safe to repeat: `agent.messages()` is
/// only ever mutated by a *successful* reply, so a retry sends a byte-identical body.
///
/// **Defaults are on** (`attempts = 2`): a rate limit or a dropped stream used to throw away the
/// whole turn, which with a 600k window is the most expensive failure the program has. The cost of
/// being wrong in the other direction is one second of waiting before the same error appears.
#[derive(Debug, Clone, Deserialize)]
pub struct RetryConfig {
    /// Extra attempts **after** the first one (`0` = never retry). Total requests ≤ `attempts + 1`.
    ///
    /// Only failures that could plausibly succeed on the next try are retried at all — `429`, `5xx`,
    /// connection errors, a stream that stopped mid-answer. A `401`, a bad model name or a context
    /// that is too long fails on the first attempt (see `llm::failure::retryable`).
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
///
/// The stage-4 hooks could only insert five points **inside the loop**, the skeleton was still
/// welded shut; this section decides **which loop skeleton to use**, and along the way provides the
/// "context trimming" capability that only the loop layer can offer (it has to touch both memory and
/// the conversation file, and rebuild the request after trimming).
///
/// Default (whole section unwritten) = `standard` + no trimming, **byte-identical** to stage 5.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoopConfig {
    /// Which loop implementation to use. **This cycle only recognizes `standard`** — any other value
    /// **errors out at startup** in `Runtime::new`, no silent fallback (the same principle as
    /// `[store] backend`: a setting written but never taking effect is the hardest class of bug).
    #[serde(rename = "impl")]
    pub implementor: String,
    /// How many "user turns" to keep at most in the context; `0` (default) = no trimming.
    ///
    /// A turn is delimited by one `role == "user"` message (tips injected by the host hook are also
    /// user messages, so each occupies a turn too). Trimming always drops from the **oldest whole
    /// turn**, and always keeps the system-prompt message — so the trimmed context is itself a legal
    /// request body, and never leaves the half-message "a tool result remains while its assistant was
    /// already dropped", which the endpoint would reject outright.
    pub max_rounds: usize,
    /// How many model requests **one turn** may make before the host stops and hands control back.
    /// `0` (default) = no limit, exactly as before this existed.
    ///
    /// A turn makes one request per round (a round = one model reply + the tools it asked for), so
    /// this is the only hard ceiling on what a confused model can spend in one go: without it, a
    /// model that keeps calling tools can run until the user notices and interrupts. The count is
    /// per turn and resets with every user message.
    ///
    /// When it fires, the turn ends **normally** (the checkpoint is persisted, the front end closes
    /// the card) and the user is told which limit fired — "the task died silently" is the failure
    /// this is trying to prevent, so it must not create that impression.
    ///
    /// Not the same thing as `max_rounds`: that one trims the **context** (how much history is kept),
    /// this one caps the **work** (how many requests a single instruction may cost). A turn can hit
    /// this ceiling at 50 with a perfectly small context, and can run 200 rounds with
    /// `max_rounds = 3`.
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

/// The plugin child-process **fence** (Job Object, see `src/sandbox.rs`).
///
/// **On** by default, and the default values are exactly the set that has "zero impact on legitimate
/// plugins": allow a plugin to open 8 more child processes, kill everything when it closes, isolate
/// the USER object. Together they remove the fork bomb, orphan processes, and a plugin reaching for
/// the host's windows/clipboard.
///
/// ⚠️ The defaults of this section are **not** "zero drift" — it is a deliberate hardening, and
/// `enabled = false` reverts it. Note however that it **cannot govern files and the network**: those
/// two need AppContainer (see the options comparison in docs).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    /// Master switch. Once off, no Job is built and no `CREATE_SUSPENDED` is added, byte-identical to before the fence.
    pub enabled: bool,
    /// How many child processes **each plugin may open at most** (**not counting the plugin itself**).
    ///
    /// `0` = forbid the plugin from opening child processes (the only tier that locks this path shut); default `8`.
    ///
    /// Why the default is no longer `1`: that number used to be "the total process count **including
    /// the plugin itself**", written straight into the Job, so a default of 1 meant "only 1 process
    /// allowed in the Job" — the plugin itself filled it, and any plugin that does work via a child
    /// process (calling `curl` / `git` / an interpreter) got `WinError 1816` (quota exceeded), showing
    /// up as "the plugin inexplicably cannot do its job". The platform's "including self" convention
    /// now lives in one place, [`SandboxConfig::job_process_limit`], and this config layer speaks
    /// only of what the user cares about.
    ///
    /// Why keep a **finite small allowance** (rather than unlimited): a fork bomb relies on concurrent
    /// doubling, and capping concurrency stops it (child processes also inherit this Job automatically,
    /// so they cannot escape the fence). To loosen it, write a large number, **not 0** — a 0 here means
    /// "not a single one allowed", the opposite direction from `max_memory_mb = 0` (unlimited): when
    /// asking "how many are allowed", 0 means 0, whereas that memory 0 is just shorthand for "no cap".
    ///
    /// Note this is an allowance **per plugin** (one Job per plugin), not a global budget — otherwise
    /// one plugin could eat the whole allowance and the others could not even start.
    pub max_processes: u32,
    /// Commit-memory cap for each plugin process, in MB. `0` (default) = unlimited.
    ///
    /// ⚠️ Why off by default: exceeding it **kills the whole process in the kernel**, rather than
    /// being one failed allocation — a tight cap would simply kill a "legitimate but memory-hungry"
    /// plugin, with no visible reason from the plugin side, showing up as "the plugin keeps
    /// inexplicably restarting". Turn it on deliberately if you want it.
    pub max_memory_mb: usize,
    /// Forbid the plugin from using the host process's USER object (windows / clipboard / desktop / global atoms).
    /// A plugin process doesn't need these anyway — all its communication goes through the stdin/stdout pipes.
    pub ui_limits: bool,
}

impl SandboxConfig {
    /// The `ActiveProcessLimit` actually written into the Job.
    ///
    /// The `+1` is **platform semantics**, not config semantics: `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`
    /// counts processes in the **whole Job**, and the plugin itself takes one slot. Keeping this layer
    /// here (the one place) is what makes [`max_processes`](SandboxConfig::max_processes) in the config
    /// really mean "how many more child processes the plugin may open" — otherwise the user would have
    /// to +1 in their head, and the consequence of missing that step is exactly "the plugin cannot do
    /// its job under the default config".
    ///
    /// `saturating_add`: a config of `u32::MAX` will not wrap to 0 (0 in a Job means "unlimited", which
    /// is a different meaning, and must not be hit by one careless extreme value).
    pub fn job_process_limit(&self) -> u32 {
        self.max_processes.saturating_add(1)
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self { enabled: true, max_processes: 8, max_memory_mb: 0, ui_limits: true }
    }
}

/// The built-in **shell tools** (`config.toml`'s `[shell]`, see `src/tool/shell.rs`).
///
/// This section governs a capability none of the others do: `[sandbox]` limits how a *plugin*
/// process runs, `[workplace]` says which files may be touched, and **this one hands the model a
/// command line** — together with the one boundary that makes doing so defensible.
///
/// # The boundary, in one paragraph
///
/// The command runs at **Low integrity** (Mandatory Integrity Control, `src/shell/lowil.rs`) and the
/// workspace is labelled Low, so `no-write-up` turns "write outside the workspace" into a **kernel
/// refusal** instead of a convention the model has to respect. Reads are deliberately untouched —
/// the same read/write asymmetry as [`crate::workplace::Access`]. The command scan in
/// `tool::shell` is a **guard rail** (it produces a readable refusal before a command half-runs),
/// never the boundary itself.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    /// Master switch. `false` = the tools are never registered, so the model never sees them.
    pub enabled: bool,
    /// Which command line the model should reach for **first**: `bash` (default) or `cmd`.
    ///
    /// This is an **instruction, not a fallback**. The host can never "try bash, then cmd": a command
    /// has side effects, so running one twice to find out which shell accepts it would be a disaster
    /// (`del`, `git commit`, `cargo build`). What the key changes is the wording the model reads —
    /// [`crate::prompt`] orders its paragraph around it, and `/shell` plus the settings row report it.
    ///
    /// `cmd` needs no new tool name: it is already `shell`'s default dialect
    /// (`Dialect::default_for_platform`), so `prefer = "cmd"` is honest about a door that already
    /// exists. Measured on Windows 11 26200: `cmd /D /S /C exit` ≈ 87 ms, `git-bash -c exit` ≈ 270 ms,
    /// busybox `sh -c exit` ≈ 28 ms. `bash` stays the default anyway: a shell with `$(…)`, `&&` and
    /// sed/grep/find/awk is worth the ~180 ms on anything longer than a one-word command, and a wrong
    /// guess costs a whole model round trip — far more than 180 ms.
    #[serde(default = "default_shell_prefer")]
    pub prefer: String,
    // NOTE: there is deliberately **no** `boundary` switch. A kernel-enforced boundary (low-integrity
    // child + labelled workspace) was implemented three ways and measured not to hold on Windows 11
    // 26200 — see `docs/SHELL.md`. The only layer is the host's command scan, and pretending otherwise
    // through a config key would be the worst kind of option: one that reads as protection.
    /// Where the `bash` tool's shell comes from: `auto` (default) | `off` | an absolute path.
    ///
    /// `auto` looks, in order, at `<app root>\runtime\bash\` (see `bash_pack`), the usual Git for
    /// Windows install locations, and finally `bash.exe` on PATH — skipping anything under
    /// `\Microsoft\WindowsApps\`, which is the WSL launcher and a different file system. Detection only
    /// stats files; a candidate is validated with `uname -s` on the first call.
    pub bash: String,
    /// Whether `<app root>\runtime\bash\bash.exe` (or `busybox.exe`) is considered by `auto`.
    ///
    /// A sidecar rather than something embedded in the exe: the binary stays small, and the pack can be
    /// shipped or skipped independently. `off` ignores it.
    pub bash_pack: String,
    /// Default per-call timeout, in seconds. A call may ask for more via its own `timeout_secs`.
    ///
    /// 60 rather than the old 120 (user decision, 2026-10-23): a normal command finishes in
    /// milliseconds, a build in a couple of minutes, and anything already running for a minute with
    /// no result is far more often wedged than working. A call that really needs longer says so —
    /// `timeout_secs` is right there in the schema, and running out of budget is a *readable*
    /// outcome (`TIMED OUT … the whole tree was terminated`), not a silent hang.
    pub timeout_secs: u64,
    /// Hard ceiling for `timeout_secs`. A model that asks for an hour waits an hour otherwise,
    /// while holding one tool slot the whole time.
    pub max_timeout_secs: u64,
    /// Kill a call that has written **nothing** to either stream for this many seconds.
    ///
    /// This is the answer to "it has been running for eight minutes, is it working or wedged?". A
    /// total timeout cannot tell the two apart — a build legitimately runs for minutes — but the
    /// *stream* can: a build prints constantly, a wedged process prints nothing. Measured from the
    /// last byte that arrived, not from the start, so `cargo build` is never killed for being slow.
    ///
    /// `0` = off (only `timeout_secs` applies). A call may override it with its own `idle_secs`.
    pub idle_timeout_secs: u64,
    /// How much of a command's output to keep (stdout and stderr counted separately), in bytes.
    ///
    /// Beyond it the **head and the tail** are kept and the elided middle is reported with its
    /// size: build logs put the failure at the end and often the reason at the start, so keeping one
    /// end only would throw away half the answer.
    pub max_output_bytes: usize,
    /// Process allowance for **one call** (a Job Object per call, the same mechanism as plugins).
    ///
    /// This one is a correctness need rather than hardening: a timeout has to kill the whole tree
    /// (`cargo` → `rustc` × N), so the fence is created regardless of `[sandbox] enabled`.
    /// `0` = the command may not spawn anything at all.
    pub max_processes: u32,
    /// How many **long-lived** `shell_job` trees may exist at once. `0` disables the tool.
    ///
    /// Separate from `max_processes`: that one is the process allowance *inside one Job*, this one
    /// is how many Jobs are allowed to stay alive across calls. A dev server, a watcher and a
    /// preview process all have legitimate reasons to coexist, but an unbounded set of them is just
    /// the orphan-process problem with better bookkeeping.
    pub background_max_jobs: usize,
    /// Process allowance **inside one background Job**.
    ///
    /// Browser-driven checks are the reason this is larger than [`Self::max_processes`]: a single
    /// Chromium launch can occupy a dozen processes before the site has even rendered. `0` means
    /// the helper may not start a command at all and is rejected by the tool.
    pub background_max_processes: u32,
    /// Maximum lifetime of one background job, in hours. `0` = no expiry (stop explicitly instead).
    ///
    /// Expiry is checked when `shell_job` is used and before every agent request; a job cannot slip
    /// past it just because the model stopped asking. The hard guarantee remains host exit: the
    /// registry owns every fence, so closing it kills all trees at once.
    pub background_max_hours: u64,
    /// Point build caches at the workspace (`CARGO_HOME`, `NPM_CONFIG_CACHE`, `PIP_CACHE_DIR`,
    /// `TMP`/`TEMP`) instead of the user profile.
    ///
    /// Not a nicety: a Low-integrity process **cannot write** `%USERPROFILE%\.cargo` (Medium), so
    /// with this off `cargo build` inside the workspace fails with "access denied" on its own cache
    /// — a failure that looks like the boundary being broken. The price is one cold cache per
    /// workspace.
    pub env_cache: bool,
    /// Report which workspace files changed during the call (a bounded mtime scan afterwards).
    pub track_changes: bool,
}

/// `prefer` defaults to bash; see [`ShellConfig::prefer`].
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

/// **Lazy loading of plugin subprocesses** (`config.toml`'s `[plugins]`, see `src/plugin/mod.rs`).
///
/// # What problem it solves
///
/// A plugin is one interpreter process. Under the old behaviour **every** plugin under `plugins/`
/// was started at boot and stayed resident for as long as the program lived — a shelf of idle
/// `python.exe` processes (roughly 10-15 MB each) that nobody was calling, which is exactly the
/// "checked the task manager and found a row of mysterious processes" complaint.
///
/// # How
///
/// A plugin slot has only two states: **dormant** (no process; only metadata in memory, a few
/// hundred bytes) and **active** (child process + Job fence). Startup reads the manifest plus
/// `plugins/<name>/plugin.cache.json` and registers the plugin's tools, hooks and prompt segments
/// **without starting anything**; the process is started by the first real use (`tools/call` or a
/// hook), and the reaper takes it away again once it has been idle past `idle_timeout_secs`.
///
/// Dormant is not "disabled": the plugin's tools are still in the model's tool list and still
/// callable — they just cost a process start (tens of milliseconds) the first time.
///
/// ⚠️ The one thing worth knowing: the tool list comes from a cache, so a plugin whose source was
/// edited since its last run may briefly advertise a stale tool table. It self-heals — every real
/// start re-reads the tools, re-registers them if they differ, and rewrites the cache — so the
/// worst case is one wasted call. To force a rebuild by hand, delete that plugin's
/// `plugin.cache.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// Master switch. `true` (default) = metadata-only at boot, start on demand, reap when idle.
    ///
    /// `false` restores the previous behaviour **item by item**: every plugin is started at boot
    /// and stays resident until the program exits. This is the escape hatch, the same idea as
    /// `[sandbox] enabled = false` — anything that turns out to misbehave under lazy loading can be
    /// taken back to the old model without downgrading the program.
    pub lazy: bool,
    /// How many seconds a plugin process may sit unused before it is shut down.
    ///
    /// `0` = never reap (processes persist once started, i.e. lazy *start* but no lazy *stop*).
    ///
    /// Why there is a threshold at all rather than "close it the moment the call returns": every
    /// start costs a process spawn (tens of milliseconds for `python.exe`), and a task usually calls
    /// the same plugin several times in a row. A minute of grace is invisible to the user while
    /// still leaving the machine clean by the time they look at the task manager.
    pub idle_timeout_secs: u64,
    /// Upper bound on **simultaneously live** plugin processes. `0` (default) = unlimited.
    ///
    /// Over the cap, the longest-idle process is closed first to make room. Leave it at 0 unless
    /// memory is genuinely tight: a turn that alternates between a hook component and a tool plugin
    /// can otherwise be forced to start a process on every single round, which is slower than just
    /// keeping both alive for the idle window.
    pub max_resident: usize,
    /// Plugin names that **always stay resident** even when idle.
    ///
    /// For components that fire on every turn (`[contributes] hooks`): they count as "in use" at the
    /// start of every request, so under lazy loading they would be started, reaped and started again
    /// on each round. Put those names here and they are started at boot like before — the right
    /// shape for "always on, always cheap".
    pub eager: Vec<String>,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self { lazy: true, idle_timeout_secs: 60, max_resident: 0, eager: Vec::new() }
    }
}

/// The workspace boundary (the config surface of sandbox option ③ "take the file capability back to
/// the host", see `src/workplace.rs`).
///
/// It and `[sandbox]` are **two different things**, don't conflate them:
/// - `[sandbox]` governs whether a **plugin child process** can open child processes / eat memory /
///   touch host windows (a process fence);
/// - `[workplace]` governs **which files the model may touch this time** (the path boundary for writes
///   and deletes; reads are unrestricted).
///
/// Default `mode = "default"` (the workspace **is the app root**; `temp/` for scratch, `workplace/`
/// for durable deliverables). **Changed from `"full"` to this on 2026-10-05** — honoring the user's
/// original "**confined by default** on every start"; the per-conversation `<timestamp>/` plot was
/// dropped on 2026-10-08 in favour of a single app-root workspace.
///
/// ⚠️ The cost: "no `[workplace]` config = byte-identical to before this layer" **no longer holds**.
/// Writing nothing now means the confined tier (writes and deletes may only land **under the app
/// root**, reads remain unrestricted); to be unrestricted you must explicitly write
/// `mode = "full"`, or switch to "full access" in the UI (that is a runtime choice, valid only for the
/// current conversation).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorkplaceConfig {
    /// `"default"` (**default**: confined to the app root — the workspace itself) /
    /// `"full"` (unrestricted) / `"fixed"` (with `root` naming a fixed directory).
    ///
    /// **An unrecognized value errors out at startup**, no silent fallback to the default tier — the
    /// same principle as `[store] backend` / `[loop] impl`: a setting written but never taking effect
    /// is the hardest class of bug to track down.
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

// ─────────────────────────── Appearance (GUI theme) ───────────────────────────
//
// This section describes **data** only and does not know about egui. Two reasons:
//   ① `gui::theme::Palette` holds `egui::Color32`, and `Color32` does not implement `Deserialize`;
//   ② the core library `aox` should not depend on egui (the GUI is an optional frontend, the CLI doesn't use it).
// So this holds "plain colour data + per-field overrides", which the GUI side converts into `Color32` and layers onto the built-in palette.

/// A colour value.
///
/// Accepts these forms (usable in both `config.toml` and theme files):
/// - `"#RRGGBB"` opaque
/// - `"#RRGGBBAA"` with alpha (`AA` is **opacity**, `FF` = fully opaque)
/// - `"#RGB"` three-digit shorthand
/// - `[r, g, b]` / `[r, g, b, a]` array form
///
/// The leading `#` may be omitted.
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
                // Three-digit shorthand: each nibble duplicated (`#abc` → `#aabbcc`)
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

/// A **per-field override** of one palette. Write only the fields you want to change; the rest inherit the built-in (paper / ink).
///
/// The fields correspond one-to-one and in the same order as `gui/src/theme.rs`'s `Palette`, for easy comparison.
/// All empty = **pixel-identical** to the built-in palette.
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
    /// **The only hue in the whole palette**: dangerous actions (currently used only for "full access").
    ///
    /// The other fields deliberately express semantics with lightness alone, but "after crossing the
    /// line there is no boundary at all" deserves one exception — it is the only switch that, once
    /// clicked, bypasses every protection, and a lightness scale cannot express "this is not an
    /// ordinary option". It is still an ordinary, overridable palette field (kept in step with
    /// `gen_themes.py`'s fields): if a theme author really wants to remove this bit of red, that is
    /// their choice, and it must not be silently ignored in the merge layer.
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
            card_radius: 14,
            item_spacing: [10.0, 10.0],
            button_padding: [14.0, 9.0],
            interact_height: 30.0,
            indent: 22.0,
        }
    }
}

/// Font file paths. When empty, continue auto-searching in the order "environment variable → built-in candidate table".
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

/// GUI appearance config: `config.toml`'s `[theme]`, or a whole theme file given with `--theme <file>`.
///
/// A `--theme` file **wholly replaces** `config.toml`'s `[theme]` (not a field-by-field layering),
/// so a theme file is either complete or expected to be filled in with built-in values.
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
pub struct ThemeConfig {
    /// Initial light/dark: `"light"` / `"dark"`.
    ///
    /// **Lowest priority**: if the user manually switched theme before, that record wins (see the GUI's
    /// `.aox-gui-theme`); this value is used only if it was never switched, and if neither exists, "ink" (dark) is used.
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
    /// Parse a theme from a TOML text. **Both forms are accepted**:
    ///
    /// - **bare theme**: `mode` / `[metrics]` / `[dark]` … written directly at the top level;
    /// - **with a `[theme]` table**: exactly as written in `config.toml`.
    ///
    /// Both are accepted so it is "usable and pasteable": `themes/*.toml` uses the latter, so the whole
    /// block can be pasted as-is into `config.toml`'s `[theme]`; conversely, the `[theme]` block from
    /// `config.toml` can be lifted out whole and saved as a standalone theme file.
    fn from_toml_text(text: &str) -> Result<Self> {
        let value: toml::Value = toml::from_str(text)?;
        // Writing `mode` at the top of the file (i.e. outside `[theme]`) is very natural, and TOML puts
        // it on the document root. Pull it out first as a fallback for when the table below lacks it —
        // avoiding "I clearly wrote mode but it doesn't take effect".
        let root_mode = value.get("mode").and_then(|v| v.as_str()).map(str::to_string);
        // Use `[theme]` if present, otherwise treat the whole file as the theme
        let node = value.get("theme").cloned().unwrap_or(value);
        let mut cfg = <ThemeConfig as Deserialize>::deserialize(node)?;
        if cfg.mode.is_none() {
            cfg.mode = root_mode;
        }
        Ok(cfg)
    }

    /// Load a standalone theme file (`--theme <path>`).
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| crate::tf!("theme file not found: {}", path.display()))?;
        Self::from_toml_text(&text)
            .with_context(|| crate::tf!("theme file {} cannot be used as a theme", path.display()))
    }
}

impl Config {
    /// Read a **single** TOML into `Config` (no layered composition done to it).
    ///
    /// After stage 5, the repo's main path is `config::layer::resolve` (profile → bundle → patch →
    /// config.toml composition). This function stays as-is for callers that "have one TOML in hand and
    /// need no composition".
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
            plugins: PluginsConfig::default(),
            tools: ToolsConfig::default(),
            workplace: WorkplaceConfig::default(),
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

    /// The default ThemeConfig must be a **zero override**, and the metric defaults must equal the
    /// constants hard-coded in `gui/src/theme.rs` before stage 2. This test is the data-layer
    /// guarantee that "the default appearance is pixel-for-pixel unchanged".
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
        assert_eq!(m.card_radius, 14);
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

    /// The nested form `[hook.tool_args_default.<tool name>]` must parse into "tool → which keys to fill".
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

    /// `[retry]` — the retry budget lives here since 2026-10-23 (it used to be
    /// `[hook] max_llm_retries`, which was only ever the hook seam's address of convenience).
    ///
    /// Two things are pinned: the defaults are **on** (a rate limit or a dropped stream must not
    /// throw the turn away), and a whole table may omit fields like every other section.
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

    /// `[workplace]` is the official section name, `[workspace]` is the **old name** (unified on 2026-10-04).
    ///
    /// This is a hard requirement: without the `alias`, a config.toml already written with `[workspace]`
    /// would be **silently ignored** by serde (unrecognized sections are skipped without error) ⇒ the user
    /// thinks the boundary is still on while it actually fell back to `full` — exactly the "written but
    /// not in effect" class of problem. So both names must be pinned.
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

        // Both names written (which theoretically should not happen): better to **error** than silently
        // pick one — picking wrong shows up as the self-diagnosis-proof phenomenon "I clearly wrote full,
        // yet it still blocks me".
        let both =
            format!("{BASE}\n[workplace]\nmode = \"default\"\n[workspace]\nmode = \"full\"\n");
        assert!(toml::from_str::<Config>(&both).is_err(), "writing both names for the same section should error");
    }

    /// An old config (with no `[hook]` at all) must still parse, and the hook table must be empty —
    /// stage 4, like stage 2, must not break any existing config.toml.
    /// The factory default is the **confined tier** (`default`) — changed from `full` on 2026-10-05.
    ///
    /// This pins a **product decision**: a user who writes no `[workplace]` should start confined
    /// (writes and deletes may only land under the app root). Reverting to `full` would
    /// **silently** open everyone's write boundary — a regression in the safety direction, so a test
    /// must speak up rather than relying on review to remember.
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

    /// `[loop] max_iters_per_turn` — the work ceiling, added 2026-10-23 next to the context ceiling.
    ///
    /// Two things matter: it is **off by default** (no written config = the behaviour every earlier
    /// version had), and it is a separate key from `max_rounds` (that one caps context, this one caps
    /// requests — conflating them would either trim memory or stop turns for the wrong reason).
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

    /// The defaults of `[sandbox]`: **on**, and exactly the set with "zero impact on legitimate plugins".
    ///
    /// This is **deliberately different** from the "default = no effect" of the other sections: the fence's
    /// default value is itself behaviour, so it must be pinned even harder — should someone casually change
    /// the default, this test should go red immediately.
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

    /// `[plugins]` defaults: lazy on, 60s idle, uncapped resident count, no eager list — and an old
    /// config that has never heard of the section has to parse into exactly that. This is the
    /// "written but not in effect" guard: if the default ever silently flipped back to eager, the
    /// symptom would be a shelf of idle processes, not an error message.
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
    /// These are not arbitrary — a browser check can exceed the one-shot process allowance, and an
    /// unbounded lifetime is the orphan-process problem this feature is meant to avoid.
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

    /// The two provider defaults, checked through **serde** rather than by reading the struct: the
    /// defaults live in `#[serde(default = ...)]` attributes, so a test that sets the fields directly
    /// would prove nothing about what a config file actually gets.
    ///
    /// Pinned because both numbers have a second job beyond "a sensible value": `context_window` and
    /// `max_tokens` are the compaction budget (`window x ratio - max_tokens`). A silent change here
    /// moves the point where long conversations start being folded, and nothing would fail.
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
}
