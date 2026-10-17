//! Lightweight Markdown rendering: draw the model's Markdown reply as egui widgets.
//!
//! Supports: headings, paragraphs, unordered/ordered lists, block quotes, fenced code blocks,
//! inline code, **bold**, *italic*, links and tables. The goal is "good-looking and loses nothing",
//! not spec completeness.

use egui::{
    text::LayoutJob, Color32, CornerRadius, FontId, Frame, Label, Margin, RichText, Stroke,
    TextFormat, Ui,
};

use crate::syntax;
use crate::theme::{self, P};

#[derive(Debug, Clone, Default)]
struct Span {
    text: String,
    bold: bool,
    italic: bool,
    code: bool,
    link: bool,
}

#[derive(Debug, Clone)]
enum Block {
    Heading { level: u8, text: String },
    Para(String),
    Bullet { indent: usize, marker: Option<String>, text: String },
    Quote(String),
    Code { lang: String, lines: Vec<String> },
    Rule,
    Table { header: Vec<String>, rows: Vec<Vec<String>> },
}

fn is_rule(s: &str) -> bool {
    let t = s.trim();
    (t.len() >= 3) && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

fn bullet_prefix(s: &str) -> Option<(usize, Option<String>, String)> {
    let indent = (s.len() - s.trim_start().len()) / 2;
    let t = s.trim_start();
    for p in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(p) {
            return Some((indent, None, rest.to_string()));
        }
    }
    // Ordered list: 12. xxx
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && digits.len() <= 3 {
        let rest = &t[digits.len()..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some((indent, Some(format!("{digits}.")), r.to_string()));
        }
    }
    None
}

fn is_table_sep(s: &str) -> bool {
    let t = s.trim();
    if !t.contains('-') || !t.contains('|') {
        return false;
    }
    t.chars().all(|c| c == '-' || c == ':' || c == '|' || c == ' ')
}

fn split_row(s: &str) -> Vec<String> {
    let t = s.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn parse(md: &str) -> Vec<Block> {
    let lines: Vec<&str> = md.lines().collect();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        let raw = lines[i];
        let t = raw.trim();

        if t.is_empty() {
            i += 1;
            continue;
        }

        // Fenced code block
        if let Some(fence) = t.strip_prefix("```").or_else(|| t.strip_prefix("~~~")) {
            let lang = fence.trim().to_string();
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() {
                let l = lines[i].trim_start();
                if l.starts_with("```") || l.starts_with("~~~") {
                    i += 1;
                    break;
                }
                body.push(lines[i].to_string());
                i += 1;
            }
            out.push(Block::Code { lang, lines: body });
            continue;
        }

        if is_rule(t) {
            out.push(Block::Rule);
            i += 1;
            continue;
        }

        if t.starts_with('#') {
            let level = t.chars().take_while(|c| *c == '#').count().min(6) as u8;
            let text = t[level as usize..].trim().trim_end_matches('#').trim().to_string();
            out.push(Block::Heading { level, text });
            i += 1;
            continue;
        }

        if t.starts_with('>') {
            let mut buf = Vec::new();
            while i < lines.len() {
                let l = lines[i].trim_start();
                if let Some(rest) = l.strip_prefix('>') {
                    buf.push(rest.trim_start_matches(' ').to_string());
                    i += 1;
                } else {
                    break;
                }
            }
            out.push(Block::Quote(buf.join("\n")));
            continue;
        }

        // Table
        if t.contains('|') && i + 1 < lines.len() && is_table_sep(lines[i + 1]) {
            let header = split_row(t);
            i += 2;
            let mut rows = Vec::new();
            while i < lines.len() && lines[i].contains('|') && !lines[i].trim().is_empty() {
                rows.push(split_row(lines[i]));
                i += 1;
            }
            let n = header.len();
            for r in rows.iter_mut() {
                r.resize(n, String::new());
            }
            out.push(Block::Table { header, rows });
            continue;
        }

        if let Some((indent, marker, text)) = bullet_prefix(raw) {
            out.push(Block::Bullet { indent, marker, text });
            i += 1;
            continue;
        }

        // Ordinary paragraph: merge consecutive non-empty, non-block-starting lines
        let mut buf = vec![t.to_string()];
        i += 1;
        while i < lines.len() {
            let l = lines[i];
            let lt = l.trim();
            if lt.is_empty()
                || lt.starts_with('#')
                || lt.starts_with('>')
                || lt.starts_with("```")
                || lt.starts_with("~~~")
                || is_rule(lt)
                || bullet_prefix(l).is_some()
            {
                break;
            }
            buf.push(lt.to_string());
            i += 1;
        }
        out.push(Block::Para(buf.join(" ")));
    }

    out
}

fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&k| chars[k] == target)
}

fn find_pair(chars: &[char], from: usize, target: char) -> Option<usize> {
    let mut k = from;
    while k + 1 < chars.len() {
        if chars[k] == target && chars[k + 1] == target {
            return Some(k);
        }
        k += 1;
    }
    None
}

/// Inline parsing: bold / italic / inline code / links.
fn inline(s: &str) -> Vec<Span> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let flush = |out: &mut Vec<Span>, buf: &mut String| {
        if !buf.is_empty() {
            out.push(Span { text: std::mem::take(buf), ..Default::default() });
        }
    };

    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];

        if c == '`' {
            if let Some(end) = find_char(&chars, i + 1, '`') {
                flush(&mut out, &mut buf);
                out.push(Span {
                    text: chars[i + 1..end].iter().collect(),
                    code: true,
                    ..Default::default()
                });
                i = end + 1;
                continue;
            }
        }

        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_pair(&chars, i + 2, '*') {
                let inner: String = chars[i + 2..end].iter().collect();
                if !inner.is_empty() {
                    flush(&mut out, &mut buf);
                    for mut sp in inline(&inner) {
                        sp.bold = true;
                        out.push(sp);
                    }
                    i = end + 2;
                    continue;
                }
            }
        }

        if (c == '*' || c == '_') && i + 1 < chars.len() {
            if let Some(end) = find_char(&chars, i + 1, c) {
                if end > i + 1 {
                    let inner: String = chars[i + 1..end].iter().collect();
                    flush(&mut out, &mut buf);
                    for mut sp in inline(&inner) {
                        sp.italic = true;
                        out.push(sp);
                    }
                    i = end + 1;
                    continue;
                }
            }
        }

        if c == '[' && i + 1 < chars.len() {
            if let Some(close) = find_char(&chars, i + 1, ']') {
                if close + 1 < chars.len() && chars[close + 1] == '(' {
                    if let Some(paren) = find_char(&chars, close + 2, ')') {
                        let text: String = chars[i + 1..close].iter().collect();
                        if !text.is_empty() {
                            flush(&mut out, &mut buf);
                            out.push(Span { text, link: true, ..Default::default() });
                            i = paren + 1;
                            continue;
                        }
                    }
                }
            }
        }

        buf.push(c);
        i += 1;
    }
    flush(&mut out, &mut buf);
    out
}

/// Line-height factor.
///
/// Font size only sets how tall the glyphs are; the gap between lines is governed by
/// `TextFormat::line_height`. A 1.3× leading is cramped for CJK body text — at full line
/// width the rows nearly touch — so we use 1.5 here. A nice side effect: the inline-code
/// "chip" background is painted per glyph rectangle, and a looser leading means the chips
/// no longer glue together vertically.
const LEADING: f32 = 1.5;

/// The breathing room between blocks. The default 11 is too small; two paragraphs nearly touch.
const BLOCK_GAP: f32 = 15.0;

fn job_of(spans: &[Span], size: f32, color: Color32, wrap: f32, force_bold: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap.max(40.0);
    job.wrap.break_anywhere = false;
    for sp in spans {
        let strong = sp.bold || force_bold;
        let font: FontId = if strong {
            theme::bold_font(size)
        } else if sp.code {
            theme::mono_font(size - 1.0)
        } else {
            theme::body_font(size)
        };
        let col = if sp.code {
            P::ink()
        } else if sp.link {
            P::ink()
        } else if strong && !theme::bold_available() {
            Color32::from_rgb(0xFF, 0xFF, 0xFF)
        } else {
            color
        };
        let fmt = TextFormat {
            font_id: font,
            color: col,
            line_height: Some(size * LEADING),
            italics: sp.italic,
            background: if sp.code { P::code_chip() } else { Color32::TRANSPARENT },
            // Links get an underline: in body text they're the same color as ordinary text, so
            // color alone can't tell them apart (in a monochrome scheme "blacker" isn't noticeable).
            // Code chips must never be underlined.
            underline: if sp.link && !sp.code {
                Stroke::new(1.0, P::ink_soft())
            } else {
                Stroke::NONE
            },
            ..Default::default()
        };
        if sp.code {
            // Inline code is rendered as a "chip": the background is painted only per glyph
            // rectangle, so code flush to the edge would smear into one highlight — hence a
            // space of padding on each side. The padding shares the **same font and size** as
            // the code: the background is merged from each glyph's own rectangle, so a
            // different size would split it into three pieces with stepped edges.
            let pad = TextFormat { background: P::code_chip(), ..fmt.clone() };
            job.append(" ", 0.0, pad.clone());
            job.append(&sp.text, 0.0, fmt);
            job.append(" ", 0.0, pad);
        } else {
            job.append(&sp.text, 0.0, fmt);
        }
    }
    job
}

/// A fenced code block: one LayoutJob section per line, colored token by token.
///
/// It must be **split into multiple sections per token** — `TextFormat` is stored per section, and
/// cramming a whole stretch of code into one section allows only a single color. A blank line must
/// also append a `"\n"`, otherwise that line's height collapses.
fn code_job(code: &str, wrap: f32, lang: &str) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap.max(60.0);
    job.wrap.break_anywhere = true;
    let base = TextFormat {
        font_id: theme::mono_font(15.0),
        // Code line-height is a touch tighter than body text: 1.35 is the convention for code
        // (VS Code's default is the same value), and any looser and it should be called a
        // "document" rather than a "code block".
        line_height: Some(15.0 * 1.35),
        ..Default::default()
    };
    let lines = syntax::highlight(code, lang);
    for (i, toks) in lines.iter().enumerate() {
        for (text, tok) in toks {
            let mut fmt = base.clone();
            if *tok != syntax::Tok::Plain {
                fmt.color = syntax::color(*tok);
            }
            if *tok == syntax::Tok::Comment {
                fmt.italics = true;
            }
            job.append(text, 0.0, fmt);
        }
        if i + 1 < lines.len() {
            job.append("\n", 0.0, base.clone());
        }
    }
    job
}

/// Render a whole stretch of Markdown.
///
/// `base` is the body color (a brighter one may be used inside user bubbles); `width` is the maximum
/// available width.
/// The unified text label: pin the wrap mode to `Wrap`.
///
/// This is the UI trap this project trips over most: `Ui::wrap_mode()` returns `Extend` inside a
/// "non-wrapping horizontal layout" (`ui.horizontal*`, whose `main_wrap == false`), and a `Label`
/// with the default `wrap_mode = None` inherits it — so epaint treats
/// `LayoutJob::wrap::max_width` as `f32::INFINITY`, long text runs to the end of a line, punches
/// straight past the column's right edge and gets clipped by the ScrollArea (showing up as "text
/// cut off"). Bubbles, list items and table cells are all this kind of horizontal layout, so we pin
/// it explicitly, one by one, here.
fn wrap_label(ui: &mut Ui, job: LayoutJob) -> egui::Response {
    ui.add(
        Label::new(job)
            .selectable(true)
            .wrap_mode(egui::TextWrapMode::Wrap),
    )
}

pub fn render(ui: &mut Ui, md: &str, base: Color32) {
    // Right-edge boundary: fixed at the available width when entering this column, and not changed for the rest of the render.
    let width = ui.available_width().max(160.0);
    // Make every descendant Ui in this column (including those inside horizontal / Frame) wrap by
    // default, rather than each degrading to Extend. Individual widgets can still override this.
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
    let blocks = parse(md);

    for block in blocks {
        match block {
            Block::Heading { level, text } => {
                let size = match level {
                    1 => 25.0,
                    2 => 21.0,
                    3 => 18.0,
                    _ => 16.5,
                };
                ui.add_space(if level <= 2 { 15.0 } else { 9.0 });
                let job = job_of(&inline(&text), size, P::text(), width, level <= 2);
                wrap_label(ui, job);
                // Level-1 and level-2 headings each carry a thin line: in a monochrome scheme,
                // "bigger and bolder" isn't enough — a line separates the heading from the body, and
                // the hierarchy pops out immediately when scanning.
                if level <= 2 {
                    ui.add_space(6.0);
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
                    let col = if level == 1 { P::border_strong() } else { P::border() };
                    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, col));
                }
                ui.add_space(7.0);
            }

            Block::Para(text) => {
                let job = job_of(&inline(&text), theme::body_size(), base, width, false);
                wrap_label(ui, job);
                ui.add_space(BLOCK_GAP);
            }

            Block::Bullet { indent, marker, text } => {
                let pad = (indent.min(3) as f32) * 20.0;
                let inner_w = (width - pad - 30.0).max(80.0);
                ui.horizontal_top(|ui| {
                    ui.add_space(pad);
                    ui.allocate_ui(egui::vec2(20.0, 0.0), |ui| {
                        ui.set_min_width(20.0);
                        let m = marker.clone().unwrap_or_else(|| "•".to_string());
                        ui.label(RichText::new(m).color(P::ink()).size(17.5));
                    });
                    let job = job_of(&inline(&text), theme::body_size(), base, inner_w, false);
                    wrap_label(ui, job);
                });
                ui.add_space(6.0);
            }

            Block::Quote(text) => {
                let inner_w = (width - 34.0).max(80.0);
                ui.horizontal_top(|ui| {
                    ui.add_space(2.0);
                    ui.label(RichText::new("▎").color(P::quote_bar()).size(20.0));
                    ui.add_space(4.0);
                    let spans = inline(&text);
                    let job = job_of(&spans, theme::body_size(), P::text_dim(), inner_w, false);
                    wrap_label(ui, job);
                });
                ui.add_space(BLOCK_GAP);
            }

            // ```` ```svg ````: **draw it**, don't show it as code.
            //
            // Going through fenced code blocks rather than adding a new tool/protocol: markdown
            // already has fences, and the model is already good at writing SVG — so "the model draws
            // a flowchart" works with zero protocol change, and it naturally lands in `.jsonl`
            // (part of the body text), so it's still there when the conversation is reopened.
            //
            // When drawing fails (unparseable), `svg::render` writes a reason line itself and leaves
            // that stretch **in the body text as-is** (the fallback below), so the user can at least
            // still see what the model wrote.
            // ```` ```svg ````: **draw it**, don't show it as code.
            //
            // Going through fenced code blocks rather than adding a new tool/protocol: markdown
            // already has fences, and the model is already good at writing SVG — so "the model draws
            // a flowchart" works with zero protocol change, and it naturally lands in `.jsonl`
            // (part of the body text), so it's still there when the conversation is reopened.
            //
            // When it can't be drawn, fall back to a **code block**: then the user can at least still
            // see what the model wrote, rather than staring at one error line (`svg::render` adds a
            // reason itself).
            Block::Code { lang, lines } if lang.trim().eq_ignore_ascii_case("svg") => {
                if crate::svg::render(ui, &lines.join("\n"), width - 20.0) <= 0.0 {
                    code_block(ui, width, "svg", &lines);
                }
                ui.add_space(BLOCK_GAP);
            }

            Block::Code { lang, lines } => {
                code_block(ui, width, &lang, &lines);
                ui.add_space(BLOCK_GAP);
            }

            Block::Rule => {
                ui.add_space(8.0);
                let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
                ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, P::border()));
                ui.add_space(11.0);
            }

            Block::Table { header, rows } => {
                table(ui, width, &header, &rows, base);
            }
        }
    }
}

// ────────────────────────────── Table ──────────────────────────────

/// Cell horizontal/vertical padding, column gap, and outer corner radius.
const CELL_PAD_X: f32 = 11.0;
const CELL_PAD_Y: f32 = 8.0;
const COL_GAP: f32 = 16.0;
const TABLE_R: u8 = 10;

/// Display width: ASCII counts as 1 cell, CJK and full-width punctuation count as 2.
fn disp_width(s: &str) -> f32 {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| if c.is_ascii() { 1.0 } else { 2.0 })
        .sum()
}

/// Column width allocation.
///
/// Evenly dividing the widths is the least effort and the ugliest: short columns leave a big gap,
/// long ones get squeezed into a vertical strip. Here we first compute an ideal width from "that
/// column's longest cell", then scale proportionally to the available width; finally a lower bound
/// catches it — if it really doesn't fit, shrink everything together, preferring wrapped text over
/// overflowing.
fn column_widths(header: &[String], rows: &[Vec<String>], avail: f32) -> Vec<f32> {
    const MIN: f32 = 68.0;
    const CH: f32 = 7.4; // approximate ASCII advance at 15px body size
    const PAD: f32 = CELL_PAD_X * 2.0 + 8.0;
    let cols = header.len().max(1);
    let ideal: Vec<f32> = (0..cols)
        .map(|c| {
            let mut w = disp_width(&header[c]) * CH + PAD;
            for r in rows {
                w = w.max(disp_width(r.get(c).map(|s| s.as_str()).unwrap_or("")) * CH + PAD);
            }
            // A single column takes at most "two columns' worth", so one column doesn't grab the
            // whole row and squeeze the rest into a sliver
            w.min(avail / cols as f32 * 2.0)
        })
        .collect();
    let sum: f32 = ideal.iter().sum();
    let mut out: Vec<f32> = if sum > 0.0 {
        ideal.iter().map(|w| avail * w / sum).collect()
    } else {
        vec![avail / cols as f32; cols]
    };
    for w in out.iter_mut() {
        *w = w.max(MIN);
    }
    let over: f32 = out.iter().sum();
    if over > avail {
        let k = avail / over;
        for w in out.iter_mut() {
            *w *= k;
        }
    }
    out
}

/// Table: full outer frame + header band + row separators, with column widths by content.
///
/// It used to be just a `Grid` with no frame, no header, no row lines — it read like a few
/// paragraphs mashed together, with all the table's information resting on "alignment", so a
/// single wrapped cell would fall apart completely.
fn table(ui: &mut Ui, width: f32, header: &[String], rows: &[Vec<String>], base: Color32) {
    // 2 = width consumed by the outer stroke (egui counts stroke width inside the Frame's margin)
    let inner = (width - 2.0).max(160.0);
    let avail = (inner - COL_GAP * header.len().saturating_sub(1) as f32).max(80.0);
    let widths = column_widths(header, rows, avail);

    // Positions of the row separators: bottom of each row (the last row isn't drawn; the outer
    // frame closes it off)
    let mut edges: Vec<f32> = Vec::new();
    let total = rows.len() + 1;

    theme::card(TABLE_R)
        .show(ui, |ui| {
            ui.set_width(inner);
            ui.set_max_width(inner);
            // The column gap is controlled with add_space ourselves. `horizontal` would additionally
            // insert the style's item_spacing.x, and without zeroing it the whole table would exceed
            // its budget by (columns - 1) gaps and push out of the column.
            ui.style_mut().spacing.item_spacing.x = 0.0;

            for (ri, row) in std::iter::once(header)
                .chain(rows.iter().map(|r| r.as_slice()))
                .enumerate()
            {
                let head = ri == 0;
                if head {
                    // Header band: one strip for the whole row, rounded only on top, so it hugs the outer frame
                    Frame::NONE
                        .fill(P::surface_hi())
                        .corner_radius(CornerRadius { nw: TABLE_R, ne: TABLE_R, sw: 0, se: 0 })
                        .show(ui, |ui| {
                            ui.set_width(inner);
                            ui.set_max_width(inner);
                            ui.horizontal_top(|ui| cells(ui, &widths, row, true, P::text()));
                        });
                } else {
                    ui.horizontal_top(|ui| cells(ui, &widths, row, false, base));
                }
                if ri + 1 < total {
                    edges.push(ui.cursor().min.y);
                }
            }

            // Lines sit only in the gaps between rows, never overlapping any text
            let content = ui.min_rect();
            for (i, y) in edges.iter().enumerate() {
                let col = if i == 0 { P::border_strong() } else { P::border() };
                ui.painter().hline(content.x_range(), *y, Stroke::new(1.0, col));
            }
        });
    ui.add_space(BLOCK_GAP);
}

/// One row of cells. `widths` holds each column's **box** width (including left/right padding).
fn cells(ui: &mut Ui, widths: &[f32], row: &[String], head: bool, color: Color32) {
    for (ci, cw) in widths.iter().enumerate() {
        if ci > 0 {
            ui.add_space(COL_GAP);
        }
        let txt = row.get(ci).map(|s| s.as_str()).unwrap_or("");
        // A very narrow column (a multi-column table hitting a narrow column slot) yields its
        // padding too: once "content + padding" exceeds the allotted column width, the whole table
        // pushes out of the column and gets clipped by the ScrollArea.
        let pad = CELL_PAD_X.min(cw * 0.22);
        let w = (cw - pad * 2.0).max(12.0);
        Frame::NONE
            .inner_margin(Margin::symmetric(pad as i8, CELL_PAD_Y as i8))
            .show(ui, |ui| {
                ui.set_width(w);
                ui.set_max_width(w);
                let job = job_of(&inline(txt), 15.0, color, w, head);
                wrap_label(ui, job);
            });
    }
}


/// How long the copy button shows its tick after a click, in seconds.
const COPIED_TTL: f64 = 1.4;

/// Which code block was copied last, and when — one temp-data slot in egui's memory.
///
/// Keyed by a **hash of the block's text** rather than by a per-block index: `render` runs fresh
/// every frame and has no stable block id, while only one block can have just been copied anyway.
/// Two byte-identical blocks would both show the tick, which is honest — copying either one puts
/// exactly the same text on the clipboard.
fn copied_mark(ui: &Ui) -> Option<(u64, f64)> {
    ui.ctx().data(|d| d.get_temp::<(u64, f64)>(egui::Id::new("aox-code-copied")))
}

/// The temp-data slot used by [`copied_mark`].
fn copy_slot() -> egui::Id {
    egui::Id::new("aox-code-copied")
}

fn text_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// The square button in a code block's top-right corner: a "copy" glyph that turns into a green
/// tick once the code is on the clipboard.
///
/// Both glyphs are **drawn**, not typed: a text glyph would come from whatever font happens to be
/// installed and would not sit on the same optical weight as the rest of the icon set.
struct CopyButton {
    copied: bool,
}

impl egui::Widget for CopyButton {
    fn ui(self, ui: &mut Ui) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
        let hot = resp.hovered() || self.copied;
        // The fill has to be opaque: it doubles as the "occluder" that makes the front sheet of the
        // copy icon sit *on top of* the back one. Not hovered = the block's own background, so the
        // button is invisible until you reach for it.
        let bg = if hot { P::surface_hi() } else { P::code_bg() };
        if hot {
            ui.painter().rect_filled(rect, CornerRadius::same(7), bg);
        }
        let color = if self.copied {
            P::add_green()
        } else if resp.hovered() {
            P::text()
        } else {
            P::text_faint()
        };
        let c = rect.center();
        let stroke = Stroke::new(1.6, color);
        let p = ui.painter();
        if self.copied {
            p.line_segment([egui::pos2(c.x - 5.5, c.y + 0.5), egui::pos2(c.x - 1.5, c.y + 4.5)], stroke);
            p.line_segment([egui::pos2(c.x - 1.5, c.y + 4.5), egui::pos2(c.x + 6.0, c.y - 4.5)], stroke);
        } else {
            let back = egui::Rect::from_min_size(egui::pos2(c.x - 6.5, c.y - 6.5), egui::vec2(9.5, 9.5));
            let front = egui::Rect::from_min_size(egui::pos2(c.x - 3.0, c.y - 3.0), egui::vec2(9.5, 9.5));
            p.rect_stroke(back, CornerRadius::same(2), stroke, egui::StrokeKind::Inside);
            p.rect_filled(front, CornerRadius::same(2), bg);
            p.rect_stroke(front, CornerRadius::same(2), stroke, egui::StrokeKind::Inside);
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }
}

/// One fenced code block: monospace, with a header row (language + copy button) and a backing color.
///
/// Extracted because it has two call sites: an ordinary code block, and **the fallback when SVG
/// drawing fails** (see the `Block::Code { lang, .. } if lang == "svg"` branch). Copying it into two
/// places would mean remembering to change both when tweaking the spacing — and the one you miss is
/// only visible when "SVG happens to fail to draw".
fn code_block(ui: &mut Ui, width: f32, lang: &str, lines: &[String]) {
    let code = lines.join("\n");
    let hash = text_hash(&code);
    let now = ui.input(|i| i.time);
    let copied = copied_mark(ui).map_or(false, |(h, t)| h == hash && now - t < COPIED_TTL);
    // While the tick is up, keep frames coming so it retires on its own — an idle window would
    // otherwise freeze with the tick still showing.
    if copied {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
    }

    Frame::NONE
        .fill(P::code_bg())
        .corner_radius(CornerRadius::same(12))
        .stroke(Stroke::new(1.0, P::edge()))
        .inner_margin(Margin::symmetric(16, 14))
        .show(ui, |ui| {
            // The card's width must be locked explicitly, otherwise a long code line widens the card
            // and it gets clipped by the panel
            let inner = (width - 34.0).max(100.0);
            ui.set_width(inner);
            ui.set_max_width(inner);
            // Header row. It is drawn even with no language: giving the button its own row keeps it
            // off the first line of code, which would otherwise sit right under it (code wraps to
            // fill the card, so there is no "empty corner" to rely on).
            ui.horizontal(|ui| {
                if !lang.is_empty() {
                    ui.label(RichText::new(lang.to_lowercase()).color(P::text_faint()).size(13.0));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(CopyButton { copied }).clicked() {
                        ui.ctx().copy_text(code.clone());
                        ui.ctx().data_mut(|d| d.insert_temp(copy_slot(), (hash, now)));
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
                    }
                });
            });
            ui.add_space(7.0);
            wrap_label(ui, code_job(&code, inner, lang));
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2, Rect};

    /// Replays the column skeleton of `App::transcript`, measuring:
    /// - the available width the Ui reports when entering Markdown
    /// - the width the content actually occupies after rendering (beyond the column it gets clipped by the ScrollArea)
    fn probe(window_w: f32, md: &str) -> (f32, f32, f32) {
        let ctx = egui::Context::default();
        let mut avail = 0.0f32;
        let mut used = 0.0f32;
        let mut inner_w = 0.0f32;
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(window_w, 760.0))),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let a = ui.available_width();
                let (inner, pad) = crate::app::column(a);
                inner_w = inner;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(26.0);
                        ui.horizontal(|ui| {
                            ui.add_space(pad);
                            ui.vertical(|ui| {
                                ui.set_max_width(inner);
                                ui.set_min_width(inner);
                                avail = ui.available_width();
                                render(ui, md, egui::Color32::BLACK);
                                used = ui.min_rect().width();
                            });
                        });
                    });
            });
        });
        (avail, inner_w, used)
    }

    const MD: &str = "\
### connection probe

- `httpfetch_raw` -> requested bilibili.com and got HTTP 200, pulled the whole page (183539 chars, truncated) and pushed it through the plugin pipe correctly.

- plain paragraph with a fairly long run of words so that any sane layout has to wrap it somewhere before it runs off the right edge of the column.

Conclusion: nothing to build, nothing to repair; the current capability covers both the video-metadata path and the generic web-request path.

| plugin | language | tools |
| --- | --- | --- |
| bilibili | python | bilibili_video_info, bilibili_danmaku, bilibili_comments, bilibili_subtitle, bilibili_related |
| httpfetch | python | httpfetch_raw |

> quoted line with enough words that it has to wrap instead of pushing the whole row past the right edge of the column.

```text
a code line long enough that it must wrap rather than widen the card beyond the column
```
";

    #[test]
    fn text_never_exceeds_the_column() {
        for w in [1000.0f32, 1280.0, 1693.0, 2400.0] {
            let (avail, inner, used) = probe(w, MD);
            println!("window={w} avail={avail} inner={inner} used={used}");
            assert!(
                used <= inner + 0.5,
                "window={w}: content used {used} > column {inner} (avail={avail})"
            );
        }
    }

    #[test]
    fn long_unbreakable_token_is_clamped() {
        let md = format!("- token {}", "x".repeat(400));
        let (_avail, inner, used) = probe(1280.0, &md);
        assert!(used <= inner + 0.5, "used {used} > column {inner}");
    }
}
