//! The first-screen background: one full-bleed image drawn behind the empty state, with a pool per theme; a new...
//! # Where the images come from.
//! Read from disk at runtime: `<app root>/assets/light/*.png` and `<app root>/assets/dark/*.png` (`app root`...

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::{SystemTime, UNIX_EPOCH};

use egui::{Color32, Context, Pos2, Rect, TextureHandle, Ui};

use aox::image::Bitmap;

use crate::theme;
use crate::tex;

/// The fallback pool, embedded. Used when `assets/<theme>/` is missing, empty, or unusable.
const LIGHT_FALLBACK: &[u8] = include_bytes!("../../assets/light.png");
const DARK_FALLBACK: &[u8] = include_bytes!("../../assets/dark.png");

/// The program icon. It is the same file that `build.rs` embeds as an exe resource; embedding a second copy here...
const ICO: &[u8] = include_bytes!("../../assets/aox.ico");

/// The program icon: used by the title-bar top-left, the taskbar, and Alt+Tab.
pub fn window_icon() -> egui::IconData {
    let frames = ico_frames(ICO);
    let want = theme::system_icon_size().max(1) as u16;
    let pick = frames
        .iter()
        .find(|(size, _)| *size == want)
        .or_else(|| frames.iter().max_by_key(|(size, _)| *size))
        .map(|(_, png)| *png);

    let Some(png) = pick else {
        return invisible_icon();
    };
    match aox::image::decode_bytes(png) {
        Ok(bm) => egui::IconData {
            rgba: bm.rgba,
            width: bm.width as u32,
            height: bm.height as u32,
        },
        Err(_) => invisible_icon(),
    }
}

/// A 1×1 fully transparent icon -- the fallback for "there should be no icon here".
fn invisible_icon() -> egui::IconData {
    egui::IconData { rgba: vec![0, 0, 0, 0], width: 1, height: 1 }
}

/// The structure is simple: a 6-byte directory header (reserved / type / frame count), then a 16-byte directory...
fn ico_frames(ico: &[u8]) -> Vec<(u16, &[u8])> {
    let Some(head) = ico.get(0..6) else { return Vec::new() };
    let count = u16::from_le_bytes([head[4], head[5]]) as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let e = 6 + i * 16;
        let Some(entry) = ico.get(e..e + 16) else { break };
        let size_of = |raw: u8| if raw == 0 { 256u16 } else { raw as u16 };
        let (w, h) = (size_of(entry[0]), size_of(entry[1]));
        if w != h {
            continue; // Non-square: never happens in this project; skipping beats guessing at an image
        }
        let len = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let off = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        if let Some(payload) = ico.get(off..off.saturating_add(len)) {
            out.push((w, payload));
        }
    }
    out
}

// the background pool

/// The two pools, relative to each candidate base.
const LIGHT_DIR: &str = "assets/light";
const DARK_DIR: &str = "assets/dark";

/// The bases searched, in order: the app root first (the canonical answer -- the nearest `Cargo.toml`...
fn bases() -> Vec<PathBuf> {
    let mut out = vec![aox::paths::app_root()];
    if let Ok(cwd) = std::env::current_dir() {
        if !out.contains(&cwd) {
            out.push(cwd);
        }
    }
    out
}

/// PNG only, case-insensitively. The folder is a "pool of backgrounds", not a general image
/// directory: accepting whatever is in there would quietly widen what the decoder gets handed.
fn is_png(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"))
}

/// One theme's pool: every `*.png` under the first base that has any, sorted by path.
fn pool_in(dir: &str) -> Vec<PathBuf> {
    for base in bases() {
        let Ok(entries) = std::fs::read_dir(base.join(dir)) else { continue };
        let mut found: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_png(p))
            .collect();
        if !found.is_empty() {
            found.sort();
            return found;
        }
    }
    Vec::new()
}

/// A counter mixed into the seed.
static ROLL: AtomicU64 = AtomicU64::new(0);

/// A random index in `0..n` (`0` when `n <= 1`).
fn roll(n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos ^ ROLL.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    if x == 0 {
        // xorshift has a fixed point at 0 -- the one seed it can never leave
        x = 0x9E37_79B9_7F4A_7C15;
    }
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as usize) % n
}

/// An in-place Fisher-Yates shuffle (dependency-free, see [`roll`]).
fn shuffle(v: &mut [PathBuf]) {
    for i in (1..v.len()).rev() {
        v.swap(i, roll(i + 1));
    }
}

/// Load one image: walk the (already shuffled) pool in order, then the embedded fallback.
fn load(pool: Vec<PathBuf>, fallback: &'static [u8]) -> Result<Bitmap, String> {
    let mut tried: Vec<String> = Vec::new();
    for p in &pool {
        match aox::image::decode(p) {
            Ok(bm) => return Ok(bm),
            Err(e) => tried.push(format!("{}: {e}", p.display())),
        }
    }
    match aox::image::decode_bytes(fallback) {
        Ok(bm) => Ok(bm),
        Err(e) => Err(if tried.is_empty() {
            e
        } else {
            format!("{} (built-in: {e})", tried.join("; "))
        }),
    }
}

/// The UV window that makes `iw × ih` cover a `rw × rh` rect: the shorter edge crops, the longer one maps to 0..1.
fn cover_uv(rw: f32, rh: f32, iw: f32, ih: f32) -> [f32; 4] {
    let scale = (rw / iw).max(rh / ih);
    let uw = ((rw / scale) / iw).min(1.0);
    let uh = ((rh / scale) / ih).min(1.0);
    let (du, dv) = ((1.0 - uw) * 0.5, (1.0 - uh) * 0.5);
    [du, dv, 1.0 - du, 1.0 - dv]
}

/// The life of one theme's background.
enum Slot {
    /// Nothing chosen yet; [`Art::poll`] starts the decode
    Idle,
    Loading(Receiver<Result<Bitmap, String>>),
    Ready { tex: TextureHandle, w: u32, h: u32 },
    /// Nothing decodable -- neither the pool nor the built-in pair.
    Failed(String),
}

/// The first-screen background (`[light, dark]`, one pool each).
pub struct Art {
    /// Off means off: the folders are not scanned, nothing is decoded, and any texture already uploaded is dropped.
    enabled: bool,
    /// The sources found on disk, so a re-roll can shuffle them again without touching the disk.
    pools: [Vec<PathBuf>; 2],
    slots: [Slot; 2],
}

impl Art {
    pub fn new() -> Self {
        let mut me =
            Self { enabled: true, pools: [Vec::new(), Vec::new()], slots: [Slot::Idle, Slot::Idle] };
        me.reroll();
        me
    }

    /// Turn the feature on or off. Cheap and idempotent: the settings page calls it whenever the
    /// switch is drawn, which is every frame.
    pub fn set_enabled(&mut self, on: bool) {
        if self.enabled == on {
            return;
        }
        self.enabled = on;
        if on {
            // Start from a clean pool: the folders may have gained a PNG while the feature was off.
            self.reroll();
        } else {
            self.pools = [Vec::new(), Vec::new()];
            // Dropping `Ready` releases the textures; `Idle` is what "nothing to show" means.
            self.slots = [Slot::Idle, Slot::Idle];
        }
    }

    /// Pick fresh images for both themes -- at startup, and every time a new conversation starts.
    pub fn reroll(&mut self) {
        self.pools = [pool_in(LIGHT_DIR), pool_in(DARK_DIR)];
        for i in 0..2 {
            if self.pools[i].is_empty() && !matches!(self.slots[i], Slot::Idle) {
                continue;
            }
            self.slots[i] = Slot::Idle;
        }
    }

    /// Once per frame: kick off decodes that should start, collect results that should be collected.
    pub fn poll(&mut self, ctx: &Context) {
        if !self.enabled {
            // Nothing to decode and nothing to collect: staying out of the frame entirely is also
            // what keeps a disabled feature from asking for repaints.
            return;
        }
        let mut waiting = false;
        for i in 0..2 {
            // Take the slot out first and write it back: `match &mut self.slotsi` followed by writing `self.slotsi` inside...
            let slot = std::mem::replace(&mut self.slots[i], Slot::Idle);
            let next = match slot {
                Slot::Idle => {
                    let mut pool = self.pools[i].clone();
                    shuffle(&mut pool);
                    let fallback =
                        if i == 0 { LIGHT_FALLBACK } else { DARK_FALLBACK };
                    let (tx, rx) = channel();
                    // Decoding goes through GDI+ and is synchronous (see `aox::image`); don't block the UI thread
                    std::thread::spawn(move || {
                        let _ = tx.send(load(pool, fallback));
                    });
                    waiting = true;
                    Slot::Loading(rx)
                }
                Slot::Loading(rx) => match rx.try_recv() {
                    Ok(Ok(bm)) => {
                        let (w, h) = (bm.width as u32, bm.height as u32);
                        Slot::Ready { tex: tex::upload(ctx, format!("aox-bg:{i}"), &bm), w, h }
                    }
                    Ok(Err(e)) => Slot::Failed(e),
                    Err(TryRecvError::Empty) => {
                        waiting = true;
                        Slot::Loading(rx)
                    }
                    Err(TryRecvError::Disconnected) => Slot::Failed(
                        aox::i18n::t("the decoder thread ended prematurely").into(),
                    ),
                },
                other => other,
            };
            self.slots[i] = next;
        }
        // Still decoding → request the next frame: egui only repaints on input, so without
        // asking it would stay frozen at "not done" forever
        if waiting {
            ctx.request_repaint();
        }
    }

    /// Paint the current theme's image full-bleed over `rect`, and hand back a line to show when nothing could...
    pub fn show_background(&self, ui: &Ui, rect: Rect) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let dark = theme::mode() == theme::Mode::Dark;
        match &self.slots[if dark { 1 } else { 0 }] {
            Slot::Ready { tex, w, h } => {
                let (iw, ih) = (*w as f32, *h as f32);
                if iw > 0.0 && ih > 0.0 && rect.width() > 0.0 && rect.height() > 0.0 {
                    let [u0, v0, u1, v1] = cover_uv(rect.width(), rect.height(), iw, ih);
                    ui.painter().image(
                        tex.id(),
                        rect,
                        Rect::from_min_max(Pos2::new(u0, v0), Pos2::new(u1, v1)),
                        // No tinting: what's drawn is the image as-is (the theme difference comes
                        // from **swapping the pool**, not a filter)
                        Color32::WHITE,
                    );
                }
                None
            }
            Slot::Failed(e) => Some(e.clone()),
            // Still decoding: draw nothing and say nothing. A complaint that appears for two frames
            // on every start would be worse than the wait.
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ICO directory must be parseable: every frame has to yield a real payload, and the "0 means 256" rule has...
    #[test]
    fn every_ico_frame_is_reachable() {
        let frames = ico_frames(ICO);
        assert!(frames.len() >= 5, "the embedded ico should have several sizes, but actually has {}", frames.len());
        for (size, payload) in &frames {
            assert!((16..=256).contains(size), "unreasonable size: {size}");
            assert!(
                payload.starts_with(&[0x89, b'P', b'N', b'G']),
                "the {size}×{size} frame is not a PNG payload"
            );
        }
        // A 0 in a directory entry means 256: it must be recognized, not dropped as "0×0"
        assert!(frames.iter().any(|(s, _)| *s == 256), "the 256 frame was not recognized");
        // 32 is `window_icon`'s fallback size, so it must be present
        assert!(frames.iter().any(|(s, _)| *s == 32), "there is no 32×32 frame");
    }

    /// The pool only takes PNGs, and the extension check is what decides it -- a stray `.jpg` or a
    /// file with no extension must not reach the decoder.
    #[test]
    fn only_png_reaches_the_pool() {
        for yes in ["a.png", "B.PNG", "c.PnG"] {
            assert!(is_png(Path::new(yes)), "{yes} should count as a PNG");
        }
        for no in ["a.jpg", "a.png.bak", "png", "a.jpeg", "a"] {
            assert!(!is_png(Path::new(no)), "{no} must not count as a PNG");
        }
    }

    /// `roll` has to stay inside its range, and two consecutive calls must be able to differ -- the whole feature...
    #[test]
    fn roll_stays_in_range_and_varies() {
        assert_eq!(roll(0), 0, "an empty pool has one answer, not a panic");
        assert_eq!(roll(1), 0);
        let n = 7;
        let picks: Vec<usize> = (0..64).map(|_| roll(n)).collect();
        assert!(picks.iter().all(|&i| i < n), "an index out of range would panic on the real pool");
        let mut distinct = picks.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(distinct.len() > 1, "64 draws from 7 choices all landed on {picks:?}");
    }

    /// A shuffle must be a permutation: it may reorder, it must never lose or duplicate an entry
    /// (a dropped background is a silently smaller pool).
    #[test]
    fn shuffle_keeps_every_entry() {
        let mut v: Vec<PathBuf> = (0..8).map(|i| PathBuf::from(format!("{i}.png"))).collect();
        let before = v.clone();
        shuffle(&mut v);
        assert_eq!(v.len(), before.len());
        let (mut a, mut b) = (v.clone(), before.clone());
        a.sort();
        b.sort();
        assert_eq!(a, b, "a shuffle must be a permutation");
    }

    /// "Cover" means exactly one axis maps to the full 0..1 window and the other is cropped, centred.
    #[test]
    fn cover_uv_crops_the_shorter_axis() {
        // A wide image in a square rect: the *width* crops, the height is used in full
        let [u0, v0, u1, v1] = cover_uv(100.0, 100.0, 200.0, 100.0);
        assert!((v0 - 0.0).abs() < 1e-6 && (v1 - 1.0).abs() < 1e-6, "height must be untouched: {v0}..{v1}");
        assert!(u0 > 0.0 && u1 < 1.0, "width must be cropped: {u0}..{u1}");
        assert!((u0 - (1.0 - u1)).abs() < 1e-6, "the crop must be centred, not left- or right-aligned");

        // The other way round: a square image in a wide rect crops *vertically*
        let [u0, v0, u1, v1] = cover_uv(200.0, 100.0, 100.0, 100.0);
        assert!((u0 - 0.0).abs() < 1e-6 && (u1 - 1.0).abs() < 1e-6, "width must be untouched: {u0}..{u1}");
        assert!(v0 > 0.0 && v1 < 1.0, "height must be cropped: {v0}..{v1}");

        // Exactly matching aspect ratios are the degenerate case: nothing to crop
        let [u0, v0, u1, v1] = cover_uv(300.0, 200.0, 630.0, 420.0);
        assert!((u0 - 0.0).abs() < 1e-6 && (v0 - 0.0).abs() < 1e-6, "same aspect: {u0} {v0}");
        assert!((u1 - 1.0).abs() < 1e-6 && (v1 - 1.0).abs() < 1e-6, "same aspect: {u1} {v1}");
    }
}
