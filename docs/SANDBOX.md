# The fence around plugin subprocesses, and the sandbox roadmap

> This document answers two questions: **what is actually blocked right now**, and **what
> blocking more would cost**.
> What is implemented is "① the Job Object fence" and "③ the workplace boundary (file capability
> taken back by the host)" (both 2026-10-04).
> ②a / ②b / ④ are not built; each one's price is written below.

## 0. In one sentence

**What exists right now is a "fence", not a "sandbox".**

A fence covers three things — process count / memory / UI — and is enforced by the Windows
kernel; **files and the network are entirely outside its reach**. A plugin can still delete any
file you have permission to delete. That sentence is printed verbatim in `/sandbox`'s output
too — it is not a disclaimer, it is a fact you have to know before using it.

## 1. Threat model: first be clear about who you are defending against

AOX's plugins **are not third-party products; the AI writes them on the spot**. So the real
ordering of risk is:

| # | Risk | Typical shape | Who can fix it |
|---|------|----------|--------|
| ① | **the AI writes it wrong** | `fs_rm(recursive=True, path=".")`, a mis-joined path, recursing into the wrong directory | narrowing the capability is enough (③ / ④) |
| ② | **the AI is tricked by content** | a file it read hides instructions telling it to delete or upload something else (prompt injection) | narrowing the capability is enough (③ / ④) |
| ③ | **a malicious third-party plugin** | a plugin directory copied in from elsewhere | **only a real sandbox** (②a) |

①② are solved by "don't let dangerous operations appear where the model can write them", at
almost no cost; ③ needs kernel-level isolation, an order of magnitude more expensive.
**Which step to take first depends on which of these you are actually worried about.**

## 2. Option comparison

Sizes are measured against `aox.exe` at 3.12 MB. The numbers ①②③④ double as "which layer
blocks it".

| | Option | Code size | Binary size | Really blocked by the kernel | Explicitly not blocked |
|---|---|---|---|---|---|
| **①** | **Job Object fence** ✅ done | ~340 lines | **+16.5 KB** | runaway process count (fork bomb, **8 more child processes by default**), memory ceiling, everything dies when the host dies, clipboard/desktop blocked | **files and network** |
| **②a** | AppContainer, a real container | ~400–600 lines | +150–300 KB | out-of-bounds files (read+write+delete), network, registry, cross-process communication | legitimate but out-of-bounds plugins stop working |
| **②b** | low integrity level (write downgrade) | ~150 lines | +5–10 KB | every kind of "write": ordinary files, registry, system settings | **reads**, network, process count |
| **③** | file capability taken back by the host | ~600 lines | **+68 KB** | risks ① and ② in the table above | a plugin that doesn't play by the rules (calls `open()` itself) |
| **④** | WASM / WASI | huge | **+3–5 MB** | everything, and cross-platform | it would overturn the whole Python/Node/Rust subprocess plugin asset |

Lateral comparison: on Linux, reaching ②a's effect takes only `pre_exec` +
`unshare(CLONE_NEWNS|CLONE_NEWNET)` + bind-mount + `setrlimit` + `PR_SET_NO_NEW_PRIVS`, about
120 lines with no dependencies.
**Windows has no single API that gets you there, so it has to be assembled from pieces.**

## 3. Implemented: ① the Job Object fence

The code lives in `src/sandbox.rs` (configured under `config.toml [sandbox]`) and hooks into
exactly one place, `plugin::spawn_command`. Every plugin process — first launch,
`create_plugin`, crash self-healing restart — takes the same path; there is no second one.

### The four things it blocks

| Limit bit | Failure mode it closes |
|--------|----------------|
| `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` (default = 8 **child** processes) | runaway process count. A plugin plus the descendants it spawns (**children inherit this Job automatically** and can't leave the fence) are capped at that number — a fork bomb grows by doubling, so a cap stops it getting off the ground. `0` locks this path down completely |
| `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` | orphan processes. A host crash, or a `taskkill`, counts too — you don't end up with a pile of pythons in the background |
| `JOB_OBJECT_LIMIT_PROCESS_MEMORY` (**off** by default) | runaway memory. For why it's off by default, see below |
| `JOB_OBJECT_UILIMIT_*` | a plugin reaching for the host's window / clipboard / desktop / global atoms |

> ⚠️ The first row **was changed once on the day it shipped**. It began as `= 1`, written into
> the Job as a total that **included the plugin itself** — meaning "only 1 process allowed in
> this Job", which the plugin filled on its own, so **any plugin that does its work through
> `subprocess` was dead under the default configuration** (calling `curl`, `git`, or an
> interpreter all gave `WinError 1816: not enough quota`). That was over-design, not a
> configuration problem. Now: **8 child processes are allowed by default**, and the protection
> changed from "none at all" to "a concurrency cap" (children inherit the Job, so a cap is
> enough); write `max_processes = 0` to go back to strictest. The platform's "including itself"
> accounting is contained in one place, `SandboxConfig::job_process_limit`, so the
> configuration layer only talks about what the user cares about.

### Three non-obvious design decisions

**① One Job per plugin, not one global Job.** `ACTIVE_PROCESS` is an allowance for **the whole
Job**. With a shared Job the number would become a global budget — one plugin could eat the whole
allowance and the others couldn't even start, and the error would point somewhere else. One Job
per plugin makes the allowance really mean "how many more child processes this plugin may open"
(which is why the `+1` in `job_process_limit()` can only be the plugin itself).

**② Children are born `CREATE_SUSPENDED`.** Putting them into the Job after birth leaves a
window: between `spawn()` returning and `AssignProcessToJobObject`, the child is already running
its first line of code. Freezing it removes that window. The price is finding the main thread
handle yourself (neither `std` nor `tokio` hands it over): one ToolHelp snapshot + `OpenThread` +
`ResumeThread`. **The resume step must never be skipped** — miss it and the process freezes
forever, which shows up as "the plugin's initialize handshake timed out", nothing to do with the
fence and extremely hard to diagnose.

**③ The fence is "best effort", and failure has to speak up.** The host may already be inside a
Job itself (started by an IDE, a terminal, a scheduled task). Windows 8+ allows nested Jobs, but
the outer policy may forbid it, and then `AssignProcessToJobObject` fails — in that case **let
the plugin through as usual**, record the reason in `Fence`, and have `/sandbox` and the startup
warning say honestly "this plugin was not fenced".
(That is exactly the `fence ✓ / ✗` marker after each plugin in `/plugins`.)
Silent degradation is unacceptable, but so is "no fence, no plugin" — the fence is
reinforcement, not a precondition.

### Self-check evidence (`aox selftest`, regression 7)

**Three-way A/B**, and all three have to pass — testing only one side proves nothing ("it was
blocked" may just mean the logic never ran):

```
[selftest]   default allowance: SPAWN: rc=0 out=child-ok
[selftest]   allowance locked to 0: SPAWN-BLOCKED: OSError: [WinError 1816] Not enough quota is available to process this command.
[selftest]   fence off: SPAWN: rc=0 out=child-ok
```

All three lines are the same plugin "trying to spawn a child process"; only the configuration
differs:

- **default allowance (8 child processes)**: the child really ran (`child-ok`) — a plugin has to
  be able to work; that is the floor for the default value;
- **`max_processes = 0`**: it didn't run => this path can still be locked down, and fork-bomb
  protection is intact;
- **fence switched off entirely**: it ran again => the "blocked" in the middle row really is
  attributable to the Job, not to some other fault in this machine's python.

The assertion is on the **content** (whether the child actually ran), not on the wording of an
error code.

> This A/B caught a real bug on the spot: originally `enabled = false` only turned off the
> "freeze first" half, and the Job was still created, so the control group for "fence off" still
> got 1816 — meaning `enabled = false` was **fake** at the time. A one-sided test can never find
> that.

Also, the whole existing `selftest` chain (create plugin -> register -> call -> CJK round trip ->
cwd check -> uninstall) passes with the fence **on** — which incidentally proves the fence
doesn't starve the plugin itself.

Running the real `aox`: all 8 real plugins (both python and rust) show `fence ✓`.

### Known limits

- **Files and network**: out of reach. To block file deletion you have to go through ②a or ③.
- **`max_processes` changed meaning (2026-10-04)**: it now counts **how many more child
  processes the plugin may open** (the plugin itself excluded), default `8`, `+1` when written
  into the Job; `0` = none at all (the only setting that truly locks this path down). The old
  meaning was "total processes including the plugin", default 1, so plugins that work via
  `subprocess` were dead under the default configuration (`WinError 1816`) — exactly what this
  change fixed.
- **Children inherit the Job**: a plugin's descendants belong to the same Job automatically
  (`BREAKAWAY_OK` is not set), so the memory / UI limits and "everything dies when the host
  dies" apply to them too; `ACTIVE_PROCESS` caps only the **concurrency count**.
- **The memory cap is off by default**: exceeding it **kills the whole process in the kernel**,
  it is not one failed allocation. A cap that is too tight will kill a plugin outright that is
  legitimate but memory-hungry, and nothing is visible from the plugin's side — it just looks
  like "it keeps getting restarted for no reason". Turn it on deliberately if you want it.
- **Non-Windows**: the whole module degrades to a no-op, and **doesn't pretend to succeed** --
  `Fence::active = false` with the reason spelled out as "this platform has no Job Object", and
  `/sandbox` reports it honestly.
- **Changing the config needs a restart**: the fence is fixed at spawn time, so editing
  `[sandbox]` during a run only affects the next spawn / restart.

## 4. Implemented ③: the workplace boundary (file capability taken back by the host)

The host ships four built-in file tools, `read_file` / `write_file` / `list_dir` / `remove`
(`src/tool/hostfs.rs`) — parameters and implementation are both in the host's hands, so "**writes
and deletes must pass the workplace check first**" **cannot be bypassed** (unless the model
doesn't use them and builds its own plugin instead, which is a different class of risk, see §1).

**Three tiers** (`src/workplace.rs`, `[workplace] mode` in `config.toml`):

| Tier | Meaning | What it governs when restricted |
|------|------|------|
| `default` | **the app root itself** — every new conversation starts there (changed 2026-10-08; previously each conversation opened a fresh `<app root>/workplace/<YYYYMMDDHHMMSS>/` plot). `temp/` for scratch, `workplace/<project>/` for durable deliverables | writes and deletes only inside the app root; **reads are unrestricted** |
| `full` | unrestricted. **You have to write `mode = "full"` explicitly** — the factory default is `default` (changed 2026-10-05) | governs nothing |
| `fixed` (with `root = "..."`) | one fixed root (the GUI's "choose workplace" is equivalent to it) | same as `default` |

Under the confined tiers the AI also keeps its **own dated notes**: on first use the host lays down
`<root>/.aox/memory/`, and the model writes one `<YYYY-MM-DD>.md` per day there (what it did, the
pitfalls, what got finished) —— inside a project it uses that project's own `.aox/memory/`. The
prompt teaches the convention; the host only creates the folder (and hands the model today's date).

**Reads and writes are asymmetric (the semantics settled on 2026-10-04; the single most
important rule at this layer)**:

| Tool | Intent (`workplace::Access`) | When restricted |
|---|---|---|
| `read_file` / `list_dir` | `Read` | **the whole device is readable, unrestricted by the workplace** (it reads even when "the root isn't established yet") |
| `write_file` / `remove` | `Write` | must be inside the workplace; relative paths are always refused |

Why split them: **the consequences are asymmetric** — reading takes information, writing and
deleting change the device (and irreversibly). Wanting to read material that lives somewhere else
(logs, configs, another project) is a very common legitimate need; putting an elevation flow in
front of it only pushes users into leaving "full access" on permanently, which makes the boundary
looser instead. Whereas "AOX may only put its pen down inside the root drawn for it" is what this
layer actually has to protect. Its price is in the first item under "known limits".

Switching at runtime: `/workplace` at the command line (show the current state) and
`/workplace full|default|<directory>` (switch tier). Both the check and the execution live in the
`workplace` module, so the CLI and the GUI share one implementation and one body of copy.

**Four non-obvious design decisions**:

- **Relative paths are always refused under a restricted tier (for writes/deletes only).** The
  host and the plugin subprocess don't share a working directory (a plugin's cwd is the app root,
  see `plugin::spawn_command`), so the same string `config.toml` points at two different files on
  the two sides — and "the host thinks it's inside while the plugin actually went outside" is far
  more dangerous than a few extra characters. The refusal text tells the model the root directly
  so it can rewrite the call. **Reads don't refuse relative paths**: reading the wrong file at
  worst reads a same-named file elsewhere, and that ambiguity (different cwd) is only fatal for
  writes.
- **The coarse net over plugin tools judges by the plugin's declared intent (since 2026-10-05).**
  A plugin is a black box to the host, which can't tell whether a given path argument is for
  reading or writing; and "blocking one write too few" costs far more than "blocking one read too
  many", so **anything undeclared** is treated as a potential write. A plugin can add a fourth
  field, `"read"`, to a tool declaration to remove that collateral damage (Python SDK:
  `("<description>", <schema>, <handler>, "read")`), and a declared tool is judged as a read --
  the same tier as `read_file`, no longer refused for out-of-area reads. **The declaration is
  optional and off by default**, so this change has zero effect on existing plugins (the
  read-only tools in `plugins/fs` were filled in while we were there).
  An undeclared `fs`-family plugin is still refused for out-of-area reads under a restricted
  tier (the host's own `read_file` is not) — that is the deliberately conservative direction:
  the host's file tools are the sanctioned read/write channel.
  ⚠️ It is **a declaration, not enforcement**: a plugin that declares `read` can still write.
  What this layer defends against is "the model casually writing outside the area", not a plugin
  process with malicious intent (that is §1's ③, which only ②a can handle).
- **`Pending` (restricted but root undecided) is strictly separate from `Full`**: under the
  `default` tier, before the first message, the verdict is **refuse everything**, not allow
  everything. Merging "unrestricted" and "not decided yet" into one `None` would produce "under a
  restricted tier, silently allow everything because the directory doesn't exist yet" — the worst
  possible kind of failure.
- **Going out of bounds is not a silent refusal.** The copy comes in two parts: one for the
  model (`Reject::model_text`), whose three paragraphs are a hard requirement — ① what happened
  and where the root is; ② this is a permission boundary, **don't retry and don't rewrite your way
  around it**; ③ relay it to the user and give the way to get elevated; and a separate line for
  the user (`Reject::user_text`, sent as a `Warn` event). Merging the two would make both worse.
  "Don't retry" is in there because a model's instinct on failure is to try a different
  formulation, and an out-of-bounds condition is not something you can formulate your way past.
- **The host's file tools are `ToolKind::Host`, not `Meta`.** The former follows the
  `[auth] plugin_tools` tier (default `auto`), the latter follows `meta_tools` (default `ask`).
  Folding them into `Meta` would pop an authorization dialog on every read and write, while the
  `fs_*` family of plugin tools in the model's hands is `auto` — two behaviours for the same
  thing, changing with which tool you use, and hardest of all to explain. To tighten one
  individually, override by name: `[auth.overrides] read_file = "ask"`.
- **The tier's test is "how it came about at runtime", not "what the config says."** The state
  records an extra `picked` (whether this root was named by the user or freshly decided by the
  `default` tier), and `snapshot()` reads it. Deriving it from `cfg.mode` goes wrong: a user
  whose config says `default` can still pick one fixed root at runtime (`/workplace <dir>`, or
  the GUI's picker), and then `set_mode` reports "switched to 'choose workplace': X" while
  `snapshot()` immediately after reports "tier: default" — one action, two stories; on the GUI
  that is "the button says default and the tick is on default, while writes actually go to X".
  So the config field was simply deleted from `Workplace`: keeping it only tempts people into
  "deriving the tier from config" again.

**Two checkpoints (defence in depth, not duplication)**:

1. `Agent::authorize_and_execute` — the single funnel for every tool call, **ahead of
   authorization**. Ahead of authorization so that an out-of-bounds call doesn't first pop an
   authorization dialog and then tell the user "actually you can't touch that"; and because this
   is where a `Ui` is available to send the user a `Warn` (which a tool's `call` can't do). The
   host's file tools are judged precisely by their `path` argument; plugin tools are swept for
   strings that "look like a path" by argument name and value shape (`workplace::scan_paths`, the
   coarse net).
2. The top of each host file tool's own `call` (`hostfs::guard` / `hostfs::guard_read`) — so
   nobody can bypass the loop and call `Runtime::execute` directly (which is how `selftest` calls
   it). Both layers use **the same** check (reads go through `check_read`, writes and deletes
   through `check`), so there aren't two sets of rules.
   The read path is **written out separately** (`guard_read` rather than reusing `guard`): that
   way "reads and writes aren't the same tier" is visible at a glance at the call site, and if
   someone ever puts a boundary on reads too, the code diff can't hide it.

**Known limits** (please don't mistake this for a sandbox):

- **"Reads are unrestricted" means this layer does not stop "reading the whole machine."** Under
  a restricted tier the model (or a model tricked by content) can read any file its process has
  permission for — including keys, configs and source elsewhere. This layer blocks **changing
  the device**, not **looking at the device**. To close reads too, you need OS-level isolation
  (§②a's AppContainer).
- **It can't stop a plugin from `open()`-ing a file itself** — a plugin process still gets the
  host's permissions. For §1's ① and ② (the AI writing it wrong / being tricked by content) it is
  **equivalent to AppContainer**, because the dangerous operation never appears where the model
  can write it; but for ③ (a malicious third-party plugin) it does nothing — that needs ②a alone.
- **The plugin-tool layer is a coarse net**: it only judges when an argument name looks like a
  path, or a value's shape is obviously a path (`/x`, `X:\x`, `~/x`, contains a `..` component).
  **Missing some is a deliberate price** — a value containing `/` whose key name doesn't look
  like a path is normally let through, because treating those as paths would also refuse
  perfectly normal calls where a plugin fetches a URL.
  That net answers only "which values are paths"; **"is it for reading or writing" is asked of the
  tool itself** (`Tool::access`; on the plugin side, the declared field) — the net doesn't
  presuppose a direction.
- ✅ **Fixed (2026-10-04)**: the "contains a `..` component" rule **didn't distinguish the nature
  of the value** — if a piece of **source code** contained `../../` (two `..`s fenced by `/`),
  the entire source was treated as one path to be judged. Measured: 2,240 bytes of plugin source
  => `scan_paths` hit **1 entry, length = 2240 bytes (i.e. the whole source)**. The consequences
  came in two grades, both bad: under a **restricted tier** it isn't an absolute path =>
  `Reject::Relative` => **`create_plugin` refused outright** — one `a/../b` in plugin source and
  the model can never build a plugin again (that is a broken feature, not a slow one); under
  **full**, the verdict is discarded but `normalize` still tries `canonicalize` level by level on
  that 2.2 KB "path" (one syscall per level) => every `create_plugin` burns about 1 ms for
  nothing. Fix: the value's shape goes through a gate first — **containing a newline, or longer
  than 260 characters (Windows `MAX_PATH`), means it is not judged as a path**; a real path
  argument never looks like a slab of source. The gate is installed only on the two "guess by
  value shape" paths; **when the key name hits `path` / `file` and the like it is still judged**
  (that is the plugin's own declaration, and a 300-character path has to be honoured).
  Pinned by the regression `multiline_source_is_not_treated_as_a_path`.
- ✅ **The workplace follows the conversation (since 2026-10-04)**: the tier and the root are
  written into that conversation's `.jsonl`
  (`{"kind":"workplace","mode":"default|fixed|full","root":"..."}`, **appended only when it
  changes**, and reading takes the last entry). So:
  - Opening an old conversation hands back its own root — previously nothing was persisted and a
    restored conversation reused "the current root", so the same conversation had a different
    workplace every time you entered it (reported by the user).
  - **`/new` always returns to the "default" tier** (`Agent::new_conversation` ->
    `reset_for_new_conversation`); a tier switched temporarily in the previous conversation is
    not inherited. ⚠️ The price: a user with `mode = "fixed"` in config gets the root named by
    the config only for **the first conversation of this launch**, and every new conversation
    after that falls back to the app root.
  - Old conversation files have no such record (the feature didn't exist yet): on restore
    **nothing is touched**, and it is recorded the next time it lands on disk, after which that
    conversation has its own root too.
  - **The model is told the *current* workspace, not the app root (fixed 2026-10-08)**: the prompt's
    layout segment reads the root live, and `Agent::refresh_system_prompt` rebuilds the system prompt
    whenever the root changes (first turn, `/workplace`, the GUI button; only on change, so it is
    free otherwise). Before the fix it hardcoded the app root, so after the user picked another
    folder the model kept asking about "its own install directory".
- **Nothing under the app root is cleaned up automatically** (like `history/`, `temp/` and the
  `workplace/<project>/` folders only grow; sweeping them is the user's call).

**Measured overhead (2026-10-04, release build, this machine)** — to answer "do these boundaries
slow the model down":

| Item | Per call | Frequency |
|---|---|---|
| `Workplace::check` (full tier / path **exists**) | **65 µs** | per "path-like argument" × per tool call |
| `Workplace::check` (full tier / path **does not exist**) | **224 µs** | same (writing a file is exactly this case: 3 `canonicalize` calls) |
| `Workplace::check` (fixed tier / inside · outside) | 68 · 95 µs | same |
| `scan_paths` (3 arguments · 2.2 KB source) | 0.25 · 0.53 µs | per plugin tool call |
| empty hook chain, `before_tool` ×1 | 0.022 µs | 5 times per turn |
| `Policy::level_for` | 0.003 µs | per tool call |
| `Runtime::tool_schemas` (7 tools) | **15 µs** | **once per turn** (about 60 µs for 27 tools) |
| Job Object fence | **0** | only at spawn / respawn, never touched at runtime |

**Conclusion: these boundaries are unmeasurable against "model execution speed".** The total per
turn is under 1 ms, while one LLM round trip is on the order of **seconds** and one plugin tool
execution is ms~s — under 0.02%. The seam layer (hook chain / policy) is free outright.

Those two "wasted numbers" **were both dealt with the same day**:

1. ✅ `Workplace::is_unrestricted()` plus a short-circuit at the top of `Agent::workplace_reject`:
   under the full tier **the whole check (including the `scan_paths` coarse net) doesn't run a
   single step**. About 95% of `check`'s cost is filesystem syscalls (debug 72 µs vs release
   65 µs, almost unchanged => it is `canonicalize` being paid for, not CPU), and the caller
   discards `normalize`'s result entirely under the full tier (it only wants `.err()`) — so those
   65~224 µs no longer exist. The short-circuit itself is one lock acquisition plus one
   `matches!` (nanosecond scale). It has to agree exactly with "any path is allowed under the
   full tier", so a test pins it: `is_unrestricted_agrees_with_check` (including "`Pending` must
   never report unrestricted" — that would be "a restricted tier silently allowing everything",
   the worst kind of failure).
2. ✅ "A whole slab of source treated as a path" is fixed — see the known-limits entry in §4
   above.

**What actually affects "speed" isn't the check, it's the behaviour**: under a restricted tier,
going out of bounds costs the model **one extra conversation turn** (an LLM round trip plus
possibly a user interaction), four or five orders of magnitude more expensive than the check
itself; and `[sandbox] max_processes = 1` makes "plugins that work via `subprocess`"
**fail outright** — that isn't slower, that's broken (raise it).

**GUI entry point (landed 2026-10-04)**: the "workplace · <tier>" button at the **bottom-left**
of the input box -> a three-way list pops upward (default / choose workplace... / full access) ->
"full access" is red and bold, and clicking it opens a **red confirmation first**
(`egui::Modal`) -> "choose workplace..." opens the **built-in egui directory picker**
(drive chips + parent directory + subdirectory list + "select this directory").

On the interface the three tiers are **the same implementation** as the CLI (both funnel into
`Workplace::set_mode`): the interface doesn't switch tiers itself — `Workplace` lives inside
`Runtime`, and only the agent thread can reach it, so the button merely sends one
`ToAgent::Workplace(Mode)` (see `gui/src/bridge.rs`), and the result is driven by the
`FromAgent::Workplace` acknowledgement (the tick compares tiers with `PartialEq`; it doesn't
parse copy). The acknowledgement is sent **at startup / at the end of every turn / after every
tier switch** — the third because the `default` tier's root isn't created until the first
message (without the resend the button would show "not decided yet" forever). The code is in
`gui/src/workplace_ui.rs`; under `--demo` the button is clickable (simulated in place, interface
only, no disk), which makes the layout easy to preview.

`Reject::model_text` (the out-of-bounds contract given to the model) has had this interface entry
point added to its list of ways to get elevated.

### ②a AppContainer (the only thing that stops a malicious plugin)

The process runs under an AppContainer SID: by default **all files, all network and all registry
access are denied**; to let it work, grant that SID each of `<root>/plugins` (read-only), the
bundled runtime directory (read-only) and the workplace (read-write) with
`SetNamedSecurityInfo`. The network is decided by `SECURITY_CAPABILITIES`; grant nothing and
there is none at all.

Why a launcher is needed: an AppContainer has to be started via `CreateProcessW` +
`STARTUPINFOEX` + `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, and neither `std` nor `tokio`
reaches that layer. **The cheapest approach is to have `aox.exe` do double duty**: add a hidden
subcommand, `aox.exe --sandbox-exec <profile.json> -- python.exe -u main.py`, which
`CreateProcessW`s the real plugin process, sets `bInheritHandles=TRUE` so the stdin/stdout pipes
are **inherited directly with no forwarding code at all**, and then lives on as the Job holder
(when it dies the Job closes and the plugins die with it). The host side barely changes — still
`Command::new`, just a different argv.

**Three pitfalls that must be measured under AC**: `from plugin_sdk import serve` makes Python
write `__pycache__` into `plugins/sdk/` (setting `PYTHONDONTWRITEBYTECODE=1` is cleanest); the
bundled python runtime directory must be granted read-only to it, or the interpreter won't
start; and `%USERPROFILE%` plus `tempfile.gettempdir()` in the `fs` plugin land in the container
directory or fail outright under AC.

### ②b Low integrity level (worth a 30-minute spike first)

The idea is the most elegant — "can read, cannot delete", one sentence. The obstacle is that
swapping tokens goes through `CreateProcessAsUser`, which the docs say requires
`SeAssignPrimaryToken`, something an ordinary user doesn't have. **The only way through is to
pass a "restricted version of its own token" (the product of `CreateRestrictedToken`)** — in that
case the privilege isn't required. Confidence is "very likely to work, but it must be measured":
30 lines of FFI plus one `open(r'C:\t.txt','w')` gives you the answer. If it works, it is an
order of magnitude cheaper than ②a.

> There is also a road nobody has tried: `SetInformationJobObject(JobObjectSecurityLimitInformation)`
> can make **all processes in one Job run under a restricted token**, and the
> `JOB_OBJECT_SECURITY_*` constants are right there in `windows-sys`. If that road works for a
> non-administrator, ②b could grow directly on top of the ① that is already built, without ever
> touching `CreateProcessAsUser`. **Unverified.**

### ④ WASM/WASI (not recommended)

The only sandbox that is **cross-platform and unbypassable**, but wasmtime + Cranelift adds about
**+3–5 MB** to the EXE (`aox.exe` is only 3.14 MB today, so this more than doubles it), and the
three-language subprocess plugin model would have to become wasm modules plus host calls. The
cost and benefit are out of proportion at this stage; leave it for "when we really do need to run
untrusted third-party plugins".

## 5. How to turn it off, how to tune it

```toml
[sandbox]
enabled = false      # fall all the way back to before the fence: byte for byte identical to the old version
max_processes = 8    # how many more child processes a plugin may open (0 = none at all; the allowance is per plugin)
max_memory_mb = 512  # only turn the memory cap on if you mean it: exceeding it kills the process, it isn't a failed allocation
ui_limits = false    # let the plugin touch the host's window/clipboard (normally unnecessary)

[workplace]          # renamed from [workspace] to [workplace] on 2026-10-04; the old name still works
mode = "default"     # factory default: the app root itself is the workspace (writes and deletes only inside)
# mode = "full"      # unrestricted -- it has to be written explicitly (the default stopped being full on 2026-10-05)
# mode = "default"   # scratch -> <app root>/temp/, durable deliverables -> <app root>/workplace/<project>/
# mode = "fixed"
# root = "sandbox-work"   # a fixed workplace (relative paths are relative to the app root)
```

Changes need a restart (both are fixed at startup). Use `/sandbox` to see the fence's current
effective values and `/workplace` to see the boundary's state (reads unrestricted, writes and
deletes restricted); `/plugins` shows each plugin's fence state. You don't have to edit the
config to switch tiers at runtime: `/workplace full` / `/workplace default` / `/workplace
<directory>`; in the GUI, the "workplace" button at the bottom-left of the input box (three tiers
+ a directory picker + a red confirmation for "full access").

## 6. Rollback

Single point of contact: `cmd.process_flags(sandbox.create_flags())` in `plugin::spawn_command`,
and `sandbox.contain(&child)` in `spawn_and_init` / `respawn`. Setting `[sandbox] enabled` to
`false` is equivalent to deleting both (`create_flags` returns 0, and `contain` doesn't even
create a Job — a point the A/B control group forced, see section 3).

Rolling back ③ is simpler still: write `[workplace] mode` back to `"full"` (or delete the whole
`[workplace]` section). In code that is equivalent to "`Workplace::check_access` always returns
`Ok`" — the full tier passes straight through, so the boundary check is one branch on the hot
path and no path is ever refused. You can also skip registering the tools at all (the two
`for t in host_file_tools()` sites in `main.rs` and `gui/src/bridge.rs`), but there's no need:
under the full tier those tools are no different from ordinary read/write tools. Also,
`src/tool/hostfs.rs` is **purely additive**, and so is `src/workplace.rs` — to remove them
completely, delete those two registrations and then those two files (dropping the
`Runtime.workplace` field along with them).
