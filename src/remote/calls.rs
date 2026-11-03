//! 跨用户调用授权：别人 `@` 我的机器时，等主人点同意。
//!
//! 与 `approvals` 的分工：那个问的是"这条命令能不能跑"（任务进行中，宿主已经开干了），这个问的
//! 是"这个人能不能用我的机器"（任务开始前，还没干活）。
//!
//! 两条硬约束：
//! 1. **等待不能在 WS 读循环里 await**——两分钟不读 socket，心跳、取消、审批全部停摆，连接会被
//!    服务端判死。所以等结果的动作扔进 `tokio::spawn`，批准后再把 `Job` 送进队列。
//! 2. **"1 小时内允许"只存内存**。进程重启即失效，这是有意的安全默认：一次授权不该变成永久后门。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

/// 超过这个时间没人理，按拒绝处理。比工具审批的 300s 短：这是人际等待，两分钟不在就是不在。
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// 「本小时内允许」的时长。
pub const REMEMBER: Duration = Duration::from_secs(3600);

/// CLI 输入前缀找不到唯一一条时的两种失败。
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
    /// "房间|用户" → 允许到什么时候。
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

    /// 登记一次等待，返回接收端。调用方负责超时与清理。
    pub fn wait(&self, id: &str, room: &str, user: &str) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().expect("call gate").pending.insert(
            id.to_string(),
            Pending { tx, room: room.to_string(), user: user.to_string() },
        );
        rx
    }

    /// 主人的回答到了。`remember` 有值时，同房间同一个人在这个时长内不再问。
    ///
    /// 返回 false 表示这条请求已经不在了（超时、或另一台设备先点了）。
    pub fn resolve(&self, id: &str, allow: bool, remember: Option<Duration>) -> bool {
        let mut g = self.inner.lock().expect("call gate");
        Self::resolve_locked(&mut g, id, allow, remember)
    }

    /// 按 id 前缀解析。CLI 里只显示前 6 位（完整 id 一屏放不下，也没人愿意抄）。
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

    /// 超时或发送失败后清掉，避免这张表无限增长。
    pub fn forget(&self, id: &str) {
        self.inner.lock().expect("call gate").pending.remove(id);
    }

    /// 这个人是不是刚被允许过。过期条目顺手删掉。
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
        // 另一端（另一台设备）再点一次：不能重复唤醒，也不能 panic。
        assert!(!gate.resolve("c1", true, None));
    }

    #[test]
    fn remember_only_covers_that_room_and_that_person() {
        let gate = CallGate::default();
        let _rx = gate.wait("c1", "r1", "bob");
        gate.resolve("c1", true, Some(REMEMBER));

        assert!(gate.is_remembered("r1", "bob"));
        assert!(!gate.is_remembered("r1", "carol"), "不能顺带把别人也放进来");
        assert!(!gate.is_remembered("r2", "bob"), "换个房间要重新问");
    }

    #[test]
    fn forgetting_a_timed_out_request_leaves_no_trace() {
        let gate = CallGate::default();
        let _rx = gate.wait("c1", "r1", "bob");
        gate.forget("c1");
        assert_eq!(gate.pending_count(), 0);
        // 超时之后主人才点"允许"：不该把任何东西唤醒，也不该崩。
        assert!(!gate.resolve("c1", true, None));
    }
}
