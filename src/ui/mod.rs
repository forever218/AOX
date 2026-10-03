pub mod cli;
pub mod spinner;
pub mod style;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::history::UsageRecord;
use crate::llm::StreamSink;

/// Agent 流向 UI 的事件。GUI 实现可以直接订阅这个事件流做渲染。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 助手的**最终答复**：这条文本之后没有再调用任何工具，任务结束。
    AssistantText(String),
    /// 助手在「还有工具要调用」时说的过程性文字（如「我先看看有哪些插件」）。
    /// 属于过程说明，应折叠进过程区，不能当成最终答案展示。
    InterimText(String),
    /// 即将执行一次工具调用
    ToolCall { name: String, args: Value },
    /// 工具已通过授权，开始执行（可能耗时）
    ToolRunning { name: String, timeout_secs: u64 },
    /// 工具调用完成
    ToolResult { name: String, is_error: bool, preview: String },
    Info(String),
    Warn(String),
    Error(String),
}

/// 授权询问结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    Allow,
    AllowSession,
    Deny,
}

/// UI 抽象层 —— 为后续图形化预留的接口。
/// Agent 主循环只依赖这个 trait；CLI 是一个实现，未来 egui/Tauri 界面是另一个实现。
#[async_trait]
pub trait Ui: Send {
    /// 读取一行用户输入；Ok(None) 表示退出（EOF 或用户主动退出）
    async fn read_user_input(&mut self) -> Result<Option<String>>;
    /// 展示 Agent 事件
    fn emit(&mut self, event: AgentEvent);
    /// 就一次工具调用向用户请求授权
    async fn authorize(&mut self, tool_name: &str, args: &Value) -> AuthDecision;

    /// 一轮任务开始（用户发出指令，模型+工具循环开始）。
    /// UI 可在此开启等待动画、准备接收本轮事件。
    async fn turn_start(&mut self) {}
    /// 一轮任务结束（模型不再调用工具）。
    /// UI 可在此停止动画，并把中间过程折叠成一行摘要。
    async fn turn_end(&mut self) {}
    /// 一轮任务的 token 账。
    ///
    /// 参数就是**落进对话文件的那一行**（`kind:"usage"`），所以界面显示的账与
    /// 存档里的账不可能对不上。里面有轮次、时刻、本轮各项、以及截至本轮的对话累计。
    ///
    /// 特意排在 `turn_end` **之后**调用：那时等待动画已经撤掉，
    /// CLI 才能干干净净地把明细打在光标当前行，而不是被 spinner 的重绘盖过去。
    ///
    /// 端点不支持 usage 时 `rec.is_empty()` 为真，UI 直接跳过这段展示即可。
    async fn turn_usage(&mut self, _rec: &UsageRecord) {}
    /// 更新进行中的状态提示（如「思考中」「执行 fs_read」）。
    /// 默认空实现；GUI 可映射为状态栏文字。
    async fn set_status(&mut self, _label: &str) {}

    /// 提供一个「流式增量接收端」：Provider 在流式返回时把模型的思考过程推给它，
    /// UI 可以即时展示（CLI 是在等待动画区域滚动回显，显示完即消失）。
    ///
    /// 返回 None 表示本 UI 不展示流式过程。默认实现即 None，
    /// 所以 GUI 想显示思考流时只需覆盖这一个方法，核心循环无需改动。
    fn stream_sink(&self) -> Option<Box<dyn StreamSink + Send + Sync + '_>> {
        None
    }
}
