//! Paper-like black-and-white theme: palette, whitespace, corner radii, and CJK font loading.
//!
//! The design leans into "paper and ink" — no hue, only lightness steps and whitespace:
//! light mode is a slightly warm sheet of paper; dark mode is a warm-tinted black.
//! Status semantics (success / warning / failure) are told apart not by color but by lightness contrast and inverted blocks.
//!
//! egui built-in fonts lack CJK glyphs, so Chinese renders as blank boxes. Here we pick a
//! modern sans-serif CJK font from the system font directories (preferring "Deng"/DengXian, then "Microsoft YaHei") and inject it as a fallback.
//! We also register a genuine "bold" family so Markdown `**bold**` actually gets a heavier weight.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use aox::config::{FontsConfig, MetricsConfig, PaletteConfig, RgbColor, ThemeConfig};
use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle, Visuals,
};

// ─────────────────────────── Font sizes / whitespace scale ───────────────────────────
//
// These values now come from `[theme.metrics]` (the defaults equal the previously hard-coded
// constants), so they can only be **runtime accessors** rather than `const`. `metrics()` goes
// through `OnceLock`: written once at assembly time, then read-only, at constant cost.

static METRICS: OnceLock<MetricsConfig> = OnceLock::new();

fn metrics() -> &'static MetricsConfig {
    METRICS.get_or_init(MetricsConfig::default)
}

/// Body font size. The whole UI is sized relative to it.
pub fn body_size() -> f32 {
    metrics().body_size
}

/// Heading font size.
pub fn heading_size() -> f32 {
    metrics().heading_size
}

/// Button label font size.
pub fn button_size() -> f32 {
    metrics().button_size
}

/// Small-text font size.
pub fn small_size() -> f32 {
    metrics().small_size
}

/// Monospace font size (used for code blocks).
pub fn mono_size() -> f32 {
    metrics().mono_size
}

/// Card corner radius.
pub fn card_radius() -> u8 {
    metrics().card_radius
}

/// Custom bold font-family name.
const BOLD_FAMILY: &str = "aox-strong";
/// Custom serif font-family name (for sub-headings like blockquotes; falls back to the plain family if not found).
const SERIF_FAMILY: &str = "aox-serif";
/// Custom brand font-family name (for the big greeting on the first screen; falls back to serif if not found).
const BRAND_FAMILY: &str = "aox-brand";
static BOLD_OK: AtomicBool = AtomicBool::new(false);
static SERIF_OK: AtomicBool = AtomicBool::new(false);
static BRAND_OK: AtomicBool = AtomicBool::new(false);

/// Whether a genuine bold font was found on the system.
pub fn bold_available() -> bool {
    BOLD_OK.load(Ordering::Relaxed)
}

/// Bold FontId (falls back to the plain family + brighter color when no bold is available).
pub fn bold_font(size: f32) -> FontId {
    if bold_available() {
        FontId::new(size, FontFamily::Name(BOLD_FAMILY.into()))
    } else {
        FontId::proportional(size)
    }
}

/// Serif FontId (for sub-headings; falls back to bold when no serif font is available).
pub fn serif_font(size: f32) -> FontId {
    if SERIF_OK.load(Ordering::Relaxed) {
        FontId::new(size, FontFamily::Name(SERIF_FAMILY.into()))
    } else {
        bold_font(size)
    }
}

/// Brand-font FontId (originally for the big greeting on the first screen).
///
/// Looser than the serif with a hint of handwritten stroke, so a line of greeting does not read like a document letterhead;
/// when no display font is found it steps down serif to bold, so calling it is always safe.
///
/// WARNING: currently **has no caller**: the first-screen greeting "Hi there" was replaced by an illustration on 2026-10-06 (per user request).
/// It is kept because this family is itself a **config item** (`brand` / `AOX_GUI_FONT_BRAND`; the font stack is still installed in
/// `install_fonts`) — if the user ever wants a display font somewhere again, just call it instead of re-implementing it.
#[allow(dead_code)]
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

// ─────────────────────────── Light / dark mode ───────────────────────────

/// The **actually active** palette (two variants).
///
/// Kept separate from [`Pref`] on purpose: `Mode` answers "which color set is in use right now" and is always either light or dark, whereas the user can also pick a third option — "follow
/// the system". If that option lived in `Mode`, every `mode() == Mode::Dark` check would have to also wonder "and what does System count as?", and the answer
/// would shift with the system.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Paper
    Light,
    /// Ink
    Dark,
}

/// The **preference** the user picks on the settings page (three options). This is what gets persisted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pref {
    Light,
    Dark,
    /// Follow the system: switch along when the system theme changes (see [`follow_system`]).
    System,
}

impl Pref {
    /// The label for this option on the settings page.
    pub fn label(self) -> &'static str {
        match self {
            Pref::Light => aox::i18n::t("Light · paper"),
            Pref::Dark => aox::i18n::t("Dark · ink"),
            Pref::System => aox::i18n::t("Follow system"),
        }
    }

    /// The string literal used when persisting.
    fn key(self) -> &'static str {
        match self {
            Pref::Light => "light",
            Pref::Dark => "dark",
            Pref::System => "system",
        }
    }

    /// The order shown on the settings page: "follow system" first (it is the "do not care" option).
    pub const ALL: [Pref; 3] = [Pref::System, Pref::Dark, Pref::Light];
}

static MODE: AtomicU8 = AtomicU8::new(1); // default dark (ink)
static PREF: AtomicU8 = AtomicU8::new(1); // default preference is also dark

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

/// The current preference (the settings page uses this to draw the selected state).
pub fn pref() -> Pref {
    match PREF.load(Ordering::Relaxed) {
        0 => Pref::Light,
        2 => Pref::System,
        _ => Pref::Dark,
    }
}

fn set_pref(p: Pref) {
    PREF.store(
        match p {
            Pref::Light => 0,
            Pref::System => 2,
            Pref::Dark => 1,
        },
        Ordering::Relaxed,
    );
}

/// Compute the **actually** active palette from "user preference + current system theme".
///
/// Extracted as a pure function so it can be asserted directly — everything about the "follow
/// system" option is this truth table, and it is exactly the kind of thing that gets written as
/// "follow == always dark" (silently falling back to dark when the system theme is unavailable,
/// so "follow system" appears to never take effect).
pub fn effective(p: Pref, system: Option<egui::Theme>) -> Mode {
    match p {
        Pref::Light => Mode::Light,
        Pref::Dark => Mode::Dark,
        // Fall back to dark when the backend reports no system theme — matches the default before this option was added.
        Pref::System => match system {
            Some(egui::Theme::Light) => Mode::Light,
            _ => Mode::Dark,
        },
    }
}

/// Which option the user picked on the settings page: apply immediately + persist.
pub fn apply_pref(ctx: &egui::Context, p: Pref) {
    set_pref(p);
    set_mode(effective(p, ctx.system_theme()));
    install_style(ctx);
    sync_native_theme(ctx);
    save_pref();
}

/// Per-frame check for "follow system": switch along when the system theme changes. Returns whether it actually changed.
///
/// Why not compute it only at the switch moment: the system theme **changes at runtime** (the user flips to dark
/// in the OS settings), and egui puts the current value in `ctx` every frame. Without this check "follow system"
/// would only mean "decide once at startup from the system".
pub fn follow_system(ctx: &egui::Context) -> bool {
    if pref() != Pref::System {
        return false;
    }
    let want = effective(Pref::System, ctx.system_theme());
    if mode() == want {
        return false;
    }
    set_mode(want);
    install_style(ctx);
    sync_native_theme(ctx);
    true
}

// Where the theme preference is persisted: a small file in the app root directory.
// Silently give up if no writable location is found (only the preference is lost, usage is unaffected).
fn pref_path() -> Option<std::path::PathBuf> {
    let (root, _) = aox::paths::resolve_workplace();
    let p = root.join(".aox-gui-theme");
    if p.parent().is_some_and(|d| d.is_dir()) {
        Some(p)
    } else {
        None
    }
}

/// Read the last saved theme preference. `None` = never recorded (or unrecognized), leaving the choice to the caller.
fn read_pref() -> Option<Pref> {
    let p = pref_path()?;
    parse_pref(&std::fs::read_to_string(p).ok()?)
}

/// `"light"` / `"dark"` / `"system"` (case and surrounding whitespace tolerated). Returns `None` if unrecognized.
fn parse_pref(s: &str) -> Option<Pref> {
    match s.trim().to_ascii_lowercase().as_str() {
        "light" => Some(Pref::Light),
        "dark" => Some(Pref::Dark),
        "system" => Some(Pref::System),
        _ => None,
    }
}

/// What gets persisted is the **preference**, not the active mode.
///
/// This must be stated clearly: persisting the active mode would erase "follow system" after the first time it follows —
/// a light system writes `"light"`, and on next launch it never follows again.
fn save_pref() {
    if let Some(p) = pref_path() {
        let _ = std::fs::write(p, pref().key());
    }
}

// ─────────────────────────── Palette ───────────────────────────

/// A complete color set. The fields deliberately differ only by lightness, carrying no hue.
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
    /// "Ink" — the accent color in the monochrome scheme: near-black in light mode, near-white in dark.
    ink: Color32,
    /// A softened version of ink.
    ink_soft: Color32,
    user_bg: Color32,
    code_bg: Color32,
    /// The "chip" background for inline code.
    ///
    /// It must clearly depart from `bg` / `surface`; otherwise inline code is just a smeared
    /// light patch and the reader can't tell it's code. Fenced code blocks use `code_bg`
    /// (a full block of background) instead; the two are deliberately set at different depths
    /// so "block" and "inline" read apart at a glance.
    code_chip: Color32,
    quote_bar: Color32,
    /// Background of the inverted block (used for error hints: ink-black in light mode, near-white in dark).
    reverse_bg: Color32,
    /// Text on the inverted block.
    on_reverse: Color32,
    /// Drop-shadow color for floating cards. A faint ink is enough in light mode; in dark mode shadows are nearly invisible, so we compensate by darkening.
    shadow: Color32,
    /// Dangerous actions — a brick-red (used at the single "full access" point).
    ///
    /// Most of this palette expresses meaning through lightness alone; this is the first deliberate
    /// exception, because "past the boundary there is no boundary" is not something a lightness step
    /// can say. `add_green` / `del_red` and the `toast_*` trio are the other, smaller exceptions —
    /// every hue in the struct carries a meaning, none of them is decoration.
    danger: Color32,
    /// Lines **added** by an edit (the `+N` after a tool name). A calm green: it sits inside a text
    /// row, so it has to read as information rather than as a signal light.
    add_green: Color32,
    /// Lines **removed** by an edit (the `−N` after a tool name).
    del_red: Color32,
    /// Background of the transient toast. A pale red — a softened relative of `danger`.
    ///
    /// The toast reports something that already happened (e.g. files moved to the recycle bin), so it
    /// must read as a notice at a glance instead of blending into the ordinary gray cards. A washed-out
    /// red does that without shouting; the text stays the regular ink color, so it remains legible.
    toast_bg: Color32,
    /// Border of the transient toast: one step darker than `toast_bg`, same hue.
    toast_border: Color32,
    /// Foreground of the toast — the text and the icon next to it. A **deep red**, darker than the
    /// fill so the notice reads as red without turning into a signal light.
    toast_text: Color32,
}

/// Paper: a slightly warm white, not pure white.
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
    // Brick-red: not a signal-bright red, but a settled red that holds up on the page (in tune with the warm paper base).
    danger: Color32::from_rgb(0x9E, 0x2B, 0x21),
    // Edit statistics: a settled green / red that hold up as text on the paper base (same calibration
    // as `danger` — legible, not fluorescent).
    add_green: Color32::from_rgb(0x2F, 0x7D, 0x4F),
    del_red: Color32::from_rgb(0xA8, 0x32, 0x2A),
    // A pale brick tint: the same hue as `danger`, lifted almost to paper so the toast pops out of the gray.
    toast_bg: Color32::from_rgb(0xFA, 0xE7, 0xE3),
    toast_border: Color32::from_rgb(0xEB, 0xC0, 0xB8),
    toast_text: Color32::from_rgb(0x7A, 0x23, 0x1B),
};

/// Ink: a warm-tinted black, not pure black.
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
    // On the ink base we cannot reuse the light-mode brick-red (#9E2B21 is too dark to read on a near-black base),
    // so we swap in a brighter red of the same hue — still a "red" within the paper-and-ink system, not a signal light.
    danger: Color32::from_rgb(0xE0, 0x6C, 0x5C),
    // Same hue family as the light theme, lifted one step so they read on the ink base.
    add_green: Color32::from_rgb(0x5F, 0xBF, 0x7A),
    del_red: Color32::from_rgb(0xDE, 0x80, 0x74),
    // On the ink base a "pale red" means low-saturation dark: the hue has to be read off the warm tint, not the lightness.
    toast_bg: Color32::from_rgb(0x35, 0x22, 0x1F),
    toast_border: Color32::from_rgb(0x67, 0x3A, 0x32),
    // On the ink base "deep red" would be unreadable, so the foreground flips to a light red of the
    // same hue — the pale fill and the deep-on-light relationship are preserved in reverse.
    toast_text: Color32::from_rgb(0xF0, 0xC0, 0xB8),
};

/// The two palettes after config overrides are merged in. Written once at `apply`, then read-only — so color lookups stay zero-cost.
static ACTIVE: OnceLock<(Palette, Palette)> = OnceLock::new();

fn pal() -> &'static Palette {
    let (light, dark) = ACTIVE.get_or_init(|| (LIGHT, DARK));
    match mode() {
        Mode::Light => light,
        Mode::Dark => dark,
    }
}

/// Merge per-field config overrides onto the built-in palette; fields not specified are kept.
///
/// Overriding only a single field (e.g. just `text`) is completely safe: the other 18 entries remain the built-in values.
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
        // Not configurable: edit statistics are a fixed pairing, kept internal like the toast tint.
        add_green: base.add_green,
        del_red: base.del_red,
        // Not configurable: the toast tint is a derived companion of `danger`, kept internal.
        toast_bg: base.toast_bg,
        toast_border: base.toast_border,
        toast_text: base.toast_text,
    }
}

/// Color accessors for the current theme. Evaluated at runtime, so they take effect immediately on theme switch.
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

    /// The fill of a **row** under the pointer — one soft step above [`P::surface`].
    ///
    /// `surface_hi` is a 22-unit step: right for a button or a menu row, too loud for a column of
    /// twenty list rows, where it turns the whole panel grey. This is the same idea at ~3% ink, so
    /// a hovered row reads as *touched* without becoming a block of colour.
    pub fn hover_fill() -> Color32 {
        match mode() {
            Mode::Light => Color32::from_rgb(0xF3, 0xF0, 0xE8),
            Mode::Dark => Color32::from_rgb(0x26, 0x26, 0x21),
        }
    }

    /// The fill of the **selected** row: one more step (~6% ink) on top of [`P::hover_fill`].
    ///
    /// Deliberately the only marker the current conversation gets (no dot, no bar): at rest the
    /// list is plain text on paper, and the one row you are in is a shade darker than the rest.
    pub fn active_fill() -> Color32 {
        match mode() {
            Mode::Light => Color32::from_rgb(0xEC, 0xE8, 0xDE),
            Mode::Dark => Color32::from_rgb(0x2C, 0x2C, 0x26),
        }
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
    /// Monochrome accent color (formerly ACCENT).
    pub fn ink() -> Color32 {
        pal().ink
    }
    /// Softened accent color (formerly ACCENT_DIM).
    pub fn ink_soft() -> Color32 {
        pal().ink_soft
    }
    pub fn user_bg() -> Color32 {
        pal().user_bg
    }
    pub fn code_bg() -> Color32 {
        pal().code_bg
    }
    /// Chip background for inline code (one step more emphatic than `code_bg`).
    pub fn code_chip() -> Color32 {
        pal().code_chip
    }
    pub fn quote_bar() -> Color32 {
        pal().quote_bar
    }

    // ── Semantic colors: expressed via lightness steps in the monochrome scheme, not hue ──

    /// Warning: mid-gray (signaled by lightness in the monochrome scheme).
    pub fn warn() -> Color32 {
        pal().text_dim
    }
    /// Inverted-block background.
    pub fn reverse_bg() -> Color32 {
        pal().reverse_bg
    }
    /// Text on the inverted block.
    pub fn on_reverse() -> Color32 {
        pal().on_reverse
    }
    /// Drop-shadow color for floating cards.
    pub fn shadow() -> Color32 {
        pal().shadow
    }
    /// Color reserved for dangerous actions — the only hue in the whole palette. Use only for toggles like "full access"
    /// that **cross every protection once clicked**; do not treat it as an ordinary accent.
    pub fn danger() -> Color32 {
        pal().danger
    }
    /// Foreground of the "lines added" figure in a tool row.
    pub fn add_green() -> Color32 {
        pal().add_green
    }
    /// Foreground of the "lines removed" figure in a tool row.
    pub fn del_red() -> Color32 {
        pal().del_red
    }
    /// Pale-red background of the transient toast (see `Palette::toast_bg`).
    pub fn toast_bg() -> Color32 {
        pal().toast_bg
    }
    /// Border of the transient toast.
    pub fn toast_border() -> Color32 {
        pal().toast_border
    }
    /// Foreground of the transient toast (its text and icon).
    pub fn toast_text() -> Color32 {
        pal().toast_text
    }

    /// The 1px line around a **card**.
    ///
    /// Deliberately not [`P::border`]. `border` is a fixed grey, which is right for hairlines
    /// *inside* a surface (table cells, separators) where they must not compete with the content.
    /// A card's outer edge is different: it sits on a base that varies (`bg`, `panel`, another
    /// card), so a fixed grey comes out too faint on one and too loud on the other — which is
    /// exactly why the cards used to dissolve into the page.
    ///
    /// So the edge is **ink at a fixed opacity**: it mixes with whatever is underneath and is
    /// visible by the same amount everywhere, on both themes.
    pub fn edge() -> Color32 {
        match mode() {
            // 24% ink on paper: unmistakably a line, still quieter than text.
            Mode::Light => Color32::from_rgba_unmultiplied(0x1A, 0x19, 0x16, 0x3D),
            // 18% white on ink: the same "one visible step", without turning into a bright ring.
            Mode::Dark => Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 0x2E),
        }
    }

    /// The edge of a card that is **raised above** the page (dialogs, the history panel, the
    /// sub-windows). One step louder than [`P::edge`], used together with [`card_shadow`].
    pub fn edge_raised() -> Color32 {
        match mode() {
            Mode::Light => Color32::from_rgba_unmultiplied(0x1A, 0x19, 0x16, 0x59),
            Mode::Dark => Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 0x47),
        }
    }

    /// The veil drawn behind a modal dialog.
    ///
    /// egui's default is a flat 39% **black**, which on a warm paper base turns the whole window a
    /// grey-green and drains the colour out of it — the page stops looking like paper and starts
    /// looking like a screenshot of one. Tinted with the theme's own ink instead, the veil only
    /// takes light away and leaves the hue alone, so the page still reads as paper behind glass.
    pub fn scrim() -> Color32 {
        match mode() {
            Mode::Light => Color32::from_rgba_unmultiplied(0x1A, 0x19, 0x16, 0x38),
            Mode::Dark => Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 0x8C),
        }
    }
}

/// The shadow under a **raised card** — dialogs, the history panel, the floating sub-windows.
///
/// A card on the page gets [`P::edge`] and nothing else: it is printed *on* the sheet, so a shadow
/// would be a lie. Only what actually floats above the page casts one, and the size of that shadow
/// is how the UI says "this is on top" without a single extra line.
pub fn card_shadow() -> egui::Shadow {
    match mode() {
        Mode::Light => egui::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(0x1A, 0x19, 0x16, 0x24),
        },
        Mode::Dark => egui::Shadow {
            offset: [0, 5],
            blur: 20,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 0x8C),
        },
    }
}

/// The shadow of a card lying **on** the page: a *contact* shadow, nearly all of it directly under
/// the edge.
///
/// Not a drop shadow — nothing is floating, so it must not look lifted. What it does is darken the
/// one pixel right under the bottom edge. On a warm paper base that is what turns the outline into a
/// clean line; a grey border on its own keeps dissolving into the page, however dark it is made.
fn contact_shadow() -> egui::Shadow {
    match mode() {
        Mode::Light => egui::Shadow {
            offset: [0, 2],
            blur: 6,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(0x1A, 0x19, 0x16, 0x40),
        },
        Mode::Dark => egui::Shadow {
            offset: [0, 2],
            blur: 6,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 0xA0),
        },
    }
}

/// A card that lies **on** the page: surface fill + a clear edge + a contact shadow.
pub fn card(radius: u8) -> egui::Frame {
    egui::Frame::NONE
        .fill(P::surface())
        .corner_radius(CornerRadius::same(radius))
        .stroke(Stroke::new(1.0, P::edge()))
        .shadow(contact_shadow())
}

/// A card that is **raised above** the page: brighter edge + shadow + the top light.
pub fn raised(radius: u8) -> egui::Frame {
    egui::Frame::NONE
        .fill(P::surface())
        .corner_radius(CornerRadius::same(radius))
        .stroke(Stroke::new(1.0, P::edge_raised()))
        .shadow(card_shadow())
}

/// A **well**: a block sunk *into* a card (code, quotes, inert insets).
///
/// No edge and no shadow on purpose — it is a change of depth inside one sheet, not a second sheet
/// laid on top. Giving it a border is what made the transcript look like a stack of boxes.
pub fn well(radius: u8) -> egui::Frame {
    egui::Frame::NONE
        .fill(P::code_bg())
        .corner_radius(CornerRadius::same(radius))
}

// ─────────────────────────── Font loading ───────────────────────────

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

/// Try each candidate filename in order and return the first one successfully read.
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

/// CJK font candidates (ordered by "modern feel").
const CJK_CANDIDATES: &[&str] = &[
    "Deng.ttf",              // Deng (Win10+, clean and modern)
    "msyh.ttc",              // Microsoft YaHei
    "msyhl.ttc",
    "simhei.ttf",            // SimHei
    "simsun.ttc",            // SimSun
    "msjh.ttc",              // Microsoft JhengHei (Traditional)
    "PingFang.ttc",          // macOS
    "Hiragino Sans GB.ttc",
    "NotoSansCJK-Regular.ttc",
    "NotoSansSC-Regular.otf",
];

/// Bold candidates.
const BOLD_CANDIDATES: &[&str] = &[
    "Dengb.ttf",
    "msyhbd.ttc",
    "simhei.ttf",
    "msjhbd.ttc",
    "PingFang.ttc",
    "NotoSansCJK-Bold.ttc",
    "NotoSansSC-Bold.otf",
];

/// Serif candidates (for sub-headings).
const SERIF_CANDIDATES: &[&str] = &[
    "georgia.ttf",
    "times.ttf",
    "constan.ttf",
    "cambria.ttc",
    "georgiab.ttf",
    "Times New Roman.ttf",
];

/// Brand-font candidates (for the first-screen greeting).
///
/// Ordered "strongest to weakest stroke"; all are Windows-bundled fonts (Gabriola / Inkfree ship with the OS from Win10 onward).
/// If none are found we fall back to serif and the UI never loses glyphs because of it.
const BRAND_CANDIDATES: &[&str] = &[
    "Gabriola.ttf",  // Calligraphic, brush-connected strokes, strongest gesture
    "Inkfree.ttf",   // Pen handwriting, thin and relaxed
    "Candara.ttf",   // Humanist sans-serif, slightly upturned stroke ends
    "georgia.ttf",
];

/// Monospace font candidates (for code blocks).
const MONO_CANDIDATES: &[&str] = &[
    "CascadiaMono.ttf",
    "CascadiaCode.ttf",
    "consola.ttf",
    "lucon.ttf",
    "SourceCodePro-Regular.otf",
];

/// Read a custom font path from an environment variable (AOX_GUI_FONT / AOX_GUI_FONT_BOLD /
/// AOX_GUI_FONT_SERIF / AOX_GUI_FONT_BRAND / AOX_GUI_FONT_MONO).
fn read_env(key: &str) -> Option<(String, Vec<u8>)> {
    let path = std::env::var(key).ok()?;
    let bytes = std::fs::read(&path).ok()?;
    if bytes.is_empty() {
        None
    } else {
        Some((path, bytes))
    }
}

/// Font paths from `[theme.fonts]`. Written once at `apply`.
static FONTS: OnceLock<FontsConfig> = OnceLock::new();

fn fcfg() -> &'static FontsConfig {
    FONTS.get_or_init(FontsConfig::default)
}

/// Read a font file (an empty file is treated as invalid, consistent with `read_env`).
fn read_font_file(path: &str) -> Option<(String, Vec<u8>)> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() {
        None
    } else {
        Some((path.to_string(), bytes))
    }
}

/// Walk the levels top to bottom: `[theme.fonts]` config → environment variable → built-in
/// candidate list.
///
/// The two upper levels collapse "path missing / file empty" into `None`, so a wrong config
/// won't make the UI lose glyphs — it just silently steps down to the next level.
fn font_source(cfg: Option<&String>, env_key: &str, cands: &[&str]) -> Option<(String, Vec<u8>)> {
    cfg.and_then(|p| read_font_file(p))
        .or_else(|| read_env(env_key))
        .or_else(|| read_first(cands))
}

/// Assemble fonts: built-in Latin fonts + system CJK fallback (placed before emoji fonts so CJK does not fall onto an emoji glyph).
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

    // Serif: for sub-headings. Falls back to bold if not found (handled inside serif_font).
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

    // Brand font: the slot kept for display fonts (the first-screen greeting is now an illustration, but the family is a config item, so it stays).
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

    // Explicitly reorder family priority: Latin to CJK to emoji
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

// ─────────────────────────── Style ───────────────────────────

fn widget(v: &mut Visuals, f: impl Fn(&mut egui::style::WidgetVisuals)) {
    f(&mut v.widgets.noninteractive);
    f(&mut v.widgets.inactive);
    f(&mut v.widgets.hovered);
    f(&mut v.widgets.active);
    f(&mut v.widgets.open);
}

/// Make the whole app read as "no border noise, low contrast, speaking only through lightness".
/// Just call this function again after switching themes (fonts need not be reinstalled).
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
    v.window_stroke = Stroke::new(1.0, P::edge_raised());
    v.window_corner_radius = CornerRadius::same(card_radius());
    // Everything egui builds for itself (popups, combo boxes, tooltips, modal backdrops) is raised
    // above the page, so it all uses the one shadow. Set here rather than at each call site: a
    // hand-drawn dialog and a built-in one then look like the same kind of object.
    v.window_shadow = card_shadow();
    v.popup_shadow = card_shadow();
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

    // Hover is **one** language everywhere: the fill steps up to `surface_hi`, the text comes
    // forward to full ink, and the outline steps to `edge` — it does not jump to a hard grey ring
    // (that was what made a resting control look "activated" the moment the pointer touched it).
    v.widgets.hovered.bg_fill = P::surface_hi();
    v.widgets.hovered.weak_bg_fill = P::surface_hi();
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, P::edge());
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, P::text());

    v.widgets.active.bg_fill = P::surface_hi();
    v.widgets.active.weak_bg_fill = P::surface_hi();
    v.widgets.active.bg_stroke = Stroke::new(1.0, P::edge_raised());
    v.widgets.active.fg_stroke = Stroke::new(1.0, P::text());

    v.widgets.open.bg_fill = P::surface();
    v.widgets.open.weak_bg_fill = P::surface();
    v.widgets.open.fg_stroke = Stroke::new(1.0, P::text());

    style.visuals = v;

    // Turn off "text in labels is selectable".
    //
    // This flag is on by default, at the cost of **every ui.label grabbing the mouse**: on hover
    // the cursor becomes the text-selection I-beam and a press can drag out a selection. Any
    // button built from labels (the "clear" / "close" / "allow once" buttons and the example
    // rows) then sits under a text cursor on hover, looking like you clicked the wrong spot. The
    // UI has no paragraph text worth copying, so just disabling it globally is cleanest.
    style.interaction.selectable_labels = false;

    // Whitespace is scaled up a notch overall — the main reason cards grow and stop feeling cramped.
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

/// One-time assembly: merge the appearance overrides from config onto the built-in theme.
///
/// **Light/dark precedence**: last manual switch record > `[theme] mode` > default (ink).
/// The most recent click wins; `[theme] mode` only counts when "never switched before".
pub fn apply(ctx: &egui::Context, cfg: &ThemeConfig) {
    // Preference order: last choice on the settings page (`.aox-gui-theme`) to `[theme] mode` from `config.toml` to factory default (dark).
    // Note that the `config.toml` layer is only a **starting value**; once the user picks something on the settings page that wins —
    // otherwise "change config to switch theme" makes the settings page pointless.
    let pref = read_pref()
        .or_else(|| cfg.mode.as_deref().and_then(parse_pref))
        .unwrap_or(Pref::Dark);
    set_pref(pref);
    set_mode(effective(pref, ctx.system_theme()));

    // All three overrides are written only once, then read-only for the rest of the run — so `P::xx()` lookups stay zero-cost.
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

/// Make the OS window decorations (title bar, border) follow the app theme.
///
/// The native title bar is not drawn by egui but by the window manager (DWM on Windows) and
/// follows the system theme by default. Without syncing, a light paper-white UI would sit under
/// a dark title bar — jarring.
pub fn sync_native_theme(ctx: &egui::Context) {
    let t = match mode() {
        Mode::Light => egui::SystemTheme::Light,
        Mode::Dark => egui::SystemTheme::Dark,
    };
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(t));
}

/// Set a DWM window attribute (`DWMWA_*`).
///
/// Windows 11 (build 22000+) recognizes these attributes; on older systems the call fails —
/// **just ignore it silently**, the only effect is falling back to the system default, not an error.
///
/// Pulled out on its own so that **this is the only extern declaration across the whole crate**. If the same symbol is declared in two files, any slight signature mismatch makes the compiler
/// emit `clashing_extern_declarations` (we actually hit this: `hwnd` was `isize` on one side and
/// `*mut c_void` on the other).
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

/// `SM_CXICON`: the edge length of the icon used for the taskbar / Alt+Tab (32 at 100% scaling,
/// 48 at 150%).
///
/// We use it because it is **exactly 2:1** with `SM_CXSMICON` (the title-bar size), while
/// `window_icon` can only hand back **one** size: so the large size maps 1:1 unscaled and the
/// small size is a clean 2:1 downscale (a 2:1 bitmap downscale is exactly the next-smaller
/// original redrawn).
#[cfg(windows)]
pub fn system_icon_size() -> i32 {
    const SM_CXICON: i32 = 11;
    #[link(name = "user32")]
    extern "system" {
        fn GetSystemMetrics(index: i32) -> i32;
    }
    // SAFETY: take a system metric; no side effects, no output parameters.
    unsafe { GetSystemMetrics(SM_CXICON) }
}

#[cfg(not(windows))]
pub fn system_icon_size() -> i32 {
    32
}

/// `DWMWA_BORDER_COLOR`
pub(crate) const DWMWA_BORDER_COLOR: u32 = 34;
/// `DWMWA_CAPTION_COLOR`
pub(crate) const DWMWA_CAPTION_COLOR: u32 = 35;
/// `DWMWA_TEXT_COLOR`
pub(crate) const DWMWA_TEXT_COLOR: u32 = 36;
/// `DWMWA_WINDOW_CORNER_PREFERENCE` (used together with `DWMWCP_*`)
pub(crate) const DWMWA_CORNER_PREFERENCE: u32 = 33;
/// `DWMWCP_ROUND`: the system standard rounded corners. `DONOTROUND` = 1, `ROUND` = 2, `ROUNDSMALL` = 3.
pub(crate) const DWMWCP_ROUND: i32 = 2;

/// Dye the native title bar to a color **identical** to the UI, so it melts into the window.
///
/// `sync_native_theme` can only make the title bar pick one of "system light / system dark", yielding DWM own gray
/// (~`#F3F3F3` light, ~`#202020` dark) that does not match the warm paper-and-ink tones.
/// To truly fit, we must set the DWM attributes directly:
///
/// - `DWMWA_CAPTION_COLOR`(35): title-bar background
/// - `DWMWA_TEXT_COLOR`(36): title-bar text color
/// - `DWMWA_BORDER_COLOR`(34): window outer border

pub fn sync_caption(handle: &dyn raw_window_handle::HasWindowHandle) {
    use raw_window_handle::RawWindowHandle;

    let Ok(h) = handle.window_handle() else { return };
    let RawWindowHandle::Win32(w) = h.as_raw() else {
        return;
    };
    let hwnd = w.hwnd.get();

    // Note DWM wants a COLORREF (0x00BBGGRR), which differs in channel order from Color32's RGBA, so we must reorder.
    let colorref = |c: Color32| -> u32 {
        ((c.b() as u32) << 16) | ((c.g() as u32) << 8) | c.r() as u32
    };

    // Both the title bar and the border take the UI base color: once the top bar was removed, the conversation area base color is the whole window base color,
    // so using panel for the title bar would leave a glaring color mismatch (user asked for "no color difference" on 2026-10-04).
    // The border is flattened too — otherwise a ring of "another shade of gray" would remain around the window.
    // (Using panel was the choice when the top bar still existed: top bar and title bar sharing a color read as one piece. With the top bar gone, follow bg.)
    let cap = colorref(P::bg());
    let text = colorref(P::text_dim());
    let border = colorref(P::bg());
    // Failure is harmless (old systems don't support it), so we just drop the return value.
    dwm_set_attribute(hwnd, DWMWA_CAPTION_COLOR, std::ptr::addr_of!(cap).cast(), 4);
    dwm_set_attribute(hwnd, DWMWA_TEXT_COLOR, std::ptr::addr_of!(text).cast(), 4);
    dwm_set_attribute(hwnd, DWMWA_BORDER_COLOR, std::ptr::addr_of!(border).cast(), 4);
    // The icon at the top-left of the title bar is **left alone**: it is now the `assets/aox.ico` from the exe resources
    // (see the comment around `with_icon(IconData::default())` in `main.rs`).
    // We used to call `hide_caption_icon(hwnd)` here to swap in a 1x1 transparent image (user asked for "title bar shows only the system buttons" on 2026-10-04); on
    // 2026-10-06 the user changed their mind and wanted the app icon shown, so we removed it —
    // worth noting: that approach later stopped working anyway, because eframe installs its own default icon (a white "e" on black) and
    // **afterwards** overrides the HICON we set, so the title bar ended up showing eframe mark.
}

#[cfg(not(windows))]
pub fn sync_caption(_handle: &dyn raw_window_handle::HasWindowHandle) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole logic of the "follow system" option is this truth table.
    ///
    /// It gets its own test because it's the easiest thing to write as a single `_ => Mode::Dark`
    /// and call it done — then "follow system" would always show dark even when the system is
    /// light, and **reading the code gives no hint**: the branch is there, the assignment is
    /// correct, only the basis for the decision is wrong.
    #[test]
    fn effective_follows_the_system_only_when_asked_to() {
        use egui::Theme;
        assert_eq!(effective(Pref::Light, Some(Theme::Dark)), Mode::Light, "once light is picked it stays light");
        assert_eq!(effective(Pref::Dark, Some(Theme::Light)), Mode::Dark, "once dark is picked it stays dark");
        assert_eq!(effective(Pref::System, Some(Theme::Light)), Mode::Light);
        assert_eq!(effective(Pref::System, Some(Theme::Dark)), Mode::Dark);
        // Backend reports no system theme: fall back to dark (matches the pre-option default), not random or panic.
        assert_eq!(effective(Pref::System, None), Mode::Dark);
    }

    #[test]
    fn pref_parsing_accepts_the_three_forms_and_rejects_junk() {
        assert_eq!(parse_pref("light"), Some(Pref::Light));
        assert_eq!(parse_pref("  DARK \n"), Some(Pref::Dark));
        assert_eq!(parse_pref("System"), Some(Pref::System));
        assert_eq!(parse_pref("paper"), None);
        assert_eq!(parse_pref(""), None);
    }

    /// What is persisted is the **preference**, not the active mode.
    ///
    /// This pins down a very sneaky regression: writing the active mode would erase "follow system" after the first time it follows —
    /// the system being light right now writes `"light"`, and on next launch it never follows again.
    /// So here we assert on `Pref::key()`, not on `mode()`.
    #[test]
    fn the_persisted_string_is_the_preference_not_the_effective_mode() {
        assert_eq!(Pref::System.key(), "system");
        assert_eq!(parse_pref(Pref::System.key()), Some(Pref::System));
        for p in Pref::ALL {
            assert_eq!(parse_pref(p.key()), Some(p), "{p:?} must read back after persisting");
        }
    }
}

