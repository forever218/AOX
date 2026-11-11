//! The workspace view: a read-only file browser for the conversation's workspace, plus this turn's edit list...
//! # What it is for.
//! Every other window in this app answers "what is the agent *doing*". This one answers "what did it *touch*" —...

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use egui::{
    Align, Color32, CornerRadius, FontId, Frame, Label, Layout, Margin, Pos2, RichText, ScrollArea,
    Sense, Stroke, Ui, Vec2,
};

use crate::gwin::{self, Drive, Gwin};
use crate::syntax;
use crate::theme::{self, P};

// Metrics

/// Width of the left column. Fixed rather than draggable: the content is a file tree whose rows are elided...
const TREE_W: f32 = 292.0;
/// Height of one row in either list (change list and tree use the same value, so the two read as
/// one column of rows rather than two lists that happen to be stacked).
const ROW_H: f32 = 25.0;
/// Height of a section heading.
const HEAD_H: f32 = 28.0;
/// Indent per tree level.
const INDENT: f32 = 13.0;
/// Line height in the diff pane.
const DIFF_LINE_H: f32 = 18.0;
/// Initial and minimum window size.
const MIN_W: f32 = 780.0;
const MIN_H: f32 = 470.0;
/// A file larger than this is not read at all — the pane says so instead.
const MAX_READ: u64 = 2 * 1024 * 1024;
/// Most entries drawn for one directory. A folder with 40 000 files in it would otherwise lay out
/// 40 000 rows every frame; the cut is silent because the tree is a *view*, not an inventory.
const MAX_ENTRIES: usize = 500;
/// Recursion limit for the tree walk. Guards against a symlink loop (Windows junctions can be
/// cyclic) — a view that recurses forever is a hung window.
const MAX_DEPTH: u16 = 24;
/// Directories never drawn. Not a filter on what the agent may touch — purely "these are never the
/// answer to 'where is the code'", and each one can contain tens of thousands of entries.
const DIRS_SKIP: [&str; 6] = [".git", "node_modules", "target", "__pycache__", ".venv", "venv"];
/// How often the selected file is re-read while a turn runs.
const LIVE_MS: u64 = 400;

// Data

/// One write the model made, exactly as the tool reported it.
#[derive(Debug, Clone)]
pub struct Edit {
    /// `(added, removed)` lines. `None` when the previous version was not text, so no diff could be
    /// computed — the file still changed, which is why this is an `Option` and not a `(0, 0)`.
    pub counts: Option<(usize, usize)>,
    /// The diff, one line per entry, each prefixed `+` / `-` / space (`⋯` = elided). Rendered by the
    /// tool while both versions of the file were still in hand (see `ToolOutput::diff_detail`).
    pub diff: Option<String>,
}

/// One file, with every write this turn made to it.
#[derive(Debug, Clone)]
pub struct FileEdit {
    pub path: PathBuf,
    pub edits: Vec<Edit>,
}

impl FileEdit {
    /// Total added lines, or `None` when no write to this file had a computable diff.
    fn added(&self) -> Option<usize> {
        let mut any = false;
        let mut n = 0;
        for e in &self.edits {
            if let Some((a, _)) = e.counts {
                any = true;
                n += a;
            }
        }
        any.then_some(n)
    }

    fn removed(&self) -> Option<usize> {
        let mut any = false;
        let mut n = 0;
        for e in &self.edits {
            if let Some((_, r)) = e.counts {
                any = true;
                n += r;
            }
        }
        any.then_some(n)
    }

    fn has_diff(&self) -> bool {
        self.edits.iter().any(|e| e.diff.is_some())
    }
}

/// Which half of the right pane is showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// The file as it is on disk now.
    #[default]
    File,
    /// The diff that got it there.
    Diff,
}

/// One row of the flattened tree.
struct Row {
    path: PathBuf,
    name: String,
    dir: bool,
    depth: u16,
    open: bool,
    /// This turn wrote this file.
    edited: bool,
}

/// The loaded file in the right pane, with the galley already laid out.
struct Loaded {
    path: PathBuf,
    /// `(len, mtime nanos)`, or `None` when the file does not exist. Compared against the disk to
    /// decide whether to re-read.
    stamp: Option<(u64, u128)>,
    /// `None` with `note` set = nothing to draw, and the note says why.
    galley: Option<Arc<egui::Galley>>,
    note: Option<&'static str>,
}

pub struct WsView {
    /// The window shell, shared verbatim with the other independent windows.
    gw: Gwin,

    // ── pushed in by the main window ── The workspace root of the conversation being chatted in. `None` =...
    root: Option<PathBuf>,
    /// This turn's edits, grouped by file, in first-touch order
    files: Vec<FileEdit>,
    /// Set whenever `root` or `files` changes, so the tree is rebuilt on the next frame instead of
    /// on every one.
    dirty: bool,

    // view state
    rows: Vec<Row>,
    expanded: BTreeSet<PathBuf>,
    sel: Option<PathBuf>,
    mode: Mode,
    open: Option<Loaded>,
    /// Set on the first frame after the window opens: pick the first edited file, so opening the
    /// window answers "what did it change" without a click.
    fresh: bool,
}

impl Default for WsView {
    fn default() -> Self {
        Self {
            gw: Gwin::default(),
            root: None,
            files: Vec::new(),
            dirty: true,
            rows: Vec::new(),
            expanded: BTreeSet::new(),
            sel: None,
            mode: Mode::default(),
            open: None,
            fresh: true,
        }
    }
}

// Shared state

pub type Shared = Arc<Mutex<WsView>>;

pub fn new_shared() -> Shared {
    Arc::new(Mutex::new(WsView::default()))
}

/// Take the lock. A poisoned mutex is recovered rather than propagated: one window panicking must
/// not take the rest of the program with it (same policy as `activity::lock`).
pub fn lock(a: &Shared) -> MutexGuard<'_, WsView> {
    a.lock().unwrap_or_else(|e| e.into_inner())
}

/// Push the current workspace root in. Called every frame; it is a `PathBuf` compare and a lock, the
/// same cost the other two windows already pay for their per-frame feeds.
pub fn set_root(a: &Shared, root: Option<PathBuf>) {
    let mut st = lock(a);
    if st.root == root {
        return;
    }
    st.root = root;
    st.dirty = true;
    // A new root means the old selection points at a different tree, so drop it. The expanded set
    // goes too: those paths belonged to the previous workspace.
    st.sel = None;
    st.open = None;
    st.expanded.clear();
}

/// Per-frame feed: whether a turn is in flight. Drives the header's dot and the live re-read of the
/// file being written — nothing else in this window moves.
pub fn tick(a: &Shared, busy: bool) {
    lock(a).gw.busy = busy;
}

/// Forget everything that belonged to the previous conversation. The tree itself is kept (the
/// workspace root usually did not change), only the edit ledger and the selection go.
pub fn reset(a: &Shared) {
    let mut st = lock(a);
    st.files.clear();
    st.dirty = true;
    st.sel = None;
    st.open = None;
    st.fresh = true;
}

/// A turn is starting: last turn's edits are no longer "this turn's". They are dropped rather than archived —...
pub fn begin_turn(a: &Shared) {
    let mut st = lock(a);
    st.files.clear();
    st.dirty = true;
}

/// Record one write. `path` is the file the tool wrote; `counts`/`diff` come straight off the
/// result (see `ToolOutput`).
pub fn note_edit(
    a: &Shared,
    path: PathBuf,
    counts: Option<(usize, usize)>,
    diff: Option<String>,
) {
    let mut st = lock(a);
    let k = key(&path);
    let e = Edit { counts, diff };
    match st.files.iter_mut().find(|f| key(&f.path) == k) {
        Some(f) => f.edits.push(e),
        None => st.files.push(FileEdit { path, edits: vec![e] }),
    }
    st.dirty = true;
}

// The window

fn ws_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("aox-wsview")
}

/// The window title. Used in two places — building the viewport, and later finding the HWND back by
/// title to ask DWM for rounded corners — so there must be exactly one copy.
const WINDOW_TITLE: &str = "AOX Workspace";

/// Open the window (idempotent; the settings entry is not a toggle — see [`Gwin::open`]).
pub fn open(ctx: &egui::Context, a: &Shared) {
    lock(a).gw.open(ctx);
}

/// Open the window directly, skipping the "already open" check — used by the demo preview.
pub fn force_open(a: &Shared) {
    lock(a).gw.force_open();
}

/// Unfold one directory. Only the demo uses it today; it is also the natural hook for a future
/// "reveal this file", which is why it takes a path rather than an index.
pub fn expand(a: &Shared, dir: PathBuf) {
    let mut st = lock(a);
    if st.expanded.insert(dir) {
        st.dirty = true;
    }
}

pub fn is_open(a: &Shared) -> bool {
    lock(a).gw.is_open()
}

/// Land the right pane on one file, optionally already in diff mode.
pub fn select(a: &Shared, path: PathBuf, mode: Mode) {
    let mut st = lock(a);
    st.sel = Some(path);
    st.mode = mode;
}

/// Called once per frame by the main window: re-declare the viewport (egui viewports are
/// declarative; one that is not declared gets reclaimed).
pub fn sync(ctx: &egui::Context, a: &Shared) {
    let mut st = lock(a);
    if !st.gw.open {
        drop(st);
        gwin::want(gwin::WIN_WSVIEW, false);
        return;
    }
    let size = gwin::initial_size(ctx);
    let size = Vec2::new(size.x.max(MIN_W), size.y.max(MIN_H));
    // Offset further than the other two windows, so all three can be open at once without landing
    // on top of each other (see `Gwin::builder`'s note on `shift`).
    let b = st.gw.builder(ctx, WINDOW_TITLE, size, Vec2::new(MIN_W, MIN_H), Vec2::new(92.0, 68.0));

    let a2 = a.clone();
    drop(st);
    ctx.show_viewport_deferred(ws_id(), b, move |ctx, _| window(ctx, &a2));
}

fn window(ctx: &egui::Context, shared: &Shared) {
    let mut st = lock(shared);

    st.gw.prelude(ctx, WINDOW_TITLE);

    let frame = egui::Frame::NONE.fill(P::panel());
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        // Child and main viewports share one `egui::Context`, so widget Ids are not automatically
        // prefixed with viewport info.
        ui.push_id("aox-wsview-window", |ui| body(ui, ctx, &mut st));
    });

    let d = st.gw.step();
    let open = st.gw.open;
    let busy = st.gw.busy;
    drop(st);
    // `busy: false` on purpose — see the module docs.
    gwin::drive(ctx, gwin::WIN_WSVIEW, Drive { busy: false, closing: d.closing });
    if open && busy {
        // The one live thing: the file being written, re-read on this window's own clock. A repaint
        // request for this viewport alone does not wake the main window.
        ctx.request_repaint_after(Duration::from_millis(LIVE_MS));
    }
}

fn body(ui: &mut Ui, ctx: &egui::Context, st: &mut WsView) {
    // The whole window's rect, taken before anything is put into the ui: `max_rect()` shrinks with
    // every allocate, and by the end only a sliver at the bottom would be left.
    let full = ui.max_rect();

    let info = header_info(st);
    gwin::header(ui, ctx, &mut st.gw, aox::i18n::t("Workspace"), &info);

    st.settle();

    let h = ui.available_height();
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;

        ui.vertical(|ui| {
            ui.set_width(TREE_W);
            ui.set_height(h);
            left_pane(ui, st);
        });

        // The divider between the two columns: one hairline, the same one the drawer uses.
        let (r, _) = ui.allocate_exact_size(Vec2::new(1.0, h), Sense::hover());
        ui.painter().rect_filled(r, CornerRadius::ZERO, P::border());

        ui.vertical(|ui| {
            ui.set_width(ui.available_width());
            ui.set_height(h);
            right_pane(ui, st);
        });
    });

    // Overlaid last, so the border hot-zones beat the top bar's drag zone (see `gwin`).
    gwin::resize_handles(ui, ctx, full);
    gwin::outline(ui, full);
}

/// The header's one-line readout. Built here rather than inside `gwin::header`: the shell has no
/// idea what a given window is measuring.
fn header_info(st: &WsView) -> String {
    if st.files.is_empty() {
        return aox::i18n::t("no edits this turn").to_string();
    }
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut known = false;
    for f in &st.files {
        if let Some(a) = f.added() {
            added += a;
            known = true;
        }
        if let Some(r) = f.removed() {
            removed += r;
            known = true;
        }
    }
    if known {
        aox::tf!("{} files · +{} −{}", st.files.len(), added, removed)
    } else {
        aox::tf!("{} files", st.files.len())
    }
}

impl WsView {
    /// Bring derived state up to date: rebuild the tree when it is stale, load the selected file,
    /// and — on the first frame after opening — pick something to show.
    fn settle(&mut self) {
        if self.dirty {
            self.dirty = false;
            self.rebuild();
        }
        if self.fresh {
            self.fresh = false;
            if self.sel.is_none() {
                if let Some(f) = self.files.first() {
                    self.sel = Some(f.path.clone());
                    self.mode = Mode::Diff;
                    self.open = None;
                }
            }
        }
    }

    fn rebuild(&mut self) {
        self.rows.clear();
        let Some(root) = self.root.clone() else {
            return;
        };
        let edited: BTreeSet<String> = self.files.iter().map(|f| key(&f.path)).collect();
        walk(&root, 0, &self.expanded, &edited, &mut self.rows);
    }

    /// The file record for the current selection, if this turn touched it.
    fn current_edit(&self) -> Option<&FileEdit> {
        let sel = self.sel.as_ref()?;
        let k = key(sel);
        self.files.iter().find(|f| key(&f.path) == k)
    }

    /// The selection, as a path relative to the workspace root (for display).
    fn sel_label(&self) -> String {
        match (&self.sel, &self.root) {
            (Some(p), Some(r)) => rel(r, p),
            (Some(p), None) => p.display().to_string(),
            (None, _) => String::new(),
        }
    }
}

/// Walk one directory into rows. Only expanded directories are descended into, so the cost is
/// proportional to what is on screen rather than to the size of the workspace.
fn walk(
    dir: &Path,
    depth: u16,
    expanded: &BTreeSet<PathBuf>,
    edited: &BTreeSet<String>,
    out: &mut Vec<Row>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let p = e.path();
        if is_dir {
            if DIRS_SKIP.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
                continue;
            }
            dirs.push((name, p));
        } else {
            files.push((name, p));
        }
    }
    // Directories first, then files; case-insensitive within each, because a case-sensitive sort
    // puts every capitalised name above every lowercase one and the tree stops reading alphabetically.
    dirs.sort_by_key(|(n, _)| n.to_lowercase());
    files.sort_by_key(|(n, _)| n.to_lowercase());

    let mut n = 0usize;
    for (is_dir, (name, p)) in dirs
        .into_iter()
        .map(|x| (true, x))
        .chain(files.into_iter().map(|x| (false, x)))
    {
        if n >= MAX_ENTRIES {
            break;
        }
        n += 1;
        let open = is_dir && expanded.contains(&p);
        let touched = !is_dir && edited.contains(&key(&p));
        out.push(Row { path: p.clone(), name, dir: is_dir, depth, open, edited: touched });
        if open {
            walk(&p, depth + 1, expanded, edited, out);
        }
    }
}

// Left pane

fn left_pane(ui: &mut Ui, st: &mut WsView) {
    ui.spacing_mut().item_spacing = Vec2::ZERO;

    // What a click asked for, applied after the borrows end: the draw path only reads `st`, and
    // collecting the requests keeps it that way (the same shape `App::drawer` uses).
    let mut pick: Option<(PathBuf, Mode)> = None;
    let mut toggle: Option<PathBuf> = None;

    Frame::NONE
        .inner_margin(Margin { left: 12, right: 8, top: 10, bottom: 10 })
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            let total = ui.available_height();

            if !st.files.is_empty() {
                section_head(ui, &aox::tf!("Changed this turn · {}", st.files.len()));
                // The change list gets at most 40% of the column: enough for a normal turn's four
                // or five files without ever squeezing the tree out of existence.
                let want = st.files.len() as f32 * ROW_H;
                let cap = (total * 0.40).max(ROW_H * 2.0);
                let h = want.min(cap);
                ScrollArea::vertical()
                    .id_salt("aox-ws-changes")
                    .max_height(h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for f in &st.files {
                            let label = rel(st.root.as_deref().unwrap_or(Path::new("")), &f.path);
                            let on = st.sel.as_deref() == Some(f.path.as_path());
                            if change_row(ui, &label, f.added(), f.removed(), on).clicked() {
                                pick = Some((f.path.clone(), Mode::Diff));
                            }
                        }
                    });
                // A hairline between the two blocks, so "this turn" and "the whole tree" do not read
                // as one long list.
                let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 13.0), Sense::hover());
                ui.painter().hline(
                    r.x_range(),
                    r.center().y,
                    Stroke::new(1.0, P::border()),
                );
            }

            section_head(ui, aox::i18n::t("Files"));
            if st.root.is_none() {
                hint(ui, aox::i18n::t("No workspace root for this conversation."));
                return;
            }
            let rest = ui.available_height().max(ROW_H);
            ScrollArea::vertical()
                .id_salt("aox-ws-tree")
                .max_height(rest)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if st.rows.is_empty() {
                        hint(ui, aox::i18n::t("The directory is empty."));
                    }
                    for row in &st.rows {
                        let on = !row.dir && st.sel.as_deref() == Some(row.path.as_path());
                        let resp = tree_row(ui, row, on);
                        if resp.clicked() {
                            if row.dir {
                                toggle = Some(row.path.clone());
                            } else {
                                pick = Some((row.path.clone(), Mode::File));
                            }
                        }
                    }
                });
        });

    if let Some(p) = toggle {
        if !st.expanded.remove(&p) {
            st.expanded.insert(p);
        }
        st.dirty = true;
    }
    if let Some((p, m)) = pick {
        if st.sel.as_deref() != Some(p.as_path()) {
            st.open = None;
        }
        st.sel = Some(p);
        st.mode = m;
    }
}

/// A section heading: a small faint label, plus the one-pixel rule under it that closes the block.
fn section_head(ui: &mut Ui, text: &str) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), HEAD_H), Sense::hover());
    let g = ui.painter().layout_no_wrap(
        text.to_owned(),
        FontId::proportional(12.0),
        P::text_faint(),
    );
    ui.painter().galley(
        Pos2::new(r.left(), r.center().y - g.size().y * 0.5),
        g,
        P::text_faint(),
    );
}

/// One row of the change list: `path` on the left, `+N −N` on the right.
fn change_row(
    ui: &mut Ui,
    label: &str,
    added: Option<usize>,
    removed: Option<usize>,
    on: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    if on {
        p.rect_filled(rect, CornerRadius::same(theme::R0), P::active_fill());
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(theme::R0), P::hover_fill());
    }

    // The counts are measured first, because the name is elided against what is left: a path is the part that can...
    let mut parts: Vec<(String, Color32)> = Vec::new();
    if let Some(a) = added {
        parts.push((format!("+{a}"), P::add_green()));
    }
    if let Some(r) = removed {
        parts.push((format!("-{r}"), P::del_red()));
    }
    let mut right = rect.right() - 6.0;
    let mut drawn = false;
    for (text, color) in parts.into_iter().rev() {
        let g = p.layout_no_wrap(text, theme::mono_font(11.5), color);
        right -= g.size().x;
        p.galley(Pos2::new(right, rect.center().y - g.size().y * 0.5), g, color);
        right -= 6.0;
        drawn = true;
    }
    // A write with no computable diff (the previous version was binary) draws neither half: saying
    // nothing is honest, whereas "+0 −0" would read as "this write changed nothing".
    if drawn {
        right -= 2.0;
    }
    let max_w = (right - rect.left() - 8.0).max(30.0);
    let fg = if on { P::text() } else { P::text_dim() };
    let g = gwin::elide(ui, label, FontId::proportional(13.0), fg, max_w);
    p.galley(
        Pos2::new(rect.left() + 8.0, rect.center().y - g.size().y * 0.5),
        g,
        fg,
    );
    resp
}

/// One row of the tree: indent, a caret for directories, a name, and a dot for a file this turn
/// wrote.
fn tree_row(ui: &mut Ui, row: &Row, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    if on {
        p.rect_filled(rect, CornerRadius::same(theme::R0), P::active_fill());
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(theme::R0), P::hover_fill());
    }

    let x = rect.left() + 6.0 + row.depth as f32 * INDENT;
    let cy = rect.center().y;
    let fg = if row.dir {
        P::text()
    } else if on {
        P::text()
    } else {
        P::text_dim()
    };

    // The caret: a small triangle, pointing right when shut and down when open. Only directories get
    // one — a file that is not open cannot be opened.
    if row.dir {
        let c = Pos2::new(x + 4.0, cy);
        let (a, b, d) = if row.open {
            (Vec2::new(-3.4, -1.6), Vec2::new(3.4, -1.6), Vec2::new(0.0, 2.4))
        } else {
            (Vec2::new(-1.6, -3.4), Vec2::new(-1.6, 3.4), Vec2::new(2.4, 0.0))
        };
        p.add(egui::Shape::convex_polygon(
            vec![c + a, c + b, c + d],
            P::text_faint(),
            Stroke::NONE,
        ));
    }

    let name_x = x + 14.0;
    // A written file gets a dot in the right margin.
    let mut right = rect.right() - 6.0;
    if row.edited {
        right -= 6.0;
        p.circle_filled(Pos2::new(right, cy), 2.6, P::add_green());
        right -= 10.0;
    }
    let max_w = (right - name_x).max(30.0);
    let font = if row.dir {
        FontId::proportional(13.0)
    } else {
        theme::mono_font(12.5)
    };
    let g = gwin::elide(ui, &row.name, font, fg, max_w);
    p.galley(Pos2::new(name_x, cy - g.size().y * 0.5), g, fg);
    resp
}

/// A dim line of explanation, for the states that have nothing to draw.
fn hint(ui: &mut Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).color(P::text_faint()).size(12.5));
    ui.add_space(8.0);
}

// Right pane

fn right_pane(ui: &mut Ui, st: &mut WsView) {
    ui.spacing_mut().item_spacing = Vec2::ZERO;

    let Some(_) = st.sel else {
        Frame::NONE
            .inner_margin(Margin { left: 22, right: 22, top: 18, bottom: 18 })
            .show(ui, |ui| {
                ui.label(
                    RichText::new(aox::i18n::t("Pick a file on the left."))
                        .color(P::text_faint())
                        .size(13.0),
                );
            });
        return;
    };

    // the path bar
    let has_diff = st.current_edit().map(|f| f.has_diff()).unwrap_or(false);
    let mut want_mode: Option<Mode> = None;
    Frame::NONE
        .inner_margin(Margin { left: 16, right: 12, top: 9, bottom: 9 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let label = st.sel_label();
                let right_reserved = if has_diff { 150.0 } else { 8.0 };
                let max_w = (ui.available_width() - right_reserved).max(80.0);
                let g = gwin::elide(ui, &label, theme::mono_font(12.5), P::text_dim(), max_w);
                let (r, _) = ui.allocate_exact_size(g.size(), Sense::hover());
                ui.painter().galley(r.min, g, P::text_dim());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if has_diff {
                        // In right-to-left layout the File entry is added first and lands rightmost.
                        if gwin::pill(ui, aox::i18n::t("File"), st.mode == Mode::File).clicked() {
                            want_mode = Some(Mode::File);
                        }
                        if gwin::pill(ui, aox::i18n::t("Diff"), st.mode == Mode::Diff).clicked() {
                            want_mode = Some(Mode::Diff);
                        }
                    }
                });
            });
        });
    if let Some(m) = want_mode {
        st.mode = m;
    }

    // A hairline under the path bar, the same one the drawer uses between its blocks.
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::ZERO, P::border());

    match st.mode {
        Mode::Diff => diff_pane(ui, st),
        Mode::File => file_pane(ui, st),
    }
}

fn diff_pane(ui: &mut Ui, st: &WsView) {
    let Some(f) = st.current_edit() else {
        Frame::NONE
            .inner_margin(Margin { left: 22, right: 22, top: 16, bottom: 16 })
            .show(ui, |ui| hint(ui, aox::i18n::t("This file was not edited this turn.")));
        return;
    };
    let many = f.edits.len() > 1;
    let total = f.edits.len();
    ScrollArea::vertical()
        .id_salt("aox-ws-diff")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (i, e) in f.edits.iter().enumerate() {
                // Each write's diff is against the version the previous write left behind, so they are shown in order and never...
                if many {
                    edit_sep(ui, i + 1, total);
                }
                match e.diff.as_deref() {
                    Some(d) => diff_lines(ui, d),
                    None => {
                        Frame::NONE
                            .inner_margin(Margin { left: 14, right: 14, top: 10, bottom: 10 })
                            .show(ui, |ui| {
                                hint(
                                    ui,
                                    aox::i18n::t(
                                        "The previous version was not text, so there is no diff — the file was still written.",
                                    ),
                                );
                            });
                    }
                }
            }
            ui.add_space(14.0);
        });
}

/// `— 2 / 3 —`: the divider between two writes to the same file.
fn edit_sep(ui: &mut Ui, i: usize, total: usize) {
    ui.add_space(10.0);
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
    let text = aox::tf!("edit {} of {}", i, total);
    let g = ui.painter().layout_no_wrap(text, FontId::proportional(11.0), P::text_faint());
    // Measured before the galley is handed to the painter: `Painter::galley` takes the `Arc` by
    // value, so anything read off it afterwards is a use-after-move.
    let (gw, gh) = (g.size().x, g.size().y);
    let x = r.center().x - gw * 0.5;
    let cy = r.center().y;
    ui.painter().galley(Pos2::new(x, cy - gh * 0.5), g, P::text_faint());
    let xr = r.x_range();
    ui.painter().hline(egui::Rangef::new(xr.min, x - 10.0), cy, Stroke::new(1.0, P::border()));
    ui.painter().hline(
        egui::Rangef::new(x + gw + 10.0, xr.max),
        cy,
        Stroke::new(1.0, P::border()),
    );
    ui.add_space(4.0);
}

/// The diff itself, one tinted row per line.
fn diff_lines(ui: &mut Ui, diff: &str) {
    let w = ui.available_width();
    for line in diff.lines() {
        // The first character is the whole contract (see `diff_text`): `+` added, `-` removed, a space for context...
        let (fg, band) = match line.chars().next() {
            Some('+') => (P::add_green(), Some(tint(P::add_green()))),
            Some('-') => (P::del_red(), Some(tint(P::del_red()))),
            Some(' ') => (P::text_dim(), None),
            _ => (P::text_faint(), None),
        };
        let (r, _) = ui.allocate_exact_size(Vec2::new(w, DIFF_LINE_H), Sense::hover());
        if let Some(b) = band {
            ui.painter().rect_filled(r, CornerRadius::ZERO, b);
        }
        let g = ui.painter().layout_no_wrap(line.to_owned(), theme::mono_font(12.5), fg);
        ui.painter()
            .galley(Pos2::new(r.left() + 12.0, r.center().y - g.size().y * 0.5), g, fg);
    }
}

/// A 10% wash of `c`. Composited normally by `rect_filled`, so it reads as a tint of the panel
/// underneath rather than as a colour in its own right.
fn tint(c: Color32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 26)
}

fn file_pane(ui: &mut Ui, st: &mut WsView) {
    st.ensure_loaded(ui);

    let Some(l) = st.open.as_ref() else {
        return;
    };
    if let Some(note) = l.note {
        Frame::NONE
            .inner_margin(Margin { left: 22, right: 22, top: 16, bottom: 16 })
            .show(ui, |ui| hint(ui, note));
        return;
    }
    let Some(g) = l.galley.clone() else {
        return;
    };
    ScrollArea::both()
        .id_salt("aox-ws-file")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // A little air on the left so the line-number gutter does not touch the divider, and
            // enough at the bottom that the last line is not flush with the window edge.
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                // `Extend` rather than `Wrap`: code is laid out to its own width, and wrapping is what turns a 200-character line...
                ui.add(
                    Label::new(g)
                        .selectable(true)
                        .wrap_mode(egui::TextWrapMode::Extend),
                );
            });
            ui.add_space(16.0);
        });
}

impl WsView {
    /// Load the selected file if it is missing or has changed on disk.
    fn ensure_loaded(&mut self, ui: &Ui) {
        let Some(path) = self.sel.clone() else {
            self.open = None;
            return;
        };
        let stamp = stamp_of(&path);
        if let Some(l) = self.open.as_ref() {
            if l.path == path && l.stamp == stamp {
                return;
            }
        }

        let mut loaded = Loaded { path: path.clone(), stamp, galley: None, note: None };
        match stamp {
            None => loaded.note = Some(aox::i18n::t("This file no longer exists.")),
            Some((len, _)) if len > MAX_READ => {
                loaded.note = Some(aox::i18n::t("This file is too large to display."))
            }
            Some(_) => match std::fs::read_to_string(&path) {
                Ok(text) => loaded.galley = Some(build_galley(ui, &path, &text)),
                // Not UTF-8: a binary file, or a text file in a legacy encoding. Saying so is
                // honest; showing replacement characters would look like a rendering bug.
                Err(_) => loaded.note = Some(aox::i18n::t("Not a text file.")),
            },
        }
        self.open = Some(loaded);
    }
}

/// Lay a whole file out once, with a line-number gutter.
fn build_galley(ui: &Ui, path: &Path, text: &str) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    // No wrapping (see `file_pane`); the horizontal scroll axis exists for exactly this.
    job.wrap.max_width = f32::INFINITY;

    let base = egui::TextFormat {
        font_id: theme::mono_font(13.0),
        line_height: Some(13.0 * 1.55),
        ..Default::default()
    };
    // The gutter is as wide as the largest line number, so the code column starts at the same x on
    // every line of the file.
    let hint = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
    let lines = syntax::highlight(text, &hint);
    let gutter = lines.len().to_string().len();

    for (i, toks) in lines.iter().enumerate() {
        let mut fmt = base.clone();
        fmt.color = P::text_faint();
        job.append(&format!("{:>1$}  ", i + 1, gutter), 0.0, fmt);
        for (t, tok) in toks {
            let mut fmt = base.clone();
            if *tok != syntax::Tok::Plain {
                fmt.color = syntax::color(*tok);
            }
            if *tok == syntax::Tok::Comment {
                fmt.italics = true;
            }
            job.append(t, 0.0, fmt);
        }
        if i + 1 < lines.len() {
            job.append("\n", 0.0, base.clone());
        }
    }
    ui.painter().layout_job(job)
}

// Helpers

/// A path's identity for comparison.
fn key(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// `path` relative to `root`, for display. Falls back to the whole path when it is not under the
/// root (which happens when the workspace was switched while an edit was still listed).
fn rel(root: &Path, path: &Path) -> String {
    let rk = key(root);
    let pk = key(path);
    let rk = rk.trim_end_matches('/');
    if pk.len() > rk.len() + 1 && pk.starts_with(rk) && pk.as_bytes().get(rk.len()) == Some(&b'/') {
        // Slice the original string, not the lower-cased key: the display has to keep the real casing, and the two have...
        let skip = rk.chars().count() + 1;
        let tail: String = path.to_string_lossy().chars().skip(skip).collect();
        if !tail.is_empty() {
            // Shown in the path bar and as a tree label, so the separator has to be the one the
            // rest of the UI speaks — `/`, on every platform.
            return tail.replace('\\', "/");
        }
    }
    path.display().to_string()
}

/// `(len, mtime in nanos)`, or `None` when the file is gone.
fn stamp_of(p: &Path) -> Option<(u64, u128)> {
    let m = std::fs::metadata(p).ok()?;
    let t = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((m.len(), t))
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    /// A relative label is the common case and has to keep the file's real casing, not the
    /// lower-cased comparison key.
    #[test]
    fn rel_strips_the_root_and_keeps_case() {
        let root = Path::new("/work/App");
        assert_eq!(rel(root, Path::new("/work/App/src/Main.rs")), "src/Main.rs");
        // Windows-style separators on both sides
        assert_eq!(rel(Path::new("E:\\a"), Path::new("E:\\a\\b\\c.rs")), "b/c.rs");
    }

    /// Not under the root: the whole path, because a truncated one would be a lie.
    #[test]
    fn rel_falls_back_to_the_whole_path() {
        let p = Path::new("/elsewhere/x.rs");
        assert_eq!(rel(Path::new("/work"), p), p.display().to_string());
    }

    /// Case and separator differences must not make two spellings of one file look like two files —
    /// that is exactly the mismatch that would leave every edited file un-dotted in the tree.
    #[test]
    fn key_ignores_case_and_separators() {
        assert_eq!(key(Path::new("E:\\rustapp\\SRC\\main.rs")), key(Path::new("e:/rustapp/src/main.rs")));
    }

    /// The two counts are summed across writes but stay `None` when no write had a computable diff —
    /// "no number" and "zero" are different statements (see `Edit::counts`).
    #[test]
    fn counts_sum_but_stay_none_when_unknown() {
        let f = FileEdit {
            path: PathBuf::from("a.rs"),
            edits: vec![
                Edit { counts: Some((3, 1)), diff: None },
                Edit { counts: Some((2, 4)), diff: None },
            ],
        };
        assert_eq!(f.added(), Some(5));
        assert_eq!(f.removed(), Some(5));

        let g = FileEdit {
            path: PathBuf::from("b.bin"),
            edits: vec![Edit { counts: None, diff: None }],
        };
        assert_eq!(g.added(), None);
        assert_eq!(g.removed(), None);
    }
}
