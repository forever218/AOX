//! 工作区按钮：输入框**左下角**那枚「工作区 · <档位>」。
//!
//! 三档（默认 / 选择工作区 / 完全访问）在**界面这一层**的入口就在这里。配置里的
//! `[workplace]` 只管「不写就不变」（默认 `full`），真正表达「我这次想收紧」的地方
//! 是这枚按钮 —— 这也就是 `aox::workplace` 模块文档里那句「用户最初要的『每次启动
//! 默认受限』由 GUI 那一枚按钮去兑现」的兑现处。
//!
//! 本模块负责四件东西：
//! - [`button`]：那枚按钮本身（手绘文件夹图标 + 当前档位名）；
//! - [`menu`]：点击后**向上**弹出的三选一列表（当前档位打勾，「完全访问」红色加粗）；
//! - [`confirm`]：切「完全访问」前的红色二次确认 —— 它是整个程序里**唯一**一个
//!   「点下去就越过所有保护」的开关（所以只有它用得上 [`P::danger`]）；
//! - [`picker`]：内置的目录选择器。
//!
//! # 三条约定
//!
//! **① 状态住在 [`WorkplaceUi`] 里，由 `App` 持有。** 这个模块不碰 `App` 的任何字段。
//!
//! **② 「进参数、出请求」。** 每个函数返回 `Option<Mode>` 就是「界面要发一条请求给
//! Agent 线程，请求把档位切成这个」。为什么界面不能自己切档：`Workplace` 住在
//! `aox::runtime::Runtime` 里，而 `Runtime` 只有 Agent 那个线程拿得到 —— 主线程手里
//! 只有一条 channel（见 `gui/src/bridge.rs` 的 `ToAgent::Workplace`）。
//!
//! **③ 现状是「灌」进来的，不是「问」出来的。** 档位名与打勾靠 [`WorkplaceUi::set_view`]
//! 收到的 `WorkplaceView`（bridge 在启动 / 每轮结束 / 每次切档后重发）。界面自己
//! 记一份「现在选了哪档」是错的：真正说了算的状态在 Agent 线程那边，两处记必然对不上。
//!
//! # 关于目录选择器为什么自己画
//!
//! 没有调系统的目录选择框（Windows 上是 `IFileDialog`，要额外拉 COM + 一堆
//! `windows-sys` 接口，还得为 macOS / Linux 各写一份）。这个选择器的用途很窄 ——
//! 挑一个「工作区根」目录，不需要新建、不需要多选、不需要网络位置 —— 用 egui
//! 自绘一屏足够，而且**主题完全一致**，不会在纸墨界面里弹出一个系统的白色对话框。

use std::path::{Path, PathBuf};

use egui::{
    Align2, Color32, CornerRadius, Frame, Id, Margin, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2,
};

use aox::workplace::Mode;

use crate::app::{danger_button, flat_button, primary_button};
use crate::bridge::WorkplaceView;
use crate::theme::{self, P};

// ─────────────────────────── 状态 ───────────────────────────

/// 工作区按钮 + 它弹出来的那几个面板的全部状态。
pub struct WorkplaceUi {
    /// 当前档位与根（由 bridge 的 `FromAgent::Workplace` 灌进来）。
    ///
    /// `None` = 还没收到过回执（刚启动的头一两帧，或根本没能连上 Agent 线程）。
    /// 这时按钮显示朴素的「工作区」，列表里一项都不打勾 —— 不猜。
    view: Option<WorkplaceView>,
    /// 三选一列表是否展开。
    open: bool,
    /// 按钮的矩形。弹出面板要以它为锚点**向上**贴（见 [`menu`] 的坐标换算）。
    button_rect: Option<Rect>,
    /// 「完全访问」的二次确认是否打开。
    confirm_full: bool,
    /// 目录选择器是否打开。
    picker: Option<Picker>,
}

impl WorkplaceUi {
    pub fn new() -> Self {
        Self { view: None, open: false, button_rect: None, confirm_full: false, picker: None }
    }

    /// 收到一份「现状」。由 `App::pump` 在 `FromAgent::Workplace` 分支里调。
    pub fn set_view(&mut self, v: WorkplaceView) {
        self.view = Some(v);
    }

    /// 按钮上那行字。
    ///
    /// 「选择工作区」档位不直接念档位名（那会变成「工作区 · 选择工作区」，读着像结巴），
    /// 而是显示**选中的那个目录名** —— 用户关心的是「我现在被关在哪块地儿」，
    /// 完整路径放在弹出的列表里。
    fn chip_text(&self) -> String {
        match &self.view {
            Some(v) => match &v.mode {
                Mode::Default => "工作区 · 默认".to_string(),
                Mode::Fixed(p) => format!("工作区 · {}", dir_name(p)),
                Mode::Full => "工作区 · 完全访问".to_string(),
            },
            None => "工作区".to_string(),
        }
    }
}

impl Default for WorkplaceUi {
    fn default() -> Self {
        Self::new()
    }
}

/// 目录选择器的状态。
struct Picker {
    /// 当前正在浏览的目录。
    dir: PathBuf,
    /// 本机存在哪些盘符（仅 Windows；其它平台为空 —— 从 `/` 走进就行）。
    drives: Vec<char>,
    /// 上一次读目录的报错（权限不足之类）。
    error: Option<String>,
    /// 当前目录下的子目录。**缓存**，只在打开 / 切目录时重读 ——
    /// 不然每帧一次 `read_dir`，一个 60fps 的界面每秒白读 60 遍磁盘。
    subdirs: Vec<PathBuf>,
}

impl Picker {
    fn open(dir: PathBuf) -> Self {
        let mut p = Self { dir, drives: Vec::new(), error: None, subdirs: Vec::new() };
        p.drives = existing_drives();
        p.reload();
        p
    }

    fn reload(&mut self) {
        self.subdirs.clear();
        self.error = None;
        match std::fs::read_dir(&self.dir) {
            Ok(rd) => {
                for entry in rd.flatten() {
                    let path = entry.path();
                    // 只看目录：这个选择器要挑的是「一块地」，文件是噪音。
                    // 顺手跳过 `.` 与 `..`（`read_dir` 本来也不给，但别指望它）。
                    if path.is_dir() {
                        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                            if name.starts_with('.') {
                                continue;
                            }
                        }
                        self.subdirs.push(path);
                    }
                }
                // 大小写不敏感地按名字排：Windows 的 `read_dir` 顺序是文件系统给的，
                // 看着是乱的。
                self.subdirs.sort_by(|a, b| {
                    let (a, b) = (dir_name(a).to_lowercase(), dir_name(b).to_lowercase());
                    a.cmp(&b).then_with(|| a.cmp(&b))
                });
            }
            Err(e) => self.error = Some(format!("{e}")),
        }
    }

    fn goto(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.reload();
    }
}

// ─────────────────────────── 按钮 ───────────────────────────

/// 画那枚按钮（在输入框左下角）。
///
/// 只做两件事：把当前档位名画出来、把热区记进 `st.button_rect`（弹出面板要用它定位）。
/// 点击在这里就地翻转 `st.open` —— 面板本身由 [`menu`] 画（它要一个独立 `Area`，
/// 不能困在输入框那张卡片的裁切区里）。
pub fn button(ui: &mut Ui, st: &mut WorkplaceUi, enabled: bool) {
    let text = st.chip_text();
    let font = egui::FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(text, font, Color32::PLACEHOLDER);

    let pad = Vec2::new(8.0, 4.0);
    let icon = 14.0;
    let size = Vec2::new(icon + 6.0 + galley.size().x + pad.x * 2.0, galley.size().y.max(icon) + pad.y * 2.0);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);

    let hov = enabled && resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    let radius = CornerRadius::same(8);
    painter.rect_filled(
        rect,
        radius,
        if hov { P::surface_hi() } else { Color32::TRANSPARENT },
    );
    // 只在悬停时描边：常态下它就贴在输入卡片里，多一圈边框会让左下角显得吵。
    if hov {
        painter.rect_stroke(rect, radius, Stroke::new(1.0, P::border()), egui::StrokeKind::Inside);
    }

    let fg = if enabled { P::text_dim() } else { P::text_faint() };
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + pad.x + icon * 0.5, rect.center().y),
        Vec2::new(icon, icon * 0.82),
    );
    paint_folder(painter, icon_rect, fg);
    painter.galley(Pos2::new(rect.left() + pad.x + icon + 6.0, rect.center().y - galley.size().y * 0.5), galley, fg);

    if resp.clicked() && enabled {
        st.open = !st.open;
        // 开列表时把另两个面板收掉：一屏上同时冒三个框没人受得了。
        st.confirm_full = false;
        st.picker = None;
    }
    st.button_rect = Some(rect);
}

// ─────────────────────────── 三选一列表 ───────────────────────────

/// 画向上弹出的三选一列表（`st.open` 为假时什么都不做）。
pub fn menu(ctx: &egui::Context, st: &mut WorkplaceUi) -> Option<Mode> {
    if !st.open {
        return None;
    }
    let Some(btn) = st.button_rect else {
        st.open = false;
        return None;
    };

    // 锚点定在按钮**左上角再往上 6px**，配合 `pivot(LEFT_BOTTOM)` 就是「面板底边
    // 压着按钮顶边」—— 于是它是往上长的，不会被窗口底边截断。
    //
    // ⚠️ **坐标不要做任何换算。** `Area::fixed_pos` 与 `ui.allocate_*` 给出来的矩形
    // 是**同一套坐标**（客户区局部，原点在客户区左上角）—— egui 自己在 `Area::begin`
    // 里就是拿 `ctx.content_rect()`（= 客户区矩形）当默认约束矩形的。早先这里按顶栏
    // 那枚 token 胶囊的写法加过一次「视口原点」（`viewport().inner_rect.left_top()`），
    // 那是**屏幕（显示器）坐标**，而窗口不在显示器原点时它非零 —— 实机一测面板就整体
    // 偏了 ~(154, 14) 逻辑像素。
    let anchor = btn.left_top() - Vec2::new(0.0, 6.0);

    let mut req = None;
    let cur = st.view.as_ref().map(|v| v.mode.clone());

    let area = egui::Area::new(Id::new("aox-workplace-menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor)
        .pivot(Align2::LEFT_BOTTOM)
        .show(ctx, |ui| {
            Frame::NONE
                .fill(P::surface())
                .corner_radius(CornerRadius::same(10))
                .stroke(Stroke::new(1.0, P::border()))
                .shadow(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: P::shadow() })
                .inner_margin(Margin::symmetric(6, 6))
                .show(ui, |ui| {
                    ui.set_width(276.0);

                    // ① 默认
                    if row(
                        ui,
                        "默认",
                        "每段新对话在 workplace/ 下开一块新地",
                        matches!(cur, Some(Mode::Default)),
                        false,
                    )
                    .clicked()
                    {
                        req = Some(Mode::Default);
                    }

                    // ② 选择工作区…（打开目录选择器，本身不是一次切档）
                    //
                    // 不带说明行（2026-10-04 用户要求「只留『默认』那条说明」）：
                    // 当前选的是哪个目录，按钮上本来就写着「工作区 · <目录名>」，
                    // 在菜单里再重复一遍只会把菜单撑高。
                    let fixed_now = matches!(cur, Some(Mode::Fixed(_)));
                    if row(ui, "选择工作区…", "", fixed_now, false).clicked() {
                        st.open = false;
                        // 打开时就落在当前根上（若已经是固定档），否则落在应用根。
                        let start = st
                            .view
                            .as_ref()
                            .and_then(|v| v.root.clone())
                            .unwrap_or_else(|| aox::paths::app_root());
                        st.picker = Some(Picker::open(start));
                    }

                    // ③ 完全访问（唯一的危险项：红色 + 加粗）
                    //    同样不带说明行 —— 红色与加粗本身就是这里的说明。
                    if row(ui, "完全访问", "", matches!(cur, Some(Mode::Full)), true).clicked() {
                        st.open = false;
                        st.confirm_full = true;
                    }
                });
        })
        .response;

    // 点面板外面、或按 Esc → 收起。
    //（与上面同一个坐标空间，所以能直接比 —— 不用再减任何原点。）
    let panel = area.rect;
    let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    let clicked_outside = ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .is_some_and(|p| !panel.contains(p) && !btn.contains(p))
    });
    if esc || clicked_outside {
        st.open = false;
    }
    if req.is_some() {
        st.open = false;
    }
    req
}

/// 列表里的一行：左侧勾选位 + 标题 + 一行小字说明。
///
/// 手绘的理由与 `app.rs` 里的 `text_button` 一样：**先铺底、再画字**，悬停换色时
/// 不用重排文字，也不会出现「底色盖住文字」那种老问题。
fn row(ui: &mut Ui, label: &str, hint: &str, checked: bool, danger: bool) -> egui::Response {
    let width = ui.available_width();
    // 两行（标题 + 说明）与一行（只有标题）两种高度：菜单里**只有「默认」还带说明**
    // （2026-10-04 用户要求删掉其余两项的说明文字）。单项行若照旧按 46 高排，
    // 那行字会浮在上半截，看着像没画完。
    let two_line = !hint.is_empty();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, if two_line { 46.0 } else { 32.0 }), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(8), P::surface_hi());
    }

    // 勾：当前档位才画。位置固定占好，没打勾的行也不会让标题左移。
    if checked {
        let color = if danger { P::danger() } else { P::ink() };
        paint_check(painter, Pos2::new(rect.left() + 17.0, rect.center().y), 5.0, color);
    }

    let text_x = rect.left() + 34.0;
    let label_color = if danger { P::danger() } else { P::text() };
    let label_font = if danger {
        theme::bold_font(15.0)
    } else {
        egui::FontId::proportional(15.0)
    };
    let g = painter.layout_no_wrap(label.to_owned(), label_font, label_color);
    // 两行时标题靠上（下面给说明留位），一行时整体居中。
    let label_y = if two_line { rect.top() + 7.0 } else { rect.center().y - g.size().y * 0.5 };
    painter.galley(Pos2::new(text_x, label_y), g, label_color);

    if two_line {
        let hg = painter.layout_no_wrap(hint.to_owned(), egui::FontId::proportional(12.0), P::text_faint());
        painter.galley(Pos2::new(text_x, rect.top() + 27.0), hg, P::text_faint());
    }

    resp
}

/// 目录选择器里的一行子目录：手绘文件夹图标 + 名字，整行可点。
///
/// 不用 emoji（📁 之类的字形要看系统字体有没有，缺了就是一个豆腐块），
/// 复用按钮上那枚 [`paint_folder`]——图标语汇全套只有一处实现。
fn dir_row(ui: &mut Ui, name: &str) -> egui::Response {
    let width = ui.available_width();
    const HEIGHT: f32 = 26.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, HEIGHT), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(6), P::surface_hi());
    }
    let icon = Rect::from_center_size(Pos2::new(rect.left() + 13.0, rect.center().y), Vec2::new(13.0, 11.0));
    paint_folder(painter, icon, P::text_faint());
    let g = painter.layout_no_wrap(name.to_owned(), egui::FontId::proportional(13.5), P::text_dim());
    painter.galley(
        Pos2::new(rect.left() + 28.0, rect.center().y - g.size().y * 0.5),
        g,
        P::text_dim(),
    );
    resp
}

// ─────────────────────────── 完全访问：二次确认 ───────────────────────────

/// 切「完全访问」前的红色确认框。
pub fn confirm(ctx: &egui::Context, st: &mut WorkplaceUi) -> Option<Mode> {
    if !st.confirm_full {
        return None;
    }
    let mut req = None;
    let mut keep = true;

    let resp = egui::Modal::new(Id::new("aox-workplace-confirm"))
        .frame(
            Frame::NONE
                .fill(P::surface())
                .corner_radius(CornerRadius::same(theme::card_radius()))
                .stroke(Stroke::new(1.0, P::border()))
                .inner_margin(Margin::symmetric(26, 22)),
        )
        .show(ctx, |ui| {
            ui.set_width(430.0);
            // 标题用危险色 + 加粗：这一屏就是要让人停一下。
            ui.label(RichText::new("切换到「完全访问」？").color(P::danger()).size(17.0).strong());
            ui.add_space(10.0);
            ui.label(
                RichText::new("AOX 将可以读写这台设备上的任何文件，工作区边界不再拦截任何路径。")
                    .color(P::text_dim())
                    .size(14.0),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("这是整个程序里唯一一个「点下去就越过所有保护」的开关。")
                    .color(P::text_faint())
                    .size(13.0),
            );
            ui.add_space(20.0);
            ui.horizontal(|ui| {
                if danger_button(ui, "确认切换").clicked() {
                    req = Some(Mode::Full);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if flat_button(ui, "取消").clicked() {
                        keep = false;
                    }
                });
            });
        });

    // 点背景 / Esc = 取消（`should_close` 是 egui::Modal 给的标准出口）
    if resp.should_close() {
        keep = false;
    }
    if req.is_some() || !keep {
        st.confirm_full = false;
    }
    req
}

// ─────────────────────────── 目录选择器 ───────────────────────────

/// 内置目录选择器。选定返回 `Set(Mode::Fixed(<目录>))` —— 与 CLI 的
/// `/workplace <目录>` 是同一条落点（都走 `Workplace::set_mode`）。
///
/// 这里**不挑文件、不新建目录**：这个入口的用途只有一个 —— 挑一块「工作区根」。
/// 需要新建目录的用户在资源管理器里建好再进来选，比在这里塞一个新建入口清爽。
pub fn picker(ctx: &egui::Context, st: &mut WorkplaceUi) -> Option<Mode> {
    let Some(p) = st.picker.as_mut() else {
        return None;
    };
    let mut chosen: Option<PathBuf> = None;
    let mut close = false;

    let resp = egui::Modal::new(Id::new("aox-workplace-picker"))
        .frame(
            Frame::NONE
                .fill(P::surface())
                .corner_radius(CornerRadius::same(theme::card_radius()))
                .stroke(Stroke::new(1.0, P::border()))
                .inner_margin(Margin::symmetric(22, 18)),
        )
        .show(ctx, |ui| {
            ui.set_width(520.0);

            ui.horizontal(|ui| {
                ui.label(RichText::new("选择工作区目录").color(P::text()).size(16.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if flat_button(ui, "取消").clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(12.0);

            // 当前路径：等宽 + 过长时中间省略（保留头部盘符与尾部目录名 —— 信息量最大的两头）
            Frame::NONE
                .fill(P::code_bg())
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(10, 7))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(elide_path(&p.dir.display().to_string(), 62))
                            .font(theme::mono_font(13.0))
                            .color(P::text_dim()),
                    );
                });

            // 盘符（Windows）：只列出真的存在的，26 次 `is_dir` 已经读过一遍了。
            if !p.drives.is_empty() {
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    for d in p.drives.clone() {
                        let here = p.dir.to_string_lossy().to_ascii_uppercase().starts_with(&format!("{d}:"));
                        let text = format!("{d}:");
                        let color = if here { P::ink() } else { P::text_dim() };
                        let resp = ui.add(
                            egui::Label::new(RichText::new(text).size(13.0).color(color))
                                .sense(Sense::click()),
                        );
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.clicked() {
                            p.goto(PathBuf::from(format!("{d}:\\")));
                        }
                        ui.add_space(4.0);
                    }
                });
            }

            ui.add_space(10.0);

            // 上级目录（用 ASCII 的 `..`，不赌字体里有没有箭头字形）
            if let Some(parent) = p.dir.parent().map(Path::to_path_buf) {
                let resp = ui.add(
                    egui::Label::new(RichText::new("..  上级目录").size(13.5).color(P::text_dim()))
                        .sense(Sense::click()),
                );
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    p.goto(parent);
                }
            } else {
                ui.label(RichText::new("（已经是顶层）").size(13.0).color(P::text_faint()));
            }

            ui.add_space(6.0);

            // 子目录列表
            if let Some(e) = &p.error {
                ui.label(RichText::new(format!("无法读取此目录：{e}")).color(P::text()).size(13.5));
            } else if p.subdirs.is_empty() {
                ui.label(RichText::new("（此目录下没有子目录）").size(13.0).color(P::text_faint()));
            } else {
                let subs = p.subdirs.clone();
                let mut goto: Option<PathBuf> = None;
                egui::ScrollArea::vertical().max_height(248.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for sub in &subs {
                        if dir_row(ui, &dir_name(sub)).clicked() {
                            goto = Some(sub.clone());
                        }
                    }
                });
                if let Some(d) = goto {
                    p.goto(d);
                }
            }

            ui.add_space(18.0);
            // 按钮落在**右下角**（用户 2026-10-04 提的）：`right_to_left` 的子布局会吃掉
            // 剩下的横向宽度，于是先放进去的那个控件就贴在最右边；说明文字留在同一行的左侧。
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("选定后，写入与删除都被限制在这个目录里（读取不受限）。")
                        .color(P::text_faint())
                        .size(12.5),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary_button(ui, "选定此目录").clicked() {
                        chosen = Some(p.dir.clone());
                    }
                });
            });
        });

    if resp.should_close() {
        close = true;
    }
    // `p` 到这里就不再用了 —— 下面可以直接改 `st`。
    if let Some(dir) = chosen {
        st.picker = None;
        return Some(Mode::Fixed(dir));
    }
    if close {
        st.picker = None;
    }
    None
}

// ─────────────────────────── 小工具 ───────────────────────────

/// 目录的显示名（取最后一段；没有就退回去掉末尾分隔符的整串）。
fn dir_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// 过长路径居中省略：保留头（盘符）与尾（当前目录名）—— 中间那段对辨认没帮助。
fn elide_path(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let head = keep / 3;
    let tail = keep - head;
    let h: String = chars[..head].iter().collect();
    let t: String = chars[chars.len() - tail..].iter().collect();
    format!("{h}…{t}")
}

/// 本机存在的盘符（只 Windows 有意义）。
fn existing_drives() -> Vec<char> {
    if !cfg!(windows) {
        return Vec::new();
    }
    ('A'..='Z').filter(|c| Path::new(&format!("{c}:\\")).is_dir()).collect()
}

/// 勾。
fn paint_check(painter: &egui::Painter, center: Pos2, s: f32, color: Color32) {
    let stroke = Stroke::new(1.8, color);
    painter.line_segment(
        [Pos2::new(center.x - s, center.y + s * 0.05), Pos2::new(center.x - s * 0.2, center.y + s * 0.8)],
        stroke,
    );
    painter.line_segment(
        [Pos2::new(center.x - s * 0.2, center.y + s * 0.8), Pos2::new(center.x + s * 0.9, center.y - s * 0.75)],
        stroke,
    );
}

/// 文件夹图标：一个「页签 + 主体」的实心剪影。
///
/// 用**实心**而不是描边：13px 见方的描边文件夹在里面那条页签横线上会糊成一团，
/// 实心剪影在两种主题下都干净。
fn paint_folder(painter: &egui::Painter, rect: Rect, color: Color32) {
    let tab = Rect::from_min_max(
        Pos2::new(rect.left(), rect.top() + rect.height() * 0.10),
        Pos2::new(rect.left() + rect.width() * 0.46, rect.top() + rect.height() * 0.34),
    );
    let body = Rect::from_min_max(
        Pos2::new(rect.left(), rect.top() + rect.height() * 0.28),
        Pos2::new(rect.right(), rect.bottom()),
    );
    painter.rect_filled(tab, CornerRadius::same(1), color);
    painter.rect_filled(body, CornerRadius::same(2), color);
}
