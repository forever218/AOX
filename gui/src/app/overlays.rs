//! The modal layer: the one-field name dialog, the authorization card, the toast.
//!
//! All three float **over** everything and own their input in a way no other screen does -- the name
//! dialog swallows Enter/Esc, the auth card holds a reply channel the agent is blocked on, and the toast
//! runs on its own wall clock. Grouping them is what lets "is a modal open?" be answered by reading one
//! file: `App` holds the state, this file decides what it looks like.

use super::*;

/// How long a toast stays up, in seconds. Also the window in which a second delete merges into it.
pub(super) const TOAST_SECS: f32 = 6.0;

/// The last stretch of that, during which it fades instead of vanishing mid-read.
pub(super) const TOAST_FADE_SECS: f32 = 0.6;

/// Corner radius of the toast. Deliberately larger than a card's: the notice is a small rounded
/// capsule, not a piece of page furniture.
pub(super) const TOAST_RADIUS: u8 = 14;

/// The name dialog's state while it is open (see [`App::name_dialog`]).
pub(super) struct NameDialog {
    /// Which question is being asked. The shapes below are shared by all three.
    pub(super) mode: NameDialogMode,
    /// What the user has typed, bound straight to the text field.
    pub(super) name: String,
    /// Why the last attempt was refused. Cleared as soon as the name changes, so the message can
    /// never outlive the input it was about.
    pub(super) error: Option<String>,
    /// Whether the field has been handed the caret yet. It has to be done **once**, on the frame the
    /// dialog appears: asking every frame would fight the user from the moment they clicked anywhere
    /// else in the dialog, and `request_focus` on an already-focused field is not idempotent in the
    /// way that would fix it -- it would just keep pulling the caret back.
    pub(super) focused: bool,
}

/// Which question the shared shell is asking.
///
/// The type is named after none of its three jobs on purpose: it asks about a workspace's folder, a
/// workspace's label and a conversation's label, and a name taken from one of them is a name that
/// gets read instead of the code.
pub(super) enum NameDialogMode {
    /// Make a folder under `<app root>/workplace/` and register it.
    New,
    /// Re-label an **existing** folder. The folder itself is never moved — see
    /// [`aox::space::Registry::rename_workspace`] — which is why the path below is read from here
    /// rather than computed from the typed name.
    Rename { root: PathBuf },
    /// Re-label one **conversation**. Nothing on disk is renamed: the id (which is the filename) is
    /// left alone and a row carrying the new title is appended to the conversation's own file. So
    /// there is no path to spell out here, and the folder-name rule does not apply either — a title
    /// is only a label, so `notes/2026` and `v1.2.` are both fine (see [`check_conv_title`]).
    RenameConversation { id: String },
}

impl NameDialogMode {
    /// A stable tag for the `Modal` id. Three questions share one body, and a `Modal` keeps its state
    /// under its id — one id for all three would let a rename inherit whatever the previous question
    /// left in the field.
    pub(super) fn tag(&self) -> u8 {
        match self {
            Self::New => 0,
            Self::Rename { .. } => 1,
            Self::RenameConversation { .. } => 2,
        }
    }
}

impl NameDialog {
    pub(super) fn new() -> Self {
        Self { mode: NameDialogMode::New, name: String::new(), error: None, focused: false }
    }

    /// Open for a workspace rename, with the field **pre-filled**: the current name is the thing being
    /// edited, so an empty field would make the common case (fix one character) the awkward one.
    pub(super) fn renaming(root: PathBuf, current: String) -> Self {
        Self { mode: NameDialogMode::Rename { root }, name: current, error: None, focused: false }
    }

    /// Open for a conversation rename. Pre-filled for the same reason as the workspace case — and with
    /// the title the row is *showing*, so clearing it and saving is visibly "back to the first
    /// message" rather than "back to something invisible".
    pub(super) fn renaming_conversation(id: String, current: String) -> Self {
        Self {
            mode: NameDialogMode::RenameConversation { id },
            name: current,
            error: None,
            focused: false,
        }
    }

    /// The folder being renamed, or `None` in the other two modes.
    pub(super) fn rename_root(&self) -> Option<&Path> {
        match &self.mode {
            NameDialogMode::Rename { root } => Some(root),
            _ => None,
        }
    }

    /// The conversation being renamed, or `None` in the other two modes.
    pub(super) fn conv_id(&self) -> Option<&str> {
        match &self.mode {
            NameDialogMode::RenameConversation { id } => Some(id),
            _ => None,
        }
    }
}

/// What the user did with the workspace dialog this frame.
pub(super) enum DialogAction {
    /// "Create" in the new-workspace mode, "Save" in the rename mode.
    Confirm,
    Cancel,
}

/// Draw the name dialog — "new workspace", "rename workspace" or "rename conversation". Returns
/// what the user asked for, if anything.
///
/// A free function taking the state by `&mut` rather than a method on `App`: the body needs no
/// `App` state at all (`app_root` comes in as a parameter), so keeping it out of the `impl` is what
/// lets the caller hold the dialog's state apart from `self` while it is open.
///
/// The three modes share one body on purpose. They ask the same kind of question — "what should
/// this thing be called?" — and differ only in the words around the field (the title, the one-line
/// blurb, the button's verb) and in **which rule validates it and what the answer goes to**. Three
/// dialogs would duplicate the input handling, the Enter key, the caret handoff and the error slot —
/// i.e. every part of this that can have a bug in it.
pub(super) fn name_dialog_modal(
    ctx: &egui::Context,
    st: &mut NameDialog,
    app_root: Option<&Path>,
) -> Option<DialogAction> {
    // Which of the three questions this is, read once here instead of five times below. Booleans and
    // a `u8` rather than a held reference to the mode: the body needs `&mut st.name` a few lines down,
    // and a borrow of `st` living across that is exactly what the compiler refuses.
    let creating = matches!(st.mode, NameDialogMode::New);
    let renaming_conversation = st.conv_id().is_some();
    let mut action = None;
    // A separate id per question: all three share one body, and a `Modal` keeps its state under this
    // id, so one id would let a rename inherit whatever the previous question left in the field.
    let resp = egui::Modal::new(egui::Id::new(("aox-name", st.mode.tag())))
        .backdrop_color(P::scrim())
        .frame(
            theme::raised(theme::card_radius())
                .inner_margin(Margin::symmetric(22, 18)),
        )
        .show(ctx, |ui| {
            ui.set_width(430.0);
            // The words, the field's hint and the one sentence of explanation — the only things the
            // three questions do not share. Everything else (the field, the caret handoff, Enter, the
            // error slot, the buttons) is one body for a reason: it is all the part that can have a
            // bug in it.
            let (title, blurb, hint) = match &st.mode {
                NameDialogMode::New => (
                    "New workspace",
                    aox::i18n::t("A folder of its own, with its own conversations. Writes and deletes stay inside it."),
                    aox::i18n::t("Workspace name"),
                ),
                NameDialogMode::Rename { .. } => (
                    "Rename workspace",
                    aox::i18n::t("Only this label changes — the folder stays where it is. Clear the name to use the folder's own name."),
                    aox::i18n::t("Workspace name"),
                ),
                // The one thing worth saying here is that nothing on disk is renamed: the *row* is a
                // label, the file behind it keeps its name, and clearing the field is how you get the
                // automatic title back.
                NameDialogMode::RenameConversation { .. } => (
                    "Rename conversation",
                    aox::i18n::t("Only this row's label changes; the file keeps its name on disk. Clear it to go back to the first message."),
                    aox::i18n::t("Conversation name"),
                ),
            };
            ui.label(RichText::new(aox::i18n::t(title)).color(P::text()).size(16.0).strong());
            ui.add_space(5.0);
            // One line, and it says what the thing *is* rather than where it goes: in the workspace
            // modes the concrete path below answers "where" far better than prose can, and this screen
            // has room for exactly one sentence.
            //
            // In the workspace-rename case the sentence is doing real work rather than setting a tone.
            // The field holds a *name* and the box below holds a *folder*, and without saying so it is
            // perfectly reasonable to read this screen as "these two are kept in step" — which is the
            // one thing it does not do.
            ui.label(RichText::new(blurb).color(P::text_faint()).size(12.5));
            ui.add_space(12.0);

            // The field's height comes from **this margin**, not from a minimum size. A `TextEdit`'s
            // box is its text plus its margin, and singleline text is drawn aligned inside whatever
            // box it is handed -- so padding is the only way to get a taller field without the text
            // ending up stuck to the top edge. The default (`Margin::symmetric(4, 2)`) makes a ~23px
            // strip, which reads as cramped next to this dialog's 16px title and its buttons.
            let edit = ui.add(
                TextEdit::singleline(&mut st.name)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(11, 10))
                    .font(FontId::proportional(14.0))
                    .hint_text(
                        RichText::new(hint).color(P::quiet()).size(13.5),
                    ),
            );
            if !st.focused {
                edit.request_focus();
                // ...and put the caret at the **end of the pre-filled name**, explicitly.
                //
                // `request_focus` alone does not promise *where in the text* the caret lands.
                // `TextEdit` only falls back to "end of text" when it has no stored cursor to
                // resume from (`default_cursor_range`, consulted in egui's own `events`), and this
                // field keeps its state under one id across every opening of the dialog. So the
                // second time it is opened, the index a previous rename left behind is what the
                // caret resumes at -- and when that is 0 the caret sits *before* the pre-filled
                // name, which makes backspace do nothing and typing prepend: "rename this" turns
                // into "rename thisOld name". Written out here, the common case (adjust the tail of
                // the name) works on the first try, every time the dialog is opened.
                let mut state =
                    egui::TextEdit::load_state(ui.ctx(), edit.id).unwrap_or_default();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(st.name.chars().count()),
                )));
                state.store(ui.ctx(), edit.id);
                st.focused = true;
            }
            if edit.changed() {
                st.error = None;
            }

            // The path, spelled out. It means a different thing in each of the two modes that have
            // one -- the folder that *is* there vs. the folder that *will* be -- which is why the
            // arms stay separate instead of one expression with a flag inside it. The conversation
            // rename is the third state and has no path at all.
            ui.add_space(8.0);
            match (renaming_conversation, st.rename_root(), app_root) {
                // **Renaming a conversation**: nothing on disk moves -- the id keeps its filename and
                // the new label is a row inside the file -- so there is no directory to spell out
                // here. The same vertical space is reserved anyway, so the buttons sit in the same
                // place as in the other two questions.
                (true, _, _) => ui.add_space(31.0),
                // **Renaming**: the folder that is already there. Tied to no typed text, so the box
                // does not flicker as the name is edited — and it is shown even while the field is
                // blank, which is the state that means "use the folder's own name".
                (_, Some(ws), _) => {
                    let path = ws.display().to_string();
                    theme::well(8)
                        .inner_margin(Margin::symmetric(10, 7))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new(workplace_ui::elide_path(&path, 56))
                                    .font(theme::mono_font(12.5))
                                    .color(P::text_dim()),
                            );
                        });
                }
                // **Creating**: where the folder *will* land. The user is about to let the model write
                // inside this directory, so "which directory" is the one thing this screen must not
                // leave implied -- and the name typed above is only half of the answer.
                //
                // Drawn for an empty or unusable name too, showing just the parent: `workplace/`. The
                // space below the field is reserved in every case anyway (see the catch-all arm), and
                // an empty box was the one state where that reservation read as a hole in the dialog.
                // A box that is always there is also a box that never jumps.
                (_, None, Some(root)) => {
                    let base = root.join(aox::workplace::WORKPLACE_DIR);
                    let path = match check_ws_name(&st.name) {
                        Ok(name) => base.join(name),
                        Err(_) => base,
                    };
                    theme::well(8)
                        .inner_margin(Margin::symmetric(10, 7))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new(workplace_ui::elide_path(&path.display().to_string(), 56))
                                    .font(theme::mono_font(12.5))
                                    .color(P::text_dim()),
                            );
                        });
                }
                // The only case left: no known app root, so there is no path to spell out. The same
                // vertical space is reserved anyway, so the buttons sit in the same place in every
                // state this dialog can be in.
                _ => ui.add_space(31.0),
            }

            // Why the button would not do anything, shown **as it is typed** rather than only after
            // a submit. The alternative is a confirm button that silently ignores the click, and "I
            // pressed it and nothing happened" is the one thing this dialog must never do. The
            // submit-time failure (a registry that cannot be written) rides in the same slot: it is
            // about the same text, and it is dropped the moment that text changes.
            //
            // Nothing is said about an **empty** field: it starts that way, and a dialog that opens
            // complaining has already spent the user's attention on nothing. (An empty field is not
            // merely tolerated either -- in the rename mode it is a real answer, which is why nothing
            // here refuses it and the caller is left to read the meaning.)
            // A conversation title is validated by a **different rule** to a folder name: a title is
            // only a label, so separators and dots are all perfectly good in it. The one thing it
            // cannot be is longer than the row can show, and that is what `check_conv_title` says.
            let live = if st.name.trim().is_empty() {
                None
            } else if renaming_conversation {
                check_conv_title(&st.name).err()
            } else {
                check_ws_name(&st.name).err()
            };
            if let Some(e) = live.or_else(|| st.error.clone()) {
                ui.add_space(8.0);
                ui.label(RichText::new(e).color(P::text()).size(13.0));
            }

            ui.add_space(16.0);
            ui.horizontal(|ui| {
                // Enter in the name field does the same as the button. The standard test for it on a
                // single-line field: it **lost focus** this frame, and Enter is what took the focus
                // away (a bare `key_pressed` would also fire while the field still has it).
                //
                // The caret is handed straight back when that happens, **unconditionally**: a
                // single-line field surrenders focus on Enter, so after a refusal the obvious next
                // move -- fix the name, press Enter again -- would otherwise be typed into nothing.
                // When the confirm does succeed this is a no-op; the dialog is gone by the next frame.
                let entered = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if entered {
                    edit.request_focus();
                    action = Some(DialogAction::Confirm);
                }
                // The button is deliberately **not** disabled while the name is invalid: a control
                // that looks live and silently does nothing is the worst of both worlds, while
                // pressing it just runs the validation and says why. The live message above is what
                // makes that feel like an answer rather than a refusal.
                let verb = if creating { "Create" } else { "Save" };
                // Cancel left, the verb right -- one footer for every dialog, see `dialog_footer`.
                if let Some(yes) = dialog_footer(ui, |ui| primary_button(ui, aox::i18n::t(verb)).clicked()) {
                    action = Some(if yes { DialogAction::Confirm } else { DialogAction::Cancel });
                }
            });
        });

    // Clicking the backdrop or pressing Esc closes it -- the standard exit `egui::Modal` provides --
    // and that has to mean **cancel**: nothing was asked for, so nothing is written.
    if resp.should_close() {
        action.get_or_insert(DialogAction::Cancel);
    }
    action
}

/// Validate a typed workspace name and hand back the trimmed form.
///
/// The only place in the GUI that has to think about what a filename may contain, and it matters
/// more than it looks: the name becomes a **path segment**, so a separator or a `..` here would
/// silently point the whole workspace -- and every write the model makes under it -- somewhere else
/// than the dialog's path preview promised. The reserved characters are rejected on **every**
/// platform rather than only on Windows: "works on my machine" is a worse outcome than one slightly
/// stricter rule, and the registry has to stay readable by every build.
pub(super) fn check_ws_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(aox::i18n::t("Please give the workspace a name.").to_string());
    }
    // Structural, not "a character we dislike": a leading dot makes a hidden directory, and Windows
    // silently drops a **trailing dot**, so `"notes."` and `"notes"` would be one folder wearing two
    // different rows in the tree. Both get the same message -- the user's fix is identical in either
    // case, which is to type a plain folder name.
    //
    // A trailing **space** is deliberately not in this list: the trim above already dealt with it,
    // and refusing what `"notes "` becomes (`"notes"`, a perfectly good name) would be a puzzle
    // rather than a rule.
    let structural = name.starts_with('.') || name.ends_with('.');
    let illegal = name
        .chars()
        .any(|c| matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control());
    if structural || illegal {
        return Err(aox::i18n::t("That name cannot be used for a folder.").to_string());
    }
    Ok(name.to_string())
}

/// Validate a typed conversation title and hand back the trimmed form.
///
/// Deliberately **not** [`check_ws_name`]. A title never becomes a path: it is one row inside the
/// conversation's own file, so the separators, `?`, `"` and even a trailing dot are all perfectly
/// good in it — a thread called `notes/2026` or `v1.2.` is fine, and refusing those would just be a
/// folder rule leaking onto a label. What actually matters here is two other things:
///
/// - **Length.** The row draws the title on one line, and the store clamps what it reads back to
///   [`aox::store::TITLE_MAX_CHARS`] — so a longer title would be silently cut, and "what I typed
///   is not what got saved" is exactly the kind of quiet edit this dialog exists to prevent.
///   Refusing it here states the limit instead of applying it behind the user's back.
/// - **Control characters.** A tab or a stray `\r` would land in the persisted row and then in the
///   row's layout. A single-line field cannot be *typed* into one, but a paste can carry one.
///
/// An **empty** string is `Ok`, not an error: for a title, blank is a real answer — "go back to the
/// automatic title, the first message" — so refusing it is not this function's business. (The
/// caller is what reads a blank as that meaning; see [`App::rename_conversation`].)
pub(super) fn check_conv_title(raw: &str) -> Result<String, String> {
    let title = raw.trim();
    if title.chars().any(char::is_control) {
        return Err(aox::i18n::t("A conversation name cannot contain control characters.").to_string());
    }
    if title.chars().count() > aox::store::TITLE_MAX_CHARS {
        return Err(aox::tf!(
            "A conversation name can be at most {} characters.",
            aox::store::TITLE_MAX_CHARS
        ));
    }
    Ok(title.to_string())
}

// ─────────────────────────── "are you sure?" ───────────────────────────

/// A **destructive** action waiting on a yes.
///
/// Two things in this window can lose something the user put there, and until 2026-10-25 both fired
/// on a single click in a right-click menu: `Delete` on a conversation row, `Remove from AOX` on a
/// workspace group. A context menu is a place where a mis-aimed click costs nothing anywhere else in
/// this program, so the one irreversible pair gets a stop.
///
/// The wording is the contract, and the two are **not** the same promise softened:
///
/// | | what actually happens |
/// |---|---|
/// | conversation | the file is moved to the recycle bin — recoverable, and the dialog says so |
/// | workspace | only a registry entry goes away; not one byte on the device changes |
///
/// Which is why each variant carries only the field its sentence talks about, and why a new
/// destructive action must not reuse an existing arm: the text would then be lying.
pub(super) enum Confirm {
    /// `ToAgent::Recycle` on one conversation file. `title` is what the row was showing, so the
    /// dialog can name its subject.
    DeleteConversation { path: PathBuf, title: String },
    /// `App::forget_workspace` — unregister the root, touch nothing on disk.
    RemoveWorkspace { root: PathBuf },
}

/// The words of a confirmation: `(title, body, verb, subject)`.
///
/// Split out from [`confirm_modal`] so the promise each dialog makes can be **asserted** — these
/// sentences are a contract with the user (one says the file is recoverable, the other says nothing
/// on the device changes), and a future destructive action must not quietly reuse an arm and thereby
/// display a promise that does not apply to it.
pub(super) fn confirm_copy(what: &Confirm) -> (&'static str, &'static str, &'static str, String) {
    match what {
        // The path is not shown: the row's label is what the user recognises, and the file stem is a
        // timestamp. It is still carried, because it is what gets recycled.
        Confirm::DeleteConversation { title, path: _ } => (
            "Delete this conversation?",
            "Its file will be moved to the system recycle bin, so it can be restored if you need it.",
            "Delete",
            middle_elide(title, 52),
        ),
        Confirm::RemoveWorkspace { root } => (
            "Remove this workspace?",
            "This only removes the directory from AOX; nothing on this device is changed.",
            "Remove",
            workplace_ui::elide_path(&root.display().to_string(), 56),
        ),
    }
}

/// The confirmation box. `None` = still open, `Some(true)` = go ahead, `Some(false)` = dismissed.
///
/// A free function for the same reason as [`name_dialog_modal`]: the body needs no `App` state, so
/// leaving it out of the `impl` lets the caller hold the pending action apart from `self` while the
/// dialog is on screen.
pub(super) fn confirm_modal(ctx: &egui::Context, what: &Confirm) -> Option<bool> {
    let (title, body, verb, subject) = confirm_copy(what);

    let mut answer = None;
    let resp = egui::Modal::new(egui::Id::new("aox-confirm"))
        .backdrop_color(P::scrim())
        .frame(theme::raised(theme::card_radius()).inner_margin(Margin::symmetric(22, 18)))
        .show(ctx, |ui| {
            ui.set_width(430.0);
            // Not in `danger`: that hue is reserved for the one toggle that crosses every protection
            // (see `P::danger`). This is a stop, not an alarm -- the sentence under it is what tells
            // the user how bad it is, and for both of these the honest answer is "not very".
            ui.label(RichText::new(aox::i18n::t(title)).color(P::text()).size(16.0).strong());
            ui.add_space(6.0);
            ui.label(RichText::new(aox::i18n::t(body)).color(P::text_faint()).size(12.5));
            ui.add_space(10.0);
            // **Which one**. A confirmation that does not name its subject is the "…was that the one I
            // meant?" dialog; the row is gone from view the moment the menu closes.
            theme::well(8).inner_margin(Margin::symmetric(10, 7)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(subject)
                        .font(theme::mono_font(12.5))
                        .color(P::text_dim()),
                );
            });
            ui.add_space(18.0);
            // Same footer as every other dialog: Cancel on the left, the verb on the right.
            if let Some(yes) = dialog_footer(ui, |ui| primary_button(ui, aox::i18n::t(verb)).clicked()) {
                answer = Some(yes);
            }
        });

    // Esc / a click on the backdrop is a dismissal, like every other modal here: nothing was asked
    // for, so nothing happens.
    if resp.should_close() {
        answer.get_or_insert(false);
    }
    answer
}

impl App {
    /// The name dialog ("new workspace" from the panel's foot row, "rename workspace" from a
    /// group's right-click menu, or "rename conversation" from a conversation row's).
    ///
    /// The state is `take()`n out of `self.name_dialog` and put back at the end, rather than borrowed
    /// across the draw: the body needs `&mut self` (the app root, and on confirm the registry), and a
    /// `&mut` into `App::name_dialog` held over that is exactly the double borrow the compiler refuses.
    /// Taking it keeps one owner at a time without cloning the whole struct every frame.
    pub(super) fn name_dialog_overlay(&mut self, ctx: &egui::Context) {
        let Some(mut st) = self.name_dialog.take() else {
            return;
        };
        let root = self.app_root();
        let action = name_dialog_modal(ctx, &mut st, root.as_deref());
        match action {
            // Still open: put the state back, so the next frame draws the same text and keeps the
            // caret where the user left it. (Anything else would make the field feel like it resets
            // itself every frame.)
            None => self.name_dialog = Some(st),
            // Cancelled -- backdrop click, Esc, or the button. Nothing was written, so there is
            // nothing to clean up.
            Some(DialogAction::Cancel) => {}
            Some(DialogAction::Confirm) => {
                let res = match &st.mode {
                    NameDialogMode::New => self.create_workspace(&st.name, root.as_deref()),
                    // Blank is not refused here, it is the undo: the dialog says the folder's own name
                    // is what an empty field means, so it has to be what an empty field does.
                    NameDialogMode::Rename { root: ws } => {
                        self.rename_workspace(ws, Some(st.name.as_str()))
                    }
                    // Same shape as the workspace rename: whatever is in the field is the answer,
                    // because a **blank** field is the real answer here too ("go back to the first
                    // message"), and the caller is the one that reads the blank.
                    NameDialogMode::RenameConversation { id } => {
                        self.rename_conversation(id, Some(st.name.as_str()))
                    }
                };
                // Refused: the dialog **stays open** with the message, so the name can be fixed
                // without typing it all again. On success it closes and the field stays `None`.
                if let Err(e) = res {
                    st.error = Some(e);
                    self.name_dialog = Some(st);
                }
            }
        }
    }

    /// Draw the "are you sure?" box and, on a yes, perform the thing it asked about.
    ///
    /// The action is carried **in the dialog's own state** ([`Confirm`]) rather than re-derived from
    /// the current selection: between opening the dialog and answering it the tree can be refreshed,
    /// re-sorted or re-folded under the pointer, and a confirm that reads "the row I would delete now"
    /// would then delete something other than what its text named.
    pub(super) fn confirm_overlay(&mut self, ctx: &egui::Context) {
        let Some(what) = self.confirm.take() else { return };
        match confirm_modal(ctx, &what) {
            // Still open: hand it back so the next frame draws the same question.
            None => self.confirm = Some(what),
            // Dismissed — the backdrop, Esc, or Cancel.
            Some(false) => {}
            Some(true) => match what {
                Confirm::DeleteConversation { path, .. } => {
                    // Down the same channel, as the same single-item list, the model's own deletions
                    // take: the move is a blocking shell call made by the agent thread, and the toast
                    // plus the refresh then arrive as one `AgentEvent::Recycled` for both doors.
                    let _ = self.tx.send(ToAgent::Recycle(vec![path]));
                }
                Confirm::RemoveWorkspace { root } => self.forget_workspace(&root),
            },
        }
    }

    pub(super) fn auth_overlay(&mut self, ctx: &egui::Context) {
        let Some(card) = self.auth.take() else { return };
        let mut decision: Option<AuthDecision> = None;
        egui::Modal::new(egui::Id::new("aox-auth"))
            .backdrop_color(P::scrim())
            .show(ctx, |ui| {
            ui.set_width(480.0);
            ui.label(
                RichText::new(aox::i18n::t("Tool call needs authorization"))
                    .color(P::text())
                    .size(17.0)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(aox::i18n::t("The model wants to call a meta-tool that changes the system itself:"))
                    .color(P::text_faint())
                    .size(14.0),
            );
            ui.add_space(8.0);
            ui.label(RichText::new(&card.tool).color(P::ink()).size(16.0));
            ui.add_space(8.0);
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = 440.0;
            job.wrap.break_anywhere = true;
            job.append(
                &clamp_str(&card.args, 1600),
                0.0,
                egui::TextFormat {
                    font_id: theme::mono_font(14.0),
                    color: P::text_dim(),
                    ..Default::default()
                },
            );
            theme::well(8)
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.add(Label::new(job));
                });
            ui.add_space(12.0);
            // The keyboard shortcuts are identical to the CLI's: y allows once / a always allows
            // for this session / n denies.
            // A 250ms debounce is added: keys are ignored right after the dialog appears, so
            // residue from the previous message (its Enter, say) can't trigger it by accident.
            let settled = self.auth_shown_at.elapsed() > Duration::from_millis(250);
            if settled {
                if ui.input(|i| i.key_pressed(egui::Key::Y)) {
                    decision = Some(AuthDecision::Allow);
                }
                if ui.input(|i| i.key_pressed(egui::Key::A)) {
                    decision = Some(AuthDecision::AllowSession);
                }
                if ui.input(|i| i.key_pressed(egui::Key::N) || i.key_pressed(egui::Key::Escape)) {
                    decision = Some(AuthDecision::Deny);
                }
            }
            // Same rule as every dialog footer (Cancel left, the committing button right), written
            // out because this card has **three** buttons rather than a pair: denying stays on the
            // left where a mis-click costs nothing, and the two ways to allow sit on the right, with
            // the narrower one ("once") nearest the edge.
            ui.horizontal(|ui| {
                if flat_button(ui, aox::i18n::t("Deny")).clicked() {
                    decision = Some(AuthDecision::Deny);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if primary_button(ui, aox::i18n::t("Allow once")).clicked() {
                        decision = Some(AuthDecision::Allow);
                    }
                    if flat_button(ui, aox::i18n::t("Always allow this session")).clicked() {
                        decision = Some(AuthDecision::AllowSession);
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(aox::i18n::t("Shortcuts: y allow once · a always for this session · n / Esc deny"))
                    .color(P::text_faint())
                    .size(12.5),
            );
        });

        match decision {
            Some(d) => {
                if let Some(tx) = self.auth_reply.take() {
                    let _ = tx.send(d);
                }
                self.waiting_auth = false;
                self.note_act(
                    ActKind::Info,
                    aox::i18n::t("Authorization result"),
                    match d {
                        AuthDecision::Allow => aox::i18n::t("Allow once"),
                        AuthDecision::AllowSession => aox::i18n::t("Always allow this session"),
                        AuthDecision::Deny => aox::i18n::t("Deny"),
                    },
                );
            }
            None => self.auth = Some(card),
        }
    }

    /// The transient toast: one line about something that just happened, gone a few seconds later.
    ///
    /// Why an overlay rather than an `Item` in the transcript: the transcript is a **record** — it
    /// goes to `history/*.jsonl` and is replayed when a conversation is resumed — whereas this is a
    /// passing notice that should not outlive its moment. Being able to sit *above* the transcript
    /// is the other half of the reason it is drawn from `update()` next to the other overlays.
    pub(super) fn toast_overlay(&mut self, ctx: &egui::Context) {
        let Some((count, shown_at)) = self.toast else { return };
        let age = shown_at.elapsed().as_secs_f32();
        if age >= TOAST_SECS {
            self.toast = None;
            return;
        }
        // A toast has to leave **on its own**, and an idle egui window doesn't repaint by itself:
        // without this it would sit there until the mouse happened to move.
        ctx.request_repaint_after(Duration::from_millis(50));
        // Only the tail fades, so the text is fully legible for as long as it is up and then leaves
        // without a snap. The whole card fades (fill, border, shadow and text together) — fading the
        // text alone leaves an empty box hanging in the air.
        let alpha = ((TOAST_SECS - age) / TOAST_FADE_SECS).clamp(0.0, 1.0);
        // Built here, not stored: the count may still grow while the toast is on screen.
        let text = aox::recycle::notice(count);
        egui::Area::new(egui::Id::new("aox-toast"))
            .order(egui::Order::Tooltip)
            .movable(false)
            .interactable(false)
            .anchor(Align2::CENTER_TOP, Vec2::new(0.0, 18.0))
            .show(ctx, |ui| {
                Frame::NONE
                    .fill(P::toast_bg().gamma_multiply(alpha))
                    .stroke(Stroke::new(1.0, P::toast_border().gamma_multiply(alpha)))
                    .corner_radius(CornerRadius::same(TOAST_RADIUS))
                    .shadow(egui::Shadow {
                        offset: [0, 6],
                        blur: 20,
                        spread: 0,
                        color: P::shadow().gamma_multiply(alpha),
                    })
                    .inner_margin(Margin::symmetric(18, 13))
                    .show(ui, |ui| {
                        // The icon and the sentence are one row, vertically centred. The whole
                        // foreground (icon + text) is the deep red, so the notice reads as one object.
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            let ink = P::toast_text().gamma_multiply(alpha);
                            trash_glyph(ui, 20.0, ink);
                            ui.add_space(10.0);
                            ui.label(RichText::new(&text).color(ink).size(14.5));
                        });
                    });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a workspace name may be, pinned down as a table.
    ///
    /// Worth its own test because this is the **only** place in the GUI that decides what a
    /// filename may contain, and it feeds a path straight into `create_dir_all` under the app root:
    /// a separator or a `..` slipping through would not fail loudly, it would quietly point the
    /// whole workspace (and every write the model makes under it) somewhere else than the dialog's
    /// path preview promised. The refusals are therefore asserted individually rather than by a
    /// generic "some are rejected".
    #[test]
    fn workspace_names_are_stripped_down_to_one_safe_segment() {
        // Accepted, in the form the dialog previews it.
        for (typed, want) in [
            ("notes", "notes"),
            // Surrounding whitespace is trimmed rather than refused: `"notes "` *is* `"notes"` as far
            // as the filesystem is concerned, so the only sensible reading is that the user meant the
            // name they typed.
            ("  notes  ", "notes"),
            ("notes ", "notes"),
            ("My Notes 2", "My Notes 2"),
            ("中文 工作区", "中文 工作区"),
        ] {
            assert_eq!(check_ws_name(typed).as_deref(), Ok(want), "typed {typed:?}");
        }

        for typed in [
            "",
            "   ",
            ".",
            "..",
            ".hidden",        // a leading dot is a hidden directory, not a name we want by accident
            "notes.",         // a trailing dot is dropped by Windows -> one folder, two rows
            "a/b",
            "a\\b",
            "C:notes",
            "a:b",
            "a*b",
            "a?b",
            "a\"b",
            "a<b",
            "a>b",
            "a|b",
            "a\nb",           // control characters would end up literally in the folder name
        ] {
            assert!(check_ws_name(typed).is_err(), "{typed:?} must be refused");
        }
    }

    /// The two "are you sure?" boxes make **different promises**, and each has to keep its own.
    ///
    /// The wording *is* the feature (user request, 2026-10-25): one says the conversation file goes to
    /// the recycle bin and can come back, the other says not one byte on the device is touched. A
    /// future destructive action that reused an arm would therefore display a promise that does not
    /// apply to it, so both the sentences and the subjects they name are pinned here.
    #[test]
    fn the_two_confirmations_promise_different_things() {
        let conv = Confirm::DeleteConversation {
            path: PathBuf::from(r"E://rustapp//.aox//history//20261025-142000-abcd.jsonl"),
            title: "重构 Activity Stream 独立窗口".to_string(),
        };
        let ws = Confirm::RemoveWorkspace { root: PathBuf::from(r"E://rustapp//workplace//text") };

        let (title_a, body_a, verb_a, subject_a) = confirm_copy(&conv);
        let (title_b, body_b, verb_b, subject_b) = confirm_copy(&ws);

        assert_ne!(title_a, title_b, "two different questions must not share a title");
        assert_ne!(body_a, body_b, "and must not share a promise");
        assert_eq!((verb_a, verb_b), ("Delete", "Remove"));

        // Each sentence has to stay true of the path behind it: `ToAgent::Recycle` moves the file,
        // `forget_workspace` only drops a registry entry.
        assert!(body_a.contains("recycle bin") && body_a.contains("restored"), "{body_a}");
        assert!(body_b.contains("nothing on this device"), "{body_b}");

        // The subject is the row's **label**, not the file stem -- a timestamp identifies nothing to
        // the person reading the dialog.
        assert_eq!(subject_a, "重构 Activity Stream 独立窗口");
        assert!(subject_b.contains("text"), "{subject_b}");
    }
}
