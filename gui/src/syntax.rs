//! Syntax highlighting for code blocks.
//! Deliberately no regex, and no per-language parser: one generic character scan plus a per-language config table...
//! The palette is a low-saturation warm set whose entries differ mainly in lightness — the UI's subject remains...

use egui::Color32;

use crate::theme::{self, Mode, P};

// Tokens

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    Plain,
    Keyword,
    /// Type names: PascalCase, primitive types, built-in names like `u32`
    Type,
    /// An identifier immediately followed by an open paren (function names and call sites), plus
    /// attribute names in markup languages
    Func,
    Str,
    Num,
    Comment,
    /// Preprocessor directives, macros, shell variables, tag names like `<div>`, and "key name"-shaped strings
    Meta,
}

struct Spec {
    kw: Color32,
    ty: Color32,
    func: Color32,
    str_: Color32,
    num: Color32,
    cmt: Color32,
    meta: Color32,
}

impl Spec {
    fn of(&self, t: Tok) -> Color32 {
        match t {
            Tok::Plain => P::text(),
            Tok::Keyword => self.kw,
            Tok::Type => self.ty,
            Tok::Func => self.func,
            Tok::Str => self.str_,
            Tok::Num => self.num,
            Tok::Comment => self.cmt,
            Tok::Meta => self.meta,
        }
    }
}

/// On paper: ochre-red keywords, pine-green types, steel-blue functions, olive strings, plum numerals, warm-grey...
const LIGHT: Spec = Spec {
    kw: Color32::from_rgb(0xA3, 0x4A, 0x2A),
    ty: Color32::from_rgb(0x1F, 0x7A, 0x6B),
    func: Color32::from_rgb(0x2F, 0x66, 0x90),
    str_: Color32::from_rgb(0x4C, 0x7A, 0x2F),
    num: Color32::from_rgb(0x8A, 0x5A, 0x9E),
    cmt: Color32::from_rgb(0x5E, 0x59, 0x4E),
    meta: Color32::from_rgb(0x7E, 0x54, 0x10),
};

/// On ink: the same hues brightened overall, with comments kept the weakest.
const DARK: Spec = Spec {
    kw: Color32::from_rgb(0xE0, 0x8A, 0x6B),
    ty: Color32::from_rgb(0x6F, 0xC7, 0xB4),
    func: Color32::from_rgb(0x86, 0xB6, 0xDD),
    str_: Color32::from_rgb(0xA8, 0xC9, 0x7A),
    num: Color32::from_rgb(0xC9, 0xA0, 0xDC),
    cmt: Color32::from_rgb(0xAD, 0xA9, 0x9F),
    meta: Color32::from_rgb(0xD8, 0xB2, 0x6A),
};

/// The color of a token under the current light/dark mode (evaluated at runtime, so switching theme takes effect immediately).
pub fn color(t: Tok) -> Color32 {
    match theme::mode() {
        Mode::Light => LIGHT.of(t),
        Mode::Dark => DARK.of(t),
    }
}

// Language config

struct Cfg {
    /// Line-comment prefixes
    line: &'static [&'static str],
    /// Block-comment delimiters
    block: Option<(&'static str, &'static str)>,
    /// Single-line string delimiters. Rust deliberately omits the `'` — otherwise a lifetime
    /// like `&'a str` would be read as a char literal and swallow the rest of the line.
    quotes: &'static [char],
    /// Triple-quoted multi-line strings (Python docstrings)
    triple: bool,
    /// Backtick strings (JS template strings, shell's `$(...)`)
    backtick: bool,
    /// Keywords
    keywords: &'static [&'static str],
    /// Built-in types
    types: &'static [&'static str],
    /// Whether keywords ignore case (SQL)
    ci: bool,
    /// Markup languages: `<tag>` and attribute names are colored separately
    markup: bool,
    /// A leading `#` is a preprocessor directive, not a comment
    hash_meta: bool,
}

const TEXT: Cfg = Cfg {
    line: &[],
    block: None,
    quotes: &['"'],
    triple: false,
    backtick: false,
    keywords: &[],
    types: &[],
    ci: false,
    markup: false,
    hash_meta: false,
};

const RUST: Cfg = Cfg {
    line: &["//"],
    block: Some(("/*", "*/")),
    quotes: &['"'],
    triple: false,
    backtick: false,
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "do", "dyn", "else", "enum",
        "extern", "final", "fn", "for", "if", "impl", "in", "let", "loop", "macro", "match", "mod",
        "move", "mut", "override", "priv", "pub", "ref", "return", "self", "Self", "static", "struct",
        "super", "trait", "try", "type", "union", "unsafe", "unsized", "use", "virtual", "where",
        "while", "yield",
    ],
    types: &[
        "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8", "u16",
        "u32", "u64", "u128", "usize", "String", "Vec", "Option", "Result", "Box", "Rc", "Arc",
        "Cow", "Path", "PathBuf", "HashMap", "BTreeMap", "Some", "None", "Ok", "Err",
    ],
    ci: false,
    markup: false,
    hash_meta: true,
};

const PYTHON: Cfg = Cfg {
    line: &["#"],
    block: None,
    quotes: &['"', '\''],
    triple: true,
    backtick: false,
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "case", "class", "continue", "def", "del",
        "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is",
        "lambda", "match", "nonlocal", "not", "or", "pass", "raise", "return", "self", "try",
        "while", "with", "yield",
    ],
    types: &[
        "True", "False", "None", "int", "float", "str", "bool", "bytes", "list", "dict", "set",
        "tuple", "object", "Exception", "ValueError", "TypeError", "KeyError", "IndexError",
    ],
    ci: false,
    markup: false,
    hash_meta: false,
};

const JS: Cfg = Cfg {
    line: &["//"],
    block: Some(("/*", "*/")),
    quotes: &['"', '\''],
    triple: false,
    backtick: true,
    keywords: &[
        "as", "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger",
        "declare", "default", "delete", "do", "else", "enum", "export", "extends", "finally", "for",
        "from", "function", "if", "implements", "import", "in", "instanceof", "interface", "let",
        "new", "of", "package", "private", "protected", "public", "readonly", "return", "static",
        "super", "switch", "this", "throw", "try", "type", "typeof", "var", "void", "while", "with",
        "yield",
    ],
    types: &[
        "Array", "Boolean", "Date", "Error", "JSON", "Map", "Math", "Number", "Object", "Promise",
        "RegExp", "Set", "String", "Symbol", "WeakMap", "any", "bigint", "boolean", "never", "number",
        "string", "unknown",
    ],
    ci: false,
    markup: false,
    hash_meta: false,
};

/// C / C++ / Java / C# / Go / Kotlin / Swift share one table: the union of their keywords.
/// Writing one fewer copy also avoids touching the struct just because "some language lacks a keyword".
const CL: Cfg = Cfg {
    line: &["//"],
    block: Some(("/*", "*/")),
    quotes: &['"', '\''],
    triple: false,
    backtick: false,
    keywords: &[
        "alignas", "alignof", "auto", "break", "case", "catch", "chan", "class", "const", "constexpr",
        "continue", "decltype", "default", "defer", "delete", "do", "double", "else", "enum",
        "explicit", "export", "extern", "final", "float", "for", "func", "go", "goto", "if", "impl",
        "import", "inline", "int", "interface", "let", "long", "mut", "namespace", "new", "noexcept",
        "nullptr", "operator", "override", "package", "private", "protected", "public", "register",
        "return", "short", "signed", "sizeof", "static", "struct", "super", "switch", "template",
        "this", "throw", "try", "typedef", "typename", "typeof", "union", "unsigned", "using", "var",
        "virtual", "void", "volatile", "while",
    ],
    types: &[
        "bool", "char", "double", "float", "int", "long", "short", "size_t", "ssize_t", "string",
        "uint8_t", "uint16_t", "uint32_t", "uint64_t", "int8_t", "int16_t", "int32_t", "int64_t",
        "wchar_t", "BOOL", "DWORD", "HANDLE", "HWND", "LPSTR", "LPWSTR", "UINT", "true", "false",
    ],
    ci: false,
    markup: false,
    hash_meta: true,
};

const SHELL: Cfg = Cfg {
    line: &["#"],
    block: None,
    quotes: &['"', '\''],
    triple: false,
    backtick: true,
    keywords: &[
        "case", "do", "done", "elif", "else", "esac", "exit", "export", "fi", "for", "function", "if",
        "in", "local", "readonly", "return", "select", "set", "shift", "source", "then", "trap",
        "unset", "until", "while",
    ],
    types: &[],
    ci: false,
    markup: false,
    hash_meta: false,
};

const SQL: Cfg = Cfg {
    line: &["--"],
    block: Some(("/*", "*/")),
    quotes: &['\'', '"'],
    triple: false,
    backtick: false,
    keywords: &[
        "ALL", "ALTER", "AND", "AS", "ASC", "BETWEEN", "BY", "CASE", "CREATE", "DEFAULT", "DELETE",
        "DESC", "DISTINCT", "DROP", "ELSE", "END", "EXISTS", "FOREIGN", "FROM", "FULL", "GROUP",
        "HAVING", "IN", "INDEX", "INNER", "INSERT", "INTO", "IS", "JOIN", "KEY", "LEFT", "LIKE",
        "LIMIT", "NOT", "NULL", "OFFSET", "ON", "OR", "ORDER", "OUTER", "PRIMARY", "REFERENCES",
        "RETURNING", "RIGHT", "SELECT", "SET", "TABLE", "THEN", "UNION", "UNIQUE", "UPDATE", "USING",
        "VALUES", "VIEW", "WHEN", "WHERE", "WITH",
    ],
    types: &[
        "BIGINT", "BLOB", "BOOLEAN", "CHAR", "DATE", "DECIMAL", "DOUBLE", "FLOAT", "INT", "INTEGER",
        "JSON", "NUMERIC", "REAL", "SMALLINT", "TEXT", "TIMESTAMP", "UUID", "VARCHAR",
    ],
    ci: true,
    markup: false,
    hash_meta: false,
};

/// JSON / TOML / YAML / INI: no comments; readability rests on strings and literals.
const DATA: Cfg = Cfg {
    line: &["#"],
    block: None,
    quotes: &['"', '\''],
    triple: false,
    backtick: false,
    keywords: &["true", "false", "null", "None", "nil", "yes", "no", "on", "off"],
    types: &["array", "bool", "float", "int", "integer", "map", "number", "object", "string"],
    ci: false,
    markup: false,
    hash_meta: false,
};

const MARKUP: Cfg = Cfg {
    line: &[],
    block: Some(("<!--", "-->")),
    quotes: &['"', '\''],
    triple: false,
    backtick: false,
    keywords: &[],
    types: &[],
    ci: false,
    markup: true,
    hash_meta: false,
};

/// Pick a config from the language hint after the fence; if unrecognized, fall back to "highlight only strings and numbers".
fn detect(hint: &str) -> &'static Cfg {
    let lower = hint.trim().to_ascii_lowercase();
    let h = lower.split(&['=', ':', ',', ' '][..]).next().unwrap_or("");
    match h {
        "rust" | "rs" => &RUST,
        "python" | "py" | "python3" | "py3" => &PYTHON,
        "js" | "javascript" | "jsx" | "mjs" | "cjs" | "ts" | "typescript" | "tsx" => &JS,
        "c" | "h" | "cpp" | "c++" | "cc" | "hpp" | "java" | "cs" | "csharp" | "go" | "golang"
        | "kt" | "kotlin" | "swift" | "scala" | "dart" => &CL,
        "sh" | "bash" | "zsh" | "shell" | "console" | "ps1" | "powershell" | "bat" | "cmd" => &SHELL,
        "sql" | "mysql" | "sqlite" | "postgres" => &SQL,
        "json" | "json5" | "jsonc" | "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "env" => {
            &DATA
        }
        "html" | "xml" | "svg" | "vue" | "svelte" | "xsl" => &MARKUP,
        _ => &TEXT,
    }
}

// Scan

/// Cross-line state: block comments and triple-quoted strings must be remembered into the next line.
#[derive(Clone, Copy, PartialEq)]
enum State {
    Normal,
    Block,
    Triple(char),
}

fn at(b: &[char], i: usize, pat: &str) -> bool {
    let mut k = i;
    for c in pat.chars() {
        if b.get(k) != Some(&c) {
            return false;
        }
        k += 1;
    }
    true
}

fn find_at(b: &[char], from: usize, pat: &str) -> Option<usize> {
    (from..b.len()).find(|&k| at(b, k, pat))
}

fn at3(b: &[char], i: usize, q: char) -> bool {
    b.get(i) == Some(&q) && b.get(i + 1) == Some(&q) && b.get(i + 2) == Some(&q)
}

fn find3(b: &[char], from: usize, q: char) -> Option<usize> {
    (from..b.len()).find(|&k| at3(b, k, q))
}

/// Skip whitespace (for lookahead of the form "what comes next")
fn skip_ws(b: &[char], mut i: usize) -> usize {
    while i < b.len() && b[i].is_whitespace() {
        i += 1;
    }
    i
}

/// Whether the character after (past whitespace) is this one
fn peek(b: &[char], i: usize, c: char) -> bool {
    b.get(skip_ws(b, i)) == Some(&c)
}

fn has(hay: &[&str], w: &str, ci: bool) -> bool {
    hay.iter().any(|k| if ci { k.eq_ignore_ascii_case(w) } else { *k == w })
}

fn classify(cfg: &Cfg, w: &str, b: &[char], start: usize, after: usize) -> Tok {
    if cfg.markup {
        // Tag name of `<div` / `</div`
        if (start > 0 && b[start - 1] == '<') || (start > 1 && b[start - 1] == '/' && b[start - 2] == '<')
        {
            return Tok::Meta;
        }
        // Attribute name like `href=`
        if peek(b, after, '=') {
            return Tok::Type;
        }
    }
    if has(cfg.keywords, w, cfg.ci) {
        return Tok::Keyword;
    }
    if has(cfg.types, w, cfg.ci) {
        return Tok::Type;
    }
    if peek(b, after, '!') {
        return Tok::Meta; // Rust macro
    }
    if peek(b, after, '(') {
        return Tok::Func;
    }
    // Treat PascalCase as a type: this heuristic is far cheaper than maintaining per-language type tables
    if w.chars().count() > 1 && w.starts_with(|c: char| c.is_uppercase()) {
        return Tok::Type;
    }
    Tok::Plain
}

/// Highlight a whole code block, returning line by line (each line is several "text + kind").
pub fn highlight(code: &str, hint: &str) -> Vec<Vec<(String, Tok)>> {
    let cfg = detect(hint);
    let mut state = State::Normal;
    code.lines().map(|l| scan_line(cfg, &mut state, l)).collect()
}

fn scan_line(cfg: &Cfg, state: &mut State, line: &str) -> Vec<(String, Tok)> {
    let b: Vec<char> = line.chars().collect();
    let mut out: Vec<(String, Tok)> = Vec::new();
    let mut plain = String::new();
    let head = skip_ws(&b, 0); // position of the first non-whitespace character on the line
    let mut i = 0usize;

    // Collapse b[start..end] into one token; an empty string does not enter the list (otherwise it would stuff in a pile of empty sections)
    macro_rules! cut {
        ($from:expr, $to:expr, $tok:expr) => {{
            let s: String = b[$from..$to].iter().collect();
            if !s.is_empty() {
                out.push((s, $tok));
            }
        }};
    }
    macro_rules! flush {
        () => {
            if !plain.is_empty() {
                out.push((std::mem::take(&mut plain), Tok::Plain));
            }
        };
    }

    while i < b.len() {
        // Handle cross-line state first
        match *state {
            State::Block => {
                let close = cfg.block.map(|(_, c)| c).unwrap_or("*/");
                match find_at(&b, i, close) {
                    Some(k) => {
                        cut!(i, k + close.chars().count(), Tok::Comment);
                        i = k + close.chars().count();
                        *state = State::Normal;
                    }
                    None => {
                        cut!(i, b.len(), Tok::Comment);
                        i = b.len();
                    }
                }
                continue;
            }
            State::Triple(q) => {
                match find3(&b, i, q) {
                    Some(k) => {
                        cut!(i, k + 3, Tok::Str);
                        i = k + 3;
                        *state = State::Normal;
                    }
                    None => {
                        cut!(i, b.len(), Tok::Str);
                        i = b.len();
                    }
                }
                // This whole line belongs to that triple-quoted string; after cutting we must go back to the `while`...
                continue;
            }
            State::Normal => {}
        }

        // Line comment
        if cfg.line.iter().any(|p| at(&b, i, p)) {
            flush!();
            cut!(i, b.len(), Tok::Comment);
            i = b.len();
            continue;
        }
        // Preprocessor directive / attribute: only recognized at the start of a line
        if cfg.hash_meta && b[i] == '#' && i == head {
            flush!();
            cut!(i, b.len(), Tok::Meta);
            i = b.len();
            continue;
        }
        // Block comment
        if let Some((open, close)) = cfg.block {
            if at(&b, i, open) {
                flush!();
                let start = i;
                i += open.chars().count();
                match find_at(&b, i, close) {
                    Some(k) => {
                        cut!(start, k + close.chars().count(), Tok::Comment);
                        i = k + close.chars().count();
                    }
                    None => {
                        cut!(start, b.len(), Tok::Comment);
                        *state = State::Block;
                        i = b.len();
                    }
                }
                continue;
            }
        }
        // Triple-quoted string
        if cfg.triple && (b[i] == '"' || b[i] == '\'') && at3(&b, i, b[i]) {
            flush!();
            let start = i;
            let q = b[i];
            i += 3;
            match find3(&b, i, q) {
                Some(k) => {
                    cut!(start, k + 3, Tok::Str);
                    i = k + 3;
                }
                None => {
                    cut!(start, b.len(), Tok::Str);
                    *state = State::Triple(q);
                    i = b.len();
                }
            }
            continue;
        }
        // Single-line string
        let c = b[i];
        if cfg.quotes.contains(&c) || (cfg.backtick && c == '`') {
            flush!();
            let start = i;
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == '\\' {
                    j += 2;
                    continue;
                }
                if b[j] == c {
                    j += 1;
                    break;
                }
                j += 1;
            }
            let j = j.min(b.len());
            // A string followed by a colon is colored as a "key name": JSON, TOML, and Python dicts all hit this
            let tok = if peek(&b, j, ':') { Tok::Meta } else { Tok::Str };
            cut!(start, j, tok);
            i = j;
            continue;
        }
        // Shell variable: $VAR / ${VAR} / $(cmd)
        if c == '$' && b.get(i + 1).is_some_and(|n| n.is_alphabetic() || *n == '_' || *n == '{') {
            flush!();
            let start = i;
            let mut j = i + 1;
            let braced = b[j] == '{';
            j += usize::from(braced);
            while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
                j += 1;
            }
            if braced && b.get(j) == Some(&'}') {
                j += 1;
            }
            cut!(start, j, Tok::Meta);
            i = j;
            continue;
        }
        // Number. One glued to a preceding identifier is part of that identifier (`x1`), handled by the identifier branch
        if c.is_ascii_digit() && !(i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == '_')) {
            flush!();
            let start = i;
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == '.' || b[j] == '_') {
                j += 1;
            }
            // Don't swallow a trailing `.` (`foo.0`, ellipsis)
            while j > start + 1 && b[j - 1] == '.' {
                j -= 1;
            }
            cut!(start, j, Tok::Num);
            i = j;
            continue;
        }
        // Identifier (ASCII only: CJK is treated as plain text and never misread as a function)
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == '_') {
                j += 1;
            }
            let word: String = b[start..j].iter().collect();
            match classify(cfg, &word, &b, start, j) {
                Tok::Plain => plain.push_str(&word),
                t => {
                    flush!();
                    out.push((word, t));
                }
            }
            i = j;
            continue;
        }
        plain.push(c);
        i += 1;
    }
    flush!();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str, hint: &str) -> Vec<Tok> {
        highlight(src, hint).into_iter().flatten().map(|(_, t)| t).collect()
    }

    #[test]
    fn rust_basics() {
        let k = kinds("// hi\nfn main() { let x = 1; }", "rust");
        assert_eq!(k[0], Tok::Comment);
        assert!(k.contains(&Tok::Keyword));
        assert!(k.contains(&Tok::Func));
        assert!(k.contains(&Tok::Num));
    }

    #[test]
    fn block_comment_spans_lines() {
        let rows = highlight("/* a\nb */ let x = 1;", "rust");
        assert_eq!(rows[0][0].1, Tok::Comment);
        assert_eq!(rows[1][0].1, Tok::Comment);
        // Only after the comment ends on the second line does the actual code begin
        assert!(rows[1].iter().any(|(_, k)| *k == Tok::Keyword));
    }

    #[test]
    fn python_docstring_spans_lines() {
        let rows = highlight("\"\"\"doc\nstill doc\"\"\"\nx = 1", "python");
        assert!(rows[0].iter().any(|(_, k)| *k == Tok::Str));
        assert!(rows[1].iter().any(|(_, k)| *k == Tok::Str));
        assert!(rows[2].iter().any(|(_, k)| *k == Tok::Num));
    }

    #[test]
    fn rust_lifetime_is_not_a_string() {
        // A lifetime `&'a str` must not swallow the rest of the line
        let rows = highlight("fn f<'a>(x: &'a str) -> &'a str { x }", "rust");
        assert!(rows[0].iter().any(|(_, k)| *k == Tok::Keyword));
        assert!(rows[0].iter().any(|(t, _)| t.contains("'a")));
    }

    #[test]
    fn json_keys_differ_from_values() {
        let k = kinds("{\"name\": \"aox\", \"n\": 1}", "json");
        assert_eq!(k.iter().filter(|t| **t == Tok::Meta).count(), 2); // two keys
        assert_eq!(k.iter().filter(|t| **t == Tok::Str).count(), 1); // one value
    }

    #[test]
    fn sql_is_case_insensitive() {
        assert!(kinds("select * from t", "sql").contains(&Tok::Keyword));
        assert!(kinds("SELECT * FROM t", "sql").contains(&Tok::Keyword));
    }

    #[test]
    fn cjk_never_becomes_a_function() {
        assert!(!kinds("调用(参数)", "rust").contains(&Tok::Func));
    }

    #[test]
    fn unknown_language_still_highlights_strings() {
        let k = kinds("a = \"x\" // not a comment", "");
        assert!(k.contains(&Tok::Str));
        assert!(!k.contains(&Tok::Comment)); // without a language, don't guess comments
    }
}
