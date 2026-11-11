//! A minimal SVG renderer: the three shapes `<rect>` / `<line>` / `<text>`, plus `viewBox` and a couple of fill /...
//! When the model writes a ```` ```svg ```` fence in its reply, this draws it inside the chat (see the matching...
//! # Why not HTML.

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, Ui, Vec2};

use crate::theme::P;

// Data

/// A paint value. Three variants rather than `Option<Color32>`: `none` (explicitly no fill) and "not written" (use...
#[derive(Clone, Copy, PartialEq, Debug)]
enum Paint {
    None,
    Custom(Color32),
    /// Attribute not written ⇒ use the color matching the current theme at render time
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

// Parsing

/// Parse an SVG. On failure it returns **a human-readable sentence** (shown in the chat).
pub fn parse(src: &str) -> Result<Drawing, String> {
    let mut shapes: Vec<Shape> = Vec::new();
    let mut vb: Option<(f32, f32)> = None;

    for (name, attrs, body) in tags(src) {
        match name.as_str() {
            // Fall back to the shapes' actual bounds at the end (see function tail) — models occasionally forget viewBox...
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
                    continue; // A box with no area can't be drawn, and there's nothing to say about it
                }
                shapes.push(Shape::Rect {
                    x: num(&attrs, "x").unwrap_or(0.0),
                    y: num(&attrs, "y").unwrap_or(0.0),
                    w,
                    h,
                    rx: num(&attrs, "rx").unwrap_or(0.0),
                    fill: paint(&attrs, "fill"),
                    // When a box has no stroke written, **default to stroking**: filling a flat
                    // color with no edge is nearly invisible as a box on a light background.
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
                    // A line can only be stroked: with no stroke written, use the theme color, never `none`
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
            // Unrecognized elements (`defs` / `style` / `marker` / `g` / `path` ...) are skipped.
            // See deviation #2 in the module docs: hiding the whole diagram over that is not worth it.
            _ => {}
        }
    }

    if shapes.is_empty() {
        return Err(aox::i18n::t("nothing drawable in this SVG (only rect / line / text are supported)").into());
    }
    let (vb_w, vb_h) = vb.unwrap_or_else(|| bbox(&shapes));
    if vb_w <= 0.0 || vb_h <= 0.0 {
        return Err(aox::i18n::t("this SVG's size is 0 (check the viewBox)").into());
    }
    Ok(Drawing { vb_w, vb_h, shapes })
}

/// Size fallback: how much space the shapes actually occupy. Add a little margin, otherwise a line
/// hugging the edge gets half clipped off.
fn bbox(shapes: &[Shape]) -> (f32, f32) {
    let (mut mx, mut my) = (1.0f32, 1.0f32);
    for s in shapes {
        let (x, y) = match s {
            Shape::Rect { x, y, w, h, .. } => (x + w, y + h),
            Shape::Line { x1, y1, x2, y2, .. } => (x1.max(*x2), y1.max(*y2)),
            // Text width must be estimated: `font-size` × 0.6 × char count (only for fallback;
            // accuracy doesn't matter)
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
    // Only look at the last two (width, height): the first two are the origin, and we do no
    // translation — the shapes already use the same coordinate system.
    match n.len() {
        4 if n[2] > 0.0 && n[3] > 0.0 => Some((n[2], n[3])),
        _ => None,
    }
}

fn num(attrs: &Attrs, key: &str) -> Option<f32> {
    let raw = attrs.get(key)?;
    // Tolerate `px` / `pt` suffixes: models often carry units, and in SVG these are all "user units"
    let raw = raw.trim().trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%');
    raw.trim().parse().ok()
}

fn paint(attrs: &Attrs, key: &str) -> Paint {
    match attrs.get(key) {
        None => Paint::Auto,
        Some(v) => Paint::from_str(v).unwrap_or(Paint::Auto),
    }
}

/// The stroke default for a box. Kept separate from `paint` only to spell out that "the default is
/// Auto, not None".
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
        // Unrecognized (a gradient `url(#g1)` and the like) counts as not written: showing the
        // theme color beats not drawing the diagram at all.
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

/// Only the most common names are recognized.
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

// Minimal XML scan

type Attrs = std::collections::HashMap<String, String>;

/// Scan a stretch of SVG into `(element name, attributes, text content)`.
fn tags(src: &str) -> Vec<(String, Attrs, String)> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != '<' {
            i += 1;
            continue;
        }
        // Comment / declaration / DOCTYPE: jump to `>` (a comment must jump to `-->`)
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
        // ️ Only `<text>` collects body text.
        if !self_closing && name == "text" {
            if let Some(end) = find(&b, next, &format!("</{name}")) {
                body = b[next..end].iter().collect();
                next = match find(&b, end, ">") {
                    Some(j) => j + 1,
                    None => b.len(),
                };
            } else {
                // Not closed: treat everything remaining as body text (the model occasionally
                // omits the closing tag)
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

/// Split attributes by "don't cut inside quotes".
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

// Drawing

/// Draw a stretch of SVG into `ui`. Returns the height it occupies (0 = nothing drawn).
pub fn render(ui: &mut Ui, src: &str, max_w: f32) -> f32 {
    let d = match parse(src) {
        Ok(d) => d,
        Err(e) => {
            ui.label(
                egui::RichText::new(aox::tf!("the diagram failed to render: {}", e))
                    .color(P::text_faint())
                    .size(12.5),
            );
            return 0.0;
        }
    };
    let scale = (max_w.min(d.vb_w) / d.vb_w).max(0.02);
    let size = Vec2::new(d.vb_w * scale, d.vb_h * scale);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());

    // A light backing plus clipping: the same vocabulary as code blocks, and clipping guarantees a
    // line escaping the viewBox doesn't get drawn elsewhere
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, CornerRadius::same(12), P::code_bg());

    let px = |x: f32| rect.left() + x * scale;
    let py = |y: f32| rect.top() + y * scale;
    // The stroke width scales too, but **never thinner than 1 pixel** — down at 0.4px a line
    // flickers in and out
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
                    // SVG's `y` is the baseline, while egui only has bounding-box anchors — approximate with BOTTOM. With descenders...
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
        parse(src).expect("should parse successfully").shapes
    }

    #[test]
    fn parses_the_three_supported_shapes() {
        let s = shapes(
            r##"<svg viewBox="0 0 200 100">
                 <rect x="10" y="20" width="80" height="30" rx="4" fill="#eee" stroke="#333" stroke-width="2"/>
                 <line x1="90" y1="35" x2="140" y2="35" stroke="black"/>
                 <text x="100" y="80" font-size="12" text-anchor="middle">Start</text>
               </svg>"##,
        );
        assert_eq!(s.len(), 3);
        match &s[0] {
            Shape::Rect { x, y, w, h, rx, fill, sw, .. } => {
                assert_eq!((*x, *y, *w, *h, *rx), (10.0, 20.0, 80.0, 30.0, 4.0));
                assert_eq!(*fill, Paint::Custom(Color32::from_rgb(0xee, 0xee, 0xee)));
                assert_eq!(*sw, 2.0);
            }
            other => panic!("the first one should be a rect: {other:?}"),
        }
        match &s[2] {
            Shape::Text { text, size, anchor, .. } => {
                assert_eq!(text, "Start");
                assert_eq!(*size, 12.0);
                assert_eq!(*anchor, Anchor::Middle);
            }
            other => panic!("the third one should be text: {other:?}"),
        }
    }

    /// Unrecognized elements must be **skipped rather than erroring**: the model easily writes
    /// `<defs>` / `<style>` on the side. Hiding the whole diagram over that is the worst reaction.
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
        assert_eq!(s.len(), 2, "only that rect and line should remain");
    }

    /// A stretch of SVG with nothing drawable in it must **say why**, not draw an empty frame.
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
        // Unrecognized (a gradient reference) falls back to "not written" rather than condemning the whole diagram
        assert_eq!(Paint::from_str("url(#g1)"), Some(Paint::Auto));
    }

    /// Attribute values with spaces must **not be torn apart**: `font-family="Microsoft YaHei"`
    /// torn apart shifts every following attribute.
    #[test]
    fn attribute_values_may_contain_spaces() {
        let (name, attrs) = split_tag(r#"text x="1" font-family="Microsoft YaHei" y="2""#);
        assert_eq!(name, "text");
        assert_eq!(attrs.get("font-family").map(String::as_str), Some("Microsoft YaHei"));
        assert_eq!(attrs.get("y").map(String::as_str), Some("2"), "the attributes after it must not go out of alignment");
    }

    #[test]
    fn viewbox_wins_over_width_height_and_units_are_tolerated() {
        let d = parse(r#"<svg width="100px" height="50px" viewBox="0 0 800 400"><rect width="1" height="1"/></svg>"#).unwrap();
        assert_eq!((d.vb_w, d.vb_h), (800.0, 400.0), "viewBox wins");
        let d = parse(r#"<svg width="100px" height="50px"><rect width="1" height="1"/></svg>"#).unwrap();
        assert_eq!((d.vb_w, d.vb_h), (100.0, 50.0), "a px suffix must be recognized too");
    }

    /// When viewBox is forgotten, fall back to the shapes — "draw nothing" is the worst reaction.
    #[test]
    fn a_missing_viewbox_falls_back_to_the_shape_bounds() {
        let d = parse(r#"<svg><rect x="0" y="0" width="60" height="40"/></svg>"#).unwrap();
        assert!(d.vb_w > 60.0 && d.vb_h > 40.0, "the fallback size must enclose the shapes: {}×{}", d.vb_w, d.vb_h);
    }

    #[test]
    fn comments_and_declarations_do_not_trip_the_scanner() {
        let s = shapes(
            r#"<?xml version="1.0"?>
               <!-- note: this is a flowchart -->
               <svg viewBox="0 0 10 10"><rect width="5" height="5"/></svg>"#,
        );
        assert_eq!(s.len(), 1);
    }
}
