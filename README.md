# AOX

A **self-growing** AI coding agent. It starts with **no tools at all** — just a core loop and three
meta-tools. When a task needs some capability (reading and writing files, running commands,
fetching web pages…), it **writes a plugin itself and injects it**, and can call it on the very next
turn. Plugins persist on disk and are restored automatically after a restart — so the more you use
it, the stronger it gets.

---

## Quick start

1. Double-click `aox-gui.exe`. The first run opens **Settings → Model** by itself (`config.toml` is
   generated in the program directory too, but it has **no** `[provider]` section — the model is not
   configured there).
2. Press "Add model", fill in the endpoint and the key, press "Scan models" and pick one (or type the
   name yourself), then save:

   | Field | Example |
   |---|---|
   | Endpoint | `https://api.deepseek.com` (any OpenAI-compatible service works) |
   | Key | `sk-your-key` |
   | Model | pick from the scan, or type `deepseek-flash` |

   The endpoint, the key, the model name and its parameters (context window / output cap /
   temperature / thinking) all live in `<program dir>/.aox/models.json` — that is the **only** copy of
   the model configuration, and the command-line `aox.exe` reads the same file. No restart needed.
3. Just talk normally, for example:

   > Read config.toml and tell me which configuration options it contains

   It discovers it has no file-reading capability, so it first builds an `fs` plugin and then uses
   it to finish the task.

## Building from source

Requires Rust 1.90+ and the MSVC toolchain.

```bash
cargo build                      # build the CLI only (the workspace default member)
cargo run                        # run the CLI
cargo build -p aox-gui --release # build the GUI
cargo run -p aox-gui             # run the GUI
```

**Windows + Git Bash users, take note**: Git Bash's bundled coreutils `link` shadows MSVC's
`link.exe` on `PATH`, reporting `link: extra operand '...rcgu.o'`. The fix is to specify the linker
and `LIB` / `INCLUDE` explicitly in `.cargo/config.toml`:

```toml
[target.x86_64-pc-windows-msvc]
linker = "C:\\Program Files\\Microsoft Visual Studio\\2022\\Community\\VC\\Tools\\MSVC\\<version>\\bin\\Hostx64\\x64\\link.exe"

# Statically link the MSVC runtime (the equivalent of /MT), **mandatory for releases**:
# otherwise the exe depends on VCRUNTIME140.dll, which is not a Windows built-in component
# (it comes from the VC++ 2015-2022 Redistributable), and a clean install dies immediately
# with "VCRUNTIME140.dll not found".
rustflags = ["-C", "target-feature=+crt-static"]

[env]
LIB     = { value = "<MSVC>\\lib\\x64;<WinSDK>\\Lib\\<version>\\um\\x64;<WinSDK>\\Lib\\<version>\\ucrt\\x64", force = true }
INCLUDE = { value = "<MSVC>\\include;<WinSDK>\\Include\\<version>\\um;<WinSDK>\\Include\\<version>\\ucrt;<WinSDK>\\Include\\<version>\\shared;<WinSDK>\\Include\\<version>\\winrt", force = true }
```

This file holds machine-specific absolute paths (useless to anyone else) and is excluded by
`.gitignore`, so **you must create it yourself**. Building from PowerShell / cmd, or from a terminal
that has already run `vcvars64`, does not need the `linker` / `LIB` / `INCLUDE` entries — but **the
`rustflags` line must stay no matter which terminal you use**.

If you leave out `rustflags` the build does not fail, but both `build.rs` scripts print a prominent
`cargo:warning` telling you the artifact won't start on a clean machine.

## Command line

| Command | What it does |
| --- | --- |
| `/help` | Show help (lists every command) |
| `/tools` | View all currently available tools |
| `/plugins` | View the loaded plugins |
| `/history` | View the conversation list (`/history show <n>` for the full text, `/history open <n>` to continue it, `/history dir` for the directory, `/history rm <n>` to delete) |
| `/model` | List the saved models (the same list the GUI's Settings → Model edits); `/model <n\|name>` switches to one, effective on the next request |
| `/new` | Start a new conversation (the current one is already on disk) |
| `/runtime` | View how the interpreters resolved; `/runtime install python` assembles the bundled runtime |
| `/demo` | Preview terminal layout and folding |
| `/expand` | Expand the most recently folded tool-call sequence |
| `/quit` | Exit (`/exit` is equivalent) |

Press `Ctrl+C` to interrupt a running task.

The project URL in the startup banner is a clickable hyperlink (OSC 8). Windows Terminal, the VS
Code terminal and the like enable it automatically; legacy conhost (for example, double-clicking the
exe) doesn't recognise the sequence and falls back to a plain-text URL. When auto-detection gets it
wrong, force it on with `AOX_HYPERLINK=1` or off with `AOX_HYPERLINK=0`.

## Waiting animation and process folding

While a task runs, the terminal keeps **a single line** of status (the text switches automatically
when a tool runs):

```
⠹ Thinking 3.2s
⠼ Running fs_read 8.7s
```

If the model is a **thinking model** (`deepseek-flash` / `deepseek-reasoner` and the like), during
the wait it also scrolls its reasoning live below the status line — seeing *what it's thinking*
tells you which step the task is at:

```
⠹ Thinking 3.2s
  I first need to see which files are in this directory,
  then decide whether to build a new plugin to read the config.
```

This reasoning is a **momentary echo that appears immediately and vanishes when the response
ends** (the scrolling window keeps the last 4 lines); it doesn't pollute the output and doesn't
enter the folded region. To turn streaming off (for example, an endpoint that doesn't support the
`stream` parameter), set `"stream": false` on that model in `.aox/models.json` — or turn
Streaming off in the model's edit dialog in the GUI.

Once a task finishes, the intermediate model thinking and tool calls are folded automatically into
a one-line summary, showing only the final output:

```
▸ Folded 8 tool calls · 12.4s · /expand to expand

(the model's final output)
```

Type `/expand` at any time to expand the details of the most recent run (each step's call arguments
and result).

The animation goes silent automatically when output is redirected to a file/pipe; force it on with
the environment variable `AOX_ANIM=1` or off with `AOX_ANIM=0`.

## Graphical interface (AOX GUI)

Besides the command-line version, the repo also carries a **standalone graphical interface**: `gui/`
(crate name `aox-gui`).

It **shares a single core** with the CLI — `src/lib.rs` exposes the Agent main loop, the LLM
Provider, the plugin host and the `Ui` abstraction as a library, and the GUI is just another
implementation of `aox::ui::Ui`, with no logic forking off anywhere. The CLI's behaviour is
unaffected (`cargo build` still builds only the CLI, see `default-members`).

```bash
cargo run -p aox-gui                      # run directly (shares config.toml with the CLI)
cargo run -p aox-gui -- --demo            # demo mode: no model, previews the interface and event stream
cargo run -p aox-gui -- --prompt "what plugins are there right now"   # send the first instruction automatically on launch
cargo build -p aox-gui --release          # produces target/release/aox-gui.exe
```

Interface layout:

| Area | Contents |
| --- | --- |
| Top bar | The two leftmost icons: **history sidebar toggle**, **new conversation**; the top-right three (left to right): About, activity-stream toggle, **light/dark toggle**. The left side shows a running elapsed time only while a task is executing |
| Left history sidebar | Collapsed by default; click the top bar's leftmost icon to expand it (see [Conversation history](#conversation-history)). Clicking an entry **switches to that conversation** to continue it |
| Main area | The conversation flow: user bubbles, the assistant's Markdown (headings with a hairline rule, **inline code as a tinted chip**, **tables with a full frame + a header band + row separators**, **fenced code blocks with syntax highlighting**), and **automatically folded process blocks** (click to expand the tool-call details) |
| Activity stream | **Not in the main window**: clicking that top-bar icon opens a separate window, initially the same size as the main one (with the system title bar — draggable, resizable, minimisable, Win11's own rounded corners). Its body is a **horizontal timeline**: the x axis is time, the task advances left to right, and a block's width is its real elapsed time; tool calls / thinking / authorisation / messages each get a row and a colour, and blocks for the same plugin use one colour family at different shades. **Click a block (or a row in the list below) to see the details**. Wheel to pan, Ctrl+wheel to zoom |
| Bottom | Multi-line input (`Enter` sends, `Shift+Enter` inserts a newline), send / stop buttons |

Other details:

- The palette is a **"paper and ink" monochrome scheme** — lightness only, no hue: light mode is a
  sheet of warm paper (`#F2EFE8`), dark mode is a warm black (`#141412`). Click the top bar's
  rightmost button to switch (light shows a crescent moon, dark shows a sun), and the native title
  bar's background and text colour follow along (DWM attributes); the preference is remembered in
  `.aox-gui-theme`. Success/warning/failure are not distinguished by colour but by lightness levels
  and inverted blocks. **The one exception is code blocks**: syntax highlighting is a low-saturation
  warm family (ochre-red keywords, pine-green types, steel-blue functions, olive strings, plum
  numbers, warm-grey comments, amber metadata), only two sets of light and dark, switching with the
  theme.
- Syntax highlighting covers rust / python / js·ts / the c family / shell / sql / json·toml·yaml /
  html·xml; an unrecognised language falls back to "highlight only strings and numbers" rather than
  mislabelling. Recognition rests on two general heuristics: **an identifier immediately followed by
  `(` is a function, and PascalCase is a type**, so no per-language type table has to be maintained.
  A ```` ``` ```` language hint supports suffixes like `rust,ignore`, of which only the first
  segment is used.
- The native title bar carries no model name (the title is always `AOX`), and no window icon is set
  separately either: the top-left shows only the title itself, branding is left to the initial
  screen, and the model name and runtime distribution state live in the About window. Note that on
  Windows the title-bar icon and the taskbar icon are the same one, so without setting it the icon
  is invisible on the taskbar too.
- The initial screen is a greeting plus three example instructions; clicking one fills it into the
  input box (it does not send immediately).
- Meta-tools that need authorisation pop up a modal, with shortcuts identical to the CLI: `y` allow
  once, `a` always allow this session, `n` / `Esc` deny.
- While a task is running you can click the stop button in the bottom-right to interrupt it, and the
  conversation history rolls back to its pre-task state (equivalent to the CLI's `Ctrl+C`); the
  in-memory messages and the already-written `history/*.jsonl` roll back together, leaving no
  half-written record.
- The interface language follows the system: it automatically picks a Chinese font from
  `C:\Windows\Fonts` (DengXian / Microsoft YaHei / SimHei…), with no manual configuration. The
  greeting on the initial screen picks a different **flourished display face** (Gabriola → Ink
  Free → …), falling back step by step to serif, bold and body fonts so it never becomes tofu.
  Use `AOX_GUI_FONT`, `AOX_GUI_FONT_BOLD`, `AOX_GUI_FONT_SERIF`, `AOX_GUI_FONT_BRAND`,
  `AOX_GUI_FONT_MONO` to point at font files.
- Scrolling up through history doesn't get yanked back to the bottom automatically; scrolling back
  near the bottom resumes following.

The repository root's behaviour is **exactly the same** as before the GUI was added; the GUI is a
separate crate, and you can remove it by deleting `gui/` and the `[workspace] members` entry in
`Cargo.toml`.

## Conversation history

Conversations are **written to disk automatically**, and the command line and the GUI use the same
implementation (the single write point inside `Agent`), so closing the window/terminal doesn't lose
them.

They live in `<app root>/history/`, **one file per conversation**:

```
history/
  20261002-155959-d81c.jsonl
  20261002-160001-e98c.jsonl
```

> The two installers default to different directories by design (`%LOCALAPPDATA%\Programs\AOX` and
> `…\AOX-GUI`), so each has its own `history/`. To share them, just install one into the other's
> directory.

The filename stem is the conversation's id (`yyyymmdd-hhmmss-4 random chars`) and is also the key
the command line's `show` / `rm` recognise.

### File format

JSONL — one JSON object per line, with the first line holding metadata and each line after that a
message:

```json
{"created":1790927628,"id":"20261002-155348-c5d8","kind":"meta","model":"deepseek-chat","root":"D:\\AOX","version":"0.1.0"}
{"content":"Hi, take a look at the directory first","kind":"message","role":"user"}
{"content":"Let me list the directory first.","kind":"message","role":"assistant","tool_calls":[...]}
{"content":"[FILE] ...","is_error":false,"kind":"message","name":"fs_ls","role":"tool","tool_call_id":"call_1"}
```

A message line is the **raw message object from the OpenAI request body**, with only one extra field
added, `is_error`; so the file is both a human-readable log and directly replayable as a request
body. The system prompt is not written to disk (it is reassembled from code on every launch, and
saving it would only store a copy that goes stale). `kind` distinguishes line types and is stripped
when reading back.

A few deliberate trade-offs:

- **Append-only**, never rewritten wholesale. A power cut or a killed process loses at most the last
  line; everything before it survives.
- **One bad line doesn't break the whole file**: unparsable lines are skipped on read, rather than
  making the entire conversation unopenable.
- **An interruption rolls back**: when `Ctrl+C` / the stop button rolls back a half-finished task,
  that half is deleted from the file as well — otherwise it would leave a broken record with
  `tool_calls` but no result, which the server rejects outright on replay.

### Command line

```bash
/history                  # list: index, time, count, first user input as the title
/history show 1           # view the full text of entry 1 (you / assistant / tool calls and results)
/history open 1           # switch to entry 1 and continue it (the old conversation is already on disk, no "save" needed)
/history dir              # print the directory path
/history rm 2             # delete entry 2
```

In the list, the entry for the current session is marked `← current`. `show` / `open` / `rm` also
accept an id or id prefix, so you don't have to count indices.

### GUI

The top-left two icons: **list** toggles the left sidebar, **plus** starts a new conversation.

- The sidebar lists every conversation, ordered **by the time it was last spoken to**, newest first;
  a small dot on the right marks **the one you're in right now** (it is always pinned to the top —
  an old conversation you just switched into also lands at the very top).
- Clicking any entry **switches to that conversation**: the main body swaps in immediately, and
  continuing the chat continues its context; to start a fresh one, click the plus — the previous one
  is already sitting complete in `history/`.
- Both side panels are collapsed by default, so the window opens to a clean conversation area.
- After the current conversation is written to disk the sidebar refreshes automatically, so the
  manual "refresh" button is rarely needed; that button is a fallback for "another window was
  opened / `history/` was changed elsewhere".

### Retention policy

`[history]` in `config.toml`:

```toml
[history]
enabled = true   # once off, no new files are written; existing history is still viewable
keep = 200       # keep only the most recent N conversations, 0 = unlimited
```

Cleanup is **incidental**: it only counts the directory and deletes the excess old files when a new
conversation file is created — no background thread, no timer, and no full scan at startup.

### Uninstall

`history/` is user data and **is not deleted on uninstall**, same as `plugins/`.

## Troubleshooting

**The task stops halfway through and the model outputs nothing?**

First check the terminal for a `⚠` / `✗` line — AOX no longer "ends silently". If something really
is wrong, work through this order:

1. **Thinking-mode models**: models like `deepseek-flash` and `deepseek-reasoner` return
   `reasoning_content`, which **must be passed back verbatim** when replaying history, otherwise the
   API rejects the second request of every multi-step task with a 400
   (`reasoning_content ... must be passed back to the API`). AOX handles this automatically, so you
   don't need to intervene.
2. **`max_tokens` too small**: reasoning tokens count against the output budget too, and exhausting
   it yields an **empty reply**. Set it to `8192` or more in `config.toml`.
3. **Capture the raw traffic**: launch with `AOX_DEBUG_LLM=1 aox` and every request and response is
   appended to `logs/llm.jsonl`, so you can see `finish_reason`, `usage`, empty replies and other
   details directly.

AOX's handling of an "empty reply" is: retry twice with a prompt, then, if it's still empty, report
an explicit error explaining why — it will **not** treat a preamble like "Let me take a look…" as the
final answer.

**In the GUI, "a black python window pops up whenever a plugin is used / initialised"?**

The GUI build uses the `windows` subsystem and **has no console of its own**. If a plugin
subprocess is spawned without `CREATE_NO_WINDOW`, Windows allocates a **windowed** console for
`python.exe` — and the symptom is a handful of black windows flashing up at random. The host has
been fixed (`src/plugin/mod.rs::hide_console`, covering all three paths: restoring plugins at first
launch, self-healing restart after a plugin crash, and `rustc` compilation), so installing the
latest `AOX-GUI-Setup` is enough. A regression check is included:

```bash
python tools/check_no_console_window.py --ab                                  # mechanism self-test, takes seconds
python tools/check_no_console_window.py --app target/release/aox-gui.exe --cwd .
```

Note that a `cargo run` **debug build is itself a console program**, so seeing a black window is
normal and not this bug.

## Directory layout

The program directory (the install directory) auto-generates:

| Path | Description |
| --- | --- |
| `config.toml` | Your configuration (auto-generated on first run) |
| `plugins/<name>/` | Plugins the model wrote itself, including source and `plugin.toml` |
| `plugins/sdk/` | The plugin SDK and protocol docs the host provides, refreshed on every launch |
| `history/*.jsonl` | Conversation history, one file per conversation (see [Conversation history](#conversation-history)) |
| `RULES.md` | Optional. Project rules written here are injected into the system prompt |
| `logs/llm.jsonl` | Optional. Records raw requests/responses only when launched with `AOX_DEBUG_LLM=1`, for diagnosing model-side problems |

Plugin subprocesses are started with **the install directory as their working directory**, and
Python plugins are forced to UTF-8 output by the host.

## System requirements

Platform: Windows 10 / 11 (x64).

The host itself (`aox.exe` / `aox-gui.exe`) **needs no external runtime**: releases are statically
linked against the MSVC runtime, so they neither require the VC++ Redistributable (there is no
`VCRUNTIME140.dll` in the import table) nor Python / Node / Rust.

External runtimes affect only **plugins**, and the three languages have different requirements:

| Plugin language | Running an existing plugin | Creating a new plugin (`create_plugin`) |
| --- | --- | --- |
| python | needs a working `python` (3.8+) | same as the left |
| node | needs a working `node` | same as the left |
| rust | **needs no environment at all** | needs `rustc` (no longer needed once compiled) |

In other words, only the python / node plugins already in `plugins/` require their interpreter to be
present; a rust plugin is a compiled native exe that the host runs directly, so it works without the
Rust toolchain installed.

### Where interpreters come from: three-tier resolution

At startup the host resolves each language (python / node / rustc) once, in this order, **taking the
first hit**:

1. A **full path** written under `[runtimes]` in `config.toml` → used directly, with no substitution;
2. The field is a **bare command** (the default is `python` / `node` / `rustc`) and a bundled runtime
   exists at `<app root>/runtime/<language>/<executable>` → the bundled one is used;
3. Still nothing → the bare command is handed to `PATH`.

Tier 2 is the important one: **drop a runtime into the app directory and it takes effect
automatically** — no config change, and nothing written to the system `PATH` or user environment
variables (it disappears on uninstall).

### Assembling the Python runtime

With no interpreter present the program still starts, can hold a conversation, and can use local
commands like `/tools` and `/plugins` — but python plugins fail to load, and the startup banner says
so explicitly. To supply it:

```
/runtime                      # view the three runtimes' status and origin (with version numbers)
/runtime install python       # assemble the Python runtime into <app root>/runtime/python
/runtime dir                  # print the runtime directory
```

`install`'s order is: **already present → local offline package → download**.

- **Offline package**: the filename is fixed at `aox-python-runtime.zip`, and it is auto-discovered
  when placed **next to the exe**, in the app root, in `<app root>/runtime`, or under
  `%LOCALAPPDATA%\AOX`. `bash tools/build_installer.sh pack` produces this package (about 10 MB),
  suited to intranets / offline distribution.
- **Download**: by default it fetches CPython's official embeddable distribution (about 10 MB) and
  verifies it against a built-in official SHA-256. A self-hosted mirror can be substituted:

  ```toml
  [runtimes]
  python_url    = "https://your-mirror.example.com/python-3.13.7-embed-amd64.zip"
  python_sha256 = "<the official or self-built package's SHA-256>"
  ```

- **It takes effect immediately** after assembly (an internal re-resolution happens): python plugins
  created afterwards use it directly, and a plugin that previously failed to load for want of python
  recovers after a restart.
- The whole process uses only the system's existing capabilities: the download goes through
  `reqwest`, extraction through the system `tar.exe` (Win10 1803+), and verification through
  `certutil`. The host **added no third-party dependency for this**.
- A single Node binary is over 80 MB, so it isn't suited to automatic assembly.
  `/runtime install node` says so outright; install it yourself and make sure `node` is on `PATH`.
- A plugin's **third-party pip dependencies** must be installed by you. Note that the official
  "embeddable distribution" **does not include `pip`**: if you really need `pip`, install the full
  Python and point `[runtimes] python` at it.

### Release artifacts (one build produces them all)

There is only one script per installer in `installer/*.iss`, using conditional compilation to
produce two variants (no second script is maintained). `bash tools/build_installer.sh` **with no
arguments produces all five files below in one go**:

| Artifact | Contents | Size |
| --- | --- | --- |
| `AOX-Setup-<version>.exe` | command-line build, host only | about 3 MB |
| `AOX-GUI-Setup-<version>.exe` | graphical build, host only | about 4 MB |
| `AOX-Setup-<version>-with-python.exe` | command-line build + Python runtime (works offline) | about 11.4 MB |
| `AOX-GUI-Setup-<version>-with-python.exe` | graphical build + Python runtime (works offline) | about 12.5 MB |
| `aox-python-runtime.zip` | runtime component package, for offline assembly by main-package users | about 10.4 MB |

```bash
bash tools/build_installer.sh                    # default: everything above
bash tools/build_installer.sh all none           # only the two main packages (fastest)
bash tools/build_installer.sh all with-python    # only the two bundled variants
bash tools/build_installer.sh cli                # the command-line build only (both variants)
bash tools/build_installer.sh pack               # only produce the runtime component package
```

The script passes the version to ISCC as `--define=AppVersion=…`; **do not change it to the old-style
`/DAppVersion=…`**: ISCC treats arguments beginning with `/` as options, and Git Bash (MSYS) rewrites
such arguments as Unix paths before launching a native program (`/DAppVersion=0.1.0` →
`<Git install dir>/DAppVersion=0.1.0`), so ISCC doesn't recognise it as an option and reports
`You may not specify more than one script filename.` `--define=` doesn't begin with `/` and is
immune to this rule; the same applies when compiling by hand, see the header comment of
`installer/*.iss`.

The two `-with-python` variants differ from the main package **only in whether `runtime/python` is
installed into the app directory along with it**; the host discovers it automatically (tier 2 of the
three-tier resolution).

## Uninstall

Use "Settings → Apps" or the uninstall entry in the Start menu. Uninstalling deletes
`<install dir>/runtime` (the runtime assembled on demand or bundled in — machine-specific and
rebuildable at any time), but **does not delete `plugins/` or `history/`** — the former is the
plugins the model accumulated, the latter your conversation records, and both are user data. If you
really no longer need them, delete the install directory by hand.

---

ISC License · <https://2am.top>
