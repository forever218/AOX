//! Build script: write the icon and version info into the Windows resources of `aox-gui.exe`.
//! Same approach as the root crate's `build.rs` (call the Windows SDK's rc.exe directly rather than winresource —...
//! Why this is mandatory: an exe with no resource section is a blank sheet in Explorer, the Start menu...

use std::path::{Path, PathBuf};
use std::process::Command;

/// SDK directories known to exist on this machine (matching the version used by .cargo/config.toml).
const SDK_BIN_CANDIDATES: &[&str] = &[
    r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64",
    r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64",
];

fn main() {
    if !cfg!(windows) {
        return;
    }

    // Distribution safety gate: on Windows the CRT must be linked statically; the reasoning and criteria live...
    if !target_features().iter().any(|f| f == "crt-static") {
        println!(
            "cargo:warning=AOX GUI built without static CRT: the artifact will depend on VCRUNTIME140.dll \
             and will fail to start on Windows without the VC++ runtime installed. \
             Add rustflags = [\"-C\", \"target-feature=+crt-static\"] under \
             [target.x86_64-pc-windows-msvc] in .cargo/config.toml, or rebuild with the \
             environment variable RUSTFLAGS=\"-C target-feature=+crt-static\"."
        );
    }

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // The icon lives in the repo root, not under gui
    let ico = manifest_dir.join("..").join("assets").join("aox.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    println!("cargo:rerun-if-changed=build.rs");

    let Some(rc_exe) = find_rc() else {
        println!("cargo:warning=rc.exe from the Windows SDK not found; skipping the exe icon");
        return;
    };

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());

    let icon_line = if ico.exists() {
        // Backslashes inside the .rc file must be escaped
        format!(
            "1 ICON \"{}\"\n\n",
            ico.display().to_string().replace('\\', "\\\\")
        )
    } else {
        println!("cargo:warning=assets/aox.ico does not exist; writing the version resource only (run tools/make_icon.py to generate it)");
        String::new()
    };

    // FILEVERSION needs four plain numbers "a,b,c,d"
    let mut nums: Vec<u32> = version
        .split('.')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    nums.truncate(4);
    while nums.len() < 4 {
        nums.push(0);
    }
    let ver_commas = nums
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");

    // The string block uses 040904b0 (English + Unicode), so the description text here stays ASCII
    // to keep rc.exe from misreading it under the local code page.
    let rc_text = format!(
        r#"{icon_line}1 VERSIONINFO
FILEVERSION {ver_commas}
PRODUCTVERSION {ver_commas}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "FileDescription", "AOX GUI - graphical front-end for the AOX agent"
      VALUE "FileVersion", "{version}.0"
      VALUE "InternalName", "aox-gui"
      VALUE "OriginalFilename", "aox-gui.exe"
      VALUE "ProductName", "AOX GUI"
      VALUE "ProductVersion", "{version}.0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );

    let rc_src = out_dir.join("aox-gui.rc");
    if let Err(e) = std::fs::write(&rc_src, rc_text) {
        println!("cargo:warning=failed to write the .rc file; skipping the exe icon: {e}");
        return;
    }

    let res = out_dir.join("aox-gui.res");
    match Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg(&rc_src)
        .output()
    {
        Ok(o) if o.status.success() => {
            // Hand the .res to link.exe (the MSVC linker accepts .res as an input)
            println!("cargo:rustc-link-arg-bins={}", res.display());
        }
        Ok(o) => println!(
            "cargo:warning=rc.exe failed to compile the resource (does not affect functionality): {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => println!("cargo:warning=could not run rc.exe (does not affect functionality): {e}"),
    }
}

/// The list of rustc features currently enabled on the target. Implementation matches the root crate's `build.rs`.
fn target_features() -> Vec<String> {
    if let Ok(v) = std::env::var("CARGO_CFG_TARGET_FEATURE") {
        return v.split(',').map(|s| s.trim().to_string()).collect();
    }
    let mut v = vec![];
    if cfg!(target_feature = "crt-static") {
        v.push("crt-static".to_string());
    }
    v
}

/// Locate rc.exe: check known directories first, then scan Windows Kits (take the highest version).
fn find_rc() -> Option<PathBuf> {
    for dir in SDK_BIN_CANDIDATES {
        let p = Path::new(dir).join("rc.exe");
        if p.exists() {
            return Some(p);
        }
    }
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut found: Vec<PathBuf> = std::fs::read_dir(kits)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("x64").join("rc.exe").exists())
        .collect();
    found.sort();
    found.pop().map(|v| v.join("x64").join("rc.exe"))
}
