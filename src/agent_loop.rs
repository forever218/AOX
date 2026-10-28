//! Loop seam (second form): move "run a whole turn" out of `Agent` and turn it into a replaceable capability.
//!
//! The stage-4 hooks could only be inserted at five points **inside** the loop — the skeleton itself was
//! still welded shut. This module hands the whole loop to an [`AgentLoop`] implementation, so
//! "trim the context", "swap in a different retry policy", or even "a completely different execution
//! model" all become replaceable things, instead of just hanging hooks off a fixed skeleton.
//!
//! # Why you must open a `Agent` write path for a loop that lives outside
//!
//! `run_loop` used to live in `Agent`, using **private** write paths like `push_message` /
//! `truncate_messages` / per-turn accounting. After the move it becomes a **peer collaborator**
//! of `Agent`, so `Agent` exposes a set of controlled primitives for it
//! (`push_message` / `truncate_messages` / `messages` / `begin_turn` / `accrue_call` / `finish_turn`).
//!
//! This is **not** "interface leakage" — the stage-3 rule "no paths on the interface" constrains
//! **backend implementation details**, whereas here two types of the same layer are collaborating.
//! The line that really must hold is unchanged: **the only write path for `messages` is
//! `Agent::push_message`** (which also writes the file). The loop never gets a bare
//! `Vec<Message>`, so it cannot create the mismatch of "in memory but not in the history file".
//!
//! # Only one implementation this cycle
//!
//! [`StandardLoop`] = the stage-4 `Agent::run_loop`, **moved statement by statement**, behaviour
//! equivalent line for line. It is the only value `config.toml [loop] impl` accepts — any other
//! value **fails at startup** in `Runtime::new`, the same principle as `[store] backend`
//! (a setting that is written but never takes effect is the hardest class of bug to track down).
//!
//! [`StandardLoop`] also settles **item 1 of the stage-4 leftovers: trim / rewrite history**.
//! The stage-4 `before_request` only dared to expose "append messages" (appending is the only
//! change that needs no `rewrite`); now that boundary is filled in by `[loop] max_rounds` —
//! trimming naturally belongs to the loop layer: it has to touch both memory and the conversation
//! file, and it must **rebuild the request** after trimming.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agent::Agent;
use crate::config::LoopConfig;
use crate::hook::{self, ErrorAction, Verdict};
use crate::message::Message;
use crate::runtime::Runtime;
use crate::tool::ToolOutput;
use crate::ui::{AgentEvent, Ui};

/// An implementation of "run one whole turn".
///
/// State when `run` takes over: the user message is **already** enqueued (done by
/// `Agent::run_turn`), and this turn's accounting has been opened (`Agent::begin_turn`).
/// Returning `Ok(())` means the model is no longer calling tools and the turn ended normally.
#[async_trait]
pub trait AgentLoop: Send + Sync {
    /// Implementation name, i.e. the value `config.toml [loop] impl` accepts.
    fn name(&self) -> &str;
    /// Run one whole turn.
    async fn run(&self, agent: &mut Agent, ui: &mut dyn Ui) -> Result<()>;
}

/// Known loop implementation names. When adding an implementation, change this and [`build`] together.
pub const IMPLS: [&str; 1] = ["standard"];

/// Default implementation name. Used when `config.toml` has no `[loop] impl`.
pub fn default_impl() -> &'static str {
    IMPLS[0]
}

/// Build a loop implementation from configuration.
///
/// **An unrecognized name is an error** — no silent fallback to `standard` — for exactly the same
/// reason as `store::backend`: a mistyped config that still starts normally shows up as
/// "I clearly wrote a different implementation, why is it still the old one?".
pub fn build(cfg: &LoopConfig) -> Result<Arc<dyn AgentLoop>> {
    let name = cfg.implementor.trim();
    if name.is_empty() || name == default_impl() {
        return Ok(Arc::new(StandardLoop));
    }
    bail!(crate::tf!(
        "unknown loop implementation: {} (available now: {})\nplease check the [loop] impl in config.toml, or remove it to use the default.",
        name,
        IMPLS.join(" / ")
    ))
}

/// Body of the `/loop` command: which implementation is active now, and whether context trimming is on.
pub fn report(rt: &Runtime) -> String {
    let cfg = &rt.config.loop_config;
    let trim = if cfg.max_rounds == 0 {
        crate::i18n::t("Off (max_rounds = 0, context unbounded)").to_string()
    } else {
        crate::tf!(
            "keep the latest {} rounds (a round starts at one user message)",
            cfg.max_rounds
        )
    };
    crate::tf!(
        "loop implementation: {} (available: {})\ncontext trimming: {}\nrequests per turn: {}\n\nchange the [loop] section in config.toml and restart for it to take effect; /hook lists the hooks, /prompt lists the prompt sections.",
        rt.loop_impl.name(),
        IMPLS.join(" / "),
        trim,
        if cfg.max_iters_per_turn == 0 {
            crate::i18n::t("Off (max_iters_per_turn = 0)").to_string()
        } else {
            crate::tf!("at most {} (max_iters_per_turn)", cfg.max_iters_per_turn)
        }
    )
}

/// The **cut point** for context trimming: returns "the index of the new start of the body";
/// `messages[1..cut]` will be dropped.
///
/// `None` = no trimming needed (this also covers "must not trim", see below).
///
/// Rules:
/// - `max_rounds == 0` → no trim. This is the default, and it is the basis for "no `[loop]` written
///   means byte-identical to stage 5".
/// - **A turn is delimited by a `role == "user"` message**: one user message opens a turn, and it
///   runs until the next user message (assistant / tool messages all belong to it). Tips injected by
///   the host hook are also user messages, so each occupies a turn too.
/// - If the turn count does not exceed `max_rounds` → no trim.
/// - The cut point **naturally lands on a user message**, so no extra alignment is needed: dropping
///   the oldest whole turn leaves a suffix that is itself a legal request body — you never get the
///   state where "a tool result remains while its assistant was dropped", which the endpoint would
///   reject with a straight 400. That property is **not re-argued here**: it *is*
///   [`crate::message::is_legal_body`], the crate's one definition of a legal boundary, and the
///   `debug_assert` below simply checks this cut against it.
/// One line, at most 120 characters: what went wrong, for a warning the user reads at a glance.
///
/// `{e:#}` renders the whole cause chain and, for an HTTP failure, up to 500 characters of the
/// endpoint's body — far too much for a status line, and it would push the useful half ("retrying in
/// 2.0 s") off the end of it. The full text still reaches the log and, if we give up, the error.
fn brief(e: &anyhow::Error) -> String {
    let full = format!("{e:#}");
    let first = full.lines().next().unwrap_or("").trim();
    if first.chars().count() <= 120 {
        return first.to_string();
    }
    format!("{}…", first.chars().take(120).collect::<String>())
}

fn trim_point(msgs: &[Message], max_rounds: usize) -> Option<usize> {
    if max_rounds == 0 {
        return None;
    }
    // Index 0 is the system section, always kept — so turn counting starts at index 1.
    let turns: Vec<usize> = msgs
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, m)| m.role == "user")
        .map(|(i, _)| i)
        .collect();
    if turns.len() <= max_rounds {
        return None;
    }
    let cut = turns[turns.len() - max_rounds];
    debug_assert_eq!(msgs[cut].role, "user", "the trim cut point must land on a user message");
    // The surviving body must still be replayable: no `tool` result may be left without the
    // `assistant` that asked for it. Checked against the shared definition rather than a
    // hand-rolled version of it (the same predicate guards the interrupt rollback in `agent.rs`).
    // Debug-only, so release builds pay nothing.
    debug_assert!(
        crate::message::is_legal_body(&msgs[cut..]),
        "the trimmed body must remain a legal request body"
    );
    Some(cut)
}

/// Standard loop: the stage-4 `Agent::run_loop`, with not a line of logic changed.
pub struct StandardLoop;

#[async_trait]
impl AgentLoop for StandardLoop {
    fn name(&self) -> &str {
        "standard"
    }

    async fn run(&self, agent: &mut Agent, ui: &mut dyn Ui) -> Result<()> {
        /// Retry cap for when the model returns an "empty reply" (no text, no tool calls).
        /// Thinking models occasionally return empty content (reasoning eats the whole output
        /// budget, upstream jitter, etc.); treating that as "task finished" would silently abort
        /// the task, so retry first and then report an explicit error.
        const MAX_EMPTY_RETRY: u32 = 2;

        let mut empty_retries = 0u32;
        // How many request errors this turn has already retried (`[retry] attempts` is the ceiling).
        // **Independent per turn** — it lives in `run`'s locals and cannot cross into the next turn,
        // so "the longer you chat the more retries you get" cannot happen.
        let mut llm_retries = 0usize;
        // Hooks need to inspect the system's current state (tool table, plugin table), while
        // `agent.messages()` is about to be lent to `llm.chat`. Clone an `Arc` out first: hooks
        // hold it, which neither fights the borrow of `agent` nor — as a bonus — guarantees that
        // "hooks cannot change Agent's internal state": they can only influence the loop through
        // their return value.
        let rt = agent.rt().clone();
        let max_rounds = rt.config.loop_config.max_rounds;
        let max_iters = rt.config.loop_config.max_iters_per_turn;
        let retry = rt.config.retry.clone();
        // "You already asked that" — see `crate::repeat`. Rebuilt per turn: a repeat only counts
        // inside one instruction, never across two.
        let mut guard = crate::repeat::Guard::default();
        // How many times one **identical** shell command has failed in this turn. This does not
        // block the retry (a transient failure can genuinely clear), but after the second failure
        // the model gets a sentence saying the command is unchanged — the loop we saw was four
        // identical `bash` calls and four identical error blocks.
        let mut shell_failures: HashMap<String, usize> = HashMap::new();
        // How many model requests this turn has spent (`[loop] max_iters_per_turn` caps it).
        let mut rounds = 0usize;
        // The project's check command, detected **once per turn** (a handful of `metadata` calls).
        // `unchecked` is the latch that makes the reminder fire once per write-batch instead of
        // nagging on every single write: it arms on the first write after a check and disarms when
        // that check actually runs.
        let check = rt.workplace.active_root().and_then(|r| crate::checks::detect(&r));
        let mut unchecked = false;
        loop {
            // ── The work ceiling ──
            // Checked **before** the request, so `max_iters_per_turn = 3` means "three requests at
            // most", not four. Breaking here ends the turn normally: `run_turn` persists the
            // checkpoint and closes the card afterwards, exactly as for a finished task.
            if max_iters > 0 && rounds >= max_iters {
                ui.emit(AgentEvent::Warn(crate::tf!(
                    "stopped after {} requests in this turn (the ceiling from [loop] max_iters_per_turn). Everything done so far is kept — send another message to carry on from here.",
                    max_iters
                )));
                break;
            }
            rounds += 1;
            // Lifetime cap for `shell_job` trees. Checked before every request so a server cannot
            // outlive its configured budget just because nobody called `shell_job` again; host exit
            // remains the hard stop (the registry owns the fences).
            {
                let rt = agent.rt();
                let expired = rt
                    .jobs
                    .lock()
                    .await
                    .expire(rt.config.shell.background_max_hours);
                if !expired.is_empty() {
                    ui.emit(AgentEvent::Warn(crate::tf!(
                        "background job lifetime reached; stopped: {}",
                        expired.join(", ")
                    )));
                }
            }
            // ── Context trimming (item 1 of the stage-4 leftovers).
            //    Placed "before this turn's request is assembled": assemble after trimming, so the
            //    model sees the trimmed context.
            //    Memory and the conversation file move together (`truncate_messages` calls
            //    `Session::rewrite` internally); otherwise a restart reads back a half history.
            if let Some(cut) = trim_point(agent.messages(), max_rounds) {
                let dropped = cut - 1;
                // Note it uses `drop_oldest` (keep the tail) rather than `truncate_messages`
                // (keep the head) — trimming wants "keep only the most recent few turns", and
                // getting the direction backwards silently keeps the oldest context.
                agent.drop_oldest(cut);
                ui.emit(AgentEvent::Info(crate::tf!(
                    "context exceeded {} rounds; dropped the oldest {} messages (memory and the conversation file were rolled back together).",
                    max_rounds,
                    dropped
                )));
            }

            // ── Hook point ① before request.
            //    The hook receives "which tools this turn uses" and "which messages to append",
            //    **not** `&mut messages`: the only write path for messages is `push_message` (which
            //    also writes the file), and handing out a bare Vec makes "in memory but not in the
            //    history file" a certainty within a minute.
            //    ⚠️ The list is **scoped**, not the whole registry (`tool::scope`): once the registry
            //    is longer than `[tools] inline_max`, plugin tools stay out until they are discovered.
            //    Gating happens **before** the hook so a hook can still add to the list; gating after
            //    it would silently drop whatever the hook added.
            let mut req = hook::Request {
                tools: rt.tool_schemas_scoped(agent.tool_scope()).await,
                inject: Vec::new(),
            };
            if let Verdict::Reject(why) = rt.hooks.before_request(&rt, &mut req).await {
                bail!(crate::tf!("this round's request was blocked by a host hook: {}", why));
            }
            for m in req.inject.drain(..) {
                agent.push_message(m, None);
            }
            let schemas = req.tools;

            // ── Context compaction (see `crate::compact`) ──
            //    Judged here rather than once per turn: every round pushes tool results into the
            //    context, so "was it too big" has to be asked again before every call.
            //    `None` means "nothing to do" and the request goes out exactly as `agent.messages()`
            //    says — the byte-stability that keeps the prompt cache warm.
            let view = crate::compact::prepare(agent, ui, &schemas).await;
            let body: &[Message] = view.as_deref().unwrap_or_else(|| agent.messages());

            // Show the UI what this request is made of (see `Ui::context_map`). Before the call and
            // not after: the reply's usage figures get attributed to these sections, and once the
            // reply is in, the body that produced it is no longer around to measure.
            // Measured on `body`, not on the full history: after compaction they differ, and a map
            // that describes a request nobody sent is worse than no map.
            let sections = agent.context_sections(body, &schemas);
            ui.context_map(&sections).await;

            ui.set_status(crate::i18n::t("Thinking")).await;
            // Hand the streaming receiver to the Provider: a thinking model's reasoning is echoed
            // in real time while waiting. This is written as its own statement (rather than
            // matching the result directly below) so the borrow of `agent.messages()` ends here —
            // otherwise the `&mut agent` that `push_message` needs below would conflict.
            let chat = {
                let sink = ui.stream_sink();
                // "A request starts here" (attempt 0). The front end uses it to mark where this
                // attempt's rows begin, so a later retry can roll exactly this attempt back — a
                // stream that was cut has already drawn half an answer.
                if let Some(s) = sink.as_deref() {
                    s.on_attempt(0, std::time::Duration::ZERO);
                }
                agent.llm().chat(body, &schemas, sink.as_deref()).await
            };
            let reply = match chat {
                Ok(r) => r,
                // ── Hook point ⑤ request error.
                //    This is the loop's only "infrastructure failure" exit (the `bail!` for a failed
                //    tool execution or an empty-reply exit does not go through here). With no hook,
                //    or when all hooks return `Propagate`, this is equivalent to the original `?`.
                Err(e) => match rt.hooks.on_error(&rt, &e).await {
                    // A component/hook explicitly says to retry: inject the tip it gives and start
                    // over, **without consulting the budget** — preventing an infinite loop is its
                    // own responsibility (see the docs of `ErrorAction::Retry`).
                    ErrorAction::Retry(note) => {
                        agent.push_message(Message::user(note), None);
                        continue;
                    }
                    // Nobody asked for a retry — but "nobody is managing it" should not mean
                    // "never even try once". `[retry] attempts` is the configured policy for exactly
                    // this: a rate limit or a dropped stream is worth asking again, without having to
                    // write a component for it.
                    //
                    // Two questions decide it, and they are answered in different places:
                    // ① **is this failure worth repeating at all** — `llm::failure::retryable`, which
                    //    reads the status out of the error (401/400/413 fail on the first attempt:
                    //    retrying a wrong key just delays the message the user has to see);
                    // ② **how long to wait** — `llm::failure::delay`, exponential from `[retry] base_ms`,
                    //    never shorter than a `Retry-After` the server sent, never longer than `max_ms`.
                    ErrorAction::Propagate => {
                        if !crate::llm::failure::retryable(&e) || llm_retries >= retry.attempts {
                            return Err(e);
                        }
                        llm_retries += 1;
                        let wait = crate::llm::failure::delay(llm_retries, &retry, crate::llm::failure::retry_after(&e));
                        // Before the sleep, not after: whatever the abandoned attempt already put on
                        // screen has to go now, so the user sees "retrying" rather than half an
                        // answer that is about to be replaced.
                        if let Some(s) = ui.stream_sink() {
                            s.on_attempt(llm_retries as u32, wait);
                        }
                        ui.emit(AgentEvent::Warn(crate::tf!(
                            "model request failed ({}); retrying in {} s (attempt {}/{})",
                            brief(&e),
                            format!("{:.1}", wait.as_secs_f32()),
                            llm_retries,
                            retry.attempts
                        )));
                        tokio::time::sleep(wait).await;
                        // **No message is injected.** The failed attempt never touched
                        // `agent.messages()` (a reply only lands there once it arrives whole), so
                        // `continue` re-sends the identical request. It used to push a "please
                        // continue the task you were on" user message, which meant every upstream
                        // hiccup permanently added a sentence the model then had to read back.
                        continue;
                    }
                },
            };
            // Accounting: when the endpoint gives no usage, each item is None and accrue skips quietly.
            agent.accrue_call(&reply.usage);

            // Hand the same figures to the UI **per call**, not just per turn. The turn aggregate is
            // reported much later (`Agent::finish_turn`), so without this a live view has nothing to
            // show until the whole turn is over — which for a ten-round tool loop is a long silence.
            // Read after `accrue_call` so the fallback `last_prompt` already carries this call.
            ui.call_usage(&reply.usage, agent.turn_last_prompt()).await;

            // ── Hook point ② after reply.
            //    Deliberately placed **after** accounting: the tokens of a rejected call really were
            //    spent, so it must not go unrecorded just because it was not adopted.
            //    Rejection = treat as one invalid reply → reuse the "empty reply" retry path below
            //    (retry cap included) rather than inventing a separate retry mechanism.
            let mut reply = reply;
            if let Verdict::Reject(why) = rt.hooks.after_reply(&rt, &mut reply).await {
                if empty_retries < MAX_EMPTY_RETRY {
                    empty_retries += 1;
                    ui.emit(AgentEvent::Warn(crate::tf!(
                        "a host hook vetoed this reply (attempt {}): {}",
                        empty_retries,
                        why
                    )));
                    agent.push_message(
                        Message::user(crate::tf!(
                            "{} your previous reply was vetoed by a host rule: {}. Please adjust and retry.",
                            crate::history::HOST_PREFIX,
                            why
                        )),
                        None,
                    );
                    continue;
                }
                bail!(crate::tf!("the model's reply was vetoed by host hooks {} times in a row: {}", MAX_EMPTY_RETRY + 1, why));
            }

            let msg = reply.message.clone();
            let calls = msg.tool_calls.clone().unwrap_or_default();

            // The model gave nothing at all (neither text nor tool calls) — it must never be treated
            // as "task finished normally", otherwise the symptom is "the task silently aborts and the
            // model returned nothing". Retry first, then report an explicit error.
            if reply.is_empty() {
                if reply.truncated() {
                    ui.emit(AgentEvent::Warn(crate::tf!(
                        "model output was truncated by max_tokens={} and the content is empty (reasoning tokens filled the output budget).",
                        rt.provider.cfg().max_tokens.unwrap_or(0)
                    )));
                }
                if empty_retries < MAX_EMPTY_RETRY {
                    empty_retries += 1;
                    ui.emit(AgentEvent::Warn(crate::tf!(
                        "the model returned an empty reply (attempt {}); retrying…",
                        empty_retries
                    )));
                    agent.push_message(
                        Message::user(crate::tf!(
                            "{} your previous reply was empty. If the task is not done yet, keep calling tools; if it is done, give a final summary in natural language.",
                            crate::history::HOST_PREFIX
                        )),
                        None,
                    );
                    continue;
                }
                bail!(crate::tf!(
                    "the model returned an empty reply {} times in a row; this round has stopped.\ndiagnostics: {}. Reproduce with `AOX_DEBUG_LLM=1 aox`; the raw response is written to logs/llm.jsonl.\ncommon causes: ① the model is a thinking model but max_tokens is too small (reasoning tokens count against the quota); ② upstream rate limiting / timeout; ③ the model does not support tool calls.",
                    MAX_EMPTY_RETRY + 1,
                    reply.diag(),
                ));
            }
            empty_retries = 0;

            if reply.truncated() {
                ui.emit(AgentEvent::Warn(crate::tf!(
                    "model output was truncated by max_tokens={} ({}); this round's content may be incomplete. Consider raising max_tokens in config.toml (a thinking model's reasoning tokens count against the quota too).",
                    rt.provider.cfg().max_tokens.unwrap_or(0),
                    reply.diag(),
                )));
            }

            // If there are tool calls → this text is just process narration; no tool calls → it is the final answer
            if let Some(text) = msg.text().map(str::to_string) {
                if calls.is_empty() {
                    ui.emit(AgentEvent::AssistantText(text));
                } else {
                    ui.emit(AgentEvent::InterimText(text));
                }
            }
            agent.push_message(msg, None);
            if calls.is_empty() {
                break; // task finished
            }

            // The **tool-produced files** of this response (attached by `attach_file`, or declared
            // by a plugin in its result). Collect them, and inject a single user message once this
            // whole batch of tools has finished — the reason is in the paragraph below.
            let mut produced: Vec<PathBuf> = Vec::new();
            // Which call produced which files, in call order. Kept separately from `produced` so
            // the GUI can draw an image under its own row rather than dumping every picture at the
            // end of the turn (the old `Images` event had no owner and put them at the top of the
            // transcript).
            let mut produced_by_call: Vec<(String, Vec<PathBuf>)> = Vec::new();
            for call in calls {
                let name = call.function.name.clone();
                let mut args: Value = match serde_json::from_str(&call.function.arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        // When the arguments are not valid JSON there is not yet an "object to
                        // modify", so there is nothing for a hook to intervene in — return an error
                        // result as-is (exactly as before hooks existed).
                        let out = ToolOutput::err(crate::tf!("tool arguments are not valid JSON: {}", e));
                        ui.emit(AgentEvent::ToolCall { name: name.clone(), args: json!({}) });
                        ui.emit(AgentEvent::ToolResult {
                            name: name.clone(),
                            is_error: true,
                            preview: out.content.clone(),
                            diff: None,
                            diff_detail: None,
                            path: None,
                        });
                        agent.push_message(Message::tool_result(&call.id, &name, out.content), Some(true));
                        continue;
                    }
                };
                // ── Hook point ③ before tool call: can modify args, can block (blocking skips the
                //    tool execution).
                //    ⚠️ The assistant message carrying tool_calls is **already persisted** at this
                //    point, and what stays in the file is the model's original arguments — the change
                //    here only affects this execution, **it does not write back to history**: history
                //    must faithfully record "what the model asked for", and the hook is merely a
                //    host-side execution policy.
                if let Verdict::Reject(why) = rt.hooks.before_tool(&rt, &name, &mut args).await {
                    let out = ToolOutput::err(why);
                    ui.emit(AgentEvent::ToolCall { name: name.clone(), args: args.clone() });
                    ui.emit(AgentEvent::ToolResult {
                            name: name.clone(),
                            is_error: true,
                            preview: out.content.clone(),
                            diff: None,
                            diff_detail: None,
                            path: None,
                        });
                    agent.push_message(Message::tool_result(&call.id, &name, out.content), Some(true));
                    continue;
                }
                ui.emit(AgentEvent::ToolCall { name: name.clone(), args: args.clone() });
                // ── "You already asked that" ──
                // A read-only call whose arguments are unchanged **and** after which nothing at all
                // ran: the answer is still in the conversation, so running it again would buy a
                // second copy of up to 20,000 characters. A write/delete/command in between clears
                // the verdict (the measured case is a script re-run after being edited) — the rule
                // is not "same tool + same args", see `crate::repeat`'s module docs.
                // Kept before `args` is handed to the tool: "did this call run the check" is answered
                // by the command line, and the command line is inside the arguments.
                let call_text = if name == "shell" || name == "bash" { Some(args.to_string()) } else { None };
                let failure_key = call_text.as_ref().map(|t| format!("{name}\u{0}{t}"));
                let read_only = crate::repeat::Guard::eligible(&rt, &name).await;
                let repeated = read_only && guard.repeats(&name, &args, agent.messages());
                // Only reads are ever recorded, so only reads need their arguments kept around.
                let guard_args = read_only.then(|| args.clone());
                let mut output = if repeated {
                    ToolOutput::ok(crate::repeat::notice())
                } else {
                    agent.authorize_and_execute(ui, &name, args).await
                };
                // `output.path` is consumed by the event below, and "did this call write a file" is
                // needed after it.
                let wrote = output.path.is_some();
                // A check that ran clears the latch — including one that failed: the model now knows
                // the answer, which is the whole point of asking.
                if let (Some(c), Some(t)) = (&check, &call_text) {
                    if c.ran_in(t) {
                        unchecked = false;
                    }
                }
                // ── Hook point ④ after tool result: can modify the result, can block (blocking =
                //    replace the whole block with a reasoned error).
                //    Placed before `ui.emit` — so the UI and the history file see the **same**
                //    (modified) result.
                if let Verdict::Reject(why) = rt.hooks.after_tool(&rt, &name, &mut output).await {
                    output = ToolOutput::err(why);
                }
                // ── Evidence for the task checkpoint ──
                // Read **before** `output.path` is moved into the event below. Only a tool that really
                // wrote a file fills this field, so `modified_files` stays host-verified: the model
                // cannot claim a file it did not touch. A no-op when this conversation has no
                // checkpoint (see `crate::task`), and after `after_tool` so a blocked result — whose
                // `path` was just dropped — is not recorded.
                if let Some(p) = &output.path {
                    agent.note_modified_file(p);
                }
                // ── What this tool really said ──
                // Kept for `verify`'s `output_contains`: the check may only cite text a tool produced,
                // never text the model remembers. Taken before `content` is moved into the message.
                agent.note_tool_output(&output.content);
                ui.emit(AgentEvent::ToolResult {
                    name: name.clone(),
                    is_error: output.is_error,
                    preview: truncate(&output.content, 300),
                    diff: output.diff,
                    diff_detail: output.diff_detail,
                    // The file this call wrote, when it was a file edit. Travels with the result so
                    // the workspace view can list "what this turn touched" without re-deriving it
                    // from prose.
                    path: output.path,
                });
                // A delete lands in the **recycle bin**, and that is worth saying out loud: "it is
                // not really gone" is the one thing a user has to know about a delete, and it is
                // invisible from the transcript. The number comes from the result and nowhere else
                // (see `ToolOutput::recycled`) — the tool's own `content` says it too, but that
                // line is written for the model, whereas this event is the shape for a human.
                if output.recycled > 0 {
                    ui.emit(AgentEvent::Recycled(output.recycled));
                }
                let mut content = cap_chars(&output.content, rt.config.agent.max_result_chars);
                // ── "the same command again" ──
                //
                // Not a block: the call already ran. The note is for the **next** round — a command
                // that fails the same way twice is usually a wrong approach (or a long-lived
                // process mistaken for a one-shot), and silently letting the model try a fifth time
                // is what turns one bad guess into a whole turn.
                if let Some(key) = &failure_key {
                    if let Some(note) =
                        repeated_shell_failure_note(&mut shell_failures, key, &name, output.is_error)
                    {
                        content.push_str(&note);
                    }
                }
                // ── "this write is unverified" ──
                //
                // `verify` + the gate only cover tasks that have a checkpoint; a short task had no
                // pressure at all (measured: half of the turns that wrote code never ran a check
                // afterwards). The line goes **into the tool result** because the model is the one who
                // has to act on it, and it is a statement of fact ("nothing has run since"), not an
                // order. Once per batch: repeating it on every write teaches the model to ignore it.
                if wrote && !unchecked {
                    if let Some(c) = &check {
                        unchecked = true;
                        content.push_str(&crate::tf!(
                            "\n\nhost: this write is unverified — `{}` has not been run since the last change.",
                            c.command
                        ));
                    }
                }
                // The guard learns from the **final** text (post-hook, post-cap) so "is that result
                // still in the conversation" compares like with like. A skipped call records
                // nothing: the entry it matched is still valid and still points at the real result.
                // A **failed** call is not recorded either — its "result" (a denial, a timeout) is
                // not an answer, and handing it back as "unchanged" would present a failure as a
                // fact about the file.
                match (&guard_args, repeated, output.is_error) {
                    (Some(a), false, false) => guard.record(&name, a, &content),
                    (None, _, _) => guard.touched(),
                    _ => {}
                }
                // Taken, not cloned: this is the only consumer of the list, and a shell result can
                // declare several attachments.
                let attached = std::mem::take(&mut output.attach);
                if !attached.is_empty() {
                    produced.extend(attached.iter().cloned());
                    produced_by_call.push((name.clone(), attached));
                }
                agent.push_message(
                    Message::tool_result(&call.id, &name, content),
                    Some(output.is_error),
                );
            }

            // ── Files produced by tools → open a separate **user** message
            //
            // This is not a style choice, it is a hard protocol constraint: the endpoint returns a
            // straight 400 when it sees an image in a `system` / `assistant` message (stated plainly
            // in the official "usage limits"), and the `tool` role in the OpenAI protocol can only
            // carry a string in content — **the only legal landing spot for an image is a user
            // message**.
            //
            // The position is also pinned by that constraint: it must come **after** the tool result
            // just pushed (a tool result must immediately follow its corresponding assistant
            // tool_calls; inserting another message in between makes argument validation fail), and
            // it must come **before** the next request (otherwise this turn's images miss this turn).
            if !produced.is_empty() {
                let (msg, shown, problems) = produced_files_message(&produced);
                for p in &problems {
                    ui.emit(AgentEvent::Warn(crate::tf!("the attachment produced by the tool could not be passed to the model: {}", p)));
                }
                if let Some(m) = msg {
                    agent.push_message(m, None);
                    // Also have the UI **draw** these images — "the image came out" is the most
                    // confusing thing about a picture the model can see but the human cannot.
                    //
                    // One event per producing call, with only the paths that really made it into
                    // the request. The GUI then puts each picture below that call's row instead of
                    // starting a separate image block at the top of the conversation.
                    for (tool, paths) in produced_by_call {
                        let visible: Vec<PathBuf> = paths
                            .into_iter()
                            .filter(|p| shown.contains(p))
                            .collect();
                        if !visible.is_empty() {
                            ui.emit(AgentEvent::ToolImages { tool, paths: visible });
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Assemble the user message for "tool-produced files".
///
/// Returns `(message to push, paths actually attached, notes to report to the user)`.
/// A message of `None` means none could be attached; "paths actually attached" is handed to the
/// UI for **display** (see `AgentEvent::ToolImages`) — only the successful ones may be reported, else
/// the UI would draw a "display failed" line for a path that was already blocked, right below a
/// blocking reason for that same path.
///
/// Extracted as a **pure function** (not touching `Ui`) so it can be asserted directly — the
/// protocol constraint "must be a user message, must carry a parts array" deserves a test that does
/// not need to actually run a loop.
fn produced_files_message(paths: &[PathBuf]) -> (Option<Message>, Vec<PathBuf>, Vec<String>) {
    let (items, mut problems) = crate::attach::probe_all(paths);
    if items.is_empty() {
        return (None, Vec::new(), problems);
    }
    let shown: Vec<PathBuf> = items.iter().map(|a| a.path.clone()).collect();
    let names = items.iter().map(|a| format!("- {}", a.name)).collect::<Vec<_>>().join("\n");
    // The wording spells out "what to look at": without it the model often guesses content from the
    // file name alone, and then sending that image was pointless
    let note = crate::tf!(
        "{} the files below were produced by the tool call just now; the image content has already been sent to you directly — judge by looking at the images instead of guessing from file names:\n{}",
        crate::history::HOST_PREFIX,
        names
    );
    match Message::user_with_files(&note, &items) {
        Ok(m) => (Some(m), shown, problems),
        Err(e) => {
            problems.push(e);
            (None, Vec::new(), problems)
        }
    }
}

/// Count a failed shell command and return the "you are repeating yourself" note on the second and
/// later identical failures. A later success clears the entry.
///
/// Deliberately a note, not a refusal: the command already ran, and a transient failure can clear.
/// The goal is to make the loop visible to the model, not to make "retry after fixing" impossible.
fn repeated_shell_failure_note(
    seen: &mut HashMap<String, usize>,
    key: &str,
    name: &str,
    is_error: bool,
) -> Option<String> {
    if !is_error {
        seen.remove(key);
        return None;
    }
    let n = seen.entry(key.to_string()).or_insert(0);
    *n += 1;
    (*n >= 2).then(|| {
        crate::tf!(
            "\n\nhost: this exact {} command has now failed {} times in this turn. Running it \
             unchanged again is unlikely to give a different result — read the error, change the \
             command, or use shell_job for a process that is meant to stay alive.",
            name,
            *n
        )
    })
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn cap_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        let total = s.chars().count();
        crate::tf!("{}…\n[result too long; truncated, original length {} characters]", s.chars().take(cap).collect::<String>(), total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FunctionCall, ToolCall};

    fn call(n: usize) -> ToolCall {
        ToolCall {
            id: format!("call-{n}"),
            kind: "function".into(),
            function: FunctionCall { name: "fs_read".into(), arguments: "{}".into() },
        }
    }

    /// The full shape of one turn: user → assistant(with tool call) → tool result → assistant(final answer).
    fn one_turn(n: usize) -> Vec<Message> {
        vec![
            Message::user(format!("question {n}")),
            Message::assistant(None, Some("let me think".into()), vec![call(n)]),
            Message::tool_result(&format!("call-{n}"), "fs_read", format!("content #{n}")),
            Message::assistant(Some(format!("answer {n}")), None, vec![]),
        ]
    }

    /// The full body of `turns` turns (index 0 is the system header).
    fn body(turns: usize) -> Vec<Message> {
        let mut v = vec![Message::system("SYS")];
        for n in 1..=turns {
            v.extend(one_turn(n));
        }
        v
    }

    #[test]
    fn no_max_rounds_means_never_trim() {
        assert_eq!(trim_point(&body(9), 0), None, "with the default max_rounds=0, nothing must be trimmed");
    }

    #[test]
    fn under_or_equal_the_limit_is_untouched() {
        assert_eq!(trim_point(&body(2), 5), None);
        assert_eq!(trim_point(&body(5), 5), None, "exactly at the limit, no trim");
    }

    #[test]
    fn over_limit_cuts_oldest_whole_turns() {
        let msgs = body(3); // [sys, u1,a,t,a, u2,a,t,a, u3,a,t,a]
        let cut = trim_point(&msgs, 2).expect("3 rounds exceed a limit of 2, so it must trim");
        assert_eq!(cut, 5, "round 2's user message is at index 5");
        assert_eq!(msgs[cut].content.as_ref().and_then(|v| v.as_str()), Some("question 2"));
        assert_eq!(cut - 1, 4, "what is dropped is a whole round (4 messages), not counting system");
    }

    /// The one dangerous spot in trimming: leaving a "tool result whose assistant was already
    /// dropped" message, which makes the endpoint return a straight 400. This pins the invariant
    /// that "the cut point always lands on a user message" — and it is exactly this that makes
    /// "no extra alignment needed" hold.
    #[test]
    fn cut_always_lands_on_a_turn_start() {
        for turns in 1..8usize {
            let msgs = body(turns);
            for keep in 1..=turns {
                if let Some(cut) = trim_point(&msgs, keep) {
                    assert!(cut > 0 && cut < msgs.len(), "with {turns} rounds keeping {keep}, the cut point is out of range");
                    assert_eq!(msgs[cut].role, "user", "with {turns} rounds keeping {keep}, the cut point is off");
                    assert_eq!(
                        msgs[cut - 1].role,
                        "assistant",
                        "the last message of the dropped segment should be the previous round's tail, otherwise a round is split in half"
                    );
                }
            }
        }
    }

    #[test]
    fn system_head_is_never_cut() {
        let msgs = body(4);
        let cut = trim_point(&msgs, 1).unwrap();
        assert_eq!(msgs[0].role, "system", "system is always at index 0 and never trimmed");
        assert!(cut > 0);
    }

    /// When there are not enough turns, no trim is allowed even if `max_rounds` is large — a "cap"
    /// is a cap, not "trim until only that many remain".
    #[test]
    fn trim_never_grows_the_context() {
        for keep in [1usize, 2, 7] {
            let msgs = body(3);
            if let Some(cut) = trim_point(&msgs, keep) {
                assert!(cut < msgs.len(), "trimming only shortens, never lengthens");
            }
        }
    }

    /// Build an `Agent` that touches no real directory and knows no Provider (`log` is `None`, so
    /// `push_message` below only touches memory — which isolates the "trim direction" for testing).
    fn test_agent() -> Agent {
        let root = std::env::temp_dir().join("aox-loop-test");
        let rt = Runtime::new(crate::config::Config::dummy(), root.clone(), root.join("plugins"))
            .expect("the test runtime should not fail");
        let llm: Arc<dyn crate::llm::LlmProvider> =
            Arc::new(crate::llm::UnavailableProvider { reason: "test".into() });
        Agent::new_with(llm, rt)
    }

    /// ⚠️ This one pins the **direction** specifically: trimming must go through
    /// `Agent::drop_oldest` (keep the tail), not `truncate_messages` (keep the head). While
    /// implementing this the two were exactly what got confused, so the result was "this turn's
    /// input is dropped while the oldest context stays" — and memory and file were **wrong in
    /// agreement**, so looking only at what was persisted revealed nothing. That is why this test
    /// asserts *which turns* remain.
    #[test]
    fn trimming_keeps_the_newest_turns_not_the_oldest() {
        let mut agent = test_agent();
        for m in body(3).into_iter().skip(1) {
            agent.push_message(m, None);
        }
        assert_eq!(agent.messages().len(), 13, "1 system message + 3 rounds × 4 messages");

        let cut = trim_point(agent.messages(), 2).expect("3 rounds exceed a limit of 2, so it must trim");
        agent.drop_oldest(cut);

        let roles: Vec<&str> = agent.messages().iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles.len(), 9, "system + the last 2 rounds");
        assert_eq!(roles[0], "system", "the system prompt is always kept");
        assert_eq!(roles[1], "user");
        assert_eq!(
            agent.messages()[1].content.as_ref().and_then(|v| v.as_str()),
            Some("question 2"),
            "what remains must be the **latest** two rounds, not the oldest two"
        );
        assert_eq!(
            agent.messages().last().unwrap().content.as_ref().and_then(|v| v.as_str()),
            Some("answer 3"),
            "the last round must be kept whole through its final answer"
        );
    }

    /// The boundary of `drop_oldest`: when the cut point is meaningless, nothing moves (better to
    /// keep the full context than trim it into an empty shell).
    #[test]
    fn drop_oldest_is_a_noop_on_meaningless_cuts() {
        let mut agent = test_agent();
        for m in body(2).into_iter().skip(1) {
            agent.push_message(m, None);
        }
        let before = agent.messages().len();
        for cut in [0usize, 1, before, before + 5] {
            agent.drop_oldest(cut);
            assert_eq!(agent.messages().len(), before, "cut = {cut} should not touch anything");
        }
    }

    #[test]
    fn unknown_impl_is_an_error() {
        let cfg = LoopConfig { implementor: "react".into(), max_rounds: 0, max_iters_per_turn: 0 };
        let e = build(&cfg).err().expect("an unknown implementation must error").to_string();
        assert!(e.contains("react"), "the error must include the misspelled name: {e}");
        assert!(e.contains("standard"), "the error must tell the user what is currently available: {e}");
    }

    #[test]
    fn empty_or_default_impl_builds_standard() {
        for name in ["", "standard", "  standard  "] {
            let cfg = LoopConfig { implementor: name.into(), max_rounds: 0, max_iters_per_turn: 0 };
            let lp = build(&cfg).unwrap_or_else(|e| panic!("{name:?} should be available: {e:#}"));
            assert_eq!(lp.name(), "standard");
        }
    }

    /// Images produced by tools must become a **user** message, with `content` being a parts array.
    ///
    /// Both are **hard protocol requirements**, not style: an image in a `tool` / `system` /
    /// `assistant` message makes the endpoint return a straight 400, and only a parts array can hold
    /// an image — a plain string cannot.
    #[test]
    fn produced_images_become_a_user_message_with_parts() {
        let d = std::env::temp_dir().join("aox-loop-produced");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let shot = d.join("shot.png");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();

        let (msg, _shown, problems) = produced_files_message(&[shot]);
        assert!(problems.is_empty(), "{problems:?}");
        let msg = msg.expect("an attachable file must produce a message");
        assert_eq!(msg.role, "user", "images may only appear in user messages");
        let parts = msg.content.as_ref().unwrap().as_array().expect("must be a parts array");
        assert!(parts.iter().any(|p| p["type"] == "image_url"), "{parts:?}");
        // The prefix is for the UI to recognize: a message with this prefix renders as a tip, not a user bubble
        assert!(msg.flat_text().starts_with(crate::history::HOST_PREFIX), "{}", msg.flat_text());
        assert!(msg.flat_text().contains("shot.png"), "{}", msg.flat_text());

        let _ = std::fs::remove_dir_all(&d);
    }

    /// A plugin reports a path that does not exist (the most common mistake a plugin author makes):
    /// it **must not be silent**.
    ///
    /// The consequence of silence is two-way — the author keeps waiting for an image that will never
    /// appear, while the model has no idea it is missing something and guesses content from the file
    /// name.
    #[test]
    fn a_missing_produced_file_is_reported_not_swallowed() {
        let missing = std::env::temp_dir().join("aox-loop-no-such-file-xyz.png");
        let _ = std::fs::remove_file(&missing);
        let (msg, _shown, problems) = produced_files_message(&[missing]);
        assert!(msg.is_none());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("no-such-file-xyz.png"), "{}", problems[0]);
    }

    /// When only half of a batch is valid: the valid ones must be attached and the invalid ones must
    /// speak up — not drop the whole batch.
    #[test]
    fn one_bad_file_does_not_sink_the_whole_batch() {
        let d = std::env::temp_dir().join("aox-loop-mixed");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let good = d.join("good.png");
        std::fs::write(&good, b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        let missing = d.join("missing.png");

        let (msg, _shown, problems) = produced_files_message(&[good, missing]);
        assert!(msg.is_some(), "the qualifying one must still be attached");
        assert_eq!(problems.len(), 1, "{problems:?}");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// The loop we are fixing: four identical failing `bash` calls in a row. The first retry gets a
    /// pointed note, and a success clears the count so a later unrelated failure starts at one again.
    #[test]
    fn a_repeated_failed_shell_command_is_called_out() {
        let mut seen = HashMap::new();
        let key = "bash\u{0}{\"command\":\"npm run dev &\"}";
        assert!(repeated_shell_failure_note(&mut seen, key, "bash", true).is_none());
        let note = repeated_shell_failure_note(&mut seen, key, "bash", true)
            .expect("the second identical failure is worth saying");
        assert!(note.contains("failed 2 times"), "{note}");
        assert!(note.contains("shell_job"), "{note}");
        assert!(repeated_shell_failure_note(&mut seen, key, "bash", true)
            .expect("third failure")
            .contains("failed 3 times"));
        // A success is evidence that the command can work; forget the history.
        assert!(repeated_shell_failure_note(&mut seen, key, "bash", false).is_none());
        assert!(seen.is_empty());
    }
}
