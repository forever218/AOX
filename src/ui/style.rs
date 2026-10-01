//! 终端排版：ANSI 配色 + Markdown 渲染。
//! 非 TTY（重定向到文件）或设置了 NO_COLOR 时自动降级为纯文本。

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn init() {
    // FORCE_COLOR=1 可强制开启（用于重定向到文件后查看效果）
    let forced = std::env::var_os("FORCE_COLOR").is_some();
    let ok = (std::io::stdout().is_terminal() || forced) && std::env::var_os("NO_COLOR").is_none();
    ENABLED.store(ok, Ordering::Relaxed);
    #[cfg(windows)]
    enable_windows_vt();
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 用 ANSI 参数包装文本（spec 例如 "1;36"）
pub fn paint(s: &str, spec: &str) -> String {
    if enabled() {
        format!("\x1b[{spec}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn dim(s: &str) -> String {
    paint(s, "2")
}
pub fn bold(s: &str) -> String {
    paint(s, "1")
}
pub fn cyan(s: &str) -> String {
    paint(s, "36")
}
pub fn bold_cyan(s: &str) -> String {
    paint(s, "1;36")
}
pub fn yellow(s: &str) -> String {
    paint(s, "33")
}
pub fn green(s: &str) -> String {
    paint(s, "32")
}
pub fn red(s: &str) -> String {
    paint(s, "1;31")
}
pub fn magenta(s: &str) -> String {
    paint(s, "35")
}
pub fn bold_white(s: &str) -> String {
    paint(s, "1;97")
}

/// AOX 启动 logo（ANSI Shadow 风格）
pub const LOGO: &str = r" █████╗  ██████╗ ██╗  ██╗
██╔══██╗██╔═══██╗╚██╗██╔╝
███████║██║   ██║ ╚███╔╝
██╔══██║██║   ██║ ██╔██╗
██║  ██║╚██████╔╝██╔╝ ██╗
╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═╝";

pub fn logo() -> String {
    if !enabled() {
        return LOGO.to_string();
    }
    LOGO.lines().map(|l| paint(l, "1;36")).collect::<Vec<_>>().join("\n")
}

/// 渲染 Markdown（标题、代码块、行内代码、列表、引用、表格）
pub fn render_markdown(md: &str) -> String {
    if !enabled() {
        return md.trim_end().to_string();
    }
    use termimad::crossterm::style::{Attribute, Color};
    use termimad::{Alignment, MadSkin};

    let mut skin = MadSkin::default_dark();
    // 标题默认居中，命令行里左对齐更清爽
    for h in skin.headers.iter_mut() {
        h.align = Alignment::Left;
    }
    skin.set_headers_fg(Color::AnsiValue(45)); // 标题：青
    skin.bold.set_fg(Color::AnsiValue(255)); // 加粗：亮白
    skin.bold.add_attr(Attribute::Bold);
    skin.italic.add_attr(Attribute::Italic);
    skin.inline_code.set_fgbg(Color::AnsiValue(114), Color::AnsiValue(236));
    skin.code_block.set_fgbg(Color::AnsiValue(252), Color::AnsiValue(236));
    skin.quote_mark.set_fg(Color::AnsiValue(244));
    skin.bullet.set_fg(Color::AnsiValue(45));
    skin.table.set_fg(Color::AnsiValue(250));

    skin.term_text(md.trim_end()).to_string()
}

#[cfg(windows)]
fn enable_windows_vt() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, SetConsoleOutputCP,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };
    unsafe {
        // 输出改为 UTF-8，避免中文/制表符乱码
        SetConsoleOutputCP(65001);
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) != 0 {
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}
