//! The settings dialog: one overlay, four columns, and the small rows they are built from.
//!
//! The columns are `App` methods on purpose -- every value they show comes from the window's own state
//! (config path, model, runtimes, usage). What lives here as free functions is the vocabulary: a nav
//! row, a choice row, and a `name / value` pair whose column width has to be measured from the widest
//! name (see `kv_key_w`; a fixed width was the "英文排版混乱" bug).

use super::*;

/// The settings dialog's **content area** size (padding 20/18 excluded).
///
/// A size up from the old 380px "about" card: it has to hold a two-column table, and the right
/// column is a list of paths (`E:\rustapp\workplace\2026-10-05-...`, values that can't be short).
pub(super) const SETTINGS_W: f32 = 700.0;

/// Height hard-coded, the reason being "the card shouldn't grow and shrink when switching columns"
/// -- see the comment on `settings_overlay`.
pub(super) const SETTINGS_H: f32 = 470.0;

/// Width of the left column (the list of columns).
pub(super) const SETTINGS_NAV_W: f32 = 132.0;

/// Right column width = total - left column - the gap between the two.
pub(super) const SETTINGS_RIGHT_W: f32 = SETTINGS_W - SETTINGS_NAV_W - 24.0;

/// The height the bottom "Close" row takes up (including the gap between it and the content).
pub(super) const SETTINGS_FOOT_H: f32 = 44.0;

/// Floor for the name column in a "name + value" row.
///
/// The column is **measured**, not fixed — see `kv_key_w` for why. This is only the lower bound, so
/// that a column of short names ("History", "App root") still looks deliberate instead of hugging
/// the values. It is also what the Chinese column lands on, so the zh layout is unchanged.
pub(super) const KV_KEY_MIN: f32 = 84.0;

/// Air between a name and its value: the value's first glyph is what the eye lines up on.
pub(super) const KV_KEY_GAP: f32 = 18.0;

/// The open-source license text. **The repo's own `LICENSE`, embedded directly** -- not copied by
/// hand: a hand copy will sooner or later drift from the file, and what "About" shows has to be the
/// one actually in force.
pub(super) const LICENSE_TEXT: &str = include_str!("../../../LICENSE");

/// One column row in the left column.
///
/// The selected state is stated in two ways: "backdrop up one step + text in the primary colour"
/// plus a short vertical rule on the left. With the backdrop alone it would be nearly invisible in
/// this monochrome scheme's narrow range (`surface_hi` and `surface` are one step apart).
pub(super) fn tab_row(ui: &mut Ui, tab: SettingsTab, selected: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let fg = if selected { P::ink() } else { P::text_dim() };

    let painter = ui.painter();
    // Selected and hovered share the **same** fill; only the marker on the left tells them apart.
    // Two different greys for "you are here" and "the pointer is here" read as two different
    // states of the same thing, which they are not.
    if selected || resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(10), P::surface_hi());
    }
    if selected {
        painter.rect_filled(
            Rect::from_min_size(
                Pos2::new(rect.left() + 6.0, rect.center().y - 8.0),
                Vec2::new(3.0, 16.0),
            ),
            CornerRadius::same(2),
            fg,
        );
    }

    let g = painter.layout_no_wrap(tab.label().to_string(), egui::FontId::proportional(14.5), fg);
    painter.galley(Pos2::new(rect.left() + 18.0, rect.center().y - g.size().y * 0.5), g, fg);
    resp
}

/// A radio row (used by the "Appearance" column): a dot on the right, filled when selected.
///
/// A dot rather than a tick: the tick's shape is already taken in this UI by the "workplace tier"
/// (meaning "the tier currently in force"), and using it again would read as the same thing.
/// Filled / hollow is the drawing that needs the least explaining for "selected / not selected".
pub(super) fn choice_row(ui: &mut Ui, label: &str, selected: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(10), P::surface_hi());
    }
    let fg = if selected { P::text() } else { P::text_dim() };
    let g = painter.layout_no_wrap(label.to_string(), egui::FontId::proportional(14.5), fg);
    painter.galley(Pos2::new(rect.left() + 4.0, rect.center().y - g.size().y * 0.5), g, fg);

    let dot = Pos2::new(rect.right() - 12.0, rect.center().y);
    if selected {
        painter.circle_filled(dot, 5.0, P::ink());
    } else {
        painter.circle_stroke(dot, 5.0, Stroke::new(1.5, P::border_strong()));
    }
    resp
}

/// The "open activity stream" button: the flow line on the left + text on the right.
///
/// Why not a plain text button: that flow line is the activity stream's **only visual identity**
/// (the old top-bar icon was exactly this), and dropping it would leave this column with just a few
/// words, so you'd have to read them before knowing what opens.
///
/// When it is already open the whole button dims one step: a button labelled "open"
/// **shouldn't close the window** when clicked again, so it isn't clickable then (which is what
/// `activity::open` does too -- see its comment).
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
///
/// A row, not a bordered button. As a full-width filled block with an outline it read as a
/// **disabled input field** — which is roughly what it looked like. The page is a column of
/// settings, so the row obeys the same rules as every other row in the app: nothing at rest, one
/// fill step on hover, full ink for the label. The chevron on the right says "this goes somewhere
/// else" without a word of explanation under it.
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
        painter.rect_filled(rect, CornerRadius::same(10), P::surface_hi());
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

/// The "open cache hit map" button: the same card as [`activity_open_button`], with the grid as its
/// mark.
///
/// Sharing the card is the point, not a shortcut: both rows open "a window that observes the agent",
/// and giving the second one a different height, radius or hover treatment would make the column
/// look assembled from two different programs. What differs is the glyph — the flow line is the
/// activity stream's identity, 4×4 cells are this one's.
///
/// The mark is a miniature of the window itself (hit cells filled, the rest outlined), so the two
/// read as the same object at two scales.
pub(super) fn cache_open_button(ui: &mut Ui, on: bool) -> egui::Response {
    entry_row(ui, on, EntryMark::Grid, aox::i18n::t("Open cache hit map"))
}

/// The "open workspace view" button: the same card again, with a file tree as its mark.
///
/// Third of the trio, and deliberately the third: the flow line and the grid both observe the
/// agent, whereas this one observes **the files it touched** — a different question, so a different
/// glyph, but the same row, because all three open a window and the column should not start
/// changing shape halfway down.
pub(super) fn wsview_open_button(ui: &mut Ui, on: bool) -> egui::Response {
    entry_row(ui, on, EntryMark::Tree, aox::i18n::t("Open workspace view"))
}

/// The tree mark: a root, two branches, two leaves — the smallest shape that reads as "a directory
/// of files" at 20 points. Drawn with the same 1.4px stroke as the other two marks so the three
/// icons sit at the same weight.
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
    p.rect_stroke(f, CornerRadius::same(1), Stroke::new(1.2, fg), egui::StrokeKind::Inside);
}

/// The 4×4 mark for the cache-hit button: ten cells filled, the rest outlined.
///
/// The count is not decoration — ten of sixteen is the "mostly cached" reading the window exists to
/// make visible, and an all-filled or half-filled square would say nothing. It is drawn at 18 points,
/// so 4×4 is the largest grid that still resolves; a literal 16×16 would be a smudge.
pub(super) fn draw_grid_mark(p: &egui::Painter, r: Rect, fg: Color32) {
    const N: i32 = 4;
    let gap = 2.0;
    let cell = (r.width() - (N as f32 - 1.0) * gap) / N as f32;
    for y in 0..N {
        for x in 0..N {
            let at = Pos2::new(r.left() + x as f32 * (cell + gap), r.top() + y as f32 * (cell + gap));
            let cr = Rect::from_min_size(at, Vec2::splat(cell));
            if y * N + x < 10 {
                p.rect_filled(cr, CornerRadius::same(1), fg);
            } else {
                p.rect_stroke(
                    cr,
                    CornerRadius::same(1),
                    Stroke::new(1.0, fg.gamma_multiply(0.45)),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
}

/// The font every name in a "name + value" row is set in.
///
/// Shared with [`kv_key_w`] on purpose: the column is measured with the same font it is painted
/// with, and a second literal here would let the two drift apart silently.
pub(super) fn kv_key_font() -> egui::FontId {
    egui::FontId::proportional(13.5)
}

/// Width of the name column for a **whole** column of rows: the widest name actually drawn.
///
/// A fixed width was the wrong tool. `84` had been measured against the Chinese label ("运行时环境");
/// English names run ~40% wider — "Runtime environment" is ~118 pt — and the name is painted
/// straight onto the painter, which neither clips nor takes part in layout, so an overlong one does
/// not push anything aside: it prints **on top of** the value. That was the whole of "英文排版混乱".
///
/// It has to be the width of the widest name in the column, not per row: a per-row width would let
/// each value start wherever, and a column that lines up is the entire point of these rows. So the
/// caller collects its rows first and measures once. The cost is one layout call per name.
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
///
/// `key_w` comes from [`kv_key_w`] and must be the same for every row of a column. The colour is a
/// parameter rather than a second function: the runtime rows use it to say "not found", and one
/// entry point is one place to change.
pub(super) fn kv_row(ui: &mut Ui, key_w: f32, key: &str, val: &str, color: Color32) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(key_w, 19.0), Sense::hover());
        if !key.is_empty() {
            let p = ui.painter();
            let g = p.layout_no_wrap(key.to_string(), kv_key_font(), P::text_dim());
            p.galley(Pos2::new(r.left(), r.top() + 1.0), g, P::text_dim());
        }
        // Values are always monospace: they are paths, commands, version numbers, not prose.
        // Selectable -- paths often need copying elsewhere (a terminal, a file manager), and a
        // read-only display amounts to making people retype them.
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

impl App {
    /// Settings: one large centred card, pick on the left, read on the right.
    ///
    /// It replaces the three buttons that used to be at the top-right (activity stream / theme /
    /// about) and that 380px "about" card -- "delete all three buttons and fold them into one
    /// settings dialog" was the user's request on 2026-10-05.
    /// So three of the four columns are **relocations**: "Extra" takes the activity-stream
    /// entrance, "Appearance" takes the light/dark switch, "About" takes the version and the
    /// license; only "System" is newly written (those paths used to be half-hidden in the about
    /// card and are now listed properly).
    ///
    /// Clicking the background, pressing Esc, or clicking "Close" all dismiss it (the first two
    /// come from `Modal`).
    pub(super) fn settings_overlay(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let resp = egui::Modal::new(egui::Id::new("aox-settings"))
            .backdrop_color(P::scrim())
            .frame(
                theme::raised(theme::card_radius())
                    .inner_margin(Margin::symmetric(20, 18)),
            )
            .show(ctx, |ui| {
                // Height hard-coded: switching columns shouldn't make the card grow and shrink with
                // the content -- going from "System" (a long column of paths) to "Extra" (a single
                // button) shrinking the whole block looks bad.
                ui.set_width(SETTINGS_W);
                ui.set_height(SETTINGS_H);
                ui.horizontal_top(|ui| {
                    // The horizontal spacing in this row is **entirely hand-written by the
                    // `add_space` calls below**.
                    //
                    // Without zeroing it, `item_spacing` (8 in this theme) stacks between every
                    // allocate: 8 extra between the left column and the vertical rule, and another 8
                    // between the rule and the content -- the total width quietly becomes 716, 16
                    // more than `SETTINGS_W`. So the card is 16px wider than designed, and "how wide
                    // is the right column really" goes from computable to guesswork (which is exactly
                    // why the first version measured 758.7 instead of 740).
                    ui.spacing_mut().item_spacing.x = 0.0;

                    // ── left: the columns ──
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
                    //
                    // The rule was there to say "picking on the left, reading on the right". The
                    // two columns are already told apart by their size, their colour and the
                    // selected row's marker; a line on top of that is one more line to look at, and
                    // the card reads calmer without it. The gap does the same job.
                    ui.add_space(SETTINGS_W - SETTINGS_NAV_W - SETTINGS_RIGHT_W);

                    // ── right: the content ──
                    ui.vertical(|ui| {
                        ui.set_width(SETTINGS_RIGHT_W);
                        // The height is hard-coded too. The reason: `ScrollArea` takes "available
                        // height ∩ max_height", and in this horizontal layout the available height is
                        // affected by the other children on the same row (the vertical rule's allocate
                        // ate into it once already). With it hard-coded, the heights of "content + gap
                        // + bottom button" are all computable, so the card height can't vary with the
                        // column's content -- and that was the source of "the card grows and shrinks
                        // when you switch columns".
                        ui.set_height(SETTINGS_H);
                        // The `SETTINGS_FOOT_H` at the bottom is reserved for "Close": without
                        // subtracting it, long content pushes the button out of the card
                        ScrollArea::vertical()
                            .id_salt("aox-settings-body")
                            .max_height(SETTINGS_H - SETTINGS_FOOT_H)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                // ⚠️ This line is **what makes the whole dialog stable**; don't delete
                                // it.
                                //
                                // A `ScrollArea`'s inner width = outer width - the scrollbar's
                                // reservation, and that reservation is **animated frame by frame**
                                // (`animate_bool_responsive` x `spacing.scroll.allocated_width()`, 6px
                                // in this project). As long as the content width still follows
                                // `available_width()`, every time the scrollbar breathes the row width
                                // changes and wrapped text re-wraps -- which shows up as "the whole panel
                                // is jittering and the text looks blurry", and it **only happens when
                                // there is a repaint** (moving the mouse repaints, so it looks like "it
                                // jitters the moment I move the mouse").
                                //
                                // Pinned down: the content width is a constant, wrapping is stable, and
                                // the scrollbar only affects its own floating 6px bar (which floats to
                                // the right of the content and doesn't press on the text).
                                let content_w =
                                    SETTINGS_RIGHT_W - ui.spacing().scroll.allocated_width();
                                ui.set_width(content_w);
                                match self.settings_tab {
                                    SettingsTab::Extra => self.tab_extra(ui),
                                    SettingsTab::Appearance => self.tab_appearance(ui),
                                    SettingsTab::System => self.tab_system(ui),
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
    ///
    /// This column deliberately holds **one entrance and no settings** -- it comes first only
    /// because the user asked for that, but it really does contain just the one thing, "open the
    /// activity stream" (which used to occupy an icon in the top-right).
    pub(super) fn tab_extra(&mut self, ui: &mut Ui) {
        // Drop the lock first, then test: `is_open()` takes the lock, and `open()` in the click
        // branch takes it again -- two locks stacked in one statement is a self-deadlock (the
        // top-bar version ran into this).
        //
        // No caption line under either button. "Open the activity stream" already says what it
        // opens; a grey sentence underneath only repeats it in smaller type and adds a second
        // thing to read on a screen that holds two buttons.
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

    /// The "Appearance" column: three light/dark options (including follow system).
    ///
    /// Picking one only changes `theme::Pref`; what "follow system" actually resolves to is
    /// computed from the system theme by `theme::effective` and **rechecked every frame** (see
    /// `theme::follow_system`) -- without the recheck, "follow system" would amount to "decide
    /// once at startup by copying the system".
    pub(super) fn tab_appearance(&mut self, ui: &mut Ui) {
        let cur = theme::pref();
        for p in theme::Pref::ALL {
            if choice_row(ui, p.label(), cur == p).clicked() {
                theme::apply_pref(ui.ctx(), p);
            }
        }
    }

    /// The "System" column: this machine, this config, these directories.
    ///
    /// The order follows the list the user asked for: system info -> runtime environment ->
    /// model -> config -> workplace -> history -> plugins -> runtime directory.
    pub(super) fn tab_system(&self, ui: &mut Ui) {
        let p = self.boot.as_ref().map(|b| b.paths.clone()).unwrap_or_default();

        // Every row is **collected** before anything is drawn: the name column has to be as wide as
        // the widest name in it (see `kv_key_w`), and that cannot be known while drawing row by row.
        let mut rows: Vec<(&str, String, Color32)> = Vec::new();

        rows.push((aox::i18n::t("System info"), p.host.clone(), P::text_faint()));

        // Which command-line tool the model reaches for first. Above the language runtimes on purpose:
        // those are for *plugins*, this is for the model's own hands. The value names the flavour
        // (`git-bash` vs `busybox`), because that is the difference between a full bash and an ash —
        // and the exact executable, because "which one did it pick" is the next question.
        if !p.command_line.is_empty() {
            rows.push((aox::i18n::t("Command line"), p.command_line.clone(), P::text_faint()));
        }

        // Runtime environment: **bundled or system PATH** -- one row per language, with the
        // source written straight into brackets. Only the first row carries the name; the rest are
        // continuations and simply get the same (measured) column.
        // No ✓/✗ drawn: those two glyphs aren't guaranteed to exist in the hand-picked font (the
        // two in the call stream are hand-drawn), and the word "not found" already states the
        // conclusion clearly.
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
        //
        // Only the name chain is shown, not the full provenance tree (for the complete table use
        // the CLI's `aox --dump-config`); **with only one layer nothing is shown** -- saying
        // "there's just one layer" amounts to saying nothing. This rule was lifted from the old
        // "about" card, not a word changed.
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

        // When there is no Python on this machine, offer a one-click install. It used to live in
        // the "about" card, but "About" now only covers version and license -- installing a
        // runtime is a system-level matter, so this is the right column for it.
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
    ///
    /// The workplace **follows the conversation** (`/new` always goes back to the default tier),
    /// so what is shown here is **this conversation's** plot of land -- which is also why it can
    /// be shown without waiting for a disk scan: the bridge pushes the current state.
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

    /// The "About" column: version, license, links.
    ///
    /// The license text is **the repo's own `LICENSE`, embedded directly**, not copied out by
    /// hand -- a hand-copied version that drifts from the file means what "About" shows is no
    /// longer the license actually in force, and nobody ever double-checks that kind of error.
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
        // No rule under the masthead, and no "Full ISC license text" label above the box: the
        // masthead is already the only large type on the column, and the box below it is plainly
        // the licence. A label saying "here is the licence" is the definition of a line of text
        // that earns nothing.
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
            .corner_radius(CornerRadius::same(12))
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

    /// The name column must be wide enough for **every** name it can show, in **every** language.
    ///
    /// This is "设置-系统界面英文排版混乱" as an executable statement. The column used to be a
    /// constant (`84`) measured against the Chinese labels; English runs ~40% wider ("Runtime
    /// environment" ≈ 118 pt) and the name is painted straight onto the painter — no clipping, no
    /// participation in layout — so the overlong name printed **on top of** its value. The fix was
    /// to measure the widest name in the column (`kv_key_w`); this states the property that fix has
    /// to keep: `column >= widest name + gap`.
    ///
    /// The second assertion is the other half of the contract: the Chinese layout must not move.
    /// `KV_KEY_MIN` exists so that a column of short names still looks deliberate — if this ever
    /// fails, the minimum was changed and the zh dialog now has a gap where there wasn't one.
    #[test]
    fn the_name_column_fits_every_name_it_can_show() {
        let ctx = egui::Context::default();
        // Fonts are per-Context while `theme`'s "bold available" flags are process-global: a Context
        // that lays out text installs its own, or it measures whatever a sibling test left behind.
        crate::theme::install_fonts(&ctx);

        /// Widest of these names, painted the way `kv_row` paints them.
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
            // Chinese is the language the old constant was tuned for. It may settle a hair above
            // the floor — 67.5 + the 18 pt gap is 85.5, so it lands on 86 rather than 84 — but it
            // must not *jump*: the floor is there precisely so the zh dialog keeps the column it
            // has always had, and 2 pt is invisible while 60 would not be.
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
