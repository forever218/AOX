//! Lightweight Markdown rendering: draw the model's Markdown reply as egui widgets.
//!
//! Supports: headings, paragraphs, unordered/ordered lists (including `- [x]` task lists), block
//! quotes, fenced code blocks, inline code, **bold**, *italic*, links and tables. The goal is
//! "good-looking and loses nothing", not spec completeness.
//!
//! **Clickable targets.** A run of text that names a URL or an absolute local path is clickable: it
//! goes through `open` on click (see that module for what a click is allowed to do). Detecting them
//! is split in two, because the two halves have opposite requirements:
//!
//! - [`scan`] is **pure string work, no filesystem** — it runs on every frame, since `render` parses
//!   and lays out the whole reply again each frame.
//! - hit testing is done against the **laid-out galley**, not by splitting the paragraph into
//!   widgets. One `Label` for the whole paragraph is what keeps wrapping, text selection and copying
//!   behaving exactly as before.

use egui::{
    text::LayoutJob, Color32, CornerRadius, FontId, Frame, Label, Margin, RichText, Stroke,
    TextFormat, Ui,
};

use crate::open::{self, Target};
use crate::syntax;
use crate::theme::{self, P};

#[derive(Debug, Clone, Default)]
struct Span {
    text: String,
    bold: bool,
    italic: bool,
    code: bool,
    /// Where a click on this run goes; `None` = ordinary text.
    href: Option<Target>,
}

/// One clickable stretch of a laid-out paragraph, as **character** offsets into `LayoutJob::text`.
///
/// Characters, not bytes: `Galley::cursor_from_pos` answers in character offsets
/// (`epaint::CCursor::index` — explicitly "NOT byte offset"), so mixing the two units would put
/// every hit box in the wrong place the moment a path sits next to Chinese text.
#[derive(Debug, Clone)]
struct Hot {
    start: usize,
    end: usize,
    target: Target,
}

#[derive(Debug, Clone)]
enum Block {
    Heading { level: u8, text: String },
    Para(String),
    Bullet { indent: usize, marker: Option<String>, text: String, task: Option<bool> },
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

/// A task-list item: `- [ ] todo` / `- [x] done`. Returns the state and the text **without** the
/// brackets.
///
/// GitHub's flavour, because that is what models write when a user asks for a checklist. The brackets
/// must be followed by whitespace — `- [x]done` is a link-ish sentence, not a checkbox, and turning it
/// into one would eat text the user never asked to hide.
fn task_prefix(text: &str) -> Option<(bool, String)> {
    let rest = text.trim_start().strip_prefix('[')?;
    let mut chars = rest.chars();
    let done = match chars.next()? {
        ' ' => false,
        'x' | 'X' => true,
        _ => return None,
    };
    let body = chars.as_str().strip_prefix(']')?;
    let body = body.strip_prefix(' ').or_else(|| body.strip_prefix('\t'))?;
    Some((done, body.trim_start().to_string()))
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
            // A checkbox is only a checkbox while something follows it; otherwise the brackets stay
            // in the sentence and the reader sees exactly what the model wrote.
            let (task, text) = match task_prefix(&text) {
                Some((done, rest)) => (Some(done), rest),
                None => (None, text),
            };
            out.push(Block::Bullet { indent, marker, text, task });
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
                            // The href is the whole point of a link — until now it was parsed and
                            // then dropped, so a standard `[text](url)` could be written but never
                            // clicked. A relative href (`[x](docs/a.md)`) classifies to `None` and
                            // simply stays plain text: the underline is an affordance, and an
                            // affordance that does nothing is worse than none.
                            let href: String = chars[close + 2..paren].iter().collect();
                            out.push(Span {
                                text,
                                href: open::classify(&href),
                                ..Default::default()
                            });
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

// ──────────────────────── Clickable targets: detection ────────────────────────

/// A boundary that ends a path or URL run.
///
/// Whitespace, quotes and CJK. The CJK half matters more than it looks: a model writing Chinese puts
/// a path flush against the sentence — no space before it, a full-width comma right after — so
/// splitting on whitespace alone would swallow that comma into the path (and `exists()` would then
/// fail, so the click would silently do nothing). Stopping at *any* CJK character is exactly right in
/// the other direction too: a path can never contain one.
fn is_boundary(c: char) -> bool {
    c.is_whitespace()
        || matches!(c, '"' | '\'' | '`' | '<' | '>' | '|' | '“' | '”' | '‘' | '’')
        || is_cjk(c)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F      // CJK punctuation: ，。、；：！？ and 「」
        | 0x3400..=0x4DBF    // extension A
        | 0x4E00..=0x9FFF    // the common block
        | 0xF900..=0xFAFF    // compatibility ideographs
        | 0xFF00..=0xFFEF    // full-width forms: ，！？：（） etc.
    )
}

/// The character that closes a run opened by `c`, if `c` opens one.
fn closing_quote(c: char) -> Option<char> {
    Some(match c {
        '"' => '"',
        '“' => '”',
        '‘' => '’',
        '\'' => '\'',
        '「' => '」',
        '『' => '』',
        _ => return None,
    })
}

/// Every URL / absolute path inside one run of prose, as character ranges.
///
/// **Pure string work — no filesystem, no allocation of a per-frame cache.** `render` re-parses the
/// whole reply every frame, so a `stat` in here would turn scrolling into disk I/O.
fn scan(s: &str) -> Vec<(usize, usize, Target)> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<(usize, usize, Target)> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        // A quoted run whose **whole** body is a target: `"E:\my folder\a.txt"`. This is the only way
        // a path containing spaces survives — a space is a boundary everywhere else.
        if let Some(close_ch) = closing_quote(chars[i]) {
            if let Some(j) = (i + 1..chars.len()).find(|&k| chars[k] == close_ch) {
                let body: String = chars[i + 1..j].iter().collect();
                if let Some(t) = open::classify(&body) {
                    out.push((i + 1, j, t));
                    i = j + 1;
                    continue;
                }
            }
            // Not a target: fall through one character at a time, so a URL inside a quoted *sentence*
            // is still found — the quotes only decide where a target-shaped run may start and end.
        }

        if let Some(end) = url_end(&chars, i).or_else(|| path_end(&chars, i)) {
            let end = trim_tail(&chars, i, end);
            if end > i {
                let text: String = chars[i..end].iter().collect();
                if let Some(t) = open::classify(&text) {
                    out.push((i, end, t));
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// End (exclusive) of a bare `http(s)://` URL starting at `i`.
fn url_end(chars: &[char], i: usize) -> Option<usize> {
    // Glued to a Latin word (`xhttps://`) it is not a URL.
    //
    // `is_ascii_alphanumeric` and **not** `is_alphanumeric`: the latter is true for every Unicode
    // letter including CJK, so a path glued to Chinese text — the most common shape in a Chinese
    // reply — would be read as "a word ends here, so this is not a path" and nothing would be
    // clickable. The test `the_sentence_full_stop_is_not_part_of_the_path` is what caught this.
    // A CJK character before a path is a boundary, not a word continuation.
    if i > 0 && chars[i - 1].is_ascii_alphanumeric() {
        return None;
    }
    let mut matched = false;
    for scheme in ["https://", "http://"] {
        let n = scheme.chars().count();
        if i + n < chars.len()
            && chars[i..i + n].iter().zip(scheme.chars()).all(|(a, b)| a.eq_ignore_ascii_case(&b))
        {
            matched = true;
            break;
        }
    }
    if !matched {
        return None;
    }
    let mut end = i;
    while end < chars.len() && !is_boundary(chars[end]) {
        end += 1;
    }
    Some(end)
}

/// End (exclusive) of an absolute Windows path starting at `i` — `X:\…`, `X:/…`, `\\server\share`.
fn path_end(chars: &[char], i: usize) -> Option<usize> {
    // ASCII only, for the reason spelled out in [`url_end`].
    if i > 0 && chars[i - 1].is_ascii_alphanumeric() {
        return None;
    }
    let n = chars.len();
    let drive = chars[i].is_ascii_alphabetic()
        && i + 2 < n
        && chars[i + 1] == ':'
        && matches!(chars[i + 2], '\\' | '/');
    let unc = chars[i] == '\\' && i + 2 < n && chars[i + 1] == '\\' && chars[i + 2] != '\\';
    if !drive && !unc {
        return None;
    }
    let mut end = i;
    while end < n {
        let c = chars[end];
        // `* ? |` are illegal in a Windows file name and are also what markdown emphasis looks like
        // when it is glued straight onto a path.
        if is_boundary(c) || matches!(c, '*' | '?' | '|') {
            break;
        }
        // A colon past the drive prefix is `file:line`, not part of the path (and Windows forbids
        // ':' in a file name anyway).
        if c == ':' && end >= i + 2 {
            break;
        }
        end += 1;
    }
    if end <= i + 2 {
        return None;
    }
    Some(end)
}

/// Drop the punctuation a sentence leaves glued to the end of a target: `see E:\a.txt.` — that
/// period belongs to the sentence, not to the file name.
///
/// Closing brackets are dropped only when they are **unbalanced**, so a URL ending in
/// `(disambiguation)` keeps its bracket.
fn trim_tail(chars: &[char], start: usize, mut end: usize) -> usize {
    while end > start {
        let c = chars[end - 1];
        let drop = match c {
            '.' | ',' | ';' | '!' | '?' | ':' => true,
            ')' => !balanced(chars, start, end, '(', ')'),
            ']' => !balanced(chars, start, end, '[', ']'),
            '}' => !balanced(chars, start, end, '{', '}'),
            _ => false,
        };
        if !drop {
            break;
        }
        end -= 1;
    }
    end
}

fn balanced(chars: &[char], start: usize, end: usize, open: char, close: char) -> bool {
    let opens = chars[start..end].iter().filter(|c| **c == open).count();
    let closes = chars[start..end].iter().filter(|c| **c == close).count();
    opens >= closes
}

/// Turn the targets inside a set of spans into clickable spans.
///
/// Runs on the **already-parsed** spans, so a path inside `**bold**` is found as well (`inline`
/// recurses for emphasis, and each recursion starts from plain runs).
fn autolink(spans: Vec<Span>) -> Vec<Span> {
    let mut out = Vec::with_capacity(spans.len());
    for sp in spans {
        let (bold, italic) = (sp.bold, sp.italic);
        let plain = |text: String, href: Option<Target>| Span { text, bold, italic, code: false, href };

        // Inline code is a literal value, not prose: the whole run is the candidate, with no scanning
        // and no trimming. This is the most common shape of all — "written to
        // `E:\rustapp\dist\x.exe`", in backticks.
        //
        // A run that is not absolute is still tried **against the workspace root**, which is what
        // makes `` `src/tool/shell.rs` `` clickable. That resolution is gated on the file existing
        // (see `open::resolve_relative`), because unlike a drive letter a relative path is
        // indistinguishable from ordinary text — `and/or` and `1.5/2` are both shaped like one.
        if sp.code {
            let mut sp = sp;
            if sp.href.is_none() {
                sp.href = open::classify(&sp.text).or_else(|| {
                    open::resolve_relative(&sp.text).map(Target::Path)
                });
            }
            out.push(sp);
            continue;
        }
        if sp.href.is_some() {
            out.push(sp);
            continue;
        }
        let ranges = scan(&sp.text);
        if ranges.is_empty() {
            out.push(sp);
            continue;
        }
        let chars: Vec<char> = sp.text.chars().collect();
        let mut at = 0usize;
        for (start, end, target) in ranges {
            if start > at {
                out.push(plain(chars[at..start].iter().collect(), None));
            }
            out.push(plain(chars[start..end].iter().collect(), Some(target)));
            at = end;
        }
        if at < chars.len() {
            out.push(plain(chars[at..].iter().collect(), None));
        }
    }
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

/// Lay out one paragraph's spans, and report where the clickable runs ended up.
///
/// The two outputs are produced in the same pass on purpose: the offsets must describe **this**
/// job's text, and the padding spaces an inline code chip prepends are part of that text. Recovering
/// the offsets afterwards would mean re-deriving them from the spans and re-counting the padding —
/// two chances to drift by a character per chip, i.e. a hit box that jumps sideways once a path
/// appears after inline code.
fn job_of(
    spans: Vec<Span>,
    size: f32,
    color: Color32,
    wrap: f32,
    force_bold: bool,
) -> (LayoutJob, Vec<Hot>) {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap.max(40.0);
    job.wrap.break_anywhere = false;
    let mut hots: Vec<Hot> = Vec::new();
    // Character offset into `job.text`, tracked as the job is built.
    let mut at = 0usize;
    for sp in autolink(spans) {
        let start = at;
        let len = sp.text.chars().count();
        let strong = sp.bold || force_bold;
        let font: FontId = if strong {
            theme::bold_font(size)
        } else if sp.code {
            theme::mono_font(size - 1.0)
        } else {
            theme::body_font(size)
        };
        let col = if sp.code || sp.href.is_some() {
            P::ink()
        } else if strong && !theme::bold_available() {
            Color32::from_rgb(0xFF, 0xFF, 0xFF)
        } else {
            color
        };
        // An inline-code chip needs its **own** line height, and a top valign.
        //
        // The background is painted from the glyph's box, whose height *is* the format's line height —
        // so requesting the body's 1.5× leading gave the chip 21 pt while its ink is only 13, with the
        // ink sitting 2.06 pt below the box top: 2 pt of air above, 6 below ("the background hangs
        // under the text"). A chip of `2 × (2.06 + 13/2) ≈ 17.1 pt` centres the ink instead, and
        // `Align::TOP` keeps the box top where the body text's is — so the baseline does not move and
        // a code span still sits level with the CJK around it. `1.32 ×` the mono size tracks that
        // ratio at every size; `markdown::tests` measures the result rather than trusting the algebra.
        let line_height = if sp.code {
            Some((size - 1.0) * 1.32)
        } else {
            Some(size * LEADING)
        };
        let fmt = TextFormat {
            font_id: font,
            color: col,
            line_height,
            valign: if sp.code { egui::Align::TOP } else { egui::Align::BOTTOM },
            italics: sp.italic,
            background: if sp.code { P::code_chip() } else { Color32::TRANSPARENT },
            // Links get an underline: in body text they're the same color as ordinary text, so
            // color alone can't tell them apart (in a monochrome scheme "blacker" isn't noticeable).
            // Code chips must never be underlined — they already look like a value, and a line
            // through the chip smears the "this is code" signal. A path in backticks is therefore
            // clickable without a visible cue: the pointer turning into a hand and the tooltip are
            // the whole affordance there.
            underline: if sp.href.is_some() && !sp.code {
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
            // The hit box covers the code itself, not the two padding spaces — hovering the one
            // pixel of air before the chip must not count as hovering the chip.
            if let Some(t) = sp.href.clone() {
                hots.push(Hot { start: start + 1, end: start + 1 + len, target: t });
            }
            job.append(&sp.text, 0.0, fmt);
            job.append(" ", 0.0, pad);
            at = start + len + 2;
        } else {
            if let Some(t) = sp.href.clone() {
                hots.push(Hot { start, end: start + len, target: t });
            }
            job.append(&sp.text, 0.0, fmt);
            at = start + len;
        }
    }
    (job, hots)
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

/// Draw a paragraph and make its `hots` clickable.
///
/// Still **one `Label`** for the whole paragraph: splitting the text into per-link widgets would put
/// every link on its own line and throw away text selection, which is the thing people actually use
/// most on a reply full of paths.
///
/// So the hit test runs *after* layout, against the galley: the pointer position is mapped to a
/// character offset (`Galley::cursor_from_pos`) and looked up in `hots`. This is why `hots` counts
/// characters, not bytes — see [`Hot`].
fn text_label(ui: &mut Ui, job: LayoutJob, hots: &[Hot]) -> egui::Response {
    if hots.is_empty() {
        // The overwhelmingly common case, and the cheap one: no galley of our own to keep.
        return wrap_label(ui, job);
    }

    // Lay out here rather than inside `Label` so the galley survives the call — the hit test needs
    // it, and a `Response` does not carry one.
    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
    // Where `Label` is about to put this galley (`allocate_exact_size` starts at the cursor). Knowing
    // it *before* the widget exists is what lets the hover wash be painted under the text instead of
    // over it — painting after `ui.add` would draw on top of the glyphs.
    let origin = ui.cursor().min;
    let bounds = egui::Rect::from_min_size(origin, galley.size()).expand(2.0);
    let hovered = ui
        .input(|i| i.pointer.hover_pos())
        .filter(|p| bounds.contains(*p))
        .and_then(|p| hot_at(&galley, hots, p - origin));

    if let Some(i) = hovered {
        // One pill per row the run covers: a path long enough to wrap gets its wash continued on the
        // next line, which is the only visual cue that the two halves are one target.
        for r in run_rects(&galley, hots[i].start, hots[i].end) {
            ui.painter().rect_filled(
                r.translate(origin.to_vec2()).expand2(egui::vec2(2.0, 0.0)),
                CornerRadius::same(theme::R0),
                P::hover_fill(),
            );
        }
    }

    let resp = ui.add(
        Label::new(egui::WidgetText::Galley(galley.clone()))
            .selectable(true)
            .wrap_mode(egui::TextWrapMode::Wrap),
    );

    if let Some(i) = hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        // The tooltip and the right-click menu are both anchored at the pointer, so with the menu open
        // they draw over each other — the tooltip's path line lands on the menu's second row and both
        // become unreadable. The menu already names the target in its own way (its rows are what you
        // are about to do), so the tooltip steps aside while it is up.
        let quiet = resp.context_menu_opened() || ui.input(|i| i.pointer.any_down());
        let t = &hots[i].target;
        // The hint is phrased from what the click will actually do, and "not found" is stated
        // before the click rather than after it: a click that quietly does nothing would read as
        // a broken link.
        let hint = if !exists_on_hover(ui, resp.id, i, t) {
            aox::i18n::t("Path not found")
        } else if matches!(t, Target::Url(_)) {
            aox::i18n::t("Open in browser")
        } else {
            aox::i18n::t("Open containing folder")
        };
        if !quiet {
            egui::Tooltip::always_open(
                ui.ctx().clone(),
                ui.layer_id(),
                resp.id.with("aox-href"),
                egui::PopupAnchor::Pointer,
            )
            .gap(14.0)
            .show(|ui| {
                ui.label(RichText::new(t.text()).font(theme::mono_font(12.5)).color(P::text()));
                ui.label(RichText::new(hint).color(P::text_dim()).size(12.0));
            });
        }
    }

    // `clicked()` and not `drag_stopped()`: a selectable label senses `click_and_drag`, and only a
    // press-and-release **without** movement counts as a click — so dragging a selection across a
    // link still selects instead of opening something.
    if resp.clicked() {
        if let Some(i) = resp.interact_pointer_pos().and_then(|p| hot_at(&galley, hots, p - resp.rect.min)) {
            open::open(ui.ctx(), &hots[i].target);
        }
    }

    context_menu(ui, &resp, &galley, hots);
    resp
}

/// Right-click menu, **only on a link**.
///
/// The "only on a link" part is why this does not just call `Response::context_menu`: that opens on a
/// secondary click anywhere on the widget, and the widget here is a whole paragraph — a right-click on
/// ordinary prose would pop an empty menu (or, worse, act on the link the pointer happens to be
/// nearest to). So the hit is taken at the moment of the click, remembered per widget, and the popup
/// is force-closed when that click was not on a link.
fn context_menu(ui: &mut Ui, resp: &egui::Response, galley: &egui::Galley, hots: &[Hot]) {
    let slot = egui::Id::new("aox-href-menu");
    if resp.secondary_clicked() {
        let hit = resp
            .interact_pointer_pos()
            .and_then(|p| hot_at(galley, hots, p - resp.rect.min));
        ui.ctx().data_mut(|d| d.insert_temp(slot, (resp.id, hit)));
    }
    let Some((id, hit)) = ui.ctx().data(|d| d.get_temp::<(egui::Id, Option<usize>)>(slot)) else {
        return;
    };
    if id != resp.id {
        return;
    }
    let mut popup = egui::Popup::context_menu(resp);
    if hit.is_none() {
        // Also clears `Popup`'s memory, otherwise the *next* right-click on a link would find a menu
        // already marked open and open without a click.
        popup = popup.open_memory(Some(egui::containers::SetOpenCommand::Bool(false)));
    }
    popup.show(|ui| {
        if let Some(i) = hit {
            link_menu(ui, &hots[i].target);
        }
    });
}

/// The rows inside [`context_menu`], drawn with the app's own row widget so a menu opened in the
/// transcript is indistinguishable from the ones in the sidebar.
fn link_menu(ui: &mut Ui, t: &Target) {
    match t {
        Target::Url(url) => {
            if menu_row(ui, aox::i18n::t("Open in browser")) {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url.clone()));
                ui.close();
            }
            if menu_row(ui, aox::i18n::t("Copy link")) {
                ui.ctx().copy_text(url.clone());
                ui.close();
            }
        }
        Target::Path(p) => {
            if menu_row(ui, aox::i18n::t("Open containing folder")) {
                open::reveal(p);
                ui.close();
            }
            // The one action that starts a program. It lives **here** and not on the click for the
            // reason the whole feature is conservative: a reply full of `.exe` paths is exactly where
            // a single mis-aimed left click would run something.
            if menu_row(ui, aox::i18n::t("Open with default app")) {
                open::open_with_default(p);
                ui.close();
            }
            if menu_row(ui, aox::i18n::t("Copy path")) {
                ui.ctx().copy_text(p.display().to_string());
                ui.close();
            }
        }
    }
}

fn menu_row(ui: &mut Ui, label: &str) -> bool {
    crate::app::menu_item(ui, label, crate::app::MENU_W, true, false).clicked()
}

/// The glyph boxes of the characters in `[start, end)`, merged into one rect per row.
///
/// Built from `Glyph::logical_rect()` — the same rect the inline-code chip paints its background
/// from — so a wash under a chip lines up with the chip exactly instead of drifting by the leading.
fn run_rects(galley: &egui::Galley, start: usize, end: usize) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    for row in galley.rows.iter() {
        let mut acc: Option<egui::Rect> = None;
        for g in &row.row.glyphs {
            if idx >= start && idx < end {
                let r = g.logical_rect();
                acc = Some(acc.map_or(r, |a: egui::Rect| a.union(r)));
            }
            idx += 1;
        }
        if let Some(r) = acc {
            out.push(r);
        }
        // Newlines (and any whitespace elided at a wrap point) occupy text positions but have no
        // glyph of their own, so the counter has to be advanced past them explicitly.
        idx += row.row.char_count_including_newline().saturating_sub(row.row.glyphs.len());
    }
    out
}

/// Which hot run is under `local`, a position relative to the galley's origin.
///
/// The row check is not belt-and-braces: `cursor_from_pos` snaps to the **nearest** caret, so a
/// pointer in the empty right margin of a short row would otherwise "hit" whatever ends that row —
/// and since the label's rect is as wide as its widest row, that margin is inside the widget. Two
/// pixels of slack keep the row's own edge clickable.
fn hot_at(galley: &egui::Galley, hots: &[Hot], local: egui::Vec2) -> Option<usize> {
    let dist = |r: &egui::epaint::text::PlacedRow| {
        (r.min_y() - local.y).abs().min((r.max_y() - local.y).abs())
    };
    let row = galley.rows.iter().min_by(|a, b| dist(a).total_cmp(&dist(b)))?;
    let rect = row.rect();
    if local.x > rect.right() + 2.0 || local.x < rect.left() - 2.0 {
        return None;
    }
    let idx = galley.cursor_from_pos(local).index;
    hots.iter().position(|h| idx >= h.start && idx < h.end)
}

/// `Target::exists`, but asked **once per link**, not once per frame.
///
/// The tooltip is redrawn every frame the pointer sits on a link, and `exists` is a `stat` — cheap on
/// a local disk, but on a path over a slow share (or a sleeping network drive) 60 stats a second is
/// enough to make the whole window stutter while the pointer rests.
///
/// Only the **hint** is cached; the click re-checks from scratch (`open::reveal` refuses a path that
/// is gone), so a stale entry can at worst phrase one tooltip wrongly for as long as the app runs —
/// it can never open or fail to open something on the strength of it.
fn exists_on_hover(ui: &Ui, widget: egui::Id, index: usize, t: &Target) -> bool {
    let slot = egui::Id::new("aox-href-exists");
    let key = (widget, index);
    if let Some((seen, yes)) = ui.ctx().data(|d| d.get_temp::<((egui::Id, usize), bool)>(slot)) {
        if seen == key {
            return yes;
        }
    }
    let yes = open::exists(t);
    ui.ctx().data_mut(|d| d.insert_temp(slot, (key, yes)));
    yes
}

/// A task-list checkbox, painted from a rounded rect and two line segments.
///
/// Drawn rather than set as `☐` / `☑`: the app ships no icon font, and a missing glyph in the marker
/// column is a box with a hex number in it — the same reason as `call_glyph` and `jump_arrow`. The
/// mark is knocked out in the page colour rather than fixed to white, so a checked item stays legible
/// in both themes.
///
/// The slot is the same 20 points the bullet glyph gets, so a task list and a plain list line up on
/// the same axis even when they are mixed.
fn task_box(ui: &mut Ui, done: bool) {
    const SIDE: f32 = 13.0;
    // The height of one body line, so the box centres on the first row of the item rather than on the
    // top of the widget — a two-line task would otherwise hang its checkbox above the text.
    let row_h = ui
        .painter()
        .layout_no_wrap("x".into(), theme::body_font(theme::body_size()), P::text())
        .size()
        .y;
    let (slot, _) =
        ui.allocate_exact_size(egui::vec2(20.0, row_h.max(SIDE)), egui::Sense::hover());
    let r = egui::Rect::from_center_size(
        egui::pos2(slot.left() + SIDE * 0.5, slot.center().y),
        egui::vec2(SIDE, SIDE),
    );
    let p = ui.painter();
    if done {
        p.rect_filled(r, CornerRadius::same(3), P::ink());
        let w = Stroke::new(1.6, P::bg());
        p.line_segment(
            [egui::pos2(r.left() + 3.1, r.center().y + 0.4), egui::pos2(r.left() + 5.6, r.bottom() - 3.5)],
            w,
        );
        p.line_segment(
            [egui::pos2(r.left() + 5.6, r.bottom() - 3.5), egui::pos2(r.right() - 3.1, r.top() + 3.7)],
            w,
        );
    } else {
        p.rect_stroke(
            r,
            CornerRadius::same(3),
            Stroke::new(1.2, P::text_faint()),
            egui::StrokeKind::Inside,
        );
    }
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
                let (job, hots) = job_of(inline(&text), size, P::text(), width, level <= 2);
                text_label(ui, job, &hots);
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
                let (job, hots) = job_of(inline(&text), theme::body_size(), base, width, false);
                text_label(ui, job, &hots);
                ui.add_space(BLOCK_GAP);
            }

            Block::Bullet { indent, marker, text, task } => {
                let pad = (indent.min(3) as f32) * 20.0;
                let inner_w = (width - pad - 30.0).max(80.0);
                ui.horizontal_top(|ui| {
                    ui.add_space(pad);
                    ui.allocate_ui(egui::vec2(20.0, 0.0), |ui| {
                        ui.set_min_width(20.0);
                        match task {
                            Some(done) => task_box(ui, done),
                            None => {
                                let m = marker.clone().unwrap_or_else(|| "•".to_string());
                                ui.label(RichText::new(m).color(P::ink()).size(17.5));
                            }
                        }
                    });
                    let (job, hots) = job_of(inline(&text), theme::body_size(), base, inner_w, false);
                    text_label(ui, job, &hots);
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
                    let (job, hots) = job_of(spans, theme::body_size(), P::text_dim(), inner_w, false);
                    text_label(ui, job, &hots);
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
            //
            // `item_spacing.y` goes to zero for the same reason, one step further: the separators
            // below are drawn at `ui.cursor().min.y`, which is the top of the row **after** this one.
            // With the default gap still in place that y sits a few pixels below the row's real
            // bottom, so the line floats between the header and the first row and reads as an empty
            // band. Zeroing it puts the line exactly on the boundary it is meant to mark.
            ui.style_mut().spacing.item_spacing = egui::vec2(0.0, 0.0);

            for (ri, row) in std::iter::once(header)
                .chain(rows.iter().map(|r| r.as_slice()))
                .enumerate()
            {
                let head = ri == 0;
                if head {
                    // Header band: one strip for the whole row, rounded only on top, so it hugs the outer frame.
                    // `band()` (4% ink) rather than `surface_hi`: the latter is the hover step, and a
                    // header painted with it read as one wide disabled button (2026-10-29).
                    Frame::NONE
                        .fill(P::band())
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
                let (job, hots) = job_of(inline(txt), 15.0, color, w, head);
                text_label(ui, job, &hots);
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
            ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), bg);
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
            p.rect_stroke(back, CornerRadius::same(theme::R_XS), stroke, egui::StrokeKind::Inside);
            p.rect_filled(front, CornerRadius::same(theme::R_XS), bg);
            p.rect_stroke(front, CornerRadius::same(theme::R_XS), stroke, egui::StrokeKind::Inside);
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
        .corner_radius(CornerRadius::same(theme::R2))
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

    /// The chip's geometry, in points: `(above, below, baseline_delta)`.
    ///
    /// - `above` / `below` — the ink's clearance to the chip background's top and bottom edge. The
    ///   background is painted from `Glyph::logical_rect()`, whose height is the **format's** line
    ///   height — so a format asking for 1.5× leading gets a 1.5×-tall chip while the ink keeps only
    ///   its own ascent/descent. That asymmetry is what "the background hangs below the text" looks
    ///   like; this measures it instead of leaving it to the eye.
    /// - `baseline_delta` — how far the code's baseline sits from the body text's baseline on the
    ///   same line. Trimming the chip must not drag the text with it: epaint puts a glyph at
    ///   `ascent + valign.to_factor() * (max_row_height - line_height)`, so a **shorter** line height
    ///   is only free because `Align::TOP` makes that term zero — with the default `BOTTOM` the code
    ///   would drop by the whole trim.
    ///
    /// The code span is located by its characters (a glyph does not expose which section it came from),
    /// and the words used below carry both an ascender and a descender so the ink is not short.
    fn chip_geometry(md: &str, code: &str) -> (f32, f32, f32) {
        let ctx = egui::Context::default();
        // The app's own fonts, so this measures the metric the user actually sees (the fallback font
        // has different ascent/descent and would calibrate the wrong thing).
        crate::theme::install_fonts(&ctx);
        let _ = ctx.run(Default::default(), |_| {});
        let (job, _) = job_of(inline(md), 14.0, egui::Color32::BLACK, 800.0, false);
        let galley = ctx.fonts_mut(|f| f.layout_job(job));

        let want: Vec<char> = code.chars().collect();
        let glyphs: Vec<&egui::epaint::text::Glyph> =
            galley.rows.iter().flat_map(|r| r.row.glyphs.iter()).collect();
        for (start, w) in glyphs.windows(want.len()).enumerate() {
            if !w.iter().map(|g| g.chr).eq(want.iter().copied()) {
                continue;
            }
            let mut above = f32::INFINITY;
            let mut below = f32::INFINITY;
            for g in w {
                if g.uv_rect.size.y <= 0.0 {
                    continue;
                }
                let rect = g.logical_rect();
                let ink_top = g.pos.y + g.uv_rect.offset.y;
                above = above.min(ink_top - rect.top());
                below = below.min(rect.bottom() - (ink_top + g.uv_rect.size.y));
            }
            // Any glyph on the same row that is **not** part of the code run is body text; `pos.y` is
            // the baseline. The padding spaces share the code's format, so they are excluded by index.
            let last = start + want.len();
            let body = glyphs
                .iter()
                .enumerate()
                .filter(|(i, g)| (*i < start || *i >= last) && g.uv_rect.size.y > 0.0)
                .map(|(_, g)| g.pos.y)
                .next();
            let baseline_delta = body.map_or(f32::NAN, |b| w[0].pos.y - b);
            return (above, below, baseline_delta);
        }
        (f32::NAN, f32::NAN, f32::NAN)
    }

    #[test]
    fn the_inline_code_chip_is_centred_on_its_ink() {
        // The exact line from the screenshot that started this: CJK body text, one inline code span.
        let (above, below, baseline) = chip_geometry("避免 `dllimport` 冲突", "dllimport");
        println!("chip: above={above:.2} below={below:.2} baseline_delta={baseline:.2}");
        assert!(
            (above - below).abs() <= 1.5,
            "the chip is lopsided: {above:.2} above the ink, {below:.2} below it"
        );
        assert!(
            baseline.abs() <= 1.0,
            "the code text sits {baseline:.2} pt off the body baseline"
        );
    }

    /// Replays the column skeleton of `App::transcript`, measuring:
    /// - the available width the Ui reports when entering Markdown
    /// - the width the content actually occupies after rendering (beyond the column it gets clipped by the ScrollArea)
    fn probe(window_w: f32, md: &str) -> (f32, f32, f32) {
        let ctx = egui::Context::default();
        // Fonts are **per-Context**, but the "is bold available" flags in `theme` are process-global —
        // so a Context that skips this silently depends on whichever sibling test ran first: with the
        // flag left false it lays out in the fallback family, and once anything else has installed
        // fonts it asks for `aox-strong`, which this Context has never bound, and panics. Installing
        // here makes the measurement order-independent *and* faithful: it is what the app does.
        crate::theme::install_fonts(&ctx);
        let _ = ctx.run(Default::default(), |_| {});
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

    // ──────────────────── Clickable targets ────────────────────

    /// `(text, target)` for each clickable run of `md`, read back **out of the laid-out job**.
    ///
    /// Read from `job.text` by character offset rather than from the spans, because that is the
    /// coordinate space the hit test uses: a miscounted padding space then fails here, as a wrong
    /// string, instead of failing on screen as a hit box two characters off.
    fn hot_texts(md: &str) -> Vec<(String, Target)> {
        let (job, hots) = job_of(inline(md), 14.0, egui::Color32::BLACK, 800.0, false);
        hots.iter()
            .map(|h| {
                let text: String = job.text.chars().skip(h.start).take(h.end - h.start).collect();
                (text, h.target.clone())
            })
            .collect()
    }

    #[test]
    fn a_bare_url_in_a_sentence_becomes_one_clickable_run() {
        let got = hot_texts("see https://example.com/a?b=c for details");
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, "https://example.com/a?b=c");
        assert!(matches!(got[0].1, Target::Url(_)), "{:?}", got[0].1);
    }

    #[test]
    fn the_sentence_full_stop_is_not_part_of_the_path() {
        // Both endings a reply actually produces: an ASCII period, and Chinese glued straight on.
        for md in [r"written to E:\rustapp\a.txt.", "已生成E:\\rustapp\\a.txt，请查看"] {
            let got = hot_texts(md);
            assert_eq!(got.len(), 1, "{md}: {got:?}");
            assert_eq!(got[0].0, r"E:\rustapp\a.txt", "{md}");
        }
    }

    #[test]
    fn an_inline_code_path_is_clickable_and_excludes_the_chip_padding() {
        // The chip is laid out as `" " + text + " "`, so a naive offset is two characters off. The
        // box must cover exactly the path: no leading space, no trailing one.
        let got = hot_texts(r"文件在 `E:\rustapp\dist\aox.exe` 下");
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, r"E:\rustapp\dist\aox.exe");
    }

    #[test]
    fn offsets_stay_aligned_across_several_chips() {
        let got = hot_texts(r"`a` E:\one.txt `bb` E:\two.txt");
        let texts: Vec<&str> = got.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, vec![r"E:\one.txt", r"E:\two.txt"]);
    }

    #[test]
    fn prose_that_only_looks_like_a_target_stays_plain() {
        // All of these turn up in ordinary replies; underlining them would be noise on every answer.
        let got = hot_texts("aox 1.5, v1.2.3, aox.exe, src/main.rs, C: drive, http//broken");
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn a_quoted_path_may_contain_spaces() {
        let got = hot_texts(r#"put it in "E:\my folder\a.txt" now"#);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, r"E:\my folder\a.txt");
    }

    #[test]
    fn a_markdown_link_carries_its_href() {
        let got = hot_texts("报告在[这里](E:\\rustapp\\out.md)，站点见[主页](https://2am.top)");
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].0, "这里");
        assert_eq!(got[0].1, Target::Path("E:\\rustapp\\out.md".into()));
        assert_eq!(got[1].1, Target::Url("https://2am.top".into()));
    }

    /// The one part a string test cannot cover: a **pointer position** through the real galley.
    ///
    /// This is the half of the feature a unit test could otherwise only assume: clicking is out of the
    /// question here (it would open Explorer), so instead take a pixel that the path's own ink
    /// occupies and require the hit test to find the path; take a pixel in the words before it and
    /// require it not to.
    #[test]
    fn a_pointer_on_the_path_hits_it_and_a_pointer_before_it_does_not() {
        let ctx = egui::Context::default();
        // Fonts are per-Context; without this the galley is laid out in a family whose metrics are
        // not the ones on screen (and `theme`'s process-wide flags would make this depend on test
        // order — see the note in `probe`).
        crate::theme::install_fonts(&ctx);
        let _ = ctx.run(Default::default(), |_| {});

        let md = r"see E:\rustapp\a.txt now";
        let (job, hots) = job_of(inline(md), 14.0, egui::Color32::BLACK, 800.0, false);
        assert_eq!(hots.len(), 1, "precondition: one path");
        let galley = ctx.fonts_mut(|f| f.layout_job(job));
        let h = &hots[0];

        // A caret rect sits *between* characters, so `start + 1` puts us just after the path's first
        // character — i.e. inside the run, which is where a click on its ink lands.
        let inside = galley.pos_from_cursor(egui::text::CCursor::new(h.start + 1));
        let p = inside.center().to_vec2();
        assert_eq!(hot_at(&galley, &hots, p), Some(0), "a click on the path must hit it (at {p:?})");

        // The same row, but on the word "see" — plain text must stay unclickable.
        let before = galley.pos_from_cursor(egui::text::CCursor::new(1));
        let p = before.center().to_vec2();
        assert_eq!(hot_at(&galley, &hots, p), None, "plain text must not be a link (at {p:?})");
    }

    #[test]
    fn an_inline_code_path_is_clickable_only_when_it_exists_in_the_workspace() {
        // Relative paths are the one place where the *only* thing separating "a file" from "a string"
        // is the filesystem, so the existence check is load-bearing rather than a nicety. The root is
        // a thread-local, so this test cannot disturb any other one.
        let dir = std::env::temp_dir().join("aox-md-relative-7c1");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes").join("todo.md"), "# t\n").unwrap();
        crate::open::set_workspace_root(&dir);

        let got = hot_texts("wrote `notes/todo.md` just now");
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            matches!(&got[0].1, Target::Path(p) if p.ends_with("todo.md")),
            "{:?}",
            got[0].1
        );

        // Not there ⇒ plain text. This is what keeps `and/or`, `1.5/2` and `src/gone.rs` quiet.
        assert!(hot_texts("wrote `notes/gone.md` just now").is_empty());
        // ...and prose is never scanned for relative paths, only inline code is.
        assert!(hot_texts("wrote notes/todo.md just now").is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── task lists ────────────────────────────────────────────────────────────

    /// `- [x] done` becomes a checkbox, and the brackets leave the text.
    ///
    /// Models write GitHub checklists whenever a user asks for a plan or a todo list, and until now
    /// the reader got a bullet with a literal `[x]` in front of it (reported 2026-10-31, with a
    /// screenshot of a "今日清单").
    #[test]
    fn task_items_become_checkboxes() {
        let blocks = parse("- [x] 手写 block_on\n- [ ] 用 io_uring 重写\n- [X] upper case counts\n- plain item");
        let mut it = blocks.iter();
        for (done, text) in [
            (true, "手写 block_on"),
            (false, "用 io_uring 重写"),
            (true, "upper case counts"),
        ] {
            match it.next() {
                Some(Block::Bullet { task, text: t, .. }) => {
                    assert_eq!(*task, Some(done), "state of {text:?}");
                    assert_eq!(t, text, "the brackets must not survive into the sentence");
                }
                other => panic!("expected a task bullet, got {other:?}"),
            }
        }
        match it.next() {
            Some(Block::Bullet { task, text, .. }) => {
                assert_eq!(*task, None, "a plain bullet stays a bullet");
                assert_eq!(text, "plain item");
            }
            other => panic!("expected a plain bullet, got {other:?}"),
        }
    }

    /// Brackets that are not followed by a space are part of the sentence.
    ///
    /// `- [x]done` and `see [x] here` must keep every character — eating text the model wrote is a
    /// worse failure than showing a checkbox one time too few.
    #[test]
    fn a_bracket_run_without_a_trailing_space_is_not_a_checkbox() {
        assert_eq!(task_prefix("[x]done"), None);
        assert_eq!(task_prefix("[y] nope"), None, "only x/X/space mark a state");
        assert_eq!(task_prefix("plain"), None);
        assert_eq!(task_prefix("[x]"), None, "nothing follows, so nothing to tick");
        assert_eq!(task_prefix("[x]   spaced"), Some((true, "spaced".to_string())));
        assert_eq!(task_prefix("  [ ] indented"), Some((false, "indented".to_string())));
    }
}
