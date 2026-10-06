//! 最简 SVG 渲染：`<rect>` / `<line>` / `<text>` 三种形状，外加 `viewBox` 与几个
//! 填充 / 描边属性。
//!
//! 模型在正文里写 ```` ```svg ```` 围栏，这一段就把它在对话里**画出来**
//!（见 `markdown.rs` 里那一条分支）。零协议改动 —— markdown 本来就有围栏代码块，
//! 而模型本来就擅长写 SVG。
//!
//! # 为什么不做 HTML
//!
//! 「用 HTML 直接在界面上渲染」听起来只差一步，实际差的是一个**排版引擎**（盒模型、
//! 行内流、层叠、字体度量……）。SVG 不一样：它是一堆绝对坐标的图形，不需要排版 ——
//! 而流程图、结构图、时序图正好就是「一堆绝对坐标的图形」。
//!
//! # 为什么只有三种形状
//!
//! 这是用户选的范围（2026-10-06）：方框、连线、文字，够画流程图式的图。
//! 圆角、曲线、旋转、椭圆、渐变都不支持。箭头用两条短线拼（`<line>` 画不出三角形）。
//!
//! # 两个刻意的偏离
//!
//! 1. **没写 `fill` / `stroke` 时用当前主题的颜色**，而不是 SVG 规范里的黑色。
//!    深色主题下黑色方框直接看不见，而「没写颜色」在模型生成的流程图里恰恰最常见。
//! 2. **认不出的元素直接跳过、不算错**：模型很容易顺手写 `<defs>` / `<style>` /
//!    `<marker>`（它们本来就是 SVG 的常规组成部分）。为此整张图不显示，得不偿失。

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, Ui, Vec2};

use crate::theme::P;

// ─────────────────────────── 数据 ───────────────────────────

/// 颜色取值。三支而不是 `Option<Color32>`：`none`（明确不要填充）与「没写」
/// （用主题色）是两件事，混成一个 `None` 就再也分不开了。
#[derive(Clone, Copy, PartialEq, Debug)]
enum Paint {
    None,
    Custom(Color32),
    /// 属性没写 ⇒ 渲染时用当前主题对应的颜色
    Auto,
}

impl Paint {
    fn or(self, fallback: Color32) -> Option<Color32> {
        match self {
            Paint::None => None,
            Paint::Custom(c) => Some(c),
            Paint::Auto => Some(fallback),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Anchor {
    Start,
    Middle,
    End,
}

#[derive(Clone, Debug)]
enum Shape {
    Rect { x: f32, y: f32, w: f32, h: f32, rx: f32, fill: Paint, stroke: Paint, sw: f32 },
    Line { x1: f32, y1: f32, x2: f32, y2: f32, stroke: Paint, sw: f32 },
    Text { x: f32, y: f32, text: String, size: f32, fill: Paint, anchor: Anchor },
}

#[derive(Debug)]
pub struct Drawing {
    pub vb_w: f32,
    pub vb_h: f32,
    shapes: Vec<Shape>,
}

// ─────────────────────────── 解析 ───────────────────────────

/// 解析一段 SVG。失败给的是**给人看的一句话**（会显示在对话里）。
pub fn parse(src: &str) -> Result<Drawing, String> {
    let mut shapes: Vec<Shape> = Vec::new();
    let mut vb: Option<(f32, f32)> = None;

    for (name, attrs, body) in tags(src) {
        match name.as_str() {
            // 根：尺寸只认这三样，顺序是 viewBox → width/height。都没有则最后按
            // 形状的实际范围兜底（见函数尾）—— 模型偶尔会忘写 viewBox，
            // 而那时「什么都不画」是最差的反应。
            "svg" => {
                vb = parse_viewbox(&attrs).or_else(|| {
                    let w = num(&attrs, "width");
                    let h = num(&attrs, "height");
                    match (w, h) {
                        (Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some((w, h)),
                        _ => None,
                    }
                });
            }
            "rect" => {
                let w = num(&attrs, "width").unwrap_or(0.0);
                let h = num(&attrs, "height").unwrap_or(0.0);
                if w <= 0.0 || h <= 0.0 {
                    continue; // 没有面积的方框画不出来，也没什么可说的
                }
                shapes.push(Shape::Rect {
                    x: num(&attrs, "x").unwrap_or(0.0),
                    y: num(&attrs, "y").unwrap_or(0.0),
                    w,
                    h,
                    rx: num(&attrs, "rx").unwrap_or(0.0),
                    fill: paint(&attrs, "fill"),
                    // 方框没写 stroke 时**默认描边**：只填一块纯色、没有边，
                    // 在浅色底上几乎看不出是个框。
                    stroke: stroke_of(&attrs, Paint::Auto),
                    sw: num(&attrs, "stroke-width").unwrap_or(1.5),
                });
            }
            "line" => {
                shapes.push(Shape::Line {
                    x1: num(&attrs, "x1").unwrap_or(0.0),
                    y1: num(&attrs, "y1").unwrap_or(0.0),
                    x2: num(&attrs, "x2").unwrap_or(0.0),
                    y2: num(&attrs, "y2").unwrap_or(0.0),
                    // 线只可能是描边出来的：没写 stroke 就用主题色，绝不用 `none`
                    stroke: match paint(&attrs, "stroke") {
                        Paint::None => Paint::Auto,
                        p => p,
                    },
                    sw: num(&attrs, "stroke-width").unwrap_or(1.5),
                });
            }
            "text" => {
                let t = body.trim();
                if t.is_empty() {
                    continue;
                }
                shapes.push(Shape::Text {
                    x: num(&attrs, "x").unwrap_or(0.0),
                    y: num(&attrs, "y").unwrap_or(0.0),
                    text: t.to_string(),
                    size: num(&attrs, "font-size").unwrap_or(14.0),
                    fill: paint(&attrs, "fill"),
                    anchor: match attrs.get("text-anchor").map(String::as_str) {
                        Some("middle") => Anchor::Middle,
                        Some("end") => Anchor::End,
                        _ => Anchor::Start,
                    },
                });
            }
            // 认不出的元素（`defs` / `style` / `marker` / `g` / `path` …）跳过。
            // 见模块文档第 2 条偏离：为此让整张图不显示，得不偿失。
            _ => {}
        }
    }

    if shapes.is_empty() {
        return Err("这段 SVG 里没有能画的东西（只认 rect / line / text）".into());
    }
    let (vb_w, vb_h) = vb.unwrap_or_else(|| bbox(&shapes));
    if vb_w <= 0.0 || vb_h <= 0.0 {
        return Err("这段 SVG 的尺寸是 0（检查 viewBox）".into());
    }
    Ok(Drawing { vb_w, vb_h, shapes })
}

/// 尺寸兜底：形状实际占多大。加一点边距，否则贴着边的线会被裁掉一半。
fn bbox(shapes: &[Shape]) -> (f32, f32) {
    let (mut mx, mut my) = (1.0f32, 1.0f32);
    for s in shapes {
        let (x, y) = match s {
            Shape::Rect { x, y, w, h, .. } => (x + w, y + h),
            Shape::Line { x1, y1, x2, y2, .. } => (x1.max(*x2), y1.max(*y2)),
            // 文字的宽度得估：`font-size` 的 0.6 倍 × 字数（只用来兜底，估不准也没关系）
            Shape::Text { x, y, text, size, .. } => {
                (*x + text.chars().count() as f32 * size * 0.6, *y + size * 0.3)
            }
        };
        mx = mx.max(x);
        my = my.max(y);
    }
    (mx + 4.0, my + 4.0)
}

fn parse_viewbox(attrs: &Attrs) -> Option<(f32, f32)> {
    let v = attrs.get("viewBox").or_else(|| attrs.get("viewbox"))?;
    let n: Vec<f32> = v
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    // 只看后两位（宽、高）：前面两位是原点，我们不做平移 —— 形状本来就用同一套坐标
    match n.len() {
        4 if n[2] > 0.0 && n[3] > 0.0 => Some((n[2], n[3])),
        _ => None,
    }
}

fn num(attrs: &Attrs, key: &str) -> Option<f32> {
    let raw = attrs.get(key)?;
    // `px` / `pt` 后缀容错：模型常带单位，而 SVG 里这些都是「用户单位」
    let raw = raw.trim().trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%');
    raw.trim().parse().ok()
}

fn paint(attrs: &Attrs, key: &str) -> Paint {
    match attrs.get(key) {
        None => Paint::Auto,
        Some(v) => Paint::from_str(v).unwrap_or(Paint::Auto),
    }
}

/// 方框的描边缺省。与 `paint` 分开只为写明「默认是 Auto 而不是 None」这件事。
fn stroke_of(attrs: &Attrs, default: Paint) -> Paint {
    match attrs.get("stroke") {
        None => default,
        Some(v) => Paint::from_str(v).unwrap_or(Paint::Auto),
    }
}

impl Paint {
    fn from_str(v: &str) -> Option<Paint> {
        let s = v.trim();
        if s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("transparent") {
            return Some(Paint::None);
        }
        if let Some(c) = hex_color(s) {
            return Some(Paint::Custom(c));
        }
        if let Some(c) = rgb_func(s) {
            return Some(Paint::Custom(c));
        }
        if let Some(c) = named(s) {
            return Some(Paint::Custom(c));
        }
        // 认不出来（`url(#g1)` 渐变之类）就当没写：显示主题色，总比整张图不画强
        Some(Paint::Auto)
    }
}

fn hex_color(s: &str) -> Option<Color32> {
    let h = s.strip_prefix('#')?;
    let d = |i: usize, n: usize| u8::from_str_radix(&h[i..i + n], 16).ok();
    match h.len() {
        3 => {
            let (r, g, b) = (d(0, 1)?, d(1, 1)?, d(2, 1)?);
            Some(Color32::from_rgb(r * 17, g * 17, b * 17))
        }
        6 => Some(Color32::from_rgb(d(0, 2)?, d(2, 2)?, d(4, 2)?)),
        8 => Some(Color32::from_rgba_unmultiplied(d(0, 2)?, d(2, 2)?, d(4, 2)?, d(6, 2)?)),
        _ => None,
    }
}

fn rgb_func(s: &str) -> Option<Color32> {
    let inner = s.strip_prefix("rgb(")?.strip_suffix(')')?;
    let n: Vec<u8> = inner
        .split(',')
        .filter_map(|p| p.trim().parse::<f32>().ok())
        .map(|v| v.clamp(0.0, 255.0) as u8)
        .collect();
    (n.len() == 3).then(|| Color32::from_rgb(n[0], n[1], n[2]))
}

/// 只认最常见的几个名字。
///
/// 不引一张 147 个名字的表：那玩意儿会占掉这个模块一半的字节，而模型画流程图
/// 用的颜色翻来覆去就是这几个（其余一律走 `#rrggbb`）。
fn named(s: &str) -> Option<Color32> {
    let c = match s.to_ascii_lowercase().as_str() {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "orange" => (255, 165, 0),
        "gray" | "grey" => (128, 128, 128),
        "lightgray" | "lightgrey" => (211, 211, 211),
        "darkgray" | "darkgrey" => (169, 169, 169),
        _ => return None,
    };
    Some(Color32::from_rgb(c.0, c.1, c.2))
}

// ─────────────────────────── 极简 XML 扫描 ───────────────────────────

type Attrs = std::collections::HashMap<String, String>;

/// 把一段 SVG 扫成 `(元素名, 属性, 文本内容)`。
///
/// 手写而不是引 `quick-xml`：这里要认的东西少到可怜（无命名空间、无实体、无 CDATA、
/// 属性值只可能是引号包起来的一串字符），而为一个「画方框」的功能加一个 XML 库
/// 不合算 —— 与 `attach::b64_encode` 手写是同一个取舍。
fn tags(src: &str) -> Vec<(String, Attrs, String)> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != '<' {
            i += 1;
            continue;
        }
        // 注释 / 声明 / DOCTYPE：跳到 `>`（注释要跳到 `-->`）
        if b[i..].starts_with(&['<', '!', '-', '-']) {
            i = match find(&b, i, "-->") {
                Some(j) => j + 3,
                None => break,
            };
            continue;
        }
        if b[i + 1..].first().is_some_and(|c| *c == '!' || *c == '?') {
            i = match find(&b, i, ">") {
                Some(j) => j + 1,
                None => break,
            };
            continue;
        }
        let Some(gt) = find(&b, i, ">") else { break };
        let inner: String = b[i + 1..gt].iter().collect();
        let self_closing = inner.trim_end().ends_with('/');
        let (name, attrs) = split_tag(inner.trim().trim_end_matches('/'));
        let mut body = String::new();
        let mut next = gt + 1;
        // ⚠️ **只有 `<text>` 收正文**。
        //
        // 其余元素（`<svg>` / `<g>` / `<defs>` …）的内容要继续当元素扫 —— 对它们也
        // 做「跳到闭合标签」就等于把整棵子树吃掉，一个形状都解析不出来。
        // 这个错第一版就犯了，表现是「什么都画不出来」而代码看起来毫无问题。
        //
        // 闭合标签本身（`</svg>`）会被 `split_tag` 切成一个叫 `/svg` 的名字，
        // 在匹配里自然落进「认不出的元素」那一支，无害。
        if !self_closing && name == "text" {
            if let Some(end) = find(&b, next, &format!("</{name}")) {
                body = b[next..end].iter().collect();
                next = match find(&b, end, ">") {
                    Some(j) => j + 1,
                    None => b.len(),
                };
            } else {
                // 没闭合：把剩下的都当正文（模型偶尔漏闭合结尾）
                body = b[next..].iter().collect();
                next = b.len();
            }
        }
        out.push((name, attrs, body));
        i = next;
    }
    out
}

fn find(b: &[char], from: usize, pat: &str) -> Option<usize> {
    let p: Vec<char> = pat.chars().collect();
    if p.is_empty() || from + p.len() > b.len() {
        return None;
    }
    (from..=b.len() - p.len()).find(|&i| b[i..i + p.len()] == p[..])
}

/// `rect x="1" y="2"` → `("rect", {x: 1, y: 2})`
fn split_tag(s: &str) -> (String, Attrs) {
    let mut it = s.split_whitespace();
    let name = it.next().unwrap_or("").to_ascii_lowercase();
    let mut attrs = Attrs::new();
    let rest = s[name.len().min(s.len())..].trim();
    for piece in split_attrs(rest) {
        if let Some((k, v)) = piece.split_once('=') {
            let v = v.trim().trim_matches(['"', '\'']);
            attrs.insert(k.trim().to_ascii_lowercase(), v.to_string());
        }
    }
    (name, attrs)
}

/// 按「引号里的内容不切」来拆属性。
///
/// 不能对空白直接 `split_whitespace`：`text-anchor="middle"` 没事，但正文里很可能有
/// `font-family="Microsoft YaHei"` 这种带空格的属性值 —— 一拆就把属性撕成两半，
/// 后面每一项都跟着错位。
fn split_attrs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    cur.push(c);
                } else if c.is_whitespace() {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                } else {
                    cur.push(c);
                }
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ─────────────────────────── 绘制 ───────────────────────────

/// 把一段 SVG 画进 `ui`。返回占用的高度（0 = 没画出来）。
///
/// 宽度按 `max_w` 收（不放大）：一张 200 宽的示意图拉满版心只会糊，
/// 而「它就是这么大」比一张虚掉的大图有用 —— 与 `images_block` 同一条判据。
pub fn render(ui: &mut Ui, src: &str, max_w: f32) -> f32 {
    let d = match parse(src) {
        Ok(d) => d,
        Err(e) => {
            ui.label(
                egui::RichText::new(format!("图没画出来：{e}"))
                    .color(P::text_faint())
                    .size(12.5),
            );
            return 0.0;
        }
    };
    let scale = (max_w.min(d.vb_w) / d.vb_w).max(0.02);
    let size = Vec2::new(d.vb_w * scale, d.vb_h * scale);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());

    // 一块浅底 + 裁切：与代码块同一套语汇，而裁切保证越出 viewBox 的线不会画到别处去
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, CornerRadius::same(12), P::code_bg());

    let px = |x: f32| rect.left() + x * scale;
    let py = |y: f32| rect.top() + y * scale;
    // 线宽也跟着缩，但**不细过 1 像素** —— 缩到 0.4px 时线会时隐时现
    let lw = |w: f32| (w * scale).max(1.0);

    for s in &d.shapes {
        match s {
            Shape::Rect { x, y, w, h, rx, fill, stroke, sw } => {
                let r = Rect::from_min_max(Pos2::new(px(*x), py(*y)), Pos2::new(px(x + w), py(y + h)));
                let radius = CornerRadius::same((rx * scale).round().clamp(0.0, 255.0) as u8);
                if let Some(c) = fill.or(P::surface_hi()) {
                    p.rect_filled(r, radius, c);
                }
                if let Some(c) = stroke.or(P::text_dim()) {
                    p.rect_stroke(
                        r,
                        radius,
                        Stroke::new(lw(*sw), c),
                        egui::StrokeKind::Inside,
                    );
                }
            }
            Shape::Line { x1, y1, x2, y2, stroke, sw } => {
                if let Some(c) = stroke.or(P::text_dim()) {
                    p.line_segment(
                        [Pos2::new(px(*x1), py(*y1)), Pos2::new(px(*x2), py(*y2))],
                        Stroke::new(lw(*sw), c),
                    );
                }
            }
            Shape::Text { x, y, text, size, fill, anchor } => {
                let a = match anchor {
                    // SVG 的 `y` 是**基线**，egui 只有包围盒锚点 —— 用 BOTTOM 近似。
                    // 有下伸部分（gjpqy）时略偏上，画流程图够用。
                    Anchor::Start => Align2::LEFT_BOTTOM,
                    Anchor::Middle => Align2::CENTER_BOTTOM,
                    Anchor::End => Align2::RIGHT_BOTTOM,
                };
                let c = fill.or(P::text());
                if let Some(c) = c {
                    p.text(
                        Pos2::new(px(*x), py(*y)),
                        a,
                        text,
                        FontId::proportional((size * scale).max(6.0)),
                        c,
                    );
                }
            }
        }
    }
    rect.height()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shapes(src: &str) -> Vec<Shape> {
        parse(src).expect("应当解析成功").shapes
    }

    #[test]
    fn parses_the_three_supported_shapes() {
        let s = shapes(
            r##"<svg viewBox="0 0 200 100">
                 <rect x="10" y="20" width="80" height="30" rx="4" fill="#eee" stroke="#333" stroke-width="2"/>
                 <line x1="90" y1="35" x2="140" y2="35" stroke="black"/>
                 <text x="100" y="80" font-size="12" text-anchor="middle">开始</text>
               </svg>"##,
        );
        assert_eq!(s.len(), 3);
        match &s[0] {
            Shape::Rect { x, y, w, h, rx, fill, sw, .. } => {
                assert_eq!((*x, *y, *w, *h, *rx), (10.0, 20.0, 80.0, 30.0, 4.0));
                assert_eq!(*fill, Paint::Custom(Color32::from_rgb(0xee, 0xee, 0xee)));
                assert_eq!(*sw, 2.0);
            }
            other => panic!("第一个应当是 rect：{other:?}"),
        }
        match &s[2] {
            Shape::Text { text, size, anchor, .. } => {
                assert_eq!(text, "开始");
                assert_eq!(*size, 12.0);
                assert_eq!(*anchor, Anchor::Middle);
            }
            other => panic!("第三个应当是 text：{other:?}"),
        }
    }

    /// 认不出的元素要**跳过而不是报错**：模型很容易顺手写 `<defs>` / `<style>`。
    /// 为此让整张图不显示是最差的一种反应。
    #[test]
    fn unknown_elements_are_skipped_not_fatal() {
        let s = shapes(
            r#"<svg viewBox="0 0 10 10">
                 <defs><marker id="a"><path d="M0 0 L4 2 L0 4Z"/></marker></defs>
                 <style>.x{fill:red}</style>
                 <g><rect width="4" height="4"/></g>
                 <line x1="0" y1="0" x2="9" y2="9" stroke="red"/>
               </svg>"#,
        );
        assert_eq!(s.len(), 2, "只该留下那个 rect 与 line");
    }

    /// 一段完全没有可画内容的 SVG 要**说清为什么**，而不是画一个空框。
    #[test]
    fn a_drawing_with_nothing_in_it_says_so() {
        let e = parse(r#"<svg viewBox="0 0 10 10"><path d="M0 0 L1 1"/></svg>"#).unwrap_err();
        assert!(e.contains("rect / line / text"), "{e}");
    }

    #[test]
    fn colors_cover_the_forms_models_actually_write() {
        assert_eq!(Paint::from_str("none"), Some(Paint::None));
        assert_eq!(Paint::from_str("#f00"), Some(Paint::Custom(Color32::from_rgb(255, 0, 0))));
        assert_eq!(
            Paint::from_str("#1a2b3c"),
            Some(Paint::Custom(Color32::from_rgb(0x1a, 0x2b, 0x3c)))
        );
        assert_eq!(Paint::from_str("rgb(1, 2, 3)"), Some(Paint::Custom(Color32::from_rgb(1, 2, 3))));
        assert_eq!(Paint::from_str("Black"), Some(Paint::Custom(Color32::from_rgb(0, 0, 0))));
        // 认不出来的（渐变引用）退回「没写」，而不是把整张图判死
        assert_eq!(Paint::from_str("url(#g1)"), Some(Paint::Auto));
    }

    /// 属性值里带空格时**不能撕开**：`font-family="Microsoft YaHei"` 一撕，
    /// 后面每一个属性都跟着错位。
    #[test]
    fn attribute_values_may_contain_spaces() {
        let (name, attrs) = split_tag(r#"text x="1" font-family="Microsoft YaHei" y="2""#);
        assert_eq!(name, "text");
        assert_eq!(attrs.get("font-family").map(String::as_str), Some("Microsoft YaHei"));
        assert_eq!(attrs.get("y").map(String::as_str), Some("2"), "后面的属性不能跟着错位");
    }

    #[test]
    fn viewbox_wins_over_width_height_and_units_are_tolerated() {
        let d = parse(r#"<svg width="100px" height="50px" viewBox="0 0 800 400"><rect width="1" height="1"/></svg>"#).unwrap();
        assert_eq!((d.vb_w, d.vb_h), (800.0, 400.0), "viewBox 优先");
        let d = parse(r#"<svg width="100px" height="50px"><rect width="1" height="1"/></svg>"#).unwrap();
        assert_eq!((d.vb_w, d.vb_h), (100.0, 50.0), "带 px 后缀也要认");
    }

    /// 忘了写 viewBox 时按形状兜底 —— 「什么都不画」是最差的反应。
    #[test]
    fn a_missing_viewbox_falls_back_to_the_shape_bounds() {
        let d = parse(r#"<svg><rect x="0" y="0" width="60" height="40"/></svg>"#).unwrap();
        assert!(d.vb_w > 60.0 && d.vb_h > 40.0, "兜底尺寸要包住形状：{}×{}", d.vb_w, d.vb_h);
    }

    #[test]
    fn comments_and_declarations_do_not_trip_the_scanner() {
        let s = shapes(
            r#"<?xml version="1.0"?>
               <!-- 说明：这是个流程图 -->
               <svg viewBox="0 0 10 10"><rect width="5" height="5"/></svg>"#,
        );
        assert_eq!(s.len(), 1);
    }
}
