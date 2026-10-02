"""黑窗回归检查：确认宿主拉起的插件子进程不会弹出控制台窗口。

背景（Windows 特有的坑）
-----------------------
`aox-gui.exe` 编译为 `windows` 子系统，自身**没有控制台**。此时用不带
`CREATE_NO_WINDOW` 的 `CreateProcess` 去拉起控制台程序（`python.exe` /
`node.exe` / `rustc.exe`），Windows **会新分配一块带窗口的控制台** —— 用户看到的就是
「一用插件就弹黑色 python 窗口」。宿主侧修法见 `src/plugin/mod.rs::hide_console`。

两种模式
--------
1) 机制自检（不需要图形界面，秒级、确定性）：

       python tools/check_no_console_window.py --ab

   造一个「没有控制台」的父进程，分别用 flags=0 与 CREATE_NO_WINDOW 拉起子进程，
   对照新出现的控制台窗口数。应当看到 0 个 -> 1 个。

2) 真机端到端（拉起打包用的 release 二进制，恢复真实插件）：

       python tools/check_no_console_window.py --app target/release/aox-gui.exe --cwd .

   用 `DETACHED_PROCESS` 启动 —— 与「双击 exe / 点开始菜单」等价，
   然后一边跑一边枚举 `ConsoleWindowClass` 顶层窗口，同时统计 `python.exe`
   进程数（证明插件确实被拉起，而不是压根没启动）。全程 0 个新窗口才算通过。

退出码：0 = 通过；1 = 检测到黑窗。
"""

from __future__ import annotations

import argparse
import ctypes
import ctypes.wintypes as wt
import subprocess
import sys
import tempfile
import time
from pathlib import Path

u = ctypes.windll.user32

DETACHED_PROCESS = 0x0000_0008
CREATE_NO_WINDOW = 0x0800_0000
CONSOLE_CLASSES = ("ConsoleWindowClass", "CASCADIA_HOSTING_WINDOW_CLASS")
TMP = Path(tempfile.gettempdir())


# --------------------------------------------------------------------------- #
# Windows 探针
# --------------------------------------------------------------------------- #
def console_windows() -> list[tuple[int, str]]:
    """枚举当前桌面上所有控制台类顶层窗口，返回 [(hwnd, 标题)]。"""
    found: list[tuple[int, str]] = []

    @ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
    def cb(hwnd, _lparam):
        buf = ctypes.create_unicode_buffer(256)
        u.GetClassNameW(hwnd, buf, 256)
        if buf.value in CONSOLE_CLASSES:
            title = ctypes.create_unicode_buffer(256)
            u.GetWindowTextW(hwnd, title, 256)
            found.append((int(hwnd), title.value))
        return True

    u.EnumWindows(cb, 0)
    return found


def count_process(image: str) -> int:
    """按镜像名统计进程数。tasklist 的输出是本地代码页，用 errors='replace' 兜住。"""
    out = subprocess.run(
        ["tasklist", "/FI", f"IMAGENAME eq {image}", "/FO", "CSV", "/NH"],
        capture_output=True, text=True, errors="replace",
    ).stdout
    return sum(1 for line in out.splitlines() if line.lstrip().startswith('"'))


def watch(seconds: float, base: set[int], label: str = "") -> list[tuple[int, str]]:
    """在 seconds 秒内轮询，返回新出现的控制台窗口。"""
    new: list[tuple[int, str]] = []
    seen = set(base)
    deadline = time.time() + seconds
    while time.time() < deadline:
        for hwnd, title in console_windows():
            if hwnd not in seen:
                seen.add(hwnd)
                new.append((hwnd, title))
                print(f"  [!] 新控制台窗口 hwnd={hwnd} title={title!r} {label}")
        time.sleep(0.03)
    return new


# --------------------------------------------------------------------------- #
# 模式 1：机制自检
# --------------------------------------------------------------------------- #
_PARENT_SRC = """
import ctypes, pathlib, subprocess, sys, time
mode, flags, pm, cm = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
# 父进程自证：确实没有控制台窗口（0）
pathlib.Path(pm).write_text(str(ctypes.windll.kernel32.GetConsoleWindow()))
child = ("import ctypes,pathlib,sys,time;"
         "pathlib.Path(sys.argv[1]).write_text(str(ctypes.windll.kernel32.GetConsoleWindow()));"
         "time.sleep(3.0)")
subprocess.Popen([sys.executable, "-X", "utf8", "-c", child, cm],
                 creationflags=flags, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(3.5)
"""


def mode_ab() -> int:
    print("=" * 74)
    print("机制自检：父进程 DETACHED_PROCESS（没有控制台，与 aox-gui.exe 同构）")
    print("=" * 74)

    results: dict[str, int] = {}
    for label, flags in (("对照组 flags=0", 0), ("修复组 CREATE_NO_WINDOW", CREATE_NO_WINDOW)):
        pm = TMP / "_cnw_parent.txt"
        cm = TMP / "_cnw_child.txt"
        pm.unlink(missing_ok=True)
        cm.unlink(missing_ok=True)

        base = {h for h, _ in console_windows()}
        proc = subprocess.Popen(
            [sys.executable, "-X", "utf8", "-c", _PARENT_SRC, label, str(flags), str(pm), str(cm)],
            creationflags=DETACHED_PROCESS, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        new = watch(4.0, base)
        proc.wait()
        time.sleep(0.2)

        parent_gcw = pm.read_text().strip() if pm.exists() else "?"
        child_gcw = cm.read_text().strip() if cm.exists() else "(未跑)"
        results[label] = len(new)
        expect = "预期 1（会弹黑窗）" if flags == 0 else "预期 0（干净）"
        print(f"\n[{label}]  creationflags={hex(flags)}  {expect}")
        print(f"    父进程 GetConsoleWindow={parent_gcw}（0 = 没有控制台）")
        print(f"    子进程 GetConsoleWindow={child_gcw}")
        print(f"    新出现的控制台窗口 = {len(new)}\n")

    leaked = results.get("修复组 CREATE_NO_WINDOW", 1)
    caught = results.get("对照组 flags=0", 0)
    ok = leaked == 0 and caught >= 1
    print(f"结论：flags=0 -> {caught} 个窗口；CREATE_NO_WINDOW -> {leaked} 个窗口")
    print("通过：机制成立，且 CREATE_NO_WINDOW 能压掉黑窗。" if ok
          else "失败：对照结果不符合预期，检查探针是否被安全软件/桌面会话影响。")
    return 0 if ok else 1


# --------------------------------------------------------------------------- #
# 模式 2：真机端到端
# --------------------------------------------------------------------------- #
def mode_app(exe: Path, cwd: Path, seconds: float) -> int:
    if not exe.exists():
        print(f"找不到 {exe}，请先 cargo build --release --workspace")
        return 1
    print("=" * 74)
    print(f"端到端：{exe.name}（DETACHED 启动，cwd={cwd}）")
    print("=" * 74)

    base_win = {h for h, _ in console_windows()}
    base_py = count_process("python.exe")
    print(f"[基线] 控制台窗口={len(base_win)}  python.exe={base_py}")

    proc = subprocess.Popen([str(exe)], cwd=str(cwd), creationflags=DETACHED_PROCESS,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print(f"[启动] {exe.name} pid={proc.pid}")

    new: list[tuple[int, str]] = []
    seen = set(base_win)
    peak_py = base_py
    deadline = time.time() + seconds
    while time.time() < deadline:
        for hwnd, title in console_windows():
            if hwnd not in seen:
                seen.add(hwnd)
                new.append((hwnd, title))
                print(f"  [!] 新控制台窗口 hwnd={hwnd} title={title!r}")
        peak_py = max(peak_py, count_process("python.exe"))
        if proc.poll() is not None:
            print("  (进程已退出)")
            break
        time.sleep(0.15)

    print(f"\n插件子进程峰值 python.exe = {peak_py}（基线 {base_py}；"
          f"{'说明插件确实被拉起了' if peak_py > base_py else '没有观察到插件进程，检查插件目录'}）")
    print(f"新出现的控制台窗口 = {len(new)}")

    subprocess.run(["taskkill", "/F", "/T", "/PID", str(proc.pid)],
                   capture_output=True, text=True, errors="replace")
    for _ in range(12):
        if count_process("python.exe") <= base_py:
            break
        time.sleep(0.4)
    print(f"[收尾] 已结束，python.exe = {count_process('python.exe')}（基线 {base_py}）")

    if new:
        print("失败：启动/加载插件过程中弹出了黑窗。")
        return 1
    print("通过：全程没有黑窗。")
    return 0


def main() -> int:
    if sys.platform != "win32":
        print("本工具仅在 Windows 上有意义。")
        return 0
    ap = argparse.ArgumentParser(description="检查插件子进程是否弹出控制台黑窗")
    ap.add_argument("--ab", action="store_true", help="只跑机制自检（不需要 GUI）")
    ap.add_argument("--app", help="要检查的可执行文件，如 target/release/aox-gui.exe")
    ap.add_argument("--cwd", default=".", help="工作目录（决定应用根与插件目录）")
    ap.add_argument("--seconds", type=float, default=22.0, help="端到端观察时长（秒）")
    args = ap.parse_args()

    if args.app:
        return mode_app(Path(args.app).resolve(), Path(args.cwd).resolve(), args.seconds)
    return mode_ab()


if __name__ == "__main__":
    sys.exit(main())
