//! Pending approval requests, keyed by id.
//!
//! The read loop resolves them; `RemoteUi::authorize` waits on them. A `Mutex<HashMap>` is enough
//! because a turn only ever waits on one approval at a time and the socket task never blocks on it.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug)]
pub struct Decision {
    pub allow: bool,
    /// "本任务内都允许" — scoped to this turn only, never written into `Runtime::session_allowed`.
    pub for_task: bool,
}

#[derive(Default)]
pub struct Approvals {
    pending: Mutex<HashMap<String, oneshot::Sender<Decision>>>,
}

impl Approvals {
    pub fn register(&self, id: impl Into<String>) -> oneshot::Receiver<Decision> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("approvals lock").insert(id.into(), tx);
        rx
    }

    /// Returns false when the id is unknown (already decided, timed out, or another tab won).
    pub fn resolve(&self, id: &str, allow: bool, for_task: bool) -> bool {
        let tx = self.pending.lock().expect("approvals lock").remove(id);
        match tx {
            Some(tx) => tx.send(Decision { allow, for_task }).is_ok(),
            None => false,
        }
    }

    pub fn forget(&self, id: &str) {
        self.pending.lock().expect("approvals lock").remove(id);
    }
}
