pub mod ask;
pub mod hostfs;
pub mod meta;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::runtime::Runtime;

/// 宿主内置的**文件工具**名（顺序与 `hostfs::host_file_tools()` 的返回一致）。
///
/// 抽成常量表而不是让四处 `name()` 各写一个字面量，是因为**名字要被两处认出来**：
/// ① `hostfs` 里每个工具自己报的名字；② `agent::authorize_and_execute` 判定
/// 「这次调用是不是宿主文件工具」。两边引同一份常量，就不会出现「改了名字、
/// 边界检查悄悄失效」这种最难查的错位（工具照样能用，只是边界没了）。
pub(crate) const HOST_FILE_TOOLS: [&str; 5] =
    ["read_file", "write_file", "list_dir", "remove", "attach_file"];

/// 表里哪几个是**只读**的（下标）—— 读不设限，写删才过工作区边界（2026-10-04 的语义）。
///
/// 从 `host_tool_access` 里抽出来单独一个常量，是为了让「读的那几个是谁」这件事有
/// 一个**唯一的说法**：`attach_file` 算读（挂载只是把文件递出去，不落盘），将来再加
/// 只读工具时只改这一行，不会漏掉下面那个 `if`。
const READ_ONLY_TOOLS: [usize; 3] = [0 /* read_file */, 2 /* list_dir */, 4 /* attach_file */];

/// 一个宿主文件工具的**访问意图**：只读（读文件 / 列目录 / 挂附件）还是落盘（写 / 删）。
///
/// 与 [`HOST_FILE_TOOLS`] 同源（按同一张表的下标取名字），所以改了名字也不会让判定
/// 悄悄失效 —— 与那张表的注释是同一个理由。名字对不上（理论上不会发生）时按
/// **落盘**处理：保守方向永远是「宁可多挡一次，别少挡一次」。
pub(crate) fn host_tool_access(name: &str) -> crate::workplace::Access {
    if READ_ONLY_TOOLS.iter().any(|i| name == HOST_FILE_TOOLS[*i]) {
        crate::workplace::Access::Read
    } else {
        crate::workplace::Access::Write
    }
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// 这道工具**产出的文件**（本机路径），宿主会把它们挂给模型看 —— 见 `crate::attach`。
    ///
    /// 为什么挂在结果上、而不是搞一个全局暂存区：语义上这就是「这一步的产物」，
    /// 跟着结果走天然有正确的归属与顺序。宿主工具（`attach_file`）与插件（在返回的
    /// JSON 里写 `"attach": ["<路径>", …]`）两条路都填这个字段 —— 于是「插件截图 →
    /// 模型真的看见那张图」成立，而不是只把路径当字符串塞进上下文。
    ///
    /// ⚠️ 图片**只能**作为 `user` 消息发出（端点在 system / assistant 里见到图片直接 400），
    /// 所以这些路径不会塞进 `tool` 结果里，而是由主循环另起一条 user 消息注入
    /// （见 `agent_loop` 的收尾一段）。
    pub attach: Vec<std::path::PathBuf>,
}

impl ToolOutput {
    pub fn ok(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: false, attach: Vec::new() }
    }
    pub fn err(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: true, attach: Vec::new() }
    }

    /// 带上这道工具产出的文件。`ok(..)` / `err(..)` 之后链式调用。
    pub fn with_attach(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.attach = paths;
        self
    }
}

#[derive(Debug, Clone)]
pub enum ToolKind {
    /// 宿主自带的元工具（`create_plugin` / `list_plugins` / `remove_plugin`）——
    /// 它们动的是**宿主自身**（增删插件），所以默认要问一次（`[auth] meta_tools`）。
    Meta,
    /// 宿主自带的**文件工具**（`read_file` / `write_file` / `list_dir` / `remove`，
    /// 见 `hostfs`）—— 沙箱方案 ③ 的落地形态：参数与实现都在宿主手里，
    /// 于是「路径必须先过工作区判定」无法被绕过。
    ///
    /// **单独一类、不并进 `Meta`**，因为两者的授权语义不同：元工具改的是宿主自身，
    /// 而文件工具是「让宿主去干一件活」—— 与插件工具同类。并进 `Meta` 的后果是
    /// 每次读写都弹一次授权框（`meta_tools` 默认 `ask`），而模型手里 `fs_*` 那类
    /// 插件工具是 `auto` 的 —— 同一件事两套行为，模型换一个工具就变，最难解释。
    Host,
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

    /// 这道工具的一次调用算「**读**」还是「**落盘**」（写 / 删）——
    /// 只用于**工作区边界**的判定（见 `agent::Agent::workplace_reject`）。
    ///
    /// 默认 **落盘**，这是刻意的保守方向：宿主猜不出一个工具拿路径去读还是去写，
    /// 而「少挡一次写」的代价远大于「多挡一次读」。两类工具各自给出更准的答案：
    ///
    /// - **宿主文件工具**按名字判（schema 是我们自己的，见 [`host_tool_access`]）；
    /// - **插件工具**按**插件自己声明的** access 判（见 `plugin::ToolDef::access`）——
    ///   声明了 `"read"` 才放开，没声明仍是落盘。
    ///
    /// 声明是**插件作者的责任**，宿主不替它猜：一个只读工具（扫描 / 统计 / 列目录）
    /// 补一行声明就能像 `read_file` 一样访问工作区外的路径；不补，它会被那张粗网按
    /// 写入拦下 —— 拦下时给模型的报错里会写清该怎么补（见 `agent::PLUGIN_READ_HINT`）。
    fn access(&self) -> crate::workplace::Access {
        crate::workplace::Access::Write
    }

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
