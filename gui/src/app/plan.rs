//! The plan card: the progress reading above the input box, and its four heights.
//!
//! One feature in two halves -- `App`'s height arithmetic (which the composer reserves space with) and
//! the painting (one cell per step). They live together because the numbers have to agree: the reserve
//! and the drawing read the same `plan_t` in the same frame, and a comment below records what happened
//! the one time they were two independent floats.

use super::*;

/// What the plan card draws, cached when a checkpoint arrives.
///
/// Cached rather than read from `TaskState` every frame: the paint path runs at 60 Hz and a plan is
/// a few hundred bytes of strings — cloning that per frame to satisfy the borrow checker would be
/// paying for nothing. Building it once, in the event arm, also keeps every "what is the current
/// step" decision in one place.
pub(super) struct PlanView {
    pub(super) done: usize,
    pub(super) total: usize,
    /// The first open step — what the model says it is working on (see `TaskState::steps`).
    pub(super) current: Option<String>,
    pub(super) steps: Vec<(String, bool)>,
}

impl PlanView {
    /// `None` when the checkpoint has no plan: a card that always says "0/0" is noise.
    pub(super) fn of(st: &aox::task::TaskState) -> Option<Self> {
        if st.steps.is_empty() {
            return None;
        }
        let steps: Vec<(String, bool)> = st.steps.iter().map(|s| (s.text.clone(), s.done)).collect();
        Some(Self {
            done: st.steps.len() - st.pending_steps(),
            total: st.steps.len(),
            current: steps.iter().find(|(_, d)| !d).map(|(t, _)| t.clone()),
            steps,
        })
    }
}

/// Conversation `id`'s last task checkpoint, or `None`.
///
/// `None` for both cases that must look the same on screen: a brand-new conversation (no id yet) and
/// an existing one whose file has no `kind:"task"` row (written before checkpoints existed, or a
/// conversation that never needed one). A card is not drawn at all in either case — an empty "0/0"
/// card is noise.
///
/// A free function rather than a method so the "the plan belongs to the conversation" rule can be
/// tested against a real store without standing up a whole `App` (see
/// `the_plan_belongs_to_the_conversation_not_to_the_window`).
pub(super) fn plan_of(store: Option<&dyn aox::store::Store>, id: Option<&str>) -> Option<aox::task::TaskState> {
    let (store, id) = (store?, id?);
    store.load_task(id).and_then(|v| aox::task::TaskState::from_value(&v))
}

/// The counterpart of the CLI's `/demo`: preview the layout and the shape of the event stream
/// without connecting a real model.
/// How many rows fit in the card before the list starts **scrolling** inside it.
///
/// The card sits above the input box, so an unbounded list would walk the composer off a short
/// window — and a 30-step plan is legal (see `task::STEPS_MAX`). Scrolling lives inside the card
/// rather than in the "show the first 8 and a `+k`" version this replaces: a plan you cannot read
/// past row 8 is not a plan you can check your work against.
pub(super) const PLAN_ROWS: usize = 8;

/// Draw the plan card.
///
/// A free function rather than a method on purpose: the caller lends `&self.plan` and
/// `&mut self.plan_open` at the same time, and two disjoint fields are fine while a method call on
/// `&mut self` would be rejected.
///
/// Markers carry the state, colour carries the attention: `✓` done (dim), `▸` the step being worked
/// on (full text colour), `·` still to come (dim). Only one row is bright, which is the whole point
/// of a progress list — the rest is context.
pub(super) fn plan_card(ui: &mut Ui, view: &PlanView, open: &mut bool, rows_h: f32, gap: f32, full: bool) {
    let w = ui.available_width();
    ui.set_width(w);

    // ── Header: always visible, one line ──
    let header = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            // "3/5" first — it is the number the eye looks for, and on its own it already justifies
            // the space the collapsed block takes.
            ui.label(
                RichText::new(format!("{}/{}", view.done, view.total))
                    .color(P::text_dim())
                    .size(12.0),
            );
            match &view.current {
                Some(cur) => {
                    ui.add(Label::new(RichText::new(cur).color(P::text()).size(12.5)).truncate());
                }
                None => {
                    ui.label(
                        RichText::new(aox::i18n::t("every step is ticked"))
                            .color(P::text_dim())
                            .size(12.0),
                    );
                }
            }
            // Painted, never typed: `▸` / `▾` have no glyph in the bundled font, so both came out as
            // tofu boxes (2026-10-23). Three lines cannot go missing.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                chevron(ui, *open, P::text_dim());
            });
        })
        .response;
    let hit = ui.interact(header.rect, ui.id().with("aox-plan-toggle"), Sense::click());
    if hit.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if hit.clicked() {
        *open = !*open;
    }

    // ── The progress bar, then the rows sliding out from under it ──
    //
    // The bar's band is part of the card from the start, and the rows are clipped **below** it. An
    // earlier version put the rule at the bottom of the animated block, so while opening it travelled
    // down across the half-revealed row — a line through the text, read as "overlapping" (2026-10-23).
    let block_h = PLAN_DIV_H + rows_h;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, block_h), Sense::hover());
    plan_segments(ui, rect, w, view);

    // The rows start where the bar's band ends — one number (`PLAN_DIV_H`) for both, so the first row
    // cannot creep up under the bar.
    let rows_rect = Rect::from_min_max(
        Pos2::new(rect.left(), rect.top() + PLAN_DIV_H),
        Pos2::new(rect.right(), rect.bottom()),
    );
    if rows_rect.height() <= 0.5 {
        ui.add_space(gap);
        return;
    }
    let mut area = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rows_rect)
            .layout(Layout::top_down(Align::Min)),
    );
    area.set_clip_rect(rows_rect.intersect(ui.clip_rect()));
    if full {
        // A plan longer than the visible window scrolls **inside the card** (user request,
        // 2026-10-23): the alternative — showing the first eight and a "+k" — means the tail of the
        // plan is unreachable, and the tail is exactly where "what is left" lives.
        egui::ScrollArea::vertical()
            .id_salt("aox-plan-rows")
            .max_height(rows_rect.height())
            .auto_shrink([false, true])
            .show(&mut area, |ui| plan_rows_into(ui, view));
    } else {
        plan_rows_into(&mut area, view);
    }
    ui.add_space(gap);
}

/// The rows themselves.
///
/// Shared by the plain path and the scrolling path so the two can never drift apart — and each row
/// is allocated at **exactly** `PLAN_ROW_H`: the block's height is computed from that constant
/// before a single row is drawn (the composer's position depends on it), so a row that lays itself
/// out naturally would make the arithmetic a guess. It was a guess once: five rows reported a
/// height of three.
pub(super) fn plan_rows_into(ui: &mut Ui, view: &PlanView) {
    let w = ui.available_width();
    // **No inter-row spacing**: `PLAN_ROW_H` is the row's full pitch, and the block's height was
    // computed from it before anything was drawn. Leaving egui's default 4 px between rows added
    // 20 px to five rows, which pushed the first row out of the viewport and produced a scrollbar
    // for a plan that fits (2026-10-23).
    ui.spacing_mut().item_spacing.y = 0.0;
    for (text, done) in view.steps.iter() {
        let current = !*done && view.current.as_deref() == Some(text.as_str());
        // Done vs to come is told by the **mark**, not by the text colour: one row is always the
        // working one, and dimming the rest of them would bury the plan.
        let (color, size) = if current {
            (P::text(), 12.5)
        } else if *done {
            (P::text_dim(), 12.0)
        } else {
            (P::text_dim(), 12.0)
        };
        ui.allocate_ui_with_layout(
            Vec2::new(w, PLAN_ROW_H),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                step_mark(ui, *done);
                ui.add(Label::new(RichText::new(text).color(color).size(size)).truncate());
            },
        );
    }
}

/// One step's marker: a hairline box while it is still to come, a low-saturation green tick once it
/// is done.
///
/// Painted rather than typed, for the same reason as the chevron: the bundled fonts have no `✓`
/// glyph and it came out as an empty rectangle. Painting also lets the box and the tick share one
/// optical size — no two font glyphs ever do, and at 10 px that shows.
///
/// The green is `P::add_green()` (the diff's "lines added" colour), deliberately low-saturation: a
/// plan is a quiet list, and a saturated tick would out-shout the one row the user is looking at.
pub(super) fn step_mark(ui: &mut Ui, done: bool) -> Rect {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
    let c = rect.center();
    let painter = ui.painter();
    if done {
        // Two strokes, corner below-left of centre: that is what makes it read as a tick instead of
        // a "v". 1.6 px at this size matches the visual weight of the box outline.
        let s = Stroke::new(1.6, P::add_green());
        painter.line_segment([c + egui::vec2(-3.7, 0.2), c + egui::vec2(-1.2, 2.7)], s);
        painter.line_segment([c + egui::vec2(-1.2, 2.7), c + egui::vec2(3.9, -2.9)], s);
    } else {
        painter.rect_stroke(
            Rect::from_center_size(c, Vec2::splat(8.4)),
            CornerRadius::same(2),
            Stroke::new(1.0, P::text_dim()),
            egui::StrokeKind::Inside,
        );
    }
    rect
}

/// Where every cell of the plan's progress bar starts, and how wide it is.
///
/// One cell per step, and the cells **always span the full width**: `w` is divided between them, so
/// a 3-step plan and a 30-step plan both start at the left edge and end exactly at the right one
/// (user request, 2026-10-24 — "5 个节点和 10 个节点都占满一条线"). Nothing is padded to a fixed cell
/// width, because a plan of five would then stop a fifth of the way across and read as a broken bar.
///
/// The gap is what gives way on a long plan, and never below a quarter of a cell: cells thinner than
/// the space between them stop reading as a measure and start reading as a dotted line. That bound is
/// also what guarantees `cell >= 3 × gap` at every width (see the test).
pub(super) fn plan_segment_spans(w: f32, total: usize) -> Vec<(f32, f32)> {
    let n = total.max(1) as f32;
    let gap = if total > 1 { PLAN_SEG_GAP.min(w / (n * 4.0)) } else { 0.0 };
    let cell = ((w - gap * (n - 1.0)) / n).max(1.0);
    (0..total.max(1)).map(|i| (i as f32 * (cell + gap), cell)).collect()
}

/// The plan's progress, drawn as **one cell per step** instead of as a rule.
///
/// A hairline said "the header ends here". A row of cells says the same thing the number beside it
/// says — `5/10` is five green cells and five grey ones — in a form the eye takes in before it reads
/// the digits. Same 10 px band as the rule it replaces, so no layout number moved.
///
/// Colour is the only difference between done and to-come; there is no fill animation, because the
/// count changes only when a checkpoint arrives and a bar that animates on its own would be claiming
/// progress between them.
pub(super) fn plan_segments(ui: &Ui, rect: Rect, w: f32, view: &PlanView) {
    let painter = ui.painter();
    let radius = CornerRadius::same((PLAN_SEG_H * 0.5) as u8);
    let y = rect.top() + (PLAN_DIV_H - PLAN_SEG_H) * 0.5;
    for (i, (x, cell_w)) in plan_segment_spans(w, view.total).into_iter().enumerate() {
        let cell = Rect::from_min_size(Pos2::new(rect.left() + x, y), Vec2::new(cell_w, PLAN_SEG_H));
        let color = if i < view.done { P::add_green() } else { P::edge() };
        painter.rect_filled(cell, radius, color);
    }
}

/// A small disclosure triangle, painted instead of typed (see `plan_card`).
pub(super) fn chevron(ui: &mut Ui, open: bool, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
    let c = rect.center();
    let pts = if open {
        vec![c + egui::vec2(-3.5, -1.5), c + egui::vec2(3.5, -1.5), c + egui::vec2(0.0, 2.5)]
    } else {
        vec![c + egui::vec2(-1.5, -3.5), c + egui::vec2(-1.5, 3.5), c + egui::vec2(2.5, 0.0)]
    };
    ui.painter().add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// The plan block inside the input card: header row, one row per step, and the gap under the
/// divider.
///
/// These are **reserve** numbers, not drawing numbers: the block lays itself out naturally, and the
/// transcript reserves at the end of its content what these add up to. Deliberately a couple of
/// points generous — under-reserving is the reported bug (the last line of the conversation hides
/// behind the card), while over-reserving is a few blank pixels nobody notices.
pub(super) const PLAN_HEAD_H: f32 = 24.0;

pub(super) const PLAN_ROW_H: f32 = 22.0;

/// Breathing room under the last step row.
pub(super) const PLAN_GAP: f32 = 9.0;

/// The band holding the plan's **progress bar** (one cell per step, see `plan_segments`), plus its
/// margins. **Constant**, never animated: a bar that slid with the rows would cross the half-revealed
/// text on the way down (that was the "重叠" seen while opening, 2026-10-23).
pub(super) const PLAN_DIV_H: f32 = 10.0;

/// Height of one cell of the progress bar. Four is the thinnest pill that still reads as a **cell**
/// rather than as a thickened rule — the bar sits above the input box, where a heavier bar would
/// start competing with the text.
pub(super) const PLAN_SEG_H: f32 = 4.0;

/// Gap between two cells. Shrinks on a long plan (see `plan_segment_spans`); never grows past this.
pub(super) const PLAN_SEG_GAP: f32 = 2.0;

/// Disclosure clock. Short enough to read as "opened", long enough not to be a jump cut.
pub(super) const PLAN_ANIM_SECS: f32 = 0.2;

impl App {
    /// Height of the **rows** part of the plan block, interpolated continuously from `plan_t`.
    ///
    /// Continuous, not `rows × t` rounded to whole rows: stepping one 20 px row at a time is what
    /// made the disclosure feel like a stutter (reported 2026-10-23) — the card, its shadow and the
    /// space reserved in the transcript all jumped together, several times per click. A fractional
    /// height is clipped instead, so the last row slides into view.
    ///
    /// The reserve and the drawing read this same number, in the same frame: two independent floats
    /// is exactly how the transcript's last line ends up behind the card for a frame.
    pub(super) fn plan_body_height(&self) -> f32 {
        self.plan_rows_height() + self.plan_gap_height() + if self.plan.is_some() { PLAN_DIV_H } else { 0.0 }
    }

    /// Height of the **viewport** the rows scroll in.
    ///
    /// Exactly `rows × PLAN_ROW_H` — not a pixel more. A viewport that is 9 px taller than the rows
    /// it shows displays the top strip of the next row, which reads as a sliced line of text at the
    /// bottom of the list (2026-10-23). The breathing room beneath belongs to the card, not to the
    /// scrolled area, so it is counted separately (see `plan_gap_height`).
    pub(super) fn plan_rows_height(&self) -> f32 {
        let Some(v) = self.plan.as_ref() else { return 0.0 };
        let rows = v.steps.len().min(PLAN_ROWS) as f32;
        rows * PLAN_ROW_H * self.plan_t
    }

    /// Breathing room under the last row; animated with the rows, and **outside** the scroll area.
    pub(super) fn plan_gap_height(&self) -> f32 {
        if self.plan.is_none() || self.plan_t <= 0.0 {
            return 0.0;
        }
        PLAN_GAP * self.plan_t
    }

    /// The whole plan block's height for this frame.
    ///
    /// The header is there whenever a plan exists — only the rows animate. (Returning 0 at
    /// `plan_t == 0` was the first version's bug: the collapsed block reserved no space and the last
    /// line of the conversation sat underneath it.)
    pub(super) fn plan_block_height(&self) -> f32 {
        if self.plan.is_none() {
            return 0.0;
        }
        PLAN_HEAD_H + self.plan_body_height()
    }
    /// The task plan: a small **card** above the input, collapsed by default.
    ///
    /// Collapsed it answers the one question worth the space while work is running — "which step is
    /// it on, and how far in". One click opens the whole list, ticked and unticked, which is what a
    /// long task needs when the model's own account of progress is worth checking.
    ///
    /// (First version was a bare dim line that turned out to be unreadable against the page — the
    /// text was `text_dim` on no background at all. A card was the user's call, and it is the right
    /// one: a persistent, clickable reading is a panel, not a whisper.)
    pub(super) fn plan_bar(&mut self, ui: &mut Ui) {
        let rows_h = self.plan_rows_height();
        let gap = self.plan_gap_height();
        // Scrolling only exists once the disclosure has finished: a scroll area whose viewport is
        // still growing shows a scrollbar for a fifth of a second and then takes it away.
        let full = self.plan_anim.is_none() && self.plan_t >= 1.0;
        let Some(view) = self.plan.as_ref() else { return };
        plan_card(ui, view, &mut self.plan_open, rows_h, gap, full);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The progress bar is a **measure**, so its cells must fill the card exactly and stay readable.
    ///
    /// Three properties, at every step count a plan can have (`task::STEPS_MAX` is 30) and every card
    /// width: one cell per step; the run of cells ends exactly at the right edge (5 steps and 30 steps
    /// both span the full width — the reported ask); and a cell is never thinner than the gap that
    /// separates it, which is the point where a measure turns into a dotted line.
    #[test]
    fn the_progress_cells_fill_the_card_at_any_step_count() {
        assert_eq!(plan_segment_spans(400.0, 0).len(), 1, "a plan with no steps still draws one cell");
        for total in [1usize, 2, 3, 5, 10, 17, 30] {
            for w in [240.0f32, 400.0, 680.0, 900.0] {
                let spans = plan_segment_spans(w, total);
                assert_eq!(spans.len(), total, "{total} steps must draw {total} cells");
                let (x0, cell) = spans[0];
                assert_eq!(x0, 0.0, "the first cell starts at the left edge");
                let (last_x, last_w) = spans[total - 1];
                assert!(
                    (last_x + last_w - w).abs() < 0.01,
                    "{total} cells at {w} pt end at {:.2}, not at the right edge",
                    last_x + last_w
                );
                assert!(
                    spans.iter().all(|(_, s)| (s - cell).abs() < 1e-3),
                    "cells of one bar must be equal: a bar of unequal cells misreads as data"
                );
                if total > 1 {
                    let gap = spans[1].0 - spans[0].0 - cell;
                    assert!(gap > 0.0, "{total} cells at {w} pt touch each other — no visible division");
                    // `plan_segment_spans` derives the gap from the width precisely to keep this.
                    assert!(
                        cell >= 3.0 * gap - 0.01,
                        "{total} cells at {w} pt: a {cell:.2} pt cell with a {gap:.2} pt gap reads as \
                         a dotted line, not as a measure"
                    );
                }
            }
        }
    }

    /// The plan card belongs to the **conversation**, not to the window it is drawn in.
    ///
    /// Reported 2026-10-24: a plan written in conversation A stayed on screen after switching to B —
    /// and on a brand-new conversation it stayed until B happened to write a checkpoint of its own,
    /// which for a one-off question may be never. Nothing was wrong with the card; it simply was
    /// never re-read on a switch, so it showed whatever it had been told last.
    ///
    /// `adopt_conversation` now asks the same row the agent reads on resume (`kind:"task"`, newest
    /// wins), and this states the three answers that has to give: the plan of a conversation that has
    /// one, nothing for a conversation that has none, nothing for a conversation not created yet.
    #[test]
    fn the_plan_belongs_to_the_conversation_not_to_the_window() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("aox-gui-plan-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch app root");

        let store = aox::store::default_store(&dir, 0);
        // Two conversations, and only the first one gets a checkpoint — exactly the pair that used to
        // show the same card.
        let mut planned = store.create(None, None).expect("create a conversation");
        let demo = aox::task::TaskState::steps_demo();
        let steps = demo.steps.len();
        planned.append_task(&demo.to_value());
        let planned = planned.id().to_string();
        let blank = store.create(None, None).expect("create a second conversation").id().to_string();

        let restored = plan_of(Some(store.as_ref()), Some(&planned)).expect("its own checkpoint comes back");
        assert_eq!(restored.steps.len(), steps, "the card must show this conversation's own steps");
        assert_eq!(
            PlanView::of(&restored).map(|v| v.total),
            Some(steps),
            "and the view built from it is the one the bar draws"
        );

        assert!(
            plan_of(Some(store.as_ref()), Some(&blank)).is_none(),
            "a conversation with no checkpoint must draw no card — the previous conversation's plan \
             used to survive the switch and claim progress that was never its own"
        );
        assert!(plan_of(Some(store.as_ref()), None).is_none(), "a new conversation starts with no plan");
        assert!(
            plan_of(None, Some(&planned)).is_none(),
            "no store (demo mode) means no plan, never a panic"
        );

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
