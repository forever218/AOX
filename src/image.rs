//! 把本机图片文件解成像素 —— 给界面**显示**用（附件里的图、插件产出的图）。
//!
//! # 为什么用 GDI+ 而不是加一个解码器依赖
//!
//! 这个项目对二进制体积是有纪律的（见 `docs/ARCH-ROADMAP.md` 的体积基准），而
//! 「显示一张图」这件事值得的代价很小：
//!
//! | 方案 | 代价 | 支持格式 |
//! |---|---|---|
//! | `image` crate | 约 +300~500 KB | 全 |
//! | `png` crate | 约 +100 KB | 只有 PNG |
//! | **GDI+**（本模块） | **0 字节**（系统自带 gdiplus.dll） | PNG / JPEG / GIF / BMP / TIFF … |
//!
//! 这个项目本来就只出 Windows 版、也已经在用 `windows-sys` 直连系统能力
//!（Job Object 围栏、DWM 标题栏、本地时间），所以「问系统要」比「塞一个解码器」
//! 一致得多。
//!
//! ⚠️ GDI+ 的 API 是**有状态**的：`GdiplusStartup` 必须先成功一次，而且**中途不能
//! 关掉再开**（官方明确不支持 shutdown→startup 循环）。所以这里启动一次就常驻，
//! 进程退出交给系统回收 —— 见 [`ensure_started`]。
//!
//! ⚠️ 解码是**同步**的，一张 4000×3000 的 PNG 要几十毫秒。界面层**必须**放到后台线程
//! 去调（`gui/src/app.rs` 里那条解码队列就是这么做的），否则每显示一张图就卡一帧。

use std::path::Path;

/// 解出来的位图：RGBA8、行优先、**未预乘**（egui 的 `ColorImage` 要的就是这个）。
pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    /// `width * height * 4` 字节
    pub rgba: Vec<u8>,
}

/// 一张图最多多少像素。
///
/// 挡住的是「一张声称 50000×50000 的图」：那会让我们一次分配 10 GB。GDI+ 自己对
/// 超大图也有拒绝，但那是它的事；我们要保护的是**自己的这次分配**。
/// 24 MPix ≈ 96 MB RGBA，比任何真实截图/照片都宽裕（6000×4000 的相机照片正好 24 MPix）。
const MAX_PIXELS: usize = 24_000_000;

/// 解一个图片文件。失败给的是**给人看的一句话**（会被显示在对话里）。
pub fn decode(path: &Path) -> Result<Bitmap, String> {
    if !path.is_file() {
        return Err(format!("图片文件不在了：{}", path.display()));
    }
    platform::decode(path)
}

/// 解内存里的一段图片字节。
///
/// 给**内嵌资源**用（首屏那张插画是 `include_bytes!` 进来的，没有文件路径）。
/// 和 [`decode`] 走的是同一个 GDI+，只是前面多一步「把这段内存包成一个 `IStream`」。
pub fn decode_bytes(bytes: &[u8]) -> Result<Bitmap, String> {
    if bytes.is_empty() {
        return Err("内嵌的图片是空的".into());
    }
    platform::decode_bytes(bytes)
}

/// 把 GDI+ 交出来的 BGRA 缓冲拷成 RGBA。
///
/// 单独抽出来（而不是写在调用点里）是因为它是整个解码里**最容易悄悄错**的一段：
/// 通道顺序、`stride` 与「宽度 × 4」的那点差、以及 `stride` 可能为负（自下而上的位图）。
/// 这些错了不会报错，只会让「图片能显示但颜色不对」—— 那种错很容易被当成
/// 「这图本来就这样」而放过，所以它值得一条直接的断言。
///
/// # Safety
///
/// `src` 必须指向至少「`height` 行、每行可读 `stride.abs()` 字节」的一块内存。
unsafe fn bgra_to_rgba(width: usize, height: usize, stride: i32, src: *const u8) -> Bitmap {
    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        // 用 `isize` 乘：`stride` 为负时（自下而上）这一步要往回走
        let row = src.offset(y as isize * stride as isize);
        let dst = &mut rgba[y * width * 4..(y + 1) * width * 4];
        for x in 0..width {
            let p = row.add(x * 4);
            // GDI+ 的 `PixelFormat32bppARGB` 在小端机器上内存里就是 B,G,R,A
            let (b, g, r, a) = (*p, *p.add(1), *p.add(2), *p.add(3));
            dst[x * 4] = r;
            dst[x * 4 + 1] = g;
            dst[x * 4 + 2] = b;
            dst[x * 4 + 3] = a;
        }
    }
    Bitmap { width, height, rgba }
}

// ─────────────────────────── Windows：GDI+ ───────────────────────────

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

    /// GDI+ 的状态码 `Ok`。**刻意不写 `use ...GdiPlus::*`**：那个模块里同样导出一个
    /// `Ok`（状态码常量），glob 进来会把 `Result::Ok` 遮住 —— 于是这个文件里所有
    /// `Ok(x)` 都会变成类型错误，报错却指向别处，很难看出是导入惹的。
    const GP_OK: i32 = 0;

    /// `PixelFormat32bppARGB`。`windows-sys` **没有导出**这个常量（它在 `windows` crate 里），
    /// 值取自 `Gdipluspixelformats.h`：`PIXEL_FORMAT_32BPP_ARGB = 0x0026200A`。
    const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x0026_200A;

    /// 启动结果只算一次。
    ///
    /// ⚠️ **从不调用 `GdiplusShutdown`**：官方文档写明「GDI+ 不能在 shutdown 之后再
    /// startup」（第二次会失败），而这是个 GUI 进程、解码会一直发生 —— 关掉它换来的是
    /// 「以后再也解不了图」。让进程退出时由系统回收是这种一次性全局库的标准做法。
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
                // SAFETY: 两个出参都是本函数栈上的变量，生命周期覆盖这次调用。
                let st = unsafe { GdiplusStartup(&mut token, &input, &mut out) };
                if st != GP_OK {
                    Err(format!("GDI+ 初始化失败（status {st}）"))
                } else {
                    Ok(())
                }
            })
            .clone()
    }

    pub fn decode(path: &Path) -> Result<Bitmap, String> {
        ensure_started()?;
        // GDI+ 只认 UTF-16 路径，且必须自带结尾 0
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

        let mut img: *mut GpBitmap = std::ptr::null_mut();
        // SAFETY: `wide` 是有效的以 0 结尾的 UTF-16 串；出参是栈上的指针。
        let st = unsafe { GdipCreateBitmapFromFile(wide.as_ptr(), &mut img) };
        if st != GP_OK || img.is_null() {
            return Err(format!(
                "解不开这张图（GDI+ status {st}）：{}",
                path.display()
            ));
        }
        let out = read_pixels(img);
        // SAFETY: `img` 是上面刚创建、还没释放的位图
        unsafe { GdipDisposeImage(img as *mut GpImage) };
        out
    }

    /// 放掉一个 COM 接口 —— 我们只需要 `IUnknown::Release` 这一个能力。
    ///
    /// ⚠️ **`windows-sys` 里没有 `IUnknown`**（那是 `windows` crate 的东西），所以只能
    /// 自己按 COM 的 ABI 走一遍虚表。这么做是站得住的：`QueryInterface / AddRef / Release`
    /// 永远是虚表的头三个槽位，这是 COM 的硬约定，不随接口而变。
    ///
    /// # Safety
    ///
    /// `p` 必须是一个尚未释放的 COM 接口指针（或空指针）。
    unsafe fn release(p: *mut core::ffi::c_void) {
        if p.is_null() {
            return;
        }
        let vtbl = *(p as *mut *const usize);
        let release: extern "system" fn(*mut core::ffi::c_void) -> u32 =
            std::mem::transmute(*vtbl.add(2));
        release(p);
    }

    /// 解内存里的一段图片字节：先用 `SHCreateMemStream` 把它包成 `IStream`，再交给 GDI+。
    ///
    /// 为什么不落一个临时文件再走 [`decode`]：那样会在磁盘上留一份可以失败的中间产物
    ///（杀软、临时目录权限、并发同名），而这里要解的是一段**编译期就固定**的字节
    /// —— 为它引入文件系统这一层风险不值得。
    pub fn decode_bytes(bytes: &[u8]) -> Result<Bitmap, String> {
        ensure_started()?;
        let len = u32::try_from(bytes.len())
            .map_err(|_| "这段内嵌图片太大了（超过 4 GB）".to_string())?;
        // SAFETY: `bytes` 在这次调用期间一直有效（借来的切片），`len` 就是它的字节数。
        // ⚠️ `cbInit` 绝不能传 0 —— 那时的语义是「pInit 是一段以 0 结尾的字符串」，
        // 而图片字节里出现 0 是家常便饭。
        let stream = unsafe { SHCreateMemStream(bytes.as_ptr(), len) };
        if stream.is_null() {
            return Err("创建内存流失败".into());
        }

        let mut img: *mut GpBitmap = std::ptr::null_mut();
        // SAFETY: `stream` 是上面刚创建、还没释放的 IStream；出参是栈上的指针。
        let st = unsafe { GdipCreateBitmapFromStream(stream, &mut img) };
        let out = if st != GP_OK || img.is_null() {
            Err(format!("解不开这段内嵌的图片（GDI+ status {st}）"))
        } else {
            let b = read_pixels(img);
            // SAFETY: `img` 是上面刚创建、还没释放的位图
            unsafe { GdipDisposeImage(img as *mut GpImage) };
            b
        };

        // SAFETY: `stream` 是上面刚创建、还没释放的接口指针；`GdipCreateBitmapFromStream`
        // 已经把需要的内容读进位图了，这里放掉它不影响到已经解出来的像素。
        unsafe { release(stream) };
        out
    }

    fn read_pixels(img: *mut GpBitmap) -> Result<Bitmap, String> {
        let (mut w, mut h) = (0u32, 0u32);
        // SAFETY: `img` 有效（调用方刚创建）；两个出参是本函数栈上的 u32
        unsafe {
            GdipGetImageWidth(img as *mut GpImage, &mut w);
            GdipGetImageHeight(img as *mut GpImage, &mut h);
        }
        let (w, h) = (w as usize, h as usize);
        if w == 0 || h == 0 {
            return Err("这张图的尺寸是 0".into());
        }
        if w.saturating_mul(h) > MAX_PIXELS {
            return Err(format!("图太大了（{w}×{h}，上限约 24 MPix），没法显示"));
        }

        let rect = Rect { X: 0, Y: 0, Width: w as i32, Height: h as i32 };
        let mut data: BitmapData = unsafe { std::mem::zeroed() };
        // SAFETY: `img` 有效；`rect` 描述的整块区域在图片内（尺寸就是图片尺寸）；
        // `data` 是栈上的结构体，GDI+ 只往里写出参。
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
            return Err(format!("读不出这张图的像素（GDI+ status {st}）"));
        }
        // SAFETY: 锁成功之后，`Scan0` 指向「`Height` 行、每行至少 `Stride` 字节」的一块
        // 只读内存（`ImageLockModeRead`）；下面拷完就立刻解锁。
        let out = unsafe { bgra_to_rgba(w, h, data.Stride, data.Scan0 as *const u8) };
        // SAFETY: `data` 是上面 LockBits 填好的同一份，且还没解锁
        unsafe { GdipBitmapUnlockBits(img, &mut data) };
        Ok(out)
    }
}

// ─────────────────────────── 其它平台 ───────────────────────────

#[cfg(not(windows))]
mod platform {
    use super::Bitmap;
    use std::path::Path;

    /// 非 Windows 上暂时不解码：这个项目目前只出货 Windows 版，
    /// 而「换个平台就多一个解码器依赖」不划算。返回一句实话说清楚。
    pub fn decode(path: &Path) -> Result<Bitmap, String> {
        Err(format!("本平台暂不支持显示图片（{}）", path.display()))
    }

    pub fn decode_bytes(_bytes: &[u8]) -> Result<Bitmap, String> {
        Err("本平台暂不支持显示图片".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BGRA → RGBA、以及 `stride` 比「宽度 × 4」宽时**只取前宽×4 字节**。
    ///
    /// 这条钉的是「颜色对不对」：通道顺序错了不会报错，只会让整张图的红蓝互换 ——
    /// 最容易被当成「这图本来就这样」而放过去。
    #[test]
    fn bgra_is_swizzled_to_rgba_and_stride_padding_is_skipped() {
        // 2 像素宽，一行 12 字节（8 字节像素 + 4 字节 padding）
        let src: Vec<u8> = vec![
            1, 2, 3, 4, /* B G R A */ 5, 6, 7, 8, /* 第二像素 */ 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        let bm = unsafe { bgra_to_rgba(2, 1, 12, src.as_ptr()) };
        assert_eq!(bm.width, 2);
        assert_eq!(bm.height, 1);
        assert_eq!(bm.rgba, vec![3, 2, 1, 4, 7, 6, 5, 8], "前两个像素要转成 RGBA，padding 要丢掉");
    }

    /// `stride` 为负（自下而上的位图）时也要拷对 —— 这是最容易漏的一种位图。
    ///
    /// 自下而上的语义是：`Scan0` 指向**图片第 0 行**，而第 1 行在内存里**往上**
    ///（地址更小），所以 `stride` 是负数、`row(y) = Scan0 + y * stride`。
    /// 下面把内存摆成「[第 1 行][第 0 行]」，让 `Scan0` 指着后半截 —— 正好就是
    /// GDI+ 交给我们时的样子。
    ///
    /// ⚠️ 写这条时第一次把期望写反了（以为 `Scan0` 指向内存末行），是这条断言把它
    /// 当场逼出来的 —— 那正是它存在的意义：负 stride 的错不报错，只是**图上下颠倒**，
    /// 很容易被当成「这图本来就这样」。
    #[test]
    fn a_negative_stride_walks_backwards() {
        let mem: Vec<u8> = vec![
            10, 10, 10, 10, /* 内存里在前半截的是**图片第 1 行** */ 20, 20, 20, 20,
            /* 后半截是第 0 行，Scan0 指着它 */
        ];
        let scan0 = unsafe { mem.as_ptr().add(4) };
        // 一行只有 1 个像素 = 4 字节，所以步长是 −4（不是 −8：那是两行）
        let bm = unsafe { bgra_to_rgba(1, 2, -4, scan0) };
        assert_eq!(
            bm.rgba,
            vec![20, 20, 20, 20, /* 第 0 行 */ 10, 10, 10, 10 /* 第 1 行，地址更小 */]
        );
    }

    /// 不存在的文件要**当场**给一句人话，而不是让 GDI+ 报一个状态码 ——
    /// 「原文件被删了」是这类显示失败里最常见的一种，值得单独一句。
    #[test]
    fn a_missing_file_says_so_plainly() {
        let e = match decode(std::path::Path::new("E:/no/such/image-xyz.png")) {
            Err(e) => e,
            Ok(_) => panic!("不存在的文件不该解开"),
        };
        assert!(e.contains("不在了"), "{e}");
    }

    /// 内嵌的插画要解得开，而且**透明必须还在**。
    ///
    /// 这条钉的是 GDI+ 最阴的一个坑：带 `tRNS` 的索引色 PNG 有可能被解成不透明 ——
    /// 表现就是「图上多了一块白底」，而它在暗色模式里会非常刺眼，且很容易被当成
    /// 「这张图本来就这样」。断言刻意**不写死尺寸也不写死颜色**（换一张插画就全变了），
    /// 只钉两件不会变的事：解出来的尺寸与 PNG 自己的 IHDR 一致，且确实存在全透明像素。
    ///
    /// ⚠️ **两张都要过一遍**：首屏是按主题挑图的，只验其中一张的话，另一张
    ///（比如暗色那张）解码失败要等用户切到那个主题才看得见 —— 而那时它已经发出去了。
    #[test]
    fn an_embedded_png_decodes_with_its_transparency_intact() {
        let assets: [(&str, &[u8]); 2] = [
            ("light", include_bytes!("../assets/light.png")),
            ("dark", include_bytes!("../assets/dark.png")),
        ];
        for (name, png) in assets {
            // PNG 的 IHDR 永远紧跟在 8 字节签名 + 4 字节长度 + 4 字节 "IHDR" 之后
            let w = u32::from_be_bytes(png[16..20].try_into().unwrap()) as usize;
            let h = u32::from_be_bytes(png[20..24].try_into().unwrap()) as usize;

            let bm = decode_bytes(png).unwrap_or_else(|e| panic!("{name}.png 应该解得开：{e}"));
            assert_eq!((bm.width, bm.height), (w, h), "{name}.png 的尺寸要与 PNG 头一致");
            assert_eq!(bm.rgba.len(), w * h * 4);

            let transparent = bm.rgba.chunks_exact(4).filter(|p| p[3] == 0).count();
            assert!(transparent > 0, "{name}.png 是透明底的，解出来却一个透明像素都没有");
        }
    }
}
