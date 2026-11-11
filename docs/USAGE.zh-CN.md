# AOX 使用手册

[← README](../README.zh-CN.md) · [文档索引](README.zh-CN.md) · [English](USAGE.md)

README 上放不下的都在这里：安装包、两个前端接受的全部命令、会话到底存在哪、配置怎么分层、
边界管什么又不管什么，以及怎么自己构建。

**目录** — [安装](#安装与首次启动) · [模型](#模型) · [命令行](#命令行) · [图形界面](#图形界面) ·
[会话](#会话与空间) · [配置](#配置分层) · [工具](#工具) · [边界](#边界) · [回滚](#检查点与回滚) ·
[运行时](#插件运行时) · [远程](#远程接管) · [构建](#从源码构建) · [故障](#故障排查) ·
[目录](#目录说明) · [卸载](#卸载)

## 安装与首次启动

Windows 10 1809+ / x64。每个前端四种包，共八种（见[发行产物](#发行产物)），任选其一即可，
装完就是完整可用的。

| 包 | 内容 |
|---|---|
| `AOX-GUI-Setup-<版本>.exe` | 图形界面版，只有宿主（约 7.1 MB） |
| `AOX-Setup-<版本>.exe` | 命令行版，只有宿主（约 4.4 MB） |
| `…-with-python.exe` | 同上 + `runtime/python`（约 16 / 13 MB） |
| `…-<版本>-zh.exe` | 中文包——界面语言是**编译期**定死的，所以单独一个文件 |

所有变体都带 `runtime/bash`（busybox-w32，637 KB）作为 POSIX shell 的兜底；不往 `PATH` 或用户
环境变量里写任何东西。卸载只删 `runtime`，`plugins/` 与 `.aox/` 属于用户数据，会保留。

两个前端都把「exe 所在目录」当应用根，它同时也是默认工作区——所以绿色版就是拷一个文件夹。

- **GUI**：首次启动自动跳到 **设置 → 模型**，光标落在 key 输入框。
- **CLI**：在 TTY 里问三个问题（端点、key、模型名）写进 `.aox/models.json`，直接回车即跳过；
  `aox --setup` 强制走一遍向导；非 TTY 下不会卡在输入上。

## 模型

端点、key、模型名与它的参数全部存在 `<程序目录>/.aox/models.json`（version 2）。这是**唯一**
存放 key 的地方；CLI 与 GUI 读同一份，GUI 从不写 `config.toml`。

| 字段 | 含义 | 默认 |
|---|---|---|
| `name` | 选择器里显示的名字 | 同模型名 |
| `base_url` / `api_key` / `api_key_env` | 端点、内联 key，或一个环境变量 | — |
| `model` | 发给 API 的模型 id | — |
| `enabled` | 是否出现在输入框的模型列表里 | true |
| `context_window` | 上下文 token 数，用于上下文环与压缩 | 600000 |
| `max_tokens` | 单次请求的输出上限 | 32000 |
| `temperature` | 采样温度 | 0.3 |
| `thinking` | `default` · `off` · `low` · `high` · `max` | default |
| `thinking_wire` | `auto` · `deepseek` · `openai` · `qwen` · `none`，决定思考强度怎么上线 | auto |
| `stream` | 流式输出；端点不认 `stream` 时关掉 | true |

可以存多个模型，其中一个是 active。切换方式：命令行 `/model <序号|名字>`、输入框的模型 chip、
或 **设置 → 模型**——下一个请求生效。「测试」只发一个 token 的探活；「扫描模型」只列出端点自报的
列表（能列出来不代表能聊天）。

旧版本留在 `config.toml` 里的 `[provider]` 只在启动时读**一次**，迁进 `models.json` 之后即失效。

## 命令行

| 命令 | 作用 |
| --- | --- |
| `/help` | 显示帮助（列出全部命令） |
| `/tools` | 列出当前所有可用工具 |
| `/plugins` | 列出已加载的插件，以及每个插件贡献了什么（hook / 提示词分段 / 能力） |
| `/runtime` | 插件运行时状态；`/runtime install python` 装配内置运行时 |
| `/history` | 历史对话：`list` · `dir` · `show <序号\|id>` · `open <序号\|id>` · `rm <序号\|id>` |
| `/new` | 开一段新对话（当前这段已经落盘） |
| `/attach` | 把本地文件放进待发托盘，随下一条消息发出；`/attach clear` 清空 |
| `/demo` | 预览终端排版与折叠效果 |
| `/expand` | 展开上一次被折叠的工具调用 |
| `/usage` | 本轮对话的 token 明细（逐轮、缓存命中/未命中、当前上下文占用） |
| `/prompt` | 系统提示由哪些分段组成（`[prompt]` 可重排 / 关闭 / 新增） |
| `/hook` | 当前生效的循环 hook，来源为 `[hook]` 与插件的 `[contributes]` |
| `/loop` | 主循环实现、上下文裁剪、单轮请求上限（`[loop]`） |
| `/task` | 当前会话的检查点：目标、打勾的计划、验收标准 |
| `/checkpoints` | 本工作区的任务级文件恢复点（`--all` 看全部） |
| `/rollback` | 预览上一个任务的改动；`--yes` 执行（另有 `--force` / `--allow-partial`） |
| `/redo` | 用安全快照撤销最近一次回滚 |
| `/backup` | 恢复系统状态：`status` · `verify` · `prune`（按保留策略回收空间） |
| `/sandbox` | 插件子进程的围栏（`[sandbox]`），并写明它**不**覆盖什么 |
| `/shell` | 内建命令行：方言、边界、上限（`[shell]`） |
| `/web` | 内建抓取工具：触达范围、预算、超时（`[web]`） |
| `/workplace` | 查看 / 切换写入边界：`full` · `default` · `<目录>`（`[workplace]`） |
| `/config` | 这份配置由哪些层组成（`aox.toml` 可自定义层栈） |
| `/model` | 列出已保存的模型；`/model <序号\|名字>` 切换，下一个请求生效 |
| `/remote` | 本会话的手机接管：`start` · `stop`；`list` / `use` / `pair` / `policy` / `remove` |
| `/quit` | 退出（`/exit` 等价） |

其它任何输入都直接发给模型。`Ctrl+C` 中断正在跑的一轮：只丢掉正在执行的那一步，已完成的都留在上下文里。

命令行参数：

| 参数 | 作用 |
|---|---|
| `--config <路径>` | 换一个文件作为最后一层配置 |
| `--plugins <目录>` | 换一个插件目录 |
| `--manifest <路径>` | 显式指定组合清单（默认 `<程序目录>/aox.toml`） |
| `--profile <名字>` | 选清单里的 `[profiles.<名字>]` |
| `--dump-config` | 打印层栈与合成后的完整配置，然后退出 |
| `--setup` | 强制走首次配置向导 |
| `selftest` | 不依赖模型，端到端跑一遍插件流水线 |
| `remote …` | 远程接管的主机侧（见[远程接管](#远程接管)） |

横幅里的项目地址是可点击的 OSC 8 超链接，Windows Terminal / VS Code 终端自动支持，传统 conhost
不支持；用 `AOX_HYPERLINK=1` / `0` 强制开关。输出被重定向时动画自动静默，可用 `AOX_ANIM=1` / `0` 覆盖。

## 图形界面

一个无边框主窗口（初始 1280×800，最小 1000×640）加三个可选工具窗口。顶部条只有抽屉按钮、状态药丸
（`思考中` / `执行 bash` / `3 秒后重试` / `等待你的选择`…）和最小化 / 最大化 / 关闭；双击最大化。
设置齿轮在抽屉头部。

### 左侧抽屉

300 px，**挤压**正文而非浮层，0.18 s 滑出，开合状态记在 `.aox/ui.json`。

- 搜索框按**工作区名**或会话标题过滤，`Esc` 清空。
- 树按工作区分组（来自 `.aox/spaces.json`）：文件夹图标 + 名称 + 会话数，可折叠；悬停组头出现 `+`
  （在该目录新建对话）。
- 会话行显示标题，悬停淡入 `时间 · N 条`；`⋯` 提供 `重命名`、`移动到 ▸`、`删除`（正在聊的那条不能删）。
- 组头右键：`新建对话` · `重命名` · `在文件管理器中打开` · `从 AOX 移除`。
- 底部：`新建工作区`、`打开工作区文件夹`（`<根>/workplace/`）。

### 输入卡

浮动卡片：计划条 → 输入行 → 第二行。

- **计划条**显示 `{已完成}/{总数}` 与当前步骤，默认折叠；展开是逐步骤一行。步骤只读，随检查点保存。
- **附件**：文件拖到窗口任意位置即可；图片会后台解码、在转录里就地显示。每个附件一个 chip，`×` 移除；
  超过 32 MiB 的图片会被拒绝并说明原因。
- **第二行**：模型 chip（`deepseek-chat · 1M`）、工作区 chip（`Default · <目录>` / `完全访问`）、
  恢复 chip（撤销图标 + 数量）、上下文环（`Context: … used, … left`，75% 转警告色、92% 转危险色，
  压缩时旋转）和 `Σ N tok` chip（点开用量明细）。
- 任务执行中发送按钮变**停止**；`Enter` 发送，`Shift+Enter` 换行。
- 占位文字会报告状态：`模型未配置，暂不可对话`、`等待授权…`、`运行中；点右侧停止`。

### 工具窗口

从 **设置 → 额外** 打开（`--demo` 会同时开三个）。每个都是独立的无边框窗口，带置顶 / 最小化 /
最大化 / 关闭。

| 窗口 | 内容 |
|---|---|
| **活动流** | 横向时间轴：输入 / 模型 / 工具 / 系统四条道，色块宽度 = 真实耗时，同一插件同一族色。工具栏：`开头` · `居中` · `最新`、`轮次` + `跳转`、行高、缩放、`适配`、`连线`。点色块看完整内容。 |
| **缓存命中图** | 16×16 格记录最近调用（命中 / 部分 / 未命中）、命中率、省下的 token、历史命中率。 |
| **工作区视图** | 只读文件树 + 本轮的改动列表，`文件` / `差异` 两种视图。 |

### 设置

| 页签 | 内容 |
|---|---|
| `额外` | 三个工具窗口的入口 |
| `模型` | 添加 / 编辑 / 批量管理；`使用` 切模型，右侧开关只决定它是否出现在输入框的列表里 |
| `外观` | `跟随系统` / `暗色 · 墨` / `亮色 · 纸`；欢迎语（`自定` / `一言` / `模型`）；明暗两套文字色；打字机效果；背景图文件夹 |
| `系统` | 只读信息：命令行、运行环境、当前模型、配置文件与层、应用根、工作区、历史、插件与运行时目录；缺 Python 时给出安装入口 |
| `远程` | 启停桥接、配对列表、近期日志 |
| `关于` | 版本、ISC 许可、项目主页 |

### 快捷键

`Enter` 发送 · `Shift+Enter` 换行 · `Ctrl+↑` / `Ctrl+↓` 上/下一个提问 · `Esc` 关闭浮层（或清空搜索；
授权框里等于拒绝）· `y` 允许一次 · `a` 本会话始终允许 · `n` 拒绝。

### 主题

两套配色只差明度：亮色是暖纸（`#F2EFE8`），暗色是暖黑（`#141412`），唯一色相是砖红危险色。
`[theme.light]` / `[theme.dark]` / `[theme.metrics]` / `[theme.fonts]` 逐字段覆盖；
`themes/paper.toml`、`ink.toml`、`high-contrast.toml` 是现成的整包；`--theme <文件>` 整段替换
`[theme]`。选择记在 `.aox-gui-theme`，原生标题栏经 DWM 跟着变。

字体解析顺序：`[theme.fonts]` → `AOX_GUI_FONT` / `…_BOLD` / `…_SERIF` / `…_BRAND` / `…_MONO` →
系统候选（等线、雅黑、黑体…；Cascadia Mono、Consolas…）。

欢迎语以打字机效果出现，背景图从 `assets/light|dark/*.png` 随机取。

## 会话与空间

每个会话就是一个 JSONL 文件：

```
<程序目录>/.aox/conversations/20261105-164145-c314.jsonl
```

文件名主干即会话 id（`年月日-时分秒-4位十六进制`）；`/history` 接受序号、完整 id、id 前缀或 `<id>.jsonl`。

**空间（space）**是逻辑分组，可以指向一个目录，不是存储分区。`spaces.json` 里登记了内置空间
（`本机` = 程序目录、`远程` = 手机来的会话、`完全访问`）、用户空间（`s:<规范化路径>`），以及从未登记过
的目录自动生成的空间（`auto:<路径>`）；`<程序目录>/workplace/**` 下的文件夹归入「本机」。

### 文件格式

一行一个 JSON 对象。首行是 `meta`（`id`、`created`、`model`、`root`、`version`、`origin`、`space`、`room`）；
之后每行是 `message`、`act`、`usage`、`workplace`、`title`、`task` 之一。附件只存路径与文件名，不存 base64。

`message` 行就是 **OpenAI 请求体里的原始消息对象**加一个 `is_error`，所以这个文件既是日志也能直接重放。
几个刻意的取舍没变：追加写；一行坏了不影响整份文件；被中断的半截任务会从文件里回滚掉，不留「有
`tool_calls` 没结果」的残缺记录。

### `/history`

```bash
/history                  # 列表：序号、时间、条数、首条用户输入当标题
/history show 1           # 看第 1 条的全文
/history open 1           # 切到第 1 条接着聊
/history dir              # 打印目录路径
/history rm 2             # 删掉第 2 条
```

### 保留策略

`config.toml` 的 `[history]`：

```toml
[history]
enabled = true   # 关掉后不再写新文件；已有历史仍可查看
keep = 200       # 所有空间合起来计数，0 = 不限制
```

清理是顺带的：只在新建会话文件时执行一次，没有后台线程、没有定时器、启动时也不扫描。
`[store] backend = "jsonl"` 是目前唯一的后端；写了不认识的名字会在启动时报错，而不是悄悄回落。

## 配置分层

配置是**合成**出来的，从低到高：`bundle → patch → config.toml`。写在 `config.toml` 里的永远最强，
所以老配置不用为「分层」做任何迁移。

- `aox.toml`（可选，放在程序目录）是清单：加载哪些 bundle、叠哪些 patch、以及具名 profile。
  `--manifest` / `--profile` 可以临时覆盖。
- bundle 放在 `<程序目录>/bundles/<名字>.toml`，整体替换同名内置层。内置层：
  `core`、`llm-openai`、`tools`、`web`、`store-jsonl`、`recovery`、`loop-standard`、`prompt-core`、`strict`。
- 未知 bundle、缺失 patch、bundle 成环、未知 profile 都是启动错误，不会静默回落。
- `/config` 打印层栈；`aox --dump-config` 打印层栈**和**每个值最终由哪一层写入。

`config.toml` 的分节：`[auth]` `[agent]` `[prompt]` `[sandbox]` `[plugins]` `[tools]` `[workplace]`
`[history]` `[store]` `[recovery]` `[shell]` `[web]` `[runtimes]` `[theme]`。

## 工具

| 分组 | 工具 |
|---|---|
| 插件 | `create_plugin`、`update_plugin`、`list_plugins`、`remove_plugin` |
| 文件 | `read_file`、`write_file`、`edit_file`、`list_dir`、`remove`（进回收站）、`attach_file`、`publish_file` |
| 宿主 | `ask_user`、`find_tools`、`task_state`、`verify` |
| 命令行 | `shell`（cmd / PowerShell / pwsh / git 方言）、`bash`（找到 POSIX shell 才注册）、`shell_job`（长期后台作业） |
| 网络 | `web_fetch`（`readable` / `links` / `text` / `json` / `raw`，`save=true` 落盘） |

`remove` 走**系统回收站**。超过 `[tools] inline_max`（默认 24）时，插件工具会从请求里隐藏，模型调用
`find_tools` 后重新出现；隐藏只是「不投放广告」，按名字调用照样能跑。

### 授权

四道闸门 `meta_tools`、`plugin_tools`、`shell`、`web`，取值 `ask` · `auto` · `deny`，**默认全是 `auto`**。
`[auth.overrides]` 支持精确工具名或插件名，以及尾部 `*` 的前缀通配；含 `*` 的键在 TOML 里必须加引号。
命中顺序：精确 → 最长前缀 → `*` → 分类默认。声明了非空 `caps` 的组件仍然强制询问。

## 边界

两件经常被混为一谈的事：

| | `[workplace]` | `[sandbox]` |
|---|---|---|
| 管什么 | 本会话能**写和删**哪里 | 插件**子进程**怎么跑 |
| 档位 | `default`（程序目录）· `full` · `fixed` + `root` | `enabled`、`max_processes`、`max_memory_mb`、`ui_limits` |
| 谁来执行 | 宿主，作用于文件工具与插件声明的路径参数 | Windows Job Object（`CREATE_SUSPENDED` → 入 Job → 放行，`KILL_ON_JOB_CLOSE`） |

- **读取从不限制**：`read_file` / `list_dir` 可以看任何地方。边界只管写入与删除，并且把插件声明的路径
  参数一律按「写」判定（粗网）。它**挡不住**插件自己 `open()` 一个文件——那需要系统级隔离。
- `full` 必须显式选择，并且要过一道红色确认；`/workplace` 与输入框的工作区 chip 可以中途换档。
- 沙箱是**护栏，不是安全边界**，也不覆盖文件与网络，只在 Windows 上生效。shell 另有自己的 Job
  （`[shell] max_processes`，默认 16），可选走低完整性 helper。

## 检查点与回滚

每个任务都在 `<工作区>/.aox/backup/` 留下一个恢复点：内容寻址（BLAKE3）的对象库 + 不可变节点 +
原子 `cursor.json` + 日志与重做快照。

```bash
/checkpoints                # 列出恢复点（默认 20 个，--all 看全部）
/rollback                   # 预览上一个任务改了哪些文件
/rollback --yes             # 执行恢复（--force 等于 --yes，--allow-partial 接受不完整的点）
/redo                       # 用安全快照撤销这次回滚
/backup status              # 对象数、体积、保留策略
/backup verify              # 重新校验对象
/backup prune               # 按保留策略裁剪并回收
```

`[recovery]` 的默认值：`keep = 30` 个任务、`max_total_mb = 512`、`max_file_mb = 64`、
`max_turn_mb = 256`、`shell_guard = "strict"`、`protect_plugins = true`，外加排除清单。GUI 里
撤销 chip 打开的是同一套东西：预览、风险确认，以及四个维护动作。

## 插件运行时

宿主本身什么都不需要，插件才需要。三种语言，启动时各解析一次，取第一个命中的：

1. `[runtimes]` 里写了**完整路径** → 原样使用（路径不存在只报告，不做替换）；
2. `prefer = "system"`（默认）→ 先 `PATH`，再内置 `runtime/<语言>/`；
3. `prefer = "builtin"` → 先内置，跳过 `PATH`。

| 语言 | 运行已有插件 | 新建插件 |
|---|---|---|
| python | 需要 `python` 3.8+ | 同左 |
| node | 需要 `node` | 同左 |
| rust | 什么都不需要（编译好的 exe） | 需要 `rustc` |

`/runtime install python` 会把 CPython 3.13.7 embeddable（约 10 MB，校验 SHA-256）装进
`runtime/python`；放在 exe 同目录、程序目录、`runtime/` 或 `%LOCALAPPDATA%\AOX` 下的离线包
`aox-python-runtime.zip` 会被优先发现。Node 单文件超过 80 MB，不自动装配；rustc 不内置。嵌入式发行版
**不含 `pip`**，需要时装完整 Python 并把 `[runtimes] python` 指向它。

`bash` **工具**与插件运行时是两回事：Git for Windows → 由 `git.exe` 推导的 bash → `PATH` 上的
`bash.exe` → `runtime/bash/bash.exe` → `runtime/bash/busybox.exe`（WSL 的 stub 会被拒绝）。
busybox 是 POSIX `sh`：没有数组、没有 `[[ ]]`、没有进程替换。

## 远程接管

`aox remote` 把本机注册成 AOXChat 服务器上的一个 agent，并保持一条 WebSocket。群里有人 @ 这个 agent，
就在**本机**跑一轮真实任务，过程、结果与产物回传到房间里。服务器地址不内置；凭证在 `.aox/remote.json`。

```bash
aox remote pair [服务器] [配对码] [agent 名]    # 配对码在手机端：设置 → 添加电脑
aox remote list | use <序号|id> | rename | remove | status
aox remote policy [ask|owner|open]             # 谁可以触发这台机器
aox remote serve                               # 常驻桥接（阻塞）
```

会话里 `/remote start` / `/remote stop` 在后台跑同一条桥，`/remote list|use|pair|policy|remove`
与上面一致。`call_policy` 默认 `ask`：别人 @ 你时需要你点头。手机接管期间会盖一层模态
（`已被远程接管`，只留「取回控制权」），键盘输入被吞掉，`.aox/remote.lock` 记录这次接管。

## 从源码构建

需要 Rust 1.90+ 与 MSVC 工具链。

```bash
cargo build                      # 只构建 CLI（workspace 默认成员）
cargo build -p aox-gui --release  # GUI → target/release/aox-gui.exe
AOX_LANG=zh cargo build -p aox-gui --release   # 中文界面，编译期嵌入
```

`AOX_LANG` 只决定把哪个 `locales/<code>.toml` 嵌进二进制——没有运行时语言切换，这也是中文安装包
必须是单独文件的原因。

**Windows + Git Bash**：Git Bash 自带的 coreutils `link` 会抢占 MSVC 的 `link.exe`
（`link: extra operand '...rcgu.o'`）。在 `.cargo/config.toml` 里钉住链接器与运行库：

```toml
[target.x86_64-pc-windows-msvc]
linker = "C:\\Program Files\\Microsoft Visual Studio\\2022\\Community\\VC\\Tools\\MSVC\\<版本>\\bin\\Hostx64\\x64\\link.exe"

# 静态链接 MSVC 运行库（/MT 等价），**发行必加**：
# 否则 exe 依赖 VCRUNTIME140.dll，而干净装机上没有这个文件。
rustflags = ["-C", "target-feature=+crt-static"]

[env]
LIB     = { value = "<MSVC>\\lib\\x64;<WinSDK>\\Lib\\<版本>\\um\\x64;<WinSDK>\\Lib\\<版本>\\ucrt\\x64", force = true }
INCLUDE = { value = "<MSVC>\\include;<WinSDK>\\Include\\<版本>\\um;<WinSDK>\\Include\\<版本>\\ucrt;<WinSDK>\\Include\\<版本>\\shared;<WinSDK>\\Include\\<版本>\\winrt", force = true }
```

这个文件含本机绝对路径、已被 `.gitignore` 排除，需要自建。用 PowerShell / cmd / 已 `vcvars64` 的终端
构建时 `linker` / `LIB` / `INCLUDE` 可以不要，但 **`rustflags` 必须留着**，否则 `build.rs` 会打一条
显眼的 `cargo:warning`，提醒产物在干净机器上起不来。

### 发行产物

```bash
bash tools/build_installer.sh                    # 八个安装包 + 运行时组件包
bash tools/build_installer.sh all none           # 只要两个前端 × 两种语言的主包
bash tools/build_installer.sh gui with-python    # 只打 GUI 的捆绑变体
bash tools/build_installer.sh pack               # 只产出 aox-python-runtime.zip
```

`installer/aox.iss` 与 `installer/aox-gui.iss` 各编译两遍（带不带 `BUNDLE_PYTHON`），`AOX_LANG=zh`
让产物多一个 `-zh` 后缀。

| 产物 | 内容 | 体积 |
|---|---|---|
| `AOX-Setup-<v>.exe` / `AOX-GUI-Setup-<v>.exe` | 只有宿主 | 约 4.4 MB / 约 7.1 MB |
| `AOX-Setup-<v>-with-python.exe` / `AOX-GUI-Setup-<v>-with-python.exe` | 加 `runtime/python` | 约 13 MB / 约 16 MB |
| `AOX-Setup-<v>-zh.exe` 及同族三个 | 中文界面 | 同上 |
| `aox-python-runtime.zip` | 运行时组件包 | 约 11 MB |

版本号以 `--define=AppVersion=…` 传给 ISCC，**不要用旧式的 `/DAppVersion=…`**：ISCC 把 `/` 开头当选项，
而 Git Bash 在启动原生程序前会把它改写成 Unix 路径，于是报
`You may not specify more than one script filename.`

## 故障排查

**任务跑到一半停下、模型什么也不说。** 先看有没有 `⚠` / `✗` 行——AOX 不会再静默结束。

1. **思考模型**返回 `reasoning_content`，回传历史时必须原样带回，否则多步任务的第二次请求会被 400 拒绝。
   AOX 已自动处理。
2. **`max_tokens` 太小**：推理 token 也占输出额度，额度耗尽就是一条空回复。8192 起步比较稳。
3. **抓原始报文**：`AOX_DEBUG_LLM=1 aox`，每条请求与响应追加到**当前工作目录**下的
   `logs/llm.jsonl`（含 `finish_reason`、`usage`、空回复）。

**GUI 里闪黑窗。** `aox-gui.exe` 是 `windows` 子系统、没有控制台；拉起 `python.exe` 时不给
`CREATE_NO_WINDOW`，Windows 就会给它分配一个。宿主在所有 spawn 路径上都加了，新构建不会再有；
`cargo run` 起来的 **debug 版本身是控制台程序**，看到黑窗是正常的。

```bash
python tools/check_no_console_window.py --ab     # 机制自检
python tools/check_no_console_window.py --app target/release/aox-gui.exe --cwd .
```

**`模型未就绪`。** `models.json` 里没有启用的模型，或 key 还是占位符。打开设置 → 模型；GUI 会告诉你
它读的是哪个文件。

**python 插件加载不了。** 没找到解释器。`/runtime` 会报告哪一档没命中，`/runtime install python` 就地解决。

## 目录说明

```
<程序目录>/
  config.toml                你的配置（首次运行生成）
  aox.toml                   可选的组合清单（bundle / patch / profile）
  bundles/<名字>.toml        可选，覆盖内置 bundle
  RULES.md                   可选，项目规则会注入系统提示
  aox.exe / aox-gui.exe
  .aox/
    models.json              模型列表与当前启用的模型
    spaces.json              空间注册表
    conversations/*.jsonl    一个会话一个文件
    backup/                  内容寻址的恢复库
    memory/*.md              agent 自己的按日笔记
    remote.json              远程配对
    ui.json / welcome.json / remote.lock
    tmp/  web/  files/  run/  toolcache/
  plugins/<名字>/            模型自己写的插件，另有 sdk/
  runtime/bash/              busybox-w32 兜底 shell
  runtime/python/            仅在 -with-python 或 /runtime install python 之后存在
  workplace/<项目>/          长期交付物
  temp/                      临时文件
  logs/llm.jsonl             仅在 AOX_DEBUG_LLM=1 时存在，落在当前工作目录
```

插件子进程以**程序目录**为工作目录启动；Python 插件由宿主强制 UTF-8 输出。

## 卸载

通过「设置 → 应用」或开始菜单卸载项。卸载会删掉 `runtime/`（机器相关、可随时重建）与装进去的
可执行文件，但**保留** `plugins/`（模型积累出来的能力）与 `.aox/`（会话、模型、快照、配对）——
两者都是用户数据。真要清干净，手动删除安装目录。
