//! Cross-user call authorization: when someone @-mentions my machine, wait for the owner to approve.
//! Unlike approvals (command allowed mid-task), this asks if a person may use the machine, before work starts.
//! Never await in the WS loop (tokio::spawn); the one-hour allow is memory-only, so a restart revokes it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

/// No answer in time means denied; shorter than the 300s tool approval because this is a human-to-human wait.
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// How long the allow-for-this-hour grant lasts.
pub const REMEMBER: Duration = Duration::from_secs(3600);

/// The two failures when a CLI input prefix does not match exactly one entry.
#[derive(Debug, PartialEq, Eq)]
pub enum PrefixErr {
    NotFound,
    Ambiguous,
}

#[derive(Default)]
pub struct CallGate {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    pending: HashMap<String, Pending>,
    /// Maps room|user to the instant until which calls are allowed.
    remembered: HashMap<String, Instant>,
}

struct Pending {
    tx: oneshot::Sender<bool>,
    room: String,
    user: String,
}

impl CallGate {
    fn key(room: &str, user: &str) -> String {
        format!("{room}|{user}")
    }

    /// Register one pending wait and return its receiver; the caller owns timeout and cleanup.
    pub fn wait(&self, id: &str, room: &str, user: &str) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().expect("call gate").pending.insert(
            id.to_string(),
            Pending { tx, room: room.to_string(), user: user.to_string() },
        );
        rx
    }

    /// Owner's reply: remember mutes repeats for that room/person; false means the request is already gone.
    pub fn resolve(&self, id: &str, allow: bool, remember: Option<Duration>) -> bool {
        let mut g = self.inner.lock().expect("call gate");
        Self::resolve_locked(&mut g, id, allow, remember)
    }

    /// Resolve by id prefix; the CLI shows only the first 6 chars (a full id will not fit a screen).
    pub fn resolve_prefix(
        &self,
        prefix: &str,
        allow: bool,
        remember: Option<Duration>,
    ) -> Result<String, PrefixErr> {
        let mut g = self.inner.lock().expect("call gate");
        let found: Vec<String> = g
            .pending
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        match found.len() {
            0 => Err(PrefixErr::NotFound),
            1 => {
                let id = found.into_iter().next().expect("len == 1");
                Self::resolve_locked(&mut g, &id, allow, remember);
                Ok(id)
            }
            _ => Err(PrefixErr::Ambiguous),
        }
    }

    fn resolve_locked(
        g: &mut Inner,
        id: &str,
        allow: bool,
        remember: Option<Duration>,
    ) -> bool {
        let Some(p) = g.pending.remove(id) else { return false };
        if allow {
            if let Some(d) = remember {
                g.remembered.insert(Self::key(&p.room, &p.user), Instant::now() + d);
            }
        }
        p.tx.send(allow).is_ok()
    }

    /// Remove entries after timeout or send failure so the table cannot grow without bound.
    pub fn forget(&self, id: &str) {
        self.inner.lock().expect("call gate").pending.remove(id);
    }

    /// Whether this person was recently allowed; expired entries are deleted on the way.
    pub fn is_remembered(&self, room: &str, user: &str) -> bool {
        let mut g = self.inner.lock().expect("call gate");
        let k = Self::key(room, user);
        match g.remembered.get(&k) {
            Some(until) if *until > Instant::now() => true,
            Some(_) => {
                g.remembered.remove(&k);
                false
            }
            None => false,
        }
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.inner.lock().expect("call gate").pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_wakes_the_waiter_with_the_decision() {
        let gate = CallGate::default();
        let mut rx = gate.wait("c1", "r1", "bob");
        assert_eq!(gate.pending_count(), 1);

        assert!(gate.resolve("c1", true, None));
        assert_eq!(rx.try_recv(), Ok(true));
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn a_second_decision_finds_nothing() {
        let gate = CallGate::default();
        let _rx = gate.wait("c1", "r1", "bob");
        assert!(gate.resolve("c1", false, None));
        // A second click from another device must not wake the waiter twice, nor panic.
        assert!(!gate.resolve("c1", true, None));
    }

    #[test]
    fn remember_only_covers_that_room_and_that_person() {
        let gate = CallGate::default();
        let _rx = gate.wait("c1", "r1", "bob");
        gate.resolve("c1", true, Some(REMEMBER));

        assert!(gate.is_remembered("r1", "bob"));
        assert!(!gate.is_remembered("r1", "carol"), "must not let anyone else in along with them");
        assert!(!gate.is_remembered("r2", "bob"), "a different room must ask again");
    }

    #[test]
    fn forgetting_a_timed_out_request_leaves_no_trace() {
        let gate = CallGate::default();
        let _rx = gate.wait("c1", "r1", "bob");
        gate.forget("c1");
        assert_eq!(gate.pending_count(), 0);
        // If the owner clicks Allow after timeout, nothing should wake and nothing should crash.
        assert!(!gate.resolve("c1", true, None));
    }
}
