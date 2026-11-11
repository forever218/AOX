<div align="center">

<img src="assets/aox-icon.png" width="112" alt="AOX">

# AOX

**A self-extending AI agent.**<br>
Ships with a built-in toolset and authors additional tools at runtime as Python, Node.js or Rust plugins.

[![stars][b-stars]][repo] [![rust][b-rust]][repo] [![platform][b-platform]][repo] [![gui][b-gui]][gui] [![plugins][b-plugins]][usage] [![license][b-license]][license]

[Quick start](#quick-start) · [Commands](#commands) · [Build](#build-from-source) · [Documentation](#documentation) · [**简体中文**](README.zh-CN.md)

</div>

## Features

- **Self-extending.** When a required capability is missing, the agent writes a plugin, loads it, and can call it on the next turn. Plugins persist on disk and are restored after a restart; `update_plugin` hot-swaps a plugin in place.
- **Built-in toolset.** 18 host tools, including file operations (`read_file`, `write_file`, `edit_file`, `remove`, …), `shell` / `shell_job`, `web_fetch`, `ask_user`, `find_tools`, `task_state` and `verify`. `bash` is also available when a POSIX shell is detected.
- **Task-level recovery.** `task_state` records the goal, plan and acceptance criteria. Each task produces a content-addressed snapshot that can be previewed, rolled back and redone.
- **Explicit boundaries.** Workplace tiers (writes are confined to the application root by default), a Job Object fence around plugin subprocesses, and four authorization gates.
- **CLI and GUI on a shared core.** Both front ends run on the same `Agent`. The GUI adds an activity-stream timeline, a cache hit map, a workspace file view and a live context ring.
- **Remote access.** `aox remote` registers the local machine as an AOXChat agent; mentioning it in a room runs the agent on that machine.

## Quick start

**1. Install.** Run `AOX-GUI-Setup-<version>.exe` (Windows 10 1809+, x64; the Chinese build carries a `-zh` suffix). **Settings → Model** opens automatically on first launch.

**2. Configure a model.** Any OpenAI-compatible endpoint is supported. **Scan models** retrieves the available model names.

```text
Endpoint   https://api.deepseek.com
Key        sk-...
Model      deepseek-flash
```

Credentials are stored only in `.aox/models.json`, which the CLI and GUI share. Model changes take effect on the next request.

**3. Start a task.** The default workspace is the application root, and writes and deletes are confined to it; use the `Workplace` chip to change the tier. Example prompt: *"Read config.example.toml and list its sections."*

> **No runtime dependencies.** The host binary is statically linked and requires no VC++ redistributable, Python or Node.js.
> `bash` is bundled as busybox-w32. Python is optional and can be added with `/runtime install python` or a `-with-python` package.

## Commands

| Command | Description |
| --- | --- |
| `/help` | Show all commands |
| `/tools` · `/plugins` | List available tools and the tools each plugin provides |
| `/model` | List saved models; `/model 2` switches to model 2, effective on the next request |
| `/workplace` | Show or change the write boundary: `full` · `default` · `<dir>` |
| `/task` | Show the current checkpoint: goal, plan progress and acceptance criteria |
| `/rollback` | Preview the last task's file changes; `--yes` applies the rollback, `/redo` reverts it |
| `/usage` | Show the token breakdown and current context size for this conversation |
| `/history` | Manage past conversations: `show 1` · `open 1` · `rm 1` · `dir` |
| `/remote` | Manage the remote bridge: `status` · `start` · `stop` · `pair …` |
| `/new` · `/quit` | Start a new conversation · exit (`Ctrl+C` interrupts a running task) |

Additional commands (`/prompt`, `/hook`, `/loop`, `/config`, `/sandbox`, `/shell`, `/web`, `/checkpoints`, `/backup`, `/attach`, `/demo`, `/expand`, `/runtime`) are covered in the [manual](docs/USAGE.md).

## Build from source

Requires Rust 1.90+ with the MSVC toolchain.

```bash
cargo build -p aox-gui --release                # GUI → target/release/aox-gui.exe
cargo build                                     # CLI → target/release/aox.exe
AOX_LANG=zh cargo build -p aox-gui --release    # Chinese UI (language is set at compile time)
```

On Windows with Git Bash, the linker and `crt-static` must be pinned in `.cargo/config.toml`. See [USAGE § Building from source](docs/USAGE.md#building-from-source).

## Documentation

| Document | Contents |
| --- | --- |
| [**USAGE**](docs/USAGE.md) | Installation, models, commands, GUI windows, conversations, configuration layers, boundaries, rollback, remote access, packaging |
| [**ARCH-ROADMAP**](docs/ARCH-ROADMAP.md) | Design principles, development stages 1–7, build size history |
| [**SANDBOX**](docs/SANDBOX.md) | Restrictions enforced on plugin subprocesses |
| [**SHELL**](docs/SHELL.md) · [**WEB**](docs/WEB.md) | The built-in command line and `web_fetch` |
| [**ROLLBACK**](docs/ROLLBACK.md) | Task-level checkpoints: semantics and invariants |
| [**I18N**](docs/I18N.md) | Localization model: one language per build |
| [**docs/README**](docs/README.md) | Full index: design records, diagrams, working notes |

Most documents have a `*.zh-CN.md` counterpart.

---

ISC License · [2am.top](https://2am.top) · [Issues](https://github.com/forever218/AOX/issues)

[repo]: https://github.com/forever218/AOX
[gui]: gui/README.md
[usage]: docs/USAGE.md
[license]: LICENSE
[b-stars]: https://img.shields.io/github/stars/forever218/AOX?style=flat-square&label=stars&labelColor=141412&color=A5644D
[b-rust]: https://img.shields.io/badge/rust-1.90%2B-F2EEE8?style=flat-square&logo=rust&logoColor=A5644D&labelColor=141412
[b-platform]: https://img.shields.io/badge/platform-Windows%2010%20%7C%2011%20x64-2B322C?style=flat-square&logo=windows&logoColor=white&labelColor=141412
[b-gui]: https://img.shields.io/badge/gui-egui%200.33%20%2B%20wgpu-6E6A5E?style=flat-square&labelColor=141412
[b-plugins]: https://img.shields.io/badge/plugins-python%20node%20rust-8C8578?style=flat-square&labelColor=141412
[b-license]: https://img.shields.io/badge/license-ISC-F2EEE8?style=flat-square&labelColor=141412