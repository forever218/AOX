//! GUI ↔ Agent 桥接层。
//!
//! 设计要点：**完全复用既有架构**。
//! - 后台线程跑一个 tokio 运行时，用 `aox::agent::Agent` 原封不动的循环驱动任务；
//! - `GuiUi` 实现 `aox::ui::Ui` trait，把 `AgentEvent`、状态、流式思考过程转发给 egui 主线程；
//! - 用户输入、授权决定、中断信号通过 channel 反向传回。
//!
//! 因此 CLI 与 GUI 共享同一套 Agent / Provider / 插件宿主实现，没有任何逻辑分叉。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use aox::agent::Agent;
use aox::llm::{LlmProvider, StreamSink};
use aox::runtime::Runtime;
use aox::ui::{AgentEvent, AuthDecision, Ui};

/// 主线程 → Agent 线程
pub enum ToAgent {
    /// 一条用户指令
    Input(String),
    /// 退出：关闭插件子进程并结束驱动线程
    Shutdown,
}

/// Agent 线程 → 主线程
pub enum FromAgent {
    /// 启动信息（配置 / 模型 / 路径 / 是否可用）
    Boot(Box<BootInfo>),
    /// 核心事件流（与 CLI 完全同源）
    Event(AgentEvent),
    /// 一轮任务开始
    TurnStart,
    /// 一轮任务结束（正常）
    TurnEnd,
    /// 一轮任务被中断
    TurnAborted,
    /// 状态栏文字（思考中 / 执行 xx）
    Status(String),
    /// 流式思考增量
    Reasoning(String),
    /// 一次响应的思考流结束（即时消失）
    ReasoningEnd,
    /// 请求授权（阻塞等待主线程回填）
    AuthRequest { tool: String, args: String, reply: oneshot::Sender<AuthDecision> },
    /// 工具 / 插件快照（周期性刷新，用于右侧面板实时展示「自我增长」）
    Snapshot(Snapshot),
    /// 本轮出现不可恢复错误
    Fatal(String),
}

#[derive(Debug, Clone)]
pub struct BootInfo {
    pub config_path: String,
    pub model: String,
    pub provider_ready: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub plugins: Vec<String>,
}

/// 跨线程的中断标志。用轮询而不是 Notify，避免「通知早于等待」导致的陈旧许可问题。
pub struct SessionFlags {
    abort: AtomicBool,
}

impl SessionFlags {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { abort: AtomicBool::new(false) })
    }

    pub fn request_abort(&self) {
        self.abort.store(true, Ordering::SeqCst);
    }

    fn clear(&self) {
        self.abort.store(false, Ordering::SeqCst);
    }
}

async fn wait_abort(flags: Arc<SessionFlags>) {
    loop {
        if flags.abort.swap(false, Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 把 Provider 的流式思考过程转发给 GUI。
struct ChanSink {
    tx: mpsc::UnboundedSender<FromAgent>,
}

impl StreamSink for ChanSink {
    fn on_reasoning(&self, delta: &str) {
        let _ = self.tx.send(FromAgent::Reasoning(delta.to_string()));
    }

    fn on_finish(&self) {
        let _ = self.tx.send(FromAgent::ReasoningEnd);
    }
}

/// `aox::ui::Ui` 的图形实现。
pub struct GuiUi {
    tx: mpsc::UnboundedSender<FromAgent>,
    rx: mpsc::UnboundedReceiver<ToAgent>,
}

#[async_trait]
impl Ui for GuiUi {
    async fn read_user_input(&mut self) -> Result<Option<String>> {
        loop {
            match self.rx.recv().await {
                Some(ToAgent::Input(s)) => return Ok(Some(s)),
                Some(ToAgent::Shutdown) | None => return Ok(None),
            }
        }
    }

    fn emit(&mut self, event: AgentEvent) {
        let _ = self.tx.send(FromAgent::Event(event));
    }

    async fn authorize(&mut self, tool_name: &str, args: &Value) -> AuthDecision {
        let (reply, rx) = oneshot::channel();
        let pretty = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
        let sent = self.tx.send(FromAgent::AuthRequest {
            tool: tool_name.to_string(),
            args: pretty,
            reply,
        });
        if sent.is_err() {
            return AuthDecision::Deny;
        }
        rx.await.unwrap_or(AuthDecision::Deny)
    }

    async fn turn_start(&mut self) {
        let _ = self.tx.send(FromAgent::TurnStart);
    }

    async fn turn_end(&mut self) {
        let _ = self.tx.send(FromAgent::TurnEnd);
    }

    async fn set_status(&mut self, label: &str) {
        let _ = self.tx.send(FromAgent::Status(label.to_string()));
    }

    fn stream_sink(&self) -> Option<Box<dyn StreamSink + Send + Sync + '_>> {
        Some(Box::new(ChanSink { tx: self.tx.clone() }))
    }
}

fn boot(
    tx: &mpsc::UnboundedSender<FromAgent>,
    config_path: &std::path::Path,
    model: &str,
    ready: bool,
    note: Option<String>,
) {
    let _ = tx.send(FromAgent::Boot(Box::new(BootInfo {
        config_path: config_path.display().to_string(),
        model: model.to_string(),
        provider_ready: ready,
        note,
    })));
}

/// 在独立线程里启动：建 tokio 运行时 → 装载运行时 → 跑驱动循环。
pub fn spawn(
    tx: mpsc::UnboundedSender<FromAgent>,
    rx: mpsc::UnboundedReceiver<ToAgent>,
    flags: Arc<SessionFlags>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("aox-agent".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(FromAgent::Fatal(format!("无法创建异步运行时: {e}")));
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(e) = boot_and_run(tx.clone(), rx, flags).await {
                    let _ = tx.send(FromAgent::Fatal(format!("{e:#}")));
                }
            });
        })
        .expect("无法启动 agent 线程")
}

async fn boot_and_run(
    tx: mpsc::UnboundedSender<FromAgent>,
    mut rx: mpsc::UnboundedReceiver<ToAgent>,
    flags: Arc<SessionFlags>,
) -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (root, default_plugins) = aox::paths::resolve_workspace();
    let plugins_dir = aox::paths::arg_value(&args, "--plugins")
        .map(PathBuf::from)
        .unwrap_or(default_plugins);
    let config_path = aox::paths::arg_value(&args, "--config")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("config.toml"));

    if let Err(e) = aox::plugin::ensure_sdk(&plugins_dir) {
        let _ = tx.send(FromAgent::Event(AgentEvent::Warn(format!(
            "插件 SDK 写入失败（{:#}）：插件功能可能不可用",
            e
        ))));
    }

    let config = match aox::config::Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            // 配置缺失/无效：GUI 照常打开，展示引导信息，输入区会被禁用。
            boot(&tx, &config_path, "", false, Some(format!("{e:#}")));
            loop {
                match rx.recv().await {
                    Some(ToAgent::Shutdown) | None => return Ok(()),
                    _ => {}
                }
            }
        }
    };

    let model = config.provider.model.clone();
    let rt = Runtime::new(config, root.clone(), plugins_dir.clone());
    for t in aox::tool::meta::meta_tools() {
        rt.register(t).await?;
    }

    let mut ui = GuiUi { tx: tx.clone(), rx };

    // 恢复持久化插件（与 CLI 同一路径）。
    // 成功保持静默——启动时不该往活动流里刷一屏「插件 xxx 已恢复」，
    // 那会把真正要看的信息挤下去。只有失败才出声。
    for (name, result) in aox::plugin::load_all(&rt).await {
        if let Err(e) = result {
            ui.emit(AgentEvent::Warn(format!("插件 {name} 加载失败: {e:#}")));
        }
    }

    let llm: Arc<dyn LlmProvider> = match aox::llm::openai::OpenAiProvider::new(&rt.config.provider) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            ui.emit(AgentEvent::Error(format!("LLM 未就绪: {e:#}")));
            ui.emit(AgentEvent::Info(
                "请在 config.toml 的 [provider] 填写 base_url 与 api_key（或 api_key_env）后重新启动。".into(),
            ));
            boot(&tx, &config_path, &model, false, Some(format!("{e:#}")));
            loop {
                match ui.rx.recv().await {
                    Some(ToAgent::Shutdown) | None => return Ok(()),
                    _ => {}
                }
            }
        }
    };

    let rules = rt.config.agent.rules_file.as_deref().and_then(|p| {
        let path = PathBuf::from(p);
        let path = if path.is_absolute() { path } else { rt.root.join(path) };
        std::fs::read_to_string(path).ok()
    });

    let mut agent = Agent::new(llm, rt.clone(), rules.as_deref());
    boot(&tx, &config_path, &model, true, None);

    // 周期性广播插件快照：模型每造出一个新插件，右侧面板立刻可见。
    // （工具名不再广播——顶栏的工具胶囊已去掉，没有消费方。）
    let snap_tx = tx.clone();
    let snap_rt = rt.clone();
    let snap_task = tokio::spawn(async move {
        loop {
            let plugins = {
                let p = snap_rt.plugins.lock().await;
                let mut v: Vec<String> = p.keys().cloned().collect();
                v.sort();
                v
            };
            if snap_tx.send(FromAgent::Snapshot(Snapshot { plugins })).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
    });

    // ---- 驱动循环：与 CLI 的 REPL 一一对应
    loop {
        let Some(input) = ui.read_user_input().await? else { break };
        let line = input.trim().to_string();
        if line.is_empty() {
            continue;
        }
        flags.clear();
        let mark = agent.messages_len();
        enum Outcome {
            Done(Result<()>),
            Aborted,
        }
        let outcome = {
            let turn = agent.run_turn(&mut ui, &line);
            tokio::pin!(turn);
            tokio::select! {
                r = &mut turn => Outcome::Done(r),
                _ = wait_abort(flags.clone()) => Outcome::Aborted,
            }
        };
        match outcome {
            Outcome::Done(Ok(())) => {}
            Outcome::Done(Err(e)) => {
                let _ = ui.tx.send(FromAgent::Fatal(format!("{e:#}")));
            }
            Outcome::Aborted => {
                agent.truncate_messages(mark);
                let _ = ui.tx.send(FromAgent::TurnAborted);
            }
        }
    }

    snap_task.abort();
    // 关闭所有插件子进程（与 CLI 退出路径一致）
    let handles: Vec<_> = rt.plugins.lock().await.values().cloned().collect();
    for h in handles {
        h.shutdown().await;
    }
    Ok(())
}
