//! Terminal formatting: ANSI colors + Markdown rendering.
//! Automatically degrades to plain text when not a TTY (redirected to a file) or NO_COLOR is set.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn init() {
    // FORCE_COLOR=1 forces it on (to preview the effect after redirecting to a file)
    let forced = std::env::var_os("FORCE_COLOR").is_some();
    let ok = (std::io::stdout().is_terminal() || forced) && std::env::var_os("NO_COLOR").is_none();
    ENABLED.store(ok, Ordering::Relaxed);
    #[cfg(windows)]
    enable_windows_vt();
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Wrap text with ANSI parameters (spec e.g. "1;36")
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

/// Project homepage (shown in the startup banner; clickable when the terminal supports it)
pub const PROJECT_URL: &str = "https://github.com/forever218/AOX";

/// Whether the current terminal supports OSC 8 hyperlinks.
fn osc8_supported() -> bool {
    if !enabled() {
        return false; // redirected to a file / NO_COLOR set
    }
    // Explicit switch: AOX_HYPERLINK=0 disables, =1 forces on
    match std::env::var("AOX_HYPERLINK").ok().as_deref() {
        Some("0") | Some("false") => return false,
        Some("1") | Some("true") => return true,
        _ => {}
    }
    #[cfg(windows)]
    {
        // Windows Terminal injects WT_SESSION; VS Code / WezTerm etc. inject TERM_PROGRAM. Neither present = legacy...
        std::env::var_os("WT_SESSION").is_some() || std::env::var_os("TERM_PROGRAM").is_some()
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("TERM").is_some()
            || std::env::var_os("TERM_PROGRAM").is_some()
            || std::env::var_os("VTE_VERSION").is_some()
    }
}

/// OSC 8 hyperlink: clickable when the terminal supports it, otherwise returns the plain text unchanged.
pub fn link(text: &str, url: &str) -> String {
    if osc8_supported() {
        // ST uses ESC\ rather than BEL, for better compatibility
        format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
    } else {
        text.to_string()
    }
}

/// AOX startup logo (ANSI Shadow style)
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

/// Render Markdown (headings, code blocks, inline code, lists, quotes, tables)
pub fn render_markdown(md: &str) -> String {
    if !enabled() {
        return md.trim_end().to_string();
    }
    use termimad::crossterm::style::{Attribute, Color};
    use termimad::{Alignment, MadSkin};

    let mut skin = MadSkin::default_dark();
    // Headings are centered by default; left-aligned reads cleaner in the terminal
    for h in skin.headers.iter_mut() {
        h.align = Alignment::Left;
    }
    skin.set_headers_fg(Color::AnsiValue(45)); // heading: cyan
    skin.bold.set_fg(Color::AnsiValue(255)); // bold: bright white
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
        // Switch output to UTF-8, to avoid mojibake in CJK text / tabs
        SetConsoleOutputCP(65001);
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) != 0 {
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}
