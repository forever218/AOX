//! 插件宿主：每个插件是一个独立子进程，通过 stdio 交换「每行一个 JSON」的 RPC 消息。
//!
//! 协议：
//!   1. 宿主启动插件后发送 {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
//!      插件回复 result: {"name":..., "tools":[{"name","description","parameters"}]}
//!   2. 宿主发送 {"jsonrpc":"2.0","id":N,"method":"tools/call","params":{"name","arguments"}}
//!      插件回复 result: {"content":"...","is_error":false}
//!   3. 宿主可发送通知 {"jsonrpc":"2.0","method":"shutdown"}，插件应自行退出。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::config::RuntimesConfig;
use crate::runtime::Runtime;

const INIT_TIMEOUT: Duration = Duration::from_secs(20);
const RUST_BUILD_TIMEOUT: Duration = Duration::from_secs(180);

/// 抑制子进程在 Windows 上弹出控制台黑窗口。
///
/// 子进程的**创建标志**。
///
/// 两件事共用这一处：
///
/// ① **`CREATE_NO_WINDOW`** —— 插件进程的 stdin/stdout 是管道、stderr 落到
///    `stderr.log`，**从不需要控制台**。但在 GUI 版里宿主编译为 `windows` 子系统
///    （自身没有控制台），此时启动 `python.exe` / `node.exe` / `rustc.exe` 这类
///    控制台程序，Windows 会专门为它分配一个新控制台——用户看到的表现就是
///    「动不动弹出几个黑色窗口」。统一加上这个标志后 CLI 与 GUI 行为一致。
/// ② **围栏的 `CREATE_SUSPENDED`**（`src/sandbox.rs`）—— 让进程先冻住，好让宿主
///    在它跑第一行代码之前把它塞进 Job Object。
///
/// 有**两种** `Command`：插件子进程用 `tokio::process::Command`（异步），
/// 而运行时装配链（certutil / tar / powershell）用 `std::process::Command`（同步）。
/// 两者都实现了 `creation_flags`，所以这里用一个小 trait 统一，
/// 避免在每个调用点各自复制一遍平台细节。
pub(crate) trait ProcessFlags {
    /// 只要求「不弹控制台窗口」。装配链那几个调用点要的就是这个。
    fn hide_console(&mut self) {
        self.process_flags(0);
    }

    /// 设置创建标志：`CREATE_NO_WINDOW` **或上** `extra`。
    ///
    /// ⚠️ 底层是**覆盖**不是追加，所以必须一次给全 —— 这也是为什么 `extra`
    /// 走参数进来、而不是让调用方自己再调一次 `creation_flags`（那样会把
    /// `CREATE_NO_WINDOW` 抹掉，黑色窗口又回来了）。
    fn process_flags(&mut self, extra: u32);
}

/// 不生成控制台窗口（`winbase.h`）。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
impl ProcessFlags for Command {
    fn process_flags(&mut self, extra: u32) {
        self.creation_flags(CREATE_NO_WINDOW | extra);
    }
}

#[cfg(windows)]
impl ProcessFlags for std::process::Command {
    fn process_flags(&mut self, extra: u32) {
        use std::os::windows::process::CommandExt as _;
        self.creation_flags(CREATE_NO_WINDOW | extra);
    }
}

#[cfg(not(windows))]
impl ProcessFlags for Command {
    fn process_flags(&mut self, _extra: u32) {}
}

#[cfg(not(windows))]
impl ProcessFlags for std::process::Command {
    fn process_flags(&mut self, _extra: u32) {}
}

/// 插件除工具之外还贡献什么（阶段 6 · 6b）。
///
/// 全部字段可缺省 —— 老插件的 `plugin.toml` 里根本没有 `[contributes]`，
/// 反序列化出来就是「什么都不贡献」，行为与今天逐字节一致。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Contributes {
    /// 要接哪几个钩子点。**只认 `hook::POINTS` 里那五个名字**（就是 trait 的方法名）；
    /// 写错一个即**激活失败** —— 与 `[store] backend` / `[loop] impl` 同一条原则：
    /// 写了却不生效是最难查的一类问题，宁可当场吵。
    pub hooks: Vec<String>,
    /// 要贡献的提示词段 id。**正文不写在这里** —— 激活时用 `prompt/segment` 取回，
    /// 免得提示词内容在两处各存一份、改了清单忘了正文。
    pub prompt_segments: Vec<String>,
    /// 是否贡献主题补丁（激活时用 `theme/patch` 取回）。
    ///
    /// 本期只做「取回 + 校验 + 展示」，GUI 合并**推迟**：界面主题在建窗口**之前**
    /// 就定型了（`gui/src/main.rs` 的 `load_theme` 跑在主线程，而插件加载在后台线程），
    /// 组件补丁天然晚于主题。协议先立好，省得将来再改一遍 RPC。
    pub theme: bool,
    /// 能力申请。非空时，**本插件的工具调用一律走 `Ask`**（见 `auth::Policy::level_for`）：
    /// 声明了要什么能力就得由人点一次头，不许静默搭 `[auth] plugin_tools` 的便车。
    pub caps: Vec<String>,
}

impl Contributes {
    /// 什么都没贡献 —— 用来判断「这个插件是不是只剩工具」。
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
            && self.prompt_segments.is_empty()
            && !self.theme
            && self.caps.is_empty()
    }

    /// 生效值 = 清单 ← `initialize` 覆盖（**后写者胜**，与阶段 5 的层栈同一个思路）。
    ///
    /// 为什么不只认一处：清单是**离线声明**（宿主不启动插件也看得见，模型照着它写），
    /// `initialize` 是**进程实况**（插件自己知道得更准，比如它按环境变量决定接哪些点）。
    /// 两处都有时以进程为准；只写清单的老插件照旧可用。
    pub fn overridden_by(self, live: Option<Self>) -> Self {
        let Some(live) = live else { return self };
        Self {
            hooks: if live.hooks.is_empty() { self.hooks } else { live.hooks },
            prompt_segments: if live.prompt_segments.is_empty() {
                self.prompt_segments
            } else {
                live.prompt_segments
            },
            theme: live.theme || self.theme,
            caps: if live.caps.is_empty() { self.caps } else { live.caps },
        }
    }

    /// 校验并规范化：去空白、丢空项、去重；钩子点再按 [`crate::hook::POINTS`] 的固定顺序排。
    ///
    /// 认不出的钩子点直接报错（见字段文档）。排序也是为了让「钩子按什么顺序跑」
    /// 只取决于声明内容、不取决于书写顺序 —— 同一份清单每次给出同一个执行序。
    pub fn normalize(mut self) -> Result<Self> {
        let mut hooks: Vec<String> = Vec::new();
        for p in self.hooks.drain(..) {
            let p = p.trim().to_string();
            if p.is_empty() {
                continue;
            }
            if !crate::hook::POINTS.contains(&p.as_str()) {
                bail!(
                    "[contributes] hooks 里有认不出的钩子点: {p}（可用的五个: {}）",
                    crate::hook::POINTS.join(" / ")
                );
            }
            if !hooks.contains(&p) {
                hooks.push(p);
            }
        }
        hooks.sort_by_key(|p| {
            crate::hook::POINTS.iter().position(|x| x == p).unwrap_or(usize::MAX)
        });
        self.hooks = hooks;

        let mut segs: Vec<String> = Vec::new();
        for s in self.prompt_segments.drain(..) {
            let s = s.trim().to_string();
            if !s.is_empty() && !segs.contains(&s) {
                segs.push(s);
            }
        }
        self.prompt_segments = segs;
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub language: String,
    pub description: String,
    /// 脚本插件为入口文件名（main.py）；rust 插件为编译产物文件名
    pub entry: String,
    /// 除工具之外还贡献什么（阶段 6 · 6b）。缺省 = 只提供工具，与今天一致。
    ///
    /// `skip_serializing_if`：`save()` 时不给「什么都没贡献」的插件写一个空的
    /// `[contributes]` 段 —— 老插件重新落盘后仍与旧格式一致。
    #[serde(default, skip_serializing_if = "Contributes::is_empty")]
    pub contributes: Contributes,
}

impl PluginManifest {
    pub fn load(dir: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(dir.join("plugin.toml"))?;
        Ok(toml::from_str(&text)?)
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::write(dir.join("plugin.toml"), toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

struct RpcChannel {
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    next_id: u64,
}

impl RpcChannel {
    /// 读一行。容忍非 UTF-8 字节（lossy 解码），返回 None 表示子进程 EOF。
    ///
    /// 子进程若在协议行里混入 GBK/二进制字节，旧实现会直接报
    /// 「stream did not contain valid UTF-8」而击穿整个 RPC；这里改为尽力解码。
    async fn next_line(&mut self) -> Result<Option<String>> {
        let mut buf: Vec<u8> = Vec::new();
        let n = self.out.read_until(b'\n', &mut buf).await.context("读取插件 stdout 失败")?;
        if n == 0 {
            return Ok(None);
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }
}

/// 一个存活中的插件进程
pub struct PluginHandle {
    pub name: String,
    pub language: String,
    pub description: String,
    pub tool_defs: Vec<ToolDef>,
    /// 生效的组件声明（`plugin.toml [contributes]` ← `initialize` 覆盖后的结果）。
    pub contributes: Contributes,
    /// 激活时取回并校验过的主题补丁（与 `config.toml [theme]` 同形状）。
    /// 本期**只存与展示**，GUI 合并推迟（见 [`Contributes::theme`]）。
    pub theme_patch: Option<Box<crate::config::ThemeConfig>>,
    /// 钩子 RPC 的失败账 —— 与 [`crate::hook::ComponentHook`] **共享同一对 Arc**：
    /// 钩子那边只读（展示），这里只写（记账）。钩子不允许抛错（抛了会击穿主循环），
    /// 所以失败只能这样留痕 —— 静默降级，但不静默消失。
    pub hook_failures: Arc<std::sync::atomic::AtomicU64>,
    pub hook_last_error: Arc<std::sync::Mutex<Option<String>>>,
    cfg: RuntimesConfig,
    /// 本插件进程的**围栏**（Job Object，见 `src/sandbox.rs`）。
    ///
    /// 放在这里而不是 `Runtime` 上，是为了让它的生命周期**等于进程的生命周期**：
    /// `Fence::drop` 会关掉 Job 句柄，而 `KILL_ON_JOB_CLOSE` 随即带走这个插件
    /// 连同它的子孙。于是「插件停止」这件事有两道保险 —— `child.kill()` 与围栏关闭。
    ///
    /// 用 `Mutex` 包一层只为一件事：`respawn` 要在 `&self` 上换掉它（原子替换，
    /// 旧围栏那一刻被 Drop ⇒ 旧进程被 Job 兜底带走，即便 `kill()` 没等干净）。
    pub fence: std::sync::Mutex<crate::sandbox::Fence>,
    /// 建围栏用的配置（`respawn` 时要用它重新套一次）。
    sandbox: crate::sandbox::Sandbox,
    /// 应用根目录：插件进程的工作目录。插件内相对路径因此与宿主视图一致。
    root: PathBuf,
    dir: PathBuf,
    manifest: PluginManifest,
    rpc: Mutex<RpcChannel>,
    child: Mutex<Child>,
}

impl PluginHandle {
    /// 发送一次 RPC 并等待匹配 id 的响应（忽略其他行）。
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let mut ch = self.rpc.lock().await;
        ch.next_id += 1;
        let id = ch.next_id;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        ch.stdin.write_all(line.as_bytes()).await.context("写入插件 stdin 失败")?;
        ch.stdin.flush().await?;
        loop {
            let line = ch
                .next_line()
                .await?
                .ok_or_else(|| anyhow!("插件 {} 进程已退出", self.name))?;
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue; // 插件打印的非协议行，忽略
            };
            if v.get("id").and_then(|x| x.as_u64()) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                bail!("插件 {} 返回错误: {err}", self.name);
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    pub async fn shutdown(&self) {
        // 尽力通知，随后强制结束
        let _ = self.rpc("shutdown", Value::Null).await;
        let _ = self.child.lock().await.kill().await;
    }

    /// 杀死当前进程并重新拉起 + 重新握手。用于插件无响应/崩溃后的自愈。
    pub async fn respawn(&self) -> Result<()> {
        {
            let mut c = self.child.lock().await;
            let _ = c.kill().await;
        }
        let mut child = spawn_command(&self.cfg, &self.root, &self.dir, &self.manifest, &self.sandbox)?
            .spawn()
            .context("重启插件进程失败")?;
        // 新进程要**重新套一次围栏**，理由与首次启动相同：它也是冻着出来的。
        // 旧围栏在这行之后才被换掉（它的 Job 里只有那个已经死掉的旧进程，
        // 关不关都无所谓，但顺序上先套新的更安全）。
        let fence = self.sandbox.contain(&child);
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("无法获取插件 stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("无法获取插件 stdout"))?;
        {
            let mut ch = self.rpc.lock().await;
            *ch = RpcChannel { stdin, out: BufReader::new(stdout), next_id: 0 };
        }
        *self.child.lock().await = child;
        if let Ok(mut f) = self.fence.lock() {
            *f = fence;
        }
        // 握手确认新进程可用
        self.rpc("initialize", json!({})).await?;
        Ok(())
    }

    /// 围栏状态的一行短文本（`/plugins` 用）。
    pub fn fence_status(&self) -> String {
        match self.fence.lock() {
            Ok(f) => f.status_text(),
            Err(_) => "围栏 ?".to_string(),
        }
    }
}

/// 插件提供的一个工具（对 ToolRegistry 注册）
pub struct PluginTool {
    handle: Arc<PluginHandle>,
    def: ToolDef,
}

#[async_trait::async_trait]
impl crate::tool::Tool for PluginTool {
    fn name(&self) -> &str {
        &self.def.name
    }
    fn kind(&self) -> crate::tool::ToolKind {
        crate::tool::ToolKind::Plugin(self.handle.name.clone())
    }
    fn description(&self) -> &str {
        &self.def.description
    }
    fn parameters(&self) -> Value {
        self.def.parameters.clone()
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> crate::tool::ToolOutput {
        use crate::tool::ToolOutput;
        let timeout = Duration::from_secs(rt.config.agent.tool_timeout_secs);
        let params = json!({"name": self.def.name, "arguments": args});

        let first = tokio::time::timeout(timeout, self.handle.rpc("tools/call", params.clone())).await;
        let first_err = match first {
            Ok(Ok(v)) => return parse_tool_result(v),
            Ok(Err(e)) => format!("插件 {} 通信失败: {e:#}", self.handle.name),
            Err(_) => format!("插件 {} 响应超时（{} 秒）", self.handle.name, timeout.as_secs()),
        };

        // 插件无响应/崩溃：重启进程后重试一次
        match self.handle.respawn().await {
            Ok(()) => {
                match tokio::time::timeout(timeout, self.handle.rpc("tools/call", params)).await {
                    Ok(Ok(v)) => parse_tool_result(v),
                    Ok(Err(e)) => ToolOutput::err(format!("{first_err}；重启插件后重试仍失败: {e:#}")),
                    Err(_) => ToolOutput::err(format!("{first_err}；重启插件后重试仍超时")),
                }
            }
            Err(re) => ToolOutput::err(format!("{first_err}；且插件重启失败: {re:#}")),
        }
    }
}

fn parse_tool_result(v: Value) -> crate::tool::ToolOutput {
    let content = v
        .get("content")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| v.to_string());
    let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
    // `"attach": ["<绝对路径>", …]` —— 插件**产出的文件**，宿主会挂给模型看。
    //
    // 这是「插件也能上传」的落点：插件是独立进程，调不到宿主的 `attach_file` 工具，
    // 但它能在自己的返回值里**声明**产出的文件。宿主拿到的是一条路径，走的是与用户拖放
    // 完全相同的判定（`Attachment::probe`）—— 所以「插件截图 → 模型真的看见那张图」成立，
    // 而不是只把路径当字符串塞进上下文。协议形状见 `prompt::BUILTIN_PROMPT`。
    let attach = v
        .get("attach")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.as_str())
                .map(std::path::PathBuf::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    crate::tool::ToolOutput { content, is_error, attach }
}

fn spawn_command(
    cfg: &RuntimesConfig,
    root: &Path,
    dir: &Path,
    manifest: &PluginManifest,
    sandbox: &crate::sandbox::Sandbox,
) -> Result<Command> {
    let stderr_log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("stderr.log"))?;
    // 入口脚本一律用绝对路径，这样即便把工作目录设成应用根也不会找错文件。
    let entry = dir.join(&manifest.entry);
    let mut cmd = match manifest.language.as_str() {
        "python" => {
            let mut c = Command::new(&cfg.python);
            // -X utf8 + PYTHONUTF8/PYTHONIOENCODING：强制子进程 stdin/stdout 走 UTF-8。
            // 这是从宿主侧根治「Windows 默认 GBK 输出破坏 JSON 协议」的关键，
            // 不再依赖模型在每个插件里手写编码补丁。
            // -u：关闭缓冲，保证协议行即时可见。
            c.args(["-X", "utf8", "-u"]).arg(&entry);
            c.env("PYTHONUTF8", "1").env("PYTHONIOENCODING", "utf-8");
            c
        }
        "node" => {
            let mut c = Command::new(&cfg.node);
            c.arg(&entry);
            c.env("NODE_OPTIONS", "--no-warnings");
            c
        }
        "rust" => Command::new(&entry),
        other => bail!("未知插件语言: {other}"),
    };
    // 围栏要求「先冻住」（`CREATE_SUSPENDED`），由 `spawn_and_init` 在加入 Job 之后再放行。
    // 沙箱关着时这一项是 0，行为与围栏之前完全一致。
    cmd.process_flags(sandbox.create_flags());
    cmd.current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_log))
        .kill_on_drop(true);
    Ok(cmd)
}

/// 启动插件进程并完成 initialize 握手
async fn spawn_and_init(
    cfg: &RuntimesConfig,
    root: &Path,
    dir: &Path,
    manifest: &PluginManifest,
    sandbox: &crate::sandbox::Sandbox,
) -> Result<PluginHandle> {
    let mut child = spawn_command(cfg, root, dir, manifest, sandbox)?
        .spawn()
        .with_context(|| format!("启动插件 {} 失败（请检查 {} 是否在 PATH 中）", manifest.name, cfg.python))?;
    // 立刻套围栏：进程是冻着出来的，这里是**唯一**能保证「它跑第一行代码之前
    // 已经在 Job 里」的位置。放进 `PluginHandle` 之后它就与进程同生共死了。
    let fence = sandbox.contain(&child);
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("无法获取插件 stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("无法获取插件 stdout"))?;

    let handle = PluginHandle {
        name: manifest.name.clone(),
        language: manifest.language.clone(),
        description: manifest.description.clone(),
        tool_defs: vec![],
        contributes: Contributes::default(),
        theme_patch: None,
        hook_failures: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        hook_last_error: Arc::new(std::sync::Mutex::new(None)),
        fence: std::sync::Mutex::new(fence),
        sandbox: sandbox.clone(),
        cfg: cfg.clone(),
        root: root.to_path_buf(),
        dir: dir.to_path_buf(),
        manifest: manifest.clone(),
        rpc: Mutex::new(RpcChannel { stdin, out: BufReader::new(stdout), next_id: 0 }),
        child: Mutex::new(child),
    };

    let result = tokio::time::timeout(INIT_TIMEOUT, handle.rpc("initialize", json!({})))
        .await
        .map_err(|_| anyhow!("插件 {} initialize 握手超时（{} 秒）", manifest.name, INIT_TIMEOUT.as_secs()))??;

    // ---- 工具（**老形状一个字段都没动**）----
    // `tools` 缺席按空处理：只贡献钩子/提示词段、不提供工具的组件，本来就没必要
    // 造一个空数组。它要是数组但不是数组（类型写错），仍然当场报错。
    let tools: Vec<Value> = match result.get("tools") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => v
            .as_array()
            .cloned()
            .ok_or_else(|| anyhow!("插件 {} initialize 的 tools 不是数组", manifest.name))?,
    };
    let mut defs = vec![];
    for t in &tools {
        let name = t
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| anyhow!("插件 {} 的工具缺少 name", manifest.name))?
            .to_string();
        let description = t.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string();
        let parameters = t.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}));
        defs.push(ToolDef { name, description, parameters });
    }

    // ---- 贡献声明：清单 ← `initialize` 覆盖（阶段 6 · 6b）----
    // 不返回 `contributes` 就是老形状，照旧只用清单（多数插件那份是空的）。
    let live = match result.get("contributes") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            serde_json::from_value::<Contributes>(v.clone())
                .with_context(|| format!("插件 {} 的 initialize.contributes 解析失败", manifest.name))?,
        ),
    };
    let contributes = manifest
        .contributes
        .clone()
        .overridden_by(live)
        .normalize()
        .with_context(|| format!("插件 {} 的贡献声明有问题", manifest.name))?;

    // **阶段 6 · 6b 放宽的那一处**：以前「未声明任何工具」直接判失败，于是
    // 「只贡献钩子、不提供工具」的组件根本没有活路。现在改成取并集 ——
    // 工具与贡献**至少得有一样**；两样都没有才是真的配置错误（它什么都不做）。
    if defs.is_empty() && contributes.is_empty() {
        bail!(
            "插件 {} 既没声明工具，也没在 [contributes] 里声明任何贡献 —— 它什么都不会做",
            manifest.name
        );
    }

    Ok(PluginHandle { tool_defs: defs, contributes, ..handle })
}

/// 组件贡献的提示词段默认排在**所有段之后**（内置 core=10 / protocol=20 /
/// env=30 / rules=40，配置段缺省 50）。组件是「补一层」，不该插到项目规则前面去。
const COMPONENT_SEGMENT_ORDER: i32 = 60;

/// 组件贡献出问题时的提示。
///
/// 走 stderr 而不是 UI 事件：激活发生在**组装 Runtime 期间**（CLI 还没进 REPL、
/// GUI 还没建窗口），此刻根本没有 ui 可以发。运行期出的事故另有
/// [`crate::hook::ComponentHook`] 那本失败账兜底（`/hook` 里看得见）。
fn component_warn(plugin: &str, why: impl std::fmt::Display) {
    eprintln!("[组件] {plugin}: {why}");
}

/// 把插件提供的**工具与贡献**注册进 Runtime，并把句柄纳入管理。
///
/// 顺序：**先取贡献内容（要 RPC）→ 再注册工具 → 最后挂贡献**。
///
/// 两条错误处理的界线是刻意分开的：
/// - **工具注册失败 = 整个插件装不上**（与今天一致，回滚已注册的）；
/// - **贡献内容取不回来 = 只警告那一条**（降级）。一个提示词段取不回来，
///   不该把插件已经能用的工具一起废掉。
async fn activate(rt: &Arc<Runtime>, handle: PluginHandle) -> Result<Vec<String>> {
    // ---- ① 取回需要 RPC 的贡献（提示词段正文 / 主题补丁）----
    let mut seg_texts: Vec<(String, String)> = Vec::new();
    for id in &handle.contributes.prompt_segments {
        match handle.rpc("prompt/segment", json!({ "id": id })).await {
            Ok(v) => {
                let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                if text.trim().is_empty() {
                    component_warn(&handle.name, format!("提示词段 {id} 的正文为空，跳过"));
                } else {
                    seg_texts.push((id.clone(), text));
                }
            }
            Err(e) => component_warn(&handle.name, format!("取提示词段 {id} 失败: {e:#}")),
        }
    }

    let mut theme_patch: Option<Box<crate::config::ThemeConfig>> = None;
    if handle.contributes.theme {
        match handle.rpc("theme/patch", Value::Null).await {
            Ok(v) => match serde_json::from_value::<crate::config::ThemeConfig>(v) {
                Ok(t) => theme_patch = Some(Box::new(t)),
                Err(e) => component_warn(
                    &handle.name,
                    format!("主题补丁不是 [theme] 那个形状，已忽略: {e}"),
                ),
            },
            Err(e) => component_warn(&handle.name, format!("取主题补丁失败: {e:#}")),
        }
    }

    let handle = Arc::new(PluginHandle { theme_patch, ..handle });

    // ---- ② 工具：注册失败即整体回滚（与今天一致）----
    let mut registered: Vec<String> = vec![];
    for def in &handle.tool_defs {
        let tool = Arc::new(PluginTool { handle: handle.clone(), def: def.clone() });
        if let Err(e) = rt.register(tool).await {
            for n in &registered {
                rt.unregister(n).await;
            }
            handle.shutdown().await;
            return Err(e.context(format!("激活插件 {} 失败", handle.name)));
        }
        registered.push(def.name.clone());
    }

    // ---- ③ 贡献：钩子（纯本地注册，不发 RPC）----
    // 失败账与句柄**共享同一对 Arc**，所以 `/hook` 报告读到的就是这里记下的。
    if !handle.contributes.hooks.is_empty() {
        rt.add_hook(Arc::new(crate::hook::ComponentHook::new(
            handle.name.clone(),
            handle.contributes.hooks.clone(),
            handle.contributes.caps.clone(),
            handle.hook_failures.clone(),
            handle.hook_last_error.clone(),
        )));
    }

    // ---- ④ 贡献：提示词段（正文在 ① 已取回，这里只入注册表）----
    for (id, body) in seg_texts {
        rt.add_prompt_segment(crate::prompt::PromptSegment {
            // 段 id 加插件名前缀：段 id 是全局唯一的，两个组件各写一个 `extra_rules`
            // 不该互相顶掉（那正是「后注册者胜」会干的事）。
            id: format!("{}:{id}", handle.name),
            title: format!("{id}（{}）", handle.name),
            body,
            order: COMPONENT_SEGMENT_ORDER,
            source: crate::prompt::Source::Plugin,
        });
    }

    rt.plugins.lock().await.insert(handle.name.clone(), handle);
    Ok(registered)
}

/// create_plugin 元工具的实现：写源码 → （rust 则编译）→ 启动握手 → 注册工具。
/// 成功返回注册的工具名列表。
pub async fn create_plugin(
    rt: &Arc<Runtime>,
    name: &str,
    language: &str,
    description: &str,
    source: &str,
) -> Result<Vec<String>> {
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        || name.is_empty()
        || name.len() > 40
    {
        bail!("插件名只能包含小写字母/数字/下划线，且长度 1-40: {name:?}");
    }
    let entry = match language {
        "python" => "main.py",
        "node" => "main.js",
        "rust" => "main.rs",
        other => bail!("language 只能是 python / node / rust，收到: {other}"),
    };
    let dir = rt.plugins_dir.join(name);
    // 只有「正在运行的插件」才需要先卸载；残留目录（上次创建失败留下的）自动清理后重建，
    // 避免出现「创建报已存在、删除报不存在」的死循环。
    if rt.plugins.lock().await.contains_key(name) {
        bail!("插件 {name} 正在运行，如需重建请先 remove_plugin");
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("清理残留插件目录失败: {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(entry), source)?;

    let result = create_plugin_inner(rt, name, language, description, entry, &dir).await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir); // 失败不留残骸
    }
    result
}

async fn create_plugin_inner(
    rt: &Arc<Runtime>,
    name: &str,
    language: &str,
    description: &str,
    entry: &str,
    dir: &Path,
) -> Result<Vec<String>> {
    let mut entry = entry.to_string();

    if language == "rust" {
        let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
        // 用解析后的 rustc（可能是应用目录内置的），而不是 config 里的原始值
        let runtimes = rt.runtimes();
        let mut cmd = Command::new(&runtimes.rustc);
        cmd.arg("--edition=2021").arg("-O");
        // 静态链接 CRT，与宿主自身的构建保持一致（见 .cargo/config.toml）。
        // 只加在 windows 上：其他平台 +crt-static 语义不同（会牵动 libc 的静态/动态链接），
        // 而这里的目标就是「让插件 exe 不再依赖 VCRUNTIME140.dll」这一件事。
        if cfg!(windows) {
            cmd.args(["-C", "target-feature=+crt-static"]);
        }
        cmd.arg("main.rs").arg("-o").arg(&exe).current_dir(dir);
        cmd.hide_console();
        let build = tokio::time::timeout(RUST_BUILD_TIMEOUT, cmd.output())
            .await
            .map_err(|_| anyhow!("rustc 编译超时"))?
            .context("无法启动 rustc（rust 插件需要 Rust 工具链在 PATH 中）")?;
        if !build.status.success() {
            bail!("rustc 编译失败:\n{}", String::from_utf8_lossy(&build.stderr));
        }
        entry = exe;
    }

    let manifest = PluginManifest {
        name: name.to_string(),
        language: language.to_string(),
        description: description.to_string(),
        entry,
        // `create_plugin` 造出来的插件默认只提供工具；要声明贡献，模型随后改
        // `plugin.toml`（或直接让 `initialize` 返回 `contributes`）——两条路都认。
        contributes: Contributes::default(),
    };
    manifest.save(dir)?;

    let runtimes = rt.runtimes();
    let handle = spawn_and_init(&runtimes, &rt.root, dir, &manifest, &rt.sandbox).await?;
    activate(rt, handle).await
}

/// remove_plugin 元工具的实现。
/// 无论插件是否处于加载状态，都尝试清理其目录，保证「删了就是删了」。
pub async fn remove_plugin(rt: &Arc<Runtime>, name: &str) -> Result<Vec<String>> {
    let handle = {
        let mut plugins = rt.plugins.lock().await;
        plugins.remove(name)
    };
    let mut removed = vec![];
    if let Some(handle) = handle {
        for def in &handle.tool_defs {
            rt.unregister(&def.name).await;
            removed.push(def.name.clone());
        }
        handle.shutdown().await;
        drop(handle);
    }
    let dir = rt.plugins_dir.join(name);
    let dir_existed = dir.exists();
    if dir_existed {
        std::fs::remove_dir_all(&dir).with_context(|| format!("删除插件目录失败: {}", dir.display()))?;
    }
    if removed.is_empty() && !dir_existed {
        bail!("插件不存在: {name}");
    }
    Ok(removed)
}

/// 启动时扫描 plugins/ 目录，恢复所有持久化插件。
/// 返回 (插件名, 结果) 列表，失败不阻断启动。
pub async fn load_all(rt: &Arc<Runtime>) -> Vec<(String, Result<Vec<String>>)> {
    let mut out = vec![];
    let Ok(entries) = std::fs::read_dir(&rt.plugins_dir) else {
        return out;
    };
    // 解析后的解释器配置，整轮扫描共用一份（中途不会被改写）
    let runtimes = rt.runtimes();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || dir.file_name().map(|n| n == "sdk").unwrap_or(false) {
            continue;
        }
        let manifest = match PluginManifest::load(&dir) {
            Ok(m) => m,
            Err(e) => {
                out.push((
                    dir.file_name().unwrap_or_default().to_string_lossy().to_string(),
                    Err(anyhow!("manifest 读取失败: {e:#}")),
                ));
                continue;
            }
        };
        let name = manifest.name.clone();
        let result = match spawn_and_init(&runtimes, &rt.root, &dir, &manifest, &rt.sandbox).await {
            Ok(handle) => activate(rt, handle).await,
            Err(e) => Err(e),
        };
        out.push((name, result));
    }
    out
}

/// 确保 plugins/sdk/ 存在（Python SDK + 协议说明），供模型编写的插件引用。
/// 这两个文件由宿主完全拥有，每次启动覆盖写入，保证与宿主版本一致。
pub fn ensure_sdk(plugins_dir: &Path) -> Result<()> {
    let sdk = plugins_dir.join("sdk");
    std::fs::create_dir_all(&sdk)?;
    std::fs::write(sdk.join("plugin_sdk.py"), PYTHON_SDK)?;
    std::fs::write(sdk.join("PROTOCOL.md"), PROTOCOL_DOC)?;
    Ok(())
}

pub const PROTOCOL_DOC: &str = r#"# 插件协议（宿主 <-> 插件子进程）

每条消息是 stdout/stdin 上的「一行 JSON」。stdout 只允许协议消息，日志一律写 stderr。

## 1. 握手

宿主发送:
   {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
插件回复一行:
   {"jsonrpc":"2.0","id":1,"result":{
      "name":"<插件名>",
      "tools":[{"name":"<工具名>","description":"<说明>","parameters":<JSON Schema>}],
      "contributes":{ ... }          // 可选，见 §4
   }}

`tools` 可以省略（只贡献、不提供工具的组件就没必要写它）。
`contributes` 省略即老形状 —— 只提供工具的插件一个字段都不用改。

## 2. 工具调用

宿主: {"jsonrpc":"2.0","id":N,"method":"tools/call","params":{"name":"<工具名>","arguments":{...}}}
插件: {"jsonrpc":"2.0","id":N,"result":{"content":"<字符串结果>","is_error":false}}

## 3. 结束

宿主发送 {"jsonrpc":"2.0","method":"shutdown"}（无 id），插件应退出。

## 4. 组件贡献（可选，阶段 6 · 6b）

在 plugin.toml 里声明：

    [contributes]
    hooks = ["before_tool", "on_error"]   # 只认下面五个名字，写错即激活失败
    prompt_segments = ["extra_rules"]     # 段 id；正文用 prompt/segment 取回
    theme = true                          # 是否贡献主题补丁
    caps = ["fs_write"]                   # 能力申请：本插件的工具调用一律走「询问」

也可以改由上面 initialize 的 `contributes` 返回 —— **两处都有时以 initialize 为准**
（清单是离线声明，进程实况更准）。

**钩子实现不许报错**：宿主把超时（10 秒）、通信失败、以及认不出的返回值一律当作
「放行」，并把失败记在账上（`/hook` 里看得到）。所以钩子只管返回想表达的意思，
不必自己兜底 —— 但**也不要**拿它做耗时的事，它挡着主循环。

### 4.1 钩子

宿主: {"jsonrpc":"2.0","id":N,"method":"hooks/<点>","params":{...}}
插件: {"jsonrpc":"2.0","id":N,"result":{...}}

点               params                              result
before_request   {"tools":[...]}                     {"verdict":"pass"|"reject","reason":"…","inject":["补充文本"]}
after_reply      {"text":…,"tool_calls":<条数>}       {"verdict":…,"text":"改写后的正文"}
before_tool      {"tool":"名字","args":{...}}         {"verdict":…,"args":{改后的参数}}
after_tool       {"tool":…,"content":…,"is_error":…}  {"verdict":…,"content":…,"is_error":…}
on_error         {"error":"…"}                        {"action":"propagate"|"retry","note":"给模型的提示"}

`inject` 只接受**用户文本**（字符串数组）—— 钩子这一层只开放「追加」，
不给「改写历史」的能力（那是 `[loop] max_rounds` 的事）。
`after_reply` 只能改正文，不能改 `tool_calls`（改了等于伪造模型意图）。

### 4.2 提示词段

宿主: {"jsonrpc":"2.0","id":N,"method":"prompt/segment","params":{"id":"extra_rules"}}
插件: {"jsonrpc":"2.0","id":N,"result":{"text":"这一段的正文"}}

### 4.3 主题补丁

宿主: {"jsonrpc":"2.0","id":N,"method":"theme/patch","params":null}
插件: {"jsonrpc":"2.0","id":N,"result":{ ...与 config.toml 的 [theme] 同一形状... }}

本期宿主只**取回并校验**主题补丁（GUI 合并尚未接线，见 `plugin/mod.rs` 的注释）。

Python 插件推荐直接使用同目录下的 plugin_sdk.py。
"#;

pub const PYTHON_SDK: &str = r#""""
AOX - Python 插件 SDK

最小插件（只提供工具）:
    import sys, os
    sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
    from plugin_sdk import serve

    def my_tool(args):
        return "结果字符串"

    serve("插件名", {
        "my_tool": ("工具说明", {"type": "object", "properties": {}}, my_tool),
    })

组件（除工具之外还贡献钩子 / 提示词段 / 主题）:
    def before_tool(params):
        # params = {"tool": "工具名", "args": {...}}
        return {"verdict": "pass", "args": params["args"]}

    serve(
        "插件名",
        {},
        contributes={"hooks": ["before_tool"], "prompt_segments": ["note"], "caps": ["fs_write"]},
        hooks={"before_tool": before_tool},
        prompt_segments={"note": "写文件之前先想清楚要写什么。"},
    )

工具或钩子里抛异常都不会连累宿主：工具异常变成 is_error 结果，
钩子异常写进 stderr 并按「放行」返回。
"""
import sys
import json


def _resolve(value):
    # 允许「值」和「无参函数」两种写法（后者用于每次现算的场景）
    return value() if callable(value) else value


def _send(msg):
    sys.stdout.write(json.dumps(msg, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def serve(plugin_name, tools, contributes=None, hooks=None, prompt_segments=None, theme=None):
    """起服务。参数都可缺省，只写用得上的那些。

    tools:           {工具名: (说明, JSON Schema, handler)}，handler(dict) -> 结果字符串
    contributes:     贡献声明 dict（hooks / prompt_segments / theme / caps），
                     等价于写在 plugin.toml 的 [contributes] 里（两处都有时以此为准）
    hooks:           {点名字: fn(params) -> dict}，点名字见 PROTOCOL.md 的五个
    prompt_segments: {段 id: 正文}，正文也可以是 fn() -> 正文
    theme:           主题补丁 dict，也可以是 fn() -> dict
    """
    tools = tools or {}
    hooks = hooks or {}
    prompt_segments = prompt_segments or {}
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except Exception:
            continue
        mid = req.get("id")
        method = req.get("method")
        params = req.get("params") or {}
        if method == "initialize":
            result = {
                "name": plugin_name,
                "tools": [
                    {"name": n, "description": d, "parameters": s}
                    for n, (d, s, h) in tools.items()
                ],
            }
            if contributes:
                result["contributes"] = contributes
            _send({"jsonrpc": "2.0", "id": mid, "result": result})
        elif method == "tools/call":
            tname = params.get("name")
            args = params.get("arguments") or {}
            try:
                if tname not in tools:
                    raise KeyError(f"未知工具: {tname}")
                handler = tools[tname][2]
                _send({"jsonrpc": "2.0", "id": mid,
                       "result": {"content": str(handler(args)), "is_error": False}})
            except Exception as e:
                _send({"jsonrpc": "2.0", "id": mid,
                       "result": {"content": f"{type(e).__name__}: {e}", "is_error": True}})
        elif method.startswith("hooks/"):
            point = method[len("hooks/"):]
            handler = hooks.get(point)
            if handler is None:
                # 没声明这个点：直接「放行」，宿主那边连失败账都不会记
                _send({"jsonrpc": "2.0", "id": mid, "result": {}})
            else:
                try:
                    _send({"jsonrpc": "2.0", "id": mid, "result": handler(params) or {}})
                except Exception as e:
                    # 钩子抛异常不连累宿主：写 stderr 留痕，按「放行」返回
                    sys.stderr.write(f"[hook {point}] {type(e).__name__}: {e}\n")
                    sys.stderr.flush()
                    _send({"jsonrpc": "2.0", "id": mid, "result": {}})
        elif method == "prompt/segment":
            sid = params.get("id")
            if sid in prompt_segments:
                _send({"jsonrpc": "2.0", "id": mid,
                       "result": {"text": str(_resolve(prompt_segments[sid]))}})
            else:
                _send({"jsonrpc": "2.0", "id": mid, "result": {"text": ""}})
        elif method == "theme/patch":
            _send({"jsonrpc": "2.0", "id": mid, "result": _resolve(theme) or {}})
        elif method == "shutdown":
            break
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// 老清单（没有 `[contributes]`）必须原样能解析 —— 向后兼容是这一阶段的硬指标。
    #[test]
    fn old_manifest_without_contributes_still_parses() {
        let m: PluginManifest = toml::from_str(
            "name = \"fs\"\nlanguage = \"python\"\ndescription = \"文件读写\"\nentry = \"main.py\"\n",
        )
        .unwrap();
        assert!(m.contributes.is_empty(), "老清单不该凭空长出贡献");
    }

    /// 新清单能解析出四类声明。
    #[test]
    fn manifest_parses_the_contributes_section() {
        let m: PluginManifest = toml::from_str(
            "name = \"comp\"\nlanguage = \"python\"\ndescription = \"组件\"\nentry = \"main.py\"\n\n\
             [contributes]\n\
             hooks = [\"before_tool\", \"on_error\"]\n\
             prompt_segments = [\"note\"]\n\
             theme = true\n\
             caps = [\"fs_write\"]\n",
        )
        .unwrap();
        assert_eq!(m.contributes.hooks, vec!["before_tool", "on_error"]);
        assert_eq!(m.contributes.prompt_segments, vec!["note"]);
        assert!(m.contributes.theme);
        assert_eq!(m.contributes.caps, vec!["fs_write"]);
        assert!(!m.contributes.is_empty());
    }

    /// 空的贡献段不该被**写出来** —— 老插件重新落盘后格式仍与旧版一致。
    #[test]
    fn save_omits_an_empty_contributes_section() {
        let dir = std::env::temp_dir().join("aox-manifest-save-test");
        std::fs::create_dir_all(&dir).unwrap();
        let m = PluginManifest {
            name: "fs".into(),
            language: "python".into(),
            description: "d".into(),
            entry: "main.py".into(),
            contributes: Contributes::default(),
        };
        m.save(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join("plugin.toml")).unwrap();
        assert!(!text.contains("contributes"), "空的贡献段不该被写出来: {text}");
        let _ = std::fs::remove_file(dir.join("plugin.toml"));
    }

    /// 认不出的钩子点名 = **激活失败**，且错误里要同时给出「是哪个」和「可用哪些」。
    #[test]
    fn normalize_rejects_an_unknown_hook_point() {
        let c = Contributes {
            hooks: vec!["before_tool".into(), "before_evrything".into()],
            ..Default::default()
        };
        let e = c.normalize().unwrap_err().to_string();
        assert!(e.contains("before_evrything"), "错误里要点名是哪个: {e}");
        assert!(e.contains("before_request"), "错误里要列出可用的五个: {e}");
    }

    /// 规范化：去空白、丢空项、去重；钩子点顺序固定按 `POINTS`，与书写顺序无关。
    #[test]
    fn normalize_dedups_and_fixes_the_order() {
        let c = Contributes {
            hooks: vec![" on_error ".into(), "before_tool".into(), "on_error".into(), "  ".into()],
            prompt_segments: vec![" note ".into(), "note".into(), "".into()],
            ..Default::default()
        };
        let c = c.normalize().unwrap();
        assert_eq!(c.hooks, vec!["before_tool", "on_error"], "按 POINTS 的固定顺序");
        assert_eq!(c.prompt_segments, vec!["note"]);
    }

    /// `initialize` 覆盖清单（**后写者胜**），但「清单有、initialize 没提」的字段留给清单。
    #[test]
    fn initialize_contributes_wins_over_the_manifest() {
        let manifest = Contributes {
            hooks: vec!["before_tool".into()],
            caps: vec!["fs_read".into()],
            ..Default::default()
        };

        // initialize 什么都没说 → 完全用清单
        assert_eq!(manifest.clone().overridden_by(None).hooks, vec!["before_tool"]);

        // initialize 说了 → 它说的字段以它为准，其余保留清单
        let live = Contributes { hooks: vec!["on_error".into()], theme: true, ..Default::default() };
        let eff = manifest.overridden_by(Some(live));
        assert_eq!(eff.hooks, vec!["on_error"], "进程实况覆盖清单");
        assert!(eff.theme, "清单没提 theme，取 initialize 的");
        assert_eq!(eff.caps, vec!["fs_read"], "initialize 没提 caps，保留清单的");
    }
}
