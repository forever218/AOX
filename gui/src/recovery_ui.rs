//! The recovery panel: "undo the last task", reached from a chip beside the workspace button.
//!
//! # Why it lives here and not in the title bar
//!
//! The title bar is about **this window** (maximize, close, the drawer). Rolling back is about
//! **this workspace's files** — the same question the workspace button answers ("under what
//! permissions does this conversation run / how far back can it go"). So it sits one chip to the
//! right of that button, in the row the hand is already in when a turn turns out wrong.
//!
//! # Conventions (the same three as [`crate::workplace_ui`])
//!
//! **① State lives in [`RecoveryUi`], owned by `App`.** This module never touches an `App` field.
//!
//! **② Parameters in, requests out.** Every entry point returns `Option<RecoveryReq>`, meaning "the
//! UI wants the agent thread to do this". The main thread must never read the recovery store: it is
//! the agent thread that owns `Runtime`, and a synchronous store read here would freeze the window
//! while a 100 MB tree is being walked.
//!
//! **③ The list is pushed in, not asked for.** Rows arrive as [`aox::recovery::view::PointsView`]
//! through the bridge. The UI keeping its own idea of "which points exist" would be a second source
//! of truth, and the first thing to drift would be the thing the user acts on.
//!
//! # Two steps, never one
//!
//! Clicking a row **unfolds a preview** (paths, counts, warnings, and the two things that make a
//! rollback unsafe: drift and incomplete coverage). The rollback itself needs a second click in a
//! modal, and — when either hazard is present — a checkbox that says which hazard the user is
//! accepting. A single click that overwrites files is a bug, not a shortcut.
//!
//! # Shapes: a row is a card, not a stripe
//!
//! Everything inside the panel is inset from its edges — rows, dividers, the preview block. A fill
//! that runs edge to edge reads as a selected *list item* (or worse, as a scrollbar track); a fill
//! with 8 points of air on both sides reads as a card, which is what these are.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use egui::{
    Align2, Color32, CornerRadius, Id, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2,
};

use aox::recovery::view::{PointsView, PreviewView};

use crate::app::{danger_button, dialog_footer, flat_button, pointer_if, primary_button};
use crate::bridge::{RecoveryReq, RecoveryView};
use crate::theme::{self, P};

/// The panel's width, and the row height inside it.
const PANEL_W: f32 = 420.0;
const ROW_H: f32 = 52.0;
/// How much a row's fill is pulled away from the panel edges. This one number is the difference
/// between "a stripe" and "a card".
const ROW_INSET: f32 = 8.0;
/// The preview block's inset, and the panel's own content gutter.
const CARD_INSET: f32 = 12.0;
/// How tall the list may grow before it scrolls (the panel still fits above the input box).
const LIST_MAX_H: f32 = 320.0;
/// Rows drawn before "show all" — the same default as `/checkpoints`.
const ROWS_FIRST: usize = 20;
/// The preview's unfold. Short enough to feel immediate, long enough to read as motion — the same
/// order as the drawer's slide and the plan block's disclosure.
const DETAIL_ANIM_SECS: f32 = 0.18;
/// The panel's own appearance clock, separate from the preview's: a popover that takes as long to
/// arrive as a disclosure to open feels broken, and one that lingers on the way out feels stuck.
/// Asymmetric on purpose — leaving is quicker than arriving, which is what every desktop menu does.
const OPEN_SECS: f32 = 0.12;
const CLOSE_SECS: f32 = 0.15;
/// How far the panel travels while it arrives and leaves: straight down, toward the chip it belongs
/// to. Six pixels is enough to read as direction and small enough not to be seen as a slide.
const SINK: f32 = 6.0;
/// The foot's metric. All four actions share these, which is the whole point of them: the row used
/// to be two ghost buttons (13.5 pt, 34 pt hot zone) followed by two bare labels (11 pt, text height
/// only), so it changed type size, height and hover behaviour halfway along.
///
/// `FOOT_H` is [`crate::app::BTN_H`] so the panel's height is exactly what it was — the row got
/// *narrower*, not taller.
const FOOT_H: f32 = crate::app::BTN_H;
/// 12 pt: a step under the 13.5 of a real button, because these are the panel's third-rate actions,
/// and a step over the 11 of the meta text, because they are still things you click.
const FOOT_SIZE: f32 = 12.0;
/// Horizontal padding. Doubles as the **gap** between two neighbours (`item_spacing.x` is zeroed),
/// so the row stays even however long one label is: 8 + 8 = 16 pt between them, whatever the text.
/// The gutter in front of the row is `CARD_INSET + 2 - FOOT_PAD_X`, so the first label starts on
/// `CARD_INSET + 2` — the same vertical line as the panel's own title and its caption lines.
const FOOT_PAD_X: f32 = 8.0;
const FOOT_RADIUS: u8 = 6;

/// A rollback waiting for the second click.
struct Ask {
    target: String,
    label: String,
    /// Files changed outside AOX would be overwritten: `--force` territory.
    overwrite: bool,
    /// The point's coverage is incomplete: `--allow-partial` territory.
    partial: bool,
    /// The user ticked the box. Required only when one of the two above is set.
    agreed: bool,
    /// This point was recorded by a different conversation. Not a hazard, but the user is about to
    /// undo work that did not happen in the chat they are looking at, and that is worth one clause.
    foreign: bool,
}

pub struct RecoveryUi {
    open: bool,
    /// 0 = not on screen, 1 = fully there. `open` is the **target**, this is what is drawn, so the
    /// panel stays a single unbroken picture while it dissolves instead of being taken away in one
    /// frame (which reads as the panel being sliced off at the bottom).
    open_t: f32,
    /// The same `(started, from, to)` clock as the preview's, for the same reason: a dismissal
    /// reversed by a second click carries on from where it is.
    open_anim: Option<(Instant, f32, f32)>,
    /// Where the chip was on the last frame; the panel anchors to it.
    ///
    /// Stored rather than passed because the chip is drawn inside the composer's nested layout,
    /// while the panel has to be drawn at the top level (above the transcript) — the same split
    /// `workplace_ui` uses for its menu, and for the same reason.
    anchor: Option<Rect>,
    view: PointsView,
    /// Which row the user asked to unfold. `None` = none, and the preview below starts folding away.
    expanded: Option<String>,
    /// The row whose preview is on screen **right now** — kept after `expanded` goes back to `None`
    /// so the block can shrink instead of vanishing.
    detail_id: Option<String>,
    /// 0 = folded, 1 = open.
    detail_t: f32,
    /// `(started, from, to)` — the same frame-by-frame clock the drawer and the plan block use, so a
    /// reversal mid-flight continues from where it is instead of jumping back to zero.
    detail_anim: Option<(Instant, f32, f32)>,
    /// The preview's natural height, measured while it draws. Animating only the **clip** means the
    /// content never re-wraps while it unfolds, which is what makes it look smooth rather than busy.
    detail_full_h: f32,
    /// The preview for `expanded`, once it arrives.
    plan: Option<PreviewView>,
    confirm: Option<Ask>,
    /// "Clear every point in this workspace" is waiting for its answer. A separate flag rather than a
    /// variant of [`Ask`]: that one carries per-row hazards (overwrite / partial / foreign) which mean
    /// nothing here, and reusing it would make the rollback dialog's promises appear on a delete.
    clear_ask: bool,
    /// The last thing that happened, shown inside the panel. `true` = it is a failure.
    notice: Option<(String, bool)>,
    /// A request is in flight.
    busy: bool,
    /// Opened while a turn was running, so the list could not be asked for yet.
    waiting_turn: bool,
    /// The user asked for the full list ("show all").
    show_all: bool,
}

impl Default for RecoveryUi {
    fn default() -> Self {
        Self::new()
    }
}

impl RecoveryUi {
    pub fn new() -> Self {
        Self {
            open: false,
            open_t: 0.0,
            open_anim: None,
            anchor: None,
            view: PointsView::default(),
            expanded: None,
            detail_id: None,
            detail_t: 0.0,
            detail_anim: None,
            detail_full_h: 0.0,
            plan: None,
            confirm: None,
            clear_ask: false,
            notice: None,
            busy: false,
            waiting_turn: false,
            show_all: false,
        }
    }

    /// A view arrived from the agent thread.
    pub fn set_view(&mut self, view: RecoveryView) {
        self.busy = false;
        match view {
            RecoveryView::Points(v) => {
                self.view = *v;
                // A point that no longer exists (pruned elsewhere) must not keep an unfolded
                // preview on screen: it would describe a rollback that cannot happen.
                if let Some(id) = &self.expanded {
                    if !self.view.rows.iter().any(|r| &r.id == id) {
                        self.expanded = None;
                        self.plan = None;
                    }
                }
            }
            RecoveryView::Preview(v) => {
                // Matched on the **requested** id, the one this panel sent. Matching on the resolved
                // node instead compares "the row I clicked" with "the row before it" — every answer
                // would be dropped and the row would sit on "reading…" forever.
                if Some(&v.requested) == self.expanded.as_ref() {
                    self.plan = Some(*v);
                }
            }
            RecoveryView::Notice { text, ok } => {
                self.notice = Some((text, !ok));
                // A successful rollback changed the list; a failure did not, but re-asking is
                // cheap and keeps the panel honest about "the point you clicked is still there".
                self.expanded = None;
                self.plan = None;
            }
        }
    }

    /// A request just went out: rows stop being clickable until the answer arrives.
    pub fn mark_busy(&mut self) {
        self.busy = true;
    }

    /// The panel is on screen waiting for the agent thread. The host polls a repaint at ~30 ms while
    /// this is true: an idle egui window does not repaint itself, so the answer would otherwise sit
    /// in the channel until the next mouse move.
    pub fn is_waiting(&self) -> bool {
        self.open && (self.busy || self.waiting_turn)
    }

    /// The workspace changed under us (a different conversation / tier): the old rows belong to it.
    ///
    /// This one does **not** fade. The rows describe a tree the user is no longer looking at, so a
    /// slow dissolve would be animation over the top of something that has already stopped being
    /// true — the panel is simply gone, and the next open starts from a clean slate.
    pub fn close(&mut self) {
        self.open = false;
        self.open_t = 0.0;
        self.open_anim = None;
        self.expanded = None;
        self.detail_id = None;
        self.detail_t = 0.0;
        self.detail_anim = None;
        self.plan = None;
        self.confirm = None;
        self.notice = None;
        self.view = PointsView::default();
    }

    /// Called when a turn ends: a panel opened mid-turn was waiting for exactly this.
    pub fn turn_ended(&mut self) -> Option<RecoveryReq> {
        if self.open && self.waiting_turn {
            self.waiting_turn = false;
            self.busy = true;
            return Some(RecoveryReq::List);
        }
        None
    }

    /// Draw the chip. `enabled == false` still draws it (dimmed) unless the caller hides it entirely.
    ///
    /// Returns the request to send, if this click asked for one.
    pub fn chip(&mut self, ui: &mut Ui, enabled: bool, turn_running: bool) -> Option<RecoveryReq> {
        let count = self.view.rows.len();
        let text = if count > 0 { format!("{count}") } else { String::new() };
        let font = egui::FontId::proportional(13.0);
        let galley = ui.painter().layout_no_wrap(text.clone(), font.clone(), Color32::PLACEHOLDER);
        // 14pt box for the glyph, then the count. The gap only exists when there is a count, so an
        // empty chip is a small square instead of a wide empty pill.
        let icon = 16.0;
        let gap = if text.is_empty() { 0.0 } else { 6.0 };
        let size = Vec2::new(icon + gap + galley.size().x + 16.0, 24.0);
        let sense = if enabled { Sense::click() } else { Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(size, sense);
        let hov = enabled && resp.hovered();
        if hov {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let lit = hov || self.open;
        if lit {
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(7),
                if self.open { P::hover_fill() } else { P::surface_hi() },
            );
        }
        // Three tiers, and the quiet one is only ever used for "nothing to undo" — a disabled-looking
        // control that is still clickable, because the empty state has something to say.
        let fg = if !enabled {
            P::quiet()
        } else if lit {
            P::ink()
        } else {
            P::text_dim()
        };
        let cy = rect.center().y;
        let icon_center = Pos2::new(rect.left() + 8.0 + icon * 0.5, cy);
        paint_undo(ui.painter(), icon_center, icon, fg);
        if !text.is_empty() {
            let g = ui.painter().layout_no_wrap(text, font, fg);
            ui.painter().galley(
                Pos2::new(icon_center.x + icon * 0.5 + gap, cy - g.size().y * 0.5),
                g,
                fg,
            );
        }
        self.anchor = Some(rect);
        if !resp.clicked() || !enabled {
            return None;
        }
        self.open = !self.open;
        self.notice = None;
        if !self.open {
            // Nothing is cleared here: the panel is about to fade, and the preview block is part of
            // the picture that fades. `panel` forgets it once the clock reaches zero.
            return None;
        }
        // While a turn is running the driver loop is not reading the queue, so asking now would
        // leave the panel "loading" for the whole turn. Wait for it instead (see `turn_ended`).
        if turn_running {
            self.waiting_turn = true;
            return None;
        }
        self.busy = true;
        Some(RecoveryReq::List)
    }

    /// The panel itself. Drawn at the top level so it sits above the transcript and the input card.
    pub fn panel(&mut self, ctx: &egui::Context) -> Option<RecoveryReq> {
        self.step_open(ctx);
        if self.open_t <= 0.001 {
            // Gone. Only now do the rows and the preview get thrown away — doing it on the click that
            // dismissed the panel would delete the bottom half of a panel that is still on screen.
            self.forget_transient();
            return None;
        }
        let Some(anchor) = self.anchor else { return None };
        // Frozen while leaving: the preview must not go on folding under a panel that is dissolving,
        // or the panel would change shape on its way out. It fades as the picture it already was.
        if self.open {
            self.step_detail(ctx);
        }
        let t = self.open_t;
        let mut out: Option<RecoveryReq> = None;
        // Up, not down: the chip lives at the bottom of the window.
        //
        // ⚠️ Coordinates here are **client-area local** (`Area::fixed_pos`) — never mix in
        // `viewport().inner_rect`, which is monitor space. `workplace_ui::menu` and the usage panel
        // both learned this the hard way; the symptom is a panel sliding with the window.
        let pos = anchor.left_top() - egui::vec2(0.0, 6.0) + egui::vec2(0.0, (1.0 - t) * SINK);
        let width = PANEL_W.min(ctx.content_rect().width() - 24.0).max(300.0);
        let area = egui::Area::new(Id::new("aox-recovery-panel"))
            .fixed_pos(pos)
            .pivot(Align2::LEFT_BOTTOM)
            .order(egui::Order::Foreground)
            // A dissolving panel is pixels, not a target: without this the last 150 ms would still
            // eat the click that dismissed it if the pointer happened to land on a row.
            .interactable(self.open)
            .show(ctx, |ui| {
                // One opacity over the whole popup — frame, rows, text — so it leaves as **one
                // picture**. Cutting the height instead is what made it look sliced.
                ui.set_opacity(t);
                egui::Frame::popup(ui.style())
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(12))
                    .stroke(Stroke::new(1.0, P::edge_raised()))
                    .inner_margin(Margin::same(0))
                    .show(ui, |ui| {
                        ui.set_width(width);
                        self.body(ui, &mut out);
                    })
                    .response
                    .rect
            })
            .inner;
        if !self.open {
            // Closing: the panel answers nothing while it leaves.
            return None;
        }
        // A click anywhere else puts it away. Tested after drawing, so the click that opened it
        // cannot immediately close it again.
        let inside = anchor.contains(ctx.pointer_hover_pos().unwrap_or_default())
            || area.contains(ctx.pointer_hover_pos().unwrap_or_default());
        let clicked = ctx.input(|i| i.pointer.any_click());
        if clicked && !inside {
            self.open = false;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            // Two stages: the first Esc folds the preview, the second closes the panel. Losing the
            // whole panel because you wanted to fold one row is the kind of small theft that makes
            // people stop using a UI.
            if self.expanded.is_some() {
                self.expanded = None;
                self.plan = None;
            } else {
                self.open = false;
            }
        }
        out
    }

    /// Throw away everything the panel was showing. Called once the dissolve is over, and by
    /// [`Self::close`], so a panel that comes back never shows a preview of a row that was folded
    /// while it was invisible.
    fn forget_transient(&mut self) {
        self.expanded = None;
        self.plan = None;
        self.detail_id = None;
        self.detail_t = 0.0;
        self.detail_anim = None;
    }

    /// Advance the appearance clock — the same idiom as [`Self::step_detail`]: step by elapsed time,
    /// keep asking for frames until it settles, and reverse from wherever it is.
    ///
    /// The easing differs by direction, and that is the whole point of this function:
    ///
    /// - **Arriving** uses ease-out: most of the distance in the first third, then a soft landing.
    /// - **Leaving** uses a smoothstep (zero velocity at *both* ends). Ease-out on the way out is
    ///   what a naive clock gives you, and it is wrong here: the panel would lose 70% of its opacity
    ///   in the first 30 ms and then crawl away as a ghost. Dismissal wants the opposite — hold, then
    ///   go — so `open_t` starts its descent slowly and reaches zero at speed.
    fn step_open(&mut self, ctx: &egui::Context) {
        let target = if self.open { 1.0 } else { 0.0 };
        match self.open_anim {
            Some((_, _, to)) if to != target => {
                self.open_anim = Some((Instant::now(), self.open_t, target));
                ctx.request_repaint();
            }
            Some((started, from, to)) => {
                let dur = if to > from { OPEN_SECS } else { CLOSE_SECS };
                let k = (started.elapsed().as_secs_f32() / dur).clamp(0.0, 1.0);
                let e = if to > from {
                    crate::app::ease_out_cubic(k)
                } else {
                    k * k * (3.0 - 2.0 * k)
                };
                self.open_t = from + (to - from) * e;
                if k >= 1.0 {
                    self.open_t = to;
                    self.open_anim = None;
                }
                ctx.request_repaint();
            }
            None if self.open_t != target => {
                self.open_anim = Some((Instant::now(), self.open_t, target));
                ctx.request_repaint();
            }
            None => {}
        }
        self.open_t = self.open_t.clamp(0.0, 1.0);
    }

    /// Advance the unfold clock. Same idiom as the drawer's slide and the plan block's disclosure:
    /// step by elapsed time, ease out, and keep asking for frames until it settles.
    ///
    /// **The clock does not start until the card's height is known.** That is the whole reason this
    /// panel used to jolt: `detail_full_h` began as a guess, so the first real measurement moved the
    /// block by tens of pixels in one frame, and the panel (pivoted at its bottom edge) jumped with
    /// it. Now the preview is laid out at a clip height of zero for one frame — invisible, but
    /// measured — and only then does the ramp begin, from a number that will not change.
    fn step_detail(&mut self, ctx: &egui::Context) {
        if let Some(id) = &self.expanded {
            if self.detail_id.as_deref() != Some(id.as_str()) {
                // A different row: start from zero. Two blocks cross-fading would need a clock each,
                // and the click that causes it is one per frame at most.
                self.detail_id = Some(id.clone());
                self.detail_t = 0.0;
                // **Zero = "not measured yet"**, and the zero clip means it costs the panel no height
                // on the frame that measures it.
                self.detail_full_h = 0.0;
                self.detail_anim = None;
            }
        }
        if self.detail_id.is_none() {
            return;
        }
        // Folded away, or open only once the content that defines its height is here.
        let target = if self.expanded.is_some() && self.plan.is_some() { 1.0 } else { 0.0 };
        match self.detail_anim {
            // Reversed mid-flight: carry on from wherever it is, never from 0.
            Some((_, _, to)) if to != target => {
                self.detail_anim = Some((Instant::now(), self.detail_t, target));
                ctx.request_repaint();
            }
            Some((started, from, to)) => {
                let k = (started.elapsed().as_secs_f32() / DETAIL_ANIM_SECS).clamp(0.0, 1.0);
                self.detail_t = from + (to - from) * crate::app::ease_out_cubic(k);
                if k >= 1.0 {
                    self.detail_t = to;
                    self.detail_anim = None;
                }
                ctx.request_repaint();
            }
            None if self.detail_t != target => {
                self.detail_anim = Some((Instant::now(), self.detail_t, target));
                ctx.request_repaint();
            }
            None => {}
        }
        // `detail_id` means "the row the block is drawn under", so it is kept while the row is
        // expanded even at zero height — that is where the one-line placeholder lives. It goes only
        // once the row is no longer the one asked for **and** the fold-out has finished.
        if self.expanded.is_none() && self.detail_t <= 0.001 {
            self.detail_t = 0.0;
            self.detail_id = None;
            // The preview goes with the block: keeping it would leave a stale card to reappear if
            // the same row were opened again, and clearing it earlier would fold a card that still
            // has to be drawn.
            self.plan = None;
        }
    }

    fn body(&mut self, ui: &mut Ui, out: &mut Option<RecoveryReq>) {
        // ── head ──
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(CARD_INSET + 2.0);
            ui.label(
                RichText::new(aox::i18n::t("Recovery points"))
                    .size(13.0)
                    .strong()
                    .color(P::text()),
            );
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(CARD_INSET + 2.0);
                ui.label(
                    RichText::new(aox::tf!("{}", self.view.rows.len()))
                        .size(11.0)
                        .color(P::text_dim()),
                );
            });
        });
        ui.add_space(3.0);
        ui.horizontal(|ui| {
            ui.add_space(CARD_INSET + 2.0);
            // The workspace is part of the title because a rollback is **scoped**: with several
            // workspaces open, "recovery points" without a root is a question, not an answer.
            let sub = if self.view.keep == 0 {
                aox::tf!("{} · no retention limit", elide(&self.view.root, 44))
            } else {
                aox::tf!("{} · keeps {} task(s)", elide(&self.view.root, 40), self.view.keep)
            };
            ui.label(RichText::new(sub).size(11.0).color(P::text_dim()));
        });
        // The chain belongs to the **workspace**, not to this conversation, and a new conversation
        // opened on the same folder inherits it. Saying so once, at the top, is cheaper than making
        // the user work it out from a row that says "other conversation".
        if self.view.rows.iter().any(|r| !r.mine) {
            ui.add_space(3.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(CARD_INSET + 2.0);
                ui.label(
                    RichText::new(aox::i18n::t(
                        "Shared by every conversation in this workspace; each row says who recorded it.",
                    ))
                    .size(11.0)
                    .color(P::quiet()),
                );
            });
        }
        ui.add_space(9.0);

        // ── list ──
        if self.busy && self.view.rows.is_empty() {
            skeleton(ui);
        } else if self.waiting_turn {
            line(ui, aox::i18n::t("Wait for this turn to finish; then the points are readable."), false);
        } else if self.view.rows.is_empty() {
            line(ui, aox::i18n::t("No recovery points yet."), false);
            line(
                ui,
                aox::i18n::t("A point is written after a task changes files; a read-only conversation leaves nothing behind."),
                false,
            );
        } else {
            let limit = if self.show_all { usize::MAX } else { ROWS_FIRST };
            // The list's budget grows with the card that is unfolding inside it. Without this the
            // scroll area hits `LIST_MAX_H` halfway through the unfold and does the worst possible
            // thing: stops the panel from growing (the card then unfolds *inside* the fixed viewport
            // and gets clipped), pops a scrollbar in — which narrows the card by 10 px, re-wrapping
            // its text and changing its height again — and, when the card folds, pops it back out.
            // Measured on a real window: the panel's top edge stepped 622 → 513 → 464 → 425 px.
            let card = if self.detail_id.is_some() {
                self.detail_full_h.max(0.0) * self.detail_t.clamp(0.0, 1.0)
            } else {
                0.0
            };
            egui::ScrollArea::vertical()
                .max_height(LIST_MAX_H + card)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for i in 0..self.view.rows.len().min(limit) {
                        self.row(ui, i, out);
                    }
                });
            if self.view.rows.len() > ROWS_FIRST && !self.show_all {
                ui.horizontal(|ui| {
                    ui.add_space(CARD_INSET);
                    if flat_button(ui, &aox::tf!("Show all {} points", self.view.rows.len())).clicked() {
                        self.show_all = true;
                    }
                });
                ui.add_space(4.0);
            }
        }

        // ── notice ──
        if let Some((text, err)) = &self.notice {
            let color = if *err { P::danger() } else { P::text_dim() };
            ui.horizontal_wrapped(|ui| {
                ui.add_space(CARD_INSET + 2.0);
                ui.label(RichText::new(elide(text, 260)).size(11.0).color(color));
            });
            ui.add_space(6.0);
        }

        // ── foot ──
        //
        // Four actions, one shape (2026-10-28). They used to be two ghost buttons and two bare
        // 11 pt labels: two type sizes, two hot-zone heights (34 pt vs. the text's own ~14), and two
        // answers to "the pointer is on me" — a wash for the first pair, nothing at all for the
        // second. A foot that changes shape halfway along reads as two groups, and the second group
        // then reads as inert text that happened to be clickable.
        //
        // `horizontal_wrapped` rather than `horizontal`: on a narrow window the four labels are the
        // first thing in this panel that can run out of width, and wrapping keeps the last one
        // reachable where clipping would simply hide it.
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            // The gap is each action's own padding, not `item_spacing`: that is what keeps the row
            // even when one label is twice as long as its neighbour.
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space(CARD_INSET + 2.0 - FOOT_PAD_X);
            if foot_action(ui, aox::i18n::t("Verify"), Foot::Quiet).clicked() {
                self.busy = true;
                *out = Some(RecoveryReq::Verify);
            }
            if foot_action(ui, aox::i18n::t("Prune old points"), Foot::Quiet).clicked() {
                self.busy = true;
                *out = Some(RecoveryReq::Prune);
            }
            // The redo door is only lit when a safety snapshot is actually waiting; a permanently
            // clickable "undo the undo" that answers "nothing to redo" teaches nothing. Disabled,
            // not hidden: the four actions must stay in one row, at one height, when one of them is
            // not available.
            if foot_action_ex(ui, aox::i18n::t("Redo last rollback"), Foot::Quiet, self.view.redoable)
                .clicked()
            {
                self.busy = true;
                *out = Some(RecoveryReq::Redo);
            }
            // Destructive, so it is the one item here in a colour: red at rest, and it opens a
            // question instead of firing (see [`Self::clear_confirm`]).
            if foot_action_ex(ui, aox::i18n::t("Clear history"), Foot::Danger, !self.busy).clicked() {
                self.clear_ask = true;
            }
        });
        ui.add_space(9.0);
        if self.view.orphans > 0 {
            // Said out loud because the alternative is worse: points that exist on disk, cannot be
            // rolled back to, and would silently disappear at the next prune.
            line(
                ui,
                &aox::tf!(
                    "{} point(s) from before an earlier rollback are no longer reachable; /backup prune clears them.",
                    self.view.orphans
                ),
                false,
            );
        }
        if self.view.truncated {
            line(
                ui,
                aox::i18n::t("Older points were dropped by [recovery] keep; only the retained chain can be rolled back."),
                false,
            );
        }
    }

    /// One row: two lines of text on a card, plus its preview underneath.
    fn row(&mut self, ui: &mut Ui, index: usize, out: &mut Option<RecoveryReq>) {
        let row = self.view.rows[index].clone();
        let selected = self.expanded.as_deref() == Some(row.id.as_str());
        // While a request is in flight the rows stop answering: a second click would queue a second
        // rollback against a list that is about to change. Disabled, not hidden — the shape stays put.
        let (rect, resp) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), ROW_H),
            if self.busy { Sense::hover() } else { Sense::click() },
        );
        let hov = pointer_if(ui, &resp) && !self.busy;
        // The fill is **inset**: a row is a card, not a stripe. Edge-to-edge fills read as either a
        // selected list item or a scrollbar track, and neither is what this is.
        let card = rect.shrink2(Vec2::new(ROW_INSET, 2.0));
        if hov || selected || row.head {
            ui.painter().rect_filled(card, CornerRadius::same(8), P::hover_fill());
        }
        // The head marker is a bar, not a second fill: hover would otherwise erase the difference
        // between "this is current" and "the pointer is here". Inset with the card so it cannot be
        // mistaken for a scrollbar.
        if row.head {
            let bar = Rect::from_min_max(
                Pos2::new(card.right() - 4.0, card.top() + 10.0),
                Pos2::new(card.right() - 2.0, card.bottom() - 10.0),
            );
            ui.painter().rect_filled(bar, CornerRadius::same(1), P::ink());
        }
        let text_left = card.left() + 12.0;
        let title = if row.label.is_empty() { kind_word(row.kind).to_string() } else { row.label.clone() };
        // Elided by **width**, not by character count: `elide(&title, 46)` was eliding 46 *chars*,
        // which is 46 Latin letters (~300 px) but 46 Chinese ones (~600 px) — so every Chinese message
        // painted straight through the panel's right edge (user report, 2026-10-28). `crate::app::elide`
        // measures with the same font that will be drawn.
        let text_w = (card.right() - 12.0 - text_left).max(40.0);
        let title_g = crate::app::elide(ui, &title, theme::body_font(13.0), P::text(), text_w);
        ui.painter().galley(Pos2::new(text_left, card.top() + 9.0), title_g, P::text());
        let meta = if selected && self.busy {
            // The wait is reported **on the row's second line**, not under it: same height, same
            // width, no layout change — so the panel does not move while the store is read.
            aox::i18n::t("Reading what this point would restore…").to_string()
        } else if row.mine {
            aox::tf!(
                "{} · {} · {} path(s) · {}",
                kind_word(row.kind),
                ago(row.created_at, now()),
                row.paths,
                status_word(row.status)
            )
        } else {
            aox::tf!(
                "{} · {} · {} · {} path(s) · {}",
                aox::i18n::t("other conversation"),
                kind_word(row.kind),
                ago(row.created_at, now()),
                row.paths,
                status_word(row.status)
            )
        };
        let meta_g = crate::app::elide(ui, &meta, theme::body_font(11.0), P::text_dim(), text_w);
        ui.painter().galley(Pos2::new(text_left, card.top() + 30.0), meta_g, P::text_dim());
        if hov {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() && !self.busy {
            if selected {
                // Only the target flips. `plan` stays: the fold-out still has to draw the card it
                // is folding, and clearing it here would swap the card for a one-line "reading…"
                // on the very frame the collapse starts — a 200 px jump for a 16 px click.
                self.expanded = None;
            } else {
                self.expanded = Some(row.id.clone());
                // Only a **different** row makes the old preview irrelevant. Re-opening a card that
                // is still folding away keeps it: dropping it would put the one-line placeholder
                // under a card that is half on screen.
                if self.detail_id.as_deref() != Some(row.id.as_str()) {
                    self.plan = None;
                }
                self.busy = true;
                *out = Some(RecoveryReq::Preview { target: row.id.clone() });
            }
        }
        // Drawn for the row that owns the clock, even while it is folding away — that is what makes
        // the close animate instead of snap.
        if self.detail_id.as_deref() == Some(row.id.as_str()) {
            if self.plan.is_some() {
                self.detail(ui, &row.id);
            } else if !self.busy {
                // Only the **failure** state is drawn here. While the request is in flight nothing
                // is added: a one-line "reading…" would push the panel up by a line and then vanish
                // again when the card arrived — two steps for one gesture, which is exactly the
                // flicker this block was reported for. The row's own selected fill is the feedback.
                self.placeholder(ui);
            }
        }
    }

    /// The one line shown when a preview never arrived.
    fn placeholder(&mut self, ui: &mut Ui) {
        line(ui, aox::i18n::t("This point could not be read. Tap the row again."), true);
    }

    /// The preview, folding open.
    ///
    /// Only the **clip** animates: the content is laid out at its natural size every frame and cut
    /// off, so nothing re-wraps mid-flight. Animating the content's own width/height would make the
    /// text reflow on every frame, which is what "janky" actually looks like.
    fn detail(&mut self, ui: &mut Ui, row_id: &str) {
        let t = self.detail_t;
        let w = ui.available_width();
        // `0` means "not measured yet": lay the content out with plenty of room and take the number
        // without reserving a single pixel, so the measuring frame cannot move the panel.
        let known = self.detail_full_h > 1.0;
        let full = if known { self.detail_full_h } else { 600.0 };
        let shown = if known { (full * t).max(0.0) } else { 0.0 };
        // Reserve exactly `shown`, then lay the content out underneath at `full` height inside a
        // clipping child. `allocate_exact_size` + `new_child` is the pair that keeps the reservation
        // and the layout independent — the same trick the title bar uses for its plates.
        let (slot, _) = ui.allocate_exact_size(Vec2::new(w, shown), Sense::hover());
        let area = Rect::from_min_size(slot.min, Vec2::new(w, full.max(1.0)));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(area));
        child.set_clip_rect(ui.clip_rect().intersect(Rect::from_min_size(slot.min, Vec2::new(w, shown))));
        let measured = self.detail_body(&mut child, row_id);
        // Measured every frame, and **only ever allowed to grow**. The layout does not depend on the
        // clip, so this is stable while it opens; the `max` guards the mid-fold case, where the plan
        // is deliberately kept and a shrinking number would move the panel twice for one gesture.
        if measured > 0.5 && measured > self.detail_full_h {
            self.detail_full_h = measured;
        }
    }

    fn detail_body(&mut self, ui: &mut Ui, row_id: &str) -> f32 {
        // `min_rect()` is in absolute coordinates, so the height is `bottom - top` — using
        // `min_rect().height()` at the end measures a *size* and reads as a different number
        // entirely (it took a screenshot to notice this class of mistake the first time).
        let top = ui.min_rect().top();
        let Some(plan) = self.plan.clone() else {
            // Unreachable from `detail` (it only draws a card once the preview is here), but kept as
            // the honest fallback: this is what the panel shows while a preview is missing.
            self.placeholder(ui);
            return ui.min_rect().bottom() - top;
        };
        // The card is **centred**: equal margins on both sides. `available - 2*margin` then a matching
        // left spacer is the whole trick; setting a smaller width alone leaves it hanging on the left.
        let card_w = (ui.available_width() - CARD_INSET * 2.0).max(140.0);
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_space(CARD_INSET);
            egui::Frame::NONE
                .fill(P::surface_hi())
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::symmetric(14, 13))
                .show(ui, |ui| {
                    ui.set_width((card_w - 28.0).max(80.0));
                    // ⚠️ The parent here is a `ui.horizontal` (it exists only to give the card a left
                    // margin), and `Frame::show` hands its layout down — so without this the title, the
                    // body and the buttons all lay out **side by side** and the right edge is clipped.
                    ui.vertical(|ui| {
                    ui.label(
                        RichText::new(aox::i18n::t("Restore to before this task"))
                            .size(13.0)
                            .strong()
                            .color(P::text()),
                    );
                    ui.add_space(9.0);
                    if plan.lines.is_empty() {
                        ui.label(
                            RichText::new(aox::i18n::t("No file changes: this point only marks a moment."))
                                .size(12.0)
                                .color(P::text_dim()),
                        );
                    } else {
                        ui.horizontal(|ui| {
                            count(ui, aox::i18n::t("modified"), plan.modified, P::text());
                            ui.add_space(12.0);
                            count(ui, aox::i18n::t("added"), plan.added, P::add_green());
                            ui.add_space(12.0);
                            count(ui, aox::i18n::t("deleted"), plan.removed, P::del_red());
                        });
                        ui.add_space(9.0);
                        for line in plan.lines.iter().take(6) {
                            let color = match line.kind {
                                'A' => P::add_green(),
                                'D' => P::del_red(),
                                _ => P::text_dim(),
                            };
                            ui.label(
                                RichText::new(format!("{}  {}", line.kind, elide(&line.path, 52)))
                                    .font(theme::mono_font(12.0))
                                    .color(color),
                            );
                        }
                        if plan.lines.len() > 6 {
                            ui.label(
                                RichText::new(aox::tf!("… and {} more", plan.lines.len() - 6))
                                    .size(11.0)
                                    .color(P::text_dim()),
                            );
                        }
                    }
                    if plan.partial || !plan.conflicts.is_empty() || plan.truncated {
                        ui.add_space(9.0);
                        if plan.partial {
                            warn(ui, aox::i18n::t("This point is incomplete: something the task wrote was not captured, so the rollback cannot guarantee it is undone."));
                        }
                        if !plan.conflicts.is_empty() {
                            warn(
                                ui,
                                &aox::tf!(
                                    "{} path(s) were changed outside AOX since then; rolling back overwrites them.",
                                    plan.conflicts.len()
                                ),
                            );
                        }
                        if plan.truncated {
                            warn(ui, aox::i18n::t("Older history is truncated; only the retained chain can be rolled back."));
                        }
                    }
                    ui.add_space(12.0);
                    // Buttons only answer once the block has finished unfolding: half a card with a
                    // live "Roll back" under the pointer is a way to commit to something you have
                    // not read yet.
                    ui.add_enabled_ui(self.detail_t > 0.98, |ui| {
                        ui.horizontal(|ui| {
                            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                                if danger_button(ui, aox::i18n::t("Roll back…")).clicked() {
                                    self.confirm = Some(Ask {
                                        // The **row's** id, not the resolved target: the bridge turns it
                                        // into "the state before this point" (`TargetSpec::Before`).
                                        // Sending the resolved id instead would resolve a second time
                                        // and undo one point too many.
                                        target: self
                                            .expanded
                                            .clone()
                                            .unwrap_or_else(|| row_id.to_string()),
                                        label: self
                                            .view
                                            .rows
                                            .iter()
                                            .find(|r| r.id == plan.requested)
                                            .map(|r| r.label.clone())
                                            .unwrap_or_default(),
                                        overwrite: !plan.conflicts.is_empty(),
                                        partial: plan.partial,
                                        agreed: false,
                                        foreign: !self
                                            .view
                                            .rows
                                            .iter()
                                            .find(|r| r.id == plan.requested)
                                            .map(|r| r.mine)
                                            .unwrap_or(true),
                                    });
                                }
                                ui.add_space(8.0);
                                if flat_button(ui, aox::i18n::t("Cancel")).clicked() {
                                    self.expanded = None;
                                    self.plan = None;
                                }
                            });
                        });
                    });
                    });
                });
        });
        ui.add_space(4.0);
        // The card is measured from the top of the preview slot, so an in-flight preview (a single
        // line) and a full one both animate from the right height.
        (ui.min_rect().bottom() - top + 8.0).max(0.0)
    }

    /// The "clear every point" question. Nothing is deleted until this returns the request.
    ///
    /// A second modal rather than an inline confirm: the rows behind it are the thing being deleted,
    /// and deleting them under a mis-click is not recoverable by a "cancel" that arrives 200 ms late.
    pub fn clear_confirm(&mut self, ctx: &egui::Context) -> Option<RecoveryReq> {
        if !self.clear_ask {
            return None;
        }
        let mut answer = None;
        let resp = egui::Modal::new(Id::new("aox-recovery-clear"))
            .backdrop_color(P::scrim())
            .frame(theme::raised(theme::card_radius()).inner_margin(Margin::symmetric(22, 18)))
            .show(ctx, |ui| {
                ui.set_width(430.0);
                ui.label(
                    RichText::new(aox::i18n::t(
                        "This clears every rollback point in this workspace. Are you sure?",
                    ))
                    .color(P::text())
                    .size(16.0)
                    .strong(),
                );
                ui.add_space(6.0);
                // The promise, and it is true: `gc::clear_all` moves the directories to the recycle
                // bin through `SHFileOperationW` instead of erasing them.
                ui.label(
                    RichText::new(aox::i18n::t(
                        "What it removes goes to the system recycle bin, so it can be restored from there.",
                    ))
                    .color(P::text_faint())
                    .size(12.5),
                );
                ui.add_space(18.0);
                if let Some(yes) =
                    dialog_footer(ui, |ui| danger_button(ui, aox::i18n::t("Clear")).clicked())
                {
                    answer = Some(yes);
                }
            });
        if resp.should_close() {
            answer.get_or_insert(false);
        }
        match answer {
            Some(true) => {
                self.clear_ask = false;
                self.busy = true;
                Some(RecoveryReq::Clear)
            }
            Some(false) => {
                self.clear_ask = false;
                None
            }
            None => None,
        }
    }

    /// The second click. Returns the rollback request when the user really means it.
    pub fn confirm(&mut self, ctx: &egui::Context) -> Option<RecoveryReq> {
        let mut ask = self.confirm.take()?;
        let needs_consent = ask.overwrite || ask.partial;
        let mut answer = None;
        let resp = egui::Modal::new(Id::new("aox-recovery-confirm"))
            .backdrop_color(P::scrim())
            .frame(theme::raised(theme::card_radius()).inner_margin(Margin::symmetric(22, 18)))
            .show(ctx, |ui| {
                ui.set_width(430.0);
                ui.label(
                    RichText::new(aox::i18n::t("How should this rollback be done?"))
                        .color(P::text())
                        .size(16.0)
                        .strong(),
                );
                ui.add_space(6.0);
                // ── these sentences are promises ──
                //
                // "A safety copy is kept" may only be said when it really is: the snapshot is
                // written before any file is touched, and if it cannot be written the restore is
                // refused. The sentence below is the honest version of that rule, and it changes
                // with the situation instead of being generic reassurance.
                let base = if ask.overwrite {
                    aox::tf!(
                        "{} path(s) were changed outside AOX. Rolling back overwrites those edits; a safety copy of the current state is kept first.",
                        ask.target.len().max(1)
                    )
                } else {
                    aox::i18n::t("The files will be restored to the state before this point. The current state is kept as a safety copy, so the rollback itself can be undone.").to_string()
                };
                // One clause, not a second paragraph: whose task it was matters at the moment of
                // committing, and nowhere earlier (the row already said it).
                let body = if ask.foreign {
                    format!("{} {}", aox::i18n::t("This point was recorded by another conversation."), base)
                } else {
                    base
                };
                ui.label(RichText::new(body).color(P::text_faint()).size(12.5));
                ui.add_space(10.0);
                theme::well(8).inner_margin(Margin::symmetric(10, 7)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(elide(&ask.label, 60))
                            .font(theme::mono_font(12.5))
                            .color(P::text_dim()),
                    );
                });
                if needs_consent {
                    ui.add_space(12.0);
                    let mut agreed = ask.agreed;
                    let label = if ask.overwrite && ask.partial {
                        aox::i18n::t("I accept overwriting those edits, and that this point is incomplete")
                    } else if ask.overwrite {
                        aox::i18n::t("I accept overwriting those external edits")
                    } else {
                        aox::i18n::t("I accept that this point is incomplete")
                    };
                    if ui.checkbox(&mut agreed, RichText::new(label).size(12.0).color(P::text_dim())).changed() {
                        ask.agreed = agreed;
                    }
                }
                ui.add_space(18.0);
                let verb = if needs_consent {
                    aox::i18n::t("Roll back anyway")
                } else {
                    aox::i18n::t("Roll back")
                };
                let ready = !needs_consent || ask.agreed;
                if let Some(yes) = dialog_footer(ui, |ui| {
                    ui.add_enabled_ui(ready, |ui| {
                        if needs_consent {
                            danger_button(ui, verb).clicked()
                        } else {
                            primary_button(ui, verb).clicked()
                        }
                    })
                    .inner
                }) {
                    answer = Some(yes);
                }
            });
        if resp.should_close() {
            answer.get_or_insert(false);
        }
        match answer {
            Some(true) => Some(RecoveryReq::Rollback {
                target: ask.target,
                force: ask.overwrite,
                allow_partial: ask.partial,
            }),
            Some(false) => None,
            // Still open: put it back.
            None => {
                self.confirm = Some(ask);
                None
            }
        }
    }
}

// ─────────────────────────── drawing helpers ───────────────────────────

/// The restore glyph: a ring with a gap on the right and a head at the gap's upper end.
///
/// **Fifth shape.** Four hand-drawn "undo arrows" were rejected from real screenshots — a "C" on its
/// side, a hook, a circle with a sail, and finally a clean line-head arrow that was *correct* and
/// still not wanted. The reference the user supplied is not an undo arrow at all: it is the
/// **circular rotate arrow** (a near-complete ring, gap at 3 o'clock, head at the gap's top pointing
/// clockwise into it). That is what is transcribed here, by measurement rather than by eye — the
/// reference was 21 px across with r = 10, its gap 21° wide, its head's barbs 0.62 r and 0.52 r, so
/// every number below is that image divided by its radius. The two deliberate departures are the gap
/// (opened to 26°, see below) and the barbs (a common 0.60 r instead of two slightly different ones).
///
/// Hand-drawn like every other icon in this program: a font's "⟲" changes shape and weight with
/// whichever font the build picked, and at 16 px a glyph from a fallback font reads as a smudge.
fn paint_undo(painter: &egui::Painter, center: Pos2, box_size: f32, color: Color32) {
    // Heavier than the reference's 0.08 r: on a dark card a 1 px ring at this size disappears.
    let stroke = Stroke::new((box_size * 0.093).clamp(1.2, 1.8), color);
    let g = ring_shape(center, box_size);
    painter.add(egui::Shape::line(g.arc, stroke));
    // A **line** head, not a filled one: every other glyph in this program (folder, gear, trash,
    // plus) is strokes, and a solid triangle here reads as a different icon set dropped into the row.
    painter.line_segment([g.tip, g.barb_up], stroke);
    painter.line_segment([g.tip, g.barb_left], stroke);
}

/// The glyph as named pieces, so its shape can be **asserted** rather than eyeballed.
struct RingShape {
    arc: Vec<Pos2>,
    /// Where the arc's head end sits, and the two barbs that make it an arrow.
    tip: Pos2,
    barb_up: Pos2,
    barb_left: Pos2,
}

/// Angles are **screen** angles in radians: 0 is 3 o'clock, and +π/2 is 6 o'clock, because the y axis
/// points down. Every constant below is the reference image's measurement over its radius r.
fn ring_shape(center: Pos2, box_size: f32) -> RingShape {
    let s = box_size;
    // The glyph is 2.1 r across, so this leaves the stroke just inside the box the chip allocated.
    let r = s * 0.42;
    // Gap from −26° (where the head attaches) round to 0°: 334° of visible ring. Drawn from 0°
    // increasing, so the polyline runs right → bottom → left → top → head.
    //
    // The reference's gap is 21°, but its glyph is 21 px and this chip draws at 16 px — at 21° the
    // head fuses into the ring and the whole thing reads as a closed "O" (checked by rendering both
    // sizes side by side). Five degrees of extra opening is the smallest change that keeps the notch.
    let head = -26.0f32.to_radians();
    let sweep = std::f32::consts::TAU + head;
    let steps = 48;
    let arc = (0..=steps)
        .map(|i| {
            let a = sweep * (i as f32 / steps as f32);
            Pos2::new(center.x + r * a.cos(), center.y + r * a.sin())
        })
        .collect();
    let tip = Pos2::new(center.x + r * head.cos(), center.y + r * head.sin());
    // Straight up and straight left, so the head points down-right into the gap — the reference's
    // barbs are axis-aligned, and a 90° head is what keeps them legible at 16 px.
    let barb = r * 0.60;
    RingShape {
        arc,
        tip,
        barb_up: Pos2::new(tip.x, tip.y - barb),
        barb_left: Pos2::new(tip.x - barb, tip.y),
    }
}

/// Which of the foot's two weights an action wears. Weight is in the **colour only** — the height,
/// the type size, the padding and the hover wash are shared — so four different weights still read
/// as one row. Same rule as [`crate::app::Weight`], at the panel's own scale.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Foot {
    /// A third-rate action ("Verify", "Prune old points", "Redo last rollback").
    Quiet,
    /// Destructive, not armed yet ("Clear history").
    Danger,
}

/// One action in the panel's foot: no chrome at rest, one fill step on hover, a hand cursor — the
/// same language as a row and a toolbar icon, at one height for all four.
///
/// Hand-drawn instead of [`crate::app::flat_button`] for one measured reason: a ghost button is
/// 13.5 pt with 12 pt of side padding, and four of those plus the gaps come to ~410 pt inside a
/// 420 pt panel — the row would silently wrap or clip. These are 12 pt with 8, which fits with room
/// to spare, and the different padding is invisible because the *height* is still [`FOOT_H`].
fn foot_action(ui: &mut Ui, text: &str, kind: Foot) -> egui::Response {
    foot_action_ex(ui, text, kind, true)
}

/// [`foot_action`], disabled rather than hidden: identical geometry, faint label, no click. Hiding
/// one of four would re-space the other three on the frames a redo appears and disappears.
fn foot_action_ex(ui: &mut Ui, text: &str, kind: Foot, enabled: bool) -> egui::Response {
    // Measure first, then allocate from that size, then paint the backdrop, then the text — the
    // order matters (see `app::widgets::paint_text_button`): painting the text first puts the hover
    // wash **over** it and the label vanishes the moment the pointer arrives.
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        theme::body_font(FOOT_SIZE),
        Color32::PLACEHOLDER,
    );
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(galley.size().x + FOOT_PAD_X * 2.0, FOOT_H),
        if enabled { Sense::click() } else { Sense::hover() },
    );
    let hov = enabled && pointer_if(ui, &resp);
    if hov {
        ui.painter().rect_filled(rect, CornerRadius::same(FOOT_RADIUS), P::hover_fill());
    }
    let (rest, lit) = match kind {
        Foot::Quiet => (P::text_dim(), P::text()),
        // Danger does not get louder on hover: a second shade of the same red would read as two
        // different amounts of danger for one action.
        Foot::Danger => (P::danger(), P::danger()),
    };
    let fg = if !enabled {
        P::quiet()
    } else if hov {
        lit
    } else {
        rest
    };
    // Centred in both axes, so all four share one baseline — the same reason the buttons have a
    // fixed height rather than growing from their padding.
    ui.painter().galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        fg,
    );
    resp
}

fn count(ui: &mut Ui, word: &str, n: usize, color: Color32) {
    ui.label(RichText::new(format!("{word} {n}")).size(12.0).color(color));
}

fn warn(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(format!("! {text}")).size(11.5).color(P::text_faint()));
}

fn skeleton(ui: &mut Ui) {
    for _ in 0..3 {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width() - ROW_INSET * 2.0, 34.0), Sense::hover());
        ui.painter().rect_filled(
            rect.translate(egui::vec2(ROW_INSET, 0.0)),
            CornerRadius::same(8),
            P::surface_hi(),
        );
        ui.add_space(6.0);
    }
    ui.add_space(4.0);
}

/// One dim line of secondary text, indented to the panel's gutter.
fn line(ui: &mut Ui, text: &str, err: bool) {
    ui.horizontal_wrapped(|ui| {
        ui.add_space(CARD_INSET + 2.0);
        ui.label(
            RichText::new(text)
                .size(11.0)
                .color(if err { P::danger() } else { P::text_dim() }),
        );
    });
    ui.add_space(6.0);
}

fn kind_word(kind: &str) -> &'static str {
    match kind {
        "external" => aox::i18n::t("external"),
        "manual" => aox::i18n::t("manual"),
        _ => aox::i18n::t("task"),
    }
}

fn status_word(status: &str) -> &'static str {
    match status {
        "partial" => aox::i18n::t("partial"),
        "unknown" => aox::i18n::t("unknown"),
        _ => aox::i18n::t("complete"),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How long ago, in words. Everything under a day is relative, everything older is a date —
/// a timestamp at 11px in a 420px panel is a number nobody converts in their head.
fn ago(at: u64, now: u64) -> String {
    let d = now.saturating_sub(at);
    if d < 60 {
        aox::i18n::t("just now").to_string()
    } else if d < 3600 {
        aox::tf!("{} min ago", d / 60)
    } else if d < 86400 {
        aox::tf!("{} h ago", d / 3600)
    } else {
        // The same conversion the rest of the program prints (UTC) — a second, subtly different
        // one would show a different date for the same point depending on which screen you looked at.
        let (_, m, day, ..) = aox::history::civil_from_unix(at);
        format!("{m:02}-{day:02}")
    }
}

/// Middle elision for paths: the tail is what identifies a file, the head is what identifies a tree.
fn elide(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max < 5 {
        return chars.into_iter().take(max).collect();
    }
    let keep = max - 1;
    let head = keep / 3;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_relative_time_never_needs_a_timezone() {
        let t = 1_700_000_000u64;
        assert_eq!(ago(t, t + 5), aox::i18n::t("just now"));
        assert_eq!(ago(t, t + 60), "1 min ago");
        assert_eq!(ago(t, t + 59 * 60), "59 min ago");
        assert_eq!(ago(t, t + 3600), "1 h ago");
        assert_eq!(ago(t, t + 23 * 3600), "23 h ago");
        // Past a day it is a date, and it is the **same** date the rest of the program prints
        // (`civil_from_unix`, UTC) rather than a second, subtly different conversion.
        let (_, m, d, ..) = aox::history::civil_from_unix(t);
        assert_eq!(ago(t, t + 30 * 3600), format!("{m:02}-{d:02}"));
    }

    /// The bug that shipped and was caught in a real screenshot: the answer was matched against the
    /// **resolved** node, which is one step older than the row the user clicked, so every preview was
    /// dropped and the row sat on "reading…" forever.
    #[test]
    fn a_preview_lands_only_on_the_row_that_asked_for_it() {
        let mut rec = RecoveryUi::new();
        rec.open = true;
        rec.expanded = Some("rp-a".into());
        rec.mark_busy();

        let mut other = PreviewView::default();
        other.requested = "rp-b".into();
        rec.set_view(RecoveryView::Preview(Box::new(other)));
        assert!(rec.plan.is_none(), "an answer for another row is not this row's answer");
        assert!(!rec.busy, "the request is over either way");

        let mut mine = PreviewView::default();
        mine.requested = "rp-a".into();
        rec.set_view(RecoveryView::Preview(Box::new(mine)));
        assert_eq!(rec.plan.as_ref().map(|p| p.requested.as_str()), Some("rp-a"));
    }

    /// Four hand-drawn arrows were rejected before the user supplied a reference image, so what is
    /// pinned here is the **reference's** geometry, not a preference: a ring, not a "C"; the gap on
    /// the right rather than at the top; the head at the gap's *upper* end; and barbs that point up
    /// and left, which is what makes the head read as pointing clockwise into the gap.
    #[test]
    fn the_restore_glyph_is_a_ring_with_the_gap_at_its_upper_right() {
        let c = Pos2::new(100.0, 100.0);
        let side = 16.0;
        let g = ring_shape(c, side);
        let r = side * 0.42;

        // 1. It is a ring: every point of the arc sits at one radius.
        for p in &g.arc {
            let d = (*p - c).length();
            assert!((d - r).abs() < 1e-3, "{p:?} is off the ring: r = {d}");
        }

        // 2. And the ring is nearly closed — 334°, so a 26° gap. A bigger gap is the "C" that was
        //    rejected twice; a smaller one fuses the head into the arc at this size.
        let angle = |p: &Pos2| (p.y - c.y).atan2(p.x - c.x);
        let swept = {
            let a = (angle(g.arc.last().unwrap()) - angle(g.arc.first().unwrap())).to_degrees();
            if a < 0.0 { a + 360.0 } else { a }
        };
        assert!((swept - 334.0).abs() < 2.0, "the arc sweeps {swept}°");

        // 3. The gap faces right: the arc's ends are at 0° and −26°, and nothing lies between them.
        let ends = [angle(g.arc.first().unwrap()), angle(g.arc.last().unwrap())];
        assert!(ends[0].to_degrees().abs() < 1.0, "one end at 3 o'clock: {}°", ends[0].to_degrees());
        assert!(
            (ends[1].to_degrees() + 26.0).abs() < 1.5,
            "the other at −26°: {}°",
            ends[1].to_degrees()
        );
        let mut in_gap = 0;
        for p in &g.arc {
            if (-25.0..=-1.0).contains(&angle(p).to_degrees()) {
                in_gap += 1;
            }
        }
        assert_eq!(in_gap, 0, "the gap is open, not bridged");
        assert!(g.arc.iter().any(|p| p.x > c.x + r * 0.8), "the ring reaches 3 o'clock");

        // 4. The head is at the gap's upper end, above the middle, and its barbs go up and left —
        //    the arrow points down-right, into the gap.
        assert!(g.tip.x > c.x + r * 0.8, "head on the right: {:?}", g.tip);
        assert!(g.tip.y < c.y - r * 0.2, "head above the middle: {:?}", g.tip);
        assert!((g.barb_up.x - g.tip.x).abs() < 0.01 && g.barb_up.y < g.tip.y - r * 0.4);
        assert!((g.barb_left.y - g.tip.y).abs() < 0.01 && g.barb_left.x < g.tip.x - r * 0.4);

        // 5. Nothing spills out of the box it was given: the head's upper barb is the furthest point,
        //    at 1.04 r, and the box's half-width is 0.5 side = 1.19 r.
        let reach = r * 1.04;
        assert!(reach < side * 0.5, "{reach} vs half a box");
        for p in g.arc.iter().chain([&g.tip, &g.barb_up, &g.barb_left]) {
            assert!(
                (p.x - c.x).abs() <= side * 0.5 && (p.y - c.y).abs() <= side * 0.5,
                "{p:?} outside the box"
            );
        }
    }

    /// The panel's own clock. Two properties are what the user actually asked for, and neither is
    /// visible in a still screenshot, so both are pinned here:
    ///
    /// ① leaving takes **several frames** (a panel that is taken away in one frame reads as sliced);
    /// ② the opacity **holds, then goes**. A naive ease-out on the way out loses 70% in the first
    ///    30 ms and then leaves a half-transparent ghost crawling for the rest of the animation,
    ///    which is exactly the "被切断" feeling this was reported as.
    #[test]
    fn the_panel_leaves_slowly_at_first_then_all_at_once() {
        let ctx = egui::Context::default();
        let mut rec = RecoveryUi::new();

        // Arriving: one clock, ease-out, lands exactly on 1.
        rec.open = true;
        rec.step_open(&ctx);
        assert!(rec.open_anim.is_some(), "opening is animated");
        rec.open_anim = Some((Instant::now() - Duration::from_millis(400), 0.0, 1.0));
        rec.step_open(&ctx);
        assert_eq!(rec.open_t, 1.0);
        assert!(rec.open_anim.is_none(), "a finished clock is dropped");

        // Leaving: still on screen one frame later, at full strength.
        rec.open = false;
        rec.step_open(&ctx);
        assert!(rec.open_anim.is_some(), "leaving is animated, not instantaneous");
        assert_eq!(rec.open_t, 1.0, "the first frame is still the whole panel");

        // A quarter of the way through, a smoothstep has only spent ~16% of the opacity; an ease-out
        // would already be down to 42%.
        rec.open_anim = Some((Instant::now() - Duration::from_millis(37), 1.0, 0.0));
        rec.step_open(&ctx);
        assert!(rec.open_t > 0.80, "holds before it goes: {}", rec.open_t);
        // …and it is more than half gone by the time it does.
        rec.open_anim = Some((Instant::now() - Duration::from_millis(112), 1.0, 0.0));
        rec.step_open(&ctx);
        assert!(rec.open_t < 0.30, "then leaves quickly: {}", rec.open_t);

        // Reached the end: exactly zero, so `panel` can stop drawing instead of leaving a 2% ghost.
        rec.open_anim = Some((Instant::now() - Duration::from_millis(400), 1.0, 0.0));
        rec.step_open(&ctx);
        assert_eq!(rec.open_t, 0.0);
        assert!(rec.open_anim.is_none());
    }

    /// What a dismissal may **not** do: clear the rows while the panel is still on screen. That is
    /// the reported bug — the preview is the panel's bottom half, so deleting it a frame early made
    /// the panel look cut in two. Only [`RecoveryUi::forget_transient`], called once the clock has
    /// reached zero, is allowed to throw the picture away.
    #[test]
    fn dismissing_keeps_the_preview_until_the_last_frame() {
        let mut rec = RecoveryUi::new();
        rec.expanded = Some("rp-a".into());
        rec.detail_id = Some("rp-a".into());
        rec.detail_t = 1.0;
        rec.plan = Some(PreviewView::default());

        rec.open = false;
        assert!(
            rec.expanded.is_some() && rec.detail_id.is_some() && rec.plan.is_some(),
            "the click only flips the target; the picture is still needed"
        );

        rec.forget_transient();
        assert!(rec.expanded.is_none(), "forgotten at the end");
        assert!(rec.plan.is_none());
        assert_eq!(rec.detail_t, 0.0);
    }

    #[test]
    fn elision_keeps_the_file_name_visible() {
        let long = "E:/rustapp/src/tool/shell/scanner/very/deep/inside/file.rs";
        let out = elide(long, 30);
        assert_eq!(out.chars().count(), 30);
        assert!(out.ends_with("file.rs"), "{out}");
        assert!(out.starts_with("E:/rust"), "{out}");
        // Short strings are untouched.
        assert_eq!(elide("a.rs", 30), "a.rs");
    }

    #[test]
    fn the_status_and_kind_words_are_stable_codes_not_free_text() {
        // Any unknown code reads as the safe word, never as an empty row.
        assert_eq!(status_word("complete"), aox::i18n::t("complete"));
        assert_eq!(status_word("partial"), aox::i18n::t("partial"));
        assert_eq!(status_word("something-new"), aox::i18n::t("complete"));
        assert_eq!(kind_word("task"), aox::i18n::t("task"));
        assert_eq!(kind_word("external"), aox::i18n::t("external"));
        assert_eq!(kind_word(""), aox::i18n::t("task"));
    }

    /// Longer than any of this module's appearance clocks, so `step_detail`/`step_open` always land
    /// on their target when a test jumps the timeline forward.
    const FAST_FORWARD: std::time::Duration =
        std::time::Duration::from_millis(1_000 + (DETAIL_ANIM_SECS.max(OPEN_SECS) * 1000.0) as u64);

    /// The unfold clock: opening reaches 1, closing reaches 0 and then forgets the row, and a
    /// reversal mid-flight carries on from where it is. Pinned because "the block vanished instead of
    /// folding" is the kind of regression nobody notices until it looks cheap.
    ///
    /// Also pinned here: **the clock waits for the preview**. Starting it on the click would make the
    /// card unfold from a guessed height and then jump when the real one arrived — at 60 fps that is
    /// the whole panel lurching, because the panel is pivoted at its bottom edge.
    #[test]
    fn the_unfold_clock_waits_for_the_preview_and_then_folds_both_ways() {
        let ctx = egui::Context::default();
        let mut rec = RecoveryUi::new();
        rec.expanded = Some("rp-a".into());
        rec.step_detail(&ctx);
        assert_eq!(rec.detail_id.as_deref(), Some("rp-a"), "the row owns the clock at once");
        assert!(rec.detail_anim.is_none(), "but it does not run yet");
        assert_eq!(rec.detail_t, 0.0);

        // The preview lands: now it may unfold, and it starts from zero.
        rec.plan = Some(PreviewView::default());
        rec.step_detail(&ctx);
        assert!(rec.detail_anim.is_some(), "the answer starts the clock");
        assert!(rec.detail_t < 0.01, "from the beginning:");
        // Jump the clock forward rather than sleeping. Derived from the constant so the test does not
        // have to be re-tuned every time the animation is slowed down for a visual check.
        rec.detail_anim = Some((Instant::now() - FAST_FORWARD, 0.0, 1.0));
        rec.step_detail(&ctx);
        assert_eq!(rec.detail_t, 1.0);

        // Collapsing keeps the preview: the fold-out has to draw the card it is folding.
        rec.expanded = None;
        rec.step_detail(&ctx);
        assert!(rec.detail_anim.is_some(), "closing is animated too");
        assert!(rec.plan.is_some(), "still drawable while it collapses");
        rec.detail_anim = Some((Instant::now() - FAST_FORWARD, 1.0, 0.0));
        rec.step_detail(&ctx);
        assert_eq!(rec.detail_t, 0.0);
        assert!(rec.detail_id.is_none(), "the row is forgotten only once it is fully folded");
        assert!(rec.plan.is_none(), "and so is the preview it was drawing");
    }
}
