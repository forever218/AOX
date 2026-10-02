//! 对话历史落盘。
//!
//! 一次对话一个文件，落在 `<app_root>/history/`，文件名就是它自己的时间戳：
//!
//! ```text
//! 20261002-153912-a1b2.jsonl
//! ```
//!
//! 内容是一行一条记录的 JSONL，**首行是 meta**：
//!
//! ```text
//! {"created":1759390752,"id":"20261002-153912-a1b2","kind":"meta","model":"deepseek-chat"}
//! {"content":"帮我给项目加历史记录功能","kind":"message","role":"user"}
//! {"content":null,"kind":"message","role":"assistant","tool_calls":[...]}
//! {"content":"...","is_error":false,"kind":"message","name":"fs_ls","role":"tool"}
//! ```
//!
//! 为什么是 JSONL 而不是一个 JSON 数组：
//!   - 追加是 O(1)。长任务可能有几百条消息，每轮重写整个文件既慢又容易写坏；
//!   - 进程被强杀时，已落地的行仍然完整，可以正常读出来；
//!   - 单行损坏只影响那一行，不会让整个文件解析失败。
//!
//! 消息体直接复用 `Message`（OpenAI wire 格式），所以这个文件既是给人看的记录，
//! 也天然是一份可回放的消息流 —— 将来要加「恢复对话」不需要改格式。

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::message::Message;

/// 历史目录名（相对应用根）。
pub const DIR_NAME: &str = "history";
const EXT: &str = "jsonl";

/// 列表里默认返回多少个对话。
const DEFAULT_LIST_LIMIT: usize = 60;
/// 一个对话最多留多少个字符当标题。
const TITLE_MAX_CHARS: usize = 46;
/// 标题只在前若干行里找：首条用户消息总在第 2 行，多给几行余量即可，
/// 免得为了一个标题把整条 tool 结果也解析一遍。
const TITLE_SCAN_LINES: usize = 6;

/// 宿主注入的提示语前缀。这类「用户消息」不是真人说的，不该拿来当标题。
const HOST_PREFIX: &str = "（宿主提示）";

/// 历史目录：`<app_root>/history`。
pub fn dir(root: &Path) -> PathBuf {
    root.join(DIR_NAME)
}

// ─────────────────────────── 本地时间 ───────────────────────────

struct Local {
    year: u16,
    month: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
}

/// 取本地时间。
///
/// 自己算时区偏移要拖进 chrono / time 之类的依赖，而宿主需要的只是一个
/// 「人看得懂的文件名」，所以直接问操作系统 —— Windows 下就是 `GetLocalTime`，
/// 零新增 crate。非 Windows 平台退回 UTC（文件名仍可排序，只是时区不同）。
#[cfg(windows)]
fn local_now() -> Local {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    // windows-sys 的 SYSTEMTIME 是纯 POD（只有 Clone/Copy），没有 Default，只能零初始化
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    Local {
        year: st.wYear,
        month: st.wMonth,
        day: st.wDay,
        hour: st.wHour,
        minute: st.wMinute,
        second: st.wSecond,
    }
}

#[cfg(not(windows))]
fn local_now() -> Local {
    // 拿不到时区，就如实按 UTC 记，不要拿「东八区近似」去骗人。
    let secs = unix_now();
    let (y, m, d, hh, mm, ss) = civil_from_unix(secs);
    Local { year: y, month: m, day: d, hour: hh, minute: mm, second: ss }
}

/// Unix 秒 → 年月日时分秒（Howard Hinnant 的 civil_from_days）。
#[cfg(not(windows))]
fn civil_from_unix(secs: u64) -> (u16, u16, u16, u16, u16, u16) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        y as u16,
        m as u16,
        d as u16,
        (rem / 3600) as u16,
        ((rem % 3600) / 60) as u16,
        (rem % 60) as u16,
    )
}

/// 当前 Unix 秒。
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 由 `YYYYMMDD-HHMMSS` 生成「10-02 15:39」这样的显示串。
///
/// 时间直接从文件名解析，不做任何时区换算 —— 文件名本来就是本地时间，
/// 这比存一个 UTC 时间戳再想办法转回来简单得多，也不会算错。
fn display_time(id: &str, this_year: u16) -> String {
    let b = id.as_bytes();
    if b.len() < 15 || b[8] != b'-' {
        return id.to_string();
    }
    let s = |a: usize, z: usize| id.get(a..z).unwrap_or("");
    let (y, mo, d, hh, mm) = (s(0, 4), s(4, 6), s(6, 8), s(9, 11), s(11, 13));
    if [y, mo, d, hh, mm].iter().any(|p| p.len() < 2 || !p.bytes().all(|c| c.is_ascii_digit())) {
        return id.to_string();
    }
    if y == this_year.to_string() {
        format!("{mo}-{d} {hh}:{mm}")
    } else {
        // 往年的补上年份，否则「10-02」会让人误以为是今年
        format!("{y}-{mo}-{d} {hh}:{mm}")
    }
}

// ─────────────────────────── 记录 ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// Unix 秒，便于程序比较
    #[serde(default)]
    pub created: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// 一行的类型标记。
///
/// 刻意**不用** serde 的内部标签（`#[serde(tag = "kind")]`）+ `flatten`：
/// 那个组合在「枚举 + 展平结构体 + 未知字段」上有微妙的坑，而这里要的是
/// 「写出去的文件十年后还读得回来」。所以手工拼 `Value`：加一个 `kind` 键，
/// 其余字段原样来自 `Message` —— 读回来时多出来的键会被 serde 忽略。
fn meta_line(meta: &Meta) -> Value {
    let mut v = serde_json::to_value(meta).unwrap_or_else(|_| json!({}));
    if let Value::Object(m) = &mut v {
        m.insert("kind".into(), json!("meta"));
    }
    v
}

fn message_line(msg: &Message, is_error: Option<bool>) -> Value {
    let mut v = serde_json::to_value(msg).unwrap_or_else(|_| json!({}));
    if let Value::Object(m) = &mut v {
        m.insert("kind".into(), json!("message"));
        if let Some(e) = is_error {
            m.insert("is_error".into(), json!(e));
        }
    }
    v
}

fn kind_of(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    let k = v.get("kind")?.as_str()?;
    if k != "meta" && k != "message" {
        return None;
    }
    Some(v)
}

// ─────────────────────────── 写入 ───────────────────────────

/// 当前对话的落盘器。
pub struct Log {
    path: PathBuf,
    file: File,
    meta: Meta,
}

impl Log {
    /// 新建一个对话文件并写入 meta 行。
    ///
    /// `keep`：创建后把目录里超出的旧对话裁掉（0 = 不限制）。放在这里而不是
    /// 启动时，是因为「保留最近 N 个」本来就只在产生新对话时才有意义。
    pub fn create(dir: &Path, model: Option<&str>, root: &Path, keep: usize) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("无法创建历史目录 {}", dir.display()))?;
        let meta = Meta {
            id: new_id(dir),
            created: unix_now(),
            model: model.map(str::to_string),
            root: Some(root.display().to_string()),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        };
        let path = dir.join(format!("{}.{EXT}", meta.id));
        let file = OpenOptions::new().create(true).write(true).append(true).open(&path)?;
        let mut log = Self { path, file, meta };
        let head = meta_line(&log.meta);
        write_line_to(&mut log.file, &head)?;
        if keep > 0 {
            prune(dir, keep);
        }
        Ok(log)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加一条消息。写入失败不返回错误 —— 历史记录不该让对话中断。
    pub fn append(&mut self, msg: &Message, is_error: Option<bool>) {
        if msg.role == "system" {
            return; // 系统提示每轮都在变，又长又没用，不入档
        }
        let line = message_line(msg, is_error);
        let _ = write_line_to(&mut self.file, &line);
    }

    /// 用给定的消息列表重写整个文件。
    ///
    /// 用于「中断回滚」：内存里的消息被丢弃了，文件必须跟着回到同一状态，
    /// 否则会留下一段「有 tool_calls 但没有结果」的半截记录
    /// （将来要恢复这段对话时，那种记录会直接让请求被拒）。
    ///
    /// 实现上**不能**对 append 句柄直接 `set_len`：Windows 上 append 打开的句柄
    /// 没有截断权限，会返回 `os error 5（拒绝访问）`；而且 `O_APPEND` 会让 `seek`
    /// 形同虚设，继续写还是落在旧内容后面（这两条都实测过）。
    /// 所以另开一个截断句柄重写全文，再把 append 句柄换回来。
    pub fn rewrite(&mut self, msgs: &[Message]) {
        let Ok(mut f) =
            OpenOptions::new().create(true).write(true).truncate(true).open(&self.path)
        else {
            return;
        };
        let head = meta_line(&self.meta);
        if write_line_to(&mut f, &head).is_err() {
            return;
        }
        for m in msgs.iter().filter(|m| m.role != "system") {
            if write_line_to(&mut f, &message_line(m, None)).is_err() {
                return;
            }
        }
        drop(f);
        // 换回追加句柄，后续 append 仍是「永远写在末尾」，不会覆盖已有内容
        if let Ok(f) = OpenOptions::new().write(true).append(true).open(&self.path) {
            self.file = f;
        }
    }
}

fn write_line_to(file: &mut File, v: &Value) -> Result<()> {
    let line = serde_json::to_string(v)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

/// 生成 `<YYYYMMDD>-<HHMMSS>-<4位十六进制>` 形式的 id，并保证同目录下不重名。
fn new_id(dir: &Path) -> String {
    let t = local_now();
    let stamp = format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    );
    for attempt in 0..64u32 {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let salt = nanos ^ std::process::id().wrapping_mul(2_654_435_761) ^ attempt.wrapping_mul(0x9E37_79B9);
        let id = format!("{stamp}-{:04x}", (salt & 0xffff) as u16);
        if !dir.join(format!("{id}.{EXT}")).exists() {
            return id;
        }
    }
    format!("{stamp}-{:04x}", (unix_now() & 0xffff) as u16)
}

// ─────────────────────────── 读取 ───────────────────────────

/// 列表里的一项。
#[derive(Debug, Clone)]
pub struct Summary {
    /// 文件名主干（时间戳 + 盐），也是 `/history show` 与 GUI 用的标识
    pub id: String,
    pub path: PathBuf,
    /// 首条用户消息（截断）；拿不到就用占位文案
    pub title: String,
    /// 消息条数（不含 meta）
    pub messages: usize,
    /// 「10-02 15:39」这样的显示时间（直接取自文件名，不做时区换算）
    pub time: String,
}

fn collect(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let id = name.strip_suffix(&format!(".{EXT}"))?.to_string();
            Some((id, e.path()))
        })
        .collect();
    // 文件名以时间戳开头 → 字典序即时间序
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files
}

/// 列出历史对话，**最新的在前**。
///
/// 只读前几行就能拿到标题（首条用户消息永远是第 2 行），条数靠数行数得出 ——
/// 不解析全文，几百个文件也只是顺序读磁盘。
pub fn list(dir: &Path, limit: usize) -> Vec<Summary> {
    let this_year = local_now().year;
    collect(dir)
        .into_iter()
        .take(limit)
        .filter_map(|(id, path)| read_summary(id, path, this_year))
        .collect()
}

fn read_summary(id: String, path: PathBuf, this_year: u16) -> Option<Summary> {
    let file = File::open(&path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();

    // 第 1 行是 meta（含 id / 模型 / 版本）：读掉它，顺便确认这确实是我们的格式
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    kind_of(line.trim())?;

    let mut messages = 0usize;
    let mut title: Option<String> = None;
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        messages += 1;
        if title.is_none() && messages <= TITLE_SCAN_LINES {
            if let Some(v) = kind_of(t) {
                if v.get("kind").and_then(Value::as_str) == Some("message")
                    && v.get("role").and_then(Value::as_str) == Some("user")
                {
                    if let Some(c) = v.get("content").and_then(Value::as_str) {
                        if !c.starts_with(HOST_PREFIX) {
                            title = Some(clamp_title(c));
                        }
                    }
                }
            }
        }
    }

    Some(Summary {
        time: display_time(&id, this_year),
        id,
        path,
        title: title.unwrap_or_else(|| "（空对话）".into()),
        messages,
    })
}

/// 一条读出来的消息。
#[derive(Debug, Clone)]
pub struct Loaded {
    pub msg: Message,
    pub is_error: Option<bool>,
}

/// 读出整个对话的消息列表。坏行直接跳过，不让一行毁掉整个文件的可读性。
pub fn load(path: &Path) -> Result<Vec<Loaded>> {
    let file = File::open(path).with_context(|| format!("无法打开 {}", path.display()))?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(v) = kind_of(t) else { continue };
        if v.get("kind").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let is_error = v.get("is_error").and_then(Value::as_bool);
        if let Ok(msg) = serde_json::from_value::<Message>(v) {
            out.push(Loaded { msg, is_error });
        }
    }
    Ok(out)
}

/// 只保留最新的 `keep` 个对话，多出来的删掉。`keep == 0` 表示不限制。
///
/// 返回被删除的文件数。
pub fn prune(dir: &Path, keep: usize) -> usize {
    if keep == 0 {
        return 0;
    }
    let mut n = 0;
    for (_, path) in collect(dir).into_iter().skip(keep) {
        if std::fs::remove_file(&path).is_ok() {
            n += 1;
        }
    }
    n
}

fn clamp_title(s: &str) -> String {
    let first = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let n = first.chars().count();
    if n <= TITLE_MAX_CHARS {
        first.to_string()
    } else {
        format!("{}…", first.chars().take(TITLE_MAX_CHARS).collect::<String>())
    }
}

// ─────────────────────────── 命令（CLI / GUI 共用） ───────────────────────────

/// `/history` 子命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// 列出历史对话
    List,
    /// 打印某个对话全文
    Show(String),
    /// 打印历史目录
    Dir,
    /// 删除某个对话
    Remove(String),
}

/// 这一行是不是 `/history` 命令。
pub fn is_cmd(line: &str) -> bool {
    line == "/history" || line.starts_with("/history ")
}

pub fn parse_cmd(line: &str) -> Result<Cmd> {
    let mut parts = line.split_whitespace();
    let _ = parts.next();
    match parts.next() {
        None | Some("list") => Ok(Cmd::List),
        Some("dir") => Ok(Cmd::Dir),
        Some("show") => match parts.next() {
            Some(id) => Ok(Cmd::Show(id.to_string())),
            None => bail!("用法: /history show <序号|文件名>"),
        },
        Some("rm") | Some("remove") => match parts.next() {
            Some(id) => Ok(Cmd::Remove(id.to_string())),
            None => bail!("用法: /history rm <序号|文件名>"),
        },
        Some(other) => bail!(
            "未知的 /history 子命令: {other}\n\
             用法: /history [list] | /history show <序号|文件名> | \
             /history rm <序号|文件名> | /history dir"
        ),
    }
}

/// 把用户给的「序号」或「文件名主干」解析成实际文件。
///
/// 序号取自 `list()` 的展示顺序（最新的为 1）。两种写法都支持 ——
/// 手输时间戳太麻烦，看到列表后直接写序号最省事。
pub fn resolve(dir: &Path, key: &str) -> Result<Summary> {
    let key = key.trim().trim_end_matches(&format!(".{EXT}"));
    if let Ok(n) = key.parse::<usize>() {
        if n == 0 {
            bail!("序号从 1 开始");
        }
        return list(dir, DEFAULT_LIST_LIMIT)
            .into_iter()
            .nth(n - 1)
            .with_context(|| format!("没有第 {n} 个对话（可用 /history 查看列表）"));
    }
    list(dir, DEFAULT_LIST_LIMIT)
        .into_iter()
        .find(|s| s.id == key || s.id.starts_with(key))
        .with_context(|| format!("找不到对话 {key}（可用 /history 查看列表）"))
}

/// `/history` 的纯文本实现（CLI 直接打印）。
pub fn list_text(dir: &Path, current: Option<&str>) -> String {
    let items = list(dir, DEFAULT_LIST_LIMIT);
    if items.is_empty() {
        return format!("（还没有历史记录）\n  目录: {}", dir.display());
    }
    let mut s = format!("历史记录（{} 个对话，最新在前）  {}", items.len(), dir.display());
    for (i, it) in items.iter().enumerate() {
        let cur = if Some(it.id.as_str()) == current { "  ← 当前" } else { "" };
        s.push_str(&format!(
            "\n  {:>2}. {}  {:>4} 条  {}{}",
            i + 1,
            it.time,
            it.messages,
            it.title,
            cur
        ));
    }
    s.push_str("\n  用法: /history show <序号> | /history rm <序号> | /history dir");
    s
}

/// 把一个对话渲染成纯文本（CLI 的 `/history show`）。
pub fn transcript_text(items: &[Loaded]) -> String {
    use crate::ui::style;
    if items.is_empty() {
        return "（空对话）".into();
    }
    let mut s = String::new();
    for it in items {
        let m = &it.msg;
        match m.role.as_str() {
            "user" => {
                let text = m.content.as_deref().unwrap_or("").trim();
                if text.starts_with(HOST_PREFIX) {
                    s.push_str(&format!("\n{} {}\n", style::dim("宿主:"), style::dim(text)));
                } else {
                    s.push_str(&format!("\n{} {}\n", style::bold("你:"), text));
                }
            }
            "assistant" => {
                if let Some(t) = m.text() {
                    s.push_str(&format!("\n{} {}\n", style::bold("助手:"), t));
                }
                for c in m.tool_calls.as_deref().unwrap_or(&[]) {
                    s.push_str(&format!(
                        "\n  {} {} {}\n",
                        style::dim("→"),
                        style::cyan(&c.function.name),
                        style::dim(&clamp_chars(c.function.arguments.trim(), 160))
                    ));
                }
            }
            "tool" => {
                let (mark, name) = (
                    if it.is_error == Some(true) { "✗" } else { "✓" },
                    m.name.as_deref().unwrap_or("tool"),
                );
                // 工具结果可能很长（写入前已被 max_result_chars 收过一道），
                // 展示时再收一道，翻历史才不用在一条结果里滚屏。
                let body = m.content.as_deref().unwrap_or("").replace('\n', " ⏎ ");
                s.push_str(&format!("  {} {} {}\n", mark, name, style::dim(&clamp_chars(body.trim(), 300))));
            }
            _ => {}
        }
    }
    s
}

fn clamp_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

// ─────────────────────────── 测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FunctionCall, ToolCall};

    /// 测试用的临时目录。**出作用域即删** —— 曾经这里只返回 PathBuf，
    /// 结果每跑一次 `cargo test` 就在 %TEMP% 里留下七个空目录。
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("aox-hist-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            Self(d)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall { name: name.into(), arguments: args.into() },
        }
    }

    /// 文件名主干就是对话 id。
    fn id_of(log: &Log) -> String {
        log.path().file_stem().unwrap().to_string_lossy().to_string()
    }

    /// 写进去什么，读出来就应该是同样的消息、同样的顺序、同样的 is_error。
    /// 这条是整块功能的地基：文件里混了 `kind` / `is_error` 这些非 wire 字段，
    /// 读回 `Message` 时它们必须被忽略而不是让解析失败。
    #[test]
    fn round_trip_messages() {
        let d = Tmp::new("rt");
        let mut log = Log::create(d.path(), Some("deepseek-chat"), Path::new("C:/root"), 0).unwrap();
        let id = id_of(&log);

        log.append(&Message::user("第一行\n第二行 \"引号\" 与中文"), None);
        log.append(&Message::assistant(None, Some("先看看".into()), vec![call("c1", "fs_ls", r#"{"path":"."}"#)]), None);
        log.append(&Message::tool_result("c1", "fs_ls", "列出 3 项".into()), Some(false));
        log.append(&Message::assistant(None, None, vec![call("c2", "fs_read", r#"{"path":"nope"}"#)]), None);
        log.append(&Message::tool_result("c2", "fs_read", "文件不存在".into()), Some(true));
        log.append(&Message::assistant(Some("完成了。".into()), None, vec![]), None);
        drop(log);

        let items = load(&d.path().join(format!("{id}.{EXT}"))).unwrap();
        assert_eq!(items.len(), 6, "system 不入档，6 条消息应全部读到");
        assert_eq!(items[0].msg.role, "user");
        assert_eq!(items[0].msg.content.as_deref(), Some("第一行\n第二行 \"引号\" 与中文"));
        assert_eq!(items[0].is_error, None);
        assert_eq!(items[1].msg.tool_calls.as_ref().unwrap()[0].function.name, "fs_ls");
        // reasoning_content 必须挺过往返：思考模型要求它原样回传
        assert_eq!(items[1].msg.reasoning_content.as_deref(), Some("先看看"));
        // 工具结果的成功/失败标记挂在消息外层，不在 Message 的 wire 字段里
        assert_eq!(items[2].is_error, Some(false));
        assert_eq!(items[2].msg.name.as_deref(), Some("fs_ls"));
        assert_eq!(items[3].msg.tool_calls.as_ref().unwrap()[0].function.name, "fs_read");
        assert_eq!(items[3].is_error, None, "assistant 消息没有工具结果标记");
        assert_eq!(items[4].is_error, Some(true));
        assert_eq!(items[5].msg.text(), Some("完成了。"));

        // 列表：标题取首条用户消息，条数不含 meta
        let list = super::list(d.path(), 10);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].messages, 6);
        assert_eq!(list[0].title, "第一行");
        assert!(list[0].time.contains(':'), "显示时间形如 10-02 15:39: {}", list[0].time);
    }

    /// 系统提示不入档：它每轮都在变，又长又没用。
    #[test]
    fn system_prompt_is_skipped() {
        let d = Tmp::new("sys");
        let mut log = Log::create(d.path(), None, Path::new("C:/root"), 0).unwrap();
        log.append(&Message::system("你是 AOX…"), None);
        log.append(&Message::user("hi"), None);
        drop(log);
        let items = load(&d.path().join(format!("{}.{EXT}", super::list(d.path(), 1)[0].id))).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].msg.role, "user");
    }

    /// 中断回滚：文件必须跟着内存一起回到任务前的状态，不留半截 tool_calls。
    #[test]
    fn rewrite_drops_rolled_back_turn() {
        let d = Tmp::new("rw");
        let mut log = Log::create(d.path(), None, Path::new("C:/root"), 0).unwrap();
        let path = log.path().to_path_buf();
        let mut msgs = vec![Message::system("sys")];
        log.append(&msgs[0], None);
        msgs.push(Message::user("保留我"));
        log.append(&msgs[1], None);
        // 这一轮被中断后回滚
        let half = vec![Message::user("别留我"), Message::assistant(None, None, vec![call("c9", "x", "{}")])];
        for m in &half {
            log.append(m, None);
        }
        // system 不入档 → 文件里是 1 条 user + 2 条被回滚的
        assert_eq!(load(&path).unwrap().len(), 3);

        msgs.truncate(2);
        log.rewrite(&msgs);
        drop(log);

        let items = load(&path).unwrap();
        assert_eq!(items.len(), 1, "只剩「保留我」一条");
        assert_eq!(items[0].msg.content.as_deref(), Some("保留我"));
        // meta 行必须还在，否则这个文件就认不出自己是谁了
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 2, "meta + 1 条消息");
        assert!(raw.contains("\"kind\":\"meta\""));
    }

    /// 标题不该用宿主注入的提示语。
    #[test]
    fn host_prompt_is_not_a_title() {
        let d = Tmp::new("host");
        let mut log = Log::create(d.path(), None, Path::new("C:/root"), 0).unwrap();
        log.append(&Message::user("（宿主提示）你上一条回复是空的。"), None);
        log.append(&Message::assistant(Some("好".into()), None, vec![]), None);
        log.append(&Message::user("修一下登录 bug"), None);
        drop(log);
        let items = super::list(d.path(), 5);
        assert_eq!(items[0].title, "修一下登录 bug");
    }

    /// 保留策略：只留最新的 keep 个。
    #[test]
    fn prune_keeps_newest() {
        let d = Tmp::new("prune");
        for _ in 0..5 {
            let mut log = Log::create(d.path(), None, Path::new("C:/root"), 0).unwrap();
            log.append(&Message::user("x"), None);
            drop(log);
            // 文件名精度到秒，靠 sleep 保证时间戳不同
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
        assert_eq!(collect(d.path()).len(), 5);
        let removed = prune(d.path(), 3);
        assert_eq!(removed, 2);
        assert_eq!(collect(d.path()).len(), 3);
        // keep = 0 表示不限制
        assert_eq!(prune(d.path(), 0), 0);
        assert_eq!(collect(d.path()).len(), 3);
    }

    /// 坏行不该毁掉整个文件。
    #[test]
    fn corrupt_line_is_skipped() {
        let d = Tmp::new("bad");
        let mut log = Log::create(d.path(), None, Path::new("C:/root"), 0).unwrap();
        let path = log.path().to_path_buf();
        log.append(&Message::user("好行"), None);
        drop(log);
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{这不是 JSON\n{\"kind\":\"unknown\"}\n");
        std::fs::write(&path, raw).unwrap();
        let items = load(&path).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].msg.content.as_deref(), Some("好行"));
    }

    /// 序号 / 文件名主干都能定位到同一个对话。
    #[test]
    fn resolve_by_index_and_id() {
        let d = Tmp::new("resolve");
        let mut log = Log::create(d.path(), None, Path::new("C:/root"), 0).unwrap();
        log.append(&Message::user("唯一一个"), None);
        let id = id_of(&log);
        drop(log);
        assert_eq!(resolve(d.path(), "1").unwrap().id, id);
        assert_eq!(resolve(d.path(), &id).unwrap().id, id);
        assert_eq!(resolve(d.path(), &id[..13]).unwrap().id, id, "支持只写时间戳前缀");
        assert!(resolve(d.path(), "9").is_err());
        assert!(resolve(d.path(), "0").is_err());
    }
}

