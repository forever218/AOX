<div align="center">

<img src="assets/aox-icon.png" width="112" alt="AOX">

# AOX

**一个会自己造工具的 AI 代理。**<br>
自带一套能用的工具——缺什么，用 Python / Node / Rust 现场写一个。

[![stars][b-stars]][repo] [![rust][b-rust]][repo] [![platform][b-platform]][repo] [![gui][b-gui]][gui] [![plugins][b-plugins]][usage] [![license][b-license]][license]

[快速开始](#快速开始) · [常用命令](#常用命令) · [从源码构建](#从源码构建) · [文档](#文档) · [**English**](README.md)

</div>

- **缺什么自己写。** 需要新能力？它写一个插件、注入、下一轮直接调用。插件就是磁盘上的文件，重启自动恢复；`update_plugin` 原地热替换。
- **开箱就能干活。** 18 个宿主工具：文件（`read_file` / `write_file` / `edit_file` / `remove`…）、`shell` · `shell_job`、`web_fetch`，以及 `ask_user`、`find_tools`、`task_state`、`verify`；找到 POSIX shell 时再加一个 `bash`。
- **每一步都可回滚。** `task_state` 记着目标、计划与验收；每个任务都留下内容寻址快照，可预览、可回滚、可重做。
- **边界看得见。** 工作区三档（默认只允许写程序目录）、插件子进程 Job 围栏、四道授权闸门。
- **一套内核，两个外壳。** CLI 与 GUI 共用同一个 `Agent`；GUI 另有活动流时间轴、缓存命中图、工作区文件视图与上下文环。
- **手机也能接管。** `aox remote` 把本机注册成 AOXChat 上的 agent——在群里 @ 它，真身在你机器上跑。

## 快速开始

**① 安装运行。** `AOX-GUI-Setup-<版本>.exe`（Windows 10 1809+ / x64；中文包文件名带 `-zh`）。首次启动自动跳到 **设置 → 模型**。

**② 添加模型。** 任何 OpenAI 兼容端点都行，点「扫描模型」可自动填名字。

```text
端点    https://api.deepseek.com
Key     sk-...
模型    deepseek-flash
```

存在 `.aox/models.json`——key 只落在这里。CLI 与 GUI 读的是同一份，切换后下一个请求生效。

**③ 直接说人话。** 默认工作区就是程序目录，写与删都落在它下面（输入框下方的 `Workspace` 可换档）。试试「读一下 config.example.toml，把它的分节列出来」。

> **不需要任何运行时。** 宿主静态链接，不依赖 VC++ 运行库，也不需要 Python / Node。
> `bash` 随包带 busybox-w32；Python 可选（`/runtime install python` 或直接装 `-with-python` 包）。

## 常用命令

| 命令 | 作用 |
| --- | --- |
| `/help` | 全部命令（含下面这些） |
| `/tools` · `/plugins` | 现在能调用什么，每个插件贡献了什么 |
| `/model` | 列出已保存的模型；`/model 2` 切换，下一个请求生效 |
| `/workplace` | 查看或切换写入边界——`full` · `default` · `<目录>` |
| `/task` | 当前任务检查点：目标、打勾的计划、验收标准 |
| `/rollback` | 预览上一个任务改了哪些文件；`--yes` 恢复，`/redo` 撤销回滚 |
| `/usage` | 本轮对话的 token 明细与当前上下文占用 |
| `/history` | 历史对话——`show 1` · `open 1` · `rm 1` · `dir` |
| `/remote` | 手机接管：`status` · `start` · `stop` · `pair …` |
| `/new` · `/quit` | 开新对话 · 退出（`Ctrl+C` 中断正在跑的任务） |

`/prompt`、`/hook`、`/loop`、`/config`、`/sandbox`、`/shell`、`/web`、`/checkpoints`、`/backup`、`/attach`、`/demo`、`/expand`、`/runtime` 见使用手册。

## 从源码构建

```bash
cargo build -p aox-gui --release                # GUI → target/release/aox-gui.exe
cargo build                                     # CLI → target/release/aox.exe
AOX_LANG=zh cargo build -p aox-gui --release    # 中文界面——语言是编译期定死的
```

需要 Rust 1.90+ 与 MSVC 工具链。Windows + Git Bash 下要在 `.cargo/config.toml` 里显式指定
链接器与 `crt-static`——见 [使用手册 § 从源码构建](docs/USAGE.zh-CN.md#从源码构建)。

## 文档

| 文档 | 内容 |
| --- | --- |
| [**使用手册**](docs/USAGE.zh-CN.md) | 安装、模型、全部命令、GUI 各窗口、会话存储、配置分层、边界、回滚、远程、打包 |
| [**ARCH-ROADMAP**](docs/ARCH-ROADMAP.zh-CN.md) | 七条设计原则、阶段 1–7、每次构建的体积全史 |
| [**SANDBOX**](docs/SANDBOX.zh-CN.md) | 插件子进程现在实际被挡住了什么 |
| [**SHELL**](docs/SHELL.md) · [**WEB**](docs/WEB.zh-CN.md) | 内建命令行与 `web_fetch` |
| [**ROLLBACK**](docs/ROLLBACK.zh-CN.md) | 任务级检查点：语义与不变量 |
| [**I18N**](docs/I18N.zh-CN.md) | 一种语言一个发行版 |
| [**docs/README**](docs/README.zh-CN.md) | 完整索引——设计记录、图表、工作笔记 |

多数文档旁边有一份 `*.zh-CN.md` 副本，英文是源语言。

---

ISC License · [2am.top](https://2am.top) · [Issues](https://github.com/forever218/AOX/issues)

[repo]: https://github.com/forever218/AOX
[gui]: gui/README.zh-CN.md
[usage]: docs/USAGE.zh-CN.md
[license]: LICENSE
[b-stars]: https://img.shields.io/github/stars/forever218/AOX?style=flat-square&label=stars&labelColor=141412&color=A5644D
[b-rust]: https://img.shields.io/badge/rust-1.90%2B-F2EEE8?style=flat-square&logo=rust&logoColor=A5644D&labelColor=141412
[b-platform]: https://img.shields.io/badge/platform-Windows%2010%20%7C%2011%20x64-2B322C?style=flat-square&logo=windows&logoColor=white&labelColor=141412
[b-gui]: https://img.shields.io/badge/gui-egui%200.33%20%2B%20wgpu-6E6A5E?style=flat-square&labelColor=141412
[b-plugins]: https://img.shields.io/badge/plugins-python%20node%20rust-8C8578?style=flat-square&labelColor=141412
[b-license]: https://img.shields.io/badge/license-ISC-F2EEE8?style=flat-square&labelColor=141412
