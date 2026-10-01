//! 构建脚本：把图标与版本信息写进 Windows exe 资源。
//!
//! 直接调用 Windows SDK 的 rc.exe 生成 .res 再交给链接器，不用 winresource——
//! 后者靠 reg.exe 查找 SDK，而本机安全策略禁止 reg.exe。
//! 任何一步失败都只产出警告，不阻断构建（图标缺失不影响功能）。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 本机已知存在的 SDK 目录（与 .cargo/config.toml 使用的版本一致）。
const SDK_BIN_CANDIDATES: &[&str] = &[
    r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64",
    r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64",
];

const VERSION: &str = "0.1.0";

fn main() {
    if !cfg!(windows) {
        return;
    }
    println!("cargo:rerun-if-changed=assets/aox.ico");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = manifest_dir.join("assets").join("aox.ico");
    if !ico.exists() {
        println!("cargo:warning=assets/aox.ico 不存在，跳过 exe 图标（可运行 tools/make_icon.py 生成）");
        return;
    }

    let Some(rc_exe) = find_rc() else {
        println!("cargo:warning=未找到 Windows SDK 的 rc.exe，跳过 exe 图标");
        return;
    };

    // .rc 里的反斜杠需要转义
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
        println!("cargo:warning=写入 .rc 失败，跳过 exe 图标: {e}");
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
            // 把 .res 交给 link.exe（MSVC 链接器接受 .res 作为输入）
            println!("cargo:rustc-link-arg-bins={}", res.display());
        }
        Ok(o) => println!(
            "cargo:warning=rc.exe 编译资源失败（不影响功能）: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => println!("cargo:warning=无法运行 rc.exe（不影响功能）: {e}"),
    }
}

/// 定位 rc.exe：先查已知目录，再扫 Windows Kits（取最高版本）。
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
