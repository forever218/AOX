//! 首屏那张插画：亮色一套、暗色一套，按当前主题挑一张画在版心正中。
//!
//! # 为什么是内嵌的
//!
//! 两张图是 `include_bytes!` 进来的，不是从 `assets/` 里读的 —— 装完之后**只有一个 exe**，
//! 不需要「assets 目录还在原位、权限还在、没被杀软挪走」。代价是 exe 大小：两张 PNG
//! 共 363 KB（见 `docs/ARCH-ROADMAP.md` 的体积基准，这一项是专门记的）。
//!
//! # 为什么要两套图
//!
//! 这两张插画都是**透明底的黑线稿**，所以它们是「贴在界面底色上」的，而不是自带背景的
//! 一张图 —— 亮色底与暗色底各需要一套（用户 2026-10-06 给的就是成对的）。也因此
//! **换主题时只要换纹理**，不必重新解码。

use std::sync::mpsc::{channel, Receiver, TryRecvError};

use egui::{Color32, Context, Pos2, Rect, RichText, Sense, TextureHandle, Ui, Vec2};

use aox::image::Bitmap;

use crate::theme::{self, P};
use crate::tex;

/// 插画资源。两张都是 630×420 的透明底 PNG。
const LIGHT: &[u8] = include_bytes!("../../assets/light.png");
const DARK: &[u8] = include_bytes!("../../assets/dark.png");

/// 程序图标。与 `build.rs` 嵌进 exe 资源的是**同一个文件**，这里再嵌一份是拿它的像素
/// 去喂窗口（原因见 [`window_icon`]）。7 帧、2.8 KB，重复嵌一次的代价可以忽略。
const ICO: &[u8] = include_bytes!("../../assets/aox.ico");

/// 窗口图标：标题栏左上角、任务栏、Alt+Tab 都用它。
///
/// # 为什么不干脆让 Windows 用 exe 资源里那份
///
/// 那本来是最理想的路子（每个尺寸都能取到原生帧，一个像素都不用重采样）：
/// `.with_icon(IconData::default())` 会让 eframe **跳过**它自己那个默认图标
/// （黑底白字的「e」，用户 2026-10-06 报的就是它）。**但这条路实测不通** ——
/// eframe 是跳过了，可 winit 仍会拿到那个 0×0 的 `IconData` 并给窗口装上一个无效
/// HICON，标题栏于是画出一个系统的通用图标（一块「照片/窗口」样的灰块，比「e」还费解）。
/// 所以只能喂**真像素**。
///
/// # 为什么挑这一档尺寸
///
/// `IconData` 只有**一个**尺寸，eframe 会拿它按 `SM_CXICON` / `SM_CXSMICON` 各缩放一次。
/// 取自 [`theme::system_icon_size`]（也就是 `SM_CXICON`）那一帧：大的那档 1:1、
/// 小的那档正好 2:1 —— 两档都干净。挑最大的 256 就不行：缩到 16 是 16:1，点阵会糊成一团。
pub fn window_icon() -> egui::IconData {
    let frames = ico_frames(ICO);
    let want = theme::system_icon_size().max(1) as u16;
    let pick = frames
        .iter()
        .find(|(size, _)| *size == want)
        // 没有正好那一档就退而取最大的：宁可糊一点，也不要没有图标
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
        // 解不开就交一张全透明的：宁可标题栏上没有图标，也不要杵一块「大概是什么」的图标
        Err(_) => invisible_icon(),
    }
}

/// 一枚 1×1 全透明的图标 —— 「这里不要有图标」的那条退路。
fn invisible_icon() -> egui::IconData {
    egui::IconData { rgba: vec![0, 0, 0, 0], width: 1, height: 1 }
}

/// 把 ICO 拆成「边长 → 该帧的原始载荷」。
///
/// 结构很简单：6 字节目录头（保留 / 类型 / 帧数），随后每帧 16 字节目录项，载荷跟在后面。
/// ⚠️ 目录项里宽高各占**一个字节**，所以 **0 表示 256**（256 塞不进一个字节）——
/// 这正是「取 256 那帧」最容易静默失败的地方。
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
            continue; // 非正方形：这个项目里不会出现，跳过比猜一张强
        }
        let len = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let off = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        if let Some(payload) = ico.get(off..off.saturating_add(len)) {
            out.push((w, payload));
        }
    }
    out
}

/// 插画最宽画到多少**逻辑点**。
///
/// 这个数是取舍出来的：再宽就压过输入框、把首屏变成一张海报；再窄则看不清画的是什么。
/// ⚠️ 别随手改小：630 px 的源图在 150% 缩放的屏幕上正好 1:1（420 × 1.5 = 630），
/// 一改小就变成放大渲染，线稿的边会发虚。
const MAX_W: f32 = 420.0;

/// 插画在给定宽度下占多高（逻辑点）。
///
/// 宽高比是**从 PNG 头里读**的，不是等解码完再问纹理 —— 首屏要在图还没解好之前
/// 就把它的位置留出来，否则「解好的那一刻整块内容会往下跳一截」。
pub fn height(dark: bool, max_w: f32) -> f32 {
    let (w, h) = ihdr(if dark { DARK } else { LIGHT });
    max_w.min(MAX_W) * h / w
}

/// 从 PNG 字节里读出 `(宽, 高)`。
///
/// PNG 的 IHDR 永远紧跟在「8 字节签名 + 4 字节长度 + 4 字节 `IHDR`」之后，
/// 宽高各是 4 字节大端。
fn ihdr(png: &[u8]) -> (f32, f32) {
    let be = |i: usize| {
        u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as f32
    };
    (be(16), be(20))
}

/// 一张插画的一生。
enum Slot {
    /// 还没开始解（第一帧才会去起线程）
    Idle,
    Loading(Receiver<Result<Bitmap, String>>),
    Ready(TextureHandle),
    /// 解不开。**不再重试** —— 它是编译期固定的一段字节，这台机器上解不开就是解不开，
    /// 每帧重来只是白烧 CPU。留一句话在屏幕上，别让首屏整块空着又不给原因。
    Failed(String),
}

/// 首屏插画（`[亮, 暗]` 两套）。
pub struct Art {
    slots: [Slot; 2],
}

impl Art {
    pub fn new() -> Self {
        Self { slots: [Slot::Idle, Slot::Idle] }
    }

    /// 每帧一次：该起线程的起线程、该收结果的收结果。
    ///
    /// 两张**一起**解，而不是「用到哪张才解哪张」：一张 630×420 的解码是毫秒级，
    /// 而「切主题的那一刻才开始解」会让切换后的头几帧空着 —— 那比多花几毫秒糟得多。
    pub fn poll(&mut self, ctx: &Context) {
        let mut waiting = false;
        for (i, src) in [LIGHT, DARK].into_iter().enumerate() {
            // 先把槽位取出来再回填：`match &mut self.slots[i]` 之后在分支里写 `self.slots[i]`
            // 会同时借两次（egui 那边这叫「借用的体操」，这里用 `replace` 一步绕开）。
            let slot = std::mem::replace(&mut self.slots[i], Slot::Idle);
            self.slots[i] = match slot {
                Slot::Idle => {
                    let (tx, rx) = channel();
                    // 解码是同步的、要过一遍 GDI+（见 `aox::image`），别占界面线程
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
                    Err(TryRecvError::Disconnected) => Slot::Failed("解码线程提前结束了".into()),
                },
                other => other,
            };
        }
        // 还在解就要求下一帧 —— egui 只在有输入时才重绘，不主动要就会永远停在「没解完」
        if waiting {
            ctx.request_repaint();
        }
    }

    /// 按当前主题把插画铺在 `ui` 里（水平居中由调用方保证），返回它占的高度。
    ///
    /// 没画出来（还在解 / 解不开）返回 `0.0` —— 调用方据此决定垂直居中的算法，
    /// 而不是「照着一个还没到的高度先把位置占了」。
    pub fn show(&mut self, ui: &mut Ui, max_w: f32) -> f32 {
        let dark = theme::mode() == theme::Mode::Dark;
        let slot = &self.slots[if dark { 1 } else { 0 }];
        match slot {
            Slot::Ready(tex) => {
                // 尺寸用 [`height`] 算出来的同一份数，而不是纹理自己的像素数 ——
                // 两处独立算的话，「留白的高度」与「画出来的高度」会差一点点，
                // 而这种差只会在垂直居中的时候露馅（画偏了半行）。
                let size = Vec2::new(max_w.min(MAX_W), height(dark, max_w));
                let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
                ui.painter().image(
                    tex.id(),
                    rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    // 不染色：画出来就是画本身的样子（主题差异靠**换图**，不靠滤镜）
                    Color32::WHITE,
                );
                size.y
            }
            Slot::Failed(e) => {
                ui.label(
                    RichText::new(format!("插画显示不出来：{e}"))
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

    /// ICO 目录要解得动：每帧都得取到真载荷，且「0 表示 256」这条要成立。
    ///
    /// 为什么值得钉：`window_icon` 拿不到帧时是**悄悄**退成一张全透明图标的 ——
    /// 不报错、不崩，只是标题栏上图标没了。所以「找得到」这件事必须有人盯着。
    #[test]
    fn every_ico_frame_is_reachable() {
        let frames = ico_frames(ICO);
        assert!(frames.len() >= 5, "内嵌的 ico 应该有多档尺寸，实际只有 {}", frames.len());
        for (size, payload) in &frames {
            assert!((16..=256).contains(size), "尺寸不合理：{size}");
            assert!(
                payload.starts_with(&[0x89, b'P', b'N', b'G']),
                "{size}×{size} 那帧不是 PNG 载荷"
            );
        }
        // 目录项里 0 表示 256：它必须被认出来，而不是当成「0×0」丢掉
        assert!(frames.iter().any(|(s, _)| *s == 256), "256 那帧没被认出来");
        // 32 是 `window_icon` 的兜底尺寸，一定要在
        assert!(frames.iter().any(|(s, _)| *s == 32), "没有 32×32 那帧");
    }
}
