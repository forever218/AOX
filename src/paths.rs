//! Shared logic tied to "where the host process runs": the app root directory, writability, plugin migration...
//! The CLI (`src/main.rs`) and the GUI (`gui/`) both need this rule set: normalise the app root to a single...
//! This is the same implementation extracted from `src/main.rs`, with identical behaviour.

use std::path::{Path, PathBuf};

/// Resolve the "app root directory".
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

/// The directory containing the current process's exe (None if it can't be obtained).
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

/// Migrate plugins from the old location (under the exe directory) into the canonical plugin directory...
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
                continue; // the sdk is rewritten by the host on every launch, so don't migrate it
            }
            if !src.join("plugin.toml").is_file() {
                continue; // skip the empty directories left behind by a previous failure
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

/// Whether a directory is writable (judged with a probe file).
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

/// Guarantee the app root is writable.
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

/// Resolve only the "writable app root", without migrating plugins.
pub fn app_root() -> PathBuf {
    ensure_writable_root(resolve_app_root(exe_dir().as_deref()))
}

/// One step to get both: the resolved writable app root + the plugin directory (overridable by --plugins).
pub fn resolve_workplace() -> (PathBuf, PathBuf) {
    let exe = exe_dir();
    let root = ensure_writable_root(resolve_app_root(exe.as_deref()));
    let plugins = root.join("plugins");
    if let Some(d) = exe.as_ref() {
        migrate_plugins(&d.join("plugins"), &plugins);
    }
    (root, plugins)
}

/// Resolve the config file path: `--config <path>` takes priority, otherwise `config.toml` under the app root.
pub fn resolve_config_path(args: &[String]) -> PathBuf {
    match arg_value(args, "--config") {
        Some(p) => PathBuf::from(p),
        None => app_root().join("config.toml"),
    }
}

/// Fetch a `--flag <value>` style value from the command-line arguments.
pub fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

/// Resolve the composition-manifest path: an explicit override via `--manifest <path>`, returning `None`...
pub fn resolve_manifest_path(args: &[String]) -> Option<PathBuf> {
    arg_value(args, "--manifest").map(PathBuf::from)
}

/// Resolve `--profile <name>`: temporarily switch to another profile without modifying `aox.toml`.
pub fn resolve_profile(args: &[String]) -> Option<String> {
    arg_value(args, "--profile")
}
