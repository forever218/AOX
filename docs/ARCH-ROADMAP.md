# AOX 架构开放化路线图

> 目标：把「编译期焊死」的四件事 —— **系统提示词 / 界面外观 / 存储 / 主循环** —— 变成可替换的
> 能力缝，并引入分层组合清单，让外观定制与「AI 可操控」有统一入口。
>
> 思想来源：DSH（DeepSeek Harness）的「分层组合 + 可逆注册 + 能力缝」。
> **不照搬其插件形态** —— DSH 是进程内组件 + Node/TS，AOX 是子进程插件 + Rust 单 exe，
> 粒度差一个数量级。可借的是组合方式，不是实现方式。

- **基线提交**：`cc8627b 修改图标`（分支 `main`，工作区干净）
- **开工日期**：2026-10-03
- **当前状态**：阶段 1–5 均已验收（阶段 5 于 2026-10-03 通过，四件事按建议执行，
  含给 `[auth]` / `[agent]` / `[runtimes]` 加的容器级 `#[serde(default)]`，用户已确认接受）。
  **阶段 6 已给出路线、等用户确认 —— 未确认不动刀。**
  用户 2026-10-03 已决定：**原阶段 6 的「UI 定制层 / 类宠物沙箱」移出，单独立项、缓做**
  → 见 **`docs/UI-CUSTOMIZATION.md`**；阶段 6 因此收敛为 **6a 循环可替换 + 6b 组件贡献**。
  **A / B / C 三件事用户已定案（「都按照你的建议来执行」）；6a 已实施完毕、正等你验收。**

---

## 七条设计原则（所有阶段共同遵守）

| # | 原则 | 为什么 |
|---|---|---|
| **P1** | **单一归一化配置树**：每阶段只往同一棵树里加字段；阶段 5 不换模型，只把「来源」从 1 层变成 N 层 | 整条路线能渐进、不返工的关键 |
| **P2** | **缝的统一形状**：`trait`（接口）+ `Provider`（实现）+ `Consumer`（不知道谁在提供）+ `Registration`（可逆句柄，Drop 即注销） | 对齐 Cordis 的「可逆副作用」；Rust 的 `Drop` 天然适配 |
| **P3** | **内核不插件化**：`Runtime` 与循环骨架保持原生编译 | 保住「单 exe / 零解释器也能启动」这个核心卖点 |
| **P4** | **体积纪律**：默认构建（零 feature）每阶段 exe 增量 ≤ 300 KB；新依赖必须能 feature 关掉 | 守住「几 MB 单文件」的体质 |
| **P5** | **每阶段两刀**：先「抽缝但不改行为」（纯重构，可字节级回归），再「接上新的 provider / override」 | 把风险压到最低 |
| **P6** | **每阶段可独立发布、可停**：任一阶段结束都能构建发行版，不留半成品 | 允许随时喊停或调序 |
| **P7** | **授权统一**：插件新增的任何「能力申请」一律并入 `AuthPolicy`（ask/auto/deny） | 开放必须以可控为前提 |

---

## 阶段总览

| 阶段 | 交付物 | 硬依赖 | 风险 | 体积影响 |
|---|---|---|---|---|
| 1 提示词分段化 | `src/prompt.rs` + `/prompt` + `[prompt]` 配置 | — | 低 | ~0（甚至略降） |
| 2 主题与外观数据化 | `theme.toml` + `[theme.*]` 配置 | — | 低 | ~0 |
| 3 存储与策略抽缝 | `Store` / `Policy` trait | — | 中低 | ~0 |
| 4 循环 hook | `Hook` trait + 5 个钩子点 | 3（能力位） | 中 | < 30 KB |
| 5 分层组合清单 | `aox.toml` + `--dump-config` | 1–4 | 中 | < 80 KB |
| 6 循环可替换 + 组件贡献 | `AgentLoop` trait + 组件贡献（hook / prompt 段 / 主题） | 4, 5 | 中高 | **0**（子进程路线） |

**关键路径**：`1 → 4 → 5 → 6`；阶段 2、3 与 1 相互独立，**可任意换序**。
🚩 **阶段 6 原有一个用户指定的追加议题**（「界面定制层 / 类宠物沙箱」）——
**2026-10-03 用户已决定移出、单独立项缓做**（`docs/UI-CUSTOMIZATION.md`），
阶段 6 因此**不再包含 UI 定制层，也不再动体积与安全面**。
🚩 **阶段 6 还有一份「阶段 4 遗留清单」**（4 条，含用户特别点名别忘的**裁剪 / 改写历史**），
在阶段 6 末节，推进到本阶段时**逐条兑现**（已分配到 6a / 6b）。

---

## 阶段 1 · 提示词分段化（Prompt 缝）

**目标**：`SYSTEM_PROMPT` 从编译期常量变成「段列表」，支持内置段 / 用户覆盖段，可重排、可禁用、可追加。

**现状**（`src/agent.rs`）：`const SYSTEM_PROMPT`（约 3 KB 文本，直接躺在二进制 rodata 里）
＋ `env_section(rt)` 拼真实路径 ＋ `Agent::new(..., rules)` 里追加 RULES.md，三段硬拼接。

**改动点**
- 新增 `src/prompt.rs`：`PromptSegment { id, title, body, order, enabled }` + `PromptBuilder`（收集 → 排序 → 拼接）。
- `Runtime` 持有段注册表；启动时注册 2 个**内置段** `core` / `protocol`；
  另有 2 个**运行期生成**的段 `env`（运行环境）/ `rules`（RULES.md），合计 4 段。
- `config.toml [prompt]`：`order = [...]`（重排）、`disable = [...]`（禁用内置段）、
  `[[prompt.segment]]`（追加自定义段，可插到任意位置）。
- `Agent::new` 改为调 `PromptBuilder::build(&rt)`。
- CLI 新增 `/prompt`：打印当前拼装结果 + 每段的来源与顺序。
- 顺带收益：这段几千字的提示词移出二进制 → 体积**反向略降**。

**行为不变基线（本阶段的验收硬指标）**
默认配置下拼出的系统提示必须与今天**逐字节相同**（单测钉死 `sha256`）。
理由不只是整洁 —— **服务端的 prompt cache 命中率依赖前缀字节稳定**，前缀一变缓存全废，直接涨钱。

**验收标准**
1. `cargo test --workspace` 全绿，且新增的「默认输出字节级一致」单测通过。
2. 改 `config.toml` 能重排内置段顺序、禁用某段、在任意位置插入自定义段。
3. `/prompt` 能看到每段标题、顺序与来源（内置 / 配置 / RULES 文件）。

**风险**：低。唯一的坑就是上面那条缓存前缀基线。

**实施记录（2026-10-03）**
- 提示词原文与 `env_section` 由脚本**从 `git HEAD` 原样搬运**，不靠转录；
  另有独立核验脚本对拍：1853 / 777 字符与 HEAD 逐字节一致（不是靠单测自证）。
- 内置常量在运行期按 `"\n\n# 插件协议"` 切分。`split_is_lossless` 测试**第一次跑就抓到**
  「切分把章节标题 `# 插件协议` 也吃掉了」的 bug —— 这正是当初不肯手工拆字符串的原因。
- 顺带消除重复：`RULES.md` 的读取原先在 `src/main.rs` 与 `gui/src/bridge.rs` 各写一遍，
  现在归 `prompt` 一处，`Agent::new` 因此从三参简化为两参。
- `config.toml [prompt]` 的三项能力（`order` / `disable` / `[[prompt.segment]]`）
  已用真实二进制验证：`order=["env"]` 生效、`disable=["protocol"]` 生效且计入
  「未被采用的段」提示、追加段生效；段字符数逐项对账无误。
- `[[prompt.segment]]` 的 id 与内置段同名时**整段替换**内置段 —— 提前对齐阶段 5
  「后层覆盖前层」的语义。
- 测试 72 项全绿（新增 7 项）；`cargo check --workspace --all-targets` 零警告；
  改动文件无 U+FFFD。
- 遗留：`Runtime::add_prompt_segment` 暂无调用方（阶段 6 的插件贡献用），
  已按仓库既有惯例标 `#[allow(dead_code)]` 并注明原因。

---

## 阶段 2 · 主题与外观数据化（Theme 缝）

**目标**：GUI 外观从 `Palette` 常量表变成可覆盖的数据。

**现状**（`gui/src/theme.rs`，686 行）：`const LIGHT` / `const DARK: Palette`（19 个颜色字段）；
`install_style` 把 Palette 映射进 egui `Visuals`。尺寸与字体同样是硬编码常量，
字体只认 5 个环境变量（`AOX_GUI_FONT*`）。

**改动点**
- `Palette` 加 `Serialize/Deserialize`，新增 `theme.toml`（或 `config.toml [theme]`）支持**逐字段覆盖**。
- `[theme.metrics]`：`body_size` / `card_radius` / `item_spacing` / `button_padding` 等现有常量。
- `[theme.fonts]`：字体路径改为配置项（环境变量保留为后备）。
- 全部落到 P1 的归一化配置树 → 阶段 5 之后可被 patch 层覆盖。
- 附赠 2~3 套示例主题（纸 / 墨 / 高对比）。

**验收标准**
1. 改 `theme.toml` 重启即生效：换色、换字号、换字体，无需重编译。
2. 现有两套内置主题（纸 / 墨）与今天**视觉一致**（截图对比）。
3. 注意：`Palette` 定义在 `gui` crate，配置读取要走 `aox::paths`，
   **不要把 egui 依赖拖进核心库 `aox`**。

**风险**：低。

**实施记录（2026-10-03）**
- 配置落在 `config.toml [theme]`（与阶段 1 的 `[prompt]` 对称，P1 一致），另加
  `--theme <file>` **整份取代**（也认 `[theme]` 嵌套写法，所以示例文件能整段粘进 config.toml）。
- **镜像结构**：`Palette` 装的是 `egui::Color32`（没有 `Deserialize`），而核心库不许依赖 egui →
  核心库只放 `PaletteConfig`（18 个 `Option<RgbColor>`，hex 字符串），gui 侧 `merge_palette` 叠成 `Palette`。
  **18 个字段，不是 19**（路线图原写的 19 需修正）。
- `P::xx()` 那几十个调用点**零迁移**：只把 `pal()` 的数据源换成
  `ACTIVE: OnceLock<(Palette, Palette)>`，仍是 `&'static Palette`，取色开销不变。
- `theme::BODY` / `theme::CARD_RADIUS` 两个 `pub const` 换成运行期访问器，迁移 12 处
  （app.rs 6 / activity.rs 1 / markdown.rs 3 / theme.rs 内部 2）。10 项尺度全进 `[theme.metrics]`。
- 明暗优先级（已定）：**上次手动切换记录 > `[theme] mode` > 默认（墨）**。
- **抓到两个真 bug，都属于「文件写了却静默不生效」**：
  1. `--theme` 起初只认裸主题，而示例文件用的是 `[theme]` 嵌套 → 加载失败、静默回退内置配色；
     现在两种写法都认，并补了 `shipped_theme_files_load` 测试把示例文件钉住。
  2. TOML 里 `mode` 写在 `[theme.light]` **之前**会被归到文档根，于是「写了 mode 却不生效」；
     加载器现在表内缺 `mode` 时会在根上兜底。
- **默认零差异已用截图逐像素证明**（纸 / 墨两套，四边各留 14px 且避开浮动滚动条带）：
  **0 个不同像素**；`--theme themes/paper.toml` / `ink.toml` 与内置配色同样 0 差异；
  高对比示例与内置差异 99.94%（1.71M 纯黑像素）。
- 功能实测：`[theme.dark] text="#FF0000"` → 34,551 个强红像素（基线 0）；
  `body_size=20 / card_radius=2 / item_spacing=16` → 24.34% 像素变化；
  `fonts.cjk = simkai.ttf` → 2.68% 像素变化（字形确实换了）。
- **顺带修的坑**：`tools/gui_shot.py` 按**子串**匹配窗口标题，同时开着「AOX」与「AOX 活动流」
  时会抓到活动流那个（本次就踩了）→ 加了 `--exact`。
- 新增 `tools/gen_themes.py`：从 `theme.rs` 的 `LIGHT`/`DARK` **反向生成** `themes/paper.toml`、
  `ink.toml`，并断言字段顺序与 `PaletteConfig` 一致 —— 目的就是不让示例文件与源码漂移。
- 体积：**零新增依赖**（核心库已有 toml/serde，gui 未加依赖）。
- 测试 **88 项全绿**（新增 6 项）；`cargo check --workspace --all-targets` **零警告**；
  改动文件无 U+FFFD、行尾统一。
- 遗留（可选，未做）：CLI 侧没有「校验主题文件」的入口 —— `ThemeConfig` 在 bin 目标里用不到，
  标了 `#[allow(dead_code)]`。想要的话可以加个 `/theme` 子命令，只做解析 + 逐字段报告。

**验收结论（2026-10-03，用户已通过）**
- 验收通过，无附加条件。本阶段关闭；阶段 3 进入「给路线 → 用户确认」环节。
- 验收时确认保留的两条遗留（下阶段以后可选再做）：①CLI 无 `/theme` 校验入口；
  ②`--theme` 与 `[theme]` 是「整份取代」语义（不做逐字段合并），已写进 `config.example.toml` 说明。

---

## 阶段 3 · 存储与策略抽缝（Store 缝 + Policy 缝）

**目标**：`history::Log` 直写 JSONL → `Store` trait；`AuthPolicy` 三档字符串 → 可扩展策略。

**现状**：`src/history.rs`（1367 行）读写全走文件；`src/auth.rs`（52 行）`level_for(kind, name)`。

**改动点**
- `trait Store`：`append_message / append_act / append_usage / rewrite / load / list / delete`。
- `JsonlStore` = 现有实现**原样搬进去**，一行逻辑不改。
- `config.toml [store] backend = "jsonl"`（为将来的 `sqlite` 留位，**本期不实现**）。
- `trait Policy`，`ConfigPolicy` = 现有三档；方法签名预留 `caps`（插件能力申请）参数位，供阶段 4/6 使用。

**验收标准**
1. `cargo test --workspace` 全绿（抽缝前的用例一条都不许动：当时 lib 侧 27 项）。
2. `/history` 全部子命令行为不变。*（实际有两处有意改变，见「实现记录」末段。）*
3. 新增 `Store` 的读写往返单测。

**风险**：中低。`history.rs` 是个 1367 行的大文件 —— **必须先抽 trait、把 JSONL 实现原样搬，
不做任何顺带重构**。这一点不接受妥协。

**本轮细化（2026-10-03 给出的路线 —— 已按此执行，见下方「实现记录」）**

现状盘点（脚本核对）：`history.rs` 1367 行 / `auth.rs` 52 行。调用点分布：
`src/agent.rs`（6 处）、`src/main.rs::history_command`（9 处）、`gui/src/app.rs`（5 处）、
`gui/src/bridge.rs`（1 处）。**关键发现：GUI 也直接用自由函数并自己拼路径**
（`dir(root)` → `file_of(dir, id)`，见 `app.rs` 346/500/546/569 与 `bridge.rs` 352）——
这正是「jsonl 形态泄漏到调用方」的证据，也是抽缝要解决的第一个问题。

**刀一（纯重构，零行为变化）**
- 新增 `Store`（目录级）+ `Session`（会话级 = 原 `Log` 的 `append` / `append_act` /
  `append_usage` / `rewrite`）两个 trait，实现为 `JsonlStore` / `JsonlSession`；
  **现有函数体逐函数搬运，不重写、不顺带重构**。
- `/history rm` 目前在 `main.rs` 里直接 `std::fs::remove_file` —— 收进 `Store::remove`，
  否则换后端时删除语义无处安放。
- `Agent.log`：`Option<Log>` → `Option<Box<dyn Session>>`；`Runtime` 持 `Arc<dyn Store>`
  （agent 线程与 GUI 主线程都要用）。
- `Policy` trait + `ConfigPolicy`（原 `AuthPolicy` 原样搬，三档语义与 override 优先级不变）；
  `level_for` 预留能力位 —— 形参先收 `&[String]`，阶段 6 再提升为 `Cap` 结构体，
  **避免现在造一个没有语义的类型**。

**刀二（接新入口，行为仍然不变）**
- `config.toml [store] backend = "jsonl"`；**只认 jsonl，其它值启动即报错退出** ——
  不接受静默回退（阶段 2 踩过「文件写了却不生效」的坑，静默回退是最难查的一类）。
- 可选 `/store` 子命令：打印当前 backend 与存储位置。

**验收标准（在原有三条之外补充）**
1. `cargo test --workspace` 全绿（抽缝前的用例全部保留，**无一条被改写或删除**）。
2. `/history list|dir|show|open|rm` 输出与改动前一致（含终端配色转义）。
   *（实际有两处有意改变，见「实现记录」末段。）*
3. GUI：对话列表 / 切换 / 活动流 / 用量块行为不变（截图对比 + 活动流行数对账）。
4. 新增 `Store` 读写往返单测（含 `rewrite` 保留 act 行、`prune` 裁剪、`remove`）。

**当时待确认的三个选择（均已按建议采纳，见下方「实现记录」）**

| # | 选择 | 建议 |
|---|---|---|
| A | `Store` 持有 root、方法不带路径（GUI 也改走它） vs 保留 `dir`/`path` 形参 | **持有 root** —— 不带路径才叫抽缝，否则接口继续泄漏 jsonl |
| B | 读写拆 `Store` + `Session` 两个 trait vs 合成一个 | **拆两个** —— 写方法是 `&mut self`，混在一起会逼 GUI 拿 `&mut` |
| C | 实现文件：真搬进 `src/store/` vs `history.rs` 不动、只加薄适配器 | **真搬** —— 薄适配器会留下两个入口，缝就白抽了 |

---

### 实现记录（2026-10-03 完成，已验收）

三个选择（A 持有 root / B 拆 Store+Session / C 真搬进 `src/store/`）**全部按建议采纳**，
刀一（纯重构）+ 刀二（接新入口）均已落地。

**交付物**

| 文件 | 内容 |
|---|---|
| `src/store.rs`（新） | `Session`（写入端：`append/append_act/append_usage/rewrite`）+ `Store`（目录级：`create/open/list/resolve/load/load_acts/load_usage/remove/prune`）；`backend()` 工厂 + `default_backend()` + `default_store()` |
| `src/store/jsonl.rs`（新） | 原 `history.rs` 的 I/O **逐函数原样搬运** → `JsonlStore` / `JsonlSession`；新增 16 项单测 |
| `src/history.rs` | 1367 → 346 行：只剩数据模型与 CLI 文案；`list_text` 改为接收**已取好的数据**，不再自己读盘 |
| `src/auth.rs` | `Policy` trait + `ConfigPolicy`（原 `AuthPolicy` 原样搬）；`level_for` 预留 `caps: &[String]` 能力位；新增 3 项单测 |
| `config.example.toml` | 新增 `[store] backend`（默认 `jsonl`，附「换后端是改配置」的说明） |

**缝的形状**：接口上**不出现任何路径**。`dir` / `file_of` 这类「id ↔ 文件名」的映射是
jsonl 后端的私有细节。调用方（`Agent` / CLI / GUI）一律按 **id** 说话 —— 这正是为了让
将来「一个库装下所有对话」的 sqlite 后端**实现得出来**。GUI 原先自己拼路径
（`dir(root)` → `file_of(dir, id)`，jsonl 形态泄漏的实证）已改为拿 `BootInfo.store`。

**API 变化**：`Runtime::new` 由 `-> Arc<Self>` 改为 `-> Result<Arc<Self>>`，好让
未知 `[store] backend` 在**启动时**报错。CLI 按「配置错误」友好提示 + 等按键
（与 TOML 解析失败同一条路径，双击 exe 不会只看到窗口一闪）；GUI 走 `Fatal`。

**行为上的两处有意改变**（与本节原验收标准第 2 条「逐字节一致」冲突，请确认接受）

1. `/history dir` 的输出多了后端名：`<路径>（后端 jsonl）`。
   理由：将来真加了 sqlite，「我改的配置生效了没有」应当一眼可见，而不用去猜。
2. `/history show|open|rm` 的键解析修掉了**一个老 bug**：`20261003` 这类纯数字日期前缀
   原先被判成序号（`.parse::<usize>()` 成功），于是报「没有第 20261003 个对话」。
   现在按长度分流 —— **< 8 位纯数字 = 序号，≥ 8 位 = id 前缀**（序号不可能有 8 位）。
   这是实机验证时暴露出来的既有缺陷，不是本次重构引入的。

**明确未做**：刀二里的「可选 `/store` 子命令」。`/history dir` 现在已带后端名，
信息等价，没必要再加一个命令入口。

**验证证据**

- `cargo check --workspace --all-targets`：**零告警**（含 `gui/`）。
- `cargo test --workspace`：48 + 48 + 12 全绿（lib 与 bin 各编译同一批测试 + gui 12 项）。
- **CLI 实机**（隔离目录 + 自造配置）：未知后端 → 友好报错后退出（`[配置错误] 未知的存储后端: sqlite`）；
  `/history` 列表 / `dir` / `show` / 实聊落盘 / `rm` 全部走通，新文件内容逐行复核
  （meta → user → 无 system 行）。
- **GUI 实机**（隔离目录 + 预置两份历史）：侧边栏正确列出两条（标题取真首句、条数只数消息、
  不含 act/usage 行）；点击可切入；消息与用量块正确回填（`#1 · 2 次调用 · 输入 30 · 合计 90`）
  → 证明 `Store::list / load / load_usage / load_acts` 全部经由缝生效。

**体积**：未单独测量。本次改动**零新增 crate**、无新增静态数据，按 P4 预期在噪声范围内；
阶段 5 前会统一对一次体积。

**验收结论（2026-10-03，用户已通过）**
- 验收通过，**上面那两处有意行为改变一并接受**（`/history dir` 多印后端名、键解析歧义修正）；
  用户未提附加条件，本阶段不留遗留项。本阶段关闭。
- 明确不做、且用户要求「不要进行下一步」：阶段 4。**下次接手不自行开工**，
  按固定节奏先给路线 → 等确认。

---

## 阶段 4 · 循环 hook（Loop 缝，第一形态）

**目标**：在 `Agent::run_loop` 上挂 5 个钩子点，**不改循环骨架**。这是「改系统行为」的 80% 解法。

**钩子点**：`before_request`（可改 messages / schemas）、`after_reply`（可改或否决 reply）、
`before_tool`（可改 args / 否决）、`after_tool`（可改 output）、`on_error`（可降级处理）。

**改动点**
- `trait Hook`（默认空实现），`Runtime.hooks: Vec<Arc<dyn Hook>>`，按注册顺序执行。
- **内置 0 个 hook** → 行为与今天完全一致。
- 本期只让「配置驱动」的 hook 生效（例如 `[hook] deny_tools = [...]`、`[hook] tool_args_default = {...}`）。
- 让**插件**声明 hook 的机制留到阶段 6。

**行为不变基线**：无 hook 时 `run_loop` 逐路径等价（对比 e2e 事件序列）。

**验收标准**
1. 能通过配置实现两件事：禁止某类工具；给某工具自动补默认参数。
2. 无 hook 时行为与基线一致。
3. 注意：钩子能改 `messages` → 必须同样走 `Agent::push_message` 的落盘约定，
   否则历史文件会失真（内存与文件不同步）。

**风险**：中。风险全在「钩子绕过落盘」这一条上。

### 本轮细化（2026-10-03 给出的路线 —— **待用户确认，未确认不动刀**）

**刀一：抽缝但不改行为（纯重构，零 hook）**

- 新增 `src/hook.rs`（核心库，**零新增依赖**）：
  - `trait Hook`：5 个方法**全部带默认空实现**（`Send + Sync`，方法 `async`，与 `Tool` / `Ui` 同一套约定）。
  - `HookChain`：按注册顺序执行、逐点短路；`run_loop` 只与它打交道，不认识具体 hook。
  - `Verdict`：`Pass` / `Replace` / `Reject(理由)` —— **否决一律要带理由**，理由会被回给模型。
  - `/hook` 的报告文案（对齐 `/prompt`）。
- `Runtime.hooks: Vec<Arc<dyn Hook>>`，在 `Runtime::new` 里与 `auth` / `store` / `prompt` 同批定型
  （P1：只往同一棵树加字段）。**默认配置下这个 Vec 就是空的** —— 刀一的「零行为变化」由此成立。

**5 个点插在哪儿（`run_loop` 内逐点一行，骨架不动）**

| 钩子点 | 插入位置 | 能力 | 否决后 |
|---|---|---|---|
| `before_request` | `tool_schemas()` 之后、`llm.chat` 之前 | 替换本轮 tools 列表；**追加**消息 | 中止本轮（`bail!` 该理由） |
| `after_reply` | `llm.chat` 返回后、**记账之后**（被否决的那次调用的 token 也是真花掉的） | 改 reply 的正文 / tool_calls | 视为无效回复，**复用现有空回复重试路径** |
| `before_tool` | 每个 call 的 args 解析后、`authorize_and_execute` 之前 | 改 args；拦下这次调用 | 不执行，回填 `ToolOutput::err(理由)` |
| `after_tool` | 执行返回后、`ui.emit(ToolResult)` 之前 | 改 `content` / `is_error` | 不适用（只允许改） |
| `on_error` | `llm.chat` 的 `?` 处 | 吞掉错误 → 注入宿主提示后重试（降级） | — |

**「钩子绕过落盘」这条风险怎么消掉（验收标准 3）**

`before_request` 拿到的**不是** `&mut Vec<Message>`，而是「**要追加的消息**」——
循环拿到后一律经 `Agent::push_message` 入列，落盘与内存天然同步。
理由：`push_message` 是 `messages` 的唯一写入口，回滚走 `truncate_messages` + `rewrite`；
把裸 `Vec` 交出去，「内存里有、文件里没有」就是一分钟后必然发生的事。

→ 代价：本期**不开放「裁剪 / 改写历史」**（那需要 `rewrite` 语义）。这类需求留给阶段 6 的 `AgentLoop`。
这是本阶段唯一一处「能力小于路线图原文」的地方，明确记录。

**刀二：配置驱动的 hook（`[hook]`）**

```toml
[hook]
# before_tool：拦下这些工具（支持 `fs_*` 前缀通配），模型会收到一条可读的拒绝理由
deny_tools = ["shell_*"]

# before_tool：给指定工具自动补参数（**只在模型没给该键时补**，从不覆盖模型显式传的值）
[hook.tool_args_default.fs_read_file]
encoding = "utf-8"
```

- 一个 `ConfigHook` 覆盖这两个动作；`[hook]` 全空 → **不注册任何 hook**（保持刀一的零开销）。
- 「补默认参数」的语义定为**只补缺**：覆盖模型的显式意图，会造出「模型说 A、工具收到 B」的怪事，
  那恰恰是这套机制最该避免的失效模式。

**改动文件清单**

| 文件 | 改动 |
|---|---|
| `src/hook.rs` | **新增**：trait + 链 + `ConfigHook` + 注册表 + `/hook` 文案 + 单测 |
| `src/agent.rs` | `run_loop` 挂 5 个点；`llm.chat` 的 `?` 包一层；其余一行不动 |
| `src/config.rs` | `HookConfig` + `Config.hook` + `dummy()` 同步 |
| `src/runtime.rs` | `hooks` 字段 + `hooks()` 访问器 |
| `src/lib.rs` / `src/main.rs` | `mod hook;`（两个 crate root 各一份，既有结构） |
| `src/main.rs` | `/hook` 子命令 + `/help` 一行 |
| `config.example.toml` | `[hook]` 说明块 —— **整块注释掉**，保证新拷贝出来的配置行为零变化 |
| `docs/ARCH-ROADMAP.md` | 实施记录 + 进度表（收尾写） |
| `gui/` | **零改动**（hooks 住在 `Runtime` 里，GUI 走同一个 `Runtime::new`，自动带上） |

**行为不变基线怎么证**

- 刀一：对 `tools/mock_llm.py` 跑 CLI 一整轮（**隔离根**，手法见 TRAPS.md），
  改动前先存一份 stdout 记录与 `history/*.jsonl`，改完再跑一次，两者逐行对比。
- 刀二：同一轮 + `deny_tools` / `tool_args_default`，验「模型请求没变、而工具收到的参数变了」。
- 单测：链的注册顺序、`Verdict` 三种语义、`[hook]` 解析、通配匹配、**只补缺不覆盖**。

**体积 / 回滚**：零新增 crate；预计 < 20 KB（P4 预算 30 KB）。
回滚 = `[hook]` 留空（等价今天），或删掉 `src/hook.rs` 与那 5 个调用点。

**明确不做**：插件声明 hook（阶段 6）、`AgentLoop` / 循环骨架改动、`caps` 能力位填充
（本期两项配置与「能力申请」无关，`level_for` 继续传 `&[]`）、GUI 外观（阶段 6 的 UI 定制层）。

**四件事已由用户确认（2026-10-03，「全部按照你的建议来」）**

| # | 选择 | 定案 |
|---|---|---|
| A | `deny_tools` 新造一套拦截，还是**复用 `[auth] overrides` 并给它加通配** | **复用 auth** —— override 键加 `fs_*` 前缀通配，`[hook]` 不设 `deny_tools` |
| B | `before_request` 的消息改动面：只给「追加」 vs 也给「裁剪」 | **只给追加** —— 裁剪绕不开 `rewrite`，**已记进阶段 6 遗留清单**（用户特别要求别忘） |
| C | `before_tool` 改写 args 后，**已落盘的 assistant 行要不要回写**成新 args | **不回写** —— 历史保持「模型的真实请求」，改写只是宿主侧执行策略 |
| D | `on_error` 覆盖面：只包 `llm.chat` 的 `?`，还是也吃「空回复重试耗尽」的 `bail!` | **只包 `?`** —— 后者是模型行为异常，诊断文案已经写好了 |

**关于 A 的说明（用户已选 A1）**

`[auth] overrides.shell_exec = "deny"` 今天**已经**能「禁止某类工具」，缺的只是通配；
且它的拒绝文案写的是「用户拒绝了这次工具调用」—— 对「宿主策略拦截」这个语义并不准确。

- **A1（推荐）**：给 `[auth] overrides` 的键加通配（`fs_*`），拒绝文案改成中性一句话。
  好处：**不造第二套拦截机制** —— 阶段 3 刚因为「信息等价」砍掉 `/store` 子命令，是同一个判断。
- **A2**：按路线图原文新造 `[hook] deny_tools`。好处：拦截与改写都住在 hook 里，语义集中；
  代价：与 auth 能力重叠，将来「这次到底是谁拦的」要多想一步。

无论选哪个，本阶段的 `Hook` 缝与 `tool_args_default` 都要做（验收标准 1 的两件事都仍是配置驱动），
差别只在「禁止工具」这件事落在哪一层。

### 实施记录（2026-10-03 完成，**待用户验收**）

**交付物**

| 文件 | 内容 |
|---|---|
| `src/hook.rs`（新） | `trait Hook`（5 方法全默认空实现）+ `HookChain`（按注册顺序、遇拒短路）+ `Verdict` + `ErrorAction` + `Request` + `ConfigHook` + `registry_from_config` + `/hook` 报告 |
| `src/config.rs` | `HookConfig` + `Config.hook` + `dummy()` 同步（2 项解析单测） |
| `src/runtime.rs` | `Runtime.hooks: HookChain`，在 `Runtime::new` 里与 auth / store / prompt 同批定型；配置写不通**启动即报错** |
| `src/agent.rs` | `run_loop` 挂 5 个点（每点一处插入，骨架一行未动） |
| `src/auth.rs` | override 键支持尾随 `*` 前缀通配：长前缀优先、同长按字典序 → 判定与 HashMap 遍历顺序无关 |
| `src/main.rs` | `/hook` 命令 + `/help` 一行 |
| `config.example.toml` | `[hook]` 说明块（**整块注释**）+ `[auth.overrides]` 通配说明 |
| `gui/` | **零改动**（钩子住在 `Runtime` 里，GUI 走同一个 `Runtime::new`） |

**与路线的三处出入（都是有意为之，请一并验收）**

1. **`after_reply` 挪到「记账之后」**（路线写的是「记账之前」）。理由：那次调用的 token 已经花出去了，
   按原顺序会让**被否决的调用不记账**，与「出错也要报账」这条既有原则冲突。空链下两种顺序完全等价。
2. **`[hook]` 不含 `deny_tools`** —— 定案 A1 把「禁止某类工具」放在 `[auth]` 的通配键上，
   所以本阶段 `[hook]` 只有 `tool_args_default` 一项。
3. **拒绝文案拆成两句**（`authorize_and_execute` 的 else 分支）：`AuthLevel::Deny` →
   「工具 X 被宿主策略禁止（config.toml 的 [auth]）……」；用户当场拒绝 → 保留原来的
   「用户拒绝了这次工具调用……」。这是 A1 的直接后果 —— 以前两者共用一句话，模型会去**说服用户**。

**验证证据**

- `cargo check --workspace --all-targets`：**零告警**（bin 与 lib 两套编译单元都干净）。
- `cargo test --workspace`：**62 + 62 + 12 全绿**（阶段 3 是 48 + 48 + 12；本轮新增 14 项 =
  hook 8 / auth 4 / config 2）。
- **「空链 = 行为不变」怎么证**：单测 `empty_chain_never_intervenes` 逐点断言空链既不动 tools、
  不塞消息、不改回复、不改参数、不动结果，且 `on_error` 恒 `Propagate` —— 而 `Propagate` 分支
  就是原来的 `?`（`return Err(e)`）。
  严格的「与加钩子之前的二进制逐字节对拍」做不到：那需要一份阶段 4 之前的产物，而 `dist/` 里的
  可执行文件跨越了阶段 1–3，无法用来隔离阶段 4。所以这里用的是「空链逐点 no-op 的结构证明 + e2e 复跑同一序列」。
- **实机 e2e**（`target/_tools/hook_e2e.py`：隔离根 + `tools/mock_llm.py`，三种配置各跑一整轮；
  过程被折叠，靠 `/expand` 才看得到工具实际收到的参数）：

| 配置 | `/hook` | 工具**实际收到**的参数 | 工具结果 | 落盘的 assistant 行 |
|---|---|---|---|---|
| A 基线（无 `[hook]`） | `0 个` | `{"path":"config.toml"}` | `工具不存在: fs_read_file` | `{"path":"config.toml"}` |
| B `[hook.tool_args_default.fs_read_file] encoding="utf-8"` | `1 个` +「补默认参数（只补模型没给的键）：fs_read_file(encoding)」 | **`{"encoding":"utf-8","path":"config.toml"}`** | `工具不存在: fs_read_file` | **仍是 `{"path":"config.toml"}`** |
| C `[auth.overrides] "fs_*" = "deny"` | `0 个`（deny 归 auth，不是 hook） | `{"path":"config.toml"}` | **`工具 fs_read_file 被宿主策略禁止（config.toml 的 [auth]）……`** | 同左（拒绝理由入档） |

  → **B 这一行同时钉住了两条语义**：模型给的 `path` 没被默认值顶掉（只补缺），
  历史文件里留的仍是模型原始参数（决策 C）；**C 这一行**证明通配命中与新文案都生效；
  **A 这一行**的过程序列与阶段 3 的实机记录一致。
- 改动的 9 个文件：**U+FFFD 零命中、行尾全 LF**。

**体积**：零新增 crate（`toml` / `serde_json` / `async-trait` / `tokio` 早在树里）。
release 体积留到阶段 5 前与其它阶段一起量。

**验收结论（2026-10-03，用户已通过）**

- 用户答复：「ok，验收通过，进入下一阶段」→ **阶段 4 关闭**。
- 三处与路线的出入（`after_reply` 挪到记账之后、`[hook]` 不含 `deny_tools`、拒绝文案拆两句）
  **一并接受**，未提附加条件 —— 与阶段 2 一样，本阶段不留遗留项
  （「裁剪 / 改写历史」等 4 条已另有归属：阶段 6 末节的遗留清单）。
- 验收后状态：新增 1 个文件（`src/hook.rs`）+ 改动 8 个文件，**全部仍未提交**
  （用户说过改动统一由他提交）；`dist/` 未重建；本轮**零 git 写操作**。

---

## 阶段 5 · 分层组合清单（组合层）

**目标**：把前 4 阶段散在 `config.toml` 里的 override 入口，统一成 `profile → bundle → patch` 三层。

**改动点**
- `aox.toml`：`profile` 声明有序 bundle 列表 + patch 文件；后层覆盖前层。
- 合成函数**复用 P1 的归一化配置树 —— 只换来源，不换模型**（这是不返工的关键）。
- `aox --dump-config` 与 GUI「关于」窗口按钮：打印合成后的树 + 每项的**来源层**。
- 内置 bundle：`core`（llm-openai + tools + store-jsonl + loop-standard + prompt-core）。

**验收标准**
1. `--dump-config` 能看出每个值来自哪一层。
2. 改 patch 文件能覆盖任意一个值。
3. 旧的 `config.toml` 仍被当作最后一层 patch 读入。

**风险**：中。**已知返工点** —— 阶段 1–4 已经落地的 `config.toml` 键不能失效。
**[需你决策]**：旧键是**长期兼容**（推荐，零迁移成本）还是**一次性迁移**（配置更干净）？

### 本轮细化（2026-10-03 给出的路线 —— **待用户确认，未确认不动刀**）

**先摸清的事实（脚本核对）**

- 配置的读入只有**两个**调用点：`src/main.rs:234`（CLI）与 `gui/src/bridge.rs:421`（GUI），
  都走 `Config::load(&config_path)` → 合成层只要接在这两处，两个前端自动同时生效。
- `paths::resolve_config_path(args)` 与 `arg_value(args, "--flag")` 已存在 →
  `--manifest <path>` / `--profile <name>` 的解析顺手就有，规则也该收在 `paths.rs`（与 `--config` 同处）。
- 「关于」窗口是一张**固定 380px 宽的小卡片**（`gui/src/app.rs:1289`）—— 塞不下整棵树。

**刀一：把「读一份配置」抽成「合成一个层栈」（纯重构，零行为变化）**

- 新增 `src/config/layer.rs`：
  - `Layer { name, kind, doc: toml::Value }`；`kind` = `Bundle` / `BundleFile` / `Patch` / `Config`。
  - `merge(layers) -> (toml::Value, Provenance)`：**表深合、其余整体替换**（后层覆盖前层）；
    `Provenance` = 叶子路径 → 最后写它的那一层。
  - `resolve(...) -> Result<Resolved>`，`Resolved { config: Config, layers, provenance }`。
- **`Config` 的形状一个字段都不动**，仍然是 `toml::from_str::<Config>(&merged)` ——
  这就是 P1 说的「只换来源、不换模型」。
- **没有 `aox.toml` 时，层栈就是 `[config.toml]`**：合成结果与今天逐字段相同（测试钉死）。

**刀二：接上组合清单**

```toml
# aox.toml（应用根下，可选）
profile = "default"

[profiles.default]
bundles = ["core"]
patches = ["aox.local.toml"]     # 可选，按序叠加

[profiles.strict]
bundles = ["core", "strict"]
patches = ["aox.local.toml"]
```

**层栈（顺序即优先级，越靠后越强）**

| 序 | 层 | 来源 | 说明 |
|---|---|---|---|
| 1..n | bundle | 内置 `include_str!`，或 `<应用根>/bundles/<名>.toml`（存在则覆盖内置，且**在 dump 里显式标出**） | bundle 里还能写 `bundles = [...]` 继续聚合 —— **DFS 展开、带环检测** |
| n+1..m | patch | `aox.toml` 里列出的文件（相对应用根） | 用户自己的定制层 |
| 最后一层 | config.toml | `--config` 指定的那份 | **永远是最后一层**（验收标准 3） |

**bundle 的形状**（对齐本阶段原文的五个叶子）

- `core` 自己**不带值**，只聚合：`bundles = ["llm-openai", "tools", "store-jsonl", "loop-standard", "prompt-core"]`。
- 叶子各管一块，携带**等于代码默认值**的出厂值（见决策 B）。
- `strict` 是随附的第二个例子 bundle（更保守的授权与额度），**不进默认 profile**。

**新入口**

- `aox --dump-config`：层清单 + 合成后的树，**每个叶子后面标来源**
  （`bundle:store-jsonl` / `patch:aox.local.toml` / `config.toml` / `默认（代码）`）。
- `aox --profile <名>`：临时换一个 profile（不改 `aox.toml`）；`--manifest <路径>` 指定清单文件。
- CLI `/config` 命令：给人看的短版面（列了哪几层、各层贡献几个键），对齐 `/prompt`、`/hook`。

**改动文件清单**

| 文件 | 改动 |
|---|---|
| `src/config/layer.rs`（新） | `Layer` / `merge` / `Provenance` / `resolve` / `dump_text` + 单测 |
| `src/config.rs` | 加 `pub mod layer;`（**不动 `Config` 本体**）；`Config::load` 保留原样（单文件语义，测试在用） |
| `bundles/core.toml` + 5 个叶子 + `bundles/strict.toml`（新） | `include_str!` 进二进制 |
| `aox.example.toml`（新） | 清单示例，整份可拷贝 |
| `src/paths.rs` | `resolve_manifest_path()`（与 `resolve_config_path` 同处，规则不散开） |
| `src/main.rs` | `--dump-config` / `--profile` / `--manifest` + `/config` + help 一行 |
| `gui/src/bridge.rs` | 配置读入改走合成；`BootInfo` 多带一份层名清单 |
| `gui/src/app.rs` | 「关于」窗口加**一行**「配置层：core → aox.local.toml → config.toml」 |
| `config.example.toml` | 头部加一段「你现在看到的这份是最后一层」的说明 |
| 其它（`theme.rs` / `hook.rs` / `store*` …） | **不动** |

**怎么证「默认零变化」**

- 刀一：无 `aox.toml` 时合成结果 ≡ 直接解析 `config.toml`（单测逐字段 + e2e 复跑同一序列）。
- **`core` 层 ≡ 代码默认**：只用 `core` 合成出来的 `Config`，与「只填必填项」解析出来的 `Config`
  逐字段相同 —— 这条测试专门防「bundle 与 `#[serde(default)]` 两个默认来源悄悄漂移」。
- e2e（隔离根 + mock 端点）：① 无 `aox.toml` ② 有 `aox.toml` + `core` → 过程与落盘应一致；
  ③ 再加一份 patch 改一个值 → **只有那一处**变化。
- 失败路径：认不出的 bundle 名 / 找不到的 patch 文件 / bundle 成环 → **启动即报错**，不静默跳过。
- 「首次运行自动生成 config.toml」那条老路径**不变**（判定只看 `--config` 那份在不在）。

**体积 / 回滚**：零新增 crate（`toml` 早在树里）；预计 < 40 KB（P4 预算 300 KB）。
回滚 = 删掉 `aox.toml`（层栈退化成今天的一条），或整体删掉 `src/config/layer.rs` 与 `bundles/`。

**明确不做**：GUI 里浏览完整来源树（只在「关于」加一行层名）、数组的「追加」语义
（后层一律整体替换；追加要新造语法，留给以后）、阶段 6 的 UI 定制层与进程内组件。

**待你拍板的四件事**

| # | 选择 | 建议 |
|---|---|---|
| A | **旧键**：长期兼容 vs 一次性迁移（本阶段原文点名要你决策） | **长期兼容**：`config.toml` 永远当最后一层 patch 读，现有配置零迁移成本 |
| B | **bundle 是否携带出厂值** | **携带**（叶子 bundle 写出等于代码默认的值）—— 否则 `--dump-config` 里绝大多数键只能显示「默认（代码）」，验收标准 1「看清来源」就名存实亡。代价是多一个默认来源，用「`core` ≡ 代码默认」的测试钉住 |
| C | **bundle 文件来源** | **内置 + 认 `<应用根>/bundles/<名>.toml`**（存在则覆盖内置，且在 dump 里标出这是覆盖）—— 这正是阶段 6「可命名、可分享的定制包」的雏形 |
| D | **GUI 侧范围** | **只在「关于」加一行层名**，完整树走 CLI —— 380px 的卡片塞不下树，塞进去也只是没人看的噪音 |

### 实施记录（2026-10-03）

四件事都按建议执行：**A 长期兼容 / B bundle 携带出厂值 / C 内置 + 认 `<应用根>/bundles/<名>.toml` / D GUI 只加一行**。

**落地的东西**

- 新 `src/config/layer.rs`：`Layer` / `LayerKind` / `Provenance` / `compose` / `resolve` /
  `expand_bundles` / `dump_text` / `layer_summary`（`Config` 形状一个字段没动）。
- 新 `bundles/`：`core`（纯聚合，0 个值）+ 5 个叶子 bundle + `strict`（例子，不进默认 profile），
  全部 `include_str!` 进二进制。
- 新 `aox.example.toml`（清单示例，整份可拷）；`config.example.toml` 头部加了「你看的这份是最后一层」。
- 新入口：`--dump-config` / `--profile <名>` / `--manifest <路径>` / CLI `/config`（`/help` 多一行）。
- GUI：`bridge` 改走 `layer::resolve`（与 CLI 同一条），`BootInfo` 多带一份层名；
  「关于」窗口多一行 `配置层  llm-openai → tools → … → config.toml`。

**三处与路线的出入（都需要过目）**

1. **给 `[auth]` / `[agent]` / `[runtimes]` 各加了一行容器级 `#[serde(default)]`** ——
   这是**唯一**动到 `config.rs` 本体的地方（路线里写的是「形状一个字段都不动」）。
   原因：这三张表是**整表必填**，而组合层必然产出「一张表的局部」（典型：
   `bundles/tools.toml` 被文件覆盖后只剩 `[auth] meta_tools`），局部一出现就 `missing field`
   解析失败，且行号指向合成结果、用户完全看不懂。其余各表（`[prompt]` / `[history]` /
   `[store]` / `[theme.metrics]` / `[hook]`）本来就缺项取默认 —— 这三个属于**遗漏**。
   影响方向是**放宽**：以前写了 `[agent]` 却漏 `tool_timeout_secs` 会启动失败，现在取默认；
   任何现在能用的配置都不会因此出问题。**这一条我做了，请确认接受。**
2. **dump 里 `默认（代码）` 这个标签实际不会出现。** 决策 B 让 bundle 把所有有出厂值的键都
   写出来了，于是合成树里每个值都能追到某一层；它只作为「来源查不到」的兜底留在代码里。
   缺省键（必填项 + `[theme]` 的各尺度）不进合成树 —— 因为 `[theme]` 只对 GUI 生效、
   默认值在 `gui/src/theme.rs`，没有进任何 bundle。dump 末尾用一句话交代这些键的来处。
3. **「关于」里那一行会折行。** 8 层的链约 120 字符，380px 的卡片装不下，实际渲染成
   2~3 行（见 `gui/_p5_about_crop.png`）。不截断、不加省略号 —— 宁可折行也不隐藏信息；
   单层或简单 profile 时仍是一行。

**零漂移怎么证的**

- 单测新增 12 项（`layer.rs` 11 + 示例钉子 1）。其中 `core_equals_code_defaults` 用
  `format!("{:#?}")` 对拍「`core` 合成 + 最小配置」与「直接解析最小配置」，**逐字段相同**
  —— 这条专门防 bundle 与 `#[serde(default)]` 两个默认来源悄悄漂移（决策 B 的代价）。
- `no_manifest_is_single_layer` 钉住「没有 `aox.toml` 时合成结果 ≡ 直接解析 config.toml」。
- e2e（隔离根）：① 无清单 ② 有 `aox.toml` + `core` + 一个 patch →
  **两者完整会话输出逐字节相同**（`diff` 无差异）。dump 里 8 层顺序与来源全对：
  `provider.stream` → `patch:aox.local.toml`、`agent.rules_file` → `bundle:prompt-core`、
  `agent.tool_timeout_secs` → `config.toml`（盖住 bundle 的 60）、`prompt.order` →
  `bundle:prompt-core`。
- 失败路径实测：认不出的 bundle / 显式指定却不存在的清单 / 没有那个 profile / bundle 成环，
  一律启动即报错并等按键，不静默跳过。
- GUI 实测：隔离根跑真实 8 层，点「关于」见到折行的层链（`gui/_p5_about_crop.png`）。
- 测试 **74 + 74 + 12 全绿**（阶段 4 为 62+62+12，新增 12）；
  `cargo check --workspace --all-targets` **零警告**；改动文件无 U+FFFD / CRLF / 制表符。
- 体积：**零新增 crate**（`toml` 早在树里）；估 < 40 KB（P4 预算 300 KB）。
- 回滚：删掉 `aox.toml` 即退化成单层；彻底回滚 = 删 `src/config/layer.rs` 与 `bundles/`。

**顺带修掉的一个文档 bug（阶段 4 留下的）**

- `config.example.toml` 的通配示例写的是 `fs_* = "deny"` —— 而 `*` **不在 TOML 裸键字符集**里，
  照抄会直接解析失败。已改为 `"fs_*" = "deny"` 并补了一句说明。
  `layer.rs` 的 `wildcard_override_key_survives_roundtrip` 把两件事一起钉住：
  「必须加引号」以及「合成要走一遍序列化 → 再解析，不能把这种键弄坏」。

**验收标准（手动步骤）**

1. `aox --dump-config`：看层清单 + 每个值后面的来源（无 `aox.toml` 时只有 `config.toml` 一层）。
2. 把 `aox.example.toml` 复制成应用根下的 `aox.toml`，再 `--dump-config` → 多出 6 个 bundle 层；
   在 `patches` 里加一个文件改任意一个值 → 只有那一处的来源变成 `patch:…`。
3. `aox --profile strict --dump-config` → 多一层 `bundle:strict`，`auth.meta_tools` 变 `deny`。
4. 启动后 `/config` 看层清单短版面；`/help` 里能找到这一条。
5. 故意写错 bundle 名 / 删掉 patch 文件 → 启动即报错。
6. GUI：打开「关于」→ 多一行 `配置层 …`（只有一层时不显示）。
7. 老配置原样可用：**不建 `aox.toml`**，一切与阶段 4 逐字节一致。

**验收结论（2026-10-03，用户已通过）**

- 用户答复：「好，没问题，验收通过，开始下一阶段」→ **阶段 5 关闭**。
- 实施记录里点明的三处出入**一并接受**（尤其第一条：
  `[auth]` / `[agent]` / `[runtimes]` 的**容器级 `#[serde(default)]`** —— 这是本阶段唯一动到
  `config.rs` 本体的地方，用户已确认）；本阶段**不留遗留项**。
- 「顺带修掉的 `fs_*` 引号 bug」也在验收范围内，无异议。

---

## 阶段 6 · 循环可整体替换 + 进程内组件（真·一切皆插件）

**目标**：`AgentLoop` 可整体替换；引入**进程内组件**，让插件能贡献 hook / prompt 段 / 主题，
而不只是工具。

**改动点**
- `trait AgentLoop`，`StandardLoop` = 现有实现（`Agent::run_loop` 原样搬）。
- 进程内组件引擎（**需先决策引擎**，见下表）。
- 组件与子进程工具的授权统一进 `AuthPolicy`：能力申请一律 `ask`。
- 子进程插件形态**保留**（隔离好、语言自由），专供工具。

**引擎选型（唯一需要你决策的技术点）**

| 方案 | 体积增量 | 备注 |
|---|---|---|
| `mlua`（vendored Lua） | +0.3~0.6 MB | 脚本化提示词/主题/hook 足够，沙箱可控 |
| `wasmi`（纯 Rust 解释器） | +0.5~1.2 MB | 真 wasm 沙箱，无 JIT |
| `rhai`（纯 Rust 脚本） | +0.8~1.5 MB | 无 C 依赖 |
| Wasmer | +10~20 MB | 超标 |
| **Wasmtime（JIT）** | **+15~30 MB** | **禁用** —— 会毁掉「几 MB」的体质 |

- 若将来确实需要 JIT：走 `cargo build --features wasm-jit` **单独出包**，主包不动。
  这个「可选重量能力出独立包」的模式，`installer/*.iss` 的 `#ifdef BUNDLE_PYTHON` 已经是现成模板。

### 阶段 6 追加议题：界面定制层（UI 缝）与行为沙箱

> ✅ **已移出阶段 6、单独立项（2026-10-03 用户决定）** —— 用户答复「6C 先不做了，写一个待完成项目，
> 先立个项」。**本项目已缓做**，立项书见 **`docs/UI-CUSTOMIZATION.md`**（含四类能力分级、三条硬约束、
> 三条开工前决策、引擎候选表、待办清单）。下面这一节作为**原始分析存档**保留不删；
> 阶段 6 因此收敛为「6a 循环可替换 + 6b 组件贡献」，**不再包含 UI 定制层**。
>
> 🚩 **提醒义务已完成** —— 用户 2026-10-03 要求：把「类宠物」的界面定制与沙箱开发记在阶段 6，
> 并在推进到本阶段时**由我提醒他重新确认这一块的范围**。本路线开篇已提醒，用户随即决定移出立项。

**议题来源**：用户提出「能不能让整个界面的元素和布局都进入『用户调用 AI 更改』的范围」，
举例为「加一个宠物 / 改 hithere 文字 / 删掉输入预设 / 改输入框样式」，类比「界面是 HTML，
用户做 JS/CSS 注入」（仅为类比，非真注入）。

**已分析的结论**（2026-10-03，**只分析未落代码**）

- egui 是**即时模式**：没有 DOM、没有可寻址节点树。GUI 呈现层是 **7712 行手写调用**
  （app 2698 / activity 2206 / theme 803 / markdown 712 / syntax 660 / bridge 540），`ui.*` 调用约 350 处，
  **零元素注册表**（现有 6 处 `Id::new`/`push_id` 只服务 egui 内部状态）。
  → 所以这不是「把现有代码配置化」，而是**新增一层保留模式定制层**：
  **元素语义 id + 属性覆盖 + 具名插槽 + 沙箱化补丁**。
- 四类能力按风险递增：①文本覆盖 ②样式/属性覆盖 ③结构显隐（即时模式下「删除」运行时**只能做到
  「隐藏且不占位」**）④插槽内注入新组件。①② 根本不需要 AI（配置即可）；③④ 才咬合「让 AI 写东西」。
- **AI 的定位是「离线定制作者」**：读元素清单（id 表 + 可用属性 + 当前值）→ 产出补丁或插件 →
  人工审阅后落地成可命名、可分享、可回滚的定制包。**不是**运行时每帧生成 UI（那会同时输掉延迟、成本、
  可复现性与可审计性）。骨架直接对齐阶段 5 的 `profile → bundle → patch`（只加一个 `[ui]` 分支）。
- 「加宠物」的自然形态：**给插件加「UI 贡献」能力**（插件声明往哪个具名插槽放自绘内容），
  而不是让 AI 直接构造界面。与「进程内组件」同源，故并入本阶段；宠物 = 一个预置 rust 插件 + 一条插槽声明。
- **三条硬约束**（写入验收前提）：
  1. **元素 id 是长期契约**：必须用语义 id（`input.preset[remove]`）而非位置式，并长期维护兼容 ——
     这是本方案最大的持续成本。
  2. **布局不做任意重排**：只开放「插槽内部」与「属性覆盖」，不抽象通用布局语言（成本与收益不成比例）。
  3. **必须可降级**：任何补丁/脚本的解析或求值失败都要回落默认界面（不得白屏/崩溃）；
     属性白名单 + 体量上限 + 能力申请统一进 `AuthPolicy`。

**验收标准**
1. 一个用脚本写的 hook / prompt 段 / 主题能放进 `plugins/` 并生效。
2. 默认包体积增量 ≤ 300 KB（若选轻量引擎，可放宽到 1.2 MB 并单独记账）。

**风险**：**高**。这是唯一同时动「体积」与「安全面」的阶段。

### 🚩 阶段 4 遗留清单（推进到本阶段时**必须逐条兑现**）

> 用户于 2026-10-03 明确要求：阶段 4 有意没开的能力要写在这里，**到阶段 6 时别忘了**。
> 以下每一条都是「当时有意留的口子」，不是遗漏 —— 也别当成「顺手补一下」的小活。

1. **裁剪 / 改写历史（决策 B 的代价）** —— 阶段 4 的 `before_request` 只开放「追加消息」：
   追加是唯一一种**不需要 `rewrite`** 就能落盘的改动。要做「上下文裁剪 / 抹掉某几条历史」，
   就必须走 `Agent::truncate_messages`（内存截断 + `Session::rewrite` 让文件同步回滚），
   而钩子拿不到 `&mut Agent` —— 所以这件事天然属于 `AgentLoop` 这一层：
   循环自己裁、自己 `rewrite`，再重新拼请求。
   **兑现标准**：能用配置表达「上下文超过 N 轮就裁掉最老的几轮」，且裁完内存与对话文件一致
   （重启读回来不残、不留半截 tool_calls）。
2. **`on_error` 的降级（决策 D 的代价）** —— 缝与 `ErrorAction::Retry` 都已就位，
   但本期**没有任何实现去构造 `Retry`**（配置驱动的钩子只做补默认参数），
   所以那个变体在 bin 编译单元里标着 `#[allow(dead_code)]`，理由就写在旁边。
   **兑现标准**：有一个真实的钩子（进程内脚本或一个配置项）在请求失败时返回 `Retry`。
3. **`caps` 能力位仍然是空的** —— `Policy::level_for` 的 `caps` 从阶段 3 就立好了，
   阶段 4 明确**不填**（本阶段的两项配置都不是「能力申请」）。
   进程内组件申请能力时才填，并保证「申请一律 ask」（P7）。
4. **钩子目前只能来自配置** —— 「让脚本/插件声明 hook」是阶段 6 的交付物之一。
   阶段 4 已把它要用的东西备齐：`&Arc<Runtime>` 上下文、`Verdict` / `ErrorAction`
   两种返回值、按注册顺序执行的 `HookChain`、以及 `/hook` 这条「看看现在挂了什么」的入口。

---

### 本轮细化（2026-10-03 给出的路线 —— **待用户确认，未确认不动刀**）

> ✅ **已提醒并已有结论（2026-10-03）**：按你当时的要求，本路线开篇提醒了「界面定制层 / 类宠物沙箱」
> 需要重新确认范围 —— 你随即决定 **6c 先不做、单独立项缓做**（`docs/UI-CUSTOMIZATION.md`）。
> **阶段 6 因此收敛为 6a 循环可替换 + 6b 组件贡献**；下面的路线保留原文（含 6c 部分）作存档，
> 6c 一节已标注移出。

**先摸清的事实（脚本核对，2026-10-03）**

- `Agent::run_loop` 在 `src/agent.rs:238`（238–419 行），5 个钩子点已就位；循环要用的东西全部
  从 `self.rt` / `self.llm` / `self.messages` 取，**没有任何隐藏的全局状态** —— 所以「把循环整段
  搬进一个 trait 实现」在结构上成立。
- 插件协议（`src/plugin/mod.rs`）已经是「一行一个 JSON-RPC」：`initialize` 回
  `{name, tools}`、`tools/call` 执行、`shutdown` 通知 —— **再加几个 method 是同构扩展**，
  既有插件一个字节都不用改。
- 子进程插件**本来就**是「一次工具调用一次 RPC」的成本量级 → **「用子进程做 hook」并不比现有
  工具调用更贵**。这一点直接推翻了「hook 必须进程内」的前提，是本轮路线与原文最大的分歧点。
- 发行体积现值（release，2026-10-03 16:57）：`aox.exe` **2.89 MB** / `aox-gui.exe` **5.83 MB**。
  +300 KB = CLI **+10%**；+1.2 MB = CLI **+41%** —— P4 那条线为什么重要，这两个数字就是答案。
- 「可选重量能力出独立包」的模式**已有现成模板**：`installer/*.iss` 的 `#ifdef BUNDLE_PYTHON`
  + `dist/AOX-*-with-python.exe`（3.2 MB → 11.9 MB 的先例）。

**把阶段 6 拆成可独立验收的子阶段（建议 —— 原为 6a/6b/6c，6c 已移出）**

| 子阶段 | 交付物 | 体积 | 风险 | 兑现的遗留 |
|---|---|---|---|---|
| **6a 循环可替换** | `trait AgentLoop` + `StandardLoop`（`run_loop` 原样搬） | ~0 | 中低 | **#1 裁剪/改写历史** |
| **6b 组件贡献** | 组件能贡献 hook / prompt 段 / 主题 | **0**（子进程路线）/ 或引擎体积 | 中 | **#2 / #3 / #4** |
| ~~6c UI 定制层~~ | **已移出：单独立项、缓做** → `docs/UI-CUSTOMIZATION.md` | 视引擎 | — | — |

理由：P6「每阶段可独立发布」+ P4 体积纪律。一口气做完，等于把「零风险的抽缝」和
「唯一同时动体积与安全面的部分」绑在一次验收里。

**用户决定（2026-10-03）**：6c 先不做 —— 已单独立项（`docs/UI-CUSTOMIZATION.md`），
**阶段 6 收敛为 6a + 6b 两个子阶段**。下面 6c 一节的原文保留作分析存档。

**6a · 循环可替换（抽缝，零行为变化）**

- 新增 `src/agent_loop.rs`：`trait AgentLoop { async fn run(&mut self, agent: &mut Agent, ui: &mut dyn Ui) -> Result<()> }`；
  `StandardLoop` = 现 `run_loop` 的**逐语句搬运**（P5 刀一，不重写、不顺带重构）。
- `Runtime.loop_impl: Arc<dyn AgentLoop>`，在 `Runtime::new` 里定型；`config.toml [loop] impl = "standard"`，
  **只认 `standard`，其它值启动即报错** —— 与 `[store] backend` 同一条原则（认不出就报错，不静默回退）。
- ⚠️ **形状难题（实现前必须先解决的唯一技术点）**：`run_loop` 是 `Agent` 的私有方法，
  它调 `push_message` / `truncate_messages` / `commit_usage` 这些**私有写入口**，还共用私有结构 `TurnAcc`。
  要让外部 trait 实现驱动循环，得给 `Agent` 开一组**受控公开原语**（`push_message` / `truncate_messages`
  / `messages()` / 回合记账），并把 `TurnAcc` 的归属想清楚（`pub` 出去，还是改成
  `Agent::begin_turn()` + `Agent::accrue(usage)` 让账留在 Agent 里 —— 后者更干净，倾向后者）。
  这次**不算「接口泄漏」**：`AgentLoop` 与 `Agent` 是同一层的协作关系（阶段 3 那条「接口上不出现路径」
  讲的是**后端**实现细节不该冒出来，与这里不同）。
- **兑现遗留 #1（裁剪 / 改写历史 —— 你特别点名别忘的那条）**：`StandardLoop` 是第一个「能裁能改」的循环。
  加配置 `[loop] max_rounds = N`：上下文超过 N 轮时，从**最老**的几轮开始裁，
  **保留 system 头 + 最近一轮 tool_calls 与其结果的完整性**（半截 tool_calls 是回放不出来的）。
  裁剪走 `Agent::truncate_messages` —— 内存截断 + `Session::rewrite`，两条腿一起动。
  **兑现标准**：裁完内存与对话文件一致，重启读回来不残、不留半截 tool_calls；不配 `max_rounds` 时
  与今天逐字节相同。
- 验证基线：`StandardLoop` 与现 `run_loop` 对拍（隔离根 + `tools/mock_llm.py` 复跑同一序列，
  完整会话输出 `diff`）；无 `[loop]` 配置时零变化；失败路径（认不出的 impl）启动即报错。

> ✅ **已落地（2026-10-03）** —— 见下方「**6a 实施记录**」。实现时踩到「裁剪方向写反」的坑
> （`truncate` 留头 vs 留尾），已修并补了钉住方向的单测与 e2e 断言，那段值得一读。

**6b · 组件贡献（默认零体积的路线 —— 建议）**

关键判断：路线图原文把「进程内组件」当成实现手段，但回头看目标 —— 「让插件能贡献 hook / prompt 段 /
主题」—— **目标本身并不需要进程内**。真正需要进程内的只有一件事（每帧绘制的宠物），而那属于 6c。

- **推荐（6b-①，体积 +0）**：把能力加在**已有的子进程协议**上。
  - `plugin.toml` 增可选 `[contributes]`：`hooks = [...]` / `prompt_segments = [...]` / `theme = "…"`。
  - `initialize` 的返回体相应扩展（**不传是老形状，老插件继续可用**）；新增 RPC 方法
    `hooks/before_request`、`hooks/after_reply`、`hooks/before_tool`、`hooks/after_tool`、
    `hooks/on_error`、`prompt/segment`、`theme/patch`。
  - 宿主侧写一个 `ComponentHook` 实现**已有的** `trait Hook`，把 RPC 结果映射成
    `Verdict` / `ErrorAction` —— **复用阶段 4 的缝，不新造概念**。
  - 子进程挂掉 = 该组件静默失效（符合「必须可降级」），主循环照常跑。
  - 体积：**0**（现有 crate 全够用）。
- **备选（6b-②，引擎体积）**：只有当你要「低延迟 / 每帧」时才必要 —— 那件事属于 6c。
  **因此引擎选型决策顺延到 6c**（那里才有真实理由）。
- 兑现 **遗留 #2**：`on_error` 的 `ErrorAction::Retry` 首次有真实构造方 —— 组件可返回
  `retry(note)`；同时给 `[hook]` 加一条 `max_llm_retries = N` 的**配置驱动兜底**，
  让「不写脚本也能重试」成立（也顺手摘掉那个 `#[allow(dead_code)]`）。
- 兑现 **遗留 #3**：`caps` 能力位首次填充 —— 组件的 `[contributes]` 声明**就是**能力申请，
  `Policy::level_for` 一律 `ask`（P7）；CLI 走 `/hook` 展示、GUI 走授权弹窗。
- 兑现 **遗留 #4**：钩子不再只能来自配置 —— 组件声明即可。
- 验收标准：一个脚本写的 hook / prompt 段 / 主题放进 `plugins/` 即生效；把组件子进程杀掉后
  宿主照常跑完一轮（可降级）。

**6c · UI 定制层 —— 已移出本阶段（2026-10-03 用户决定：单独立项、缓做）**

→ 立项书：**`docs/UI-CUSTOMIZATION.md`**（含四类能力分级、三条硬约束、三条开工前决策、
引擎候选表、待办清单）。本节的 6c 分析原文保留在**上面「追加议题」**一节里作为存档。

它原本同时决定的三件事（留作将来开工时的输入）：①是否要进程内引擎；②是否接受元素 id 的
长期维护成本；③体积怎么记（单列 1.2 MB 或出独立包）。**这些决策随立项书一起推迟。**

**三件事已全部定案（6c 已移出；2026-10-03 用户答复「都按照你的建议来执行」）**

| # | 选择 | 定案 |
|---|---|---|
| A | 阶段 6 拆成 **6a / 6b 两步** vs 一口气做完 | ✅ **拆** —— 6a 零体积零风险、可先交 |
| B | 6b 走「**子进程组件（+0 KB）**」vs「进程内引擎（+0.3~1.5 MB）」 | ✅ **子进程** —— 目标不需要进程内；引擎用不到，随 6c 推迟 |
| C | `[loop] max_rounds` 的裁剪策略（遗留 #1 的兑现形态） | ✅ **超限裁最老几轮，保 system 头与最近一轮 tool_calls 完整** |

> ✅ **另两件（D / E）也已定（2026-10-03）**：D（6c 本期做？）→ **单独立项、缓做**
> （`docs/UI-CUSTOMIZATION.md`）；E（引擎选型）→ **随 6c 推迟**。五件事至此全部有结论，
> 不再有待拍板项；**6a 已按此实施完毕**（见「6a 实施记录」）。

**明确不做（本阶段，除非你另行要求）**：AI 运行时每帧生成 UI（定位是「离线定制作者」）；
通用布局语言；数组的「追加」语义；`[store] backend` 的第二实现（sqlite）。

**怎么证「6a/6b 默认零变化」**

- 6a：无 `[loop]` 配置时 `StandardLoop` 与现 `run_loop` 对拍（e2e 复跑 + 落盘 `diff`）；
  「认不出的 impl」启动即报错。
- 6b：**不装任何组件**时，宿主侧不注册任何 `ComponentHook`，`HookChain` 仍是空链 ——
  与阶段 4 的「空链 = 行为不变」是同一份证明，直接复用。
- 组件侧：新增一个 `plugins/` 示例组件（贡献一条 prompt 段 + 一个 before_tool），
  在隔离根里验证「模型请求没变、而宿主行为变了」。

---

### 6a 实施记录（2026-10-03 完成，**已验收**）

**A / B / C 三件事用户已定案（「都按照你的建议来执行」）**：拆成两步、6b 走子进程、裁剪策略按建议。
本记录只覆盖 **6a（循环可替换）**；6b 待 6a 验收通过后开工。

**交付物**

| 文件 | 内容 |
|---|---|
| `src/agent_loop.rs`（新，517 行） | `trait AgentLoop`（`name` + `run`）+ `StandardLoop`（= 原 `Agent::run_loop` **逐语句搬运**）+ `build()` 工厂 + `trim_point()` 纯函数 + `report()` + 10 项单测 |
| `src/agent.rs` | 484 → 359 行：`run_loop`（182 行）与 `truncate`/`cap_chars` 搬走；新增**受控原语** `rt/llm/messages/push_message/begin_turn/accrue_call/finish_turn/authorize_and_execute/drop_oldest`；`TurnAcc` 从局部变量变成 `Agent` 字段 |
| `src/config.rs` | `LoopConfig`（`impl` / `max_rounds`）+ `Config.loop_config`（`rename = "loop"`，因为 `loop` 是 Rust 关键字）+ `dummy()` 同步 + 3 项解析单测 |
| `src/runtime.rs` | `loop_impl: Arc<dyn AgentLoop>`，在 `Runtime::new` 里由 `[loop] impl` 定型 |
| `src/lib.rs` / `src/main.rs` | `mod agent_loop;`（两个 crate root 各一份） |
| `src/main.rs` | `/loop` 命令 + `/help` 一行 |
| `config.example.toml` | `[loop]` 说明块 —— **整块注释掉**，保证新拷贝出来的配置行为零变化 |
| `src/hook.rs` / `src/auth.rs` / `src/prompt.rs` | 只改注释：三处「留给阶段 6」的说法更新为已落地 / 改指 6b |
| `gui/` | **零改动**（循环住在 `Runtime` 里，GUI 走同一个 `Runtime::new`） |

**缝的形状**

```rust
#[async_trait]
pub trait AgentLoop: Send + Sync {
    fn name(&self) -> &str;                                // = [loop] impl 认的值
    async fn run(&self, agent: &mut Agent, ui: &mut dyn Ui) -> Result<()>;
}
```

`Agent` 为此开了一组**受控原语**。这不是「接口泄漏」—— 阶段 3 那条「接口上不出现路径」管的是
**后端实现细节**，而 `AgentLoop` 与 `Agent` 是同一层的协作者。真正没放松的那条线是：
**`messages` 的唯一写入口仍是 `push_message`**（它同时写文件），循环只拿得到 `&[Message]` 只读视图，
所以「内存里有、历史文件里没有」这条错位依然造不出来。

`TurnAcc` 的选择：**没有**把它 `pub` 出去，而是改成 `Agent` 的字段，由循环调
`accrue_call()` 记一笔、`run_turn` 调 `finish_turn()` 收口。理由：循环是可替换的，**账不是** ——
换一个实现也得记在同一本账上（`finish_turn` 一次调用都没发生时不落行，空账不值得记）。

**遗留 #1 的兑现：`[loop] max_rounds`**

- 轮次以 **`role == "user"`** 的消息为界；`max_rounds = 0`（默认）= 不裁剪。
- 只从**最老的整轮**开始丢，`messages[0]`（系统提示）永远保留。
- **为什么不需要额外对齐**：切点由「第 N 个 user 消息的下标」直接算出，天然落在一条 user 上 ——
  丢掉最老的整轮，剩下的后缀本身就是一个合法请求体，不会出现「tool 结果留着、它的 assistant
  被丢了」这种端点会直接 400 的状态。这条不变式用 `cut_always_lands_on_a_turn_start`
  在 1~7 轮的组合上全量钉住。
- 落盘对称：`Agent::drop_oldest` 内部走 `Session::rewrite`，内存丢了什么文件也丢什么。

**⚠️ 本轮踩到的坑（值得单独记一笔）**

第一版把裁剪写成了 `agent.truncate_messages(cut)` —— **方向反了**。`Vec::truncate(n)` 的语义是
「留前 n 条」（中断回滚要的正是它），而裁剪要的是「**留后**几轮」。后果是**静默**的：

- 表现是「本轮刚说的话没了、最老的上下文还留着」；
- 而内存与文件**一致地错**，所以单看落盘内容非常像正常；
- 更糟的是，**第一版 e2e 断言太弱**（只查了「剩下 1 条 user」「不以 tool 开头」），
  方向反了照样判定通过 —— 是盯着落盘内容逐行读才发现第 2 行是**第一轮**的输入。

修法：新增 `Agent::drop_oldest(cut)`（`drain(1..cut)`，名字与 `truncate_messages` 刻意区分，
注释里写明两者方向相反），并把 e2e 断言改成断**内容**：「留下的必须是**最新**那一轮的输入、
最老那轮的输入一条不剩」。单测 `trimming_keeps_the_newest_turns_not_the_oldest` 同样断言内容。

**验证证据**

- `cargo check --workspace --all-targets`：**零告警**（bin 与 lib 两套编译单元都干净）。
- `cargo test --workspace`：**87 + 87 + 12 全绿**（6a 之前是 74+74+12；本轮新增 13 项 =
  `agent_loop` 10 / `config` 3）。
- **默认零漂移（A vs B）**：隔离根 + `tools/mock_llm.py`，跑完整一轮会话
  （`/loop` → 一次真实任务 → `/expand`），**显式写 `[loop] impl = "standard"` 与完全不写
  `[loop]`：stdout 逐字节相同、落盘内容完全相同**。
- **裁剪真的生效（C，`max_rounds = 1`，两轮）**：裁剪提示出现；对话文件里只剩
  **第二轮**（`再用一句话总结一下刚才的结论`）的输入；**第一轮的输入一条不剩**；
  文件不以 `tool` 结果开头。内存与文件一致（重启读回来不残）。
- **失败路径（D，`impl = "react"`）**：启动即报错
  `[配置错误] 未知的循环实现: react（当前可用: standard）`，等按键退出，**不静默退回 standard**。
- 改动文件：**U+FFFD 零命中、无制表符、行尾全 LF**。

**体积**：零新增 crate（`toml` / `serde` / `async-trait` 早在树里）。新增代码约 330 行（其中约
180 行是从 `agent.rs` 搬过来的），本次实测 release `aox.exe` = **3,038,208 B（2.90 MB）**。

> 与上一次测得的 2,893,312 B（16:57 构建）相比是 **+141 KB**，但那一对数字**跨了阶段 5 + 6a
> 两个阶段**，所以 6a 单独的增量无法从这一对里摘出来 —— 不假装能。**3,038,208 B 作为
> 6b 开工前的基准记在这里**，正因为 6b 的核心卖点是「子进程路线 +0 KB」，届时直接拿它对比
> 就能证伪或证实。按 P4（预算 300 KB）本轮属噪声范围。
>
> **更正（2026-10-03 22:25 全量发行构建后）**：`bash tools/build_installer.sh`（all + both）
> 跑出的**出货二进制**是 `aox.exe` = **3,038,720 B（2.90 MB）** / `aox-gui.exe` = 5,947,392 B。
> 两者与上面 22:05 的数字**同源码**（最后一次源码改动 21:59）：`aox.exe` 那 +512 B 是 release
> 链接的不可复现抖动；`aox-gui.exe` 的 +116,736 B 则是 21:15 的 gui 源码改动一直没被重新编译
> （上一版 exe 停在 16:57），本次全量构建才纳入。**6b 的基准改以出货值 `aox.exe` =
> 3,038,720 B 为准。**

**验收标准（手动步骤）**

1. `aox` 启动后输入 `/loop` → 看到「循环实现: standard（可选: standard）」与「上下文裁剪: 关闭」。
2. **不建 `[loop]` 段**，随便聊一轮 → 行为与 6a 之前**完全一致**（这条是本次的核心验收点）。
3. `config.example.toml` 里把 `[loop]` 那一段取消注释（`impl = "standard"`）→ 重启，行为不变。
4. 把 `impl` 改成 `"react"` → 启动即报错，提示里带 `react` 与 `standard`。
5. 设 `max_rounds = 1`，连聊两轮 → 第二轮开始时会看到「已丢掉最老的 N 条消息」；
   用 `/history show <序号>` 看全文，应**只剩最后一轮**，且没有半截的 tool 调用。
6. `/help` 里能找到 `/loop` 这一条。

**明确不做**：6b 的组件贡献（子进程 `[contributes]` / `ComponentHook`）、进程内引擎、
`[store] backend` 的第二实现、GUI 侧任何改动。

---

### 6b 实施记录（2026-10-03 完成，**已验收**）

> 面向使用者的独立说明（技术变化 / 体验变化 / 现在能改到多深 / 边界 / 回滚）另见
> **`docs/COMPONENT-CONTRIBUTIONS.md`**。

**三处由用户定案**（「按你推荐来」）：**①A** `HookChain` 内部换 `RwLock` + 运行期追加；
**②** 放宽「组件必须声明工具」（只贡献 hook / 段 / 主题也可以）；**③a** 主题贡献 v1 只做
「声明 → 拉取 → 校验 → 报告」，**不动 GUI 活调色板**。

**交付物**

| 文件 | 内容 |
|---|---|
| `src/plugin/mod.rs` | `[contributes]`（`hooks` / `prompt_segments` / `theme` / `caps`）→ `Contributes`（`is_empty` / `normalize` / `overridden_by`）；`activate()` 里经 **3 个新 RPC** 取贡献并注册；`PROTOCOL_DOC` 加 §4；`PYTHON_SDK` 同步；**6 项单测** |
| `src/hook.rs` | `HookChain` 内部 `Vec` → `RwLock<Vec<Arc<dyn Hook>>>` + `push()` / `snapshot()`；新增 `POINTS`（5 个点名的唯一真值）；`ComponentHook`（实现**既有** `trait Hook`）+ `verdict_of`；**4 项单测** |
| `src/runtime.rs` | `add_hook()`（组件注册入口）；`caps_for(kind)`（按工具种类取该组件声明的能力位） |
| `src/auth.rs` | `ConfigPolicy::level_for` 正式填 `caps`：**插件工具 + caps 非空 ⇒ `Ask`**，位置在「用户显式 override」**之后**、「类别默认」**之前**；**3 项单测** |
| `src/agent.rs` | 授权处 `&[]` → `self.rt.caps_for(&kind).await` |
| `src/agent_loop.rs` | `on_error` 的 `Propagate` 分支接入 `[hook] max_llm_retries`（注入一条 user 提示后重试） |
| `src/config.rs` | `HookConfig.max_llm_retries`（默认 0）+ **1 项单测** |
| `src/prompt.rs` | `Source::Plugin` 摘掉 `#[allow(dead_code)]`（真生产者出现了） |
| `src/main.rs` | `/plugins` 多印一条「组件贡献」；`/help`、`/hook` 文案 |
| `config.example.toml` | `[hook] max_llm_retries` 说明块 —— **整块注释掉** |
| `tools/samples/contributing_component/`（新） | `plugin.toml` + `main.py`：贡献 2 个 hook 点 + 1 条 prompt 段 + 1 项 caps |
| `gui/` | **零改动** |

**缝的形状（协议层，全是同构扩展 —— 既有插件一个字节都不用改）**

```
initialize 的返回里多带一个可选项：
  contributes: { hooks: [...], prompt_segments: [...], theme: bool, caps: [...] }

随后宿主按需拉取（都走既有的一行一个 JSON-RPC）：
  hooks/<point>    → {verdict:"pass"|"reject", reason, ...}；on_error 回 {action:"retry"|"propagate"}
  prompt/segment   → {text:"..."}        （每个声明的段名一次）
  theme/patch      → {theme:{...}}       （theme = true 时一次）
```

`Contributes` 的四条规矩：字段全 `#[serde(default)]`（老清单照旧解析）；`normalize()` 逐条
**校验 hook 点名**（认不出就报错并**列出可用点**，不静默丢）、去重、定序；`initialize` 里带的
**覆盖** `plugin.toml` 里的（live 胜 manifest）；`save` 时空的 `[contributes]` **不落盘**。

**`ComponentHook` 的三个刻意选择**

1. **不持有 `PluginHandle`，只记插件名，调用时现查** → 插件卸载即**自动降级**，不需要反注册
   （`component_hook_degrades_when_the_plugin_is_gone` 钉住）。
2. **失败账与句柄共享同一对 `Arc<AtomicU64>` / `Arc<Mutex<Option<String>>>`** → `/hook` 与
   `/plugins` 报的是同一本账。
3. **只问声明过的点**（`undeclared_points_are_never_asked`）—— 没声明的钩子点，一次 RPC 都不发。

**为什么 `HookChain` 必须支持运行期追加**：`Runtime::new` 时链子已定型，而插件是在那**之后**
才 `activate()` 的。于是 `Vec` → `RwLock<Vec<_>>`；每个钩子点先 `snapshot()`（读锁下克隆一批
`Arc`）再迭代 —— `std::sync` 读锁不是 `Send`，**不能跨 `await` 持有**。**空链仍迭代 0 次**，
阶段 4「空链 = 行为不变」的证明原封不动。

**遗留 #2 / #3 / #4 的兑现**

- **#2（`on_error` 无人构造 `Retry`）** → 现在**两条**真实生产者：①组件在 `hooks/on_error` 回
  `{action:"retry"}`（`ErrorAction::Retry` 上的 `#[allow(dead_code)]` 随之摘掉）；②`[hook]
  max_llm_retries = N` 让**内置循环**在 `Propagate` 上重试 N 次（默认 **0** = 与阶段 4 逐字节相同）。
- **#3（`caps` 一直是空的）** → 组件在 `[contributes] caps` 声明能力位，宿主经 `caps_for()`
  填进 `Policy::level_for`。**只对 `ToolKind::Plugin` 生效，且一律 `Ask`**（P7）；用户显式
  override 仍然压得住它；meta 工具**永不受影响**（三条各有单测）。
- **#4（钩子只能来自配置）** → `HookChain` 现在有**两个来源**：配置（阶段 4）＋ 组件（本轮）。
  `/hook` 的报告把两者分开列。

**⚠️ 本轮踩到的坑：e2e 三次误报（没有一次是代码的错）**

`component_e2e.py` 首轮挂了 B / C / D 三例，逐个查下来**全是断言写得太想当然**：

- **B**「不装组件时 `/hook` 不该提组件贡献」—— 可空链的报告里**本来就有**「两个来源」这句，
  其中含「组件贡献」四字。改成要求**全角冒号**的 `组件贡献：`（真实贡献行用的才是它）。
- **C**「`after_tool` 的印记应出现在历史里」—— 是 e2e 自己的 `history_lines()` 把内容**截到
  120 字**，正好把追加在末尾的印记切掉。放宽到 400 字。
- **D**「卡住的组件应记『已失败 1 次』」—— 这一轮有**两次**工具调用，各自撞上 10 s 超时，
  计数是 **2**。改成 `已失败 (\d+) 次` 且要求 `>= 1`。

教训与 6a 的**是同一条**：**断言要断内容、断确切措辞**，别断「大概出现没出现」；再加一条新的 ——
**测试夹具自己也会截断/改写被测数据**。

**验证证据**

- `cargo check --workspace --all-targets`：**零告警**。
- `cargo test --workspace`：**101 + 101 + 12 全绿**（6a 是 87+87+12；本轮 +14 =
  `hook` 4 / `plugin` 6 / `config` 1 / `auth` 3）。
- **默认零漂移（A）**：隔离根 + `tools/mock_llm.py` 跑完一整轮，`/hook` 报 0 个 hook ——
  与 6a 的 baseline **逐字节相同**。
- **老插件不受影响（B）**：只声明工具、不含 `[contributes]` 的老插件，清单照旧解析、工具照旧跑、
  链上仍是 0 个 hook。
- **组件真的生效（C）**：加载示例组件后，`/hook` 列出 2 个 hook 点 + 1 项 caps；`/plugins`
  显示贡献；`/prompt` 里该段来源是「**插件**」；请求体里出现 `before_request` 注入的标记，
  工具结果末尾出现 `after_tool` 追加的印记。
- **降级真的会降（D）**：把组件改成「`before_request` 睡 25 s」→ 每次调用撞 10 s 超时 →
  **本轮照常走完**（降级为 Pass），失败次数被记下。
- **caps ⇒ ask（E）**：声明了 caps 的插件工具在 CLI 里被问到；EOF 处按既有规则判 `Deny`。
- **重试真的会重（F / G）**：`[hook] max_llm_retries = 2` → 失败后重试 2 次；**不写这一段**
  → 0 次，与之前一致。
- **6a 回归**：`loop_e2e.py` 全绿，A/B 两例 stdout **逐字节相同** —— 证 `HookChain` 从 `Vec`
  换 `RwLock` **没有**动摇 6a 的零漂移。
- 改动文件：**U+FFFD 零命中、无制表符、行尾全 LF**。

**体积**

- 基准（6a 出货值）：`aox.exe` = 3,038,720 B / `aox-gui.exe` = 5,947,392 B。
- 6b 实测：`aox.exe` = **3,122,688 B**（**+83,968 B**，+2.8%）/
  `aox-gui.exe` = **6,024,704 B**（**+77,312 B**）。
- **可复现**：`touch src/main.rs` 强制重链后再测，**仍是 3,122,688 B**（不是链接抖动）。
- **交叉验证**：`aox-gui.exe` 在 6b 中**零源码改动**却同步 +77 KB ⇒ 增量确实来自两个二进制
  **共享的 `src` 代码**，也顺带**反证 6a 的基准没有测错**。
- 归因：内嵌模板字符串（`PROTOCOL_DOC` 3,471 B + `PYTHON_SDK` 4,715 B，共 8.2 KB，且大半
  6a 就有）**不构成主因**；其余是机器码。
- 密度参照：`src` 全树 **11,045 行 → 3.12 MB**，约 **283 B/行**；+84 KB 落在**正常代码增量**
  范围，不是被某个依赖/引擎拖进来的。
- **出货安装包（2026-10-03 23:17 全量构建，`tools/build_installer.sh all both`，一次通过）**：
  `AOX-Setup-0.1.0.exe` 3,301,246 B / `AOX-GUI-Setup-0.1.0.exe` 4,524,600 B /
  `AOX-Setup-0.1.0-with-python.exe` 11,972,499 B /
  `AOX-GUI-Setup-0.1.0-with-python.exe` 13,196,002 B / `aox-python-runtime.zip` 10,922,561 B。
- **结论**：**依赖层面 +0 成立**（无新 crate，`Cargo.toml` / `Cargo.lock` 全程未改）；
  **字节层面 +84 KB**，落在 P4 的 ≤300 KB 预算内 ⇒ **验收标准 #2 满足**。
- **更正**：上文「6b 体积 = 0」应读作「**零依赖增量**」（走子进程、不拉 crate、不要引擎），
  **不是**「零字节增量」—— 后者从来不是子进程路线的卖点。

**验收标准（手动步骤）**

1. **什么都不装**，`aox` 起一轮，输入 `/hook` → 报「循环钩子 · 0 个」，并列出**两个来源**；
   行为与 6b 之前完全一致。
2. 把 `tools/samples/contributing_component/` **整个目录**拷进 `plugins/` → 重启 →
   `/plugins` 应多出一条「**组件贡献：**」，含 2 个钩子点、1 条提示词段、申请能力 `fs_write`。
   （这份示例**特意放在 `tools/samples/` 而不是 `plugins/`**，就是为了默认不加载、不破坏零漂移。）
3. `/hook` → 链上变 1 个，来源那栏写着「组件贡献 · 接 before_request、after_tool」。
4. 让它跑一个工具（比如读个文件）→ **工具结果末尾**应多出 `after_tool` 追加的印记；
   同时上下文里应能看到 `before_request` 注入的规矩段。
5. `/prompt` → 找到段 **`house_rules:rules`**（标题写作 `rules（house_rules）`），
   来源标「**插件**」，排在配置段之后（order 60）。
6. 把 `main.py` 的 `before_request` 改成 `time.sleep(25)` 再重启 → 跑一轮，**界面不应卡死**
   （10 s 超时后降级为 Pass）；`/plugins` 里该组件的失败计数会增加。
7. `config.example.toml` 的 `[hook] max_llm_retries = 2` 取消注释 → 对着会失败的端点连试，
   应看到「模型请求失败，宿主自动重试（第 1/2 次）」；把这段删掉 → 恢复 0 次重试。

**明确不做**：6c 界面定制层（已立项缓做 → `docs/UI-CUSTOMIZATION.md`）；进程内脚本引擎；
**把 `theme_patch` 真正应用到 GUI 活调色板**（v1 只做声明 → 拉取 → 校验 → 报告，即定案 ③a）；
`[store] backend` 的第二实现。

---

## 协作流程（每个阶段固定节奏）

1. 我给出**该阶段的详细路线**：涉及文件、改动点、验收标准、体积影响、回滚方式。
2. **你确认**（或要求调整）—— 未确认不动任何代码。
3. 我实施，**只在确认范围内动刀，不顺带重构**。
4. 我自检：`cargo test --workspace` ＋ 该阶段的行为基线对比 ＋ 中文文件 U+FFFD 扫描。
5. 我给出**你手动验收的具体步骤**。
6. **你验收通过** → 才进入下一阶段的第 1 步。

全程遵守：每阶段结束都能构建发行版；任一阶段都可以喊停或调序。

---

## 进度

| 阶段 | 状态 | 验收日期 |
|---|---|---|
| 1 提示词分段化 | 已验收（2026-10-03） | 2026-10-03 |
| 2 主题数据化 | 已验收（2026-10-03） | 2026-10-03 |
| 3 存储与策略抽缝 | 已验收（2026-10-03） | 2026-10-03 |
| 4 循环 hook | 已验收（2026-10-03） | 2026-10-03 |
| 5 分层组合清单 | 已验收（2026-10-03） | 2026-10-03 |
| 6a 循环可替换 | 已验收（2026-10-03） | 2026-10-03 |
| 6b 组件贡献 | 已验收（2026-10-03） | 2026-10-03 |
| — UI 定制层（原 6c） | **已立项、缓做** → `docs/UI-CUSTOMIZATION.md` | — |
| 7a 插件围栏（新线 · ① Job Object） | **待验收** → `docs/SANDBOX.md` | — |
| 7b 工作区边界（新线 · ③ 文件能力收回宿主 + GUI 入口 + 读写语义 + 命名统一） | **待验收** → `docs/SANDBOX.md` | — |

---

## 阶段 7 · 插件围栏（新线，2026-10-04 立项）

**方案对照、威胁模型、以及「现在到底挡住了什么」，全部写在
[`docs/SANDBOX.md`](SANDBOX.md)** —— 包括 ②a AppContainer / ②b 低完整性级别 /
③ 文件能力收回宿主 / ④ WASM 四套备选方案的代价对照与实施要点。

- **为什么另起一条线**：阶段 1–6 全部在「扩展性」轴上（把编译期硬编码换成可替换接口），
  安全边界轴基本没动。6 收束后重新审计，最大的缺口是「模型自己写的插件能干任何事」。
- **本阶段做两件**：① Job Object 围栏（进程数 / 内存 / UI，由内核强制）与
  ③ 工作区边界（把文件能力收回宿主）。两样都**不含文件系统的 OS 级隔离** ——
  真要挡住恶意第三方插件得靠 ②a，还没做。
- 落地 ①：新增 `src/sandbox.rs` ＋ `config.toml [sandbox]` 段；接入点只有
  `plugin::spawn_command` / `spawn_and_init` / `respawn` 三处（没有第二条启动路径）。
- 证据 ①：`aox selftest` 回归 7 的**三段对拍** —— 默认额度下插件能开子进程、且自身照常干活 /
  `max_processes = 0` 时被围栏拦下 / 关掉围栏又能开（对照组：证明拦住它的确实是 Job，
  而不是这台机器上 python 自己有别的毛病）；
  实机 `aox` 跑一遍，8 个真实插件（python 与 rust 都有）全部 `围栏 ✓`。
- 体积 ①：`aox.exe` 3,122,688 → **3,139,584 B（+16,896，+0.54%）**；
  `aox-gui.exe` 6,024,704 → **6,034,944 B（+10,240）**。远在 ≤300 KB/阶段预算内。
- 新增命令：`/sandbox`（看限制，并复述「它管不了什么」）；`/plugins` 每项多一枚
  `围栏 ✓ / ✗`；启动时若某个插件没套上围栏，CLI 与 GUI 都会出声（共用一份判定与文案）。
- ✅ **三处修复（2026-10-04，用户反馈「都修了」）**：
  ① `Workplace::is_unrestricted()` + `Agent::workplace_reject` 开头短路 —— full 档（配置默认）
     整条判定一步不跑，省掉每次工具调用 65~224 µs 的 `canonicalize`；
  ② `scan_paths` 的值形态闸门（含换行 / 长过 260 字符 ⇒ 不判为路径）—— 修掉「2.2 KB 插件源码
     被整段当路径」：受限档下 `create_plugin` 不再被误拒、full 档不再白烧 ~1 ms；
  ③ `max_processes` 语义改为「插件能再开几个子进程」，默认 1 → 8 —— 见下一条。
- ⚠️ **`max_processes` 的语义在 2026-10-04 改过一次**（同一次反馈）：
  它现在的意思是**「插件能再开几个子进程」（不含插件自己）**，写进 Job 时 `+1`
  （`ACTIVE_PROCESS` 数的是整个 Job，插件自己占一个名额）。以前那个数字是
  「含插件自己的进程总数」，于是默认值 1 = 插件连自己都刚好占满 ⇒ 任何靠 `subprocess`
  干活的插件一律 `WinError 1816 配额不足`（**插件全废，不是变慢**）。
  现在默认 8，`0` 才是「一个都不许」，平台口径收在 `SandboxConfig::job_process_limit` 一处。
- 体积（三处修复之后，2026-10-04 收尾口径）：`aox.exe` **3,209,216 B**、
  `aox-gui.exe` **6,088,704 B** —— 相对 ③ 结束时 +1,024 / +512 B（+0.03%）：
  full 档短路与粗网闸门都是「加一道判断、少一次文件系统调用」，`max_processes` 那次换的是语义与文案。
- 落地 ③：新增 `src/workplace.rs`（三档 + 边界判定 + 越界契约）与 `src/tool/hostfs.rs`
  （四个宿主文件工具），＋ `config.toml [workplace]` 段 —— **默认 `full`，不写则行为不变**。
  接入点：`Runtime.workplace`、`Agent::run_turn`（首条消息建地）、
  `Agent::authorize_and_execute`（早于授权的判定 + 给用户的 `Warn`）、
  `Agent::new_conversation`（换地）、CLI/GUI 各注册一次 `host_file_tools()`、新增 `/workplace`。
- 新增 `ToolKind::Host`：宿主文件工具**不并进** `Meta` —— `meta_tools` 默认 `ask`，
  并进去会让每次读写都弹授权框，而 `fs_*` 那类插件工具是 `auto`，同一件事两套行为最难解释。
- 体积 ③：`aox.exe` 3,139,584 → **3,208,192 B（+68,608，+2.19%）**；
  `aox-gui.exe` 6,034,944 → **6,088,192 B（+53,248）**。比 ① 大得多，因为这一块带来两个
  新模块（约 1,150 行，其中越界契约文案与成套单测占大头）；仍在 ≤300 KB/阶段预算内。
- 证据 ③：`aox selftest` 回归 8 的 A/B 对拍（受限档拦住越界**且文件真的没被创建** /
  full 档同一个调用必须成功 —— 单边测不出「拦住它的到底是工作区还是别的」）；
  另有 `workplace` / `hostfs` 两个模块共 15 条单测（`..` 逃逸、大小写、Pending 必须拒、
  URL 不被误判为路径等）。
- ✅ **GUI 的「工作区」按钮（2026-10-04 落地）** —— ③ 的界面入口，本轮完成。
  输入框**左下角**一枚「工作区 · <档位>」→ 向上弹出三选一（默认 / 选择工作区… / 完全访问）→
  「完全访问」红色加粗 + 红色二次确认 →「选择工作区…」开**内置 egui 目录选择器**
  （盘符 chips / 上级目录 / 子目录列表 / 选定此目录）。新增 `gui/src/workplace_ui.rs`。
  - **界面不自己切档**：`Workplace` 住在 `Runtime` 里、只有 Agent 线程拿得到 ⇒
    新增 `ToAgent::Workplace(Mode)`（请求）+ `FromAgent::Workplace(WorkplaceView)`（回执）。
    回执在**启动 / 每轮结束 / 每次切档后**各推一次（第三处不能省：`default` 档的根要到
    首条消息才建，不重推界面会一直显示「还没定」）。打勾靠 `PartialEq` 比档位，不解析文案。
  - `Workplace::snapshot()`：**一次取锁**同时给「档位 + 根」，`mode()` 也改走它 ——
    防的正是「两次取锁之间首条消息刚好在建地」导致界面上档位与目录对不上（也是坑 1 的复发点）。
    回归 `snapshot_agrees_with_mode_and_root`（含「`default` 建成后档位仍须是 Default」）。
  - `Reject::model_text()` 的提权途径补上界面这一条（两条前端入口都写全）。
  - `Palette` 新增 `danger`（整套配色里**唯一**的色相，只给「完全访问」用）；
    它是普通可覆盖的调色板字段 ⇒ `PaletteConfig` 同步加、`tools/gen_themes.py` 的字段数
    18 → 19、`themes/{paper,ink,high-contrast}.toml` 同步。
  - 体积（2026-10-04 收尾口径）：`aox.exe` **3,210,240 B**（+1,024）、
    `aox-gui.exe` **6,124,544 B**（相对三处修复后 6,088,704，**+35,840 / +0.59%**）——
    新增约 640 行界面代码，仍在 ≤300 KB/阶段预算内。
  - 证据：`--demo` 实机截图逐屏核对（按钮 / 向上弹出 / 打勾 / 红确认框 / 目录选择器 /
    进子目录 / 选定后按钮改名 / 点外部收起）；`cargo check --all-targets` 零警告、
    `cargo test --workspace` 122 + 12 全过、`aox selftest` 全过。
- ✅ **读写语义修正 + 顶栏 Σ 面板偏移（2026-10-04，用户反馈「1.修正偏移，2.修改一点…」）**：
  - **新语义：读不设限，写·删限定在工作区里。** `workplace` 新增 `Access::{Read, Write}` 与
    `check_access`：`check` = `Write`（行为原样），`check_read` = `Read` —— **从不返回 `Err`**，
    只把路径规格化（连 `Pending`「地还没开」也照读）。`read_file` / `list_dir` 走
    `hostfs::guard_read`（不设限）；`write_file` / `remove` 仍走 `guard`（受限）。
    `Agent::workplace_reject` 增加 `name` 参数，经 `tool::host_tool_access` 按工具名分流；
    **插件粗网一律按「写」判**（宿主看不出插件意图，取保守方向：宁可多挡一次读）。
    理由：读是取信息、写删是改设备（不可逆），后果不对称 —— 为「读一份别处的资料」设提权
    只会把用户逼去常开 `full`，边界反而更松。代价如实写进 `docs/SANDBOX.md` 已知边界第一条
    （**这层挡不住「读遍整台机器」**，要连读一起关只有 ②a）。
  - **`token_chip` 的 Σ 用量面板偏移已修**：`Area::fixed_pos` 与 `ui.allocate_*` 同处
    **客户区局部坐标**，原先误加了「视口原点」`viewport().inner_rect.left_top()`（**屏幕坐标**）
    ⇒ 面板随窗口在显示器上的位置整体平移（实测偏 ~(154,14) 逻辑像素）。改为直接用
    `resp.rect.left_bottom() + vec2(0,6)` 当锚点。同一错误在 `workplace_ui::menu` 已先修，
    这处是当时报告、本轮一并修掉的既存 bug。
  - 体积：`aox.exe` **3,215,872 B**（相对 GUI 落地的 3,210,240，**+5,632 / +0.18%**）；
    `aox-gui.exe` **6,123,520 B**（相对 6,124,544，**−1,024 / −0.02%** —— 去掉那段
    「屏幕坐标换算」正好抵掉新增文案）。均在 ≤300 KB/阶段预算内。
  - 证据：`cargo check --workspace --all-targets` 零警告；`workplace` 新增
    `read_is_never_restricted_but_write_is`、`hostfs` 新增 `read_outside_is_allowed_but_remove_is_not`
    （**读区外放行 + 删区外拦截**对拍，只钉一边会变成永远绿的假测试）；`selftest` 回归 8
    加「读区外放行 / 删区外拦截」，并把原先挂在 `read_file` 上的「相对路径必须拒」改挂
    `write_file`（新语义下读**不**拒相对路径）。
- ✅ **工作区界面两处微调（2026-10-04，用户实机验收「界面没问题」之后提的）**：
  - **目录选择器的「选定此目录」从左下角挪到右下角**：同排改成一个 `ui.horizontal` ——
    说明文字照旧在左，右侧用 `with_layout(right_to_left)` 的子布局（它会吃掉剩下的横向宽度，
    于是先放进去的按钮就贴在最右）。与「完全访问」确认框里「取消」的摆法同一套。
  - **上拉菜单只保留「默认」那行的说明文字**，`选择工作区…` / `完全访问` 改为单行 ——
    前者不带说明是因为「当前选了哪个目录」按钮上本来就写着（「工作区 · <目录名>」），
    重复一遍只会把菜单撑高；后者红色加粗本身就是说明。
    连带 `row()` 支持**两种行高**（两行 46 / 单行 32，单行时标题整体居中）——
    单行若沿用 46，那行字会浮在上半截，看着像没画完。
  - 体积：`aox.exe` 3,215,872 B（未变）、`aox-gui.exe` **6,123,520 B**（与上一条同值：
    删掉的两段说明文字与新增的行高/布局分支正好抵消）。
- ✅ **命名统一到 `workplace`（2026-10-04，两轮）**。此前一个功能有三个名字：CLI 命令
  `/workspace`、配置段 `[workspace]`、实际目录 `workplace/`（只有 GUI 的提示用了最后那个）。
  **第一轮**（用户提「命令行是 workspace、GUI 是 workplace」）改的是用户看得见的三面；
  **第二轮**（用户提「把文件 `src/workspace.rs` 也改成 workplace」）连代码侧一起改了。
  - 第一轮：CLI 命令 → `/workplace`；配置段 → `[workplace]`；目录名本来就是 `workplace/`，不动。
  - 第二轮：`src/workspace.rs` → **`src/workplace.rs`**、`gui/src/workspace_ui.rs` →
    **`gui/src/workplace_ui.rs`**；类型 `Workspace` / `WorkspaceConfig` / `WorkspaceView` /
    `WorkspaceUi` → `Workplace…`；字段 `rt.workspace` → `rt.workplace`；
    `paths::resolve_workspace` → `resolve_workplace`。全仓机械替换 + 编译器兜底。
  - **两个旧名都留着**：`/workspace` 是命令的隐式别名（不写进 `/help`）、`[workspace]`
    走 serde 的 `alias`。不留的话，`config.toml` 里写着 `[workspace]` 的用户会被 serde
    **静默忽略**（认不出的段直接跳过）⇒ 以为边界还开着、实际退回 `full` ——
    「写了却不生效」那一类最难查的问题。两条各有一个回归钉着
    （`command_answers_to_both_names_and_parses_args`、`workplace_section_accepts_the_legacy_name`，
    后者还钉住「同一段写两个名字要报错而不是静默取一个」）。
  - 顺带修 **`args` 的剥法**：原来用 `trim_start_matches("/workspace")`，它会**反复**剥前缀 ⇒
    目录名恰好叫 `/workplace` 时，`/workplace /workplace` 被剥成空串＝「不带参数」，
    用户拿到现状而不是切换结果且毫无提示。改成「按名精确匹配 + 剥一次前缀 + 去掉一个空格」。
  - 顺带修一句过期的提示：`Agent::run_turn` 建地时那句「AOX 只会在这个目录里**读写**文件」
    是改语义之前写的，会让用户以为连读也被关住（于是白白去提权）；改成
    「写入与删除都被限制在这个目录里（读取不受限…）」。
  - ⚠️ **机械替换的教训**：`--workspace`（**cargo 的参数**）也在替换面里，被改成了
    `--workplace` ⇒ 文档里那 18 处命令全成了假的。`[workspace]`（cargo 的清单段）因为
    `Cargo.toml` 刻意没进替换范围而躲过一劫。**改名脚本必须先圈定范围，替换完立刻 grep
    复核**——只看「能不能编译过」是查不出这类假的。
  - 体积：`aox.exe` 3,215,872 → **3,215,360 B**、`aox-gui.exe` 6,123,520 → **6,123,008 B**
    （各 −512，纯改名，净增长为零）。验证：`cargo check --workspace --all-targets` 零警告、
    `cargo test --workspace` **127 + 12 全过**、`aox selftest` 全过；另用一份**临时应用根**
    实机跑通（config.toml 里写**旧段名** `[workspace] mode = "default"` ⇒ 正确报「档位：默认」；
    `/workplace` 与 `/workspace` 都响应；`/workplace /workplace` 的参数没被吞掉）。
- ✅ **工作区跟着对话走 + `/new` 回默认档 + GUI 回执（2026-10-04，用户报「每次新建对话，
  默认工作区还是上一个」与「每次进入同一个对话都是不同的工作区」）**。两个现象、三个不同的
  原因，一起修：
  - **GUI 少了一条回执**（真 bug，第一个现象的直接原因）：`ToAgent::NewConversation` 与
    `ToAgent::Resume` 都推了 `Switched` + `UsageLog`，**唯独没推工作区回执** ⇒ 按钮上一直
    挂着上一段对话（甚至上一次点击）的档位名，界面显示的是陈旧值。两处各补一次 `sync_workplace`。
  - **`/new` 一律回到「默认」档**：原先只在 `cfg.mode == "default"` 时才重置，于是
    「上一段挑了某个目录 → 点 + → 新对话还在那个目录里」。现在 `reset_for_new_conversation`
    无条件回到 `Pending`。⚠️ 代价（用户确认接受）：配置写 `mode = "fixed"` 的用户，只有本次
    启动的**第一段**对话落在配置点名的那块地。
  - **工作区按对话存档**：`.jsonl` 新增一种记录 `{"kind":"workplace","mode":…,"root":…}`
    （`Store::{append,load}_workplace`，读的时候取最后一条）；`Agent` 在「每轮开头」与
    「每次切档后」**比对后**追加（没变就不写，免得文件里堆重复行），`resume` 读回来 →
    `Workplace::restore`。`rewrite`（中断回滚）也把它捞出来原样补回 —— 回滚只该丢消息，
    不该把「这段对话在哪块地」一起抹掉。
  - **顺带修 `snapshot()` 的档位来源**（新增 `State::picked`）：判据改成「运行期怎么来的」，
    不再拿 `cfg.mode` 推；`Workplace` 里那份「只用来定启动档」的 `cfg` 字段直接删掉 ——
    留着只会诱使人再按配置推档位。
  - 验证：`cargo check --workspace --all-targets` 零警告 · `cargo test --workspace`
    **131 + 12 全过**（新增 4 条：`snapshot_follows_the_runtime_origin_not_the_config` /
    `new_conversation_always_returns_to_default` / `persisted_and_restore_round_trip` /
    `workplace_record_keeps_the_last_one_across_rewrite`）· `aox selftest` 全过。
    实机（临时应用根）跑通三件事：`mode = "fixed"` ⇒ `/new` 后报「默认」且根还没定；
    手写一份带 `workplace` 记录的旧存档 ⇒ `/history open` 后工作区还原成**它自己**那块；
    在对话里发 `/workplace full` ⇒ 文件里真的追加了 `{"kind":"workplace","mode":"full"}`，
    再打开这段对话即报「完全访问」。
  - 体积：`aox.exe` 3,215,360 → **3,218,944 B**、`aox-gui.exe` 6,123,008 → **6,127,616 B**。
- **下一步**：②b 的 30 分钟 spike（若通，②a 可降级为备选）。
