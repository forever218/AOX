<div align="center">

<img src="assets/aox-icon.png" width="112" alt="AOX">

# AOX

**An AI agent that grows its own tools.**<br>
Ships with a working set — everything else it writes itself, in Python, Node or Rust.

[![stars][b-stars]][repo] [![rust][b-rust]][repo] [![platform][b-platform]][repo] [![gui][b-gui]][gui] [![plugins][b-plugins]][usage] [![license][b-license]][license]

[Quick start](#quick-start) · [Commands](#commands) · [Build](#build-from-source) · [Docs](#documentation) · [**简体中文**](README.zh-CN.md)

</div>

- **It writes what it lacks.** Needs a capability? It authors a plugin, injects it, calls it on the very next turn. Plugins are files on disk, restored after a restart; `update_plugin` hot-swaps one in place.
- **A real working set out of the box.** 18 host tools: files (`read_file` / `write_file` / `edit_file` / `remove`…), `shell` · `shell_job`, `web_fetch`, plus `ask_user`, `find_tools`, `task_state` and `verify` — and `bash` whenever a POSIX shell is found.
- **Every task is recoverable.** `task_state` holds the goal, the plan and the acceptance criteria; each task leaves a content-addressed snapshot you can preview, roll back and redo.
- **Boundaries you can see.** Workplace tiers (writes stay under the app root by default), a Job Object fence around plugin subprocesses, four authorization gates.
- **Two front ends, one core.** CLI and GUI over the same `Agent`; the GUI adds an activity-stream timeline, a cache hit map, a workspace file view and a live context ring.
- **Answer from your phone.** `aox remote` registers this machine as an AOXChat agent — @ it in a room and the real agent runs here.

## Quick start

**1 · Run it.** `AOX-GUI-Setup-<version>.exe` (Windows 10 1809+ / x64; a Chinese build ends in `-zh`). The first launch opens **Settings → Model** by itself.

**2 · Add a model.** Any OpenAI-compatible endpoint; "Scan models" fills the name for you.

```text
Endpoint   https://api.deepseek.com
Key        sk-...
Model      deepseek-flash
```

Stored in `.aox/models.json` — the only place a key lives. CLI and GUI read the same file, and the switch takes effect on the next request.

**3 · Ask.** The default workspace is the app root, so writes and deletes stay under it (switch tiers with the `Workspace` chip). Try *"read config.example.toml and list its sections"*.

> **No runtime needed.** The host is statically linked — no VC++ redistributable, no Python, no Node.
> `bash` ships as busybox-w32; Python is optional (`/runtime install python` or a `-with-python` package).

## Commands

| Command | What it does |
| --- | --- |
| `/help` | Everything, including the commands below |
| `/tools` · `/plugins` | What it can call right now, and what each plugin contributes |
| `/model` | List saved models; `/model 2` switches, effective on the next request |
| `/workplace` | Show or switch the write boundary — `full` · `default` · `<dir>` |
| `/task` | The current checkpoint: goal, plan with ticks, acceptance criteria |
| `/rollback` | Preview the last task's file changes; `--yes` restores, `/redo` undoes it |
| `/usage` | Token breakdown for this conversation, and the current context size |
| `/history` | Past conversations — `show 1` · `open 1` · `rm 1` · `dir` |
| `/remote` | Phone bridge: `status` · `start` · `stop` · `pair …` |
| `/new` · `/quit` | Start a fresh conversation · exit (`Ctrl+C` interrupts a running task) |

`/prompt`, `/hook`, `/loop`, `/config`, `/sandbox`, `/shell`, `/web`, `/checkpoints`, `/backup`, `/attach`, `/demo`, `/expand` and `/runtime` are in the manual.

## Build from source

```bash
cargo build -p aox-gui --release                # GUI → target/release/aox-gui.exe
cargo build                                     # CLI → target/release/aox.exe
AOX_LANG=zh cargo build -p aox-gui --release    # Chinese UI — the language is compile-time
```

Rust 1.90+ with the MSVC toolchain. On Windows + Git Bash the linker and `crt-static` have to be
pinned in `.cargo/config.toml` — see [USAGE § Building from source](docs/USAGE.md#building-from-source).

## Documentation

| Doc | What's inside |
| --- | --- |
| [**USAGE**](docs/USAGE.md) | Install, models, every command, GUI windows, conversations, config layers, boundaries, rollback, remote, packaging |
| [**ARCH-ROADMAP**](docs/ARCH-ROADMAP.md) | The seven design principles, stages 1–7, the size history of every build |
| [**SANDBOX**](docs/SANDBOX.md) | What a plugin subprocess is actually blocked from doing |
| [**SHELL**](docs/SHELL.md) · [**WEB**](docs/WEB.md) | The built-in command line and `web_fetch` |
| [**ROLLBACK**](docs/ROLLBACK.md) | Task-level checkpoints: semantics and invariants |
| [**I18N**](docs/I18N.md) | One language, one build |
| [**docs/README**](docs/README.md) | The full index — design record, diagrams, working notebooks |

Most documents carry a `*.zh-CN.md` copy alongside them.

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
