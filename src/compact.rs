//! Context budget and automatic compaction (stage 3).
//! # In one sentence.
//! Compaction is a request projection, not a rewrite of history: `Agent::messages` (which *is* the conversation...

use serde_json::Value;

use crate::agent::Agent;
use crate::llm::TokenEstimate;
use crate::message::Message;
use crate::ui::{AgentEvent, Ui};

/// Tool results within the last this-many messages are never stubbed: the ones the model is about
/// to use must stay verbatim.
pub const KEEP_RECENT: usize = 10;
/// A tool result shorter than this saves little when stubbed and only adds a line of noise.
pub const ELIDE_MIN_CHARS: usize = 800;
/// Character cap for the summary input (the raw conversation): summarising costs tokens too, so the
/// whole history cannot simply be sent again.
pub const SUMMARY_INPUT_CHARS: usize = 24_000;
/// Character cap for a single message inside the summary input.
pub const SUMMARY_ITEM_CHARS: usize = 900;
/// Word cap on the summary output (written into the prompt, and used to size the cut).
pub const SUMMARY_MAX_WORDS: usize = 400;
/// Character cap for the fallback digest.
pub const DIGEST_MAX_CHARS: usize = 4_000;
/// The budget is never taken below this, or the very first call would fold the history into an
/// empty shell.
pub const SMALLEST_BUDGET: u64 = 2_048;

/// The token budget for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Tokens usable for input: the reply's reservation is **already subtracted**.
    pub budget: u64,
}

impl Policy {
    /// Read the budget from config.
    pub fn from_config(cfg: &crate::config::Config) -> Option<Self> {
        Self::from_parts(&cfg.agent, &cfg.provider)
    }

    /// The same derivation, from the two sections separately.
    pub fn from_parts(
        agent: &crate::config::AgentConfig,
        provider: &crate::config::ProviderConfig,
    ) -> Option<Self> {
        let explicit = agent.context_budget_tokens;
        if explicit > 0 {
            return Some(Self { budget: explicit });
        }
        let window = provider.context_window? as u64;
        // `compact_ratio` comes from config but is clamped to 0.3..=1.0: 0.1 would throw away most
        // of the window, and 1.0 squeezes out the reply's reservation.
        let ratio = if agent.compact_ratio.is_finite() {
            (agent.compact_ratio as f64).clamp(0.3, 1.0)
        } else {
            0.8
        };
        let reserve = provider.max_tokens.unwrap_or(crate::config::DEFAULT_MAX_TOKENS) as u64;
        let budget = (window as f64 * ratio) as u64;
        Some(Self { budget: budget.saturating_sub(reserve).max(SMALLEST_BUDGET) })
    }
}

/// Estimated tokens for one request (messages + tool schemas + per-message protocol overhead).
pub fn estimate(msgs: &[Message], schemas: &[Value]) -> u64 {
    let mut e = TokenEstimate::default();
    for m in msgs {
        e.add(&m.flat_text());
        if let Some(r) = &m.reasoning_content {
            e.add(r);
        }
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                e.add(&c.function.name);
                e.add(&c.function.arguments);
            }
        }
        if let Some(n) = &m.name {
            e.add(n);
        }
    }
    let mut s = TokenEstimate::default();
    if let Ok(json) = serde_json::to_string(schemas) {
        s.add(&json);
    }
    // Each message also carries fixed protocol overhead (role, separators); 4 tokens is a rough
    // allowance.
    e.tokens() + 4 * msgs.len() as u64 + s.tokens()
}

/// A compacted request body together with its watermark.
#[derive(Debug, Clone)]
pub struct ViewCache {
    /// Cache key: `(message count, fingerprint of the last message)`. Messages only append, so "the count changed"...
    pub key: (usize, String),
    /// The compacted body (system prompt included).
    pub body: Vec<Message>,
    /// How many messages are folded into `summary` (the watermark).
    pub covered: usize,
    /// The summary produced by folding. `None` means only stubbing happened, no summary yet.
    pub summary: Option<String>,
}

/// Cache key: the count plus the last message's fingerprint
fn cache_key(msgs: &[Message]) -> (usize, String) {
    let last = msgs.last().map(|m| {
        let mut s = m.role.clone();
        s.push('\u{1}');
        s.push_str(&m.flat_text());
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                s.push('\u{1}');
                s.push_str(&c.id);
            }
        }
        s
    });
    (msgs.len(), last.map(|s| crate::tool::fingerprint::of_bytes(s.as_bytes())).unwrap_or_default())
}

/// Shape of the body: system prompt + (summary) + the kept tail.
fn assemble(msgs: &[Message], covered: usize, summary: &str) -> Vec<Message> {
    let mut out = Vec::with_capacity(msgs.len() - covered + 2);
    out.push(msgs[0].clone());
    if covered > 1 {
        out.push(Message::user(summary.to_string()));
    }
    out.extend_from_slice(&msgs[covered.min(msgs.len())..]);
    out
}

/// Stub out old tool results.
/// Only what lies before `KEEP_RECENT`: the recent ones are exactly what the model needs right now, and stubbing...
fn elide_old_tool_results(body: &mut [Message]) -> usize {
    let end = body.len().saturating_sub(KEEP_RECENT);
    let mut n = 0;
    for m in body.iter_mut().take(end).skip(1) {
        if m.role != "tool" {
            continue;
        }
        let text = m.flat_text();
        let chars = text.chars().count();
        if chars < ELIDE_MIN_CHARS {
            continue;
        }
        let name = m.name.clone().unwrap_or_else(|| "tool".to_string());
        // The stub is written for the model: what was elided, how big it was, and that re-running the
        // call brings it back.
        m.content = Some(Value::String(format!(
            "[old tool result elided to save context: {name}, {chars} chars. Call {name} again if you still need it.]"
        )));
        n += 1;
    }
    n
}

/// Pick one candidate cut: the later the better (less information lost), as long as the result fits the budget.
fn pick_cut(msgs: &[Message], covered: usize, budget: u64, schemas: &[Value]) -> Option<usize> {
    let candidates: Vec<usize> = msgs
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(i, m)| *i > covered && *i < msgs.len() && m.role == "user")
        .map(|(i, _)| i)
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // The summary itself takes up room: estimate characters from the word cap, and round up.
    let placeholder = "x".repeat(SUMMARY_MAX_WORDS * 2);
    candidates
        .iter()
        .rev()
        .find(|&&cut| estimate(&assemble(msgs, cut, &placeholder), schemas) <= budget)
        .copied()
}

/// Summary input: render the messages being folded as plain text (with a cap).
fn render_excerpt(msgs: &[Message], covered: usize, cut: usize) -> String {
    let mut out = String::new();
    for m in msgs.iter().take(cut).skip(covered.max(1)) {
        let text = m.flat_text();
        if text.is_empty() {
            continue;
        }
        let role = match m.role.as_str() {
            "user" => "user",
            "assistant" => "assistant",
            "tool" => "tool",
            other => other,
        };
        let piece: String = text.chars().take(SUMMARY_ITEM_CHARS).collect();
        out.push_str(&format!("[{role}] {piece}\n"));
        if out.chars().count() >= SUMMARY_INPUT_CHARS {
            out.push_str("[… excerpt truncated …]\n");
            break;
        }
    }
    out
}

/// The summary request. Its prompt is deliberately English-only (the same convention as the system
/// prompt).
fn summary_request(excerpt: &str, previous: Option<&str>, checkpoint: Option<&str>) -> Vec<Message> {
    let mut input = String::new();
    if let Some(cp) = checkpoint {
        input.push_str("## Host-maintained task checkpoint (facts, keep them)\n");
        input.push_str(cp);
        input.push_str("\n\n");
    }
    if let Some(prev) = previous {
        input.push_str("## Summary of what came before this excerpt\n");
        input.push_str(prev);
        input.push_str("\n\n");
    }
    input.push_str("## Conversation excerpt to compress\n");
    input.push_str(excerpt);
    let prompt = format!(
        "Compress the conversation excerpt below into a handover note for the assistant that continues this work.\n\
         Keep, and keep precisely: the user's goal and any constraint they stated; decisions already made and why; \
         files changed (paths) and what changed; attempts that failed; questions still open.\n\
         Drop: chit-chat, repeated tool output, step-by-step narration, reasoning.\n\
         Do not invent anything that is not in the excerpt or the checkpoint. Do not address the user.\n\
         Output plain text, at most {SUMMARY_MAX_WORDS} words, no preamble, no headings other than short ones.\n\n\
         {input}"
    );
    vec![
        Message::system(
            "You compress conversation history for another assistant. Be faithful, terse and concrete.",
        ),
        Message::user(prompt),
    ]
}

/// Fallback when summarising fails: a deterministic digest (the first line of each message) — free,
/// non-blocking, and it does lose detail.
fn digest(msgs: &[Message], covered: usize, cut: usize, previous: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(prev) = previous {
        out.push_str(prev);
        out.push_str("\n");
    }
    for m in msgs.iter().take(cut).skip(covered.max(1)) {
        let text = m.flat_text();
        if text.is_empty() {
            if let Some(calls) = &m.tool_calls {
                let names: Vec<&str> = calls.iter().map(|c| c.function.name.as_str()).collect();
                out.push_str(&format!("- called: {}\n", names.join(", ")));
            }
            continue;
        }
        let first = text.lines().next().unwrap_or("").trim();
        let piece: String = first.chars().take(200).collect();
        out.push_str(&format!("- {}: {piece}\n", m.role));
        if out.chars().count() >= DIGEST_MAX_CHARS {
            out.push_str("- [… truncated …]\n");
            break;
        }
    }
    out
}

/// The body of the summary message (model-facing, English)
fn summary_message(summary: &str) -> String {
    format!(
        "{} the earlier part of this conversation was compacted to save context. Everything before this point is \
         summarized below; nothing else about it survives. Continue the task from here, and re-read any file you \
         are about to change.\n\n{}",
        crate::history::HOST_PREFIX,
        summary
    )
}

/// Build this round's request body.
pub async fn prepare(agent: &mut Agent, ui: &mut dyn Ui, schemas: &[Value]) -> Option<Vec<Message>> {
    // The budget follows the live provider: a model switch (smaller window, larger reply reservation) takes effect...
    let policy = {
        let rt = agent.rt();
        Policy::from_parts(&rt.config.agent, &rt.provider.cfg())?
    };
    let key = cache_key(agent.messages());

    // One batch of messages is computed once: the tool loop re-enters many times within a turn.
    if let Some(c) = agent.view_cache() {
        if c.key == key {
            return Some(c.body.clone());
        }
    }

    let before = estimate(agent.messages(), schemas);
    if before <= policy.budget {
        return None;
    }

    // Compaction only happens over budget, so one snapshot here is cheap: the model call later borrows `agent`...
    let msgs = agent.messages().to_vec();

    // Where to start from: continue from a usable watermark (the prefix no longer changes), or from
    // the full history.
    let (start_body, covered, summary) = match agent.view_cache() {
        Some(c) if c.covered > 1 && c.covered <= msgs.len() && c.summary.is_some() => {
            let s = c.summary.clone().unwrap();
            (assemble(&msgs, c.covered, &summary_message(&s)), c.covered, Some(s))
        }
        _ => (msgs.clone(), 1, None),
    };

    // L1: stub out old tool results
    let mut body = start_body;
    let elided = elide_old_tool_results(&mut body);
    let after_l1 = estimate(&body, schemas);
    if after_l1 <= policy.budget {
        // This may be "enough on the first try" (`covered == 1`, no summary), or "the new part was
        // handled by stubbing" (a summary exists; the watermark stays put and is not re-summarized).
        ui.emit(AgentEvent::ContextCompacted { before, after: after_l1, folded: 0, elided });
        agent.set_view_cache(Some(ViewCache { key, body: body.clone(), covered, summary }));
        return Some(body);
    }

    // L2: fold the oldest complete user turns
    let Some(cut) = pick_cut(&msgs, covered, policy.budget, schemas) else {
        // Either there is no legal cut (a single turn), or "everything folded is still over" (the system prompt + tool...
        if !agent.compact_warned() {
            ui.emit(AgentEvent::Warn(crate::tf!(
                "the request cannot be brought under the context budget by folding history (~{} tokens vs budget {}): the system prompt, the tool definitions and the current turn already exceed it. Raise [agent] context_budget_tokens, or the model's context window in Settings → Model, or start a new conversation.",
                before,
                policy.budget
            )));
            agent.set_compact_warned(true);
        }
        agent.set_view_cache(Some(ViewCache { key, body: body.clone(), covered, summary }));
        return Some(body);
    };

    // The last real task before compacting: persist the checkpoint first. The summary input uses it,
    // and "compaction loses context" is exactly why checkpoints exist — it must not be an afterthought.
    agent.persist_task();
    let checkpoint = agent.task().map(|t| t.context_block());
    ui.emit(AgentEvent::ContextCompacting { used: before, budget: policy.budget });

    let excerpt = render_excerpt(&msgs, covered, cut);
    let previous = summary.clone();
    let request = summary_request(&excerpt, previous.as_deref(), checkpoint.as_deref());
    let llm = agent.llm().clone();
    let reply = llm.chat(&request, &[], None).await;
    let new_summary = match reply {
        Ok(r) => {
            // The summary call really costs money; the ledger must not pretend otherwise.
            agent.accrue_call(&r.usage);
            let text = r.message.flat_text();
            if text.trim().is_empty() {
                ui.emit(AgentEvent::Warn(
                    crate::i18n::t(
                        "the context summary came back empty; falling back to a host-side digest (details will be lost).",
                    )
                    .to_string(),
                ));
                digest(&msgs, covered, cut, previous.as_deref())
            } else {
                text
            }
        }
        Err(e) => {
            ui.emit(AgentEvent::Warn(crate::tf!(
                "the context summary failed ({}); falling back to a host-side digest (details will be lost).",
                format!("{e:#}")
            )));
            digest(&msgs, covered, cut, previous.as_deref())
        }
    };

    let mut body = assemble(&msgs, cut, &summary_message(&new_summary));
    let extra = elide_old_tool_results(&mut body);
    let after = estimate(&body, schemas);
    let folded = cut.saturating_sub(covered.max(1));
    ui.emit(AgentEvent::ContextCompacted { before, after, folded, elided: elided + extra });
    if after > policy.budget {
        ui.emit(AgentEvent::Warn(crate::tf!(
            "context is still over budget after compaction (~{} tokens, budget {}). One message is probably too large on its own.",
            after,
            policy.budget
        )));
    }
    agent.set_view_cache(Some(ViewCache {
        key,
        body: body.clone(),
        covered: cut,
        summary: Some(new_summary),
    }));
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys() -> Message {
        Message::system("sys prompt")
    }

    fn big(n: usize) -> String {
        "x".repeat(n)
    }

    /// One turn = one user message plus some assistant/tool messages. Build n turns.
    fn conversation(turns: usize, tool_chars: usize) -> Vec<Message> {
        let mut v = vec![sys()];
        for i in 0..turns {
            v.push(Message::user(format!("user turn {i}")));
            v.push(Message::assistant(None, None, Vec::new()));
            let mut tool = Message::tool_result(&format!("c{i}"), "read_file", big(tool_chars));
            tool.name = Some("read_file".into());
            v.push(tool);
        }
        v
    }

    fn cfg(window: Option<u32>, ratio: f32) -> crate::config::Config {
        let mut c = crate::config::Config::dummy();
        c.provider.context_window = window;
        c.provider.max_tokens = Some(1_000);
        c.agent.compact_ratio = ratio;
        c
    }

    #[test]
    fn no_window_and_no_explicit_budget_means_no_compaction() {
        assert!(Policy::from_config(&cfg(None, 0.8)).is_none());
        // An explicit budget wins even when there is no window
        let mut c = cfg(None, 0.8);
        c.agent.context_budget_tokens = 5_000;
        assert_eq!(Policy::from_config(&c).unwrap().budget, 5_000);
    }

    #[test]
    fn the_budget_leaves_room_for_the_reply() {
        let p = Policy::from_config(&cfg(Some(10_000), 0.8)).unwrap();
        // 10000*0.8 - 1000
        assert_eq!(p.budget, 7_000);
        // A tiny window must still not compact down to zero
        let mut c = cfg(Some(100), 0.8);
        c.provider.max_tokens = Some(500);
        assert_eq!(Policy::from_config(&c).unwrap().budget, SMALLEST_BUDGET);
    }

    #[test]
    fn stubs_replace_only_old_tool_results() {
        let mut msgs = conversation(6, 2_000);
        let n = elide_old_tool_results(&mut msgs);
        assert!(n > 0, "there are old, large tool results");
        let tail = &msgs[msgs.len() - KEEP_RECENT..];
        assert!(
            tail.iter().all(|m| m.role != "tool" || m.flat_text().chars().count() >= ELIDE_MIN_CHARS),
            "the most recent tool results must stay intact"
        );
        let stub = msgs[3].flat_text();
        assert!(stub.contains("elided"), "{stub}");
        assert!(stub.contains("read_file"), "the stub must name the tool: {stub}");
        assert_eq!(msgs[0].flat_text(), "sys prompt", "the system prompt is never touched");
    }

    #[test]
    fn the_cut_lands_on_a_user_message_and_keeps_the_system_prompt() {
        let msgs = conversation(10, 500);
        let full = estimate(&msgs, &[]);
        let cut = pick_cut(&msgs, 1, full / 2, &[]).expect("a cut must exist");
        assert_eq!(msgs[cut].role, "user");
        let body = assemble(&msgs, cut, "summary text");
        assert_eq!(body[0].flat_text(), "sys prompt");
        assert_eq!(body[1].role, "user");
        assert!(body[1].flat_text().contains("summary text"));
        assert_eq!(body.len(), msgs.len() - cut + 2);
        assert!(crate::message::is_legal_body(&body), "the compacted body must still be a legal request");
    }

    #[test]
    fn a_tighter_budget_folds_more() {
        let msgs = conversation(12, 800);
        let full = estimate(&msgs, &[]);
        let loose = pick_cut(&msgs, 1, full * 9 / 10, &[]).unwrap();
        let tight = pick_cut(&msgs, 1, full / 3, &[]).unwrap();
        assert!(tight <= loose, "a tighter budget must not keep more history (tight={tight}, loose={loose})");
        // Folded as far as possible and still too big => no cut (the caller skips the summary and only
        // warns)
        assert!(
            pick_cut(&msgs, 1, 10, &[]).is_none(),
            "a budget that nothing can satisfy must yield no cut at all"
        );
    }

    #[test]
    fn the_watermark_is_respected() {
        let msgs = conversation(10, 500);
        let cut = pick_cut(&msgs, 6, estimate(&msgs, &[]) / 2, &[]).unwrap();
        assert!(cut > 6, "a new cut must move past the watermark, otherwise compaction never progresses");
    }

    #[test]
    fn the_digest_is_deterministic_and_bounded() {
        let msgs = conversation(20, 5_000);
        let d = digest(&msgs, 1, 18, None);
        assert!(d.contains("user turn 0"), "{d}");
        assert!(d.chars().count() <= DIGEST_MAX_CHARS + 40, "{}", d.chars().count());
        assert_eq!(d, digest(&msgs, 1, 18, None), "the fallback must be deterministic");
    }

    #[test]
    fn the_excerpt_is_bounded() {
        let msgs = conversation(60, 5_000);
        let e = render_excerpt(&msgs, 1, 59);
        assert!(e.chars().count() <= SUMMARY_INPUT_CHARS + 200, "{}", e.chars().count());
    }

    #[test]
    fn the_summary_message_says_what_was_lost() {
        let text = summary_message("did A and B");
        assert!(text.contains("compacted"), "{text}");
        assert!(text.contains("did A and B"));
        assert!(text.contains("re-read"), "the model must be told to re-check files: {text}");
    }

    #[test]
    fn the_cache_key_changes_when_messages_grow_or_change() {
        let a = conversation(3, 10);
        let mut b = a.clone();
        assert_eq!(cache_key(&a), cache_key(&b));
        b.push(Message::user("more"));
        assert_ne!(cache_key(&a), cache_key(&b), "a new message must invalidate the view");
        let mut c = a.clone();
        *c.last_mut().unwrap() = Message::user("different tail");
        assert_ne!(cache_key(&a), cache_key(&c), "the same length with different content must not hit");
    }

    // Wiring into `Agent` (real cache, real disk)

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use crate::runtime::Runtime;

    /// A provider that answers every request with the same summary and counts how many times it was
    /// asked — the counter *is* the assertion for "we did not summarize twice".
    struct CountingLlm {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::llm::LlmProvider for CountingLlm {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[Value],
            _sink: Option<&(dyn crate::llm::StreamSink + Send + Sync)>,
        ) -> anyhow::Result<crate::llm::ChatReply> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::llm::ChatReply {
                message: Message::assistant(Some("SUMMARY: goal is X; files touched: a.rs".into()), None, Vec::new()),
                finish_reason: Some("stop".into()),
                usage: crate::llm::Usage::default(),
            })
        }
    }

    struct TestUi;

    #[async_trait::async_trait]
    impl Ui for TestUi {
        async fn read_user_input(&mut self) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn emit(&mut self, _event: AgentEvent) {}
        async fn authorize(&mut self, _tool: &str, _args: &Value) -> crate::ui::AuthDecision {
            crate::ui::AuthDecision::Allow
        }
    }

    /// A conversation long enough to be over a small budget, wired into a real `Agent` + real
    /// conversation file.
    async fn agent_with_history(
        tag: &str,
        turns: usize,
        chars: usize,
        budget: u64,
    ) -> (Agent, Arc<CountingLlm>, String, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("aox-compact-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = crate::config::Config::dummy();
        // The budget is passed in because it has to be comparable with the real system prompt (the built-in one alone...
        c.agent.context_budget_tokens = budget;
        c.workplace = crate::config::WorkplaceConfig { mode: "fixed".into(), root: Some(dir.to_string_lossy().into()) };
        let rt = Runtime::new(c, dir.clone(), dir.join("plugins")).unwrap();
        let llm = Arc::new(CountingLlm { calls: AtomicUsize::new(0) });
        let mut agent = Agent::new_with(llm.clone(), rt);
        let id = {
            let log = agent.rt().store.create(None, &Default::default()).unwrap();
            log.id().to_string()
        };
        agent.resume(&id).unwrap();
        for i in 0..turns {
            agent.push_message(Message::user(format!("user turn {i} {}", "y".repeat(chars))), None);
            let text = format!("reply {i}");
            agent.push_message(Message::assistant(Some(text), None, Vec::new()), None);
        }
        (agent, llm, id, dir)
    }

    #[tokio::test]
    async fn compaction_shrinks_the_request_but_not_the_conversation() {
        let (mut agent, llm, _id, dir) = agent_with_history("shrink", 20, 3_000, 9_000).await;
        let before_msgs = agent.messages().len();
        let raw_before = std::fs::read_to_string(
            std::fs::read_dir(dir.join(".aox").join("conversations")).unwrap().next().unwrap().unwrap().path(),
        )
        .unwrap();

        let body = prepare(&mut agent, &mut TestUi, &[]).await.expect("over budget ⇒ a compacted view");
        assert!(body.len() < before_msgs, "the body must be shorter than the conversation");
        assert_eq!(agent.messages().len(), before_msgs, "compaction is a projection: memory keeps everything");
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1, "one summary call");

        // The file did not move. One byte of difference would mean the projection leaked into disk.
        let raw_after = std::fs::read_to_string(
            std::fs::read_dir(dir.join(".aox").join("conversations")).unwrap().next().unwrap().unwrap().path(),
        )
        .unwrap();
        assert_eq!(raw_before, raw_after, "compaction must never touch the conversation file");

        // Same messages again ⇒ cache hit, no second summary.
        let again = prepare(&mut agent, &mut TestUi, &[]).await.expect("still over budget");
        assert_eq!(again.len(), body.len());
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1, "the same message list must not be summarized twice");

        // A rollback writes the **full** history back, not the compacted view. This is the trap the
        // whole "projection, not mutation" design exists for.
        let cut = before_msgs - 4;
        agent.truncate_messages(cut);
        let raw = std::fs::read_to_string(
            std::fs::read_dir(dir.join(".aox").join("conversations")).unwrap().next().unwrap().unwrap().path(),
        )
        .unwrap();
        assert!(raw.contains("user turn 0"), "the earliest turn must still be on disk after a rollback");
        assert!(!raw.contains("SUMMARY:"), "the summary is a request-shaping device, never a file row");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Under budget ⇒ `None`, i.e. the loop sends `agent.messages()` untouched. Byte-stability is what
    /// keeps the endpoint's prompt cache warm, so this is the default path for ordinary chats.
    #[tokio::test]
    async fn a_small_conversation_produces_no_view_at_all() {
        let (mut agent, llm, _id, dir) = agent_with_history("small", 1, 200, 1_000_000).await;
        let body = prepare(&mut agent, &mut TestUi, &[]).await;
        assert!(body.is_none(), "a short conversation must go out exactly as it is");
        assert_eq!(llm.calls.load(Ordering::SeqCst), 0, "no summary call, no cost");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
