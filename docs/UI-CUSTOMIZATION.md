# AOX UI customization layer (the UI seam) · project charter

- **Status**: **chartered, deferred** (moved out of stage 6 by user decision on 2026-10-03 and
  chartered separately; not part of stage 6)
- **Origin**: the user asked "can the whole interface's elements and layout be brought into the
  range of 'the user asks the AI to change it'", giving examples like "add a pet / change the
  hi-there text / remove the input presets / restyle the input box", by analogy with "the
  interface is HTML and the user injects JS/CSS" (**an analogy only, not real injection**).
- **Predecessor**: the section "additional topic: UI customization layer (UI seam) and
  behaviour sandbox" in stage 6 of `docs/ARCH-ROADMAP.md` -- the original text stays in the
  ROADMAP and is not deleted; this file is the chartered version (with the to-dos and
  decisions filled in).

---

## 1. The goal: four tiers of capability, ascending in risk

| Tier | Capability | The user's example | Does it really need AI? |
|---|---|---|---|
| ① | **text overrides** | change the hi-there text | no, plain config is enough |
| ② | **style / property overrides** | change the input box radius, colours, font size | no, plain config is enough |
| ③ | **structural show/hide** | remove the input presets | *this* is where "let the AI write things" bites |
| ④ | **injecting new components into slots** | add a pet | *this* is where "let the AI write things" bites |

> ① and ② don't need AI at all (the config layer suffices); **③ and ④ are the real value in
> this topic** -- which is what "the AI as an **offline customization author**" below means.
> Put another way: if only ① and ② were built, this charter could shrink to "write a few more
> `[ui]` config items" and would need no scripting or engine at all.

## 2. Why this isn't "turning the existing code into configuration"

- egui is **immediate mode**: no DOM, no addressable node tree, so nothing like a CSS selector
  can "select an element".
- The GUI presentation layer is **7,712 lines of hand-written calls** (`app.rs` 2698 /
  `activity.rs` 2206 / `theme.rs` 803 / `markdown.rs` 712 / `syntax.rs` 660 / `bridge.rs` 540),
  with about **350 `ui.*` call sites**, and **zero element registry** (the 6 existing
  `Id::new` / `push_id` uses serve egui's internal state only and are not addressable nodes).
- → so this is not "turning the existing code into configuration" but **adding a retained-mode
  customization layer on top**: **semantic element ids + property overrides + named slots +
  sandboxed patches**.

## 3. Where the AI sits: offline customization author (not a runtime UI generator)

Flow: **read the element manifest** (id table + available properties + current values) →
**produce a patch or a plugin** → **land it after human review** as a nameable, shareable,
rollbackable **customization pack**.

- **Not** generating UI every frame at runtime -- that loses on latency, cost, reproducibility
  and auditability all at once.
- The skeleton lines up directly with stage 5's `profile → bundle → patch` (just one more
  `[ui]` branch).
- **The natural shape of "add a pet"**: give plugins a "**UI contribution**" capability
  (a plugin declares which **named slot** it draws into), rather than letting the AI construct
  interface directly. A pet = one preinstalled plugin + one slot declaration.

## 4. Three hard constraints (written into the acceptance criteria)

1. **Element ids are a long-term contract**: they must be **semantic ids** (e.g.
   `input.preset[remove]`) rather than positional, and must stay compatible over the long run --
   **this is the largest ongoing cost of the design**, and the essential difference between it
   and "just tweak it as you go".
2. **No arbitrary layout reflow**: only "inside a slot" and "property overrides" are opened up;
   no general layout language is abstracted (cost and benefit are out of proportion).
3. **Must degrade gracefully**: a **parse or evaluation failure of any patch / script falls back
   to the default interface** (no blank screen, no crash); the property allow-list, the size
   caps and the capability requests all go into `AuthPolicy` together.

## 5. Three things that must be settled before work starts

| # | Decision | Notes |
|---|---|---|
| 1 | **engine choice** (only if in-process) | see the table below. In-process is **mandatory** only if "draw every frame" is really needed |
| 2 | **how the pet is implemented** | ① in-process script (draw every frame) vs ② **subprocess + declarative animation spec** (the component emits an animation description and the host renders it) -- ② trades expressiveness for size and can keep the **+0 KB** route |
| 3 | **how to account for the size** | a separate 1.2 MB tier, or a **separate package** following the `#ifdef` pattern in `installer/*.iss` (there is already the `BUNDLE_PYTHON` precedent: 3.2 MB → 11.9 MB) |

Engine candidates (size deltas are **pre-measurement** estimates, from the stage-6 selection
table in the ROADMAP):

| Option | Size delta | Notes |
|---|---|---|
| `mlua` (vendored Lua) | +0.3~0.6 MB | enough for scripted prompts / themes / hooks, sandbox is controllable; **but it pulls in a C build dependency**, which has to be re-aligned with the "static CRT / single exe" toolchain |
| `wasmi` (pure-Rust interpreter) | +0.5~1.2 MB | **the only true sandbox** (wasm linear-memory isolation); but writing scripts needs a wasm toolchain first, which fights the "the AI dashes off a script" positioning |
| `rhai` (pure-Rust scripting) | +0.8~1.5 MB | no C dependency, syntax close to Rust, easiest for an AI to write; **the preferred option** |
| Wasmer | +10~20 MB | over budget |
| Wasmtime (JIT) | +15~30 MB | **forbidden** -- it would wreck the "a few MB" physique |

If a JIT really is needed later: `cargo build --features wasm-jit` **as a separate package**,
with the main package untouched.

## 6. Relationship to stage 6 (6a / 6b): 6b burns no bridges

- 6b's `ComponentHook` implements the **already existing** `trait Hook`; how it obtains a
  `Verdict` internally (subprocess RPC or in-process evaluation) is an **implementation
  detail**.
- → **6b choosing a subprocess burns no bridges**: if in-process really is needed later, that is
  **one more provider** (a `HookChain` can hold both kinds of source at once), not a rewrite.
- This project depends on the output of 6a / 6b (`AgentLoop` / the component registry / the
  capability-request channel), but can start **long after** them without blocking either way.

## 7. To-do list

- [ ] settle "build this release / keep deferring"
- [ ] settle the engine (if in-process) or confirm "the pet goes the declarative animation spec
      route" (if continuing at +0 KB)
- [ ] inventory the GUI elements and fix the **semantic id table** (mainly `app` / `activity` /
      `markdown`)
- [ ] fix the `[ui]` config shape (aligned with stage 5's `profile → bundle → patch`)
- [ ] property allow-list + size caps + degradation path (failures fall back to the default
      interface)
- [ ] a minimal demo: change one piece of text + hide one button + put a pet in a slot

## 8. Risk

**High**. This is the only topic that touches **size** and **attack surface** at the same time,
and it introduces a **long-term-maintained** contract (semantic element ids). Recommend starting
it only after 6a / 6b have landed and run stably for a while.
