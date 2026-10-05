//! 循环缝（Loop 缝，第二形态）：把「跑完一整轮」从 `Agent` 里搬出来，变成可替换的能力。
//!
//! 阶段 4 的钩子只能**在循环内部**插五个点 —— 骨架本身还是焊死的。本模块把整段循环
//! 交给一个 [`AgentLoop`] 实现，于是「裁剪上下文」「换一套重试策略」「甚至完全不同的
//! 执行模型」都成了可替换的东西，而不是只能在固定骨架上挂钩子。
//!
//! # 为什么循环搬到外面，却要给它开 `Agent` 的写入口
//!
//! `run_loop` 原先住在 `Agent` 里，用的是 `push_message` / `truncate_messages` / 回合记账
//! 这些**私有**写入口。搬到 `AgentLoop` 之后它成了 `Agent` 的**同层协作者**，
//! 所以 `Agent` 为此开了一组受控原语（`push_message` / `truncate_messages` / `messages` /
//! `begin_turn` / `accrue_call` / `finish_turn`）。
//!
//! 这**不是**「接口泄漏」—— 阶段 3 那条「接口上不出现路径」约束的是**后端实现细节**，
//! 而这里是同一层的两个类型在协作。真正必须守住的那条线没有变：
//! **`messages` 的唯一写入口是 `Agent::push_message`**（它同时写文件），
//! 循环拿不到裸 `Vec<Message>`，也就造不出「内存里有、历史文件里没有」的错位。
//!
//! # 本期只有一个实现
//!
//! [`StandardLoop`] = 阶段 4 的 `Agent::run_loop` **逐语句搬运**，行为逐条等价。
//! 它是 `config.toml [loop] impl` 唯一认的值 —— 别的值在 `Runtime::new` 里**启动即报错**，
//! 与 `[store] backend` 同一条原则（写了却不生效是最难查的一类问题）。
//!
//! [`StandardLoop`] 顺带兑现了**阶段 4 遗留清单的第 1 条：裁剪 / 改写历史**。
//! 阶段 4 的 `before_request` 只敢开放「追加消息」（追加是唯一不需要 `rewrite` 的改动），
//! 现在这条边界由 `[loop] max_rounds` 补上 —— 裁剪天然属于循环这一层：
//! 它要同时动内存与对话文件，还要在裁完**重新拼请求**。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agent::Agent;
use crate::config::LoopConfig;
use crate::hook::{self, ErrorAction, Verdict};
use crate::message::Message;
use crate::runtime::Runtime;
use crate::tool::ToolOutput;
use crate::ui::{AgentEvent, Ui};

/// 一次「跑完一整轮」的实现。
///
/// `run` 接手时的状态：用户消息**已经**入列（`Agent::run_turn` 做的），本轮的账也已经开了
/// （`Agent::begin_turn`）。返回 `Ok(())` 表示模型不再调用工具、本轮正常结束。
#[async_trait]
pub trait AgentLoop: Send + Sync {
    /// 实现名，即 `config.toml [loop] impl` 认的那个值。
    fn name(&self) -> &str;
    /// 跑完一整轮。
    async fn run(&self, agent: &mut Agent, ui: &mut dyn Ui) -> Result<()>;
}

/// 已知的循环实现名。加实现时同步改这里与 [`build`]。
pub const IMPLS: [&str; 1] = ["standard"];

/// 默认实现名。`config.toml` 里不写 `[loop] impl` 时用的就是它。
pub fn default_impl() -> &'static str {
    IMPLS[0]
}

/// 按配置造一个循环实现。
///
/// **认不出的名字直接报错**，不静默退回 `standard` —— 理由与 `store::backend` 完全相同：
/// 配置写错却照常启动，表现就是「我明明写了别的实现，怎么还是原来那套」。
pub fn build(cfg: &LoopConfig) -> Result<Arc<dyn AgentLoop>> {
    let name = cfg.implementor.trim();
    if name.is_empty() || name == default_impl() {
        return Ok(Arc::new(StandardLoop));
    }
    bail!(
        "未知的循环实现: {name}（当前可用: {}）\n\
         请检查 config.toml 的 [loop] impl，或删掉它使用默认值。",
        IMPLS.join(" / ")
    )
}

/// `/loop` 命令的正文：现在生效的是哪个实现、上下文裁剪开没开。
pub fn report(rt: &Runtime) -> String {
    let cfg = &rt.config.loop_config;
    let trim = if cfg.max_rounds == 0 {
        "关闭（max_rounds = 0，上下文不设上限）".to_string()
    } else {
        format!(
            "保留最近 {} 轮（一轮从一条 user 消息开始）",
            cfg.max_rounds
        )
    };
    format!(
        "循环实现: {}（可选: {}）\n\
         上下文裁剪: {trim}\n\n\
         改 config.toml 的 [loop] 段后重启生效；/hook 看钩子、/prompt 看提示词段。",
        rt.loop_impl.name(),
        IMPLS.join(" / ")
    )
}

/// 上下文裁剪的**切点**：返回「新的正文起点下标」，`messages[1..cut]` 会被丢掉。
///
/// `None` = 不需要裁（也包含「不该裁」，见下）。
///
/// 规则：
/// - `max_rounds == 0` → 不裁。这是默认值，也是「不写 `[loop]` 就与阶段 5 逐字节相同」的根据。
/// - **轮次以 `role == "user"` 的消息为界**：一条 user 开启一轮，直到下一条 user 之前
///   （assistant / tool 消息都归它）。宿主钩子注入的提示也是 user 消息，同样各占一轮。
/// - 轮数没超过 `max_rounds` → 不裁。
/// - 切点**天然落在一条 user 消息上**，所以不需要额外对齐：丢掉最老的整轮，剩下的后缀
///   本身就是一个合法请求体 —— 不会出现「tool 结果留着、它的 assistant 被丢了」这种
///   端点会直接 400 拒掉的状态。这条是整个裁剪机制唯一真正的危险点，见下面的 `debug_assert`。
fn trim_point(msgs: &[Message], max_rounds: usize) -> Option<usize> {
    if max_rounds == 0 {
        return None;
    }
    // 下标 0 是 system 段，永远保留 —— 所以轮次从下标 1 开始数。
    let turns: Vec<usize> = msgs
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, m)| m.role == "user")
        .map(|(i, _)| i)
        .collect();
    if turns.len() <= max_rounds {
        return None;
    }
    let cut = turns[turns.len() - max_rounds];
    debug_assert_eq!(msgs[cut].role, "user", "裁剪切点必须落在一条 user 消息上");
    Some(cut)
}

/// 标准循环：阶段 4 的 `Agent::run_loop`，一行逻辑不改。
pub struct StandardLoop;

#[async_trait]
impl AgentLoop for StandardLoop {
    fn name(&self) -> &str {
        "standard"
    }

    async fn run(&self, agent: &mut Agent, ui: &mut dyn Ui) -> Result<()> {
        /// 模型返回「空回复」（无正文、无工具调用）时的重试上限。
        /// 思考模型偶发会返回空 content（推理把输出额度吃光、上游抖动等），
        /// 直接当成「任务结束」会让任务静默中断，所以先重试并明确报错。
        const MAX_EMPTY_RETRY: u32 = 2;

        let mut empty_retries = 0u32;
        // 请求出错时的宿主兜底重试预算（`[hook] max_llm_retries`）。**每轮独立** ——
        // 它活在 `run` 的局部，跨不过下一轮，所以「越聊越能重试」不会发生。
        let mut llm_retries = 0usize;
        // 钩子要能查系统的当前状态（工具表、插件表），而 `agent.messages()` 马上要借给
        // `llm.chat`。先克隆一个 `Arc` 出来：钩子持它，既不与 `agent` 的借用打架，
        // 也顺带保证了「钩子改不了 Agent 的内部状态」—— 它只能靠返回值影响循环。
        let rt = agent.rt().clone();
        let max_rounds = rt.config.loop_config.max_rounds;
        let max_llm_retries = rt.config.hook.max_llm_retries;
        loop {
            // ── 上下文裁剪（阶段 4 遗留清单第 1 条）。
            //    位置在「本轮请求拼出来之前」：裁完再拼，模型看到的就是裁过的上下文。
            //    内存与对话文件一起动（`truncate_messages` 内部会 `Session::rewrite`），
            //    否则重启读回来就是一段半截历史。
            if let Some(cut) = trim_point(agent.messages(), max_rounds) {
                let dropped = cut - 1;
                // 注意用 `drop_oldest`（留尾部）而不是 `truncate_messages`（留头部）——
                // 裁剪要的是「只留最近几轮」，方向反了会静默留下最老的上下文。
                agent.drop_oldest(cut);
                ui.emit(AgentEvent::Info(format!(
                    "上下文超过 {max_rounds} 轮，已丢掉最老的 {dropped} 条消息（内存与对话文件已同步回滚）。"
                )));
            }

            // ── 钩子点 ① 请求前。
            //    钩子拿到的是「本轮用哪些工具」与「要追加哪些消息」，**不是** `&mut messages`：
            //    messages 的唯一写入口是 `push_message`（它同时写文件），把裸 Vec 交出去，
            //    「内存里有、历史文件里没有」就是一分钟后必然发生的事。
            let mut req = hook::Request { tools: rt.tool_schemas().await, inject: Vec::new() };
            if let Verdict::Reject(why) = rt.hooks.before_request(&rt, &mut req).await {
                bail!("本轮请求被宿主钩子拦下：{why}");
            }
            for m in req.inject.drain(..) {
                agent.push_message(m, None);
            }
            let schemas = req.tools;

            ui.set_status("思考中").await;
            // 把流式接收端交给 Provider：思考模型的推理过程会在等待期间实时回显。
            // 单独写成一个语句（而不是直接在下面 match 结果）是为了让对 `agent.messages()`
            // 的借用在这里就结束 —— 否则下面 `push_message` 要的 `&mut agent` 会冲突。
            let chat = {
                let sink = ui.stream_sink();
                agent.llm().chat(agent.messages(), &schemas, sink.as_deref()).await
            };
            let reply = match chat {
                Ok(r) => r,
                // ── 钩子点 ⑤ 请求出错。
                //    这是循环里唯一的「基础设施故障」出口（工具执行失败、空回复退出的
                //    `bail!` 都不走这里）。没有钩子、或钩子都返回 `Propagate` 时，
                //    这里就等价于原来的 `?`。
                Err(e) => match rt.hooks.on_error(&rt, &e).await {
                    // 组件/钩子明确说要重试：按它给的提示注入后重来，**不看预算** ——
                    // 防死循环是它自己的责任（见 `ErrorAction::Retry` 的文档）。
                    ErrorAction::Retry(note) => {
                        agent.push_message(Message::user(note), None);
                        continue;
                    }
                    // 没人要求重试 —— 但「没人管」不该等于「一次都不试」。
                    // `[hook] max_llm_retries` 就是给这种情况的配置兜底：偶发的上游抖动
                    // 值得再问一次，而不必为此写一个组件。默认 0，所以**不写配置时
                    // 这里与原实现逐字节等价**（直接抛出）。
                    ErrorAction::Propagate => {
                        if llm_retries >= max_llm_retries {
                            return Err(e);
                        }
                        llm_retries += 1;
                        ui.emit(AgentEvent::Warn(format!(
                            "模型请求失败，宿主自动重试（第 {llm_retries}/{max_llm_retries} 次）：{e:#}"
                        )));
                        agent.push_message(
                            Message::user(format!(
                                "（宿主提示）上一次模型请求失败：{e:#}。请继续完成刚才的任务。"
                            )),
                            None,
                        );
                        continue;
                    }
                },
            };
            // 记账：端点不给 usage 时各项是 None，accrue 会安静跳过
            agent.accrue_call(&reply.usage);

            // ── 钩子点 ② 回复后。
            //    特意排在记账**之后**：被否决的那次调用的 token 是真花掉了，
            //    不能因为它没被采用就不记账。
            //    否决 = 视为一次无效回复 → 复用下面「空回复」那条重试路径（连上限一起），
            //    不另造一套重试逻辑。
            let mut reply = reply;
            if let Verdict::Reject(why) = rt.hooks.after_reply(&rt, &mut reply).await {
                if empty_retries < MAX_EMPTY_RETRY {
                    empty_retries += 1;
                    ui.emit(AgentEvent::Warn(format!(
                        "宿主钩子否决了这次回复（第 {empty_retries} 次）：{why}"
                    )));
                    agent.push_message(
                        Message::user(format!(
                            "（宿主提示）你上一条回复被宿主规则否决：{why}。请调整后重试。"
                        )),
                        None,
                    );
                    continue;
                }
                bail!("模型回复连续 {} 次被宿主钩子否决：{why}", MAX_EMPTY_RETRY + 1);
            }

            let msg = reply.message.clone();
            let calls = msg.tool_calls.clone().unwrap_or_default();

            // 模型什么都没给（既无正文也无工具调用）——绝不能当成「任务正常结束」，
            // 否则表现就是「任务静默中断、模型没有任何返回」。先重试，再明确报错。
            if reply.is_empty() {
                if reply.truncated() {
                    ui.emit(AgentEvent::Warn(format!(
                        "模型输出被 max_tokens={} 截断且正文为空（推理 token 占满了输出额度）。",
                        rt.config.provider.max_tokens.unwrap_or(0)
                    )));
                }
                if empty_retries < MAX_EMPTY_RETRY {
                    empty_retries += 1;
                    ui.emit(AgentEvent::Warn(format!(
                        "模型返回空回复（第 {empty_retries} 次），正在重试…"
                    )));
                    agent.push_message(
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
                    rt.config.provider.max_tokens.unwrap_or(0),
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
            agent.push_message(msg, None);
            if calls.is_empty() {
                break; // 任务结束
            }

            // 本次响应里的**工具产出文件**（`attach_file` 挂的、或插件在结果里声明的）。
            // 收集起来，等这一批工具都跑完再统一注入一条 user 消息 —— 理由见下面那段。
            let mut produced: Vec<PathBuf> = Vec::new();
            for call in calls {
                let name = call.function.name.clone();
                let mut args: Value = match serde_json::from_str(&call.function.arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        // 参数不是合法 JSON 时还没有「可改的对象」，钩子无从插手 ——
                        // 照原样回一条错误结果（与加钩子之前完全一致）。
                        let out = ToolOutput::err(format!("工具参数不是合法 JSON: {e}"));
                        ui.emit(AgentEvent::ToolCall { name: name.clone(), args: json!({}) });
                        ui.emit(AgentEvent::ToolResult { name: name.clone(), is_error: true, preview: out.content.clone() });
                        agent.push_message(Message::tool_result(&call.id, &name, out.content), Some(true));
                        continue;
                    }
                };
                // ── 钩子点 ③ 工具调用前：可改 args，可拦下（拦下就不执行工具）。
                //    ⚠️ 带 tool_calls 的 assistant 消息此刻**已经落盘**，文件里留的是模型
                //    原始参数 —— 这里改的只影响本次执行，**不回写历史**：历史要如实记录
                //    「模型要了什么」，钩子只是宿主侧的执行策略。
                if let Verdict::Reject(why) = rt.hooks.before_tool(&rt, &name, &mut args).await {
                    let out = ToolOutput::err(why);
                    ui.emit(AgentEvent::ToolCall { name: name.clone(), args: args.clone() });
                    ui.emit(AgentEvent::ToolResult { name: name.clone(), is_error: true, preview: out.content.clone() });
                    agent.push_message(Message::tool_result(&call.id, &name, out.content), Some(true));
                    continue;
                }
                ui.emit(AgentEvent::ToolCall { name: name.clone(), args: args.clone() });
                let mut output = agent.authorize_and_execute(ui, &name, args).await;
                // ── 钩子点 ④ 工具结果后：可改结果，可拦下（拦下 = 整块换成带理由的错误）。
                //    排在 `ui.emit` 之前 —— 界面与历史文件因此看到的是**同一份**（改过的）结果。
                if let Verdict::Reject(why) = rt.hooks.after_tool(&rt, &name, &mut output).await {
                    output = ToolOutput::err(why);
                }
                ui.emit(AgentEvent::ToolResult {
                    name: name.clone(),
                    is_error: output.is_error,
                    preview: truncate(&output.content, 300),
                });
                let content = cap_chars(&output.content, rt.config.agent.max_result_chars);
                produced.extend(output.attach.iter().cloned());
                agent.push_message(
                    Message::tool_result(&call.id, &name, content),
                    Some(output.is_error),
                );
            }

            // ── 工具产出的文件 → 另起一条 **user** 消息
            //
            // 这不是风格选择，是协议硬约束：端点在 `system` / `assistant` 消息里见到图片
            // 直接 400（官方「使用限制」明写），而 `tool` 角色在 OpenAI 协议里 content
            // 只能是字符串 —— **图片的合法落点只有 user 消息这一处**。
            //
            // 位置也是被这条约束定死的：必须排在刚推入的 tool 结果**之后**（tool 结果要
            // 紧跟它对应的 assistant.tool_calls，中间插一条别的消息会让参数校验失败），
            // 又要在下一次请求**之前**（否则这一轮的图赶不上这一轮）。
            if !produced.is_empty() {
                let (msg, problems) = produced_files_message(&produced);
                for p in &problems {
                    ui.emit(AgentEvent::Warn(format!("工具产出的附件没能传给模型：{p}")));
                }
                if let Some(m) = msg {
                    agent.push_message(m, None);
                }
            }
        }
        Ok(())
    }
}

/// 组装「工具产出文件」那条 user 消息。
///
/// 返回 `(要推入的消息, 要报给用户的说明)`，`None` 表示一个都挂不上。
///
/// 抽成**纯函数**（不碰 `Ui`）是为了能直接断言 —— 「必须是 user 消息、必须带 parts 数组」
/// 这条协议约束值得一条不必真跑一轮循环的测试。
fn produced_files_message(paths: &[PathBuf]) -> (Option<Message>, Vec<String>) {
    let (items, mut problems) = crate::attach::probe_all(paths);
    if items.is_empty() {
        return (None, problems);
    }
    let names = items.iter().map(|a| format!("- {}", a.name)).collect::<Vec<_>>().join("\n");
    // 措辞说清「该看什么」：不说的话模型常会只根据文件名猜内容，那这张图就白发了
    let note = format!(
        "（宿主提示）以下是刚才那次工具调用产出的文件，图片内容已经直接发给你了 —— \
         请直接看图判断，不要只根据文件名猜：\n{names}"
    );
    match Message::user_with_files(&note, &items) {
        Ok(m) => (Some(m), problems),
        Err(e) => {
            problems.push(e);
            (None, problems)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FunctionCall, ToolCall};

    fn call(n: usize) -> ToolCall {
        ToolCall {
            id: format!("call-{n}"),
            kind: "function".into(),
            function: FunctionCall { name: "fs_read".into(), arguments: "{}".into() },
        }
    }

    /// 一轮的完整形状：user → assistant(带工具调用) → tool 结果 → assistant(最终答复)。
    fn one_turn(n: usize) -> Vec<Message> {
        vec![
            Message::user(format!("问题 {n}")),
            Message::assistant(None, Some("想一下".into()), vec![call(n)]),
            Message::tool_result(&format!("call-{n}"), "fs_read", format!("第 {n} 份内容")),
            Message::assistant(Some(format!("答复 {n}")), None, vec![]),
        ]
    }

    /// `turns` 轮的完整正文（下标 0 是 system 头）。
    fn body(turns: usize) -> Vec<Message> {
        let mut v = vec![Message::system("SYS")];
        for n in 1..=turns {
            v.extend(one_turn(n));
        }
        v
    }

    #[test]
    fn no_max_rounds_means_never_trim() {
        assert_eq!(trim_point(&body(9), 0), None, "默认 max_rounds=0 必须完全不裁");
    }

    #[test]
    fn under_or_equal_the_limit_is_untouched() {
        assert_eq!(trim_point(&body(2), 5), None);
        assert_eq!(trim_point(&body(5), 5), None, "刚好等于上限不裁");
    }

    #[test]
    fn over_limit_cuts_oldest_whole_turns() {
        let msgs = body(3); // [sys, u1,a,t,a, u2,a,t,a, u3,a,t,a]
        let cut = trim_point(&msgs, 2).expect("3 轮超过 2 轮上限，应当裁");
        assert_eq!(cut, 5, "第 2 轮的 user 在第 5 位");
        assert_eq!(msgs[cut].content.as_ref().and_then(|v| v.as_str()), Some("问题 2"));
        assert_eq!(cut - 1, 4, "丢掉的是完整的一轮（4 条），不含 system");
    }

    /// 裁剪唯一危险的地方：留下一条「tool 结果、但它的 assistant 已被丢掉」的消息，
    /// 端点会直接 400。这条把「切点永远落在一条 user 上」这个不变式钉住 ——
    /// 也正是它让「不需要额外对齐」这个结论成立。
    #[test]
    fn cut_always_lands_on_a_turn_start() {
        for turns in 1..8usize {
            let msgs = body(turns);
            for keep in 1..=turns {
                if let Some(cut) = trim_point(&msgs, keep) {
                    assert!(cut > 0 && cut < msgs.len(), "轮数 {turns} 保留 {keep} 时切点越界");
                    assert_eq!(msgs[cut].role, "user", "轮数 {turns} 保留 {keep} 时切点歪了");
                    assert_eq!(
                        msgs[cut - 1].role,
                        "assistant",
                        "丢弃段的最后一条应当是上一轮的收尾，否则就是把一轮劈成两半"
                    );
                }
            }
        }
    }

    #[test]
    fn system_head_is_never_cut() {
        let msgs = body(4);
        let cut = trim_point(&msgs, 1).unwrap();
        assert_eq!(msgs[0].role, "system", "system 永远在下标 0，不参与裁剪");
        assert!(cut > 0);
    }

    /// 轮数不够时即使 `max_rounds` 很大也不许裁 —— 「上限」是上限，不是「必须裁到只剩它」。
    #[test]
    fn trim_never_grows_the_context() {
        for keep in [1usize, 2, 7] {
            let msgs = body(3);
            if let Some(cut) = trim_point(&msgs, keep) {
                assert!(cut < msgs.len(), "裁剪只会变短，不会变长");
            }
        }
    }

    /// 造一个不碰真实目录、也不认识任何 Provider 的 `Agent`（`log` 为 `None`，
    /// 所以下面的 `push_message` 只动内存 —— 正好把「裁剪方向」单独隔离出来测）。
    fn test_agent() -> Agent {
        let root = std::env::temp_dir().join("aox-loop-test");
        let rt = Runtime::new(crate::config::Config::dummy(), root.clone(), root.join("plugins"))
            .expect("测试用运行时不应失败");
        let llm: Arc<dyn crate::llm::LlmProvider> =
            Arc::new(crate::llm::UnavailableProvider { reason: "test".into() });
        Agent::new(llm, rt)
    }

    /// ⚠️ 这条专门钉住**方向**：裁剪必须走 `Agent::drop_oldest`（留尾部），
    /// 而不是 `truncate_messages`（留头部）。写实现时正是把二者搞混，
    /// 结果「本轮输入被丢掉、最老的上下文还留着」—— 而且内存与文件**一致地错**，
    /// 光看落盘内容根本发现不了。所以这条测试断言的是「留下哪几轮」。
    #[test]
    fn trimming_keeps_the_newest_turns_not_the_oldest() {
        let mut agent = test_agent();
        for m in body(3).into_iter().skip(1) {
            agent.push_message(m, None);
        }
        assert_eq!(agent.messages().len(), 13, "1 条 system + 3 轮 × 4 条");

        let cut = trim_point(agent.messages(), 2).expect("3 轮超过 2 轮上限，应当裁");
        agent.drop_oldest(cut);

        let roles: Vec<&str> = agent.messages().iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles.len(), 9, "system + 最后 2 轮");
        assert_eq!(roles[0], "system", "系统提示永远保留");
        assert_eq!(roles[1], "user");
        assert_eq!(
            agent.messages()[1].content.as_ref().and_then(|v| v.as_str()),
            Some("问题 2"),
            "留下的必须是**最新**两轮，不是最老两轮"
        );
        assert_eq!(
            agent.messages().last().unwrap().content.as_ref().and_then(|v| v.as_str()),
            Some("答复 3"),
            "最后一轮要完整保留到它的最终答复"
        );
    }

    /// `drop_oldest` 的边界：切点没意义时一条都不动（宁可留着完整上下文，也不裁成空壳）。
    #[test]
    fn drop_oldest_is_a_noop_on_meaningless_cuts() {
        let mut agent = test_agent();
        for m in body(2).into_iter().skip(1) {
            agent.push_message(m, None);
        }
        let before = agent.messages().len();
        for cut in [0usize, 1, before, before + 5] {
            agent.drop_oldest(cut);
            assert_eq!(agent.messages().len(), before, "cut = {cut} 不该动任何东西");
        }
    }

    #[test]
    fn unknown_impl_is_an_error() {
        let cfg = LoopConfig { implementor: "react".into(), max_rounds: 0 };
        let e = build(&cfg).err().expect("未知实现必须报错").to_string();
        assert!(e.contains("react"), "报错要带上写错的那个名字: {e}");
        assert!(e.contains("standard"), "报错要告诉用户当前可用的是什么: {e}");
    }

    #[test]
    fn empty_or_default_impl_builds_standard() {
        for name in ["", "standard", "  standard  "] {
            let cfg = LoopConfig { implementor: name.into(), max_rounds: 0 };
            let lp = build(&cfg).unwrap_or_else(|e| panic!("{name:?} 应可用: {e:#}"));
            assert_eq!(lp.name(), "standard");
        }
    }

    /// 工具产出的图片必须变成一条 **user** 消息，且 `content` 是 parts 数组。
    ///
    /// 这两条都是**协议硬要求**，不是风格：图片出现在 `tool` / `system` / `assistant`
    /// 消息里端点直接 400；而只有 parts 数组装得下图片，纯字符串装不下。
    #[test]
    fn produced_images_become_a_user_message_with_parts() {
        let d = std::env::temp_dir().join("aox-loop-produced");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let shot = d.join("shot.png");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();

        let (msg, problems) = produced_files_message(&[shot]);
        assert!(problems.is_empty(), "{problems:?}");
        let msg = msg.expect("挂得上的文件必须产出一条消息");
        assert_eq!(msg.role, "user", "图片只能出现在 user 消息里");
        let parts = msg.content.as_ref().unwrap().as_array().expect("必须是 parts 数组");
        assert!(parts.iter().any(|p| p["type"] == "image_url"), "{parts:?}");
        // 前缀是给界面认的：带这个前缀的消息渲染成提示，而不是一枚用户气泡
        assert!(msg.flat_text().starts_with("（宿主提示）"), "{}", msg.flat_text());
        assert!(msg.flat_text().contains("shot.png"), "{}", msg.flat_text());

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 插件报了一条不存在的路径（插件作者最常犯的错）：**不能静默**。
    ///
    /// 静默的后果是双向的 —— 作者那边一直等一张永远不会出现的图，模型那边则完全
    /// 不知道自己缺了东西，于是照着文件名猜内容。
    #[test]
    fn a_missing_produced_file_is_reported_not_swallowed() {
        let missing = std::env::temp_dir().join("aox-loop-no-such-file-xyz.png");
        let _ = std::fs::remove_file(&missing);
        let (msg, problems) = produced_files_message(&[missing]);
        assert!(msg.is_none());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("no-such-file-xyz.png"), "{}", problems[0]);
    }

    /// 一批里只有一半合格时：合格的要挂上，不合格的要出声 —— 不是整批丢掉。
    #[test]
    fn one_bad_file_does_not_sink_the_whole_batch() {
        let d = std::env::temp_dir().join("aox-loop-mixed");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let good = d.join("good.png");
        std::fs::write(&good, b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        let missing = d.join("missing.png");

        let (msg, problems) = produced_files_message(&[good, missing]);
        assert!(msg.is_some(), "合格那个必须照挂");
        assert_eq!(problems.len(), 1, "{problems:?}");

        let _ = std::fs::remove_dir_all(&d);
    }
}
