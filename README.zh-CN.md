<div align="center">

<img src="assets/aox-icon.png" width="112" alt="AOX">

# AOX

**可自我扩展的 AI 代理。**<br>
内置基础工具集，缺失的能力由代理在运行时以 Python、Node.js 或 Rust 插件的形式自行编写。

[![stars][b-stars]][repo] [![rust][b-rust]][repo] [![platform][b-platform]][repo] [![gui][b-gui]][gui] [![plugins][b-plugins]][usage] [![license][b-license]][license]

[快速开始](#快速开始) · [常用命令](#常用命令) · [从源码构建](#从源码构建) · [文档](#文档) · [**English**](README.md)

</div>

## 功能特性

- **自我扩展。** 遇到缺失的能力时，代理会自行编写插件并加载，下一轮即可调用。插件以文件形式保存在磁盘上，重启后自动恢复；`update_plugin` 支持原地热替换。
- **内置工具集。** 提供 18 个宿主工具，包括文件操作（`read_file`、`write_file`、`edit_file`、`remove` 等）、`shell` / `shell_job`、`web_fetch`、`ask_user`、`find_tools`、`task_state` 和 `verify`。检测到 POSIX shell 时，另提供 `bash`。
- **任务级回滚。** `task_state` 记录任务目标、计划与验收标准。每个任务生成一份内容寻址快照，支持预览、回滚与重做。
- **明确的权限边界。** 工作区分级（默认仅允许写入程序目录）、插件子进程的 Job Object 围栏，以及四道授权关卡。
- **CLI 与 GUI 共用内核。** 两种界面基于同一个 `Agent`。GUI 额外提供活动流时间轴、缓存命中图、工作区文件视图和上下文占用环。
- **远程接入。** `aox remote` 将本机注册为 AOXChat 中的 agent，在聊天室中 @ 它即可在本机运行代理。

## 快速开始

**1. 安装。** 运行 `AOX-GUI-Setup-<版本>.exe`（Windows 10 1809+，x64；中文版安装包文件名带 `-zh` 后缀）。首次启动时会自动打开 **设置 → 模型**。

**2. 配置模型。** 支持任意 OpenAI 兼容端点。点击「扫描模型」可获取可用的模型名称。

```text
端点    https://api.deepseek.com
Key     sk-...
模型    deepseek-flash
```

凭据仅保存在 `.aox/models.json` 中，CLI 与 GUI 共用该文件。切换模型后，下一次请求即生效。

**3. 开始任务。** 默认工作区为程序目录，写入与删除操作均限制在该目录内；可通过输入框下方的 `Workspace` 切换分级。示例提示词：「读取 config.example.toml，并列出其中的各个分节。」

> **无运行时依赖。** 宿主程序为静态链接，无需 VC++ 运行库、Python 或 Node.js。
> `bash` 以 busybox-w32 形式随包提供；Python 为可选组件，可通过 `/runtime install python` 或 `-with-python` 安装包获得。

## 常用命令

| 命令 | 说明 |
| --- | --- |
| `/help` | 显示全部命令 |
| `/tools` · `/plugins` | 列出当前可用工具，以及各插件提供的工具 |
| `/model` | 列出已保存的模型；`/model 2` 切换到第 2 个模型，下一次请求生效 |
| `/workplace` | 查看或切换写入边界：`full` · `default` · `<目录>` |
| `/task` | 查看当前检查点：目标、计划进度与验收标准 |
| `/rollback` | 预览上一个任务的文件变更；`--yes` 执行回滚，`/redo` 撤销回滚 |
| `/usage` | 查看本次对话的 token 明细与当前上下文大小 |
| `/history` | 管理历史对话：`show 1` · `open 1` · `rm 1` · `dir` |
| `/remote` | 管理远程桥接：`status` · `start` · `stop` · `pair …` |
| `/new` · `/quit` | 新建对话 · 退出（`Ctrl+C` 可中断正在运行的任务） |

其余命令（`/prompt`、`/hook`、`/loop`、`/config`、`/sandbox`、`/shell`、`/web`、`/checkpoints`、`/backup`、`/attach`、`/demo`、`/expand`、`/runtime`）详见[使用手册](docs/USAGE.zh-CN.md)。

## 从源码构建

需要 Rust 1.90+ 及 MSVC 工具链。

```bash
cargo build -p aox-gui --release                # GUI → target/release/aox-gui.exe
cargo build                                     # CLI → target/release/aox.exe
AOX_LANG=zh cargo build -p aox-gui --release    # 中文界面（语言在编译期确定）
```

在 Windows + Git Bash 环境下，需在 `.cargo/config.toml` 中显式指定链接器与 `crt-static`，详见 [使用手册 § 从源码构建](docs/USAGE.zh-CN.md#从源码构建)。

## 文档

| 文档 | 内容 |
| --- | --- |
| [**使用手册**](docs/USAGE.zh-CN.md) | 安装、模型、命令、GUI 窗口、会话存储、配置分层、权限边界、回滚、远程接入、打包 |
| [**ARCH-ROADMAP**](docs/ARCH-ROADMAP.zh-CN.md) | 设计原则、开发阶段 1–7、历次构建的体积记录 |
| [**SANDBOX**](docs/SANDBOX.zh-CN.md) | 插件子进程所受的实际限制 |
| [**SHELL**](docs/SHELL.md) · [**WEB**](docs/WEB.zh-CN.md) | 内置命令行与 `web_fetch` |
| [**ROLLBACK**](docs/ROLLBACK.zh-CN.md) | 任务级检查点：语义与不变量 |
| [**I18N**](docs/I18N.zh-CN.md) | 本地化模型：每个发行版对应一种语言 |
| [**docs/README**](docs/README.zh-CN.md) | 完整索引：设计记录、图表、工作笔记 |

多数文档提供 `*.zh-CN.md` 中文副本，以英文版为准。

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