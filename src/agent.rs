use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::attach;
use crate::auth::AuthLevel;
use crate::history::UsageRecord;
use crate::llm::{LlmProvider, TokenEstimate, Usage};
use crate::message::Message;
use crate::runtime::Runtime;
use crate::tool::{ToolKind, ToolOutput};
use crate::ui::{AgentEvent, AuthDecision, Ui};

/// Per-turn usage accumulator.
/// It is a field of `Agent` rather than a local variable of some function, for two reasons.
#[derive(Default)]
struct TurnAcc {
    calls: u32,
    total: Usage,
    /// Size of the last request's input = current context footprint.
    last_prompt: Option<u64>,
}

pub struct Agent {
    llm: Arc<crate::provider_live::LiveProvider>,
    rt: Arc<Runtime>,
    messages: Vec<Message>,
    /// The persister for this conversation (the write end of the storage seam).
    log: Option<Box<dyn crate::store::Session>>,
    /// Session birth info (origin, space, room); affects only the first meta line and list grouping, never paths.
    birth: crate::store::Birth,
    /// Persist errors are reported only once, so we don't repeat the same warning every turn.
    log_error_reported: bool,
    /// Cumulative usage for this conversation so far (reloaded from file or reset on switch/new conversation).
    session_usage: Usage,
    session_calls: u32,
    /// Per-turn usage records for this conversation, one line per `kind:"usage"` row in the conversation file.
    usage_log: Vec<UsageRecord>,
    /// The workplace record this conversation has already persisted (tier + root + this conversation's own dir)...
    workplace_logged: Option<crate::workplace::Record>,
    /// This turn's usage accumulator (driven by `AgentLoop` via Agent::accrue_call).
    turn: TurnAcc,
    /// The workspace root already baked into `messages[0]` (the system prompt).
    prompt_ws: PathBuf,
    /// The current task checkpoint (`crate::task`), when this conversation has one.
    task: Option<crate::task::TaskState>,
    /// The checkpoint as last written to the file, for "write only when it really changed".
    task_saved: Option<crate::task::TaskState>,
    /// The last compacted request body (`crate::compact`), with its watermark.
    view_cache: Option<crate::compact::ViewCache>,
    /// Whether the "cannot get under the budget by folding history" warning has already been shown...
    compact_warned: bool,
    /// Which tools this conversation is advertising (see crate::tool::scope).
    tool_scope: crate::tool::scope::Scope,
    /// The tail of the last tool result the host saw, kept for `verify`'s `output_contains`.
    last_output: Option<String>,
}

/// The workspace root that should appear in the system prompt.
fn prompt_workspace_root(rt: &Runtime) -> PathBuf {
    rt.workplace.snapshot().1.unwrap_or_else(|| rt.root.clone())
}

impl Agent {
    /// Assemble an Agent.
    /// The system prompt is composed from "segments" by crate::prompt::build: built-in segments, the runtime...
    pub fn new(rt: Arc<Runtime>) -> Self {
        // The provider comes from the runtime, not from an argument: that is what makes "change the model in the GUI...
        let llm = rt.provider.clone();
        Self::assemble(llm, rt)
    }

    /// Assemble an Agent around a specific provider handle, bypassing the runtime's.
    pub fn new_with(llm: Arc<dyn LlmProvider>, rt: Arc<Runtime>) -> Self {
        let live = crate::provider_live::LiveProvider::with_handle(rt.provider.cfg(), llm);
        Self::assemble(live, rt)
    }

    fn assemble(llm: Arc<crate::provider_live::LiveProvider>, rt: Arc<Runtime>) -> Self {
        let sys = crate::prompt::build(&rt);
        // Remember which workspace that prompt names, so `refresh_system_prompt` can tell when it
        // has gone stale (see the `prompt_ws` field docs).
        let prompt_ws = prompt_workspace_root(&rt);
        // Built before `rt` is moved into `Self` below.
        let tool_scope = crate::tool::scope::Scope::from_config(&rt.config.tools);
        Self {
            llm,
            rt,
            messages: vec![Message::system(sys)],
            log: None,
            birth: crate::store::Birth::default(),
            log_error_reported: false,
            session_usage: Usage::default(),
            session_calls: 0,
            usage_log: Vec::new(),
            workplace_logged: None,
            turn: TurnAcc::default(),
            prompt_ws,
            task: None,
            task_saved: None,
            view_cache: None,
            compact_warned: false,
            tool_scope,
            last_output: None,
        }
    }

    /// This conversation's history id (None before a conversation has started).
    /// The id is fixed by the storage backend when created (for the jsonl backend it's the filename stem); this just relays it.
    pub fn history_id(&self) -> Option<&str> {
        self.log.as_ref().map(|l| l.id())
    }

    /// Birth marker for later sessions; call before the first message, since meta is fixed at file creation.
    pub fn set_birth(&mut self, birth: crate::store::Birth) {
        self.birth = birth;
    }

    /// Persist the current workplace state into this conversation's archive (only writing when it actually changed).
    pub fn persist_workplace(&mut self) {
        let Some(log) = self.log.as_mut() else { return };
        let cur = self.rt.workplace.persisted();
        if self.workplace_logged.as_ref() == Some(&cur) {
            return;
        }
        log.append_workplace(&cur);
        self.workplace_logged = Some(cur);
    }

    /// Rebuild the system prompt (`messages[0]`) so it names the current workspace.
    fn refresh_system_prompt(&mut self) {
        let cur = prompt_workspace_root(&self.rt);
        if cur == self.prompt_ws {
            return;
        }
        let sys = crate::prompt::build(&self.rt);
        match self.messages.first_mut() {
            Some(first) => *first = Message::system(sys),
            None => self.messages.push(Message::system(sys)),
        }
        self.prompt_ws = cur;
    }

    /// Ensure the current conversation already has an on-disk file.
    fn ensure_log(&mut self, ui: &mut dyn Ui) {
        if self.log.is_some() {
            return;
        }
        let cfg = &self.rt.config.history;
        if !cfg.enabled {
            return;
        }
        // The model **in force**, which after a `/model` switch is no longer the one in `config.toml`:
        // the file name (and the history header) should say which model the conversation ran on.
        let model = self.rt.provider.cfg().model;
        // No directory argument: since 2026-11-05 every conversation lives in the one store, and "which workspace /...
        match self.rt.store.create(Some(&model), &self.birth) {
            Ok(log) => self.log = Some(log),
            Err(e) if !self.log_error_reported => {
                self.log_error_reported = true;
                ui.emit(AgentEvent::Warn(crate::tf!(
                    "conversation history cannot be written to {} ({}): this conversation will not be saved",
                    self.rt.store.location().display(),
                    format!("{e:#}")
                )));
            }
            Err(_) => {}
        }
    }

    /// Turn one user input (+ attachment paths) into a message.
    fn user_message(&self, input: &str, files: &[PathBuf]) -> Message {
        if files.is_empty() {
            return Message::user(input);
        }
        let (items, problems) = attach::probe_all(files);
        let text = if problems.is_empty() {
            input.to_string()
        } else {
            crate::tf!(
                "{}\n\n{} the following attachments could not be attached:\n{}",
                input,
                crate::history::HOST_PREFIX,
                problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n")
            )
        };
        match Message::user_with_files(&text, &items) {
            Ok(m) => m,
            Err(e) => Message::user(crate::tf!("{}\n\n{} an attachment could not be attached: {}", text, crate::history::HOST_PREFIX, e)),
        }
    }

    /// Push a message and persist it in sync.
    pub fn push_message(&mut self, msg: Message, is_error: Option<bool>) {
        if let Some(log) = self.log.as_mut() {
            log.append(&msg, is_error);
        }
        self.messages.push(msg);
    }

    /// Current conversation history length (used for rollback on interrupt).
    pub fn messages_len(&self) -> usize {
        self.messages.len()
    }

    /// Break the request about to be sent into sections, with a rough token estimate for each.
    pub fn context_sections(&self, msgs: &[Message], schemas: &[Value]) -> Vec<(&'static str, u64)> {
        fn est(s: &str) -> u64 {
            let mut e = TokenEstimate::default();
            e.add(s);
            e.tokens().max(1)
        }

        let mut out: Vec<(&'static str, u64)> = Vec::new();
        let mut rest: &[Message] = msgs;

        // The system prompt is messages[0] whenever it is present (see `refresh_system_prompt`).
        if rest.first().is_some_and(|m| m.role == "system") {
            out.push(("System prompt", est(rest[0].text().unwrap_or_default())));
            rest = &rest[1..];
        }

        // Tool schemas are a field of their own in the OpenAI wire format, so they are a section of
        // their own here rather than part of the system prompt.
        if !schemas.is_empty() {
            out.push((
                "Tool definitions",
                schemas.iter().map(|s| est(&s.to_string())).sum::<u64>(),
            ));
        }

        // The last user message is this turn's instruction; everything before it is history.
        let (head, tail) = match rest.split_last() {
            Some((last, head)) if last.role == "user" => (head, Some(last)),
            _ => (rest, None),
        };
        if !head.is_empty() {
            let mut n = 0u64;
            for m in head {
                n += est(m.text().unwrap_or_default());
                // An assistant turn's tool calls travel in their own field, arguments included — for
                // a `write_file` that JSON is most of the message.
                for c in m.tool_calls.iter().flatten() {
                    n += est(&c.function.arguments);
                }
            }
            out.push(("Conversation history", n));
        }
        if let Some(last) = tail {
            out.push(("Your input", est(last.text().unwrap_or_default())));
        }
        out
    }

    /// Per-turn usage records for the current conversation (so the UI can show "how much each turn cost").
    pub fn usage_log(&self) -> &[UsageRecord] {
        &self.usage_log
    }

    /// Append a batch of activity-stream records (produced by the UI side, written into the same conversation file).
    #[allow(dead_code)]
    pub fn append_acts(&mut self, acts: &[Value]) {
        let Some(log) = self.log.as_mut() else { return };
        for a in acts {
            log.append_act(a);
        }
    }

    /// Switch to an archived conversation (by id): load its messages back into context and reconnect the persister...
    pub fn resume(&mut self, id: &str) -> Result<usize> {
        // A conversation switch must not leave a recovery transaction belonging to the old
        // workspace open.
        let _ = self.rt.recovery.abort_turn();
        let items = self.rt.store.load(id)?;
        let log = self.rt.store.open(id)?;
        // Keep only the system prompt (entry 0); replace everything else with the archived messages.
        // Deliberately leaving the system prompt out of the archive (it changes every turn), so only here can it be preserved.
        self.messages.truncate(1);
        self.messages.extend(items.iter().map(|i| i.msg.clone()));
        self.log = Some(log);
        self.log_error_reported = false;
        // Pick the usage ledger back up from the storage layer.
        self.usage_log = self.rt.store.load_usage(id);
        self.session_usage = Usage::default();
        self.session_calls = 0;
        for r in &self.usage_log {
            self.session_usage.accrue(&r.usage);
            self.session_calls += r.calls;
        }
        // The workplace follows the conversation, not the process: restore the record from this conversation's archive...
        self.workplace_logged = match self.rt.store.load_workplace(id) {
            Some(rec) => {
                self.rt.workplace.restore(&rec);
                Some(rec)
            }
            None => None,
        };
        // The task checkpoint travels with the conversation (see `crate::task`).
        self.task = self.rt.store.load_task(id).and_then(|v| crate::task::TaskState::from_value(&v));
        // Aligned to the file's state, so "nothing changed" really writes nothing this turn.
        self.task_saved = self.task.clone();
        // A different conversation means a different message list; any compacted view belongs to the
        // old one. Cheap to drop, and the first request will rebuild it if it is needed.
        self.view_cache = None;
        self.compact_warned = false;
        // A different conversation means a different set of tools "already discovered". The config's
        // pinned names survive (`Scope::reset` only clears what this conversation turned on).
        self.tool_scope.reset();
        // The output buffer belongs to the conversation that produced it; evidence about "what the last tool said"...
        self.last_output = None;
        if let Some(st) = &self.task {
            // ⚠️ **Memory-only** — see `push_ephemeral`. It is regenerated on every resume, so writing
            // it into the file would append the same paragraph once per open.
            let note = crate::tf!(
                "{} a task checkpoint was restored for this conversation. It says where the work stood when it was written — not what the files look like now.\n\n{}",
                crate::history::HOST_PREFIX,
                st.context_block()
            );
            self.push_ephemeral(Message::user(note));
        }
        Ok(items.len())
    }

    /// Rename a conversation by appending a `kind:"title"` row to its file.
    #[allow(dead_code)]
    pub fn rename_conversation(&mut self, id: &str, title: Option<&str>) -> Result<()> {
        let title = title.map(str::trim).filter(|t| !t.is_empty());
        if self.history_id() == Some(id) {
            if let Some(log) = self.log.as_mut() {
                log.append_title(title);
            }
            return Ok(());
        }
        let mut session = self.rt.store.open(id)?;
        session.append_title(title);
        Ok(())
    }

    /// Start a new conversation.
    /// The old conversation needs no "save" action at all: it was appended as you talked, so it already sits complete...
    pub fn new_conversation(&mut self) {
        // Same reason as `resume`: a new conversation is a different workspace context.
        let _ = self.rt.recovery.abort_turn();
        self.messages.truncate(1);
        self.log = None;
        self.log_error_reported = false;
        self.session_usage = Usage::default();
        self.session_calls = 0;
        self.usage_log.clear();
        // The workplace turns the page too: always back to the "default" tier — the tier temporarily switched...
        self.rt.workplace.reset_for_new_conversation();
        // The new conversation's file isn't created yet; the persisted record is filled in by `persist_workplace` on the first message;
        // here we clear "already persisted" first, so we don't compare against the last conversation's record and miss writing that first message.
        self.workplace_logged = None;
        // A checkpoint belongs to the conversation it was recorded in.
        self.task = None;
        self.task_saved = None;
        self.view_cache = None;
        self.compact_warned = false;
        self.last_output = None;
        // A different conversation means a different set of tools "already discovered". The config's
        // pinned names survive (`Scope::reset` only clears what this conversation turned on).
        self.tool_scope.reset();
    }

    /// Drop everything from `len` onward, in memory and on disk.
    pub fn truncate_messages(&mut self, len: usize) {
        self.messages.truncate(len);
        if let Some(log) = self.log.as_mut() {
            log.rewrite(&self.messages);
        }
    }

    /// Where to roll the conversation back to when the user interrupts a running turn.
    pub fn interrupt_rollback_point(&self, mark: usize) -> usize {
        let msgs = &self.messages;
        // At most one batch can be incomplete, and it is the last one: the loop only asks the model again once every call...
        let Some(i) = msgs.iter().rposition(|m| {
            m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|c| !c.is_empty())
        }) else {
            // No tool call anywhere: the tail is a plain message, so nothing needs rolling back.
            return msgs.len();
        };
        if crate::message::is_legal_body(&msgs[i..]) {
            // The batch finished; the abort landed while waiting for the next reply.
            msgs.len()
        } else {
            // Drop the dangling (or half-answered) batch, keeping the user's instruction and every completed step before...
            i.max(mark)
        }
    }

    /// Roll the conversation back after an interrupt, keeping every completed step.
    pub fn rollback_interrupted_turn(&mut self, mark: usize) {
        let n = self.interrupt_rollback_point(mark);
        if n < self.messages.len() {
            self.truncate_messages(n);
        }
    }

    /// Drop the oldest chunk of the body: keep the system prompt, plus `messages[cut..]` (everything from `cut`...
    pub fn drop_oldest(&mut self, cut: usize) {
        if cut <= 1 || cut >= self.messages.len() {
            return;
        }
        self.messages.drain(1..cut);
        if let Some(log) = self.log.as_mut() {
            log.rewrite(&self.messages);
        }
    }

    /// The loop itself is not here — it's provided by `Runtime.loop_impl` (the loop seam, see `src/agent_loop.rs`)...
    pub async fn run_turn(&mut self, ui: &mut dyn Ui, input: &str, files: &[PathBuf]) -> Result<()> {
        // Workplace: the `default` tier fixes its root (the app root) on the first message (`started` guarantees...
        match self.rt.workplace.ensure_started(&self.rt.root) {
            Ok(Some(dir)) => ui.emit(AgentEvent::Info(crate::tf!(
                // Wording follows the new semantics (reads unrestricted, writes/deletes restricted): saying "can only read and write files"
                // would make the user think reads are locked down too, prompting a needless privilege escalation — a sentence missed when the semantics changed on 2026-10-04.
                "this conversation's workspace: {}\n  writes and deletes are restricted to this directory (reads are unrestricted — any file on the device can be read). Put scratch files in .aox\\tmp\\, new projects in a folder under workplace\\, and notes in .aox\\memory\\. To loosen it: `/workplace`, or the \"workspace\" control at the bottom-left of the input box.",
                dir.display()
            ))),
            Ok(None) => {}
            // Directory creation itself failed (permissions / disk); report it honestly but don't interrupt the conversation: from now on file writes will be rejected by the boundary.
            Err(e) => ui.emit(AgentEvent::Warn(crate::tf!("failed to create the workspace: {}", format!("{e:#}")))),
        }
        // Scratch hygiene, part 1: whatever a crashed turn left behind (`SCRATCH_STALE` — a day is long enough...
        if let Some(ws) = self.rt.workplace.active_root() {
            crate::workplace::sweep_scratch(&ws, crate::workplace::SCRATCH_STALE);
        }
        // `ensure_started` may have just decided the root (first turn), and the user may have switched it since the last...
        self.refresh_system_prompt();
        self.ensure_log(ui);
        // On this first message, record "which dir this conversation works in" into the archive (`ensure_started` just fixed the root,
        // so this record carries that dir's real path). After that we only write again when the tier switches.
        self.persist_workplace();
        // The recovery transaction starts after the workplace root is fixed and before any tool can write.
        if let Some(root) = self.rt.workplace.active_root() {
            let label = input.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            if let Err(e) = self
                .rt
                .recovery
                .begin_turn(&root, self.history_id(), label)
            {
                ui.emit(AgentEvent::Warn(crate::tf!(
                    "task recovery had an unfinished previous transaction ({}); closing it and retrying once",
                    e
                )));
                let _ = self.rt.recovery.abort_turn();
                if let Err(e) = self
                    .rt
                    .recovery
                    .begin_turn(&root, self.history_id(), label)
                {
                    ui.emit(AgentEvent::Warn(crate::tf!(
                        "task recovery is unavailable this turn: {}",
                        e
                    )));
                }
            }
        }
        self.push_message(self.user_message(input, files), None);
        // Wrap the whole turn: the UI shows a waiting animation and collapses the intermediate steps into one line at the end.
        ui.turn_start().await;
        self.begin_turn();
        // Take the `Arc` out first: `run` needs `&mut self`, so we can't borrow `loop_impl` from `self` anymore.
        let lp = self.rt.loop_impl.clone();
        let result = lp.run(self, ui).await;
        let rec = self.rt.recovery.finish_turn(if result.is_ok() {
            crate::recovery::model::TurnStatus::Completed
        } else {
            crate::recovery::model::TurnStatus::Failed
        });
        for w in rec.warnings {
            ui.emit(AgentEvent::Warn(crate::tf!("recovery: {}", w)));
        }
        if let Some(e) = rec.error {
            ui.emit(AgentEvent::Warn(crate::tf!("recovery checkpoint was not committed: {}", e)));
        }
        // Persist the checkpoint before the turn is closed out, and on both success and failure: "the task died halfway"...
        self.persist_task();
        // Scratch hygiene, part 2: the everyday sweep. `<ws>/.aox/tmp` is where the host points `TMP`/`TEMP`...
        if let Some(ws) = self.rt.workplace.active_root() {
            crate::workplace::sweep_scratch(&ws, crate::workplace::SCRATCH_GRACE);
        }
        ui.turn_end().await;
        // Report usage after the waiting animation stops (see `Ui::turn_usage`'s docs).
        // Report it even on error: the tokens were genuinely spent; we can't pretend they weren't just because the task failed.
        if let Some(rec) = self.finish_turn() {
            ui.turn_usage(&rec).await;
        }
        result
    }

    // ───────── Controlled primitives for `AgentLoop` (see `src/agent_loop.rs`'s module docs)─────────.

    /// The runtime (the tool table / plugin table / config / hook chain all hang off it).
    pub fn rt(&self) -> &Arc<Runtime> {
        &self.rt
    }

    /// Return the current LLM provider handle (Arc<dyn LlmProvider>).
    pub fn llm(&self) -> Arc<dyn LlmProvider> {
        self.llm.llm()
    }

    /// Read-only view of the current context (includes the system prompt at index 0). **Read-only** — to mutate, use `push_message`.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Open a new set of books for the turn. Called by `run_turn` before handing control to the loop.
    pub fn begin_turn(&mut self) {
        self.turn = TurnAcc::default();
    }

    /// Record one LLM call (count + each token kind). When the endpoint gives no usage, `accrue` silently skips.
    pub fn accrue_call(&mut self, usage: &Usage) {
        self.turn.calls += 1;
        self.turn.total.accrue(usage);
        if usage.prompt.is_some() {
            self.turn.last_prompt = usage.prompt;
        }
    }

    /// The input size of the most recent call in this turn, if the endpoint reported one.
    pub fn turn_last_prompt(&self) -> Option<u64> {
        self.turn.last_prompt
    }

    /// Close out the turn: fold the books into the session totals, record into `usage_log`, and persist...
    pub fn finish_turn(&mut self) -> Option<UsageRecord> {
        if self.turn.calls == 0 {
            return None;
        }
        self.session_usage.accrue(&self.turn.total);
        self.session_calls += self.turn.calls;
        let rec = UsageRecord {
            round: self.usage_log.len() as u32 + 1,
            at: crate::history::unix_now(),
            calls: self.turn.calls,
            usage: self.turn.total.clone(),
            last_prompt: self.turn.last_prompt,
            session_total: self.session_usage.total,
            session_calls: self.session_calls,
        };
        if let Some(log) = self.log.as_mut() {
            log.append_usage(&rec);
        }
        self.usage_log.push(rec.clone());
        Some(rec)
    }

    /// Close a recovery turn from outside `run_turn` (the GUI abort path).
    #[allow(dead_code)] // Only the GUI abort path calls this; the CLI goes through `run_turn`.
    pub fn finish_recovery_turn(
        &mut self,
        status: crate::recovery::model::TurnStatus,
    ) -> crate::recovery::RecoveryFinish {
        self.rt.recovery.finish_turn(status)
    }

    /// Record that the workspace was restored behind the model's back.
    pub fn note_workspace_restored(&mut self, report: &crate::recovery::model::RestoreReport) {
        self.last_output = None;
        if let Some(st) = self.task.as_mut() {
            st.invalidate_after_restore();
        }
        self.task_saved = None;
        self.persist_task();
        let note = crate::tf!(
            "{} the workspace was restored to {}. Old verification evidence and modified-file facts were invalidated. Re-read affected files and rerun acceptance before continuing. ({} path(s) restored, {} removed; chat history was not rolled back.)",
            crate::history::HOST_PREFIX,
            report.target.as_deref().unwrap_or("genesis"),
            report.restored,
            report.removed
        );
        self.push_ephemeral(Message::user(note));
    }

    // Task checkpoint (`crate::task`)

    /// Which tools this conversation is advertising (see crate::tool::scope).
    pub fn tool_scope(&self) -> &crate::tool::scope::Scope {
        &self.tool_scope
    }

    /// The current checkpoint, for a front end that wants to show "what this task is about" — and
    /// for the compaction step, which hands it to the summarizer as facts not to lose.
    pub fn task(&self) -> Option<&crate::task::TaskState> {
        self.task.as_ref()
    }

    /// The last compacted request body, if one is cached (see `crate::compact`).
    pub fn view_cache(&self) -> Option<&crate::compact::ViewCache> {
        self.view_cache.as_ref()
    }

    /// Replace the compacted view. Only `crate::compact` calls this: the cache is keyed by the message
    /// list it was built from, and a caller that sets it by hand would be asserting a key it cannot know.
    pub fn set_view_cache(&mut self, cache: Option<crate::compact::ViewCache>) {
        self.view_cache = cache;
    }

    /// Whether the "folding cannot help" warning was already shown for this conversation.
    pub fn compact_warned(&self) -> bool {
        self.compact_warned
    }

    /// Mark that warning as shown (see [`Agent::compact_warned`])
    pub fn set_compact_warned(&mut self, warned: bool) {
        self.compact_warned = warned;
    }

    /// Record a file this turn really changed, from the tool result that changed it.
    pub fn note_modified_file(&mut self, path: &std::path::Path) {
        let Some(st) = self.task.as_mut() else { return };
        st.note_modified(&path.to_string_lossy());
    }

    /// Keep the tail of the last tool result, for `verify`'s `output_contains`.
    pub fn note_tool_output(&mut self, content: &str) {
        const TAIL: usize = 8192;
        let n = content.chars().count();
        let tail: String = if n <= TAIL {
            content.to_string()
        } else {
            content.chars().skip(n - TAIL).collect()
        };
        self.last_output = Some(tail);
    }

    /// Body of the `verify` tool (intercepted in Agent::authorize_and_execute, same story as `task_state`...
    fn verify_tool(&mut self, args: Value) -> ToolOutput {
        // Read the buffer **before** borrowing the task: two disjoint fields, so no clone is needed.
        let last = self.last_output.as_deref();
        let Some(task) = self.task.as_mut() else {
            return ToolOutput::err(crate::i18n::t(
                "this conversation has no task checkpoint, so there is nothing to attach evidence to. Record one first: task_state {goal, acceptance: [...]}, then verify each acceptance item with its 1-based criterion.",
            ));
        };
        let mut check = match crate::tool::verify::run(&args, last, task.epoch) {
            Ok(c) => c,
            // "Could not run" is strictly different from "ran and failed": the model has to fix the
            // arguments (or produce some output first), not the workspace.
            Err(e) => return ToolOutput::err(crate::tf!("the check did not run: {}", e)),
        };
        // The criterion has to point at a real item before anything is recorded.
        let total = task.acceptance.len();
        if total == 0 {
            return ToolOutput::err(crate::i18n::t(
                "this checkpoint has no acceptance criteria yet, so a check cannot prove anything. Add them with task_state {acceptance: [...]} first.",
            ));
        }
        if check.criterion > total {
            return ToolOutput::err(crate::tf!(
                "criterion {} is out of range: this checkpoint has {} acceptance item(s), so the range is 1..={}. Nothing was recorded.",
                check.criterion,
                total,
                total
            ));
        }
        // The host's copy of what the check claims to prove — echoed back below and kept in the row.
        check.claim = crate::task::clamp(&task.acceptance[check.criterion - 1], crate::task::DETAIL_MAX);
        let (criterion, kind, ok) = (check.criterion, check.kind.clone(), check.ok);
        let detail = check.detail.clone();
        let target = check.target.clone();
        let claim = crate::task::clamp(&check.claim, 80);
        task.record(check);
        let covered = task.covered();

        let verdict = if ok {
            crate::i18n::t("PASS").to_string()
        } else {
            crate::i18n::t("FAIL").to_string()
        };
        let mut out = crate::tf!(
            "check recorded for acceptance #{} \"{}\": {} ({}) — {}\n  {}\n  => evidence {} of {}",
            criterion,
            claim,
            verdict,
            kind,
            target,
            detail,
            covered,
            total
        );
        out.push_str(if ok {
            crate::i18n::t(
                "\nThe host does not judge whether this check proves that criterion — compare the echoed text above with what you actually checked. If they differ, run the check that matches.\nEvidence is now attached to the checkpoint. It expires the moment a file is written, so run all checks after your last edit, then ask for status \"done\".",
            )
        } else {
            crate::i18n::t(
                "\nA failed check does not stop you, but it does block \"done\": fix the cause and run the check again — a later pass supersedes this failure.",
            )
        });
        ToolOutput::ok(out)
    }

    /// Append the checkpoint to the conversation file, only when it differs from what is already recorded there.
    pub fn persist_task(&mut self) {
        let Some(st) = &self.task else { return };
        if self.task_saved.as_ref() == Some(st) {
            return;
        }
        if let Some(log) = self.log.as_mut() {
            log.append_task(&st.to_value());
            // Marked saved only after a real write: with no open conversation file there is nowhere to
            // put it, and the next turn should try again rather than silently drop it.
            self.task_saved = Some(st.clone());
        }
    }

    /// Body of the `task_state` tool (intercepted in Agent::authorize_and_execute, the same way `ask_user` is —...
    fn task_tool(&mut self, args: Value) -> ToolOutput {
        let applied = crate::task::Patch::from_json(&args)
            .and_then(|p| crate::task::apply(self.task.clone(), &p));
        match applied {
            Ok(st) => {
                let mut out = crate::tf!(
                    "checkpoint saved ({}). This is what a restored session will see:\n{}",
                    st.task_id,
                    st.context_block()
                );
                // The next open step, named with its number: the checkpoint reply is read right before the model continues...
                let pending = st.pending_steps();
                if let Some((i, step)) = st
                    .steps
                    .iter()
                    .enumerate()
                    .find(|(_, s)| !s.done)
                {
                    let n = i + 1;
                    out.push_str(&crate::tf!(
                        "\nnext open step: #{} \"{}\" — the moment it is genuinely finished, call task_state {{\"tick\":[{}]}} before starting any tool for the step after it. Do not defer this tick to the final answer.",
                        n,
                        crate::task::clamp(&step.text, crate::task::STEP_TEXT_MAX),
                        n
                    ));
                }
                // `done` is only accepted when the gate passed (see `task::apply`), but the model's own plan may still have open...
                if st.status == crate::task::Status::Done && pending > 0 {
                    out.push_str(&crate::tf!(
                        "\nnote: the host accepted \"done\", but {} step(s) of your plan are still open — tick them if they are genuinely finished, so the next session is not misled.",
                        pending
                    ));
                }
                self.task = Some(st);
                ToolOutput::ok(out)
            }
            Err(e) => ToolOutput::err(crate::tf!("no checkpoint change was made: {}", e)),
        }
    }

    /// Push a message into the in-memory context without writing it to the conversation file.
    fn push_ephemeral(&mut self, msg: Message) {
        self.messages.push(msg);
    }

    /// Authorize → run one tool call. A controlled primitive for `AgentLoop`: it bundles the whole "ask policy, ask user, record allowlist, execute and catch timeouts" into a single entry point, so the loop needn't — and shouldn't — assemble it itself
    pub async fn authorize_and_execute(&mut self, ui: &mut dyn Ui, name: &str, args: Value) -> ToolOutput {
        let kind = self.rt.tool_kind(name).await.unwrap_or(ToolKind::Meta);
        // ── Workplace boundary: judged before authorization ──.
        let access = self.rt.tool_access(name).await;
        if let Some(reject) = self.workplace_reject(&kind, access, name, &args) {
            ui.emit(AgentEvent::Warn(reject.user_text()));
            return ToolOutput::err(reject_text(&kind, &reject));
        }
        // Capability bits (actually filled in since stage 6 · 6b): a plugin declares what capability it wants under...
        let caps = self.rt.caps_for(&kind).await;
        let level = self.rt.auth.level_for(&kind, name, &caps);
        let allowed = match level {
            AuthLevel::Auto => true,
            AuthLevel::Deny => false,
            AuthLevel::Ask => {
                if self.rt.session_allowed.read().await.contains(name) {
                    true
                } else {
                    match ui.authorize(name, &args).await {
                        AuthDecision::Allow => true,
                        AuthDecision::AllowSession => {
                            self.rt.session_allowed.write().await.insert(name.to_string());
                            true
                        }
                        AuthDecision::Deny => false,
                    }
                }
            }
        };
        if allowed {
            // `ask_user`: the only tool that needs the user to answer on the spot; branch it off here.
            if name == crate::tool::ask::ASK_TOOL {
                return crate::tool::ask::interact(args, ui).await;
            }
            // `task_state`: same interception story as `ask_user` — the checkpoint belongs to this conversation...
            if name == crate::tool::task::TASK_TOOL {
                let out = self.task_tool(args);
                // A checkpoint is the only thing a human can watch a long task by, and until 2026-10-23 nothing outside...
                if !out.is_error {
                    if let Some(st) = self.task.clone() {
                        ui.emit(AgentEvent::Task(Box::new(st)));
                    }
                }
                return out;
            }
            // `find_tools`: third interception, same story — "make a hidden tool visible" changes this conversation's scope...
            if name == crate::tool::discover::FIND_TOOL {
                let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                return crate::tool::discover::search(&self.rt, &mut self.tool_scope, query).await;
            }
            // `verify`: fourth interception, same story.
            if name == crate::tool::verify::VERIFY_TOOL {
                return self.verify_tool(args);
            }
            // The numbers the user sees have to be this call's, not the generic per-tool defaults: `shell`/`bash` compute...
            let (cap, expect) = self.rt.call_hints(name, &args).await;
            let cap = cap.unwrap_or(self.rt.config.agent.tool_timeout_secs);
            ui.emit(AgentEvent::ToolRunning {
                name: name.to_string(),
                timeout_secs: cap,
                expect_secs: expect,
            });
            // A plugin that has just been built gets its tools advertised immediately: the model is one step away...
            let built = if name == crate::tool::meta::CREATE_TOOL || name == crate::tool::meta::UPDATE_TOOL
            {
                args.get("name").and_then(|v| v.as_str()).map(str::to_owned)
            } else {
                None
            };
            // ── the recovery window for plugin writes ──.
            let guard = if plugin_window(&kind, access, self.rt.config.recovery.protect_plugins) {
                match self.rt.workplace.active_root() {
                    Some(root) => match self.rt.recovery.begin_opaque(&root, &format!("plugin {name}")) {
                        Ok(g) => g,
                        Err(block) => return ToolOutput::err(block.model),
                    },
                    // `full` mode has no protected workspace, so there is nothing to scan against.
                    None => None,
                }
            } else {
                None
            };
            let mut out = self.rt.execute(name, args).await;
            if let Some(w) = guard.map(|g| self.rt.recovery.finish_opaque(g)) {
                if !w.changed.is_empty() {
                    out.content.push_str(&crate::tf!(
                        "\nrecovery: this plugin changed {} file(s); they are recorded and can be rolled back\n",
                        w.changed.len()
                    ));
                    for p in w.changed.iter().take(20) {
                        out.content.push_str(&format!("  {p}\n"));
                    }
                    if w.changed.len() > 20 {
                        out.content.push_str(&crate::tf!("  … and {} more\n", w.changed.len() - 20));
                    }
                }
                if let Some(note) = w.note_text() {
                    out.content.push_str(&format!("\n{note}\n"));
                }
            }
            if let Some(plugin) = built.filter(|_| !out.is_error) {
                for t in self.rt.plugin_tool_names(&plugin).await {
                    self.tool_scope.enable(&t);
                }
            }
            out
        } else {
            // "Blocked by config" and "the user refused on the spot" are two different things, and the wording to the model...
            match level {
                AuthLevel::Deny => ToolOutput::err(crate::tf!(
                    "tool {} is forbidden by host policy (config.toml's [auth]). Do not try to call it again; use another tool or a different implementation path.",
                    name
                )),
                _ => ToolOutput::err(
                    crate::i18n::t("The user refused this tool call. Find another way to finish the task, or explain the reason to the user."),
                ),
            }
        }
    }

    /// Decide whether one tool call touches the workplace boundary. `None` = allowed.
    fn workplace_reject(
        &self,
        kind: &ToolKind,
        access: crate::workplace::Access,
        name: &str,
        args: &Value,
    ) -> Option<crate::workplace::Reject> {
        // The `full` tier: **allow everything**. Placed before the `match` so we don't even run
        // the coarse net of `scan_paths` — see the comment at the call site.
        if self.rt.workplace.is_unrestricted() {
            return None;
        }
        match kind {
            ToolKind::Meta => None,
            ToolKind::Host => {
                // The shell tools (including `shell_job`) carry their directory in `cwd`, not in `path` — and a missing `path`...
                if name == crate::tool::shell::SHELL_TOOL
                    || name == crate::tool::job::JOB_TOOL
                {
                    let raw = args.get("cwd").and_then(|v| v.as_str())?;
                    return self
                        .rt
                        .workplace
                        .check_access(raw, crate::workplace::Access::Write)
                        .err();
                }
                let raw = args.get(crate::workplace::PATH_ARG).and_then(|v| v.as_str())?;
                self.rt.workplace.check_access(raw, access).err()
            }
            ToolKind::Plugin(_) => {
                for (_key, raw) in crate::workplace::scan_paths(args) {
                    // Use `check_access` rather than `check`: the coarse net itself presumes neither read nor write, it's decided...
                    if let Err(r) = self.rt.workplace.check_access(&raw, access) {
                        return Some(r);
                    }
                }
                None
            }
        }
    }
}

/// The full text to show the model for one out-of-bounds event.
fn reject_text(kind: &ToolKind, reject: &crate::workplace::Reject) -> String {
    match kind {
        ToolKind::Plugin(_) => format!("{}\n\n{}", reject.model_text(), crate::i18n::t(PLUGIN_READ_HINT)),
        _ => reject.model_text(),
    }
}

/// The extra passage shown to the model when a plugin tool's path parameter hits the boundary.
const PLUGIN_READ_HINT: &str = "[If this tool is read-only, the \"do not retry\" line above does not apply]\nIf this plugin tool is only meant to **read** (scanning / counting / listing directories / parsing files), then what blocked you is not a permission, but the plugin not declaring itself read-only.\nAsk the plugin to add `\"read\"` as the 4th position of its tool declaration; after rebuilding the plugin it can access paths outside the workspace as usual. In the Python SDK you write the triple as a 4-tuple:\n  \"<tool name>\": (\"<description>\", <JSON Schema>, <handler>, \"read\")\nThe host then judges it as a **read** — the same tier as read_file / list_dir — and **no privilege escalation from the user is needed**.Conversely, tools that write files / delete things must **not** add this position.";

/// Should this call be bracketed by a workspace scan?
fn plugin_window(kind: &ToolKind, access: crate::workplace::Access, protect: bool) -> bool {
    matches!(kind, ToolKind::Plugin(_)) && access == crate::workplace::Access::Write && protect
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    /// A fake plugin tool: reports kind Plugin, `access` is supplied by the test.
    struct FakeTool {
        name: String,
        access: crate::workplace::Access,
    }

    #[async_trait::async_trait]
    impl crate::tool::Tool for FakeTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> ToolKind {
            // The plugin name is arbitrary: `workplace_reject` only looks at "is it a plugin", not which one.
            ToolKind::Plugin("fake".into())
        }
        fn description(&self) -> &str {
            "test double"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn access(&self) -> crate::workplace::Access {
            self.access
        }
        async fn call(&self, _args: Value, _rt: &Arc<Runtime>) -> ToolOutput {
            ToolOutput::ok("test double")
        }
    }

    /// An Agent with its workplace fixed at `ws`, and a registered tool named `tool` with intent `access`.
    async fn agent_with(ws: &Path, tool: &str, access: crate::workplace::Access) -> Agent {
        let mut cfg = crate::config::Config::dummy();
        cfg.workplace = crate::config::WorkplaceConfig {
            mode: "fixed".into(),
            root: Some(ws.to_string_lossy().into_owned()),
        };
        let rt =
            Runtime::new(cfg, ws.to_path_buf(), ws.join("plugins")).expect("the runtime should build");
        rt.register(Arc::new(FakeTool { name: tool.to_string(), access }))
            .await
            .expect("registering the fake tool should succeed");
        let llm: Arc<dyn LlmProvider> =
            Arc::new(crate::llm::UnavailableProvider { reason: "test".into() });
        Agent::new_with(llm, rt)
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aox-agent-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The system prompt must name the workspace the model is confined to, and be rebuilt when that workspace changes...
    #[tokio::test]
    async fn system_prompt_follows_a_workspace_switch() {
        let ws = tmp("sysprompt");
        let mut agent = agent_with(&ws, "t", crate::workplace::Access::Read).await;
        let app_root = agent.rt.root.clone();

        // Unchanged root ⇒ no rebuild (the prompt string is identical).
        let before = agent.messages[0].flat_text();
        agent.refresh_system_prompt();
        assert_eq!(agent.messages[0].flat_text(), before, "an unchanged root must not rebuild the prompt");

        // Switch to another folder ⇒ the next refresh swaps the new root into the prompt.
        let picked = std::env::temp_dir().join("aox-agent-sysprompt-picked");
        let _ = std::fs::remove_dir_all(&picked);
        std::fs::create_dir_all(&picked).unwrap();
        agent
            .rt
            .workplace
            .set_mode(crate::workplace::Mode::Fixed(picked.clone()), &app_root)
            .unwrap();
        agent.refresh_system_prompt();

        let after = agent.messages[0].flat_text();
        assert!(after.contains(&picked.display().to_string()), "the prompt must name the picked root:\n{after}");
        assert_ne!(after, before, "the prompt must actually change after the switch");

        let _ = std::fs::remove_dir_all(&picked);
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// A/B comparison: the same tool, the same out-of-workplace path, differing only by one declaration.
    #[tokio::test]
    async fn plugin_read_declaration_controls_the_workplace_gate() {
        use crate::workplace::Access;
        let ws = tmp("access");
        // A path **outside** the workplace. Need not really exist: the judgment only compares paths, doesn't touch disk.
        let outside = ws.parent().unwrap().join("aox-agent-outside").join("x.bin");
        let args = json!({ "path": outside.to_string_lossy(), "limit": 10 });
        let kind = ToolKind::Plugin("fake".into());

        // ① Declared read-only ⇒ allow (same tier as read_file / list_dir)
        let a_read = agent_with(&ws, "fake_scan", Access::Read).await;
        let access = a_read.rt().tool_access("fake_scan").await;
        assert_eq!(access, Access::Read, "`tool_access` must surface the intent declared by the tool");
        assert!(
            a_read.workplace_reject(&kind, access, "fake_scan", &args).is_none(),
            "a read-only tool accessing a path outside the workspace must be allowed — this is that \"plugin reads are opened up too\" requirement"
        );

        // ② Undeclared (= persist) ⇒ the same path is blocked
        let a_write = agent_with(&ws, "fake_scan", Access::Write).await;
        let access = a_write.rt().tool_access("fake_scan").await;
        assert_eq!(access, Access::Write, "undeclared means it writes to disk; that is the default tier");
        let rej = a_write
            .workplace_reject(&kind, access, "fake_scan", &args)
            .expect("a plugin tool that does not declare read-only must be intercepted with an outside path");
        assert!(rej.model_text().contains("permission boundary"), "{}", rej.model_text());

        // The host file-tool path wasn't affected by this change: the same out-of-workplace path is still blocked (writing tier)
        let host_write = a_write
            .workplace_reject(&ToolKind::Host, Access::Write, "write_file", &args)
            .expect("a host write tool with an outside path must be intercepted");
        assert!(host_write.model_text().contains("permission boundary"));
        // And the read tier is still allowed — consistent with the "reads unrestricted" rule set on 2026-10-04
        assert!(a_write.workplace_reject(&ToolKind::Host, Access::Read, "read_file", &args).is_none());

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// The plugin-only escape hatch in the out-of-bounds text: plugins must see it, host file tools must not.
    #[test]
    fn the_read_hint_is_attached_only_to_plugin_rejects() {
        let rej = crate::workplace::Reject::NotStarted;
        let base = rej.model_text();

        let plugin = reject_text(&ToolKind::Plugin("x".into()), &rej);
        assert!(plugin.starts_with(&base), "the generic part must come first unchanged: {plugin}");
        assert!(plugin.contains(PLUGIN_READ_HINT), "the plugin's escape hatch must be attached");
        assert!(plugin.contains("\"read\""), "it must spell out which literal to add: {plugin}");

        let host = reject_text(&ToolKind::Host, &rej);
        assert_eq!(host, base, "host tools must not receive that plugin-only escape hatch");
    }

    /// A tool call with the given id — enough for the rollback scan, which only reads ids.
    fn call(id: &str) -> crate::message::ToolCall {
        crate::message::ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: crate::message::FunctionCall { name: "fs_read".into(), arguments: "{}".into() },
        }
    }

    /// The whole point of the fix: an interrupt keeps the user's instruction and every completed step, and drops only...
    #[tokio::test]
    async fn an_interrupt_keeps_the_instruction_and_the_completed_steps() {
        let ws = std::env::temp_dir().join("aox-rollback-keep");
        let _ = std::fs::create_dir_all(&ws);
        let mut a = agent_with(&ws, "t", crate::workplace::Access::Read).await;

        let mark = a.messages_len(); // before the turn, as both front ends record it
        a.push_message(Message::user("rewrite the project"), None);
        a.push_message(Message::assistant(None, None, vec![call("c1")]), None);
        a.push_message(Message::tool_result("c1", "fs_read", "ok".into()), Some(false));
        a.push_message(Message::assistant(None, None, vec![call("c2")]), None); // ← interrupted here

        assert_eq!(a.interrupt_rollback_point(mark), 4, "only the dangling batch may go");

        a.rollback_interrupted_turn(mark);
        assert_eq!(a.messages_len(), 4);
        assert_eq!(a.messages()[1].role, "user", "the user's instruction survives");
        assert_eq!(a.messages()[3].role, "tool", "the completed step survives");
        assert!(crate::message::is_legal_body(a.messages()), "and what remains is replayable");

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Half a parallel batch is still illegal: the endpoint wants a result for every call id, so "keep the completed...
    #[tokio::test]
    async fn half_a_parallel_batch_is_dropped_whole() {
        let ws = std::env::temp_dir().join("aox-rollback-half");
        let _ = std::fs::create_dir_all(&ws);
        let mut a = agent_with(&ws, "t", crate::workplace::Access::Read).await;

        let mark = a.messages_len();
        a.push_message(Message::user("rewrite the project"), None);
        a.push_message(Message::assistant(None, None, vec![call("c1"), call("c2")]), None);
        a.push_message(Message::tool_result("c1", "fs_read", "ok".into()), Some(false)); // c2 missing

        assert_eq!(a.interrupt_rollback_point(mark), 2, "the half-answered batch goes as a unit");
        a.rollback_interrupted_turn(mark);
        assert_eq!(a.messages_len(), 2);
        assert_eq!(a.messages()[1].role, "user");
        assert!(crate::message::is_legal_body(a.messages()));

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// When nothing was mid-flight there is nothing to undo, and the conversation file must not be rewritten...
    #[tokio::test]
    async fn an_interrupt_with_nothing_in_flight_rolls_back_nothing() {
        let ws = std::env::temp_dir().join("aox-rollback-noop");
        let _ = std::fs::create_dir_all(&ws);

        let mut a = agent_with(&ws, "t", crate::workplace::Access::Read).await;
        let mark = a.messages_len();
        a.push_message(Message::user("rewrite the project"), None);
        a.push_message(Message::assistant(None, None, vec![call("c1")]), None);
        a.push_message(Message::tool_result("c1", "fs_read", "ok".into()), Some(false));
        let before = a.messages_len();
        assert_eq!(a.interrupt_rollback_point(mark), before);
        a.rollback_interrupted_turn(mark);
        assert_eq!(a.messages_len(), before, "a finished batch is left alone");

        let mut b = agent_with(&ws, "t", crate::workplace::Access::Read).await;
        let mark = b.messages_len();
        b.push_message(Message::user("rewrite the project"), None);
        let before = b.messages_len();
        assert_eq!(b.interrupt_rollback_point(mark), before, "no tool call → nothing to drop");
        b.rollback_interrupted_turn(mark);
        assert_eq!(b.messages_len(), before, "the instruction is not thrown away");

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// The floor: `mark` is never crossed.
    #[tokio::test]
    async fn the_rollback_never_reaches_past_the_turn_start() {
        let ws = std::env::temp_dir().join("aox-rollback-floor");
        let _ = std::fs::create_dir_all(&ws);
        let mut a = agent_with(&ws, "t", crate::workplace::Access::Read).await;

        a.push_message(Message::user("earlier turn"), None);
        a.push_message(Message::assistant(None, None, vec![call("c1")]), None); // dangling, before mark
        let mark = a.messages_len();

        assert_eq!(a.interrupt_rollback_point(mark), mark, "the floor holds");
        a.rollback_interrupted_turn(mark);
        assert_eq!(a.messages_len(), mark);

        let _ = std::fs::remove_dir_all(&ws);
    }

    // Task checkpoint (`crate::task`, `task_state`)

    /// The whole point of a checkpoint is surviving a session: write it, drop the agent, build a new...
    #[tokio::test]
    async fn a_checkpoint_travels_with_the_conversation() {
        let ws = std::env::temp_dir().join("aox-task-checkpoint");
        let _ = std::fs::remove_dir_all(&ws);
        let mut a = agent_with(&ws, "t", crate::workplace::Access::Read).await;

        // A conversation must exist before a checkpoint has anywhere to live.
        let id = {
            let log = a.rt.store.create(None, &Default::default()).unwrap();
            log.id().to_string()
        };
        a.resume(&id).unwrap();
        assert!(a.task().is_none(), "a conversation written before the feature has no checkpoint");

        // The first update needs a goal; evidence is **not** accepted from the model.
        let bad = a.task_tool(json!({"modified_files": ["fake.rs"]}));
        assert!(bad.is_error, "the model must not be able to declare its own evidence");
        assert!(a.task().is_none(), "a rejected patch changes nothing");

        let out = a.task_tool(json!({
            "goal": "add task checkpoints",
            "constraints": ["keep the jsonl format"],
            "next_steps": ["write the tool"],
        }));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(a.task().unwrap().goal, "add task checkpoints");

        // Host-side evidence: comes from a tool result, and is a no-op when no checkpoint exists.
        a.note_modified_file(std::path::Path::new(r"E:\rustapp\src\task.rs"));
        a.persist_task();
        let saved = a.rt.store.load_task(&id).expect("the checkpoint must be on disk");
        assert_eq!(
            crate::task::TaskState::from_value(&saved).unwrap().modified_files,
            vec![r"E:\rustapp\src\task.rs".to_string()]
        );

        // Rewriting the same state again must not add a second row.
        let rows = |ws: &std::path::Path| {
            let mut n = 0;
            if let Ok(rd) = std::fs::read_dir(ws.join(".aox").join("conversations")) {
                for e in rd.flatten() {
                    if let Ok(s) = std::fs::read_to_string(e.path()) {
                        n += s.lines().filter(|l| l.contains("\"kind\":\"task\"")).count();
                    }
                }
            }
            n
        };
        let before = rows(&ws);
        a.persist_task();
        assert_eq!(rows(&ws), before, "an unchanged checkpoint must not be written again");

        // New process, same conversation: the state comes back, and the restore note is memory-only.
        let msgs_before = a.rt.store.load(&id).unwrap().len();
        drop(a);
        let mut b = agent_with(&ws, "t", crate::workplace::Access::Read).await;
        b.resume(&id).unwrap();
        let st = b.task().expect("the checkpoint must be restored");
        assert_eq!(st.goal, "add task checkpoints");
        assert!(b.messages().last().unwrap().flat_text().contains("add task checkpoints"), "the restore note must reach the model");
        assert_eq!(
            b.rt.store.load(&id).unwrap().len(),
            msgs_before,
            "the restore note is regenerated on every resume, so it must not be written into the file"
        );

        // A reset starts a different task and drops the old progress — but keeps the evidence rules.
        let out = b.task_tool(json!({"reset": true, "goal": "something else"}));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(b.task().unwrap().goal, "something else");
        assert!(!b.task().unwrap().task_id.is_empty());

        b.new_conversation();
        assert!(b.task().is_none(), "a new conversation must not inherit the previous task");
        assert!(b.task_saved.is_none());

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// The checkpoint reply must name the exact next tick while the plan is open.
    #[tokio::test]
    async fn every_checkpoint_reply_points_at_the_next_tick() {
        let ws = std::env::temp_dir().join("aox-task-next-tick");
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::create_dir_all(&ws);
        let mut a = agent_with(&ws, "t", crate::workplace::Access::Read).await;

        let out = a.task_tool(json!({
            "goal": "ship the plan",
            "steps": ["read it", "write it", "test it"],
        }));
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("next open step: #1"), "{}", out.content);
        assert!(out.content.contains("\"read it\""), "{}", out.content);
        assert!(out.content.contains("{\"tick\":[1]}"), "{}", out.content);

        // One tick moves the instruction to the next open step — never back to #1.
        let out = a.task_tool(json!({"tick": 1}));
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("next open step: #2"), "{}", out.content);
        assert!(out.content.contains("{\"tick\":[2]}"), "{}", out.content);

        // A finished plan has nothing left to nag about.
        let out = a.task_tool(json!({"tick": [2, 3]}));
        assert!(!out.is_error, "{}", out.content);
        assert!(!out.content.contains("next open step"), "{}", out.content);

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// The closing loop of the stage: `done` is a decision the host makes from evidence, not a value the model writes.
    #[tokio::test]
    async fn done_is_a_decision_made_from_evidence() {
        let ws = std::env::temp_dir().join("aox-verify-gate");
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::create_dir_all(&ws);
        let mut a = agent_with(&ws, "t", crate::workplace::Access::Read).await;
        let proof = ws.join("proof.txt");

        a.task_tool(json!({
            "goal": "prove the gate works",
            "acceptance": ["proof.txt exists"],
            "completed": ["wrote the checkpoint"],
        }));

        // ① Nothing has been checked yet — the reply must say how to fix it, not just "no".
        let refused = a.task_tool(json!({"status": "done"}));
        assert!(refused.is_error, "done without evidence must be refused");
        assert!(refused.content.contains("verify"), "the refusal must name the fix: {}", refused.content);
        assert_eq!(a.task().unwrap().status, crate::task::Status::Open);

        // ② A real check against a real file.
        std::fs::write(&proof, "ok\n").unwrap();
        let out = a.verify_tool(json!({
            "check": "path_exists",
            "criterion": 1,
            "path": proof.to_string_lossy(),
        }));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(a.task().unwrap().covered(), 1, "{}", out.content);

        let accepted = a.task_tool(json!({"status": "done"}));
        assert!(!accepted.is_error, "{}", accepted.content);
        assert_eq!(a.task().unwrap().status, crate::task::Status::Done);

        // ③ The workspace moves on → the old evidence is no longer proof
        a.note_modified_file(&proof);
        assert_eq!(a.task().unwrap().status, crate::task::Status::Open, "a write reopens the task");
        let refused = a.task_tool(json!({"status": "done"}));
        assert!(refused.is_error, "stale evidence must not pass");
        assert!(refused.content.contains("stale"), "{}", refused.content);

        let out = a.verify_tool(json!({
            "check": "file_contains",
            "criterion": 1,
            "path": proof.to_string_lossy(),
            "text": "ok",
        }));
        assert!(!out.is_error && a.task().unwrap().gate().is_ok(), "{}", out.content);
        // The criterion text comes back with the evidence: this is what lets the model notice that it
        // checked something other than what the criterion says.
        assert!(
            out.content.contains("proof.txt exists"),
            "the reply must echo the criterion it claims to prove: {}",
            out.content
        );
        assert_eq!(a.task().unwrap().checks.last().unwrap().claim, "proof.txt exists");

        // No checkpoint at all: `verify` says so instead of pretending a record exists.
        a.new_conversation();
        let out = a.verify_tool(json!({"check": "path_exists", "criterion": 1, "path": proof.to_string_lossy()}));
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("task_state"), "{}", out.content);

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// An off-by-one on `criterion` must be caught **before** recording: evidence that can never
    /// justify `done` is worse than no evidence, because the model would believe it had proof.
    #[tokio::test]
    async fn a_criterion_out_of_range_records_nothing() {
        let ws = std::env::temp_dir().join("aox-verify-range");
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::create_dir_all(&ws);
        let proof = ws.join("p.txt");
        std::fs::write(&proof, "x").unwrap();
        let mut a = agent_with(&ws, "t", crate::workplace::Access::Read).await;

        a.task_tool(json!({"goal": "g", "acceptance": ["one"], "completed": ["c"]}));
        let out = a.verify_tool(json!({"check": "path_exists", "criterion": 2, "path": proof.to_string_lossy()}));
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("1..=1"), "{}", out.content);
        assert!(a.task().unwrap().checks.is_empty(), "a rejected criterion must record nothing");

        // No acceptance at all: the same refusal, with the fix named
        a.new_conversation();
        a.task_tool(json!({"goal": "g", "completed": ["c"]}));
        let out = a.verify_tool(json!({"check": "path_exists", "criterion": 1, "path": proof.to_string_lossy()}));
        assert!(out.is_error && out.content.contains("acceptance"), "{}", out.content);

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// `output_contains` is the command-level degraded path: the host can't run the tests, but it can insist the text...
    #[tokio::test]
    async fn output_evidence_only_comes_from_what_a_tool_said() {
        let ws = std::env::temp_dir().join("aox-verify-output");
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::create_dir_all(&ws);
        let mut a = agent_with(&ws, "t", crate::workplace::Access::Read).await;

        a.task_tool(json!({"goal": "g", "acceptance": ["tests pass"], "completed": ["ran them"]}));
        let out = a.verify_tool(json!({"check": "output_contains", "criterion": 1, "text": "ok"}));
        assert!(out.is_error, "with no tool output there is nothing to check: {}", out.content);

        a.note_tool_output("... test result: ok. 299 passed; 0 failed ...");
        let out = a.verify_tool(json!({"check": "output_contains", "criterion": 1, "text": "299 passed"}));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(a.task().unwrap().covered(), 1);

        // A different conversation must not inherit the buffer ("the last output" is per conversation).
        a.new_conversation();
        a.task_tool(json!({"goal": "g", "acceptance": ["tests pass"], "completed": ["ran them"]}));
        let out = a.verify_tool(json!({"check": "output_contains", "criterion": 1, "text": "299 passed"}));
        assert!(out.is_error, "the buffer belongs to the conversation that produced it: {}", out.content);

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// A plugin tool that writes a file, so the agent-level window can be observed end to end.
    struct WritingPlugin {
        name: String,
        path: std::path::PathBuf,
    }

    #[async_trait::async_trait]
    impl crate::tool::Tool for WritingPlugin {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> ToolKind {
            ToolKind::Plugin("fake".into())
        }
        fn description(&self) -> &str {
            "writes one file and claims to be a plugin"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn access(&self) -> crate::workplace::Access {
            crate::workplace::Access::Write
        }
        async fn call(&self, _args: Value, _rt: &Arc<Runtime>) -> ToolOutput {
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&self.path, "written by a plugin") {
                Ok(()) => ToolOutput::ok("wrote it"),
                Err(e) => ToolOutput::err(e.to_string()),
            }
        }
    }

    struct RecordingUi;

    #[async_trait::async_trait]
    impl crate::ui::Ui for RecordingUi {
        async fn read_user_input(&mut self) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn emit(&mut self, _event: crate::ui::AgentEvent) {}
        async fn authorize(&mut self, _tool: &str, _args: &Value) -> crate::ui::AuthDecision {
            crate::ui::AuthDecision::Allow
        }
    }

    /// The whole point of W6: a plugin that declares `write` is bracketed by a scan, so what it wrote
    /// is in the recovery point instead of being an invisible hole.
    #[tokio::test]
    async fn a_plugin_that_declares_write_lands_in_the_recovery_point() {
        let ws = std::env::temp_dir().join("aox-plugin-window");
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("keep.txt"), "keep").unwrap();

        let mut cfg = crate::config::Config::dummy();
        cfg.workplace = crate::config::WorkplaceConfig {
            mode: "fixed".into(),
            root: Some(ws.to_string_lossy().into_owned()),
        };
        let rt = Runtime::new(cfg, ws.clone(), ws.join("plugins")).expect("runtime");
        rt.workplace.ensure_started(&ws).expect("workspace");
        rt.register(Arc::new(WritingPlugin {
            name: "fs_write".into(),
            path: ws.join("nested/made.txt"),
        }))
        .await
        .expect("registered");

        let llm: Arc<dyn LlmProvider> =
            Arc::new(crate::llm::UnavailableProvider { reason: "test".into() });
        let mut agent = Agent::new_with(llm, rt);
        agent.rt.recovery.begin_turn(&ws, None, "plugin").expect("a turn");

        let mut ui = RecordingUi;
        let out = agent.authorize_and_execute(&mut ui, "fs_write", json!({})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("recovery: this plugin changed 1 file"), "{}", out.content);

        let id = agent.rt.recovery.finish_turn(crate::recovery::model::TurnStatus::Completed).point;
        let store = agent.rt.recovery.open_store(&ws, false).unwrap();
        let node = store.load_node(&id.expect("a recovery point")).unwrap();
        let op = node.ops.iter().find(|o| o.path == "nested/made.txt").expect("recorded");
        assert!(op.before.is_none(), "the file did not exist before the plugin ran");
        assert!(op.after.as_ref().unwrap().stored);

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// The plugin window is decided by three things and nothing else.
    #[test]
    fn a_plugin_window_is_opened_only_for_a_declared_writer() {
        use crate::workplace::Access;
        let plugin = ToolKind::Plugin("fs".into());

        // Declared write, protection on: scan it
        assert!(plugin_window(&plugin, Access::Write, true));
        // Declared read is taken at its word — the declaration is the plugin's own promise.
        assert!(!plugin_window(&plugin, Access::Read, true));
        // `protect_plugins = false` is the documented opt-out
        assert!(!plugin_window(&plugin, Access::Write, false));
        // Host tools are bracketed by their own capture, not by a scan.
        assert!(!plugin_window(&ToolKind::Host, Access::Write, true));
        assert!(!plugin_window(&ToolKind::Meta, Access::Write, true));
    }
}
