//! On-disk types for the recovery store. Kept free of I/O so schema decisions are visible in one
//! place.

use serde::{Deserialize, Serialize};

pub const SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    Raw,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub hash: String,
    pub size: u64,
    pub codec: Codec,
    #[serde(default)]
    pub readonly: bool,
    /// `false` = only hash/size were observed; no object exists. Undo is impossible, drift checks
    /// remain possible.
    #[serde(default = "default_true")]
    pub stored: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOpKind {
    Create,
    Modify,
    Delete,
    TypeChange,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileOp {
    pub path: String,
    pub kind: FileOpKind,
    pub before: Option<BlobRef>,
    pub after: Option<BlobRef>,
}

impl FileOp {
    /// A path-level operation has no effect when both sides are identical (or both absent).
    pub fn is_noop(&self) -> bool {
        match (&self.before, &self.after) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.hash == b.hash && a.size == b.size && a.codec == b.codec && a.readonly == b.readonly
            }
            _ => false,
        }
    }

    pub fn finish_kind(&mut self) {
        self.kind = match (&self.before, &self.after) {
            (None, Some(_)) => FileOpKind::Create,
            (Some(_), None) => FileOpKind::Delete,
            (Some(_), Some(_)) => FileOpKind::Modify,
            (None, None) => FileOpKind::Modify,
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointKind {
    Turn,
    External,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointStatus {
    Completed,
    Failed,
    Interrupted,
    Recovered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageState {
    Complete,
    Partial,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Coverage {
    pub state: CoverageState,
    pub scanned: u64,
    pub excluded_roots: Vec<String>,
    pub reasons: Vec<String>,
}

impl Coverage {
    pub fn complete() -> Self {
        Self {
            state: CoverageState::Complete,
            scanned: 0,
            excluded_roots: Vec::new(),
            reasons: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryPoint {
    pub schema: u32,
    pub id: String,
    pub parent: Option<String>,
    pub kind: PointKind,
    pub status: PointStatus,
    pub created_at: u64,
    pub finished_at: u64,
    pub conversation: Option<String>,
    pub turn: Option<u64>,
    pub label: String,
    pub ops: Vec<FileOp>,
    pub coverage: Coverage,
    #[serde(default)]
    pub redoable: bool,
}


#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cursor {
    pub schema: u32,
    pub workspace_id: String,
    pub head: Option<String>,
    #[serde(default)]
    pub redo: Vec<String>,
    #[serde(default)]
    pub truncated: bool,
    pub updated_at: u64,
}

impl Cursor {
    pub fn genesis(workspace_id: String) -> Self {
        Self {
            schema: SCHEMA,
            workspace_id,
            head: None,
            redo: Vec::new(),
            truncated: false,
            updated_at: crate::history::unix_now(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SafetyPath {
    pub path: String,
    pub state: Option<BlobRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SafetySnapshot {
    pub schema: u32,
    pub id: String,
    pub from_cursor: Option<String>,
    pub target_cursor: Option<String>,
    pub created_at: u64,
    /// The state immediately before the restore (what redo writes back).
    pub states: Vec<SafetyPath>,
    /// What the restore is expected to produce; redo preflights against this.
    pub expected: Vec<SafetyPath>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnStatus {
    Completed,
    Failed,
    Interrupted,
}

impl TurnStatus {
    pub fn as_status(self) -> PointStatus {
        match self {
            Self::Completed => PointStatus::Completed,
            Self::Failed => PointStatus::Failed,
            Self::Interrupted => PointStatus::Interrupted,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RestorePath {
    pub path: String,
    /// What the file should be for the restore to be safe.
    pub current: Option<BlobRef>,
    /// What to write (None = remove the path)
    pub restore: Option<BlobRef>,
}

#[derive(Clone, Debug, Default)]
pub struct RestorePlan {
    pub from: Option<String>,
    pub target: Option<String>,
    pub safety_id: Option<String>,
    pub actions: Vec<RestorePath>,
    pub conflicts: Vec<String>,
    pub warnings: Vec<String>,
    pub truncated: bool,
    pub partial: bool,
}

#[derive(Clone, Debug, Default)]
pub struct RestoreReport {
    #[allow(dead_code)] // Retained for UI/debug surfaces; the agent note currently only needs target/counts.
    pub from: Option<String>,
    pub target: Option<String>,
    pub restored: usize,
    pub removed: usize,
    pub safety_id: Option<String>,
    pub warnings: Vec<String>,
}

