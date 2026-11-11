//! Long-lived shell jobs: process trees that outlive the tool call that started them.
//! # Why this is separate from one-shot `shell` / `bash`.
//! Every one-shot call runs inside a Job Object with `KILL_ON_JOB_CLOSE`, and that fence is dropped as soon...

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::process::Child;

/// One managed process tree.
pub struct Job {
    id: String,
    name: String,
    workspace: PathBuf,
    command: String,
    log: PathBuf,
    pid: Option<u32>,
    started: Instant,
    child: Child,
    fence: crate::sandbox::Fence,
}

/// A borrowed snapshot for tool output. Deliberately no `Child` / `Fence`: the model gets facts,
/// not handles it could accidentally keep.
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: String,
    pub name: String,
    pub workspace: PathBuf,
    pub command: String,
    pub log: PathBuf,
    pub pid: Option<u32>,
    pub age_secs: u64,
    /// `None` when the platform cannot report a count (or the fence is off).
    pub alive: Option<u32>,
    /// The helper's exit code, when it has exited (the server itself can still be alive).
    pub exit: Option<i32>,
    /// `true` while the helper is alive **or** any descendant is still in the Job.
    pub running: bool,
}

impl JobInfo {
    pub fn state_label(&self) -> String {
        if self.running {
            let n = self
                .alive
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".to_string());
            crate::tf!("running · {} process(es)", n)
        } else {
            match self.exit {
                Some(code) => crate::tf!("exited ({})", code),
                None => crate::i18n::t("exited").to_string(),
            }
        }
    }
}

/// The registry carried by [`crate::runtime::Runtime`]
#[derive(Default)]
pub struct Jobs {
    items: HashMap<String, Job>,
    seq: u64,
}

impl Jobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop jobs whose total lifetime has passed. `max_hours == 0` disables expiry.
    pub fn expire(&mut self, max_hours: u64) -> Vec<String> {
        if max_hours == 0 {
            return Vec::new();
        }
        let limit = Duration::from_secs(max_hours.saturating_mul(3600));
        let expired: Vec<String> = self
            .items
            .values()
            .filter(|j| j.started.elapsed() >= limit)
            .map(|j| j.id.clone())
            .collect();
        for id in &expired {
            if let Some(job) = self.items.remove(id) {
                job.fence.terminate();
            }
        }
        expired
    }

    /// Remove records whose process tree is already gone. Their logs stay on disk; only the
    /// bookkeeping slot is freed.
    fn drop_finished(&mut self) {
        let finished: Vec<String> = self
            .items
            .iter_mut()
            .filter_map(|(id, j)| (!j.snapshot().running).then(|| id.clone()))
            .collect();
        for id in finished {
            self.items.remove(&id);
        }
    }

    /// Reserve a slot and insert an already-started background process.
    pub fn insert(
        &mut self,
        name: &str,
        workspace: PathBuf,
        command: String,
        log: PathBuf,
        bg: crate::shell::runner::Background,
    ) -> JobInfo {
        self.seq = self.seq.wrapping_add(1);
        let id = format!("{}-{}", name, self.seq);
        let mut job = Job {
            id: id.clone(),
            name: name.to_string(),
            workspace,
            command,
            log,
            pid: bg.child.id(),
            started: bg.started,
            child: bg.child,
            fence: bg.fence,
        };
        let info = job.snapshot();
        self.items.insert(id, job);
        info
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Is there room for another job? `max == 0` means the feature is disabled.
    pub fn has_room(&self, max: usize) -> bool {
        max > 0 && self.items.len() < max
    }

    pub fn ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.items.keys().cloned().collect();
        v.sort();
        v
    }

    /// Free finished records before checking capacity, so a server that exited does not occupy a
    /// slot forever.
    pub fn make_room(&mut self) {
        self.drop_finished();
    }

    /// Snapshot one job, or all of them when `id` is `None`.
    pub fn status(&mut self, id: Option<&str>) -> Result<Vec<JobInfo>, String> {
        match id {
            Some(id) => self
                .items
                .get_mut(id)
                .map(|j| vec![j.snapshot()])
                .ok_or_else(|| self.not_found(id)),
            None => {
                let mut v: Vec<JobInfo> = self.items.values_mut().map(|j| j.snapshot()).collect();
                v.sort_by(|a, b| a.id.cmp(&b.id));
                Ok(v)
            }
        }
    }

    /// Where a job's log lives, without keeping the registry locked while it is read.
    pub fn log_path(&mut self, id: &str) -> Result<(JobInfo, PathBuf), String> {
        if !self.items.contains_key(id) {
            return Err(self.not_found(id));
        }
        let job = self.items.get_mut(id).expect("checked immediately above");
        let info = job.snapshot();
        Ok((info, job.log.clone()))
    }

    /// Stop one job now. Returns its last snapshot.
    pub fn stop(&mut self, id: &str) -> Result<JobInfo, String> {
        let Some(mut job) = self.items.remove(id) else {
            return Err(self.not_found(id));
        };
        let info = job.snapshot();
        job.fence.terminate();
        Ok(info)
    }

    pub fn stop_all(&mut self) -> usize {
        let n = self.items.len();
        for (_, job) in self.items.drain() {
            job.fence.terminate();
        }
        n
    }

    fn not_found(&self, id: &str) -> String {
        let known = self.ids();
        if known.is_empty() {
            crate::tf!("no background job \"{}\" exists (there are no jobs)", id)
        } else {
            crate::tf!(
                "no background job \"{}\" exists. Known: {}",
                id,
                known.join(", ")
            )
        }
    }
}

impl Job {
    fn snapshot(&mut self) -> JobInfo {
        let exit = match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code()),
            _ => None,
        };
        let alive = self.fence.active_processes();
        let helper_alive = exit.is_none();
        let running = helper_alive || alive.unwrap_or(0) > 0;
        JobInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            workspace: self.workspace.clone(),
            command: self.command.clone(),
            log: self.log.clone(),
            pid: self.pid,
            age_secs: self.started.elapsed().as_secs(),
            alive,
            exit: exit.flatten(),
            running,
        }
    }
}

/// Directory for job logs, created on demand.
pub fn run_dir(workspace: &Path) -> PathBuf {
    workspace.join(crate::workplace::AOX_DIR).join("run")
}

/// Human name for a job id: keep it short and safe, never let it become a path.
pub fn clean_name(raw: Option<&str>) -> String {
    let src = raw.unwrap_or("job").trim();
    let mut out = String::new();
    for ch in src.chars() {
        if out.chars().count() >= 24 {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }
    if out.is_empty() { "job".into() } else { out }
}

/// Read the tail of a log without loading an arbitrarily large file.
pub fn read_log_tail(path: &Path, max_bytes: usize) -> Result<(String, bool, u64), String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)
        .map_err(|e| crate::tf!("could not open the job log {}: {}", path.display(), e))?;
    let len = f
        .metadata()
        .map_err(|e| crate::tf!("could not stat the job log {}: {}", path.display(), e))?
        .len();
    let max = max_bytes.clamp(256, 200_000) as u64;
    let cut = len.saturating_sub(max);
    if cut > 0 {
        f.seek(SeekFrom::Start(cut))
            .map_err(|e| crate::tf!("could not seek the job log {}: {}", path.display(), e))?;
    }
    let mut buf = Vec::with_capacity((len - cut) as usize);
    f.read_to_end(&mut buf)
        .map_err(|e| crate::tf!("could not read the job log {}: {}", path.display(), e))?;
    // Same decoder as a one-shot call: UTF-8 first, then the machine's ANSI code page.
    Ok((crate::shell::decode(&buf), cut > 0, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_turn_into_paths() {
        assert_eq!(clean_name(Some("vite dev")), "vite-dev");
        assert_eq!(clean_name(Some("../../evil")), "evil");
        assert_eq!(clean_name(Some("   ")), "job");
        assert!(clean_name(Some(&"x".repeat(100))).chars().count() <= 24);
    }

    #[test]
    fn reading_a_tail_never_loads_the_whole_file() {
        let d = std::env::temp_dir().join("aox-job-log-tail");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("run.log");
        std::fs::write(&p, "a".repeat(5000) + "tail").unwrap();
        let (text, cut, len) = read_log_tail(&p, 256).unwrap();
        assert!(cut, "the caller must be told the log was truncated");
        assert_eq!(len, 5004);
        assert!(text.ends_with("tail"));
        assert!(text.len() <= 256);
        let _ = std::fs::remove_dir_all(&d);
    }
}
