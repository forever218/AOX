pub mod openai;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::message::Message;

/// The complete result of one model call.
///
/// Besides the assistant message itself it carries protocol-level metadata: `finish_reason`
/// distinguishes "finished normally / called a tool / was cut off by max_tokens", which is the key
/// to diagnosing "the task stopped for no apparent reason".
#[derive(Debug, Clone)]
pub struct ChatReply {
    pub message: Message,
    /// stop | tool_calls | length | content_filter | ...
    pub finish_reason: Option<String>,
    /// Tokens consumed by this call (each field is `None` when the endpoint does not return usage)
    pub usage: Usage,
}

impl ChatReply {
    /// Whether it was truncated because max_tokens ran out (reasoning tokens count too; thinking models hit this especially easily)
    pub fn truncated(&self) -> bool {
        self.finish_reason.as_deref() == Some("length")
    }

    /// The model neither produced body text nor asked to call a tool — this turn did nothing at all.
    pub fn is_empty(&self) -> bool {
        let no_text = self.message.text().is_none();
        let no_calls = self.message.tool_calls.as_ref().is_none_or(|c| c.is_empty());
        no_text && no_calls
    }

    /// A short summary for diagnostics: finish_reason + reasoning-token spend.
    pub fn diag(&self) -> String {
        match self.usage.reasoning {
            Some(n) => crate::tf!("finish_reason={}, reasoning tokens={}", format!("{:?}", self.finish_reason), n),
            None => format!("finish_reason={:?}", self.finish_reason),
        }
    }
}

// ────────────────────────────── token accounting ──────────────────────────────

/// The tokens one model call consumed — record whatever the server reports, and `None` for whatever
/// is missing.
///
/// Different OpenAI-compatible endpoints differ slightly in the fields they return, so this
/// normalises once: a cache hit may appear as either
/// `prompt_tokens_details.cached_tokens` (newer OpenAI/DeepSeek) or `prompt_cache_hit_tokens`
/// (older DeepSeek), and both forms are accepted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// Input (prompt) tokens
    pub prompt: Option<u64>,
    /// Output (completion) tokens
    pub completion: Option<u64>,
    /// Total
    pub total: Option<u64>,
    /// The tokens spent on "thinking" among those (already included in completion, not a separate share)
    pub reasoning: Option<u64>,
    /// Input tokens that hit the cache
    pub cached: Option<u64>,
    /// Input tokens that missed the cache (DeepSeek only)
    pub cache_miss: Option<u64>,
}

impl Usage {
    /// Parse the `usage` from a response. What's passed in may be **the whole response body** or
    /// already the `usage` object itself — if there's a `usage` key it drills down one level itself,
    /// saving every call site from writing the `pointer` by hand. Passing `Value::Null` is fine too;
    /// nothing found means all `None`.
    pub fn parse(v: &Value) -> Self {
        let v = v.get("usage").unwrap_or(v);
        let get = |p: &str| v.pointer(p).and_then(Value::as_u64);
        let mut u = Self {
            prompt: get("/prompt_tokens"),
            completion: get("/completion_tokens"),
            total: get("/total_tokens"),
            reasoning: get("/completion_tokens_details/reasoning_tokens"),
            cached: get("/prompt_tokens_details/cached_tokens")
                .or_else(|| get("/prompt_cache_hit_tokens")),
            cache_miss: get("/prompt_cache_miss_tokens"),
        };
        // Plenty of self-hosted endpoints give only prompt/completion and no total; fill it in here,
        // so every downstream display doesn't have to write the same fallback.
        if u.total.is_none() {
            u.total = match (u.prompt, u.completion) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            };
        }
        u
    }

    /// Not a single field was obtained (the endpoint doesn't return usage at all)
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Accumulate another call's usage: a field is summed only when both sides have a number,
    /// otherwise the side that has a value is kept.
    pub fn accrue(&mut self, other: &Usage) {
        fn add(a: &mut Option<u64>, b: Option<u64>) {
            if let Some(b) = b {
                *a = Some(a.unwrap_or(0) + b);
            }
        }
        add(&mut self.prompt, other.prompt);
        add(&mut self.completion, other.completion);
        add(&mut self.total, other.total);
        add(&mut self.reasoning, other.reasoning);
        add(&mut self.cached, other.cached);
        add(&mut self.cache_miss, other.cache_miss);
    }
}

/// A token estimator for the streaming process (fed incrementally, never rescanning the whole text).
///
/// In a streaming response the server provides usage **only on the very last frame**, so to show a
/// number ticking up mid-stream it has to be estimated: roughly 4 ASCII characters per token, and
/// about 1 token per full-width CJK character. It is a rough estimate, and at the end the server's
/// exact value overrides it.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokenEstimate {
    ascii: u64,
    wide: u64,
}

impl TokenEstimate {
    /// Feed a newly arrived delta of text. It is called at high frequency, so it only adds and never
    /// rescans.
    pub fn add(&mut self, delta: &str) {
        for ch in delta.chars() {
            if ch.is_ascii() {
                self.ascii += 1;
            } else {
                self.wide += 1;
            }
        }
    }

    /// The estimated token count (output side: thinking + body text)
    pub fn tokens(&self) -> u64 {
        self.ascii / 4 + self.wide
    }
}

/// Thousands separators: `12345` → `12,345`. Everything that needs this lives here, so don't reinvent it.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The sink for streaming deltas.
///
/// The provider calls these callbacks **synchronously** as it parses the SSE response chunk by chunk
/// (no cross-thread work), so implementations should stay lightweight: the UI usually just stuffs
/// the text into a shared buffer for the render thread to draw.
///
/// Methods take `&self` (interior mutability): this lets the sink be passed by shared reference,
/// avoiding borrow conflicts and making the same implementation reusable in multiple places.
pub trait StreamSink: Send + Sync {
    /// Deltas of a thinking-mode model's reasoning process (deepseek-flash / deepseek-reasoner, etc.).
    /// May be called at high frequency within one response, each time giving only a small fragment (even a single character).
    fn on_reasoning(&self, _delta: &str) {}

    /// Deltas of the actual body text.
    ///
    /// Keeping it separate from `on_reasoning` is useful: the reasoning process should "show
    /// instantly, vanish instantly", whereas the body text stays on screen. The live token counter,
    /// by contrast, has to count both — output-side spend already includes reasoning — so an
    /// implementation normally feeds both callbacks into the same estimator.
    fn on_content(&self, _delta: &str) {}

    /// This response ended (whether it succeeded or failed). The UI should tear down the temporarily
    /// displayed reasoning process here, realising "show instantly, vanish instantly".
    fn on_finish(&self) {}
}

/// The model-provider abstraction. Wiring up Anthropic / local inference in the future just means
/// implementing one more Provider.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Send the full conversation history and the current tool schema list, and return the model's
    /// reply (which may contain tool_calls).
    ///
    /// `sink` is an optional streaming-delta sink: when passed, the Provider requests in streaming
    /// mode where possible and pushes the model's reasoning process to it in real time; passing None
    /// just means waiting quietly for the complete response.
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[Value],
        sink: Option<&(dyn StreamSink + Send + Sync)>,
    ) -> Result<ChatReply>;
}

/// A placeholder Provider used when config is invalid: the REPL can still start (inspect tools, preview formatting), and only errors when an actual conversation begins.
pub struct UnavailableProvider {
    pub reason: String,
}

#[async_trait]
impl LlmProvider for UnavailableProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[Value],
        _sink: Option<&(dyn StreamSink + Send + Sync)>,
    ) -> Result<ChatReply> {
        anyhow::bail!(crate::tf!("model unavailable: {}", self.reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_reads_every_known_field() {
        let u = Usage::parse(&json!({
            "prompt_tokens": 1200,
            "completion_tokens": 300,
            "total_tokens": 1500,
            "prompt_tokens_details": {"cached_tokens": 1024},
            "completion_tokens_details": {"reasoning_tokens": 42}
        }));
        assert_eq!(u.prompt, Some(1200));
        assert_eq!(u.completion, Some(300));
        assert_eq!(u.total, Some(1500));
        assert_eq!(u.cached, Some(1024));
        assert_eq!(u.reasoning, Some(42));
        assert_eq!(u.cache_miss, None);
        assert!(!u.is_empty());
    }

    /// Older DeepSeek spreads the cache hit across two top-level fields, not inside details
    #[test]
    fn usage_reads_flat_cache_fields() {
        let u = Usage::parse(&json!({
            "prompt_tokens": 1000,
            "completion_tokens": 10,
            "prompt_cache_hit_tokens": 768,
            "prompt_cache_miss_tokens": 232
        }));
        assert_eq!(u.cached, Some(768));
        assert_eq!(u.cache_miss, Some(232));
    }

    /// Plenty of self-hosted endpoints give only prompt/completion. The total can be inferred, so it
    /// must not be left blank in the interface
    #[test]
    fn usage_infers_missing_total() {
        let u = Usage::parse(&json!({"prompt_tokens": 10, "completion_tokens": 5}));
        assert_eq!(u.total, Some(15));
    }

    /// The final streaming chunk has only usage and no choices; passing the whole response body or
    /// the bare object must both parse
    #[test]
    fn usage_accepts_wrapper_or_bare_object() {
        let wrapped = Usage::parse(&json!({"choices": [], "usage": {"prompt_tokens": 7}}));
        let bare = Usage::parse(&json!({"prompt_tokens": 7}));
        assert_eq!(wrapped.prompt, Some(7));
        assert_eq!(bare.prompt, Some(7));
        assert!(Usage::parse(&Value::Null).is_empty());
    }

    #[test]
    fn usage_accrue_sums_each_field() {
        let mut a = Usage::parse(&json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}));
        a.accrue(&Usage::parse(&json!({"prompt_tokens": 20, "completion_tokens": 7, "total_tokens": 27})));
        assert_eq!(a.prompt, Some(30));
        assert_eq!(a.completion, Some(12));
        assert_eq!(a.total, Some(42));
        // A field the other side did not report must not be zeroed, nor can it be invented out of thin air
        assert_eq!(a.reasoning, None);
    }

    /// One CJK character is about one token and ASCII about four — the weights on the two sides must
    /// differ by an order of magnitude, otherwise the live number in a Chinese conversation is too
    /// low to look like it's moving.
    #[test]
    fn estimate_weights_cjk_above_ascii() {
        let mut e = TokenEstimate::default();
        e.add("abcd");
        assert_eq!(e.tokens(), 1);
        e.add("中文");
        assert_eq!(e.tokens(), 3);
        // Incremental accumulation rather than rescanning: the order of the fragments doesn't matter
        let mut a = TokenEstimate::default();
        let mut b = TokenEstimate::default();
        for piece in ["hel", "lo 世", "界"] {
            a.add(piece);
        }
        b.add("hello 世界");
        assert_eq!(a.tokens(), b.tokens());
    }

    #[test]
    fn thousands_groups_by_three() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(12345), "12,345");
        assert_eq!(thousands(1234567), "1,234,567");
    }
}
