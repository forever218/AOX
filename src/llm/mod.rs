pub mod failure;
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

/// The **complete** string value of `key` in a JSON object — `None` while it is still arriving.
///
/// Same input as [`partial_json_lines`] (tool-call arguments, which stream in fragments), but for
/// values that have to be shown **whole or not at all**: a `path`, say, where a half-typed
/// `E:/exam` reads as a glitch rather than as progress. A value whose closing quote has not turned
/// up yet simply counts as "not there yet".
///
/// The object is walked key by key rather than searched for the literal `"key"`: the arguments carry
/// the model's own text (`write_file`'s `content` is a whole source file), and that text very often
/// contains the string `"path"` itself — a plain `find` would happily match inside it.
///
/// `#[allow(dead_code)]` because `src/main.rs` (the CLI bin) compiles these modules a second time and
/// has no live display to feed — the same reason [`partial_json_lines`] carries it.
#[allow(dead_code)]
pub fn partial_json_string(args: &str, key: &str) -> Option<String> {
    match object_string_value(args, key) {
        Some((text, true)) => Some(text),
        _ => None,
    }
}

/// How many lines the string value of `key` already holds in a **partially received** JSON object.
///
/// While the model streams a `write_file` call its arguments arrive in fragments, so for a while the
/// value of `content` is a truncated JSON string — the closing quote has not arrived, and the last
/// line is usually cut in half. This counts the lines that have arrived so far, which is what lets a
/// front end put a **live, growing** number next to the call instead of going silent until the
/// whole file is in.
///
/// It counts the way `str::lines` does, so the reading matches the tool's own final tally: newlines
/// seen in the decoded value, plus one for a trailing partial line — but not for one that ended with
/// a newline. It is deliberately a **lower bound**, never an over-report: the tool's real diff
/// replaces it the moment the call actually runs.
///
/// Returns 0 when the key, or the opening quote of its value, has not arrived yet.
///
/// `#[allow(dead_code)]` because `src/main.rs` (the CLI bin) compiles these modules a second time,
/// and the CLI has no live-progress display to feed — the same reason the `Images` / `ToolResult`
/// events carry it.
#[allow(dead_code)]
pub fn partial_json_lines(args: &str, key: &str) -> usize {
    match object_string_value(args, key) {
        Some((text, _)) => text.lines().count(),
        None => 0,
    }
}

/// Walk a (possibly unfinished) JSON object and report the string value stored under `key`: the text
/// decoded so far, plus whether its closing quote had arrived.
///
/// `None` means "not there yet, and cannot be for now": the key was not reached, the object has
/// ended, or something *in front of* the key is itself still arriving — in which case where it ends
/// is unknown, and a later key cannot be located at all.
fn object_string_value(src: &str, key: &str) -> Option<(String, bool)> {
    let mut rest = src.trim_start().strip_prefix('{')?;
    loop {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix(',') {
            rest = r.trim_start();
        }
        // End of the object (or of what has arrived so far): the key is not in it.
        if !rest.starts_with('"') {
            return None;
        }
        let (k, after) = read_json_string(rest)?;
        // A key is only ever compared once it is whole — a fragment of one matching `key` would be
        // pure accident.
        let after = after?.trim_start().strip_prefix(':')?;
        let after = after.trim_start();
        if k == key {
            return read_json_string(after).map(|(v, tail)| (v, tail.is_some()));
        }
        // Not the key we want: step over its value. A string is stepped over by the reader itself;
        // anything else (number, bool, array, object) is stepped over by bracket depth.
        rest = if after.starts_with('"') {
            read_json_string(after)?.1?
        } else {
            skip_json_value(after)?
        };
    }
}

/// Read one JSON string starting at `s`. `None` as the whole result means `s` does not start with a
/// quote, so it is not a string at all.
///
/// The second half of the pair is what follows the closing quote — `None` while the string is still
/// in flight, including the case of a lone trailing backslash (the character it escapes does not
/// exist yet).
fn read_json_string(s: &str) -> Option<(String, Option<&str>)> {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, '"')) => {}
        _ => return None,
    }
    let mut out = String::new();
    while let Some((i, c)) = chars.next() {
        match c {
            // The closing quote: whatever follows it is the caller's business.
            '"' => return Some((out, Some(&s[i + 1..]))),
            '\\' => {
                let Some((_, e)) = chars.next() else { return Some((out, None)) };
                match e {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => {
                        // Four hex digits; if they have not all arrived the value is still in flight.
                        // A code point from outside the BMP needs a surrogate *pair* to come through
                        // exactly, so one that arrives alone shows as `?` here — this reading is only
                        // ever the live one, and the call's own arguments (already parsed JSON)
                        // replace it as soon as the model has finished writing.
                        let mut hex = String::new();
                        for _ in 0..4 {
                            match chars.next() {
                                Some((_, h)) => hex.push(h),
                                None => return Some((out, None)),
                            }
                        }
                        if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                            out.push(char::from_u32(cp).unwrap_or('?'));
                        }
                    }
                    // `\"`, `\\`, `\/` and anything unrecognised: the character itself.
                    e => out.push(e),
                }
            }
            c => out.push(c),
        }
    }
    // Ran out of input: still arriving.
    Some((out, None))
}

/// Step over one non-string JSON value, returning the remainder **starting at** whichever delimiter
/// ends it (`,` or `}`) — or `None` if the value has not finished arriving.
fn skip_json_value(s: &str) -> Option<&str> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        if in_str {
            match c {
                '\\' => {
                    chars.next();
                }
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' if depth > 0 => depth -= 1,
            ',' | '}' if depth == 0 => return Some(&s[i..]),
            _ => {}
        }
    }
    None
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

    /// Fragments of a **tool call's arguments**, as the model writes them.
    ///
    /// This is the only signal that exists *before* a tool runs: the loop only learns about tool
    /// calls once the whole response has arrived, so without this callback a long call — writing a
    /// file, above all — is a silent stretch in which the UI can show nothing at all. `index`
    /// identifies the call within the response (several may stream at once, and their fragments
    /// interleave); `name` is non-empty **only on the fragment that first carries it**, so an
    /// implementation that wants the tool's name should remember the first non-empty one.
    ///
    /// `delta` is a raw substring of the argument JSON and may be cut anywhere, including
    /// mid-escape — never parse it as JSON, treat it as text (see [`partial_json_lines`]).
    ///
    /// The default implementation does nothing, so a front end that has nowhere to put a live
    /// progress reading simply ignores it.
    fn on_tool_call(&self, _index: u64, _name: &str, _delta: &str) {}

    /// This response ended (whether it succeeded or failed). The UI should tear down the temporarily
    /// displayed reasoning process here, realising "show instantly, vanish instantly".
    fn on_finish(&self) {}

    /// A **new attempt** at the same request is about to start (`attempt` 1 = the first retry).
    ///
    /// Called by the agent loop, not by the provider: the retry decision lives there (see
    /// `crate::llm::failure`), and the provider only ever sees one attempt.
    ///
    /// This exists because a retry after a *cut stream* has already shown the user half an answer.
    /// Whatever is on screen from the abandoned attempt has to go — otherwise the retyped answer
    /// arrives underneath the fragment and the reply reads as if the model had said it twice. A
    /// front end is free to ignore this (the default does): the conversation file was never
    /// polluted, so ignoring it costs only the visual duplication.
    fn on_attempt(&self, _attempt: u32, _delay: std::time::Duration) {}
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

    /// The live reading has to behave while the JSON is still half-arrived: before the value starts,
    /// midway through a line, and with an escape cut in half.
    #[test]
    fn partial_lines_reads_a_half_arrived_value() {
        // Nothing of the value yet — the preamble alone must not invent a line.
        assert_eq!(partial_json_lines(r##"{"path":"a.rs","##, "content"), 0);
        assert_eq!(partial_json_lines(r##"{"path":"a.rs","content""##, "content"), 0);
        assert_eq!(partial_json_lines(r##"{"path":"a.rs","content":"##, "content"), 0);
        // The value started but holds no characters yet.
        assert_eq!(partial_json_lines(r##"{"path":"a.rs","content":""##, "content"), 0);
        // One line, still being written.
        assert_eq!(partial_json_lines(r##"{"path":"a.rs","content":"fn main() {}"##, "content"), 1);
        // ... and it grows as the escapes arrive.
        assert_eq!(
            partial_json_lines(r##"{"path":"a.rs","content":"fn main() {\n    println!(\"hi\");"##, "content"),
            2
        );
        // A trailing newline is not a line of its own -- the same rule as `str::lines`.
        assert_eq!(partial_json_lines(r##"{"path":"a.rs","content":"a\nb\n"##, "content"), 2);
        // A complete value: the count matches `lines()` exactly.
        assert_eq!(partial_json_lines(r##"{"path":"a.rs","content":"a\nb\nc"}"##, "content"), 3);
        // A lone backslash, with the escaped character still in flight, must not turn into a
        // phantom line.
        assert_eq!(partial_json_lines(r##"{"content":"ab\"##, "content"), 1);
        // A different key is not the one we count.
        assert_eq!(partial_json_lines(r##"{"content":"a\nb"}"##, "path"), 0);
    }

    /// A value that has to be shown **whole or not at all** — the reading a `path` needs, and the
    /// reason the object is walked rather than searched.
    #[test]
    fn partial_string_only_yields_a_finished_value() {
        // Not there yet, at every stage before the closing quote.
        assert_eq!(partial_json_string("{", "path"), None);
        assert_eq!(partial_json_string(r##"{"path"##, "path"), None);
        assert_eq!(partial_json_string(r##"{"path":"##, "path"), None);
        assert_eq!(partial_json_string(r##"{"path":"E:/ex"##, "path"), None);
        // Complete: the text comes back decoded, with other keys (in any order) sitting around it.
        assert_eq!(
            partial_json_string(r##"{"path":"E:/ex.html"}"##, "path").as_deref(),
            Some("E:/ex.html")
        );
        assert_eq!(
            partial_json_string(r##"{"content":"x","path":"/a/b.rs","create_dirs":true}"##, "path")
                .as_deref(),
            Some("/a/b.rs")
        );
        // The trap this walker exists for: the value of an **earlier** key contains the literal text
        // `"path"`, exactly as any source file about paths would. A search would match inside it.
        let tricky = r##"{"content":"let p = \"path\";\nfn f() {}","path":"/real/one.rs"}"##;
        assert_eq!(partial_json_string(tricky, "path").as_deref(), Some("/real/one.rs"));
        // The same arguments, still arriving: no path yet, even though the bogus one is long since in.
        let half = r##"{"content":"let p = \"path\";\nfn f() {}","path":"/real"##;
        assert_eq!(partial_json_string(half, "path"), None);
        // A key that is not in the object, and arguments that are not an object (a tool called with
        // no arguments at all), both mean "nothing".
        assert_eq!(partial_json_string(r##"{"content":"x"}"##, "path"), None);
        assert_eq!(partial_json_string("{}", "path"), None);
        assert_eq!(partial_json_string("not json", "path"), None);
    }
}
