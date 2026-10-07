//! Plugin runtime "resolution" and "installation".
//!
//! ## 1. Interpreter resolution (three tiers, shared by CLI / GUI / selftest)
//!
//! 1. `config.toml`'s `[runtimes]` has an **explicit path** → use it directly, no guessing;
//! 2. The field is a **bare command** (`python` / `node` / `rustc`) and a built-in runtime
//!    exists under the app directory `<app_root>/runtime/<lang>/<exe>` → use the built-in one;
//! 3. Neither → keep the bare command and leave it to `PATH`.
//!
//! Tier 2 is the fulcrum of this design: **dropping a runtime into the app directory makes it
//! take effect automatically**, with no config change needed, and we write nothing into the
//! system `PATH` or the user's environment variables (non-invasive, gone on uninstall).
//!
//! ## 2. Installation
//!
//! Only Python is installed automatically (a Node single file is over 80 MB, unfit for
//! bundling; rust plugins need nothing at runtime anyway).
//! Order: already exists → local offline pack → download (the offline pack and non-default
//! sources need no network).
//!
//! **Deliberately introduces no new crate**:
//! - Downloading reuses `reqwest`, already in the dependency tree (`chunk()` needs no extra feature);
//! - Extraction uses the system's own `tar.exe` (bsdtar, Win10 1803+), falling back to PowerShell
//!   `Expand-Archive`;
//! - Verification uses the system's own `certutil -hashfile ... SHA256`.
//!
//! All three paths are "capabilities the system already has", so the installation chain adds no
//! size or dependency to the host.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::config::RuntimesConfig;
use crate::plugin::ProcessFlags;

/// The agreed filename of the offline runtime pack.
/// Placing it in the **exe's directory** / the app root / `<app_root>/runtime` /
/// `%LOCALAPPDATA%\AOX` gets it discovered automatically — a fully offline installation path.
pub const PACK_NAME: &str = "aox-python-runtime.zip";

/// The default download source: CPython's official "embeddable distribution" (about 10 MB, ~21 MB extracted).
pub const DEFAULT_PYTHON_URL: &str =
    "https://www.python.org/ftp/python/3.13.7/python-3.13.7-embed-amd64.zip";

/// The official SHA-256 that goes with [`DEFAULT_PYTHON_URL`].
/// Applies only to the default source; if you use a custom `python_url` and still want
/// verification, fill in `python_sha256` yourself under `[runtimes]`.
pub const DEFAULT_PYTHON_SHA256: &str =
    "f6cca216a359be84797cabb54149ce5e062afb16cc7567eb7fc51cacb2d86b65";

/// The reason Node auto-installation is explicitly refused (size).
pub const NODE_MANUAL_HINT: &str =
    "The Node runtime is over 80 MB as a single file, so it is not suitable for auto-install. Install Node.js \
     yourself and make sure `node` is on PATH, or write its full path under [runtimes] node in config.toml.";

// ─────────────────────────── Languages ───────────────────────────

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

    /// The executable filename inside the built-in runtime directory.
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

// ─────────────────────────── Status ───────────────────────────

/// An interpreter's origin, used by the `/runtime` report and the system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A path hard-coded in `config.toml`
    Config,
    /// The built-in runtime under the app directory
    Builtin,
    /// Found in `PATH`
    Path,
    /// Not found
    Missing,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Config => crate::i18n::t("set in config.toml"),
            Origin::Builtin => crate::i18n::t("built into the app directory"),
            Origin::Path => "PATH",
            Origin::Missing => crate::i18n::t("not found"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Status {
    pub lang: Lang,
    /// The command finally used (an absolute path or a bare command)
    pub command: String,
    pub origin: Origin,
}

impl Status {
    pub fn ok(&self) -> bool {
        self.origin != Origin::Missing
    }
}

/// A one-line textual representation of the status. The CLI's `/runtime` report and the GUI's
/// About window share the same wording.
pub fn one_line(st: &Status) -> String {
    format!(
        "{} {:<7} {}  [{}]",
        if st.ok() { "✓" } else { "✗" },
        st.lang.key(),
        st.command,
        st.origin.label()
    )
}

// ─────────────────────────── Resolution ───────────────────────────

/// A "bare command" = containing no path component, hence safe to be replaced by the built-in
/// runtime.
///
/// Values containing `/`, `\` or `:` (a drive letter) are all treated as paths the user
/// explicitly specified and are never overwritten.
fn is_bare(cmd: &str) -> bool {
    let t = cmd.trim();
    !t.is_empty() && !t.contains(['/', '\\', ':'])
}

/// The built-in runtime directory: `<app_root>/runtime/<lang>`.
pub fn builtin_dir(root: &Path, lang: Lang) -> PathBuf {
    root.join("runtime").join(lang.key())
}

/// The built-in runtime's executable; returned only when it really exists.
pub fn builtin_exe(root: &Path, lang: Lang) -> Option<PathBuf> {
    let p = builtin_dir(root, lang).join(lang.exe());
    p.is_file().then_some(p)
}

/// Look up an executable in `PATH` — the standard library has no `which`, so here are a dozen lines.
fn find_in_path(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    // On Windows, search a common subset of PATHEXT; other platforms have only the bare name.
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

/// The three-tier resolution, rewriting `cfg` in place.
///
/// The built-in runtime only overrides when "the config value is a bare command" — a path the
/// user wrote explicitly is always respected.
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

/// Static judgment only, **starting no subprocess** — this path is walked on every startup and
/// must not be slowed down for it.
///
/// Note: `cfg` must be the **raw** config (not passed through [`resolve`]).
/// Only the raw value lets us tell whether the "origin" is `config.toml` or `built-in under the
/// app directory`; passing the resolved snapshot would mislabel every built-in runtime as the
/// former.
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

/// Run `--version` once to get the version line. Called only when the user explicitly queries
/// `/runtime`.
pub fn probe_version(command: &str) -> Option<String> {
    let mut cmd = Command::new(command);
    cmd.arg("--version");
    cmd.hide_console();
    let out = cmd.output().ok()?;
    let text = if out.stdout.is_empty() { out.stderr } else { out.stdout };
    let line = String::from_utf8_lossy(&text).lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

// ─────────────────────────── User-facing wording ───────────────────────────

/// The `/runtime` report text.
pub fn status_report(root: &Path, cfg: &RuntimesConfig) -> String {
    let mut s = String::from(crate::i18n::t("Plugin runtimes:\n"));
    for st in status(root, cfg) {
        s.push_str("  ");
        s.push_str(&one_line(&st));
        // Probe the version only when it is really usable, to avoid spawning a process for a
        // path that was never found
        if st.ok() {
            if let Some(v) = probe_version(&st.command) {
                s.push_str(&format!("  ·  {v}"));
            }
        }
        s.push('\n');
    }
    s.push_str(&crate::tf!("  built-in runtime directory: {}\n", root.join("runtime").display()));
    s.push_str(&crate::tf!("  offline pack name:   {} (place it next to the exe or in the app root)\n", PACK_NAME));
    if let Some(p) = find_offline_pack(root) {
        s.push_str(&crate::tf!("  offline pack found:   {}\n", p.display()));
    }
    s
}

/// The one-line hint for the startup banner; returns `None` when Python is ready, keeping
/// startup output clean.
pub fn missing_hint(root: &Path, cfg: &RuntimesConfig) -> Option<String> {
    let st = status(root, cfg);
    let py = st.iter().find(|s| s.lang == Lang::Python)?;
    if py.ok() {
        return None;
    }
    Some(crate::tf!(
        "Python was not detected, so python plugins are unavailable (file I/O, system info and other capabilities all depend on it). \
         Run /runtime install python to install a bundled runtime (about 10 MB, into {}).",
        builtin_dir(root, Lang::Python).display()
    ))
}

/// The "run environment" segment written into the system prompt, to keep the model from
/// repeatedly creating doomed plugins.
pub fn prompt_section(root: &Path, cfg: &RuntimesConfig) -> String {
    let st = status(root, cfg);
    // The leading newline is required: this segment is concatenated right after the end of the
    // previous one (which ends with a period)
    let mut s = String::from(crate::i18n::t("\n- Available plugin runtimes:"));
    for x in &st {
        s.push_str("\n  - ");
        s.push_str(x.lang.key());
        s.push_str(": ");
        if x.ok() {
            s.push_str(&crate::tf!("{} ({})", x.command, x.origin.label()));
        } else {
            s.push_str(crate::i18n::t("missing"));
        }
    }
    let py_missing = st.iter().any(|x| x.lang == Lang::Python && !x.ok());
    let rustc_missing = st.iter().any(|x| x.lang == Lang::Rustc && !x.ok());
    if py_missing {
        s.push_str(
            crate::i18n::t("\n- **No Python on this machine**: do not create a plugin with language=\"python\" — it will certainly fail. Use language=\"rust\" instead (a compiled artifact needs no runtime at run time); or ask the user to run /runtime install python to install the bundled runtime, then retry."),
        );
    }
    if rustc_missing {
        s.push_str(
            crate::i18n::t("\n- No rustc on this machine: **creating** a rust plugin will fail, but an already-compiled rust plugin loads and runs fine."),
        );
    }
    s
}

// ─────────────────────────── Offline pack discovery ───────────────────────────

/// The directory holding the current process's exe.
///
/// `crate::paths::exe_dir()` is not reused here: `src/paths.rs` hangs off lib only (the CLI's bin
/// root doesn't declare it), whereas this module is compiled by both bin and lib — keeping zero
/// module coupling is what keeps the behaviour consistent.
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// Offline pack candidate locations, by priority: exe directory → app root → app root/runtime →
/// %LOCALAPPDATA%\AOX.
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

// ─────────────────────────── System-capability wrappers ───────────────────────────

/// Dependency-free SHA-256: calls the system's own `certutil`.
/// The only place in the installation chain that needs a hash, so it isn't worth pulling in `sha2`.
fn sha256_of(path: &Path) -> Result<String> {
    let mut cmd = Command::new("certutil");
    cmd.arg("-hashfile").arg(path).arg("SHA256");
    cmd.hide_console();
    let out = cmd
        .output()
        .context(crate::i18n::t("failed to compute the checksum with certutil (is certutil missing?)"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    // certutil prints three lines: a heading / the hash (possibly with spaces) / a completion
    // note — only the 64-digit hex line counts.
    text.lines()
        .map(|l| l.trim().replace(' ', ""))
        .find(|l| l.len() == 64 && l.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|l| l.to_lowercase())
        .ok_or_else(|| anyhow!(crate::tf!("could not parse SHA-256 from the certutil output:\n{}", text.trim())))
}

/// Extract a zip. Prefers the system's own `tar.exe` (bsdtar), falling back to PowerShell.
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
    let out = ps.output().context(crate::i18n::t("extraction failed: neither tar nor PowerShell could be invoked"))?;
    if !out.status.success() {
        bail!(crate::tf!(
            "failed to extract {}:\n{}",
            zip.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Some distributions stuff the content into a single top-level directory; after extraction,
/// flatten one level so `<dest>/python.exe` lands in place.
fn flatten_single_dir(dest: &Path) -> Result<()> {
    if dest.join(Lang::Python.exe()).is_file() {
        return Ok(());
    }
    let dirs: Vec<_> = std::fs::read_dir(dest)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    if dirs.len() != 1 {
        return Ok(()); // Not this shape; leave it to the existence check later to error
    }
    let inner = dirs[0].path();
    for e in std::fs::read_dir(&inner)? {
        let e = e?;
        std::fs::rename(e.path(), dest.join(e.file_name()))?;
    }
    let _ = std::fs::remove_dir(&inner);
    Ok(())
}

// ─────────────────────────── Download ───────────────────────────

async fn download(url: &str, dest: &Path, progress: &mut dyn FnMut(&str)) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()?;
    let mut resp = client
        .get(url)
        .send()
        .await
        .with_context(|| crate::tf!("request failed: {}", url))?
        .error_for_status()
        .with_context(|| crate::tf!("the server rejected the request: {}", url))?;

    let total = resp.content_length();
    let mut file =
        std::fs::File::create(dest).with_context(|| crate::tf!("cannot create {}", dest.display()))?;
    let mut got: u64 = 0;
    let mut last_bucket = u64::MAX;
    // Write to disk chunk by chunk: `chunk()` needs no extra feature, and a synchronous File is
    // fine because installation happens only once and what blocks is the agent thread, not the UI
    // thread.
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk)?;
        got += chunk.len() as u64;
        if let Some(t) = total {
            let pct = got.saturating_mul(100) / t.max(1);
            if pct / 10 != last_bucket {
                last_bucket = pct / 10;
                progress(&crate::tf!(
                    "Downloading {}% ({}/{} MB)",
                    pct,
                    format!("{:.1}", got as f64 / 1048576.0),
                    format!("{:.1}", t as f64 / 1048576.0)
                ));
            }
        }
    }
    file.flush()?;
    if let Some(t) = total {
        if got != t {
            bail!(crate::tf!("incomplete download: received {} bytes, Content-Length was {}", got, t));
        }
    }
    Ok(())
}

// ─────────────────────────── Installation ───────────────────────────

/// Verify (optional) → extract → flatten → existence check.
fn unpack(zip: &Path, staging: &Path, expect_sha: Option<&str>, downloaded: bool, progress: &mut dyn FnMut(&str)) -> Result<()> {
    if let Some(expect) = expect_sha {
        let expect = expect.trim();
        if !expect.is_empty() {
            progress(crate::i18n::t("verifying SHA-256…"));
            let got = sha256_of(zip)?;
            if !got.eq_ignore_ascii_case(expect) {
                if downloaded {
                    // Don't keep a corrupted/tampered pack, or the next run hits the same pit
                    let _ = std::fs::remove_file(zip);
                }
                bail!(
                    crate::tf!("runtime pack verification failed:\n  expected {}\n  actual {}\n(if this is a pack you host yourself, update python_sha256 under [runtimes])", expect, got)
                );
            }
            progress(crate::i18n::t("verification passed"));
        } else {
            progress(crate::i18n::t("python_sha256 is not configured; skipping verification"));
        }
    } else {
        progress(crate::i18n::t("python_sha256 is not configured; skipping verification"));
    }
    progress(crate::i18n::t("extracting…"));
    extract_zip(zip, staging)?;
    flatten_single_dir(staging)?;
    if !staging.join(Lang::Python.exe()).is_file() {
        bail!(crate::tf!("{} was not found after extraction; the pack layout may be wrong", Lang::Python.exe()));
    }
    Ok(())
}

/// Install the Python runtime to `<app_root>/runtime/python` and return that directory.
///
/// Order: already exists → local offline pack → download. `progress` receives line-by-line
/// progress text meant for humans (the CLI prints it; the GUI discards it or routes it to the
/// activity stream).
pub async fn install_python(
    root: &Path,
    cfg: &RuntimesConfig,
    progress: &mut dyn FnMut(&str),
) -> Result<PathBuf> {
    let dest = builtin_dir(root, Lang::Python);
    if builtin_exe(root, Lang::Python).is_some() {
        progress(&crate::tf!("the built-in Python runtime already exists: {}", dest.display()));
        return Ok(dest);
    }

    let staging = root.join("runtime").join("python.staging");
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }

    match find_offline_pack(root) {
        // ---- 1) Local offline pack: never touches the network
        Some(pack) => {
            progress(&crate::tf!("offline runtime pack found: {}", pack.display()));
            unpack(&pack, &staging, cfg.python_sha256.as_deref(), false, progress)?;
        }
        // ---- 2) Download
        None => {
            let url = cfg
                .python_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_PYTHON_URL)
                .to_string();
            // Checksum: explicit config wins; otherwise the built-in checksum applies only to the
            // default official source
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
            progress(&crate::tf!("downloading runtime: {}", url));
            download(&url, &zip, progress).await?;
            let res = unpack(&zip, &staging, expect.as_deref(), true, progress);
            // Clear the download cache once installation is done (success or not), leaving no
            // 10 MB wreck in the app directory
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
        .with_context(|| crate::tf!("cannot move the extracted result to {}", dest.display()))?;

    progress(&crate::tf!(
        "the runtime is in place: {}",
        dest.join(Lang::Python.exe()).display()
    ));
    Ok(dest)
}

// ─────────────────────────── The `/runtime` command ───────────────────────────

/// The `/runtime` subcommands.
///
/// Parsing and execution both live here; the CLI and GUI only decide "where the result gets
/// written" — both share one set of logic and wording, so the behaviour can't fork.
pub enum Cmd {
    Status,
    Dir,
    Install(Lang),
}

/// Parse one `/runtime ...` line (check the prefix with [`is_cmd`] first).
pub fn parse_cmd(line: &str) -> Result<Cmd> {
    let mut parts = line.split_whitespace();
    let _ = parts.next(); // "/runtime" itself
    match parts.next() {
        None | Some("status") => Ok(Cmd::Status),
        Some("dir") => Ok(Cmd::Dir),
        Some("install") => match parts.next().unwrap_or("python") {
            "python" => Ok(Cmd::Install(Lang::Python)),
            "node" => bail!(crate::i18n::t(NODE_MANUAL_HINT)),
            other => bail!(crate::tf!("unsupported runtime: {} (available: python)", other)),
        },
        Some(other) => bail!(
            crate::tf!("Unknown /runtime subcommand: {}\nUsage: /runtime [status] | /runtime install [python] | /runtime dir", other)
        ),
    }
}

/// Whether this line is a `/runtime` command (handled by the host, not sent to the model).
pub fn is_cmd(line: &str) -> bool {
    line == "/runtime" || line.starts_with("/runtime ")
}

/// Install a runtime and, on success, **make it take effect immediately** (`refresh_runtimes`,
/// with no process restart).
/// Returns a summary text meant for humans; `progress` receives line-by-line installation progress.
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
                .unwrap_or_else(|| crate::i18n::t("(could not get the version number)").into());
            Ok(crate::tf!(
                "Python runtime installed: {}\n  {}\n  It takes effect immediately: newly created python plugins use it; plugins that previously failed to load for lack of python recover automatically after a restart.",
                dir.display(),
                ver
            ))
        }
        other => bail!(crate::tf!("{} does not support auto-install", other.key())),
    }
}
