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
    /// 宿主刚刚把**工具产出的图片**挂进了对话（见 `crate::attach`）。
    ///
    /// 单开一条事件、而不是把路径塞进那条 `Warn` 里：`Warn` 是**给人看的一句话**，
    /// 而这里是**给人看的画面**。图形界面拿它把图渲染出来；命令行没有可渲染的地方，
    /// 直接忽略即可（文件名已经在那句话里列出来了）。
    ///
    /// `#[allow(dead_code)]` 的理由：`src/main.rs`（CLI bin）会把同一批模块再编译
    /// 一遍，而它只写 `Images(_)`、从不读里面的路径 —— 于是那个字段在 bin 里被判成
    /// 死代码，`lib` 侧却明明在用。**不要**为此动模块结构（与 `TRAPS.md` 里记的
    /// 「bin 与 lib 各编译一遍同名模块」那条同一个原因）。
    #[allow(dead_code)]
    Images(Vec<std::path::PathBuf>),
}

/// 授权询问结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    Allow,
    AllowSession,
    Deny,
}

/// 用户对一次「选项提问」的回答（`ask_user` 的返回值来源）。
///
/// 三支对应三种**真实发生过**的情形，措辞也不同（见 `tool::ask::interact`）：
/// 点了某一项 / 没点、自己打了一段字 / 压根没答。把它们合成 `Option<String>`
/// 会把「自己打的字」与「点了选项」混成一件事 —— 而模型对这两者的处理不一样
///（前者要多问一句「你是这个意思吗」，后者直接照做）。
#[derive(Debug, Clone)]
pub enum ChoiceReply {
    /// 点了第 n 项（0 基）
    Picked(usize),
    /// 没点选项，直接打了一段字
    Text(String),
    /// 没答（界面关掉了，或者断了）
    Cancelled,
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

    /// 就一道「选择题」问用户一次，等他作答（`ask_user` 的落地）。
    ///
    /// 默认实现返回 [`ChoiceReply::Cancelled`]：不覆盖它的 UI 不会因为缺这个能力而崩，
    /// 只会让 `ask_user` 得到「用户没作答」—— 那正是实情（这个界面确实没法问）。
    /// 两个前端（CLI / GUI）都实现了它。
    async fn choose(&mut self, _question: &str, _options: &[String]) -> ChoiceReply {
        ChoiceReply::Cancelled
    }

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
