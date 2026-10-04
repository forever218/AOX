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
use std::sync::{Arc, OnceLock};

use aox::config::{FontsConfig, MetricsConfig, PaletteConfig, RgbColor, ThemeConfig};
use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle, Visuals,
};

// ─────────────────────────── 字号 / 留白刻度 ───────────────────────────
//
// 这些值现在来自 `[theme.metrics]`（默认即原先写死的常量），所以只能是**运行期访问器**
// 而不是 `const`。`metrics()` 走 `OnceLock`：装配时写一次，之后只读，开销等同常量。

static METRICS: OnceLock<MetricsConfig> = OnceLock::new();

fn metrics() -> &'static MetricsConfig {
    METRICS.get_or_init(MetricsConfig::default)
}

/// 正文字号。全套界面以它为基准。
pub fn body_size() -> f32 {
    metrics().body_size
}

/// 标题（Heading）字号。
pub fn heading_size() -> f32 {
    metrics().heading_size
}

/// 按钮文字字号。
pub fn button_size() -> f32 {
    metrics().button_size
}

/// 小字字号。
pub fn small_size() -> f32 {
    metrics().small_size
}

/// 等宽字号（代码块用）。
pub fn mono_size() -> f32 {
    metrics().mono_size
}

/// 卡片圆角。
pub fn card_radius() -> u8 {
    metrics().card_radius
}

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
    let (root, _) = aox::paths::resolve_workplace();
    let p = root.join(".aox-gui-theme");
    if p.parent().is_some_and(|d| d.is_dir()) {
        Some(p)
    } else {
        None
    }
}

/// 读取上次的主题偏好。`None` = 从没记录过（或记录不可辨认），交由上层决定用哪个。
fn read_pref() -> Option<Mode> {
    let p = pref_path()?;
    parse_mode(&std::fs::read_to_string(p).ok()?)
}

/// `"light"` / `"dark"`（大小写与首尾空白都容忍）。认不出来返回 `None`。
fn parse_mode(s: &str) -> Option<Mode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "light" => Some(Mode::Light),
        "dark" => Some(Mode::Dark),
        _ => None,
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
    /// 行内代码的「芯片」底色。
    ///
    /// 必须明显偏离 `bg` / `surface`，否则行内代码就是一片糊过去的浅色，
    /// 读者完全看不出这是代码。围栏代码块另有 `code_bg`（一整块底色），
    /// 两者刻意用不同深浅，好让「块」与「行内」一眼分得开。
    code_chip: Color32,
    quote_bar: Color32,
    /// 反白块的底色（错误提示用：亮色=墨黑底，暗色=近白底）
    reverse_bg: Color32,
    /// 反白块上的文字
    on_reverse: Color32,
    /// 悬浮卡片的投影色。亮色下一层淡墨就够；暗色下阴影几乎看不见，只能加重补偿。
    shadow: Color32,
    /// **整套配色里唯一的色相**：危险动作（目前只有「完全访问」这一处用）。
    ///
    /// 上面那一排刻意只用明度表达语义，但「越界之后没有任何边界」这件事值得破一次例 ——
    /// 它是唯一一个**点下去就越过所有保护**的开关，明度层次表达不出「这不是一个普通选项」。
    /// 所以只给这一处留一点砖红，其余仍然全灰。
    danger: Color32,
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
    code_chip: Color32::from_rgb(0xE0, 0xD9, 0xC7),
    quote_bar: Color32::from_rgb(0xC2, 0xBA, 0xA8),
    reverse_bg: Color32::from_rgb(0x1A, 0x19, 0x16),
    on_reverse: Color32::from_rgb(0xF7, 0xF5, 0xF0),
    shadow: Color32::from_rgba_unmultiplied_const(0x1A, 0x19, 0x16, 26),
    // 砖红：不是信号用的鲜红，是纸面上站得住的那种沉红（与整块暖纸底同调）。
    danger: Color32::from_rgb(0x9E, 0x2B, 0x21),
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
    code_chip: Color32::from_rgb(0x39, 0x39, 0x32),
    quote_bar: Color32::from_rgb(0x4C, 0x4C, 0x45),
    reverse_bg: Color32::from_rgb(0xEC, 0xE9, 0xE1),
    on_reverse: Color32::from_rgb(0x14, 0x14, 0x12),
    shadow: Color32::from_rgba_unmultiplied_const(0, 0, 0, 96),
    // 墨底上不能用亮色模式那份砖红（#9E2B21 在近黑底上暗得读不出来），
    // 换成同色相、亮度提上去的高一档红 —— 一样是纸墨体系里的「红」，不是信号灯。
    danger: Color32::from_rgb(0xE0, 0x6C, 0x5C),
};

/// 叠加了配置覆盖之后的两套配色。`apply` 时写一次，之后只读 —— 所以取色仍是零开销。
static ACTIVE: OnceLock<(Palette, Palette)> = OnceLock::new();

fn pal() -> &'static Palette {
    let (light, dark) = ACTIVE.get_or_init(|| (LIGHT, DARK));
    match mode() {
        Mode::Light => light,
        Mode::Dark => dark,
    }
}

/// 把配置里的逐字段覆盖叠到内置配色上；没写的字段原样保留。
///
/// 只覆盖某个字段（例如只改 `text`）是完全安全的：其余 18 项仍是内置值。
fn merge_palette(base: &Palette, ov: &PaletteConfig) -> Palette {
    let c = |d: Color32, o: Option<RgbColor>| match o {
        Some(x) => Color32::from_rgba_unmultiplied(x.r, x.g, x.b, x.a),
        None => d,
    };
    Palette {
        bg: c(base.bg, ov.bg),
        panel: c(base.panel, ov.panel),
        surface: c(base.surface, ov.surface),
        surface_hi: c(base.surface_hi, ov.surface_hi),
        border: c(base.border, ov.border),
        border_strong: c(base.border_strong, ov.border_strong),
        text: c(base.text, ov.text),
        text_dim: c(base.text_dim, ov.text_dim),
        text_faint: c(base.text_faint, ov.text_faint),
        ink: c(base.ink, ov.ink),
        ink_soft: c(base.ink_soft, ov.ink_soft),
        user_bg: c(base.user_bg, ov.user_bg),
        code_bg: c(base.code_bg, ov.code_bg),
        code_chip: c(base.code_chip, ov.code_chip),
        quote_bar: c(base.quote_bar, ov.quote_bar),
        reverse_bg: c(base.reverse_bg, ov.reverse_bg),
        on_reverse: c(base.on_reverse, ov.on_reverse),
        shadow: c(base.shadow, ov.shadow),
        danger: c(base.danger, ov.danger),
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
    /// 行内代码的芯片底色（比 `code_bg` 更跳一档）。
    pub fn code_chip() -> Color32 {
        pal().code_chip
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
    /// 危险动作专用色 —— 整套配色里唯一的色相。只给「完全访问」这类
    /// **点下去就越过所有保护**的开关用，别拿它当普通强调色。
    pub fn danger() -> Color32 {
        pal().danger
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

/// `[theme.fonts]` 里的字体路径。`apply` 时写一次。
static FONTS: OnceLock<FontsConfig> = OnceLock::new();

fn fcfg() -> &'static FontsConfig {
    FONTS.get_or_init(FontsConfig::default)
}

/// 读一个字体文件（空文件视为无效，与 `read_env` 一致）。
fn read_font_file(path: &str) -> Option<(String, Vec<u8>)> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() {
        None
    } else {
        Some((path.to_string(), bytes))
    }
}

/// 一级一级往下找：`[theme.fonts]` 配置 → 环境变量 → 内置候选表。
///
/// 上面两级都把「路径不存在 / 文件为空」化简成 `None`，所以配置写错也不会让界面缺字，
/// 只会安静地退回下一级。
fn font_source(cfg: Option<&String>, env_key: &str, cands: &[&str]) -> Option<(String, Vec<u8>)> {
    cfg.and_then(|p| read_font_file(p))
        .or_else(|| read_env(env_key))
        .or_else(|| read_first(cands))
}

/// 装配字体：内建拉丁字体 + 系统中文回退（排在 emoji 字体之前，避免 CJK 落到 emoji 字形上）。
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    let cjk = font_source(fcfg().cjk.as_ref(), "AOX_GUI_FONT", CJK_CANDIDATES);
    let cjk_name = if let Some((_, bytes)) = cjk {
        fonts.font_data.insert("cjk".to_owned(), Arc::new(FontData::from_owned(bytes)));
        "cjk".to_owned()
    } else {
        String::new()
    };

    let bold = font_source(fcfg().bold.as_ref(), "AOX_GUI_FONT_BOLD", BOLD_CANDIDATES);
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
    let serif = font_source(fcfg().serif.as_ref(), "AOX_GUI_FONT_SERIF", SERIF_CANDIDATES);
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
    let brand = font_source(fcfg().brand.as_ref(), "AOX_GUI_FONT_BRAND", BRAND_CANDIDATES);
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

    let mono = font_source(fcfg().mono.as_ref(), "AOX_GUI_FONT_MONO", MONO_CANDIDATES);
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
    v.window_corner_radius = CornerRadius::same(card_radius());
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
    let m = metrics();
    style.spacing.item_spacing = egui::vec2(m.item_spacing[0], m.item_spacing[1]);
    style.spacing.button_padding = egui::vec2(m.button_padding[0], m.button_padding[1]);
    style.spacing.interact_size.y = m.interact_height;
    style.spacing.indent = m.indent;
    style.spacing.scroll = egui::style::ScrollStyle::thin();
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.bar_inner_margin = 3.0;
    style.spacing.scroll.bar_outer_margin = 4.0;
    style.spacing.scroll.floating = true;

    style.text_styles.insert(TextStyle::Body, body_font(body_size()));
    style.text_styles.insert(TextStyle::Button, body_font(button_size()));
    style.text_styles.insert(TextStyle::Small, body_font(small_size()));
    style.text_styles.insert(TextStyle::Monospace, mono_font(mono_size()));
    style.text_styles.insert(TextStyle::Heading, bold_font(heading_size()));

    ctx.set_style(style);
}

/// 一次性装配：把配置里的外观覆盖叠到内置主题上。
///
/// **明暗的优先级**：上次手动切换的记录 > `[theme] mode` > 默认（墨）。
/// 用户刚点过的那一下最算数；`[theme] mode` 只在「从没切过」时说了算。
pub fn apply(ctx: &egui::Context, cfg: &ThemeConfig) {
    set_mode(
        read_pref()
            .or_else(|| cfg.mode.as_deref().and_then(parse_mode))
            .unwrap_or(Mode::Dark),
    );

    // 三份覆盖都只写一次，之后全程只读 —— 所以 `P::xx()` 取色仍是零开销。
    let _ = ACTIVE.set((
        merge_palette(&LIGHT, &cfg.light),
        merge_palette(&DARK, &cfg.dark),
    ));
    let _ = METRICS.set(cfg.metrics.clone());
    let _ = FONTS.set(cfg.fonts.clone());

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

/// 设一条 DWM 窗口属性（`DWMWA_*`）。
///
/// Windows 11 (build 22000+) 才认识这类属性，旧系统上调用返回失败 ——
/// **静默忽略即可**，效果只是退回系统默认，不是错误。
///
/// 单独抽出来是为了**全 crate 只有这一处 extern 声明**。同一个符号在两个文件里
/// 各声明一遍，签名稍有出入编译器就报 `clashing_extern_declarations`
/// （真踩过：`hwnd` 一边写 `isize`、一边写 `*mut c_void`）。
#[cfg(windows)]
pub(crate) fn dwm_set_attribute(
    hwnd: isize,
    attr: u32,
    value: *const core::ffi::c_void,
    size: u32,
) -> i32 {
    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: isize,
            attr: u32,
            value: *const core::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    unsafe { DwmSetWindowAttribute(hwnd, attr, value, size) }
}

#[cfg(not(windows))]
pub(crate) fn dwm_set_attribute(
    _hwnd: isize,
    _attr: u32,
    _value: *const core::ffi::c_void,
    _size: u32,
) -> i32 {
    -1
}

/// `DWMWA_BORDER_COLOR`
pub(crate) const DWMWA_BORDER_COLOR: u32 = 34;
/// `DWMWA_CAPTION_COLOR`
pub(crate) const DWMWA_CAPTION_COLOR: u32 = 35;
/// `DWMWA_TEXT_COLOR`
pub(crate) const DWMWA_TEXT_COLOR: u32 = 36;
/// `DWMWA_WINDOW_CORNER_PREFERENCE`（配合 `DWMWCP_*`）
pub(crate) const DWMWA_CORNER_PREFERENCE: u32 = 33;
/// `DWMWCP_ROUND`：系统的标准圆角。`DONOTROUND` = 1，`ROUND` = 2，`ROUNDSMALL` = 3。
pub(crate) const DWMWCP_ROUND: i32 = 2;

/// 把原生标题栏染成与界面**完全一致**的颜色，让它彻底融入窗口。
///
/// `sync_native_theme` 只能让标题栏在「系统亮色 / 系统暗色」里二选一，拿到的是
/// DWM 自己那套灰（亮色约 `#F3F3F3`、暗色约 `#202020`），和纸与墨的暖调对不上。
/// 想真正严丝合缝，只能直接设 DWM 属性：
///
/// - `DWMWA_CAPTION_COLOR`(35)：标题栏底色
/// - `DWMWA_TEXT_COLOR`(36)：标题栏文字色
/// - `DWMWA_BORDER_COLOR`(34)：窗口外框线
#[cfg(windows)]
/// 标题栏左上角那枚小图标的**替身**：一张 1×1 的全透明 PNG。
///
/// Windows 在「窗口没设图标、窗口类也没设图标」时会一路回退到 **exe 资源里的
/// 第一个图标**（实测 `WM_SETICON` 传 0 清不掉）—— 标题栏左上角于是永远杵着
/// 那枚 AOX 图标。想让它消失，稳的办法只有一个：**给它一个看不见的图标**。
///
/// 字节由 `PIL` 生成 `Image.new('RGBA',(1,1),(0,0,0,0))` 得来，紧跟在
/// `CreateIconFromResourceEx` 能直接消化的 PNG 裸字节。
const TRANSPARENT_PNG: [u8; 70] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
    0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x60,
    0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x05, 0x00, 0x01, 0xA5, 0xF6, 0x45, 0x40, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// 把标题栏左上角那枚**小**图标换成透明。
///
/// 只动 `ICON_SMALL`：`ICON_BIG`（任务栏与 Alt+Tab 用的那枚）不碰 ——
/// 那里还靠 exe 的图标认人，一并抹掉的话任务栏就只剩一块空按钮。
///
/// HICON 只造一次、缓存复用，并且**故意不销毁**：`WM_SETICON` 的句柄归应用所有，
/// 销毁了窗口再画标题栏就是在用悬空句柄 —— 反正只有一个，进程退出时随进程回收。
#[cfg(windows)]
fn hide_caption_icon(hwnd: isize) {
    use std::sync::OnceLock;
    static ICON: OnceLock<isize> = OnceLock::new();

    #[link(name = "user32")]
    extern "system" {
        fn CreateIconFromResourceEx(
            pb_icon_bits: *const u8,
            cb_icon_bits: u32,
            f_icon: i32,
            dw_version: u32,
            cx_desired: i32,
            cy_desired: i32,
            flags: u32,
        ) -> isize;
        fn SendMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    }

    let icon = *ICON.get_or_init(|| unsafe {
        CreateIconFromResourceEx(TRANSPARENT_PNG.as_ptr(), 70, 1, 0x0003_0000, 16, 16, 0)
    });
    if icon != 0 {
        const WM_SETICON: u32 = 0x0080;
        const ICON_SMALL: usize = 0;
        unsafe { SendMessageW(hwnd, WM_SETICON, ICON_SMALL, icon) };
    }
}

#[cfg(not(windows))]
fn hide_caption_icon(_hwnd: isize) {}

pub fn sync_caption(handle: &dyn raw_window_handle::HasWindowHandle) {
    use raw_window_handle::RawWindowHandle;

    let Ok(h) = handle.window_handle() else { return };
    let RawWindowHandle::Win32(w) = h.as_raw() else {
        return;
    };
    let hwnd = w.hwnd.get();

    // 注意 DWM 要的是 COLORREF（0x00BBGGRR），和 Color32 的 RGBA 排列不同，得换序
    let colorref = |c: Color32| -> u32 {
        ((c.b() as u32) << 16) | ((c.g() as u32) << 8) | c.r() as u32
    };

    // 标题栏与边框都吃主界面的**底色**：顶栏拆掉之后，对话区的底色就是整个窗口
    // 的底色，标题栏再用 panel 就成了一条明晃晃的色差（2026-10-04 用户要求「无色差」）。
    // 边框一并抹平 —— 不然窗口四周还会留一圈「另一档灰」。
    // （用 panel 是顶栏还在时的选择：顶栏与标题栏同色读作一体。顶栏没了，跟 bg 走。）
    let cap = colorref(P::bg());
    let text = colorref(P::text_dim());
    let border = colorref(P::bg());
    // 失败也无所谓（老系统不支持），所以返回值直接丢掉
    dwm_set_attribute(hwnd, DWMWA_CAPTION_COLOR, std::ptr::addr_of!(cap).cast(), 4);
    dwm_set_attribute(hwnd, DWMWA_TEXT_COLOR, std::ptr::addr_of!(text).cast(), 4);
    dwm_set_attribute(hwnd, DWMWA_BORDER_COLOR, std::ptr::addr_of!(border).cast(), 4);
    hide_caption_icon(hwnd);
}

#[cfg(not(windows))]
pub fn sync_caption(_handle: &dyn raw_window_handle::HasWindowHandle) {}
