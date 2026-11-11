//! The generic shell of an independent window — everything a borderless egui viewport needs that has nothing to do...
//! Extracted from `activity.rs`, because the activity stream is no longer the only window: the cache-hit map...
//! # What a caller gets.

use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Galley, Pos2, Rect, ResizeDirection, Sense,
    Stroke, TextFormat, Ui, Vec2, ViewportBuilder, ViewportCommand, ViewportId, WindowLevel,
};

use crate::app::default_window_size;
use crate::theme::{self, P};

// Layout metrics

/// Height of the self-drawn top bar. Shared by every window so the two look like siblings.
pub const HEADER_H: f32 = 52.0;

/// DWM's standard corner radius, matching the main window. The unit is egui points, which grow
/// with scaling.
pub const CORNER_RADIUS: u8 = theme::R1;

/// Grab thickness for border resizing (the four edges).
const RESIZE_BORDER: f32 = 5.0;
/// Side length of the four corner grab zones.
const RESIZE_CORNER: f32 = 12.0;

// Window ids

/// The window ids below are poker slots, not viewport ids: each is a bit in an atomic mask saying "this window...
pub const WIN_ACTIVITY: u8 = 0;
pub const WIN_CACHE: u8 = 1;
pub const WIN_WSVIEW: u8 = 2;

// The shell state

/// Window-level state shared between the main window and a child window.
#[derive(Default)]
pub struct Gwin {
    /// Whether the window should exist. Declarative: egui drops a child viewport that is not
    /// re-declared in a pass, so this flag is what `sync` turns into a declaration.
    pub open: bool,
    /// Always-on-top. Toggled by the pin button
    pub pinned: bool,
    /// Whether the main window is currently working. Drives the breathing dot and the repaint loop.
    pub busy: bool,
    /// Phase for the breathing dot.
    pub spin: f32,

    /// Mirror of `pinned` as last dispatched. The `WindowLevel` command is only sent on a change.
    applied_top: bool,
    /// The native theme was last sent for this viewport. Also sent only on a change.
    theme_mode: Option<theme::Mode>,
    /// Whether size and position have been set once (see the note in [`Gwin::builder`]).
    placed: bool,
    /// Whether rounded corners have been handed to the system (also a "do once per window" thing).
    corners: bool,
}

impl Gwin {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open the window (idempotent).
    pub fn open(&mut self, ctx: &egui::Context) {
        if self.open {
            return;
        }
        self.open = true;
        self.spin = 0.0;
        self.forget_placement(); // reopening a window means placing it afresh
        ctx.request_repaint();
    }

    /// Open the window directly, skipping the "already open" check — used by the demo preview.
    pub fn force_open(&mut self) {
        self.open = true;
        self.spin = 0.0;
        self.forget_placement();
    }

    /// Close the window from inside it.
    pub fn close(&mut self, ctx: &egui::Context) {
        self.open = false;
        self.forget_placement();
        ctx.send_viewport_cmd(ViewportCommand::Close);
        nudge_root(ctx);
    }

    /// "This window is gone". Every do-once-per-window initialization must be invalidated here, so it gets redone next...
    pub fn forget_placement(&mut self) {
        self.placed = false;
        self.corners = false;
    }

    /// Build the viewport declaration for this frame.
    pub fn builder(
        &mut self,
        ctx: &egui::Context,
        title: &'static str,
        size: Vec2,
        min: Vec2,
        shift: Vec2,
    ) -> ViewportBuilder {
        let fresh = !self.placed;
        self.placed = true;

        let mut b = ViewportBuilder::default()
            .with_title(aox::i18n::t(title))
            .with_resizable(true)
            // No system decorations: keeping them would just add a Windows-supplied title bar on top (icon + title +...
            .with_decorations(false);

        if fresh {
            b = b
                .with_inner_size([size.x, size.y])
                .with_min_inner_size([min.x, min.y])
                .with_position(initial_pos(ctx, size) + shift);
        }
        if self.pinned {
            b = b.with_window_level(WindowLevel::AlwaysOnTop);
        }
        b
    }

    /// Per-frame window-callback prologue: everything that must happen before the content is drawn, and nothing...
    pub fn prelude(&mut self, ctx: &egui::Context, title: &'static str) {
        // System-level close (Alt+F4, taskbar right-click close) must flip the switch back too, otherwise the main window...
        if ctx.input(|i| i.viewport().close_requested()) {
            self.open = false;
            self.forget_placement();
            nudge_root(ctx);
        }
        // Dispatch the command only when the pinned state changes, not every frame.
        if self.pinned != self.applied_top {
            self.applied_top = self.pinned;
            ctx.send_viewport_cmd(ViewportCommand::WindowLevel(if self.pinned {
                WindowLevel::AlwaysOnTop
            } else {
                WindowLevel::Normal
            }));
        }
        // The native theme must be sent **once per viewport**: `SetTheme` is dispatched per
        // viewport, so a child that has never sent it would keep the system's default.
        if self.theme_mode != Some(theme::mode()) {
            self.theme_mode = Some(theme::mode());
            theme::sync_native_theme(ctx);
        }
        // Rounded corners are the same — a "do once per window" thing.
        if !self.corners {
            self.corners = true;
            set_round_corners(aox::i18n::t(title));
        }
    }

    /// First half of the per-frame epilogue: advance the breathing dot and decide what this frame wants.
    pub fn step(&mut self) -> Drive {
        let busy = self.busy && self.open;
        if busy {
            self.spin = (self.spin + 0.016) % 1000.0;
        }
        Drive { busy, closing: !self.open }
    }
}

/// What [`Gwin::step`] decided, on its way to [`drive`]
pub struct Drive {
    /// The window is working: animate, and keep the main window painting.
    pub busy: bool,
    /// The window is on its way out: nudge the root once so it stops re-declaring us.
    pub closing: bool,
}

/// Second half of the per-frame epilogue: drive repaints, and declare whether this window needs the main window...
pub fn drive(ctx: &egui::Context, id: u8, d: Drive) {
    if d.busy {
        // Keep the main window's pass loop alive: it is what feeds this window (see [`poke_main`]).
        want(id, true);
        nudge_root(ctx);
        ctx.request_repaint_after(Duration::from_millis(16));
    } else {
        want(id, false);
        if d.closing {
            nudge_root(ctx);
        }
    }
}

/// Ask the root viewport for a pass right now.
pub fn nudge_root(ctx: &egui::Context) {
    ctx.request_repaint_of(ViewportId::ROOT);
}

// Waking the main window (the poker)

/// The main window's native handle, remembered so any child can poke it awake.
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

pub fn remember_main_window(handle: &dyn raw_window_handle::HasWindowHandle) {
    #[cfg(windows)]
    {
        use raw_window_handle::RawWindowHandle;
        if MAIN_HWND.load(Ordering::Relaxed) != 0 {
            return;
        }
        if let Ok(h) = handle.window_handle() {
            if let RawWindowHandle::Win32(w) = h.as_raw() {
                MAIN_HWND.store(w.hwnd.get(), Ordering::Relaxed);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = handle;
}

/// Send the main window a `WM_PAINT` now, bypassing the message queue.
#[cfg(windows)]
fn poke_main(hwnd: isize) {
    use std::ffi::c_void;

    extern "system" {
        fn InvalidateRect(hwnd: *mut c_void, rect: *const c_void, erase: i32) -> i32;
        fn UpdateWindow(hwnd: *mut c_void) -> i32;
    }
    let h = hwnd as *mut c_void;
    // SAFETY: `hwnd` is the main window's own handle, taken from its `RawWindowHandle`; the window
    // outlives this app and both calls are safe to issue at any time.
    unsafe {
        InvalidateRect(h, std::ptr::null(), 0);
        UpdateWindow(h);
    }
}

#[cfg(not(windows))]
fn poke_main(_hwnd: isize) {}

/// Which windows currently want the main window poked — one bit per [`WIN_ACTIVITY`]/[`WIN_CACHE`].
static POKE_WANT: AtomicU32 = AtomicU32::new(0);
/// Started on first use and never stopped: the thread itself is cheap (it sleeps), and tearing it
/// down would mean deciding when no window can ever want a poke again.
static WORKER: OnceLock<()> = OnceLock::new();

/// Declare whether window `id` needs the main window painting this frame.
pub fn want(id: u8, on: bool) {
    let bit = 1u32 << id;
    if on {
        POKE_WANT.fetch_or(bit, Ordering::Relaxed);
        WORKER.get_or_init(|| {
            // If the thread cannot be started, the windows simply behave as they did before: the
            // pokes never happen, which is a lost feature rather than a broken window.
            let _ = std::thread::Builder::new().name("aox-window-poke".into()).spawn(|| loop {
                if POKE_WANT.load(Ordering::Relaxed) != 0 {
                    let h = MAIN_HWND.load(Ordering::Relaxed);
                    if h != 0 {
                        poke_main(h);
                    }
                }
                std::thread::sleep(Duration::from_millis(16));
            });
        });
    } else {
        POKE_WANT.fetch_and(!bit, Ordering::Relaxed);
    }
}

// Window furniture

/// An undecorated window has no native border resizing; we add it back here.
pub fn resize_handles(ui: &mut Ui, ctx: &egui::Context, full: Rect) {
    // A maximized window should not be edge-draggable (Windows itself works this way), which also
    // avoids the oddity of "dragging along and falling out of the maximized state".
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }

    let zones = resize_zones(full);
    // The cursor must change on hover (without waiting for press), otherwise the user has no idea this is draggable.
    let clip = ui.clip_rect();
    let hover = ctx.input(|i| i.pointer.hover_pos());
    if let Some(p) = hover {
        if let Some((_, _, d)) = zones.iter().find(|(_, r, _)| r.intersect(clip).contains(p)) {
            ctx.set_cursor_icon(resize_cursor(*d));
        }
    }

    for (id, r, dir) in zones {
        let resp = ui.interact(r, ui.id().with(id), Sense::drag());
        if resp.hovered() {
            ctx.set_cursor_icon(resize_cursor(dir));
        }
        if resp.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
        }
    }
}

/// Geometry of the eight hot-zones.
fn resize_zones(full: Rect) -> [(&'static str, Rect, ResizeDirection); 8] {
    let (l, r, t, b) = (full.left(), full.right(), full.top(), full.bottom());
    let d = RESIZE_BORDER;
    let c = RESIZE_CORNER;
    let rect =
        |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1));
    use ResizeDirection::*;
    [
        ("nw", rect(l, t, l + c, t + c), NorthWest),
        ("ne", rect(r - c, t, r, t + c), NorthEast),
        ("sw", rect(l, b - c, l + c, b), SouthWest),
        ("se", rect(r - c, b - c, r, b), SouthEast),
        ("n", rect(l + c, t, r - c, t + d), North),
        ("s", rect(l + c, b - d, r - c, b), South),
        ("w", rect(l, t + c, l + d, b - c), West),
        ("e", rect(r - d, t + c, r, b - c), East),
    ]
}

fn resize_cursor(d: ResizeDirection) -> CursorIcon {
    use ResizeDirection::*;
    match d {
        North | South => CursorIcon::ResizeVertical,
        East | West => CursorIcon::ResizeHorizontal,
        NorthEast | SouthWest => CursorIcon::ResizeNeSw,
        NorthWest | SouthEast => CursorIcon::ResizeNwSe,
    }
}

/// There is no dividing line between an undecorated window and the desktop; in a light theme you can barely tell...
pub fn outline(ui: &Ui, full: Rect) {
    outline_painter(ui.painter(), full);
}

/// [`outline`] without a `Ui`, for a caller that has to paint the frame **after** its own panels
/// have finished (the main window: the central panel's fill would cover a stroke drawn earlier).
pub fn outline_painter(painter: &egui::Painter, full: Rect) {
    painter.rect_stroke(
        full.shrink(0.5),
        CornerRadius::same(CORNER_RADIUS),
        Stroke::new(1.0, P::border_strong()),
        egui::StrokeKind::Inside,
    );
}

/// Corner radius of a [`float_plate`] — one step rounder than a button's 8, because the plate is
/// the outer shape and has to look like it, not like a button that happens to be big.
pub const FLOAT_R: u8 = 10;

/// The plate under a control that floats directly on the content instead of on a bar.
pub fn float_plate(painter: &egui::Painter, r: Rect, radius: u8, hot: bool) {
    painter.rect_filled(
        r,
        CornerRadius::same(radius),
        if hot { P::surface_hi() } else { P::float_fill() },
    );
    painter.rect_stroke(
        r,
        CornerRadius::same(radius),
        Stroke::new(1.0, P::edge_raised()),
        egui::StrokeKind::Inside,
    );
}

/// Let the system draw rounded corners on this window.
#[cfg(windows)]
fn set_round_corners(title: &str) {
    use std::ffi::c_void;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(cb: extern "system" fn(*mut c_void, isize) -> i32, param: isize) -> i32;
        fn IsWindowVisible(h: *mut c_void) -> i32;
        fn GetWindowTextLengthW(h: *mut c_void) -> i32;
        fn GetWindowTextW(h: *mut c_void, buf: *mut u16, max: i32) -> i32;
        fn GetWindowThreadProcessId(h: *mut c_void, pid: *mut u32) -> u32;
    }

    /// The context used during enumeration. The callback can only be a plain fn that captures no
    /// environment, so things must be passed in via `param`.
    struct Probe<'a> {
        title: &'a str,
        pid: u32,
        hwnd: *mut c_void,
    }

    /// Whether it is "a window opened by this process with a matching title".
    unsafe fn mine(h: *mut c_void, title: &str, pid: u32) -> bool {
        if unsafe { IsWindowVisible(h) } == 0 {
            return false;
        }
        // Looking at the title alone is not enough: a user may well have two AOX instances open at
        // once (e.g. two installs), and then we could reach another's window and change its corners.
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(h, &mut owner) };
        if owner != pid {
            return false;
        }
        let n = unsafe { GetWindowTextLengthW(h) };
        if n <= 0 {
            return false;
        }
        let mut buf: Vec<u16> = vec![0; n as usize + 1];
        let got = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), n + 1) };
        if got <= 0 {
            return false;
        }
        String::from_utf16_lossy(&buf[..got as usize]).contains(title)
    }

    extern "system" fn probe(h: *mut c_void, param: isize) -> i32 {
        // SAFETY: `param` is the address of the `Probe` below, with a lifetime covering the whole
        // `EnumWindows` call.
        let p = unsafe { &mut *(param as *mut Probe) };
        if p.hwnd.is_null() && unsafe { mine(h, p.title, p.pid) } {
            p.hwnd = h;
            return 0; // stop once found, don't keep enumerating
        }
        1
    }

    let mut ctx = Probe { title, pid: std::process::id(), hwnd: std::ptr::null_mut() };
    unsafe { EnumWindows(probe, &mut ctx as *mut Probe as isize) };
    if ctx.hwnd.is_null() {
        return; // not built yet (or the platform doesn't provide it); try again next time the window opens
    }
    // DWM calls all go through `theme`: each symbol is declared only once in the whole crate.
    // Failure (Win10 does not know this attribute) is fine — corners simply stay square, as they were.
    theme::dwm_set_attribute(
        ctx.hwnd as isize,
        theme::DWMWA_CORNER_PREFERENCE,
        &theme::DWMWCP_ROUND as *const i32 as *const c_void,
        std::mem::size_of::<i32>() as u32,
    );
}

#[cfg(not(windows))]
fn set_round_corners(_title: &str) {}

// Top bar

#[derive(Clone, Copy)]
pub enum Glyph {
    Close,
    Min,
    /// Maximize / restore. `on` = currently maximized (so it is drawn as the "restore" double frame)
    Max { on: bool },
    Pin { on: bool },
}

/// The self-drawn top bar: breathing dot, title, state text, and the four window buttons.
pub fn header(ui: &mut Ui, ctx: &egui::Context, gw: &mut Gwin, title: &str, info: &str) {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), HEADER_H), Sense::click_and_drag());
    // StartDrag must be sent on the **very frame** the mouse is pressed; a frame late and the
    // window won't follow.
    if resp.drag_started() {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
    // Double-clicking a title bar maximizes: the system used to do it for us, and it is the one gesture people try...
    let maxed = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    if resp.double_clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maxed));
    }

    // Left: breathing dot + title + current state
    let pulse = ((gw.spin * 2.0) as i32) % 2 == 0;
    ui.painter().circle_filled(
        Pos2::new(rect.left() + 22.0, rect.center().y),
        5.0,
        if gw.busy {
            if pulse {
                P::ink()
            } else {
                P::ink_soft()
            }
        } else {
            P::text_faint()
        },
    );
    // The state text starts after the title, measured rather than at a fixed offset: the title is a translated...
    let title_font = theme::bold_font(18.0);
    let title_x = rect.left() + 40.0;
    let title_w = text_w(ui, &title_font, title);
    ui.painter().text(
        Pos2::new(title_x, rect.center().y),
        Align2::LEFT_CENTER,
        title,
        title_font,
        P::text(),
    );
    ui.painter().text(
        Pos2::new(title_x + title_w + 14.0, rect.center().y),
        Align2::LEFT_CENTER,
        info,
        FontId::proportional(13.5),
        P::text_faint(),
    );

    // Laid out right-to-left: close / maximize / minimize / pin, which lands on screen as Windows' usual "pin ·...
    const BW: f32 = 32.0;
    const BH: f32 = 28.0;
    const GAP: f32 = 2.0;
    let y = rect.center().y - BH / 2.0;
    let mut x = rect.right() - 10.0 - BW;
    let btn = |x: f32| Rect::from_min_size(Pos2::new(x, y), Vec2::new(BW, BH));
    if win_button(ui, btn(x), "close", Glyph::Close) {
        gw.close(ctx);
    }
    x -= BW + GAP;
    // The maximized state must be asked for actively — this window is self-managed now, and nobody
    // remembers whether it is maximized for us.
    if win_button(ui, btn(x), "max", Glyph::Max { on: maxed }) {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maxed));
    }
    x -= BW + GAP;
    if win_button(ui, btn(x), "min", Glyph::Min) {
        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
    }
    x -= BW + GAP;
    if win_button(ui, btn(x), "pin", Glyph::Pin { on: gw.pinned }) {
        gw.pinned = !gw.pinned;
    }

    // A thin line between the title bar and the content
    let line = Rect::from_min_max(
        Pos2::new(rect.left(), rect.bottom() - 1.0),
        Pos2::new(rect.right(), rect.bottom()),
    );
    ui.painter().rect_filled(line, CornerRadius::ZERO, P::border());
}

pub fn win_button(ui: &mut Ui, r: Rect, id: &str, glyph: Glyph) -> bool {
    let resp = ui.interact(r, ui.id().with(id), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, CornerRadius::same(theme::R1), P::surface_hi());
    }
    // "Pinned" is a persistent state and must stay visible: give it the deepest foreground colour even...
    let pinned = matches!(glyph, Glyph::Pin { on: true });
    let fg = if resp.hovered() || pinned { P::ink() } else { P::text_dim() };
    let p = ui.painter();
    let c = r.center();
    let stroke = Stroke::new(1.4, fg);
    match glyph {
        Glyph::Close => {
            let d = 4.2;
            p.line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], stroke);
            p.line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], stroke);
        }
        Glyph::Min => {
            p.line_segment([c + Vec2::new(-5.0, 2.4), c + Vec2::new(5.0, 2.4)], stroke);
        }
        Glyph::Max { on: false } => {
            // A single box — exactly Windows' maximize
            p.rect_stroke(
                Rect::from_center_size(c, Vec2::splat(9.6)),
                CornerRadius::ZERO,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        Glyph::Max { on: true } => {
            // "Restore": the rear frame is half-hidden by the front one, so only its top and right
            // edges are stroked.
            let (b0, b1) = (c + Vec2::new(-2.4, -5.0), c + Vec2::new(5.0, 1.6));
            p.line_segment([b0, Pos2::new(b1.x, b0.y)], stroke);
            p.line_segment([Pos2::new(b1.x, b0.y), b1], stroke);
            p.rect_stroke(
                Rect::from_min_max(c + Vec2::new(-4.8, -2.0), c + Vec2::new(2.6, 5.4)),
                CornerRadius::ZERO,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        Glyph::Pin { on } => {
            // A pushpin: a flat head + a needle tapering to a point.
            let rot = |v: Vec2| if on { v } else { Vec2::new(-v.y, v.x) };
            // After a 90° rotation a rect's length and width swap, so the dimensions need a
            // separate swap.
            let swap = |v: Vec2| if on { v } else { Vec2::new(v.y, v.x) };

            // Head
            p.rect_filled(
                Rect::from_center_size(c + rot(Vec2::new(0.0, -4.2)), swap(Vec2::new(10.0, 2.6))),
                CornerRadius::same(theme::R_XS),
                fg,
            );
            // Needle shaft
            p.rect_filled(
                Rect::from_center_size(c + rot(Vec2::new(0.0, -1.0)), swap(Vec2::new(3.2, 3.8))),
                CornerRadius::ZERO,
                fg,
            );
            // Needle tip
            p.add(egui::Shape::convex_polygon(
                vec![
                    c + rot(Vec2::new(0.0, 4.8)),
                    c + rot(Vec2::new(-1.6, 0.9)),
                    c + rot(Vec2::new(1.6, 0.9)),
                ],
                fg,
                Stroke::NONE,
            ));
        }
    }
    resp.clicked()
}

// Small shared widgets

/// A thin vertical separator, for toolbars.
pub fn vsep(ui: &mut Ui) {
    ui.add_space(6.0);
    let (r, _) = ui.allocate_exact_size(Vec2::new(1.0, 20.0), Sense::hover());
    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, P::border()));
    ui.add_space(6.0);
}

/// A pill button. `on` = the currently selected one (reversed).
pub fn pill(ui: &mut Ui, text: &str, on: bool) -> egui::Response {
    let font = FontId::proportional(14.0);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    // 7 rather than 6 of vertical padding: the pill is the toolbar's only hit target, so it gets the
    // same "comfortably clickable" height as the buttons in the main window.
    let pad = Vec2::new(13.0, 7.0);
    let size = galley.size() + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hov = resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    let r = Rect::from_center_size(rect.center(), size);
    p.rect_filled(
        r,
        CornerRadius::same(theme::R1),
        if on {
            P::reverse_bg()
        } else if hov {
            P::surface_hi()
        } else {
            Color32::TRANSPARENT
        },
    );
    if !on {
        p.rect_stroke(
            r,
            CornerRadius::same(theme::R1),
            Stroke::new(1.0, if hov { P::edge_raised() } else { P::edge() }),
            egui::StrokeKind::Inside,
        );
    }
    let fg = if on { P::on_reverse() } else if hov { P::text() } else { P::text_dim() };
    p.galley(r.min + pad, galley, fg);
    resp
}

/// Rendered width of a single line, in pixels.
pub fn text_w(ui: &Ui, font: &FontId, s: &str) -> f32 {
    let mut job = egui::text::LayoutJob::default();
    job.append(s, 0.0, TextFormat { font_id: font.clone(), ..Default::default() });
    ui.fonts_mut(|f| f.layout_job(job).size().x)
}

/// Clamp single-line text by **pixels**: elide with an ellipsis when it doesn't fit (row names and
/// tool names can be long).
pub fn elide(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> std::sync::Arc<Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        TextFormat { font_id: font, color, ..Default::default() },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: max_w.max(20.0),
        max_rows: 1,
        // CJK has no word boundaries; without this, "the whole thing doesn't fit" would simply not
        // elide.
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

// Initial geometry

/// Initial size = the main window's default size (the same formula, see `app::default_window_size`).
pub fn initial_size(ctx: &egui::Context) -> Vec2 {
    let (mon, chrome) = ctx.input(|i| {
        // All viewports' info is in `raw.viewports` (eframe fills the whole table in every frame)
        let root = i.raw.viewports.get(&ViewportId::ROOT);
        let mon = i.viewport().monitor_size.or_else(|| root.and_then(|v| v.monitor_size));
        let chrome = root
            .and_then(|v| Some(v.outer_rect?.size() - v.inner_rect?.size()))
            .filter(|c| c.x > 0.0 && c.y > 0.0)
            // If unavailable (first frame, or the platform doesn't provide it), use a conservative
            // estimate.
            .unwrap_or(Vec2::new(18.0, 46.0));
        (mon, chrome)
    });
    let Some(mon) = mon else {
        return Vec2::new(1280.0, 800.0);
    };
    default_window_size(mon, chrome)
}

/// Initial position: centered on the same monitor as the main window.
fn initial_pos(ctx: &egui::Context, size: Vec2) -> Pos2 {
    let mon_size = ctx.input(|i| i.viewport().monitor_size).unwrap_or(Vec2::new(1920.0, 1080.0));
    let mon = Rect::from_min_size(Pos2::ZERO, mon_size);
    let clamp = |v: f32, lo: f32, hi: f32| v.clamp(lo, hi.max(lo));
    Pos2::new(
        clamp(mon.center().x - size.x / 2.0, 8.0, mon.max.x - size.x - 8.0),
        clamp(mon.center().y - size.y / 2.0, 8.0, mon.max.y - size.y - 8.0),
    )
}
