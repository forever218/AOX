//! Activity stream: lives in its **own window** (an egui viewport), not in the
//! main window's side panel.
//!
//! Why share state: eframe's `AppCreator` is `FnOnce`, and each viewport gets its
//! own `App` instance, so the child window can't reach the main window's `App`
//! through a `&mut self`. Everything the activity stream needs therefore lives in
//! [`Activity`], shared between the two sides via `Arc<Mutex<_>>` — the main
//! window only writes, the child window only reads and renders.
//!
//! The window is a **borderless top-level window** (a "mini AOX", initially the
//! same size as the main window). That means Windows' built-in title bar — icon,
//! title, minimize / maximize / close — is gone entirely; this module draws the
//! whole top bar itself.
//!
//! The cost is that the system-provided niceties are gone too and must be rebuilt:
//! - **Drag**: press on the top bar to send [`ViewportCommand::StartDrag`].
//! - **Resize**: a drag hot-zone is embedded along each edge and corner
//!   ([`resize_handles`]); on press the direction is handed to winit.
//! - **Maximize**: an extra button on the top bar (once the system bar is gone,
//!   double-clicking the title bar, Win+↑, etc. stop working too).
//! - **Border**: with no decoration there's no seam between the borderless window
//!   and the desktop, so a 1px outline is drawn around it at the end.
//! - **Rounded corners**: instead of drawing them ourselves (a transparent
//!   viewport would also lose the system shadow), we let DWM clip them — see
//!   [`set_round_corners`]. Radius matches the main window.
//!
//! # Core: one horizontal timeline (a Gantt chart)
//!
//! ```text
//!  0.0s      2.0s      4.0s      6.0s     8.0s
//!    │         │         │         │        │
//!  ①          ②
//! Input       ▐███▌        ▐███▌
//! Model       ▐█████▌        ▐███████████▌
//! filestat_count  ▐██▌
//! create_plugin        ▐███▌
//! System              ▐▌
//! ```
//!
//! - **The horizontal axis is time (seconds)**; tasks march left to right, and
//!   each colored block is a stretch of real elapsed time.
//! - Rows are the two fixed rows "Input / Model", then **one row per tool**, and
//!   finally an extra "System" row when needed.
//! - The horizontal axis can be zoomed freely (Ctrl+wheel / toolbar ±) and panned
//!   left/right (drag / wheel).
//! - The vertical axis: when there are too many tool rows to fit, **vertical
//!   scrolling takes over** (Shift+wheel or the scrollbar on the far right); row
//!   height itself can also be stretched (toolbar "row height −/+") — we don't
//!   squash rows unreadably just to fit one screen.
//! - The anchor button at top-left pins the view to "start / center / latest"; the
//!   "round" field jumps straight to round N.
//! - With "link lines" on, the blocks are joined head-to-tail in true execution
//!   order by one line, colored after the block it leaves.
//!
//! **Idle means nothing is recorded**: we only ever write here while a round of
//! work is running. When the program is idle, not a single pixel grows on the
//! timeline.
//!
//! **Records travel with the conversation**: at the end of each round, the new
//! blocks are handed to the Agent thread to append to **that conversation's own
//! `history/*.jsonl`** (`kind:"act"` rows, see `aox::history`). So "open an old
//! conversation" shows its timeline as it was then, and "delete that conversation"
//! takes the timeline with it — no extra cleanup logic needed. For the same
//! reason there is **no "clear" button** here: clearing the UI but not the file
//! would just let it grow back next time you open it, which is the truly
//! confusing case.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use std::collections::VecDeque;

use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Frame, Galley, Layout, Margin, Pos2,
    Rect, ResizeDirection, RichText, ScrollArea, Sense, Stroke, TextEdit, TextFormat, Ui, Vec2,
    ViewportBuilder, ViewportCommand, ViewportId, WindowLevel,
};
use serde::{Deserialize, Serialize};

use crate::app::default_window_size;
use crate::theme::{self, P};

// ────────────────────────────── Category ──────────────────────────────

/// Recording category. `Serialize` is here for persistence (see
/// [`Activity::export_acts`]): the copy written to the conversation file uses
/// `snake_case` names (`input` / `tool` …) rather than the Rust enum names —
/// the file is meant for humans and other tools to read.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActKind {
    /// User-issued instruction: the start of one round of work
    Input,
    /// Activity on the model side (thinking + output)
    Model,
    /// One tool call (call → execute → return is a single segment)
    Tool,
    /// Authorization request / result
    Auth,
    Info,
    Warn,
    Error,
}

impl ActKind {
    /// Name shown on the badge
    pub fn label(self) -> &'static str {
        match self {
            ActKind::Input => aox::i18n::t("Input"),
            ActKind::Model => aox::i18n::t("Model"),
            ActKind::Tool => aox::i18n::t("Tool"),
            ActKind::Auth => aox::i18n::t("Auth"),
            ActKind::Info => aox::i18n::t("Info"),
            ActKind::Warn => aox::i18n::t("Warning"),
            ActKind::Error => aox::i18n::t("Error"),
        }
    }

    /// Base color of the block. A low-saturation warm palette, the same language
    /// as code highlighting: input is gray, model is purple, tools are orange, the
    /// rest (auth / notice / error) sit in warm yellow and brick red.
    fn color(self) -> Color32 {
        match theme::mode() {
            theme::Mode::Light => match self {
                ActKind::Input => Color32::from_rgb(0x6E, 0x69, 0x5E),
                ActKind::Model => Color32::from_rgb(0x76, 0x57, 0x9C),
                ActKind::Tool => Color32::from_rgb(0xB3, 0x74, 0x22),
                ActKind::Auth => Color32::from_rgb(0x9A, 0x6C, 0x1C),
                ActKind::Info => Color32::from_rgb(0x77, 0x71, 0x64),
                ActKind::Warn => Color32::from_rgb(0xA8, 0x76, 0x20),
                ActKind::Error => Color32::from_rgb(0xA3, 0x4A, 0x2A),
            },
            theme::Mode::Dark => match self {
                ActKind::Input => Color32::from_rgb(0x8E, 0x8A, 0x80),
                ActKind::Model => Color32::from_rgb(0xB2, 0x93, 0xD6),
                ActKind::Tool => Color32::from_rgb(0xD3, 0x94, 0x5A),
                ActKind::Auth => Color32::from_rgb(0xD8, 0xB2, 0x6A),
                ActKind::Info => Color32::from_rgb(0x7E, 0x79, 0x6E),
                ActKind::Warn => Color32::from_rgb(0xD8, 0xB2, 0x6A),
                ActKind::Error => Color32::from_rgb(0xE0, 0x8A, 0x6B),
            },
        }
    }

    /// Brick red for failed tool calls / error events (overrides the category color)
    fn failed_color() -> Color32 {
        match theme::mode() {
            theme::Mode::Light => Color32::from_rgb(0xA3, 0x4A, 0x2A),
            theme::Mode::Dark => Color32::from_rgb(0xE0, 0x8A, 0x6B),
        }
    }

    /// Body uses monospace (parameters, results, errors are all code or data)
    fn mono(self) -> bool {
        matches!(self, ActKind::Tool | ActKind::Auth | ActKind::Error)
    }

    /// Lands on the "System" row
    fn is_sys(self) -> bool {
        matches!(self, ActKind::Auth | ActKind::Info | ActKind::Warn | ActKind::Error)
    }
}

/// Rows of the timeline. The first two rows are fixed, each tool gets its own row (in first-seen order), and "System" appears as needed.
#[derive(Clone, PartialEq, Eq)]
enum Row {
    Input,
    Model,
    Tool(String),
    Sys,
}

impl Row {
    fn label(&self) -> &str {
        match self {
            Row::Input => aox::i18n::t("Input"),
            Row::Model => aox::i18n::t("Model"),
            Row::Tool(t) => t,
            Row::Sys => aox::i18n::t("System"),
        }
    }

    /// Font for the row name: tool names are identifiers, so monospace; the rest use the body font
    fn dim(&self) -> bool {
        matches!(self, Row::Input | Row::Sys)
    }
}

/// Derive the row list from the current records. Tool rows follow first-seen
/// order, so when a new tool appears mid-stream the existing rows don't
/// reshuffle (the eye won't be thrown off by rows jumping around).
fn rows_of(acts: &VecDeque<Act>) -> Vec<Row> {
    let mut rows = vec![Row::Input, Row::Model];
    for a in acts {
        if a.kind == ActKind::Tool && !rows.iter().any(|r| matches!(r, Row::Tool(n) if n == &a.title))
        {
            rows.push(Row::Tool(a.title.clone()));
        }
    }
    if acts.iter().any(|a| a.kind.is_sys()) {
        rows.push(Row::Sys);
    }
    rows
}

fn row_of(rows: &[Row], a: &Act) -> usize {
    match a.kind {
        ActKind::Input => 0,
        ActKind::Model => 1,
        ActKind::Tool => rows
            .iter()
            .position(|r| matches!(r, Row::Tool(n) if n == &a.title))
            .unwrap_or(rows.len().saturating_sub(1)),
        _ => rows.iter().position(|r| *r == Row::Sys).unwrap_or(rows.len().saturating_sub(1)),
    }
}

// ────────────────────────────── Data ──────────────────────────────

/// One record in the activity stream = one colored block on the timeline.
///
/// serde is here because it must **persist** (written to the conversation file,
/// see [`Activity::export_acts`]): after closing and reopening that conversation,
/// the timeline should grow back exactly as it was. What's persisted is "seconds
/// relative to the start of that session"; on load it's shifted as a batch (see
/// [`Activity::load_acts`]).
#[derive(Clone, Serialize, Deserialize)]
pub struct Act {
    kind: ActKind,
    /// Which round this belongs to (the user's Nth issued instruction)
    round: u32,
    /// Plugin name (present for tool calls). Tools of the same plugin share one color family
    /// at different shades.
    plugin: String,
    /// Start time (seconds, absolute time since the app launched)
    at: f32,
    /// End time. `None` = still in progress, the bar grows in real time.
    end: Option<f32>,
    failed: bool,
    title: String,
    body: String,
    /// Stable sequence number: used by block clicks and the detail panel
    seq: u64,
}

impl Act {
    /// Duration of this entry on the axis; if still running, count up to "now"
    fn dur(&self, now: f32) -> f32 {
        (self.end.unwrap_or(now) - self.at).max(0.0)
    }

    /// End time; for in-progress entries fall back to `now`
    fn end_t(&self, now: f32) -> f32 {
        self.end.unwrap_or(now).max(self.at)
    }

    fn open(&self) -> bool {
        self.end.is_none()
    }
}

/// How many entries to keep at most. Overflow drops from the front: the stream is "recent activity", not a log archive.
const ACT_LIMIT: usize = 600;
/// The model segment's body keeps at most this many bytes (only the tail; the front is dropped)
const NOTE_CAP: usize = 4000;
/// When reloading a save, how long before "now" the last entry should end (seconds). Leave
/// a little gap so the new round doesn't grow right on top of the old records.
const START_GAP: f32 = 0.4;

/// View anchor: decides which moment the left edge aligns to
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Anchor {
    /// Start pinned to the left
    Start,
    /// "Current" pinned to the center
    Center,
    /// "Current" hugs the right edge (follows the task)
    #[default]
    Latest,
    /// Wherever the user dragged to
    Free,
}

#[derive(Default)]
pub struct Activity {
    acts: VecDeque<Act>,
    next_seq: u64,
    /// The "flushed" watermark: records whose `seq` is no greater than this are already in the conversation file (see `export_acts`).
    flushed: u64,
    /// Time origin: the moment of the first record. 0.0s on the axis is this.
    t_zero: Option<f32>,
    /// Current round number
    round: u32,
    open_input: Option<u64>,
    open_model: Option<u64>,
    open_tool: Option<u64>,
    /// The block the user selected
    selected: Option<u64>,

    status: String,
    busy: bool,
    started: Option<Instant>,
    /// The main window's time base (seconds), used to compute real-time length for "in-progress" blocks
    now: f32,

    open: bool,
    pinned: bool,
    applied_top: bool,
    theme_mode: Option<theme::Mode>,
    /// Whether size and position have been set once (see the note in `sync`)
    placed: bool,
    /// Whether rounded corners have been handed to the system (also a "do once per window" thing)
    corners: bool,
    /// Phase for the breathing dot
    spin: f32,

    // ── Timeline view ──
    anchor: Anchor,
    /// Pixels per second. `None` = not manually tuned yet, use the default (60s per screen)
    scale: Option<f32>,
    /// When `Anchor::Free`, the moment at the left edge (relative to `t_zero`, seconds)
    t0: f32,
    /// Whether to draw link lines
    link: bool,
    /// Row height: `None` = auto-fill by available height (when too many tool rows to fit, vertical scroll takes over)
    row_h: Option<f32>,
    /// Pending row-height multiplier (the toolbar can't compute "fill", same as `pending_zoom`)
    pending_row: Option<f32>,
    /// Input box for "view by round"
    round_input: String,
    /// Pending jump (queued by the toolbar, executed once the timeline knows its draw width)
    goto: Option<u32>,
    /// Pending zoom factor (same as above: the toolbar doesn't know the draw width)
    pending_zoom: Option<f32>,
    /// Pending "fit the whole span"
    fit_all: bool,
}

impl Activity {
    /// Time origin (relative time = absolute time - t_zero)
    fn tz(&self) -> f32 {
        self.t_zero.unwrap_or(0.0)
    }

    fn next_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    fn find(&mut self, seq: u64) -> Option<&mut Act> {
        self.acts.iter_mut().rev().find(|a| a.seq == seq)
    }

    /// Record one entry. Returns its sequence number.
    fn push(&mut self, mut a: Act) -> u64 {
        let seq = self.next_seq();
        a.seq = seq;
        if self.t_zero.is_none() {
            self.t_zero = Some(a.at);
        }
        self.acts.push_back(a);
        while self.acts.len() > ACT_LIMIT {
            self.acts.pop_front();
        }
        seq
    }

    /// Close one entry (only fills in its end time, changes nothing else)
    fn end_act(&mut self, seq: u64, at: f32) {
        if let Some(a) = self.find(seq) {
            if a.end.is_none() {
                a.end = Some(at.max(a.at));
            }
        }
    }

    fn close_input(&mut self, at: f32) {
        if let Some(seq) = self.open_input.take() {
            self.end_act(seq, at);
        }
    }

    fn close_model(&mut self, at: f32) {
        if let Some(seq) = self.open_model.take() {
            self.end_act(seq, at);
        }
    }

    /// Close all still-open segments
    fn close_open(&mut self, at: f32) {
        self.close_input(at);
        self.close_model(at);
        if let Some(seq) = self.open_tool.take() {
            self.end_act(seq, at);
        }
    }

    // ── Writing (called by main window) ──

    /// A round begins: the user issues an instruction. The "Input" segment = from
    /// sending to when the model actually starts speaking — i.e. the "wait for the
    /// model" time.
    pub fn round_begin(&mut self, text: &str, at: f32) {
        self.close_open(at);
        self.round += 1;
        let seq = self.push(Act {
            kind: ActKind::Input,
            round: self.round,
            plugin: String::new(),
            at,
            end: None,
            failed: false,
            title: aox::i18n::t("User instruction").to_owned(),
            body: text.to_owned(),
            seq: 0,
        });
        self.open_input = Some(seq);
    }

    /// The model starts producing (first thinking / body). Also closes the "Input" segment.
    pub fn model_begin(&mut self, at: f32) {
        self.close_input(at);
        // The model can't be speaking while a tool runs (tools are serial)
        if self.open_tool.is_some() || self.open_model.is_some() {
            return;
        }
        let seq = self.push(Act {
            kind: ActKind::Model,
            round: self.round,
            plugin: String::new(),
            at,
            end: None,
            failed: false,
            title: aox::i18n::t("Model output").to_owned(),
            body: String::new(),
            seq: 0,
        });
        self.open_model = Some(seq);
    }

    /// The model is streaming out thinking text (only goes to the detail panel, doesn't affect the timeline)
    pub fn model_push(&mut self, chunk: &str) {
        let Some(seq) = self.open_model else { return };
        let Some(a) = self.find(seq) else { return };
        a.body.push_str(chunk);
        if a.body.len() > NOTE_CAP {
            // Cut on a character boundary: slicing by bytes would split a CJK char in half and `[..]` would panic
            let mut cut = a.body.len() - NOTE_CAP;
            while cut < a.body.len() && !a.body.is_char_boundary(cut) {
                cut += 1;
            }
            a.body = aox::tf!("…(earlier text trimmed){}", &a.body[cut..]);
        }
    }

    /// One tool call starts. This also closes the "Model" segment.
    pub fn tool_begin(&mut self, tool: &str, plugin: Option<&str>, args: &str, at: f32) {
        self.close_input(at);
        self.close_model(at);
        if let Some(seq) = self.open_tool.take() {
            self.end_act(seq, at);
        }
        let seq = self.push(Act {
            kind: ActKind::Tool,
            round: self.round,
            plugin: plugin.unwrap_or("").to_owned(),
            at,
            end: None,
            failed: false,
            title: tool.to_owned(),
            body: args.to_owned(),
            seq: 0,
        });
        self.open_tool = Some(seq);
    }

    /// Append a line during a tool call (e.g. "timeout limit 30s")
    pub fn tool_note(&mut self, note: &str) {
        let Some(seq) = self.open_tool else { return };
        if let Some(a) = self.find(seq) {
            if !a.body.trim().is_empty() {
                a.body.push('\n');
            }
            a.body.push_str("· ");
            a.body.push_str(note);
        }
    }

    /// Tool call wraps up: fill in the duration, result, and success/failure
    pub fn tool_end(&mut self, is_error: bool, preview: &str, at: f32) {
        let Some(seq) = self.open_tool.take() else { return };
        if let Some(a) = self.find(seq) {
            a.end = Some(at.max(a.at));
            a.failed = is_error;
            if !preview.trim().is_empty() {
                if a.body.trim().is_empty() {
                    a.body = preview.to_owned();
                } else {
                    a.body.push_str(aox::i18n::t("\n\n── Result ──\n"));
                    a.body.push_str(preview);
                }
            }
        }
    }

    /// System event (info / warning / error / auth) — a transient entry, lands on the "System" row.
    /// **Only recorded while a round of work is running**: when the program is idle nothing is
    /// written to the timeline.
    pub fn note(&mut self, kind: ActKind, title: &str, body: &str, at: f32) {
        self.push(Act {
            kind,
            round: self.round,
            plugin: String::new(),
            at,
            end: Some(at),
            failed: false,
            title: title.to_owned(),
            body: body.to_owned(),
            seq: 0,
        });
    }

    /// A round ends: close all still-open segments
    pub fn end_round(&mut self, at: f32) {
        self.close_open(at);
    }

    /// Per-frame sync: time base + what's happening right now
    pub fn tick(&mut self, now: f32, status: &str, busy: bool) {
        if busy && !self.busy {
            self.started = Some(Instant::now());
        }
        if !busy {
            self.started = None;
        }
        self.busy = busy;
        self.now = now;
        self.status = status.to_owned();
    }

    /// Clear the timeline (used when switching conversations).
    ///
    /// **Deliberately no "Clear" button is exposed**: the activity stream is part of this
    /// conversation's record, so it can only be removed by deleting that conversation — otherwise
    /// the UI clears it but the file still holds it, and reopening the conversation grows it back,
    /// which would be genuinely incomprehensible.
    pub fn reset(&mut self) {
        self.acts.clear();
        self.open_input = None;
        self.open_model = None;
        self.open_tool = None;
        self.selected = None;
        self.t_zero = None;
        self.flushed = 0;
        self.round = 0;
        self.anchor = Anchor::Latest;
        self.scale = None;
        self.t0 = 0.0;
        self.goto = None;
        self.pending_zoom = None;
        self.fit_all = false;
        self.row_h = None;
        self.pending_row = None;
        self.round_input.clear();
    }

    /// Take the records that haven't been persisted yet (serialized to JSON) and advance the
    /// watermark to the latest.
    ///
    /// **Call per round**: if we flushed a half-finished segment (a block still open), it would
    /// land as a "no end time" stub that can't be recovered on a mid-way rollback. So we hand it
    /// all over only after a round ends and every segment is closed.
    pub fn export_acts(&mut self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for a in &self.acts {
            if a.seq > self.flushed {
                if let Ok(v) = serde_json::to_value(a) {
                    out.push(v);
                }
            }
        }
        if let Some(last) = self.acts.back() {
            self.flushed = self.flushed.max(last.seq);
        }
        out
    }

    /// Load a saved conversation's activity stream back (called when switching to that
    /// conversation).
    ///
    /// `now` = seconds since **this launch**. The save's timestamps come from the last launch's
    /// coordinate system; loading them directly would be far off from the current `now`, so the
    /// new blocks would either fly off-screen or shove the old records out. So we shift the whole
    /// batch: end the last entry slightly before `now`, and the next round naturally continues
    /// from the right.
    pub fn load_acts(&mut self, rows: &[serde_json::Value], now: f32) {
        self.reset();
        let acts: Vec<Act> = rows
            .iter()
            .filter_map(|v| serde_json::from_value::<Act>(v.clone()).ok())
            .collect();
        if acts.is_empty() {
            return;
        }
        let earliest = acts.iter().fold(f32::MAX, |m, a| m.min(a.at));
        let latest = acts.iter().fold(f32::MIN, |m, a| m.max(a.end.unwrap_or(a.at)));
        let shift = (now - START_GAP) - latest;

        let mut round = 0;
        let mut top_seq = 0;
        for mut a in acts {
            a.at += shift;
            a.end = a.end.map(|e| e + shift);
            round = round.max(a.round);
            top_seq = top_seq.max(a.seq);
            // Push straight into the queue (not via `push`): the sequence numbers come from the
            // save, and renumbering would desync the watermark from the file.
            self.acts.push_back(a);
        }
        self.round = round;
        self.next_seq = top_seq;
        self.flushed = top_seq;
        // The origin follows this batch: 0.0s on the axis = the earliest moment in the save
        self.t_zero = Some(earliest + shift);
        self.anchor = Anchor::Latest;
        if self.acts.len() > ACT_LIMIT {
            let drop = self.acts.len() - ACT_LIMIT;
            self.acts.drain(..drop);
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// "This window is gone". Every **do-once-per-window** initialization must be invalidated
    /// here, so it gets redone next time the window opens — currently two things: placing the
    /// size and position (`placed`), and telling the system to draw rounded corners (`corners`,
    /// since the rounding is attached to the HWND and vanishes with the window).
    fn forget_placement(&mut self) {
        self.placed = false;
        self.corners = false;
    }

    /// Open the window directly (used for the demo-mode preview; the normal path goes through [`toggle`]).
    pub fn force_open(&mut self) {
        self.open = true;
        self.spin = 0.0;
        self.forget_placement();
    }

    /// The currently selected one; if none selected, take the latest
    fn focused(&self) -> Option<&Act> {
        match self.selected {
            Some(seq) => {
                self.acts.iter().rev().find(|a| a.seq == seq).or_else(|| self.acts.back())
            }
            None => self.acts.back(),
        }
    }

    /// The time range covered by all records (relative seconds)
    fn span(&self) -> (f32, f32) {
        let tz = self.tz();
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for a in &self.acts {
            lo = lo.min(a.at - tz);
            hi = hi.max(a.end_t(self.now) - tz);
        }
        if lo > hi {
            (0.0, 1.0)
        } else {
            (lo, hi)
        }
    }

    /// Aim the view at round `r`
    fn fit_round(&mut self, r: u32, plot_w: f32) {
        let tz = self.tz();
        let now = self.now;
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        let mut first: Option<u64> = None;
        for a in &self.acts {
            if a.round != r {
                continue;
            }
            lo = lo.min(a.at - tz);
            hi = hi.max(a.end_t(now) - tz);
            if first.is_none() {
                first = Some(a.seq);
            }
        }
        if lo > hi {
            return;
        }
        let pad = ((hi - lo) * 0.10).clamp(0.25, 8.0);
        self.fit_range(lo - pad, hi + pad, plot_w);
        if let Some(seq) = first {
            self.selected = Some(seq);
        }
    }

    /// Make the `[lo, hi]` span fill the plot area
    fn fit_range(&mut self, lo: f32, hi: f32, plot_w: f32) {
        let span = (hi - lo).clamp(0.2, 1.0e7);
        self.anchor = Anchor::Free;
        self.scale = Some((plot_w / span).clamp(MIN_SCALE, MAX_SCALE));
        self.t0 = lo;
    }
}

pub type Shared = Arc<Mutex<Activity>>;

pub fn new_shared() -> Shared {
    Arc::new(Mutex::new(Activity::default()))
}

/// Take the lock. A previous panic poisons it; recover directly here — one window crashing should not
/// take the whole program's activity stream down with it.
pub fn lock(a: &Shared) -> MutexGuard<'_, Activity> {
    a.lock().unwrap_or_else(|e| e.into_inner())
}

fn act_id() -> ViewportId {
    ViewportId::from_hash_of("aox-activity")
}

/// The window title. Used in two places: reported when building the viewport, and later used to find the HWND back by title to set rounded corners
/// (see [`set_round_corners`]), so there must be exactly one copy.
const WINDOW_TITLE: &str = "AOX Activity Stream";

// ─────────────────────────── Window open/close ───────────────────────────

const MIN_W: f32 = 860.0;
const MIN_H: f32 = 520.0;

/// Open the activity-stream window (idempotent).
///
/// Deliberately **not a toggle**: the button in the settings page says "Open activity stream", and a control **labelled "Open" that actually
/// closes the window** is one of the most program-is-broken-feeling interactions. The window has its own close button (the × at top-right)
/// and supports system-level Alt+F4, so there is a proper way to close it. When already open it is a no-op —
/// egui's child viewports are declared by `sync` every frame, and the window gets brought to the front.
pub fn open(ctx: &egui::Context, a: &Shared) {
    let mut st = lock(a);
    if st.open {
        return;
    }
    st.open = true;
    st.spin = 0.0;
    st.forget_placement(); // reopening a window means placing it afresh
    drop(st);
    ctx.request_repaint();
}

/// Called once per frame by the main window.
///
/// egui's viewports are "declarative": **a child viewport not declared this round is simply dropped**.
/// So even while the window is minimized and untouched, it must be re-declared every frame.
pub fn sync(ctx: &egui::Context, a: &Shared) {
    let mut st = lock(a);
    if !st.open {
        return;
    }
    // Size and position are given **only on the first declaration**.
    //
    // Every frame eframe `patch`es the old builder with a new one, dispatching any changed field as a
    // command (position → `OuterPosition`, size → `InnerSize`), and some fields even rebuild the window outright.
    // So "reporting the position every frame" equals yanking the window back every frame — showing up as it not moving when dragged, resizing not working,
    // and always-on-top being lost. From the second time on, report only **never-changing** fields like title and resizable.
    let fresh = !st.placed;
    st.placed = true;

    let mut b = ViewportBuilder::default()
        .with_title(aox::i18n::t(WINDOW_TITLE))
        .with_resizable(true)
        // **No system decorations**: keeping them would just add a Windows-supplied title bar on top
        // (icon + title + minimize/maximize/close), while our top bar's buttons are self-drawn,
        // leaving two bars stacked. After turning it off, drag / resize / maximize are all taken over
        // by `window()` itself.
        .with_decorations(false);

    if fresh {
        let size = initial_size(ctx);
        b = b
            .with_inner_size([size.x, size.y])
            .with_min_inner_size([MIN_W, MIN_H])
            .with_position(initial_pos(ctx, size));
    }
    if st.pinned {
        b = b.with_window_level(WindowLevel::AlwaysOnTop);
    }

    let a2 = a.clone();
    drop(st);
    ctx.show_viewport_deferred(act_id(), b, move |ctx, _| window(ctx, &a2));
}

/// Initial size = **the main window's default size** (the same formula, see `app::default_window_size`).
///
/// `default_window_size` subtracts "the thickness decoration takes up" (title bar + border),
/// and what it computes is the **content area** size. The activity-stream window itself has no decoration,
/// so its `outer - inner` is always 0 — computing with it would add 46 points of height out of nowhere.
/// So here we must ask the **main window** for the decoration thickness, for the two content areas to line up.
fn initial_size(ctx: &egui::Context) -> Vec2 {
    let (mon, chrome) = ctx.input(|i| {
        // All viewports' info is in `raw.viewports` (eframe fills the whole table in every frame)
        let root = i.raw.viewports.get(&ViewportId::ROOT);
        let mon = i.viewport().monitor_size.or_else(|| root.and_then(|v| v.monitor_size));
        let chrome = root
            .and_then(|v| Some(v.outer_rect?.size() - v.inner_rect?.size()))
            .filter(|c| c.x > 0.0 && c.y > 0.0)
            // If unavailable (first frame, or the platform doesn't provide it), use a conservative estimate
            .unwrap_or(Vec2::new(18.0, 46.0));
        (mon, chrome)
    });
    let Some(mon) = mon else {
        return Vec2::new(1280.0, 800.0);
    };
    default_window_size(mon, chrome)
}

/// Initial position: centered on **the same monitor** as the main window. When the window is as large as the main one, placing it beside is bound
/// to not fit, so it is better to lay it straight over the middle and let the user move it.
///
/// Note `monitor_size` gives a **size**; egui does not expose the monitor origin, so we can only compute by
/// "monitor top-left = screen origin" — correct on the primary monitor, degrading on a secondary one to
/// "placed at the middle of the primary monitor", but at least it is not pushed off-screen.
fn initial_pos(ctx: &egui::Context, size: Vec2) -> Pos2 {
    let mon_size = ctx.input(|i| i.viewport().monitor_size).unwrap_or(Vec2::new(1920.0, 1080.0));
    let mon = Rect::from_min_size(Pos2::ZERO, mon_size);
    let clamp = |v: f32, lo: f32, hi: f32| v.clamp(lo, hi.max(lo));
    Pos2::new(
        clamp(mon.center().x - size.x / 2.0, 8.0, mon.max.x - size.x - 8.0),
        clamp(mon.center().y - size.y / 2.0, 8.0, mon.max.y - size.y - 8.0),
    )
}

// ═══════════════════════════ Layout metrics ═══════════════════════════

const HEADER_H: f32 = 52.0;
/// Width of the row-name gutter on the left (tool names can be long; leave enough, elide when it doesn't fit)
const GUTTER: f32 = 182.0;
/// Top time-scale band
const AXIS_H: f32 = 32.0;
/// Round-scale band
const ROUND_H: f32 = 24.0;
/// Row spacing
const LANE_GAP: f32 = 8.0;
/// Width reserved on the right for the vertical scrollbar (the axis and rows must align to the same width)
const RSV: f32 = 14.0;
/// The shortest bar (minimum width for instantaneous events, so they stay clickable)
const MIN_BAR_W: f32 = 6.0;
/// Zoom range: pixels / second
const MIN_SCALE: f32 = 0.15;
const MAX_SCALE: f32 = 320.0;
/// Default view: 60 seconds per screen
const DEFAULT_SPAN: f32 = 60.0;
/// The shortest a row may be, in pixels. **This is the vertical-scrolling boundary**: when there are too many tool rows to spread out, do not keep
/// squashing rows (past a dozen pixels both row names and bars become unreadable); let the content overflow and hand it to the scrollbar.
const LANE_MIN: f32 = 28.0;
/// The tallest a row may be when auto-spreading (fewer rows means taller rows, which are also easier to click)
const LANE_MAX: f32 = 48.0;
/// Limits for manual stretching (wider than the auto range; stretching to 100 pixels to see one row's detail is a reasonable need)
const LANE_MIN_USER: f32 = 16.0;
const LANE_MAX_USER: f32 = 104.0;

/// Grab thickness for border resizing (the four edges). An undecorated window has no native border, so these pixels must be reserved by us:
/// too narrow is hard to grab, too wide eats into the top bar's drag zone at both ends.
const RESIZE_BORDER: f32 = 5.0;
/// Side length of the four corner grab zones. Corners must support diagonal dragging, so they are thicker than the edges.
///
/// The upper bound is set by the top-bar buttons: the close button's left edge sits 10 + 32 = 42 points from the right,
/// and its top edge is at `HEADER_H/2 - BH/2 = 12` points, so a 12-point square corner zone cannot reach it.
const RESIZE_CORNER: f32 = 12.0;

// ─────────────────────────── The child window's content ───────────────────────────

fn window(ctx: &egui::Context, shared: &Shared) {
    let mut st = lock(shared);

    // System-level close (Alt+F4, taskbar right-click close) must flip the switch back too,
    // otherwise the main window re-declares this viewport next frame and the window cannot be closed.
    if ctx.input(|i| i.viewport().close_requested()) {
        st.open = false;
        st.forget_placement();
    }
    // Dispatch the command only when the pinned state changes, not every frame.
    if st.pinned != st.applied_top {
        st.applied_top = st.pinned;
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(if st.pinned {
            WindowLevel::AlwaysOnTop
        } else {
            WindowLevel::Normal
        }));
    }
    // The native theme must be sent **once per viewport**: `SetTheme` is dispatched per viewport.
    if st.theme_mode != Some(theme::mode()) {
        st.theme_mode = Some(theme::mode());
        theme::sync_native_theme(ctx);
    }

    // Rounded corners are the same — a "do once per window" thing. It is done here rather than when building the viewport:
    // at that point the HWND has not been created by winit yet, so it cannot be found by title at all.
    if !st.corners {
        st.corners = true;
        set_round_corners(aox::i18n::t(WINDOW_TITLE));
    }

    let frame = Frame::NONE.fill(P::panel());
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        // Child and main viewports share one `egui::Context`, so widget Ids are **not** automatically prefixed with viewport info.
        ui.push_id("aox-activity-window", |ui| {
            body(ui, ctx, &mut st);
        });
    });

    // While busy, drive repaints ourselves: the child viewport's repaint requests are independent.
    if st.busy {
        st.spin = (st.spin + 0.016) % 1000.0;
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

fn body(ui: &mut Ui, ctx: &egui::Context, st: &mut Activity) {
    // The whole window's rect. **Must be taken before putting anything into the ui**: `max_rect()` shrinks
    // with each allocate, and by the end of the function only a sliver at the bottom would be left.
    let full = ui.max_rect();

    header(ui, ctx, st);

    Frame::NONE.inner_margin(Margin { left: 18, right: 18, top: 12, bottom: 14 }).show(ui, |ui| {
        // All vertical spacing inside this block is added by us, so egui's item_spacing doesn't skew the height
        ui.spacing_mut().item_spacing.y = 0.0;

        toolbar(ui, st);
        ui.add_space(14.0);

        // The row list must be known first: how tall the timeline is depends on how many tool rows there are.
        // With few rows the timeline is shorter, ceding space to the detail panel below; with many rows there is a cap,
        // so the detail panel is not squeezed out — the surplus rows go to vertical scrolling (see `timeline`).
        let rows = rows_of(&st.acts);
        let n = rows.len() as f32;
        let natural = AXIS_H + ROUND_H + n * LANE_MAX + (n - 1.0).max(0.0) * LANE_GAP;
        let rest = ui.available_height();
        let chart_h = natural.min(rest - 170.0).max(170.0);
        // The detail panel **takes all the remaining height**, no longer capped.
        // Capping it (it used to clamp to 420) leaves a blank strip in the window's lower half — when the timeline clearly
        // could be taller and the detail clearly could be wider, a hard-coded cap blocks them. It is internally
        // scrollable anyway, so no height will break it.
        let detail_h = (rest - chart_h - 14.0).max(132.0);

        timeline(ui, ctx, st, &rows, chart_h);
        ui.add_space(14.0);
        detail(ui, st, detail_h);
    });

    // Finally overlay two "window-level" things: the border drag hot-zones (which must beat the top bar's drag zone, so they must be
    // declared last) and the 1px outline on the outermost ring.
    resize_handles(ui, ctx, full);
    outline(ui, full);
}

/// An undecorated window **has no native border resizing**; we add it back here.
///
/// The approach is the same as egui's built-in `Resize` container: bury one transparent hot-zone using `Sense::drag`
/// along each of the four edges and four corners; on press it sends a single [`ViewportCommand::BeginResize`], and from then on the whole drag
/// is taken over by the system (not us changing the window size every frame — that would both stutter and fight the system).
///
/// Order matters: these hot-zones are declared **last** in `body`, and egui's hit testing is
/// "later-declared wins within a layer", so the outermost few pixels go to resizing rather than top-bar dragging.
/// Overlap happens only at the top bar's edge; the buttons (close etc.) are still a dozen pixels from the edge and are not stolen.
fn resize_handles(ui: &mut Ui, ctx: &egui::Context, full: Rect) {
    // A maximized window should not be edge-draggable (Windows itself works this way), which also avoids
    // the oddity of "dragging along and falling out of the maximized state".
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }

    let zones = resize_zones(full);
    // The cursor must change on **hover** (without waiting for press), otherwise the user has no idea this is draggable
    let hover = ctx.input(|i| i.pointer.hover_pos());
    if let Some(p) = hover {
        if let Some((_, _, d)) = zones.iter().find(|(_, r, _)| r.contains(p)) {
            ctx.set_cursor_icon(resize_cursor(*d));
        }
    }

    for (id, r, dir) in zones {
        let resp = ui.interact(r, ui.id().with(id), Sense::drag());
        if resp.hovered() {
            ctx.set_cursor_icon(resize_cursor(dir));
        }
        if resp.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
        }
    }
}

/// Geometry of the eight hot-zones. The four edges take only the segment **with the corners removed**,
/// and each corner gets its own square zone — otherwise a corner would hit two zones at once and the
/// direction couldn't be determined.
fn resize_zones(full: Rect) -> [(&'static str, Rect, ResizeDirection); 8] {
    let (l, r, t, b) = (full.left(), full.right(), full.top(), full.bottom());
    let d = RESIZE_BORDER;
    let c = RESIZE_CORNER;
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| {
        Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1))
    };
    use ResizeDirection::*;
    [
        ("nw", rect(l, t, l + c, t + c), NorthWest),
        ("ne", rect(r - c, t, r, t + c), NorthEast),
        ("sw", rect(l, b - c, l + c, b), SouthWest),
        ("se", rect(r - c, b - c, r, b), SouthEast),
        ("n", rect(l + c, t, r - c, t + d), North),
        ("s", rect(l + c, b - d, r - c, b), South),
        ("w", rect(l, t + c, l + d, b - c), West),
        ("e", rect(r - d, t + c, r, b - c), East),
    ]
}

fn resize_cursor(d: ResizeDirection) -> CursorIcon {
    use ResizeDirection::*;
    match d {
        North | South => CursorIcon::ResizeVertical,
        East | West => CursorIcon::ResizeHorizontal,
        NorthEast | SouthWest => CursorIcon::ResizeNeSw,
        NorthWest | SouthEast => CursorIcon::ResizeNwSe,
    }
}

/// There is no dividing line between an undecorated window and the desktop; in a light theme you can barely tell where the window ends.
/// Draw a 1px outline to frame it (a touch heavier than the top bar's divider, so it holds against the background).
///
/// The corners must follow the system's: the window is ultimately clipped to rounded corners by DWM, and drawing square corners here would
/// leave the corner few pixels of border clipped, producing a square break in the corner.
fn outline(ui: &Ui, full: Rect) {
    ui.painter().rect_stroke(
        full.shrink(0.5),
        CornerRadius::same(CORNER_RADIUS),
        Stroke::new(1.0, P::border_strong()),
        egui::StrokeKind::Inside,
    );
}

// ───────────────────── System rounded corners (Windows 11) ─────────────────────

/// DWM's standard corner radius, matching the main window. The unit is egui points, which grow with scaling.
const CORNER_RADIUS: u8 = 8;

/// Let the system draw rounded corners on this window.
///
/// **Why not draw them ourselves** (viewport with `with_transparent` + draw a rounded rect ourselves):
/// the corners can be drawn, but everything outside the border must be truly transparent, so the
/// **system shadow is lost with them** — the window becomes a sticker slapped on the desktop, and
/// transparent windows tend to flicker when dragged or resized.
///
/// Yet winit's undecorated window is not `WS_POPUP`: it uses `WM_NCCALCSIZE` to squash the non-client
/// area to 0 while keeping `WS_BORDER` / `WS_SYSMENU` — so in the system's eyes this is still an
/// **ordinary overlapping window**, and going through DWM gets us antialiased corners while keeping the
/// shadow intact, with a radius that naturally matches the main window (exactly the thing we lost when
/// we removed the system title bar).
///
/// winit actually ships this switch (`WindowAttributesExtWindows::with_corner_preference`),
/// but egui's `ViewportBuilder` has no passthrough for it, and child viewports don't even have a hook
/// (`ViewportBuilder` only has `has_shadow`, `taskbar`, etc., and eframe doesn't use them all either).
/// So here we just call the system API once: three functions, a dozen-ish lines of declarations;
/// not worth pulling in windows-sys for this.
///
/// It only needs to be called **once** after the window is built — rounded corners are a state hung on
/// the HWND, not a per-frame thing. The moment the window is rebuilt (closed and reopened) the corners
/// vanish, so the caller tracks it via [`Activity::corners`].
#[cfg(windows)]
fn set_round_corners(title: &str) {
    use std::ffi::c_void;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(cb: extern "system" fn(*mut c_void, isize) -> i32, param: isize) -> i32;
        fn IsWindowVisible(h: *mut c_void) -> i32;
        fn GetWindowTextLengthW(h: *mut c_void) -> i32;
        fn GetWindowTextW(h: *mut c_void, buf: *mut u16, max: i32) -> i32;
        fn GetWindowThreadProcessId(h: *mut c_void, pid: *mut u32) -> u32;
    }

    /// The context used during enumeration. The callback can only be a plain fn that captures no environment, so things must be passed in via `param`.
    struct Probe<'a> {
        title: &'a str,
        pid: u32,
        hwnd: *mut c_void,
    }

    /// Whether it is "a window opened by this process with a matching title".
    ///
    /// SAFETY: `h` is a handle passed by `EnumWindows` and is valid for the duration of this call.
    unsafe fn mine(h: *mut c_void, title: &str, pid: u32) -> bool {
        if unsafe { IsWindowVisible(h) } == 0 {
            return false;
        }
        // Looking at the title alone is not enough: a user may well have two AOX instances open at once (e.g. two installs),
        // and then we could reach another's window and change its corners.
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(h, &mut owner) };
        if owner != pid {
            return false;
        }
        let n = unsafe { GetWindowTextLengthW(h) };
        if n <= 0 {
            return false;
        }
        let mut buf: Vec<u16> = vec![0; n as usize + 1];
        let got = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), n + 1) };
        if got <= 0 {
            return false;
        }
        String::from_utf16_lossy(&buf[..got as usize]).contains(title)
    }

    extern "system" fn probe(h: *mut c_void, param: isize) -> i32 {
        // SAFETY: `param` is the address of the `Probe` below, with a lifetime covering the whole `EnumWindows`
        let p = unsafe { &mut *(param as *mut Probe) };
        if p.hwnd.is_null() && unsafe { mine(h, p.title, p.pid) } {
            p.hwnd = h;
            return 0; // stop once found, don't keep enumerating
        }
        1
    }

    let mut ctx = Probe { title, pid: std::process::id(), hwnd: std::ptr::null_mut() };
    unsafe { EnumWindows(probe, &mut ctx as *mut Probe as isize) };
    if ctx.hwnd.is_null() {
        return; // not built yet (or the platform doesn't provide it); try again next time the window opens
    }
    // DWM calls all go through `theme`: each symbol is declared only once in the whole crate.
    // Failure (Win10 does not know this attribute) is fine — corners simply stay square, as they were.
    theme::dwm_set_attribute(
        ctx.hwnd as isize,
        theme::DWMWA_CORNER_PREFERENCE,
        &theme::DWMWCP_ROUND as *const i32 as *const c_void,
        std::mem::size_of::<i32>() as u32,
    );
}

#[cfg(not(windows))]
fn set_round_corners(_title: &str) {}

// ───────────────────────────── Top bar ─────────────────────────────

fn header(ui: &mut Ui, ctx: &egui::Context, st: &mut Activity) {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), HEADER_H), Sense::click_and_drag());
    // StartDrag must be sent on the **very frame** the mouse is pressed; a frame late and the window won't follow.
    if resp.drag_started() {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }

    // Left: breathing dot + title + current state
    let pulse = ((st.spin * 2.0) as i32) % 2 == 0;
    ui.painter().circle_filled(
        Pos2::new(rect.left() + 22.0, rect.center().y),
        5.0,
        if st.busy {
            if pulse {
                P::ink()
            } else {
                P::ink_soft()
            }
        } else {
            P::text_faint()
        },
    );
    ui.painter().text(
        Pos2::new(rect.left() + 40.0, rect.center().y),
        Align2::LEFT_CENTER,
        aox::i18n::t("Activity Stream"),
        theme::bold_font(18.0),
        P::text(),
    );

    let status = if st.status.is_empty() { aox::i18n::t("Ready") } else { &st.status };
    let info = if st.busy {
        format!("{status} · {:.1}s", st.started.map_or(0.0, |s| s.elapsed().as_secs_f32()))
    } else if st.round == 0 {
        aox::i18n::t("Idle · nothing is recorded").to_string()
    } else {
        aox::tf!("Idle · {} rounds · {} records", st.round, st.acts.len())
    };
    ui.painter().text(
        Pos2::new(rect.left() + 108.0, rect.center().y),
        Align2::LEFT_CENTER,
        info,
        FontId::proportional(13.5),
        P::text_faint(),
    );

    // Laid out right-to-left: close / maximize / minimize / pin, which lands on screen as Windows'
    // usual "pin · minimize · maximize · close". Since the system title bar is gone,
    // these self-drawn buttons are the window's only way out.
    //
    // There is **no "clear" here any more**: the activity stream is already persisted into the same file with this conversation, so removing it can only
    // be done by deleting that conversation — clearing on a click in the UI while the file still holds it, and reopening this
    // conversation grows it back, is the truly incomprehensible case.
    const BW: f32 = 32.0;
    const BH: f32 = 28.0;
    const GAP: f32 = 2.0;
    let y = rect.center().y - BH / 2.0;
    let mut x = rect.right() - 10.0 - BW;
    let btn = |x: f32| Rect::from_min_size(Pos2::new(x, y), Vec2::new(BW, BH));
    if win_button(ui, btn(x), "close", Glyph::Close) {
        st.open = false;
        st.forget_placement();
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }
    x -= BW + GAP;
    // The maximized state must be asked for actively — this window is self-managed now, and nobody remembers whether it is maximized for us
    let maxed = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    if win_button(ui, btn(x), "max", Glyph::Max { on: maxed }) {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maxed));
    }
    x -= BW + GAP;
    if win_button(ui, btn(x), "min", Glyph::Min) {
        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
    }
    x -= BW + GAP;
    if win_button(ui, btn(x), "pin", Glyph::Pin { on: st.pinned }) {
        st.pinned = !st.pinned;
    }

    // A thin line between the title bar and the content
    let line = Rect::from_min_max(
        Pos2::new(rect.left(), rect.bottom() - 1.0),
        Pos2::new(rect.right(), rect.bottom()),
    );
    ui.painter().rect_filled(line, CornerRadius::ZERO, P::border());
}

#[derive(Clone, Copy)]
enum Glyph {
    Close,
    Min,
    /// Maximize / restore. `on` = currently maximized (so it is drawn as the "restore" double frame)
    Max { on: bool },
    Pin { on: bool },
}

fn win_button(ui: &mut Ui, r: Rect, id: &str, glyph: Glyph) -> bool {
    let resp = ui.interact(r, ui.id().with(id), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, CornerRadius::same(7), P::surface_hi());
    }
    // "Pinned" is a **persistent state** and must stay visible: give it the deepest foreground colour even when not hovered,
    // otherwise a grey pushpin on a grey background leaves the user unable to tell whether it is pinned.
    let pinned = matches!(glyph, Glyph::Pin { on: true });
    let fg = if resp.hovered() || pinned { P::ink() } else { P::text_dim() };
    let p = ui.painter();
    let c = r.center();
    let stroke = Stroke::new(1.4, fg);
    match glyph {
        Glyph::Close => {
            let d = 4.2;
            p.line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], stroke);
            p.line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], stroke);
        }
        Glyph::Min => {
            p.line_segment([c + Vec2::new(-5.0, 2.4), c + Vec2::new(5.0, 2.4)], stroke);
        }
        Glyph::Max { on: false } => {
            // A single box — exactly Windows' maximize
            p.rect_stroke(
                Rect::from_center_size(c, Vec2::splat(9.6)),
                CornerRadius::ZERO,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        Glyph::Max { on: true } => {
            // "Restore": the rear frame is half-hidden by the front one, so only its top and right edges are stroked
            let (b0, b1) = (c + Vec2::new(-2.4, -5.0), c + Vec2::new(5.0, 1.6));
            p.line_segment([b0, Pos2::new(b1.x, b0.y)], stroke);
            p.line_segment([Pos2::new(b1.x, b0.y), b1], stroke);
            p.rect_stroke(
                Rect::from_min_max(c + Vec2::new(-4.8, -2.0), c + Vec2::new(2.6, 5.4)),
                CornerRadius::ZERO,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        Glyph::Pin { on } => {
            // A pushpin: a flat head + a needle tapering to a point.
            //
            // Pinned and unpinned **share the same shape**, differing only by a 90° orientation —
            // unpinned it lies down (tip toward lower-left), pinned it stands up (tip pointing down).
            // So the two states aren't "a different picture" but the same pin lying down vs standing up,
            // and the user knows the state changed without having to re-learn an icon.
            //
            // Fully filled, no stroking: thin lines are genuinely invisible on this dark background.
            let rot = |v: Vec2| if on { v } else { Vec2::new(-v.y, v.x) };
            // After a 90° rotation a rect's length and width swap, so the dimensions need a separate swap
            let swap = |v: Vec2| if on { v } else { Vec2::new(v.y, v.x) };

            // Head
            p.rect_filled(
                Rect::from_center_size(c + rot(Vec2::new(0.0, -4.2)), swap(Vec2::new(10.0, 2.6))),
                CornerRadius::same(1),
                fg,
            );
            // Needle shaft
            p.rect_filled(
                Rect::from_center_size(c + rot(Vec2::new(0.0, -1.0)), swap(Vec2::new(3.2, 3.8))),
                CornerRadius::ZERO,
                fg,
            );
            // Needle tip
            p.add(egui::Shape::convex_polygon(
                vec![
                    c + rot(Vec2::new(0.0, 4.8)),
                    c + rot(Vec2::new(-1.6, 0.9)),
                    c + rot(Vec2::new(1.6, 0.9)),
                ],
                fg,
                Stroke::NONE,
            ));
        }
    }
    resp.clicked()
}

// ───────────────────────────── Toolbar ─────────────────────────────

/// A row of view tools: the left half is "which stretch to look at", the right half is "how to view it".
fn toolbar(ui: &mut Ui, st: &mut Activity) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;

        // ── Anchors: pin a moment to some position on screen ──
        if pill(ui, aox::i18n::t("Start"), st.anchor == Anchor::Start)
            .on_hover_text(aox::i18n::t("Pin the start of the first round to the left edge"))
            .clicked()
        {
            st.anchor = Anchor::Start;
        }
        if pill(ui, aox::i18n::t("Center"), st.anchor == Anchor::Center)
            .on_hover_text(aox::i18n::t("Keep the current moment centered, visible on both sides"))
            .clicked()
        {
            st.anchor = Anchor::Center;
        }
        if pill(ui, aox::i18n::t("Latest"), st.anchor == Anchor::Latest)
            .on_hover_text(aox::i18n::t("Follow the latest progress"))
            .clicked()
        {
            st.anchor = Anchor::Latest;
        }

        vsep(ui);

        // ── View by round ──
        ui.label(RichText::new(aox::i18n::t("Round")).color(P::text_faint()).size(13.5));
        let edit = ui.add(
            TextEdit::singleline(&mut st.round_input)
                .id(egui::Id::new("aox-act-round-input"))
                .desired_width(48.0)
                .font(theme::mono_font(14.0))
                .hint_text("1"),
        );
        let entered = edit.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter))
            && !st.round_input.trim().is_empty();
        if pill(ui, aox::i18n::t("Go"), false).clicked() || entered {
            if let Ok(r) = st.round_input.trim().parse::<u32>() {
                if r >= 1 && r <= st.round {
                    st.goto = Some(r);
                }
            }
        }
        ui.label(
            RichText::new(aox::tf!("{} rounds", st.round)).color(P::text_faint()).size(13.5),
        );

        // ── Right half: row height + horizontal zoom + link lines ──
        //
        // In `right_to_left`, **whatever is added first is rightmost**, so the writing order below is the reverse of
        // the on-screen left-to-right order: what you finally see is
        //     Link lines  Fit │ Zoom − + │ Row height − +
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;

            // Row height: with many tools, rows get thinned until unreadable — these two stretch/shrink
            // each row, and anything past the viewport is scrolled with Shift+wheel (or the rightmost scrollbar).
            if pill(ui, "+", false)
                .on_hover_text(aox::i18n::t("Row height +: stretch each row; when it overflows, scroll with Shift+wheel"))
                .clicked()
            {
                st.pending_row = Some(1.25);
            }
            if pill(ui, "−", false).on_hover_text(aox::i18n::t("Row height −: squeeze each row to fit more on screen")).clicked()
            {
                st.pending_row = Some(1.0 / 1.25);
            }
            ui.label(RichText::new(aox::i18n::t("Row height")).color(P::text_faint()).size(13.5));
            vsep(ui);

            if pill(ui, "+", false).on_hover_text(aox::i18n::t("Zoom in (horizontal axis)")).clicked() {
                st.pending_zoom = Some(1.35);
            }
            if pill(ui, "−", false).on_hover_text(aox::i18n::t("Zoom out (horizontal axis)")).clicked() {
                st.pending_zoom = Some(1.0 / 1.35);
            }
            ui.label(RichText::new(aox::i18n::t("Zoom")).color(P::text_faint()).size(13.5));
            vsep(ui);

            if pill(ui, aox::i18n::t("Fit"), false)
                .on_hover_text(aox::i18n::t("Fill the window with the whole span and hand row height back to auto"))
                .clicked()
            {
                st.fit_all = true;
            }
            if pill(ui, aox::i18n::t("Link lines"), st.link)
                .on_hover_text(aox::i18n::t("Join the blocks head-to-tail in true execution order; each line takes the color of the block it leaves"))
                .clicked()
            {
                st.link = !st.link;
            }
        });
    });
}

/// A thin vertical separator in the toolbar
fn vsep(ui: &mut Ui) {
    ui.add_space(6.0);
    let (r, _) = ui.allocate_exact_size(Vec2::new(1.0, 20.0), Sense::hover());
    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, P::border()));
    ui.add_space(6.0);
}

/// A pill button. `on` = the currently selected one (reversed).
fn pill(ui: &mut Ui, text: &str, on: bool) -> egui::Response {
    let font = FontId::proportional(14.0);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    let pad = Vec2::new(12.0, 6.0);
    let size = galley.size() + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hov = resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    let r = Rect::from_center_size(rect.center(), size);
    p.rect_filled(
        r,
        CornerRadius::same(10),
        if on {
            P::reverse_bg()
        } else if hov {
            P::surface_hi()
        } else {
            Color32::TRANSPARENT
        },
    );
    if !on {
        p.rect_stroke(
            r,
            CornerRadius::same(10),
            Stroke::new(1.0, if hov { P::border_strong() } else { P::border() }),
            egui::StrokeKind::Inside,
        );
    }
    let fg =
        if on { P::on_reverse() } else if hov { P::text() } else { P::text_dim() };
    p.galley(r.min + pad, galley, fg);
    resp
}

// ═══════════════════════════ Timeline ═══════════════════════════

/// Map time to pixels: the horizontal axis is time, and tasks advance left to right.
#[derive(Clone, Copy)]
struct View {
    /// The moment at the left edge (seconds, relative to `t_zero`)
    t0: f32,
    /// Pixels per second
    scale: f32,
}

impl View {
    fn compute(st: &Activity, plot_w: f32) -> View {
        let (t_min, t_max) = if st.acts.is_empty() {
            (0.0, 1.0)
        } else {
            let tz = st.tz();
            let mut lo = f32::MAX;
            let mut hi = f32::MIN;
            for a in &st.acts {
                lo = lo.min(a.at - tz);
                // In-progress segments take "now" as their tail; otherwise use the real end time
                hi = hi.max(a.end_t(st.now) - tz);
            }
            (lo, hi)
        };
        let scale = st.scale.unwrap_or_else(|| (plot_w / DEFAULT_SPAN).clamp(MIN_SCALE, MAX_SCALE));
        let vis = plot_w / scale;
        // Leave at least a little margin beyond the view, so bars at either end don't hug the edge
        let pad = (vis * 0.015).clamp(0.05, 2.0);
        let t0 = match st.anchor {
            Anchor::Start => t_min,
            Anchor::Center => t_max - vis * 0.5,
            Anchor::Latest => t_max - vis,
            Anchor::Free => st.t0,
        };
        let t0 = t0.max(t_min - pad);
        let t0 = if st.anchor == Anchor::Free {
            t0.min((t_max - vis * 0.15).max(t_min - pad))
        } else {
            t0
        };
        View { t0, scale }
    }

    fn vis(self, plot_w: f32) -> f32 {
        plot_w / self.scale
    }

    fn x(self, plot: Rect, t: f32) -> f32 {
        plot.left() + (t - self.t0) * self.scale
    }

    fn t_at(self, plot: Rect, x: f32) -> f32 {
        self.t0 + (x - plot.left()) / self.scale
    }
}

fn timeline(ui: &mut Ui, ctx: &egui::Context, st: &mut Activity, rows: &[Row], avail_h: f32) {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let w = ui.available_width();
        let (band, band_resp) = ui.allocate_exact_size(Vec2::new(w, AXIS_H), Sense::hover());
        let (rband, _) = ui.allocate_exact_size(Vec2::new(w, ROUND_H), Sense::hover());
        // There is more than one gesture and no clickable widget to carry the hint, so it hangs off the scale band:
        // this is the "timeline" object itself, and the hint is most apt when the mouse passes over it.
        band_resp.on_hover_text(
            "Wheel: pan left/right · Ctrl+wheel: zoom the horizontal axis · Shift+wheel: scroll rows\n\
             Drag: pan · Click a block: see this step's details",
        );

        // The plot area's left edge and width: axis, row names and bars all align to it
        let plot_left = band.left() + GUTTER;
        let plot_w = (band.right() - RSV - plot_left).max(60.0);
        let area_h = (avail_h - AXIS_H - ROUND_H).max(60.0);
        // The two top bands are not the "plot area": they only borrow the plot's x coordinate,
        // so a rect with an aligned left edge must be built for them — passing the whole band into
        // `View::x` would offset the ticks from the bars by a whole gutter width.
        let axis = Rect::from_min_size(Pos2::new(plot_left, band.top()), Vec2::new(plot_w, AXIS_H));
        let ruler =
            Rect::from_min_size(Pos2::new(plot_left, rband.top()), Vec2::new(plot_w, ROUND_H));
        // The rows' visible area (also the pan/zoom response area). The ScrollArea's viewport lands exactly here.
        let lane_rect =
            Rect::from_min_size(Pos2::new(band.left(), rband.bottom()), Vec2::new(w - RSV, area_h));

        // Compute row height first: by default "spread this height evenly", but **not down to squashed** — as tool rows grow,
        // `LANE_MIN` holds the floor and the surplus goes to vertical scrolling. If the user has dragged it, use their value.
        let n = rows.len().max(1) as f32;
        let fit = ((area_h - LANE_GAP * (n - 1.0)) / n).clamp(LANE_MIN, LANE_MAX);

        // ── Handle jumps and interaction first, then draw by the final view ──
        if let Some(r) = st.goto.take() {
            st.fit_round(r, plot_w);
        }
        if st.fit_all {
            st.fit_all = false;
            // "Fit" = both axes back to auto: the horizontal axis fills the whole span, and row height returns to spreading by height
            st.row_h = None;
            let (lo, hi) = st.span();
            let pad = ((hi - lo) * 0.04).max(0.2);
            st.fit_range(lo - pad, hi + pad, plot_w);
            // When fitting the whole span, pin the left end so the view isn't carried away as the task advances
            st.anchor = Anchor::Start;
        }
        if let Some(f) = st.pending_zoom.take() {
            st.zoom_by(f, plot_w);
        }
        if let Some(k) = st.pending_row.take() {
            // On the first stretch, base it on the "auto" current value, so the feel stays continuous
            st.row_h = Some((st.row_h.unwrap_or(fit) * k).clamp(LANE_MIN_USER, LANE_MAX_USER));
        }

        let v0 = View::compute(st, plot_w);
        input(ui, ctx, st, lane_rect, plot_left, plot_w, v0);
        let v = View::compute(st, plot_w);

        // ── Time scale band ──
        let base_y = band.bottom() - 1.0;
        {
            let p = ui.painter();
            p.line_segment(
                [Pos2::new(plot_left, base_y), Pos2::new(plot_left + plot_w, base_y)],
                Stroke::new(1.0, P::border()),
            );
            let step = nice_step(90.0 / v.scale);
            let right = plot_left + plot_w;
            let mut t = (v.t0 / step).floor() * step;
            let mut guard = 0;
            while v.x(axis, t) <= right + step && guard < 4096 {
                guard += 1;
                let x = v.x(axis, t);
                if t >= -0.0001 && x >= plot_left - 0.5 {
                    p.line_segment(
                        [Pos2::new(x, base_y - 6.0), Pos2::new(x, base_y)],
                        Stroke::new(1.0, P::border_strong()),
                    );
                    p.text(
                        Pos2::new(x, base_y - 17.0),
                        Align2::CENTER_CENTER,
                        fmt_at(t, step),
                        theme::mono_font(12.0),
                        P::text_faint(),
                    );
                }
                t += step;
            }
            p.text(
                Pos2::new(plot_left - 12.0, base_y - 15.0),
                Align2::RIGHT_CENTER,
                aox::i18n::t("s"),
                FontId::proportional(12.5),
                P::text_faint(),
            );
            p.text(
                Pos2::new(plot_left - 12.0, rband.center().y),
                Align2::RIGHT_CENTER,
                aox::i18n::t("Round"),
                FontId::proportional(12.5),
                P::text_faint(),
            );
        }

        // ── Round scale band ──
        {
            let p = ui.painter();
            let tz = st.tz();
            let mut last_right = f32::MIN;
            for a in st.acts.iter().filter(|a| a.kind == ActKind::Input) {
                let x = v.x(ruler, a.at - tz);
                if x < plot_left - 20.0 || x > plot_left + plot_w {
                    continue;
                }
                let text = a.round.to_string();
                let tw = text_w(ui, &theme::mono_font(12.0), &text) + 14.0;
                let r =
                    Rect::from_min_size(Pos2::new(x + 3.0, rband.top() + 2.0), Vec2::new(tw, 19.0));
                if r.right() < last_right + 4.0 {
                    continue; // skip drawing when they crowd together, so they don't pile up
                }
                last_right = r.right();
                p.rect_filled(r, CornerRadius::same(6), P::surface());
                p.rect_stroke(
                    r,
                    CornerRadius::same(6),
                    Stroke::new(1.0, P::border()),
                    egui::StrokeKind::Inside,
                );
                p.text(r.center(), Align2::CENTER_CENTER, text, theme::mono_font(12.0), P::text_dim());
            }
        }

        // ── Rows ──
        // Row height can be "auto-spread" or a value the user dragged; either may exceed the visible area,
        // and when it does, the scroll area below handles paging up and down.
        let lane_h = st.row_h.unwrap_or(fit);
        let content_h = n * lane_h + (n - 1.0) * LANE_GAP;
        let overflows = content_h > area_h + 0.5;

        // The wheel pans horizontally by default (see `input`), so vertical scrolling hangs off Shift:
        // only while Shift is held is the wheel fed to the scroll area, otherwise it goes to the timeline.
        let shift_down = ctx.input(|i| i.modifiers.shift);
        ScrollArea::vertical()
            .id_salt("aox-act-lanes")
            .scroll_source(egui::scroll_area::ScrollSource {
                scroll_bar: true,
                mouse_wheel: shift_down,
                drag: false,
            })
            .auto_shrink([false, false])
            .max_height(area_h)
            .show(ui, |ui| {
                let (content, _) =
                    ui.allocate_exact_size(Vec2::new(w - RSV, content_h), Sense::hover());
                let plot = Rect::from_min_max(
                    Pos2::new(content.left() + GUTTER, content.top()),
                    Pos2::new(content.left() + GUTTER + plot_w, content.bottom()),
                );
                lanes(ui, st, rows, plot, lane_h, v);
            });

        // When rows are folded out of view, give a line at the bottom-right — a scrollbar alone is too thin a clue,
        // and it is easy to think "that's all the tools there are".
        if overflows {
            let below = ((content_h - area_h) / (lane_h + LANE_GAP)).ceil() as i32;
            ui.painter().text(
                Pos2::new(band.right() - RSV - 6.0, lane_rect.bottom() - 4.0),
                Align2::RIGHT_BOTTOM,
                aox::tf!("{} more rows · scroll with Shift+wheel", below),
                FontId::proportional(12.0),
                P::text_faint(),
            );
        }
    });
}

impl Activity {
    /// Zoom anchored at the view's centre (the toolbar's ± uses it).
    fn zoom_by(&mut self, factor: f32, plot_w: f32) {
        let v = View::compute(self, plot_w);
        let ns = (v.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        if self.anchor == Anchor::Free {
            let c = v.t0 + v.vis(plot_w) * 0.5;
            self.t0 = c - (plot_w / ns) * 0.5;
        }
        self.scale = Some(ns);
    }
}

/// Pan / zoom / click-select. All only take effect when landing in the timeline's row area.
///
/// The response declaration order matters: this pan area is declared here first (before the scroll area),
/// and the bars are declared later inside the scroll area — so clicking a bar lets the bar win, clicking blank space lets this win.
fn input(
    ui: &mut Ui,
    ctx: &egui::Context,
    st: &mut Activity,
    lane: Rect,
    plot_left: f32,
    plot_w: f32,
    v0: View,
) {
    let plot = Rect::from_min_size(Pos2::new(plot_left, lane.top()), Vec2::new(plot_w, lane.height()));
    let resp = ui.interact(lane, ui.id().with("aox-act-pan"), Sense::click_and_drag());

    // Drag = pan (grab feel: drag right = see earlier)
    if resp.dragged() {
        let dx = resp.drag_delta().x;
        if dx != 0.0 {
            st.anchor = Anchor::Free;
            st.t0 = v0.t0 - dx / v0.scale;
        }
    }
    // Clicking blank space = deselect (when clicking a bar the click is taken by the bar, so this doesn't fire)
    if resp.clicked() {
        st.selected = None;
    }

    let (pointer, zoom, scroll, shift) = ctx.input(|i| {
        (
            i.pointer.hover_pos(),
            i.zoom_delta(),
            i.smooth_scroll_delta,
            i.modifiers.shift,
        )
    });
    let Some(p) = pointer else { return };
    if !lane.contains(p) {
        return;
    }

    if (zoom - 1.0).abs() > 1e-4 {
        // Ctrl+wheel: zoom anchored at the second under the cursor — whatever you point at stays put
        let t_anchor = v0.t_at(plot, p.x);
        let ns = (v0.scale * zoom).clamp(MIN_SCALE, MAX_SCALE);
        let keep = st.anchor == Anchor::Free;
        st.scale = Some(ns);
        if keep {
            st.t0 = t_anchor - (p.x - plot_left) / ns;
        }
    } else if scroll != Vec2::ZERO && !shift {
        // Wheel: pan horizontally, one scroll moves about 18% of the view.
        // While Shift is held this yields — the wheel is taken by the scroll area to page rows (see `timeline`).
        let vis = plot_w / v0.scale;
        let d = (scroll.x + scroll.y) / 50.0;
        st.anchor = Anchor::Free;
        st.t0 = v0.t0 - d * vis * 0.18;
    }
}

// ── Rows, bars, link lines ──

fn lanes(ui: &mut Ui, st: &mut Activity, rows: &[Row], plot: Rect, lane_h: f32, v: View) {
    let now = st.now;
    let tz = st.tz();
    let clip = ui.clip_rect();
    let visible = plot.intersect(clip);
    if visible.width() < 8.0 || visible.height() < 4.0 {
        return;
    }
    let step = lane_h + LANE_GAP;
    // Painter dedicated to the plot area: all bars/link lines are clipped to the plot area, so they don't
    // smear onto the right-side whitespace
    let p = ui.painter_at(plot);

    // ── Rows: baseline + row name ──
    for (i, row) in rows.iter().enumerate() {
        let y = plot.top() + i as f32 * step;
        if y > clip.bottom() || y + lane_h < clip.top() {
            continue;
        }
        p.line_segment(
            [Pos2::new(plot.left(), y + lane_h), Pos2::new(plot.right(), y + lane_h)],
            Stroke::new(1.0, P::border()),
        );
        // The row name is drawn in the left gutter with the outer painter (which is not clipped to the plot area)
        let name = row.label();
        let font =
            if row.dim() { FontId::proportional(13.5) } else { theme::mono_font(13.5) };
        let g = elide(ui, name, font, P::text_dim(), GUTTER - 32.0);
        let yc = (y + lane_h * 0.5).clamp(clip.top() + 5.0, clip.bottom() - 5.0);
        ui.painter().galley(
            Pos2::new(plot.left() - 12.0 - g.size().x, yc - g.size().y * 0.5),
            g,
            P::text_dim(),
        );
    }

    // ── Round separators (dashed): see at a glance when each round starts ──
    for a in st.acts.iter().filter(|a| a.kind == ActKind::Input) {
        let x = v.x(plot, a.at - tz);
        if x < plot.left() - 2.0 || x > plot.right() {
            continue;
        }
        let mut y = plot.top().max(clip.top());
        let bottom = plot.bottom().min(clip.bottom());
        while y < bottom {
            let y2 = (y + 4.0).min(bottom);
            p.line_segment([Pos2::new(x, y), Pos2::new(x, y2)], Stroke::new(1.0, P::border()));
            y = y2 + 5.0;
        }
    }

    // ── Geometry: computed once, reused by the link lines ──
    let shades = shades_of(st);
    struct Bar {
        row: usize,
        x0: f32,
        x1: f32,
        fill: Color32,
    }
    let mut bars: Vec<Bar> = Vec::with_capacity(st.acts.len());
    for a in st.acts.iter() {
        bars.push(Bar {
            row: row_of(rows, a),
            x0: v.x(plot, a.at - tz),
            x1: v.x(plot, a.end_t(now) - tz),
            fill: fill_of(a, &shades),
        });
    }
    let cy = |row: usize| plot.top() + row as f32 * step + lane_h * 0.5;
    let bar_rect = |b: &Bar| -> Rect {
        let l = b.x0.max(plot.left());
        let r = b.x1.max(l + MIN_BAR_W).min(plot.right());
        let y = plot.top() + b.row as f32 * step;
        Rect::from_min_max(Pos2::new(l, y), Pos2::new(r, y + lane_h))
    };

    // ── Link lines: joined head-to-tail in true execution order, coloured after the one it leaves ──
    if st.link {
        for w in bars.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            let p0 = Pos2::new(a.x1.max(a.x0), cy(a.row));
            let p1 = Pos2::new(b.x0, cy(b.row));
            if p0.x > plot.right() + 60.0 || p1.x < plot.left() - 60.0 {
                continue;
            }
            let g = ((p1.x - p0.x) * 0.45).clamp(5.0, 46.0);
            let c1 = Pos2::new(p0.x + g, p0.y);
            let c2 = Pos2::new(p1.x - g, p1.y);
            let mut pts = Vec::with_capacity(17);
            for i in 0..=16 {
                let t = i as f32 / 16.0;
                let u = 1.0 - t;
                pts.push(Pos2::new(
                    u * u * u * p0.x
                        + 3.0 * u * u * t * c1.x
                        + 3.0 * u * t * t * c2.x
                        + t * t * t * p1.x,
                    u * u * u * p0.y
                        + 3.0 * u * u * t * c1.y
                        + 3.0 * u * t * t * c2.y
                        + t * t * t * p1.y,
                ));
            }
            p.add(egui::Shape::line(pts, Stroke::new(1.5, a.fill.gamma_multiply(0.7))));
            p.circle_filled(p0, 1.7, a.fill.gamma_multiply(0.85));
        }
    }

    // ── Bars ──
    let mut hit: Option<u64> = None;
    for (i, a) in st.acts.iter().enumerate() {
        let Some(b) = bars.get(i) else { continue };
        let r = bar_rect(b);
        if r.right() < visible.left() || r.left() > visible.right() {
            continue;
        }
        p.rect_filled(r, CornerRadius::same(4), b.fill);
        // Still running: stroke a ring + a bright dot at the right end, so "not finished" is obvious at a glance
        if a.open() {
            p.rect_stroke(
                r,
                CornerRadius::same(4),
                Stroke::new(1.2, P::text()),
                egui::StrokeKind::Inside,
            );
            p.circle_filled(Pos2::new(r.right() - 2.0, r.center().y), 1.6, P::bg());
        }
        if st.selected == Some(a.seq) {
            p.rect_stroke(
                r.expand(1.5),
                CornerRadius::same(5),
                Stroke::new(1.8, P::ink()),
                egui::StrokeKind::Outside,
            );
        }

        let resp = ui.interact(r, ui.id().with(("bar", a.seq)), Sense::click());
        let hovered = resp.hovered();
        let clicked = resp.clicked();
        if hovered {
            p.rect_stroke(
                r.expand(1.0),
                CornerRadius::same(5),
                Stroke::new(1.2, P::ink_soft()),
                egui::StrokeKind::Inside,
            );
            let dur = a.dur(now);
            resp.on_hover_text(aox::tf!(
                "{}\n{} · {} → {} ({}s)",
                a.title,
                a.kind.label(),
                fmt_at(a.at - tz, 0.5),
                fmt_at(a.at - tz + dur, 0.5),
                format!("{:.2}", dur)
            ));
        }
        if clicked {
            hit = Some(a.seq);
        }
    }
    if let Some(seq) = hit {
        st.selected = Some(seq);
    }

    // ── "Now": only meaningful while a task is running ──
    if st.busy {
        let x = v.x(plot, st.now - tz);
        if x >= plot.left() && x <= plot.right() {
            p.line_segment(
                [Pos2::new(x, plot.top() - 2.0), Pos2::new(x, plot.bottom() + 2.0)],
                Stroke::new(1.0, P::ink().gamma_multiply(0.75)),
            );
        }
    }

    // ── Empty state: explain "why there is nothing here" ──
    if st.acts.is_empty() {
        ui.painter_at(visible).text(
            visible.center(),
            Align2::CENTER_CENTER,
            aox::i18n::t("Nothing is recorded while idle · after you send a command, the model and every tool call are laid out here by time"),
            FontId::proportional(14.5),
            P::text_faint(),
        );
    }
}

/// Plugin → shade index: tools of the same plugin use different shades of one colour family
fn shades_of(st: &Activity) -> Vec<(String, usize)> {
    let mut v: Vec<(String, usize)> = Vec::new();
    for a in &st.acts {
        if a.kind == ActKind::Tool
            && !a.plugin.is_empty()
            && !v.iter().any(|(t, _)| *t == a.plugin)
        {
            v.push((a.plugin.clone(), v.len()));
        }
    }
    v
}

fn fill_of(a: &Act, shades: &[(String, usize)]) -> Color32 {
    let base = if a.failed { ActKind::failed_color() } else { a.kind.color() };
    if a.kind == ActKind::Tool && !a.plugin.is_empty() {
        let idx = shades.iter().find(|(t, _)| *t == a.plugin).map_or(0, |(_, i)| *i);
        shade(base, idx)
    } else {
        base
    }
}

/// Layer lightness by index: tool calls of the same plugin use one colour family at different shades
fn shade(base: Color32, idx: usize) -> Color32 {
    const STEPS: [f32; 5] = [0.0, 0.22, 0.40, -0.18, 0.56];
    let k = STEPS[idx % STEPS.len()];
    if k >= 0.0 {
        base.lerp_to_gamma(P::text(), k)
    } else {
        base.lerp_to_gamma(P::panel(), -k)
    }
}

/// Take the closest tick interval among 1/2/5×10ⁿ, so ticks don't crowd together
fn nice_step(rough: f32) -> f32 {
    const STEPS: [f32; 20] = [
        0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0,
        600.0, 900.0, 1800.0, 3600.0, 7200.0,
    ];
    STEPS.iter().copied().find(|s| *s >= rough).unwrap_or(7200.0)
}

/// Text representation of a moment. Decimal places follow the tick granularity, so a 0.01s tick doesn't show "0.0s".
fn fmt_at(t: f32, step: f32) -> String {
    let t = t.max(0.0);
    if t < 60.0 {
        if step < 0.1 {
            format!("{t:.2}s")
        } else if step < 1.0 {
            format!("{t:.1}s")
        } else {
            format!("{t:.0}s")
        }
    } else if t < 3600.0 {
        format!("{}:{:02}", (t / 60.0) as i32, (t % 60.0) as i32)
    } else {
        format!("{}:{:02}", (t / 3600.0) as i32, ((t % 3600.0) / 60.0) as i32)
    }
}

fn text_w(ui: &Ui, font: &FontId, s: &str) -> f32 {
    let mut job = egui::text::LayoutJob::default();
    job.append(s, 0.0, TextFormat { font_id: font.clone(), ..Default::default() });
    ui.fonts_mut(|f| f.layout_job(job).size().x)
}

/// Clamp single-line text by **pixels**: elide with an ellipsis when it doesn't fit (row names and tool names can be long)
fn elide(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> Arc<Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        TextFormat { font_id: font, color, ..Default::default() },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: max_w.max(20.0),
        max_rows: 1,
        // CJK has no word boundaries; without this, "the whole thing doesn't fit" would simply not elide
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

// ══════════════════════ Below: details of the selected item ══════════════════════

fn detail(ui: &mut Ui, st: &mut Activity, h: f32) {
    let inner_h = (h - 34.0).max(80.0);
    Frame::NONE
        .fill(P::surface())
        .corner_radius(CornerRadius::same(theme::card_radius()))
        .stroke(Stroke::new(1.0, P::border()))
        .inner_margin(Margin::symmetric(20, 16))
        .show(ui, |ui| {
            let w = ui.available_width();
            ui.set_width(w);
            ui.set_max_width(w);
            ui.set_min_height(inner_h);
            ui.set_max_height(inner_h);
            ui.spacing_mut().item_spacing.y = 0.0;

            let Some(a) = st.focused() else {
                ui.add_space(4.0);
                ui.label(RichText::new(aox::i18n::t("No step selected yet")).color(P::text_dim()).size(15.0));
                ui.add_space(7.0);
                ui.label(
                    RichText::new(aox::i18n::t("Nothing is recorded while the app is idle; after you send a command, every step is laid out here by time."))
                        .color(P::text_faint())
                        .size(14.0),
                );
                return;
            };

            // ── Header: category badge + round + title ──
            let (head, _) = ui.allocate_exact_size(Vec2::new(w, 24.0), Sense::hover());
            {
                let p = ui.painter();
                let text = if a.failed { ActKind::Error } else { a.kind };
                let font = FontId::proportional(13.0);
                let tw = text_w(ui, &font, text.label());
                let br = Rect::from_min_size(
                    Pos2::new(head.left(), head.top() + 1.0),
                    Vec2::new(tw + 18.0, 22.0),
                );
                p.rect_filled(br, CornerRadius::same(6), text.color());
                p.text(br.center(), Align2::CENTER_CENTER, text.label(), font, P::bg());

                let mut x = br.right() + 12.0;
                let meta = aox::tf!("Round {}", a.round);
                p.text(
                    Pos2::new(x, head.center().y),
                    Align2::LEFT_CENTER,
                    &meta,
                    FontId::proportional(13.5),
                    P::text_faint(),
                );
                x += text_w(ui, &FontId::proportional(13.5), &meta) + 14.0;

                // Title: monospaced identifiers use the monospace font
                let font = if a.kind.mono() {
                    theme::mono_font(15.5)
                } else {
                    FontId::proportional(17.0)
                };
                let room = head.right() - x;
                let g = elide(ui, &a.title, font, P::text(), room);
                p.galley(Pos2::new(x, head.center().y - g.size().y * 0.5), g, P::text());

                // Right end: time and duration
                let dur = a.dur(st.now);
                let when = if a.open() {
                    aox::tf!("{} started · running {}s", fmt_at(a.at - st.tz(), 0.5), format!("{:.2}", dur))
                } else {
                    format!(
                        "{} → {} · {dur:.2}s",
                        fmt_at(a.at - st.tz(), 0.5),
                        fmt_at(a.at - st.tz() + dur, 0.5)
                    )
                };
                let wf = theme::mono_font(12.5);
                let ww = text_w(ui, &wf, &when);
                p.text(
                    Pos2::new(head.right() - ww, head.center().y),
                    Align2::LEFT_CENTER,
                    when,
                    wf,
                    P::text_faint(),
                );
            }

            ui.add_space(10.0);
            let (rule, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
            ui.painter().hline(rule.x_range(), rule.center().y, Stroke::new(1.0, P::border()));
            ui.add_space(10.0);

            // ── Body ──
            let body = a.body.trim().to_owned();
            let avail = (inner_h - 46.0).max(40.0);
            if body.is_empty() {
                ui.label(RichText::new(aox::i18n::t("(no more information)")).color(P::text_faint()).size(14.5));
                return;
            }
            let font = if a.kind.mono() {
                theme::mono_font(14.5)
            } else {
                FontId::proportional(16.0)
            };
            let col = if a.failed { ActKind::failed_color() } else { P::text() };
            ScrollArea::vertical()
                .id_salt("aox-act-detail")
                .max_height(avail)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_max_width(ui.available_width());
                    let mut job = egui::text::LayoutJob::default();
                    job.wrap.max_width = ui.available_width();
                    job.wrap.break_anywhere = a.kind.mono();
                    for (i, seg) in body.split('\n').enumerate() {
                        if i > 0 {
                            job.append(
                                "\n",
                                0.0,
                                TextFormat { font_id: font.clone(), ..Default::default() },
                            );
                        }
                        job.append(
                            seg,
                            0.0,
                            TextFormat { font_id: font.clone(), color: col, ..Default::default() },
                        );
                    }
                    ui.add(
                        egui::Label::new(job)
                            .selectable(true)
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                    ui.add_space(8.0);
                });
        });
}

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Export → load back → keep chatting. Three easy-to-get-wrong spots on this chain, pinned together:
    /// the same record exported twice (the watermark not advancing), a loaded record treated as new and written again
    /// (the watermark not aligned to the save), and a new round not continuing after the save.
    #[test]
    fn acts_round_trip_and_flush_once() {
        let mut a = Activity::default();
        a.round_begin("list the directory", 0.0); // input
        a.model_begin(0.5); // model
        a.tool_begin("fs_ls", Some("fs"), "{}", 1.7); // tool
        a.tool_end(false, "3 items", 2.0);
        a.end_round(2.4);
        a.note(ActKind::Info, "Info", "x", 2.4); // system

        let first = a.export_acts();
        assert_eq!(first.len(), 4, "one each of input / model / tool / system");
        assert!(a.export_acts().is_empty(), "the same records must not be exported twice");

        // Second round: only that round's one record is newly exported
        a.round_begin("write another file", 3.0);
        a.end_round(3.4);
        assert_eq!(a.export_acts().len(), 1);

        // Load it back
        let mut b = Activity::default();
        b.load_acts(&first, 100.0);
        assert_eq!(b.acts.len(), 4);
        assert_eq!(b.round, 1);
        assert_eq!(b.next_seq, 4);
        assert_eq!(b.flushed, 4, "loaded records were already in the file; the watermark must line up");
        assert!(b.export_acts().is_empty(), "loaded records must not be rewritten as new");
        // Shift the whole batch to "just now": the last entry ends slightly before now
        let last = b.acts.back().unwrap();
        assert!(last.end_t(100.0) < 100.0 && last.end_t(100.0) > 90.0);
        // Keep chatting: the round count continues and the new round grows in from the right
        b.round_begin("continue", 100.0);
        assert_eq!(b.round, 2);
    }

    /// Bad lines in a save, or an unknown category written by another version, must not wreck the whole timeline.
    #[test]
    fn load_acts_skips_bad_rows() {
        let mut a = Activity::default();
        let good = serde_json::json!({
            "kind": "tool", "round": 1, "plugin": "fs", "at": 1.0, "end": 2.0,
            "failed": false, "title": "fs_ls", "body": "", "seq": 7
        });
        a.load_acts(&[serde_json::json!({"nope": 1}), good], 50.0);
        assert_eq!(a.acts.len(), 1);
        assert_eq!(a.next_seq, 7);
        assert_eq!(a.flushed, 7);
    }
}
