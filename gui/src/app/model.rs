//! The data model: what a conversation looks like to the window, and the small derivations on it.
//! These are the types every screen passes around -- a transcript is a `Vec<Item>`, a turn in flight is a `Live`...
//! Fields are `pub(super)` rather than private: Rust's privacy stops at the module boundary, so a type declared...

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
    /// What this call is for, in a few words — the model's own `purpose` argument when it gave one, otherwise...
    pub(super) purpose: Option<String>,
    /// The file this call is *about*, when naming it is worth a glance — `write_file`'s `path`, read off...
    pub(super) subject: Option<String>,
    /// Call arguments (a JSON string).
    #[allow(dead_code)]
    pub(super) args: String,
    pub(super) result: Option<(bool, String)>,
    /// `(added, removed)` lines when this call edited a text file (see `ToolOutput::diff`), drawn as "+N / −N" after...
    pub(super) diff: Option<(usize, usize)>,
    /// The edit itself, one line per entry, each prefixed with `+`, `-` or a space (see
    /// `ToolOutput::diff_detail`). This is what the row unfolds into; `None` wherever `diff` is.
    pub(super) diff_detail: Option<String>,
    /// Whether the row is unfolded. Clicking it flips this — possible only when there is a
    /// `diff_detail` to unfold, which is also exactly when the row shows its caret.
    pub(super) expanded: bool,
    /// Images this call handed to the model (`attach_file`, or a plugin's `attach` declaration).
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
pub(super) fn attach_history_images(p: &mut Process, paths: &[PathBuf]) {
    for path in paths {
        // `attach_file` first: a batch may also contain a `write_file` to the same path, and that call did not produce...
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

/// A tool call the model is still writing — its arguments are arriving in fragments.
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
    /// What the user said + the display names of the attachments on that message.
    User(String, Vec<String>),
    Assistant(String),
    Process(Process),
    Note(NoteKind, String),
    /// Images attached to a user's own message, drawn as its own block below the bubble.
    Images(Vec<PathBuf>),
    /// One turn's token breakdown.
    Usage(String),
    /// Error card (unrecoverable)
    Error(String),
}

/// A turn in progress.
pub(super) struct Live {
    pub(super) status: String,
    /// The tool call running right now, if any — its own clock and the two numbers published for it.
    pub(super) call: Option<RunningCall>,
    pub(super) reasoning: String,
    pub(super) reasoning_live: bool,
    pub(super) steps: Vec<StepView>,
    pub(super) interim: Vec<String>,
    /// How many call steps had already run when `interimi` was spoken -- used to split the live area into alternating...
    pub(super) cuts: Vec<usize>,
    pub(super) pending_final: Option<String>,
    /// The body text being typed (the sentence that isn't finished yet at this instant).
    pub(super) streaming: String,
    /// The activity clock's reading when this turn started.
    pub(super) turn_base: f32,
    /// Characters received on the output side (reasoning + body), used to scroll a live token count that keeps...
    pub(super) out: TokenEstimate,
}

/// The call the live row is reporting on right now.
pub(super) struct RunningCall {
    /// Activity-clock reading when `ToolRunning` arrived
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

/// Options are the model's own words.
#[derive(Clone)]
pub(super) struct ChoiceCard {
    pub(super) question: String,
    pub(super) options: Vec<String>,
}

/// Which stage an image is currently in.
pub(super) enum Tex {
    /// A background thread is decoding
    Loading(std::sync::mpsc::Receiver<Result<aox::image::Bitmap, String>>),
    Ready(egui::TextureHandle),
    Failed(String),
}

/// The columns down the left of the settings dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SettingsTab {
    /// Extra: things like the activity-stream entrance that "belong to no setting"
    Extra,
    /// Model: endpoint, key, model list, window, thinking strength.
    Model,
    /// Appearance: light/dark / follow system
    Appearance,
    /// System: paths and runtime diagnostics
    System,
    /// Remote tab: pairing, start/stop, logs; placed before About so settings-type tabs stay up front.
    Remote,
    /// About: version, license, links
    About,
}

impl SettingsTab {
    pub(super) const ALL: [SettingsTab; 6] = [
        SettingsTab::Extra,
        SettingsTab::Model,
        SettingsTab::Appearance,
        SettingsTab::System,
        SettingsTab::Remote,
        SettingsTab::About,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            SettingsTab::Extra => aox::i18n::t("Extra"),
            SettingsTab::Model => aox::i18n::t("Model"),
            SettingsTab::Appearance => aox::i18n::t("Appearance"),
            SettingsTab::System => aox::i18n::t("System"),
            SettingsTab::Remote => aox::i18n::t("Remote"),
            SettingsTab::About => aox::i18n::t("About"),
        }
    }
}

/// The activity clock: seconds of *work* since launch, with the stretches where nothing was running cut out.
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

    /// Run or stop the clock. Idempotent, so it is safe to call every frame with a derived condition: only...
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

// App

/// The host's file-writing tool, by name.
pub(super) const WRITE_FILE_TOOL: &str = "write_file";

/// Tools whose row names the file it works on.
pub(super) const SUBJECT_TOOLS: &[&str] = &[WRITE_FILE_TOOL, "edit_file", "publish_file"];

/// Height cap of the unfolded diff panel.
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

/// What a call is for, read off its arguments: `(purpose, subject)`.
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

        // The model's own words win: the command line cannot say *why
        assert_eq!(
            purpose_from_value("edit_file", &json!({"path": "a.rs", "purpose": "fix the 471 s hang"})),
            Some("fix the 471 s hang".into())
        );
        // `bash` has no purpose of its own here, so the command is the label — real, not paraphrased.
        assert_eq!(
            purpose_from_value("bash", &json!({"command": "cargo build --release"})),
            Some("cargo build --release".into())
        );
        // A multi-line script must not turn one row into five
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
        // Whitespace-only purpose is not a purpose
        assert_eq!(purpose_from_value("bash", &json!({"purpose": "   ", "command": "dir"})), Some("dir".into()));
    }

    /// Which calls name their file. `publish_file` is the odd one: it writes nothing, but "which
    /// file went to the user" is the only fact its row can carry, so it must be here.
    #[test]
    fn only_the_calls_that_are_about_a_file_name_one() {
        use serde_json::json;

        assert_eq!(
            subject_from_value("publish_file", &json!({"path": r"C:\out\aoxchat.apk"})),
            Some(r"C:\out\aoxchat.apk".into())
        );
        assert_eq!(
            subject_from_value("write_file", &json!({"path": "a.rs", "content": "x"})),
            Some("a.rs".into())
        );
        // A read is not about producing the file, so its row stays bare.
        assert_eq!(subject_from_value("read_file", &json!({"path": "a.rs"})), None);
        // No `path` argument at all — nothing to name, nothing invented.
        assert_eq!(subject_from_value("publish_file", &json!({})), None);
    }

    /// Live events carry only the tool name, so the path argument is what keeps two calls of the same tool...
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
