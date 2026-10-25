//! The window chrome: the title bar row and the buttons that live in it.
//!
//! Not a `TopBottomPanel` -- there is no top bar (2026-10-04). The five icons are standalone floating
//! buttons whose positions are pinned by the constants here, and the geometry is the whole point: the
//! numbers reproduce the old bar's `Margin::symmetric(20, 0)` + 36 px hot zone pixel for pixel, so this
//! file is also the record of "where each button is and why".

use super::*;

/// The top-left button's hot zone (and the drawer head's, which shares the same drawing).
///
/// 32 rather than the 36 this used to be: the drawer's head icons are 32 so that the whole row
/// lines up with the "new conversation" row below it, and one number for both places is the point
/// of [`sidebar_button`] — the floating button and the in-panel one have to look like the same
/// control.
pub(super) const BAR_BTN: f32 = 32.0;

/// Height of the top row's strip — the band that drags the window and holds the plates.
///
/// It is **not** a reserved panel: nothing is cut out of the column, the background image simply
/// runs underneath. 52 keeps the row on the same line the old bar had, and the transcript's own
/// 76-point top padding (see [`App::transcript`]) is what keeps text out of it.
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
///
/// Fixed rather than "after whatever was drawn before": the pill is drawn whether or not the drawer
/// toggle occupies the slot before it, so opening the drawer can't make the readout jump.
pub(super) const STATUS_X: f32 = 52.0;

/// Height of the status pill — [`PLATE`] minus 6, so a readout is visibly lighter than a control.
pub(super) const STATUS_H: f32 = 28.0;

/// Breathing room inside the status pill.
pub(super) const STATUS_PAD: f32 = 12.0;

/// From the busy dot's centre to the status text's left edge — the same 9 [`gwin::header`] leaves
/// between its dot and its title.
pub(super) const STATUS_DOT_GAP: f32 = 9.0;

/// The sidebar toggle — **one control in two places**: the button in the window's title bar while
/// the drawer is shut, and the same button in the drawer's head while it is open.
///
/// Sharing the drawing is the point. The two used to be different glyphs (a rule with three lines
/// floating over the transcript, a panel outline inside the panel) at different sizes with
/// different resting backdrops, so "the thing that opens the sidebar" and "the thing that closes
/// it" didn't look related at all — the user's complaint. Now: same 32px hot zone, same panel
/// outline with the divider down its left edge, same 1.3px stroke, same hover language.
///
/// The only thing that still differs is the **resting backdrop**, and it has to: the floating copy
/// sits straight on the transcript text and is illegible without a plate under it, while the one
/// in the drawer's head already sits on the drawer's own surface, where a plate would be a second
/// box drawn on a box.
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
            CornerRadius::same(9),
            if hov { P::surface_hi() } else { P::panel() },
        );
    }
    // The glyph: a panel with a divider down its left edge — "there is a column over there".
    let r = Rect::from_center_size(rect.center(), Vec2::new(14.0, 12.0));
    painter.rect_stroke(
        r,
        CornerRadius::same(2),
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
///
/// `used / limit` is the whole reading, and it is deliberately tiny and unlabelled — it answers
/// "am I about to run out of room" at a glance, and the exact figures are one hover away. Colour is
/// the only alarm, and it borrows the three-band language the cache-hit map already taught this UI:
/// quiet ink below 75%, amber past it, brick past 92%.
///
/// Drawn as a hand-built polyline rather than a real arc: egui has no arc primitive, and at 16px a
/// 40-segment `PathShape` is pixel-identical to one. Zero arc → no fill at all, so a fresh
/// conversation shows the empty track and nothing else.
///
/// `spin` is `Some(seconds since the compaction started)` while the host is folding history into a
/// summary. In that state the ring **stops being a reading**: the numbers will only be known after
/// the next request, so filling it with the old (full) value would say "still full" at the exact
/// moment the problem is being solved. Instead a short arc travels around the track — the one
/// animation this row has, and the honest shape for "working, no measurement yet".
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

/// The settings button in the drawer's head: same 32px hot zone and same hover language as
/// [`sidebar_button`] (they sit side by side, and two different box sizes on one row is exactly the
/// kind of thing that reads as sloppy), **no backdrop at rest** — the drawer's own surface is the
/// backdrop.
pub(super) fn gear_button(ui: &mut Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(BAR_BTN), Sense::click());
    let hov = pointer_if(ui, &resp);
    if hov {
        ui.painter().rect_filled(rect, CornerRadius::same(9), P::hover_fill());
    }
    let fg = if hov { P::ink() } else { P::text_dim() };
    draw_gear(ui.painter(), rect.center(), 7.8, 5.5, 1.3, fg);
    resp.on_hover_text(aox::i18n::t("Settings"))
}

/// The gear, drawn as a closed polyline of teeth plus a centre hole.
///
/// One drawing for both sizes (the drawer head and the settings dialog's rows): two copies
/// would drift apart the first time either is touched, and the difference is only three numbers.
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
    /// The window's top row: four controls that **float on the content**, not on a bar.
    ///
    /// # Why the bar is ours
    ///
    /// Until 2026-10-18 the top of this window was two strips stacked on each other: the native
    /// caption (empty since 2026-10-04 — no title, only the system buttons, dyed to `P::bg()` so it
    /// read as background) and, inside the content area, a row of floating controls. Both were
    /// background-coloured, neither was quite "the window bar", and the independent windows
    /// (`activity`, `cache`, `wsview`) meanwhile drew a better shape: a bar that is *part of* the
    /// window rather than a strip above it.
    ///
    /// So the caption is gone (`with_decorations(false)` in `main.rs`) and the row is drawn here,
    /// to the same measurements as [`gwin::header`]: same 32x28 buttons, same hover language, same
    /// drag and double-click-to-maximize, same 1px outline ([`gwin::outline`]) and same DWM rounded
    /// corners (`theme::round_corners`). The one thing it does **not** carry is the pin: the main
    /// window is not something you keep on top of anything.
    ///
    /// # Why it is an `Area` and not a panel
    ///
    /// A `TopBottomPanel` reserves a full-width strip and paints it. On the plain background that is
    /// invisible; on the **first screen's background image** it cuts the picture off 52 points down,
    /// and the image then reads as "content under a bar" instead of as the window's own paper. An
    /// `Area` on the `Foreground` layer reserves nothing: the picture runs to the top edge and the
    /// four controls sit on small translucent plates ([`gwin::float_plate`]) that hold their own
    /// against any photograph.
    ///
    /// Nothing is covered by that: the transcript already starts 76 points down (see
    /// [`App::transcript`]), so the plates never sit on a line of text — which is also why the whole
    /// strip can stay the drag handle.
    ///
    /// # What is on it
    ///
    /// - Left: the drawer toggle, **only while the drawer is shut** — the drawer's head carries the
    ///   same control while it is open ([`sidebar_button`]: one drawing, two places).
    /// - Then a status pill (dot + text), **only while a turn runs**. It is a pill rather than bare
    ///   text because it now sits on a picture, and text alone would be unreadable on some of them.
    /// - Right: minimize / maximize / close, on one plate.
    ///
    /// No title **on screen**. The caption lost its "AOX" on 2026-10-04 — the user wanted the top
    /// edge to be the window's own edge, not a label — and putting the name back would reopen
    /// exactly that. The *system* title is "AOX" (see `main.rs`): the window is undecorated, so it
    /// is never painted, but it is what the taskbar and Alt-Tab call this window.
    ///
    /// "New conversation" and "settings" are still **not** here: both live inside the drawer, the
    /// only place they can be reached from once the drawer owns that corner.
    pub(super) fn titlebar(&mut self, ctx: &egui::Context) {
        let mut toggle = false;
        let full = ctx.content_rect();
        // The row starts where the drawer ends — on the same eased clock the panel itself runs on,
        // so it travels with the column instead of jumping when the animation lands. While the
        // drawer is shut the offset is zero and the row spans the window.
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
                // Everything this ui *touches* must stay inside the strip, input included. The
                // eight resize zones are computed from the window rect, and this layer wins the hit
                // test over the panels below it — unclipped, its two zones would claim the whole
                // left edge and the whole bottom edge as well.
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

                // ── left: the drawer toggle, only while the drawer is shut ──
                if self.hist_t <= 0.0 {
                    let plate = Rect::from_center_size(
                        Pos2::new(rect.left() + TITLEBAR_INSET + PLATE * 0.5, rect.center().y),
                        Vec2::splat(PLATE),
                    );
                    gwin::float_plate(ui.painter(), plate, gwin::FLOAT_R, false);
                    // A child ui rather than `ui.scope_builder`: the strip has already been
                    // allocated in full, and a scope would allocate its own button a second time —
                    // below the strip, outside the Area.
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(plate.shrink(1.0)));
                    // `floating = false`: the plate is the background now, and the button's own
                    // hover fill (a step above the plate) is the whole hover language here.
                    if sidebar_button(&mut child, false, false).clicked() {
                        toggle = true;
                    }
                }

                // ── the running status: a readout, not a control ──
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

                // ── right: minimize / maximize / close, one plate each ──
                //
                // Laid out right-to-left exactly as [`gwin::header`] does it, so the two windows
                // put their buttons in the same places. **Three plates, not one**: a single plate
                // holding all three reads as a toolbar of segmented buttons, which is a heavier
                // object than this row is -- each of these is one button, and at this size the
                // separate chips are also easier to hit.
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

                // Resize hot-zones: the top edge and the two upper corners live in this ui's clip
                // rect. The drawer and the central column call this too, with the same full-window
                // rect — each `ui.interact` is clipped to its own ui, so the three calls together
                // cover all eight zones.
                gwin::resize_handles(ui, ctx, full);
            });

        if toggle {
            self.show_history = !self.show_history;
            if self.show_history {
                self.refresh_history();
            }
            // Opening or putting it away is a **choice about the next launch too** (see the
            // fold-state load in `refresh_history`), so it goes to `ui.json` — the same file, and
            // the same write, the group folds use. One small JSON per click is nothing next to the
            // directory scan `refresh_history` just did.
            self.save_fold_state();
            //
            // The drawer has already been drawn by the time this row runs (it is declared first),
            // so the flip needs one more pass to show — and an idle window doesn't repaint on its
            // own. One request is enough: from the next pass on, the drawer's own animation keeps
            // the frames coming.
            ctx.request_repaint();
        }
    }
}

