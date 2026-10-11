//! Meta-tools: the entry point to the system's "self-growth". The initial registry holds only these
//! three tools.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolKind, ToolOutput};
use crate::plugin;
use crate::runtime::Runtime;

pub fn meta_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(CreatePluginTool),
        Arc::new(UpdatePluginTool),
        Arc::new(ListPluginsTool),
        Arc::new(RemovePluginTool),
    ]
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
        crate::i18n::t("Write and inject a new tool plugin (self-growth). A plugin is a standalone subprocess that talks over the stdio line-JSON protocol (see the system prompt and plugins/sdk/PROTOCOL.md). Once created, the tools it declares are immediately callable and persisted under plugins/. If a previous creation of the same name failed and left a directory behind, it is cleaned up and rebuilt — just reuse the original plugin name.")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": crate::i18n::t("plugin name: lowercase letters / digits / underscore, 1-40 chars")},
                "language": {"type": "string", "enum": ["python", "node", "rust"],
                             "description": crate::i18n::t("plugin language. python is recommended (fast startup, built-in json); rust is limited to a single-file build using std")},
                "description": {"type": "string", "description": crate::i18n::t("one sentence describing what capability this plugin provides")},
                "source": {"type": "string", "description": crate::i18n::t("full plugin source code")}
            },
            "required": ["name", "language", "description", "source"]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return ToolOutput::err(crate::i18n::t("missing argument name"));
        };
        let Some(language) = args.get("language").and_then(|v| v.as_str()) else {
            return ToolOutput::err(crate::i18n::t("missing argument language"));
        };
        let description = args.get("description").and_then(|v| v.as_str()).unwrap_or("");
        let Some(source) = args.get("source").and_then(|v| v.as_str()) else {
            return ToolOutput::err(crate::i18n::t("missing argument source"));
        };
        match plugin::create_plugin(rt, name, language, description, source).await {
            Ok(tools) => ToolOutput::ok(crate::tf!(
                "plugin {} created; registered {} tool(s): {}. You can call them right away.",
                name,
                tools.len(),
                tools.join(", ")
            )),
            Err(e) => ToolOutput::err(crate::tf!("failed to create plugin: {}", format!("{e:#}"))),
        }
    }
}

// ---------------------------------------------------------------- update_plugin

struct UpdatePluginTool;

#[async_trait]
impl Tool for UpdatePluginTool {
    fn name(&self) -> &str {
        "update_plugin"
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Meta
    }
    fn description(&self) -> &str {
        crate::i18n::t("Change the source of an existing plugin and hot-swap it in place: the old process is retired and its tools are re-registered with the new code, all in one step — no remove_plugin first, and the capability never goes away. Use this whenever your first version of a plugin was wrong or incomplete and you want to fix it. Always pass the full new source, not a patch. To change a plugin created in this session, or one you can see in list_plugins, call this instead of editing plugins/<name>/main.py with write_file: a running plugin keeps its old process, so a plain file edit appears to have no effect.")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": crate::i18n::t("name of the existing plugin to update (it must already exist; to make a new one use create_plugin)")},
                "source": {"type": "string", "description": crate::i18n::t("the plugin's full new source code (complete replacement; the language and entry file stay as they were)")},
                "description": {"type": "string", "description": crate::i18n::t("optional new one-sentence description; omit to keep the current one")}
            },
            "required": ["name", "source"]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return ToolOutput::err(crate::i18n::t("missing argument name"));
        };
        let Some(source) = args.get("source").and_then(|v| v.as_str()) else {
            return ToolOutput::err(crate::i18n::t("missing argument source"));
        };
        let description = args.get("description").and_then(|v| v.as_str());
        match plugin::update_plugin(rt, name, source, description).await {
            Ok(tools) => ToolOutput::ok(crate::tf!(
                "plugin {} updated and hot-swapped; it now registers {} tool(s): {}. The new code is live right away.",
                name,
                tools.len(),
                tools.join(", ")
            )),
            Err(e) => ToolOutput::err(crate::tf!("failed to update plugin: {}", format!("{e:#}"))),
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
        crate::i18n::t("List every currently loaded plugin and the tools it provides. Before creating a new plugin, call this tool first to check whether a reusable capability already exists.")
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let plugins = rt.plugins.lock().await;
        if plugins.is_empty() {
            return ToolOutput::ok(
                crate::i18n::t("No plugins are loaded right now. If the task needs tool capabilities, create one with create_plugin.").to_string(),
            );
        }
        let mut lines = vec![crate::tf!("{} plugin(s) loaded:", plugins.len())];
        let mut items: Vec<_> = plugins.values().collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        for p in items {
            lines.push(format!("- {} [{}]: {}", p.name, p.language, p.description));
            for t in p.tools() {
                lines.push(crate::tf!("    tool {}: {}", t.name, t.description));
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
        crate::i18n::t("Unload and delete a plugin: stop its process, unregister its tools, and delete its directory under plugins/ (even if the plugin is not loaded, its leftover directory is cleaned up). Use it to rebuild a broken plugin or clear capabilities you no longer need.")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": crate::i18n::t("name of the plugin to delete")}
            },
            "required": ["name"]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return ToolOutput::err(crate::i18n::t("missing argument name"));
        };
        match plugin::remove_plugin(rt, name).await {
            Ok(tools) => ToolOutput::ok(crate::tf!(
                "plugin {} deleted; unregistered tools: {}",
                name,
                if tools.is_empty() { crate::i18n::t("(none)").to_string() } else { tools.join(", ") }
            )),
            Err(e) => ToolOutput::err(crate::tf!("failed to delete plugin: {}", format!("{e:#}"))),
        }
    }
}
