//! 轻量 Markdown 渲染：把模型返回的 Markdown 画成 egui 控件。
//!
//! 支持：标题、段落、无序/有序列表、引用、围栏代码块、行内代码、**粗体**、*斜体*、
//! 链接与表格。目标是「好看且不丢内容」，不追求 spec 完整。

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

/// 行高系数。
///
/// 字号只管"字有多大"，行与行之间留多少由 `TextFormat::line_height` 决定。
/// 中文正文用 1.3 倍行高偏挤 —— 满行时上下几乎黏住，所以这里给到 1.5。
/// 附带好处：行内代码的芯片底色是按字形矩形画的，行距一松，芯片之间也就
/// 再不会上下粘连。
const LEADING: f32 = 1.5;

/// 块与块之间的呼吸。默认 11 太小，两段话几乎贴在一起。
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
            // 链接加一道下划线：正文里它和普通文字同色，光靠颜色分不出来
            // （单色方案里「更黑」并不显眼）。代码芯片绝不能加下划线。
            underline: if sp.link && !sp.code {
                Stroke::new(1.0, P::ink_soft())
            } else {
                Stroke::NONE
            },
            ..Default::default()
        };
        if sp.code {
            // 行内代码做成「芯片」：底色只按字形矩形绘制，代码贴边时会糊成一条高亮，
            // 所以两侧各补一格空格。空格与代码**同字体同字号** —— 底色是按每段
            // 字形自己的矩形合并的，字号一旦不同就会断成三块、边缘出现台阶。
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

/// 围栏代码块：一行一个 LayoutJob section，逐 token 上色。
///
/// 必须**逐 token 切成多个 section** —— `TextFormat` 是按 section 存的，
/// 一整段代码塞进一个 section 就只能有一个颜色。空行也要 append 一个 `"\n"`，
/// 否则那一行的高度会塌掉。
fn code_job(code: &str, wrap: f32, lang: &str) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap.max(60.0);
    job.wrap.break_anywhere = true;
    let base = TextFormat {
        font_id: theme::mono_font(15.0),
        // 代码的行高比正文再紧一点：1.35 是代码的惯例（VS Code 默认同值），
        // 再松就该改叫"文档"而不是"代码块"了。
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
                ui.add_space(if level <= 2 { 15.0 } else { 9.0 });
                let job = job_of(&inline(&text), size, P::text(), width, level <= 2);
                wrap_label(ui, job);
                // 一、二级标题各带一条细线：单色方案里「更大更粗」还不够，
                // 靠一道线把标题与正文分开，扫读时的层次立刻就出来了。
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

            // ```` ```svg ````：**画出来**，不当代码显示。
            //
            // 走围栏代码块而不是新加一个工具／协议：markdown 本来就有围栏，
            // 模型本来就擅长写 SVG —— 于是「模型画个流程图」这件事零协议改动就成立了，
            // 而且它天然进 `.jsonl`（就是正文的一部分），重开对话还在。
            //
            // 画失败（解析不了）时 `svg::render` 自己会写一行原因，并且把那一段
            // **原样留在正文里**（下面的兜底），用户至少还能看到模型写了什么。
            // ```` ```svg ````：**画出来**，不当代码显示。
            //
            // 走围栏代码块而不是新加一个工具／协议：markdown 本来就有围栏，模型本来
            // 就擅长写 SVG —— 于是「模型画个流程图」这件事零协议改动就成立了，而且它
            // 天然进 `.jsonl`（正文的一部分），重开对话还在。
            //
            // 画不出来时退回**代码块**：那时用户至少还能看到模型写了什么，
            // 而不是对着一行错误发呆（`svg::render` 自己会补一句原因）。
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

// ────────────────────────────── 表格 ──────────────────────────────

/// 单元格左右内边距、上下内边距、列间距、外框圆角。
const CELL_PAD_X: f32 = 11.0;
const CELL_PAD_Y: f32 = 8.0;
const COL_GAP: f32 = 16.0;
const TABLE_R: u8 = 10;

/// 显示宽度：ASCII 记 1 格，中日韩与全角标点记 2 格。
fn disp_width(s: &str) -> f32 {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| if c.is_ascii() { 1.0 } else { 2.0 })
        .sum()
}

/// 列宽分配。
///
/// 均分列宽最省事也最难看：内容短的列空一大片，内容长的列被挤成竖条。这里先按
/// 「该列最长的一格」算理想宽度，再按可用宽度等比缩放；最后用下限兜底 ——
/// 实在放不下就整体一起缩，宁可让文字换行也不越界。
fn column_widths(header: &[String], rows: &[Vec<String>], avail: f32) -> Vec<f32> {
    const MIN: f32 = 68.0;
    const CH: f32 = 7.4; // 15px 正文下 ASCII 的近似步进
    const PAD: f32 = CELL_PAD_X * 2.0 + 8.0;
    let cols = header.len().max(1);
    let ideal: Vec<f32> = (0..cols)
        .map(|c| {
            let mut w = disp_width(&header[c]) * CH + PAD;
            for r in rows {
                w = w.max(disp_width(r.get(c).map(|s| s.as_str()).unwrap_or("")) * CH + PAD);
            }
            // 单列最多吃「两列的份」，免得某一列拿走整行、其余挤成一条缝
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

/// 表格：完整外框 + 表头色带 + 行分隔线，列宽按内容长度分配。
///
/// 之前只是一张 `Grid`，没有框、没有表头、没有分行线，读起来像几段文字凑在
/// 一起 —— 表格的全部信息都压在「对齐」上，一旦某格换行就彻底散了。
fn table(ui: &mut Ui, width: f32, header: &[String], rows: &[Vec<String>], base: Color32) {
    // 2 = 外框描边占掉的宽度（egui 的 stroke 宽度算在 Frame 的 margin 里）
    let inner = (width - 2.0).max(160.0);
    let avail = (inner - COL_GAP * header.len().saturating_sub(1) as f32).max(80.0);
    let widths = column_widths(header, rows, avail);

    // 行分隔线的位置：每行底部（最后一行不画，交给外框收口）
    let mut edges: Vec<f32> = Vec::new();
    let total = rows.len() + 1;

    Frame::NONE
        .fill(P::surface())
        .stroke(Stroke::new(1.0, P::border_strong()))
        .corner_radius(CornerRadius::same(TABLE_R))
        .show(ui, |ui| {
            ui.set_width(inner);
            ui.set_max_width(inner);
            // 列间距自己用 add_space 控制。`horizontal` 还会额外插入 style 的
            // item_spacing.x，不清零整表就会比预算多出 (列数-1) 个间距、撑出栏位。
            ui.style_mut().spacing.item_spacing.x = 0.0;

            for (ri, row) in std::iter::once(header)
                .chain(rows.iter().map(|r| r.as_slice()))
                .enumerate()
            {
                let head = ri == 0;
                if head {
                    // 表头色带：整行一条、只有上圆角，正好贴住外框
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

            // 线只落在行与行之间的空隙里，压不到任何文字
            let content = ui.min_rect();
            for (i, y) in edges.iter().enumerate() {
                let col = if i == 0 { P::border_strong() } else { P::border() };
                ui.painter().hline(content.x_range(), *y, Stroke::new(1.0, col));
            }
        });
    ui.add_space(BLOCK_GAP);
}

/// 一行单元格。`widths` 里存的是每列**盒子**的宽度（含左右内边距）。
fn cells(ui: &mut Ui, widths: &[f32], row: &[String], head: bool, color: Color32) {
    for (ci, cw) in widths.iter().enumerate() {
        if ci > 0 {
            ui.add_space(COL_GAP);
        }
        let txt = row.get(ci).map(|s| s.as_str()).unwrap_or("");
        // 极窄的列（多列表格撞上窄栏位）连内边距一起让位：「内容 + 内边距」一旦
        // 超过分配到的列宽，整张表就会撑出栏位、被 ScrollArea 裁掉。
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


/// 一块围栏代码：等宽、带 lang 标签、底色一块。
///
/// 抽出来是因为它有两个调用点：普通代码块，以及 **SVG 画失败时的兜底**
///（见那条 `Block::Code { lang, .. } if lang == "svg"` 分支）。抄成两份的话，
/// 改一次留白就得记得改两处 —— 而漏掉的那一处只有「SVG 恰好画失败」时才看得见。
fn code_block(ui: &mut Ui, width: f32, lang: &str, lines: &[String]) {
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
                ui.add_space(7.0);
            }
            wrap_label(ui, code_job(&code, inner, lang));
        });
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
