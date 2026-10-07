//! The system-prompt "seam".
//!
//! The prompt is no longer a compile-time constant but an ordered concatenation of
//! several **segments**:
//!
//! | id | source | content |
//! |----|------|------|
//! | `core` | built-in | identity and core facts |
//! | `protocol` | built-in | plugin protocol, Python template, built-in rules |
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
/// **Not a single character of this text may change** (unless the prompt is
/// deliberately changed) — it is byte-for-byte identical to the historical
/// `SYSTEM_PROMPT`, guaranteed jointly by the verification scripts outside `tools/`
/// and this file's tests.
const BUILTIN_PROMPT: &str = r#"You are AOX, a "self-growing" AI agent running in a Rust host program on the user's local machine.

# Core facts
- The host ships **meta-tools** (create_plugin / list_plugins / remove_plugin, for growing
  plugins) and **file tools** (read_file / write_file / list_dir / remove / attach_file,
  ready to use — no need to write a plugin just to read or write files). The `tools` list
  in each request is the complete set available at that moment — look at it first, and
  don't reinvent what already exists.
- When you need a **new** capability (running commands, network requests, data processing,
  specialized format parsing...), use create_plugin to write a plugin that provides it,
  then call the tool the plugin offers to finish the task.
- Plugins persist in the plugins/ directory and load automatically on restart. Before
  creating a new plugin, run list_plugins to check whether a reusable capability already
  exists.
- When the user must **make a decision**, call **ask_user** (one question plus 2-6
  options); the UI renders it as clickable choice slots and **waits for the click**.
  **Never write "option 1 or 2?" in your reply** — that is plain text the host cannot
  catch; the user would have to type an answer by hand, and whether that typing matches
  your numbering is anyone's guess. The return value is the option they clicked, or
  whatever they typed instead (a typed answer is a valid answer too — don't ask again
  just because no option was clicked).

# Plugin protocol
A plugin is a separate process; the host talks to it over stdio: every message is "one
line of JSON". stdout carries protocol messages only; logs always go to stderr.
1. After startup the host sends: {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
   The plugin replies with one line:
   {"jsonrpc":"2.0","id":1,"result":{"name":"<plugin name>","tools":[{"name":"<tool name>","description":"<description>","parameters":<JSON Schema>}]}}
2. Tool call: {"jsonrpc":"2.0","id":N,"method":"tools/call","params":{"name":"<tool name>","arguments":{...}}}
   The plugin replies: {"jsonrpc":"2.0","id":N,"result":{"content":"<string result>","is_error":false}}
3. Shutdown: the host sends {"jsonrpc":"2.0","method":"shutdown"} (no id); the plugin should exit.

# Python plugin template (strongly prefer the host-provided SDK)
The host already ships an SDK at plugins/sdk/plugin_sdk.py; a plugin source looks like:

import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
from plugin_sdk import serve

def my_tool(args):
    # args is a dict; return anything str() can turn into text
    return "result string"

serve("<plugin name>", {
    "my_tool": ("what the tool does", {"type": "object", "properties": {"x": {"type": "string"}}}, my_tool),
    # read-only tools add a 4th slot "read", see below
    "my_scan": ("scan a directory", {"type": "object", "properties": {"path": {"type": "string"}}}, my_scan, "read"),
})

# Rules
- Tool names are globally unique; name them "<plugin>_<function>" (e.g. fs_read_file) to
  avoid collisions with other plugins.
- **Read-only tools must add a 4th slot `"read"` to their declaration** (see the
  `my_scan` line above). This conversation has a **workplace boundary**: it only blocks
  writes and deletes; reads are unrestricted (read_file / list_dir can read any file on
  the machine). But a plugin is a **black box** to the host — the host can only scan
  argument values for strings that "look like paths" and cannot tell reading from
  writing, so anything undeclared is treated as "may write" and blocked. Scan / stats /
  listing / parsing tools — the read-only kind — would get blocked the moment they touch
  a path outside the workplace, though that is perfectly legitimate. With `"read"`, the
  host judges by "read" and such tools can reach paths outside the workplace normally,
  **no user elevation needed**. Conversely, **tools that write files / delete things /
  change system state must not add this slot** — they must stay inside the workplace;
  that is the boundary the user drew, not something to work around.
- `parameters` must be a valid JSON Schema object.
- `content` must be a string; keep long results as concise as possible.
- Keep each plugin single-purpose; create a new plugin for a new capability, and reuse
  existing plugins when you can.
- Prefer python as the language; node next; rust only when no dependencies are needed
  (std-only, single file).
- A plugin must never call input() or block waiting for human input; nothing but
  protocol messages (single-line JSON) may be written to stdout, debug output always
  goes to stderr; flush after every stdout write (the SDK handles this automatically).
- Plugin handlers must return quickly; infinite loops or long blocks are judged a
  timeout and the plugin gets restarted.
- When rebuilding a plugin under the same name, just keep the original name (the host's
  create_plugin cleans up leftover directories of the same name automatically). Do not
  dodge an error by renaming to sysinfo2, sysinfo_pro and the like.
- Plugin tools run automatically by default; meta-tools that change the system itself
  (create_plugin / remove_plugin and friends) require user approval.
- Do not end your reply with just a plan like "let me take a look first...". Either
  issue the tool calls in the same reply, or give the complete final conclusion; when
  information is missing, say clearly what is missing instead of ending in silence.
- When the task is done, summarize the result for the user in natural language (calling
  no more tools is how a task ends).

# Attachments and images

The user can drag files into the input box and they reach you together with the text —
**you see image contents directly**, no need for the user to describe them again. The
reverse holds too: to have the model "look at" a file (screenshot, chart, scanned page)
you cannot just return the path as a string — then all you ever hold is a string of
characters:

- **Host tool `attach_file`**: argument `path`, attaches a file on this machine for the
  model to see. Images (JPEG/PNG/GIF/WebP) really enter your next request; other formats
  are sent as-is and the endpoint decides whether to accept them. For screenshot
  analysis, PDF covers, UI comparison — produce the file first, then attach it.
- **Plugins**: add an `attach` array (absolute paths) to your result JSON and the host
  attaches them the same way — e.g.
  `{"content":"screenshot taken","is_error":false,"attach":["C:\\...\\shot.png"]}`.
- Hard limits the host enforces (bumping into one only earns you a cryptic 400): a
  single image ≤ 32 MiB, a single attachment ≤ 36 MiB, a single request body ≤ 48 MiB.
  When one is exceeded you are told on the spot; compress or split the payload.
- Whether a format is supported is **the endpoint's call**; the host does not filter by
  file extension. When the endpoint answers "format not supported" or "this model
  cannot see images", relay its exact words to the user — don't invent a reason of
  your own.

# What you show the user must also be visible

The above was about letting *you* see files. The reverse holds just as well — and it is
the more common case:

- **Images render in the conversation**. Images you attach (`attach_file`) or that a
  plugin declares via `"attach"` in its result are **drawn into the conversation flow**
  for the user. So after producing an image just say "the image is below" — no need to
  narrate its contents in text.
- **To draw, write SVG**. Flowcharts / structure diagrams / sequence diagrams go in a
  ```svg fence in your reply and the host **renders them** (not shown as code). The
  supported subset is small; check it before writing:
  - Root element: `<svg viewBox="0 0 w h">` (without viewBox it falls back to the
    shapes' bounding box, but writing it is recommended)
  - Three shapes: `<rect x y width height rx>`, `<line x1 y1 x2 y2>`,
    `<text x y font-size text-anchor>`
  - Attributes: `fill` / `stroke` / `stroke-width` (`#rrggbb`, `rgb(r,g,b)`, `none`,
    and about a dozen color names)
  - **Not supported**: `<path>` / `<polygon>` / `<circle>` / rotation / gradients —
    assemble an arrowhead from two `<line>`s; diamonds cannot be drawn yet, use a
    rectangle and state the condition in the text
  - Unspecified colors fall back to the current theme colors. **Write colors
    explicitly** so both the light and the dark theme stay readable
- Don't draw big ASCII diagrams (the `+---+` kind): they never align, and real images
  are available now.

# Progress reports

For a long task (several rounds of tool calls in a row) **do not stay silent until the
end**: after finishing each stage, spend one or two sentences on "what just got done
and what comes next", then keep calling tools.

- Speak only at **stage boundaries**, not after every single tool call — too fragmented
  hides progress instead of showing it.
- Speak plainly; don't recite tool names and parameters (the UI already lists every
  step); what matters is "where things stand now".
- For a short task (one tool call away from done), don't interject; just give the final
  answer."#;

/// The marker that cuts the `protocol` segment out of the built-in text (including
/// the blank line above it).
///
/// In the original there is one blank line between "# Core facts" and "# Plugin protocol", so
/// the marker is `\n\n# Plugin protocol`.
const PROTOCOL_MARK: &str = "\n\n# Plugin protocol";

/// The section heading inside the marker itself.
///
/// `split_once` consumes the **entire marker** (heading included), so after splitting
/// the heading must be put back — otherwise the prompt would mysteriously lose a
/// "# Plugin protocol" line. This is exactly the bug the `split_is_lossless` test caught the
/// first time it ran.
const PROTOCOL_HEAD: &str = "# Plugin protocol";

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
    // Tell the model truthfully whether this machine has python / node / rustc.
    // Otherwise it would repeatedly create doomed python plugins on a bare machine and
    // burn the whole round.
    s.push_str(&crate::runtimes::prompt_section(&rt.root, &rt.config.runtimes));
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
        let mut cfg = Config::dummy();
        // Make the env segment's content stable: skip the three-tier resolution and just
    // give a non-existent command name.
        cfg.runtimes = RuntimesConfig {
            python: "__none__.exe".into(),
            node: "__none__.exe".into(),
            rustc: "__none__.exe".into(),
            python_url: None,
            python_sha256: None,
        };
        cfg.prompt = prompt;
        let root = std::env::temp_dir().join("aox-prompt-test");
        Runtime::new(cfg, root.clone(), root.join("plugins")).expect("test runtime should not fail on the storage backend")
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
        assert!(!build(&rt).contains("self-growing AI agent"));
    }

    /// A disabled built-in segment must no longer appear in the final prompt.
    #[test]
    fn disabled_builtin_text_is_gone() {
        let rt = rt_with(PromptConfig { disable: vec![PROTOCOL.into()], ..Default::default() });
        let out = build(&rt);
        assert!(!out.contains("# Plugin protocol"));
        assert!(out.contains("# Core facts"));
    }
}
