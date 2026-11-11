//! A wait-state animation: a single-line spinner plus a live, scrolling echo of the model's reasoning...
//! Played only on a real TTY; automatically silent when redirected to a file/pipe (self-checks, log capture)...
//! Reasoning is "shown the moment it arrives and gone the moment it ends": the Provider keeps feeding increments...

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::style as s;
use crate::llm::{thousands, TokenEstimate};

/// Compact braille spinner frames
const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Interval between frames (milliseconds)
const FRAME_MS: u64 = 80;
/// Height (in lines) of the reasoning echo window; lines beyond this are dropped.
const REASONING_ROWS: usize = 4;
/// Cap on reasoning characters kept in memory (older head is discarded to avoid unbounded growth in long sessions).
const REASONING_KEEP: usize = 4000;
/// Left indent for each reasoning line (pure ASCII, so width math never mis-aligns).
const REASONING_INDENT: &str = "  ";

/// Whether the animation is enabled: by default it plays only on a real terminal, to avoid polluting...
pub fn supported() -> bool {
    match std::env::var("AOX_ANIM").ok().as_deref() {
        Some("1") | Some("true") | Some("on") => true,
        Some("0") | Some("false") | Some("off") => false,
        _ => std::io::stdout().is_terminal(),
    }
}

/// State shared between the UI and the animation task: the current label, the turn start time, the reasoning buffer, and an output estimate.
pub struct Status {
    label: Mutex<String>,
    started: Instant,
    /// Live increments of the model's reasoning (non-empty only for streaming reasoning models).
    reasoning: Mutex<String>,
    /// Character count received on the output side (reasoning + body), used to estimate a rising live token count.
    out: Mutex<TokenEstimate>,
}

impl Status {
    pub fn new(label: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            label: Mutex::new(label.into()),
            started: Instant::now(),
            reasoning: Mutex::new(String::new()),
            out: Mutex::new(TokenEstimate::default()),
        })
    }

    /// Update the label (e.g. "thinking" → "running fs_read")
    pub fn set(&self, label: impl Into<String>) {
        if let Ok(mut l) = self.label.lock() {
            *l = label.into();
        }
    }

    /// Append a chunk of reasoning. Driven directly by the Provider's streaming callback, so it is
    /// called at high frequency.
    pub fn push_reasoning(&self, delta: &str) {
        self.count(delta);
        let Ok(mut r) = self.reasoning.lock() else { return };
        r.push_str(delta);
        if r.len() > REASONING_KEEP {
            let mut cut = r.len() - REASONING_KEEP;
            while cut < r.len() && !r.is_char_boundary(cut) {
                cut += 1; // Don't split in the middle of a multi-byte character
            }
            r.drain(..cut);
        }
    }

    /// Append a chunk of body text: the body itself is stored elsewhere; here we only count tokens.
    pub fn push_content(&self, delta: &str) {
        self.count(delta);
    }

    /// Character count on the output side. Both reasoning and body must be counted — the server's
    /// completion_tokens already include reasoning, so counting only the body underestimates badly.
    fn count(&self, delta: &str) {
        if let Ok(mut o) = self.out.lock() {
            o.add(delta);
        }
    }

    /// Clear the reasoning: called when a response finishes, to achieve the "disappear at once" effect.
    pub fn clear_reasoning(&self) {
        if let Ok(mut r) = self.reasoning.lock() {
            r.clear();
        }
    }

    fn snapshot(&self) -> (String, f64, Option<String>, u64) {
        let label = self.label.lock().map(|g| g.clone()).unwrap_or_default();
        let reasoning = self
            .reasoning
            .lock()
            .ok()
            .map(|g| g.clone())
            .filter(|r| !r.trim().is_empty());
        let tok = self.out.lock().map(|o| o.tokens()).unwrap_or(0);
        (label, self.started.elapsed().as_secs_f64(), reasoning, tok)
    }
}

/// A running wait animation. Dropped automatically stops and aborts the background task.
pub struct Spinner {
    stop: Arc<Notify>,
    handle: JoinHandle<()>,
}

impl Spinner {
    /// Start the animation; the label and elapsed time come from the shared `Status`.
    pub fn start(status: Arc<Status>) -> Self {
        let stop = Arc::new(Notify::new());
        let stop_task = stop.clone();
        let width = term_width();
        let handle = tokio::spawn(async move {
            let mut i = 0usize;
            // Number of lines currently occupying the screen: the block height changes as reasoning
            // appears/disappears, so a repaint must first move the cursor back.
            let mut drawn = 0usize;
            loop {
                draw(FRAMES[i % FRAMES.len()], &status, width, &mut drawn);
                i += 1;
                tokio::select! {
                    _ = stop_task.notified() => break,
                    _ = tokio::time::sleep(Duration::from_millis(FRAME_MS)) => {}
                }
            }
            clear_block(&mut drawn);
        });
        Self { stop, handle }
    }

    /// Stop the animation and clear the line. Waits for the background task to finish, so "cleanup"
    /// happens before any later output.
    pub async fn stop(&mut self) {
        self.stop.notify_one();
        let _ = (&mut self.handle).await;
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        // Fallback: if the task is interrupted (e.g. Ctrl+C) without an explicit stop, notify and
        // abort the background task.
        self.stop.notify_one();
        self.handle.abort();
    }
}

/// Repaint a single status line in place; if reasoning exists, scroll its most recent lines below.
fn draw(frame: &str, status: &Status, width: usize, drawn: &mut usize) {
    let (label, secs, reasoning, tok) = status.snapshot();
    let mut head = format!(
        "{} {} {}",
        s::cyan(frame),
        s::bold(&label),
        s::dim(&format!("{secs:.1}s"))
    );
    // The output token count ticks live.
    if tok > 0 {
        head.push_str(&format!("  {}", s::dim(&format!("~{} tok", thousands(tok)))));
    }
    let mut lines = vec![head];
    if let Some(text) = reasoning {
        // Leave room for the indent and a little margin on the right, so a wrapped line does not
        // mis-compute the cursor position.
        let wrap = width.saturating_sub(REASONING_INDENT.len() + 2).max(24);
        for l in tail_lines(&text, wrap, REASONING_ROWS) {
            lines.push(format!("{REASONING_INDENT}{}", s::dim(&l)));
        }
    }
    repaint(&lines, drawn);
}

/// Repaint a multi-line block in place: move back to the block top, overwrite line by line, then
/// handle height changes (it may grow or shrink).
fn repaint(lines: &[String], drawn: &mut usize) {
    let mut out = String::new();
    if *drawn > 1 {
        out.push_str(&format!("\x1b[{}A", *drawn - 1));
    }
    for (i, l) in lines.iter().enumerate() {
        out.push_str("\r\x1b[2K");
        out.push_str(l);
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    // If this pass is shorter than the last, clear the extra lines and lift the cursor back to
    // the block's end.
    if *drawn > lines.len() {
        for _ in lines.len()..*drawn {
            out.push_str("\n\r\x1b[2K");
        }
        out.push_str(&format!("\x1b[{}A", *drawn - lines.len()));
    }
    *drawn = lines.len();
    print!("{out}");
    let _ = std::io::stdout().flush();
}

/// Clear the whole block and park the cursor on the block's top line, where later output continues.
fn clear_block(drawn: &mut usize) {
    let n = (*drawn).max(1);
    let mut out = String::new();
    if n > 1 {
        out.push_str(&format!("\x1b[{}A", n - 1));
    }
    for i in 0..n {
        out.push_str("\r\x1b[2K");
        if i + 1 < n {
            out.push('\n');
        }
    }
    if n > 1 {
        out.push_str(&format!("\x1b[{}A", n - 1));
    }
    *drawn = 0;
    print!("{out}");
    let _ = std::io::stdout().flush();
}

/// width → keep the last `max` lines
fn tail_lines(text: &str, width: usize, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split(['\n', '\r']) {
        if raw.trim().is_empty() {
            continue; // Blank lines in reasoning are just paragraph breaks; no need to take window height.
        }
        let mut cur = String::new();
        let mut w = 0usize;
        for ch in raw.chars() {
            let cw = if ch.is_ascii() { 1 } else { 2 };
            if w + cw > width && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                w = 0;
            }
            cur.push(ch);
            w += cw;
        }
        out.push(cur);
    }
    if out.len() > max {
        out.split_off(out.len() - max)
    } else {
        out
    }
}

fn term_width() -> usize {
    termimad::crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(100)
}
