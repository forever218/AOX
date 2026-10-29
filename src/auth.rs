//! Authorization policy (the Policy seam).
//!
//! Turns "should this tool call be allowed?" from a hard-coded three-tier check
//! into a replaceable policy object: `Runtime` only holds an `Arc<dyn Policy>`
//! and no longer knows how the decision is computed.
//!
//! Phase 3 only "carves the seam + moves the code verbatim": `ConfigPolicy` is
//! the old `AuthPolicy`, and the three tiers (auto / ask / deny) plus the
//! override precedence (**tool name > plugin name > category default**) are
//! unchanged, not a single line. Adding another policy source later (a policy
//! file, a plugin-provided policy, ...) means implementing a new `Policy`, not
//! editing this one.

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
///
/// `Send + Sync`: `Runtime` is shared between the agent thread and the GUI main
/// thread, so a policy object must be holdable across threads.
pub trait Policy: Send + Sync {
    /// `caps`: the **capabilities requested** by this call.
    ///
    /// Phase 3 reserved the parameter slot and phase 4 still passed an empty
    /// slice (neither of those two configs is a "capability request"), but
    /// **phase 6 · 6b actually fills it in**: a component declares the
    /// capabilities it wants in `[contributes] caps`, and the host passes that
    /// plugin tool's request straight through here — so "a component requests a
    /// capability" lands in the very same authorization decision (principle P7),
    /// with no second decision path.
    fn level_for(&self, kind: &ToolKind, tool_name: &str, caps: &[String]) -> AuthLevel;
}

/// The host's **command-line** tools — the names `[auth] shell` governs.
///
/// A name list rather than "every `ToolKind::Host` tool": `read_file` / `write_file` are host tools
/// too, and putting a command line and a file write behind one switch would be wrong in both
/// directions (someone who tightens the shell should not suddenly be prompted on every read).
const SHELL_TOOLS: [&str; 3] = [
    crate::tool::shell::SHELL_TOOL,
    crate::tool::bash::BASH_TOOL,
    crate::tool::job::JOB_TOOL,
];

/// The host's **network** tools — the names `[auth] web` governs.
///
/// Same shape and the same reasoning as [`SHELL_TOOLS`]: a name list, not "every `ToolKind::Host`
/// tool", because one switch for "reach the internet" must not also gate `read_file`. It is a list
/// of one, and it stays a list: a tier now means a second network tool inherits it instead of
/// needing an override per tool name later.
const NET_TOOLS: [&str; 1] = [crate::tool::web::WEB_TOOL];

/// The policy decided by `config.toml [auth]`: overrides win (by tool name, then
/// by plugin name), otherwise the tool's category default applies.
///
/// Override keys come in two forms (since phase 4):
/// - **a full name** (`shell_exec`) — an exact hit;
/// - **a prefix with a trailing `*`** (`fs_*`) — hits every name starting with
///   that prefix.
///
/// Wildcards only look at the prefix and don't respect separators: `fs*` will
/// also hit `fsx`, so if you want to fence off just one group of tools, remember
/// to include the underscore (`fs_*`). A lone `*` is "catch-all".
pub struct ConfigPolicy {
    meta: AuthLevel,
    plugin: AuthLevel,
    /// The **command-line** tools' own tier (`[auth] shell`). Separate from `plugin` because what it
    /// gates is not "a tool that does one job" but "arbitrary programs, plus the ability to read
    /// every file the user can read". See [`SHELL_TOOLS`].
    shell: AuthLevel,
    /// The **network** tools' own tier (`[auth] web`). See [`NET_TOOLS`].
    web: AuthLevel,
    overrides: HashMap<String, AuthLevel>,
    /// The overrides carrying a `*` (stored as the prefix with `*` stripped),
    /// sorted by prefix **longest first** — the more specific one hits first:
    /// `fs_read_*` must beat `fs_*`.
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
        // The sort serves two purposes: longer prefixes win (the more specific
        // rule), and it makes the decision independent of HashMap's random
        // iteration order — the same config must yield the same answer every time.
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
        // ① Tool name (exact → wildcard)
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
        // ③ Capability request (first filled in phase 6 · 6b, principle P7): a
        //    component declares "what capability I want" in `[contributes] caps`,
        //    and such a call is **always asked once** — it must not silently ride
        //    the category default. E.g. `[auth] plugin_tools = "auto"` means
        //    "ordinary plugins may run freely", which should not become "and by
        //    the way the right to write files is granted too". The position is
        //    deliberate: after the override, because a hand-written override is
        //    an explicit stance and deserves to speak louder than a component's
        //    request.
        //
        //    Only **plugin tools** are affected: a capability request can only
        //    come from a component, whereas meta tools belong to the host and
        //    request on nobody's behalf. This extra check exists so that
        //    "passing it makes it take effect" cannot accidentally reach meta
        //    tools.
        if matches!(kind, ToolKind::Plugin(_)) && !caps.is_empty() {
            return AuthLevel::Ask;
        }
        // ④ Only when none of the above hit do we fall back to the category default
        //
        //    Host file tools (`ToolKind::Host`) follow the **same tier as plugin
        //    tools**: both are "let something else do a job" rather than "modify
        //    the host itself". So under `plugin_tools = "auto"` (the default) file
        //    tools are auto too — consistent with plugin tools of the `fs_*` kind,
        //    so the model behaves the same whichever tool it picks; to tighten one
        //    specifically, use a name override (e.g. `read_file = "ask"`), which
        //    step ① will hit first.
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

    /// The three-level precedence **did not change by a single word** once
    /// wildcards were added: exact tool name > longer prefix > shorter prefix >
    /// plugin default.
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

    /// The capability slot **only affects the plugin-tool path**: meta tools
    /// belong to the host, request capabilities on nobody's behalf and must not
    /// be caught in the crossfire (this pins that "passing it makes it take
    /// effect" cannot accidentally reach meta tools).
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
