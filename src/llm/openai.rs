use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{ChatReply, LlmProvider, StreamSink, Usage};
use crate::config::ProviderConfig;
use crate::message::{FunctionCall, Message, ToolCall};

/// 单次流式响应写入日志的原始内容上限（防止长会话把内存吃满）
const MAX_RAW: usize = 60_000;

/// OpenAI 兼容协议（/chat/completions + tool_calls）。
/// 一份实现可接 DeepSeek / Kimi / OpenAI / vLLM / Ollama 等。
pub struct OpenAiProvider {
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    /// 是否以 SSE 流式方式请求。开启后思考过程可实时回显；
    /// 若某个端点不接受 stream/stream_options，可在 config.toml 里设为 false 回退。
    stream: bool,
}

impl OpenAiProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(180))
                .build()?,
            url: cfg.chat_url(),
            api_key: cfg.resolve_key()?,
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
            stream: cfg.stream,
        })
    }

    fn build_body(&self, messages: &[Message], tools: &[Value], stream: bool) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
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
        if stream {
            body["stream"] = json!(true);
            // 让服务端在最后一个 chunk 里带上 usage（推理 token 统计依赖它）
            body["stream_options"] = json!({ "include_usage": true });
        }
        body
    }

    /// 非流式请求：等完整响应一次性返回。
    async fn chat_blocking(&self, body: &Value) -> Result<ChatReply> {
        let resp = self.client.post(&self.url).bearer_auth(&self.api_key).json(body).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        debug_dump(body, &text);
        if !status.is_success() {
            bail!("LLM 请求失败 HTTP {status}: {}", api_error(&text));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("LLM 返回不是合法 JSON: {e}; body={}", truncate(&text, 500)))?;
        let choice = v.pointer("/choices/0").cloned().unwrap_or(Value::Null);
        let raw = choice.get("message").cloned().unwrap_or(Value::Null);
        if raw.is_null() {
            bail!("LLM 返回缺少 choices[0].message: {}", truncate(&text, 500));
        }
        let message: Message = serde_json::from_value(raw)
            .map_err(|e| anyhow!("assistant 消息解析失败: {e}; raw={}", truncate(&text, 500)))?;
        Ok(ChatReply {
            message,
            finish_reason: choice.get("finish_reason").and_then(|f| f.as_str()).map(str::to_string),
            usage: Usage::parse(&v),
        })
    }

    /// 流式请求：逐块解析 SSE，一边累加完整消息，一边把思考过程推给 UI。
    async fn chat_stream(
        &self,
        body: &Value,
        sink: Option<&(dyn StreamSink + Send + Sync)>,
    ) -> Result<ChatReply> {
        let mut resp = self.client.post(&self.url).bearer_auth(&self.api_key).json(body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            debug_dump(body, &text);
            bail!("LLM 请求失败 HTTP {status}: {}", api_error(&text));
        }

        let mut acc = StreamAcc::default();
        let mut raw = String::new();
        // 网络分块与 SSE 行边界不对齐，必须自己缓冲到换行才算一行
        let mut buf = String::new();
        while let Some(chunk) = resp.chunk().await? {
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
        // 个别服务端最后一个 data 行没有换行收尾，单独处理残留
        if !acc.is_done() && !buf.trim().is_empty() {
            let line = buf.trim_end().to_string();
            push_raw(&mut raw, &line);
            acc.feed(&line, sink);
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
        // 成功或失败都要通知 UI：撤掉临时展示的思考过程，避免它赖在屏幕上
        if let Some(s) = sink {
            s.on_finish();
        }
        result
    }
}

/// 一个工具调用的累加分片
#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

/// 流式响应的增量累加器：把一串 SSE data chunk 拼成完整的 assistant 消息。
///
/// 独立成结构体便于离线回归测试（selftest 直接用合成 chunk 喂它）。
#[derive(Default)]
pub struct StreamAcc {
    content: String,
    reasoning: String,
    /// 按 index 归并：同一工具调用的 name / arguments 会被拆到多个 chunk 里
    tool_calls: BTreeMap<u64, ToolCallAcc>,
    finish_reason: Option<String>,
    /// 端点给的用量。开了 `stream_options.include_usage` 之后，最后一帧会带上来。
    usage: Usage,
    done: bool,
}

impl StreamAcc {
    /// 是否已收到 `data: [DONE]`
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// 处理一行 SSE 原始文本（形如 `data: {...}`）；非协议行会被安静忽略。
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
            return; // 心跳/注释/非法行，一律跳过
        };
        self.apply(&v, sink);
    }

    fn apply(&mut self, v: &Value, sink: Option<&(dyn StreamSink + Send + Sync)>) {
        // 用量：多数端点在最后一帧单独发一个只有 usage 的 chunk。
        // 只在真的带了数字时才覆盖 —— 否则会把先前那一份清成空。
        let u = Usage::parse(v);
        if !u.is_empty() {
            self.usage = u;
        }
        // 纯 usage 的收尾 chunk 没有 choices，直接返回
        let Some(choice) = v.pointer("/choices/0") else { return };
        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(fr.to_string());
        }
        let Some(delta) = choice.get("delta") else { return };

        // 思考过程：DeepSeek 用 reasoning_content，少数实现叫 reasoning
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
                // 正文增量也推给 UI：实时 token 滚动要把输出侧（推理 + 正文）一起数
                if let Some(s) = sink {
                    s.on_content(c);
                }
            }
        }

        if let Some(arr) = delta.get("tool_calls").and_then(Value::as_array) {
            for (fallback, tc) in arr.iter().enumerate() {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(fallback as u64);
                let entry = self.tool_calls.entry(idx).or_default();
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        entry.id = id.to_string();
                    }
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(Value::as_str) {
                        if !n.is_empty() {
                            entry.name = n.to_string();
                        }
                    }
                    if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                        entry.arguments.push_str(a);
                    }
                }
            }
        }
    }

    /// 收尾：拼出 assistant 消息与协议元信息。
    pub fn into_reply(self) -> ChatReply {
        let content = if self.content.is_empty() { None } else { Some(self.content) };
        let tool_calls: Vec<ToolCall> = self
            .tool_calls
            .into_iter()
            .enumerate()
            .map(|(i, (_, a))| ToolCall {
                // 少数端点在流式分片里省略 id，这里补一个稳定可用的占位
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

fn push_raw(raw: &mut String, line: &str) {
    if raw.len() < MAX_RAW {
        raw.push_str(line);
        raw.push('\n');
    }
}

/// 从错误响应体里抽出可读信息：优先取 `error.message`，否则退回原始片段。
fn api_error(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(m) = v.pointer("/error/message").and_then(|m| m.as_str()) {
            let hint = if m.contains("reasoning_content") {
                "\n  提示: 该模型为思考模式（thinking），必须把上一条 assistant 消息的 \
                 reasoning_content 原样回传。若仍报此错，请把日志中的原始响应发我。"
            } else {
                ""
            };
            return format!("{m}{hint}");
        }
    }
    truncate(body, 500)
}

/// AOX_DEBUG_LLM=1 时把请求/响应原始 JSON 追加到 <cwd>/logs/llm.jsonl，便于事后定位。
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

/// 掐中间而不是掐尾：流式日志里最有诊断价值的是尾部（finish_reason / usage / [DONE]）。
fn head_tail(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    let head = n * 3 / 4;
    let tail = n - head;
    let h: String = s.chars().take(head).collect();
    let t: String = s.chars().skip(total - tail).collect();
    format!("{h}\n…[中间省略 {} 字符]…\n{t}", total - n)
}
