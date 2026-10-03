pub mod openai;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::message::Message;

/// 一次模型调用的完整结果。
///
/// 除了 assistant 消息本身，还带上协议层的元信息：`finish_reason` 能区分
/// 「正常结束 / 调用了工具 / 被 max_tokens 截断」，是诊断「任务莫名中断」的关键。
#[derive(Debug, Clone)]
pub struct ChatReply {
    pub message: Message,
    /// stop | tool_calls | length | content_filter | ...
    pub finish_reason: Option<String>,
    /// 本次调用消耗的 token（端点未返回 usage 时各项均为 None）
    pub usage: Usage,
}

impl ChatReply {
    /// 是否因为 max_tokens 用尽被截断（推理 token 也算在内，思考模型尤其容易触发）
    pub fn truncated(&self) -> bool {
        self.finish_reason.as_deref() == Some("length")
    }

    /// 模型既没给正文也没要求调用工具 —— 这一轮等于什么都没做。
    pub fn is_empty(&self) -> bool {
        let no_text = self.message.text().is_none();
        let no_calls = self.message.tool_calls.as_ref().is_none_or(|c| c.is_empty());
        no_text && no_calls
    }

    /// 供诊断信息使用的简述：finish_reason + 推理 token 消耗。
    pub fn diag(&self) -> String {
        match self.usage.reasoning {
            Some(n) => format!("finish_reason={:?}, 推理 token={n}", self.finish_reason),
            None => format!("finish_reason={:?}", self.finish_reason),
        }
    }
}

// ────────────────────────────── token 账 ──────────────────────────────

/// 一次模型调用消耗的 token —— 服务端报什么就记什么，缺哪项就是 `None`。
///
/// 各家 OpenAI 兼容端点给的字段小有出入，这里做一次归一化：缓存命中有
/// `prompt_tokens_details.cached_tokens`（OpenAI/DeepSeek 新版）与
/// `prompt_cache_hit_tokens`（DeepSeek 旧版）两种写法，两种都收。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// 输入（prompt）token
    pub prompt: Option<u64>,
    /// 输出（completion）token
    pub completion: Option<u64>,
    /// 合计
    pub total: Option<u64>,
    /// 其中「思考」用掉的 token（已经包含在 completion 里，不是额外的一份）
    pub reasoning: Option<u64>,
    /// 输入里命中缓存的 token
    pub cached: Option<u64>,
    /// 输入里未命中缓存的 token（DeepSeek 独有）
    pub cache_miss: Option<u64>,
}

impl Usage {
    /// 解析响应里的 usage。传进来的既可能是**整个响应体**，也可能已经是
    /// `usage` 对象本身 —— 有 `usage` 键就自己下钻一层，省得每个调用点都写一遍
    /// `pointer`。传 `Value::Null` 也不会出错，取不到就是全 `None`。
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
        // 不少自建端点只给 prompt/completion、不给 total，这里补齐，
        // 省得下游每处展示都要写一遍 fallback。
        if u.total.is_none() {
            u.total = match (u.prompt, u.completion) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            };
        }
        u
    }

    /// 一项都没拿到（端点压根不返回 usage）
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// 累加另一次调用的用量：某一项两边都有数字时才相加，否则保留有值的那边。
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

/// 流式过程中的 token 估算器（增量喂入，不重扫全文）。
///
/// 流式响应里服务端**直到最后一帧**才给 usage，中途想显示一个在涨的数字
/// 就只能自己估：ASCII 约 4 字符 1 token，CJK 等宽字符约 1 字符 1 token。
/// 这是个粗估，收尾时会用服务端返回的精确值顶掉。
#[derive(Debug, Default, Clone, Copy)]
pub struct TokenEstimate {
    ascii: u64,
    wide: u64,
}

impl TokenEstimate {
    /// 喂一段新来的增量文本。会被高频调用，所以只做加法、不重扫。
    pub fn add(&mut self, delta: &str) {
        for ch in delta.chars() {
            if ch.is_ascii() {
                self.ascii += 1;
            } else {
                self.wide += 1;
            }
        }
    }

    /// 估算的 token 数（输出侧：思考 + 正文）
    pub fn tokens(&self) -> u64 {
        self.ascii / 4 + self.wide
    }
}

/// 千分位写法：`12345` → `12,345`。用得上的地方都在这儿，别各写各的。
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

/// 流式增量的接收端。
///
/// Provider 在逐块解析 SSE 响应时**同步**调用这些回调（不跨线程），
/// 因此实现方应尽量轻量：UI 通常只是把文本塞进一个共享缓冲区，由渲染线程去画。
///
/// 方法取 `&self`（内部可变性）：这样接收端可以以共享引用传递，
/// 既免去借用冲突，也方便同一实现被多处复用。
pub trait StreamSink: Send + Sync {
    /// 思考模式模型的推理过程增量（deepseek-flash / deepseek-reasoner 等）。
    /// 一次响应内可能被高频调用，每次只给一小段（甚至一个字）。
    fn on_reasoning(&self, _delta: &str) {}

    /// 正式正文的增量。
    ///
    /// 与 `on_reasoning` 分开是有用的：推理过程要「即时显示、即时消失」，
    /// 正文却要留在屏幕上。实时 token 滚动则两者都要数 —— 输出侧的消耗
    /// 本来就包含推理，所以实现方通常把两个回调都喂给同一个估算器。
    fn on_content(&self, _delta: &str) {}

    /// 本次响应结束（无论成功还是出错）。UI 应在此撤掉临时展示的思考过程，
    /// 实现「即时显示、即时消失」。
    fn on_finish(&self) {}
}

/// 模型提供方抽象。未来接入 Anthropic / 本地推理等，只需再实现一个 Provider。
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// 发送完整对话历史与当前工具 schema 列表，返回模型回复（可能含 tool_calls）。
    ///
    /// `sink` 为可选的流式增量接收端：传入时 Provider 会尽量以流式方式请求，
    /// 并把模型的思考过程实时推给它；传 None 则只是安静地等待完整响应。
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[Value],
        sink: Option<&(dyn StreamSink + Send + Sync)>,
    ) -> Result<ChatReply>;
}

/// 配置无效时的占位 Provider：REPL 仍可启动（查看工具、排版预览），真正对话时才报错。
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
        anyhow::bail!("模型不可用：{}", self.reason)
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

    /// DeepSeek 旧版把缓存命中摊在顶层的两个字段上，不在 details 里
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

    /// 不少自建端点只给 prompt/completion。合计可以推出来，就不该让界面空着
    #[test]
    fn usage_infers_missing_total() {
        let u = Usage::parse(&json!({"prompt_tokens": 10, "completion_tokens": 5}));
        assert_eq!(u.total, Some(15));
    }

    /// 流式的收尾 chunk 只有 usage、没 choices；传整个响应体或裸对象都得能解析
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
        // 对方没报的那一项不该被清零，也不可能被凭空造出来
        assert_eq!(a.reasoning, None);
    }

    /// CJK 一个字约一个 token，ASCII 约四个 —— 两边的权重必须差出量级，
    /// 否则中文对话的实时数字会低到看不出在涨。
    #[test]
    fn estimate_weights_cjk_above_ascii() {
        let mut e = TokenEstimate::default();
        e.add("abcd");
        assert_eq!(e.tokens(), 1);
        e.add("中文");
        assert_eq!(e.tokens(), 3);
        // 增量累加而不是重扫：分片顺序不影响结果
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
