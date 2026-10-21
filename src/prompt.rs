//! The system-prompt "seam".
//!
//! The prompt is no longer a compile-time constant but an ordered concatenation of
//! several **segments**:
//!
//! | id | source | content |
//! |----|------|------|
//! | `core` | built-in | identity and core facts |
//! | `protocol` | built-in | plugin protocol, Python template, plugin rules, communication style, reply rules, attachments & images |
//! | `env` | generated at runtime | the host's real paths and this machine's interpreter situation |
//! | `rules` | project file | the RULES.md project rules |
//!
//! Users can reorder (`order`), disable (`disable`) or append
//! (`[[prompt.segment]]`) any segment in `config.toml`'s `[prompt]`; a config segment
//! with the same id **overrides** the built-in one — the same semantics as phase 5's
//! "later layer overrides earlier layer", only with two layers for now.
//!
//! # The byte-for-byte contract
//!
//! A segment body carries **no separators**; [`build`] connects them uniformly with
//! `"\n\n"`. This makes the result under the default config **exactly identical** to
//! the historical hard concatenation of "constant + `env_section` + project rules".
//!
//! This is not pedantry: the server's **prompt cache hit rate depends on byte-stable
//! prefixes**, and once the prefix changes the cache is entirely wasted, directly
//! raising cost. The `split_is_lossless` and `default_build_equals_legacy_assembly`
//! tests at the end of this file pin this down.
//!
//! # Why the built-in constant is "one whole block + runtime splitting"
//!
//! Splitting that text into two halves by hand and writing them into the source
//! amounts to importing "mistype one full-width bracket" into the core prompt. So we
//! keep the **verbatim whole block** [`BUILTIN_PROMPT`], split it by section heading at
//! runtime, and use a test to prove "split it, put it back == the original string".

use std::path::PathBuf;

use crate::config::PromptConfig;
use crate::i18n::t;
use crate::tf;
use crate::runtime::Runtime;

/// The built-in prompt text, verbatim.
///
/// This constant is the **single source of truth** for the built-in prompt: the `core` / `protocol`
/// segments are cut out of it at runtime, so the text lives in one place instead of two
/// hand-maintained halves that could drift apart.
///
/// It is **not frozen** — it was rewritten once (in the i18n pass) away from the historical
/// `SYSTEM_PROMPT`. What the tests at the end of this file pin is the *structure*, not the wording:
/// `split_is_lossless` proves the split/rejoin round-trips, and `default_build_equals_legacy_assembly`
/// proves the default assembly equals `BUILTIN_PROMPT + env_section`. Edit the wording freely, but
/// keep those two invariants true — the reason they matter is cost, since the server's prompt cache
/// keys off byte-stable prefixes and an accidental change wastes the cached prefix.
const BUILTIN_PROMPT: &str = r#"You are AOX, an AI agent running in a Rust host program on the user's local machine.

[Core Facts]
- The host ships with meta-tools (create_plugin / update_plugin / list_plugins / remove_plugin) and file tools (read_file / write_file / list_dir / remove / attach_file / edit_file). To read or write files, use the file tools directly. You don't need a plugin for that.
- To change part of an existing file, use edit_file (exact old_text → new_text, all edits applied atomically), not write_file. write_file is for creating a file or replacing all of its content. Before rewriting a file you have not read in this session, read it — and for a large file read only the slice you need (read_file's start_line / max_lines), rather than pulling the whole thing into the request.
- edit_file needs each old_text to occur exactly once: include the surrounding lines that make it unique. If it reports the file changed, read it again instead of guessing — the header's hash is what expected_hash is for.
- For a long task (several stages, several files, or work that may be interrupted), keep task_state's checkpoint current: goal, constraints, acceptance, what is done, what is next. A resumed conversation gets it back automatically. A single question or a one-file edit needs no checkpoint.
- Before you call a task done, make the evidence real: run verify once per acceptance item (file_hash, file_contains, path_exists, or output_contains over the output of a command tool). task_state with status "done" is refused while any item lacks a current passing check — and any file write after a check makes that check stale, so verify after your last edit.
- The tools list in each request is what you can call right now. Check it first, and don't reinvent what already exists. On a large registry some plugin tools are left out of the list until you call find_tools (the host's own tools are always listed), so search before concluding a capability is missing.
- When you need a new capability (running commands, network requests, data processing, parsing specialized formats, etc.), first call list_plugins to see whether an existing plugin can be reused. If not, write one with create_plugin, then call it to finish the task. Plugins are stored in plugins/ and load automatically after a restart.
- When the user needs to make a decision, call ask_user (one question plus 2 to 6 options). The UI renders it as clickable options and waits for the answer. Do not ask "Do you pick 1 or 2?" in the body text, because the host can't handle plain text there. The return value is the option the user clicked, or text they typed themselves (that is also a valid answer, so don't ask again).

[Plugin Protocol]
A plugin is a standalone process that communicates over stdio, one JSON object per line. Only protocol messages go to stdout. Write logs to stderr. You must flush after every write to stdout (the SDK does this automatically).

1. Initialize: the host sends {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}, and the plugin replies {"jsonrpc":"2.0","id":1,"result":{"name":"<plugin name>","tools":[{"name":"<tool name>","description":"<description>","parameters":<JSON Schema>}]}}
2. Call: the host sends {"jsonrpc":"2.0","id":N,"method":"tools/call","params":{"name":"<tool name>","arguments":{...}}}, and the plugin replies {"jsonrpc":"2.0","id":N,"result":{"content":"<result string>","is_error":false}}
3. Shutdown: the host sends {"jsonrpc":"2.0","method":"shutdown"} (no id), and the plugin exits.

[Python Plugin Template] (use the host SDK when possible)

import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
from plugin_sdk import serve

def my_tool(args):
    return "result string"  # args is a dict; return anything that str() can convert

serve("<plugin name>", {
    "my_tool": ("tool description", {"type": "object", "properties": {"x": {"type": "string"}}}, my_tool),
    # For read-only tools, add "read" as the 4th element
    "my_scan": ("scan a directory", {"type": "object", "properties": {"path": {"type": "string"}}}, my_scan, "read"),
})

[Plugin Rules]
- Tool names must be globally unique. Name them "pluginname_function" (e.g. fs_read_file).
- Read-only tools must add "read" as the 4th element in the declaration. The workspace boundary only blocks writes and deletes and places no limit on reads, but the host can't tell whether a black-box plugin is reading or writing. Anything not declared is blocked as "possibly writing", so tools that scan, count, list directories, or parse files get blocked as soon as they touch a path outside the workspace. Once "read" is declared, they can access those paths normally, and you don't need to ask the user for elevated permissions. Do not add this element to tools that write files, delete things, or change system state. Those must stay inside the workspace.
- parameters must be a valid JSON Schema object. content must be a string, and should be as concise as possible.
- Each plugin has a single responsibility. Prefer Python, then Node. Use Rust only for dependency-free, std-only single files.
- Never call input() or do anything that waits for human input. Handlers must return quickly. No infinite loops or long blocking calls, or the plugin will time out and be restarted.
- To change an existing plugin — your own from an earlier step, or any one you see in list_plugins — call update_plugin with the full new source. It hot-swaps in place: the old process is retired and the tools come back under the new code, no remove_plugin needed. Do NOT edit plugins/<name>/main.py with write_file: a running plugin keeps its old process, so the edit appears to have no effect and you'll wrongly conclude the plugin is broken.
- When rebuilding a plugin with the same name, reuse the original name (create_plugin also cleans up a leftover directory, and calling it on a live name simply rebuilds in place). Don't switch to new names like sysinfo2 or sysinfo_pro to get around an error.
- Plugin tools run automatically by default. Meta-tools that change the system itself, such as create_plugin, update_plugin and remove_plugin, require user authorization.

[Communication Style]
You are a friendly, capable collaborator. These principles apply to every message you send: progress updates, questions, error reports, and final summaries.
- By default, reply in the language the user uses.
- Warm and relaxed: talk like a thoughtful teammate, not a manual. Avoid terms the user hasn't used themselves. Never blame the user. Describe what happened and focus on how to fix it. If you make a mistake, say so, fix it, and move on, without long apologies.
- Ready to help: when the user is stuck or frustrated, acknowledge it briefly, then go straight to the solution. Give only the information the user needs to decide or act, and don't dump logs or internal details. If you can't do something, say so directly and offer the closest alternative.
- Crisp and clear: users skim. Lead with the key point, then add detail. No preamble, no restating the question, no "good question". Use the active voice and make clear who did what. Use plain words. Don't assume the user remembers earlier details, and reintroduce a term or file name when needed.
- Addressing the user: call the user "you", use "I" for what you did, and use "we" for shared work or the shared codebase.
- Abbreviations: spell out the full term the first time it appears, unless the user has already used it.
- Format: default to short plain-text paragraphs. Use lists only for steps or parallel items, and headings only for long answers with multiple parts. Use emoji sparingly. End full sentences with a period, but not short labels.

[Reply Rules]
- Don't end your reply with just a line like "Let me take a look...". Either start a tool call in the same reply, or give a complete conclusion. If information is missing, say clearly what is missing.
- Progress updates: during long tasks (many consecutive rounds of tool calls), speak only at phase boundaries, in one or two sentences saying what you just finished and what comes next, then continue. Don't narrate every tool, and don't repeat tool names or parameters. For very short tasks, just give the final answer.
- Questions: ask only when you can't proceed without the answer, and ask one clear question at a time. If there's a reasonable default, state it and continue, for example: "The repo already uses Postgres, so I'll stick with it. Tell me if you'd rather switch."
- Failure reports: cover three things: what went wrong, what happens next, and a realistic way forward.
- Pushing back: if a request looks risky or likely to fail, explain why in a friendly, brief way and suggest an alternative, then follow the user's decision.
- Final summary: when the task is done, summarize the result in natural language (making no further tool calls signals that the task is over). Start with the outcome, then list only what the user must know: key changes, anything you couldn't complete, anything the user needs to do, and how you verified the result. Don't repeat the diff. If there are no follow-ups, don't invent "next steps".
- When you write UI text, error messages, or documentation for end users, follow the same style: address the user as "you", use the active voice, never blame the user, give a way forward, and keep button text to a few short action words.

[Attachments and Images]
Files the user drags in are sent to you along with their text. You can see images directly, so there's no need to ask the user to describe them. Conversely, to let the model "see" a file (a screenshot, chart, or scan), you can't just return the path as a string. Attach it in one of the two ways below:
- attach_file (parameter: path): attaches a local file for the model. Images (JPEG/PNG/GIF/WebP) go into the next request. Other formats are sent as-is, and the endpoint decides whether to accept them.
- Plugin: add an attach array (absolute paths) to the returned JSON, for example {"content":"Screenshot taken","is_error":false,"attach":["C:\\...\\shot.png"]}.
- Hard limits: a single image <= 32 MiB, a single attachment <= 36 MiB, a single request body <= 48 MiB. If you exceed a limit you'll be told immediately. Compress or split into batches.
- Whether a format is supported is decided by the endpoint. If the endpoint replies "format not supported" or "this model does not support images", relay its words to the user as-is. Don't invent a reason of your own.

[Showing Content to the User]
- Images are displayed in the conversation stream (images attached via attach_file, and images declared by a plugin's attach). After producing an image, just say "the image is below". Don't restate its contents in text.
- To draw a diagram, write SVG: in the body text, use an svg code fence (three backticks immediately followed by svg, closed with three backticks), and the host will render it as a real image. The supported scope is small:
  - Root element: <svg viewBox="0 0 width height"> (writing the viewBox is recommended)
  - Only three shapes: <rect x y width height rx>, <line x1 y1 x2 y2>, <text x y font-size text-anchor>
  - Attributes: fill / stroke / stroke-width (#rrggbb, rgb(r,g,b), none, and common color names)
  - Not supported: <path> / <polygon> / <circle> / rotation / gradients. Build arrows from two <line> elements. Diamonds can't be drawn, so draw decision boxes as rectangles and state the condition in the text.
  - Set colors explicitly so the diagram stays clear in both light and dark themes.
- Don't draw large ASCII diagrams (the +---+ kind). They don't align, and real diagrams are now available."#;

/// The marker that cuts the `protocol` segment out of the built-in text (including
/// the blank line above it).
///
/// In the built-in text there is one blank line between "[Core Facts]" and "[Plugin Protocol]", so
/// the marker is `\n\n[Plugin Protocol]`.
const PROTOCOL_MARK: &str = "\n\n[Plugin Protocol]";

/// The section heading inside the marker itself.
///
/// `split_once` consumes the **entire marker** (heading included), so after splitting
/// the heading must be put back — otherwise the prompt would mysteriously lose a
/// "[Plugin Protocol]" line. This is exactly the bug the `split_is_lossless` test caught the
/// first time it ran.
///
/// Invariant: `PROTOCOL_MARK == "\n\n" + PROTOCOL_HEAD`.
const PROTOCOL_HEAD: &str = "[Plugin Protocol]";

/// The built-in segments' ids.
pub const CORE: &str = "core";
pub const PROTOCOL: &str = "protocol";
/// The runtime-generated segment's id.
pub const ENV: &str = "env";
/// The project rules file segment's id.
pub const RULES: &str = "rules";

/// The default sort key for config-appended segments: after the built-in segments
/// (10 / 20) and the runtime segment (30).
const FALLBACK_ORDER: i32 = 50;

/// A segment's source. Determines the label in `/prompt` and, in the future, "which
/// layer overrides which".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Built into the host
    Builtin,
    /// `config.toml`'s `[[prompt.segment]]`
    Config,
    /// Generated by the host at runtime (the run environment)
    Runtime,
    /// A project rules file such as `RULES.md`
    RulesFile,
    /// A component contribution (enabled in phase 6 · 6b: a plugin declares it via
    /// `[contributes] prompt_segments`, and the body is fetched from the subprocess by
    /// `prompt/segment`)
    Plugin,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Builtin => t("built-in"),
            Source::Config => t("config"),
            Source::Runtime => t("runtime"),
            Source::RulesFile => t("project file"),
            Source::Plugin => t("plugin"),
        }
    }
}

/// One segment of the prompt.
///
/// `body` carries **no** separators — concatenation is [`build`]'s job alone. See the
/// module docs for why.
#[derive(Debug, Clone)]
pub struct PromptSegment {
    /// Globally unique. A config segment with the same id overrides the built-in one.
    pub id: String,
    /// Display name (used by `/prompt`).
    pub title: String,
    pub body: String,
    /// The default sort key (ascending); the same order keeps registration order (the sort is stable).
    pub order: i32,
    pub source: Source,
}

// ─────────────────────────── Built-in segments ───────────────────────────

/// The two built-in segments: `core` / `protocol`.
pub fn builtin_segments() -> Vec<PromptSegment> {
    let (core, protocol) = split_builtin();
    vec![
        PromptSegment {
            id: CORE.into(),
            title: t("Identity & core facts").into(),
            body: core,
            order: 10,
            source: Source::Builtin,
        },
        PromptSegment {
            id: PROTOCOL.into(),
            title: t("Plugin protocol & built-in rules").into(),
            body: protocol,
            order: 20,
            source: Source::Builtin,
        },
    ]
}

/// Split the built-in constant into the `core` / `protocol` segments.
///
/// When it can't be split, degrade to "the whole thing counts as core" and **do not
/// panic** — a prompt missing a segment beats a program that won't start.
fn split_builtin() -> (String, String) {
    match t(BUILTIN_PROMPT).split_once(t(PROTOCOL_MARK)) {
        Some((core, rest)) => (core.to_string(), format!("{}{rest}", t(PROTOCOL_HEAD))),
        None => (BUILTIN_PROMPT.to_string(), String::new()),
    }
}

// ─────────────────────────── Runtime-generated segment ───────────────────────────

/// The run-environment segment.
///
/// **Moved verbatim from `src/agent.rs`** (byte-for-byte, including the leading
/// `\n\n`, which [`strip_sep`] strips off). It tells the model the host's real paths
/// and whether this machine actually has python / node / rustc — otherwise the model
/// would repeatedly create doomed python plugins on a bare machine and burn the whole
/// round.
fn env_section(rt: &Runtime) -> String {
    let mut s = tf!(
        "\n\n# Run environment (determined by the host; use as-is, do not guess)\n\
         - App root (= the plugin process's working directory): {}\n\
         - Plugins directory: {}\n\
         - Plugin sources look like: {}\\<plugin name>\\main.py\n\
         - Platform: {}\n\
         - All relative paths are relative to the app root; when touching this project's files, the absolute paths above are the safest thing to use.\n\
         - Plugin subprocesses are started by the host with the app root as their working directory, so relative paths inside a plugin resolve exactly as you see them on the host machine.\n\
         - Python plugins are forced to UTF-8 by the host (PYTHONUTF8=1, PYTHONIOENCODING=utf-8); you don't need to write encoding-compat code.\n\
         - There is only one plugins/ inside your plugin directory (the one above); don't go looking in target/ or other build-output directories.",
        rt.root.display(),
        rt.plugins_dir.display(),
        rt.plugins_dir.display(),
        std::env::consts::OS,
    );
    // The workspace the model is **actually** confined to. **Read live, never from a startup-time
    // value**: under the `default` tier the root is only decided on the first turn, and it can
    // change mid-conversation (`/workplace`, the GUI button). Baking in the app root here is
    // exactly why the model once answered "should I tidy the documents in my own install
    // directory?" after the user had picked the Desktop. `Agent::refresh_system_prompt` rebuilds
    // this whole prompt whenever the root changes, so this line stays honest.
    //
    // Kept out of the big `tf!` above so that block's translation key is untouched.
    let (ws_mode, ws_root) = rt.workplace.snapshot();
    // No root means "not decided yet" (`Pending`) or the unrestricted tier (`Full`); either way the
    // conventional base is the app root.
    let ws = ws_root.unwrap_or_else(|| rt.root.clone());
    let rule = if matches!(ws_mode, crate::workplace::Mode::Full) {
        t("it is unrestricted (full access): you may read, write and delete anywhere")
    } else {
        t("writes and deletes may only land under it; reading anywhere is still allowed")
    };
    s.push_str(&tf!(
        "\n\n# Workspace layout\n\
         Your workspace is {} —— {}. The \"app root\" above is only where plugins run; your workspace can be a different directory, and it is where the user expects you to work.\n\
         - Scratch files —— screenshots, caches, one-off output —— go under {}\\.aox\\tmp\\. That `.aox\\` folder is the host's own: `history\\` holds this workspace's conversations and `memory\\` your notes (see the next section), so read them if they help, but never reorganise or delete them. Don't scatter files in the workspace root.\n\
         - When the user asks you to create something durable (a document, a code project), give it its own folder: {}\\workplace\\<short project name>\\, and work inside it. Name the folder after the project in a few words (e.g. workplace\\business-site).\n",
        ws.display(),
        rule,
        ws.display(),
        ws.display(),
    ));
    // The AI's own per-workspace notes (see `src/workplace.rs`: the `.aox/memory/` folder). Today's
    // local date is passed in so the model can name the file without guessing. Kept in its own `tf!`
    // so the big "run environment" key above is untouched.
    s.push_str(&tf!(
        "\n\n# Memory (your own notes about this workspace)\n\
         The host sets aside a `.aox\\memory\\` folder at the workspace root for your notes. Record durable work there, in the folder you are actually working in:\n\
         - If you are working inside a project folder (e.g. `workplace\\<project>\\`), keep the note in that project's own `.aox\\memory\\`; otherwise use the workspace root's one.\n\
         - One markdown file per day, named after the date: `<YYYY-MM-DD>.md`. **Today is {}**.\n\
         - **Append** to today's file rather than overwriting it: read it first, then write back the old content plus what you just added. Never wipe notes from earlier in the same day.\n\
         - Core content only, no filler: what you did, any pitfall you hit, how you got past it, and what got finished. A few terse bullets beat a paragraph of prose.\n\
         - Skip it for trivial questions or a one-line answer. For long-running or multi-session work, read the most recent notes first when it helps.\n",
        crate::history::local_date(),
    ));
    // Tell the model truthfully whether this machine has python / node / rustc.
    // Otherwise it would repeatedly create doomed python plugins on a bare machine and
    // burn the whole round.
    s.push_str(&crate::runtimes::prompt_section(&rt.root, &rt.config.runtimes));
    // The command line. Its own paragraph (not folded into the block above) because the boundary
    // rules here are the ones a model is most likely to get wrong — retrying a refused write, or
    // assuming reads are blocked too.
    if rt.config.shell.enabled {
        let bash = crate::tool::bash::available(rt);
        let prefer_bash = crate::shell::prefers_bash(&rt.config.shell);

        s.push_str(&t("\n\n# Command line (`bash` and `shell`)\n"));
        // The order *is* the whole of `[shell] prefer`: one line states it together with the reason,
        // and each tool then gets one bullet written order-neutrally. Two small literals instead of two
        // whole paragraphs, so the ordering is data rather than duplicated prose.
        //
        // It cannot be anything more than wording: a command has side effects, so the host must never
        // run one twice to see which shell accepts it. That is why this reads as advice, not fallback.
        match (&bash, prefer_bash) {
            (Some(_), true) => s.push_str(&t(
                "- Preferred order: **bash, then `shell` (cmd.exe), then PowerShell.** bash can do a \
                 whole job in one call — pipes, globs, `$(…)`, `&&`, sed/grep/find/awk. `cmd` starts \
                 ~3× faster (≈87 ms vs ≈270 ms) and takes `C:\\` paths and `/flag` arguments verbatim, \
                 so use it for one short native command or a cmd builtin, not for a pipeline.\n",
            )),
            (Some(_), false) => s.push_str(&t(
                "- Preferred order: **`shell` (cmd.exe), then bash, then PowerShell.** cmd starts ~3× \
                 faster (≈87 ms vs ≈270 ms) and takes `C:\\` paths and `/flag` arguments verbatim; bash \
                 is what to reach for when the job needs pipes, globs, sed/grep/find, or several steps \
                 in one script.\n",
            )),
            (None, _) => s.push_str(&t(
                "- `bash` is not installed on this machine, so `shell` (cmd.exe) is your command line: \
                 `dir`, `type`, `where`, builtins, and any `.exe`. (`[shell] bash` in config.toml, or a \
                 Git for Windows install, would add bash.)\n",
            )),
        }
        let bash_bullet = bash.as_ref().map(|b| {
            crate::tf!(
                "- **`bash`** ({} on this machine) — pipes, globs, `$(…)`, `&&`, sed/grep/find/awk, \
                 loops: several steps in one call. `$AOX_WORKSPACE` is the workspace Windows-style and \
                 `$AOX_WORKSPACE_U` the same path with forward slashes (`E:/dir`) — use the second one \
                 inside the script, it is the form every bash flavour understands.\n",
                b.flavour
            )
        });
        let cmd_bullet = t(
            "- **`shell`** — cmd.exe unless you pass `shell`, and the Windows-native things belong \
             here: `dir`, `type`, `where`, `copy`, a cmd builtin, or anything taking `C:\\` paths or \
             `/flag` arguments that bash would rewrite. Pass `shell: \"powershell\"` only for the .NET \
             object model, JSON or COM (it needs ~1.1 s just to start); `shell: \"git\"` runs a plain \
             git subcommand with no shell in between.\n",
        );
        if prefer_bash {
            if let Some(b) = &bash_bullet {
                s.push_str(b);
            }
            s.push_str(cmd_bullet);
        } else {
            s.push_str(cmd_bullet);
            if let Some(b) = &bash_bullet {
                s.push_str(b);
            }
        }
        s.push_str(&t(
            "- Everything runs **inside your workspace**: `cwd` must be an absolute directory inside \
             it (it defaults to the workspace root), so a relative path in the command lands inside \
             too. Leaving the workspace with `cd` is refused.\n\
             - **Writes and deletes outside the workspace are refused**: the host checks the command \
             before it runs, so keep those targets inside the workspace. **Reading anywhere is \
             allowed**, including outside the workspace. So `type C:\\logs\\x.log` works while \
             `> C:\\logs\\x.log` does not.\n\
             - A refusal is the workspace boundary, not a broken command: report it to the user \
             instead of retrying or rephrasing it.\n\
             - Every call returns the exit code, the elapsed time, stdout/stderr (long output keeps its \
             head and tail) and the boundary it actually got. Read `stderr` before concluding a \
             command failed for your own reason.\n\
             - Prefer one command that pipes and chains over several round trips: each call costs a \
             request.",
        ));
    }
    s
}

fn env_segment(rt: &Runtime) -> PromptSegment {
    PromptSegment {
        id: ENV.into(),
        title: t("Run environment").into(),
        body: strip_sep(env_section(rt)),
        order: 30,
        source: Source::Runtime,
    }
}

/// The project rules segment (`RULES.md`). Returns `None` when the file is missing or
/// its content is empty.
fn rules_segment(rt: &Runtime) -> Option<PromptSegment> {
    let path = rt.config.agent.rules_file.as_deref()?;
    let path = PathBuf::from(path);
    let path = if path.is_absolute() { path } else { rt.root.join(path) };
    let text = std::fs::read_to_string(path).ok()?;
    let body = text.trim();
    if body.is_empty() {
        return None;
    }
    Some(PromptSegment {
        id: RULES.into(),
        title: t("Project rules (RULES.md)").into(),
        // The segment format is "# Project rules\n" + body; the leading "\n\n"
        // separator is supplied by build(), so here we only need the heading + body.
        body: tf!("# Project rules\n{}", body),
        order: 40,
        source: Source::RulesFile,
    })
}

/// Strip the `\n\n` at the start of a segment used for concatenation. [`build`] puts it
/// back uniformly; not stripping would leave two extra blank lines.
fn strip_sep(s: String) -> String {
    match s.strip_prefix("\n\n") {
        Some(rest) => rest.to_string(),
        None => s,
    }
}

// ─────────────────────────── Registry ───────────────────────────

/// For `Runtime::new` to build the initial registry: built-in segments + config
/// segments (same id overrides built-in).
///
/// "Config overrides built-in" is deliberate — it lets `[[prompt.segment]]` both append
/// new segments and replace the built-in `core` / `protocol` wholesale, without having
/// to `disable` first and then add a segment.
pub fn registry_from_config(cfg: &PromptConfig) -> Vec<PromptSegment> {
    let mut segs = builtin_segments();
    for extra in &cfg.segment {
        let seg = PromptSegment {
            id: extra.id.trim().to_string(),
            title: extra.title.clone().unwrap_or_else(|| extra.id.trim().to_string()),
            body: extra.body.trim().to_string(),
            order: extra.order.unwrap_or(FALLBACK_ORDER),
            source: Source::Config,
        };
        if seg.id.is_empty() {
            continue;
        }
        match segs.iter_mut().find(|s| s.id == seg.id) {
            Some(slot) => *slot = seg,
            None => segs.push(seg),
        }
    }
    segs
}

// ─────────────────────────── Assembly ───────────────────────────

/// All currently effective segments (unsorted, unfiltered): those in the registry plus
/// those generated at runtime.
pub fn collect(rt: &Runtime) -> Vec<PromptSegment> {
    let mut segs = rt.prompt_segments();
    segs.push(env_segment(rt));
    if let Some(r) = rules_segment(rt) {
        segs.push(r);
    }
    segs
}

/// The final sequence after config filtering + sorting — the one `/prompt` displays and
/// [`build`] uses.
pub fn assemble(rt: &Runtime) -> Vec<PromptSegment> {
    let cfg = &rt.config.prompt;
    let mut segs = collect(rt);

    // 1. Disable
    segs.retain(|s| !cfg.disable.iter().any(|d| d.trim() == s.id));
    // 2. Drop empty segments (an emptied config segment needn't occupy a "\n\n")
    segs.retain(|s| !s.body.is_empty());
    // 3. Default order (stable sort; the same order keeps registration order)
    segs.sort_by_key(|s| s.order);

    // 4. Explicit order: the listed ids move to the front in listed order, and the
    //    unlisted ones keep the default order after them. So `order = ["core", "env"]`
    //    means "move env after core", without having to list all four segments.
    if !cfg.order.is_empty() {
        let mut head: Vec<PromptSegment> = Vec::with_capacity(segs.len());
        for id in &cfg.order {
            let id = id.trim();
            if let Some(pos) = segs.iter().position(|s| s.id == id) {
                head.push(segs.remove(pos));
            }
        }
        head.extend(segs);
        segs = head;
    }
    segs
}

/// Assemble the final system prompt.
pub fn build(rt: &Runtime) -> String {
    assemble(rt)
        .iter()
        .map(|s| s.body.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The body of the `/prompt` command: the segment list + each segment's source and
/// first line.
///
/// Deliberately **not column-aligned**: segment ids are ASCII while titles are Chinese,
/// so padding with `{:<10}` and the like only gets knocked askew by the double-width
/// characters. Listing them one per line actually reads better.
pub fn report(rt: &Runtime) -> String {
    let segs = assemble(rt);
    let total: usize = segs.iter().map(|s| s.body.chars().count()).sum();
    let mut out = tf!(
        "System prompt: {} segments · {} chars\n",
        segs.len(),
        crate::llm::thousands(total as u64)
    );
    for (i, s) in segs.iter().enumerate() {
        let first = s.body.lines().next().unwrap_or("").trim();
        out.push_str(&format!(
            "\n  {}. {} ({}) · {}\n     {}\n",
            i + 1,
            s.id,
            s.source.label(),
            s.title,
            elide(first, 60)
        ));
    }
    let (all, shown) = (collect(rt).len(), segs.len());
    if all > shown {
        out.push_str(&tf!("\n  (plus {} segments disabled or empty, not assembled)", all - shown));
    }
    out.push_str(t("\n  Edit [prompt] in config.toml to reorder / disable / add segments."));
    out
}

fn elide(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, PromptConfig, PromptSegmentConfig, RuntimesConfig};

    fn rt_with(prompt: PromptConfig) -> std::sync::Arc<Runtime> {
        rt_tuned(prompt, "bash")
    }

    /// The same, with `[shell] prefer` under the test's control.
    fn rt_tuned(prompt: PromptConfig, shell_prefer: &str) -> std::sync::Arc<Runtime> {
        let mut cfg = Config::dummy();
        // Make the env segment's content stable: skip the three-tier resolution and just
    // give a non-existent command name.
        cfg.runtimes = RuntimesConfig {
            python: "__none__.exe".into(),
            node: "__none__.exe".into(),
            rustc: "__none__.exe".into(),
            prefer: "system".into(),
            python_url: None,
            python_sha256: None,
        };
        cfg.shell.prefer = shell_prefer.to_string();
        cfg.prompt = prompt;
        let root = std::env::temp_dir().join("aox-prompt-test");
        Runtime::new(cfg, root.clone(), root.join("plugins")).expect("test runtime should not fail on the storage backend")
    }

    /// `[shell] prefer` moves **one line** and one bullet order — nothing else in the section.
    ///
    /// That is the whole reason the section is assembled from per-sentence literals instead of two
    /// finished paragraphs: the ordering is data, so it can be flipped in one place and checked the
    /// same way. It stays advice to the model either way; the host would never run a command twice to
    /// see which shell accepts it.
    #[test]
    fn the_command_line_order_follows_the_config() {
        let bash_first = rt_tuned(PromptConfig::default(), "bash");
        let cmd_first = rt_tuned(PromptConfig::default(), "cmd");
        let (b, c) = (env_section(&bash_first), env_section(&cmd_first));
        assert_ne!(b, c, "prefer must change the wording");

        if crate::tool::bash::available(&bash_first).is_none() {
            // No bash on this machine: there is no order to flip, and both settings must agree on
            // saying so rather than promising a tool that is not there.
            assert_eq!(b, c, "without bash the order cannot differ");
            return;
        }
        let at = |s: &str, marker: &str| s.find(marker).unwrap_or_else(|| panic!("{marker} missing"));
        assert!(at(&b, "- **`bash`** (") < at(&b, "- **`shell`** —"), "bash must come first:\n{b}");
        assert!(at(&c, "- **`shell`** —") < at(&c, "- **`bash`** ("), "cmd must come first:\n{c}");
        // The order line itself has to name the same first tool as the bullets below it.
        assert!(b.contains("Preferred order: **bash,"), "the order line must agree with the bullets:\n{b}");
        assert!(c.contains("Preferred order: **`shell` (cmd.exe),"), "the order line must agree:\n{c}");
        // `--nocapture` prints both paragraphs: the assertions above only pin the *order*, and the
        // wording is the part a person has to read once.
        for (tag, s) in [("bash", &b), ("cmd", &c)] {
            if let Some(i) = s.find("# Command line") {
                let end = s[i..].find("\n\n#").map_or(s.len(), |j| i + j);
                println!("--- prefer={tag} ---\n{}", &s[i..end]);
            }
        }
    }

    /// Splitting must be lossless: reassembling verbatim must equal the original string.
    #[test]
    fn split_is_lossless() {
        let (core, protocol) = split_builtin();
        assert!(!core.is_empty() && !protocol.is_empty(), "both segments should be non-empty");
        assert_eq!(format!("{core}\n\n{protocol}"), BUILTIN_PROMPT);
    }

    /// Under the default config, the assembled result must be **byte-for-byte
    /// identical** to the historical implementation: `SYSTEM_PROMPT + env_section(rt)`
    /// (the test environment has no RULES.md, hence no project rules segment).
    #[test]
    fn default_build_equals_legacy_assembly() {
        let rt = rt_with(PromptConfig::default());
        let legacy = format!("{BUILTIN_PROMPT}{}", env_section(&rt));
        assert_eq!(build(&rt), legacy);
    }

    /// Segment bodies never carry separators — with them, four newlines would appear at
    /// the joins.
    #[test]
    fn segment_bodies_carry_no_separators() {
        let rt = rt_with(PromptConfig::default());
        for s in assemble(&rt) {
            assert!(!s.body.starts_with('\n'), "segment {} starts with a newline", s.id);
            assert!(!s.body.ends_with('\n'), "segment {} ends with a newline", s.id);
        }
    }

    /// The env segment must teach the workspace-memory convention **and carry today's date**, so
    /// the model can name its note file (`<YYYY-MM-DD>.md`) without guessing the day.
    #[test]
    fn env_segment_carries_the_memory_rule_and_todays_date() {
        let rt = rt_with(PromptConfig::default());
        let env = env_section(&rt);
        assert!(
            env.contains("# Memory (your own notes about this workspace)"),
            "the memory rule must be in the env segment:\n{env}"
        );
        assert!(env.contains(".aox\\memory"), "the note folder must be named:\n{env}");
        assert!(
            env.contains(&crate::history::local_date()),
            "the env segment must carry today's local date ({})",
            crate::history::local_date()
        );
    }

    /// The layout segment must name the **current** workspace and follow a mid-conversation switch.
    ///
    /// Regression: it used to hardcode `rt.root` (the app root / install dir), so after the user
    /// picked another folder (`/workplace <dir>`, the GUI button) the model still believed it was
    /// confined to the install directory — it then asked whether it should tidy "its own install
    /// directory" instead of the folder the user meant.
    #[test]
    fn layout_segment_names_the_current_workspace() {
        let rt = rt_with(PromptConfig::default());
        let app = rt.root.clone();
        // Before the root is decided, the app root is the honest fallback.
        assert!(env_section(&rt).contains(&app.display().to_string()), "the undecided root should fall back to the app root");

        let picked = std::env::temp_dir().join("aox-prompt-ws-picked");
        let _ = std::fs::remove_dir_all(&picked);
        std::fs::create_dir_all(&picked).unwrap();
        rt.workplace
            .set_mode(crate::workplace::Mode::Fixed(picked.clone()), &app)
            .unwrap();

        let env = env_section(&rt);
        assert!(env.contains(&picked.display().to_string()), "the picked root must be named:\n{env}");
        assert!(
            !env.contains(&format!("Your workspace is {} ——", app.display())),
            "the app root must no longer be presented as the workspace:\n{env}"
        );

        // The unrestricted tier must say so rather than claim a confined root.
        rt.workplace.set_mode(crate::workplace::Mode::Full, &app).unwrap();
        assert!(env_section(&rt).contains("unrestricted"), "the full tier must be described as unrestricted");
        let _ = std::fs::remove_dir_all(&picked);
    }

    #[test]
    fn disable_removes_segment() {
        let rt = rt_with(PromptConfig { disable: vec![ENV.into()], ..Default::default() });
        let ids: Vec<_> = assemble(&rt).iter().map(|s| s.id.clone()).collect();
        assert!(!ids.contains(&ENV.to_string()));
        assert!(ids.contains(&CORE.to_string()));
    }

    #[test]
    fn order_moves_segment_to_front() {
        let rt = rt_with(PromptConfig { order: vec![ENV.into()], ..Default::default() });
        let ids: Vec<_> = assemble(&rt).iter().map(|s| s.id.clone()).collect();
        assert_eq!(ids.first().map(String::as_str), Some(ENV));
        // Unlisted segments keep the default order after it
        assert_eq!(&ids[1..], &[CORE.to_string(), PROTOCOL.to_string()]);
    }

    #[test]
    fn config_segment_is_appended() {
        let rt = rt_with(PromptConfig {
            segment: vec![PromptSegmentConfig {
                id: "style".into(),
                title: Some("Team style".into()),
                body: "Conclusion first, reasons after.".into(),
                order: None,
            }],
            ..Default::default()
        });
        let segs = assemble(&rt);
        let last = segs.last().unwrap();
        assert_eq!(last.id, "style");
        assert_eq!(last.source, Source::Config);
        assert!(build(&rt).ends_with("Conclusion first, reasons after."));
    }

    /// A config segment with the same id replaces the built-in one wholesale (rather
    /// than both coexisting).
    #[test]
    fn config_segment_overrides_builtin() {
        let rt = rt_with(PromptConfig {
            segment: vec![PromptSegmentConfig {
                id: CORE.into(),
                title: None,
                body: "You are an assistant that only does code review.".into(),
                order: None,
            }],
            ..Default::default()
        });
        let segs = assemble(&rt);
        let core = segs.iter().find(|s| s.id == CORE).unwrap();
        assert_eq!(core.source, Source::Config);
        assert_eq!(core.body, "You are an assistant that only does code review.");
        // The built-in body must not appear anymore
        assert!(!build(&rt).contains("You are AOX"));
    }

    /// A disabled built-in segment must no longer appear in the final prompt.
    #[test]
    fn disabled_builtin_text_is_gone() {
        let rt = rt_with(PromptConfig { disable: vec![PROTOCOL.into()], ..Default::default() });
        let out = build(&rt);
        assert!(!out.contains("[Plugin Protocol]"));
        assert!(out.contains("[Core Facts]"));
    }
}
