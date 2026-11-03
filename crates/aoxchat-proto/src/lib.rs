//! AOX Remote wire protocol.
//!
//! **This crate is the single source of truth.** The server (`server/`) and the host bridge
//! (AOX's `src/remote/`) both depend on it, so a field cannot be renamed on one side only.
//!
//! # Versioning rules
//!
//! 1. `PROTO_VERSION` bumps **only** on a breaking change (removed/renamed field, changed meaning).
//! 2. Adding an optional field is **not** breaking: every payload struct tolerates unknown keys on
//!    read (serde ignores them by default) and skips `None` on write, so an old peer simply does
//!    not see it.
//! 3. The server rejects a `hello` whose `proto` it cannot speak, with `error.proto_mismatch`.
//!    It never guesses.
//!
//! # Envelope
//!
//! Every frame is one JSON object: the envelope (`v` / `id` / `ts` / `room`) plus the body
//! flattened in, tagged by `type`:
//!
//! ```json
//! {"v":1,"id":"01J...","ts":1730000000000,"room":"r_1",
//!  "type":"msg_send","client_msg_id":"c_1","body":{"kind":"text","text":"构建 release"}}
//! ```
//!
//! `room` lives on the envelope rather than inside each body: half the frames are about a room,
//! and repeating the field in ten structs is how two of them end up disagreeing.

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

// ─────────────────────────────────────────────────────────────────────────────
// Envelope
// ─────────────────────────────────────────────────────────────────────────────

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
    // ── client → server ──────────────────────────────────────────────────
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
    /// The host asks its owner for permission to run **someone else's** instruction. Delivered only
    /// to the owner's phones — sending it back to the asking host would leave it waiting for an
    /// answer only it could give.
    CallRequest(CallRequest),
    /// 宿主宣告"我开始/结束干活了"。服务端据此把该 Agent 标成占用中，并向它所在的房间广播
    /// `Presence`（`status` 为 `busy` / `idle`）。
    AgentBusy(AgentBusy),

    // ── server → client ──────────────────────────────────────────────────
    HelloOk(HelloOk),
    Pong(Pong),
    MsgNew(MsgNew),
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

// ─────────────────────────────────────────────────────────────────────────────
// Handshake
// ─────────────────────────────────────────────────────────────────────────────

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
    /// Opaque device token. Never a JWT — see the design doc D5.
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloOk {
    pub user: User,
    pub rooms: Vec<Room>,
    pub server_ts: i64,
    pub proto: u32,
}

/// Heartbeat payload. **Deliberately empty**: the envelope already carries `ts`, and a `ts` here
/// would be flattened next to it — producing `{"ts":…,"ts":…}` and a `duplicate field` error on
/// the receiving side. (Found the hard way: the first `aox remote serve` connection killed itself
/// on its own first ping.) Use `Frame::ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ping {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pong {}

// ─────────────────────────────────────────────────────────────────────────────
// Core entities
// ─────────────────────────────────────────────────────────────────────────────

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
    /// 群头像，房主可改。**相对路径**（`/api/avatars/<token>.jpg`），客户端自己拼上服务端
    /// 地址——服务端不知道自己对外叫什么域名，写死绝对地址等于把换域名变成数据迁移。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub members: Vec<MemberView>,
    #[serde(default)]
    pub agents: Vec<AgentView>,
    /// 这个房间最多能占多少 MB 文件（图片与产物），0 = 不限。房主可改。
    ///
    /// 服务端按它挡上传，宿主按它清自己的 inbox 副本——两边共用一个数字，否则"房间上限"
    /// 只是服务端的账，手机那头照样把磁盘吃满。旧服务端不带这个字段，缺省按 200 理解，
    /// 与改动前宿主写死的上限一致。
    #[serde(default = "default_storage_mb")]
    pub max_storage_mb: u32,
    /// 只保留最近多少条消息，更早的在写入时删掉，0 = 全留。房主可改。
    #[serde(default)]
    pub keep_messages: i64,
    /// 房间里的文件现在占了服务端多少字节。上限没有用量就是猜谜：给房主看的进度。
    #[serde(default)]
    pub storage_used: u64,
    /// 最后一条消息的摘要，会话列表那一行要用。没有消息的房间是 `None`。
    ///
    /// 只在这里带一句话预览，不是把整条消息塞进房间对象：列表页每行只需要"谁说了什么、什么时候"，
    /// 真正的消息仍按 seq 拉。旧服务端不带这个字段，缺省为 None（列表退回只显示成员数）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_msg: Option<LastMsg>,
}

/// 会话列表里"最后一条"的摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastMsg {
    pub seq: i64,
    pub sender_name: String,
    /// 消息种类（`text` / `media` / `artifact` / `task` / `system` / `approval`），
    /// 和 [`MessageBody`] 的 tag 一致。
    pub kind: String,
    /// **已经截断过**的一句话。截断放在服务端：一条 8 KB 的回复不该跟着每次列表请求跑一遍。
    pub preview: String,
    pub created_at: i64,
}

/// 旧服务端不发这个字段。缺省值必须是改动前写死的那一个，否则升级瞬间所有房间"变成不限"，
/// 或者反过来"变成 0 兆谁都传不了"。
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
    /// Read-only:问答与读文件, nothing that writes.
    Readonly,
    /// Default: every write/command needs an approval.
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
    /// 头像相对路径。没有设置就是 None，客户端退回"首字方块"。
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
    /// 正在执行任务。为真时别的群发来的 `@` 会排队，客户端要把它显示成"占用中"，
    /// 否则用户只会看到"发了没反应"。
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub share_policy: SharePolicy,
    #[serde(default)]
    pub caps: Caps,
    /// 头像相对路径。由 Agent 的主人设置，所有房间共用一张。
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

// ─────────────────────────────────────────────────────────────────────────────
// Messages
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub room: String,
    pub seq: i64,
    pub sender_id: String,
    pub sender_kind: SenderKind,
    pub sender_name: String,
    /// 发送者的头像（相对路径）。**读的时候现查**，不是落库时冗余的那一份：刚设置完头像的人
    /// 应该连历史消息里也立刻显示新头像，否则"我设了头像"这件事在自己的旧对话里看不出来。
    ///
    /// 代价是一页消息多两次 `IN` 查询（users / agents 各一次，按发送者去重），与引用的展开
    /// 同一次批量完成，不是每条一次往返。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_avatar: Option<String>,
    pub created_at: i64,
    /// 这条消息引用的是哪一条。**已展开**成一小段摘要，手机不用为了画引用条再拉一次原消息。
    ///
    /// 展开在服务端做：引用条只需要 20 个字，让客户端自己按 id 回查会变成每条引用一次往返。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<ReplyView>,
    #[serde(flatten)]
    pub body: MessageBody,
}

/// 引用条要显示的东西：谁说的、说了什么（已截断）、跳回去时按 seq 定位。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplyView {
    pub id: String,
    pub seq: i64,
    pub sender_name: String,
    /// 已经压成一行并截断，规则和会话列表的预览是同一套（`preview_of`）。
    pub preview: String,
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
    /// 一句话 + 最多 9 张图。图必须先经 `/artifacts?defer=1` 上传拿到 id，发送时服务端会用
    /// 库里的数据覆盖一遍，所以客户端伪造 url / size 没有意义。
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
    /// Nested on purpose (no `flatten`): the frame already has a `type`, and collapsing the
    /// message body into the envelope would put `kind` and `type` side by side and make
    /// "which one am I reading" a coin flip.
    pub body: MessageBody,
    /// 引用的是哪一条（消息 id）。只传 id：摘要由服务端按库里的原文展开，客户端伪造不了。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MsgNew {
    pub msg: Message,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Typing {
    #[serde(default)]
    pub on: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Tasks
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskView {
    pub id: String,
    pub agent_id: String,
    pub agent_handle: String,
    pub created_by: String,
    /// The instruction as the human wrote it, without the @mention.
    pub prompt: String,
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
    /// Lines added / removed, for an edit.
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
    #[serde(default)]
    pub calls: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskUpdate {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<StepView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageView>,
    /// One line of interim narration ("先看目录结构").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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

// ─────────────────────────────────────────────────────────────────────────────
// Approvals
// ─────────────────────────────────────────────────────────────────────────────

/// 一个人请求调用另一个人的机器。由宿主发出，只投递给 Agent 的主人。
///
/// 与 `ApprovalView` 的区别：那个问的是"这条命令能不能跑"（任务进行中，带 task_id），
/// 这个问的是"这个人能不能用我的机器"（任务开始前，没有 task_id）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallRequest {
    /// 注意字段名是 `call_id` 而不是 `id`：`Body` 会被 flatten 到信封上，而信封已经有 `id`
    /// （帧编号）。两个 `id` 并排会让 serde 报 duplicate field —— 和 `Ping{ts}` 是同一个坑。
    pub call_id: String,
    pub agent_id: String,
    pub agent_handle: String,
    pub from_user_id: String,
    pub from_name: String,
    /// 原始指令，宿主已截断到 200 字。不脱敏：房间里的人本来就看得见原文。
    pub text: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallDecide {
    /// 同上：不能叫 `id`。
    pub call_id: String,
    pub allow: bool,
    /// 在这个时长内，同房间、同一个人再 `@` 不再询问。缺省 = 只此一次。
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

// ─────────────────────────────────────────────────────────────────────────────
// Presence / room / sync / errors
// ─────────────────────────────────────────────────────────────────────────────

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
    /// 房间的文件总额度用完了。与 `PayloadTooLarge` 分开：那个是"这一个文件太大"，
    /// 换个小文件就能过；这个只能等清理、调高上限或删掉旧内容，重试没有意义。
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
    /// 谁传的：`user`（手机相册 / 拍照）还是 `agent`（宿主产出）。
    #[serde(default)]
    pub origin: String,
    /// 图片像素尺寸，非图片为 0。客户端靠它在图片加载完成前把位置占好，列表不会跳。
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — the point is that the wire shape is pinned, not that serde works
// ─────────────────────────────────────────────────────────────────────────────

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
                body: MessageBody::Text { text: "构建 release".into() },
                reply_to: None,
            }),
        )
        .in_room("r_1");

        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "msg_send");
        assert_eq!(v["room"], "r_1");
        assert_eq!(v["body"]["kind"], "text");
        assert_eq!(v["body"]["text"], "构建 release");
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
            agent_handle: "aox-lisiran".into(),
            created_by: "u_1".into(),
            prompt: "做一个番茄钟".into(),
            status: TaskStatus::Running,
            steps: vec![StepView {
                idx: 0,
                tool: "write_file".into(),
                purpose: Some("写 main.dart".into()),
                state: StepState::Ok,
                ms: Some(120),
                diff: Some((184, 0)),
                preview: None,
            }],
            usage: UsageView { prompt: 12_400, completion: 900, calls: 3 },
            context_digest: Some("20 条消息 / 1 个产物".into()),
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
            created_at: 1,
            reply_to: None,
            body: MessageBody::System { text: "任务已排队".into() },
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["kind"], "system");
        assert_eq!(v["seq"], 7);
        // 没引用时整个键都不出现：老客户端不用看见一个永远为 null 的字段。
        assert!(v.get("reply_to").is_none());
    }

    /// 引用是**展开后**下发的：手机拿到就能画引用条，不用再回查一次原消息。
    #[test]
    fn a_reply_carries_the_quote_itself() {
        let m = Message {
            id: "m_2".into(),
            room: "r_1".into(),
            seq: 9,
            sender_id: "u_1".into(),
            sender_kind: SenderKind::User,
            sender_name: "lisiran".into(),
            created_at: 2,
            reply_to: Some(ReplyView {
                id: "m_1".into(),
                seq: 7,
                sender_name: "aox-A".into(),
                preview: "任务已排队".into(),
            }),
            body: MessageBody::Text { text: "好".into() },
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["reply_to"]["seq"], 7);
        assert_eq!(v["reply_to"]["preview"], "任务已排队");
        // flatten 的 body 和 reply_to 并列，不互相盖掉。
        assert_eq!(v["kind"], "text");
        assert_eq!(v["text"], "好");
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
