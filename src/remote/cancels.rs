//! 取消信号：手机点「停止」→ 服务端定向转发 → 这里的 oneshot 被唤醒。
//!
//! 「取消」在宿主侧就是 **drop 掉 `run_turn` 的 future**，再调 `recovery.abort_turn()`
//! 把恢复用的轮次收尾——和 GUI 的停止按钮是同一条路径。工具执行到一半被丢掉时，
//! 已经写出的字节不会回滚：这是可接受的不精确，但必须让人知道"停止"不是事务回滚。

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
