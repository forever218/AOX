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
    /// 思考模型消耗的推理 token 数（未返回则为 None）
    pub reasoning_tokens: Option<u64>,
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
        match self.reasoning_tokens {
            Some(n) => format!("finish_reason={:?}, 推理 token={n}", self.finish_reason),
            None => format!("finish_reason={:?}", self.finish_reason),
        }
    }
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
