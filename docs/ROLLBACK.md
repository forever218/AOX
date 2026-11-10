# Task-level checkpoints and rollback for AOX

> Status: **design only, not implemented** (2026-10-25).
> This document changes no code. Type names and field spellings may move during implementation,
> but the semantics below must not silently change.

## 0. Summary

Do not copy the whole workspace per checkpoint, and do not rely on text reverse-patches. Use:

**one path-level reverse journal per user task, with before/after file contents stored in a
BLAKE3-addressed object store shared by all checkpoints; rollback walks the task chain backwards,
preflights every affected path, and saves the current state as an independent redo snapshot.**

This gives four properties at once:

- memory is independent of file and workspace size: only metadata is resident;
- identical content is stored once across any number of checkpoints;
- rollback only touches paths a task actually changed, never wipes the workspace;
- a crash can be resumed or rejected explicitly, never mistaken for a successful checkpoint.

It is **not** a point-in-time filesystem snapshot. That distinction must be visible in the product
copy, or users will assume it replaces Git, database transactions, or VSS.

## 1. Constraints in the current code

| Fact | Location | Consequence |
|---|---|---|
| One user turn is entered and closed by `run_turn`, on success and failure | `src/agent.rs:610-671` | Natural automatic checkpoint boundary; failed turns must still commit disk changes |
| `task_state` is model-written progress, not file state | `src/task.rs` | Cannot be used as a recovery point; the two need different names and lifetimes |
| Host file tools are `read_file` / `write_file` / `edit_file` / `list_dir` / `remove` | `src/tool/hostfs.rs`, `src/tool/edit.rs` | The host can capture an exact pre-image before it writes |
| `edit_file` validates the whole batch in memory and replaces the file once | `src/tool/edit.rs:178-208, 427-448` | Capture around one tool call, not around each edit |
| `shell` only does a bounded post-hoc mtime scan and skips `.aox`, `target`, etc. | `src/tool/shell.rs:185-196, 742-780` | A post-hoc scan cannot recover overwritten content; a pre-command image is required |
| `shell_job` keeps running after `start` | `src/tool/job.rs` | The first version cannot promise turn-level consistency for a live job |
| Plugins are black boxes; `access: "read"` is a declaration, not enforcement | `docs/SANDBOX.md`, `src/plugin/mod.rs` | Plugin writes can only be best-effort and must be labelled as such |
| Workspace history lives in `<workspace>/.aox/history`; one session handle writes it | `src/store/jsonl.rs` | Backup must never roll back `.aox` itself |
| `verify` evidence expires after a file write; `task_state` has a workspace epoch | `src/agent.rs`, `src/checks.rs` | Restore must invalidate old proof, not preserve a stale "done" |

Two proven traps:

1. **mtime plus size is not sufficient.** Same-size in-place rewrites, restored timestamps, and
   delete-and-recreate can fool it. Metadata may reduce hashing; it cannot be the final proof.
2. **Do not record changes from `ToolOutput.path`.** A plugin can write without declaring a path
   in its result, and shell writes are entirely invisible there. Capture must wrap execution.

## 2. Goals and non-goals

### Goals

- Every user task that produces a **net file change** becomes one recovery point. No change, no node.
- Roll back one task, the previous n tasks, or an explicitly named checkpoint.
- Preview paths before restoring; refuse by default on external drift.
- Rollback itself is undoable (redo), without polluting the normal task chain.
- Survive crash or force-kill: startup resolves or rejects incomplete transactions.
- Hard memory budget; stream large and binary files; deduplicate identical content.
- Protect the active workspace only, never follow links outside it, never back up `.aox`, `.git`,
  or build caches by default.
- CLI and GUI share one implementation and one body of copy.

### Non-goals (first version)

- No Git replacement: no commits, index, refs, stash, or `.git` contents restored.
- No registry, database pages, network effects, process state, or environment variables.
- No ACL, owner, SACL, NTFS ADS, EFS, hard-link topology, or junction targets.
- No guarantee for writes outside the workspace (`full` mode).
- No guarantee while a `shell_job`, watcher, or third-party editor is actively writing.
- No automatic three-way merge; external work is not disguised as an AOX task.
- No absolute power-loss atomicity. Metadata is ordered durably; corruption is reported, not hidden.
- Chat history is not rolled back automatically; chat and workspace remain two audit trails.

## 3. Vocabulary

| Name | Owner | Contents | Restores files? |
|---|---|---|---|
| `TaskState` | model | goal, steps, acceptance, next | no |
| `RecoveryPoint` | host | paths changed by one task + before/after versions | yes |
| `SafetySnapshot` | host | current versions of paths about to be restored | only for redo/failure rollback |

Throughout this document, "checkpoint" means `RecoveryPoint`. Do not name it `task` in code or UI:
the existing JSONL `kind:"task"` already means the model's progress checkpoint.

### 3.1 What one "task" means

> **One `run_turn` that produced a net file change = one automatic recovery point.**

This is the only boundary the host can observe without trusting the model. It is finer than
`TaskState.task_id`: one long goal may create several recovery points, because "go back one task"
usually means "undo the previous turn".

Failed, timed-out, or interrupted turns still commit if they changed files, marked
`status = failed | interrupted`. Losing them because there was no final answer would be wrong.

### 3.2 What `/rollback n` means

> Move the cursor to **before the nth most recent `kind=turn` node**, undoing every node traversed,
> including interleaved `external` nodes.

It does not mean n chat messages, n `TaskState` goals, or a complete point-in-time workspace image.
Unrelated files, external changes to untouched paths, and build outputs remain as they are.

## 4. Architecture

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

Three responsibilities:

1. **Capture**: before a write, the old content is already an immutable object. If capture fails,
   the original write must not happen.
2. **Journal**: the operation order is durable; a crash can be completed or explicitly abandoned.
3. **Store**: immutable nodes, content-addressed blobs, `cursor.json`, independent redo snapshots,
   and garbage collection.

Suggested modules:

```text
src/recovery/
  mod.rs       RecoveryManager, RecoveryTxn, RollbackPlan, RestoreReport
  store.rs     blob/node/cursor/lock/journal atomic I/O
  capture.rs   exact host capture, opaque pre/post scan, path rules
  restore.rs   preflight, apply, undo/redo, failure rollback
  gc.rs        reachability marking, quotas, retention
  model.rs     RecoveryPoint / FileOp / BlobRef / Coverage
```

Do not put everything in one `backup.rs`. Capture and restore fail in different ways; coupling them
turns a GC fix into a write-path risk.

## 5. On-disk layout

Let `WS` be the workspace root.

```text
WS/.aox/backup/
  format.json                    # {"schema":1,"workspace_id":"...","created_at":...}
  lock                           # OS exclusive lock, released on process death
  cursor.json                    # current cursor; the only mutable pointer
  index/refs.json                # recent-node cache, rebuildable, never truth
  nodes/rp_20261025T172839Z_7f3a.json
  objects/ab/cd/abcdef...        # BLAKE3-256 content objects, immutable
  journals/tx_20261025T172900Z_91c2.jsonl
  redo/redo_20261025T173500Z_2d10.json
  baseline/manifest.json         # strict-shell current-scope baseline: path -> content ref
  tmp/                           # same-volume temps; >24h leftovers removed at startup
  quarantine/                    # current files moved aside during restore, cleaned on success
```

Rules:

- `objects/` and `nodes/` are append-only. "Editing" a node means writing a new node.
- `cursor.json` alone records where we are, and is updated last.
- `format.json.workspace_id` detects a backup directory copied from another workspace. Node paths
  are workspace-relative with `/`, never absolute.
- `.aox/backup` is the first exclusion. `.aox/history`, `.aox/memory`, and `.aox/tmp` are also
  excluded; chat history and agent notes have their own lifetimes.
- `quarantine/` is a failure-rollback staging area, not a user trash can. The safety snapshot already
  holds deleted content.
- All writes use same-directory temp + flush + `sync_all` + atomic rename (Windows
  `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` or `ReplaceFileW`). A node is published only after every
  blob it references is durable.

## 6. Data model

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

- `hash` is BLAKE3-256 over **uncompressed** bytes; the object filename is the hash. Verify after
  decompression.
- `stored:false` means only hash/size were recorded, without the object. It still supports drift
  preflight but cannot redo; the node must have `redoable:false`.
- Identical content is one object; `mode` is not part of addressing.
- Start with `codec:"raw"` if needed; compression is an optimization, not correctness. zstd must be
  streamed and its binary-size cost measured before becoming a dependency.
- Do not use the existing FNV-1a fingerprint as a backup address. It is a change detector, not an
  integrity or identity primitive.

### 6.2 FileOp

```json
{
  "path": "src/main.rs",
  "kind": "modify",
  "before": { "hash": "...", "size": 1200, "codec": "zstd", "mode": "100644" },
  "after":  { "hash": "...", "size": 1234, "codec": "zstd", "mode": "100644" }
}
```

| `kind` | before | after | Undo |
|---|---|---|---|
| `create` | null | BlobRef | remove the path |
| `modify` | BlobRef | BlobRef | write `before` |
| `delete` | BlobRef | null | recreate and write `before` |
| `type_change` | one type | another | move current aside, restore old; first version may refuse |

- Rename is old-path `delete` plus new-path `create`; no separate rename operation.
- Repeated writes to one path in one task keep the **first before** and **last after**. This is what
  makes undo idempotent and keeps nodes bounded.
- `after` may be absent only when post-capture failed or quota blocked it. Prefer retaining its
  hash/size with `stored:false`: drift preflight still works, but redo does not. Mark
  `redoable:false` and say so in the UI.
- Directories are not file ops. A deleted tree records one `delete` per regular file. Restoring empty
  directories is best-effort, not a guarantee.
- Do not follow symlinks/junctions. A link change is `type_change` and not guaranteed. Targets
  outside the workspace are out of scope.
- Hard links are recorded per path. Restoring may split the link; this is an explicit limitation.

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
  "label": "add rollback to the CLI",
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

`kind`:

- `turn`: automatic task node, counted by `/rollback n`.
- `external`: bridges a tracked path whose current content differs from the previous node's `after`
  before a new task starts. It does not count as a task, but is undone when a rollback crosses it.
- `manual`: a named anchor created by `/checkpoint <name>`, usually with no ops and not counted.
- `genesis`: implicit start, no file. Rolling back to it means undoing the first task only; it never
  deletes files that existed before that task.

`status`: `completed`, `failed`, `interrupted`, `recovered`.

`coverage.state`:

- `complete`: within the declared scan scope, pre/post scans were symmetric and all pre-images durable;
- `partial`: an unsupported reparse point, a file over budget, a live job, or unknown excluded change;
- `unknown`: crash recovery could not establish intermediate state.

`partial`/`unknown` nodes remain visible and can undo known ops, but automatic `/rollback` refuses to
cross them without `--allow-partial` and a red confirmation. Otherwise "rollback succeeded" is false.

### 6.4 Journal

Every transaction creates `journals/<tx>.jsonl`, append + flush, with `sync_all` on critical rows:

```json
{"t":"begin","tx":"tx_...","parent":"rp_...","workspace":"...","conversation":"...","at":1792920519}
{"t":"intent","tool":"write_file","path":"src/a.rs","before":"...","after":null}
{"t":"commit","path":"src/a.rs","after":"...","kind":"modify"}
{"t":"finish","node":"rp_...","at":1792920531}
```

- `intent` is durable **before** the original write; `before` must already be a verified object.
- `commit` is written after the tool, even if the tool reported an error but changed bytes.
- Publish node, then cursor, then remove the journal. A failed removal is harmless.
- A corrupt journal is preserved and the transaction marked `unknown`; never guess.

## 7. Capture

### 7.1 Tool matrix

| Entry | Capture | Default guarantee | Notes |
|---|---|---|---|
| `write_file` | exact read before, exact read after | exact | Parent-directory creation is not a file; record only if needed |
| `edit_file` | read original once, final once | exact | Reuse the existing single atomic replace |
| `remove` | enumerate and save every file before recycle/delete | exact per file | Must run before the recycle-bin call |
| `shell` / `bash` | workspace pre-scan + symmetric post-scan | best-effort within scope | Also on non-zero, timeout, idle kill, spawn failure |
| `shell_job` | not included in automatic nodes | explicitly not guaranteed | It writes asynchronously beyond turn end |
| Plugin tool (declared write/undeclared) | pre/post scan | best-effort | Declared `read` plugins are trusted; if they lie, no guarantee |
| `create_plugin` / `update_plugin` | exact directory capture if inside the workspace | outside scope otherwise | Plugin dirs usually live under the app root |
| User/external process | detected as drift before the next task | protects current content | Never disguised as an AOX task |

### 7.2 Why host tools capture inside `call`

The workplace check runs both in `Agent::authorize_and_execute` and in each tool. Passing the check
does not mean a write happened. Capture must sit after `guard` and before the real write:

```text
parse args -> guard/workplace -> capture_before() -> mutate -> capture_after() -> journal
```

Reasons:

- the authorization layer cannot know a write will be a no-op; capturing there creates empty nodes;
- hooks may rewrite arguments, so the authorization layer sees a different target;
- `selftest` calls `Runtime::execute` directly and bypasses `Agent`;
- only the tool knows whether it really wrote, how many times, and whether it failed after writing.

If `capture_before` fails, the tool returns an error and **the original write does not execute**.
The error names the path, size, failed reason, current exclusions, that the file is untouched, and
how to fix it.

### 7.3 Shell pre/post scan

Do not reuse the existing post-hoc `track_changes` scan as the source of truth.

```text
1. acquire the workspace transaction lock
2. walk the declared scope, build a before manifest (path, type, size, mtime, ctime/change time,
   Windows file id/volume id; strict still streams BLAKE3 over every regular file, without using
   metadata as a shortcut)
3. reuse baseline objects by hash; write missing content to CAS. The first strict call establishes
   the baseline for the whole scoped tree
4. persist manifest + ready marker and fsync
5. start the command
6. in a finally block, walk again with the same rules and build the after manifest
7. diff new/deleted/type/metadata/content changes
8. aggregate FileOps with first-before and last-after
9. commit the node
```

- **Symmetric scans**: identical exclusions, depth, and type rules. Asymmetry creates phantom
  files or deletions.
- **Metadata never proves equality.** Strict hashes every regular file before the command and
  rehashes changed/suspected paths after it; the final decision is hash-based. Only `best_effort`
  may skip reads on unchanged metadata, and it must mark the node `partial`.
- **Same content with only mtime changed is not a net change**; omit it.
- **The first opaque call needs a workspace pre-image.** That is the unavoidable cost of allowing
  arbitrary shell writes. Strict mode keeps **one** `baseline/manifest.json` whose entries reference
  current content objects. Later commands still hash the whole tree, but hash-equal files reuse
  those objects instead of rewriting the tree. The baseline is a GC root, **not a recovery node**.
  All tasks share it; each historical node still references only paths it changed.
- Baseline size is roughly one copy of the scoped source tree, not one per task. The first strict
  shell, or the first after switching workspace, establishes it. If that cost is unacceptable, use
  `best_effort` or `off` and accept the weaker guarantee. Metadata memory and total quota include
  the baseline.
- `best_effort` saves pre-images only for files known changed before the command. Writes discovered
  only afterwards have a hash but no recoverable old bytes, so the node is `partial` and automatic
  rollback may not cross it.
- Across tasks, strict performs another full hash scan. After the command the baseline is replaced
  and unreferenced old objects become GC candidates, while objects still held by historical nodes
  survive.
- If file count, bytes, metadata memory, or scan time exceeds budget: do not start the command in
  strict mode, or run it and mark the node `partial` in explicit best-effort mode. Never silently
  weaken and still claim completeness.
- A directory that changes wildly during a scan gets one retry; if still unstable, mark `partial`.
- On timeout/force-kill, the Job Object kills the process tree, but disk writes may already have
  landed. The finally scan still runs.
- While a `shell_job` is active, any task scan can absorb its asynchronous writes. Mark the node
  `partial` and tell the user to stop the job before relying on rollback.

### 7.4 Exclusions

Default exclusions:

```text
.aox/
.git/ .hg/ .svn/
target/ node_modules/ .venv/ venv/ __pycache__/
.pytest_cache/ dist/ build/ out/
*.tmp *.swp *.lock
```

Exclusions are not an excuse to hide user changes. At commit, if excluded scopes changed, show
"1 change in an excluded scope, not covered by rollback" in the preview. This is configuration, not
a hard-coded constant; a project that treats `dist` as source can opt in and pay the storage cost.

### 7.5 Path identity

- Store workspace-relative paths with `/`, preserving case.
- Compare with platform semantics: Windows generally case-insensitive, but do not assume it for UNC
  or case-sensitive directories. Reuse/extend `workplace::normalize`; do not write a second normalizer.
- Optionally record `file_id` (Windows `FileIdInfo`, Unix `dev+ino`) as secondary evidence. Content
  hash is the primary identity.
- Reject `..` escapes and paths outside the root; never follow reparse points. Record unsupported
  links and mark partial.
- Restore through wide-character paths; test >260 characters.
- A workspace may move; nodes are relative. A backup directory with a different `workspace_id` must
  be rejected, not restored by relative-path coincidence.

## 8. Commit and crash recovery

### 8.1 Commit order

Order is correctness:

```text
1. acquire workspace OS lock
2. every before/after blob: temp -> flush -> sync_all -> verify hash -> atomic publish
3. node JSON: temp -> flush -> sync_all -> atomic rename
4. cursor.json: temp -> flush -> sync_all -> atomic rename (last)
5. remove journal (leftovers cleaned next start)
```

- Crash after node publish, before cursor: startup compares node `after` with disk. Match means
  advance cursor; mismatch means mark `recovered/unknown`, never auto-advance.
- Crash after blobs, before node: the journal's `intent` lets startup scan current state and create
  a `recovered` node if it can prove net changes; otherwise keep the journal and report.
- Capture failure before the write: no write, no node.

### 8.2 Startup state machine

| Current workspace | Action |
|---|---|
| Every path equals `before` | Transaction never wrote; drop journal, no node |
| Every path equals `after` or a known intermediate | Commit a `recovered` node |
| Some known, some unknown | Node with `coverage=unknown`; refuse automatic rollback across it |
| Missing blob / corrupt journal | Preserve state and report; do not clear or guess |

Only run recovery while holding the workspace lock. No lock means another AOX is active; do not steal.

### 8.3 Safety snapshot and redo

Before a restore, save current versions of the paths to be changed. Do **not** append this to the
normal parent chain: doing so breaks `/rollback n` counting and redo direction.

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
  ]
}
```

- The safety record is durable **before the first restore write**. Failure to persist it aborts.
- `/redo` preflights that current state equals `target_cursor`, restores only safety paths, then
  moves the cursor back to `from_cursor`.
- A new task clears the redo stack; old safety records become GC candidates. Never keep a phantom
  redo whose parent no longer matches.
- Sequential rollbacks stack safety records LIFO, capped at 8; dropping the oldest is reported.

### 8.4 External drift bridge

When a new task first touches an already tracked path:

```text
previous node's recorded after(H) != current content X
```

Create an `external` node with `before=H`, `after=X`, `kind=external`, parent=current cursor, and
make the new task its child. This records external work honestly:

- undo of the task returns to X, not H;
- rollback further back previews "also undo 1 external change";
- `external` is not counted in n but is traversed on rollback.

If the path has no previous tracked state (pre-existing file at genesis, or a new untracked path),
do **not** create an `external create`. The task's own `before` already preserves it. Recording it
as AOX-created would delete a user's pre-existing file when rolling past genesis.

## 9. Rollback

### 9.1 Resolve target

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

`external`/`manual` are traversed but not counted. Exceeding retained history is an error, never a
silent partial rollback.

### 9.2 Phase A: read-only preflight

1. Walk cursor -> target, collect undo operations, collapse per path to the state expected at the
   current edge.
2. Verify chain continuity. `external` is a legitimate bridge and must not be skipped.
3. Verify every blob exists, has the right size, and matches BLAKE3 after decompression.
4. For each affected path compare current type/existence/hash with expectation. Missing vs expected,
   or present vs expected absent, is drift. Same content but different mode is restorable with a note.
5. Verify same workspace/volume and no excluded target.
6. Refuse if a turn, restore, or active background job is writing.
7. Render preview: modified/created/deleted, byte sizes, drift, excluded untracked changes.

Any drift or incomplete coverage stops **before the first byte is written**. Suggest saving the
external change, rolling back to an earlier point, or `--force` with a safety snapshot and red
confirmation.

### 9.3 Phase B: apply

1. Reacquire lock; re-hash critical preflight paths to shrink the TOCTOU window.
2. Persist safety and `sync_all`.
3. Write `journals/restore-*.jsonl` with target, safety, each path, and progress.
4. Apply in reverse order:
   - delete AOX-created files by renovating into `quarantine/<restore>/...`, not direct deletion;
   - restore/overwrite through same-directory temp, flush, permissions, atomic rename;
   - create only required parent directories;
   - remove newly created empty directories best-effort.
5. Append `applied` per path.
6. On success, update cursor; clean quarantine and the restore journal, but **retain safety** for
   `/redo`. Safety becomes a GC candidate only after a new task invalidates the redo stack.
7. On failure, roll applied paths back from safety, itself journaled. Keep safety so the user can
   retry or return to the pre-restore state.

Multiple files cannot be truly atomic across an OS filesystem. We promise **convergent batch
restore**, not absolute atomicity: preflight before writes, journal every step, resume or hold on
restart, roll back from safety on failure.

### 9.4 Agent state after restore

Chat history is not rolled back by default. Append a host notice visible to model and UI:

```text
The workspace was restored to rp_... (2 tasks undone, 5 paths).
Old task steps, verification evidence, and tool output may be stale.
File contents win; re-read affected files and rerun acceptance before continuing.
```

Also:

- invalidate all `verify` evidence for the conversation (workspace epoch or equivalent);
- do not keep stale `modified_files` as current fact;
- never auto-mark `task_state` done;
- the next modification creates a new node whose parent is the restored cursor;
- the GUI refreshes affected views only after completion, not per path.

`/rollback --with-chat` is out of scope; it would involve `Session::rewrite`, act/usage retention,
and interrupt rollback semantics, with much higher blast radius.

## 10. Retention, quotas, GC

Defaults:

```toml
[recovery]
enabled = true
keep = 30                 # maximum undoable turn nodes on the current branch
max_total_mb = 512
max_file_mb = 64          # strict refuses a larger pre-image
max_turn_mb = 256
metadata_memory_mb = 32
shell_guard = "strict"    # strict | best_effort | off
protect_plugins = true
```

`keep` counts undoable turn nodes from the cursor, not files in a directory. Retained ancestors
cannot be pruned by time alone.

Prune only nodes unreachable from the cursor, except that exceeding `keep` drops the oldest ancestor
prefix and marks the oldest survivor `truncated=true`. Persist the reduced effective depth before
deleting nodes; otherwise a cursor can point at a deleted node. On quota pressure: prune unreachable
nodes first, then oldest ancestors only down to `keep` without explicit `--force`, and finally refuse
new capture. Never delete the current task's `before` to make room.

No refcounts. Use mark-and-sweep because a crash, directory copy, or concurrent GC can make counts
drift, and the symptom is missing blobs during restore.

Roots:

```text
cursor and all ancestors
+ the strict-shell baseline manifest and its blobs
+ nodes/blobs referenced by every redo/safety record
+ blobs referenced by incomplete journals
+ blobs referenced by an in-progress restore plan
+ manually pinned nodes
```

GC holds the workspace lock and runs only when no turn/restore is active. Failure is non-fatal.

### Example

```text
a.rs = 10 KiB(A)
T1: a.rs -> B(10 KiB), create b.rs(B)
T2: a.rs -> C(11 KiB), b.rs -> D
T3: only c.rs
chain: genesis -> T1 -> T2 -> T3
```

- `/rollback 1`: undo T3 only; a.rs/b.rs untouched.
- `/rollback 2`: undo T3, T2; a.rs -> B, b.rs -> B.
- `/rollback 3`: also undo T1; a.rs -> A, b.rs removed.
- If the user manually changed a.rs to X between T1 and T2, the next task touching it creates an
  `external` node; `/rollback 1` returns to X, and the preview says so.

## 11. Memory, disk, performance

- Fixed 128 KiB stream buffers for copy/hash/compression; memory does not grow with file size.
- Scan index is `BTreeMap<RelativePath, FileState>`, roughly 100-300 bytes per entry. Default
  32 MiB; exceeding it refuses strict mode rather than risking OOM.
- A task aggregates only changed paths. Opaque scan indexes are released after commit.
- CLI/GUI lists read `index/refs.json`; details load one node on demand.
- Restore preflight holds only paths and BlobRefs, never file bodies.

Disk overhead: one strict-shell baseline for the scoped tree, first before/after per changed path
per task, identical content shared across tasks, optional compression, excluded build outputs, hard
quotas.

Typical cost: host single-file edit is a read + hash + object write + node write, milliseconds
against a model round trip. Every strict opaque shell call reads and hashes the scoped tree, which
is inherent to arbitrary shell recovery; only the first also writes the full baseline, later calls
write just new objects. Rollback is linear in affected paths, not workspace size. A no-change task
writes only journal begin/finish, or nothing if no write hook fires.

## 12. Integration points

| Location | Change |
|---|---|
| `Agent::run_turn` (`src/agent.rs:610-671`) | Begin transaction after workplace root is fixed; finish on both success and failure; drop empty transactions |
| `Agent::authorize_and_execute` | No capture before authorization; capture lives in tools so hooks cannot misdirect it |
| `src/tool/hostfs.rs` | Capture before/after for write/remove, including error paths |
| `src/tool/edit.rs` | Capture around `write_atomically`; preserve all-or-nothing batch semantics |
| `src/tool/shell.rs` | Wrap `prepare -> spawn -> wait -> finally scan`; cover every exit branch |
| `src/tool/job.rs` | Mark active job as `partial`; no async tracking in v1 |
| `Runtime` | Hold a `RecoveryManager`; keyed by workspace, not a process-global singleton |
| `src/store/jsonl.rs` | Optional `kind:"recovery"` row for audit; preserve it in `rewrite` |
| `src/tool/verify.rs` / `task_state` | Invalidate evidence; never rewrite model progress text |
| CLI help | Add `/checkpoints`, `/checkpoint`, `/rollback`, `/redo`, `/backup` |
| GUI bridge | Checkpoint list, preview, confirmation, receipts; disk work stays on agent thread |
| Config layer | Add `[recovery]` to bundles/defaults; decide enabled-by-default separately |

Concurrency:

- One `RecoveryTxn` per workspace per process. Today there is one agent thread; future parallel
  conversations must not share a cursor transaction.
- Cross-process OS lock on `.aox/backup/lock`. Acquire it at the **first write that needs capture**
  and hold it until the turn commits; do not lock at message send, and do not hold it only for the
  cursor update. No lock means another AOX is writing; report, do not steal. Revalidate that the
  cursor still equals the transaction parent before commit; otherwise abort with a conflict.
- External editors ignore the lock. Preflight reduces but cannot eliminate TOCTOU. Tell the user not
  to edit during restore.
- Serialize journal appends if host tools ever run concurrently; first before wins per path. V1 can
  keep tool execution sequential.

Workspace switching:

- Bind transaction to `workspace_id + canonical root`; finish or abort before switching.
- `/workplace`, closing a conversation, and exit all use the same finish/abort path.
- `full` mode writes outside the workspace cannot be stored as workspace-relative paths. Refuse to
  track them and warn that they are unprotected.
- In `default` mode the root is fixed on the first message; order must be root -> transaction ->
  user message.
- A workspace that moves is fine because paths are relative; a different `workspace_id` is not.

## 13. CLI and GUI

```text
/checkpoints                 last 20 points: id, time, label, path count, status
/checkpoints --all           include external/manual/truncated
/checkpoint "before refactor"
/rollback                    preview the previous turn, do not execute
/rollback 2                  preview two turns
/rollback rp_...             preview a specific point
/rollback --yes              execute if no drift
/rollback --force            save safety and overwrite drift after red warning
/redo                        restore the state before the last rollback
/backup status               space, node count, earliest restorable, open journals
/backup verify               verify every blob in the current branch
/backup prune                collect unreachable objects
```

`/rollback` previews by default. File restore deserves more friction than a scroll error. With
drift, the default is refusal.

GUI: one compact popover next to the existing workspace control. Rows show time, one-line label,
`+N/-M`, and status. Selecting shows paths and a conflict badge. No conflict -> normal primary
button; drift/partial -> red and an extra confirmation. Progress and completion are host events;
failures name the stuck path and how many files were/weren't restored.

The model cannot call rollback as a tool. It is a user-level destructive action; automatic
self-undo would create unauditable state jumps.

## 14. Security and privacy

- Backups may contain secrets and private source. Keep them local under the workspace, never put
  blob content in LLM context or chat history, never upload.
- Object files inherit workspace permissions. Do not claim encryption; content addressing is not
  encryption.
- Excluding `.git` means Git internals are not protected, though the worktree is.
- Restore writes only inside the workspace and refuses reparse-point escapes.
- Never log file content, secrets, or tokens into labels/errors. Truncate and escape user labels.

## 15. Failure behavior

| Failure | Required behavior |
|---|---|
| Out of backup space | strict: refuse before write, original untouched; opaque already ran: node `partial`, explicit warning |
| Missing/corrupt blob | preflight aborts before any write; verify surfaces the node |
| External drift | default abort with path list; `--force` saves safety first |
| Crash during restore | journal + safety; resume or roll back; never report success |
| File locked | per-path report; no unbounded retry; applied paths can be rolled back |
| Workspace root moved | reject restore; do not guess by relative path |
| Two AOX instances | lock failure with clear message; never merge cursors |
| Background job writing | `partial`, tell user to stop it; no completeness claim |
| Excluded path changed | preview shows untracked change |
| `task_state` disagrees with disk | file wins; evidence expires; model re-reads and re-verifies |

## 16. Test plan

Pure logic:

- normalization: case, slashes, `..`, Unicode, long paths, reserved names;
- aggregation: first before / last after; no net change -> no node;
- n resolution across turn/external/manual; out of range errors;
- preflight: modify/create/delete/type change, drift, missing blob;
- redo: single, sequential, LIFO safety, invalidation after new task;
- retention: truncated semantics, pin survives, quota never drops current before;
- GC: shared blobs survive; redo/journal roots survive.

Filesystem, in temp dirs:

- write create/overwrite/directory collision/read-only/parent creation;
- edit batch success and failure, CRLF, BOM, binary rejection leaves original untouched;
- remove file/tree/empty dir/recycle failure;
- large file > `max_file_mb`, streaming memory does not grow;
- shell success/nonzero/timeout/idle kill/spawn failure, same-size rewrite, delete-recreate,
  mtime-only change;
- excluded dirs produce no node ops but do show an untracked-change hint;
- external modification creates an `external` bridge;
- symlink/junction not followed and marked partial;
- locked target fails with safety able to roll back already-applied paths.

Crash and concurrency:

- kill during blob write, node publish, cursor update; startup matches Section 8.2;
- kill restore after preflight, after first file, before last; resume or roll back;
- two processes contend for the workspace lock;
- external write between preflight and apply is caught by second hash (remaining TOCTOU documented).

Acceptance commands during implementation:

```bash
cargo test --lib
cargo test -p aox-gui
cargo build --release -p aox-gui
```

Use an isolated root for smoke tests: copy the exe somewhere with no `Cargo.toml` ancestor. Never
exercise this against the real `history/`.

## 17. Rollout

- **R0 contract**: types, config schema, unit tests, default-off or unregistered. Behavior byte-identical.
- **R1 exact host tools**: CAS, nodes, cursor, journal, lock, CLI list/rollback/redo, preflight,
  drift refusal, safety redo. Cover write/edit/remove, binary, CRLF, crash.
- **R2 opaque shell**: symmetric scan, metadata index, content hash, finally on all exits,
  strict/best-effort coverage. Decouple `track_changes`.
- **R3 GUI + GC**: popover, preview, confirmation, events; reachability GC, quota, trim, pin.
- **R4 plugins/jobs**: plugin scan; explicit partial for `shell_job`; a future watcher/USN may
  promote jobs to their own watch nodes.

## 18. Rejected alternatives

| Option | Why not |
|---|---|
| Full workspace copy per checkpoint | Disk and IO multiply per task; unacceptable on real repos |
| Text unified diff / reverse patch only | Binary, encoding, newline, deletion, rename, and context drift make long chains fragile |
| Auto Git commit per task | Pollutes history/index/refs, must handle untracked/ignored/submodules/LFS; also forbidden by current no-auto-commit policy |
| VSS / volume shadow copy | Needs admin/service on Windows; not a portable default |
| Full manifest per checkpoint | Still needs a full scan and cannot recover overwritten content; audit is not restore data |
| Refcounted blob deletion | Counts drift after crashes/copies/concurrency and eventually delete needed blobs |
| Automatic three-way merge of external edits | Without a sound base, silently mixes files and looks successful |
| Let the model decide when to roll back | Destructive, user-level action must be auditable; model is not a security boundary |

## 19. Public promise

> AOX saves old file contents before a task changes workspace files and records them as deduplicated
> recovery points. You can go back one task or the previous n tasks. AOX previews affected paths and
> refuses to overwrite external changes by default. It protects regular file contents inside the
> active workspace; it does not replace Git and does not cover build directories, writes outside the
> workspace, running background jobs, database internals, or full Windows filesystem attributes.

Short limitations for `/help`:

- only AOX-recorded paths are undone, not the entire workspace;
- `.git`, `target`, `node_modules`, `.aox` are excluded by default, and so are executable images
  (`*.exe`/`*.dll`/`*.pdb`) — including the running one, unconditionally;
- writes by a running `shell_job` are not guaranteed;
- external editors can make restore abort; that is protection, not a bug;
- chat history is not rolled back, and old verification evidence expires;
- backups may contain secrets and need the same protection as the source.

## 20. Three choices for the user

1. **Default switch**: `enabled = true` (safe, but every write adds a disk read) or `false` (zero
   drift, opt-in)? Recommendation: `true`, with capture only for host file tools and explicit
   shell guard; no-write tasks cost nothing.
2. **`keep` default**: 30 turns is recommended. Deduplicated source usually stays far below 512 MB;
   large binary workflows should lower it to 10.
3. **Allow `--force` over drift**: recommended yes, but only with an explicit flag, a red GUI
   confirmation, and a durable safety snapshot. The default path never overwrites external work.

---

**Conclusion:** the point is not "back up many files"; it is "save immutable pre-images at the
right moment and let each recovery point describe only what it really changed". That bounds disk
growth to deduplicated net deltas, bounds memory to fixed buffers plus a capped index, and bounds
restore risk with a strict preflight. It is more precise than full workspace copies and more
crash-tolerant than a pure diff chain, especially for binaries and long-lived projects.
