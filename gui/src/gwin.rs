//! The **generic shell of an independent window** — everything a borderless egui viewport needs
//! that has nothing to do with what is drawn inside it.
//!
//! Extracted from `activity.rs`, because the activity stream is no longer the only window: the
//! cache-hit map (`cache.rs`) is a second one, with the same furniture. Two windows that each grew
//! their own copy of "drag, resize, rounded corners, self-drawn buttons, wake the main window"
//! would drift apart within a release or two — the close button would hover differently, the
//! corner radius would differ by a point, and one of them would forget to stand the poker down.
//! One shape, one drawing.
//!
//! # What a caller gets
//!
//! [`Gwin`] holds the window-level state (open / pinned / busy / spin, plus the Rust-side flags for
//! the two "do once per window" steps). A caller's window callback reads as:
//!
//! ```text
//! fn window(ctx, shared) {
//!     let mut st = lock(shared);
//!     st.gw.prelude(ctx, TITLE);              // system close, pin, native theme, rounded corners
//!     CentralPanel::default().frame(...).show(ctx, |ui| { body(ui, ctx, &mut st) });
//!     st.gw.upkeep(ctx, WIN_CACHE);           // repaint while busy, and poke the main window
//! }
//! ```
//!
//! and [`header`] draws the whole top bar (breathing dot, title, state text, pin / minimize /
//! maximize / close).
//!
//! # Why the shell exists at all
//!
//! Windows' own title bar is gone (`with_decorations(false)`), which throws away the system's
//! free services with it. Each one is rebuilt here:
//!
//! - **Drag**: press anywhere on the top bar and send [`ViewportCommand::StartDrag`].
//! - **Resize**: [`resize_handles`] buries a transparent hot-zone along each edge and corner; the
//!   press hands the direction to winit, which then owns the whole drag.
//! - **Rounded corners**: not drawn by us — a transparent viewport would lose the system shadow
//!   with the corners. [`set_round_corners`] asks DWM to clip them instead, so the radius matches
//!   the main window for free.
//! - **Border**: 1px [`outline`], since borderless means no seam against the desktop.
//! - **Maximize**: an extra top-bar button; once the system bar is gone, double-clicking the title
//!   bar and Win+↑ stop working too.
//!
//! # The poker
//!
//! A deferred child viewport is fed by the **main window's** pass: `App::update` drains the agent
//! channel and stamps a time base onto the shared state. A sleeping main window therefore freezes
//! every child at once (see the long note on [`poke_main`]). While any child is busy it declares
//! that with [`want`], and the one background thread sends the main window a `WM_PAINT`.
//!
//! The declaration is **per window and idempotent per frame** (`want(WIN_ACTIVITY, false)` retires
//! that window's claim) rather than an `arm`/`disarm` pair around a bare flag: with two windows
//! open, whichever one closed first used to be able to stand the other's pokes down.

use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Galley, Pos2, Rect, ResizeDirection, Sense,
    Stroke, TextFormat, Ui, Vec2, ViewportBuilder, ViewportCommand, ViewportId, WindowLevel,
};

use crate::app::default_window_size;
use crate::theme::{self, P};

// ─────────────────────────── Layout metrics ───────────────────────────

/// Height of the self-drawn top bar. Shared by every window so the two look like siblings.
pub const HEADER_H: f32 = 52.0;

/// DWM's standard corner radius, matching the main window. The unit is egui points, which grow
/// with scaling.
pub const CORNER_RADIUS: u8 = 8;

/// Grab thickness for border resizing (the four edges). An undecorated window has no native border,
/// so these pixels must be reserved by us: too narrow is hard to grab, too wide eats into the top
/// bar's drag zone at both ends.
const RESIZE_BORDER: f32 = 5.0;
/// Side length of the four corner grab zones. Corners must support diagonal dragging, so they are
/// thicker than the edges.
///
/// The upper bound is set by the top-bar buttons: the close button's left edge sits 10 + 32 = 42
/// points from the right, and its top edge is at `HEADER_H/2 - BH/2 = 12` points, so a 12-point
/// square corner zone cannot reach it.
const RESIZE_CORNER: f32 = 12.0;

// ───────────────────────────── Window ids ─────────────────────────────

/// The window ids below are **poker slots**, not viewport ids: each is a bit in an atomic mask
/// saying "this window needs the main window painting". Two is a hard ceiling on how many
/// independent windows can matter here; the mask is 32 bits wide, so there is room.
pub const WIN_ACTIVITY: u8 = 0;
pub const WIN_CACHE: u8 = 1;

// ─────────────────────────── The shell state ───────────────────────────

/// Window-level state shared between the main window and a child window.
///
/// Everything here is "about the window", never "about its contents" — the contents live in the
/// caller's own state next to this.
#[derive(Default)]
pub struct Gwin {
    /// Whether the window should exist. Declarative: egui drops a child viewport that is not
    /// re-declared in a pass, so this flag is what `sync` turns into a declaration.
    pub open: bool,
    /// Always-on-top. Toggled by the pin button.
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
    ///
    /// Deliberately **not a toggle** when called from a menu entry: a control labelled "open" that
    /// actually closes the window is one of the most program-is-broken-feeling interactions. The
    /// window has its own × and supports Alt+F4, so there is a proper way to close it.
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
    ///
    /// The `Close` command alone is not enough: a deferred child viewport cannot destroy itself —
    /// the command only surfaces as `ViewportEvent::Close` (i.e. as *input*) — and the window
    /// actually goes away when the **parent** stops re-declaring it. So wake the root up to run
    /// that pass now, rather than whenever it happens to repaint next.
    pub fn close(&mut self, ctx: &egui::Context) {
        self.open = false;
        self.forget_placement();
        ctx.send_viewport_cmd(ViewportCommand::Close);
        nudge_root(ctx);
    }

    /// "This window is gone". Every **do-once-per-window** initialization must be invalidated here,
    /// so it gets redone next time the window opens — currently two things: placing the size and
    /// position (`placed`), and telling the system to draw rounded corners (`corners`, since the
    /// rounding is attached to the HWND and vanishes with the window).
    pub fn forget_placement(&mut self) {
        self.placed = false;
        self.corners = false;
    }

    /// Build the viewport declaration for this frame.
    ///
    /// Size and position are given **only on the first declaration**. Every frame eframe `patch`es
    /// the old builder with a new one and dispatches any changed field as a command (position →
    /// `OuterPosition`, size → `InnerSize`), and some fields even rebuild the window outright. So
    /// "reporting the position every frame" equals yanking the window back every frame — showing up
    /// as it refusing to be dragged, refusing to be resized, and losing always-on-top. From the
    /// second time on, report only **never-changing** fields like title and resizable.
    ///
    /// `shift` moves the window off dead centre. Every independent window shares the same
    /// "centre of the monitor" default ([`initial_pos`]), so two of them opened in one session
    /// would land exactly on top of each other — the second looking like the first having failed to
    /// change anything. Callers pass a small offset; the first window passes zero.
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
            // **No system decorations**: keeping them would just add a Windows-supplied title bar
            // on top (icon + title + minimize/maximize/close) while our top bar's buttons are
            // self-drawn, leaving two bars stacked. After turning it off, drag / resize / maximize
            // are all taken over by [`header`] and [`resize_handles`].
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

    /// Per-frame window-callback prologue: everything that must happen **before** the content is
    /// drawn, and nothing the content can influence.
    ///
    /// `title` must be `'static` because the translation table hands back a `&'static str` (and,
    /// with no entry for the key, the key itself) — the same constraint every other string literal
    /// in this crate is under.
    pub fn prelude(&mut self, ctx: &egui::Context, title: &'static str) {
        // System-level close (Alt+F4, taskbar right-click close) must flip the switch back too,
        // otherwise the main window re-declares this viewport next frame and the window cannot be
        // closed.
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
        // Rounded corners are the same — a "do once per window" thing. It is done here rather than
        // when building the viewport: at that point winit has not created the HWND yet, so it
        // cannot be found by title at all.
        if !self.corners {
            self.corners = true;
            set_round_corners(aox::i18n::t(title));
        }
    }

    /// First half of the per-frame epilogue: advance the breathing dot and decide what this frame
    /// wants. **Called while the shared lock is still held** — the returned [`Drive`] carries the
    /// two answers out, so the second half can run without it.
    ///
    /// While busy the window drives its own repaints: a child viewport's repaint requests are
    /// independent of the root's, and without them the contents would freeze the moment the main
    /// window stops passing.
    ///
    /// Gated on `open`: once the switch has been flipped this window is on its way out, and a
    /// self-repainting child is precisely what would keep it on screen — it has to keep nudging
    /// the root (which is what actually retires the viewport) instead of redrawing itself forever.
    pub fn step(&mut self) -> Drive {
        let busy = self.busy && self.open;
        if busy {
            self.spin = (self.spin + 0.016) % 1000.0;
        }
        Drive { busy, closing: !self.open }
    }
}

/// What [`Gwin::step`] decided, on its way to [`drive`].
pub struct Drive {
    /// The window is working: animate, and keep the main window painting.
    pub busy: bool,
    /// The window is on its way out: nudge the root once so it stops re-declaring us.
    pub closing: bool,
}

/// Second half of the per-frame epilogue: drive repaints, and declare whether this window needs the
/// main window painting.
///
/// **Call this with the shared lock released.** The poke lands as a `WM_PAINT` on the main thread,
/// whose own pass takes the same lock — poking while still holding it would stall the main window
/// until this frame's draw finished.
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

/// Ask the **root** viewport for a pass right now.
///
/// egui's own docs put it plainly for a deferred child: the callback "may be called ... while the
/// parent viewport (the caller) is sleeping" — exactly our shape, since a busy child animates
/// itself and a sleeping main window has no reason to run a pass. So flipping `open` to `false`
/// used to change nothing until the main window happened to repaint again (which is why closing
/// lagged *only while a recording was under way*). Nudging the root makes that pass happen now.
pub fn nudge_root(ctx: &egui::Context) {
    ctx.request_repaint_of(ViewportId::ROOT);
}

// ─────────────────────── Waking the main window (the poker) ───────────────────────

/// The main window's native handle, remembered so any child can poke it awake.
///
/// Only the main window has a handle to give, and it has one for exactly one frame's worth of
/// effort, so it hands it over once (`App::update`, from the `eframe::Frame`). A single global
/// rather than a field per window: there is one main window, and both children need to reach it.
/// On platforms without windows this is a no-op.
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

/// Send the main window a `WM_PAINT` **now**, bypassing the message queue.
///
/// The records *and* the time base shown by every child window are produced by the main window's
/// own pass: `App::update` drains the agent channel into the shared state and stamps `now` onto it.
/// So a main window that stops passing takes the whole live view with it — no new blocks, and an
/// axis that never moves.
///
/// And it does stop. winit asks for a repaint with `RedrawWindow(.., RDW_INTERNALPAINT)`, which
/// *posts* WM_PAINT — and that posted message does not reach this window's sibling here. Runs
/// measured on Windows 11: the main window gets 4 passes in a 4-second run while a turn is under
/// way (`now` stuck at 0.112 s, the timeline dead) against ~80/s once something else wakes it.
/// `InvalidateRect` + `UpdateWindow` *send* WM_PAINT straight to the window procedure instead, and
/// that does get through: the same run recovers ~43 passes/s and the axis moves.
///
/// Only sent while some window is busy ([`want`]), so an idle app still costs nothing.
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

/// Declare whether window `id` needs the main window painting this frame. Idempotent, so callers
/// can set it every frame on both paths without bookkeeping.
///
/// The thread has to be a *different* one. A poke from a window's own paint callback is a
/// same-thread send, received re-entrantly while winit has its event handler taken — winit then
/// handles that by deferring the `RedrawRequested` and asking for the paint again with
/// `RedrawWindow(.., RDW_INTERNALPAINT)`, i.e. the very posted message that does not get through
/// (measured: it changes nothing). Sent from another thread the message is picked up at a message
/// pump instead, and the pass runs.
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

// ─────────────────────────── Window furniture ───────────────────────────

/// An undecorated window **has no native border resizing**; we add it back here.
///
/// The approach is the same as egui's built-in `Resize` container: bury one transparent hot-zone
/// using `Sense::drag` along each of the four edges and four corners; on press it sends a single
/// [`ViewportCommand::BeginResize`], and from then on the whole drag is taken over by the system
/// (not us changing the window size every frame — that would both stutter and fight the system).
///
/// Order matters: callers declare these **last** in their body, and egui's hit testing is
/// "later-declared wins within a layer", so the outermost few pixels go to resizing rather than
/// top-bar dragging. Overlap happens only at the top bar's edge; the buttons are a dozen pixels
/// from the edge and are not stolen.
pub fn resize_handles(ui: &mut Ui, ctx: &egui::Context, full: Rect) {
    // A maximized window should not be edge-draggable (Windows itself works this way), which also
    // avoids the oddity of "dragging along and falling out of the maximized state".
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }

    let zones = resize_zones(full);
    // The cursor must change on **hover** (without waiting for press), otherwise the user has no
    // idea this is draggable.
    let hover = ctx.input(|i| i.pointer.hover_pos());
    if let Some(p) = hover {
        if let Some((_, _, d)) = zones.iter().find(|(_, r, _)| r.contains(p)) {
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

/// Geometry of the eight hot-zones. The four edges take only the segment **with the corners
/// removed**, and each corner gets its own square zone — otherwise a corner would hit two zones at
/// once and the direction couldn't be determined.
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

/// There is no dividing line between an undecorated window and the desktop; in a light theme you
/// can barely tell where the window ends. Draw a 1px outline to frame it (a touch heavier than the
/// top bar's divider, so it holds against the background).
///
/// The corners must follow the system's: the window is ultimately clipped to rounded corners by
/// DWM, and drawing square corners here would leave the corner few pixels of border clipped,
/// producing a square break in the corner.
pub fn outline(ui: &Ui, full: Rect) {
    ui.painter().rect_stroke(
        full.shrink(0.5),
        CornerRadius::same(CORNER_RADIUS),
        Stroke::new(1.0, P::border_strong()),
        egui::StrokeKind::Inside,
    );
}

/// Let the system draw rounded corners on this window.
///
/// **Why not draw them ourselves** (a viewport with `with_transparent` + a rounded rect): the
/// corners can be drawn, but everything outside the border must be truly transparent, so the
/// **system shadow is lost with them** — the window becomes a sticker slapped on the desktop, and
/// transparent windows tend to flicker when dragged or resized.
///
/// Yet winit's undecorated window is not `WS_POPUP`: it uses `WM_NCCALCSIZE` to squash the
/// non-client area to 0 while keeping `WS_BORDER` / `WS_SYSMENU` — so in the system's eyes this is
/// still an **ordinary overlapping window**, and going through DWM gets us antialiased corners
/// while keeping the shadow intact, with a radius that naturally matches the main window (exactly
/// the thing we lost when we removed the system title bar).
///
/// winit actually ships this switch (`WindowAttributesExtWindows::with_corner_preference`), but
/// egui's `ViewportBuilder` has no passthrough for it, and child viewports don't even have a hook.
/// So here we just call the system API once: three functions, a dozen-ish lines of declarations;
/// not worth pulling in windows-sys for this.
///
/// It only needs to be called **once** after the window is built — rounded corners are a state hung
/// on the HWND, not a per-frame thing. Every `reason` below is created with `false` on the
/// "do once" flag by [`Gwin::forget_placement`], so the window rebuilds them.
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
    ///
    /// SAFETY: `h` is a handle passed by `EnumWindows` and is valid for the duration of this call.
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

// ───────────────────────────── Top bar ─────────────────────────────

#[derive(Clone, Copy)]
pub enum Glyph {
    Close,
    Min,
    /// Maximize / restore. `on` = currently maximized (so it is drawn as the "restore" double frame)
    Max { on: bool },
    Pin { on: bool },
}

/// The self-drawn top bar: breathing dot, title, state text, and the four window buttons.
///
/// `info` is the caller's one-line state readout (it follows the title) — the shell has no idea
/// what a given window is busy with, so the text is built outside.
pub fn header(ui: &mut Ui, ctx: &egui::Context, gw: &mut Gwin, title: &str, info: &str) {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), HEADER_H), Sense::click_and_drag());
    // StartDrag must be sent on the **very frame** the mouse is pressed; a frame late and the
    // window won't follow.
    if resp.drag_started() {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
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
    // The state text starts **after the title**, measured rather than at a fixed offset: the title
    // is a translated string, and the English one (18pt bold) is more than twice as wide as the
    // Chinese — a hard-coded offset had the two running straight through each other.
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

    // Laid out right-to-left: close / maximize / minimize / pin, which lands on screen as Windows'
    // usual "pin · minimize · maximize · close". Since the system title bar is gone, these
    // self-drawn buttons are the window's only way out.
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
    let maxed = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
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
        ui.painter().rect_filled(r, CornerRadius::same(8), P::surface_hi());
    }
    // "Pinned" is a **persistent state** and must stay visible: give it the deepest foreground
    // colour even when not hovered, otherwise a grey pushpin on a grey background leaves the user
    // unable to tell whether it is pinned.
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
            //
            // Pinned and unpinned **share the same shape**, differing only by a 90° orientation —
            // unpinned it lies down (tip toward lower-left), pinned it stands up (tip pointing
            // down). So the two states aren't "a different picture" but the same pin lying down vs
            // standing up, and the user knows the state changed without having to re-learn an icon.
            //
            // Fully filled, no stroking: thin lines are genuinely invisible on this background.
            let rot = |v: Vec2| if on { v } else { Vec2::new(-v.y, v.x) };
            // After a 90° rotation a rect's length and width swap, so the dimensions need a
            // separate swap.
            let swap = |v: Vec2| if on { v } else { Vec2::new(v.y, v.x) };

            // Head
            p.rect_filled(
                Rect::from_center_size(c + rot(Vec2::new(0.0, -4.2)), swap(Vec2::new(10.0, 2.6))),
                CornerRadius::same(1),
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

// ───────────────────────── Small shared widgets ─────────────────────────

/// A thin vertical separator, for toolbars.
pub fn vsep(ui: &mut Ui) {
    ui.add_space(6.0);
    let (r, _) = ui.allocate_exact_size(Vec2::new(1.0, 20.0), Sense::hover());
    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, P::border()));
    ui.add_space(6.0);
}

/// A pill button. `on` = the currently selected one (reversed).
///
/// Same hover language as every other control in the app: the fill steps up to `surface_hi` and the
/// outline steps to `edge` — no second set of greys for the sub-windows.
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
        CornerRadius::same(10),
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
            CornerRadius::same(10),
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

// ─────────────────────────── Initial geometry ───────────────────────────

/// Initial size = **the main window's default size** (the same formula, see
/// `app::default_window_size`). Callers may scale it down for a window whose content wants less
/// room, but the base is shared so the windows are visibly siblings.
///
/// `default_window_size` subtracts "the thickness decoration takes up" (title bar + border), and
/// what it computes is the **content area** size. These windows themselves have no decoration, so
/// their `outer - inner` is always 0 — computing with it would add 46 points of height out of
/// nowhere. So here we must ask the **main window** for the decoration thickness, for the two
/// content areas to line up.
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

/// Initial position: centered on **the same monitor** as the main window. When the window is as
/// large as the main one, placing it beside is bound to not fit, so it is better to lay it straight
/// over the middle and let the user move it.
///
/// Note `monitor_size` gives a **size**; egui does not expose the monitor origin, so we can only
/// compute by "monitor top-left = screen origin" — correct on the primary monitor, degrading on a
/// secondary one to "placed at the middle of the primary monitor", but at least it is not pushed
/// off-screen.
fn initial_pos(ctx: &egui::Context, size: Vec2) -> Pos2 {
    let mon_size = ctx.input(|i| i.viewport().monitor_size).unwrap_or(Vec2::new(1920.0, 1080.0));
    let mon = Rect::from_min_size(Pos2::ZERO, mon_size);
    let clamp = |v: f32, lo: f32, hi: f32| v.clamp(lo, hi.max(lo));
    Pos2::new(
        clamp(mon.center().x - size.x / 2.0, 8.0, mon.max.x - size.x - 8.0),
        clamp(mon.center().y - size.y / 2.0, 8.0, mon.max.y - size.y - 8.0),
    )
}
