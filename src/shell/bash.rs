//! Finding a **bash** on this machine, and making it usable from a Windows host.
//!
//! Why a separate tool at all: for an agent, POSIX syntax is the difference between one call and five —
//! pipes, globs, `$(…)`, `sed`/`grep`/`find`. Measured on this machine, a `bash -c true` costs ~180 ms
//! more than a `cmd` (318 ms vs a 136 ms floor) while `powershell -NoProfile -Command exit` costs
//! ~1100 ms more; so bash is both cheaper per call and expressively denser. `shell` stays as the
//! fallback for what bash cannot do (a cmd builtin, a PowerShell object model).
//!
//! # Where the bash comes from
//!
//! In this order, and **file existence only** — nothing is executed during detection, because that
//! would put a third of a second on every startup:
//!
//! 1. `[shell] bash = "<absolute path>"` (explicit wins, and is the escape hatch);
//! 2. `<app root>\runtime\bash\bash.exe` — the sidecar pack, when `[shell] bash_pack = "runtime"`;
//! 3. `%ProgramFiles%\Git\bin\bash.exe`, `%ProgramFiles(x86)%\…`, `%LOCALAPPDATA%\Programs\Git\…`;
//! 4. derived from `git` on `PATH` (`<git>\cmd\git.exe` → `<git>\bin\bash.exe`);
//! 5. a `bash.exe` on `PATH` — **except** anything under `\Microsoft\WindowsApps\`, which is the WSL
//!    launcher and runs a completely different file system.
//!
//! The WSL check is not only a path filter: [`Bash::check`] runs `uname -s` on the first real call and
//! refuses anything that does not say MINGW/MSYS/CYGWIN. A path filter alone would not survive a
//! symlink or a shim.
//!
//! # The PATH trap
//!
//! Measured: handing MSYS a Windows-style `PATH` (`C:\Windows\System32;…`) makes it treat each entry as
//! a **relative** POSIX path and glue it under its own root — `C:/d/…/PortableGit/Windows/System32` —
//! after which `cargo` is simply missing. So the host converts every entry to `/c/…` form itself and
//! prepends the bash's own `usr\bin` + `mingw64\bin`. Getting this wrong is the difference between
//! "bash works" and "bash cannot see any tool you installed".

use std::path::{Path, PathBuf};

use crate::config::ShellConfig;

/// A usable bash-like shell.
#[derive(Debug, Clone)]
pub struct Bash {
    pub exe: PathBuf,
    /// Arguments that must precede the script (`["sh"]` for busybox, empty for bash).
    pub prefix: Vec<String>,
    /// A word for the output header: `git-bash`, `busybox`, `custom`.
    pub flavour: &'static str,
    /// The POSIX form of the git root (`/c/Program Files/Git`), when it can be derived — used to put
    /// `usr/bin` and `mingw64/bin` on PATH.
    pub posix_root: Option<String>,
}

impl Bash {
    /// The program and arguments for one script.
    pub fn command_line(&self, script: &str) -> (String, Vec<String>) {
        let mut args: Vec<String> = Vec::new();
        if self.prefix.is_empty() {
            // No user dotfiles: a `.bashrc` that prints a banner or `cd`s elsewhere would make every
            // call non-reproducible.
            args.push("--noprofile".into());
            args.push("--norc".into());
        } else {
            args.extend(self.prefix.iter().cloned());
        }
        args.push("-c".into());
        args.push(script.to_string());
        (self.exe.display().to_string(), args)
    }

    /// Refuse a `bash.exe` that is not an MSYS/Cygwin shell. Runs `uname -s` — one process, so this is
    /// done on the first **call**, not during detection.
    ///
    /// Bounded on purpose: `Command::output()` waits forever, and this runs **before** any of the
    /// call's timeouts are armed (`runner::run` calls it first), so a shim that hangs here would hang
    /// the whole tool call with nothing counting down. Five seconds is ~20× the measured cost of a
    /// real bash (`uname -s` ≈ 270 ms including the Git Bash start).
    pub fn check(&self) -> Result<(), String> {
        let mut child = std::process::Command::new(&self.exe)
            .args(&self.prefix)
            .args(["-c", "uname -s"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| crate::tf!("could not run {}: {}", self.exe.display(), e))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut timed_out = false;
        loop {
            match child.try_wait() {
                // Exited (and reaped) — or the handle is gone, which also means "nothing to wait for".
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {}
            }
            if std::time::Instant::now() >= deadline {
                timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if timed_out {
            return Err(crate::tf!(
                "{} did not answer `uname -s` within 5 s — refusing it rather than hanging the first \
                 command that uses it",
                self.exe.display()
            ));
        }
        // Read what it printed. The pipe is bounded by the answer's size (`uname -s` is one line), and
        // the process has already exited, so this cannot block on a live writer.
        let mut buf = Vec::new();
        if let Some(mut out) = child.stdout.take() {
            use std::io::Read as _;
            let _ = out.read_to_end(&mut buf);
        }
        let s = String::from_utf8_lossy(&buf).to_ascii_uppercase();
        // `Windows_NT` is what busybox-w32 answers — it is a native Win32 program with POSIX tools, not
        // an MSYS runtime. Accepting it here is what lets the ash fallback work at all.
        if s.contains("MINGW") || s.contains("MSYS") || s.contains("CYGWIN") || s.contains("WINDOWS_NT") {
            return Ok(());
        }
        Err(crate::tf!(
            "{} does not look like a Windows bash: `uname -s` said {:?}. The WSL launcher runs a \
             different file system, so it is refused.",
            self.exe.display(),
            s.trim()
        ))
    }
}

/// Convert a Windows path to its POSIX form (`C:\a\b` → `/c/a/b`).
///
/// A UNC path comes out as `//server/share`, which is how MSYS addresses a share, so it keeps its
/// meaning. A **relative** path returns `None`: there is no POSIX form the host could guess without
/// inventing a base, and inventing one is how "it pointed somewhere else" bugs start.
pub fn posix(p: &Path) -> Option<String> {
    let s = p.to_string_lossy().replace('\\', "/");
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let drive = (b[0] as char).to_ascii_lowercase();
        return Some(format!("/{drive}{}", &s[2..]));
    }
    if s.starts_with('/') {
        return Some(s);
    }
    None
}

/// The PATH a bash child should see: the bash's own bin directories first, then every inherited entry
/// converted to POSIX form.
pub fn path_env(bash: &Bash, inherited: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(root) = &bash.posix_root {
        parts.push(format!("{root}/usr/bin"));
        parts.push(format!("{root}/mingw64/bin"));
    }
    parts.push("/c/Windows/System32".to_string());
    parts.push("/c/Windows".to_string());
    for entry in inherited.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some(p) = posix(Path::new(entry)) {
            if !parts.contains(&p) {
                parts.push(p);
            }
        }
    }
    parts.join(":")
}

/// A short prologue prepended to every bash script.
///
/// Two variables only — deliberately **not** `set -e` or `set -o pipefail`: those change what the
/// model's own script means, and a host that quietly changes semantics is worse than one that exposes
/// two names. The export forms are separated so ash (busybox) accepts them too.
pub fn prologue(ws: &Path) -> String {
    let win = ws.display().to_string().replace('\'', "");
    // Forward slashes, **not** `/e/...`: measured, busybox-w32 accepts `E:/rustapp/Cargo.toml` but not
    // `/e/rustapp/...` (it does no drive-letter mapping for arguments, only for PATH entries). MSYS bash
    // accepts both forms, so `E:/...` is the one value that works everywhere.
    let flat = win.replace('\\', "/");
    format!(
        "AOX_WORKSPACE='{win}'; export AOX_WORKSPACE\nAOX_WORKSPACE_U='{flat}'; export AOX_WORKSPACE_U\n"
    )
}

/// Find a bash, without running anything.
pub fn detect(cfg: &ShellConfig, app_root: &Path) -> Option<Bash> {
    let setting = cfg.bash.trim();
    if setting.eq_ignore_ascii_case("off") {
        return None;
    }
    if !setting.eq_ignore_ascii_case("auto") {
        let p = PathBuf::from(setting);
        return p.is_file().then(|| from_exe(p));
    }
    // ① the machine's own shell: a real bash 5.x beats anything this project could ship, and costs
    //    no bytes to distribute.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        candidates.push(PathBuf::from(&pf).join("Git").join("bin").join("bash.exe"));
    }
    if let Some(pf) = std::env::var_os("ProgramFiles(x86)") {
        candidates.push(PathBuf::from(&pf).join("Git").join("bin").join("bash.exe"));
    }
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(&la).join("Programs").join("Git").join("bin").join("bash.exe"),
        );
    }
    // Derived from `git` on PATH: <git>\cmd\git.exe → <git>\bin\bash.exe
    if let Some(git) = which("git.exe") {
        if let Some(root) = git.parent().and_then(|p| p.parent()) {
            candidates.push(root.join("bin").join("bash.exe"));
        }
    }
    if let Some(b) = which("bash.exe") {
        candidates.push(b);
    }
    for c in candidates {
        if !c.is_file() {
            continue;
        }
        // The WSL stub lives in WindowsApps and starts a Linux VM; never take it. Separators are
        // normalised first (a PATH-derived candidate may use `/`), and the target must carry data —
        // both are the same rule `runtimes::usable` applies, for the same measured reason.
        let lowered = c.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
        if lowered.contains("\\windowsapps\\") {
            continue;
        }
        let has_data = std::fs::metadata(&c).map(|m| m.is_file() && m.len() > 0).unwrap_or(false);
        if !has_data {
            continue;
        }
        return Some(from_exe(c));
    }

    // ② the sidecar (`[shell] bash_pack`). A real `bash.exe` dropped here is used before busybox,
    //    because busybox is an *ash*: no arrays, no `[[ ]]`, and a subset of the applet options. It is
    //    the last resort, never the first choice.
    if cfg.bash_pack.trim().eq_ignore_ascii_case("runtime") {
        let dir = app_root.join("runtime").join("bash");
        let real = dir.join("bash.exe");
        if real.is_file() {
            return Some(from_exe(real));
        }
        let busybox = dir.join("busybox.exe");
        if busybox.is_file() {
            return Some(Bash {
                exe: busybox,
                prefix: vec!["sh".into()],
                flavour: "busybox",
                posix_root: None,
            });
        }
    }
    None
}

/// Detect **and validate** a bash, remembering the verdict per executable path.
///
/// The validation runs `uname -s` (one short process). Doing it once per executable keeps the cost off
/// every call while still refusing a WSL shim or a broken install with a sentence that says why.
pub fn validated(cfg: &ShellConfig, app_root: &Path) -> Result<Bash, String> {
    let b = detect(cfg, app_root).ok_or_else(|| {
        crate::i18n::t(
            "no bash was found: put one in <app root>/runtime/bash/, install Git for Windows, or set \
             [shell] bash to the path of a bash.exe",
        )
        .to_string()
    })?;
    if checked().lock().unwrap_or_else(|e| e.into_inner()).contains(&b.exe) {
        return Ok(b);
    }
    b.check()?;
    checked().lock().unwrap_or_else(|e| e.into_inner()).insert(b.exe.clone());
    Ok(b)
}

fn checked() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

fn from_exe(exe: PathBuf) -> Bash {
    // `<root>\bin\bash.exe` → `<root>`; anything else gets no derived root.
    let posix_root = exe
        .parent()
        .filter(|p| p.file_name().map(|n| n.eq_ignore_ascii_case("bin")).unwrap_or(false))
        .and_then(|bin| bin.parent())
        .and_then(posix);
    // A busybox is *driven* (`busybox sh -c …`), not a bash: without the `sh` prefix it would be asked
    // for `--noprofile`, which it does not have. An explicit `[shell] bash = "...busybox.exe"` lands here
    // too, so this has to be decided from the file name.
    let is_busybox = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase().contains("busybox"))
        .unwrap_or(false);
    Bash {
        exe,
        prefix: if is_busybox { vec!["sh".into()] } else { Vec::new() },
        flavour: if is_busybox { "busybox" } else { "git-bash" },
        posix_root: if is_busybox { None } else { posix_root },
    }
}

/// Minimal `which`: walk `PATH`, no shell involved.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_paths_become_posix_ones() {
        assert_eq!(posix(Path::new(r"C:\a\b")).as_deref(), Some("/c/a/b"));
        assert_eq!(posix(Path::new(r"E:\rustapp")).as_deref(), Some("/e/rustapp"));
        assert_eq!(posix(Path::new(r"C:\Program Files\Git")).as_deref(), Some("/c/Program Files/Git"));
        assert_eq!(posix(Path::new("C:")).as_deref(), Some("/c"));
        // UNC keeps its meaning: `\\server\share` becomes `//server/share`, which is exactly how MSYS
        // addresses a network share. A *relative* path, on the other hand, has no POSIX meaning the host
        // could guess, so it is dropped rather than faked.
        assert_eq!(posix(Path::new(r"\\server\share")).as_deref(), Some("//server/share"));
        assert_eq!(posix(Path::new("relative\\dir")), None);
    }

    #[test]
    fn the_path_puts_the_bash_bins_first_and_converts_the_rest() {
        let b = Bash {
            exe: PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
            prefix: Vec::new(),
            flavour: "git-bash",
            posix_root: Some("/c/Program Files/Git".to_string()),
        };
        let got = path_env(&b, r"C:\Windows\System32;C:\Users\me\.cargo\bin");
        assert!(got.starts_with("/c/Program Files/Git/usr/bin:"), "{got}");
        assert!(got.contains("/c/Users/me/.cargo/bin"), "{got}");
        // No Windows-style entry may survive: MSYS would mangle it into a relative path.
        assert!(!got.to_ascii_lowercase().contains("c:\\"), "{got}");
        assert!(!got.contains(';'), "entries must be ':'-separated now: {got}");
    }

    #[test]
    fn the_prologue_exports_both_forms_of_the_workspace() {
        let p = prologue(Path::new(r"E:\rustapp\workplace\demo"));
        assert!(p.contains("AOX_WORKSPACE='E:\\rustapp\\workplace\\demo'"), "{p}");
        // Forward slashes and a drive letter: the one form busybox and MSYS bash both accept.
        assert!(p.contains("AOX_WORKSPACE_U='E:/rustapp/workplace/demo'"), "{p}");
        // No `set -e` / `pipefail`: the host must not silently change what the script means.
        assert!(!p.contains("set -e") && !p.contains("pipefail"), "{p}");
    }
}
