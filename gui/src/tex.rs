//! 把解好的位图挂成 egui 纹理 —— 只有一个函数，但**两个地方**要用它。
//!
//! 两个调用点：对话里显示图片（`app::App::poll_images`）与首屏那张插画
//! （[`crate::brand::Art`]）。都是「`aox::image::Bitmap` → `TextureHandle`」这一步。
//!
//! 为什么值得单开一个模块而不是各写一遍：这一步里**每一个参数错了都不会报错**
//! —— 少写 `from_rgba_unmultiplied` 就会把整张图当成预乘色（暗部发灰）、
//! 用 `NEAREST` 采样就会让缩放后的线稿发毛。抄成两份时，只有「恰好同时看着两处」
//! 才会发现漏改，而那几乎不会发生。

use egui::{ColorImage, Context, TextureHandle, TextureOptions};

use aox::image::Bitmap;

/// 挂在 `ctx` 上、名为 `name` 的一张纹理。
///
/// `name` 只给人看（调试器 / `ctx.texture_ui`），不参与任何判等 —— 调用方给个
/// 眼熟的名字就行，不必保证唯一。
pub fn upload(ctx: &Context, name: impl Into<String>, bm: &Bitmap) -> TextureHandle {
    ctx.load_texture(
        name,
        // `Bitmap` 交出来的是**未预乘**的 RGBA（GDI+ 锁出来的原始值，见 `aox::image`），
        // 正好对上这个构造函数的语义。
        ColorImage::from_rgba_unmultiplied([bm.width, bm.height], &bm.rgba),
        // 插画会被缩放到版心里；`NEAREST` 会让线稿的边发毛，必须线性。
        TextureOptions::LINEAR,
    )
}
