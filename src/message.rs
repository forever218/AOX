use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::attach::{self, Attachment};

/// 对话消息，直接对齐 OpenAI chat.completions 的 wire 格式。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    /// assistant 发起工具调用时 content 可能为 null，因此始终序列化该字段。
    ///
    /// **类型是 `Value` 而不是 `String`**（2026-10-05 为附件改的）：OpenAI 兼容协议里
    /// 纯文字是 JSON 字符串、带附件是 parts 数组，同一个字段承载两种形状。
    /// 改这一处就够 —— `openai.rs` 的 `build_body` 本来就把 `messages` 原样透传，
    /// 序列化交给 serde，所以协议层零改动。
    ///
    /// ⚠️ **纯文字仍然序列化成 JSON 字符串**（[`Self::plain`] 就是这么建的），
    /// 所以老对话文件、老端点、以及不发图的那条路径**逐字节不变** —— 这是本次改动
    /// 必须成立的零漂移条件，由 `plain_text_stays_a_json_string` 钉住。
    pub content: Option<Value>,
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

    /// 带附件的用户消息。
    ///
    /// **附件只能进 user 消息** —— 端点在 `system` / `assistant` 消息里见到图片直接 400
    /// （官方「使用限制」明写）。这条不是风格问题，是协议硬约束，所以本函数是唯一的
    /// 附件入口：想在别处塞图片，函数签名就先拦住了。
    ///
    /// 没有附件时退化成 [`Self::user`]（纯字符串），保证老路径逐字节不变。
    pub fn user_with_files(text: &str, files: &[Attachment]) -> Result<Self, String> {
        if files.is_empty() {
            return Ok(Self::user(text));
        }
        let body = attach::to_content(text, files)?;
        let mut m = Self::plain("user", String::new());
        m.content = Some(body);
        Ok(m)
    }

    fn plain(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(Value::String(content.into())),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn tool_result(call_id: &str, tool_name: &str, content: String) -> Self {
        Self {
            role: "tool".into(),
            content: Some(Value::String(content)),
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
            content: content.map(Value::String),
            reasoning_content: reasoning_content.filter(|r| !r.trim().is_empty()),
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            tool_call_id: None,
            name: None,
        }
    }

    /// 去掉首尾空白的正文；全空白视为「没有内容」。
    ///
    /// **只认纯字符串**：带附件的 parts 数组返回 `None` —— 调用点想显示「用户说了什么」
    /// 时应该用 [`Self::flat_text`]，那里的语义是「给人看的一行文本」而不是「模型的正文」。
    /// 保留这个严格语义是因为它在主循环里判定「模型是不是没说话」（`assistant` 的响应
    /// 永远是字符串），放宽会把空 parts 也当成有话说。
    pub fn text(&self) -> Option<&str> {
        self.content.as_ref()?.as_str().map(str::trim).filter(|t| !t.is_empty())
    }

    /// 任意形状的 content → 「给人看的一段文本」。
    ///
    /// 历史渲染、`.jsonl` 回读、界面气泡都走它：parts 数组把各 `text` 块拼起来，
    /// 图片与文件块换成 `[图片: 名字]` 这类占位符（附件本体不在内容里，占位才是
    /// 那段历史真正留下的东西）。
    pub fn flat_text(&self) -> String {
        let Some(v) = &self.content else {
            return String::new();
        };
        if let Some(s) = v.as_str() {
            return s.to_string();
        }
        let Some(parts) = v.as_array() else {
            // 既不是字符串也不是数组：如实吐出 JSON，好过悄悄显示成空白
            return v.to_string();
        };
        let mut out = String::new();
        for p in parts {
            match p.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    let Some(t) = p.get("text").and_then(|t| t.as_str()) else { continue };
                    // 附件清单是宿主加的，不是用户说的话 —— 见 `attach::ATTACH_MARK`
                    if t.starts_with(attach::ATTACH_MARK) {
                        continue;
                    }
                    out.push_str(t);
                }
                Some("image_url") => out.push_str("[图片]"),
                Some("file") => {
                    let name = p.get("filename").and_then(|f| f.as_str()).unwrap_or("附件");
                    out.push_str(&format!("[文件: {name}]"));
                }
                _ => {}
            }
        }
        out.trim().to_string()
    }

    /// 换掉正文（纯文本）。`hook` 改写模型回复时用。
    pub fn set_text(&mut self, s: impl Into<String>) {
        self.content = Some(Value::String(s.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **零漂移的关键断言**：没有附件时，一条 user 消息序列化出来必须与改类型之前
    /// 逐字节相同 —— `content` 是 JSON 字符串，不是 `[{...}]`。
    #[test]
    fn plain_text_stays_a_json_string() {
        let v = serde_json::to_value(Message::user("hi")).unwrap();
        assert_eq!(v["content"], Value::String("hi".into()));

        let v = serde_json::to_value(Message::tool_result("c1", "read_file", "ok".into())).unwrap();
        assert_eq!(v["content"], Value::String("ok".into()));
    }

    #[test]
    fn text_is_strict_but_flat_text_is_forgiving() {
        let d = std::env::temp_dir().join("aox-msg-flattext");
        let _ = std::fs::create_dir_all(&d);
        let png = d.join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();

        let a = Attachment::probe(&png).unwrap();
        let m = Message::user_with_files("看这张", &[a]).unwrap();

        assert!(m.text().is_none(), "parts 数组不该被当成「模型的正文」");
        let flat = m.flat_text();
        assert!(flat.contains("看这张"), "{flat}");
        assert!(flat.contains("[图片]"), "{flat}");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn user_with_files_without_files_is_the_old_message() {
        let a = Message::user_with_files("你好", &[]).unwrap();
        let b = Message::user("你好");
        assert_eq!(serde_json::to_value(&a).unwrap(), serde_json::to_value(&b).unwrap());
    }
}
