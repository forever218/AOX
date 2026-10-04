pub mod hostfs;
pub mod meta;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::runtime::Runtime;

/// 宿主内置的**文件工具**名（顺序与 `hostfs::host_file_tools()` 的返回一致）。
///
/// 抽成常量表而不是让四处 `name()` 各写一个字面量，是因为**名字要被两处认出来**：
/// ① `hostfs` 里四个工具自己报的名字；② `agent::authorize_and_execute` 判定
/// 「这次调用是不是宿主文件工具」。两边引同一份常量，就不会出现「改了名字、
/// 边界检查悄悄失效」这种最难查的错位（工具照样能用，只是边界没了）。
pub(crate) const HOST_FILE_TOOLS: [&str; 4] = ["read_file", "write_file", "list_dir", "remove"];

/// 一个宿主文件工具的**访问意图**：只读（读文件 / 列目录）还是落盘（写 / 删）。
///
/// 与 [`HOST_FILE_TOOLS`] 同源（按同一张表的下标取名字），所以改了名字也不会让判定
/// 悄悄失效 —— 与那张表的注释是同一个理由。名字对不上（理论上不会发生）时按
/// **落盘**处理：保守方向永远是「宁可多挡一次，别少挡一次」。
pub(crate) fn host_tool_access(name: &str) -> crate::workplace::Access {
    if name == HOST_FILE_TOOLS[0] /* read_file */ || name == HOST_FILE_TOOLS[2] /* list_dir */ {
        crate::workplace::Access::Read
    } else {
        crate::workplace::Access::Write
    }
}

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
