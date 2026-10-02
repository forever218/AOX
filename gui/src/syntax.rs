//! 代码块的语法高亮。
//!
//! 刻意**不引正则、也不为每种语言各写一套解析器**：一份通用的字符扫描 +
//! 每语言一张配置表（注释符、字符串定界符、关键字）。对"代码块"这种只要看着
//! 舒服的场景已经够用，而引正则会给这个零重依赖的项目加一个 crate，不划算。
//!
//! 配色是低饱和的暖色系，彼此主要差在明度上 —— 界面的主语仍然是纸与墨，
//! 高亮只负责"扫得过去"，不负责抢戏。

use egui::Color32;

use crate::theme::{self, Mode, P};

// ────────────────────────────── 词法单元 ──────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    Plain,
    Keyword,
    /// 类型名：PascalCase、原始类型、`u32` 这类内建名
    Type,
    /// 紧跟左括号的标识符（函数名与调用点）、标签语言里的属性名
    Func,
    Str,
    Num,
    Comment,
    /// 预处理指令、宏、shell 变量、`<div>` 这样的标签名、以及「键名」形态的字符串
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

/// 纸底：赭红关键字、松绿类型、钢蓝函数、橄榄字符串、梅紫数字、暖灰注释、琥珀元信息。
const LIGHT: Spec = Spec {
    kw: Color32::from_rgb(0xA3, 0x4A, 0x2A),
    ty: Color32::from_rgb(0x1F, 0x7A, 0x6B),
    func: Color32::from_rgb(0x2F, 0x66, 0x90),
    str_: Color32::from_rgb(0x4C, 0x7A, 0x2F),
    num: Color32::from_rgb(0x8A, 0x5A, 0x9E),
    cmt: Color32::from_rgb(0x93, 0x8C, 0x7D),
    meta: Color32::from_rgb(0xB0, 0x7A, 0x1E),
};

/// 墨底：同一组色相整体提亮，注释保持最弱。
const DARK: Spec = Spec {
    kw: Color32::from_rgb(0xE0, 0x8A, 0x6B),
    ty: Color32::from_rgb(0x6F, 0xC7, 0xB4),
    func: Color32::from_rgb(0x86, 0xB6, 0xDD),
    str_: Color32::from_rgb(0xA8, 0xC9, 0x7A),
    num: Color32::from_rgb(0xC9, 0xA0, 0xDC),
    cmt: Color32::from_rgb(0x83, 0x7D, 0x70),
    meta: Color32::from_rgb(0xD8, 0xB2, 0x6A),
};

/// 取某个词法单元在当前明暗模式下的颜色（运行时求值，切主题立即生效）。
pub fn color(t: Tok) -> Color32 {
    match theme::mode() {
        Mode::Light => LIGHT.of(t),
        Mode::Dark => DARK.of(t),
    }
}

// ────────────────────────────── 语言配置 ──────────────────────────────

struct Cfg {
    /// 行注释前缀
    line: &'static [&'static str],
    /// 块注释定界符
    block: Option<(&'static str, &'static str)>,
    /// 单行字符串定界符。Rust 刻意不含 `'` —— 否则生命周期 `&'a str` 会被当成
    /// 字符字面量，把整行后半段都吃掉。
    quotes: &'static [char],
    /// 三引号跨行字符串（Python 文档串）
    triple: bool,
    /// 反引号字符串（JS 模板串、shell 的 `$(...)`）
    backtick: bool,
    /// 关键字
    keywords: &'static [&'static str],
    /// 内建类型
    types: &'static [&'static str],
    /// 关键字是否忽略大小写（SQL）
    ci: bool,
    /// 标签语言：`<tag>` 与属性名单独上色
    markup: bool,
    /// 行首的 `#` 是预处理指令而不是注释
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

/// C / C++ / Java / C# / Go / Kotlin / Swift 共用一张表：关键字取并集，
/// 少写一份，也免得为了"某语言少一个关键字"去动结构。
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

/// JSON / TOML / YAML / INI：没有注释，靠字符串与字面量撑起可读性。
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

/// 按围栏后面的语言提示取配置；认不出来就退回"只高亮字符串与数字"。
fn detect(hint: &str) -> &'static Cfg {
    let lower = hint.trim().to_ascii_lowercase();
    // ```rust,ignore / ```js:line-numbers —— 只取第一段
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

// ────────────────────────────── 扫描 ──────────────────────────────

/// 跨行状态：块注释与三引号字符串要一直记到下一行。
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

/// 跳过空白（用于"后面跟的是什么"这类前瞻）
fn skip_ws(b: &[char], mut i: usize) -> usize {
    while i < b.len() && b[i].is_whitespace() {
        i += 1;
    }
    i
}

/// 后面（越过空白）是不是这个字符
fn peek(b: &[char], i: usize, c: char) -> bool {
    b.get(skip_ws(b, i)) == Some(&c)
}

fn has(hay: &[&str], w: &str, ci: bool) -> bool {
    hay.iter().any(|k| if ci { k.eq_ignore_ascii_case(w) } else { *k == w })
}

fn classify(cfg: &Cfg, w: &str, b: &[char], start: usize, after: usize) -> Tok {
    if cfg.markup {
        // `<div` / `</div` 的标签名
        if (start > 0 && b[start - 1] == '<') || (start > 1 && b[start - 1] == '/' && b[start - 2] == '<')
        {
            return Tok::Meta;
        }
        // `href=` 这种属性名
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
        return Tok::Meta; // Rust 宏
    }
    if peek(b, after, '(') {
        return Tok::Func;
    }
    // PascalCase 当类型：这条启发式比维护各语言类型表省事得多
    if w.chars().count() > 1 && w.starts_with(|c: char| c.is_uppercase()) {
        return Tok::Type;
    }
    Tok::Plain
}

/// 高亮整段代码，按行返回（每行若干个「文本 + 类别」）。
pub fn highlight(code: &str, hint: &str) -> Vec<Vec<(String, Tok)>> {
    let cfg = detect(hint);
    let mut state = State::Normal;
    code.lines().map(|l| scan_line(cfg, &mut state, l)).collect()
}

fn scan_line(cfg: &Cfg, state: &mut State, line: &str) -> Vec<(String, Tok)> {
    let b: Vec<char> = line.chars().collect();
    let mut out: Vec<(String, Tok)> = Vec::new();
    let mut plain = String::new();
    let head = skip_ws(&b, 0); // 行首第一个非空白字符的位置
    let mut i = 0usize;

    // 把 b[start..end] 收成一个 token；空串不进列表（否则会塞进一堆空 section）
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
        // 跨行状态先处理
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
            State::Triple(q) => match find3(&b, i, q) {
                Some(k) => {
                    cut!(i, k + 3, Tok::Str);
                    i = k + 3;
                    *state = State::Normal;
                }
                None => {
                    cut!(i, b.len(), Tok::Str);
                    i = b.len();
                }
            },
            State::Normal => {}
        }

        // 行注释
        if cfg.line.iter().any(|p| at(&b, i, p)) {
            flush!();
            cut!(i, b.len(), Tok::Comment);
            i = b.len();
            continue;
        }
        // 预处理指令 / 属性：只认行首
        if cfg.hash_meta && b[i] == '#' && i == head {
            flush!();
            cut!(i, b.len(), Tok::Meta);
            i = b.len();
            continue;
        }
        // 块注释
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
        // 三引号字符串
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
        // 单行字符串
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
            // 后面跟着冒号的字符串按"键名"上色：JSON、TOML、Python 字典都吃这一条
            let tok = if peek(&b, j, ':') { Tok::Meta } else { Tok::Str };
            cut!(start, j, tok);
            i = j;
            continue;
        }
        // shell 变量：$VAR / ${VAR} / $(cmd)
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
        // 数字。紧跟在标识符后面的是标识符的一部分（`x1`），交给标识符分支
        if c.is_ascii_digit() && !(i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == '_')) {
            flush!();
            let start = i;
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == '.' || b[j] == '_') {
                j += 1;
            }
            // 别把结尾的 `.` 吞掉（`foo.0`、省略号）
            while j > start + 1 && b[j - 1] == '.' {
                j -= 1;
            }
            cut!(start, j, Tok::Num);
            i = j;
            continue;
        }
        // 标识符（只认 ASCII：中文直接当普通文本，不会被误判成函数）
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
        // 第二行注释结束之后才是真正的代码
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
        // 生命周期 `&'a str` 不该把整行后半段吃掉
        let rows = highlight("fn f<'a>(x: &'a str) -> &'a str { x }", "rust");
        assert!(rows[0].iter().any(|(_, k)| *k == Tok::Keyword));
        assert!(rows[0].iter().any(|(t, _)| t.contains("'a")));
    }

    #[test]
    fn json_keys_differ_from_values() {
        let k = kinds("{\"name\": \"aox\", \"n\": 1}", "json");
        assert_eq!(k.iter().filter(|t| **t == Tok::Meta).count(), 2); // 两个键
        assert_eq!(k.iter().filter(|t| **t == Tok::Str).count(), 1); // 一个值
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
        let k = kinds("a = \"x\" // 不是注释", "");
        assert!(k.contains(&Tok::Str));
        assert!(!k.contains(&Tok::Comment)); // 没语言就别乱认注释
    }
}
