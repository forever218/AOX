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
            italics: sp.italic,
            background: if sp.code { P::code_bg() } else { Color32::TRANSPARENT },
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
            font_id: theme::mono_font(15.0),
            color: P::text(),
            ..Default::default()
        },
    );
    job
}

/// 渲染整段 Markdown。
///
/// `base` 是正文颜色（用户气泡里可以用更亮的色）；`width` 是可用的最大宽度。
/// 统一的文本标签：把换行模式钉死成 `Wrap`。
///
/// 这是本项目最容易踩的 UI 坑：`Ui::wrap_mode()` 在「非换行的水平布局」
/// （`ui.horizontal*`，其 `main_wrap == false`）里返回 `Extend`，
/// 而 `Label` 默认 `wrap_mode = None` 会去继承它 —— 于是 epaint 把
/// `LayoutJob::wrap::max_width` 当成 `f32::INFINITY`，长文本一行到底，
/// 直接冲出栏位右边界、被 ScrollArea 裁掉（表现为「文字被切」）。
/// 气泡、列表项、表格单元格里全是这种水平布局，所以这里逐个显式钉死。
fn wrap_label(ui: &mut Ui, job: LayoutJob) -> egui::Response {
    ui.add(
        Label::new(job)
            .selectable(true)
            .wrap_mode(egui::TextWrapMode::Wrap),
    )
}

pub fn render(ui: &mut Ui, md: &str, base: Color32) {
    // 右端边界：以进入本栏时的可用宽度为准，整段渲染期间不再变化。
    let width = ui.available_width().max(160.0);
    // 让这一栏里的所有后代 Ui（含 horizontal / Frame 内部）都默认换行，
    // 而不是各自退化成 Extend。个别控件仍可自行覆盖。
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
                ui.add_space(if level <= 2 { 12.0 } else { 7.0 });
                let job = job_of(&inline(&text), size, P::text(), width, level <= 2);
                wrap_label(ui, job);
                if level == 1 {
                    ui.add_space(6.0);
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
                    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, P::border()));
                }
                ui.add_space(5.0);
            }

            Block::Para(text) => {
                let job = job_of(&inline(&text), theme::BODY, base, width, false);
                wrap_label(ui, job);
                ui.add_space(11.0);
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
                    let job = job_of(&inline(&text), theme::BODY, base, inner_w, false);
                    wrap_label(ui, job);
                });
                ui.add_space(5.0);
            }

            Block::Quote(text) => {
                let inner_w = (width - 34.0).max(80.0);
                ui.horizontal_top(|ui| {
                    ui.add_space(2.0);
                    ui.label(RichText::new("▎").color(P::quote_bar()).size(20.0));
                    ui.add_space(4.0);
                    let spans = inline(&text);
                    let job = job_of(&spans, theme::BODY, P::text_dim(), inner_w, false);
                    wrap_label(ui, job);
                });
                ui.add_space(11.0);
            }

            Block::Code { lang, lines } => {
                let code = lines.join("\n");
                Frame::NONE
                    .fill(P::code_bg())
                    .corner_radius(CornerRadius::same(12))
                    .stroke(Stroke::new(1.0, P::border()))
                    .inner_margin(Margin::symmetric(16, 14))
                    .show(ui, |ui| {
                        // 卡片的宽度必须显式锁死，否则代码行的长行会把卡片顶宽、被面板裁掉
                        let inner = (width - 34.0).max(100.0);
                        ui.set_width(inner);
                        ui.set_max_width(inner);
                        if !lang.is_empty() {
                            ui.label(RichText::new(lang.to_lowercase()).color(P::text_faint()).size(13.0));
                            ui.add_space(6.0);
                        }
                        wrap_label(ui, code_job(&code, inner));
                    });
                ui.add_space(12.0);
            }

            Block::Rule => {
                ui.add_space(6.0);
                let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
                ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, P::border()));
                ui.add_space(9.0);
            }

            Block::Table { header, rows } => {
                let cols = header.len().max(1);
                let gap = 18.0;
                // 列宽要连列间距一起算进去，否则最后一列会超出可用宽度被裁掉
                let cell_w = ((width - gap * (cols.saturating_sub(1)) as f32) / cols as f32 - 4.0)
                    .max(60.0);
                let id = ui.next_auto_id();
                egui::Grid::new(id)
                    .num_columns(cols)
                    .spacing(egui::vec2(gap, 8.0))
                    .striped(false)
                    .min_col_width(cell_w)
                    .max_col_width(cell_w)
                    .show(ui, |ui| {
                        for cell in &header {
                            let job = job_of(&inline(cell), 15.0, P::text(), cell_w, true);
                            wrap_label(ui, job);
                        }
                        ui.end_row();
                        for row in &rows {
                            for cell in row {
                                let job = job_of(&inline(cell), 15.0, P::text_dim(), cell_w, false);
                                wrap_label(ui, job);
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(12.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2, Rect};

    /// 复刻 `App::transcript` 的版心骨架，量出：
    /// - 进入 markdown 时 Ui 认为的可用宽度
    /// - 渲染完之后内容实际占用的宽度（超出栏宽即被 ScrollArea 裁切）
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
