//! The history drawer: the resident left column, its conversation tree, and its foot row.
//! A `SidePanel` that takes part in the layout rather than floating over it: opening it pushes the transcript...

use super::*;

/// How many conversations the history overlay lists at most.
pub(super) const HISTORY_LIST_LIMIT: usize = 80;

// ────────────── geometry of the title bar and the history drawer ──────────────.

/// Width of the history drawer.
pub(super) const HISTORY_W: f32 = 300.0;

/// Left inset of a conversation row under its workspace row.
pub(super) const HIST_INDENT: f32 = 14.0;

/// Height of the drawer's head: icon row, "new conversation", search, section title.
pub(super) const DRAWER_HEAD: f32 = 179.0;

/// Height of the drawer's foot: the rule above it plus its two rows.
pub(super) const DRAWER_FOOT: f32 = 83.0;

/// How long the drawer takes to take its width / give it back (seconds).
pub(super) const HIST_ANIM_SECS: f32 = 0.18;

/// Workspace drawer row (name, count, chevron): whole row folds; returns its response and a plus-click flag.
pub(super) fn history_group_header(
    ui: &mut Ui,
    g: &aox::space::Group,
    width: f32,
    count: usize,
    open: bool,
) -> (egui::Response, bool) {
    let w = (width - 6.0).max(60.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click());
    // Hover uses the rect, not resp.hovered(): a nested plus winning the hit test would flicker the row's fill.
    let hov = ui.rect_contains_pointer(rect);
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), P::hover_fill());
    }
    let fg = if hov { P::text() } else { P::text_dim() };
    let cy = rect.center().y;
    let icon_x = rect.left() + 11.0;

    let count = aox::tf!("{}", count);
    // Three right gutters (plus, count, chevron) are always reserved so the name never jumps sideways on hover.
    let chev_w = 20.0;
    let count_w = 22.0;
    let plus_w = 22.0;
    // Far enough right that the name clears the folder instead of sitting on its corner.
    let name_x = icon_x + 15.0;
    let name_w = (rect.right() - 6.0 - chev_w - count_w - plus_w - name_x).max(24.0);
    // Bold marks the header as a heading rather than a row (same treatment as markdown titles and
    // the activity stream's headings).
    let name = elide(ui, &g.name, theme::bold_font(12.5), fg, name_w);
    let cnt = elide(ui, &count, theme::body_font(11.0), P::text_faint(), count_w);

    let painter = ui.painter();
    // A folder, where this used to be a disclosure triangle.
    workplace_ui::paint_folder(
        painter,
        Rect::from_center_size(Pos2::new(icon_x, cy), Vec2::new(13.0, 11.0)),
        fg,
    );
    painter.galley(Pos2::new(name_x, cy - name.size().y * 0.5), name, fg);
    let cnt_x = rect.right() - 6.0 - chev_w - cnt.size().x;
    painter.galley(Pos2::new(cnt_x, cy - cnt.size().y * 0.5), cnt, P::text_faint());

    // ── the chevron ── Points down when the group is open, right when it is folded.
    let chev_x = rect.right() - 6.0 - chev_w * 0.5;
    let s = 3.6;
    let stroke = Stroke::new(1.4, if hov { P::text_dim() } else { P::text_faint() });
    let chev = if open {
        vec![
            Pos2::new(chev_x - s, cy - s * 0.5),
            Pos2::new(chev_x, cy + s * 0.6),
            Pos2::new(chev_x + s, cy - s * 0.5),
        ]
    } else {
        vec![
            Pos2::new(chev_x - s * 0.5, cy - s),
            Pos2::new(chev_x + s * 0.6, cy),
            Pos2::new(chev_x - s * 0.5, cy + s),
        ]
    };
    painter.add(egui::Shape::line(chev, stroke));

    // The plus starts a conversation here: hover-only, only for real folders, and placed before the count.
    let mut plus_clicked = false;
    if g.root.is_some() && hov {
        let plus_rect = Rect::from_center_size(
            Pos2::new(rect.right() - 6.0 - chev_w - count_w - plus_w * 0.5, cy),
            Vec2::splat(20.0),
        );
        let click = ui.interact(plus_rect, resp.id.with("new-conv"), Sense::click());
        let on = pointer_if(ui, &click);
        if on {
            // One more step than the row's own fill, so a second hoverable thing inside a hovered
            // row is still readable as "this, specifically, is the target".
            ui.painter().rect_filled(plus_rect, CornerRadius::same(theme::R0), P::active_fill());
        }
        let pfg = if on { P::ink() } else { P::text_dim() };
        let s = 3.8;
        let p = ui.painter();
        p.line_segment(
            [Pos2::new(plus_rect.center().x - s, cy), Pos2::new(plus_rect.center().x + s, cy)],
            Stroke::new(1.5, pfg),
        );
        p.line_segment(
            [Pos2::new(plus_rect.center().x, cy - s), Pos2::new(plus_rect.center().x, cy + s)],
            Stroke::new(1.5, pfg),
        );
        plus_clicked = click.clicked();
        if click.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }

    // The workspace's directory is the one thing a header can't show (a path doesn't fit, and for the built-ins there...
    let resp = match g.root.as_deref() {
        Some(root) => resp.on_hover_text(root.display().to_string()),
        None => resp,
    };
    (resp, plus_clicked)
}

/// The drawer's one button: full width, 40 high, an outline rather than a filled block.
pub(super) fn new_chat_row(ui: &mut Ui, width: f32, enabled: bool) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 40.0), sense);
    let hov = pointer_if(ui, &resp) && enabled;
    let p = ui.painter();
    p.rect_filled(
        rect,
        CornerRadius::same(theme::R1),
        if hov { P::active_fill() } else { P::hover_fill() },
    );
    p.rect_stroke(
        rect,
        CornerRadius::same(theme::R1),
        Stroke::new(1.0, P::border()),
        egui::StrokeKind::Inside,
    );
    let fg = if enabled { P::text() } else { P::quiet() };
    let cy = rect.center().y;
    let cx = rect.left() + 16.0;
    let s = 4.2;
    p.line_segment([Pos2::new(cx - s, cy), Pos2::new(cx + s, cy)], Stroke::new(1.5, fg));
    p.line_segment([Pos2::new(cx, cy - s), Pos2::new(cx, cy + s)], Stroke::new(1.5, fg));
    let label = p.layout_no_wrap(
        aox::i18n::t("New conversation").to_string(),
        theme::body_font(13.0),
        Color32::PLACEHOLDER,
    );
    p.galley(Pos2::new(rect.left() + 32.0, cy - label.size().y * 0.5), label, fg);
    // The shortcut hint is drawn, not wired: there is no global key handler for it yet, and a hint for a key...
    let hint = p.layout_no_wrap(
        "Ctrl K".to_string(),
        theme::body_font(10.5),
        Color32::PLACEHOLDER,
    );
    p.galley(
        Pos2::new(rect.right() - 12.0 - hint.size().x, cy - hint.size().y * 0.5),
        hint,
        P::text_faint(),
    );
    resp
}

/// The drawer's search field: a sunken well with a magnifier, filled in place.
pub(super) fn search_row(ui: &mut Ui, width: f32, query: &mut String) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::hover());
    let id = egui::Id::new("aox-hist-search");
    let focused = ui.memory(|m| m.has_focus(id));
    let p = ui.painter();
    // `bg`, not `code_bg`: on the drawer's `surface` this is the same one-step sunken well the
    // reference has, and it is already a token that exists in both themes.
    p.rect_filled(rect, CornerRadius::same(theme::R1), P::bg());
    if focused {
        p.rect_stroke(
            rect,
            CornerRadius::same(theme::R1),
            Stroke::new(1.0, P::border_strong()),
            egui::StrokeKind::Inside,
        );
    }
    let c = Pos2::new(rect.left() + 15.0, rect.center().y);
    p.circle_stroke(
        Pos2::new(c.x - 1.0, c.y - 1.0),
        4.2,
        Stroke::new(1.4, P::text_faint()),
    );
    p.line_segment(
        [Pos2::new(c.x + 2.0, c.y + 2.0), Pos2::new(c.x + 5.2, c.y + 5.2)],
        Stroke::new(1.4, P::text_faint()),
    );
    let inner = Rect::from_min_max(
        Pos2::new(rect.left() + 28.0, rect.top() + 7.0),
        Pos2::new(rect.right() - 8.0, rect.bottom() - 7.0),
    );
    ui.put(
        inner,
        egui::TextEdit::singleline(query)
            .id(id)
            .frame(false)
            .margin(Margin::ZERO)
            .hint_text(
                RichText::new(aox::i18n::t("Search workspaces")).color(P::quiet()).size(12.0),
            )
            .font(theme::body_font(12.0))
            .desired_width(inner.width()),
    );
}

/// A section title: small, faint, letter-spaced, with an optional count on the right.
pub(super) fn section_head(ui: &mut Ui, width: f32, label: &str, count: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 16.0), Sense::hover());
    let p = ui.painter();
    let font = theme::body_font(10.0);
    let cy = rect.center().y;
    let mut x = rect.left() + 2.0;
    for ch in label.chars() {
        let g = p.layout_no_wrap(ch.to_string(), font.clone(), Color32::PLACEHOLDER);
        p.galley(Pos2::new(x, cy - g.size().y * 0.5), g.clone(), P::text_faint());
        x += g.size().x + 1.3;
    }
    let c = p.layout_no_wrap(count.to_string(), font, Color32::PLACEHOLDER);
    p.galley(
        Pos2::new(rect.right() - 2.0 - c.size().x, cy - c.size().y * 0.5),
        c,
        P::text_faint(),
    );
}

/// Does this workspace survive the current search? A workspace matches by **name** (which keeps all
/// of its conversations) or by having at least one conversation whose title matches.
pub(super) fn group_matches(g: &aox::space::Group, query: &str) -> bool {
    g.name.to_lowercase().contains(query)
        || g.items.iter().any(|s| s.title.to_lowercase().contains(query))
}

/// The contents of a conversation's menu — shared by the row's right-click and its "⋯" button.
pub(super) fn conversation_menu(
    ui: &mut Ui,
    id: &str,
    title: &str,
    path: &Path,
    current: bool,
    move_targets: &[(String, String)],
    rename_conv: &mut Option<(String, String)>,
    move_conv: &mut Option<(String, String)>,
    delete: &mut Option<(PathBuf, String)>,
) {
    // Renaming is offered on every row, the current one included (unlike Delete below): the current conversation...
    if menu_item(ui, aox::i18n::t("Rename"), MENU_W, true, false).clicked() {
        *rename_conv = Some((id.to_string(), title.to_string()));
        ui.close();
    }
    // With the user's workspaces, and a menu that keeps getting taller is worse than one click deeper.
    if !move_targets.is_empty() {
        ui.menu_button(aox::i18n::t("Move to"), |ui| {
            for (tid, tname) in move_targets {
                if menu_item(ui, tname, MENU_W, true, false).clicked() {
                    *move_conv = Some((id.to_string(), tid.clone()));
                    ui.close();
                }
            }
        });
    }
    // The conversation being chatted in is refused rather than hidden: the agent has it open and would keep appending...
    let del = menu_item(ui, aox::i18n::t("Delete"), MENU_W, !current, true);
    if current {
        del.on_hover_text(aox::i18n::t("Cannot delete the conversation you are in"));
    } else if del.clicked() {
        // Asks first. The path *and* the label are handed over: the confirmation names the row it is about to lose...
        *delete = Some((path.to_path_buf(), title.to_string()));
        ui.close();
    }
}

/// Which glyph a history-panel foot row leads with
pub(super) enum FootIcon {
    /// A hand-drawn plus: "make one"
    Plus,
    /// The folder from [`workplace_ui::paint_folder`] — the same drawing a group header now carries:
    /// "the directory they all live in".
    Folder,
}

/// One row of the history panel's foot: a glyph, then a label.
pub(super) fn foot_row(ui: &mut Ui, width: f32, enabled: bool, icon: FootIcon, label: &str) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new((width - 6.0).max(60.0), 32.0), sense);
    let hov = pointer_if(ui, &resp) && enabled;
    if hov {
        // The same one-step fill the list rows use, so the foot reads as part of the same list
        // rather than as a second kind of control.
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), P::hover_fill());
    }
    let fg = if !enabled {
        P::quiet()
    } else if hov {
        P::text()
    } else {
        P::text_dim()
    };
    // Both glyphs sit on the same 11px gutter as a group header's folder, so the panel's left edge
    // reads as one column from the list all the way down to the foot.
    let cy = rect.center().y;
    let cx = rect.left() + 11.0;
    let painter = ui.painter();
    match icon {
        FootIcon::Plus => {
            let s = 4.0;
            let stroke = Stroke::new(1.5, fg);
            painter.line_segment([Pos2::new(cx - s, cy), Pos2::new(cx + s, cy)], stroke);
            painter.line_segment([Pos2::new(cx, cy - s), Pos2::new(cx, cy + s)], stroke);
        }
        FootIcon::Folder => {
            // The same 13x11 box the workplace picker's rows and the group headers use.
            workplace_ui::paint_folder(
                painter,
                Rect::from_center_size(Pos2::new(cx, cy), Vec2::new(13.0, 11.0)),
                fg,
            );
        }
    }
    let g = painter.layout_no_wrap(label.to_string(), theme::body_font(13.0), Color32::PLACEHOLDER);
    painter.galley(Pos2::new(rect.left() + 24.0, cy - g.size().y * 0.5), g, fg);
    resp
}

/// Show `dir` in the OS file manager.
pub(super) fn reveal_in_file_manager(dir: &Path) {
    crate::open::reveal(dir);
}

/// One conversation row in the drawer: title on the first line, "last activity · message count" on the second...
pub(super) fn history_row(
    ui: &mut Ui,
    s: &aox::history::Summary,
    current: bool,
    locked: bool,
    width: f32,
    indent: f32,
) -> (egui::Response, egui::Response) {
    let w = (width - 6.0).max(60.0); // leave room for the scroll bar
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::click());
    // Asked of the rect rather than of `resp`: the "⋯" below is a child interact target, and on the frames...
    let hov = !locked && ui.rect_contains_pointer(rect);
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // The current row is filled **whether or not** it is hovered; a hovered row that is not the
    // current one gets the lighter step. Both are one fill, no border, no marker.
    let fill = if current {
        Some(P::active_fill())
    } else if hov {
        Some(P::hover_fill())
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), fill);
    }

    let x = rect.left() + 10.0 + indent;
    let title_color = if locked {
        P::text_faint()
    } else if hov || current {
        P::text()
    } else {
        P::text_dim()
    };

    // The "⋯" gutter is always taken off the title's width, so the title does not reflow the moment
    // the pointer arrives and the glyph appears.
    let more_w = 22.0;
    let right_edge = rect.right() - 6.0 - more_w;

    // the metadata, faded in
    let meta_text = aox::tf!("{} · {} msgs", s.time, s.messages);
    let reveal = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("meta"), hov && !locked, 0.12);
    let meta_full = ui
        .painter()
        .layout_no_wrap(meta_text.clone(), theme::body_font(10.5), P::text_faint())
        .size()
        .x
        + 10.0;
    let meta_w = meta_full * reveal;

    let max_w = (right_edge - x - meta_w).max(24.0);
    let title = elide(ui, &s.title, theme::body_font(13.5), title_color, max_w);
    let cy = rect.center().y;
    let painter = ui.painter();
    painter.galley(Pos2::new(x, cy - title.size().y * 0.5), title, title_color);

    if reveal > 0.01 {
        // Fade by alpha, not `gamma_multiply`: that helper multiplies the alpha channel too, so it would both thin...
        let base = P::text_faint();
        let c = Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), (255.0 * reveal) as u8);
        let g = painter.layout_no_wrap(meta_text, theme::body_font(10.5), c);
        painter.galley(
            Pos2::new(right_edge - g.size().x, cy - g.size().y * 0.5),
            g,
            c,
        );
    }

    // ── the "⋯" ── Drawn on hover and on the current row, and never at rest: twenty rows each wearing three dots...
    let show_more = hov || current;
    let more_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 6.0 - more_w * 0.5, cy),
        Vec2::splat(20.0),
    );
    let more = ui.interact(
        more_rect,
        resp.id.with("more"),
        if show_more { Sense::click() } else { Sense::hover() },
    );
    if show_more {
        let on = pointer_if(ui, &more);
        if on {
            ui.painter()
                .rect_filled(more_rect, CornerRadius::same(theme::R0), P::active_fill());
        }
        let p = ui.painter();
        let fg = if on { P::text() } else { P::text_faint() };
        for dx in [-3.0_f32, 0.0, 3.0] {
            p.circle_filled(Pos2::new(more_rect.center().x + dx, more_rect.center().y), 1.0, fg);
        }
    }

    if locked {
        return (resp.on_hover_text(format!("{}\n{}", s.title, s.id)), more);
    }
    (
        resp.on_hover_text(aox::tf!("{}\n{}\nClick to switch to this conversation", s.title, s.id)),
        more,
    )
}

impl App {
    /// The workspace drawer: a resident left column that pushes the transcript aside.
    pub(super) fn drawer(&mut self, ctx: &egui::Context) {
        if self.hist_t <= 0.0 {
            // Fully closed: drop the query.
            self.hist_query.clear();
            return;
        }
        let ease = ease_out_cubic(self.hist_t);
        // Clicks are refused while a switch request is in flight or a turn is running: a switch started during either...
        let locked = self.busy || self.switching;

        // ── geometry: a column that takes part in the layout ──.
        let w = (HISTORY_W * ease).max(1.0);
        let inset = (12.0 * ease).round() as i8;
        let pw = (w - 2.0 * inset as f32).max(8.0);
        // The window's height comes from the largest of two independent reports, because they can disagree...
        let win_h = ctx.content_rect().height().max(
            ctx.input(|i| i.viewport().inner_rect.map_or(0.0, |r| r.height())),
        );
        // What the scrolling list gets: the window, minus the head and the foot, minus the frame's bottom margin.
        let list_h = (win_h - DRAWER_HEAD - DRAWER_FOOT - 12.0).max(120.0);

        let mut want_new_ws = false;
        let mut want_open_ws = false;
        let mut want_new_conv = false;
        let mut want_settings = false;
        let mut want_close = false;
        // Read once, for the foot row that opens `<app root>/workplace/`: "is there a root at all"
        // is also what decides whether that row is clickable.
        let app_root = self.app_root();
        // The search box's text, lower-cased **once**: `hist_tree` asks "does this row match" for
        // every row it draws, and folding the case inside that loop would be the same work N times.
        let query = self.hist_query.trim().to_lowercase();
        let mut open: Option<String> = None;
        // Group folds requested this frame.
        let mut toggles: Vec<String> = Vec::new();
        // Row / header requests, collected for the same reason as `toggles`. One slot per destination rather...
        let mut delete: Option<(PathBuf, String)> = None;
        let mut forget_ws: Option<PathBuf> = None;
        let mut reveal_ws: Option<PathBuf> = None;
        let mut rename_ws: Option<PathBuf> = None;
        // One slot for starting a conversation here, fed by the header plus and the menu row: one action, two doors.
        let mut new_conv_ws: Option<PathBuf> = None;
        // "Rename this conversation", as `(id, the title currently shown)`. The second half is what pre-fills the dialog...
        let mut rename_conv: Option<(String, String)> = None;
        // "Re-file this conversation into that group", as `(conversation id, group id)`. The group id is the id the tree...
        let mut move_conv: Option<(String, String)> = None;

        let panel = egui::SidePanel::left("aox-drawer")
            // Not resizable: the width is the animation's own output, and a drag handle would be
            // fighting it for the same number.
            .resizable(false)
            .exact_width(w)
            // The drawer's only chrome is the hairline drawn below.
            .show_separator_line(false)
            .frame(
                Frame::NONE
                    .fill(P::surface())
                    .inner_margin(Margin { left: inset, right: inset, top: 12, bottom: 12 }),
            )
            .show(ctx, |ui| {
                ui.set_width(pw);
                // Every gap in this panel is placed by hand. `item_spacing` is a theme-wide 10, and a column this dense cannot...
                ui.spacing_mut().item_spacing = Vec2::ZERO;

                // head: collapse, then settings at the far end
                ui.horizontal(|ui| {
                    // The **same** button that opened the drawer (see `sidebar_button`): one
                    // control, two places, so it reads as a toggle instead of two unrelated icons.
                    if sidebar_button(ui, true, false).clicked() {
                        want_close = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if gear_button(ui).clicked() {
                            want_settings = true;
                        }
                        // The empty gap between the two icons is the window's drag handle.
                        let gap = ui.available_rect_before_wrap();
                        let resp =
                            ui.interact(gap, ui.id().with("head-drag"), Sense::click_and_drag());
                        if resp.drag_started() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                        }
                        if resp.double_clicked() {
                            let maxed = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maxed));
                        }
                    });
                });
                ui.add_space(10.0);
                if new_chat_row(ui, pw, !locked).clicked() && !locked {
                    want_new_conv = true;
                }
                ui.add_space(10.0);
                search_row(ui, pw, &mut self.hist_query);
                ui.add_space(18.0);
                section_head(
                    ui,
                    pw,
                    aox::i18n::t("Workspaces"),
                    &aox::tf!("{}", self.hist_groups.len()),
                );
                ui.add_space(7.0);

                // Is there anything in the tree other than the two built-ins? A registered workspace counts as "something" even...
                let custom_group = self.hist_groups.iter().any(|g| g.builtin.is_none());

                // `auto_shrink([false, false])` is the load-bearing part: the scroll area takes the height we decided instead...
                ScrollArea::vertical()
                    .max_height(list_h)
                    .min_scrolled_height(list_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Rows sit 3 apart; the tree adds its own gaps between groups.
                        ui.spacing_mut().item_spacing.y = 3.0;
                        // The empty state is drawn inside the scroll area rather than short-circuiting the panel.
                        if self.history.is_empty() && !custom_group {
                            ui.label(
                                RichText::new(
                                    aox::i18n::t("No history yet.\nAfter your first message, the conversation is saved here automatically."),
                                )
                                .color(P::text_faint())
                                .size(12.5),
                            );
                            ui.add_space(10.0);
                            // A directory path can be long; use truncate so it closes itself off within the panel width instead of being cut...
                            ui.add(
                                Label::new(
                                    RichText::new(
                                        self.store
                                            .as_ref()
                                            .map(|s| s.location().display().to_string())
                                            .unwrap_or_default(),
                                    )
                                    .color(P::text_faint())
                                    .size(11.0),
                                )
                                .truncate(),
                            );
                        } else if !query.is_empty()
                            && !self.hist_groups.iter().any(|g| group_matches(g, &query))
                        {
                            // Said out loud rather than drawn as an empty column: with a query in the box, "nothing here" and "the list...
                            ui.add_space(2.0);
                            ui.label(
                                RichText::new(aox::i18n::t("No matches"))
                                    .color(P::text_faint())
                                    .size(12.0),
                            );
                        } else {
                            self.hist_tree(
                                ui,
                                pw,
                                locked,
                                &query,
                                &mut open,
                                &mut toggles,
                                &mut delete,
                                &mut forget_ws,
                                &mut reveal_ws,
                                &mut rename_ws,
                                &mut new_conv_ws,
                                &mut rename_conv,
                                &mut move_conv,
                            );
                        }
                    });

                // The foot: the actions that are not about a row.
                ui.add_space(8.0);
                let x0 = ui.cursor().left();
                let y0 = ui.cursor().top();
                ui.painter()
                    .hline(x0..=(x0 + pw), y0, Stroke::new(1.0, P::border()));
                ui.add_space(8.0);
                // "New workspace" only goes live when no turn is running: it hands this
                // conversation to a new root, which is a workspace switch like any other.
                if foot_row(ui, pw, !locked, FootIcon::Plus, aox::i18n::t("New workspace"))
                    .clicked()
                {
                    want_new_ws = true;
                }
                ui.add_space(2.0);
                // ...whereas opening the folder touches nothing at all, so this row stays live during a turn.
                if foot_row(
                    ui,
                    pw,
                    app_root.is_some(),
                    FootIcon::Folder,
                    aox::i18n::t("Open workspace folder"),
                )
                .clicked()
                {
                    want_open_ws = true;
                }

                // Resize hot-zones for the parts of the window's border that live in this column: the whole left edge...
                gwin::resize_handles(ui, ctx, ctx.content_rect());
            });
        // The drawer's only chrome: one hairline down the full height, painted from the panel's own rect (so it lands...
        let r = panel.response.rect;
        ctx.layer_painter(panel.response.layer_id)
            .vline(r.right() - 0.5, r.top()..=r.bottom(), Stroke::new(1.0, P::border()));

        // Esc belongs to the search box: with a query typed, "get me out of this" means "show me everything again"....
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !self.hist_query.is_empty() {
            self.hist_query.clear();
        }

        for id in toggles {
            self.toggle_group(&id);
        }
        // The order here is deliberate: the workspace edits come before the refresh, so the refresh
        // sees the registry as it is *after* the edit (see `refresh_history`, which re-reads it).
        if let Some(root) = reveal_ws {
            reveal_in_file_manager(&root);
        }
        if let Some(root) = forget_ws {
            // Same treatment as the delete above: the registry edit waits for a yes, and the text promises exactly what...
            if self.confirm.is_none() {
                self.confirm = Some(Confirm::RemoveWorkspace { root });
            }
        }
        if let Some(root) = rename_ws {
            // Opens the dialog; nothing is written until the name is confirmed. Unlike the two above
            // this is not an edit at all yet, which is why it is here rather than among them.
            self.start_rename_workspace(&root);
        }
        if let Some((id, current)) = rename_conv {
            // Same shape as the workspace rename: the dialog is opened, and nothing is written until "Save". The pre-fill...
            self.start_rename_conversation(&id, current);
        }
        if let Some((conv, target)) = move_conv {
            // A registry edit, done here (after the `Area` closure) because it needs `&mut self`.
            // Unlike the rename above it writes immediately -- moving a row has no dialog to confirm.
            self.move_conversation(&conv, &target);
        }
        if let Some(root) = new_conv_ws {
            // New conversation bound to that workspace -- two messages down the queue, in an order that matters (see...
            self.new_conversation_in(&root);
        }
        // Both of these used to act here, on the click.
        if let Some((path, title)) = delete {
            if self.confirm.is_none() {
                self.confirm = Some(Confirm::DeleteConversation { path, title });
            }
        }
        if want_new_ws {
            // Just opens the dialog. The folder is not touched until the user has named it and hit "Create" ...
            self.name_dialog = Some(NameDialog::new());
        }
        if want_open_ws {
            if let Some(root) = app_root.as_deref() {
                let dir = root.join(aox::workplace::WORKPLACE_DIR);
                // Made if it is not there yet: `ensure_started` normally creates it, but the panel can be opened before the agent...
                let _ = std::fs::create_dir_all(&dir);
                reveal_in_file_manager(&dir);
            }
        }
        if want_new_conv {
            // Same path the toolbar's "+" used to take.
            self.new_conversation();
        }
        if want_settings {
            self.settings_open = true;
        }
        if want_close {
            // Closing the drawer with its own × is a choice, not an accident, so it is written down like the toolbar toggle...
            self.show_history = false;
            self.save_fold_state();
        }
        if let Some(id) = open {
            self.switch_history(&id);
        }
    }

    /// Draw the workspace-grouped conversation tree inside the drawer's scroll area.
    #[allow(clippy::too_many_arguments)] // one `&mut` slot per thing the tree can ask for; see above
    pub(super) fn hist_tree(
        &self,
        ui: &mut Ui,
        width: f32,
        locked: bool,
        query: &str,
        open: &mut Option<String>,
        toggles: &mut Vec<String>,
        delete: &mut Option<(PathBuf, String)>,
        forget_ws: &mut Option<PathBuf>,
        reveal_ws: &mut Option<PathBuf>,
        rename_ws: &mut Option<PathBuf>,
        new_conv_ws: &mut Option<PathBuf>,
        rename_conv: &mut Option<(String, String)>,
        move_conv: &mut Option<(String, String)>,
    ) {
        let mut order: Vec<usize> = (0..self.hist_groups.len()).collect();
        if let Some(cur) = self.current_group_id() {
            if let Some(i) = self.hist_groups.iter().position(|g| g.id == cur) {
                order.retain(|&j| j != i);
                order.insert(0, i);
            }
        }

        let cur = self.current_history.as_deref();
        // With a query in the box the tree is flattened open.
        let filtering = !query.is_empty();
        let mut drawn = 0usize;
        for &gi in order.iter() {
            let g = &self.hist_groups[gi];
            // A workspace whose **name** matches keeps all of its conversations (you searched for
            // the place, not for a title); otherwise only the rows whose title matches survive.
            let name_hit = !filtering || g.name.to_lowercase().contains(query);
            let rows: Vec<&aox::history::Summary> = if name_hit {
                g.items.iter().collect()
            } else {
                g.items.iter().filter(|s| s.title.to_lowercase().contains(query)).collect()
            };
            if filtering && !name_hit && rows.is_empty() {
                continue;
            }
            if drawn > 0 {
                // A breath between groups; without it the last row of one group sits flush against
                // the next group's header and the whole thing reads as one undifferentiated list.
                ui.add_space(3.0);
            }
            drawn += 1;
            let unfolded = filtering || self.group_is_open(&g.id);
            let (head, plus) = history_group_header(ui, g, width, rows.len(), unfolded);
            if head.clicked() && !filtering {
                toggles.push(g.id.clone());
            }
            // Header plus and menu row share one slot; no-root groups have no plus, and the root is read from g.
            if plus {
                if let Some(root) = g.root.clone() {
                    *new_conv_ws = Some(root);
                }
            }
            // Only a real folder gets a menu. `Default` and `Full access` have no root of their own (and are not the user's...
            if let Some(root) = g.root.clone() {
                head.context_menu(|ui| {
                    // First row: makes a conversation here without hover; not gated on locked, the queue blocks mid-turn switches.
                    if menu_item(ui, aox::i18n::t("New conversation"), MENU_W, true, false).clicked() {
                        *new_conv_ws = Some(root.clone());
                        ui.close();
                    }
                    // Renaming here moves the label, never the folder: the conversations record the folder they were written...
                    if menu_item(ui, aox::i18n::t("Rename"), MENU_W, true, false).clicked() {
                        *rename_ws = Some(root.clone());
                        ui.close();
                    }
                    if menu_item(ui, aox::i18n::t("Open in file manager"), MENU_W, true, false)
                        .clicked()
                    {
                        *reveal_ws = Some(root.clone());
                        ui.close();
                    }
                    // Deliberately not styled as a danger action and not confirmed: nothing is deleted.
                    if menu_item(ui, aox::i18n::t("Remove from AOX"), MENU_W, true, false).clicked() {
                        *forget_ws = Some(root.clone());
                        ui.close();
                    }
                });
            }
            if !unfolded {
                continue;
            }
            // The "Move to" targets: every drawn group except this one.
            let move_targets: Vec<(String, String)> = self
                .hist_groups
                .iter()
                .filter(|x| x.id != g.id)
                .map(|x| (x.id.clone(), x.name.clone()))
                .collect();
            for s in rows {
                let current = Some(s.id.as_str()) == cur;
                let (row, more) = history_row(ui, s, current, locked, width, HIST_INDENT);
                if row.clicked() && !locked {
                    *open = Some(s.id.clone());
                }
                // A row's menu is offered even while a turn runs: deleting an old conversation touches nothing the running turn...
                let path = s.path.clone();
                let id = s.id.clone();
                // What the dialog's field is pre-filled with. The **title**, not the id: the id is a
                // timestamp, and "edit the timestamp" is not the thing anybody right-clicked.
                let title = s.title.clone();
                let mut menu = |ui: &mut Ui| {
                    conversation_menu(
                        ui,
                        &id,
                        &title,
                        &path,
                        current,
                        &move_targets,
                        rename_conv,
                        move_conv,
                        delete,
                    );
                };
                row.context_menu(&mut menu);
                // The "⋯" only exists on a hovered (or current) row, so this popup can only ever be opened from a visible...
                egui::Popup::menu(&more).show(&mut menu);
            }
        }
    }

    /// Whether the workspace group `id` is drawn unfolded (see [`App::hist_open`] / [`App::hist_closed`]).
    pub(super) fn group_is_open(&self, id: &str) -> bool {
        if self.hist_closed.contains(id) {
            return false;
        }
        if self.hist_open.contains(id) {
            return true;
        }
        match self.current_group_id() {
            Some(cur) => cur == id,
            // Nothing is on screen yet (a brand-new conversation that has not reached disk), so there is no "where you are"...
            None => true,
        }
    }

    /// Fold / unfold a workspace group.
    pub(super) fn toggle_group(&mut self, id: &str) {
        let now = self.group_is_open(id);
        self.hist_open.remove(id);
        self.hist_closed.remove(id);
        if now {
            self.hist_closed.insert(id.to_string());
        } else {
            self.hist_open.insert(id.to_string());
        }
        // Persisted right away: a fold is a deliberate click, it is cheap to write (one tiny file), and deferring...
        self.save_fold_state();
    }

    /// The id of the group holding the conversation being chatted in, if any.
    pub(super) fn current_group_id(&self) -> Option<&str> {
        let cur = self.current_history.as_deref()?;
        self.hist_groups
            .iter()
            .find(|g| g.items.iter().any(|s| s.id == cur))
            .map(|g| g.id.as_str())
    }
}

