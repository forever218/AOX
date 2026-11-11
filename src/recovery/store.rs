//! Recovery store: content-addressed blobs plus immutable task nodes and an atomic cursor.
//! No database and no refcounts. `objects/` and `nodes/` are append-only; `cursor.json` is the one mutable pointer...

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::{
    BlobRef, Codec, Cursor, RecoveryPoint, SafetySnapshot, SCHEMA,
};
use super::{RecoveryError, RecoveryResult};
use crate::config::RecoveryConfig;

const BACKUP_DIR: &str = "backup";
const FORMAT: &str = "format.json";
const CURSOR: &str = "cursor.json";
const BASELINE: &str = "baseline/manifest.json";
const LOCK: &str = "lock";
const BUFFER: usize = 128 * 1024;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Format {
    schema: u32,
    workspace_id: String,
    created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BaselineManifest {
    schema: u32,
    workspace_id: String,
    scanned_at: u64,
    files: BTreeMap<String, BlobRef>,
}

pub struct RecoveryStore {
    root: PathBuf,
    dir: PathBuf,
    workspace_id: String,
    cfg: RecoveryConfig,
    used: Mutex<u64>,
    /// The running executable, workspace-relative and case-folded, when it lives inside `root`.
    self_rel: Option<String>,
}

pub struct FileLock {
    file: File,
    path: PathBuf,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
        // The file is intentionally left behind: removing a lock file races another process that
        // already opened it. The OS lock, not the path, is the exclusion primitive.
        let _ = &self.path;
    }
}

impl RecoveryStore {
    pub fn open(root: &Path, cfg: &RecoveryConfig, create: bool) -> RecoveryResult<Self> {
        let dir = root.join(".aox").join(BACKUP_DIR);
        if !dir.exists() {
            if !create {
                return Err(RecoveryError::NoStore(dir));
            }
            std::fs::create_dir_all(&dir)
                .map_err(|e| RecoveryError::BackupUnavailable(format!("{}: {e}", dir.display())))?;
        }
        reject_reparse(&root.join(".aox"))?;
        reject_reparse(&dir)?;

        let format_path = dir.join(FORMAT);
        let format = if format_path.exists() {
            read_json::<Format>(&format_path)?
        } else {
            let f = Format {
                schema: SCHEMA,
                workspace_id: new_workspace_id(root),
                created_at: crate::history::unix_now(),
            };
            write_json_atomic(&format_path, &f)?;
            f
        };
        if format.schema != SCHEMA {
            return Err(RecoveryError::Corrupt {
                path: format_path,
                reason: format!("unsupported recovery schema {} (expected {})", format.schema, SCHEMA),
            });
        }
        for sub in ["objects", "nodes", "journals", "redo", "tmp", "baseline"] {
            let p = dir.join(sub);
            if create {
                std::fs::create_dir_all(&p).map_err(|e| {
                    RecoveryError::BackupUnavailable(format!("{}: {e}", p.display()))
                })?;
            }
        }
        let used = dir_size(&dir).unwrap_or(0);
        Ok(Self {
            root: root.to_path_buf(),
            dir,
            workspace_id: format.workspace_id,
            cfg: cfg.clone(),
            used: Mutex::new(used),
            self_rel: self_executable_rel(root),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Test hook: pretend the process runs from `rel`, so the "never back up yourself" rule can be
    /// exercised without moving the test binary into the temporary workspace.
    #[cfg(test)]
    fn with_self_exe(mut self, rel: &str) -> Self {
        self.self_rel = Some(rel.to_ascii_lowercase());
        self
    }

    pub fn lock(&self) -> RecoveryResult<FileLock> {
        let path = self.dir.join(LOCK);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| RecoveryError::BackupUnavailable(format!("{}: {e}", path.display())))?;
        file.try_lock().map_err(|e| {
            RecoveryError::Locked(format!(
                "another AOX is writing this workspace ({}): {e}",
                path.display()
            ))
        })?;
        Ok(FileLock { file, path })
    }

    /// Convert an absolute path to a workspace-relative, `/`-separated path. `Ok(None)` means the
    /// path is intentionally excluded.
    pub fn relative(&self, abs: &Path) -> RecoveryResult<Option<String>> {
        let rel = abs.strip_prefix(&self.root).map_err(|_| {
            RecoveryError::OutsideWorkspace(format!(
                "{} is outside {}",
                abs.display(),
                self.root.display()
            ))
        })?;
        let mut parts = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(s) => parts.push(s.to_string_lossy().replace('\\', "/")),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(RecoveryError::OutsideWorkspace(format!(
                        "{} is not a clean workspace-relative path",
                        rel.display()
                    )))
                }
            }
        }
        if parts.is_empty() {
            return Err(RecoveryError::OutsideWorkspace("the workspace root itself".into()));
        }
        // Reject a path reached through a junction/symlink inside the workspace.
        let mut cur = self.root.clone();
        for part in &parts {
            cur.push(part);
            match std::fs::symlink_metadata(&cur) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(RecoveryError::UnsupportedEntry(format!(
                        "{} is reached through a symlink/reparse point; refusing to track it",
                        cur.display()
                    )))
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => break,
            }
        }
        let s = parts.join("/");
        if self.is_excluded(&s) {
            Ok(None)
        } else {
            Ok(Some(s))
        }
    }

    pub fn absolute(&self, rel: &str) -> RecoveryResult<PathBuf> {
        if rel.is_empty() || rel.starts_with('/') || rel.starts_with('\\') {
            return Err(RecoveryError::Corrupt {
                path: self.dir.clone(),
                reason: format!("bad relative path in node: {rel:?}"),
            });
        }
        let mut p = self.root.clone();
        for part in rel.split('/') {
            if part.is_empty() || part == "." || part == ".." || part.contains(':') {
                return Err(RecoveryError::Corrupt {
                    path: self.dir.clone(),
                    reason: format!("bad relative path in node: {rel:?}"),
                });
            }
            p.push(part);
        }
        Ok(p)
    }

    pub fn is_excluded(&self, rel: &str) -> bool {
        let r = rel.replace('\\', "/");
        let lower = r.to_ascii_lowercase();
        // Not configurable on purpose: the file this process is running from is never rollback material, whatever...
        if self.self_rel.as_deref() == Some(lower.as_str()) {
            return true;
        }
        for pat in &self.cfg.exclude {
            let p = pat.replace('\\', "/");
            let p_low = p.to_ascii_lowercase();
            if p_low.ends_with('/') {
                let prefix = p_low.trim_end_matches('/');
                if lower == prefix
                    || lower.starts_with(&(prefix.to_string() + "/"))
                    || lower.ends_with(&("/".to_string() + prefix))
                    || lower.contains(&("/".to_string() + prefix + "/"))
                {
                    return true;
                }
            } else if let Some(suffix) = p_low.strip_prefix('*') {
                if lower.ends_with(suffix) {
                    return true;
                }
            } else if lower == p_low || lower.split('/').any(|part| part == p_low) {
                return true;
            }
        }
        false
    }

    pub fn put_file(&self, path: &Path, max_file: u64) -> RecoveryResult<BlobRef> {
        let meta = std::fs::metadata(path)
            .map_err(|e| RecoveryError::BackupUnavailable(format!("{}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(RecoveryError::UnsupportedEntry(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        if meta.len() > max_file {
            return Err(RecoveryError::LimitExceeded {
                path: path.to_path_buf(),
                size: meta.len(),
                cap: max_file,
            });
        }
        let mut f = File::open(path)
            .map_err(|e| RecoveryError::BackupUnavailable(format!("{}: {e}", path.display())))?;
        self.put_reader(&mut f, Some(meta.len()), max_file, meta.permissions().readonly())
    }

    /// Store a stream. When `cap` is exceeded the temp file is removed and `LimitExceeded` returned.
    pub fn put_reader(
        &self,
        r: &mut impl Read,
        known_size: Option<u64>,
        cap: u64,
        readonly: bool,
    ) -> RecoveryResult<BlobRef> {
        let tmp = self.temp_path("blob");
        let result = (|| -> RecoveryResult<BlobRef> {
            if let Some(parent) = tmp.parent() {
                std::fs::create_dir_all(parent).map_err(io_unavailable)?;
            }
            let file = File::create(&tmp)
                .map_err(|e| RecoveryError::BackupUnavailable(format!("{}: {e}", tmp.display())))?;
            let mut out = BufWriter::new(file);
            let mut hasher = blake3::Hasher::new();
            let mut buf = vec![0u8; BUFFER];
            let mut size = 0u64;
            loop {
                let n = r.read(&mut buf).map_err(|e| {
                    RecoveryError::BackupUnavailable(format!("read failed: {e}"))
                })?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                if size > cap {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(RecoveryError::LimitExceeded {
                        path: PathBuf::from("<stream>"),
                        size,
                        cap,
                    });
                }
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n]).map_err(io_unavailable)?;
            }
            out.flush().map_err(io_unavailable)?;
            out.get_ref().sync_all().map_err(io_unavailable)?;
            drop(out);
            if let Some(want) = known_size {
                if want != size {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(RecoveryError::BackupUnavailable(format!(
                        "size changed while reading: expected {want}, read {size}"
                    )));
                }
            }
            let hash = hasher.finalize().to_hex().to_string();
            let blob = BlobRef {
                hash: hash.clone(),
                size,
                codec: Codec::Raw,
                readonly,
                stored: true,
            };
            let dest = self.object_path(&hash);
            if dest.exists() {
                let _ = std::fs::remove_file(&tmp);
                let existing = std::fs::metadata(&dest).map_err(io_unavailable)?;
                if existing.len() != size {
                    return Err(RecoveryError::Corrupt {
                        path: dest,
                        reason: "existing object has a different size".into(),
                    });
                }
            } else {
                let used = self.total_bytes();
                let total_cap = self.cfg.max_total_mb.saturating_mul(1024 * 1024);
                if total_cap > 0 && used.saturating_add(size) > total_cap {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(RecoveryError::BackupUnavailable(format!(
                        "recovery storage is full ({} used + {} new > {} cap)",
                        used, size, total_cap
                    )));
                }
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent).map_err(io_unavailable)?;
                }
                std::fs::rename(&tmp, &dest).map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    RecoveryError::BackupUnavailable(format!(
                        "publish object {}: {e}",
                        dest.display()
                    ))
                })?;
                let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
                *used = used.saturating_add(size);
            }
            Ok(blob)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Hash a file without writing an object. Used by previews and drift checks.
    pub fn hash_file_only(&self, path: &Path) -> RecoveryResult<BlobRef> {
        let meta = std::fs::metadata(path)
            .map_err(|e| RecoveryError::BackupUnavailable(format!("{}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(RecoveryError::UnsupportedEntry(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        let mut f = File::open(path)
            .map_err(|e| RecoveryError::BackupUnavailable(format!("{}: {e}", path.display())))?;
        let mut buf = vec![0u8; BUFFER];
        let mut hasher = blake3::Hasher::new();
        let mut size = 0u64;
        loop {
            let n = f.read(&mut buf).map_err(io_unavailable)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            hasher.update(&buf[..n]);
        }
        Ok(BlobRef {
            hash: hasher.finalize().to_hex().to_string(),
            size,
            codec: Codec::Raw,
            readonly: meta.permissions().readonly(),
            stored: false,
        })
    }

    /// Compute a hash without necessarily storing the bytes. Used for a post-image that exceeded
    /// the storage cap: drift detection still needs its hash, redo does not.
    pub fn observe_file(&self, path: &Path, max_store: u64) -> RecoveryResult<BlobRef> {
        let meta = std::fs::metadata(path)
            .map_err(|e| RecoveryError::BackupUnavailable(format!("{}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(RecoveryError::UnsupportedEntry(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        if meta.len() <= max_store {
            return self.put_file(path, max_store);
        }
        let mut f = File::open(path)
            .map_err(|e| RecoveryError::BackupUnavailable(format!("{}: {e}", path.display())))?;
        let mut buf = vec![0u8; BUFFER];
        let mut hasher = blake3::Hasher::new();
        let mut size = 0u64;
        loop {
            let n = f.read(&mut buf).map_err(io_unavailable)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            hasher.update(&buf[..n]);
        }
        Ok(BlobRef {
            hash: hasher.finalize().to_hex().to_string(),
            size,
            codec: Codec::Raw,
            readonly: meta.permissions().readonly(),
            stored: false,
        })
    }

    pub fn materialize(&self, blob: &BlobRef, target: &Path) -> RecoveryResult<()> {
        if !blob.stored {
            return Err(RecoveryError::Corrupt {
                path: target.to_path_buf(),
                reason: format!("object {} was not stored", blob.hash),
            });
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(io_unavailable)?;
        }
        let tmp = target.with_file_name(format!(
            ".{}.aox-recovery-{}.tmp",
            target.file_name().unwrap_or_default().to_string_lossy(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> RecoveryResult<()> {
            let obj = self.object_path(&blob.hash);
            let mut src = File::open(&obj).map_err(|e| {
                RecoveryError::Corrupt {
                    path: obj.clone(),
                    reason: format!("missing object: {e}"),
                }
            })?;
            let mut out = BufWriter::new(File::create(&tmp).map_err(io_unavailable)?);
            let mut hasher = blake3::Hasher::new();
            let mut buf = vec![0u8; BUFFER];
            let mut size = 0u64;
            loop {
                let n = src.read(&mut buf).map_err(io_unavailable)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n]).map_err(io_unavailable)?;
            }
            out.flush().map_err(io_unavailable)?;
            out.get_ref().sync_all().map_err(io_unavailable)?;
            drop(out);
            if size != blob.size || hasher.finalize().to_hex().to_string() != blob.hash {
                return Err(RecoveryError::Corrupt {
                    path: obj,
                    reason: "object hash/size mismatch".into(),
                });
            }
            if target.exists() {
                let perms = writable_permissions(target).map_err(io_unavailable)?;
                let _ = std::fs::set_permissions(target, perms);
            }
            let mut perms = std::fs::metadata(&tmp).map_err(io_unavailable)?.permissions();
            perms.set_readonly(blob.readonly);
            std::fs::set_permissions(&tmp, perms).map_err(io_unavailable)?;
            std::fs::rename(&tmp, target).map_err(|e| {
                RecoveryError::BackupUnavailable(format!("replace {}: {e}", target.display()))
            })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    pub fn node_path(&self, id: &str) -> PathBuf {
        self.dir.join("nodes").join(format!("{id}.json"))
    }

    pub fn save_node(&self, node: &RecoveryPoint) -> RecoveryResult<()> {
        write_json_atomic(&self.node_path(&node.id), node)
    }

    pub fn load_node(&self, id: &str) -> RecoveryResult<RecoveryPoint> {
        let path = self.node_path(id);
        let node: RecoveryPoint = read_json(&path)?;
        if node.schema != SCHEMA {
            return Err(RecoveryError::Corrupt {
                path,
                reason: format!("unsupported node schema {}", node.schema),
            });
        }
        Ok(node)
    }

    pub fn list_nodes(&self) -> RecoveryResult<Vec<RecoveryPoint>> {
        let dir = self.dir.join("nodes");
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&dir).map_err(io_unavailable)? {
            let entry = entry.map_err(io_unavailable)?;
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            out.push(read_json::<RecoveryPoint>(&p)?);
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    pub fn cursor(&self) -> RecoveryResult<Cursor> {
        let path = self.dir.join(CURSOR);
        if !path.exists() {
            return Ok(Cursor::genesis(self.workspace_id.clone()));
        }
        let c: Cursor = read_json(&path)?;
        if c.schema != SCHEMA || c.workspace_id != self.workspace_id {
            return Err(RecoveryError::Corrupt {
                path,
                reason: "cursor schema or workspace id mismatch".into(),
            });
        }
        Ok(c)
    }

    pub fn save_cursor(&self, cursor: &Cursor) -> RecoveryResult<()> {
        write_json_atomic(&self.dir.join(CURSOR), cursor)
    }

    // ─────────── the strict-shell baseline ───────────.

    /// Load the baseline, or an empty map when there is none.
    pub fn load_baseline(&self) -> BTreeMap<String, BlobRef> {
        let path = self.dir.join(BASELINE);
        let Ok(text) = std::fs::read_to_string(&path) else { return BTreeMap::new() };
        let Ok(m) = serde_json::from_str::<BaselineManifest>(&text) else { return BTreeMap::new() };
        if m.schema != SCHEMA || m.workspace_id != self.workspace_id {
            return BTreeMap::new();
        }
        m.files
    }

    pub fn save_baseline(&self, files: &BTreeMap<String, BlobRef>) -> RecoveryResult<()> {
        let path = self.dir.join(BASELINE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_unavailable)?;
        }
        write_json_atomic(
            &path,
            &BaselineManifest {
                schema: SCHEMA,
                workspace_id: self.workspace_id.clone(),
                scanned_at: crate::history::unix_now(),
                files: files.clone(),
            },
        )
    }

    /// Is the object for `hash` actually on disk? A `stat`, not a read: this is asked once per file
    /// per scan, so it must not become the cost it is trying to avoid.
    pub fn has_object(&self, hash: &str) -> bool {
        hash.len() == 64 && self.object_path(hash).is_file()
    }

    pub fn save_safety(&self, safety: &SafetySnapshot) -> RecoveryResult<()> {
        write_json_atomic(&self.dir.join("redo").join(format!("{}.json", safety.id)), safety)
    }

    pub fn load_safety(&self, id: &str) -> RecoveryResult<SafetySnapshot> {
        read_json(&self.dir.join("redo").join(format!("{id}.json")))
    }

    pub fn remove_safety(&self, id: &str) {
        let _ = std::fs::remove_file(self.dir.join("redo").join(format!("{id}.json")));
    }

    pub fn new_journal(&self) -> RecoveryResult<Journal> {
        self.new_journal_named("tx")
    }

    pub fn new_restore_journal(&self) -> RecoveryResult<Journal> {
        self.new_journal_named("restore")
    }

    fn new_journal_named(&self, prefix: &str) -> RecoveryResult<Journal> {
        let id = new_id(prefix);
        let path = self.dir.join("journals").join(format!("{id}.jsonl"));
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(io_unavailable)?;
        Ok(Journal { path, file })
    }

    /// Recover a BlobRef from a hash recorded in a journal line. The object must already exist.
    pub fn blob_by_hash(&self, hash: &str) -> RecoveryResult<BlobRef> {
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(RecoveryError::Corrupt {
                path: self.dir.clone(),
                reason: format!("bad object hash in journal: {hash:?}"),
            });
        }
        let path = self.object_path(hash);
        let meta = std::fs::metadata(&path).map_err(|e| RecoveryError::Corrupt {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        Ok(BlobRef {
            hash: hash.to_string(),
            size: meta.len(),
            codec: Codec::Raw,
            readonly: false,
            stored: true,
        })
    }

    pub fn verify_blob(&self, blob: &BlobRef) -> RecoveryResult<()> {
        if !blob.stored {
            return Err(RecoveryError::Corrupt {
                path: self.dir.clone(),
                reason: format!("object {} was never stored", blob.hash),
            });
        }
        let path = self.object_path(&blob.hash);
        let mut f = File::open(&path).map_err(|e| RecoveryError::Corrupt {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; BUFFER];
        let mut size = 0u64;
        loop {
            let n = f.read(&mut buf).map_err(io_unavailable)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            hasher.update(&buf[..n]);
        }
        if size != blob.size || hasher.finalize().to_hex().to_string() != blob.hash {
            return Err(RecoveryError::Corrupt {
                path,
                reason: "object hash/size mismatch".into(),
            });
        }
        Ok(())
    }

    #[allow(dead_code)] // W4 crash recovery consumes these.
    pub fn journal_paths(&self) -> RecoveryResult<Vec<PathBuf>> {
        let dir = self.dir.join("journals");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut v = Vec::new();
        for e in std::fs::read_dir(dir).map_err(io_unavailable)? {
            let p = e.map_err(io_unavailable)?.path();
            if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                v.push(p);
            }
        }
        Ok(v)
    }

    pub fn total_bytes(&self) -> u64 {
        *self.used.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Actual on-disk footprint, including nodes/cursor/journals. Used for `/backup status`; quota
    /// decisions use the cached object counter so a hot write path does not walk the tree.
    pub fn disk_usage(&self) -> u64 {
        dir_size(&self.dir).unwrap_or_else(|_| self.total_bytes())
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        self.dir.join("objects").join(&hash[..2]).join(&hash[2..4]).join(hash)
    }

    fn temp_path(&self, kind: &str) -> PathBuf {
        let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        self.dir.join("tmp").join(format!(
            "{kind}-{}-{}-{n}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }
}

pub struct Journal {
    path: PathBuf,
    file: File,
}

impl Journal {
    pub fn append(&mut self, value: &Value, sync: bool) -> RecoveryResult<()> {
        let mut line = serde_json::to_string(value)
            .map_err(|e| RecoveryError::BackupUnavailable(e.to_string()))?;
        line.push('\n');
        self.file.write_all(line.as_bytes()).map_err(io_unavailable)?;
        self.file.flush().map_err(io_unavailable)?;
        if sync {
            self.file.sync_all().map_err(io_unavailable)?;
        }
        Ok(())
    }

    pub fn remove(self) {
        let _ = std::fs::remove_file(&self.path);
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn new_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let h = blake3::hash(format!("{nanos}-{}-{n}", std::process::id()).as_bytes())
        .to_hex()
        .to_string();
    format!("{prefix}_{}{}_{}", crate::history::unix_now(), n, &h[..4])
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> RecoveryResult<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| RecoveryError::BackupUnavailable(e.to_string()))?;
    write_atomic(path, &bytes)
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> RecoveryResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_unavailable)?;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(
        ".{name}.aox-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        if path.exists() {
            let _ = std::fs::set_permissions(path, writable_permissions(path)?);
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(io_unavailable)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> RecoveryResult<T> {
    let bytes = std::fs::read(path).map_err(|e| RecoveryError::Corrupt {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    serde_json::from_slice(&bytes).map_err(|e| RecoveryError::Corrupt {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

fn writable_permissions(path: &Path) -> io::Result<std::fs::Permissions> {
    let mut p = std::fs::metadata(path)?.permissions();
    if p.readonly() {
        p.set_readonly(false);
    }
    Ok(p)
}

fn io_unavailable(e: io::Error) -> RecoveryError {
    RecoveryError::BackupUnavailable(e.to_string())
}

fn reject_reparse(path: &Path) -> RecoveryResult<()> {
    if !path.exists() {
        return Ok(());
    }
    let meta = std::fs::symlink_metadata(path).map_err(io_unavailable)?;
    if meta.file_type().is_symlink() {
        return Err(RecoveryError::UnsupportedEntry(format!(
            "{} is a symlink/reparse point; refusing to use it as the backup directory",
            path.display()
        )));
    }
    Ok(())
}

fn new_workspace_id(root: &Path) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    blake3::hash(format!("{}|{}|{}", root.display(), std::process::id(), now).as_bytes())
        .to_hex()
        .to_string()
}

/// The running executable's path relative to `root`, `/`-separated and case-folded, or `None` when it is outside...
fn self_executable_rel(root: &Path) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let rel = match exe.strip_prefix(root) {
        Ok(p) => p.to_path_buf(),
        Err(_) => {
            let canon_root = std::fs::canonicalize(root).ok()?;
            let canon_exe = std::fs::canonicalize(&exe).ok()?;
            canon_exe.strip_prefix(&canon_root).ok()?.to_path_buf()
        }
    };
    if rel.as_os_str().is_empty() {
        return None;
    }
    Some(rel.to_string_lossy().replace('\\', "/").to_ascii_lowercase())
}

fn dir_size(path: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    if !path.exists() {
        return Ok(0);
    }
    for e in std::fs::read_dir(path)? {
        let e = e?;
        let p = e.path();
        let m = e.metadata()?;
        if m.is_dir() {
            total = total.saturating_add(dir_size(&p)?);
        } else {
            total = total.saturating_add(m.len());
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aox-recovery-store-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cnt() -> RecoveryConfig {
        let mut c = RecoveryConfig::default();
        c.max_file_mb = 4;
        c
    }

    #[test]
    fn blobs_are_deduplicated_and_verified() {
        let d = tmp("blobs");
        let s = RecoveryStore::open(&d, &cnt(), true).unwrap();
        let a = d.join("a.txt");
        std::fs::write(&a, "same").unwrap();
        let b = d.join("b.txt");
        std::fs::write(&b, "same").unwrap();
        let x = s.put_file(&a, 4096).unwrap();
        let y = s.put_file(&b, 4096).unwrap();
        assert_eq!(x.hash, y.hash);
        assert!(s.object_path(&x.hash).exists());
        s.materialize(&x, &d.join("restored.txt")).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("restored.txt")).unwrap(), "same");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn file_cap_is_enforced_before_publishing() {
        let d = tmp("cap");
        let s = RecoveryStore::open(&d, &cnt(), true).unwrap();
        let p = d.join("big");
        std::fs::write(&p, "12345").unwrap();
        let before = s.total_bytes();
        assert!(matches!(s.put_file(&p, 4), Err(RecoveryError::LimitExceeded { .. })));
        assert_eq!(s.total_bytes(), before);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cursor_round_trip_and_unknown_schema_refused() {
        let d = tmp("cursor");
        let s = RecoveryStore::open(&d, &cnt(), true).unwrap();
        let mut c = s.cursor().unwrap();
        assert!(c.head.is_none());
        c.head = Some("rp_x".into());
        s.save_cursor(&c).unwrap();
        assert_eq!(s.cursor().unwrap().head.as_deref(), Some("rp_x"));
        let mut v: Value = serde_json::from_slice(&std::fs::read(&s.dir.join(CURSOR)).unwrap()).unwrap();
        v["schema"] = serde_json::json!(99);
        std::fs::write(s.dir.join(CURSOR), serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(matches!(s.cursor(), Err(RecoveryError::Corrupt { .. })));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn second_lock_is_refused_while_first_is_alive() {
        let d = tmp("lock");
        let s = RecoveryStore::open(&d, &cnt(), true).unwrap();
        let _held = s.lock().unwrap();
        assert!(matches!(s.lock(), Err(RecoveryError::Locked(_))));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn excluded_paths_never_become_relative() {
        let d = tmp("exclude");
        let s = RecoveryStore::open(&d, &cnt(), true).unwrap();
        assert_eq!(s.relative(&d.join("target").join("x")).unwrap(), None);
        assert_eq!(s.relative(&d.join(".aox").join("backup").join("x")).unwrap(), None);
        assert_eq!(s.relative(&d.join("src").join("a.rs")).unwrap().as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn executable_images_are_never_stored() {
        let d = tmp("binaries");
        let s = RecoveryStore::open(&d, &cnt(), true).unwrap();
        for rel in ["aox-gui.exe", "bin/tool.dll", "debug/app.pdb", "setup.msi", "app.apk"] {
            assert_eq!(s.relative(&d.join(rel)).unwrap(), None, "{rel} must be excluded");
        }
        // A name that merely contains "exe" is ordinary source.
        assert!(s.relative(&d.join("notes.exe.txt")).unwrap().is_some());
    }

    #[test]
    fn the_running_executable_is_excluded_even_without_a_matching_pattern() {
        let d = tmp("self-exe");
        let s = RecoveryStore::open(&d, &cnt(), true).unwrap().with_self_exe("sub/aox-helper.dat");
        assert_eq!(s.relative(&d.join("sub").join("aox-helper.dat")).unwrap(), None);
        assert!(
            s.relative(&d.join("sub").join("other.dat")).unwrap().is_some(),
            "only the running image is refused, not its neighbours"
        );
    }
}
