# AOX 使用手册

[← README](../README.zh-CN.md) · [文档索引](README.zh-CN.md) · [English](USAGE.md)

README 上放不下的都在这里：GUI 细节、对话怎么落盘、解释器从哪来、构建产出什么、出问题怎么查。

**目录** — [模型](#模型) · [命令行](#命令行) · [等待动画](#等待动画与过程折叠) ·
[图形界面](#图形界面) · [对话历史](#对话历史) · [系统要求](#系统要求) ·
[从源码构建](#从源码构建) · [故障排查](#故障排查) · [目录说明](#目录说明) · [卸载](#卸载)

## 模型

端点、key、模型名，以及上下文窗口 / 输出上限 / 温度 / 思考强度，全部存在
`<程序目录>/.aox/models.json` 里——那是**唯一**一份模型配置，命令行版 `aox.exe` 和 GUI 读的
都是它。GUI 的「设置 → 模型」直接改这个文件，**不写** `config.toml`；改完不用重启。

列表里可以存多个模型，其中一个是 `active`。命令行用 `/model <序号|名字>` 切换，GUI 用启用开关，
都在下一个请求生效。

「测试连接」只发一个 token 的探活；「扫描」只列出端点自报的模型——能列出模型不代表能聊天。

## 命令行

| 命令 | 作用 |
| --- | --- |
| `/help` | 显示帮助（列出全部命令） |
| `/tools` | 查看当前所有可用工具 |
| `/plugins` | 查看已加载的插件 |
| `/history` | 查看历史对话列表（`/history show <序号>` 看全文、`/history open <序号>` 接着聊、`/history dir` 看目录、`/history rm <序号>` 删除） |
| `/model` | 列出已保存的模型（和 GUI 的「设置 → 模型」是同一份）；`/model <序号\|名字>` 切换，下一个请求生效 |
| `/new` | 开一段新对话（当前这段已经落盘） |
| `/runtime` | 查看解释器解析结果；`/runtime install python` 自动装配内置运行时 |
| `/demo` | 预览终端排版与折叠效果 |
| `/expand` | 展开上一次被折叠的工具调用过程 |
| `/quit` | 退出（`/exit` 等价） |

按 `Ctrl+C` 可中断正在进行的任务。

启动横幅里的项目地址是可点击超链接（OSC 8）。Windows Terminal、VS Code 终端等会自动启用；
传统 conhost（例如直接双击 exe）不认这个序列，会自动退回纯文本 URL。
自动探测不灵时可用 `AOX_HYPERLINK=1` 强制开启、`AOX_HYPERLINK=0` 关闭。

## 等待动画与过程折叠

任务进行中，终端只保留**一行**状态（执行工具时会自动切换提示文字）：

```
⠹ 思考中 3.2s
⠼ 执行 fs_read 8.7s
```

如果模型是**思考模式**（`deepseek-flash` / `deepseek-reasoner` 等），等待期间它还会把推理过程
实时滚动显示在状态行下方——看到它「在想什么」，就知道任务跑到哪一步了：

```
⠹ 思考中 3.2s
  我需要先确认这个目录下有哪些文件，
  然后判断是否要新建一个插件来读取配置。
```

这段推理过程是**即时出现、响应结束即消失**的临时回显（滚动窗口保留末尾 4 行），
不会污染输出，也不会进入折叠区。想关掉流式（例如端点不支持 `stream` 参数），
在 `.aox/models.json` 里给那条模型记录加 `"stream": false`，或在 GUI 的模型编辑弹窗里关掉「流式输出」。

任务完成后，中间的模型思考与工具调用会自动折叠成一行摘要，只显示最终输出：

```
▸ 已折叠 8 步工具调用 · 12.4s · /expand 展开

（这里是模型的最终输出）
```

输入 `/expand` 可随时展开最近一次的过程明细（每一步的调用参数与结果）。

动画在输出被重定向到文件/管道时自动静默；可用环境变量 `AOX_ANIM=1` 强制开启、`AOX_ANIM=0` 强制关闭。

## 图形界面

除了命令行版本，仓库里还有一套**独立的图形界面**：`gui/`（crate 名 `aox-gui`）。

它和 CLI **共用同一套核心**——`src/lib.rs` 把 Agent 主循环、LLM Provider、插件宿主、`Ui`
抽象层暴露成库，GUI 只是 `aox::ui::Ui` 的另一个实现，没有任何逻辑分叉。CLI 的行为不受影响
（`cargo build` 仍然只构建 CLI，见 `default-members`）。

```bash
cargo run -p aox-gui                      # 直接运行（与 CLI 共用 config.toml）
cargo run -p aox-gui -- --demo            # 演示模式：不连模型，预览界面与事件流
cargo run -p aox-gui -- --prompt "看看现在有哪些插件"   # 启动后自动发第一条指令
cargo build -p aox-gui --release          # 产出 target/release/aox-gui.exe
```

界面结构：

| 区域 | 内容 |
| --- | --- |
| 顶栏 | 最左两枚图标：**历史侧边栏开关**、**新建对话**；右上角三枚（自左向右）关于、活动流开关、**明暗切换**。只有任务执行中才在左侧显示一行耗时 |
| 左侧历史栏 | 默认折叠，点顶栏最左那枚图标展开（见 [对话历史](#对话历史)）。点某一条就**切换到那段对话**接着聊 |
| 主区 | 对话流：用户气泡、助手 Markdown（标题带细线、**行内代码是带底色的芯片**、**表格有完整外框 + 表头色带 + 行分隔线**、**围栏代码块带语法高亮**）、**自动折叠的过程块**（点击展开工具调用明细） |
| 活动流 | **不在主窗口里**：点顶栏那枚图标会打开一个独立窗口，初始尺寸与主窗口一致（用系统标题栏，可拖动、缩放、最小化，Win11 自带圆角）。主体是一条**横向时间轴**：横轴是时间，任务自左向右推进，色块宽度 = 真实耗时；工具调用 / 思考 / 授权 / 消息分三行各用一种颜色，同一插件的色块用同一族色分深浅。**点色块（或下方列表里的一行）看详情**。滚轮平移、Ctrl+滚轮缩放 |
| 底部 | 多行输入（`Enter` 发送、`Shift+Enter` 换行）、发送 / 停止按钮 |

其它细节：

- 配色是**「纸与墨」的单色方案**，只有明度、没有色相：亮色是一张发暖的纸（`#F2EFE8`），
  暗色是暖调的黑（`#141412`）。点顶栏最右的按钮即可切换（亮色显示月牙、暗色显示太阳），
  原生标题栏的底色与文字色也跟着一起变（DWM 属性）；偏好记在 `.aox-gui-theme`。
  成功/警告/失败不靠颜色区分，而用明度层次与反白块表达。
  **唯一的例外是代码块**：语法高亮是低饱和的暖色系（赭红关键字、松绿类型、钢蓝函数、
  橄榄字符串、梅紫数字、暖灰注释、琥珀元信息），只跟明暗两套、随主题一起切换。
- 语法高亮覆盖 rust / python / js·ts / c 系 / shell / sql / json·toml·yaml / html·xml，
  认不出来的语言按「只高亮字符串与数字」处理，不会乱标。识别靠两条通用启发式：
  **紧跟 `(` 的标识符当函数、PascalCase 当类型**，因此不必为每种语言维护类型表。
  围栏代码块的信息串支持 `rust,ignore` 这类后缀，只取第一段。
- 原生标题栏上不缀模型名（标题固定是 `AOX`），也不再单独设窗口图标：左上角只剩标题本身，
  品牌交给初始界面承担，模型名与运行时分发状态都在「关于」窗口里。注意 Windows 的标题栏图标
  与任务栏图标共用同一枚，不设它之后任务栏上也看不到这个图标。
- 初始界面是一句问候加三条示例指令，点一下会填进输入框（不会直接发送）。
- 需要授权的元工具会弹出 Modal，快捷键与 CLI 完全一致：`y` 允许一次、`a` 本会话始终允许、`n` / `Esc` 拒绝。
- 任务执行中可点右下角的停止按钮中断，对话历史会回滚到任务前状态（与 CLI 的 `Ctrl+C` 等价）；
  内存里的消息和已落盘的 `history/*.jsonl` 一起回滚，不会留下半截记录。
- 界面语言跟随系统：自动从 `C:\Windows\Fonts` 挑一个中文字体（等线 / 微软雅黑 / 黑体…），
  无需手动配置。初始界面那句问候另挑一个**带笔势的展示体**（Gabriola → Ink Free → …），
  找不到就逐级退回衬线、粗体、正文字体，永远不会变成方块。
  可用 `AOX_GUI_FONT`、`AOX_GUI_FONT_BOLD`、`AOX_GUI_FONT_SERIF`、`AOX_GUI_FONT_BRAND`、
  `AOX_GUI_FONT_MONO` 指定字体文件。
- 鼠标往上翻历史时不会自动被拽回底部；滚回底部附近会自动恢复跟随。

仓库根目录的行为与加 GUI 之前**完全一致**；GUI 是独立 crate，删掉 `gui/` 与
`Cargo.toml` 里的 `[workspace] members` 即可移除。

## 对话历史

对话**自动落盘**，命令行和 GUI 走的是同一套实现（`Agent` 里的唯一写入点），
关掉窗口/终端也不会丢。

存在 `<应用根>/history/`，**每个对话一个文件**：

```
history/
  20261002-155959-d81c.jsonl
  20261002-160001-e98c.jsonl
```

> 两个安装包默认装到不同目录（`%LOCALAPPDATA%\Programs\AOX` 与 `…\AOX-GUI`），
> 因此各自有一份 `history/`。想让它们共用，把其中一个装到另一个的目录即可。

文件名主干就是这次对话的 id（`年月日-时分秒-4位随机`），也是命令行 `show` / `rm` 认的键。

### 文件格式

JSONL——一行一个 JSON 对象，第一行是元信息，之后每行是一条消息：

```json
{"created":1790927628,"id":"20261002-155348-c5d8","kind":"meta","model":"deepseek-chat","root":"D:\\AOX","version":"0.1.0"}
{"content":"你好，先看看目录","kind":"message","role":"user"}
{"content":"我先列一下目录。","kind":"message","role":"assistant","tool_calls":[...]}
{"content":"[FILE] ...","is_error":false,"kind":"message","name":"fs_ls","role":"tool","tool_call_id":"call_1"}
```

消息行就是 **OpenAI 请求体里的原始消息对象**，额外只多一个 `is_error` 字段；
所以这个文件既是给人看的日志，也能直接当请求体重放。系统提示不落盘（每次启动都由代码重组，
存下来只会是一份随时会过期的副本）。`kind` 用来区分行类型，读回时会被剥掉。

几个刻意的取舍：

- **追加写**，不整体重写。中途断电/杀进程最多丢最后一行，前面都还在。
- **一行坏了不影响整个文件**：读取时跳过解析失败的行，而不是让整个对话打不开。
- **中断会回滚**：`Ctrl+C` / 停止按钮回滚半截任务时，文件里那半截也一并删掉——
  否则会留下「有 `tool_calls` 但没有结果」的残缺记录，重放时会被服务端直接拒绝。

### 命令行

```bash
/history                  # 列表：序号、时间、条数、首条用户输入当标题
/history show 1           # 看第 1 条的全文（你 / 助手 / 工具调用与结果）
/history open 1           # 切到第 1 条接着聊（旧对话已落盘，不需要「保存」）
/history dir              # 打印目录路径
/history rm 2             # 删掉第 2 条
```

列表里本次会话那一条会标 `← 当前`。`show` / `open` / `rm` 也接受 id 或 id 前缀，不必去数序号。

### GUI

左上角两枚图标：**列表**开关左侧边栏，**加号**新开一段对话。

- 侧边栏列出全部历史，按**最后一次说话的时间**从新到旧排；右侧一个小圆点标出
  **眼下正在聊的**那条（它永远置顶——刚切进去的老对话也排得到最上面）。
- 点任意一条即**切换到那段对话**：界面上那段的正文立刻换上，接着往下聊是接着它的上下文；
  想另起一段就点加号，原来那段已经完整躺在 `history/` 里了。
- 两侧面板默认都是收起的，窗口打开就是干净的对话区。
- 当前对话落盘后侧边栏会自动刷新，一般用不着手动点「刷新」；那个按钮是给
  「另开了窗口 / 在别处改动了 `history/`」这类情况兜底的。

### 保留策略

`config.toml` 的 `[history]`：

```toml
[history]
enabled = true   # 关掉后不再写新文件；已有历史仍可查看
keep = 200       # 只保留最近 N 个对话，0 = 不限制
```

清理是**顺带的**：只在新建对话文件时数一下目录、删掉超出的旧文件，没有后台线程、
没有定时器、也不会在启动时全盘扫描。

## 系统要求

平台：Windows 10 / 11（x64）。

宿主本体（`aox.exe` / `aox-gui.exe`）**不需要任何外部运行时**：发行版已静态链接 MSVC 运行库，
既不要求 VC++ Redistributable（导入表里没有 `VCRUNTIME140.dll`），也不需要 Python / Node / Rust。

外部运行时只影响**插件**，而且三种语言的要求不一样：

| 插件语言 | 运行已有插件 | 新建插件（`create_plugin`） |
| --- | --- | --- |
| python | 需要能用的 `python`（3.8+） | 同左 |
| node | 需要能用的 `node` | 同左 |
| rust | **不需要任何环境** | 需要 `rustc`（编译完就不再需要） |

也就是说，`plugins/` 里已有的插件只有 python / node 两类要求解释器在场；rust 插件是编译好的原生 exe，
宿主直接运行它，没装 Rust 工具链也能用。

### 解释器从哪来：三档解析

宿主启动时对每个语言（python / node / rustc）按下面的顺序解析一次，**取第一个命中的**：

1. `config.toml` 的 `[runtimes]` 里写了**完整路径** → 直接用它，不做任何替换；
2. 该字段是**裸命令**（默认值就是 `python` / `node` / `rustc`）且应用目录下存在内置运行时
   `<应用根>/runtime/<语言>/<可执行文件>` → 用内置的那份；
3. 仍然没有 → 用裸命令交给 `PATH`。

第 2 档是重点：**把运行时放进应用目录就自动生效**，不用改配置，也不会往系统 `PATH`
或用户环境变量里写任何东西（卸载即消失）。

### 装配 Python 运行时

没有任何解释器时程序照常启动、能对话、能用 `/tools` `/plugins` 等本地命令，
但 python 插件会加载失败——此时启动横幅会明确提示一行。补上它的方式：

```
/runtime                      # 查看三种运行时的状态与来源（含版本号）
/runtime install python       # 装配 Python 运行时到 <应用根>/runtime/python
/runtime dir                  # 打印运行时目录
```

`install` 的顺序是：**已存在 → 本地离线包 → 下载**。

- **离线包**：文件名固定为 `aox-python-runtime.zip`，放在 **exe 同目录**、应用根、
  `<应用根>/runtime` 或 `%LOCALAPPDATA%\AOX` 下都会被自动发现。用
  `bash tools/build_installer.sh pack` 可以产出这个包（约 10 MB），适合内网 / 离线分发。
- **下载**：默认取 CPython 官方嵌入式发行版（约 10 MB），并用内置的官方 SHA-256 校验。
  可换成自建镜像：

  ```toml
  [runtimes]
  python_url    = "https://your-mirror.example.com/python-3.13.7-embed-amd64.zip"
  python_sha256 = "<官方或自建包的 SHA-256>"
  ```

- 装配完成后**立即生效**（内部会重新解析一次）：之后新建的 python 插件直接用它；
  此前因缺少 python 而加载失败的插件，重启程序即可恢复。
- 整个过程只用系统已有能力：下载走 `reqwest`，解压走系统 `tar.exe`（Win10 1803+），
  校验走 `certutil`。宿主**没有为此增加任何第三方依赖**。
- Node 单文件就超过 80 MB，不适合自动装配。`/runtime install node` 会直接说明这一点，
  请自行安装并保证 `node` 在 `PATH` 中。
- 个别插件的**第三方 pip 依赖**要自己装。注意官方「嵌入式发行版」**不含 `pip`**：
  确实需要 `pip` 时请装完整 Python，并在 `[runtimes] python` 指向它。

### 发行产物（一次构建全部产出）

`installer/*.iss` 只有一份脚本，靠条件编译产出两种变体（不维护第二份脚本）。
`bash tools/build_installer.sh` **不带参数时一次产出下面全部五种文件**：

| 产物 | 内容 | 体积 |
| --- | --- | --- |
| `AOX-Setup-<版本>.exe` | 命令行版，只有宿主 | 约 3 MB |
| `AOX-GUI-Setup-<版本>.exe` | 图形界面版，只有宿主 | 约 4 MB |
| `AOX-Setup-<版本>-with-python.exe` | 命令行版 + Python 运行时（离线可用） | 约 11.4 MB |
| `AOX-GUI-Setup-<版本>-with-python.exe` | 图形界面版 + Python 运行时（离线可用） | 约 12.5 MB |
| `aox-python-runtime.zip` | 运行时组件包，给主包用户离线装配用 | 约 10.4 MB |

```bash
bash tools/build_installer.sh                    # 默认：上面全部
bash tools/build_installer.sh all none           # 只要两个主包（最快）
bash tools/build_installer.sh all with-python    # 只要两个捆绑变体
bash tools/build_installer.sh cli                # 只做命令行版（两种变体）
bash tools/build_installer.sh pack               # 只产出运行时组件包
```

脚本把版本号以 `--define=AppVersion=…` 传给 ISCC，**不要改成旧式的 `/DAppVersion=…`**：
ISCC 把 `/` 开头的参数当选项，而 Git Bash（MSYS）在启动原生程序前会把这类参数当 Unix
路径改写一遍（`/DAppVersion=0.1.0` → `<Git 安装目录>/DAppVersion=0.1.0`），ISCC 认不出它是
选项，就报 `You may not specify more than one script filename.`。`--define=` 不以 `/` 开头，
不受这条规则影响；手动编译时同理，见 `installer/*.iss` 头部注释。

两个 `-with-python` 变体与主包的**差别只在于是否把 `runtime/python` 一并装进应用目录**，
宿主会自动发现它（三档解析的第 2 档）。

## 从源码构建

需要 Rust 1.90+ 与 MSVC 工具链。

```bash
cargo build                      # 只构建 CLI（workspace 默认成员）
cargo run                        # 运行 CLI
cargo build -p aox-gui --release # 构建 GUI
cargo run -p aox-gui             # 运行 GUI
```

**Windows + Git Bash 用户注意**：Git Bash 自带的 coreutils `link` 会抢占 PATH 里 MSVC 的
`link.exe`，报 `link: extra operand '...rcgu.o'`。解决办法是在 `.cargo/config.toml` 里
显式指定链接器与 `LIB` / `INCLUDE`：

```toml
[target.x86_64-pc-windows-msvc]
linker = "C:\\Program Files\\Microsoft Visual Studio\\2022\\Community\\VC\\Tools\\MSVC\\<版本>\\bin\\Hostx64\\x64\\link.exe"

# 静态链接 MSVC 运行库（/MT 等价），**发行必加**：
# 否则 exe 会依赖 VCRUNTIME140.dll，而它不属于 Windows 自带组件
# （来自 VC++ 2015-2022 Redistributable），干净装机上直接报「找不到 VCRUNTIME140.dll」。
rustflags = ["-C", "target-feature=+crt-static"]

[env]
LIB     = { value = "<MSVC>\\lib\\x64;<WinSDK>\\Lib\\<版本>\\um\\x64;<WinSDK>\\Lib\\<版本>\\ucrt\\x64", force = true }
INCLUDE = { value = "<MSVC>\\include;<WinSDK>\\Include\\<版本>\\um;<WinSDK>\\Include\\<版本>\\ucrt;<WinSDK>\\Include\\<版本>\\shared;<WinSDK>\\Include\\<版本>\\winrt", force = true }
```

这个文件含本机绝对路径（对别人无效），已被 `.gitignore` 排除，**需要自建**。
用 PowerShell / cmd 或已 `vcvars64` 的终端构建则不需要其中的 `linker` / `LIB` / `INCLUDE`，
但 **`rustflags` 那行无论用哪个终端都要保留**。

漏加 `rustflags` 时构建不会失败，但两个 `build.rs` 会打出显眼的 `cargo:warning` 提醒你产物
在干净机器上起不来。

## 故障排查

**任务跑到一半停下、模型什么也没输出？**

先看终端里有没有 `⚠` / `✗` 行——AOX 现在不会再「静默结束」了。若确有异常，按以下顺序排查：

1. **思考模式（thinking）模型**：`deepseek-flash`、`deepseek-reasoner` 这类模型会返回
   `reasoning_content`，回传历史时**必须原样带回**，否则每次多步任务的第二次请求都会被
   API 以 400 拒绝（`reasoning_content ... must be passed back to the API`）。
   AOX 已自动处理，无需你干预。
2. **`max_tokens` 太小**：推理 token 也占输出额度，额度耗尽会得到一条**空回复**。
   建议在模型参数里设为 `8192` 以上。
3. **抓原始报文**：用 `AOX_DEBUG_LLM=1 aox` 启动，每次请求与响应会追加到
   `logs/llm.jsonl`，可直接看到 `finish_reason`、`usage`、空回复等细节。

AOX 对「空回复」的处置是：先重试 2 次并提示，仍为空则明确报错说明原因，**不会**把
一句「我先看看……」的前言当成最终答案。

**图形界面里「一用插件 / 插件初始化就弹黑色 python 窗口」？**

GUI 版编译为 `windows` 子系统，**自身没有控制台**。如果拉起插件子进程时不加
`CREATE_NO_WINDOW`，Windows 会为 `python.exe` 新分配一块**带窗口**的控制台 —— 现象就是
动不动弹出几个黑窗。宿主已修（`src/plugin/mod.rs::hide_console`，覆盖首次启动恢复插件、
插件崩溃自愈重启、`rustc` 编译三条路径），装上最新构建的 `AOX-GUI-Setup` 即可。
留了回归检查：

```bash
python tools/check_no_console_window.py --ab                                  # 机制自检，秒级
python tools/check_no_console_window.py --app target/release/aox-gui.exe --cwd .
```

注意 `cargo run` 起来的 **debug 版本身就是控制台程序**，看到一个黑窗属于正常现象，
不是这个 bug。

## 目录说明

程序目录（安装目录）下会自动生成：

| 路径 | 说明 |
| --- | --- |
| `config.toml` | 你的配置（首次运行自动生成） |
| `.aox/models.json` | 模型列表与当前启用的模型（见 [模型](#模型)） |
| `plugins/<名字>/` | 模型自己写的插件，含源码与 `plugin.toml` |
| `plugins/sdk/` | 宿主提供的插件 SDK 与协议文档，每次启动自动更新 |
| `history/*.jsonl` | 对话历史，每个对话一个文件（见 [对话历史](#对话历史)） |
| `RULES.md` | 可选。写在这里的项目规则会被注入系统提示 |
| `logs/llm.jsonl` | 可选。仅在 `AOX_DEBUG_LLM=1` 启动时记录原始请求/响应，用于排查模型侧问题 |

插件子进程以**安装目录为工作目录**启动，Python 插件已由宿主强制 UTF-8 输出。

## 卸载

通过「设置 → 应用」或开始菜单里的卸载项即可。卸载会删除 `<安装目录>/runtime`
（按需装配或捆绑进来的运行时，机器相关、可随时重建），但**不会删除 `plugins/` 与 `history/`**——
前者是模型积累出来的插件，后者是你的对话记录，两者都是用户数据。
如果你确实不再需要，请手动删除安装目录。
