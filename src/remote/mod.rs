//! `aox remote` — the host side of AOX Remote.
//!
//! Two verbs for the person, one long-running loop underneath:
//!
//! ```text
//! aox remote pair     输服务端地址 + 手机上的配对码，换一张长期设备凭证
//! aox remote serve    连上服务端，收到 @自己 的指令就在本机跑一轮
//! aox remote status   看看现在配的是哪台服务器
//! aox remote unpair   删掉凭证
//! ```
//!
//! **No endpoint is compiled in.** The address is whatever the person typed at `pair` time and is
//! stored in `.aox/remote.json`; this file only knows how to talk to "a server", not to a
//! particular one.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use aoxchat_proto::{
    now_ms, Auth, Body, CallRequest, DeviceInfo, DeviceKind, Frame, Hello, MessageBody, MsgNew,
    MsgSend, Ping, PROTO_VERSION,
};

use crate::agent::Agent;
use crate::attach::Attachment;
use crate::runtime::Runtime;

pub mod approvals;
pub mod calls;
pub mod cancels;
pub mod crypto;
pub mod roomkeys;
pub mod store;
pub mod ui;

use approvals::Approvals;
use calls::CallGate;
use cancels::Cancels;
use roomkeys::RoomKeys;
use store::{normalize_server, Book, CallPolicy, RemoteProfile};
use ui::{RemoteUi, TaskMeta};

static SEQ: AtomicU64 = AtomicU64::new(1);

pub(crate) fn frame_id(prefix: &str) -> String {
    format!("{prefix}{}-{}", now_ms(), SEQ.fetch_add(1, Ordering::Relaxed))
}

pub async fn cli(rt: Arc<Runtime>, root: PathBuf, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("pair") => pair(&root, args).await,
        Some("serve") => serve(rt, root).await,
        Some("status") => status(&root),
        Some("list") => status(&root),
        Some("use") => match args.get(1) {
            Some(n) => {
                let mut book = Book::load(&root)?;
                if book.select(n) {
                    book.save(&root)?;
                    println!("已选择：{}", book.active().map(|p| p.title().to_string()).unwrap_or_default());
                } else {
                    println!("没有这一条配对：{n}");
                }
                Ok(())
            }
            None => {
                println!("用法：aox remote use <序号|id>");
                status(&root)
            }
        },
        Some("rename") => match (args.get(1), args.get(2)) {
            (Some(n), Some(label)) => {
                let mut book = Book::load(&root)?;
                if book.rename(n, label) {
                    book.save(&root)?;
                    println!("已重命名。");
                } else {
                    println!("没有这一条配对：{n}");
                }
                Ok(())
            }
            _ => {
                println!("用法：aox remote rename <序号|id> <名字>");
                Ok(())
            }
        },
        Some("policy") => match args.get(1) {
            Some(want) => {
                let Some(p) = CallPolicy::parse(want) else {
                    println!("用法：aox remote policy <ask|owner|open>");
                    return Ok(());
                };
                let mut book = Book::load(&root)?;
                {
                    let Some(active) = book.active_mut() else {
                        println!("还没有配对（先 aox remote pair）");
                        return Ok(());
                    };
                    active.call_policy = p;
                }
                book.save(&root)?;
                println!("已设置：{}（{}）", p.as_str(), policy_hint(p));
                Ok(())
            }
            None => {
                let book = Book::load(&root)?;
                match book.active() {
                    Some(p) => {
                        println!("当前策略：{}（{}）", p.call_policy.as_str(), policy_hint(p.call_policy));
                        println!("  ask   别人 @ 时先问你（默认）");
                        println!("  owner 只执行你自己的指令");
                        println!("  open  谁都能用，不再询问");
                    }
                    None => println!("还没有配对。"),
                }
                Ok(())
            }
        },
        Some("remove") | Some("unpair") => remove_pairing(&root, args.get(1).map(String::as_str)),
        _ => {
            help();
            Ok(())
        }
    }
}

fn policy_hint(p: CallPolicy) -> &'static str {
    match p {
        CallPolicy::Ask => "别人 @ 时先问你",
        CallPolicy::Owner => "只执行你的指令",
        CallPolicy::Open => "谁都能用",
    }
}

fn help() {
    println!(
        r#"
AOX 远程（主机端）

  aox remote list                列出本机保存的所有配对（* 为当前）
  aox remote use <序号|id>        指定 serve 连接哪一条
  aox remote rename <序号|id> <名字>   给这条配对起个本机叫法
  aox remote remove [序号|id]     删除配对（不带参数 = 删当前那条）
  aox remote status              当前配对与服务端
  aox remote policy [ask|owner|open]   别人 @ 这台机器时怎么办（不带参数 = 查看）

  aox remote pair [服务端地址] [配对码] [Agent 名称]
      新增一条配对。地址与配对码都可以不带参数、按提示输入。
      配对码在手机 App 的「添加电脑」里生成，5 分钟内有效。

  aox remote serve
      常驻运行当前配对：收到 @你的Agent 的指令，就在本机执行并把过程与结果发回房间。
"#
    );
}

fn is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    if std::io::stdin().read_line(&mut s)? == 0 {
        bail!("没有读到输入，已取消");
    }
    Ok(s.trim().to_string())
}

fn prompt_or(label: &str, default: &str) -> Result<String> {
    let v = prompt(&format!("{label} [{default}]："))?;
    Ok(if v.is_empty() { default.to_string() } else { v })
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "aox-host".into())
}

fn default_handle(host: &str) -> String {
    let mut s = String::from("aox-");
    for c in host.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-').to_string();
    if s.len() <= 4 {
        "aox-host".into()
    } else {
        s
    }
}

#[derive(Deserialize)]
struct PairResp {
    token: String,
    device_id: String,
    agent: PairAgent,
    user: PairUser,
}

#[derive(Deserialize)]
struct PairUser {
    id: String,
}

#[derive(Deserialize)]
struct PairAgent {
    id: String,
    handle: String,
}

/// 配对的纯逻辑：不读 stdin、不打印。CLI 与 GUI 共用。
pub async fn pair_with(
    raw_server: &str,
    raw_code: &str,
    raw_handle: &str,
    device_name: &str,
    root: &Path,
) -> Result<RemoteProfile> {
    let server = normalize_server(raw_server)?;
    let code = raw_code.trim().to_ascii_uppercase();
    if code.is_empty() {
        bail!("配对码不能为空");
    }
    let handle = raw_handle.trim().to_ascii_lowercase();
    if handle.is_empty() {
        bail!("Agent 名称不能为空");
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let resp = client
        .post(format!("{server}/api/devices/pair"))
        .json(&serde_json::json!({
            "code": code,
            "agent_handle": handle,
            "agent_name": device_name,
            "device_name": device_name,
        }))
        .send()
        .await
        .with_context(|| format!("连不上 {server}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("配对失败（HTTP {status}）：{body}");
    }
    let paired: PairResp =
        serde_json::from_str(&body).context("服务端返回的不是预期的配对结果")?;

    let profile = RemoteProfile {
        id: format!("p{}", now_ms()),
        server,
        device_id: paired.device_id,
        token: paired.token,
        agent_id: paired.agent.id,
        agent_handle: paired.agent.handle,
        device_name: device_name.to_string(),
        owner_id: paired.user.id,
        label: String::new(),
        call_policy: store::CallPolicy::default(),
        created_at: now_ms(),
        last_connected_at: 0,
    };
    let mut book = Book::load(root)?;
    book.upsert(profile.clone());
    book.save(root)?;
    Ok(profile)
}

async fn pair(root: &Path, args: &[String]) -> Result<()> {
    println!("\nAOX 远程 · 配对");
    println!("服务端地址请填写你自己那台服务器的地址，本程序不内置任何服务器。\n");

    let server = match args.get(1) {
        Some(s) => normalize_server(s)?,
        None => normalize_server(&prompt("服务端地址（例如 https://chat.example.com）：")?)?,
    };
    let code = match args.get(2) {
        Some(c) => c.trim().to_ascii_uppercase(),
        None => prompt("配对码（手机 App → 添加电脑）：")?.to_ascii_uppercase(),
    };
    if code.is_empty() {
        bail!("配对码不能为空");
    }

    let host = hostname();
    let handle = match args.get(3) {
        Some(h) => h.trim().to_ascii_lowercase(),
        None if !is_tty() => default_handle(&host),
        None => prompt_or("Agent 名称", &default_handle(&host))?.to_ascii_lowercase(),
    };
    let device_name = if is_tty() {
        prompt_or("设备名称", &host)?
    } else {
        host.clone()
    };

    println!("\n正在向 {server} 配对…");
    let profile = pair_with(&server, &code, &handle, &device_name, root).await?;
    println!("\n配对成功。");
    println!("  服务端  {}", profile.server);
    println!("  Agent   @{}", profile.agent_handle);
    println!("  凭证    {}", RemoteProfile::path(root).display());
    println!("\n下一步：aox remote serve");
    println!("在手机或群里发：@{} 你的指令", profile.agent_handle);
    Ok(())
}

fn status(root: &Path) -> Result<()> {
    let book = Book::load(root)?;
    if book.pairings.is_empty() {
        println!("尚未配对。运行：aox remote pair");
        return Ok(());
    }
    println!("共 {} 条配对（* 为当前）：", book.pairings.len());
    for (i, p) in book.pairings.iter().enumerate() {
        let mark = if Some(&p.id) == book.active.as_ref() { "*" } else { " " };
        let name = if p.label.trim().is_empty() { "" } else { p.label.as_str() };
        println!("{mark} {}. {name} {}", i + 1, p.server);
        println!("     @{:<16} {}  {}", p.agent_handle, p.device_name, RemoteProfile::path(root).display());
    }
    Ok(())
}

fn remove_pairing(root: &Path, needle: Option<&str>) -> Result<()> {
    let mut book = Book::load(root)?;
    if book.pairings.is_empty() {
        println!("本来就没有配对。");
        return Ok(());
    }
    let target = match needle {
        Some(n) => n.to_string(),
        // 没给参数就删当前那条
        None => match book.active.clone() {
            Some(id) => id,
            None => {
                println!("没有当前配对；用 aox remote remove <序号> 指定一条。");
                return Ok(());
            }
        },
    };
    if book.remove(&target) {
        book.save(root)?;
        println!("已删除。剩下 {} 条。", book.pairings.len());
    } else {
        println!("没有这一条配对：{target}");
    }
    Ok(())
}

/// 给前端（GUI 的远程页）看的事件。CLI 里这些同时打到 stdout。
#[derive(Clone, Debug)]
pub enum RemoteEvent {
    Status(String),
    Log(String),
}

/// 接管凭证：桥在运行时写一个心跳文件，GUI 盯着它决定要不要锁屏。
/// 点「取回控制权」= 删掉这个文件；桥发现文件没了就自己退出。
pub fn lock_path(root: &Path) -> PathBuf {
    root.join(".aox").join("remote.lock")
}

fn write_lock(root: &Path, profile: &RemoteProfile, who: &str, since: i64) {
    let path = lock_path(root);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = serde_json::json!({
        "pid": std::process::id(),
        "server": profile.server,
        "agent": profile.agent_handle,
        "who": who,
        "since": since,
    });
    let _ = std::fs::write(path, body.to_string());
}

/// 连接者是谁：手机账号的显示名。拿不到就退回"手机端"，不编造。
async fn owner_name(profile: &RemoteProfile) -> String {
    let url = format!("{}/api/me", profile.server.trim_end_matches('/'));
    match reqwest::Client::new()
        .get(url)
        .bearer_auth(&profile.token)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            let v: serde_json::Value = r.json().await.unwrap_or_default();
            v["user"]["display_name"]
                .as_str()
                .or_else(|| v["user"]["handle"].as_str())
                .unwrap_or("手机端")
                .to_string()
        }
        _ => "手机端".to_string(),
    }
}

async fn serve(rt: Arc<Runtime>, root: PathBuf) -> Result<()> {
    serve_with(rt, root, None, Arc::new(std::sync::atomic::AtomicBool::new(false))).await
}

/// 常驻主体。`events` 给了就发事件（GUI），`stop` 置位后在一个心跳周期内退出。
///
/// 授权只在**手机**上决定：桌面端不弹框（用户 2026-11-03 要求），CLI 只在交互终端里打一行
/// 提示，不打断任何东西。
pub async fn serve_with(
    rt: Arc<Runtime>,
    root: PathBuf,
    events: Option<mpsc::UnboundedSender<RemoteEvent>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    use std::sync::atomic::Ordering;

    let emit = |e: RemoteEvent| {
        if let Some(tx) = &events {
            let _ = tx.send(e);
        }
    };

    let book = Book::load(&root)?;
    let profile = book
        .active()
        .cloned()
        .context("还没有配对（或没有选中哪一条）。先运行：aox remote pair")?;

    // 连接者名字取一次就够：接管期间不会换人。
    let who = owner_name(&profile).await;
    let since = now_ms();
    write_lock(&root, &profile, &who, since);

    // 心跳 + 看门狗合一：每 5 秒写一次；如果文件被别人删了（GUI 点了"取回控制权"），
    // 立刻置 stop 并**不再重建**——重建会让那个按钮失效。
    let hb = {
        let stop_hb = stop.clone();
        let root_hb = root.clone();
        let profile_hb = profile.clone();
        let who_hb = who.clone();
        tokio::spawn(async move {
            let mut first = true;
            loop {
                if !first && !lock_path(&root_hb).exists() {
                    stop_hb.store(true, Ordering::Relaxed);
                    break;
                }
                write_lock(&root_hb, &profile_hb, &who_hb, since);
                first = false;
                tokio::time::sleep(Duration::from_secs(5)).await;
                if stop_hb.load(Ordering::Relaxed) {
                    break;
                }
            }
        })
    };

    println!("AOX 远程 · 主机常驻");
    println!("  服务端  {}", profile.server);
    println!("  Agent   @{}", profile.agent_handle);
    println!("  策略    别人的 @ → {}", profile.call_policy.as_str());
    println!("  （Ctrl+C 退出）\n");
    emit(RemoteEvent::Status(format!(
        "已配对：{} @{}",
        profile.server, profile.agent_handle
    )));

    let (job_tx, job_rx) = mpsc::unbounded_channel::<Job>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
    let approvals = Arc::new(Approvals::default());
    let cancels = Arc::new(Cancels::default());
    let keys = Arc::new(RoomKeys::load(&root));
    let calls = Arc::new(CallGate::default());
    spawn_stdin_control(calls.clone());

    tokio::spawn(worker(
        job_rx,
        rt.clone(),
        out_tx.clone(),
        approvals.clone(),
        cancels.clone(),
        keys.clone(),
        profile.clone(),
        root.clone(),
        rt.plugins_dir.clone(),
    ));

    let mut backoff = 3u64;
    while !stop.load(Ordering::Relaxed) {
        match run_once(
            &profile,
            &job_tx,
            &out_tx,
            &mut out_rx,
            &approvals,
            &cancels,
            &keys,
            &calls,
            &events,
            &stop,
        )
        .await
        {
            Ok(()) => {
                eprintln!("连接已关闭，{backoff}s 后重连");
                emit(RemoteEvent::Status(format!("连接已关闭，{backoff}s 后重连")));
            }
            Err(e) => {
                eprintln!("连接失败：{e:#}，{backoff}s 后重连");
                emit(RemoteEvent::Status(format!("连接失败：{e:#}")));
            }
        }
        // 分段睡：否则 stop 要等满 30 秒才生效。
        for _ in 0..backoff {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        backoff = (backoff * 2).min(30);
    }
    hb.abort();
    let _ = std::fs::remove_file(lock_path(&root));
    emit(RemoteEvent::Status("已停止".into()));
    Ok(())
}

struct Job {
    room: String,
    prompt: String,
    /// 只有设备主人能改配置（模型、工作空间）。群里的协作者可以下指令，但不能改你的机器。
    from_owner: bool,
    /// 这条消息带的图。**不在 WS 循环里下载**：几百 KB 的图也能把心跳卡住，交给 worker。
    media: Vec<aoxchat_proto::ArtifactView>,
    /// 这个房间的存储上限（MB），0 = 不限。由房主在房间设置里定，服务端在 `Room` 里带下来。
    /// 宿主的 inbox 只是服务端那份的副本，两边用同一个数字，否则"房间上限"管不住本机磁盘。
    cap_mb: u32,
}

/// 没见过这个房间时按服务端的默认值（200 MB）兜底。这不是宿主自己的策略，
/// 只是"服务端没告诉我"时的保守取值——真正说了算的是房主设的那个数。
const DEFAULT_ROOM_STORAGE_MB: u32 = 200;

async fn run_once(
    profile: &RemoteProfile,
    jobs: &mpsc::UnboundedSender<Job>,
    out_tx: &mpsc::UnboundedSender<Frame>,
    out_rx: &mut mpsc::UnboundedReceiver<Frame>,
    approvals: &Arc<Approvals>,
    cancels: &Arc<Cancels>,
    keys: &Arc<RoomKeys>,
    calls: &Arc<CallGate>,
    events: &Option<mpsc::UnboundedSender<RemoteEvent>>,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    let emit = |e: RemoteEvent| {
        if let Some(tx) = events {
            let _ = tx.send(e);
        }
    };
    let url = profile.ws_url();
    // 房间 → 我在这个房间里的名字。同一个 Agent 可以在 A 群叫 `aox`、在 B 群叫 `aox-b`，
    // 而 `@` 匹配必须用房间内的那个名字，不是配对时的全局 handle。
    let mut room_handles: HashMap<String, String> = HashMap::new();
    // 房间 → 存储上限（MB）。和 room_handles 一起维护：都来自同一个 `Room` 快照。
    let mut room_caps: HashMap<String, u32> = HashMap::new();
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("无法连接 {url}"))?;
    let (mut tx, mut rx) = ws.split();

    let hello = Frame::new(
        frame_id("h"),
        now_ms(),
        Body::Hello(Hello {
            auth: Auth {
                token: profile.token.clone(),
                device_id: profile.device_id.clone(),
                proto: PROTO_VERSION,
            },
            device: DeviceInfo {
                kind: DeviceKind::Host,
                name: profile.device_name.clone(),
                app_version: env!("CARGO_PKG_VERSION").to_string(),
            },
            resume: Default::default(),
        }),
    );
    let text = serde_json::to_string(&hello)?;
    tx.send(WsMessage::Text(text.into())).await?;

    // Handshake: anything other than hello_ok means we are not allowed in.
    let deadline = Duration::from_secs(15);
    loop {
        let msg = tokio::time::timeout(deadline, rx.next())
            .await
            .context("等待 hello_ok 超时")?
            .context("连接在握手阶段断开")?;
        let WsMessage::Text(t) = msg? else { continue };
        let frame: Frame = serde_json::from_str(&t).context("服务端发来无法解析的帧")?;
        match frame.body {
            Body::HelloOk(ok) => {
                room_handles.extend(ok.rooms.iter().filter_map(|r| {
                    r.agents
                        .iter()
                        .find(|a| a.owner_id == profile.owner_id && a.id == profile.agent_id)
                        .map(|a| (r.id.clone(), a.handle.clone()))
                }));
                room_caps.extend(ok.rooms.iter().map(|r| (r.id.clone(), r.max_storage_mb)));
                println!(
                    "已连接，{} 个房间。等待 @{} 的指令…",
                    ok.rooms.len(),
                    profile.agent_handle
                );
                emit(RemoteEvent::Status(format!(
                    "已连接 · {} 个房间 · @{}",
                    ok.rooms.len(),
                    profile.agent_handle
                )));
                break;
            }
            Body::Error(e) => bail!("服务端拒绝：{:?} {}", e.code, e.message),
            _ => {}
        }
    }

    let mut tick = tokio::time::interval(Duration::from_secs(25));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let ping = Frame::new(frame_id("h"), now_ms(), Body::Ping(Ping {}));
                tx.send(WsMessage::Text(serde_json::to_string(&ping)?.into())).await?;
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                // 每秒看一眼"取回控制权"：长连接可能挂几小时，不能只在重连时才检查。
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(());
                }
            }
            Some(frame) = out_rx.recv() => {
                tx.send(WsMessage::Text(serde_json::to_string(&frame)?.into())).await?;
            }
            incoming = rx.next() => {
                let Some(incoming) = incoming else { return Ok(()) };
                match incoming? {
                    WsMessage::Text(t) => {
                        let Ok(frame) = serde_json::from_str::<Frame>(&t) else { continue };
                        match frame.body {
                            Body::TaskCancel(c) => {
                                if cancels.cancel(&c.task_id) {
                                    println!("收到停止请求：{}", c.task_id);
                                } else {
                                    eprintln!("停止请求已过期：{}", c.task_id);
                                }
                            }
                            Body::ApprovalDecide(d) => {
                                if approvals.resolve(&d.approval_id, d.allow, d.for_task) {
                                    println!(
                                        "审批 {} → {}",
                                        d.approval_id,
                                        if d.allow { "允许" } else { "拒绝" }
                                    );
                                }
                            }
                            Body::CallDecide(d) => {
                                let remember =
                                    d.remember_secs.map(|s| Duration::from_secs(s as u64));
                                if calls.resolve(&d.call_id, d.allow, remember) {
                                    println!(
                                        "授权 {}：{}",
                                        d.call_id,
                                        if d.allow { "允许" } else { "拒绝" }
                                    );
                                } else {
                                    eprintln!("授权决定已过期：{}", d.call_id);
                                }
                            }
                            Body::MsgNew(MsgNew { msg }) => {
                                let msg = match decrypt_incoming(keys, msg) {
                                    Ok(m) => m,
                                    Err(reason) => {
                                        eprintln!("跳过一条消息：{reason}");
                                        continue;
                                    }
                                };
                                // 用这个房间里生效的名字匹配。新房间（运行期建/被拉进去）靠
                                // RoomUpdated 补进表里；表里没有才退回配对时的全局名。
                                let handle = room_handles
                                    .get(&msg.room)
                                    .map(String::as_str)
                                    .unwrap_or(&profile.agent_handle);
                                let Some(prompt) = mention_prompt(&msg, handle) else {
                                    continue;
                                };

                                println!("收到指令（{}）：{prompt}", msg.room);
                                let from_owner = msg.sender_id == profile.owner_id;
                                let media = match &msg.body {
                                    MessageBody::Media { artifacts, .. } => artifacts.clone(),
                                    _ => Vec::new(),
                                };
                                let job = Job {
                                    room: msg.room.clone(),
                                    prompt,
                                    from_owner,
                                    media,
                                    cap_mb: room_caps
                                        .get(&msg.room)
                                        .copied()
                                        .unwrap_or(DEFAULT_ROOM_STORAGE_MB),
                                };

                                // 斜杠命令先拦：这是配置操作，批了也会被 worker 拒，白问一次。
                                if !from_owner && job.prompt.trim_start().starts_with('/') {
                                    send_text(out_tx, &msg.room, "只有设备主人能执行配置命令");
                                    continue;
                                }

                                if from_owner || profile.call_policy == CallPolicy::Open {
                                    let _ = jobs.send(job);
                                } else if profile.call_policy == CallPolicy::Owner {
                                    send_text(
                                        out_tx,
                                        &msg.room,
                                        &format!("{}，我只听主人的。", msg.sender_name),
                                    );
                                } else if calls.is_remembered(&msg.room, &msg.sender_id) {
                                    // 一小时内已经允许过这个人。
                                    let _ = jobs.send(job);
                                } else {
                                    request_call(out_tx, jobs, calls, profile, &msg, handle, job);
                                }
                            }
                            Body::RoomUpdated(u) => {
                                // 房主刚把上限调了，下一条消息就该按新数清 inbox。
                                room_caps.insert(u.room.id.clone(), u.room.max_storage_mb);
                                // 房间里我的别名可能被改了，也可能刚被拉进一个新房间。
                                match u.room.agents.iter().find(|a| {
                                    a.owner_id == profile.owner_id && a.id == profile.agent_id
                                }) {
                                    Some(a) => {
                                        let is_new = room_handles
                                            .insert(u.room.id.clone(), a.handle.clone())
                                            .is_none();
                                        if is_new {
                                            println!(
                                                "新房间「{}」，我在里面的名字是 @{}",
                                                u.room.name, a.handle
                                            );
                                        }
                                    }
                                    None => {
                                        room_handles.remove(&u.room.id);
                                    }
                                }
                            }
                            Body::RoomRemoved(r) => {
                                room_handles.remove(&r.room);
                                room_caps.remove(&r.room);
                                println!("房间 {} 对我已不可用（{:?}）", r.room, r.reason);
                            }
                            Body::Error(e) => {
                                eprintln!("服务端错误：{:?} {}", e.code, e.message);
                            }
                            _ => {}
                        }
                    }
                    WsMessage::Close(_) => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}

/// 一条消息里可能同时点名多个 Agent。**所有** `@handle` 都要从句子里剔除，
/// 只剩"这台机器该做什么"；否则 `@a @b 一起看下` 会让 b 收到一句带 `@a` 的指令。
fn split_mentions(text: &str) -> (Vec<String>, String) {
    let chars: Vec<char> = text.chars().collect();
    let mut mentions = Vec::new();
    let mut rest = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            let start = i + 1;
            let mut j = start;
            while j < chars.len()
                && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '-' || chars[j] == '.')
            {
                j += 1;
            }
            if j > start {
                mentions.push(chars[start..j].iter().collect::<String>().to_ascii_lowercase());
                i = j;
                continue;
            }
        }
        rest.push(chars[i]);
        i += 1;
    }
    (mentions, rest.trim().to_string())
}

/// Only human messages addressed to this agent count. Two exclusions matter:
/// agents never obey agents (that is how a loop starts), and this host never obeys itself.
fn mention_prompt(msg: &aoxchat_proto::Message, handle: &str) -> Option<String> {
    if msg.sender_kind != aoxchat_proto::SenderKind::User {
        return None;
    }
    let MessageBody::Text { text } = &msg.body else {
        return None;
    };
    let (mentions, rest) = split_mentions(text);
    if !mentions.iter().any(|m| m == handle) {
        return None;
    }
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// 按**字符**截断，不是按字节：中文按字节切会切出半个字。
fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// 别人要调用这台机器：发一张授权卡给主人的手机，然后在**独立任务**里等结果。
///
/// 不能在这里 `await`：这条分支跑在 WS 读循环里，等两分钟不读 socket，心跳、取消、审批全部
/// 停摆，连接会被服务端判死。所以这里只登记 + 发包，等待交给 `tokio::spawn`。
fn request_call(
    out: &mpsc::UnboundedSender<Frame>,
    jobs: &mpsc::UnboundedSender<Job>,
    calls: &Arc<CallGate>,
    profile: &RemoteProfile,
    msg: &aoxchat_proto::Message,
    handle: &str,
    job: Job,
) {
    let id = frame_id("c");
    let rx = calls.wait(&id, &msg.room, &msg.sender_id);

    let req = CallRequest {
        call_id: id.clone(),
        agent_id: profile.agent_id.clone(),
        agent_handle: handle.to_string(),
        from_user_id: msg.sender_id.clone(),
        from_name: msg.sender_name.clone(),
        text: truncate(&job.prompt, 200),
        expires_at: now_ms() + calls::TIMEOUT.as_millis() as i64,
    };

    let frame = Frame::in_room(
        Frame::new(frame_id("f"), now_ms(), Body::CallRequest(req)),
        msg.room.clone(),
    );
    let _ = out.send(frame);

    // 终端里也要看得见、点得动：GUI 和手机会弹卡，CLI 用户只有这一行字。
    let short: String = id.chars().take(6).collect();
    println!("\n[授权] {} 想调用 @{}", msg.sender_name, handle);
    println!("       内容：{}", truncate(&job.prompt, 60));
    println!("       允许 allow {short} ｜ 本小时 allow1h {short} ｜ 拒绝 deny {short}");

    let out2 = out.clone();
    let jobs2 = jobs.clone();
    let room = msg.room.clone();
    let gate = calls.clone();
    tokio::spawn(async move {
        match tokio::time::timeout(calls::TIMEOUT, rx).await {
            Ok(Ok(true)) => {
                // 把结论也发回服务端：GUI 在本地点的"允许"服务端并不知道，登记项得清掉，
                // 否则手机上的卡片会一直挂到超时。手机点的则由服务端先处理，这里会收到
                // not_found，无害。
                let decided = Frame::in_room(
                    Frame::new(
                        frame_id("f"),
                        now_ms(),
                        Body::CallDecide(aoxchat_proto::CallDecide {
                            call_id: id.clone(),
                            allow: true,
                            remember_secs: None,
                        }),
                    ),
                    room.clone(),
                );
                let _ = out2.send(decided);
                // 主人同意了，这才真正进队列。
                let _ = jobs2.send(job);
            }
            Ok(Ok(false)) => send_text(&out2, &room, "主人没有同意这次调用。"),
            Ok(Err(_)) => send_text(&out2, &room, "授权请求已失效。"),
            Err(_) => send_text(&out2, &room, "等了两分钟没有回应，已取消。"),
        }
        gate.forget(&id);
    });
}

/// CLI 里的授权入口：`serve` 跑在前台，stdin 一直空着，正好用来敲 allow / deny。
///
/// GUI 和后台进程没有交互终端（`is_terminal()` 为假），直接不启动。这一点很关键：读不到
/// stdin 时绝不能把一个阻塞循环挂在那里，否则整个授权流程会被它拖死。
fn spawn_stdin_control(calls: Arc<CallGate>) {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return;
    }
    println!("（别人要调用这台机器时，会打印一行 [授权]；输入 allow / allow1h / deny + 前 6 位 id 即可）\n");
    tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        let mut line = String::new();
        loop {
            line.clear();
            match input.read_line(&mut line) {
                Ok(0) | Err(_) => break, // EOF：终端关了，安静退出
                Ok(_) => {}
            }
            let mut it = line.split_whitespace();
            let (Some(cmd), Some(prefix)) = (it.next(), it.next()) else { continue };
            let (allow, remember) = match cmd {
                "allow" => (true, None),
                "allow1h" => (true, Some(calls::REMEMBER)),
                "deny" => (false, None),
                _ => continue,
            };
            match calls.resolve_prefix(prefix, allow, remember) {
                Ok(id) => println!("  已处理 {id}"),
                Err(calls::PrefixErr::Ambiguous) => {
                    println!("  前缀 {prefix} 不唯一，请多输几位")
                }
                Err(calls::PrefixErr::NotFound) => {
                    println!("  没有这条待批请求（可能已经超时）")
                }
            }
        }
    });
}

/// 告诉服务端这台机器是不是在干活。服务端据此向所有相关房间推 presence。
fn send_agent_busy(out: &mpsc::UnboundedSender<Frame>, agent_id: &str, room: &str, busy: bool) {
    let frame = Frame::in_room(
        Frame::new(
            frame_id("f"),
            now_ms(),
            Body::AgentBusy(aoxchat_proto::AgentBusy {
                agent_id: agent_id.to_string(),
                busy,
            }),
        ),
        room.to_string(),
    );
    let _ = out.send(frame);
}

/// 任务结束时把"占用中"撤掉。用 Drop 而不是在每个出口手写，是因为 run_turn 的返回路径太多
/// （完成、报错、取消、panic），漏一条就会永远显示占用。
struct BusyGuard {
    out: mpsc::UnboundedSender<Frame>,
    agent_id: String,
    room: String,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        send_agent_busy(&self.out, &self.agent_id, &self.room, false);
    }
}

/// 把一条消息里的图拉到本地，返回可以直接喂给模型的附件。
///
/// 落在 `.aox/remote/inbox/<room>/`：一次会话一个目录，整目录清理即可，也不会和产物目录混。
/// 下载走能力链接、不带 Authorization——链接本身就是凭证，和服务端 `download` 的约定一致。
async fn download_media(
    http: &reqwest::Client,
    profile: &RemoteProfile,
    root: &std::path::Path,
    room: &str,
    items: &[aoxchat_proto::ArtifactView],
    cap_mb: u32,
) -> Vec<std::path::PathBuf> {
    let dir = root.join(".aox").join("remote").join("inbox").join(room);
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return Vec::new();
    }

    let mut out = Vec::new();
    // 服务端已经限过 9 张，这里再兜一次：真收到 100 条也不会把机器拖死。
    for a in items.iter().take(9) {
        let url = format!("{}{}", profile.server.trim_end_matches('/'), a.url);
        let resp = match http.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                eprintln!("拉取「{}」失败：HTTP {}", a.name, r.status());
                continue;
            }
            Err(e) => {
                eprintln!("拉取「{}」失败：{e}", a.name);
                continue;
            }
        };
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("读取「{}」失败：{e}", a.name);
                continue;
            }
        };
        let path = dir.join(format!("{}_{}", short_id(&a.id), safe_file_name(&a.name)));
        if let Err(e) = tokio::fs::write(&path, &bytes).await {
            eprintln!("写入「{}」失败：{e}", a.name);
            continue;
        }
        match Attachment::probe(&path) {
            // probe 只是校验（大小上限、是不是目录），真正的解析由 run_turn 自己做；
            // 这里提前拦一次，日志能落到具体文件名上，而不是一行"附件加载失败"。
            Ok(_) => out.push(path),
            Err(reason) => eprintln!("「{}」不能作为附件：{reason}", a.name),
        }
    }

    prune_inbox(&dir, cap_mb).await;
    out
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// 文件名里只留安全字符。服务端已经清洗过一遍，这里是第二道：`..` 之类不能跨出目录。
fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("file");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .take(80)
        .collect();
    if cleaned.trim().is_empty() || cleaned == "." || cleaned == ".." {
        "file".to_string()
    } else {
        cleaned
    }
}

/// inbox 只增不减会吃满磁盘。超过这个房间的上限就按修改时间从旧到新删。
///
/// `cap_mb` 来自房主在房间设置里设的数（0 = 不限），不是宿主自己的常量。这里删的是**本机
/// 的副本**，不影响服务端：手机再要这张图会重新下，代价是流量而不是数据丢失。
async fn prune_inbox(dir: &std::path::Path, cap_mb: u32) {
    if cap_mb == 0 {
        return;
    }
    let limit = cap_mb as u64 * 1024 * 1024;
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return };
    let mut entries: Vec<(u64, u64, std::path::PathBuf)> = Vec::new();
    let mut total = 0u64;
    while let Ok(Some(e)) = rd.next_entry().await {
        let Ok(md) = e.metadata().await else { continue };
        if !md.is_file() {
            continue;
        }
        let ts = md
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        total += md.len();
        entries.push((ts, md.len(), e.path()));
    }
    if total <= limit {
        return;
    }
    entries.sort_by_key(|(ts, _, _)| *ts);
    for (_, len, path) in entries {
        if total <= limit {
            break;
        }
        if tokio::fs::remove_file(&path).await.is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

/// 一个房间的执行上下文。`/cwd` 会把它整体换掉（新的 Runtime + 新的 Agent），
/// 所以运行时不能是 worker 的一个参数，而要是每个房间自己的东西。
struct RoomCtx {
    rt: Arc<Runtime>,
    agent: Agent,
}

async fn worker(
    mut jobs: mpsc::UnboundedReceiver<Job>,
    base: Arc<Runtime>,
    out: mpsc::UnboundedSender<Frame>,
    approvals: Arc<Approvals>,
    cancels: Arc<Cancels>,
    keys: Arc<RoomKeys>,
    profile: RemoteProfile,
    root: PathBuf,
    plugins_dir: PathBuf,
) {
    // One Agent per room: separate conversations, separate checkpoints, separate usage books.
    let mut rooms: HashMap<String, RoomCtx> = HashMap::new();
    let http = reqwest::Client::new();

    while let Some(job) = jobs.recv().await {
        if job.prompt.starts_with('/') {
            handle_command(&job, &mut rooms, &base, &root, &plugins_dir, &profile, &keys, &out).await;
            continue;
        }

        if !rooms.contains_key(&job.room) {
            let agent = Agent::new(base.clone());
            rooms.insert(job.room.clone(), RoomCtx { rt: base.clone(), agent });
        }

        // 从这一刻起这台机器是"占用中"：别的群再发 `@` 要看得见，不然只会以为没响应。
        // guard 在离开作用域时自动发 false，取消/报错/提前 return 都不会漏。
        send_agent_busy(&out, &profile.agent_id, &job.room, true);
        let _busy = BusyGuard {
            out: out.clone(),
            agent_id: profile.agent_id.clone(),
            room: job.room.clone(),
        };

        // 取出再放回：run_turn 要 &mut，而 await 期间不能一直借着 map。
        let mut ctx = rooms.remove(&job.room).expect("just inserted");

        // 图先落到本地：模型只能读路径，读不了 url。放在 worker 里做而不是 WS 循环，
        // 是为了不让下载把心跳和取消卡住。
        let attachments = if job.media.is_empty() {
            Vec::new()
        } else {
            download_media(&http, &profile, &root, &job.room, &job.media, job.cap_mb).await
        };
        if !attachments.is_empty() {
            println!("附件 {} 个", attachments.len());
        }

        let meta = TaskMeta {
            agent_id: profile.agent_id.clone(),
            agent_handle: profile.agent_handle.clone(),
            owner_id: profile.owner_id.clone(),
            prompt: job.prompt.clone(),
        };
        let mut ui = RemoteUi::new(out.clone(), job.room.clone(), approvals.clone(), meta);

        let started = std::time::Instant::now();
        println!("开始执行…");
        // 取消 = drop 掉这个 future + 关掉恢复轮次（和 GUI 的停止按钮同一条路径）。
        let mut cancel_rx = cancels.register(ui.task_id());
        let outcome = tokio::select! {
            r = ctx.agent.run_turn(&mut ui, &job.prompt, &attachments) => Some(r),
            _ = &mut cancel_rx => None,
        };
        match outcome {
            None => {
                let _ = ctx.rt.recovery.abort_turn();
                println!("已停止（{:.1}s）：手机点了停止", started.elapsed().as_secs_f32());
                send_text(&out, &job.room, &outbound(&keys, &job.room, "已停止。"));
                let task = ui.snapshot(aoxchat_proto::TaskStatus::Cancelled, None);
                if let Err(e) = upload_task(&profile, &job.room, &task).await {
                    println!("  ↑ 任务记录写入失败：{e:#}");
                }
            }
            Some(Ok(())) => {
                let text = ui.take_final_text();
                println!("执行结束（{:.1}s），回复 {} 字", started.elapsed().as_secs_f32(), text.chars().count());
                if !text.trim().is_empty() {
                    send_text(&out, &job.room, &outbound(&keys, &job.room, &text));
                }
                let task = ui.snapshot(aoxchat_proto::TaskStatus::Done, None);
                if let Err(e) = upload_task(&profile, &job.room, &task).await {
                    println!("  ↑ 任务记录写入失败：{e:#}");
                }
                // 产物在回复之后上传：先让人看到结论，再传文件。文件此刻是**暂存副本**，
                // 原件可能已经被这一轮删掉了。
                for file in ui.take_artifacts() {
                    match upload_artifact(&profile, &job.room, &file.name, &file.path).await {
                        Ok(name) => println!("  ↑ 已上传产物 {name}"),
                        Err(e) => {
                            println!("  ↑ 产物上传失败 {}：{e:#}", file.path.display());
                            // 手机上看不到宿主的标准输出。文件没送到就得说出来，
                            // 否则模型那句"已经发到房间里了"就是唯一的说法，而它是错的。
                            let msg = format!("产物「{}」没传上去：{e}", file.name);
                            send_text(&out, &job.room, &outbound(&keys, &job.room, &msg));
                        }
                    }
                }
            }
            Some(Err(e)) => {
                let msg = format!("{e:#}");
                println!("执行失败（{:.1}s）：{msg}", started.elapsed().as_secs_f32());
                send_text(&out, &job.room, &outbound(&keys, &job.room, &format!("执行失败：{msg}")));
                let task = ui.snapshot(aoxchat_proto::TaskStatus::Failed, Some(msg));
                if let Err(e) = upload_task(&profile, &job.room, &task).await {
                    println!("  ↑ 任务记录写入失败：{e:#}");
                }
            }
        }

        cancels.finish(ui.task_id());
        rooms.insert(job.room.clone(), ctx);
    }
}

/// 斜杠命令：改模型、换工作空间。只有设备主人能执行——群里的协作者可以下指令，
/// 但不能替你改这台机器的执行环境。
async fn handle_command(
    job: &Job,
    rooms: &mut HashMap<String, RoomCtx>,
    base: &Arc<Runtime>,
    root: &Path,
    plugins_dir: &Path,
    profile: &RemoteProfile,
    keys: &Arc<RoomKeys>,
    out: &mpsc::UnboundedSender<Frame>,
) {
    let trimmed = job.prompt.trim();
    let mut it = trimmed.splitn(2, char::is_whitespace);
    let cmd = it.next().unwrap_or("");
    let arg = it.next().unwrap_or("").trim();

    if !job.from_owner {
        send_text(out, &job.room, "只有设备主人能执行配置命令");
        return;
    }

    match cmd {
        "/model" => {
            let mut book = crate::modelbook::Book::load(root);
            if arg.is_empty() {
                let active = book.active.clone().unwrap_or_default();
                let cur = book
                    .active_record()
                    .map(|r| format!("{}（{}）", r.model, r.id))
                    .unwrap_or_else(|| "未设置".into());
                let mut text = format!("当前模型：{cur}");
                if !book.models.is_empty() {
                    text.push_str("\n可用：");
                    for m in &book.models {
                        text.push_str(&format!(
                            "\n{} {}",
                            if m.id == active { "●" } else { "○" },
                            m.id
                        ));
                    }
                }
                send_text(out, &job.room, &text);
            } else if book.set_active(arg) {
                if let Err(e) = book.save(root) {
                    send_text(out, &job.room, &format!("保存失败：{e:#}"));
                    return;
                }
                if let Some(cfg) = book.active_config() {
                    let model = cfg.model.clone();
                    let _ = base.provider.apply(cfg.clone());
                    for ctx in rooms.values() {
                        let _ = ctx.rt.provider.apply(cfg.clone());
                    }
                    send_text(out, &job.room, &format!("已切换到 {model}（{arg}）"));
                }
            } else {
                send_text(out, &job.room, &format!("没有这个模型：{arg}"));
            }
        }
        "/key" => {
            if arg.is_empty() || arg.eq_ignore_ascii_case("off") {
                match keys.clear(&job.room) {
                    Ok(true) => send_text(out, &job.room, "本房间已关闭加密（历史密文将无法再解）。"),
                    Ok(false) => send_text(out, &job.room, "本房间本来就没有设置口令。"),
                    Err(e) => send_text(out, &job.room, &format!("清除失败：{e:#}")),
                }
            } else {
                match keys.set(&job.room, arg) {
                    Ok(()) => send_text(
                        out,
                        &job.room,
                        "本房间已启用加密。\n⚠ 手机端还没做口令输入：启用后手机上只会看到一串 enc:v1: 开头的密文。\n口令不上网，忘了我这边也解不开。",
                    ),
                    Err(e) => send_text(out, &job.room, &format!("保存失败：{e:#}")),
                }
            }
        }
        "/cwd" => {
            if arg.is_empty() {
                let cur = base
                    .config
                    .workplace
                    .root
                    .clone()
                    .unwrap_or_else(|| root.display().to_string());
                send_text(
                    out,
                    &job.room,
                    &format!("当前工作空间：{cur}\n用法：/cwd D:\\work\\proj"),
                );
                return;
            }
            let path = PathBuf::from(arg);
            if !path.is_dir() {
                send_text(out, &job.room, &format!("目录不存在：{}", path.display()));
                return;
            }
            let mut cfg = base.config.clone();
            cfg.workplace.mode = "fixed".into();
            cfg.workplace.root = Some(path.to_string_lossy().to_string());
            match crate::bootstrap::build(cfg, root.to_path_buf(), plugins_dir.to_path_buf()).await {
                Ok(new_rt) => {
                    let agent = Agent::new(new_rt.clone());
                    rooms.insert(job.room.clone(), RoomCtx { rt: new_rt, agent });
                    send_text(
                        out,
                        &job.room,
                        &format!(
                            "工作空间已切到 {}\n本房间的下一轮从新目录开始（模型沿用当前设置）",
                            path.display()
                        ),
                    );
                }
                Err(e) => send_text(out, &job.room, &format!("切换失败：{e:#}")),
            }
        }
        _ => send_text(
            out,
            &job.room,
            "可用命令：\n/model [id]  查看或切换模型\n/cwd [路径]   查看或切换工作空间",
        ),
    }
    let _ = profile; // 目前命令只用得到 owner 判定，保留参数便于后续加 /status
}

/// 出站：本房间有口令就加密，否则原样发。加密只是字符串变换，服务端不需要知道。
fn outbound(keys: &RoomKeys, room: &str, text: &str) -> String {
    match keys.get(room) {
        Some(k) => crypto::encrypt(&k, text),
        None => text.to_string(),
    }
}

/// inbound：密文必须能解开才处理。解不开就跳过并说明原因——
/// 把密文当普通文本喂给模型，等于让它对着一串 base64 编故事。
fn decrypt_incoming(
    keys: &RoomKeys,
    mut msg: aoxchat_proto::Message,
) -> std::result::Result<aoxchat_proto::Message, String> {
    let MessageBody::Text { text } = &msg.body else {
        return Ok(msg);
    };
    if !crypto::is_encrypted(text) {
        return Ok(msg);
    }
    let Some(key) = keys.get(&msg.room) else {
        return Err("收到密文，但本机未设置口令（先发 /key <口令>）".to_string());
    };
    match crypto::decrypt(&key, text) {
        Some(plain) => {
            msg.body = MessageBody::Text { text: plain };
            Ok(msg)
        }
        None => Err("口令不对，解不开这条消息".to_string()),
    }
}

fn send_text(out: &mpsc::UnboundedSender<Frame>, room: &str, text: &str) {
    let frame = Frame::in_room(
        Frame::new(
            frame_id("h"),
            now_ms(),
            Body::MsgSend(MsgSend {
                client_msg_id: frame_id("c"),
                body: MessageBody::Text { text: text.to_string() },
                // 宿主发的是机器人的话，不引用任何人的消息。字段在协议里是
                // `Option` + `skip_serializing_if`，这里显式写 None 只是为了跟着 crate 走。
                reply_to: None,
            }),
        ),
        room,
    );
    let _ = out.send(frame);
}

/// 与服务端 `MAX_ARTIFACT_BYTES` 对齐。本地先挡一次，省得把 128MB 传上去再被拒。
const MAX_UPLOAD: u64 = 128 * 1024 * 1024;

fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "apk" => "application/vnd.android.package-archive",
        "md" => "text/markdown",
        "json" => "application/json",
        "txt" | "log" | "rs" | "dart" | "py" | "js" | "ts" | "toml" | "yaml" | "yml" => {
            "text/plain"
        }
        _ => "application/octet-stream",
    }
}

/// 一轮结束时的任务快照：写进历史，重开 App 还能看到这次执行做过什么。
async fn upload_task(
    profile: &RemoteProfile,
    room: &str,
    task: &aoxchat_proto::TaskView,
) -> Result<()> {
    let url = format!("{}/api/rooms/{room}/tasks", profile.server.trim_end_matches('/'));
    let res = reqwest::Client::new()
        .post(url)
        .bearer_auth(&profile.token)
        .json(task)
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("HTTP {} {}", res.status(), res.text().await.unwrap_or_default());
    }
    Ok(())
}

/// 上传一个文件并让它出现在房间里。服务端一次请求完成"存文件 + 落库 + 发消息"。
async fn upload_artifact(
    profile: &RemoteProfile,
    room: &str,
    name: &str,
    path: &std::path::Path,
) -> Result<String> {
    let meta = tokio::fs::metadata(path).await?;
    if meta.len() > MAX_UPLOAD {
        anyhow::bail!("超过 {} MB", MAX_UPLOAD / 1024 / 1024);
    }
    let mut url = reqwest::Url::parse(&format!(
        "{}/api/rooms/{room}/artifacts",
        profile.server.trim_end_matches('/')
    ))?;
    // 用 Url 的 query_pairs_mut 而不是手拼字符串：中文文件名必须被正确编码。
    url.query_pairs_mut()
        .append_pair("name", name)
        .append_pair("mime", mime_of(name));

    let bytes = tokio::fs::read(path).await?;
    let res = reqwest::Client::new()
        .post(url)
        .bearer_auth(&profile.token)
        .header("content-type", "application/octet-stream")
        .body(bytes)
        .send()
        .await?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("HTTP {status} {text}");
    }
    let v: serde_json::Value = serde_json::from_str(&text)?;
    Ok(v["name"].as_str().unwrap_or(name).to_string())
}
