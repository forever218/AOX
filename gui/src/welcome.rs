//! The welcome line on the first screen: which sentence, in what colour, typed or not.
//! This module owns three things and nothing else.
//! * Prefs — what the user chose on the Appearance page, and its little on-disk file (`<app...

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use egui::{Color32, FontId, Galley, Pos2, Rect, Stroke, StrokeKind, Ui, Vec2};

use aox::workplace::AOX_DIR;

use crate::theme::{self, P};

// preferences

/// Where the line comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Source {
    /// What the user typed in the settings panel.
    #[default]
    Custom,
    /// A quoting API over HTTP (default `aox::hello::HITOKOTO_DEFAULT`).
    Hitokoto,
    /// One small call to the model in use.
    Model,
}

impl Source {
    pub const ALL: [Source; 3] = [Source::Custom, Source::Hitokoto, Source::Model];

    pub fn key(self) -> &'static str {
        match self {
            Source::Custom => "custom",
            Source::Hitokoto => "hitokoto",
            Source::Model => "model",
        }
    }

    fn parse(s: &str) -> Source {
        match s {
            "hitokoto" => Source::Hitokoto,
            "model" => Source::Model,
            _ => Source::Custom,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Source::Custom => aox::i18n::t("My own"),
            Source::Hitokoto => aox::i18n::t("Hitokoto"),
            Source::Model => aox::i18n::t("Model"),
        }
    }
}

/// Everything the Appearance page can change about the first screen.
#[derive(Clone, Debug)]
pub struct Prefs {
    pub source: Source,
    /// The sentence for Source::Custom.
    pub custom: String,
    pub hitokoto_url: String,
    /// Colour on the light theme.
    pub light: Color32,
    /// Colour on the dark theme.
    pub dark: Color32,
    /// Typewriter on/off. Off = the whole sentence appears at once (no caret, no fade).
    pub typing: bool,
    /// The first screen's background image on/off. Off = the plain `P::bg()` fill, exactly as it was
    /// before the artwork feature existed.
    pub background: bool,
}

/// Default colours: black on paper, white on ink. Not `P::text()` on purpose — the line is meant to
/// sit on artwork, so it needs its own pair rather than inheriting the interface's ink.
const DEFAULT_LIGHT: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
const DEFAULT_DARK: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);

impl Default for Prefs {
    fn default() -> Self {
        Self {
            source: Source::Custom,
            // A fresh install shows a sentence, not an empty screen: the shipped default is the same line the local pool...
            custom: aox::i18n::t("Ready when you are.").to_string(),
            hitokoto_url: aox::hello::HITOKOTO_DEFAULT.to_string(),
            light: DEFAULT_LIGHT,
            dark: DEFAULT_DARK,
            typing: true,
            background: true,
        }
    }
}

impl Prefs {
    /// The colour in force for a mode.
    pub fn color(&self, mode: theme::Mode) -> Color32 {
        match mode {
            theme::Mode::Light => self.light,
            theme::Mode::Dark => self.dark,
        }
    }

    pub fn default_color(mode: theme::Mode) -> Color32 {
        match mode {
            theme::Mode::Light => DEFAULT_LIGHT,
            theme::Mode::Dark => DEFAULT_DARK,
        }
    }

    /// Read the preferences. **Never fails** — see the module docs
    pub fn load(root: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path(root)) else { return Self::default() };
        let Ok(wire) = serde_json::from_str::<Wire>(&text) else { return Self::default() };
        if wire.version != VERSION {
            return Self::default();
        }
        let d = Self::default();
        Self {
            source: wire.source.as_deref().map(Source::parse).unwrap_or(d.source),
            custom: wire.custom.unwrap_or(d.custom),
            hitokoto_url: wire
                .hitokoto_url
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .unwrap_or(d.hitokoto_url),
            // A colour that does not parse falls back **per field**: one bad hex must not throw the
            // other colour away with it.
            light: wire.light.as_deref().and_then(parse_hex).unwrap_or(d.light),
            dark: wire.dark.as_deref().and_then(parse_hex).unwrap_or(d.dark),
            typing: wire.typing.unwrap_or(d.typing),
            background: wire.background.unwrap_or(d.background),
        }
    }

    /// Write the preferences back, atomically (temp file + rename) — the GUI can be killed at any
    /// moment, and a half-written file is one the next launch silently discards.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let dir = root.join(AOX_DIR);
        std::fs::create_dir_all(&dir)?;
        let wire = Wire {
            version: VERSION,
            source: Some(self.source.key().to_string()),
            custom: Some(self.custom.clone()),
            hitokoto_url: Some(self.hitokoto_url.clone()),
            light: Some(hex(self.light)),
            dark: Some(hex(self.dark)),
            typing: Some(self.typing),
            background: Some(self.background),
        };
        let tmp = dir.join("welcome.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&wire)?)?;
        std::fs::rename(&tmp, path(root))?;
        Ok(())
    }
}

/// The on-disk layout version — rejected, not guessed at, when it does not match.
const VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct Wire {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    custom: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hitokoto_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    light: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dark: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    typing: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    background: Option<bool>,
}

fn path(root: &Path) -> PathBuf {
    root.join(AOX_DIR).join("welcome.json")
}

/// `#RRGGBB` → colour. Accepts a missing `#`, and a leading/trailing space (people paste from
/// anywhere). Anything else is `None`; the caller decides the fallback.
pub fn parse_hex(s: &str) -> Option<Color32> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// Colour → `#RRGGBB` (uppercase, so two saves produce the same bytes)
pub fn hex(c: Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", c.r(), c.g(), c.b())
}

// the stage

/// First character waits this long after the screen appears. Long enough that the window has
/// finished settling, short enough not to read as a delay.
const FIRST_CHAR: f32 = 0.28;
/// Per-character step. Chinese characters are wide, so they need a slower beat than Latin; 52 ms is
/// about reading speed for a short line.
const CHAR_STEP: f32 = 0.052;
/// Extra pause after a punctuation mark — without it a Chinese sentence arrives as a machine gun.
const PUNCT_PAUSE: f32 = 0.16;
/// How long the caret stays after the last character, fading out over that window.
const CARET_FADE: f32 = 0.25;
/// How long to wait for a quoting API before typing something local instead.
pub const GRACE: f32 = 1.2;
/// Longer wait for model calls (2-4s), so a greeting is not typed over the answer and then swapped.
const MODEL_GRACE: f32 = 6.0;
/// How long after the first screen appears a late line may replace the local fallback that is already typing.
const REPLACE_WINDOW: f32 = 10.0;
/// A fetched line kept for "next time" goes stale: ten minutes later the greeting should be about
/// *now*, not about the last time the user opened a new conversation.
const PENDING_TTL: f32 = 600.0;

/// What the stage needs from the outside world. The caller turns this into a
/// `ToAgent` message and answers later through [`Stage::on_line`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    /// Fetch one line from this address.
    Api(String),
    /// Ask the model for one line.
    Model,
}

enum Phase {
    /// Not on the first screen (or the custom text is empty).
    Idle,
    /// A request is in flight; the caret waits on an empty line.
    Waiting { since: Instant },
    /// Revealing characters. `sched[i]` is when character `i` appears.
    Typing { text: String, start: Instant, sched: Vec<f32> },
    /// Whole line on screen, nothing animating.
    Shown { text: String },
}

/// The first screen's line: what to show, in what state, at what speed.
pub struct Stage {
    pub prefs: Prefs,
    phase: Phase,
    /// Whether the first screen is on show **right now**. Owned here rather than asked every frame
    /// because "entered it" is an edge, and the whole behaviour is built on that edge.
    active: bool,
    /// A line that arrived too late to be shown; used by the next first screen.
    pending: Option<(String, Instant)>,
    /// A line the user explicitly asked for on the settings page: shown even if something is
    /// already on screen (see the module docs, rule 1's exception).
    expect_refresh: bool,
    /// Why the last request failed — shown **only** in the settings panel.
    note: Option<String>,
    /// The last line that actually made it onto the screen (for the settings panel's status line).
    last_line: Option<String>,
    /// Rotates the local pool so two new conversations in a row do not open with the same sentence.
    round: usize,
    /// When the current first screen was entered (the replace window is measured from here).
    entered_at: Instant,
    /// Whether what is on screen is the local fallback rather than a real answer.
    fallback_shown: bool,
    /// What the two colour fields currently show, one per mode.
    drafts: [String; 2],
    /// The laid-out line, kept between frames: layout is the expensive part of drawing text, and the text only changes...
    fit: Option<Fit>,
}

struct Fit {
    src: String,
    size: f32,
    max_w: f32,
    /// What is actually drawn (identical to `src`, or cut with an ellipsis when even the smallest
    /// size does not fit).
    text: String,
    galley: Arc<Galley>,
}

impl Stage {
    pub fn new(prefs: Prefs) -> Self {
        let drafts = [hex(prefs.light), hex(prefs.dark)];
        Self {
            prefs,
            phase: Phase::Idle,
            active: false,
            pending: None,
            expect_refresh: false,
            note: None,
            last_line: None,
            round: 0,
            entered_at: Instant::now(),
            fallback_shown: false,
            drafts,
            fit: None,
        }
    }

    /// Set one of the two colours (from a preset swatch), keeping its field in step.
    pub fn set_color(&mut self, mode: theme::Mode, c: Color32) {
        match mode {
            theme::Mode::Light => self.prefs.light = c,
            theme::Mode::Dark => self.prefs.dark = c,
        }
        self.drafts[idx(mode)] = hex(c);
    }

    /// The text the colour field edits. See [`Stage::drafts`] for why this is not the colour itself.
    pub fn draft_mut(&mut self, mode: theme::Mode) -> &mut String {
        &mut self.drafts[idx(mode)]
    }

    /// Commit a typed colour. Returns whether the colour actually changed.
    pub fn commit_draft(&mut self, mode: theme::Mode) -> bool {
        let i = idx(mode);
        match parse_hex(&self.drafts[i]) {
            Some(c) if c != self.prefs.color(mode) => {
                self.set_color(mode, c);
                true
            }
            Some(_) => {
                self.drafts[i] = hex(self.prefs.color(mode));
                false
            }
            None => {
                self.drafts[i] = hex(self.prefs.color(mode));
                false
            }
        }
    }

    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Settings drops two small status lines; on_line still decides pending, and these accessors are its public face.
    #[allow(dead_code)]
    pub fn last_line(&self) -> Option<&str> {
        self.last_line.as_deref()
    }

    /// The line held for next time, if it is still fresh.
    #[allow(dead_code)]
    pub fn pending_line(&self) -> Option<&str> {
        match &self.pending {
            Some((t, at)) if at.elapsed().as_secs_f32() < PENDING_TTL => Some(t),
            _ => None,
        }
    }

    /// Preferences changed: drop what is on screen so the next frame starts over with the new
    /// settings. Also drops a pending line — it came from the source that was just replaced.
    pub fn restart(&mut self) {
        self.phase = Phase::Idle;
        self.active = false;
        self.pending = None;
        self.expect_refresh = false;
        self.fit = None;
        self.drafts = [hex(self.prefs.light), hex(self.prefs.dark)];
    }

    /// Drive the first screen's edge.
    pub fn sync(&mut self, empty: bool) -> Option<Ask> {
        if !empty {
            if self.active {
                self.active = false;
                self.phase = Phase::Idle;
                self.fit = None;
            }
            return None;
        }
        if self.active {
            return None;
        }
        self.active = true;
        self.begin()
    }

    fn begin(&mut self) -> Option<Ask> {
        self.round = self.round.wrapping_add(1);
        self.entered_at = Instant::now();
        self.fallback_shown = false;
        match self.prefs.source {
            Source::Custom => {
                let text = self.prefs.custom.trim().to_string();
                if text.is_empty() {
                    // "Empty" is the user's way of turning the line off; the local pool is **not**
                    // substituted here, or there would be no way to have a quiet first screen.
                    self.phase = Phase::Idle;
                    return None;
                }
                self.start_text(text);
                None
            }
            Source::Hitokoto | Source::Model => {
                if let Some((text, at)) = self.pending.take() {
                    if at.elapsed().as_secs_f32() < PENDING_TTL {
                        self.start_text(text);
                        return None;
                    }
                }
                self.phase = Phase::Waiting { since: Instant::now() };
                Some(match self.prefs.source {
                    Source::Hitokoto => Ask::Api(self.prefs.hitokoto_url.clone()),
                    _ => Ask::Model,
                })
            }
        }
    }

    /// The answer to an [`Ask`]. `ok = false` carries the reason in `why`.
    pub fn on_line(&mut self, ok: bool, text: String, why: String) {
        if ok {
            self.note = None;
        } else {
            self.note = Some(why);
        }
        // An explicit refresh replaces what is on screen; a late answer to the automatic request
        // waits for the next first screen (module docs, rule 1).
        if self.expect_refresh && self.active && ok {
            self.expect_refresh = false;
            self.start_text(text);
            return;
        }
        self.expect_refresh = false;
        if let Phase::Waiting { .. } = self.phase {
            if ok {
                self.start_text(text);
            } else {
                self.start_local();
            }
            return;
        }
        if !ok {
            return;
        }
        // A real answer arriving while the fallback is on screen takes the screen over.
        if self.active
            && self.fallback_shown
            && self.entered_at.elapsed().as_secs_f32() < REPLACE_WINDOW
        {
            self.fallback_shown = false;
            self.start_text(text);
            return;
        }
        self.pending = Some((text, Instant::now()));
    }

    /// The settings page's "another one": ask again and show the answer when it arrives.
    pub fn refresh(&mut self) -> Option<Ask> {
        self.expect_refresh = true;
        self.note = None;
        match self.prefs.source {
            Source::Custom => None,
            Source::Hitokoto => Some(Ask::Api(self.prefs.hitokoto_url.clone())),
            Source::Model => Some(Ask::Model),
        }
    }

    /// A sentence from the local pool, chosen by the clock.
    fn start_local(&mut self) {
        let part = aox::hello::Daypart::of(aox::hello::hour());
        let text = aox::hello::fallback_line(part, self.round);
        self.fallback_shown = true;
        self.start_text(text);
    }

    fn start_text(&mut self, text: String) {
        self.last_line = Some(text.clone());
        self.fit = None;
        if !self.prefs.typing {
            self.phase = Phase::Shown { text };
            return;
        }
        let sched = schedule(&text);
        self.phase = Phase::Typing { text, start: Instant::now(), sched };
    }

    /// How long this source may take before the local pool stands in for it.
    fn grace(&self) -> f32 {
        match self.prefs.source {
            Source::Model => MODEL_GRACE,
            _ => GRACE,
        }
    }

    /// Advance the clocks. Call once per frame, before [`Stage::draw`]; asks for a repaint while
    /// anything is still moving (an idle egui window does not redraw on its own).
    pub fn step(&mut self, ctx: &egui::Context) {
        match &self.phase {
            Phase::Waiting { since } => {
                if since.elapsed().as_secs_f32() >= self.grace() {
                    self.start_local();
                }
                ctx.request_repaint_after(std::time::Duration::from_millis(16));
            }
            Phase::Typing { text, start, sched } => {
                let elapsed = start.elapsed().as_secs_f32();
                let done = sched.last().copied().unwrap_or(0.0);
                if elapsed >= done + CARET_FADE {
                    self.phase = Phase::Shown { text: text.clone() };
                } else {
                    ctx.request_repaint_after(std::time::Duration::from_millis(16));
                }
            }
            _ => {}
        }
    }

    /// Paint the line, centred in `stage` (the column above the composer).
    pub fn draw(&mut self, ui: &Ui, stage: Rect) {
        if stage.height() < 120.0 || stage.width() < 160.0 {
            return;
        }
        let color = self.prefs.color(theme::mode());
        let (text, reveal, caret) = match &self.phase {
            Phase::Idle => return,
            Phase::Waiting { since } => (String::new(), 0, Some(blink(since))),
            Phase::Typing { text, start, sched } => {
                let elapsed = start.elapsed().as_secs_f32();
                let n = sched.iter().filter(|t| **t <= elapsed).count();
                let done = sched.last().copied().unwrap_or(0.0);
                // The caret outlives the last character by a short fade, so the line does not end
                // with a bar that vanishes between two frames.
                let alpha = if elapsed <= done {
                    1.0
                } else {
                    (1.0 - (elapsed - done) / CARET_FADE).clamp(0.0, 1.0)
                };
                (text.clone(), n, Some(alpha))
            }
            Phase::Shown { text } => (text.clone(), usize::MAX, None),
        };

        let max_w = (stage.width() - 96.0).clamp(160.0, 720.0);
        let center_x = stage.center().x;
        let center_y = stage.top() + stage.height() * 0.42;

        // Waiting on a remote line: no text yet, only the caret — placed where the line will start,
        // which is the centre, because a not-yet-existing line is zero wide.
        if text.is_empty() {
            let h = 26.0_f32 * 1.15;
            if let Some(alpha) = caret {
                if alpha > 0.01 {
                    caret_bar(ui, Pos2::new(center_x, center_y), h, color, alpha);
                }
            }
            return;
        }

        let fit = self.fitted(ui, &text, max_w);
        let size = fit.size;
        let galley = fit.galley.clone();
        let box_w = galley.size().x;
        let left = center_x - box_w * 0.5;
        let top = center_y - galley.size().y * 0.5;

        // The reveal is a clip, not a partial layout: laying out the prefix every frame would re-shape Chinese text 60...
        let shown: String = if reveal >= fit.text.chars().count() {
            fit.text.clone()
        } else {
            fit.text.chars().take(reveal).collect()
        };
        let prefix_w = if shown.is_empty() {
            0.0
        } else {
            ui.painter()
                .layout_no_wrap(shown.clone(), FontId::proportional(size), Color32::PLACEHOLDER)
                .size()
                .x
        };
        let clip = Rect::from_min_max(
            Pos2::new(left, top),
            Pos2::new((left + prefix_w).min(left + box_w), top + galley.size().y),
        );
        let painter = ui.painter();
        if !shown.is_empty() {
            painter.with_clip_rect(clip).galley(Pos2::new(left, top), galley.clone(), color);
        }
        if let Some(alpha) = caret {
            if alpha > 0.01 {
                caret_bar(ui, Pos2::new(left + prefix_w, center_y), size * 1.15, color, alpha);
            }
        }
    }

    /// Takes the `Ui` because layout has to go through this window's fonts: building a throwaway `Context` here (the...
    fn fitted(&mut self, ui: &Ui, text: &str, max_w: f32) -> &Fit {
        let rebuild = match &self.fit {
            Some(f) => f.src != text || (f.max_w - max_w).abs() > 0.5,
            None => true,
        };
        if rebuild {
            let mut size = 26.0_f32;
            let mut galley = layout(ui, text, size);
            for smaller in [24.0_f32, 22.0, 20.0] {
                if galley.size().x <= max_w {
                    break;
                }
                size = smaller;
                galley = layout(ui, text, size);
            }
            let mut shown = text.to_string();
            if galley.size().x > max_w {
                // Last resort: cut with an ellipsis. A long custom sentence is the only way to get
                // here — the model and the API are both asked for short lines.
                let ell = layout(ui, "…", size).size().x;
                let mut cut = String::new();
                for ch in text.chars() {
                    let mut probe = cut.clone();
                    probe.push(ch);
                    if layout(ui, &probe, size).size().x + ell > max_w {
                        break;
                    }
                    cut = probe;
                }
                cut.push('…');
                shown = cut;
                galley = layout(ui, &shown, size);
            }
            self.fit = Some(Fit { src: text.to_string(), size, max_w, text: shown, galley });
        }
        self.fit.as_ref().expect("just built")
    }
}

/// Which of the two drafts / colours a mode refers to
fn idx(mode: theme::Mode) -> usize {
    match mode {
        theme::Mode::Light => 0,
        theme::Mode::Dark => 1,
    }
}

/// Lay out one line with no wrapping; the colour is a placeholder so a theme switch does not
/// invalidate the cache (the real colour is passed to `Painter::galley` as its fallback).
fn layout(ui: &Ui, text: &str, size: f32) -> Arc<Galley> {
    ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(size), Color32::PLACEHOLDER)
}

/// Draw the typewriter caret: a soft 2pt bar in the line's own colour, at the given opacity.
fn caret_bar(ui: &Ui, at: Pos2, h: f32, color: Color32, alpha: f32) {
    let a = (255.0 * alpha.clamp(0.0, 1.0) * 0.85) as u8;
    let c = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a);
    let rect = Rect::from_center_size(at, Vec2::new(2.0, h));
    ui.painter().rect_filled(rect, 1.0, c);
}

/// Blink while waiting: on for half a second, off for half a second, from the moment the wait
/// started (so it does not restart every frame).
fn blink(since: &Instant) -> f32 {
    if (since.elapsed().as_secs_f32() * 2.0).fract() < 0.5 {
        1.0
    } else {
        0.0
    }
}

/// When each character appears, in seconds from the start of the typing.
fn schedule(text: &str) -> Vec<f32> {
    let mut out = Vec::with_capacity(text.chars().count());
    let mut t = FIRST_CHAR;
    for ch in text.chars() {
        out.push(t);
        t += CHAR_STEP;
        if matches!(ch, '，' | '。' | '！' | '？' | '；' | '：' | '、' | '…' | '—' | ',' | '.' | '!' | '?' | ';' | ':') {
            t += PUNCT_PAUSE;
        }
    }
    out
}

/// A tiny helper the settings page uses to draw the colour swatch: a filled rounded square with a
/// hairline edge, so a white swatch is still visible on paper.
pub fn swatch(ui: &Ui, rect: Rect, color: Color32) {
    ui.painter().rect_filled(rect, 3.0, color);
    ui.painter().rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0, P::edge()),
        StrokeKind::Inside,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aox-welcome-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Preferences must survive a restart — that is the whole reason they are on disk — and a file
    /// written by a **future** build must be refused rather than half-read.
    #[test]
    fn prefs_round_trip_and_reject_the_future() {
        let dir = temp_dir("rt");
        let p = Prefs {
            source: Source::Model,
            custom: "早上好".into(),
            hitokoto_url: "http://127.0.0.1:9/x?encode=text".into(),
            light: Color32::from_rgb(0x11, 0x22, 0x33),
            dark: Color32::from_rgb(0xAA, 0xBB, 0xCC),
            typing: false,
            background: false,
        };
        p.save(&dir).unwrap();
        let back = Prefs::load(&dir);
        assert_eq!(back.source, Source::Model);
        assert_eq!(back.custom, "早上好");
        assert_eq!(back.hitokoto_url, "http://127.0.0.1:9/x?encode=text");
        assert_eq!(back.light, p.light);
        assert_eq!(back.dark, p.dark);
        assert!(!back.typing && !back.background);

        std::fs::write(path(&dir), br#"{"version":99,"source":"model","typing":false}"#).unwrap();
        let back = Prefs::load(&dir);
        assert_eq!(back.source, Source::Custom);
        assert!(back.typing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing file, a broken file and a bad colour all have to leave a usable first screen.
    #[test]
    fn broken_files_and_colours_fall_back() {
        let dir = temp_dir("bad");
        assert_eq!(Prefs::load(&dir).source, Source::Custom, "no file is not an error");

        std::fs::create_dir_all(dir.join(AOX_DIR)).unwrap();
        std::fs::write(path(&dir), b"not json").unwrap();
        assert!(Prefs::load(&dir).background, "corrupt file -> defaults");

        std::fs::write(
            path(&dir),
            br##"{"version":1,"light":"#GGGGGG","dark":"#112233"}"##,
        )
        .unwrap();
        let p = Prefs::load(&dir);
        assert_eq!(p.light, DEFAULT_LIGHT, "the broken colour falls back on its own");
        assert_eq!(p.dark, Color32::from_rgb(0x11, 0x22, 0x33), "its neighbour survives");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hex parsing is what the settings field commits through: it has to accept what people paste
    /// and reject what it cannot honestly read.
    #[test]
    fn hex_parses_and_formats() {
        assert_eq!(parse_hex("#0A0B0C"), Some(Color32::from_rgb(10, 11, 12)));
        assert_eq!(parse_hex(" 0a0b0c "), Some(Color32::from_rgb(10, 11, 12)));
        assert_eq!(parse_hex("#FFF"), None, "three digits is not enough to be unambiguous");
        assert_eq!(parse_hex("#12345G"), None);
        assert_eq!(parse_hex(""), None);
        assert_eq!(hex(Color32::from_rgb(0xAB, 0xCD, 0xEF)), "#ABCDEF");
        // Round trip through both
        let c = Color32::from_rgb(1, 2, 3);
        assert_eq!(parse_hex(&hex(c)), Some(c));
    }

    /// The schedule must be monotonic and start after the initial pause — a non-monotonic schedule
    /// makes `revealed` jump backwards, which looks like the text flickering.
    #[test]
    fn schedule_is_monotonic_and_pauses_on_punctuation() {
        let s = schedule("你好，世界");
        assert_eq!(s.len(), 5);
        assert!(s.windows(2).all(|w| w[1] > w[0]), "must be strictly increasing: {s:?}");
        assert!((s[0] - FIRST_CHAR).abs() < 1e-6);
        // The pause lands **after** the comma: the gap following it is longer than a plain char
        let plain = schedule("你好世界!");
        assert!(s[3] - s[2] > plain[3] - plain[2], "punctuation has to breathe");
    }

    /// Every phase has to answer "how much is shown" without panicking, including the empty ones.
    #[test]
    fn empty_text_schedules_nothing() {
        assert!(schedule("").is_empty());
    }

    /// A custom line that the user cleared means a **quiet** first screen: no local substitution,
    /// no request. If this ever regresses, clearing the field would silently start fetching quotes.
    #[test]
    fn empty_custom_text_shows_nothing_and_asks_nothing() {
        let mut st = Stage::new(Prefs { custom: "   ".into(), ..Prefs::default() });
        let ask = st.sync(true);
        assert!(ask.is_none(), "no source to ask");
        assert!(matches!(st.phase, Phase::Idle));
    }

    /// Entering the first screen once asks **once**; staying there must not fire a request per
    /// frame (that would hammer the API at 60 requests a second).
    #[test]
    fn the_request_fires_on_the_edge_only() {
        let mut st = Stage::new(Prefs { source: Source::Hitokoto, ..Prefs::default() });
        assert!(matches!(st.sync(true), Some(Ask::Api(_))));
        assert!(st.sync(true).is_none(), "already waiting");
        assert!(st.sync(true).is_none());
        st.sync(false);
        assert!(matches!(st.sync(true), Some(Ask::Api(_))));
    }

    /// A late answer replaces the local fallback — the fallback is a stand-in, not a choice.
    #[test]
    fn a_late_answer_takes_over_from_the_local_fallback() {
        let mut st = Stage::new(Prefs { source: Source::Hitokoto, ..Prefs::default() });
        st.sync(true);
        st.on_line(false, String::new(), "boom".into()); // the request failed outright
        assert!(matches!(st.phase, Phase::Typing { .. }), "the local pool took over");
        assert!(st.fallback_shown);
        st.on_line(true, "真正的句子".into(), String::new());
        match &st.phase {
            Phase::Typing { text, .. } => assert_eq!(text, "真正的句子"),
            _ => panic!("the real line has to replace the fallback"),
        }
        assert!(!st.fallback_shown, "what is on screen is no longer a stand-in");
        assert_eq!(st.pending_line(), None, "the line was used, not filed");
    }

    /// Two **real** lines are never swapped mid-screen: the second one waits for next time.
    #[test]
    fn a_late_answer_after_a_real_line_waits() {
        let mut st = Stage::new(Prefs { source: Source::Hitokoto, ..Prefs::default() });
        st.sync(true);
        st.on_line(true, "第一句".into(), String::new());
        st.on_line(true, "第二句".into(), String::new());
        match &st.phase {
            Phase::Typing { text, .. } => assert_eq!(text, "第一句"),
            _ => panic!("the first line stays on screen"),
        }
        assert_eq!(st.pending_line(), Some("第二句"));
        // Next first screen uses it without asking again
        st.sync(false);
        assert!(st.sync(true).is_none(), "the pending line is used, no new request");
        match &st.phase {
            Phase::Typing { text, .. } => assert_eq!(text, "第二句"),
            _ => panic!("the pending line should start typing"),
        }
    }

    /// A model call is allowed to take much longer than a quoting API before the local pool stands in: 6 s versus 1.2...
    #[test]
    fn the_model_gets_a_longer_grace_than_the_api() {
        let api = Stage::new(Prefs { source: Source::Hitokoto, ..Prefs::default() });
        let model = Stage::new(Prefs { source: Source::Model, ..Prefs::default() });
        assert!(model.grace() > api.grace());
        assert!((api.grace() - GRACE).abs() < 1e-6);
        assert!((model.grace() - MODEL_GRACE).abs() < 1e-6);
    }

    /// "Another one" from the settings page is the documented exception: it replaces what is shown.
    #[test]
    fn an_explicit_refresh_replaces_the_visible_line() {
        let mut st = Stage::new(Prefs { source: Source::Model, ..Prefs::default() });
        st.sync(true);
        st.on_line(true, "第一句".into(), String::new());
        assert!(st.refresh().is_some());
        st.on_line(true, "第二句".into(), String::new());
        match &st.phase {
            Phase::Typing { text, .. } => assert_eq!(text, "第二句"),
            _ => panic!("the refresh must replace the line"),
        }
    }

    /// Typing off = the sentence is there immediately; no caret, no animation.
    #[test]
    fn typing_off_shows_the_line_at_once() {
        let mut st = Stage::new(Prefs {
            source: Source::Custom,
            custom: "准备就绪".into(),
            typing: false,
            ..Prefs::default()
        });
        st.sync(true);
        match &st.phase {
            Phase::Shown { text } => assert_eq!(text, "准备就绪"),
            _ => panic!("no animation when typing is off"),
        }
    }

    /// Drawing a long sentence must shrink and then cut, never overflow the stage: the line is painted straight onto...
    #[test]
    fn a_long_line_shrinks_instead_of_overflowing() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut st = Stage::new(Prefs {
            custom: "很长的一句话".repeat(12),
            typing: false,
            ..Prefs::default()
        });
        let stage = Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 600.0));
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                st.sync(true);
                st.draw(ui, stage);
            });
        });
        let fit = st.fit.as_ref().expect("the line was laid out");
        let max_w = (stage.width() - 96.0).clamp(160.0, 720.0);
        assert!(
            fit.galley.size().x <= max_w + 1.0,
            "the line is {:.1} pt wide but the stage allows {max_w:.1}",
            fit.galley.size().x
        );
        assert!(fit.text.ends_with('…'), "overlong text has to show that it was cut");
    }

    /// A window too small to hold a line simply has no line — no panic, no negative rect.
    #[test]
    fn a_tiny_stage_draws_nothing() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut st = Stage::new(Prefs { custom: "你好".into(), ..Prefs::default() });
        let stage = Rect::from_min_size(Pos2::ZERO, Vec2::new(120.0, 80.0));
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                st.sync(true);
                st.draw(ui, stage);
            });
        });
        assert!(st.fit.is_none(), "nothing was laid out for a stage this small");
    }
}
