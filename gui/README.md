# aox-gui

The graphical front-end for AOX. It is **not** a new implementation: it is a "view + input"
layer over the existing core library.

## Why there is zero fork

`src/lib.rs` exposes every core capability the CLI uses as a library (`aox`):

```
aox::agent      the agent main loop (ask the model -> run tools -> fill results back -> until no more tool calls)
aox::llm        the LlmProvider trait / ChatReply / StreamSink (streamed reasoning)
aox::plugin     the plugin host (subprocess + stdio JSON-RPC)
aox::runtime    Runtime (tool registry / authorization policy / plugin handles)
aox::tool       meta tools and the tool abstraction
aox::ui         the Ui trait + AgentEvent  <- the GUI only has to implement this one trait
aox::paths      application root resolution / writability / plugin migration
```

`src/main.rs` (the CLI) does not reference this lib; each module is compiled once more from its
own crate root. So **the CLI's behaviour is byte for byte equivalent to before the GUI existed**,
and no regression pass is needed.

## Threading model

```
┌────────────────────────┐   ToAgent   ┌──────────────────────────────┐
│ egui main thread       │ ──────────▶ │ background thread: tokio runtime │
│  eframe::App (app.rs)  │             │  bridge::boot_and_run        │
│  pump() per frame      │ ◀────────── │   aox::agent::Agent::run_turn│
└────────────────────────┘  FromAgent  └──────────────────────────────┘
```

- **Both directions use `tokio::sync::mpsc::unbounded_channel`.** `UnboundedSender` is
  `Send + Sync + Clone`, which is exactly what the `StreamSink: Send + Sync` bound needs; the
  main thread drains events with `try_recv()` without blocking (no runtime context required).
- **Authorization** uses a `oneshot`: `GuiUi::authorize()` stuffs the `oneshot::Sender` into an
  `AuthRequest`, sends it to the UI, then `await`s the result. Clicking a button, or pressing
  `y|a|n`, sends it back.
- **Interrupts** use `Arc<SessionFlags>` (an `AtomicBool`) polled every 50ms, not `Notify`. The
  reason is that a `Notify` permit survives across turns: pressing stop once while idle would
  interrupt the **next** turn immediately. Polling has no perceptible latency at this scale, and
  the semantics are deterministic.
- **Shutdown** happens in `Drop for App`: `request_abort()` -> send `Shutdown` -> bounded wait
  of 1.5s. `aox::plugin`'s subprocesses are `kill_on_drop(true)`, so plugins are cleaned up on a
  normal exit.

## Visual design: paper and ink

The palette has **lightness only**, no hue. Both palettes live in `theme.rs`:

| | Paper (light) | Ink (dark) |
| --- | --- | --- |
| background bg | `#F2EFE8` | `#141412` |
| panel | `#EBE7DE` | `#191917` |
| card surface | `#FAF8F3` | `#20201D` |
| body text | `#1A1916` | `#EAE7DF` |

Two conventions run through everything:

- **Semantic state is not carried by colour.** The difference between success / warning / failure
  is carried by **lightness levels** (`ActKind::color()` is really an "emphasis" scale) plus
  **reversed blocks**: an error card is reversed wholesale (ink-black with white text in the
  light theme, near-white with black text in the dark one), and timeline nodes are told apart by
  "filled dot / hollow ring / crossed". One set of drawing code then serves both themes, and
  it is colour-blind friendly by construction.
- **The palette must be evaluated at runtime.** Accessors like `P::bg()` read the current mode
  out of an `AtomicU8` every time, so switching themes takes effect immediately; written as
  `const Color32` they would be frozen at compile time and switching would do nothing. The
  preference lands in `<app root>/.aox-gui-theme` (contents `light` / `dark`).

The toggle sits at the far right of the button row: a crescent in light mode, a sun in dark
mode. `theme::sync_native_theme()` pushes `ViewportCommand::SetTheme` down to the window
manager so the **native title bar** changes with it -- otherwise a paper-white interface would
sit under a black title bar.

## Events -> interface

`AgentEvent` (the same source as the CLI) lands in `app.rs::on_event` as two kinds of state:

- **The transcript** `items`: user bubbles / assistant Markdown / collapsed process blocks /
  notices. At the end of a turn, `finish_turn()` collapses the in-progress `Live` into a
  `Process` (collapsed by default) and then appends `pending_final` as the final answer
  **after** it -- the order must not be inverted.
- **The activity stream** `acts`: every event appends one row (with a relative timestamp), and
  the side panel renders them in order. Streamed reasoning is one `live` row: deltas are
  appended in place and it freezes when the response ends.

## Traps (read before touching this crate)

1. **`Ui::horizontal_centered()` eats the whole window.**
   Internally it is `allocate_ui_with_layout_dyn(available_size_before_wrap(), ..)` with
   cross-align `Center`, which stretches the child ui to "the full available height". Inside an
   **auto-height** `TopBottomPanel` that forms a positive feedback loop: the panel records the
   last content height -> the content grows with the panel -> the panel grows again, until the
   bottom bar has swallowed the entire window. Use `ui.horizontal()` (initial height is a single
   row) instead.
2. **A `TopBottomPanel` with no fixed height remembers the last content height**
   (`PanelState`), so anything whose content height depends on the available height runs away.
   Give it an explicit `min_height` / `exact_height` where necessary.
3. **Screenshot verification must be DPI aware.** The first thing `tools/gui_shot.ps1` does is
   `SetProcessDPIAware()`: otherwise the `GetWindowRect` a non-DPI-aware PowerShell sees is in
   virtualized coordinates while `CopyFromScreen` grabs physical pixels, and the screenshot
   comes out offset as a whole. To get only the target window you also need
   `SetWindowPos(HWND_TOPMOST)`, or you capture whatever program happens to be on top.
4. **`egui::LayoutJob` lives at `egui::text::LayoutJob`**, not at the crate root.
5. **Fonts**: egui's built-in font has no CJK glyphs (Chinese renders as empty boxes).
   `theme.rs` injects a CJK font from the system font directory and inserts it **before** the
   emoji font (egui's built-in `emoji-icon-font` grabs a slice of CJK codepoints). Another family
   is registered for true bold, otherwise `**bold**` can only be faked with colour.
6. **Versions**: `eframe 0.33` is the highest version usable with the current rustc 1.90
   (0.34+ requires 1.92/1.95).
7. **"The card is clipped" is almost always a width that wasn't pinned.** A `ScrollArea`'s
   `available_width` **includes the scrollbar's reservation**, so a child that sizes itself by
   "the text's ideal width" overflows the panel and gets clipped. The rule: on entering a panel,
   do `let w = ui.available_width(); ui.set_width(w); ui.set_max_width(w);` and pass `w`
   downward; children inside a frame must derive from the **frame's own**
   `ui.available_width()`, not from the value passed in from outside (and subtract their own
   `inner_margin` first). Don't forget to subtract column spacing when sizing table columns.
8. **Specify the window position explicitly; don't leave it to the OS.** Windows "cascades"
   new windows toward the bottom-right, and the window's bottom edge can cross the screen's
   bottom -- the user can't see the input box, and screenshots also pick up a **black band**
   (`CopyFromScreen` captures off-screen area as black, which looks like a rendering bug but
   isn't). `app.rs::fit_window()` uses `ViewportCommand::OuterPosition` to centre it and
   computes the outer frame size from the decoration thickness measured as
   `outer_rect - inner_rect`. To diagnose, just measure: compare the window's `Bottom` with the
   screen height.
9. **The native title bar isn't egui's.** It is drawn by DWM and follows the system theme by
   default. To sync it you have to send `ViewportCommand::SetTheme`; note that a command issued
   while the window is being created can be dropped before the front-end event loop comes up, so
   `App` keeps a `native_theme: Option<Mode>` -- sent once on the first frame for sure, and
   after that only when it changes.
10. **The real culprit behind "long text doesn't wrap and gets clipped at the right edge" is
    `Ui::wrap_mode()`.** Its order of decision is `style.wrap_mode` -> `placer.grid()` ->
    `layout()`. That last branch returns `Wrap` only for
    `layout.is_vertical() || layout.is_horizontal() && layout.main_wrap()`, and the
    `Layout::left_to_right` built by `ui.horizontal()` / `horizontal_top()` has `main_wrap` set
    to **false** => inside a horizontal layout it always returns **`Extend`**.
    `Label` defaults to `wrap_mode = None` and inherits it, so `Label::layout_in_ui` overwrites
    `LayoutJob::wrap.max_width` with `f32::INFINITY` -- the width we computed ourselves is
    thrown away on the spot, and the text runs to the end of the line, past the column's right
    edge, and gets clipped by the `ScrollArea`. Bubbles, list items, tool-call rows and table
    cells are all horizontal layouts internally, so it bites everywhere.
    The fix (two layers):
    - set `ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap)` once at the **column root**
      (the transcript column / the activity-stream panel); descendant Uis (including inside a
      `Frame`) inherit it;
    - in `markdown.rs`, route every text label through `wrap_label()`, which applies
      `.wrap_mode(TextWrapMode::Wrap)` as a backstop.
    Note `break_anywhere` is **not** needed: epaint's line-break candidates are
    `word_boundary -> dash -> punctuation -> any`, and that last `any` already catches
    over-long tokens with no spaces.
11. **Measure "is it clipped" with assertions, not with your eyes.** `markdown.rs` carries a
    headless unit test, `text_never_exceeds_the_column`: it uses `egui::Context::default()` plus
    `RawInput.screen_rect` to run the real measure skeleton (CentralPanel -> ScrollArea ->
    horizontal -> add_space -> vertical) and asserts
    `ui.min_rect().width() <= inner`. After a layout change, `cargo test -p aox-gui` reproduces
    or guards against the regression an order of magnitude faster than repeatedly screenshotting.
12. **"A black python window pops up the moment a plugin runs" is the host spawning the
    subprocess wrong, not the plugin's fault.** `aox-gui.exe` is built with
    `windows_subsystem = "windows"` and has **no console of its own**; spawning a console program
    (`python.exe` / `node.exe` / `rustc.exe`) then, without `CREATE_NO_WINDOW`, makes Windows
    **allocate a fresh console with a window for it**. The fix is one line:
    `src/plugin/mod.rs::hide_console()` adds `creation_flags(0x0800_0000)` to `Command`, and
    **every** spawn path must go through it (`spawn_command` covers first launch, `respawn`
    covers self-healing, and `rustc` compilation is a separate site).
    Measured (`tools/check_no_console_window.py --ab`, parent process `DETACHED_PROCESS`):

    | child creationflags | child's `GetConsoleWindow()` | new console windows |
    |---|---|---|
    | `0` (control) | non-zero (e.g. 2952730) | **1** |
    | `CREATE_NO_WINDOW` | `0` | **0** |

    Two directions that are easy to waste time on:
    - **`subprocess` inside a plugin needs no attention.** `CREATE_NO_WINDOW` gives the plugin
      process a "console with no window", and when the plugin then runs `cmd.exe` with
      `shell=True`, the grandchild inherits that same windowless console => still 0 windows
      (`plugins/probe_runtime` and `sysinfo_pro` are written exactly this way, and measure
      clean). What actually leaks is **the layer where the plugin process itself is spawned**.
    - **Don't add armour plating to the plugin SDK**; it is dead code -- unless the host's flag
      is ever changed from `CREATE_NO_WINDOW` to `DETACHED_PROCESS` (which is "no console at
      all", and grandchildren would grow one again).
    - Mind `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`: a **debug build
      is itself a console program**, so seeing a black window under `cargo run` is normal --
      don't go fix it.

## Development-time verification

```bash
# 1) stand up a fake OpenAI-compatible endpoint (streams, emits reasoning_content, calls tools)
python tools/mock_llm.py 8791

# 2) start with the test config and send one instruction automatically
target/debug/aox-gui.exe --config gui/test-config.toml --plugins gui/test-plugins \
    --prompt "which plugins and tools are there right now?"

# 3) screenshot (Windows, pick one)
python tools/gui_shot.py --out gui/shot.png --w 0 --h 0        # recommended: ctypes into user32, no PowerShell
powershell -ExecutionPolicy Bypass -File tools/gui_shot.ps1 -Out gui/shot.png -W 0 -H 0

# 4) layout regression (no window needed)
cargo test -p aox-gui --bin aox-gui

# 5) black-window regression (plugin subprocesses must not pop a console)
python tools/check_no_console_window.py --ab                          # mechanism self-check, seconds
python tools/check_no_console_window.py --app target/release/aox-gui.exe --cwd .
```

`tools/check_no_console_window.py`'s end-to-end mode starts the exe with `DETACHED_PROCESS`
(equivalent to "double-click / Start menu"), enumerates `ConsoleWindowClass` top-level windows
while it runs, and also counts `python.exe` processes so that "the plugin never started, so
there was no window" can't produce a false pass.

`tools/gui_shot.py` and `.ps1` are equivalent (`MoveWindow` + `SetForegroundWindow` +
`HWND_TOPMOST` + screen grab + `SetProcessDPIAware`), but the `.py` calls `user32` directly via
`ctypes`, unaffected by PowerShell's execution policy (`Add-Type` gets blocked), and on failure
it prints the window title / DPI / logical size so you can align coordinate systems.

`gui/test-config.toml` sets `meta_tools` to `ask`, for exercising the authorization dialog;
`gui/test-config-auto.toml` sets it to `auto`, for running a whole flow in one go.
Use `--demo` when you only want to see the interface in full and don't want to connect a model.

A few efficiency notes when changing the interface:

- **`-W 0 -H 0`** means "keep the window's current size". Passing non-zero goes through
  `MoveWindow`, and resizing the GL surface easily produces white strips or garbled output.
- **Start the GUI itself as a background task** (don't `nohup … &` and return) -- the process
  gets reaped when the bash task ends, and the screenshot script can then only report
  `NOTFOUND` / `BADRECT 0 x 0`.
- `taskkill` the old instance before rebuilding, or linking fails with
  `LNK1104: cannot open aox_gui.exe`.
- **A PowerShell tool's stdout is swallowed in some environments** (you get back only
  `Command completed with exit code 0`). Have the script `Set-Content` a file instead, and read
  the result from the file.
- **`Add-Type` (P/Invoke) can be blocked by the security layer.** For measuring window/screen
  geometry, switch to Python `ctypes` calling `user32` directly -- less faff than working around
  it in PowerShell, and PIL is right there for a pixel probe to cross-check.
- To judge whether something is clipped, **don't rely on the screenshot thumbnail** -- scaling
  creates the illusion of "the left side is cut". Probe that column of pixels instead.
- To verify that a light/dark switch really took effect, confirm the window has focus and then
  do a real `SetCursorPos` + `mouse_event` click, and check whether `.aox-gui-theme` flipped.

## Status

The threading model, palette and egui traps above still hold. The **interface** has moved on since
this file was written: the top strip now carries only the drawer button, the status pill and the
window buttons, Settings lives in the drawer header, and the activity stream / cache hit map /
workspace view are three separate windows. For the current behaviour see
[`../docs/USAGE.md`](../docs/USAGE.md#graphical-interface).
