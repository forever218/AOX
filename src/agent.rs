use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::attach;
use crate::auth::AuthLevel;
use crate::history::UsageRecord;
use crate::llm::{LlmProvider, Usage};
use crate::message::Message;
use crate::runtime::Runtime;
use crate::tool::{ToolKind, ToolOutput};
use crate::ui::{AgentEvent, AuthDecision, Ui};

/// Per-turn usage accumulator.
///
/// It is a field of `Agent` rather than a local variable of some function, for two reasons:
///
/// ① **Tokens already spent cannot be recovered**: `AgentLoop::run` may `bail!` mid-way
///    (e.g. the model returns empty replies repeatedly), yet the tally stays with `Agent`,
///    so `run_turn` can still report it honestly on error instead of losing the books over one `Err`.
/// ② The loop has been moved into `AgentLoop` (stage 6, 6a), so "who runs the loop" and
///    "whose books the tally lands on" must be decoupled — the loop records an entry via
///    [`Agent::accrue_call`], and [`Agent::finish_turn`] closes the books.
#[derive(Default)]
struct TurnAcc {
    calls: u32,
    total: Usage,
    /// Size of the last request's input = current context footprint.
    last_prompt: Option<u64>,
}

pub struct Agent {
    llm: Arc<dyn LlmProvider>,
    rt: Arc<Runtime>,
    messages: Vec<Message>,
    /// The persister for this conversation (the **write end** of the storage seam).
    ///
    /// Created lazily: a file is only made once a real user message exists, so a session like
    /// "launch → /help → quit" never leaves a pile of empty files in history/.
    ///
    /// Which file it lands in and what it is named is none of this type's business — that is an
    /// implementation detail of `self.rt.store` (see `src/store.rs`). This type only talks to it by **id**.
    log: Option<Box<dyn crate::store::Session>>,
    /// Persist errors are reported only once, so we don't repeat the same warning every turn.
    log_error_reported: bool,
    /// Cumulative usage for this conversation so far (reloaded from file or reset on switch/new conversation).
    session_usage: Usage,
    session_calls: u32,
    /// Per-turn usage records for this conversation, one line per `kind:"usage"` row in the conversation file.
    ///
    /// We keep a copy in memory so the UI can instantly answer "how much this whole conversation cost, which turn was the priciest",
    /// without re-reading disk every frame; the file is the source of truth — after a restart we pick back up from there.
    usage_log: Vec<UsageRecord>,
    /// The workplace record this conversation has **already persisted** (tier + root + this conversation's own dir); `None` = not written yet.
    ///
    /// This is what lets us "append a line only when something actually changed": blindly appending every turn would pile duplicate rows into the file,
    /// yet readers only take the last one — such a file gives no clue which lines actually count.
    /// Reset on new conversation / opening an old one, to match the file's state at that moment.
    workplace_logged: Option<crate::workplace::Record>,
    /// This turn's usage accumulator (driven by `AgentLoop` via [`Agent::accrue_call`]).
    ///
    /// Lives on `Agent`, not as a loop-local variable: the loop is swappable, the books are not —
    /// a different implementation must still record onto the same books (see [`TurnAcc`]'s docs).
    turn: TurnAcc,
}

impl Agent {
    /// Assemble an Agent.
    ///
    /// The system prompt is composed from "segments" by [`crate::prompt::build`]: built-in segments, the runtime environment,
    /// the project rules file, and whatever the user added/removed/edited under `config.toml [prompt]` — all synthesized there.
    /// This type **no longer reads `RULES.md`** — that "fetch the rules file" logic used to be in the CLI (`src/main.rs`)
    /// and the GUI (`gui/src/bridge.rs`) separately; now it lives in one place, `prompt`.
    pub fn new(llm: Arc<dyn LlmProvider>, rt: Arc<Runtime>) -> Self {
        let sys = crate::prompt::build(&rt);
        Self {
            llm,
            rt,
            messages: vec![Message::system(sys)],
            log: None,
            log_error_reported: false,
            session_usage: Usage::default(),
            session_calls: 0,
            usage_log: Vec::new(),
            workplace_logged: None,
            turn: TurnAcc::default(),
        }
    }

    /// This conversation's history id (None before a conversation has started).
    /// The id is fixed by the storage backend when created (for the jsonl backend it's the filename stem); this just relays it.
    pub fn history_id(&self) -> Option<&str> {
        self.log.as_ref().map(|l| l.id())
    }

    /// Persist the current workplace state into **this conversation's archive** (only writing when it actually changed).
    ///
    /// Called from three sites: once at the start of every turn (the first message records the just-created dir), after a CLI `/workplace`
    /// tier switch, and after a GUI workplace-button tier switch.
    ///
    /// **Returns immediately when there is no conversation file** — in that case we wait for `ensure_log` to create the file, and the next turn's
    /// step records it; nothing is lost.
    pub fn persist_workplace(&mut self) {
        let Some(log) = self.log.as_mut() else { return };
        let cur = self.rt.workplace.persisted();
        if self.workplace_logged.as_ref() == Some(&cur) {
            return;
        }
        log.append_workplace(&cur);
        self.workplace_logged = Some(cur);
    }

    /// Ensure the current conversation already has an on-disk file. On failure we warn only once and don't interrupt the conversation.
    fn ensure_log(&mut self, ui: &mut dyn Ui) {
        if self.log.is_some() {
            return;
        }
        let cfg = &self.rt.config.history;
        if !cfg.enabled {
            return;
        }
        match self.rt.store.create(Some(&self.rt.config.provider.model)) {
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
    ///
    /// Attachments take shape here, and **only here**: reading disk, base64, assembling parts are all delegated to
    /// [`Message::user_with_files`]. Both GUI and CLI only hand over paths; neither assembles them itself —
    /// otherwise the "in memory but not in the history file" divergence would reappear from the other entry point.
    ///
    /// When an attachment misbehaves (unreadable / oversized / wrong dir) we **neither swallow nor interrupt** it: what can attach does, and what can't
    /// has its reason appended to the end of the body. This way "an attachment went missing" is **visible in the conversation** —
    /// a mere UI flash would leave both the model and a user scrolling back through history unaware that something was dropped.
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
    ///
    /// **All** writes to `messages` must go through here — if the on-disk copy and memory ever
    /// diverge, the history file is no longer a faithful record of the conversation. This
    /// constraint applies to `AgentLoop` too: it is `pub`, yet `messages` is only exposed as a
    /// read-only view (see the controlled-primitives section below).
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

    /// Per-turn usage records for the current conversation (so the UI can show "how much each turn cost").
    pub fn usage_log(&self) -> &[UsageRecord] {
        &self.usage_log
    }

    /// Append a batch of **activity-stream** records (produced by the UI side, written into the same conversation file).
    ///
    /// Why this writes rather than the UI opening a file itself: the file handle is held only by
    /// this thread, and "new conversation" / "interrupt rollback" also happen here — a second
    /// writer would mean another set of cross-thread mutex and ordering guarantees, which we
    /// already get for free from a single in-thread queue.
    ///
    /// Discarded outright when `log` is `None`. Normally this won't happen: the activity stream is
    /// only produced during a "task turn", and that turn calls `ensure_log` up front. The only
    /// exception is when history is disabled in config (`[history] enabled = false`) — which
    /// shouldn't be persisted anyway.
    ///
    /// `allow(dead_code)`: `src/main.rs` and `src/lib.rs` each compile the same set of modules
    /// (see the top of lib.rs), but the CLI build has no activity stream at all, so in its eyes
    /// this method is dead code. The GUI build uses it.
    #[allow(dead_code)]
    pub fn append_acts(&mut self, acts: &[Value]) {
        let Some(log) = self.log.as_mut() else { return };
        for a in acts {
            log.append_act(a);
        }
    }

    /// Switch to an archived conversation (by **id**): load its messages back into context and reconnect the persister to that conversation.
    ///
    /// Returns the number of messages loaded (excluding the system prompt). On load failure the context is **left as-is** —
    /// clearing it halfway then erroring would make the user lose the conversation in front of them and still not get the one they wanted.
    ///
    /// The full history is loaded. This makes "continue where we left off" a real continuation, at the cost of a larger context
    /// the longer the conversation; to start a new topic press "+", which is a different path.
    pub fn resume(&mut self, id: &str) -> Result<usize> {
        let items = self.rt.store.load(id)?;
        let log = self.rt.store.open(id)?;
        // Keep only the system prompt (entry 0); replace everything else with the archived messages.
        // Deliberately leaving the system prompt out of the archive (it changes every turn), so only here can it be preserved.
        self.messages.truncate(1);
        self.messages.extend(items.iter().map(|i| i.msg.clone()));
        self.log = Some(log);
        self.log_error_reported = false;
        // Pick the usage ledger back up from the storage layer. Without this, a restart or a conversation switch would
        // make the previous turns' tokens vanish into thin air — yet "how much this conversation cost in total"
        // is precisely the number that most deserves to survive across sessions.
        self.usage_log = self.rt.store.load_usage(id);
        self.session_usage = Usage::default();
        self.session_calls = 0;
        for r in &self.usage_log {
            self.session_usage.accrue(&r.usage);
            self.session_calls += r.calls;
        }
        // The workplace follows the **conversation**, not the process: restore the record from this
        // conversation's archive and align "already persisted" to it. So reopening this conversation
        // lands in **its own** dir, not "the current one" — the latter was the old behavior, which
        // made the same conversation land in a different workplace every time you entered it.
        //
        // Old conversation files lack this record (the feature didn't exist yet): leave everything
        // as-is; the next time it persists, `persist_workplace` records the current state, and from
        // then on this conversation has its own dir.
        self.workplace_logged = match self.rt.store.load_workplace(id) {
            Some(rec) => {
                self.rt.workplace.restore(&rec);
                Some(rec)
            }
            None => None,
        };
        Ok(items.len())
    }

    /// Start a new conversation.
    ///
    /// The old conversation needs **no "save" action** at all: it was appended as you talked, so it already sits complete
    /// in `history/`. Here we only null out the persister — the next turn lazily creates a new file,
    /// and so the previous conversation naturally falls into history.
    pub fn new_conversation(&mut self) {
        self.messages.truncate(1);
        self.log = None;
        self.log_error_reported = false;
        self.session_usage = Usage::default();
        self.session_calls = 0;
        self.usage_log.clear();
        // The workplace turns the page too: **always back to the "default" tier** (semantics set by the user on 2026-10-04) —
        // the tier temporarily switched to in the last conversation is not inherited; the next turn's first message opens a fresh dir under `workplace/`.
        // Without clearing it, after "+" the model would still write into the dir picked in the last conversation, which is no longer a new conversation.
        self.rt.workplace.reset_for_new_conversation();
        // The new conversation's file isn't created yet; the persisted record is filled in by `persist_workplace` on the first message;
        // here we clear "already persisted" first, so we don't compare against the last conversation's record and miss writing that first message.
        self.workplace_logged = None;
    }

    /// After interrupting the current task, roll back the conversation history to avoid leaving half messages "with tool_calls but no result".
    ///
    /// The on-disk file rolls back in sync: whatever is dropped from memory must also be dropped from the file,
    /// otherwise we'd leave a broken record that can't be replayed as a request body.
    pub fn truncate_messages(&mut self, len: usize) {
        self.messages.truncate(len);
        if let Some(log) = self.log.as_mut() {
            log.rewrite(&self.messages);
        }
    }

    /// Drop the oldest chunk of the body: keep the system prompt, plus `messages[cut..]` (everything from `cut` onward).
    ///
    /// ⚠️ **Goes the opposite direction from `truncate_messages`**, hence the deliberately distinct name:
    /// `Vec::truncate(n)` means "**keep the first** n" — which is exactly what interrupt-rollback wants (back to before the task);
    /// but `[loop] max_rounds` wants "**keep the last** few turns". Mixing the two silently truncates the wrong way —
    /// manifesting as "this turn's just-said words are gone, the oldest context remains", and memory and file are "consistently wrong",
    /// so looking at the persisted content alone won't easily reveal it. This comment exists precisely for that.
    ///
    /// The persisted file is symmetric too: whatever is dropped from memory is dropped from the file (via `Session::rewrite`).
    pub fn drop_oldest(&mut self, cut: usize) {
        if cut <= 1 || cut >= self.messages.len() {
            return;
        }
        self.messages.drain(1..cut);
        if let Some(log) = self.log.as_mut() {
            log.rewrite(&self.messages);
        }
    }

    /// Handle one round of user input: loop "ask the model → run the tool → fill back the result" until the model stops calling tools.
    ///
    /// **The loop itself is not here** — it's provided by `Runtime.loop_impl` (the loop seam, see `src/agent_loop.rs`)
    /// this method only does three things: "open a turn, open the books, close the books". Swapping in a different `[loop] impl`
    /// means a different execution model, yet accounting and wrap-up still go through the same path.
    ///
    /// `files` are the attachments on this one message (local paths). They are attached **to this message only** (one-shot):
    /// in later turns the model refers back to them via the paths recorded in the message and reads them itself — the attachment bytes stay in context,
    /// so every turn pays token cost for them again.
    pub async fn run_turn(&mut self, ui: &mut dyn Ui, input: &str, files: &[PathBuf]) -> Result<()> {
        // Workplace: the `default` tier opens a fresh dir on the **first message** (`started` guarantees one dir per conversation).
        //
        // Placed at the very front of the turn (before `push_message` and the loop) so the root is
        // fixed **before any tool call this turn**. Otherwise, when the model wants to write a file in
        // the first message it would be rejected as "workplace not yet established", which would baffle
        // the user — a fake failure caused by implementation order, not by a permission boundary.
        match self.rt.workplace.ensure_started(&self.rt.root) {
            Ok(Some(dir)) => ui.emit(AgentEvent::Info(crate::tf!(
                // Wording follows the new semantics (reads unrestricted, writes/deletes restricted): saying "can only read and write files"
                // would make the user think reads are locked down too, prompting a needless privilege escalation — a sentence missed when the semantics changed on 2026-10-04.
                "this conversation's workspace: {}\n  writes and deletes are restricted to this directory (reads are unrestricted — any file on the device can be read). To loosen it: `/workplace`, or the \"workspace\" control at the bottom-left of the input box.",
                dir.display()
            ))),
            Ok(None) => {}
            // Directory creation itself failed (permissions / disk); report it honestly but don't interrupt the conversation: from now on file writes will be rejected by the boundary.
            Err(e) => ui.emit(AgentEvent::Warn(crate::tf!("failed to create the workspace: {}", format!("{e:#}")))),
        }
        self.ensure_log(ui);
        // On this first message, record "which dir this conversation works in" into the archive (`ensure_started` just fixed the root,
        // so this record carries that dir's real path). After that we only write again when the tier switches.
        self.persist_workplace();
        self.push_message(self.user_message(input, files), None);
        // Wrap the whole turn: the UI shows a waiting animation and collapses the intermediate steps into one line at the end.
        ui.turn_start().await;
        self.begin_turn();
        // Take the `Arc` out first: `run` needs `&mut self`, so we can't borrow `loop_impl` from `self` anymore.
        let lp = self.rt.loop_impl.clone();
        let result = lp.run(self, ui).await;
        ui.turn_end().await;
        // Report usage after the waiting animation stops (see `Ui::turn_usage`'s docs).
        // Report it even on error: the tokens were genuinely spent; we can't pretend they weren't just because the task failed.
        if let Some(rec) = self.finish_turn() {
            ui.turn_usage(&rec).await;
        }
        result
    }

    // ───────── Controlled primitives for `AgentLoop` (see `src/agent_loop.rs`'s module docs)─────────
    //
    // After the loop moved into `AgentLoop` it became a **peer collaborator** of `Agent`, so the following methods must be visible.
    // But the write entry is still only `push_message`: the loop can't get the bare `Vec<Message>`,
    // so it can't create the "in memory but not in the history file" divergence — that line wasn't loosened by extracting the seam.

    /// The runtime (the tool table / plugin table / config / hook chain all hang off it).
    pub fn rt(&self) -> &Arc<Runtime> {
        &self.rt
    }

    /// LLM Provider。
    pub fn llm(&self) -> &Arc<dyn LlmProvider> {
        &self.llm
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

    /// Close out the turn: fold the books into the session totals, record into `usage_log`, and persist into the conversation file.
    ///
    /// Returns `None` when not a single call happened (the loop failed at the start) — an empty ledger isn't worth a line.
    ///
    /// What we persist and what we hand to the UI is **the same object** — so the ledger the UI shows and the one stored in the file can never
    /// disagree (a pit like "one number on the UI, another in the file" isn't worth stepping in twice).
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

    /// Authorize → run one tool call. A controlled primitive for `AgentLoop`: it bundles the whole "ask policy, ask user, record allowlist, execute and catch timeouts" into a single entry point, so the loop needn't — and shouldn't — assemble it itself.
    pub async fn authorize_and_execute(&mut self, ui: &mut dyn Ui, name: &str, args: Value) -> ToolOutput {
        let kind = self.rt.tool_kind(name).await.unwrap_or(ToolKind::Meta);
        // ── Workplace boundary: judged **before authorization** ──
        //
        // Two reasons for placing it before authorization:
        // ① An out-of-bounds call needn't pop an auth dialog first, then tell the user "that location is actually unreachable";
        // ② Here we have `Ui` and can emit a `Warn` to the user (the tool's own `call` can't get Ui,
        //    so the tool's inner layer can only stuff the contract back to the model — two layers are defense in depth, not duplication).
        //
        // Whether this call counts as "read" or "persist" is **said by the tool itself** (`Tool::access`: host file tools judge by
        // name, plugin tools judge by the access the plugin declared; undeclared means persist).
        //
        // The `full` tier is **short-circuited entirely** inside `workplace_reject`: no param scan, no `canonicalize`.
        // Earlier this called `check` anyway ("it just returns `Ok`") — but the caller only takes `.err()`,
        // so the `normalize` result was thrown away, wasting 65~224 µs of syscalls; and this path is taken on
        // **every tool call**.
        // ⚠️ Since 2026-10-05 the config default is the restricted tier (`default`), so this short-circuit only applies to users
        // who **explicitly** chose full access — don't justify the short-circuit with "most users go through here" anymore.
        let access = self.rt.tool_access(name).await;
        if let Some(reject) = self.workplace_reject(&kind, access, &args) {
            ui.emit(AgentEvent::Warn(reject.user_text()));
            return ToolOutput::err(reject_text(&kind, &reject));
        }
        // Capability bits (actually filled in since stage 6 · 6b): a plugin declares what capability it wants under `[contributes] caps`,
        // relayed here verbatim to policy — a plugin that declared a capability has all its tool calls go through `Ask`
        // (see `auth::Policy::level_for`). Meta tools, and plugins that declared no capability, get an empty slice,
        // the judgment matches stage 4 line by line.
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
            // `ask_user`: the only tool that **needs the user to answer on the spot**; branch it off here.
            //
            // Three reasons, each hard:
            // ① `Tool::call(args, rt)` can't get `Ui`, and "ask the user" is impossible without it — the
            //    authorization path therefore also goes through Agent (same comment in `tool::ask`'s module docs).
            // ② Placed **after** authorization: first "may I use this tool", then "which one" — the two dialogs never appear at once.
            // ③ Placed **before** `ToolRunning`: that event carries a "timeout cap Ns" note, but this tool waits
            //    for a **human** — taking three minutes is perfectly normal, and showing a countdown would just rush them into misclicks.
            if name == crate::tool::ask::ASK_TOOL {
                return crate::tool::ask::interact(args, ui).await;
            }
            ui.emit(AgentEvent::ToolRunning {
                name: name.to_string(),
                timeout_secs: self.rt.config.agent.tool_timeout_secs,
            });
            self.rt.execute(name, args).await
        } else {
            // "Blocked by config" and "the user refused on the spot" are two different things, and the wording to the model must differ:
            // the former is a host-hardcoded rule (`[auth]`'s deny, which since stage 4 supports `fs_*` wildcards),
            // the latter is a human. They used to share one sentence, so the model would try to "convince the user" — wrong direction from the start.
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
    ///
    /// Three paths by tool kind, of differing precision — all deliberate:
    ///
    /// - `ToolKind::Meta` (`create_plugin` / `list_plugins` / `remove_plugin`): declares no path parameter, so allow outright.
    ///
    /// - `ToolKind::Host` (host file tools): the schema is our own, so read the `path` parameter directly.
    ///   Reads (`read_file` / `list_dir` / `attach_file`) are not subject to the workplace restriction ⇒ this path is
    ///   always allow; only writes (`write_file`) and deletes (`remove`) actually pass the boundary. `list_dir` may be
    ///   called **without** `path` (then it means the workplace root, position supplied by the host), so a missing arg
    ///   is not an error — what's not allowed is "a location the model specified".
    ///
    /// - `ToolKind::Plugin`: the plugin is a black box; the host can only scan for path-like strings by parameter
    ///   name and value shape (see `workplace::scan_paths`). These parameters are judged by the **plugin-declared**
    ///   access ([`crate::tool::Tool::access`]): ones declaring `"read"` are judged as reads (reads unrestricted ⇒
    ///   always allow), undeclared ones are still judged as "persist" — the host can't see its intent, and "missing one
    ///   write-block" costs far less than "blocking one read too many", so the conservative direction is to treat every
    ///   path-like parameter as a potential write. It also **prefers false negatives over false positives**: a miss only
    ///   thins one coarse net, a misjudgment would reject a legitimate call outright.
    ///
    /// All three paths use the **same** `Workplace` judgment, so there is no "two sets of rules".
    fn workplace_reject(
        &self,
        kind: &ToolKind,
        access: crate::workplace::Access,
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
                let raw = args.get(crate::workplace::PATH_ARG).and_then(|v| v.as_str())?;
                self.rt.workplace.check_access(raw, access).err()
            }
            ToolKind::Plugin(_) => {
                for (_key, raw) in crate::workplace::scan_paths(args) {
                    // Use `check_access` rather than `check`: the coarse net itself presumes neither read nor write,
                    // it's decided by the `access` the caller passed. Passing `Read` makes it always `Ok`,
                    // so "read-only tools may touch out-of-workplace paths" holds **automatically**,
                    // using the same judgment as `read_file` / `list_dir`.
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
///
/// `kind` decides whether to append the plugin's **escape hatch** ([`PLUGIN_READ_HINT`]): only plugin
/// tools need it; host file tools shouldn't see it — that text would steer the model's attention toward a
/// path that doesn't exist for them (host tools have no notion of "declaring read-only"; their tier is hardcoded in the host's hands).
fn reject_text(kind: &ToolKind, reject: &crate::workplace::Reject) -> String {
    match kind {
        ToolKind::Plugin(_) => format!("{}\n\n{}", reject.model_text(), crate::i18n::t(PLUGIN_READ_HINT)),
        _ => reject.model_text(),
    }
}

/// The extra passage shown to the model when a plugin tool's path parameter hits the boundary.
///
/// Added only in the `ToolKind::Plugin` case. Why it exists: the coarse net blocking a **read-only** tool as a write is a
/// **false positive**, yet the model only has the generic error at hand ("don't retry, ask the user for escalation") —
/// so it either gives up or goes asking for privilege escalation. But this doesn't need to touch the boundary at all: **the plugin just adds one declaration line**.
/// Without this path, one false positive would trade for one needless privilege widening.
///
/// The wording must **clearly override** the generic error's "don't retry": that line is for "really wants to write out of bounds",
/// this is a different matter (the tool is read-only to begin with); if unstated the model would give up following the previous line.
const PLUGIN_READ_HINT: &str = "[If this tool is read-only, the \"do not retry\" line above does not apply]\nIf this plugin tool is only meant to **read** (scanning / counting / listing directories / parsing files), then what blocked you is not a permission, but the plugin not declaring itself read-only.\nAsk the plugin to add `\"read\"` as the 4th position of its tool declaration; after rebuilding the plugin it can access paths outside the workspace as usual. In the Python SDK you write the triple as a 4-tuple:\n  \"<tool name>\": (\"<description>\", <JSON Schema>, <handler>, \"read\")\nThe host then judges it as a **read** — the same tier as read_file / list_dir — and **no privilege escalation from the user is needed**.Conversely, tools that write files / delete things must **not** add this position.";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    /// A **fake** plugin tool: reports kind Plugin, `access` is supplied by the test.
    ///
    /// We use it instead of spawning a real plugin subprocess: this test pins down "how the boundary uses the declared access",
    /// which has nothing to do with "how the plugin process starts" — the latter has its own real-plugin self-test (`selftest`).
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
        Agent::new(llm, rt)
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aox-agent-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// **A/B comparison**: the same tool, the same out-of-workplace path, differing only by one declaration.
    ///
    /// Why we must assert both sides: testing only "declared read → allowed" would also pass if we deleted the whole judgment (always allow);
    /// testing only "undeclared → blocked" would also pass if we hardcoded `access` to `Write` and ignored the declaration.
    /// Only both together prove "this tier is **genuinely decided by the declaration**" — which is the entire content of this change.
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
            a_read.workplace_reject(&kind, access, &args).is_none(),
            "a read-only tool accessing a path outside the workspace must be allowed — this is that \"plugin reads are opened up too\" requirement"
        );

        // ② Undeclared (= persist) ⇒ the same path is blocked
        let a_write = agent_with(&ws, "fake_scan", Access::Write).await;
        let access = a_write.rt().tool_access("fake_scan").await;
        assert_eq!(access, Access::Write, "undeclared means it writes to disk; that is the default tier");
        let rej = a_write
            .workplace_reject(&kind, access, &args)
            .expect("a plugin tool that does not declare read-only must be intercepted with an outside path");
        assert!(rej.model_text().contains("permission boundary"), "{}", rej.model_text());

        // The host file-tool path wasn't affected by this change: the same out-of-workplace path is still blocked (writing tier)
        let host_write = a_write
            .workplace_reject(&ToolKind::Host, Access::Write, &args)
            .expect("a host write tool with an outside path must be intercepted");
        assert!(host_write.model_text().contains("permission boundary"));
        // And the read tier is still allowed — consistent with the "reads unrestricted" rule set on 2026-10-04
        assert!(a_write.workplace_reject(&ToolKind::Host, Access::Read, &args).is_none());

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// The **plugin-only escape hatch** in the out-of-bounds text: plugins must see it, host file tools must not.
    ///
    /// The reasoning for asserting both is the same as the previous test: asserting only "plugins have it"
    /// would also pass if the hint were unconditionally added to all tools — which would make host file tools
    /// receive advice pointing at a **nonexistent mechanism** (host tools have no "declare read-only" notion;
    /// their tier is hardcoded in the host's hands), steering the model onto a wrong track.
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
}
