//! 构建脚本：把图标与版本信息写进 `aox-gui.exe` 的 Windows 资源。
//!
//! 与根 crate 的 `build.rs` 同一套做法（直接调用 Windows SDK 的 rc.exe，
//! 不用 winresource——后者靠 reg.exe 查找 SDK，而本机安全策略禁止 reg.exe）。
//! 两点差别：
//!   * 图标在仓库根的 `assets/aox.ico`（不在 gui/ 下），版本号直接读
//!     cargo 注入的 `CARGO_PKG_VERSION`，不再手写常量。
//!   * 找不到图标也照样写版本资源，而不是整个跳过。
//!
//! 为什么必须有：没有资源段的 exe 在资源管理器、开始菜单、任务栏里都是一张
//! 白纸，「设置 → 应用」里也看不到版本号——CLI 版一直有，GUI 版不该缺。
//!
//! 任何一步失败都只产出警告，不阻断构建（图标缺失不影响功能）。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 本机已知存在的 SDK 目录（与 .cargo/config.toml 使用的版本一致）。
const SDK_BIN_CANDIDATES: &[&str] = &[
    r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64",
    r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64",
];

fn main() {
    if !cfg!(windows) {
        return;
    }

    // 分发安全闸门：Windows 上必须静态链接 CRT，理由与判据见根 crate 的 build.rs。
    // 单独装 GUI 版的用户最多，这个包尤其不能依赖 VCRUNTIME140.dll。
    if !target_features().iter().any(|f| f == "crt-static") {
        println!(
            "cargo:warning=AOX GUI 未启用静态 CRT：产物将依赖 VCRUNTIME140.dll，\
             在未安装 VC++ 运行库的 Windows 上无法启动。\
             请在 .cargo/config.toml 的 [target.x86_64-pc-windows-msvc] 下加 \
             rustflags = [\"-C\", \"target-feature=+crt-static\"]，或改用工装环境变量 \
             RUSTFLAGS=\"-C target-feature=+crt-static\" 重新构建。"
        );
    }

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // 图标在仓库根，不在 gui/ 下
    let ico = manifest_dir.join("..").join("assets").join("aox.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    println!("cargo:rerun-if-changed=build.rs");

    let Some(rc_exe) = find_rc() else {
        println!("cargo:warning=未找到 Windows SDK 的 rc.exe，跳过 exe 图标");
        return;
    };

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());

    let icon_line = if ico.exists() {
        // .rc 里的反斜杠需要转义
        format!(
            "1 ICON \"{}\"\n\n",
            ico.display().to_string().replace('\\', "\\\\")
        )
    } else {
        println!("cargo:warning=assets/aox.ico 不存在，只写入版本资源（可运行 tools/make_icon.py 生成）");
        String::new()
    };

    // FILEVERSION 需要 "a,b,c,d" 四个纯数字
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

    // 字符串块用 040904b0（英文 + Unicode），所以这里的描述文字保持 ASCII，
    // 避免 rc.exe 按本地代码页误读。
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
        println!("cargo:warning=写入 .rc 失败，跳过 exe 图标: {e}");
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

/// 目标平台当前启用的 rustc 特性列表。实现与根 crate 的 `build.rs` 保持一致。
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
