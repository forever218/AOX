# AOX architecture openness roadmap

> Goal: turn the four things that are **welded shut at compile time** — **system prompt /
> interface appearance / storage / main loop** — into replaceable capability seams, and
> introduce a layered composition manifest, so that appearance customization and "the AI can
> steer it" have one unified entry point.
>
> Where the idea comes from: DSH (DeepSeek Harness)'s "layered composition + reversible
> registration + capability seams".
> **Its plugin shape is not being copied** — DSH is in-process components plus Node/TS, while
> AOX is subprocess plugins plus a single Rust exe; the granularity is an order of magnitude
> apart. What can be borrowed is *how things compose*, not *how they are implemented*.

- **Baseline commit**: `cc8627b 修改图标` (branch `main`, clean working tree)
- **Start date**: 2026-10-03
- **Current status**: stages 1–5 are all accepted (stage 5 passed on 2026-10-03; four items were
  carried out as recommended, including the container-level `#[serde(default)]` added to
  `[auth]` / `[agent]` / `[runtimes]`, which the user has confirmed they accept).
  **Stage 6 has a roadmap and is waiting on your confirmation — no cutting until it is
  confirmed.**
  On 2026-10-03 the user decided: **the original stage 6 "UI customization layer / pet-like
  sandbox" moves out, is chartered separately and deferred**
  → see **`docs/UI-CUSTOMIZATION.md`**; stage 6 therefore narrows to **6a replaceable loop +
  6b component contributions**.
  **Items A / B / C are settled by the user ("do everything as you recommend"); 6a is
  implemented and awaiting your acceptance.**

---

## Seven design principles (every stage obeys all of them)

| # | Principle | Why |
|---|---|---|
| **P1** | **A single normalized config tree**: each stage only adds fields to the same tree; stage 5 doesn't swap the model, it only turns "source" from 1 layer into N | The key to the whole roadmap being incremental and rework-free |
| **P2** | **A uniform shape for seams**: `trait` (interface) + `Provider` (implementation) + `Consumer` (doesn't know who provides it) + `Registration` (reversible handle; Drop unregisters) | Aligned with Cordis's "reversible side effects"; Rust's `Drop` fits naturally |
| **P3** | **The kernel is not pluginized**: `Runtime` and the loop skeleton stay natively compiled | Protects the core selling point: "single exe, starts with no interpreter at all" |
| **P4** | **Size discipline**: the default build (zero features) grows the exe by ≤ 300 KB per stage; any new dependency must be switchable off by feature | Guards the "a few MB, one file" physique |
| **P5** | **Two cuts per stage**: first "extract the seam but don't change behaviour" (pure refactor, byte-level regression possible), then "wire up the new provider / override" | Pushes the risk as low as it goes |
| **P6** | **Every stage is independently shippable and stoppable**: at the end of any stage a release can be built, with nothing half-finished | Lets you call a halt or reorder at any time |
| **P7** | **Authorization is unified**: any "capability request" a plugin introduces is folded into `AuthPolicy` (ask/auto/deny) | Openness has to be controllable first |

---

## Stage overview

| Stage | Deliverable | Hard dependency | Risk | Size impact |
|---|---|---|---|---|
| 1 prompt segmentation | `src/prompt.rs` + `/prompt` + `[prompt]` config | — | low | ~0 (even slightly negative) |
| 2 theme/appearance as data | `theme.toml` + `[theme.*]` config | — | low | ~0 |
| 3 storage & policy seams | `Store` / `Policy` traits | — | low-medium | ~0 |
| 4 loop hooks | `Hook` trait + 5 hook points | 3 (capability bit) | medium | < 30 KB |
| 5 layered composition manifest | `aox.toml` + `--dump-config` | 1–4 | medium | < 80 KB |
| 6 replaceable loop + component contributions | `AgentLoop` trait + component contributions (hook / prompt segment / theme) | 4, 5 | medium-high | **0** (subprocess route) |

**Critical path**: `1 → 4 → 5 → 6`; stages 2 and 3 are independent of 1 and **may be reordered
freely**.
🚩 **Stage 6 originally carried one additional topic the user named** (the "UI customization
layer / pet-like sandbox") — **on 2026-10-03 the user decided to move it out, charter it
separately and defer it** (`docs/UI-CUSTOMIZATION.md`), so stage 6 **no longer contains the UI
customization layer and no longer touches size or attack surface**.
🚩 **Stage 6 also carries a "stage 4 leftover list"** (4 items, including **pruning /
rewriting history**, which the user specifically named as not to be forgotten); it sits at the
end of stage 6 and is **cashed in item by item** when we reach that stage (already allocated to
6a / 6b).

---

## Stage 1 · Prompt segmentation (the prompt seam)

**Goal**: turn `SYSTEM_PROMPT` from a compile-time constant into a "segment list", supporting
built-in segments / user override segments, reorderable, disableable and appendable.

**Current state** (`src/agent.rs`): `const SYSTEM_PROMPT` (about 3 KB of text, sitting directly
in the binary's rodata) + `env_section(rt)` appending the real paths + `Agent::new(..., rules)`
appending RULES.md — three segments hard-spliced together.

**Changes**
- New `src/prompt.rs`: `PromptSegment { id, title, body, order, enabled }` plus
  `PromptBuilder` (collect → sort → splice).
- `Runtime` holds the segment registry; at startup it registers 2 **built-in** segments `core`
  / `protocol`; plus 2 **runtime-generated** segments `env` (runtime environment) / `rules`
  (RULES.md), 4 in total.
- `config.toml [prompt]`: `order = [...]` (reorder), `disable = [...]` (disable built-ins),
  `[[prompt.segment]]` (append custom segments, insertable anywhere).
- `Agent::new` calls `PromptBuilder::build(&rt)` instead.
- New CLI command `/prompt`: prints the current assembled result plus each segment's source and
  order.
- Side benefit: moving several thousand characters of prompt out of the binary makes the
  **size go slightly down**.

**The behaviour-unchanged baseline (this stage's hard acceptance metric)**
Under the default configuration the assembled system prompt must be **byte for byte identical**
to today's (a unit test pins the `sha256`).
The reason isn't just tidiness — **the server-side prompt cache hit rate depends on the prefix
bytes being stable**; change the prefix and the whole cache is void, which costs money
directly.

**Acceptance criteria**
1. `cargo test --workspace` all green, and the new "default output is byte-identical" unit test
   passes.
2. Editing `config.toml` can reorder built-in segments, disable one, and insert a custom
   segment anywhere.
3. `/prompt` shows each segment's title, order and source (built-in / config / RULES file).

**Risk**: low. The only trap is the cache-prefix baseline above.

**Implementation log (2026-10-03)**
- The prompt text and `env_section` were **carried over verbatim from `git HEAD` by script**,
  not retyped; a separate verification script cross-checks them: 1853 / 777 characters, byte
  for byte identical to HEAD (not self-certified by a unit test).
- The built-in constant is split at runtime on `"\n\n# 插件协议"`. The `split_is_lossless` test
  **caught a bug on its first run**: the split was swallowing the section heading `# 插件协议`
  too — exactly why we refused to split the string by hand in the first place.
- Duplication removed along the way: reading `RULES.md` used to be written twice, in
  `src/main.rs` and in `gui/src/bridge.rs`; it now lives in `prompt` alone, and `Agent::new`
  went from three parameters to two.
- All three `config.toml [prompt]` capabilities (`order` / `disable` / `[[prompt.segment]]`)
  have been verified against a real binary: `order=["env"]` works, `disable=["protocol"]` works
  and is counted in the "segments not adopted" notice, and appending a segment works; segment
  character counts reconcile item by item.
- When a `[[prompt.segment]]` id matches a built-in segment it **replaces that built-in segment
  wholesale** — aligned ahead of time with stage 5's "later layers override earlier ones"
  semantics.
- 72 tests, all green (7 new); `cargo check --workspace --all-targets` zero warnings; no
  U+FFFD in changed files.
- Leftover: `Runtime::add_prompt_segment` has no caller yet (stage 6's plugin contributions will
  use it); it is marked `#[allow(dead_code)]` with the reason noted, per the repo's existing
  convention.

---

## Stage 2 · Theme and appearance as data (the theme seam)

**Goal**: turn the GUI's appearance from a `Palette` constant table into overridable data.

**Current state** (`gui/src/theme.rs`, 686 lines): `const LIGHT` / `const DARK: Palette`
(19 colour fields); `install_style` maps the Palette into egui's `Visuals`. Sizes and fonts are
equally hard-coded constants, and the font code only honours 5 environment variables
(`AOX_GUI_FONT*`).

**Changes**
- `Palette` gains `Serialize/Deserialize`; new `theme.toml` (or `config.toml [theme]`)
  supporting **field-by-field overrides**.
- `[theme.metrics]`: `body_size` / `card_radius` / `item_spacing` / `button_padding` and the
  other existing constants.
- `[theme.fonts]`: font paths become config items (environment variables stay as a fallback).
- Everything lands in P1's normalized config tree → after stage 5 it can be overridden by the
  patch layer.
- 2–3 bundled example themes (paper / ink / high contrast).

**Acceptance criteria**
1. Editing `theme.toml` takes effect on restart: change colours, sizes, fonts, no recompilation.
2. The two existing built-in themes (paper / ink) are **visually identical** to today
   (screenshot comparison).
3. Note: `Palette` is defined in the `gui` crate, and reading config goes through `aox::paths`
   — **don't drag an egui dependency into the core `aox` library**.

**Risk**: low.

**Implementation log (2026-10-03)**
- Config lands in `config.toml [theme]` (symmetric with stage 1's `[prompt]`, per P1), plus
  `--theme <file>` which **replaces the whole section** (it also accepts the nested `[theme]`
  form, so an example file can be pasted into config.toml wholesale).
- **Mirrored structure**: `Palette` holds `egui::Color32` (which has no `Deserialize`), and the
  core library may not depend on egui → the core library only holds `PaletteConfig` (18
  `Option<RgbColor>`, hex strings), and the gui side merges it into a `Palette`.
  **18 fields, not 19** (the roadmap's original "19" needs correcting).
- The dozens of `P::xx()` call sites needed **zero migration**: only `pal()`'s data source
  changed, to `ACTIVE: OnceLock<(Palette, Palette)>`; it is still a `&'static Palette`, so the
  cost of reading a colour is unchanged.
- `theme::BODY` / `theme::CARD_RADIUS`, two `pub const`s, became runtime accessors; 12 sites
  migrated (app.rs 6 / activity.rs 1 / markdown.rs 3 / theme.rs internal 2). All 10 metrics went
  into `[theme.metrics]`.
- Light/dark priority (settled): **last manual switch record > `[theme] mode` > default (ink)**.
- **Two real bugs caught, both of the "you wrote the file and it silently did nothing" kind**:
  1. `--theme` initially only accepted a bare theme, while the example files use the nested
     `[theme]` form → load failed and silently fell back to the built-in palette; both forms are
     now accepted, and a `shipped_theme_files_load` test pins the example files.
  2. In TOML, a `mode` written **before** `[theme.light]` lands on the document root, so "you
     wrote `mode` and it did nothing"; the loader now falls back to the root when the table has
     no `mode`.
- **Default zero difference proven pixel by pixel from screenshots** (both paper and ink, 14px
  inset on all four sides and clear of the floating scrollbar band): **0 differing pixels**;
  `--theme themes/paper.toml` / `ink.toml` likewise 0 difference against the built-in palette;
  the high-contrast example differs by 99.94% (1.71M pure black pixels).
- Functionality measured: `[theme.dark] text="#FF0000"` → 34,551 strongly red pixels (baseline
  0); `body_size=20 / card_radius=2 / item_spacing=16` → 24.34% of pixels changed;
  `fonts.cjk = simkai.ttf` → 2.68% of pixels changed (the glyphs really changed).
- **A trap fixed along the way**: `tools/gui_shot.py` matched window titles by **substring**, so
  with both "AOX" and "AOX activity stream" open it grabbed the activity-stream one (which
  happened during this work) → added `--exact`.
- New `tools/gen_themes.py`: **generates** `themes/paper.toml` and `ink.toml` **backwards** from
  `theme.rs`'s `LIGHT`/`DARK`, asserting that field order matches `PaletteConfig` — the whole
  point is to stop the example files drifting from the source.
- Size: **zero new dependencies** (the core library already had toml/serde; gui added none).
- **88 tests all green** (6 new); `cargo check --workspace --all-targets` **zero warnings**;
  no U+FFFD in changed files, line endings unified.
- Leftover (optional, not done): the CLI has no "validate a theme file" entry point —
  `ThemeConfig` isn't used in the bin target and is marked `#[allow(dead_code)]`. A `/theme`
  subcommand could be added, doing parsing plus a field-by-field report.

**Acceptance conclusion (2026-10-03, passed by the user)**
- Accepted with no conditions attached. This stage is closed; stage 3 moves into the
  "give the roadmap → user confirms" step.
- Two leftovers confirmed as acceptable to keep (optional from the next stage onward): ① no
  `/theme` validation entry in the CLI; ② `--theme` and `[theme]` mean "replace wholesale" (not
  a field-by-field merge), which is documented in `config.example.toml`.

---

## Stage 3 · Storage and policy seams (store seam + policy seam)

**Goal**: `history::Log` writing JSONL directly → a `Store` trait; `AuthPolicy`'s three-tier
strings → an extensible policy.

**Current state**: `src/history.rs` (1367 lines) does all I/O against files; `src/auth.rs`
(52 lines) has `level_for(kind, name)`.

**Changes**
- `trait Store`: `append_message / append_act / append_usage / rewrite / load / list / delete`.
- `JsonlStore` = the existing implementation **moved in as-is**, not one line of logic changed.
- `config.toml [store] backend = "jsonl"` (leaves room for a future `sqlite`, **not implemented
  this round**).
- `trait Policy`, `ConfigPolicy` = the existing three tiers; the method signature reserves a
  `caps` (plugin capability request) parameter slot for stages 4/6.

**Acceptance criteria**
1. `cargo test --workspace` all green (not one pre-seam case may be touched: the lib side had 27
   at the time).
2. All `/history` subcommands behave identically. *(Two intentional changes in practice — see
   the end of the implementation log.)*
3. New round-trip unit tests for `Store` reads and writes.

**Risk**: low-medium. `history.rs` is a 1367-line file — **extract the trait first and move the
JSONL implementation as-is, with no opportunistic refactoring**. This point is not negotiable.

**This round's detail (the roadmap given on 2026-10-03 — executed as written, see the
implementation log below)**

Inventory (script-verified): `history.rs` 1367 lines / `auth.rs` 52 lines. Call sites:
`src/agent.rs` (6), `src/main.rs::history_command` (9), `gui/src/app.rs` (5),
`gui/src/bridge.rs` (1). **Key finding: the GUI also calls the free functions directly and
assembles paths itself** (`dir(root)` → `file_of(dir, id)`, see `app.rs` 346/500/546/569 and
`bridge.rs` 352) — that is precisely the evidence of "the jsonl shape leaking into callers",
and the first problem extracting the seam has to solve.

**Cut one (pure refactor, zero behaviour change)**
- New `Store` (directory level) and `Session` (session level = the original `Log`'s `append` /
  `append_act` / `append_usage` / `rewrite`) traits, implemented as `JsonlStore` /
  `JsonlSession`; **existing function bodies moved function by function, not rewritten, no
  opportunistic refactoring**.
- `/history rm` currently calls `std::fs::remove_file` directly in `main.rs` — fold it into
  `Store::remove`, or the delete semantics have nowhere to live when the backend changes.
- `Agent.log`: `Option<Log>` → `Option<Box<dyn Session>>`; `Runtime` holds `Arc<dyn Store>`
  (both the agent thread and the GUI main thread need it).
- `Policy` trait + `ConfigPolicy` (the original `AuthPolicy` moved as-is; the three-tier
  semantics and override priority unchanged); `level_for` reserves the capability slot — the
  parameter takes `&[String]` for now and is promoted to a `Cap` struct in stage 6,
  **avoiding the creation of a type with no semantics today**.

**Cut two (wire up the new entry point, behaviour still unchanged)**
- `config.toml [store] backend = "jsonl"`; **only `jsonl` is recognized, any other value errors
  out at startup** — no silent fallback (stage 2 hit the "you wrote the file and it did
  nothing" trap, and silent fallback is the hardest class to diagnose).
- Optional `/store` subcommand: print the current backend and storage location.

**Acceptance criteria (added to the original three)**
1. `cargo test --workspace` all green (every pre-seam case preserved, **none rewritten or
   deleted**).
2. `/history list|dir|show|open|rm` output identical to before the change (terminal colour
   escapes included). *(Two intentional changes in practice — see the end of the implementation
   log.)*
3. GUI: conversation list / switching / activity stream / usage block behave identically
   (screenshot comparison + activity-stream row count reconciliation).
4. New round-trip unit tests for `Store` (including `rewrite` preserving act rows, `prune`
   trimming, `remove`).

**Three choices that were pending confirmation (all adopted as recommended — see the
implementation log below)**

| # | Choice | Recommendation |
|---|---|---|
| A | `Store` holds the root and methods take no paths (GUI goes through it too) vs keeping `dir`/`path` parameters | **hold the root** — no paths in the signature is what "extracting a seam" means; otherwise the interface keeps leaking jsonl |
| B | split reads/writes into `Store` + `Session` vs one combined trait | **split into two** — the write methods are `&mut self`, and mixing them would force the GUI to take `&mut` |
| C | implementation files: really move them into `src/store/` vs leave `history.rs` alone and add a thin adapter | **really move** — a thin adapter leaves two entry points and the seam was extracted for nothing |

---

### Implementation log (completed 2026-10-03, accepted)

All three choices (A hold the root / B split Store+Session / C really move into `src/store/`)
**were adopted as recommended**; cut one (pure refactor) and cut two (wire up the new entry
point) have both landed.

**Deliverables**

| File | Contents |
|---|---|
| `src/store.rs` (new) | `Session` (write side: `append/append_act/append_usage/rewrite`) + `Store` (directory level: `create/open/list/resolve/load/load_acts/load_usage/remove/prune`); `backend()` factory + `default_backend()` + `default_store()` |
| `src/store/jsonl.rs` (new) | the original `history.rs` I/O **moved function by function as-is** → `JsonlStore` / `JsonlSession`; 16 new unit tests |
| `src/history.rs` | 1367 → 346 lines: only the data model and CLI copy remain; `list_text` now receives **already-fetched data** and no longer reads the disk itself |
| `src/auth.rs` | `Policy` trait + `ConfigPolicy` (the original `AuthPolicy` moved as-is); `level_for` reserves the `caps: &[String]` capability slot; 3 new unit tests |
| `config.example.toml` | new `[store] backend` (default `jsonl`, with a note that "changing backend is a config change") |

**The shape of the seam**: **no path appears anywhere on the interface**. The id↔filename
mapping (`dir` / `file_of`) is a private detail of the jsonl backend. Callers (`Agent` / CLI /
GUI) all speak in terms of **id** — which is exactly what makes a future sqlite backend that
"keeps every conversation in one database" **implementable**. The GUI's own path assembly
(`dir(root)` → `file_of(dir, id)`, the live evidence of jsonl shape leakage) now takes
`BootInfo.store`.

**API change**: `Runtime::new` changed from `-> Arc<Self>` to `-> Result<Arc<Self>>`, so that an
unknown `[store] backend` can error **at startup**. The CLI reports it as a friendly "config
error" and waits for a keypress (the same path as a TOML parse failure, so double-clicking the
exe doesn't just flash a window); the GUI goes through `Fatal`.

**Two intentional behaviour changes** (conflicting with item 2 of this section's original
acceptance criteria, "byte for byte identical" — please confirm you accept them)

1. `/history dir`'s output gains the backend name: `<path> (backend jsonl)`.
   Reason: when sqlite really arrives, "did my config change take effect" should be visible at
   a glance rather than guessed at.
2. `/history show|open|rm` key parsing fixes **an old bug**: a purely numeric date prefix like
   `20261003` used to be parsed as an index (`.parse::<usize>()` succeeds), so it reported
   "there is no conversation #20261003". It now branches on length — **fewer than 8 digits of
   digits = an index, 8 or more = an id prefix** (an index can never have 8 digits).
   This is a pre-existing defect exposed by hands-on verification, not one introduced by this
   refactor.

**Explicitly not done**: the "optional `/store` subcommand" from cut two. `/history dir` now
carries the backend name, which is equivalent information; a second command entry point isn't
needed.

**Verification evidence**

- `cargo check --workspace --all-targets`: **zero warnings** (`gui/` included).
- `cargo test --workspace`: 48 + 48 + 12, all green (the lib and the bin each compile the same
  batch of tests, plus 12 for gui).
- **CLI, hands-on** (isolated directory + hand-made config): unknown backend → friendly error
  then exit (`[config error] unknown storage backend: sqlite`); `/history` list / `dir` /
  `show` / a real conversation landing on disk / `rm` all work, with the new file's contents
  reviewed line by line (meta → user → no system line).
- **GUI, hands-on** (isolated directory + two pre-seeded histories): the sidebar lists both
  correctly (title taken from the real first sentence, count counts messages only, excluding
  act/usage rows); clicking switches into one; messages and the usage block fill back in
  correctly (`#1 · 2 calls · input 30 · total 90`) → proving `Store::list / load / load_usage /
  load_acts` all work through the seam.

**Size**: not measured separately. This change adds **zero new crates** and no new static data;
per P4 it is expected to be within noise; sizes will be reconciled together before stage 5.

**Acceptance conclusion (2026-10-03, passed by the user)**
- Accepted, **including both intentional behaviour changes above** (`/history dir` printing the
  backend name, and the key-parsing ambiguity fix); the user attached no conditions and this
  stage leaves no leftover items. Stage closed.
- Explicitly not to be done, with the user asking for "no next step yet": stage 4. **Don't start
  it yourself next time** — follow the fixed rhythm of giving the roadmap first, then waiting
  for confirmation.

---

## Stage 4 · Loop hooks (the loop seam, first form)

**Goal**: hang 5 hook points off `Agent::run_loop` **without changing the loop skeleton**. This
is 80% of the answer to "change how the system behaves".

**Hook points**: `before_request` (may change messages / schemas), `after_reply` (may change or
veto the reply), `before_tool` (may change args / veto), `after_tool` (may change output),
`on_error` (may degrade).

**Changes**
- `trait Hook` (empty default implementations), `Runtime.hooks: Vec<Arc<dyn Hook>>`, executed in
  registration order.
- **0 built-in hooks** → behaviour exactly as today.
- This round only lets "config-driven" hooks take effect (e.g. `[hook] deny_tools = [...]`,
  `[hook] tool_args_default = {...}`).
- The mechanism for **plugins** to declare hooks is left to stage 6.

**Behaviour-unchanged baseline**: with no hooks, `run_loop` is equivalent path by path
(compare the e2e event sequence).

**Acceptance criteria**
1. Two things must be achievable through config: forbid a class of tools; auto-fill default
   arguments for a tool.
2. With no hooks, behaviour matches the baseline.
3. Note: hooks can change `messages` → they must also honour `Agent::push_message`'s
   disk-writing convention, or the history file will drift (memory and file out of sync).

**Risk**: medium. All of it sits in that one item, "hooks bypassing disk writes".

### This round's detail (the roadmap given on 2026-10-03 — **awaiting your confirmation, no
cutting until confirmed**)

**Cut one: extract the seam but don't change behaviour (pure refactor, zero hooks)**

- New `src/hook.rs` (core library, **zero new dependencies**):
  - `trait Hook`: all 5 methods **have empty default implementations** (`Send + Sync`, methods
    `async`, the same convention as `Tool` / `Ui`).
  - `HookChain`: executes in registration order, short-circuits per point; `run_loop` deals only
    with it and knows nothing about individual hooks.
  - `Verdict`: `Pass` / `Replace` / `Reject(reason)` — **a veto must always carry a reason**, and
    the reason is given back to the model.
  - `/hook` report copy (aligned with `/prompt`).
- `Runtime.hooks: Vec<Arc<dyn Hook>>`, fixed in `Runtime::new` in the same batch as `auth` /
  `store` / `prompt` (P1: only add fields to the same tree). **Under the default configuration
  this Vec is empty** — which is what makes cut one's "zero behaviour change" hold.

**Where the 5 points go (one line each inside `run_loop`; the skeleton doesn't move)**

| Hook point | Insertion site | Capability | After a veto |
|---|---|---|---|
| `before_request` | after `tool_schemas()`, before `llm.chat` | replace this turn's tool list; **append** messages | abort the turn (`bail!` with that reason) |
| `after_reply` | after `llm.chat` returns, **after accounting** (the tokens of a vetoed call were really spent) | change the reply's body / tool_calls | treated as an invalid reply, **reusing the existing empty-reply retry path** |
| `before_tool` | after each call's args are parsed, before `authorize_and_execute` | change args; block this call | don't execute; fill back `ToolOutput::err(reason)` |
| `after_tool` | after execution returns, before `ui.emit(ToolResult)` | change `content` / `is_error` | not applicable (change only) |
| `on_error` | at `llm.chat`'s `?` | swallow the error → inject a host note and retry (degrade) | — |

**How the "hooks bypass disk writes" risk is removed (acceptance criterion 3)**

`before_request` receives **not** a `&mut Vec<Message>` but "**the messages to append**" —
the loop enters them all through `Agent::push_message`, so disk and memory stay in sync by
construction.
Reason: `push_message` is the sole write entry point for `messages`, and rollback goes through
`truncate_messages` + `rewrite`; hand out a bare `Vec` and "it's in memory but not in the file"
is something that will happen within a minute.

→ The price: this round **does not open up "pruning / rewriting history"** (that needs
`rewrite` semantics). That class of need is left to stage 6's `AgentLoop`.
This is the one place where this round's capability is smaller than the roadmap's original text;
it is recorded explicitly.

**Cut two: config-driven hooks (`[hook]`)**

```toml
[hook]
# before_tool: block these tools (supports the `fs_*` prefix wildcard); the model gets a
# readable refusal reason
deny_tools = ["shell_*"]

# before_tool: auto-fill arguments for a given tool (**only when the model didn't supply that
# key**; a value the model passed explicitly is never overwritten)
[hook.tool_args_default.fs_read_file]
encoding = "utf-8"
```

- One `ConfigHook` covers both actions; an entirely empty `[hook]` → **no hook is registered**
  (keeping cut one's zero cost).
- "Fill default arguments" is defined as **fill gaps only**: overwriting the model's explicit
  intent would produce the absurdity of "the model said A, the tool received B", which is
  exactly the failure mode this mechanism most exists to avoid.

**Changed-file list**

| File | Change |
|---|---|
| `src/hook.rs` | **new**: trait + chain + `ConfigHook` + registry + `/hook` copy + unit tests |
| `src/agent.rs` | `run_loop` hangs the 5 points; wraps `llm.chat`'s `?`; nothing else touched |
| `src/config.rs` | `HookConfig` + `Config.hook` + `dummy()` kept in step |
| `src/runtime.rs` | `hooks` field + `hooks()` accessor |
| `src/lib.rs` / `src/main.rs` | `mod hook;` (one copy in each crate root, the existing structure) |
| `src/main.rs` | `/hook` subcommand + one line in `/help` |
| `config.example.toml` | `[hook]` explanation block — **commented out wholesale**, so a freshly copied config behaves with zero change |
| `docs/ARCH-ROADMAP.md` | implementation log + progress table (written at the end) |
| `gui/` | **zero changes** (hooks live in `Runtime`; the GUI goes through the same `Runtime::new` and gets them automatically) |

**How the behaviour-unchanged baseline is proven**

- Cut one: run one whole CLI turn against `tools/mock_llm.py` (**isolated root**; the technique
  is in TRAPS.md), saving a stdout record and `history/*.jsonl` before the change, then run it
  again afterwards and compare the two line by line.
- Cut two: the same turn plus `deny_tools` / `tool_args_default`, verifying that "the model's
  request didn't change while the arguments the tool received did".
- Unit tests: chain registration order, the three `Verdict` semantics, `[hook]` parsing,
  wildcard matching, and **fill gaps only, never overwrite**.

**Size / rollback**: zero new crates; expected < 20 KB (P4 budget 30 KB).
Rollback = leave `[hook]` empty (equivalent to today), or delete `src/hook.rs` and those 5 call
sites.

**Explicitly not done**: plugins declaring hooks (stage 6), `AgentLoop` / loop skeleton changes,
filling the `caps` capability slot (this round's two config items have nothing to do with
"capability requests", and `level_for` keeps receiving `&[]`), GUI appearance (stage 6's UI
customization layer).

**Four items confirmed by the user (2026-10-03, "do everything as you recommend")**

| # | Choice | Decision |
|---|---|---|
| A | build a new blocking mechanism for `deny_tools`, or **reuse `[auth] overrides` and give it wildcards** | **reuse auth** — override keys gain the `fs_*` prefix wildcard, `[hook]` gets no `deny_tools` |
| B | how much `before_request` may change messages: "append" only, or "prune" too | **append only** — pruning can't avoid `rewrite`, and **has been written into the stage 6 leftover list** (something the user specifically asked not to forget) |
| C | after `before_tool` rewrites args, should the **already-persisted assistant row** be rewritten with the new args | **don't rewrite** — history keeps "the model's real request"; the rewrite is only a host-side execution policy |
| D | `on_error`'s coverage: only wrap `llm.chat`'s `?`, or also swallow the `bail!` from "empty-reply retries exhausted" | **only the `?`** — the latter is anomalous model behaviour, and its diagnostic copy is already written |

**A note on A (the user chose A1)**

`[auth] overrides.shell_exec = "deny"` **already** "forbids a class of tools" today; what it
lacks is the wildcard. And its refusal copy says "the user refused this tool call" — which isn't
accurate for the "blocked by host policy" semantics.

- **A1 (recommended)**: add wildcards to `[auth] overrides` keys (`fs_*`), and change the refusal
  copy to one neutral sentence. The benefit: **no second blocking mechanism** — stage 3 cut the
  `/store` subcommand on exactly the same "equivalent information" judgement.
- **A2**: build `[hook] deny_tools` as the roadmap's original text says. Benefit: blocking and
  rewriting both live in the hook, so the semantics are in one place; cost: it overlaps with
  auth's capability, and "who exactly blocked this one" takes an extra thought later.

Either way, this stage's `Hook` seam and `tool_args_default` both get built (both items of
acceptance criterion 1 remain config-driven); the only difference is which layer "forbid a tool"
lands on.

### Implementation log (completed 2026-10-03, **awaiting acceptance**)

**Deliverables**

| File | Contents |
|---|---|
| `src/hook.rs` (new) | `trait Hook` (5 methods, all empty defaults) + `HookChain` (registration order, short-circuit on veto) + `Verdict` + `ErrorAction` + `Request` + `ConfigHook` + `registry_from_config` + `/hook` report |
| `src/config.rs` | `HookConfig` + `Config.hook` + `dummy()` kept in step (2 parsing unit tests) |
| `src/runtime.rs` | `Runtime.hooks: HookChain`, fixed in `Runtime::new` alongside auth / store / prompt; an unwritable config **errors out at startup** |
| `src/agent.rs` | `run_loop` hangs the 5 points (one insertion each; not one line of the skeleton moved) |
| `src/auth.rs` | override keys support a trailing `*` prefix wildcard: longer prefix wins, same length by lexicographic order → the verdict is independent of HashMap iteration order |
| `src/main.rs` | `/hook` command + one line in `/help` |
| `config.example.toml` | `[hook]` explanation block (**commented out wholesale**) + a note on `[auth.overrides]` wildcards |
| `gui/` | **zero changes** (hooks live in `Runtime`; the GUI goes through the same `Runtime::new`) |

**Three departures from the roadmap (all deliberate — please accept them together)**

1. **`after_reply` moved to "after accounting"** (the roadmap said "before accounting").
   Reason: the tokens for that call have already been spent, and the original order would leave
   **a vetoed call unaccounted**, conflicting with the existing principle that "errors get
   reported too". With an empty chain the two orders are exactly equivalent.
2. **`[hook]` contains no `deny_tools`** — decision A1 puts "forbid a class of tools" on
   `[auth]`'s wildcard key, so this stage's `[hook]` has only `tool_args_default`.
3. **The refusal copy is split into two sentences** (the else branch of
   `authorize_and_execute`): `AuthLevel::Deny` → "tool X is forbidden by host policy
   (`[auth]` in config.toml) ..."; an on-the-spot user refusal keeps the original "the user
   refused this tool call ...". This is a direct consequence of A1 — previously both shared one
   sentence and the model would try to **persuade the user**.

**Verification evidence**

- `cargo check --workspace --all-targets`: **zero warnings** (both the bin and lib compilation
  units are clean).
- `cargo test --workspace`: **62 + 62 + 12, all green** (stage 3 was 48 + 48 + 12; 14 new this
  round = hook 8 / auth 4 / config 2).
- **How "empty chain = behaviour unchanged" is proven**: the unit test
  `empty_chain_never_intervenes` asserts point by point that an empty chain doesn't touch tools,
  doesn't inject messages, doesn't change the reply, doesn't change arguments and doesn't touch
  results, and that `on_error` is always `Propagate` — and the `Propagate` branch *is* the
  original `?` (`return Err(e)`).
  A strict "byte-for-byte comparison against a pre-stage-4 binary" isn't possible: that would
  need a pre-stage-4 artifact, and the executables in `dist/` span stages 1–3 and can't isolate
  stage 4. So what's used here is "a structural proof that the empty chain is a no-op at every
  point + an e2e re-run of the same sequence".
- **Hands-on e2e** (`target/_tools/hook_e2e.py`: isolated root + `tools/mock_llm.py`, one whole
  turn per each of three configurations; the process is folded away, so you need `/expand` to
  see the arguments the tool actually received):

| Config | `/hook` | arguments the tool **actually received** | tool result | persisted assistant row |
|---|---|---|---|---|
| A baseline (no `[hook]`) | `0` | `{"path":"config.toml"}` | `no such tool: fs_read_file` | `{"path":"config.toml"}` |
| B `[hook.tool_args_default.fs_read_file] encoding="utf-8"` | `1` + "fill default arguments (only keys the model didn't give): fs_read_file(encoding)" | **`{"encoding":"utf-8","path":"config.toml"}`** | `no such tool: fs_read_file` | **still `{"path":"config.toml"}`** |
| C `[auth.overrides] "fs_*" = "deny"` | `0` (deny belongs to auth, not hooks) | `{"path":"config.toml"}` | **`tool fs_read_file is forbidden by host policy (`[auth]` in config.toml) ...`** | same as left (the refusal reason is archived) |

  → **row B pins two semantics at once**: the model's `path` wasn't displaced by the default
  (fill gaps only), and the history file still holds the model's original arguments (decision C);
  **row C** proves the wildcard hit and the new copy both took effect; **row A**'s process
  sequence matches stage 3's hands-on record.
- The 9 changed files: **zero U+FFFD hits, all-LF line endings**.

**Size**: zero new crates (`toml` / `serde_json` / `async-trait` / `tokio` were already in the
tree). Release size is left to be measured with the other stages before stage 5.

**Acceptance conclusion (2026-10-03, passed by the user)**

- The user replied: "ok, accepted, on to the next stage" → **stage 4 closed**.
- The three departures from the roadmap (`after_reply` moved after accounting, `[hook]` carrying
  no `deny_tools`, the refusal copy split into two sentences) **were accepted together**, with no
  conditions attached — as in stage 2, this stage leaves no leftover items (the 4 items such as
  "pruning / rewriting history" have other homes: the leftover list at the end of stage 6).
- Post-acceptance state: 1 new file (`src/hook.rs`) + 8 changed files, **all still uncommitted**
  (the user said he commits changes himself); `dist/` not rebuilt; **zero git write operations
  this round**.

---

## Stage 5 · Layered composition manifest (the composition layer)

**Goal**: unify the override entry points that stages 1–4 scattered across `config.toml` into
three layers: `profile → bundle → patch`.

**Changes**
- `aox.toml`: `profile` declares an ordered bundle list plus patch files; later layers override
  earlier ones.
- The composition function **reuses P1's normalized config tree — only the source changes, not
  the model** (this is the key to no rework).
- `aox --dump-config` and the GUI's "About" window button: print the composed tree plus each
  item's **source layer**.
- Built-in bundle: `core` (llm-openai + tools + store-jsonl + loop-standard + prompt-core).

**Acceptance criteria**
1. `--dump-config` shows which layer each value came from.
2. Editing a patch file can override any single value.
3. The old `config.toml` is still read as the last patch layer.

**Risk**: medium. **Known rework point** — the `config.toml` keys already landed in stages 1–4
must not stop working.
**[Your decision needed]**: are the old keys **long-term compatible** (recommended, zero
migration cost) or **migrated once** (cleaner config)?

### This round's detail (the roadmap given on 2026-10-03 — **awaiting your confirmation, no
cutting until confirmed**)

**Facts established first (script-verified)**

- Config is read at only **two** call sites: `src/main.rs:234` (CLI) and
  `gui/src/bridge.rs:421` (GUI), both through `Config::load(&config_path)` → hooking the
  composition layer into those two places makes both front ends take effect automatically.
- `paths::resolve_config_path(args)` and `arg_value(args, "--flag")` already exist → parsing for
  `--manifest <path>` / `--profile <name>` comes for free, and the rules belong in `paths.rs`
  (alongside `--config`).
- The "About" window is a **fixed 380px-wide card** (`gui/src/app.rs:1289`) — a whole tree won't
  fit in it.

**Cut one: turn "read one config" into "compose a layer stack" (pure refactor, zero behaviour
change)**

- New `src/config/layer.rs`:
  - `Layer { name, kind, doc: toml::Value }`; `kind` = `Bundle` / `BundleFile` / `Patch` /
    `Config`.
  - `merge(layers) -> (toml::Value, Provenance)`: **tables deep-merge, everything else is
    replaced wholesale** (later layers override earlier ones); `Provenance` maps a leaf path to
    the last layer that wrote it.
  - `resolve(...) -> Result<Resolved>`, `Resolved { config: Config, layers, provenance }`.
- **Not one field of `Config`'s shape moves**; it is still
  `toml::from_str::<Config>(&merged)` — which is what P1 means by "only the source changes, not
  the model".
- **With no `aox.toml`, the layer stack is `[config.toml]`**: the composed result is field for
  field the same as today (pinned by a test).

**Cut two: wire up the composition manifest**

```toml
# aox.toml (in the application root, optional)
profile = "default"

[profiles.default]
bundles = ["core"]
patches = ["aox.local.toml"]     # optional, stacked in order

[profiles.strict]
bundles = ["core", "strict"]
patches = ["aox.local.toml"]
```

**The layer stack (order is priority; later is stronger)**

| # | Layer | Source | Notes |
|---|---|---|---|
| 1..n | bundle | built-in `include_str!`, or `<app root>/bundles/<name>.toml` (which overrides the built-in if present, and is **explicitly flagged in the dump**) | a bundle may itself write `bundles = [...]` to keep aggregating — **DFS expansion with cycle detection** |
| n+1..m | patch | files listed in `aox.toml` (relative to the app root) | the user's own customization layer |
| last | config.toml | the one given by `--config` | **always the last layer** (acceptance criterion 3) |

**The shape of a bundle** (aligned with the five leaves in this stage's original text)

- `core` carries **no values of its own**, it only aggregates:
  `bundles = ["llm-openai", "tools", "store-jsonl", "loop-standard", "prompt-core"]`.
- Each leaf owns one area and carries factory values **equal to the code defaults** (see
  decision B).
- `strict` is a second bundled example bundle (more conservative authorization and quotas),
  **not part of the default profile**.

**New entry points**

- `aox --dump-config`: the layer list plus the composed tree, with **each leaf's source marked**
  (`bundle:store-jsonl` / `patch:aox.local.toml` / `config.toml` / `default (code)`).
- `aox --profile <name>`: switch profile temporarily (without editing `aox.toml`);
  `--manifest <path>` points at a manifest file.
- CLI `/config` command: a short human-readable view (which layers are listed, how many keys each
  contributes), aligned with `/prompt` and `/hook`.

**Changed-file list**

| File | Change |
|---|---|
| `src/config/layer.rs` (new) | `Layer` / `merge` / `Provenance` / `resolve` / `dump_text` + unit tests |
| `src/config.rs` | adds `pub mod layer;` (**`Config` itself untouched**); `Config::load` kept as-is (single-file semantics, tests rely on it) |
| `bundles/core.toml` + 5 leaves + `bundles/strict.toml` (new) | pulled into the binary with `include_str!` |
| `aox.example.toml` (new) | manifest example, copyable as a whole |
| `src/paths.rs` | `resolve_manifest_path()` (alongside `resolve_config_path`, so the rules don't scatter) |
| `src/main.rs` | `--dump-config` / `--profile` / `--manifest` + `/config` + one help line |
| `gui/src/bridge.rs` | config reading goes through composition; `BootInfo` carries an extra layer-name list |
| `gui/src/app.rs` | the "About" window gains **one line**: "config layers: core → aox.local.toml → config.toml" |
| `config.example.toml` | a note at the top: "what you're looking at is the last layer" |
| others (`theme.rs` / `hook.rs` / `store*` …) | **untouched** |

**How "default zero change" is proven**

- Cut one: with no `aox.toml`, the composed result ≡ parsing `config.toml` directly (unit test
  field by field + e2e re-run of the same sequence).
- **The `core` layer ≡ the code defaults**: a `Config` composed from `core` alone is field for
  field identical to one parsed from "just the required fields" — this test exists specifically
  to stop the two default sources (bundle and `#[serde(default)]`) from quietly drifting apart.
- e2e (isolated root + mock endpoint): ① no `aox.toml`; ② an `aox.toml` + `core` → the process
  and the persisted result should match; ③ plus one patch changing one value → **only that one
  place** changes.
- Failure paths: an unrecognized bundle name / a missing patch file / a cycle among bundles →
  **error out at startup**, never silently skipped.
- The old "generate config.toml on first run" path **doesn't change** (the check only looks at
  whether the `--config` file exists).

**Size / rollback**: zero new crates (`toml` was already in the tree); expected < 40 KB (P4
budget 300 KB). Rollback = delete `aox.toml` (the stack degrades to today's single layer), or
delete `src/config/layer.rs` and `bundles/` entirely.

**Explicitly not done**: browsing the full provenance tree in the GUI (only one line of layer
names in "About"), an "append" semantics for arrays (later layers always replace wholesale;
appending would need new syntax, left for later), stage 6's UI customization layer and
in-process components.

**Four items for you to settle**

| # | Choice | Recommendation |
|---|---|---|
| A | **old keys**: long-term compatible vs one-time migration (this stage's original text names this as your decision) | **long-term compatible**: `config.toml` is always read as the last patch layer, so existing configs cost nothing to migrate |
| B | **do bundles carry factory values?** | **yes** (leaf bundles write out values equal to the code defaults) — otherwise most keys in `--dump-config` could only show "default (code)" and acceptance criterion 1, "see the source", would be dead in the water. The price is one extra default source, pinned by the "core ≡ code defaults" test |
| C | **where bundle files come from** | **built-in + honour `<app root>/bundles/<name>.toml`** (which overrides the built-in if present, and the dump flags that it is an override) — this is the embryo of stage 6's "nameable, shareable customization packs" |
| D | **scope on the GUI side** | **only one line of layer names in "About"**, with the full tree in the CLI — a 380px card can't hold a tree, and cramming it in would just be noise nobody reads |

### Implementation log (2026-10-03)

All four items were carried out as recommended: **A long-term compatibility / B bundles carry
factory values / C built-in + honour `<app root>/bundles/<name>.toml` / D one line in the GUI**.

**What landed**

- New `src/config/layer.rs`: `Layer` / `LayerKind` / `Provenance` / `compose` / `resolve` /
  `expand_bundles` / `dump_text` / `layer_summary` (not one field of `Config`'s shape moved).
- New `bundles/`: `core` (pure aggregation, 0 values) + 5 leaf bundles + `strict` (an example,
  not in the default profile), all pulled into the binary with `include_str!`.
- New `aox.example.toml` (manifest example, copyable wholesale); `config.example.toml` gained a
  note at the top that "what you're looking at is the last layer".
- New entry points: `--dump-config` / `--profile <name>` / `--manifest <path>` / CLI `/config`
  (plus one line in `/help`).
- GUI: `bridge` now goes through `layer::resolve` (the same path as the CLI), and `BootInfo`
  carries an extra layer-name list; the "About" window gained one line,
  `config layers  llm-openai → tools → … → config.toml`.

**Three departures from the roadmap (all need your eyes)**

1. **`[auth]` / `[agent]` / `[runtimes]` each gained a container-level `#[serde(default)]`** —
   this is the **only** place `config.rs` itself was touched (the roadmap said "not one field of
   the shape moves").
   Reason: those three tables are **required as a whole**, while the composition layer
   necessarily produces "part of a table" (typically: `bundles/tools.toml` overridden by a file
   leaves only `[auth] meta_tools`), and the moment a partial appears the parse fails with
   `missing field`, with a line number pointing into the composed result that no user can read.
   The other tables (`[prompt]` / `[history]` / `[store]` / `[theme.metrics]` / `[hook]`)
   already take defaults for missing items — these three were an **oversight**.
   The direction of the impact is a **loosening**: previously writing `[agent]` but omitting
   `tool_timeout_secs` failed at startup; now it takes the default. No configuration that works
   today can break because of it. **I made this change; please confirm you accept it.**
2. **The `default (code)` label never actually appears in the dump.** Decision B makes bundles
   write out every key that has a factory value, so every value in the composed tree traces to
   some layer; the label stays in the code only as a fallback for "source not found". Keys with
   no default (required items plus `[theme]`'s metrics) don't enter the composed tree — because
   `[theme]` only affects the GUI and its defaults live in `gui/src/theme.rs`, not in any
   bundle. One sentence at the end of the dump accounts for where those keys come from.
3. **That one line in "About" wraps.** An 8-layer chain is about 120 characters and won't fit in
   a 380px card; it renders as 2–3 lines in practice (see `gui/_p5_about_crop.png`). It isn't
   truncated and gets no ellipsis — better to wrap than to hide information; with a single layer
   or a simple profile it is still one line.

**How zero drift was proven**

- 12 new unit tests (`layer.rs` 11 + one example pin). Among them, `core_equals_code_defaults`
  cross-checks "`core` composed + minimal config" against "minimal config parsed directly" using
  `format!("{:#?}")` — **field for field identical**. This one exists specifically to stop the
  bundle default source and the `#[serde(default)]` default source from quietly drifting apart
  (the price of decision B).
- `no_manifest_is_single_layer` pins "with no `aox.toml`, the composed result ≡ parsing
  config.toml directly".
- e2e (isolated root): ① no manifest; ② an `aox.toml` + `core` + one patch → **both complete
  session outputs byte for byte identical** (`diff` shows nothing). The dump's 8 layers are in
  the right order with the right sources: `provider.stream` → `patch:aox.local.toml`,
  `agent.rules_file` → `bundle:prompt-core`, `agent.tool_timeout_secs` → `config.toml`
  (covering the bundle's 60), `prompt.order` → `bundle:prompt-core`.
- Failure paths measured: an unrecognized bundle / a manifest explicitly named but missing / a
  profile that doesn't exist / a cycle among bundles — all error out at startup and wait for a
  keypress, never silently skipped.
- GUI, hands-on: an isolated root with a real 8-layer stack, and clicking "About" shows the
  wrapping layer chain (`gui/_p5_about_crop.png`).
- Tests: **74 + 74 + 12, all green** (stage 4 was 62+62+12; 12 new);
  `cargo check --workspace --all-targets` **zero warnings**; no U+FFFD / CRLF / tabs in changed
  files.
- Size: **zero new crates** (`toml` was already in the tree); estimated < 40 KB (P4 budget
  300 KB).
- Rollback: deleting `aox.toml` degrades it to a single layer; a full rollback means deleting
  `src/config/layer.rs` and `bundles/`.

**A documentation bug fixed along the way (left over from stage 4)**

- `config.example.toml`'s wildcard example read `fs_* = "deny"` — but `*` is **not in TOML's
  bare-key character set**, so copying it fails to parse outright. It now reads
  `"fs_*" = "deny"` with a note added.
  `layer.rs`'s `wildcard_override_key_survives_roundtrip` pins both things together: "it must be
  quoted" and "composition has to go through serialize → parse again without breaking such a
  key".

**Acceptance criteria (manual steps)**

1. `aox --dump-config`: see the layer list plus the source after each value (with no `aox.toml`
   there is only the single `config.toml` layer).
2. Copy `aox.example.toml` to `aox.toml` in the application root, then `--dump-config` again →
   6 more bundle layers; add a file to `patches` changing any one value → only that one place's
   source becomes `patch:…`.
3. `aox --profile strict --dump-config` → one more `bundle:strict` layer, and
   `auth.meta_tools` becomes `deny`.
4. Once running, `/config` shows the short layer list; `/help` contains this entry.
5. Deliberately misspell a bundle name / delete a patch file → error out at startup.
6. GUI: open "About" → one more line, `config layers …` (hidden when there is only one layer).
7. Old configs keep working as-is: **don't create an `aox.toml`** and everything is byte for byte
   identical to stage 4.

**Acceptance conclusion (2026-10-03, passed by the user)**

- The user replied: "fine, no problem, accepted, start the next stage" → **stage 5 closed**.
- The three departures named in the implementation log **were accepted together** (especially the
  first: the **container-level `#[serde(default)]` on `[auth]` / `[agent]` / `[runtimes]`** —
  the only place this stage touched `config.rs` itself, confirmed by the user); this stage
  **leaves no leftover items**.
- The "quote bug on `fs_*` fixed along the way" was also in scope for acceptance, with no
  objections.

---

## Stage 6 · the whole loop replaceable + in-process components (everything is a plugin, for real)

**Goal**: `AgentLoop` becomes wholly replaceable; introduce **in-process components** so a
plugin can contribute a hook / prompt segment / theme, not just a tool.

**Changes**
- `trait AgentLoop`, `StandardLoop` = the existing implementation (`Agent::run_loop` moved
  as-is).
- An in-process component engine (**the engine has to be decided first**, see the table below).
- Authorization for components and subprocess tools is unified into `AuthPolicy`: capability
  requests are always `ask`.
- The subprocess plugin form **stays** (good isolation, free choice of language), serving tools
  specifically.

**Engine selection (the only technical point needing your decision)**

| Option | Size delta | Notes |
|---|---|---|
| `mlua` (vendored Lua) | +0.3~0.6 MB | enough for scripted prompts/themes/hooks; sandbox controllable |
| `wasmi` (pure-Rust interpreter) | +0.5~1.2 MB | a real wasm sandbox, no JIT |
| `rhai` (pure-Rust scripting) | +0.8~1.5 MB | no C dependency |
| Wasmer | +10~20 MB | over budget |
| **Wasmtime (JIT)** | **+15~30 MB** | **forbidden** — it would destroy the "a few MB" physique |

- If a JIT is ever genuinely needed: `cargo build --features wasm-jit` **as a separate package**,
  with the main package untouched. That "ship heavy optional capability as its own package"
  pattern already has a working template in `installer/*.iss`'s `#ifdef BUNDLE_PYTHON`.

### Stage 6 additional topic: the UI customization layer (UI seam) and a behaviour sandbox

> ✅ **Moved out of stage 6 and chartered separately (user decision, 2026-10-03)** — the user
> replied "don't do 6C for now, write up a pending project and charter it first". **This project
> is deferred**; the charter is in **`docs/UI-CUSTOMIZATION.md`** (the four capability tiers,
> three hard constraints, three pre-work decisions, the engine candidate table and the to-do
> list). The section below is kept **verbatim as the original analysis** and not deleted; stage 6
> therefore narrows to "6a replaceable loop + 6b component contributions" and **no longer
> contains the UI customization layer**.
>
> 🚩 **The reminder obligation is discharged** — on 2026-10-03 the user asked that the "pet-like"
> UI customization and sandbox work be recorded against stage 6, and that **I remind him to
> re-confirm the scope of this area when we reach this stage**. The reminder appears at the top
> of this roadmap, and the user then decided to move it out and charter it.

**Where the topic came from**: the user asked "can the whole interface's elements and layout be
brought into the range of 'the user asks the AI to change it'", giving examples like "add a pet /
change the hi-there text / remove the input presets / restyle the input box", by analogy with "the
interface is HTML and the user injects JS/CSS" (an analogy only, not real injection).

**Conclusions already analysed (2026-10-03, **analysis only, no code written**)**

- egui is **immediate mode**: no DOM, no addressable node tree. The GUI presentation layer is
  **7,712 lines of hand-written calls** (app 2698 / activity 2206 / theme 803 / markdown 712 /
  syntax 660 / bridge 540), with about 350 `ui.*` call sites and **zero element registry** (the
  6 existing `Id::new`/`push_id` uses serve egui's internal state only).
  → so this is not "turning the existing code into configuration" but **adding a retained-mode
  customization layer**: **semantic element ids + property overrides + named slots + sandboxed
  patches**.
- The four capability tiers ascend in risk: ① text overrides ② style/property overrides
  ③ structural show/hide (in immediate mode, runtime "delete" can only mean **"hide and take no
  space"**) ④ injecting new components into slots. ① and ② need no AI at all (config suffices);
  ③ and ④ are where "let the AI write things" bites.
- **The AI's role is "offline customization author"**: read the element manifest (id table +
  available properties + current values) → produce a patch or a plugin → land it after human
  review as a nameable, shareable, rollbackable customization pack. It is **not** generating UI
  every frame at runtime (that would lose latency, cost, reproducibility and auditability all at
  once). The skeleton lines up directly with stage 5's `profile → bundle → patch` (just one more
  `[ui]` branch).
- The natural shape of "add a pet": **give plugins a "UI contribution" capability** (a plugin
  declares which named slot it draws into), rather than letting the AI construct the interface
  directly. Same origin as "in-process components", hence folded into this stage; a pet = one
  preinstalled rust plugin + one slot declaration.
- **Three hard constraints** (written into the acceptance criteria):
  1. **Element ids are a long-term contract**: they must be semantic ids
     (`input.preset[remove]`) rather than positional, and must stay compatible over the long run
     — this is the largest ongoing cost of the design.
  2. **No arbitrary layout reflow**: only "inside a slot" and "property overrides" are opened
     up; no general layout language is abstracted (cost and benefit are out of proportion).
  3. **Must degrade gracefully**: a parse or evaluation failure of any patch/script falls back
     to the default interface (no blank screen, no crash); the property allow-list, size caps
     and capability requests all go into `AuthPolicy` together.

**Acceptance criteria**
1. A hook / prompt segment / theme written as a script can be dropped into `plugins/` and work.
2. The default package's size growth is ≤ 300 KB (with a lightweight engine this can be relaxed
   to 1.2 MB and accounted separately).

**Risk**: **high**. This is the only stage that touches **size** and **attack surface** at the
same time.

### 🚩 Stage 4 leftover list (**every item must be cashed in when we reach this stage**)

> On 2026-10-03 the user asked explicitly that the capabilities stage 4 deliberately left closed
> be written here, **so they aren't forgotten when stage 6 arrives**.
> Each item below is a gap that was left open on purpose, not an oversight — and none of them is
> a "just patch it in while you're there" job.

1. **Pruning / rewriting history (the price of decision B)** — stage 4's `before_request` opens
   up only "append messages": appending is the only kind of change that can land on disk
   **without `rewrite`**. To do "prune the context / erase some history" you have to go through
   `Agent::truncate_messages` (truncate in memory + `Session::rewrite` so the file rolls back in
   step), and a hook can't get `&mut Agent` — so this belongs naturally at the `AgentLoop` layer:
   the loop prunes, `rewrite`s, and re-assembles the request itself.
   **Cashing-in criterion**: configuration can express "when the context exceeds N turns, drop
   the oldest ones", and after pruning memory and the conversation file agree (reading it back
   after a restart isn't truncated, no half-finished tool_calls are left).
2. **`on_error`'s degradation (the price of decision D)** — the seam and `ErrorAction::Retry` are
   both in place, but this round **nothing constructs `Retry`** (the config-driven hook only
   fills default arguments), so that variant is marked `#[allow(dead_code)]` in the bin
   compilation unit, with the reason noted beside it.
   **Cashing-in criterion**: a real hook (an in-process script or a config item) returns `Retry`
   when a request fails.
3. **The `caps` capability slot is still empty** — `Policy::level_for`'s `caps` has stood since
   stage 3, and stage 4 explicitly **doesn't fill it** (this stage's two config items aren't
   "capability requests"). It gets filled when in-process components request capabilities, and
   "requests are always ask" is guaranteed (P7).
4. **Hooks can currently only come from config** — "letting a script/plugin declare a hook" is
   one of stage 6's deliverables. Stage 4 already prepared what it needs: the `&Arc<Runtime>`
   context, the two return kinds `Verdict` / `ErrorAction`, a `HookChain` that executes in
   registration order, and `/hook` as the "see what's attached right now" entry point.

---

### This round's detail (the roadmap given on 2026-10-03 — **awaiting your confirmation, no
cutting until confirmed**)

> ✅ **Reminded, and already settled (2026-10-03)**: as you asked, the top of this roadmap
> reminded you that the "UI customization layer / pet-like sandbox" scope needed re-confirming —
> you then decided **not to do 6c yet and to charter it separately, deferred**
> (`docs/UI-CUSTOMIZATION.md`).
> **Stage 6 therefore narrows to 6a replaceable loop + 6b component contributions**; the roadmap
> below keeps the original text (including the 6c part) as an archive, with the 6c section marked
> as moved out.

**Facts established first (script-verified, 2026-10-03)**

- `Agent::run_loop` is at `src/agent.rs:238` (lines 238–419), with the 5 hook points already in
  place; everything the loop needs comes from `self.rt` / `self.llm` / `self.messages`, with
  **no hidden global state** — so "move the whole loop into a trait implementation" is
  structurally sound.
- The plugin protocol (`src/plugin/mod.rs`) is already "one JSON-RPC object per line":
  `initialize` returns `{name, tools}`, `tools/call` executes, `shutdown` notifies — **adding a
  few more methods is a structurally homogeneous extension**, and not one byte of an existing
  plugin needs to change.
- A subprocess plugin **already** costs "one RPC per tool call" → **"using a subprocess for a
  hook" is no more expensive than an existing tool call**. This directly overturns the premise
  that "hooks must be in-process", and is the biggest divergence between this round's roadmap and
  the original text.
- Current release sizes (2026-10-03 16:57): `aox.exe` **2.89 MB** / `aox-gui.exe` **5.83 MB**.
  +300 KB = **+10%** for the CLI; +1.2 MB = **+41%** — these two numbers are the answer to why
  P4's line matters.
- The "ship heavy optional capability as its own package" pattern **already has a working
  template**: `installer/*.iss`'s `#ifdef BUNDLE_PYTHON` plus `dist/AOX-*-with-python.exe`
  (the 3.2 MB → 11.9 MB precedent).

**Splitting stage 6 into independently acceptable sub-stages (recommended — originally 6a/6b/6c,
6c now moved out)**

| Sub-stage | Deliverable | Size | Risk | Leftover cashed in |
|---|---|---|---|---|
| **6a replaceable loop** | `trait AgentLoop` + `StandardLoop` (`run_loop` moved as-is) | ~0 | low-medium | **#1 prune/rewrite history** |
| **6b component contributions** | a component can contribute a hook / prompt segment / theme | **0** (subprocess route) / or the engine's size | medium | **#2 / #3 / #4** |
| ~~6c UI customization layer~~ | **moved out: chartered separately, deferred** → `docs/UI-CUSTOMIZATION.md` | depends on engine | — | — |

Reason: P6 "every stage is independently shippable" plus P4's size discipline. Doing it all in
one go would tie "the zero-risk seam extraction" and "the only part that touches size and attack
surface" into a single acceptance.

**User decision (2026-10-03)**: 6c is deferred — it has been chartered separately
(`docs/UI-CUSTOMIZATION.md`), and **stage 6 narrows to the two sub-stages 6a + 6b**. The 6c
section below keeps the original text as an analysis archive.

**6a · Replaceable loop (extract the seam, zero behaviour change)**

- New `src/agent_loop.rs`: `trait AgentLoop { async fn run(&mut self, agent: &mut Agent, ui:
  &mut dyn Ui) -> Result<()> }`; `StandardLoop` = the current `run_loop` **moved statement by
  statement** (P5 cut one: no rewriting, no opportunistic refactoring).
- `Runtime.loop_impl: Arc<dyn AgentLoop>`, fixed in `Runtime::new`; `config.toml [loop] impl =
  "standard"`, which **recognizes only `standard`, and any other value errors out at startup** —
  the same principle as `[store] backend` (unrecognized means error, not silent fallback).
- ⚠️ **The shape problem (the only technical point that must be solved before implementing)**:
  `run_loop` is a private method of `Agent`, and it calls the **private write entry points**
  `push_message` / `truncate_messages` / `commit_usage` and shares the private `TurnAcc`
  structure. For an external trait implementation to drive the loop, `Agent` has to expose a set
  of **controlled public primitives** (`push_message` / `truncate_messages` / `messages()` /
  per-turn accounting), and the ownership of `TurnAcc` has to be thought through (make it `pub`,
  or replace it with `Agent::begin_turn()` + `Agent::accrue(usage)` so the accounting stays
  inside `Agent` — the latter is cleaner and is the preference).
  This is **not** "interface leakage": `AgentLoop` and `Agent` are collaborators at the same
  layer (stage 3's "no paths on the interface" was about **backend** implementation details not
  surfacing, which is different).
- **Cashing in leftover #1 (prune / rewrite history — the one you specifically named as not to
  be forgotten)**: `StandardLoop` is the first loop that "can prune and rewrite". Add the config
  `[loop] max_rounds = N`: when the context exceeds N turns, prune starting from the **oldest**
  turns, **keeping the system head plus the integrity of the most recent turn's tool_calls and
  their results** (a half-finished tool_calls can't be replayed).
  Pruning goes through `Agent::truncate_messages` — truncate in memory plus `Session::rewrite`,
  both legs moving together.
  **Cashing-in criterion**: after pruning, memory and the conversation file agree; reading it
  back after a restart isn't truncated and leaves no half-finished tool_calls; with no
  `max_rounds` configured it is byte for byte identical to today.
- Verification baseline: `StandardLoop` cross-checked against the current `run_loop` (isolated
  root + `tools/mock_llm.py` re-running the same sequence, `diff`ing the complete session
  output); zero change with no `[loop]` config; the failure path (an unrecognized impl) errors
  out at startup.

> ✅ **Landed (2026-10-03)** — see the **6a implementation log** below. Implementation hit the
> "pruning direction written backwards" trap; it is fixed, with unit tests and e2e assertions
> pinning the direction. That part is worth reading.

**6b · Component contributions (the zero-size-by-default route — recommended)**

The key judgement: the roadmap's original text treats "in-process components" as the *means*,
but looking back at the *goal* — "let a plugin contribute a hook / prompt segment / theme" —
**the goal itself doesn't need in-process**. The only thing that really needs in-process is one
item (a pet that draws every frame), and that belongs to 6c.

- **Recommended (6b-①, size +0)**: add the capability to the **existing subprocess protocol**.
  - `plugin.toml` gains an optional `[contributes]`: `hooks = [...]` / `prompt_segments = [...]`
    / `theme = "…"`.
  - `initialize`'s response is extended accordingly (**not sending it is the old shape, so old
    plugins keep working**); new RPC methods `hooks/before_request`, `hooks/after_reply`,
    `hooks/before_tool`, `hooks/after_tool`, `hooks/on_error`, `prompt/segment`, `theme/patch`.
  - On the host side, write a `ComponentHook` implementing the **existing** `trait Hook`, mapping
    RPC results onto `Verdict` / `ErrorAction` — **reusing stage 4's seam rather than inventing a
    new concept**.
  - A crashed subprocess = that component silently fails (which satisfies "must degrade"), and
    the main loop carries on.
  - Size: **0** (the existing crates are all sufficient).
- **Alternative (6b-②, engine size)**: only necessary if you want "low latency / every frame" —
  which belongs to 6c. **So the engine selection decision is deferred to 6c** (where there is a
  real reason for it).
- Cashing in **leftover #2**: `on_error`'s `ErrorAction::Retry` gets its first real constructor —
  a component can return `retry(note)`; and `[hook]` gains a `max_llm_retries = N`
  **config-driven fallback** so "retries without writing a script" is true too (which also lets
  that `#[allow(dead_code)]` be dropped).
- Cashing in **leftover #3**: the `caps` capability slot gets filled for the first time — a
  component's `[contributes]` declaration **is** the capability request, and `Policy::level_for`
  is always `ask` (P7); the CLI shows it via `/hook`, the GUI via the authorization dialog.
- Cashing in **leftover #4**: hooks no longer come only from config — a component can declare
  them.
- Acceptance criteria: a script-written hook / prompt segment / theme dropped into `plugins/`
  works; killing the component subprocess leaves the host finishing its turn normally
  (degradable).

**6c · UI customization layer — moved out of this stage (user decision, 2026-10-03: chartered
separately, deferred)**

→ Charter: **`docs/UI-CUSTOMIZATION.md`** (the four capability tiers, three hard constraints,
three pre-work decisions, the engine candidate table and the to-do list). This section's 6c
analysis is kept **in the "additional topic" section above** as an archive.

The three things it would have decided at the same time (kept as input for whenever work starts):
① whether an in-process engine is wanted; ② whether the long-term maintenance cost of element
ids is acceptable; ③ how to account for the size (a separate 1.2 MB tier, or a separate package).
**These decisions are deferred along with the charter.**

**All three items are settled (6c moved out; the user replied on 2026-10-03: "do everything as
you recommend")**

| # | Choice | Decision |
|---|---|---|
| A | split stage 6 into **6a / 6b steps** vs doing it in one go | ✅ **split** — 6a is zero size and zero risk, and can be delivered first |
| B | 6b via "**subprocess components (+0 KB)**" vs "an in-process engine (+0.3~1.5 MB)" | ✅ **subprocess** — the goal doesn't need in-process; the engine isn't needed and is deferred along with 6c |
| C | the pruning policy for `[loop] max_rounds` (the form in which leftover #1 is cashed in) | ✅ **when exceeded, drop the oldest turns, keeping the system head and the most recent turn's tool_calls intact** |

> ✅ **Two more (D / E) are also settled (2026-10-03)**: D (do 6c this round?) → **chartered
> separately, deferred** (`docs/UI-CUSTOMIZATION.md`); E (engine selection) → **deferred along
> with 6c**. All five items now have conclusions and nothing is left pending; **6a has been
> implemented accordingly** (see the "6a implementation log").

**Explicitly not done (this stage, unless you ask otherwise)**: the AI generating UI every frame
at runtime (the role is "offline customization author"); a general layout language; an "append"
semantics for arrays; a second implementation of `[store] backend` (sqlite).

**How "6a/6b default zero change" is proven**

- 6a: with no `[loop]` config, `StandardLoop` is cross-checked against the current `run_loop`
  (e2e re-run + persisted `diff`); an "unrecognized impl" errors out at startup.
- 6b: with **no component installed**, the host registers no `ComponentHook` and `HookChain` is
  still an empty chain — the same proof as stage 4's "empty chain = behaviour unchanged", reused
  directly.
- Component side: add an example component under `plugins/` (contributing one prompt segment +
  one before_tool) and verify, in an isolated root, that "the model's request didn't change while
  the host's behaviour did".

---

### 6a implementation log (completed 2026-10-03, **accepted**)

**Items A / B / C settled by the user ("do everything as you recommend")**: split into two steps,
6b via subprocess, pruning policy as recommended. This log covers only **6a (replaceable loop)**;
6b starts once 6a is accepted.

**Deliverables**

| File | Contents |
|---|---|
| `src/agent_loop.rs` (new, 517 lines) | `trait AgentLoop` (`name` + `run`) + `StandardLoop` (= the original `Agent::run_loop` **moved statement by statement**) + `build()` factory + the `trim_point()` pure function + `report()` + 10 unit tests |
| `src/agent.rs` | 484 → 359 lines: `run_loop` (182 lines) plus `truncate`/`cap_chars` moved out; new **controlled primitives** `rt/llm/messages/push_message/begin_turn/accrue_call/finish_turn/authorize_and_execute/drop_oldest`; `TurnAcc` went from a local variable to an `Agent` field |
| `src/config.rs` | `LoopConfig` (`impl` / `max_rounds`) + `Config.loop_config` (`rename = "loop"`, since `loop` is a Rust keyword) + `dummy()` kept in step + 3 parsing unit tests |
| `src/runtime.rs` | `loop_impl: Arc<dyn AgentLoop>`, fixed in `Runtime::new` from `[loop] impl` |
| `src/lib.rs` / `src/main.rs` | `mod agent_loop;` (one copy in each crate root) |
| `src/main.rs` | `/loop` command + one line in `/help` |
| `config.example.toml` | `[loop]` explanation block — **commented out wholesale**, so a freshly copied config behaves with zero change |
| `src/hook.rs` / `src/auth.rs` / `src/prompt.rs` | comments only: three "left for stage 6" notes updated to "landed" or re-pointed at 6b |
| `gui/` | **zero changes** (the loop lives in `Runtime`; the GUI goes through the same `Runtime::new`) |

**The shape of the seam**

```rust
#[async_trait]
pub trait AgentLoop: Send + Sync {
    fn name(&self) -> &str;                                // = the value `[loop] impl` recognizes
    async fn run(&self, agent: &mut Agent, ui: &mut dyn Ui) -> Result<()>;
}
```

For this, `Agent` opened a set of **controlled primitives**. That is not "interface leakage" —
stage 3's "no paths on the interface" governs **backend implementation details**, while
`AgentLoop` and `Agent` are collaborators at the same layer. The line that genuinely wasn't
relaxed is: **`push_message` is still the sole write entry point for `messages`** (it writes the
file at the same time), and the loop only gets a read-only `&[Message]` view, so "it's in memory
but not in the history file" still can't be created.

The `TurnAcc` choice: it was **not** made `pub`; instead it became an `Agent` field, with the
loop calling `accrue_call()` to record and `run_turn` calling `finish_turn()` to close. Reason:
the loop is replaceable, **the ledger is not** — a different implementation still has to record
into the same ledger (and when `finish_turn` never happens, no row is written; an empty ledger
isn't worth recording).

**Cashing in leftover #1: `[loop] max_rounds`**

- A turn is bounded by a message with **`role == "user"`**; `max_rounds = 0` (the default) means
  no pruning.
- Only whole turns are dropped, starting from the **oldest**, and `messages[0]` (the system
  prompt) is always kept.
- **Why no extra alignment is needed**: the cut point is computed directly from "the index of the
  Nth user message" and therefore always lands on a user message — dropping the oldest whole
  turns leaves a suffix that is itself a legal request body, so you never get the "tool result
  still here while its assistant was dropped" state that the endpoint rejects with a 400. This
  invariant is pinned exhaustively over 1–7 turns by `cut_always_lands_on_a_turn_start`.
- Disk is symmetric: `Agent::drop_oldest` goes through `Session::rewrite` internally, so
  whatever memory loses, the file loses too.

**⚠️ The trap hit this round (worth recording separately)**

The first version wrote the pruning as `agent.truncate_messages(cut)` — **the direction was
backwards**. `Vec::truncate(n)` means "keep the first n" (which is exactly what interrupt
rollback wants), whereas pruning wants to **keep the last few turns**. The consequences were
**silent**:

- it showed up as "what I just said this turn is gone, while the oldest context is still there";
- and memory and file were **consistently wrong together**, so looking at the persisted content
  alone looked perfectly normal;
- worse, **the first version's e2e assertion was too weak** (it only checked "1 user message
  remains" and "doesn't start with a tool"), so the reversed direction still passed — it was only
  found by reading the persisted content line by line and noticing that line 2 was the **first
  turn's** input.

The fix: a new `Agent::drop_oldest(cut)` (`drain(1..cut)`, deliberately named differently from
`truncate_messages`, with the comment stating that the two go in opposite directions), and the
e2e assertion changed to assert **content**: "what remains must be the **newest** turn's input,
with none of the oldest turn's input left". The unit test
`trimming_keeps_the_newest_turns_not_the_oldest` asserts content too.

**Verification evidence**

- `cargo check --workspace --all-targets`: **zero warnings** (both the bin and lib compilation
  units clean).
- `cargo test --workspace`: **87 + 87 + 12, all green** (before 6a it was 74+74+12; 13 new this
  round = `agent_loop` 10 / `config` 3).
- **Default zero drift (A vs B)**: isolated root + `tools/mock_llm.py`, one complete session
  (`/loop` → one real task → `/expand`); **writing `[loop] impl = "standard"` explicitly versus
  not writing `[loop]` at all: stdout byte for byte identical, persisted content identical**.
- **Pruning really works (C, `max_rounds = 1`, two turns)**: the pruning notice appears; the
  conversation file holds only the **second** turn's input (`再用一句话总结一下刚才的结论`);
  **not one line of the first turn's input remains**; the file doesn't start with a `tool`
  result. Memory and file agree (reading back after a restart isn't truncated).
- **Failure path (D, `impl = "react"`)**: errors out at startup with
  `[config error] unknown loop implementation: react (available: standard)`, waits for a
  keypress, **does not silently fall back to standard**.
- Changed files: **zero U+FFFD hits, no tabs, all-LF line endings**.

**Size**: zero new crates (`toml` / `serde` / `async-trait` were already in the tree). About 330
new lines (of which ~180 were moved from `agent.rs`); the release `aox.exe` measured this round
is **3,038,208 B (2.90 MB)**.

> Against the previous measurement of 2,893,312 B (16:57 build) that is **+141 KB**, but that pair
> of numbers **spans both stage 5 and 6a**, so 6a's own delta can't be isolated from it — and I
> won't pretend it can. **3,038,208 B is recorded here as the baseline before 6b starts**,
> precisely because 6b's headline claim is "the subprocess route is +0 KB"; comparing against it
> then will confirm or refute that. Under P4 (a 300 KB budget) this round is within noise.
>
> **Correction (2026-10-03, after the 22:25 full release build)**: `bash
> tools/build_installer.sh` (all + both) produced **shipping binaries** of `aox.exe` =
> **3,038,720 B (2.90 MB)** and `aox-gui.exe` = 5,947,392 B. Both come from **the same source**
> as the 22:05 numbers (the last source change was at 21:59): the +512 B on `aox.exe` is
> irreproducible release-link jitter; the +116,736 B on `aox-gui.exe` is because the 21:15 gui
> source change had never been recompiled (the previous exe was still from 16:57) and was only
> picked up by this full build. **6b's baseline is therefore the shipping value, `aox.exe`
> = 3,038,720 B.**

**Acceptance criteria (manual steps)**

1. Start `aox` and type `/loop` → see "loop implementation: standard (available: standard)" and
   "context pruning: off".
2. **Don't create a `[loop]` section**, chat for one turn → behaviour **exactly** as before 6a
   (this is the core acceptance point of this round).
3. Uncomment the `[loop]` block in `config.example.toml` (`impl = "standard"`) → restart,
   behaviour unchanged.
4. Change `impl` to `"react"` → errors out at startup, with the message naming both `react` and
   `standard`.
5. Set `max_rounds = 1` and chat for two turns → at the start of the second you see "dropped the
   oldest N messages"; use `/history show <index>` to read the whole thing, which should
   **contain only the last turn** with no half-finished tool calls.
6. `/help` contains the `/loop` entry.

**Explicitly not done**: 6b's component contributions (subprocess `[contributes]` /
`ComponentHook`), an in-process engine, a second implementation of `[store] backend`, and any
GUI-side change.

---

### 6b implementation log (completed 2026-10-03, **accepted**)

> The standalone user-facing write-up (technical changes / experience changes / how deep you can
> reach now / boundaries / rollback) lives separately in
> **`docs/COMPONENT-CONTRIBUTIONS.md`**.

**Three items settled by the user ("go with your recommendation")**: **①A** `HookChain` switches
to `RwLock` internally plus runtime appending; **②** relax "a component must declare tools" (one
that only contributes a hook / segment / theme is fine); **③a** theme contributions in v1 only do
"declare → fetch → validate → report", **not touching the GUI's live palette**.

**Deliverables**

| File | Contents |
|---|---|
| `src/plugin/mod.rs` | `[contributes]` (`hooks` / `prompt_segments` / `theme` / `caps`) → `Contributes` (`is_empty` / `normalize` / `overridden_by`); `activate()` fetches contributions and registers them via **3 new RPCs**; `PROTOCOL_DOC` gains §4; `PYTHON_SDK` kept in step; **6 unit tests** |
| `src/hook.rs` | `HookChain`'s internal `Vec` → `RwLock<Vec<Arc<dyn Hook>>>` plus `push()` / `snapshot()`; new `POINTS` (the single source of truth for the 5 point names); `ComponentHook` (implementing the **existing** `trait Hook`) + `verdict_of`; **4 unit tests** |
| `src/runtime.rs` | `add_hook()` (the component registration entry point); `caps_for(kind)` (the capability bits a component declared, by tool kind) |
| `src/auth.rs` | `ConfigPolicy::level_for` now really fills `caps`: **a plugin tool with non-empty caps ⇒ `Ask`**, positioned **after** "explicit user override" and **before** "category default"; **3 unit tests** |
| `src/agent.rs` | at the authorization site, `&[]` → `self.rt.caps_for(&kind).await` |
| `src/agent_loop.rs` | `on_error`'s `Propagate` branch wired to `[hook] max_llm_retries` (inject a user note, then retry) |
| `src/config.rs` | `HookConfig.max_llm_retries` (default 0) + **1 unit test** |
| `src/prompt.rs` | `Source::Plugin` drops `#[allow(dead_code)]` (a real producer now exists) |
| `src/main.rs` | `/plugins` prints one more "component contributions" line; `/help` and `/hook` copy |
| `config.example.toml` | `[hook] max_llm_retries` explanation block — **commented out wholesale** |
| `tools/samples/contributing_component/` (new) | `plugin.toml` + `main.py`: contributes 2 hook points + 1 prompt segment + 1 caps entry |
| `gui/` | **zero changes** |

**The shape of the seam (protocol layer — all of it a structurally homogeneous extension; not
one byte of an existing plugin needs to change)**

```
initialize's response carries one more optional item:
  contributes: { hooks: [...], prompt_segments: [...], theme: bool, caps: [...] }

the host then pulls on demand (all over the existing one-JSON-RPC-per-line channel):
  hooks/<point>    → {verdict:"pass"|"reject", reason, ...}; on_error returns {action:"retry"|"propagate"}
  prompt/segment   → {text:"..."}        (once per declared segment name)
  theme/patch      → {theme:{...}}       (once, when theme = true)
```

The four rules of `Contributes`: every field is `#[serde(default)]` (old manifests parse as
before); `normalize()` **validates** hook point names one by one (an unknown one errors and
**lists the available points**, never dropped silently), dedups and orders them; what `initialize`
carries **overrides** what `plugin.toml` says (live beats manifest); and an empty
`[contributes]` is **not persisted** on `save`.

**Three deliberate choices in `ComponentHook`**

1. **Doesn't hold a `PluginHandle`, only records the plugin name and looks it up at call time**
   → unloading the plugin **degrades it automatically**, with no unregistration needed (pinned by
   `component_hook_degrades_when_the_plugin_is_gone`).
2. **The failure ledger shares the same `Arc<AtomicU64>` / `Arc<Mutex<Option<String>>>` pair as
   the handle** → `/hook` and `/plugins` report from the same ledger.
3. **Only asks the points that were declared** (`undeclared_points_are_never_asked`) — a point
   that wasn't declared costs zero RPCs.

**Why `HookChain` had to support runtime appending**: the chain is already fixed by the time
`Runtime::new` returns, while plugins are only `activate()`d **after** that. Hence `Vec` →
`RwLock<Vec<_>>`; every hook point takes a `snapshot()` first (cloning a batch of `Arc`s under
the read lock) and then iterates — `std::sync`'s read guard is not `Send` and **can't be held
across an `await`**. **An empty chain still iterates 0 times**, so stage 4's "empty chain =
behaviour unchanged" proof is untouched.

**Cashing in leftovers #2 / #3 / #4**

- **#2 (nothing constructed `Retry` for `on_error`)** → now **two** real producers: ① a
  component returning `{action:"retry"}` from `hooks/on_error` (and `ErrorAction::Retry`'s
  `#[allow(dead_code)]` is dropped accordingly); ② `[hook] max_llm_retries = N` makes the
  **built-in loop** retry N times on `Propagate` (default **0** = byte for byte identical to
  stage 4).
- **#3 (`caps` was always empty)** → a component declares capability bits in
  `[contributes] caps`, and the host fills them into `Policy::level_for` via `caps_for()`.
  **Only `ToolKind::Plugin` is affected, and it is always `Ask`** (P7); an explicit user override
  still beats it; meta tools are **never affected** (each of the three is pinned by its own unit
  test).
- **#4 (hooks could only come from config)** → `HookChain` now has **two sources**: config
  (stage 4) plus components (this round). `/hook`'s report lists the two separately.

**⚠️ The trap hit this round: three e2e false alarms (none of them the code's fault)**

`component_e2e.py` failed cases B / C / D on its first run, and each one turned out to be an
**over-confident assertion**:

- **B** "with no component installed, `/hook` shouldn't mention component contributions" — but
  the empty chain's report **already contains** the phrase "two sources", which includes the
  words "component contributions". Changed to require the **full-width colon** form
  `组件贡献：` (which is what a real contributions line uses).
- **C** "`after_tool`'s mark should appear in the history" — the e2e's own `history_lines()`
  **truncates content to 120 characters**, which cut exactly the mark appended at the end.
  Relaxed to 400.
- **D** "a stuck component should record 'has failed 1 time'" — this turn had **two** tool calls,
  each hitting the 10 s timeout, so the count was **2**. Changed to `已失败 (\d+) 次` with `>= 1`
  required.

The lesson is **the same one as in 6a**: **assertions must assert content and exact wording**,
not "roughly whether it appeared"; plus a new one — **the test fixture itself can truncate or
rewrite the data under test**.

**Verification evidence**

- `cargo check --workspace --all-targets`: **zero warnings**.
- `cargo test --workspace`: **101 + 101 + 12, all green** (6a was 87+87+12; +14 this round =
  `hook` 4 / `plugin` 6 / `config` 1 / `auth` 3).
- **Default zero drift (A)**: isolated root + `tools/mock_llm.py`, one complete turn; `/hook`
  reports 0 hooks — **byte for byte identical** to 6a's baseline.
- **Old plugins unaffected (B)**: an old plugin that only declares tools and has no
  `[contributes]` still parses, still runs its tools, and the chain still has 0 hooks.
- **A component really works (C)**: after loading the example component, `/hook` lists 2 hook
  points + 1 caps entry; `/plugins` shows the contributions; `/prompt` shows that segment's
  source as "**plugin**"; the marker injected by `before_request` appears in the request body,
  and the mark appended by `after_tool` appears at the end of the tool result.
- **Degradation really degrades (D)**: change the component so `before_request` sleeps 25 s →
  every call hits the 10 s timeout → **the turn still completes** (degraded to Pass), and the
  failure count is recorded.
- **caps ⇒ ask (E)**: a plugin tool that declared caps is asked about in the CLI; at EOF the
  existing rules judge it `Deny`.
- **Retries really retry (F / G)**: `[hook] max_llm_retries = 2` → retries twice after a failure;
  **without that section** → 0 times, as before.
- **6a regression**: `loop_e2e.py` all green, with both A/B stdout **byte for byte identical** —
  proving that switching `HookChain` from `Vec` to `RwLock` did **not** shake 6a's zero drift.
- Changed files: **zero U+FFFD hits, no tabs, all-LF line endings**.

**Size**

- Baseline (6a shipping value): `aox.exe` = 3,038,720 B / `aox-gui.exe` = 5,947,392 B.
- 6b measured: `aox.exe` = **3,122,688 B** (**+83,968 B**, +2.8%) / `aox-gui.exe` =
  **6,024,704 B** (**+77,312 B**).
- **Reproducible**: after `touch src/main.rs` forcing a relink, still **3,122,688 B** (not linker
  jitter).
- **Cross-check**: `aox-gui.exe` had **zero source changes** in 6b yet also grew +77 KB ⇒ the
  growth really comes from the **`src` code shared by both binaries**, which also **retroactively
  proves that 6a's baseline was measured correctly**.
- Attribution: the embedded template strings (`PROTOCOL_DOC` 3,471 B + `PYTHON_SDK` 4,715 B,
  8.2 KB total, and mostly already there in 6a) are **not the main cause**; the rest is machine
  code.
- Density reference: the `src` tree as a whole is **11,045 lines → 3.12 MB**, about
  **283 B/line**; +84 KB falls in the range of **normal code growth**, not something dragged in
  by a dependency or engine.
- **Shipping installers (full build 2026-10-03 23:17, `tools/build_installer.sh all both`, passed
  first time)**: `AOX-Setup-0.1.0.exe` 3,301,246 B / `AOX-GUI-Setup-0.1.0.exe` 4,524,600 B /
  `AOX-Setup-0.1.0-with-python.exe` 11,972,499 B /
  `AOX-GUI-Setup-0.1.0-with-python.exe` 13,196,002 B / `aox-python-runtime.zip` 10,922,561 B.
- **Conclusion**: **+0 holds at the dependency level** (no new crate; `Cargo.toml` /
  `Cargo.lock` untouched throughout); **+84 KB at the byte level**, within P4's ≤300 KB budget ⇒
  **acceptance criterion #2 satisfied**.
- **Correction**: "6b size = 0" above should be read as "**zero dependency growth**" (subprocess
  route, no crate pulled in, no engine needed), **not** "zero byte growth" — the latter was never
  the point of the subprocess route.

**Acceptance criteria (manual steps)**

1. With **nothing installed**, start `aox` for one turn and type `/hook` → reports "loop hooks ·
   0" and lists **two sources**; behaviour exactly as before 6b.
2. Copy the whole `tools/samples/contributing_component/` directory into `plugins/` → restart →
   `/plugins` should gain a "**component contributions:**" line with 2 hook points, 1 prompt
   segment and the requested capability `fs_write`.
   (This example sits **deliberately in `tools/samples/` rather than `plugins/`**, precisely so
   that it isn't loaded by default and doesn't break zero drift.)
3. `/hook` → the chain becomes 1, with the source column reading "component contributions ·
   hooked before_request, after_tool".
4. Have it run a tool (read a file, say) → the **end of the tool result** should gain the mark
   appended by `after_tool`; and the rules segment injected by `before_request` should be
   visible in the context.
5. `/prompt` → find the segment **`house_rules:rules`** (titled `rules（house_rules）`), sourced
   as "**plugin**", ordered after the config segments (order 60).
6. Change `main.py`'s `before_request` to `time.sleep(25)` and restart → run one turn, and
   **the interface should not hang** (it degrades to Pass after the 10 s timeout); the
   component's failure count in `/plugins` increases.
7. Uncomment `[hook] max_llm_retries = 2` in `config.example.toml` → against a failing endpoint,
   you should see "model request failed, host retrying automatically (1/2)"; delete that block →
   back to 0 retries.

**Explicitly not done**: the 6c UI customization layer (chartered and deferred →
`docs/UI-CUSTOMIZATION.md`); an in-process scripting engine; **really applying `theme_patch` to
the GUI's live palette** (v1 only does declare → fetch → validate → report, i.e. decision ③a); a
second implementation of `[store] backend`.

---

## Collaboration rhythm (fixed for every stage)

1. I give **that stage's detailed roadmap**: files touched, changes, acceptance criteria, size
   impact, rollback.
2. **You confirm** (or ask for adjustments) — no code moves until confirmed.
3. I implement, **cutting only inside the confirmed scope, with no opportunistic refactoring**.
4. I self-check: `cargo test --workspace` + that stage's behaviour-baseline comparison + a
   U+FFFD scan of Chinese files.
5. I give **concrete steps for you to accept by hand**.
6. **You accept** → only then do we go to step 1 of the next stage.

Throughout: a release can be built at the end of every stage; any stage can be halted or
reordered.

---

## Progress

| Stage | Status | Accepted |
|---|---|---|
| 1 prompt segmentation | accepted (2026-10-03) | 2026-10-03 |
| 2 appearance as data | accepted (2026-10-03) | 2026-10-03 |
| 3 storage & policy seams | accepted (2026-10-03) | 2026-10-03 |
| 4 loop hooks | accepted (2026-10-03) | 2026-10-03 |
| 5 layered composition manifest | accepted (2026-10-03) | 2026-10-03 |
| 6a replaceable loop | accepted (2026-10-03) | 2026-10-03 |
| 6b component contributions | accepted (2026-10-03) | 2026-10-03 |
| — UI customization layer (formerly 6c) | **chartered, deferred** → `docs/UI-CUSTOMIZATION.md` | — |
| 7a plugin fence (new line · ① Job Object) | **awaiting acceptance** → `docs/SANDBOX.md` | — |
| 7b workplace boundary (new line · ③ file capability back to the host + GUI entry + read/write semantics + naming unified) | **awaiting acceptance** → `docs/SANDBOX.md` | — |

---

## Stage 7 · Plugin fence (new line, chartered 2026-10-04)

**The option comparison, the threat model, and "what is actually blocked right now" are all
written up in [`docs/SANDBOX.md`](SANDBOX.md)** — including the cost comparison and
implementation notes for the four alternatives: ②a AppContainer / ②b low integrity level /
③ file capability back to the host / ④ WASM.

- **Why a separate line**: stages 1–6 were entirely on the "extensibility" axis (replacing
  compile-time hard-coding with replaceable interfaces); the security-boundary axis barely moved.
  Re-auditing after 6 closed, the biggest gap was "a plugin the model wrote itself can do
  anything".
- **This stage does two things**: ① the Job Object fence (process count / memory / UI, enforced
  by the kernel) and ③ the workplace boundary (take file capability back to the host). Neither
  includes **OS-level filesystem isolation** — really stopping a malicious third-party plugin
  needs ②a, which isn't built.
- Landing ①: new `src/sandbox.rs` plus a `config.toml [sandbox]` section; the hook-in points are
  only `plugin::spawn_command` / `spawn_and_init` / `respawn` (there is no second launch path).
- Evidence ①: `aox selftest` regression 7's **three-way A/B** — under the default allowance a
  plugin can open child processes and still do its own work / with `max_processes = 0` the fence
  blocks it / with the fence off it can again (the control group: proving it really is the Job
  that blocked it, not some other fault in this machine's python);
  running the real `aox` once, all 8 real plugins (python and rust alike) show `fence ✓`.
- Size ①: `aox.exe` 3,122,688 → **3,139,584 B (+16,896, +0.54%)**; `aox-gui.exe` 6,024,704 →
  **6,034,944 B (+10,240)**. Well inside the ≤300 KB-per-stage budget.
- New commands: `/sandbox` (see the limits, and restate "what it can't govern"); `/plugins` gains
  a `fence ✓ / ✗` marker per entry; if a plugin didn't get fenced, both the CLI and the GUI speak
  up at startup (sharing one check and one piece of copy).
- ✅ **Three fixes (2026-10-04, user feedback: "all fixed")**:
  ① `Workplace::is_unrestricted()` plus a short-circuit at the top of `Agent::workplace_reject`
     — under the full tier (the config default) the whole check doesn't run a single step, saving
     65~224 µs of `canonicalize` per tool call;
  ② `scan_paths`'s value-shape gate (contains a newline / longer than 260 characters ⇒ not judged
     a path) — fixes "2.2 KB of plugin source treated as one path": under a restricted tier
     `create_plugin` is no longer refused by mistake, and under full it no longer burns ~1 ms for
     nothing;
  ③ `max_processes`'s meaning changed to "how many more child processes a plugin may open",
     default 1 → 8 — see the next item.
- ⚠️ **`max_processes`' meaning changed once on 2026-10-04** (same round of feedback):
  it now means **"how many more child processes the plugin may open" (the plugin itself
  excluded)**, with `+1` when written into the Job (`ACTIVE_PROCESS` counts the whole Job, and
  the plugin takes one slot itself). The old number was "total processes including the plugin",
  so the default of 1 meant the plugin exactly filled its own quota ⇒ any plugin working through
  `subprocess` got `WinError 1816: not enough quota` (**plugins completely dead, not slowed**).
  The default is now 8, and `0` means "none at all"; the platform accounting is contained in one
  place, `SandboxConfig::job_process_limit`.
- Size (after the three fixes, the 2026-10-04 closing figure): `aox.exe` **3,209,216 B** and
  `aox-gui.exe` **6,088,704 B** — relative to the end of ③ that is +1,024 / +512 B (+0.03%):
  the full-tier short-circuit and the coarse-net gate are both "add one test, remove one
  filesystem call", and the `max_processes` change was semantics and copy.
- Landing ③: new `src/workplace.rs` (three tiers + boundary checks + the out-of-bounds contract)
  and `src/tool/hostfs.rs` (four host file tools), plus a `config.toml [workplace]` section —
  **default `full`, and not writing it leaves behaviour unchanged**.
  Hook-in points: `Runtime.workplace`, `Agent::run_turn` (opens the plot on the first message),
  `Agent::authorize_and_execute` (the check ahead of authorization plus the user-facing `Warn`),
  `Agent::new_conversation` (changes plots), one `host_file_tools()` registration each in the CLI
  and the GUI, and a new `/workplace`.
- New `ToolKind::Host`: the host file tools are **not folded into** `Meta` — `meta_tools` defaults
  to `ask`, and folding them in would pop an authorization dialog on every read and write, while
  the `fs_*` family of plugin tools is `auto`; two behaviours for the same thing is the hardest
  thing to explain.
- Size ③: `aox.exe` 3,139,584 → **3,208,192 B (+68,608, +2.19%)**; `aox-gui.exe` 6,034,944 →
  **6,088,192 B (+53,248)**. Much bigger than ①, because this block brings two new modules
  (~1,150 lines, of which the out-of-bounds contract copy and the whole set of unit tests are the
  bulk); still inside the ≤300 KB-per-stage budget.
- Evidence ③: `aox selftest` regression 8's A/B cross-check (a restricted tier blocks going out
  of bounds **and the file really wasn't created** / under the full tier the same call must
  succeed — a one-sided test can't tell you whether it was the workplace or something else that
  blocked it); plus 15 unit tests across the `workplace` / `hostfs` modules (`..` escapes, case
  sensitivity, `Pending` must refuse, URLs not misjudged as paths, and so on).
- ✅ **The GUI's "workplace" button (landed 2026-10-04)** — the interface entry point for ③,
  completed this round.
  At the **bottom-left** of the input box, a "workplace · <tier>" button → a three-way list pops
  upward (default / choose workplace… / full access) → "full access" is red and bold with a red
  confirmation → "choose workplace…" opens the **built-in egui directory picker** (drive chips /
  parent directory / subdirectory list / select this directory). New `gui/src/workplace_ui.rs`.
  - **The interface doesn't switch tiers itself**: `Workplace` lives inside `Runtime` and only
    the agent thread can reach it ⇒ new `ToAgent::Workplace(Mode)` (request) and
    `FromAgent::Workplace(WorkplaceView)` (acknowledgement). The acknowledgement is pushed
    **at startup / at the end of every turn / after every tier switch** (the third can't be
    skipped: the `default` tier's root isn't created until the first message, so without the
    resend the interface would show "not decided yet" forever). The tick compares tiers with
    `PartialEq`; it doesn't parse copy.
  - `Workplace::snapshot()`: **one lock acquisition** returns "tier + root" together, and
    `mode()` goes through it too — this is precisely what prevents "the first message happens to
    be creating the plot between two lock acquisitions", which left the interface showing a tier
    that didn't match the directory (a recurrence of trap 1). Regression
    `snapshot_agrees_with_mode_and_root` (including "after `default` is created, the tier must
    still be Default").
  - `Reject::model_text()`'s list of ways to get elevated gained the interface one (both front-end
    entries are now written out).
  - `Palette` gained `danger` (the **only** hue in the whole palette, used solely by "full
    access"); it is an ordinary overridable palette field ⇒ `PaletteConfig` gained it too,
    `tools/gen_themes.py`'s field count went 18 → 19, and
    `themes/{paper,ink,high-contrast}.toml` were updated in step.
  - Size (2026-10-04 closing figure): `aox.exe` **3,210,240 B** (+1,024), `aox-gui.exe`
    **6,124,544 B** (relative to 6,088,704 after the three fixes, **+35,840 / +0.59%**) — about
    640 new lines of interface code, still inside the ≤300 KB-per-stage budget.
  - Evidence: `--demo` hands-on screenshots checked screen by screen (button / popping upward /
    tick / red confirmation / directory picker / entering a subdirectory / the button renaming
    after selection / collapsing when clicking outside); `cargo check --all-targets` zero
    warnings, `cargo test --workspace` 122 + 12 all passing, `aox selftest` all passing.
- ✅ **Read/write semantics corrected + the top-bar Σ panel offset (2026-10-04, user feedback:
  "1. fix the offset, 2. one small change…")**:
  - **New semantics: reads are unrestricted; writes and deletes are confined to the workplace.**
    `workplace` gains `Access::{Read, Write}` and `check_access`: `check` = `Write` (behaviour
    as before), `check_read` = `Read` — **never returns `Err`**, it only normalizes the path (and
    reads even when `Pending`, "the plot isn't open yet"). `read_file` / `list_dir` go through
    `hostfs::guard_read` (unrestricted); `write_file` / `remove` still go through `guard`
    (restricted). `Agent::workplace_reject` gained a `name` parameter and is routed by tool name
    through `tool::host_tool_access`; **the plugin coarse net is always judged as "write"** (the
    host can't see a plugin's intent, so it takes the conservative direction: better to block one
    read too many).
    Reason: reading takes information, writing and deleting change the device (irreversibly), so
    the consequences are asymmetric — putting an elevation flow in front of "read some material
    elsewhere" only pushes users into leaving `full` on permanently, which makes the boundary
    looser instead. The price is written honestly as the first item under "known limits" in
    `docs/SANDBOX.md` (**this layer does not stop "reading the whole machine"**; closing reads too
    requires ②a alone).
  - **The Σ usage panel offset on `token_chip` is fixed**: `Area::fixed_pos` and `ui.allocate_*`
    are both in **client-area local coordinates**, and a "viewport origin" term,
    `viewport().inner_rect.left_top()` (**screen coordinates**), had been added by mistake ⇒ the
    panel shifted wholesale with the window's position on the monitor (measured ~(154,14)
    logical pixels off). It now uses `resp.rect.left_bottom() + vec2(0,6)` directly as the
    anchor. The same mistake had already been fixed in `workplace_ui::menu`; this one was
    reported then and fixed here in the same round, an existing bug.
  - Size: `aox.exe` **3,215,872 B** (relative to 3,210,240 when the GUI landed, **+5,632 /
    +0.18%**); `aox-gui.exe` **6,123,520 B** (relative to 6,124,544, **−1,024 / −0.02%** —
    removing that "screen coordinate conversion" exactly cancelled the new copy). Both inside the
    ≤300 KB-per-stage budget.
  - Evidence: `cargo check --workspace --all-targets` zero warnings; `workplace` gained
    `read_is_never_restricted_but_write_is`, `hostfs` gained
    `read_outside_is_allowed_but_remove_is_not` (a cross-check of **out-of-area read allowed +
    out-of-area delete blocked**; pinning only one side would make it a test that is always
    green); `selftest` regression 8 gained "out-of-area read allowed / out-of-area delete
    blocked", and the "relative paths must be refused" check that used to hang on `read_file` now
    hangs on `write_file` (under the new semantics reads do **not** refuse relative paths).
- ✅ **Two small workplace interface tweaks (2026-10-04, raised after the user accepted the
  interface by hand with "the interface is fine")**:
  - **The directory picker's "select this directory" moved from the bottom-left to the
    bottom-right**: that row became a single `ui.horizontal` — the explanatory copy is still on
    the left, and the right side uses a `with_layout(right_to_left)` child layout (which eats the
    remaining horizontal width, so the button placed into it first ends up flush right). Same
    arrangement as "Cancel" in the full-access confirmation.
  - **The pull-up menu keeps the explanatory copy only on the "default" row**, with
    `选择工作区…` / `完全访问` becoming single-line rows — the former needs no explanation because
    "which directory is currently selected" is already written on the button ("workplace ·
    <directory name>"), and repeating it would only make the menu taller; for the latter, being
    red and bold *is* the explanation.
    Along with that, `row()` now supports **two row heights** (46 for two lines, 32 for one, with
    the title centred as a whole on a single line) — if a single line reused 46, the text would
    float in the upper half and look unfinished.
  - Size: `aox.exe` 3,215,872 B (unchanged), `aox-gui.exe` **6,123,520 B** (same as the previous
    item: the two blocks of copy removed and the new row-height/layout branches cancelled out
    exactly).
- ✅ **Naming unified on `workplace` (2026-10-04, two rounds)**. Previously one feature had three
  names: the CLI command `/workspace`, the config section `[workspace]`, and the actual directory
  `workplace/` (only the GUI's copy used the last one).
  **Round one** (the user noted "the command line says workspace, the GUI says workplace")
  changed the three faces the user can see; **round two** (the user noted "rename the file
  `src/workspace.rs` too") changed the code side along with it.
  - Round one: CLI command → `/workplace`; config section → `[workplace]`; the directory name was
    already `workplace/`, so it stayed.
  - Round two: `src/workspace.rs` → **`src/workplace.rs`**, `gui/src/workspace_ui.rs` →
    **`gui/src/workplace_ui.rs`**; the types `Workspace` / `WorkspaceConfig` / `WorkspaceView` /
    `WorkspaceUi` → `Workplace…`; the field `rt.workspace` → `rt.workplace`;
    `paths::resolve_workspace` → `resolve_workplace`. A mechanical repo-wide replacement, with
    the compiler as the backstop.
  - **Both old names are kept**: `/workspace` is an implicit alias of the command (not written
    into `/help`), and `[workspace]` goes through serde's `alias`. Without that, a user with
    `[workspace]` in `config.toml` would be **silently ignored** by serde (an unrecognized
    section is skipped) ⇒ they'd think the boundary was still on while it had actually fallen
    back to `full` — the hardest kind of "you wrote it and it did nothing" problem. Each of the
    two is pinned by a regression (`command_answers_to_both_names_and_parses_args`,
    `workplace_section_accepts_the_legacy_name`, the latter also pinning that "writing both names
    in one section must error rather than silently picking one").
  - A bug in **how `args` was stripped** was fixed along the way: it used
    `trim_start_matches("/workspace")`, which strips the prefix **repeatedly**, so with a
    directory actually named `/workplace`, `/workplace /workplace` was stripped to the empty
    string = "no argument", and the user got the current state instead of a switch, with no
    notice. Now it matches the name exactly, strips the prefix once, and removes one space.
  - One stale sentence was fixed along the way: the "AOX will only **read and write** files in
    this directory" shown when `Agent::run_turn` opens the plot was written before the semantics
    changed, and would make users think reads were closed too (so they'd elevate for nothing);
    it now reads "writes and deletes are confined to this directory (reads are unrestricted…)".
  - ⚠️ **The lesson from the mechanical rename**: `--workspace` (**cargo's parameter**) was also
    in the replacement scope and became `--workplace`, turning 18 commands in the docs into
    fakes. `[workspace]` (cargo's manifest section) escaped only because `Cargo.toml` was
    deliberately left out of scope. **A rename script must define its scope first and grep to
    double-check right after** — looking only at "does it compile" can't find fakes like that.
  - Size: `aox.exe` 3,215,872 → **3,215,360 B**, `aox-gui.exe` 6,123,520 → **6,123,008 B**
    (−512 each; a pure rename with net zero growth). Verification:
    `cargo check --workspace --all-targets` zero warnings, `cargo test --workspace` **127 + 12
    all passing**, `aox selftest` all passing; plus a **temporary application root** run
    hands-on (with the **old section name** `[workspace] mode = "default"` in config.toml ⇒
    correctly reporting "tier: default"; both `/workplace` and `/workspace` responding; and
    `/workplace /workplace`'s argument not being swallowed).
- ✅ **The workplace follows the conversation + `/new` returns to the default tier + GUI
  acknowledgement (2026-10-04, the user reported "every time I create a new conversation the
  default workplace is still the previous one" and "every time I enter the same conversation it's
  a different workplace")**. Two symptoms, three different causes, all fixed together:
  - **The GUI was missing an acknowledgement** (a real bug, the direct cause of the first
    symptom): `ToAgent::NewConversation` and `ToAgent::Resume` both pushed `Switched` +
    `UsageLog`, but **not the workplace acknowledgement** ⇒ the button kept showing the previous
    conversation's (or even the previous click's) tier name, i.e. a stale value. One
    `sync_workplace` was added to each of the two.
  - **`/new` always returns to the "default" tier**: it used to reset only when
    `cfg.mode == "default"`, so "picked a directory in the previous conversation → click + → the
    new conversation is still in that directory". `reset_for_new_conversation` now returns to
    `Pending` unconditionally. ⚠️ The price (accepted by the user): a user with
    `mode = "fixed"` in config gets the plot named by the config only for the **first**
    conversation of this launch.
  - **The workplace is persisted per conversation**: `.jsonl` gains a record kind
    `{"kind":"workplace","mode":…,"root":…}` (`Store::{append,load}_workplace`, reading takes the
    last one); `Agent` **compares first** and then appends "at the start of every turn" and
    "after every tier switch" (no write when nothing changed, so the file doesn't fill with
    duplicate rows), and `resume` reads it back → `Workplace::restore`. `rewrite` (interrupt
    rollback) also fishes it out and puts it back as-is — a rollback should lose messages only,
    not also erase "which plot this conversation is in".
  - **`snapshot()`'s tier source was fixed along the way** (new `State::picked`): the test is now
    "how it came about at runtime", no longer derived from `cfg.mode`; the `cfg` field in
    `Workplace`, which existed only to fix the startup tier, was deleted outright — keeping it
    only tempts people into deriving the tier from config again.
  - Verification: `cargo check --workspace --all-targets` zero warnings · `cargo test
    --workspace` **131 + 12 all passing** (4 new:
    `snapshot_follows_the_runtime_origin_not_the_config` /
    `new_conversation_always_returns_to_default` / `persisted_and_restore_round_trip` /
    `workplace_record_keeps_the_last_one_across_rewrite`) · `aox selftest` all passing.
    Hands-on (temporary application root) covering three things: `mode = "fixed"` ⇒ after `/new`
    it reports "default" with the root not yet decided; a hand-written old archive with a
    `workplace` record ⇒ after `/history open` the workplace is restored to **its own** plot;
    issuing `/workplace full` in a conversation ⇒ `{"kind":"workplace","mode":"full"}` really is
    appended, and reopening that conversation reports "full access".
  - Size: `aox.exe` 3,215,360 → **3,218,944 B**, `aox-gui.exe` 6,123,008 → **6,127,616 B**.
- ✅ **Factory default changed to the "default" tier · the default tier binds to the
  conversation's own plot · three transcript-stream items (2026-10-05)**:
  - **`[workplace]`'s factory default went `full` → `default`** (restricted). What the user's
    "every time I enter the app it's still full access" actually struck was that the original
    requirement had never been delivered: back then, to protect "no config = byte for byte
    unchanged behaviour", the default was left at `full` and "confined at startup" was pushed onto
    the GUI button to express — while the button only reflects config faithfully and will never
    tighten itself. ⚠️ One principle is hereby voided: **"not writing `[workplace]` = byte for
    byte identical to before this layer existed" no longer holds**; you must write
    `mode = "full"` explicitly to be unrestricted. A new regression,
    `config::tests::factory_default_is_the_confined_mode`, pins this **product decision**.
  - **The default tier now binds to "the plot this conversation created itself"**: the user
    reported "switch to full access and back to default and it creates another workplace,
    accumulating endlessly". `State` gained `own` (because `active` gets overwritten by a tier
    switch, so looking at it alone you can't tell which plot was the original);
    `set_mode(Default)` returns to `own` first (with the notice changed to "this conversation is
    still in …"), and only returns to `Pending` when it was never created. The persisted record
    became `workplace::Record{mode, root, own}`, and `own` is persisted **even under the
    full/fixed tiers** (without it, reopening the conversation and switching back to default
    would create another plot). Regression
    `switching_back_to_default_reuses_the_conversation_own_dir` (switching back and forth 3
    times, the directory count must still be 1).
  - **Three transcript-stream items** (the user raised: "the tool row's arguments are so long you
    can't scroll to the end" / "on a long task you can't tell how far it's got" / "the reply only
    pops in all at once when everything is done, and until then the conversation is black"):
    ① `step_row` no longer inlines arguments (`StepView.args` keeps the field, marked
    `allow(dead_code)`, so debugging can still match it against the jsonl);
    ② the built-in prompt gains a section, "# progress reports" — the channel already existed
    (`agent_loop` emits `InterimText` on "carries tool calls"); what was missing was only
    **getting the model to speak**;
    ③ body text types out as it streams: the deltas from `StreamSink::on_content` **already**
    reached the GUI (`FromAgent::Content`), but were only used to count tokens, with the body
    still waiting for `AssistantText` ⇒ they now go into `Live.streaming` and are rendered live
    by `live_block` (stage notes one step lighter, the sentence currently typing in normal
    colour).
    The typing speed is the model's real output speed; there is no local "speed it up" knob.
  - Verification: `cargo check --workspace --all-targets` zero warnings · `cargo test
    --workspace` **133 + 12 all passing** · `aox selftest` all passing · `/prompt` shows 3
    built-in segments (the new text landed inside the protocol segment).
    Size: `aox.exe` 3,218,944 → **3,220,480 B**, `aox-gui.exe` 6,123,520 → **6,125,056 B**.
  - ⚠️ **The GUI hands-on screenshots didn't happen this time**: the `aox-gui.exe` process was
    alive but **created no window** (enumerating top-level windows by PID returned 0, logs empty),
    while the same binary ran fine in earlier rounds — most likely an issue with this session's
    desktop/focus environment, not a program problem. These three items **need you to go through
    them on real hardware**.
- **Next**: the 30-minute spike on ②b (if it works, ②a can be demoted to a fallback).
