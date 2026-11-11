# AOX documentation index

[← README](../README.md) · [中文](README.zh-CN.md)

Two kinds of writing live in this repo:

- **`docs/`** — the design record: how the architecture got to where it is, what each seam is, what
  is decided and what is still open. **English is the source language**; most documents have a
  `*.zh-CN.md` copy alongside them.
- **`.workbuddy/memory/`** — the working notebooks: the operational detail you read *before*
  touching a given area (pitfalls, current shapes, historical traps). Chinese only.

Start with the **roadmap** if you want the whole picture; start with the **notebooks** if you are
about to change code.

---

## Start here

| Document | What it covers |
|---|---|
| [`USAGE.md`](USAGE.md) | **The manual** — install, model setup, GUI layout, history on disk, runtime resolution, release artifacts, troubleshooting |
| [`ARCH-ROADMAP.md`](ARCH-ROADMAP.md) | **The why** — the seven design principles and the stage-by-stage plan they produced |

## 1. Design record (`docs/`)

| Document | What it covers | Status | 中文副本 |
|---|---|---|---|
| [`ARCH-ROADMAP.md`](ARCH-ROADMAP.md) | **The roadmap** — the seven design principles, stages 1–7, and the **full size history** of every build | living document; stages 1–6 accepted, 7a/7b awaiting acceptance | [✓](ARCH-ROADMAP.zh-CN.md) |
| [`SANDBOX.md`](SANDBOX.md) | **The fence line** — what a plugin subprocess is actually blocked from doing right now, and what blocking more would cost (the ①/②a/②b/③/④ option comparison) | ① Job Object + ③ workplace boundary implemented, awaiting acceptance; ②a / ②b / ④ not built | [✓](SANDBOX.zh-CN.md) |
| [`COMPONENT-CONTRIBUTIONS.md`](COMPONENT-CONTRIBUTIONS.md) | **Stage 6b** — plugins are no longer just "add a tool": how components contribute hooks, themes, prompts and tools | shipped | [✓](COMPONENT-CONTRIBUTIONS.zh-CN.md) |
| [`I18N.md`](I18N.md) | **Internationalisation** — one language, one distribution: the `t()` / `tf!` pipeline, `AOX_LANG`, the catalogue rules | shipped (copy phase complete) | [✓](I18N.zh-CN.md) |
| [`WEB.md`](WEB.md) | **The web tool** — `web_fetch`: reach (`[web] egress`), readable extraction, cursors, and what it is *not* (a network boundary) | M2 + prompt shipped; finished — the cache, the JS renderer and `web_search` were each scoped and dropped (D5, D10, D11) | [✓](WEB.zh-CN.md) |
| [`UI-CUSTOMIZATION.md`](UI-CUSTOMIZATION.md) | **Charter**: the UI seam — element ids, property overrides, named slots, sandboxed patches | **chartered, deferred** (2026-10-03) | [✓](UI-CUSTOMIZATION.zh-CN.md) |
| [`SUBAGENT.md`](SUBAGENT.md) | **Charter**: subagents — self-spawning a headless instance, the protocol, the three isolation axes, the auth policy | **chartered, deferred** (2026-10-09) | [✓](SUBAGENT.zh-CN.md) |

Diagrams (self-contained HTML, open in a browser):

| File | What it shows | Language |
|---|---|---|
| [`TOOL-PIPELINE.html`](TOOL-PIPELINE.html) | the tool **execution pipeline** | 中文 |
| [`TOOL-PIPELINE-TOPO.html`](TOOL-PIPELINE-TOPO.html) | the tool execution **topology** | English |

## 2. The roadmap's stages at a glance

From the Progress table in [`ARCH-ROADMAP.md`](ARCH-ROADMAP.md#progress). The first six stages are
the **extensibility** axis; stage 7 onward is the **security-boundary** axis.

| Stage | Theme | Status |
|---|---|---|
| 1 | prompt segmentation (the prompt seam) | ✅ accepted 2026-10-03 |
| 2 | theme and appearance as data (the theme seam) | ✅ accepted 2026-10-03 |
| 3 | storage and policy seams (store seam + policy seam) | ✅ accepted 2026-10-03 |
| 4 | loop hooks (the loop seam, first form) | ✅ accepted 2026-10-03 |
| 5 | layered composition manifest (the composition layer) | ✅ accepted 2026-10-03 |
| 6a | the whole loop replaceable | ✅ accepted 2026-10-03 |
| 6b | component contributions | ✅ accepted 2026-10-03 |
| — | UI customization layer (formerly 6c) | ⏸ chartered, deferred |
| 7a | plugin fence · ① Job Object | ⏳ awaiting acceptance |
| 7b | workplace boundary · ③ file capability back to the host | ⏳ awaiting acceptance |
| — | subagents (self-spawn a headless instance) | ⏸ chartered, deferred |

Legend: ✅ accepted · ⏳ implemented, awaiting acceptance · ⏸ chartered, deferred · 🚧 not started

## 3. Working notebooks (`.workbuddy/memory/`, Chinese)

These are the operational layer — read the relevant one before editing the corresponding area.

| Notebook | Read it when |
|---|---|
| [`MEMORY.md`](../.workbuddy/memory/MEMORY.md) | **always** — the distilled long-term notes: progress, the rules that must never be broken, the i18n conventions, the change-safety checklist |
| [`SANDBOX.md`](../.workbuddy/memory/SANDBOX.md) | touching the sandbox, the GUI overlays, or anything in `src/sandbox.rs` / `src/workplace.rs` |
| [`SEAMS.md`](../.workbuddy/memory/SEAMS.md) | touching any of the seven seams — the "what you must know before changing a seam" detail layer |
| [`TRAPS.md`](../.workbuddy/memory/TRAPS.md) | hitting something odd — GUI/egui 0.33 traps, the runtime dispatch traps, **the history file format**, token accounting |
| `YYYY-MM-DD.md` | tracing *when* and *why* something changed — the append-only daily log |

## 4. Conventions

- **Bilingual**: English is the source for every `docs/` file; the `*.zh-CN.md` copy must be kept in
  step. The notebooks under `.workbuddy/memory/` are Chinese only.
- **Newlines**: after editing, run `tools/fix_newlines.py` (restores CRLF/LF to match `HEAD`).
- **Encoding**: scan for U+FFFD after touching any Chinese file — `grep -rInP '\x{FFFD}' src gui/src docs`.
- **Rhythm**: roadmap → confirm → implement → accept → next stage. **No automatic commits** during
  the architecture-refactor period; the user commits by hand per stage.
- **Zero drift**: every stage must leave the default configuration behaving exactly as before, and
  each one is accepted before the next begins.
