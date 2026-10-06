//! JSONL 后端：一次对话一个文件，落在 `<app_root>/history/`，文件名就是它自己的时间戳：
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
//! {"act":{"at":1.7,"end":2.0,"kind":"tool","title":"fs_ls"},"kind":"act"}
//! {"kind":"usage","round":1,"at":1759390999,"calls":3,"prompt":384,"total":1152}
//! ```
//!
//! 除了消息，同一个文件里还会追加**活动流**记录（`kind:"act"`）与 **token 用量**记录
//! （`kind:"usage"`）：时间轴上的每一枚色块是一行，一轮任务的 token 账也是一行。
//!
//! 三者共用一个文件是**刻意**的：它们本来描述同一件事（这一轮对话是怎么跑的、花了多少），
//! 而且「删掉一段对话」必须连它的时间轴和账本一起删掉 —— 共用一个文件，这件事是自动的，
//! 不需要任何额外的清理代码。
//!
//! 活动流的负载嵌在 `act` 键下，而不是平铺：`Act` 自己也有一个 `kind` 字段
//! （输入 / 模型 / 工具 …），和行类型同名同层会互相覆盖，读回来就分不清是哪种行。
//!
//! 为什么是 JSONL 而不是一个 JSON 数组：
//!   - 追加是 O(1)。长任务可能有几百条消息，每轮重写整个文件既慢又容易写坏；
//!   - 进程被强杀时，已落地的行仍然完整，可以正常读出来；
//!   - 单行损坏只影响那一行，不会让整个文件解析失败。
//!
//! 消息体直接复用 `Message`（OpenAI wire 格式），所以这个文件既是给人看的记录，
//! 也天然是一份可回放的消息流 —— 将来要加「恢复对话」不需要改格式。
//!
//! **本文件是阶段 3 从 `src/history.rs` 搬过来的**：逻辑逐函数原样搬运，
//! 只把「自由函数 + `dir` / `path` 参数」收成结构体的方法（`JsonlStore` / `JsonlSession`）。
//! 文件格式、排序规则、回滚语义一行没动。

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::attach;
use crate::history::{
    unix_now, Loaded, Meta, Summary, UsageRecord, DEFAULT_LIST_LIMIT, HOST_PREFIX,
};
use crate::message::Message;
use crate::store::{Session, Store};

/// 历史目录名（相对应用根）。
const DIR_NAME: &str = "history";
const EXT: &str = "jsonl";

/// 一个对话最多留多少个字符当标题。
const TITLE_MAX_CHARS: usize = 46;
/// 标题只在前若干行里找：首条用户消息总在第 2 行，多给几行余量即可，
/// 免得为了一个标题把整条 tool 结果也解析一遍。
const TITLE_SCAN_LINES: usize = 6;

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

/// 拼出「10-02 15:39」。往年的补上年份，否则「10-02」会让人误以为是今年。
fn display_parts(y: u16, mo: u16, d: u16, hh: u16, mm: u16, this_year: u16) -> String {
    if y == this_year {
        format!("{mo:02}-{d:02} {hh:02}:{mm:02}")
    } else {
        format!("{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}")
    }
}

/// 由文件名主干 `YYYYMMDD-HHMMSS` 解析出显示时间（= 这段对话**开始**的时间）。
///
/// 时间直接从文件名解析，不做任何时区换算 —— 文件名本来就是本地时间，
/// 这比存一个 UTC 时间戳再想办法转回来简单得多，也不会算错。
/// 现在只在拿不到文件 mtime 时兜底（见 `display_mtime`）。
fn display_time(id: &str, this_year: u16) -> String {
    let b = id.as_bytes();
    if b.len() < 15 || b[8] != b'-' {
        return id.to_string();
    }
    // 逐段解析而不是直接切字符串：非 ASCII 的 id 切出来的可能不是字符边界
    let num = |a: usize, z: usize| {
        let s = id.get(a..z)?;
        if !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()) {
            s.parse::<u16>().ok()
        } else {
            None
        }
    };
    match (num(0, 4), num(4, 6), num(6, 8), num(9, 11), num(11, 13)) {
        (Some(y), Some(mo), Some(d), Some(hh), Some(mm)) => display_parts(y, mo, d, hh, mm, this_year),
        _ => id.to_string(),
    }
}

/// 系统时间 → 本地时间。
///
/// Windows 交给 `FileTimeToLocalFileTime` 去算时区与夏令时；自己拿
/// 「当前本地时间 − 当前 UTC」推偏移，会在夏令时切换的那一小时里错一小时。
/// 非 Windows 平台没有等价物，退回 UTC（显示上差一个时区，不值得为它引依赖）。
#[cfg(windows)]
fn local_from_system(t: SystemTime) -> Option<Local> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    use windows_sys::Win32::System::Time::FileTimeToSystemTime;

    // Unix 秒 → FILETIME：单位 100ns，起点是 1601-01-01
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let ticks = (secs + 11_644_473_600) * 10_000_000;
    let utc = FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut local = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        if FileTimeToLocalFileTime(&utc, &mut local) == 0 {
            return None;
        }
        if FileTimeToSystemTime(&local, &mut st) == 0 {
            return None;
        }
    }
    Some(Local {
        year: st.wYear,
        month: st.wMonth,
        day: st.wDay,
        hour: st.wHour,
        minute: st.wMinute,
        second: st.wSecond,
    })
}

#[cfg(not(windows))]
fn local_from_system(t: SystemTime) -> Option<Local> {
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let (year, month, day, hour, minute, second) = civil_from_unix(secs);
    Some(Local { year, month, day, hour, minute, second })
}

/// 文件 mtime → 「最后一次对话时间」显示串。
///
/// 列表按最后一次对话时间排序，显示的就必须是同一个时间 —— 否则一段三天前的
/// 对话被顶到最上面、旁边仍写着三天前，看着就像排错了。
fn display_mtime(t: SystemTime, this_year: u16) -> Option<String> {
    let l = local_from_system(t)?;
    Some(display_parts(l.year, l.month, l.day, l.hour, l.minute, this_year))
}

// ─────────────────────────── 行格式 ───────────────────────────

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
        // 附件的**载荷（base64）不入档** —— 一张截图就是几 MB，而这是逐条追加的对话
        // 记录。留下的正文与附件清单照旧，所以「当时发了什么」仍然查得出来。
        // 完整理由见 `attach::strip_payloads`。
        if let Some(slim) = m.get("content").and_then(attach::strip_payloads) {
            m.insert("content".into(), slim);
        }
        m.insert("kind".into(), json!("message"));
        if let Some(e) = is_error {
            m.insert("is_error".into(), json!(e));
        }
    }
    v
}

/// 活动流一行。负载嵌在 `act` 键下 —— `Act` 自己有一个 `kind` 字段
/// （输入 / 模型 / 工具 …），与行类型同名同层会互相覆盖。
fn act_line(act: &Value) -> Value {
    json!({ "kind": "act", "act": act })
}

fn usage_line(rec: &UsageRecord) -> Value {
    let mut v = rec.to_value();
    v["kind"] = json!("usage");
    v
}

/// 工作区一行。字段由 [`crate::workplace::Record::to_value`] 决定 —— 那边负责
/// 「没有的字段不写」（`full` 档就没有根），这里只补上行类型。
fn workplace_line(rec: &crate::workplace::Record) -> Value {
    let mut v = rec.to_value();
    v["kind"] = json!("workplace");
    v
}

/// 我们写出去的行类型。别的行（误放进 `history/` 的其它文件、被手改坏的行）
/// 一律不认 —— 这也是「这个文件是我们的」最便宜的判据。
const ROW_KINDS: [&str; 5] = ["meta", "message", "act", "usage", "workplace"];

/// 解析一行，认得出来就返回它的对象。
fn parse_row(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    let k = v.get("kind")?.as_str()?;
    ROW_KINDS.contains(&k).then_some(v)
}

fn is_kind(v: &Value, kind: &str) -> bool {
    v.get("kind").and_then(Value::as_str) == Some(kind)
}

fn write_line_to(file: &mut File, v: &Value) -> Result<()> {
    let mut line = serde_json::to_string(v)?;
    line.push('\n');
    // 一次写入，而不是「先写正文、再写换行」：同一行要么整条落地要么整条不落，
    // 否则中途出错会留下半行残记录 —— 读的时候被当成坏行丢掉，等于这条白写了。
    file.write_all(line.as_bytes())?;
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
        if !file_of(dir, &id).exists() {
            return id;
        }
    }
    format!("{stamp}-{:04x}", (unix_now() & 0xffff) as u16)
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

/// 某个对话 id 对应的文件路径。
///
/// 「id ↔ 文件名」这条映射被四个地方用到（新建、查重、扫目录、按 id 切回），
/// 所以只留这一个写法 —— 它俩对不上的表现是「找不到刚存下的对话」，
/// 而不是编译错误。**这是 jsonl 后端的内部细节，不对外暴露。**
fn file_of(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.{EXT}"))
}

fn dir_of(root: &Path) -> PathBuf {
    root.join(DIR_NAME)
}

// ─────────────────────────── 后端 ───────────────────────────

/// JSONL 后端：`<app_root>/history/*.jsonl`。
pub struct JsonlStore {
    /// 应用根目录。meta 行里的 `root` 字段记的是它。
    root: PathBuf,
    /// 历史目录（`root/history`）。算一次存下来，避免每次调用都 join。
    dir: PathBuf,
    /// 新建对话后裁剪到多少个（0 = 不限制）。
    keep: usize,
}

impl JsonlStore {
    pub fn new(root: &Path, keep: usize) -> Self {
        let root = root.to_path_buf();
        let dir = dir_of(&root);
        Self { root, dir, keep }
    }
}

impl Store for JsonlStore {
    fn backend(&self) -> &'static str {
        "jsonl"
    }

    fn location(&self) -> &Path {
        &self.dir
    }

    /// `keep` 是构造时定下的（来自 `config.toml [history] keep`）。
    fn create(&self, model: Option<&str>) -> Result<Box<dyn Session>> {
        JsonlSession::create(&self.dir, model, &self.root, self.keep).map(|s| Box::new(s) as _)
    }

    fn open(&self, id: &str) -> Result<Box<dyn Session>> {
        JsonlSession::open(&file_of(&self.dir, id)).map(|s| Box::new(s) as _)
    }

    fn list(&self, limit: usize) -> Vec<Summary> {
        list_in(&self.dir, limit)
    }

    fn resolve(&self, key: &str) -> Result<Summary> {
        resolve_in(&self.dir, key)
    }

    fn load(&self, id: &str) -> Result<Vec<Loaded>> {
        load(&file_of(&self.dir, id))
    }

    fn load_acts(&self, id: &str) -> Vec<Value> {
        load_acts(&file_of(&self.dir, id))
    }

    fn load_usage(&self, id: &str) -> Vec<UsageRecord> {
        load_usage(&file_of(&self.dir, id))
    }

    fn load_workplace(&self, id: &str) -> Option<crate::workplace::Record> {
        // 「最后一条」= 这段对话当前的工作区。
        load_workplaces(&file_of(&self.dir, id)).pop()
    }

    fn remove(&self, key: &str) -> Result<Summary> {
        // 删除原先散在 `main.rs` 里（直接 `std::fs::remove_file`）——
        // 收进后端，否则换个后端（比如一个库装所有对话）就没有「删除」这个语义。
        let s = resolve_in(&self.dir, key)?;
        std::fs::remove_file(&s.path)
            .with_context(|| format!("无法删除 {}", s.path.display()))?;
        Ok(s)
    }

    fn prune(&self, keep: usize) -> usize {
        prune_in(&self.dir, keep)
    }
}

// ─────────────────────────── 写入 ───────────────────────────

/// 一段正在写的对话（原 `history::Log`）。
pub struct JsonlSession {
    path: PathBuf,
    file: File,
    meta: Meta,
}

impl JsonlSession {
    /// 新建一个对话文件并写入 meta 行。
    ///
    /// `keep`：创建后把目录里超出的旧对话裁掉（0 = 不限制）。放在这里而不是
    /// 启动时，是因为「保留最近 N 个」本来就只在产生新对话时才有意义。
    fn create(dir: &Path, model: Option<&str>, root: &Path, keep: usize) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("无法创建历史目录 {}", dir.display()))?;
        let meta = Meta {
            id: new_id(dir),
            created: unix_now(),
            model: model.map(str::to_string),
            root: Some(root.display().to_string()),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        };
        let path = file_of(dir, &meta.id);
        let file = OpenOptions::new().create(true).write(true).append(true).open(&path)?;
        let mut log = Self { path, file, meta };
        let head = meta_line(&log.meta);
        write_line_to(&mut log.file, &head)?;
        if keep > 0 {
            prune_in(dir, keep);
        }
        Ok(log)
    }

    /// 接上一段已经存在的对话（继续往里写）。
    ///
    /// 与 `create` 的区别：不新建文件、不生成新 id、也不做裁剪。meta 行是从
    /// 文件里读回来的 —— 关键是要留住**原来那个 id**，否则「继续这段对话」
    /// 会写出第二个文件，历史里就会出现两条并行的记录。
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("无法打开 {}", path.display()))?;
        let first = BufReader::new(file).lines().next();
        let meta = first
            .and_then(|l| l.ok())
            .and_then(|l| serde_json::from_str::<Meta>(l.trim()).ok())
            // 首行不是 meta（手改过 / 早期版本写的文件）：退回用文件名当 id，
            // 至少还能继续写，而不是整段对话都打不开
            .unwrap_or_else(|| Meta {
                id: path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string(),
                created: 0,
                model: None,
                root: None,
                version: None,
            });
        let file = OpenOptions::new().write(true).append(true).open(path)?;
        Ok(Self { path: path.to_path_buf(), file, meta })
    }
}

impl Session for JsonlSession {
    fn id(&self) -> &str {
        &self.meta.id
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// 追加一条消息。写入失败不返回错误 —— 历史记录不该让对话中断。
    fn append(&mut self, msg: &Message, is_error: Option<bool>) {
        if msg.role == "system" {
            return; // 系统提示每轮都在变，又长又没用，不入档
        }
        let line = message_line(msg, is_error);
        let _ = write_line_to(&mut self.file, &line);
    }

    /// 追加一条活动流记录（界面上时间轴里的一枚色块）。
    ///
    /// 与消息一样，写失败不返回错误：时间轴是「过程的样子」，丢了顶多少一条记录，
    /// 不该让正在跑的对话出问题。
    fn append_act(&mut self, act: &Value) {
        let _ = write_line_to(&mut self.file, &act_line(act));
    }

    /// 追加一轮的 token 账。
    ///
    /// 与消息、活动流一样，写失败不返回错误：一行统计丢了不该让正在跑的对话出问题。
    fn append_usage(&mut self, rec: &UsageRecord) {
        let _ = write_line_to(&mut self.file, &usage_line(rec));
    }

    /// 追加一条「本对话的工作区」。同上：写失败不出声。
    ///
    /// 写在对话文件里（而不是另开一个旁路文件），是因为它**属于这段对话**：
    /// 对话被删、被裁剪、被换后端时，这条记录自然跟着走，不会留下孤儿。
    fn append_workplace(&mut self, rec: &crate::workplace::Record) {
        let _ = write_line_to(&mut self.file, &workplace_line(rec));
    }

    /// 用给定的消息列表重写整个文件。
    ///
    /// 用于「中断回滚」：内存里的消息被丢弃了，文件必须跟着回到同一状态，
    /// 否则会留下一段「有 tool_calls 但没有结果」的半截记录
    /// （将来要恢复这段对话时，那种记录会直接让请求被拒）。
    ///
    /// 活动流（`kind:"act"`）、token 用量（`kind:"usage"`）与工作区（`kind:"workplace"`）
    /// 不属于消息流，所以**要先捞出来再原样补回**：回滚丢的是消息，没道理顺手把时间轴上
    /// 「已经发生过的过程」、已经记下的账、以及「这段对话在哪块地里干活」一起抹掉。
    ///
    /// 实现上**不能**对 append 句柄直接 `set_len`：Windows 上 append 打开的句柄
    /// 没有截断权限，会返回 `os error 5（拒绝访问）`；而且 `O_APPEND` 会让 `seek`
    /// 形同虚设，继续写还是落在旧内容后面（这两条都实测过）。
    /// 所以另开一个截断句柄重写全文，再把 append 句柄换回来。
    fn rewrite(&mut self, msgs: &[Message]) {
        let acts = load_acts(&self.path);
        let usages = load_usage(&self.path);
        let workplaces = load_workplaces(&self.path);
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
        for a in &acts {
            if write_line_to(&mut f, &act_line(a)).is_err() {
                return;
            }
        }
        for u in &usages {
            if write_line_to(&mut f, &usage_line(u)).is_err() {
                return;
            }
        }
        for rec in &workplaces {
            if write_line_to(&mut f, &workplace_line(rec)).is_err() {
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

// ─────────────────────────── 读取 ───────────────────────────

/// 目录里的一个历史文件。
struct Entry {
    id: String,
    path: PathBuf,
    /// 最后一次写入的时间（文件 mtime），也就是「最后一次对话时间」
    mtime: Option<SystemTime>,
}

/// 扫目录，**按最后一次对话时间从新到旧**排列。
///
/// 用 mtime 而不是文件名里的时间戳：文件名记的是「这段对话什么时候开始」，
/// 而列表要回答的是「最近在跟哪段对话说话」。接着聊一段三天前的对话，
/// 它就该浮到最上面 —— 这正是用户找「我刚刚在聊什么」时的预期顺序。
fn collect(dir: &Path) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<Entry> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let id = name.strip_suffix(&format!(".{EXT}"))?.to_string();
            Some(Entry {
                path: file_of(dir, &id),
                mtime: e.metadata().ok().and_then(|m| m.modified().ok()),
                id,
            })
        })
        .collect();
    // mtime 相同时（同一秒内建了两个文件）退回文件名：文件名以时间戳开头，
    // 字典序即时间序。这条兜底让顺序在任何情况下都是确定的。
    files.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.id.cmp(&a.id)));
    files
}

/// 列出历史对话，**最近聊过的在前**。
///
/// 只读前几行就能拿到标题（首条用户消息永远是第 2 行），条数靠数行数得出 ——
/// 不解析全文，几百个文件也只是顺序读磁盘。
fn list_in(dir: &Path, limit: usize) -> Vec<Summary> {
    let this_year = local_now().year;
    collect(dir)
        .into_iter()
        .take(limit)
        .filter_map(|e| read_summary(e, this_year))
        .collect()
}

fn read_summary(e: Entry, this_year: u16) -> Option<Summary> {
    let Entry { id, path, mtime } = e;
    let file = File::open(&path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();

    // 第 1 行是 meta（含 id / 模型 / 版本）：读掉它，顺便确认这确实是我们的格式
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    if !parse_row(line.trim()).is_some_and(|v| is_kind(&v, "meta")) {
        return None;
    }

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
        let Some(v) = parse_row(t) else { continue };
        // 活动流（`kind:"act"`）与用量（`kind:"usage"`）既不进「几条消息」，
        // 也不当标题 —— 否则侧边栏的条数会虚高，而且第一轮的一堆色块会把
        // `TITLE_SCAN_LINES` 的名额吃光，标题就取不到了。
        if !is_kind(&v, "message") {
            continue;
        }
        messages += 1;
        if title.is_none() && messages <= TITLE_SCAN_LINES {
            if v.get("role").and_then(Value::as_str) == Some("user") {
                if let Some(c) = v.get("content").and_then(Value::as_str) {
                    if !c.starts_with(HOST_PREFIX) {
                        title = Some(clamp_title(c));
                    }
                }
            }
        }
    }

    Some(Summary {
        // 显示「最后一次对话时间」（列表排序用的就是它）；mtime 拿不到时退回
        // 文件名里的开始时间，宁可显示得旧一点，也不要留一格空白。
        time: mtime
            .and_then(|t| display_mtime(t, this_year))
            .unwrap_or_else(|| display_time(&id, this_year)),
        id,
        path,
        title: title.unwrap_or_else(|| "（空对话）".into()),
        messages,
    })
}

/// 读出整个对话的消息列表。坏行直接跳过，不让一行毁掉整个文件的可读性。
fn load(path: &Path) -> Result<Vec<Loaded>> {
    let file = File::open(path).with_context(|| format!("无法打开 {}", path.display()))?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(v) = parse_row(t) else { continue };
        if !is_kind(&v, "message") {
            continue; // 活动流记录不是消息，回放对话时被跳过
        }
        let is_error = v.get("is_error").and_then(Value::as_bool);
        if let Ok(msg) = serde_json::from_value::<Message>(v) {
            out.push(Loaded { msg, is_error });
        }
    }
    Ok(out)
}

/// 读出一个文件里所有的**活动流**记录（按写入顺序），负载原样返回。
///
/// 解析成什么结构由调用方决定：这一段是界面上的显示数据，存储层不该知道
/// 「色块」「轮次」这些概念，只管把行搬出来。坏行与未知行跳过。
fn load_acts(path: &Path) -> Vec<Value> {
    let Ok(file) = File::open(path) else { return Vec::new() };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(v) = parse_row(t) else { continue };
        if !is_kind(&v, "act") {
            continue;
        }
        if let Some(payload) = v.get("act") {
            out.push(payload.clone());
        }
    }
    out
}

/// 读出一个文件里所有的 **token 用量**记录（按写入顺序，即轮次顺序）。
///
/// 打开一段旧对话时靠它把账本接回来 —— 否则重启一次，之前那几轮的用量就凭空消失了。
/// 坏行跳过，`kind:"usage"` 之外的行一概不认。
fn load_usage(path: &Path) -> Vec<UsageRecord> {
    let Ok(file) = File::open(path) else { return Vec::new() };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(v) = parse_row(t) else { continue };
        if !is_kind(&v, "usage") {
            continue;
        }
        out.push(UsageRecord::from_value(&v));
    }
    out
}

/// 读出一个文件里所有的**工作区**记录（按写入顺序）—— 调用方取最后一条 = 当前那块地。
///
/// 与 `load_usage` 一样：坏行跳过，`kind:"workplace"` 之外的行一概不认；
/// 单行内部认不出（缺 `mode`）也跳过。
fn load_workplaces(path: &Path) -> Vec<crate::workplace::Record> {
    let Ok(file) = File::open(path) else { return Vec::new() };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(v) = parse_row(t) else { continue };
        if !is_kind(&v, "workplace") {
            continue;
        }
        let Some(rec) = crate::workplace::Record::from_value(&v) else { continue };
        out.push(rec);
    }
    out
}

/// 只保留最新的 `keep` 个对话，多出来的删掉。`keep == 0` 表示不限制。
/// 「最新」= 最后一次对话时间，所以裁剪淘汰的是**最久没聊过**的那些 ——
/// 刚接着聊过的旧对话会被保住，哪怕它是几个月前开的。
/// 返回被删除的文件数。
fn prune_in(dir: &Path, keep: usize) -> usize {
    if keep == 0 {
        return 0;
    }
    let mut n = 0;
    for e in collect(dir).into_iter().skip(keep) {
        if std::fs::remove_file(&e.path).is_ok() {
            n += 1;
        }
    }
    n
}

/// 把用户给的「序号」或「文件名主干」解析成实际文件。
///
/// 序号取自 `list()` 的展示顺序（最新的为 1）。两种写法都支持 ——
/// 手输时间戳太麻烦，看到列表后直接写序号最省事。
///
/// 「纯数字」这个歧义要单独判：id 以 8 位日期开头，所以 `20261003` 既像序号、
/// 也像一段 id 前缀。判据是长度 —— 序号不可能有 8 位（列表上限 60，就算把上限
/// 放开也到不了 8 位），所以**8 位及以上的纯数字一律当 id 前缀**。
/// 不这么分的话，`/history rm 20261003` 会变成「没有第 20261003 个对话」。
fn resolve_in(dir: &Path, key: &str) -> Result<Summary> {
    let key = key.trim().trim_end_matches(&format!(".{EXT}"));
    // 空键要挡在这里：`id.starts_with("")` 恒为真，放过去就会「解析成功」成列表第一条，
    // 那是随机的。宁可明确报错。
    if key.is_empty() {
        bail!("请给出对话序号或文件名（可用 /history 查看列表）");
    }
    let is_index = key.len() < 8 && key.bytes().all(|c| c.is_ascii_digit());
    let items = list_in(dir, DEFAULT_LIST_LIMIT);
    if is_index {
        let n: usize = key
            .parse()
            .with_context(|| format!("序号不合法: {key}"))?;
        if n == 0 {
            bail!("序号从 1 开始");
        }
        return items
            .into_iter()
            .nth(n - 1)
            .with_context(|| format!("没有第 {n} 个对话（可用 /history 查看列表）"));
    }
    items
        .into_iter()
        .find(|s| s.id == key || s.id.starts_with(key))
        .with_context(|| format!("找不到对话 {key}（可用 /history 查看列表）"))
}

// ─────────────────────────── 测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FunctionCall, ToolCall};

    /// 测试用的临时目录。**出作用域即删** —— 曾经这里只返回 PathBuf，
    /// 结果每跑一次 `cargo test` 就在 %TEMP% 里留下七个空目录。
    ///
    /// 它扮演的是**应用根**：历史落在 `<tmp>/history/`，与真实布局一致
    /// （这正是「后端自己管布局」的意义：测试也不必知道文件名长什么样）。
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("aox-store-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            Self(d)
        }

        /// 一个「不裁剪、不关心模型名」的后端，测试里最常用。
        fn store(&self) -> JsonlStore {
            JsonlStore::new(&self.0, 0)
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

    /// 带附件的消息落盘时**载荷（base64）必须被摘掉**，但正文与附件清单要留着。
    ///
    /// 这条是「对话文件不被一张截图撑胖几 MB」的唯一保险 —— 而它很容易在将来某次
    /// 「统一走 Message 序列化」的重构里被悄悄取消，所以钉在**文件的实际内容**上，
    /// 不是钉在内存里的消息上。
    #[test]
    fn attachment_payloads_are_not_persisted() {
        let d = Tmp::new("attach-slim");
        let s = d.store();
        let mut log = s.create(Some("deepseek-flash")).unwrap();
        let id = log.id().to_string();

        let png = d.0.join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();
        let a = crate::attach::Attachment::probe(&png).unwrap();
        log.append(&Message::user_with_files("看这张", &[a]).unwrap(), None);
        drop(log);

        // 直接看文件：一个字都不该有 base64。
        // 目录问后端要（`location()`），不自己拼 `<root>/history` —— 那正是本测试
        // 要顺带钉住的一点：换后端时这里跟着变。文件名同样不拼，扫目录取唯一那份。
        let file = std::fs::read_dir(s.location())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .expect("对话文件应当已经写出来");
        let raw = std::fs::read_to_string(file).unwrap();
        assert!(!raw.contains("base64"), "载荷不该进对话文件：{}", &raw[..raw.len().min(400)]);
        assert!(raw.contains("shot.png"), "附件清单要留着（历史里得看得出带了什么）");
        assert!(raw.contains("看这张"), "正文当然要留着");

        // 读回来仍然是一条**合法**的用户消息（图片块已整块丢掉，不留假 URL）
        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].msg.role, "user");
        assert!(items[0].msg.flat_text().contains("看这张"), "{}", items[0].msg.flat_text());
        let names = crate::attach::names_from_content(items[0].msg.content.as_ref().unwrap());
        assert_eq!(names, vec!["shot.png".to_string()], "重开旧对话时要能把胶囊画回来");
    }

    /// 写进去什么，读出来就应该是同样的消息、同样的顺序、同样的 is_error。
    /// 这条是整块功能的地基：文件里混了 `kind` / `is_error` 这些非 wire 字段，
    /// 读回 `Message` 时它们必须被忽略而不是让解析失败。
    #[test]
    fn round_trip_messages() {
        let d = Tmp::new("rt");
        let s = d.store();
        let mut log = s.create(Some("deepseek-chat")).unwrap();
        let id = log.id().to_string();

        log.append(&Message::user("第一行\n第二行 \"引号\" 与中文"), None);
        log.append(&Message::assistant(None, Some("先看看".into()), vec![call("c1", "fs_ls", r#"{"path":"."}"#)]), None);
        log.append(&Message::tool_result("c1", "fs_ls", "列出 3 项".into()), Some(false));
        log.append(&Message::assistant(None, None, vec![call("c2", "fs_read", r#"{"path":"nope"}"#)]), None);
        log.append(&Message::tool_result("c2", "fs_read", "文件不存在".into()), Some(true));
        log.append(&Message::assistant(Some("完成了。".into()), None, vec![]), None);
        drop(log);

        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 6, "system 不入档，6 条消息应全部读到");
        assert_eq!(items[0].msg.role, "user");
        assert_eq!(items[0].msg.content.as_ref().and_then(|v| v.as_str()), Some("第一行\n第二行 \"引号\" 与中文"));
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
        let list = s.list(10);
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
        let s = d.store();
        let mut log = s.create(None).unwrap();
        log.append(&Message::system("你是 AOX…"), None);
        log.append(&Message::user("hi"), None);
        drop(log);
        let id = s.list(1)[0].id.clone();
        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].msg.role, "user");
    }

    /// 中断回滚：文件必须跟着内存一起回到任务前的状态，不留半截 tool_calls。
    #[test]
    fn rewrite_drops_rolled_back_turn() {
        let d = Tmp::new("rw");
        let s = d.store();
        let mut log = s.create(None).unwrap();
        let id = log.id().to_string();
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
        assert_eq!(s.load(&id).unwrap().len(), 3);

        msgs.truncate(2);
        log.rewrite(&msgs);
        drop(log);

        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 1, "只剩「保留我」一条");
        assert_eq!(items[0].msg.content.as_ref().and_then(|v| v.as_str()), Some("保留我"));
        // meta 行必须还在，否则这个文件就认不出自己是谁了
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 2, "meta + 1 条消息");
        assert!(raw.contains("\"kind\":\"meta\""));
    }

    /// 标题不该用宿主注入的提示语。
    #[test]
    fn host_prompt_is_not_a_title() {
        let d = Tmp::new("host");
        let s = d.store();
        let mut log = s.create(None).unwrap();
        log.append(&Message::user("（宿主提示）你上一条回复是空的。"), None);
        log.append(&Message::assistant(Some("好".into()), None, vec![]), None);
        log.append(&Message::user("修一下登录 bug"), None);
        drop(log);
        let items = s.list(5);
        assert_eq!(items[0].title, "修一下登录 bug");
    }

    /// 保留策略：只留最新的 keep 个。
    #[test]
    fn prune_keeps_newest() {
        let d = Tmp::new("prune");
        let s = d.store();
        for _ in 0..5 {
            let mut log = s.create(None).unwrap();
            log.append(&Message::user("x"), None);
            drop(log);
            // 文件名精度到秒，靠 sleep 保证时间戳不同
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
        assert_eq!(collect(&s.dir).len(), 5);
        let removed = s.prune(3);
        assert_eq!(removed, 2);
        assert_eq!(collect(&s.dir).len(), 3);
        // keep = 0 表示不限制
        assert_eq!(s.prune(0), 0);
        assert_eq!(collect(&s.dir).len(), 3);
    }

    /// 坏行不该毁掉整个文件。
    #[test]
    fn corrupt_line_is_skipped() {
        let d = Tmp::new("bad");
        let s = d.store();
        let mut log = s.create(None).unwrap();
        let id = log.id().to_string();
        let path = log.path().to_path_buf();
        log.append(&Message::user("好行"), None);
        drop(log);
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{这不是 JSON\n{\"kind\":\"unknown\"}\n");
        std::fs::write(&path, raw).unwrap();
        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].msg.content.as_ref().and_then(|v| v.as_str()), Some("好行"));
    }

    /// 活动流与消息共处一个文件，且互不干扰：
    /// - `load` 只吐消息，`load_acts` 只吐活动流；
    /// - 列表的「条数」只数消息（色块不该让侧边栏的条数虚高）；
    /// - **中断回滚不牵连活动流**：回滚丢的是消息，不是「已经发生过的过程」。
    #[test]
    fn acts_live_alongside_messages() {
        let d = Tmp::new("act");
        let s = d.store();
        let mut log = s.create(None).unwrap();
        let id = log.id().to_string();
        log.append(&Message::user("第一句"), None);
        log.append_act(&json!({"at": 0.0, "kind": "input", "title": "用户指令", "seq": 1}));
        log.append_act(&json!({"at": 1.7, "end": 2.0, "kind": "tool", "title": "fs_ls", "seq": 2}));
        log.append(&Message::assistant(Some("好".into()), None, vec![]), None);
        drop(log);

        // 消息侧：只有 2 条，色块不算
        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].msg.role, "user");
        let l = s.list(5);
        assert_eq!(l[0].messages, 2, "活动流记录不该被当成消息计数");
        assert_eq!(l[0].title, "第一句");

        // 活动流侧：两条，顺序与负载原样
        let acts = s.load_acts(&id);
        assert_eq!(acts.len(), 2);
        assert_eq!(acts[0]["title"], "用户指令");
        assert_eq!(acts[1]["end"], 2.0);
        assert_eq!(acts[1]["kind"], "tool", "负载里的 kind 是类别（工具/模型…），不是行类型");
        assert_eq!(acts[1]["seq"], 2);

        // 回滚：消息被丢掉，活动流必须还在
        let mut again = s.open(&id).unwrap();
        again.rewrite(&[Message::user("第一句")]);
        drop(again);
        assert_eq!(s.load(&id).unwrap().len(), 1, "只剩「第一句」");
        assert_eq!(s.load_acts(&id).len(), 2, "活动流不受消息回滚影响");
    }

    /// 序号 / 文件名主干都能定位到同一个对话。
    #[test]
    fn resolve_by_index_and_id() {
        let d = Tmp::new("resolve");
        let s = d.store();
        let mut log = s.create(None).unwrap();
        log.append(&Message::user("唯一一个"), None);
        let id = log.id().to_string();
        drop(log);
        assert_eq!(s.resolve("1").unwrap().id, id);
        assert_eq!(s.resolve(&id).unwrap().id, id);
        assert_eq!(s.resolve(&id[..13]).unwrap().id, id, "支持只写时间戳前缀");
        // 8 位纯数字 = 日期前缀，不能落进「序号」那条分支
        // （否则 `/history rm 20261003` 会报「没有第 20261003 个对话」）
        assert_eq!(s.resolve(&id[..8]).unwrap().id, id, "8 位日期前缀要当 id 前缀");
        assert_eq!(s.resolve(&format!("{id}.jsonl")).unwrap().id, id, "带扩展名也认");
        assert!(s.resolve("9").is_err());
        assert!(s.resolve("0").is_err(), "序号从 1 开始");
        assert!(s.resolve("").is_err(), "空键不该被当成序号");
        assert!(s.resolve("20200101").is_err(), "像日期的前缀找不到就该找不到");
    }

    /// 「接着上次聊」必须写回**原文件**，而不是另起一个。
    ///
    /// 这是切换会话最容易出的错：id 是从 meta 读回来的，一旦读错（或干脆
    /// 重新生成一个），历史里就会出现两条并行的记录，用户还会以为消息丢了。
    #[test]
    fn open_continues_the_same_file() {
        let d = Tmp::new("reopen");
        let s = d.store();
        let mut log = s.create(Some("m")).unwrap();
        let id = log.id().to_string();
        log.append(&Message::user("第一句"), None);
        drop(log); // 关掉句柄，模拟「这次会话结束」

        let mut again = s.open(&id).unwrap();
        again.append(&Message::user("第二句"), None);
        drop(again);

        assert_eq!(s.list(10).len(), 1, "不能多出第二个文件");
        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].msg.content.as_ref().and_then(|v| v.as_str()), Some("第二句"));
    }

    /// 首行不是 meta（手改过 / 早期版本写的文件）也要能接上：`open` 退回用文件名
    /// 当 id，消息照旧追加在原文件末尾，不会另起一个。
    ///
    /// 这类文件在列表里是**看不见**的（`list` 拿首行 meta 当格式校验，免得把
    /// 误放进 `history/` 的其它文件也列出来），所以这里只断言写入行为。
    #[test]
    fn open_tolerates_a_file_without_meta() {
        let d = Tmp::new("nometa");
        let s = d.store();
        std::fs::create_dir_all(&s.dir).unwrap();
        let id = "20260101-000000-abcd";
        let path = file_of(&s.dir, id);
        std::fs::write(&path, "{\"role\":\"user\",\"content\":\"只有消息\"}\n").unwrap();

        let mut log = s.open(id).unwrap();
        assert_eq!(log.path(), path.as_path(), "写回原文件");
        log.append(&Message::user("接着写"), None);
        drop(log);

        let lines = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        assert_eq!(lines, 2, "新消息追加在原文件末尾");
        assert_eq!(std::fs::read_dir(&s.dir).unwrap().count(), 1, "没有另起文件");
    }

    /// 列表按**最后一次对话时间**排，而不是文件名里的开始时间：
    /// 接着聊一段早先开的旧对话，它就该浮到最上面。
    #[test]
    fn list_orders_by_last_activity() {
        let d = Tmp::new("order");
        let s = d.store();
        std::fs::create_dir_all(&s.dir).unwrap();
        let write = |id: &str, text: &str| {
            let meta = Meta {
                id: id.to_string(),
                created: 0,
                model: None,
                root: None,
                version: None,
            };
            let body =
                format!("{}\n{}\n", meta_line(&meta), message_line(&Message::user(text), None));
            std::fs::write(file_of(&s.dir, id), body).unwrap();
        };

        // 名字早的那段后写：这样「按文件名排」与「按 mtime 排」结论相反，
        // 断言才分得开是哪种排序在起作用。（NTFS 的 mtime 精度是 100ns，
        // 30ms 足够拉开；文件系统粒度粗到秒的话这条测试会不可靠。）
        write("20260102-100000-bbbb", "先开的对话");
        std::thread::sleep(std::time::Duration::from_millis(30));
        write("20260101-100000-aaaa", "后开的对话");
        assert_eq!(s.list(10)[0].id, "20260101-100000-aaaa", "后写的文件在前");

        // 回到那段旧对话里再说一句 —— 它应该立刻跳到最前面
        let mut log = s.open("20260102-100000-bbbb").unwrap();
        log.append(&Message::user("又聊了一句"), None);
        drop(log);

        let items = s.list(10);
        assert_eq!(items[0].id, "20260102-100000-bbbb", "刚聊过的那段排最前");
        assert_eq!(items[0].messages, 2);
        assert!(items[0].time.contains(':'), "显示时间取自 mtime，不该为空: {}", items[0].time);
    }

    /// token 用量必须能跟着对话文件活过重启 —— 这条钉住「落盘 → 读回」整条链路。
    #[test]
    fn usage_round_trip() {
        let d = Tmp::new("usage");
        let s = d.store();
        let mut log = s.create(Some("mock-model")).unwrap();
        let id = log.id().to_string();
        let rec = UsageRecord {
            round: 1,
            at: 1_700_000_000,
            calls: 3,
            usage: crate::llm::Usage {
                prompt: Some(384),
                completion: Some(768),
                total: Some(1152),
                reasoning: Some(288),
                cached: Some(192),
                cache_miss: Some(192),
            },
            last_prompt: Some(128),
            session_total: Some(1152),
            session_calls: 3,
        };
        log.append_usage(&rec);
        drop(log);

        let back = s.load_usage(&id);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].round, 1);
        assert_eq!(back[0].at, 1_700_000_000);
        assert_eq!(back[0].calls, 3);
        assert_eq!(back[0].usage, rec.usage, "各项 token 必须逐项往返");
        assert_eq!(back[0].last_prompt, Some(128));
        assert_eq!(back[0].session_total, Some(1152));
        assert_eq!(back[0].session_calls, 3);
    }

    /// 工作区记录：**最后一条才算数**，要活过「中断回滚」，而且 `own`（本对话自建地）
    /// 在切到别的档之后**不能丢** —— 丢了「切走再切回默认」就会另建一块，
    /// `workplace/` 会被空目录堆满。
    #[test]
    fn workplace_record_keeps_the_last_one_across_rewrite() {
        use crate::workplace::Record;

        let d = Tmp::new("workplace");
        let s = d.store();
        let mut log = s.create(None).unwrap();
        let id = log.id().to_string();

        assert_eq!(s.load_workplace(&id), None, "还没写过就是 None（旧对话文件都走这一支）");

        let own = PathBuf::from(r"C:\tmp\workplace\20261004200000");
        // ① 首条消息时根还没定
        log.append_workplace(&Record { mode: "default".into(), root: None, own: None });
        log.append(&Message::user("第一句"), None);
        // ② 地建好了
        log.append_workplace(&Record {
            mode: "default".into(),
            root: Some(own.clone()),
            own: Some(own.clone()),
        });
        // ③ 切到完全访问：当前档位没有根了，但 `own` 必须留着
        log.append_workplace(&Record { mode: "full".into(), root: None, own: Some(own.clone()) });
        drop(log);

        let got = s.load_workplace(&id).expect("应当读到最后一条");
        assert_eq!(got.mode, "full", "取的是最后一条，不是第一条");
        assert_eq!(got.root, None, "full 档没有根");
        assert_eq!(got.own.as_ref(), Some(&own), "切档不该把「本对话自建地」弄丢");

        // 回滚重写消息：工作区记录必须原样还在
        let mut log = s.open(&id).unwrap();
        log.rewrite(&[Message::system("sys"), Message::user("只剩这句")]);
        drop(log);
        let back = s.load_workplace(&id).expect("回滚不该丢掉工作区记录");
        assert_eq!(back.mode, "full");
        assert_eq!(back.own.as_ref(), Some(&own), "中断回滚只该丢消息");
        assert_eq!(s.load(&id).unwrap().len(), 1, "消息确实被回滚成 1 条");
    }

    /// 端点少报一项，读回来就该是 `None` —— 不能变成 0。
    /// 「没有消耗」和「对方没报」是两件事，混起来就成了假账。
    #[test]
    fn usage_keeps_missing_fields_absent() {
        let d = Tmp::new("usage-sparse");
        let s = d.store();
        let mut log = s.create(None).unwrap();
        let id = log.id().to_string();
        log.append_usage(&UsageRecord {
            round: 1,
            at: 1,
            calls: 1,
            usage: crate::llm::Usage { total: Some(384), ..Default::default() },
            last_prompt: None,
            session_total: Some(384),
            session_calls: 1,
        });
        drop(log);

        let back = s.load_usage(&id);
        assert_eq!(back[0].usage.total, Some(384));
        assert_eq!(back[0].usage.reasoning, None, "没报就是没有，不许变成 0");
        assert_eq!(back[0].last_prompt, None);
        assert!(!back[0].is_empty());
    }

    /// 中断回滚丢的是消息，不该顺手把「已经记下的账」与「已经发生过的时间轴」抹掉
    #[test]
    fn rewrite_keeps_usage_and_acts() {
        let d = Tmp::new("usage-rollback");
        let s = d.store();
        let id;
        {
            let mut log = s.create(None).unwrap();
            id = log.id().to_string();
            log.append(&Message::user("第一句"), None);
            log.append(&Message::assistant(Some("半截".into()), None, vec![]), None);
            log.append_usage(&UsageRecord {
                round: 1,
                at: 1,
                calls: 1,
                usage: crate::llm::Usage { total: Some(10), ..Default::default() },
                last_prompt: Some(4),
                session_total: Some(10),
                session_calls: 1,
            });
            log.append_act(&json!({ "kind": "tool", "title": "fs_ls", "at": 0.5 }));
        }

        // 回滚到只剩第一条消息
        let mut log = s.open(&id).unwrap();
        let mut msgs: Vec<Message> = s.load(&id).unwrap().into_iter().map(|l| l.msg).collect();
        msgs.truncate(1);
        log.rewrite(&msgs);
        drop(log);

        assert_eq!(s.load(&id).unwrap().len(), 1, "回滚后只剩一条消息");
        assert_eq!(s.load_usage(&id).len(), 1, "用量记录必须原样保留");
        assert_eq!(s.load_acts(&id).len(), 1, "活动流也必须原样保留");
    }

    /// 侧边栏的「几条消息」不能把 usage 行算进去，否则条数会虚高
    #[test]
    fn summary_counts_messages_only() {
        let d = Tmp::new("usage-count");
        let s = d.store();
        {
            let mut log = s.create(Some("mock-model")).unwrap();
            log.append(&Message::user("看看现在有什么能力"), None);
            log.append_usage(&UsageRecord {
                round: 1,
                at: 1,
                calls: 1,
                usage: crate::llm::Usage { total: Some(384), ..Default::default() },
                last_prompt: None,
                session_total: Some(384),
                session_calls: 1,
            });
        }

        let items = s.list(10);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].messages, 1, "usage 行不算消息");
        assert_eq!(items[0].title, "看看现在有什么能力");
    }

    /// `remove` 收编了原先散在 `main.rs` 里的删除动作：删得掉、删完列表就少一个，
    /// 而且用序号或 id 都可以。这是「换后端也能实现删除」的前提。
    #[test]
    fn remove_deletes_by_index_or_id() {
        let d = Tmp::new("remove");
        let s = d.store();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let mut log = s.create(None).unwrap();
            log.append(&Message::user("x"), None);
            ids.push(log.id().to_string());
            drop(log);
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
        assert_eq!(s.list(10).len(), 2);

        let removed = s.remove(&ids[0]).unwrap();
        assert_eq!(removed.id, ids[0]);
        assert_eq!(s.list(10).len(), 1);
        assert_eq!(s.list(10)[0].id, ids[1]);
        assert!(!file_of(&s.dir, &ids[0]).exists(), "文件确实被删了");

        // 按序号也删得掉（剩下的那个是第 1 个）
        let removed = s.remove("1").unwrap();
        assert_eq!(removed.id, ids[1]);
        assert!(s.list(10).is_empty());
    }
}
