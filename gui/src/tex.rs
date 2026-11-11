//! Turn a decoded bitmap into an egui texture — a single function, but two call sites need it.
//! Those two call sites are: showing an image inside the conversation (`app::App::poll_images`) and the first...
//! Why this deserves its own module instead of being written twice: every argument here fails silently.

use egui::{ColorImage, Context, TextureHandle, TextureOptions};

use aox::image::Bitmap;

/// A texture named `name`, uploaded onto `ctx`.
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
