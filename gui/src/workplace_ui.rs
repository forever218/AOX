//! Workspace button: the "Workspace · <tier>" chip at the **bottom-left** of the input box.
//!
//! The three tiers (default / choose workspace / full access) enter through **this UI layer** here. The
//! `[workplace] mode` in config only fixes the tier **at launch** — the factory default is `default` (restricted;
//! switched from `full` on 2026-10-05 because the user wants "default restricted every launch"); to change tiers at runtime you use this
//! button. Switching only affects **the current conversation**: the tier and root travel with the conversation save, while `/new` always returns to "default".
//!
//! This module owns four things:
//! - [`button`]: the button itself (hand-drawn folder icon + current tier name);
//! - [`menu`]: the three-choice list that pops **upward** on click (current tier checked, "full access" shown red and bold);
//! - [`confirm`]: the red second confirmation before switching to "full access" — the **only** toggle in the whole
//!   program that **crosses every protection once clicked** (so only it uses [`P::danger`]);
//! - [`picker`]: the built-in directory picker.
//!
//! # Three conventions
//!
//! **① State lives in [`WorkplaceUi`], held by `App`.** This module never touches any field of `App`.
//!
//! **② "Take parameters in, emit requests out."** Each function returning `Option<Mode>` means "the UI wants to send a request to
//! the Agent thread, requesting that the tier be switched to this". Why the UI cannot switch tiers itself: `Workplace` lives inside
//! `aox::runtime::Runtime`, and only the Agent thread can reach `Runtime` — the main thread holds only a
//! channel (see `ToAgent::Workplace` in `gui/src/bridge.rs`).
//!
//! **③ The current state is "pushed in", not "asked for".** The tier name and checkmarks come from the `WorkplaceView` received by
//! [`WorkplaceUi::set_view`] (the bridge re-sends it at startup / after each turn / after every tier switch). The UI keeping
//! its own "which tier is selected now" is wrong: the authoritative state is on the Agent thread, and two copies will inevitably drift.
//!
//! # Why the directory picker is hand-drawn
//!
//! We do not call the system directory dialog (on Windows that is `IFileDialog`, which pulls in extra COM + a pile of
//! `windows-sys` interfaces, and you would have to write another copy for macOS / Linux). This picker has a narrow purpose —
//! pick a "workspace root" directory, no creation, no multi-select, no network locations — so a single egui-drawn screen
//! suffices, and it stays **fully on-theme** instead of popping a white system dialog over the paper-and-ink UI.

use std::path::{Path, PathBuf};

use egui::{
    Align2, Color32, CornerRadius, Frame, Id, Margin, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2,
};

use aox::workplace::Mode;

use crate::app::{danger_button, flat_button, primary_button};
use crate::bridge::WorkplaceView;
use crate::theme::{self, P};

// ─────────────────────────── State ───────────────────────────

/// All state for the workspace button plus the panels it pops up.
pub struct WorkplaceUi {
    /// Current tier and root (pushed in from the bridge `FromAgent::Workplace`).
    ///
    /// `None` = no receipt received yet (the first frame or two after launch, or never managed to connect to the Agent thread).
    /// In that case the button shows the plain "Workspace" and no list item is checked — we do not guess.
    view: Option<WorkplaceView>,
    /// Whether the three-choice list is expanded.
    open: bool,
    /// The button rectangle. Pop-up panels anchor to it **upward** (see the coordinate math in [`menu`]).
    button_rect: Option<Rect>,
    /// Whether the "full access" second confirmation is open.
    confirm_full: bool,
    /// Whether the directory picker is open.
    picker: Option<Picker>,
}

impl WorkplaceUi {
    pub fn new() -> Self {
        Self { view: None, open: false, button_rect: None, confirm_full: false, picker: None }
    }

    /// Receive a "current state". Called by `App::pump` in the `FromAgent::Workplace` branch.
    pub fn set_view(&mut self, v: WorkplaceView) {
        self.view = Some(v);
    }

    /// Read-only peek at the current state. The settings page "System" section needs to show "where this conversation workspace is" —
    /// that is the only place it needs this field, so we expose a read-only accessor instead of opening up the field.
    pub fn view(&self) -> Option<&WorkplaceView> {
        self.view.as_ref()
    }

    /// The text on the button.
    ///
    /// The "choose workspace" tier does not spell out its tier name (that would read "Workspace · Choose workspace", stuttering); instead it
    /// shows **the selected directory name** — what the user cares about is "which patch of ground am I locked into right now",
    /// and the full path lives in the pop-up list.
    ///
    /// The "default" tier also **carries the directory name once the ground is built** (`Workspace · Default · 20261005162915`): default is
    /// **one fixed patch for the conversation** (switch away and back and it is still the same one; see `aox::workplace::State::own`),
    /// and just writing "default" hides which patch it is, so you cannot confirm it is not quietly accumulating patch by patch (raised by the user on 2026-10-05).
    fn chip_text(&self) -> String {
        match &self.view {
            Some(v) => match &v.mode {
                Mode::Default => match &v.root {
                    Some(p) => aox::tf!("Workspace · Default · {}", dir_name(p)),
                    None => aox::i18n::t("Workspace · Default").to_string(),
                },
                Mode::Fixed(p) => aox::tf!("Workspace · {}", dir_name(p)),
                Mode::Full => aox::i18n::t("Workspace · Full access").to_string(),
            },
            None => aox::i18n::t("Workspace").to_string(),
        }
    }
}

impl Default for WorkplaceUi {
    fn default() -> Self {
        Self::new()
    }
}

/// State for the directory picker.
struct Picker {
    /// The directory currently being browsed.
    dir: PathBuf,
    /// Which drive letters exist on this machine (Windows only; empty elsewhere — just walk in from `/`).
    drives: Vec<char>,
    /// The last directory-read error (permission denied, etc.).
    error: Option<String>,
    /// Subdirectories under the current directory. **Cached**, only re-read on open / directory switch —
    /// otherwise `read_dir` would run every frame and a 60fps UI would pointlessly hit the disk 60 times per second.
    subdirs: Vec<PathBuf>,
}

impl Picker {
    fn open(dir: PathBuf) -> Self {
        let mut p = Self { dir, drives: Vec::new(), error: None, subdirs: Vec::new() };
        p.drives = existing_drives();
        p.reload();
        p
    }

    fn reload(&mut self) {
        self.subdirs.clear();
        self.error = None;
        match std::fs::read_dir(&self.dir) {
            Ok(rd) => {
                for entry in rd.flatten() {
                    let path = entry.path();
                    // Only directories: this picker is choosing "a patch of ground", files are noise.
                    // Also skip `.` and `..` (read_dir does not hand them out anyway, but do not rely on it).
                    if path.is_dir() {
                        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                            if name.starts_with('.') {
                                continue;
                            }
                        }
                        self.subdirs.push(path);
                    }
                }
                // Case-insensitive sort by name: Windows `read_dir` order is whatever the filesystem returns,
                // which looks scrambled.
                self.subdirs.sort_by(|a, b| {
                    let (a, b) = (dir_name(a).to_lowercase(), dir_name(b).to_lowercase());
                    a.cmp(&b).then_with(|| a.cmp(&b))
                });
            }
            Err(e) => self.error = Some(format!("{e}")),
        }
    }

    fn goto(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.reload();
    }
}

// ─────────────────────────── Button ───────────────────────────

/// Draw the button (at the bottom-left of the input box).
///
/// It does only two things: paint the current tier name, and record the hot-rect into `st.button_rect` (the pop-up panel uses it for positioning).
/// The click flips `st.open` in place here — the panel itself is drawn by [`menu`] (it needs an independent `Area`
/// and cannot be trapped inside the input card clip region).
pub fn button(ui: &mut Ui, st: &mut WorkplaceUi, enabled: bool) {
    let text = st.chip_text();
    let font = egui::FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(text, font, Color32::PLACEHOLDER);

    let pad = Vec2::new(8.0, 4.0);
    let icon = 14.0;
    let size = Vec2::new(icon + 6.0 + galley.size().x + pad.x * 2.0, galley.size().y.max(icon) + pad.y * 2.0);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);

    let hov = enabled && resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    let radius = CornerRadius::same(8);
    painter.rect_filled(
        rect,
        radius,
        if hov { P::surface_hi() } else { Color32::TRANSPARENT },
    );
    // Only stroke on hover: normally it sits inside the input card, and an extra border would make the bottom-left corner noisy.
    if hov {
        painter.rect_stroke(rect, radius, Stroke::new(1.0, P::border()), egui::StrokeKind::Inside);
    }

    let fg = if enabled { P::text_dim() } else { P::text_faint() };
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + pad.x + icon * 0.5, rect.center().y),
        Vec2::new(icon, icon * 0.82),
    );
    paint_folder(painter, icon_rect, fg);
    painter.galley(Pos2::new(rect.left() + pad.x + icon + 6.0, rect.center().y - galley.size().y * 0.5), galley, fg);

    if resp.clicked() && enabled {
        st.open = !st.open;
        // When opening the list, collapse the other two panels: nobody can stand three boxes popping up at once.
        st.confirm_full = false;
        st.picker = None;
    }
    st.button_rect = Some(rect);
}

// ─────────────────────────── Three-choice list ───────────────────────────

/// Draw the upward-popping three-choice list (does nothing when `st.open` is false).
pub fn menu(ctx: &egui::Context, st: &mut WorkplaceUi) -> Option<Mode> {
    if !st.open {
        return None;
    }
    let Some(btn) = st.button_rect else {
        st.open = false;
        return None;
    };

    // Anchor at the button top-left corner, 6px further up; combined with `pivot(LEFT_BOTTOM)` that means "panel bottom edge sits on the button top edge"
    // — so it grows upward and will not be clipped by the window bottom edge.
    //
    // WARNING: **do not apply any coordinate conversion.** The rects from `Area::fixed_pos` and `ui.allocate_*` live in
    // **the same coordinate space** (client-area local, origin at the client area top-left) — egui itself uses `ctx.content_rect()` (= the client rect)
    // as the default constraint rect inside `Area::begin`. Earlier here we added a "viewport origin" (`viewport().inner_rect.left_top()`)
    // following the top-bar token-pill code, but that is
    // **screen (monitor) coordinates**, and it is non-zero whenever the window is not at the monitor origin — on real hardware the panel ended up
    // shifted by ~(154, 14) logical pixels.
    let anchor = btn.left_top() - Vec2::new(0.0, 6.0);

    let mut req = None;
    let cur = st.view.as_ref().map(|v| v.mode.clone());

    let area = egui::Area::new(Id::new("aox-workplace-menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor)
        .pivot(Align2::LEFT_BOTTOM)
        .show(ctx, |ui| {
            Frame::NONE
                .fill(P::surface())
                .corner_radius(CornerRadius::same(10))
                .stroke(Stroke::new(1.0, P::border()))
                .shadow(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: P::shadow() })
                .inner_margin(Margin::symmetric(6, 6))
                .show(ui, |ui| {
                    ui.set_width(276.0);

                    // ① Default
                    if row(
                        ui,
                        aox::i18n::t("Default"),
                        aox::i18n::t("each new conversation starts a fresh plot under workplace/"),
                        matches!(cur, Some(Mode::Default)),
                        false,
                    )
                    .clicked()
                    {
                        req = Some(Mode::Default);
                    }

                    // ② Choose workspace… (opens the directory picker; not itself a tier switch)
                    //
                    // No hint line (user asked on 2026-10-04 to "keep only the default hint"): which directory is currently selected is already written on the button as "Workspace · <dir name>",
                    // which directory is currently selected is already written on the button as "Workspace · <dir name>",
                    // and repeating it in the menu would only inflate it.
                    let fixed_now = matches!(cur, Some(Mode::Fixed(_)));
                    if row(ui, aox::i18n::t("Choose workspace…"), "", fixed_now, false).clicked() {
                        st.open = false;
                        // Open on the current root when already a fixed tier, otherwise on the app root.
                        let start = st
                            .view
                            .as_ref()
                            .and_then(|v| v.root.clone())
                            .unwrap_or_else(|| aox::paths::app_root());
                        st.picker = Some(Picker::open(start));
                    }

                    // ③ Full access (the only dangerous item: red + bold)
                    //    Also no hint line — the red and bold already serve as the hint here.
                    if row(ui, aox::i18n::t("Full access"), "", matches!(cur, Some(Mode::Full)), true).clicked() {
                        st.open = false;
                        st.confirm_full = true;
                    }
                });
        })
        .response;

    // Click outside the panel, or press Esc to collapse.
    // (Same coordinate space as above, so we can compare directly — no origin to subtract.)
    let panel = area.rect;
    let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    let clicked_outside = ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .is_some_and(|p| !panel.contains(p) && !btn.contains(p))
    });
    if esc || clicked_outside {
        st.open = false;
    }
    if req.is_some() {
        st.open = false;
    }
    req
}

/// One row in the list: a check slot on the left + a title + a small hint line.
///
/// The hand-drawn reason is the same as `text_button` in `app.rs`: **paint the base first, then the text**, so on hover color change we
/// do not re-layout the text and we avoid the old problem of "the base color covering the text".
fn row(ui: &mut Ui, label: &str, hint: &str, checked: bool, danger: bool) -> egui::Response {
    let width = ui.available_width();
    // Two heights: two-line (title + hint) and one-line (title only). In the menu **only "default" still carries a hint**
    // (user asked on 2026-10-04 to drop the hint text on the other two). A single-line row laid out at the old 46px height
    // would leave its text floating in the top half, looking unfinished.
    let two_line = !hint.is_empty();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, if two_line { 46.0 } else { 32.0 }), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(8), P::surface_hi());
    }

    // Check: drawn only for the current tier. The slot is reserved so unchecked rows do not shift the title left.
    if checked {
        let color = if danger { P::danger() } else { P::ink() };
        paint_check(painter, Pos2::new(rect.left() + 17.0, rect.center().y), 5.0, color);
    }

    let text_x = rect.left() + 34.0;
    let label_color = if danger { P::danger() } else { P::text() };
    let label_font = if danger {
        theme::bold_font(15.0)
    } else {
        egui::FontId::proportional(15.0)
    };
    let g = painter.layout_no_wrap(label.to_owned(), label_font, label_color);
    // On two-line rows the title sits high (leaving room for the hint below); on one-line rows it is vertically centered.
    let label_y = if two_line { rect.top() + 7.0 } else { rect.center().y - g.size().y * 0.5 };
    painter.galley(Pos2::new(text_x, label_y), g, label_color);

    if two_line {
        let hg = painter.layout_no_wrap(hint.to_owned(), egui::FontId::proportional(12.0), P::text_faint());
        painter.galley(Pos2::new(text_x, rect.top() + 27.0), hg, P::text_faint());
    }

    resp
}

/// One subdirectory row in the directory picker: hand-drawn folder icon + name, whole row clickable.
///
/// No emoji (glyphs like the folder emoji depend on whether the system font has them; missing, they render as tofu boxes),
/// reusing the [`paint_folder`] from the button — the whole icon vocabulary has a single implementation.
fn dir_row(ui: &mut Ui, name: &str) -> egui::Response {
    let width = ui.available_width();
    const HEIGHT: f32 = 26.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, HEIGHT), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(6), P::surface_hi());
    }
    let icon = Rect::from_center_size(Pos2::new(rect.left() + 13.0, rect.center().y), Vec2::new(13.0, 11.0));
    paint_folder(painter, icon, P::text_faint());
    let g = painter.layout_no_wrap(name.to_owned(), egui::FontId::proportional(13.5), P::text_dim());
    painter.galley(
        Pos2::new(rect.left() + 28.0, rect.center().y - g.size().y * 0.5),
        g,
        P::text_dim(),
    );
    resp
}

// ─────────────────────────── Full access: second confirmation ───────────────────────────

/// The red confirmation box shown before switching to "full access".
pub fn confirm(ctx: &egui::Context, st: &mut WorkplaceUi) -> Option<Mode> {
    if !st.confirm_full {
        return None;
    }
    let mut req = None;
    let mut keep = true;

    let resp = egui::Modal::new(Id::new("aox-workplace-confirm"))
        .frame(
            Frame::NONE
                .fill(P::surface())
                .corner_radius(CornerRadius::same(theme::card_radius()))
                .stroke(Stroke::new(1.0, P::border()))
                .inner_margin(Margin::symmetric(26, 22)),
        )
        .show(ctx, |ui| {
            ui.set_width(430.0);
            // Title in danger color + bold: this screen is meant to make people pause.
            ui.label(RichText::new(aox::i18n::t("Switch to \"Full access\"?")).color(P::danger()).size(17.0).strong());
            ui.add_space(10.0);
            ui.label(
                RichText::new(aox::i18n::t("AOX will be able to read and write any file on the device; the workspace boundary will no longer stop it."))
                    .color(P::text_dim())
                    .size(14.0),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(aox::i18n::t("Pandora's box is opening... though that is not always a bad thing"))
                    .color(P::text_faint())
                    .size(13.0),
            );
            ui.add_space(20.0);
            ui.horizontal(|ui| {
                if danger_button(ui, aox::i18n::t("Confirm switch")).clicked() {
                    req = Some(Mode::Full);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if flat_button(ui, aox::i18n::t("Cancel")).clicked() {
                        keep = false;
                    }
                });
            });
        });

    // Click background / Esc = cancel (`should_close` is the standard exit egui::Modal provides).
    if resp.should_close() {
        keep = false;
    }
    if req.is_some() || !keep {
        st.confirm_full = false;
    }
    req
}

// ─────────────────────────── Directory picker ───────────────────────────

/// Built-in directory picker. On selection returns `Set(Mode::Fixed(<dir>))` — the same landing point as the CLI
/// `/workplace <dir>` (both go through `Workplace::set_mode`).
///
/// Here we **do not pick files, do not create directories**: this entry has one purpose only — pick a "workspace root".
/// Users who need a new directory can make it in the file manager first and then come back to pick it, which is cleaner than stuffing a "create" entry in here.
pub fn picker(ctx: &egui::Context, st: &mut WorkplaceUi) -> Option<Mode> {
    let Some(p) = st.picker.as_mut() else {
        return None;
    };
    let mut chosen: Option<PathBuf> = None;
    let mut close = false;

    let resp = egui::Modal::new(Id::new("aox-workplace-picker"))
        .frame(
            Frame::NONE
                .fill(P::surface())
                .corner_radius(CornerRadius::same(theme::card_radius()))
                .stroke(Stroke::new(1.0, P::border()))
                .inner_margin(Margin::symmetric(22, 18)),
        )
        .show(ctx, |ui| {
            ui.set_width(520.0);

            ui.horizontal(|ui| {
                ui.label(RichText::new(aox::i18n::t("Choose a workspace directory")).color(P::text()).size(16.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if flat_button(ui, aox::i18n::t("Cancel")).clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(12.0);

            // Current path: monospace + middle-elide when too long (keep the head drive letter and tail dir name — the two ends with the most information).
            Frame::NONE
                .fill(P::code_bg())
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(10, 7))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(elide_path(&p.dir.display().to_string(), 62))
                            .font(theme::mono_font(13.0))
                            .color(P::text_dim()),
                    );
                });

            // Drive letters (Windows): only list ones that actually exist; the 26 `is_dir` calls already read them once.
            if !p.drives.is_empty() {
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    for d in p.drives.clone() {
                        let here = p.dir.to_string_lossy().to_ascii_uppercase().starts_with(&format!("{d}:"));
                        let text = format!("{d}:");
                        let color = if here { P::ink() } else { P::text_dim() };
                        let resp = ui.add(
                            egui::Label::new(RichText::new(text).size(13.0).color(color))
                                .sense(Sense::click()),
                        );
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.clicked() {
                            p.goto(PathBuf::from(format!("{d}:\\")));
                        }
                        ui.add_space(4.0);
                    }
                });
            }

            ui.add_space(10.0);

            // Parent directory (use ASCII `..`; do not gamble on whether the font has an arrow glyph).
            if let Some(parent) = p.dir.parent().map(Path::to_path_buf) {
                let resp = ui.add(
                    egui::Label::new(RichText::new(aox::i18n::t("..  parent directory")).size(13.5).color(P::text_dim()))
                        .sense(Sense::click()),
                );
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    p.goto(parent);
                }
            } else {
                ui.label(RichText::new(aox::i18n::t("(already at the top level)")).size(13.0).color(P::text_faint()));
            }

            ui.add_space(6.0);

            // Subdirectory list
            if let Some(e) = &p.error {
                ui.label(RichText::new(aox::tf!("cannot read this directory: {}", e)).color(P::text()).size(13.5));
            } else if p.subdirs.is_empty() {
                ui.label(RichText::new(aox::i18n::t("(no subdirectories here)")).size(13.0).color(P::text_faint()));
            } else {
                let subs = p.subdirs.clone();
                let mut goto: Option<PathBuf> = None;
                egui::ScrollArea::vertical().max_height(248.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for sub in &subs {
                        if dir_row(ui, &dir_name(sub)).clicked() {
                            goto = Some(sub.clone());
                        }
                    }
                });
                if let Some(d) = goto {
                    p.goto(d);
                }
            }

            ui.add_space(18.0);
            // Button sits at the **bottom-right** (raised by the user on 2026-10-04): the `right_to_left` child layout consumes
            // the remaining horizontal width, so the control placed first ends up flush right; the hint text stays on the left of the same row.
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(aox::i18n::t("Once chosen, writes and deletes are confined to this directory (reads are unrestricted)."))
                        .color(P::text_faint())
                        .size(12.5),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary_button(ui, aox::i18n::t("Select this directory")).clicked() {
                        chosen = Some(p.dir.clone());
                    }
                });
            });
        });

    if resp.should_close() {
        close = true;
    }
    // `p` is no longer used past this point — we can mutate `st` directly below.
    if let Some(dir) = chosen {
        st.picker = None;
        return Some(Mode::Fixed(dir));
    }
    if close {
        st.picker = None;
    }
    None
}

// ─────────────────────────── Helpers ───────────────────────────

/// Display name of a directory (take the last segment; if none, fall back to the whole string minus the trailing separator).
fn dir_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Middle-elide an over-long path: keep the head (drive letter) and tail (current dir name) — the middle part does not help identification.
fn elide_path(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let head = keep / 3;
    let tail = keep - head;
    let h: String = chars[..head].iter().collect();
    let t: String = chars[chars.len() - tail..].iter().collect();
    format!("{h}…{t}")
}

/// Drive letters present on this machine (meaningful on Windows only).
fn existing_drives() -> Vec<char> {
    if !cfg!(windows) {
        return Vec::new();
    }
    ('A'..='Z').filter(|c| Path::new(&format!("{c}:\\")).is_dir()).collect()
}

/// Check mark.
fn paint_check(painter: &egui::Painter, center: Pos2, s: f32, color: Color32) {
    let stroke = Stroke::new(1.8, color);
    painter.line_segment(
        [Pos2::new(center.x - s, center.y + s * 0.05), Pos2::new(center.x - s * 0.2, center.y + s * 0.8)],
        stroke,
    );
    painter.line_segment(
        [Pos2::new(center.x - s * 0.2, center.y + s * 0.8), Pos2::new(center.x + s * 0.9, center.y - s * 0.75)],
        stroke,
    );
}

/// Folder icon: a "tab + body" solid silhouette.
///
/// Use **solid** rather than stroked: a stroked folder at 13px squares up into a mush on the tab inner horizontal line,
/// while a solid silhouette stays clean under both themes.
fn paint_folder(painter: &egui::Painter, rect: Rect, color: Color32) {
    let tab = Rect::from_min_max(
        Pos2::new(rect.left(), rect.top() + rect.height() * 0.10),
        Pos2::new(rect.left() + rect.width() * 0.46, rect.top() + rect.height() * 0.34),
    );
    let body = Rect::from_min_max(
        Pos2::new(rect.left(), rect.top() + rect.height() * 0.28),
        Pos2::new(rect.right(), rect.bottom()),
    );
    painter.rect_filled(tab, CornerRadius::same(1), color);
    painter.rect_filled(body, CornerRadius::same(2), color);
}
