//! The plan card: the progress reading above the input box, and its four heights.
//! One feature in two halves -- `App`'s height arithmetic (which the composer reserves space with)...

use super::*;

/// What the plan card draws, cached when a checkpoint arrives.
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
pub(super) fn plan_of(store: Option<&dyn aox::store::Store>, id: Option<&str>) -> Option<aox::task::TaskState> {
    let (store, id) = (store?, id?);
    store.load_task(id).and_then(|v| aox::task::TaskState::from_value(&v))
}

/// The counterpart of the CLI's `/demo`: preview the layout and the shape of the event stream without connecting...
pub(super) const PLAN_ROWS: usize = 8;

/// Draw the plan card.
/// A free function rather than a method on purpose: the caller lends `&self.plan` and `&mut self.plan_open`...
pub(super) fn plan_card(ui: &mut Ui, view: &PlanView, open: &mut bool, rows_h: f32, gap: f32, full: bool) {
    let w = ui.available_width();
    ui.set_width(w);

    // Header: always visible, one line
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

    // ── The progress bar, then the rows sliding out from under it ──.
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
        // A plan longer than the visible window scrolls inside the card: the alternative — showing the first eight...
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
/// Shared by the plain path and the scrolling path so the two can never drift apart — and each row is allocated...
pub(super) fn plan_rows_into(ui: &mut Ui, view: &PlanView) {
    let w = ui.available_width();
    // No inter-row spacing: `PLAN_ROW_H` is the row's full pitch, and the block's height was computed from it before...
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

/// One step's marker: a hairline box while it is still to come, a low-saturation green tick once it is done.
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
            CornerRadius::same(theme::R_XS),
            Stroke::new(1.0, P::text_dim()),
            egui::StrokeKind::Inside,
        );
    }
    rect
}

/// Progress cells: one per step spanning full width; gaps shrink on long plans but never below a quarter cell.
pub(super) fn plan_segment_spans(w: f32, total: usize) -> Vec<(f32, f32)> {
    let n = total.max(1) as f32;
    let gap = if total > 1 { PLAN_SEG_GAP.min(w / (n * 4.0)) } else { 0.0 };
    let cell = ((w - gap * (n - 1.0)) / n).max(1.0);
    (0..total.max(1)).map(|i| (i as f32 * (cell + gap), cell)).collect()
}

/// The plan's progress, drawn as one cell per step instead of as a rule.
pub(super) fn plan_segments(ui: &Ui, rect: Rect, w: f32, view: &PlanView) {
    let painter = ui.painter();
    let radius = CornerRadius::same(theme::pill(PLAN_SEG_H));
    let y = rect.top() + (PLAN_DIV_H - PLAN_SEG_H) * 0.5;
    let moving = view.done < view.total;
    if moving {
        // ~12 fps is enough for a 1.7 s sine and costs far less than a full-rate repaint.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(80));
    }
    let t = ui.input(|i| i.time);
    let pulse = 0.62 + 0.38 * ((t * std::f64::consts::TAU / 1.7).sin() * 0.5 + 0.5);
    for (i, (x, cell_w)) in plan_segment_spans(w, view.total).into_iter().enumerate() {
        let cell = Rect::from_min_size(Pos2::new(rect.left() + x, y), Vec2::new(cell_w, PLAN_SEG_H));
        let color = if i < view.done {
            P::add_green()
        } else if i == view.done && moving {
            // Alpha, not `gamma_multiply`: the latter would darken the green into a different hue
            // family, and this cell has to read as "the same green, not all the way there yet".
            let c = P::add_green();
            Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (255.0 * pulse) as u8)
        } else {
            P::edge()
        };
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

/// The plan block inside the input card: header row, one row per step, and the gap under the divider.
pub(super) const PLAN_HEAD_H: f32 = 24.0;

pub(super) const PLAN_ROW_H: f32 = 22.0;

/// Breathing room under the last step row.
pub(super) const PLAN_GAP: f32 = 9.0;

/// Constant, never-animated band for the progress bar; sliding with rows would cross half-revealed text on open.
pub(super) const PLAN_DIV_H: f32 = 10.0;

/// Height of one cell of the progress bar.
pub(super) const PLAN_SEG_H: f32 = 4.0;

/// Gap between two cells. Shrinks on a long plan (see `plan_segment_spans`); never grows past this.
pub(super) const PLAN_SEG_GAP: f32 = 2.0;

/// Disclosure clock. Short enough to read as "opened", long enough not to be a jump cut.
pub(super) const PLAN_ANIM_SECS: f32 = 0.2;

impl App {
    /// Height of the rows part of the plan block, interpolated continuously from `plan_t`.
    pub(super) fn plan_body_height(&self) -> f32 {
        self.plan_rows_height() + self.plan_gap_height() + if self.plan.is_some() { PLAN_DIV_H } else { 0.0 }
    }

    /// Height of the viewport the rows scroll in.
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
    pub(super) fn plan_block_height(&self) -> f32 {
        if self.plan.is_none() {
            return 0.0;
        }
        PLAN_HEAD_H + self.plan_body_height()
    }
    /// The task plan: a small card above the input, collapsed by default.
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

    /// The progress bar is a measure, so its cells must fill the card exactly and stay readable.
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

    /// The plan card belongs to the conversation, not to the window it is drawn in.
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
