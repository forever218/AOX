//! Conversations and workspaces: which one is open, where it lives, and what has to be written down...
//! The window's only file-level state lives here -- `.aox/ui.json` (group folds + drawer position) and the space...

use super::*;

impl App {
    pub(super) fn apply_snapshot(&mut self, s: Snapshot) {
        // The top bar no longer has a "plugins N / tools N" chip, so only the data itself is kept here; plugin changes...
        self.plugins = s.plugins;
    }

    /// Rescan the history directory.
    pub(super) fn refresh_history(&mut self) {
        let Some(store) = self.store.clone() else {
            return;
        };
        // Read the registry and hand its workspace roots to the store before listing. `list()` only scans the workspaces...
        let root = self.app_root();
        // Fold state and the drawer's position, read once — the first refresh that knows the root (see...
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
        // No workspace list to hand the store any more — every conversation lives in the one directory, and "which...
        let reg = root.as_deref().map(aox::space::Registry::load);
        let mut items = store.list(HISTORY_LIST_LIMIT);
        // The conversation being chatted in is always pinned to the top.
        if let Some(cur) = self.current_history.as_deref() {
            if let Some(i) = items.iter().position(|s| s.id == cur) {
                let it = items.remove(i);
                items.insert(0, it);
            }
        }
        // Group the (already newest-first) list into workspaces, for the drawer's tree.
        self.hist_groups = match (root.as_deref(), reg.as_ref()) {
            (Some(root), Some(reg)) => aox::space::group(root, reg, items.clone()),
            _ => Vec::new(),
        };
        self.history = items;
    }

    /// Drop a workspace from AOX: registration only, nothing on disk is touched.
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
        // The fold state is deliberately not reset here.
        self.refresh_history();
    }

    /// Create a workspace, register it, and move this conversation into it.
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
        // Unfold it, so the group the button just made is visibly there instead of being one more folded header among...
        self.hist_closed.remove(&id);
        self.hist_open.insert(id);
        // The unfold is a fold-state change like any other, so it is written through the same path --
        // otherwise "create a workspace, restart" would show the new group folded again.
        self.save_fold_state();
        // ...and move this conversation in, so the next thing written lands in the new folder.
        self.send_ws(Mode::Fixed(dir));
        Ok(())
    }

    /// Open the rename dialog for a workspace, pre-filled with the name the tree is showing.
    pub(super) fn start_rename_workspace(&mut self, root: &Path) {
        let current = self
            .hist_groups
            .iter()
            .find(|g| g.root.as_deref() == Some(root))
            .map(|g| g.name.clone())
            .unwrap_or_default();
        self.name_dialog = Some(NameDialog::renaming(root.to_path_buf(), current));
    }

    /// Rename a workspace's label, and nothing else.
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
        // The fold state is deliberately not touched: a group's header is on screen whether it is folded...
        self.refresh_history();
        Ok(())
    }

    /// Open the rename dialog for a conversation, pre-filled with the title the row is showing.
    pub(super) fn start_rename_conversation(&mut self, id: &str, current: String) {
        self.name_dialog = Some(NameDialog::renaming_conversation(id.to_string(), current));
    }

    /// Rename one conversation's label, and nothing else: the id (the filename), the workspace it is bound...
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
    pub(super) fn switch_history(&mut self, id: &str) {
        if self.busy || self.switching || Some(id) == self.current_history.as_deref() {
            return;
        }
        self.switching = true;
        let _ = self.tx.send(ToAgent::Resume(id.to_string()));
    }

    /// Start a new conversation.
    /// The old conversation needs no wrap-up at all: it was appended to as we chatted, so it already sits complete...
    pub(super) fn new_conversation(&mut self) {
        if self.busy || self.switching || (self.current_history.is_none() && self.items.is_empty()) {
            return;
        }
        self.switching = true;
        let _ = self.tx.send(ToAgent::NewConversation);
    }

    /// Workspace-bound new conversation: NewConversation must precede Workplace(Fixed(root)) or the reset undoes it.
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

    /// Re-file a conversation into another group — a registry edit only; nothing on disk moves.
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
    pub(super) fn adopt_conversation(&mut self, id: Option<&str>) {
        self.items = match (id, self.store.as_ref()) {
            (Some(id), Some(store)) => store
                .load(id)
                .map(|l| items_from_history(&l))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        // A brand-new conversation draws a fresh background (the user's ask: "pick one at startup and on every new...
        if id.is_none() {
            self.brand.reroll();
        }
        // Swap out the previous conversation's in-progress state along with the activity stream, otherwise the right side...
        self.live = None;
        self.waiting_auth = false;
        self.auth = None;
        self.auth_reply = None;
        // The option slot is folded away too.
        self.choice = None;
        self.choice_reply = None;
        // The usage ledger is swapped along with the messages and the activity stream: the agent side has read its ledger...
        self.usage_log.clear();
        // The cache map is swapped with the rest: it describes "the conversation I am in", and the ledger it is fed...
        cache::lock(&self.cache).reset();
        // The workspace view's edit ledger belongs to the conversation too: it lists what this conversation's current...
        wsview::reset(&self.wsview);
        // The activity stream is swapped too.
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
        // The plan card is the conversation's own, off the same row the agent reads on resume (`kind:"task"`, newest...
        self.task = plan_of(self.store.as_deref(), id);
        self.plan = self.task.as_ref().and_then(PlanView::of);
        // The card always arrives collapsed, and without an animation: `plan_open` / `plan_t` / `plan_anim` describe...
        self.plan_open = false;
        self.plan_t = 0.0;
        self.plan_anim = None;
        self.follow = true;
        self.force_follow = true;
    }

    /// Write the two fold sets and the drawer's position to `<app root>/.aox/ui.json`.
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

    /// Write a folder the user has just chosen into the registry, so it survives a restart.
    pub(super) fn adopt_workspace(&mut self, root: &Path) {
        let Some(app_root) = self.app_root() else {
            return;
        };
        let mut reg = aox::space::Registry::load(&app_root);
        let Some(id) = reg.adopt_workspace(root, &app_root) else {
            return;
        };
        if let Err(e) = reg.save(&app_root) {
            // Same policy as every other registry write: logged, not surfaced.
            eprintln!("aox-gui: could not save the space registry: {e:#}");
            return;
        }
        // The tree is rebuilt from the registry, so the group is on screen from this frame on -- and unfolded, exactly...
        self.refresh_history();
        self.hist_closed.remove(&id);
        self.hist_open.insert(id);
        self.save_fold_state();
    }

    /// Send a request produced by the workplace button to the agent thread.
    pub(super) fn send_ws(&mut self, mode: Mode) {
        if self.demo {
            // Demo mode has no agent thread to acknowledge, so simulate it in place -- that way `--demo` can also show...
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
        // The recovery rows belong to the old root; a different workspace has different points.
        self.rec.close();
        let _ = self.tx.send(ToAgent::Workplace(mode));
    }

    /// Re-read the recovery list (the chip's count, and the panel if it is open).
    pub(super) fn refresh_recovery(&mut self) {
        let on = self.boot.as_ref().is_some_and(|b| b.recovery);
        if self.demo || !on {
            return;
        }
        self.send_recovery(crate::bridge::RecoveryReq::List);
    }

    /// Send a recovery request to the agent thread.
    pub(super) fn send_recovery(&mut self, req: crate::bridge::RecoveryReq) {
        if self.demo {
            // `--demo` has no agent thread behind it. Answering in place is better than a panel that
            // sits on its skeleton forever: the layout is what demo mode is for.
            self.rec.set_view(crate::bridge::RecoveryView::Notice {
                text: aox::i18n::t("Demo mode: there is no recovery store behind this window.").into(),
                ok: false,
            });
            return;
        }
        self.rec.mark_busy();
        let _ = self.tx.send(ToAgent::Recovery(req));
    }

    /// Send a model request (apply / scan) to the agent thread.
    pub(super) fn send_requests(&mut self, req: crate::model_ui::Requests) {
        if let Some(edit) = req.apply {
            self.send_model(crate::model_ui::Request::Apply(edit));
        }
        if let Some(scan) = req.scan {
            self.send_model(crate::model_ui::Request::Scan(scan));
        }
        if let Some(probe) = req.test {
            self.send_model(crate::model_ui::Request::Test(probe));
        }
        if req.open_settings {
            self.settings_open = true;
            self.settings_tab = SettingsTab::Model;
        }
    }

    pub(super) fn send_model(&mut self, req: crate::model_ui::Request) {
        if self.demo {
            self.mdl.set_notice(
                false,
                aox::i18n::t("Demo mode: there is no agent thread behind this window.").into(),
            );
            return;
        }
        match req {
            crate::model_ui::Request::Apply(edit) => {
                let _ = self.tx.send(ToAgent::ApplyProvider(edit));
            }
            crate::model_ui::Request::Test(probe) => {
                let _ = self.tx.send(ToAgent::TestModel {
                    base_url: probe.base_url,
                    api_key: probe.api_key,
                    model: probe.model,
                });
            }
            crate::model_ui::Request::Scan(req) => {
                let _ = self.tx.send(ToAgent::ScanModels {
                    base_url: req.base_url,
                    api_key: req.api_key,
                });
            }
        }
    }
}

