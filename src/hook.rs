//! 循环 hook（Loop 缝的第一形态）。
//!
//! 在循环骨架上挂 5 个钩子点，**不动骨架本身**：请求前、回复后、
//! 工具调用前、工具结果后、请求出错时。这五个点是「改系统行为」的 80% 解法 ——
//! 剩下的 20%（整体换掉循环）由 [`crate::agent_loop`] 的 `AgentLoop` 负责（阶段 6 · 6a
//! 已落地：`StandardLoop` 就是这几个点原先住的那段骨架）。
//!
//! | 钩子点 | 位置 | 能做的事 | 否决后 |
//! |---|---|---|---|
//! | [`Hook::before_request`] | 请求前 | 换工具列表、追加消息 | 中止本轮 |
//! | [`Hook::after_reply`] | 回复后（**记账之后**） | 改/清空这条回复 | 视为无效回复，走空回复重试路径 |
//! | [`Hook::before_tool`] | 工具调用前 | 改 `args` | 不执行工具，把理由当工具结果回填 |
//! | [`Hook::after_tool`] | 工具结果后 | 改结果 | 整块换成带理由的错误结果 |
//! | [`Hook::on_error`] | `llm.chat` 出错处 | 降级重试 | — |
//!
//! # 没有钩子时，行为与加钩子之前完全一样
//!
//! [`Hook`] 的 5 个方法**全部带默认空实现**；`config.toml [hook]` 为空时
//! `Runtime.hooks` 是一条空链。空链的每次调用都退化成「查一下长度然后返回」，
//! 因此循环逐条路径与加钩子之前**等价** —— 这是本阶段的验收硬指标。
//!
//! # 为什么钩子拿不到 `&mut Vec<Message>`
//!
//! `before_request` 能改东西，但**拿不到** `&mut messages`：它只能把自己想说的话放进
//! [`Request::inject`]，由循环经 `Agent::push_message` 入列。
//!
//! 原因：`messages` 的唯一写入口是 `Agent::push_message`，它**同时**写文件；
//! 回滚走 `truncate_messages` + `rewrite`。把裸 `Vec` 交给钩子，等于开了一个
//! 「内存里有、历史文件里没有」的后门 —— 那不是万一，是一分钟后必然发生的事。
//!
//! 「裁剪 / 改写历史」这条当时有意留下的口子，**已由阶段 6 · 6a 的 `[loop] max_rounds`
//! 补上**：裁剪天然属于循环那一层（它要同时动内存与对话文件，还要在裁完重新拼请求）。
//! 钩子这一层仍然只开放「追加」。
//!
//! # 两个来源：配置驱动 + 组件贡献（阶段 6 · 6b）
//!
//! - [`ConfigHook`] —— 来自 `config.toml [hook]`，启动时定型；
//! - [`ComponentHook`] —— 来自插件 `plugin.toml [contributes] hooks`，插件激活时挂上。
//!
//! 后者走的是[已有的子进程 JSON-RPC 协议](crate::plugin)：子进程插件本来就是
//! 「一次工具调用一次 RPC」的成本量级，所以**撑得起钩子** —— 不需要为此往宿主里
//! 塞一个脚本引擎。这条判断是阶段 6 路线与原始设想最大的分歧点（原文假设
//! 「钩子必须是进程内的」，而目标本身并不需要）。
//!
//! 组件子进程挂掉 / 超时 / 返回垃圾 = 该组件**静默失效**：记一笔失败账后放行，
//! 主循环照常跑完。「必须可降级」是靠 [`ComponentHook`] 每个出口都有兜底值实现的。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::config::HookConfig;
use crate::llm::ChatReply;
use crate::message::Message;
use crate::runtime::Runtime;
use crate::tool::ToolOutput;

/// 钩子对「即将发生的这一步」的裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 放行 —— 默认值，也是「一个钩子都没有」时的结果。
    Pass,
    /// 拦下这一步。`reason` 一律要能见人：它会被回给模型，或当作本轮的中止理由。
    Reject(String),
}

/// `before_request` 能看到、能改的那部分请求。
pub struct Request {
    /// 本轮发给模型的工具 schema 列表（默认 = `Runtime::tool_schemas()`）。
    /// 钩子可以整段替换它 —— 比如按策略把某几个工具从这一轮里摘掉。
    pub tools: Vec<Value>,
    /// 钩子要求**追加**的消息。循环会经 `Agent::push_message` 入列（见模块文档）。
    ///
    /// 只给「追加」不给「改写」是刻意的：追加是唯一一种不需要 `rewrite` 就能落盘的改动。
    pub inject: Vec<Message>,
}

/// 请求出错时，钩子能给的处置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorAction {
    /// 原样抛出（默认）—— 与没有钩子时完全一致。
    Propagate,
    /// 吞掉这次错误：把 `note` 作为宿主提示注入对话，然后重试。
    ///
    /// 防死循环是**发出方自己的责任**（比如：同一条错误只降级一次）。宿主那侧另有一条
    /// 配置兜底（`[hook] max_llm_retries`）—— 不写钩子、不装组件也能重试。
    ///
    /// 真实构造方（阶段 6 · 6b 起）：组件的 `hooks/on_error` 返回
    /// `{"action":"retry","note":"…"}`，由 [`ComponentHook`] 映射到这里。
    Retry(String),
}

/// 五个钩子点的名字 —— 就是 [`Hook`] 的五个方法名，**一个不多一个不少**。
///
/// 组件在 `plugin.toml [contributes] hooks` 里写的就是这几个字符串。刻意不做「别名表」：
/// 多一层映射就多一处能写错的地方，而写错的表现是「钩子不生效」—— 那恰恰是最难查的一类。
pub const POINTS: [&str; 5] = [
    "before_request",
    "after_reply",
    "before_tool",
    "after_tool",
    "on_error",
];

/// 循环钩子。5 个方法全部是「默认什么都不做」。
///
/// 之所以方法都是 `async`：钩子常常要查系统的当前状态（工具清单、插件表），
/// 那些查询本身就是异步的。与 [`crate::tool::Tool`] / [`crate::ui::Ui`] 同一套约定。
#[async_trait]
pub trait Hook: Send + Sync {
    /// 名字（`/hook` 报告与诊断用）
    fn name(&self) -> &str;

    /// `/hook` 报告里的一句话说明（默认退回名字）
    fn describe(&self) -> String {
        self.name().to_string()
    }

    /// ① 请求前：可替换本轮工具列表、可要求追加消息，也可**中止本轮**。
    async fn before_request(&self, _rt: &Arc<Runtime>, _req: &mut Request) -> Verdict {
        Verdict::Pass
    }

    /// ② 回复后：可改这条回复（正文 / 工具调用），也可**否决**它。
    ///
    /// 否决 = 视为一次无效回复，走与「模型返回空回复」同一条重试路径。
    /// 注意这个点排在**记账之后**：被否决的那次调用的 token 是真花掉了，
    /// 不能因为它没被采用就不记账。
    async fn after_reply(&self, _rt: &Arc<Runtime>, _reply: &mut ChatReply) -> Verdict {
        Verdict::Pass
    }

    /// ③ 工具调用前：可改 `args`，也可拦下这次调用（拦下则**不执行**工具）。
    ///
    /// ⚠️ 此刻带 `tool_calls` 的 assistant 消息**已经落盘**，文件里留的是模型原始参数。
    /// 这里改的只影响本次执行，**不回写历史** —— 历史要如实记录「模型要了什么」，
    /// 而钩子是宿主侧的执行策略。
    async fn before_tool(&self, _rt: &Arc<Runtime>, _name: &str, _args: &mut Value) -> Verdict {
        Verdict::Pass
    }

    /// ④ 工具结果后：可改结果，也可拦下（拦下 = 整块换成带 `reason` 的错误结果）。
    async fn after_tool(&self, _rt: &Arc<Runtime>, _name: &str, _out: &mut ToolOutput) -> Verdict {
        Verdict::Pass
    }

    /// ⑤ 请求出错：可降级处理。
    ///
    /// **只有模型请求本身的错误会走到这里**。工具执行失败、以及「模型连续返回空回复」
    /// 那个 `bail!` 都不算 —— 那些是模型行为，不是基础设施故障，各自的诊断文案已经写好。
    async fn on_error(&self, _rt: &Arc<Runtime>, _err: &anyhow::Error) -> ErrorAction {
        ErrorAction::Propagate
    }
}

/// 一组钩子 + 「按注册顺序执行、遇到拒绝就短路」的语义。
///
/// `run_loop` 只与它打交道，不知道有几个钩子、分别是谁 —— 与
/// [`crate::store::Store`] / [`crate::auth::Policy`] 是同一套形状（原则 P2）。
///
/// # 为什么链是可变的（阶段 6 · 6b）
///
/// 配置驱动的钩子在 `Runtime::new` 里就定型了，但**组件贡献的钩子来不及** ——
/// 插件是在 `Runtime::new` **之后**才被 `load_all` 拉起来的。要让「组件声明即生效」，
/// 链就必须支持运行期追加，于是内部从 `Vec` 换成了 `RwLock<Vec<_>>`。
///
/// 「不装任何组件 = 与今天逐字节相同」这条硬指标不受影响：链里 0 个钩子时，
/// 五个钩子点各自在**一个空 vec** 上循环 0 次 —— 与改动前逐条等价
/// （`empty_chain_never_intervenes` 钉的就是这条）。
#[derive(Default)]
pub struct HookChain {
    hooks: std::sync::RwLock<Vec<Arc<dyn Hook>>>,
}

impl HookChain {
    pub fn new(hooks: Vec<Arc<dyn Hook>>) -> Self {
        Self { hooks: std::sync::RwLock::new(hooks) }
    }

    /// 取一份钩子快照（克隆 `Arc`，N 次引用计数自增）。
    ///
    /// 为什么不在锁里迭代：钩子点的每次调用都要 `await`（钩子常要查异步状态），
    /// 而 `std::sync` 的读锁**不是 `Send`** —— 跨 `await` 持着它，整个 future 就
    /// 不再是 `Send`，`tokio::spawn` 那边直接编译不过。所以老实克隆一份再放锁。
    ///
    /// 空链克隆不分配（`Vec::clone` 对空 vec 不建堆），于是「一个钩子都没有」的
    /// 默认配置下，这条快照路径是零开销的。
    fn snapshot(&self) -> Vec<Arc<dyn Hook>> {
        self.hooks.read().unwrap().clone()
    }

    /// 运行期追加一个钩子（组件贡献的钩子走这里进来）。
    pub fn push(&self, hook: Arc<dyn Hook>) {
        self.hooks.write().unwrap().push(hook);
    }

    /// 一个钩子都没有 —— 此时 `run_loop` 走的路径与「Loop 缝」之前逐条等价。
    pub fn is_empty(&self) -> bool {
        self.hooks.read().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.hooks.read().unwrap().len()
    }

    pub async fn before_request(&self, rt: &Arc<Runtime>, req: &mut Request) -> Verdict {
        for h in &self.snapshot() {
            if let Verdict::Reject(why) = h.before_request(rt, req).await {
                return Verdict::Reject(why);
            }
        }
        Verdict::Pass
    }

    pub async fn after_reply(&self, rt: &Arc<Runtime>, reply: &mut ChatReply) -> Verdict {
        for h in &self.snapshot() {
            if let Verdict::Reject(why) = h.after_reply(rt, reply).await {
                return Verdict::Reject(why);
            }
        }
        Verdict::Pass
    }

    pub async fn before_tool(&self, rt: &Arc<Runtime>, name: &str, args: &mut Value) -> Verdict {
        for h in &self.snapshot() {
            if let Verdict::Reject(why) = h.before_tool(rt, name, args).await {
                return Verdict::Reject(why);
            }
        }
        Verdict::Pass
    }

    pub async fn after_tool(&self, rt: &Arc<Runtime>, name: &str, out: &mut ToolOutput) -> Verdict {
        for h in &self.snapshot() {
            if let Verdict::Reject(why) = h.after_tool(rt, name, out).await {
                return Verdict::Reject(why);
            }
        }
        Verdict::Pass
    }

    /// 依次问每个钩子，**第一个给出降级方案的说了算**（都返回 `Propagate` 就原样抛）。
    pub async fn on_error(&self, rt: &Arc<Runtime>, err: &anyhow::Error) -> ErrorAction {
        for h in &self.snapshot() {
            match h.on_error(rt, err).await {
                ErrorAction::Propagate => {}
                other => return other,
            }
        }
        ErrorAction::Propagate
    }

    /// `/hook` 报告：当前注册了哪些钩子、各自做什么。
    ///
    /// 与 `/prompt` 一样刻意**不做列对齐**：名字是 ASCII 而说明是中文，
    /// 用 `{:<12}` 补齐只会被汉字的双倍宽度顶歪。
    pub fn report(&self) -> String {
        if self.is_empty() {
            return "循环钩子 · 0 个 —— 行为与内置循环完全一致。\n  \
                    两个来源：config.toml 的 [hook]（配置驱动，启动时定型）；\
                    插件 plugin.toml 的 [contributes] hooks（组件贡献，激活时挂上）。"
                .to_string();
        }
        let mut out = format!("循环钩子 · {} 个 · 按注册顺序执行\n", self.len());
        for (i, h) in self.snapshot().iter().enumerate() {
            out.push_str(&format!("\n  {}. {}\n     {}\n", i + 1, h.name(), h.describe()));
        }
        out.push_str(
            "\n  来源：config.toml 的 [hook] / 插件 [contributes] hooks。\
             配置改完需重启；组件随插件激活/卸载。",
        );
        out
    }
}

// ─────────────────────────── 配置驱动的实现 ───────────────────────────

/// 由 `config.toml [hook]` 驱动的钩子。
///
/// 本期 `[hook]` 只有一件事：给指定工具**补默认参数**。
///
/// 「禁止某类工具」**不在这里** —— 那是授权问题，归 `[auth]`
/// （阶段 4 起它的 override 键支持 `fs_*` 这样的通配）。两套机制管两件不同的事：
/// `[auth]` 决定「放不放行」，`[hook]` 决定「放行之后，参数要不要动」。
pub struct ConfigHook {
    tool_args_default: HashMap<String, Value>,
}

impl ConfigHook {
    /// `Ok(None)` = 配置里什么都没写，不必注册这个钩子（保住「空配置 = 空链」）。
    pub fn from_config(cfg: &HookConfig) -> Result<Option<Self>> {
        if cfg.tool_args_default.is_empty() {
            return Ok(None);
        }
        let mut map = HashMap::with_capacity(cfg.tool_args_default.len());
        for (tool, defaults) in &cfg.tool_args_default {
            let tool = tool.trim();
            if tool.is_empty() {
                continue;
            }
            // toml::Value → serde_json::Value：模型那侧的参数本来就是 JSON 值，
            // 两边要在同一个类型上做「只补缺」的合并，不能各留一份表示。
            let v = serde_json::to_value(defaults)
                .with_context(|| format!("[hook.tool_args_default.{tool}] 无法转成 JSON"))?;
            map.insert(tool.to_string(), v);
        }
        if map.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self { tool_args_default: map }))
    }
}

#[async_trait]
impl Hook for ConfigHook {
    fn name(&self) -> &str {
        "config"
    }

    fn describe(&self) -> String {
        let mut names: Vec<&String> = self.tool_args_default.keys().collect();
        names.sort();
        let items: Vec<String> = names
            .iter()
            .map(|n| {
                let keys = match self.tool_args_default.get(*n) {
                    Some(Value::Object(m)) => {
                        let mut k: Vec<&String> = m.keys().collect();
                        k.sort();
                        k.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("、")
                    }
                    _ => "（不是对象，将被忽略）".to_string(),
                };
                format!("{n}({keys})")
            })
            .collect();
        format!("补默认参数（只补模型没给的键）：{}", items.join("，"))
    }

    async fn before_tool(&self, _rt: &Arc<Runtime>, name: &str, args: &mut Value) -> Verdict {
        let Some(defaults) = self.tool_args_default.get(name) else {
            return Verdict::Pass;
        };
        let (Some(dst), Some(src)) = (args.as_object_mut(), defaults.as_object()) else {
            // 参数不是对象（或默认值写成了标量）—— 没法合并，安静放行，不猜。
            return Verdict::Pass;
        };
        // **只补缺**：模型显式给的键一律不动。
        // 覆盖模型的显式意图会造出「模型说 A、工具收到 B」这种最不该出现的失效模式 ——
        // 那比「少一个默认值」难查得多。
        for (k, v) in src {
            dst.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Verdict::Pass
    }
}

// ─────────────────────── 组件贡献的钩子（阶段 6 · 6b）───────────────────────

/// 一次钩子 RPC 的等待上限。
///
/// 为什么钩子单独设一个远小于 `agent.tool_timeout_secs`（默认 120 秒）的上限：
/// 钩子卡住挡住的是**整个主循环**（用户对着转圈），而工具卡住只挡住那一次调用。
/// 本地脚本做一次判断用不到 10 秒；超过就是「这个组件坏了」——按可降级处理，
/// 比让用户等两分钟有意义得多。
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// 由插件 `[contributes] hooks` 声明的钩子。
///
/// 它自己**不含任何策略**，只做三件事：把钩子点转成一次 RPC、把结果映射成
/// [`Verdict`] / [`ErrorAction`]、失败时放行并把失败记在账上。
///
/// # 为什么按「插件名」现查句柄，而不是直接持一个 `Arc<PluginHandle>`
///
/// ① 免得绕成一个环 —— `Runtime.plugins` 里已经存着句柄，钩子再持一份就成了双向引用；
/// ② 白拿「组件卸载即失效」：查不到就放行，不需要任何反注册逻辑；
/// ③ 与 [`crate::plugin::PluginTool`] 做法一致 —— 它也是持名字、用的时候现查。
///
/// 代价是每次钩子点多一次 `plugins` 锁 —— 五个点一轮，可以忽略。
pub struct ComponentHook {
    plugin: String,
    /// 组件声明要接的点（已校验过，只可能来自 [`POINTS`]）
    points: Vec<String>,
    /// 组件申请的能力（`/hook` 报告里显示；授权判定本身走 [`crate::auth::Policy`]）
    caps: Vec<String>,
    /// 与 `PluginHandle` **共享同一对计数器**，所以 `/hook` 的报告能同步读到失败账。
    failures: Arc<AtomicU64>,
    last_error: Arc<std::sync::Mutex<Option<String>>>,
}

impl ComponentHook {
    /// 按「零件」造一个。
    ///
    /// 刻意**不吃 `PluginHandle`**：`hook` 这一层不需要知道插件句柄长什么样，
    /// 只认「插件名 + 声明」这两件事。少一处耦合，降级路径也就更容易单独测
    /// （给一个不存在的插件名即可，见本模块的测试）。
    pub fn new(
        plugin: String,
        points: Vec<String>,
        caps: Vec<String>,
        failures: Arc<AtomicU64>,
        last_error: Arc<std::sync::Mutex<Option<String>>>,
    ) -> Self {
        Self { plugin, points, caps, failures, last_error }
    }

    fn has(&self, point: &str) -> bool {
        self.points.iter().any(|p| p == point)
    }

    fn note_failure(&self, why: String) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = Some(why);
    }

    /// 发一次钩子 RPC。**所有**非正常路径（组件不在、超时、报错）都收敛成 `None`，
    /// 调用方据此放行 —— 钩子这条路上不允许把错误抛回主循环。
    async fn ask(&self, rt: &Arc<Runtime>, point: &str, params: Value) -> Option<Value> {
        let handle = rt.plugins.lock().await.get(&self.plugin).cloned();
        let Some(handle) = handle else {
            self.note_failure(format!("组件 {} 不在运行（已卸载或加载失败）", self.plugin));
            return None;
        };
        let method = format!("hooks/{point}");
        match tokio::time::timeout(HOOK_TIMEOUT, handle.rpc(&method, params)).await {
            Ok(Ok(v)) => Some(v),
            Ok(Err(e)) => {
                self.note_failure(format!("{method}: {e:#}"));
                None
            }
            Err(_) => {
                self.note_failure(format!("{method}: 超时（{} 秒）", HOOK_TIMEOUT.as_secs()));
                None
            }
        }
    }
}

/// 把组件返回的 `{"verdict": …}` 映射成 [`Verdict`]。
///
/// 认不出的值（拼错、类型不对、干脆没给）一律当 `Pass` —— 这里是**执行路径**，
/// 抛错等于让一个坏组件把整轮任务打死，与「组件必须可降级」直接冲突。
/// 该让人看见的问题走 `note_failure` 那本账，不走这里。
fn verdict_of(v: &Value) -> Verdict {
    match v.get("verdict").and_then(|x| x.as_str()) {
        Some("reject") => Verdict::Reject(
            v.get("reason")
                .and_then(|x| x.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("组件要求拦下这一步，但没给理由")
                .to_string(),
        ),
        _ => Verdict::Pass,
    }
}

#[async_trait]
impl Hook for ComponentHook {
    fn name(&self) -> &str {
        &self.plugin
    }

    fn describe(&self) -> String {
        let mut s = format!("组件贡献 · 接 {}", self.points.join("、"));
        if !self.caps.is_empty() {
            s.push_str(&format!(" · 申请能力 {}", self.caps.join("、")));
        }
        let n = self.failures.load(Ordering::Relaxed);
        if n > 0 {
            let last = self
                .last_error
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "（无详情）".to_string());
            s.push_str(&format!("\n     ⚠ 已失败 {n} 次（按可降级处理；最近一次：{last}）"));
        }
        s
    }

    async fn before_request(&self, rt: &Arc<Runtime>, req: &mut Request) -> Verdict {
        if !self.has("before_request") {
            return Verdict::Pass;
        }
        // 工具列表只**发出去**给组件看清楚（供它判断），本期不接受组件替换整张工具表 ——
        // 那等于把「这一轮模型能看见什么」整个交给组件，风险与收益不成比例。
        let params = json!({ "tools": req.tools });
        let Some(v) = self.ask(rt, "before_request", params).await else {
            return Verdict::Pass;
        };
        // 注入只接受「用户文本」：钩子这一层只开放「追加」（见模块文档第一条）。
        if let Some(arr) = v.get("inject").and_then(|x| x.as_array()) {
            for t in arr {
                if let Some(s) = t.as_str().filter(|s| !s.trim().is_empty()) {
                    req.inject.push(Message::user(s));
                }
            }
        }
        verdict_of(&v)
    }

    async fn after_reply(&self, rt: &Arc<Runtime>, reply: &mut ChatReply) -> Verdict {
        if !self.has("after_reply") {
            return Verdict::Pass;
        }
        // 只把**条数**发过去，不发工具调用的明文 —— 本期组件也没有改它的途径，
        // 发了只会诱使人以为能用。
        let params = json!({
            "text": reply.message.text(),
            "tool_calls": reply.message.tool_calls.as_ref().map_or(0, |c| c.len()),
        });
        let Some(v) = self.ask(rt, "after_reply", params).await else {
            return Verdict::Pass;
        };
        // 只允许改**正文**。工具调用不给改：那是「模型要做什么」的记录，
        // 让组件改它等于伪造模型意图，请求体与落盘历史都会跟着歪。
        if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
            reply.message.content = Some(t.to_string());
        }
        verdict_of(&v)
    }

    async fn before_tool(&self, rt: &Arc<Runtime>, name: &str, args: &mut Value) -> Verdict {
        if !self.has("before_tool") {
            return Verdict::Pass;
        }
        let params = json!({ "tool": name, "args": args });
        let Some(v) = self.ask(rt, "before_tool", params).await else {
            return Verdict::Pass;
        };
        if let Some(a) = v.get("args") {
            if !a.is_null() {
                *args = a.clone();
            }
        }
        verdict_of(&v)
    }

    async fn after_tool(&self, rt: &Arc<Runtime>, name: &str, out: &mut ToolOutput) -> Verdict {
        if !self.has("after_tool") {
            return Verdict::Pass;
        }
        let params = json!({ "tool": name, "content": out.content, "is_error": out.is_error });
        let Some(v) = self.ask(rt, "after_tool", params).await else {
            return Verdict::Pass;
        };
        if let Some(c) = v.get("content").and_then(|x| x.as_str()) {
            out.content = c.to_string();
        }
        if let Some(b) = v.get("is_error").and_then(|x| x.as_bool()) {
            out.is_error = b;
        }
        verdict_of(&v)
    }

    async fn on_error(&self, rt: &Arc<Runtime>, err: &anyhow::Error) -> ErrorAction {
        if !self.has("on_error") {
            return ErrorAction::Propagate;
        }
        let params = json!({ "error": format!("{err:#}") });
        let Some(v) = self.ask(rt, "on_error", params).await else {
            return ErrorAction::Propagate;
        };
        match v.get("action").and_then(|x| x.as_str()) {
            Some("retry") => ErrorAction::Retry(
                v.get("note")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or("（组件建议重试，未给出提示）")
                    .to_string(),
            ),
            _ => ErrorAction::Propagate,
        }
    }
}

/// 供 `Runtime::new` 构造**配置驱动**的钩子链：目前只有 `[hook]` 这一种实现。
///
/// 组件贡献的钩子不从这里来 —— 它们在插件激活时经
/// [`HookChain::push`] 挂上去（插件是在 `Runtime::new` 之后才加载的）。
///
/// 返回 `Vec` 而不是链本身，是为了让 `Runtime` 那边一眼看出「空配置 = 空链」。
/// 配置里有写不通的地方（比如默认值不是对象）会**在启动时报错**，不静默跳过 ——
/// 与 `[store] backend` 同一条原则：写了却不生效是最难查的一类问题。
pub fn registry_from_config(cfg: &HookConfig) -> Result<Vec<Arc<dyn Hook>>> {
    let mut hooks: Vec<Arc<dyn Hook>> = Vec::new();
    if let Some(h) = ConfigHook::from_config(cfg)? {
        hooks.push(Arc::new(h));
    }
    Ok(hooks)
}

/// `/hook` 命令的正文。
pub fn report(rt: &Runtime) -> String {
    rt.hooks.report()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RuntimesConfig};
    use serde_json::json;
    use std::sync::Mutex;

    /// 造一个不碰真实目录的运行时（`Config::dummy()` + 上下文里不存在的解释器）。
    fn rt() -> Arc<Runtime> {
        let mut cfg = Config::dummy();
        cfg.runtimes = RuntimesConfig {
            python: "__none__.exe".into(),
            node: "__none__.exe".into(),
            rustc: "__none__.exe".into(),
            python_url: None,
            python_sha256: None,
        };
        let root = std::env::temp_dir().join("aox-hook-test");
        Runtime::new(cfg, root.clone(), root.join("plugins")).expect("测试用运行时不应失败")
    }

    /// 记录「自己被调用过」的假钩子。
    struct Rec {
        tag: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
        verdict: Verdict,
    }

    #[async_trait]
    impl Hook for Rec {
        fn name(&self) -> &str {
            self.tag
        }
        async fn before_tool(&self, _rt: &Arc<Runtime>, _name: &str, _args: &mut Value) -> Verdict {
            self.log.lock().unwrap().push(self.tag);
            self.verdict.clone()
        }
    }

    fn rec(tag: &'static str, log: &Arc<Mutex<Vec<&'static str>>>, verdict: Verdict) -> Arc<dyn Hook> {
        Arc::new(Rec { tag, log: log.clone(), verdict })
    }

    fn cfg_with(tool: &str, keys: &[(&str, &str)]) -> HookConfig {
        let mut inner = HashMap::new();
        for (k, v) in keys {
            inner.insert((*k).to_string(), toml::Value::String((*v).to_string()));
        }
        HookConfig {
            tool_args_default: [(tool.to_string(), inner)].into_iter().collect(),
            ..Default::default()
        }
    }

    /// 空链的五个点全部是「什么都不做」—— 这就是「无钩子 = 行为不变」的机制保证。
    #[tokio::test]
    async fn empty_chain_never_intervenes() {
        let rt = rt();
        let chain = HookChain::default();
        assert!(chain.is_empty() && chain.len() == 0);

        let mut req = Request { tools: vec![json!({"a": 1})], inject: Vec::new() };
        assert_eq!(chain.before_request(&rt, &mut req).await, Verdict::Pass);
        assert_eq!(req.tools.len(), 1, "空链不许动 tools");
        assert!(req.inject.is_empty(), "空链不许塞消息");

        let mut reply = ChatReply {
            message: Message::assistant(Some("hi".into()), None, Vec::new()),
            finish_reason: Some("stop".into()),
            usage: Default::default(),
        };
        assert_eq!(chain.after_reply(&rt, &mut reply).await, Verdict::Pass);
        assert_eq!(reply.message.text(), Some("hi"), "空链不许改回复");

        let mut args = json!({"path": "a"});
        assert_eq!(chain.before_tool(&rt, "t", &mut args).await, Verdict::Pass);
        assert_eq!(args, json!({"path": "a"}), "空链不许改参数");

        let mut out = ToolOutput::ok("result");
        assert_eq!(chain.after_tool(&rt, "t", &mut out).await, Verdict::Pass);
        assert_eq!(out.content, "result");

        let e = anyhow::anyhow!("boom");
        assert_eq!(chain.on_error(&rt, &e).await, ErrorAction::Propagate);
    }

    /// 按注册顺序执行；遇到第一个 `Reject` 就短路，后面的钩子不许再被调用。
    #[tokio::test]
    async fn chain_runs_in_order_and_stops_at_first_reject() {
        let rt = rt();
        let log = Arc::new(Mutex::new(Vec::new()));
        let chain = HookChain::new(vec![
            rec("a", &log, Verdict::Pass),
            rec("b", &log, Verdict::Reject("b 说不".into())),
            rec("c", &log, Verdict::Pass),
        ]);
        assert_eq!(chain.len(), 3);

        let v = chain.before_tool(&rt, "t", &mut json!({})).await;
        assert_eq!(v, Verdict::Reject("b 说不".into()));
        assert_eq!(*log.lock().unwrap(), vec!["a", "b"], "被拦下之后不该再问 c");
    }

    /// `before_tool` 拿到的 `args` 是**可写的**：钩子改完，后续钩子与循环看到的是改后的值。
    #[tokio::test]
    async fn before_tool_edits_are_visible_to_later_hooks() {
        let rt = rt();
        let log = Arc::new(Mutex::new(Vec::new()));

        struct Fill;
        #[async_trait]
        impl Hook for Fill {
            fn name(&self) -> &str {
                "fill"
            }
            async fn before_tool(&self, _rt: &Arc<Runtime>, _n: &str, args: &mut Value) -> Verdict {
                args["filled"] = json!(true);
                Verdict::Pass
            }
        }

        struct Check(Arc<Mutex<Vec<&'static str>>>);
        #[async_trait]
        impl Hook for Check {
            fn name(&self) -> &str {
                "check"
            }
            async fn before_tool(&self, _rt: &Arc<Runtime>, _n: &str, args: &mut Value) -> Verdict {
                assert_eq!(args["filled"], json!(true), "后一个钩子必须看到前一个的改动");
                self.0.lock().unwrap().push("check");
                Verdict::Pass
            }
        }

        let chain = HookChain::new(vec![Arc::new(Fill), Arc::new(Check(log.clone()))]);
        let mut args = json!({});
        assert_eq!(chain.before_tool(&rt, "t", &mut args).await, Verdict::Pass);
        assert_eq!(args, json!({"filled": true}));
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    /// `before_request` 注入的消息原样交回给循环（落盘由循环经 `push_message` 负责）。
    #[tokio::test]
    async fn before_request_hands_injected_messages_back() {
        let rt = rt();
        struct Inject;
        #[async_trait]
        impl Hook for Inject {
            fn name(&self) -> &str {
                "inject"
            }
            async fn before_request(&self, _rt: &Arc<Runtime>, req: &mut Request) -> Verdict {
                req.inject.push(Message::user("宿主补充说明"));
                Verdict::Pass
            }
        }
        let chain = HookChain::new(vec![Arc::new(Inject)]);
        let mut req = Request { tools: Vec::new(), inject: Vec::new() };
        assert_eq!(chain.before_request(&rt, &mut req).await, Verdict::Pass);
        assert_eq!(req.inject.len(), 1);
        assert_eq!(req.inject[0].text(), Some("宿主补充说明"));
    }

    /// **只补缺**：模型显式给的值绝不被默认值覆盖。
    #[tokio::test]
    async fn args_default_only_fills_missing_keys() {
        let rt = rt();
        let hook = ConfigHook::from_config(&cfg_with(
            "fs_read_file",
            &[("path", "config.toml"), ("encoding", "utf-8")],
        ))
        .unwrap()
        .expect("有配置就要造出钩子");

        // 模型给了 path，没给 encoding
        let mut args = json!({ "path": "我自己的路径" });
        assert_eq!(hook.before_tool(&rt, "fs_read_file", &mut args).await, Verdict::Pass);
        assert_eq!(args["path"], json!("我自己的路径"), "显式值不许被默认值顶掉");
        assert_eq!(args["encoding"], json!("utf-8"), "缺的那个补上");

        // 别的工具一概不碰
        let mut other = json!({});
        assert_eq!(hook.before_tool(&rt, "fs_write_file", &mut other).await, Verdict::Pass);
        assert_eq!(other, json!({}), "没配的工具不该被动");
    }

    /// 参数不是对象时安静放行 —— 猜不出意图就不要动它。
    #[tokio::test]
    async fn args_default_leaves_non_object_args_alone() {
        let rt = rt();
        let hook = ConfigHook::from_config(&cfg_with("t", &[("k", "v")])).unwrap().unwrap();
        for mut args in [json!("字符串"), json!([1, 2]), json!(null)] {
            let before = args.clone();
            assert_eq!(hook.before_tool(&rt, "t", &mut args).await, Verdict::Pass);
            assert_eq!(args, before);
        }
    }

    /// `[hook]` 全空 → 不注册任何钩子（「空配置 = 空链」的入口保证）。
    #[test]
    fn empty_config_registers_no_hook() {
        let hooks = registry_from_config(&HookConfig::default()).unwrap();
        assert!(hooks.is_empty(), "默认配置不该造出钩子");
        // 空表（写了 [hook] 但没写内容）同样不造
        let empty = HookConfig { tool_args_default: HashMap::new(), ..Default::default() };
        assert!(registry_from_config(&empty).unwrap().is_empty());
    }

    /// 配置 → 运行时这条链要真的通：默认 0 个，配了就 1 个。
    #[test]
    fn runtime_builds_chain_from_config() {
        let mut cfg = Config::dummy();
        cfg.runtimes = RuntimesConfig {
            python: "__none__.exe".into(),
            node: "__none__.exe".into(),
            rustc: "__none__.exe".into(),
            python_url: None,
            python_sha256: None,
        };
        let root = std::env::temp_dir().join("aox-hook-test-rt");

        let off = Runtime::new(cfg.clone(), root.clone(), root.join("plugins")).unwrap();
        assert!(off.hooks.is_empty(), "默认配置：空链");
        assert!(off.hooks.report().contains("0 个"));

        cfg.hook = cfg_with("fs_read_file", &[("encoding", "utf-8")]);
        let on = Runtime::new(cfg, root.clone(), root.join("plugins")).unwrap();
        assert_eq!(on.hooks.len(), 1, "配了就该挂上");
        let rep = on.hooks.report();
        assert!(rep.contains("config"), "报告要带上钩子名: {rep}");
        assert!(rep.contains("encoding"), "报告要说清补了哪个键: {rep}");
    }

    // ─────────────── 阶段 6 · 6b：运行期追加 + 组件贡献 ───────────────

    /// 空链被 `push` 之后，钩子点**立刻**开始问它 —— 这就是「组件声明即生效」的机制。
    #[tokio::test]
    async fn push_adds_a_hook_to_a_live_chain() {
        let rt = rt();
        let chain = HookChain::default();
        assert!(chain.is_empty(), "一开始必须是空链（零漂移的前提）");

        let log = Arc::new(Mutex::new(Vec::new()));
        chain.push(rec("late", &log, Verdict::Reject("不".into())));
        assert_eq!(chain.len(), 1);
        assert!(!chain.is_empty());

        let v = chain.before_tool(&rt, "t", &mut json!({})).await;
        assert_eq!(v, Verdict::Reject("不".into()));
        assert_eq!(*log.lock().unwrap(), vec!["late"], "push 进来的钩子必须真的被调用");
    }

    /// 返回体映射：只有**显式**的 `"reject"` 才算拒绝；其余（拼错、缺字段、类型不对）
    /// 一律当放行 —— 这是执行路径，宁可少拦一次，也别让一个坏组件把整轮任务打死。
    #[test]
    fn verdict_only_rejects_on_an_explicit_word() {
        assert_eq!(verdict_of(&json!({ "verdict": "pass" })), Verdict::Pass);
        assert_eq!(verdict_of(&json!({})), Verdict::Pass);
        assert_eq!(verdict_of(&json!(null)), Verdict::Pass);
        assert_eq!(verdict_of(&json!({ "verdict": "REJECT" })), Verdict::Pass, "大小写不认");
        assert_eq!(verdict_of(&json!({ "verdict": 1 })), Verdict::Pass, "类型不对不认");
        assert_eq!(
            verdict_of(&json!({ "verdict": "reject", "reason": "   " })),
            Verdict::Reject("组件要求拦下这一步，但没给理由".into()),
            "理由为空时也要有一句能见人的话"
        );
        assert_eq!(
            verdict_of(&json!({ "verdict": "reject", "reason": "越界了" })),
            Verdict::Reject("越界了".into())
        );
    }

    /// 组件不在（没装 / 已卸载 / 加载失败）→ 放行 + 记一笔账，参数一个字都不许动。
    /// 这条是「组件必须可降级」的最小证据。
    #[tokio::test]
    async fn component_hook_degrades_when_the_plugin_is_gone() {
        let rt = rt(); // 这个运行时里一个插件都没有
        let failures = Arc::new(AtomicU64::new(0));
        let last = Arc::new(std::sync::Mutex::new(None));
        let hook = ComponentHook::new(
            "ghost".into(),
            vec!["before_tool".into()],
            Vec::new(),
            failures.clone(),
            last.clone(),
        );

        let mut args = json!({ "a": 1 });
        assert_eq!(hook.before_tool(&rt, "t", &mut args).await, Verdict::Pass);
        assert_eq!(args, json!({ "a": 1 }), "组件不在时参数不许被改动");
        assert_eq!(failures.load(Ordering::Relaxed), 1, "失败要记账");
        assert!(
            last.lock().unwrap().as_deref().unwrap_or("").contains("ghost"),
            "账上要写清是哪个组件: {:?}",
            last.lock().unwrap()
        );
        // 失败账要能从报告里看到（否则就是「静默消失」而不是「静默降级」）
        assert!(hook.describe().contains("已失败 1 次"), "报告里要看得到失败: {}", hook.describe());
    }

    /// 没声明的点：宿主连一次 RPC 都不该发，更不该产生失败账。
    #[tokio::test]
    async fn undeclared_points_are_never_asked() {
        let rt = rt();
        let failures = Arc::new(AtomicU64::new(0));
        let hook = ComponentHook::new(
            "ghost".into(),
            vec!["before_tool".into()],
            Vec::new(),
            failures.clone(),
            Arc::new(std::sync::Mutex::new(None)),
        );

        let e = anyhow::anyhow!("boom");
        assert_eq!(hook.on_error(&rt, &e).await, ErrorAction::Propagate);

        let mut reply = ChatReply {
            message: Message::assistant(Some("hi".into()), None, Vec::new()),
            finish_reason: Some("stop".into()),
            usage: Default::default(),
        };
        assert_eq!(hook.after_reply(&rt, &mut reply).await, Verdict::Pass);
        assert_eq!(reply.message.text(), Some("hi"), "没声明的点不许改回复");

        assert_eq!(failures.load(Ordering::Relaxed), 0, "没声明的点不该产生失败账");
    }
}
