//! 轻量 Markdown 渲染：把模型返回的 Markdown 画成 egui 控件。
//!
//! 支持：标题、段落、无序/有序列表、引用、围栏代码块、行内代码、**粗体**、*斜体*、
//! 链接与表格。目标是「好看且不丢内容」，不追求 spec 完整。

use egui::{
    text::LayoutJob, Color32, CornerRadius, FontId, Frame, Label, Margin, RichText, Stroke,
    TextFormat, Ui,
};

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
    // 有序列表： 12. xxx
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

        // 围栏代码块
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

        // 表格
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

        // 普通段落：连续的非空、非块起始行合并
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

/// 行内解析：粗体 / 斜体 / 行内代码 / 链接。
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
            P::ACCENT
        } else if sp.link {
            P::ACCENT
        } else if strong && !theme::bold_available() {
            Color32::from_rgb(0xFF, 0xFF, 0xFF)
        } else {
            color
        };
        let fmt = TextFormat {
            font_id: font,
            color: col,
            italics: sp.italic,
            background: if sp.code { P::CODE_BG } else { Color32::TRANSPARENT },
            ..Default::default()
        };
        job.append(&sp.text, 0.0, fmt);
    }
    job
}

fn code_job(code: &str, wrap: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap.max(60.0);
    job.wrap.break_anywhere = true;
    job.append(
        code,
        0.0,
        TextFormat {
            font_id: theme::mono_font(12.0),
            color: P::TEXT,
            ..Default::default()
        },
    );
    job
}

/// 渲染整段 Markdown。
///
/// `base` 是正文颜色（用户气泡里可以用更亮的色）；`width` 是可用的最大宽度。
pub fn render(ui: &mut Ui, md: &str, base: Color32) {
    let width = ui.available_width().max(120.0);
    let blocks = parse(md);

    for block in blocks {
        match block {
            Block::Heading { level, text } => {
                let size = match level {
                    1 => 18.5,
                    2 => 16.0,
                    3 => 14.5,
                    _ => 13.5,
                };
                ui.add_space(if level <= 2 { 6.0 } else { 3.0 });
                let job = job_of(&inline(&text), size, P::TEXT, width, level <= 2);
                ui.add(Label::new(job).selectable(true));
                if level == 1 {
                    ui.add_space(3.0);
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
                    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, P::BORDER));
                }
                ui.add_space(3.0);
            }

            Block::Para(text) => {
                let job = job_of(&inline(&text), 14.0, base, width, false);
                ui.add(Label::new(job).selectable(true));
                ui.add_space(7.0);
            }

            Block::Bullet { indent, marker, text } => {
                let pad = (indent.min(3) as f32) * 14.0;
                let inner_w = (width - pad - 22.0).max(60.0);
                ui.horizontal_top(|ui| {
                    ui.add_space(pad);
                    ui.allocate_ui(egui::vec2(16.0, 0.0), |ui| {
                        ui.set_min_width(16.0);
                        let m = marker.clone().unwrap_or_else(|| "•".to_string());
                        ui.label(RichText::new(m).color(P::ACCENT).size(13.0));
                    });
                    let job = job_of(&inline(&text), 14.0, base, inner_w, false);
                    ui.add(Label::new(job).selectable(true));
                });
                ui.add_space(3.0);
            }

            Block::Quote(text) => {
                let inner_w = (width - 26.0).max(60.0);
                ui.horizontal_top(|ui| {
                    ui.add_space(2.0);
                    ui.label(RichText::new("▎").color(P::QUOTE_BAR).size(15.0));
                    ui.add_space(2.0);
                    let spans = inline(&text);
                    let job = job_of(&spans, 13.5, P::TEXT_DIM, inner_w, false);
                    ui.add(Label::new(job).selectable(true));
                });
                ui.add_space(7.0);
            }

            Block::Code { lang, lines } => {
                let code = lines.join("\n");
                Frame::NONE
                    .fill(P::CODE_BG)
                    .corner_radius(CornerRadius::same(8))
                    .stroke(Stroke::new(1.0, P::BORDER))
                    .inner_margin(Margin::symmetric(11, 9))
                    .show(ui, |ui| {
                        ui.set_width((width - 4.0).max(80.0));
                        if !lang.is_empty() {
                            ui.label(RichText::new(lang.to_lowercase()).color(P::TEXT_FAINT).size(10.5));
                            ui.add_space(3.0);
                        }
                        ui.add(Label::new(code_job(&code, width - 26.0)).selectable(true));
                    });
                ui.add_space(8.0);
            }

            Block::Rule => {
                ui.add_space(4.0);
                let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
                ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, P::BORDER));
                ui.add_space(6.0);
            }

            Block::Table { header, rows } => {
                let cols = header.len().max(1);
                let cell_w = (width / cols as f32 - 18.0).max(50.0);
                let id = ui.next_auto_id();
                egui::Grid::new(id)
                    .num_columns(cols)
                    .spacing(egui::vec2(14.0, 5.0))
                    .striped(false)
                    .min_col_width(cell_w.min(180.0))
                    .show(ui, |ui| {
                        for cell in &header {
                            let job = job_of(&inline(cell), 13.0, P::TEXT, cell_w, true);
                            ui.add(Label::new(job).selectable(true));
                        }
                        ui.end_row();
                        for row in &rows {
                            for cell in row {
                                let job = job_of(&inline(cell), 13.0, P::TEXT_DIM, cell_w, false);
                                ui.add(Label::new(job).selectable(true));
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(8.0);
            }
        }
    }
}
