# AOX subagents (spawning our own instance headless) · project charter

- **Status**: **chartered, deferred** (user decision 2026-10-09 — the design was worked out in
  full, then deliberately parked: the change is large, the project is intentionally lightweight,
  and at this size a subagent doesn't clearly earn its keep).
- **Origin**: the user asked whether the GUI could "start a subagent — another instance of
  itself — but running headless (`/headless`), with no window popping up". For the CLI it is
  trivially simple (just another background process).
- **This file** is the charter: the feasibility finding, the design, the open decisions, and the
  reasons for deferring. Nothing here is implemented.

> **Revisit trigger** (when this becomes worth building again): a task that genuinely needs
  **parallel independent exploration** (read-only) or **context isolation** — i.e. work big enough
  that one linear loop becomes the bottleneck, or where dumping everything into one context
  degrades it. Until then, the default single loop is the cheaper and more honest answer.

---

## 1. Feasibility verdict: the project is almost built for this

The host already owns a **complete "spawn a child process + line-JSON protocol + fence +
reap"** facility — it is currently used to run plugins. A subagent is **the same facility with a
different protocol**. Nothing about the idea fights the "single exe, a few MB" physique.

| What is needed | Existing asset | Location |
|---|---|---|
| A background **windowless** process | the unified `CREATE_NO_WINDOW` wrapper (plugins / python / node / rustc all go through it); the comment already states the motive is "child processes must not flash a black window" | `src/plugin/mod.rs:85-116` |
| freeze → Job Object → resume | `Sandbox::create_flags()` + `contain()`, already including a UI restriction | `src/sandbox.rs:94,113` |
| pipes + stderr to a file + child dies with the parent | `spawn_command`: `stdin/stdout` piped, stderr → file, `kill_on_drop(true)`, `current_dir(root)` | `src/plugin/mod.rs:766-802` |
| a line-JSON protocol | the plugin RPC is already "one JSON per line", with the discipline "stdout carries only the protocol, logs go to stderr" | `src/plugin/mod.rs` |
| a UI-less `Ui` | `trait Ui` has only **4 required methods**; everything else has a default body | `src/ui/mod.rs:77-129` |
| a precedent for a non-interactive entry | the `selftest` subcommand and `--dump-config`: parse args, do the work, return — no window is ever built | `src/main.rs:241,269` |
| adding a tool | `trait Tool` | `src/tool/mod.rs:107-148` |
| returning a result | `ToolOutput { content, is_error, attach }` | `src/tool/mod.rs:48` |
| usage accounting | the `kind:"usage"` line + `UsageRecord` are already a format | `src/history.rs:146` |
| a fast start | plugin lazy-loading is already the default ⇒ a child does **not** have to wait out cold probing (otherwise every subagent would cost up to 20 s) | `src/config.rs [plugins]` |

## 2. The one fact that kills the naive route

**The GUI installer ships only `aox-gui.exe` — there is no `aox.exe`**
(`installer/aox-gui.iss:122`; the two installers have different `AppId`s and share no directory).

So "the GUI starts a CLI instance" **does not work**. The correct route is to **self-spawn
`current_exe()`** — and adding a `--headless` branch to `aox-gui.exe` needs no new binary and adds
**no dependencies and no size**. Two binaries already link all the core code, so a `--headless`
branch is free; a third exe is not needed.

## 3. The six gaps (two are the real difficulty)

| Gap | Size |
|---|---|
| a `--headless` entry point (neither binary has one) | small |
| `HeadlessUi` (implements `Ui`) | ~60 lines |
| protocol + Runner | medium |
| **history isolation** — a child using the same app root writes to `<root>/history/*.jsonl`, **directly polluting the GUI sidebar** | **critical** |
| **authorization policy** — `AuthLevel::Ask` reaches `Ui::authorize`, and headless has nobody to answer | **critical** |
| recursion / concurrency caps | small |

### 3.1 History pollution (must be solved, or the sidebar fills up with strangers)

A child inherits the same `config.toml` and the same app root ⇒ `default_store(root)` writes its
conversation to `<root>/history/<ts>.jsonl`, and `Store::list` **scans exactly that directory**
(`src/store/jsonl.rs:62`). So one subagent run leaves a stray session in the GUI sidebar.
`Store` deliberately does not expose a path, so this cannot be papered over with a "subdirectory"
argument — the clean fix is a **config snapshot** (below).

### 3.2 Authorization pass-through (must be solved, or the subagent silently stalls)

In `authorize_and_execute` (`src/agent.rs:529`) `AuthLevel::Ask` goes through `ui.authorize`.
Headless has nobody to answer ⇒ the default body can only refuse. A subagent therefore needs an
**explicit** policy source; it cannot rely on luck.

### 3.3 Progress visibility (decides the v1 experience; can slip)

`Tool::call(&self, args, rt) -> ToolOutput` has **no progress channel** — this is the **only**
place in the whole design where a core interface would have to change. Optimistic reading: this is
exactly where a would-be **eighth seam** should act (the same family as `Ui::stream_sink`).
Pessimistic reading: v1 is a black-box call whose result arrives only at the end. **Recommendation:
accept the latter for v1.**

Two more easy-to-trip details: `Tool::call` has no access to `AgentEvent`, so a subagent's events
**must** travel as protocol lines rather than a direct `ui.emit`; and `ProcessFlags` is currently
`pub(crate)` in `plugin/mod.rs`, so the subagent runner would first need it moved up to
`sandbox.rs` or a new `proc.rs`.

## 4. Design

### 4.1 Layering — in the existing "seam" style

Add `trait AgentRunner`; `Runtime` holds an `Arc<dyn AgentRunner>`; this round's only impl is
`ProcessRunner` (self-spawn). Swapping in "an in-process Agent" (no process overhead) or "a remote
runner" later then needs no change on the caller side. Config-side: a new `[subagent]` section,
**disabled by default** ⇒ the zero-drift promise holds.

### 4.2 Protocol — bidirectional line JSON, reusing the plugin discipline

| Direction | Messages |
|---|---|
| parent → child | `task{prompt, workspace, policy, depth}` · `cancel{}` · `auth_reply{}` |
| child → parent | `ready{model, tools, workspace}` · `status{text}` · `tool{name,args,result,is_error}` · `usage{...}` · `auth_req{tool,args}` · `final{text, tokens, transcript}` |

Discipline copied from plugins: **stdout carries only the protocol; stderr is redirected wholesale
to `temp/subagent/<job>/stderr.log`** — an uncleared pipe deadlocks, and the GUI build is
`windows_subsystem="windows"` (no console), so the user cannot see stderr at all and diagnostics
must land in a file.

### 4.3 Isolation on three axes

| Axis | Mechanism | Why |
|---|---|---|
| **config** | the parent dumps its **effective** config to `temp/subagent/<job>/config.toml`; the child reads it via `--config` | ① a mid-session config change cannot make parent and child drift ② the user's `config.toml` is not polluted ③ it **also settles the history axis** — the snapshot can point `[store]` at its own directory, or set `[history] enabled = false` |
| **history** | the child never writes the parent's `history/`; its transcript goes to `temp/subagent/<job>/transcript.jsonl`, whose path is returned in `final`, and the parent folds it into **its own activity stream** | the sidebar stays clean, and the child's output is still visible (through the parent's Activity, a chain that already persists with the conversation file) |
| **workspace** | inherit the parent's snapshot (root + tier); **v1 recommends read-only** | the boundary only stops "going out of bounds", **not concurrent writes to one directory by parent and child** — two processes editing one file means the later write silently wins and the earlier change is lost |

### 4.4 Authorization

- **v1**: the config snapshot carries the parent's `[auth]`, plus the parent's `session_allowed`
  allow-list passed as `--allow-session`; still-undecided calls follow
  `[subagent] on_ask = "deny" | "allow"`, **default deny**, and refused calls are recorded in the
  `final` summary.
- **v2**: carry `auth_req` protocol lines through to the parent's `Ui::authorize` — the GUI pops
  **the same card**, the CLI asks in the terminal. Semantically clean: **a subagent's authority
  never exceeds what the parent grants on the spot.**

### 4.5 Lifecycle and caps

- Depth: `AOX_SUBAGENT_DEPTH` env var, parent passes `depth+1`; above `max_depth`, `spawn_agent`
  errors immediately. **Default `max_depth = 1` (recursion off).**
- Concurrency: `[subagent] max_concurrent` (2–3 suggested); over the limit it **errors rather than
  queues** (queueing makes the model think it is stuck).
- Timeout: a **separate** `[subagent] timeout_secs` is required — reusing
  `[agent] tool_timeout_secs` (default 60 s) is far too short for a subagent.
- Reaping: reuse `kill_on_drop(true)` + the Job Object. "Parent exits ⇒ everything dies" is free;
  "parent lives, kill just this one subagent" needs the `cancel` protocol line plus an explicit
  kill — that is P2.

### 4.6 Observability

The child's `usage` lines fold into the parent's `UsageRecord` (tagged with a `subagent` origin),
so `/usage` total accounts are the true accounts — otherwise the tokens a subagent burns vanish
into thin air. From P2, the activity stream draws **nested swimlanes** (the existing multi-segment
Activity mechanism can be reused).

## 5. Phasing

| Phase | Content | Notes |
|---|---|---|
| **P0** | `--headless --prompt` one-shot run + a single-line JSON result; the `spawn_agent` tool **blocks synchronously**; no progress, no auth pass-through, no recursion | **touches no core interface**; once it runs end to end it can be accepted |
| **P1** | protocolise (incl. status/usage/auth_req) + three-axis isolation + depth/concurrency/timeout caps | the real difficulty lives here |
| **P2** | add a progress sink to `Tool::call` → live nested swimlanes in the activity stream; background tasks + `agent_wait`; targeted cancel | the only place core interfaces change |
| **P3** | an in-process runner (no process overhead) / a remote runner | optional |

## 6. First thing P0 must test

Under release, `aox-gui.exe` has no console (`gui/src/main.rs:26`). When the parent takes over its
stdio with pipes, can `println!` actually reach the pipe? On paper yes (`STARTF_USESTDHANDLES`
sets the handles up), but this is the class of thing I have seen hold on paper and fail in
practice. **The first P0 task should therefore be a ~30-line empty-shell child:** the parent spawns
itself with `--headless`, the child prints one line `{"type":"ready"}`, and the parent reads it —
that is a pass. Once this passes, the whole design has no dangling premise.

## 7. Decisions still to be taken (five)

1. **May a subagent write?** Recommendation: read-only. v1 serial + read-only; writing goes back to
   the parent (or to a later serialised subagent) — concurrent writes silently lose changes.
2. **Default depth?** Recommendation: 1 (recursion off), configurable up to 2–3.
3. **P0 synchronously blocking, or straight to background tasks?** Recommendation: synchronous
   first — it answers "does this actually work" fastest.
4. **Should a subagent's conversation be visible in the GUI sidebar?** Recommendation: no; output
   surfaces through the parent's activity stream. Making it visible would force a different history
   isolation design (subagent sessions nested under the parent session) at a higher cost.
5. **`on_ask` default deny or allow?** Recommendation: deny; allow turns a subagent into an
   "unapproved executor", a risk bigger than the convenience it buys.

## 8. Risk

**Medium.** The mechanism is largely already present, so the build risk is not the problem — the
problem is that the **value is not proportional to the change** at this project's size:

- The main selling point is "single exe, a few MB, starts with no interpreter at all". A subagent
  adds a whole second process lifecycle, a protocol, three isolation axes and an auth policy to
  maintain — for a benefit that only appears on genuinely large, parallel or context-heavy tasks.
- Until the core interface (`Tool::call`'s progress channel) is opened up, v1 is a black box, so
  the experience gain is limited even when it works.
- Therefore: **keep the default single loop**; build this only when a concrete task forces it (see
  the revisit trigger at the top).
