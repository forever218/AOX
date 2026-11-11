<div align="center">

<img src="assets/aox-icon.png" width="112" alt="AOX">

# AOX

**A self-growing AI coding agent.**<br>
No tools at all — it writes them itself, the moment a task needs one.

[![stars][b-stars]][repo] [![rust][b-rust]][repo] [![platform][b-platform]][repo] [![gui][b-gui]][gui] [![plugins][b-plugins]][usage] [![license][b-license]][license]

[Quick start](#quick-start) · [Commands](#commands) · [Build](#build-from-source) · [Docs](#documentation) · [**简体中文**](README.zh-CN.md)

</div>

- **No tools at all.** The host ships a core loop and three meta-tools — not one file, shell or web tool.
- **It builds what it needs.** Read a file? It writes a plugin, injects it, calls it on the very next turn.
- **It remembers.** Plugins are plain files on disk, restored after a restart — it gets stronger the longer you use it.
- **One core, two faces.** The CLI and the GUI are two `Ui` implementations over the same agent.

## Quick start

**1 · Run it.** Double-click `aox-gui.exe` (or `aox.exe` for the terminal). The first launch opens the
model dialog by itself. `config.toml` has **no** `[provider]` section — the model lives in
`.aox/models.json`, the one file both builds read.

**2 · Add a model.** Any OpenAI-compatible endpoint; "Scan" fills the model name for you.

```text
Endpoint   https://api.deepseek.com
Key        sk-...
Model      deepseek-flash
```

**3 · Ask.** For example *"read config.toml and tell me what's in it"* — it has no file tool, so it
writes an `fs` plugin first, then uses it. That plugin stays on disk and is back after a restart.

> **No runtime needed.** The host is statically linked — no VC++ redistributable, no Python, no Node.
> Only python / node **plugins** need their interpreter; rust plugins compile to native exes.

## Commands

| Command | What it does |
| --- | --- |
| `/help` | Everything, including the commands below |
| `/tools` · `/plugins` | What it can do right now, and which plugins are loaded |
| `/model` | List the saved models; `/model 2` switches, effective on the next request |
| `/history` | Past conversations — `show 1` · `open 1` · `rm 1` · `dir` |
| `/new` | Start a fresh conversation (the current one is already on disk) |
| `/runtime` | Interpreter status; `/runtime install python` assembles one |
| `/expand` | Unfold the last run's tool calls |
| `/quit` | Exit — `Ctrl+C` interrupts a running task |

## Build from source

```bash
cargo build -p aox-gui --release   # GUI → target/release/aox-gui.exe
cargo build                        # CLI → target/release/aox.exe
```

Rust 1.90+ with the MSVC toolchain. On Windows + Git Bash the linker and `crt-static` have to be
pinned in `.cargo/config.toml` — see [USAGE § Building from source](docs/USAGE.md#building-from-source).

## Documentation

| Doc | What's inside |
| --- | --- |
| [**USAGE**](docs/USAGE.md) | GUI layout, history format, runtime resolution, release artifacts, troubleshooting |
| [**ARCH-ROADMAP**](docs/ARCH-ROADMAP.md) | The seven design principles, stages 1–7, the size history of every build |
| [**SANDBOX**](docs/SANDBOX.md) | What a plugin subprocess is actually blocked from doing |
| [**SHELL**](docs/SHELL.md) · [**WEB**](docs/WEB.md) | The shell tool and `web_fetch` |
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
[b-platform]: https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2B322C?style=flat-square&logo=windows&logoColor=white&labelColor=141412
[b-gui]: https://img.shields.io/badge/gui-egui%200.33%20%2B%20wgpu-6E6A5E?style=flat-square&labelColor=141412
[b-plugins]: https://img.shields.io/badge/plugins-python%20node%20rust-8C8578?style=flat-square&labelColor=141412
[b-license]: https://img.shields.io/badge/license-ISC-F2EEE8?style=flat-square&labelColor=141412
