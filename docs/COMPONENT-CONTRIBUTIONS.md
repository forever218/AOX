# Component contributions (stage 6b) — plugins are no longer just "add a tool"

> This document is written in two layers: **the top half is the experience** (what you can do
> with it right now, how to start), **the bottom half is the implementation** (the protocol, the
> shape of the seam, why it is built this way).
> For the roadmap, the decisions and the acceptance records, see "6b implementation log" in
> `docs/ARCH-ROADMAP.md`.

---

## 0. In one sentence

**Before 6b**, a plugin could only **add tools** to the host. The host's own behaviour skeleton
-- what text goes into the request, whether tool arguments are touched, whether tool results are
processed, whether a failed request is retried, whether a given plugin tool should ask first --
was all hard-coded in the host, and **only editing the host's source and shipping a new release**
could change it.

**After 6b**, every one of those positions became a contribution point a plugin can **declare
and take over**. And it travels over the subprocess JSON-RPC channel the plugin **already had**:
the host's facade is unchanged, no dependency was added, and **not one byte of an existing
plugin needs to change**.

| | before 6b | after 6b |
|---|---|---|
| what a plugin may change | "provide new tools" only | tools **+ request content + tool arguments + tool results + error handling + the permission bar** |
| what has to change in the host | — | nothing at all; add one `[contributes]` section to the plugin directory |
| host performance cost | — | one local RPC per hook point (on the order of 5 points per turn) |
| new dependencies | — | **zero** (no new crate, no scripting engine) |

Its significance in one line: **it moves the system's control plane out of the host's source
code and into the plugin directory**. Plugins used to be peripherals; now they can adjust the
engine's ignition timing -- though **the key stays with the host** (see §4).

---

# Part one · the experience

## 1.1 A comparison table: what you want to change, and how you do it now

| What you want | before 6b | now |
|---|---|---|
| inject project rules into **every** request (coding conventions, tone of voice) | edit the host's system-prompt constant and rebuild | a component contributes one `prompt_segments` |
| stamp / redact / truncate tool results uniformly | impossible | the `after_tool` hook |
| auto-fill or correct an argument before a tool call | only `[hook] tool_args_default` (fills gaps only, static values only) | the `before_tool` hook, which **can compute from the content** |
| auto-retry a failed request | nothing (a failed request is thrown at the user) | the `on_error` hook, or the built-in `[retry]` policy — which since 2026-10-23 retries `429` / `5xx` / cut streams on its own, with backoff, and never retries a `401` |
| require a question before a plugin tool runs | allow or deny a whole class | a `caps` capability request => **that plugin's tools always ask** |
| let a plugin colour the interface | impossible | a component contributes a theme patch (v1 only reaches "fetch + validate + report", see §3) |
| log/audit every model reply | impossible | the `after_reply` hook |

## 1.2 Six ready-made scenarios (copy them as they are)

**① Inject project rules** — the cheapest one, with the most direct payoff.

```python
# main.py
def before_request(params):
    return {"inject": ["This project forbids opportunistic refactoring of unrelated code."]}
```

```toml
# plugin.toml
[contributes]
hooks = ["before_request"]
```

What gets injected is a **user message** — it really enters the conversation and really lands on
disk; it is not a silent rewrite of the request body.

**② Stamp / redact tool results** — proof that the component really is wired into the execution
path.

```python
def after_tool(params):
    if params["is_error"]:
        return {}                                    # leave failed results alone, so diagnostics aren't drowned
    return {"content": params["content"] + "\n(reviewed by component)"}
```

**③ Correct tool arguments based on content** — `[hook] tool_args_default` can only hold static
values; this one can compute.

```python
def before_tool(params):
    if params["tool"] == "read_file":
        args = params["args"]
        args["path"] = args["path"].replace("\\", "/")   # normalize to POSIX separators
        return {"args": args}
```

**④ Auto-retry a failed request** — two paths, chosen by "who provides the fallback":

```toml
# config.toml: the host-level fallback budget; not writing it = 0 = byte for byte identical to before 6b
[hook]
max_llm_retries = 2
```

```python
# or let the component decide by error type (finer, e.g. retry only on 429)
def on_error(params):
    if "429" in params["error"]:
        return {"action": "retry", "note": "upstream is rate-limiting; try again shortly"}
    return {}
```

**⑤ Make a plugin tool ask first** — once a plugin declares a capability, **all** of its tool
calls go through "ask"; it doesn't ride on `[auth] plugin_tools`.

```toml
[contributes]
caps = ["fs_write"]     # non-empty => this plugin's tools are always Ask
```

**⑥ Theme patches (limited support in v1)**

```toml
[contributes]
theme = true            # the host fetches it with theme/patch, validates it against [theme]'s shape, then reports
```

## 1.3 Three commands to see the state

| Command | What it shows |
|---|---|
| `/plugins` | an extra line, "**component contributions:**" — hook points, contributed segments, requested capabilities, theme-patch status |
| `/hook` | what the hook chain holds right now, with **config sources and component sources listed separately**; component failure counts live here too |
| `/prompt` | segments contributed by components, sourced as "**plugin**" (segment ids look like `house_rules:rules`, ordered after the config segments) |

## 1.4 Writing a component: only two places change

```toml
# plugin.toml — old plugins change nothing; new ones just gain this section
name = "house_rules"
entry = "main.py"

[contributes]
hooks = ["before_request", "after_tool"]
prompt_segments = ["rules"]
caps = ["fs_write"]
theme = false
```

```python
# main.py
from plugin_sdk import serve

def before_request(params):
    return {"inject": ["..."]}

def after_tool(params):
    return {"content": params["content"] + "\n(reviewed)"}

serve(
    "house_rules",
    {"house_rules_check": ("a trivial tool (to try the capability request)", {"type": "object", "properties": {}}, lambda a: "ok")},
    hooks={"before_request": before_request, "after_tool": after_tool},
    prompt_segments={"rules": "## A few rules for this project\n\n1. ..."},
)
```

**Leaving `[contributes]` out entirely = an old plugin**, whose behaviour is **byte for byte
identical** to before 6b.
A complete runnable example: `tools/samples/contributing_component/` (not loaded by default; copy
it into `plugins/` by hand when accepting).

---

# Part two · the technical layer

## 2.1 Protocol extension: three new methods plus one optional field

The plugin protocol was already JSON-RPC, "one JSON object per line on stdout/stdin", and this
is a **structurally homogeneous extension**:

```
initialize's response gains one optional field:
    contributes: { hooks: [...], prompt_segments: [...], theme: bool, caps: [...] }

the host then pulls on demand:
    hooks/<point>    → {"verdict":"pass"|"reject", "reason":..., plus that point's payload}
    prompt/segment   → {"text": "..."}        (once per declared segment name)
    theme/patch      → {"theme": {...}}       (once, when theme = true)
```

- An old plugin not recognizing these methods is fine — the host **only calls the points that
  were declared**.
- Segment bodies and theme patches **don't go into the manifest**: the manifest is an offline
  declaration and bodies are fetched over RPC, so large blocks of text never land in
  `plugin.toml`.

## 2.2 The exact contract of the five hook points

| Point | What the host sends | What the component may return | What it can change |
|---|---|---|---|
| `before_request` | `{tools: [...]}` | `inject: [text, ...]`, `verdict` | **appends user messages** to the request (append only) |
| `after_reply` | `{text, tool_calls: count}` | `text`, `verdict` | the **body** of the model's reply (tool calls are not sent in the clear and can't be changed) |
| `before_tool` | `{tool, args}` | `args`, `verdict` | the **arguments** of this call |
| `after_tool` | `{tool, content, is_error}` | `content`, `is_error`, `verdict` | the tool result's **body and success flag** |
| `on_error` | `{error}` | `action: "retry"`, `note` | the **handling** of a failed request (retry / keep propagating) |

Three "deliberately not sent" items are part of the design, not oversights:

- `after_reply` does **not** send tool calls in the clear — sending them would tempt people into
  thinking they can be changed, and changing them amounts to **forging the model's intent**.
- `before_request` does **not** send a replaceable tool table — that would hand over "what the
  model can see in this turn" wholesale.
- No point ever receives `&mut Agent` — so history can't be rewritten (see §3).

`verdict: "reject"` must carry a `reason`, and the reason goes back to the model verbatim;
**unrecognized or absent is always treated as allow**.

## 2.3 Four rules for declarations

1. **Every field is optional** (`#[serde(default)]`) — old manifests parse as before.
2. **`normalize()` validates** hook point names one by one, and on an unknown name **reports an
   error and lists the available points** (never drops silently), then dedups and orders them.
   The single source of truth is the one constant table, `hook::POINTS`.
3. **What `initialize` carries overrides what `plugin.toml` says** (the live process is more
   accurate than an offline manifest).
4. **An empty `[contributes]` is not written back out on `save`** — old manifests are never
   dirtied by the host.

Also, a component that "only contributes hooks and provides no tools" is legal (6b relaxed this
on purpose); the test is "refuse only when tools and contributions are **both** empty".

## 2.4 `HookChain` becomes appendable, and a `Send` trap

**Problem**: the chain was fixed inside `Runtime::new`, while plugins are only `activate()`d
**after** that.

**The change**:

```rust
pub struct HookChain {
    hooks: std::sync::RwLock<Vec<Arc<dyn Hook>>>,   // used to be Vec
}
// new: push() (the registration entry point) and snapshot() (clone a batch of Arcs under the read lock, then release it)
```

⚠️ **`snapshot()` is mandatory**: `std::sync`'s read guard is **not `Send`**, and holding it
across an `await` makes the whole future lose `Send`. So every hook point is "take a snapshot
first, then iterate", never "iterate while holding the lock, then await".

**Why zero drift is unaffected**: an empty chain **still iterates 0 times**. Stage 4's proof
that "no hooks = no behaviour change" is structural, so this change needed no edit at all -- and
6a's e2e re-run confirms it (stdout byte for byte identical in both A/B cases).

## 2.5 Four design decisions in `ComponentHook`

| Decision | What it buys |
|---|---|
| **doesn't hold a `PluginHandle`**, just records the plugin name and looks it up at call time | if the plugin is unloaded or fails to load, the hook **degrades to allow automatically**, with **no unregistration logic at all** (one fewer cleanup path, and that is the easiest kind to forget) |
| failure counts use an **`Arc<AtomicU64>` shared with the handle** | `/hook` and `/plugins` read from **the same ledger** |
| **only asks the points that were declared** | points a plugin didn't declare cost zero RPCs -> old plugins pay nothing |
| **one 10-second timeout per call**, any failure degrades (Pass / Propagate) | a stuck hook blocks **the entire main loop** (the user watches a spinner), whereas a stuck tool blocks only that one call -- so the hook's ceiling must be far below the tool timeout (120 s by default) |

**Degrading is not the same as going silent**: every degradation is recorded (count + the most
recent reason) and visible in `/hook` and `/plugins`. "Failing silently" is the state this design
most wants to avoid — the same principle as stage 3/5's "if you wrote it and it doesn't take
effect, that's an error".

## 2.6 Capability requests enter authorization (`caps` -> `Ask`)

The `caps` argument in `Policy::level_for(kind, name, caps)` — standing since stage 3 and always
passed as `&[]` — finally has someone who fills it in:

```
ToolKind::Plugin(name) → Runtime.caps_for(kind) → that plugin's [contributes] caps
```

Both the rule and its **position** matter:

```
1. an explicit user override (including prefix wildcards like "fs_*")   ← highest priority, still beats caps
2. [new] a plugin tool with non-empty caps ⇒ Ask                          ← pinned at this layer
3. the category default
```

It sits at layer 2 and not layer 1 so that **an explicit user configuration always wins**; it
sits before the category default so that `plugin_tools = "allow"` can't **hollow out** a
capability request. Also: **meta tools are never affected** (they aren't plugin tools). Each of
those three is pinned by its own unit test, and none of the 13 existing authorization tests were
touched.

## 2.7 The built-in loop's retry budget

`on_error`'s `ErrorAction::Retry` has existed since stage 4, but **never had a producer** (it was
still marked `#[allow(dead_code)]`). 6b gave it two real sources:

1. a component returning `{"action":"retry"}` from `hooks/on_error`;
2. the config `[hook] max_llm_retries = N` — making the **built-in loop** retry N times when it
   would otherwise "keep propagating".

The default is **0**, byte for byte identical to stage 4's behaviour. Retries are counted
**independently per turn**, and a human-readable note is injected before each retry.

## 2.8 Zero drift and size

**Three layers of evidence for zero drift**

- Structural: with no component installed the host registers no `ComponentHook`, so the empty
  chain still iterates 0 times.
- Unit tests: `empty_chain_never_intervenes` (stage 4) plus 14 new ones (including "a point that
  wasn't declared is never asked").
- End to end: an isolated root plus a mock endpoint, running a whole turn with no component
  installed is **byte for byte identical** to the 6a baseline; old plugins that only provide
  tools still parse and still run.

**Size (the P4 discipline: ≤ 300 KB per stage)**

| | 6a shipped | 6b measured | delta |
|---|---|---|---|
| `aox.exe` | 3,038,720 B | **3,122,688 B** | +83,968 B (+2.8%) |
| `aox-gui.exe` | 5,947,392 B | **6,024,704 B** | +77,312 B |

- New dependencies: **0** (`Cargo.toml` / `Cargo.lock` untouched throughout).
- A `touch`-forced relink still measures 3,122,688 B — it isn't linker jitter.
- **Attribution confirmed**: `aox-gui.exe` had **zero source changes** this round yet also grew
  +77 KB => the growth really is in the **`src` code shared by both binaries**, which also
  retroactively confirms that 6a's baseline was measured right.
- ⚠️ **"the subprocess route is +0 KB" should be read as "zero dependency growth"** (no crate
  pulled in, no engine needed), **not as a zero-byte delta** — a few hundred lines of new code is
  tens to hundreds of KB in a size-optimized binary.
  As a density reference: `src` as a whole is 11,045 lines -> 3.12 MB, about 283 B/line.

## 2.9 Change map

| File | What changed |
|---|---|
| `src/hook.rs` | `HookChain` switched to `RwLock` + `push()`/`snapshot()`; added `POINTS`, `ComponentHook`, `verdict_of` |
| `src/plugin/mod.rs` | `[contributes]` -> `Contributes`; `activate()` takes the contributions and registers them; `PROTOCOL_DOC` §4; `PYTHON_SDK` in step |
| `src/runtime.rs` | `add_hook()` (the component registration entry point), `caps_for(kind)` |
| `src/auth.rs` | `level_for` now actually fills `caps` (rule and position in §2.6) |
| `src/agent.rs` | at the authorization site, `&[]` became the real caps |
| `src/agent_loop.rs` | `on_error`'s Propagate branch wired to `[hook] max_llm_retries` |
| `src/config.rs` | `HookConfig.max_llm_retries` |
| `src/prompt.rs` | `Source::Plugin` drops `#[allow(dead_code)]` |
| `src/main.rs` | `/plugins` prints an extra "component contributions:"; `/help` and `/hook` copy |
| `config.example.toml` | the `[hook]` explanation block (commented out wholesale) |
| `gui/` | **zero changes** |

---

# Part three · the boundary: what still can't be done

Drawn honestly, so nobody expects the wrong thing:

1. **Conversation history can't be changed.** This hook layer opens up "append" only. For
   "prune the context / erase some messages", use `config.toml`'s `[loop] max_rounds` — it is
   the **only** path that changes both memory and the conversation file (because it lives at the
   loop layer and can call `Session::rewrite`).
2. **The loop implementation can't be swapped.** `[loop] impl` currently recognizes only
   `"standard"`, and anything else **errors out at startup**.
3. **Theme patches aren't painted onto the interface yet.** v1 is "declare -> fetch via
   `theme/patch` -> validate against `[theme]` -> report"; actually refreshing the GUI's live
   palette belongs to the UI customization layer (formerly 6c, see
   `docs/UI-CUSTOMIZATION.md`). The root cause is timing: the GUI fixes its theme before the
   window is created, and plugins load after that.
4. **There is no in-process scripting engine.** Something like "a pet that draws every frame"
   must be in-process, which is 6c's territory; this stage deliberately introduces no engine, to
   keep the few-MB physique.
5. **Hooks shouldn't do anything slow.** They block the main loop, and past 10 seconds they are
   degraded as "the component is broken" — that is **intentional**.

---

# Part four · the security model

| Gate | Rule |
|---|---|
| Distribution | the sample component sits **deliberately** in `tools/samples/`, not in `plugins/` — uninstalled means inert, and zero drift isn't broken |
| Permissions | a capability request is **always `Ask`** (P7); but an explicit user `[auth]` override still wins |
| Degradation | component missing / timeout / error / unreadable return value => **always allow**, recorded only, never thrown back into the main loop |
| No intent forging | a component can't touch the record of "what the model intends to do" (tool calls aren't sent in the clear and can't be changed) |
| No execution takeover | a hook never gets `&mut Agent`, so it can't rewrite history or swap the tool table |

**Bottom line**: a component can influence **how this turn goes**, but it cannot change **what
has already happened**.

---

# Part five · how to roll back

Component contributions **live entirely inside the plugin directory**:

```
delete plugins/<component>/  → restart  → one fewer hook on the chain, one fewer prompt segment; the host is back to its pre-6b shape
```

- No host change, no recompilation, no touching `config.toml`.
- As long as `[contributes]` is empty (or absent entirely), host behaviour is **byte for byte
  identical** to before 6b.
- Both new things on the config side can also be deleted wholesale: not writing
  `[hook] max_llm_retries` means 0.

---

# Part six · self-check evidence (2026-10-03)

| Item | Result |
|---|---|
| `cargo check --workspace --all-targets` | zero warnings |
| `cargo test --workspace` | **101 + 101 + 12, all green** (+14 this round: hook 4 / plugin 6 / config 1 / auth 3) |
| `target/_tools/component_e2e.py` | **7/7** (empty chain zero drift / old plugins unaffected / component effective / degradation / caps→ask / retry with config / retry without config) |
| `target/_tools/loop_e2e.py` (6a regression) | all pass; stdout **byte for byte identical** in both A/B cases |
| full release build | all four installers plus the runtime component package produced (see "release artifacts" in `README.md`) |
| encoding | changed files: U+FFFD 0 / tabs 0 / CRLF 0 |
