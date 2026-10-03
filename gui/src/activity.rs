//! 活动流：住在**独立窗口**里（egui 的 viewport），不在主窗口的右侧栏。
//!
//! 为什么要共享状态：eframe 的 `AppCreator` 是 `FnOnce`，每个视口一份 `App` 实例，
//! 子窗口拿不到主窗口 `App` 的 `&mut self`。所以活动流要用的东西全放在
//! [`Activity`] 里，用 `Arc<Mutex<_>>` 两边共享 —— 主窗口只管往里写，
//! 子窗口只管读和渲染。
//!
//! 窗口是个**无系统装饰的顶层窗口**（就是「小 AOX」，初始尺寸与主窗口一致）。
//! 也就是说 Windows 那条自带的白底（深色下是黑底）标题栏 —— 图标、标题、
//! 最小化 / 最大化 / 关闭 —— 一律不显示，整条顶栏由这里自绘。
//!
//! 代价是系统那几样现成的东西也没了，得自己补：
//! - **拖动**：顶栏按住即发 [`ViewportCommand::StartDrag`]。
//! - **缩放**：四边四角各埋一条拖拽热区（[`resize_handles`]），按下时把方向交给 winit。
//! - **最大化**：顶栏多一枚按钮（关掉系统栏后，双击标题栏、Win+↑ 这些也就没了）。
//! - **边界**：无装饰窗口和桌面之间没有分界线，所以最后描一圈一像素的边。
//! - **圆角**：不想自绘（透明视口会把系统阴影一起弄丢），而是让 DWM 裁 ——
//!   见 [`set_round_corners`]。半径与主窗口一致。
//!
//! # 主体：一条横向时间轴（甘特图）
//!
//! ```text
//!  0.0s      2.0s      4.0s      6.0s     8.0s
//!    │         │         │         │        │
//!  ①          ②
//! 输入 ▐███▌        ▐███▌
//! 模型      ▐█████▌        ▐███████████▌
//! filestat_count  ▐██▌
//! create_plugin        ▐███▌
//! 系统                ▐▌
//! ```
//!
//! - **横轴是时间（秒）**，任务自左向右推进；每一枚色块条就是一段真实耗时。
//! - 行是固定的两行「输入 / 模型」，之后**每个工具各占一行**，最后按需多一行「系统」。
//! - 横轴可以随意缩放（Ctrl+滚轮 / 工具栏 ±）、左右平移（拖动 / 滚轮）。
//! - 纵轴：工具行多到摊不下时**由纵向滚动接管**（Shift+滚轮或最右边的滚动条），
//!   行高本身也能拉（工具栏「行高 −/＋」），不会为了塞进一屏把行压到看不清。
//! - 左上的锚点按钮把视图钉在「起点 / 居中 / 最新」；「轮次」可以直接跳到第 N 轮。
//! - 打开「连接线」后，各条色块按真实执行顺序被一根线首尾相连，线色取自出发色块。
//!
//! **空闲不记录**：只有一轮任务执行期间才会往这里写东西。程序闲着的时候，
//! 时间轴上一个像素都不长。
//!
//! **记录随对话走**：每一轮结束，这一轮新增的色块会被交给 Agent 线程追加进
//! **这段对话自己的那个 `history/*.jsonl`**（`kind:"act"` 行，见 `aox::history`）。
//! 于是「打开一段旧对话」能看到它当时的时间轴，「删掉这段对话」时间轴也跟着没了 ——
//! 不需要任何额外的清理逻辑。也正因为如此，这里**没有「清空」按钮**：清界面不清文件，
//! 下次打开它又长回来，那才是真的说不清。

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use std::collections::VecDeque;

use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Frame, Galley, Layout, Margin, Pos2,
    Rect, ResizeDirection, RichText, ScrollArea, Sense, Stroke, TextEdit, TextFormat, Ui, Vec2,
    ViewportBuilder, ViewportCommand, ViewportId, WindowLevel,
};
use serde::{Deserialize, Serialize};

use crate::app::default_window_size;
use crate::theme::{self, P};

// ────────────────────────────── 类别 ──────────────────────────────

/// 记录类别。带 `Serialize` 是为了落盘（见 [`Activity::export_acts`]）：
/// 存进对话文件的那一份用 `snake_case` 的名字（`input` / `tool` …），
/// 而不是 Rust 里的枚举名 —— 文件是给人看也给别的工具看的。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActKind {
    /// 用户下达的指令：一轮任务的起点
    Input,
    /// 模型侧的活动（思考 + 产出）
    Model,
    /// 一次工具调用（调用 → 执行 → 返回是同一段）
    Tool,
    /// 授权请求 / 结果
    Auth,
    Info,
    Warn,
    Error,
}

impl ActKind {
    /// 徽标上的名字
    pub fn label(self) -> &'static str {
        match self {
            ActKind::Input => "输入",
            ActKind::Model => "模型",
            ActKind::Tool => "工具",
            ActKind::Auth => "授权",
            ActKind::Info => "信息",
            ActKind::Warn => "警告",
            ActKind::Error => "错误",
        }
    }

    /// 色块基色。低饱和暖色系，与代码高亮同一套语言：
    /// 输入是灰、模型是紫、工具是橙，其余（授权/提示/异常）落在暖黄与砖红上。
    fn color(self) -> Color32 {
        match theme::mode() {
            theme::Mode::Light => match self {
                ActKind::Input => Color32::from_rgb(0x6E, 0x69, 0x5E),
                ActKind::Model => Color32::from_rgb(0x76, 0x57, 0x9C),
                ActKind::Tool => Color32::from_rgb(0xB3, 0x74, 0x22),
                ActKind::Auth => Color32::from_rgb(0x9A, 0x6C, 0x1C),
                ActKind::Info => Color32::from_rgb(0x77, 0x71, 0x64),
                ActKind::Warn => Color32::from_rgb(0xA8, 0x76, 0x20),
                ActKind::Error => Color32::from_rgb(0xA3, 0x4A, 0x2A),
            },
            theme::Mode::Dark => match self {
                ActKind::Input => Color32::from_rgb(0x8E, 0x8A, 0x80),
                ActKind::Model => Color32::from_rgb(0xB2, 0x93, 0xD6),
                ActKind::Tool => Color32::from_rgb(0xD3, 0x94, 0x5A),
                ActKind::Auth => Color32::from_rgb(0xD8, 0xB2, 0x6A),
                ActKind::Info => Color32::from_rgb(0x7E, 0x79, 0x6E),
                ActKind::Warn => Color32::from_rgb(0xD8, 0xB2, 0x6A),
                ActKind::Error => Color32::from_rgb(0xE0, 0x8A, 0x6B),
            },
        }
    }

    /// 工具调用失败 / 错误事件的砖红（覆盖类别色）
    fn failed_color() -> Color32 {
        match theme::mode() {
            theme::Mode::Light => Color32::from_rgb(0xA3, 0x4A, 0x2A),
            theme::Mode::Dark => Color32::from_rgb(0xE0, 0x8A, 0x6B),
        }
    }

    /// 正文用等宽（参数、结果、错误都是代码或数据）
    fn mono(self) -> bool {
        matches!(self, ActKind::Tool | ActKind::Auth | ActKind::Error)
    }

    /// 落在「系统」行
    fn is_sys(self) -> bool {
        matches!(self, ActKind::Auth | ActKind::Info | ActKind::Warn | ActKind::Error)
    }
}

/// 时间轴的行。前两行固定，工具各占一行（按首次出现的顺序），「系统」按需出现。
#[derive(Clone, PartialEq, Eq)]
enum Row {
    Input,
    Model,
    Tool(String),
    Sys,
}

impl Row {
    fn label(&self) -> &str {
        match self {
            Row::Input => "输入",
            Row::Model => "模型",
            Row::Tool(t) => t,
            Row::Sys => "系统",
        }
    }

    /// 行名的字体：工具名是标识符，用等宽；其余用正文字体
    fn dim(&self) -> bool {
        matches!(self, Row::Input | Row::Sys)
    }
}

/// 由当前记录推出行表。工具行的顺序 = 首次出现顺序，所以中途冒出新工具时，
/// 已有行不会重排（眼睛不会被跳来跳去的行带跑）。
fn rows_of(acts: &VecDeque<Act>) -> Vec<Row> {
    let mut rows = vec![Row::Input, Row::Model];
    for a in acts {
        if a.kind == ActKind::Tool && !rows.iter().any(|r| matches!(r, Row::Tool(n) if n == &a.title))
        {
            rows.push(Row::Tool(a.title.clone()));
        }
    }
    if acts.iter().any(|a| a.kind.is_sys()) {
        rows.push(Row::Sys);
    }
    rows
}

fn row_of(rows: &[Row], a: &Act) -> usize {
    match a.kind {
        ActKind::Input => 0,
        ActKind::Model => 1,
        ActKind::Tool => rows
            .iter()
            .position(|r| matches!(r, Row::Tool(n) if n == &a.title))
            .unwrap_or(rows.len().saturating_sub(1)),
        _ => rows.iter().position(|r| *r == Row::Sys).unwrap_or(rows.len().saturating_sub(1)),
    }
}

// ────────────────────────────── 数据 ──────────────────────────────

/// 活动流里的一条记录 = 时间轴上的一枚色块。
///
/// 带 serde 是因为它要**落盘**（存进对话文件，见 [`Activity::export_acts`]）：
/// 关掉程序再打开那段对话，时间轴要能原样长回来。落盘的是「相对那次会话开始时
/// 刻的秒数」，装回来时会整批平移（见 [`Activity::load_acts`]）。
#[derive(Clone, Serialize, Deserialize)]
pub struct Act {
    kind: ActKind,
    /// 属于第几轮（用户第 N 次下达指令）
    round: u32,
    /// 插件名（工具调用有）。同一插件的工具用同一族色的不同深浅。
    plugin: String,
    /// 起始时刻（秒，应用启动以来的绝对时间）
    at: f32,
    /// 结束时刻。`None` = 还在进行中，色块随时间实时变长。
    end: Option<f32>,
    failed: bool,
    title: String,
    body: String,
    /// 稳定序号：色块点击、详情面板都靠它
    seq: u64,
}

impl Act {
    /// 这一条在轴上的时长；仍在进行中就算到「现在」
    fn dur(&self, now: f32) -> f32 {
        (self.end.unwrap_or(now) - self.at).max(0.0)
    }

    /// 结束时刻；进行中的用 `now` 顶上
    fn end_t(&self, now: f32) -> f32 {
        self.end.unwrap_or(now).max(self.at)
    }

    fn open(&self) -> bool {
        self.end.is_none()
    }
}

/// 最多留多少条。超了从头丢：活动流是「近期过程」，不是日志归档。
const ACT_LIMIT: usize = 600;
/// 模型那一段的正文最多留这么多字节（只看尾部，前面截掉）
const NOTE_CAP: usize = 4000;
/// 装回存档时，让最后一条结束在「现在」之前多久（秒）。留一点空，新一轮
/// 才不会紧贴着旧记录长出来。
const START_GAP: f32 = 0.4;

/// 视图锚点：决定左边缘对齐到哪一刻
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Anchor {
    /// 起点固定在左侧
    Start,
    /// 「当前」固定在正中
    Center,
    /// 「当前」贴在最右侧（跟着任务走）
    #[default]
    Latest,
    /// 用户自己拖到的地方
    Free,
}

#[derive(Default)]
pub struct Activity {
    acts: VecDeque<Act>,
    next_seq: u64,
    /// 「已落盘」水位：`seq` 不大于它的记录已经在对话文件里了（见 `export_acts`）。
    flushed: u64,
    /// 时间原点：第一条记录的时刻。横轴上的 0.0s 就是它。
    t_zero: Option<f32>,
    /// 当前是第几轮
    round: u32,
    open_input: Option<u64>,
    open_model: Option<u64>,
    open_tool: Option<u64>,
    /// 用户点选的色块
    selected: Option<u64>,

    status: String,
    busy: bool,
    started: Option<Instant>,
    /// 主窗口的时间基准（秒），用来给「进行中」的色块算实时长度
    now: f32,

    open: bool,
    pinned: bool,
    applied_top: bool,
    theme_mode: Option<theme::Mode>,
    /// 尺寸与位置是否已经摆过一次（见 `sync` 里那段注释）
    placed: bool,
    /// 圆角是否已经交代过系统（同样是「一扇窗口只做一次」的事）
    corners: bool,
    /// 呼吸圆点用的相位
    spin: f32,

    // ── 时间轴视图 ──
    anchor: Anchor,
    /// 每秒多少像素。`None` = 还没手动调过，用默认（一屏 60 秒）
    scale: Option<f32>,
    /// `Anchor::Free` 时左边缘的时刻（相对 `t_zero`，秒）
    t0: f32,
    /// 是否画连接线
    link: bool,
    /// 行高：`None` = 按可用高度自动铺满（工具行多到摊不下时由纵向滚动接管）
    row_h: Option<f32>,
    /// 待执行的行高倍率（工具栏不知道怎么算「铺满」，和 `pending_zoom` 同理）
    pending_row: Option<f32>,
    /// 「按轮次查看」的输入框
    round_input: String,
    /// 待执行的跳转（工具栏排队，等时间轴拿到绘图宽度后再执行）
    goto: Option<u32>,
    /// 待执行的缩放倍率（同上：工具栏不知道绘图宽度）
    pending_zoom: Option<f32>,
    /// 待执行的「整段铺满」
    fit_all: bool,
}

impl Activity {
    /// 时间原点（相对时间 = 绝对时间 - t_zero）
    fn tz(&self) -> f32 {
        self.t_zero.unwrap_or(0.0)
    }

    fn next_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    fn find(&mut self, seq: u64) -> Option<&mut Act> {
        self.acts.iter_mut().rev().find(|a| a.seq == seq)
    }

    /// 记一条。返回它的序号。
    fn push(&mut self, mut a: Act) -> u64 {
        let seq = self.next_seq();
        a.seq = seq;
        if self.t_zero.is_none() {
            self.t_zero = Some(a.at);
        }
        self.acts.push_back(a);
        while self.acts.len() > ACT_LIMIT {
            self.acts.pop_front();
        }
        seq
    }

    /// 收掉某一条（只补结束时刻，不改别的）
    fn end_act(&mut self, seq: u64, at: f32) {
        if let Some(a) = self.find(seq) {
            if a.end.is_none() {
                a.end = Some(at.max(a.at));
            }
        }
    }

    fn close_input(&mut self, at: f32) {
        if let Some(seq) = self.open_input.take() {
            self.end_act(seq, at);
        }
    }

    fn close_model(&mut self, at: f32) {
        if let Some(seq) = self.open_model.take() {
            self.end_act(seq, at);
        }
    }

    /// 收掉所有还开着的段
    fn close_open(&mut self, at: f32) {
        self.close_input(at);
        self.close_model(at);
        if let Some(seq) = self.open_tool.take() {
            self.end_act(seq, at);
        }
    }

    // ── 写入（主窗口调用） ──

    /// 一轮开始：用户下达指令。「输入」这一段 = 从发出到模型真正开口，
    /// 也就是「等模型的时间」。
    pub fn round_begin(&mut self, text: &str, at: f32) {
        self.close_open(at);
        self.round += 1;
        let seq = self.push(Act {
            kind: ActKind::Input,
            round: self.round,
            plugin: String::new(),
            at,
            end: None,
            failed: false,
            title: "用户指令".to_owned(),
            body: text.to_owned(),
            seq: 0,
        });
        self.open_input = Some(seq);
    }

    /// 模型开始产出（第一段思考 / 正文）。也负责把「输入」那一段收掉。
    pub fn model_begin(&mut self, at: f32) {
        self.close_input(at);
        // 工具在跑的时候不可能是模型在说话（工具是串行的）
        if self.open_tool.is_some() || self.open_model.is_some() {
            return;
        }
        let seq = self.push(Act {
            kind: ActKind::Model,
            round: self.round,
            plugin: String::new(),
            at,
            end: None,
            failed: false,
            title: "模型产出".to_owned(),
            body: String::new(),
            seq: 0,
        });
        self.open_model = Some(seq);
    }

    /// 模型正在流式吐出的思考文本（只进详情面板，不影响时间轴）
    pub fn model_push(&mut self, chunk: &str) {
        let Some(seq) = self.open_model else { return };
        let Some(a) = self.find(seq) else { return };
        a.body.push_str(chunk);
        if a.body.len() > NOTE_CAP {
            // 截在字符边界上：按字节切会把一个汉字劈成两半，`[..]` 直接 panic
            let mut cut = a.body.len() - NOTE_CAP;
            while cut < a.body.len() && !a.body.is_char_boundary(cut) {
                cut += 1;
            }
            a.body = format!("…（前文省略）{}", &a.body[cut..]);
        }
    }

    /// 一次工具调用开始。「模型」那一段到此收尾。
    pub fn tool_begin(&mut self, tool: &str, plugin: Option<&str>, args: &str, at: f32) {
        self.close_input(at);
        self.close_model(at);
        if let Some(seq) = self.open_tool.take() {
            self.end_act(seq, at);
        }
        let seq = self.push(Act {
            kind: ActKind::Tool,
            round: self.round,
            plugin: plugin.unwrap_or("").to_owned(),
            at,
            end: None,
            failed: false,
            title: tool.to_owned(),
            body: args.to_owned(),
            seq: 0,
        });
        self.open_tool = Some(seq);
    }

    /// 工具调用期间追加一句（如「超时上限 30s」）
    pub fn tool_note(&mut self, note: &str) {
        let Some(seq) = self.open_tool else { return };
        if let Some(a) = self.find(seq) {
            if !a.body.trim().is_empty() {
                a.body.push('\n');
            }
            a.body.push_str("· ");
            a.body.push_str(note);
        }
    }

    /// 工具调用收尾：补上耗时、结果与成败
    pub fn tool_end(&mut self, is_error: bool, preview: &str, at: f32) {
        let Some(seq) = self.open_tool.take() else { return };
        if let Some(a) = self.find(seq) {
            a.end = Some(at.max(a.at));
            a.failed = is_error;
            if !preview.trim().is_empty() {
                if a.body.trim().is_empty() {
                    a.body = preview.to_owned();
                } else {
                    a.body.push_str("\n\n── 结果 ──\n");
                    a.body.push_str(preview);
                }
            }
        }
    }

    /// 系统事件（信息 / 警告 / 错误 / 授权）—— 瞬时的一条，落在「系统」行。
    /// **只在一轮任务执行期间记**：程序闲着时不往时间轴上写任何东西。
    pub fn note(&mut self, kind: ActKind, title: &str, body: &str, at: f32) {
        self.push(Act {
            kind,
            round: self.round,
            plugin: String::new(),
            at,
            end: Some(at),
            failed: false,
            title: title.to_owned(),
            body: body.to_owned(),
            seq: 0,
        });
    }

    /// 一轮结束：把还开着的段全收掉
    pub fn end_round(&mut self, at: f32) {
        self.close_open(at);
    }

    /// 每帧同步：时间基准 + 现在在干什么
    pub fn tick(&mut self, now: f32, status: &str, busy: bool) {
        if busy && !self.busy {
            self.started = Some(Instant::now());
        }
        if !busy {
            self.started = None;
        }
        self.busy = busy;
        self.now = now;
        self.status = status.to_owned();
    }

    /// 把时间轴清空（换对话时用）。
    ///
    /// **刻意不对外提供「清空」按钮**：活动流是这段对话记录的一部分，
    /// 删它只能通过删那段对话来做 —— 否则界面上清掉了、文件里还留着，
    /// 下次打开这段对话它又长回来，那才叫不可理解。
    pub fn reset(&mut self) {
        self.acts.clear();
        self.open_input = None;
        self.open_model = None;
        self.open_tool = None;
        self.selected = None;
        self.t_zero = None;
        self.flushed = 0;
        self.round = 0;
        self.anchor = Anchor::Latest;
        self.scale = None;
        self.t0 = 0.0;
        self.goto = None;
        self.pending_zoom = None;
        self.fit_all = false;
        self.row_h = None;
        self.pending_row = None;
        self.round_input.clear();
    }

    /// 取走还没落盘的那些记录（序列化成 JSON），并把水位推到最新。
    ///
    /// **按轮调用**：中途半截的段（还开着的色块）落下去就成了「没有结束时刻」的
    /// 残记录，中途回滚时也没法把它收回来。等一轮结束、所有段都闭上了再一次性给。
    pub fn export_acts(&mut self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for a in &self.acts {
            if a.seq > self.flushed {
                if let Ok(v) = serde_json::to_value(a) {
                    out.push(v);
                }
            }
        }
        if let Some(last) = self.acts.back() {
            self.flushed = self.flushed.max(last.seq);
        }
        out
    }

    /// 把一段存档对话的活动流装回来（切到那段对话时调）。
    ///
    /// `now` = **本次启动**以来的秒数。存档里的时刻是上一次启动的坐标系，直接装进来
    /// 会和现在的 `now` 差出一大截，新长出来的色块要么跑到视野外、要么把旧记录挤没。
    /// 所以整批平移：让最后一条结束在 `now` 稍前一点，接着聊时新一轮自然从右边继续。
    pub fn load_acts(&mut self, rows: &[serde_json::Value], now: f32) {
        self.reset();
        let acts: Vec<Act> = rows
            .iter()
            .filter_map(|v| serde_json::from_value::<Act>(v.clone()).ok())
            .collect();
        if acts.is_empty() {
            return;
        }
        let earliest = acts.iter().fold(f32::MAX, |m, a| m.min(a.at));
        let latest = acts.iter().fold(f32::MIN, |m, a| m.max(a.end.unwrap_or(a.at)));
        let shift = (now - START_GAP) - latest;

        let mut round = 0;
        let mut top_seq = 0;
        for mut a in acts {
            a.at += shift;
            a.end = a.end.map(|e| e + shift);
            round = round.max(a.round);
            top_seq = top_seq.max(a.seq);
            // 直接塞进队列（不走 `push`）：序号是存档里带的，重编会让
            // 「水位」和文件的对应关系错位。
            self.acts.push_back(a);
        }
        self.round = round;
        self.next_seq = top_seq;
        self.flushed = top_seq;
        // 坐标原点跟着这一批走：轴上的 0.0s = 存档里最早的那一刻
        self.t_zero = Some(earliest + shift);
        self.anchor = Anchor::Latest;
        if self.acts.len() > ACT_LIMIT {
            let drop = self.acts.len() - ACT_LIMIT;
            self.acts.drain(..drop);
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 「这扇窗口已经没了」。所有**一扇窗口只做一次**的初始化都要在这里作废，
    /// 下次再开才会重做 —— 现在有两样：摆尺寸位置（`placed`）、
    /// 交代系统画圆角（`corners`，圆角是挂在 HWND 上的，窗口没了就一起没了）。
    fn forget_placement(&mut self) {
        self.placed = false;
        self.corners = false;
    }

    /// 直接把窗口打开（演示模式预览用；正常路径走 [`toggle`]）。
    pub fn force_open(&mut self) {
        self.open = true;
        self.spin = 0.0;
        self.forget_placement();
    }

    /// 当前被选中的那条；没选就取最新一条
    fn focused(&self) -> Option<&Act> {
        match self.selected {
            Some(seq) => {
                self.acts.iter().rev().find(|a| a.seq == seq).or_else(|| self.acts.back())
            }
            None => self.acts.back(),
        }
    }

    /// 全部记录的时间范围（相对秒）
    fn span(&self) -> (f32, f32) {
        let tz = self.tz();
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for a in &self.acts {
            lo = lo.min(a.at - tz);
            hi = hi.max(a.end_t(self.now) - tz);
        }
        if lo > hi {
            (0.0, 1.0)
        } else {
            (lo, hi)
        }
    }

    /// 把视图对准第 `r` 轮
    fn fit_round(&mut self, r: u32, plot_w: f32) {
        let tz = self.tz();
        let now = self.now;
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        let mut first: Option<u64> = None;
        for a in &self.acts {
            if a.round != r {
                continue;
            }
            lo = lo.min(a.at - tz);
            hi = hi.max(a.end_t(now) - tz);
            if first.is_none() {
                first = Some(a.seq);
            }
        }
        if lo > hi {
            return;
        }
        let pad = ((hi - lo) * 0.10).clamp(0.25, 8.0);
        self.fit_range(lo - pad, hi + pad, plot_w);
        if let Some(seq) = first {
            self.selected = Some(seq);
        }
    }

    /// 让 `[lo, hi]` 这段铺满绘图区
    fn fit_range(&mut self, lo: f32, hi: f32, plot_w: f32) {
        let span = (hi - lo).clamp(0.2, 1.0e7);
        self.anchor = Anchor::Free;
        self.scale = Some((plot_w / span).clamp(MIN_SCALE, MAX_SCALE));
        self.t0 = lo;
    }
}

pub type Shared = Arc<Mutex<Activity>>;

pub fn new_shared() -> Shared {
    Arc::new(Mutex::new(Activity::default()))
}

/// 取锁。上一次 panic 会把锁毒死，这里直接 recovering —— 一个窗口崩了不该
/// 让整个程序的活动流一起死。
pub fn lock(a: &Shared) -> MutexGuard<'_, Activity> {
    a.lock().unwrap_or_else(|e| e.into_inner())
}

fn act_id() -> ViewportId {
    ViewportId::from_hash_of("aox-activity")
}

/// 窗口标题。两处在用：建视口时上报，以及事后按标题把 HWND 找回来设圆角
/// （见 [`set_round_corners`]），所以必须只有一份。
const WINDOW_TITLE: &str = "AOX 活动流";

// ─────────────────────────── 窗口的开关 ───────────────────────────

const MIN_W: f32 = 860.0;
const MIN_H: f32 = 520.0;

/// 顶栏那枚图标的开关。
pub fn toggle(ctx: &egui::Context, a: &Shared) {
    let mut st = lock(a);
    st.open = !st.open;
    if st.open {
        st.spin = 0.0;
        st.forget_placement(); // 重新开一个窗口要重新摆位
    }
    drop(st);
    ctx.request_repaint();
}

/// 主窗口每帧调一次。
///
/// egui 的视口是「声明式」的：**这一轮没有声明的子视口会被直接丢掉**。
/// 所以哪怕窗口正被最小化、没人碰它，也必须每帧重新声明一次。
pub fn sync(ctx: &egui::Context, a: &Shared) {
    let mut st = lock(a);
    if !st.open {
        return;
    }
    // 尺寸与位置**只在第一次声明时给**。
    //
    // eframe 每帧都会拿新 builder 去 `patch` 旧 builder，凡是有变化的字段就下发成
    // 命令（位置 → `OuterPosition`、尺寸 → `InnerSize`），个别字段甚至直接重建窗口。
    // 所以「每帧都报一遍位置」等于每帧把窗口拽回原处 —— 表现为拖不动、缩放不生效、
    // 置顶也丢。第二次起只报标题和 resizable 这类**恒定不变**的字段。
    let fresh = !st.placed;
    st.placed = true;

    let mut b = ViewportBuilder::default()
        .with_title(WINDOW_TITLE)
        .with_resizable(true)
        // **不要系统装饰**：留着它就是顶上多一条 Windows 自带的标题栏
        // （图标 + 标题 + 最小化/最大化/关闭），而我们顶栏里那套按钮是自绘的，
        // 于是变成两条栏叠着。关掉之后拖动 / 缩放 / 最大化全部由 `window()` 自己接管。
        .with_decorations(false);

    if fresh {
        let size = initial_size(ctx);
        b = b
            .with_inner_size([size.x, size.y])
            .with_min_inner_size([MIN_W, MIN_H])
            .with_position(initial_pos(ctx, size));
    }
    if st.pinned {
        b = b.with_window_level(WindowLevel::AlwaysOnTop);
    }

    let a2 = a.clone();
    drop(st);
    ctx.show_viewport_deferred(act_id(), b, move |ctx, _| window(ctx, &a2));
}

/// 初始尺寸 = **主窗口的默认尺寸**（同一个公式，见 `app::default_window_size`）。
///
/// `default_window_size` 要减掉「装饰占掉的那点厚度」（标题栏 + 边框），
/// 算出来的才是**内容区**的尺寸。而活动流窗口自己已经没有装饰了，
/// 它的 `outer - inner` 恒等于 0 —— 拿它算会凭空多出 46 点高度。
/// 所以这里必须去问**主窗口**要装饰厚度，两边的内容区才对得上。
fn initial_size(ctx: &egui::Context) -> Vec2 {
    let (mon, chrome) = ctx.input(|i| {
        // 所有视口的信息都在 `raw.viewports` 里（eframe 每帧把整张表灌进来）
        let root = i.raw.viewports.get(&ViewportId::ROOT);
        let mon = i.viewport().monitor_size.or_else(|| root.and_then(|v| v.monitor_size));
        let chrome = root
            .and_then(|v| Some(v.outer_rect?.size() - v.inner_rect?.size()))
            .filter(|c| c.x > 0.0 && c.y > 0.0)
            // 取不到（首帧、或平台不给）就用一个保守的估计值
            .unwrap_or(Vec2::new(18.0, 46.0));
        (mon, chrome)
    });
    let Some(mon) = mon else {
        return Vec2::new(1280.0, 800.0);
    };
    default_window_size(mon, chrome)
}

/// 初始位置：与主窗口**同一个显示器居中**。窗口和主窗口一样大时，往旁边摆必然
/// 摆不下，不如直接盖在中间、让用户自己挪。
///
/// 注意 `monitor_size` 给的是**尺寸**，egui 不暴露显示器的原点，所以只能按
/// 「显示器左上角 = 屏幕原点」来算 —— 主显示器上这是对的，副显示器上会退化成
/// 「摆到主显示器中间」，但至少不会被推到屏幕外面去。
fn initial_pos(ctx: &egui::Context, size: Vec2) -> Pos2 {
    let mon_size = ctx.input(|i| i.viewport().monitor_size).unwrap_or(Vec2::new(1920.0, 1080.0));
    let mon = Rect::from_min_size(Pos2::ZERO, mon_size);
    let clamp = |v: f32, lo: f32, hi: f32| v.clamp(lo, hi.max(lo));
    Pos2::new(
        clamp(mon.center().x - size.x / 2.0, 8.0, mon.max.x - size.x - 8.0),
        clamp(mon.center().y - size.y / 2.0, 8.0, mon.max.y - size.y - 8.0),
    )
}

// ═══════════════════════════ 布局刻度 ═══════════════════════════

const HEADER_H: f32 = 52.0;
/// 左侧行名那一栏的宽度（工具名可以很长，这一栏要留够，放不下再省略号）
const GUTTER: f32 = 182.0;
/// 顶部时间刻度带
const AXIS_H: f32 = 32.0;
/// 轮次刻度带
const ROUND_H: f32 = 24.0;
/// 行间距
const LANE_GAP: f32 = 8.0;
/// 右侧给竖向滚动条留的宽度（轴与行必须按同一个宽度对齐）
const RSV: f32 = 14.0;
/// 最短的一枚色块（瞬时事件的宽度下限，保证点得中）
const MIN_BAR_W: f32 = 6.0;
/// 缩放区间：像素 / 秒
const MIN_SCALE: f32 = 0.15;
const MAX_SCALE: f32 = 320.0;
/// 默认视野：一屏 60 秒
const DEFAULT_SPAN: f32 = 60.0;
/// 一行最矮多少像素。**这是纵向滚动的分界线**：工具行多到摊不下时不再继续把行
/// 压扁（压到十几个像素时行名和色块都没法看了），而是让内容溢出、交给滚动条。
const LANE_MIN: f32 = 28.0;
/// 自动铺满时一行最高多少像素（行少时行就高一点，点起来也容易）
const LANE_MAX: f32 = 48.0;
/// 手动拉伸时的上下限（比自动档宽，撑到 100 像素看单行细节是合理需求）
const LANE_MIN_USER: f32 = 16.0;
const LANE_MAX_USER: f32 = 104.0;

/// 边框缩放的抓取厚度（四边）。无装饰窗口没有原生边框，这几像素得自己留，
/// 太窄不好抓、太宽又会把顶栏两端的拖动区吃掉。
const RESIZE_BORDER: f32 = 5.0;
/// 四角抓取区的边长。角上要能斜着拖，所以比边厚一截。
///
/// 上限由顶栏的按钮决定：关闭按钮左缘离右边 10 + 32 = 42 点，
/// 上缘在 `HEADER_H/2 - BH/2 = 12` 点处，所以 12 点见方的角区碰不到它。
const RESIZE_CORNER: f32 = 12.0;

// ─────────────────────────── 子窗口的内容 ───────────────────────────

fn window(ctx: &egui::Context, shared: &Shared) {
    let mut st = lock(shared);

    // 系统级的关闭（Alt+F4、任务栏右键关闭）也要把开关拨回去，
    // 否则主窗口下一帧还会重新声明这个视口，窗口关不掉。
    if ctx.input(|i| i.viewport().close_requested()) {
        st.open = false;
        st.forget_placement();
    }
    // 置顶状态变化时才下发命令，别每帧都发。
    if st.pinned != st.applied_top {
        st.applied_top = st.pinned;
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(if st.pinned {
            WindowLevel::AlwaysOnTop
        } else {
            WindowLevel::Normal
        }));
    }
    // 原生主题要**每个视口各发一次**：`SetTheme` 是按视口下发的。
    if st.theme_mode != Some(theme::mode()) {
        st.theme_mode = Some(theme::mode());
        theme::sync_native_theme(ctx);
    }

    // 圆角同理，也是「一扇窗口只做一次」的事。放在这里而不是建视口时做：
    // 那会儿 HWND 还没被 winit 创建出来，按标题根本找不到它。
    if !st.corners {
        st.corners = true;
        set_round_corners(WINDOW_TITLE);
    }

    let frame = Frame::NONE.fill(P::panel());
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        // 子视口与主视口共用一个 `egui::Context`，控件 Id **不会**自动带上视口信息。
        ui.push_id("aox-activity-window", |ui| {
            body(ui, ctx, &mut st);
        });
    });

    // 忙的时候自己驱动重绘：子视口的 repaint 请求是独立的。
    if st.busy {
        st.spin = (st.spin + 0.016) % 1000.0;
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

fn body(ui: &mut Ui, ctx: &egui::Context, st: &mut Activity) {
    // 整个窗口的矩形。**必须在往 ui 里放东西之前取**：`max_rect()` 会随着
    // 逐个 allocate 一路收缩，到函数末尾就只剩底下一条缝了。
    let full = ui.max_rect();

    header(ui, ctx, st);

    Frame::NONE.inner_margin(Margin { left: 18, right: 18, top: 12, bottom: 14 }).show(ui, |ui| {
        // 这一块里的纵向间距一律自己加，免得 egui 的 item_spacing 把高度算歪
        ui.spacing_mut().item_spacing.y = 0.0;

        toolbar(ui, st);
        ui.add_space(14.0);

        // 行表要先知道：时间轴要多高取决于有几行工具。
        // 行少的时候时间轴就矮一点，把空间让给下面的详情；行多就有个上限，
        // 不会把详情面板挤没 —— 多出来的行交给纵向滚动（见 `timeline`）。
        let rows = rows_of(&st.acts);
        let n = rows.len() as f32;
        let natural = AXIS_H + ROUND_H + n * LANE_MAX + (n - 1.0).max(0.0) * LANE_GAP;
        let rest = ui.available_height();
        let chart_h = natural.min(rest - 170.0).max(170.0);
        // 详情面板**吃掉剩下的全部高度**，不再封顶。
        // 封顶（原来 clamp 到 420）会让它在窗口下半截留出一片空白 —— 时间轴明明
        // 可以更高、详情明明可以更宽，却被一个写死的上限挡住。它内部本来就是
        // 可滚动的，给多高都不会撑破。
        let detail_h = (rest - chart_h - 14.0).max(132.0);

        timeline(ui, ctx, st, &rows, chart_h);
        ui.add_space(14.0);
        detail(ui, st, detail_h);
    });

    // 最后再叠两样「窗口级」的东西：边框拖拽热区（要压过顶栏的拖动区，所以必须
    // 最后声明）和最外圈那道 1px 的边。
    resize_handles(ui, ctx, full);
    outline(ui, full);
}

/// 无装饰窗口**没有原生边框缩放**了，这里自己补回来。
///
/// 做法和 egui 内置的 `Resize` 容器一样：四边四角各埋一块用 `Sense::drag` 的
/// 透明热区，按下时只发一条 [`ViewportCommand::BeginResize`]，之后整个拖拽过程
/// 由系统接管（不是我们每帧去改窗口尺寸 —— 那样既卡又会和系统打架）。
///
/// 顺序很关键：这些热区在 `body` 的**最后**声明，而 egui 的命中测试是
/// 「同层里后声明的在上面」，所以窗口最外圈那几像素会判给缩放、而不是顶栏拖动。
/// 重叠只发生在顶栏的边缘，按钮（关闭等）离边还有十来像素，不会被抢。
fn resize_handles(ui: &mut Ui, ctx: &egui::Context, full: Rect) {
    // 最大化的窗口不该能再拖边（Windows 自己也这样），顺手也避免了
    // 「拖着拖着从最大化状态里掉出来」的怪相。
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }

    let zones = resize_zones(full);
    // 光标要在**悬停**时就变（不等按下），否则用户根本不知道这儿能拖
    let hover = ctx.input(|i| i.pointer.hover_pos());
    if let Some(p) = hover {
        if let Some((_, _, d)) = zones.iter().find(|(_, r, _)| r.contains(p)) {
            ctx.set_cursor_icon(resize_cursor(*d));
        }
    }

    for (id, r, dir) in zones {
        let resp = ui.interact(r, ui.id().with(id), Sense::drag());
        if resp.hovered() {
            ctx.set_cursor_icon(resize_cursor(dir));
        }
        if resp.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
        }
    }
}

/// 八块热区的几何。四边只取**去掉四角**的那一段，四角另给一块方形的
/// —— 否则角上会同时命中两块，方向就没法确定。
fn resize_zones(full: Rect) -> [(&'static str, Rect, ResizeDirection); 8] {
    let (l, r, t, b) = (full.left(), full.right(), full.top(), full.bottom());
    let d = RESIZE_BORDER;
    let c = RESIZE_CORNER;
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| {
        Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1))
    };
    use ResizeDirection::*;
    [
        ("nw", rect(l, t, l + c, t + c), NorthWest),
        ("ne", rect(r - c, t, r, t + c), NorthEast),
        ("sw", rect(l, b - c, l + c, b), SouthWest),
        ("se", rect(r - c, b - c, r, b), SouthEast),
        ("n", rect(l + c, t, r - c, t + d), North),
        ("s", rect(l + c, b - d, r - c, b), South),
        ("w", rect(l, t + c, l + d, b - c), West),
        ("e", rect(r - d, t + c, r, b - c), East),
    ]
}

fn resize_cursor(d: ResizeDirection) -> CursorIcon {
    use ResizeDirection::*;
    match d {
        North | South => CursorIcon::ResizeVertical,
        East | West => CursorIcon::ResizeHorizontal,
        NorthEast | SouthWest => CursorIcon::ResizeNeSw,
        NorthWest | SouthEast => CursorIcon::ResizeNwSe,
    }
}

/// 无装饰窗口和桌面之间没有任何分界线，浅色主题下几乎看不出窗口到哪儿为止。
/// 描一圈一像素的边把它框出来（比顶栏那条分隔线重一点，才压得住底色）。
///
/// 圆角要跟着系统的来：窗口最后是被 DWM 按圆角裁掉的，这里若还画直角，
/// 四角那几像素的边框会被裁掉、剩下一个方方正正的断口。
fn outline(ui: &Ui, full: Rect) {
    ui.painter().rect_stroke(
        full.shrink(0.5),
        CornerRadius::same(CORNER_RADIUS),
        Stroke::new(1.0, P::border_strong()),
        egui::StrokeKind::Inside,
    );
}

// ───────────────────── 系统圆角（Windows 11） ─────────────────────

/// DWM 的标准圆角半径，与主窗口一致。单位是 egui 点，会随缩放一起变大。
const CORNER_RADIUS: u8 = 8;

/// 让系统给这扇窗口画上圆角。
///
/// **为什么不自绘**（视口开 `with_transparent` + 自己画一个圆角矩形）：
/// 圆角是画得出来，但边界以外必须真的透明，于是**系统阴影一起丢掉** ——
/// 窗口会像一张贴纸糊在桌面上，而且透明窗口在拖拽缩放时容易闪。
///
/// 而 winit 的无装饰窗口并不是 `WS_POPUP`：它靠 `WM_NCCALCSIZE` 把非客户区
/// 压成 0，`WS_BORDER` / `WS_SYSMENU` 都还留着 —— 在系统眼里这仍是一扇
/// **普通的重叠窗口**，所以走 DWM 那条路既能拿到抗锯齿的圆角，又原样保留阴影，
/// 半径还天然和主窗口一致（这正是我们刚把系统标题栏去掉时丢掉的那样东西）。
///
/// winit 其实自带这个开关（`WindowAttributesExtWindows::with_corner_preference`），
/// 但 egui 的 `ViewportBuilder` 没有透传的口子，子视口更是连 hook 都没有
/// （`ViewportBuilder` 只有 `has_shadow`、`taskbar` 这类，且 eframe 也没全用）。
/// 所以这里直接喊一次系统 API：三个函数、十来行声明，不值得为它引入 windows-sys。
///
/// 只需要在建好窗口之后叫**一次** —— 圆角是挂在 HWND 上的状态，不是每帧的事。
/// 窗口一重建（关掉再开）圆角就没了，所以调用方用 [`Activity::corners`] 记着。
#[cfg(windows)]
fn set_round_corners(title: &str) {
    use std::ffi::c_void;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(cb: extern "system" fn(*mut c_void, isize) -> i32, param: isize) -> i32;
        fn IsWindowVisible(h: *mut c_void) -> i32;
        fn GetWindowTextLengthW(h: *mut c_void) -> i32;
        fn GetWindowTextW(h: *mut c_void, buf: *mut u16, max: i32) -> i32;
        fn GetWindowThreadProcessId(h: *mut c_void, pid: *mut u32) -> u32;
    }

    /// 枚举时用的上下文。回调只能是不捕获环境的普通 fn，所以东西得从 `param` 递进去。
    struct Probe<'a> {
        title: &'a str,
        pid: u32,
        hwnd: *mut c_void,
    }

    /// 是不是「本进程开的那扇标题对得上的窗口」。
    ///
    /// SAFETY: `h` 是 `EnumWindows` 递过来的句柄，在本次调用期间有效。
    unsafe fn mine(h: *mut c_void, title: &str, pid: u32) -> bool {
        if unsafe { IsWindowVisible(h) } == 0 {
            return false;
        }
        // 光看标题不够：用户完全可以同时开着两个 AOX 实例（比如装了两个包），
        // 那样可能摸到别人的窗口上去改圆角。
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(h, &mut owner) };
        if owner != pid {
            return false;
        }
        let n = unsafe { GetWindowTextLengthW(h) };
        if n <= 0 {
            return false;
        }
        let mut buf: Vec<u16> = vec![0; n as usize + 1];
        let got = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), n + 1) };
        if got <= 0 {
            return false;
        }
        String::from_utf16_lossy(&buf[..got as usize]).contains(title)
    }

    extern "system" fn probe(h: *mut c_void, param: isize) -> i32 {
        // SAFETY: `param` 就是下面那个 `Probe` 的地址，生命周期覆盖整个 `EnumWindows`
        let p = unsafe { &mut *(param as *mut Probe) };
        if p.hwnd.is_null() && unsafe { mine(h, p.title, p.pid) } {
            p.hwnd = h;
            return 0; // 找到就停，别再枚举下去
        }
        1
    }

    let mut ctx = Probe { title, pid: std::process::id(), hwnd: std::ptr::null_mut() };
    unsafe { EnumWindows(probe, &mut ctx as *mut Probe as isize) };
    if ctx.hwnd.is_null() {
        return; // 还没建出来（或平台不给），下次开窗口再试
    }
    // DWM 调用统一走 `theme`：同一个符号在整个 crate 里只声明一次。
    // 失败（Win10 不认识这个属性）也无所谓 —— 那就维持方角，本来的样子。
    theme::dwm_set_attribute(
        ctx.hwnd as isize,
        theme::DWMWA_CORNER_PREFERENCE,
        &theme::DWMWCP_ROUND as *const i32 as *const c_void,
        std::mem::size_of::<i32>() as u32,
    );
}

#[cfg(not(windows))]
fn set_round_corners(_title: &str) {}

// ───────────────────────────── 顶栏 ─────────────────────────────

fn header(ui: &mut Ui, ctx: &egui::Context, st: &mut Activity) {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), HEADER_H), Sense::click_and_drag());
    // 必须在按下鼠标的**那一帧**发 StartDrag，晚一帧窗口就跟不动了。
    if resp.drag_started() {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }

    // 左：呼吸点 + 标题 + 现状
    let pulse = ((st.spin * 2.0) as i32) % 2 == 0;
    ui.painter().circle_filled(
        Pos2::new(rect.left() + 22.0, rect.center().y),
        5.0,
        if st.busy {
            if pulse {
                P::ink()
            } else {
                P::ink_soft()
            }
        } else {
            P::text_faint()
        },
    );
    ui.painter().text(
        Pos2::new(rect.left() + 40.0, rect.center().y),
        Align2::LEFT_CENTER,
        "活动流",
        theme::bold_font(18.0),
        P::text(),
    );

    let status = if st.status.is_empty() { "就绪" } else { &st.status };
    let info = if st.busy {
        format!("{status} · {:.1}s", st.started.map_or(0.0, |s| s.elapsed().as_secs_f32()))
    } else if st.round == 0 {
        "空闲 · 不记录任何内容".to_string()
    } else {
        format!("空闲 · 共 {} 轮 · {} 条记录", st.round, st.acts.len())
    };
    ui.painter().text(
        Pos2::new(rect.left() + 108.0, rect.center().y),
        Align2::LEFT_CENTER,
        info,
        FontId::proportional(13.5),
        P::text_faint(),
    );

    // 右侧自右向左排：关闭 / 最大化 / 最小化 / 固定，落到屏幕上就是 Windows
    // 那套「固定 · 最小化 · 最大化 · 关闭」。因为系统标题栏已经不要了，
    // 这几枚自绘按钮就是窗口唯一的出口。
    //
    // 这里**不再有「清空」**：活动流已经随这段对话落进了同一个文件，删它只能
    // 通过删那段对话去做 —— 界面上一按就清空，文件里却还留着，下次打开这段
    // 对话它又长回来了，那才是真的说不清。
    const BW: f32 = 32.0;
    const BH: f32 = 28.0;
    const GAP: f32 = 2.0;
    let y = rect.center().y - BH / 2.0;
    let mut x = rect.right() - 10.0 - BW;
    let btn = |x: f32| Rect::from_min_size(Pos2::new(x, y), Vec2::new(BW, BH));
    if win_button(ui, btn(x), "close", Glyph::Close) {
        st.open = false;
        st.forget_placement();
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }
    x -= BW + GAP;
    // 最大化状态得主动去问 —— 这台窗口现在是自己管的，没人替我们记着它是不是最大了
    let maxed = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    if win_button(ui, btn(x), "max", Glyph::Max { on: maxed }) {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maxed));
    }
    x -= BW + GAP;
    if win_button(ui, btn(x), "min", Glyph::Min) {
        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
    }
    x -= BW + GAP;
    if win_button(ui, btn(x), "pin", Glyph::Pin { on: st.pinned }) {
        st.pinned = !st.pinned;
    }

    // 标题栏与内容之间的一道细线
    let line = Rect::from_min_max(
        Pos2::new(rect.left(), rect.bottom() - 1.0),
        Pos2::new(rect.right(), rect.bottom()),
    );
    ui.painter().rect_filled(line, CornerRadius::ZERO, P::border());
}

#[derive(Clone, Copy)]
enum Glyph {
    Close,
    Min,
    /// 最大化 / 还原。`on` = 当前已经是最大化（于是画成「还原」那个双框）
    Max { on: bool },
    Pin { on: bool },
}

fn win_button(ui: &mut Ui, r: Rect, id: &str, glyph: Glyph) -> bool {
    let resp = ui.interact(r, ui.id().with(id), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, CornerRadius::same(7), P::surface_hi());
    }
    // 「已固定」是个**持续状态**，得一直看得见：不悬停时也给它最深的前景色，
    // 否则一枚灰图钉躺在灰底上，用户根本分不出自己到底钉没钉上。
    let pinned = matches!(glyph, Glyph::Pin { on: true });
    let fg = if resp.hovered() || pinned { P::ink() } else { P::text_dim() };
    let p = ui.painter();
    let c = r.center();
    let stroke = Stroke::new(1.4, fg);
    match glyph {
        Glyph::Close => {
            let d = 4.2;
            p.line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], stroke);
            p.line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], stroke);
        }
        Glyph::Min => {
            p.line_segment([c + Vec2::new(-5.0, 2.4), c + Vec2::new(5.0, 2.4)], stroke);
        }
        Glyph::Max { on: false } => {
            // 一个方框 —— 就是 Windows 的最大化
            p.rect_stroke(
                Rect::from_center_size(c, Vec2::splat(9.6)),
                CornerRadius::ZERO,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        Glyph::Max { on: true } => {
            // 「还原」：后面那个框被前面的挡住半截，所以只描它的上边与右边
            let (b0, b1) = (c + Vec2::new(-2.4, -5.0), c + Vec2::new(5.0, 1.6));
            p.line_segment([b0, Pos2::new(b1.x, b0.y)], stroke);
            p.line_segment([Pos2::new(b1.x, b0.y), b1], stroke);
            p.rect_stroke(
                Rect::from_min_max(c + Vec2::new(-4.8, -2.0), c + Vec2::new(2.6, 5.4)),
                CornerRadius::ZERO,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        Glyph::Pin { on } => {
            // 一枚图钉：扁平的帽 + 一根收成尖的针。
            //
            // 固定与不固定**共用同一个形状**，只差一个 90° 的朝向 ——
            // 不固定时横躺着（针尖朝左下），固定时立起来（针尖朝下）。
            // 这样两态之间不是「换了张图」，而是同一枚钉子躺下与站起，
            // 用户不用重新认识一个图标就知道状态变了。
            //
            // 全部实心填色，不再用描边：细线条在这块深底上是真的看不清。
            let rot = |v: Vec2| if on { v } else { Vec2::new(-v.y, v.x) };
            // 矩形转过 90° 之后长宽对调，所以尺寸得单独换一次
            let swap = |v: Vec2| if on { v } else { Vec2::new(v.y, v.x) };

            // 帽
            p.rect_filled(
                Rect::from_center_size(c + rot(Vec2::new(0.0, -4.2)), swap(Vec2::new(10.0, 2.6))),
                CornerRadius::same(1),
                fg,
            );
            // 针杆
            p.rect_filled(
                Rect::from_center_size(c + rot(Vec2::new(0.0, -1.0)), swap(Vec2::new(3.2, 3.8))),
                CornerRadius::ZERO,
                fg,
            );
            // 针尖
            p.add(egui::Shape::convex_polygon(
                vec![
                    c + rot(Vec2::new(0.0, 4.8)),
                    c + rot(Vec2::new(-1.6, 0.9)),
                    c + rot(Vec2::new(1.6, 0.9)),
                ],
                fg,
                Stroke::NONE,
            ));
        }
    }
    resp.clicked()
}

// ───────────────────────────── 工具栏 ─────────────────────────────

/// 一排视图工具：左半是「看哪一段」，右半是「怎么看」。
fn toolbar(ui: &mut Ui, st: &mut Activity) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;

        // ── 锚点：把某一刻钉在屏幕的某个位置 ──
        if pill(ui, "起点", st.anchor == Anchor::Start)
            .on_hover_text("把第一轮的开始钉在最左边")
            .clicked()
        {
            st.anchor = Anchor::Start;
        }
        if pill(ui, "居中", st.anchor == Anchor::Center)
            .on_hover_text("让「当前时刻」停在正中，两边都看得见")
            .clicked()
        {
            st.anchor = Anchor::Center;
        }
        if pill(ui, "最新", st.anchor == Anchor::Latest)
            .on_hover_text("贴着最新的进度走")
            .clicked()
        {
            st.anchor = Anchor::Latest;
        }

        vsep(ui);

        // ── 按轮次查看 ──
        ui.label(RichText::new("轮次").color(P::text_faint()).size(13.5));
        let edit = ui.add(
            TextEdit::singleline(&mut st.round_input)
                .id(egui::Id::new("aox-act-round-input"))
                .desired_width(48.0)
                .font(theme::mono_font(14.0))
                .hint_text("1"),
        );
        let entered = edit.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter))
            && !st.round_input.trim().is_empty();
        if pill(ui, "定位", false).clicked() || entered {
            if let Ok(r) = st.round_input.trim().parse::<u32>() {
                if r >= 1 && r <= st.round {
                    st.goto = Some(r);
                }
            }
        }
        ui.label(
            RichText::new(format!("共 {} 轮", st.round)).color(P::text_faint()).size(13.5),
        );

        // ── 右半：行高 + 横向缩放 + 连接线 ──
        //
        // `right_to_left` 里**先加的在最右边**，所以下面这段的书写顺序与屏幕上
        // 从左到右的顺序相反：最终看到的是
        //     连接线  适应 │ 缩放 − ＋ │ 行高 − ＋
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;

            // 行高：工具一多，行会被摊薄到看不清 —— 这两枚把每行撑开/压回，
            // 超出可视区的部分由 Shift+滚轮（或最右边的滚动条）上下翻。
            if pill(ui, "＋", false)
                .on_hover_text("行高 ＋：把每一行撑高。超出可视区后用 Shift+滚轮 上下翻")
                .clicked()
            {
                st.pending_row = Some(1.25);
            }
            if pill(ui, "－", false).on_hover_text("行高 －：把每一行压矮，一屏看更多行").clicked()
            {
                st.pending_row = Some(1.0 / 1.25);
            }
            ui.label(RichText::new("行高").color(P::text_faint()).size(13.5));
            vsep(ui);

            if pill(ui, "＋", false).on_hover_text("放大（横轴）").clicked() {
                st.pending_zoom = Some(1.35);
            }
            if pill(ui, "－", false).on_hover_text("缩小（横轴）").clicked() {
                st.pending_zoom = Some(1.0 / 1.35);
            }
            ui.label(RichText::new("缩放").color(P::text_faint()).size(13.5));
            vsep(ui);

            if pill(ui, "适应", false)
                .on_hover_text("整段铺满窗口，并把行高交回自动")
                .clicked()
            {
                st.fit_all = true;
            }
            if pill(ui, "连接线", st.link)
                .on_hover_text("按真实执行顺序把各条色块首尾相连，线色取自出发的那一条")
                .clicked()
            {
                st.link = !st.link;
            }
        });
    });
}

/// 工具栏里的细竖分隔
fn vsep(ui: &mut Ui) {
    ui.add_space(6.0);
    let (r, _) = ui.allocate_exact_size(Vec2::new(1.0, 20.0), Sense::hover());
    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, P::border()));
    ui.add_space(6.0);
}

/// 胶囊按钮。`on` = 当前选中的那个（反白）。
fn pill(ui: &mut Ui, text: &str, on: bool) -> egui::Response {
    let font = FontId::proportional(14.0);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    let pad = Vec2::new(12.0, 6.0);
    let size = galley.size() + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hov = resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    let r = Rect::from_center_size(rect.center(), size);
    p.rect_filled(
        r,
        CornerRadius::same(10),
        if on {
            P::reverse_bg()
        } else if hov {
            P::surface_hi()
        } else {
            Color32::TRANSPARENT
        },
    );
    if !on {
        p.rect_stroke(
            r,
            CornerRadius::same(10),
            Stroke::new(1.0, if hov { P::border_strong() } else { P::border() }),
            egui::StrokeKind::Inside,
        );
    }
    let fg =
        if on { P::on_reverse() } else if hov { P::text() } else { P::text_dim() };
    p.galley(r.min + pad, galley, fg);
    resp
}

// ═══════════════════════════ 时间轴 ═══════════════════════════

/// 把时间映射到像素：横轴 = 时间，任务自左向右推进。
#[derive(Clone, Copy)]
struct View {
    /// 左边缘的时刻（相对 `t_zero`，秒）
    t0: f32,
    /// 每秒多少像素
    scale: f32,
}

impl View {
    fn compute(st: &Activity, plot_w: f32) -> View {
        let (t_min, t_max) = if st.acts.is_empty() {
            (0.0, 1.0)
        } else {
            let tz = st.tz();
            let mut lo = f32::MAX;
            let mut hi = f32::MIN;
            for a in &st.acts {
                lo = lo.min(a.at - tz);
                // 进行中的段按「现在」算尾端，否则按真实结束时刻
                hi = hi.max(a.end_t(st.now) - tz);
            }
            (lo, hi)
        };
        let scale = st.scale.unwrap_or_else(|| (plot_w / DEFAULT_SPAN).clamp(MIN_SCALE, MAX_SCALE));
        let vis = plot_w / scale;
        // 视野之外至少留一点点余量，免得首尾的色块贴着边
        let pad = (vis * 0.015).clamp(0.05, 2.0);
        let t0 = match st.anchor {
            Anchor::Start => t_min,
            Anchor::Center => t_max - vis * 0.5,
            Anchor::Latest => t_max - vis,
            Anchor::Free => st.t0,
        };
        let t0 = t0.max(t_min - pad);
        let t0 = if st.anchor == Anchor::Free {
            t0.min((t_max - vis * 0.15).max(t_min - pad))
        } else {
            t0
        };
        View { t0, scale }
    }

    fn vis(self, plot_w: f32) -> f32 {
        plot_w / self.scale
    }

    fn x(self, plot: Rect, t: f32) -> f32 {
        plot.left() + (t - self.t0) * self.scale
    }

    fn t_at(self, plot: Rect, x: f32) -> f32 {
        self.t0 + (x - plot.left()) / self.scale
    }
}

fn timeline(ui: &mut Ui, ctx: &egui::Context, st: &mut Activity, rows: &[Row], avail_h: f32) {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let w = ui.available_width();
        let (band, band_resp) = ui.allocate_exact_size(Vec2::new(w, AXIS_H), Sense::hover());
        let (rband, _) = ui.allocate_exact_size(Vec2::new(w, ROUND_H), Sense::hover());
        // 手势不止一种，又没有可点的控件来承载提示，就挂在刻度带上：
        // 这里是「时间轴」这个对象本身，鼠标划过它时提示最贴切。
        band_resp.on_hover_text(
            "滚轮：左右平移 · Ctrl+滚轮：缩放横轴 · Shift+滚轮：上下翻行\n\
             拖动：平移 · 点击色块：看这一步的详情",
        );

        // 绘图区的左边界与宽度：轴、行名、色块全按它对齐
        let plot_left = band.left() + GUTTER;
        let plot_w = (band.right() - RSV - plot_left).max(60.0);
        let area_h = (avail_h - AXIS_H - ROUND_H).max(60.0);
        // 顶栏那两条带子不是「绘图区」：它们只借绘图区的横坐标，
        // 所以必须自己造一个左边界对齐的矩形 —— 直接把整条 band 传进
        // `View::x` 会让刻度和色块错开整整一个行名栏的宽度。
        let axis = Rect::from_min_size(Pos2::new(plot_left, band.top()), Vec2::new(plot_w, AXIS_H));
        let ruler =
            Rect::from_min_size(Pos2::new(plot_left, rband.top()), Vec2::new(plot_w, ROUND_H));
        // 行的可视区（也是平移/缩放的响应区）。ScrollArea 的视口正好落在这里。
        let lane_rect =
            Rect::from_min_size(Pos2::new(band.left(), rband.bottom()), Vec2::new(w - RSV, area_h));

        // 行高先算：默认「把这块高度摊平」，但**不摊到挤扁** —— 工具行一多，
        // `LANE_MIN` 就兜住下限，多出来的部分交给纵向滚动。用户手动拉过就用他的值。
        let n = rows.len().max(1) as f32;
        let fit = ((area_h - LANE_GAP * (n - 1.0)) / n).clamp(LANE_MIN, LANE_MAX);

        // ── 先处理跳转与交互，再按最终视图绘制 ──
        if let Some(r) = st.goto.take() {
            st.fit_round(r, plot_w);
        }
        if st.fit_all {
            st.fit_all = false;
            // 「适应」= 横竖都回到自动：横轴整段铺满、行高交回按高度摊平
            st.row_h = None;
            let (lo, hi) = st.span();
            let pad = ((hi - lo) * 0.04).max(0.2);
            st.fit_range(lo - pad, hi + pad, plot_w);
            // 整段铺满时把左端钉死，任务继续推进也不会把视野带走
            st.anchor = Anchor::Start;
        }
        if let Some(f) = st.pending_zoom.take() {
            st.zoom_by(f, plot_w);
        }
        if let Some(k) = st.pending_row.take() {
            // 第一次拉伸时以「自动档的当前值」为基准，手感才连续
            st.row_h = Some((st.row_h.unwrap_or(fit) * k).clamp(LANE_MIN_USER, LANE_MAX_USER));
        }

        let v0 = View::compute(st, plot_w);
        input(ui, ctx, st, lane_rect, plot_left, plot_w, v0);
        let v = View::compute(st, plot_w);

        // ── 时间刻度带 ──
        let base_y = band.bottom() - 1.0;
        {
            let p = ui.painter();
            p.line_segment(
                [Pos2::new(plot_left, base_y), Pos2::new(plot_left + plot_w, base_y)],
                Stroke::new(1.0, P::border()),
            );
            let step = nice_step(90.0 / v.scale);
            let right = plot_left + plot_w;
            let mut t = (v.t0 / step).floor() * step;
            let mut guard = 0;
            while v.x(axis, t) <= right + step && guard < 4096 {
                guard += 1;
                let x = v.x(axis, t);
                if t >= -0.0001 && x >= plot_left - 0.5 {
                    p.line_segment(
                        [Pos2::new(x, base_y - 6.0), Pos2::new(x, base_y)],
                        Stroke::new(1.0, P::border_strong()),
                    );
                    p.text(
                        Pos2::new(x, base_y - 17.0),
                        Align2::CENTER_CENTER,
                        fmt_at(t, step),
                        theme::mono_font(12.0),
                        P::text_faint(),
                    );
                }
                t += step;
            }
            p.text(
                Pos2::new(plot_left - 12.0, base_y - 15.0),
                Align2::RIGHT_CENTER,
                "秒",
                FontId::proportional(12.5),
                P::text_faint(),
            );
            p.text(
                Pos2::new(plot_left - 12.0, rband.center().y),
                Align2::RIGHT_CENTER,
                "轮次",
                FontId::proportional(12.5),
                P::text_faint(),
            );
        }

        // ── 轮次刻度带 ──
        {
            let p = ui.painter();
            let tz = st.tz();
            let mut last_right = f32::MIN;
            for a in st.acts.iter().filter(|a| a.kind == ActKind::Input) {
                let x = v.x(ruler, a.at - tz);
                if x < plot_left - 20.0 || x > plot_left + plot_w {
                    continue;
                }
                let text = a.round.to_string();
                let tw = text_w(ui, &theme::mono_font(12.0), &text) + 14.0;
                let r =
                    Rect::from_min_size(Pos2::new(x + 3.0, rband.top() + 2.0), Vec2::new(tw, 19.0));
                if r.right() < last_right + 4.0 {
                    continue; // 挤在一起就不画，免得叠成一团
                }
                last_right = r.right();
                p.rect_filled(r, CornerRadius::same(6), P::surface());
                p.rect_stroke(
                    r,
                    CornerRadius::same(6),
                    Stroke::new(1.0, P::border()),
                    egui::StrokeKind::Inside,
                );
                p.text(r.center(), Align2::CENTER_CENTER, text, theme::mono_font(12.0), P::text_dim());
            }
        }

        // ── 行 ──
        // 行高可以是「自动摊平」，也可以是用户手拉的值；两者都可能超出可视区，
        // 超出就由下面这个滚动区负责上下翻。
        let lane_h = st.row_h.unwrap_or(fit);
        let content_h = n * lane_h + (n - 1.0) * LANE_GAP;
        let overflows = content_h > area_h + 0.5;

        // 滚轮默认是「横向平移」（见 `input`），所以纵向滚动挂在 Shift 上：
        // 按住 Shift 才把滚轮喂给滚动区，否则交给时间轴。
        let shift_down = ctx.input(|i| i.modifiers.shift);
        ScrollArea::vertical()
            .id_salt("aox-act-lanes")
            .scroll_source(egui::scroll_area::ScrollSource {
                scroll_bar: true,
                mouse_wheel: shift_down,
                drag: false,
            })
            .auto_shrink([false, false])
            .max_height(area_h)
            .show(ui, |ui| {
                let (content, _) =
                    ui.allocate_exact_size(Vec2::new(w - RSV, content_h), Sense::hover());
                let plot = Rect::from_min_max(
                    Pos2::new(content.left() + GUTTER, content.top()),
                    Pos2::new(content.left() + GUTTER + plot_w, content.bottom()),
                );
                lanes(ui, st, rows, plot, lane_h, v);
            });

        // 行被折叠到视野之外时，右下角给一句话 —— 只有滚动条这一个细窄的线索，
        // 很容易让人以为「工具就这么多」。
        if overflows {
            let below = ((content_h - area_h) / (lane_h + LANE_GAP)).ceil() as i32;
            ui.painter().text(
                Pos2::new(band.right() - RSV - 6.0, lane_rect.bottom() - 4.0),
                Align2::RIGHT_BOTTOM,
                format!("还有 {below} 行 · Shift+滚轮 上下翻"),
                FontId::proportional(12.0),
                P::text_faint(),
            );
        }
    });
}

impl Activity {
    /// 以视图中心为锚缩放（工具栏的 ± 用它）。
    fn zoom_by(&mut self, factor: f32, plot_w: f32) {
        let v = View::compute(self, plot_w);
        let ns = (v.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        if self.anchor == Anchor::Free {
            let c = v.t0 + v.vis(plot_w) * 0.5;
            self.t0 = c - (plot_w / ns) * 0.5;
        }
        self.scale = Some(ns);
    }
}

/// 平移 / 缩放 / 点选。全部落在时间轴的行区里才生效。
///
/// 响应的声明顺序很重要：这个平移区在这儿先声明（在滚动区**之前**），
/// 色块在滚动区里后声明 —— 于是点在色块上时色块赢、点在空白处时这里赢。
fn input(
    ui: &mut Ui,
    ctx: &egui::Context,
    st: &mut Activity,
    lane: Rect,
    plot_left: f32,
    plot_w: f32,
    v0: View,
) {
    let plot = Rect::from_min_size(Pos2::new(plot_left, lane.top()), Vec2::new(plot_w, lane.height()));
    let resp = ui.interact(lane, ui.id().with("aox-act-pan"), Sense::click_and_drag());

    // 拖动 = 平移（抓手感：往右拖 = 看到更早）
    if resp.dragged() {
        let dx = resp.drag_delta().x;
        if dx != 0.0 {
            st.anchor = Anchor::Free;
            st.t0 = v0.t0 - dx / v0.scale;
        }
    }
    // 点空白处 = 取消选中（点在色块上时这次点击被色块拿走了，这里不会触发）
    if resp.clicked() {
        st.selected = None;
    }

    let (pointer, zoom, scroll, shift) = ctx.input(|i| {
        (
            i.pointer.hover_pos(),
            i.zoom_delta(),
            i.smooth_scroll_delta,
            i.modifiers.shift,
        )
    });
    let Some(p) = pointer else { return };
    if !lane.contains(p) {
        return;
    }

    if (zoom - 1.0).abs() > 1e-4 {
        // Ctrl+滚轮：以光标处那一秒为锚缩放，指哪儿放哪儿
        let t_anchor = v0.t_at(plot, p.x);
        let ns = (v0.scale * zoom).clamp(MIN_SCALE, MAX_SCALE);
        let keep = st.anchor == Anchor::Free;
        st.scale = Some(ns);
        if keep {
            st.t0 = t_anchor - (p.x - plot_left) / ns;
        }
    } else if scroll != Vec2::ZERO && !shift {
        // 滚轮：横向平移，一次滚动约走视野的 18%。
        // Shift 按住时这里让路 —— 那颗滚轮被滚动区拿去翻行了（见 `timeline`）。
        let vis = plot_w / v0.scale;
        let d = (scroll.x + scroll.y) / 50.0;
        st.anchor = Anchor::Free;
        st.t0 = v0.t0 - d * vis * 0.18;
    }
}

// ── 行、色块、连接线 ──

fn lanes(ui: &mut Ui, st: &mut Activity, rows: &[Row], plot: Rect, lane_h: f32, v: View) {
    let now = st.now;
    let tz = st.tz();
    let clip = ui.clip_rect();
    let visible = plot.intersect(clip);
    if visible.width() < 8.0 || visible.height() < 4.0 {
        return;
    }
    let step = lane_h + LANE_GAP;
    // 绘图区专用画笔：所有色块/连接线都被裁在绘图区内，不会糊到右侧留白上
    let p = ui.painter_at(plot);

    // ── 行：基线 + 行名 ──
    for (i, row) in rows.iter().enumerate() {
        let y = plot.top() + i as f32 * step;
        if y > clip.bottom() || y + lane_h < clip.top() {
            continue;
        }
        p.line_segment(
            [Pos2::new(plot.left(), y + lane_h), Pos2::new(plot.right(), y + lane_h)],
            Stroke::new(1.0, P::border()),
        );
        // 行名画在左侧栏里，用外层画笔（它不在绘图区的裁剪范围内）
        let name = row.label();
        let font =
            if row.dim() { FontId::proportional(13.5) } else { theme::mono_font(13.5) };
        let g = elide(ui, name, font, P::text_dim(), GUTTER - 32.0);
        let yc = (y + lane_h * 0.5).clamp(clip.top() + 5.0, clip.bottom() - 5.0);
        ui.painter().galley(
            Pos2::new(plot.left() - 12.0 - g.size().x, yc - g.size().y * 0.5),
            g,
            P::text_dim(),
        );
    }

    // ── 轮次分隔（虚线）：一眼看出第几轮从哪一刻开始 ──
    for a in st.acts.iter().filter(|a| a.kind == ActKind::Input) {
        let x = v.x(plot, a.at - tz);
        if x < plot.left() - 2.0 || x > plot.right() {
            continue;
        }
        let mut y = plot.top().max(clip.top());
        let bottom = plot.bottom().min(clip.bottom());
        while y < bottom {
            let y2 = (y + 4.0).min(bottom);
            p.line_segment([Pos2::new(x, y), Pos2::new(x, y2)], Stroke::new(1.0, P::border()));
            y = y2 + 5.0;
        }
    }

    // ── 几何：先算一遍，连接线要复用 ──
    let shades = shades_of(st);
    struct Bar {
        row: usize,
        x0: f32,
        x1: f32,
        fill: Color32,
    }
    let mut bars: Vec<Bar> = Vec::with_capacity(st.acts.len());
    for a in st.acts.iter() {
        bars.push(Bar {
            row: row_of(rows, a),
            x0: v.x(plot, a.at - tz),
            x1: v.x(plot, a.end_t(now) - tz),
            fill: fill_of(a, &shades),
        });
    }
    let cy = |row: usize| plot.top() + row as f32 * step + lane_h * 0.5;
    let bar_rect = |b: &Bar| -> Rect {
        let l = b.x0.max(plot.left());
        let r = b.x1.max(l + MIN_BAR_W).min(plot.right());
        let y = plot.top() + b.row as f32 * step;
        Rect::from_min_max(Pos2::new(l, y), Pos2::new(r, y + lane_h))
    };

    // ── 连接线：按真实执行顺序首尾相连，线色取自出发的那一条 ──
    if st.link {
        for w in bars.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            let p0 = Pos2::new(a.x1.max(a.x0), cy(a.row));
            let p1 = Pos2::new(b.x0, cy(b.row));
            if p0.x > plot.right() + 60.0 || p1.x < plot.left() - 60.0 {
                continue;
            }
            let g = ((p1.x - p0.x) * 0.45).clamp(5.0, 46.0);
            let c1 = Pos2::new(p0.x + g, p0.y);
            let c2 = Pos2::new(p1.x - g, p1.y);
            let mut pts = Vec::with_capacity(17);
            for i in 0..=16 {
                let t = i as f32 / 16.0;
                let u = 1.0 - t;
                pts.push(Pos2::new(
                    u * u * u * p0.x
                        + 3.0 * u * u * t * c1.x
                        + 3.0 * u * t * t * c2.x
                        + t * t * t * p1.x,
                    u * u * u * p0.y
                        + 3.0 * u * u * t * c1.y
                        + 3.0 * u * t * t * c2.y
                        + t * t * t * p1.y,
                ));
            }
            p.add(egui::Shape::line(pts, Stroke::new(1.5, a.fill.gamma_multiply(0.7))));
            p.circle_filled(p0, 1.7, a.fill.gamma_multiply(0.85));
        }
    }

    // ── 色块 ──
    let mut hit: Option<u64> = None;
    for (i, a) in st.acts.iter().enumerate() {
        let Some(b) = bars.get(i) else { continue };
        let r = bar_rect(b);
        if r.right() < visible.left() || r.left() > visible.right() {
            continue;
        }
        p.rect_filled(r, CornerRadius::same(4), b.fill);
        // 还在跑的：描一圈 + 右端一个亮点，一眼看出「没结束」
        if a.open() {
            p.rect_stroke(
                r,
                CornerRadius::same(4),
                Stroke::new(1.2, P::text()),
                egui::StrokeKind::Inside,
            );
            p.circle_filled(Pos2::new(r.right() - 2.0, r.center().y), 1.6, P::bg());
        }
        if st.selected == Some(a.seq) {
            p.rect_stroke(
                r.expand(1.5),
                CornerRadius::same(5),
                Stroke::new(1.8, P::ink()),
                egui::StrokeKind::Outside,
            );
        }

        let resp = ui.interact(r, ui.id().with(("bar", a.seq)), Sense::click());
        let hovered = resp.hovered();
        let clicked = resp.clicked();
        if hovered {
            p.rect_stroke(
                r.expand(1.0),
                CornerRadius::same(5),
                Stroke::new(1.2, P::ink_soft()),
                egui::StrokeKind::Inside,
            );
            let dur = a.dur(now);
            resp.on_hover_text(format!(
                "{}\n{} · {} → {}（{:.2}s）",
                a.title,
                a.kind.label(),
                fmt_at(a.at - tz, 0.5),
                fmt_at(a.at - tz + dur, 0.5),
                dur
            ));
        }
        if clicked {
            hit = Some(a.seq);
        }
    }
    if let Some(seq) = hit {
        st.selected = Some(seq);
    }

    // ── 「现在」：只有任务在跑的时候才有意义 ──
    if st.busy {
        let x = v.x(plot, st.now - tz);
        if x >= plot.left() && x <= plot.right() {
            p.line_segment(
                [Pos2::new(x, plot.top() - 2.0), Pos2::new(x, plot.bottom() + 2.0)],
                Stroke::new(1.0, P::ink().gamma_multiply(0.75)),
            );
        }
    }

    // ── 空状态：讲清楚「为什么什么都没有」 ──
    if st.acts.is_empty() {
        ui.painter_at(visible).text(
            visible.center(),
            Align2::CENTER_CENTER,
            "空闲时不记录任何内容 · 下达指令后，这里会按时间铺开模型与每一步工具调用",
            FontId::proportional(14.5),
            P::text_faint(),
        );
    }
}

/// 插件 → 色阶序号：同一插件的工具用同一族色的不同深浅
fn shades_of(st: &Activity) -> Vec<(String, usize)> {
    let mut v: Vec<(String, usize)> = Vec::new();
    for a in &st.acts {
        if a.kind == ActKind::Tool
            && !a.plugin.is_empty()
            && !v.iter().any(|(t, _)| *t == a.plugin)
        {
            v.push((a.plugin.clone(), v.len()));
        }
    }
    v
}

fn fill_of(a: &Act, shades: &[(String, usize)]) -> Color32 {
    let base = if a.failed { ActKind::failed_color() } else { a.kind.color() };
    if a.kind == ActKind::Tool && !a.plugin.is_empty() {
        let idx = shades.iter().find(|(t, _)| *t == a.plugin).map_or(0, |(_, i)| *i);
        shade(base, idx)
    } else {
        base
    }
}

/// 让明度按序号分层：同一插件的工具调用用同一族色，深浅不同
fn shade(base: Color32, idx: usize) -> Color32 {
    const STEPS: [f32; 5] = [0.0, 0.22, 0.40, -0.18, 0.56];
    let k = STEPS[idx % STEPS.len()];
    if k >= 0.0 {
        base.lerp_to_gamma(P::text(), k)
    } else {
        base.lerp_to_gamma(P::panel(), -k)
    }
}

/// 刻度间隔取 1/2/5×10ⁿ 里最接近的那个，保证刻度线不至于挤成一团
fn nice_step(rough: f32) -> f32 {
    const STEPS: [f32; 20] = [
        0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0,
        600.0, 900.0, 1800.0, 3600.0, 7200.0,
    ];
    STEPS.iter().copied().find(|s| *s >= rough).unwrap_or(7200.0)
}

/// 时刻的文字表示。小数位随刻度粗细走，免得 0.01s 的刻度上写着「0.0s」。
fn fmt_at(t: f32, step: f32) -> String {
    let t = t.max(0.0);
    if t < 60.0 {
        if step < 0.1 {
            format!("{t:.2}s")
        } else if step < 1.0 {
            format!("{t:.1}s")
        } else {
            format!("{t:.0}s")
        }
    } else if t < 3600.0 {
        format!("{}:{:02}", (t / 60.0) as i32, (t % 60.0) as i32)
    } else {
        format!("{}:{:02}", (t / 3600.0) as i32, ((t % 3600.0) / 60.0) as i32)
    }
}

fn text_w(ui: &Ui, font: &FontId, s: &str) -> f32 {
    let mut job = egui::text::LayoutJob::default();
    job.append(s, 0.0, TextFormat { font_id: font.clone(), ..Default::default() });
    ui.fonts_mut(|f| f.layout_job(job).size().x)
}

/// 单行文本按**像素**收口：放不下就省略号截断（行名、工具名都可能很长）
fn elide(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> Arc<Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        TextFormat { font_id: font, color, ..Default::default() },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: max_w.max(20.0),
        max_rows: 1,
        // 中文没有词边界，不开这个的话「整段放不下」时会直接不截
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

// ══════════════════════ 下方：选中项的详情 ══════════════════════

fn detail(ui: &mut Ui, st: &mut Activity, h: f32) {
    let inner_h = (h - 34.0).max(80.0);
    Frame::NONE
        .fill(P::surface())
        .corner_radius(CornerRadius::same(theme::CARD_RADIUS))
        .stroke(Stroke::new(1.0, P::border()))
        .inner_margin(Margin::symmetric(20, 16))
        .show(ui, |ui| {
            let w = ui.available_width();
            ui.set_width(w);
            ui.set_max_width(w);
            ui.set_min_height(inner_h);
            ui.set_max_height(inner_h);
            ui.spacing_mut().item_spacing.y = 0.0;

            let Some(a) = st.focused() else {
                ui.add_space(4.0);
                ui.label(RichText::new("还没有选中任何过程").color(P::text_dim()).size(15.0));
                ui.add_space(7.0);
                ui.label(
                    RichText::new("程序空闲时不记录任何内容；下达指令后，这里会按时间铺开每一步。")
                        .color(P::text_faint())
                        .size(14.0),
                );
                return;
            };

            // ── 头部：类别徽标 + 轮次 + 标题 ──
            let (head, _) = ui.allocate_exact_size(Vec2::new(w, 24.0), Sense::hover());
            {
                let p = ui.painter();
                let text = if a.failed { ActKind::Error } else { a.kind };
                let font = FontId::proportional(13.0);
                let tw = text_w(ui, &font, text.label());
                let br = Rect::from_min_size(
                    Pos2::new(head.left(), head.top() + 1.0),
                    Vec2::new(tw + 18.0, 22.0),
                );
                p.rect_filled(br, CornerRadius::same(6), text.color());
                p.text(br.center(), Align2::CENTER_CENTER, text.label(), font, P::bg());

                let mut x = br.right() + 12.0;
                let meta = format!("第 {} 轮", a.round);
                p.text(
                    Pos2::new(x, head.center().y),
                    Align2::LEFT_CENTER,
                    &meta,
                    FontId::proportional(13.5),
                    P::text_faint(),
                );
                x += text_w(ui, &FontId::proportional(13.5), &meta) + 14.0;

                // 标题：等宽的标识符用等宽字体
                let font = if a.kind.mono() {
                    theme::mono_font(15.5)
                } else {
                    FontId::proportional(17.0)
                };
                let room = head.right() - x;
                let g = elide(ui, &a.title, font, P::text(), room);
                p.galley(Pos2::new(x, head.center().y - g.size().y * 0.5), g, P::text());

                // 右端：时刻与耗时
                let dur = a.dur(st.now);
                let when = if a.open() {
                    format!("{} 开始 · 进行中 {dur:.2}s", fmt_at(a.at - st.tz(), 0.5))
                } else {
                    format!(
                        "{} → {} · {dur:.2}s",
                        fmt_at(a.at - st.tz(), 0.5),
                        fmt_at(a.at - st.tz() + dur, 0.5)
                    )
                };
                let wf = theme::mono_font(12.5);
                let ww = text_w(ui, &wf, &when);
                p.text(
                    Pos2::new(head.right() - ww, head.center().y),
                    Align2::LEFT_CENTER,
                    when,
                    wf,
                    P::text_faint(),
                );
            }

            ui.add_space(10.0);
            let (rule, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
            ui.painter().hline(rule.x_range(), rule.center().y, Stroke::new(1.0, P::border()));
            ui.add_space(10.0);

            // ── 正文 ──
            let body = a.body.trim().to_owned();
            let avail = (inner_h - 46.0).max(40.0);
            if body.is_empty() {
                ui.label(RichText::new("（没有更多信息）").color(P::text_faint()).size(14.5));
                return;
            }
            let font = if a.kind.mono() {
                theme::mono_font(14.5)
            } else {
                FontId::proportional(16.0)
            };
            let col = if a.failed { ActKind::failed_color() } else { P::text() };
            ScrollArea::vertical()
                .id_salt("aox-act-detail")
                .max_height(avail)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_max_width(ui.available_width());
                    let mut job = egui::text::LayoutJob::default();
                    job.wrap.max_width = ui.available_width();
                    job.wrap.break_anywhere = a.kind.mono();
                    for (i, seg) in body.split('\n').enumerate() {
                        if i > 0 {
                            job.append(
                                "\n",
                                0.0,
                                TextFormat { font_id: font.clone(), ..Default::default() },
                            );
                        }
                        job.append(
                            seg,
                            0.0,
                            TextFormat { font_id: font.clone(), color: col, ..Default::default() },
                        );
                    }
                    ui.add(
                        egui::Label::new(job)
                            .selectable(true)
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                    ui.add_space(8.0);
                });
        });
}

// ─────────────────────────── 测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 导出 → 装回 → 接着聊。这条链路上有三个容易错的地方，一起钉住：
    /// 同一条记录被导出两次（水位没推进）、装回来的被当成新记录又写一遍
    /// （水位没对齐存档）、以及新一轮没有接在存档后面。
    #[test]
    fn acts_round_trip_and_flush_once() {
        let mut a = Activity::default();
        a.round_begin("看看目录", 0.0); // 输入
        a.model_begin(0.5); // 模型
        a.tool_begin("fs_ls", Some("fs"), "{}", 1.7); // 工具
        a.tool_end(false, "3 项", 2.0);
        a.end_round(2.4);
        a.note(ActKind::Info, "信息", "x", 2.4); // 系统

        let first = a.export_acts();
        assert_eq!(first.len(), 4, "输入 / 模型 / 工具 / 系统 各一条");
        assert!(a.export_acts().is_empty(), "同样的记录不该被导出两次");

        // 第二轮：只多导出第二轮的那一条
        a.round_begin("再写个文件", 3.0);
        a.end_round(3.4);
        assert_eq!(a.export_acts().len(), 1);

        // 装回来
        let mut b = Activity::default();
        b.load_acts(&first, 100.0);
        assert_eq!(b.acts.len(), 4);
        assert_eq!(b.round, 1);
        assert_eq!(b.next_seq, 4);
        assert_eq!(b.flushed, 4, "装回来的记录本来就在文件里，水位要跟着对齐");
        assert!(b.export_acts().is_empty(), "装回来的不该被当成新记录再写一遍");
        // 整批平移到「刚才」：最后一条结束在 now 之前一点点
        let last = b.acts.back().unwrap();
        assert!(last.end_t(100.0) < 100.0 && last.end_t(100.0) > 90.0);
        // 接着聊：轮次接着数、新一轮从右边长出来
        b.round_begin("继续", 100.0);
        assert_eq!(b.round, 2);
    }

    /// 存档里的坏行、或别的版本写出来的未知类别，都不该毁掉整条时间轴。
    #[test]
    fn load_acts_skips_bad_rows() {
        let mut a = Activity::default();
        let good = serde_json::json!({
            "kind": "tool", "round": 1, "plugin": "fs", "at": 1.0, "end": 2.0,
            "failed": false, "title": "fs_ls", "body": "", "seq": 7
        });
        a.load_acts(&[serde_json::json!({"nope": 1}), good], 50.0);
        assert_eq!(a.acts.len(), 1);
        assert_eq!(a.next_seq, 7);
        assert_eq!(a.flushed, 7);
    }
}
