//! The **advertising boundary** of the tool list: which schemas go into a given request.
//!
//! # Why it exists
//!
//! The registry and "what the model should see" are two different things. The registry grows with
//! plugins (a single `fs` plugin is 10 tools), and **every request sends all of those schemas
//! verbatim**: tokens are the visible cost, but the real one is picking the wrong tool — the longer
//! the list, the lower the chance each tool is chosen correctly. So "advertising" is split from
//! "capability": every capability stays, advertising happens on demand.
//!
//! # The line is `ToolKind`, not a list of names
//!
//! The host's own tools (`Meta` / `Host`) are **always** listed; only `Plugin(_)` may be held back.
//! Writing that as a structural check instead of a name table has two reasons:
//!
//! ① A name table drifts. Add a host tool, forget to register it there, and the new tool is
//!    "invisible to the model" — the hardest kind of failure to find (the tool works; nobody knows
//!    it exists).
//! ② Host tools are **exactly 14**, plugin tools are **unbounded**. Keeping the list stable — and
//!    the prefix cache warm — is about the former. However many plugins the model builds, they can
//!    never push the host's own vocabulary out.
//!
//! # Hidden != unavailable
//!
//! `Runtime::execute` / `Agent::authorize_and_execute` resolve names against the **registry**, not
//! against this list, so a model that remembers a hidden tool **still succeeds** when it calls it.
//! What is switched off here is the advertising slot, not the capability — hiding must never create
//! a new failure mode, or it becomes a breeding ground for bugs instead of an optimisation.
//!
//! # The three tiers
//!
//! - **core**: `ToolKind::Meta | Host`, always visible (14 tools).
//! - **enabled**: names pinned in `[tools] enabled`, always visible (plugin tools included).
//! - **discovered**: added by a `find_tools` hit, or automatically after a successful
//!   `create_plugin` / `update_plugin`. Visible for the rest of this conversation; **never
//!   persisted** — a new conversation or a resume clears it (it is a projection, the same kind of
//!   thing as `compact::ViewCache`).
//!
//! A name outside those three tiers appears in no request at all, yet can still be called.

use std::collections::BTreeSet;

/// One conversation's visibility policy (held by `Agent`, survives rounds, resets with the
/// conversation).
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Nothing is hidden while the registry is **at or below** this size (the old behaviour).
    /// `0` = never hide anything, ever.
    inline_max: usize,
    /// `[tools] enabled`: tools the user pinned as always residents. `reset` leaves them alone.
    pinned: BTreeSet<String>,
    /// Tools this conversation pulled in via `find_tools`, or because the plugin that provides them
    /// was just built. Cleared by `reset`.
    discovered: BTreeSet<String>,
}

impl Scope {
    /// Build one from `[tools]`. Names are trimmed and empty strings dropped — one stray blank line
    /// in a config file must not become a ghost entry that can never match a tool.
    pub fn from_config(cfg: &crate::config::ToolsConfig) -> Self {
        Self {
            inline_max: cfg.inline_max,
            pinned: cfg
                .enabled
                .iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
            discovered: BTreeSet::new(),
        }
    }

    /// Whether this request is in "on demand" mode.
    ///
    /// Decided by the **live registry size**, not by a one-off switch: install a plugin and the
    /// registry crosses the threshold, so the very next round starts hiding; remove it and the list
    /// falls back to full on its own. No state to keep in sync means no "forgot to flip the flag"
    /// class of bug.
    pub fn hides(&self, registered: usize) -> bool {
        self.inline_max > 0 && registered > self.inline_max
    }

    /// Whether this name belongs in the list. `plugin == false` (a host tool) is always true — see
    /// the module docs.
    pub fn advertises(&self, name: &str, plugin: bool) -> bool {
        !plugin || self.pinned.contains(name) || self.discovered.contains(name)
    }

    /// Add a name to `discovered`. `true` means it went in **for the first time** (the caller uses
    /// that to word "this is new"; a repeated hit must not say "now available" a second time).
    pub fn enable(&mut self, name: &str) -> bool {
        if self.pinned.contains(name) {
            // Already listed; adding it again must not report "new".
            return false;
        }
        self.discovered.insert(name.to_owned())
    }

    /// Called when the conversation changes: clears `discovered` only, config pins stay.
    pub fn reset(&mut self) {
        self.discovered.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolsConfig;

    fn scope(inline_max: usize, enabled: &[&str]) -> Scope {
        Scope::from_config(&ToolsConfig {
            inline_max,
            enabled: enabled.iter().map(|s| s.to_string()).collect(),
        })
    }

    #[test]
    fn host_tools_are_never_hidden() {
        let s = scope(4, &[]);
        assert!(s.hides(12), "12 tools cross the threshold of 4 => on-demand mode");
        // Once on-demand, host tools are still all there.
        for n in ["read_file", "task_state", "create_plugin"] {
            assert!(s.advertises(n, false), "{n} is a host tool and must always be visible");
        }
        assert!(!s.advertises("fs_ls", true), "a plugin tool is not advertised by default");
    }

    #[test]
    fn plugin_tools_appear_only_when_enabled() {
        let mut s = scope(4, &[]);
        assert!(!s.advertises("fs_ls", true));
        assert!(s.enable("fs_ls"), "the first enable reports \"new\"");
        assert!(s.advertises("fs_ls", true));
        assert!(!s.enable("fs_ls"), "a second enable must not report \"new\" again");
    }

    #[test]
    fn below_the_threshold_nothing_is_hidden() {
        // The default threshold (24): 14 host tools plus a few plugins => full list, byte-identical
        // to the behaviour before this module existed.
        let s = scope(24, &[]);
        assert!(!s.hides(12));
        assert!(!s.hides(24), "at the threshold nothing is hidden either");
        assert!(s.hides(25));
    }

    #[test]
    fn zero_disables_hiding_entirely() {
        // The permanent escape hatch: 0 = never hide, however large the registry gets.
        let s = scope(0, &[]);
        assert!(!s.hides(1_000));
    }

    #[test]
    fn config_can_pin_a_plugin_tool() {
        let s = scope(24, &["fs_ls", " "]);
        assert!(s.advertises("fs_ls", true), "a config-pinned plugin tool is always visible");
        assert!(!s.advertises("fs_write", true));
    }

    #[test]
    fn reset_keeps_what_the_config_pinned() {
        let mut s = scope(24, &["fs_ls"]);
        s.enable("sysinfo_pro_overview");
        s.reset();
        assert!(s.advertises("fs_ls", true), "config pins survive across conversations");
        assert!(
            !s.advertises("sysinfo_pro_overview", true),
            "something discovered at runtime belonged to the previous conversation and must be dropped"
        );
    }
}
