//! 设置页里的「远程」：配对列表、启停、日志。
//!
//! 桥跑在**独立线程**里（自己的 tokio runtime + 自己的 Runtime），不占 GUI 的 agent 线程。
//! 锁屏不依赖这里的状态，而是靠桥写的心跳文件——所以即使这个页面没打开，接管锁照样工作。
//!
//! 版式跟设置页其它列共用一套词汇：一个操作一行的高度（[`BTN_H`]）、整行当点击区、悬停时
//! 整行铺一层 `surface_hi`、动作是文字按钮，**不画边框卡片**。
//!
//! 之前这一页用的是 egui 原生的 `Button` / `small_button`：24pt 的灰盒子，悬停高亮只有文字
//! 那么大一圈，而整页其它地方是"整行变色 + 手型光标"。同一屏里出现两套交互语言，看起来就像
//! 嵌了个别的程序——这不是配色问题，是几何问题。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use egui::{Align, CornerRadius, FontId, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use tokio::sync::mpsc::UnboundedReceiver;

use aox::i18n::t;
use aox::remote::store::Book;
use aox::remote::{self, RemoteEvent};

use crate::app::{
    button, button_ex, button_size, danger_button, elide, flat_button, paint_button, Weight, BTN_H,
};
use crate::model_ui::paint_check;
use crate::theme::{self, P};

/// 配对行的高度。
///
/// 和模型列表的 `ROW_H` 一样：名字（14）在上、副标题（11.5）在下，上下各 11。行高固定，
/// 同一列里两行差 4pt 就会看出来。
const ROW_H: f32 = 56.0;

/// 行内动作按钮的高度：比整行低一档，两边各留 11，不顶着行的上下沿。
const ACT_H: f32 = BTN_H.min(ROW_H - 12.0);

/// 整列的左沿。名字、标题、空态、表单都用同一个数。
const PAD_L: f32 = 16.0;

enum Act {
    Select(String),
    AskDelete(String),
    StartRename(String),
}

/// 一个行内的编辑动作。就地改名 / 就地确认删除共用。
enum EditAct {
    None,
    Save,
    Cancel,
}

pub struct RemoteState {
    pub status: String,
    pub logs: Vec<String>,
    pub running: bool,
    busy: bool,
    stop: Arc<AtomicBool>,
    rx: Option<UnboundedReceiver<RemoteEvent>>,
    book: Book,

    adding: bool,
    server: String,
    code: String,
    handle: String,

    renaming: Option<(String, String)>,
    deleting: Option<String>,
    log_open: bool,
}

impl Default for RemoteState {
    fn default() -> Self {
        let book = Book::load(&aox::paths::app_root()).unwrap_or(Book {
            version: 2,
            active: None,
            pairings: Vec::new(),
        });
        Self {
            status: if book.pairings.is_empty() { "未配对".into() } else { "已配对，未启动".into() },
            logs: Vec::new(),
            running: false,
            busy: false,
            stop: Arc::new(AtomicBool::new(false)),
            rx: None,
            book,
            adding: false,
            server: String::new(),
            code: String::new(),
            handle: String::new(),
            renaming: None,
            deleting: None,
            log_open: false,
        }
    }
}

impl RemoteState {
    fn reload(&mut self) {
        if let Ok(b) = Book::load(&aox::paths::app_root()) {
            self.book = b;
        }
    }

    fn persist(&mut self) {
        if let Err(e) = self.book.save(&aox::paths::app_root()) {
            self.status = format!("保存失败：{e:#}");
        }
    }

    pub fn poll(&mut self) {
        let Some(rx) = self.rx.as_mut() else { return };
        let mut got = false;
        while let Ok(ev) = rx.try_recv() {
            got = true;
            match ev {
                RemoteEvent::Status(s) => {
                    let stopped = s.contains("已停止") || s.contains("失败") || s.contains("错误");
                    if stopped {
                        self.running = false;
                    }
                    // 配对成功就把表单收起来、把码擦掉：留着它只会让人以为还能再配一次，
                    // 而那个 8 位码用过就废了。
                    if s.contains("配对成功") {
                        self.adding = false;
                        self.server.clear();
                        self.code.clear();
                        self.handle.clear();
                    }
                    self.logs.push(format!("· {s}"));
                    self.status = s;
                }
                RemoteEvent::Log(s) => self.logs.push(s),
            }
        }
        if got {
            self.busy = false;
            // 配对成功会写进 remote.json，列表要跟着变。
            self.reload();
        }
        if self.logs.len() > 80 {
            let cut = self.logs.len() - 80;
            self.logs.drain(0..cut);
        }
    }

    pub fn start(&mut self) {
        if self.running || self.busy {
            return;
        }
        if self.book.active().is_none() {
            self.status = "先选一条配对".into();
            return;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.rx = Some(rx);
        self.stop = Arc::new(AtomicBool::new(false));
        let stop = self.stop.clone();
        self.running = true;
        self.status = "启动中…".into();

        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(RemoteEvent::Log(format!("无法创建运行时：{e}")));
                    return;
                }
            };
            rt.block_on(async move {
                let root = aox::paths::app_root();
                let cfg_path = root.join("config.toml");
                let resolved = match aox::config::layer::resolve(&root, &cfg_path, None, None) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(RemoteEvent::Status(format!("已停止（配置错误：{e:#}）")));
                        return;
                    }
                };
                let runtime =
                    match aox::bootstrap::build(resolved.config, root.clone(), root.join("plugins"))
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            let _ =
                                tx.send(RemoteEvent::Status(format!("已停止（运行时构建失败：{e:#}）")));
                            return;
                        }
                    };
                // GUI 没有终端输入可接管：`control_stdin = false`。授权在手机上点。
                if let Err(e) =
                    aox::remote::serve_with(runtime, root, Some(tx.clone()), stop, false).await
                {
                    let _ = tx.send(RemoteEvent::Status(format!("已停止（{e:#}）")));
                }
            });
        });
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.status = "正在停止…".into();
    }

    pub fn pair(&mut self) {
        if self.busy {
            return;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.rx = Some(rx);
        self.busy = true;
        self.status = "配对中…".into();

        let server = self.server.clone();
        let code = self.code.clone();
        let handle = self.handle.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(RemoteEvent::Status(format!("配对失败：{e}")));
                    return;
                }
            };
            let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "aox-gui".into());
            let root = aox::paths::app_root();
            match rt.block_on(remote::pair_with(&server, &code, &handle, &host, &root)) {
                Ok(p) => {
                    let _ = tx.send(RemoteEvent::Status(format!(
                        "配对成功：{} @{}",
                        p.server, p.agent_handle
                    )));
                }
                Err(e) => {
                    let _ = tx.send(RemoteEvent::Status(format!("配对失败：{e:#}")));
                }
            }
        });
    }

    // ── 页面 ────────────────────────────────────────────────────────────

    pub fn page(&mut self, ui: &mut egui::Ui) {
        // 设置对话框把整棵子树的横向间距清成 0（它自己是手写版式，靠 `add_space` 排），
        // 但这一页全是用 egui 控件拼的：不还原的话圆点和"未运行"会贴在一起，状态和地址也会
        // 粘成一串。
        ui.spacing_mut().item_spacing.x = 8.0;
        self.status_block(ui);
        ui.add_space(20.0);
        self.pairings_block(ui);
        if !self.logs.is_empty() {
            ui.add_space(18.0);
            self.log_block(ui);
        }
    }

    /// 顶部：一个状态点、一句话、右边一个动作按钮。
    ///
    /// 状态点**画**出来而不是打 `●` / `○`：这两个字形不在自带字体里时会变成豆腐块，而圆一定
    /// 画得出来（勾、叉、折角也都是这个理由）。
    fn status_block(&mut self, ui: &mut egui::Ui) {
        let active = self.book.active().map(|p| (p.server.clone(), p.agent_handle.clone()));
        let running = self.running;
        let can_start = active.is_some() && !self.busy;

        ui.horizontal(|ui| {
            // 按钮**先**占住右边，剩下的宽度才留给文字。
            //
            // 反过来写（先文字、再用 `right_to_left` 收按钮）时，文字是照它的自然宽度排的，
            // 一个长地址就能把按钮顶出画面，或者干脆压在上面。
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if running {
                    if button(ui, t("Stop remote"), Weight::Secondary).clicked() {
                        self.stop();
                    }
                } else if button_ex(ui, t("Start remote"), Weight::Primary, can_start).clicked() {
                    self.start();
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
                    if running {
                        ui.painter().circle_filled(dot.center(), 4.0, P::add_green());
                    } else {
                        ui.painter().circle_stroke(
                            dot.center(),
                            3.5,
                            Stroke::new(1.4, P::border_strong()),
                        );
                    }
                    ui.label(
                        RichText::new(if running { t("Running") } else { t("Not running") })
                            .color(P::text())
                            .size(14.5),
                    );
                    if let Some((server, agent)) = &active {
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("{server} · @{agent}"))
                                    .color(P::text_faint())
                                    .size(12.0),
                            )
                            .truncate(),
                        );
                    }
                });
            });
        });

        // 状态原文（"已停止（…）"、"配对中…"）。只占一行，超出省略：它是解释不是正文，折成
        // 两行会把列表整体推下去，而它每帧都可能变。
        if !self.status.is_empty() {
            ui.add_space(3.0);
            ui.add(
                egui::Label::new(RichText::new(&self.status).color(P::text_faint()).size(11.5))
                    .truncate(),
            );
        }
    }

    /// 配对列表：标题 + 行 +（添加中的）表单。
    ///
    /// 三件事挤在同一列而不是各自成块，因为它们回答同一个问题——"这台机器接进了哪几个账号"。
    /// 重命名和删除**就地**变成那一行，不再到列表上方另开一块：点了第三行的"重命名"，输入框
    /// 却在列表顶上，眼睛得先离开那一行。
    fn pairings_block(&mut self, ui: &mut egui::Ui) {
        let mut add_clicked = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(t("Pairings")).color(P::text_faint()).size(12.5));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !self.adding && flat_button(ui, t("+ Add pairing")).clicked() {
                    add_clicked = true;
                }
            });
        });
        if add_clicked {
            self.adding = true;
            self.renaming = None;
            self.deleting = None;
        }

        if self.adding {
            ui.add_space(2.0);
            self.add_form(ui);
            ui.add_space(4.0);
        }

        if self.book.pairings.is_empty() {
            if !self.adding {
                ui.add_space(2.0);
                ui.label(
                    RichText::new(t(
                        "No computer is paired yet. Generate a code on the phone, then add it here.",
                    ))
                    .color(P::text_dim())
                    .size(13.0),
                );
            }
            return;
        }

        ui.add_space(4.0);

        let rows: Vec<Row> = self
            .book
            .pairings
            .iter()
            .map(|p| Row {
                id: p.id.clone(),
                title: p.title().to_string(),
                server: p.server.clone(),
                agent: p.agent_handle.clone(),
                device: p.device_name.clone(),
                title_is_server: p.label.trim().is_empty(),
            })
            .collect();

        let active_id = self.book.active.clone();
        // 草稿借出来改，走完一遍再放回：编辑中的输入框要 `&mut String`，而它还得跟行的顺序对上。
        let mut renaming = self.renaming.clone();
        let mut deleting = self.deleting.clone();
        let mut act: Option<Act> = None;

        for r in &rows {
            let is_active = active_id.as_deref() == Some(r.id.as_str());

            if renaming.as_ref().is_some_and(|(id, _)| id == &r.id) {
                let mut draft = renaming.as_ref().map(|(_, d)| d.clone()).unwrap_or_default();
                match rename_row(ui, &mut draft) {
                    EditAct::Save => {
                        self.book.rename(&r.id, &draft);
                        self.persist();
                        renaming = None;
                    }
                    EditAct::Cancel => renaming = None,
                    // 每帧写回：`TextEdit` 的字符串得活过这一帧，光标才跟手。
                    EditAct::None => renaming = Some((r.id.clone(), draft)),
                }
                continue;
            }

            if deleting.as_deref() == Some(r.id.as_str()) {
                match delete_row(ui, &r.title) {
                    EditAct::Save => {
                        self.book.remove(&r.id);
                        self.persist();
                        self.reload();
                        deleting = None;
                    }
                    EditAct::Cancel => deleting = None,
                    EditAct::None => {}
                }
                continue;
            }

            if let Some(a) = row_ui(ui, r, is_active) {
                act = Some(a);
            }
        }

        // 本帧的编辑状态先落盘，再处理点击——点击是"另一个动作"，优先于正在编辑的那一行。
        self.renaming = renaming;
        self.deleting = deleting;

        match act {
            Some(Act::Select(id)) => {
                if self.book.select(&id) {
                    self.persist();
                    self.status = "已切换当前配对（下次启动生效）".into();
                }
            }
            Some(Act::AskDelete(id)) => {
                self.deleting = Some(id);
                self.renaming = None;
            }
            Some(Act::StartRename(id)) => {
                let current = self
                    .book
                    .pairings
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.label.clone())
                    .unwrap_or_default();
                self.renaming = Some((id, current));
                self.deleting = None;
            }
            None => {}
        }
    }

    fn add_form(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new(t(
                "On the phone: Settings → Add computer. Type the 8-character code here within five minutes.",
            ))
            .color(P::text_faint())
            .size(12.0),
        );
        ui.add_space(8.0);

        // 名字列按实测最宽的那一个算，不是写死一个数：英文比中文宽 40%，写死就会压到值上。
        let keys = [t("Server"), t("Pairing code"), t("Agent name")];
        let kw = key_w(ui, &keys);
        let _ = field_row(ui, kw, t("Server"), &mut self.server, "https://chat.example.com");
        let _ = field_row(ui, kw, t("Pairing code"), &mut self.code, t("8 characters"));
        let _ = field_row(ui, kw, t("Agent name"), &mut self.handle, "aox-ran");

        ui.add_space(10.0);
        let ready = !self.server.trim().is_empty()
            && !self.code.trim().is_empty()
            && !self.handle.trim().is_empty()
            && !self.busy;
        let mut pair_now = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            if button_ex(ui, t("Pair"), Weight::Primary, ready).clicked() {
                pair_now = true;
            }
            if flat_button(ui, t("Cancel")).clicked() {
                cancel = true;
            }
        });
        if pair_now {
            self.pair();
        }
        if cancel {
            self.adding = false;
        }
    }

    /// 最近日志：一行可点的折角标题 + 一个 well 里的等宽正文。
    ///
    /// 不用 `CollapsingHeader`：它自带 egui 的三角和缩进，字号也不跟这一页走，展开时整块会
    /// 往右缩 8pt——一个零件另一套度量，和上面"悬停只有文字大"是同一类问题。
    fn log_block(&mut self, ui: &mut egui::Ui) {
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 30.0), Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let fg = if resp.hovered() { P::text() } else { P::text_dim() };
        let g = ui.painter().layout_no_wrap(t("Recent log").to_owned(), FontId::proportional(12.5), fg);
        let tw = g.size().x;
        ui.painter().galley(Pos2::new(rect.left(), rect.center().y - g.size().y * 0.5), g, fg);

        // 折角：开着朝下，关着朝右。两条线段，不赌字体里有没有 ▸。
        let c = Pos2::new(rect.left() + tw + 12.0, rect.center().y);
        let s = 3.2;
        let st = Stroke::new(1.3, fg);
        if self.log_open {
            ui.painter().line_segment([c + Vec2::new(-s, -s * 0.5), c + Vec2::new(0.0, s * 0.7)], st);
            ui.painter().line_segment([c + Vec2::new(0.0, s * 0.7), c + Vec2::new(s, -s * 0.5)], st);
        } else {
            ui.painter().line_segment([c + Vec2::new(-s * 0.5, -s), c + Vec2::new(s * 0.7, 0.0)], st);
            ui.painter().line_segment([c + Vec2::new(s * 0.7, 0.0), c + Vec2::new(-s * 0.5, s)], st);
        }
        if resp.clicked() {
            self.log_open = !self.log_open;
        }

        if self.log_open {
            ui.add_space(4.0);
            theme::well(theme::R1).inner_margin(Margin::symmetric(12, 10)).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(160.0).auto_shrink([false, true]).show(
                    ui,
                    |ui| {
                        for line in &self.logs {
                            ui.label(
                                RichText::new(line)
                                    .font(theme::mono_font(11.5))
                                    .color(P::text_faint()),
                            );
                        }
                    },
                );
            });
        }
    }
}

/// 一行配对。
struct Row {
    id: String,
    title: String,
    server: String,
    agent: String,
    device: String,
    /// 标题就是地址（没起名字）时，副标题不再重复地址。
    title_is_server: bool,
}

/// 画一行配对，返回这一帧发生的动作。
///
/// 整行（除右侧动作区）是**点击区**，点了就等于"设为当前"——和模型列表里"整行点了就编辑"是
/// 同一条规则：手型光标落在哪，哪就能点，而不用为它再画一个按钮。
fn row_ui(ui: &mut egui::Ui, r: &Row, active: bool) -> Option<Act> {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::hover());

    // 右侧动作区的宽度**先算出来**，不管这一帧画不画：算进去之后名字的省略宽度才是常数，
    // 否则鼠标一进一出，名字会先缩短再弹回。
    let del_w = button_size(ui, t("Delete"), Weight::Ghost).x;
    let ren_w = button_size(ui, t("Rename"), Weight::Ghost).x;
    let reserved = del_w + ren_w + 10.0;

    // 左半边可点（= 设为当前）。已经是的就不给 sense，免得点出一个"什么也没发生"。
    let can_use = !active;
    let hit = Rect::from_min_max(rect.min, Pos2::new(rect.right() - reserved, rect.max.y));
    let hresp = ui.interact(
        hit,
        egui::Id::new(("aox-pairing", &r.id)),
        if can_use { Sense::click() } else { Sense::hover() },
    );

    // 两个动作的点击区**每帧都建**，只是不悬停时不画。
    //
    // 反过来做（悬停才建）看着更省，实际点不中：egui 的 `hovered` 是按**上一帧**的控件表算
    // 的，指针一落到按钮上，父行这一帧就可能报 `hovered = false` → 按钮被"撤掉" → 点击落在
    // 空处，按钮原地闪烁。模型列表那几个子控件也是无条件的，同一个理由。
    let mut right = rect.right() - PAD_L;
    let mut acts: Vec<(Rect, &'static str, egui::Response, fn(String) -> Act)> = Vec::new();
    for (label, bw, make) in [
        (t("Delete"), del_w, Act::AskDelete as fn(String) -> Act),
        (t("Rename"), ren_w, Act::StartRename as fn(String) -> Act),
    ] {
        let br = Rect::from_center_size(
            Pos2::new(right - bw * 0.5, rect.center().y),
            Vec2::new(bw, ACT_H),
        );
        let bresp = ui.interact(br, egui::Id::new(("aox-pairing-act", &r.id, label)), Sense::click());
        acts.push((br, label, bresp, make));
        right -= bw + 4.0;
    }

    // 整行的悬停态：行本身或它上面的任何一个子点击区，都算"指针在这一行里"。
    let hov = resp.hovered() || hresp.hovered() || acts.iter().any(|(_, _, r, _)| r.hovered());
    if hov {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), P::surface_hi());
    }

    let mut act = None;
    for (br, label, bresp, make) in &acts {
        if !hov {
            continue;
        }
        if bresp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        paint_button(ui, *br, label, Weight::Ghost, bresp.hovered());
        if bresp.clicked() {
            act = Some(make(r.id.clone()));
        }
    }
    if can_use && hresp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // 整行可点这件事得说出来：列表里只有手型光标，没有第二个提示。
    let hresp = if can_use { hresp.on_hover_text(t("Set as current")) } else { hresp };

    let name_fg = if active { P::ink() } else { P::text() };
    let check_w = if active { 20.0 } else { 0.0 };
    let text_w = (rect.right() - reserved - PAD_L - rect.left() - check_w - 6.0).max(80.0);
    let name_x = rect.left() + PAD_L;
    let g = elide(ui, &r.title, FontId::proportional(14.0), name_fg, text_w);
    let name_w = g.size().x;
    ui.painter().galley(Pos2::new(name_x, rect.top() + 11.0), g, name_fg);
    if active {
        paint_check(
            ui.painter(),
            Rect::from_center_size(
                Pos2::new(name_x + name_w + 12.0, rect.top() + 19.0),
                Vec2::splat(12.0),
            ),
        );
    }

    let sub = if r.title_is_server {
        format!("@{} · {}", r.agent, r.device)
    } else {
        format!("{} · @{} · {}", r.server, r.agent, r.device)
    };
    let sg = elide(ui, &sub, FontId::proportional(11.5), P::text_faint(), text_w);
    ui.painter().galley(Pos2::new(name_x, rect.top() + 32.0), sg, P::text_faint());

    if act.is_none() && hresp.clicked() {
        act = Some(Act::Select(r.id.clone()));
    }
    act
}

/// 就地重命名：一个输入框 + 保存 / 取消，高度和一行一样，列表不跳。
fn rename_row(ui: &mut egui::Ui, draft: &mut String) -> EditAct {
    let mut out = EditAct::None;
    let w = ui.available_width();
    egui::Frame::NONE
        .fill(P::surface_hi())
        .corner_radius(CornerRadius::same(theme::R1))
        .inner_margin(Margin::symmetric(PAD_L as i8, 11))
        .show(ui, |ui| {
            ui.set_min_width(w - PAD_L * 2.0);
            ui.horizontal(|ui| {
                // 输入框按"这一行还剩多少"来定宽，不写死：写死 240 时，列一窄（窗口缩小）
                // 保存 / 取消就压到框上。上限 280 是为了别让一个改名框横穿整列。
                let bw = button_size(ui, t("Save"), Weight::Primary).x
                    + button_size(ui, t("Cancel"), Weight::Ghost).x
                    + 8.0;
                let fw = (ui.available_width() - bw - 12.0).clamp(120.0, 280.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(draft)
                        .desired_width(fw)
                        .margin(Margin { left: 10, right: 10, top: 8, bottom: 8 })
                        .font(FontId::proportional(14.0))
                        .hint_text(
                            RichText::new(t("e.g. Office server")).color(P::text_faint()).size(13.0),
                        ),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    out = EditAct::Save;
                }
                // 打开就落在输入框里：点了"重命名"还得再点一次输入框才能打字，那这一步就是白点的。
                // 只在**没有任何东西**有焦点时要，免得每帧跟别的输入框抢。
                if ui.memory(|m| m.focused()).is_none() {
                    resp.request_focus();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button(ui, t("Save"), Weight::Primary).clicked() {
                        out = EditAct::Save;
                    }
                    if flat_button(ui, t("Cancel")).clicked() {
                        out = EditAct::Cancel;
                    }
                });
            });
        });
    out
}

/// 就地确认删除：一句话 + 删除 / 取消。红色只出现在这一步，且是实心的——它已经是第二下了。
fn delete_row(ui: &mut egui::Ui, title: &str) -> EditAct {
    let mut out = EditAct::None;
    let w = ui.available_width();
    egui::Frame::NONE
        .fill(P::surface_hi())
        .corner_radius(CornerRadius::same(theme::R1))
        .inner_margin(Margin::symmetric(PAD_L as i8, 11))
        .show(ui, |ui| {
            ui.set_min_width(w - PAD_L * 2.0);
            ui.horizontal(|ui| {
                // 按钮宽度先算出来，那句话只能用**剩下**的宽度：不这样的话长标题会直接压到
                // 按钮上（"服务器上的记录还在"就是从 Cancel 底下穿过去的）。
                let bw = button_size(ui, t("Delete"), Weight::DangerSolid).x
                    + button_size(ui, t("Cancel"), Weight::Ghost).x
                    + 8.0;
                let tw = (ui.available_width() - bw - 12.0).max(80.0);
                ui.add_sized(
                    Vec2::new(tw, BTN_H),
                    egui::Label::new(
                        RichText::new(aox::tf!("删除「{}」？只删本机凭证，服务器上的记录还在。", title))
                            .color(P::text_dim())
                            .size(13.0),
                    )
                    .truncate(),
                );
                if danger_button(ui, t("Delete")).clicked() {
                    out = EditAct::Save;
                }
                if flat_button(ui, t("Cancel")).clicked() {
                    out = EditAct::Cancel;
                }
            });
        });
    out
}

/// 名字列宽度：这一组里实测最宽的那个名字 + 18pt 气口，再向上取整。
///
/// 照抄设置页 `kv_key_w` 的理由：名字是直接画在 painter 上的，既不裁剪也不参与布局，写死一个
/// 宽度只会在换语言时压到值上。
fn key_w(ui: &egui::Ui, keys: &[&str]) -> f32 {
    let mut w = 84.0_f32;
    for k in keys {
        let g = ui.painter().layout_no_wrap(
            (*k).to_owned(),
            FontId::proportional(13.0),
            P::text_dim(),
        );
        w = w.max(g.size().x + 18.0);
    }
    w.ceil()
}

/// 一行「名字 + 输入框」。输入框的内边距是 8，所以它和 [`BTN_H`] 一样高（14 的字 + 上下 8）。
fn field_row(
    ui: &mut egui::Ui,
    key_w: f32,
    label: &str,
    text: &mut String,
    hint: &str,
) -> egui::Response {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(key_w, BTN_H), Sense::hover());
        let g = ui.painter().layout_no_wrap(label.to_owned(), FontId::proportional(13.0), P::text_dim());
        ui.painter().galley(Pos2::new(r.left(), r.center().y - g.size().y * 0.5), g, P::text_dim());
        ui.add(
            egui::TextEdit::singleline(text)
                .desired_width(300.0)
                .margin(Margin { left: 10, right: 10, top: 8, bottom: 8 })
                .font(FontId::proportional(14.0))
                .hint_text(RichText::new(hint).color(P::text_faint()).size(13.0)),
        )
    })
    .inner
}
