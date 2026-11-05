//! The **model UI**: the settings page, the add/edit dialog, and the chip at the bottom of the input box.
//!
//! # The shape, and why it changed (2026-10-27, second cut)
//!
//! The first version put one long form on the settings page: endpoint, key, model, window, cap,
//! temperature, thinking. That is a form for **one** model, and every number lived in it. It reads as
//! a configuration sheet, not as a list of models — you cannot see what you have, cannot keep two
//! providers ready, and every glance at the temperature costs a trip through the whole page.
//!
//! What replaced it splits along "how often is it touched":
//!
//! * **Settings → Model is a list**: `添加模型` / `批量管理` on top, then one row per saved model
//!   (56pt, whole row clickable, an enable switch on the right). Clicking a row edits it; the switch
//!   decides whether it shows up in the input box. That is the thing a person has *several* of.
//! * **The dialog is one model**: endpoint, key (+ 测试连接), model name (+ 扫描模型) — the three
//!   things only the provider can tell you. Cancel / Save, like any add dialog.
//! * **The input box's pop-up is the current model's numbers**: window, cap, temperature, thinking.
//!   Those are tuned *while working*, so they sit where the work happens — and they apply to the model
//!   being talked to, not to a global setting.
//!
//!   The four are a **draft**: typing only moves the draft, and the footer's 确定 is the single thing
//!   that writes them (`commit_all`) — one edit, one provider swap. Every other way out of the panel
//!   (click elsewhere, `Esc`, the chip) is a cancel and puts the draft back (`revert_inline`). The
//!   first cut committed per field on focus loss, which needed four writes and four swaps for one
//!   visit, and — because it edited a *clone* of the draft and copied it back only on commit — threw
//!   away every keystroke in between ("改不了数字").
//!
//! # Three conventions (unchanged; they are what keeps ownership clear)
//!
//! **① State lives in [`ModelUi`], held by `App`.** This module never touches a field of `App`.
//! **② Parameters in, requests out.** Nothing here calls the bridge or writes a file: the provider
//! lives in `Runtime`, which only the agent thread owns.
//! **③ The current state is pushed in** ([`ProviderView`]), so "which model is running" has one answer.
//!
//! # Where the data lives
//!
//! `config.toml` stays the statement of **what runs now**; `.aox/models.json` is the saved list (see
//! `gui/src/models.rs`). Activating a record copies its values into `config.toml`, so the CLI and a
//! hand edit still see exactly one truth.

use std::path::{Path, PathBuf};

use egui::{Align2, Color32, CornerRadius, Id, Margin, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};

use aox::config::Thinking;

use crate::app::{
    button, button_ex, button_size, button_sized, paint_button, primary_button, primary_plus_button,
    Weight, BTN_H,
};
use crate::bridge::{ModelsView, ProviderEdit, ProviderView};
use aox::modelbook::{Book, ModelRecord, SavedModel};
use crate::theme::{self, P};

/// The chip's height, shared with the workplace chip so the bottom row is one row of one height.
const CHIP_H: f32 = 28.0;
/// How wide the chip may get before the model name is elided.
const CHIP_MAX_W: f32 = 220.0;
/// The pop-up list.
const MENU_W: f32 = 316.0;
/// One model's card, opened beside the list.
const CARD_W: f32 = 296.0;
/// One row of the settings list. Tall on purpose: the whole row is the click target, and a 56pt band
/// is what makes "click a row to edit it" obvious without an edit icon on every line.
const ROW_H: f32 = 56.0;
/// The add/edit dialog's content width (the reference screenshot's proportions).
const DIALOG_W: f32 = 552.0;
/// Field padding in the dialog. `text + 2 × this` has to come out at [`BTN_H`] (34), so a label, its
/// field and the button beside it share one height and one baseline — that is the whole reason the
/// padding is a constant instead of a taste.
const FIELD_PAD: i8 = 8;

// ─────────────────────────── what the UI asks the app to do ───────────────────────────

/// One frame's worth of requests.
///
/// Several at once is the **normal** case, not an edge one: pressing "测试连接" right after typing a
/// key takes the focus off that field, which commits it — so one click legitimately means "apply this
/// *and* test it". Carrying them in one struct keeps both; a single `Option` would let one silently
/// overwrite the other.
#[derive(Default)]
pub struct Requests {
    pub apply: Option<Box<ProviderEdit>>,
    pub scan: Option<Box<ScanReq>>,
    pub test: Option<Box<TestProbe>>,
    /// "Manage models…" in the pop-up: open Settings on the Model tab.
    pub open_settings: bool,
}

/// One thing to send to the agent thread. The app turns each into a `ToAgent` message.
pub enum Request {
    /// Make this configuration the running model (and, on the bridge, save it).
    Apply(Box<ProviderEdit>),
    /// Ask the endpoint which models it serves.
    Scan(Box<ScanReq>),
    /// Ask whether these three values can actually talk.
    Test(Box<TestProbe>),
}

/// "Scan this endpoint" — carrying its own target, rather than the app reading the values back out of
/// the UI: the dialog may be showing something else by the time the request is sent.
pub struct ScanReq {
    pub base_url: String,
    pub api_key: Option<String>,
}

/// "Test connection": one tiny chat request against these three values.
pub struct TestProbe {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
}

impl Requests {
    fn apply(&mut self, e: ProviderEdit) {
        self.apply = Some(Box::new(e));
    }
}

// ─────────────────────────── state ───────────────────────────

pub struct ModelUi {
    /// The last state the bridge pushed. `None` before the first one arrives.
    view: Option<ProviderView>,
    /// The saved models (see `gui/src/models.rs`).
    book: Book,
    root: Option<PathBuf>,
    /// The chip's list is open.
    open: bool,
    /// Where the chip was drawn, so the list can anchor to it.
    button_rect: Option<Rect>,
    /// One line on the settings page: the last outcome.
    notice: Option<(String, bool)>,
    /// The add/edit dialog, when open.
    editor: Option<Editor>,
    /// The record whose card is open **beside** the list (`None` = the list alone). Picking a row does
    /// not switch anything: it opens this model's own card, which is where 使用 and its numbers are.
    selected: Option<String>,
    /// Batch mode: `Some(selected ids)` while it is on. `None` is the normal list.
    batch: Option<Vec<String>>,
    /// The batch delete's first press landed; the second one deletes. Reset whenever the selection
    /// changes, so "armed" never carries over to a different set of models.
    batch_armed: bool,
    /// The running model's numbers, as text (so a half-typed number is not rounded while typing).
    ///
    /// These four are a **draft**: they are what the pop-up's fields show, and they do not have to
    /// agree with what is running. Only 确定 (`commit_all`) turns them into a request; closing the
    /// panel any other way puts them back (`revert_inline`). Keeping them as `String` is what lets a
    /// half-typed `100` sit in the box without being read as a window of 100 tokens.
    win: String,
    mtok: String,
    temp: String,
    thy: Thinking,
    /// A scan or a test is in flight — the buttons say so and cannot be pressed twice.
    busy_scan: bool,
    busy_test: bool,
    /// The running model has already been adopted into an empty list (once per launch: after that, an
    /// empty list means the user emptied it, and putting things back would fight them).
    adopted: bool,
}

/// The add/edit dialog.
///
/// One dialog for both, because they are the same three fields: adding is editing a blank record. The
/// differences are the title, the verb on the button, and whether "delete" exists.
struct Editor {
    /// `None` = adding.
    id: Option<String>,
    base_url: String,
    api_key: String,
    model: String,
    /// The last scan of this endpoint, for the model dropdown.
    scanned: Vec<SavedModel>,
    show_key: bool,
    /// The model dropdown is open.
    combo: bool,
    /// Put the cursor in the key field on the next frame.
    focus_key: bool,
    /// What the last scan / test said.
    status: Option<(String, bool)>,
    /// "Delete" was pressed once; a second press does it. Cheaper than a confirmation modal for
    /// something this small, and impossible to trigger by accident.
    armed_delete: bool,
    /// Ask for SSE streaming. Per endpoint: a gateway that rejects `stream_options` needs it off.
    stream: bool,
}

impl Default for ModelUi {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelUi {
    pub fn new() -> Self {
        Self {
            view: None,
            book: Book::default(),
            root: None,
            open: false,
            button_rect: None,
            notice: None,
            editor: None,
            selected: None,
            batch: None,
            batch_armed: false,
            win: String::new(),
            mtok: String::new(),
            temp: String::new(),
            thy: Thinking::default(),
            busy_scan: false,
            busy_test: false,
            adopted: false,
        }
    }

    /// Give the UI the app root once it is known (the first `Boot`), and load the list then.
    ///
    /// Not in `new()`: at construction the root is not known, and a list loaded from the wrong place
    /// would silently look empty — or worse, be saved there.
    pub fn attach_root(&mut self, root: &Path) {
        if self.root.as_deref() == Some(root) {
            return;
        }
        self.root = Some(root.to_path_buf());
        self.book = Book::load(root);
        self.sync_inline();
    }

    /// Collapse the list (another pop-up on the same row was opened).
    pub fn close_menu(&mut self) {
        self.open = false;
    }

    /// Open a blank dialog with the cursor in the key field (a fresh install opens the page for
    /// exactly that one purpose).
    pub fn focus_key(&mut self) {
        self.open_editor(None, true);
    }

    pub fn set_view(&mut self, v: ProviderView) {
        self.reload_book();
        // A dialog opened before the first `Provider` (which is exactly what happens on a fresh
        // install: `Boot` opens it, `Provider` follows) would otherwise show an empty endpoint even
        // though one is known. Fill it in — "add a model on the endpoint I am already on" is the
        // common case, and retyping the host is not.
        if let Some(e) = &mut self.editor {
            if e.id.is_none() && e.base_url.trim().is_empty() && !v.base_url.trim().is_empty() {
                e.base_url = v.base_url.clone();
            }
        }
        // The switch landed: the model (or the endpoint) changed, so the list has done its job.
        let switched = self
            .view
            .as_ref()
            .is_some_and(|old| old.model != v.model || norm(&old.base_url) != norm(&v.base_url));
        if switched {
            self.open = false;
        }
        // A model is running but the list does not know it — the state a hand-edited `config.toml`
        // (and every install that upgrades into this design) starts in. Adopt it **once**, so the
        // settings page never says "no models" while a conversation is being answered. The key is
        // deliberately left empty: it may come from the environment or from the file, and copying
        // either into the list would be a guess that outlives the guess.
        if !self.adopted
            && self.book.models.is_empty()
            && !v.model.trim().is_empty()
            && !v.base_url.trim().is_empty()
        {
            let mut r = ModelRecord::blank(self.book.next_id());
            r.name = v.model.clone();
            r.base_url = v.base_url.clone();
            r.model = v.model.clone();
            r.context_window = Some(v.context_window);
            r.max_tokens = Some(v.max_tokens);
            r.temperature = v.temperature;
            r.thinking = v.thinking;
            self.book.upsert(r);
            self.save_book();
        }
        self.adopted = true;
        self.view = Some(v);
        self.sync_inline();
    }

    /// A scan came back.
    ///
    /// Routed to the **dialog** when one is open (that is where the button was pressed and where the
    /// dropdown has to fill), and into the book either way. A result that arrives after the dialog
    /// closed still lands in the book: dropping it would mean scanning twice for nothing.
    pub fn set_models(&mut self, v: ModelsView) {
        self.busy_scan = false;
        let models: Vec<SavedModel> =
            v.entries.iter().map(|(id, w)| SavedModel { id: id.clone(), window: *w }).collect();
        let n = models.len();
        self.book.set_scan(&v.base_url, models.clone(), v.url.clone());
        self.save_book();
        match &mut self.editor {
            Some(e) if norm(&e.base_url) == norm(&v.base_url) => match &v.error {
                Some(err) => e.status = Some((err.clone(), false)),
                None => {
                    e.scanned = models;
                    e.status = Some((aox::tf!("Found {} model(s)", n), true));
                }
            },
            _ => match v.error {
                Some(err) => self.notice = Some((err, false)),
                None => self.notice = Some((aox::tf!("Found {} model(s)", n), true)),
            },
        }
    }

    /// A "test connection" came back.
    pub fn set_test(&mut self, base_url: String, model: String, ok: bool, text: String) {
        self.busy_test = false;
        let _ = (base_url, model);
        match &mut self.editor {
            // The dialog is where the button was pressed and where the answer is useful. It is also
            // the only place the endpoint + model it was asked about are still on screen, so a late
            // reply belongs there rather than in a line nobody is looking at.
            Some(e) => e.status = Some((text, ok)),
            None => self.notice = Some((text, ok)),
        }
    }

    /// One line about a model operation (applied / saved / failed).
    pub fn set_notice(&mut self, ok: bool, text: String) {
        self.busy_scan = false;
        self.busy_test = false;
        if ok {
            // The switch landed (or the same model was re-applied, which `set_view` cannot see).
            self.open = false;
            // Fold what is now in force back onto the record it belongs to, so the list and the pop-up
            // agree with the request path.
            if let Some(v) = self.view.clone() {
                if let Some(mut rec) = self.book.active_record().cloned() {
                    rec.context_window = Some(v.context_window);
                    rec.max_tokens = Some(v.max_tokens);
                    rec.temperature = v.temperature;
                    rec.thinking = v.thinking;
                    self.book.upsert(rec);
                    self.save_book();
                }
                self.sync_inline();
            }
        }
        self.notice = Some((text, ok));
    }

    pub fn model(&self) -> Option<&str> {
        self.view.as_ref().map(|v| v.model.as_str()).filter(|m| !m.is_empty())
    }

    pub fn ready(&self) -> bool {
        self.view.as_ref().is_some_and(|v| v.ready)
    }

    fn window(&self) -> u32 {
        self.view.as_ref().map_or(0, |v| v.context_window)
    }

    fn base_url(&self) -> &str {
        self.view.as_ref().map_or("", |v| v.base_url.as_str())
    }

    /// The record matching what the runtime is running. `None` when `config.toml` holds a model nobody
    /// saved (a hand edit, or a fresh install).
    fn is_active(&self, rec: &ModelRecord) -> bool {
        let Some(v) = &self.view else { return false };
        // The book says which record is in force (`active`), and that is the answer — comparing by
        // (endpoint, model) is what once marked two rows as current. The fallback is for a book with
        // no `active` (version 1, or one written before anything was chosen).
        match self.book.active.as_deref() {
            Some(id) => id == rec.id,
            None => rec.is_active(&v.base_url, &v.model),
        }
    }

    /// Re-read the list from disk.
    ///
    /// The GUI keeps a copy for drawing, the agent thread writes the file (every apply marks the
    /// record in force), so the copy has to be refreshed when a new state is pushed in — otherwise
    /// "which one is current" would follow the GUI's idea rather than the runtime's.
    fn reload_book(&mut self) {
        if let Some(root) = self.root.clone() {
            self.book = Book::load(&root);
        }
    }

    /// Fill the pop-up's numbers from the running model.
    /// Refill the draft from what is actually running. Called whenever a `ProviderView` arrives and
    /// whenever the pop-up closes without 确定.
    fn sync_inline(&mut self) {
        let Some(v) = self.view.clone() else { return };
        self.win = window_text(v.context_window);
        self.mtok = v.max_tokens.to_string();
        self.temp = v.temperature.map(trim_f32).unwrap_or_default();
        self.thy = v.thinking;
    }

    /// Closing the pop-up without 确定 is a **cancel**, so the draft goes back to what is in force.
    /// Keeping it would leave three fields showing numbers nobody applied — and the chip under them
    /// saying something else.
    fn revert_inline(&mut self) {
        self.sync_inline();
    }

    /// Has the draft drifted from what is running? Drives the footer's "unsaved" hint, and makes
    /// 确定 on an untouched panel a plain close instead of a provider swap.
    fn inline_dirty(&self) -> bool {
        let Some(v) = &self.view else { return false };
        self.win != window_text(v.context_window)
            || self.mtok != v.max_tokens.to_string()
            || self.temp != v.temperature.map(trim_f32).unwrap_or_default()
            || self.thy != v.thinking
    }

    fn save_book(&mut self) {
        let Some(root) = self.root.clone() else { return };
        // A failed save costs the remembered list, not the session; say nothing and move on (the same
        // rule as the fold state in `uistate.rs`).
        let _ = self.book.save(&root);
    }

    /// Flip one record's switch.
    ///
    /// **Always allowed, the one in use included.** An earlier cut refused that case with a status line
    /// — which is precisely what "clicked 启用/停用 and nothing happened" was: on a fresh install there
    /// is exactly one model, it is the one running, and it was the one being clicked. "Enabled" here
    /// means "offered in the input box's list", not "permitted to run": hiding the current model only
    /// removes it from the picker, and the picker keeps a row for whatever is running anyway (see
    /// `menu`), so nothing becomes unreachable.
    fn toggle_enabled(&mut self, id: &str) {
        if let Some(r) = self.book.by_id_mut(id) {
            r.enabled = !r.enabled;
            let (on, name) = (r.enabled, r.name.clone());
            self.save_book();
            self.notice = Some((
                if on {
                    aox::tf!("“{}” is shown in the model list again.", name)
                } else {
                    aox::tf!("“{}” is hidden from the model list. It keeps running if it was in use.", name)
                },
                true,
            ));
        }
    }

    fn open_editor(&mut self, id: Option<&str>, focus_key: bool) {
        let e = match id.and_then(|id| self.book.by_id(id)) {
            Some(rec) => Editor {
                id: Some(rec.id.clone()),
                base_url: rec.base_url.clone(),
                api_key: String::new(),
                model: rec.model.clone(),
                scanned: rec.scanned.clone(),
                show_key: false,
                combo: false,
                focus_key,
                status: None,
                armed_delete: false,
                stream: rec.stream,
            },
            None => {
                // A new record starts from what is running when there is one: "add another model on
                // the endpoint I am already using" is the common case, and retyping the host is not.
                let v = self.view.clone();
                Editor {
                    id: None,
                    base_url: v.as_ref().map(|v| v.base_url.clone()).unwrap_or_default(),
                    api_key: String::new(),
                    model: String::new(),
                    scanned: v
                        .as_ref()
                        .and_then(|v| self.book.active(&v.base_url, &v.model))
                        .map(|r| r.scanned.clone())
                        .unwrap_or_default(),
                    show_key: false,
                    combo: false,
                    focus_key,
                    status: None,
                    armed_delete: false,
                    stream: true,
                }
            }
        };
        // The stored key is never put on screen: an empty field means "unchanged". Showing it again
        // would be one screenshot away from being written into the activity stream.
        self.editor = Some(e);
    }

    /// Turn a record into the edit that makes it the running model.
    fn edit_of(rec: &ModelRecord) -> ProviderEdit {
        ProviderEdit {
            base_url: rec.base_url.clone(),
            model: rec.model.clone(),
            api_key: Some(rec.api_key.clone()).filter(|k| !k.trim().is_empty()),
            context_window: rec.window().unwrap_or(aox::config::DEFAULT_CONTEXT_WINDOW),
            max_tokens: rec.max_tokens.unwrap_or(aox::config::DEFAULT_MAX_TOKENS),
            temperature: rec.temperature,
            thinking: rec.thinking,
            thinking_wire: rec.thinking_wire,
            stream: rec.stream,
        }
    }

    /// Open a model's card: remember which one, and load the draft from **it**.
    ///
    /// The draft is one set of four strings, but the card can be opened on any record — so selecting
    /// has to re-point the draft. For the record in force the values come from the `ProviderView`
    /// (that is what is actually answering); for any other one, from the record itself.
    pub fn select(&mut self, id: &str) {
        let is_active = self.book.active.as_deref() == Some(id);
        if is_active {
            self.sync_inline();
        } else if let Some(r) = self.book.by_id(id) {
            self.win = window_text(r.window().unwrap_or(0));
            self.mtok = r.max_tokens.unwrap_or(aox::config::DEFAULT_MAX_TOKENS).to_string();
            self.temp = r.temperature.map(trim_f32).unwrap_or_default();
            self.thy = r.thinking;
        }
        self.selected = Some(id.to_string());
        self.notice = None;
    }

    /// Whether the draft differs from **that** record — the card's 确定 is disabled when it does not.
    fn dirty_for(&self, id: &str) -> bool {
        if self.book.active.as_deref() == Some(id) {
            return self.inline_dirty();
        }
        let Some(rec) = self.book.by_id(id) else { return false };
        self.win != window_text(rec.window().unwrap_or(0))
            || self.mtok != rec.max_tokens.unwrap_or(aox::config::DEFAULT_MAX_TOKENS).to_string()
            || self.temp != rec.temperature.map(trim_f32).unwrap_or_default()
            || self.thy != rec.thinking
    }

    /// Turn the draft into **one** edit: 窗口 / 输出 / 温度 / 思考 together.
    ///
    /// One request, because all four land in the same record and the same live swap. The earlier
    /// shape committed one field at a time — four writes and four provider swaps for one visit to the
    /// panel, and three of them made with numbers the user had already replaced.
    ///
    /// The pop-up edits the model you are talking to (there is no "which record" question in a list
    /// opened from a conversation), so the record is found by (endpoint, model) and, when there is
    /// none, **created** — otherwise the numbers would apply to the session and evaporate next launch.
    ///
    /// Returns `None` **with the draft untouched** on a bad number: the message goes to `notice` and
    /// the panel stays open, so the typo can be fixed instead of retyped.
    #[cfg(test)]
    fn commit_all(&mut self) -> Option<ProviderEdit> {
        let id = self.book.active.clone()?;
        self.commit_to(&id).ok().flatten()
    }

    /// Write the draft onto **that** record. `Ok(Some(edit))` means "apply it as well" (it is the
    /// record in force), `Ok(None)` means "saved only", `Err` means a number did not parse — in which
    /// case the draft is left exactly as typed and the message is in `notice`.
    fn commit_to(&mut self, id: &str) -> Result<Option<ProviderEdit>, ()> {
        let Some(mut rec) = self.book.by_id(id).cloned() else { return Err(()) };
        let Some(window) = parse_num(&self.win) else {
            self.notice = Some((
                aox::i18n::t("The context window has to be a whole number").into(),
                false,
            ));
            return Err(());
        };
        let Some(mtok) = parse_num(&self.mtok) else {
            self.notice = Some((
                aox::i18n::t("The reply cap has to be a whole number").into(),
                false,
            ));
            return Err(());
        };
        let temperature = match self.temp.trim() {
            "" => None,
            t => match t.parse::<f32>() {
                Ok(x) if x.is_finite() => Some(x),
                _ => {
                    self.notice = Some((
                        aox::i18n::t("Temperature has to be a number").into(),
                        false,
                    ));
                    return Err(());
                }
            },
        };
        rec.context_window = Some(window);
        rec.max_tokens = Some(mtok);
        rec.temperature = temperature;
        rec.thinking = self.thy;
        let is_active = self.book.active.as_deref() == Some(id);
        self.book.upsert(rec.clone());
        self.save_book();
        Ok(if is_active { Some(Self::edit_of(&rec)) } else { None })
    }
}

// ─────────────────────────── small helpers ───────────────────────────

/// The window as the field shows it, and what `sync_inline` / `inline_dirty` compare against. `0` is
/// "not known yet", which is an empty box rather than a literal zero.
fn window_text(w: u32) -> String {
    if w > 0 {
        w.to_string()
    } else {
        String::new()
    }
}

fn trim_f32(v: f32) -> String {
    if (v - v.round()).abs() < 0.0001 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v}")
    }
}

/// "128000" → 128000. Also accepts `128k` / `1m`, because that is how people write windows.
fn parse_num(s: &str) -> Option<u32> {
    let t = s.trim().to_ascii_lowercase().replace(['_', ',', ' '], "");
    if t.is_empty() {
        return None;
    }
    let (num, mul) = match t.chars().last()? {
        'k' => (&t[..t.len() - 1], 1_000u64),
        'm' => (&t[..t.len() - 1], 1_000_000u64),
        _ => (t.as_str(), 1u64),
    };
    let n: f64 = num.parse().ok()?;
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    u32::try_from((n * mul as f64).round() as u64).ok()
}

/// `1000000` → `1M`. Rounding is only ever for reading; the exact number stays in the field.
///
/// One implementation, in the library: `/model` in the CLI prints the same windows, and two rounding
/// rules would eventually disagree on the one number a user compares between the two front ends.
pub use aox::modelbook::window_label as fmt_window;

fn norm(a: &str) -> String {
    a.trim().trim_end_matches('/').to_ascii_lowercase()
}

pub fn think_label(t: Thinking) -> &'static str {
    match t {
        Thinking::Default => aox::i18n::t("Default"),
        Thinking::Off => aox::i18n::t("Off"),
        Thinking::Low => aox::i18n::t("Low"),
        Thinking::High => aox::i18n::t("High"),
        Thinking::Max => aox::i18n::t("Max"),
    }
}

// ─────────────────────────── the chip ───────────────────────────

/// The chip at the bottom-left of the input box: `◇ deepseek-flash · 1M ⌄`.
///
/// Same geometry as the workplace chip next to it (28 high, radius 8, 10pt padding, 15pt glyph, 7pt
/// gap, 13pt type) — the two sit on one row and any difference reads as two kinds of control.
pub fn chip(ui: &mut Ui, st: &mut ModelUi, enabled: bool) -> bool {
    let ready = st.ready();
    let name = st.model().unwrap_or("").to_string();
    let text = if name.is_empty() {
        aox::i18n::t("Model · not configured").to_string()
    } else {
        let w = st.window();
        if w > 0 {
            format!("{name} · {}", fmt_window(w))
        } else {
            name.clone()
        }
    };

    let font = egui::FontId::proportional(13.0);
    let pad = 10.0;
    let icon = 15.0;
    let gap = 7.0;
    let avail = (CHIP_MAX_W - icon - gap - pad * 2.0).max(40.0);
    let galley = crate::app::elide(ui, &text, font.clone(), Color32::PLACEHOLDER, avail);
    let size = Vec2::new(icon + gap + galley.size().x + pad * 2.0, CHIP_H);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);

    let hov = enabled && resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    let radius = CornerRadius::same(theme::R1);
    let fg = if !enabled {
        P::text_faint()
    } else if !ready {
        P::warn()
    } else {
        P::text_dim()
    };
    painter.rect_filled(rect, radius, if hov { P::surface_hi() } else { Color32::TRANSPARENT });
    if hov {
        painter.rect_stroke(rect, radius, Stroke::new(1.0, P::edge()), egui::StrokeKind::Inside);
    }

    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + pad + icon * 0.5, rect.center().y),
        Vec2::splat(icon),
    );
    paint_model_mark(painter, icon_rect, fg);
    let g = crate::app::elide(ui, &text, font, fg, avail);
    painter.galley(
        Pos2::new(rect.left() + pad + icon + gap, rect.center().y - g.size().y * 0.5),
        g,
        fg,
    );

    let clicked = resp.clicked() && enabled;
    if clicked {
        st.open = !st.open;
    }
    st.button_rect = Some(rect);
    clicked
}

/// The chip's mark: a **wireframe cube** — "a model is a box you talk to", drawn as an isometric box.
///
/// Outline only, on purpose: at 15pt a filled cube is a blob, and two squares (what this used to be)
/// is not a box at all. The hexagon is the silhouette; the three edges meeting at the middle are what
/// turn it into a box, and they are also what survives at this size when everything else mushes.
fn paint_model_mark(p: &egui::Painter, r: Rect, fg: Color32) {
    let c = r.center();
    let s = r.width() * 0.42;
    let wx = s * 0.92;
    let wy = s * 0.55;
    let top = Pos2::new(c.x, c.y - s);
    let ur = Pos2::new(c.x + wx, c.y - wy);
    let lr = Pos2::new(c.x + wx, c.y + wy);
    let bot = Pos2::new(c.x, c.y + s);
    let ll = Pos2::new(c.x - wx, c.y + wy);
    let ul = Pos2::new(c.x - wx, c.y - wy);
    let st = Stroke::new(1.15, fg);
    p.add(egui::Shape::closed_line(vec![top, ur, lr, bot, ll, ul], st));
    // The front-top corner and its three edges: up-left, up-right, straight down.
    p.line_segment([c, ul], st);
    p.line_segment([c, ur], st);
    p.line_segment([c, bot], st);
}

// ─────────────────────────── the pop-up (switch + tune) ───────────────────────────

/// The list the chip opens: the enabled models, then the running model's numbers.
pub fn menu(ctx: &egui::Context, st: &mut ModelUi) -> Requests {
    let mut out = Requests::default();
    if !st.open {
        return out;
    }
    let Some(btn) = st.button_rect else {
        st.open = false;
        return out;
    };
    // Anchor on the chip's top edge and grow upward (`pivot(LEFT_BOTTOM)`), so the panel is not clipped
    // by the window's bottom edge — the same maths as the workplace menu, and the same warning: no
    // coordinate conversion here (these rects are already client-local).
    let anchor = btn.left_top() - Vec2::new(0.0, 6.0);
    let mut close = false;
    let mut save = false;
    let mut apply_rec: Option<ModelRecord> = None;

    // The **enabled** models, each with "is this the one in force" decided here (by id, so twins
    // cannot both light up). If the running model is not saved, it is shown first anyway: a list that
    // omits the model you are talking to reads as a bug.
    let cur_model = st.model().unwrap_or("").to_string();
    let cur_base = st.base_url().to_string();
    let mut rows: Vec<(ModelRecord, bool)> =
        st.book.enabled().map(|r| (r.clone(), st.is_active(r))).collect();
    if !rows.iter().any(|(_, cur)| *cur) && !cur_model.is_empty() {
        let mut temp = ModelRecord::blank(String::new());
        temp.name = cur_model.clone();
        temp.model = cur_model.clone();
        temp.base_url = cur_base.clone();
        temp.context_window = Some(st.window());
        rows.insert(0, (temp, true));
    }

    // Where the picked row ended up, so the card can be drawn level with it.
    let mut sel_row: Option<Rect> = None;
    let list = egui::Area::new(Id::new("aox-model-menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor)
        .pivot(Align2::LEFT_BOTTOM)
        .show(ctx, |ui| {
            theme::floating(theme::R2)
                .inner_margin(Margin::symmetric(10, 10))
                .show(ui, |ui| {
                    ui.set_width(MENU_W);
                    if rows.is_empty() {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(aox::i18n::t("No model yet — add one in Settings → Model"))
                                .size(12.5)
                                .color(P::text_dim()),
                        );
                        ui.add_space(4.0);
                    } else {
                        // A taller allowance than before: the list no longer has to leave room for
                        // four number rows underneath, so it can be the whole panel.
                        egui::ScrollArea::vertical()
                            .id_salt("aox-model-menu-list")
                            .max_height(330.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                for (r, active) in &rows {
                                    let picked = st.selected.as_deref() == Some(r.id.as_str());
                                    let resp = pick_row(ui, r, *active, picked);
                                    if picked {
                                        sel_row = Some(resp.rect);
                                    }
                                    if resp.clicked() {
                                        st.select(&r.id);
                                    }
                                }
                            });
                        ui.add_space(6.0);
                        if config_row(ui).clicked() {
                            out.open_settings = true;
                            close = true;
                        }
                    }
                });
        });

    // ── the second level: one model's card, beside the list, level with its row ──
    let mut card: Option<Rect> = None;
    if let Some(id) = st.selected.clone() {
        if let Some(rec) = st.book.by_id(&id).cloned() {
            // Beside the list, but never off the window: on a narrow window the card slides left
            // over the list instead of being drawn where it cannot be read (it is a foreground area,
            // so overlapping is honest — disappearing is not).
            let screen = ctx.content_rect();
            let left =
                (list.response.rect.right() + 8.0).min(screen.right() - CARD_W - 24.0).max(screen.left() + 8.0);
            let y = sel_row.map(|r| r.center().y).unwrap_or(list.response.rect.center().y);
            let active = st.is_active(&rec);
            let area = egui::Area::new(Id::new("aox-model-card"))
                .order(egui::Order::Foreground)
                .fixed_pos(Pos2::new(left, y))
                .pivot(Align2::LEFT_CENTER)
                .show(ctx, |ui| {
                    theme::floating(theme::R2)
                        .inner_margin(Margin::symmetric(12, 12))
                        .show(ui, |ui| {
                            ui.set_width(CARD_W);
                            ui.label(RichText::new(&rec.name).size(14.5).color(P::ink()));
                            let sub = rec.subtitle();
                            if !sub.is_empty() {
                                ui.add_space(2.0);
                                ui.label(
                                    RichText::new(sub).size(11.0).color(P::text_faint()),
                                );
                            }
                            ui.add_space(12.0);

                            let lw = label_col_w(ui);
                            let fw = (CARD_W - lw - 6.0).max(70.0);
                            let mut enter = false;
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                enter |=
                                    num_row(ui, lw, aox::i18n::t("Window"), &mut st.win, fw, "aox-pop-window");
                            });
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                enter |=
                                    num_row(ui, lw, aox::i18n::t("Output"), &mut st.mtok, fw, "aox-pop-output");
                            });
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                enter |= num_row(
                                    ui,
                                    lw,
                                    aox::i18n::t("Temperature"),
                                    &mut st.temp,
                                    fw,
                                    "aox-pop-temp",
                                );
                            });
                            // Thinking overrides temperature at the provider; saying so here is
                            // cheaper than letting someone chase "the setting does nothing".
                            if st.thy != Thinking::Default && st.thy != Thinking::Off {
                                ui.add_space(2.0);
                                ui.label(
                                    RichText::new(aox::i18n::t("ignored while thinking is on"))
                                        .size(10.5)
                                        .color(P::text_faint()),
                                );
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                row_label(ui, lw, aox::i18n::t("Thinking"));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.spacing_mut().item_spacing.x = 4.0;
                                        for t in Thinking::ALL.iter().rev() {
                                            if think_chip(ui, *t, st.thy == *t).clicked() {
                                                st.thy = *t;
                                            }
                                        }
                                    },
                                );
                            });

                            // A failed number says why, right above the button that failed, and the
                            // draft stays as typed so the typo can be corrected instead of retyped.
                            if let Some((text, false)) = st.notice.clone() {
                                ui.add_space(8.0);
                                ui.label(RichText::new(text).size(11.0).color(P::warn()));
                            }

                            ui.add_space(12.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                // 使用 only when it is not already the one running: the check in the
                                // list is the *other* half of that answer, and a live button that does
                                // nothing is worse than no button.
                                if !active && button(ui, aox::i18n::t("Use"), Weight::Secondary).clicked()
                                {
                                    apply_rec = Some(rec.clone());
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if button_ex(
                                            ui,
                                            aox::i18n::t("Confirm"),
                                            Weight::Primary,
                                            st.dirty_for(&rec.id),
                                        )
                                        .clicked()
                                        {
                                            save = true;
                                        }
                                    },
                                );
                            });
                            if enter {
                                save = true;
                            }
                        });
                });
            card = Some(area.response.rect);
        }
    }

    // 确定: writes the draft onto the model the card is showing. When that is the model in force it
    // also applies it (one request, one swap); for any other record it is a plain save.
    if save {
        let id = st.selected.clone().unwrap_or_default();
        match st.commit_to(&id) {
            Ok(Some(edit)) => {
                out.apply(edit);
                close = true;
            }
            Ok(None) => close = true,
            // A number did not parse: the card stays open with the message on it.
            Err(()) => {}
        }
    }
    if let Some(rec) = apply_rec {
        out.apply(ModelUi::edit_of(&rec));
        close = true;
    }

    // A click anywhere else closes both panels — and Esc does too. Without this the only ways out
    // were the chip again or picking something, so the panel sat on top of the conversation.
    //
    // The chip's own rect is excluded: that click is what *opens* the list, and on the same frame it
    // is already "outside" the freshly drawn panel, which would close it again immediately.
    let chip = st.button_rect;
    let inside = |p: Pos2| {
        list.response.rect.contains(p) || card.is_some_and(|r| r.contains(p))
    };
    let clicked_away = ctx.input(|i| {
        i.pointer.any_click()
            && i.pointer
                .interact_pos()
                .is_some_and(|p| !inside(p) && chip.map_or(true, |c| !c.contains(p)))
    });
    let escaped = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    if close || clicked_away || escaped {
        st.open = false;
        st.selected = None;
        // Every way out other than a successful 确定 is a **cancel**, so the draft goes back to what is
        // in force (or to the record, which `select` reloads next time).
        if out.apply.is_none() && !save {
            st.revert_inline();
        }
    }
    out
}


/// One model row in the pop-up: the name, `model · host` under it, and a green check on the one in
/// force.
///
/// The check replaces three earlier attempts at the same job (a 3pt left rule, a filled dot, ink text).
/// On this monochrome palette those differ from an ordinary row by one grey step; a green check is the
/// only mark in the whole list that is a colour, so "which one am I on" is answered at a glance without
/// reading anything.
fn model_row(ui: &mut Ui, name: &str, sub: &str, current: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), P::surface_hi());
    }
    let fg = if current { P::ink() } else { P::text() };
    let check_w = if current { 26.0 } else { 10.0 };
    let text_x = rect.left() + 12.0;
    let text_w = (rect.right() - check_w - text_x).max(40.0);
    let g = crate::app::elide(ui, name, egui::FontId::proportional(13.5), fg, text_w);
    ui.painter().galley(Pos2::new(text_x, rect.top() + 7.0), g, fg);
    if !sub.is_empty() {
        let sg = crate::app::elide(ui, sub, egui::FontId::proportional(11.0), P::text_faint(), text_w);
        ui.painter().galley(Pos2::new(text_x, rect.bottom() - 17.0), sg, P::text_faint());
    }
    if current {
        paint_check(
            ui.painter(),
            Rect::from_center_size(Pos2::new(rect.right() - 18.0, rect.center().y), Vec2::splat(14.0)),
        );
    }
    resp
}

/// One row of the pop-up's list: the model's name, its window on the right, and a check when it is the
/// one in force. Picking the row opens its card (`menu`), it does **not** switch.
fn pick_row(ui: &mut Ui, rec: &ModelRecord, active: bool, picked: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
    let hov = resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if hov || picked {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), P::surface_hi());
    }

    // Right side first, so the name can be elided against what is left: window, then the check.
    let mut right = rect.right() - 12.0;
    let check_w = if active { 20.0 } else { 0.0 };
    right -= check_w;
    if let Some(w) = rec.window() {
        let text = aox::modelbook::window_label(w);
        let g = ui.painter().layout_no_wrap(
            text,
            egui::FontId::proportional(11.5),
            Color32::PLACEHOLDER,
        );
        ui.painter().galley(
            Pos2::new(right - g.size().x, rect.center().y - g.size().y * 0.5),
            g.clone(),
            P::text_faint(),
        );
        right -= g.size().x + 10.0;
    }
    let fg = if active { P::ink() } else { P::text() };
    let text_w = (right - rect.left() - 12.0).max(40.0);
    let g = crate::app::elide(ui, &rec.name, egui::FontId::proportional(13.5), fg, text_w);
    ui.painter().galley(
        Pos2::new(rect.left() + 12.0, rect.center().y - g.size().y * 0.5),
        g,
        fg,
    );
    if active {
        paint_check(
            ui.painter(),
            Rect::from_center_size(
                Pos2::new(rect.right() - 15.0, rect.center().y),
                Vec2::splat(13.0),
            ),
        );
    }
    resp
}

/// The list's last row: the way to the full list (the reference's "配置自定义模型").
fn config_row(ui: &mut Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), P::surface_hi());
    }
    paint_pencil(
        ui.painter(),
        Rect::from_center_size(
            Pos2::new(rect.left() + 17.0, rect.center().y),
            Vec2::splat(13.0),
        ),
    );
    let g = crate::app::elide(
        ui,
        aox::i18n::t("Configure models…"),
        egui::FontId::proportional(13.0),
        P::text_dim(),
        (rect.width() - 40.0).max(60.0),
    );
    ui.painter().galley(
        Pos2::new(rect.left() + 34.0, rect.center().y - g.size().y * 0.5),
        g,
        P::text_dim(),
    );
    resp
}

/// A pen writing a line: "this row is where models are edited" (the reference glyph).
///
/// Outline, not a filled pencil: at 13pt the only thing that reads is the **diagonal body plus the
/// line under it**, so those are drawn first and the cap/nib are one short stroke each — enough to
/// say "pen", nothing that turns into a smudge at this size.
fn paint_pencil(p: &egui::Painter, r: Rect) {
    let st = Stroke::new(1.15, P::text_faint());
    let k = r.width();
    let at = |fx: f32, fy: f32| Pos2::new(r.left() + fx * k, r.top() + fy * k);
    // The body runs lower-left → upper-right; `off` is its half width, perpendicular to that axis.
    let off = Vec2::new(0.085 * k, 0.085 * k);
    let nib = at(0.20, 0.74);
    let cap = at(0.80, 0.14);
    let body = vec![
        Pos2::new(nib.x + off.x, nib.y + off.y),
        Pos2::new(cap.x + off.x, cap.y + off.y),
        Pos2::new(cap.x - off.x, cap.y - off.y),
        Pos2::new(nib.x - off.x, nib.y - off.y),
    ];
    p.add(egui::Shape::closed_line(body, st));
    // The ferrule: the line across the top end, which is what makes it a pen rather than a stick.
    p.line_segment(
        [
            Pos2::new(cap.x + off.x - 0.10 * k, cap.y + off.y + 0.10 * k),
            Pos2::new(cap.x - off.x - 0.10 * k, cap.y - off.y + 0.10 * k),
        ],
        st,
    );
    // What it just wrote.
    p.line_segment([at(0.42, 0.92), at(0.94, 0.92)], st);
}

/// The green check: the only coloured mark in the model UI, and the one that answers "which one is in
/// force". Drawn, not typed — the glyph may not exist in the hand-picked font.
pub(crate) fn paint_check(p: &egui::Painter, r: Rect) {
    let fg = P::add_green();
    let a = Pos2::new(r.left() + 1.5, r.center().y + 0.5);
    let b = Pos2::new(r.center().x - 0.8, r.bottom() - 2.6);
    let c = Pos2::new(r.right() - 1.0, r.top() + 2.6);
    p.add(egui::Shape::line(vec![a, b, c], Stroke::new(1.9, fg)));
}

/// A checkbox with a label and a hint, on one line.
///
/// Hand-drawn for the same reason as everything else here: `ui.checkbox` brings egui's own colours and
/// metrics into a dialog that has neither, and the box would be a different size from the batch-mode one
/// two panels over. **The label is part of the hit target** — a 16pt box is not something to aim at.
fn check_row(ui: &mut Ui, on: &mut bool, label: &str, hint: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        paint_checkbox(ui.painter(), rect, *on, resp.hovered());
        let mut hit = resp.clicked();
        ui.add_space(9.0);
        let text = ui.add(
            egui::Label::new(RichText::new(label).size(12.5).color(P::text())).sense(Sense::click()),
        );
        if text.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        hit |= text.clicked();
        if !hint.is_empty() {
            ui.add_space(10.0);
            ui.label(RichText::new(hint).size(11.0).color(P::text_faint()));
        }
        if hit {
            *on = !*on;
        }
    });
}

/// A one-line thinking choice (`默认 / 关 / 低 / 高 / 最高`).
fn think_chip(ui: &mut Ui, t: Thinking, on: bool) -> egui::Response {
    let label = think_label(t);
    let font = egui::FontId::proportional(11.0);
    let g = ui.painter().layout_no_wrap(label.to_string(), font.clone(), Color32::PLACEHOLDER);
    let w = g.size().x + 12.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 24.0), Sense::click());
    let hov = resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    let radius = CornerRadius::same(theme::R1);
    if on {
        painter.rect_filled(rect, radius, P::surface_hi());
        painter.rect_stroke(rect, radius, Stroke::new(1.0, P::edge_raised()), egui::StrokeKind::Inside);
    } else if hov {
        painter.rect_filled(rect, radius, P::surface_hi());
    }
    let fg = if on { P::text() } else { P::text_dim() };
    let gate = ui.painter().layout_no_wrap(label.to_string(), font, fg);
    ui.painter().galley(
        Pos2::new(rect.center().x - gate.size().x * 0.5, rect.center().y - gate.size().y * 0.5),
        gate,
        fg,
    );
    resp
}

/// The pop-up's label column, in one width for every row: the longest caption plus the gap before its
/// field. `ui.label` sizes itself to its text, which is exactly what puts each field on a different x.
fn label_col_w(ui: &Ui) -> f32 {
    let widest = ["Window", "Output", "Temperature", "Thinking"]
        .iter()
        .map(|k| {
            ui.painter()
                .layout_no_wrap(
                    aox::i18n::t(k).to_string(),
                    egui::FontId::proportional(12.0),
                    Color32::PLACEHOLDER,
                )
                .size()
                .x
        })
        .fold(0.0_f32, f32::max);
    widest + 8.0
}

/// One `label + field` pair of the pop-up's number rows. `id` is the field's stable widget id.
///
/// Returns `true` when Enter was pressed **in that field** — the panel's 确定 is one keystroke away from
/// where the number was typed, which is the same thing every other form in this app does.
fn num_row(ui: &mut Ui, lw: f32, text: &str, val: &mut String, fw: f32, id: &str) -> bool {
    row_label(ui, lw, text);
    let r = compact_field(ui, val, fw, id);
    r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

/// A caption drawn into a **fixed-width** slot (see [`label_col_w`]).
fn row_label(ui: &mut Ui, w: f32, text: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, BTN_H), Sense::hover());
    let g = ui.painter().layout_no_wrap(
        text.to_string(),
        egui::FontId::proportional(12.0),
        Color32::PLACEHOLDER,
    );
    ui.painter().galley(
        Pos2::new(rect.left(), rect.center().y - g.size().y * 0.5),
        g,
        P::text_dim(),
    );
}

/// A compact field for the pop-up: a well with no frame of its own.
///
/// The text is edited **in place, on every keystroke** — the caller's `String` *is* the draft, and
/// nothing is saved until 确定. That is the fix for "改不了数字": the first version handed the widget a
/// clone and copied it back only on focus loss, so the next frame refilled the box from the old string
/// and every keystroke (and every backspace) looked inert.
fn compact_field(ui: &mut Ui, text: &mut String, w: f32, id: &str) -> egui::Response {
    theme::well(theme::R0)
        // Same height as a button: `text + 2 × FIELD_PAD` = `BTN_H`, so a field and the 确定 under it
        // are the same control in two skins.
        .inner_margin(Margin::symmetric(8, FIELD_PAD))
        .show(ui, |ui| {
            ui.set_width(w - 16.0);
            ui.add(
                egui::TextEdit::singleline(text)
                    // A stable id: three of these exist side by side, and an auto id derived from the
                    // label would drop the focus whenever the text changed length.
                    .id(Id::new(id))
                    .frame(false)
                    .font(theme::mono_font(12.5))
                    .desired_width(w - 16.0),
            )
        })
        .inner
}

// ─────────────────────────── the settings page: a list of models ───────────────────────────

/// Settings → Model: two buttons on top, then the list.
///
/// A list, not a form. Everything belonging to *one* model is in the dialog behind a row, and the three
/// numbers you tune while working are in the input box's pop-up — see the module docs.
pub fn panel(ui: &mut Ui, st: &mut ModelUi) -> Requests {
    let mut out = Requests::default();

    // ── top: add / batch ──
    //
    // Two states of **one** row rather than a bar that appears: "批量管理" becomes "完成", and the four
    // actions it holds line up on the right. They used to appear only *after* the first tick, which
    // meant entering batch mode and finding nothing to do with it.
    ui.horizontal(|ui| {
        if primary_plus_button(ui, aox::i18n::t("Add model")).clicked() {
            st.open_editor(None, false);
        }
        ui.add_space(8.0);
        // Copied out first: the buttons below change `st.batch`, which cannot happen while it is
        // borrowed by a `match` on it.
        let batch_mode = st.batch.is_some();
        let selected: Vec<String> = st.batch.clone().unwrap_or_default();
        if !batch_mode {
            if button_ex(
                ui,
                aox::i18n::t("Batch manage"),
                Weight::Ghost,
                !st.book.models.is_empty(),
            )
            .clicked()
            {
                st.batch = Some(Vec::new());
                st.batch_armed = false;
            }
        } else {
            if button(ui, aox::i18n::t("Done"), Weight::Ghost).clicked() {
                st.batch = None;
                st.batch_armed = false;
            }
            let n = selected.len();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let del = if st.batch_armed {
                    aox::i18n::t("Confirm delete")
                } else {
                    aox::i18n::t("Delete")
                };
                if button_ex(ui, del, Weight::Danger, n > 0).clicked() {
                    if st.batch_armed {
                        delete_batch(st, &selected);
                    } else {
                        // Two-step, like the dialog's delete: the first press says "you mean this",
                        // the second does it. A batch delete has no undo.
                        st.batch_armed = true;
                        st.notice = Some((
                            aox::tf!("Press “{}” again to delete {} model(s).", del, n),
                            false,
                        ));
                    }
                }
                ui.add_space(6.0);
                if button_ex(ui, aox::i18n::t("Hide"), Weight::Secondary, n > 0).clicked() {
                    apply_batch(st, &selected, false);
                }
                ui.add_space(6.0);
                if button_ex(ui, aox::i18n::t("Show"), Weight::Secondary, n > 0).clicked() {
                    apply_batch(st, &selected, true);
                }
                ui.add_space(10.0);
                if button(ui, aox::i18n::t("Select all"), Weight::Ghost).clicked() {
                    st.batch = Some(st.book.models.iter().map(|m| m.id.clone()).collect());
                    st.batch_armed = false;
                }
                ui.add_space(8.0);
                ui.label(
                    RichText::new(aox::tf!("{} selected", n))
                        .size(11.5)
                        .color(if n > 0 { P::text_dim() } else { P::text_faint() }),
                );
            });
        }
    });
    ui.add_space(8.0);

    // ── the list ──
    if st.book.models.is_empty() {
        ui.add_space(10.0);
        ui.label(
            RichText::new(aox::i18n::t("No models yet. Add one — an endpoint and a key are all it needs."))
                .size(12.5)
                .color(P::text_dim()),
        );
        ui.add_space(4.0);
        return out;
    }

    // The rows are **collected first**: a click can change the list (edit / toggle), and mutating it
    // while iterating it would be a borrow error at best and a skipped row at worst.
    let rows: Vec<ModelRecord> = st.book.models.clone();
    let mut edit: Option<String> = None;
    let mut toggle: Option<String> = None;
    let mut select: Option<String> = None;
    let mut use_id: Option<String> = None;
    for rec in &rows {
        let active = st.is_active(rec);
        let selected = st.batch.as_ref().is_some_and(|s| s.contains(&rec.id));
        match model_list_row(ui, rec, active, st.batch.is_some(), selected) {
            RowHit::Open => edit = Some(rec.id.clone()),
            RowHit::Toggle => toggle = Some(rec.id.clone()),
            RowHit::Select => select = Some(rec.id.clone()),
            RowHit::Use => use_id = Some(rec.id.clone()),
            RowHit::None => {}
        }
    }
    // "Use": the same request the input box's pop-up sends, so there is one switch path
    // (construct → swap → remember) rather than a second one that only the settings page knows.
    if let Some(id) = use_id {
        if let Some(rec) = st.book.by_id(&id) {
            out.apply(ModelUi::edit_of(rec));
        }
    }
    if let Some(id) = toggle {
        st.toggle_enabled(&id);
    }
    if let Some(id) = select {
        if let Some(sel) = &mut st.batch {
            match sel.iter().position(|x| x == &id) {
                Some(i) => {
                    sel.remove(i);
                }
                None => sel.push(id),
            }
        }
        // A different selection is a different question: the armed delete does not carry over.
        st.batch_armed = false;
    }
    if let Some(id) = edit {
        st.open_editor(Some(&id), false);
    }

    // ── the status line ──
    //
    // Three sources, in order of urgency: what just happened, why the running model is unusable, and
    // (when neither applies) where the numbers live. The middle one is what a fresh install reads:
    // "no API key yet" — precisely the sentence that has to be on screen for the panel to make sense.
    ui.add_space(8.0);
    if let Some((text, ok)) = st.notice.clone() {
        ui.label(RichText::new(text).size(11.5).color(if ok { P::text_dim() } else { P::warn() }));
    } else if let Some(err) = st.view.as_ref().filter(|v| !v.ready).and_then(|v| v.error.clone()) {
        ui.label(RichText::new(err).size(11.5).color(P::warn()));
    } else {
        ui.label(
            RichText::new(aox::i18n::t(
                "「使用」switches to a model; the switch on the right only decides whether it appears in the input box's list.",
            ))
            .size(11.5)
            .color(P::text_faint()),
        );
    }
    out
}

fn apply_batch(st: &mut ModelUi, ids: &[String], enabled: bool) {
    for id in ids {
        if let Some(r) = st.book.by_id_mut(id) {
            r.enabled = enabled;
        }
    }
    st.save_book();
    st.notice = Some((
        if enabled {
            aox::tf!("{} model(s) enabled", ids.len())
        } else {
            aox::tf!("{} model(s) hidden", ids.len())
        },
        true,
    ));
}

/// Delete the selected records.
///
/// The record **in force is left alone** and says so: the session keeps running it either way, and
/// removing it would also clear `active`, which is the one piece of state the next launch depends on.
fn delete_batch(st: &mut ModelUi, ids: &[String]) {
    let running = st.book.active.clone();
    let mut removed = 0;
    let mut skipped = 0;
    for id in ids {
        if running.as_deref() == Some(id.as_str()) {
            skipped += 1;
            continue;
        }
        st.book.remove(id);
        removed += 1;
    }
    st.save_book();
    st.batch = None;
    st.batch_armed = false;
    st.notice = Some(match skipped {
        0 => (aox::tf!("Deleted {} model(s).", removed), true),
        _ if removed == 0 => (
            aox::i18n::t("The model in force was not deleted — switch to another one first.").into(),
            false,
        ),
        _ => (
            aox::tf!(
                "Deleted {} model(s); the one in force was left alone.",
                removed
            ),
            true,
        ),
    });
}

/// What a click on a list row meant.
enum RowHit {
    None,
    /// Open the editor.
    Open,
    /// Flip the enable switch.
    Toggle,
    /// Batch mode: tick / untick.
    Select,
    /// "Use this one": make it the running model.
    Use,
}

/// One row of the settings list.
///
/// The **whole row** is the click target with a full-height hover fill, because "click a model to edit
/// it" has to be discoverable without an icon on every line; the switch and the batch checkbox swallow
/// their own clicks first, so they never open the editor by accident.
fn model_list_row(
    ui: &mut Ui,
    rec: &ModelRecord,
    active: bool,
    batch: bool,
    selected: bool,
) -> RowHit {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    let hov = resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    if hov || selected {
        painter.rect_filled(rect, CornerRadius::same(theme::R1), P::surface_hi());
    }

    // The right side is laid out first so the text can be elided against what is left.
    let mut right = rect.right() - 14.0;
    if batch {
        let box_rect = Rect::from_center_size(Pos2::new(right - 9.0, rect.center().y), Vec2::splat(18.0));
        let sresp = ui.interact(box_rect, Id::new(("aox-model-check", &rec.id)), Sense::click());
        paint_checkbox(painter, box_rect, selected, sresp.hovered());
        if sresp.clicked() {
            return RowHit::Select;
        }
        right -= 28.0;
    }
    let sw = Rect::from_center_size(Pos2::new(right - 20.0, rect.center().y), Vec2::new(40.0, 22.0));
    let sresp = ui.interact(sw, Id::new(("aox-model-switch", &rec.id)), Sense::click());
    if sresp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    paint_switch(painter, sw, rec.enabled, sresp.hovered());
    if sresp.clicked() {
        return RowHit::Toggle;
    }
    right -= 44.0;
    // "Use this one" — the verb the page was missing entirely: before this, switching model was only
    // possible from the input box's pop-up, so a list that shows a green check on one of five rows had
    // no way to move the check. Hidden in batch mode (there a click means "select") and on the row
    // that already is the one in force (the check says it).
    if !batch && !active {
        let text = aox::i18n::t("Use");
        let w = button_size(ui, text, Weight::Secondary).x;
        let r = Rect::from_center_size(
            Pos2::new(right - w * 0.5, rect.center().y),
            Vec2::new(w, BTN_H.min(ROW_H - 12.0)),
        );
        let uresp = ui.interact(r, Id::new(("aox-model-use", &rec.id)), Sense::click());
        if uresp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        paint_button(ui, r, text, Weight::Secondary, uresp.hovered());
        if uresp.clicked() {
            return RowHit::Use;
        }
        right -= w + 10.0;
    }

    let name_fg = if active {
        P::ink()
    } else if !rec.enabled {
        P::text_dim()
    } else {
        P::text()
    };
    // The one in force: a **green check right after the name**, and nothing else. Earlier attempts (a
    // left rule, a bold name, a "当前" label) all lived in the same grey range as everything else, and
    // one coloured mark is what actually answers "which one am I on".
    let check_w = if active { 22.0 } else { 0.0 };
    let text_w = (right - check_w - rect.left() - 20.0).max(60.0);
    let name_x = rect.left() + 16.0;
    let g = crate::app::elide(ui, &rec.name, egui::FontId::proportional(14.0), name_fg, text_w);
    let name_w = g.size().x;
    painter.galley(Pos2::new(name_x, rect.top() + 11.0), g, name_fg);
    if active {
        paint_check(
            painter,
            Rect::from_center_size(
                Pos2::new(name_x + name_w + 13.0, rect.top() + 11.0 + 8.0),
                Vec2::splat(13.0),
            ),
        );
    } else if hov {
        // "This row is clickable" — the click opens the editor, and a hand cursor alone does not say
        // what it opens. Drawn only on hover, so the resting list stays quiet.
        paint_pencil(
            painter,
            Rect::from_center_size(
                Pos2::new(name_x + name_w + 13.0, rect.top() + 11.0 + 8.0),
                Vec2::splat(12.0),
            ),
        );
    }
    let sub = rec.subtitle();
    if !sub.is_empty() {
        let sg =
            crate::app::elide(ui, &sub, egui::FontId::proportional(11.5), P::text_faint(), text_w);
        painter.galley(Pos2::new(name_x, rect.bottom() - 21.0), sg, P::text_faint());
    }

    if resp.clicked() {
        return RowHit::Open;
    }
    RowHit::None
}

/// The enable switch: a pill with a knob, filled when on.
fn paint_switch(p: &egui::Painter, r: Rect, on: bool, hover: bool) {
    let fill = if on {
        if hover {
            P::ink()
        } else {
            P::text()
        }
    } else if hover {
        P::edge_raised()
    } else {
        P::surface_hi()
    };
    p.rect_filled(r, CornerRadius::same(theme::pill(22.0)), fill);
    if !on {
        p.rect_stroke(
            r,
            CornerRadius::same(theme::pill(22.0)),
            Stroke::new(1.0, P::edge()),
            egui::StrokeKind::Inside,
        );
    }
    let knob_x = if on { r.right() - 11.0 } else { r.left() + 11.0 };
    let knob = if on { P::surface() } else { P::text_dim() };
    p.circle_filled(Pos2::new(knob_x, r.center().y), 7.5, knob);
}

fn paint_checkbox(p: &egui::Painter, r: Rect, on: bool, hover: bool) {
    let radius = CornerRadius::same(theme::R0);
    if on {
        p.rect_filled(r, radius, P::ink());
        // A tick, drawn as two strokes (the glyph may not exist in the hand-picked font).
        let a = Pos2::new(r.left() + 4.6, r.center().y);
        let b = Pos2::new(r.center().x - 0.6, r.bottom() - 5.0);
        let c = Pos2::new(r.right() - 4.4, r.top() + 5.2);
        p.add(egui::Shape::line(vec![a, b, c], Stroke::new(1.6, P::on_reverse())));
    } else {
        p.rect_stroke(
            r,
            radius,
            Stroke::new(1.0, if hover { P::border_strong() } else { P::edge() }),
            egui::StrokeKind::Inside,
        );
    }
}

// ─────────────────────────── the add / edit dialog ───────────────────────────

/// The add/edit dialog. Drawn as an overlay, so `App` calls it from `update` beside the other modals —
/// not from inside the settings card (a dialog inside a scrolling panel gets clipped by it).
pub fn dialog(ctx: &egui::Context, st: &mut ModelUi) -> Requests {
    let mut out = Requests::default();
    // The editor is **taken out** of `st` for the duration: the body needs `st.busy_*` and the dialog's
    // own fields at the same time, and a `&mut` into `st.editor` would forbid the former.
    let Some(mut ed) = st.editor.take() else { return out };

    let mut close = false;
    let mut save = false;
    let mut delete = false;
    let mut probe = false;
    let mut scan = false;

    let title = if ed.id.is_some() {
        aox::i18n::t("Edit model")
    } else {
        aox::i18n::t("Add model")
    };

    egui::Modal::new(egui::Id::new("aox-model-editor"))
        .backdrop_color(P::scrim())
        .frame(theme::floating(theme::card_radius()).inner_margin(Margin::symmetric(22, 18)))
        .show(ctx, |ui| {
            ui.set_width(DIALOG_W);
            ui.label(RichText::new(title).color(P::text()).size(16.0).strong());
            ui.add_space(14.0);

            // ── endpoint ──
            caption(ui, aox::i18n::t("API endpoint"));
            tall_field(ui, &mut ed.base_url, DIALOG_W, "https://api.deepseek.com", false);

            // ── key + test ──
            ui.add_space(12.0);
            caption(ui, "API Key");
            // The button column is **measured**, not fixed: `96` was chosen against "Test" and the
            // English "Scan models" runs past its own frame, which is what "按钮被压缩" was.
            let btn_w = button_w(
                ui,
                &[
                    aox::i18n::t("Test"),
                    aox::i18n::t("Testing…"),
                    aox::i18n::t("Scan models"),
                    aox::i18n::t("Scanning…"),
                ],
            );
            let field_w = DIALOG_W - btn_w - 10.0;
            ui.horizontal(|ui| {
                // The gap between field and button is decided **here**, not by `item_spacing`: the
                // theme's 8pt would stack on top of the explicit 10 and push the button 8pt past the
                // dialog's right edge (the same trap the settings card documents).
                ui.spacing_mut().item_spacing.x = 0.0;
                let r = tall_field_with(
                    ui,
                    &mut ed.api_key,
                    field_w,
                    aox::i18n::t("Paste your API key"),
                    !ed.show_key,
                    // Room for the eye inside the field's own background, so the value never runs
                    // under it.
                    30,
                );
                if ed.focus_key {
                    r.request_focus();
                    ed.focus_key = false;
                }
                // The eye sits **inside** the field's right edge, the way the reference does it: it is
                // a property of the value, not a separate action.
                let eye = Rect::from_center_size(
                    Pos2::new(r.rect.right() - 15.0, r.rect.center().y),
                    Vec2::splat(20.0),
                );
                let hov = ui.rect_contains_pointer(eye);
                if hov {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                paint_eye(
                    ui.painter(),
                    eye,
                    ed.show_key,
                    if hov { P::text() } else { P::text_faint() },
                );
                if ui.interact(eye, Id::new("aox-model-eye"), Sense::click()).clicked() {
                    ed.show_key = !ed.show_key;
                }
                ui.add_space(10.0);
                let label = if st.busy_test {
                    aox::i18n::t("Testing…")
                } else {
                    aox::i18n::t("Test")
                };
                if button_sized(ui, label, Weight::Secondary, btn_w).clicked() && !st.busy_test {
                    probe = true;
                }
            });

            // ── model name + scan ──
            ui.add_space(12.0);
            caption(ui, aox::i18n::t("Model name"));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                model_combo(ui, field_w, &mut ed.model, &mut ed.combo, &ed.scanned);
                ui.add_space(10.0);
                let label = if st.busy_scan {
                    aox::i18n::t("Scanning…")
                } else {
                    aox::i18n::t("Scan models")
                };
                if button_sized(ui, label, Weight::Secondary, btn_w).clicked() && !st.busy_scan {
                    scan = true;
                }
            });
            // The dropdown, under the field (the reference's combo, spelled out).
            if ed.combo && !ed.scanned.is_empty() {
                ui.add_space(4.0);
                theme::floating(theme::R2)
                    .inner_margin(Margin::symmetric(6, 6))
                    .show(ui, |ui| {
                        ui.set_width(DIALOG_W - 106.0);
                        egui::ScrollArea::vertical()
                            .id_salt("aox-model-combo")
                            .max_height(150.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                for m in ed.scanned.clone() {
                                    let on = m.id == ed.model;
                                    let label = match m.window {
                                        Some(w) => format!("{} · {}", m.id, fmt_window(w)),
                                        None => m.id.clone(),
                                    };
                                    if model_row(ui, &m.id, &label, on).clicked() {
                                        ed.model = m.id.clone();
                                        ed.combo = false;
                                    }
                                }
                            });
                    });
            }
            if ed.scanned.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(aox::i18n::t(
                        "Press Scan models to list what this endpoint serves — or type the name yourself.",
                    ))
                    .size(11.0)
                    .color(P::text_faint()),
                );
            }

            // ── streaming ──
            ui.add_space(10.0);
            check_row(
                ui,
                &mut ed.stream,
                aox::i18n::t("Streaming"),
                aox::i18n::t("the reply appears as it is written; turn it off for endpoints that reject stream"),
            );

            // ── status ──
            ui.add_space(10.0);
            match ed.status.clone() {
                Some((text, ok)) => {
                    ui.label(
                        RichText::new(text)
                            .size(11.5)
                            .color(if ok { P::text_dim() } else { P::warn() }),
                    );
                }
                None => {
                    ui.label(
                        RichText::new(aox::i18n::t(
                            "“Test” sends one tiny request; “Scan” lists the models this endpoint serves.",
                        ))
                        .size(11.5)
                        .color(P::text_faint()),
                    );
                }
            }

            // ── buttons ──
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                // Delete sits on the left, away from Save: a destructive action next to the affirmative
                // one is how people lose things.
                if ed.id.is_some() {
                    let label = if ed.armed_delete {
                        aox::i18n::t("Confirm delete")
                    } else {
                        aox::i18n::t("Delete")
                    };
                    let weight =
                        if ed.armed_delete { Weight::DangerSolid } else { Weight::Danger };
                    if button(ui, label, weight).clicked() {
                        if ed.armed_delete {
                            delete = true;
                        } else {
                            ed.armed_delete = true;
                        }
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary_button(ui, aox::i18n::t("Save")).clicked() {
                        save = true;
                    }
                    ui.add_space(6.0);
                    if button(ui, aox::i18n::t("Cancel"), Weight::Ghost).clicked() {
                        close = true;
                    }
                });
            });
        });

    // ── act on the buttons (after the closure: `ed` was borrowed inside it) ──
    if scan {
        let base = ed.base_url.trim().to_string();
        if base.is_empty() {
            ed.status = Some((aox::i18n::t("Fill in the endpoint first").into(), false));
        } else {
            let api_key = Some(ed.api_key.trim().to_string()).filter(|k| !k.is_empty());
            st.busy_scan = true;
            ed.status = Some((aox::i18n::t("Scanning…").into(), true));
            out.scan = Some(Box::new(ScanReq { base_url: base, api_key }));
        }
    }
    if probe {
        let base = ed.base_url.trim().to_string();
        let key = Some(ed.api_key.trim().to_string()).filter(|k| !k.is_empty());
        let model = ed.model.trim().to_string();
        if base.is_empty() {
            ed.status = Some((aox::i18n::t("Fill in the endpoint first").into(), false));
        } else if model.is_empty() {
            ed.status = Some((aox::i18n::t("Fill in the model name first").into(), false));
        } else {
            st.busy_test = true;
            ed.status = Some((aox::i18n::t("Testing…").into(), true));
            out.test = Some(Box::new(TestProbe { base_url: base, api_key: key, model }));
        }
    }
    if delete {
        if let Some(id) = ed.id.clone() {
            st.book.remove(&id);
            st.save_book();
            st.notice = Some((aox::i18n::t("Removed from the list.").into(), true));
        }
        close = true;
    }

    // Put the editor back before saving: `save_editor` consumes it (so a validation failure can hand
    // it straight back with the message attached).
    st.editor = Some(ed);
    if save {
        match save_editor(st) {
            Ok(Some(edit)) => out.apply(edit),
            Ok(None) => {}
            Err(msg) => {
                if let Some(e) = st.editor.as_mut() {
                    e.status = Some((msg, false));
                }
            }
        }
    } else if close {
        st.editor = None;
    }
    out
}

/// Take the dialog's values into the list. `Err` = the dialog stays open with this message.
///
/// Returns `Some(edit)` when the record should also become the **running** model, which happens when
/// nothing usable is running yet: on a fresh install, "save" is the last step of "make this work", and
/// making the user then find the chip and pick the model they just created is a step with no
/// information in it.
fn save_editor(st: &mut ModelUi) -> Result<Option<ProviderEdit>, String> {
    let Some(ed) = st.editor.take() else { return Ok(None) };
    let base_url = ed.base_url.trim().to_string();
    if base_url.is_empty() {
        st.editor = Some(ed);
        return Err(aox::i18n::t("Fill in the endpoint first").into());
    }
    // "Auto" is what the reference dialog shows before anything is picked. It means "the first model
    // this endpoint listed" — an answer that only exists once a scan does.
    let mut model = ed.model.trim().to_string();
    if model.eq_ignore_ascii_case("auto") || model.is_empty() {
        match ed.scanned.first() {
            Some(m) => model = m.id.clone(),
            None => {
                st.editor = Some(ed);
                return Err(aox::i18n::t(
                    "Scan the endpoint first, or type the model name — “Auto” needs a list to choose from.",
                )
                .into());
            }
        }
    }
    let api_key = ed.api_key.trim().to_string();

    // Keep what the record already had; a new one starts from the defaults plus the scan's window.
    //
    // "Add" with an endpoint + model that is **already saved** resolves to that record instead of
    // adding a twin: two identical rows are indistinguishable, one of them is dead weight, and the
    // duplicate is exactly what "why are two models shown as in use" was about.
    let mut rec = ed
        .id
        .as_deref()
        .and_then(|id| st.book.by_id(id))
        .cloned()
        .or_else(|| st.book.find_same(&base_url, &model).cloned())
        .unwrap_or_else(|| ModelRecord::blank(st.book.next_id()));
    if rec.name.trim().is_empty() || rec.name == rec.model {
        rec.name = model.clone();
    }
    rec.base_url = base_url;
    rec.model = model.clone();
    if !api_key.is_empty() {
        rec.api_key = api_key;
    }
    rec.stream = ed.stream;
    // Carry this endpoint's scan over (the dialog's copy is the freshest one).
    rec.scanned = ed.scanned.clone();
    if rec.context_window.is_none() {
        rec.context_window = rec
            .scanned
            .iter()
            .find(|m| m.id == model)
            .and_then(|m| m.window)
            .or_else(|| aox::llm::catalog::window_hint(&model));
    }
    let should_apply = !st.ready();
    st.book.upsert(rec.clone());
    st.save_book();
    st.editor = None;
    st.notice = Some((
        if should_apply {
            aox::tf!("Saved “{}” and made it the current model.", rec.name)
        } else {
            aox::tf!("Saved “{}”. Pick it from the input box to switch.", rec.name)
        },
        true,
    ));
    Ok(should_apply.then(|| ModelUi::edit_of(&rec)))
}

fn caption(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(13.0).color(P::text_dim()));
    ui.add_space(6.0);
}

/// A tall single-line field: the height comes from the **margin**, not from a minimum size (a
/// `TextEdit`'s box is its text plus its margin, and a taller box with the default margin leaves the
/// text stuck to the top edge).
fn tall_field(ui: &mut Ui, text: &mut String, w: f32, hint: &str, password: bool) -> egui::Response {
    tall_field_with(ui, text, w, hint, password, 12)
}

/// `right_pad` leaves room **inside the field's background** for the eye / the combo caret: drawing them
/// beside the field put them in the gap between field and button, which read as a stray glyph.
fn tall_field_with(
    ui: &mut Ui,
    text: &mut String,
    w: f32,
    hint: &str,
    password: bool,
    right_pad: i8,
) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(text)
            .desired_width(w)
            .margin(Margin { left: 12, right: right_pad, top: FIELD_PAD, bottom: FIELD_PAD })
            .font(egui::FontId::proportional(14.0))
            .password(password)
            .hint_text(RichText::new(hint).color(P::quiet()).size(13.0)),
    )
}

/// Width of a column of word buttons: the widest label plus padding, so no label can outgrow its frame.
fn button_w(ui: &Ui, labels: &[&str]) -> f32 {
    let widest = labels
        .iter()
        .map(|l| {
            ui.painter()
                .layout_no_wrap((*l).to_string(), egui::FontId::proportional(13.5), Color32::PLACEHOLDER)
                .size()
                .x
        })
        .fold(0.0f32, f32::max);
    (widest + 26.0).clamp(92.0, 150.0)
}

/// The model-name field plus its dropdown caret. The caret is drawn just inside the field's right
/// edge, like the reference; clicking it opens the scanned list below.
fn model_combo(
    ui: &mut Ui,
    w: f32,
    model: &mut String,
    open: &mut bool,
    scanned: &[SavedModel],
) -> egui::Response {
    let mut buf = if model.trim().is_empty() { "Auto".to_string() } else { model.clone() };
    let r = tall_field_with(ui, &mut buf, w, "", false, 30);
    // "Auto" is shown as a value, not as a name: typing something else has to reach the state, while
    // leaving it alone must not turn the placeholder into a real model called `Auto`.
    let untouched_placeholder = buf == "Auto" && model.trim().is_empty();
    if !untouched_placeholder && buf != *model {
        *model = buf;
    }
    let caret = Rect::from_center_size(
        Pos2::new(r.rect.right() - 15.0, r.rect.center().y),
        Vec2::splat(22.0),
    );
    let hov = ui.rect_contains_pointer(caret);
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let fg = if hov { P::text() } else { P::text_dim() };
    let c = caret.center();
    ui.painter()
        .line_segment([c + Vec2::new(-4.0, -2.0), c + Vec2::new(0.0, 2.5)], Stroke::new(1.4, fg));
    ui.painter()
        .line_segment([c + Vec2::new(0.0, 2.5), c + Vec2::new(4.0, -2.0)], Stroke::new(1.4, fg));
    if ui.interact(caret, Id::new("aox-model-caret"), Sense::click()).clicked() && !scanned.is_empty() {
        *open = !*open;
    }
    r
}

/// A flat word button at a fixed size (the dialog's "测试" / "扫描模型").

/// The show/hide eye. `shown` = plain text is on screen (a slashed eye when it is not).
fn paint_eye(p: &egui::Painter, r: Rect, shown: bool, fg: Color32) {
    let c = r.center();
    let w = 7.5;
    let h = 4.6;
    let stroke = Stroke::new(1.2, fg);
    // Two arcs, approximated by two petals — small enough that the shape reads as an eye.
    let left = Pos2::new(c.x - w, c.y);
    let right = Pos2::new(c.x + w, c.y);
    p.add(egui::Shape::line(
        vec![
            left,
            Pos2::new(c.x - w * 0.5, c.y - h),
            Pos2::new(c.x, c.y - h),
            Pos2::new(c.x + w * 0.5, c.y - h),
            right,
        ],
        stroke,
    ));
    p.add(egui::Shape::line(
        vec![
            right,
            Pos2::new(c.x + w * 0.5, c.y + h),
            Pos2::new(c.x, c.y + h),
            Pos2::new(c.x - w * 0.5, c.y + h),
            left,
        ],
        stroke,
    ));
    p.circle_filled(c, 1.9, fg);
    if !shown {
        p.line_segment(
            [c + Vec2::new(-w - 1.0, h + 1.0), c + Vec2::new(w + 1.0, -h - 1.0)],
            stroke,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window field has to accept the two ways people write a window.
    #[test]
    fn a_window_can_be_typed_as_a_number_or_with_a_suffix() {
        assert_eq!(parse_num("128000"), Some(128_000));
        assert_eq!(parse_num("128k"), Some(128_000));
        assert_eq!(parse_num("1m"), Some(1_000_000));
        assert_eq!(parse_num(" 1_000_000 "), Some(1_000_000));
    }

    /// Garbage must come back as `None`, not as 0 or a silently dropped digit.
    #[test]
    fn a_bad_window_is_refused_rather_than_rounded() {
        assert_eq!(parse_num(""), None);
        assert_eq!(parse_num("abc"), None);
        assert_eq!(parse_num("0"), None);
        assert_eq!(parse_num("-5"), None);
        assert_eq!(parse_num("12k5"), None);
    }

    /// The chip and the lists show a rounded window; the exact number stays in the field.
    ///
    /// `1048576` must read as `1M`: the earlier version only rounded numbers that divided evenly, so a
    /// real 1024² window printed as a seven-digit figure and squeezed the chip it sat in.
    #[test]
    fn the_window_label_rounds_only_what_it_displays() {
        assert_eq!(fmt_window(1_000_000), "1M");
        assert_eq!(fmt_window(1_048_576), "1M");
        assert_eq!(fmt_window(128_000), "128K");
        assert_eq!(fmt_window(131_072), "131K");
        assert_eq!(fmt_window(32_768), "33K");
        assert_eq!(fmt_window(512), "512");
    }

    /// A book with one record, marked in force — the state `Runtime::new` leaves the book in after
    /// `adopt`, which is what every "the running model" query resolves against.
    fn seed(st: &mut ModelUi) {
        let mut r = ModelRecord::blank("m1".into());
        r.name = "flash".into();
        r.base_url = "https://api.deepseek.com".into();
        r.model = "deepseek-flash".into();
        r.api_key = "sk-test".into();
        st.book.upsert(r);
        st.book.set_active("m1");
    }

    fn running() -> ProviderView {
        ProviderView {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-flash".into(),
            context_window: 1_000_000,
            max_tokens: 32_000,
            temperature: Some(0.3),
            thinking: Thinking::Default,
            ready: true,
            error: None,
        }
    }

    /// Saving a new record while something already works must **not** switch to it: the user asked for
    /// "save, then I can pick it in the input box", and silently changing the running model in the
    /// middle of a conversation is exactly the surprise this design has to avoid.
    #[test]
    fn saving_does_not_switch_when_a_model_is_already_running() {
        let mut st = ModelUi::new();
        seed(&mut st);
        st.set_view(running());
        st.open_editor(None, false);
        {
            let e = st.editor.as_mut().unwrap();
            e.base_url = "https://api.deepseek.com".into();
            e.model = "deepseek-v4-pro".into();
        }
        let edit = save_editor(&mut st).expect("valid");
        assert!(edit.is_none(), "no switch");
        assert_eq!(st.book.models.len(), 2);
        assert_eq!(st.book.models[1].model, "deepseek-v4-pro");
        assert_eq!(st.book.models[1].name, "deepseek-v4-pro", "the name defaults to the model");
    }

    /// With nothing running (a fresh install), saving **does** switch: that is the last step of "make
    /// this work", and asking the user to then find the chip would be a step with no information in it.
    #[test]
    fn saving_switches_when_nothing_is_running() {
        let mut st = ModelUi::new();
        st.open_editor(None, false);
        {
            let e = st.editor.as_mut().unwrap();
            e.base_url = "https://api.deepseek.com".into();
            e.model = "deepseek-flash".into();
        }
        let edit = save_editor(&mut st).expect("valid").expect("applies");
        assert_eq!(edit.model, "deepseek-flash");
        assert_eq!(edit.base_url, "https://api.deepseek.com");
    }

    /// "Auto" means "the first model the endpoint listed" — and is refused when there is no list,
    /// rather than sending the literal string `Auto` to the provider.
    #[test]
    fn auto_resolves_to_the_first_scanned_model_or_is_refused() {
        let mut st = ModelUi::new();
        st.open_editor(None, false);
        {
            let e = st.editor.as_mut().unwrap();
            e.base_url = "https://x.dev".into();
            e.model = "Auto".into();
        }
        assert!(save_editor(&mut st).is_err(), "no list to choose from");

        st.open_editor(None, false);
        {
            let e = st.editor.as_mut().unwrap();
            e.base_url = "https://x.dev".into();
            e.model = "auto".into();
            e.scanned = vec![
                SavedModel { id: "first".into(), window: Some(128_000) },
                SavedModel { id: "second".into(), window: None },
            ];
        }
        save_editor(&mut st).expect("resolves");
        let rec = &st.book.models[0];
        assert_eq!(rec.model, "first");
        assert_eq!(rec.context_window, Some(128_000), "the scan's window comes along");
    }

    /// The dialog never shows the stored key: an empty field means "unchanged", and saving keeps the
    /// key that is there.
    #[test]
    fn an_untouched_key_field_keeps_the_stored_key() {
        let mut st = ModelUi::new();
        seed(&mut st);
        st.open_editor(Some("m1"), false);
        assert_eq!(st.editor.as_ref().unwrap().api_key, "", "not put on screen");
        st.editor.as_mut().unwrap().model = "deepseek-v4-pro".into();
        save_editor(&mut st).expect("valid");
        assert_eq!(st.book.by_id("m1").unwrap().api_key, "sk-test");
        assert_eq!(st.book.by_id("m1").unwrap().model, "deepseek-v4-pro");
    }

    /// A running model that the list does not know about is adopted, once: the settings page must not
    /// claim "no models" while a conversation is being answered (the state every hand-edited
    /// `config.toml` is in).
    #[test]
    fn a_running_model_is_adopted_into_an_empty_list_once() {
        let mut st = ModelUi::new();
        st.set_view(running());
        assert_eq!(st.book.models.len(), 1);
        assert_eq!(st.book.models[0].model, "deepseek-flash");
        assert!(st.book.models[0].api_key.is_empty(), "the key is not invented");

        // The user deletes it; the next state push must not put it back.
        st.book.remove("m1");
        st.set_view(running());
        assert!(st.book.models.is_empty(), "an emptied list stays empty");
    }

    /// Drive a real pointer at a row and report what the row thought was clicked.
    ///
    /// The headless version of "I clicked the switch and nothing happened": press on one frame, release
    /// on the next (that is when egui reports a click), and read the `RowHit` back. A pure logic test
    /// would have passed while the actual hit test was broken.
    fn click_at(pos: Pos2) -> RowHit {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut rec = ModelRecord::blank("m1".into());
        rec.name = "flash".into();
        rec.base_url = "https://api.deepseek.com".into();
        rec.model = "deepseek-flash".into();

        let width = 400.0;
        let mut hit = RowHit::None;
        let run = |ctx: &egui::Context, events: Vec<egui::Event>, hit: &mut RowHit| {
            let mut input = egui::RawInput::default();
            input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 600.0)));
            input.events = events;
            let _ = ctx.run(input, |ctx| {
                egui::Area::new(Id::new("row-test")).fixed_pos(Pos2::new(10.0, 10.0)).show(
                    ctx,
                    |ui| {
                        ui.set_width(width);
                        *hit = model_list_row(ui, &rec, false, false, false);
                    },
                );
            });
        };
        // Frame 0: settle (the area's first layout).
        run(&ctx, vec![], &mut hit);
        run(&ctx, vec![egui::Event::PointerMoved(pos)], &mut hit);
        run(
            &ctx,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            }],
            &mut hit,
        );
        run(
            &ctx,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
            &mut hit,
        );
        hit
    }

    /// The enable switch has to swallow its own click. It sits *inside* a row that is itself clickable,
    /// and if the row wins the overlap then "click the switch" opens the editor instead — which is
    /// exactly the "点了没反应" report (the editor opening over the list is easy to miss, and the switch
    /// never moves).
    #[test]
    fn the_switch_takes_the_click_and_the_rest_of_the_row_opens_the_editor() {
        // The switch is 40 wide, 20pt in from the row's right edge; the row starts at x=10.
        let switch_x = 10.0 + 400.0 - 14.0 - 20.0;
        let hit = click_at(Pos2::new(switch_x, 10.0 + ROW_H * 0.5));
        assert!(matches!(hit, RowHit::Toggle), "the switch must win its own click");

        let hit = click_at(Pos2::new(10.0 + 120.0, 10.0 + ROW_H * 0.5));
        assert!(matches!(hit, RowHit::Open), "and the label area must open the editor");
    }

    /// The switch always flips — **including the model in use**. Refusing that case was the bug behind
    /// "clicked 启用/停用 and nothing happened": on a fresh install the only model is the running one, and
    /// an inert control reads as broken. Hiding it only takes it out of the picker, which keeps a row
    /// for whatever is running.
    #[test]
    fn the_switch_always_flips_even_for_the_model_in_use() {
        let mut st = ModelUi::new();
        seed(&mut st);
        st.set_view(running());
        st.toggle_enabled("m1");
        assert!(!st.book.by_id("m1").unwrap().enabled, "hidden");
        assert!(st.notice.as_ref().is_some_and(|(_, ok)| *ok), "and it says so");

        st.toggle_enabled("m1");
        assert!(st.book.by_id("m1").unwrap().enabled, "and back");
    }

    /// Editing the pop-up's numbers writes them onto the **record**, not just the session: otherwise
    /// the value would be forgotten on the next launch. 确定 sends all four at once — one write, one
    /// provider swap.
    #[test]
    fn a_popup_change_is_saved_with_the_model() {
        let mut st = ModelUi::new();
        seed(&mut st);
        st.set_view(running());
        st.temp = "0.9".into();
        let edit = st.commit_all().expect("valid");
        assert_eq!(edit.temperature, Some(0.9));
        assert_eq!(st.book.by_id("m1").unwrap().temperature, Some(0.9));
        assert_eq!(st.book.by_id("m1").unwrap().api_key, "sk-test", "the key rides along untouched");
    }

    /// Streaming is per model now (it used to be one `[provider] stream` in `config.toml`), so the
    /// dialog's toggle has to reach both the record and the edit that is applied from it.
    #[test]
    fn the_streaming_toggle_survives_the_round_trip() {
        let mut st = ModelUi::new();
        seed(&mut st);
        st.set_view(running());
        st.open_editor(Some("m1"), false);
        st.editor.as_mut().unwrap().stream = false;
        assert!(save_editor(&mut st).is_ok(), "an untouched key field is not an error");
        assert!(!st.book.by_id("m1").unwrap().stream, "the record remembers it");
        let rec = st.book.by_id("m1").unwrap().clone();
        assert!(!ModelUi::edit_of(&rec).stream, "and the edit carries it to the runtime");
    }

    /// The pop-up is two levels now: picking a row **opens that model's card**, it does not switch.
    /// And the card edits that record — saving it is not applying it unless it is the one in force.
    #[test]
    fn picking_a_model_opens_its_card_instead_of_switching() {
        let mut st = ModelUi::new();
        seed(&mut st); // m1 is in force
        let mut other = ModelRecord::blank("m2".into());
        other.name = "pro".into();
        other.model = "deepseek-v4-pro".into();
        other.base_url = "https://api.deepseek.com".into();
        other.context_window = Some(128_000);
        st.book.upsert(other);
        st.set_view(running());

        st.select("m2");
        assert_eq!(st.selected.as_deref(), Some("m2"));
        assert_eq!(st.win, "128000", "the card loads *that* record's numbers");
        assert_eq!(st.book.active.as_deref(), Some("m1"), "picking switched nothing");

        st.win = "200000".into();
        assert!(st.dirty_for("m2"));
        let applied = st.commit_to("m2").expect("valid");
        assert!(applied.is_none(), "a record that is not in force is saved, not applied");
        assert_eq!(st.book.by_id("m2").unwrap().context_window, Some(200_000));
        assert_eq!(st.book.active.as_deref(), Some("m1"), "and it is still not the running one");

        // The record in force is the one that produces an edit (a live swap).
        st.select("m1");
        st.temp = "0.9".into();
        let applied = st.commit_to("m1").expect("valid");
        assert!(applied.is_some(), "editing the running model applies it");
    }

    /// The draft is a draft: typing changes nothing on disk, closing without 确定 puts it back, and
    /// 确定 sends all four values in one edit. Every half of that is a separate promise, and the panel
    /// is only honest if none of them is skipped.
    #[test]
    fn the_popup_draft_is_a_cancel_until_confirm() {
        let mut st = ModelUi::new();
        seed(&mut st);
        st.set_view(running());
        assert!(!st.inline_dirty(), "the draft starts as what is running");

        st.win = "200000".into();
        st.mtok = "16000".into();
        st.temp = "0.9".into();
        st.thy = Thinking::High;
        assert!(st.inline_dirty(), "typing only dirties the draft");
        assert_eq!(
            st.book.by_id("m1").unwrap().context_window,
            None,
            "nothing has been written yet"
        );

        st.revert_inline();
        assert!(!st.inline_dirty(), "closing without 确定 puts it back");
        assert_eq!(st.win, "1000000");
        assert_eq!(st.thy, Thinking::Default);

        st.win = "200000".into();
        st.mtok = "16000".into();
        st.temp = "0.9".into();
        st.thy = Thinking::High;
        let edit = st.commit_all().expect("all four are valid");
        assert_eq!(edit.context_window, 200_000);
        assert_eq!(edit.max_tokens, 16_000);
        assert_eq!(edit.temperature, Some(0.9));
        assert_eq!(edit.thinking, Thinking::High);
        let rec = st.book.by_id("m1").unwrap().clone();
        assert_eq!(rec.context_window, Some(200_000));
        assert_eq!(rec.thinking, Thinking::High);
    }

    /// A bad number must not be rounded, dropped, or written: the message explains it, the typo stays
    /// in the box so it can be fixed, and the record is untouched.
    #[test]
    fn a_bad_number_keeps_the_draft_and_says_why() {
        let mut st = ModelUi::new();
        seed(&mut st);
        st.set_view(running());
        // `1e6` is a fine number here (it parses as 1000000); only a word is not.
        st.win = "128 kilo".into();
        assert!(st.commit_all().is_none());
        assert_eq!(st.win, "128 kilo", "the draft stays as typed");
        assert!(st.notice.as_ref().is_some_and(|(_, ok)| !*ok), "and it says why");
        assert_eq!(st.book.by_id("m1").unwrap().context_window, None, "nothing was written");
    }

    /// The bug behind "改不了数字": the field edited a **clone** of the draft and copied it back only
    /// on focus loss, so the next frame refilled the box from the old string and every keystroke was
    /// thrown away. Drive a real key into a real `TextEdit` and check the caller's string moved.
    #[test]
    fn typing_reaches_the_draft_instead_of_being_thrown_away() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut draft = String::from("32000");
        let id = Id::new("aox-test-field");

        let run = |ctx: &egui::Context, events: Vec<egui::Event>, draft: &mut String| {
            let mut input = egui::RawInput::default();
            input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 200.0)));
            input.events = events;
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let _ = compact_field(ui, &mut *draft, 120.0, "aox-test-field");
                });
            });
        };

        // Focus has to be requested before the widget exists, and a text event only means something on
        // the frame *after* the field has it.
        ctx.memory_mut(|m| m.request_focus(id));
        run(&ctx, vec![], &mut draft);
        run(&ctx, vec![egui::Event::Text("4".into())], &mut draft);

        assert_ne!(draft, "32000", "a keystroke has to land in the caller's string");
        assert!(draft.contains('4'), "got {draft:?}");
    }
}
