//! HTML → readable Markdown, without a parser dependency.
//!
//! # Why hand-written
//!
//! The valuable half of "make a web page usable" is **choosing the main content**, which none of the
//! HTML-to-Markdown crates do — they convert the whole page, navigation included. The other half
//! (tokens → Markdown) is a few hundred lines and buys exact control over two things that matter
//! here: **CJK spacing** (a Chinese paragraph must not come back as `你 好 世 界`) and **output size**
//! (the tool's budget is the whole point). Same reasoning as `gui/src/markdown.rs`.
//!
//! # Tolerance, not correctness
//!
//! Real pages are malformed: unclosed `<p>`, a `<div>` closed by `</span>`, attributes without
//! quotes. So the tokenizer never rejects anything — an unknown end tag is ignored, an unclosed
//! element is closed at EOF, nesting deeper than [`MAX_DEPTH`] stops growing, and `script`/`style`
//! bodies are skipped without being parsed. **No input may panic**; a test feeds it random bytes.
//!
//! # What it does not do
//!
//! No CSS, no `<base>`, no charset (that happened before the text got here), no JavaScript, no
//! `<iframe>` contents, no form state. A page whose text only exists after script execution comes
//! back thin; the tool is expected to say so in the result instead of pretending (`docs/WEB.md`
//! D10 — a headless renderer was built, measured, and removed).

use std::collections::BTreeSet;

use url::Url;

/// How deep the tree may nest. Deeper tags are parsed but not opened, which bounds both memory and
/// recursion (the scoring and rendering passes are recursive).
const MAX_DEPTH: usize = 256;

/// How many links `links` mode will ever list. The tool pages 60 at a time; this is the ceiling.
pub const MAX_LINKS: usize = 500;

/// One link found on the page, already absolute.
#[derive(Debug, Clone)]
pub struct Link {
    pub text: String,
    pub url: String,
}

/// Everything the tool needs from one HTML document.
#[derive(Debug, Default)]
pub struct Extracted {
    pub title: Option<String>,
    pub markdown: String,
    pub links: Vec<Link>,
}

/// Parse `html` and produce the readable body, the title, and the link list.
pub fn extract(html: &str, base: &Url) -> Extracted {
    let root = parse(html);
    let picked = pick_body(&root);
    let title = title_of(&root).or_else(|| {
        find_tag(picked, "h1").map(|n| inline_text(n, base)).filter(|s| !s.is_empty())
    });
    let mut md = String::new();
    render_blocks(picked, &mut md, base, 0);
    Extracted { title, markdown: tidy(&md), links: collect_links(&root, base) }
}

// ───────────────────────────────── the tree ─────────────────────────────────

#[derive(Debug)]
struct Node {
    tag: String,
    attrs: Vec<(String, String)>,
    children: Vec<Child>,
}

#[derive(Debug)]
enum Child {
    Node(Node),
    Text(String),
}

impl Node {
    fn new(tag: &str) -> Self {
        Self { tag: tag.to_string(), attrs: Vec::new(), children: Vec::new() }
    }

    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn append(stack: &mut Vec<Node>, root: &mut Node, child: Child) {
    match stack.last_mut() {
        Some(top) => top.children.push(child),
        None => root.children.push(child),
    }
}

fn push_text(stack: &mut Vec<Node>, root: &mut Node, text: &str) {
    if text.is_empty() {
        return;
    }
    match stack.last_mut() {
        Some(top) => match top.children.last_mut() {
            Some(Child::Text(prev)) => prev.push_str(text),
            _ => top.children.push(Child::Text(text.to_string())),
        },
        None => root.children.push(Child::Text(text.to_string())),
    }
}

// ─────────────────────────────── the tokenizer ───────────────────────────────

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
}

fn parse(html: &str) -> Node {
    let mut p = Parser { s: html, b: html.as_bytes(), i: 0 };
    let mut root = Node::new("#document");
    let mut stack: Vec<Node> = Vec::new();

    while p.i < p.b.len() {
        let Some(lt) = p.find(b'<') else {
            let tail = p.slice(p.i, p.b.len()).to_string();
            push_text(&mut stack, &mut root, &tail);
            p.i = p.b.len();
            break;
        };
        if lt > p.i {
            let text = p.slice(p.i, lt).to_string();
            push_text(&mut stack, &mut root, &text);
            p.i = lt;
        }

        if p.starts(b"<!--") {
            p.skip_past_str("-->");
            continue;
        }
        if p.starts(b"<!") || p.starts(b"<?") {
            p.skip_past(b'>');
            continue;
        }
        if p.starts(b"</") {
            let (name, end) = p.read_name(p.i + 2);
            p.i = end;
            p.skip_past(b'>');
            close(&mut stack, &mut root, &name);
            continue;
        }

        let (name, end) = p.read_name(p.i + 1);
        if name.is_empty() {
            // A stray `<` is text, not a tag.
            p.i += 1;
            let one = p.slice(lt, lt + 1).to_string();
            push_text(&mut stack, &mut root, &one);
            continue;
        }
        p.i = end;
        let attrs = p.read_attrs();
        let self_closing = p.consume_gt();

        if is_raw(&name) {
            match p.raw_until_close(&name) {
                Some((content, next)) => {
                    p.i = next;
                    if name == "title" {
                        let mut node = Node { tag: name, attrs, children: Vec::new() };
                        node.children.push(Child::Text(content));
                        append(&mut stack, &mut root, Child::Node(node));
                    }
                    // script / style / textarea: the text is never rendered, so it is dropped here.
                }
                None => {
                    // Unterminated raw element: everything left belongs to it.
                    if name == "title" {
                        let rest = p.slice(p.i, p.b.len()).to_string();
                        let mut node = Node { tag: name, attrs, children: Vec::new() };
                        node.children.push(Child::Text(rest));
                        append(&mut stack, &mut root, Child::Node(node));
                    }
                    p.i = p.b.len();
                }
            }
            continue;
        }

        let node = Node { tag: name, attrs, children: Vec::new() };
        if self_closing || is_void(&node.tag) {
            append(&mut stack, &mut root, Child::Node(node));
        } else if stack.len() < MAX_DEPTH {
            stack.push(node);
        }
        // Beyond MAX_DEPTH the element is skipped as a *container*, but its text still arrives in the
        // enclosing node — losing text would be worse than losing nesting.
    }

    while let Some(n) = stack.pop() {
        append(&mut stack, &mut root, Child::Node(n));
    }
    root
}

fn close(stack: &mut Vec<Node>, root: &mut Node, name: &str) {
    let Some(pos) = stack.iter().rposition(|n| n.tag == name) else {
        // An end tag with no opener: ignored, which is what every browser does with `</span>` alone.
        return;
    };
    while stack.len() > pos + 1 {
        let n = stack.pop().expect("len > pos + 1");
        append(stack, root, Child::Node(n));
    }
    let n = stack.pop().expect("the opener is on the stack");
    append(stack, root, Child::Node(n));
}

impl<'a> Parser<'a> {
    fn slice(&self, a: usize, b: usize) -> &'a str {
        &self.s[a..b]
    }

    fn find(&self, byte: u8) -> Option<usize> {
        self.b[self.i..].iter().position(|b| *b == byte).map(|p| p + self.i)
    }

    fn starts(&self, prefix: &[u8]) -> bool {
        self.b[self.i..].starts_with(prefix)
    }

    fn skip_past(&mut self, byte: u8) {
        match self.find(byte) {
            Some(at) => self.i = at + 1,
            None => self.i = self.b.len(),
        }
    }

    fn skip_past_str(&mut self, needle: &str) {
        match self.s[self.i..].find(needle) {
            Some(at) => self.i += at + needle.len(),
            None => self.i = self.b.len(),
        }
    }

    fn skip_ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\r' | b'\n' | b'\x0c')
        {
            self.i += 1;
        }
    }

    /// A tag name: letters, digits, `-`, `_`, `:`. Lower-cased; `(name, index_after_name)`.
    fn read_name(&mut self, at: usize) -> (String, usize) {
        let mut i = at;
        while i < self.b.len() {
            let c = self.b[i];
            if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':') {
                i += 1;
            } else {
                break;
            }
        }
        (self.s[at..i].to_ascii_lowercase(), i)
    }

    fn read_attrs(&mut self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        loop {
            self.skip_ws();
            if self.i >= self.b.len() {
                break;
            }
            if self.b[self.i] == b'>' {
                break;
            }
            if self.b[self.i] == b'/' && self.b.get(self.i + 1) == Some(&b'>') {
                self.i += 1;
                break;
            }
            let start = self.i;
            while self.i < self.b.len()
                && !matches!(self.b[self.i], b'=' | b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n')
            {
                self.i += 1;
            }
            if self.i == start {
                // A character that starts nothing we understand: skip it, never loop forever.
                self.i += 1;
                continue;
            }
            let name = self.s[start..self.i].to_ascii_lowercase();
            self.skip_ws();
            let mut value = String::new();
            if self.i < self.b.len() && self.b[self.i] == b'=' {
                self.i += 1;
                self.skip_ws();
                if self.i < self.b.len() && matches!(self.b[self.i], b'"' | b'\'') {
                    let quote = self.b[self.i];
                    self.i += 1;
                    let vs = self.i;
                    while self.i < self.b.len() && self.b[self.i] != quote {
                        self.i += 1;
                    }
                    value = self.s[vs..self.i].to_string();
                    if self.i < self.b.len() {
                        self.i += 1;
                    }
                } else {
                    let vs = self.i;
                    while self.i < self.b.len()
                        && !matches!(self.b[self.i], b'>' | b' ' | b'\t' | b'\r' | b'\n')
                    {
                        self.i += 1;
                    }
                    value = self.s[vs..self.i].to_string();
                }
            }
            if out.len() < 64 {
                out.push((name, value));
            }
        }
        out
    }

    fn consume_gt(&mut self) -> bool {
        self.skip_ws();
        if self.b.get(self.i) == Some(&b'/') {
            self.i += 1;
            if self.b.get(self.i) == Some(&b'>') {
                self.i += 1;
                return true;
            }
        }
        if self.b.get(self.i) == Some(&b'>') {
            self.i += 1;
        }
        false
    }

    /// For `script` / `style` / `title` / `textarea`: everything up to `</name`, and the index after
    /// that closing tag.
    fn raw_until_close(&mut self, name: &str) -> Option<(String, usize)> {
        let needle = format!("</{name}");
        let hay = &self.s[self.i..];
        let lower = hay.to_ascii_lowercase();
        let at = lower.find(&needle)?;
        let content = self.s[self.i..self.i + at].to_string();
        let after = self.i + at;
        let end = self.s[after..].find('>').map(|p| after + p + 1).unwrap_or(self.b.len());
        Some((content, end))
    }
}

fn is_void(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_raw(tag: &str) -> bool {
    matches!(tag, "script" | "style" | "title" | "textarea")
}

// ─────────────────────────────── picking content ───────────────────────────────

#[derive(Default, Clone, Copy)]
struct Stats {
    text: usize,
    link_text: usize,
    paragraphs: usize,
}

fn stats(node: &Node) -> Stats {
    let mut s = Stats::default();
    accumulate(node, &mut s, false);
    s
}

fn accumulate(node: &Node, s: &mut Stats, inside_link: bool) {
    if is_chrome(node) || is_dropped(&node.tag) {
        return;
    }
    match node.tag.as_str() {
        "a" => {
            let inner = plain_text(node).chars().count();
            s.text += inner;
            s.link_text += inner;
            return;
        }
        "p" => s.paragraphs += 1,
        _ => {}
    }
    for ch in &node.children {
        match ch {
            Child::Text(t) => {
                let n = t.chars().count();
                s.text += n;
                if inside_link {
                    s.link_text += n;
                }
            }
            Child::Node(n) => accumulate(n, s, inside_link || node.tag == "a"),
        }
    }
}

/// The `<body>` if there is one, else the document node.
fn body_of<'a>(root: &'a Node) -> &'a Node {
    find_tag(root, "body").unwrap_or(root)
}

fn find_tag<'a>(node: &'a Node, tag: &str) -> Option<&'a Node> {
    if node.tag == tag {
        return Some(node);
    }
    for ch in &node.children {
        if let Child::Node(n) = ch {
            if let Some(hit) = find_tag(n, tag) {
                return Some(hit);
            }
        }
    }
    None
}

fn is_semantic(node: &Node) -> bool {
    matches!(node.tag.as_str(), "article" | "main")
        || node.attr("role").map(|r| r.eq_ignore_ascii_case("main")).unwrap_or(false)
}

fn is_candidate(node: &Node) -> bool {
    if is_semantic(node) {
        return true;
    }
    if !matches!(node.tag.as_str(), "div" | "section") {
        return false;
    }
    let hint = format!(
        "{} {}",
        node.attr("id").unwrap_or(""),
        node.attr("class").unwrap_or("")
    )
    .to_ascii_lowercase();
    ["article", "content", "post", "entry", "main", "markdown", "story"]
        .iter()
        .any(|k| hint.contains(k))
}

/// Readability, reduced to its useful core: the element with the most **non-link text** and the most
/// paragraphs wins, and a sliver of the page only wins when the page itself is a link farm.
fn pick_body(root: &Node) -> &Node {
    let body = body_of(root);
    let body_stats = stats(body);
    let body_density = density(body_stats);

    // `(semantic, score)` compared lexicographically: `<article>` / `<main>` beat a class-hinted
    // `<div>`, because they are what the page author *called* the content. Among equals, the higher
    // score wins; the walk is parent-first, so a wrapper wins over the element it wraps.
    let mut best: Option<(&Node, i64, bool)> = None;
    visit(root, &mut |n| {
        if !is_candidate(n) || is_chrome(n) {
            return;
        }
        let st = stats(n);
        if st.text < 40 || density(st) > 0.5 {
            return;
        }
        let sc = score(st);
        let semantic = is_semantic(n);
        let better = match best {
            None => sc > 0,
            Some((_, b, semantic_b)) => sc > 0 && (semantic, sc) > (semantic_b, b),
        };
        if better {
            best = Some((n, sc, semantic));
        }
    });

    let Some((node, _, semantic)) = best else {
        return body;
    };
    if semantic {
        return node;
    }
    // A class-hinted div is weaker evidence. A 300-character "article" inside a 20,000-character page
    // is usually a sidebar teaser — take it only when the rest of the page really is links.
    let st = stats(node);
    if st.text < 400 && st.text * 6 < body_stats.text && body_density < 0.3 {
        body
    } else {
        node
    }
}

fn density(s: Stats) -> f32 {
    if s.text == 0 {
        0.0
    } else {
        s.link_text as f32 / s.text as f32
    }
}

fn score(s: Stats) -> i64 {
    s.text as i64 - s.link_text as i64 + 50 * s.paragraphs as i64
}

fn visit<'a>(node: &'a Node, f: &mut impl FnMut(&'a Node)) {
    f(node);
    for ch in &node.children {
        if let Child::Node(n) = ch {
            visit(n, f);
        }
    }
}

// ─────────────────────────────── rendering ───────────────────────────────

fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "html"
            | "body"
            | "div"
            | "section"
            | "article"
            | "main"
            | "header"
            | "footer"
            | "aside"
            | "nav"
            | "p"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "li"
            | "dl"
            | "dt"
            | "dd"
            | "pre"
            | "blockquote"
            | "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "td"
            | "th"
            | "hr"
            | "figure"
            | "figcaption"
            | "form"
            | "details"
            | "summary"
    )
}

/// Tags whose content never reaches the reader: chrome, scripts, and interactive widgets.
fn is_dropped(tag: &str) -> bool {
    matches!(
        tag,
        "script"
            | "style"
            | "noscript"
            | "template"
            | "svg"
            | "canvas"
            | "iframe"
            | "form"
            | "input"
            | "button"
            | "select"
            | "option"
            | "nav"
            | "footer"
            | "aside"
    )
}

/// Page furniture that is not the article and not a link list: menus, search boxes, share bars,
/// cookie banners, sidebars, "related" rails.
///
/// Matched on **tokens** of `id`/`class`, never on substrings: `research` must not match `search`,
/// and `protocol` must not match `toc`. A blanket rule is what makes readability-style extractors
/// drop a page's actual content because its wrapper is called `comment-content`.
fn is_chrome(node: &Node) -> bool {
    const TOKENS: [&str; 24] = [
        "menu",
        "menubar",
        "sidebar",
        "nav",
        "navigation",
        "breadcrumb",
        "breadcrumbs",
        "toolbar",
        "search",
        "share",
        "social",
        "cookie",
        "cookies",
        "banner",
        "pagination",
        "pager",
        "comment",
        "comments",
        "related",
        "advert",
        "ads",
        "promo",
        "hidden",
        "toc",
    ];
    if let Some(role) = node.attr("role") {
        let role = role.to_ascii_lowercase();
        if matches!(
            role.as_str(),
            "menu" | "menubar" | "navigation" | "banner" | "search" | "toolbar"
        ) {
            return true;
        }
    }
    if node.attr("hidden").is_some() {
        return true;
    }
    if node.attr("aria-hidden").map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(false) {
        return true;
    }
    if !matches!(node.tag.as_str(), "div" | "section" | "header" | "ul" | "ol" | "form" | "aside") {
        return false;
    }
    let hint = format!(
        "{} {}",
        node.attr("id").unwrap_or(""),
        node.attr("class").unwrap_or("")
    )
    .to_ascii_lowercase();
    hint.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| !t.is_empty() && TOKENS.contains(&t))
}

fn render_blocks(node: &Node, out: &mut String, base: &Url, depth: usize) {
    if depth > 64 || is_chrome(node) {
        return;
    }
    match node.tag.as_str() {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = node.tag.as_bytes()[1] as usize - b'0' as usize;
            let text = inline_text(node, base);
            if !text.is_empty() {
                push_block(out, &format!("{} {}", "#".repeat(level), text));
            }
        }
        "ul" | "ol" => {
            // A list's indentation is its **list** nesting, not the element depth: a `<ul>` inside
            // five `<div>`s is still a top-level list.
            let list = render_list(node, base, 0);
            push_block(out, &list);
        }
        "pre" => {
            let (lang, code) = code_of(node);
            if !code.trim().is_empty() {
                push_block(out, &fence(&lang, &code));
            }
        }
        "blockquote" => {
            let mut inner = String::new();
            render_children(node, &mut inner, base, depth + 1);
            let quoted: String = inner
                .trim()
                .lines()
                .map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") })
                .collect::<Vec<_>>()
                .join("\n");
            push_block(out, &quoted);
        }
        "table" => {
            if let Some(t) = render_table(node, base) {
                push_block(out, &t);
            }
        }
        "hr" => push_block(out, "---"),
        "br" => out.push('\n'),
        t if is_dropped(t) => {}
        _ => render_children(node, out, base, depth + 1),
    }
}

fn render_children(node: &Node, out: &mut String, base: &Url, depth: usize) {
    let mut run = String::new();
    for ch in &node.children {
        match ch {
            Child::Text(t) => run.push_str(&decode_entities(t)),
            Child::Node(n) if is_block(&n.tag) => {
                flush_run(&mut run, out);
                render_blocks(n, out, base, depth);
            }
            Child::Node(n) => collect_inline(n, &mut run, base),
        }
    }
    flush_run(&mut run, out);
}

fn flush_run(run: &mut String, out: &mut String) {
    if run.trim().is_empty() {
        run.clear();
        return;
    }
    let text = normalize_inline(run);
    run.clear();
    if !text.is_empty() {
        push_block(out, &text);
    }
}

fn push_block(out: &mut String, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if !out.is_empty() && !out.ends_with("\n\n") {
        if out.ends_with('\n') {
            out.push('\n');
        } else {
            out.push_str("\n\n");
        }
    }
    out.push_str(text);
    out.push_str("\n\n");
}

fn render_list(node: &Node, base: &Url, depth: usize) -> String {
    let ordered = node.tag == "ol";
    let indent = "  ".repeat(depth);
    let mut out = String::new();
    let mut index = 1usize;
    for ch in &node.children {
        let Child::Node(li) = ch else { continue };
        if li.tag != "li" {
            continue;
        }
        let mut text = String::new();
        for c in &li.children {
            match c {
                Child::Node(n) if n.tag == "ul" || n.tag == "ol" => {}
                Child::Node(n) if is_block(&n.tag) => {
                    let t = inline_text(n, base);
                    if !t.is_empty() {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(&t);
                    }
                }
                Child::Text(t) => text.push_str(&decode_entities(t)),
                Child::Node(n) => collect_inline(n, &mut text, base),
            }
        }
        let label = normalize_inline(&text);
        let marker = if ordered { format!("{index}. ") } else { "- ".to_string() };
        index += 1;
        out.push_str(&indent);
        out.push_str(&marker);
        out.push_str(&label);
        out.push('\n');
        for c in &li.children {
            if let Child::Node(n) = c {
                if n.tag == "ul" || n.tag == "ol" {
                    out.push_str(&render_list(n, base, depth + 1));
                }
            }
        }
    }
    out.trim_end().to_string()
}

fn code_of(node: &Node) -> (String, String) {
    let mut lang = String::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if let Some(class) = n.attr("class") {
            for part in class.split_whitespace() {
                if let Some(rest) = part.strip_prefix("language-").or_else(|| part.strip_prefix("lang-"))
                {
                    if lang.is_empty() {
                        lang = rest.to_string();
                    }
                }
            }
        }
        for ch in n.children.iter().rev() {
            if let Child::Node(c) = ch {
                stack.push(c);
            }
        }
    }
    let raw = plain_text(node).replace("\r\n", "\n").replace('\r', "\n");
    (lang, raw.trim_matches('\n').to_string())
}

fn fence(lang: &str, code: &str) -> String {
    let ticks = if code.contains("```") { "````" } else { "```" };
    format!("{ticks}{lang}\n{code}\n{ticks}")
}

fn render_table(node: &Node, base: &Url) -> Option<String> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    collect_rows(node, &mut rows, base);
    if rows.is_empty() {
        return None;
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if rows.len() > 50 || cols > 8 || cols == 0 {
        return Some(format!("[table omitted: {} rows × {} cols]", rows.len(), cols));
    }
    for row in &mut rows {
        row.resize(cols, String::new());
        for cell in row.iter_mut() {
            *cell = cell.replace('|', "\\|").replace('\n', " ");
        }
    }
    let mut out = String::new();
    let head = &rows[0];
    out.push('|');
    for cell in head {
        out.push(' ');
        out.push_str(cell);
        out.push_str(" |");
    }
    out.push('\n');
    out.push('|');
    for _ in 0..cols {
        out.push_str(" --- |");
    }
    for row in &rows[1..] {
        out.push('\n');
        out.push('|');
        for cell in row {
            out.push(' ');
            out.push_str(cell);
            out.push_str(" |");
        }
    }
    Some(out)
}

fn collect_rows(node: &Node, rows: &mut Vec<Vec<String>>, base: &Url) {
    for ch in &node.children {
        let Child::Node(n) = ch else { continue };
        match n.tag.as_str() {
            "tr" => {
                let mut cells = Vec::new();
                for c in &n.children {
                    if let Child::Node(cell) = c {
                        if cell.tag == "td" || cell.tag == "th" {
                            cells.push(inline_text(cell, base));
                        }
                    }
                }
                if !cells.is_empty() {
                    rows.push(cells);
                }
            }
            "table" => {} // a nested table is its own block; do not fold it into this one
            _ => collect_rows(n, rows, base),
        }
    }
}

// ─────────────────────────────── inline text ───────────────────────────────

fn inline_text(node: &Node, base: &Url) -> String {
    let mut s = String::new();
    for ch in &node.children {
        match ch {
            Child::Text(t) => s.push_str(&decode_entities(t)),
            Child::Node(n) => collect_inline(n, &mut s, base),
        }
    }
    normalize_inline(&s)
}

fn collect_inline(node: &Node, out: &mut String, base: &Url) {
    if is_dropped(&node.tag) {
        return;
    }
    match node.tag.as_str() {
        "a" => {
            let label = inline_text(node, base);
            let href = node.attr("href").unwrap_or("").trim();
            let shown = if label.is_empty() {
                href.to_string()
            } else {
                label
            };
            match resolve(base, href) {
                Some(url) => {
                    out.push('[');
                    out.push_str(&shown);
                    out.push_str("](");
                    out.push_str(&url);
                    out.push(')');
                }
                None => out.push_str(&shown),
            }
        }
        "img" => {
            let src = node.attr("src").unwrap_or("").trim();
            if src.is_empty() || src.starts_with("data:") || tracking_pixel(node) {
                return;
            }
            if let Some(url) = resolve(base, src) {
                out.push_str("![");
                out.push_str(&plain_text(node));
                out.push_str("](");
                out.push_str(&url);
                out.push(')');
            }
        }
        "strong" | "b" => wrap(node, out, base, "**"),
        "em" | "i" => wrap(node, out, base, "*"),
        "del" | "s" | "strike" => wrap(node, out, base, "~~"),
        "code" => {
            let text = plain_text(node).replace('\n', " ");
            if !text.trim().is_empty() {
                out.push('`');
                out.push_str(text.trim());
                out.push('`');
            }
        }
        "br" => out.push('\n'),
        "script" | "style" => {}
        _ => {
            for ch in &node.children {
                match ch {
                    Child::Text(t) => out.push_str(&decode_entities(t)),
                    Child::Node(n) => collect_inline(n, out, base),
                }
            }
        }
    }
}

fn wrap(node: &Node, out: &mut String, base: &Url, mark: &str) {
    let inner = inline_text(node, base);
    if inner.is_empty() {
        return;
    }
    out.push_str(mark);
    out.push_str(&inner);
    out.push_str(mark);
}

fn tracking_pixel(node: &Node) -> bool {
    let one = |name: &str| node.attr(name).map(|v| v.trim() == "1").unwrap_or(false);
    one("width") && one("height")
}

fn resolve(base: &Url, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
        return None;
    }
    let url = base.join(href).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

/// Text with no Markdown decoration — used for `<title>`, link labels and table cells.
fn plain_text(node: &Node) -> String {
    let mut s = String::new();
    fn walk(n: &Node, s: &mut String) {
        for ch in &n.children {
            match ch {
                Child::Text(t) => s.push_str(&decode_entities(t)),
                Child::Node(c) if !is_dropped(&c.tag) => walk(c, s),
                _ => {}
            }
        }
    }
    walk(node, &mut s);
    s
}

// ─────────────────────────────── text hygiene ───────────────────────────────

/// Collapse whitespace and remove the spaces that Chinese typesetting never had.
///
/// "你 好" → "你好" when both sides are CJK, but "中文 English" keeps its space. Without this, every
/// page that wraps a sentence in a `<span>` comes back with holes in it.
fn normalize_inline(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    let mut last: Option<char> = None;
    for c in raw.chars() {
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            if let Some(l) = last {
                if !(is_cjk(l) && is_cjk(c)) {
                    out.push(' ');
                }
            }
            pending_space = false;
        }
        out.push(c);
        last = Some(c);
    }
    out.trim().to_string()
}

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3000..=0x303F   // CJK punctuation
            | 0x3040..=0x30FF // kana
            | 0x3400..=0x4DBF // extension A
            | 0x4E00..=0x9FFF // unified
            | 0xF900..=0xFAFF // compatibility
            | 0xFF00..=0xFFEF // fullwidth forms
            | 0x20000..=0x2FA1F // extensions B+
    )
}

/// `&amp;` / `&#233;` / `&#xE9;` → the character. Unknown names are left alone (`&foo;` stays).
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        // Entity names are short and ASCII, so only the first 32 **bytes** are worth searching —
        // but that cut can land inside a multi-byte character, and `&[..32]` on a `str` panics
        // when it does. Walk back to a boundary instead (no `floor_char_boundary` on stable).
        let mut cut = tail.len().min(32);
        while cut > 0 && !tail.is_char_boundary(cut) {
            cut -= 1;
        }
        let Some(semi) = tail[..cut].find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let body = &tail[1..semi];
        if let Some(text) = entity(body) {
            out.push_str(&text);
            rest = &tail[semi + 1..];
        } else {
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

fn entity(body: &str) -> Option<String> {
    if let Some(num) = body.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse::<u32>().ok()?,
        };
        return char::from_u32(code).map(|c| c.to_string());
    }
    let c = match body.to_ascii_lowercase().as_str() {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{00A0}',
        "ensp" => '\u{2002}',
        "emsp" => '\u{2003}',
        "thinsp" => '\u{2009}',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "laquo" => '«',
        "raquo" => '»',
        "ldquo" => '“',
        "rdquo" => '”',
        "lsquo" => '‘',
        "rsquo" => '’',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "times" => '×',
        "divide" => '÷',
        "deg" => '°',
        "plusmn" => '±',
        "middot" => '·',
        "bull" => '•',
        "sect" => '§',
        "para" => '¶',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "larr" => '←',
        "rarr" => '→',
        "uarr" => '↑',
        "darr" => '↓',
        "harr" => '↔',
        "minus" => '−',
        "le" => '≤',
        "ge" => '≥',
        "ne" => '≠',
        "asymp" => '≈',
        "infin" => '∞',
        _ => return None,
    };
    Some(c.to_string())
}

/// Final pass: one blank line between blocks, no trailing whitespace, fence contents untouched.
fn tidy(md: &str) -> String {
    let mut out = String::new();
    let mut blanks = 0usize;
    let mut in_fence = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if in_fence {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
            out.push('\n');
            continue;
        }
        blanks = 0;
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim_end().to_string()
}

// ─────────────────────────────── links & title ───────────────────────────────

fn title_of(root: &Node) -> Option<String> {
    let t = find_tag(root, "title").map(|n| normalize_inline(&plain_text(n)))?;
    (!t.is_empty()).then_some(t)
}

fn collect_links(root: &Node, base: &Url) -> Vec<Link> {
    let mut out: Vec<Link> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    walk_links(root, base, &mut out, &mut seen);
    out
}

fn walk_links(node: &Node, base: &Url, out: &mut Vec<Link>, seen: &mut BTreeSet<String>) {
    if out.len() >= MAX_LINKS {
        return;
    }
    if node.tag == "a" {
        if let Some(href) = node.attr("href") {
            if let Some(url) = resolve(base, href) {
                if seen.insert(url.clone()) {
                    let text = normalize_inline(&plain_text(node));
                    out.push(Link { text, url });
                }
            }
        }
    }
    for ch in &node.children {
        if let Child::Node(n) = ch {
            walk_links(n, base, out, seen);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/docs/page.html").unwrap()
    }

    #[test]
    fn the_article_wins_over_the_navigation() {
        let mut html = String::from("<html><head><title>Doc</title></head><body>");
        html.push_str("<nav><a href=\"/a\">A</a><a href=\"/b\">B</a><a href=\"/c\">C</a></nav>");
        html.push_str("<div class=\"sidebar\"><a href=\"/x\">x</a><a href=\"/y\">y</a></div>");
        html.push_str("<article><h1>标题</h1><p>第一段正文，足够长，让打分器相信这里是正文内容。</p>");
        html.push_str("<p>第二段也有足够的文字，并且没有多少链接。</p></article>");
        html.push_str("<footer>© 2026</footer></body></html>");
        let out = extract(&html, &base());
        assert_eq!(out.title.as_deref(), Some("Doc"));
        assert!(out.markdown.starts_with("# 标题"), "{}", out.markdown);
        assert!(out.markdown.contains("第一段正文"), "{}", out.markdown);
        assert!(!out.markdown.contains("© 2026"), "footer must be dropped: {}", out.markdown);
    }

    #[test]
    fn entities_are_decoded_and_cjk_spaces_disappear() {
        let html = "<p>中文 <span>测试</span> &amp; 实体 &lt;tag&gt; &#20320;&#x597d;</p>";
        let out = extract(html, &base());
        assert_eq!(out.markdown, "中文测试 & 实体 <tag> 你好");
        // Inline emphasis keeps its Markdown markers, and the space before them is Markdown's, not
        // the page's: a renderer shows one word.
        assert!(extract("<p>非常 <b>重要</b></p>", &base()).markdown.contains("**重要**"));
    }

    /// A bare `&` followed by a run of CJK put byte 32 in the middle of a character and the
    /// entity scanner sliced a `str` there — a panic on real Bing result pages (2026-10-29).
    #[test]
    fn a_bare_ampersand_does_not_slice_a_character_in_half() {
        for run in 28..=36 {
            let text = format!("&{}中文正文", "a".repeat(run));
            assert_eq!(decode_entities(&text), text, "run of {run} must pass through unchanged");
        }
        assert_eq!(decode_entities("&amp;中文"), "&中文");
        assert_eq!(decode_entities("&abcdefghijklmnopqrstuvwxyzabcdefghij中文"), "&abcdefghijklmnopqrstuvwxyzabcdefghij中文");
    }

    #[test]
    fn a_link_keeps_its_label_and_resolves_relative_hrefs() {
        let html = r#"<p>see <a href="../guide/intro.html">the guide</a> and <a href="https://other.test/x">x</a></p>"#;
        let out = extract(html, &base());
        assert!(
            out.markdown.contains("[the guide](https://example.com/guide/intro.html)"),
            "{}",
            out.markdown
        );
        assert_eq!(out.links.len(), 2);
        assert_eq!(out.links[0].url, "https://example.com/guide/intro.html");
        assert_eq!(out.links[0].text, "the guide");
    }

    #[test]
    fn anchors_and_scripts_are_not_links() {
        let html = r##"<a href="#top">top</a><a href="javascript:void(0)">js</a><a href="mailto:a@b.c">mail</a><a href="/real">real</a>"##;
        let out = extract(html, &base());
        assert_eq!(out.links.len(), 1, "{:?}", out.links);
        assert_eq!(out.links[0].url, "https://example.com/real");
    }

    #[test]
    fn links_are_deduped_by_url() {
        let html = r#"<a href="/x">one</a><a href="/x">two</a><a href="/y">three</a>"#;
        let out = extract(html, &base());
        assert_eq!(out.links.len(), 2);
        assert_eq!(out.links[0].text, "one");
    }

    #[test]
    fn a_code_block_becomes_a_fence_with_its_language() {
        let html = r#"<pre><code class="language-rust">fn main() {
    println!("hi");
}</code></pre>"#;
        let out = extract(html, &base());
        assert!(out.markdown.contains("```rust"), "{}", out.markdown);
        assert!(out.markdown.contains("    println!"), "{}", out.markdown);
    }

    #[test]
    fn a_table_becomes_a_pipe_table() {
        let html = "<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>";
        let out = extract(html, &base());
        assert!(out.markdown.contains("| a | b |"), "{}", out.markdown);
        assert!(out.markdown.contains("| 1 | 2 |"), "{}", out.markdown);
    }

    #[test]
    fn a_huge_table_is_summarised_instead_of_dumped() {
        let mut html = String::from("<table><tr>");
        for i in 0..9 {
            html.push_str(&format!("<th>c{i}</th>"));
        }
        html.push_str("</tr></table>");
        let out = extract(&html, &base());
        assert!(out.markdown.contains("table omitted"), "{}", out.markdown);
    }

    #[test]
    fn sibling_list_items_stay_at_one_level() {
        let html = "<ul><li>one</li><li>two with <code>x</code></li><li>three</li></ul>";
        let out = extract(html, &base());
        assert_eq!(out.markdown, "- one\n- two with `x`\n- three");
    }

    #[test]
    fn a_list_nests() {
        let html = "<ul><li>one</li><li>two<ul><li>inner</li></ul></li></ul>";
        let out = extract(html, &base());
        assert_eq!(out.markdown, "- one\n- two\n  - inner");
    }

    #[test]
    fn malformed_markup_does_not_panic_or_lose_the_text() {
        for html in [
            "<div><p>a<div>b</span></p></div>",
            "<p>unclosed",
            "<",
            "</span></div>",
            "<a href='没有引号>标签</a>",
            "<table><tr><td>1</table>",
            "<h2>x",
            "<![CDATA[ nope ]]>",
        ] {
            let out = extract(html, &base());
            let _ = out.markdown;
        }
        // Text inside broken nesting still reaches the reader.
        assert!(extract("<div><p>kept<div>also", &base()).markdown.contains("kept"));
    }

    #[test]
    fn random_bytes_never_panic() {
        // Deterministic pseudo-random noise shaped like markup, so a crash is reproducible.
        let mut seed = 0x1234_5678u32;
        for _ in 0..40 {
            let mut s = String::from("<div>");
            for _ in 0..200 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let c = char::from_u32(seed % 0x80).unwrap_or('x');
                s.push(c);
            }
            s.push_str("</div>");
            let _ = extract(&s, &base());
        }
    }

    #[test]
    fn an_empty_document_is_empty_and_titleless() {
        let out = extract("", &base());
        assert!(out.markdown.is_empty());
        assert!(out.title.is_none());
        assert!(out.links.is_empty());
    }

    #[test]
    fn script_bodies_are_not_text() {
        let out = extract("<p>shown</p><script>var secret = 'hidden';</script>", &base());
        assert_eq!(out.markdown, "shown");
    }

    #[test]
    fn a_half_megabyte_page_is_handled_quickly() {
        let mut html = String::with_capacity(900 * 1024);
        html.push_str("<html><body><article>");
        for i in 0..12_000 {
            html.push_str(&format!("<p>段落 {i} 有一些文字，还有一些 English words 混排。</p>"));
        }
        html.push_str("</article></body></html>");
        assert!(html.len() > 500 * 1024);
        let started = std::time::Instant::now();
        let out = extract(&html, &base());
        assert!(out.markdown.contains("段落 11999"));
        assert!(out.markdown.len() > 50_000);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "extraction took {:?}",
            started.elapsed()
        );
    }
}
