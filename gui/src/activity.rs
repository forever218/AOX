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
//! same size as the main window). Everything that makes that work — the self-drawn
//! top bar, dragging, eight-way resize, the DWM rounded corners, the 1px outline,
//! and the poke that keeps the main window painting — lives in [`crate::gwin`],
//! shared with the cache-hit window (`cache.rs`). This module draws the
//! **timeline** and nothing about the window itself.
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

use std::collections::VecDeque;

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, Pos2, Rect, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Ui, Vec2, ViewportId,
};
use serde::{Deserialize, Serialize};

use crate::gwin::{self, Gwin};
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
    /// as code highlighting: input is gray, model is purple, the rest (auth / notice / error)
    /// sit in warm yellow and brick red.
    ///
    /// **Tool blocks never reach the `Tool` arm below**: [`fill_of`] routes them to
    /// [`tool_color`] first, so each distinct tool gets its own stable colour instead of every
    /// call sharing this one flat orange.
    fn color(self) -> Color32 {
        match theme::mode() {
            theme::Mode::Light => match self {
                ActKind::Input => Color32::from_rgb(0x6E, 0x69, 0x5E),
                ActKind::Model => Color32::from_rgb(0x76, 0x57, 0x9C),
                // Only a last-resort fallback — see the note on this function
                ActKind::Tool => Color32::from_rgb(0xB3, 0x74, 0x22),
                ActKind::Auth => Color32::from_rgb(0x9A, 0x6C, 0x1C),
                ActKind::Info => Color32::from_rgb(0x77, 0x71, 0x64),
                ActKind::Warn => Color32::from_rgb(0xA8, 0x76, 0x20),
                ActKind::Error => Color32::from_rgb(0xA3, 0x4A, 0x2A),
            },
            theme::Mode::Dark => match self {
                ActKind::Input => Color32::from_rgb(0x8E, 0x8A, 0x80),
                ActKind::Model => Color32::from_rgb(0xB2, 0x93, 0xD6),
                // Only a last-resort fallback — see the note on this function
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
    /// Plugin name (present for tool calls). Kept purely as metadata (and as part of the on-disk
    /// format) — the block's colour is derived from the tool name itself these days, see
    /// [`tool_color`].
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
    /// The block the user clicked: it draws the highlight ring, and while it is on screen its
    /// detail popover floats beside it (see `popover`).
    selected: Option<u64>,
    /// The selected bar's screen rect, recorded by `lanes` as it draws. `None` means the selected
    /// node is scrolled out of view, so there is nothing to anchor the popover to — in that case
    /// no popover is shown at all. Pure view state; never persisted.
    pop_anchor: Option<Rect>,

    /// The window shell: open / pinned / busy / breathing-dot phase, plus the "do once per window"
    /// flags. Shared verbatim with the cache-hit window — see `gwin`.
    gw: Gwin,
    status: String,
    /// The main window's clock reading when the current busy stretch began; `None` = idle. The
    /// header's "· 12.3s" readout is `now - busy_base`.
    ///
    /// This used to hold an `Instant`, i.e. a **second, independent clock** — and the two
    /// disagreed: the main window's clock stops while the user is answering a question, this one
    /// didn't, so the header could calmly report a turn as having run for several minutes while
    /// the chart right under it showed a few seconds of work. Deriving both readouts from the one
    /// time base the main window hands over ([`Activity::tick`]) is what keeps them in step.
    busy_base: Option<f32>,
    /// The main window's time base (seconds), used to compute real-time length for "in-progress" blocks
    now: f32,

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
        if busy && !self.gw.busy {
            self.busy_base = Some(now);
        }
        if !busy {
            self.busy_base = None;
        }
        self.gw.busy = busy;
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
        self.pop_anchor = None;
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
        self.gw.is_open()
    }

    /// Open the window directly (used for the demo-mode preview; the normal path is [`open`]).
    pub fn force_open(&mut self) {
        self.gw.force_open();
    }

    /// The node the user clicked, if it is still in the record.
    fn selected_act(&self) -> Option<&Act> {
        let seq = self.selected?;
        self.acts.iter().rev().find(|a| a.seq == seq)
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

    /// Per-round `[start, end]` in axis seconds, indexed by round number (`[0]` is unused; the
    /// rounds shown to the user are numbered from 1).
    ///
    /// Folded out of the records rather than kept in a table, for the same reason as [`rows_of`]:
    /// the answer is fully determined by the records, so a second copy would only add a way for the
    /// two to disagree (a dropped front, a rolled-back turn). The vector is indexed by round so the
    /// ruler can walk one round's ticks at a time without rescanning every record per tick.
    fn round_spans(&self) -> Vec<(f32, f32)> {
        let tz = self.tz();
        let mut v = vec![(f32::MAX, f32::MIN); self.round as usize + 1];
        for a in &self.acts {
            if let Some(s) = v.get_mut(a.round as usize) {
                s.0 = s.0.min(a.at - tz);
                s.1 = s.1.max(a.end_t(self.now) - tz);
            }
        }
        v
    }

    /// Axis-seconds at which round `r` began (`0.0` when the round has no records left, which is
    /// what happens after the front of the stream is dropped once it passes [`ACT_LIMIT`]).
    ///
    /// This is the origin for "how far into this round did this step happen" — what the ruler
    /// labels and the hover/popover readouts count from, so every round reads from 0.
    fn round_start(&self, r: u32) -> f32 {
        let tz = self.tz();
        let mut lo = f32::MAX;
        for a in &self.acts {
            if a.round == r {
                lo = lo.min(a.at - tz);
            }
        }
        if lo == f32::MAX {
            0.0
        } else {
            lo
        }
    }

    /// Aim the view at round `r`
    fn fit_round(&mut self, r: u32, plot_w: f32) {
        let tz = self.tz();
        let now = self.now;
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for a in &self.acts {
            if a.round != r {
                continue;
            }
            lo = lo.min(a.at - tz);
            hi = hi.max(a.end_t(now) - tz);
        }
        if lo > hi {
            return;
        }
        let pad = ((hi - lo) * 0.10).clamp(0.25, 8.0);
        self.fit_range(lo - pad, hi + pad, plot_w);
        // Deliberately does **not** touch `selected`: the popover belongs to a node the user
        // clicked, and having a "jump to round" press pop one open would be noise.
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
/// (see [`gwin::set_round_corners`]), so there must be exactly one copy.
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
    lock(a).gw.open(ctx);
}

/// Called once per frame by the main window.
///
/// egui's viewports are "declarative": **a child viewport not declared this round is simply dropped**.
/// So even while the window is minimized and untouched, it must be re-declared every frame.
pub fn sync(ctx: &egui::Context, a: &Shared) {
    let mut st = lock(a);
    if !st.gw.open {
        // Closing while a turn is running: the window callback stops here, so it would never get to
        // withdraw this window's claim on the poker — and a claim left standing would keep the main
        // window repainting at full rate forever.
        drop(st);
        gwin::want(gwin::WIN_ACTIVITY, false);
        return;
    }
    // Size, position and the "report only what never changes" rule all live in
    // [`gwin::Gwin::builder`] — this window only supplies what is its own: the title and how small
    // it may get.
    let size = gwin::initial_size(ctx);
    let b = st.gw.builder(ctx, WINDOW_TITLE, size, Vec2::new(MIN_W, MIN_H), Vec2::ZERO);

    let a2 = a.clone();
    drop(st);
    ctx.show_viewport_deferred(act_id(), b, move |ctx, _| window(ctx, &a2));
}

// ═══════════════════════════ Layout metrics ═══════════════════════════

/// Bounds of the row-name gutter on the left. Its width is **no longer a constant**: it is measured
/// from this frame's actual rows (see `gutter_w`), so a timeline whose names are all short hugs the
/// left edge instead of leaving a fixed ~180-point blank strip. These are only the floor (the
/// column never collapses) and the ceiling (past it, names are elided).
const GUTTER_MIN: f32 = 64.0;
const GUTTER_MAX: f32 = 182.0;
/// Nominal size of the click-a-node popover. It shrinks to fit when the window is small, and the
/// body inside scrolls — the popover itself never grows past this.
const POP_W: f32 = 384.0;
const POP_H: f32 = 300.0;
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


// ─────────────────────────── The child window's content ───────────────────────────

fn window(ctx: &egui::Context, shared: &Shared) {
    let mut st = lock(shared);

    // Everything about the window itself — system-level close, always-on-top, the native theme, DWM
    // rounded corners — belongs to the shell (`gwin`); what is left here is the timeline.
    st.gw.prelude(ctx, WINDOW_TITLE);

    let frame = Frame::NONE.fill(P::panel());
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        // Child and main viewports share one `egui::Context`, so widget Ids are **not** automatically prefixed with viewport info.
        ui.push_id("aox-activity-window", |ui| {
            body(ui, ctx, &mut st);
        });
    });

    // The epilogue is split in two on purpose: `step` advances the breathing dot and decides what
    // this frame wants, then the lock is **released** before `drive` pokes the main window. That
    // poke lands as a `WM_PAINT` on the main thread, whose own pass takes this same lock — running it
    // while still holding the lock would stall the main window until this frame finished.
    let d = st.gw.step();
    drop(st);
    gwin::drive(ctx, gwin::WIN_ACTIVITY, d);
}

fn body(ui: &mut Ui, ctx: &egui::Context, st: &mut Activity) {
    // The whole window's rect. **Must be taken before putting anything into the ui**: `max_rect()` shrinks
    // with each allocate, and by the end of the function only a sliver at the bottom would be left.
    let full = ui.max_rect();

    // The top bar's state readout is built here rather than inside `gwin::header`: the shell has no
    // idea what a given window is busy with, and "which round, how many records" is this window's
    // own vocabulary.
    let status = if st.status.is_empty() { aox::i18n::t("Ready") } else { st.status.as_str() };
    let info = if st.gw.busy {
        format!("{status} · {:.1}s", st.busy_base.map_or(0.0, |b| (st.now - b).max(0.0)))
    } else if st.round == 0 {
        aox::i18n::t("Idle · nothing is recorded").to_string()
    } else {
        aox::tf!("Idle · {} rounds · {} records", st.round, st.acts.len())
    };
    gwin::header(ui, ctx, &mut st.gw, aox::i18n::t("Activity Stream"), &info);

    Frame::NONE.inner_margin(Margin { left: 18, right: 18, top: 12, bottom: 14 }).show(ui, |ui| {
        // All vertical spacing inside this block is added by us, so egui's item_spacing doesn't skew the height
        ui.spacing_mut().item_spacing.y = 0.0;

        toolbar(ui, st);
        ui.add_space(14.0);

        // The row list must be known first: the timeline lays rows out in this order, and the
        // number of tool rows decides whether vertical scrolling takes over (see `timeline`).
        let rows = rows_of(&st.acts);
        let rest = ui.available_height();
        // The timeline now owns **all** the remaining height. The big bottom detail card is gone —
        // a step's content moved into a small popover beside the bar (`popover`), so there is
        // nothing left to reserve room for, and the chart stops being squeezed into the top half.
        // Taking the full height also means the blank area below the last row stays part of the
        // timeline: clicking it still deselects, and dragging it still pans.
        let chart_h = rest.max(170.0);

        timeline(ui, ctx, st, &rows, chart_h, full);
    });

    // The node popover floats above everything, so it is declared after the panel's own content
    // (and after the layout above has finished, so it never disturbs it).
    popover(ui, st, full);

    // Finally overlay two "window-level" things: the border drag hot-zones (which must beat the top bar's drag zone, so they must be
    // declared last) and the 1px outline on the outermost ring.
    gwin::resize_handles(ui, ctx, full);
    gwin::outline(ui, full);
}



// ───────────────────────────── Top bar ─────────────────────────────


// ───────────────────────────── Toolbar ─────────────────────────────

/// A row of view tools: the left half is "which stretch to look at", the right half is "how to view it".
fn toolbar(ui: &mut Ui, st: &mut Activity) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;

        // ── Anchors: pin a moment to some position on screen ──
        if gwin::pill(ui, aox::i18n::t("Start"), st.anchor == Anchor::Start)
            .on_hover_text(aox::i18n::t("Pin the start of the first round to the left edge"))
            .clicked()
        {
            st.anchor = Anchor::Start;
        }
        if gwin::pill(ui, aox::i18n::t("Center"), st.anchor == Anchor::Center)
            .on_hover_text(aox::i18n::t("Keep the current moment centered, visible on both sides"))
            .clicked()
        {
            st.anchor = Anchor::Center;
        }
        if gwin::pill(ui, aox::i18n::t("Latest"), st.anchor == Anchor::Latest)
            .on_hover_text(aox::i18n::t("Follow the latest progress"))
            .clicked()
        {
            st.anchor = Anchor::Latest;
        }

        gwin::vsep(ui);

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
        if gwin::pill(ui, aox::i18n::t("Go"), false).clicked() || entered {
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
            if gwin::pill(ui, "+", false)
                .on_hover_text(aox::i18n::t("Row height +: stretch each row; when it overflows, scroll with Shift+wheel"))
                .clicked()
            {
                st.pending_row = Some(1.25);
            }
            if gwin::pill(ui, "−", false).on_hover_text(aox::i18n::t("Row height −: squeeze each row to fit more on screen")).clicked()
            {
                st.pending_row = Some(1.0 / 1.25);
            }
            ui.label(RichText::new(aox::i18n::t("Row height")).color(P::text_faint()).size(13.5));
            gwin::vsep(ui);

            if gwin::pill(ui, "+", false).on_hover_text(aox::i18n::t("Zoom in (horizontal axis)")).clicked() {
                st.pending_zoom = Some(1.35);
            }
            if gwin::pill(ui, "−", false).on_hover_text(aox::i18n::t("Zoom out (horizontal axis)")).clicked() {
                st.pending_zoom = Some(1.0 / 1.35);
            }
            ui.label(RichText::new(aox::i18n::t("Zoom")).color(P::text_faint()).size(13.5));
            gwin::vsep(ui);

            if gwin::pill(ui, aox::i18n::t("Fit"), false)
                .on_hover_text(aox::i18n::t("Fill the window with the whole span and hand row height back to auto"))
                .clicked()
            {
                st.fit_all = true;
            }
            if gwin::pill(ui, aox::i18n::t("Link lines"), st.link)
                .on_hover_text(aox::i18n::t("Join the blocks head-to-tail in true execution order; each line takes the color of the block it leaves"))
                .clicked()
            {
                st.link = !st.link;
            }
        });
    });
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

fn timeline(
    ui: &mut Ui,
    ctx: &egui::Context,
    st: &mut Activity,
    rows: &[Row],
    avail_h: f32,
    full: Rect,
) {
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

        // The plot area's left edge and width: axis, row names and bars all align to it.
        // The row-name column is measured from this frame's rows rather than fixed, so a timeline
        // whose names are short hugs the left edge instead of leaving a wide blank strip.
        let gutter = gutter_w(ui, rows);
        let plot_left = band.left() + gutter;
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
        input(ui, ctx, st, lane_rect, plot_left, plot_w, v0, full);
        let v = View::compute(st, plot_w);

        // ── Time scale band ──
        //
        // The ruler counts **each round from 0** instead of running on from the first record for
        // ever. The stream is a sequence of rounds, not one long process, so the number worth
        // reading is always "how far into *this* round" — a cumulative reading just drifts into
        // "17:03" a few rounds in and stops saying anything about the step you are looking at.
        //
        // Because the clock stops between rounds (see `Clock` in the main window), rounds are laid
        // flush end to end: a round's 0 tick lands exactly on the previous round's final tick, so
        // one of the two labels would double up there. The tick is cheap and always drawn; the
        // *label* is what is skipped when it would collide with the one before it.
        let base_y = band.bottom() - 1.0;
        {
            let p = ui.painter();
            p.line_segment(
                [Pos2::new(plot_left, base_y), Pos2::new(plot_left + plot_w, base_y)],
                Stroke::new(1.0, P::border()),
            );
            let step = nice_step(90.0 / v.scale);
            let right = plot_left + plot_w;
            let font = theme::mono_font(12.0);
            // Right edge of the last label drawn; while a new one would overlap it, it is skipped.
            let mut last_label = f32::MIN;
            for &(rs, re) in st.round_spans().iter() {
                if rs > re {
                    continue; // no records left for this round (dropped / rolled back)
                }
                // The round's first tick that can be on screen: never negative (this round's own 0
                // is the origin) and never left of the edge.
                let mut local = (((v.t_at(axis, plot_left) - rs) / step).floor()).max(0.0) * step;
                while rs + local <= re + 1.0e-3 {
                    let x = v.x(axis, rs + local);
                    if x > right + step {
                        break;
                    }
                    if x >= plot_left - 0.5 {
                        let label = fmt_at(local, step);
                        let lw = gwin::text_w(ui, &font, &label);
                        p.line_segment(
                            [Pos2::new(x, base_y - 6.0), Pos2::new(x, base_y)],
                            Stroke::new(1.0, P::border_strong()),
                        );
                        if x - lw * 0.5 > last_label + 4.0 {
                            last_label = x + lw * 0.5;
                            p.text(
                                Pos2::new(x, base_y - 17.0),
                                Align2::CENTER_CENTER,
                                &label,
                                font.clone(),
                                P::text_faint(),
                            );
                        }
                    }
                    local += step;
                }
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
                let tw = gwin::text_w(ui, &theme::mono_font(12.0), &text) + 14.0;
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
                    Pos2::new(content.left() + gutter, content.top()),
                    Pos2::new(content.left() + gutter + plot_w, content.bottom()),
                );
                lanes(ui, st, rows, plot, lane_h, v, gutter);
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
    full: Rect,
) {
    let plot = Rect::from_min_size(Pos2::new(plot_left, lane.top()), Vec2::new(plot_w, lane.height()));
    let resp = ui.interact(lane, ui.id().with("aox-act-pan"), Sense::click_and_drag());

    // Esc dismisses the node popover from anywhere over the timeline.
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        st.selected = None;
    }

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

    // The popover floats over the timeline, and its content is meant to be read with the wheel:
    // while the pointer is inside it the wheel belongs to that content alone and the axis behind
    // must hold still. egui hands a wheel to whichever widget consumes it, but `popover` is drawn
    // *after* this runs, so it cannot take the delta out of our hands in time — hence the guard
    // here. (The anchor read is last pass's, since `popover` recomputes `pop_anchor` later in this
    // same pass; for a settled selection the two are the same rect, and a one-frame lag on the
    // very frame the selection changes is harmless — a click and a wheel together are not a thing.)
    if let Some(anchor) = st.pop_anchor {
        if popover_rect(anchor, full).contains(p) {
            // Shift is only ever a "make it vertical" hint in this window; egui has already
            // collapsed it into a horizontal-only delta, so put it back for the popover's own
            // vertical scroll area to read.
            if shift && scroll != Vec2::ZERO {
                ctx.input_mut(|i| {
                    i.smooth_scroll_delta.y += i.smooth_scroll_delta.x;
                    i.smooth_scroll_delta.x = 0.0;
                });
            }
            return;
        }
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
    } else if shift {
        // Shift+wheel: page the rows.
        //
        // "Just let the scroll area have the wheel" is not enough, because by the time we see the
        // event egui has already rewritten it: `Options::horizontal_scroll_modifier` is **SHIFT** by
        // default, so `InputState` collapses the wheel into a horizontal-only delta
        // (`vec2(x + y, 0.0)`). A vertical-only `ScrollArea` reads `smooth_scroll_delta.y` alone
        // (egui's `always_scroll_the_only_direction` is off by default), so it read a permanent zero
        // and the rows never moved. Put the delta back on the axis it actually reads — it is ours to
        // move: nothing else in this window wants the wheel, and the scroll area clears the delta
        // once it has consumed it.
        if scroll != Vec2::ZERO {
            ctx.input_mut(|i| {
                i.smooth_scroll_delta.y += i.smooth_scroll_delta.x;
                i.smooth_scroll_delta.x = 0.0;
            });
        }
    } else if scroll != Vec2::ZERO {
        // Wheel: pan horizontally, one scroll moves about 18% of the view.
        // While Shift is held this yields — the wheel is handed to the scroll area to page rows (above).
        let vis = plot_w / v0.scale;
        let d = (scroll.x + scroll.y) / 50.0;
        st.anchor = Anchor::Free;
        st.t0 = v0.t0 - d * vis * 0.18;
    }
}

// ── Rows, bars, link lines ──

/// Where the hand-off between two consecutive records is drawn, and how wide the bezier's control
/// offset should be — or `None` when the pair should not be linked at all.
///
/// Pulled out of the drawing loop so the rules can be pinned by a test rather than re-derived by
/// eye from a screenshot. `head` / `tail` are the two ends of the earlier bar, `next` the head of
/// the later one.
///
/// **Leave from the tail** — records run left to right, and the tail is where the hand-off happens.
/// The exception is a pair that **overlaps in time**: records are stored in *write* order, and a
/// system note (an authorization request, say) is written while the tool bar it belongs to is still
/// running, so that bar's tail lands *after* the note's own position. Leaving from the tail there
/// would draw a line running **backwards** through the chart. Joining the two **heads** instead
/// keeps every hand-off pointing forwards, and it is the honest reading anyway ("the tool started,
/// then this was noted").
///
/// **Both ends off screen → no link.** The painter clips to the visible area, so what reached the
/// screen was the *middle* of a long segment with no endpoint anywhere in sight: a stray diagonal
/// floating across the chart. This is the "long flow, small view" case — rows scrolled out of sight
/// and bars off both ends — and why such marks disappeared again as soon as the view was zoomed
/// back in far enough to bring one end into view.
///
/// **The offset is proportional, with no floor.** The offset used to be clamped up to a 5pt
/// minimum, so two bars sitting essentially on top of each other still got a 10pt-wide S-wiggle —
/// and *that* pair is not the exception in this stream, it is the rule: `tool_begin` closes the
/// model segment at exactly the tool's own start time (and `model_begin` closes the input the same
/// way), so most hand-offs share one timestamp. Zoomed out, a compressed stretch of them became a
/// field of little loops; zoomed in, the same spans are wide enough to look like the smooth
/// connectors they are meant to be. With the floor gone a zero-width hand-off comes out as the
/// straight line the eye expects, and a *negative* one degenerates to the same straight line rather
/// than folding into a loop.
fn link_offset(head: Pos2, tail: Pos2, next: Pos2, visible: Rect) -> Option<(Pos2, f32)> {
    let from = if tail.x <= next.x { tail } else { head };
    if !visible.contains(from) && !visible.contains(next) {
        return None;
    }
    Some((from, ((next.x - from.x) * 0.45).clamp(0.0, 46.0)))
}

fn lanes(
    ui: &mut Ui,
    st: &mut Activity,
    rows: &[Row],
    plot: Rect,
    lane_h: f32,
    v: View,
    gutter: f32,
) {
    let now = st.now;
    let tz = st.tz();
    // Cleared up front: if this pass bails out (or the selected bar is scrolled away) the popover
    // must not keep pointing at a stale rect from an earlier frame.
    st.pop_anchor = None;
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
        let g = gwin::elide(ui, name, font, P::text_dim(), (gutter - 24.0).max(24.0));
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
            fill: fill_of(a),
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
    //
    // **Every** consecutive pair gets a line. The only pair that may go unlinked is one whose two
    // ends are both outside the visible area (a link you could not see the ends of anyway — see
    // `link_offset`), which in practice means the hand-off from the end of one round to the start
    // of the next once the view is zoomed in past that gap.
    //
    // Which end of the first bar the line leaves from, and how wide the curve is, is decided in
    // [`link_offset`] rather than inline here, so the rules can be tested.
    if st.link {
        let vis = visible.expand(6.0);
        for w in bars.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            let head = Pos2::new(a.x0, cy(a.row));
            let tail = Pos2::new(a.x1.max(a.x0), cy(a.row));
            let p1 = Pos2::new(b.x0, cy(b.row));
            let Some((p0, g)) = link_offset(head, tail, p1, vis) else { continue };
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
    // Where the selected bar ended up on screen, for `popover` to sit next to. Collected locally
    // and written back after the walk, because the walk borrows `st.acts` immutably.
    let mut pop_anchor: Option<Rect> = None;
    let sel = st.selected;
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
        if sel == Some(a.seq) {
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
            // Times are read **within the round** (see the ruler note in `timeline`), so the two
            // numbers match the axis above instead of being a global reading the ruler no longer
            // shows.
            let rs = st.round_start(a.round);
            resp.on_hover_text(aox::tf!(
                "{}\n{} · {} → {} ({}s)",
                a.title,
                a.kind.label(),
                fmt_at(a.at - tz - rs, 0.5),
                fmt_at(a.at - tz - rs + dur, 0.5),
                format!("{:.2}", dur)
            ));
        }
        if clicked {
            hit = Some(a.seq);
        }
        // Anchor the popover on whichever bar is — or just became — the selection, but only while
        // it is genuinely on screen (a scrolled-away node gets no popover rather than a stray one).
        let on_screen = r.bottom() > visible.top() && r.top() < visible.bottom();
        if (sel == Some(a.seq) || clicked) && on_screen {
            pop_anchor = Some(r);
        }
    }
    if let Some(seq) = hit {
        st.selected = Some(seq);
    }
    st.pop_anchor = pop_anchor;

    // ── "Now": only meaningful while a task is running ──
    if st.gw.busy {
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

/// The colour of one block.
///
/// Tool calls are the interesting case: they used to share a single orange, which made a row of
/// tool blocks read as one undifferentiated band. Now **each distinct tool gets its own colour**,
/// picked by hashing the tool name, so `read_file` is always the same hue and two different tools
/// are told apart at a glance. Failed calls stay brick red, overriding everything.
fn fill_of(a: &Act) -> Color32 {
    if a.failed {
        return ActKind::failed_color();
    }
    if a.kind == ActKind::Tool {
        return tool_color(&a.title);
    }
    a.kind.color()
}

/// Stable per-tool colour: a hash of the tool name indexes a small fixed palette.
///
/// Deliberately hashed rather than drawn fresh each frame — a literally random colour would strobe
/// as the timeline redraws, and would not survive remembering "the green one was the shell call".
/// The palette is kept in the same low-saturation family as the rest of the chart, just spread
/// across hues.
fn tool_color(name: &str) -> Color32 {
    const LIGHT: [Color32; 10] = [
        Color32::from_rgb(0xB3, 0x74, 0x22), // orange
        Color32::from_rgb(0x2F, 0x6B, 0x9E), // blue
        Color32::from_rgb(0x3F, 0x7A, 0x5E), // green
        Color32::from_rgb(0x8B, 0x5A, 0x2B), // brown
        Color32::from_rgb(0x6B, 0x4E, 0x9E), // violet
        Color32::from_rgb(0x9A, 0x54, 0x6E), // plum
        Color32::from_rgb(0x53, 0x6E, 0x2A), // olive
        Color32::from_rgb(0x8A, 0x6D, 0x17), // amber
        Color32::from_rgb(0x2B, 0x6B, 0x6B), // teal
        Color32::from_rgb(0x7A, 0x4B, 0x4B), // clay
    ];
    const DARK: [Color32; 10] = [
        Color32::from_rgb(0xD3, 0x94, 0x5A), // orange
        Color32::from_rgb(0x74, 0xA9, 0xE0), // blue
        Color32::from_rgb(0x7F, 0xC0, 0xA0), // green
        Color32::from_rgb(0xC9, 0x9A, 0x6B), // brown
        Color32::from_rgb(0xB2, 0x93, 0xD6), // violet
        Color32::from_rgb(0xE0, 0x9A, 0xB0), // plum
        Color32::from_rgb(0x9E, 0xC0, 0x74), // olive
        Color32::from_rgb(0xD8, 0xBE, 0x6A), // amber
        Color32::from_rgb(0x74, 0xC0, 0xC0), // teal
        Color32::from_rgb(0xD9, 0x8F, 0x8F), // clay
    ];
    let palette = match theme::mode() {
        theme::Mode::Light => &LIGHT,
        theme::Mode::Dark => &DARK,
    };
    palette[(fnv1a(name) as usize) % palette.len()]
}

/// FNV-1a over the tool name. Small, dependency-free, and — the point here — **stable across
/// runs**, unlike `std`'s `DefaultHasher`, which is randomly seeded per process and would repaint
/// every tool a different colour each time the app starts.
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
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

/// Width of the left row-name gutter for this frame's rows: the longest name plus padding, clamped
/// between [`GUTTER_MIN`] and [`GUTTER_MAX`].
///
/// Measured rather than fixed on purpose. The column used to be a hard 182 points, which is only
/// right for the longest tool names — for the common case (`Input` / `Model` / a short tool) it
/// left a wide blank strip between the window edge and the chart, and that strip is exactly what
/// made the table look "pushed away from the left".
fn gutter_w(ui: &Ui, rows: &[Row]) -> f32 {
    let mut widest: f32 = 0.0;
    for row in rows {
        let font =
            if row.dim() { FontId::proportional(13.5) } else { theme::mono_font(13.5) };
        widest = widest.max(gwin::text_w(ui, &font, row.label()));
    }
    // 12 points of breathing room on each side of the longest name.
    (widest + 24.0).clamp(GUTTER_MIN, GUTTER_MAX)
}


// ══════════════════════ Below: details of the selected item ══════════════════════

/// The popover's size, decided from its own fixed bounds rather than measured after the fact, so it
/// can never flip-flop between frames. Shrinks to fit a small window; the body inside then scrolls.
fn popover_size(full: Rect) -> (f32, f32) {
    (
        POP_W.min(full.width() - 16.0).max(220.0),
        POP_H.min(full.height() - gwin::HEADER_H - 24.0).max(180.0),
    )
}

/// Where the popover sits: beside the anchor bar, flipping to its left when there is no room on the
/// right, then clamped into the window.
fn popover_pos(anchor: Rect, full: Rect, pop_w: f32, pop_h: f32) -> (f32, f32) {
    let mut x = anchor.right() + 10.0;
    if x + pop_w > full.right() - 8.0 {
        x = anchor.left() - 10.0 - pop_w;
    }
    let x = x.clamp(full.left() + 8.0, (full.right() - 8.0 - pop_w).max(full.left() + 8.0));
    let top = full.top() + gwin::HEADER_H + 6.0;
    let y = (anchor.center().y - pop_h * 0.5).clamp(top, (full.bottom() - 8.0 - pop_h).max(top));
    (x, y)
}

/// The rect the popover will occupy. Used by [`input`] as well, so the wheel can be left to the
/// popover's own content while the pointer is over it.
fn popover_rect(anchor: Rect, full: Rect) -> Rect {
    let (pop_w, pop_h) = popover_size(full);
    let (x, y) = popover_pos(anchor, full, pop_w, pop_h);
    Rect::from_min_size(Pos2::new(x, y), Vec2::new(pop_w, pop_h))
}

/// The click-a-node popover: a small floating panel that appears beside the selected bar, with a
/// body that scrolls inside it.
///
/// This replaced the big bottom detail card. The card was always present, always eating a third of
/// the window (which squeezed the chart), and it showed the *latest* step whether or not that was
/// the one you cared about. A popover belongs to something you actually clicked, and while it is
/// closed the timeline gets the whole window.
fn popover(ui: &mut Ui, st: &mut Activity, full: Rect) {
    // No anchor ⇒ the selected node is scrolled out of view (or nothing is selected): nothing to
    // point at, so draw nothing — rather than parking a panel in a corner.
    let Some(anchor) = st.pop_anchor else { return };
    let Some(a) = st.selected_act() else { return };

    // Take an owned snapshot so the borrow of `st.acts` ends here — the close button below needs
    // `st` mutably.
    let failed = a.failed;
    let kind = a.kind;
    let round = a.round;
    let open = a.open();
    let dur = a.dur(st.now);
    // Round-local, matching the ruler and the hover tooltip: the badge right below already says
    // which round this is, so counting from that round's start is unambiguous.
    let at_rel = a.at - st.tz() - st.round_start(round);
    let title = a.title.clone();
    let text = a.body.trim().to_owned();

    let (pop_w, pop_h) = popover_size(full);
    let (x, y) = popover_pos(anchor, full, pop_w, pop_h);

    let label = if failed { ActKind::Error } else { kind };
    egui::Area::new(egui::Id::new("aox-act-pop"))
        .order(egui::Order::Foreground)
        .fixed_pos(Pos2::new(x, y))
        .show(ui.ctx(), |ui| {
            Frame::NONE
                .fill(P::surface())
                .corner_radius(CornerRadius::same(theme::card_radius()))
                .stroke(Stroke::new(1.0, P::border_strong()))
                .shadow(egui::Shadow {
                    offset: [0, 8],
                    blur: 22,
                    spread: 0,
                    color: P::shadow(),
                })
                .inner_margin(Margin::symmetric(16, 12))
                .show(ui, |ui| {
                    // Content width = the nominal width minus the 16pt margins on each side, so the
                    // whole panel comes out at `pop_w` and the clamp above is honest.
                    let w = (pop_w - 32.0).max(140.0);
                    ui.set_width(w);
                    // Fixed height: the body scrolls inside it, so a long tool result can never
                    // stretch the popover off the screen.
                    let body_h = (pop_h - 24.0).max(120.0);
                    ui.set_min_height(body_h);
                    ui.set_max_height(body_h);
                    ui.spacing_mut().item_spacing.y = 0.0;

                    // ── Header: category badge + round ──
                    let (head, _) = ui.allocate_exact_size(Vec2::new(w, 22.0), Sense::hover());
                    {
                        let font = FontId::proportional(12.5);
                        let tw = gwin::text_w(ui, &font, label.label());
                        let br = Rect::from_min_size(head.min, Vec2::new(tw + 16.0, 20.0));
                        let meta = aox::tf!("Round {}", round);
                        let p = ui.painter();
                        p.rect_filled(br, CornerRadius::same(5), label.color());
                        p.text(br.center(), Align2::CENTER_CENTER, label.label(), font, P::bg());
                        p.text(
                            Pos2::new(br.right() + 10.0, head.center().y),
                            Align2::LEFT_CENTER,
                            &meta,
                            FontId::proportional(13.0),
                            P::text_faint(),
                        );
                    }
                    // The popover's own way out — Esc and clicking blank timeline space also work.
                    let cr = Rect::from_min_size(
                        Pos2::new(head.right() - 20.0, head.top() + 1.0),
                        Vec2::splat(20.0),
                    );
                    let cresp = ui.interact(cr, ui.id().with("aox-pop-close"), Sense::click());
                    if cresp.hovered() {
                        ui.painter().rect_filled(cr, CornerRadius::same(5), P::surface_hi());
                    }
                    let fg = if cresp.hovered() { P::ink() } else { P::text_dim() };
                    let c = cr.center();
                    let stroke = Stroke::new(1.3, fg);
                    let p = ui.painter();
                    p.line_segment([c + Vec2::new(-3.4, -3.4), c + Vec2::new(3.4, 3.4)], stroke);
                    p.line_segment([c + Vec2::new(-3.4, 3.4), c + Vec2::new(3.4, -3.4)], stroke);
                    if cresp.clicked() {
                        st.selected = None;
                    }

                    // ── Title ──
                    ui.add_space(8.0);
                    let font = if kind.mono() {
                        theme::mono_font(15.0)
                    } else {
                        FontId::proportional(15.5)
                    };
                    let (trow, _) = ui.allocate_exact_size(Vec2::new(w, 20.0), Sense::hover());
                    let g = gwin::elide(ui, &title, font, P::text(), trow.width());
                    let p = ui.painter();
                    p.galley(
                        Pos2::new(trow.left(), trow.center().y - g.size().y * 0.5),
                        g,
                        P::text(),
                    );

                    // ── When it ran, and for how long ──
                    let when = if open {
                        aox::tf!(
                            "{} started · running {}s",
                            fmt_at(at_rel, 0.5),
                            format!("{:.2}", dur)
                        )
                    } else {
                        format!(
                            "{} → {} · {dur:.2}s",
                            fmt_at(at_rel, 0.5),
                            fmt_at(at_rel + dur, 0.5)
                        )
                    };
                    ui.add_space(2.0);
                    let (wrow, _) = ui.allocate_exact_size(Vec2::new(w, 16.0), Sense::hover());
                    ui.painter().text(
                        wrow.left_top(),
                        Align2::LEFT_TOP,
                        when,
                        theme::mono_font(12.0),
                        P::text_faint(),
                    );

                    ui.add_space(8.0);
                    let (rule, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
                    ui.painter().hline(
                        rule.x_range(),
                        rule.center().y,
                        Stroke::new(1.0, P::border()),
                    );
                    ui.add_space(8.0);

                    // ── Body (the scrollable part) ──
                    if text.is_empty() {
                        ui.label(
                            RichText::new(aox::i18n::t("(no more information)"))
                                .color(P::text_faint())
                                .size(13.5),
                        );
                        return;
                    }
                    let font = if kind.mono() {
                        theme::mono_font(13.0)
                    } else {
                        FontId::proportional(14.0)
                    };
                    let col = if failed { ActKind::failed_color() } else { P::text() };
                    // Pin the width instead of asking for `available_width()`: a `ScrollArea`'s
                    // scrollbar allocation animates, so a width derived from it changes every frame
                    // and the text re-wraps (see the note in TRAPS).
                    let sc_w = (w - 10.0).max(80.0);
                    let avail = (ui.available_height() - 6.0).max(40.0);
                    ScrollArea::vertical()
                        .id_salt("aox-act-pop-body")
                        .max_height(avail)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width(sc_w);
                            let mut job = egui::text::LayoutJob::default();
                            job.wrap.max_width = sc_w;
                            job.wrap.break_anywhere = kind.mono();
                            for (i, seg) in text.split('\n').enumerate() {
                                if i > 0 {
                                    job.append(
                                        "\n",
                                        0.0,
                                        egui::text::TextFormat {
                                            font_id: font.clone(),
                                            ..Default::default()
                                        },
                                    );
                                }
                                job.append(
                                    seg,
                                    0.0,
                                    egui::text::TextFormat {
                                        font_id: font.clone(),
                                        color: col,
                                        ..Default::default()
                                    },
                                );
                            }
                            ui.add(
                                egui::Label::new(job)
                                    .selectable(true)
                                    .wrap_mode(egui::TextWrapMode::Wrap),
                            );
                            ui.add_space(6.0);
                        });
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

    /// Which hand-offs get a link line, and where it leaves from. The cases are the shapes the
    /// stream actually produces: mostly zero-width spans (one segment is closed at exactly the next
    /// one's start time), a few real gaps, and the occasional pair that overlaps in time.
    #[test]
    fn link_lines_join_every_hand_off_that_is_on_screen() {
        let vis = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(100.0, 50.0));
        let at = |x: f32, y: f32| Pos2::new(x, y);

        // The ordinary case — and the one an over-eager rule dropped by mistake. The next record
        // starts exactly where the previous one stopped (`tool_begin` closes the model segment at
        // the tool's own start time), so the span is zero-width: the link must be a *straight* line
        // between the two rows, not a missing one.
        assert_eq!(
            link_offset(at(30.0, 10.0), at(30.0, 10.0), at(30.0, 40.0), vis),
            Some((at(30.0, 10.0), 0.0))
        );
        // A real gap: leave from the tail, with an offset proportional to the span.
        assert_eq!(
            link_offset(at(10.0, 10.0), at(10.0, 10.0), at(40.0, 20.0), vis),
            Some((at(10.0, 10.0), 30.0 * 0.45))
        );
        // Capped, so a very long hand-off still gets the same tidy curve.
        assert_eq!(
            link_offset(at(10.0, 10.0), at(10.0, 10.0), at(900.0, 10.0), vis),
            Some((at(10.0, 10.0), 46.0))
        );
        // Overlapping in time (a note written while its tool bar was still running): the tail sits
        // *behind* the next head, so the link leaves from the head — still pointing forwards.
        assert_eq!(
            link_offset(at(20.0, 10.0), at(60.0, 10.0), at(40.0, 30.0), vis),
            Some((at(20.0, 10.0), 20.0 * 0.45))
        );
        // One end on screen is enough: the other simply runs off the edge.
        assert!(link_offset(at(-80.0, 10.0), at(-80.0, 10.0), at(30.0, 10.0), vis).is_some());
        assert!(link_offset(at(70.0, 10.0), at(70.0, 10.0), at(260.0, 10.0), vis).is_some());

        // Both ends outside: on screen you would only ever see the clipped middle of the line.
        assert_eq!(link_offset(at(-300.0, 10.0), at(-300.0, 10.0), at(700.0, 10.0), vis), None);
        // ...including when it is the *rows* that are out of view (scrolled off vertically).
        assert_eq!(link_offset(at(20.0, -200.0), at(20.0, -200.0), at(30.0, -260.0), vis), None);
    }

    /// `input` decides "the wheel belongs to the popover" from this rect, so it has to be the rect
    /// `popover` actually draws into — otherwise the guard would either let the axis slide while the
    /// user scrolls the content, or swallow the content's own scrolling.
    #[test]
    fn popover_rect_matches_where_the_panel_is_drawn() {
        let full = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(1000.0, 700.0));
        let (w, h) = popover_size(full);
        let anchor = Rect::from_min_size(Pos2::new(300.0, 400.0), Vec2::new(40.0, 28.0));

        // Room on the right ⇒ beside the bar, clear of it.
        let r = popover_rect(anchor, full);
        assert_eq!((r.width(), r.height()), (w, h));
        assert!(r.left() >= anchor.right());
        assert!(!r.intersects(anchor));

        // Bar at the far right ⇒ flip to its left, still inside the window.
        let edge = Rect::from_min_size(Pos2::new(full.right() - 30.0, 400.0), Vec2::new(20.0, 28.0));
        let flipped = popover_rect(edge, full);
        assert!(flipped.left() < edge.left(), "no room on the right therefore flips left");
        assert!(full.contains_rect(flipped));

        // A window too small for the nominal size: it shrinks, and the rect stays inside.
        let tiny = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(260.0, 240.0));
        let tr = popover_rect(Rect::from_min_size(Pos2::new(20.0, 120.0), Vec2::new(10.0, 20.0)), tiny);
        assert!(tr.width() < w && tr.height() < h);
        assert!(tiny.contains_rect(tr));
    }

    /// The ruler is labelled per round, so each round's origin is its own record's start — and
    /// because the clock stops between rounds, that origin is the previous round's end.
    #[test]
    fn each_round_starts_where_the_previous_one_ended() {
        let mut a = Activity::default();
        a.round_begin("one", 0.0);
        a.model_begin(0.0);
        a.end_round(4.0);
        a.round_begin("two", 4.0); // the clock was stopped in between, so it resumes here
        a.model_begin(4.0);
        a.end_round(6.0);

        // Spans indexed by round; round 0 is unused.
        let spans = a.round_spans();
        assert_eq!(spans[0].0, f32::MAX, "round 0 holds nothing");
        assert_eq!(spans[1], (0.0, 4.0));
        assert_eq!(spans[2], (4.0, 6.0), "round two starts exactly where round one stopped");
        // The origins the ruler labels from: 0s for each round, not a running total.
        assert_eq!(a.round_start(1), 0.0);
        assert_eq!(a.round_start(2), 4.0);
        assert_eq!(a.round_start(9), 0.0, "a round with no records left cannot invent an origin");
    }
}
