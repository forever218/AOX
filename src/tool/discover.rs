//! `find_tools` — lets the model **call up** tools that are registered but not advertised.
//!
//! # Why a tool
//!
//! On-demand advertising (see [`super::scope`]) saves a few thousand tokens per round, at the price
//! of "the model no longer knows what else exists". There is exactly one way to pay that price back:
//! give it a **look in the catalogue**. Not "remind it every round how many tools are left" — that
//! is noise, and every sentence the model reads crowds out the actual task context; just one passive
//! entry point, used when needed.
//!
//! # Why it can't go through `Runtime::execute` itself
//!
//! Because "call up" means **changing this conversation's visibility policy**, and `Tool::call` only
//! receives `&Runtime`, never `&mut Agent`. So it gets the same treatment as `ask_user` /
//! `task_state`: registered in the tool table (otherwise the model cannot see the entry point), with
//! execution intercepted by [`crate::agent::Agent::authorize_and_execute`]. Its `call` is reached
//! only when "someone bypassed the Agent and called `rt.execute` directly", and then it can only
//! report the failure — succeeding silently would let the caller believe the tools were revealed.
//!
//! # What it returns
//!
//! Name + one line + **full parameters** for each hit. All three are needed: the name alone leaves
//! the model guessing at arguments (another round trip); name and description together still don't
//! let it call anything. Give everything at once and the next round can use them directly.
//!
//! A hit is **enabled immediately** (written into `Scope::discovered`); no second call to "confirm".
//! That is deliberate: two steps (search, then enable) save nothing, cost one extra round trip, and
//! add one more way to fail — "found it, forgot to enable it".

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::scope::Scope;
use super::{Tool, ToolKind, ToolOutput};
use crate::runtime::Runtime;

/// The tool name. `Agent::authorize_and_execute` and this module share one constant.
pub const FIND_TOOL: &str = "find_tools";

/// How many hits one call may reveal.
///
/// 8 is a rule of thumb: hit too many at once and "on demand" turns back into "everything" (while
/// still paying a round trip); hit too few and the model has to search again and again. The excess
/// is cut off in a stable name order — not ranked by a second "relevance" tier, because word
/// frequency in a description is a very poor ranking signal.
pub const MAX_HITS: usize = 8;

/// The host's tool-discovery tool.
pub fn find_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(FindToolsTool)]
}

pub(crate) struct FindToolsTool;

#[async_trait]
impl Tool for FindToolsTool {
    fn name(&self) -> &str {
        FIND_TOOL
    }
    fn kind(&self) -> ToolKind {
        // `Host`: it changes neither the host machine nor any file, only "which tools are visible in
        // this conversation" — the same tier as `task_state` => it follows `[auth] plugin_tools`
        // (auto by default). Pop an authorization dialog for a catalogue lookup and the model learns
        // to route around it.
        ToolKind::Host
    }
    fn access(&self) -> crate::workplace::Access {
        // It reads and writes no path at all. Declared `Read` so the workplace boundary waves it
        // through instead of scanning its arguments for nothing.
        crate::workplace::Access::Read
    }
    fn description(&self) -> &str {
        crate::i18n::t("Look up tools that are registered but NOT currently in your tool list (this happens with plugin tools once the registry is large), and put them into your list from the next round on. The query is matched case-insensitively against tool names and their one-line descriptions; several words must all match. Returns up to 8 matches with their full parameter schema. The host's own tools are always listed already — you never need this for read_file / write_file / edit_file / list_dir / remove / attach_file / create_plugin / update_plugin / list_plugins / remove_plugin / ask_user / task_state / verify.")
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": crate::i18n::t("Words to look for, e.g. \"fs\" or \"screen capture\".")},
            },
            "required": ["query"]
        })
    }

    async fn call(&self, _args: Value, _rt: &Arc<Runtime>) -> ToolOutput {
        // Only a call that bypassed the Agent lands here (`Runtime::execute` called directly). This
        // path has no conversation, so it has no visibility policy to change; pretending to succeed
        // is the worst answer — the caller would believe the tools had already appeared.
        ToolOutput::err(crate::i18n::t(
            "find_tools is executed by the agent loop, not by the tool registry: it changes which tools this conversation can see, and this path has no conversation.",
        ))
    }
}

/// Search and enable. Called by `Agent::authorize_and_execute` (it needs `&mut Scope`).
pub async fn search(rt: &Runtime, scope: &mut Scope, query: &str) -> ToolOutput {
    let tokens: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
    if tokens.is_empty() {
        return ToolOutput::err(crate::i18n::t(
            "find_tools needs a query: what capability are you looking for? (e.g. {\"query\": \"screen\"})",
        ));
    }

    let entries = rt.tool_entries().await;
    let needle = query.trim().to_lowercase();
    // A name the model can already see answers the question directly. **Exact** name only: a loose
    // match here would steal real queries (searching "file" would answer "read_file" and never look
    // at the hidden `fs_*` tools), so partial matches fall through to the search below.
    if let Some(visible) = entries
        .iter()
        .find(|e| scope.advertises(&e.name, e.plugin) && e.name.eq_ignore_ascii_case(&needle))
    {
        return ToolOutput::ok(crate::tf!(
            "{} is already in your tool list — call it directly, no lookup needed.",
            visible.name
        ));
    }
    // Candidates = whatever is not advertised right now (in practice the plugin tools that have
    // not been called up yet; host tools are always in the list).
    let hidden: Vec<&crate::runtime::ToolEntry> =
        entries.iter().filter(|e| !scope.advertises(&e.name, e.plugin)).collect();

    if hidden.is_empty() {
        // Nothing is held back: either the registry is small (full-list mode), or everything has
        // already been revealed.
        return ToolOutput::ok(crate::i18n::t(
            "Nothing is hidden right now: every registered tool is already in your tool list. Use list_plugins to see which plugins are loaded.",
        ));
    }

    let mut hits: Vec<(u8, &crate::runtime::ToolEntry)> = hidden
        .iter()
        .filter_map(|e| {
            let name = e.name.to_lowercase();
            let hay = format!("{} {}", name, e.description.to_lowercase());
            if !tokens.iter().all(|t| hay.contains(t.as_str())) {
                return None;
            }
            // A full name match ranks above a description-only hit; within a tier the name decides,
            // so the same query always returns the same order (the order goes into the context, and
            // an arbitrary order means arbitrary cache hits).
            let tier = if tokens.iter().all(|t| name.contains(t.as_str())) { 2 } else { 1 };
            Some((tier, *e))
        })
        .collect();

    if hits.is_empty() {
        // Getting here means "not in the list, and not held back either": the name was already
        // checked at the top of the function, so the honest answer is "nothing", plus the next step
        // (narrow the query / look at plugin names).
        return ToolOutput::err(crate::tf!(
            "no hidden tool matches \"{}\" ({} tool(s) are hidden right now). Try a shorter word — plugin tool names are usually prefixed by the plugin name, which list_plugins can show you.",
            query.trim(),
            hidden.len()
        ));
    }

    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    let total = hits.len();
    hits.truncate(MAX_HITS);
    for (_, e) in &hits {
        scope.enable(&e.name);
    }

    let mut out = crate::tf!(
        "{} matching tool(s) for \"{}\". They are in your tool list from the next round on — call them directly:\n\n",
        total.min(MAX_HITS),
        query.trim()
    );
    for (_, e) in &hits {
        out.push_str(&crate::tf!("- {} — {}\n", e.name, e.description));
        out.push_str(&crate::tf!("  parameters: {}\n", e.schema["function"]["parameters"]));
    }
    if total > MAX_HITS {
        out.push_str(&crate::tf!(
            "\n({} more matched but were not listed — narrow the query.)\n",
            total - MAX_HITS
        ));
    }
    ToolOutput::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ToolsConfig, WorkplaceConfig};

    /// A stand-in for a plugin tool. Real ones need a subprocess; what this feature reasons about is
    /// the **kind**, the name and the description — nothing else.
    struct Fake {
        name: &'static str,
        desc: &'static str,
        host: bool,
    }

    #[async_trait]
    impl Tool for Fake {
        fn name(&self) -> &str {
            self.name
        }
        fn kind(&self) -> ToolKind {
            if self.host {
                ToolKind::Host
            } else {
                ToolKind::Plugin("demo".into())
            }
        }
        fn description(&self) -> &str {
            self.desc
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {"path": {"type": "string"}}})
        }
        async fn call(&self, _args: Value, _rt: &Arc<Runtime>) -> ToolOutput {
            // Reachable on purpose: one test calls a **hidden** tool to prove hiding is advertising
            // only. If this ever stops being reachable, that test fails loudly.
            ToolOutput::ok("fake ran")
        }
    }

    const PLUGIN_TOOLS: [(&str, &str); 11] = [
        ("demo_ls", "List a directory tree under the workspace"),
        ("demo_read", "Read a text file from disk"),
        ("demo_write", "Write text to a file"),
        ("demo_move", "Move a path to another place"),
        ("demo_info", "Show information about a path"),
        ("demo_find", "Find files whose name matches"),
        ("demo_stat", "Print metadata for a path"),
        ("demo_cp", "Copy a path"),
        ("demo_rm", "Move a path to the recycle bin"),
        ("demo_mkdir", "Create a directory"),
        ("demo_touch", "Create an empty file"),
    ];

    /// A real runtime (real registry, real config) with host file tools + `find_tools` + the fake
    /// plugin tools. `inline_max` is deliberately tiny so the registry crosses it.
    async fn rt(tag: &str, inline_max: usize, plugin_tools: usize) -> (Arc<Runtime>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("aox-discover-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = Config::dummy();
        c.tools = ToolsConfig { inline_max, enabled: Vec::new() };
        c.workplace = WorkplaceConfig { mode: "fixed".into(), root: Some(dir.to_string_lossy().into()) };
        let rt = Runtime::new(c, dir.clone(), dir.join("plugins")).expect("the runtime should build");
        for t in crate::tool::hostfs::host_file_tools() {
            rt.register(t).await.unwrap();
        }
        for t in find_tools() {
            rt.register(t).await.unwrap();
        }
        for (name, desc) in PLUGIN_TOOLS.iter().take(plugin_tools) {
            rt.register(Arc::new(Fake { name, desc, host: false })).await.unwrap();
        }
        (rt, dir)
    }

    /// The names advertised in a scoped request (sorted, like the request itself).
    fn advertised(schemas: &[Value]) -> Vec<String> {
        let mut v: Vec<String> = schemas
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap_or("").to_string())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn plugin_tools_are_hidden_until_one_lookup_reveals_them() {
        // 7 host tools + find_tools + 5 plugin tools = 13 > 6 => on-demand mode.
        let (rt, dir) = rt("reveal", 6, 5).await;
        let mut scope = Scope::from_config(&rt.config.tools);

        let before = advertised(&rt.tool_schemas_scoped(&scope).await);
        assert!(
            before.iter().all(|n| !n.starts_with("demo_")),
            "no plugin tool may appear in the request: {before:?}"
        );
        assert!(before.contains(&FIND_TOOL.to_string()), "the discovery entry point must itself be listed");
        assert!(before.contains(&"read_file".to_string()), "host tools are always visible");

        // The full list is still there — what is hidden is advertising, not capability.
        assert_eq!(rt.tool_schemas().await.len(), 13);

        let out = search(&rt, &mut scope, "directory tree").await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("demo_ls"), "{}", out.content);
        assert!(out.content.contains("\"path\""), "a hit must carry its full parameters: {}", out.content);

        let after = advertised(&rt.tool_schemas_scoped(&scope).await);
        assert!(after.contains(&"demo_ls".to_string()), "it must be listed from the next round on: {after:?}");
        assert!(!after.contains(&"demo_read".to_string()), "a tool that did not match stays hidden: {after:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_small_registry_is_sent_whole() {
        // 6 + 1 + 2 = 9 <= 24 => byte-identical to the behaviour before this feature.
        let (rt, dir) = rt("small", 24, 2).await;
        let scope = Scope::from_config(&rt.config.tools);
        let scoped = serde_json::to_string(&rt.tool_schemas_scoped(&scope).await).unwrap();
        let all = serde_json::to_string(&rt.tool_schemas().await).unwrap();
        assert_eq!(scoped, all, "below the threshold nothing may be hidden (zero drift)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn zero_turns_the_feature_off_entirely() {
        let (rt, dir) = rt("off", 0, 5).await;
        let scope = Scope::from_config(&rt.config.tools);
        assert_eq!(
            serde_json::to_string(&rt.tool_schemas_scoped(&scope).await).unwrap(),
            serde_json::to_string(&rt.tool_schemas().await).unwrap(),
            "inline_max = 0 is the permanent escape hatch"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn every_word_must_match_and_case_does_not() {
        let (rt, dir) = rt("words", 6, 5).await;
        let mut scope = Scope::from_config(&rt.config.tools);

        let out = search(&rt, &mut scope, "DEMO WRITE").await;
        assert!(out.content.contains("demo_write"), "{}", out.content);
        assert!(!out.content.contains("demo_read"), "every word must match: {}", out.content);

        let none = search(&rt, &mut scope, "demo nonexistent").await;
        assert!(none.is_error);
        assert!(none.content.contains("no hidden tool matches"), "{}", none.content);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_empty_query_is_refused_with_an_example() {
        let (rt, dir) = rt("empty", 6, 5).await;
        let mut scope = Scope::from_config(&rt.config.tools);
        let out = search(&rt, &mut scope, "   ").await;
        assert!(out.is_error);
        assert!(out.content.contains("needs a query"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_name_that_is_already_listed_says_so_instead_of_the_empty_answer() {
        let (rt, dir) = rt("visible", 6, 5).await;
        let mut scope = Scope::from_config(&rt.config.tools);
        let out = search(&rt, &mut scope, "read_file").await;
        assert!(!out.is_error);
        assert!(out.content.contains("already in your tool list"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_answer_is_capped_and_says_how_much_was_left_out() {
        // All 11 plugin tools match "demo", the cap is 8 => 8 listed, 3 reported as left out.
        let (rt, dir) = rt("cap", 6, 11).await;
        let mut scope = Scope::from_config(&rt.config.tools);
        let out = search(&rt, &mut scope, "demo").await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("8 matching"), "{}", out.content);
        assert!(out.content.contains("3 more matched"), "{}", out.content);
        let enabled = PLUGIN_TOOLS
            .iter()
            .filter(|(n, _)| scope.advertises(n, true))
            .count();
        assert_eq!(
            enabled, MAX_HITS,
            "only the hits actually returned to the model are enabled; the rest must not slip in"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ───────── Wiring into `Agent` (real interception, real Scope) ─────────

    use crate::message::Message;
    use crate::ui::{AgentEvent, AuthDecision, Ui};
    use crate::agent::Agent;

    struct NoLlm;

    #[async_trait]
    impl crate::llm::LlmProvider for NoLlm {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[Value],
            _sink: Option<&(dyn crate::llm::StreamSink + Send + Sync)>,
        ) -> anyhow::Result<crate::llm::ChatReply> {
            unreachable!("this test only exercises the tool path")
        }
    }

    struct TestUi;

    #[async_trait]
    impl Ui for TestUi {
        async fn read_user_input(&mut self) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn emit(&mut self, _event: AgentEvent) {}
        async fn authorize(&mut self, _tool: &str, _args: &Value) -> AuthDecision {
            AuthDecision::Allow
        }
    }

    #[tokio::test]
    async fn the_agent_interception_is_what_makes_a_lookup_stick() {
        let (rt, dir) = rt("agent", 6, 5).await;
        let mut agent = Agent::new_with(Arc::new(NoLlm), rt.clone());

        let out = agent
            .authorize_and_execute(&mut TestUi, FIND_TOOL, json!({"query": "demo ls"}))
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("demo_ls"), "{}", out.content);

        // The point: what changed is **this Agent's** scope (`Tool::call` cannot reach it, which is
        // exactly why interception is required).
        let names = advertised(&rt.tool_schemas_scoped(agent.tool_scope()).await);
        assert!(names.contains(&"demo_ls".to_string()), "{names:?}");

        // A hidden tool **still runs when called directly**: hiding is advertising only.
        let ran = agent.authorize_and_execute(&mut TestUi, "demo_move", json!({})).await;
        assert!(!ran.is_error, "a hidden tool must still be callable: {}", ran.content);
        assert!(ran.content.contains("fake ran"), "{}", ran.content);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
