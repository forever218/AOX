//! The window chrome: the title bar row and the buttons that live in it.
//! Not a `TopBottomPanel` -- there is no top bar.

use super::*;

/// The top-left button's hot zone (and the drawer head's, which shares the same drawing).
pub(super) const BAR_BTN: f32 = 32.0;

/// Height of the top row's strip — the band that drags the window and holds the plates.
pub(super) const TITLEBAR_H: f32 = 52.0;

/// Distance from the strip's edge to its first plate — the same 10 [`gwin::header`] leaves at the
/// far right, so the two windows are framed by the same margin.
pub(super) const TITLEBAR_INSET: f32 = 10.0;

/// The square plate a single button floats on: [`BAR_BTN`] plus one point of breathing on each
/// side, so the button's hover fill never touches the plate's hairline.
pub(super) const PLATE: f32 = 34.0;

/// Gap between two **separate** floating window buttons: wide enough that they read as three
/// buttons rather than as one segmented strip, narrow enough that they still belong together.
pub(super) const WBTN_GAP: f32 = 6.0;

/// Left edge of the status pill, measured from the strip's left edge.
pub(super) const STATUS_X: f32 = 52.0;

/// Height of the status pill — [`PLATE`] minus 6, so a readout is visibly lighter than a control.
pub(super) const STATUS_H: f32 = 28.0;

/// Breathing room inside the status pill.
pub(super) const STATUS_PAD: f32 = 12.0;

/// From the busy dot's centre to the status text's left edge — the same 9 [`gwin::header`] leaves
/// between its dot and its title.
pub(super) const STATUS_DOT_GAP: f32 = 9.0;

/// The sidebar toggle — one control in two places: the button in the window's title bar while the drawer is shut...
pub(super) fn sidebar_button(ui: &mut Ui, open: bool, floating: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(BAR_BTN), Sense::click());
    let hov = pointer_if(ui, &resp);
    let fg = if hov {
        P::ink()
    } else if open {
        P::text_dim()
    } else {
        P::text_faint()
    };

    let painter = ui.painter();
    if floating || hov {
        painter.rect_filled(
            rect,
            CornerRadius::same(theme::R1),
            if hov { P::surface_hi() } else { P::panel() },
        );
    }
    // The glyph: a panel with a divider down its left edge — "there is a column over there".
    let r = Rect::from_center_size(rect.center(), Vec2::new(14.0, 12.0));
    painter.rect_stroke(
        r,
        CornerRadius::same(theme::R_XS),
        Stroke::new(1.3, fg),
        egui::StrokeKind::Inside,
    );
    painter.line_segment(
        [Pos2::new(r.left() + 4.6, r.top()), Pos2::new(r.left() + 4.6, r.bottom())],
        Stroke::new(1.3, fg),
    );
    resp.on_hover_text(if open {
        aox::i18n::t("Hide history")
    } else {
        aox::i18n::t("Show history")
    })
}

/// The context gauge: a ring that fills as the conversation eats the model's context window.
pub(super) fn context_ring(ui: &mut Ui, used: u64, limit: u64, spin: Option<f32>) -> egui::Response {
    let d = 16.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(d), Sense::hover());
    let c = rect.center();
    let r = d * 0.5 - 1.3;
    let p = ui.painter();
    p.circle_stroke(c, r, Stroke::new(2.0, P::border()));
    if let Some(t) = spin {
        // One turn per second, a 90° tail: slow enough to read as "working", fast enough that a
        // one-second summary does not look frozen.
        let sweep = std::f32::consts::FRAC_PI_2;
        let a0 = -std::f32::consts::FRAC_PI_2 + t * std::f32::consts::TAU;
        let n = 10usize;
        let pts: Vec<Pos2> = (0..=n)
            .map(|i| {
                let a = a0 + sweep * (i as f32 / n as f32);
                Pos2::new(c.x + r * a.cos(), c.y + r * a.sin())
            })
            .collect();
        p.add(egui::Shape::line(pts, Stroke::new(2.0, P::text_dim())));
        return resp.on_hover_text(aox::i18n::t("Compacting the context…"));
    }
    let frac = if limit == 0 {
        0.0
    } else {
        (used as f32 / limit as f32).clamp(0.0, 1.0)
    };
    if frac > 0.0 {
        let col = if frac >= 0.92 {
            P::danger()
        } else if frac >= 0.75 {
            P::warn()
        } else {
            P::text_dim()
        };
        let n = ((frac * 40.0).ceil() as usize).max(2);
        let start = -std::f32::consts::FRAC_PI_2; // 12 o'clock, clockwise from there
        let pts: Vec<Pos2> = (0..=n)
            .map(|i| {
                let a = start + std::f32::consts::TAU * frac * (i as f32 / n as f32);
                Pos2::new(c.x + r * a.cos(), c.y + r * a.sin())
            })
            .collect();
        p.add(egui::Shape::line(pts, Stroke::new(2.0, col)));
    }
    let left = limit.saturating_sub(used);
    resp.on_hover_text(aox::tf!(
        "Context: {} of {} used, {} left",
        thousands(used),
        thousands(limit),
        thousands(left)
    ))
}

/// The settings button in the drawer's head: same 32px hot zone and same hover language as sidebar_button (they...
pub(super) fn gear_button(ui: &mut Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(BAR_BTN), Sense::click());
    let hov = pointer_if(ui, &resp);
    if hov {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), P::hover_fill());
    }
    let fg = if hov { P::ink() } else { P::text_dim() };
    draw_gear(ui.painter(), rect.center(), 7.8, 5.5, 1.3, fg);
    resp.on_hover_text(aox::i18n::t("Settings"))
}

/// The gear, drawn as a closed polyline of teeth plus a centre hole.
pub(super) fn draw_gear(
    painter: &egui::Painter,
    c: Pos2,
    r_out: f32,
    r_in: f32,
    width: f32,
    fg: Color32,
) {
    const TEETH: usize = 8;
    // The circle is divided into TEETH segments, and each one holds four points:
    // "tip -> valley -> valley -> tip".
    let step = std::f32::consts::TAU / TEETH as f32;
    let tooth = step * 0.22; // the tooth tip's share of the angle (the rest is the valley)
    let mut pts: Vec<Pos2> = Vec::with_capacity(TEETH * 4 + 1);
    for k in 0..TEETH {
        let a0 = step * k as f32;
        for (ang, r) in [
            (a0 - tooth, r_in),
            (a0, r_out),
            (a0 + tooth, r_out),
            (a0 + tooth * 2.0, r_in),
        ] {
            let (s, co) = ang.sin_cos();
            pts.push(Pos2::new(c.x + co * r, c.y + s * r));
        }
    }
    pts.push(pts[0]); // close the loop
    painter.add(egui::Shape::line(pts, Stroke::new(width, fg)));
    // The centre hole -- with it, it looks like a gear; without it, it's an eight-pointed star
    painter.circle_stroke(c, r_out * 0.26, Stroke::new(width, fg));
}

impl App {
    /// The window's top row: four controls that float on the content, not on a bar.
    pub(super) fn titlebar(&mut self, ctx: &egui::Context) {
        let mut toggle = false;
        let full = ctx.content_rect();
        // The row starts where the drawer ends — on the same eased clock the panel itself runs on, so it travels...
        let bar_x = HISTORY_W * ease_out_cubic(self.hist_t);
        let strip = Rect::from_min_max(
            Pos2::new(full.left() + bar_x, full.top()),
            Pos2::new(full.right(), full.top() + TITLEBAR_H),
        );
        // Mid-animation the column can be a few points wide; a row that cannot hold two plates is
        // not worth drawing (and would paint its plates off the right edge).
        if strip.width() < 4.0 * PLATE {
            return;
        }

        egui::Area::new(egui::Id::new("aox-titlebar"))
            .order(egui::Order::Foreground)
            .movable(false)
            .fixed_pos(strip.min)
            .show(ctx, |ui| {
                // Everything this ui *touches* must stay inside the strip, input included.
                ui.set_clip_rect(strip);
                // The whole strip is the window's drag handle. The plates are declared after it,
                // and later-declared widgets win the hit test, so they still answer the pointer.
                let (rect, resp) = ui.allocate_exact_size(strip.size(), Sense::click_and_drag());
                // `StartDrag` has to go out on the **very frame** of the press, or the window lags
                // behind the pointer for the whole drag.
                if resp.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                let maxed = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                // Double-clicking a title bar maximizes — the system did that for us until the
                // caption went away. (It is also why this strip senses clicks, not just drags.)
                if resp.double_clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maxed));
                }

                // left: the drawer toggle, only while the drawer is shut
                if self.hist_t <= 0.0 {
                    let plate = Rect::from_center_size(
                        Pos2::new(rect.left() + TITLEBAR_INSET + PLATE * 0.5, rect.center().y),
                        Vec2::splat(PLATE),
                    );
                    gwin::float_plate(ui.painter(), plate, gwin::FLOAT_R, false);
                    // A child ui rather than `ui.scope_builder`: the strip has already been allocated in full, and a scope would...
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(plate.shrink(1.0)));
                    // `floating = false`: the plate is the background now, and the button's own
                    // hover fill (a step above the plate) is the whole hover language here.
                    if sidebar_button(&mut child, false, false).clicked() {
                        toggle = true;
                    }
                }

                // the running status: a readout, not a control
                if self.busy {
                    // Working time, same basis as every other readout (see [`Clock`]) -- otherwise
                    // the number here would keep climbing while the model sits waiting on the user.
                    let secs = self
                        .live
                        .as_ref()
                        .map_or(0.0, |l| (self.now() - l.turn_base).max(0.0));
                    let galley = ui.painter().layout_no_wrap(
                        format!("{} · {:.1}s", self.status, secs),
                        FontId::proportional(13.0),
                        P::text_dim(),
                    );
                    let pill = Rect::from_min_size(
                        Pos2::new(rect.left() + STATUS_X, rect.center().y - STATUS_H * 0.5),
                        Vec2::new(STATUS_PAD * 2.0 + STATUS_DOT_GAP + galley.size().x, STATUS_H),
                    );
                    gwin::float_plate(ui.painter(), pill, 9, false);
                    let pulse = ((self.spin * 2.0) as i32) % 2 == 0;
                    let dot = Pos2::new(pill.left() + STATUS_PAD + 4.0, pill.center().y);
                    ui.painter().circle_filled(
                        dot,
                        4.0,
                        if pulse { P::ink() } else { P::ink_soft() },
                    );
                    ui.painter().galley(
                        Pos2::new(dot.x + STATUS_DOT_GAP, pill.center().y - galley.size().y * 0.5),
                        galley,
                        P::text_dim(),
                    );
                }

                // ── right: minimize / maximize / close, one plate each ──.
                let plate_at = |cx: f32| {
                    Rect::from_center_size(Pos2::new(cx, rect.center().y), Vec2::splat(PLATE))
                };
                let mut cx = rect.right() - TITLEBAR_INSET - PLATE * 0.5;
                let close = plate_at(cx);
                gwin::float_plate(ui.painter(), close, gwin::FLOAT_R, false);
                if gwin::win_button(ui, close.shrink(1.0), "close", gwin::Glyph::Close) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                cx -= PLATE + WBTN_GAP;
                let max = plate_at(cx);
                gwin::float_plate(ui.painter(), max, gwin::FLOAT_R, false);
                if gwin::win_button(ui, max.shrink(1.0), "max", gwin::Glyph::Max { on: maxed }) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maxed));
                }
                cx -= PLATE + WBTN_GAP;
                let min = plate_at(cx);
                gwin::float_plate(ui.painter(), min, gwin::FLOAT_R, false);
                if gwin::win_button(ui, min.shrink(1.0), "min", gwin::Glyph::Min) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }

                // Resize hot-zones: the top edge and the two upper corners live in this ui's clip rect.
                gwin::resize_handles(ui, ctx, full);
            });

        if toggle {
            self.show_history = !self.show_history;
            if self.show_history {
                self.refresh_history();
            }
            // Opening or putting it away is a choice about the next launch too (see the fold-state load...
            self.save_fold_state();
            // The drawer has already been drawn by the time this row runs (it is declared first), so the flip needs one more...
            ctx.request_repaint();
        }
    }
}

