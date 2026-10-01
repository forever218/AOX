//! 应用根目录 / 可写性 / 插件迁移等与「宿主进程运行位置」相关的公共逻辑。
//!
//! CLI（`src/main.rs`）与 GUI（`gui/`）都需要这套规则：把应用根归一化到同一个位置，
//! 避免 `cargo run`（exe 在 target/debug）与双击 exe 出现两套 `plugins/` 目录，
//! 也避免把 `config.toml` 写到不可写的位置导致闪退。
//!
//! 这里是从 `src/main.rs` 提取的同一套实现，行为完全一致。

use std::path::{Path, PathBuf};

/// 解析「应用根目录」。
///
/// 规则：从 exe 所在目录向上找最近的 Cargo.toml；找到就用它所在目录（开发态），
/// 否则就用 exe 所在目录（分发态）。
pub fn resolve_app_root(exe_dir: Option<&Path>) -> PathBuf {
    let Some(exe_dir) = exe_dir else {
        return PathBuf::from(".");
    };
    let mut cur: Option<&Path> = Some(exe_dir);
    while let Some(d) = cur {
        if d.join("Cargo.toml").exists() {
            return d.to_path_buf();
        }
        cur = d.parent();
    }
    exe_dir.to_path_buf()
}

/// 当前进程 exe 所在目录（拿不到则为 None）。
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&s, &d)?;
        } else if ty.is_file() {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

/// 把旧位置（exe 目录下）的插件迁到规范插件目录，避免历史插件「凭空消失」。
/// 只补不存在的，不覆盖已有文件；全部迁移成功后清理遗留的构建产物目录。
pub fn migrate_plugins(from: &Path, to: &Path) {
    if from == to || !from.is_dir() {
        return;
    }
    let mut failed = 0usize;
    if let Ok(entries) = std::fs::read_dir(from) {
        for e in entries.flatten() {
            let src = e.path();
            if !src.is_dir() {
                continue;
            }
            if e.file_name() == "sdk" {
                continue; // sdk 由宿主每次启动重写，不迁移
            }
            if !src.join("plugin.toml").is_file() {
                continue; // 跳过上次失败留下的空目录
            }
            let dst = to.join(e.file_name());
            if dst.exists() {
                continue;
            }
            if copy_dir(&src, &dst).is_err() {
                failed += 1;
            }
        }
    }
    let _ = std::fs::create_dir_all(to);
    if failed == 0 && to.is_dir() && from.components().any(|c| c.as_os_str() == "target") {
        let _ = std::fs::remove_dir_all(from);
    }
}

/// 目录是否可写（用一个探测文件判断）。
pub fn is_writable_dir(dir: &Path) -> bool {
    let probe = dir.join(".aox-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 保证应用根可写。AOX 运行时要在根目录下写 config.toml 与 plugins/，
/// 若被装到 Program Files 这类只读位置，就退回到用户可写的 %LOCALAPPDATA%\AOX。
pub fn ensure_writable_root(root: PathBuf) -> PathBuf {
    if is_writable_dir(&root) {
        return root;
    }
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return root;
    };
    let alt = PathBuf::from(local).join("AOX");
    if std::fs::create_dir_all(&alt).is_ok() && is_writable_dir(&alt) {
        alt
    } else {
        root
    }
}

/// 一步到位：解析出可写的应用根 + 插件目录（可被 --plugins 覆盖）。
pub fn resolve_workspace() -> (PathBuf, PathBuf) {
    let exe = exe_dir();
    let root = ensure_writable_root(resolve_app_root(exe.as_deref()));
    let plugins = root.join("plugins");
    if let Some(d) = exe.as_ref() {
        migrate_plugins(&d.join("plugins"), &plugins);
    }
    (root, plugins)
}

/// 在命令行参数里取 `--flag <value>` 形式的取值。
pub fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}
