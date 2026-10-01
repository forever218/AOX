pub mod meta;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::runtime::Runtime;

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: false }
    }
    pub fn err(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: true }
    }
}

#[derive(Debug, Clone)]
pub enum ToolKind {
    /// 宿主自带的元工具（create_plugin 等）
    Meta,
    /// 由某个插件提供的工具
    Plugin(String),
}

/// 工具抽象。所有工具（元工具 / 插件工具）对 Agent 循环透明。
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn kind(&self) -> ToolKind;
    fn description(&self) -> &str;
    /// 参数的 JSON Schema（object 类型）
    fn parameters(&self) -> Value;

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": self.description(),
                "parameters": self.parameters(),
            }
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput;
}
