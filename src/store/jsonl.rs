//! The JSONL backend: one file per conversation, all of them under
//! `<app_root>/.aox/conversations/`, with the filename being the conversation's timestamp:
//!
//! ```text
//! 20261002-153912-a1b2.jsonl
//! ```
//!
//! **One store, one directory** (since 2026-11-05). Conversations used to be scattered across
//! `<workspace>/.aox/history` for every registered workspace, which meant the file's location
//! doubled as the sandbox boundary: pick a project as the working directory and its history landed
//! inside the project. Now the folder is fixed and "which workspace / which room" lives in the
//! header, where it belongs (see `crate::history::Meta`).
//!
//! The content is JSONL with one record per line, and the **first line is the meta**:
//!
//! ```text
//! {"created":1759390752,"id":"20261002-153912-a1b2","kind":"meta","model":"deepseek-chat"}
//! {"content":"Add history-record support to the project","kind":"message","role":"user"}
//! {"content":null,"kind":"message","role":"assistant","tool_calls":[...]}
//! {"content":"...","is_error":false,"kind":"message","name":"fs_ls","role":"tool"}
//! {"act":{"at":1.7,"end":2.0,"kind":"tool","title":"fs_ls"},"kind":"act"}
//! {"kind":"usage","round":1,"at":1759390999,"calls":3,"prompt":384,"total":1152}
//! {"kind":"title","title":"Refactor the activity-stream window"}
//! ```
//!
//! Besides messages, the same file also appends **activity-stream** records (`kind:"act"`),
//! **token-usage** records (`kind:"usage"`) and the **renamed title** (`kind:"title"`, written only
//! when the user renames the conversation): every colored block on the timeline is one row, one
//! round's token ledger is one row too, and a rename is one more row.
//!
//! The `title` row is **appended, never patched into the meta**: this format only ever appends (see
//! below), so the newest title wins and an absent/empty one means "back to the automatic title" (the
//! first user message) — the same undo the space registry uses for a workspace whose name is cleared.
//! The record sits *inside* the conversation file because the title **belongs to this conversation**:
//! deleting or moving it takes the title along and leaves no orphan behind (the same reasoning as
//! `workplace`).
//!
//! Keeping all of them in one file is **deliberate**: they describe the same thing (how this round ran
//! and how much it cost), and "deleting a conversation" must delete its timeline, ledger and title
//! along with it — sharing one file makes that automatic, with no extra cleanup code.
//!
//! The activity-stream payload is nested under the `act` key rather than flattened: `Act` itself has a
//! `kind` field (input / model / tool …), and if it sat on the same level as the row type it would
//! overwrite it, so on read you couldn't tell which kind of row it was.
//!
//! Why JSONL rather than a single JSON array:
//!   - Appending is O(1). A long task may have hundreds of messages; rewriting the whole file every
//!     round is both slow and easy to corrupt.
//!   - When the process is force-killed, the rows already written stay intact and can still be read.
//!   - A single corrupted line affects only that line, not the whole-file parse.
//!
//! The message body reuses `Message` directly (the OpenAI wire format), so this file is both a
//! human-readable record and a natively replayable message stream — adding "resume conversation" later
//! needs no format change.
//!
//! **This file was moved here in phase 3 from `src/history.rs`**: the logic was carried over function
//! by function as-is, only folding the "free functions + `dir` / `path` parameters" into struct methods
//! (`JsonlStore` / `JsonlSession`). The file format, sort order, and rollback semantics were not touched
//! in a single line.

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
///
/// Named `conversations` rather than `history`: it holds every conversation of this install, and the
/// old name is what made "history" sound like it belonged to a workspace.
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

/// Parse the display time (the time this conversation **started**) out of the filename stem
/// `YYYYMMDD-HHMMSS`.
///
/// The time is parsed straight from the filename, with no timezone conversion at all — the filename
/// is local time to begin with, which is far simpler than storing a UTC timestamp and working out how
/// to convert it back, and it can't be computed wrong. It is now only a fallback for when the file
/// mtime is unavailable (see `display_mtime`).
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

/// System time → local time.
///
/// On Windows we let `FileTimeToLocalFileTime` compute the timezone and DST; deriving the offset from
/// "current local time − current UTC" would be off by an hour during the DST switch hour. Non-Windows
/// platforms have no equivalent, so they fall back to UTC (off by one timezone on display, not worth a
/// dependency for).
#[cfg(windows)]
fn local_from_system(t: SystemTime) -> Option<Local> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    use windows_sys::Win32::System::Time::FileTimeToSystemTime;

    // Unix seconds → FILETIME: unit 100ns, epoch 1601-01-01
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

/// File mtime → the "last conversation time" display string.
///
/// The list is sorted by last conversation time, so what we display must be that same time — otherwise a
/// conversation from three days ago gets pushed to the top while still showing "three days ago" beside it,
/// which looks like a sort bug.
fn display_mtime(t: SystemTime, this_year: u16) -> Option<String> {
    let l = local_from_system(t)?;
    Some(display_parts(l.year, l.month, l.day, l.hour, l.minute, this_year))
}

// ─────────────────────────── Row formats ───────────────────────────

/// The type tag of a row.
///
/// Deliberately **not** using serde's internally tagged enum (`#[serde(tag = "kind")]`) + `flatten`:
/// that combination has subtle pitfalls around "enum + flattened struct + unknown fields", whereas what
/// we want here is "a file written today still reads back in ten years". So we hand-build a `Value`: add a
/// `kind` key, and take the rest of the fields verbatim from `Message` — extra keys are ignored by serde on
/// read-back.
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
        // An attachment's **payload (base64) never goes into the file** — one screenshot is several
        // MB, and this is an append-per-entry conversation log. The body text and the attachment
        // listing stay as they are, so "what was sent at the time" is still recoverable.
        // See `attach::strip_payloads` for the full reasoning.
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

/// One task-checkpoint row. The payload is nested under `task` (like `act`) because the checkpoint
/// carries its own `version` field and its own vocabulary — keeping the row type at the top level
/// means a reader can tell "which kind of row is this" without knowing anything about the payload.
///
/// Shape decided by [`crate::task::TaskState::to_value`]; the store only adds the row type.
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

/// One rename row. `None` writes `{"kind":"title"}` with **no** `title` key at all.
///
/// The absence of the key is the message ("show the automatic title again"), which is why the key is
/// omitted rather than written as `""`: a reader then cannot mistake an empty rename for a real one
/// whose title happens to be blank, and hand-editing the file to drop the key does the obvious thing.
fn title_line(title: Option<&str>) -> Value {
    let mut v = json!({ "kind": "title" });
    if let Some(t) = title {
        v["title"] = json!(t);
    }
    v
}

/// The row types we write out. Any other row (another file mistakenly dropped into `history/`, a line
/// broken by hand-editing) is rejected outright — which is also the cheapest evidence that "this file
/// is ours".
///
/// ⚠️ Adding a kind here is only half the work: [`JsonlSession::rewrite`] pulls each non-message kind
/// out and writes it back, so a kind missing from there would be **silently erased by the first
/// interrupt rollback**. `task` was added to both places together.
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
    // One write, rather than "write the body, then write the newline": one line either lands whole
    // or not at all, otherwise a mid-way error leaves a half-line record — which gets discarded as a
    // bad line on read, meaning that entry was written for nothing.
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
///
/// The "id ↔ filename" mapping is used in four places (create, collision check, directory scan,
/// switching back by id), so it's kept in this one form — a mismatch between the two shows up as
/// "can't find the conversation I just saved" rather than a compile error. **This is an internal
/// detail of the jsonl backend, not exposed to the outside.**
fn file_of(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.{EXT}"))
}

/// Where a workspace's history directory sits: `<workspace>/.aox/history/`.
///
/// Layout decision of 2026-10-14 — history travels with the workspace instead of every conversation
/// piling up in the install directory. The gain is that one workspace folder becomes self-contained
/// (copy it and its conversations come along); the price is that listing has to visit every
/// workspace, see [`JsonlStore::dirs`].
///
/// `.aox` is the existing "AOX's own per-workspace data" convention (see [`crate::workplace::AOX_DIR`]),
/// which already holds `memory/`; history simply joins it. One hidden folder rather than several
/// visible ones means a user's project directory gains exactly one entry from AOX.
fn dir_of(root: &Path) -> PathBuf {
    root.join(crate::workplace::AOX_DIR).join(DIR_NAME)
}

// ─────────────────────────── Backend ───────────────────────────

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
    ///
    /// Ids are globally unique (a timestamp plus a random salt). `exists()` rather than trying to
    /// open: opening a session takes an exclusive-ish handle, and this is also called on read paths
    /// that must not leave a file open.
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

    /// `keep` is fixed at construction time (from `config.toml [history] keep`).
    ///
    /// Pruning happens **before** the new file is written: trimming after creation would make the
    /// newcomer a candidate for its own trim.
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
        // "The last one" = this conversation's current workspace.
        self.find(id).map(|p| load_workplaces(&p)).unwrap_or_default().pop()
    }

    fn load_task(&self, id: &str) -> Option<Value> {
        // "The last one" = the checkpoint as it currently stands; earlier rows are the same task at
        // earlier moments, which nobody reads.
        self.find(id).map(|p| load_tasks(&p)).unwrap_or_default().pop()
    }

    fn remove(&self, key: &str) -> Result<Summary> {
        // Deletion used to be scattered in `main.rs` (a direct `std::fs::remove_file`) — folding it
        // into the backend, otherwise switching backends (say a library holding all conversations)
        // would have no "delete" semantics at all.
        //
        // ⚠️ This is a **plain, permanent** delete. The GUI deliberately does not use it: sending a
        // conversation to the system recycle bin is `crate::recycle`'s job, and that has to happen on
        // a thread that is allowed to block. Kept as-is because the CLI's `/history rm` has always
        // meant "gone".
        let s = resolve_any(&self.home, key)?;
        std::fs::remove_file(&s.path)
            .with_context(|| crate::tf!("Cannot delete {}", s.path.display()))?;
        Ok(s)
    }

    fn prune(&self, keep: usize) -> usize {
        prune_all(&self.home, keep)
    }
}

// ─────────────────────────── Writing ───────────────────────────

/// A conversation being written (formerly `history::Log`).
pub struct JsonlSession {
    path: PathBuf,
    file: File,
    meta: Meta,
}

impl JsonlSession {
    /// Create a new conversation file and write the meta row.
    ///
    /// No pruning here: the limit is the backend's call (it is the one that can see the whole
    /// directory), and the caller prunes before creating, which also keeps the new file out of the
    /// reckoning.
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
    ///
    /// Unlike `create`: no new file, no new id, no pruning. The meta row is read back from the file —
    /// the key is to keep **the original id**, otherwise "continue this conversation" would write a second
    /// file and the history would show two parallel records.
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
    ///
    /// Like messages, a write failure returns no error: the timeline is "what the process looked
    /// like", and losing it costs at most one record — it shouldn't break a running conversation.
    fn append_act(&mut self, act: &Value) {
        let _ = write_line_to(&mut self.file, &act_line(act));
    }

    /// Append one round's token ledger.
    ///
    /// Like messages and the activity stream, a write failure returns no error: losing one line of
    /// statistics shouldn't break a running conversation.
    fn append_usage(&mut self, rec: &UsageRecord) {
        let _ = write_line_to(&mut self.file, &usage_line(rec));
    }

    /// Append one "workplace for this conversation" record. Same as above: write failures are silent.
    ///
    /// It is written inside the conversation file (rather than a separate side file) because it **belongs
    /// to this conversation**: when the conversation is deleted, pruned, or moved to another backend, this
    /// record follows naturally and leaves no orphan.
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
    ///
    /// Used for "interruption rollback": the in-memory messages were discarded, so the file must return to
    /// the same state, otherwise it leaves a half-written record "with tool_calls but no result"
    /// (when resuming this conversation later, such a record would get the request rejected outright).
    ///
    /// The activity stream (`kind:"act"`), token usage (`kind:"usage"`), workplace
    /// (`kind:"workplace"`), the rename (`kind:"title"`) and the task checkpoint (`kind:"task"`) are
    /// not part of the message stream, so **they must be pulled out first and written back
    /// verbatim**: a rollback only loses messages, and there is no reason to also wipe the timeline's
    /// "process that already happened", the ledger already recorded, "which plot this conversation
    /// was working in", what the user decided to call it — or the checkpoint that says what the task
    /// was even about.
    ///
    /// The checkpoint is the one exception to "verbatim": only the **last** row is written back.
    /// Earlier rows are the same task at earlier moments and nothing reads them, so carrying them
    /// through a rollback would just re-inflate the file with history nobody will ever look at.
    ///
    /// Implementation-wise we **must not** call `set_len` directly on the append handle: on Windows a handle
    /// opened for append has no truncate permission and returns `os error 5 (access denied)`; moreover
    /// `O_APPEND` makes `seek` a no-op, so continued writes still land after the old content (both verified
    /// empirically). So we open a separate truncating handle, rewrite the whole file, then swap the append
    /// handle back.
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

// ─────────────────────────── Reading ───────────────────────────

/// One history file in the directory.
struct Entry {
    id: String,
    path: PathBuf,
    /// The time of the last write (file mtime), i.e. the "last conversation time"
    mtime: Option<SystemTime>,
}

/// Scan the directory, **ordered from newest to oldest by last conversation time**.
///
/// mtime rather than the timestamp in the filename: the filename records "when this conversation
/// started", whereas the list has to answer "which conversation was I most recently talking to".
/// Continuing a three-day-old conversation should float it to the top — which is exactly the order
/// users expect when looking for "what was I just talking about".
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
    // When mtimes are equal (two files created in the same second) fall back to the filename: the
    // filename starts with a timestamp, so lexicographic order is time order. This fallback makes the
    // order deterministic in every case.
    files.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.id.cmp(&a.id)));
    files
}

/// List history conversations across **every workspace**, most recently talked-to first.
///
/// The title comes from just the first few lines (the first user message is always line 2), and the
/// count is obtained by counting lines — no full parse, so even a few hundred files is just a
/// sequential disk read.
///
/// Merging N directories means the per-directory order can no longer be trusted, so it is re-sorted
/// here with the **same tie-break `collect` uses** (the filename, which opens with a timestamp). If
/// the two ever disagreed, the list would look randomly re-ordered between refreshes.
///
/// `limit` is applied **after** the merge, not per directory: it caps the list the user sees, not how
/// many conversations each workspace may contribute.
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

    // Line 1 is the meta (id / model / version / origin / space / room): consumed here, and it
    // doubles as "this really is our format". `origin`/`space`/`room` are read from the same line
    // so grouping needs no second source — a `spaces.json` that lost the entry still gets it right.
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
    // The workspace this conversation is bound to: the **last** `kind:"workplace"` record wins
    // (switching tier appends another one, and readers take the last). Harvested on this same walk
    // rather than in a second pass — the file is being read line by line for the title and the
    // message count anyway, and a second pass would double the list's disk cost.
    let mut ws: Option<crate::workplace::Record> = None;
    // What the user last renamed this conversation to. The **outer** `Option` is "has it ever been
    // renamed at all" and the inner one is the last record's own value — they are not the same
    // question, and folding them into one `Option<String>` would make "renamed back to automatic"
    // (a row with no `title` key) indistinguishable from "never renamed", i.e. the automatic title
    // would refuse to come back. Same single-walk, last-one-wins treatment as `ws` above.
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
        // The activity stream (`kind:"act"`) and usage (`kind:"usage"`) are neither counted in "message
        // count" nor used as a title — otherwise the sidebar count would be inflated, and the first round's
        // pile of blocks would eat up `TITLE_SCAN_LINES`'s budget and the title couldn't be found.
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

/// Read out all **activity-stream** records in a file (in write order), returning the payloads
/// verbatim.
///
/// What to parse them into is the caller's decision: this is display data for the UI, and the storage
/// layer shouldn't know about concepts like "colored block" or "round" — it just moves the rows out.
/// Bad lines and unknown rows are skipped.
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

/// Read out all **token usage** records in a file (in write order, i.e. round order).
///
/// Opening an old conversation relies on this to reconnect the ledger — otherwise, after one restart,
/// the usage of all earlier rounds would vanish into thin air. Bad lines are skipped, and any row other
/// than `kind:"usage"` is rejected outright.
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

/// Read all **workplace** records in a file (in write order) — the caller takes the last one == the current
/// plot.
///
/// Like `load_usage`: bad lines are skipped; any row other than `kind:"workplace"` is rejected; and a row
/// whose internals aren't recognizable (missing `mode`) is skipped too.
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

/// Read all `kind:"task"` payloads in a file, in write order — the caller takes the last one (the
/// checkpoint as it currently stands).
///
/// The payload is handed back **unparsed**: whether it is a valid checkpoint, and what a checkpoint
/// even looks like, is `crate::task`'s business. That keeps the store from growing a second opinion
/// about the schema.
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

/// Read all `kind:"title"` rows in a file, in write order — the caller takes the last one == what the
/// conversation is currently called (`None` inside = the row that cleared the rename).
///
/// Kept for one reason: `rewrite` (interrupt rollback) only loses **messages**, so the renames have to
/// be pulled out and written back verbatim, exactly like the activity stream, the ledger and the
/// workplace. Without this a rollback would silently undo a rename.
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

/// Keep only the newest `keep` conversations **across every workspace**, deleting the rest.
/// `keep == 0` means no limit.
///
/// "Newest" == last conversation time, so pruning drops the **least recently talked-to** ones —
/// an old conversation just resumed is kept, even if it was opened months ago.
///
/// The limit is global on purpose: the user configured one number, and letting each workspace keep
/// that many would quietly multiply it by the number of workspaces.
/// Returns the number of deleted files.
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
///
/// The index comes from `list()`'s display order (the newest is 1) — which, since history spans
/// several workspaces, is the order of the **merged** list, not of any one directory. Both front ends
/// show that same merged order, so "number 3" means the same conversation in the GUI and in
/// `/history rm 3`; if they ever disagreed, the CLI would delete something the user never saw.
///
/// Both forms are supported — typing a timestamp by hand is a nuisance, and after seeing the list it's
/// easiest to just write the index.
///
/// The "pure digits" ambiguity needs separate handling: ids start with an 8-digit date, so `20261003`
/// looks both like an index and like an id prefix. The criterion is length — an index can't have 8
/// digits (the list cap is 60, and even lifted it wouldn't reach 8), so **pure digits of 8 or more are
/// always treated as an id prefix**. Without this split, `/history rm 20261003` would become "there is
/// no conversation number 20261003".
fn resolve_any(dir: &Path, key: &str) -> Result<Summary> {
    let key = key.trim().trim_end_matches(&format!(".{EXT}"));
    // An empty key must be blocked here: `id.starts_with("")` is always true, so letting it through
    // would "successfully resolve" to the list's first entry — which is arbitrary. Better to error
    // explicitly.
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

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FunctionCall, ToolCall};

    /// A temp directory for tests. **Deleted on drop** — it used to return only a PathBuf, and every
    /// `cargo test` run left seven empty directories in %TEMP%.
    ///
    /// It plays the role of the **app root**: history lands in `<tmp>/history/`, matching the real
    /// layout (which is exactly the point of "the backend manages its own layout": the tests don't have
    /// to know what the filenames look like either).
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

    /// A message with an attachment must have its **payload (base64) stripped** when persisted, while
    /// the body and the attachment listing stay.
    ///
    /// This is the only insurance against "the conversation file gets fattened by several MB by one
    /// screenshot" — and it could easily be quietly dropped in some future "route everything through
    /// Message serialization" refactor, so it is pinned on the **file's actual content**, not on the
    /// in-memory message.
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
        // The directory is asked of the backend (`location()`), not hand-joined as `<root>/history` —
        // which is exactly the point this test incidentally pins: switching backends changes it here
        // too. The filename isn't hand-joined either; scan the directory and take the only one.
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

    /// Whatever goes in should come back out as the same messages, in the same order, with the same
    /// is_error. This is the foundation of the whole feature: the file mixes non-wire fields like `kind`
    /// / `is_error`, and when reading back into `Message` they must be ignored rather than making the
    /// parse fail. One fixture also carries CJK on purpose, so the UTF-8/JSON round-trip is covered.
    #[test]
    fn round_trip_messages() {
        let d = Tmp::new("rt");
        let s = d.store();
        let mut log = s.create(Some("deepseek-chat"), &Default::default()).unwrap();
        let id = log.id().to_string();

        log.append(&Message::user("第一行\n第二行 \"引号\" 与中文"), None);
        log.append(&Message::assistant(None, Some("Let me have a look".into()), vec![call("c1", "fs_ls", r#"{"path":"."}"#)]), None);
        log.append(&Message::tool_result("c1", "fs_ls", "listed 3 items".into()), Some(false));
        log.append(&Message::assistant(None, None, vec![call("c2", "fs_read", r#"{"path":"nope"}"#)]), None);
        log.append(&Message::tool_result("c2", "fs_read", "file does not exist".into()), Some(true));
        log.append(&Message::assistant(Some("Done.".into()), None, vec![]), None);
        drop(log);

        let items = s.load(&id).unwrap();
        assert_eq!(items.len(), 6, "system messages are not stored, so all 6 messages should be read back");
        assert_eq!(items[0].msg.role, "user");
        assert_eq!(items[0].msg.content.as_ref().and_then(|v| v.as_str()), Some("第一行\n第二行 \"引号\" 与中文"));
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
        assert_eq!(list[0].title, "第一行");
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
        // system isn't stored → the file has 1 user message + 2 rolled-back ones
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

    /// A host-injected prompt shouldn't be used as a title.
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

    /// The retention policy: keep only the newest `keep`.
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

    /// The activity stream and messages share one file without interfering with each other:
    /// - `load` yields only messages, `load_acts` only activity-stream records;
    /// - the list's "count" counts only messages (colored blocks mustn't inflate the sidebar count);
    /// - **an interruption rollback doesn't drag the activity stream along**: what a rollback loses is
    ///   messages, not "the process that already happened".
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

    /// "Continue where we left off" must write back to the **original file**, not start another one.
    ///
    /// This is the easiest mistake to make when switching sessions: the id is read back from meta, and
    /// once it's read wrong (or simply regenerated), two parallel records appear in the history and the
    /// user thinks messages were lost.
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

    /// A file whose first line isn't meta (hand-edited / written by an early version) must be
    /// resumable too: `open` falls back to using the filename as the id, and messages are still
    /// appended to the end of the original file rather than starting a new one.
    ///
    /// Such files are **invisible** in the list (`list` uses the first-line meta as a format check,
    /// so other files mistakenly dropped into `history/` don't get listed), so only the write
    /// behaviour is asserted here.
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

        // The earlier-named one is written later: this way "sort by filename" and "sort by mtime"
        // reach opposite conclusions, so the assertion can tell which sort is in effect. (NTFS mtime
        // precision is 100ns, so 30ms is plenty to separate them; if the filesystem's granularity
        // were as coarse as a second this test would be unreliable.)
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

    /// Workplace records: **only the last one counts**, it must survive "interruption rollback", and
    /// `own` (this conversation's self-built plot) **must not be lost** after switching to another
    /// tier — losing it means "switch away and back to default" builds another one, and `workplace/`
    /// gets piled up with empty directories.
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

    /// A checkpoint is a **conversation record**, not a sidecar file: it must survive an interruption
    /// rollback (messages vanish; the checkpoint stays) and only the **last** row is carried through —
    /// earlier rows are the same task at earlier moments and nothing ever reads them.
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

    /// If the endpoint omits one item, reading it back should give `None` — it must not become 0.
    /// "Nothing was consumed" and "the other side didn't report it" are two different things, and
    /// conflating them makes the ledger lie.
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

    /// `remove` absorbed the deletion action that used to be scattered in `main.rs`: it deletes, the
    /// list shrinks by one afterwards, and it works by index or by id. This is the precondition for
    /// "switching backends can still implement deletion".
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

    // ─────────────── 单一会话库（2026-11-05） ───────────────
    //
    // 这里原本有六条“按工作区分散存储”的测试：会话落在 `<workspace>/.aox/history`、列表合并多个
    // 目录、resolve/open/remove/prune 跨目录可达。那套设计已经取消——所有会话都在
    // `<app root>/.aox/conversations`，文件位置不再跟着工作目录走（“哪个工作区”写进首行 meta）。
    // 对应的行为没有了，测试跟着删掉，而不是改成断言“还是只有一个目录”。

    /// A rename beats the title derived from the first user message — and clearing it hands the row
    /// back to that first message instead of leaving it blank or stuck on the old name.
    ///
    /// The two halves are one test on purpose: they are the same rule read in both directions, and the
    /// bug worth preventing (they get out of step when one of them is "the last record wins" and the
    /// other is not) only shows up as a comparison between them.
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

    /// An interrupt rollback rewrites the file from the message list — and must **not** throw the
    /// rename away with it. A rollback only loses messages (see `Session::rewrite`); a conversation
    /// silently going back to its old name after an abort would be exactly the kind of loss nobody
    /// connects to the abort.
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
