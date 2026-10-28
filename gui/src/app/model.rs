//! The data model: what a conversation looks like to the window, and the small derivations on it.
//!
//! These are the types every screen passes around -- a transcript is a `Vec<Item>`, a turn in flight is
//! a `Live`, a tool call is a `StepView`. Nothing here draws or talks to the agent; keeping it in one
//! file means "what can appear on screen" is one screenful of code instead of being spread through the
//! painters.
//!
//! Fields are `pub(super)` rather than private: Rust's privacy stops at the module boundary, so a type
//! declared here is only reachable from its **siblings** (`transcript`, `events`, …) if its fields say
//! so. `pub(super)` is the narrowest annotation that means "inside this window's code only".

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    Info,
    Warn,
    Error,
}

impl NoteKind {
    /// A monochrome scheme has no red/yellow/green left to spend, so we use lightness
    /// instead: body colour / secondary / weaker.
    pub(super) fn color(self) -> Color32 {
        match self {
            NoteKind::Info => P::text_dim(),
            NoteKind::Warn => P::text_dim(),
            NoteKind::Error => P::text(),
        }
    }
}

/// One tool call (for the collapsed display).
#[derive(Clone)]
pub(super) struct StepView {
    pub(super) name: String,
    pub(super) plugin: Option<String>,
    /// What this call is **for**, in a few words — the model's own `purpose` argument when it gave
    /// one, otherwise the command itself for `bash`/`shell`.
    ///
    /// The row used to name the tool and nothing else, which for the two shell tools is the whole
    /// problem: a row reading `bash` says nothing about whether it is compiling, searching or
    /// reading a log. Like [`Self::subject`], this is parsed out of the arguments while they are
    /// still arriving, so the label appears with the row rather than after the call.
    pub(super) purpose: Option<String>,
    /// The file this call is *about*, when naming it is worth a glance — `write_file`'s `path`, read
    /// off the arguments. `None` for every other tool (see `SUBJECT_TOOLS`).
    ///
    /// A field of its own rather than a read of `args` below: while the model is still *writing* the
    /// call the row is already on screen and the arguments are not in yet, and this is what lets the
    /// row name the file from its very first fragment.
    pub(super) subject: Option<String>,
    /// Call arguments (a JSON string). **Not shown in the UI** -- the row keeps only the
    /// tool name, see `step_row`; the field is kept so that when debugging you can line it
    /// up with the matching row in `history/*.jsonl` at a glance.
    #[allow(dead_code)]
    pub(super) args: String,
    pub(super) result: Option<(bool, String)>,
    /// `(added, removed)` lines when this call edited a text file (see `ToolOutput::diff`), drawn as
    /// "+N / −N" after the tool name. `None` on every non-edit call — and on edits restored from an
    /// old `history/*.jsonl`, which never carried the number.
    pub(super) diff: Option<(usize, usize)>,
    /// The edit itself, one line per entry, each prefixed with `+`, `-` or a space (see
    /// `ToolOutput::diff_detail`). This is what the row unfolds into; `None` wherever `diff` is.
    pub(super) diff_detail: Option<String>,
    /// Whether the row is unfolded. Clicking it flips this — possible only when there is a
    /// `diff_detail` to unfold, which is also exactly when the row shows its caret.
    pub(super) expanded: bool,
    /// Images this call handed to the model (`attach_file`, or a plugin's `attach` declaration).
    ///
    /// Stored on the call, not as a separate transcript entry: the picture answers "what did this
    /// step produce", so it belongs directly under this row. The old separate `Images` item put
    /// every tool picture at the **top** of the conversation, detached from the call that made it.
    pub(super) images: Vec<PathBuf>,
}

/// Compare two local paths the way Windows does: separators and letter case are not identity.
pub(super) fn same_path(a: &Path, b: &Path) -> bool {
    fn norm(p: &Path) -> String {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    }
    norm(a) == norm(b)
}

/// The `path` argument of a call, when it has one and the arguments can still be parsed.
pub(super) fn call_path(st: &StepView) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(&st.args).ok()?;
    v.get("path").and_then(|p| p.as_str()).map(PathBuf::from)
}

/// Put one tool's images under the call that produced them.
///
/// First choice is the call whose `path` argument **is** this file (`attach_file`'s exact case;
/// this also keeps two `attach_file` calls in one response from swapping pictures). A plugin's
/// attachments carry no path in the call arguments, so they fall back to the latest call of the
/// same name. Appending rather than replacing matters: one call may attach several files.
pub(super) fn attach_tool_images(steps: &mut [StepView], tool: &str, paths: &[PathBuf]) {
    for path in paths {
        let exact = steps
            .iter()
            .rposition(|s| s.name == tool && call_path(s).is_some_and(|p| same_path(&p, path)));
        let fallback = || {
            steps
                .iter()
                .rposition(|s| s.name == tool && s.result.is_some())
                .or_else(|| steps.iter().rposition(|s| s.name == tool))
                .or_else(|| steps.len().checked_sub(1))
        };
        let Some(i) = exact.or_else(fallback) else { continue };
        if !steps[i].images.iter().any(|p| same_path(p, path)) {
            steps[i].images.push(path.clone());
        }
    }
}

/// Find the call that produced a file when replaying history.
///
/// History has no call id and no per-event owner, so the matching is deliberately conservative:
/// exact `path` argument first, then the most recent `attach_file`, then the last completed call
/// (a plugin's `attach` has no path to match). Paths that cannot be matched still land on the last
/// call rather than at the top of the conversation.
pub(super) fn attach_history_images(p: &mut Process, paths: &[PathBuf]) {
    for path in paths {
        // `attach_file` first: a batch may also contain a `write_file` to the same path, and that
        // call did not produce this attachment. A path match on another tool (a plugin that takes
        // `path` arguments) is the second-best evidence.
        let exact = p
            .steps
            .iter()
            .rposition(|s| {
                s.name == "attach_file" && call_path(s).is_some_and(|q| same_path(&q, path))
            })
            .or_else(|| {
                p.steps
                    .iter()
                    .rposition(|s| call_path(s).is_some_and(|q| same_path(&q, path)))
            });
        let fallback = || {
            p.steps
                .iter()
                .rposition(|s| s.name == "attach_file" && s.result.is_some())
                .or_else(|| p.steps.iter().rposition(|s| s.result.is_some()))
                .or_else(|| p.steps.len().checked_sub(1))
        };
        let Some(i) = exact.or_else(fallback) else { continue };
        if !p.steps[i].images.iter().any(|q| same_path(q, path)) {
            p.steps[i].images.push(path.clone());
        }
    }
}

/// A tool call the model is **still writing** — its arguments are arriving in fragments.
///
/// This is what keeps a call from being invisible until it is over. The loop only learns about tool
/// calls once the whole response has landed, and by then a `write_file` is usually done, so on its
/// own the conversation sits still through the longest part of the job and then shows a finished
/// row. A `StreamCall` is opened on the first fragment (`StreamSink::on_tool_call`), which puts the
/// row and its waiting marquee on screen straight away, and carries the fragments in `args` so the
/// reading next to the name can grow while the model writes.
///
/// `step` is the row's slot in `Live::steps`. `usize::MAX` means "the row is not drawn yet" — a
/// provider is free to send arguments before it sends the name, and a row with no name has nothing
/// to show.
pub(super) struct StreamCall {
    pub(super) index: u64,
    pub(super) name: String,
    pub(super) args: String,
    pub(super) step: usize,
}

/// The "process" of one turn (collapsed into a single row once it ends).
pub(super) struct Process {
    pub(super) steps: Vec<StepView>,
    pub(super) interim: Vec<String>,
    pub(super) reasoning: String,
    /// Elapsed time. History files carry no timing information, so `None` means "don't
    /// show a duration".
    pub(super) elapsed: Option<f32>,
    pub(super) expanded: bool,
    pub(super) aborted: bool,
}

/// One entry in the transcript.
pub(super) enum Item {
    /// What the user said + the **display names** of the attachments on that message.
    ///
    /// Names only, no bytes: what the bubble draws is a row of small chips (`name.ext`),
    /// not the images themselves; the real attachment content is nothing but a path
    /// reference in the `.jsonl`.
    User(String, Vec<String>),
    Assistant(String),
    Process(Process),
    Note(NoteKind, String),
    /// Images attached to a **user's own message**, drawn as its own block below the bubble.
    ///
    /// Tool-produced images do **not** live here any more: they belong to the call that made them
    /// (`StepView::images`), so each picture appears under its own `attach_file` row instead of in
    /// a detached block above it. This variant is still the right shape for a user's drag-and-drop
    /// attachments, whose owner is the message bubble itself.
    Images(Vec<PathBuf>),
    /// One turn's token breakdown. It sits right under that turn's AI reply -- the bill
    /// belongs next to where it was spent, not piled into a top bar for the user to match
    /// up with turns by themselves.
    Usage(String),
    /// Error card (unrecoverable)
    Error(String),
}

/// A turn in progress.
pub(super) struct Live {
    pub(super) status: String,
    /// The tool call running right now, if any — its own clock and the two numbers published for it.
    ///
    /// The header used to show only the **turn** clock, which is why "Running bash 471.5s" told the
    /// user nothing they could act on: 471 s of a turn may be 40 s of thinking plus a 430 s command,
    /// and there was no way to see either the call's own time or its ceiling.
    pub(super) call: Option<RunningCall>,
    pub(super) reasoning: String,
    pub(super) reasoning_live: bool,
    pub(super) steps: Vec<StepView>,
    pub(super) interim: Vec<String>,
    /// How many call steps had **already run** when `interim[i]` was spoken -- used to
    /// split the live area into alternating segments "call stream -> interim reply ->
    /// call stream -> ..." (user request, 2026-10-05: previously all the calls were crammed
    /// at the top and all the replies in the middle, so you couldn't tell which steps
    /// belonged to which stage).
    ///
    /// We store "cut points" rather than replacing `steps` / `interim` with
    /// `Vec<Segment>`: at the end of a turn these two have to be handed to the process card
    /// as-is, and keeping them flat means that commit path doesn't have to change.
    pub(super) cuts: Vec<usize>,
    pub(super) pending_final: Option<String>,
    /// **The body text being typed** (the sentence that isn't finished yet at this
    /// instant).
    ///
    /// Division of labour with `pending_final`: `pending_final` is "the complete body text
    /// this response has settled on", which only turns into a message at the end of a turn;
    /// `streaming` is what makes it **visible** before it hits disk (user request,
    /// 2026-10-05: the reply only popped in all at once when everything was finished, and
    /// until then the conversation was blank).
    /// The two are reconciled when `AgentEvent::AssistantText` / `InterimText` arrive --
    /// by then the deltas are done, so the authoritative text overwrites once and a few
    /// dropped frames can't leave the screen a few characters off.
    pub(super) streaming: String,
    /// The activity clock's reading when this turn started. The live "12.3s" readouts subtract it
    /// from the clock's current reading, so they show the turn's **working** time — the turn
    /// doesn't look like it took four minutes just because the user pondered a question for that
    /// long (see [`Clock`]).
    pub(super) turn_base: f32,
    /// Characters received on the output side (reasoning + body), used to scroll a live
    /// token count that keeps climbing.
    ///
    /// During streaming the server hasn't reported yet, so this can only be an estimate;
    /// at the end of a turn the exact value from `TurnUsage` replaces it.
    pub(super) out: TokenEstimate,
}

/// The call the live row is reporting on right now.
///
/// Three numbers, and each answers a different question: `started` = "how long has it been going",
/// `cap` = "when will the host give up on it", `expect` = "how long did the model say it would take".
/// Without all three, a long call is indistinguishable from a wedged one.
pub(super) struct RunningCall {
    /// Activity-clock reading when `ToolRunning` arrived.
    pub(super) started: f32,
    /// This call's ceiling, in seconds (the shell tools compute it per call).
    pub(super) cap: u64,
    /// The model's own estimate, when it chose to give one.
    pub(super) expect: Option<u64>,
}

pub(super) struct AuthCard {
    pub(super) tool: String,
    pub(super) args: String,
}/// A multiple-choice question waiting on the user (`ask_user`). The question and the

/// options are the model's own words.
///
/// `Clone` is derived because the answer row needs `&mut self.choice_text` while the same
/// frame still has to read the question and the options -- cloning something small (one
/// sentence plus at most 6 short strings) beats gymnastics with borrows.
#[derive(Clone)]
pub(super) struct ChoiceCard {
    pub(super) question: String,
    pub(super) options: Vec<String>,
}

/// Which stage an image is currently in.
///
/// The three stages each get a variant rather than one `Option<TextureHandle>` plus a
/// separate error table: the UI already draws all three differently (the image /
/// "decoding..." / a one-line failure reason), and merging them would only make the
/// renderer split them apart again.
pub(super) enum Tex {
    /// A background thread is decoding
    Loading(std::sync::mpsc::Receiver<Result<aox::image::Bitmap, String>>),
    Ready(egui::TextureHandle),
    Failed(String),
}

/// The columns down the left of the settings dialog. **Declaration order is the order
/// they appear in the list, top to bottom.**
///
/// "Extra" comes first because what it holds isn't a setting (it's an entrance), and the
/// further from the middle it sits the less it gets in the way of someone actually
/// looking for settings -- but the user explicitly asked for it first, so first it is; we
/// don't reorder on their behalf.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SettingsTab {
    /// Extra: things like the activity-stream entrance that "belong to no setting"
    Extra,
    /// Model: endpoint, key, model list, window, thinking strength.
    ///
    /// Second, right after "Extra": it is the one page a **fresh install** has to visit before
    /// anything else can happen, and hunting for it under "System" was the friction this page exists
    /// to remove.
    Model,
    /// Appearance: light/dark / follow system
    Appearance,
    /// System: paths and runtime diagnostics
    System,
    /// About: version, license, links
    About,
}

impl SettingsTab {
    pub(super) const ALL: [SettingsTab; 5] = [
        SettingsTab::Extra,
        SettingsTab::Model,
        SettingsTab::Appearance,
        SettingsTab::System,
        SettingsTab::About,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            SettingsTab::Extra => aox::i18n::t("Extra"),
            SettingsTab::Model => aox::i18n::t("Model"),
            SettingsTab::Appearance => aox::i18n::t("Appearance"),
            SettingsTab::System => aox::i18n::t("System"),
            SettingsTab::About => aox::i18n::t("About"),
        }
    }
}

/// The activity clock: seconds of *work* since launch, with the stretches where nothing was
/// running **cut out**.
///
/// Everything timed in this program is timed against this clock rather than the wall, because the
/// wall is the wrong unit for "how long did this take". Two stretches must not count:
///
/// * **Idle** — after a turn ends and before the next instruction. The user reading a reply or
///   typing has nothing to do with how long the work took, and (worse) it is usually minutes, so
///   leaving it in stretched one round away from the next and buried the actual work in a mostly
///   empty timeline. Cutting it is also what makes consecutive rounds sit flush: the next round's
///   first record lands exactly where the previous round's last one stopped.
/// * **Waiting on the user** — an authorization prompt or an `ask_user` question. While the
///   question is on screen the model is blocked on a person, not working, so that time belongs to
///   neither side's duration. This is why a "waiting for your choice" bar no longer reads as if
///   the tool ran for four minutes.
///
/// Summing instead of subtracting a start point (`Instant::now()` would give only the running
/// stretch, losing every earlier one) is what makes it survive many pause/resume cycles: each
/// stopped stretch is folded into `paused` once, on the way out.
pub(super) struct Clock {
    pub(super) started: Instant,
    /// Seconds already accumulated as "not running"
    pub(super) paused: f32,
    /// When the current pause began (`None` = running right now)
    pub(super) since: Option<Instant>,
}

impl Clock {
    pub(super) fn new() -> Self {
        Self { started: Instant::now(), paused: 0.0, since: None }
    }

    pub(super) fn now(&self) -> f32 {
        let paused = self.paused + self.since.map_or(0.0, |t: Instant| t.elapsed().as_secs_f32());
        (self.started.elapsed().as_secs_f32() - paused).max(0.0)
    }

    /// Run or stop the clock. Idempotent, so it is safe to call every frame with a derived
    /// condition: only the *change* folds time into `paused` (and `set_running` is the one place
    /// that does, so no stretch can be counted twice or lost).
    pub(super) fn set_running(&mut self, run: bool) {
        match (run, self.since) {
            (false, None) => self.since = Some(Instant::now()),
            (true, Some(t)) => {
                self.paused += t.elapsed().as_secs_f32();
                self.since = None;
            }
            _ => {}
        }
    }
}

// ─────────────────────────── App ───────────────────────────

/// The host's file-writing tool, by name.
///
/// The GUI needs it to know which call is carrying text it can count line by line
/// (`StreamCall::args` → `aox::llm::partial_json_lines`). It is written out here because the core's
/// table (`tool::HOST_FILE_TOOLS`) is `pub(crate)` and unreachable from this crate — same reason
/// `plugin_of` spells out the three `meta` tool names.
pub(super) const WRITE_FILE_TOOL: &str = "write_file";

/// Tools whose row **names the file it works on**.
///
/// The two file writers — `write_file` (whole-file) and `edit_file` (in-place snippets) — both take a
/// `path` and both produce a diff, so both rows should say which file they are about. Adding a read or
/// a delete here is a one-line change the day the same is wanted for them; nothing else keys off this
/// list.
pub(super) const SUBJECT_TOOLS: &[&str] = &[WRITE_FILE_TOOL, "edit_file"];

/// Height cap of the unfolded diff panel. A peephole into the edit, not a second editor — the point
/// is "which lines moved", and the panel scrolls for the rest.
///
/// **Not draggable** (the grab strip was built and then dropped, user decision 2026-10-11: "forget
/// the resizing, just double the height"): a fixed cap is predictable, and the panel still shrinks to
/// fit a short diff, so a two-line edit is not buried in empty background.
pub(super) const DIFF_PANEL_H: f32 = 480.0;

/// Longest path shown on a call row before it is shortened from the middle. The tail is the half
/// that identifies a file, so a shortened path keeps it.
pub(super) const SUBJECT_MAX: usize = 56;

/// The file a call is about, straight off its **parsed** arguments.
pub(super) fn subject_from_value(name: &str, args: &serde_json::Value) -> Option<String> {
    if !SUBJECT_TOOLS.contains(&name) {
        return None;
    }
    args.get("path").and_then(|v| v.as_str()).map(str::to_string)
}

/// The same, from the arguments as text (a row rebuilt from `history/*.jsonl`, which keeps them as
/// the model wrote them).
pub(super) fn subject_from_json(name: &str, args: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    subject_from_value(name, &v)
}

/// Longest `purpose` shown on a call row. Long enough for "fix the 471 s shell hang", short enough
/// that the row still fits beside the tool name and the file path.
pub(super) const PURPOSE_MAX: usize = 48;

/// What a call is **for**, read off its arguments: `(purpose, subject)`.
///
/// | tool | purpose | subject |
/// |------|---------|---------|
/// | anything with a `purpose` argument | the model's words | — |
/// | `bash` / `shell` | the **command**, elided at the tail | — |
/// | `write_file` / `edit_file` | the model's words, if any | the file path |
///
/// The command is used as a *fallback*, never paraphrased: `bash` + `cargo build --release` tells the
/// user more than any four-word summary could, and it cannot be wrong. A purpose the model *did* write
/// wins, because "why" is the thing the command line does not say.
pub(super) fn purpose_from_value(name: &str, args: &serde_json::Value) -> Option<String> {
    if let Some(p) = args.get(aox::tool::PURPOSE_ARG).and_then(|v| v.as_str()) {
        let p = flat_line(p);
        if !p.is_empty() {
            return Some(clip_head(&p, PURPOSE_MAX));
        }
    }
    if matches!(name, "shell" | "bash") {
        let cmd = flat_line(args.get("command").and_then(|v| v.as_str())?);
        return (!cmd.is_empty()).then(|| clip_head(&cmd, PURPOSE_MAX));
    }
    None
}

/// The same, from the arguments as text (a row rebuilt from `history/*.jsonl`).
pub(super) fn purpose_from_json(name: &str, args: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    purpose_from_value(name, &v)
}

/// Collapse a multi-line script into one line: a `bash` row must stay one row.
pub(super) fn flat_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut at the **tail** — unlike [`clip_path`], the beginning of a command is the identifying half
/// (the program being run), so the ellipsis belongs at the end.
pub(super) fn clip_head(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Shorten a path from the middle: `keep/both/ends.rs`. See `SUBJECT_MAX`.
pub(super) fn clip_path(s: &str) -> String {
    let n = s.chars().count();
    if n <= SUBJECT_MAX {
        return s.to_string();
    }
    let keep_head = SUBJECT_MAX / 4;
    let keep_tail = SUBJECT_MAX - keep_head - 1;
    let head: String = s.chars().take(keep_head).collect();
    let tail: String = s.chars().skip(n - keep_tail).collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a call row says about the call — the three shapes it can arrive in, and the one case
    /// where it must stay silent.
    #[test]
    fn a_row_says_what_the_call_is_for() {
        use serde_json::json;

        // The model's own words win: the command line cannot say *why*.
        assert_eq!(
            purpose_from_value("edit_file", &json!({"path": "a.rs", "purpose": "fix the 471 s hang"})),
            Some("fix the 471 s hang".into())
        );
        // `bash` has no purpose of its own here, so the command is the label — real, not paraphrased.
        assert_eq!(
            purpose_from_value("bash", &json!({"command": "cargo build --release"})),
            Some("cargo build --release".into())
        );
        // A multi-line script must not turn one row into five.
        assert_eq!(
            purpose_from_value("shell", &json!({"command": "cd a\n  && cargo test\n"})),
            Some("cd a && cargo test".into())
        );
        // A long command keeps its **head** (the program identifies it) and gets one ellipsis.
        let long = purpose_from_value("bash", &json!({"command": "x".repeat(200)})).unwrap();
        assert!(long.ends_with('…'), "{long}");
        assert_eq!(long.chars().count(), PURPOSE_MAX);
        // Nothing to say, and nothing invented.
        assert_eq!(purpose_from_value("read_file", &json!({"path": "a.rs"})), None);
        // Whitespace-only purpose is not a purpose.
        assert_eq!(purpose_from_value("bash", &json!({"purpose": "   ", "command": "dir"})), Some("dir".into()));
    }

    /// Live events carry only the tool **name**, so the path argument is what keeps two calls of
    /// the same tool from swapping pictures. A plugin has no path argument; it still lands on the
    /// most recent call of that tool rather than being dropped.
    #[test]
    fn live_tool_images_land_on_the_matching_call() {
        use serde_json::json;

        let step = |name: &str, args: serde_json::Value| StepView {
            name: name.into(),
            plugin: None,
            purpose: None,
            subject: None,
            args: args.to_string(),
            result: Some((false, "ok".into())),
            diff: None,
            diff_detail: None,
            expanded: false,
            images: Vec::new(),
        };
        let (a, b) = (PathBuf::from(r"C:\shots\a.png"), PathBuf::from(r"C:\shots\b.png"));
        let mut calls = vec![
            step("attach_file", json!({"path": a.to_string_lossy()})),
            step("attach_file", json!({"path": b.to_string_lossy()})),
            // A plugin's result has no `path` argument to match.
            step("probe_screen", json!({})),
        ];

        attach_tool_images(&mut calls, "attach_file", &[b.clone(), a.clone()]);
        assert_eq!(calls[0].images, vec![a], "exact path wins over call order");
        assert_eq!(calls[1].images, vec![b], "the second call keeps its own picture");

        attach_tool_images(&mut calls, "probe_screen", &[PathBuf::from(r"C:\shots\c.png")]);
        assert_eq!(calls[2].images.len(), 1, "a plugin image still attaches to its own tool row");
        assert_eq!(calls[0].images.len(), 1, "other calls stay untouched");
    }
}
