//! Decode local image files into pixels — for the UI to display (images in attachments, images produced...
//! # Why GDI+ rather than adding a decoder dependency.
//! This project is disciplined about binary size (see the size baselines in `docs/ARCH-ROADMAP.md`), and "display...

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
pub fn decode_bytes(bytes: &[u8]) -> Result<Bitmap, String> {
    if bytes.is_empty() {
        return Err(crate::i18n::t("the embedded image is empty").into());
    }
    platform::decode_bytes(bytes)
}

/// Copy GDI+'s BGRA buffer into RGBA.
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

// Windows: GDI

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

    /// GDI+'s `Ok` status code. Deliberately not writing `use...GdiPlus::*`: that module also exports an `Ok` (a...
    const GP_OK: i32 = 0;

    /// `PixelFormat32bppARGB`. `windows-sys` does not export this constant (it lives in the `windows` crate)...
    const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x0026_200A;

    /// The startup result is computed only once.
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
    unsafe fn release(p: *mut core::ffi::c_void) {
        if p.is_null() {
            return;
        }
        let vtbl = *(p as *mut *const usize);
        let release: extern "system" fn(*mut core::ffi::c_void) -> u32 =
            std::mem::transmute(*vtbl.add(2));
        release(p);
    }

    /// Decode a slice of image bytes held in memory: first wrap it into an `IStream` with `SHCreateMemStream`, then...
    pub fn decode_bytes(bytes: &[u8]) -> Result<Bitmap, String> {
        ensure_started()?;
        let len = u32::try_from(bytes.len())
            .map_err(|_| crate::i18n::t("this embedded image is too large (over 4 GB)").to_string())?;
        // SAFETY: `bytes` stays valid for this whole call (a borrowed slice) and `len` is its byte count. ️ `cbInit` must...
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

        // SAFETY: `stream` is the interface pointer just created above and not yet released; `GdipCreateBitmapFromStream`...
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
        // SAFETY: `img` is valid; the whole region `rect` describes is inside the image (its dimensions are the image's...
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
        // SAFETY: after a successful lock, `Scan0` points at a read-only block of "`Height` rows, each at least `Stride`...
        let out = unsafe { bgra_to_rgba(w, h, data.Stride, data.Scan0 as *const u8) };
        // SAFETY: `data` is the same one filled in by LockBits above and not yet unlocked
        unsafe { GdipBitmapUnlockBits(img, &mut data) };
        Ok(out)
    }
}

// Other platforms

#[cfg(not(windows))]
mod platform {
    use super::Bitmap;
    use std::path::Path;

    /// No decoding on non-Windows for now: this project only ships a Windows build, and "another platform means...
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

    /// Bytes.
    /// This pins "are the colours right": a wrong channel order doesn't report anything, it just swaps red and blue...
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

    /// A negative `stride` (a bottom-up bitmap) must be copied correctly too — the easiest kind of bitmap to miss.
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

    /// A missing file must produce a plain sentence right away, rather than letting GDI+ report a status code — "the...
    #[test]
    fn a_missing_file_says_so_plainly() {
        let e = match decode(std::path::Path::new("E:/no/such/image-xyz.png")) {
            Err(e) => e,
            Ok(_) => panic!("a non-existent file must not decode"),
        };
        assert!(e.contains("gone"), "{e}");
    }

    /// An embedded illustration must decode, and its transparency must survive.
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
