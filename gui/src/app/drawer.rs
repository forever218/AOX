//! The history drawer: the resident left column, its conversation tree, and its foot row.
//!
//! A `SidePanel` that **takes part in the layout** rather than floating over it: opening it pushes the
//! transcript aside by exactly the panel's width (`hist_t` drives both). This file owns the tree's
//! defaults too -- a group is drawn open iff it holds the current conversation unless the user has said
//! otherwise, which is what keeps the panel short while still showing you where you are.

use super::*;

/// How many conversations the history overlay lists at most.
///
/// No pagination: a few dozen is already plenty for "scroll back and find the one from just now",
/// whereas listing everything would mean scanning the whole directory on every refresh. To really
/// go further back, both `/history` and the `history/` directory are there.
pub(super) const HISTORY_LIST_LIMIT: usize = 80;

// ────────────── geometry of the title bar and the history drawer ──────────────
//
// These numbers used to be scattered across two places: the buttons' landing spots lived in
// `top_bar`'s `exact_height(50)` + `Margin::symmetric(20, 0)`, and the history column's width
// lived in `SidePanel::default_width(266)`. With both the top bar and the side column removed they
// have no "container" to lean on, so they are pinned explicitly here.

/// Width of the history drawer.
///
/// 300: the drawer is a **full-height column**, not a card with a list in it, so it is sized like a
/// sidebar (a share of the window, not "as wide as the titles happen to need"). Conversation titles
/// are elided against what is left -- at this width that is ~24 Han characters, and the row's
/// second line already carries the time and the message count.
pub(super) const HISTORY_W: f32 = 300.0;

/// Left inset of a conversation row under its workspace row.
///
/// 14 rather than the old 18: the row no longer leads with a dot (the nesting is read from the
/// indent plus the folder above it), so it only has to clear the workspace row's icon column.
pub(super) const HIST_INDENT: f32 = 14.0;

/// Height of the drawer's head: icon row, "new conversation", search, section title.
///
/// **Exact, not measured**: the drawer zeroes `item_spacing` and places every gap by hand (see
/// [`App::drawer`]), so this is a plain sum -- 12 (top margin) + 32 (icon row) + 10 + 40 (new
/// conversation) + 10 + 34 (search) + 18 + 16 (section title) + 7. If one of those gaps is edited
/// below, this number has to follow, or the list gets the wrong allowance.
pub(super) const DRAWER_HEAD: f32 = 179.0;

/// Height of the drawer's foot: the rule above it plus its two rows.
///
/// 8 (gap) + 1 (rule) + 8 + 32 + 2 + 32. Same rule as [`DRAWER_HEAD`]: a plain sum of what is drawn.
pub(super) const DRAWER_FOOT: f32 = 83.0;

/// How long the drawer takes to take its width / give it back (seconds).
///
/// Shorter than the 0.24 this used to be: the drawer now **pushes** the transcript, so every frame
/// of the animation is a frame in which the text re-wraps. 0.18 still reads as motion and gets the
/// reflow over with before the eye starts following the words.
pub(super) const HIST_ANIM_SECS: f32 = 0.18;

/// One workspace row in the drawer: a folder, the name, how many conversations are in it, and a
/// disclosure chevron on the right. The whole row is the click target for folding / unfolding.
///
/// The count is drawn whether or not the group is folded: `list()` scans every workspace in one go
/// (see the store's multi-directory layout), so the number is already in hand -- a "not counted yet"
/// placeholder would be a promise the panel has no reason to break. While a search is running it is
/// the number of **matches**, not of conversations, because that is what is under the row.
///
/// Returns the row's own response (the fold toggle) plus whether the hovered "＋" was clicked. Two
/// answers rather than one because they are two different gestures on one rect: the "＋" is a
/// **child** click target nested inside the row, and it has to be the one that wins the pointer or
/// "new conversation here" would also fold the group the user is pointing at.
pub(super) fn history_group_header(
    ui: &mut Ui,
    g: &aox::space::Group,
    width: f32,
    count: usize,
    open: bool,
) -> (egui::Response, bool) {
    let w = (width - 6.0).max(60.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click());
    // Pointer over the row -- asked of the **rect**, not `resp.hovered()`. The "＋" below is a child
    // interact target nested inside this rect, so on the frames the pointer is on it the row is no
    // longer the *hovered widget*; `resp.hovered()` would flip to false and take the row's fill --
    // and, since the "＋" is only drawn while the row reads as hovered, the glyph with it -- which is
    // a stable-looking half-frame flicker: draw, erase, draw. `rect_contains_pointer` is the one
    // reading that does not depend on which of the two widgets won the hit test.
    let hov = ui.rect_contains_pointer(rect);
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().rect_filled(rect, CornerRadius::same(8), P::hover_fill());
    }
    let fg = if hov { P::text() } else { P::text_dim() };
    let cy = rect.center().y;
    let icon_x = rect.left() + 11.0;

    let count = aox::tf!("{}", count);
    // Three gutters on the right, always taken off the name's width whether or not the glyphs are
    // drawn right now: the "＋" (hover only), the count, and the chevron. If the width only shrank
    // on hover, the name would jump sideways the instant the pointer arrived -- the one thing a
    // hover affordance must never do, since it moves the ground out from under the click that is
    // about to happen.
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
    // A folder, where this used to be a disclosure triangle. What it adds is the one thing a
    // header could never show: that this row is a *directory*, which is also what tells a workspace
    // apart from a conversation at a glance. Same 13x11 box the workplace picker's rows use, so the
    // two trees draw the same folder. The folded-or-not signal is the chevron on the right.
    workplace_ui::paint_folder(
        painter,
        Rect::from_center_size(Pos2::new(icon_x, cy), Vec2::new(13.0, 11.0)),
        fg,
    );
    painter.galley(Pos2::new(name_x, cy - name.size().y * 0.5), name, fg);
    let cnt_x = rect.right() - 6.0 - chev_w - cnt.size().x;
    painter.galley(Pos2::new(cnt_x, cy - cnt.size().y * 0.5), cnt, P::text_faint());

    // ── the chevron ──
    // Points down when the group is open, right when it is folded. Drawn as two strokes rather than
    // a glyph: a font's "v" changes shape with whichever face the build picked, and this one has to
    // sit on the same optical centre as the folder.
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

    // ── the "＋": new conversation *in this workspace* ──
    //
    // Drawn only for a group with a real folder (`g.root.is_some()`), matching the context menu
    // below: `Default` and `Full access` are not directories, so there is nowhere to put a
    // conversation and the glyph would be a lie. Drawn only on hover for the same reason it is
    // hidden at rest -- at rest the header is a *label*, and five of them at once wearing a "＋"
    // would turn a list of workspaces into a row of buttons.
    //
    // It sits **before the count** rather than at the far right: the count is the row's own data and
    // it should not be pushed around by a control, and "＋ 3" reads as "add one to these three",
    // which is exactly what the click does.
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
            ui.painter().rect_filled(plus_rect, CornerRadius::same(6), P::active_fill());
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

    // The workspace's directory is the one thing a header can't show (a path doesn't fit, and for
    // the built-ins there is nothing to show anyway), so it rides on the tooltip: with several
    // workspaces listed, "which folder is this one again" is exactly the question the tree raises.
    let resp = match g.root.as_deref() {
        Some(root) => resp.on_hover_text(root.display().to_string()),
        None => resp,
    };
    (resp, plus_clicked)
}

/// The drawer's one **button**: full width, 40 high, an outline rather than a filled block.
///
/// A reversed (ink) block would shout louder than anything else in the drawer, and "new
/// conversation" is the one action here that is also reachable from the keyboard — so it is drawn
/// as the quietest thing that still reads as pressable. `enabled == false` (a turn is running)
/// keeps the shape and only dims it: a button that disappears mid-turn is worse than one that says
/// "not now".
pub(super) fn new_chat_row(ui: &mut Ui, width: f32, enabled: bool) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 40.0), sense);
    let hov = pointer_if(ui, &resp) && enabled;
    let p = ui.painter();
    p.rect_filled(
        rect,
        CornerRadius::same(9),
        if hov { P::active_fill() } else { P::hover_fill() },
    );
    p.rect_stroke(
        rect,
        CornerRadius::same(9),
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
    // The shortcut hint is drawn, not wired: there is no global key handler for it yet, and a hint
    // for a key that does nothing is exactly the kind of small lie that makes a UI feel unreliable.
    // It is here because the row is the drawer's one action and the reference puts it in this
    // corner; the binding lands with the rest of the keyboard work.
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
///
/// The `TextEdit` is placed **inside** an already-allocated rect (`ui.put`) rather than laid out,
/// so the well, the glyph and the field share one box; `frame(false)` keeps the widget from drawing
/// a second background inside the first. Focus is read from memory *before* the field is put down,
/// so the border is right on the frame the click lands rather than one frame later.
pub(super) fn search_row(ui: &mut Ui, width: f32, query: &mut String) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::hover());
    let id = egui::Id::new("aox-hist-search");
    let focused = ui.memory(|m| m.has_focus(id));
    let p = ui.painter();
    // `bg`, not `code_bg`: on the drawer's `surface` this is the same one-step sunken well the
    // reference has, and it is already a token that exists in both themes.
    p.rect_filled(rect, CornerRadius::same(9), P::bg());
    if focused {
        p.rect_stroke(
            rect,
            CornerRadius::same(9),
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

/// A section title: small, faint, **letter-spaced**, with an optional count on the right.
///
/// The tracking is drawn by hand (one glyph at a time) because `RichText` has no letter-spacing and
/// this is the one place in the panel where the spacing *is* the design: at 10px, three Han
/// characters set solid read as a stray word, and the same three with 1.3px between them read as a
/// label over a list.
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
///
/// A free function rather than a closure: the two doors would otherwise each need their own copy of
/// the body, and the slots they write to are `&mut` borrows that a single closure could only hand
/// out once.
pub(super) fn conversation_menu(
    ui: &mut Ui,
    id: &str,
    title: &str,
    path: &Path,
    current: bool,
    move_targets: &[(String, String)],
    rename_conv: &mut Option<(String, String)>,
    move_conv: &mut Option<(String, String)>,
    recycle: &mut Vec<PathBuf>,
) {
    // Renaming is offered on **every** row, the current one included (unlike Delete below): the
    // current conversation is the one whose file the agent already has open, and the append is
    // exactly what makes the rename work there.
    if menu_item(ui, aox::i18n::t("Rename"), MENU_W, true, false).clicked() {
        *rename_conv = Some((id.to_string(), title.to_string()));
        ui.close();
    }
    // Move to → another group. A submenu rather than one flat row per group: the target list grows
    // with the user's workspaces, and a menu that keeps getting taller is worse than one click
    // deeper. Offered on every row including the current one -- re-filing is a registry edit, so it
    // touches neither the open file nor the running turn. Hidden when there is only one group, so it
    // is never a row that opens onto nothing.
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
    // The conversation being chatted in is refused rather than hidden: the agent has it open and
    // would keep appending to a file that is no longer there. The row stays visible but greyed, with
    // the reason on hover, so the menu looks the same everywhere and the missing row is never a
    // mystery.
    let del = menu_item(ui, aox::i18n::t("Delete"), MENU_W, !current, true);
    if current {
        del.on_hover_text(aox::i18n::t("Cannot delete the conversation you are in"));
    } else if del.clicked() {
        recycle.push(path.to_path_buf());
        ui.close();
    }
}

/// Which glyph a history-panel foot row leads with.
pub(super) enum FootIcon {
    /// A hand-drawn plus: "make one".
    Plus,
    /// The folder from [`workplace_ui::paint_folder`] — the same drawing a group header now carries:
    /// "the directory they all live in".
    Folder,
}

/// One row of the history panel's foot: a glyph, then a label.
///
/// Both rows go through here rather than each drawing itself: the hover fill, the three-way colour
/// rule and the click sense are the parts with anything to get wrong, and they have to be identical
/// or the two rows would answer the same pointer differently.
///
/// Hand-drawn like every other icon in this file (the folder in `workplace_ui`): a font's `+` would
/// change shape and weight with whichever font the build picked. `enabled == false` swaps the sense
/// to hover, so the row is **visible but not clickable** rather than absent -- the panel's shape does
/// not depend on whether a turn happens to be running.
pub(super) fn foot_row(ui: &mut Ui, width: f32, enabled: bool, icon: FootIcon, label: &str) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new((width - 6.0).max(60.0), 32.0), sense);
    let hov = pointer_if(ui, &resp) && enabled;
    if hov {
        // The same one-step fill the list rows use, so the foot reads as part of the same list
        // rather than as a second kind of control.
        ui.painter().rect_filled(rect, CornerRadius::same(8), P::hover_fill());
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
///
/// Delegates to [`crate::open`]: there is now a second caller (a clickable path in a reply) that
/// needs exactly this behaviour, and two copies of "how this OS shows a folder" is one copy too
/// many — the copy that gets fixed is never the copy that is used.
pub(super) fn reveal_in_file_manager(dir: &Path) {
    crate::open::reveal(dir);
}

/// One conversation row in the drawer: title on the first line, "last activity · message count" on
/// the second, and a "⋯" on the right once the pointer is on it.
///
/// `current` = the one being chatted in right now. It is marked by **fill only** — no dot, no left
/// bar: at rest the list is plain text on paper, and the row you are in is one shade darker than the
/// rest, which is the same signal a hover gives. `locked` = a switch request is in flight or a turn
/// is running, in which case clicks are refused and the whole row is dimmed one step. `indent`
/// shifts the text right so the row sits nested under its workspace row (the fill still spans the
/// full width, so the clickable strip doesn't shrink with the depth).
///
/// Returns the row's response plus the "⋯" child response — the second is the anchor for the row's
/// menu (`Popup::menu`), so the same menu can be opened by right-clicking the row or by clicking the
/// glyph. It is only a *click* target while the glyph is drawn; the rest of the time it senses hover,
/// so it can never swallow a click meant for the row.
pub(super) fn history_row(
    ui: &mut Ui,
    s: &aox::history::Summary,
    current: bool,
    locked: bool,
    width: f32,
    indent: f32,
) -> (egui::Response, egui::Response) {
    let w = (width - 6.0).max(60.0); // leave room for the scroll bar
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 48.0), Sense::click());
    // Asked of the rect rather than of `resp`: the "⋯" below is a child interact target, and on the
    // frames the pointer is over it the row is no longer the *hovered widget* -- which would make
    // the fill (and, since the glyph is only drawn while the row reads as hovered, the glyph with
    // it) flicker off. Same reasoning as the workspace row.
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
        ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
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
    let max_w = (rect.right() - 6.0 - more_w - x).max(24.0);
    let title = elide(ui, &s.title, theme::body_font(13.5), title_color, max_w);
    let sub = elide(
        ui,
        &aox::tf!("{} · {} msgs", s.time, s.messages),
        theme::body_font(10.5),
        P::text_faint(),
        max_w,
    );
    let painter = ui.painter();
    // A galley is positioned by its top-left, so half its own height is subtracted here to get the
    // "vertically centred on its line" placement. Two lines at 15 and 32 in a 48-high row: the block
    // reads as centred without either line touching the fill's edge.
    painter.galley(Pos2::new(x, rect.top() + 16.0 - title.size().y * 0.5), title, title_color);
    painter.galley(Pos2::new(x, rect.top() + 33.0 - sub.size().y * 0.5), sub, P::text_faint());

    // ── the "⋯" ──
    // Drawn on hover and on the current row, and never at rest: twenty rows each wearing three dots
    // would read as a column of buttons rather than a list of titles. Three 2px dots, hand-drawn --
    // a font's "…" sits on the baseline and would be off-centre against the title's optical middle.
    let show_more = hov || current;
    let more_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 6.0 - more_w * 0.5, rect.top() + 16.0),
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
            ui.painter().rect_filled(more_rect, CornerRadius::same(6), P::active_fill());
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
    /// The workspace drawer: a **resident left column that pushes the transcript aside**.
    ///
    /// It used to be a card floating over the transcript. That read as a pop-up, and a pop-up is
    /// something you dismiss; this is a place you work from, so it takes part in the layout instead:
    /// `SidePanel::left` shrinks the central column by exactly its width and gives it back when the
    /// drawer is put away. Nothing is covered, no scrim is needed, and the transcript is never
    /// drawn underneath a panel it cannot be read through.
    ///
    /// # How the push is done
    ///
    /// The **panel width** is what animates, so the eased value *is* the push: at any frame the
    /// transcript has given up exactly as much room as the drawer occupies. The content is laid out
    /// to that same width rather than at a fixed 300, which means it squeezes on the way in exactly
    /// like a sidebar being dragged wider — the alternative (fixed-width content riding the panel's
    /// edge) would let the content's min-rect overflow the panel, and egui would then hand the
    /// central column the *content's* width instead of the panel's, killing the animation.
    ///
    /// The horizontal inset eases with the width for the same reason: a 12px margin inside a 3px
    /// panel is a negative content rect.
    ///
    /// # Where the closing affordances went
    ///
    /// There is no "click outside" and no Esc-to-close any more. A resident column that disappears
    /// because the pointer landed on the transcript is a column you stop trusting; the drawer's head
    /// carries the one control that puts it away ([`sidebar_button`]), and Esc belongs to the
    /// search box (it clears the query).
    pub(super) fn drawer(&mut self, ctx: &egui::Context) {
        if self.hist_t <= 0.0 {
            // Fully closed: drop the query. It is done here rather than at each door that closes
            // the drawer (the collapse button, a conversation switch) because there is exactly one
            // state that means "not showing", and a stale filter would greet the next open with a
            // list that looks like it lost its rows.
            self.hist_query.clear();
            return;
        }
        let ease = ease_out_cubic(self.hist_t);
        // Clicks are refused while a switch request is in flight or a turn is running: a switch
        // started during either of those takes effect only later in the queue, and all the
        // clicker sees is the UI "hesitating before it jumps".
        let locked = self.busy || self.switching;

        // ── geometry: a column that takes part in the layout ──
        //
        // The eased width **is** the push: whatever the drawer takes, the transcript gives up, and
        // both numbers come from the same expression. A panel narrower than its own margin would
        // hand its content a negative rect, so the inset eases with it instead of being a constant.
        let w = (HISTORY_W * ease).max(1.0);
        let inset = (12.0 * ease).round() as i8;
        let pw = (w - 2.0 * inset as f32).max(8.0);
        // The window's height comes from **the largest of two independent reports**, because they
        // can disagree: `content_rect` is what the rest of this file uses for coordinates, but on a
        // window whose DPI the backend reports differently it can come back at a fraction of the
        // real client area. Taking the max is safe in both directions: the value only ever bounds
        // a height.
        //
        // ⚠️ Compare **heights**, not `bottom()`s: `viewport().inner_rect` is in *screen*
        // coordinates (its `min` is the window's position on the desktop), so its `bottom()` is
        // where the window ends on the **monitor**, not how tall it is.
        let win_h = ctx.content_rect().height().max(
            ctx.input(|i| i.viewport().inner_rect.map_or(0.0, |r| r.height())),
        );
        // What the scrolling list gets: the window, minus the head and the foot, minus the frame's
        // bottom margin. Both constants are plain sums of what is drawn (see their docs), because
        // the drawer places every gap by hand instead of letting `item_spacing` do it.
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
        // Group folds requested this frame. Collected rather than applied in place: the tree is
        // drawn inside the `Area` closure, so `&mut self` edits have to wait until after it returns
        // (the same shape `open` already used for switching).
        let mut toggles: Vec<String> = Vec::new();
        // Row / header requests, collected for the same reason as `toggles`. One slot per
        // destination rather than one enum because each lands somewhere different afterwards (a
        // message to the agent / a registry edit / a shell open / a dialog), and a single click can
        // only ever produce one of them.
        let mut recycle: Vec<PathBuf> = Vec::new();
        let mut forget_ws: Option<PathBuf> = None;
        let mut reveal_ws: Option<PathBuf> = None;
        let mut rename_ws: Option<PathBuf> = None;
        // "Start a conversation in this workspace" -- fed by the header's hover "＋" and by the
        // workspace menu's first row. One slot for both because they are one action with two doors.
        let mut new_conv_ws: Option<PathBuf> = None;
        // "Rename this conversation", as `(id, the title currently shown)`. The second half is what
        // pre-fills the dialog, and it carries the **title** rather than a path because a conversation
        // rename never touches the filesystem (see `NameDialogMode::RenameConversation`).
        let mut rename_conv: Option<(String, String)> = None;
        // "Re-file this conversation into that group", as `(conversation id, group id)`. The group id
        // is the id the tree drew -- possibly an `auto:` one -- and the lookup back to its root happens
        // in `move_conversation`, which has the drawn tree in hand.
        let mut move_conv: Option<(String, String)> = None;

        let panel = egui::SidePanel::left("aox-drawer")
            // Not resizable: the width is the animation's own output, and a drag handle would be
            // fighting it for the same number.
            .resizable(false)
            .exact_width(w)
            // The drawer's only chrome is the hairline drawn below. The built-in separator is the
            // same line in a theme colour that is not this one, and it would also be drawn for a
            // panel that is 3px wide mid-animation.
            .show_separator_line(false)
            .frame(
                Frame::NONE
                    .fill(P::surface())
                    .inner_margin(Margin { left: inset, right: inset, top: 12, bottom: 12 }),
            )
            .show(ctx, |ui| {
                ui.set_width(pw);
                // Every gap in this panel is placed by hand. `item_spacing` is a theme-wide 10, and
                // a column this dense cannot afford ten unaccounted pixels between every widget --
                // it is also what lets `DRAWER_HEAD` / `DRAWER_FOOT` be exact sums instead of
                // "measured, and re-measured whenever a row moves".
                ui.spacing_mut().item_spacing = Vec2::ZERO;

                // ── head: collapse, then settings at the far end ──
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
                        // The empty gap between the two icons is the window's drag handle. The
                        // title bar (`App::titlebar`) starts at this column's right edge, so
                        // without this the top-left corner of the window would be the one patch
                        // you cannot move it by — and it reads as the bar, so it has to behave
                        // like the bar: drag to move, double-click to maximize.
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

                // Is there anything in the tree other than the two built-ins? A registered
                // workspace counts as "something" **even while it is empty** -- that is exactly
                // the state the "new workspace" button leaves behind -- so the friendly
                // first-run screen below is reserved for a genuinely empty install.
                let custom_group = self.hist_groups.iter().any(|g| g.builtin.is_none());

                // `auto_shrink([false, false])` is the load-bearing part: the scroll area
                // takes the height we decided instead of collapsing back onto its content,
                // which is what stops the panel resizing itself whenever a group is folded.
                //
                // `min_scrolled_height` is the *other* half, and it is not redundant: a
                // `ScrollArea` sizes itself from `ui.available_rect_before_wrap()`, so
                // "take all available space" is only as good as that number. Forcing the
                // inner height is what actually makes `list_h` real.
                ScrollArea::vertical()
                    .max_height(list_h)
                    .min_scrolled_height(list_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Rows sit 3 apart; the tree adds its own gaps between groups.
                        ui.spacing_mut().item_spacing.y = 3.0;
                        // The empty state is drawn **inside** the scroll area rather than
                        // short-circuiting the panel. It has to be: "no conversations yet" is
                        // not the same statement as "no workspaces yet", and returning early
                        // here would hide the very group a workspace just created shows up as
                        // -- and the button that made it, which sits below the list.
                        if self.history.is_empty() && !custom_group {
                            ui.label(
                                RichText::new(
                                    aox::i18n::t("No history yet.\nAfter your first message, the conversation is saved here automatically."),
                                )
                                .color(P::text_faint())
                                .size(12.5),
                            );
                            ui.add_space(10.0);
                            // A directory path can be long; use truncate so it closes itself
                            // off within the panel width instead of being cut by the panel
                            // boundary.
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
                            // Said out loud rather than drawn as an empty column: with a
                            // query in the box, "nothing here" and "the list is broken"
                            // look identical otherwise.
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
                                &mut recycle,
                                &mut forget_ws,
                                &mut reveal_ws,
                                &mut rename_ws,
                                &mut new_conv_ws,
                                &mut rename_conv,
                                &mut move_conv,
                            );
                        }
                    });

                // The foot: the actions that are not about a row. Pinned outside the
                // scroll area, so they cannot be scrolled away by a workspace with eighty
                // conversations in it.
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
                // ...whereas opening the folder touches nothing at all, so this row stays
                // live during a turn. Disabled only when the app root is unknown -- which is
                // the one case with no folder to name and no path to hand the shell.
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

                // Resize hot-zones for the parts of the window's border that live in this column:
                // the whole left edge and the bottom-left corner (the drawer is a full-height
                // column, and the top row is an `Area` that stops at this column's right edge).
                // `ui.interact` is clipped to the panel's rect, so this call and the two in
                // `titlebar` / the central panel add up to exactly the eight zones -- see
                // `titlebar`.
                gwin::resize_handles(ui, ctx, ctx.content_rect());
            });
        // The drawer's only chrome: one hairline down the full height, painted from the panel's own
        // rect (so it lands on the moving edge for the whole animation) and **after** the content,
        // so nothing can draw over it.
        let r = panel.response.rect;
        ctx.layer_painter(panel.response.layer_id)
            .vline(r.right() - 0.5, r.top()..=r.bottom(), Stroke::new(1.0, P::border()));

        // Esc belongs to the search box: with a query typed, "get me out of this" means "show me
        // everything again". It deliberately does **not** close the drawer -- a resident column that
        // vanishes on a stray Esc is a column you stop trusting, and the head has the one control
        // that puts it away on purpose.
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
            self.forget_workspace(&root);
        }
        if let Some(root) = rename_ws {
            // Opens the dialog; nothing is written until the name is confirmed. Unlike the two above
            // this is not an edit at all yet, which is why it is here rather than among them.
            self.start_rename_workspace(&root);
        }
        if let Some((id, current)) = rename_conv {
            // Same shape as the workspace rename: the dialog is opened, and nothing is written until
            // "Save". The pre-fill is the title the row was showing, so an empty field visibly means
            // "back to the first message".
            self.start_rename_conversation(&id, current);
        }
        if let Some((conv, target)) = move_conv {
            // A registry edit, done here (after the `Area` closure) because it needs `&mut self`.
            // Unlike the rename above it writes immediately -- moving a row has no dialog to confirm.
            self.move_conversation(&conv, &target);
        }
        if let Some(root) = new_conv_ws {
            // New conversation **bound to that workspace** -- two messages down the queue, in an order
            // that matters (see `new_conversation_in`). The panel stays open: the new thread is empty
            // and its row only appears once something is written, so closing the panel here would make
            // the click look like it did nothing.
            self.new_conversation_in(&root);
        }
        if !recycle.is_empty() {
            // Handed to the agent thread rather than recycled here: the move is a blocking shell
            // call, and it also keeps one code path for "a conversation was deleted" -- the toast
            // and the list refresh then come back through the same `AgentEvent::Recycled` the
            // model's own deletions use.
            let _ = self.tx.send(ToAgent::Recycle(recycle));
        }
        if want_new_ws {
            // Just opens the dialog. The folder is not touched until the user has named it and hit
            // "Create" -- this is the one action in the panel that writes to disk, so it gets a step
            // of its own rather than happening under the pointer.
            self.name_dialog = Some(NameDialog::new());
        }
        if want_open_ws {
            if let Some(root) = app_root.as_deref() {
                let dir = root.join(aox::workplace::WORKPLACE_DIR);
                // Made if it is not there yet: `ensure_started` normally creates it, but the panel can
                // be opened before the agent thread has run, and handing `explorer` a path that does
                // not exist opens nothing at all -- a button that silently does nothing.
                let _ = std::fs::create_dir_all(&dir);
                reveal_in_file_manager(&dir);
            }
        }
        if want_new_conv {
            // Same path the toolbar's "+" used to take. Refused while a turn runs for the same
            // reason `new_chat_row` is drawn disabled then: the switch would only land after the
            // turn, and the click would look like it did nothing.
            self.new_conversation();
        }
        if want_settings {
            self.settings_open = true;
        }
        if want_close {
            // Closing the drawer with its own × is a choice, not an accident, so it is written down
            // like the toolbar toggle: the next launch comes back folded if that is where you left
            // it (see the fold-state load in `refresh_history`).
            self.show_history = false;
            self.save_fold_state();
        }
        if let Some(id) = open {
            self.switch_history(&id);
        }
    }

    /// Draw the workspace-grouped conversation tree inside the drawer's scroll area.
    ///
    /// Takes `&self` -- the drawer's `SidePanel` closure only holds an immutable borrow -- and hands
    /// everything it wants to change back to the caller: `open` (switch to this conversation),
    /// `toggles` (fold / unfold this group), and the seven right-click requests `recycle` /
    /// `forget_ws` / `reveal_ws` / `rename_ws` / `new_conv_ws` / `rename_conv` / `move_conv` (delete
    /// this conversation / drop this workspace / open its folder / rename it / start a conversation in
    /// an existing workspace / rename a conversation / re-file a conversation into another group).
    ///
    /// The groups arrive from [`aox::space::group`] in a **fixed order** (Default, Full access, then
    /// the user's workspaces, most recently active first). Exactly one adjustment is made on top:
    /// the workspace holding the current conversation is pulled to the very top, so the place you
    /// are in is the first thing in the panel -- the tree's counterpart of the old flat list pinning
    /// the current row. It is applied as an index order, so the groups themselves are never touched.
    #[allow(clippy::too_many_arguments)] // one `&mut` slot per thing the tree can ask for; see above
    pub(super) fn hist_tree(
        &self,
        ui: &mut Ui,
        width: f32,
        locked: bool,
        query: &str,
        open: &mut Option<String>,
        toggles: &mut Vec<String>,
        recycle: &mut Vec<PathBuf>,
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
        // With a query in the box the tree is **flattened open**. Matching a conversation that sits
        // inside a folded workspace and then showing nothing would be a filter that hides its own
        // results; the fold state is left alone on disk, it just does not apply while filtering.
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
            // The header's own "＋" and its menu row say the same thing, so they feed the same slot.
            // A group with no root has no "＋" (see `history_group_header`), so the root here is a
            // formality -- but it is still read from `g` rather than assumed.
            if plus {
                if let Some(root) = g.root.clone() {
                    *new_conv_ws = Some(root);
                }
            }
            // Only a real folder gets a menu. `Default` and `Full access` have no root of their own
            // (and are not the user's to remove), so neither "new conversation", "rename", "open in
            // the file manager" nor "remove from AOX" has anything to act on -- a menu whose every
            // row is dead is worse than no menu at all.
            if let Some(root) = g.root.clone() {
                head.context_menu(|ui| {
                    // First row, because it is the one thing this menu can do that the header cannot
                    // do without hovering: make a conversation here. Not gated on `locked` -- the
                    // click goes down the same queue as the "＋", which refuses a switch while a turn
                    // runs by doing nothing, and graying the row out here would only be a second,
                    // silent copy of that rule.
                    if menu_item(ui, aox::i18n::t("New conversation"), MENU_W, true, false).clicked() {
                        *new_conv_ws = Some(root.clone());
                        ui.close();
                    }
                    // Renaming here moves **the label, never the folder**: the conversations record
                    // the folder they were written to, so a folder move would strand every one of
                    // them under a root the registry no longer knows (see
                    // `Registry::rename_workspace`). The dialog says which part is being renamed --
                    // a menu row that has to explain itself is a menu row nobody reads.
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
                    // Deliberately not styled as a danger action and not confirmed: nothing is
                    // deleted. The folder, its conversations and everything inside stay exactly
                    // where they are; this only drops the row from the tree, and re-adding the
                    // folder is a working undo (see `Registry`'s `hidden` mark).
                    if menu_item(ui, aox::i18n::t("Remove from AOX"), MENU_W, true, false).clicked() {
                        *forget_ws = Some(root.clone());
                        ui.close();
                    }
                });
            }
            if !unfolded {
                continue;
            }
            // The "Move to" targets: every drawn group except this one. Built once per group rather
            // than per row, since the list is identical for every row inside it. It is a pair of
            // (group id, display name): the id is what the move records, the name is what the menu
            // shows -- and the name is never re-derived from the id, because for `Default`/`Full
            // access` and for a renamed workspace they have nothing to do with each other.
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
                // A row's menu is offered **even while a turn runs**: deleting an old conversation
                // touches nothing the running turn holds (the move happens on the agent thread).
                // Only *switching* has to wait for the turn, and that is the `locked` gate above.
                //
                // Two doors onto **one** menu: the row's right-click, and the "⋯" that appears on
                // hover. The contents live in `conversation_menu` rather than in a closure so both
                // doors can hand it the same `&mut` slots (a closure could only be consumed once).
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
                        recycle,
                    );
                };
                row.context_menu(&mut menu);
                // The "⋯" only exists on a hovered (or current) row, so this popup can only ever be
                // opened from a visible affordance -- but it is registered every frame regardless,
                // because a popup that is only *created* on the frame of the click has no id to
                // remember being open by.
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
            // Nothing is on screen yet (a brand-new conversation that has not reached disk), so
            // there is no "where you are" to point at. Unfold everything rather than greet the user
            // with a wall of headers and nothing underneath.
            None => true,
        }
    }

    /// Fold / unfold a workspace group.
    ///
    /// Flips relative to **what is on screen right now** rather than inserting into a chosen set:
    /// the effective state is the two explicit sets plus the default, so a caller holding only the
    /// id would have to re-derive all three just to know which set to write to.
    pub(super) fn toggle_group(&mut self, id: &str) {
        let now = self.group_is_open(id);
        self.hist_open.remove(id);
        self.hist_closed.remove(id);
        if now {
            self.hist_closed.insert(id.to_string());
        } else {
            self.hist_open.insert(id.to_string());
        }
        // Persisted right away: a fold is a deliberate click, it is cheap to write (one tiny file),
        // and deferring it would mean choosing a moment to flush that always turns out to be the
        // wrong one (the app being killed is exactly when nothing gets flushed).
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

