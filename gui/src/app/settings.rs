//! The settings dialog: one overlay, four columns, and the small rows built from them.
//! Columns are App methods: shown values come from the window's own state (config path, model, runtimes, usage).
//! Free functions are the vocabulary: nav/choice rows and name/value pairs with a measured key column (kv_key_w).

use super::*;

/// The settings dialog's content area size (padding 20/18 excluded).
pub(super) const SETTINGS_W: f32 = 700.0;

/// Height hard-coded, the reason being "the card shouldn't grow and shrink when switching columns" -- see...
pub(super) const SETTINGS_H: f32 = 540.0;

/// Width of the left column (the list of columns).
pub(super) const SETTINGS_NAV_W: f32 = 132.0;

/// Right column width = total - left column - the gap between the two.
pub(super) const SETTINGS_RIGHT_W: f32 = SETTINGS_W - SETTINGS_NAV_W - 24.0;

/// The height the bottom "Close" row takes up (including the gap between it and the content).
pub(super) const SETTINGS_FOOT_H: f32 = 44.0;

/// Floor for the name column in a "name + value" row.
pub(super) const KV_KEY_MIN: f32 = 84.0;

/// Air between a name and its value: the value's first glyph is what the eye lines up on.
pub(super) const KV_KEY_GAP: f32 = 18.0;

/// The open-source license text.
pub(super) const LICENSE_TEXT: &str = include_str!("../../../LICENSE");

/// One column row in the left column.
pub(super) fn tab_row(ui: &mut Ui, tab: SettingsTab, selected: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let fg = if selected { P::ink() } else { P::text_dim() };

    let painter = ui.painter();
    // Selected and hovered share the same fill; only the marker on the left tells them apart.
    if selected || resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(theme::R1), P::surface_hi());
    }
    if selected {
        painter.rect_filled(
            Rect::from_min_size(
                Pos2::new(rect.left() + 6.0, rect.center().y - 8.0),
                Vec2::new(3.0, 16.0),
            ),
            CornerRadius::same(theme::R_XS),
            fg,
        );
    }

    let g = painter.layout_no_wrap(tab.label().to_string(), egui::FontId::proportional(14.5), fg);
    painter.galley(Pos2::new(rect.left() + 18.0, rect.center().y - g.size().y * 0.5), g, fg);
    resp
}

/// Card base: 1px border, 12pt radius; hover/select lifts fill and border, so rows read as controls (52pt).
fn card_bg(p: &egui::Painter, rect: Rect, hot: bool, on: bool) {
    let fill = if hot || on { P::surface_hi() } else { P::surface() };
    let edge = if hot || on { P::border_strong() } else { P::border() };
    p.rect_filled(rect, CornerRadius::same(theme::R2), fill);
    p.rect_stroke(
        rect,
        CornerRadius::same(theme::R2),
        Stroke::new(1.0, edge),
        egui::StrokeKind::Inside,
    );
}

/// The height of one card row on the settings page. The model list uses 56; the appearance column
/// keeps the same family so switching tabs doesn't feel like switching programs.
fn card_h() -> f32 {
    52.0
}

/// Choice card (light/dark/system) with a dot that fills on select; a check would collide with the tier.
fn choice_card(ui: &mut Ui, label: &str, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), card_h()), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    card_bg(ui.painter(), rect, resp.hovered(), selected);

    let fg = if selected { P::text() } else { P::text_dim() };
    let g = ui.painter().layout_no_wrap(label.to_string(), egui::FontId::proportional(15.0), fg);
    ui.painter()
        .galley(Pos2::new(rect.left() + 16.0, rect.center().y - g.size().y * 0.5), g, fg);

    let dot = Pos2::new(rect.right() - 18.0, rect.center().y);
    if selected {
        ui.painter().circle_filled(dot, 5.0, P::ink());
    } else {
        ui.painter()
            .circle_stroke(dot, 5.0, Stroke::new(1.5, P::border_strong()));
    }
    resp
}

/// The "open activity stream" button: the flow line on the left + text on the right.
pub(super) fn activity_open_button(ui: &mut Ui, on: bool) -> egui::Response {
    entry_row(ui, on, EntryMark::Flow, aox::i18n::t("Open activity stream"))
}

/// Which mark an entry row paints: the activity stream's flow line, the cache map's grid, or the
/// workspace view's tree.
#[derive(Clone, Copy)]
pub(super) enum EntryMark {
    Flow,
    Grid,
    Tree,
}

/// One "opens a window" row on the settings page.
pub(super) fn entry_row(ui: &mut Ui, on: bool, mark: EntryMark, label: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 48.0), Sense::click());
    let hot = resp.hovered() && !on;
    if hot {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // Already open: the row stays readable but stops responding. Dimming it all the way to
    // `text_faint` made it look like a broken/disabled entry rather than "the window is already up".
    let fg = if on { P::text_dim() } else { P::text() };

    let painter = ui.painter();
    if hot {
        painter.rect_filled(rect, CornerRadius::same(theme::R1), P::surface_hi());
    }
    let icon =
        Rect::from_center_size(Pos2::new(rect.left() + 20.0, rect.center().y), Vec2::splat(20.0));
    match mark {
        EntryMark::Flow => draw_flow_curve(painter, icon, fg),
        EntryMark::Grid => draw_grid_mark(painter, icon, fg),
        EntryMark::Tree => draw_tree_mark(painter, icon, fg),
    }
    let g = painter.layout_no_wrap(label.to_string(), egui::FontId::proportional(15.0), fg);
    painter.galley(Pos2::new(rect.left() + 48.0, rect.center().y - g.size().y * 0.5), g, fg);

    let c = Pos2::new(rect.right() - 20.0, rect.center().y);
    let s = 3.4;
    let st = Stroke::new(1.4, if hot { P::text() } else { P::text_faint() });
    painter.line_segment([c + Vec2::new(-s * 0.45, -s), c + Vec2::new(s * 0.65, 0.0)], st);
    painter.line_segment([c + Vec2::new(s * 0.65, 0.0), c + Vec2::new(-s * 0.45, s)], st);
    resp
}

/// The "open cache hit map" button: the same card as activity_open_button, with the grid as its mark.
pub(super) fn cache_open_button(ui: &mut Ui, on: bool) -> egui::Response {
    entry_row(ui, on, EntryMark::Grid, aox::i18n::t("Open cache hit map"))
}

/// The "open workspace view" button: the same card again, with a file tree as its mark.
pub(super) fn wsview_open_button(ui: &mut Ui, on: bool) -> egui::Response {
    entry_row(ui, on, EntryMark::Tree, aox::i18n::t("Open workspace view"))
}

/// The tree mark: a root, two branches, two leaves — the smallest shape that reads as "a directory of files" at 20...
pub(super) fn draw_tree_mark(p: &egui::Painter, r: Rect, fg: Color32) {
    let st = Stroke::new(1.4, fg);
    let x0 = r.left() + 2.5;
    let cy = r.center().y;
    // The trunk, and the two rows hanging off it.
    let y1 = r.top() + 4.5;
    let y2 = r.bottom() - 4.5;
    p.line_segment([Pos2::new(x0, y1), Pos2::new(x0, y2)], st);
    // Top branch: trunk -> elbow -> leaf
    p.line_segment([Pos2::new(x0, y1 + 3.0), Pos2::new(x0 + 4.0, y1 + 3.0)], st);
    p.circle_filled(Pos2::new(x0 + 7.0, y1 + 3.0), 1.9, fg);
    // Bottom branch
    p.line_segment([Pos2::new(x0, y2 - 3.0), Pos2::new(x0 + 4.0, y2 - 3.0)], st);
    p.circle_filled(Pos2::new(x0 + 7.0, y2 - 3.0), 1.9, fg);
    // A folder at the root end: a small open rectangle, which is what separates "a tree" from "a
    // fork in the road".
    let f = Rect::from_min_size(Pos2::new(x0 + 4.5, cy - 2.6), Vec2::new(8.0, 5.6));
    p.rect_stroke(f, CornerRadius::same(theme::R_XS), Stroke::new(1.2, fg), egui::StrokeKind::Inside);
}

/// The 4×4 mark for the cache-hit button: ten cells filled, the rest outlined.
pub(super) fn draw_grid_mark(p: &egui::Painter, r: Rect, fg: Color32) {
    const N: i32 = 4;
    let gap = 2.0;
    let cell = (r.width() - (N as f32 - 1.0) * gap) / N as f32;
    for y in 0..N {
        for x in 0..N {
            let at = Pos2::new(r.left() + x as f32 * (cell + gap), r.top() + y as f32 * (cell + gap));
            let cr = Rect::from_min_size(at, Vec2::splat(cell));
            if y * N + x < 10 {
                p.rect_filled(cr, CornerRadius::same(theme::R_XS), fg);
            } else {
                p.rect_stroke(
                    cr,
                    CornerRadius::same(theme::R_XS),
                    Stroke::new(1.0, fg.gamma_multiply(0.45)),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
}

/// The font every name in a "name + value" row is set in.
pub(super) fn kv_key_font() -> egui::FontId {
    egui::FontId::proportional(13.5)
}

/// Name column width is the widest name, measured once; a fixed 84 let wider English names overlap values.
pub(super) fn kv_key_w(ui: &Ui, keys: &[&str]) -> f32 {
    let mut w = KV_KEY_MIN;
    for k in keys {
        if k.is_empty() {
            continue;
        }
        // The same entry point the drawing code uses, so the measured width is the painted width.
        let g = ui.painter().layout_no_wrap((*k).to_owned(), kv_key_font(), P::text_dim());
        w = w.max(g.size().x + KV_KEY_GAP);
    }
    // Rounded up to a whole point: the values' x would otherwise land on a fraction, and every glyph
    // after the first in a monospace value would be nudged off the pixel grid.
    w.ceil()
}

/// A "name + value" row (the "System" column is all rows like this).
pub(super) fn kv_row(ui: &mut Ui, key_w: f32, key: &str, val: &str, color: Color32) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(key_w, 19.0), Sense::hover());
        if !key.is_empty() {
            let p = ui.painter();
            let g = p.layout_no_wrap(key.to_string(), kv_key_font(), P::text_dim());
            p.galley(Pos2::new(r.left(), r.top() + 1.0), g, P::text_dim());
        }
        // Values are always monospace: they are paths, commands, version numbers, not prose.
        ui.add(
            Label::new(RichText::new(val).font(theme::mono_font(12.5)).color(color))
                .selectable(true)
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    });
    ui.add_space(4.0);
}

/// A "name + clickable link" row (used by the "About" column). `key_w` as in [`kv_row`].
pub(super) fn link_row(ui: &mut Ui, key_w: f32, key: &str, url: &str) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(key_w, 19.0), Sense::hover());
        let p = ui.painter();
        let g = p.layout_no_wrap(key.to_string(), kv_key_font(), P::text_dim());
        p.galley(Pos2::new(r.left(), r.top() + 1.0), g, P::text_dim());

        let resp = ui.add(
            Label::new(RichText::new(url).color(P::text()).size(13.5).underline())
                .sense(Sense::click())
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
    });
    ui.add_space(4.0);
}

/// A hairline across the column.
fn rule(ui: &mut Ui) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(0), P::edge());
}

/// A segmented control: one pill per option, the chosen one filled.
fn segmented(ui: &mut Ui, salt: &str, items: &[(&str, bool)]) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(theme::R2), P::surface_hi());
    let seg_w = rect.width() / items.len() as f32;
    let mut hit = None;
    for (i, (label, on)) in items.iter().enumerate() {
        let seg = Rect::from_min_size(
            Pos2::new(rect.left() + seg_w * i as f32, rect.top()),
            Vec2::new(seg_w, rect.height()),
        );
        let resp = ui.interact(seg, egui::Id::new((salt, i)), Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let inner = seg.shrink2(Vec2::new(2.0, 2.0));
        if *on {
            ui.painter().rect_filled(inner, CornerRadius::same(theme::R2), P::surface());
        } else if resp.hovered() {
            // Unselected cells get a hover outline instead of a fill, which would look the same as selected.
            ui.painter().rect_stroke(
                inner,
                CornerRadius::same(theme::R2),
                Stroke::new(1.0, P::border_strong()),
                egui::StrokeKind::Inside,
            );
        }
        let color = if *on { P::ink() } else { P::text_dim() };
        let g = ui.painter().layout_no_wrap(
            (*label).to_string(),
            egui::FontId::proportional(14.0),
            color,
        );
        ui.painter().galley(seg.center() - g.size() * 0.5, g, color);
        if resp.clicked() {
            hit = Some(i);
        }
    }
    hit
}

/// A single-action card row (label, chevron) matching other cards in height, border and hover, not flat_button.
fn action_card(ui: &mut Ui, label: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), card_h()), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    card_bg(ui.painter(), rect, resp.hovered(), false);

    let fg = P::text();
    let g = ui.painter().layout_no_wrap(label.to_string(), egui::FontId::proportional(15.0), fg);
    ui.painter()
        .galley(Pos2::new(rect.left() + 16.0, rect.center().y - g.size().y * 0.5), g, fg);
    chevron(ui.painter(), Pos2::new(rect.right() - 18.0, rect.center().y), resp.hovered());
    resp
}

/// A two-line card for the background folder (label + path) with a right chevron; the whole card is clickable.
fn folder_card(ui: &mut Ui, label: &str, path: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 66.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    card_bg(ui.painter(), rect, resp.hovered(), false);

    let g = ui.painter().layout_no_wrap(label.to_string(), egui::FontId::proportional(15.0), P::text());
    ui.painter().galley(Pos2::new(rect.left() + 16.0, rect.top() + 13.0), g, P::text());

    // Monospace path, truncated when long; full value in the hover tip and a click opens that directory.
    let avail = rect.width() - 48.0;
    let mut job = egui::text::LayoutJob::single_section(
        path.to_owned(),
        egui::TextFormat {
            font_id: theme::mono_font(12.5),
            color: P::text_dim(),
            ..Default::default()
        },
    );
    job.wrap.max_width = avail;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    ui.painter()
        .galley(Pos2::new(rect.left() + 16.0, rect.top() + 37.0), galley, P::text_dim());

    chevron(ui.painter(), Pos2::new(rect.right() - 18.0, rect.center().y), resp.hovered());
    resp
}

/// The right chevron, shared by three cards, with the same stroke weight as entry_row.
fn chevron(p: &egui::Painter, c: Pos2, hot: bool) {
    let s = 3.4;
    let st = Stroke::new(1.4, if hot { P::text() } else { P::text_faint() });
    p.line_segment([c + Vec2::new(-s * 0.45, -s), c + Vec2::new(s * 0.65, 0.0)], st);
    p.line_segment([c + Vec2::new(s * 0.65, 0.0), c + Vec2::new(-s * 0.45, s)], st);
}

/// A one-line toggle card: label on the left, switch on the right, and the whole row is the hit area.
fn toggle_card(ui: &mut Ui, on: &mut bool, label: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), card_h()), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    card_bg(ui.painter(), rect, resp.hovered(), false);

    let sw = Rect::from_center_size(
        Pos2::new(rect.right() - 34.0, rect.center().y),
        Vec2::new(40.0, 22.0),
    );
    switch_mark(ui.painter(), sw, *on, resp.hovered());

    let g = ui.painter().layout_no_wrap(
        label.to_string(),
        egui::FontId::proportional(15.0),
        P::text(),
    );
    ui.painter().galley(
        Pos2::new(rect.left() + 16.0, rect.center().y - g.size().y * 0.5),
        g,
        P::text(),
    );
    if resp.clicked() {
        *on = !*on;
        return true;
    }
    false
}

/// The switch: a pill with a knob, filled when on. Same vocabulary as the model list's switch
/// (`model_ui::paint_switch`) — two kinds of switch in one dialog would be a bug of its own.
fn switch_mark(p: &egui::Painter, r: Rect, on: bool, hover: bool) {
    let fill = if on {
        if hover {
            P::ink()
        } else {
            P::text()
        }
    } else if hover {
        P::edge_raised()
    } else {
        P::surface_hi()
    };
    p.rect_filled(r, CornerRadius::same(theme::pill(22.0)), fill);
    if !on {
        p.rect_stroke(
            r,
            CornerRadius::same(theme::pill(22.0)),
            Stroke::new(1.0, P::edge()),
            egui::StrokeKind::Inside,
        );
    }
    let knob_x = if on { r.right() - 11.0 } else { r.left() + 11.0 };
    let knob = if on { P::surface() } else { P::text_dim() };
    p.circle_filled(Pos2::new(knob_x, r.center().y), 7.5, knob);
}

/// The presets offered behind the colour swatch.
const PRESETS: [&str; 8] = [
    "#000000", "#FFFFFF", "#F5F1E8", "#1C1C18", "#8A8A80", "#3E7B4F", "#B4543C", "#2F4B7C",
];

/// One colour: `name | swatch | #RRGGBB | Default`.
fn color_row(
    ui: &mut Ui,
    key_w: f32,
    key: &str,
    mode: theme::Mode,
    st: &mut crate::welcome::Stage,
) -> bool {
    let mut changed = false;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::hover());
    card_bg(ui.painter(), rect, false, false);

    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(16.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    let ui = &mut child;
    ui.spacing_mut().item_spacing.x = 0.0;

    let color = st.prefs.color(mode);
    let (lr, _) = ui.allocate_exact_size(Vec2::new(key_w, 18.0), Sense::hover());
    let g = ui.painter().layout_no_wrap(key.to_string(), kv_key_font(), P::text_dim());
    ui.painter().galley(Pos2::new(lr.left(), lr.top()), g, P::text_dim());
    ui.add_space(KV_KEY_GAP);

    let (sw, sw_resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
    crate::welcome::swatch(ui, sw, color);
    if sw_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let _open = egui::Popup::menu(&sw_resp)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
        .frame(theme::floating(theme::card_radius()).inner_margin(Margin::symmetric(8, 8)))
        .show(|ui| {
            ui.spacing_mut().item_spacing = Vec2::splat(4.0);
            for row in 0..2 {
                ui.horizontal(|ui| {
                    for cell in 0..4 {
                        let Some(c) = crate::welcome::parse_hex(PRESETS[row * 4 + cell]) else {
                            continue;
                        };
                        let (r, resp) = ui.allocate_exact_size(Vec2::new(34.0, 22.0), Sense::click());
                        crate::welcome::swatch(ui, r, c);
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.on_hover_text(PRESETS[row * 4 + cell]).clicked() {
                            st.set_color(mode, c);
                            changed = true;
                        }
                    }
                });
            }
        });

    ui.add_space(10.0);
    let field_w = (ui.available_width() - 66.0).clamp(76.0, 100.0);
    let r = ui.add(
        egui::TextEdit::singleline(st.draft_mut(mode))
            .desired_width(field_w)
            .margin(Margin { left: 8, right: 8, top: 5, bottom: 5 })
            .font(egui::FontId::proportional(13.0)),
    );
    // Committed on leaving the field, not per keystroke: `#E0` is not a colour, and treating
    // it as one would fight the person typing it.
    if r.lost_focus() && st.commit_draft(mode) {
        changed = true;
    }
    ui.add_space(12.0);
    let d = ui.add(
        Label::new(
            RichText::new(aox::i18n::t("Default"))
                .color(P::text_dim())
                .size(13.0)
                .underline(),
        )
        .sense(Sense::click()),
    );
    if d.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if d.clicked() {
        st.set_color(mode, crate::welcome::Prefs::default_color(mode));
        changed = true;
    }
    changed
}

/// An underlined link that opens in the browser.
fn link_label(ui: &mut Ui, text: &str, url: &str) {
    let r = ui.add(
        Label::new(RichText::new(text).color(P::text_dim()).size(12.0).underline())
            .sense(Sense::click()),
    );
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let r = r.on_hover_text(url);
    if r.clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
    }
}

/// The last failure reason, shown only on error; success draws nothing (an extra line adds only small text).
fn error_line(ui: &mut Ui, st: &crate::welcome::Stage) {
    if let Some(e) = st.note() {
        ui.label(RichText::new(e).color(P::warn()).size(12.5));
    }
}

impl App {
    /// Settings: one large centred card, pick on the left, read on the right.
    pub(super) fn settings_overlay(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let resp = egui::Modal::new(egui::Id::new("aox-settings"))
            .backdrop_color(P::scrim())
            .frame(
                theme::floating(theme::card_radius())
                    .inner_margin(Margin::symmetric(20, 18)),
            )
            .show(ctx, |ui| {
                // Height hard-coded: switching columns shouldn't make the card grow and shrink with the content -- going...
                ui.set_width(SETTINGS_W);
                ui.set_height(SETTINGS_H);
                ui.horizontal_top(|ui| {
                    // The horizontal spacing in this row is entirely hand-written by the `add_space` calls below.
                    ui.spacing_mut().item_spacing.x = 0.0;

                    // left: the columns
                    ui.vertical(|ui| {
                        ui.set_width(SETTINGS_NAV_W);
                        ui.set_height(SETTINGS_H);
                        for tab in SettingsTab::ALL {
                            if tab_row(ui, tab, self.settings_tab == tab).clicked() {
                                self.settings_tab = tab;
                            }
                        }
                    });

                    // No vertical rule between the two columns.
                    ui.add_space(SETTINGS_W - SETTINGS_NAV_W - SETTINGS_RIGHT_W);

                    // right: the content
                    ui.vertical(|ui| {
                        ui.set_width(SETTINGS_RIGHT_W);
                        // The height is hard-coded too.
                        ui.set_height(SETTINGS_H);
                        // The `SETTINGS_FOOT_H` at the bottom is reserved for "Close": without
                        // subtracting it, long content pushes the button out of the card
                        ScrollArea::vertical()
                            .id_salt("aox-settings-body")
                            .max_height(SETTINGS_H - SETTINGS_FOOT_H)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                // ️ This line is what makes the whole dialog stable; don't delete it.
                                let content_w =
                                    SETTINGS_RIGHT_W - ui.spacing().scroll.allocated_width();
                                ui.set_width(content_w);
                                match self.settings_tab {
                                    SettingsTab::Extra => self.tab_extra(ui),
                                    SettingsTab::Model => self.tab_model(ui),
                                    SettingsTab::Appearance => self.tab_appearance(ui),
                                    SettingsTab::System => self.tab_system(ui),
                                    SettingsTab::Remote => self.tab_remote(ui),
                                    SettingsTab::About => self.tab_about(ui),
                                }
                            });
                        ui.add_space(6.0);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if flat_button(ui, aox::i18n::t("Close")).clicked() {
                                open = false;
                            }
                        });
                    });
                });
            });
        // Clicking the background or pressing Esc also counts as closing
        if resp.should_close() || !open {
            self.settings_open = false;
        }
    }

    /// The "Extra" column: the activity-stream entrance.
    pub(super) fn tab_extra(&mut self, ui: &mut Ui) {
        // Drop the lock first, then test: `is_open()` takes the lock, and `open()` in the click branch takes it again ...
        let on = activity::lock(&self.act).is_open();
        if activity_open_button(ui, on).clicked() {
            activity::open(ui.ctx(), &self.act);
        }

        ui.add_space(2.0);
        let on = cache::lock(&self.cache).is_open();
        if cache_open_button(ui, on).clicked() {
            cache::open(ui.ctx(), &self.cache);
        }

        ui.add_space(2.0);
        let on = wsview::is_open(&self.wsview);
        if wsview_open_button(ui, on).clicked() {
            wsview::open(ui.ctx(), &self.wsview);
        }
    }

    /// The "Model" column: the endpoint, the key, the scanned model list, and the two numbers that decide how...
    pub(super) fn tab_model(&mut self, ui: &mut Ui) {
        let req = crate::model_ui::panel(ui, &mut self.mdl);
        self.send_requests(req);
    }

    /// The "Appearance" column: the light/dark choice, then everything about the first screen.
    pub(super) fn tab_appearance(&mut self, ui: &mut Ui) {
        let cur = theme::pref();
        for p in theme::Pref::ALL {
            if choice_card(ui, p.label(), cur == p).clicked() {
                theme::apply_pref(ui.ctx(), p);
            }
            ui.add_space(8.0);
        }

        ui.add_space(14.0);
        rule(ui);
        ui.add_space(15.0);
        // A small heading (not body text) naming the three options custom / hitokoto / model, at the card-label size.
        ui.label(
            RichText::new(aox::i18n::t("Welcome line"))
                .color(P::text())
                .size(15.0),
        );
        ui.add_space(10.0);

        // "Restart the line" vs "write the file": a colour or a switch changes what is on the first screen right now...
        let mut restart = false;
        let mut save = false;

        // the source
        let items: Vec<(&str, bool)> =
            crate::welcome::Source::ALL.iter().map(|s| (s.label(), self.welcome.prefs.source == *s)).collect();
        if let Some(hit) = segmented(ui, "aox-welcome-source", &items) {
            let picked = crate::welcome::Source::ALL[hit];
            if picked != self.welcome.prefs.source {
                self.welcome.prefs.source = picked;
                restart = true;
                save = true;
            }
        }
        ui.add_space(10.0);

        match self.welcome.prefs.source {
            crate::welcome::Source::Custom => {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let field_w = ui.available_width() - 46.0;
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.welcome.prefs.custom)
                            .desired_width(field_w)
                            .margin(Margin { left: 12, right: 12, top: 9, bottom: 9 })
                            .font(egui::FontId::proportional(14.5))
                            .hint_text(
                                RichText::new(aox::i18n::t("One short sentence"))
                                    .color(P::quiet())
                                    .size(13.5),
                            ),
                    );
                    // 40 characters is what still reads as one line at 26pt on a 1280 window; cutting here is honest (the field...
                    let n = self.welcome.prefs.custom.chars().count();
                    if n > 40 {
                        self.welcome.prefs.custom =
                            self.welcome.prefs.custom.chars().take(40).collect();
                    }
                    if r.changed() {
                        save = true;
                    }
                    if r.lost_focus() {
                        restart = true;
                    }
                    // The counter shows only near the limit; before that it is useless, only then answering how many are left.
                    if n >= 34 {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(format!("{n}/40"))
                                .color(if n >= 40 { P::warn() } else { P::text_faint() })
                                .size(12.0),
                        );
                    }
                });
            }
            crate::welcome::Source::Hitokoto => {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.welcome.prefs.hitokoto_url)
                        .desired_width(ui.available_width())
                        .margin(Margin { left: 10, right: 10, top: 7, bottom: 7 })
                        .font(egui::FontId::proportional(13.5))
                        .hint_text(
                            RichText::new(aox::hello::HITOKOTO_DEFAULT)
                                .color(P::quiet())
                                .size(13.0),
                        ),
                );
                if r.changed() {
                    save = true;
                }
                // An emptied field goes back to the documented default rather than becoming a
                // request to nowhere.
                if r.lost_focus() {
                    if self.welcome.prefs.hitokoto_url.trim().is_empty() {
                        self.welcome.prefs.hitokoto_url = aox::hello::HITOKOTO_DEFAULT.to_string();
                        save = true;
                    }
                    restart = true;
                }
                ui.add_space(8.0);
                link_label(ui, aox::i18n::t("Hitokoto API documentation"), aox::hello::HITOKOTO_DOCS);
                error_line(ui, &self.welcome);
            }
            crate::welcome::Source::Model => {
                // No help paragraph: the option names say it all, and the model line is judged from the first screen.
                if action_card(ui, aox::i18n::t("Another one")).clicked() {
                    if let Some(ask) = self.welcome.refresh() {
                        self.request_hello(ask);
                    }
                }
                error_line(ui, &self.welcome);
            }
        }

        ui.add_space(16.0);

        // the two colours
        let keys = [aox::i18n::t("Text colour · light"), aox::i18n::t("Text colour · dark")];
        let key_w = kv_key_w(ui, &keys);
        for (i, mode) in [theme::Mode::Light, theme::Mode::Dark].into_iter().enumerate() {
            if color_row(ui, key_w, keys[i], mode, &mut self.welcome) {
                restart = true;
                save = true;
            }
            ui.add_space(8.0);
        }

        ui.add_space(8.0);
        // the two switches
        if toggle_card(ui, &mut self.welcome.prefs.typing, aox::i18n::t("Typewriter effect")) {
            restart = true;
            save = true;
        }
        ui.add_space(8.0);
        if toggle_card(ui, &mut self.welcome.prefs.background, aox::i18n::t("Background image")) {
            restart = true;
            save = true;
            // Takes effect this frame: the texture is released, or the pool is re-scanned.
            self.brand.set_enabled(self.welcome.prefs.background);
        }

        ui.add_space(14.0);
        // where the pictures go
        let dir = aox::paths::app_root().join("assets");
        let hit = folder_card(
            ui,
            aox::i18n::t("Background folder"),
            &dir.display().to_string(),
        );
        if hit.clicked() {
            // Create both pools first: a link that opens onto "this folder is empty" is a link
            // that taught the user nothing.
            let _ = std::fs::create_dir_all(dir.join("light"));
            let _ = std::fs::create_dir_all(dir.join("dark"));
            let _ = crate::open::reveal(&dir);
        }
        hit.on_hover_text(aox::i18n::t("Open this folder"));

        ui.add_space(8.0);
        // The only kept small text explains file layout (light/, dark/, PNG only, random), invisible in the UI.
        ui.label(
            RichText::new(aox::i18n::t(
                "light/ for the light theme, dark/ for the dark one. PNG only; one is picked at \
                 random on every start and every new conversation.",
            ))
            .color(P::text_faint())
            .size(11.5),
        );

        if save {
            let _ = self.welcome.prefs.save(&aox::paths::app_root());
        }
        if restart {
            self.welcome.restart();
        }
    }

    /// The "System" column: this machine, this config, these directories.
    pub(super) fn tab_system(&self, ui: &mut Ui) {
        let p = self.boot.as_ref().map(|b| b.paths.clone()).unwrap_or_default();

        // Every row is **collected** before anything is drawn: the name column has to be as wide as
        // the widest name in it (see `kv_key_w`), and that cannot be known while drawing row by row.
        let mut rows: Vec<(&str, String, Color32)> = Vec::new();

        rows.push((aox::i18n::t("System info"), p.host.clone(), P::text_faint()));

        // Which command-line tool the model reaches for first.
        if !p.command_line.is_empty() {
            rows.push((aox::i18n::t("Command line"), p.command_line.clone(), P::text_faint()));
        }

        // Runtime environment: bundled or system PATH -- one row per language, with the source written straight...
        for (i, it) in self.runtimes.items.iter().enumerate() {
            rows.push((
                if i == 0 { aox::i18n::t("Runtime environment") } else { "" },
                format!("{}  {}  [{}]", it.lang, it.command, it.origin),
                if it.ok { P::text_faint() } else { P::warn() },
            ));
        }

        if !self.model.is_empty() {
            rows.push((aox::i18n::t("Model in use"), self.model.clone(), P::text_faint()));
        }
        if !p.config.is_empty() {
            rows.push((aox::i18n::t("Config file"), p.config.clone(), P::text_faint()));
        }
        // Which layers this config is merged from: `core -> patch -> config.toml`.
        let layers: Vec<String> = self
            .boot
            .as_ref()
            .map(|b| {
                b.config_layers
                    .iter()
                    .map(|l| l.strip_prefix("bundle:").unwrap_or(l).to_string())
                    .collect()
            })
            .unwrap_or_default();
        if layers.len() > 1 {
            rows.push((aox::i18n::t("Config layers"), layers.join(" → "), P::text_faint()));
        }
        if !p.root.is_empty() {
            rows.push((aox::i18n::t("App root"), p.root.clone(), P::text_faint()));
        }
        rows.push((aox::i18n::t("Workspace"), self.workplace_line(), P::text_faint()));
        if !p.history.is_empty() {
            rows.push((aox::i18n::t("History"), p.history.clone(), P::text_faint()));
        }
        if !p.plugins.is_empty() {
            rows.push((aox::i18n::t("Plugins directory"), p.plugins.clone(), P::text_faint()));
        }
        if !p.runtime.is_empty() {
            // "if it's bundled" -- whether the directory is there tells you at a glance whether
            // the bundled runtime is in use
            let note = if std::path::Path::new(&p.runtime).is_dir() {
                ""
            } else {
                aox::i18n::t(" (built-in runtime not installed)")
            };
            rows.push((
                aox::i18n::t("Runtime directory"),
                format!("{}{note}", p.runtime),
                P::text_faint(),
            ));
        }

        let keys: Vec<&str> = rows.iter().map(|r| r.0).collect();
        let key_w = kv_key_w(ui, &keys);
        for (k, v, c) in &rows {
            kv_row(ui, key_w, k, v, *c);
        }

        // When there is no Python on this machine, offer a one-click install.
        if self.runtimes.python_missing {
            ui.add_space(14.0);
            ui.label(
                RichText::new(aox::i18n::t("Python is missing on this machine; file I/O, system info and similar capabilities are unavailable."))
                    .color(P::text_dim())
                    .size(13.5),
            );
            ui.add_space(8.0);
            if primary_button(ui, aox::i18n::t("Install Python runtime (~10 MB)")).clicked() {
                // Goes down exactly the same command path as the CLI (handled by the host itself,
                // not sent to the model)
                let _ = self.tx.send(ToAgent::Input("/runtime install python".into(), Vec::new()));
            }
        }
    }

    /// The workplace row: "path + tier".
    pub(super) fn workplace_line(&self) -> String {
        use aox::workplace::Mode;
        let Some(v) = self.ws.view() else {
            // The first frame or two after startup hasn't received an acknowledgement yet. Don't
            // guess a directory; say honestly that it isn't known yet.
            return aox::i18n::t("(no acknowledgement yet)").into();
        };
        let name = match &v.mode {
            Mode::Default => aox::i18n::t("Default tier").to_string(),
            Mode::Full => aox::i18n::t("Full access tier (writes unrestricted)").to_string(),
            Mode::Fixed(p) => aox::tf!("Fixed tier {}", p.display()),
        };
        match &v.root {
            Some(r) => aox::tf!("{} ({})", r.display(), name),
            // The `default` tier only opens its plot once the first message arrives. This is
            // deliberately stated separately from "unrestricted" -- two different things.
            None => aox::tf!("({} · plot not opened yet)", name),
        }
    }

    /// About column (version, repo LICENSE, links); the Remote tab only draws state from a bridge on its own thread.
    pub(super) fn tab_remote(&mut self, ui: &mut Ui) {
        self.remote.page(ui);
    }

    pub(super) fn tab_about(&self, ui: &mut Ui) {
        // Masthead: AOX GUI in a serif face + the version, with a thin rule underneath (the same
        // vocabulary as the empty state)
        ui.horizontal(|ui| {
            ui.label(RichText::new("AOX GUI").font(theme::serif_font(24.0)).color(P::text()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .color(P::text_faint())
                        .size(13.5),
                );
            });
        });
        ui.add_space(12.0);
        // No rule under the masthead, and no "Full ISC license text" label above the box: the masthead is already...
        ui.label(
            RichText::new(aox::i18n::t("AOX GUI, written in Rust. Licensed under the open-source ISC license."))
                .color(P::text_dim())
                .size(14.0),
        );

        ui.add_space(14.0);
        // The license text sits in a filled box. **Selectable**: when quoting a clause you can
        // copy it directly instead of retyping it off the screen.
        Frame::NONE
            .fill(P::code_bg())
            .corner_radius(CornerRadius::same(theme::R2))
            .stroke(Stroke::new(1.0, P::edge()))
            .inner_margin(Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.add(
                    Label::new(
                        RichText::new(LICENSE_TEXT.trim_end())
                            .font(theme::mono_font(12.0))
                            .color(P::text_faint()),
                    )
                    .selectable(true)
                    .wrap_mode(egui::TextWrapMode::Wrap),
                );
            });

        ui.add_space(16.0);
        // The two links are one column, so they share one measured width — "Author's blog" is the
        // wider of the two and would otherwise print over its own URL.
        let link_keys = [aox::i18n::t("Author's blog"), aox::i18n::t("Project home")];
        let link_key_w = kv_key_w(ui, &link_keys);
        link_row(ui, link_key_w, link_keys[0], "https://2am.top");
        link_row(ui, link_key_w, link_keys[1], "https://github.com/forever218/AOX");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEM_EN: [&str; 11] = [
        "System info",
        "Command line",
        "Runtime environment",
        "Model in use",
        "Config file",
        "Config layers",
        "App root",
        "Workspace",
        "History",
        "Plugins directory",
        "Runtime directory",
    ];
    const SYSTEM_ZH: [&str; 11] = [
        "系统信息",
        "命令行",
        "运行时环境",
        "所用模型",
        "配置文件",
        "配置层",
        "应用根目录",
        "工作区",
        "历史记录",
        "插件目录",
        "运行时目录",
    ];
    const ABOUT_EN: [&str; 2] = ["Author's blog", "Project home"];
    const ABOUT_ZH: [&str; 2] = ["作者博客", "项目地址"];

    /// Name column must hold the widest label in any language; KV_KEY_MIN keeps the Chinese layout from moving.
    #[test]
    fn the_name_column_fits_every_name_it_can_show() {
        let ctx = egui::Context::default();
        // Fonts are per-Context while `theme`'s "bold available" flags are process-global: a Context
        // that lays out text installs its own, or it measures whatever a sibling test left behind.
        crate::theme::install_fonts(&ctx);

        /// Widest of these names, painted the way `kv_row` paints them
        fn widest(ui: &Ui, keys: &[&str]) -> f32 {
            keys.iter()
                .map(|k| {
                    ui.painter()
                        .layout_no_wrap((*k).to_owned(), kv_key_font(), P::text_dim())
                        .size()
                        .x
                })
                .fold(0.0f32, f32::max)
        }

        let cases: [(&str, &[&str], &[&str]); 2] =
            [("System", &SYSTEM_EN, &SYSTEM_ZH), ("About", &ABOUT_EN, &ABOUT_ZH)];
        // (column, key_w for English, key_w for Chinese, widest English name, widest Chinese name)
        let mut measured: Vec<(&str, f32, f32, f32, f32)> = Vec::new();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                for (col, en, zh) in cases {
                    measured.push((
                        col,
                        kv_key_w(ui, en),
                        kv_key_w(ui, zh),
                        widest(ui, en),
                        widest(ui, zh),
                    ));
                }
            });
        });

        for (col, key_en, key_zh, widest_en, widest_zh) in measured {
            println!(
                "{col}: en column={key_en:.1} (widest {widest_en:.1}) · \
                 zh column={key_zh:.1} (widest {widest_zh:.1})"
            );
            assert!(
                key_en + 0.01 >= widest_en + KV_KEY_GAP,
                "{col}: the column is {key_en:.1} pt but the widest English name needs \
                 {:.1} — it will print over its value",
                widest_en + KV_KEY_GAP
            );
            assert!(
                key_zh + 0.01 >= widest_zh + KV_KEY_GAP,
                "{col}: the column is {key_zh:.1} pt but the widest Chinese name needs {:.1}",
                widest_zh + KV_KEY_GAP
            );
            // Chinese is the language the old constant was tuned for.
            assert!(
                key_zh <= KV_KEY_MIN + 4.0,
                "{col}: the Chinese column jumped from {KV_KEY_MIN} to {key_zh:.1}"
            );
            // The values are paths; a name column that eats the panel would wrap them for nothing.
            assert!(
                key_en <= SETTINGS_RIGHT_W - 240.0,
                "{col}: a {key_en:.1} pt name column leaves too little room for values \
                 (panel {SETTINGS_RIGHT_W:.0} pt)"
            );
        }
    }
}
