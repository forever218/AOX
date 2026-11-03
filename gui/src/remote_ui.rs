//! 设置页里的「远程」：配对列表、启停、日志。
//!
//! 桥跑在**独立线程**里（自己的 tokio runtime + 自己的 Runtime），不占 GUI 的 agent 线程。
//! 锁屏不依赖这里的状态，而是靠桥写的心跳文件——所以即使这个页面没打开，接管锁照样工作。
//!
//! 页面是「状态 → 列表 → 添加」三段，行内只放**该行才有的动作**：设为当前 / 重命名 / 删除。
//! 重命名和删除的确认放在列表上方而不是弹窗里，原因是这一页本身就在一个滚动区域里，
//! 再套一层 Modal 会同时挡住用户想对照的另外几行。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use egui::RichText;
use tokio::sync::mpsc::UnboundedReceiver;

use aox::remote::store::Book;
use aox::remote::{self, RemoteEvent};

use crate::theme::P;

enum Act {
    Select(String),
    AskDelete(String),
    StartRename(String),
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
                if let Err(e) = aox::remote::serve_with(runtime, root, Some(tx.clone()), stop).await
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
        self.status_block(ui);
        ui.add_space(12.0);
        self.rename_block(ui);
        self.delete_block(ui);
        self.list_block(ui);
        ui.add_space(10.0);
        self.add_block(ui);
        if !self.logs.is_empty() {
            ui.add_space(10.0);
            self.log_block(ui);
        }
    }

    fn status_block(&mut self, ui: &mut egui::Ui) {
        let active = self.book.active().map(|p| (p.server.clone(), p.agent_handle.clone()));
        ui.horizontal(|ui| {
            let (dot, color) = if self.running {
                ("●", P::add_green())
            } else {
                ("○", P::text_faint())
            };
            ui.label(RichText::new(dot).color(color).size(13.0));
            ui.label(
                RichText::new(if self.running { "运行中" } else { "未运行" })
                    .color(P::text())
                    .size(14.0),
            );
            if let Some((server, agent)) = &active {
                ui.label(RichText::new(format!("{server} · @{agent}")).color(P::text_dim()).size(13.0));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.running {
                    if ui.button(aox::i18n::t("Stop")).clicked() {
                        self.stop();
                    }
                } else if ui
                    .add_enabled(active.is_some(), egui::Button::new(aox::i18n::t("Start")))
                    .clicked()
                {
                    self.start();
                }
            });
        });
        ui.add_space(2.0);
        ui.label(RichText::new(&self.status).color(P::text_faint()).size(12.0));
    }

    fn rename_block(&mut self, ui: &mut egui::Ui) {
        let Some((id, draft)) = self.renaming.clone() else { return };
        let mut draft = draft;
        let mut commit = false;
        let mut cancel = false;

        egui::Frame::NONE
            .fill(P::surface())
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(aox::i18n::t("Name")).color(P::text_faint()).size(12.5));
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut draft)
                            .hint_text(aox::i18n::t("e.g. Office server"))
                            .desired_width(240.0),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        commit = true;
                    }
                    if ui.button(aox::i18n::t("Save")).clicked() {
                        commit = true;
                    }
                    if ui.button(aox::i18n::t("Cancel")).clicked() {
                        cancel = true;
                    }
                });
            });
        ui.add_space(6.0);

        if cancel {
            self.renaming = None;
        } else if commit {
            self.book.rename(&id, &draft);
            self.persist();
            self.renaming = None;
        } else {
            self.renaming = Some((id, draft));
        }
    }

    fn delete_block(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.deleting.clone() else { return };
        let name = self
            .book
            .pairings
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.title().to_string())
            .unwrap_or_default();
        let mut go = false;
        let mut cancel = false;

        egui::Frame::NONE
            .fill(P::surface())
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(aox::tf!("删除「{}」？只删本机凭证，服务器上的记录还在。", name))
                            .color(P::text_dim())
                            .size(12.5),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(RichText::new(aox::i18n::t("Delete")).color(P::danger())).clicked()
                        {
                            go = true;
                        }
                        if ui.button(aox::i18n::t("Cancel")).clicked() {
                            cancel = true;
                        }
                    });
                });
            });
        ui.add_space(6.0);

        if go {
            self.book.remove(&id);
            self.persist();
            self.deleting = None;
            self.reload();
        } else if cancel {
            self.deleting = None;
        }
    }

    fn list_block(&mut self, ui: &mut egui::Ui) {
        if self.book.pairings.is_empty() {
            ui.label(
                RichText::new(aox::i18n::t(
                    "No computer is paired yet. Generate a code on the phone, then add it here.",
                ))
                .color(P::text_faint())
                .size(12.5),
            );
            return;
        }

        let mut act: Option<Act> = None;
        let active_id = self.book.active.clone();
        let rows: Vec<(String, String, String, String, String, bool)> = self
            .book
            .pairings
            .iter()
            .map(|p| {
                (
                    p.id.clone(),
                    p.title().to_string(),
                    p.server.clone(),
                    p.agent_handle.clone(),
                    p.device_name.clone(),
                    // 标题就是地址时不重复显示地址
                    p.label.trim().is_empty(),
                )
            })
            .collect();

        for (id, title, server, agent, device, title_is_server) in rows {
            let is_active = active_id.as_deref() == Some(id.as_str());
            let row = egui::Frame::NONE
                .fill(if is_active { P::surface() } else { P::panel() })
                .stroke(egui::Stroke::new(1.0, P::border()))
                .corner_radius(egui::CornerRadius::same(6))
                .inner_margin(egui::Margin::symmetric(10, 8));

            row.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if is_active { "✓" } else { "○" })
                            .color(if is_active { P::add_green() } else { P::text_faint() })
                            .size(13.0),
                    );
                    ui.label(RichText::new(&title).color(P::text()).size(14.0));
                    if !title_is_server {
                        ui.label(RichText::new(&server).color(P::text_faint()).size(12.0));
                    }
                    ui.label(RichText::new(format!("@{agent}")).color(P::text_dim()).size(12.5));
                    ui.label(RichText::new(&device).color(P::text_faint()).size(12.0));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(aox::i18n::t("Delete")).clicked() {
                            act = Some(Act::AskDelete(id.clone()));
                        }
                        if ui.small_button(aox::i18n::t("Rename")).clicked() {
                            act = Some(Act::StartRename(id.clone()));
                        }
                        if !is_active && ui.small_button(aox::i18n::t("Use")).clicked() {
                            act = Some(Act::Select(id.clone()));
                        }
                    });
                });
            });
            ui.add_space(6.0);
        }

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

    fn add_block(&mut self, ui: &mut egui::Ui) {
        if !self.adding {
            if ui.button(aox::i18n::t("+ Add pairing")).clicked() {
                self.adding = true;
            }
            return;
        }

        egui::Frame::NONE
            .fill(P::surface())
            .stroke(egui::Stroke::new(1.0, P::border()))
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(aox::i18n::t(
                        "On the phone: Settings → Add computer. Type the 8-character code here within five minutes.",
                    ))
                    .color(P::text_faint())
                    .size(12.0),
                );
                ui.add_space(8.0);
                egui::Grid::new("remote-add").num_columns(2).spacing([8.0, 8.0]).show(ui, |ui| {
                    ui.label(RichText::new(aox::i18n::t("Server")).color(P::text_faint()).size(12.5));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.server)
                            .hint_text("https://chat.example.com")
                            .desired_width(300.0),
                    );
                    ui.end_row();
                    ui.label(RichText::new(aox::i18n::t("Pairing code")).color(P::text_faint()).size(12.5));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.code)
                            .hint_text(aox::i18n::t("8 characters"))
                            .desired_width(300.0),
                    );
                    ui.end_row();
                    ui.label(RichText::new(aox::i18n::t("Agent name")).color(P::text_faint()).size(12.5));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.handle)
                            .hint_text("aox-ran")
                            .desired_width(300.0),
                    );
                    ui.end_row();
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let ready = !self.server.trim().is_empty()
                        && !self.code.trim().is_empty()
                        && !self.handle.trim().is_empty()
                        && !self.busy;
                    if ui
                        .add_enabled(ready, egui::Button::new(aox::i18n::t("Pair")))
                        .clicked()
                    {
                        self.pair();
                    }
                    if ui.button(aox::i18n::t("Cancel")).clicked() {
                        self.adding = false;
                    }
                });
            });
    }

    fn log_block(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(
            RichText::new(aox::i18n::t("Recent log")).color(P::text_dim()).size(12.5),
        )
        .default_open(false)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for line in &self.logs {
                        ui.label(
                            RichText::new(line)
                                .family(egui::FontFamily::Monospace)
                                .size(11.5)
                                .color(P::text_faint()),
                        );
                    }
                });
        });
    }
}
