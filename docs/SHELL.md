# The built-in shell tool (`shell`)

> Answers three questions: **what the model can run**, **what actually stops it from writing outside the
> workspace**, and **why that is not a kernel boundary**.
>
> Shipped 2026-10-20. The short version: the command line exists and works; the write boundary is a
> **guard rail**, and the kernel-enforced version was attempted three ways and measured not to hold on
> Windows 11 26200. The measurements are in §4 so nobody has to take that on faith.

## 1. What it is

One host tool, four dialects, registered like the file tools (`src/tool/shell.rs`):

| Dialect | Command line |
|---------|--------------|
| `cmd` (default) | `cmd.exe /D /S /C "<command>"` — the tail is passed **verbatim** (`raw_arg`), because cmd cannot parse Rust's MSVC-style quoting |
| `powershell` | `powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command <command>` |
| `pwsh` | the same for PowerShell 7 |
| `git` | `git.exe` with the command split into arguments (a convenience, not a shell) |

Why a host tool rather than a plugin: a plugin is a black box that can `open()` anything it likes. Here
the host holds both the arguments and the execution, so "the working directory must be inside the
workspace" is not a convention the model may skip — the same argument as `src/tool/hostfs.rs`.

Each call returns: exit code, elapsed time, the boundary it got, stdout and stderr (capped at
`max_output_bytes`, keeping the first quarter and the last three quarters), and the list of workspace
files whose mtime moved during the call.

## 2. What the boundary is (read this before trusting it)

Two layers, both in the host:

1. **`cwd` must be an absolute path inside the workspace** (`workplace::check_access(.., Write)`).
   Everything relative in the command therefore lands inside.
2. **A command scan** (`scan` in `src/tool/shell.rs`) refuses commands whose *explicit* write targets
   point outside: redirects (`>` / `>>`), `cd`/`pushd` leaving the workspace, the common destructive
   verbs (`del`, `rm`, `copy`, `move`, `robocopy`, …), the PowerShell writer cmdlets, `sed -i`, and
   `git clone <url> <dir>`.

Layer 2 is **a guard rail, not a boundary**. It is bypassable by construction — an encoded command, a
script file, or a child process hides its target from any string scan. It exists to (a) produce a
readable refusal *before* half a command has run, and (b) give non-Windows something at all.

Reads are deliberately **not** restricted, the same asymmetry as `workplace::Access`: reading takes
information, writing changes the device. `type C:\logs\x.log` works; `> C:\logs\x.log` does not.

## 3. What each call reports

The header of every result states the layer it actually got:

```
exit 0 · 0.26 s · cmd · boundary: guard-rail-only · cwd E:\rustapp\workplace\demo
```

`guard-rail-only` is the only value under a confined tier; `unrestricted` appears under
`[workplace] mode = "full"`. There is no `kernel` value because nothing produces one (§4).

## 3a. When a call takes too long

Two independent conditions end a call, and they answer two different questions:

| Condition | Answers | Default |
|-----------|---------|---------|
| `timeout_secs` | "this must not run forever" | 60 s (ceiling `max_timeout_secs`, 600) |
| `idle_timeout_secs` | "is it doing anything at all" | 30 s without a single byte on either stream |

The idle one is the answer to the report that started it: a `bash` call sat at **471 s** with no
visible reason, and from the outside "building for eight minutes" and "wedged" looked identical. The
**stream** is what separates them — a build prints, a deadlock does not — and the clock runs from the
*last byte*, never from the start, so a slow-but-chatty command is never killed for being slow.
A call that is legitimately silent (`sleep`, a download with no progress output) raises it with its
own `idle_secs`, or turns it off with `idle_secs = 0`.

Both kills are bounded: `TerminateJobObject` gets 5 s, then a direct `kill` gets 2 s, and if the
process still has not been reaped **the call returns anyway** with `TIMED OUT` / `KILLED` in the
header. An earlier version called `child.wait()` right after `terminate()`, which is a no-op when
there is no Job — the one code path that could outlive every timeout in the program.

What the model can announce: `expect_secs`. It is **not** a limit and nothing enforces it — the user
just sees `expected ~2:00` next to `0:47 / 2:00` in the running row (and the same in the CLI's
"running" line), which is how "still working" stops looking like "stuck".

## 4. Why there is no kernel boundary (the measurements)

The intended design: run the command at **Low integrity** and label the workspace Low, so
`no-write-up` makes any out-of-workspace write a kernel refusal. Implemented three ways, all measured on
Windows 11 26200, all rejected:

| Attempt | Result |
|---------|--------|
| Lower the helper's own token, then `CreateProcess` | the child reports `IL=0x1000 (Low)`, attribute bits `0x60` (**correct**), and still gets `拒绝访问` writing a **Low-labelled** directory — including `cmd`, `tar`, .NET and a Rust grandchild |
| Set the level on the **suspended** child's token, then resume | identical refusal |
| `CreateProcessAsUser` with a duplicated token / a `CreateRestrictedToken` token | `1346` / `5` — not available to an unelevated caller |

So: the process that lowers *itself* can write a Low directory; **any process it spawns cannot**, even
though its token says Low and its SID attributes are right. Lowering the label target to *Untrusted*
did not help either, and labelling the app root turned out to be actively harmful: Windows starts a
process at `min(token, image label)`, so `aox.exe` became Untrusted and could no longer lower its own
token (`SetTokenInformation` → `1314`). Both of those are why `prepare`/`unprepare` were removed rather
than kept behind a switch.

A `boundary = "kernel"` config key would read as protection while delivering nothing, which is the one
failure mode this project does not accept. Hence no key.

**What would change the answer**: a different execution model rather than a different token —
AppContainer (a real container, but it blocks *reads* too, which conflicts with the read/write
asymmetry above), or a file-system minifilter. Both are orders of magnitude more work than a token
tweak, and both would change the feature's contract.

## 5. Configuration

```toml
[shell]
enabled          = true      # false = the tools are never registered
prefer           = "bash"    # which tool the model is told to reach for first: bash | cmd
timeout_secs     = 60        # per call; the call may ask for more
max_timeout_secs = 600       # hard cap
idle_timeout_secs = 30       # no output for this long ⇒ treated as wedged and killed (0 = off)
max_output_bytes = 131072    # per stream, head + tail
max_processes    = 16        # per call (one Job Object per call; 0 = no children at all)
env_cache        = true      # CARGO_HOME / NPM_CONFIG_CACHE / PIP_CACHE_DIR / TMP inside the workspace
track_changes    = true      # report workspace files whose mtime moved
```

`prefer` is **wording, not a fallback** — and that is not a shortcut, it is the only honest option. A
command has side effects, so the host can never "try bash and, if it fails, try cmd": `del`,
`git commit` and `cargo build` would run twice. What the key changes is the instruction the model
reads (one order line plus the bullet order in the prompt's command-line section), plus the `/shell`
report and the "Command line" row in Settings → System. `cmd` needs no tool name of its own: it is
already `shell`'s default dialect, so both settings expose the same two tools.

`[auth] shell = "ask" | "auto" | "deny"` — its own key, default `ask`: a command line is a different
kind of capability from "a tool that does one designed job", and one "allow for this session" is
enough after the first prompt.

## 6. Deliberately not done

- **No output streaming**: a call returns when the command ends (bounded by the timeout), so
  `npm run dev` hangs until it is killed. Streaming needs an event channel through the `Ui` trait.
- **No background processes**: no `shell_bg`, no `/ps`. A long-lived server is out of scope until
  streaming exists.
- **No network boundary**: a command can download and upload. Nothing here restricts that.
- **No stdin**: the child's stdin is null; use `echo y | …` inside the command if something prompts.

## 7. How to check all of this

```
cargo run --example shell_probe
```

Seven checks against a throwaway workspace in `%TEMP%`: a write inside succeeds, an out-of-bounds
redirect is refused with the contract text and the target is untouched, a read outside works, a
non-zero exit code is reported as output rather than as a tool failure, and the header states the
boundary. An eighth check runs a real bash script when this machine has one. `AOX_PROBE_ROOT=<dir>`
puts the probe workspace somewhere else (useful for testing a real project path).

## 8. The `bash` tool, and which tool comes first

A second tool, same engine. For an agent, POSIX syntax is the difference between one call and five, and
a bash is cheap next to PowerShell: measured here, `powershell -NoProfile -Command exit` costs
~1100 ms, a bash ~270 ms, `cmd /D /S /C exit` ~87 ms, busybox `sh` ~28 ms.

So `cmd` is faster than bash and bash is the denser language — which is why the order is a **decision
the user makes**, not something the host decides for them: `[shell] prefer = "bash" | "cmd"` (default
`bash`) sets the wording the model reads. The default stays `bash` because a wrong guess costs a whole
model round trip, far more than the ~180 ms cmd saves; the escape hatch exists for machines where
Windows-native paths and cmd builtins dominate the work. Even with `prefer = "cmd"`, bash is still
listed second with its own bullet — the model is being told what to reach for first, not what is
allowed.

`shell` keeps the dialects bash cannot be: cmd builtins, PowerShell objects, and `git` without a shell
in between.

**Why a separate name rather than `shell: "bash"`**: the model picks its syntax from the tool's name.
Behind `shell` it reaches for `cmd`; a tool called `bash` gets POSIX. One extra schema is a cheap price
for that.

**Where it comes from** (`src/shell/bash.rs`), file existence only — nothing is executed during
detection, because that would put a third of a second on startup:

1. `[shell] bash = "<absolute path>"` (explicit wins, and is the escape hatch);
2. **the machine's own bash**: `%ProgramFiles%\Git\bin\bash.exe`, `%ProgramFiles(x86)%\…`,
   `%LOCALAPPDATA%\Programs\Git\…`, a path derived from `git` on `PATH`, then a `bash.exe` on `PATH` —
   **never** one under `\Microsoft\WindowsApps\` (that is the WSL launcher, a different file system);
3. the sidecar (`[shell] bash_pack = "runtime"`): `<app root>\runtime\bash\bash.exe` if someone put a
   real bash there, otherwise `<app root>\runtime\bash\busybox.exe`.

Order is deliberate: a full bash 5.x beats anything this project ships, and costs no bytes to
distribute. `runtime/bash/README.md` records the busybox build, its GPLv2 obligations, and its measured
quirks.

**The busybox fallback** is 637 KB, one file, no DLLs — and an **ash**, not a bash: no arrays, no
`[[ ]]`, no process substitution, and a subset of the applet options (`grep -P` and `find -printf` are
missing). Every call's header reports `flavour: busybox`, so the model can see what it got. Two quirks
are load-bearing:

- `uname -s` answers **`Windows_NT`** (it is a native Win32 program, not an MSYS runtime) — the host
  accepts that value deliberately;
- its **arguments** must be Windows paths (`E:/dir` works, `/e/dir` does not), while its **PATH** is
  POSIX (`:`-separated, `/c/…` entries). The host therefore writes POSIX form into `PATH` for both
  flavours, and the prologue exports the forward-slash Windows form (`E:/…`) as `AOX_WORKSPACE_U` —
  the one form both shells accept as an argument.

The first real call validates the chosen shell with `uname -s` and refuses anything that is not
MINGW/MSYS/CYGWIN/Windows_NT — a path filter alone would not survive a shim. No shell at all ⇒ **the
tool is not registered**: an entry that can only fail costs the model a round trip.

**The PATH trap**, measured: handing MSYS a Windows-style `PATH` makes it treat each entry as a
*relative* POSIX path and glue it under its own root (`C:/…/PortableGit/Windows/System32`), after which
`cargo` is simply missing. So the host converts every entry to `/c/…` form and prepends the bash's own
`usr\bin` and `mingw64\bin`. Getting this wrong is the difference between "bash works" and "bash cannot
see anything you installed".

**The prologue**: two exports, and nothing else.

```
AOX_WORKSPACE='E:\rustapp\workplace\demo'; export AOX_WORKSPACE
AOX_WORKSPACE_U='/e/rustapp/workplace/demo'; export AOX_WORKSPACE_U
```

Deliberately **no** `set -e` / `pipefail`: those change what the model's script means, and a host that
quietly changes semantics is worse than one that exposes two names. `--noprofile --norc` keeps a user's
`.bashrc` from printing banners or moving the shell elsewhere.

Same boundary as `shell` (guard rail + the `cwd` check), same Job Object, same timeout and output
handling — one execution path, so there is only one boundary to keep correct.
