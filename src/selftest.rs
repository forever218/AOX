//! 自检：不依赖任何 LLM，直接验证「创建插件 → 注册 → 调用 → 卸载」全链路。
//! 运行方式: aox selftest

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
    # 附带 cwd 与非 ASCII 内容：用于验证「工作目录=应用根」与「UTF-8 往返」两条修复
    return "ECHO: {} | cwd={} | 中文标记OK".format(args.get("text", ""), os.getcwd())

serve("selftest", {
    "selftest_echo": ("回显输入文本并报告进程工作目录", {"type": "object", "properties": {"text": {"type": "string"}}}, echo),
})
"#;

/// 围栏验证用插件：它的唯一工作就是**尝试拉起一个子进程**。
///
/// 用「子进程有没有真的跑起来（有没有打印 child-ok）」而不是「有没有抛异常」来判定 ——
/// 断言要断内容，不要断错误的形状（错误码/措辞会随系统版本变）。
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
    "selftest_spawn_child": ("尝试拉起一个子进程", {"type": "object", "properties": {}}, child),
})
"#;

pub async fn run(root: PathBuf, plugins_dir: PathBuf) -> Result<()> {
    println!("[selftest] 初始化运行时…");
    plugin::ensure_sdk(&plugins_dir)?;
    let rt = Runtime::new(Config::dummy(), root, plugins_dir)?;
    for t in meta::meta_tools() {
        rt.register(t).await?;
    }

    println!("[selftest] 通过 create_plugin 链路创建自检插件…");
    let tools = plugin::create_plugin(&rt, "selftest", "python", "自检回显插件", ECHO_SOURCE).await?;
    println!("[selftest] 已注册工具: {}", tools.join(", "));
    if !tools.contains(&"selftest_echo".to_string()) {
        bail!("selftest_echo 未注册");
    }

    println!("[selftest] 调用 selftest_echo…");
    let out = rt.execute("selftest_echo", json!({"text": "hello-agent"})).await;
    println!("[selftest] 返回: {}", out.content);
    if out.is_error || !out.content.contains("hello-agent") {
        bail!("回显结果不符合预期");
    }
    // 回归 1：非 ASCII 内容必须能无损往返（此前 Windows 默认 GBK 会击穿协议）
    if !out.content.contains("中文标记OK") {
        bail!("非 ASCII 内容未能正确往返，编码链路仍有问题: {}", out.content);
    }
    // 回归 2：插件进程的工作目录必须是应用根，而不是插件自身目录
    //         （此前 cwd=插件目录导致插件内相对路径错乱）
    if out.content.contains(&format!("plugins{}selftest", std::path::MAIN_SEPARATOR)) {
        bail!("插件工作目录仍是插件自身目录，未指向应用根: {}", out.content);
    }

    println!("[selftest] 通过 remove_plugin 链路卸载插件…");
    let removed = plugin::remove_plugin(&rt, "selftest").await?;
    println!("[selftest] 已注销工具: {}", removed.join(", "));

    let names = rt.tool_names().await;
    if names.iter().any(|n| n == "selftest_echo") {
        bail!("卸载后工具仍残留");
    }

    // 回归 3：同名残留目录（上次创建失败的遗骸）应被自动清理并成功重建
    println!("[selftest] 回归：残留目录重建…");
    let stale = rt.plugins_dir.join("selftest_stale");
    std::fs::create_dir_all(&stale)?;
    std::fs::write(stale.join("leftover.txt"), "x")?;
    let t3 = plugin::create_plugin(&rt, "selftest_stale", "python", "残留目录重建测试", ECHO_SOURCE).await?;
    if !t3.contains(&"selftest_echo".to_string()) {
        bail!("残留目录重建后工具未注册");
    }
    if stale.join("leftover.txt").exists() {
        bail!("残留文件未被清理");
    }
    plugin::remove_plugin(&rt, "selftest_stale").await?;

    // 回归 4：目录存在但插件未加载（幽灵插件）也应能被 remove 清掉
    println!("[selftest] 回归：幽灵插件清理…");
    let orphan = rt.plugins_dir.join("selftest_orphan");
    std::fs::create_dir_all(&orphan)?;
    std::fs::write(orphan.join("plugin.toml"), "name = \"selftest_orphan\"\n")?;
    plugin::remove_plugin(&rt, "selftest_orphan").await?;
    if orphan.exists() {
        bail!("幽灵插件目录未被清理");
    }

    // 回归 5：思考模式（DeepSeek thinking）消息必须原样回传 reasoning_content。
    //        此前 Message 丢弃该字段，带 tool_calls 的历史消息再回传时会被 API 以
    //        400 "reasoning_content must be passed back" 拒绝，表现为「多步任务跑到一半静默中断」。
    println!("[selftest] 回归：思考模式 reasoning_content 往返…");
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
        bail!("assistant 消息的 reasoning_content 未被保留（思考模式下回传会被 API 拒绝）");
    }
    let back = serde_json::to_value(&msg)?;
    if back.get("reasoning_content").and_then(|v| v.as_str()) != Some("需要先确认当前可用的能力。") {
        bail!("回传请求中缺少 reasoning_content: {back}");
    }
    if back.get("tool_calls").is_none() {
        bail!("回传请求中缺少 tool_calls");
    }
    // 非思考消息不应凭空带上该字段，避免污染 OpenAI 等严格校验的端点
    let plain = serde_json::to_value(crate::message::Message::user("hi"))?;
    if plain.get("reasoning_content").is_some() {
        bail!("普通消息不应序列化 reasoning_content: {plain}");
    }

    // 回归 6：SSE 流式解析。思考过程必须能实时推给 sink（否则等待时看不到进度），
    //        工具调用的分片参数必须能拼回完整 JSON（流式下 arguments 会被拆成多个 chunk）。
    println!("[selftest] 回归：SSE 流式解析（思考过程 + 工具调用分片）…");
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
        acc.feed(": keep-alive", Some(&cap)); // 注释行必须被忽略
        acc.feed("data: [DONE]", Some(&cap));

        let streamed = cap.reasoning.lock().unwrap().clone();
        if streamed != "先看有哪些插件" {
            bail!("思考过程未实时推送给 sink: {streamed:?}");
        }
        // 正文增量也得推给 sink —— 实时 token 滚动要靠它数输出侧
        let streamed_text = cap.content.lock().unwrap().clone();
        if streamed_text != "好的" {
            bail!("正文增量未实时推送给 sink: {streamed_text:?}");
        }
        if !acc.is_done() {
            bail!("未识别 data: [DONE]");
        }
        let reply = acc.into_reply();
        if reply.message.reasoning_content.as_deref() != Some("先看有哪些插件") {
            bail!("流式累加未保留 reasoning_content（思考模式回传会被 API 拒绝）");
        }
        if reply.message.text() != Some("好的") {
            bail!("流式正文累加错误: {:?}", reply.message.content);
        }
        let calls = reply.message.tool_calls.clone().unwrap_or_default();
        if calls.len() != 2 {
            bail!("流式工具调用数量错误: {}", calls.len());
        }
        if calls[0].function.name != "list_plugins" || calls[0].function.arguments != "{}" {
            bail!("第 1 个工具调用拼装错误: {:?}", calls[0]);
        }
        if calls[1].id != "call_b"
            || calls[1].function.name != "fs_read"
            || calls[1].function.arguments != "{\"path\":\"a.txt\"}"
        {
            bail!("分片参数拼接错误: {:?}", calls[1]);
        }
        if reply.finish_reason.as_deref() != Some("tool_calls") {
            bail!("finish_reason 未捕获: {:?}", reply.finish_reason);
        }
        // usage 要逐项解析到位：这些数字直接决定界面上给用户看什么
        let u = &reply.usage;
        if u.prompt != Some(1200)
            || u.completion != Some(300)
            || u.total != Some(1500)
            || u.reasoning != Some(42)
            || u.cached != Some(1024)
            || u.cache_miss != Some(176)
        {
            bail!("usage 未完整捕获: {u:?}");
        }
    }

    // 回归 7：进程围栏（Job Object）。**三段对拍** ——
    //        A 默认额度：插件必须照常干活，子进程也拉得起来（「围栏不该挡插件干活」）；
    //        B 额度锁 0：必须有围栏拦下（fork bomb 防护还在）；
    //        C 围栏整个关掉：又拉得起来（对照组 —— 若 C 也失败，那 B 的「被拦下」
    //          根本不是围栏的功劳，而是这台机器上的 python 自己有别的毛病）。
    //        只有三段都过，上面两条结论才成立。
    println!("[selftest] 回归：进程围栏（Job Object）…");
    {
        let t = plugin::create_plugin(&rt, "selftest_spawn", "python", "围栏验证", SPAWN_SOURCE).await?;
        if !t.contains(&"selftest_spawn_child".to_string()) {
            bail!("围栏验证插件未注册工具");
        }
        // A：默认额度。这一条钉住「默认配置不阻止插件干活」—— 以前默认是 1
        // （而且那个 1 是**含插件自己**的进程总数），于是任何靠 `subprocess` 的插件
        // 都是 `WinError 1816 配额不足`，插件等于废了。
        let ok = rt.execute("selftest_spawn_child", json!({})).await;
        println!("[selftest]   默认额度: {}", ok.content);
        if ok.is_error {
            bail!("围栏验证插件自身执行失败（围栏不该影响插件自己）: {}", ok.content);
        }
        if !ok.content.contains("child-ok") {
            bail!(
                "默认配置（max_processes = {}）下插件拉不起子进程 —— \
                 默认额度把插件干活挡住了: {}",
                rt.config.sandbox.max_processes,
                ok.content
            );
        }
        plugin::remove_plugin(&rt, "selftest_spawn").await?;
    }

    {
        // B：把子进程额度锁到 0 ⇒ 必须拦下。
        let mut cfg = Config::dummy();
        cfg.sandbox.max_processes = 0;
        let rt0 = Runtime::new(cfg, rt.root.clone(), rt.plugins_dir.clone())?;
        plugin::create_plugin(&rt0, "selftest_spawn", "python", "围栏验证（锁死）", SPAWN_SOURCE).await?;
        let blocked = rt0.execute("selftest_spawn_child", json!({})).await;
        println!("[selftest]   额度锁 0: {}", blocked.content);
        if blocked.is_error {
            bail!("额度锁 0 时插件自身执行失败（围栏不该影响插件自己）: {}", blocked.content);
        }
        if blocked.content.contains("child-ok") {
            bail!("max_processes = 0 没锁住：插件仍然拉起了子进程 —— {}", blocked.content);
        }
        plugin::remove_plugin(&rt0, "selftest_spawn").await?;
    }

    {
        // C：对照组（围栏整个关掉）。
        let mut cfg = Config::dummy();
        cfg.sandbox.enabled = false;
        let rt2 = Runtime::new(cfg, rt.root.clone(), rt.plugins_dir.clone())?;
        plugin::create_plugin(&rt2, "selftest_spawn", "python", "围栏验证（对照）", SPAWN_SOURCE).await?;
        let open = rt2.execute("selftest_spawn_child", json!({})).await;
        println!("[selftest]   围栏关闭: {}", open.content);
        if !open.content.contains("child-ok") {
            bail!(
                "对照组失败：关掉围栏后插件仍拉不起子进程 ⇒ 挡住它的不是围栏，\
                 上面那条「额度锁 0 生效」的结论无效 —— {}",
                open.content
            );
        }
        plugin::remove_plugin(&rt2, "selftest_spawn").await?;
    }

    // 回归 8：工作区边界（沙箱方案 ③）。同样是 **A/B 对拍** —— 受限档必须拦住越界、
    //         **而且那个文件真的没被创建**；full 档下同一个调用必须成功。
    //         只测一边的话，「被拦住了」可能只是因为路径本身有问题（父目录不存在之类），
    //         那样这条测试是在自欺 —— 与回归 7 同一个理由。
    //         新语义（2026-10-04）：**读不设限、写删受限** —— 「相对路径被拒」那条已改到
    //         **写**上，另加「读区外放行 / 删区外拦截」的对拍。
    println!("[selftest] 回归：工作区边界（文件能力收回宿主）…");
    {
        let ws_root = std::env::temp_dir().join("aox-selftest-ws");
        let outside = std::env::temp_dir().join("aox-selftest-outside.txt");
        // 「读不设限」那条要读一个**真实存在**的区外文件：读一个不存在的路径也会失败，
        // 那种失败归因不到边界上。
        let outside_read = std::env::temp_dir().join("aox-selftest-outside-read.txt");
        let _ = std::fs::remove_dir_all(&ws_root);
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_file(&outside_read);
        std::fs::create_dir_all(ws_root.join("inside"))?;

        // ── A：固定工作区 ⇒ 只准里面 ──
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
            bail!("fixed 档位没有确定工作区根");
        }

        let inside = ws_root.join("inside").join("ok.txt");
        let w = rt_ws
            .execute("write_file", json!({"path": inside.to_string_lossy(), "content": "工作区内的内容"}))
            .await;
        if w.is_error {
            bail!("工作区内写入被拒了（边界太紧）: {}", w.content);
        }
        let r = rt_ws.execute("read_file", json!({"path": inside.to_string_lossy()})).await;
        if r.is_error || !r.content.contains("工作区内的内容") {
            bail!("工作区内的读写往返失败: {}", r.content);
        }

        let bad = rt_ws
            .execute("write_file", json!({"path": outside.to_string_lossy(), "content": "x"}))
            .await;
        println!("[selftest]   受限档越界: {}", bad.content.lines().next().unwrap_or(""));
        if !bad.is_error {
            bail!("越界写入没被拦下");
        }
        if outside.exists() {
            bail!("越界写入被拒了，但文件真的被创建了 —— 这个边界是假的");
        }
        for needle in ["权限边界", "不要重试", "转述给用户"] {
            if !bad.content.contains(needle) {
                bail!("越界文案缺了「{needle}」这一段（模型会因此反复重试）: {}", bad.content);
            }
        }
        // 相对路径必须拒（**只对写/删成立**）：宿主与插件子进程的 cwd 不同，
        // 同一个相对路径会指到两个文件 —— 那是「写」才有的风险。
        let rel = rt_ws
            .execute("write_file", json!({"path": "config.toml", "content": "x"}))
            .await;
        if !rel.is_error {
            bail!("受限档位下**写**的相对路径没被拒");
        }
        // 新语义（2026-10-04）：**读不设限、写删受限**。两条一起对拍 ——
        // 只测一边的话，「读通过了」也可能只是因为读压根没走判定（那是实现漏了，不是语义对）。
        std::fs::write(&outside_read, "区外的内容")?;
        let rd = rt_ws.execute("read_file", json!({"path": outside_read.to_string_lossy()})).await;
        if rd.is_error || !rd.content.contains("区外的内容") {
            bail!("受限档位下读区外文件失败（读应当不设限）: {}", rd.content);
        }
        let rm = rt_ws.execute("remove", json!({"path": outside_read.to_string_lossy()})).await;
        if !rm.is_error {
            bail!("受限档位下删区外文件没被拦下");
        }
        if !outside_read.exists() {
            bail!("区外删除被拒了，但文件真的被删了 —— 这个边界是假的");
        }
        println!("[selftest]   读区外放行 / 删区外拦截: 对拍通过");
        let _ = std::fs::remove_file(&outside_read);

        // ── B：对照组（full）—— 同一个越界调用必须成功，A 的拦截才归因于工作区 ──
        let mut cfg_full = Config::dummy();
        cfg_full.workplace = crate::config::WorkplaceConfig { mode: "full".into(), root: None };
        let rt_full = Runtime::new(cfg_full, ws_root.clone(), ws_root.join("plugins"))?;
        for t in crate::tool::hostfs::host_file_tools() {
            rt_full.register(t).await?;
        }
        if rt_full.workplace.active_root().is_some() {
            bail!("full 档位不该有工作区根");
        }
        let ok = rt_full
            .execute("write_file", json!({"path": outside.to_string_lossy(), "content": "x"}))
            .await;
        if ok.is_error {
            bail!(
                "对照组失败：full 档位下同一个路径也写不了 ⇒ 上面「越界被拦」不能归因于工作区 —— {}",
                ok.content
            );
        }
        if !outside.exists() {
            bail!("对照组报告写入成功，但文件不在");
        }
        let _ = std::fs::remove_dir_all(&ws_root);
        let _ = std::fs::remove_file(&outside);
    }

    println!("[selftest] 全部通过 ✔");
    Ok(())
}
