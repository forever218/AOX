use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::auth::AuthLevel;
use crate::history::UsageRecord;
use crate::llm::{LlmProvider, Usage};
use crate::message::Message;
use crate::runtime::Runtime;
use crate::tool::{ToolKind, ToolOutput};
use crate::ui::{AgentEvent, AuthDecision, Ui};

/// 系统提示：告诉模型「你什么都没有，需要自己造工具」。
pub const SYSTEM_PROMPT: &str = r#"你是 AOX，一个「自我增长」的 AI 代理，运行在用户本地的 Rust 宿主程序中。

# 核心事实
- 你初始没有任何实际工具，只有三个元工具：create_plugin、list_plugins、remove_plugin。
- 当任务需要某种能力（读写文件、执行命令、网络请求、数据处理……）时，你必须先用 create_plugin 自己编写一个插件来提供该能力，然后调用插件提供的工具完成任务。
- 插件持久保存在 plugins/ 目录，重启后自动加载。动手创建新插件前，先 list_plugins 检查是否已有可复用的能力。

# 插件协议
插件是独立进程，宿主通过 stdio 与之通信：每条消息是「一行 JSON」。stdout 只允许协议消息，日志一律写 stderr。
1. 启动后宿主发送: {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
   插件回复一行: {"jsonrpc":"2.0","id":1,"result":{"name":"<插件名>","tools":[{"name":"<工具名>","description":"<说明>","parameters":<JSON Schema>}]}}
2. 工具调用: {"jsonrpc":"2.0","id":N,"method":"tools/call","params":{"name":"<工具名>","arguments":{...}}}
   插件回复: {"jsonrpc":"2.0","id":N,"result":{"content":"<字符串结果>","is_error":false}}
3. 结束: 宿主发送 {"jsonrpc":"2.0","method":"shutdown"}（无 id），插件应退出。

# Python 插件模板（强烈推荐直接使用宿主提供的 SDK）
宿主已在 plugins/sdk/plugin_sdk.py 提供 SDK，插件源码这样写：

import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
from plugin_sdk import serve

def my_tool(args):
    # args 是 dict，返回任意可 str() 的结果
    return "结果字符串"

serve("<插件名>", {
    "my_tool": ("工具说明", {"type": "object", "properties": {"x": {"type": "string"}}}, my_tool),
})

# 规则
- 工具名全局唯一，必须用「插件名_功能」命名（例如 fs_read_file），避免与其他插件冲突。
- parameters 必须是合法的 JSON Schema object。
- 结果 content 必须是字符串；长结果尽量精炼。
- 每个插件保持单一职责；需要新能力就造新插件，也可以复用已有插件。
- 语言优先选 python；node 次之；rust 仅在没有依赖需求时使用（只能 std 单文件）。
- 插件禁止调用 input() 或任何等待人工输入的操作；除协议消息（单行 JSON）外不得向 stdout 写任何内容，调试信息一律写 stderr；每次写 stdout 后必须 flush（使用 SDK 则自动处理）。
- 插件 handler 必须快速返回；禁止死循环或长时间阻塞，否则会被判定超时并重启。
- 重建同名插件时直接用原名字即可（宿主的 create_plugin 会自动清理同名残留目录），不要为了绕开报错而改用 sysinfo2、sysinfo_pro 这类新名字。
- 插件工具默认自动执行；create_plugin / remove_plugin 等改变系统自身的元工具需要用户授权。
- 不要在只抛出一句「我先看看……」之类的计划后就结束回复。要么在同一条回复里直接发起工具调用，
  要么给出完整的最终结论；信息不足时明确说明还缺什么，而不是沉默结束。
- 任务完成后用自然语言向用户总结结果（不再调用任何工具即表示任务结束）。"#;

/// 把宿主的真实路径与运行约定写进系统提示，避免模型靠猜导致路径错乱。
fn env_section(rt: &Runtime) -> String {
    let mut s = format!(
        "\n\n# 运行环境（由宿主确定，直接使用，勿猜测）\n\
         - 应用根目录（＝插件进程的工作目录）: {root}\n\
         - 插件目录: {plugins}\n\
         - 插件源码路径形如: {plugins}\\<插件名>\\main.py\n\
         - 平台: {os}\n\
         - 所有相对路径都相对「应用根目录」。涉及本项目文件时，直接用上面这些绝对路径最稳妥。\n\
         - 插件子进程由宿主以「应用根目录」为工作目录启动，因此插件内的相对路径与你在宿主机上看到的一致。\n\
         - Python 插件已由宿主强制 UTF-8（PYTHONUTF8=1，PYTHONIOENCODING=utf-8），你无需再写编码兼容代码。\n\
         - 你的插件目录里只会有一个 plugins/（就是上面这个），不要再去 target/ 等构建产物目录里找。",
        root = rt.root.display(),
        plugins = rt.plugins_dir.display(),
        os = std::env::consts::OS,
    );
    // 把「本机到底有没有 python / node / rustc」如实告诉模型。
    // 否则它会在裸机上反复创建注定失败的 python 插件，把整轮任务耗光。
    s.push_str(&crate::runtimes::prompt_section(&rt.root, &rt.config.runtimes));
    s
}

/// 一轮任务的用量累加器。
///
/// 单独拎出来是因为 `run_loop` 可能中途 `bail!`（模型连续返回空回复等），
/// 而**已经花掉的 token 是收不回来的** —— 用 `&mut` 传进去，出错时调用方
/// 手里照样有一份完整账，能如实报给用户，而不是因为一个 `Err` 就把账丢了。
#[derive(Default)]
struct TurnAcc {
    calls: u32,
    total: Usage,
    /// 最后一次请求的输入规模 = 当前上下文占用
    last_prompt: Option<u64>,
}

pub struct Agent {
    llm: Arc<dyn LlmProvider>,
    rt: Arc<Runtime>,
    messages: Vec<Message>,
    /// 本次对话的落盘器。
    ///
    /// 惰性创建：只有真正产生了用户消息才建文件，所以「启动 → /help → 退出」
    /// 这种会话不会在 history/ 里留下一堆空文件。
    log: Option<crate::history::Log>,
    /// 落盘过程中的错误只报一次，避免每轮都刷同一句警告。
    log_error_reported: bool,
    /// 本次对话到目前的累计用量（切对话 / 新建对话时从文件接回或归零）
    session_usage: Usage,
    session_calls: u32,
    /// 当前对话**逐轮**的用量记录，与对话文件里的 `kind:"usage"` 行一一对应。
    ///
    /// 内存里留一份，是为了让界面立刻能答「这段对话一共花了多少、哪一轮最贵」，
    /// 而不必每帧读盘；文件那边才是权威 —— 重启后从这里接回来。
    usage_log: Vec<UsageRecord>,
}

impl Agent {
    pub fn new(llm: Arc<dyn LlmProvider>, rt: Arc<Runtime>, rules: Option<&str>) -> Self {
        let mut sys = SYSTEM_PROMPT.to_string();
        sys.push_str(&env_section(&rt));
        if let Some(r) = rules {
            let r = r.trim();
            if !r.is_empty() {
                sys.push_str("\n\n# 项目规则\n");
                sys.push_str(r);
            }
        }
        Self {
            llm,
            rt,
            messages: vec![Message::system(sys)],
            log: None,
            log_error_reported: false,
            session_usage: Usage::default(),
            session_calls: 0,
            usage_log: Vec::new(),
        }
    }

    /// 本次对话的历史文件 id（还没开始对话时为 None）。
    /// 文件名主干就是 id，所以不需要单独存一份。
    pub fn history_id(&self) -> Option<&str> {
        self.log
            .as_ref()
            .and_then(|l| l.path().file_stem())
            .and_then(|s| s.to_str())
    }

    /// 确保本次对话已有落盘文件。失败只警告一次，不打断对话。
    fn ensure_log(&mut self, ui: &mut dyn Ui) {
        if self.log.is_some() {
            return;
        }
        let cfg = &self.rt.config.history;
        if !cfg.enabled {
            return;
        }
        let dir = crate::history::dir(&self.rt.root);
        match crate::history::Log::create(
            &dir,
            Some(&self.rt.config.provider.model),
            &self.rt.root,
            cfg.keep,
        ) {
            Ok(log) => self.log = Some(log),
            Err(e) if !self.log_error_reported => {
                self.log_error_reported = true;
                ui.emit(AgentEvent::Warn(format!(
                    "对话历史无法写入 {}（{e:#}）：本次对话不会被保存",
                    dir.display()
                )));
            }
            Err(_) => {}
        }
    }

    /// 推入一条消息，并同步落盘。
    ///
    /// 所有对 `messages` 的写入都必须走这里 —— 落盘与内存只要有一处不同步，
    /// 历史文件就不再是这段对话的忠实记录。
    fn push_message(&mut self, msg: Message, is_error: Option<bool>) {
        if let Some(log) = self.log.as_mut() {
            log.append(&msg, is_error);
        }
        self.messages.push(msg);
    }

    /// 当前对话历史长度（用于中断时回滚）
    pub fn messages_len(&self) -> usize {
        self.messages.len()
    }

    /// 当前对话逐轮的用量记录（供界面显示「每轮各花了多少」）
    pub fn usage_log(&self) -> &[UsageRecord] {
        &self.usage_log
    }

    /// 追加上一批**活动流**记录（界面侧产生，写进同一份对话文件）。
    ///
    /// 为什么由这里写、而不是界面自己去开文件：文件句柄只有这个线程握着，
    /// 而且「新建对话」「中断回滚」也发生在这里 —— 多一个写入方就要多一套
    /// 跨线程的互斥与顺序保证，而这些本来靠线程内的一条队列就白拿了。
    ///
    /// `log` 为 `None` 时直接丢弃。正常情况下不会发生：活动流只在「一轮任务」
    /// 期间产生，而那一轮一开始就会 `ensure_log`。唯一的例外是配置里关掉了
    /// 历史（`[history] enabled = false`）—— 那本来就不该落盘。
    ///
    /// `allow(dead_code)`：`src/main.rs` 与 `src/lib.rs` 各自编译同一批模块
    /// （见 lib.rs 开头），而 CLI 那一份根本没有活动流，所以它眼里的这个方法是
    /// 死的。GUI 那一份在用。
    #[allow(dead_code)]
    pub fn append_acts(&mut self, acts: &[Value]) {
        let Some(log) = self.log.as_mut() else { return };
        for a in acts {
            log.append_act(a);
        }
    }

    /// 切换到一段已存档的对话：读回消息载入上下文，并把落盘器接回那个文件。
    ///
    /// 返回载入的消息条数（不含系统提示）。载入失败时上下文**保持原样** ——
    /// 半途清空再报错，用户会既丢了眼前的对话、又没拿到想要的那段。
    ///
    /// 载入的是全部历史。这让「接着上次聊」是真的接着聊，代价是对话越长
    /// 上下文越大；想开新话题就按「+」，那是另一条路径。
    pub fn resume(&mut self, path: &Path) -> Result<usize> {
        let items = crate::history::load(path)?;
        let log = crate::history::Log::open(path)?;
        // 只保留系统提示（第 0 条），其余全部换成存档里的消息。
        // 系统提示不入档是有意的（每轮都在变），所以它只能由这里保住。
        self.messages.truncate(1);
        self.messages.extend(items.iter().map(|i| i.msg.clone()));
        self.log = Some(log);
        self.log_error_reported = false;
        // 用量账本从文件接回来。不接的话，重启一次或切一次对话，
        // 之前那几轮的 token 就凭空消失了 —— 而「这段对话一共花了多少」
        // 恰恰是最该跨会话保留的数字。
        self.usage_log = crate::history::load_usage(path);
        self.session_usage = Usage::default();
        self.session_calls = 0;
        for r in &self.usage_log {
            self.session_usage.accrue(&r.usage);
            self.session_calls += r.calls;
        }
        Ok(items.len())
    }

    /// 开始一段新对话。
    ///
    /// 旧对话**不需要任何「保存」动作**：它是边聊边追加的，此刻已经完整地
    /// 躺在 `history/` 里了。这里只把落盘器置空 —— 下一轮会惰性新建一个文件，
    /// 于是「上一段对话」自然就归入历史。
    pub fn new_conversation(&mut self) {
        self.messages.truncate(1);
        self.log = None;
        self.log_error_reported = false;
        self.session_usage = Usage::default();
        self.session_calls = 0;
        self.usage_log.clear();
    }

    /// 中断当前任务后回滚对话历史，避免遗留「有 tool_calls 但无结果」的半截消息。
    ///
    /// 落盘文件同步回滚：内存里丢掉了什么，文件里也必须跟着丢掉，
    /// 否则会留下一段无法作为请求体回放的残记录。
    pub fn truncate_messages(&mut self, len: usize) {
        self.messages.truncate(len);
        if let Some(log) = self.log.as_mut() {
            log.rewrite(&self.messages);
        }
    }

    /// 处理一轮用户输入：循环「问模型 → 执行工具 → 回填结果」直到模型不再调用工具。
    pub async fn run_turn(&mut self, ui: &mut dyn Ui, input: &str) -> Result<()> {
        self.ensure_log(ui);
        self.push_message(Message::user(input), None);
        // 包住整轮：UI 借此显示等待动画，并在结束时把中间过程折叠成一行
        ui.turn_start().await;
        let mut acc = TurnAcc::default();
        let result = self.run_loop(ui, &mut acc).await;
        ui.turn_end().await;
        // 用量排在动画停掉之后再报（见 `Ui::turn_usage` 的注释）。
        // 出错也报：token 是真花出去了，不能因为任务失败就装作没花。
        if acc.calls > 0 {
            let rec = self.commit_usage(&acc);
            ui.turn_usage(&rec).await;
        }
        result
    }

    /// 把本轮的账并入会话累计、记进 `usage_log`、落进对话文件，返回这一行的记录。
    ///
    /// 落盘的与交给界面的是**同一个对象** —— 界面显示的账与文件里存的账因此不可能
    /// 对不上（这类「界面上一个数、文件里另一个数」的坑不值得踩第二次）。
    fn commit_usage(&mut self, acc: &TurnAcc) -> UsageRecord {
        self.session_usage.accrue(&acc.total);
        self.session_calls += acc.calls;
        let rec = UsageRecord {
            round: self.usage_log.len() as u32 + 1,
            at: crate::history::unix_now(),
            calls: acc.calls,
            usage: acc.total.clone(),
            last_prompt: acc.last_prompt,
            session_total: self.session_usage.total,
            session_calls: self.session_calls,
        };
        if let Some(log) = self.log.as_mut() {
            log.append_usage(&rec);
        }
        self.usage_log.push(rec.clone());
        rec
    }

    async fn run_loop(&mut self, ui: &mut dyn Ui, acc: &mut TurnAcc) -> Result<()> {
        /// 模型返回「空回复」（无正文、无工具调用）时的重试上限。
        /// 思考模型偶发会返回空 content（推理把输出额度吃光、上游抖动等），
        /// 直接当成「任务结束」会让任务静默中断，所以先重试并明确报错。
        const MAX_EMPTY_RETRY: u32 = 2;

        let mut empty_retries = 0u32;
        loop {
            let schemas = self.rt.tool_schemas().await;
            ui.set_status("思考中").await;
            // 把流式接收端交给 Provider：思考模型的推理过程会在等待期间实时回显
            let reply = {
                let sink = ui.stream_sink();
                self.llm.chat(&self.messages, &schemas, sink.as_deref()).await?
            };
            // 记账：端点不给 usage 时各项是 None，accrue 会安静跳过
            acc.calls += 1;
            acc.total.accrue(&reply.usage);
            if reply.usage.prompt.is_some() {
                acc.last_prompt = reply.usage.prompt;
            }
            let msg = reply.message.clone();
            let calls = msg.tool_calls.clone().unwrap_or_default();

            // 模型什么都没给（既无正文也无工具调用）——绝不能当成「任务正常结束」，
            // 否则表现就是「任务静默中断、模型没有任何返回」。先重试，再明确报错。
            if reply.is_empty() {
                if reply.truncated() {
                    ui.emit(AgentEvent::Warn(format!(
                        "模型输出被 max_tokens={} 截断且正文为空（推理 token 占满了输出额度）。",
                        self.rt.config.provider.max_tokens.unwrap_or(0)
                    )));
                }
                if empty_retries < MAX_EMPTY_RETRY {
                    empty_retries += 1;
                    ui.emit(AgentEvent::Warn(format!(
                        "模型返回空回复（第 {empty_retries} 次），正在重试…"
                    )));
                    self.push_message(
                        Message::user(
                            "（宿主提示）你上一条回复是空的。若任务尚未完成，请继续调用工具；\
                             若已完成，请用自然语言给出最终总结。",
                        ),
                        None,
                    );
                    continue;
                }
                bail!(
                    "模型连续 {} 次返回空回复，本轮任务已停止。\n\
                     诊断：{}。可执行 `AOX_DEBUG_LLM=1 aox` 复现，原始响应会写入 logs/llm.jsonl。\n\
                     常见原因：① 模型是思考模式但 max_tokens 太小（推理 token 也算额度）；\
                     ② 上游限流/超时；③ 该模型不支持工具调用。",
                    MAX_EMPTY_RETRY + 1,
                    reply.diag(),
                );
            }
            empty_retries = 0;

            if reply.truncated() {
                ui.emit(AgentEvent::Warn(format!(
                    "模型输出被 max_tokens={} 截断（{}），本轮内容可能不完整。\
                     建议调大 config.toml 的 max_tokens（思考模型的推理 token 也计入额度）。",
                    self.rt.config.provider.max_tokens.unwrap_or(0),
                    reply.diag(),
                )));
            }

            // 有工具调用 → 这段文字只是过程说明；没有工具调用 → 才是最终答复
            if let Some(text) = msg.text().map(str::to_string) {
                if calls.is_empty() {
                    ui.emit(AgentEvent::AssistantText(text));
                } else {
                    ui.emit(AgentEvent::InterimText(text));
                }
            }
            self.push_message(msg, None);
            if calls.is_empty() {
                break; // 任务结束
            }

            for call in calls {
                let name = call.function.name.clone();
                let args: Value = match serde_json::from_str(&call.function.arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        let out = ToolOutput::err(format!("工具参数不是合法 JSON: {e}"));
                        ui.emit(AgentEvent::ToolCall { name: name.clone(), args: json!({}) });
                        ui.emit(AgentEvent::ToolResult { name: name.clone(), is_error: true, preview: out.content.clone() });
                        self.push_message(Message::tool_result(&call.id, &name, out.content), Some(true));
                        continue;
                    }
                };
                ui.emit(AgentEvent::ToolCall { name: name.clone(), args: args.clone() });
                let output = self.authorize_and_execute(ui, &name, args).await;
                ui.emit(AgentEvent::ToolResult {
                    name: name.clone(),
                    is_error: output.is_error,
                    preview: truncate(&output.content, 300),
                });
                let content = cap_chars(&output.content, self.rt.config.agent.max_result_chars);
                self.push_message(
                    Message::tool_result(&call.id, &name, content),
                    Some(output.is_error),
                );
            }
        }
        Ok(())
    }

    async fn authorize_and_execute(&mut self, ui: &mut dyn Ui, name: &str, args: Value) -> ToolOutput {
        let kind = self.rt.tool_kind(name).await.unwrap_or(ToolKind::Meta);
        let level = self.rt.auth.level_for(&kind, name);
        let allowed = match level {
            AuthLevel::Auto => true,
            AuthLevel::Deny => false,
            AuthLevel::Ask => {
                if self.rt.session_allowed.read().await.contains(name) {
                    true
                } else {
                    match ui.authorize(name, &args).await {
                        AuthDecision::Allow => true,
                        AuthDecision::AllowSession => {
                            self.rt.session_allowed.write().await.insert(name.to_string());
                            true
                        }
                        AuthDecision::Deny => false,
                    }
                }
            }
        };
        if allowed {
            ui.emit(AgentEvent::ToolRunning {
                name: name.to_string(),
                timeout_secs: self.rt.config.agent.tool_timeout_secs,
            });
            self.rt.execute(name, args).await
        } else {
            ToolOutput::err("用户拒绝了这次工具调用。请改用其他方式完成任务，或向用户说明原因。")
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn cap_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        let total = s.chars().count();
        format!("{}…\n[结果过长已截断，原长度 {total} 字符]", s.chars().take(cap).collect::<String>())
    }
}
