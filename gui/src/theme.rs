//! 纸张感黑白主题：配色、留白、圆角，以及中文字体加载。
//!
//! 设计取向是「纸与墨」——没有色相，只有明度层次与留白：
//! 亮色模式是一张略微发暖的纸，暗色模式是暖调的黑。
//! 状态语义（成功/警告/失败）不靠颜色区分，而是靠明度对比与反白块。
//!
//! egui 自带字体不含 CJK 字形，中文会渲染成空白方块。这里从系统字体目录里挑一个
//! 现代无衬线中文字体（优先「等线 Deng」，其次「微软雅黑」）作为回退字体注入。
//! 同时注册一个真正的「粗体」字族，让 Markdown 的 **加粗** 有实际字重变化。

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle, Visuals,
};

// ─────────────────────────── 字号 / 留白刻度 ───────────────────────────

/// 正文字号。全套界面以它为基准。
pub const BODY: f32 = 16.0;
/// 卡片圆角
pub const CARD_RADIUS: u8 = 14;

/// 自定义粗体字族名。
const BOLD_FAMILY: &str = "aox-strong";
/// 自定义衬线字族名（内嵌引用等次级标题用，找不到就退回普通字体）。
const SERIF_FAMILY: &str = "aox-serif";
/// 自定义品牌字族名（首屏那句大招呼用，找不到就退到衬线）。
const BRAND_FAMILY: &str = "aox-brand";
static BOLD_OK: AtomicBool = AtomicBool::new(false);
static SERIF_OK: AtomicBool = AtomicBool::new(false);
static BRAND_OK: AtomicBool = AtomicBool::new(false);

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

/// 衬线 FontId（次级标题用；找不到衬线字体时退回粗体）。
pub fn serif_font(size: f32) -> FontId {
    if SERIF_OK.load(Ordering::Relaxed) {
        FontId::new(size, FontFamily::Name(SERIF_FAMILY.into()))
    } else {
        bold_font(size)
    }
}

/// 品牌字 FontId（首屏那句大招呼用）。
///
/// 比衬线松、带点手写的笔意，免得一行问候看起来像文件抬头；
/// 找不到艺术体时逐级退回衬线 → 粗体，所以调它总是安全的。
pub fn brand_font(size: f32) -> FontId {
    if BRAND_OK.load(Ordering::Relaxed) {
        FontId::new(size, FontFamily::Name(BRAND_FAMILY.into()))
    } else {
        serif_font(size)
    }
}

pub fn body_font(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn mono_font(size: f32) -> FontId {
    FontId::monospace(size)
}

// ─────────────────────────── 明暗模式 ───────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 纸
    Light,
    /// 墨
    Dark,
}

static MODE: AtomicU8 = AtomicU8::new(1); // 默认暗（墨）

pub fn mode() -> Mode {
    if MODE.load(Ordering::Relaxed) == 0 {
        Mode::Light
    } else {
        Mode::Dark
    }
}

pub fn set_mode(m: Mode) {
    MODE.store(if m == Mode::Light { 0 } else { 1 }, Ordering::Relaxed);
}

/// 切换并返回新模式。
pub fn toggle_mode() -> Mode {
    let m = match mode() {
        Mode::Light => Mode::Dark,
        Mode::Dark => Mode::Light,
    };
    set_mode(m);
    m
}

// 主题偏好落盘位置：应用根目录下的一个小文件。
// 找不到可写位置就静默放弃（只是记不住偏好，不影响使用）。
fn pref_path() -> Option<std::path::PathBuf> {
    let (root, _) = aox::paths::resolve_workspace();
    let p = root.join(".aox-gui-theme");
    if p.parent().is_some_and(|d| d.is_dir()) {
        Some(p)
    } else {
        None
    }
}

/// 读取上次的主题偏好（默认暗色）。
pub fn load_pref() {
    if let Some(p) = pref_path() {
        if let Ok(s) = std::fs::read_to_string(p) {
            set_mode(match s.trim() {
                "light" => Mode::Light,
                _ => Mode::Dark,
            });
        }
    }
}

fn save_pref() {
    if let Some(p) = pref_path() {
        let _ = std::fs::write(p, if mode() == Mode::Light { "light" } else { "dark" });
    }
}

// ─────────────────────────── 调色板 ───────────────────────────

/// 一套完整配色。字段刻意只用明度区分，不带色相。
#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    panel: Color32,
    surface: Color32,
    surface_hi: Color32,
    border: Color32,
    border_strong: Color32,
    text: Color32,
    text_dim: Color32,
    text_faint: Color32,
    /// 「墨」——单色方案里的强调色：亮色模式下几乎全黑，暗色模式下几乎全白
    ink: Color32,
    /// 墨的弱化版
    ink_soft: Color32,
    user_bg: Color32,
    code_bg: Color32,
    quote_bar: Color32,
    /// 反白块的底色（错误提示用：亮色=墨黑底，暗色=近白底）
    reverse_bg: Color32,
    /// 反白块上的文字
    on_reverse: Color32,
    /// 悬浮卡片的投影色。亮色下一层淡墨就够；暗色下阴影几乎看不见，只能加重补偿。
    shadow: Color32,
}

/// 纸：略微发暖的白，不是纯白。
const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xF2, 0xEF, 0xE8),
    panel: Color32::from_rgb(0xEB, 0xE7, 0xDE),
    surface: Color32::from_rgb(0xFA, 0xF8, 0xF3),
    surface_hi: Color32::from_rgb(0xE4, 0xE0, 0xD5),
    border: Color32::from_rgb(0xDD, 0xD7, 0xC9),
    border_strong: Color32::from_rgb(0xC2, 0xBA, 0xA8),
    text: Color32::from_rgb(0x1A, 0x19, 0x16),
    text_dim: Color32::from_rgb(0x64, 0x5F, 0x55),
    text_faint: Color32::from_rgb(0x9B, 0x94, 0x86),
    ink: Color32::from_rgb(0x1A, 0x19, 0x16),
    ink_soft: Color32::from_rgb(0x6B, 0x65, 0x5A),
    user_bg: Color32::from_rgb(0xE7, 0xE2, 0xD6),
    code_bg: Color32::from_rgb(0xEC, 0xE8, 0xDE),
    quote_bar: Color32::from_rgb(0xC2, 0xBA, 0xA8),
    reverse_bg: Color32::from_rgb(0x1A, 0x19, 0x16),
    on_reverse: Color32::from_rgb(0xF7, 0xF5, 0xF0),
    shadow: Color32::from_rgba_unmultiplied_const(0x1A, 0x19, 0x16, 26),
};

/// 墨：暖调的黑，不是纯黑。
const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x14, 0x14, 0x12),
    panel: Color32::from_rgb(0x19, 0x19, 0x17),
    surface: Color32::from_rgb(0x20, 0x20, 0x1D),
    surface_hi: Color32::from_rgb(0x2A, 0x2A, 0x26),
    border: Color32::from_rgb(0x33, 0x33, 0x2E),
    border_strong: Color32::from_rgb(0x4C, 0x4C, 0x45),
    text: Color32::from_rgb(0xEA, 0xE7, 0xDF),
    text_dim: Color32::from_rgb(0xA3, 0x9F, 0x96),
    text_faint: Color32::from_rgb(0x71, 0x6E, 0x66),
    ink: Color32::from_rgb(0xEC, 0xE9, 0xE1),
    ink_soft: Color32::from_rgb(0x9A, 0x96, 0x8D),
    user_bg: Color32::from_rgb(0x25, 0x25, 0x21),
    code_bg: Color32::from_rgb(0x10, 0x10, 0x0E),
    quote_bar: Color32::from_rgb(0x4C, 0x4C, 0x45),
    reverse_bg: Color32::from_rgb(0xEC, 0xE9, 0xE1),
    on_reverse: Color32::from_rgb(0x14, 0x14, 0x12),
    shadow: Color32::from_rgba_unmultiplied_const(0, 0, 0, 96),
};

fn pal() -> &'static Palette {
    match mode() {
        Mode::Light => &LIGHT,
        Mode::Dark => &DARK,
    }
}

/// 当前主题下的取色器。运行时求值，所以切换主题后立即生效。
pub struct P;

impl P {
    pub fn bg() -> Color32 {
        pal().bg
    }
    pub fn panel() -> Color32 {
        pal().panel
    }
    pub fn surface() -> Color32 {
        pal().surface
    }
    pub fn surface_hi() -> Color32 {
        pal().surface_hi
    }
    pub fn border() -> Color32 {
        pal().border
    }
    pub fn border_strong() -> Color32 {
        pal().border_strong
    }
    pub fn text() -> Color32 {
        pal().text
    }
    pub fn text_dim() -> Color32 {
        pal().text_dim
    }
    pub fn text_faint() -> Color32 {
        pal().text_faint
    }
    /// 单色强调色（原 ACCENT）
    pub fn ink() -> Color32 {
        pal().ink
    }
    /// 强调色的弱化版（原 ACCENT_DIM）
    pub fn ink_soft() -> Color32 {
        pal().ink_soft
    }
    pub fn user_bg() -> Color32 {
        pal().user_bg
    }
    pub fn code_bg() -> Color32 {
        pal().code_bg
    }
    pub fn quote_bar() -> Color32 {
        pal().quote_bar
    }

    // ── 语义色：单色方案里用明度层次表达，而不是色相 ──

    /// 警告：中间灰（单色方案下靠明度提示）
    pub fn warn() -> Color32 {
        pal().text_dim
    }
    /// 反白块底色
    pub fn reverse_bg() -> Color32 {
        pal().reverse_bg
    }
    /// 反白块上的文字
    pub fn on_reverse() -> Color32 {
        pal().on_reverse
    }
    /// 悬浮卡片的投影色
    pub fn shadow() -> Color32 {
        pal().shadow
    }
}

// ─────────────────────────── 字体加载 ───────────────────────────

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

/// 衬线候选（次级标题用）。
const SERIF_CANDIDATES: &[&str] = &[
    "georgia.ttf",
    "times.ttf",
    "constan.ttf",
    "cambria.ttc",
    "georgiab.ttf",
    "Times New Roman.ttf",
];

/// 品牌字候选（首屏大招呼用）。
///
/// 按「笔势从强到弱」排，全部是 Windows 自带字体（Gabriola / Inkfree 从 Win10 起
/// 随系统提供）；一个都没有就退到衬线，界面不会因此缺字。
const BRAND_CANDIDATES: &[&str] = &[
    "Gabriola.ttf",  // 花体，笔画带书法连带，笔势最足
    "Inkfree.ttf",   // 钢笔手写，细而松弛
    "Candara.ttf",   // 人文无衬线，笔尾微翘
    "georgia.ttf",
];

/// 等宽字体候选（代码块）。
const MONO_CANDIDATES: &[&str] = &[
    "CascadiaMono.ttf",
    "CascadiaCode.ttf",
    "consola.ttf",
    "lucon.ttf",
    "SourceCodePro-Regular.otf",
];

/// 从环境变量读取自定义字体路径（AOX_GUI_FONT / AOX_GUI_FONT_BOLD /
/// AOX_GUI_FONT_SERIF / AOX_GUI_FONT_BRAND / AOX_GUI_FONT_MONO）。
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

    // 衬线：次级标题用。找不到就退回粗体（serif_font 内部处理）。
    let serif = read_env("AOX_GUI_FONT_SERIF").or_else(|| read_first(SERIF_CANDIDATES));
    if let Some((_, bytes)) = serif {
        fonts.font_data.insert("serif".to_owned(), Arc::new(FontData::from_owned(bytes)));
        let mut fam: Vec<String> = vec!["serif".to_owned()];
        if !cjk_name.is_empty() {
            fam.push(cjk_name.clone());
        }
        fam.push("Ubuntu-Light".to_owned());
        fonts.families.insert(FontFamily::Name(SERIF_FAMILY.into()), fam);
        SERIF_OK.store(true, Ordering::Relaxed);
    }

    // 品牌字：首屏大招呼用。找不到就退回衬线（brand_font 内部处理）。
    let brand = read_env("AOX_GUI_FONT_BRAND").or_else(|| read_first(BRAND_CANDIDATES));
    if let Some((_, bytes)) = brand {
        fonts.font_data.insert("brand".to_owned(), Arc::new(FontData::from_owned(bytes)));
        let mut fam: Vec<String> = vec!["brand".to_owned()];
        if !cjk_name.is_empty() {
            fam.push(cjk_name.clone());
        }
        fam.push("Ubuntu-Light".to_owned());
        fonts.families.insert(FontFamily::Name(BRAND_FAMILY.into()), fam);
        BRAND_OK.store(true, Ordering::Relaxed);
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

// ─────────────────────────── 样式 ───────────────────────────

fn widget(v: &mut Visuals, f: impl Fn(&mut egui::style::WidgetVisuals)) {
    f(&mut v.widgets.noninteractive);
    f(&mut v.widgets.inactive);
    f(&mut v.widgets.hovered);
    f(&mut v.widgets.active);
    f(&mut v.widgets.open);
}

/// 让整个应用呈现出「无边线噪声、低对比、只用明度说话」的观感。
/// 切换主题后重新调用本函数即可（字体不必重装）。
pub fn install_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    let mut v = match mode() {
        Mode::Light => Visuals::light(),
        Mode::Dark => Visuals::dark(),
    };
    v.panel_fill = P::bg();
    v.window_fill = P::surface();
    v.extreme_bg_color = P::code_bg();
    v.faint_bg_color = P::surface();
    v.code_bg_color = P::code_bg();
    v.window_stroke = Stroke::new(1.0, P::border());
    v.window_corner_radius = CornerRadius::same(CARD_RADIUS);
    v.hyperlink_color = P::ink();
    v.warn_fg_color = P::warn();
    v.error_fg_color = P::text();
    v.override_text_color = Some(P::text());
    v.striped = false;
    v.selection.bg_fill = P::ink().gamma_multiply(0.20);
    v.selection.stroke = Stroke::new(1.0, P::ink_soft());

    let radius = CornerRadius::same(10);
    widget(&mut v, |w| {
        w.corner_radius = radius;
        w.bg_stroke = Stroke::new(1.0, P::border());
        w.expansion = 0.0;
    });
    v.widgets.noninteractive.bg_fill = P::bg();
    v.widgets.noninteractive.weak_bg_fill = P::bg();
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, P::border());
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, P::text());

    v.widgets.inactive.bg_fill = P::surface();
    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::TRANSPARENT);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, P::text_dim());

    v.widgets.hovered.bg_fill = P::surface_hi();
    v.widgets.hovered.weak_bg_fill = P::surface_hi();
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, P::border_strong());
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, P::text());

    v.widgets.active.bg_fill = P::surface_hi();
    v.widgets.active.weak_bg_fill = P::surface_hi();
    v.widgets.active.bg_stroke = Stroke::new(1.0, P::ink_soft());
    v.widgets.active.fg_stroke = Stroke::new(1.0, P::text());

    v.widgets.open.bg_fill = P::surface();
    v.widgets.open.weak_bg_fill = P::surface();
    v.widgets.open.fg_stroke = Stroke::new(1.0, P::text());

    style.visuals = v;

    // 关掉「标签里的文字可选」。
    //
    // 这个开关默认是开的，代价是**每个 ui.label 都会去抢鼠标**：悬停时光标变成
    // 文本选择的 I 形，按下去还能拖出一段选区。界面里凡是用 label 拼出来的按钮
    // （「清空」「关闭」「允许一次」以及各条示例），鼠标一放上去就顶着输入光标，
    // 看着像点错了地方。界面里没有需要复制的段落文本，直接全局关掉最干净。
    style.interaction.selectable_labels = false;

    // 留白整体放大一档，是「卡片变大、不再拥挤」的主要来源
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 9.0);
    style.spacing.interact_size.y = 30.0;
    style.spacing.indent = 22.0;
    style.spacing.scroll = egui::style::ScrollStyle::thin();
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.bar_inner_margin = 3.0;
    style.spacing.scroll.bar_outer_margin = 4.0;
    style.spacing.scroll.floating = true;

    style.text_styles.insert(TextStyle::Body, body_font(BODY));
    style.text_styles.insert(TextStyle::Button, body_font(15.0));
    style.text_styles.insert(TextStyle::Small, body_font(13.0));
    style.text_styles.insert(TextStyle::Monospace, mono_font(14.5));
    style.text_styles.insert(TextStyle::Heading, bold_font(21.0));

    ctx.set_style(style);
}

/// 一次性装配。
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    install_style(ctx);
    sync_native_theme(ctx);
}

/// 切换主题：重算样式并记住偏好。
pub fn apply_mode(ctx: &egui::Context, m: Mode) {
    set_mode(m);
    install_style(ctx);
    sync_native_theme(ctx);
    save_pref();
}

/// 让操作系统的窗口装饰（标题栏、边框）跟随应用主题。
///
/// 原生标题栏不是 egui 画的，而是窗口管理器（Windows 上是 DWM）画的，默认跟随
/// 系统主题。不同步的话，亮色的纸白界面会顶着一条深色标题栏，很割裂。
pub fn sync_native_theme(ctx: &egui::Context) {
    let t = match mode() {
        Mode::Light => egui::SystemTheme::Light,
        Mode::Dark => egui::SystemTheme::Dark,
    };
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(t));
}

/// 把原生标题栏染成与界面**完全一致**的颜色，让它彻底融入窗口。
///
/// `sync_native_theme` 只能让标题栏在「系统亮色 / 系统暗色」里二选一，拿到的是
/// DWM 自己那套灰（亮色约 `#F3F3F3`、暗色约 `#202020`），和纸与墨的暖调对不上。
/// 想真正严丝合缝，只能直接设 DWM 属性：
///
/// - `DWMWA_CAPTION_COLOR`(35)：标题栏底色
/// - `DWMWA_TEXT_COLOR`(36)：标题栏文字色
/// - `DWMWA_BORDER_COLOR`(34)：窗口外框线
///
/// 这三个是 Windows 11 (build 22000+) 才有的，旧系统上调用返回失败——静默忽略即可，
/// 效果只是退回系统默认色，不会出错。
#[cfg(windows)]
pub fn sync_caption(handle: &dyn raw_window_handle::HasWindowHandle) {
    use raw_window_handle::RawWindowHandle;

    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: isize,
            attr: u32,
            value: *const core::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    const DWMWA_BORDER_COLOR: u32 = 34;
    const DWMWA_CAPTION_COLOR: u32 = 35;
    const DWMWA_TEXT_COLOR: u32 = 36;

    let Ok(h) = handle.window_handle() else { return };
    let RawWindowHandle::Win32(w) = h.as_raw() else {
        return;
    };
    let hwnd = w.hwnd.get();

    // 注意 DWM 要的是 COLORREF（0x00BBGGRR），和 Color32 的 RGBA 排列不同，得换序
    let colorref = |c: Color32| -> u32 {
        ((c.b() as u32) << 16) | ((c.g() as u32) << 8) | c.r() as u32
    };

    let cap = colorref(P::panel());
    let text = colorref(P::text_dim());
    let border = colorref(P::border());
    // 失败也无所谓（老系统不支持），所以返回值直接丢掉
    unsafe {
        DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, std::ptr::addr_of!(cap).cast(), 4);
        DwmSetWindowAttribute(hwnd, DWMWA_TEXT_COLOR, std::ptr::addr_of!(text).cast(), 4);
        DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, std::ptr::addr_of!(border).cast(), 4);
    }
}

#[cfg(not(windows))]
pub fn sync_caption(_handle: &dyn raw_window_handle::HasWindowHandle) {}
