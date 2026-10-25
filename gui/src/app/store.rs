//! Conversations and workspaces: which one is open, where it lives, and what has to be written
//! down when that changes.
//!
//! The window's only file-level state lives here -- `.aox/ui.json` (group folds + drawer position) and
//! the space registry -- plus the swap that happens when the agent says it has opened another
//! conversation. Keeping it together means "switching conversations resets what belongs to the old one"
//! is one function you can read top to bottom, rather than a rule spread over six screens.

use super::*;

impl App {
    pub(super) fn apply_snapshot(&mut self, s: Snapshot) {
        // The top bar no longer has a "plugins N / tools N" chip, so only the data itself is
        // kept here; plugin changes already show up in the bridge's event stream and don't need
        // another entry here.
        self.plugins = s.plugins;
    }

    /// Rescan the history directory.
    ///
    /// Done only when the sidebar opens, a turn ends, or a new conversation is started, rather
    /// than every frame -- scanning every frame needlessly interrupts the UI's smoothness, and
    /// the history list doesn't need to be that fresh anyway.
    pub(super) fn refresh_history(&mut self) {
        let Some(store) = self.store.clone() else {
            return;
        };
        // Read the registry and hand its workspace roots to the store **before** listing. `list()`
        // only scans the workspaces the store has been told about, so doing this afterwards would
        // make the first refresh after a workspace is added (or removed) draw the **old** tree --
        // and in the add case the new workspace's conversations would be missing from it.
        //
        // The registry is re-read every time (one small JSON file): a workspace can change while the
        // app runs, and a cached copy would keep drawing a tree that no longer matches the disk.
        let root = self.app_root();
        // Fold state and the drawer's position, read **once** — the first refresh that knows the
        // root (see `App::hist_fold_loaded`). Doing it here rather than in the boot arm means the
        // `--demo` constructor, which sets up its own synthetic boot and then refreshes, is covered
        // too.
        //
        // The drawer comes back **the way it was left** (user request, 2026-10-25; for the week
        // before that it always started folded). `hist_t` is set alongside `show_history` rather
        // than left to the animation: `update` eases `hist_t` toward `show_history` every frame, so
        // restoring one without the other would slide the panel open on launch — a wake-up animation
        // nobody asked for. Snapped, the first painted frame is already the final layout.
        if !self.hist_fold_loaded {
            if let Some(root) = root.as_deref() {
                let fs = uistate::FoldState::load(root);
                self.hist_open = fs.open;
                self.hist_closed = fs.closed;
                self.show_history = fs.drawer;
                self.hist_t = if fs.drawer { 1.0 } else { 0.0 };
                self.hist_fold_loaded = true;
            }
        }
        let reg = root.as_deref().map(aox::space::Registry::load);
        if let (Some(root), Some(reg)) = (root.as_deref(), reg.as_ref()) {
            store.set_workspaces(&reg.workspace_roots(root));
        }
        let mut items = store.list(HISTORY_LIST_LIMIT);
        // The conversation being chatted in is always pinned to the top.
        //
        // The on-disk sort key is "time of the last utterance", and the conversation just
        // switched into may not have been touched for three days, so it lands far down the
        // list -- yet that is the one the user is looking for right now. Pinning is a purely
        // presentational adjustment: it doesn't touch file times and doesn't affect the order
        // `list()` gives the CLI.
        if let Some(cur) = self.current_history.as_deref() {
            if let Some(i) = items.iter().position(|s| s.id == cur) {
                let it = items.remove(i);
                items.insert(0, it);
            }
        }
        // Group the (already newest-first) list into workspaces, for the drawer's tree.
        //
        // Done here rather than in the drawer: the drawer redraws at frame rate, and grouping
        // walks and re-sorts the whole list. There is a useful side effect of the pinning above --
        // `group` orders groups by their newest item's position in the list it is handed, so the
        // workspace holding the current conversation comes out first among the user's workspaces,
        // and the current conversation is first inside its group. That is the two-level pin for free.
        self.hist_groups = match (root.as_deref(), reg.as_ref()) {
            (Some(root), Some(reg)) => aox::space::group(root, reg, items.clone()),
            _ => Vec::new(),
        };
        self.history = items;
    }

    /// Drop a workspace from AOX: **registration only, nothing on disk is touched**.
    ///
    /// Writes the `hidden` mark into the registry so the folder stops being listed *and* stops being
    /// scanned -- dropping it from the spaces alone would not do, because `group()` invents a space
    /// for any unknown root it meets, so the row would be back on the very next refresh.
    ///
    /// A failed write is swallowed on purpose: the visible result of "the file could not be saved" is
    /// the row still being there, which is the same thing the user sees if they cancel. There is no
    /// separate error surface for this, and a toast for a UI-organisation file would be noise.
    pub(super) fn forget_workspace(&mut self, root: &Path) {
        let Some(app_root) = self.app_root() else {
            return;
        };
        if let Err(e) = (|| -> anyhow::Result<()> {
            let mut reg = aox::space::Registry::load(&app_root);
            reg.remove_workspace(root);
            reg.save(&app_root)
        })() {
            // The row staying on screen *is* the failure report; put the reason in the log where a
            // developer can find it (`log_error` shows on screen, which this does not deserve).
            eprintln!("aox-gui: could not save the space registry: {e:#}");
            return;
        }
        // The fold state is deliberately **not** reset here. The removed group's id stops being drawn
        // (`hist_groups` is rebuilt without it), so whatever it left in the two sets is inert -- and
        // clearing them would mean "I dropped one workspace and all my other groups jumped open",
        // which is a side effect on places the user did not touch.
        self.refresh_history();
    }

    /// Create a workspace, register it, and move this conversation into it.
    ///
    /// Three steps, in this order and each for its own reason:
    ///
    /// 1. **The folder**, under `<app root>/workplace/` — the layout convention the system prompt
    ///    already describes to the model, so "make yourself a project folder" and this button agree
    ///    about where projects live. An **existing** folder is adopted instead of refused: putting a
    ///    directory you already have back into the tree is the same gesture as making a new one, and
    ///    `create_dir_all` is a no-op when it is already there. Nothing is ever overwritten.
    /// 2. **The registry**, which is what makes the group exist at all. The tree is rebuilt from that
    ///    file on every refresh, so a folder nobody registered is not a workspace, it is just a folder.
    /// 3. **The conversation's own workspace**, over the bridge: the root only decides where writes
    ///    may land once the agent thread has applied it (`ToAgent::Workplace`), because the tier
    ///    belongs to the conversation, not to the window. The bridge answers with a `Workplace` push,
    ///    which is what renames the chip at the bottom of the input box -- this call is a request.
    ///
    /// Returns the reason it failed, for the dialog to show. Both are things the user can act on (a
    /// name that cannot be a folder, a directory that cannot be written), which is why the message
    /// belongs in the dialog rather than in a toast that has already faded.
    pub(super) fn create_workspace(&mut self, raw: &str, app_root: Option<&Path>) -> Result<(), String> {
        let Some(app_root) = app_root else {
            return Err(aox::i18n::t("AOX does not know its own application root").to_string());
        };
        let name = check_ws_name(raw)?;
        let dir = app_root.join(aox::workplace::WORKPLACE_DIR).join(&name);
        std::fs::create_dir_all(&dir)
            .map_err(|e| aox::tf!("Could not make the workspace: {}", e))?;

        let mut reg = aox::space::Registry::load(app_root);
        let id = reg.add_workspace(&dir, Some(name));
        reg.save(app_root)
            .map_err(|e| aox::tf!("Could not make the workspace: {}", format!("{e:#}")))?;

        // The tree is rebuilt from the registry, so the new group is on screen from this frame on.
        self.refresh_history();
        // Unfold it, so the group the button just made is visibly there instead of being one more
        // folded header among the others. The stale "explicitly folded" mark is dropped first --
        // re-adding a folder that had been removed lands on the same id, and a group the user once
        // folded must not come back folded when they have just asked for it by name.
        self.hist_closed.remove(&id);
        self.hist_open.insert(id);
        // The unfold is a fold-state change like any other, so it is written through the same path --
        // otherwise "create a workspace, restart" would show the new group folded again.
        self.save_fold_state();
        // ...and move this conversation in, so the next thing written lands in the new folder. The
        // bridge answers with a `Workplace` push, which is what renames the chip at the bottom of the
        // input box -- this call is a request, not a state change.
        self.send_ws(Mode::Fixed(dir));
        Ok(())
    }

    /// Open the rename dialog for a workspace, pre-filled with the name the tree is showing.
    ///
    /// The current name is read from the **drawn group** rather than from a fresh registry read:
    /// they are the same value, and this way the name in the field is exactly the one the user just
    /// right-clicked, with no chance of the two disagreeing mid-frame.
    pub(super) fn start_rename_workspace(&mut self, root: &Path) {
        let current = self
            .hist_groups
            .iter()
            .find(|g| g.root.as_deref() == Some(root))
            .map(|g| g.name.clone())
            .unwrap_or_default();
        self.name_dialog = Some(NameDialog::renaming(root.to_path_buf(), current));
    }

    /// Rename a workspace's **label**, and nothing else. See [`aox::space::Registry::rename_workspace`]
    /// for why the folder is not moved; from here it matters that this is a registry edit only, so it
    /// cannot disturb the conversation being chatted in, and it does not go near the bridge.
    ///
    /// `None` for `raw` (or a blank name) means "show the folder's own name again" — the rename
    /// dialog's working undo, since a label equal to the folder name is what "no name" already
    /// displays as. Anything else goes through **the same** validation as creating: the name lands in
    /// the same file and the same tree, and a looser rule here would just be a second way to write a
    /// name the create dialog refuses.
    pub(super) fn rename_workspace(&mut self, root: &Path, raw: Option<&str>) -> Result<(), String> {
        let Some(app_root) = self.app_root() else {
            return Err(aox::i18n::t("AOX does not know its own application root").to_string());
        };
        let name = match raw.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => Some(check_ws_name(n)?),
            None => None,
        };
        let mut reg = aox::space::Registry::load(&app_root);
        reg.rename_workspace(root, name);
        reg.save(&app_root)
            .map_err(|e| aox::tf!("Could not rename the workspace: {}", format!("{e:#}")))?;
        // The fold state is deliberately **not** touched: a group's header is on screen whether it is
        // folded or not, so the new name is visible either way — and forcing the group open would be
        // an unasked-for change to a place the user did not click. (Creating a workspace does unfold,
        // because there the group is brand new and folded is how it would arrive.)
        self.refresh_history();
        Ok(())
    }

    /// Open the rename dialog for a conversation, pre-filled with the title the row is showing.
    ///
    /// Read from the **drawn row** for the same reason as the workspace case: it is the exact string
    /// the user just right-clicked, with no chance of the dialog and the row disagreeing.
    pub(super) fn start_rename_conversation(&mut self, id: &str, current: String) {
        self.name_dialog = Some(NameDialog::renaming_conversation(id.to_string(), current));
    }

    /// Rename one conversation's **label**, and nothing else: the id (the filename), the workspace it
    /// is bound to and every message in it are all untouched. A `kind:"title"` row is appended to the
    /// conversation's own file (see [`aox::store::Session::append_title`]) — which is what makes this
    /// survive a rollback, and why the newest row is the one that counts.
    ///
    /// `None` (or a blank name) means "show the automatic title again" — the first user message. That
    /// is the dialog's working undo, and it needs the store's nested `Option` to be distinguishable
    /// from "never renamed": a cleared label is a row with no `title` key, not the absence of a row.
    ///
    /// This goes **over the bridge** rather than touching the file here. The conversation being
    /// chatted in has its file owned by the agent thread (that is where the append handle lives), so
    /// writing it from the UI thread would be a second writer on the same file; and for any other
    /// conversation the agent is the one that owns the store handle. So this is a request — the tree
    /// is rebuilt when the bridge answers with a fresh history list.
    pub(super) fn rename_conversation(&mut self, id: &str, raw: Option<&str>) -> Result<(), String> {
        let title = match raw.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => Some(check_conv_title(t)?),
            None => None,
        };
        let _ = self.tx.send(ToAgent::RenameConversation {
            id: id.to_string(),
            title,
        });
        Ok(())
    }

    /// Switch to a history conversation.
    ///
    /// Send the request and return: the actual context swap happens on the agent thread, and
    /// the UI only moves once `FromAgent::Switched` comes back -- that way a failed switch
    /// doesn't have the UI jump ahead and leave the UI on one conversation while the model is
    /// on another.
    pub(super) fn switch_history(&mut self, id: &str) {
        if self.busy || self.switching || Some(id) == self.current_history.as_deref() {
            return;
        }
        self.switching = true;
        let _ = self.tx.send(ToAgent::Resume(id.to_string()));
    }

    /// Start a new conversation.
    ///
    /// The old conversation **needs no wrap-up at all**: it was appended to as we chatted, so
    /// it already sits complete in `history/`. So all this does is let go on the agent side,
    /// with the UI clearing along with it.
    pub(super) fn new_conversation(&mut self) {
        if self.busy || self.switching || (self.current_history.is_none() && self.items.is_empty()) {
            return;
        }
        self.switching = true;
        let _ = self.tx.send(ToAgent::NewConversation);
    }

    /// Start a **new conversation bound to a particular workspace** — the "＋" on a group header, and
    /// the same row in its right-click menu.
    ///
    /// Two messages, and the **order is the whole implementation**: `NewConversation` first, then
    /// `Workplace(Fixed(root))`. The agent's `new_conversation` resets the workspace tier to `Default`,
    /// so sending the root first would have it wiped a moment later and the new thread would land back
    /// in the app root — the exact opposite of what the row promised. Both go down one channel, which
    /// the agent drains in order, so "first" here really is first there.
    ///
    /// The early return for an already-empty conversation is deliberately **not** copied from
    /// [`App::new_conversation`]: there, doing nothing is right, because the point is "give me a fresh
    /// one" and a fresh one is already on screen. Here the point is **where the next thing gets
    /// written**, and a blank conversation that is still bound to the app root is not in the workspace
    /// the user just clicked — so the `Workplace` half has to go through in that case too.
    ///
    /// Like every other conversation switch this is a request: the UI moves when the bridge answers
    /// (`FromAgent::Switched`, `FromAgent::Workplace`).
    pub(super) fn new_conversation_in(&mut self, root: &Path) {
        if self.busy || self.switching {
            return;
        }
        if self.demo {
            // No agent thread to answer, so put the one thing the demo can show -- the workspace the
            // chip at the bottom of the input box names -- into place the same way `send_ws` does.
            self.send_ws(Mode::Fixed(root.to_path_buf()));
            return;
        }
        self.switching = true;
        let _ = self.tx.send(ToAgent::NewConversation);
        let _ = self.tx.send(ToAgent::Workplace(Mode::Fixed(root.to_path_buf())));
    }

    /// Re-file a conversation into another group — **a registry edit only**; nothing on disk moves.
    ///
    /// See [`aox::space::Registry::place_conversation`] for what "placement" does and does not mean
    /// (the file stays put, the write boundary is untouched). From here the interesting part is the
    /// target: the menu hands back a **group id**, which may be an `auto:` id for an unregistered root,
    /// so the root is read off the drawn tree rather than assumed to be in the registry.
    ///
    /// The destination is unfolded on the way, so the moved row is visibly there instead of vanishing
    /// into a folded group — the same gesture `create_workspace` makes for the group it just made. A
    /// move whose destination you cannot see is the one outcome worth spending a fold on.
    pub(super) fn move_conversation(&mut self, conv_id: &str, target_group_id: &str) {
        let Some(app_root) = self.app_root() else {
            return;
        };
        // The root comes off the **drawn** group, which is what lets an auto group (a root the
        // registry has never heard of) be registered by the placement itself.
        let target_root = self
            .hist_groups
            .iter()
            .find(|g| g.id == target_group_id)
            .and_then(|g| g.root.clone());
        let mut reg = aox::space::Registry::load(&app_root);
        let id = reg.place_conversation(conv_id, target_group_id, target_root.as_deref());
        if let Err(e) = reg.save(&app_root) {
            // As with the other registry edits: the row not moving *is* the failure report.
            eprintln!("aox-gui: could not save the space registry: {e:#}");
            return;
        }
        // Rebuilding the tree is what actually moves the row between groups.
        self.refresh_history();
        // Unfold the destination -- under the id it ended up with, which differs from the target's when
        // the target was an auto group that the placement just registered.
        self.hist_closed.remove(&id);
        self.hist_open.insert(id);
        self.save_fold_state();
    }

    /// Move the UI to the conversation with id `id` (`None` = a brand-new empty one).
    ///
    /// The content is rebuilt by reading the disk directly rather than having the agent send
    /// the messages over: the history file is local anyway, and passing only the id down the
    /// channel keeps the responsibilities cleaner (the agent owns context, the UI owns
    /// presentation).
    pub(super) fn adopt_conversation(&mut self, id: Option<&str>) {
        self.items = match (id, self.store.as_ref()) {
            (Some(id), Some(store)) => store
                .load(id)
                .map(|l| items_from_history(&l))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        // A brand-new conversation draws a fresh background (the user's ask: "pick one at startup
        // and on every new conversation"). Opening an existing conversation keeps whatever is
        // showing -- only `None` means "nothing has been said yet", which is the one state the
        // background belongs to. Note that this re-rolls even when startup already rolled once:
        // the second roll is the one that lands on screen, so there is nothing to reconcile.
        if id.is_none() {
            self.brand.reroll();
        }
        // Swap out the previous conversation's in-progress state along with the activity
        // stream, otherwise the right side would show the new conversation while scrolling the
        // old conversation's tool calls.
        self.live = None;
        self.waiting_auth = false;
        self.auth = None;
        self.auth_reply = None;
        // The option slot is folded away too. **Dropping the reply channel is enough**: the
        // peer's `rx.await` errors out immediately, and `GuiUi::choose` maps "channel closed" to
        // `Cancelled` (see the comment in bridge) -- so the agent thread doesn't get stuck in a
        // `choose()` that will never see an answer.
        self.choice = None;
        self.choice_reply = None;
        // The usage ledger is swapped along with the messages and the activity stream: the
        // agent side has read its ledger back too, and the `UsageLog` that follows will send
        // this conversation's records over.
        self.usage_log.clear();
        // The cache map is swapped with the rest: it describes "the conversation I am in", and the
        // ledger it is fed from has just been cleared. The window itself stays open (it is a lens on
        // whatever conversation is current, not a property of one), and its position is left alone.
        cache::lock(&self.cache).reset();
        // The workspace view's edit ledger belongs to the conversation too: it lists what **this**
        // conversation's current turn touched, and the turn count restarts here. The window stays
        // open (it is a lens on the current workspace, not a property of one conversation).
        wsview::reset(&self.wsview);
        // The activity stream is swapped too. It lives in the **same file** as the messages, so
        // "open this conversation" should show the timeline as it was then; a new conversation
        // clears it.
        //
        // Fetch the records before taking the lock: reading the store goes through `self.store`,
        // while the lock guard holds `self.act` for a long time.
        let rows = match (id, self.store.as_ref()) {
            (Some(id), Some(store)) => store.load_acts(id),
            _ => Vec::new(),
        };
        // Compute `now` before taking the lock: `self.now()` borrows all of `self`, which
        // tangles easily with the lock guard.
        let now = self.now();
        let mut act = activity::lock(&self.act);
        match id {
            Some(_) => act.load_acts(&rows, now),
            None => act.reset(),
        }
        drop(act);
        // The plan card is the conversation's **own**, off the same row the agent reads on resume
        // (`kind:"task"`, newest wins). Reopening a conversation therefore draws the plan it stopped
        // at -- and a conversation that never had a checkpoint draws nothing.
        //
        // Reading it from the store, rather than keeping whatever is already in `self.plan`, is the
        // fix for 2026-10-24: the card used to survive a switch and sit there claiming the *previous*
        // conversation's progress until a new checkpoint happened to overwrite it -- on a brand-new
        // conversation, which may never produce one, indefinitely.
        self.task = plan_of(self.store.as_deref(), id);
        self.plan = self.task.as_ref().and_then(PlanView::of);
        // The card always arrives **collapsed**, and without an animation: `plan_open` / `plan_t` /
        // `plan_anim` describe the widget on screen, not the conversation. Carrying them over would
        // unfold the new conversation's plan on its first frame, and the disclosure would look like
        // it sprang open by itself.
        self.plan_open = false;
        self.plan_t = 0.0;
        self.plan_anim = None;
        self.follow = true;
        self.force_follow = true;
    }

    /// Write the two fold sets **and the drawer's position** to `<app root>/.aox/ui.json`.
    ///
    /// Best-effort, like the registry: a failure is logged, never surfaced. The tree keeps working
    /// with the in-memory sets either way, so a failure only costs "the fold is not remembered next
    /// launch" — not worth an error the user has to dismiss.
    pub(super) fn save_fold_state(&self) {
        let Some(root) = self.app_root() else {
            return;
        };
        let fs = uistate::FoldState {
            open: self.hist_open.clone(),
            closed: self.hist_closed.clone(),
            drawer: self.show_history,
        };
        if let Err(e) = fs.save(&root) {
            eprintln!("aox-gui: could not save the fold state: {e:#}");
        }
    }

    /// Write a folder the user has just chosen into the registry, so it **survives a restart**.
    ///
    /// This is the missing half of the directory picker. `ToAgent::Workplace` only tells the agent
    /// thread where the next conversation may write; it says nothing to the registry, and the
    /// registry is what `workspace_roots` — and therefore the store's scan — is built from. So a
    /// folder picked from the list switched the writes correctly and then **vanished on the next
    /// launch**: the group was drawn from an `auto:` fallback for as long as a conversation lived
    /// there, and the conversations themselves were never listed again because their directory was
    /// never scanned.
    ///
    /// Called from [`App::send_ws`] rather than from the picker so that every door onto "switch to
    /// this folder" — the tier menu, the full-access confirmation, the picker — passes through it.
    /// The app root and folders already in the registry are refused there (see
    /// [`aox::space::Registry::adopt_workspace`]), which is what keeps a repeat click from
    /// rewriting the file and keeps `Default`'s conversations in `Default`.
    pub(super) fn adopt_workspace(&mut self, root: &Path) {
        let Some(app_root) = self.app_root() else {
            return;
        };
        let mut reg = aox::space::Registry::load(&app_root);
        let Some(id) = reg.adopt_workspace(root, &app_root) else {
            return;
        };
        if let Err(e) = reg.save(&app_root) {
            // Same policy as every other registry write: logged, not surfaced. The conversation
            // still goes where the user asked; what is lost is only "the folder is still there next
            // launch", and a modal for that would interrupt the thing they were doing.
            eprintln!("aox-gui: could not save the space registry: {e:#}");
            return;
        }
        // The tree is rebuilt from the registry, so the group is on screen from this frame on --
        // and unfolded, exactly as `create_workspace` leaves the group it makes. An empty new space
        // is not drawn yet (`group()` drops a group with no conversations unless it is built-in), so
        // this mostly matters for the frame the first conversation lands in it.
        self.refresh_history();
        self.hist_closed.remove(&id);
        self.hist_open.insert(id);
        self.save_fold_state();
    }

    /// Send a request produced by the workplace button to the agent thread.
    ///
    /// The UI can't change tiers by itself: `Workplace` lives inside `Runtime`, and only the
    /// agent thread can get at `Runtime` (see `ToAgent::Workplace` in `bridge.rs`). So all this
    /// does is "send it out"; the real result and the new tier are driven by the bridge's
    /// acknowledgement (`FromAgent::Workplace`).
    pub(super) fn send_ws(&mut self, mode: Mode) {
        if self.demo {
            // Demo mode has no agent thread to acknowledge, so simulate it in place -- that way
            // `--demo` can also show "switch tier -> button renames + the tick in the list
            // follows". UI only, no disk involved.
            let (view_mode, root) = match &mode {
                Mode::Default => (Mode::Default, None),
                Mode::Fixed(p) => (Mode::Fixed(p.clone()), Some(p.clone())),
                Mode::Full => (Mode::Full, None),
            };
            self.ws.set_view(WorkplaceView { mode: view_mode, root });
            return;
        }
        // A folder is a workspace only if the registry says so (see `adopt_workspace`); the agent
        // message below moves the write boundary, and this is the other half of the same click.
        if let Mode::Fixed(root) = &mode {
            self.adopt_workspace(root);
        }
        let _ = self.tx.send(ToAgent::Workplace(mode));
    }
}

