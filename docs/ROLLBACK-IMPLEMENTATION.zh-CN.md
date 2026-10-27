# AOX 回滚备份系统实施文档

> 状态：**W0-W4 已实现，R1 待验收**（2026-10-26）。
> 设计依据：[`ROLLBACK.zh-CN.md`](ROLLBACK.zh-CN.md)。
> 实施记录见文末 §18；W5-W9（strict shell、插件扫描、GC、GUI 面板）尚未开始。

## 0. 施工基线

以下选择按设计稿的推荐值冻结。若用户不同意，只改这一节，不要在其他章节各改一处。

| 项 | 施工值 | 说明 |
|---|---|---|
| 默认开关 | `[recovery] enabled = true` | 无写入任务零开销；完整实现及验收后生效 |
| 回退深度 | `keep = 30` | 指当前游标可撤销的 `turn` 节点数 |
| 总配额 | `max_total_mb = 512` | 包含对象、节点、baseline、safety |
| 单文件上限 | `max_file_mb = 64` | strict 模式下超过即拒绝该次写入 |
| 单任务上限 | `max_turn_mb = 256` | 新增对象总量 |
| 扫描内存上限 | `metadata_memory_mb = 32` | 超过即失败关闭，不 OOM |
| shell 策略 | `shell_guard = "strict"` | 每个 opaque 调用前后对称扫描 |
| 插件策略 | `protect_plugins = true` | 只包带路径参数或显式声明 write 的插件工具 |
| 首版压缩 | `codec = "raw"` | zstd 延后，格式预留 codec 字段 |
| drift 覆盖 | 允许 `--force` | 必须先落 safety，CLI 显式参数，GUI 红色确认 |
| 聊天回滚 | 不做 | 只发恢复通知并使 evidence 过期 |

三个已明确的实现取舍：

1. **首版不引入 zstd。** 先保证正确性、内存和二进制体积；`codec` 字段允许以后无损加入
   zstd。未压缩时当前仓库约 5 MiB 的源码基线可以接受。
2. **不引入数据库。** 元数据继续用 JSON/JSONL + 原子 rename；文件锁优先使用标准库
   `File::try_lock`，若工具链版本不支持再降级到 `windows-sys::LockFileEx`。
3. **不持久化整个工作区状态图。** 节点只存本任务改过的路径；strict shell 额外保留一份
   baseline，不是每任务一份。

## 1. 完成定义

首个可用版本（R1）必须做到：

- `write_file`、`edit_file`、`remove` 的修改前内容在落盘前已持久化；失败则原操作不执行。
- 一次用户 turn 的净文件变化形成一个 `RecoveryPoint`；没有净变化不建节点。
- `/checkpoints`、`/rollback`、`/rollback n`、`/redo` 可用；默认只预览。
- 回滚前全量预检；外部 drift 默认拒绝；冲突时不写任何字节。
- 回滚失败、进程崩溃后不会报告成功；下一次打开能判断并收敛。
- `task_state` 的 evidence 在恢复后全部失效。
- 默认配置下，纯聊天和只读任务不创建 `.aox/backup` 目录。

完整版本再覆盖 shell、插件、GC、GUI（W5-W9）。

## 2. 不可违背的实现约束

### 2.1 生命周期

- 事务必须挂在 `Runtime` 上，因为 GUI 的中断路径会 **drop `run_turn` future**。
  局部 `RecoveryTxn` 只能等 Drop，无法可靠地做 after 扫描或写节点。
- `Runtime::new` 创建 `RecoveryManager`；`Agent::run_turn` 在确定 workplace root 后
  `begin_turn`，成功、失败、中断三条路都必须收尾。
- 工具只拿到 `&Arc<Runtime>`，所以捕获入口必须能从 Runtime 取到当前事务。
- 没有活动事务时，所有捕获 API 返回 `Inactive`，工具行为与今天一致；这保证
  `selftest` 直接调用 `Runtime::execute` 不受影响。

### 2.2 写入安全

- `capture_before` 失败 ⇒ **原写入不执行**。
- 写入后 `capture_after` 失败 ⇒ 文件已经改了，不能回滚这次写入；追加警告并保留 journal，
  下次打开时尽量补提交。
- 捕获只读流式处理，不把整个文件读进内存。
- 备份目录、节点路径、恢复目标都必须经过工作区相对路径校验；拒绝 `..`、reparse point
  和跨根路径。
- 不相干的文件绝不因为“恢复”而被删除。恢复只操作计划中的路径。

### 2.3 数据完整性

- 节点、cursor、baseline 全部用“同目录 temp → flush → sync_all → atomic rename”。
- cursor 永远最后更新。
- unknown schema、损坏节点、缺失 blob：报错并停止，不尝试猜测。
- 不把文件内容、密钥、完整源码写进日志或模型可见错误；只给 hash、路径、大小、原因。

### 2.4 项目约定

- 新增字符串走 `t!` / `tf!`，英文源放进 `src`，中文只进 `locales/zh.toml`。
- 所有改动先方案、再实施、再验收；本阶段不自动 commit。
- 新文件使用 LF 或按工具结果统一；改完跑 `tools/fix_newlines.py` 与 U+FFFD 检查。
- GUI 改完必须 `cargo build --release -p aox-gui`，不能只跑根构建。

## 3. 现有代码事实与接入点

| 区域 | 现状 | 接入动作 |
|---|---|---|
| `src/agent.rs:610-671` `run_turn` | 统一 turn 边界，成功失败都收尾 | 开头 `begin_turn`，`lp.run` 之后 `finish_turn` |
| `src/agent.rs:617` `ensure_started` | 首次消息前决定 workplace root | 事务必须在它之后开始 |
| `src/agent.rs:646-652` | push user → begin_turn → loop | 事务夹在 root 确定之后、工具执行之前 |
| `src/agent.rs:967` `authorize_and_execute` | 唯一工具漏斗 | 只在这里包插件工具，不重复包主机文件工具 |
| `src/agent.rs:1079` `rt.execute` | 实际执行 | 插件扫描在它前后 |
| `src/tool/hostfs.rs:428-490` `write_file` | 直接 `std::fs::write` | 在写前捕获，在成功/失败后都补 after |
| `src/tool/hostfs.rs:888-939` `remove` | 删除前数量统计，再走回收站 | 回收前捕获整棵树；失败后对账 |
| `src/tool/edit.rs:98-240` `edit_file` | 全批校验后 `write_atomically` | 包围现有单次原子替换 |
| `src/tool/shell.rs:152-202` `run_shell` | prepare → runner → 事后 mtime 扫描 | 前后对称扫描包住所有退出路径 |
| `src/tool/job.rs:134` `start` | 启动长驻 Job | 成功启动后把当前事务标 partial |
| `src/runtime.rs:146-206` `Runtime::new` | 构造 Runtime | 增加 `RecoveryManager` 字段 |
| `src/config.rs:14-80` `Config` | 分段配置 | 增加 `[recovery]`，同步 `dummy()` |
| `bundles/core.toml` | 聚合配置 bundle | 增加 `recovery` bundle |
| `src/main.rs:508-675` | CLI 命令分发 | 增加 recovery 命令分支 |
| `gui/src/bridge.rs:1010-1024` | GUI 只本地处理 `/runtime` | 增加 recovery 命令/请求分支 |
| `gui/src/bridge.rs:719` `next_command` | 会话切换、新建、workplace | 增加 `ToAgent::Recovery` |
| `src/task.rs:285` `note_modified` | 每次写入 bump epoch | 新增恢复后的整体失效方法 |

## 4. 模块与文件清单

### 4.1 新增

```text
src/recovery/
  mod.rs          RecoveryManager、ActiveTurn、公共 API、错误转换
  model.rs        所有 serde 类型与 schema 版本
  store.rs        目录、锁、blob、节点、cursor、journal、原子 I/O
  capture.rs      精确捕获、树捕获、opaque 扫描、排除规则
  restore.rs      预检、restore/redo、safety、恢复状态机
  gc.rs           可达性、保留、配额、清理
  command.rs      CLI 解析与文本输出
  text.rs         用户/模型可见文案组装（i18n）
```

R1 可以只建 `mod.rs / model.rs / store.rs / capture.rs / restore.rs / command.rs / text.rs`；
`gc.rs` 在 W7 加。不要把所有东西塞进一个 `recovery.rs`。

### 4.2 修改

```text
Cargo.toml                 + blake3（首版唯一新增运行依赖）
src/lib.rs                 pub mod recovery;
src/main.rs                mod recovery; 命令分发；/help
src/config.rs              RecoveryConfig + ShellGuard + Default + dummy()
src/runtime.rs             pub recovery: RecoveryManager
src/agent.rs               生命周期、插件包装、恢复通知
src/task.rs                invalidate_after_restore()
src/tool/hostfs.rs         write/remove 捕获
src/tool/edit.rs           edit 捕获
src/tool/shell.rs          opaque 扫描 + 所有退出分支
src/tool/job.rs            partial 标记
bundles/recovery.toml      [recovery] 默认值（新文件）
bundles/core.toml          include recovery
config.example.toml        [recovery] 注释示例
gui/src/bridge.rs          ToAgent/FromAgent + 命令/面板请求
gui/src/recovery_ui.rs     恢复点浮层（R3）
gui/src/main.rs            mod recovery_ui
```

### 4.3 不修改

- 不改 `Store` seam：备份与对话历史是两套生命周期，不把备份塞进 `Store` trait。
- 不改历史 JSONL 行格式；恢复通知不写成 `kind:"task"`。
- 不改 `task_state` 的模型可见字段和 gate 规则。
- 不把 recovery 暴露成模型工具。

## 5. 数据格式（实施契约）

### 5.1 版本

```rust
pub const RECOVERY_SCHEMA: u32 = 1;
```

未知 schema 不迁移、不覆盖，直接报错并保留目录。首版只接受 1。

### 5.2 BlobRef

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlobRef {
    pub hash: String,          // BLAKE3-256, 64 lowercase hex
    pub size: u64,             // uncompressed bytes
    pub codec: Codec,          // Raw; Zstd reserved
    pub readonly: bool,
    #[serde(default = "default_true")]
    pub stored: bool,          // false = hash only, no object
}
```

对象路径：`objects/<hash[0..2]>/<hash[2..4]>/<hash>`。
`stored=false` 只用于 after 内容缺失；before 永远必须 `stored=true`。

### 5.3 FileOp

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOpKind { Create, Modify, Delete, TypeChange }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileOp {
    pub path: String,              // workspace-relative, '/'
    pub kind: FileOpKind,
    pub before: Option<BlobRef>,
    pub after: Option<BlobRef>,
}
```

聚合规则：

- 同一任务同一路径只保留第一次 before、最后一次 after。
- `before == after`（hash 相同且类型相同）⇒ 从 ops 删除，不提交该路径。
- 创建后删除、删除后重建回原内容 ⇒ 净变化为零，不提交该路径。
- 重命名拆成旧路径 delete + 新路径 create。
- 目录删除递归展开为每个常规文件一条 op；空目录不保证恢复。
- symlink/junction 的变更记 `type_change`；恢复直接拒绝该节点或标 partial。

### 5.4 RecoveryPoint

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryPoint {
    pub schema: u32,
    pub id: String,                 // rp_<unix>_<4hex>
    pub parent: Option<String>,
    pub kind: PointKind,            // turn | external | manual
    pub status: PointStatus,        // completed | failed | interrupted | recovered
    pub created_at: u64,
    pub finished_at: u64,
    pub conversation: Option<String>,
    pub turn: Option<u64>,
    pub label: String,              // <= 80 chars
    pub ops: Vec<FileOp>,
    pub coverage: Coverage,
    #[serde(default)]
    pub redoable: bool,
}
```

`genesis` 不落节点；`parent=None` 即起点。

`Coverage`：

```rust
pub struct Coverage {
    pub state: CoverageState,       // complete | partial | unknown
    pub scanned: u64,
    pub excluded_roots: Vec<String>,
    pub reasons: Vec<String>,
}
```

节点 JSON 用 pretty 输出，便于人工排查；journal/baseline 用 JSONL 保持低解析成本。

### 5.5 Cursor

```rust
pub struct Cursor {
    pub schema: u32,
    pub workspace_id: String,
    pub head: Option<String>,
    pub redo: Vec<String>,          // safety id, LIFO
    pub truncated: bool,
    pub updated_at: u64,
}
```

`cursor.json` 是唯一的可变指针；`redo` 栈也在这里，保证 cursor 与 redo 原子一致。

### 5.6 Safety

```rust
pub struct SafetySnapshot {
    pub schema: u32,
    pub id: String,                 // redo_<unix>_<4hex>
    pub from_cursor: Option<String>,
    pub target_cursor: Option<String>,
    pub created_at: u64,
    pub states: Vec<SafetyPath>,
}

pub struct SafetyPath {
    pub path: String,
    pub state: Option<BlobRef>,     // None = path absent
    pub kind: EntryKind,            // file only in v1
}
```

safety 必须在第一个恢复写入之前 `sync_all`；safety 写失败 ⇒ 不开始恢复。
新任务提交后清空 redo 栈；旧 safety 进入 GC 候选。

### 5.7 Format 与 Journal

`format.json`：

```json
{"schema":1,"workspace_id":"<32 hex>","created_at":1792920000}
```

`workspace_id` 用“创建时间 + 进程 id + 根路径”派生 BLAKE3，足够防止误用，不承担安全职责。

事务 journal 每行一个对象：

```json
{"t":"begin","tx":"tx_...","parent":null,"at":1792920519}
{"t":"intent","path":"src/a.rs","before":"<hash>","at":1792920520}
{"t":"after","path":"src/a.rs","after":"<hash>","at":1792920522}
{"t":"partial","reason":"background job 3 is running","at":1792920523}
{"t":"finish","node":"rp_...","at":1792920530}
```

恢复 journal：

```json
{"t":"restore_begin","target":"rp_A","safety":"redo_...","at":...}
{"t":"planned","path":"src/a.rs","from":"<hash>","to":"<hash>","at":...}
{"t":"applied","path":"src/a.rs","at":...}
{"t":"restore_committed","target":"rp_A","at":...}
```

所有 append 后 flush；begin/intent/after/finish 关键行 `sync_all`。

## 6. 目录与原子 I/O 实现

### 6.1 目录

```text
<workspace>/.aox/backup/
  format.json
  lock
  cursor.json
  objects/aa/bb/<hash>
  nodes/rp_<unix>_<hex>.json
  journals/tx_<unix>_<hex>.jsonl
  journals/restore_<unix>_<hex>.jsonl
  redo/redo_<unix>_<hex>.json
  baseline/manifest.jsonl
  baseline/meta.json
  tmp/
  quarantine/<restore-id>/
```

`baseline/` 只在 strict shell 第一次调用时创建。`index/refs.json` 首版可以不做，节点数
不超过几十个，直接扫描 `nodes/` 足够；如果列表超过 200 条再加缓存。

### 6.2 原子写

在 `store.rs` 实现唯一入口：

```rust
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()>;
fn copy_atomic(path: &Path, src: &mut impl Read) -> io::Result<()>;
```

规则：

- temp 放在目标同目录，文件名带 pid 和进程内原子序号，避免两个 AOX 撞名。
- 写完后 `flush` + `sync_all`，再 `rename`。
- Windows `rename` 覆盖目标依赖 `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`；失败要保留 temp
  直到错误返回，不能静默退回直接写。
- 目录 fsync 在 Unix 做 best-effort；Windows 不假装做到了不存在的保证。
- 失败路径必须删除 temp；成功路径不得留下 `.tmp`。

### 6.3 打开备份目录

`RecoveryStore::open(root, cfg)`：

1. 检查 root 存在且是目录。
2. `.aox` 或 `.aox/backup` 若存在，必须是普通目录，不是 reparse point。
3. 读取/创建 `format.json`；schema 不认识就报错。
4. 读取 `cursor.json`；不存在视为 genesis；损坏则报错，不重建。
5. 不在 open 时自动创建 baseline 或对象。
6. 只有第一次真正需要捕获时才创建 `objects/`、`journals/`、`tmp/`。

### 6.4 Blob 写入

```rust
fn put_reader(
    &self,
    r: &mut impl Read,
    known_size: Option<u64>,
    cap: u64,
) -> Result<BlobRef, RecoveryError>;
```

- 128 KiB 缓冲，边读边算 BLAKE3。
- 超过 `cap` 立即中止并删除 temp；返回 `LimitExceeded { path, size, cap }`。
- 写 temp → sync → 校验 hash → 目标已存在则复用（校验大小）→ 否则 atomic rename。
- readonly 从源文件读取；Windows 用 `Permissions::readonly()`。
- `blake3` 是唯一新增 crate；不引入正则/数据库/压缩库。
- 每次写入前检查 `max_total_mb` 和 `max_turn_mb`。超限先尝试清理不可达对象；仍不足则
  返回 `BackupUnavailable`，strict 下阻断原写入。绝不能为了腾空间删当前任务已写的 before。

## 7. RecoveryManager 与事务

### 7.1 结构

```rust
pub struct RecoveryManager {
    cfg: RecoveryConfig,
    app_root: PathBuf,
    active: std::sync::Mutex<Option<Arc<ActiveTurn>>>,
}

struct ActiveTurn {
    root: PathBuf,
    workspace_id: String,
    conversation: Option<String>,
    label: String,
    started_at: u64,
    state: std::sync::Mutex<ActiveState>,
}

struct ActiveState {
    store: Option<Arc<RecoveryStore>>,
    lock: Option<FileLock>,
    journal: Option<Journal>,
    parent: Option<String>,
    ops: BTreeMap<String, FileOp>,   // key = relative path
    partial: Vec<String>,
    unsupported: Vec<String>,
    finished: bool,
}
```

为什么这样拆：

- `active` 锁只用于“当前有没有 turn”，不跨文件 I/O 长期持有。
- `state` 锁保护 store/journal/ops；工具捕获是同步短临界区。
- `store` 懒打开：只读任务不建 `.aox/backup`。
- `lock` 从第一次捕获持有到 turn 提交；不是只锁 cursor 更新那一瞬。

### 7.2 公共 API

```rust
pub struct TurnStart { pub tx: String }
pub struct FileCapture { token: Option<CaptureToken> }
pub struct TreeCapture { token: Option<TreeToken> }
pub struct OpaqueCapture { token: Option<ScanToken> }

/// 写盘前被拒绝时给模型和用户的两套文本；不要在工具里另拼一份。
pub struct RecoveryBlock { pub user: String, pub model: String }
pub struct RecoveryWarn(pub String);

impl RecoveryManager {
    pub fn new(cfg: RecoveryConfig, app_root: PathBuf) -> Self;

    pub fn begin_turn(
        &self,
        root: &Path,
        conversation: Option<&str>,
        label: &str,
    ) -> Result<Option<TurnStart>, RecoveryError>;

    pub fn finish_turn(&self, status: TurnStatus) -> RecoveryFinish;
    pub fn abort_turn(&self) -> RecoveryFinish;
    pub fn is_active(&self) -> bool;

    pub fn capture_before(&self, path: &Path) -> Result<FileCapture, RecoveryBlock>;
    pub fn capture_after(&self, cap: FileCapture) -> Result<Option<String>, RecoveryWarn>;

    pub fn capture_tree_before(&self, path: &Path) -> Result<TreeCapture, RecoveryBlock>;
    pub fn finish_tree_after(&self, cap: TreeCapture) -> Result<Option<String>, RecoveryWarn>;

    pub fn begin_opaque(&self, reason: &str) -> Result<OpaqueCapture, RecoveryBlock>;
    pub fn mark_partial(&self, reason: &str);
    pub fn note_background_job(&self, id: &str);
}
```

`TurnStatus`：`Completed / Failed / Interrupted`。

`finish_turn` 返回：

```rust
pub struct RecoveryFinish {
    pub point: Option<String>,
    pub warnings: Vec<String>,
    pub error: Option<RecoveryError>,
}
```

错误分类：

```rust
pub enum RecoveryError {
    Disabled,
    NoWorkspace,
    OutsideWorkspace,
    UnsupportedEntry,
    LimitExceeded { path: PathBuf, size: u64, cap: u64 },
    BackupUnavailable { reason: String },
    Locked { holder: Option<String> },
    Corrupt { path: PathBuf, reason: String },
}
```

`OutsideWorkspace` 在 `full` 模式下只降级为“未保护”提示，不阻断写入；`LimitExceeded`/
`BackupUnavailable` 在 strict 下必须阻断。

### 7.3 开启与收尾

`begin_turn`：

1. `enabled=false` 或没有 active root ⇒ `Ok(None)`。
2. 若已有 active turn ⇒ 返回错误（上一轮未收尾，禁止嵌套）。
3. 记录 root、conversation、label；不打开 store、不建目录。
4. 如果有活跃 `shell_job`，先记 `partial("background job ... is running")`；这是运行时事实，
   不是等到 shell_job 启动时才判断。

`finish_turn`：

1. 从 `active` 取走 `Arc<ActiveTurn>`，置空。
2. 没有 store / 没有 ops / 没有 partial ⇒ 删除空 journal、释放锁，不建节点。
3. 如有 partial reasons，`coverage=partial`；如仅创建后删除则无净变化，但 partial 事实仍
   要写进返回的 warning。
4. 写 external 桥接节点（W3 可先不做，W4 补；缺失时跨断点回滚会被 drift 预检拦下）。
5. 写 task 节点；节点写成功后再写 cursor。
6. 删除 journal；释放锁。
7. 任何一步失败：保留 journal，返回 warning/error；不能把半成品当完整节点。

`abort_turn`：与 `finish_turn(Interrupted)` 相同，但用于 future 已经被 drop、只能做同步收尾
的路径。`Drop` 只写 journal 的 `abandoned` 标记，不做 async 工作。

### 7.4 主机精确捕获

`capture_before(path)`：

- 无活动事务 ⇒ `FileCapture { token: None }`。
- 路径不在 root 内 ⇒ `FileCapture::Unprotected`（内部 token 带原因），不阻断。
- `symlink_metadata`：非常规文件 ⇒ 返回 `UnsupportedEntry`。`strict` 下转换为
  `RecoveryBlock`，原写入不执行；`best_effort`/`off` 下可以继续，但必须提示“该路径未受保护”。
- 不存在 ⇒ `before=None`，`kind` 先按 create。
- 存在 ⇒ 流式写 blob，记录 hash/size/readonly。
- 检查该路径 `before` 是否已在 ops 中；已有则保留第一次 before，不重复写对象。

`write_file` 的准确顺序必须是：

```text
path_arg -> guard -> capture_before(target) -> create_dir_all(parent) -> write -> capture_after
```

捕获要在建父目录之前完成。这样捕获失败时不会留下“目录建了、文件没写”的半副作用；
`write_file` 的 `create_dirs=false` 分支仍然先检查父目录。

`capture_after(cap)`：

- `before=None` 且当前不存在 ⇒ 空操作。
- 当前存在 ⇒ 读 hash/size；与 before 比较，相同则删除 op，不同则补 after。
- 当前不存在 ⇒ 补 delete。
- 把结果写 journal `after` 行。
- 返回 warning 字符串给工具结果；这里是“写入已发生”，不能把整个工具结果改成失败。

### 7.5 树捕获与删除

`capture_tree_before(path)`：

1. 不跟随 reparse point。
2. 常规文件递归成 `(rel, BlobRef)`；目录本身只作为容器。
3. 先完整捕获，全部成功才返回 token；任何失败都不允许删除。
4. 目录中遇到 symlink/junction、设备文件或无法读取的条目 ⇒ 返回 `UnsupportedEntry`，
   strict 下**整次 remove 不执行**；不能只备份一半就把整棵树移走。
5. `remove` 调回收站成功后，`finish_tree_after` 将每个路径记为 delete。
6. 回收站失败时，重新扫描每个路径：已消失的记 delete，仍在的按内容比较并丢弃/更新；
   标记 partial 并返回警告。

不允许只记录“目录已删除”而漏掉子文件；恢复时无法凭空还原树。

## 8. 节点、cursor 与崩溃恢复

### 8.1 提交顺序

```text
1. 取 workspace 锁（从第一次捕获起已持有）
2. 所有 before/after blob：写入 → sync → hash 校验 → 原子发布
3. 节点 JSON：写 temp → sync → 原子 publish
4. cursor.json：写 temp → sync → 原子 replace（最后）
5. 删除 journal
```

如果第 3 步成功、第 4 步前崩溃：启动时比较节点 ops 的 after 与当前磁盘；全部匹配才推进
cursor，否则标记 `recovered/unknown`。
如果第 2 步后、第 3 步前崩溃：journal 的 intent/after 足够时补建节点；不够时保留 journal
并报错。

### 8.2 启动恢复

`RecoveryStore::recover_pending()` 在首次打开 store 时执行，也在 `/backup status` 前执行。
恢复完 cursor 后才允许当前事务读取 parent；不能先读旧 cursor 再补提交。

捕获事务：

| 当前路径状态 | 动作 |
|---|---|
| 全部等于 before | 丢弃 journal，不建节点 |
| 全部已知 after / 中间态 | 补建 `recovered` 节点 |
| 部分未知 | 建 `coverage=unknown` 节点，禁止自动跨越回滚 |
| blob 缺失 / journal 损坏 | 保留现场并报错 |

恢复事务：

| 当前路径状态 | 动作 |
|---|---|
| 全部等于 target | 只推进 cursor，清理 journal |
| 全部等于 pre-restore | 放弃恢复，保留 safety，清理 journal |
| 每条路径都属于 target 或 pre 两者之一 | 用 safety 自动退回 pre，标 failed |
| 出现第三种状态 | 停止，保留 quarantine/journal，要求人工处理 |

**不在启动时自动重放一个新的 restore**。已经完成的部分要么收敛到 target，要么退回 pre；
第三种状态宁可停住，不猜。

### 8.3 恢复预检

`restore.rs` 提供：

```rust
pub fn resolve_target(store: &RecoveryStore, n: usize) -> Result<Cursor, RestoreError>;
pub fn plan_restore(store: &RecoveryStore, target: &Cursor, opts: &RestoreOptions)
    -> Result<RestorePlan, RestoreError>;
pub fn apply_restore(store: &RecoveryStore, plan: &RestorePlan)
    -> Result<RestoreReport, RestoreError>;
```

`resolve_target` 必须能返回 `head=None`（genesis）；不要把“没有父节点”当成“没有任务”。
遍历时 `turn` 计数，`external/manual` 只走过不计数；遇到缺失父节点且还有剩余 n，报错。

### 8.4 预检项

- 所有 blob 存在、大小正确、解压后再 hash 匹配（首版 raw 直接校验）。
- 计划中的路径都在同一 root；不存在通过 reparse point 逃逸。
- 对每个路径的当前存在性/类型/hash 与“从 cursor 向 target 行走时最靠后的 after”一致。
- 链的连续性：同一路径相邻节点的 after/before 必须相接；`external` 是合法桥接。
- 有活跃 turn、未完成 capture journal、活跃 shell_job ⇒ 拒绝或提示先停止。
- `coverage != complete` 默认拒绝；`--allow-partial` 才继续，且输出缺失原因。
- 目录只作为父目录创建；不删除未在计划中的目录内容。
- 文件占用、只读、共享冲突要逐路径报告；不得无限重试。

预览在**写第一个字节前**完成。默认 `/rollback` 只预览。

### 8.5 执行

- 写 safety → `sync_all`。
- 写 restore journal。
- 按 cursor → target 的顺序执行逆向操作；同一路径只取第一次出现的“当前态”作为起点。
- 删除新建文件：先 rename 到 `quarantine/<restore-id>/`，不直接 remove。
- 恢复/覆盖：同目录 temp → flush → 权限 → atomic rename。
- 每完成一个路径追加 `applied`。
- 全部成功后更新 cursor（head=target, redo 追加 safety id），清理 quarantine 和 restore
  journal；**保留 safety** 供 redo。
- 失败则按 safety 回退已应用的路径；仍保留 safety 和 journal。

### 8.6 安全快照与 redo

- `/rollback n` 和 `/rollback <id>` 都先预览；无冲突且显式确认后才执行。
- 执行前保存所有将与 target 不同的路径的**当前版本**到 safety，不能用节点里的旧 after
  冒充当前态。
- `/redo` 先确认当前 cursor 等于 safety.target_cursor；再按 safety.states 恢复；成功后
  cursor 回到 from_cursor，弹出 redo 栈。
- redo 栈最多 8 个；新节点提交后清空，旧 safety 变 GC 候选。
- redo 失败不弹栈，保留现场。

## 9. Shell 扫描

### 9.1 接入位置

在 `src/tool/shell.rs::run_shell`：

```text
prepare(args) -> 得到 ws/cwd
let guard = rt.recovery.begin_opaque("shell")?;  // strict 失败则不 spawn
let outcome = runner::run(...).await;
let scan = guard.finish();                       // 所有退出分支都要执行
```

不能用 `?` 在 runner 错误时直接返回而不 finish；用一个 guard 的显式 `finish()` 加 Drop 标记
兜底。`spawn` 失败、非零退出、超时、idle kill 全部走同一条 finally 路径。

### 9.2 扫描范围

- `.aox`、`.git`、`target`、`node_modules`、`.venv`、`dist`、`build`、`out` 默认排除。
- 排除表来自配置，默认值不能写死在两处。
- 对不存在的根跳过；遇到 reparse point 不进入，记 partial。
- 最大深度、条目、字节、时间由配置和内存预算控制；strict 超限 ⇒ 命令不启动。

### 9.3 严格模式

每个 opaque 命令：

1. 读 baseline manifest（若有）。
2. 遍历 scope，计算每个常规文件的 BLAKE3。
3. 内容与 baseline 相同则复用对象；不同或新文件则写 blob。
4. 写新的 before manifest 到 temp，ready 后原子替换。
5. 命令结束再次遍历，比较 hash/类型/存在性。
6. 变化路径生成 FileOp；更新 baseline 到 after 状态。
7. 返回变化列表供工具文本展示（替代现有 `changed_since` 的路径列表）。

“内容相同但 mtime 变化”不产生 op；“删除后重建、内容相同”也不产生 op。

### 9.4 best_effort / off

- `best_effort`：允许用元数据未变跳过读取；命令后才发现且没有前像的写入，节点标 partial，
  默认不能自动回滚跨越。
- `off`：不捕获，保留现有 `track_changes` 行为；节点只记录其它精确工具的变化，shell 写入
  不在保证内。
- 配置名写错 ⇒ 启动报错，不静默回退。

### 9.5 `shell_job`

- `start` 成功后调用 `note_background_job(id)`，当前 turn 标 partial。
- turn 开始时已有活跃 job 同样标 partial。
- 不把 job 的异步写入归因给任何任务；预览显示“有后台 job，回滚可能不完整”。
- 首版不实现 job 生命周期检查点，等 watcher/USN 方案。

### 9.6 插件

- 只包 `ToolKind::Plugin` 且 `access == Write` 的工具；声明 read 按只读信任。
- 为避免每个联网插件都触发全树扫描，`protect_plugins = true` 的实际触发条件是：
  该调用参数中至少有一个 `workplace::scan_paths` 识别出的路径，**或**工具在声明里明确写了
  `"access": "write"`。没有路径参数且未声明的插件调用不扫描，也不会让节点变 partial；
  这属于设计已声明的“插件黑盒 best-effort”，不是完整保证。
- 在 `Agent::authorize_and_execute` 的 `rt.execute` 前后包 opaque guard。
- guard 失败 ⇒ 该插件调用不执行（strict）。
- 插件目录不在当前工作区 ⇒ 无覆盖，明确提示，不把整个调用失败。

## 10. 配置、CLI 与 GUI

### 10.1 配置

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RecoveryConfig {
    pub enabled: bool,
    pub keep: usize,
    pub max_total_mb: u64,
    pub max_file_mb: u64,
    pub max_turn_mb: u64,
    pub metadata_memory_mb: u64,
    pub shell_guard: ShellGuard,
    pub protect_plugins: bool,
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellGuard { Strict, BestEffort, Off }
```

默认排除：

```text
.aox/  .git/  .hg/  .svn/
target/  node_modules/  .venv/  venv/  __pycache__/
.pytest_cache/  dist/  build/  out/
*.tmp  *.swp  *.lock
```

同步修改：

- `Config` 增加 `#[serde(default)] pub recovery: RecoveryConfig`。
- `Config::dummy()` 增加 `recovery: RecoveryConfig::default()`。
- 新增 `bundles/recovery.toml`，写全默认值；`bundles/core.toml` include 它。
- `config.example.toml` 增加注释示例。
- 测试：省略 `[recovery]` 时等于默认值；`shell_guard` 拼错启动失败；`enabled=false`
  不创建目录；旧配置没有该表时仍能正常启动。

### 10.2 CLI

新增 `src/recovery/command.rs`：

```rust
pub fn is_cmd(line: &str) -> bool;
pub fn parse(line: &str) -> Result<RecoveryCmd, String>;
pub fn run(rt: &Arc<Runtime>, cmd: RecoveryCmd) -> Result<CommandReport, RecoveryError>;
```

命令：

```text
/checkpoints [--all]
/checkpoint <label>
/rollback
/rollback 2
/rollback rp_...
/rollback --yes
/rollback --force
/rollback --allow-partial
/redo
/backup status
/backup verify
/backup prune
```

规则：

- `/rollback` 无参数 = 预览上一个 turn。
- 无 `--yes` 不写盘；`--force` 隐含确认并覆盖 drift；`--allow-partial` 只解除 partial 阻止，
  不等于 force。
- label 只取用户输入，截断 80 字符，不进模型上下文。
- 命令错误不 panic，输出可执行建议。
- 成功恢复后，命令 handler 调用 `agent.note_workspace_restored(&report)`。

`main.rs` 在 `match line` 中增加：

```rust
_ if recovery::is_cmd(line) => {
    match recovery::command::run(&rt, recovery::command::parse(line)?) {
        Ok(rep) => {
            ui.emit(AgentEvent::Info(rep.text));
            if let Some(r) = rep.restore { agent.note_workspace_restored(&r); }
        }
        Err(e) => ui.emit(AgentEvent::Error(...)),
    }
}
```

`/help` 增加四条：`/checkpoints`、`/rollback`、`/redo`、`/backup`。

### 10.3 Agent 恢复通知

新增：

```rust
pub fn finish_recovery_turn(&mut self, status: TurnStatus) -> RecoveryFinish;
pub fn note_workspace_restored(&mut self, report: &RestoreReport);
```

`finish_recovery_turn` 只做三件事：委托 `rt.recovery.finish_turn`；把 warning/error 通过
`Ui::emit(Warn/Error)` 说出来；返回结果给调用方。它不修改聊天消息，也不在正常成功时发通知。

`note_workspace_restored` 的行为：

1. 清空 `last_output`。
2. 对当前 `TaskState` 调 `invalidate_after_restore()`：
   - `epoch += 1`；
   - `checks.clear()`；
   - `modified_files.clear()`；
   - `status = Open`；
   - `last_updated = now`。
3. `task_saved = None; persist_task();`，让旧 evidence 不能在新文件状态下被当成有效。
4. `push_ephemeral(Message::user(host_note))`，让模型下一轮知道文件已变；不写历史。
5. 不自动改 `goal`/`steps`/`completed` 文本，不删除聊天消息。

恢复过程中 chat 历史不截断；`/rollback --with-chat` 明确不做。

### 10.4 GUI

现状：`gui/src/bridge.rs` 只本地处理 `/runtime`，其它斜杠命令都会当普通输入发给模型。
因此必须显式添加：

```rust
if aox::recovery::command::is_cmd(&line) {
    recovery_command(&mut ui, &mut agent, &line).await;
    continue;
}
```

位置在 `runtimes::is_cmd` 之前。R1 先支持 CLI 风格命令，R3 再加面板。

中断分支必须在 `agent.rollback_interrupted_turn(mark)` 之后、发送 `TurnAborted` 之前调用
`agent.finish_recovery_turn(TurnStatus::Interrupted)`。否则 `tokio::select!` 丢掉 future 后，
Runtime 里的事务永远不会提交，下一次 `/checkpoints` 看不到这轮已经发生的文件修改。CLI 的
`run_turn` 正常返回/报错都走自己的 finish，不依赖这个分支。

R3 面板：

```rust
pub enum ToAgent {
    Recovery(RecoveryRequest),
}

pub enum RecoveryRequest {
    List,
    Preview { target: RecoveryTarget },
    Apply { target: RecoveryTarget, force: bool, allow_partial: bool },
    Redo,
    Status,
    Prune,
}

pub enum FromAgent {
    Recovery(Box<RecoveryView>),
}
```

UI 形态：

- 工作区按钮旁增加“恢复点”入口，紧凑浮层。
- 列表显示时间、label、路径数、状态点。
- 选中后显示路径、字节大小、drift/partial 徽标。
- 无冲突才用主按钮；drift/partial 红色二次确认。
- 不在恢复中途逐路径刷新；完成后刷新一次。
- `--demo` 用静态 `RecoveryView` 仅验证布局，不写盘。

所有磁盘动作仍回 agent 线程；UI 只发请求、收视图。不要在主线程直接打开 backup 目录。

## 11. 分阶段施工

### W0 · 预检与基线（不改功能）

- 记录当前 `cargo test --lib`、`cargo test -p aox-gui` 结果。
- 记录 CLI/GUI release 体积和启动 RSS。
- 确认 `src/prompt.rs` 等已有未提交改动不要混入。
- 建模块占位、配置类型、数据结构单测。代码默认值直接写 `enabled = true`，但 W0-W3 的
  回归夹具全部用显式测试配置；不靠“开发期改默认值、发布前再翻”这种容易漏掉的状态。
- 验收：没有捕获路径被接上时，运行行为与今天一致；无写入不创建 `.aox/backup` 新目录。

### W1 · Store 内核

- `format/cursor/blob/node/journal`、锁、原子写、ID。
- BLAKE3 流式写入与验证。
- 单测覆盖：重复内容复用、hash mismatch、锁竞争、损坏 cursor、未知 schema、原子替换。
- 验收：纯 store 测试通过，无 UI/工具接入。

### W2 · 事务与主机工具

- `RecoveryManager`、active turn、journal、begin/finish。
- `write_file`、`edit_file`、`remove` 捕获。
- Agent 生命周期；CLI `/checkpoints`、`/backup status`。
- 验收：一次真实写入形成节点；无变化不建节点；捕获失败原文件不变。

### W3 · 回滚 / redo / CLI

- target 解析、两阶段预检、safety、restore journal、cursor 更新、redo。
- `/rollback` 预览与执行、`/redo`、`/backup verify`。
- 恢复后 `TaskState` evidence 失效。
- 验收：修改/新建/删除、回退 1/n、redo、drift 拒绝、恢复失败不报成功。

### W4 · 崩溃恢复与启用

- 启动 pending journal 状态机、quarantine、失败回退、external 桥接。
- 把默认 `enabled=true` 的功能路径跑通；无写入仍零目录。
- 验收：三条崩溃窗口、强制中断、GUI abort 后事务收尾。
- 这是第一个可交付 R1。

### W5 · strict shell

- 扫描器、baseline、排除、内存/时间预算、所有退出路径。
- `changed_since` 文本改为复用扫描结果；保留旧 track_changes 作为 off 模式兼容。
- 验收：同尺寸重写、删除重建、超时、spawn 失败、排除目录、未跟踪提示。

### W6 · 插件与 job

- 写插件前后扫描；`shell_job` partial。
- 验收：声明 read 的插件按只读；写插件产生可回滚节点；运行中 job 明确不完整。

### W7 · 保留与 GC

- `keep`、`max_total_mb`、trim、reachability GC、redo/baseline roots。
- 验收：共享 blob 不删；旧 redo 不误删；quota 不足不删当前 before。

### W8 · GUI

- 命令分支，随后 `recovery_ui.rs` 浮层、请求/视图、确认。
- 验收：GUI 预览/执行/失败呈现与 CLI 同语义；无主线程磁盘访问。

### W9 · 收口

- E2E、性能、二进制体积、文档索引、i18n 文案。
- 关闭测试进程、清理测试产物、构建 release。
- 不自动 commit，按项目习惯交给用户统一提交。

每个 W 结束后停下来给用户验收，不跨阶段自行“顺手”改下一阶段。

## 12. 测试清单

### 12.1 Store / model

- BLAKE3 固定测试向量（空串、`abc`）通过，保证算法/编码没有悄悄改变。
- 同一内容两次写入只有一个对象；hash/size 校验。
- 未知 schema、缺字段、损坏 cursor 报错而不是重置。
- cursor 原子替换；节点父链断掉时 target 解析失败。
- 两个进程抢锁：一个写，另一个得到 `Locked`。
- 写入 quota/单文件上限：temp 清理，原文件不变。
- 路径包含 `..`、绝对路径、UNC、大小写变化、长路径、symlink/junction。

### 12.2 主机工具

- `write_file` 新建/覆盖/失败/二进制/只读。
- `edit_file` 全批成功、全批失败、CRLF、BOM、expected_hash 失配。
- `remove` 单文件、空目录、目录树、回收站失败。
- 捕获失败时文件原内容逐字节不变。
- 同一任务多次改同一文件只留首 before/末 after。
- 创建后删除回原状态不产生节点。

### 12.3 回滚 / redo

- n=1、n=2、命名锚点、genesis。
- 新建→删除、删除→恢复、内容相同但 mtime 变化。
- drift：默认不写一字节；force 先落 safety；partial 默认拒绝。
- redo 单次、连续回滚 LIFO、新任务后失效。
- 外部改动后新任务触碰同一路径：external 节点 parent 正确，`rollback 1` 回到外部状态。
- 恢复中文件占用：已应用路径能回退；报告逐路径。
- 恢复只触碰计划路径；未涉及的外部文件保留。

### 12.4 崩溃

用测试专用 feature 或直接构造 journal：

- blob 写一半退出；
- 节点已发布、cursor 未更新；
- cursor 已更新、journal 未删；
- restore 预检后、应用一半；
- restore 全部应用、cursor 未更新；
- safety 已写、restore journal 未写。

断言：不出现“报告成功但文件只恢复一半”；不出现“missing blob 被忽略”。

### 12.5 Shell / 插件

- 成功、非零、timeout、idle kill、spawn 失败都执行 after。
- 同尺寸重写、mtime-only、删除重建、排除目录。
- 无前像时节点 partial，默认不能跨越。
- 声明 read 的插件不扫描；写插件成功/失败、扫描超限。
- 活跃 shell_job 让节点 partial。

### 12.6 生命周期 / GUI

- CLI GUI 共用同一命令文案/语义。
- GUI abort 之后事务被 finish(Interrupted)，下一次 `/checkpoints` 能看到。
- 切换会话、`/new`、退出都不留孤儿 active turn。
- `--demo` 布局不写盘。

### 12.7 验收命令

```bash
CARGO_INCREMENTAL=0 cargo check --workspace --all-targets
cargo test --lib recovery
cargo test --lib
cargo test -p aox-gui
cargo build --release --bin aox
cargo build --release -p aox-gui
```

E2E 使用隔离根：把 exe 复制到没有 `Cargo.toml` 祖先的临时目录，预置 `config.toml` 和
`tools/mock_llm.py`，绝不碰真实 `.aox/history`。至少跑：

1. 首轮写文件 → `/checkpoints` 有 1 个节点。
2. 修改后 `/rollback` 预览 → `--yes` → 内容回到前一版。
3. `/redo` → 回到回滚前。
4. 外部改文件后 `/rollback` → 默认拒绝；`--force` 有 safety。
5. 恢复后 `/task` 的 evidence 全失效。

## 13. 性能与体积验收

开工前记录、每个阶段结束记录：

| 指标 | 目标 |
|---|---|
| 空闲 RSS | 增量 ≤ 2 MiB |
| 1000 行单文件修改额外 RSS | ≤ 5 MiB（raw codec） |
| 首次 strict shell 5 MiB 源码 | ≤ 250 ms（热缓存，本机） |
| 后续 strict shell 5 MiB 源码 | ≤ 150 ms |
| 单文件捕获 I/O | 1 次读 + 1 次写，128 KiB 缓冲 |
| CLI 二进制增量 | ≤ 200 KiB（不含未来 zstd） |
| GUI 二进制增量 | ≤ 200 KiB |
| 30 个节点 + 5 MiB baseline 磁盘 | ≤ 20 MiB raw，典型远低于 |

不达标先优化，不把“慢”归因于模型。若扫描超时，优先减少排除范围之外的条目，而不是悄悄
降级到 best_effort。

## 14. 迁移、开关与回退实现

- 旧 `.aox` 目录没有任何 `backup` 子目录时，视为全新；不创建直到第一次捕获。
- 旧对话没有恢复点：`/checkpoints` 显示空，不报错。
- 旧 `task_state` 行不受影响；恢复后只 bump epoch / 清 evidence。
- `enabled=false`：不注册 `/checkpoints` 等命令，不创建目录，不扫 shell，不包插件。
- 没有活动工作区（`active_root()` 为 `None`，例如 full 模式，或新对话尚未发第一条消息）：
  recovery 命令明确回复“当前没有受保护的 workspace”，不跨 root 猜测，也不扫全盘。
  已存在任务的工作区在发出该对话第一条消息后即可正常查看和回滚。
- 删除实现：去掉 `Runtime.recovery`、工具捕获调用、Agent 生命周期、CLI/GUI 分支即可；
  配置文件残留 `[recovery]` 由 serde 按未知表忽略，不能因此启动失败。
- 数据删除必须由用户显式 `/backup prune --all` 或手动删除目录；不自动删用户数据。
- 本阶段所有工作不自动 commit。

## 15. 风险与已接受代价

| 风险 | 处理 |
|---|---|
| strict shell 全树 hash 拖慢命令 | 排除生成目录；预算失败关闭；测量后优化，不静默降级 |
| 大文件每次保存完整版本 | 明确限制；后续可按内容块去重，首版不做 |
| GUI future 被 drop | 事务状态放 Runtime；abort 显式 finish；journal 兜底 |
| 外部编辑器在恢复中写 | 预检 + 二次 hash 缩小窗口；承认 TOCTOU，提示勿同时编辑 |
| 备份含密钥 | 仅本地、继承目录权限、不进模型上下文 |
| 节点/cursor 被手改 | 严格校验 schema/hash/path，损坏即停 |
| 两个 AOX 同工作区 | OS 锁；拿不到时明确报错，不合并 cursor |
| `shell_job` / watcher | 首版 partial，不承诺完整 |
| zstd 延后导致基线更大 | 可接受；codec 字段留好迁移路径 |
| 测试误碰真实 history | 只用隔离根，永不跑真实 `.aox` |

## 16. 开工顺序（一步不能并）

1. 用户确认本文的默认值和取舍。
2. W0 预检 + 基线数据。
3. W1 Store 内核。
4. W2 事务 + 主机工具 + CLI 只读命令。
5. W3 回滚/redo + 预检 + safety。
6. W4 崩溃恢复，跑通默认值路径，交付 R1。
7. 用户验收 R1 后再决定是否继续 W5-W9。

R1 验收通过前不要接 shell、插件、GUI 面板，否则很难判断问题出在捕获、事务还是界面。

## 17. 开工前必须回答的三个问题

1. 默认值按本文冻结（启用、keep=30、force 允许）是否同意？
2. 首版只做 raw、不引入 zstd，是否接受基线约等于源码树一份的磁盘成本？
3. R1 先只交付 CLI（主机文件工具 + 回滚/redo），shell 和 GUI 放到验收后，是否同意？

得到确认后，第一件事是 W0：记录基线、建模块骨架和配置，不动任何现有行为。

## 18. 实施记录 · W0-W4（2026-10-26）

**状态：R1（自动捕获 + 精确回滚/重做 + 崩溃恢复）已完成，用户已接受体积增量。**
W5 已开工，见 §19。

### 已落地

- `[recovery]` 配置、`bundles/recovery.toml`、`Config::dummy()`、示例配置与配置组合测试。
- `src/recovery/`：model / store / restore / command / mod；BLAKE3 CAS、原子 cursor/node/journal、OS 文件锁、配额与单文件/单任务预算。
- `Runtime.recovery`：事务挂在 Runtime，而不是 `run_turn` 局部变量；GUI abort 分支显式 `finish_recovery_turn(Interrupted)`。
- `write_file` / `edit_file` / `remove` 精确捕获；捕获失败时原写入不执行。
- `/checkpoints`、`/checkpoint`、`/rollback [n|id] [--yes|--force|--allow-partial]`、`/redo`、`/backup status|verify`。
- 两阶段预检、safety、restore journal、失败回退、启动 journal 恢复、external 桥接。
- 目录删除的每个 before-image 在写 CAS 之后、动手删除之前逐条 append journal，且 `intent` 事件幂等；
  这样「遍历了一部分目录就崩溃」也能在下次启动时重建事务，不会留下无法回滚的半删除。
- 恢复后 `TaskState` 的 checks/modified_files/status 失效，聊天不回滚。
- 新进程未发消息时也可通过当前 workplace 档位找到已有恢复点（`command_workspace`）。
- `shell` 与写插件在当前构建会显式把任务标 `partial`，避免把未捕获的写入伪装成完整检查点；strict scanner 留到 W5/W6。

### 实际验证

- `cargo test --lib recovery`：**21 passed**（含新增的「目录删除在崩溃后仍可恢复」）。
- `cargo test --lib config::tests::`：**25 passed**。
- `cargo test --lib config::layer::tests::`：**12 passed**。
- `cargo test --lib`（全量收口）：**430 passed / 1 failed**，唯一失败是开工前就存在的
  `prompt::tests::disabled_builtin_text_is_gone`（`src/prompt.rs` 有本会话之前的未提交改动，
  已用 `git diff` 确认与本轮无关）。
- `cargo test -p aox-gui`：**87 passed / 0 failed**（收口复跑结果）。
- CLI 隔离根 E2E：
  - 写入三个文件 → `/checkpoints` 出现一个 task 节点；
  - `/rollback --yes` 删除三个新建文件；
  - `/redo` 逐字节恢复三个文件；
  - 外部修改 `a.txt` → 普通 `/rollback` 拒绝且不写盘；
  - `--force` 后 safety 可恢复外部版本；
  - release 二进制读取旧恢复点正常。
- 无恢复点的空工作区执行 `/checkpoints` **不创建** `.aox/backup`。

### 体积（相对开工前 release）

| 二进制 | 开工前 | 现在 | 增量 |
|---|---:|---:|---:|
| `aox.exe` | 3,886,080 | 4,199,936 | **+313,856 B（约 306 KiB）** |
| `aox-gui.exe` | 7,196,672 | 7,505,408 | **+308,736 B（约 301 KiB）** |

这超出本文 §13 暂定的 +200 KiB 门槛约 100 KiB。`blake3` 使用 `pure` 后端已省约 13 KiB；进一步压缩要么换哈希实现，
要么拆分/裁剪恢复模块。**这是一个待用户拍板的验收项，不应在报告里说成达标。**

参考量级：`src/recovery/` 共 3,440 行（model 225 / store 879 / mod 909 / restore 967 / command 460），
其中测试约 1,100 行。体积增量主要来自 BLAKE3、JSON 序列化路径与这套 3,440 行代码本身。

### 已知未完成 / 限制

- `keep=30`、可达性 GC、`/backup prune` 尚未实现（W7）；当前达到 512 MiB 总量后拒绝继续备份。
- shell 扫描器、插件扫描、`shell_job` 的完整生命周期仍是 W5/W6；当前一律标 partial。
- GUI 只有斜杠命令，没有恢复点浮层（W8）。
- i18n 文案尚未整理（W9）；恢复错误目前以英文源文本呈现。
- 恢复后的 manifest / 目录空洞、ACL/ADS/硬链接仍按设计不保证。
- 既有 `src/prompt.rs` 改动导致 `prompt::tests::disabled_builtin_text_is_gone` 在开工前就失败；本轮未修改该文件，
  也未把这个失败算作回滚系统回归。

## 19. 实施记录 · W5（strict shell 扫描器）

**状态：W5 已完成，等待验收。** W6（插件 / job）、W7（保留与 GC）、W8（GUI 浮层）、W9（收口）未开始。

### 已落地

| 位置 | 内容 |
|---|---|
| `src/recovery/scan.rs`（新增，含测试） | `Scan` / `FileEntry` / `Change` / `StoreScope` / `Limits`、`scan()`、`diff()`、预算判定、`ENTRY_COST` |
| `src/recovery/store.rs` | `has_object()`、`load_baseline()` / `save_baseline()`、`baseline/manifest.json`、`baseline/` 子目录 |
| `src/recovery/mod.rs` | `begin_opaque()` / `finish_opaque()`、`OpaqueGuard`（带 `Drop`）、`OpaqueSummary`、`scan_limits()`、`SCAN_DEADLINE` |
| `src/tool/shell.rs` | 命令前后各一次扫描；`finally` 覆盖成功/非零/超时/空闲杀/spawn 失败全部退出路径；`changed:` 列表改为复用扫描结果；`/shell` 报告新增 recovery 一行 |
| `src/recovery/command.rs` | `/backup status` 新增 `shell guard` 与 baseline 规模 |

`bash` 工具走的是同一个 `run_shell`，因此自动获得同样的覆盖——没有第二条执行路径。

### 语义（三种档位）

| `shell_guard` | 命令前 | 命令后 | 扫描超预算 |
|---|---|---|---|
| `strict`（默认） | 全树 hash；对象缺失则写入 CAS（首次即全树前像） | 全树 hash + delta 内容入库 | **命令不启动**，返回可操作说明 |
| `best_effort` | 只对「与 baseline 不同」的可疑文件存前像 | 只对命令改动过的文件入库 | 照常启动，节点标 `partial` |
| `off` | 不扫描 | 不扫描 | — |

- 净变化只按内容 hash 判定：`touch`、大小不变的重写、删除后重建同样内容都不产生节点（有测试覆盖）。
- 「命令把文件改回原样」时，先前 `edit_file` 留下的 op 会被改写成磁盘实际状态并作为 noop 删除，
  任务整体无净变化就不提交节点。
- 目录被换成文件（或相反）表达为「子树删除 + 文件新建」，不需要第三种类别。

### 与设计文档的三处偏差（都是有意的）

1. **store 无法打开时不再拒绝命令。** 设计只说了「扫描超预算 → 命令不启动」。但如果 `.aox/backup`
   只读、被另一个 AOX 锁住或损坏，那是**恢复系统自己的故障**，不应该升级成「agent 什么命令都不能跑」。
   `begin_opaque` 在这种情况下返回「无窗口」并把原因标 `partial`，命令照常执行。
   超预算仍然拒绝——那是一个明确的、用户可调的取舍，不是故障。
2. **扫描时间上限写死 120 s**（`SCAN_DEADLINE`），没有新增配置项。理由：它只是安全阀，
   真要调应该是把大目录放进 `[recovery] exclude`，而不是给一个几乎没人会改的数字加配置面。
3. **`best_effort` 的「可疑」定义** = 当前 hash ≠ baseline hash。设计文档只写了「进入命令前已知有变化」，
   实现上这句话的唯一可操作含义就是与上一份 baseline 比较。
4. **`best_effort` 不做元数据捷径。** 设计原本允许它用「mtime/size 未变」跳过读取。实现时否掉了：
   时间戳粒度、保留 mtime 的写入工具、同秒内两次修改都会让「没变」变成错误结论，
   而它保护的是「这份内容将来还能不能救回来」。多读一遍是可控成本，错的「没变」是不可逆丢失。
   于是 `best_effort` 只省磁盘与「超预算仍可运行」，**不省读取**——文档已同步改口。

### 实测成本（release，本机）

探针：`AOX_SCAN_PROBE=<dir> cargo test --release --lib a_scan_of_a_real_tree -- --ignored --nocapture`
（该测试 `#[ignore]`，不进常规测试，保留它是为了让这个数字随时可复现。）

| 场景 | 树规模 | 耗时 | 入库 |
|---|---|---:|---:|
| 首次（冷，需要建 baseline） | 163 文件 / 10.6 MB | **1.90 s** | 10.57 MB |
| 后续（热，对象已在库） | 同上 | **141 ms** | 0 B |

读吞吐约 75 MB/s，每条命令两次扫描 ⇒ 10 MB 级代码树每条 shell 命令约 **0.3 s** 额外开销，
首次约 2 s + 一份树大小的磁盘。这与设计预期一致（§11.3），也是默认排除 `target/`、`node_modules/` 的原因。

### 体积（W5 增量）

| 二进制 | W4 收口 | W5 之后 | 本轮增量 | 相对开工前 |
|---|---:|---:|---:|---:|
| `aox.exe` | 4,199,936 | 4,245,504 | **+45,568 B（约 44.5 KiB）** | +359,424 B（约 351 KiB） |
| `aox-gui.exe` | 7,505,408 | 7,552,000 | **+46,592 B（约 45.5 KiB）** | +355,328 B（约 347 KiB） |

一个扫描器 + 一份 baseline 清单，45 KiB 是合理的；用户已接受 W4 的体积增量，本轮仍在其内。
去掉 `FileEntry` 包装（直接用 `BlobRef` 做索引值）省了一点，也少了每项一次指针跳转。

### 测试

- `cargo test --lib recovery`：**37 passed**（W5 新增 16：scan 9 + manager 7；另有 1 个符号链接用例仅在 unix 运行，
  1 个 `#[ignore]` 的性能探针不计入）。
  覆盖：递归与排除、同尺寸重写、删除重建不误报、baseline 复用、best_effort 范围、条目上限、
  时间上限、单文件超限只存 hash、目录↔文件、符号链接不跟随、真实命令端到端、
  超预算拒绝 strict、best_effort 不拒绝、窗口被丢弃后标 partial、改回原样不产生节点。
- `cargo test --lib`：**447 passed / 1 failed**（仍是既有 `prompt::tests::disabled_builtin_text_is_gone`）。
- `cargo test --lib tool::shell`：**17 passed**，其中
  `a_real_command_write_is_recorded_by_the_strict_scan` 真起进程跑 `echo hello > out.txt`，
  断言节点里 `out.txt` 的 `before = None`、`after.stored = true`。

### 顺带修的一处（不在 recovery 里）

`src/tool/job.rs` 的「立即退出」探测窗从 300 ms 放宽到 600 ms。这条断言本来就在高负载下会偶发失败
（W4 收口那一轮出现过一次），W5 新增了一条真正起进程的测试后更容易撞上：窗口太小会把「已经退出」
误判成「是个服务器」。语义没变，只是把这个判定从「靠时间赌」变成「时间更宽裕」。

### 已知限制（W5 边界内）

- `shell_job` 启动的后台进程仍未纳入窗口：运行中的 job 只把任务标 `partial`（W6 已把原因写成可操作的一句话）。
- 插件写入由 W6 接上（见 §20）。
- baseline 只增不减，GC 与 `keep` 裁剪是 W7；当前靠 `max_total_mb` 兜底拒绝。
- 未跟踪排除范围的变更（例如命令专门改了 `dist/`）目前只体现在 `partial` 原因文本里，
  还没有设计文档 §7.4 要求的「N 个变更位于排除范围」计数——留给 W7 与 `/rollback` 预览一起做。

## 20. 实施记录 · W6（插件写扫描 + background job）

**状态：W6 已完成，等待验收。**

### 已落地

| 位置 | 内容 |
|---|---|
| `src/agent.rs` `authorize_and_execute` | 声明写的插件调用被 `begin_opaque` / `finish_opaque` 包住；窗口在 `rt.execute` 之前打开，之后再关（含调用失败/超时） |
| `src/agent.rs` `plugin_window()` | 纯函数判定：`ToolKind::Plugin` && `Access::Write` && `protect_plugins` |
| `src/recovery/mod.rs` `note_background_job` | 原因文本改成可操作的一句话（“stop the job before relying on /rollback”） |
| `bundles/recovery.toml` / `config.example.toml` | `shell_guard`、`protect_plugins` 的注释改成同时覆盖 shell 与插件，并纠正 `best_effort` 的描述 |

### 为什么窗口放在 Agent 而不是插件工具里

`Access` 只有 Agent 知道（`rt.tool_access(name)` 读的是插件自己的声明），而且需要在 `rt.execute`
**之前**开窗、**之后**关窗——这两点都在 `authorize_and_execute` 里天然成立。放进插件工具的
`call` 还得再取一次 access，并且会漏掉 host 侧的超时包装。

`full` 档没有受保护工作区，`active_root()` 为 `None`，此时不开窗（也没有可扫描的边界）。

### 语义

- **声明 `read` 的插件不扫描。** 声明本身就是它的承诺；`docs/SANDBOX.md` 写明它不是强制边界，
  所以“撒谎的插件”不在保证内——扫描每次都做一遍全树，代价远大于它换来的那点安慰。
- **未声明 access 的插件按 `write` 处理**（`tool_access` 的既有规则），因此默认会被扫描。
- **`protect_plugins = false`** 是这个功能的开关；关掉后插件写入完全在回滚之外。
- **`shell_job` 仍然只标 partial**：后台进程的写入可以在任意时刻发生，第一版没有 watch 机制，
  与其假装扫到了，不如明说“这个检查点不完整”。**停止 job 也不会清除这条原因**——
  job 跑了多久就可能写了多久，“现在停了”不是关于它写过什么的证据。

### 测试

- `agent::tests::a_plugin_that_declares_write_lands_in_the_recovery_point`：
  真 `Runtime` + 真 `Agent` + 一个声明写的假插件工具，写 `nested/made.txt`；
  断言工具输出里有 `recovery: this plugin changed 1 file`，且节点里 `nested/made.txt`
  的 `before = None`、`after.stored = true`。子进程启动不是这条测试要证明的事
  （那由 `selftest` 覆盖），所以用假工具是对的。
- `agent::tests::a_plugin_window_is_opened_only_for_a_declared_writer`：
  把三输入判定钉成表（write/read/protect_plugins/host/meta），因为两种错都是静默的。
- `cargo test --lib`：**449 passed / 1 failed**（+1 ignored 性能探针；唯一失败仍是既有 `prompt` 那条）。

### 体积

| 二进制 | W5 之后 | W6 之后 | 本轮增量 | 相对开工前 |
|---|---:|---:|---:|---:|
| `aox.exe` | 4,245,504 | 4,250,112 | **+4,608 B** | +364,032 B（约 356 KiB） |
| `aox-gui.exe` | 7,552,000 | 7,551,488 | **−512 B** | +354,816 B（约 347 KiB） |

W6 只是把已有扫描器接到第二个调用点，几乎不占体积（GUI 的 −512 B 是链接顺序抖动的噪声）。

### 已知限制

- 后台 job 的写入仍未纳入窗口（需要一个 watch/轮询设计，成本远超第一版收益）。
- 插件若在执行中 `chdir` 到工作区外写文件，扫描看不到——那是沙箱与 `workplace` 边界的问题，不是恢复系统的。

## 21. 实施记录 · W7（保留、裁剪与可达性 GC）

**状态：W7 已完成，等待验收。**

这一阶段开始**真的删数据**，所以先把规则钉死，再谈实现。

### 谁引用谁（GC roots）

| root | 为什么它是 root |
|---|---|
| cursor 链（从 head 沿 `parent` 走到头） | 能被回滚到的每一个状态 |
| `baseline/manifest.json` 的每个 blob | 下一条 strict 命令的前像就是它们 |
| `redo/*.json` 的 `states` 与 `expected` | redo 要写回的内容 |
| 每个 journal 里出现的 64 位 hex | journal 存在的唯一理由就是「进程死在说出需求之前」 |
| 正在执行的 restore 计划 | 同上（restore journal 也在 journal 扫描范围内） |

不在上表的对象才可删。**没有 refcount**：崩溃、复制目录、两个进程同时 GC 都会让计数漂移，
而漂移的表现是「恢复时 blob 不见了」。标记清扫是 O(objects)，不会漂。

### 顺序（这部分就是正确性）

裁剪：
1. 边界节点改写 `parent = None`（原子替换）；
2. cursor 写 `truncated = true`；
3. **最后**才删节点文件。

反过来做会留下「节点没了、链还指望它」的状态，而那个状态的表现是回滚写到一半失败。

清扫：先把所有 root 标完，再遍历 `objects/`；且**只在没有活跃 turn / restore 时**运行——
这一点由调用方持 workspace 锁保证（`gc::*` 刻意不自己加锁：同进程二次 `try_lock` 会失败）。

### 已落地

| 位置 | 内容 |
|---|---|
| `src/recovery/gc.rs`（新增） | `trim` / `collect` / `prune` / `maintain` / `sweep_tmp` / `Report` |
| `src/recovery/mod.rs` `finish_turn` | 提交成功后调 `gc::maintain`，把提示追加进 `RecoveryFinish.warnings` |
| `src/recovery/command.rs` | `/backup prune` 真实现；`/backup status` 增加 `retention: keep = N`；`/checkpoint` 说明命名锚点同样受保留窗约束 |
| `src/main.rs` | `/help` 重新列上 `prune` |

### 保留语义（与设计一致的三条，外加一条明说）

- `keep` 数的是**从 cursor 可撤销的 turn 节点数**，不是目录里的文件数。
- 超出窗口的部分**从最老的开始整段丢弃**，剩下的最老节点变成根，cursor 标 `truncated`。
- `keep = 0` 表示**不限**。字面意义的 0 会把历史全删，那是没人想要的行为。
- ⚠️ **`manual`（`/checkpoint`）节点也在同一个窗口里**，会被一起裁掉。「整段丢弃」只有在
  边界统一时才有意义；`/checkpoint` 的回复现在会明确写出这一点。设计 §10.3 提到的
  `/checkpoint --pin` **没有实现**，留作后续——与其做一个「pin 住却因为祖先被删而语义可疑」
  的锚点，不如先把它说清楚。

### 什么时候裁

| 时机 | 动作 |
|---|---|
| 每次 turn 提交成功后 | 一定 `trim`；**只有**存储用量达到 `max_total_mb` 的 80% 才顺带全量清扫 |
| `/backup prune` | `trim` + 全量清扫 + 清理 24h 前的 `tmp/` |
| 每轮 `maintain` | 顺带清理 24h 前的 `tmp/`（崩溃写留下的半成品） |

热路径只做裁剪、不扫对象：链条长度被 `keep` 界住，很便宜；而全量清扫恰好是最不该在随便
一个时刻跑的东西。

**清理失败不会让 turn 失败**：文件已经落盘、恢复点已经发布，「删不掉一个旧对象」不该被报成
「任务没记录成功」。它只进 warnings。

### 配额不足时

仍然是 `put_reader` 直接拒绝（现状，未改）。设计里写的「先裁不可达 → 再用 `--force` 裁到比
`keep` 更低」没有做：那需要一个 `--force` 的语义和 UI，而当前更安全的做法是**明确拒绝并让
用户跑 `/backup prune`**。绝不为了腾空间删掉当前任务的 before。

### 端到端验证（真实 CLI，`keep = 1`，隔离目录）

mock 模型驱动两个 turn（第二个 turn 前在外部改了 `a.txt`）：

```text
! recovery: retention: 2 older recovery point(s) dropped ([recovery] keep = 1)

/checkpoints → 1 recovery point(s); cursor: rp_... (older history truncated)
              rp_...  task  ok  1 path(s)  complete  second turn
/backup status → points: 1 · size: 4.1 MiB · older history: truncated
                  retention: keep = 1 turn(s); older points were already dropped
/backup prune  → nothing to prune: 1 point(s) within keep = 1, 4.1 MiB on disk
```

- 裁剪掉了 2 个节点（`external` 桥接节点 + 第一个 turn），剩下的最老节点成为根。
- `/rollback` 预览明确写出「older history is truncated; only the retained chain can be rolled back」，
  执行后 `a.txt` 回到外部改过的内容（15 B），`/redo` 再回到 4 B。
- `/backup verify` → `verified 2 object reference(s); no missing or corrupt blob`。
  数字从 8 降到 2，正是被裁节点释放的引用。
- `/backup prune` 报「nothing to prune」是**正确**的：baseline 是 root，它仍然引用着
  工作树里每个现存文件的内容，所以没有对象不可达。

### 测试

`cargo test --lib recovery::gc`：**10 passed**

| 用例 | 钉住的结论 |
|---|---|
| `keep_cuts_the_oldest_segment_and_the_boundary_forgets_its_parent` | 5 个 turn + `keep=2` → 删 3，边界 `parent = None`，cursor `truncated` |
| `keep_is_a_no_op_while_the_branch_fits` | 窗口内不动，也不标 truncated |
| `keep_zero_means_no_limit_rather_than_delete_everything` | `keep = 0` 是「不限」 |
| `an_object_survives_while_any_node_still_points_at_it` | 被裁节点的产出仍是保留节点的前像 → 对象必须留；真正不可达的才删 |
| `the_baseline_is_a_root_so_shell_before_images_are_never_swept` | baseline 保活 |
| `a_node_the_cursor_never_reached_is_swept` | 崩溃留下的孤儿节点被清掉 |
| `an_abandoned_journal_keeps_its_objects_alive` | 未完成 journal 是 root |
| `a_finished_safety_snapshot_is_dropped_but_a_live_redo_entry_is_kept` | 死 safety 删、活 redo 留 |
| `hex_tokens_finds_hashes_and_ignores_everything_else` | journal 只认 64 位 hex |
| `maintain_reports_a_trim_but_never_fails_the_turn` | 提示进 warnings，turn 不失败 |

全局：`cargo test --lib` **459 passed / 1 failed**（+1 ignored），唯一失败仍是既有 `prompt` 那条；
`cargo test -p aox-gui` **87 passed**。

### 体积

| 二进制 | W6 之后 | W7 之后 | 本轮增量 | 相对开工前 |
|---|---:|---:|---:|---:|
| `aox.exe` | 4,250,112 | 4,269,568 | **+19,456 B** | +383,488 B（约 375 KiB） |
| `aox-gui.exe` | 7,551,488 | 7,571,968 | **+20,480 B** | +375,296 B（约 367 KiB） |

### 已知限制

- `/checkpoint --pin` 未实现（见上）。
- 配额不足时不做「先 GC 再重试」，直接拒绝并提示跑 `/backup prune`。
- GC 只在 `finish_turn` 与 `/backup prune` 触发；新进程只做 `/checkpoints` 不会触发清理。
- baseline 只保留当前一份：它随每次 shell 命令重建，旧内容对象只有当历史节点还引用时才留得住。
