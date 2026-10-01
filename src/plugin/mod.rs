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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub language: String,
    pub description: String,
    /// 脚本插件为入口文件名（main.py）；rust 插件为编译产物文件名
    pub entry: String,
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
    cfg: RuntimesConfig,
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
        let mut child = spawn_command(&self.cfg, &self.root, &self.dir, &self.manifest)?
            .spawn()
            .context("重启插件进程失败")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("无法获取插件 stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("无法获取插件 stdout"))?;
        {
            let mut ch = self.rpc.lock().await;
            *ch = RpcChannel { stdin, out: BufReader::new(stdout), next_id: 0 };
        }
        *self.child.lock().await = child;
        // 握手确认新进程可用
        self.rpc("initialize", json!({})).await?;
        Ok(())
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
    crate::tool::ToolOutput { content, is_error }
}

fn spawn_command(
    cfg: &RuntimesConfig,
    root: &Path,
    dir: &Path,
    manifest: &PluginManifest,
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
) -> Result<PluginHandle> {
    let mut child = spawn_command(cfg, root, dir, manifest)?
        .spawn()
        .with_context(|| format!("启动插件 {} 失败（请检查 {} 是否在 PATH 中）", manifest.name, cfg.python))?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("无法获取插件 stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("无法获取插件 stdout"))?;

    let handle = PluginHandle {
        name: manifest.name.clone(),
        language: manifest.language.clone(),
        description: manifest.description.clone(),
        tool_defs: vec![],
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

    let tools = result
        .get("tools")
        .and_then(|t| t.as_array())
        .ok_or_else(|| anyhow!("插件 {} initialize 返回缺少 tools 数组", manifest.name))?;
    let mut defs = vec![];
    for t in tools {
        let name = t
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| anyhow!("插件 {} 的工具缺少 name", manifest.name))?
            .to_string();
        let description = t.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string();
        let parameters = t.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}));
        defs.push(ToolDef { name, description, parameters });
    }
    if defs.is_empty() {
        bail!("插件 {} 未声明任何工具", manifest.name);
    }

    Ok(PluginHandle { tool_defs: defs, ..handle })
}

/// 把插件提供的所有工具注册进 Runtime，并把句柄纳入管理。
async fn activate(rt: &Arc<Runtime>, handle: PluginHandle) -> Result<Vec<String>> {
    let handle = Arc::new(handle);
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
        let build = tokio::time::timeout(
            RUST_BUILD_TIMEOUT,
            Command::new(&rt.config.runtimes.rustc)
                .args(["--edition=2021", "-O", "main.rs", "-o", &exe])
                .current_dir(dir)
                .output(),
        )
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
    };
    manifest.save(dir)?;

    let handle = spawn_and_init(&rt.config.runtimes, &rt.root, dir, &manifest).await?;
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
        let result = match spawn_and_init(&rt.config.runtimes, &rt.root, &dir, &manifest).await {
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

1. 启动后宿主发送:
   {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
   插件回复一行:
   {"jsonrpc":"2.0","id":1,"result":{"name":"<插件名>","tools":[{"name":"<工具名>","description":"<说明>","parameters":<JSON Schema>}]}}

2. 工具调用:
   {"jsonrpc":"2.0","id":N,"method":"tools/call","params":{"name":"<工具名>","arguments":{...}}}
   插件回复:
   {"jsonrpc":"2.0","id":N,"result":{"content":"<字符串结果>","is_error":false}}

3. 结束: 宿主发送 {"jsonrpc":"2.0","method":"shutdown"}（无 id），插件应退出。

Python 插件推荐直接使用同目录下的 plugin_sdk.py。
"#;

pub const PYTHON_SDK: &str = r#""""
AOX - Python 插件 SDK
用法:
    import sys, os
    sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
    from plugin_sdk import serve

    def my_tool(args):
        return "结果字符串"

    serve("插件名", {
        "my_tool": ("工具说明", {"type": "object", "properties": {...}}, my_tool),
    })
"""
import sys
import json


def _send(msg):
    sys.stdout.write(json.dumps(msg, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def serve(plugin_name, tools):
    """tools: {tool_name: (description, parameters_schema, handler)}。
    handler 接收 dict 参数，返回值会 str() 后作为 content。"""
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
        if method == "initialize":
            _send({"jsonrpc": "2.0", "id": mid, "result": {
                "name": plugin_name,
                "tools": [
                    {"name": n, "description": d, "parameters": s}
                    for n, (d, s, h) in tools.items()
                ],
            }})
        elif method == "tools/call":
            params = req.get("params") or {}
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
        elif method == "shutdown":
            break
"#;
