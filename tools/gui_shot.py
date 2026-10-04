#!/usr/bin/env python
"""截取 AOX GUI 窗口（开发期视觉验证）。

用法:
    python tools/gui_shot.py --out gui/_shot.png [--w 2400 --h 1350] [--title AOX]

为什么不用 PowerShell: 部分环境里 `Add-Type` 会被安全策略拦掉，
而 ctypes 走的是同一套 Win32 API（这个脚本等价于 tools/gui_shot.ps1）。
关键点: 必须先声明进程 DPI aware，否则 GetWindowRect 返回被虚拟化（按缩放
比例缩小）的坐标，而抓屏拿到的是物理像素，截图会整体偏移。
"""

import argparse
import ctypes
import ctypes.wintypes as wt
import sys
import time

user32 = ctypes.windll.user32
shcore = ctypes.windll.shcore
try:
    shcore.SetProcessDpiAwareness(2)  # PROCESS_PER_MONITOR_DPI_AWARE
except Exception:
    user32.SetProcessDPIAware()

EnumWindows = user32.EnumWindows
EnumWindowsProc = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)


def find_window(title_substr: str, exact: bool = False, pid: int = 0):
    """找目标窗口，两种定位方式二选一：

    - 按标题（默认）：子串或全等匹配；
    - 按 `pid`：主窗口标题改成空串之后（2026-10-04 用户要求删掉标题栏字样），
      按标题这条路走不通 —— 改成「该进程可见窗口里**面积最大**的那个」，就是主窗口。
    """
    found = []

    def cb(hwnd, _):
        if not user32.IsWindowVisible(hwnd):
            return True
        if pid:
            p = wt.DWORD()
            user32.GetWindowThreadProcessId(hwnd, ctypes.byref(p))
            if p.value != pid:
                return True
            r = wt.RECT()
            user32.GetWindowRect(hwnd, ctypes.byref(r))
            area = (r.right - r.left) * (r.bottom - r.top)
            found.append((area, hwnd, "(pid {})".format(pid)))
            return True
        n = user32.GetWindowTextLengthW(hwnd)
        if n <= 0:
            return True
        buf = ctypes.create_unicode_buffer(n + 1)
        user32.GetWindowTextW(hwnd, buf, n + 1)
        hit = (buf.value == title_substr) if exact else (title_substr.lower() in buf.value.lower())
        if hit:
            found.append((hwnd, buf.value))
            return False
        return True

    EnumWindows(EnumWindowsProc(cb), 0)
    if pid:
        found.sort(reverse=True)  # 面积大的在前
        return [(h, label) for _, h, label in found]
    return found


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="gui/_shot.png")
    ap.add_argument("--title", default="AOX")
    ap.add_argument(
        "--exact",
        action="store_true",
        help="标题必须完全相等（默认子串匹配）。界面上同时开着「AOX」与「AOX 活动流」"
        "两个窗口，子串匹配很容易抓到活动流那个 —— 截主窗口时加这个开关。",
    )
    ap.add_argument(
        "--pid",
        type=int,
        default=0,
        help="按进程 ID 找窗口（取该进程可见窗口中面积最大的）。主窗口标题改成空串后，"
        "按标题找不到它了 —— 用 tasklist 拿到 aox-gui.exe 的 pid 再传进来。",
    )
    ap.add_argument("--w", type=int, default=0, help="物理像素宽，0=保持原尺寸")
    ap.add_argument("--h", type=int, default=0)
    ap.add_argument("--wait", type=float, default=25.0)
    a = ap.parse_args()

    deadline = time.time() + a.wait
    win = []
    while time.time() < deadline:
        win = find_window(a.title, a.exact, a.pid)
        if win:
            break
        time.sleep(0.3)
    if not win:
        print(f"NOTFOUND: 没有标题包含 {a.title!r} 的可见窗口", file=sys.stderr)
        return 2

    hwnd, title = win[0]
    dpi = user32.GetDpiForWindow(hwnd) or 96
    scale = dpi / 96.0

    if a.w > 0 and a.h > 0:
        user32.MoveWindow(hwnd, 40, 40, a.w, a.h, True)
    user32.SetForegroundWindow(hwnd)
    HWND_TOPMOST = wt.HWND(-1)
    HWND_NOTOPMOST = wt.HWND(-2)
    user32.SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, 0x0003)  # NOMOVE|NOSIZE
    time.sleep(1.4)

    r = wt.RECT()
    if not user32.GetWindowRect(hwnd, ctypes.byref(r)):
        print("BADRECT: GetWindowRect 失败", file=sys.stderr)
        return 3
    w, h = r.right - r.left, r.bottom - r.top
    if w <= 0 or h <= 0:
        print(f"BADRECT {w}x{h}", file=sys.stderr)
        return 3

    from PIL import ImageGrab

    img = ImageGrab.grab(bbox=(r.left, r.top, r.right, r.bottom), all_screens=True)
    img.save(a.out)
    user32.SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, 0x0003)

    print(
        f"OK title={title!r} {w}x{h}px at ({r.left},{r.top}) "
        f"scale={scale:.2f} logical={w / scale:.0f}x{h / scale:.0f} -> {a.out}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
