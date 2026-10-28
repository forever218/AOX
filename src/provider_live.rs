//! The **live** model provider: one cell that the whole program reads, and that the GUI can swap.
//!
//! Before this existed, the provider was built once at startup (`OpenAiProvider::new(&config.provider)`)
//! and then moved into `Agent` as an `Arc<dyn LlmProvider>`. That was fine while the only way to change
//! the model was to edit `config.toml` and restart — which is the very friction the Model tab removes.
//! Swapping it needs two things at once, and both live here:
//!
//! * a **config snapshot** the request-building code reads (window, max_tokens, thinking, wire);
//! * a **provider handle** the loop calls.
//!
//! Two separate cells would allow "the new window with the old model"， a state nobody can debug. One
//! struct with one `apply` keeps them in step: either both are the new values, or neither is.
//!
//! Rules this type exists to enforce:
//!
//! 1. **Never leave the program without a working provider.** [`LiveProvider::apply`] builds the new
//!    one first and only then swaps; a bad key or an empty base URL leaves the previous handle in
//!    place and returns the reason.
//! 2. **A missing key is not a crash.** Construction can fail (that is how "not configured yet"
//!    looks), so the cell starts out holding an [`UnavailableProvider`] that explains itself. The CLI
//!    and the GUI both run fine in that state — local commands, history, plugins, settings — and the
//!    panel is what fixes it.
//! 3. **Reads are cheap and lock-scoped.** `llm()` clones an `Arc` under a read lock that is never
//!    held across an `await`; `cfg()` clones a small struct. No `tokio` locks: the same rule as
//!    `Runtime::runtimes`.

use std::sync::{Arc, RwLock};

use anyhow::Result;

use crate::config::{
    KeyState, ProviderConfig, Thinking, ThinkingWire, DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_TOKENS,
};
use crate::llm::{openai::OpenAiProvider, LlmProvider, UnavailableProvider};

pub struct LiveProvider {
    cfg: RwLock<ProviderConfig>,
    llm: RwLock<Arc<dyn LlmProvider>>,
    /// Why the current handle is a placeholder. `None` = the handle is real.
    ///
    /// Kept beside the handle rather than derived from its type: `dyn LlmProvider` cannot be
    /// downcast here, and "is it usable" is asked on every frame by the GUI's status line.
    error: RwLock<Option<String>>,
}

impl LiveProvider {
    /// Build from config. **Never fails** — an unusable config becomes a placeholder provider that
    /// reports [`LiveProvider::error`].
    pub fn new(cfg: ProviderConfig) -> Arc<Self> {
        let (llm, error) = build(&cfg);
        Arc::new(Self {
            cfg: RwLock::new(cfg),
            llm: RwLock::new(llm),
            error: RwLock::new(error),
        })
    }

    /// A cell that starts with a **given** handle, bypassing construction.
    ///
    /// For tests and the selftest: they need a provider that answers without a network, and the loop
    /// must still go through the same `llm()` call as in production.
    pub fn with_handle(cfg: ProviderConfig, llm: Arc<dyn LlmProvider>) -> Arc<Self> {
        Arc::new(Self {
            cfg: RwLock::new(cfg),
            llm: RwLock::new(llm),
            error: RwLock::new(None),
        })
    }

    /// Take a new configuration into force.
    ///
    /// Order matters and is the whole point: **construct, then swap.** A failure returns before either
    /// cell is touched, so the caller can show the error and carry on with the previous model instead
    /// of leaving the session unable to make a request at all.
    pub fn apply(&self, cfg: ProviderConfig) -> Result<()> {
        if let Some(reason) = missing_piece(&cfg) {
            // Not `OpenAiProvider::new`'s error: a placeholder key would *succeed* there and only fail
            // on the first request, which is the whole "everything looks configured until it isn't"
            // problem this check exists to close.
            return Err(anyhow::anyhow!(reason));
        }
        let provider = OpenAiProvider::new(&cfg)?;
        let handle: Arc<dyn LlmProvider> = Arc::new(provider);
        // Two short writes back to back; no reader can observe "new handle, old config" in a way that
        // matters, because every reader that cares takes both in one call (`info`) or takes only the
        // handle to send a request.
        if let Ok(mut w) = self.llm.write() {
            *w = handle;
        }
        if let Ok(mut w) = self.cfg.write() {
            *w = cfg;
        }
        if let Ok(mut w) = self.error.write() {
            *w = None;
        }
        Ok(())
    }

    /// The configuration the next request will use.
    pub fn cfg(&self) -> ProviderConfig {
        self.cfg.read().map(|c| c.clone()).unwrap_or_else(|e| e.into_inner().clone())
    }

    /// The handle to call. Cloning an `Arc` under a read lock is the cheapest correct answer: the
    /// caller may hold it across an `await` without pinning the lock.
    pub fn llm(&self) -> Arc<dyn LlmProvider> {
        self.llm.read().map(|l| l.clone()).unwrap_or_else(|e| e.into_inner().clone())
    }

    pub fn ready(&self) -> bool {
        self.error().is_none()
    }

    pub fn error(&self) -> Option<String> {
        self.error.read().ok().and_then(|e| e.clone())
    }

    /// Everything a front end shows about the model, in one snapshot.
    ///
    /// One call rather than five getters: the GUI redraws the "model in use" row, the context ring and
    /// the panel from this, and five separate reads could straddle a swap and mix two generations.
    pub fn info(&self) -> Info {
        let cfg = self.cfg();
        Info {
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            context_window: cfg.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW),
            max_tokens: cfg.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            temperature: cfg.temperature,
            thinking: cfg.thinking,
            // The word written in `config.toml` (`auto` stays `auto`), as opposed to `wire`, which is
            // what that word resolves to. The panel's chips edit this one: pinning the file to the
            // resolved value would silently stop `auto` from following an endpoint change.
            wire_setting: cfg.thinking_wire,
            wire: cfg.wire(),
            key_state: cfg.key_state(),
            ready: self.ready(),
            error: self.error(),
        }
    }
}

/// Why this configuration cannot answer a request yet, or `None` when it can.
///
/// Separate from "can `OpenAiProvider` be constructed": the shipped template carries the literal
/// placeholder `sk-...`, which constructs fine and then 401s on the first message. Treating that as
/// "configured" is what makes a fresh install look ready while nothing works, so the placeholder test
/// belongs here, in front of the constructor.
fn missing_piece(cfg: &ProviderConfig) -> Option<String> {
    if cfg.base_url.trim().is_empty() {
        return Some(crate::i18n::t("no endpoint configured: fill in base_url in Settings → Model").into());
    }
    match cfg.key_state() {
        KeyState::Inline | KeyState::Env(_) => None,
        KeyState::EnvUnset(name) => Some(crate::tf!(
            "the environment variable {} is named in config.toml but is not set",
            name
        )),
        KeyState::Missing => Some(
            crate::i18n::t("no API key yet: fill it in under Settings → Model (or point api_key_env at an environment variable)").into(),
        ),
    }
}

/// Build a handle plus the reason it is only a placeholder.
fn build(cfg: &ProviderConfig) -> (Arc<dyn LlmProvider>, Option<String>) {
    if let Some(reason) = missing_piece(cfg) {
        return (Arc::new(UnavailableProvider { reason: reason.clone() }), Some(reason));
    }
    match OpenAiProvider::new(cfg) {
        Ok(p) => (Arc::new(p), None),
        Err(e) => (
            Arc::new(UnavailableProvider { reason: format!("{e:#}") }),
            Some(format!("{e:#}")),
        ),
    }
}

/// A read-only snapshot of the model in force (see [`LiveProvider::info`]).
#[derive(Debug, Clone)]
pub struct Info {
    pub base_url: String,
    pub model: String,
    pub context_window: u32,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub thinking: Thinking,
    /// As written (may be `Auto`).
    pub wire_setting: ThinkingWire,
    /// As resolved from the endpoint (never `Auto`).
    pub wire: ThinkingWire,
    pub key_state: KeyState,
    pub ready: bool,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(model: &str, key: Option<&str>, window: u32) -> ProviderConfig {
        ProviderConfig {
            base_url: "https://api.deepseek.com".into(),
            model: model.into(),
            api_key: key.map(str::to_string),
            api_key_env: None,
            max_tokens: Some(8_000),
            temperature: Some(0.3),
            stream: true,
            context_window: Some(window),
            thinking: Thinking::High,
            thinking_wire: ThinkingWire::Auto,
        }
    }

    /// No key at construction is the **normal first-run state**, not an error: the cell has to come up,
    /// report itself unusable, and still be replaceable later without a restart.
    #[test]
    fn a_missing_key_gives_a_placeholder_that_says_why() {
        let live = LiveProvider::new(cfg("deepseek-flash", None, 1_000_000));
        assert!(!live.ready());
        assert!(live.error().unwrap().contains("API key"));
        // The config is still readable: the panel needs to show what *is* configured.
        assert_eq!(live.info().context_window, 1_000_000);
    }

    /// The placeholder the installer ships is **not** a configured key. This is the difference between
    /// "a fresh install says it is ready" (and 401s on the first message) and "a fresh install opens the
    /// Model tab", which is the entire point of the feature.
    #[test]
    fn the_shipped_placeholder_key_is_treated_as_not_configured() {
        let live = LiveProvider::new(cfg("deepseek-flash", Some("sk-..."), 1_000_000));
        assert!(!live.ready(), "sk-... is a placeholder, not a key");
        assert!(live.info().error.unwrap().contains("API key"));

        // And it cannot become ready by applying it either — the panel would otherwise write a key that
        // cannot work into config.toml and the next launch would start broken.
        let mut bad = cfg("deepseek-flash", Some("sk-..."), 1_000_000);
        bad.api_key = Some("sk-...".into());
        assert!(live.apply(bad).is_err());
        assert!(!live.ready());
    }

    /// The rule the whole type exists for: a failed apply **changes nothing**.
    #[test]
    fn a_failed_apply_leaves_the_previous_model_in_force() {
        let live = LiveProvider::new(cfg("good-model", Some("sk-good"), 128_000));
        assert!(live.ready());

        // Empty key → `resolve_key` fails → apply fails.
        let mut bad = cfg("bad-model", None, 4_000);
        bad.api_key = Some("   ".into());
        assert!(live.apply(bad).is_err());

        let info = live.info();
        assert_eq!(info.model, "good-model", "the model must not change on a failed apply");
        assert_eq!(info.context_window, 128_000, "and neither must the window");
        assert!(info.ready, "the previous handle stays usable");
    }

    /// Applying a good config swaps **both** cells together — the window travels with the model, which
    /// is what makes "switch the model, the ring's denominator follows" true.
    #[test]
    fn a_successful_apply_swaps_the_model_and_the_window_together() {
        let live = LiveProvider::new(cfg("small", Some("sk-a"), 32_768));
        live.apply(cfg("large", Some("sk-b"), 1_000_000)).unwrap();
        let info = live.info();
        assert_eq!(info.model, "large");
        assert_eq!(info.context_window, 1_000_000);
        assert!(info.ready);
        assert!(info.error.is_none(), "the previous error must be cleared");
    }

    /// `info()` reports the wire the request will really use, not the word `auto` from the file — the
    /// panel tells the user "this endpoint gets DeepSeek-style fields" from this value.
    #[test]
    fn info_resolves_the_auto_wire() {
        let live = LiveProvider::new(cfg("m", Some("sk-x"), 1_000));
        assert_eq!(live.info().wire, ThinkingWire::Deepseek);
    }
}
