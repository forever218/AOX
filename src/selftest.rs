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

    println!("[selftest] 全部通过 ✔");
    Ok(())
}
