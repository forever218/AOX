//! The **saved model list** — one record per model the user has set up, plus the little bit of state
//! each one needs.
//!
//! Three things this file is, in order of importance:
//!
//! 1. **A list of models, not a list of endpoints.** Earlier (2026-10-27 morning) the unit here was
//!    "an endpoint with a key and a scan result". That was the wrong grain: one endpoint serves many
//!    models, and people switch *models*, not hosts. Each record now carries its own endpoint, key,
//!    model name and per-model parameters, so two records may point at the same host with different
//!    keys and different windows.
//! 2. **Convenience data, not configuration.** `config.toml` stays the single statement of **what is
//!    running right now** (that is what the CLI reads and what a hand edit is checked against). This
//!    file is what the GUI remembers so the list is there next launch; activating a record is a copy
//!    of its values into `config.toml`, exactly as if they had been typed there.
//! 3. **Never fatal.** Missing / unreadable / wrong-version → an empty list. A broken book must cost
//!    the user their saved list, never their session.
//!
//! The file is `<app root>/.aox/models.json`, beside `ui.json` / `spaces.json`: it describes this
//! installation's models, not any one workspace. It is written atomically (temp + rename), because the
//! GUI can be killed at any moment and a half-written file would take every record with it.
//!
//! ⚠️ It holds API keys in clear text, like `config.toml` does. Nothing here may be logged.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use crate::config::{ProviderConfig, Thinking, ThinkingWire};
use crate::config::{DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_TOKENS};
use crate::workplace::AOX_DIR;

const FILE_NAME: &str = "models.json";
/// The file this one grew out of (2026-10-27 morning). Read once for migration, never written again.
const LEGACY_FILE: &str = "endpoints.json";
/// On-disk layout version — **rejected, not guessed at**, when it does not match.
///
/// 2 = the book is the model source of truth (`active` names the record in force). Version 1 is
/// still read, with `active` empty: it costs three lines and keeps this install's list, which is
/// the only copy of it.
const VERSION: u32 = 2;

/// One saved model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRecord {
    /// Stable identity, independent of the display name and of the wire model name (both editable).
    pub id: String,
    /// What the list shows. Defaults to the model name, and is editable.
    pub name: String,
    pub base_url: String,
    /// In clear text. This file is the **only** place a key lives now, so it leaves here only to go
    /// into a `Bearer` header.
    #[serde(default)]
    pub api_key: String,
    /// Name of an environment variable that holds the key, for people who would rather not have it on
    /// disk at all. When set it wins; the field above is then a leftover from an earlier edit.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// The name sent to the endpoint (`deepseek-flash`, `qwen3:8b`, …).
    pub model: String,
    /// Whether it appears in the input box's list. Disabling is for "keep the record, stop showing it",
    /// not for deleting it.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-model, because it is a property of the model and not of the app: see `ProviderConfig`.
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub thinking: Thinking,
    #[serde(default)]
    pub thinking_wire: ThinkingWire,
    /// Request by SSE streaming. A property of the endpoint, so it is per record: a gateway that
    /// rejects `stream_options` needs it off, and that says nothing about the next one.
    #[serde(default = "default_true")]
    pub stream: bool,
    /// The last scan of this record's endpoint: model ids and the windows the endpoint reported.
    #[serde(default)]
    pub scanned: Vec<SavedModel>,
    #[serde(default)]
    pub scanned_url: Option<String>,
    #[serde(default)]
    pub scanned_at: i64,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedModel {
    pub id: String,
    #[serde(default)]
    pub window: Option<u32>,
}

/// The whole list, and **which one is running**.
///
/// Since 2026-10-28 this file is the program's model configuration, not a GUI convenience copy:
/// `config.toml` no longer carries `[provider]`, the CLI and the GUI both read the active record
/// here, and "switch model" is one write to this file. `active` is an id rather than an
/// (endpoint, model) pair because a pair can match two records — which is how two rows once both
/// claimed to be in use.
#[derive(Debug, Default, Clone)]
pub struct Book {
    /// [`ModelRecord::id`] of the record in force. `None` = nothing has been chosen yet (a fresh
    /// install, or a book written by version 1).
    pub active: Option<String>,
    pub models: Vec<ModelRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Wire {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    models: Vec<ModelRecord>,
}

impl ModelRecord {
    /// A blank record with the defaults a new model should start from.
    pub fn blank(id: String) -> Self {
        Self {
            id,
            name: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            enabled: true,
            context_window: None,
            max_tokens: Some(DEFAULT_MAX_TOKENS),
            temperature: Some(0.3),
            thinking: Thinking::Default,
            thinking_wire: ThinkingWire::Auto,
            stream: true,
            api_key_env: None,
            scanned: Vec::new(),
            scanned_url: None,
            scanned_at: 0,
        }
    }

    /// The line under the name: `model · host`, or just the host when the name **is** the model.
    ///
    /// Built here so the list and the pop-up say the same thing about the same record. The
    /// de-duplication matters: the default name of a saved model is its model name, so without this
    /// every ordinary row read `deepseek-flash` / `deepseek-flash · api.deepseek.com`.
    pub fn subtitle(&self) -> String {
        let host = host_of(&self.base_url);
        let model = if self.model == self.name { "" } else { self.model.as_str() };
        match (model, host.as_str()) {
            ("", "") => String::new(),
            (m, "") => m.to_string(),
            ("", h) => h.to_string(),
            (m, h) => format!("{m} · {h}"),
        }
    }

    /// The window to use when this record is activated: the one it carries, else what the endpoint
    /// reported for this model in the last scan, else nothing (the caller falls back to the config
    /// default and says so in the UI).
    pub fn window(&self) -> Option<u32> {
        self.context_window.or_else(|| {
            self.scanned
                .iter()
                .find(|m| m.id == self.model)
                .and_then(|m| m.window)
        })
    }

    /// This record as the runtime's provider configuration.
    ///
    /// The single conversion point. Before 2026-10-28 the same mapping existed three times (the
    /// bridge's `edit_to_config`, the dialogs, and `config.toml` itself); with the book as the only
    /// source it lives here, and "what runs" cannot drift from "what is saved".
    pub fn to_config(&self) -> ProviderConfig {
        ProviderConfig {
            base_url: self.base_url.trim().to_string(),
            model: self.model.trim().to_string(),
            api_key: Some(self.api_key.clone()).filter(|k| !k.trim().is_empty()),
            api_key_env: self.api_key_env.clone().filter(|e| !e.trim().is_empty()),
            max_tokens: self.max_tokens.or(Some(DEFAULT_MAX_TOKENS)),
            temperature: self.temperature,
            stream: self.stream,
            context_window: self.window().or(Some(DEFAULT_CONTEXT_WINDOW)),
            thinking: self.thinking,
            thinking_wire: self.thinking_wire,
        }
    }

    /// Whether this record is the one currently in force, given what the runtime reports.
    ///
    /// Matched on **(endpoint, model)** and not on the id: the id is a GUI invention the runtime has
    /// never heard of, `config.toml` may have been edited by hand, and the same model may legitimately
    /// be saved twice against two endpoints.
    pub fn is_active(&self, base_url: &str, model: &str) -> bool {
        self.model == model && norm(&self.base_url) == norm(base_url)
    }
}

impl Book {
    pub fn load(app_root: &Path) -> Self {
        if let Some(b) = Self::load_models(app_root) {
            return b;
        }
        // One-time pickup of the earlier layout. It is read, converted, and the next save writes the
        // new file — the old one is left alone (it costs 300 bytes and is the only copy of anything
        // that did not convert).
        Self::migrate_from_endpoints(app_root).unwrap_or_default()
    }

    fn load_models(app_root: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(file_path(app_root)).ok()?;
        let wire = serde_json::from_str::<Wire>(&text).ok()?;
        if wire.version != VERSION && wire.version != 1 {
            return None;
        }
        Some(Self { active: wire.active, models: wire.models })
    }

    fn migrate_from_endpoints(app_root: &Path) -> Option<Self> {
        #[derive(Deserialize)]
        struct OldModel {
            id: String,
            #[serde(default)]
            window: Option<u32>,
        }
        #[derive(Deserialize)]
        struct OldEndpoint {
            name: String,
            base_url: String,
            #[serde(default)]
            api_key: String,
            #[serde(default)]
            models: Vec<OldModel>,
            #[serde(default)]
            models_url: Option<String>,
            #[serde(default)]
            scanned_at: i64,
        }
        #[derive(Deserialize)]
        struct OldWire {
            #[serde(default)]
            endpoints: Vec<OldEndpoint>,
        }

        let text = std::fs::read_to_string(app_root.join(AOX_DIR).join(LEGACY_FILE)).ok()?;
        let old: OldWire = serde_json::from_str(&text).ok()?;
        let mut book = Book::default();
        for (i, e) in old.endpoints.into_iter().enumerate() {
            // One record per endpoint, carrying the first model the scan found (or none: the record
            // still holds the endpoint and key, which is the part that was expensive to type).
            let mut rec = ModelRecord::blank(format!("m{}", i + 1));
            rec.name = e.name;
            rec.base_url = e.base_url;
            rec.api_key = e.api_key;
            rec.scanned =
                e.models.iter().map(|m| SavedModel { id: m.id.clone(), window: m.window }).collect();
            if let Some(first) = e.models.first() {
                rec.model = first.id.clone();
                rec.context_window = first.window;
            }
            rec.scanned_url = e.models_url;
            rec.scanned_at = e.scanned_at;
            book.models.push(rec);
        }
        Some(book)
    }

    pub fn save(&self, app_root: &Path) -> anyhow::Result<()> {
        let dir = app_root.join(AOX_DIR);
        std::fs::create_dir_all(&dir)?;
        let wire = Wire { version: VERSION, active: self.active.clone(), models: self.models.clone() };
        let tmp = dir.join(format!("{FILE_NAME}.tmp"));
        std::fs::write(&tmp, serde_json::to_string_pretty(&wire)?)?;
        std::fs::rename(&tmp, file_path(app_root))?;
        Ok(())
    }

    /// The record in force, resolved through [`Book::active`].
    pub fn active_record(&self) -> Option<&ModelRecord> {
        self.active.as_ref().and_then(|id| self.by_id(id))
    }

    /// The runtime's configuration: the record in force, or `None` when nothing has been chosen.
    ///
    /// Every front end goes through here — that is the whole point of the book being in the library
    /// rather than in the GUI. `Runtime::new` falls back to the layered `[provider]` when this is
    /// `None`, which is what a fresh install starts from.
    pub fn active_config(&self) -> Option<ProviderConfig> {
        self.active_record().map(ModelRecord::to_config)
    }

    /// Put a record in force by id. `false` when there is no such record.
    pub fn set_active(&mut self, id: &str) -> bool {
        if self.by_id(id).is_none() {
            return false;
        }
        self.active = Some(id.to_string());
        true
    }

    /// Seed the book from a `[provider]` still sitting in `config.toml`, and put it in force.
    ///
    /// Runs once, at startup. A **no-op as soon as there is an active record**: the book wins, which
    /// is the entire contract of this file. The key is copied on purpose — the book is the only place
    /// a key lives now, and a seeded record without one is a model that cannot answer, which reads as
    /// a regression rather than as an invitation to retype it.
    ///
    /// Returns whether anything changed (and therefore whether the file was rewritten).
    pub fn adopt(&mut self, cfg: &ProviderConfig, app_root: &Path) -> bool {
        if self.active.is_some() {
            return false;
        }
        if cfg.base_url.trim().is_empty() || cfg.model.trim().is_empty() {
            return false;
        }
        self.remember(cfg);
        let _ = self.save(app_root);
        true
    }

    /// Make `cfg` the record in force, creating it if this (endpoint, model) is new. Returns its id.
    ///
    /// The one place "the thing that runs" is folded back into the list — used by `adopt` at startup
    /// and by the GUI after a successful apply. Two rules it keeps:
    ///
    /// * a **stored key is never replaced by an empty one**. The dialogs show a blank key field to
    ///   mean "unchanged", so an edit arrives here with no key at all;
    /// * `enabled` is left alone. A record can be hidden from the picker while still running (see
    ///   `ModelUi::toggle_enabled`); re-applying it must not silently un-hide it.
    pub fn remember(&mut self, cfg: &ProviderConfig) -> String {
        let id = match self.find_same(&cfg.base_url, &cfg.model) {
            Some(r) => r.id.clone(),
            None => {
                let mut r = ModelRecord::blank(self.next_id());
                r.name = cfg.model.trim().to_string();
                let id = r.id.clone();
                self.upsert(r);
                id
            }
        };
        if let Some(r) = self.by_id_mut(&id) {
            r.base_url = cfg.base_url.trim().to_string();
            r.model = cfg.model.trim().to_string();
            if let Some(k) = cfg.api_key.clone().filter(|k| !k.trim().is_empty()) {
                r.api_key = k;
                // An inline key wins in `ProviderConfig::resolve_key`, so a variable left in place
                // would be a second source that no longer does anything.
                r.api_key_env = None;
            } else if r.api_key_env.is_none() {
                r.api_key_env = cfg.api_key_env.clone();
            }
            if let Some(w) = cfg.context_window {
                r.context_window = Some(w);
            }
            if let Some(m) = cfg.max_tokens {
                r.max_tokens = Some(m);
            }
            r.temperature = cfg.temperature;
            r.thinking = cfg.thinking;
            r.thinking_wire = cfg.thinking_wire;
            r.stream = cfg.stream;
        }
        self.active = Some(id.clone());
        id
    }

    pub fn by_id(&self, id: &str) -> Option<&ModelRecord> {
        self.models.iter().find(|m| m.id == id)
    }

    pub fn by_id_mut(&mut self, id: &str) -> Option<&mut ModelRecord> {
        self.models.iter_mut().find(|m| m.id == id)
    }

    /// The record that matches what the runtime reports as in force.
    ///
    /// **One**, even when two records tie on (endpoint, model) — which happens for real: an install
    /// that upgraded into this layout can carry a migrated record *and* one adopted later, and marking
    /// both "in use" is what "why are two models shown as current" was.
    pub fn active(&self, base_url: &str, model: &str) -> Option<&ModelRecord> {
        self.models.iter().find(|m| m.is_active(base_url, model))
    }

    /// The id of [`Book::active`], for callers that compare rows rather than hold references.
    pub fn active_id(&self, base_url: &str, model: &str) -> Option<&str> {
        self.active(base_url, model).map(|m| m.id.as_str())
    }

    /// The record already saved for this (endpoint, model), if any.
    ///
    /// "Add model" twice with the same endpoint and model is a duplicate the user cannot tell apart
    /// from an edit — two identical rows, one of them dead weight. Editing the existing record is what
    /// pressing "Save" in that situation means.
    pub fn find_same(&self, base_url: &str, model: &str) -> Option<&ModelRecord> {
        self.active(base_url, model)
    }

    /// The records the input box's list shows.
    pub fn enabled(&self) -> impl Iterator<Item = &ModelRecord> {
        self.models.iter().filter(|m| m.enabled)
    }

    /// Insert or replace. Replacing keeps the position, so editing a record does not make it jump to
    /// the end of the list — which would look like it had been re-added.
    pub fn upsert(&mut self, rec: ModelRecord) {
        match self.models.iter().position(|m| m.id == rec.id) {
            Some(i) => self.models[i] = rec,
            None => self.models.push(rec),
        }
    }

    /// Drop a record. When it was the one in force, `active` is cleared with it: a dangling id would
    /// make `active_config()` return `None` for a reason the user cannot see (the record is simply
    /// gone), and "nothing is chosen" is the truth at that point.
    pub fn remove(&mut self, id: &str) {
        self.models.retain(|m| m.id != id);
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
    }

    /// An id no record is using. `m1`, `m2`, … — short, readable in a JSON diff, and stable.
    pub fn next_id(&self) -> String {
        for n in 1.. {
            let id = format!("m{n}");
            if self.by_id(&id).is_none() {
                return id;
            }
        }
        unreachable!("the loop above is infinite")
    }

    /// Store a completed scan on every record pointing at that endpoint.
    ///
    /// Every record, not just one: two records on the same host differ in key and model, and both
    /// benefit from the list. Windows the user set by hand outlive a rescan (most endpoints report
    /// none), so the fresh list wins on membership while a known number is kept.
    pub fn set_scan(&mut self, base_url: &str, models: Vec<SavedModel>, url: Option<String>) {
        let key = norm(base_url);
        for rec in self.models.iter_mut().filter(|m| norm(&m.base_url) == key) {
            let old = rec.scanned.clone();
            rec.scanned = models
                .iter()
                .map(|m| {
                    let mut m = m.clone();
                    if m.window.is_none() {
                        m.window = old.iter().find(|o| o.id == m.id).and_then(|o| o.window);
                    }
                    m
                })
                .collect();
            rec.scanned_url = url.clone();
            rec.scanned_at = now_secs();
        }
    }
}

fn file_path(app_root: &Path) -> PathBuf {
    app_root.join(AOX_DIR).join(FILE_NAME)
}

/// Normalised for **comparison only** (never for display): lower-case, no trailing slashes.
fn norm(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// `https://api.deepseek.com/v1` → `api.deepseek.com`.
pub fn host_of(base_url: &str) -> String {
    let s = base_url.trim();
    let s = s.split_once("://").map_or(s, |(_, rest)| rest);
    let host = s.split('/').next().unwrap_or(s);
    host.to_ascii_lowercase()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ─────────────────────────── `/model`: the CLI's window onto this file ───────────────────────────

/// A window as a person reads it: `1M`, `128K`, `8192`. Rounded to the nearest unit, because a real
/// window is 1048576 or 131072 and printing those raw reads as a serial number.
pub fn window_label(w: u32) -> String {
    if w >= 1_000_000 {
        format!("{}M", (f64::from(w) / 1_000_000.0).round() as u32)
    } else if w >= 1_000 {
        format!("{}K", (f64::from(w) / 1_000.0).round() as u32)
    } else {
        w.to_string()
    }
}

/// Is this line a `/model` command?
pub fn is_cmd(line: &str) -> bool {
    line == "/model" || line.starts_with("/model ")
}

/// `/model` lists the saved models; `/model <n|name>` switches to one.
///
/// The CLI's window onto the same file the GUI's Model tab edits, and the **same switch path**: write
/// `active`, then hand the record to `LiveProvider::apply` (construct-then-swap). No `Runtime` is
/// rebuilt, so the conversation, its history file and the task checkpoint all stay where they are —
/// switching model mid-task is a property change, not a restart.
pub fn command(rt: &crate::runtime::Runtime, line: &str) -> anyhow::Result<String> {
    let mut book = Book::load(&rt.root);
    let arg = line.strip_prefix("/model").unwrap_or("").trim();
    if arg.is_empty() {
        return Ok(list(&book));
    }

    let id = resolve(&book, arg)?;
    let rec = book.by_id(&id).cloned().ok_or_else(|| anyhow::anyhow!("record {id} vanished"))?;
    if book.active.as_deref() == Some(id.as_str()) {
        return Ok(crate::tf!(
            "“{}” is already the model in force — nothing switched.",
            rec.name
        ));
    }
    rt.provider
        .apply(rec.to_config())
        .with_context(|| crate::tf!("“{}” cannot be used", rec.name))?;
    // Only now: an apply that failed must leave the book pointing at the model that is actually
    // answering, or the next launch would start from a configuration that never ran.
    book.set_active(&id);
    book.save(&rt.root)?;
    Ok(crate::tf!(
        "Switched to “{}” ({}) · window {} · the next request uses it.",
        rec.name,
        host_of(&rec.base_url),
        rec.window().map(window_label).unwrap_or_else(|| "—".into())
    ))
}

/// The list, with the record in force marked.
fn list(book: &Book) -> String {
    if book.models.is_empty() {
        return crate::i18n::t(
            "No model saved yet — add one in the GUI (Settings → Model). It is stored in .aox/models.json.",
        )
        .to_string();
    }
    let mut out = crate::i18n::t("Models (.aox/models.json)").to_string();
    for (i, r) in book.models.iter().enumerate() {
        let cur = book.active.as_deref() == Some(r.id.as_str());
        let window = r.window().map(window_label).unwrap_or_else(|| "—".into());
        // `·`-separated rather than padded columns: the names are user text, often Chinese, and
        // `{:<20}` pads by chars, not by display width, so a padded table is crooked half the time.
        // Identity, then the two facts worth comparing. Each part is conditional because otherwise
        // the same string shows up twice: a record whose name is the host read
        // `1. api.deepseek.com · api.deepseek.com · …`.
        let host = host_of(&r.base_url);
        let mut row = format!("\n  {}. {}", i + 1, r.name);
        if !r.model.is_empty() && !r.model.eq_ignore_ascii_case(&r.name) {
            row.push_str(&format!(" · {}", r.model));
        }
        if !host.is_empty() && !r.name.to_lowercase().contains(&host) {
            row.push_str(&format!(" · {host}"));
        }
        row.push_str(&format!(" · {} {}", crate::i18n::t("window"), window));
        out.push_str(&row);
        if cur {
            out.push_str(&format!(" · {}", crate::i18n::t("in force")));
        } else if !r.enabled {
            out.push_str(&format!(" · {}", crate::i18n::t("hidden from the picker")));
        }
    }
    out.push_str(&format!(
        "\n\n{}",
        crate::i18n::t("Switch with /model <number|name>; it takes effect on the next request.")
    ));
    out
}

/// Turn an argument into a record id: 1-based number, then id / name / model name, then a unique
/// substring. Ambiguity is an error that lists the candidates — a wrong guess would silently change
/// which model answers, and that is the one thing a switch must never do by accident.
fn resolve(book: &Book, arg: &str) -> anyhow::Result<String> {
    if let Ok(n) = arg.parse::<usize>() {
        return match book.models.get(n.wrapping_sub(1)) {
            Some(r) if n >= 1 => Ok(r.id.clone()),
            _ => bail!(crate::tf!(
                "there is no model number {} (1..{})",
                n,
                book.models.len()
            )),
        };
    }
    let low = arg.to_lowercase();
    if let Some(r) = book
        .models
        .iter()
        .find(|r| r.id.eq_ignore_ascii_case(arg) || r.name.eq_ignore_ascii_case(arg) || r.model.eq_ignore_ascii_case(arg))
    {
        return Ok(r.id.clone());
    }
    let hits: Vec<&ModelRecord> = book
        .models
        .iter()
        .filter(|r| r.name.to_lowercase().contains(&low) || r.model.to_lowercase().contains(&low))
        .collect();
    match hits.len() {
        1 => Ok(hits[0].id.clone()),
        0 => bail!(crate::tf!(
            "no model matches “{}”. Saved: {}",
            arg,
            names(book.models.iter())
        )),
        _ => bail!(crate::tf!(
            "“{}” matches {} models: {} — give the number instead.",
            arg,
            hits.len(),
            names(hits.into_iter())
        )),
    }
}

fn names<'a>(it: impl Iterator<Item = &'a ModelRecord>) -> String {
    it.map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aox-models-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn cfg(base: &str, model: &str, key: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            base_url: base.into(),
            model: model.into(),
            api_key: key.map(str::to_string),
            ..ProviderConfig::default()
        }
    }

    /// The book *is* the model configuration now: whatever `active` points at has to come back out as
    /// a runnable `ProviderConfig`, with the per-model numbers and the key intact.
    #[test]
    fn the_active_record_is_what_runs() {
        let mut b = Book::default();
        let mut r = rec("m1", "deepseek-flash", "https://api.deepseek.com");
        r.api_key = "sk-k".into();
        r.context_window = Some(200_000);
        r.max_tokens = Some(8_000);
        r.temperature = Some(0.2);
        r.thinking = Thinking::High;
        r.stream = false;
        b.upsert(r);
        assert!(b.active_config().is_none(), "nothing is in force until it is chosen");

        assert!(b.set_active("m1"));
        let c = b.active_config().expect("active");
        assert_eq!(c.base_url, "https://api.deepseek.com");
        assert_eq!(c.model, "deepseek-flash");
        assert_eq!(c.api_key.as_deref(), Some("sk-k"));
        assert_eq!(c.context_window, Some(200_000));
        assert_eq!(c.max_tokens, Some(8_000));
        assert_eq!(c.temperature, Some(0.2));
        assert_eq!(c.thinking, Thinking::High);
        assert!(!c.stream, "stream is per endpoint and has to survive the round trip");
        assert!(!b.set_active("nope"), "an id that is not there changes nothing");
    }

    /// `adopt` is the one-time import from a `[provider]` still in `config.toml`. The key comes with
    /// it (the book is the only place a key lives now), and it must not fire twice.
    #[test]
    fn adoption_takes_the_key_once_and_then_stays_out_of_the_way() {
        let dir = temp_dir("adopt");
        let mut b = Book::default();
        assert!(b.adopt(&cfg("https://api.x.dev", "m", Some("sk-from-config")), &dir));
        let active = b.active.clone().expect("adopted");
        let r = b.by_id(&active).unwrap();
        assert_eq!(r.api_key, "sk-from-config");
        assert_eq!(r.model, "m");

        // A second call with a different endpoint changes nothing: the book wins.
        assert!(!b.adopt(&cfg("https://other.dev", "z", Some("sk-2")), &dir));
        assert_eq!(b.active.as_deref(), Some(active.as_str()));
        assert_eq!(b.models.len(), 1);

        // And it survived to disk.
        let reloaded = Book::load(&dir);
        assert_eq!(reloaded.active.as_deref(), Some(active.as_str()));
        assert_eq!(reloaded.active_record().unwrap().api_key, "sk-from-config");
    }

    /// An edit that carries no key (the dialog's empty field means "unchanged") must not wipe the
    /// stored one — that would turn "change the temperature" into a 401.
    #[test]
    fn remembering_an_edit_without_a_key_keeps_the_stored_one() {
        let mut b = Book::default();
        let mut r = rec("m1", "m", "https://api.x.dev");
        r.api_key = "sk-stored".into();
        b.upsert(r);
        b.set_active("m1");

        b.remember(&cfg("https://api.x.dev", "m", None));
        assert_eq!(b.active_record().unwrap().api_key, "sk-stored");

        // …and a new key replaces both the old one and any environment reference.
        let mut c = cfg("https://api.x.dev", "m", Some("sk-new"));
        c.api_key_env = Some("SHOULD_GO".into());
        b.remember(&c);
        assert_eq!(b.active_record().unwrap().api_key, "sk-new");
        assert!(b.active_record().unwrap().api_key_env.is_none());
    }

    /// `/model`'s argument resolution: number → id, exact name → id, unique substring → id, and
    /// **ambiguity is refused**. A wrong guess here silently changes which model answers, which is the
    /// one thing a switch must never do by accident.
    #[test]
    fn a_model_argument_resolves_or_refuses() {
        let mut b = Book::default();
        b.upsert(rec("m1", "deepseek-flash", "https://api.deepseek.com"));
        b.upsert(rec("m2", "deepseek-v4-pro", "https://api.deepseek.com"));
        b.upsert(rec("m3", "qwen3-8b", "https://dashscope.aliyuncs.com"));

        assert_eq!(resolve(&b, "1").unwrap(), "m1");
        assert_eq!(resolve(&b, "3").unwrap(), "m3");
        assert_eq!(resolve(&b, "deepseek-v4-pro").unwrap(), "m2");
        assert_eq!(resolve(&b, "m3").unwrap(), "m3", "the id works too");
        assert_eq!(resolve(&b, "DEEPSEEK-V4-PRO").unwrap(), "m2", "names compare case-insensitively");
        assert_eq!(resolve(&b, "qwen").unwrap(), "m3", "a unique substring is enough");

        assert!(resolve(&b, "0").is_err(), "numbering is 1-based");
        assert!(resolve(&b, "9").is_err());
        let ambiguous = resolve(&b, "deepseek").unwrap_err().to_string();
        assert!(ambiguous.contains("2 models"), "ambiguity says how many: {ambiguous}");
        assert!(ambiguous.contains("deepseek-flash"), "…and which: {ambiguous}");
    }

    /// The list marks the record in force, and marks a hidden one — otherwise "why is this model not
    /// in the picker" has no answer in the one place that shows every record.
    #[test]
    fn the_list_marks_what_is_running_and_what_is_hidden() {
        let mut b = Book::default();
        b.upsert(rec("m1", "deepseek-flash", "https://api.deepseek.com"));
        let mut hidden = rec("m2", "pro", "https://api.deepseek.com");
        hidden.enabled = false;
        b.upsert(hidden);
        b.set_active("m1");

        let text = list(&b);
        assert!(text.contains("1. deepseek-flash"), "{text}");
        assert!(text.contains("api.deepseek.com"), "the host, not the whole URL: {text}");
        assert!(text.contains("在用") || text.contains("in force"), "{text}");
        assert!(text.contains("2. pro"), "{text}");
        assert!(text.contains("已从选择器隐藏") || text.contains("hidden"), "{text}");
        assert!(text.contains("/model"), "and it says how to switch: {text}");

        assert!(list(&Book::default()).contains("models.json"), "an empty book points at the file");
    }

    /// Windows are shown rounded to the unit people say out loud; the exact number stays in the file.
    #[test]
    fn windows_are_labelled_the_way_people_say_them() {
        assert_eq!(window_label(1_048_576), "1M");
        assert_eq!(window_label(131_072), "131K");
        assert_eq!(window_label(8_192), "8K");
        assert_eq!(window_label(512), "512");
    }

    /// A file written by the previous layout (version 1) still loads: no `active`, same records.
    #[test]
    fn a_version_1_file_loads_with_nothing_in_force() {
        let dir = temp_dir("v1");
        std::fs::create_dir_all(dir.join(AOX_DIR)).unwrap();
        std::fs::write(
            file_path(&dir),
            br#"{"version":1,"models":[{"id":"m1","name":"x","base_url":"u","model":"m"}]}"#,
        )
        .unwrap();
        let b = Book::load(&dir);
        assert_eq!(b.models.len(), 1);
        assert!(b.active.is_none(), "version 1 never named one");
        assert!(b.active_config().is_none());
    }

    fn rec(id: &str, model: &str, base: &str) -> ModelRecord {
        let mut r = ModelRecord::blank(id.into());
        r.name = model.into();
        r.model = model.into();
        r.base_url = base.into();
        r
    }

    /// The line under a row must not repeat the name: a model saved with its default name showed
    /// `deepseek-flash` above `deepseek-flash · api.deepseek.com`, which is the same words twice.
    #[test]
    fn the_subtitle_drops_the_model_name_when_it_is_already_the_title() {
        let r = rec("m1", "deepseek-flash", "https://api.deepseek.com");
        assert_eq!(r.subtitle(), "api.deepseek.com");

        let mut r = rec("m1", "deepseek-flash", "https://api.deepseek.com");
        r.name = "Flash · 便宜的".into();
        assert_eq!(r.subtitle(), "deepseek-flash · api.deepseek.com");
    }

    /// Every field the list depends on has to survive the round trip, or "set it up once" is false.
    #[test]
    fn a_saved_model_comes_back_intact() {
        let dir = temp_dir("rt");
        let mut b = Book::default();
        let mut r = rec("m1", "deepseek-flash", "https://api.deepseek.com");
        r.api_key = "sk-1".into();
        r.context_window = Some(1_000_000);
        r.thinking = Thinking::High;
        r.temperature = Some(0.5);
        b.upsert(r);
        b.save(&dir).unwrap();

        let back = Book::load(&dir);
        let r = back.by_id("m1").expect("the record is back");
        assert_eq!(r.api_key, "sk-1");
        assert_eq!(r.context_window, Some(1_000_000));
        assert_eq!(r.thinking, Thinking::High);
        assert_eq!(r.temperature, Some(0.5));
        assert!(r.enabled, "a record without the key reads as enabled");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The identity of "the one in force" is (endpoint, model), not the record id: the runtime has
    /// never heard of the id, and `config.toml` may have been edited by hand.
    #[test]
    fn the_active_record_is_matched_on_endpoint_and_model() {
        let mut b = Book::default();
        b.upsert(rec("m1", "deepseek-flash", "https://api.deepseek.com"));
        b.upsert(rec("m2", "deepseek-flash", "https://other.example"));
        let a = b.active("https://api.deepseek.com/", "deepseek-flash").expect("trailing slash");
        assert_eq!(a.id, "m1");
        assert!(b.active("https://api.deepseek.com", "deepseek-pro").is_none());
    }

    /// Editing a record must not move it to the bottom — a row that jumps after every edit reads as
    /// "that got re-added" rather than "that got changed".
    #[test]
    fn editing_keeps_the_position_in_the_list() {
        let mut b = Book::default();
        b.upsert(rec("m1", "a", "https://x.dev"));
        b.upsert(rec("m2", "b", "https://x.dev"));
        let mut r = rec("m1", "a2", "https://x.dev");
        r.name = "renamed".into();
        b.upsert(r);
        assert_eq!(b.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["m1", "m2"]);
        assert_eq!(b.by_id("m1").unwrap().name, "renamed");
    }

    /// Ids are free slots, so deleting one and adding another reuses the smallest — the JSON stays
    /// tidy and no id ever collides.
    #[test]
    fn ids_fill_the_lowest_free_slot() {
        let mut b = Book::default();
        assert_eq!(b.next_id(), "m1");
        b.upsert(rec("m1", "a", "https://x.dev"));
        assert_eq!(b.next_id(), "m2");
        b.upsert(rec("m3", "c", "https://x.dev"));
        assert_eq!(b.next_id(), "m2");
        b.remove("m2");
        assert_eq!(b.next_id(), "m2");
    }

    /// A window the user typed outlives a rescan; a window the endpoint reported is refreshed.
    #[test]
    fn a_rescan_keeps_hand_set_windows_and_refreshes_reported_ones() {
        let mut b = Book::default();
        let mut r = rec("m1", "a", "https://x.dev");
        r.context_window = Some(262_144);
        b.upsert(r);
        b.upsert(rec("m2", "b", "https://x.dev"));
        b.set_scan(
            "https://x.dev/",
            vec![
                SavedModel { id: "a".into(), window: Some(111) },
                SavedModel { id: "b".into(), window: Some(222) },
            ],
            Some("https://x.dev/v1/models".into()),
        );
        // `scanned` is the *endpoint's* list, so both records hold both models — matched by id, not by
        // position (the list is sorted by the endpoint, not by the record).
        let win = |id: &str, model: &str| {
            b.by_id(id)
                .unwrap()
                .scanned
                .iter()
                .find(|m| m.id == model)
                .and_then(|m| m.window)
        };
        assert_eq!(win("m1", "a"), Some(111));
        assert_eq!(win("m2", "b"), Some(222));
        // Both records learned the list, which is the point of matching on the endpoint.
        assert_eq!(b.by_id("m2").unwrap().scanned.len(), 2);
        assert_eq!(b.by_id("m2").unwrap().scanned_url.as_deref(), Some("https://x.dev/v1/models"));
    }

    /// A record with no window of its own falls back to the one the endpoint reported for its model.
    #[test]
    fn the_window_falls_back_to_the_scan() {
        let mut r = rec("m1", "a", "https://x.dev");
        r.scanned = vec![SavedModel { id: "a".into(), window: Some(128_000) }];
        assert_eq!(r.window(), Some(128_000));
        r.context_window = Some(1_000_000);
        assert_eq!(r.window(), Some(1_000_000), "the record's own value wins");
    }

    /// Only enabled records are offered in the input box.
    #[test]
    fn disabled_records_are_hidden_but_kept() {
        let mut b = Book::default();
        let mut r = rec("m1", "a", "https://x.dev");
        r.enabled = false;
        b.upsert(r);
        b.upsert(rec("m2", "b", "https://x.dev"));
        assert_eq!(b.enabled().count(), 1);
        assert_eq!(b.models.len(), 2, "the record itself stays");
    }

    /// A broken or future file reads as an empty list rather than half a book.
    #[test]
    fn a_broken_or_future_file_reads_as_empty() {
        let dir = temp_dir("bad");
        std::fs::create_dir_all(dir.join(AOX_DIR)).unwrap();
        std::fs::write(dir.join(AOX_DIR).join(FILE_NAME), b"{ not json").unwrap();
        assert!(Book::load(&dir).models.is_empty());

        std::fs::write(
            dir.join(AOX_DIR).join(FILE_NAME),
            br#"{"version":99,"models":[{"id":"m1","name":"x","base_url":"u","model":"m"}]}"#,
        )
        .unwrap();
        assert!(Book::load(&dir).models.is_empty(), "an unknown version is not guessed at");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The earlier layout (`endpoints.json`) is picked up once, so the endpoint and key typed before
    /// the redesign are not lost.
    #[test]
    fn the_old_endpoint_file_is_migrated() {
        let dir = temp_dir("legacy");
        std::fs::create_dir_all(dir.join(AOX_DIR)).unwrap();
        std::fs::write(
            dir.join(AOX_DIR).join(LEGACY_FILE),
            br#"{"version":1,"endpoints":[{"name":"api.x.dev","base_url":"https://api.x.dev","api_key":"sk-old","models":[{"id":"x-1","window":128000},{"id":"x-2"}],"scanned_at":5}]}"#,
        )
        .unwrap();
        let b = Book::load(&dir);
        assert_eq!(b.models.len(), 1);
        let r = &b.models[0];
        assert_eq!(r.base_url, "https://api.x.dev");
        assert_eq!(r.api_key, "sk-old");
        assert_eq!(r.model, "x-1", "the first scanned model becomes the record's model");
        assert_eq!(r.context_window, Some(128_000));
        assert_eq!(r.scanned.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
