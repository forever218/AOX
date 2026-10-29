//! Pumping the agent's channel, and turning its events into window state.
//!
//! This is the one module that touches most of `App`'s fields, which is exactly why it is a module: the
//! rules for "what an event does to the window" (a retried request rolls its own rows back, a finished
//! turn moves the live area into the transcript, a checkpoint replaces the plan) sit in one place
//! instead of being interleaved with painting.

use super::*;

impl App {
    // ── event pump: land the agent thread's messages into local state ──
    pub(super) fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                FromAgent::Boot(b) => {
                    self.model = b.model.clone();
                    self.config_path = b.config_path.clone();
                    // The ring's denominator. Zero would mean "not configured": fall back to the
                    // default rather than dividing by it.
                    self.context_window = if b.context_window > 0 {
                        b.context_window
                    } else {
                        DEFAULT_CONTEXT_WINDOW
                    };
                    // The UI-side history backend is shipped along by the bridge: the same
                    // implementation the agent thread uses.
                    self.store = Some(b.store.clone());
                    if let Some(note) = &b.note {
                        self.log_error = Some(note.clone());
                        self.items.push(Item::Error(note.clone()));
                    }
                    self.status = if b.provider_ready { aox::i18n::t("Ready").to_string() } else { aox::i18n::t("No model configured").to_string() };
                    let ready = b.provider_ready;
                    // A fresh install lands here: `config.toml` was just written by the host, so the one
                    // thing that can make the window useful opens by itself, with the cursor in the key
                    // field. Doing nothing would leave a first-time user looking at a disabled input box
                    // and a sentence telling them to edit a file — the exact friction this removes.
                    //
                    // The **dialog alone** (2026-10-28, user request). It used to also open the settings
                    // page, so an install's first screen was a modal on top of another modal saying the
                    // same thing — two windows for one question. The page is one keystroke away if it is
                    // ever wanted; the tab is still preselected, so opening Settings later lands on Model
                    // rather than on whichever tab was last visited.
                    //
                    // Offered **once** per launch: the not-configured path boots twice (once before the
                    // save, once after it), and both used to carry `first_run`, which is why saving left
                    // a second, empty dialog on screen. The bridge now spends the flag on the first of
                    // the two; this latch makes it impossible to stack the dialog even if it comes back.
                    if b.first_run && !self.setup_asked {
                        self.setup_asked = true;
                        self.settings_tab = SettingsTab::Model;
                        self.mdl.focus_key();
                    }
                    // The address book lives beside `ui.json` / `spaces.json` under the app root, and
                    // the root is only known once `Boot` arrives — so the model UI is handed it here.
                    let root = PathBuf::from(&b.paths.root);
                    // The boot info has to be in place **before** the first refresh: `refresh_history`
                    // reads the app root out of it (`App::app_root`) to find the space registry, and a
                    // refresh that landed first would build an empty history tree.
                    self.boot = Some(*b);
                    self.mdl.attach_root(&root);
                    self.refresh_history();
                    // The chip's number has to be right before anyone clicks it, so ask once at
                    // startup rather than "the count appears after you open the panel once".
                    self.refresh_recovery();
                    if ready {
                        // --prompt: send the first instruction automatically once the model is ready
                        if let Some(p) = self.pending_prompt.take() {
                            self.input = p;
                            self.send();
                        }
                    }
                }
                FromAgent::Snapshot(s) => self.apply_snapshot(s),
                FromAgent::Runtimes(v) => self.runtimes = v,
                // Current workplace state: the tier name on the button and the ticked row
                // in the list both come from it. The bridge re-sends one at startup, at the
                // end of every turn, and after every tier change.
                FromAgent::Workplace(v) => self.ws.set_view(*v),
                // The recovery panel. Rows are data, never text parsed back out of the CLI's output
                // (see `bridge::RecoveryView`); the panel only decides how they look.
                FromAgent::Recovery(v) => self.rec.set_view(*v),
                // The model in force. Three things read it, and all three must change together:
                // the chip in the input box, the "Model in use" row on the System tab, and the
                // context ring's denominator — switching to a smaller model has to shrink the ring.
                FromAgent::Provider(v) => {
                    // Read before the update below overwrites it: "the window could not talk until
                    // now" is the condition for dropping the notices that say so.
                    let was_ready = self.boot.as_ref().is_some_and(|b| b.provider_ready);
                    self.model = v.model.clone();
                    if v.context_window > 0 {
                        self.context_window = u64::from(v.context_window);
                    }
                    self.status = if v.ready {
                        aox::i18n::t("Ready").to_string()
                    } else {
                        aox::i18n::t("No model configured").to_string()
                    };
                    // The input box's "chat unavailable" state is driven by `Boot`'s `provider_ready`
                    // one-shot. A model configured **after** boot has to unblock it, or the panel would
                    // work while the box stayed greyed out until a restart — the exact bug this whole
                    // feature is about.
                    if let Some(b) = self.boot.as_mut() {
                        b.provider_ready = v.ready;
                    }
                    // The window can talk for the first time: drop everything that said it could not.
                    //
                    // Those rows describe a **state**, not an event in a conversation — the template
                    // notice, "LLM not ready: …", "Open Settings → Model and fill in the endpoint" and
                    // the `log_error` card drawn by the empty state. Left in place they outlive their
                    // truth: the user fills the dialog in, the session starts, and the first screen
                    // still reads "no endpoint configured" (2026-10-28, user request).
                    //
                    // Guarded by `!was_ready` on purpose: switching models in the middle of a long
                    // conversation also arrives here with `ready == true`, and that conversation's own
                    // error rows are history, not a stale caption. Note that `Warn` rows survive —
                    // "python is missing" has nothing to do with the model being configured.
                    //
                    // The two shapes are both here because the host says it twice: `Boot`'s `note`
                    // becomes an `Item::Error` card, while `AgentEvent::Error` ("LLM not ready: …")
                    // becomes an `Item::Note(Error)` line — clearing only one of them left half the
                    // screen still claiming the model was unconfigured (seen in the first run of
                    // this fix).
                    if v.ready && !was_ready {
                        self.log_error = None;
                        self.items.retain(|i| {
                            !matches!(
                                i,
                                Item::Error(_)
                                    | Item::Note(NoteKind::Info | NoteKind::Error, _)
                            )
                        });
                    }
                    self.mdl.set_view(*v);
                }
                FromAgent::Models(v) => self.mdl.set_models(*v),
                FromAgent::TestResult { base_url, model, ok, text } => {
                    self.mdl.set_test(base_url, model, ok, text)
                }
                FromAgent::ModelNotice { ok, text } => self.mdl.set_notice(ok, text),
                FromAgent::HistoryCurrent(id) => {
                    // Keep counts/titles fresh while the sidebar is open; while it is closed
                    // only "the conversation file changed" is worth a directory scan
                    // (otherwise every turn re-reads the disk for nothing).
                    let changed = self.current_history != id;
                    self.current_history = id;
                    if changed || self.show_history {
                        self.refresh_history();
                    }
                }
                FromAgent::Switched { opened, error } => {
                    self.switching = false;
                    // The rows belonged to the conversation (and workspace) we just left. Keeping them
                    // on screen would offer a rollback against a tree the user is no longer looking at.
                    self.rec.close();
                    // The *workspace* chain is still there, and "which of these are mine" is answered
                    // per conversation — so re-read it for the conversation we just opened. Without
                    // this the chip keeps the previous chat's count and every row keeps its old
                    // attribution.
                    self.refresh_recovery();
                    if let Some(e) = error {
                        // When loading fails the agent's context was never touched, so change
                        // nothing here either -- otherwise the UI has already "jumped" while
                        // the model is still in the old conversation.
                        self.items.push(Item::Error(e));
                        continue;
                    }
                    self.current_history = opened.clone();
                    self.adopt_conversation(opened.as_deref());
                    self.refresh_history();
                }
                FromAgent::TurnStart => {
                    // Read the clock before `busy` is set: the turn's own time starts where the
                    // instruction landed, and the turn's readouts then measure from here.
                    let base = self.now();
                    self.live = Some(Live {
                        status: aox::i18n::t("Thinking").into(),
                        call: None,
                        reasoning: String::new(),
                        reasoning_live: false,
                        steps: Vec::new(),
                        interim: Vec::new(),
                        cuts: Vec::new(),
                        pending_final: None,
                        streaming: String::new(),
                        turn_base: base,
                        out: TokenEstimate::default(),
                    });
                    self.busy = true;
                    self.status = aox::i18n::t("Thinking").into();
                    // A new turn rewrites the context from scratch: whatever the last compaction
                    // left on the gauge is no longer the reading anyone needs (and if the previous
                    // turn died mid-compaction, the "compacting…" hint must not survive it either).
                    self.ctx_reset = None;
                    self.compacting = None;
                    // Defensive: a call still marked as "being written" from the previous turn would
                    // otherwise be adopted by a call of the same name in this one.
                    self.stream_calls.clear();
                    // Last turn's writes stop being "this turn's". The workspace view lists what the
                    // turn in flight is touching; the transcript already keeps every earlier edit,
                    // with its own diff, where it happened.
                    wsview::begin_turn(&self.wsview);
                }
                FromAgent::Status(s) => {
                    self.status = s.clone();
                    if let Some(l) = self.live.as_mut() {
                        l.status = s;
                    }
                }
                FromAgent::Content(d) => {
                    // The body delta serves two purposes: (1) keep the token counter climbing;
                    // (2) **show it immediately** -- appending to `streaming` lets the UI type it
                    // out character by character (user request, 2026-10-05: the reply only popped
                    // in once everything was finished, and until then the conversation was
                    // blank).
                    // The activity stream's model segment is still advanced by
                    // `Event(AssistantText)`: inserting it again here would put the same stretch
                    // of text on the timeline twice.
                    if let Some(l) = self.live.as_mut() {
                        l.out.add(&d);
                        l.streaming.push_str(&d);
                    }
                    // Body text means the model is past any call it was writing, so the status must
                    // stop claiming a tool is running (`stream_tool_delta` makes the opposite switch).
                    self.status_back_to_thinking();
                }
                FromAgent::ToolDelta { index, name, delta } => {
                    self.stream_tool_delta(index, name, delta)
                }
                FromAgent::Usage(rec) => {
                    // If the endpoint reports no numbers, hide the whole block -- don't fob it off
                    // with 0s
                    if !rec.is_empty() {
                        // The cache-hit window reads the same record, but only for what the record
                        // alone can say: the lifetime totals and the fallback reading for endpoints
                        // that report nothing per call. Its grid was already fed by `CallUsage`
                        // above -- a turn's `usage` is a *sum* over its calls, so letting it
                        // overwrite the grid would paint blocks the last request never sent.
                        cache::lock(&self.cache).observe_turn(&rec);
                        // The breakdown goes right after this turn's reply (by now the turn has
                        // ended and the tail of `items` is exactly that).
                        self.items.push(Item::Usage(usage_line(&rec)));
                        self.usage_log.push(*rec);
                        // A real reading is in: the post-compaction placeholder has done its job.
                        self.ctx_reset = None;
                        // No `force_follow`: the row lands at the tail, which the sticky scroll
                        // already follows whenever the view is at the bottom. When it is not, the
                        // user is reading history, and appending a line off-screen must not drag
                        // them away from it (2026-10-28: forcing here is what made "read back, then
                        // get yanked to the end when the turn finished" possible).
                    }
                }
                FromAgent::CallUsage { usage, last_prompt } => {
                    // One finished model call, mid-turn. This is what makes the cache map move: the
                    // turn's aggregate only arrives once the whole turn is over, which for a tool
                    // loop can be a dozen calls later.
                    cache::lock(&self.cache).observe_call(&usage, last_prompt);
                }
                FromAgent::UsageLog(log) => {
                    // The whole ledger the agent read back from file after a conversation switch.
                    // The per-turn breakdowns have to go back into the transcript -- when an old
                    // conversation is opened, each turn's bill should be stuck back where it was,
                    // not heaped up at the top.
                    self.usage_log = log;
                    // The gauge belongs to the conversation on screen; a switch replaces the reading
                    // wholesale, and a leftover override from the previous one would be a lie.
                    self.ctx_reset = None;
                    self.compacting = None;
                    self.rebuild_usage_rows();
                    // Rebuild the cache map from that ledger. The map is a history — one tile per
                    // recorded round — so it is replayed whole rather than seeded with the last
                    // line: opening a conversation with 30 turns should show 30 tiles, not one.
                    // Only sums survive in the file, which is why a replayed tile stands for a
                    // round and says so in its tooltip.
                    let mut cv = cache::lock(&self.cache);
                    cv.reset();
                    cv.replay(&self.usage_log);
                }
                FromAgent::ContextMap(segs) => {
                    // The section breakdown of the request just sent. Converted into the window's own
                    // `Seg` here rather than over there: the message crosses from the core crate, and
                    // the window's type belongs to the GUI crate.
                    cache::lock(&self.cache).observe_context(
                        segs.into_iter().map(|(label, tokens)| cache::Seg { label, tokens }).collect(),
                    );
                }
                FromAgent::Reasoning(d) => {
                    if let Some(l) = self.live.as_mut() {
                        // `self.now()` cannot be used here (it borrows all of self), while `l` is
                        // holding a mutable borrow of `self.live`. `self.clock` is a *different*
                        // field, so borrowing it alone is fine — and it has to be this clock, not
                        // `Instant::now()`, or the model segment would start on the wall clock and
                        // drift away from every other record the moment a pause happened.
                        let at = self.clock.now();
                        if !l.reasoning_live {
                            l.reasoning_live = true;
                            activity::lock(&self.act).model_begin(at);
                        }
                        if l.reasoning.len() < 120_000 {
                            l.reasoning.push_str(&d);
                        }
                        // Reasoning counts as output too: the server's completion_tokens already
                        // includes it, so counting only the body would come out noticeably low.
                        l.out.add(&d);
                        activity::lock(&self.act).model_push(&d);
                    }
                    // Same switch as the body-text arm: thinking again means no tool is running.
                    self.status_back_to_thinking();
                            }
                FromAgent::ReasoningEnd => {
                    // Reasoning ending does not mean the model's segment has ended: it may go on
                    // to emit body text or to call a tool, and either of those closes this segment
                    // on its own.
                    if let Some(l) = self.live.as_mut() {
                        l.reasoning_live = false;
                    }
                }
                FromAgent::Attempt { n, delay_ms } => {
                    if n == 0 {
                        // A request is starting. Remember where its rows begin so a retry can undo
                        // exactly this attempt and nothing before it.
                        self.attempt_mark = self.live.as_ref().map(|l| l.steps.len());
                    } else {
                        // The abandoned attempt is being thrown away: its half-typed body text, the
                        // tool rows its streamed fragments opened, and the clock it was holding.
                        if let Some(l) = self.live.as_mut() {
                            l.streaming.clear();
                            if let Some(mark) = self.attempt_mark {
                                l.steps.truncate(mark);
                            }
                            l.call = None;
                        }
                        self.stream_calls.clear();
                        self.status = aox::tf!("Retrying in {} s", format!("{:.1}", delay_ms as f32 / 1000.0));
                    }
                }
                FromAgent::Event(ev) => self.on_event(ev),
                FromAgent::TurnEnd => {
                    self.finish_turn(false);
                    // A panel opened while the turn was running could not ask for its list (the
                    // driver loop is not reading the queue during a turn). Now it can.
                    if let Some(req) = self.rec.turn_ended() {
                        self.send_recovery(req);
                    } else {
                        // Every turn that changed files just published a point; the count on the
                        // chip has to follow even when the panel is shut.
                        self.refresh_recovery();
                    }
                }
                FromAgent::TurnAborted => {
                    self.finish_turn(true);
                }
                FromAgent::AuthRequest { tool, args, reply } => {
                    self.auth = Some(AuthCard { tool: tool.clone(), args: args.clone() });
                    self.auth_reply = Some(reply);
                    self.waiting_auth = true;
                    self.auth_shown_at = Instant::now();
                    let at = self.now();
                    activity::lock(&self.act).note(
                        ActKind::Auth,
                        &aox::tf!("Request authorization · {}", tool),
                        &args,
                        at,
                    );
                }
                // A multiple-choice question asked via `ask_user`. Same pattern as an
                // authorization request, but it **does not block input** (the user has to be able
                // to type an answer), so the `input` / `can_send` switches are left alone here --
                // only the card is hung up and the state changed.
                FromAgent::ChoiceRequest { question, options, reply } => {
                    self.status = aox::i18n::t("Waiting for your choice").into();
                    self.choice = Some(ChoiceCard { question: question.clone(), options });
                    // Left-over text from the previous question must not be carried over: that
                    // produces "I haven't typed anything yet, but there's text in the box"
                    self.choice_text.clear();
                    self.choice_reply = Some(reply);
                    // The option slot sits above the input box and the user is probably looking at
                    // the end of the transcript right now -- it is only visible once you scroll to
                    // the bottom. Without the scroll it reads as "the model asked something and then
                    // nothing happened".
                    self.force_follow = true;
                    let at = self.now();
                    activity::lock(&self.act).note(ActKind::Info, aox::i18n::t("Waiting for your choice"), &question, at);
                }
                FromAgent::Fatal(e) => {
                    self.items.push(Item::Error(e.clone()));
                    self.note_act(ActKind::Error, aox::i18n::t("Fatal error"), &e);
                    if self.live.is_some() {
                        self.finish_turn(false);
                    }
                }
            }
        }
    }

    /// A fragment of a tool call the model is **still writing**: open its row once, then keep the
    /// reading next to it climbing as the fragments arrive.
    ///
    /// Without this the entire stretch in which the model writes a call is invisible — the tool
    /// itself has not started, so there is nothing on screen to look at, and for `write_file`
    /// (whose arguments *are* the file) that stretch is the whole job.
    pub(super) fn stream_tool_delta(&mut self, index: u64, name: String, delta: String) {
        if self.live.is_none() {
            return;
        }
        // One entry per call, keyed by the API's own call index: several calls may be in flight at
        // once and their fragments interleave.
        let slot = match self.stream_calls.iter().position(|c| c.index == index) {
            Some(i) => i,
            None => {
                self.stream_calls.push(StreamCall {
                    index,
                    name: String::new(),
                    args: String::new(),
                    step: usize::MAX,
                });
                self.stream_calls.len() - 1
            }
        };
        // The name only rides on the first fragment of a call; the fragments after it have to
        // inherit it, or the row would be nameless.
        if !name.is_empty() {
            self.stream_calls[slot].name = name;
        }
        self.stream_calls[slot].args.push_str(&delta);

        let call_name = self.stream_calls[slot].name.clone();
        if call_name.is_empty() {
            return; // No name yet, so there is no row to draw -- wait for the fragment that has it.
        }
        // Only the host's file writer has a reading to show: its arguments carry the text itself,
        // so "how many lines have arrived" is meaningful. Everything else gets a bare marquee.
        let lines = if call_name == WRITE_FILE_TOOL {
            aox::llm::partial_json_lines(&self.stream_calls[slot].args, "content")
        } else {
            0
        };
        // The path is the first thing in the arguments, so it is normally in hand within a fragment
        // or two — and naming the file is most of what this row has to say. Only the whole value is
        // taken: a half-typed `E:/exam` would read as a glitch rather than as progress.
        let subject = if SUBJECT_TOOLS.contains(&call_name.as_str()) {
            aox::llm::partial_json_string(&self.stream_calls[slot].args, "path")
        } else {
            None
        };
        // Same reasoning for the purpose: `purpose` is a short string and `command` is the first
        // argument of a shell call, so either is normally complete within a fragment or two. Taking
        // only whole values keeps a half-arrived command from being drawn as if it were the command.
        let purpose = match aox::llm::partial_json_string(
            &self.stream_calls[slot].args,
            aox::tool::PURPOSE_ARG,
        ) {
            Some(p) => Some(clip_head(&flat_line(&p), PURPOSE_MAX)),
            None if matches!(call_name.as_str(), "shell" | "bash") => {
                aox::llm::partial_json_string(&self.stream_calls[slot].args, "command")
                    .map(|c| clip_head(&flat_line(&c), PURPOSE_MAX))
            }
            None => None,
        };
        let fresh = self.stream_calls[slot].step == usize::MAX;
        let step = self.stream_calls[slot].step;
        let plugin = self.plugin_of(&call_name);
        let Some(l) = self.live.as_mut() else { return };
        let step = if fresh {
            l.steps.push(StepView {
                name: call_name.clone(),
                plugin,
                purpose: purpose.clone(),
                subject: subject.clone(),
                // Left empty until the call really runs: the row shows no arguments (see
                // `step_row`), and what the model eventually sends is filled in by the `ToolCall`
                // arm, which adopts this row.
                args: String::new(),
                result: None,
                diff: None,
                // Not known until the call returns, and there is nothing to unfold before that.
                diff_detail: None,
                expanded: false,
                images: Vec::new(),
            });
            l.steps.len() - 1
        } else {
            step
        };
        self.stream_calls[slot].step = step;
        if let Some(st) = l.steps.get_mut(step) {
            // Either reading may land a fragment after the row does — keep the row in step with
            // whatever has arrived.
            if subject.is_some() {
                st.subject = subject;
            }
            if purpose.is_some() {
                st.purpose = purpose;
            }
            // Only ever the added side: how many lines are *disappearing* cannot be known until the
            // old file is read, which happens when the call runs.
            if lines > 0 {
                st.diff = Some((lines, 0));
            }
        }
        if fresh {
            // Name the tool that is working. "Thinking" would be plainly wrong once the model is
            // visibly writing a call.
            let want = aox::tf!("Running {}", call_name);
            l.status = want.clone();
            self.status = want;
        }
    }

    /// The model is producing text again, so no tool can be running: put the status line back to
    /// "Thinking".
    ///
    /// Called on every delta, hence the comparison before anything is built — this costs a string
    /// compare in the common case and allocates nothing.
    pub(super) fn status_back_to_thinking(&mut self) {
        // A tool cannot be running while the model is writing, so this is also where the call's own
        // clock retires. Cleared unconditionally: leaving it behind would show a frozen "0:47 / 2:00"
        // next to a "Thinking" label.
        if let Some(l) = self.live.as_mut() {
            l.call = None;
        }
        let want = aox::i18n::t("Thinking");
        if self.status != want {
            self.status = want.to_string();
            if let Some(l) = self.live.as_mut() {
                l.status = want.to_string();
            }
        }
    }

    pub(super) fn on_event(&mut self, ev: AgentEvent) {
        // The activity stream records only "how a turn ran": events outside a turn (switching
        // conversations, the tail left behind after a fatal error, ...) never go on the
        // timeline.
        let live = self.live.is_some();
        match ev {
            AgentEvent::AssistantText(t) => {
                match self.live.as_mut() {
                    Some(l) => {
                        let at = self.clock.now();
                        // The model starting to emit body text = the "model" segment starts (if it
                        // hasn't already)
                        activity::lock(&self.act).model_begin(at);
                        if let Some(prev) = l.pending_final.replace(t.clone()) {
                            l.interim.push(prev);
                        }
                        // The deltas are done, so reconcile against the authoritative text once: if a
                        // frame is dropped while typing, the characters on screen end up a few off the
                        // real text, and that has to be smoothed out at the very end.
                        l.streaming = t;
                    }
                    None => self.items.push(Item::Assistant(t)),
                }
            }
            AgentEvent::InterimText(t) => {
                if let Some(l) = self.live.as_mut() {
                    let at = self.clock.now();
                    activity::lock(&self.act).model_begin(at);
                    // The stage note is final here: it goes into `interim` (which stays in the live
                    // area and ends up in the process card afterwards) and `streaming` is cleared --
                    // the next stretch of body text starts typing from a fresh line.
                    l.streaming.clear();
                    l.interim.push(t);
                    // Record "how many steps ran before this sentence": the live area uses it to
                    // draw the preceding calls as their own card.
                    l.cuts.push(l.steps.len());
                }
            }
            AgentEvent::ToolCall { name, args } => {
                let plugin = self.plugin_of(&name);
                // Read the file name off the parsed arguments *before* they are flattened to text --
                // this is the authoritative reading, and the streaming one (which has to decode a
                // half-arrived string) is only ever a preview of it.
                let subject = subject_from_value(&name, &args);
                let purpose = purpose_from_value(&name, &args);
                let args = compact_json(&args);
                // The model has just finished writing this call, so its row is normally already on
                // screen (opened by the streaming fragments — see `stream_tool_delta`). Adopt that
                // row rather than pushing a second one for the same call; `position` takes the first
                // unclaimed entry, which is the right one when a response calls the same tool twice.
                let mut adopted = false;
                if let Some(i) = self
                    .stream_calls
                    .iter()
                    .position(|c| c.name == name && c.step != usize::MAX)
                {
                    let step = self.stream_calls[i].step;
                    if let Some(l) = self.live.as_mut() {
                        if let Some(st) = l.steps.get_mut(step) {
                            st.args = args.clone();
                            // The streaming reading of the path is a preview; this one wins.
                            if subject.is_some() {
                                st.subject = subject.clone();
                            }
                            if purpose.is_some() {
                                st.purpose = purpose.clone();
                            }
                            adopted = true;
                        }
                    }
                    // The live reading (`diff`) is deliberately left in place: there is no reason to
                    // blank it for the moment between "model finished writing" and "tool returned".
                    if adopted {
                        self.stream_calls.remove(i);
                    }
                }
                if !adopted {
                    if let Some(l) = self.live.as_mut() {
                        l.steps.push(StepView {
                            name: name.clone(),
                            plugin: plugin.clone(),
                            purpose,
                            subject,
                            args: args.clone(),
                            result: None,
                            // Unknown until the call returns; filled in by the `ToolResult` arm.
                            diff: None,
                            diff_detail: None,
                            expanded: false,
                            images: Vec::new(),
                        });
                    }
                }
                if live {
                    // Only one bar is opened on the activity stream: call -> execute -> return is a
                    // single thing, and splitting it into three makes it impossible to see how long
                    // the step actually took.
                    activity::lock(&self.act).tool_begin(
                        &name,
                        plugin.as_deref().filter(|p| *p != "meta"),
                        &args,
                        self.now(),
                    );
                }
            }
            AgentEvent::ToolRunning { name, timeout_secs, expect_secs } => {
                self.status = aox::tf!("Running {}", name);
                let started = self.now();
                if let Some(l) = self.live.as_mut() {
                    l.call = Some(RunningCall { started, cap: timeout_secs, expect: expect_secs });
                }
                if live {
                    activity::lock(&self.act).tool_note(&aox::tf!("Timeout limit {}s", timeout_secs));
                }
            }
            AgentEvent::ToolResult { name, is_error, preview, diff, diff_detail, path } => {
                // A file write is also a row in the workspace view: that window answers "what did
                // this turn touch", which is the one thing this event knows and the transcript can
                // only say in prose. Recorded before the step is adopted, so the list and the
                // "+N / −N" on the transcript row appear on the same frame.
                if let Some(p) = path {
                    wsview::note_edit(&self.wsview, p, diff, diff_detail.clone());
                }
                if let Some(l) = self.live.as_mut() {
                    // The call is over: retire its clock before the row shows its result.
                    l.call = None;
                    if let Some(st) =
                        l.steps.iter_mut().rev().find(|s| s.name == name && s.result.is_none())
                    {
                        st.result = Some((is_error, preview.clone()));
                        st.diff = diff;
                        // What the row can be unfolded into. The row is drawable-with-a-caret from
                        // this instant on, which is the same instant the "+N / −N" lands.
                        st.diff_detail = diff_detail;
                    }
                }
                if live {
                    activity::lock(&self.act).tool_end(is_error, &preview, self.now());
                }
            }
            AgentEvent::Info(m) => {
                self.note_act(ActKind::Info, aox::i18n::t("Info"), &m);
                if self.live.is_none() {
                    self.items.push(Item::Note(NoteKind::Info, m));
                }
            }
            AgentEvent::Warn(m) => {
                self.note_act(ActKind::Warn, aox::i18n::t("Warning"), &m);
                if self.live.is_none() {
                    self.items.push(Item::Note(NoteKind::Warn, m));
                }
            }
            AgentEvent::Task(t) => {
                // Nothing is drawn here: the bar above the input reads it on every frame (see
                // `plan_bar`). Storing it is enough, and it must survive the end of the turn --
                // a plan outlives the work that wrote it.
                self.plan = PlanView::of(&t);
                self.task = Some(*t);
            }
            AgentEvent::Recycled(n) => {
                // Two audiences, two shapes (the same split as `Images` below): the activity stream
                // gets a bar — "how this turn ran" — and the human gets a **toast** that fades on
                // its own.
                //
                // The toast is set unconditionally, not only while a turn is running: a delete the
                // user asked for can easily finish after the turn that ordered it has ended, and
                // "where did my file go" is precisely the moment this has to be visible.
                self.note_act(ActKind::Info, aox::i18n::t("Recycle bin"), &aox::recycle::notice(n));
                // One `remove` call takes one path, so "delete two files" arrives as two events.
                // Merge the new one into the total already on screen — two flashes of "1 file"
                // read as a bug, and the number is the part the user is actually looking at.
                // Restarting the timer keeps the merged toast up long enough to be read.
                match &mut self.toast {
                    Some((count, shown_at)) if shown_at.elapsed().as_secs_f32() < TOAST_SECS => {
                        *count += n;
                        *shown_at = Instant::now();
                    }
                    _ => self.toast = Some((n, Instant::now())),
                }
                // The tree is redrawn only when something asks for it, and a recycled conversation is
                // exactly such a thing: without this, a row right-clicked away would sit in the list
                // until the next turn ended. Done **here** rather than at the point the delete was
                // requested -- the move happens on the agent thread, so a refresh issued then would
                // race it and put the row straight back on screen.
                if self.show_history {
                    self.refresh_history();
                }
            }
            // ── Context compaction (`aox::compact`) ──
            // Two events, because the user sees two different things: while the summary call is in
            // flight the input box shows "compacting…" (the one moment the turn genuinely pauses),
            // and when it lands the gauge drops to the new figure instead of staying pinned at full
            // until the next request comes back.
            AgentEvent::ContextCompacting { used, budget } => {
                self.compacting = Some(std::time::Instant::now());
                self.note_act(
                    ActKind::Info,
                    aox::i18n::t("Context"),
                    &aox::tf!("compacting ~{} of {} tokens", used, budget),
                );
            }
            AgentEvent::ContextCompacted { before, after, folded, elided } => {
                self.compacting = None;
                self.ctx_reset = Some(after);
                self.note_act(
                    ActKind::Info,
                    aox::i18n::t("Context"),
                    &aox::tf!(
                        "compacted ~{} → ~{} tokens ({} messages folded, {} tool results stubbed)",
                        before,
                        after,
                        folded,
                        elided
                    ),
                );
            }
            AgentEvent::ToolImages { tool, paths } => {
                // Images produced by a tool: **attached to that call's own row**.
                //
                // It used to be a separate `Item::Images`, which could only sit in the transcript's
                // normal flow -- and because the host message carrying the attachments starts a new
                // user message, that meant the picture appeared at the top of a fresh block, with a
                // visible "(host hint) …" sentence beside it. The hint is protocol, not content: it
                // stays in the model's request but is no longer drawn. The picture now belongs to
                // the call that produced it.
                if let Some(l) = self.live.as_mut() {
                    attach_tool_images(&mut l.steps, &tool, &paths);
                } else {
                    // Late event (no turn in flight): keep it rather than dropping it, but still
                    // below the conversation instead of pretending it belongs to a call.
                    self.items.push(Item::Images(paths));
                }
            }
            AgentEvent::Error(m) => {
                self.note_act(ActKind::Error, aox::i18n::t("Error"), &m);
                if self.live.is_none() {
                    self.items.push(Item::Note(NoteKind::Error, m));
                }
            }
        }
    }

    /// A turn ends: collapse the process into one row, then paste the final answer beneath it.
    pub(super) fn finish_turn(&mut self, aborted: bool) {
        self.busy = false;
        // An aborted turn can be aborted *during* a compaction; the hint must not outlive the turn.
        self.compacting = None;
        self.waiting_auth = false;
        self.auth = None;
        self.auth_reply = None;
        // At the end of a turn there shouldn't be an option slot hanging around (the model has
        // moved on). If there is, the UI failed to clean up -- so drop it too: the peer isn't
        // waiting any more (that turn's `choose()` returned long ago) and `tx.send` fails
        // silently. Keeping it is actually dangerous: if someone calls `answer_choice` before the
        // next `ChoiceRequest`, that lands on a channel nobody is listening to, and the UI looks
        // like "I clicked it and nothing happened".
        self.choice = None;
        self.choice_reply = None;
        self.status = aox::i18n::t("Ready").into();
        let Some(mut l) = self.live.take() else {
            self.stream_calls.clear();
            return;
        };
        // A call the model was still writing when the turn ended never ran — the response was cut
        // off, or the turn was aborted. Its row has to go, otherwise the process card keeps a
        // marquee spinning forever for a step that never happened.
        //
        // They are always the *trailing* rows (a streaming row is appended at the tail, and a row
        // stops streaming once its `ToolCall` arrives), so one truncation removes exactly them.
        if !self.stream_calls.is_empty() {
            let first = self
                .stream_calls
                .iter()
                .map(|c| c.step)
                .filter(|s| *s != usize::MAX)
                .min();
            if let Some(first) = first {
                l.steps.truncate(first);
            }
            self.stream_calls.clear();
        }
        // Working time, not wall time: a turn the user sat on for ten minutes while answering a
        // question is not a ten-minute turn (see [`Clock`]).
        let elapsed = (self.now() - l.turn_base).max(0.0);

        if !l.steps.is_empty() {
            self.items.push(Item::Process(Process {
                steps: l.steps,
                interim: l.interim,
                reasoning: l.reasoning,
                elapsed: Some(elapsed),
                expanded: false,
                aborted,
            }));
        }
        if let Some(f) = l.pending_final {
            self.items.push(Item::Assistant(f));
        }
        let at = self.now();
        let pending = {
            let mut act = activity::lock(&self.act);
            // A turn ends: close every segment still open (don't leave a bar that keeps growing
            // forever, longer and longer).
            act.end_round(at);
            if aborted {
                act.note(ActKind::Warn, aox::i18n::t("Task interrupted"), &aox::tf!("Ran for {}s", format!("{elapsed:.1}")), at);
            }
            // Once closed, take this turn's new records and hand them to the agent thread to
            // write into **this conversation's file**. Hand them over rather than opening the file
            // from the UI: the handle is on the agent side, and "new conversation / rollback on
            // interrupt" also happens on that thread; this message goes to the back of the queue,
            // so the ordering is naturally right -- it can't leak into the next conversation.
            act.export_acts()
        };
        if !pending.is_empty() {
            let _ = self.tx.send(ToAgent::Act(pending));
        }
        // The turn's rows are being swapped in for the live block here, so the content height
        // changes either way; the sticky scroll keeps a view that is at the bottom at the bottom.
        // Deliberately **not** `force_follow`: the user may be half-way up re-reading what the turn
        // said, and finishing the turn must not yank them to the end.
    }
}

