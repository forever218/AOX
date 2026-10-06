use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{Mutex, RwLock};

use crate::auth::{ConfigPolicy, Policy};
use crate::config::{Config, RuntimesConfig};
use crate::plugin::PluginHandle;
use crate::prompt::PromptSegment;
use crate::tool::{Tool, ToolKind, ToolOutput};

/// 运行时共享状态：工具注册表 + 插件管理 + 会话级授权白名单。
/// 元工具通过它对系统自身进行「生长」。
pub struct Runtime {
    pub config: Config,
    /// 授权策略（策略缝，见 `src/auth.rs`）。
    ///
    /// 持 trait 对象而不是具体类型：`Agent` 只问「这个调用放不放行」，不关心
    /// 判定是从 `config.toml [auth]` 读来的还是别处算出来的。
    /// `Send + Sync` 由 `Policy` 约束保证 —— 它要被 agent 线程与 GUI 主线程共享。
    pub auth: Arc<dyn Policy>,
    /// 会话存储后端（存储缝，见 `src/store.rs`）。
    ///
    /// 由 `config.toml [store] backend` 在 `Runtime::new` 时定型；agent 线程用它
    /// 建/开对话文件，界面侧按同一个后端另建一个只读实例（`location` 指向同一处）。
    pub store: Arc<dyn crate::store::Store>,
    /// 应用根目录。插件子进程以此为工作目录，保证插件内的相对路径与宿主视图一致。
    pub root: PathBuf,
    pub plugins_dir: PathBuf,
    /// 解释器解析结果（`config.runtimes` 经过三档解析后的快照）。
    ///
    /// 单独放一份而不是直接读 `config.runtimes`，是为了让「装配完运行时立刻生效」成为可能：
    /// 安装完成后原地刷新这里，插件子进程下一次 spawn 就用上新解释器，无需重启进程。
    /// 极小的结构，克隆开销可忽略。
    ///
    /// 用 `std::sync::RwLock` 而不是 tokio 的：这里只在同步上下文里读写，
    /// 且从不跨 `await` 持锁——`spawn_command` 是同步函数，必须能直接拿。
    runtimes: std::sync::RwLock<RuntimesConfig>,
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    pub plugins: Mutex<HashMap<String, Arc<PluginHandle>>>,
    /// 用户在本会话中选择「总是允许」的工具名
    pub session_allowed: RwLock<HashSet<String>>,
    /// 系统提示词的**段注册表**（`src/prompt.rs`）。
    ///
    /// 内置的 `core` / `protocol` 段与 `config.toml [[prompt.segment]]` 里的追加段
    /// 都在 `Runtime::new` 时注册进来；运行环境段与项目规则段由 `prompt` 模块
    /// 按需生成（它们依赖 `root` 与文件系统，不适合固化在注册表里）。
    ///
    /// 用 `std::sync::RwLock` 而不是 tokio 的：读写只发生在同步上下文
    /// （`Agent::new` → `prompt::build`），从不跨 `await` 持锁。
    prompt_segments: std::sync::RwLock<Vec<PromptSegment>>,
    /// 循环钩子链（Loop 缝，见 `src/hook.rs`）。
    ///
    /// 在 `Runtime::new` 里由 `config.toml [hook]` 定型。默认配置下它是**空链**，
    /// 那时 `Agent::run_loop` 的五个钩子点全部退化成「查一下长度然后返回」——
    /// 这就是本阶段「没有钩子 = 行为与今天逐条等价」的实现方式。
    ///
    /// 与 `store` / `auth` 一样持具体类型而不再加一层 trait 对象：链本身就是那层间接
    /// （它内部装的才是 `Vec<Arc<dyn Hook>>`），再套一层 ABC 没有意义。
    pub hooks: crate::hook::HookChain,
    /// 主循环实现（循环缝的第二形态，见 `src/agent_loop.rs`）。
    ///
    /// 在 `Runtime::new` 里由 `config.toml [loop] impl` 定型；**认不出的名字启动即报错**，
    /// 不静默退回 `standard`。`Agent::run_turn` 从这儿取一段循环来跑 ——
    /// 于是「换一套执行模型」是改配置，而不是改代码。
    pub loop_impl: Arc<dyn crate::agent_loop::AgentLoop>,
    /// 插件子进程的**围栏**（Job Object，见 `src/sandbox.rs`）。
    ///
    /// 在 `Runtime::new` 里由 `config.toml [sandbox]` 定型。它本身只是个配置的持有者，
    /// 真正的内核对象是每次 spawn 时现建的（一个插件一个 Job）——
    /// 所以插件**运行期间**改这里不影响已经起来的进程，只影响下一次 spawn / 重启。
    pub sandbox: crate::sandbox::Sandbox,
    /// 模型的**文件边界**（工作区，见 `src/workplace.rs`）。
    ///
    /// 与 `sandbox` 并排但管的是另一件事：`sandbox` 限制插件子进程怎么跑，
    /// `workplace` 限制「这次能碰哪些文件」。`full` 时不受限；`default` / `fixed` 时
    /// 写入与删除只准落在划定的那块地里（读不设限）。
    /// **配置默认是 `default`（受限）** —— 2026-10-05 由 `full` 改过来，兑现用户那句
    /// 「每次启动默认受限」。所以「不写 `[workplace]`」现在**不再**等于「行为不变」。
    ///
    /// 内部是 `std::sync::Mutex`，只在同步上下文读写（判定在工具调用路径上跑）——
    /// 与 `runtimes` / `prompt_segments` 同样的理由：从不跨 `await` 持锁。
    pub workplace: crate::workplace::Workplace,
}

impl Runtime {
    /// 组装运行时。返回 `Result` 是为了让「存储后端名字写错」在**启动时**就报错
    /// （`[store] backend` 认不出的值直接失败，不静默退回 jsonl —— 见 `store::backend`）。
    pub fn new(config: Config, root: PathBuf, plugins_dir: PathBuf) -> Result<Arc<Self>> {
        // 三档解释器解析：config 显式路径 > 应用目录内置运行时 > PATH。
        // 放在这里而不是各自的入口，是为了让 CLI / GUI / selftest 走同一条路径，
        // 不出现「GUI 能跑 CLI 不能跑」这类逻辑分叉。
        //
        // 注意：`config` 本身**保持原样**不动。解析结果只存进 `runtimes` 字段——
        // 展示层（/runtime 报告、系统提示）需要拿原始值才能说清「解释器是从哪来的」，
        // 一旦把绝对路径写回 config，所有来源都会被误报成「config.toml 指定」。
        let mut resolved = config.runtimes.clone();
        crate::runtimes::resolve(&mut resolved, &root);
        let auth: Arc<dyn Policy> = Arc::new(ConfigPolicy::from_config(&config.auth));
        // 提示词注册表在 `config` 被 move 进 Self 之前先算好。
        // 内置段在 prompt 模块里注册；配置段在这里一并进去（同 id 覆盖内置）。
        let prompt_segments = crate::prompt::registry_from_config(&config.prompt);
        // 存储后端在 `config` 被 move 进 Self 之前定型（要用到 config.store / config.history）。
        let store = crate::store::backend(&config.store.backend, &root, config.history.keep)?;
        // 循环钩子链同样在 `config` 被 move 之前定型。
        // 这里用 `?`：`[hook]` 里写了没法理解的东西（不是一个对象、值转不成 JSON）
        // 会在**启动时**报错，而不是安静地不生效 —— 与 `[store] backend` 同一条原则。
        let hooks = crate::hook::HookChain::new(crate::hook::registry_from_config(&config.hook)?);
        // 主循环实现同样在 `config` 被 move 之前定型。
        // 这里用 `?`：`[loop] impl` 写了认不出的名字会在**启动时**报错，不静默退回 standard ——
        // 与 `[store] backend` 同一条原则。
        let loop_impl = crate::agent_loop::build(&config.loop_config)?;
        // 围栏的配置在 `config` 被 move 进 Self 之前取出（`Sandbox` 只是个廉价的手持配置）。
        let sandbox = crate::sandbox::Sandbox::new(config.sandbox.clone());
        // 工作区同样在 `config` 被 move 之前定型。**这里用 `?`**：`[workplace] mode`
        // 写了认不出的值、或 `fixed` 却没给 `root`，都在**启动时**报错 ——
        // 与 `[store] backend` / `[loop] impl` 同一条原则（写了却不生效最难查）。
        let workplace = crate::workplace::Workplace::new(config.workplace.clone(), &root)?;
        Ok(Arc::new(Self {
            config,
            auth,
            store,
            root,
            plugins_dir,
            runtimes: std::sync::RwLock::new(resolved),
            tools: RwLock::new(HashMap::new()),
            plugins: Mutex::new(HashMap::new()),
            session_allowed: RwLock::new(HashSet::new()),
            prompt_segments: std::sync::RwLock::new(prompt_segments),
            hooks,
            loop_impl,
            sandbox,
            workplace,
        }))
    }

    /// 当前注册的提示词段（克隆一份，调用方可随意排序/过滤）。
    pub fn prompt_segments(&self) -> Vec<PromptSegment> {
        self.prompt_segments.read().unwrap().clone()
    }

    /// 追加一个提示词段。id 已存在时**整段替换**它 —— 与 `config.toml` 里
    /// `[[prompt.segment]]` 覆盖内置段是同一套语义（后注册者胜）。
    ///
    /// 调用方（阶段 6 · 6b 起）：插件激活时把组件贡献的提示词段挂上来 ——
    /// 正文由 `prompt/segment` 从子进程取回，见 `plugin::activate`。
    pub fn add_prompt_segment(&self, seg: PromptSegment) {
        let mut g = self.prompt_segments.write().unwrap();
        match g.iter_mut().find(|s| s.id == seg.id) {
            Some(slot) => *slot = seg,
            None => g.push(seg),
        }
    }

    /// 运行期追加一个循环钩子（组件贡献的钩子在这里进来）。
    ///
    /// 为什么需要「运行期」：插件是在 `Runtime::new` **之后**才被加载的，而配置驱动的
    /// 钩子在 `new` 里就定型了。钩子点每次调用先取一份快照再迭代，所以这里加进来的
    /// 钩子对**下一次**钩子点调用立刻生效（见 `hook::HookChain::snapshot`）。
    ///
    /// 反方向（卸载）不需要对应的方法：`ComponentHook` 是按**插件名**现查句柄的，
    /// 插件一从 `plugins` 里消失它就自己失效了（见 `hook::ComponentHook` 的注释）。
    pub fn add_hook(&self, hook: Arc<dyn crate::hook::Hook>) {
        self.hooks.push(hook);
    }

    /// 当前生效的解释器配置（克隆）。
    pub fn runtimes(&self) -> RuntimesConfig {
        self.runtimes.read().unwrap().clone()
    }

    /// 重新做一次三档解析。
    /// 装配运行时、或用户手改 `config.runtimes` 之后调用，让改动立即生效。
    pub fn refresh_runtimes(&self) {
        let mut g = self.runtimes.write().unwrap();
        *g = self.config.runtimes.clone();
        let root = self.root.clone();
        crate::runtimes::resolve(&mut g, &root);
    }

    pub async fn register(&self, tool: Arc<dyn Tool>) -> Result<()> {
        let mut tools = self.tools.write().await;
        let name = tool.name().to_string();
        if tools.contains_key(&name) {
            anyhow::bail!("工具名冲突: {name}（工具名全局唯一，请用「插件名_功能」命名）");
        }
        tools.insert(name, tool);
        Ok(())
    }

    pub async fn unregister(&self, name: &str) {
        self.tools.write().await.remove(name);
    }

    /// 每轮对话重新生成——模型刚注入的插件工具下一轮立即可见。
    pub async fn tool_schemas(&self) -> Vec<serde_json::Value> {
        let tools = self.tools.read().await;
        let mut v: Vec<_> = tools.values().map(|t| t.schema()).collect();
        v.sort_by(|a, b| {
            a["function"]["name"].as_str().unwrap_or("").cmp(b["function"]["name"].as_str().unwrap_or(""))
        });
        v
    }

    pub async fn tool_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tools.read().await.keys().cloned().collect();
        v.sort();
        v
    }

    pub async fn tool_kind(&self, name: &str) -> Option<ToolKind> {
        self.tools.read().await.get(name).map(|t| t.kind())
    }

    /// 这道工具的一次调用算「读」还是「落盘」—— 工作区边界用的就是它
    /// （见 [`crate::tool::Tool::access`] 与 `agent::Agent::workplace_reject`）。
    ///
    /// 工具不在表里时返回**落盘**：调用方都是先查过名字的，真出现这种情况说明
    /// 注册表与调用方对不上，那时保守方向仍然是「宁可多挡一次」。
    pub async fn tool_access(&self, name: &str) -> crate::workplace::Access {
        match self.tools.read().await.get(name) {
            Some(t) => t.access(),
            None => crate::workplace::Access::Write,
        }
    }

    /// 这次调用**申请的能力**（来自插件的 `[contributes] caps`，阶段 6 · 6b 起）。
    ///
    /// 只有插件工具可能有 —— 元工具是宿主自己的，不替谁申请能力。
    /// 插件不在（没装 / 已卸载 / 加载失败）时返回空：**能力申请随组件一起消失**，
    /// 不留「插件已经没了、限制却还在」这种半状态。
    pub async fn caps_for(&self, kind: &ToolKind) -> Vec<String> {
        let ToolKind::Plugin(name) = kind else {
            return Vec::new();
        };
        self.plugins
            .lock()
            .await
            .get(name)
            .map(|h| h.contributes.caps.clone())
            .unwrap_or_default()
    }

    pub async fn execute(self: &Arc<Self>, name: &str, args: serde_json::Value) -> ToolOutput {
        let tool = { self.tools.read().await.get(name).cloned() };
        let Some(tool) = tool else {
            return ToolOutput::err(format!("工具不存在: {name}"));
        };
        // 兜底超时：插件工具内部已有 tool_timeout_secs 超时 + 重启重试，
        // 这里额外加 30 秒余量，防任何路径上的永久阻塞。
        let timeout = Duration::from_secs(self.config.agent.tool_timeout_secs + 30);
        match tokio::time::timeout(timeout, tool.call(args, self)).await {
            Ok(out) => out,
            Err(_) => ToolOutput::err(format!("工具 {name} 执行超时（{} 秒）", timeout.as_secs())),
        }
    }
}
