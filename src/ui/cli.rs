use std::io::Write as _;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};

use super::spinner::{self, Spinner, Status};
use super::style as s;
use super::{AgentEvent, AuthDecision, Ui};
use crate::history::UsageRecord;
use crate::llm::{thousands, StreamSink};

/// 把模型的思考过程与正文增量实时喂给共享状态，由等待动画在屏幕上滚动回显。
///
/// 它不直接画屏：渲染由 spinner 后台任务负责（80ms 一帧），
/// 因此即便 Provider 高频回调也只是往缓冲区追加字符串，不会拖慢流式读取。
struct ReasoningSink {
    status: Arc<Status>,
}

impl StreamSink for ReasoningSink {
    fn on_reasoning(&self, delta: &str) {
        self.status.push_reasoning(delta);
    }

    /// 正文也喂进去，只为数 token —— 正文本身由 `TurnLog` 留存并渲染。
    fn on_content(&self, delta: &str) {
        self.status.push_content(delta);
    }

    fn on_finish(&self) {
        // 一次响应结束 → 思考过程立刻消失（token 计数继续留着，一轮里会多次请求）
        self.status.clear_reasoning();
    }
}

/// 一次工具调用的记录（用于折叠 / 展开）
struct Step {
    name: String,
    args: String,
    /// (是否错误, 结果摘要)
    result: Option<(bool, String)>,
}

/// 一轮任务（一次用户指令 → 模型+工具循环）的完整过程。
/// 进行中只在终端显示一行状态；结束后折叠成一行摘要，按需展开。
struct TurnLog {
    steps: Vec<Step>,
    /// 中间过程的助手说明（不单独展示，展开时可见）
    interim: Vec<String>,
    /// 过程中的提示 / 警告 / 错误
    notes: Vec<(char, String)>,
    /// 最终输出：最后一次不带工具调用的助手文本
    final_text: Option<String>,
    started: Instant,
}

impl TurnLog {
    fn new() -> Self {
        Self {
            steps: Vec::new(),
            interim: Vec::new(),
            notes: Vec::new(),
            final_text: None,
            started: Instant::now(),
        }
    }

    fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

/// 命令行界面实现
pub struct CliUi {
    lines: Lines<BufReader<tokio::io::Stdin>>,
    spinner: Option<Spinner>,
    status: Option<Arc<Status>>,
    /// 进行中的一轮任务
    turn: Option<TurnLog>,
    /// 最近一次已完成的任务，供 /expand 展开
    last: Option<TurnLog>,
}

impl CliUi {
    pub fn new() -> Self {
        Self {
            lines: BufReader::new(tokio::io::stdin()).lines(),
            spinner: None,
            status: None,
            turn: None,
            last: None,
        }
    }

    async fn read_line(&mut self) -> Result<Option<String>> {
        std::io::stdout().flush()?;
        Ok(self.lines.next_line().await?)
    }

    fn set_label(&self, label: &str) {
        if let Some(st) = &self.status {
            st.set(label);
        }
    }

    async fn start_spinner(&mut self) {
        if self.spinner.is_some() || !spinner::supported() {
            return;
        }
        if let Some(st) = self.status.clone() {
            self.spinner = Some(Spinner::start(st));
        }
    }

    async fn stop_spinner(&mut self) {
        if let Some(mut sp) = self.spinner.take() {
            sp.stop().await;
        }
    }

    /// 中断当前任务：停止动画并丢弃半截记录（不回填到 /expand）。
    pub async fn abort_turn(&mut self) {
        self.stop_spinner().await;
        self.status = None;
        self.turn = None;
    }

    /// 展开最近一次被折叠的任务过程。
    pub fn expand_last_turn(&self) {
        let Some(t) = self.last.as_ref() else {
            println!("{} {}", s::yellow("!"), s::dim("暂无可展开的记录（尚未完成过任务）"));
            return;
        };
        println!(
            "{}",
            s::dim(&format!("──────────────── 上次任务过程 · {} 步 · {:.1}s ────────────────", t.steps.len(), t.elapsed()))
        );
        for (i, text) in t.interim.iter().enumerate() {
            println!("{}", s::dim(&format!("[中间说明 {}]", i + 1)));
            println!("{}", s::render_markdown(text));
        }
        for (i, st) in t.steps.iter().enumerate() {
            println!(
                "{:>3}. {} {} {}",
                i + 1,
                s::yellow("→"),
                s::bold_white(&st.name),
                s::dim(&one_line(&st.args, 160))
            );
            match &st.result {
                Some((false, p)) => println!("     {} {}", s::green("✓"), s::dim(&one_line(p, 200))),
                Some((true, p)) => println!("     {} {}", s::red("✗"), s::red(&one_line(p, 200))),
                None => println!("     {}", s::dim("（未完成）")),
            }
        }
        if let Some(f) = &t.final_text {
            println!("{}", s::dim("最终输出："));
            println!("{}", s::render_markdown(f));
        }
        println!("{}", s::dim("────────────────────────────────────────────────────────────"));
    }

    /// 回看**当前对话逐轮**的 token 账（`/usage`）。
    ///
    /// 数据来自 agent 手里的账本 —— 也就是从对话文件读回来的那一份，所以切过对话、
    /// 重启过程序都还在。逐轮列出来是有必要的：只给一个总数，就答不出「哪一轮最贵」，
    /// 而最贵的那一轮往往正是出问题的那一轮。
    pub fn show_usage(&self, log: &[UsageRecord]) {
        let rows: Vec<&UsageRecord> = log.iter().filter(|r| !r.is_empty()).collect();
        if rows.is_empty() {
            println!(
                "{} {}",
                s::yellow("!"),
                s::dim("当前对话还没有用量记录（尚未完成过任务，或端点不返回 usage）")
            );
            return;
        }
        let calls: u32 = rows.iter().map(|r| r.calls).sum();
        println!();
        println!(
            "{}",
            s::bold(&format!("token 用量 · 本次对话 {} 轮 · {} 次模型调用", rows.len(), calls))
        );
        if let Some(v) = rows.last().and_then(|r| r.session_total) {
            println!("  {}  {}", s::dim("合计"), s::bold_cyan(&thousands(v)));
        }
        println!();
        for r in &rows {
            let mut line = vec![
                s::dim(&format!("#{:<3}", r.round)),
                // 先把数字补齐再上色：ANSI 转义符也算字符，对上色后的串做宽度填充会错位
                s::bold_white(&format!("{:>9}", thousands(r.usage.total.unwrap_or(0)))),
                s::dim(&format!("{} 次调用", r.calls)),
            ];
            if let Some(v) = r.usage.prompt {
                line.push(s::dim(&format!("输入 {}", thousands(v))));
            }
            if let Some(v) = r.usage.completion {
                line.push(s::dim(&format!("输出 {}", thousands(v))));
            }
            if let Some(v) = r.usage.reasoning {
                line.push(s::dim(&format!("推理 {}", thousands(v))));
            }
            if let Some(v) = r.usage.cached {
                line.push(s::green(&format!("缓存命中 {}", thousands(v))));
            }
            println!("  {}", line.join(&s::dim("  ·  ")));
        }
        if let Some(v) = rows.last().and_then(|r| r.last_prompt) {
            println!();
            println!("  {} {}", s::dim("当前上下文"), s::bold_white(&thousands(v)));
        }
        println!();
    }

    /// 打印一轮结束的 token 明细。
    ///
    /// 分三行是有取舍的：第一行放所有人都要看的输入 / 输出 / 合计，
    /// 第二行是细项（推理、缓存命中），第三行才是调用次数与上下文规模。
    /// 端点没给的项直接不出现 —— 宁可少一行，也不要拿 0 去冒充「没有消耗」。
    fn print_usage(r: &UsageRecord) {
        println!();
        println!("{}", s::dim(&format!("  token 用量 · 第 {} 轮", r.round)));

        let mut row1: Vec<String> = Vec::new();
        if let Some(v) = r.usage.prompt {
            row1.push(format!("{} {}", s::dim("输入"), s::bold_white(&thousands(v))));
        }
        if let Some(v) = r.usage.completion {
            row1.push(format!("{} {}", s::dim("输出"), s::bold_white(&thousands(v))));
        }
        if let Some(v) = r.usage.total {
            row1.push(format!("{} {}", s::dim("合计"), s::bold_cyan(&thousands(v))));
        }
        if !row1.is_empty() {
            println!("    {}", row1.join(&s::dim("  ·  ")));
        }

        let mut row2: Vec<String> = Vec::new();
        if let Some(v) = r.usage.reasoning {
            row2.push(format!("{} {}", s::dim("其中推理"), s::bold_white(&thousands(v))));
        }
        if let Some(v) = r.usage.cached {
            row2.push(format!("{} {}", s::dim("缓存命中"), s::green(&thousands(v))));
        }
        if let Some(v) = r.usage.cache_miss {
            row2.push(format!("{} {}", s::dim("缓存未命中"), s::dim(&thousands(v))));
        }
        if !row2.is_empty() {
            println!("    {}", row2.join(&s::dim("  ·  ")));
        }

        let mut row3: Vec<String> =
            vec![format!("{} {}", s::dim("模型调用"), s::bold_white(&format!("{} 次", r.calls)))];
        if let Some(v) = r.last_prompt {
            row3.push(format!("{} {}", s::dim("末次输入"), s::dim(&thousands(v))));
        }
        if let Some(v) = r.session_total {
            row3.push(format!(
                "{} {}",
                s::dim("本次对话累计"),
                s::dim(&format!("{}（{} 次调用）", thousands(v), r.session_calls))
            ));
        }
        println!("    {}", row3.join(&s::dim("  ·  ")));
        println!();
    }

    /// 任务进行中的事件：折叠进缓冲，不在终端逐条刷屏。
    fn emit_into_turn(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AssistantText(text) => {
                // 只有「不带工具调用的回复」才会走到这里，它就是最终输出
                if let Some(t) = self.turn.as_mut() {
                    if let Some(prev) = t.final_text.replace(text) {
                        t.interim.push(prev);
                    }
                }
            }
            AgentEvent::InterimText(text) => {
                // 伴随工具调用的过程性说明（如「我先看看有哪些插件」）——折叠进过程区
                if let Some(t) = self.turn.as_mut() {
                    t.interim.push(text);
                }
            }
            AgentEvent::ToolCall { name, args } => {
                if let Some(t) = self.turn.as_mut() {
                    t.steps.push(Step { name, args: args.to_string(), result: None });
                }
            }
            AgentEvent::ToolRunning { name, .. } => {
                self.set_label(&format!("执行 {name}"));
            }
            AgentEvent::ToolResult { name, is_error, preview } => {
                if let Some(t) = self.turn.as_mut() {
                    // 挂到最近一个同名且尚未回填结果的步骤上
                    match t.steps.iter_mut().rev().find(|s| s.name == name && s.result.is_none()) {
                        Some(st) => st.result = Some((is_error, preview)),
                        None => t.steps.push(Step {
                            name,
                            args: String::new(),
                            result: Some((is_error, preview)),
                        }),
                    }
                }
                self.set_label("思考中");
            }
            AgentEvent::Info(m) => self.push_note('i', m),
            AgentEvent::Warn(m) => self.push_note('w', m),
            AgentEvent::Error(m) => self.push_note('e', m),
        }
    }

    fn push_note(&mut self, kind: char, msg: String) {
        if let Some(t) = self.turn.as_mut() {
            t.notes.push((kind, msg));
        }
    }

    fn print_note(kind: char, msg: &str) {
        match kind {
            'e' => println!("{} {}", s::red("✗"), s::red(msg)),
            'w' => println!("{} {}", s::yellow("!"), s::yellow(msg)),
            _ => println!("{} {}", s::cyan("·"), s::dim(msg)),
        }
    }

    /// 打印授权询问框并读取用户选择（不含动画控制）
    async fn ask_authorization(&mut self, tool_name: &str, args: &Value) -> AuthDecision {
        let pretty = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
        let width = term_width().saturating_sub(2);
        println!();
        println!("{}", s::magenta("┌─ 授权请求 ────────────────────────────────"));
        println!("{} {} {}", s::magenta("│"), s::dim("工具"), s::bold_cyan(tool_name));
        for line in pretty.lines().take(20) {
            println!("{} {}", s::magenta("│"), s::dim(&clip(line, width)));
        }
        println!(
            "{} {}",
            s::magenta("│"),
            s::dim("[y]允许一次   [a]本会话始终允许   [n]拒绝   (回车=y)")
        );
        print!("{} ", s::magenta("└─ 请选择 >"));
        loop {
            match self.read_line().await {
                Ok(Some(line)) => {
                    let ans = line.trim().to_ascii_lowercase();
                    match ans.as_str() {
                        "" | "y" | "yes" => return AuthDecision::Allow,
                        "a" | "always" => return AuthDecision::AllowSession,
                        "n" | "no" => return AuthDecision::Deny,
                        _ => {
                            print!("{} ", s::magenta("无效输入，请输入 y / a / n >"));
                            continue;
                        }
                    }
                }
                _ => return AuthDecision::Deny,
            }
        }
    }
}

#[async_trait]
impl Ui for CliUi {
    async fn read_user_input(&mut self) -> Result<Option<String>> {
        print!("\n{} ", s::bold_cyan("你 >"));
        self.read_line().await
    }

    fn emit(&mut self, event: AgentEvent) {
        // 任务进行中：过程折叠进缓冲，只保留最终输出
        if self.turn.is_some() {
            self.emit_into_turn(event);
            return;
        }
        match event {
            // ── 助手回复：按 Markdown 渲染（标题/代码块/列表/表格）
            AgentEvent::AssistantText(text) | AgentEvent::InterimText(text) => {
                println!();
                println!("{}", s::render_markdown(&text));
            }
            // ── 工具调用：黄色箭头 + 加粗工具名 + 暗色参数
            AgentEvent::ToolCall { name, args } => {
                println!(
                    "{} {} {}",
                    s::yellow("→"),
                    s::bold_white(&name),
                    s::dim(&format!("参数 {}", one_line(&args.to_string(), 200)))
                );
            }
            AgentEvent::ToolRunning { name, timeout_secs } => {
                println!(
                    "  {}",
                    s::dim(&format!("…{name} 执行中（超时 {timeout_secs} 秒，Ctrl+C 中断）"))
                );
            }
            // ── 工具结果：绿/红标记 + 单行摘要
            AgentEvent::ToolResult { name, is_error, preview } => {
                let (mark, tag) = if is_error {
                    (s::red("✗"), s::red("失败"))
                } else {
                    (s::green("✓"), s::green("完成"))
                };
                println!(
                    "{} {} {}  {}",
                    mark,
                    s::bold(&name),
                    tag,
                    s::dim(&one_line(&preview, 160))
                );
            }
            AgentEvent::Info(msg) => println!("{} {}", s::cyan("·"), s::dim(&msg)),
            AgentEvent::Warn(msg) => println!("{} {}", s::yellow("!"), s::yellow(&msg)),
            AgentEvent::Error(msg) => println!("{} {}", s::red("✗"), s::red(&msg)),
        }
    }

    async fn turn_start(&mut self) {
        self.turn = Some(TurnLog::new());
        self.status = Some(Status::new("思考中"));
        self.start_spinner().await;
    }

    async fn turn_end(&mut self) {
        self.stop_spinner().await;
        self.status = None;
        let Some(t) = self.turn.take() else { return };

        // 过程折叠为一行摘要；/expand 可按需展开
        if !t.steps.is_empty() {
            println!(
                "{} {} {}",
                s::dim("▸"),
                s::bold(&format!("已折叠 {} 步工具调用", t.steps.len())),
                s::dim(&format!("· {:.1}s · /expand 展开", t.elapsed()))
            );
        }
        for (kind, msg) in &t.notes {
            Self::print_note(*kind, msg);
        }
        match t.final_text.as_deref() {
            Some(text) => {
                println!();
                println!("{}", s::render_markdown(text));
            }
            // 兜底：正常轮次一定有最终答复，这里只在异常时提示，避免「结束却什么都没显示」
            None if !t.steps.is_empty() => {
                println!(
                    "{}",
                    s::yellow("! 本轮没有产出最终答复（任务可能被模型提前结束），输入 /expand 查看执行过程")
                );
            }
            None => {}
        }
        self.last = Some(t);
    }

    async fn turn_usage(&mut self, rec: &UsageRecord) {
        // 端点不报 usage 就整块不显示 —— 拿一串 0 去糊弄用户比什么都不说更糟
        if rec.is_empty() {
            return;
        }
        Self::print_usage(rec);
    }

    async fn set_status(&mut self, label: &str) {
        self.set_label(label);
    }

    fn stream_sink(&self) -> Option<Box<dyn StreamSink + Send + Sync + '_>> {
        let status = self.status.clone()?;
        Some(Box::new(ReasoningSink { status }))
    }

    async fn authorize(&mut self, tool_name: &str, args: &Value) -> AuthDecision {
        // 暂停动画，避免 spinner 覆盖授权框；问完再恢复
        let was_running = self.spinner.is_some();
        self.stop_spinner().await;
        let decision = self.ask_authorization(tool_name, args).await;
        if was_running {
            self.set_label("思考中");
            self.start_spinner().await;
        }
        decision
    }
}

/// /demo：打印一段样例 Markdown 渲染效果，方便检查终端配色
pub fn print_demo() {
    let sample = "# 一级标题\n\n\
## 二级标题\n\n\
这是普通段落，包含 **加粗**、*斜体* 和 `行内代码`。\n\n\
- 列表项一\n\
- 列表项二\n\
  - 嵌套项\n\n\
> 引用块：说明性文字。\n\n\
```python\n\
def hello(name: str) -> str:\n    return f\"你好, {name}\"\n\
```\n\n\
| 工具 | 类型 | 状态 |\n\
|------|------|------|\n\
| fs_read | plugin | ok |\n\
| create_plugin | meta | ok |\n";
    println!();
    println!("{}", s::render_markdown(sample));
}

fn term_width() -> usize {
    termimad::crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(100)
}

/// 折叠换行/多余空白，便于单行展示
fn one_line(s: &str, max: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&joined, max)
}

/// 按显示宽度截断（ASCII 记 1，其他记 2）
fn clip(s: &str, max: usize) -> String {
    let mut width = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let w = if ch.is_ascii() { 1 } else { 2 };
        if width + w > max {
            out.push('…');
            return out;
        }
        width += w;
        out.push(ch);
    }
    out
}
