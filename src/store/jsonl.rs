//! The JSONL backend: one file per conversation, all of them under `<app_root>/.aox/conversations/`...
//! 20261002-153912-a1b2.jsonl.
//! One store, one directory. Conversations used to be scattered across `<workspace>/.aox/history` for every...

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::attach;
use crate::history::{
    local_now, unix_now, Local, Loaded, Meta, Summary, UsageRecord, DEFAULT_LIST_LIMIT, HOST_PREFIX,
};
use crate::message::Message;
use crate::store::{Session, Store, TITLE_MAX_CHARS};

/// The conversation store's directory name (relative to the app root).
const DIR_NAME: &str = "conversations";
const EXT: &str = "jsonl";

/// The title is only searched for in the first few lines: the first user message is always on line 2,
/// so a few lines of headroom is enough — no need to parse an entire tool result just for a title.
const TITLE_SCAN_LINES: usize = 6;

/// Compose "10-02 15:39". Years other than the current one get the year prepended, otherwise
/// "10-02" would be mistaken for this year.
fn display_parts(y: u16, mo: u16, d: u16, hh: u16, mm: u16, this_year: u16) -> String {
    if y == this_year {
        format!("{mo:02}-{d:02} {hh:02}:{mm:02}")
    } else {
        format!("{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}")
    }
}

/// Parse the display time (the time this conversation started) out of the filename stem `YYYYMMDD-HHMMSS`.
fn display_time(id: &str, this_year: u16) -> String {
    let b = id.as_bytes();
    if b.len() < 15 || b[8] != b'-' {
        return id.to_string();
    }
    // Parse segment by segment instead of slicing the string directly: a non-ASCII id might not slice on a char boundary
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

/// On Windows we let `FileTimeToLocalFileTime` compute the timezone and DST; deriving the offset from "current...
#[cfg(windows)]
fn local_from_system(t: SystemTime) -> Option<Local> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    use windows_sys::Win32::System::Time::FileTimeToSystemTime;

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
        weekday: st.wDayOfWeek,
        hour: st.wHour,
        minute: st.wMinute,
        second: st.wSecond,
    })
}

#[cfg(not(windows))]
fn local_from_system(t: SystemTime) -> Option<Local> {
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let (year, month, day, hour, minute, second) = crate::history::civil_from_unix(secs);
    let weekday = crate::history::weekday_of(year, month, day);
    Some(Local { year, month, day, weekday, hour, minute, second })
}

/// The list is sorted by last conversation time, so what we display must be that same time — otherwise...
fn display_mtime(t: SystemTime, this_year: u16) -> Option<String> {
    let l = local_from_system(t)?;
    Some(display_parts(l.year, l.month, l.day, l.hour, l.minute, this_year))
}

// Row formats

/// The type tag of a row.
/// Deliberately not using serde's internally tagged enum (`#[serde(tag = "kind")]`) + `flatten`: that combination...
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
        // An attachment's payload (base64) never goes into the file — one screenshot is several MB...
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

/// One activity-stream row. The payload is nested under the `act` key — `Act` itself has a `kind` field
/// (input / model / tool …) which would overwrite the row type if on the same name and level.
fn act_line(act: &Value) -> Value {
    json!({ "kind": "act", "act": act })
}

fn usage_line(rec: &UsageRecord) -> Value {
    let mut v = rec.to_value();
    v["kind"] = json!("usage");
    v
}

/// One task-checkpoint row. The payload is nested under `task` (like `act`) because the checkpoint carries its own...
fn task_line(task: &Value) -> Value {
    json!({ "kind": "task", "task": task })
}

/// One workplace row. The fields are decided by [`crate::workplace::Record::to_value`] — that side
/// handles "don't write absent fields" (the `full` tier has no root); here we only add the row type.
fn workplace_line(rec: &crate::workplace::Record) -> Value {
    let mut v = rec.to_value();
    v["kind"] = json!("workplace");
    v
}

/// One rename row. `None` writes `{"kind":"title"}` with no `title` key at all.
fn title_line(title: Option<&str>) -> Value {
    let mut v = json!({ "kind": "title" });
    if let Some(t) = title {
        v["title"] = json!(t);
    }
    v
}

/// The row types we write out.
/// ️ Adding a kind here is only half the work: JsonlSession::rewrite pulls each non-message kind out and writes...
const ROW_KINDS: [&str; 7] = ["meta", "message", "act", "usage", "workplace", "title", "task"];

/// Parse one line; if it's recognized, return its object.
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
    // One write, rather than "write the body, then write the newline": one line either lands whole or not at all...
    file.write_all(line.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Generate an id of the form `<YYYYMMDD>-<HHMMSS>-<4 hex digits>`, and guarantee no name collision within the same directory.
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

/// The file path for a given conversation id.
fn file_of(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.{EXT}"))
}

/// Where a workspace's history directory sits: `<workspace>/.aox/history/`.
fn dir_of(root: &Path) -> PathBuf {
    root.join(crate::workplace::AOX_DIR).join(DIR_NAME)
}

// Backend

/// The JSONL backend: one file per conversation, all under `<app root>/.aox/conversations/`.
pub struct JsonlStore {
    /// The app root. The meta row's `root` field records it.
    root: PathBuf,
    /// The conversation directory (`<app root>/.aox/conversations`). The only one.
    home: PathBuf,
    /// How many conversations to keep (0 = no limit). Global by design: "keep the newest 200" is a
    /// different promise from "each workspace keeps its own 200".
    keep: usize,
}

impl JsonlStore {
    pub fn new(root: &Path, keep: usize) -> Self {
        let root = root.to_path_buf();
        let home = dir_of(&root);
        Self { root, home, keep }
    }

    /// Locate `id` in the store.
    /// Ids are globally unique (a timestamp plus a random salt). `exists()` rather than trying to open: opening...
    fn find(&self, id: &str) -> Option<PathBuf> {
        let p = file_of(&self.home, id);
        p.exists().then_some(p)
    }
}

impl Store for JsonlStore {
    fn backend(&self) -> &'static str {
        "jsonl"
    }

    fn location(&self) -> &Path {
        &self.home
    }

    /// `keep` is fixed at construction time (from `config.toml history keep`).
    fn create(&self, model: Option<&str>, birth: &crate::store::Birth) -> Result<Box<dyn Session>> {
        if self.keep > 0 {
            prune_all(&self.home, self.keep);
        }
        JsonlSession::create(&self.home, model, &self.root, birth).map(|s| Box::new(s) as _)
    }

    fn open(&self, id: &str) -> Result<Box<dyn Session>> {
        let path = self
            .find(id)
            .with_context(|| crate::tf!("No conversation found for {} (see /history for the list)", id))?;
        JsonlSession::open(&path).map(|s| Box::new(s) as _)
    }

    fn list(&self, limit: usize) -> Vec<Summary> {
        list_all(&self.home, limit)
    }

    fn resolve(&self, key: &str) -> Result<Summary> {
        resolve_any(&self.home, key)
    }

    fn load(&self, id: &str) -> Result<Vec<Loaded>> {
        let path = self
            .find(id)
            .with_context(|| crate::tf!("No conversation found for {} (see /history for the list)", id))?;
        load(&path)
    }

    fn load_acts(&self, id: &str) -> Vec<Value> {
        self.find(id).map(|p| load_acts(&p)).unwrap_or_default()
    }

    fn load_usage(&self, id: &str) -> Vec<UsageRecord> {
        self.find(id).map(|p| load_usage(&p)).unwrap_or_default()
    }

    fn load_workplace(&self, id: &str) -> Option<crate::workplace::Record> {
        // "The last one" = this conversation's current workspace
        self.find(id).map(|p| load_workplaces(&p)).unwrap_or_default().pop()
    }

    fn load_task(&self, id: &str) -> Option<Value> {
        // "The last one" = the checkpoint as it currently stands; earlier rows are the same task at
        // earlier moments, which nobody reads.
        self.find(id).map(|p| load_tasks(&p)).unwrap_or_default().pop()
    }

    fn remove(&self, key: &str) -> Result<Summary> {
        // Deletion used to be scattered in `main.rs` (a direct `std::fs::remove_file`) — folding it into the backend...
        let s = resolve_any(&self.home, key)?;
        std::fs::remove_file(&s.path)
            .with_context(|| crate::tf!("Cannot delete {}", s.path.display()))?;
        Ok(s)
    }

    fn prune(&self, keep: usize) -> usize {
        prune_all(&self.home, keep)
    }
}

// Writing

/// A conversation being written (formerly `history::Log`)
pub struct JsonlSession {
    path: PathBuf,
    file: File,
    meta: Meta,
}

impl JsonlSession {
    /// Create a new conversation file and write the meta row.
    fn create(
        dir: &Path,
        model: Option<&str>,
        root: &Path,
        birth: &crate::store::Birth,
    ) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| crate::tf!("Cannot create the conversation directory {}", dir.display()))?;
        let meta = Meta {
            id: new_id(dir),
            created: unix_now(),
            model: model.map(str::to_string),
            root: Some(root.display().to_string()),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            origin: Some(birth.origin.as_str().to_string()),
            space: birth.space.clone(),
            room: birth.room.as_ref().map(|(id, name)| crate::history::MetaRoom {
                id: id.clone(),
                name: name.clone(),
            }),
        };
        let path = file_of(dir, &meta.id);
        let file = OpenOptions::new().create(true).write(true).append(true).open(&path)?;
        let mut log = Self { path, file, meta };
        let head = meta_line(&log.meta);
        write_line_to(&mut log.file, &head)?;
        Ok(log)
    }

    /// Resume an already-existing conversation (keep writing into it).
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| crate::tf!("Cannot open {}", path.display()))?;
        let first = BufReader::new(file).lines().next();
        let meta = first
            .and_then(|l| l.ok())
            .and_then(|l| serde_json::from_str::<Meta>(l.trim()).ok())
            // First line isn't meta (hand-edited / written by an older version): fall back to using the
            // filename as the id, so at least we can keep writing instead of failing to open the whole conversation
            .unwrap_or_else(|| Meta {
                id: path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string(),
                created: 0,
                model: None,
                root: None,
                version: None,
                origin: None,
                space: None,
                room: None,
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

    /// Append one message. A write failure returns no error — history must not interrupt the conversation.
    fn append(&mut self, msg: &Message, is_error: Option<bool>) {
        if msg.role == "system" {
            return; // The system prompt changes every round and is long and useless, so it isn't stored
        }
        let line = message_line(msg, is_error);
        let _ = write_line_to(&mut self.file, &line);
    }

    /// Append one activity-stream record (one colored block on the timeline in the UI).
    fn append_act(&mut self, act: &Value) {
        let _ = write_line_to(&mut self.file, &act_line(act));
    }

    /// Append one round's token ledger.
    fn append_usage(&mut self, rec: &UsageRecord) {
        let _ = write_line_to(&mut self.file, &usage_line(rec));
    }

    /// Append one "workplace for this conversation" record.
    fn append_workplace(&mut self, rec: &crate::workplace::Record) {
        let _ = write_line_to(&mut self.file, &workplace_line(rec));
    }

    /// Append one rename row (`None` = "show the automatic title again"). Same as above: silent on
    /// failure, and appended rather than patched into the meta row, so the file only ever grows.
    fn append_title(&mut self, title: Option<&str>) {
        let _ = write_line_to(&mut self.file, &title_line(title));
    }

    /// Append one task checkpoint. Same silent-on-failure rule as everything else here: losing the
    /// newest checkpoint costs one "where was I", it must not break the conversation in progress.
    fn append_task(&mut self, task: &Value) {
        let _ = write_line_to(&mut self.file, &task_line(task));
    }

    /// Rewrite the whole file with the given message list.
    fn rewrite(&mut self, msgs: &[Message]) {
        let acts = load_acts(&self.path);
        let usages = load_usage(&self.path);
        let workplaces = load_workplaces(&self.path);
        let titles = load_titles(&self.path);
        let task = load_tasks(&self.path).pop();
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
        if let Some(t) = &task {
            if write_line_to(&mut f, &task_line(t)).is_err() {
                return;
            }
        }
        // The rename rows go last, in their original order: their meaning is "the last one wins", so
        // they have to keep their relative order even though nothing else here cares.
        for t in &titles {
            if write_line_to(&mut f, &title_line(t.as_deref())).is_err() {
                return;
            }
        }
        drop(f);
        // Swap the append handle back, so subsequent appends still "always write at the end" and never overwrite existing content
        if let Ok(f) = OpenOptions::new().write(true).append(true).open(&self.path) {
            self.file = f;
        }
    }
}

// Reading

/// One history file in the directory.
struct Entry {
    id: String,
    path: PathBuf,
    /// The time of the last write (file mtime), i.e. the "last conversation time"
    mtime: Option<SystemTime>,
}

/// Scan the directory, ordered from newest to oldest by last conversation time.
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
    // When mtimes are equal (two files created in the same second) fall back to the filename: the filename starts...
    files.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.id.cmp(&a.id)));
    files
}

/// List history conversations across every workspace, most recently talked-to first.
fn list_all(dir: &Path, limit: usize) -> Vec<Summary> {
    let this_year = local_now().year;
    let mut all: Vec<Entry> = collect(dir);
    all.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.id.cmp(&a.id)));
    all.into_iter().take(limit).filter_map(|e| read_summary(e, this_year)).collect()
}

fn read_summary(e: Entry, this_year: u16) -> Option<Summary> {
    let Entry { id, path, mtime } = e;
    let file = File::open(&path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();

    // Line 1 is the meta (id / model / version / origin / space / room): consumed here, and it doubles as "this...
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let meta: crate::history::Meta = match parse_row(line.trim())
        .and_then(|v| serde_json::from_value::<crate::history::Meta>(v).ok())
    {
        Some(m) => m,
        None => return None,
    };
    let origin = crate::history::Origin::parse(meta.origin.as_deref().unwrap_or(""));
    let born_in = meta.space.clone();
    let room_name = meta.room.as_ref().map(|r| r.name.clone()).filter(|n| !n.is_empty());

    let mut messages = 0usize;
    let mut title: Option<String> = None;
    // The workspace this conversation is bound to: the last `kind:"workplace"` record wins (switching tier appends...
    let mut ws: Option<crate::workplace::Record> = None;
    // What the user last renamed this conversation to.
    let mut renamed: Option<Option<String>> = None;
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
        if is_kind(&v, "title") {
            // Clamped on the way **out** as well as on the way in: a hand-edited file (or a title
            // typed before the rule was written) must not be able to break the one-line row.
            renamed = Some(
                v.get("title")
                    .and_then(Value::as_str)
                    .map(clamp_title)
                    .filter(|s| !s.is_empty()),
            );
            continue;
        }
        if is_kind(&v, "workplace") {
            // An unparsable record is skipped, not treated as "no workspace": a half-written last
            // line must not throw away the good record that came before it.
            if let Some(rec) = crate::workplace::Record::from_value(&v) {
                ws = Some(rec);
            }
            continue;
        }
        // The activity stream (`kind:"act"`) and usage (`kind:"usage"`) are neither counted in "message count" nor used...
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
        // Show "last conversation time" (the same thing the list is sorted by); when mtime is unavailable
        // fall back to the start time from the filename — better to show something older than to leave a blank.
        time: mtime
            .and_then(|t| display_mtime(t, this_year))
            .unwrap_or_else(|| display_time(&id, this_year)),
        id,
        path,
        // The user's rename beats the derived title when there is one; clearing it (a `title` row
        // with no value) lets the first user message show through again.
        title: renamed
            .flatten()
            .or(title)
            .unwrap_or_else(|| crate::i18n::t("(empty conversation)").into()),
        messages,
        ws_root: ws.as_ref().and_then(|r| r.root.clone()),
        ws_full: ws.as_ref().is_some_and(|r| r.mode == "full"),
        origin,
        space: born_in,
        room_name,
    })
}

/// Read out the whole conversation's message list. Bad lines are simply skipped — one line shouldn't
/// ruin the whole file's readability.
fn load(path: &Path) -> Result<Vec<Loaded>> {
    let file = File::open(path).with_context(|| crate::tf!("Cannot open {}", path.display()))?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(v) = parse_row(t) else { continue };
        if !is_kind(&v, "message") {
            continue; // An activity-stream record isn't a message, so it's skipped when replaying the conversation
        }
        let is_error = v.get("is_error").and_then(Value::as_bool);
        if let Ok(msg) = serde_json::from_value::<Message>(v) {
            out.push(Loaded { msg, is_error });
        }
    }
    Ok(out)
}

/// Read out all activity-stream records in a file (in write order), returning the payloads verbatim.
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

/// Read out all token usage records in a file (in write order, i.e. round order).
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

/// Read all workplace records in a file (in write order) — the caller takes the last one == the current plot.
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

/// Read all `kind:"task"` payloads in a file, in write order — the caller takes the last one (the checkpoint...
fn load_tasks(path: &Path) -> Vec<Value> {
    let Ok(file) = File::open(path) else { return Vec::new() };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(v) = parse_row(t) else { continue };
        if !is_kind(&v, "task") {
            continue;
        }
        if let Some(payload) = v.get("task") {
            out.push(payload.clone());
        }
    }
    out
}

/// Read all `kind:"title"` rows in a file, in write order — the caller takes the last one == what the conversation...
fn load_titles(path: &Path) -> Vec<Option<String>> {
    let Ok(file) = File::open(path) else { return Vec::new() };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(v) = parse_row(t) else { continue };
        if !is_kind(&v, "title") {
            continue;
        }
        out.push(v.get("title").and_then(Value::as_str).map(str::to_string));
    }
    out
}

/// Keep only the newest `keep` conversations across every workspace, deleting the rest. `keep == 0` means no limit.
fn prune_all(dir: &Path, keep: usize) -> usize {
    if keep == 0 {
        return 0;
    }
    let mut all: Vec<Entry> = collect(dir);
    all.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.id.cmp(&a.id)));
    let mut n = 0;
    for e in all.into_iter().skip(keep) {
        if std::fs::remove_file(&e.path).is_ok() {
            n += 1;
        }
    }
    n
}

/// Resolve the "index" or "filename stem" the user gave into an actual file.
fn resolve_any(dir: &Path, key: &str) -> Result<Summary> {
    let key = key.trim().trim_end_matches(&format!(".{EXT}"));
    // An empty key must be blocked here: `id.starts_with("")` is always true, so letting it through would...
    if key.is_empty() {
        bail!(crate::i18n::t("Give a conversation number or file name (see /history for the list)"));
    }
    let is_index = key.len() < 8 && key.bytes().all(|c| c.is_ascii_digit());
    let items = list_all(dir, DEFAULT_LIST_LIMIT);
    if is_index {
        let n: usize = key
            .parse()
            .with_context(|| crate::tf!("Invalid number: {}", key))?;
        if n == 0 {
            bail!(crate::i18n::t("Numbers start at 1"));
        }
        return items
            .into_iter()
            .nth(n - 1)
            .with_context(|| crate::tf!("There is no conversation number {} (see /history for the list)", n));
    }
    items
        .into_iter()
        .find(|s| s.id == key || s.id.starts_with(key))
        .with_context(|| crate::tf!("No conversation found for {} (see /history for the list)", key))
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FunctionCall, ToolCall};

    /// A temp directory for tests.
    /// It plays the role of the app root: history lands in `<tmp>/history/`, matching the real layout (which...
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("aox-store-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            Self(d)
        }

        /// A backend that "doesn't prune and doesn't care about the model name" — the most common one
        /// in tests.
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

    /// A message with an attachment must have its payload (base64) stripped when persisted, while the body...
    #[test]
    fn attachment_payloads_are_not_persisted() {
        let d = Tmp::new("attach-slim");
        let s = d.store();
        let mut log = s.create(Some("deepseek-flash"), &Default::default()).unwrap();
        let id = log.id().to_string();

        let png = d.0.join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();
        let a = crate::attach::Attachment::probe(&png).unwrap();
        log.append(&Message::user_with_files("Look at this", &[a]).unwrap(), None);
        drop(log);

        // Look straight at the file: not one occurrence of base64.
        let file = std::fs::read_dir(s.location())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .expect("the conversation file should have been written by now");
        let raw = std::fs::read_to_string(file).unwrap();
        assert!(!raw.contains("base64"), "the payload must not go into the conversation file: {}", &raw[..raw.len().min(400)]);
        assert!(raw.contains("shot.png"), "the attachment listing must stay (history must show what was attached)");
        assert!(raw.contains("Look at this"), "the body of course must stay");

        // Read back, it's still a **legal** user message (the image block was dropped whole, with no
        // fake URL left behind)
        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].msg.role, "user");
        assert!(items[0].msg.flat_text().contains("Look at this"), "{}", items[0].msg.flat_text());
        let names = crate::attach::names_from_content(items[0].msg.content.as_ref().unwrap());
        assert_eq!(names, vec!["shot.png".to_string()], "reopening an old conversation must redraw the chips");
    }

    /// Whatever goes in should come back out as the same messages, in the same order, with the same is_error.
    #[test]
    fn round_trip_messages() {
        let d = Tmp::new("rt");
        let s = d.store();
        let mut log = s.create(Some("deepseek-chat"), &Default::default()).unwrap();
        let id = log.id().to_string();

        log.append(&Message::user("first line\nsecond line \"quoted\" and more"), None);
        log.append(&Message::assistant(None, Some("Let me have a look".into()), vec![call("c1", "fs_ls", r#"{"path":"."}"#)]), None);
        log.append(&Message::tool_result("c1", "fs_ls", "listed 3 items".into()), Some(false));
        log.append(&Message::assistant(None, None, vec![call("c2", "fs_read", r#"{"path":"nope"}"#)]), None);
        log.append(&Message::tool_result("c2", "fs_read", "file does not exist".into()), Some(true));
        log.append(&Message::assistant(Some("Done.".into()), None, vec![]), None);
        drop(log);

        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 6, "system messages are not stored, so all 6 messages should be read back");
        assert_eq!(items[0].msg.role, "user");
        assert_eq!(items[0].msg.content.as_ref().and_then(|v| v.as_str()), Some("first line\nsecond line \"quoted\" and more"));
        assert_eq!(items[0].is_error, None);
        assert_eq!(items[1].msg.tool_calls.as_ref().unwrap()[0].function.name, "fs_ls");
        // reasoning_content must survive the round trip: thinking models require it to be sent back
        // verbatim
        assert_eq!(items[1].msg.reasoning_content.as_deref(), Some("Let me have a look"));
        // The tool result's success/failure marker hangs off the message's outer layer, not in
        // Message's wire fields
        assert_eq!(items[2].is_error, Some(false));
        assert_eq!(items[2].msg.name.as_deref(), Some("fs_ls"));
        assert_eq!(items[3].msg.tool_calls.as_ref().unwrap()[0].function.name, "fs_read");
        assert_eq!(items[3].is_error, None, "an assistant message has no tool-result marker");
        assert_eq!(items[4].is_error, Some(true));
        assert_eq!(items[5].msg.text(), Some("Done."));

        // List: the title comes from the first user message, and the count excludes meta
        let list = s.list(10);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].messages, 6);
        assert_eq!(list[0].title, "first line");
        assert!(list[0].time.contains(':'), "the displayed time looks like 10-02 15:39: {}", list[0].time);
    }

    /// The system prompt is not stored: it changes every round and is long and useless.
    #[test]
    fn system_prompt_is_skipped() {
        let d = Tmp::new("sys");
        let s = d.store();
        let mut log = s.create(None, &Default::default()).unwrap();
        log.append(&Message::system("You are AOX…"), None);
        log.append(&Message::user("hi"), None);
        drop(log);
        let id = s.list(1)[0].id.clone();
        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].msg.role, "user");
    }

    /// Interruption rollback: the file must follow memory back to the pre-task state, leaving no
    /// half-written tool_calls behind.
    #[test]
    fn rewrite_drops_rolled_back_turn() {
        let d = Tmp::new("rw");
        let s = d.store();
        let mut log = s.create(None, &Default::default()).unwrap();
        let id = log.id().to_string();
        let path = log.path().to_path_buf();
        let mut msgs = vec![Message::system("sys")];
        log.append(&msgs[0], None);
        msgs.push(Message::user("keep me"));
        log.append(&msgs[1], None);
        // This round was interrupted and rolled back
        let half = vec![Message::user("drop me"), Message::assistant(None, None, vec![call("c9", "x", "{}")])];
        for m in &half {
            log.append(m, None);
        }
        assert_eq!(s.load(&id).unwrap().len(), 3);

        msgs.truncate(2);
        log.rewrite(&msgs);
        drop(log);

        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 1, "only 'keep me' is left");
        assert_eq!(items[0].msg.content.as_ref().and_then(|v| v.as_str()), Some("keep me"));
        // The meta row must still be there, otherwise this file can't recognize itself
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 2, "meta + 1 message");
        assert!(raw.contains("\"kind\":\"meta\""));
    }

    /// A host-injected prompt shouldn't be used as a title
    #[test]
    fn host_prompt_is_not_a_title() {
        let d = Tmp::new("host");
        let s = d.store();
        let mut log = s.create(None, &Default::default()).unwrap();
        log.append(&Message::user("(host hint) your previous reply was empty."), None);
        log.append(&Message::assistant(Some("ok".into()), None, vec![]), None);
        log.append(&Message::user("fix the login bug"), None);
        drop(log);
        let items = s.list(5);
        assert_eq!(items[0].title, "fix the login bug");
    }

    /// The retention policy: keep only the newest `keep`
    #[test]
    fn prune_keeps_newest() {
        let d = Tmp::new("prune");
        let s = d.store();
        for _ in 0..5 {
            let mut log = s.create(None, &Default::default()).unwrap();
            log.append(&Message::user("x"), None);
            drop(log);
            // Filename precision is to the second, so sleep to guarantee different timestamps
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
        assert_eq!(collect(&s.home).len(), 5);
        let removed = s.prune(3);
        assert_eq!(removed, 2);
        assert_eq!(collect(&s.home).len(), 3);
        // keep = 0 means no limit
        assert_eq!(s.prune(0), 0);
        assert_eq!(collect(&s.home).len(), 3);
    }

    /// A bad line must not destroy the whole file.
    #[test]
    fn corrupt_line_is_skipped() {
        let d = Tmp::new("bad");
        let s = d.store();
        let mut log = s.create(None, &Default::default()).unwrap();
        let id = log.id().to_string();
        let path = log.path().to_path_buf();
        log.append(&Message::user("fine"), None);
        drop(log);
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{not JSON\n{\"kind\":\"unknown\"}\n");
        std::fs::write(&path, raw).unwrap();
        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].msg.content.as_ref().and_then(|v| v.as_str()), Some("fine"));
    }

    /// The activity stream and messages share one file without interfering with each other: - `load` yields only...
    #[test]
    fn acts_live_alongside_messages() {
        let d = Tmp::new("act");
        let s = d.store();
        let mut log = s.create(None, &Default::default()).unwrap();
        let id = log.id().to_string();
        log.append(&Message::user("first line"), None);
        log.append_act(&json!({"at": 0.0, "kind": "input", "title": "user instruction", "seq": 1}));
        log.append_act(&json!({"at": 1.7, "end": 2.0, "kind": "tool", "title": "fs_ls", "seq": 2}));
        log.append(&Message::assistant(Some("ok".into()), None, vec![]), None);
        drop(log);

        // Message side: only 2, colored blocks don't count
        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].msg.role, "user");
        let l = s.list(5);
        assert_eq!(l[0].messages, 2, "activity-stream records must not be counted as messages");
        assert_eq!(l[0].title, "first line");

        // Activity-stream side: two, order and payloads verbatim
        let acts = s.load_acts(&id);
        assert_eq!(acts.len(), 2);
        assert_eq!(acts[0]["title"], "user instruction");
        assert_eq!(acts[1]["end"], 2.0);
        assert_eq!(acts[1]["kind"], "tool", "the payload's kind is a category (tool/model…), not a row type");
        assert_eq!(acts[1]["seq"], 2);

        // Rollback: messages are dropped, the activity stream must still be there
        let mut again = s.open(&id).unwrap();
        again.rewrite(&[Message::user("first line")]);
        drop(again);
        assert_eq!(s.load(&id).unwrap().len(), 1, "only 'first line' is left");
        assert_eq!(s.load_acts(&id).len(), 2, "the activity stream is unaffected by a message rollback");
    }

    /// Both an index and a filename stem resolve to the same conversation.
    #[test]
    fn resolve_by_index_and_id() {
        let d = Tmp::new("resolve");
        let s = d.store();
        let mut log = s.create(None, &Default::default()).unwrap();
        log.append(&Message::user("the only one"), None);
        let id = log.id().to_string();
        drop(log);
        assert_eq!(s.resolve("1").unwrap().id, id);
        assert_eq!(s.resolve(&id).unwrap().id, id);
        assert_eq!(s.resolve(&id[..13]).unwrap().id, id, "a timestamp-only prefix is accepted");
        // 8 pure digits = a date prefix, and must not fall into the "index" branch
        // (otherwise `/history rm 20261003` would report "there is no conversation number 20261003")
        assert_eq!(s.resolve(&id[..8]).unwrap().id, id, "an 8-digit date prefix must be treated as an id prefix");
        assert_eq!(s.resolve(&format!("{id}.jsonl")).unwrap().id, id, "the extension is accepted too");
        assert!(s.resolve("9").is_err());
        assert!(s.resolve("0").is_err(), "numbers start at 1");
        assert!(s.resolve("").is_err(), "an empty key must not be treated as an index");
        assert!(s.resolve("20200101").is_err(), "a date-looking prefix that does not match must not match");
    }

    /// "Continue where we left off" must write back to the original file, not start another one.
    #[test]
    fn open_continues_the_same_file() {
        let d = Tmp::new("reopen");
        let s = d.store();
        let mut log = s.create(Some("m"), &Default::default()).unwrap();
        let id = log.id().to_string();
        log.append(&Message::user("first line"), None);
        drop(log); // Close the handle, simulating "this session ended"

        let mut again = s.open(&id).unwrap();
        again.append(&Message::user("second line"), None);
        drop(again);

        assert_eq!(s.list(10).len(), 1, "there must not be a second file");
        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].msg.content.as_ref().and_then(|v| v.as_str()), Some("second line"));
    }

    /// A file whose first line isn't meta (hand-edited / written by an early version) must be resumable too: `open`...
    #[test]
    fn open_tolerates_a_file_without_meta() {
        let d = Tmp::new("nometa");
        let s = d.store();
        std::fs::create_dir_all(&s.home).unwrap();
        let id = "20260101-000000-abcd";
        let path = file_of(&s.home, id);
        std::fs::write(&path, "{\"role\":\"user\",\"content\":\"messages only\"}\n").unwrap();

        let mut log = s.open(id).unwrap();
        assert_eq!(log.path(), path.as_path(), "writes back to the original file");
        log.append(&Message::user("keep writing"), None);
        drop(log);

        let lines = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        assert_eq!(lines, 2, "new messages are appended to the end of the original file");
        assert_eq!(std::fs::read_dir(&s.home).unwrap().count(), 1, "no new file was started");
    }

    /// The list is sorted by **last conversation time**, not by the start time in the filename:
    /// continuing an earlier old conversation should float it to the top.
    #[test]
    fn list_orders_by_last_activity() {
        let d = Tmp::new("order");
        let s = d.store();
        std::fs::create_dir_all(&s.home).unwrap();
        let write = |id: &str, text: &str| {
            let meta = Meta {
                id: id.to_string(),
                created: 0,
                model: None,
                root: None,
                version: None,
                origin: None,
                space: None,
                room: None,
            };
            let body =
                format!("{}\n{}\n", meta_line(&meta), message_line(&Message::user(text), None));
            std::fs::write(file_of(&s.home, id), body).unwrap();
        };

        // The earlier-named one is written later: this way "sort by filename" and "sort by mtime" reach opposite...
        write("20260102-100000-bbbb", "the conversation started first");
        std::thread::sleep(std::time::Duration::from_millis(30));
        write("20260101-100000-aaaa", "the conversation started later");
        assert_eq!(s.list(10)[0].id, "20260101-100000-aaaa", "the file written later comes first");

        // Say one more thing in the older conversation — it should jump straight to the top
        let mut log = s.open("20260102-100000-bbbb").unwrap();
        log.append(&Message::user("said one more thing"), None);
        drop(log);

        let items = s.list(10);
        assert_eq!(items[0].id, "20260102-100000-bbbb", "the one just chatted in comes first");
        assert_eq!(items[0].messages, 2);
        assert!(items[0].time.contains(':'), "the displayed time comes from mtime and must not be empty: {}", items[0].time);
    }

    /// Token usage must be able to survive a restart along with the conversation file — this pins the
    /// whole "persist → read back" chain.
    #[test]
    fn usage_round_trip() {
        let d = Tmp::new("usage");
        let s = d.store();
        let mut log = s.create(Some("mock-model"), &Default::default()).unwrap();
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
        assert_eq!(back[0].usage, rec.usage, "every token field must round-trip individually");
        assert_eq!(back[0].last_prompt, Some(128));
        assert_eq!(back[0].session_total, Some(1152));
        assert_eq!(back[0].session_calls, 3);
    }

    /// Workplace records: only the last one counts, it must survive "interruption rollback", and `own` (this...
    #[test]
    fn workplace_record_keeps_the_last_one_across_rewrite() {
        use crate::workplace::Record;

        let d = Tmp::new("workplace");
        let s = d.store();
        let mut log = s.create(None, &Default::default()).unwrap();
        let id = log.id().to_string();

        assert_eq!(s.load_workplace(&id), None, "None when nothing was written yet (old conversation files all take this branch)");

        let own = PathBuf::from(r"C:\tmp\workplace\20261004200000");
        // ① At the first message the root isn't decided yet
        log.append_workplace(&Record { mode: "default".into(), root: None, own: None });
        log.append(&Message::user("first line"), None);
        // ② The plot is built
        log.append_workplace(&Record {
            mode: "default".into(),
            root: Some(own.clone()),
            own: Some(own.clone()),
        });
        // ③ Switch to full access: the current tier has no root, but `own` must stay
        log.append_workplace(&Record { mode: "full".into(), root: None, own: Some(own.clone()) });
        drop(log);

        let got = s.load_workplace(&id).expect("should read the last one");
        assert_eq!(got.mode, "full", "the last one is taken, not the first");
        assert_eq!(got.root, None, "the full tier has no root");
        assert_eq!(got.own.as_ref(), Some(&own), "switching tiers must not lose this conversation's self-built plot");

        // Roll back and rewrite the messages: the workplace record must still be there verbatim
        let mut log = s.open(&id).unwrap();
        log.rewrite(&[Message::system("sys"), Message::user("only this line is left")]);
        drop(log);
        let back = s.load_workplace(&id).expect("a rollback must not lose the workplace record");
        assert_eq!(back.mode, "full");
        assert_eq!(back.own.as_ref(), Some(&own), "an interruption rollback should only lose messages");
        assert_eq!(s.load(&id).unwrap().len(), 1, "messages were indeed rolled back to 1");
    }

    /// A checkpoint is a conversation record, not a sidecar file: it must survive an interruption rollback (messages...
    #[test]
    fn task_checkpoint_keeps_the_last_one_across_rewrite() {
        use crate::task::TaskState;

        let d = Tmp::new("task");
        let s = d.store();
        let mut log = s.create(None, &Default::default()).unwrap();
        let id = log.id().to_string();

        assert!(s.load_task(&id).is_none(), "no checkpoint yet must read as None, not as an error");

        let st = TaskState::new("add task checkpoints");
        log.append_task(&st.to_value());
        let mut st2 = st.clone();
        st2.completed = vec!["host side".into()];
        st2.modified_files = vec![r"E:\rustapp\src\task.rs".into()];
        log.append_task(&st2.to_value());
        log.append(&Message::user("a message that the rollback will drop"), None);
        drop(log);

        let got = TaskState::from_value(&s.load_task(&id).expect("the last row wins")).unwrap();
        assert_eq!(got.goal, "add task checkpoints");
        assert_eq!(got.completed, vec!["host side"], "the last row is the current state");

        // Interruption rollback: messages go back, the checkpoint must not.
        let mut log = s.open(&id).unwrap();
        log.rewrite(&[Message::system("sys"), Message::user("only this line is left")]);
        drop(log);
        let back = TaskState::from_value(&s.load_task(&id).expect("a rollback must not lose the checkpoint"))
            .unwrap();
        assert_eq!(back.modified_files, st2.modified_files, "evidence must survive a rollback");
        assert_eq!(s.load(&id).unwrap().len(), 1, "messages were rolled back to 1");

        // …and the rewrite kept exactly one task row, not two (see `JsonlSession::rewrite`).
        let path = s.find(&id).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let rows = raw.lines().filter(|l| l.contains("\"kind\":\"task\"")).count();
        assert_eq!(rows, 1, "only the last checkpoint row is written back:\n{raw}");
    }

    /// If the endpoint omits one item, reading it back should give `None` — it must not become 0. "Nothing was...
    #[test]
    fn usage_keeps_missing_fields_absent() {
        let d = Tmp::new("usage-sparse");
        let s = d.store();
        let mut log = s.create(None, &Default::default()).unwrap();
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
        assert_eq!(back[0].usage.reasoning, None, "not reported means absent; it must not become 0");
        assert_eq!(back[0].last_prompt, None);
        assert!(!back[0].is_empty());
    }

    /// What an interruption rollback loses is messages; it must not incidentally wipe "the ledger
    /// already recorded" and "the timeline that already happened"
    #[test]
    fn rewrite_keeps_usage_and_acts() {
        let d = Tmp::new("usage-rollback");
        let s = d.store();
        let id;
        {
            let mut log = s.create(None, &Default::default()).unwrap();
            id = log.id().to_string();
            log.append(&Message::user("first line"), None);
            log.append(&Message::assistant(Some("cut off".into()), None, vec![]), None);
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

        // Roll back to just the first message
        let mut log = s.open(&id).unwrap();
        let mut msgs: Vec<Message> = s.load(&id).unwrap().into_iter().map(|l| l.msg).collect();
        msgs.truncate(1);
        log.rewrite(&msgs);
        drop(log);

        assert_eq!(s.load(&id).unwrap().len(), 1, "only one message remains after the rollback");
        assert_eq!(s.load_usage(&id).len(), 1, "the usage record must be preserved verbatim");
        assert_eq!(s.load_acts(&id).len(), 1, "the activity stream must be preserved verbatim too");
    }

    /// The sidebar's "how many messages" must not count usage rows, otherwise the count is inflated
    #[test]
    fn summary_counts_messages_only() {
        let d = Tmp::new("usage-count");
        let s = d.store();
        {
            let mut log = s.create(Some("mock-model"), &Default::default()).unwrap();
            log.append(&Message::user("see what capabilities exist now"), None);
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
        assert_eq!(items[0].messages, 1, "usage rows do not count as messages");
        assert_eq!(items[0].title, "see what capabilities exist now");
    }

    /// `remove` absorbed the deletion action that used to be scattered in `main.rs`: it deletes, the list shrinks...
    #[test]
    fn remove_deletes_by_index_or_id() {
        let d = Tmp::new("remove");
        let s = d.store();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let mut log = s.create(None, &Default::default()).unwrap();
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
        assert!(!file_of(&s.home, &ids[0]).exists(), "the file was indeed deleted");

        // Deleting by index works too (the remaining one is #1)
        let removed = s.remove("1").unwrap();
        assert_eq!(removed.id, ids[1]);
        assert!(s.list(10).is_empty());
    }

    // Marker for the single conversation store; old per-workspace tests were removed with that design.

    /// A rename beats the title derived from the first user message — and clearing it hands the row back to that first...
    #[test]
    fn rename_overrides_the_derived_title_and_clearing_restores_it() {
        let d = Tmp::new("rename");
        let s = d.store();
        let mut log = s.create(None, &Default::default()).unwrap();
        let id = log.id().to_string();
        log.append(&Message::user("the first thing I asked"), None);
        drop(log);

        let derived = s.list(10).pop().unwrap().title;
        assert_eq!(derived, "the first thing I asked", "with no rename, the first user message is the title");

        let mut log = s.open(&id).unwrap();
        log.append_title(Some("Pinned name"));
        drop(log);
        assert_eq!(s.list(10).pop().unwrap().title, "Pinned name", "the rename wins");

        // Renaming again appends a second row; the newest one is the one that counts.
        let mut log = s.open(&id).unwrap();
        log.append_title(Some("Second name"));
        drop(log);
        assert_eq!(s.list(10).pop().unwrap().title, "Second name", "the last rename wins");

        // ...and clearing it (a row with no `title` key) goes back to the derived title, rather than
        // freezing on "Second name" -- the whole reason the override is a nested Option.
        let mut log = s.open(&id).unwrap();
        log.append_title(None);
        drop(log);
        assert_eq!(s.list(10).pop().unwrap().title, derived, "clearing the rename restores the automatic title");
    }

    /// An interrupt rollback rewrites the file from the message list — and must not throw the rename away with it.
    #[test]
    fn a_rollback_keeps_the_rename() {
        let d = Tmp::new("rename-rollback");
        let s = d.store();
        let mut log = s.create(None, &Default::default()).unwrap();
        let id = log.id().to_string();
        let keep = Message::user("kept");
        log.append(&keep, None);
        log.append(&Message::user("dropped by the rollback"), None);
        log.append_title(Some("Renamed before the abort"));
        // Roll back to just the first message, the way an interrupt does.
        log.rewrite(&[keep]);
        drop(log);

        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 1, "the rollback really did drop the second message");
        assert_eq!(s.list(10).pop().unwrap().title, "Renamed before the abort", "the rename survived");
    }
}
