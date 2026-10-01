//! 元工具：系统「自我增长」的入口。初始注册表里只有这三个工具。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolKind, ToolOutput};
use crate::plugin;
use crate::runtime::Runtime;

pub fn meta_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(CreatePluginTool), Arc::new(ListPluginsTool), Arc::new(RemovePluginTool)]
}

// ---------------------------------------------------------------- create_plugin

struct CreatePluginTool;

#[async_trait]
impl Tool for CreatePluginTool {
    fn name(&self) -> &str {
        "create_plugin"
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Meta
    }
    fn description(&self) -> &str {
        "编写并注入一个新工具插件（自我增长）。插件是独立子进程，通过 stdio 行 JSON 协议通信（协议见系统提示与 plugins/sdk/PROTOCOL.md）。创建成功后其声明的工具立即可调用，并持久化到 plugins/ 目录。若同名插件上次创建失败留下残留目录，会自动清理后重建，直接复用原插件名即可。"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "插件名，小写字母/数字/下划线，1-40 字符"},
                "language": {"type": "string", "enum": ["python", "node", "rust"],
                             "description": "插件语言。推荐 python（启动快、json 内置）；rust 仅可用 std 单文件编译"},
                "description": {"type": "string", "description": "一句话说明这个插件提供什么能力"},
                "source": {"type": "string", "description": "完整插件源码"}
            },
            "required": ["name", "language", "description", "source"]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return ToolOutput::err("缺少参数 name");
        };
        let Some(language) = args.get("language").and_then(|v| v.as_str()) else {
            return ToolOutput::err("缺少参数 language");
        };
        let description = args.get("description").and_then(|v| v.as_str()).unwrap_or("");
        let Some(source) = args.get("source").and_then(|v| v.as_str()) else {
            return ToolOutput::err("缺少参数 source");
        };
        match plugin::create_plugin(rt, name, language, description, source).await {
            Ok(tools) => ToolOutput::ok(format!(
                "插件 {name} 创建成功，已注册 {} 个工具: {}。现在可以直接调用它们。",
                tools.len(),
                tools.join(", ")
            )),
            Err(e) => ToolOutput::err(format!("插件创建失败: {e:#}")),
        }
    }
}

// ---------------------------------------------------------------- list_plugins

struct ListPluginsTool;

#[async_trait]
impl Tool for ListPluginsTool {
    fn name(&self) -> &str {
        "list_plugins"
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Meta
    }
    fn description(&self) -> &str {
        "列出当前已加载的所有插件及其提供的工具。创建新插件前应先调用本工具检查是否已有可复用的能力。"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let plugins = rt.plugins.lock().await;
        if plugins.is_empty() {
            return ToolOutput::ok(
                "当前没有任何已加载的插件。如任务需要工具能力，请用 create_plugin 创建。".to_string(),
            );
        }
        let mut lines = vec![format!("已加载 {} 个插件:", plugins.len())];
        let mut items: Vec<_> = plugins.values().collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        for p in items {
            lines.push(format!("- {} [{}]: {}", p.name, p.language, p.description));
            for t in &p.tool_defs {
                lines.push(format!("    工具 {}: {}", t.name, t.description));
            }
        }
        ToolOutput::ok(lines.join("\n"))
    }
}

// ---------------------------------------------------------------- remove_plugin

struct RemovePluginTool;

#[async_trait]
impl Tool for RemovePluginTool {
    fn name(&self) -> &str {
        "remove_plugin"
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Meta
    }
    fn description(&self) -> &str {
        "卸载并删除一个插件：停止其进程、注销其工具、删除 plugins/ 下对应目录（即使插件未被加载，也会清理其残留目录）。用于重建坏掉的插件或清理不再需要的能力。"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "要删除的插件名"}
            },
            "required": ["name"]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return ToolOutput::err("缺少参数 name");
        };
        match plugin::remove_plugin(rt, name).await {
            Ok(tools) => ToolOutput::ok(format!(
                "插件 {name} 已删除，注销工具: {}",
                if tools.is_empty() { "（无）".to_string() } else { tools.join(", ") }
            )),
            Err(e) => ToolOutput::err(format!("删除插件失败: {e:#}")),
        }
    }
}
