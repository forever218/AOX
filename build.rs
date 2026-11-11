//! Build script: writes the icon and version information into the Windows exe resources.
//! It calls the Windows SDK's rc.exe to produce a.res and hands it to the linker, rather than using winresource —...

use std::path::{Path, PathBuf};
use std::process::Command;

/// SDK directories known to exist on this machine (the same versions `.cargo/config.toml` uses).
const SDK_BIN_CANDIDATES: &[&str] = &[
    r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64",
    r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64",
];

const VERSION: &str = "0.1.0";

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // The language registry is **platform independent**, so do it first — non-Windows platforms
    // must be able to build every language variant too.
    emit_lang(&manifest_dir, &out_dir);

    if !cfg!(windows) {
        return;
    }

    // Distribution safety gate: on Windows the CRT must be statically linked.
    if !target_features().iter().any(|f| f == "crt-static") {
        println!(
            "cargo:warning=AOX was built without a static CRT: the artifact will depend on \
             VCRUNTIME140.dll and cannot start on Windows without the VC++ runtime installed. \
             Please add rustflags = [\"-C\", \"target-feature=+crt-static\"] under \
             [target.x86_64-pc-windows-msvc] in .cargo/config.toml, or rebuild with the \
             environment variable RUSTFLAGS=\"-C target-feature=+crt-static\"."
        );
    }

    println!("cargo:rerun-if-changed=assets/aox.ico");
    println!("cargo:rerun-if-changed=build.rs");

    let ico = manifest_dir.join("assets").join("aox.ico");
    if !ico.exists() {
        println!("cargo:warning=assets/aox.ico does not exist, skipping the exe icon (run tools/make_icon.py to generate it)");
        return;
    }

    let Some(rc_exe) = find_rc() else {
        println!("cargo:warning=rc.exe from the Windows SDK was not found, skipping the exe icon");
        return;
    };

    // Backslashes inside the .rc need escaping.
    let ico_esc = ico.display().to_string().replace('\\', "\\\\");
    let rc_src = out_dir.join("aox.rc");
    let ver_commas = format!("{},0,0,0", VERSION.split('.').next().unwrap_or("0"));
    let rc_text = format!(
        r#"1 ICON "{ico_esc}"

1 VERSIONINFO
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
      VALUE "FileDescription", "AOX - self-growing AI agent"
      VALUE "FileVersion", "{VERSION}.0"
      VALUE "InternalName", "aox"
      VALUE "OriginalFilename", "aox.exe"
      VALUE "ProductName", "AOX"
      VALUE "ProductVersion", "{VERSION}.0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    if let Err(e) = std::fs::write(&rc_src, rc_text) {
        println!("cargo:warning=failed to write the .rc, skipping the exe icon: {e}");
        return;
    }

    let res = out_dir.join("aox.res");
    match Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg(&rc_src)
        .output()
    {
        Ok(o) if o.status.success() => {
            // Hand the .res to link.exe (the MSVC linker accepts .res as an input).
            println!("cargo:rustc-link-arg-bins={}", res.display());
        }
        Ok(o) => println!(
            "cargo:warning=rc.exe failed to compile the resource (does not affect functionality): {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => println!("cargo:warning=could not run rc.exe (does not affect functionality): {e}"),
    }
}

/// The target platform's currently enabled rustc features.
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

/// Locate rc.exe: check the known directories first, then scan Windows Kits (taking the highest
/// version).
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

/// The source language — the literals in the code *are* this variant's final wording, so it needs no catalogue.
const SOURCE_CODE: &str = "en";

/// Generate the language registry (which `src/i18n.rs` `include!`s).
fn emit_lang(manifest_dir: &Path, out_dir: &Path) {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=locales");
    println!("cargo:rerun-if-env-changed=AOX_LANG");

    let code = std::env::var("AOX_LANG")
        .unwrap_or_else(|_| "en".to_string())
        .trim()
        .to_ascii_lowercase();
    if code.is_empty() {
        panic!("AOX_LANG is empty: name at least one language, e.g. AOX_LANG=en or AOX_LANG=zh");
    }

    // The source language embeds no catalogue: the literals are the wording it wants.
    let catalog = if code == SOURCE_CODE {
        "\"\"".to_string()
    } else {
        let file = manifest_dir.join("locales").join(format!("{code}.toml"));
        if !file.exists() {
            panic!(
                "AOX_LANG=`{code}` has no matching catalogue: {}\n\
                 Adding a language takes three steps: ① locales/{code}.toml; ② add a Lang variant \
                 and a from_code arm in src/i18n.rs; ③ build with AOX_LANG={code}.",
                file.display()
            );
        }
        println!("cargo:rerun-if-changed={}", file.display());
        // `{:?}` yields the Rust-escaped string literal, which also handles Windows backslashes.
        format!("include_str!({:?})", file.display().to_string())
    };

    let src = format!(
        "// Generated by build.rs, **do not edit by hand** (edit build.rs or the AOX_LANG env var).\n\
         /// Which language this binary speaks.\n\
         pub const CODE: &str = {code:?};\n\n\
         /// The one catalogue this binary embedded; empty string for the source-language build.\n\
         pub const CATALOG: &str = {catalog};\n"
    );
    std::fs::write(out_dir.join("langs.rs"), src)
        .unwrap_or_else(|e| panic!("failed to write langs.rs: {e}"));
}
