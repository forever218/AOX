//! 等待态动画：单行 spinner + 模型思考过程的滚动回显。
//!
//! 仅在真实 TTY 下播放；重定向到文件/管道（自检、日志采集）时自动静默，
//! 避免把光标控制字符写进输出。
//!
//! 思考过程「即时显示、即时消失」：Provider 通过 `Status::push_reasoning` 持续喂入增量，
//! 后台任务把最近若干行原地滚动画出来；一次响应结束（`clear_reasoning`）后立刻收掉。

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::style as s;
use crate::llm::{thousands, TokenEstimate};

/// 简洁的 braille 旋转帧
const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// 每帧间隔（毫秒）
const FRAME_MS: u64 = 80;
/// 思考过程回显窗口的高度（行），超出只展示末尾
const REASONING_ROWS: usize = 4;
/// 思考过程在内存里保留的字符上限（超出丢弃头部，避免长会话堆积）
const REASONING_KEEP: usize = 4000;
/// 思考过程每行的左侧缩进（纯 ASCII，确保按宽度折算不会算错）
const REASONING_INDENT: &str = "  ";

/// 动画是否可用：默认只在真实终端里播放，避免污染管道/重定向输出。
/// 可用环境变量 `AOX_ANIM` 覆盖：`1` 强制开启（便于录制/调试），`0` 强制关闭。
pub fn supported() -> bool {
    match std::env::var("AOX_ANIM").ok().as_deref() {
        Some("1") | Some("true") | Some("on") => true,
        Some("0") | Some("false") | Some("off") => false,
        _ => std::io::stdout().is_terminal(),
    }
}

/// UI 与动画任务共享的状态：当前提示文字 + 本轮开始时间 + 思考过程缓冲 + 输出估算。
pub struct Status {
    label: Mutex<String>,
    started: Instant,
    /// 模型思考过程的实时增量（仅流式思考模型非空）
    reasoning: Mutex<String>,
    /// 输出侧（思考 + 正文）收到的字符数，用来实时估一个在涨的 token 数。
    ///
    /// 只估算 —— 流式响应里服务端直到最后一帧才给 usage，中途没有别的来源。
    /// 一轮结束后由 `turn_usage` 给出精确值，两者不会同时出现在屏幕上。
    out: Mutex<TokenEstimate>,
}

impl Status {
    pub fn new(label: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            label: Mutex::new(label.into()),
            started: Instant::now(),
            reasoning: Mutex::new(String::new()),
            out: Mutex::new(TokenEstimate::default()),
        })
    }

    /// 更新提示文字（如「思考中」→「执行 fs_read」）
    pub fn set(&self, label: impl Into<String>) {
        if let Ok(mut l) = self.label.lock() {
            *l = label.into();
        }
    }

    /// 追加一段思考过程增量。由 Provider 的流式回调直接驱动，会被高频调用。
    pub fn push_reasoning(&self, delta: &str) {
        self.count(delta);
        let Ok(mut r) = self.reasoning.lock() else { return };
        r.push_str(delta);
        if r.len() > REASONING_KEEP {
            let mut cut = r.len() - REASONING_KEEP;
            while cut < r.len() && !r.is_char_boundary(cut) {
                cut += 1; // 不要从多字节字符中间切开
            }
            r.drain(..cut);
        }
    }

    /// 追加一段正文增量：正文本身由别处留存，这里只要拿来数 token。
    pub fn push_content(&self, delta: &str) {
        self.count(delta);
    }

    /// 输出侧的字符计数。推理与正文都要数 —— 服务端的 completion_tokens
    /// 本来就把推理算在里面，只数正文会明显偏低。
    fn count(&self, delta: &str) {
        if let Ok(mut o) = self.out.lock() {
            o.add(delta);
        }
    }

    /// 清空思考过程：一次响应结束后调用，实现「即时消失」。
    /// 注意**不清** token 估算：一轮里模型可能被调用很多次，
    /// 计数要跨响应累加，清了就等于只报最后一次。
    pub fn clear_reasoning(&self) {
        if let Ok(mut r) = self.reasoning.lock() {
            r.clear();
        }
    }

    fn snapshot(&self) -> (String, f64, Option<String>, u64) {
        let label = self.label.lock().map(|g| g.clone()).unwrap_or_default();
        let reasoning = self
            .reasoning
            .lock()
            .ok()
            .map(|g| g.clone())
            .filter(|r| !r.trim().is_empty());
        let tok = self.out.lock().map(|o| o.tokens()).unwrap_or(0);
        (label, self.started.elapsed().as_secs_f64(), reasoning, tok)
    }
}

/// 一个正在播放的等待动画。丢弃时会自动停止并中止后台任务。
pub struct Spinner {
    stop: Arc<Notify>,
    handle: JoinHandle<()>,
}

impl Spinner {
    /// 启动动画；提示文字与耗时来自共享的 `Status`。
    pub fn start(status: Arc<Status>) -> Self {
        let stop = Arc::new(Notify::new());
        let stop_task = stop.clone();
        let width = term_width();
        let handle = tokio::spawn(async move {
            let mut i = 0usize;
            // 当前在屏幕上占用的行数：块高度会随思考过程出现/消失变化，重绘时要先回退。
            let mut drawn = 0usize;
            loop {
                draw(FRAMES[i % FRAMES.len()], &status, width, &mut drawn);
                i += 1;
                tokio::select! {
                    _ = stop_task.notified() => break,
                    _ = tokio::time::sleep(Duration::from_millis(FRAME_MS)) => {}
                }
            }
            clear_block(&mut drawn);
        });
        Self { stop, handle }
    }

    /// 停止动画并清除该行。会等待后台任务收尾，确保「清理」先于后续输出。
    pub async fn stop(&mut self) {
        self.stop.notify_one();
        let _ = (&mut self.handle).await;
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        // 兜底：任务被中断（如 Ctrl+C）而未显式 stop 时，通知并中止后台任务
        self.stop.notify_one();
        self.handle.abort();
    }
}

/// 原地重绘一行状态；若有思考过程，则在其下方滚动展示末尾若干行。
fn draw(frame: &str, status: &Status, width: usize, drawn: &mut usize) {
    let (label, secs, reasoning, tok) = status.snapshot();
    let mut head = format!(
        "{} {} {}",
        s::cyan(frame),
        s::bold(&label),
        s::dim(&format!("{secs:.1}s"))
    );
    // 输出 token 实时滚动。带 `~` 是因为流式过程中服务端还没报数，
    // 这个值是按字符估的；一轮结束时会用精确值另报一条明细。
    if tok > 0 {
        head.push_str(&format!("  {}", s::dim(&format!("~{} tok", thousands(tok)))));
    }
    let mut lines = vec![head];
    if let Some(text) = reasoning {
        // 留出缩进与右侧一点余量，避免整行折行把光标定位算错
        let wrap = width.saturating_sub(REASONING_INDENT.len() + 2).max(24);
        for l in tail_lines(&text, wrap, REASONING_ROWS) {
            lines.push(format!("{REASONING_INDENT}{}", s::dim(&l)));
        }
    }
    repaint(&lines, drawn);
}

/// 原地重绘一个多行块：先回退到块首，逐行覆盖，再处理高度变化（可能变高也可能变矮）。
fn repaint(lines: &[String], drawn: &mut usize) {
    let mut out = String::new();
    if *drawn > 1 {
        out.push_str(&format!("\x1b[{}A", *drawn - 1));
    }
    for (i, l) in lines.iter().enumerate() {
        out.push_str("\r\x1b[2K");
        out.push_str(l);
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    // 本次比上次矮：清掉多出来的行，再把光标升回块尾
    if *drawn > lines.len() {
        for _ in lines.len()..*drawn {
            out.push_str("\n\r\x1b[2K");
        }
        out.push_str(&format!("\x1b[{}A", *drawn - lines.len()));
    }
    *drawn = lines.len();
    print!("{out}");
    let _ = std::io::stdout().flush();
}

/// 清掉整块并让光标停在块首行，后续输出从这里接上。
fn clear_block(drawn: &mut usize) {
    let n = (*drawn).max(1);
    let mut out = String::new();
    if n > 1 {
        out.push_str(&format!("\x1b[{}A", n - 1));
    }
    for i in 0..n {
        out.push_str("\r\x1b[2K");
        if i + 1 < n {
            out.push('\n');
        }
    }
    if n > 1 {
        out.push_str(&format!("\x1b[{}A", n - 1));
    }
    *drawn = 0;
    print!("{out}");
    let _ = std::io::stdout().flush();
}

/// 取文本末尾若干行用于滚动回显：按换行切分 → 按显示宽度折行 → 取尾部 `max` 行。
fn tail_lines(text: &str, width: usize, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split(['\n', '\r']) {
        if raw.trim().is_empty() {
            continue; // 思考过程里的空行只是分段，不必占窗口高度
        }
        let mut cur = String::new();
        let mut w = 0usize;
        for ch in raw.chars() {
            let cw = if ch.is_ascii() { 1 } else { 2 };
            if w + cw > width && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                w = 0;
            }
            cur.push(ch);
            w += cw;
        }
        out.push(cur);
    }
    if out.len() > max {
        out.split_off(out.len() - max)
    } else {
        out
    }
}

fn term_width() -> usize {
    termimad::crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(100)
}
