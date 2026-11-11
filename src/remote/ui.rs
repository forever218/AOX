//! The `Ui` implementation that speaks the wire instead of the terminal.
//! Same trait the CLI and GUI implement, so `Agent::run_turn` is unchanged: every event the host would have...
//! Approvals go over the wire in both directions: `approval_req` out, `approval_decide` back in through the read...

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use aoxchat_proto::{
    now_ms, ApprovalReq, ApprovalStatus, ApprovalView, Body, Frame, StepState, StepView,
    TaskStatus, TaskUpdate, TaskView, UsageView,
};

use crate::llm::Usage;
use crate::ui::{AgentEvent, AuthDecision, Ui};

use super::approvals::Approvals;

/// Identity of one execution, needed for the task snapshot but not derivable by RemoteUi itself.
pub struct TaskMeta {
    pub agent_id: String,
    pub agent_handle: String,
    pub owner_id: String,
    pub prompt: String,
    /// The message that triggered this turn, so the phone can attach its running indicator in the right place.
    pub msg_id: String,
}

/// How long a phone has to answer before the tool is refused. Long enough to read a command,
/// short enough that a desk-side turn does not hang forever on a phone left in another room.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// An artifact already copied into the staging area.
pub struct Staged {
    /// Original path, used only for dedup since the original may already be gone.
    pub src: std::path::PathBuf,
    /// Upload name; staged filenames get a numeric prefix to avoid same-name collisions.
    pub name: String,
    /// The staged copy, the only file read during upload.
    pub path: std::path::PathBuf,
}

pub struct RemoteUi {
    out: mpsc::UnboundedSender<Frame>,
    room: String,
    task_id: String,
    step: u32,
    /// The step a `ToolResult` most likely closes (tools are called one at a time).
    last: Option<(u32, String)>,
    final_text: String,
    approvals: Arc<Approvals>,
    /// Tools the person allowed for the rest of this task; scoped per turn by construction.
    allowed_for_task: HashSet<String>,
    /// This turn's files, deduped and staged on announce, uploaded after the turn so a deleted original still ships.
    artifacts: Vec<Staged>,
    meta: TaskMeta,
    steps: Vec<StepView>,
    started_at: i64,
    /// What was fetched from the room this turn, recorded in the snapshot for the phone to verify.
    digest: Option<String>,
    /// Token total for this turn; previously always zero because the host never filled it in.
    usage: UsageView,
    /// Artifacts of this turn (views available only after upload), written to the final snapshot for the task card.
    made: Vec<aoxchat_proto::ArtifactView>,
}

impl RemoteUi {
    pub fn new(
        out: mpsc::UnboundedSender<Frame>,
        room: impl Into<String>,
        approvals: Arc<Approvals>,
        meta: TaskMeta,
    ) -> Self {
        Self {
            out,
            room: room.into(),
            task_id: format!("t{}", now_ms()),
            step: 0,
            last: None,
            final_text: String::new(),
            approvals,
            allowed_for_task: HashSet::new(),
            artifacts: Vec::new(),
            meta,
            steps: Vec::new(),
            started_at: now_ms(),
            digest: None,
            usage: UsageView::default(),
            made: Vec::new(),
        }
    }

    /// Record produced artifacts in the snapshot; upload precedes finalization, so views exist for the task card.
    pub fn set_artifacts(&mut self, views: Vec<aoxchat_proto::ArtifactView>) {
        self.made = views;
    }

    /// The final snapshot of this execution, written into room history.
    pub fn snapshot(&self, status: TaskStatus, error: Option<String>) -> TaskView {
        TaskView {
            id: self.task_id.clone(),
            agent_id: self.meta.agent_id.clone(),
            agent_handle: self.meta.agent_handle.clone(),
            created_by: self.meta.owner_id.clone(),
            prompt: self.meta.prompt.clone(),
            source_msg_id: Some(self.meta.msg_id.clone()),
            status,
            steps: self.steps.clone(),
            usage: self.usage,
            context_digest: self.digest.clone(),
            error,
            artifacts: self.made.clone(),
            started_at: Some(self.started_at),
            ended_at: Some(now_ms()),
        }
    }

    pub fn take_artifacts(&mut self) -> Vec<Staged> {
        std::mem::take(&mut self.artifacts)
    }

    /// Staging directory per turn, named by the unique task id also used to route cancellation.
    fn stage_dir(&self) -> std::path::PathBuf {
        std::env::temp_dir().join("aox-artifacts").join(&self.task_id)
    }

    /// Copy an artifact into staging; fall back to the original path if the copy fails, since it still exists.
    fn stage(&mut self, src: &std::path::Path) -> Staged {
        let name = src
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "artifact".to_string());
        let dir = self.stage_dir();
        let dest = dir.join(format!("{}-{name}", self.artifacts.len()));
        let path = match std::fs::create_dir_all(&dir).and_then(|()| std::fs::copy(src, &dest)) {
            Ok(_) => dest,
            Err(e) => {
                eprintln!("could not stage the artifact ({}): {e}; reading the original directly instead", src.display());
                src.to_path_buf()
            }
        };
        Staged { src: src.to_path_buf(), name, path }
    }

    /// Task id, available before the run starts because cancellation is routed by it.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn take_final_text(&mut self) -> String {
        std::mem::take(&mut self.final_text)
    }

    fn send(&self, body: Body) {
        let frame = Frame::in_room(
            Frame::new(super::frame_id("h"), now_ms(), body),
            self.room.clone(),
        );
        // A closed channel means the socket is gone; there is nobody left to tell.
        let _ = self.out.send(frame);
    }

    /// A transient progress line pushed to the phone task card, not persisted since TaskUpdate is ephemeral.
    pub fn note(&self, text: impl Into<String>) {
        self.send(Body::TaskUpdate(TaskUpdate {
            task_id: self.task_id.clone(),
            agent_id: self.meta.agent_id.clone(),
            source_msg_id: Some(self.meta.msg_id.clone()),
            status: None,
            step: None,
            usage: None,
            note: Some(text.into()),
            context_digest: None,
        }));
    }

    /// Fill context_digest with what the model saw: push a frame now and keep one for the final snapshot.
    pub fn set_digest(&mut self, digest: String) {
        self.digest = Some(digest.clone());
        self.send(Body::TaskUpdate(TaskUpdate {
            task_id: self.task_id.clone(),
            agent_id: self.meta.agent_id.clone(),
            source_msg_id: Some(self.meta.msg_id.clone()),
            status: None,
            step: None,
            usage: None,
            note: None,
            context_digest: Some(digest),
        }));
    }

    fn update(&self, step: StepView, status: Option<TaskStatus>) {
        self.send(Body::TaskUpdate(TaskUpdate {
            task_id: self.task_id.clone(),
            agent_id: self.meta.agent_id.clone(),
            source_msg_id: Some(self.meta.msg_id.clone()),
            status,
            step: Some(step),
            usage: None,
            note: None,
            context_digest: None,
        }));
    }
}

/// Staging is removed at normal end, or by Drop on cancel/error; a cancelled turn's files should not be sent.
impl Drop for RemoteUi {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.stage_dir());
    }
}

#[async_trait]
impl Ui for RemoteUi {
    /// `run_turn` receives the instruction as an argument; this is only for a front end that
    /// keeps a REPL. The bridge has no REPL, so "no more input" is the honest answer.
    async fn read_user_input(&mut self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    fn emit(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::ToolCall { name, args } => {
                println!("  · calling {name}");
                self.step += 1;
                let purpose = args
                    .get("purpose")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                self.last = Some((self.step, name.clone()));
                let sv = StepView {
                    idx: self.step,
                    tool: name,
                    purpose,
                    state: StepState::Running,
                    ms: None,
                    diff: None,
                    preview: None,
                };
                self.steps.push(sv.clone());
                self.update(sv, Some(TaskStatus::Running));
            }
            AgentEvent::ToolResult { name, is_error, preview, diff, path, .. } => {
                if let Some(p) = path {
                    // Accept only existing, not-yet-seen files and copy them at once, since the turn may delete them later.
                    if p.is_file() {
                        match self.artifacts.iter().position(|x| x.src == p) {
                            // If the same path is announced again, replace the staged copy so the final on-disk version is what gets sent.
                            Some(i) => {
                                let dest = self.artifacts[i].path.clone();
                                let _ = std::fs::copy(&p, &dest);
                            }
                            None => {
                                let st = self.stage(&p);
                                self.artifacts.push(st);
                            }
                        }
                    }
                }
                let idx = match &self.last {
                    Some((i, n)) if n == &name => *i,
                    _ => self.step,
                };
                let purpose = self
                    .steps
                    .iter()
                    .find(|s| s.idx == idx)
                    .and_then(|s| s.purpose.clone());
                let sv = StepView {
                    idx,
                    tool: name,
                    purpose,
                    state: if is_error { StepState::Error } else { StepState::Ok },
                    ms: None,
                    diff: diff.map(|(a, b)| (a as u32, b as u32)),
                    preview: Some(truncate(&preview, 160)),
                };
                if let Some(slot) = self.steps.iter_mut().find(|s| s.idx == idx) {
                    *slot = sv.clone();
                }
                self.update(sv, None);
            }
            AgentEvent::AssistantText(text) => {
                println!("  · reply, {} characters", text.chars().count());
                self.final_text = text
            }
            AgentEvent::InterimText(text) => {
                println!("  · {text}");
                self.note(text)
            }
            AgentEvent::Warn(text) => self.note(format!("⚠ {text}")),
            AgentEvent::Error(text) => self.note(format!("✗ {text}")),
            AgentEvent::Recycled(n) => self.note(format!("Moved to the recycle bin: {n} file(s)")),
            _ => {}
        }
    }

    /// Ask the room, then wait. The decision travels back over the same socket that is running
    /// this turn, which is why waiting here cannot deadlock: reading happens on its own task.
    async fn authorize(&mut self, tool_name: &str, args: &Value) -> AuthDecision {
        if self.allowed_for_task.contains(tool_name) {
            return AuthDecision::Allow;
        }

        let id = super::frame_id("ap");
        let rx = self.approvals.register(id.clone());
        let approval = ApprovalView {
            id: id.clone(),
            task_id: self.task_id.clone(),
            tool: tool_name.to_string(),
            args_summary: summarize(args),
            purpose: args
                .get("purpose")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            status: ApprovalStatus::Pending,
            expires_at: now_ms() + APPROVAL_TIMEOUT.as_millis() as i64,
        };
        self.send(Body::ApprovalReq(ApprovalReq { approval }));
        println!("  ? waiting for approval: {tool_name}");

        match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(d)) if d.allow => {
                println!("  ✓ allowed");
                if d.for_task {
                    self.allowed_for_task.insert(tool_name.to_string());
                }
                AuthDecision::Allow
            }
            Ok(Ok(_)) => {
                println!("  ✗ denied");
                AuthDecision::Deny
            }
            _ => {
                self.approvals.forget(&id);
                println!("  ✗ approval timed out; treating it as denied");
                AuthDecision::Deny
            }
        }
    }

    async fn turn_end(&mut self) {
        self.send(Body::TaskUpdate(TaskUpdate {
            task_id: self.task_id.clone(),
            agent_id: self.meta.agent_id.clone(),
            source_msg_id: Some(self.meta.msg_id.clone()),
            status: Some(TaskStatus::Done),
            step: None,
            // Report the total again at finalization, since the phone's live counter may miss a frame or two.
            usage: Some(self.usage),
            note: None,
            context_digest: None,
        }));
    }

    /// After each model call, add its usage to the token ledger and push the cumulative total to the phone.
    async fn call_usage(&mut self, usage: &Usage, _last_prompt: Option<u64>) {
        self.usage.calls = self.usage.calls.saturating_add(1);
        self.usage.prompt = self.usage.prompt.saturating_add(usage.prompt.unwrap_or(0));
        self.usage.completion = self
            .usage
            .completion
            .saturating_add(usage.completion.unwrap_or(0));
        // Some endpoints return only a total without input/output split; record it so usage is not stuck at zero.
        if usage.prompt.is_none() && usage.completion.is_none() {
            self.usage.total = self.usage.total.saturating_add(usage.total.unwrap_or(0));
        }
        self.send(Body::TaskUpdate(TaskUpdate {
            task_id: self.task_id.clone(),
            agent_id: self.meta.agent_id.clone(),
            source_msg_id: Some(self.meta.msg_id.clone()),
            status: Some(TaskStatus::Running),
            step: None,
            usage: Some(self.usage),
            note: None,
            context_digest: None,
        }));
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// A one-line, redacted rendering of the arguments — what the person needs to decide, nothing more.
/// The full command is *not* sent: a `shell` call can carry an environment with a key in it.
fn summarize(args: &Value) -> String {
    let Some(obj) = args.as_object() else {
        return truncate(&redact(&args.to_string()), 120);
    };
    let mut parts = Vec::new();
    for (k, v) in obj.iter().take(4) {
        let raw = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        parts.push(format!("{k}={}", truncate(&redact(&raw), 60)));
    }
    parts.join("  ")
}

fn redact(s: &str) -> String {
    let low = s.to_ascii_lowercase();
    const SIGNS: [&str; 6] = ["sk-", "token", "password", "secret", "api_key", "apikey"];
    if SIGNS.iter().any(|k| low.contains(k)) {
        "[redacted]".to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui() -> RemoteUi {
        let (tx, _rx) = mpsc::unbounded_channel();
        RemoteUi::new(
            tx,
            "r1",
            Arc::new(Approvals::default()),
            TaskMeta {
                agent_id: "a1".into(),
                agent_handle: "text".into(),
                owner_id: "u1".into(),
                prompt: "go".into(),
                msg_id: "m1".into(),
            },
        )
    }

    /// Verify an artifact survives a deleted temp copy, since upload happens after the turn ends.
    #[test]
    fn an_artifact_outlives_the_turn_that_deletes_it() {
        let dir = std::env::temp_dir().join("aox-test-stage-src");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("shot.jpg");
        std::fs::write(&src, b"jpeg bytes").unwrap();

        let mut ui = ui();
        ui.emit(AgentEvent::ToolResult {
            name: "publish_file".into(),
            is_error: false,
            preview: "Published shot.jpg".into(),
            diff: None,
            diff_detail: None,
            path: Some(src.clone()),
        });
        std::fs::remove_file(&src).unwrap();

        let arts = ui.take_artifacts();
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].name, "shot.jpg", "the upload name must be the original file name");
        assert_eq!(
            std::fs::read(&arts[0].path).unwrap(),
            b"jpeg bytes",
            "the original is gone, so the staged copy must still be there"
        );

        drop(ui);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same path reported twice should be staged once, and the last on-disk version is what gets delivered.
    #[test]
    fn the_same_file_announced_twice_is_staged_once() {
        let dir = std::env::temp_dir().join("aox-test-stage-dup");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.zip");
        std::fs::write(&src, b"v1").unwrap();

        let mut ui = ui();
        ui.emit(AgentEvent::ToolResult {
            name: "write_file".into(),
            is_error: false,
            preview: String::new(),
            diff: None,
            diff_detail: None,
            path: Some(src.clone()),
        });
        std::fs::write(&src, b"v2").unwrap();
        ui.emit(AgentEvent::ToolResult {
            name: "edit_file".into(),
            is_error: false,
            preview: String::new(),
            diff: None,
            diff_detail: None,
            path: Some(src.clone()),
        });

        let arts = ui.take_artifacts();
        assert_eq!(arts.len(), 1, "the same path should take one slot only");
        assert_eq!(
            std::fs::read(&arts[0].path).unwrap(),
            b"v2",
            "what gets sent is the last version, not the first"
        );

        drop(ui);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
