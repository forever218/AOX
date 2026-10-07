//! Decode local image files into pixels — for the UI to **display** (images in attachments,
//! images produced by plugins).
//!
//! # Why GDI+ rather than adding a decoder dependency
//!
//! This project is disciplined about binary size (see the size baselines in
//! `docs/ARCH-ROADMAP.md`), and "display one image" is worth very little:
//!
//! | Approach | Cost | Formats supported |
//! |---|---|---|
//! | the `image` crate | about +300~500 KB | everything |
//! | the `png` crate | about +100 KB | PNG only |
//! | **GDI+** (this module) | **0 bytes** (gdiplus.dll ships with the system) | PNG / JPEG / GIF / BMP / TIFF … |
//!
//! This project only ships a Windows build anyway, and already uses `windows-sys` to talk to
//! system capabilities directly (the Job Object fence, the DWM title bar, local time), so
//! "ask the system for it" is far more consistent than "stuff in a decoder".
//!
//! ⚠️ The GDI+ API is **stateful**: `GdiplusStartup` must succeed once, and **it must not be
//! shut down and started again mid-flight** (the official docs explicitly don't support a
//! shutdown→startup cycle). So it starts once here and stays resident; the process exit is
//! left to the system to reclaim — see [`ensure_started`].
//!
//! ⚠️ Decoding is **synchronous**, and a 4000×3000 PNG takes tens of milliseconds. The UI
//! layer **must** call it on a background thread (that is what the decode queue in
//! `gui/src/app.rs` does), otherwise every image displayed costs a dropped frame.

use std::path::Path;

/// The decoded bitmap: RGBA8, row-major, **not premultiplied** (which is exactly what egui's
/// `ColorImage` wants).
pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    /// `width * height * 4` bytes
    pub rgba: Vec<u8>,
}

/// The maximum pixels in one image.
///
/// What it stops is "an image claiming to be 50000×50000": that would make us allocate 10 GB in
/// one go. GDI+ has its own refusal for oversized images, but that's its business; what we must
/// protect is **our own allocation here**.
/// 24 MPix ≈ 96 MB of RGBA, roomier than any real screenshot/photo (a 6000×4000 camera photo is
/// exactly 24 MPix).
const MAX_PIXELS: usize = 24_000_000;

/// Decode an image file. A failure yields **one sentence meant for humans** (it gets shown in
/// the conversation).
pub fn decode(path: &Path) -> Result<Bitmap, String> {
    if !path.is_file() {
        return Err(crate::tf!("the image file is gone: {}", path.display()));
    }
    platform::decode(path)
}

/// Decode a slice of image bytes held in memory.
///
/// For **embedded resources** (the first-screen illustration comes in via `include_bytes!` and
/// has no file path). It goes through the same GDI+ as [`decode`], with one extra step in front:
/// "wrap this memory into an `IStream`".
pub fn decode_bytes(bytes: &[u8]) -> Result<Bitmap, String> {
    if bytes.is_empty() {
        return Err(crate::i18n::t("the embedded image is empty").into());
    }
    platform::decode_bytes(bytes)
}

/// Copy GDI+'s BGRA buffer into RGBA.
///
/// Pulled out on its own (rather than written at the call site) because it is the part of the
/// whole decode **most likely to go quietly wrong**: the channel order, the difference between
/// `stride` and "width × 4", and `stride` possibly being negative (bottom-up bitmaps). None of
/// these errors report anything; they just make "the image displays but the colours are wrong" —
/// the kind of bug easily shrugged off as "that's how the image is", so it deserves a direct
/// assertion.
///
/// # Safety
///
/// `src` must point to a block of at least "`height` rows, each readable for `stride.abs()` bytes".
unsafe fn bgra_to_rgba(width: usize, height: usize, stride: i32, src: *const u8) -> Bitmap {
    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        // Multiply as `isize`: when `stride` is negative (bottom-up), this step must walk
        // backwards
        let row = src.offset(y as isize * stride as isize);
        let dst = &mut rgba[y * width * 4..(y + 1) * width * 4];
        for x in 0..width {
            let p = row.add(x * 4);
            // GDI+'s `PixelFormat32bppARGB` is B,G,R,A in memory on a little-endian machine
            let (b, g, r, a) = (*p, *p.add(1), *p.add(2), *p.add(3));
            dst[x * 4] = r;
            dst[x * 4 + 1] = g;
            dst[x * 4 + 2] = b;
            dst[x * 4 + 3] = a;
        }
    }
    Bitmap { width, height, rgba }
}

// ─────────────────────────── Windows: GDI+ ───────────────────────────

#[cfg(windows)]
mod platform {
    use super::{bgra_to_rgba, Bitmap, MAX_PIXELS};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::OnceLock;
    use windows_sys::Win32::Graphics::GdiPlus::{
        GdipBitmapLockBits, GdipBitmapUnlockBits, GdipCreateBitmapFromFile,
        GdipCreateBitmapFromStream, GdipDisposeImage, GdipGetImageHeight, GdipGetImageWidth,
        GdiplusStartup, GdiplusStartupInput, GdiplusStartupOutput, BitmapData, GpBitmap, GpImage,
        ImageLockModeRead, Rect,
    };
    use windows_sys::Win32::UI::Shell::SHCreateMemStream;

    /// GDI+'s `Ok` status code. **Deliberately not writing `use ...GdiPlus::*`**: that module
    /// also exports an `Ok` (a status-code constant), and glob-importing it would shadow
    /// `Result::Ok` — making every `Ok(x)` in this file a type error whose message points
    /// elsewhere, hard to trace back to the import.
    const GP_OK: i32 = 0;

    /// `PixelFormat32bppARGB`. `windows-sys` **does not export** this constant (it lives in the
    /// `windows` crate), so the value is taken from `Gdipluspixelformats.h`:
    /// `PIXEL_FORMAT_32BPP_ARGB = 0x0026200A`.
    const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x0026_200A;

    /// The startup result is computed only once.
    ///
    /// ⚠️ **`GdiplusShutdown` is never called**: the official docs state "GDI+ cannot be started
    /// again after a shutdown" (the second attempt fails), and this is a GUI process where
    /// decoding keeps happening — shutting it down would buy us "images can never be decoded
    /// again". Letting the system reclaim it at process exit is the standard practice for a
    /// one-shot global library like this.
    static START: OnceLock<Result<(), String>> = OnceLock::new();

    fn ensure_started() -> Result<(), String> {
        START
            .get_or_init(|| {
                let input = GdiplusStartupInput {
                    GdiplusVersion: 1,
                    DebugEventCallback: 0,
                    SuppressBackgroundThread: 0,
                    SuppressExternalCodecs: 0,
                };
                let mut token: usize = 0;
                let mut out: GdiplusStartupOutput = unsafe { std::mem::zeroed() };
                // SAFETY: both out-params are variables on this function's stack, and their
                // lifetime covers this call.
                let st = unsafe { GdiplusStartup(&mut token, &input, &mut out) };
                if st != GP_OK {
                    Err(crate::tf!("GDI+ failed to initialize (status {})", st))
                } else {
                    Ok(())
                }
            })
            .clone()
    }

    pub fn decode(path: &Path) -> Result<Bitmap, String> {
        ensure_started()?;
        // GDI+ only understands UTF-16 paths, and they must carry a trailing 0
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

        let mut img: *mut GpBitmap = std::ptr::null_mut();
        // SAFETY: `wide` is a valid NUL-terminated UTF-16 string; the out-param is a stack
        // pointer.
        let st = unsafe { GdipCreateBitmapFromFile(wide.as_ptr(), &mut img) };
        if st != GP_OK || img.is_null() {
            return Err(crate::tf!(
                "cannot decode this image (GDI+ status {}): {}",
                st,
                path.display()
            ));
        }
        let out = read_pixels(img);
        // SAFETY: `img` is the bitmap just created above and not yet released
        unsafe { GdipDisposeImage(img as *mut GpImage) };
        out
    }

    /// Release a COM interface — the only capability we need is `IUnknown::Release`.
    ///
    /// ⚠️ **`windows-sys` has no `IUnknown`** (that belongs to the `windows` crate), so we have
    /// to walk the vtable ourselves per the COM ABI. Doing so is defensible:
    /// `QueryInterface / AddRef / Release` are always the first three vtable slots, a hard COM
    /// convention that doesn't vary between interfaces.
    ///
    /// # Safety
    ///
    /// `p` must be a COM interface pointer that has not been released (or a null pointer).
    unsafe fn release(p: *mut core::ffi::c_void) {
        if p.is_null() {
            return;
        }
        let vtbl = *(p as *mut *const usize);
        let release: extern "system" fn(*mut core::ffi::c_void) -> u32 =
            std::mem::transmute(*vtbl.add(2));
        release(p);
    }

    /// Decode a slice of image bytes held in memory: first wrap it into an `IStream` with
    /// `SHCreateMemStream`, then hand it to GDI+.
    ///
    /// Why not drop a temp file and go through [`decode`] instead: that would leave a
    /// failure-prone intermediate artifact on disk (antivirus, temp-directory permissions,
    /// concurrently identical names), whereas what we decode here is a slice of bytes **fixed at
    /// compile time** — it isn't worth introducing the filesystem layer of risk for it.
    pub fn decode_bytes(bytes: &[u8]) -> Result<Bitmap, String> {
        ensure_started()?;
        let len = u32::try_from(bytes.len())
            .map_err(|_| crate::i18n::t("this embedded image is too large (over 4 GB)").to_string())?;
        // SAFETY: `bytes` stays valid for this whole call (a borrowed slice) and `len` is its
        // byte count.
        // ⚠️ `cbInit` must never be 0 — that means "pInit is a NUL-terminated string", and
        // image bytes containing a 0 are commonplace.
        let stream = unsafe { SHCreateMemStream(bytes.as_ptr(), len) };
        if stream.is_null() {
            return Err(crate::i18n::t("failed to create the memory stream").into());
        }

        let mut img: *mut GpBitmap = std::ptr::null_mut();
        // SAFETY: `stream` is the IStream just created above and not yet released; the out-param
        // is a stack pointer.
        let st = unsafe { GdipCreateBitmapFromStream(stream, &mut img) };
        let out = if st != GP_OK || img.is_null() {
            Err(crate::tf!("cannot decode this embedded image (GDI+ status {})", st))
        } else {
            let b = read_pixels(img);
            // SAFETY: `img` is the bitmap just created above and not yet released
            unsafe { GdipDisposeImage(img as *mut GpImage) };
            b
        };

        // SAFETY: `stream` is the interface pointer just created above and not yet released;
        // `GdipCreateBitmapFromStream` has already read what it needs into the bitmap, so
        // releasing it here doesn't affect the pixels already decoded.
        unsafe { release(stream) };
        out
    }

    fn read_pixels(img: *mut GpBitmap) -> Result<Bitmap, String> {
        let (mut w, mut h) = (0u32, 0u32);
        // SAFETY: `img` is valid (the caller just created it); both out-params are u32s on this
        // function's stack
        unsafe {
            GdipGetImageWidth(img as *mut GpImage, &mut w);
            GdipGetImageHeight(img as *mut GpImage, &mut h);
        }
        let (w, h) = (w as usize, h as usize);
        if w == 0 || h == 0 {
            return Err(crate::i18n::t("this image's dimensions are 0").into());
        }
        if w.saturating_mul(h) > MAX_PIXELS {
            return Err(crate::tf!("image is too large ({} × {}, limit about 24 MPix); cannot display", w, h));
        }

        let rect = Rect { X: 0, Y: 0, Width: w as i32, Height: h as i32 };
        let mut data: BitmapData = unsafe { std::mem::zeroed() };
        // SAFETY: `img` is valid; the whole region `rect` describes is inside the image (its
        // dimensions are the image's dimensions); `data` is a stack struct that GDI+ only writes
        // out-params into.
        let st = unsafe {
            GdipBitmapLockBits(
                img,
                &rect,
                ImageLockModeRead as u32,
                PIXEL_FORMAT_32BPP_ARGB,
                &mut data,
            )
        };
        if st != GP_OK || data.Scan0.is_null() {
            return Err(crate::tf!("cannot read this image's pixels (GDI+ status {})", st));
        }
        // SAFETY: after a successful lock, `Scan0` points at a read-only block of "`Height` rows,
        // each at least `Stride` bytes" (`ImageLockModeRead`); it is unlocked immediately after
        // the copy below.
        let out = unsafe { bgra_to_rgba(w, h, data.Stride, data.Scan0 as *const u8) };
        // SAFETY: `data` is the same one filled in by LockBits above and not yet unlocked
        unsafe { GdipBitmapUnlockBits(img, &mut data) };
        Ok(out)
    }
}

// ─────────────────────────── Other platforms ───────────────────────────

#[cfg(not(windows))]
mod platform {
    use super::Bitmap;
    use std::path::Path;

    /// No decoding on non-Windows for now: this project only ships a Windows build, and
    /// "another platform means another decoder dependency" isn't worth it. Return an honest
    /// sentence explaining so.
    pub fn decode(path: &Path) -> Result<Bitmap, String> {
        Err(crate::tf!("displaying images is not supported on this platform yet ({})", path.display()))
    }

    pub fn decode_bytes(_bytes: &[u8]) -> Result<Bitmap, String> {
        Err(crate::i18n::t("displaying images is not supported on this platform yet").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BGRA → RGBA, and when `stride` is wider than "width × 4", **take only the first width×4
    /// bytes**.
    ///
    /// This pins "are the colours right": a wrong channel order doesn't report anything, it just
    /// swaps red and blue across the whole image — the easiest thing to shrug off as "that's how
    /// the image is".
    #[test]
    fn bgra_is_swizzled_to_rgba_and_stride_padding_is_skipped() {
        // 2 pixels wide, 12 bytes per row (8 bytes of pixels + 4 bytes of padding)
        let src: Vec<u8> = vec![
            1, 2, 3, 4, /* B G R A */ 5, 6, 7, 8, /* second pixel */ 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        let bm = unsafe { bgra_to_rgba(2, 1, 12, src.as_ptr()) };
        assert_eq!(bm.width, 2);
        assert_eq!(bm.height, 1);
        assert_eq!(bm.rgba, vec![3, 2, 1, 4, 7, 6, 5, 8], "the first two pixels must become RGBA and the padding must be dropped");
    }

    /// A negative `stride` (a bottom-up bitmap) must be copied correctly too — the easiest kind
    /// of bitmap to miss.
    ///
    /// Bottom-up means: `Scan0` points at **image row 0**, while row 1 sits **above** it in
    /// memory (a smaller address), so `stride` is negative and `row(y) = Scan0 + y * stride`.
    /// Below, memory is laid out as "[row 1][row 0]" with `Scan0` pointing at the second half —
    /// exactly what GDI+ hands us.
    ///
    /// ⚠️ When writing this test I first got the expectation backwards (thinking `Scan0` pointed
    /// at the last row in memory), and this assertion forced it out on the spot — which is
    /// precisely why it exists: a negative-stride bug reports nothing, it just **flips the image
    /// upside down**, easily shrugged off as "that's how the image is".
    #[test]
    fn a_negative_stride_walks_backwards() {
        let mem: Vec<u8> = vec![
            10, 10, 10, 10, /* what sits in the first half of memory is **image row 1** */ 20, 20, 20, 20,
            /* the second half is row 0, and Scan0 points at it */
        ];
        let scan0 = unsafe { mem.as_ptr().add(4) };
        // One row is only 1 pixel = 4 bytes, so the stride is −4 (not −8: that would be two rows)
        let bm = unsafe { bgra_to_rgba(1, 2, -4, scan0) };
        assert_eq!(
            bm.rgba,
            vec![20, 20, 20, 20, /* row 0 */ 10, 10, 10, 10 /* row 1, smaller address */]
        );
    }

    /// A missing file must produce a plain sentence **right away**, rather than letting GDI+
    /// report a status code — "the original file was deleted" is the most common such display
    /// failure and deserves a sentence of its own.
    #[test]
    fn a_missing_file_says_so_plainly() {
        let e = match decode(std::path::Path::new("E:/no/such/image-xyz.png")) {
            Err(e) => e,
            Ok(_) => panic!("a non-existent file must not decode"),
        };
        assert!(e.contains("gone"), "{e}");
    }

    /// An embedded illustration must decode, and **its transparency must survive**.
    ///
    /// This pins GDI+'s nastiest trap: an indexed-colour PNG with `tRNS` can decode as opaque —
    /// showing up as "a white patch on the image", which is glaring in dark mode and easily
    /// shrugged off as "that's how the image is". The assertion deliberately **hard-codes neither
    /// the dimensions nor the colours** (swapping the illustration changes them all), pinning only
    /// the two things that won't change: the decoded dimensions match the PNG's own IHDR, and
    /// fully transparent pixels really exist.
    ///
    /// ⚠️ **Both must be walked**: the first screen picks its image by theme, so verifying only
    /// one means a decode failure in the other (the dark one, say) only shows up when the user
    /// switches to that theme — by which point it has already shipped.
    #[test]
    fn an_embedded_png_decodes_with_its_transparency_intact() {
        let assets: [(&str, &[u8]); 2] = [
            ("light", include_bytes!("../assets/light.png")),
            ("dark", include_bytes!("../assets/dark.png")),
        ];
        for (name, png) in assets {
            // A PNG's IHDR always follows the 8-byte signature + 4-byte length + 4-byte tag
            let w = u32::from_be_bytes(png[16..20].try_into().unwrap()) as usize;
            let h = u32::from_be_bytes(png[20..24].try_into().unwrap()) as usize;

            let bm = decode_bytes(png).unwrap_or_else(|e| panic!("{name}.png should decode: {e}"));
            assert_eq!((bm.width, bm.height), (w, h), "{name}.png dimensions must match the PNG header");
            assert_eq!(bm.rgba.len(), w * h * 4);

            let transparent = bm.rgba.chunks_exact(4).filter(|p| p[3] == 0).count();
            assert!(transparent > 0, "{name}.png has a transparent background but decoded to zero transparent pixels");
        }
    }
}
