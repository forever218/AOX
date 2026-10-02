#!/usr/bin/env bash
# 一键构建 AOX 发行安装包（命令行版 + 图形界面版）
#
#   1) cargo build --release --workspace  —— 同时产出 aox.exe 与 aox-gui.exe
#   2) Inno Setup 编译 installer/aox.iss      -> dist/AOX-Setup-<版本>.exe
#                     installer/aox-gui.iss  -> dist/AOX-GUI-Setup-<版本>.exe
#
# 用法：
#   bash tools/build_installer.sh          # 两个都构建
#   bash tools/build_installer.sh cli      # 只构建命令行版
#   bash tools/build_installer.sh gui      # 只构建图形界面版
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
cd "$ROOT"

TARGET="${1:-all}"
case "$TARGET" in
  all|cli|gui) ;;
  *) echo "用法: bash tools/build_installer.sh [all|cli|gui]"; exit 1 ;;
esac

# ---- 版本号以 Cargo.toml 为唯一来源
VERSION="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "无法从 Cargo.toml 读取 version"; exit 1; }
echo "==> AOX 版本: $VERSION（目标: $TARGET）"

# ---- 1. 编译 release
# 必须带 --workspace：根 crate 的 default-members 只有 ["."]，
# 只写 `cargo build --release` 根本不会编译 gui/，也就没有 aox-gui.exe 可打包。
echo "==> cargo build --release --workspace"
cargo build --release --workspace

# 打包前先确认二进制在位，避免 ISCC 报出难懂的 "Source file does not exist"
for exe in aox.exe aox-gui.exe; do
  case "$TARGET:$exe" in
    cli:aox-gui.exe|gui:aox.exe) continue ;;
  esac
  [ -f "target/release/$exe" ] || { echo "缺少 target/release/$exe，构建中止"; exit 1; }
done

# ---- 2. 定位 ISCC.exe
ISCC=""
for c in \
  "/e/INNO SETUP/Inno Setup 7/ISCC.exe" \
  "/e/INNO SETUP/Inno Setup 6/ISCC.exe" \
  "/c/Program Files (x86)/Inno Setup 7/ISCC.exe" \
  "/c/Program Files (x86)/Inno Setup 6/ISCC.exe" \
  "/c/Program Files/Inno Setup 6/ISCC.exe" \
  "/d/INNO SETUP/Inno Setup 7/ISCC.exe"; do
  if [ -x "$c" ]; then ISCC="$c"; break; fi
done
if [ -z "$ISCC" ]; then
  echo "未找到 ISCC.exe（Inno Setup 命令行编译器）。"
  echo "请手动编译：\"<Inno 安装目录>\\ISCC.exe\" installer\\aox.iss"
  exit 1
fi

# ---- 3. 编译安装包
# 注意：ISCC 会把以 / 开头的参数当成选项，脚本文件必须用相对路径或 Windows 风格路径
#
# 为什么要重试：ISCC 收尾时会给刚写出的 Setup.exe 写图标资源
# （Updating icons (Setup.exe)），这一步偶尔会撞上杀软实时扫描或索引器持有文件，
# 报 "EndUpdateResource failed (110)" 并中断。实测同一脚本重跑一次即成功，
# 所以这里退避重试，连续 3 次失败才真的报错。
compile() {
  local iss="$1" try=1 out=""
  while :; do
    echo
    echo "==> $ISCC  ($iss, AppVersion=$VERSION)  第 $try 次"
    if out="$("$ISCC" "/DAppVersion=$VERSION" "$iss" 2>&1)"; then
      echo "$out" | tail -3
      return 0
    fi
    echo "$out" | tail -5
    if [ "$try" -ge 3 ]; then
      echo "!! $iss 连续 $try 次编译失败（若为 EndUpdateResource(110)，请把 dist/ 加入杀软排除项）"
      return 1
    fi
    try=$((try + 1))
    sleep 2
  done
}

case "$TARGET" in
  all) compile "installer\\aox.iss"; compile "installer\\aox-gui.iss" ;;
  cli) compile "installer\\aox.iss" ;;
  gui) compile "installer\\aox-gui.iss" ;;
esac

echo
echo "==> 产物："
ls -lh "$ROOT/dist"
