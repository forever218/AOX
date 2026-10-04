# 组件贡献（阶段 6b）—— 插件不再只是「加工具」

> 本文分两层写：**上半是使用体验**（现在能拿它干什么、怎么下手），**下半是技术实现**
> （协议、缝的形状、为什么这么做）。
> 路线、决策与验收记录见 `docs/ARCH-ROADMAP.md` 的「6b 实施记录」。

---

## 0. 一句话

**在 6b 之前**，插件只能给宿主**加工具**。宿主自己的行为骨架 —— 请求里塞什么话、
工具参数要不要动、工具结果要不要处理、请求失败了要不要重试、某个插件工具要不要先问一句 ——
全部写死在宿主代码里，**只有改宿主源码 + 重新发版**才能动。

**在 6b 之后**，这些位置都成了插件可以**声明并接管**的贡献点。而且走的是插件**本来就有**的
子进程 JSON-RPC 通道：宿主门面没变、依赖没增、**既有插件一个字节都不用改**。

| | 6b 之前 | 6b 之后 |
|---|---|---|
| 插件能改什么 | 只能「提供新工具」 | 工具 **＋ 请求内容 ＋ 工具参数 ＋ 工具结果 ＋ 错误处置 ＋ 权限门槛** |
| 要动宿主什么 | — | 什么都不用动，插件目录里加一段 `[contributes]` |
| 宿主性能开销 | — | 每次钩子点一次本地 RPC（一轮对话 5 个点级别） |
| 新增依赖 | — | **零**（不拉 crate、不要脚本引擎） |

一句话概括它的意义：**把「系统的控制面」从宿主源码里挪出来，交到插件目录里**。
以前插件是外接设备，现在它能调发动机的点火时序了 —— 但**钥匙仍然在宿主手里**（见 §4）。

---

# 第一部分 · 使用体验

## 1.1 一张对照表：想改什么，现在怎么办

| 你想干的事 | 6b 之前 | 现在 |
|---|---|---|
| 给**每一个**请求注入项目规矩（编码规范、说话风格） | 改宿主的系统提示词常量，重编 | 组件贡献一条 `prompt_segments` |
| 工具结果统一盖章 / 脱敏 / 截断 | 不可能 | `after_tool` 钩子 |
| 某个工具调用前自动补/纠正参数 | 只能靠 `[hook] tool_args_default`（只补缺、只能配死值） | `before_tool` 钩子，**可以按内容算** |
| 请求失败自动重试 | 不可能（`Retry` 变体没人构造） | `on_error` 钩子，或配置 `[hook] max_llm_retries` |
| 插件工具调用前必须问用户一句 | 只能整类放开或整类禁掉 | `caps` 能力申请 ⇒ **该插件的工具一律询问** |
| 插件给界面配色 | 不可能 | 组件贡献主题补丁（v1 只到「取回 + 校验 + 报告」，见 §3） |
| 统计/审计每一次模型回复 | 不可能 | `after_reply` 钩子 |

## 1.2 六个现成场景（照着抄就行）

**① 注入项目规矩** —— 最省事、收益最直接的一个。

```python
# main.py
def before_request(params):
    return {"inject": ["本项目禁止顺手重构无关代码。"]}
```

```toml
# plugin.toml
[contributes]
hooks = ["before_request"]
```

这条注入的是**用户消息** —— 它会真的进对话、真的落盘，不是偷偷改请求体。

**② 工具结果盖章 / 脱敏** —— 证明组件确实插到执行路径上了。

```python
def after_tool(params):
    if params["is_error"]:
        return {}                                    # 出错的结果不动，免得淹掉诊断信息
    return {"content": params["content"] + "\n（已由组件审阅）"}
```

**③ 按内容纠正工具参数** —— `[hook] tool_args_default` 只能配死值，这个能算。

```python
def before_tool(params):
    if params["tool"] == "read_file":
        args = params["args"]
        args["path"] = args["path"].replace("\\", "/")   # 统一成 POSIX 分隔符
        return {"args": args}
```

**④ 请求失败自动重试** —— 两条路，按「谁来兜底」选：

```toml
# config.toml：宿主级的兜底预算，不写 = 0 = 与 6b 之前逐字节一致
[hook]
max_llm_retries = 2
```

```python
# 或者由组件按错误类型决定（更细，比如只对 429 重试）
def on_error(params):
    if "429" in params["error"]:
        return {"action": "retry", "note": "上游限流了，稍后再试"}
    return {}
```

**⑤ 让插件工具先问一句** —— 一个插件声明了能力，它的**所有**工具调用都会走询问，
不会搭 `[auth] plugin_tools` 的便车。

```toml
[contributes]
caps = ["fs_write"]     # 非空 ⇒ 本插件工具一律 Ask
```

**⑥ 主题补丁（v1 有限支持）**

```toml
[contributes]
theme = true            # 宿主会用 theme/patch 取回，按 [theme] 的形状校验，然后报告
```

## 1.3 三个命令看状态

| 命令 | 看什么 |
|---|---|
| `/plugins` | 多出一行「**组件贡献：**」——挂钩子点、贡献的段、申请的能力、主题补丁状态 |
| `/hook` | 钩子链现在挂了什么，**配置来源与组件来源分开列**；组件失败次数也在这儿 |
| `/prompt` | 组件贡献的段，来源标「**插件**」（段 id 形如 `house_rules:rules`，排在配置段之后） |

## 1.4 写一个组件：改动只有两处

```toml
# plugin.toml —— 老插件一字节不改；新插件只多这一段
name = "house_rules"
entry = "main.py"

[contributes]
hooks = ["before_request", "after_tool"]
prompt_segments = ["rules"]
caps = ["fs_write"]
theme = false
```

```python
# main.py
from plugin_sdk import serve

def before_request(params):
    return {"inject": ["..."]}

def after_tool(params):
    return {"content": params["content"] + "\n（已审阅）"}

serve(
    "house_rules",
    {"house_rules_check": ("平凡工具（用来试能力申请）", {"type": "object", "properties": {}}, lambda a: "ok")},
    hooks={"before_request": before_request, "after_tool": after_tool},
    prompt_segments={"rules": "## 本项目的几条规矩\n\n1. ..."},
)
```

**`[contributes]` 整段不写 = 老插件**，行为与 6b 之前**逐字节一致**。
可运行的完整示例：`tools/samples/contributing_component/`（默认不加载，验收时手拷进 `plugins/`）。

---

# 第二部分 · 技术层面

## 2.1 协议扩展：三个新方法 + 一个可选字段

插件协议本来就是「stdout/stdin 上一行一个 JSON」的 JSON-RPC，本次是**同构扩展**：

```
initialize 的返回体多一个可选字段：
    contributes: { hooks: [...], prompt_segments: [...], theme: bool, caps: [...] }

随后宿主按需拉取：
    hooks/<point>    → {"verdict":"pass"|"reject", "reason":..., 以及该点的有效载荷}
    prompt/segment   → {"text": "..."}        （每个声明的段名一次）
    theme/patch      → {"theme": {...}}       （theme = true 时一次）
```

- 老插件不认识这些方法也没关系 —— 宿主**只为声明过的点发调用**。
- 段正文与主题补丁**不进清单**：清单是离线声明，正文走 RPC 取，避免把大块文本塞进 `plugin.toml`。

## 2.2 五个钩子点的精确契约

| 点 | 宿主发过去 | 组件可以回什么 | 能改到的东西 |
|---|---|---|---|
| `before_request` | `{tools: [...]}` | `inject: [文本, ...]`、`verdict` | 往请求里**追加用户消息**（只追加） |
| `after_reply` | `{text, tool_calls: 条数}` | `text`、`verdict` | 模型回复的**正文**（工具调用明文不发、也不给改） |
| `before_tool` | `{tool, args}` | `args`、`verdict` | 这次调用的**参数** |
| `after_tool` | `{tool, content, is_error}` | `content`、`is_error`、`verdict` | 工具结果的**正文与成败标记** |
| `on_error` | `{error}` | `action: "retry"`、`note` | 请求失败的**处置**（重试 / 继续上抛） |

三处「刻意不发」是本设计的一部分，不是遗漏：

- `after_reply` **不发**工具调用明文 —— 发了会诱使人以为能改，而改它等于**伪造模型意图**。
- `before_request` **不发**可替换的工具表 —— 那等于把「这一轮模型能看见什么」整个交出去。
- 任何点都**拿不到 `&mut Agent`** —— 所以改不了历史（见 §3）。

`verdict: "reject"` 必须带 `reason`，理由会原样回给模型；**认不出/没给一律当放行**。

## 2.3 声明的四条规矩

1. **全字段可省**（`#[serde(default)]`）—— 老清单照旧解析。
2. **`normalize()` 逐条校验**钩子点名，认不出就**报错并列出可用点**（不静默丢）、去重、定序。
   唯一真值是 `hook::POINTS` 这一个常量表。
3. **`initialize` 里带的覆盖 `plugin.toml` 里的**（进程实况比离线清单准）。
4. **`save` 时空的 `[contributes]` 不落盘** —— 老清单不被宿主写脏。

另外，「只贡献钩子、不给工具」的组件是合法的（6b 特意放宽了这条），
判据是「工具与贡献**都**空才拒」。

## 2.4 `HookChain` 变成「可追加」，以及一个 `Send` 陷阱

**问题**：链在 `Runtime::new` 里就定型了，而插件是在那**之后**才 `activate()` 的。

**改法**：

```rust
pub struct HookChain {
    hooks: std::sync::RwLock<Vec<Arc<dyn Hook>>>,   // 原来是 Vec
}
// 新增：push()（注册入口）与 snapshot()（读锁下克隆一批 Arc，随即放锁）
```

⚠️ **必须 `snapshot()`**：`std::sync` 的读锁**不是 `Send`**，跨 `await` 持有会让整个 future
失去 `Send`。所以每个钩子点是「先取快照、后迭代」，而不是「持锁迭代 + await」。

**零漂移为什么不受影响**：空链**仍然迭代 0 次**。阶段 4「无钩子 = 行为不变」的证明是结构性的，
这次改动一个字都不用改 —— 6a 的 e2e 复跑也印证了（A/B 两例 stdout 逐字节相同）。

## 2.5 `ComponentHook` 的四个设计决定

| 决定 | 换来什么 |
|---|---|
| **不持有 `PluginHandle`**，只记插件名、调用时现查 | 插件卸载/加载失败 ⇒ 钩子**自动降级**为放行，**不需要任何反注册逻辑**（少一条最容易漏的清理路径） |
| 失败计数用**与句柄共享的 `Arc<AtomicU64>`** | `/hook` 与 `/plugins` 读到的是**同一本账** |
| **只问声明过的点** | 没声明的钩子点，一次 RPC 都不发 → 老插件零开销 |
| **一次调用一个 10 秒超时**，任何失败一律降级（Pass / Propagate） | 钩子卡住挡的是**整个主循环**（用户对着转圈），而工具卡住只挡那一次调用 —— 所以钩子的上限必须远小于工具超时（默认 120 s） |

**降级不等于沉默**：所有降级都会记账（次数 + 最近一次原因），在 `/hook` 与 `/plugins` 里看得见。
「静默失效」是这套设计里最想避免的状态 —— 与阶段 3/5 的「写了不生效就报错」是同一条原则。

## 2.6 能力申请进授权（`caps` → `Ask`）

`Policy::level_for(kind, name, caps)` 里那个从阶段 3 就立好、一直**恒传 `&[]`** 的 `caps`，
这次终于有填值者了：

```
ToolKind::Plugin(名) → Runtime.caps_for(kind) → 该插件 [contributes] caps
```

判定规则与**位置**都很关键：

```
1. 用户显式 override（含 "fs_*" 这类前缀通配）   ← 优先级最高，仍能压住 caps
2. 【本次新增】插件工具 + caps 非空 ⇒ Ask        ← 钉在这一层
3. 类别默认
```

放在第 2 层而不是第 1 层，是为了让**用户显式配置永远赢**；放在类别默认之前，
是为了 `plugin_tools = "allow"` 不能把能力申请**架空**。另外：**meta 工具永不受影响**
（它们不是插件工具）。这三条各有单测钉住，既有 13 项授权测试一条都没动。

## 2.7 内置循环的重试预算

`on_error` 的 `ErrorAction::Retry` 从阶段 4 起就存在，但**一直没有生产者**（当时还标着
`#[allow(dead_code)]`）。6b 给了它两条真实来源：

1. 组件在 `hooks/on_error` 里回 `{"action":"retry"}`；
2. 配置 `[hook] max_llm_retries = N` —— 让**内置循环**在「继续上抛」时重试 N 次。

默认 **0**，与阶段 4 的行为逐字节相同。重试是**每轮独立**计数的，重试前会注入一条可读提示。

## 2.8 零漂移与体积

**零漂移的三层证据**

- 结构层：不装组件 ⇒ 宿主不注册任何 `ComponentHook` ⇒ 空链仍迭代 0 次。
- 单测层：`empty_chain_never_intervenes`（阶段 4）+ 本次新增 14 项（含「未声明的点从不被问」）。
- 端到端：隔离根 + mock 端点，不装组件跑完整一轮，与 6a baseline **逐字节相同**；
  只提供工具的老插件也照旧解析、照旧跑。

**体积（P4 纪律：每阶段 ≤ 300 KB）**

| | 6a 出货值 | 6b 实测 | 增量 |
|---|---|---|---|
| `aox.exe` | 3,038,720 B | **3,122,688 B** | +83,968 B（+2.8%） |
| `aox-gui.exe` | 5,947,392 B | **6,024,704 B** | +77,312 B |

- 新增依赖：**0**（`Cargo.toml` / `Cargo.lock` 全程未改）。
- `touch` 强制重链复测仍是 3,122,688 B —— 不是链接抖动。
- **归属确认**：`aox-gui.exe` 本轮**零源码改动**却同步 +77 KB ⇒ 增量确系两个二进制
  **共享的 `src` 代码**，也顺带反证 6a 的基准没测错。
- ⚠️ **「子进程路线 +0 KB」应读作「零依赖增量」**（不拉 crate、不要引擎），
  **不是零字节增量** —— 几百行新代码在 size 优化的二进制里就是几十上百 KB。
  作为密度参照：`src` 全树 11,045 行 → 3.12 MB，约 283 B/行。

## 2.9 改动地图

| 文件 | 改了什么 |
|---|---|
| `src/hook.rs` | `HookChain` 换 `RwLock` + `push()`/`snapshot()`；新增 `POINTS`、`ComponentHook`、`verdict_of` |
| `src/plugin/mod.rs` | `[contributes]` → `Contributes`；`activate()` 取贡献并注册；`PROTOCOL_DOC` §4；`PYTHON_SDK` 同步 |
| `src/runtime.rs` | `add_hook()`（组件注册入口）、`caps_for(kind)` |
| `src/auth.rs` | `level_for` 正式填 `caps`（规则与位置见 §2.6） |
| `src/agent.rs` | 授权处把 `&[]` 换成真实 caps |
| `src/agent_loop.rs` | `on_error` 的 Propagate 分支接入 `[hook] max_llm_retries` |
| `src/config.rs` | `HookConfig.max_llm_retries` |
| `src/prompt.rs` | `Source::Plugin` 摘掉 `#[allow(dead_code)]` |
| `src/main.rs` | `/plugins` 多印「组件贡献：」；`/help`、`/hook` 文案 |
| `config.example.toml` | `[hook]` 说明块（整块注释掉） |
| `gui/` | **零改动** |

---

# 第三部分 · 边界：现在仍然做不到的

诚实地划出来，免得指望错方向：

1. **改不了对话历史**。钩子这一层只开放「追加」。要做「上下文裁剪/抹掉某几条」，
   用 `config.toml` 的 `[loop] max_rounds` —— 它是**唯一**一条能同时改内存与对话文件的路径
   （因为它住在循环层，能调到 `Session::rewrite`）。
2. **换不了循环实现**。`[loop] impl` 目前只认 `"standard"`，认不出**启动即报错**。
3. **主题补丁还没画到界面上**。v1 是「声明 → `theme/patch` 取回 → 按 `[theme]` 校验 → 报告」，
   真正重刷 GUI 活调色板属于界面定制层（原 6c，见 `docs/UI-CUSTOMIZATION.md`）。
   根因是时序：GUI 在建窗口前就把主题定型了，而插件在那之后才加载。
4. **没有进程内脚本引擎**。要「每帧绘制的宠物」那种能力必须进程内，属于 6c；
   本阶段刻意不引入任何引擎，以保住几 MB 的体质。
5. **钩子不该做耗时的事**。它挡着主循环，超过 10 秒就按「组件坏了」降级 —— 这是**故意的**。

---

# 第四部分 · 安全模型

| 关卡 | 规则 |
|---|---|
| 分发 | 示例组件**故意**放在 `tools/samples/`，不进 `plugins/` —— 不装就不生效，零漂移不被破坏 |
| 权限 | 能力申请**一律 `Ask`**（P7）；但用户的显式 `[auth]` override 仍然赢 |
| 降级 | 组件不在 / 超时 / 报错 / 返回值看不懂 ⇒ **一律放行**，只记账，不抛回主循环 |
| 不伪造意图 | 组件改不到「模型要做什么」的记录（工具调用明文不发、不给改） |
| 不接管执行 | 钩子拿不到 `&mut Agent`，改不了历史、换不了工具表 |

**底线**：组件能影响**这一轮怎么走**，但改不掉**已经发生过什么**。

---

# 第五部分 · 怎么回滚

组件贡献**完全住在插件目录里**：

```
删掉 plugins/<组件>/  → 重启  → 链上少一个钩子、提示词少一段，宿主回到 6b 之前的形态
```

- 不用改宿主、不用重新编译、不用动 `config.toml`。
- 只要 `[contributes]` 是空的（或整段不存在），宿主行为与 6b 之前**逐字节一致**。
- 配置侧的两个新东西也都可整块删掉：`[hook] max_llm_retries` 不写 = 0。

---

# 第六部分 · 自检证据（2026-10-03）

| 项 | 结果 |
|---|---|
| `cargo check --workspace --all-targets` | 零告警 |
| `cargo test --workspace` | **101 + 101 + 12 全绿**（本轮 +14：hook 4 / plugin 6 / config 1 / auth 3） |
| `target/_tools/component_e2e.py` | **7/7**（空链零漂移 / 老插件不受影响 / 组件生效 / 降级 / caps→ask / 重试有配置 / 重试无配置） |
| `target/_tools/loop_e2e.py`（6a 回归） | 全过，A/B 两例 stdout **逐字节相同** |
| 全量发行构建 | 四种安装包 + 运行时组件包全部产出（见 `README.md`「发行产物」） |
| 编码 | 改动文件 U+FFFD 0 / 制表符 0 / CRLF 0 |
