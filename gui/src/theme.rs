//! 深色极简主题：配色、留白、圆角，以及中文字体加载。
//!
//! egui 自带字体不含 CJK 字形，中文会渲染成空白方块。这里从系统字体目录里挑一个
//! 现代无衬线中文字体（优先「等线 Deng」，其次「微软雅黑」）作为回退字体注入。
//! 同时注册一个真正的「粗体」字族，让 Markdown 的 **加粗** 有实际字重变化。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle, Visuals,
};

/// 自定义粗体字族名。
const BOLD_FAMILY: &str = "aox-strong";
static BOLD_OK: AtomicBool = AtomicBool::new(false);

/// 系统里是否找到了真正的粗体字体。
pub fn bold_available() -> bool {
    BOLD_OK.load(Ordering::Relaxed)
}

/// 粗体 FontId（找不到粗体时退回普通字体 + 提亮颜色）。
pub fn bold_font(size: f32) -> FontId {
    if bold_available() {
        FontId::new(size, FontFamily::Name(BOLD_FAMILY.into()))
    } else {
        FontId::proportional(size)
    }
}

pub fn body_font(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn mono_font(size: f32) -> FontId {
    FontId::monospace(size)
}

/// 统一配色（深色）。
pub struct P;

impl P {
    /// 主背景
    pub const BG: Color32 = Color32::from_rgb(0x0D, 0x0F, 0x13);
    /// 顶栏 / 侧栏背景
    pub const PANEL: Color32 = Color32::from_rgb(0x11, 0x14, 0x19);
    /// 卡片 / 抬升面
    pub const SURFACE: Color32 = Color32::from_rgb(0x16, 0x1A, 0x21);
    /// 悬停态
    pub const SURFACE_HI: Color32 = Color32::from_rgb(0x1C, 0x21, 0x2A);
    /// 分隔线
    pub const BORDER: Color32 = Color32::from_rgb(0x22, 0x27, 0x31);
    /// 正文
    pub const TEXT: Color32 = Color32::from_rgb(0xE2, 0xE6, 0xED);
    /// 次要文字
    pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8A, 0x93, 0xA5);
    /// 更弱的说明文字
    pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x56, 0x5D, 0x6C);
    /// 主题色（青）
    pub const ACCENT: Color32 = Color32::from_rgb(0x4F, 0xC3, 0xE0);
    /// 主题色的暗调（描边用）
    pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x2B, 0x63, 0x74);
    /// 成功
    pub const OK: Color32 = Color32::from_rgb(0x5A, 0xD1, 0x8A);
    /// 警告
    pub const WARN: Color32 = Color32::from_rgb(0xE8, 0xBB, 0x62);
    /// 失败
    pub const ERR: Color32 = Color32::from_rgb(0xF2, 0x7A, 0x7A);
    /// 用户气泡
    pub const USER_BG: Color32 = Color32::from_rgb(0x16, 0x27, 0x2E);
    /// 代码块背景
    pub const CODE_BG: Color32 = Color32::from_rgb(0x0A, 0x0C, 0x0F);
    /// 引用块左侧竖线
    pub const QUOTE_BAR: Color32 = Color32::from_rgb(0x2F, 0x36, 0x42);
}

fn font_dirs() -> Vec<String> {
    let mut v = Vec::new();
    if cfg!(windows) {
        let win = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
        v.push(format!("{win}\\Fonts"));
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            v.push(format!("{local}\\Microsoft\\Windows\\Fonts"));
        }
    }
    v.push("/System/Library/Fonts".into());
    v.push("/Library/Fonts".into());
    v.push("/usr/share/fonts".into());
    v
}

/// 依次尝试候选文件名，返回第一个成功读到的。
fn read_first(cands: &[&str]) -> Option<(String, Vec<u8>)> {
    for dir in font_dirs() {
        for name in cands {
            let path = format!("{dir}/{name}");
            if let Ok(bytes) = std::fs::read(&path) {
                if !bytes.is_empty() {
                    return Some((path, bytes));
                }
            }
        }
    }
    None
}

/// 中文字体候选（按「现代感」排序）。
const CJK_CANDIDATES: &[&str] = &[
    "Deng.ttf",              // 等线（Win10+，干净现代）
    "msyh.ttc",              // 微软雅黑
    "msyhl.ttc",
    "simhei.ttf",            // 黑体
    "simsun.ttc",            // 宋体
    "msjh.ttc",              // 微软正黑（繁体）
    "PingFang.ttc",          // macOS
    "Hiragino Sans GB.ttc",
    "NotoSansCJK-Regular.ttc",
    "NotoSansSC-Regular.otf",
];

/// 粗体候选。
const BOLD_CANDIDATES: &[&str] = &[
    "Dengb.ttf",
    "msyhbd.ttc",
    "simhei.ttf",
    "msjhbd.ttc",
    "PingFang.ttc",
    "NotoSansCJK-Bold.ttc",
    "NotoSansSC-Bold.otf",
];

/// 等宽字体候选（代码块）。
const MONO_CANDIDATES: &[&str] = &[
    "CascadiaMono.ttf",
    "CascadiaCode.ttf",
    "consola.ttf",
    "lucon.ttf",
    "SourceCodePro-Regular.otf",
];

/// 从环境变量读取自定义字体路径（AOX_GUI_FONT / AOX_GUI_FONT_BOLD / AOX_GUI_FONT_MONO）。
fn read_env(key: &str) -> Option<(String, Vec<u8>)> {
    let path = std::env::var(key).ok()?;
    let bytes = std::fs::read(&path).ok()?;
    if bytes.is_empty() {
        None
    } else {
        Some((path, bytes))
    }
}

/// 装配字体：内建拉丁字体 + 系统中文回退（排在 emoji 字体之前，避免 CJK 落到 emoji 字形上）。
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    let cjk = read_env("AOX_GUI_FONT").or_else(|| read_first(CJK_CANDIDATES));
    let cjk_name = if let Some((_, bytes)) = cjk {
        fonts.font_data.insert("cjk".to_owned(), Arc::new(FontData::from_owned(bytes)));
        "cjk".to_owned()
    } else {
        String::new()
    };

    let bold = read_env("AOX_GUI_FONT_BOLD").or_else(|| read_first(BOLD_CANDIDATES));
    if let Some((_, bytes)) = bold {
        fonts.font_data.insert("cjk-bold".to_owned(), Arc::new(FontData::from_owned(bytes)));
        let mut fam: Vec<String> = vec!["cjk-bold".to_owned()];
        if !cjk_name.is_empty() {
            fam.push(cjk_name.clone());
        }
        fam.push("Hack".to_owned());
        fonts.families.insert(FontFamily::Name(BOLD_FAMILY.into()), fam);
        BOLD_OK.store(true, Ordering::Relaxed);
    }

    let mono = read_env("AOX_GUI_FONT_MONO").or_else(|| read_first(MONO_CANDIDATES));
    let mono_name = if let Some((_, bytes)) = mono {
        fonts.font_data.insert("mono".to_owned(), Arc::new(FontData::from_owned(bytes)));
        Some("mono".to_owned())
    } else {
        None
    };

    // 显式重排字族优先级：拉丁 → 中文 → emoji
    let mut prop: Vec<String> = vec!["Ubuntu-Light".to_owned()];
    if !cjk_name.is_empty() {
        prop.push(cjk_name.clone());
    }
    prop.push("NotoEmoji-Regular".to_owned());
    prop.push("emoji-icon-font".to_owned());
    fonts.families.insert(FontFamily::Proportional, prop);

    let mut monospace: Vec<String> = Vec::new();
    if let Some(m) = mono_name {
        monospace.push(m);
    }
    monospace.push("Hack".to_owned());
    if !cjk_name.is_empty() {
        monospace.push(cjk_name);
    }
    monospace.push("Ubuntu-Light".to_owned());
    monospace.push("NotoEmoji-Regular".to_owned());
    monospace.push("emoji-icon-font".to_owned());
    fonts.families.insert(FontFamily::Monospace, monospace);

    ctx.set_fonts(fonts);
}

fn widget(
    v: &mut Visuals,
    f: impl Fn(&mut egui::style::WidgetVisuals),
) {
    f(&mut v.widgets.noninteractive);
    f(&mut v.widgets.inactive);
    f(&mut v.widgets.hovered);
    f(&mut v.widgets.active);
    f(&mut v.widgets.open);
}

/// 让整个应用呈现出「无边框、低对比、单一强调色」的观感。
pub fn install_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    let mut v = Visuals::dark();
    v.panel_fill = P::BG;
    v.window_fill = P::SURFACE;
    v.extreme_bg_color = P::CODE_BG;
    v.faint_bg_color = P::SURFACE;
    v.code_bg_color = P::CODE_BG;
    v.window_stroke = Stroke::new(1.0, P::BORDER);
    v.window_corner_radius = CornerRadius::same(12);
    v.hyperlink_color = P::ACCENT;
    v.warn_fg_color = P::WARN;
    v.error_fg_color = P::ERR;
    v.override_text_color = Some(P::TEXT);
    v.striped = false;
    v.selection.bg_fill = P::ACCENT.gamma_multiply(0.30);
    v.selection.stroke = Stroke::new(1.0, P::ACCENT);

    let radius = CornerRadius::same(8);
    widget(&mut v, |w| {
        w.corner_radius = radius;
        w.bg_stroke = Stroke::new(1.0, P::BORDER);
        w.expansion = 0.0;
    });
    v.widgets.noninteractive.bg_fill = P::BG;
    v.widgets.noninteractive.weak_bg_fill = P::BG;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, P::BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, P::TEXT);

    v.widgets.inactive.bg_fill = P::SURFACE;
    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::TRANSPARENT);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, P::TEXT_DIM);

    v.widgets.hovered.bg_fill = P::SURFACE_HI;
    v.widgets.hovered.weak_bg_fill = P::SURFACE_HI;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, P::BORDER);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, P::TEXT);

    v.widgets.active.bg_fill = P::SURFACE_HI;
    v.widgets.active.weak_bg_fill = P::SURFACE_HI;
    v.widgets.active.bg_stroke = Stroke::new(1.0, P::ACCENT_DIM);
    v.widgets.active.fg_stroke = Stroke::new(1.0, P::TEXT);

    v.widgets.open.bg_fill = P::SURFACE;
    v.widgets.open.weak_bg_fill = P::SURFACE;
    v.widgets.open.fg_stroke = Stroke::new(1.0, P::TEXT);

    style.visuals = v;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size.y = 24.0;
    style.spacing.indent = 16.0;
    style.spacing.scroll = egui::style::ScrollStyle::thin();
    style.spacing.scroll.bar_width = 6.0;
    style.spacing.scroll.bar_inner_margin = 2.0;
    style.spacing.scroll.bar_outer_margin = 2.0;
    style.spacing.scroll.floating = true;

    style.text_styles.insert(TextStyle::Body, body_font(14.0));
    style.text_styles.insert(TextStyle::Button, body_font(13.0));
    style.text_styles.insert(TextStyle::Small, body_font(11.5));
    style.text_styles.insert(TextStyle::Monospace, mono_font(12.5));
    style.text_styles.insert(TextStyle::Heading, bold_font(17.0));

    ctx.set_style(style);
}

/// 一次性装配。
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    install_style(ctx);
}
