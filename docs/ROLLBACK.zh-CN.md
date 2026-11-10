# AOX 任务级检查点与回滚

> 状态：**设计稿，尚未实现**（2026-10-25）。  
> 本文只定义机制，不改现有代码。文中的路径、类型名和字段名是实施契约，落地时可以调整拼写，  
> 但语义不能悄悄改变。

## 0. 一句话结论

不要为每个检查点复制工作区，也不要保存容易失效的文本反补丁。采用：

**每个用户任务提交一个“路径级逆向日志”，修改前/后的文件内容按 BLAKE3 内容寻址去重；  
回滚就是沿任务链逆序撤销，写入前先做全量预检，并把当前状态另存为可重做的安全副本。**

它同时满足四个目标：

- 内存上限与文件大小、工作区大小基本无关，只保留元数据；
- 多个检查点共享同一份相同内容，仓库式去重；
- 只触碰任务实际改过的路径，回滚绝不“清空工作区”；
- 崩溃后可从 journal 继续或退出，不把半完成状态当成成功。

它不承诺“工作区在任意时刻的完整文件系统快照”。这个区别必须写进产品文案，否则用户会以  
为它能替代 Git、数据库事务或卷影副本。

## 1. 现状约束（设计依据）

| 事实                                                                         | 位置                                      | 对设计的影响                        |
| -------------------------------------------------------------------------- | --------------------------------------- | ----------------------------- |
| 一轮用户输入由 `run_turn` 统一开始与收尾，成功和失败都会走到收尾                                     | `src/agent.rs:610-671`                  | 天然的自动检查点边界；失败轮也要提交已发生的修改      |
| `task_state` 是模型写的任务进度（目标、步骤、验收），不是文件状态                                    | `src/task.rs`                           | 不能拿它当恢复点；两者必须分名、分生命周期         |
| 主机文件工具是 `read_file` / `write_file` / `edit_file` / `list_dir` / `remove` 等 | `src/tool/hostfs.rs`、`src/tool/edit.rs` | 写入路径可控，可在真正落盘前精确捕获旧内容         |
| `edit_file` 已经把整批编辑做成内存校验后一次原子替换                                           | `src/tool/edit.rs:178-208, 427-448`     | 检查点只需包围一次工具调用，不关心中间编辑步骤       |
| `shell` 只能在命令结束后做有上限的 mtime 扫描，且跳过 `.aox`、`target` 等                       | `src/tool/shell.rs:185-196, 742-780`    | 事后扫描无法找回被覆盖的内容，必须在命令前建立前像     |
| `shell_job` 长驻，`start` 返回后仍会继续写文件                                          | `src/tool/job.rs`                       | 首版不能对运行中的后台任务承诺任务级一致性         |
| 插件是黑盒，`access: "read"` 只是声明，不是强制                                           | `docs/SANDBOX.md`、`src/plugin/mod.rs`   | 插件写入只能做兜底扫描，不能宣称精确归因          |
| 工作区内的历史在 `<workspace>/.aox/history`，对话文件是唯一写入句柄                            | `src/store/jsonl.rs`                    | 备份不能回滚 `.aox` 自身，否则会递归并破坏审计记录 |
| 现有 `verify` 证据在文件写入后失效，`task_state` 有 workspace epoch                      | `src/agent.rs`、`src/checks.rs`          | 恢复后必须让旧验收证据过期，不能沿用“已验证”       |

### 1.1 代码已经证明的两个坑

1. **不能只信 mtime + 大小。** 同尺寸原地重写、时间戳被工具恢复、删除后重建，都可能骗过  
   mtime。mtime 只能用于减少哈希，不能作为“没变”的最终证据。
2. **不能在 `ToolOutput.path` 上记修改。** 插件可以在工具结果里不声明路径却实际写盘；  
   shell 更是完全不声明。检查点必须包围执行路径本身，而不是读结果字段。

## 2. 目标与非目标

### 2.1 目标

- 每个**真正改过文件**的用户任务自动形成一个恢复点；没有净变化就不产生节点。
- 用户可回到上一个任务，或前 n 个任务；可用 id 定位更早的命名检查点。
- 回滚前能看见将修改/删除/恢复哪些文件，冲突时默认拒绝而不是覆盖。
- 回滚本身可撤销（redo），且 redo 不会因为“又写了一个 safety 节点”而错位。
- 崩溃、强杀、插件挂死之后，下一次启动能识别未完成事务，不把半成品当完整检查点。
- 常驻内存有硬上限；大文件、二进制文件不整体读入内存；相同内容在多个检查点只存一次。
- 默认只保护当前工作区；不跟随 symlink/junction 越界；不回滚 `.aox`、`.git`、构建缓存。
- CLI 与 GUI 共用一套实现和一套文案。

### 2.2 非目标（首版明确不做）

- 不替代 Git，不恢复 `.git` 中的 commit、index、ref、 stash。
- 不恢复注册表、数据库内部页、网络副作用、进程状态、环境变量。
- 不恢复 ACL、owner、审计 SACL、NTFS ADS、EFS、硬链接拓扑、junction 目标。
- 不保证工作区外写入（`full` 工作区模式尤其明显）。
- 不保证运行中的 `shell_job`、watcher 或第三方编辑器在恢复瞬间不写入。
- 不做自动三方合并，不把外部改动“猜”成 AOX 的任务。
- 不承诺断电后绝对原子；只承诺关键元数据按顺序持久化，并在损坏时拒绝假装成功。
- 不自动把回滚写进聊天历史并删除消息；聊天记录与工作区是两条独立审计线。

## 3. 先统一术语

### 3.1 三种“检查点”不能混

| 名称                     | 归属 | 内容               | 是否可恢复文件           |
| ---------------------- | -- | ---------------- | ----------------- |
| `TaskState`            | 模型 | 目标、步骤、验收、下一步     | 否                 |
| `RecoveryPoint`（恢复点）   | 宿主 | 一个任务改过的路径及其前/后版本 | 是                 |
| `SafetySnapshot`（安全副本） | 宿主 | 一次恢复前，被恢复路径的当前版本 | 仅用于 redo / 恢复失败回退 |

本文中的“检查点”一律指 `RecoveryPoint`。代码和界面不要把它命名为 `task`，否则会和现有  
`task_state` 的 `kind:"task"` 行混淆。

### 3.2 “任务”的边界

首版定义：

> **一次 `run_turn` 且产生净文件变化 = 一个自动恢复点。**

这是宿主可观测、无需模型配合、不会因模型忘记调用工具而丢边界的唯一边界。它比  
`TaskState.task_id` 更细：一个跨多轮的大任务会产生多个恢复点，这是刻意的，因为用户说  
“退回上一个任务”通常指“退回上一轮操作”。

失败、超时、用户中断的 turn 只要产生了净变化，也提交节点，并标记

|                   |                                  |
| ----------------- | -------------------------------- |
| \`status = failed | interrupted\`。不能因为没有最终答复就丢掉已写文件。 |

### 3.3 “回滚 n”到底回退什么

`/rollback n` 的定义是：

> 把游标移到**最近第 n 个 `kind=turn` 节点之前**，并逆序撤销沿途所有节点（包括穿插的  
> `external` 节点）。

它不等价于：

- 回退 n 条聊天消息；
- 回退 n 个 `TaskState` 任务；
- 把整个工作区恢复到某个时间点的完整快照。

**只撤销 AOX 记录过的、任务实际触碰的路径。** 任务之外别人新建的文件、未触碰的外部修改、  
构建目录里的产物都会留在原地。这是安全的选择，也是低存储的关键。

## 4. 总体架构

```text
                         run_turn
                            │
                    RecoveryTxn::begin
                            │
        ┌───────────────────┼────────────────────┐
        │                   │                    │
   host write tool     shell / opaque      nothing changed
        │                   │                    │
  exact pre-image     pre-scan + CAS             │
        │             run command                │
  exact post-image    post-scan diff             │
        └───────────────────┼────────────────────┘
                            │
                    append journal (fsync)
                            │
                    commit RecoveryPoint
                            │
                      atomically move cursor
```

三层职责：

1. **Capture（捕获）**：在写入前保证旧内容已经成为不可变 blob；捕获失败则**不允许原写入发生**。
2. **Journal（事务日志）**：把本次任务的操作顺序写到磁盘，崩溃后可恢复或明确放弃。
3. **Store（存储与游标）**：不可变节点、内容寻址 blob、`cursor.json`、redo 安全副本和 GC。

### 4.1 建议的模块划分

```text
src/recovery/
  mod.rs       公开 API：RecoveryManager、RecoveryTxn、RollbackPlan、RestoreReport
  store.rs     blob / node / cursor / lock / journal 的低层原子 I/O
  capture.rs   主机精确捕获、opaque 前后扫描、路径规范化与排除规则
  restore.rs   两阶段预检、恢复执行、undo/redo、失败回退
  gc.rs        可达性标记、配额与保留策略
  model.rs     RecoveryPoint / FileOp / BlobRef / Coverage 的 serde 类型
```

不要把全部逻辑塞进 `src/backup.rs`：捕获策略和恢复策略的失败方式完全不同，混在一起会  
出现“为了修 GC 改到写路径”的高风险耦合。

## 5. 磁盘布局

工作区根记为 `WS`：

```text
WS/.aox/backup/
  format.json                    # {"schema":1,"workspace_id":"...","created_at":...}
  lock                           # OS 级独占锁，进程死亡自动释放
  cursor.json                    # 当前游标；唯一可变入口
  index/
    refs.json                    # 最近节点缓存，可重建，不是真值
  nodes/
    rp_20261025T172839Z_7f3a.json
    ...
  objects/
    ab/cd/abcdef...              # BLAKE3-256 内容对象，不可变
  journals/
    tx_20261025T172900Z_91c2.jsonl
  redo/
    redo_20261025T173500Z_2d10.json
  baseline/
    manifest.json                # strict shell 的“当前作用域基线”：全树路径 -> 内容引用
  tmp/                           # 同卷临时文件；启动时清理超过 24h 的残留
  quarantine/                    # 恢复时被替换/删除的当前文件，成功后清理
```

原则：

- `objects/` 和 `nodes/` **只增不改**。任何“改写节点”都是新节点。
- `cursor.json` 是唯一记录“我现在在哪”的文件，最后才更新。
- `format.json` 里的 `workspace_id` 用于发现“把别的备份目录拷进来了”；节点内路径一律是  
  **相对工作区根、用 `/` 分隔**，不存绝对路径。
- `.aox/backup` 永远在排除表第一位；`.aox/history`、`.aox/memory`、`.aox/tmp` 也不进入  
  文件回滚。对话历史和 agent 笔记有自己的生命周期。
- `quarantine/` 只做恢复失败回退的中转，成功后可以删除；被删除的文件在 safety 快照里已有  
  内容，不依赖回收站。
- 所有写入使用“同目录临时文件 + flush + `sync_all` + 原子 rename（Windows 用  
  `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` 或 `ReplaceFileW`）”。发布节点前必须确保 blob  
  已持久化。

## 6. 数据模型

### 6.1 BlobRef

```json
{
  "hash": "8f14e45fceea167a5a36dedd4bea2543f0b7c9d6e1a4b3c2d1e0f9a8b7c6d5e4",
  "size": 18342,
  "codec": "zstd",
  "mode": "100644",
  "stored": true
}
```

- `hash` 针对**原始未压缩字节**计算，BLAKE3-256；对象文件名是 hash。读取时解压后再校验 hash。
- `stored:false` 表示只记录了 hash/size、对象未保存。它仍能用于 drift 预检，但不能 redo；  
  节点的 `redoable` 必须为 false。
- 同一内容出现多次只保留一个对象；`mode` 不参与寻址，允许同内容不同权限的引用。
- 初始版本可以只用 `codec:"raw"`；压缩是存储优化，不是正确性前提。建议后续用 zstd  
  流式压缩，二进制体积与内存需实测后再定。
- 不用 FNV-1a 作为备份地址：`edit_file` 的指纹只用于变更检测，碰撞风险不适合决定  
  “恢复哪份字节”。

### 6.2 FileOp

```json
{
  "path": "src/main.rs",
  "kind": "modify",
  "before": { "hash": "...", "size": 1200, "codec": "zstd", "mode": "100644" },
  "after":  { "hash": "...", "size": 1234, "codec": "zstd", "mode": "100644" }
}
```

`kind`：

| 值             | 前               | 后       | 撤销                           |
| ------------- | --------------- | ------- | ---------------------------- |
| `create`      | `null`          | BlobRef | 删除该路径                        |
| `modify`      | BlobRef         | BlobRef | 写回 before                    |
| `delete`      | BlobRef         | `null`  | 重建文件并写回 before               |
| `type_change` | 文件/symlink/目录之一 | 另一种     | 先移走当前项，再恢复 before；首版建议直接拒绝跟踪 |

- 重命名等价于“旧路径 delete + 新路径 create”，不单独建 rename 操作，恢复语义更简单。
- 同一任务内同一路径多次修改只保留**第一次 before**和**最后一次 after**。这是幂等恢复  
  的关键，也避免节点膨胀。
- `after` 可以缺失，但只在存储配额或后像捕获失败时发生。更稳妥的表示是保留  
  `after` 的 hash/size 并置 `stored:false`：这样仍能判断当前文件是否被外部改动，只是不能  
  redo。节点上必须标 `redoable:false` 并在界面说明，不能静默。
- 目录不作为普通文件记录。删除目录时递归枚举其中的普通文件，每个文件一条 `delete`；  
  空目录是否重建是锦上添花，不作为保证。
- 不跟随 symlink/junction。symlink 本身变化记 `type_change` 并标记不可恢复；目标文件在  
  工作区外的修改不属于本工作区。
- 硬链接按路径分别记录内容；恢复后可能不再共享同一 inode，这是明示限制。

### 6.3 RecoveryPoint

```json
{
  "schema": 1,
  "id": "rp_20261025T172839Z_7f3a",
  "parent": "rp_20261025T171210Z_4b91",
  "kind": "turn",
  "status": "completed",
  "created_at": 1792920519,
  "finished_at": 1792920531,
  "conversation": "20261025-172540-a1b2",
  "turn": 12,
  "label": "给回滚增加 CLI",
  "coverage": {
    "state": "complete",
    "scanned": 348,
    "excluded_roots": [".git", "target", "node_modules", ".aox"],
    "reasons": []
  },
  "ops": [
    {
      "path": "src/agent.rs",
      "kind": "modify",
      "before": { "hash": "...", "size": 30211, "codec": "zstd", "mode": "100644" },
      "after":  { "hash": "...", "size": 30874, "codec": "zstd", "mode": "100644" }
    }
  ],
  "redoable": true
}
```

`kind`：

- `turn`：自动任务节点，计入 `/rollback n`。
- `external`：在任务开始前检测到“上一节点记录的后态 ≠ 当前内容”的桥接节点；只针对已被  
  跟踪过的路径，不计入 n，但回滚越过它时必须一起撤销。它明确表示“这些变化不是 AOX 干的”。
- `manual`：用户用 `/checkpoint <名字>` 生成的命名锚点，通常 `ops` 为空，不计入 n；  
  它的价值是给“大任务开始前”一个可引用的 id。
- `genesis`：每个工作区的隐含起点，不落节点文件；回滚到它等价于“撤销第一个任务”，  
  绝不删除任务开始前就存在的文件。

`status`：

- `completed`：turn 正常结束；
- `failed`：turn 报错，但文件变更已提交；
- `interrupted`：用户中断或进程崩溃后被恢复；
- `recovered`：启动时由未完成 journal 补提交，`coverage` 可能降级。

`coverage.state`：

- `complete`：声明的扫描范围内，前后状态对称且所有前像已持久化；
- `partial`：有排除项以外的漏扫、文件超限、出现不支持的重解析点，或长驻 job 正在写；
- `unknown`：崩溃恢复时无法确认某些路径的中间状态。

`partial/unknown` 的节点仍然可以被用户看到和撤销**已知操作**，但默认拒绝自动 `/rollback`  
跨越它；必须显式 `--allow-partial` 并红色确认。否则“回滚成功”是假话。

### 6.4 journal

每次事务先创建 `journals/<tx>.jsonl`。一行一个事件，追加 + flush，关键行 `sync_all`：

```json
{"t":"begin","tx":"tx_...","parent":"rp_...","workspace":"...","conversation":"...","at":1792920519}
{"t":"intent","tool":"write_file","path":"src/a.rs","before":"...","after":null}
{"t":"commit","path":"src/a.rs","after":"...","kind":"modify"}
{"t":"finish","node":"rp_...","at":1792920531}
```

- `intent` 必须在原始写入**之前**落盘；`before` 必须已经完整写入 objects 并通过 hash 校验。
- `commit` 可以在工具返回后写；如果工具报错但文件实际变了，仍要写 `commit`。
- 最后写节点，再更新 cursor，最后删除 journal。删除失败不影响正确性，启动时会清理已提交  
  journal。
- journal 行损坏：保留原文件，标记事务 `unknown`，不要凭猜继续。

## 7. 捕获策略

### 7.1 工具矩阵

| 入口                                | 捕获方式                 | 默认覆盖        | 说明                                 |
| --------------------------------- | -------------------- | ----------- | ---------------------------------- |
| `write_file`                      | 写前精确读取目标；写后读取新内容     | 精确          | 目录创建需单独记录或明确不恢复空目录                 |
| `edit_file`                       | 写前读取一次原文；写后读取一次新文    | 精确          | 复用原子写入，不复制中间状态                     |
| `remove`                          | 删除前递归枚举并保存每个文件；目录项记录 | 精确到文件       | 必须在移入回收站/删除之前                      |
| `shell` / `bash`                  | 命令前工作区快照 + 命令后对称扫描   | 尽力，范围受排除表限制 | 成功、非零、超时、空闲杀、spawn 失败都要 finally 扫描 |
| `shell_job`                       | 首版不纳入自动节点            | 明确不保证       | 运行期间持续写入，无法给 turn 一个结束边界           |
| 插件工具（声明写/未声明）                     | 工具前快照 + 工具后扫描        | 尽力          | 声明 `read` 的插件按只读信任；它若撒谎不在保证内       |
| `create_plugin` / `update_plugin` | 若目录在工作区内，按目录精确捕获     | 范围外不保证      | 插件目录通常在应用根，不在固定工作区                 |
| 用户/外部进程                           | 下次任务开始时用 drift 检测发现  | 保护现场，不自动吸收  | 不把外部修改伪装成 AOX 任务                   |

### 7.2 主机工具为什么必须在 `call` 内捕获

边界检查在 `Agent::authorize_and_execute` 与工具 `call` 两处都有；但检查通过不等于写入  
已发生。捕获应放在主机工具自己的执行路径上，且位于 `guard` 之后、真正写盘之前：

```text
parse args -> guard/workplace -> capture_before() -> mutate -> capture_after() -> journal
```


原因：

- 授权层拿不到“这次写入是否 no-op”，提前备份会制造空节点；
- hook 可以改写参数，授权层看到的参数不一定是最终参数；
- `selftest` 直接调用 `Runtime::execute` 会绕过 `Agent`，放在工具内仍然覆盖；
- 只有工具知道它是否真的写了、写了几次、是否写后报错。

`capture_before` 失败时，工具返回错误，**原操作不执行**。错误文案要包含：

- 是哪个路径、多大、为什么没备份成功；
- 当前检查点配置与排除表；
- “文件未被修改”这一事实；
- 用户可以如何调整（清理空间、增加预算、临时关闭进程）。

### 7.3 shell 的前后扫描

首版不依赖 `track_changes` 的事后 mtime 扫描。流程必须是：

```text
1. 获取工作区事务锁
2. 遍历声明范围，生成 before manifest（路径、类型、大小、mtime、ctime/change time、
   Windows file id/volume id；strict 仍对每个常规文件流式计算 BLAKE3，不靠元数据跳过）
3. 按 hash 复用已有 baseline 对象；缺失的内容写入 CAS。首次 strict 调用为全树建立 baseline
4. 写入 before manifest 与 ready 标记并 fsync
5. 启动命令
6. 无论退出方式，finally 再遍历一次，按同一规则生成 after manifest
7. 对新增、删除、类型变化、元数据变化、内容 hash 变化的路径求差集
8. 写入 FileOp（first-before / last-after 由任务聚合）
9. commit 节点
```

关键点：

- **前后遍历必须对称**：同一 exclusion、同一最大深度、同一文件类型规则。一边跳过一边不  
  跳过，会制造隐形的“文件凭空出现/消失”。
- **元数据不单独证明未变化。** strict 在命令前对每个常规文件流式计算 BLAKE3，命令后对
  新增、删除、类型/元数据变化以及全部疑似路径重算；最终以 hash 判断净变化。
  `best_effort` **同样全量 hash**（W5 实施时改的，见下），区别只在“存什么”。

  > 原计划让 `best_effort` 用 mtime/size 未变来跳过读取。实现时放弃了：Windows 的 mtime 粒度、
  > 保留时间戳的写入工具、以及同秒内的两次修改，都会让“元数据未变”变成**错误结论**，
  > 而它保护的恰好是“这份内容将来能不能救回来”。多读一遍文件是可预期的成本；
  > 一次错的“没变”是不可逆的数据丢失。所以 `best_effort` 的收益只有两条：
  > 少占磁盘（只存可疑文件的前像）、以及在超预算时仍然运行（节点标 partial）。
- **“内容相同但 mtime 变了”不算净变化**，节点不提交该路径；这避免 `touch`/构建系统制造  
  噪声。
- **首次 opaque 调用需要工作区前像。** 这是任意 shell 可写任意文件所付出的不可避免成本。  
  strict 模式保留**一份** `baseline/manifest.json`，其中每个路径引用当前内容对象；后续命令
  仍要读全树算 hash，但 hash 相同的文件直接复用对象，不必每次重写全树。基线是 GC root，
  但**不是检查点节点**。全部任务共用这一份基线，历史检查点仍只引用自己改过的路径。
- 基线的大小约等于“可跟踪源码树”一份，不是每个任务一份。第一次 strict shell、或切换工作区  
  后第一次 strict shell 要为全树建立它；若这份成本不可接受，使用 `best_effort` 或 `off`，  
  并接受相应的不完整保证。元数据内存和总配额同样包含基线。
- `best_effort` 只对“进入命令前已知有变化”的文件保存前像；命令结束后才发现的其它写入  
  只能记 hash、无法恢复旧内容，因此节点必须是 `partial`，默认不能自动跨它回滚。
- 跨任务同样先做一次完整 hash 扫描（strict）。命令结束更新基线，并把被替换的旧对象交给
  GC（仍被历史节点引用的对象不会删）。
- 大小、文件数、元数据内存、扫描时间任一超预算：**命令不启动**（strict），或启动后把节点  
  标 `partial`（explicit best-effort）。不能悄悄降级后仍宣称完整。
- 扫描期间如果目录发生剧烈变化，重试一次；仍不稳定则标 `partial`，不启动 strict 命令。
- 命令超时或被杀时，子进程的 Job Object 会杀死进程树，但磁盘写入可能已经发生；finally  
  扫描必须仍然执行。
- `shell_job` 运行期间，任何任务的扫描都可能把它的异步写入算进当前任务。首版在检测到  
  有活跃 job 时把节点标 `partial`，并提示“先停止后台任务再依赖回滚”。

实现口径（W5 落地后补记）：

- **前后两次扫描都是「全树 hash」**，不是「先看 mtime 再决定读不读」。热态一次扫描约等于读一遍
  可跟踪文件，本机实测 ≈75 MB/s（10.6 MB / 141 ms）；首次还要把整棵树写进 CAS（≈1.9 s）。
  这就是为什么默认排除 `target/`、`node_modules/`：它们通常比源码大两个数量级。
- **baseline 是「前像缓存」，不是承诺。** 它让第二次命令不必重写全树，但即使 baseline 丢了，
  正确性也不受影响，只是要多付一次全量入库。
- **恢复系统自身的故障不阻断命令。** store 打不开（只读、被另一个 AOX 锁住、损坏）时，
  命令照常执行，只是这次写入不进回滚，并在节点原因里写清楚。只有「扫描超出预算」这种
  用户可调的明确取舍才会让 strict 拒绝启动命令——把故障升级成「什么都不能跑」是错误的取舍。

### 7.4 排除表

默认排除：

```text
.aox/                 # 备份、历史、笔记、临时目录
.git/ .hg/ .svn/      # 版本库内部状态，不是工作树内容
target/ node_modules/ .venv/ venv/ __pycache__/ .pytest_cache/
runtime/ .workbuddy/  # 下载下来的工具链 / 伙伴工具自己的状态，都不是用户源码
dist/ build/ out/     # 需要按项目调整；默认视为生成物
*.exe *.dll *.pdb *.msi *.apk   # 可执行影像与符号：一次构建约 9 MiB，且无法"恢复"进运行中的进程
*.tmp *.swp *.lock    # 编辑器/进程临时文件
```

排除不是“忽略用户文件”的借口。节点提交时要统计排除根之外是否还有未跟踪的变更；如果  
用户通过 shell 专门改了 `dist/`，节点应显示“1 个变更位于排除范围，未纳入回滚”。不是  
每次都弹窗，但恢复预览必须能看到这一行。

排除表是配置，不是代码常量。固定工作区里不同项目可能需要把 `target` 改回可跟踪；此时  
存储代价由用户承担。

### 7.5 文件身份与路径规范化

- 节点路径统一为工作区相对路径、`/` 分隔、保留原始大小写。
- 比较时按平台语义判断大小写：Windows 默认不区分，但不能对 UNC、大小写敏感目录做错误  
  假设。实现应复用/扩展现有 `workplace::normalize` 的逻辑，而不是再写一套。
- 对每个路径记录可选 `file_id`（Windows `FileIdInfo`，Unix `dev+ino`）。文件被替换但路径  
  相同，内容 hash 会变；`file_id` 用于发现“这是新文件”以及 drift 的辅助证据，不作为唯一  
  身份。
- 拒绝 `..` 逃逸、拒绝越过工作区根、拒绝跟随 reparse point。扫描时遇到 junction/symlink：  
  不进入目标，记录为 `type_change`/不支持项并标 partial。
- 恢复目标使用原生宽字符路径，不走 UTF-8 窄 API；长路径前缀由标准库处理，测试要覆盖
  > 260 字符。
- 同一个 `workspace_id` 下，根路径可以移动；节点是相对的，不存绝对根。若检测到备份目录  
  来自不同 `workspace_id`，拒绝使用而不是按相对路径乱恢复。

## 8. 节点提交与崩溃恢复

### 8.1 提交顺序

**顺序本身就是正确性的一部分。**

```text
1. 获取 workspace 级 OS 独占锁
2. 所有 before / after blob：写临时文件 -> flush -> sync_all -> 校验 hash -> 原子 rename 发布
3. 节点 JSON：写临时文件 -> flush -> sync_all -> 原子 rename
4. cursor.json：写临时文件 -> flush -> sync_all -> 原子 rename（最后）
5. 删除 journal（失败可留给下次清理）
```

如果第 3 步成功、第 4 步前崩溃：节点已存在但游标没动。启动恢复比较节点 `after` 与当前  
文件，若匹配则把游标推进到该节点；不匹配则把节点标 `recovered/unknown`，不自动推进。

如果第 2 步后、第 3 步前崩溃：journal 里有 `intent`，启动时重新扫描当前状态，能确认  
净变化就补建 `recovered` 节点；不能确认就保留 journal 并提示“上次任务未完整记录”。

如果写入前捕获失败：原写入根本没有发生，不应创建节点。

### 8.2 启动恢复状态机

对每个未完成 journal：

| 当前工作区状态                 | 动作                                |
| ----------------------- | --------------------------------- |
| 所有路径仍等于 `before`        | 事务未真正写入，丢弃 journal，不建节点           |
| 所有路径等于 `after` 或已知的中间状态 | 补提交节点，标 `recovered`               |
| 部分路径匹配、部分未知             | 建 `coverage=unknown` 节点，但禁止自动回滚跨越 |
| blob 缺失 / journal 损坏    | 保留现场，报错；不清空、不猜                    |

启动恢复只在获得 workspace 锁后执行。没有锁说明另一个 AOX 正在写，不要抢。

### 8.3 安全副本（safety）与 redo

回滚前必须先保存**将被恢复路径的当前版本**，但并不把它塞进普通任务链。原因：如果 safety  
成为 `parent` 链的一环，`/rollback n` 的计数和 redo 方向都会被污染。

独立文件：

```json
{
  "schema": 1,
  "id": "redo_20261025T173500Z_2d10",
  "from_cursor": "rp_C",
  "target_cursor": "rp_A",
  "created_at": 1792920900,
  "paths": [
    {
      "path": "src/a.rs",
      "state": { "hash": "...", "size": 1234, "codec": "zstd", "mode": "100644" }
    }
  ],
  "ops": [
    { "path": "src/a.rs", "kind": "restore_state", "state": { "...": "..." } }
  ]
}
```

- safety 必须在**第一个恢复写入之前**完整落盘；落盘失败则拒绝开始恢复。
- `/redo` 只恢复 safety 中记录的路径，先预检当前状态是否等于 `target_cursor` 的状态；成立  
  才执行。成功后游标回到 `from_cursor`。
- 新任务一旦提交，清空 redo 栈（或至少不再把旧 redo 当作可继续分支），旧 safety 成为 GC  
  候选。不能悄悄保留一个“看起来能 redo，实际父节点已经变了”的幻影。
- 连续两次 `/rollback 1` 会生成两个 safety；`/redo` 按 LIFO 回退，最多保留 8 个，超出丢  
  最旧并明确提示。

### 8.4 外部改动与 `external` 桥接

只在“新任务第一次触碰某条已跟踪路径”时检查：

```text
上一节点记录的 after(H) ≠ 当前内容 X
```

这时不做二选一（覆盖 X 或拒绝整个任务），而是先生成一个 `external` 节点：

```text
before = H
after  = X
kind   = external
parent = 当前游标
```

然后任务节点以它为新 parent。这样：

- 外部修改被如实记录，不冒充 AOX 的任务；
- 当前任务撤销后，回到的是 X，不是 H，用户的外部工作不会莫名消失；
- 若用户继续回退到更早任务，预览会显示“还会撤销 1 次外部修改”，不会静默覆盖；
- `external` 不计入 `n`，但回滚跨越它时会作为普通逆向操作执行。

如果第一次触碰时 head 没有该路径的记录（例如任务开始前就存在的文件，或 genesis 后  
的新文件），**不要**为它生成 `external create`。任务的 `before` 已保存当前内容；把“本来就  
存在”记成 AOX 创建，会导致回滚越过 genesis 时误删用户文件。

## 9. 回滚算法

### 9.1 解析目标

```text
fn target_for(n):
    cur = cursor
    count = 0
    while cur is not null:
        node = load(cur)
        if node.kind == "turn":
            count += 1
            if count == n:
                return node.parent       # may be null == genesis
        cur = node.parent
    error("only k tasks are retained")
```

`external`/`manual` 节点被走过但不计数。请求超出保留范围时明确报错，**不能静默只回退  
一部分**。

### 9.2 两阶段恢复

**阶段 A：只读预检，不改任何文件。**

1. 从 cursor 向 target 走，收集逆向操作，按路径取最靠后的状态作为“当前应有状态”。
2. 检查链连续性：相邻节点中同一路径的后态/前态必须一致；`external` 节点是合法的断点  
   记录，不能跳过。
3. 校验所有 blob 存在、大小正确、解压后 BLAKE3 匹配；缺失即中止。
4. 对每个受影响路径检查当前状态是否等于预期：
   - 当前 hash、类型、存在性与预期一致：可恢复；
   - 当前缺失而预期存在：drift；
   - 当前存在而预期缺失：drift（可能是外部新建）；
   - 权限位不同但内容相同：可恢复，恢复时按 before 权限处理，并提示。
5. 检查目标是否仍在同一工作区、same volume、不是排除路径。
6. 检查有无活跃 `shell_job`、活跃 turn、未完成 journal。
7. 生成预览：

```text
将回滚 2 个任务，涉及 5 个路径
  M  src/a.rs            12.3 KiB -> 11.8 KiB
  A  src/new.rs          (created) -> removed
  D  docs/old.md         removed -> restored
  !  README.md           外部改动，默认阻止
  -  dist/app.exe        排除范围内，未跟踪
```

任一 drift 或覆盖不完整：**在写第一个字节前停止**。输出冲突路径，并建议：

- 先提交/备份外部修改（或手动另存）；
- 使用 `/rollback <id>` 只回到冲突发生之前；
- 确认要丢弃时用 `--force`，仍先保存 safety。

**阶段 B：执行恢复。**

1. 再次获取锁。
2. 重新计算预检的关键 hash（减小 TOCTOU 窗口；不能消除外部进程竞争）。
3. 写 safety 文件并 `sync_all`。
4. 写 `journals/restore-*.jsonl`，记录 target、safety、每个路径的计划与进度。
5. 按逆序逐路径执行：
   - 删除 AOX 新建的文件：先 `rename` 到 `quarantine/<restore>/...`，不直接 `remove`；
   - 恢复/覆盖：写同目录 temp -> flush -> 权限 -> 原子 rename；
   - 重建父目录：`create_dir_all`，只创建恢复所需目录，不碰已有目录内容；
   - 删除文件后清空不再需要的父目录只限本次创建且为空的目录，失败不算恢复失败。
6. 每完成一个路径，journal 追加一行 `applied`。
7. 全部成功后，先更新 cursor 到 target；清理 quarantine 和 restore journal，但**保留  
   safety** 供 `/redo` 使用。safety 只在新任务提交、redo 栈失效后才进入 GC。
8. 失败时用 safety 逆向回滚已应用的路径；回退也走 journal，不直接假定成功。失败后可保留  
   safety，允许重试恢复或退回恢复前状态。

### 9.3 不是“全局原子”，但必须是“可收敛”

多文件恢复在操作系统上不存在跨文件原子提交。我们保证的是：

- 在正常路径上，全部预检通过才开始；
- 任一时刻崩溃，journal 足以判断哪些已完成、哪些未完成；
- 启动时能继续同一个恢复，或在无法判定时停住并保留现场；
- 失败回退使用 safety，不会留下只有一半且失去原像的文件。

因此产品文案应说“整批恢复可收敛”，不要说“绝对原子”。

### 9.4 恢复后的 agent 状态

工作区回滚后，聊天记录默认**不回滚**。宿主追加一条只对模型可见/界面可见的恢复通知：

```text
工作区已回滚到 rp_...（撤销 2 个任务，5 个路径）。
旧的 task_state 步骤、验收证据和工具输出可能已过期。
文件内容优先；重新读取受影响文件并重跑验收后再继续。
```

必须做：

- 使该对话的 `verify` 证据全部过期（workspace epoch +1，或等价机制）；
- 不把旧 `modified_files` 当成当前事实；
- 不自动把 `task_state.status` 改成 `done`；
- 模型若继续修改，按新任务提交新节点，parent 是回滚后的 cursor。
- GUI 只刷新受影响的文件视图；不要在恢复中途逐文件刷新，避免用户看到半完成状态。

可选（需明确确认）：`/rollback --with-chat` 截断聊天到对应 turn。首版不做，因为它同时  
涉及 `Session::rewrite`、act/usage 保留和中断回滚语义，风险远高于文件恢复。

## 10. 保留、配额与 GC

### 10.1 保留策略

默认：

```toml
[recovery]
enabled = true
keep = 30                 # 当前分支最多可撤销 30 个 turn 节点
max_total_mb = 512        # 所有 blob + 节点 + safety 的硬上限
max_file_mb = 64          # 单文件前像上限；超过即 strict 拒绝写入
max_turn_mb = 256         # 单任务新增对象上限
metadata_memory_mb = 32   # 扫描索引的内存预算
shell_guard = "strict"    # strict | best_effort | off
protect_plugins = true
```

`keep` 的语义是“从当前 cursor 可撤销的 turn 节点数”，不是目录里文件数。被保留的祖先节点  
不能因为按时间排序而被随便删掉。

### 10.2 裁剪

只允许裁掉**从 cursor 不可达**的节点：

- 旧 redo 分支（如果已经失效）；
- 超过 `keep` 的更早祖先，必须**从最老的开始整段丢弃**，并在剩余最老节点写  
  `truncated=true`；
- 已完成的 safety、临时 quarantine、24 小时前的 tmp。

什么时候可以裁祖先：

1. 新节点提交成功后；
2. 用户执行 `/backup prune`；
3. 启动清理。

裁剪前先把 `keep` 降到实际可撤销数量并持久化，再删节点；反过来会出现“节点没了但 cursor  
还指向它”。如果 quota 不足：

- 先裁不可达节点；
- 再按保留策略裁最老祖先，但**不能低于用户设置的 `keep`**，除非显式 `--force`；
- 仍不足：strict 模式下拒绝下一次需要前像的写入，明确告诉用户；绝不为了腾空间删掉  
  当前任务的 before。

### 10.3 GC

不维护对象 refcount。refcount 在任何一次崩溃、复制目录或并发 GC 后都可能漂移，最终表现  
是“恢复时 blob 不见了”。

使用标记清扫：

```text
roots =
  当前 cursor 及其所有祖先
  + strict shell 的 baseline manifest 及其 blob
  + 所有 redo/safety 引用的节点和 blob
  + 所有未完成 journal 引用的 blob
  + 正在执行的 restore plan 引用的 blob
  + 手工 pin 的节点
```

从 roots 的 `ops.before/after` 收集 hash，扫描 `objects/` 删除未标记对象。GC 必须持  
workspace 锁，且只在没有活跃 turn/restore 时运行。GC 失败不影响写路径；下次再试。

手工 pin：`/checkpoint --pin <id>` 可让一个命名锚点不受 `keep` 裁剪，但仍受  
`max_total_mb` 约束。空间不足时应让用户选择解除 pin，而不是静默删。

### 10.4 一个直观例子

```text
初始 a.rs = 10 KiB(A)
T1 改 a.rs -> B(10 KiB)，新增 b.rs(B)
T2 改 a.rs -> C(11 KiB)，改 b.rs -> D
T3 只改 c.rs
```

回收点：

```text
genesis -> rp_T1 -> rp_T2 -> rp_T3
```

`/rollback 1`：撤销 T3，只动 c.rs；a/b 完全不动。  
`/rollback 2`：撤销 T3、T2，a.rs 回到 B，b.rs 回到 B；T1 的 A 仍在 blob 中但未被本次引用。  
`/rollback 3`：再撤销 T1，a.rs 回到 A，b.rs 删除。  
如果 T1/T2 之间用户手动把 a.rs 改成 X：下次任务触碰 a.rs 时生成 `external` 节点，  
`/rollback 1` 回到 X 而不是 B；预览会明确写出这一点。

## 11. 内存、磁盘与性能预算

### 11.1 内存

- 文件拷贝：128 KiB 固定缓冲区，流式读、流式 hash、流式压缩；不因文件变大而增加内存。
- 扫描索引：`BTreeMap<RelativePath, FileState>`，每项约 100–300 字节；默认 32 MiB 预算，  
  超出即 strict 拒绝，不 OOM。
- 节点构建：一个任务只在内存中聚合**被改过**的路径；opaque shell 的扫描索引可复用，但  
  任务结束时释放。
- CLI/GUI 列表只读 `index/refs.json`，不加载全部 ops；查看详情时按需读单个节点。
- 恢复预检只保存计划中的路径和 BlobRef；不加载文件内容。

### 11.2 磁盘

- 同一任务同一路径只保留首前像和末后像；
- strict shell 额外保留一份当前作用域基线（不是每任务一份），用于下一次命令的前像；
- 同一内容跨任务只存一次；
- 文本压缩可选，默认先 raw 保证可实施性；zstd 只是 codec 优化；
- 排除构建目录，避免把编译产物纳入备份；
- 单任务/总量有硬配额，达到上限就拒绝，不无限膨胀。

### 11.3 典型开销

| 操作                             | 额外开销                                                  |
| ------------------------------ | ----------------------------------------------------- |
| `write_file` / `edit_file` 单文件 | 读旧文件 + 哈希 + 写对象 + 写节点，通常毫秒级；与模型几十秒相比可忽略               |
| 每个 strict opaque shell 命令      | 完整遍历 + 流式 hash 可跟踪文件；首次还写 baseline 对象，之后只写新增对象 |
| 同一任务后续 shell                   | 仍要完整 hash 扫描，但只写新增内容对象；IO 仍与源码树大小线性 |
| 回滚                             | 预检读取受影响文件 + 写回；与文件数线性，不触碰无关文件                         |
| 无文件变化的任务                       | 只写一条 begin/finish journal，不提交节点；可进一步在无写入时完全不建 journal |

### 11.4 什么时候不值得开

- 巨型二进制仓库、视频素材目录：把素材目录放进排除表，否则首个 shell 会扫很多 GB。
- 纯聊天/只读任务：从不进入捕获路径，开销为零。
- 用户只信 Git：可 `enabled=false`，不注册 CLI 命令，不显示按钮；行为与今天完全一致。


## 12. 与现有代码的接入点

### 12.1 生命周期

| 位置 | 改动 |
|---|---|
| `Agent::run_turn`（`src/agent.rs:610-671`） | `begin_turn` 后开恢复事务；`lp.run` 返回后无论成败调用 `finish_turn_transaction`；空事务丢弃 |
| `Agent::authorize_and_execute` | 不需要在授权前备份；捕获放在工具内部，避免 hook 改参数后备份错误目标 |
| `src/tool/hostfs.rs` | `write_file`/`remove` 在真正落盘前调用 `capture_before`，返回后 `capture_after`，错误路径也调用 |
| `src/tool/edit.rs` | 在 `write_atomically` 前后捕获；保留现有“全批校验后一次写”语义 |
| `src/tool/shell.rs` | 统一包装整个 `prepare -> spawn -> wait -> finally scan` 生命周期，覆盖所有退出分支 |
| `src/tool/job.rs` | 首版只标记“运行中 job → coverage partial”，不做异步追踪 |
| `Runtime` | 持有一个 `RecoveryManager`，按活动工作区/`workspace_id` 取 store；不要做成全局静态单例 |
| `src/store/jsonl.rs` | 可选增加 `kind:"recovery"` 行，把恢复事件随对话一起审计；不影响 `rewrite` 保留其他 kind 的规则 |
| `src/tool/verify.rs` / `task_state` | 恢复后使证据过期；不直接改写模型的任务文本 |
| CLI `/help` | 增加 `/rollback`、`/redo`、`/checkpoints`、`/checkpoint`、`/backup` |
| GUI `ToAgent` / `FromAgent` | 增加检查点列表、恢复预览、确认和回执；所有磁盘动作仍回 agent 线程 |
| 配置层 | `[recovery]` 加入 bundle/default；未配置时默认启用还是关闭要单独拍板 |

### 12.2 锁与并发

- 一个进程内按工作区持有一个 `RecoveryTxn`；当前 AOX 是单 agent 线程，但将来多对话并行时
  不能让两个 turn 同时改同一 store。
- 跨进程使用 OS 文件锁锁定 `.aox/backup/lock`。锁从**第一次需要捕获的写入**开始持有，直到
  该 turn 提交；不是从用户发出消息就锁住，也不能只锁 cursor 更新的那一瞬。拿不到锁就明确
  提示“另一个 AOX 正在此工作区写入”，不要抢写 cursor。提交前重新确认 cursor 仍等于事务
  的 parent，否则放弃本次提交并报告冲突。
- 外部编辑器/构建进程不受锁约束。预检与执行之间的 TOCTOU 只能缩小，不能消除；UI 必须
  提示“恢复时请勿同时编辑”。
- 同一进程内 host 工具并发时，事务日志追加要串行化；同一路径的 before 只取第一次。首版
  可保持工具顺序执行，不为并行提前增加复杂度。

### 12.3 工作区切换

- 事务绑定 `workspace_id + canonical root`；切换工作区前必须提交或放弃当前事务。
- GUI 的 `/workplace` 切换、关闭对话、退出程序都走同一个 `finish or abort` 路径。
- `full` 模式允许写到工作区外：这些路径不能进本工作区节点的相对路径；捕获层应拒绝跟踪
  并警告“此写入不在回滚范围”。不要偷偷把它塞进相对路径或备份到别的根。
- `default` 模式首次消息才确定根，事务必须在根确定后再开；`run_turn` 当前在开头调用
  `ensure_started`，接入时要把顺序改成“确定根 → 开事务 → 推送用户消息”。

## 13. CLI 与 GUI 语义

### 13.1 CLI

```text
/checkpoints                最近 20 个恢复点：id、时间、任务摘要、路径数、状态
/checkpoints --all          包含 external / manual / 已截断标记
/checkpoint "before refactor"   命名锚点，不产生文件操作
/rollback                   预览上一个 turn，不执行
/rollback 2                 预览前两个 turn
/rollback rp_...            预览指定恢复点
/rollback --yes             确认执行（有冲突时仍需 --force）
/rollback --force           保存 safety 后覆盖 drift；红色警告
/redo                       恢复最近一次回滚前的状态
/backup status              空间、节点数、最早可回退点、未完成 journal
/backup verify              校验当前分支所有 blob
/backup prune               清理不可达对象；不跨越保留语义
```

`/rollback` 默认只预览。这是有意的：文件恢复比“看错一行”更难挽回。没有冲突时确认框
可显示文件差异统计；有冲突时逐条列出，默认拒绝。

### 13.2 GUI

不要新开一整套复杂窗口。最小形态：

- 输入框左下角工作区按钮旁，增加“恢复点”入口（与现有 workplace 控件同排）。
- 打开一个紧凑浮层：列表项显示时间、一句任务摘要、`+N/−M` 路径数、状态点。
- 选中后底部显示文件列表和冲突徽标；“回滚到这里”按钮在无冲突时可用。
- `partial/unknown` 或 drift 时按钮变红，必须再勾选确认，不使用默认主按钮。
- 回滚中进度以宿主事件显示，完成后刷新列表和文件视图；失败明确显示卡住的路径和
  “已回退/未回退”数量。

模型不能直接调用 `/rollback`。这是用户级操作，不进入工具表；否则模型可能为了“修好”
把自己的错误操作自动撤销，造成无法审计的状态跳变。

## 14. 安全与隐私

- 备份对象可能包含密钥、`.env`、私有源码。默认只存放在工作区本地 `.aox/backup`，不进入
  LLM 上下文、不随对话历史发送、不上传网络。
- 对象文件继承工作区权限。不要为了方便而放宽 backup 目录的 ACL；如果当前用户目录本身
  是共享的，应把工作区放到访问受控的位置，或关闭本机制。
- 备份不是加密保险箱。磁盘被读取时对象仍可能被恢复；不要把“内容寻址”误当加密。
- 排除 `.git` 意味着 Git 内部历史不被保护；但工作树源码会被保护。不要向用户宣传能
  “撤销 commit”。
- 任何恢复都只写工作区内路径，且拒绝 reparse point 逃逸；这是安全边界，不是可选优化。
- 不在错误文案、日志、节点 label 中写入文件内容或密钥；label 来自用户/首条指令，需截断
  且转义。

## 15. 失败模式与必须给出的行为

| 失败 | 行为 |
|---|---|
| 备份空间不足 | strict：写入前拒绝，原文件不变；opaque 已执行：节点标 `partial`，明确告知 |
| blob 缺失/损坏 | 预检中止，不写任何文件；`/backup verify` 标出节点 |
| 外部改动 | 默认中止，列出路径；`--force` 才覆盖，且先存 safety |
| 恢复中途崩溃 | journal + safety，启动时继续/回退；不能当成功 |
| 文件被占用 | 逐路径报告；不无限重试，不阻塞整个会话；已应用部分可回退 |
| 工作区根变化 | 拒绝恢复，提示旧根与新根；不按相对路径猜 |
| 两个 AOX 同时写 | 获取不到锁，提示等待；不合并游标 |
| 后台 job 在写 | 标 partial，提示先停 job；不宣称完整回滚 |
| 插件/脚本改了排除目录 | 预览显示“未跟踪变更”，建议改用可跟踪工作区或调整排除 |
| `task_state` 与文件冲突 | 文件优先；恢复后证据过期，模型需重读、重验 |

## 16. 测试计划

### 16.1 纯逻辑（不碰真实工作区）

- 路径规范化：大小写、斜杠、`..`、Unicode、长路径、保留名。
- FileOp 聚合：同任务同路径多次写只留首 before/末 after；净无变化不提交。
- `n` 解析：turn/external/manual 混合链，请求超过保留范围报错。
- 预检计划：modify/create/delete/type_change、drift、缺失 blob。
- redo：一次回滚、连续回滚、safety LIFO、新任务后 redo 失效。
- 保留：最老祖先删除后 `truncated` 语义、pin 不被裁、quota 不裁当前 before。
- GC：共享 blob 不被误删；redo/journal roots 保活。

### 16.2 文件系统（临时目录，带真实 I/O）

- `write_file` 新建、覆盖、同名目录、只读文件、父目录创建。
- `edit_file` 批量全成功/全失败、CRLF、BOM、二进制拒绝后的原文件不变。
- `remove` 单文件、目录树、空目录、回收站失败分支。
- 大文件：>64 MiB 的 strict 拒绝；流式内存不随大小增长（可用计数器/测试钩子）。
- shell：成功、非零退出、超时、空闲杀、spawn 失败都要有 finally 扫描；比较同尺寸重写、
  删除重建、mtime 变化但内容不变。
- 排除目录：只改 target 时节点不产生路径；预览能看到“未跟踪”提示。
- 外部修改后执行任务：生成 external 节点；rollback 只看得到正确前态。
- 符号链接/junction：不跟随、不越界、标 partial。
- 恢复文件占用：目标被独占打开时报告失败，safety 能让已改文件回退。

### 16.3 崩溃与并发

- 在 blob 写入中、节点发布中、cursor 更新中分别模拟进程退出，重启状态机结果符合表 8.2。
- restore 在预检后、第 1 个文件后、最后文件前强杀；重启能续做或回退。
- 两个进程抢 workspace 锁：一个写，另一个明确失败。
- 恢复过程中外部进程再改文件：预检二次 hash 发现后中止（剩余 TOCTOU 在文档中承认）。

### 16.4 验收命令

实现阶段至少跑：

```bash
cargo test --lib
cargo test -p aox-gui
cargo build --release -p aox-gui
```

交付前按项目习惯做隔离根（复制 exe 到无 `Cargo.toml` 祖先的临时目录），不要在真实
`history/` 上做冒烟测试。

## 17. 分阶段落地

### 阶段 R0：契约与空实现（不改行为）

- 定义 `RecoveryPoint` / `FileOp` / `BlobRef` / 配置 schema。
- 只做 unit test 与 `--dump-config` 可见性；默认 `enabled=false` 或完全不注册。
- 验收：默认配置行为与今天逐字节一致。

### 阶段 R1：精确主机工具与线性回滚

- CAS、节点、cursor、journal、锁、`/checkpoints`、`/rollback`、`/redo`。
- 先接 `write_file` / `edit_file` / `remove`。
- 两阶段预检、drift 拒绝、safety redo。
- 验收：单文件/多文件、新建/删除、崩溃恢复、二进制与 CRLF。

### 阶段 R2：opaque shell 捕获

- 前后对称扫描、元数据索引、内容 hash、所有退出分支 finally。
- strict/best_effort 配置与 `coverage` 语义。
- 与现有 `track_changes` 解耦；旧选项保留为纯 UI 变更提示，不再承担回滚正确性。
- 验收：同尺寸重写、命令超时、排除目录、未跟踪提示。

### 阶段 R3：GUI、GC 与保留

- 恢复点浮层、预览、冲突确认、事件回执。
- 可达性 GC、quota、trim、pin。
- 验收：手动 UI 流程、崩溃后重启、空间耗尽、两个 AOX 竞争。

### 阶段 R4：插件与后台任务的诚实降级

- 插件工具 before/after 扫描；`shell_job` 明确的 partial 标识。
- 若后续实现文件系统 watcher/USN，可将 job 提升为独立 watch 节点；在此之前不承诺。
- 验收：插件声明 read 却写盘时仍能被外部 drift 发现；job 写文件时回滚提示不完整。

## 18. 被否决的方案

| 方案 | 为什么不选 |
|---|---|
| 每个检查点复制整个工作区 | 磁盘随任务数倍增，首次大仓库不可接受；内存不是主要问题，磁盘和 IO 才是 |
| 只存 unified diff / 文本反补丁 | 二进制、换行、编码、删除、重命名、上下文漂移都容易失败；补丁链越长越脆 |
| 用 Git 自动 commit 作为检查点 | 会污染用户仓库 history/index/ref，需处理未跟踪、忽略、子模块、大型二进制；用户明确要求架构期不自动 commit |
| 直接调用 VSS/卷影副本 | Windows 需要管理员/服务，不能作为普通用户默认能力；跨平台更不可行 |
| 每个任务保留完整 manifest（文件 hash 列表） | 仍要全量扫描且不能恢复被覆盖内容；适合审计，不适合作为回滚主数据 |
| refcount 删除 blob | 崩溃、复制目录、并发 GC 后计数易漂移，最终误删恢复所需对象 |
| 自动三方合并外部修改 | 无共同祖先的语义不可靠；会静默制造“看起来成功、实际混合”的文件 |
| 用模型判断该不该回滚 | 回滚是用户级、破坏性操作，必须可审计；模型不能成为安全边界 |

## 19. 最终承诺（务必按此措辞对外）

> AOX 会在任务修改工作区文件前保存旧内容，并按任务记录成可去重的恢复点。你可以回退最近
> 一个任务或前 n 个任务；AOX 会先展示将改动的路径，发现外部改动时默认不覆盖。它保护当前
> 工作区内的常规文件内容，不替代 Git，也不覆盖构建目录、工作区外写入、运行中的后台进程、
> 数据库内部状态或完整的 Windows 文件系统属性。

### 19.1 必须写进 `/help` 或文档的简短限制

- 只回滚 AOX 记录过的路径，不重置整个工作区。
- `.git`、`target`、`node_modules`、`.aox` 默认不在保护范围。
- `shell_job` 运行期间的文件写入不保证可回滚。
- 外部编辑器同时写入时，恢复可能中止；这是保护，不是 bug。
- 回滚文件不会回滚聊天记录；恢复后旧的 `verify` 证据失效。
- 备份包含工作区文件内容，可能含密钥；备份目录应与源码同等保护。

## 20. 仍需用户拍板的三个选择

1. **默认开关**：`[recovery] enabled` 默认 `true`（安全优先，但所有写入多一次磁盘读）
   还是 `false`（零漂移优先，由用户主动开启）？
   建议默认 `true`，但只对主机文件工具和显式 `shell_guard` 生效；无写入任务零成本。
2. **`keep` 默认值**：建议 `30` 个 turn。项目仓库当前源码体量下，去重后通常远低于 512 MB；
   若用户经常改大二进制，需要单独压到 10。
3. **是否允许 `--force` 覆盖 drift**：建议允许，但必须在 GUI 红色二次确认、CLI 要求
   `--force` 明写，并先落 safety；默认路径永远不覆盖外部改动。

---

**结论：** 这套机制的核心不是“备份很多文件”，而是“在正确的时刻保存不可变的前像，并让
每个恢复点只描述自己真正改变的那几条路径”。它把磁盘增长压到内容去重后的净增量，把内存
压到固定缓冲区加有界索引，把恢复风险压到“预检失败就不写”。这比在工作区里堆整份副本更
精巧，也比纯 diff 链更耐崩溃、更适合二进制与长期使用。
