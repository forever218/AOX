#!/usr/bin/env bash
# 一键构建 AOX 发行安装包
#
#   1) 编译 release 版 aox.exe（含图标与版本资源）
#   2) 用 Inno Setup 编译 installer/aox.iss，产出 dist/AOX-Setup-<版本>.exe
#
# 用法：bash tools/build_installer.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
cd "$ROOT"

# ---- 版本号以 Cargo.toml 为唯一来源
VERSION="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "无法从 Cargo.toml 读取 version"; exit 1; }
echo "==> AOX 版本: $VERSION"

# ---- 1. 编译 release
echo "==> cargo build --release"
cargo build --release

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
echo "==> $ISCC  (AppVersion=$VERSION)"
"$ISCC" "/DAppVersion=$VERSION" "installer\\aox.iss" | tail -5

echo
echo "==> 产物："
ls -lh "$ROOT/dist"
