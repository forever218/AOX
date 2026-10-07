//! Self-test: depends on no LLM and directly verifies the whole chain
//! "create plugin → register → call → uninstall".
//! How to run: aox selftest

use std::path::PathBuf;

use anyhow::{bail, Result};
use serde_json::json;

use crate::config::Config;
use crate::plugin;
use crate::runtime::Runtime;
use crate::tool::meta;

const ECHO_SOURCE: &str = r#"
import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
from plugin_sdk import serve

def echo(args):
    # Carries cwd and non-ASCII content: verifies the "workdir = app root" and "UTF-8 round-trip" fixes
    return "ECHO: {} | cwd={} | 中文标记OK".format(args.get("text", ""), os.getcwd())

serve("selftest", {
    "selftest_echo": ("Echo the input text and report the process working directory", {"type": "object", "properties": {"text": {"type": "string"}}}, echo),
})
"#;

/// The plugin for fence verification: its only job is to **try to spawn a child process**.
///
/// Judgment uses "did the child process really run (did it print child-ok)" rather than
/// "did it throw" — assert on content, not on the shape of the error (error codes and
/// wording change with system versions).
const SPAWN_SOURCE: &str = r#"
import sys, os, subprocess
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
from plugin_sdk import serve

def child(args):
    try:
        p = subprocess.run([sys.executable, "-c", "print('child-ok')"],
                           capture_output=True, timeout=30)
        return "SPAWN: rc={} out={}".format(
            p.returncode, p.stdout.decode("utf-8", "replace").strip())
    except Exception as e:
        return "SPAWN-BLOCKED: {}: {}".format(type(e).__name__, e)

serve("selftest_spawn", {
    "selftest_spawn_child": ("Try to spawn a child process", {"type": "object", "properties": {}}, child),
})
"#;

/// Read-only declaration self-test plugin: the two tools **deliberately differ only in the 4th position (the `access` argument)**.
///
/// The point of testing with a pair of tools that "differ by a single field" is that this
/// change is *entirely* about that one position — if only one side is declared, the SDK could
/// still send `access` wrong (e.g. as a constant) and the test would pass anyway.
const READ_DECL_SOURCE: &str = r#"
import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
from plugin_sdk import serve

def probe(args):
    return "PROBE: {}".format(args.get("path", ""))

serve("selftest_read", {
    "selftest_read_scan": ("Read-only scan (declares read)",
                           {"type": "object", "properties": {"path": {"type": "string"}}},
                           probe, "read"),
    "selftest_read_write": ("Not declared (treated as may-write)",
                            {"type": "object", "properties": {"path": {"type": "string"}}},
                            probe),
})
"#;

pub async fn run(root: PathBuf, plugins_dir: PathBuf) -> Result<()> {
    println!("{}", crate::i18n::t("[selftest] initializing runtimes…"));
    plugin::ensure_sdk(&plugins_dir)?;
    let rt = Runtime::new(Config::dummy(), root, plugins_dir)?;
    for t in meta::meta_tools() {
        rt.register(t).await?;
    }

    println!("{}", crate::i18n::t("[selftest] creating the selftest plugin via the create_plugin path…"));
    let tools = plugin::create_plugin(&rt, "selftest", "python", crate::i18n::t("selftest echo plugin"), ECHO_SOURCE).await?;
    println!("{}", crate::tf!("[selftest] registered tools: {}", tools.join(", ")));
    if !tools.contains(&"selftest_echo".to_string()) {
        bail!(crate::i18n::t("selftest_echo is not registered"));
    }

    println!("{}", crate::i18n::t("[selftest] calling selftest_echo…"));
    let out = rt.execute("selftest_echo", json!({"text": "hello-agent"})).await;
    println!("{}", crate::tf!("[selftest] returned: {}", out.content));
    if out.is_error || !out.content.contains("hello-agent") {
        bail!(crate::i18n::t("echo result does not match expectation"));
    }
    // Regression 1: non-ASCII content must round-trip losslessly (the Windows default
    // GBK used to break through the protocol)
    if !out.content.contains("中文标记OK") {
        bail!(crate::tf!("non-ASCII content did not round-trip correctly; the encoding pipeline is still broken: {}", out.content));
    }
    // Regression 2: the plugin process's working directory must be the app root, not
    //             the plugin's own directory (cwd=plugin dir used to scramble relative
    //             paths inside plugins)
    if out.content.contains(&format!("plugins{}selftest", std::path::MAIN_SEPARATOR)) {
        bail!(crate::tf!("the plugin's working directory is still the plugin's own directory, not the app root: {}", out.content));
    }

    println!("{}", crate::i18n::t("[selftest] removing the plugin via the remove_plugin path…"));
    let removed = plugin::remove_plugin(&rt, "selftest").await?;
    println!("{}", crate::tf!("[selftest] unregistered tools: {}", removed.join(", ")));

    let names = rt.tool_names().await;
    if names.iter().any(|n| n == "selftest_echo") {
        bail!(crate::i18n::t("tools still present after removal"));
    }

    // Regression 3: a same-named leftover directory (the remains of a previous failed
    // creation) must be cleaned up automatically and rebuilt successfully
    println!("{}", crate::i18n::t("[selftest] regression: rebuilding a stale directory…"));
    let stale = rt.plugins_dir.join("selftest_stale");
    std::fs::create_dir_all(&stale)?;
    std::fs::write(stale.join("leftover.txt"), "x")?;
    let t3 = plugin::create_plugin(&rt, "selftest_stale", "python", crate::i18n::t("stale-directory rebuild test"), ECHO_SOURCE).await?;
    if !t3.contains(&"selftest_echo".to_string()) {
        bail!(crate::i18n::t("tool not registered after the stale-directory rebuild"));
    }
    if stale.join("leftover.txt").exists() {
        bail!(crate::i18n::t("stale files were not cleaned up"));
    }
    plugin::remove_plugin(&rt, "selftest_stale").await?;

    // Regression 4: a directory that exists but whose plugin isn't loaded (a ghost
    // plugin) must also be clearable by remove
    println!("{}", crate::i18n::t("[selftest] regression: ghost-plugin cleanup…"));
    let orphan = rt.plugins_dir.join("selftest_orphan");
    std::fs::create_dir_all(&orphan)?;
    std::fs::write(orphan.join("plugin.toml"), "name = \"selftest_orphan\"\n")?;
    plugin::remove_plugin(&rt, "selftest_orphan").await?;
    if orphan.exists() {
        bail!(crate::i18n::t("ghost plugin directory was not cleaned up"));
    }

    // Regression 5: in thinking mode (DeepSeek thinking), messages must pass
    //             reasoning_content back verbatim. Previously Message dropped the field,
    //             and replaying a historical message with tool_calls got rejected by the
    //             API with 400 "reasoning_content must be passed back", showing up as "a
    //             multi-step task silently dying halfway".
    println!("{}", crate::i18n::t("[selftest] regression: thinking-mode reasoning_content round-trip…"));
    let raw = json!({
        "role": "assistant",
        "content": "我先看看有哪些插件。",
        "reasoning_content": "需要先确认当前可用的能力。",
        "tool_calls": [{
            "id": "call_1",
            "type": "function",
            "function": {"name": "list_plugins", "arguments": "{}"}
        }]
    });
    let msg: crate::message::Message = serde_json::from_value(raw)?;
    if msg.reasoning_content.as_deref() != Some("需要先确认当前可用的能力。") {
        bail!(crate::i18n::t("the assistant message's reasoning_content was not preserved (echoing it back in thinking mode would be rejected by the API)"));
    }
    let back = serde_json::to_value(&msg)?;
    if back.get("reasoning_content").and_then(|v| v.as_str()) != Some("需要先确认当前可用的能力。") {
        bail!(crate::tf!("reasoning_content missing from the echoed-back request: {}", back));
    }
    if back.get("tool_calls").is_none() {
        bail!(crate::i18n::t("tool_calls missing from the echoed-back request"));
    }
    // Non-thinking messages must not carry the field out of nowhere, to avoid polluting
    // strict endpoints like OpenAI
    let plain = serde_json::to_value(crate::message::Message::user("hi"))?;
    if plain.get("reasoning_content").is_some() {
        bail!(crate::tf!("plain messages must not serialize reasoning_content: {}", plain));
    }

    // Regression 6: SSE streaming parsing. The reasoning process must be pushed to the
    //             sink in real time (otherwise no progress is visible while waiting), and
    //             a tool call's fragmented arguments must reassemble into complete JSON
    //             (in streaming, arguments get split across several chunks).
    println!("{}", crate::i18n::t("[selftest] regression: SSE stream parsing (reasoning + tool-call deltas)…"));
    {
        use crate::llm::openai::StreamAcc;
        use crate::llm::StreamSink;
        use std::sync::Mutex;

        #[derive(Default)]
        struct Capture {
            reasoning: Mutex<String>,
            content: Mutex<String>,
        }
        impl StreamSink for Capture {
            fn on_reasoning(&self, delta: &str) {
                self.reasoning.lock().unwrap().push_str(delta);
            }
            fn on_content(&self, delta: &str) {
                self.content.lock().unwrap().push_str(delta);
            }
        }

        let cap = Capture::default();
        let mut acc = StreamAcc::default();
        let chunks = [
            json!({"choices": [{"delta": {"reasoning_content": "先看"}, "finish_reason": null}]}),
            json!({"choices": [{"delta": {"reasoning_content": "有哪些插件"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_a", "function": {"name": "list_plugins", "arguments": "{}"}}
            ]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 1, "id": "call_b", "function": {"name": "fs_read"}}
            ]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 1, "function": {"arguments": "{\"path\":"}}
            ]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 1, "function": {"arguments": "\"a.txt\"}"}}
            ]}}]}),
            json!({"choices": [{"delta": {"content": "好的"}, "finish_reason": null}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {
                "prompt_tokens": 1200,
                "completion_tokens": 300,
                "total_tokens": 1500,
                "prompt_tokens_details": {"cached_tokens": 1024},
                "prompt_cache_miss_tokens": 176,
                "completion_tokens_details": {"reasoning_tokens": 42}
            }}),
        ];
        for c in &chunks {
            acc.feed(&format!("data: {c}"), Some(&cap));
        }
        acc.feed(": keep-alive", Some(&cap)); // Comment lines must be ignored
        acc.feed("data: [DONE]", Some(&cap));

        let streamed = cap.reasoning.lock().unwrap().clone();
        if streamed != "先看有哪些插件" {
            bail!(crate::tf!("reasoning was not pushed to the sink in real time: {}", format!("{streamed:?}")));
        }
        // The content deltas must also be pushed to the sink — real-time token scrolling depends on counting the output side
        let streamed_text = cap.content.lock().unwrap().clone();
        if streamed_text != "好的" {
            bail!(crate::tf!("content deltas were not pushed to the sink in real time: {}", format!("{streamed_text:?}")));
        }
        if !acc.is_done() {
            bail!(crate::i18n::t("did not recognize data: [DONE]"));
        }
        let reply = acc.into_reply();
        if reply.message.reasoning_content.as_deref() != Some("先看有哪些插件") {
            bail!(crate::i18n::t("streaming accumulation did not preserve reasoning_content (echoing it back in thinking mode would be rejected by the API)"));
        }
        if reply.message.text() != Some("好的") {
            bail!(crate::tf!("streamed content accumulation mismatch: {}", format!("{:?}", reply.message.content)));
        }
        let calls = reply.message.tool_calls.clone().unwrap_or_default();
        if calls.len() != 2 {
            bail!(crate::tf!("streaming tool-call count mismatch: {}", calls.len()));
        }
        if calls[0].function.name != "list_plugins" || calls[0].function.arguments != "{}" {
            bail!(crate::tf!("tool call #1 assembled incorrectly: {}", format!("{:?}", calls[0])));
        }
        if calls[1].id != "call_b"
            || calls[1].function.name != "fs_read"
            || calls[1].function.arguments != "{\"path\":\"a.txt\"}"
        {
            bail!(crate::tf!("delta arguments joined incorrectly: {}", format!("{:?}", calls[1])));
        }
        if reply.finish_reason.as_deref() != Some("tool_calls") {
            bail!(crate::tf!("finish_reason not captured: {}", format!("{:?}", reply.finish_reason)));
        }
        // usage must be parsed field by field: these numbers directly decide what the
        // UI shows the user
        let u = &reply.usage;
        if u.prompt != Some(1200)
            || u.completion != Some(300)
            || u.total != Some(1500)
            || u.reasoning != Some(42)
            || u.cached != Some(1024)
            || u.cache_miss != Some(176)
        {
            bail!(crate::tf!("usage not captured completely: {}", format!("{u:?}")));
        }
    }

    // Regression 7: the process fence (Job Object). A **three-way cross-check** —
    //        A default quota: the plugin must work as usual and be able to spawn a child
    //          ("the fence must not block the plugin from working");
    //        B quota locked to 0: the fence must block it (the fork-bomb protection is
    //          still there);
    //        C fence fully disabled: it can spawn again (the control group — if C also
    //          fails, then B's "blocked" isn't the fence's doing at all, but some other
    //          problem with python on this machine).
    //        Only when all three pass do the two conclusions above hold.
    println!("{}", crate::i18n::t("[selftest] regression: process fence (Job Object)…"));
    {
        let t = plugin::create_plugin(&rt, "selftest_spawn", "python", crate::i18n::t("fence verification"), SPAWN_SOURCE).await?;
        if !t.contains(&"selftest_spawn_child".to_string()) {
            bail!(crate::i18n::t("the fence-verification plugin registered no tools"));
        }
        // A: the default quota. This pins "the default config does not stop plugins from
        // working" — the default used to be 1 (and that 1 was the total process count
        // **including the plugin itself**), so any plugin relying on `subprocess` got
        // `WinError 1816 insufficient quota`, which effectively disabled the plugin.
        let ok = rt.execute("selftest_spawn_child", json!({})).await;
        println!("{}", crate::tf!("[selftest]   default quota: {}", ok.content));
        if ok.is_error {
            bail!(crate::tf!("the fence-verification plugin failed on its own (the fence must not affect the plugin itself): {}", ok.content));
        }
        if !ok.content.contains("child-ok") {
            bail!(crate::tf!(
                "under the default config (max_processes = {}) the plugin cannot spawn a child — the default quota blocks the plugin from working: {}",
                rt.config.sandbox.max_processes,
                ok.content
            ));
        }
        plugin::remove_plugin(&rt, "selftest_spawn").await?;
    }

    {
        // B: lock the subprocess quota to 0 ⇒ it must be blocked.
        let mut cfg = Config::dummy();
        cfg.sandbox.max_processes = 0;
        let rt0 = Runtime::new(cfg, rt.root.clone(), rt.plugins_dir.clone())?;
        plugin::create_plugin(&rt0, "selftest_spawn", "python", crate::i18n::t("fence verification (locked)"), SPAWN_SOURCE).await?;
        let blocked = rt0.execute("selftest_spawn_child", json!({})).await;
        println!("{}", crate::tf!("[selftest]   quota locked to 0: {}", blocked.content));
        if blocked.is_error {
            bail!(crate::tf!("with the quota locked to 0 the plugin failed on its own (the fence must not affect the plugin itself): {}", blocked.content));
        }
        if blocked.content.contains("child-ok") {
            bail!(crate::tf!("max_processes = 0 did not lock it down: the plugin still spawned a child — {}", blocked.content));
        }
        plugin::remove_plugin(&rt0, "selftest_spawn").await?;
    }

    {
        // C: control group (fence fully disabled).
        let mut cfg = Config::dummy();
        cfg.sandbox.enabled = false;
        let rt2 = Runtime::new(cfg, rt.root.clone(), rt.plugins_dir.clone())?;
        plugin::create_plugin(&rt2, "selftest_spawn", "python", crate::i18n::t("fence verification (control)"), SPAWN_SOURCE).await?;
        let open = rt2.execute("selftest_spawn_child", json!({})).await;
        println!("{}", crate::tf!("[selftest]   fence off: {}", open.content));
        if !open.content.contains("child-ok") {
            bail!(crate::tf!(
                "control group failed: with the fence off the plugin still cannot spawn a child ⇒ what blocks it is not the fence, invalidating the \"quota locked to 0\" conclusion above — {}",
                open.content
            ));
        }
        plugin::remove_plugin(&rt2, "selftest_spawn").await?;
    }

    // Regression 8: workspace boundary (sandbox approach ③). Likewise an **A/B cross-check** — the restricted tier must block the out-of-bounds write,
    //         **and the file must genuinely not have been created**; under the `full` tier the same call must succeed.
    //         Testing only one side, "it got blocked" might just mean the path itself was broken (missing parent dir, etc.),
    //         which would make this test lie to itself — the same reasoning as Regression 7.
    //         New semantics (2026-10-04): **reads unrestricted, writes/deletes restricted** — the "relative path rejected" case
    //         has moved to **writes**, plus a cross-check of "reads outside allowed / deletes outside blocked".
    println!("{}", crate::i18n::t("[selftest] regression: workspace boundary (file capability returned to the host)…"));
    {
        let ws_root = std::env::temp_dir().join("aox-selftest-ws");
        let outside = std::env::temp_dir().join("aox-selftest-outside.txt");
        // The "reads unrestricted" case needs to read a **genuinely existing**
        // out-of-bounds file: reading a non-existent path fails too, and that failure
        // can't be attributed to the boundary.
        let outside_read = std::env::temp_dir().join("aox-selftest-outside-read.txt");
        let _ = std::fs::remove_dir_all(&ws_root);
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_file(&outside_read);
        std::fs::create_dir_all(ws_root.join("inside"))?;

        // ── A: fixed workplace ⇒ only inside is allowed ──
        let mut cfg = Config::dummy();
        cfg.workplace = crate::config::WorkplaceConfig {
            mode: "fixed".into(),
            root: Some(ws_root.to_string_lossy().into()),
        };
        let rt_ws = Runtime::new(cfg, ws_root.clone(), ws_root.join("plugins"))?;
        for t in crate::tool::hostfs::host_file_tools() {
            rt_ws.register(t).await?;
        }
        if rt_ws.workplace.active_root().is_none() {
            bail!(crate::i18n::t("the fixed tier has no definite workspace root"));
        }

        let inside = ws_root.join("inside").join("ok.txt");
        let w = rt_ws
            .execute("write_file", json!({"path": inside.to_string_lossy(), "content": "工作区内的内容"}))
            .await;
        if w.is_error {
            bail!(crate::tf!("a write inside the workspace was rejected (boundary too tight): {}", w.content));
        }
        let r = rt_ws.execute("read_file", json!({"path": inside.to_string_lossy()})).await;
        if r.is_error || !r.content.contains("工作区内的内容") {
            bail!(crate::tf!("the read/write round-trip inside the workspace failed: {}", r.content));
        }

        let bad = rt_ws
            .execute("write_file", json!({"path": outside.to_string_lossy(), "content": "x"}))
            .await;
        println!("{}", crate::tf!("[selftest]   restricted-tier out-of-bounds: {}", bad.content.lines().next().unwrap_or("")));
        if !bad.is_error {
            bail!(crate::i18n::t("the out-of-bounds write was not intercepted"));
        }
        if outside.exists() {
            bail!(crate::i18n::t("the out-of-bounds write was rejected, but the file was really created — this boundary is fake"));
        }
        for needle in [crate::i18n::t("permission boundary"), crate::i18n::t("Do not retry"), crate::i18n::t("relay this to the user")] {
            if !bad.content.contains(needle) {
                bail!(crate::tf!("the out-of-bounds message is missing the \"{}\" section (the model would keep retrying): {}", needle, bad.content));
            }
        }
        // A relative path must be refused (**true only for write/delete**): the host and
        // the plugin subprocess have different cwds, so the same relative path points at
        // two different files — a risk that only "writing" carries.
        let rel = rt_ws
            .execute("write_file", json!({"path": "config.toml", "content": "x"}))
            .await;
        if !rel.is_error {
            bail!(crate::i18n::t("a **write** with a relative path was not rejected under the restricted tier"));
        }
        // New semantics (2026-10-04): **reads unrestricted, writes/deletes restricted**.
        // Cross-check both together — testing only one side means "read passed" could just
        // be because reading never went through the check at all (an implementation leak,
        // not correct semantics).
        std::fs::write(&outside_read, "区外的内容")?;
        let rd = rt_ws.execute("read_file", json!({"path": outside_read.to_string_lossy()})).await;
        if rd.is_error || !rd.content.contains("区外的内容") {
            bail!(crate::tf!("reading an outside file failed under the restricted tier (reads should be unrestricted): {}", rd.content));
        }
        let rm = rt_ws.execute("remove", json!({"path": outside_read.to_string_lossy()})).await;
        if !rm.is_error {
            bail!(crate::i18n::t("deleting an outside file was not intercepted under the restricted tier"));
        }
        if !outside_read.exists() {
            bail!(crate::i18n::t("the outside delete was rejected, but the file was really deleted — this boundary is fake"));
        }
        println!("{}", crate::i18n::t("[selftest]   read outside allowed / delete outside blocked: cross-check passed"));
        let _ = std::fs::remove_file(&outside_read);

        // ── B: the control group (full) — the same out-of-bounds call must succeed, so
        // that A's interception can be attributed to the workplace ──
        let mut cfg_full = Config::dummy();
        cfg_full.workplace = crate::config::WorkplaceConfig { mode: "full".into(), root: None };
        let rt_full = Runtime::new(cfg_full, ws_root.clone(), ws_root.join("plugins"))?;
        for t in crate::tool::hostfs::host_file_tools() {
            rt_full.register(t).await?;
        }
        if rt_full.workplace.active_root().is_some() {
            bail!(crate::i18n::t("the full tier must not have a workspace root"));
        }
        let ok = rt_full
            .execute("write_file", json!({"path": outside.to_string_lossy(), "content": "x"}))
            .await;
        if ok.is_error {
            bail!(crate::tf!(
                "control group failed: under the full tier the same path cannot be written either ⇒ the \"out-of-bounds blocked\" above cannot be attributed to the workspace — {}",
                ok.content
            ));
        }
        if !outside.exists() {
            bail!(crate::i18n::t("the control group reported a successful write, but the file is not there"));
        }
        let _ = std::fs::remove_dir_all(&ws_root);
        let _ = std::fs::remove_file(&outside);
    }

    // The plugin-declared **workplace intent** (the 4th position in a tool declaration)
    // travels all the way from the SDK to the registry.
    //
    // This section fills in the **middle hop** that can't be tested elsewhere: the parse
    // rules (`plugin::parse_access`'s unit test) and how the boundary uses it (`agent`'s
    // unit test) are each covered, but "did the access a plugin wrote in initialize
    // actually become the intent on the registry" can only be verified here — it spans
    // four layers: the Python SDK, protocol parsing, ToolDef and PluginTool, and a gap in
    // any one of them silently falls back to "writes to disk".
    println!("{}", crate::i18n::t("[selftest] regression: plugin-declared read intent…"));
    {
        let tools =
            plugin::create_plugin(&rt, "selftest_read", "python", crate::i18n::t("read-only declaration selftest"), READ_DECL_SOURCE)
                .await?;
        for t in ["selftest_read_scan", "selftest_read_write"] {
            if !tools.contains(&t.to_string()) {
                bail!(crate::tf!("{} is not registered (registered: {})", t, format!("{tools:?}")));
            }
        }
        // A: declared read ⇒ the registry intent must be Read
        let a = rt.tool_access("selftest_read_scan").await;
        if a != crate::workplace::Access::Read {
            bail!(crate::tf!(
                "a tool that declared \"read\" is not read-only in the registry (got {}) — first check whether the Python SDK's `_tool_entry` sends the 4th position",
                format!("{a:?}")
            ));
        }
        // B: not declared ⇒ still falls back to "writes to disk". Old plugins' behavior must not change by a single byte —
        //    if this loosens, it silently opens up all existing write-class plugins.
        let b = rt.tool_access("selftest_read_write").await;
        if b != crate::workplace::Access::Write {
            bail!(crate::tf!("a tool without an access declaration was treated as read-only (got {}) — this silently loosens the write boundary", format!("{b:?}")));
        }
        println!("{}", crate::i18n::t("[selftest]   declared read → Read; undeclared → Write: cross-check passed"));
    }

    println!("{}", crate::i18n::t("[selftest] all checks passed ✔"));
    Ok(())
}
