<div align="center">

<img src="assets/aox-icon.png" width="112" alt="AOX">

# AOX

**简洁 AI 代理**<br>
需要时，用**任意**编程语言自行编写插件

[![stars][b-stars]][repo] [![rust][b-rust]][repo] [![platform][b-platform]][repo] [![gui][b-gui]][gui] [![plugins][b-plugins]][usage] [![license][b-license]][license]

[快速开始](#快速开始) · [常用命令](#常用命令) · [从源码构建](#从源码构建) · [文档](#文档) · [**English**](README.md)

</div>

## 特性
- **原生rust**： 宿主0运行时依赖，体积小，启动迅速。性能指标：**安装包7MB，安装大小12MB（不含python），运行时内存100-200MB（GUI）**
- **自我扩展**： 需要时，代理会自行编写插件并加载，插件以文件形式保存在磁盘上，支持原地热替换
- **插件语言**： AOX插件不限语言，可以用电脑上任意编程语言编写
- **任务级回滚**：自带回滚系统，寻址快照，支持预览、回滚与重做
- **远程接入与协作** `aox remote` 将本机注册为 AOXChat 中的 agent，在聊天室中 @ 它即可在本机运行代理，产物轻松共享，与他人协作

## 快速开始

**1. 安装** 运行 `AOX-GUI-Setup-<版本>.exe`（Windows 10 1809+，x64）

**2. 配置模型** 支持任意 OpenAI 兼容端点

```text
端点    https://api.deepseek.com
Key     sk-...
模型    deepseek-flash
```

凭据仅保存在 `.aox/models.json` 中

**3. 开始任务** 默认工作区为程序目录，写入与删除操作均限制在该目录内

> `bash` 以 busybox-w32 形式随包提供；Python 为可选组件，可通过 `/runtime install python` 或 `-with-python` 安装包获得


## 从源码构建

需要 Rust 1.90+ 及 MSVC 工具链

```bash
cargo build -p aox-gui --release                # GUI → target/release/aox-gui.exe
cargo build                                     # CLI → target/release/aox.exe
AOX_LANG=zh cargo build -p aox-gui --release    # 中文界面（语言在编译期确定）
```

在 Windows + Git Bash 环境下，需在 `.cargo/config.toml` 中显式指定链接器与 `crt-static`，详见 [使用手册 § 从源码构建](docs/USAGE.zh-CN.md#从源码构建)

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

多数文档提供 `*.zh-CN.md` 中文副本，以英文版为准

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