use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::failure::{self, HttpFailure, Stalled, StreamCut};
use super::{ChatReply, LlmProvider, StreamSink, Usage};
use crate::config::ProviderConfig;
use crate::message::{FunctionCall, Message, ToolCall};

/// Cap on the raw content of one streaming response written to the log (prevents
/// long sessions from filling memory)
const MAX_RAW: usize = 60_000;

/// The OpenAI-compatible protocol (/chat/completions + tool_calls).
/// One implementation can plug into DeepSeek / Kimi / OpenAI / vLLM / Ollama, etc.
pub struct OpenAiProvider {
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    /// Whether to request via SSE streaming. When on, the reasoning process can be
    /// echoed live; if an endpoint doesn't accept stream/stream_options, set this
    /// to false in config.toml to fall back.
    stream: bool,
    /// How much thinking to ask for, and **which vendor's spelling** to say it in.
    ///
    /// Both are resolved once, when the provider is built (an `auto` wire becomes a concrete one from
    /// the host). That is deliberate: rebuilding the provider is exactly what the GUI's model switch
    /// does, so "the endpoint changed" and "the wire changed" are the same event.
    thinking: crate::config::Thinking,
    wire: crate::config::ThinkingWire,
}

impl OpenAiProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        Ok(Self {
            // No **total** deadline on the client any more: a streaming answer may legitimately take
            // minutes (a thinking model writing 32k tokens), and a whole-request deadline kills it
            // mid-sentence and, with retries on, pays for it twice. What replaces it is a silence
            // watchdog inside `chat_stream`; the non-streaming path sets its own per-request
            // deadline, where "the whole response" is the right unit.
            client: super::endpoint_client(&cfg.base_url).build()?,
            url: cfg.chat_url(),
            api_key: cfg.resolve_key()?,
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
            stream: cfg.stream,
            thinking: cfg.thinking,
            wire: cfg.wire(),
        })
    }

    /// The `messages` array for the request body.
    ///
    /// The only change from "just serialise them" is `reasoning_content`: as long as a message with
    /// tool calls exists, DeepSeek **requires** that field back and 400s without it, so it is always
    /// sent when the wire is DeepSeek. Every other endpoint may treat an unknown message field as a
    /// bad request, and "switched models mid-conversation and the next message 400s" is exactly the
    /// failure that would come of sending it there — so on any other wire it is stripped.
    fn body_messages(&self, messages: &[Message]) -> Value {
        let mut v = json!(messages);
        if self.wire != crate::config::ThinkingWire::Deepseek {
            if let Some(arr) = v.as_array_mut() {
                for m in arr.iter_mut() {
                    if let Some(o) = m.as_object_mut() {
                        o.remove("reasoning_content");
                    }
                }
            }
        }
        v
    }

    fn build_body(&self, messages: &[Message], tools: &[Value], stream: bool) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": self.body_messages(messages),
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
            body["tool_choice"] = json!("auto");
        }
        if let Some(t) = self.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = self.max_tokens {
            body["max_tokens"] = json!(m);
        }
        // Thinking strength: one call, every wire. `default` and `none` write nothing at all.
        self.wire.apply(self.thinking, &mut body);
        if stream {
            body["stream"] = json!(true);
            // Have the server include usage in the last chunk (reasoning-token
            // accounting depends on it)
            body["stream_options"] = json!({ "include_usage": true });
        }
        body
    }

    /// Non-streaming request: wait for the full response and return it in one go.
    async fn chat_blocking(&self, body: &Value) -> Result<ChatReply> {
        let resp = self
            .client
            .post(&self.url)
            // Here a whole-response deadline *is* the right shape: nothing is being displayed, so
            // "it has taken too long" is exactly what we need to know.
            .timeout(Duration::from_secs(failure::BLOCKING_TOTAL_SECS))
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        // Read the header **before** `text()` consumes the response.
        let retry_after = retry_after_of(&resp);
        let text = resp.text().await?;
        debug_dump(body, &text);
        if !status.is_success() {
            return Err(http_failure(status, retry_after, &text));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!(crate::tf!("LLM returned invalid JSON: {}; body={}", format!("{e}"), truncate(&text, 500))))?;
        let choice = v.pointer("/choices/0").cloned().unwrap_or(Value::Null);
        let raw = choice.get("message").cloned().unwrap_or(Value::Null);
        if raw.is_null() {
            bail!(crate::tf!("LLM response lacks choices[0].message: {}", truncate(&text, 500)));
        }
        let message: Message = serde_json::from_value(raw)
            .map_err(|e| anyhow!(crate::tf!("failed to parse the assistant message: {}; raw={}", format!("{e}"), truncate(&text, 500))))?;
        Ok(ChatReply {
            message,
            finish_reason: choice.get("finish_reason").and_then(|f| f.as_str()).map(str::to_string),
            usage: Usage::parse(&v),
        })
    }

    /// Streaming request: parse SSE chunk by chunk, accumulating the full message
    /// while pushing the reasoning process to the UI.
    async fn chat_stream(
        &self,
        body: &Value,
        sink: Option<&(dyn StreamSink + Send + Sync)>,
    ) -> Result<ChatReply> {
        let mut resp = self.client.post(&self.url).bearer_auth(&self.api_key).json(body).send().await?;
        let status = resp.status();
        let retry_after = retry_after_of(&resp);
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            debug_dump(body, &text);
            return Err(http_failure(status, retry_after, &text));
        }

        let mut acc = StreamAcc::default();
        let mut raw = String::new();
        // Network chunks don't align with SSE line boundaries, so buffer to a
        // newline before treating it as a line
        let mut buf = String::new();
        let idle = Duration::from_secs(failure::STREAM_IDLE_SECS);
        loop {
            // A **silence** deadline per chunk, not a deadline for the response as a whole: a model
            // thinking for two minutes between two tokens is normal, a model that has said nothing
            // for two minutes has died. `?` here keeps a transport error as a `reqwest::Error`, which
            // the retry policy reads as "try again" (see `failure::retryable`).
            let chunk = match tokio::time::timeout(idle, resp.chunk()).await {
                Ok(next) => next?,
                Err(_) => return Err(anyhow::Error::new(Stalled { idle_secs: failure::STREAM_IDLE_SECS })),
            };
            let Some(chunk) = chunk else { break };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line: String = buf[..pos].trim_end_matches('\r').to_string();
                buf.drain(..=pos);
                push_raw(&mut raw, &line);
                acc.feed(&line, sink);
            }
            if acc.is_done() {
                break;
            }
        }
        // A few servers end the last data line without a trailing newline;
        // handle the leftover separately
        if !acc.is_done() && !buf.trim().is_empty() {
            let line = buf.trim_end().to_string();
            push_raw(&mut raw, &line);
            acc.feed(&line, sink);
        }
        // ── Did the model actually finish? ──
        // Until 2026-10-23 this question was not asked, and a dropped connection (or a proxy cutting
        // a long body) produced a **successful** reply holding half a sentence: it went into the
        // conversation file and the turn ended as if the model had answered. A `finish_reason` chunk
        // or `data: [DONE]` is the only proof that the model is done talking.
        if !finished(&acc) {
            debug_dump(body, &raw);
            return Err(anyhow::Error::new(StreamCut {
                detail: crate::tf!("{} chars received, no finish_reason", acc.content.chars().count()),
            }));
        }
        debug_dump(body, &raw);
        Ok(acc.into_reply())
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[Value],
        sink: Option<&(dyn StreamSink + Send + Sync)>,
    ) -> Result<ChatReply> {
        let body = self.build_body(messages, tools, self.stream);
        let result = if self.stream {
            self.chat_stream(&body, sink).await
        } else {
            self.chat_blocking(&body).await
        };
        // Notify the UI whether we succeed or fail: tear down the temporarily shown reasoning process so it doesn't stick on screen
        if let Some(s) = sink {
            s.on_finish();
        }
        result
    }
}

/// An accumulating fragment of one tool call
#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

/// The incremental accumulator for a streaming response: assembles a series of
/// SSE data chunks into a complete assistant message.
///
/// Kept as its own struct for easy offline regression testing (selftest feeds it
/// synthetic chunks directly).
#[derive(Default)]
pub struct StreamAcc {
    content: String,
    reasoning: String,
    /// Merged by index: one tool call's name / arguments are split across several chunks
    tool_calls: BTreeMap<u64, ToolCallAcc>,
    finish_reason: Option<String>,
    /// The usage the endpoint provides. With `stream_options.include_usage` on,
    /// the last frame carries it.
    usage: Usage,
    done: bool,
}

impl StreamAcc {
    /// Whether `data: [DONE]` has been received
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Process one raw SSE line (of the form `data: {...}`); non-protocol lines are silently ignored.
    pub fn feed(&mut self, line: &str, sink: Option<&(dyn StreamSink + Send + Sync)>) {
        let Some(payload) = line.trim().strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            return;
        }
        if payload == "[DONE]" {
            self.done = true;
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            return; // Heartbeats/comments/invalid lines are all skipped
        };
        self.apply(&v, sink);
    }

    fn apply(&mut self, v: &Value, sink: Option<&(dyn StreamSink + Send + Sync)>) {
        // Usage: most endpoints send a usage-only chunk as the final frame.
        // Only overwrite when it really carries numbers — otherwise it would wipe
        // the earlier copy to empty.
        let u = Usage::parse(v);
        if !u.is_empty() {
            self.usage = u;
        }
        // A pure-usage closing chunk has no choices; return right away
        let Some(choice) = v.pointer("/choices/0") else { return };
        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(fr.to_string());
        }
        let Some(delta) = choice.get("delta") else { return };

        // Reasoning process: DeepSeek uses reasoning_content, a few implementations call it reasoning
        if let Some(r) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
        {
            if !r.is_empty() {
                self.reasoning.push_str(r);
                if let Some(s) = sink {
                    s.on_reasoning(r);
                }
            }
        }

        if let Some(c) = delta.get("content").and_then(Value::as_str) {
            if !c.is_empty() {
                self.content.push_str(c);
                // Push the content delta to the UI too: live token scrolling must
                // count the output side (reasoning + content) together
                if let Some(s) = sink {
                    s.on_content(c);
                }
            }
        }

        if let Some(arr) = delta.get("tool_calls").and_then(Value::as_array) {
            for (fallback, tc) in arr.iter().enumerate() {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(fallback as u64);
                let entry = self.tool_calls.entry(idx).or_default();
                // What this fragment actually carried, to hand to the UI below. The name and the
                // arguments are collected rather than sent one by one so a fragment that brings
                // both produces a single callback.
                let mut name_delta: &str = "";
                let mut arg_delta: &str = "";
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        entry.id = id.to_string();
                    }
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(Value::as_str) {
                        if !n.is_empty() {
                            entry.name = n.to_string();
                            name_delta = n;
                        }
                    }
                    if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                        entry.arguments.push_str(a);
                        arg_delta = a;
                    }
                }
                // Tell the UI a call is being **written right now**. This stretches before the tool
                // ever runs — the loop only sees tool calls once the whole response has landed — and
                // for `write_file` it is by far the longest part of the job, so it is the only place
                // a live "how much has arrived" reading can come from.
                if let Some(s) = sink {
                    if !name_delta.is_empty() || !arg_delta.is_empty() {
                        s.on_tool_call(idx, name_delta, arg_delta);
                    }
                }
            }
        }
    }

    /// Finalize: assemble the assistant message and protocol metadata.
    pub fn into_reply(self) -> ChatReply {
        let content = if self.content.is_empty() { None } else { Some(self.content) };
        let tool_calls: Vec<ToolCall> = self
            .tool_calls
            .into_iter()
            .enumerate()
            .map(|(i, (_, a))| ToolCall {
                // A few endpoints omit id in the streaming fragment; fill in a stable placeholder here
                id: if a.id.is_empty() { format!("call_{i}") } else { a.id },
                kind: "function".into(),
                function: FunctionCall {
                    name: a.name,
                    arguments: if a.arguments.trim().is_empty() { "{}".into() } else { a.arguments },
                },
            })
            .collect();
        ChatReply {
            message: Message::assistant(content, Some(self.reasoning), tool_calls),
            finish_reason: self.finish_reason,
            usage: self.usage,
        }
    }
}

/// Did the model prove it finished talking?
///
/// Two signals, either is enough: the `data: [DONE]` sentinel, or a chunk carrying `finish_reason`.
/// Endpoints vary — some send both, some only one — so "both missing" is the only reliable sign of a
/// response that was cut, and it is exactly what a dropped TCP connection leaves behind.
fn finished(acc: &StreamAcc) -> bool {
    acc.is_done() || acc.finish_reason.is_some()
}

fn push_raw(raw: &mut String, line: &str) {
    if raw.len() < MAX_RAW {
        raw.push_str(line);
        raw.push('\n');
    }
}

/// The `Retry-After` the endpoint sent, if any. Read **before** the response is consumed.
fn retry_after_of(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok().and_then(failure::parse_retry_after)
}

/// A non-2xx response, as an error the retry policy can read ([`failure::retryable`]).
///
/// The status travels *inside* the error rather than in its message: telling `429` from `401` by
/// parsing text would be the kind of thing that breaks the day an endpoint rewords its body.
fn http_failure(status: reqwest::StatusCode, retry_after: Option<Duration>, body: &str) -> anyhow::Error {
    anyhow::Error::new(HttpFailure { status: status.as_u16(), retry_after, detail: api_error(body) })
}

/// Extract readable information from an error response body: prefer
/// `error.message`, otherwise fall back to the raw fragment.
fn api_error(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(m) = v.pointer("/error/message").and_then(|m| m.as_str()) {
            let hint = if m.contains("reasoning_content") {
                crate::i18n::t("\n  Hint: this model is a thinking model; the previous assistant message's \
                 reasoning_content must be sent back verbatim. If it still errors, send me the raw response from the log.")
            } else {
                ""
            };
            return format!("{m}{hint}");
        }
    }
    truncate(body, 500)
}

/// With AOX_DEBUG_LLM=1, append the raw request/response JSON to <cwd>/logs/llm.jsonl for later diagnosis.
fn debug_dump(request: &Value, response: &str) {
    if std::env::var("AOX_DEBUG_LLM").map(|v| v == "0").unwrap_or(true) {
        return;
    }
    let dir = std::path::PathBuf::from("logs");
    let _ = std::fs::create_dir_all(&dir);
    let line = json!({
        "ts": now_secs(),
        "request": request,
        "response_raw": head_tail(response, 20_000),
    });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("llm.jsonl")) {
        use std::io::Write as _;
        let _ = writeln!(f, "{line}");
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// Clip the middle rather than the tail: in a streaming log the most diagnostically valuable part is the tail (finish_reason / usage / [DONE]).
fn head_tail(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    let head = n * 3 / 4;
    let tail = n - head;
    let h: String = s.chars().take(head).collect();
    let t: String = s.chars().skip(total - tail).collect();
    crate::tf!("{}\n…[{} chars omitted in the middle]…\n{}", h, total - n, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed raw SSE lines, exactly as `chat_stream` would, without a socket.
    fn acc_of(lines: &[&str]) -> StreamAcc {
        let mut acc = StreamAcc::default();
        for l in lines {
            acc.feed(l, None);
        }
        acc
    }

    const CONTENT: &str = r#"data: {"choices":[{"delta":{"content":"half an answer"},"finish_reason":null}]}"#;
    const FINISH: &str = r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#;

    #[test]
    fn a_stream_that_just_stops_is_not_a_finished_answer() {
        // The 2026-10-23 bug: this used to be a *successful* reply holding half a sentence.
        let acc = acc_of(&[CONTENT]);
        assert!(!finished(&acc), "content without finish_reason means the connection dropped");
        assert!(!acc.is_done());
    }

    #[test]
    fn either_proof_of_completion_is_enough() {
        assert!(finished(&acc_of(&[CONTENT, FINISH])), "finish_reason alone is a finish");
        assert!(finished(&acc_of(&[CONTENT, "data: [DONE]"])), "[DONE] alone is a finish");
    }

    #[test]
    fn an_empty_stream_is_not_finished_either() {
        // Zero lines: nothing to hand to the model, and handing back "no message" as success is how
        // a turn silently ends with no answer at all.
        assert!(!finished(&acc_of(&[])));
    }
}
