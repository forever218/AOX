//! AOX Remote wire protocol: the single source of truth shared by the server and the host bridge.
//! PROTO_VERSION bumps only on breaking changes; optional fields are safe (unknown keys ignored, None skipped).
//! Every frame is one JSON object: envelope (v/id/ts/room) plus a type-tagged body; room lives on the envelope.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Bumped only on a breaking change. See the module docs.
pub const PROTO_VERSION: u32 = 1;

/// Heartbeat interval. Both ends send a `ping` this often when idle; three missed beats and the
/// connection is considered dead (the client reconnects with jittered backoff).
pub const HEARTBEAT_SECS: u64 = 25;

/// Hard cap on one text message. Longer input is rejected by the server, not truncated silently.
pub const MAX_TEXT_CHARS: usize = 8_000;

/// A frame larger than this is refused before parsing (protects the server from a 2 GB `msg_send`).
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// How many messages one `sync` request may return.
pub const SYNC_PAGE_MAX: i64 = 200;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// Envelope.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub v: u32,
    pub id: String,
    pub ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    #[serde(flatten)]
    pub body: Body,
}

impl Frame {
    pub fn new(id: impl Into<String>, ts: i64, body: Body) -> Self {
        Self { v: PROTO_VERSION, id: id.into(), ts, room: None, body }
    }

    pub fn in_room(mut self, room: impl Into<String>) -> Self {
        self.room = Some(room.into());
        self
    }

    pub fn is_compatible(&self) -> bool {
        self.v == PROTO_VERSION
    }
}

/// Every message either direction. The variant name is the wire `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Body {
    // client → server
    Hello(Hello),
    Ping(Ping),
    MsgSend(MsgSend),
    TaskCancel(TaskCancel),
    ApprovalDecide(ApprovalDecide),
    /// The owner's answer to "may this person drive your machine?".
    CallDecide(CallDecide),
    Typing(Typing),

    // ── host bridge only ─────────────────────────────────────────────────
    /// The host announces which agent it carries and which rooms it is serving.
    AgentOnline(AgentOnline),
    /// The host answers a dispatched task: accepted, or refused (offline / quota / unknown room).
    TaskAccept(TaskAccept),
    /// The host asks its owner for permission to run someone else's instruction.
    CallRequest(CallRequest),
    /// Host announces work start/stop; the server marks the Agent busy or idle and broadcasts Presence to its room.
    AgentBusy(AgentBusy),

    // server → client
    HelloOk(HelloOk),
    Pong(Pong),
    MsgNew(MsgNew),
    /// Full room todo list sent on each change; one frame merges add/edit/delete, so clients need no replay order.
    Todos(Todos),
    /// Transient progress — **never persisted**, safe to drop when a client is slow.
    TaskUpdate(TaskUpdate),
    /// A persisted state transition; this is what a reconnecting client rebuilds from.
    TaskState(TaskState),
    ApprovalReq(ApprovalReq),
    Presence(Presence),
    RoomUpdated(RoomUpdated),
    /// The room is gone for this client (dissolved / kicked / left). The client drops it from the
    /// local list; it is not an error, so it must not be shown as one.
    RoomRemoved(RoomRemoved),
    /// "You are too far behind; fetch this range over HTTP." Never followed by a flood of frames.
    SyncGap(SyncGap),
    Error(FrameError),
}

// Handshake.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub auth: Auth,
    pub device: DeviceInfo,
    /// Last seq the client already has, per room. The server replays what is missing.
    #[serde(default)]
    pub resume: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Auth {
    /// Opaque device token. Never a JWT — see the design doc D5
    pub token: String,
    pub device_id: String,
    pub proto: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub kind: DeviceKind,
    pub name: String,
    #[serde(default)]
    pub app_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Phone,
    Host,
    /// A browser tab. Speaks as its owner like a phone, but is not one: it cannot receive
    /// background pushes, and the device list should not pretend it is a handset.
    Web,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloOk {
    pub user: User,
    pub rooms: Vec<Room>,
    pub server_ts: i64,
    pub proto: u32,
}

/// Heartbeat payload. Deliberately empty: the envelope already carries `ts`, and a `ts` here would be flattened...
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ping {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pong {}

// Core entities.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub handle: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Room {
    pub id: String,
    pub name: String,
    pub kind: RoomKind,
    pub policy: RoomPolicy,
    pub owner_id: String,
    pub last_seq: i64,
    /// Owner-editable room avatar as a relative path; clients prepend the host, avoiding domain-change migrations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub members: Vec<MemberView>,
    #[serde(default)]
    pub agents: Vec<AgentView>,
    /// Room file quota in MB (0 = unlimited); server blocks uploads, host prunes inbox; old servers default to 200.
    #[serde(default = "default_storage_mb")]
    pub max_storage_mb: u32,
    /// Keep only the most recent N messages, deleting older ones on write; 0 keeps all. Owner-editable.
    #[serde(default)]
    pub keep_messages: i64,
    /// Bytes the room's files currently occupy on the server; a quota needs usage to show the owner a progress bar.
    #[serde(default)]
    pub storage_used: u64,
    /// Last-message preview for the conversation list; full messages fetch by seq, and old servers default to None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_msg: Option<LastMsg>,
}

/// The one-line preview of the last message shown in the conversation list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastMsg {
    pub seq: i64,
    pub sender_name: String,
    /// Message kind (text/media/artifact/task/system/approval), matching the tag of MessageBody.
    pub kind: String,
    /// An already-truncated one-liner, truncated server-side so an 8 KB reply is not sent with every list request.
    pub preview: String,
    pub created_at: i64,
}

/// Old servers omit it; the default must match the old value or rooms silently flip to unlimited/zero.
fn default_storage_mb() -> u32 {
    200
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomKind {
    Direct,
    Group,
}

/// What a room is allowed to do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomPolicy {
    /// Read-only mode: answering questions and reading files, with nothing that writes.
    Readonly,
    /// Default: every write/command needs an approval
    Ask,
    /// Auto-run, still bounded by workplace + tool allowlist + quota.
    Trusted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemberView {
    pub user_id: String,
    pub handle: String,
    pub display_name: String,
    pub role: Role,
    #[serde(default)]
    pub online: bool,
    /// Relative avatar path; None when unset, so clients fall back to a first-letter tile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Member,
    Viewer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentView {
    pub id: String,
    pub handle: String,
    pub display_name: String,
    pub owner_id: String,
    #[serde(default)]
    pub online: bool,
    /// True while busy: foreign @-mentions queue and clients show busy, so a send does not look unanswered.
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub share_policy: SharePolicy,
    #[serde(default)]
    pub caps: Caps,
    /// Relative avatar path, set by the Agent's owner and shared across all rooms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

/// Who may drive this agent inside a room. Default is `owner` — a group does not silently grant
/// everyone a shell on your machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharePolicy {
    Owner,
    Room,
    Nobody,
}

impl Default for SharePolicy {
    fn default() -> Self {
        Self::Owner
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Caps {
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub version: String,
    /// A short label for the workspace ("rustapp"), never the full path — paths leak usernames.
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub tools: Vec<String>,
}

// Messages.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub room: String,
    pub seq: i64,
    pub sender_id: String,
    pub sender_kind: SenderKind,
    pub sender_name: String,
    /// Sender avatar read live, not stored, so new avatars show in history; two batched IN queries per page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_avatar: Option<String>,
    pub created_at: i64,
    /// Which message is quoted, pre-expanded into a short summary so the phone draws the quote bar without a refetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<ReplyView>,
    #[serde(flatten)]
    pub body: MessageBody,
}

/// What the quote bar shows: who said it, what (truncated), and the seq to jump back to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplyView {
    pub id: String,
    pub seq: i64,
    pub sender_name: String,
    /// One truncated line; encrypted rooms use a placeholder (base64 cannot be cut) and keep ciphertext in sealed.
    pub preview: String,
    /// Full ciphertext for encrypted messages; clients with the passphrase decrypt it, and the server holds no keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed: Option<String>,
    /// Artifacts of the quoted message, so the quote bar names them and the host fetches an authoritative list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    User,
    Agent,
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageBody {
    Text { text: String },
    /// One line plus up to 9 images uploaded first; the server overwrites url/size from the DB so fakes are ignored.
    Media { text: String, artifacts: Vec<ArtifactView> },
    Task { task: TaskView },
    Artifact { artifact: ArtifactView },
    Approval { approval: ApprovalView },
    System { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MsgSend {
    /// Client-generated idempotency key. A retry after a timeout must not double-post.
    pub client_msg_id: String,
    /// Nested on purpose (no `flatten`): the frame already has a `type`, and collapsing the message body...
    pub body: MessageBody,
    /// Which message is quoted (id only); the server expands the summary from stored text so clients cannot forge it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Only these ids are cited (empty = all); the server validates and stores the pick so clients agree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply_artifact_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MsgNew {
    pub msg: Message,
}

/// One room todo; editing is open to everyone (a product decision), and created_by only labels who added it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub done: bool,
    pub created_by: String,
    /// Display name of whoever added the todo, kept as a snapshot: renaming or leaving does not change it.
    #[serde(default)]
    pub created_by_name: String,
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

/// A full snapshot of one room's todos.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Todos {
    pub room: String,
    pub items: Vec<TodoItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Typing {
    #[serde(default)]
    pub on: bool,
}

// Tasks.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskView {
    pub id: String,
    pub agent_id: String,
    pub agent_handle: String,
    pub created_by: String,
    /// The instruction as the human wrote it, without the @mention.
    pub prompt: String,
    /// The triggering message id, so the phone attaches the running bar to the exact @agent line, not a fuzzy match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_msg_id: Option<String>,
    pub status: TaskStatus,
    #[serde(default)]
    pub steps: Vec<StepView>,
    #[serde(default)]
    pub usage: UsageView,
    /// Filled in whenever the prompt used a room-context digest, so "what did it see" is auditable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    WaitingApproval,
    Done,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepView {
    pub idx: u32,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    pub state: StepState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    /// Lines added / removed, for an edit
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<(u32, u32)>,
    /// First line(s) of the result, already redacted on the host. Never the full output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Running,
    Ok,
    Error,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageView {
    #[serde(default)]
    pub prompt: u64,
    #[serde(default)]
    pub completion: u64,
    /// Total token count used when the endpoint reports only a total, not split input/output; 0 means unreported.
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub calls: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskUpdate {
    pub task_id: String,
    /// Machine running this round; task_id has no owner, so the server overwrites it with the connection's agent_id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_id: String,
    /// The message that triggered this round (the original @agent line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_msg_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<StepView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageView>,
    /// One line of interim narration (e.g. looking at the directory structure first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// What the round gathered from the room, sent once collected so users see it at once, not later in history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskState {
    pub task: TaskView,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCancel {
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskAccept {
    pub task_id: String,
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// Approvals.

/// Host request for machine access, sent only to the Agent's owner; pre-task (no task_id), unlike ApprovalView.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallRequest {
    /// Named call_id, not id: Body flattens beside the envelope's id, so serde would see a duplicate field.
    pub call_id: String,
    pub agent_id: String,
    pub agent_handle: String,
    pub from_user_id: String,
    pub from_name: String,
    /// The raw command, host-truncated to 200 chars and not redacted since room members can already see the original.
    pub text: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallDecide {
    /// Same as above: this field cannot be named id either.
    pub call_id: String,
    pub allow: bool,
    /// For this duration, repeat @-mentions from the same person/room skip the prompt; default is once only.
    #[serde(default)]
    pub remember_secs: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentBusy {
    pub agent_id: String,
    pub busy: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalView {
    pub id: String,
    pub task_id: String,
    pub tool: String,
    /// What will actually run, redacted and truncated on the host ("npm run build", not the full env).
    pub args_summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    pub status: ApprovalStatus,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Allowed,
    Denied,
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalReq {
    pub approval: ApprovalView,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalDecide {
    pub approval_id: String,
    pub allow: bool,
    /// `true` = remember for this task only (never for the whole process — that would leak across
    /// rooms, see design doc §4 D2).
    #[serde(default)]
    pub for_task: bool,
}

// Presence / room / sync / errors.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentOnline {
    pub agent: AgentView,
    #[serde(default)]
    pub rooms: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Presence {
    pub who_id: String,
    pub who_kind: SenderKind,
    pub online: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomUpdated {
    pub room: Room,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomRemoved {
    pub room: String,
    pub reason: RemovedReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovedReason {
    /// The owner dissolved the room.
    Deleted,
    /// You were removed by the owner.
    Kicked,
    /// You left on your own.
    Left,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncGap {
    pub from: i64,
    pub to: i64,
    pub reason: GapReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    TooFarBehind,
    Resync,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadAuth,
    Forbidden,
    NotFound,
    RateLimited,
    ProtoMismatch,
    PayloadTooLarge,
    /// A well-formed frame for something this build does not implement yet (M1 answers
    /// `task.cancel` / `approval.decide` with this rather than silently dropping them).
    Unsupported,
    /// The agent's owner has no phone connected, so a `call_request` cannot be delivered. Distinct
    /// from `not_found`: the host should tell the requester "owner is away", not retry.
    OwnerOffline,
    /// Room quota exhausted; retrying is pointless (unlike PayloadTooLarge) until space is freed or the limit raised.
    StorageFull,
    Internal,
}

/// Files an agent produced. `url` is a server-relative path; the client joins it with its base URL
/// and adds its own token, so a file link can never be forwarded to someone without access.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactView {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub mime: String,
    #[serde(default)]
    pub sha256: String,
    pub url: String,
    /// Who uploaded it: user (phone album/camera) or agent (host-produced).
    #[serde(default)]
    pub origin: String,
    /// Image pixel dimensions, 0 for non-images; clients reserve the space before load so the list does not jump.
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

// Tests — the point is that the wire shape is pinned, not that serde works.

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(f: &Frame) -> Frame {
        let s = serde_json::to_string(f).expect("serialize");
        serde_json::from_str(&s).expect("deserialize")
    }

    #[test]
    fn msg_send_keeps_the_message_nested() {
        let f = Frame::new(
            "01J1",
            now_ms(),
            Body::MsgSend(MsgSend {
                client_msg_id: "c_1".into(),
                body: MessageBody::Text { text: "Build the release".into() },
                reply_to: None,
                reply_artifact_ids: vec![],
            }),
        )
        .in_room("r_1");

        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "msg_send");
        assert_eq!(v["room"], "r_1");
        assert_eq!(v["body"]["kind"], "text");
        assert_eq!(v["body"]["text"], "Build the release");
        assert_eq!(v["v"], PROTO_VERSION);

        assert_eq!(round_trip(&f), f);
    }

    #[test]
    fn task_update_survives_missing_optionals() {
        let wire = r#"{"v":1,"id":"x","ts":1,"type":"task_update","task_id":"t_1"}"#;
        let f: Frame = serde_json::from_str(wire).expect("parse");
        match &f.body {
            Body::TaskUpdate(u) => {
                assert_eq!(u.task_id, "t_1");
                assert!(u.step.is_none());
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // and it must re-serialize without inventing nulls
        let out = serde_json::to_value(&f).unwrap();
        assert!(out.get("step").is_none());
    }

    #[test]
    fn unknown_fields_are_tolerated_for_forward_compat() {
        let wire = r#"{"v":1,"id":"x","ts":1,"type":"ping","future_hint":"whatever"}"#;
        let f: Frame = serde_json::from_str(wire).expect("an old peer must not choke on new fields");
        assert!(matches!(f.body, Body::Ping(_)));
    }

    #[test]
    fn a_full_room_and_task_round_trip() {
        let task = TaskView {
            id: "t_1".into(),
            agent_id: "a_1".into(),
            agent_handle: "aox-alice".into(),
            created_by: "u_1".into(),
            prompt: "Build a pomodoro timer".into(),
            source_msg_id: Some("m_9".into()),
            status: TaskStatus::Running,
            steps: vec![StepView {
                idx: 0,
                tool: "write_file".into(),
                purpose: Some("write main.dart".into()),
                state: StepState::Ok,
                ms: Some(120),
                diff: Some((184, 0)),
                preview: None,
            }],
            usage: UsageView { prompt: 12_400, completion: 900, total: 0, calls: 3 },
            context_digest: Some("20 messages / 1 artifact".into()),
            error: None,
            artifacts: vec![],
            started_at: Some(100),
            ended_at: None,
        };
        let f = Frame::in_room(
            Frame::new("01J2", 1, Body::TaskState(TaskState { task })),
            "r_1",
        );
        assert_eq!(round_trip(&f), f);
    }

    #[test]
    fn message_body_is_tagged_by_kind() {
        let m = Message {
            id: "m_1".into(),
            room: "r_1".into(),
            seq: 7,
            sender_id: "a_1".into(),
            sender_kind: SenderKind::Agent,
            sender_name: "aox-A".into(),
            sender_avatar: None,
            created_at: 1,
            reply_to: None,
            body: MessageBody::System { text: "Task queued".into() },
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["kind"], "system");
        assert_eq!(v["seq"], 7);
        // With no quote the whole key is omitted, so old clients never see a permanently null field.
        assert!(v.get("reply_to").is_none());
    }

    /// Quotes are delivered pre-expanded, so the phone draws the quote bar without refetching the original.
    #[test]
    fn a_reply_carries_the_quote_itself() {
        let m = Message {
            id: "m_2".into(),
            room: "r_1".into(),
            seq: 9,
            sender_id: "u_1".into(),
            sender_kind: SenderKind::User,
            sender_name: "alice".into(),
            sender_avatar: None,
            created_at: 2,
            reply_to: Some(ReplyView {
                id: "m_1".into(),
                seq: 7,
                sender_name: "aox-A".into(),
                preview: "Task queued".into(),
                sealed: None,
                artifacts: vec![],
            }),
            body: MessageBody::Text { text: "ok".into() },
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["reply_to"]["seq"], 7);
        assert_eq!(v["reply_to"]["preview"], "Task queued");
        // The flattened body and reply_to sit side by side without overwriting each other.
        assert_eq!(v["kind"], "text");
        assert_eq!(v["text"], "ok");
    }

    /// Encrypted ciphertext must not be cut (base64 breaks), so it goes to sealed and preview is only a placeholder.
    #[test]
    fn a_sealed_reply_keeps_the_ciphertext_whole() {
        let ct = "enc:v1:AAAABBBBCCCCDDDD";
        let sealed = ReplyView {
            id: "m_1".into(),
            seq: 3,
            sender_name: "B".into(),
            preview: "Encrypted message".into(),
            sealed: Some(ct.into()),
            artifacts: vec![],
        };
        let v = serde_json::to_value(&sealed).unwrap();
        assert_eq!(v["sealed"], ct);
        assert_eq!(v["preview"], "Encrypted message");

        // Plaintext messages omit this key, so old clients never see a permanently null field.
        let plain = ReplyView { sealed: None, ..sealed };
        assert!(serde_json::to_value(&plain).unwrap().get("sealed").is_none());
    }

    #[test]
    fn proto_mismatch_is_detectable() {
        let mut f = Frame::new("x", 1, Body::Ping(Ping {}));
        assert!(f.is_compatible());
        f.v = PROTO_VERSION + 1;
        assert!(!f.is_compatible());
    }

    #[test]
    fn default_share_policy_is_owner() {
        // A group must never hand out a shell by accident.
        assert_eq!(SharePolicy::default(), SharePolicy::Owner);
    }
}
