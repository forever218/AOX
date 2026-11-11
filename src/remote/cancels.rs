//! Cancellation signal: mobile Stop -> server forwards -> this oneshot fires.
//! On the host, cancel drops the run_turn future and calls recovery.abort_turn(), same as the GUI stop button.
//! Already-written bytes stay: stopping is not a transaction rollback, and that imprecision must be clear.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::oneshot;

#[derive(Default)]
pub struct Cancels {
    pending: Mutex<HashMap<String, oneshot::Sender<()>>>,
}

impl Cancels {
    pub fn register(&self, task_id: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("cancels lock")
            .insert(task_id.to_string(), tx);
        rx
    }

    /// Returns false when the id is unknown (already finished, or a stale click).
    pub fn cancel(&self, task_id: &str) -> bool {
        let tx = self.pending.lock().expect("cancels lock").remove(task_id);
        match tx {
            Some(tx) => tx.send(()).is_ok(),
            None => false,
        }
    }

    pub fn finish(&self, task_id: &str) {
        self.pending.lock().expect("cancels lock").remove(task_id);
    }
}
