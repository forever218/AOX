//! Small painting primitives shared by more than one screen.
//! Anything here takes a `&mut Ui` (or a `&Painter`) and nothing else: no `App`, no state, no side effects beyond...
//! The `pub(crate)` items (the menu row, the three text buttons, `column`, `MENU_W`) are the ones other modules...

use super::*;

/// A stroke-drawn trash can, sized to sit at the left of a toast line.
pub(super) fn trash_glyph(ui: &mut Ui, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    // Everything below is laid out on a 20×20 grid, then scaled to `size`.
    let s = size / 20.0;
    let o = rect.min.to_vec2();
    let p = |x: f32, y: f32| Pos2::new(o.x + x * s, o.y + y * s);
    let w = Stroke::new((1.5 * s).max(1.0), color);
    let painter = ui.painter();
    // Handle, then lid.
    painter.line_segment([p(8.0, 3.0), p(12.0, 3.0)], w);
    painter.line_segment([p(3.0, 6.0), p(17.0, 6.0)], w);
    // Body: two slightly tapered sides and the base
    painter.line_segment([p(5.2, 6.0), p(6.4, 17.0)], w);
    painter.line_segment([p(14.8, 6.0), p(13.6, 17.0)], w);
    painter.line_segment([p(6.4, 17.0), p(13.6, 17.0)], w);
    // Two ribs.
    painter.line_segment([p(8.9, 9.0), p(8.9, 14.4)], w);
    painter.line_segment([p(11.1, 9.0), p(11.1, 14.4)], w);
}

// demo data (--demo)

/// The transcript column's measure: returns (content width, left/right padding).
pub(crate) fn column(available: f32) -> (f32, f32) {
    let inner = (available - 96.0).min(880.0).max(280.0);
    let pad = ((available - inner) * 0.5).max(18.0);
    (inner, pad)
}

/// Width of a right-click menu's rows.
pub(crate) const MENU_W: f32 = 178.0;

/// Ease-out curve (cubic ease-out): quick off the mark, slow to settle.
pub(crate) fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

/// A single line of text, closed off by pixels: anything past `max_w` is elided.
pub(crate) fn elide(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat { font_id: font, color, ..Default::default() },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: max_w,
        max_rows: 1,
        // Chinese has no word boundaries; without this, "the whole run doesn't fit" results in no
        // truncation at all
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

/// One row of a right-click menu.
pub(crate) fn menu_item(ui: &mut Ui, label: &str, w: f32, enabled: bool, strong: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 30.0), Sense::click());
    let hov = pointer_if(ui, &resp) && enabled;
    if hov {
        // Hover raises the surface one step -- the same language as list rows and the top-bar icons.
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), P::surface_hi());
    }
    let fg = if !enabled {
        P::quiet()
    } else if strong {
        P::text()
    } else if hov {
        P::text()
    } else {
        P::text_dim()
    };
    let galley =
        ui.painter().layout_no_wrap(label.to_owned(), theme::body_font(13.5), Color32::PLACEHOLDER);
    ui.painter().galley(
        Pos2::new(rect.left() + 10.0, rect.center().y - galley.size().y * 0.5),
        galley,
        fg,
    );
    resp
}

/// That S-shaped flow line meaning "information flows from here to there".
pub(super) fn draw_flow_curve(painter: &egui::Painter, rect: Rect, fg: Color32) {
    // One control point above and one below gives a smooth S
    let a = Pos2::new(rect.left(), rect.bottom());
    let b = Pos2::new(rect.left(), rect.top() + rect.height() * 0.12);
    let c = Pos2::new(rect.right(), rect.bottom() - rect.height() * 0.12);
    let e = Pos2::new(rect.right(), rect.top());

    let at = |t: f32| {
        let u = 1.0 - t;
        Pos2::new(
            u * u * u * a.x + 3.0 * u * u * t * b.x + 3.0 * u * t * t * c.x + t * t * t * e.x,
            u * u * u * a.y + 3.0 * u * u * t * b.y + 3.0 * u * t * t * c.y + t * t * t * e.y,
        )
    };

    const N: usize = 24;
    let pts: Vec<Pos2> = (0..=N).map(|i| at(i as f32 / N as f32)).collect();
    painter.add(egui::Shape::line(pts.clone(), Stroke::new(1.8, fg)));

    // start node
    painter.circle_filled(a, 2.1, fg);

    // end arrow: along the tangent of the curve's last segment
    let dir = {
        let v = e - pts[N - 3];
        let l = v.length();
        if l > 0.001 {
            v / l
        } else {
            Vec2::new(0.0, -1.0)
        }
    };
    let n = Vec2::new(-dir.y, dir.x);
    painter.add(egui::Shape::convex_polygon(
        vec![e + dir * 5.2, e - dir * 2.2 + n * 4.0, e - dir * 2.2 - n * 4.0],
        fg,
        Stroke::NONE,
    ));
}

/// Switch to the "hand" cursor while hovering a clickable area, and return whether it is hovered.
pub(crate) fn pointer_if(ui: &Ui, resp: &egui::Response) -> bool {
    let hov = resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    hov
}

/// The skin for text buttons: sizing plus a colour pair for resting / hover.
#[derive(Clone, Copy)]
pub(super) struct Btn {
    pub(super) size: f32,
    /// Horizontal padding (the vertical one is implied by `h`: the label is centred).
    pub(super) pad: Vec2,
    /// Fixed height of the hot zone.
    pub(super) h: f32,
    /// Lower bound on the width, so one-word buttons don't turn into tiny pills.
    pub(super) min_w: f32,
    pub(super) radius: u8,
    pub(super) stroke: Stroke,
    pub(super) fg: Color32,
    pub(super) fg_hover: Color32,
    pub(super) fill: Color32,
    pub(super) fill_hover: Color32,
    /// `false` = disabled: no hand cursor, no hover, no click. The **shape** stays identical, so a
    /// disabled action does not move the ones next to it (which is what hiding it would do).
    pub(super) interactive: bool,
}

/// The one control metric: every button in the window is this tall, on this radius, with the same type size.
pub(crate) const BTN_H: f32 = 34.0;
pub(crate) const BTN_RADIUS: u8 = 8;

/// What a button **means**. Weight is communicated by fill and colour only — never by size — so a
/// row of four different weights still reads as one row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Weight {
    /// The one affirmative action of a surface.
    Primary,
    /// A real action that is not *the* action ("Test", "Scan models", "Use").
    Secondary,
    /// No chrome at rest; for the third-rate actions ("Cancel", "Done", "Manage models…").
    Ghost,
    /// Destructive, not armed yet. Outlined, so it does not compete with Primary.
    Danger,
    /// Destructive and confirmed — the second press of a two-step delete.
    DangerSolid,
}

fn wash(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

fn btn_style(weight: Weight) -> Btn {
    let base = Btn {
        size: 13.5,
        pad: Vec2::new(16.0, 0.0),
        h: BTN_H,
        min_w: 0.0,
        radius: BTN_RADIUS,
        stroke: Stroke::NONE,
        fg: P::text(),
        fg_hover: P::text(),
        fill: Color32::TRANSPARENT,
        fill_hover: P::surface_hi(),
        interactive: true,
    };
    match weight {
        Weight::Primary => Btn {
            pad: Vec2::new(20.0, 0.0),
            fg: P::on_reverse(),
            fg_hover: P::on_reverse(),
            fill: P::reverse_bg(),
            fill_hover: P::ink_soft(),
            ..base
        },
        Weight::Secondary => Btn {
            stroke: Stroke::new(1.0, P::edge()),
            fill_hover: P::surface_hi(),
            ..base
        },
        Weight::Ghost => Btn { pad: Vec2::new(12.0, 0.0), fg: P::text_dim(), fg_hover: P::text(), ..base },
        Weight::Danger => Btn {
            stroke: Stroke::new(1.0, wash(P::danger(), 110)),
            fg: P::danger(),
            fg_hover: P::danger(),
            fill: Color32::TRANSPARENT,
            fill_hover: wash(P::danger(), 30),
            ..base
        },
        Weight::DangerSolid => Btn {
            pad: Vec2::new(20.0, 0.0),
            fg: P::on_reverse(),
            fg_hover: P::on_reverse(),
            fill: P::danger(),
            fill_hover: P::danger().gamma_multiply(0.82),
            ..base
        },
    }
}

/// A text button of the given weight, sized from its label.
pub(crate) fn button(ui: &mut Ui, text: &str, weight: Weight) -> egui::Response {
    text_button(ui, text, btn_style(weight))
}

/// The same button, but **disabled** rather than hidden: identical geometry, faint label, no click.
pub(crate) fn button_ex(ui: &mut Ui, text: &str, weight: Weight, enabled: bool) -> egui::Response {
    if enabled {
        return button(ui, text, weight);
    }
    let mut b = btn_style(weight);
    b.fg = P::text_faint();
    b.fg_hover = P::text_faint();
    b.fill = Color32::TRANSPARENT;
    b.fill_hover = Color32::TRANSPARENT;
    b.stroke = Stroke::new(1.0, P::edge());
    b.interactive = false;
    text_button(ui, text, b)
}

/// The same button at a **fixed** width — for a column of two ("Test" / "Scan models") where the
/// labels differ and the right edges still have to line up.
pub(crate) fn button_sized(ui: &mut Ui, text: &str, weight: Weight, w: f32) -> egui::Response {
    text_button(ui, text, Btn { min_w: w, ..btn_style(weight) })
}

/// A hand-drawn text button.
/// The key is the order of drawing: measure the text first -> allocate a hot zone from that size -> lay down...
pub(crate) fn dialog_footer(ui: &mut Ui, confirm: impl FnOnce(&mut Ui) -> bool) -> Option<bool> {
    let mut answer = None;
    ui.horizontal(|ui| {
        if flat_button(ui, aox::i18n::t("Cancel")).clicked() {
            answer = Some(false);
        }
        // `right_to_left` takes the rest of the row, so the first thing added inside it lands flush with the right edge...
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if confirm(ui) {
                answer = Some(true);
            }
        });
    });
    answer
}

pub(super) fn text_button(ui: &mut Ui, text: &str, b: Btn) -> egui::Response {
    let size = btn_extent(ui, text, b);
    let sense = if b.interactive { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let hov = if b.interactive { pointer_if(ui, &resp) } else { resp.hovered() };
    paint_text_button(ui, rect, text, b, hov);
    resp
}

/// How big a text button with this label is, before anything is laid out.
fn btn_extent(ui: &Ui, text: &str, b: Btn) -> Vec2 {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::proportional(b.size),
        Color32::PLACEHOLDER,
    );
    Vec2::new((galley.size().x + b.pad.x * 2.0).max(b.min_w), b.h)
}

/// Paint a text button into a rect the caller owns.
fn paint_text_button(ui: &Ui, rect: Rect, text: &str, b: Btn, hov: bool) {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::proportional(b.size),
        Color32::PLACEHOLDER,
    );
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(b.radius), if hov { b.fill_hover } else { b.fill });
    if b.stroke != Stroke::NONE {
        painter.rect_stroke(rect, CornerRadius::same(b.radius), b.stroke, egui::StrokeKind::Inside);
    }
    // The layout is done with PLACEHOLDER and the real colours are applied here -- so a hover recolour doesn't have...
    painter.galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        if hov { b.fg_hover } else { b.fg },
    );
}

/// The size a [`button`] with this label will take — for callers that have to reserve space first.
pub(crate) fn button_size(ui: &Ui, text: &str, weight: Weight) -> Vec2 {
    btn_extent(ui, text, btn_style(weight))
}

/// Paint a [`button`] into a rect the caller owns (see [`paint_text_button`]).
pub(crate) fn paint_button(ui: &Ui, rect: Rect, text: &str, weight: Weight, hover: bool) {
    paint_text_button(ui, rect, text, btn_style(weight), hover);
}

/// A ghost button: no chrome at rest, one fill step on hover ("Cancel", "Done", "Manage models…").
pub(crate) fn flat_button(ui: &mut Ui, text: &str) -> egui::Response {
    text_button(ui, text, btn_style(Weight::Ghost))
}

/// The primary action: a reversed block (ink-black with white text in the light theme), which
/// carries more weight than an outline — and, unlike before, no more *height* than an outline.
pub(crate) fn primary_button(ui: &mut Ui, text: &str) -> egui::Response {
    text_button(ui, text, btn_style(Weight::Primary))
}

/// The dangerous-action button (the "full access" confirmation, a confirmed delete).
pub(crate) fn danger_button(ui: &mut Ui, text: &str) -> egui::Response {
    text_button(ui, text, btn_style(Weight::DangerSolid))
}

/// Primary button with a leading plus drawn as two strokes, not a font glyph that varies in weight.
pub(crate) fn primary_plus_button(ui: &mut Ui, text: &str) -> egui::Response {
    let b = btn_style(Weight::Primary);
    let galley =
        ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(b.size), Color32::PLACEHOLDER);
    let arm = 4.5;
    let gap = 9.0;
    let w = galley.size().x + arm * 2.0 + gap + b.pad.x * 2.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, b.h), Sense::click());
    let hov = pointer_if(ui, &resp);
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(b.radius), if hov { b.fill_hover } else { b.fill });
    let cx = rect.left() + b.pad.x + arm;
    let cy = rect.center().y;
    let stroke = Stroke::new(1.5, b.fg);
    painter.line_segment([Pos2::new(cx - arm, cy), Pos2::new(cx + arm, cy)], stroke);
    painter.line_segment([Pos2::new(cx, cy - arm), Pos2::new(cx, cy + arm)], stroke);
    painter.galley(
        Pos2::new(
            cx + arm + gap,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        if hov { b.fg_hover } else { b.fg },
    );
    resp
}

/// The × on the right of a chip.
pub(super) fn close_x(ui: &mut Ui) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(17.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(aox::i18n::t("Remove"));
    let col = if resp.hovered() { P::text() } else { P::text_faint() };
    let c = rect.center();
    let s = 3.4;
    let stroke = Stroke::new(1.4, col);
    ui.painter().line_segment([c + Vec2::new(-s, -s), c + Vec2::new(s, s)], stroke);
    ui.painter().line_segment([c + Vec2::new(s, -s), c + Vec2::new(-s, s)], stroke);
    resp.clicked()
}

/// When too long, elide in the middle: keep both ends, cut the middle out.
pub(crate) fn middle_elide(s: &str, keep: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    // Too short, no need to elide; and when `keep` is too small to fit "head + ellipsis + tail",
    // don't force it either (it renders as a smear)
    if chars.len() <= keep || keep < 9 {
        return s.to_string();
    }
    let head = keep / 2;
    let tail = keep - head - 1; // the subtracted 1 is the ellipsis's own slot
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
}

pub(super) fn icon_button(
    ui: &mut Ui,
    diameter: f32,
    base: Color32,
    hover: Color32,
    enabled: bool,
    draw: impl Fn(&egui::Painter, egui::Rect, Color32),
) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(diameter), sense);
    let hov = enabled && pointer_if(ui, &resp);
    let fill = if !enabled {
        P::surface()
    } else if hov {
        hover
    } else {
        base
    };
    let painter = ui.painter();
    if enabled {
        painter.circle_filled(rect.center(), diameter * 0.5, fill);
    } else {
        painter.circle_stroke(rect.center(), diameter * 0.5, Stroke::new(1.0, P::border()));
    }
    // Inside a solid-fill button the glyph has to use the page colour to be visible (in a
    // monochrome scheme `fill` is the "ink")
    let fg = if !enabled { P::text_faint() } else { P::bg() };
    draw(painter, rect, fg);
    resp
}

pub(super) fn paint_arrow_up(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let c = rect.center();
    let s = rect.width() * 0.30;
    painter.line_segment(
        [Pos2::new(c.x, c.y - s * 0.9), Pos2::new(c.x, c.y + s * 0.75)],
        Stroke::new(1.6, color),
    );
    painter.line_segment(
        [Pos2::new(c.x - s * 0.62, c.y - s * 0.22), Pos2::new(c.x, c.y - s * 0.9)],
        Stroke::new(1.6, color),
    );
    painter.line_segment(
        [Pos2::new(c.x + s * 0.62, c.y - s * 0.22), Pos2::new(c.x, c.y - s * 0.9)],
        Stroke::new(1.6, color),
    );
}

pub(super) fn paint_square(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let c = rect.center();
    let s = rect.width() * 0.20;
    painter.rect_filled(
        egui::Rect::from_center_size(c, Vec2::splat(s * 1.5)),
        CornerRadius::same(theme::R_XS),
        color,
    );
}

pub(super) fn send_button(ui: &mut Ui, enabled: bool) -> egui::Response {
    icon_button(ui, 38.0, P::ink(), P::ink().gamma_multiply(0.75), enabled, paint_arrow_up)
}

pub(super) fn stop_button(ui: &mut Ui) -> egui::Response {
    icon_button(ui, 38.0, P::ink_soft(), P::ink_soft().gamma_multiply(0.8), true, paint_square)
}

pub(super) fn paint_tri(painter: &egui::Painter, center: Pos2, size: f32, down: bool, color: Color32) {
    let pts = if down {
        vec![
            Pos2::new(center.x - size, center.y - size * 0.5),
            Pos2::new(center.x + size, center.y - size * 0.5),
            Pos2::new(center.x, center.y + size * 0.6),
        ]
    } else {
        vec![
            Pos2::new(center.x - size * 0.5, center.y - size),
            Pos2::new(center.x - size * 0.5, center.y + size),
            Pos2::new(center.x + size * 0.6, center.y),
        ]
    };
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// A duration as a clock: `0:47`, `2:00`.
pub(super) fn clock(secs: f32) -> String {
    let s = secs.max(0.0) as u32;
    format!("{}:{:02}", s / 60, s % 60)
}

/// `elapsed / cap` — the one readout that answers "is this still fine".
pub(super) fn span(elapsed: f32, cap: f32) -> String {
    format!("{} / {}", clock(elapsed), clock(cap))
}

pub(super) fn spinner_dot(ui: &mut Ui, spin: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(16.0, 16.0), Sense::hover());
    let c = rect.center();
    let painter = ui.painter();
    for i in 0..8 {
        let phase = (spin * 7.0) as i32 + i;
        let alpha = if phase % 8 == 0 { 1.0 } else { 0.22 };
        let ang = std::f32::consts::TAU * (i as f32) / 8.0;
        let p = Pos2::new(c.x + ang.cos() * 5.4, c.y + ang.sin() * 5.4);
        painter.circle_filled(p, 1.7, color.gamma_multiply(alpha));
    }
}

/// How wide a sent attachment chip is (estimated).
pub(super) fn chip_width(ui: &Ui, name: &str) -> f32 {
    const PAD_X: f32 = 11.0;
    const STROKE: f32 = 1.0;
    let g = ui.painter().layout_no_wrap(
        middle_elide(name, 30),
        FontId::proportional(13.0),
        Color32::PLACEHOLDER,
    );
    g.size().x + PAD_X * 2.0 + STROKE * 2.0
}

/// The attachment chip inside an already-sent message bubble -- read-only (no ×).
pub(super) fn sent_chip(ui: &mut Ui, name: &str) {
    Frame::NONE
        .fill(P::surface())
        .corner_radius(CornerRadius::same(theme::pill(24.0)))
        .stroke(Stroke::new(1.0, P::edge()))
        .inner_margin(Margin::symmetric(11, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(middle_elide(name, 30)).color(P::text_dim()).size(13.0));
        });
}

pub(super) fn clamp_str(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut n = 0usize;
    for ch in s.chars() {
        if n >= max {
            out.push('…');
            break;
        }
        out.push(ch);
        n += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The readout that makes a long call legible: elapsed over ceiling, plus the model's estimate.
    #[test]
    fn the_running_call_reads_elapsed_over_ceiling() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(59.9), "0:59");
        assert_eq!(clock(120.0), "2:00");
        assert_eq!(clock(-3.0), "0:00", "a clock cannot run backwards");
        assert_eq!(span(47.2, 120.0), "0:47 / 2:00");
        assert_eq!(span(5.0, 600.0), "0:05 / 10:00");
    }

    /// Why `step_row` re-registers the file name after the row's own click, instead of just adding a clickable label...
    #[test]
    fn a_click_goes_to_the_widget_registered_last_on_the_same_rect() {
        use egui::{Event, PointerButton, Sense};

        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_size(egui::pos2(20.0, 20.0), egui::vec2(240.0, 22.0));
        let (row, name) = (egui::Id::new("aox-test-row"), egui::Id::new("aox-test-name"));
        let mut clicks = (false, false);

        // The hit test for clicks uses **last frame's** widget rects, so this needs a couple of idle
        // frames before the press/release, or nothing is registered yet and neither widget is hit.
        let mut frame = |events: Vec<Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 200.0))),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::Area::new(egui::Id::new("aox-test-area")).show(ctx, |ui| {
                    let row_resp = ui.interact(rect, row, Sense::click());
                    // ...and this is the line `step_row_inner` adds, in the same position.
                    let name_resp = ui.interact(rect, name, Sense::click());
                    clicks = (row_resp.clicked(), name_resp.clicked());
                });
            });
        };

        frame(vec![]);
        frame(vec![Event::PointerMoved(rect.center())]);
        frame(vec![Event::PointerButton {
            pos: rect.center(),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        }]);
        frame(vec![Event::PointerButton {
            pos: rect.center(),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        }]);

        assert_eq!(
            clicks,
            (false, true),
            "the widget registered last must own the click; if this flips, `step_row_inner` has to \
             register the file name before the row (and then the row would swallow it instead)"
        );
    }

    /// Nail down, as an executable statement, why the settings dialog has to pin its content width.
    #[test]
    fn egui_animates_the_scroll_bar_allocation() {
        let ctx = egui::Context::default();
        // Consistent with this project's `theme::install_style`: floating style + reserved width
        // (thin() gives 6px)
        ctx.style_mut(|s| s.spacing.scroll = egui::style::ScrollStyle::thin());
        assert!(
            ctx.style().spacing.scroll.allocated_width() > 0.0,
            "precondition: the style must reserve scroll-bar width, otherwise this test proves nothing"
        );

        let mut widths: Vec<f32> = Vec::new();
        for frame in 0..8 {
            let input = egui::RawInput {
                // The animation advances on `time`: with no change in time, `animate_bool` sits at
                // its starting point forever
                time: Some(frame as f64 * 0.05),
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 200.0))),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ScrollArea::vertical()
                        .id_salt("aox-test-scroll")
                        .max_height(80.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            widths.push(ui.available_width());
                            // Content clearly taller than the viewport, forcing a scrollbar
                            for _ in 0..40 {
                                ui.label("x");
                            }
                        });
                });
            });
        }

        let first = widths[0];
        let changed = widths.iter().any(|w| (w - first).abs() > 0.5);
        assert!(
            changed,
                        "egui no longer changes scroll-bar allocation per frame? Then the **rationale comment** for the `ui.set_width(content_w)` block in `App::settings_overlay` must be updated -- its only purpose is to dodge this animation (it is not redundant). Measured width sequence: {widths:?}"
        );
    }
}
