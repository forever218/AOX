# AOX 子代理（自孵化一个无界面实例）· 立项册

- **状态**：**已立项、延后**（用户决定，2026-10-09 —— 设计已完整推演，随后刻意搁置：改动
  偏大，项目本身刻意保持轻量，而在这个体量下子代理并不能明显挣回它的成本）。
- **由来**：用户问，GUI 能否「启用子代理 —— 再开一个它自己的实例 —— 但以 `/headless` 无界面
  模式运行，不弹窗」。对 CLI 而言这件事很简单（就是再开一个后台进程）。
- **本文件**是立项册：可行性结论、设计方案、待定决策，以及延后的理由。此处**没有任何实现**。

> **重启触发条件**（当这件事重新值得做时）：出现真正需要**并行独立探索**（只读）或**上下文
> 隔离**的任务 —— 也就是复杂到单条线性循环成为瓶颈，或把所有东西塞进一个上下文会使其劣化
> 的规模。在那之前，默认的单循环是更便宜也更诚实的答案。

---

## 1. 可行性结论：这个项目几乎就是为这件事长的

宿主已经拥有一整套**「孵化子进程 + 行 JSON 协议 + 围栏 + 回收」**设施 —— 目前用于跑插件。
子代理就是**同一套设施换一份协议**。这件事与「单 exe、几 MB」的体格毫无冲突。

| 需要什么 | 现有资产 | 位置 |
|---|---|---|
| 后台**无窗口**进程 | 统一的 `CREATE_NO_WINDOW` 封装（插件 / python / node / rustc 全走它）；注释已写明动机就是「子进程不得闪黑窗」 | `src/plugin/mod.rs:85-116` |
| 冻结 → 入 Job → 恢复 | `Sandbox::create_flags()` + `contain()`，且已含 UI 限制项 | `src/sandbox.rs:94,113` |
| 管道 + stderr 落文件 + 父死子灭 | `spawn_command`：`stdin/stdout` piped、stderr → 文件、`kill_on_drop(true)`、`current_dir(root)` | `src/plugin/mod.rs:766-802` |
| 行 JSON 协议 | 插件 RPC 本就是「一行一条 JSON」，连「stdout 只跑协议、日志走 stderr」的纪律都定好了 | `src/plugin/mod.rs` |
| 无界面 UI | `trait Ui` 只有 **4 个必填方法**，其余全有默认实现 | `src/ui/mod.rs:77-129` |
| 非交互入口的先例 | `selftest` 子命令与 `--dump-config`：解析完参数直接干活然后 return，从不建窗口 | `src/main.rs:241,269` |
| 加一个工具 | `trait Tool` | `src/tool/mod.rs:107-148` |
| 结果回传 | `ToolOutput { content, is_error, attach }` | `src/tool/mod.rs:48` |
| 用量归集 | `kind:"usage"` 行 + `UsageRecord` 已是现成格式 | `src/history.rs:146` |
| 启动不慢 | 插件懒加载已是默认 ⇒ 子进程**不必**等冷探测（否则每个子代理要多等最多 20 秒） | `src/config.rs [plugins]` |

## 2. 杀死天真路线的那一个事实

**GUI 安装包只打包了 `aox-gui.exe` —— 没有 `aox.exe`**
（`installer/aox-gui.iss:122`；两个装包 `AppId` 不同、互不共享目录）。

所以「GUI 再开一个 CLI 实例」**走不通**。正确做法是**自孵化 `current_exe()`** —— 而给
`aox-gui.exe` 加一个 `--headless` 分支不需要新二进制，也**不加依赖、不增体积**。两个二进制
都已链接全部核心代码，故 `--headless` 分支是免费的；不需要第三个 exe。

## 3. 缺的六件事（其中两件是真难点）

| 缺什么 | 规模 |
|---|---|
| `--headless` 入口（两个二进制都没有） | 小 |
| `HeadlessUi`（实现 `Ui`） | ~60 行 |
| 协议 + Runner | 中 |
| **历史隔离** —— 子进程用同一 app root 会往 `<root>/history/*.jsonl` 写，**直接污染 GUI 侧栏** | **关键** |
| **授权策略** —— `AuthLevel::Ask` 会打到 `Ui::authorize`，无头下没人答 | **关键** |
| 递归 / 并发上限 | 小 |

### 3.1 历史污染（必解，否则侧栏会塞满陌生会话）

子进程默认读同一份 `config.toml`、用同一个 app root ⇒ `default_store(root)` 会把它的对话写成
`<root>/history/<ts>.jsonl`，而 `Store::list` **就是扫这个目录**（`src/store/jsonl.rs:62`）。
结果：跑一次子代理，GUI 侧栏就多出一个陌生会话。`Store` 接口刻意不暴露路径，所以不能靠
「给个子目录参数」糊过去 —— 干净的解法是**配置快照**（见下）。

### 3.2 授权穿透（必解，否则子代理会被莫名卡住）

`authorize_and_execute`（`src/agent.rs:529`）里 `AuthLevel::Ask` 会走 `ui.authorize`。无头
下没人答 ⇒ 默认实现只能拒绝。所以子代理必须有**明确**的策略来源，不能靠碰运气。

### 3.3 进度不可见（决定第一版体验；可延后）

`Tool::call(&self, args, rt) -> ToolOutput` **没有进度通道** —— 这是全部设计里**唯一**需要动
核心接口的地方。乐观解读：这正好是一条第 8 条缝该出手的位置（形状与 `Ui::stream_sink` 同族）。
悲观解读：v1 只能是「黑盒调用，跑完才见到结果」。**建议接受后者。**

另外两个容易翻车的细节：`Tool::call` 拿不到 `AgentEvent`，所以子代理的事件**必须**走协议行而
不是直接 `ui.emit`；`ProcessFlags` 现在是 `plugin/mod.rs` 里的 `pub(crate)`，子代理 runner 要用
得先上移到 `sandbox.rs` 或新的 `proc.rs`。

## 4. 设计方案

### 4.1 分层 —— 照既有「缝」的风格

新增 `trait AgentRunner`；`Runtime` 持 `Arc<dyn AgentRunner>`；本期唯一实现 `ProcessRunner`
（自孵化）。这样以后换「同进程内起一个 Agent」（省掉进程开销）或「远端 runner」都不用动调用
侧。配置侧对应新增 `[subagent]` 段，**默认不启用** ⇒ 零漂移承诺保住。

### 4.2 协议 —— 双向行 JSON，复用插件的纪律

| 方向 | 消息 |
|---|---|
| 父→子 | `task{prompt, workspace, policy, depth}` · `cancel{}` · `auth_reply{}` |
| 子→父 | `ready{model, tools, workspace}` · `status{text}` · `tool{name,args,result,is_error}` · `usage{...}` · `auth_req{tool,args}` · `final{text, tokens, transcript}` |

纪律照抄插件：**stdout 只跑协议，stderr 全部重定向到 `temp/subagent/<job>/stderr.log`** ——
管道读不干净会死锁；而且 GUI 版是 `windows_subsystem="windows"`（无控制台），用户根本看不到
stderr，诊断只能落文件。

### 4.3 隔离三轴

| 轴 | 做法 | 为什么 |
|---|---|---|
| **配置** | 父把自己的**生效配置** dump 到 `temp/subagent/<job>/config.toml`，子用 `--config` 读它 | ① 会话中途改配置不会造成父子漂移 ② 不污染用户的 `config.toml` ③ **顺带把历史轴也解决了** —— 快照里可以把 `[store]` 指到独立目录，或直接 `[history] enabled = false` |
| **历史** | 子绝不写父的 `history/`；自己的 transcript 落 `temp/subagent/<job>/transcript.jsonl`，路径在 `final` 里回传，父把它折进**自己的活动流** | 侧栏干净；子代理的产出仍然可见（通过父的 Activity，那条链路已经能随对话文件持久化） |
| **工作区** | 继承父的快照（root + 档位）；**v1 建议强制只读** | 边界只防「越界」，**不防父子同目录并发写** —— 两个进程同时改一个文件，后写覆盖，静默丢改动 |

### 4.4 授权

- **v1**：配置快照里带上父的 `[auth]`，外加父的 `session_allowed` 白名单作为 `--allow-session`
  传入；仍未决的调用按 `[subagent] on_ask = "deny" | "allow"`，**默认 deny**，并把被拒的调用记进
  `final` 摘要。
- **v2**：走 `auth_req` 协议行转发到父的 `Ui::authorize` —— GUI 弹**同一张卡片**，CLI 在终端问。
  语义上很干净：**子代理的权限永远不超过父当场给的**。

### 4.5 生命周期与上限

- 深度：`AOX_SUBAGENT_DEPTH` 环境变量，父传 `depth+1`；超过 `max_depth` 时 `spawn_agent`
  直接报错。**默认 `max_depth = 1`（禁递归）。**
- 并发：`[subagent] max_concurrent`（建议 2–3），超额**报错而不排队**（排队会让模型以为卡住了）。
- 超时：**必须独立** `[subagent] timeout_secs` —— 复用 `[agent] tool_timeout_secs`（默认 60 秒）
  对子代理太短。
- 收尸：复用 `kill_on_drop(true)` + JobObject。「父退全灭」是免费的；「父不死、只停这一个子代理」
  需要 `cancel` 协议行 + 显式 kill —— 那是 P2。

### 4.6 观测

子的 `usage` 行折叠进父的 `UsageRecord`（账里标个 `subagent` 来源），`/usage` 的总账才是真账 ——
否则子代理烧的 token 会凭空消失。P2 起活动流画**嵌套泳道**（现有 Activity 的多段机制可以复用）。

## 5. 分期

| 期 | 内容 | 备注 |
|---|---|---|
| **P0** | `--headless --prompt` 一次性运行 + 单行 JSON 结果；`spawn_agent` 工具**同步阻塞**；无进度、无授权穿透、无递归 | **不动任何核心接口**；一次能跑完就能验收 |
| **P1** | 协议化（含 status/usage/auth_req）+ 三轴隔离 + 深度/并发/超时上限 | 真难点都在这期 |
| **P2** | `Tool::call` 加 progress sink → 活动流实时嵌套泳道；后台任务 + `agent_wait`；定向 cancel | 唯一动核心接口的地方 |
| **P3** | 进程内 runner（免进程开销）/ 远端 runner | 可选 |

## 6. P0 必须先试的一件事

release 下 `aox-gui.exe` 没有控制台（`gui/src/main.rs:26`）。被父以 piped stdio 接管时
`println!` 到底能不能进管道？纸面上成立（`STARTF_USESTDHANDLES` 会设好句柄），但这类事我见过
纸面成立、实测不行的。**所以 P0 第一件事应是一个约 30 行的空壳子进程**：父 spawn 自己
`--headless`，子只打印一行 `{"type":"ready"}`，父读到就算通过。这一步过了，整个方案就没有悬空
的前提了。

## 7. 仍需拍板的五件事

1. **子代理能不能写？** 建议只读。v1 串行只读，要写就交回父（或之后的串行子代理）—— 并发写会
   静默丢改动。
2. **默认深度？** 建议 1（禁递归），可用配置放到 2–3。
3. **P0 同步阻塞，还是直接上后台任务？** 建议先同步 —— 能最快拿到「这事到底成不成」的答案。
4. **子代理的对话要不要在 GUI 侧栏可见？** 建议不可见；产出通过父的活动流呈现。若要做成可见，
   历史隔离那套就要换个设计（子代理会话挂到父会话名下的子树），成本高一档。
5. **`on_ask` 默认 deny 还是 allow？** 建议 deny；allow 等于把子代理变成「无审批执行器」，风险
   比它带来的便利大。

## 8. 风险

**中。** 机制大体已经现成，所以构建风险不是问题 —— 问题在于这个项目体量下**收益与改动不成
比例**：

- 主卖点是「单 exe、几 MB、启动时不带任何解释器」。子代理为此额外引入整套第二个进程生命周期、
  一份协议、三条隔离轴和一套授权策略要维护 —— 而收益只在真正大、需要并行或上下文吃紧的任务
  上才出现。
- 在核心接口（`Tool::call` 的 progress 通道）开放之前，v1 是黑盒，即便能跑，体验增益也有限。
- 因此：**默认保持单循环**；只在具体任务逼着要它时才动手（见文首的重启触发条件）。
