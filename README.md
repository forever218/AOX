# AOX

一个**自我增长**的 AI 编程代理。它启动时**没有任何工具**，只有一个核心循环和三个元工具。
当任务需要某种能力（读写文件、跑命令、抓网页……）时，它会**自己编写一个插件并注入**，
下一轮立刻就能调用。插件会持久化在磁盘上，重启后自动恢复——所以它用着用着会越来越强。

---

## 快速开始

1. 双击 `aox.exe`（首次运行会在程序目录自动生成 `config.toml`）。
2. 用记事本打开 `config.toml`，填入你的模型服务信息：

   ```toml
   [provider]
   base_url = "https://api.deepseek.com/v1"   # 任何 OpenAI 兼容服务都行
   model    = "deepseek-chat"
   api_key  = "sk-你的key"                    # 或者改用 api_key_env 指向环境变量
   ```

3. 再次双击 `aox.exe`，进入交互界面。
4. 直接说人话，例如：

   > 读取 config.toml，告诉我里面有哪些配置项

   它发现没有文件读取能力，会先造一个 `fs` 插件，然后用它完成任务。

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

## 命令行

| 命令 | 作用 |
| --- | --- |
| `/help` | 显示帮助（列出全部命令） |
| `/tools` | 查看当前所有可用工具 |
| `/plugins` | 查看已加载的插件 |
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
在 `config.toml` 的 `[provider]` 里加一行 `stream = false` 即可。

任务完成后，中间的模型思考与工具调用会自动折叠成一行摘要，只显示最终输出：

```
▸ 已折叠 8 步工具调用 · 12.4s · /expand 展开

（这里是模型的最终输出）
```

输入 `/expand` 可随时展开最近一次的过程明细（每一步的调用参数与结果）。

动画在输出被重定向到文件/管道时自动静默；可用环境变量 `AOX_ANIM=1` 强制开启、`AOX_ANIM=0` 强制关闭。

## 图形界面（AOX GUI）

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
| 顶栏 | 品牌与实时状态（`思考中 3.2s` / `执行 fs_read`）、模型、工具数、插件数、右侧面板开关、**明暗切换** |
| 主区 | 对话流：用户气泡、助手 Markdown（标题/代码块/表格/引用/行内代码）、**自动折叠的过程块**（点击展开工具调用明细） |
| 右侧活动流 | 可折叠、可拖宽。实时滚动展示：每轮状态与耗时、**流式思考过程**、每一次工具/插件调用与结果、授权请求与结果、插件加载与生长 |
| 底部 | 多行输入（`Enter` 发送、`Shift+Enter` 换行）、发送 / 停止按钮 |

其它细节：

- 配色是**「纸与墨」的单色方案**，只有明度、没有色相：亮色是一张发暖的纸（`#F2EFE8`），
  暗色是暖调的黑（`#141412`）。点顶栏最右的按钮即可切换（亮色显示月牙、暗色显示太阳），
  原生标题栏会跟着一起变；偏好记在 `.aox-gui-theme`。
  成功/警告/失败不靠颜色区分，而用明度层次与反白块表达。
- 需要授权的元工具会弹出 Modal，快捷键与 CLI 完全一致：`y` 允许一次、`a` 本会话始终允许、`n` / `Esc` 拒绝。
- 任务执行中可点右下角的停止按钮中断，对话历史会回滚到任务前状态（与 CLI 的 `Ctrl+C` 等价）。
- 界面语言跟随系统：自动从 `C:\Windows\Fonts` 挑一个中文字体（等线 / 微软雅黑 / 黑体…），
  无需手动配置。可用 `AOX_GUI_FONT`、`AOX_GUI_FONT_BOLD`、`AOX_GUI_FONT_MONO` 指定字体文件。
- 鼠标往上翻历史时不会自动被拽回底部；滚回底部附近会自动恢复跟随。

仓库根目录的行为与加 GUI 之前**完全一致**；GUI 是独立 crate，删掉 `gui/` 与
`Cargo.toml` 里的 `[workspace] members` 即可移除。

## 故障排查

**任务跑到一半停下、模型什么也没输出？**

先看终端里有没有 `⚠` / `✗` 行——AOX 现在不会再「静默结束」了。若确有异常，按以下顺序排查：

1. **思考模式（thinking）模型**：`deepseek-flash`、`deepseek-reasoner` 这类模型会返回
   `reasoning_content`，回传历史时**必须原样带回**，否则每次多步任务的第二次请求都会被
   API 以 400 拒绝（`reasoning_content ... must be passed back to the API`）。
   AOX 已自动处理，无需你干预。
2. **`max_tokens` 太小**：推理 token 也占输出额度，额度耗尽会得到一条**空回复**。
   建议 `config.toml` 里设为 `8192` 以上。
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
| `plugins/<名字>/` | 模型自己写的插件，含源码与 `plugin.toml` |
| `plugins/sdk/` | 宿主提供的插件 SDK 与协议文档，每次启动自动更新 |
| `RULES.md` | 可选。写在这里的项目规则会被注入系统提示 |
| `logs/llm.jsonl` | 可选。仅在 `AOX_DEBUG_LLM=1` 启动时记录原始请求/响应，用于排查模型侧问题 |

插件子进程以**安装目录为工作目录**启动，Python 插件已由宿主强制 UTF-8 输出。

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

### 安装包的两个变体

`installer/*.iss` 是同一份脚本，靠条件编译产出两种包（不维护第二份脚本）：

| 产物 | 内容 | 体积 |
| --- | --- | --- |
| `AOX-Setup-<版本>.exe` | 只有宿主 | 约 3 MB |
| `AOX-Setup-<版本>-with-python.exe` | 宿主 + Python 运行时（离线可用） | 约 13.5 MB |

```bash
bash tools/build_installer.sh                    # 主包
bash tools/build_installer.sh all with-python    # 额外产出捆绑变体
bash tools/build_installer.sh pack               # 只产出运行时组件包
```

## 卸载

通过「设置 → 应用」或开始菜单里的卸载项即可。卸载会删除 `<安装目录>/runtime`
（按需装配或捆绑进来的运行时，机器相关、可随时重建），但**不会删除 `plugins/` 目录**，
如果你不再需要模型积累的插件，请手动删除安装目录。

---

ISC License · <https://2am.top>
