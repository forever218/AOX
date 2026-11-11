//! Remote tab in Settings: pairing list, start/stop, logs. Bridge: own thread, own tokio runtime + Runtime.
//! Lock uses the bridge's heartbeat file, so it works with the page closed; layout shares Settings vocabulary.
//! Whole-row hover, text buttons, no border cards; native egui Buttons looked foreign -- geometry, not color.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use egui::{Align, CornerRadius, FontId, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use tokio::sync::mpsc::UnboundedReceiver;

use aox::i18n::t;
use aox::remote::store::Book;
use aox::remote::{self, RemoteEvent};

use crate::app::{
    button, button_ex, button_size, danger_button, elide, flat_button, paint_button, Weight, BTN_H,
};
use crate::model_ui::paint_check;
use crate::theme::{self, P};

/// Pairing row height (same as model ROW_H): 14pt name over 11.5pt subtitle, 11 padding, fixed for alignment.
const ROW_H: f32 = 56.0;

/// In-row action button height: one step below the row, 11 padding each side, not touching the edges.
const ACT_H: f32 = BTN_H.min(ROW_H - 12.0);

/// Left edge of the whole column; the same number is used for names, titles, empty states and forms.
const PAD_L: f32 = 16.0;

enum Act {
    Select(String),
    AskDelete(String),
    StartRename(String),
}

/// One in-row edit action, shared by inline rename and inline delete confirmation.
enum EditAct {
    None,
    Save,
    Cancel,
}

pub struct RemoteState {
    pub status: String,
    pub logs: Vec<String>,
    pub running: bool,
    busy: bool,
    stop: Arc<AtomicBool>,
    rx: Option<UnboundedReceiver<RemoteEvent>>,
    book: Book,

    adding: bool,
    server: String,
    code: String,
    handle: String,

    renaming: Option<(String, String)>,
    deleting: Option<String>,
    log_open: bool,
}

impl Default for RemoteState {
    fn default() -> Self {
        let book = Book::load(&aox::paths::app_root()).unwrap_or(Book {
            version: 2,
            active: None,
            pairings: Vec::new(),
        });
        Self {
            status: if book.pairings.is_empty() { "Not paired".into() } else { "Paired, not running".into() },
            logs: Vec::new(),
            running: false,
            busy: false,
            stop: Arc::new(AtomicBool::new(false)),
            rx: None,
            book,
            adding: false,
            server: String::new(),
            code: String::new(),
            handle: String::new(),
            renaming: None,
            deleting: None,
            log_open: false,
        }
    }
}

impl RemoteState {
    fn reload(&mut self) {
        if let Ok(b) = Book::load(&aox::paths::app_root()) {
            self.book = b;
        }
    }

    fn persist(&mut self) {
        if let Err(e) = self.book.save(&aox::paths::app_root()) {
            self.status = format!("Save failed: {e:#}");
        }
    }

    pub fn poll(&mut self) {
        let Some(rx) = self.rx.as_mut() else { return };
        let mut got = false;
        while let Ok(ev) = rx.try_recv() {
            got = true;
            match ev {
                RemoteEvent::Status(s) => {
                    let stopped = s.contains("Stopped") || s.contains("failed") || s.contains("error");
                    if stopped {
                        self.running = false;
                    }
                    // On success, hide the form and wipe the code; the 8-digit code is single-use, so keeping it misleads.
                    if s.contains("Paired") {
                        self.adding = false;
                        self.server.clear();
                        self.code.clear();
                        self.handle.clear();
                    }
                    self.logs.push(format!("· {s}"));
                    self.status = s;
                }
                RemoteEvent::Log(s) => self.logs.push(s),
            }
        }
        if got {
            self.busy = false;
            // A successful pairing is written to remote.json, so the list must refresh.
            self.reload();
        }
        if self.logs.len() > 80 {
            let cut = self.logs.len() - 80;
            self.logs.drain(0..cut);
        }
    }

    pub fn start(&mut self) {
        if self.running || self.busy {
            return;
        }
        if self.book.active().is_none() {
            self.status = "Pick a pairing first".into();
            return;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.rx = Some(rx);
        self.stop = Arc::new(AtomicBool::new(false));
        let stop = self.stop.clone();
        self.running = true;
        self.status = "Starting…".into();

        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(RemoteEvent::Log(format!("Cannot create the runtime: {e}")));
                    return;
                }
            };
            rt.block_on(async move {
                let root = aox::paths::app_root();
                let cfg_path = root.join("config.toml");
                let resolved = match aox::config::layer::resolve(&root, &cfg_path, None, None) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(RemoteEvent::Status(format!("Stopped (config error: {e:#})")));
                        return;
                    }
                };
                let runtime =
                    match aox::bootstrap::build(resolved.config, root.clone(), root.join("plugins"))
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            let _ =
                                tx.send(RemoteEvent::Status(format!("Stopped (runtime build failed: {e:#})")));
                            return;
                        }
                    };
                // The GUI has no terminal to take over, so control_stdin is false and authorization happens on the phone.
                if let Err(e) =
                    aox::remote::serve_with(runtime, root, Some(tx.clone()), stop, false).await
                {
                    let _ = tx.send(RemoteEvent::Status(format!("Stopped ({e:#})")));
                }
            });
        });
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.status = "Stopping…".into();
    }

    pub fn pair(&mut self) {
        if self.busy {
            return;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.rx = Some(rx);
        self.busy = true;
        self.status = "Pairing…".into();

        let server = self.server.clone();
        let code = self.code.clone();
        let handle = self.handle.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(RemoteEvent::Status(format!("Pairing failed: {e}")));
                    return;
                }
            };
            let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "aox-gui".into());
            let root = aox::paths::app_root();
            match rt.block_on(remote::pair_with(&server, &code, &handle, &host, &root)) {
                Ok(p) => {
                    let _ = tx.send(RemoteEvent::Status(format!(
                        "Paired: {} @{}",
                        p.server, p.agent_handle
                    )));
                }
                Err(e) => {
                    let _ = tx.send(RemoteEvent::Status(format!("Pairing failed: {e:#}")));
                }
            }
        });
    }

    // Section divider marking the start of the page layout.

    pub fn page(&mut self, ui: &mut egui::Ui) {
        // The dialog zeroes horizontal spacing (hand layout via add_space); this page restores it or dots/text stick.
        ui.spacing_mut().item_spacing.x = 8.0;
        self.status_block(ui);
        ui.add_space(20.0);
        self.pairings_block(ui);
        if !self.logs.is_empty() {
            ui.add_space(18.0);
            self.log_block(ui);
        }
    }

    /// Header: status dot, one line, right action button; the dot is painted, not a glyph that a font may lack.
    fn status_block(&mut self, ui: &mut egui::Ui) {
        let active = self.book.active().map(|p| (p.server.clone(), p.agent_handle.clone()));
        let running = self.running;
        let can_start = active.is_some() && !self.busy;

        ui.horizontal(|ui| {
            // Reserve the right button first, then the rest for text; otherwise a long address pushes the button off-screen.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if running {
                    if button(ui, t("Stop remote"), Weight::Secondary).clicked() {
                        self.stop();
                    }
                } else if button_ex(ui, t("Start remote"), Weight::Primary, can_start).clicked() {
                    self.start();
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
                    if running {
                        ui.painter().circle_filled(dot.center(), 4.0, P::add_green());
                    } else {
                        ui.painter().circle_stroke(
                            dot.center(),
                            3.5,
                            Stroke::new(1.4, P::border_strong()),
                        );
                    }
                    ui.label(
                        RichText::new(if running { t("Running") } else { t("Not running") })
                            .color(P::text())
                            .size(14.5),
                    );
                    if let Some((server, agent)) = &active {
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("{server} · @{agent}"))
                                    .color(P::text_faint())
                                    .size(12.0),
                            )
                            .truncate(),
                        );
                    }
                });
            });
        });

        // The status text stays on one elided line; it changes every frame, so wrapping would push the list down.
        if !self.status.is_empty() {
            ui.add_space(3.0);
            ui.add(
                egui::Label::new(RichText::new(&self.status).color(P::text_faint()).size(11.5))
                    .truncate(),
            );
        }
    }

    /// Pairing list (title, rows, add-form) in one column; rename and delete happen inline on that row.
    fn pairings_block(&mut self, ui: &mut egui::Ui) {
        let mut add_clicked = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(t("Pairings")).color(P::text_faint()).size(12.5));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !self.adding && flat_button(ui, t("+ Add pairing")).clicked() {
                    add_clicked = true;
                }
            });
        });
        if add_clicked {
            self.adding = true;
            self.renaming = None;
            self.deleting = None;
        }

        if self.adding {
            ui.add_space(2.0);
            self.add_form(ui);
            ui.add_space(4.0);
        }

        if self.book.pairings.is_empty() {
            if !self.adding {
                ui.add_space(2.0);
                ui.label(
                    RichText::new(t(
                        "No computer is paired yet. Generate a code on the phone, then add it here.",
                    ))
                    .color(P::text_dim())
                    .size(13.0),
                );
            }
            return;
        }

        ui.add_space(4.0);

        let rows: Vec<Row> = self
            .book
            .pairings
            .iter()
            .map(|p| Row {
                id: p.id.clone(),
                title: p.title().to_string(),
                server: p.server.clone(),
                agent: p.agent_handle.clone(),
                device: p.device_name.clone(),
                title_is_server: p.label.trim().is_empty(),
            })
            .collect();

        let active_id = self.book.active.clone();
        // Borrow the draft out for the pass and put it back: an active TextEdit needs a mutable, row-aligned String.
        let mut renaming = self.renaming.clone();
        let mut deleting = self.deleting.clone();
        let mut act: Option<Act> = None;

        for r in &rows {
            let is_active = active_id.as_deref() == Some(r.id.as_str());

            if renaming.as_ref().is_some_and(|(id, _)| id == &r.id) {
                let mut draft = renaming.as_ref().map(|(_, d)| d.clone()).unwrap_or_default();
                match rename_row(ui, &mut draft) {
                    EditAct::Save => {
                        self.book.rename(&r.id, &draft);
                        self.persist();
                        renaming = None;
                    }
                    EditAct::Cancel => renaming = None,
                    // Write the draft back every frame so the TextEdit string survives the frame and the caret tracks typing.
                    EditAct::None => renaming = Some((r.id.clone(), draft)),
                }
                continue;
            }

            if deleting.as_deref() == Some(r.id.as_str()) {
                match delete_row(ui, &r.title) {
                    EditAct::Save => {
                        self.book.remove(&r.id);
                        self.persist();
                        self.reload();
                        deleting = None;
                    }
                    EditAct::Cancel => deleting = None,
                    EditAct::None => {}
                }
                continue;
            }

            if let Some(a) = row_ui(ui, r, is_active) {
                act = Some(a);
            }
        }

        // Commit this frame's edit state before handling clicks, since a click outranks the row being edited.
        self.renaming = renaming;
        self.deleting = deleting;

        match act {
            Some(Act::Select(id)) => {
                if self.book.select(&id) {
                    self.persist();
                    self.status = "Switched the current pairing (applies on next start)".into();
                }
            }
            Some(Act::AskDelete(id)) => {
                self.deleting = Some(id);
                self.renaming = None;
            }
            Some(Act::StartRename(id)) => {
                let current = self
                    .book
                    .pairings
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.label.clone())
                    .unwrap_or_default();
                self.renaming = Some((id, current));
                self.deleting = None;
            }
            None => {}
        }
    }

    fn add_form(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new(t(
                "On the phone: Settings → Add computer. Type the 8-character code here within five minutes.",
            ))
            .color(P::text_faint())
            .size(12.0),
        );
        ui.add_space(8.0);

        // Name column width is measured from the widest name, not fixed; English is ~40% wider and would overlap.
        let keys = [t("Server"), t("Pairing code"), t("Agent name")];
        let kw = key_w(ui, &keys);
        let _ = field_row(ui, kw, t("Server"), &mut self.server, "https://chat.example.com");
        let _ = field_row(ui, kw, t("Pairing code"), &mut self.code, t("8 characters"));
        let _ = field_row(ui, kw, t("Agent name"), &mut self.handle, "aox-ran");

        ui.add_space(10.0);
        let ready = !self.server.trim().is_empty()
            && !self.code.trim().is_empty()
            && !self.handle.trim().is_empty()
            && !self.busy;
        let mut pair_now = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            if button_ex(ui, t("Pair"), Weight::Primary, ready).clicked() {
                pair_now = true;
            }
            if flat_button(ui, t("Cancel")).clicked() {
                cancel = true;
            }
        });
        if pair_now {
            self.pair();
        }
        if cancel {
            self.adding = false;
        }
    }

    /// Recent logs: clickable fold title plus monospace body in a well; no CollapsingHeader (its indent shifts 8pt).
    fn log_block(&mut self, ui: &mut egui::Ui) {
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 30.0), Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let fg = if resp.hovered() { P::text() } else { P::text_dim() };
        let g = ui.painter().layout_no_wrap(t("Recent log").to_owned(), FontId::proportional(12.5), fg);
        let tw = g.size().x;
        ui.painter().galley(Pos2::new(rect.left(), rect.center().y - g.size().y * 0.5), g, fg);

        // The fold chevron points down when open, right when closed, drawn as two segments (no font glyph).
        let c = Pos2::new(rect.left() + tw + 12.0, rect.center().y);
        let s = 3.2;
        let st = Stroke::new(1.3, fg);
        if self.log_open {
            ui.painter().line_segment([c + Vec2::new(-s, -s * 0.5), c + Vec2::new(0.0, s * 0.7)], st);
            ui.painter().line_segment([c + Vec2::new(0.0, s * 0.7), c + Vec2::new(s, -s * 0.5)], st);
        } else {
            ui.painter().line_segment([c + Vec2::new(-s * 0.5, -s), c + Vec2::new(s * 0.7, 0.0)], st);
            ui.painter().line_segment([c + Vec2::new(s * 0.7, 0.0), c + Vec2::new(-s * 0.5, s)], st);
        }
        if resp.clicked() {
            self.log_open = !self.log_open;
        }

        if self.log_open {
            ui.add_space(4.0);
            theme::well(theme::R1).inner_margin(Margin::symmetric(12, 10)).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(160.0).auto_shrink([false, true]).show(
                    ui,
                    |ui| {
                        for line in &self.logs {
                            ui.label(
                                RichText::new(line)
                                    .font(theme::mono_font(11.5))
                                    .color(P::text_faint()),
                            );
                        }
                    },
                );
            });
        }
    }
}

/// One pairing row.
struct Row {
    id: String,
    title: String,
    server: String,
    agent: String,
    device: String,
    /// When the title is just the address (no name set), the subtitle does not repeat the address.
    title_is_server: bool,
}

/// Draw a pairing row, returning this frame's action; the whole row (minus right actions) sets it as current.
fn row_ui(ui: &mut egui::Ui, r: &Row, active: bool) -> Option<Act> {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::hover());

    // Compute the right action width every frame, drawn or not, so the name's ellipsis stays constant on hover.
    let del_w = button_size(ui, t("Delete"), Weight::Ghost).x;
    let ren_w = button_size(ui, t("Rename"), Weight::Ghost).x;
    let reserved = del_w + ren_w + 10.0;

    // Only the left half is clickable (set as current); the current row gets no sense, so clicks are never no-ops.
    let can_use = !active;
    let hit = Rect::from_min_max(rect.min, Pos2::new(rect.right() - reserved, rect.max.y));
    let hresp = ui.interact(
        hit,
        egui::Id::new(("aox-pairing", &r.id)),
        if can_use { Sense::click() } else { Sense::hover() },
    );

    // Build both action areas every frame; hovered uses the prior frame, so hover-only buttons flicker.
    let mut right = rect.right() - PAD_L;
    let mut acts: Vec<(Rect, &'static str, egui::Response, fn(String) -> Act)> = Vec::new();
    for (label, bw, make) in [
        (t("Delete"), del_w, Act::AskDelete as fn(String) -> Act),
        (t("Rename"), ren_w, Act::StartRename as fn(String) -> Act),
    ] {
        let br = Rect::from_center_size(
            Pos2::new(right - bw * 0.5, rect.center().y),
            Vec2::new(bw, ACT_H),
        );
        let bresp = ui.interact(br, egui::Id::new(("aox-pairing-act", &r.id, label)), Sense::click());
        acts.push((br, label, bresp, make));
        right -= bw + 4.0;
    }

    // The row's hover state counts the row or any child hit area over it as pointer-in-row.
    let hov = resp.hovered() || hresp.hovered() || acts.iter().any(|(_, _, r, _)| r.hovered());
    if hov {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), P::surface_hi());
    }

    let mut act = None;
    for (br, label, bresp, make) in &acts {
        if !hov {
            continue;
        }
        if bresp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        paint_button(ui, *br, label, Weight::Ghost, bresp.hovered());
        if bresp.clicked() {
            act = Some(make(r.id.clone()));
        }
    }
    if can_use && hresp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // Note the whole row is clickable: the list offers only a hand cursor, no other hint.
    let hresp = if can_use { hresp.on_hover_text(t("Set as current")) } else { hresp };

    let name_fg = if active { P::ink() } else { P::text() };
    let check_w = if active { 20.0 } else { 0.0 };
    let text_w = (rect.right() - reserved - PAD_L - rect.left() - check_w - 6.0).max(80.0);
    let name_x = rect.left() + PAD_L;
    let g = elide(ui, &r.title, FontId::proportional(14.0), name_fg, text_w);
    let name_w = g.size().x;
    ui.painter().galley(Pos2::new(name_x, rect.top() + 11.0), g, name_fg);
    if active {
        paint_check(
            ui.painter(),
            Rect::from_center_size(
                Pos2::new(name_x + name_w + 12.0, rect.top() + 19.0),
                Vec2::splat(12.0),
            ),
        );
    }

    let sub = if r.title_is_server {
        format!("@{} · {}", r.agent, r.device)
    } else {
        format!("{} · @{} · {}", r.server, r.agent, r.device)
    };
    let sg = elide(ui, &sub, FontId::proportional(11.5), P::text_faint(), text_w);
    ui.painter().galley(Pos2::new(name_x, rect.top() + 32.0), sg, P::text_faint());

    if act.is_none() && hresp.clicked() {
        act = Some(Act::Select(r.id.clone()));
    }
    act
}

/// In-place rename: one field plus Save/Cancel, the height of one row so the list does not jump.
fn rename_row(ui: &mut egui::Ui, draft: &mut String) -> EditAct {
    let mut out = EditAct::None;
    let w = ui.available_width();
    egui::Frame::NONE
        .fill(P::surface_hi())
        .corner_radius(CornerRadius::same(theme::R1))
        .inner_margin(Margin::symmetric(PAD_L as i8, 11))
        .show(ui, |ui| {
            ui.set_min_width(w - PAD_L * 2.0);
            ui.horizontal(|ui| {
                // Size the field from the remaining row width, not a fixed value; cap at 280 so it cannot span the column.
                let bw = button_size(ui, t("Save"), Weight::Primary).x
                    + button_size(ui, t("Cancel"), Weight::Ghost).x
                    + 8.0;
                let fw = (ui.available_width() - bw - 12.0).clamp(120.0, 280.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(draft)
                        .desired_width(fw)
                        .margin(Margin { left: 10, right: 10, top: 8, bottom: 8 })
                        .font(FontId::proportional(14.0))
                        .hint_text(
                            RichText::new(t("e.g. Office server")).color(P::text_faint()).size(13.0),
                        ),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    out = EditAct::Save;
                }
                // Focus the field on open, but only when nothing else has focus, so it does not steal focus every frame.
                if ui.memory(|m| m.focused()).is_none() {
                    resp.request_focus();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button(ui, t("Save"), Weight::Primary).clicked() {
                        out = EditAct::Save;
                    }
                    if flat_button(ui, t("Cancel")).clicked() {
                        out = EditAct::Cancel;
                    }
                });
            });
        });
    out
}

/// In-place delete confirm: one line plus Delete/Cancel; red only here, solid since it is the second click.
fn delete_row(ui: &mut egui::Ui, title: &str) -> EditAct {
    let mut out = EditAct::None;
    let w = ui.available_width();
    egui::Frame::NONE
        .fill(P::surface_hi())
        .corner_radius(CornerRadius::same(theme::R1))
        .inner_margin(Margin::symmetric(PAD_L as i8, 11))
        .show(ui, |ui| {
            ui.set_min_width(w - PAD_L * 2.0);
            ui.horizontal(|ui| {
                // Compute button width first so the message uses only the remaining width, or long titles overlap buttons.
                let bw = button_size(ui, t("Delete"), Weight::DangerSolid).x
                    + button_size(ui, t("Cancel"), Weight::Ghost).x
                    + 8.0;
                let tw = (ui.available_width() - bw - 12.0).max(80.0);
                ui.add_sized(
                    Vec2::new(tw, BTN_H),
                    egui::Label::new(
                        RichText::new(aox::tf!("Delete {}? This only removes the local credential; the server record stays.", title))
                            .color(P::text_dim())
                            .size(13.0),
                    )
                    .truncate(),
                );
                if danger_button(ui, t("Delete")).clicked() {
                    out = EditAct::Save;
                }
                if flat_button(ui, t("Cancel")).clicked() {
                    out = EditAct::Cancel;
                }
            });
        });
    out
}

/// Widest name + 18pt rounded up (kv_key_w); painted directly, so fixed widths overlap values on language change.
fn key_w(ui: &egui::Ui, keys: &[&str]) -> f32 {
    let mut w = 84.0_f32;
    for k in keys {
        let g = ui.painter().layout_no_wrap(
            (*k).to_owned(),
            FontId::proportional(13.0),
            P::text_dim(),
        );
        w = w.max(g.size().x + 18.0);
    }
    w.ceil()
}

/// A row of name plus field; the field's 8px padding makes it as tall as BTN_H (14px text + 8 top/bottom).
fn field_row(
    ui: &mut egui::Ui,
    key_w: f32,
    label: &str,
    text: &mut String,
    hint: &str,
) -> egui::Response {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(key_w, BTN_H), Sense::hover());
        let g = ui.painter().layout_no_wrap(label.to_owned(), FontId::proportional(13.0), P::text_dim());
        ui.painter().galley(Pos2::new(r.left(), r.center().y - g.size().y * 0.5), g, P::text_dim());
        ui.add(
            egui::TextEdit::singleline(text)
                .desired_width(300.0)
                .margin(Margin { left: 10, right: 10, top: 8, bottom: 8 })
                .font(FontId::proportional(14.0))
                .hint_text(RichText::new(hint).color(P::text_faint()).size(13.0)),
        )
    })
    .inner
}
