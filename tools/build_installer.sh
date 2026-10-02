#!/usr/bin/env bash
# 一键构建 AOX 发行安装包（命令行版 + 图形界面版，各含两种变体）
#
#   1) cargo build --release --workspace  —— 同时产出 aox.exe 与 aox-gui.exe
#   2) Inno Setup 把 installer/aox.iss 与 installer/aox-gui.iss 各编译两遍：
#        不带定义  -> dist/AOX[-GUI]-Setup-<版本>.exe               （主包，不捆绑运行时）
#        带定义    -> dist/AOX[-GUI]-Setup-<版本>-with-python.exe   （装完即用，离线可用）
#   3) 顺便把运行时组件包复制到 dist/（已经是上游压好的 zip，零成本）
#
# 用法：
#   bash tools/build_installer.sh                      # 默认：四种包全出 + 组件包
#   bash tools/build_installer.sh cli                  # 只做命令行版（两种变体）
#   bash tools/build_installer.sh gui                  # 只做图形界面版（两种变体）
#   bash tools/build_installer.sh all none             # 只要两个主包（最快）
#   bash tools/build_installer.sh all with-python      # 只要两个捆绑变体
#   bash tools/build_installer.sh pack                 # 只产出运行时组件包（不打安装包）
#
# 关于运行时：主包**不捆绑**任何运行时（保持 3 MB 级体积）。需要 python 能力时有三条路，
# 都靠宿主的三档解释器解析（config 显式路径 > 应用目录内置 > PATH）落地：
#   1. 发行时附上 dist/aox-python-runtime.zip，用户放到 exe 同目录后执行
#      /runtime install python —— 全程离线；
#   2. 用户直接 /runtime install python，由宿主从上游下载并校验；
#   3. 直接用 -with-python 变体，一次装好。
# 三条路都不写系统 PATH，也不碰用户环境变量。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
cd "$ROOT"

TARGET="${1:-all}"
BUNDLE="${2:-both}"
case "$TARGET" in
  all|cli|gui|pack) ;;
  *) echo "用法: bash tools/build_installer.sh [all|cli|gui|pack] [both|none|with-python]"; exit 1 ;;
esac
case "$BUNDLE" in
  both|none|with-python) ;;
  *) echo "第二个参数只能是 both / none / with-python"; exit 1 ;;
esac

# 版本号以 Cargo.toml 为唯一来源
VERSION="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "无法从 Cargo.toml 读取 version"; exit 1; }
echo "==> AOX 版本: $VERSION（目标: $TARGET，变体: $BUNDLE）"

# ---- Python 运行时来源（可由环境变量覆盖成自建镜像）
PYTHON_URL="${AOX_PYTHON_URL:-https://www.python.org/ftp/python/3.13.7/python-3.13.7-embed-amd64.zip}"
# 与 src/runtimes.rs 里的 DEFAULT_PYTHON_SHA256 保持一致
PYTHON_SHA256="${AOX_PYTHON_SHA256:-f6cca216a359be84797cabb54149ce5e062afb16cc7567eb7fc51cacb2d86b65}"
RUNTIME_DIR="$ROOT/runtime/python"
RUNTIME_PACK="$ROOT/dist/aox-python-runtime.zip"
# 下载缓存：按 URL 的文件名存放，换源不会误用旧包；首次之后就完全离线可构建。
ZIP_CACHE="$ROOT/runtime/.cache-$(basename "$PYTHON_URL")"

sha_of() {
  certutil -hashfile "$(cygpath -w "$1" 2>/dev/null || echo "$1")" SHA256 \
    | sed -n 2p | tr -d ' \r' | tr 'A-F' 'a-f'
}

# 取到「已校验的官方包」，落在 ZIP_CACHE。已缓存则直接复用。
fetch_zip() {
  if [ -f "$ZIP_CACHE" ]; then
    echo "==> 复用已下载的运行时包: $ZIP_CACHE"
    return 0
  fi
  mkdir -p "$ROOT/runtime"
  echo "==> 下载 Python 运行时: $PYTHON_URL"
  curl -fL --progress-bar -o "$ZIP_CACHE" "$PYTHON_URL"
  echo "==> 校验 SHA-256"
  local got
  got="$(sha_of "$ZIP_CACHE")"
  if [ "$got" != "$PYTHON_SHA256" ]; then
    echo "!! 校验失败：期望 $PYTHON_SHA256，实际 $got"
    echo "   若确实要用别的版本，用 AOX_PYTHON_URL / AOX_PYTHON_SHA256 覆盖。"
    rm -f "$ZIP_CACHE"
    exit 1
  fi
}

# 把 zip 解压到 runtime/python（捆绑安装包需要这个目录）
ensure_runtime() {
  if [ -f "$RUNTIME_DIR/python.exe" ]; then
    echo "==> 运行时已就绪: $RUNTIME_DIR（$(du -sh "$RUNTIME_DIR" | cut -f1)）"
    return 0
  fi
  fetch_zip
  local stage="$ROOT/runtime/.staging"
  rm -rf "$stage"; mkdir -p "$stage"
  echo "==> 解压"
  tar -xf "$ZIP_CACHE" -C "$stage"
  mv "$stage" "$RUNTIME_DIR"
  [ -f "$RUNTIME_DIR/python.exe" ] || { echo "!! 解压后没有 python.exe，中止"; exit 1; }
  echo "==> 运行时已就位: $RUNTIME_DIR（$(du -sh "$RUNTIME_DIR" | cut -f1)）"
}

# 产出可分发的运行时组件包。
#
# 直接沿用官方那个已经压好的 zip（另存为约定的名字），而不是把解压出来的目录
# 重新压一遍：官方包 10.4 MB，重新压缩后反而涨到 21 MB（已压缩过的 stdlib zip
# 与 DLL 再压没有收益），而且这样组件包的 SHA-256 就是官方值，可信度更高。
make_pack() {
  fetch_zip
  mkdir -p "$ROOT/dist"
  cp -f "$ZIP_CACHE" "$RUNTIME_PACK"
  echo
  echo "==> 运行时组件包: $RUNTIME_PACK（$(du -h "$RUNTIME_PACK" | cut -f1)）"
  echo "    用户用法：把它放到 aox.exe 同目录，然后执行 /runtime install python"
  echo
  echo "    SHA-256（自建镜像时填入 config.toml 的 [runtimes] python_sha256）:"
  sha_of "$RUNTIME_PACK"
}

if [ "$TARGET" = "pack" ]; then
  make_pack
  exit 0
fi

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
# 定义一律用长式 `--define=`，**不要**用旧式的 `/D名字=值`：
# ISCC 把以 / 开头的参数当选项，而 Git Bash（MSYS）在启动原生程序前会把这类参数
# 当成 Unix 路径改写一遍 —— `/DAppVersion=0.1.0` 会被改写成
# `<Git 安装目录>/DAppVersion=0.1.0`，ISCC 认不出这是个选项，就把它和脚本文件
# 一起算成两个「script filename」，报 “You may not specify more than one script
# filename.”。`--define=` 不以 / 开头，不受这条规则影响（本机实测两种写法展开结果
# 完全一致，见 README「发行产物」一节）。
#
# 为什么要重试：ISCC 收尾时会给刚写出的 Setup.exe 写图标资源
# （Updating icons (Setup.exe)），这一步偶尔会撞上杀软实时扫描或索引器持有文件，
# 报 "EndUpdateResource failed (110)" 并中断。实测同一脚本重跑一次即成功，
# 所以这里退避重试，连续 3 次失败才真的报错。
compile() {
  local iss="$1" variant="$2" try=1 out=""
  local -a args=("--define=AppVersion=$VERSION")
  if [ "$variant" = "with-python" ]; then
    args+=("--define=BUNDLE_PYTHON=1")
  fi
  args+=("$iss")
  while :; do
    echo
    echo "==> $ISCC ($iss, 变体=$variant)  第 $try 次"
    if out="$("$ISCC" "${args[@]}" 2>&1)"; then
      # ISCC 的 "Resulting Setup program filename is:" 与文件名分两行，一起带出来
      echo "$out" | grep -A1 -E "Successful compile" | sed 's/^/    /'
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
  all) ISS_LIST=("installer\\aox.iss" "installer\\aox-gui.iss") ;;
  cli) ISS_LIST=("installer\\aox.iss") ;;
  gui) ISS_LIST=("installer\\aox-gui.iss") ;;
esac

case "$BUNDLE" in
  none)        VARIANTS=("none") ;;
  with-python) VARIANTS=("with-python") ;;
  both)        VARIANTS=("none" "with-python") ;;
esac

for variant in "${VARIANTS[@]}"; do
  if [ "$variant" = "with-python" ]; then
    ensure_runtime
  fi
  for iss in "${ISS_LIST[@]}"; do
    compile "$iss" "$variant"
  done
done

# 顺带把组件包放进 dist/：捆绑流程已经把官方 zip 拿到本地，复制一份零成本，
# 而离线装配这条路要的正是这个文件。
if [ "$BUNDLE" != "none" ]; then
  make_pack
fi

echo
echo "==> 产物："
ls -lh "$ROOT/dist"
