//! Turn a decoded bitmap into an egui texture — a single function, but **two** call sites
//! need it.
//!
//! Those two call sites are: showing an image inside the conversation
//! (`app::App::poll_images`) and the illustration on the first screen ([`crate::brand::Art`]).
//! Both are exactly the same step: `aox::image::Bitmap` → `TextureHandle`.
//!
//! Why this deserves its own module instead of being written twice: **every argument here
//! fails silently**. Drop `from_rgba_unmultiplied` and the whole image is treated as
//! premultiplied (shadows go grey); use `NEAREST` sampling and a scaled line drawing turns
//! fuzzy. With two copies, you would only notice the missed edit if you happened to be
//! looking at both places at once — which almost never happens.

use egui::{ColorImage, Context, TextureHandle, TextureOptions};

use aox::image::Bitmap;

/// A texture named `name`, uploaded onto `ctx`.
///
/// `name` is for humans only (debugger, `ctx.texture_ui`); it takes no part in any equality
/// check. Callers just need something recognisable — uniqueness is not required.
pub fn upload(ctx: &Context, name: impl Into<String>, bm: &Bitmap) -> TextureHandle {
    ctx.load_texture(
        name,
        // `Bitmap` hands out **unpremultiplied** RGBA (the raw values GDI+ locks, see
        // `aox::image`), which is exactly what this constructor expects.
        ColorImage::from_rgba_unmultiplied([bm.width, bm.height], &bm.rgba),
        // Illustrations get scaled into the layout box; `NEAREST` would fray the edges of
        // line art, so it has to be linear.
        TextureOptions::LINEAR,
    )
}
