use std::io::Write as _;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};

use super::spinner::{self, Spinner, Status};
use super::style as s;
use super::{AgentEvent, AuthDecision, ChoiceReply, Ui};
use crate::history::UsageRecord;
use crate::llm::{thousands, StreamSink};

/// Feeds the model's reasoning process and body-text deltas live into shared state, for the waiting
/// animation to scroll back on screen.
///
/// It doesn't paint the screen directly: rendering is the spinner background task's job (one frame
/// every 80ms), so even high-frequency Provider callbacks just append strings to a buffer and never
/// slow down the streaming read.
struct ReasoningSink {
    status: Arc<Status>,
}

impl StreamSink for ReasoningSink {
    fn on_reasoning(&self, delta: &str) {
        self.status.push_reasoning(delta);
    }

    /// The content is also fed in, solely to count tokens — the content itself is kept and rendered by `TurnLog`.
    fn on_content(&self, delta: &str) {
        self.status.push_content(delta);
    }

    fn on_finish(&self) {
        // One response ended → the reasoning process vanishes immediately (the token counter stays,
        // since a turn makes several requests)
        self.status.clear_reasoning();
    }
}

/// A record of one tool call (used for folding / expanding)
struct Step {
    name: String,
    args: String,
    /// (is_error, result summary)
    result: Option<(bool, String)>,
}

/// The complete process of one turn (one user instruction → model + tool loop).
/// While it runs only one status line shows in the terminal; once done it folds into a one-line
/// summary and expands on demand.
struct TurnLog {
    steps: Vec<Step>,
    /// The assistant's interim narration (not shown on its own, visible when expanded)
    interim: Vec<String>,
    /// The info / warning / error notes along the way
    notes: Vec<(char, String)>,
    /// The final output: the last assistant text without tool calls
    final_text: Option<String>,
    started: Instant,
}

impl TurnLog {
    fn new() -> Self {
        Self {
            steps: Vec::new(),
            interim: Vec::new(),
            notes: Vec::new(),
            final_text: None,
            started: Instant::now(),
        }
    }

    fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

/// The command-line interface implementation
pub struct CliUi {
    lines: Lines<BufReader<tokio::io::Stdin>>,
    spinner: Option<Spinner>,
    status: Option<Arc<Status>>,
    /// The turn in progress
    turn: Option<TurnLog>,
    /// The most recently finished turn, for `/expand` to unfold
    last: Option<TurnLog>,
}

impl CliUi {
    pub fn new() -> Self {
        Self {
            lines: BufReader::new(tokio::io::stdin()).lines(),
            spinner: None,
            status: None,
            turn: None,
            last: None,
        }
    }

    async fn read_line(&mut self) -> Result<Option<String>> {
        std::io::stdout().flush()?;
        Ok(self.lines.next_line().await?)
    }

    fn set_label(&self, label: &str) {
        if let Some(st) = &self.status {
            st.set(label);
        }
    }

    async fn start_spinner(&mut self) {
        if self.spinner.is_some() || !spinner::supported() {
            return;
        }
        if let Some(st) = self.status.clone() {
            self.spinner = Some(Spinner::start(st));
        }
    }

    async fn stop_spinner(&mut self) {
        if let Some(mut sp) = self.spinner.take() {
            sp.stop().await;
        }
    }

    /// Interrupt the current turn: stop the animation and discard the half-record (it is not fed
    /// back to `/expand`).
    pub async fn abort_turn(&mut self) {
        self.stop_spinner().await;
        self.status = None;
        self.turn = None;
    }

    /// Expand the most recently folded turn's process.
    pub fn expand_last_turn(&self) {
        let Some(t) = self.last.as_ref() else {
            println!("{} {}", s::yellow("!"), s::dim(crate::i18n::t("Nothing to expand yet (no task has finished)")));
            return;
        };
        println!(
            "{}",
            s::dim(&crate::tf!("──────────────── last task process · {} steps · {}s ────────────────", t.steps.len(), format!("{:.1}", t.elapsed())))
        );
        for (i, text) in t.interim.iter().enumerate() {
            println!("{}", s::dim(&crate::tf!("[interim note {}]", i + 1)));
            println!("{}", s::render_markdown(text));
        }
        for (i, st) in t.steps.iter().enumerate() {
            println!(
                "{:>3}. {} {} {}",
                i + 1,
                s::yellow("→"),
                s::bold_white(&st.name),
                s::dim(&one_line(&st.args, 160))
            );
            match &st.result {
                Some((false, p)) => println!("     {} {}", s::green("✓"), s::dim(&one_line(p, 200))),
                Some((true, p)) => println!("     {} {}", s::red("✗"), s::red(&one_line(p, 200))),
                None => println!("     {}", s::dim(crate::i18n::t("(unfinished)"))),
            }
        }
        if let Some(f) = &t.final_text {
            println!("{}", s::dim(crate::i18n::t("Final output:")));
            println!("{}", s::render_markdown(f));
        }
        println!("{}", s::dim("────────────────────────────────────────────────────────────"));
    }

    /// Look back at the **current conversation's per-turn** token account (`/usage`).
    ///
    /// The data comes from the ledger the agent holds — the very copy read back from the conversation
    /// file, so it survives switching conversations and restarting the program. Listing it per turn
    /// matters: a bare total can't answer "which turn was the most expensive", and the most expensive
    /// turn is usually exactly the one that went wrong.
    pub fn show_usage(&self, log: &[UsageRecord]) {
        let rows: Vec<&UsageRecord> = log.iter().filter(|r| !r.is_empty()).collect();
        if rows.is_empty() {
            println!(
                "{} {}",
                s::yellow("!"),
                s::dim(crate::i18n::t("This conversation has no usage records yet (no task has finished, or the endpoint does not return usage)"))
            );
            return;
        }
        let calls: u32 = rows.iter().map(|r| r.calls).sum();
        println!();
        println!(
            "{}",
            s::bold(&crate::tf!("token usage · {} turns in this conversation · {} model calls", rows.len(), calls))
        );
        if let Some(v) = rows.last().and_then(|r| r.session_total) {
            println!("  {}  {}", s::dim(crate::i18n::t("Total")), s::bold_cyan(&thousands(v)));
        }
        println!();
        for r in &rows {
            let mut line = vec![
                s::dim(&format!("#{:<3}", r.round)),
                // Pad the number to width *before* colouring: ANSI escape codes count as characters,
                // so width-padding an already-coloured string misaligns
                s::bold_white(&format!("{:>9}", thousands(r.usage.total.unwrap_or(0)))),
                s::dim(&crate::tf!("{} calls", r.calls)),
            ];
            if let Some(v) = r.usage.prompt {
                line.push(s::dim(&crate::tf!("input {}", thousands(v))));
            }
            if let Some(v) = r.usage.completion {
                line.push(s::dim(&crate::tf!("output {}", thousands(v))));
            }
            if let Some(v) = r.usage.reasoning {
                line.push(s::dim(&crate::tf!("reasoning {}", thousands(v))));
            }
            if let Some(v) = r.usage.cached {
                line.push(s::green(&crate::tf!("cache hit {}", thousands(v))));
            }
            println!("  {}", line.join(&s::dim("  ·  ")));
        }
        if let Some(v) = rows.last().and_then(|r| r.last_prompt) {
            println!();
            println!("  {} {}", s::dim(crate::i18n::t("current context")), s::bold_white(&thousands(v)));
        }
        println!();
    }

    /// Print the token breakdown for one finished turn.
    ///
    /// Splitting into three lines is a deliberate trade-off: the first holds the input / output / total
    /// everyone looks at, the second the sub-items (reasoning, cache hits), and the third the call count
    /// and context size. Items the endpoint didn't report simply don't appear — better to drop a line than
    /// to fake "zero consumption" with a 0.
    fn print_usage(r: &UsageRecord) {
        println!();
        println!("{}", s::dim(&crate::tf!("  token usage · round {}", r.round)));

        let mut row1: Vec<String> = Vec::new();
        if let Some(v) = r.usage.prompt {
            row1.push(format!("{} {}", s::dim(crate::i18n::t("input")), s::bold_white(&thousands(v))));
        }
        if let Some(v) = r.usage.completion {
            row1.push(format!("{} {}", s::dim(crate::i18n::t("output")), s::bold_white(&thousands(v))));
        }
        if let Some(v) = r.usage.total {
            row1.push(format!("{} {}", s::dim(crate::i18n::t("Total")), s::bold_cyan(&thousands(v))));
        }
        if !row1.is_empty() {
            println!("    {}", row1.join(&s::dim("  ·  ")));
        }

        let mut row2: Vec<String> = Vec::new();
        if let Some(v) = r.usage.reasoning {
            row2.push(format!("{} {}", s::dim(crate::i18n::t("of which reasoning")), s::bold_white(&thousands(v))));
        }
        if let Some(v) = r.usage.cached {
            row2.push(format!("{} {}", s::dim(crate::i18n::t("cache hit")), s::green(&thousands(v))));
        }
        if let Some(v) = r.usage.cache_miss {
            row2.push(format!("{} {}", s::dim(crate::i18n::t("cache miss")), s::dim(&thousands(v))));
        }
        if !row2.is_empty() {
            println!("    {}", row2.join(&s::dim("  ·  ")));
        }

        let mut row3: Vec<String> =
            vec![format!("{} {}", s::dim(crate::i18n::t("model calls")), s::bold_white(&crate::tf!("{} times", r.calls)))];
        if let Some(v) = r.last_prompt {
            row3.push(format!("{} {}", s::dim(crate::i18n::t("last input")), s::dim(&thousands(v))));
        }
        if let Some(v) = r.session_total {
            row3.push(format!(
                "{} {}",
                s::dim(crate::i18n::t("this conversation total")),
                s::dim(&crate::tf!("{} ({} calls)", thousands(v), r.session_calls))
            ));
        }
        println!("    {}", row3.join(&s::dim("  ·  ")));
        println!();
    }

    /// Events while a turn is running: folded into the buffer rather than splashed line by line in
    /// the terminal.
    fn emit_into_turn(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AssistantText(text) => {
                // Only "a reply without tool calls" reaches here, and that is the final output
                if let Some(t) = self.turn.as_mut() {
                    if let Some(prev) = t.final_text.replace(text) {
                        t.interim.push(prev);
                    }
                }
            }
            AgentEvent::InterimText(text) => {
                // The narration accompanying tool calls (e.g. "let me see which plugins there are") — folded into the process area
                if let Some(t) = self.turn.as_mut() {
                    t.interim.push(text);
                }
            }
            AgentEvent::ToolCall { name, args } => {
                if let Some(t) = self.turn.as_mut() {
                    t.steps.push(Step { name, args: args.to_string(), result: None });
                }
            }
            AgentEvent::ToolRunning { name, .. } => {
                self.set_label(&crate::tf!("running {}", name));
            }
            AgentEvent::ToolResult { name, is_error, preview, .. } => {
                if let Some(t) = self.turn.as_mut() {
                    // Attach to the most recent step with the same name that has no result yet
                    match t.steps.iter_mut().rev().find(|s| s.name == name && s.result.is_none()) {
                        Some(st) => st.result = Some((is_error, preview)),
                        None => t.steps.push(Step {
                            name,
                            args: String::new(),
                            result: Some((is_error, preview)),
                        }),
                    }
                }
                self.set_label(crate::i18n::t("Thinking"));
            }
            AgentEvent::Info(m) => self.push_note('i', m),
            AgentEvent::Warn(m) => self.push_note('w', m),
            AgentEvent::Error(m) => self.push_note('e', m),
            // The files went to the recycle bin. A **note** here (folded into the turn summary,
            // unfolded with `/expand`) rather than a line of its own: a terminal has no "vanish
            // after a few seconds", so a permanent line per delete would end up as clutter — and
            // the tool result printed just above already names the path.
            AgentEvent::Recycled(n) => self.push_note('i', crate::recycle::notice(n)),
            // An image produced by a tool: the terminal has nowhere to draw it. **Silently dropping
            // it** is deliberate — the accompanying message already lists the filenames (see
            // `attach`'s path list), and adding a "there's an image but it can't be shown" line
            // would just be noise.
            AgentEvent::Images(_) => {}
        }
    }

    fn push_note(&mut self, kind: char, msg: String) {
        if let Some(t) = self.turn.as_mut() {
            t.notes.push((kind, msg));
        }
    }

    fn print_note(kind: char, msg: &str) {
        match kind {
            'e' => println!("{} {}", s::red("✗"), s::red(msg)),
            'w' => println!("{} {}", s::yellow("!"), s::yellow(msg)),
            _ => println!("{} {}", s::cyan("·"), s::dim(msg)),
        }
    }

    /// Print the authorisation prompt and read the user's choice (without animation control)
    async fn ask_authorization(&mut self, tool_name: &str, args: &Value) -> AuthDecision {
        let pretty = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
        let width = term_width().saturating_sub(2);
        println!();
        println!("{}", s::magenta(crate::i18n::t("┌─ authorization request ────────────────────────────────")));
        println!("{} {} {}", s::magenta("│"), s::dim(crate::i18n::t("tool")), s::bold_cyan(tool_name));
        for line in pretty.lines().take(20) {
            println!("{} {}", s::magenta("│"), s::dim(&clip(line, width)));
        }
        println!(
            "{} {}",
            s::magenta("│"),
            s::dim(crate::i18n::t("[y] allow once   [a] always allow in this session   [n] deny   (enter = y)"))
        );
        print!("{} ", s::magenta(crate::i18n::t("└─ choose >")));
        loop {
            match self.read_line().await {
                Ok(Some(line)) => {
                    let ans = line.trim().to_ascii_lowercase();
                    match ans.as_str() {
                        "" | "y" | "yes" => return AuthDecision::Allow,
                        "a" | "always" => return AuthDecision::AllowSession,
                        "n" | "no" => return AuthDecision::Deny,
                        _ => {
                            print!("{} ", s::magenta(crate::i18n::t("invalid input; please enter y / a / n >")));
                            continue;
                        }
                    }
                }
                _ => return AuthDecision::Deny,
            }
        }
    }
}

#[async_trait]
impl Ui for CliUi {
    async fn read_user_input(&mut self) -> Result<Option<String>> {
        print!("\n{} ", s::bold_cyan(crate::i18n::t("you >")));
        self.read_line().await
    }

    fn emit(&mut self, event: AgentEvent) {
        // A turn is running: the process folds into the buffer, keeping only the final output
        if self.turn.is_some() {
            self.emit_into_turn(event);
            return;
        }
        match event {
            // ── Assistant reply: rendered as Markdown (headings/code blocks/lists/tables)
            AgentEvent::AssistantText(text) | AgentEvent::InterimText(text) => {
                println!();
                println!("{}", s::render_markdown(&text));
            }
            // ── Tool call: a yellow arrow + bold tool name + dim arguments
            AgentEvent::ToolCall { name, args } => {
                println!(
                    "{} {} {}",
                    s::yellow("→"),
                    s::bold_white(&name),
                    s::dim(&crate::tf!("args {}", one_line(&args.to_string(), 200)))
                );
            }
            AgentEvent::ToolRunning { name, timeout_secs } => {
                println!(
                    "  {}",
                    s::dim(&crate::tf!("…{} running (timeout {}s, Ctrl+C to interrupt)", name, timeout_secs))
                );
            }
            // ── Tool result: a green/red marker + a one-line summary
            AgentEvent::ToolResult { name, is_error, preview, .. } => {
                let (mark, tag) = if is_error {
                    (s::red("✗"), s::red(crate::i18n::t("failed")))
                } else {
                    (s::green("✓"), s::green(crate::i18n::t("done")))
                };
                println!(
                    "{} {} {}  {}",
                    mark,
                    s::bold(&name),
                    tag,
                    s::dim(&one_line(&preview, 160))
                );
            }
            AgentEvent::Info(msg) => println!("{} {}", s::cyan("·"), s::dim(&msg)),
            AgentEvent::Warn(msg) => println!("{} {}", s::yellow("!"), s::yellow(&msg)),
            AgentEvent::Error(msg) => println!("{} {}", s::red("✗"), s::red(&msg)),
            AgentEvent::Recycled(n) => println!(
                "{} {}",
                s::cyan("·"),
                s::dim(&crate::recycle::notice(n))
            ),
            AgentEvent::Images(_) => {}
        }
    }

    async fn turn_start(&mut self) {
        self.turn = Some(TurnLog::new());
        self.status = Some(Status::new(crate::i18n::t("Thinking")));
        self.start_spinner().await;
    }

    async fn turn_end(&mut self) {
        self.stop_spinner().await;
        self.status = None;
        let Some(t) = self.turn.take() else { return };

        // The process folds into a one-line summary; `/expand` unfolds it on demand
        if !t.steps.is_empty() {
            println!(
                "{} {} {}",
                s::dim("▸"),
                s::bold(&crate::tf!("folded {} tool calls", t.steps.len())),
                s::dim(&crate::tf!("· {}s · /expand to unfold", format!("{:.1}", t.elapsed())))
            );
        }
        for (kind, msg) in &t.notes {
            Self::print_note(*kind, msg);
        }
        match t.final_text.as_deref() {
            Some(text) => {
                println!();
                println!("{}", s::render_markdown(text));
            }
            // Fallback: a normal turn always has a final reply, so this only prompts on an anomaly,
            // avoiding "it ended but showed nothing"
            None if !t.steps.is_empty() => {
                println!(
                    "{}",
                    s::yellow(crate::i18n::t("! this turn produced no final reply (the task may have ended early); type /expand to see the process"))
                );
            }
            None => {}
        }
        self.last = Some(t);
    }

    async fn turn_usage(&mut self, rec: &UsageRecord) {
        // If the endpoint doesn't report usage, the whole block is hidden — fobbing the user off with
        // a string of 0s is worse than saying nothing
        if rec.is_empty() {
            return;
        }
        Self::print_usage(rec);
    }

    async fn set_status(&mut self, label: &str) {
        self.set_label(label);
    }

    fn stream_sink(&self) -> Option<Box<dyn StreamSink + Send + Sync + '_>> {
        let status = self.status.clone()?;
        Some(Box::new(ReasoningSink { status }))
    }

    async fn authorize(&mut self, tool_name: &str, args: &Value) -> AuthDecision {
        // Pause the animation so the spinner doesn't paint over the authorisation box; resume after asking
        let was_running = self.spinner.is_some();
        self.stop_spinner().await;
        let decision = self.ask_authorization(tool_name, args).await;
        if was_running {
            self.set_label(crate::i18n::t("Thinking"));
            self.start_spinner().await;
        }
        decision
    }

    /// The text version of `ask_user`: print the numbered options and read one line.
    ///
    /// The same look as the authorisation box (`┌─ … │ … └─ >`), and the same reasoning: this is a
    /// question-answer injected **mid-turn**, so the user has to see at a glance that "it's my turn
    /// now".
    ///
    /// The tolerance is deliberately loosened: input that isn't recognised as a label is **treated as
    /// a free-form answer**, rather than reported as "invalid input" and re-asked. A user typing "let's
    /// just go with plan B" is giving a valid answer — bouncing it back with "please enter A/B/C" is
    /// forcing them to translate what they wanted to say into a single click.
    async fn choose(&mut self, question: &str, options: &[String]) -> ChoiceReply {
        let was_running = self.spinner.is_some();
        self.stop_spinner().await;

        let width = term_width().saturating_sub(2);
        println!();
        println!("{}", s::magenta(crate::i18n::t("┌─ pick one ────────────────────────────")));
        for line in question.lines() {
            println!("{} {}", s::magenta("│"), s::bold_cyan(&clip(line, width)));
        }
        println!("{}", s::magenta("│"));
        for (i, o) in options.iter().enumerate() {
            println!(
                "{} {} {}",
                s::magenta("│"),
                s::bold_cyan(&format!("{}.", crate::tool::ask::letter(i))),
                clip(o, width.saturating_sub(3))
            );
        }
        println!(
            "{} {}",
            s::magenta("│"),
            s::dim(crate::i18n::t("enter = give no answer and continue; anything else = treat it as your own answer"))
        );

        let mut reply = ChoiceReply::Cancelled;
        loop {
            print!("{} ", s::magenta(crate::i18n::t("└─ choose >")));
            let _ = std::io::Write::flush(&mut std::io::stdout());
            match self.read_line().await {
                Ok(Some(line)) => {
                    let ans = line.trim();
                    if ans.is_empty() {
                        break;
                    }
                    // Try the label first (A / a / 1); if it isn't recognised, treat it as a free-form answer
                    if let Some(i) = crate::tool::ask::as_choice(ans, options.len()) {
                        reply = ChoiceReply::Picked(i);
                    } else {
                        reply = ChoiceReply::Text(ans.to_string());
                    }
                    break;
                }
                // EOF / read failure: don't loop forever; honestly report "no answer given"
                _ => break,
            }
        }

        if was_running {
            self.set_label(crate::i18n::t("Thinking"));
            self.start_spinner().await;
        }
        reply
    }
}

/// /demo: print a sample of Markdown rendering, handy for checking terminal colours
pub fn print_demo() {
    let sample = "# Heading 1\n\n\
## Heading 2\n\n\
This is an ordinary paragraph, with **bold**, *italic* and `inline code`.\n\n\
- list item one\n\
- list item two\n\
  - nested item\n\n\
> Blockquote: explanatory text.\n\n\
```python\n\
def hello(name: str) -> str:\n    return f\"Hello, {name}\"\n\
```\n\n\
| Tool | Type | Status |\n\
|------|------|------|\n\
| fs_read | plugin | ok |\n\
| create_plugin | meta | ok |\n";
    println!();
    println!("{}", s::render_markdown(crate::i18n::t(sample)));
}

fn term_width() -> usize {
    termimad::crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(100)
}

/// Flatten newlines / excess whitespace, for single-line display
fn one_line(s: &str, max: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&joined, max)
}

/// Truncate by display width (ASCII counts as 1, everything else as 2)
fn clip(s: &str, max: usize) -> String {
    let mut width = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let w = if ch.is_ascii() { 1 } else { 2 };
        if width + w > max {
            out.push('…');
            return out;
        }
        width += w;
        out.push(ch);
    }
    out
}
