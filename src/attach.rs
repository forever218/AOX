//! 附件 —— 把本机文件挂到一条消息上发给模型。
//!
//! # 为什么是**一个**模块
//!
//! 三个入口都要干同一件事，所以判定、嗅探、限额只写一份：
//!
//! | 入口 | 路径 |
//! |---|---|
//! | 用户 | GUI 拖入窗口 / CLI 的 `/attach` → [`Attachment::probe`] |
//! | 工具（宿主）| `attach_file` → 同一个 `probe` |
//! | 工具（插件）| 插件结果里的 `"attach": ["<路径>", …]` → 同一个 `probe` |
//!
//! 「插件也能上传」就这么落地：插件是独立进程，调不到宿主工具，但它**能在自己的返回值里
//! 声明产出的文件**。宿主把那条声明与用户拖进来的文件走同一条路 —— 于是「插件截图 →
//! 模型真的看见那张图」成立，而不是只把路径作为字符串塞进上下文。
//!
//! # 不按后缀拦，也不按声明的 MIME 拦
//!
//! 端点**按文件实际内容**判断格式，官方明说不看扩展名、不看声明的 MIME。所以嗅探魔数
//! 在本模块里只有一个用途：**把 `data:` URL 的 MIME 填对**（那是端点确实需要的一项信息）。
//! 「这个格式支不支持」由端点回答 —— 它比宿主猜得准，而且用户要的就是「直到被 API 返回
//! 不支持，再由模型告知」。
//!
//! # 宿主只挡三类「必然失败」的情况
//!
//! 这三条撞上去只会换来一句难懂的 400，而且**发出去之前就能确定**：
//!
//! | 闸门 | 数值 |
//! |---|---|
//! | 单张图片 | ≤ [`MAX_IMAGE_BYTES`]（32 MiB） |
//! | 任意单个附件 | ≤ [`MAX_FILE_BYTES`]（36 MiB，= 48 MiB 折算回原始字节） |
//! | 单次请求体（内联 base64 计入） | ≤ [`MAX_BODY_BYTES`]（48 MiB） |
//! | 单次请求图片数 | ≤ [`MAX_IMAGES`]（600） |
//!
//! **刻意不做**图片像素上限（单边 8192px、单次 ≥15 张时降为 4096px）：那要解码各格式的
//! 图片头才判得出来，写错会**误拦**一张本来合法的图 —— 那比不检查更糟。交给端点报。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// 单张图片的字节上限（端点硬限制，内联 base64 与外部 URL 同数）。
pub const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
/// 单次请求体的字节上限（**内联 base64 计入**）。base64 膨胀约 4/3，判定时按 4/3 估。
pub const MAX_BODY_BYTES: u64 = 48 * 1024 * 1024;
/// **任何**单个附件的字节上限：48 MiB 的请求体按 4/3 折算回原始字节就是 36 MiB。
///
/// 这条与图像无关，是给**非图片**兜的（图片另有更紧的 32 MiB）。没有它，一个 2 GB 的
/// 文件被拖进来会先在 `read()` 里把内存吃干净 —— 那不是端点报错能救回来的。
pub const MAX_FILE_BYTES: u64 = MAX_BODY_BYTES * 3 / 4;
/// 单次请求的图片张数上限。
pub const MAX_IMAGES: usize = 600;

/// 端点在「图像理解」文档里列出的四种图片格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Jpeg,
    Png,
    Gif,
    Webp,
}

impl ImageKind {
    pub fn mime(self) -> &'static str {
        match self {
            ImageKind::Jpeg => "image/jpeg",
            ImageKind::Png => "image/png",
            ImageKind::Gif => "image/gif",
            ImageKind::Webp => "image/webp",
        }
    }

    /// 按**文件内容**（魔数）判定。认不出就是 `None` —— 即「非图片」。
    ///
    /// 前缀都取自各格式自己的签名：JPEG 的 `FF D8 FF`、PNG 的八字节签名、
    /// GIF 的两个版本串、WebP 的 `RIFF….WEBP`。
    pub fn sniff(head: &[u8]) -> Option<Self> {
        if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Some(ImageKind::Jpeg);
        }
        if head.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(ImageKind::Png);
        }
        if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
            return Some(ImageKind::Gif);
        }
        // WebP：`RIFF` + 4 字节长度 + `WEBP`。长度那 4 字节不校验（不参与格式判定）。
        if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
            return Some(ImageKind::Webp);
        }
        None
    }
}

/// 一个待发送的附件。
///
/// **不持有文件内容**：待上传栏可能挂着一个几十 MB 的文件，界面状态不该背着它。
/// 内容只在真正要发出去的那一刻读（[`Attachment::read`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// 本机路径。三个用途：读盘、写进 `.jsonl` 的引用、以及**告诉模型它在哪**
    /// —— 端点不支持这个格式时，模型还能自己用 `read_file` 去读。
    pub path: PathBuf,
    /// 展示名（界面上的胶囊、`filename` 字段都用它）。
    pub name: String,
    /// 文件的字节数（`probe` 时 stat 得到）。
    pub size: u64,
    /// 按魔数嗅探出的图片类型；`None` 表示不是这四种图片之一。
    pub kind: Option<ImageKind>,
}

/// 嗅探时读多少字节的头。12 就够了（WebP 的签名最长，到第 12 字节），
/// 留到 32 只是为了不必在改嗅探逻辑时重新算这个数。
const HEAD_LEN: usize = 32;

impl Attachment {
    /// 从路径建一个附件：stat + 读 [`HEAD_LEN`] 字节头，**不把整份文件读进内存**。
    ///
    /// 错误一律是「给用户看的一句话」—— 拖入 / 挂载的调用点直接把这句话显示出来。
    pub fn probe(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let md = std::fs::metadata(&path)
            .map_err(|e| format!("{} 读不到：{e}", path.display()))?;
        if md.is_dir() {
            return Err(format!(
                "{} 是目录。附件只收单个文件 —— 要打包请先压成一个文件。",
                path.display()
            ));
        }
        if !md.is_file() {
            return Err(format!("{} 不是普通文件，无法作为附件。", path.display()));
        }
        let size = md.len();
        let kind = sniff_path(&path)?;
        if kind.is_some() && size > MAX_IMAGE_BYTES {
            return Err(format!(
                "{} 是图片但 {} 字节，超过单张上限 {} MiB（端点硬限制）。\
                 请先压缩或缩小尺寸再挂。",
                path.display(),
                size,
                MAX_IMAGE_BYTES / 1024 / 1024
            ));
        }
        if size > MAX_FILE_BYTES {
            return Err(format!(
                "{} 有 {} MiB，超过单个附件上限 {} MiB —— 它编码成 base64 之后放不进\
                 单次请求体（{} MiB）。请先压缩或分块。",
                path.display(),
                size / 1024 / 1024,
                MAX_FILE_BYTES / 1024 / 1024,
                MAX_BODY_BYTES / 1024 / 1024
            ));
        }
        Ok(Self {
            name: display_name(&path),
            path,
            size,
            kind,
        })
    }

    /// 把整份文件读进内存。只在组装请求时调。
    pub fn read(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.path).map_err(|e| format!("读取 {} 失败：{e}", self.path.display()))
    }
}

/// 展示名：文件名（不含目录）。界面上的胶囊靠它显示，过长由界面自己省略。
fn display_name(path: &Path) -> String {
    path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| {
        // 极端情况（路径以 `..` 结尾之类）—— 退回整条路径，总比显示空白强
        path.to_string_lossy().into_owned()
    })
}

/// 读文件头判图片类型。文件打不开时**不报错**：一个读不了的文件会在
/// [`Attachment::read`] 那一步给出一次更准的错误，这里不必提前喊。
fn sniff_path(path: &Path) -> Result<Option<ImageKind>, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("{} 打不开：{e}", path.display()))?;
    let mut head = [0u8; HEAD_LEN];
    let n = f.read(&mut head).map_err(|e| format!("读取 {} 的文件头失败：{e}", path.display()))?;
    Ok(ImageKind::sniff(&head[..n]))
}

/// 发送前的**批量**闸门：张数与请求体总量。单文件的那条在 [`Attachment::probe`] 里。
///
/// `extra` 是随附件一起发的文字量（正文 + 路径清单），一起计入请求体 —— 一份 47 MiB 的
/// 附件加上几千字正文就可能越线，这个数不能漏。
pub fn check_batch(items: &[Attachment], extra_bytes: usize) -> Result<(), String> {
    let images = items.iter().filter(|a| a.kind.is_some()).count();
    if images > MAX_IMAGES {
        return Err(format!(
            "一次最多发 {MAX_IMAGES} 张图片，这次有 {images} 张。请分批发。"
        ));
    }
    // base64 是 4 字节换 3 字节 ⇒ 按 ceil(n/3)*4 估，再叠上正文。
    let raw: u64 = items.iter().map(|a| a.size).sum();
    let inlined = raw / 3 * 4 + (raw % 3 != 0) as u64 * 4;
    let total = inlined + extra_bytes as u64;
    if total > MAX_BODY_BYTES {
        return Err(format!(
            "这批附件太大：编码后约 {} MiB，超过单次请求体上限 {} MiB（端点硬限制）。\
             请分批发，或先压缩。",
            total / 1024 / 1024,
            MAX_BODY_BYTES / 1024 / 1024
        ));
    }
    Ok(())
}

/// 附件清单块的起始标记。
///
/// 清单**单独占一个 `text` part**（而不是拼进用户正文），这样两件事同时成立：
/// - 端点看到的仍是合法形状（就是普通的 `text` 块，没有多余字段）；
/// - [`crate::message::Message::flat_text`] 靠这个标记**跳过**它 —— 界面上不会把
///   「- a.png → E:\…」这种宿主自己加的清单当成用户说的话。
pub const ATTACH_MARK: &str = "[附件]\n";

/// 把「文字 + 附件」组装成一条 message 的 `content`。
///
/// - 没有附件时返回**纯字符串** —— 与加本功能之前的请求逐字节相同（老对话文件、
///   老端点、以及不支持图片的模型都照旧）。
/// - 有附件时返回 parts 数组：
///   - 图片 → `{"type":"image_url","image_url":{"url":"data:<MIME>;base64,…"}}`
///   - 非图片 → `{"type":"file","file_data":"data:<MIME>;base64,…","filename":"…"}`
///     （端点文档里 `file` 块只承诺用于 Files API 的图片；非图片照发是**故意的** ——
///     用户要的是「不设限，由端点报不支持」，而不是宿主替他判死刑）
///
/// 无论哪种附件，都会附一段**路径清单**：端点说「不支持这个格式」时，模型还能顺着
/// 路径自己用 `read_file` 去读 —— 不然那句报错就只是死路一条。
pub fn to_content(text: &str, items: &[Attachment]) -> Result<Value, String> {
    if items.is_empty() {
        return Ok(Value::String(text.to_string()));
    }

    let mut parts: Vec<Value> = Vec::with_capacity(items.len() + 2);
    // 正文为空时不发一个空的 text 块（没有内容的块不值得赌端点收不收）
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }
    let mut listing = String::from(ATTACH_MARK);
    for a in items {
        listing.push_str(&format!("- {} → {}\n", a.name, a.path.display()));
    }
    listing.push_str(
        "（以上文件已随本条消息一并发送；若端点提示某个格式不受支持，\
         请用 read_file 直接读取上面的路径。）",
    );
    parts.push(json!({"type": "text", "text": listing}));

    for a in items {
        let bytes = a.read()?;
        let data = format!("data:{};base64,{}", mime_of(a, &bytes), b64_encode(&bytes));
        match a.kind {
            Some(_) => parts.push(json!({"type": "image_url", "image_url": {"url": data}})),
            None => parts.push(json!({"type": "file", "file_data": data, "filename": a.name})),
        }
    }
    Ok(Value::Array(parts))
}

/// 落盘前把附件的**载荷**（base64）摘掉，只留「这里原本有个附件」的记录。
///
/// 为什么不存 base64：一张 1080p 截图转成 base64 就是 2–4 MB，而 `.jsonl` 是**逐条追加**的
/// 对话记录 —— 一张图会让这个文件永久胖几 MB，翻历史、读历史、乃至 git diff 全部遭殃。
/// 而附件本来就是**一次性**的（只附在发出去的那一条消息上），文件名与路径已经足够回答
/// 「当时发了什么」；模型想再看一眼，顺着消息里的路径重新 `attach_file` 一次就行 ——
/// 这也正是那个宿主工具存在的意义之一。
///
/// ⚠️ 图片 / 文件块是**整块丢掉**，不是换成占位 URL：那段历史被重新载入时会原样发给
/// 端点，一个假 URL 换来的是一句难懂的 400。丢掉之后剩下的是一条合法的纯文本消息。
///
/// 返回 `None` 表示没有可摘的东西（纯文本消息），调用点据此跳过重写。
pub fn strip_payloads(content: &Value) -> Option<Value> {
    let parts = content.as_array()?;
    let mut changed = false;
    let mut kept: Vec<Value> = Vec::with_capacity(parts.len());
    for p in parts {
        match p.get("type").and_then(|t| t.as_str()) {
            Some("image_url") | Some("file") => changed = true,
            _ => kept.push(p.clone()),
        }
    }
    if !changed {
        return None;
    }
    // 载荷全被摘光（理论上不会：正文与清单都是 text 块）时别留一个空数组 ——
    // 空 content 数组端点是不收的，退成空字符串至少是一个合法形状。
    if kept.is_empty() {
        return Some(Value::String(String::new()));
    }
    Some(Value::Array(kept))
}

/// 逐个 [`Attachment::probe`]，顺手去重，最后过整批闸门。
///
/// 返回 `(能挂上的, 一句句「给用户看的话」)`。**绝不因为一个文件不合格就把整批丢掉** ——
/// 用户拖了 5 个文件、其中一个是目录，合理的做法是挂 4 个并说清第 5 个为什么不行。
///
/// 两个入口（用户拖放 / 工具产出）共用它，所以两处对「什么能挂、什么不能」的回答
/// 必定一致 —— 这正是当初把判定收进本模块的原因。
pub fn probe_all(paths: &[PathBuf]) -> (Vec<Attachment>, Vec<String>) {
    let mut ok: Vec<Attachment> = Vec::with_capacity(paths.len());
    let mut bad: Vec<String> = Vec::new();
    for p in paths {
        match Attachment::probe(p.clone()) {
            // 同一个文件被声明两次 = 一个附件（插件把同一张图报两遍是常见的笔误）
            Ok(a) if ok.iter().any(|x| x.path == a.path) => {}
            Ok(a) => ok.push(a),
            Err(e) => bad.push(e),
        }
    }
    if let Err(e) = check_batch(&ok, 0) {
        bad.push(e);
        // 整批超限时全丢：留着前几个会让「到底发出去几个」变成一件要数的事
        ok.clear();
    }
    (ok, bad)
}

/// 从 `content` 里还原附件清单（`- 名字 → 路径` 的那些行）。
///
/// 用途只有一个：**重开一段旧对话时把待上传栏/气泡里的胶囊画回来**。
/// 图片的 `image_url` 块里没有文件名（协议里就没有这个字段），所以清单是
/// 「名字」的唯一来源 —— 这也是清单必须留成独立 part 的第二个理由。
pub fn names_from_content(v: &Value) -> Vec<String> {
    files_from_content(v).into_iter().map(|(n, _)| n).collect()
}

/// 从 `content` 里还原附件清单的**名字与路径**。
///
/// 与 [`names_from_content`] 同源（同一个清单块），只是多给出路径 —— 界面要拿它
/// **显示**图片（`gui/src/app.rs` 的解码队列），而 `image_url` 块里没有路径
///（只有 data URL），清单是唯一来源。
pub fn files_from_content(v: &Value) -> Vec<(String, PathBuf)> {
    let Some(parts) = v.as_array() else {
        return Vec::new();
    };
    for p in parts {
        if p.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
        }
        let Some(t) = p.get("text").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some(rest) = t.strip_prefix(ATTACH_MARK) else {
            continue;
        };
        return rest
            .lines()
            .filter_map(|line| {
                let line = line.strip_prefix("- ")?;
                // 「名字 → 路径」：名字里出现 ` → ` 的概率可以忽略，取第一个分隔符最稳
                let (name, path) = line.split_once(" → ")?;
                Some((name.trim().to_string(), PathBuf::from(path.trim())))
            })
            .collect();
    }
    Vec::new()
}

/// 填 `data:` URL 的 MIME。
///
/// 图片由魔数定；非图片再认两样最常见的（PDF / 纯文本），其余一律
/// `application/octet-stream`。**这个值只影响端点怎么解析载荷，不影响「收不收」** ——
/// 端点按实际内容判格式，所以填错也不会把一件事从「能收」变成「不能收」。
fn mime_of(a: &Attachment, bytes: &[u8]) -> &'static str {
    if let Some(k) = a.kind {
        return k.mime();
    }
    if bytes.starts_with(b"%PDF") {
        return "application/pdf";
    }
    // 前 8 KiB 没有 NUL 且能按 UTF-8 解出来 ⇒ 当文本。
    let head = &bytes[..bytes.len().min(8192)];
    if !head.contains(&0) && std::str::from_utf8(head).is_ok() {
        return "text/plain";
    }
    "application/octet-stream"
}

/// 标准 base64（含 `=` 补齐）。
///
/// 自己写而不是引 crate：本项目只有**编码**这一处需要它，而 base64 编码是个
/// 二十行的定式（解码才是有坑的那一半，而这里用不到）。零依赖也符合本项目
/// 对体积的一贯态度。正确性由本模块的已知向量测试钉住。
pub fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        // 不足 3 字节时，缺的那一两个 6 位组用 `=` 顶掉
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 的标准向量，覆盖「整 3 字节 / 余 1 / 余 2」三种长度。
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(b64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_high_bytes() {
        // 高位字节是手写编码器最容易翻车的地方（符号扩展）。
        assert_eq!(b64_encode(&[0xFF]), "/w==");
        assert_eq!(b64_encode(&[0xFF, 0xFF]), "//8=");
        assert_eq!(b64_encode(&[0xFF, 0xFF, 0xFF]), "////");
        assert_eq!(b64_encode(&[0x00, 0x00, 0x00]), "AAAA");
    }

    #[test]
    fn base64_length_is_always_a_multiple_of_four() {
        // 长度公式错一个字节，端点收到的就是坏图。
        for n in 0..64usize {
            let data = vec![0x5Au8; n];
            assert_eq!(b64_encode(&data).len() % 4, 0, "n={n}");
            assert_eq!(b64_encode(&data).len(), (n + 2) / 3 * 4, "n={n}");
        }
    }

    #[test]
    fn sniff_recognises_the_four_supported_formats() {
        assert_eq!(ImageKind::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(ImageKind::Jpeg));
        assert_eq!(ImageKind::sniff(b"\x89PNG\r\n\x1a\n\x00\x00"), Some(ImageKind::Png));
        assert_eq!(ImageKind::sniff(b"GIF89a.........."), Some(ImageKind::Gif));
        assert_eq!(ImageKind::sniff(b"GIF87a.........."), Some(ImageKind::Gif));
        assert_eq!(ImageKind::sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some(ImageKind::Webp));
    }

    #[test]
    fn sniff_rejects_near_misses() {
        // `RIFF` 但非 `WEBP`（例如 .wav）不能被当成图片
        assert_eq!(ImageKind::sniff(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
        // 只有 4 字节的 `RIFF` 头（还没读到 WEBP 那四个字节）
        assert_eq!(ImageKind::sniff(b"RIFF"), None);
        assert_eq!(ImageKind::sniff(b"hello world"), None);
        assert_eq!(ImageKind::sniff(b""), None);
    }

    #[test]
    fn content_is_a_plain_string_when_there_are_no_attachments() {
        // 这条是**零漂移**的保证：没有附件时序列化出来必须还是 JSON 字符串。
        let v = to_content("你好", &[]).unwrap();
        assert_eq!(v, Value::String("你好".into()));
    }

    #[test]
    fn content_becomes_parts_when_attachments_exist() {
        let d = std::env::temp_dir().join("aox-attach-parts");
        let _ = std::fs::create_dir_all(&d);
        // 一张最小的合法 PNG 头（内容不必是真图：本模块不验证像素）
        let png = d.join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();

        let a = Attachment::probe(&png).unwrap();
        assert_eq!(a.kind, Some(ImageKind::Png));
        assert_eq!(a.name, "a.png");

        let v = to_content("看看这张图", &[a]).unwrap();
        let parts = v.as_array().unwrap();
        assert_eq!(parts.len(), 3, "正文 + 清单 + 图片块");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "看看这张图", "用户正文必须原样，不被清单污染");
        // 清单占独立一块，且必须带路径 —— 端点拒收时模型要靠它去 read_file
        assert!(parts[1]["text"].as_str().unwrap().starts_with(ATTACH_MARK));
        assert!(parts[1]["text"].as_str().unwrap().contains("a.png"));
        assert_eq!(parts[2]["type"], "image_url");
        assert!(parts[2]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));

        // 清单能回读（重开旧对话时把胶囊画回来的唯一来源）
        assert_eq!(names_from_content(&v), vec!["a.png".to_string()]);

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 附件清单是宿主加的话，**不能**出现在用户气泡里 —— 否则界面上会多出一段
    /// 「- a.png → E:\…」，看起来像用户自己打的。
    #[test]
    fn flat_text_hides_the_host_listing_but_recovers_the_names() {
        let d = std::env::temp_dir().join("aox-attach-flat");
        let _ = std::fs::create_dir_all(&d);
        let f = d.join("report.pdf");
        std::fs::write(&f, b"%PDF-1.4 fake").unwrap();

        let a = Attachment::probe(&f).unwrap();
        let m = crate::message::Message::user_with_files("帮我看下", &[a]).unwrap();
        let flat = m.flat_text();
        assert!(flat.contains("帮我看下"), "{flat}");
        assert!(!flat.contains("→"), "路径清单不该出现在可读文本里：{flat}");

        assert_eq!(names_from_content(m.content.as_ref().unwrap()), vec!["report.pdf".to_string()]);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn non_image_goes_out_as_a_file_part_and_is_not_refused_locally() {
        let d = std::env::temp_dir().join("aox-attach-filepart");
        let _ = std::fs::create_dir_all(&d);
        // 一个 .xyz 后缀、内容也不认识的文件 —— 宿主**不该**拦它（用户要的就是交给端点判）
        let odd = d.join("note.xyz");
        std::fs::write(&odd, b"just some text").unwrap();

        let a = Attachment::probe(&odd).expect("非图片不该在 probe 阶段被拒");
        assert_eq!(a.kind, None);
        let v = to_content("读一下", &[a]).unwrap();
        let parts = v.as_array().unwrap();
        assert_eq!(parts[2]["type"], "file", "0=正文 1=清单 2=文件块");
        assert_eq!(parts[2]["filename"], "note.xyz");
        assert!(parts[2]["file_data"].as_str().unwrap().starts_with("data:text/plain;base64,"));

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn oversized_image_is_refused_before_it_leaves_the_host() {
        let d = std::env::temp_dir().join("aox-attach-bigimg");
        let _ = std::fs::create_dir_all(&d);
        let p = d.join("big.png");
        // 造一个「PNG 头 + 32 MiB + 1 字节」——probe 只看头与 stat，不必真写图
        let mut f = std::fs::File::create(&p).unwrap();
        use std::io::Write;
        f.write_all(b"\x89PNG\r\n\x1a\n").unwrap();
        f.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        drop(f);

        let err = Attachment::probe(&p).unwrap_err();
        assert!(err.contains("32 MiB"), "错误里要说清是哪个闸门与数值：{err}");
        assert!(err.contains("压缩"), "还要给出下一步：{err}");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn batch_gate_counts_base64_inflation() {
        // 恰好压在 48 MiB 之下：36 MiB 原始 ⇒ 48 MiB 编码，不能再叠正文
        let mk = |size: u64| Attachment {
            path: PathBuf::from("x"),
            name: "x".into(),
            size,
            kind: None,
        };
        let ok = 36 * 1024 * 1024u64;
        assert!(check_batch(&[mk(ok)], 0).is_ok(), "36 MiB 编码后正好 48 MiB，应当放行");
        assert!(check_batch(&[mk(ok)], 4096).is_err(), "再叠一点正文就超了，必须拦");
        assert!(check_batch(&[mk(1)], 0).is_ok());
    }

    #[test]
    fn batch_gate_counts_images() {
        let mk = |kind: Option<ImageKind>| Attachment {
            path: PathBuf::from("x"),
            name: "x".into(),
            size: 1,
            kind,
        };
        let many: Vec<_> = (0..MAX_IMAGES + 1).map(|_| mk(Some(ImageKind::Png))).collect();
        assert!(check_batch(&many, 0).is_err());
        // 非图片不吃这条闸门（它们由端点逐个判）
        let files: Vec<_> = (0..MAX_IMAGES + 1).map(|_| mk(None)).collect();
        assert!(check_batch(&files, 0).is_ok());
    }

    #[test]
    fn directory_is_not_an_attachment() {
        let d = std::env::temp_dir().join("aox-attach-dir");
        let _ = std::fs::create_dir_all(&d);
        let err = Attachment::probe(&d).unwrap_err();
        assert!(err.contains("目录"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `probe_all` 的口味：**能挂的照挂，不能挂的说清理由**，绝不整批丢掉。
    ///
    /// 这条行为两个入口共用（用户拖放 / 工具产出）。用户拖 5 个文件、其中一个是目录，
    /// 合理的做法是挂 4 个并说第 5 个为什么不行，而不是「有一个不合格，全都别要了」。
    #[test]
    fn probe_all_keeps_the_good_ones_and_explains_the_bad_ones() {
        let d = std::env::temp_dir().join("aox-attach-all");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let good = d.join("good.png");
        std::fs::write(&good, b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        let subdir = d.join("sub");
        std::fs::create_dir_all(&subdir).unwrap();
        let missing = d.join("nope.bin");

        let (ok, bad) = probe_all(&[good.clone(), subdir, missing]);
        assert_eq!(ok.len(), 1, "合格的那个必须留下");
        assert_eq!(ok[0].name, "good.png");
        assert_eq!(bad.len(), 2, "另外两个各要有一句话：{bad:?}");

        // 同一个路径给两次 = 一个附件（插件把同一张图报两遍是常见的笔误）
        let (ok, _) = probe_all(&[good.clone(), good]);
        assert_eq!(ok.len(), 1);

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 落盘时把 base64 摘掉，**但清单必须留着** —— 否则重开旧对话时，
    /// 界面上就再也看不出「这条消息当时带了什么」。
    #[test]
    fn strip_payloads_drops_the_bytes_but_keeps_the_listing() {
        let d = std::env::temp_dir().join("aox-attach-strip");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let png = d.join("big.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();

        let a = Attachment::probe(&png).unwrap();
        let v = to_content("看这张", &[a]).unwrap();
        assert!(v.to_string().contains("base64"), "前提：编码后确实带着载荷");

        let slim = strip_payloads(&v).expect("带附件的消息一定要被改写");
        let s = slim.to_string();
        assert!(!s.contains("base64"), "载荷必须摘掉：{s}");
        assert!(!s.contains("image_url"), "图片块整块丢掉，不留假的 url：{s}");
        assert!(s.contains("big.png"), "清单要留着，否则历史里看不出带了什么：{s}");
        assert!(s.contains("看这张"), "正文当然要留着：{s}");
        // 剩下的是合法形状（能被解析成一条消息，且不再带图片）
        let m: crate::message::Message = serde_json::from_value(json!({
            "role": "user", "content": slim
        }))
        .unwrap();
        assert!(m.flat_text().contains("看这张"), "{}", m.flat_text());
        assert_eq!(names_from_content(m.content.as_ref().unwrap()), vec!["big.png".to_string()]);

        // 纯文本消息不动（`None` = 没改），调用点据此跳过重写
        assert!(strip_payloads(&Value::String("你好".into())).is_none());
        assert!(strip_payloads(&json!([{"type": "text", "text": "只有字"}])).is_none());

        let _ = std::fs::remove_dir_all(&d);
    }
}
