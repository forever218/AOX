//! A **lightweight checkpoint** for long-horizon tasks: readable, persistable, restorable state.
//!
//! # Why it exists
//!
//! Three things make a model "forget why it is here": context compaction, an interrupted task, and a
//! restart. Compaction folds the early user goal into a summary, an interrupt rollback drops the
//! step in flight, and a restart does not even have the summary. Left with only the last few
//! messages, a model readily **redoes work that is already done**, or breaks a constraint stated
//! long ago ("do not touch the public API").
//!
//! There is no task engine, no DAG, no sub-agent scheduler here — one struct plus one row in the
//! conversation file. It answers four questions: what is the goal, what constrains it, how far did
//! we get, what happens next.
//!
//! # Three design constraints
//!
//! 1. **No new storage.** The checkpoint is written as a `kind:"task"` row in this conversation's
//!    `.jsonl` (the last one wins), the same mechanism as `usage` / `workplace` / `title`: when a
//!    conversation is deleted, trimmed or moved to another backend, the checkpoint goes with it. No
//!    orphan files, and **nothing dropped into the user's project root**.
//! 2. **The model writes only what only the model can know.** Goal, constraints, acceptance
//!    criteria, progress, decisions, open questions — submitted through the `task_state` tool.
//!    `modified_files`, by contrast, is **accumulated by the host alone** from real
//!    `ToolOutput::path` values: a file the model changed is a fact, not a claim; letting it fill
//!    that in would be allowing it to invent evidence.
//! 3. **Short tasks produce no state.** The host never creates a checkpoint on its own; if the model
//!    never calls `task_state`, this is `None` and not a single row is written.
//!
//! # Evidence, and why `done` is derived rather than declared
//!
//! A checkpoint that only records what the model *believes* is half a checkpoint: the moment it
//! says "done", the next session has no way to tell a finished task from an optimistic one. So the
//! host keeps its own ledger (`checks`, written only by `verify`) and [`TaskState::gate`] decides.
//! Two rules do the work:
//!
//! - **Every acceptance criterion needs a check that passed** — "I tested it" is not evidence,
//!   "this file contains that line, hashed at this moment" is.
//! - **Evidence expires.** Every real file write bumps [`TaskState::epoch`], and a check carries the
//!   epoch it ran in. A check older than the last write proves something about a workspace that no
//!   longer exists, so it cannot justify `done`.
//!
//! The model still chooses *what* to check and *which criterion* a check covers. That is fine: the
//! question the gate answers is "did anything real happen", not "was the right thing checked" — the
//! latter is what the user's review is for.
//!
//! # Rules at restore time (more important than "it can be restored")
//!
//! What comes back is a **snapshot from then**, and the files on disk may long since look different.
//! So [`TaskState::context_block`] always ends with "re-check this", and a restore **never** treats
//! any item as a verified fact — it is only "what was believed last time".

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// On-disk format version. **An unknown version reads as "no checkpoint"** (not as a hard error): a
/// state written by an older version, read with a guess, would leave the model believing it is still
/// working on something that ended long ago — far worse than having no state at all.
pub const VERSION: u32 = 1;

/// Per-field length caps. Not about performance but about being **bounded**: a checkpoint enters the
/// context again and again (compaction summaries, restore notes), and an unbounded structure sooner
/// or later becomes a second thing that needs compacting.
pub const GOAL_MAX: usize = 2000;
pub const ITEMS_MAX: usize = 40;
pub const ITEM_MAX: usize = 500;
pub const FILES_MAX: usize = 60;
pub const CHECKS_MAX: usize = 40;
/// A check's detail is a one-liner for the model, not a log — bounded like every other column.
pub const DETAIL_MAX: usize = 300;
/// How much of an acceptance item is echoed beside its evidence. Long enough to be recognisable,
/// short enough that the verification line does not crowd out the rest of the restore block.
pub const CLAIM_SHOW: usize = 48;
/// Character cap for the whole checkpoint once rendered as a context block (truncated by priority).
pub const BLOCK_MAX: usize = 2400;

/// Whether the host considers the task finished.
///
/// `Done` is **derived, never declared**: the model may *ask* for it (`task_state {status: "done"}`),
/// but [`TaskState::gate`] has to pass first, and only the host has the evidence to decide that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Open,
    Done,
}

impl Status {
    /// Used by `skip_serializing_if`: an open task writes no `status` field at all, so a checkpoint
    /// that never used the verification loop serialises exactly as it did before it existed.
    fn is_open(&self) -> bool {
        matches!(self, Status::Open)
    }
}

/// `skip_serializing_if` for counters that are only meaningful once they move.
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// One piece of **host-collected** evidence: a check that really ran, against something really there.
///
/// The model chooses *what* to check and *which acceptance item* it proves; it never writes `ok`,
/// `detail` or `epoch`. That split is the whole point — a field the model can fill in is a field it
/// can fill in wrongly, and "tests pass" is the single claim agents are most optimistic about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    /// Which acceptance item this covers, **1-based** (index into [`TaskState::acceptance`]).
    pub criterion: usize,
    /// The acceptance text **as it read when the check ran** — the host's copy, not the model's
    /// paraphrase of it.
    ///
    /// This is the cheapest defence available against the one thing the gate cannot judge: whether
    /// the check actually proves *that* criterion. The host still does not compare meanings, but
    /// echoing the claim back at the moment of recording — and again in the restore block — makes a
    /// mismatch ("this check proves the file contains hello, the criterion says the last line is
    /// bye") impossible to miss. In a live test the model spotted exactly that mismatch on its own
    /// and had to read the criterion text to do it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub claim: String,
    /// What kind of check ran — one of `crate::tool::verify::KINDS`.
    pub kind: String,
    /// What it ran against (a path, or "the last tool output").
    pub target: String,
    pub ok: bool,
    /// One line for the model: what was found. Bounded by [`DETAIL_MAX`].
    pub detail: String,
    pub at: u64,
    /// The workspace epoch this check ran in (see [`TaskState::epoch`]).
    pub epoch: u64,
}

/// One task checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskState {
    /// Format version, see [`VERSION`].
    #[serde(default)]
    pub version: u32,
    /// Task id (tells "a different task" apart within one conversation). Generated by the host; the
    /// model never touches it.
    #[serde(default)]
    pub task_id: String,
    /// The goal in one sentence. **Required when a checkpoint is created** — one without a goal has
    /// no restore value.
    pub goal: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    /// Acceptance criteria: what counts as done (checkable wording wins, e.g. "cargo test passes").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
    /// What is genuinely finished.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed: Vec<String>,
    /// What is being worked on right now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_progress: Vec<String>,
    /// Important decisions and why ("chose A because B").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    /// Files this task really changed. **Written by the host only** (see constraint 2 in the module
    /// docs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modified_files: Vec<String>,
    /// Host-collected evidence, one entry per `verify` call (see [`Check`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    /// How many times the host has written a file during this task.
    ///
    /// This is what makes evidence **expire**: a check is proof about the workspace *at the moment it
    /// ran*, and a later write can invalidate it. Rather than trying to guess which checks a given
    /// file touches (a path mapping that would be wrong the first time a check reads two files), any
    /// real write bumps this counter and every older check becomes stale. Conservative on purpose: a
    /// re-run costs one tool call, a wrong "done" costs the user's trust.
    ///
    /// ⚠️ It only counts writes the host **knows about** — the ones a tool reports through
    /// `ToolOutput::path`. A plugin that writes a file without filling that field can slip past, so
    /// this is a strong guard against the common case, not a proof against a careless plugin.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub epoch: u64,
    /// `open` while work continues; `done` only once [`TaskState::gate`] has passed.
    ///
    /// Not written while `open`: a checkpoint that never verified anything must produce the **same
    /// row** it did before this feature existed (the zero-drift rule this repo holds every stage to).
    #[serde(default, skip_serializing_if = "Status::is_open")]
    pub status: Status,
    /// Open questions / known obstacles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
    /// What happens next.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
    /// Unix seconds.
    #[serde(default)]
    pub last_updated: u64,
}

impl TaskState {
    /// Start a new checkpoint. `task_id` carries a fragment of the goal's fingerprint purely so two
    /// tasks created within the same second cannot collide.
    pub fn new(goal: &str) -> Self {
        let goal = clamp(goal.trim(), GOAL_MAX);
        let fp = crate::tool::fingerprint::of_bytes(goal.as_bytes());
        Self {
            version: VERSION,
            task_id: format!("t{}-{}", crate::history::unix_now(), &fp[..6.min(fp.len())]),
            goal,
            constraints: Vec::new(),
            acceptance: Vec::new(),
            completed: Vec::new(),
            in_progress: Vec::new(),
            decisions: Vec::new(),
            modified_files: Vec::new(),
            checks: Vec::new(),
            epoch: 0,
            status: Status::Open,
            unresolved: Vec::new(),
            next_steps: Vec::new(),
            last_updated: crate::history::unix_now(),
        }
    }

    /// Parse the `task` object of a `kind:"task"` row.
    ///
    /// Anything unrecognised returns `None`: a missing field, a version mismatch, an empty `goal`.
    /// Every caller **degrades to "no checkpoint"** and keeps going — a broken checkpoint must never
    /// block a conversation.
    pub fn from_value(v: &Value) -> Option<Self> {
        let st: Self = serde_json::from_value(v.clone()).ok()?;
        if st.version != VERSION || st.goal.trim().is_empty() {
            return None;
        }
        Some(st)
    }

    /// Render as one conversation record (the `task` value of a `kind:"task"` row).
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Record a file that was **really modified** (called by the host after a tool returns).
    ///
    /// Deduplicated by path string; past [`FILES_MAX`] nothing more is appended — keeping the earliest
    /// beats keeping the latest: in a long-running task, "which files it started from" is usually the
    /// clue restore needs most.
    pub fn note_modified(&mut self, path: &str) {
        if path.trim().is_empty() {
            return;
        }
        // Bumped **before** the dedupe check: writing the same file a second time still changes the
        // workspace, so it still invalidates evidence. Only the file *list* dedupes, not the fact.
        self.epoch = self.epoch.wrapping_add(1);
        // The workspace moved on, so a previous "done" no longer holds. Reopening here (rather than
        // in `gate`) keeps the stored status honest even if nobody ever asks for the gate again.
        self.status = Status::Open;
        self.last_updated = crate::history::unix_now();
        if self.modified_files.iter().any(|p| p == path) {
            return;
        }
        if self.modified_files.len() < FILES_MAX {
            self.modified_files.push(path.to_string());
        }
    }

    /// Record one piece of evidence. Called by the host only (see `crate::tool::verify`).
    ///
    /// Past [`CHECKS_MAX`] the oldest entry is dropped: for a bounded checkpoint, "the most recent
    /// evidence" is the only part that can still justify a `done`, so it is the part worth keeping.
    pub fn record(&mut self, check: Check) {
        if self.checks.len() >= CHECKS_MAX {
            self.checks.remove(0);
        }
        self.checks.push(check);
        self.last_updated = crate::history::unix_now();
    }

    /// How many acceptance criteria are currently covered by a **passing, non-stale** check.
    ///
    /// Used in the tool's reply ("evidence 2 of 3") so the model can see how far it is from `done`
    /// without guessing — the gate uses the same rule, one criterion at a time.
    pub fn covered(&self) -> usize {
        (1..=self.acceptance.len())
            .filter(|n| {
                self.checks
                    .iter()
                    .rev()
                    .find(|c| c.criterion == *n)
                    .is_some_and(|c| c.ok && c.epoch >= self.epoch)
            })
            .count()
    }

    /// Can this task be called **done**? `Ok(())` when every acceptance criterion is covered by a
    /// passing, *current* check; otherwise the list of reasons, each naming a fix.
    ///
    /// Four rules, in the order the model should fix them:
    /// 1. there must be acceptance criteria (nothing to verify otherwise);
    /// 2. something must actually be reported as completed;
    /// 3. every criterion needs at least one check (the **latest** one decides — a fixed check
    ///    supersedes an earlier failure);
    /// 4. that check must have passed and must not predate the last workspace write.
    pub fn gate(&self) -> Result<(), Vec<String>> {
        let mut why = Vec::new();
        if self.acceptance.is_empty() {
            why.push(crate::i18n::t(
                "there are no acceptance criteria yet: write them first (what counts as done, in checkable terms)",
            ).to_string());
        }
        if self.completed.is_empty() {
            why.push(crate::i18n::t("nothing is reported as completed yet").to_string());
        }
        for (i, item) in self.acceptance.iter().enumerate() {
            let n = i + 1;
            // The criterion text travels with every refusal: "acceptance #2" alone tells the model
            // nothing about *what* it failed to prove, and that ambiguity is exactly how a check of
            // the wrong thing slips through.
            let claim = clamp(item, CLAIM_SHOW);
            match self.checks.iter().rev().find(|c| c.criterion == n) {
                None => why.push(crate::tf!(
                    "acceptance #{} \"{}\" has never been checked — run verify with criterion: {}",
                    n,
                    claim,
                    n
                )),
                Some(c) if !c.ok => why.push(crate::tf!(
                    "the latest check for acceptance #{} \"{}\" FAILED: {} ({})",
                    n,
                    claim,
                    c.detail,
                    c.target
                )),
                Some(c) if c.epoch < self.epoch => why.push(crate::tf!(
                    "the check for acceptance #{} \"{}\" is stale: the workspace was written after it ran — re-run it",
                    n,
                    claim
                )),
                Some(_) => {}
            }
        }
        if why.is_empty() {
            Ok(())
        } else {
            Err(why)
        }
    }

    /// The text injected into the context on restore.
    ///
    /// Ordered by what restoring the work needs first: goal -> constraints -> progress -> unresolved
    /// -> next steps -> files. That closing "re-check" line is **mandatory**: a checkpoint records
    /// the state from then, not the facts of now.
    ///
    /// ⚠️ Truncation applies to the **body** only; that closing warning is never cut: when the body is
    /// too long it is trimmed first and the warning is kept — the other order (cut the tail) would
    /// drop the only warning at exactly the moment it matters.
    pub fn context_block(&self) -> String {
        let caveat = crate::i18n::t(
            "This checkpoint describes the task as it stood when it was written, NOT the current state of the project: re-read any file before you change it, and re-run any check before you call it done. If what you find disagrees with this list, the file wins — then update the checkpoint.",
        );
        let mut out = crate::tf!("[task checkpoint · {}]", self.task_id);
        out.push_str(&crate::tf!("\ngoal: {}", self.goal));
        push_list(&mut out, crate::i18n::t("constraints"), &self.constraints, 8);
        push_list(&mut out, crate::i18n::t("acceptance criteria"), &self.acceptance, 8);
        self.push_verification(&mut out);
        push_list(&mut out, crate::i18n::t("done"), &self.completed, 8);
        push_list(&mut out, crate::i18n::t("in progress"), &self.in_progress, 6);
        push_list(&mut out, crate::i18n::t("decisions"), &self.decisions, 6);
        push_list(&mut out, crate::i18n::t("unresolved"), &self.unresolved, 6);
        push_list(&mut out, crate::i18n::t("next steps"), &self.next_steps, 6);
        push_list(&mut out, crate::i18n::t("files touched so far"), &self.modified_files, 10);
        let body = clamp(&out, BLOCK_MAX.saturating_sub(caveat.chars().count() + 1));
        format!("{body}\n{caveat}")
    }

    /// One `verification: #1 ok (file_hash) · #2 stale (output_contains)` line.
    ///
    /// Only emitted **when there is evidence at all**: a checkpoint that never called `verify` must
    /// render byte-identically to the version before this feature existed (the zero-drift rule that
    /// every stage here is held to).
    fn push_verification(&self, out: &mut String) {
        if self.checks.is_empty() {
            return;
        }
        let mut parts: Vec<String> = Vec::with_capacity(self.acceptance.len());
        for (i, item) in self.acceptance.iter().enumerate() {
            let n = i + 1;
            let claim = clamp(item, CLAIM_SHOW);
            let label = match self.checks.iter().rev().find(|c| c.criterion == n) {
                None => crate::i18n::t("unchecked").to_string(),
                Some(c) if !c.ok => crate::tf!("failed ({})", c.kind),
                Some(c) if c.epoch < self.epoch => crate::tf!("stale ({})", c.kind),
                Some(c) => crate::tf!("ok ({})", c.kind),
            };
            parts.push(format!("#{n} \"{claim}\" {label}"));
        }
        out.push_str(&crate::tf!("\nverification: {}", parts.join(" · ")));
        if self.status == Status::Done {
            out.push_str(crate::i18n::t(" — accepted by the host"));
        }
    }
}

/// One `- label: a · b · c` line (past `n` items it truncates and says how many are left).
fn push_list(out: &mut String, label: &str, items: &[String], n: usize) {
    if items.is_empty() {
        return;
    }
    let shown = items.iter().take(n).map(|s| s.as_str()).collect::<Vec<_>>().join(" · ");
    out.push_str(&crate::tf!("\n{}: {}", label, shown));
    if items.len() > n {
        out.push_str(&crate::tf!(" (+{} more)", items.len() - n));
    }
}

/// Truncate by **characters** (not bytes — never cut a multi-byte character in half).
pub(crate) fn clamp(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// One update submitted by the model: **only the fields present change**, everything else is kept.
///
/// Arrays **replace wholesale** rather than append: the model submits "this column as it stands now",
/// so it can fix its own mistake, and repeated calls cannot grow without bound. This has to be stated
/// in the tool description, or the model assumes it is appending.
#[derive(Debug, Default, Clone)]
pub struct Patch {
    pub goal: Option<String>,
    pub constraints: Option<Vec<String>>,
    pub acceptance: Option<Vec<String>>,
    pub completed: Option<Vec<String>>,
    pub in_progress: Option<Vec<String>>,
    pub decisions: Option<Vec<String>>,
    pub unresolved: Option<Vec<String>>,
    pub next_steps: Option<Vec<String>>,
    /// `done` asks the host to accept the task; `open` reopens it (new work was found).
    pub status: Option<Status>,
    /// Discard the old state and start a new task from this batch of fields.
    pub reset: bool,
}

/// The keys allowed in the tool arguments. A **whitelist**, because with lenient "ignore unknown
/// fields" parsing a typo (`next_step`) fails silently — the model believes it recorded the next step,
/// nothing was recorded, and it only shows up at restore time.
const FIELDS: [&str; 10] = [
    "goal",
    "constraints",
    "acceptance",
    "completed",
    "in_progress",
    "decisions",
    "unresolved",
    "next_steps",
    "status",
    "reset",
];

impl Patch {
    /// Parse from the tool arguments. The errors are written for the model (it has to fix them), so
    /// each one names **the key** at fault.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let Some(obj) = v.as_object() else {
            return Err(crate::i18n::t("task_state expects an object of fields").to_string());
        };
        // ⚠️ `tf!` only understands positional `{}` (see `src/i18n.rs`), so the field list is
        // formatted once here instead of interpolated inline — a `{:?}` in the template would be
        // left as literal text and silently shift every later argument.
        let valid = FIELDS.join(", ");
        for k in obj.keys() {
            if !FIELDS.contains(&k.as_str()) {
                return Err(if k == "modified_files" {
                    crate::tf!(
                        "modified_files is maintained by the host from actual tool results and cannot be set here — it is evidence, not a claim. Accepted fields: {}.",
                        valid
                    )
                } else if k == "verification" || k == "verification_status" {
                    crate::tf!(
                        "verification status is derived by the host from real checks, not declared. Accepted fields: {}.",
                        valid
                    )
                } else {
                    crate::tf!(
                        "unknown field \"{}\". Accepted fields: {}. (Arrays are replaced wholesale, not appended.)",
                        k,
                        valid
                    )
                });
            }
        }
        let list = |k: &str| -> Result<Option<Vec<String>>, String> {
            let Some(v) = obj.get(k) else { return Ok(None) };
            let Some(arr) = v.as_array() else {
                return Err(crate::tf!("{} must be an array of strings", k));
            };
            let mut out = Vec::with_capacity(arr.len().min(ITEMS_MAX));
            for it in arr.iter().take(ITEMS_MAX) {
                let Some(s) = it.as_str() else {
                    return Err(crate::tf!("{} must contain only strings (found {})", k, it));
                };
                let s = s.trim();
                if s.is_empty() {
                    continue; // an empty string carries nothing; drop it, it is not an error
                }
                let s = clamp(s, ITEM_MAX);
                if !out.iter().any(|e: &String| *e == s) {
                    out.push(s); // the same entry twice is noise, not information
                }
            }
            Ok(Some(out))
        };
        let goal = match obj.get("goal") {
            None => None,
            Some(Value::String(s)) if !s.trim().is_empty() => Some(clamp(s.trim(), GOAL_MAX)),
            Some(_) => return Err(crate::i18n::t("goal must be a non-empty string").to_string()),
        };
        let status = match obj.get("status") {
            None => None,
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "done" => Some(Status::Done),
                "open" => Some(Status::Open),
                other => {
                    return Err(crate::tf!(
                        "status must be \"done\" or \"open\" (got \"{}\")",
                        other
                    ))
                }
            },
            Some(_) => return Err(crate::i18n::t("status must be the string \"done\" or \"open\"").to_string()),
        };
        Ok(Self {
            goal,
            constraints: list("constraints")?,
            acceptance: list("acceptance")?,
            completed: list("completed")?,
            in_progress: list("in_progress")?,
            decisions: list("decisions")?,
            unresolved: list("unresolved")?,
            next_steps: list("next_steps")?,
            status,
            reset: obj.get("reset").and_then(|v| v.as_bool()).unwrap_or(false),
        })
    }

    /// The update gives nothing besides `reset` — there is nothing to write, and the call is
    /// probably a no-op.
    fn is_empty(&self) -> bool {
        self.goal.is_none()
            && self.constraints.is_none()
            && self.acceptance.is_none()
            && self.completed.is_none()
            && self.in_progress.is_none()
            && self.decisions.is_none()
            && self.unresolved.is_none()
            && self.next_steps.is_none()
            && self.status.is_none()
    }
}

/// Merge one update into the current state, returning the merged state.
///
/// When `current` is `None` (no checkpoint yet) the update must carry `goal` — a state without a
/// goal cannot be restored, and storing one only confuses the next restore.
pub fn apply(current: Option<TaskState>, patch: &Patch) -> Result<TaskState, String> {
    if patch.is_empty() && !patch.reset {
        return Err(crate::i18n::t(
            "nothing to update: pass at least one of goal / constraints / acceptance / completed / in_progress / decisions / unresolved / next_steps",
        )
        .to_string());
    }
    // `reset` means "start from zero": the old state is dropped entirely, so the base is always
    // `None`.
    let base = if patch.reset { None } else { current };
    let mut st = match base {
        Some(s) => s,
        None => {
            let goal = patch.goal.as_deref().ok_or_else(|| {
                crate::i18n::t("the first update of a task must include goal (one sentence saying what is being achieved)")
                    .to_string()
            })?;
            TaskState::new(goal)
        }
    };
    // A goal overwrites (on `reset` it was already used to create the state above; this is the
    // "change the goal" path).
    if let Some(g) = &patch.goal {
        st.goal = g.clone();
        // The goal changes but `task_id` does not: it identifies *this task*, not that sentence.
    }
    if let Some(v) = &patch.constraints {
        st.constraints = v.clone();
    }
    if let Some(v) = &patch.acceptance {
        // Checks are bound to acceptance **indices**, so replacing the list re-points every index.
        // Keeping the old evidence would attach it to criteria it never proved — strictly worse than
        // losing it, because the gate would then pass on a coincidence.
        st.checks.clear();
        st.acceptance = v.clone();
    }
    if let Some(v) = &patch.completed {
        st.completed = v.clone();
    }
    if let Some(v) = &patch.in_progress {
        st.in_progress = v.clone();
    }
    if let Some(v) = &patch.decisions {
        st.decisions = v.clone();
    }
    if let Some(v) = &patch.unresolved {
        st.unresolved = v.clone();
    }
    if let Some(v) = &patch.next_steps {
        st.next_steps = v.clone();
    }
    st.version = VERSION;
    // `done` is a **request**, not an assignment: the gate looks at evidence the host collected, and
    // refuses with a list of what is missing. Reopening needs no evidence — that direction is always
    // safe, and it is how the model corrects an over-eager `done`.
    match patch.status {
        Some(Status::Done) => match st.gate() {
            Ok(()) => st.status = Status::Done,
            Err(why) => {
                let mut msg = crate::i18n::t(
                    "cannot mark this task done — the host has no evidence for it yet:",
                )
                .to_string();
                for line in why {
                    msg.push_str("\n  - ");
                    msg.push_str(&line);
                }
                return Err(msg);
            }
        },
        Some(Status::Open) => st.status = Status::Open,
        None => {}
    }
    st.last_updated = crate::history::unix_now();
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn patch(v: Value) -> Patch {
        Patch::from_json(&v).expect("the patch should parse")
    }

    #[test]
    fn the_first_update_creates_a_checkpoint_and_needs_a_goal() {
        let st = apply(None, &patch(json!({"goal": "add edit_file", "constraints": ["keep it Rust"]})))
            .ok()
            .unwrap();
        assert_eq!(st.goal, "add edit_file");
        assert_eq!(st.constraints, vec!["keep it Rust"]);
        assert!(st.task_id.starts_with('t'));
        assert_eq!(st.version, VERSION);

        let e = apply(None, &patch(json!({"next_steps": ["do it"]}))).err().unwrap();
        assert!(e.contains("goal"), "the error must name the missing field: {e}");
    }

    /// An update is a **patch**: fields not mentioned are left as they are. This is the concrete form
    /// of "updating must not lose unrelated state".
    #[test]
    fn an_update_only_touches_the_fields_it_names() {
        let a = apply(None, &patch(json!({
            "goal": "g",
            "constraints": ["c1"],
            "completed": ["step 1"],
        })))
        .ok()
        .unwrap();
        let b = apply(Some(a.clone()), &patch(json!({"completed": ["step 1", "step 2"]})))
            .ok()
            .unwrap();
        assert_eq!(b.constraints, vec!["c1"], "an untouched field must survive");
        assert_eq!(b.goal, "g");
        assert_eq!(b.completed, vec!["step 1", "step 2"]);
    }

    /// Arrays replace; an empty array clears the column (the model's own way of correcting itself).
    #[test]
    fn arrays_are_replaced_not_appended() {
        let a = apply(None, &patch(json!({"goal": "g", "completed": ["1", "2"]}))).ok().unwrap();
        let b = apply(Some(a), &patch(json!({"completed": []}))).ok().unwrap();
        assert!(b.completed.is_empty());
    }

    #[test]
    fn reset_starts_a_new_task_and_still_needs_a_goal() {
        let a = apply(None, &patch(json!({"goal": "old", "completed": ["x"]}))).ok().unwrap();
        let b = apply(Some(a), &patch(json!({"reset": true, "goal": "new"}))).ok().unwrap();
        assert_eq!(b.goal, "new");
        assert!(b.completed.is_empty(), "reset must not carry the old progress over");
        let e = apply(None, &patch(json!({"reset": true}))).err().unwrap();
        assert!(e.contains("goal"), "{e}");
    }

    /// Why the whitelist matters: a misspelled key must error, not silently drop a field the model
    /// believes it wrote.
    #[test]
    fn unknown_fields_are_rejected_with_the_list_of_valid_ones() {
        let e = Patch::from_json(&json!({"next_step": ["typo"]})).err().unwrap();
        assert!(e.contains("next_step"), "{e}");
        assert!(e.contains("next_steps"), "the error should show the valid set: {e}");
    }

    /// The model **cannot** declare which files it changed — that is evidence the host takes from
    /// real tool results.
    #[test]
    fn the_model_cannot_write_its_own_evidence() {
        for key in ["modified_files", "verification", "verification_status"] {
            let e = Patch::from_json(&json!({key: ["x"]})).err().unwrap();
            assert!(e.contains("host"), "{key}: {e}");
        }
    }

    #[test]
    fn empty_and_duplicate_items_are_dropped_and_lengths_are_capped() {
        let p = patch(json!({"goal": "g", "constraints": ["a", "  ", "a", "b"]}));
        assert_eq!(p.constraints.unwrap(), vec!["a", "b"]);
        let long = "x".repeat(ITEM_MAX + 50);
        let p = patch(json!({"goal": "g", "constraints": [long]}));
        assert_eq!(p.constraints.unwrap()[0].chars().count(), ITEM_MAX + 1); // includes the … marker
    }

    #[test]
    fn a_checkpoint_round_trips_through_json() {
        let st = apply(
            None,
            &patch(json!({
                "goal": "g",
                "constraints": ["c"],
                "acceptance": ["a"],
                "completed": ["d"],
                "in_progress": ["i"],
                "decisions": ["because"],
                "unresolved": ["u"],
                "next_steps": ["n"],
            })),
        )
        .ok()
        .unwrap();
        let v = st.to_value();
        let back = TaskState::from_value(&v).expect("must parse back");
        assert_eq!(back, st);
        // Empty fields are not written: the row is smaller and only populated columns are readable
        assert!(v.get("unresolved").is_some());
        let minimal = TaskState::new("only a goal");
        assert!(minimal.to_value().get("constraints").is_none());
        // Zero drift in the row too: an unverified, unfinished task writes neither `status` nor `epoch`,
        // so its line is byte-identical to what older builds wrote.
        assert!(minimal.to_value().get("status").is_none());
        assert!(minimal.to_value().get("epoch").is_none());
    }

    /// Bad data always degrades to "no checkpoint": version mismatch, missing goal, wrong shape.
    #[test]
    fn unreadable_or_foreign_rows_degrade_to_none() {
        assert!(TaskState::from_value(&json!({"version": 99, "goal": "g"})).is_none());
        assert!(TaskState::from_value(&json!({"version": 1, "goal": "  "})).is_none());
        assert!(TaskState::from_value(&json!({"kind": "usage", "round": 1})).is_none());
        assert!(TaskState::from_value(&Value::Null).is_none());
    }

    #[test]
    fn modified_files_are_deduped_and_bounded() {
        let mut st = TaskState::new("g");
        st.note_modified("a.rs");
        st.note_modified("a.rs");
        assert_eq!(st.modified_files, vec!["a.rs"]);
        for i in 0..(FILES_MAX + 10) {
            st.note_modified(&format!("f{i}.rs"));
        }
        assert_eq!(st.modified_files.len(), FILES_MAX);
        assert_eq!(st.modified_files[0], "a.rs", "the earliest files are the ones worth keeping");
    }

    /// The restore block must carry "re-check" and stay bounded — it re-enters the context on every
    /// compaction and every restore.
    #[test]
    fn the_context_block_is_bounded_and_says_to_recheck() {
        let mut st = TaskState::new("do the thing");
        for i in 0..ITEMS_MAX {
            st.completed.push(format!("step {i} {}", "x".repeat(200)));
        }
        st.next_steps.push("run the tests".into());
        let b = st.context_block();
        assert!(b.contains("do the thing"), "{b}");
        assert!(b.contains("next steps"), "{b}");
        assert!(b.chars().count() <= BLOCK_MAX + 1, "block must stay bounded: {}", b.chars().count());
        assert!(b.contains("re-read") || b.contains("re-run"), "must tell the model to re-check: {b}");
    }

    // ───────── Evidence and the `done` gate ─────────

    fn check(n: usize, ok: bool, epoch: u64) -> Check {
        Check {
            criterion: n,
            claim: format!("criterion {n}"),
            kind: "path_exists".into(),
            target: format!("target-{n}"),
            ok,
            detail: if ok { "found".into() } else { "missing".into() },
            at: 0,
            epoch,
        }
    }

    fn with_acceptance(items: &[&str]) -> TaskState {
        let mut st = TaskState::new("g");
        st.acceptance = items.iter().map(|s| s.to_string()).collect();
        st.completed = vec!["something".into()];
        st
    }

    /// The rule that makes "done" mean something: one passing check per acceptance item, no exceptions.
    #[test]
    fn done_needs_a_current_passing_check_for_every_criterion() {
        let mut st = with_acceptance(&["file exists", "tests pass"]);
        let why = st.gate().err().expect("no evidence at all must not pass");
        assert_eq!(why.len(), 2, "both criteria should be reported at once: {why:?}");
        assert!(why[0].contains("#1") && why[1].contains("#2"), "{why:?}");

        st.record(check(1, true, 0));
        assert!(st.gate().is_err(), "one criterion is still uncovered");
        st.record(check(2, true, 0));
        assert!(st.gate().is_ok(), "both covered, nothing stale");
    }

    /// A failure is not permanent: the **latest** check for a criterion decides. A model that fixed
    /// the cause must be able to move on without deleting history.
    #[test]
    fn a_later_pass_supersedes_an_earlier_failure() {
        let mut st = with_acceptance(&["a"]);
        st.record(check(1, false, 0));
        let why = st.gate().err().unwrap();
        assert!(why[0].contains("FAILED"), "{why:?}");

        st.record(check(1, true, 0));
        assert!(st.gate().is_ok(), "the newest check wins");
    }

    /// The whole reason `epoch` exists: proof about a workspace that has since changed proves nothing.
    #[test]
    fn evidence_goes_stale_as_soon_as_a_file_is_written() {
        let mut st = with_acceptance(&["a"]);
        st.record(check(1, true, 0));
        assert!(st.gate().is_ok());

        st.note_modified("a.rs");
        let why = st.gate().err().expect("a write after the check must invalidate it");
        assert!(why[0].contains("stale"), "{why:?}");
        assert_eq!(st.status, Status::Open, "the stored status must stop claiming done");

        st.record(check(1, true, st.epoch));
        assert!(st.gate().is_ok(), "re-running the check at the current epoch fixes it");
    }

    /// Checks are bound to acceptance **indices**. Replacing the list re-points them, so keeping the
    /// old evidence would let the gate pass on a coincidence.
    #[test]
    fn replacing_acceptance_drops_the_evidence_bound_to_the_old_list() {
        let mut st = with_acceptance(&["a"]);
        st.record(check(1, true, 0));
        assert!(st.gate().is_ok());

        let next = apply(Some(st), &patch(json!({"acceptance": ["totally different"]}))).ok().unwrap();
        assert!(next.checks.is_empty(), "evidence about the old criterion must not survive");
        assert!(next.gate().is_err());
    }

    /// Asking for `done` is a request the gate can refuse — and a refusal must write nothing.
    #[test]
    fn asking_for_done_without_evidence_is_refused() {
        let base = apply(None, &patch(json!({"goal": "g", "acceptance": ["a"], "completed": ["x"]}))).ok().unwrap();
        let e = apply(Some(base.clone()), &patch(json!({"status": "done"}))).err().unwrap();
        assert!(e.contains("evidence") || e.contains("check"), "{e}");
        assert_eq!(base.status, Status::Open, "the base state is what the caller still holds");
    }

    /// Zero drift: a checkpoint that never used `verify` must render exactly as it did before this
    /// feature existed. The `verification:` line is the only new output, and only when there is one.
    #[test]
    fn a_checkpoint_without_evidence_renders_unchanged() {
        let st = with_acceptance(&["a"]);
        let b = st.context_block();
        assert!(!b.contains("verification"), "no evidence, no verification line: {b}");

        let mut st = st;
        st.record(check(1, true, 0));
        let b = st.context_block();
        assert!(b.contains("verification: #1 \"a\" ok"), "the criterion text must travel with the evidence: {b}");
        assert!(!b.contains("accepted by the host"), "only a host-accepted task says so: {b}");
    }

    /// The status is visible at restore time — "the host accepted this evidence" is exactly what a
    /// later session needs to know before redoing the work.
    #[test]
    fn an_accepted_task_says_so_in_the_restore_block() {
        let mut st = with_acceptance(&["a"]);
        st.record(check(1, true, 0));
        let done = apply(Some(st), &patch(json!({"status": "done"}))).ok().unwrap();
        assert_eq!(done.status, Status::Done);
        assert!(done.context_block().contains("accepted by the host"), "{}", done.context_block());

        // And it round-trips through the row, so a resume sees the same thing.
        let back = TaskState::from_value(&done.to_value()).unwrap();
        assert_eq!(back.status, Status::Done);
        assert_eq!(back.checks.len(), 1);
        assert_eq!(back.epoch, done.epoch);
    }
}
