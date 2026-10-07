//! The first-screen illustration: one set for light, one for dark; pick whichever matches the
//! current theme and draw it centered in the column.
//!
//! # Why it's embedded
//!
//! The two images come from `include_bytes!`, not read from `assets/` — after install there is
//! **just one exe**, with no need for "assets dir still in place, permissions intact, not moved
//! by antivirus". The cost is exe size: the two PNGs total 363 KB (see the size budget in
//! `docs/ARCH-ROADMAP.md`, which records this item specifically).
//!
//! # Why two sets of images
//!
//! Both illustrations are **transparent black line art**, so they sit "on the UI background"
//! rather than being a self-backed image — light and dark backgrounds each need a set (the user
//! supplied a matching pair on 2026-10-06). Hence **switching theme only swaps the texture**,
//! no re-decode needed.

use std::sync::mpsc::{channel, Receiver, TryRecvError};

use egui::{Color32, Context, Pos2, Rect, RichText, Sense, TextureHandle, Ui, Vec2};

use aox::image::Bitmap;

use crate::theme::{self, P};
use crate::tex;

/// The illustration assets. Both are 630×420 transparent-background PNGs.
const LIGHT: &[u8] = include_bytes!("../../assets/light.png");
const DARK: &[u8] = include_bytes!("../../assets/dark.png");

/// The program icon. It is the **same file** that `build.rs` embeds as an exe resource; embedding
/// a second copy here is to take its pixels and feed the window (see [`window_icon`] for why).
/// 7 frames, 2.8 KB — the cost of embedding it twice is negligible.
const ICO: &[u8] = include_bytes!("../../assets/aox.ico");

/// The window icon: used by the title-bar top-left, the taskbar, and Alt+Tab.
///
/// # Why not just let Windows use the copy in the exe resource
///
/// That would have been the ideal route (every size could take a native frame, no resampling
/// needed at all): `.with_icon(IconData::default())` makes eframe **skip** its own default icon
/// (the black-on-white "e" the user reported on 2026-10-06). **But that route does not work in
/// practice** — eframe skips it, yet winit still receives that 0×0 `IconData` and installs an
/// invalid HICON on the window, so the title bar paints a system generic icon (a grey "photo/
/// window" block, more confusing than the "e"). So we have to feed it **real pixels**.
///
/// # Why this particular size
///
/// `IconData` carries only **one** size, and eframe scales it once each for `SM_CXICON` /
/// `SM_CXSMICON`. Take the frame from [`theme::system_icon_size`] (that is, `SM_CXICON`): the
/// larger bracket is 1:1 and the smaller one is exactly 2:1 — both clean. Picking the largest 256
/// would not do: scaling down to 16 is 16:1, and the bitmap smears into a blob.
pub fn window_icon() -> egui::IconData {
    let frames = ico_frames(ICO);
    let want = theme::system_icon_size().max(1) as u16;
    let pick = frames
        .iter()
        .find(|(size, _)| *size == want)
        // No exact bracket → fall back to the largest: better slightly blurry than no icon
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
        // Can't decode → hand back a fully transparent one: better no icon in the title bar than a "probably something" icon standing there
        Err(_) => invisible_icon(),
    }
}

/// A 1×1 fully transparent icon — the fallback for "there should be no icon here".
fn invisible_icon() -> egui::IconData {
    egui::IconData { rgba: vec![0, 0, 0, 0], width: 1, height: 1 }
}

/// Split an ICO into "edge length → that frame's raw payload".
///
/// The structure is simple: a 6-byte directory header (reserved / type / frame count), then a
/// 16-byte directory entry per frame, with the payloads following after.
/// ⚠️ Width and height each occupy **one byte** in a directory entry, so **0 means 256** (256
/// doesn't fit in a byte) — this is exactly where "grab the 256 frame" most easily fails silently.
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

/// The widest the illustration may be drawn, in **logical points**.
///
/// This number is a trade-off: any wider and it overpowers the input box, turning the first
/// screen into a poster; any narrower and you can't make out what's drawn.
/// ⚠️ Don't casually shrink it: the 630 px source image is exactly 1:1 on a 150%-scaled screen
/// (420 × 1.5 = 630), and shrinking turns it into upscaled rendering where the line-art edges blur.
const MAX_W: f32 = 420.0;

/// How tall the illustration occupies at a given width (logical points).
///
/// The aspect ratio is **read from the PNG header**, not queried from the texture after decoding —
/// the first screen must reserve its space before the image finishes decoding, otherwise "the
/// moment it decodes, the whole block of content jumps down".
pub fn height(dark: bool, max_w: f32) -> f32 {
    let (w, h) = ihdr(if dark { DARK } else { LIGHT });
    max_w.min(MAX_W) * h / w
}

/// Read `(width, height)` straight from the PNG bytes.
///
/// A PNG's IHDR always follows "8-byte signature + 4-byte length + 4-byte `IHDR`", with width
/// and height each 4 bytes big-endian.
fn ihdr(png: &[u8]) -> (f32, f32) {
    let be = |i: usize| {
        u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as f32
    };
    (be(16), be(20))
}

/// The life of one illustration.
enum Slot {
    /// Not started decoding yet (only the first frame kicks off a thread)
    Idle,
    Loading(Receiver<Result<Bitmap, String>>),
    Ready(TextureHandle),
    /// Can't decode. **No retry** — it's a fixed blob of bytes from compile time; if this machine
    /// can't decode it, that's that, and redoing it every frame just burns CPU for nothing. Leave a
    /// line of text on screen so the first screen isn't a wordless blank block.
    Failed(String),
}

/// The first-screen illustration (`[light, dark]`, two sets).
pub struct Art {
    slots: [Slot; 2],
}

impl Art {
    pub fn new() -> Self {
        Self { slots: [Slot::Idle, Slot::Idle] }
    }

    /// Once per frame: kick off threads that should start, collect results that should be collected.
    ///
    /// Both images decode **together**, rather than "decode whichever one gets used": a 630×420
    /// decode is millisecond-scale, whereas "start decoding only the moment the theme switches"
    /// leaves the first few frames blank after the switch — far worse than a few extra milliseconds.
    pub fn poll(&mut self, ctx: &Context) {
        let mut waiting = false;
        for (i, src) in [LIGHT, DARK].into_iter().enumerate() {
            // Take the slot out first and write it back: `match &mut self.slots[i]` followed by
            // writing `self.slots[i]` inside a branch would borrow it twice (egui people call this
            // "borrow gymnastics"; here `replace` sidesteps it in one step).
            let slot = std::mem::replace(&mut self.slots[i], Slot::Idle);
            self.slots[i] = match slot {
                Slot::Idle => {
                    let (tx, rx) = channel();
                    // Decoding is synchronous and goes through GDI+ (see `aox::image`); don't block the UI thread
                    std::thread::spawn(move || {
                        let _ = tx.send(aox::image::decode_bytes(src));
                    });
                    waiting = true;
                    Slot::Loading(rx)
                }
                Slot::Loading(rx) => match rx.try_recv() {
                    Ok(Ok(bm)) => Slot::Ready(tex::upload(ctx, format!("aox-art:{i}"), &bm)),
                    Ok(Err(e)) => Slot::Failed(e),
                    Err(TryRecvError::Empty) => {
                        waiting = true;
                        Slot::Loading(rx)
                    }
                    Err(TryRecvError::Disconnected) => Slot::Failed(aox::i18n::t("the decoder thread ended prematurely").into()),
                },
                other => other,
            };
        }
        // Still decoding → request the next frame: egui only repaints on input, so without
        // asking it would stay frozen at "not done" forever
        if waiting {
            ctx.request_repaint();
        }
    }

    /// Lay the illustration out in `ui` according to the current theme (horizontal centering is the
    /// caller's job), returning the height it occupies.
    ///
    /// If nothing was drawn (still decoding / can't decode) return `0.0` — the caller uses that to
    /// decide its vertical-centering formula, rather than "reserving space for a height that hasn't arrived".
    pub fn show(&mut self, ui: &mut Ui, max_w: f32) -> f32 {
        let dark = theme::mode() == theme::Mode::Dark;
        let slot = &self.slots[if dark { 1 } else { 0 }];
        match slot {
            Slot::Ready(tex) => {
                // The size uses the same number [`height`] computed, not the texture's own pixel
                // count — computing it in two places would make "the reserved height" and "the drawn
                // height" differ slightly, and that difference only shows up in vertical centering
                // (drawn half a line off).
                let size = Vec2::new(max_w.min(MAX_W), height(dark, max_w));
                let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
                ui.painter().image(
                    tex.id(),
                    rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    // No tinting: what's drawn is the art as-is (theme difference comes from
                    // **swapping the image**, not a filter)
                    Color32::WHITE,
                );
                size.y
            }
            Slot::Failed(e) => {
                ui.label(
                    RichText::new(aox::tf!("cannot show the illustration: {}", e))
                        .color(P::text_faint())
                        .size(13.0),
                );
                0.0
            }
            _ => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ICO directory must be parseable: every frame has to yield a real payload, and the
    /// "0 means 256" rule has to hold.
    ///
    /// Why it's worth pinning: when `window_icon` can't find a frame it **silently** falls back to
    /// a fully transparent icon — no error, no crash, just a vanished title-bar icon. So someone has
    /// to watch over the "can it be found" question.
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
}
