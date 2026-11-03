//! The `Ui` implementation that speaks the wire instead of the terminal.
//!
//! Same trait the CLI and GUI implement, so `Agent::run_turn` is unchanged: every event the host
//! would have painted locally becomes a `task_update` frame the phone can draw.
//!
//! Approvals go over the wire in both directions: `approval_req` out, `approval_decide` back in
//! through the read loop (see [`super::approvals`]). A request that nobody answers inside
//! [`APPROVAL_TIMEOUT`] is treated as a refusal — silence is never consent.
//!
//! **Reasoning tokens are not uploaded.** They are the most private part of a turn and the least
//! useful on a 6-inch screen.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use aoxchat_proto::{
    now_ms, ApprovalReq, ApprovalStatus, ApprovalView, Body, Frame, StepState, StepView,
    TaskStatus, TaskUpdate, TaskView,
};

use crate::ui::{AgentEvent, AuthDecision, Ui};

use super::approvals::Approvals;

/// 一场执行的身份信息：写任务快照时要用，但 `RemoteUi` 自己推不出来。
pub struct TaskMeta {
    pub agent_id: String,
    pub agent_handle: String,
    pub owner_id: String,
    pub prompt: String,
}

/// How long a phone has to answer before the tool is refused. Long enough to read a command,
/// short enough that a desk-side turn does not hang forever on a phone left in another room.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// 一个已经落到暂存区的产物。
pub struct Staged {
    /// 原件路径。只用来去重——原件此刻可能已经不在了。
    pub src: std::path::PathBuf,
    /// 上传时用的名字。暂存区里的文件名被加了序号前缀，避免同名互相覆盖。
    pub name: String,
    /// 暂存副本。上传只读这一份。
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
    /// Tools the person chose "本任务内都允许" for. Per turn by construction.
    allowed_for_task: HashSet<String>,
    /// Files this turn produced (`ToolOutput::path`), deduped, in the order they appeared.
    /// Uploaded **after** the turn ends — uploading mid-turn would put half-written files in the room.
    ///
    /// Each one is **copied out** the moment it is announced, not just remembered by path. The
    /// agent routinely publishes a temp copy and then deletes it in the same turn ("顺手清掉了
    /// 工作区里的临时副本"); by the time the turn ends the original is gone and the room gets
    /// nothing. Staging costs one copy and keeps the bytes.
    artifacts: Vec<Staged>,
    meta: TaskMeta,
    steps: Vec<StepView>,
    started_at: i64,
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
        }
    }

    /// 这轮执行的最终快照，写到房间历史里。
    pub fn snapshot(&self, status: TaskStatus, error: Option<String>) -> TaskView {
        TaskView {
            id: self.task_id.clone(),
            agent_id: self.meta.agent_id.clone(),
            agent_handle: self.meta.agent_handle.clone(),
            created_by: self.meta.owner_id.clone(),
            prompt: self.meta.prompt.clone(),
            status,
            steps: self.steps.clone(),
            usage: Default::default(),
            context_digest: None,
            error,
            artifacts: Vec::new(),
            started_at: Some(self.started_at),
            ended_at: Some(now_ms()),
        }
    }

    pub fn take_artifacts(&mut self) -> Vec<Staged> {
        std::mem::take(&mut self.artifacts)
    }

    /// 暂存目录：每轮一个，名字用任务 id（它已经是唯一的，取消也按它路由）。
    fn stage_dir(&self) -> std::path::PathBuf {
        std::env::temp_dir().join("aox-artifacts").join(&self.task_id)
    }

    /// 把产物拷进暂存区。拷不动就退回原路径——原件此刻还在，至少还有一次机会。
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
                eprintln!("产物暂存失败（{}）：{e}，改为直接读原件", src.display());
                src.to_path_buf()
            }
        };
        Staged { src: src.to_path_buf(), name, path }
    }

    /// 任务 id：取消要按它路由，所以必须在开跑前就能拿到。
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

    fn note(&self, text: impl Into<String>) {
        self.send(Body::TaskUpdate(TaskUpdate {
            task_id: self.task_id.clone(),
            status: None,
            step: None,
            usage: None,
            note: Some(text.into()),
        }));
    }

    fn update(&self, step: StepView, status: Option<TaskStatus>) {
        self.send(Body::TaskUpdate(TaskUpdate {
            task_id: self.task_id.clone(),
            status,
            step: Some(step),
            usage: None,
            note: None,
        }));
    }
}

/// 暂存区跟着这一轮走：正常结束在 `worker` 里删，取消/报错靠这里兜底。
/// 取消的那一轮产物本来就不该发出去，删掉正好。
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
                println!("  · 调用 {name}");
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
                    // 只收真实存在、且本轮没出现过的文件。出现即拷贝：这一轮后面很可能把它删掉。
                    if p.is_file() {
                        match self.artifacts.iter().position(|x| x.src == p) {
                            // 同一路径又写了一次（`write_file` 之后 `edit_file`）：换掉暂存副本，
                            // 让最后落盘的那一版发出去，而不是第一版。
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
                println!("  · 回复 {} 字", text.chars().count());
                self.final_text = text
            }
            AgentEvent::InterimText(text) => {
                println!("  · {text}");
                self.note(text)
            }
            AgentEvent::Warn(text) => self.note(format!("⚠ {text}")),
            AgentEvent::Error(text) => self.note(format!("✗ {text}")),
            AgentEvent::Recycled(n) => self.note(format!("已移入回收站：{n} 个文件")),
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
        println!("  ? 等待审批：{tool_name}");

        match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(d)) if d.allow => {
                println!("  ✓ 已允许");
                if d.for_task {
                    self.allowed_for_task.insert(tool_name.to_string());
                }
                AuthDecision::Allow
            }
            Ok(Ok(_)) => {
                println!("  ✗ 被拒绝");
                AuthDecision::Deny
            }
            _ => {
                self.approvals.forget(&id);
                println!("  ✗ 审批超时，按拒绝处理");
                AuthDecision::Deny
            }
        }
    }

    async fn turn_end(&mut self) {
        self.send(Body::TaskUpdate(TaskUpdate {
            task_id: self.task_id.clone(),
            status: Some(TaskStatus::Done),
            step: None,
            usage: None,
            note: None,
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
        "[已隐藏]".to_string()
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
            },
        )
    }

    /// 这一轮把临时副本删掉了，产物也必须还在。
    ///
    /// 这就是"图片发不出来"的现场：模型 publish 一张桌面截图后顺手删了工作区里的副本，
    /// 上传发生在整轮结束之后，那时原件已经没了，房间什么也没收到，而回复里还写着"已经发到房间里了"。
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
        assert_eq!(arts[0].name, "shot.jpg", "上传用的名字必须是原文件名");
        assert_eq!(
            std::fs::read(&arts[0].path).unwrap(),
            b"jpeg bytes",
            "原件没了，暂存副本还得在"
        );

        drop(ui);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同一路径报了两次（`write_file` 之后 `edit_file`）：只发一份，且是最后落盘的那一版。
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
        assert_eq!(arts.len(), 1, "同一路径只该占一条");
        assert_eq!(
            std::fs::read(&arts[0].path).unwrap(),
            b"v2",
            "发出去的是最后一版，不是第一版"
        );

        drop(ui);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
