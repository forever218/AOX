use crate::config::AuthConfig;
use crate::tool::ToolKind;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthLevel {
    /// 直接放行
    Auto,
    /// 询问用户
    Ask,
    /// 直接拒绝
    Deny,
}

impl AuthLevel {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "allow" | "always" => Self::Auto,
            "deny" | "never" => Self::Deny,
            _ => Self::Ask,
        }
    }
}

/// 授权策略：overrides 优先（按工具名，其次按插件名），否则按工具类别取默认值。
pub struct AuthPolicy {
    meta: AuthLevel,
    plugin: AuthLevel,
    overrides: HashMap<String, AuthLevel>,
}

impl AuthPolicy {
    pub fn from_config(cfg: &AuthConfig) -> Self {
        Self {
            meta: AuthLevel::parse(&cfg.meta_tools),
            plugin: AuthLevel::parse(&cfg.plugin_tools),
            overrides: cfg.overrides.iter().map(|(k, v)| (k.clone(), AuthLevel::parse(v))).collect(),
        }
    }

    pub fn level_for(&self, kind: &ToolKind, tool_name: &str) -> AuthLevel {
        if let Some(l) = self.overrides.get(tool_name) {
            return *l;
        }
        match kind {
            ToolKind::Meta => self.meta,
            ToolKind::Plugin(plugin_name) => {
                self.overrides.get(plugin_name).copied().unwrap_or(self.plugin)
            }
        }
    }
}
