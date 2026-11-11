# AOX usage manual

[← README](../README.md) · [Docs index](README.md) · [中文](USAGE.zh-CN.md)

Everything that doesn't belong on the front page: the packages, every command the two front ends
accept, where conversations actually live, how the config is composed, what the boundaries do and do
not cover, and how to build the thing.

**Contents** — [Install](#install-and-first-run) · [Models](#models) · [Commands](#command-line) ·
[GUI](#graphical-interface) · [Conversations](#conversations-and-spaces) · [Config](#configuration-layers) ·
[Tools](#tools) · [Boundaries](#boundaries) · [Rollback](#checkpoints-and-rollback) · [Runtimes](#runtimes) ·
[Remote](#remote) · [Building](#building-from-source) · [Troubleshooting](#troubleshooting) ·
[Layout](#directory-layout) · [Uninstall](#uninstall)

## Install and first run

Windows 10 1809+ on x64. Four packages per front end, eight in total (see
[Building from source](#release-artifacts)); any one of them is self-contained.

| Package | Contents |
|---|---|
| `AOX-GUI-Setup-<version>.exe` | the graphical build, host only (~7.1 MB) |
| `AOX-Setup-<version>.exe` | the command-line build, host only (~4.4 MB) |
| `…-with-python.exe` | the same build plus `runtime/python` (~16 / ~13 MB) |
| `…-<version>-zh.exe` | a Chinese build — the UI language is **compile-time**, hence a separate file |

Every variant ships `runtime/bash` (busybox-w32, 637 KB) as a fallback POSIX shell, and nothing is
written to `PATH` or to user environment variables. Uninstalling removes `runtime` only; `plugins/`
and `.aox/` are user data and survive.

Both front ends resolve "the app root" as the directory the exe sits in, and that is also the
default workspace — so a portable copy is just a copied folder.

- **GUI**: first launch opens **Settings → Model** by itself with the caret in the key field.
- **CLI**: on a TTY it asks three questions (endpoint, key, model name) and writes
  `.aox/models.json`; an empty answer skips. `aox --setup` forces the wizard, and a non-TTY run
  never blocks on input.

## Models

Endpoint, key, model name and its parameters all live in `<app root>/.aox/models.json`
(version 2). That is the **only** place a key lives; the CLI and the GUI read the same file, and the
GUI never writes `config.toml`.

| Field | Meaning | Default |
|---|---|---|
| `name` | label shown in the pickers | the model name |
| `base_url` / `api_key` / `api_key_env` | endpoint, inline key, or an environment variable | — |
| `model` | the model id sent to the API | — |
| `enabled` | whether it appears in the input box's list | true |
| `context_window` | tokens of context, used for the budget ring and compaction | 600000 |
| `max_tokens` | output cap per request | 32000 |
| `temperature` | sampling temperature | 0.3 |
| `thinking` | `default` · `off` · `low` · `high` · `max` | default |
| `thinking_wire` | `auto` · `deepseek` · `openai` · `qwen` · `none` — how the level is put on the wire | auto |
| `stream` | stream the response; turn off for endpoints that reject `stream` | true |

Several models can be saved; one is active. Switch with `/model <n|name>` in the terminal, the
model chip under the input box, or **Settings → Model** — effective on the next request. "Test"
sends a one-token probe; "Scan models" only lists what the endpoint advertises (a service can list
models and still refuse to chat).

A `[provider]` section left in `config.toml` by an older version is read **once**, migrated into
`models.json`, and inert afterwards.

## Command line

| Command | What it does |
| --- | --- |
| `/help` | Show help (lists every command) |
| `/tools` | List all currently available tools |
| `/plugins` | List loaded plugins and what each contributes (hooks / prompt segments / capabilities) |
| `/runtime` | Plugin runtime status; `/runtime install python` assembles a bundled runtime |
| `/history` | Conversations: `list` · `dir` · `show <n\|id>` · `open <n\|id>` · `rm <n\|id>` |
| `/new` | Start a new conversation (the current one is already on disk) |
| `/attach` | Stage local files in the pending-upload tray; `/attach clear` empties it |
| `/demo` | Preview the terminal layout and the collapsed-trace effect |
| `/expand` | Expand the most recent collapsed tool-call trace |
| `/usage` | Token breakdown of this conversation (per round, cache hit/miss, current context) |
| `/prompt` | How the system prompt is assembled from segments (`[prompt]` reorders / disables / adds) |
| `/hook` | The active loop hooks, from `[hook]` and plugins' `[contributes]` |
| `/loop` | The main-loop implementation, context trimming and the per-turn request ceiling (`[loop]`) |
| `/task` | This conversation's checkpoint: goal, plan with ticks, acceptance criteria |
| `/checkpoints` | Task-level file recovery points in this workspace (`--all` for the full list) |
| `/rollback` | Preview the previous task's changes; `--yes` restores (`--force`, `--allow-partial`) |
| `/redo` | Undo the most recent rollback, using its safety snapshot |
| `/backup` | Recovery status: `status` · `verify` · `prune` (apply retention, free space) |
| `/sandbox` | The fence around plugin subprocesses (`[sandbox]`), including what it does not cover |
| `/shell` | The built-in command line: dialects, boundary, limits (`[shell]`) |
| `/web` | The built-in fetch tool: reach, budget, timeout (`[web]`) |
| `/workplace` | Show / switch the write boundary: `full` · `default` · `<dir>` (`[workplace]`) |
| `/config` | Which layers this config is composed of (`aox.toml` customizes the stack) |
| `/model` | List saved models; `/model <n\|name>` switches, effective on the next request |
| `/remote` | Phone bridge in this session: `start` · `stop`; `list` / `use` / `pair` / `policy` / `remove` |
| `/quit` | Quit (`/exit` is equivalent) |

Anything else is sent to the model. `Ctrl+C` interrupts the turn that is running: only the step in
flight is dropped, everything already completed stays in context.

Command-line flags:

| Flag | Effect |
|---|---|
| `--config <path>` | use another file as the last config layer |
| `--plugins <dir>` | use another plugin directory |
| `--manifest <path>` | explicit composition manifest (default `<app root>/aox.toml`) |
| `--profile <name>` | pick a `[profiles.<name>]` from the manifest |
| `--dump-config` | print the layer stack and the composed tree, then exit |
| `--setup` | force the first-run wizard |
| `selftest` | exercise the plugin pipeline end to end without a model |
| `remote …` | the host side of AOX Remote (see [Remote](#remote)) |

The project URL in the banner is a clickable OSC 8 hyperlink. Windows Terminal and the VS Code
terminal enable it automatically; legacy conhost does not. Force it with `AOX_HYPERLINK=1`, or off
with `AOX_HYPERLINK=0`. The spinner goes quiet when output is redirected; `AOX_ANIM=1` / `0`
overrides.

## Graphical interface

One borderless window (1280×800 initially, 1000×640 minimum) plus three optional tool windows. The
top strip carries only the drawer button, a status pill (`Thinking`, `Running bash`, `Retrying in
3 s`, `Waiting for your choice`…) and minimise / maximise / close; double-clicking it maximises. The
settings gear lives in the drawer header.

### Drawer (left)

300 px, pushes the transcript instead of floating, 0.18 s slide, open/closed state remembered in
`.aox/ui.json`.

- Search box filters by **workspace name** or conversation title; `Esc` clears it.
- The tree is grouped by workspace (from `.aox/spaces.json`): folder icon + name + conversation
  count, collapsible; hover a group for `+` (new conversation in that directory).
- A conversation row shows its title, then `time · N msgs` on hover; `⋯` offers `Rename`,
  `Move to ▸` and `Delete` (the conversation you are in cannot be deleted).
- Group header context menu: `New conversation` · `Rename` · `Open in file manager` ·
  `Remove from AOX`.
- Footer: `New workspace`, `Open workspace folder` (`<root>/workplace/`).

### Composer (bottom)

A floating card: plan strip → input → second row.

- **Plan strip** shows `{done}/{total}` and the current step, collapsed by default; expanding lists
  every step. Steps are read-only and travel with the checkpoint.
- **Attachments**: drag files anywhere onto the window; image files are decoded and shown inline in
  the transcript. A chip per file, `×` to remove. Images over 32 MiB are refused with a reason.
- **Second row**: model chip (`deepseek-chat · 1M`), workspace chip (`Default · <dir>` /
  `Full access`), recovery chip (undo icon + count), context ring (`Context: … used, … left`; turns
  amber at 75 %, red at 92 %, spins while compacting) and a `Σ N tok` chip that opens the usage
  panel.
- The send button becomes **Stop** while a turn runs; `Enter` sends, `Shift+Enter` breaks the line.
- Placeholders report state: `Model not configured; chat unavailable`, `Waiting for
  authorization…`, `Running; click Stop on the right`.

### Tool windows

Opened from **Settings → Extra** (and all three at once under `--demo`). Each is a separate
borderless window with pin / minimise / maximise / close.

| Window | Contents |
|---|---|
| **Activity Stream** | A horizontal timeline: lanes for Input / Model / Tool / System, block width = real elapsed time, same plugin = same colour family. Toolbar: `Start` · `Center` · `Latest`, `Round` + `Go`, row height, zoom, `Fit`, `Link lines`. Click a block for its full content. |
| **Cache Hit Map** | 16×16 grid of recent calls (hit / partial / miss), hit rate, tokens saved, lifetime hit rate. |
| **Workspace** | Read-only file tree plus this turn's changes, with `File` / `Diff` views. |

### Settings

| Tab | Contents |
|---|---|
| `Extra` | Entrances to the three tool windows |
| `Model` | Add / edit / batch-manage models; `Use` switches, the switch decides whether it is listed in the composer |
| `Appearance` | `Follow system` / `Dark · ink` / `Light · paper`; welcome line (`My own` / `Hitokoto` / `Model`); text colour per theme; typewriter effect; background image folder |
| `System` | Read-only facts: command line, runtime environment, model, config file and layers, app root, workspace, history, plugins and runtime directories; `Install Python runtime` when missing |
| `Remote` | Start / stop the bridge, pairings, recent log |
| `About` | Version, ISC licence, project home |

### Shortcuts

`Enter` send · `Shift+Enter` newline · `Ctrl+↑` / `Ctrl+↓` previous / next question ·
`Esc` close overlay (or clear the search; in the authorization modal it denies) ·
`y` allow once · `a` allow for the session · `n` deny.

### Theme

Two palettes, lightness only: light is warm paper (`#F2EFE8`), dark is warm black (`#141412`); the
one hue is the brick-red danger colour. `[theme.light]` / `[theme.dark]` / `[theme.metrics]` /
`[theme.fonts]` override field by field; `themes/paper.toml`, `ink.toml` and `high-contrast.toml`
are ready-made; `--theme <file>` replaces the whole `[theme]` table. The choice lands in
`.aox-gui-theme`, and the native caption follows via DWM.

Fonts resolve as `[theme.fonts]` → `AOX_GUI_FONT` / `…_BOLD` / `…_SERIF` / `…_BRAND` / `…_MONO` →
a system candidate list (DengXian, YaHei, SimHei…; Cascadia Mono, Consolas…).

The welcome line appears with a typewriter effect over a random background from
`assets/light|dark/*.png`.

## Conversations and spaces

Every conversation is one JSONL file:

```
<app root>/.aox/conversations/20261105-164145-c314.jsonl
```

The stem is the conversation id (`YYYYMMDD-HHMMSS-4 hex`); `/history` accepts an index, the full
id, an id prefix, or `<id>.jsonl`.

A **space** is a logical grouping that may point at a directory — not a storage partition.
`spaces.json` registers the built-ins (`Local` = the app root, `Remote` = phone conversations,
`Full access`), user spaces (`s:<canonical path>`) and automatic ones for roots that were never
registered (`auto:<path>`); folders under `<app root>/workplace/**` fold into `Local`.

### File format

One JSON object per line. The first line is `meta` (`id`, `created`, `model`, `root`, `version`,
`origin`, `space`, `room`); every following line is one of `message`, `act`, `usage`, `workplace`,
`title`, `task`. Attachments store a path and a name, never base64.

A `message` line is the **raw message object of the OpenAI request body** plus `is_error`, so the
file is both a log and a replayable request. Trade-offs, unchanged: append-only; one unparsable
line does not break the file; an interrupted turn is rolled back out of the file so no half-written
`tool_calls` survives.

### `/history`

```bash
/history                  # list: index, time, count, first user input as the title
/history show 1            # full text of entry 1
/history open 1            # switch to entry 1 and keep talking
/history dir               # print the directory
/history rm 2              # delete entry 2
```

### Retention

`[history]` in `config.toml`:

```toml
[history]
enabled = true   # off = no new files; existing ones stay readable
keep = 200       # counted across all spaces, 0 = unlimited
```

Cleanup is incidental: it runs only when a new conversation file is created — no timer, no scan at
startup. `[store] backend = "jsonl"` is the only backend today; an unknown name fails loudly at
startup instead of silently falling back.

## Configuration layers

The config is **composed**, lowest to highest: `bundle → patch → config.toml`. What you write in
`config.toml` always wins, so an existing file needs no migration.

- `aox.toml` (optional, `<app root>`) is the manifest: which bundles to load, which patch files to
  stack, and named profiles. `--manifest` / `--profile` override it for one run.
- Bundles live in `<app root>/bundles/<name>.toml` and replace the built-in layer of the same name.
  Built-ins: `core`, `llm-openai`, `tools`, `web`, `store-jsonl`, `recovery`, `loop-standard`,
  `prompt-core`, `strict`.
- Unknown bundle, missing patch, bundle cycle or unknown profile is a startup error, never a quiet
  fallback.
- `/config` prints the layer stack; `aox --dump-config` prints the stack **and** every composed
  value with the layer that last wrote it.

Sections in `config.toml`: `[auth]` `[agent]` `[prompt]` `[sandbox]` `[plugins]` `[tools]`
`[workplace]` `[history]` `[store]` `[recovery]` `[shell]` `[web]` `[runtimes]` `[theme]`.

## Tools

| Group | Tools |
|---|---|
| Plugins | `create_plugin`, `update_plugin`, `list_plugins`, `remove_plugin` |
| Files | `read_file`, `write_file`, `edit_file`, `list_dir`, `remove` (recycle bin), `attach_file`, `publish_file` |
| Host | `ask_user`, `find_tools`, `task_state`, `verify` |
| Command line | `shell` (cmd / PowerShell / pwsh / git dialects), `bash` (registered when a POSIX shell is found), `shell_job` (long-running background jobs) |
| Web | `web_fetch` (`readable` / `links` / `text` / `json` / `raw`, `save=true` to land it on disk) |

Files deleted with `remove` go to the **system recycle bin**. Above `[tools] inline_max` (default 24)
plugin tools are hidden from the request until the model calls `find_tools`; hiding is advertising
only, so a hidden tool still runs when called by name.

### Authorization

Four gates — `meta_tools`, `plugin_tools`, `shell`, `web` — each `ask` · `auto` · `deny`, **all
default to `auto`**. `[auth.overrides]` matches by exact tool or plugin name, or by a trailing `*`
prefix; a key containing `*` must be quoted in TOML. Order: exact → longest prefix → `*` →
category default. Components that declare non-empty `caps` still force a prompt.

## Boundaries

Two independent things, often confused:

| | `[workplace]` | `[sandbox]` |
|---|---|---|
| Governs | where this session may **write and delete** | how plugin **subprocesses** run |
| Tiers | `default` (app root) · `full` · `fixed` + `root` | `enabled`, `max_processes`, `max_memory_mb`, `ui_limits` |
| Enforced by | the host, on file tools and declared plugin path arguments | a Windows Job Object (`CREATE_SUSPENDED` → assign → resume, `KILL_ON_JOB_CLOSE`) |

- **Reading is never restricted** by the workplace — `read_file` / `list_dir` can look anywhere.
  The boundary covers writes and deletes, and it judges a plugin's declared path arguments as
  writes (a coarse net). It **cannot** stop a misbehaving plugin from `open()`-ing a file itself;
  that would need OS-level isolation.
- `full` must be chosen explicitly and is confirmed with a red dialog; `/workplace` and the
  `Workspace` chip switch tiers mid-session.
- The sandbox is a **guard rail, not a security boundary**, and it does not cover files or network.
  Windows only. The shell has its own job (`[shell] max_processes`, default 16) and can optionally
  run through a low-integrity helper.

## Checkpoints and rollback

Each task leaves a recovery point under `<workspace>/.aox/backup/`: a content-addressed object
store (BLAKE3) plus immutable nodes, an atomic `cursor.json`, journals and a redo snapshot.

```bash
/checkpoints                # list recovery points (20 by default, --all for everything)
/rollback                   # preview the previous task's changes
/rollback --yes             # restore them (--force = --yes, --allow-partial accepts an incomplete point)
/redo                       # undo that rollback from its safety snapshot
/backup status              # object count, size, retention
/backup verify              # re-hash the store
/backup prune               # apply retention and free unused space
```

Retention defaults in `[recovery]`: `keep = 30` tasks, `max_total_mb = 512`, `max_file_mb = 64`,
`max_turn_mb = 256`, `shell_guard = "strict"`, `protect_plugins = true`, plus exclusion globs. In
the GUI the undo chip opens the same thing with a preview, a risk confirmation and the four
maintenance actions.

## Runtimes

The host itself needs nothing. Plugins do — three languages, resolved once at startup, first hit
wins:

1. a **full path** in `[runtimes]` → used as-is (a missing path is reported, never substituted);
2. `prefer = "system"` (default) → `PATH` first, then the bundled `runtime/<lang>/`;
3. `prefer = "builtin"` → the bundled one first, skipping `PATH`.

| Language | Running a plugin | Creating one |
|---|---|---|
| python | needs `python` 3.8+ | same |
| node | needs `node` | same |
| rust | needs nothing (compiled exe) | needs `rustc` |

`/runtime install python` assembles CPython 3.13.7 embeddable (~10 MB, SHA-256 verified) into
`runtime/python`; an offline `aox-python-runtime.zip` next to the exe, in the app root, in
`runtime/` or under `%LOCALAPPDATA%\AOX` is picked up first. Node is over 80 MB and is not
auto-installed; rustc is not bundled. The embeddable distribution has no `pip` — point
`[runtimes] python` at a full install if you need it.

The `bash` **tool** is separate from plugin runtimes: Git for Windows → a `git.exe`-derived bash →
`bash.exe` on `PATH` → `runtime/bash/bash.exe` → `runtime/bash/busybox.exe` (WSL's stub is
rejected). busybox is a POSIX `sh`: no arrays, no `[[ ]]`, no process substitution.

## Remote

`aox remote` registers this machine as an agent on an AOXChat server and keeps a WebSocket open. A
room member @-mentions the agent and a real turn runs **here**; process, result and artefacts go
back to the room. The server address is not baked in; credentials live in `.aox/remote.json`.

```bash
aox remote pair [server] [code] [agent name]   # code comes from the phone: Settings → Add computer
aox remote list | use <n|id> | rename | remove | status
aox remote policy [ask|owner|open]             # who may trigger this machine
aox remote serve                               # long-running bridge (blocking)
```

Inside a session, `/remote start` / `/remote stop` run the same bridge in the background, and
`/remote list|use|pair|policy|remove` behave as above. `call_policy` defaults to `ask`: someone
else's mention requires your nod. While the phone is in control, a modal covers the window
(`Remote control is active`, with `Take back control`), keyboard input is swallowed, and
`.aox/remote.lock` records the takeover.

## Building from source

Requires Rust 1.90+ and the MSVC toolchain.

```bash
cargo build                      # CLI only (the workspace default member)
cargo build -p aox-gui --release  # GUI  → target/release/aox-gui.exe
AOX_LANG=zh cargo build -p aox-gui --release   # Chinese catalogue, compiled in
```

`AOX_LANG` only picks which `locales/<code>.toml` is embedded — there is no runtime language switch,
which is why the Chinese installer is a separate file.

**Windows + Git Bash**: Git Bash's coreutils `link` shadows MSVC's `link.exe` (`link: extra operand
'...rcgu.o'`). Pin the linker and the MSVC runtime in `.cargo/config.toml`:

```toml
[target.x86_64-pc-windows-msvc]
linker = "C:\\Program Files\\Microsoft Visual Studio\\2022\\Community\\VC\\Tools\\MSVC\\<version>\\bin\\Hostx64\\x64\\link.exe"

# Static MSVC runtime (the /MT equivalent). **Mandatory for releases**: without it the exe
# depends on VCRUNTIME140.dll, which no clean Windows install has.
rustflags = ["-C", "target-feature=+crt-static"]

[env]
LIB     = { value = "<MSVC>\\lib\\x64;<WinSDK>\\Lib\\<version>\\um\\x64;<WinSDK>\\Lib\\<version>\\ucrt\\x64", force = true }
INCLUDE = { value = "<MSVC>\\include;<WinSDK>\\Include\\<version>\\um;<WinSDK>\\Include\\<version>\\ucrt;<WinSDK>\\Include\\<version>\\shared;<WinSDK>\\Include\\<version>\\winrt", force = true }
```

The file holds machine-specific paths and is git-ignored, so create it yourself. From PowerShell,
cmd or a `vcvars64` terminal the `linker` / `LIB` / `INCLUDE` entries are unnecessary — but
`rustflags` must stay in every case, or `build.rs` prints a loud `cargo:warning` about the artefact
not starting on a clean machine.

### Release artifacts

```bash
bash tools/build_installer.sh                    # eight installers + the runtime zip
bash tools/build_installer.sh all none           # hosts only (both front ends, both languages)
bash tools/build_installer.sh gui with-python    # GUI, bundled python only
bash tools/build_installer.sh pack               # only aox-python-runtime.zip
```

`installer/aox.iss` and `installer/aox-gui.iss` are compiled twice each (with and without
`BUNDLE_PYTHON`), and `AOX_LANG=zh` adds the `-zh` file names.

| Artifact | Contents | Size |
|---|---|---|
| `AOX-Setup-<v>.exe` / `AOX-GUI-Setup-<v>.exe` | host only | ≈4.4 MB / ≈7.1 MB |
| `AOX-Setup-<v>-with-python.exe` / `AOX-GUI-Setup-<v>-with-python.exe` | plus `runtime/python` | ≈13 MB / ≈16 MB |
| `AOX-Setup-<v>-zh.exe` and the three siblings | Chinese UI | same |
| `aox-python-runtime.zip` | runtime component package | ≈11 MB |

Pass the version to ISCC as `--define=AppVersion=…`, **not** the old `/DAppVersion=…`: ISCC treats
`/`-prefixed arguments as options, and Git Bash rewrites them as Unix paths before launching a
native program, so ISCC reports `You may not specify more than one script filename.`

## Troubleshooting

**A turn stops halfway and the model says nothing.** Look for a `⚠` / `✗` line first; AOX no longer
ends silently.

1. **Thinking models** return `reasoning_content`, which must be replayed verbatim or the second
   request of a multi-step task is rejected with a 400. AOX handles this; nothing to do.
2. **`max_tokens` too small** — reasoning tokens are billed against the output budget, and an
   exhausted budget yields an empty reply. 8192+ is a sane floor.
3. **Raw traffic**: `AOX_DEBUG_LLM=1 aox` appends every request and response to `logs/llm.jsonl`
   under the **current working directory** (`finish_reason`, `usage`, empty replies).

**A black console flashes in the GUI.** `aox-gui.exe` is a `windows`-subsystem program with no
console; spawning `python.exe` without `CREATE_NO_WINDOW` makes Windows allocate one. The host sets
it on every spawn path, so a current build is clean. `cargo run` builds a **console** binary — a
black window there is normal.

```bash
python tools/check_no_console_window.py --ab     # mechanism self-check
python tools/check_no_console_window.py --app target/release/aox-gui.exe --cwd .
```

**`Model not ready`.** `models.json` has no active model, or the key is a placeholder. Open
Settings → Model; the GUI says which file it read.

**Python plugins will not load.** No interpreter was found. `/runtime` reports which tier resolved
(or not), and `/runtime install python` fixes it in place.

## Directory layout

```
<app root>/
  config.toml                your configuration (generated on first run)
  aox.toml                   optional composition manifest (bundles / patches / profiles)
  bundles/<name>.toml        optional bundle overrides
  RULES.md                   optional project rules, injected into the system prompt
  aox.exe / aox-gui.exe
  .aox/
    models.json              the model list and the active model
    spaces.json              the space registry
    conversations/*.jsonl     one file per conversation
    backup/                  content-addressed recovery store
    memory/*.md              the agent's own dated notes
    remote.json              AOX Remote pairings
    ui.json / welcome.json / remote.lock
    tmp/  web/  files/  run/  toolcache/
  plugins/<name>/            plugins the model wrote, plus sdk/
  runtime/bash/              busybox-w32 fallback shell
  runtime/python/            only with -with-python or /runtime install python
  workplace/<project>/       durable deliverables
  temp/                      scratch files
  logs/llm.jsonl             only with AOX_DEBUG_LLM=1, under the current working directory
```

Plugin subprocesses start with the **app root** as their working directory; Python plugins are
forced to UTF-8 output.

## Uninstall

Settings → Apps, or the Start-menu entry. Uninstalling removes `runtime/` (machine-specific and
re-provisionable) and the installed binaries. It **keeps** `plugins/` (capability the model built)
and `.aox/` (conversations, models, snapshots, pairings) — both are user data. Delete the install
folder by hand if you really want them gone.
