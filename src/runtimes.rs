//! 插件运行时的「解析」与「装配」。
//!
//! ## 一、解释器解析（三档，CLI / GUI / selftest 共用）
//!
//! 1. `config.toml` 的 `[runtimes]` 写了**显式路径** → 直接用，不做任何猜测；
//! 2. 该字段是**裸命令**（`python` / `node` / `rustc`）且应用目录下存在内置运行时
//!    `<app_root>/runtime/<lang>/<exe>` → 用内置的那份；
//! 3. 都不是 → 保持裸命令，交给 `PATH`。
//!
//! 第 2 档是这套设计的支点：**把运行时放进应用目录就自动生效**，用户不必改配置，
//! 我们也不需要往系统 `PATH` 或用户环境变量里写任何东西（无侵入、卸载即消失）。
//!
//! ## 二、装配
//!
//! 只自动装配 Python（Node 单文件 80 MB 以上，不适合捆绑；rust 插件运行期本就不需要任何东西）。
//! 顺序：已存在 → 本地离线包 → 下载（离线包与非默认源都不需要网络）。
//!
//! **刻意不引入任何新 crate**：
//! - 下载复用已在依赖里的 `reqwest`（`chunk()` 不需要额外 feature）；
//! - 解压用系统自带 `tar.exe`（bsdtar，Win10 1803+），回退 PowerShell `Expand-Archive`；
//! - 校验用系统自带 `certutil -hashfile ... SHA256`。
//!
//! 三条路都是「系统已有能力」，因此装配链路没有给宿主增加任何体积与依赖。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::config::RuntimesConfig;
use crate::plugin::HideConsole;

/// 离线运行时包的约定文件名。
/// 放到 **exe 同目录** / 应用根 / `<应用根>/runtime` / `%LOCALAPPDATA%\AOX` 下都会被自动发现，
/// 这是完全离线的装配路径。
pub const PACK_NAME: &str = "aox-python-runtime.zip";

/// 默认下载源：CPython 官方「嵌入式发行版」（约 10 MB，解压约 21 MB）。
pub const DEFAULT_PYTHON_URL: &str =
    "https://www.python.org/ftp/python/3.13.7/python-3.13.7-embed-amd64.zip";

/// 与 [`DEFAULT_PYTHON_URL`] 配套的官方 SHA-256。
/// 只对默认源生效；用了自定义 `python_url` 又想校验时，自行在 `[runtimes]` 填 `python_sha256`。
pub const DEFAULT_PYTHON_SHA256: &str =
    "f6cca216a359be84797cabb54149ce5e062afb16cc7567eb7fc51cacb2d86b65";

/// 明确拒绝自动装配 Node 的理由（体积）。
pub const NODE_MANUAL_HINT: &str =
    "Node 运行时单文件就超过 80 MB，不适合自动装配。请自行安装 Node.js 后确保 `node` 在 PATH 中，\
     或在 config.toml 的 [runtimes] node 写入完整路径。";

// ─────────────────────────── 语言 ───────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Python,
    Node,
    Rustc,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::Python, Lang::Node, Lang::Rustc];

    pub fn key(self) -> &'static str {
        match self {
            Lang::Python => "python",
            Lang::Node => "node",
            Lang::Rustc => "rustc",
        }
    }

    /// 内置运行时目录里的可执行文件名。
    pub fn exe(self) -> &'static str {
        if cfg!(windows) {
            match self {
                Lang::Python => "python.exe",
                Lang::Node => "node.exe",
                Lang::Rustc => "rustc.exe",
            }
        } else {
            self.key()
        }
    }
}

// ─────────────────────────── 状态 ───────────────────────────

/// 解释器的来源，用于 `/runtime` 报告与系统提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `config.toml` 里写死的路径
    Config,
    /// 应用目录内置运行时
    Builtin,
    /// `PATH` 里找到的
    Path,
    /// 找不到
    Missing,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Config => "config.toml 指定",
            Origin::Builtin => "应用目录内置",
            Origin::Path => "PATH",
            Origin::Missing => "未找到",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Status {
    pub lang: Lang,
    /// 最终使用的命令（绝对路径或裸命令）
    pub command: String,
    pub origin: Origin,
}

impl Status {
    pub fn ok(&self) -> bool {
        self.origin != Origin::Missing
    }
}

/// 状态的一行文本表示。CLI 的 `/runtime` 报告与 GUI 的关于窗口共用同一份文案。
pub fn one_line(st: &Status) -> String {
    format!(
        "{} {:<7} {}  [{}]",
        if st.ok() { "✓" } else { "✗" },
        st.lang.key(),
        st.command,
        st.origin.label()
    )
}

// ─────────────────────────── 解析 ───────────────────────────

/// 「裸命令」= 不含任何路径分量，因此可以安全地被内置运行时替代。
///
/// 带 `/`、`\` 或 `:`（盘符）的值都视为用户显式指定的路径，绝不覆盖。
fn is_bare(cmd: &str) -> bool {
    let t = cmd.trim();
    !t.is_empty() && !t.contains(['/', '\\', ':'])
}

/// 内置运行时目录：`<app_root>/runtime/<lang>`。
pub fn builtin_dir(root: &Path, lang: Lang) -> PathBuf {
    root.join("runtime").join(lang.key())
}

/// 内置运行时的可执行文件；只有真的存在才返回。
pub fn builtin_exe(root: &Path, lang: Lang) -> Option<PathBuf> {
    let p = builtin_dir(root, lang).join(lang.exe());
    p.is_file().then_some(p)
}

/// `PATH` 里查找可执行文件——标准库没有 `which`，自己写十几行。
fn find_in_path(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    // Windows 上按 PATHEXT 的常见子集找；其余平台只有裸名。
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat", ".com"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let cand = dir.join(format!("{cmd}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

fn cfg_get(cfg: &RuntimesConfig, lang: Lang) -> String {
    match lang {
        Lang::Python => cfg.python.clone(),
        Lang::Node => cfg.node.clone(),
        Lang::Rustc => cfg.rustc.clone(),
    }
}

fn cfg_set(cfg: &mut RuntimesConfig, lang: Lang, v: String) {
    match lang {
        Lang::Python => cfg.python = v,
        Lang::Node => cfg.node = v,
        Lang::Rustc => cfg.rustc = v,
    }
}

/// 三档解析，就地改写 `cfg`。
///
/// 只在「配置值是裸命令」时才用内置运行时覆盖——用户显式写的路径一律尊重。
pub fn resolve(cfg: &mut RuntimesConfig, root: &Path) {
    for lang in Lang::ALL {
        let cur = cfg_get(cfg, lang);
        if !is_bare(&cur) {
            continue;
        }
        if let Some(exe) = builtin_exe(root, lang) {
            cfg_set(cfg, lang, exe.display().to_string());
        }
    }
}

/// 只做静态判定，**不启动任何子进程**——这条路径在每次启动时都会走到，不能为此变慢。
///
/// 注意：`cfg` 必须是**原始**（未经过 [`resolve`]）的配置。
/// 只有拿原始值才能判断出「来源」是 `config.toml 指定` 还是 `应用目录内置`；
/// 若传入解析后的快照，所有内置运行时都会被误标成前者。
pub fn status(root: &Path, cfg: &RuntimesConfig) -> Vec<Status> {
    Lang::ALL
        .into_iter()
        .map(|lang| {
            let raw = cfg_get(cfg, lang);
            let raw = raw.trim().to_string();
            if !is_bare(&raw) {
                let ok = Path::new(&raw).is_file();
                return Status {
                    lang,
                    command: raw,
                    origin: if ok { Origin::Config } else { Origin::Missing },
                };
            }
            if let Some(exe) = builtin_exe(root, lang) {
                return Status { lang, command: exe.display().to_string(), origin: Origin::Builtin };
            }
            if find_in_path(&raw).is_some() {
                return Status { lang, command: raw, origin: Origin::Path };
            }
            Status { lang, command: raw, origin: Origin::Missing }
        })
        .collect()
}

/// 跑一次 `--version` 取版本行。只在用户显式查询 `/runtime` 时调用。
pub fn probe_version(command: &str) -> Option<String> {
    let mut cmd = Command::new(command);
    cmd.arg("--version");
    cmd.hide_console();
    let out = cmd.output().ok()?;
    let text = if out.stdout.is_empty() { out.stderr } else { out.stdout };
    let line = String::from_utf8_lossy(&text).lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

// ─────────────────────────── 对外文案 ───────────────────────────

/// `/runtime` 的报告文本。
pub fn status_report(root: &Path, cfg: &RuntimesConfig) -> String {
    let mut s = String::from("插件运行时：\n");
    for st in status(root, cfg) {
        s.push_str("  ");
        s.push_str(&one_line(&st));
        // 只有真的能用才去跑一次版本探测，避免为「未找到」白起进程
        if st.ok() {
            if let Some(v) = probe_version(&st.command) {
                s.push_str(&format!("  ·  {v}"));
            }
        }
        s.push('\n');
    }
    s.push_str(&format!("  内置运行时目录: {}\n", root.join("runtime").display()));
    s.push_str(&format!("  离线包约定名:   {PACK_NAME}（放在 exe 同目录或应用根即可）\n"));
    if let Some(p) = find_offline_pack(root) {
        s.push_str(&format!("  已发现离线包:   {}\n", p.display()));
    }
    s
}

/// 启动横幅用的一行提示；Python 就绪时返回 `None`，让启动输出保持干净。
pub fn missing_hint(root: &Path, cfg: &RuntimesConfig) -> Option<String> {
    let st = status(root, cfg);
    let py = st.iter().find(|s| s.lang == Lang::Python)?;
    if py.ok() {
        return None;
    }
    Some(format!(
        "未检测到 Python，python 插件不可用（文件读写、系统信息等能力都靠它）。\
         输入 /runtime install python 可安装内置运行时（约 10 MB，装到 {}）。",
        builtin_dir(root, Lang::Python).display()
    ))
}

/// 写进系统提示的「运行时环境」段，避免模型反复创建注定失败的插件。
pub fn prompt_section(root: &Path, cfg: &RuntimesConfig) -> String {
    let st = status(root, cfg);
    // 开头必须带换行：本段是紧接在上一段末尾（以「。」结束）拼接的
    let mut s = String::from("\n- 可用插件运行时：");
    for x in &st {
        s.push_str("\n  - ");
        s.push_str(x.lang.key());
        s.push_str(": ");
        if x.ok() {
            s.push_str(&format!("{}（{}）", x.command, x.origin.label()));
        } else {
            s.push_str("缺失");
        }
    }
    let py_missing = st.iter().any(|x| x.lang == Lang::Python && !x.ok());
    let rustc_missing = st.iter().any(|x| x.lang == Lang::Rustc && !x.ok());
    if py_missing {
        s.push_str(
            "\n- **本机没有 Python**：不要创建 language=\"python\" 的插件，那一定会失败。\
             可以改用 language=\"rust\"（编译产物运行期不需要任何运行时）；\
             或先让用户执行 /runtime install python 安装内置运行时，再重试。",
        );
    }
    if rustc_missing {
        s.push_str(
            "\n- 本机没有 rustc：**创建** rust 插件会失败，但已经编译好的 rust 插件可以正常加载运行。",
        );
    }
    s
}

// ─────────────────────────── 离线包发现 ───────────────────────────

/// 当前进程 exe 所在目录。
///
/// 这里不复用 `crate::paths::exe_dir()`：`src/paths.rs` 只挂在 lib 上（CLI 的 bin 根
/// 没有声明它），而本模块要同时被 bin 与 lib 编译——保持零模块耦合，行为才一致。
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// 离线包候选位置，按优先级：exe 同目录 → 应用根 → 应用根/runtime → %LOCALAPPDATA%\AOX。
pub fn find_offline_pack(root: &Path) -> Option<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = exe_dir() {
        v.push(d.join(PACK_NAME));
    }
    v.push(root.join(PACK_NAME));
    v.push(root.join("runtime").join(PACK_NAME));
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        v.push(PathBuf::from(local).join("AOX").join(PACK_NAME));
    }
    v.into_iter().find(|p| p.is_file())
}

// ─────────────────────────── 系统能力封装 ───────────────────────────

/// 免依赖的 SHA-256：调系统自带 `certutil`。
/// 装配链里唯一需要哈希的地方，因此不值得为此引入 `sha2`。
fn sha256_of(path: &Path) -> Result<String> {
    let mut cmd = Command::new("certutil");
    cmd.arg("-hashfile").arg(path).arg("SHA256");
    cmd.hide_console();
    let out = cmd
        .output()
        .context("调用 certutil 计算校验和失败（系统缺少 certutil？）")?;
    let text = String::from_utf8_lossy(&out.stdout);
    // certutil 输出三行：标题 / 哈希（可能夹空格）/ 完成提示——只认那行 64 位十六进制。
    text.lines()
        .map(|l| l.trim().replace(' ', ""))
        .find(|l| l.len() == 64 && l.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|l| l.to_lowercase())
        .ok_or_else(|| anyhow!("无法从 certutil 输出中解析 SHA-256：\n{}", text.trim()))
}

/// 解压 zip。优先系统自带 `tar.exe`（bsdtar），失败回退 PowerShell。
fn extract_zip(zip: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;

    let mut tar = Command::new("tar");
    tar.arg("-xf").arg(zip).arg("-C").arg(dest);
    tar.hide_console();
    if let Ok(out) = tar.output() {
        if out.status.success() {
            return Ok(());
        }
    }

    let mut ps = Command::new("powershell");
    ps.args(["-NoProfile", "-NonInteractive", "-Command"]).arg(format!(
        "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
        zip.display(),
        dest.display()
    ));
    ps.hide_console();
    let out = ps.output().context("解压失败：tar 与 PowerShell 都无法调用")?;
    if !out.status.success() {
        bail!(
            "解压 {} 失败：\n{}",
            zip.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// 有些分发包把内容塞在一个顶层目录里；解压后拉平一层，保证 `<dest>/python.exe` 就位。
fn flatten_single_dir(dest: &Path) -> Result<()> {
    if dest.join(Lang::Python.exe()).is_file() {
        return Ok(());
    }
    let dirs: Vec<_> = std::fs::read_dir(dest)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    if dirs.len() != 1 {
        return Ok(()); // 结构不属于这种情况，交给后面的存在性校验报错
    }
    let inner = dirs[0].path();
    for e in std::fs::read_dir(&inner)? {
        let e = e?;
        std::fs::rename(e.path(), dest.join(e.file_name()))?;
    }
    let _ = std::fs::remove_dir(&inner);
    Ok(())
}

// ─────────────────────────── 下载 ───────────────────────────

async fn download(url: &str, dest: &Path, progress: &mut dyn FnMut(&str)) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()?;
    let mut resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("请求失败：{url}"))?
        .error_for_status()
        .with_context(|| format!("服务端拒绝了请求：{url}"))?;

    let total = resp.content_length();
    let mut file =
        std::fs::File::create(dest).with_context(|| format!("无法创建 {}", dest.display()))?;
    let mut got: u64 = 0;
    let mut last_bucket = u64::MAX;
    // 逐块写盘：`chunk()` 不需要额外 feature；用同步 File 是因为装配只发生一次，
    // 且阻塞的是 agent 线程而非 UI 线程。
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk)?;
        got += chunk.len() as u64;
        if let Some(t) = total {
            let pct = got.saturating_mul(100) / t.max(1);
            if pct / 10 != last_bucket {
                last_bucket = pct / 10;
                progress(&format!(
                    "下载中 {pct}%（{:.1}/{:.1} MB）",
                    got as f64 / 1048576.0,
                    t as f64 / 1048576.0
                ));
            }
        }
    }
    file.flush()?;
    if let Some(t) = total {
        if got != t {
            bail!("下载不完整：收到 {got} 字节，Content-Length 为 {t}");
        }
    }
    Ok(())
}

// ─────────────────────────── 装配 ───────────────────────────

/// 校验（可选）→ 解压 → 拉平 → 存在性检查。
fn unpack(zip: &Path, staging: &Path, expect_sha: Option<&str>, downloaded: bool, progress: &mut dyn FnMut(&str)) -> Result<()> {
    if let Some(expect) = expect_sha {
        let expect = expect.trim();
        if !expect.is_empty() {
            progress("校验 SHA-256…");
            let got = sha256_of(zip)?;
            if !got.eq_ignore_ascii_case(expect) {
                if downloaded {
                    // 下坏了/被篡改的包别留着，否则下次又踩同一个坑
                    let _ = std::fs::remove_file(zip);
                }
                bail!(
                    "运行时包校验失败：\n  期望 {expect}\n  实际 {got}\n\
                     （若这是你自己托管的包，请在 [runtimes] 里更新 python_sha256）"
                );
            }
            progress("校验通过");
        } else {
            progress("未配置 python_sha256，跳过校验");
        }
    } else {
        progress("未配置 python_sha256，跳过校验");
    }
    progress("解压…");
    extract_zip(zip, staging)?;
    flatten_single_dir(staging)?;
    if !staging.join(Lang::Python.exe()).is_file() {
        bail!("解压后没有找到 {}，包结构可能不正确", Lang::Python.exe());
    }
    Ok(())
}

/// 装配 Python 运行时到 `<app_root>/runtime/python`，返回该目录。
///
/// 顺序：已存在 → 本地离线包 → 下载。`progress` 收到的是给人看的一行行进度文本
/// （CLI 直接打印；GUI 丢弃或转投活动流）。
pub async fn install_python(
    root: &Path,
    cfg: &RuntimesConfig,
    progress: &mut dyn FnMut(&str),
) -> Result<PathBuf> {
    let dest = builtin_dir(root, Lang::Python);
    if builtin_exe(root, Lang::Python).is_some() {
        progress(&format!("内置 Python 运行时已存在：{}", dest.display()));
        return Ok(dest);
    }

    let staging = root.join("runtime").join("python.staging");
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }

    match find_offline_pack(root) {
        // ---- 1) 本地离线包：完全不碰网络
        Some(pack) => {
            progress(&format!("发现离线运行时包：{}", pack.display()));
            unpack(&pack, &staging, cfg.python_sha256.as_deref(), false, progress)?;
        }
        // ---- 2) 下载
        None => {
            let url = cfg
                .python_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_PYTHON_URL)
                .to_string();
            // 校验值：显式配置优先；否则仅对默认官方源套用内置校验值
            let expect = cfg
                .python_sha256
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| (url == DEFAULT_PYTHON_URL).then(|| DEFAULT_PYTHON_SHA256.to_string()));

            let cache = root.join("runtime").join("download");
            std::fs::create_dir_all(&cache)?;
            let name = url.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(PACK_NAME);
            let zip = cache.join(name);
            progress(&format!("下载运行时：{url}"));
            download(&url, &zip, progress).await?;
            let res = unpack(&zip, &staging, expect.as_deref(), true, progress);
            // 装配完成（无论成败）都清掉下载缓存，避免在应用目录里留一个 10 MB 的残骸
            let _ = std::fs::remove_dir_all(&cache);
            res?;
        }
    }

    if dest.exists() {
        let _ = std::fs::remove_dir_all(&dest);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&staging, &dest)
        .with_context(|| format!("无法把解压结果移动到 {}", dest.display()))?;

    progress(&format!(
        "运行时已就位：{}",
        dest.join(Lang::Python.exe()).display()
    ));
    Ok(dest)
}

// ─────────────────────────── `/runtime` 命令 ───────────────────────────

/// `/runtime` 的子命令。
///
/// 解析与执行都放在这里，CLI 与 GUI 只负责「把结果写到哪」——
/// 两边共用同一份逻辑与文案，不会出现行为分叉。
pub enum Cmd {
    Status,
    Dir,
    Install(Lang),
}

/// 解析一行 `/runtime ...`（调用前请先用 [`is_cmd`] 判定前缀）。
pub fn parse_cmd(line: &str) -> Result<Cmd> {
    let mut parts = line.split_whitespace();
    let _ = parts.next(); // "/runtime" 本身
    match parts.next() {
        None | Some("status") => Ok(Cmd::Status),
        Some("dir") => Ok(Cmd::Dir),
        Some("install") => match parts.next().unwrap_or("python") {
            "python" => Ok(Cmd::Install(Lang::Python)),
            "node" => bail!(NODE_MANUAL_HINT),
            other => bail!("不支持的运行时: {other}（可选：python）"),
        },
        Some(other) => bail!(
            "未知的 /runtime 子命令: {other}\n\
             用法: /runtime [status] | /runtime install [python] | /runtime dir"
        ),
    }
}

/// 这一行是不是 `/runtime` 命令（宿主自己处理，不发给模型）。
pub fn is_cmd(line: &str) -> bool {
    line == "/runtime" || line.starts_with("/runtime ")
}

/// 装配运行时，并在成功后**立刻让它生效**（`refresh_runtimes`，无需重启进程）。
/// 返回给人看的总结文本；`progress` 收到装配过程的一行行进度。
pub async fn install(
    rt: &std::sync::Arc<crate::runtime::Runtime>,
    lang: Lang,
    progress: &mut dyn FnMut(&str),
) -> Result<String> {
    match lang {
        Lang::Python => {
            let dir = install_python(&rt.root, &rt.config.runtimes, progress).await?;
            rt.refresh_runtimes();
            let ver = probe_version(&rt.runtimes().python)
                .unwrap_or_else(|| "（没能取到版本号）".into());
            Ok(format!(
                "Python 运行时已装配：{}\n  {ver}\n  \
                 已立即生效：之后新建的 python 插件直接用它；\
                 此前因缺少 python 而加载失败的插件，重启程序后会自动恢复。",
                dir.display()
            ))
        }
        other => bail!("{} 不支持自动装配", other.key()),
    }
}
