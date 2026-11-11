//! Authorization policy (the Policy seam).
//! Turns "should this tool call be allowed?" from a hard-coded three-tier check into a replaceable policy object...
//! Phase 3 only "carves the seam + moves the code verbatim": `ConfigPolicy` is the old `AuthPolicy`, and the three...

use crate::config::AuthConfig;
use crate::tool::ToolKind;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthLevel {
    /// Allow outright.
    Auto,
    /// Ask the user.
    Ask,
    /// Deny outright.
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

/// The authorization policy: answers "should this tool call be auto, ask or deny".
pub trait Policy: Send + Sync {
    /// `caps`: the capabilities requested by this call.
    fn level_for(&self, kind: &ToolKind, tool_name: &str, caps: &[String]) -> AuthLevel;
}

/// The host's command-line tools — the names `auth shell` governs.
const SHELL_TOOLS: [&str; 3] = [
    crate::tool::shell::SHELL_TOOL,
    crate::tool::bash::BASH_TOOL,
    crate::tool::job::JOB_TOOL,
];

/// The host's network tools — the names `auth web` governs.
const NET_TOOLS: [&str; 1] = [crate::tool::web::WEB_TOOL];

/// The policy decided by `config.toml auth`: overrides win (by tool name, then by plugin name), otherwise...
pub struct ConfigPolicy {
    meta: AuthLevel,
    plugin: AuthLevel,
    /// The command-line tools' own tier (`auth shell`).
    shell: AuthLevel,
    /// The **network** tools' own tier (`[auth] web`). See [`NET_TOOLS`].
    web: AuthLevel,
    overrides: HashMap<String, AuthLevel>,
    /// The overrides carrying a `*` (stored as the prefix with `*` stripped), sorted by prefix longest first —...
    wildcards: Vec<(String, AuthLevel)>,
}

impl ConfigPolicy {
    pub fn from_config(cfg: &AuthConfig) -> Self {
        let mut overrides = HashMap::new();
        let mut wildcards: Vec<(String, AuthLevel)> = Vec::new();
        for (k, v) in &cfg.overrides {
            let level = AuthLevel::parse(v);
            let key = k.trim();
            match key.strip_suffix('*') {
                Some(prefix) => wildcards.push((prefix.to_string(), level)),
                None => {
                    overrides.insert(key.to_string(), level);
                }
            }
        }
        // The sort serves two purposes: longer prefixes win (the more specific rule), and it makes the decision...
        wildcards.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Self {
            meta: AuthLevel::parse(&cfg.meta_tools),
            plugin: AuthLevel::parse(&cfg.plugin_tools),
            shell: AuthLevel::parse(&cfg.shell),
            web: AuthLevel::parse(&cfg.web),
            overrides,
            wildcards,
        }
    }

    /// Look up a name (tool name or plugin name): **exact first, then wildcard**.
    fn lookup(&self, name: &str) -> Option<AuthLevel> {
        if let Some(l) = self.overrides.get(name) {
            return Some(*l);
        }
        self.wildcards
            .iter()
            .find(|(prefix, _)| name.starts_with(prefix.as_str()))
            .map(|(_, l)| *l)
    }
}

impl Policy for ConfigPolicy {
    fn level_for(&self, kind: &ToolKind, tool_name: &str, caps: &[String]) -> AuthLevel {
        if let Some(l) = self.lookup(tool_name) {
            return l;
        }
        // ② Plugin name (exact → wildcard) — equally a "stance the user wrote
        //    down explicitly", so it outranks the capability slot
        let plugin_hit = match kind {
            ToolKind::Plugin(plugin_name) => self.lookup(plugin_name),
            // Neither meta tools nor host file tools have a "plugin name" layer
            ToolKind::Meta | ToolKind::Host => None,
        };
        if let Some(l) = plugin_hit {
            return l;
        }
        // Capability request (first filled in phase 6 · 6b, principle P7): a component declares "what capability I want"...
        if matches!(kind, ToolKind::Plugin(_)) && !caps.is_empty() {
            return AuthLevel::Ask;
        }
        // Only when none of the above hit do we fall back to the category default.
        match kind {
            ToolKind::Meta => self.meta,
            // The command line gets its own tier: it is neither "modify the host" nor "one designed
            // job". `[auth.overrides] shell = "auto"` (step ①) still wins over this.
            ToolKind::Host if SHELL_TOOLS.contains(&tool_name) => self.shell,
            // The network's own tier, for the same reason: one HTTP request is not "a designed job"
            // in the plugin sense, and it is the only capability that leaves the machine.
            ToolKind::Host if NET_TOOLS.contains(&tool_name) => self.web,
            ToolKind::Host | ToolKind::Plugin(_) => self.plugin,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AuthConfig {
        AuthConfig {
            meta_tools: "ask".into(),
            plugin_tools: "deny".into(),
            shell: "ask".into(),
            web: "auto".into(),
            overrides: [
                ("create_plugin".to_string(), "auto".to_string()),
                ("myplugin".to_string(), "ask".to_string()),
            ]
            .into_iter()
            .collect(),
        }
    }

    /// Carving the seam is a pure refactor — this pins down the original
    /// precedence: tool-name override > plugin-name override > category default.
    #[test]
    fn override_beats_category_defaults() {
        let p = ConfigPolicy::from_config(&cfg());
        // ① A tool-name override wins first, beating even the category default
        assert_eq!(p.level_for(&ToolKind::Meta, "create_plugin", &[]), AuthLevel::Auto);
        // ② Next comes a plugin-name override
        assert_eq!(
            p.level_for(&ToolKind::Plugin("myplugin".into()), "myplugin_fs", &[]),
            AuthLevel::Ask
        );
        // ③ With no override, fall back to the category default
        assert_eq!(p.level_for(&ToolKind::Meta, "list_plugins", &[]), AuthLevel::Ask);
        assert_eq!(p.level_for(&ToolKind::Plugin("other".into()), "other_do", &[]), AuthLevel::Deny);
    }

    /// A tool-name override must not be stolen by a plugin-name override — the
    /// two lookups must not be swapped.
    #[test]
    fn tool_name_wins_over_plugin_name() {
        let mut c = cfg();
        c.overrides.insert("myplugin".into(), "deny".into());
        c.overrides.insert("myplugin_fs".into(), "auto".into());
        let p = ConfigPolicy::from_config(&c);
        assert_eq!(
            p.level_for(&ToolKind::Plugin("myplugin".into()), "myplugin_fs", &[]),
            AuthLevel::Auto,
            "when a tool and a plugin share a name and both have an override, the tool wins"
        );
    }

    /// The factory stance: nothing is special-cased into a prompt.
    #[test]
    fn the_factory_policy_asks_for_nothing() {
        let p = ConfigPolicy::from_config(&AuthConfig::default());
        let shell = crate::tool::shell::SHELL_TOOL;
        let cases: [(ToolKind, &str); 6] = [
            (ToolKind::Meta, "create_plugin"),
            (ToolKind::Meta, "remove_plugin"),
            (ToolKind::Host, shell),
            (ToolKind::Host, "write_file"),
            (ToolKind::Host, crate::tool::web::WEB_TOOL),
            (ToolKind::Plugin("mine".into()), "mine_do"),
        ];
        for (kind, name) in cases {
            assert_eq!(p.level_for(&kind, name, &[]), AuthLevel::Auto, "{name} must not ask");
        }
        // A component's capability request is still an ask: that is the component asking the user
        // for a capability, not an ordinary tool call, and `[auth.overrides]` can lift it.
        let caps = ["fs_write".to_string()];
        assert_eq!(
            p.level_for(&ToolKind::Plugin("comp".into()), "comp_do", &caps),
            AuthLevel::Ask
        );
    }

    /// The aliases and casing of the three tier strings: changing them would
    /// silently turn a value the user wrote in config into ask.
    #[test]
    fn level_parse_accepts_aliases_case_insensitively() {
        for s in ["auto", "AUTO", " allow ", "always"] {
            assert_eq!(AuthLevel::parse(s), AuthLevel::Auto, "{s}");
        }
        for s in ["deny", "DENY", "never"] {
            assert_eq!(AuthLevel::parse(s), AuthLevel::Deny, "{s}");
        }
        for s in ["ask", "whatever", ""] {
            assert_eq!(AuthLevel::parse(s), AuthLevel::Ask, "{s:?}");
        }
    }

    fn cfg_with_overrides(pairs: &[(&str, &str)]) -> AuthConfig {
        AuthConfig {
            meta_tools: "ask".into(),
            plugin_tools: "auto".into(),
            shell: "ask".into(),
            web: "auto".into(),
            overrides: pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
        }
    }

    /// The trailing-`*` prefix wildcard: block "a class of tools" in one go.
    #[test]
    fn wildcard_override_matches_by_prefix() {
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[("fs_*", "deny")]));
        assert_eq!(p.level_for(&ToolKind::Plugin("fs".into()), "fs_read_file", &[]), AuthLevel::Deny);
        assert_eq!(p.level_for(&ToolKind::Plugin("fs".into()), "fs_write_file", &[]), AuthLevel::Deny);
        // What the wildcard doesn't catch still follows the original precedence
        // (plugin default auto)
        assert_eq!(p.level_for(&ToolKind::Plugin("net".into()), "net_fetch", &[]), AuthLevel::Auto);
    }

    /// The three-level precedence did not change by a single word once wildcards were added: exact tool name > longer...
    #[test]
    fn exact_key_beats_wildcard_and_longer_prefix_wins() {
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[
            ("fs_*", "deny"),
            ("fs_read_*", "auto"),
            ("fs_read_file", "ask"),
        ]));
        let kind = ToolKind::Plugin("fs".into());
        // ① An exact key wins first, beating even a longer prefix
        assert_eq!(p.level_for(&kind, "fs_read_file", &[]), AuthLevel::Ask);
        // ② With no exact key, the longer (more specific) prefix wins
        assert_eq!(p.level_for(&kind, "fs_read_dir", &[]), AuthLevel::Auto);
        // ③ Only when neither matches do we fall back to the shorter prefix
        assert_eq!(p.level_for(&kind, "fs_write_file", &[]), AuthLevel::Deny);
        // ④ Plugin names take wildcards too — `myplugin*` hits the plugin name myplugin
        let p2 = ConfigPolicy::from_config(&cfg_with_overrides(&[("myplugin*", "deny")]));
        assert_eq!(
            p2.level_for(&ToolKind::Plugin("myplugin".into()), "unrelated", &[]),
            AuthLevel::Deny
        );
    }

    /// A lone `*` = catch-all — if it can be written, it should take effect
    /// literally.
    #[test]
    fn bare_star_catches_everything() {
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[("*", "deny")]));
        assert_eq!(p.level_for(&ToolKind::Meta, "list_plugins", &[]), AuthLevel::Deny);
        assert_eq!(
            p.level_for(&ToolKind::Plugin("any".into()), "any_tool", &[]),
            AuthLevel::Deny
        );
    }

    /// Wildcards are **prefix** matches and don't respect separators — this pins
    /// the semantics so nobody later thinks they're path-aware.
    #[test]
    fn wildcard_is_a_plain_prefix() {
        // Without `*` it's an exact match: the key `fs` only hits things named
        // `fs`, not fs_read_file
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[("fs", "deny")]));
        assert_eq!(
            p.level_for(&ToolKind::Plugin("other".into()), "fs_read_file", &[]),
            AuthLevel::Auto
        );

        // Written as `fs*` it matches by prefix — hitting `fsx_do` too (that's
        // what "prefix only" means)
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[("fs*", "deny")]));
        assert_eq!(p.level_for(&ToolKind::Plugin("x".into()), "fs_read_file", &[]), AuthLevel::Deny);
        assert_eq!(p.level_for(&ToolKind::Plugin("x".into()), "fsx_do", &[]), AuthLevel::Deny);
    }

    /// Capability request (first filled in phase 6 · 6b): a component that
    /// declares caps has all its tool calls go through asking.
    #[test]
    fn declared_caps_force_ask_over_plugin_default() {
        // The plugin category default is auto (see cfg_with_overrides) —
        // ordinary plugins need no ask
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[]));
        let kind = ToolKind::Plugin("comp".into());
        assert_eq!(p.level_for(&kind, "comp_do", &[]), AuthLevel::Auto);
        // But once a component requests a capability, it must get a nod from the user
        assert_eq!(p.level_for(&kind, "comp_do", &["fs_write".to_string()]), AuthLevel::Ask);
    }

    /// A hand-written override beats a capability request — it's an explicit
    /// stance and deserves to speak louder than a component's request.
    #[test]
    fn explicit_override_beats_declared_caps() {
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[("comp", "auto"), ("comp_do", "deny")]));
        let kind = ToolKind::Plugin("comp".into());
        let caps = ["fs_write".to_string()];
        assert_eq!(p.level_for(&kind, "comp_do", &caps), AuthLevel::Deny, "the tool-name override has top priority");
        assert_eq!(p.level_for(&kind, "comp_other", &caps), AuthLevel::Auto, "the plugin-name override comes next");
    }

    /// The capability slot only affects the plugin-tool path: meta tools belong to the host, request capabilities...
    #[test]
    fn caps_never_apply_to_meta_tools() {
        // meta_tools = deny: if the caps check leaked onto this path, the result
        // would become Ask
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[("*", "deny")]));
        assert_eq!(
            p.level_for(&ToolKind::Meta, "list_plugins", &["fs_write".to_string()]),
            AuthLevel::Deny,
            "the fallback override hits a meta-tool; the caps bit must not turn it into ask"
        );
    }
}
