use serde::{Deserialize, Serialize};

/// 对话消息，直接对齐 OpenAI chat.completions 的 wire 格式。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    /// assistant 发起工具调用时 content 可能为 null，因此始终序列化该字段
    pub content: Option<String>,
    /// 思考模式（DeepSeek thinking，如 deepseek-flash / deepseek-reasoner）返回的推理内容。
    ///
    /// ⚠️ 必须原样回传给 API：只要历史里存在「带 tool_calls 的 assistant 消息」，
    /// DeepSeek 就要求它连同 reasoning_content 一起送回，否则整次请求被 400 拒绝：
    /// "The `reasoning_content` in the thinking mode must be passed back to the API."
    /// 这正是「多步任务中途静默断掉」的元凶。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// OpenAI 协议中参数是 JSON 字符串
    pub arguments: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain("system", content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::plain("user", content)
    }

    fn plain(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn tool_result(call_id: &str, tool_name: &str, content: String) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(call_id.to_string()),
            name: Some(tool_name.to_string()),
        }
    }

    /// 组装一条 assistant 消息。
    ///
    /// 流式累加的结果也走这里，确保「流式」与「非流式」两条路径产出的字段完全一致
    /// （尤其是思考模式必须回传的 reasoning_content）。
    pub fn assistant(
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: "assistant".into(),
            content,
            reasoning_content: reasoning_content.filter(|r| !r.trim().is_empty()),
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            tool_call_id: None,
            name: None,
        }
    }

    /// 去掉首尾空白的正文；全空白视为「没有内容」。
    pub fn text(&self) -> Option<&str> {
        self.content.as_deref().map(str::trim).filter(|t| !t.is_empty())
    }
}
