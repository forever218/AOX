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
    /// The workspace root **already baked into `messages[0]`** (the system prompt).
    ///
    /// The prompt names the workspace (`prompt::env_section`), and the root only exists after the
    /// first turn and can change mid-conversation (`/workplace`, the GUI button). Comparing against
    /// this is what lets [`Agent::refresh_system_prompt`] rebuild the prompt **only when it actually
    /// changed** — otherwise the model keeps believing it is still in the previous directory.
    prompt_ws: PathBuf,
    /// The current task checkpoint (`crate::task`), when this conversation has one.
    ///
    /// `None` is the normal state for a one-off question or a single-file edit: the host never invents
    /// a checkpoint, so "no checkpoint" costs nothing at all.
    task: Option<crate::task::TaskState>,
    /// The checkpoint **as last written to the file**, for "write only when it really changed".
    ///
    /// Without it every turn would append a `kind:"task"` row (the state is rebuilt/compared each
    /// turn), and a long conversation would accumulate hundreds of identical records. `None` = nothing
    /// persisted yet in this conversation's current incarnation.
    task_saved: Option<crate::task::TaskState>,
    /// The last compacted request body (`crate::compact`), with its watermark.
    ///
    /// A **cache, not state**: `messages` stays the full conversation and remains what gets written
    /// to disk. It exists so one turn's tool loop, which calls the model many times over the same
    /// message list, does not re-summarize the same prefix on every round.
    view_cache: Option<crate::compact::ViewCache>,
    /// Whether the "cannot get under the budget by folding history" warning has already been shown
    /// for this conversation. It is a **config** problem (the budget is below the fixed part), so
    /// repeating it on every round would be noise, and there is nothing the user can do from here
    /// except change the config or start a new conversation.
    compact_warned: bool,
    /// Which tools this conversation is **advertising** (see [`crate::tool::scope`]).
    /// Per conversation, not per runtime: the runtime's registry is shared by every conversation and
    /// every UI, whereas "which of those tools I have discovered" is part of what this conversation
    /// knows. Reset when the conversation changes; never persisted — it is a projection, the same
    /// kind of thing as `view_cache`, and a resumed conversation can ask `find_tools` again.
    tool_scope: crate::tool::scope::Scope,
    /// The **tail of the last tool result** the host saw, kept for `verify`'s `output_contains`.
    ///
    /// The whole point of that check is "the text came from a tool, not from the model", so the host
    /// has to keep a copy of what it actually received. Only the tail is kept: test summaries and
    /// exit statuses live there, and a full copy of every result would double the memory this
    /// conversation already costs. Cleared with the conversation, never persisted (it is a buffer,
    /// not state — after a resume there is nothing to check anyway).
    last_output: Option<String>,
}

/// The workspace root that should appear in the system prompt.
///
/// The live root when there is one; otherwise the app root — that covers both "not decided yet"
/// (`Pending`, before the first turn) and the unrestricted tier (`Full`, which has no root).
fn prompt_workspace_root(rt: &Runtime) -> PathBuf {
    rt.workplace.snapshot().1.unwrap_or_else(|| rt.root.clone())
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
        // Remember which workspace that prompt names, so `refresh_system_prompt` can tell when it
        // has gone stale (see the `prompt_ws` field docs).
        let prompt_ws = prompt_workspace_root(&rt);
        // Built before `rt` is moved into `Self` below. The scope starts from `[tools]` only; the
        // "is the list too long to send whole" decision is made per request against the live
        // registry size (see `Runtime::tool_schemas_scoped`), so nothing here can go stale.
        let tool_scope = crate::tool::scope::Scope::from_config(&rt.config.tools);
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

    /// Rebuild the system prompt (`messages[0]`) so it names the **current** workspace.
    ///
    /// The prompt bakes in the workspace root (`prompt::env_section`); that root only exists after
    /// the first turn and can change mid-conversation (`/workplace`, the GUI button). Without this,
    /// the model keeps believing it is still in the previous directory — the reported symptom was
    /// "I picked the Desktop, but the model asked whether it should tidy the documents in its own
    /// install directory". Rebuilding **only when the root actually changed** keeps it free in the
    /// common case.
    ///
    /// The system message is **memory-only**: it is never persisted and is rebuilt on resume, so
    /// replacing it here does not break the "all writes to `messages` go through `push_message`"
    /// invariant — that one is about conversation messages.
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

    /// Ensure the current conversation already has an on-disk file. On failure we warn only once and don't interrupt the conversation.
    ///
    /// The file lands in **this conversation's workspace** (`<workspace>/.aox/history/`), which is why
    /// this runs *after* `ensure_started` in `run_turn`: by the time the first message is persisted the
    /// root is already decided, so the file is written in the right place from the start and never has
    /// to be migrated afterwards.
    fn ensure_log(&mut self, ui: &mut dyn Ui) {
        if self.log.is_some() {
            return;
        }
        let cfg = &self.rt.config.history;
        if !cfg.enabled {
            return;
        }
        let at = self.rt.workplace.active_root();
        match self.rt.store.create(Some(&self.rt.config.provider.model), at.as_deref()) {
            Ok(log) => self.log = Some(log),
            Err(e) if !self.log_error_reported => {
                self.log_error_reported = true;
                ui.emit(AgentEvent::Warn(crate::tf!(
                    "conversation history cannot be written to {} ({}): this conversation will not be saved",
                    self.rt.store.history_dir_for(at.as_deref()).display(),
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

    /// Break the request **about to be sent** into sections, with a rough token estimate for each.
    ///
    /// Only the host can answer this: it owns the system prompt, the tool schemas and the message
    /// list, while the endpoint sees just the assembled body and reports a single total for it. The
    /// cache-hit window (`gui/cache.rs`) walks this list to attribute the real `prompt` / `cached`
    /// figures back to the sections they came from.
    ///
    /// `msgs` is the **body actually going out**, not necessarily `self.messages`: once the context is
    /// compacted (`crate::compact`) the request is a projection of the conversation, and measuring the
    /// full history here would draw a gauge that describes a request nobody sent.
    ///
    /// The estimate follows [`TokenEstimate`]'s rule (about 4 ASCII characters per token, 1 token
    /// per CJK character). That is good enough for proportions and is **never quoted as a total** —
    /// the exact number comes back from the provider and replaces it on screen.
    ///
    /// The order is the order the sections occupy in the request body, and it is load-bearing: a
    /// prefix cache hit covers the earliest tokens first, so where a section sits decides how much
    /// of the hit belongs to it.
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
        // The task checkpoint travels with the conversation (see `crate::task`). A file written before
        // checkpoints existed simply has no such row, and an unreadable one degrades to `None` — this
        // line must never be able to fail a resume.
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
        // The output buffer belongs to the conversation that produced it; evidence about "what the
        // last tool said" is meaningless after switching away. A check that needs it will simply ask
        // for a fresh run.
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
    ///
    /// `None` — or a title that is blank once trimmed — means "show the automatic title again": the
    /// row is written with no value at all, which is exactly how readers spell "no rename" (see
    /// `store::jsonl`). The user-facing validation (one line, a length limit) belongs to the front end
    /// that asked the question; what is enforced here is only what must hold for the file's sake.
    ///
    /// The **current** conversation goes through the session we already hold instead of opening the
    /// file a second time: two append handles on one file would race, and this thread is the only one
    /// allowed to write into it (the same rule `append_acts` follows). Every other conversation has
    /// nobody holding it, so it is opened just long enough to append the row and dropped again.
    ///
    /// ⚠️ The CLI has no rename entry point, so on that target this is dead code and the compiler says
    /// so. Silenced here rather than by a `cfg` or a `#[allow]` on the module: the two targets genuinely
    /// differ (`src/main.rs` compiles these files as its own modules, so each target gets its own
    /// dead-code pass), and this is the item the difference is about -- the same reason `mod space`
    /// carries the attribute in `src/main.rs`.
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
        // the tier temporarily switched to in the last conversation is not inherited; the next turn's first message works in the app root.
        // Without clearing it, after "+" the model would still write into the dir picked in the last conversation, which is no longer a new conversation.
        self.rt.workplace.reset_for_new_conversation();
        // The new conversation's file isn't created yet; the persisted record is filled in by `persist_workplace` on the first message;
        // here we clear "already persisted" first, so we don't compare against the last conversation's record and miss writing that first message.
        self.workplace_logged = None;
        // A checkpoint belongs to the conversation it was recorded in. Carrying it into the next one
        // would tell the model it is still working on the previous task — the exact confusion this
        // feature exists to prevent.
        self.task = None;
        self.task_saved = None;
        self.view_cache = None;
        self.compact_warned = false;
        self.last_output = None;
        // A different conversation means a different set of tools "already discovered". The config's
        // pinned names survive (`Scope::reset` only clears what this conversation turned on).
        self.tool_scope.reset();
    }

    /// Drop everything from `len` onward, in memory **and** on disk.
    ///
    /// `Vec::truncate` semantics: **keeps the first `len`**. The on-disk file rolls back in sync —
    /// whatever is dropped from memory must also be dropped from the file, otherwise we'd leave a
    /// broken record that can't be replayed as a request body.
    ///
    /// This is the blunt primitive. The *policy* for "how far back an interrupt should reach" lives
    /// in [`Self::interrupt_rollback_point`], and that is what both front ends call.
    pub fn truncate_messages(&mut self, len: usize) {
        self.messages.truncate(len);
        if let Some(log) = self.log.as_mut() {
            log.rewrite(&self.messages);
        }
    }

    /// Where to roll the conversation back to when the user interrupts a running turn.
    ///
    /// Returns the **largest** `n` in `[mark, messages.len()]` for which `messages[..n]` is still a
    /// legal request body (see [`crate::message::is_legal_body`]) — i.e. drop the in-flight,
    /// unreplayable tail and **nothing more**.
    ///
    /// ⚠️ Deliberately *not* the start of the turn. That was the old behaviour, and it threw away
    /// two things it had no business touching: the user's own instruction for this turn, and every
    /// tool call that had **already run**. The model then saw a bare message with no context and
    /// restarted the task from scratch — while the files on disk still carried the edits it had
    /// just made, so the conversation history and reality disagreed with each other.
    ///
    /// The original motive was sound and still is — a dangling `tool_calls` is refused by the
    /// endpoint with a flat 400 — but it only argues for "cut back to a legal boundary", not for
    /// "cut back to the *turn* boundary". Those are different points, and only the second one is
    /// destructive.
    ///
    /// `mark` is the history length **before** this turn (`run_turn` pushes the user message
    /// itself, so `messages[mark]` is that message). It acts as a floor: we never roll back past
    /// where this turn started.
    pub fn interrupt_rollback_point(&self, mark: usize) -> usize {
        let msgs = &self.messages;
        // At most **one** batch can be incomplete, and it is the last one: the loop only asks the
        // model again once every call of the previous batch has its result. So finding the last
        // assistant that carries calls is enough — no need to validate the whole history.
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
            // Drop the dangling (or half-answered) batch, keeping the user's instruction and every
            // completed step before it. `mark` is the floor for the pathological case where the
            // last incomplete batch somehow sits *before* this turn.
            i.max(mark)
        }
    }

    /// Roll the conversation back after an interrupt, keeping every completed step.
    ///
    /// The single entry point for both front ends (`src/main.rs` CLI, `gui/src/bridge.rs` GUI) —
    /// the granularity decision lives in [`Self::interrupt_rollback_point`], not in the front ends,
    /// so the two cannot drift apart.
    ///
    /// A **no-op** when that point is already the end of the history (nothing was mid-flight): no
    /// truncation, and therefore no pointless rewrite of the conversation file either.
    pub fn rollback_interrupted_turn(&mut self, mark: usize) {
        let n = self.interrupt_rollback_point(mark);
        if n < self.messages.len() {
            self.truncate_messages(n);
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
        // Workplace: the `default` tier fixes its root (the app root) on the **first message** (`started` guarantees this happens once per conversation).
        //
        // Placed at the very front of the turn (before `push_message` and the loop) so the root is
        // fixed **before any tool call this turn**. Otherwise, when the model wants to write a file in
        // the first message it would be rejected as "workplace not yet established", which would baffle
        // the user — a fake failure caused by implementation order, not by a permission boundary.
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
        // Scratch hygiene, part 1: whatever a **crashed** turn left behind (`SCRATCH_STALE` — a day
        // is long enough that no live session notices). The turn-end sweep below is the one that does
        // the everyday work; this is the safety net for the process that never got there.
        //
        // Both sweeps live here, in the agent, rather than in the two frontends: "clean up after
        // yourself" is a property of the host, and a CLI-only or GUI-only version would be a bug
        // nobody notices until they switch frontends.
        if let Some(ws) = self.rt.workplace.active_root() {
            crate::workplace::sweep_scratch(&ws, crate::workplace::SCRATCH_STALE);
        }
        // `ensure_started` may have just decided the root (first turn), and the user may have
        // switched it since the last turn — either way the prompt must name the current workspace
        // before the model sees this message.
        self.refresh_system_prompt();
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
        // Persist the checkpoint **before** the turn is closed out, and on both success and failure:
        // "the task died halfway" is precisely the moment the next `resume` needs to know where it got
        // to. Writes only when something actually changed (see `persist_task`).
        self.persist_task();
        // Scratch hygiene, part 2: the everyday sweep. `<ws>/.aox/tmp` is where the host points
        // `TMP`/`TEMP`, so it is where the model's throwaway scripts land — and before this line
        // existed, nothing ever removed them (122 files / 4.5 MB found in this very repo, none of it
        // asked for). The ten-minute grace keeps a script alive across the turns of one task; see
        // `workplace::SCRATCH_GRACE`.
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

    /// The input size of the most recent call in this turn, if the endpoint reported one.
    ///
    /// Only for [`Ui::call_usage`]'s fallback argument: an endpoint that reports `cached` but omits
    /// `prompt` on an individual call would otherwise have a cache figure and nothing to measure it
    /// against.
    pub fn turn_last_prompt(&self) -> Option<u64> {
        self.turn.last_prompt
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

    // ───────── Task checkpoint (`crate::task`) ─────────

    /// Which tools this conversation is advertising (see [`crate::tool::scope`]).
    ///
    /// Read by the loop once per round, right before the request is assembled: the advertised list
    /// changes only when the registry grows (a plugin was installed) or the model discovered
    /// something, so re-deciding per round costs a lock and buys correctness.
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

    /// Mark that warning as shown (see [`Agent::compact_warned`]).
    pub fn set_compact_warned(&mut self, warned: bool) {
        self.compact_warned = warned;
    }

    /// Record a file this turn **really changed**, from the tool result that changed it.
    ///
    /// Host-side and unconditional by design: `modified_files` is evidence, so it is taken from
    /// `ToolOutput::path` (only a tool that actually wrote a file fills it) rather than from anything
    /// the model says. With no checkpoint the call is a no-op — a one-off edit must not create state.
    pub fn note_modified_file(&mut self, path: &std::path::Path) {
        let Some(st) = self.task.as_mut() else { return };
        st.note_modified(&path.to_string_lossy());
    }

    /// Keep the tail of the last tool result, for `verify`'s `output_contains`.
    ///
    /// Deliberately **not** gated on a checkpoint existing: the buffer is also what makes "run the
    /// command, then check its output" work for a task that has not been checkpointed yet, and it
    /// costs one small string. The cap keeps a 20 KB tool result from being mirrored in memory — the
    /// part worth checking (a summary, an exit status) is at the end.
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

    /// Body of the `verify` tool (intercepted in [`Agent::authorize_and_execute`], same story as
    /// `task_state`: the evidence belongs to this conversation's checkpoint).
    ///
    /// The check itself runs in `crate::tool::verify` — this function only supplies the two things
    /// the check cannot know by itself (the last tool output, and the workspace epoch it should be
    /// stamped with) and records the outcome.
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
        // The criterion has to point at a real item **before** anything is recorded. An off-by-one
        // here would otherwise produce evidence that can never justify `done` — the model would
        // believe it had proof, and the gate would keep saying "never checked".
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

    /// Append the checkpoint to the conversation file, **only when it differs** from what is already
    /// recorded there.
    ///
    /// Called at the end of a turn (and by the compaction step before it elides history). Not per tool
    /// call: a checkpoint row is a few hundred bytes to a few KB, and rewriting it after every edit
    /// would be the "constant disk churn" this feature is supposed to avoid.
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

    /// Body of the `task_state` tool (intercepted in [`Agent::authorize_and_execute`], the same way
    /// `ask_user` is — a checkpoint hangs off this conversation, not off the tool registry).
    ///
    /// Rejections are **total**: a bad patch changes nothing. A checkpoint that is half-updated is
    /// worse than an old one, because the next resume would read a state that never existed.
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
                // The next open step, named with its number: the checkpoint reply is read right
                // before the model continues, so this is the cheapest place to put the exact tick
                // call it should make when that step is finished. Without it the model tends to run
                // a whole chain of tools and tick everything at the end — the GUI can only draw a
                // checkpoint it receives, so the delay looks like a UI bug.
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
                // `done` is only accepted when the gate passed (see `task::apply`), but the model's
                // own plan may still have open steps. Saying so out loud is cheap; accepting in
                // silence is how "did four of six and called it done" stays invisible.
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

    /// Push a message into the in-memory context **without** writing it to the conversation file.
    ///
    /// Exactly one family of messages uses this: host hints that are **regenerated** — today, the task
    /// checkpoint injected by `resume`. Writing it would append the same paragraph once per open, and
    /// rewriting the file just to avoid that duplicate is worse. The system prompt at index 0 is
    /// already memory-only, so the precedent exists; anything the user or the model actually *says*
    /// still goes through [`Agent::push_message`].
    fn push_ephemeral(&mut self, msg: Message) {
        self.messages.push(msg);
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
        if let Some(reject) = self.workplace_reject(&kind, access, name, &args) {
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
            // `task_state`: same interception story as `ask_user` — the checkpoint belongs to this
            // conversation, so `Tool::call` (which only sees `Runtime`) could not record it. Placed
            // after authorization like everything else: being a host tool is not a bypass.
            if name == crate::tool::task::TASK_TOOL {
                let out = self.task_tool(args);
                // A checkpoint is the only thing a human can watch a long task by, and until
                // 2026-10-23 nothing outside the model's context ever saw one. Emitted **here**
                // rather than inside `task_tool`: that function needs no `Ui`, which is what lets
                // the checkpoint logic be tested without one. A rejected patch changed nothing, so
                // only a successful write is announced.
                if !out.is_error {
                    if let Some(st) = self.task.clone() {
                        ui.emit(AgentEvent::Task(Box::new(st)));
                    }
                }
                return out;
            }
            // `find_tools`: third interception, same story — "make a hidden tool visible" changes
            // this **conversation's** scope, and `Tool::call` only ever sees `&Runtime`. Placed after
            // authorization like the other two: being a host tool is not a bypass. It is also the only
            // one of the three that **writes to `self`**: everything else here is a read of runtime state.
            if name == crate::tool::discover::FIND_TOOL {
                let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                return crate::tool::discover::search(&self.rt, &mut self.tool_scope, query).await;
            }
            // `verify`: fourth interception, same story. The evidence has to be stamped with this
            // conversation's workspace epoch and recorded on **its** checkpoint — neither is visible
            // to `Tool::call`, which only ever sees `&Runtime`. Also after authorization: "it only
            // reads files" is not a reason to skip the policy.
            if name == crate::tool::verify::VERIFY_TOOL {
                return self.verify_tool(args);
            }
            // The numbers the user sees have to be **this call's**, not the generic per-tool
            // defaults: `shell`/`bash` compute their own budget from `timeout_secs` (up to the
            // configured ceiling), and the model may announce an estimate of its own. Reading them
            // through the same trait hooks the outer guard uses means the two can never disagree.
            let (cap, expect) = self.rt.call_hints(name, &args).await;
            let cap = cap.unwrap_or(self.rt.config.agent.tool_timeout_secs);
            ui.emit(AgentEvent::ToolRunning {
                name: name.to_string(),
                timeout_secs: cap,
                expect_secs: expect,
            });
            // A plugin that has just been built gets its tools **advertised immediately**: the model
            // is one step away from calling them, and making it run `find_tools` for the thing it
            // wrote itself would be a round trip that buys nothing. `args` moves into `execute`, so
            // the plugin name is read out first. A failed build adds nothing — there is no plugin.
            let built = if name == crate::tool::meta::CREATE_TOOL || name == crate::tool::meta::UPDATE_TOOL
            {
                args.get("name").and_then(|v| v.as_str()).map(str::to_owned)
            } else {
                None
            };
            let out = self.rt.execute(name, args).await;
            if let Some(plugin) = built.filter(|_| !out.is_error) {
                for t in self.rt.plugin_tool_names(&plugin).await {
                    self.tool_scope.enable(&t);
                }
            }
            out
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
    /// - `ToolKind::Meta` (`create_plugin` / `update_plugin` / `list_plugins` / `remove_plugin`): declares no path parameter, so allow outright.
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
                // The shell tools carry their directory in `cwd`, not in `path` — and a missing
                // `path` here makes the generic branch below return `None` through `?`, i.e. **allow
                // the call**. That is why they are handled first and explicitly: "no parameter" must
                // never quietly mean "no boundary".
                if name == crate::tool::shell::SHELL_TOOL {
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

    /// The system prompt must name the workspace the model is confined to, and be **rebuilt when
    /// that workspace changes** — otherwise the model keeps answering as if it were still in the
    /// previous directory ("should I tidy the documents in my own install directory?" after the
    /// user had picked the Desktop).
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

    /// A tool call with the given id — enough for the rollback scan, which only reads ids.
    fn call(id: &str) -> crate::message::ToolCall {
        crate::message::ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: crate::message::FunctionCall { name: "fs_read".into(), arguments: "{}".into() },
        }
    }

    /// The whole point of the fix: an interrupt keeps the user's instruction and every
    /// **completed** step, and drops only the in-flight one.
    ///
    /// Rolling back to the start of the turn (the old behaviour) left the model with no context at
    /// all — while the files on disk still carried the edits it had already made, so the history it
    /// was given contradicted what it could read.
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

        // [0] system · [1] user · [2] assistant(c1) · [3] tool(c1) · [4] assistant(c2) ← dangling
        assert_eq!(a.interrupt_rollback_point(mark), 4, "only the dangling batch may go");

        a.rollback_interrupted_turn(mark);
        assert_eq!(a.messages_len(), 4);
        assert_eq!(a.messages()[1].role, "user", "the user's instruction survives");
        assert_eq!(a.messages()[3].role, "tool", "the completed step survives");
        assert!(crate::message::is_legal_body(a.messages()), "and what remains is replayable");

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Half a parallel batch is **still** illegal: the endpoint wants a result for every call id, so
    /// "keep the completed calls" cannot mean "keep everything". The whole batch goes — but the
    /// user's instruction, which is all the model needs to carry on, stays.
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

    /// When nothing was mid-flight there is nothing to undo, and the conversation file must not be
    /// rewritten for nothing (`rollback_interrupted_turn` only calls `truncate_messages` when the
    /// point actually moves).
    ///
    /// Covers the two shapes that mean "no rollback": a finished batch (the abort landed while
    /// waiting for the next reply) and no tool call at all (the abort landed during the very first
    /// request of the turn) — in the second case the user's instruction must still be kept.
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

    /// The floor: `mark` is never crossed. Should a dangling batch somehow sit *before* this turn,
    /// the rollback must not reach back into earlier history — the worst case is a no-op, never
    /// "erase a previous turn as well".
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

    // ───────── Task checkpoint (`crate::task`, `task_state`) ─────────

    /// The whole point of a checkpoint is **surviving a session**: write it, drop the agent, build a
    /// new one on the same conversation, and the state must come back — while `modified_files` only
    /// ever carries what a real tool result reported.
    #[tokio::test]
    async fn a_checkpoint_travels_with_the_conversation() {
        let ws = std::env::temp_dir().join("aox-task-checkpoint");
        let _ = std::fs::remove_dir_all(&ws);
        let mut a = agent_with(&ws, "t", crate::workplace::Access::Read).await;

        // A conversation must exist before a checkpoint has anywhere to live.
        let id = {
            let log = a.rt.store.create(None, None).unwrap();
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
            if let Ok(rd) = std::fs::read_dir(ws.join("history")) {
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

    /// The checkpoint reply must name the **exact next tick** while the plan is open. The GUI can
    /// only draw ticks it receives; a model that batches five steps and ticks them at the end looks
    /// like a broken progress bar even though every event was delivered immediately.
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

    /// The closing loop of the stage: `done` is a **decision the host makes from evidence**, not a
    /// value the model writes. Three things are pinned here in the order they matter:
    /// ① no evidence → refused; ② a real check → accepted; ③ a write afterwards → refused again until
    /// re-checked.
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

        // ③ The workspace moves on → the old evidence is no longer proof.
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

        // No acceptance at all: the same refusal, with the fix named.
        a.new_conversation();
        a.task_tool(json!({"goal": "g", "completed": ["c"]}));
        let out = a.verify_tool(json!({"check": "path_exists", "criterion": 1, "path": proof.to_string_lossy()}));
        assert!(out.is_error && out.content.contains("acceptance"), "{}", out.content);

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// `output_contains` is the command-level degraded path: the host can't run the tests, but it can
    /// insist the text came from a tool. Pinned here because the buffer's lifetime is what makes it
    /// trustworthy.
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
}
