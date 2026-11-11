//! aox remote: the host side of AOX Remote; two verbs for the person, one long-running loop underneath.
//! pair trades a server address + pairing code for a device credential; serve runs a local turn on @-mentions.
//! status and unpair show/remove the config; no endpoint is compiled in (address is stored in .aox/remote.json).

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
pub mod context;
pub mod crypto;
pub mod roomkeys;
pub mod store;
pub mod tool;
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

/// interactive gates prompts: CLI pair true, REPL /remote pair false (its line editor owns stdin).
pub async fn cli(rt: Arc<Runtime>, root: PathBuf, args: &[String], interactive: bool) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("pair") => pair(&root, args, interactive).await,
        Some("serve") => serve(rt, root).await,
        Some("status") => status(&root),
        Some("list") => status(&root),
        Some("use") => match args.get(1) {
            Some(n) => {
                let mut book = Book::load(&root)?;
                if book.select(n) {
                    book.save(&root)?;
                    println!("Selected: {}", book.active().map(|p| p.title().to_string()).unwrap_or_default());
                } else {
                    println!("No such pairing: {n}");
                }
                Ok(())
            }
            None => {
                println!("Usage: aox remote use <index|id>");
                status(&root)
            }
        },
        Some("rename") => match (args.get(1), args.get(2)) {
            (Some(n), Some(label)) => {
                let mut book = Book::load(&root)?;
                if book.rename(n, label) {
                    book.save(&root)?;
                    println!("Renamed.");
                } else {
                    println!("No such pairing: {n}");
                }
                Ok(())
            }
            _ => {
                println!("Usage: aox remote rename <index|id> <name>");
                Ok(())
            }
        },
        Some("policy") => match args.get(1) {
            Some(want) => {
                let Some(p) = CallPolicy::parse(want) else {
                    println!("Usage: aox remote policy <ask|owner|open>");
                    return Ok(());
                };
                let mut book = Book::load(&root)?;
                {
                    let Some(active) = book.active_mut() else {
                        println!("No pairing yet (run aox remote pair first)");
                        return Ok(());
                    };
                    active.call_policy = p;
                }
                book.save(&root)?;
                println!("Set: {} ({})", p.as_str(), policy_hint(p));
                Ok(())
            }
            None => {
                let book = Book::load(&root)?;
                match book.active() {
                    Some(p) => {
                        println!("Current policy: {} ({})", p.call_policy.as_str(), policy_hint(p.call_policy));
                        println!("  ask   ask first when others @ (default)");
                        println!("  owner only run your own instructions");
                        println!("  open  anyone can use, no more asking");
                    }
                    None => println!("No pairing yet."),
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
        CallPolicy::Ask => "ask first when others @",
        CallPolicy::Owner => "only run your instructions",
        CallPolicy::Open => "anyone can use",
    }
}

fn help() {
    println!(
        r#"
AOX Remote (host side)

  aox remote list                list all pairings saved on this machine (* = active)
  aox remote use <index|id>        choose which one serve connects to
  aox remote rename <index|id> <name>   give this pairing a local name
  aox remote remove [index|id]     delete a pairing (no argument = delete the current one)
  aox remote status              current pairing and server
  aox remote policy [ask|owner|open]   what to do when others @ this machine (no argument = view)

  aox remote pair [server address] [pairing code] [Agent name]
      Add a pairing. Address and pairing code can be omitted and entered at the prompts.
      The pairing code is generated in the phone app's "Add Computer", valid for 5 minutes.

  aox remote serve
      Run the current pairing continuously: on an @-mention to your Agent, run it on this machine and post the process and result back to the room.
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
        bail!("No input read, cancelled");
    }
    Ok(s.trim().to_string())
}

fn prompt_or(label: &str, default: &str) -> Result<String> {
    let v = prompt(&format!("{label} [{default}]: "))?;
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

/// Pure pairing logic: no stdin, no printing, shared by CLI and GUI.
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
        bail!("Pairing code cannot be empty");
    }
    let handle = raw_handle.trim().to_ascii_lowercase();
    if handle.is_empty() {
        bail!("Agent name cannot be empty");
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
        .with_context(|| format!("Cannot reach {server}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Pairing failed (HTTP {status}): {body}");
    }
    let paired: PairResp =
        serde_json::from_str(&body).context("Server did not return the expected pairing result")?;

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
        rooms: Default::default(),
    };
    let mut book = Book::load(root)?;
    book.upsert(profile.clone());
    book.save(root)?;
    Ok(profile)
}

async fn pair(root: &Path, args: &[String], interactive: bool) -> Result<()> {
    println!("\nAOX Remote · Pairing");
    println!("Enter the address of your own server; this program does not bundle any server.\n");

    // Where prompting is impossible (REPL), require all args: a missing one reports usage instead of touching stdin.
    let ask = interactive && is_tty();

    let server = match args.get(1) {
        Some(s) => normalize_server(s)?,
        None if !ask => bail!("Missing server address: aox remote pair <server address> <pairing code> [Agent name]"),
        None => normalize_server(&prompt("Server address (e.g. https://chat.example.com): ")?)?,
    };
    let code = match args.get(2) {
        Some(c) => c.trim().to_ascii_uppercase(),
        None if !ask => bail!("Missing pairing code: aox remote pair <server address> <pairing code> [Agent name]"),
        None => prompt("Pairing code (phone app → Add Computer): ")?.to_ascii_uppercase(),
    };
    if code.is_empty() {
        bail!("Pairing code cannot be empty");
    }

    let host = hostname();
    let handle = match args.get(3) {
        Some(h) => h.trim().to_ascii_lowercase(),
        None if !ask => default_handle(&host),
        None => prompt_or("Agent name", &default_handle(&host))?.to_ascii_lowercase(),
    };
    let device_name = if ask { prompt_or("Device name", &host)? } else { host.clone() };

    println!("\nPairing with {server}...");
    let profile = pair_with(&server, &code, &handle, &device_name, root).await?;
    println!("\nPairing succeeded.");
    println!("  Server   {}", profile.server);
    println!("  Agent   @{}", profile.agent_handle);
    println!("  Credential  {}", RemoteProfile::path(root).display());
    println!("\nNext: aox remote serve");
    println!("Send from your phone or a group: @{} your instruction", profile.agent_handle);
    Ok(())
}

fn status(root: &Path) -> Result<()> {
    let book = Book::load(root)?;
    if book.pairings.is_empty() {
        println!("Not paired yet. Run: aox remote pair");
        return Ok(());
    }
    println!("{} pairing(s) (* = active):", book.pairings.len());
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
        println!("No pairings to begin with.");
        return Ok(());
    }
    let target = match needle {
        Some(n) => n.to_string(),
        // With no argument, delete the current entry.
        None => match book.active.clone() {
            Some(id) => id,
            None => {
                println!("No active pairing; specify one with aox remote remove <index>.");
                return Ok(());
            }
        },
    };
    if book.remove(&target) {
        book.save(root)?;
        println!("Deleted. {} remaining.", book.pairings.len());
    } else {
        println!("No such pairing: {target}");
    }
    Ok(())
}

/// Events for the frontend (GUI remote page); in the CLI these also go to stdout.
#[derive(Clone, Debug)]
pub enum RemoteEvent {
    Status(String),
    Log(String),
}

/// Bridge heartbeat file the GUI watches; Reclaim Control deletes it, and the bridge exits once it is gone.
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

/// Who is connected: the phone account's display name; fall back to a generic mobile label, never invent one.
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
                .unwrap_or("Mobile")
                .to_string()
        }
        _ => "Mobile".to_string(),
    }
}

async fn serve(rt: Arc<Runtime>, root: PathBuf) -> Result<()> {
    serve_with(rt, root, None, Arc::new(std::sync::atomic::AtomicBool::new(false)), true).await
}

/// REPL /remote start is background, takes no stdin (avoids line-editor conflict); approvals are phone taps.
pub async fn serve_headless(
    rt: Arc<Runtime>,
    root: PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    serve_with(rt, root, None, stop, false).await
}

/// REPL /remote is one status screen (configured? running?), not a full manual (still in /remote list).
pub fn brief(root: &Path, running: bool) -> String {
    let mut out = String::new();
    out.push_str(if running {
        "Remote service: running\n"
    } else {
        "Remote service: not running (/remote start to launch)\n"
    });
    match Book::load(root) {
        Ok(book) if !book.pairings.is_empty() => {
            out.push_str(&format!("{} pairing(s) (* = active):\n", book.pairings.len()));
            for (i, p) in book.pairings.iter().enumerate() {
                let mark = if Some(&p.id) == book.active.as_ref() { "*" } else { " " };
                out.push_str(&format!("{mark} {}. {} @{}\n", i + 1, p.title(), p.agent_handle));
            }
        }
        _ => out.push_str("No pairing yet (/remote pair <server address> <pairing code>)\n"),
    }
    out.push_str("Other subcommands same as aox remote: list / use / pair / policy / remove / rename");
    out
}

/// Resident loop: events and stop within one heartbeat; approvals phone-only; control_stdin for aox remote serve.
pub async fn serve_with(
    rt: Arc<Runtime>,
    root: PathBuf,
    events: Option<mpsc::UnboundedSender<RemoteEvent>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    control_stdin: bool,
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
        .context("No pairing yet (or none selected). Run first: aox remote pair")?;

    // Fetch the connected user's name once; it cannot change during a takeover.
    let who = owner_name(&profile).await;
    let since = now_ms();
    write_lock(&root, &profile, &who, since);

    // Heartbeat/watchdog writes every 5s; GUI deletion (reclaim) sets stop and is never rebuilt.
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

    println!("AOX Remote · Host resident");
    println!("  Server   {}", profile.server);
    println!("  Agent   @{}", profile.agent_handle);
    println!("  Policy   others' @ → {}", profile.call_policy.as_str());
    println!("  (Ctrl+C to quit)\n");
    emit(RemoteEvent::Status(format!(
        "Paired: {} @{}",
        profile.server, profile.agent_handle
    )));

    let (job_tx, job_rx) = mpsc::unbounded_channel::<Job>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
    let approvals = Arc::new(Approvals::default());
    let cancels = Arc::new(Cancels::default());
    let keys = Arc::new(RoomKeys::load(&root));
    let calls = Arc::new(CallGate::default());
    if control_stdin {
        spawn_stdin_control(calls.clone());
    }

    // room_history/room_pull need aoxchat; unregister then register replaces cleanly across service restarts.
    let room_now: tool::Slot = Arc::new(tokio::sync::RwLock::new(None));
    for t in tool::tools(room_now.clone(), profile.clone(), keys.clone()) {
        rt.unregister(t.name()).await;
        if let Err(e) = rt.register(t).await {
            // Failing to register is non-fatal: the turn still runs, the model just lacks a history-lookup entry point.
            println!("  ↑ room tools failed to register: {e:#}");
        }
    }

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
        room_now,
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
                eprintln!("Connection closed, retrying in {backoff}s");
                emit(RemoteEvent::Status(format!("Connection closed, retrying in {backoff}s")));
            }
            Err(e) => {
                eprintln!("Connection failed: {e:#}, retrying in {backoff}s");
                emit(RemoteEvent::Status(format!("Connection failed: {e:#}")));
            }
        }
        // Sleep in short slices, otherwise stop takes a full 30s to take effect.
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
    emit(RemoteEvent::Status("Stopped".into()));
    Ok(())
}

struct Job {
    room: String,
    /// Room name (from hello_ok rooms), used only to title sessions; empty when missing, never an invented room id.
    room_name: String,
    /// Id of the message that triggered this turn; the phone hangs its running indicator under that original.
    msg_id: String,
    prompt: String,
    /// Only the owner can change config (model, workspace); collaborators may instruct but not reconfigure it.
    from_owner: bool,
    /// Images on this message go to the worker, not the WS loop, since a few hundred KB can stall the heartbeat.
    media: Vec<aoxchat_proto::ArtifactView>,
    /// Which message this quotes; fetching, decrypting and downloads all happen in the worker (see context.rs).
    quote: Option<aoxchat_proto::ReplyView>,
    /// Room storage cap (MB, 0 = unlimited) set by the owner; the host inbox is a server copy using the same number.
    cap_mb: u32,
    /// Seq of the triggering message; room_history reads backward from it, excluding itself.
    last_seq: i64,
}

/// Unseen rooms fall back to the server default (200MB), not host policy; the owner's cap really governs.
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
    // Room -> my in-room name; @-matching uses it, not the global handle, since one Agent differs per room.
    let mut room_handles: HashMap<String, String> = HashMap::new();
    // Room -> storage cap (MB), maintained with room_handles since both come from the same Room snapshot.
    let mut room_caps: HashMap<String, u32> = HashMap::new();
    // Room name, only for session titles; runtime-created rooms are added via RoomUpdated, like handles.
    let mut room_names: HashMap<String, String> = HashMap::new();
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("Cannot connect to {url}"))?;
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
            .context("Timed out waiting for hello_ok")?
            .context("Connection dropped during handshake")?;
        let WsMessage::Text(t) = msg? else { continue };
        let frame: Frame = serde_json::from_str(&t).context("Server sent an unparsable frame")?;
        match frame.body {
            Body::HelloOk(ok) => {
                room_handles.extend(ok.rooms.iter().filter_map(|r| {
                    r.agents
                        .iter()
                        .find(|a| a.owner_id == profile.owner_id && a.id == profile.agent_id)
                        .map(|a| (r.id.clone(), a.handle.clone()))
                }));
                room_caps.extend(ok.rooms.iter().map(|r| (r.id.clone(), r.max_storage_mb)));
                room_names.extend(ok.rooms.iter().map(|r| (r.id.clone(), r.name.clone())));
                println!(
                    "Connected, {} rooms. Waiting for @{} instructions...",
                    ok.rooms.len(),
                    profile.agent_handle
                );
                emit(RemoteEvent::Status(format!(
                    "Connected · {} rooms · @{}",
                    ok.rooms.len(),
                    profile.agent_handle
                )));
                break;
            }
            Body::Error(e) => bail!("Server rejected: {:?} {}", e.code, e.message),
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
                // Check reclaim-control every second: a connection can last hours, so checking only on reconnect is not enough.
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
                                    println!("Stop request received: {}", c.task_id);
                                } else {
                                    eprintln!("Stop request expired: {}", c.task_id);
                                }
                            }
                            Body::ApprovalDecide(d) => {
                                if approvals.resolve(&d.approval_id, d.allow, d.for_task) {
                                    println!(
                                        "Approval {} → {}",
                                        d.approval_id,
                                        if d.allow { "allowed" } else { "denied" }
                                    );
                                }
                            }
                            Body::CallDecide(d) => {
                                let remember =
                                    d.remember_secs.map(|s| Duration::from_secs(s as u64));
                                if calls.resolve(&d.call_id, d.allow, remember) {
                                    println!(
                                        "Authorization {}: {}",
                                        d.call_id,
                                        if d.allow { "allowed" } else { "denied" }
                                    );
                                } else {
                                    eprintln!("Authorization decision expired: {}", d.call_id);
                                }
                            }
                            Body::MsgNew(MsgNew { msg }) => {
                                let msg = match decrypt_incoming(keys, msg) {
                                    Ok(m) => m,
                                    Err(reason) => {
                                        eprintln!("Skipping a message: {reason}");
                                        continue;
                                    }
                                };
                                // Match using the room-effective name; RoomUpdated adds new rooms; only a miss falls back to the global one.
                                let handle = room_handles
                                    .get(&msg.room)
                                    .map(String::as_str)
                                    .unwrap_or(&profile.agent_handle);
                                let Some(mention) = mention_of(&msg, handle) else {
                                    continue;
                                };

                                println!("Instruction received ({}): {}", msg.room, mention.prompt);
                                let from_owner = msg.sender_id == profile.owner_id;
                                let job = Job {
                                    room: msg.room.clone(),
                                    room_name: room_names.get(&msg.room).cloned().unwrap_or_default(),
                                    msg_id: msg.id.clone(),
                                    prompt: mention.prompt,
                                    from_owner,
                                    media: mention.media,
                                    // A quote only records its target; the worker fetches, since HTTP in the WS loop stalls the heartbeat.
                                    quote: msg.reply_to.clone(),
                                    cap_mb: room_caps
                                        .get(&msg.room)
                                        .copied()
                                        .unwrap_or(DEFAULT_ROOM_STORAGE_MB),
                                    last_seq: msg.seq,
                                };

                                // Intercept slash commands first: they are config ops the worker rejects anyway, so approving is wasted.
                                if !from_owner && job.prompt.trim_start().starts_with('/') {
                                    send_text(out_tx, &msg.room, "Only the device owner can run config commands");
                                    continue;
                                }

                                if from_owner || profile.call_policy == CallPolicy::Open {
                                    let _ = jobs.send(job);
                                } else if profile.call_policy == CallPolicy::Owner {
                                    send_text(
                                        out_tx,
                                        &msg.room,
                                        &format!("{}, I only take orders from my owner.", msg.sender_name),
                                    );
                                } else if calls.is_remembered(&msg.room, &msg.sender_id) {
                                    // This person was already allowed within the hour.
                                    let _ = jobs.send(job);
                                } else {
                                    request_call(out_tx, jobs, calls, profile, &msg, handle, job);
                                }
                            }
                            Body::RoomUpdated(u) => {
                                // The owner just changed the cap, so the next message clears the inbox by the new number.
                                room_caps.insert(u.room.id.clone(), u.room.max_storage_mb);
                                // The room name may have just changed, or this is a newly created room.
                                room_names.insert(u.room.id.clone(), u.room.name.clone());
                                // My alias in the room may have changed, or I was just added to a new room.
                                match u.room.agents.iter().find(|a| {
                                    a.owner_id == profile.owner_id && a.id == profile.agent_id
                                }) {
                                    Some(a) => {
                                        let is_new = room_handles
                                            .insert(u.room.id.clone(), a.handle.clone())
                                            .is_none();
                                        if is_new {
                                            println!(
                                                "New room '{}', my name there is @{}",
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
                                println!("Room {} is no longer available to me ({:?})", r.room, r.reason);
                            }
                            Body::Error(e) => {
                                eprintln!("Server error: {:?} {}", e.code, e.message);
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

/// Strip all @handles so only this machine's task remains; a message can name several agents at once.
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

/// Whether this message asks this machine to work, and if so what to do.
struct Mention {
    /// What remains after stripping all @handles: the actual instruction for this machine.
    prompt: String,
    /// Images carried by this message itself (media); those inside a quote belong to another message.
    media: Vec<aoxchat_proto::ArtifactView>,
}

/// Only addressed human messages count; no agent-to-agent (loops) or self-obey; media can also mention.
fn mention_of(msg: &aoxchat_proto::Message, handle: &str) -> Option<Mention> {
    if msg.sender_kind != aoxchat_proto::SenderKind::User {
        return None;
    }
    let (text, media) = match &msg.body {
        MessageBody::Text { text } => (text.as_str(), Vec::new()),
        MessageBody::Media { text, artifacts } => (text.as_str(), artifacts.clone()),
        _ => return None,
    };
    let (mentions, rest) = split_mentions(text);
    if !mentions.iter().any(|m| m == handle) {
        return None;
    }
    // A mention with no text is no instruction; images alone are the same (an image is the object, not the verb).
    if rest.is_empty() {
        None
    } else {
        Some(Mention { prompt: rest, media })
    }
}

/// Truncate by characters, not bytes, so multibyte text is never split in half.
fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// Send an approval card, then wait in a tokio::spawn task; awaiting in the WS read loop would stall it.
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

    // Make it visible and clickable in a terminal too: GUI/phone show a card, a CLI user has only this line.
    let short: String = id.chars().take(6).collect();
    println!("\n[auth] {} wants to call @{}", msg.sender_name, handle);
    println!("       content: {}", truncate(&job.prompt, 60));
    println!("       allow {short} | allow1h {short} (this hour) | deny {short}");

    let out2 = out.clone();
    let jobs2 = jobs.clone();
    let room = msg.room.clone();
    let gate = calls.clone();
    tokio::spawn(async move {
        match tokio::time::timeout(calls::TIMEOUT, rx).await {
            Ok(Ok(true)) => {
                // Also report local approval to the server so the phone's pending call card clears instead of timing out.
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
                // Enqueue the job only after the owner has actually approved it.
                let _ = jobs2.send(job);
            }
            Ok(Ok(false)) => send_text(&out2, &room, "The owner did not approve this call."),
            Ok(Err(_)) => send_text(&out2, &room, "The authorization request has expired."),
            Err(_) => send_text(&out2, &room, "No response after two minutes, cancelled."),
        }
        gate.forget(&id);
    });
}

/// CLI approval loop over stdin; started only with a real terminal so a blocking read never stalls authorization.
fn spawn_stdin_control(calls: Arc<CallGate>) {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return;
    }
    println!("(When someone wants to call this machine, an [auth] line is printed; enter allow / allow1h / deny + the first 6 chars of the id)\n");
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
                Ok(id) => println!("  handled {id}"),
                Err(calls::PrefixErr::Ambiguous) => {
                    println!("  prefix {prefix} is ambiguous, enter more characters")
                }
                Err(calls::PrefixErr::NotFound) => {
                    println!("  no such pending request (it may have timed out)")
                }
            }
        }
    });
}

/// Tell the server whether this machine is busy so it can push presence to all relevant rooms.
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

/// Clear the busy flag on turn end via Drop, covering done, error, cancel, and panic paths.
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

/// Stage attachments in a per-room inbox; only images go to the model, others are given by path to avoid 400s.
async fn download_media(
    http: &reqwest::Client,
    profile: &RemoteProfile,
    root: &std::path::Path,
    room: &str,
    items: &[aoxchat_proto::ArtifactView],
    cap_mb: u32,
) -> (Vec<std::path::PathBuf>, Vec<Loose>) {
    let dir = root.join(".aox").join("remote").join("inbox").join(room);
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return (Vec::new(), Vec::new());
    }

    let mut out = Vec::new();
    let mut loose = Vec::new();
    // Re-check the attachment count locally as a backstop to the server's cap, so a flood cannot stall the machine.
    for a in items.iter().take(9) {
        let url = format!("{}{}", profile.server.trim_end_matches('/'), a.url);
        let resp = match http.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                eprintln!("Failed to fetch '{}': HTTP {}", a.name, r.status());
                continue;
            }
            Err(e) => {
                eprintln!("Failed to fetch '{}': {e}", a.name);
                continue;
            }
        };
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Failed to read '{}': {e}", a.name);
                continue;
            }
        };
        let path = dir.join(format!("{}_{}", short_id(&a.id), safe_file_name(&a.name)));
        if let Err(e) = tokio::fs::write(&path, &bytes).await {
            eprintln!("Failed to write '{}': {e}", a.name);
            continue;
        }
        if !a.mime.starts_with("image/") {
            loose.push(Loose { name: a.name.clone(), path, size: bytes.len() as u64 });
            continue;
        }
        match Attachment::probe(&path) {
            // Pre-probe attachments so failures log a filename; parsing still happens in run_turn.
            Ok(_) => out.push(path),
            // Oversized images are saved to disk instead of dropped, so the model still knows the image exists and where.
            Err(reason) => {
                eprintln!("'{}' cannot be used as an attachment, saved to disk only: {reason}", a.name);
                loose.push(Loose { name: a.name.clone(), path, size: bytes.len() as u64 });
            }
        }
    }

    prune_inbox(&dir, cap_mb).await;
    (out, loose)
}

/// A file saved to disk but not attached to the model; its path is written into the prompt.
struct Loose {
    name: String,
    path: std::path::PathBuf,
    size: u64,
}

/// Prompt block listing disk-only files: note they were sent by others and give paths, without ordering a read.
fn media_block(files: &[Loose]) -> String {
    let mut s = String::from("\n\n[Files sent with the message]\n");
    s.push_str("Downloaded to this machine, paths can be used directly:\n");
    for f in files {
        s.push_str(&format!(
            "  {} ({}, {})\n",
            f.path.display(),
            context::size_text(f.size),
            f.name
        ));
    }
    s
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Keep only safe characters in filenames as a second pass, so .. cannot escape the inbox directory.
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

/// Delete oldest inbox files by mtime at the room's size cap; only local copies go, server data stays.
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

/// A room's own Runtime and Agent, swapped by /cwd; kept per room, not passed as a worker argument.
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
    mut profile: RemoteProfile,
    root: PathBuf,
    plugins_dir: PathBuf,
    room_now: tool::Slot,
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
            // One conversation per group: resume the registered session if its file still exists, else start a new one.
            let agent = room_agent(&base, &profile, &job.room, &job.room_name);
            rooms.insert(job.room.clone(), RoomCtx { rt: base.clone(), agent });
        }

        // Mark the machine busy so other groups see it; a guard clears it on scope exit even on cancel or error.
        send_agent_busy(&out, &profile.agent_id, &job.room, true);
        let _busy = BusyGuard {
            out: out.clone(),
            agent_id: profile.agent_id.clone(),
            room: job.room.clone(),
        };

        // Take the room context out of the map for run_turn (&mut); the borrow cannot cross an await.
        let mut ctx = rooms.remove(&job.room).expect("just inserted");

        // Save images locally in the worker, not the WS loop, so downloads never block heartbeat or cancellation.
        let meta = TaskMeta {
            agent_id: profile.agent_id.clone(),
            agent_handle: profile.agent_handle.clone(),
            owner_id: profile.owner_id.clone(),
            prompt: job.prompt.clone(),
            msg_id: job.msg_id.clone(),
        };
        // Build the UI before fetching so the phone shows progress during a long download, not only once it finishes.
        let mut ui = RemoteUi::new(out.clone(), job.room.clone(), approvals.clone(), meta);

        let (mut attachments, loose) = if job.media.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            download_media(&http, &profile, &root, &job.room, &job.media, job.cap_mb).await
        };

        // Fetch quoted content with one HTTP request, then download its files by type.
        let mut prompt = job.prompt.clone();
        // Append disk-only files at the end of the prompt so the model reads the instruction before the file list.
        if !loose.is_empty() {
            prompt.push_str(&media_block(&loose));
        }
        if let Some(reply) = &job.quote {
            ui.note("Fetching quoted content...");
            // Executables must sit in the workspace (only in-zone writes/runs); compute it separately from app root.
            let ws = ctx
                .rt
                .config
                .workplace
                .root
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| ctx.rt.root.clone());
            let quote = context::resolve(
                &http,
                &profile,
                &keys,
                &job.room,
                reply,
                &root.join(".aox").join("remote").join("inbox"),
                &ws.join(".aox").join("remote").join("files"),
            )
            .await;
            for n in &quote.notes {
                ui.note(n.clone());
            }
            // Say so in the room when nothing was fetched, otherwise silence and not-seen look identical to the user.
            if quote.is_empty() {
                let why = quote
                    .notes
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "Nothing to fetch from the quote".to_string());
                let text = format!("Could not fetch quoted content: {why}");
                send_text(&out, &job.room, &outbound(&keys, &job.room, &text));
            }
            ui.set_digest(context::digest(&quote));
            attachments.extend(quote.attachments());
            // Prepend the quote block before the instruction so the model first learns what it is, then what to do.
            prompt.insert_str(0, &context::prompt_block(&quote));
        }

        if !attachments.is_empty() {
            println!("{} attachment(s)", attachments.len());
        }

        let started = std::time::Instant::now();
        println!("Starting...");
        // Cancellation drops this future and closes the recovery turn, the same path as the GUI stop button.
        let mut cancel_rx = cancels.register(ui.task_id());
        // Set room_history for this turn and clear it afterward, since local GUI and remote worker share one Runtime.
        *room_now.write().await =
            Some(tool::Current::new(job.room.clone(), job.last_seq));
        let outcome = tokio::select! {
            r = ctx.agent.run_turn(&mut ui, &prompt, &attachments) => Some(r),
            _ = &mut cancel_rx => None,
        };
        *room_now.write().await = None;
        match outcome {
            None => {
                let _ = ctx.rt.recovery.abort_turn();
                println!("Stopped ({:.1}s): the phone tapped stop", started.elapsed().as_secs_f32());
                send_text(&out, &job.room, &outbound(&keys, &job.room, "Stopped."));
                let task = ui.snapshot(aoxchat_proto::TaskStatus::Cancelled, None);
                if let Err(e) = upload_task(&profile, &job.room, &task).await {
                    println!("  ↑ failed to write task record: {e:#}");
                }
            }
            Some(Ok(())) => {
                let text = ui.take_final_text();
                println!("Finished ({:.1}s), replied with {} chars", started.elapsed().as_secs_f32(), text.chars().count());
                if !text.trim().is_empty() {
                    send_text(&out, &job.room, &outbound(&keys, &job.room, &text));
                }
                // After the reply, upload staged artifacts as one grouped message per turn, then write a snapshot listing them.
                let files = ui.take_artifacts();
                let mut views = Vec::new();
                for file in files {
                    match defer_artifact(&profile, &job.room, &file.name, &file.path).await {
                        Ok(view) => {
                            println!("  ↑ uploaded artifact {}", view.name);
                            views.push(view);
                        }
                        Err(e) => {
                            println!("  ↑ artifact upload failed {}: {e:#}", file.path.display());
                            // Report failed uploads to the phone (no host stdout visible), correcting a false 'sent to room' claim.
                            let msg = format!("Artifact '{}' was not uploaded: {e}", file.name);
                            send_text(&out, &job.room, &outbound(&keys, &job.room, &msg));
                        }
                    }
                }
                ui.set_artifacts(views.clone());
                let task = ui.snapshot(aoxchat_proto::TaskStatus::Done, None);
                if let Err(e) = upload_task(&profile, &job.room, &task).await {
                    println!("  ↑ failed to write task record: {e:#}");
                }
                for chunk in views.chunks(9) {
                    send_media(&out, &job.room, "", chunk.to_vec());
                }
            }
            Some(Err(e)) => {
                let msg = format!("{e:#}");
                println!("Execution failed ({:.1}s): {msg}", started.elapsed().as_secs_f32());
                send_text(&out, &job.room, &outbound(&keys, &job.room, &format!("Execution failed: {msg}")));
                let task = ui.snapshot(aoxchat_proto::TaskStatus::Failed, Some(msg));
                if let Err(e) = upload_task(&profile, &job.room, &task).await {
                    println!("  ↑ failed to write task record: {e:#}");
                }
            }
        }

        cancels.finish(ui.task_id());

        // Persist the group-to-session id for restart resume, writing only on change to avoid per-turn rewrites.
        let conv_id = ctx.agent.history_id().map(str::to_string);
        if let Some(id) = conv_id {
            let known = profile.rooms.get(&job.room).map(|r| r.conversation.as_str());
            if known != Some(id.as_str()) {
                // Write the display title (Remote plus group name) only once, on the first turn of a session.
                let name = room_title(&job.room_name);
                let _ = ctx.agent.rename_conversation(&id, Some(&name));
                profile.rooms.insert(
                    job.room.clone(),
                    crate::remote::store::RoomConv {
                        conversation: id.clone(),
                        name: job.room_name.clone(),
                        updated_at: now_ms(),
                    },
                );
                if let Err(e) = remember_room(&root, &profile.id, &job.room, &profile.rooms[&job.room])
                {
                    println!("  ↑ failed to register room session: {e:#}");
                }
            }
        }
        rooms.insert(job.room.clone(), ctx);
    }
}



/// Create a room Agent with a remote birth marker and resume its session; all call sites must use this helper.
fn room_agent(
    base: &Arc<Runtime>,
    profile: &RemoteProfile,
    room: &str,
    room_name: &str,
) -> Agent {
    let mut agent = Agent::new(base.clone());
    agent.set_birth(crate::store::Birth::remote(room, room_name));
    if let Some(rc) = profile.rooms.get(room) {
        if agent.resume(&rc.conversation).is_err() {
            // If the session file is gone (cleared on phone or another machine), start a new segment instead of stalling.
            agent.new_conversation();
        }
    }
    agent
}

/// Display name for a group in the session list, falling back to just Remote when no room name exists.
fn room_title(room_name: &str) -> String {
    let tag = crate::i18n::t("Remote");
    if room_name.trim().is_empty() {
        tag.to_string()
    } else {
        format!("{tag} · {room_name}")
    }
}

/// Save the group-to-session mapping to remote.json via load-modify-save, since Book is its only structured view.
fn remember_room(
    root: &Path,
    profile_id: &str,
    room: &str,
    rc: &crate::remote::store::RoomConv,
) -> Result<()> {
    let mut book = Book::load(root)?;
    if let Some(p) = book.pairings.iter_mut().find(|p| p.id == profile_id) {
        p.rooms.insert(room.to_string(), rc.clone());
        book.save(root)?;
    }
    Ok(())
}

/// Slash commands to change model/workspace; only the device owner may run them, not group collaborators.
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
        send_text(out, &job.room, "Only the device owner can run config commands");
        return;
    }

    match cmd {
        "/model" => {
            let mut book = crate::modelbook::Book::load(root);
            if arg.is_empty() {
                let active = book.active.clone().unwrap_or_default();
                let cur = book
                    .active_record()
                    .map(|r| format!("{} ({})", r.model, r.id))
                    .unwrap_or_else(|| "not set".into());
                let mut text = format!("Current model: {cur}");
                if !book.models.is_empty() {
                    text.push_str("\nAvailable:");
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
                    send_text(out, &job.room, &format!("Save failed: {e:#}"));
                    return;
                }
                if let Some(cfg) = book.active_config() {
                    let model = cfg.model.clone();
                    let _ = base.provider.apply(cfg.clone());
                    for ctx in rooms.values() {
                        let _ = ctx.rt.provider.apply(cfg.clone());
                    }
                    send_text(out, &job.room, &format!("Switched to {model} ({arg})"));
                }
            } else {
                send_text(out, &job.room, &format!("No such model: {arg}"));
            }
        }
        "/key" => {
            if arg.is_empty() || arg.eq_ignore_ascii_case("off") {
                match keys.clear(&job.room) {
                    Ok(true) => send_text(out, &job.room, "Encryption is off for this room (past ciphertext can no longer be decrypted)."),
                    Ok(false) => send_text(out, &job.room, "No passphrase was set for this room anyway."),
                    Err(e) => send_text(out, &job.room, &format!("Clear failed: {e:#}")),
                }
            } else {
                match keys.set(&job.room, arg) {
                    Ok(()) => send_text(
                        out,
                        &job.room,
                        "Encryption is on for this room.\n⚠ The phone has no passphrase input yet: after enabling, the phone will only see ciphertext starting with enc:v1:.\nThe passphrase never goes online; if you forget it, I cannot decrypt it either.",
                    ),
                    Err(e) => send_text(out, &job.room, &format!("Save failed: {e:#}")),
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
                    &format!("Current workspace: {cur}\nUsage: /cwd D:\\work\\proj"),
                );
                return;
            }
            let path = PathBuf::from(arg);
            if !path.is_dir() {
                send_text(out, &job.room, &format!("Directory does not exist: {}", path.display()));
                return;
            }
            let mut cfg = base.config.clone();
            cfg.workplace.mode = "fixed".into();
            cfg.workplace.root = Some(path.to_string_lossy().to_string());
            match crate::bootstrap::build(cfg, root.to_path_buf(), plugins_dir.to_path_buf()).await {
                Ok(new_rt) => {
                    // Changing the working directory does not change the topic: keep talking in the group's existing session.
                    let agent = room_agent(&new_rt, profile, &job.room, &job.room_name);
                    rooms.insert(job.room.clone(), RoomCtx { rt: new_rt, agent });
                    send_text(
                        out,
                        &job.room,
                        &format!(
                            "Workspace switched to {}\nThis room's next turn starts from the new directory (model keeps the current setting)",
                            path.display()
                        ),
                    );
                }
                Err(e) => send_text(out, &job.room, &format!("Switch failed: {e:#}")),
            }
        }
        "/new" => {
            // On a full or changed segment, start fresh on the phone and drop its entry so restarts do not resume it.
            rooms.remove(&job.room);
            let mut book = match Book::load(root) {
                Ok(b) => b,
                Err(e) => {
                    send_text(out, &job.room, &format!("Failed to read session registry: {e:#}"));
                    return;
                }
            };
            let mut cleared = false;
            if let Some(p) = book.pairings.iter_mut().find(|p| p.id == profile.id) {
                cleared = p.rooms.remove(&job.room).is_some();
                if let Err(e) = book.save(root) {
                    send_text(out, &job.room, &format!("Save failed: {e:#}"));
                    return;
                }
            }
            send_text(
                out,
                &job.room,
                if cleared {
                    "OK, the next message starts a new segment (the previous one is still in local history)"
                } else {
                    "OK, the next message starts a new segment"
                },
            );
        }
        _ => send_text(
            out,
            &job.room,
            "Available commands:\n/model [id]  view or switch model\n/cwd [path]   view or switch workspace\n/new          start a new segment with the next message",
        ),
    }
    let _ = profile; // 目前命令只用得到 owner 判定，保留参数便于后续加 /status
}

/// Outbound: encrypt text if the room has a passphrase, else send plain; a transform the server ignores.
fn outbound(keys: &RoomKeys, room: &str, text: &str) -> String {
    match keys.get(room) {
        Some(k) => crypto::encrypt(&k, text),
        None => text.to_string(),
    }
}

/// Inbound: process only decryptable text, skip the rest with a reason; decrypt media captions, not just Text.
fn decrypt_incoming(
    keys: &RoomKeys,
    mut msg: aoxchat_proto::Message,
) -> std::result::Result<aoxchat_proto::Message, String> {
    match &msg.body {
        MessageBody::Text { text } => {
            let plain = open_text(keys, &msg.room, text)?;
            msg.body = MessageBody::Text { text: plain };
        }
        MessageBody::Media { text, artifacts } => {
            let plain = open_text(keys, &msg.room, text)?;
            msg.body = MessageBody::Media { text: plain, artifacts: artifacts.clone() };
        }
        _ => {}
    }
    Ok(msg)
}

/// Decrypt one text body; return plaintext unchanged since only some members may have set the passphrase.
fn open_text(keys: &RoomKeys, room: &str, text: &str) -> std::result::Result<String, String> {
    if !crypto::is_encrypted(text) {
        return Ok(text.to_string());
    }
    let Some(key) = keys.get(room) else {
        return Err("Received ciphertext, but no passphrase is set on this machine (send /key <passphrase> first)".to_string());
    };
    crypto::decrypt(&key, text).ok_or_else(|| "Wrong passphrase, cannot decrypt this message".to_string())
}

fn send_text(out: &mpsc::UnboundedSender<Frame>, room: &str, text: &str) {
    let frame = Frame::in_room(
        Frame::new(
            frame_id("h"),
            now_ms(),
            Body::MsgSend(MsgSend {
                client_msg_id: frame_id("c"),
                body: MessageBody::Text { text: text.to_string() },
                // Host messages quote nobody; reply_to is None to follow the protocol's Option and skip_serializing_if.
                reply_to: None,
                reply_artifact_ids: Vec::new(),
            }),
        ),
        room,
    );
    let _ = out.send(frame);
}

/// Match the server's artifact size cap locally to reject oversized files before a wasted upload.
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

/// A task snapshot written at turn end into history, so reopening the app still shows what the turn did.
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

/// Upload a file without posting, returning just its artifact view, so a turn's files share one grouped message.
async fn defer_artifact(
    profile: &RemoteProfile,
    room: &str,
    name: &str,
    path: &std::path::Path,
) -> Result<aoxchat_proto::ArtifactView> {
    let meta = tokio::fs::metadata(path).await?;
    if meta.len() > MAX_UPLOAD {
        anyhow::bail!("Exceeds {} MB", MAX_UPLOAD / 1024 / 1024);
    }
    let mut url = reqwest::Url::parse(&format!(
        "{}/api/rooms/{room}/artifacts",
        profile.server.trim_end_matches('/')
    ))?;
    // Use Url query_pairs_mut instead of manual string building so non-ASCII filenames are encoded correctly.
    url.query_pairs_mut()
        .append_pair("name", name)
        .append_pair("mime", mime_of(name))
        // Send the defer flag as true, its most direct form even though the server accepts the string too.
        .append_pair("defer", "true");

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
    Ok(serde_json::from_str(&text)?)
}

/// Send an attachment message; the server only needs the id and fills the rest from its database.
fn send_media(
    out: &mpsc::UnboundedSender<Frame>,
    room: &str,
    text: &str,
    artifacts: Vec<aoxchat_proto::ArtifactView>,
) {
    let frame = Frame::in_room(
        Frame::new(
            frame_id("h"),
            now_ms(),
            Body::MsgSend(MsgSend {
                client_msg_id: frame_id("c"),
                body: MessageBody::Media {
                    text: text.to_string(),
                    artifacts,
                },
                reply_to: None,
                reply_artifact_ids: Vec::new(),
            }),
        ),
        room,
    );
    let _ = out.send(frame);
}

#[cfg(test)]
mod tests {
    use super::*;
    use aoxchat_proto::{ArtifactView, Message, SenderKind};

    fn msg(kind: SenderKind, body: MessageBody) -> Message {
        Message {
            id: "m_1".into(),
            room: "r_1".into(),
            seq: 1,
            sender_id: "u_1".into(),
            sender_kind: kind,
            sender_name: "B".into(),
            sender_avatar: None,
            created_at: 1,
            reply_to: None,
            body,
        }
    }

    fn art() -> ArtifactView {
        ArtifactView {
            id: "a1".into(),
            name: "shot.png".into(),
            size: 10,
            mime: "image/png".into(),
            sha256: String::new(),
            url: "/api/artifacts/a1?k=t".into(),
            origin: "user".into(),
            width: 0,
            height: 0,
        }
    }

    /// Media messages can @-mention an Agent too; only Text was checked before, so the media mention never fired.
    #[test]
    fn a_photo_with_a_mention_still_reaches_the_agent() {
        let m = msg(
            SenderKind::User,
            MessageBody::Media { text: "@aox look at this picture".into(), artifacts: vec![art()] },
        );
        let got = mention_of(&m, "aox").expect("a message with an image should also be able to mention");
        assert_eq!(got.prompt, "look at this picture");
        assert_eq!(got.media.len(), 1);
    }

    /// A textless mention, even with an image, is no instruction: the image is the object, a person gives the verb.
    #[test]
    fn a_bare_mention_is_not_an_instruction() {
        assert!(mention_of(&msg(SenderKind::User, MessageBody::Text { text: "@aox".into() }), "aox")
            .is_none());
        assert!(mention_of(
            &msg(SenderKind::User, MessageBody::Media { text: "@aox ".into(), artifacts: vec![art()] }),
            "aox"
        )
        .is_none());
    }

    /// Ensure disk-only files appear in the prompt; silent probe failures made large files look like no response.
    #[test]
    fn files_that_are_only_saved_on_disk_still_reach_the_prompt() {
        let block = media_block(&[Loose {
            name: "build.zip".into(),
            path: std::path::PathBuf::from("/tmp/inbox/build.zip"),
            size: 2048,
        }]);
        assert!(block.contains("/tmp/inbox/build.zip"), "{block}");
        assert!(block.contains("build.zip"), "{block}");
        // Include the file size in the listing so the model can judge whether to read the whole thing.
        assert!(block.contains("2 KB"), "{block}");
    }

    /// Ignore messages from other Agents (and our own) to avoid feedback loops.
    #[test]
    fn agents_never_obey_agents() {
        let m = msg(SenderKind::Agent, MessageBody::Text { text: "@aox go change it".into() });
        assert!(mention_of(&m, "aox").is_none());
    }

    /// Strip every @ mention when one message names multiple Agents, leaving only the instruction for this machine.
    #[test]
    fn every_mention_is_stripped_from_the_prompt() {
        let m = msg(SenderKind::User, MessageBody::Text { text: "@aox @other take a look together".into() });
        assert_eq!(mention_of(&m, "aox").unwrap().prompt, "take a look together");
        // If the message does not mention me, it is not my job.
        assert!(mention_of(&m, "aox-b").is_none());
    }

    /// The /remote screen shows running state, then config status; a missing remote.json is first-run, not an error.
    #[test]
    fn brief_survives_a_missing_booking_file() {
        let dir = std::env::temp_dir().join(format!("aox-remote-brief-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let idle = brief(&dir, false);
        assert!(idle.contains("not running"), "{idle}");
        assert!(idle.contains("No pairing yet"), "{idle}");
        assert!(idle.contains("/remote pair"), "Should tell the user how to start: {idle}");

        let running = brief(&dir, true);
        assert!(running.contains("running"), "{running}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
