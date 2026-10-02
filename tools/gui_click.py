#!/usr/bin/env python
"""在窗口内按「相对右边距」点击一次（开发期验证弹窗用）。

    python tools/gui_click.py --title AOX --right 146 --y 25

坐标以窗口**逻辑像素**给出（左上角为原点），脚本会按 DPI 换算成物理像素，
所以不受显示器缩放影响。与 gui_shot.py 一样用 ctypes 直连 Win32，
避免 Add-Type 被安全策略拦掉。
"""

import argparse
import ctypes
import ctypes.wintypes as wt
import sys
import time

user32 = ctypes.windll.user32
try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except Exception:
    user32.SetProcessDPIAware()

EnumWindows = user32.EnumWindows
EnumWindowsProc = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)

MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004


def find_window(title_substr):
    found = []

    def cb(hwnd, _):
        if not user32.IsWindowVisible(hwnd):
            return True
        n = user32.GetWindowTextLengthW(hwnd)
        if n <= 0:
            return True
        buf = ctypes.create_unicode_buffer(n + 1)
        user32.GetWindowTextW(hwnd, buf, n + 1)
        if title_substr.lower() in buf.value.lower():
            found.append((hwnd, buf.value))
            return False
        return True

    EnumWindows(EnumWindowsProc(cb), 0)
    return found


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--title", default="AOX")
    ap.add_argument("--right", type=float, required=True, help="距窗口右边多少逻辑像素")
    ap.add_argument("--y", type=float, default=25.0, help="距窗口顶部多少逻辑像素")
    a = ap.parse_args()

    win = find_window(a.title)
    if not win:
        print(f"NOTFOUND: 没有标题包含 {a.title!r} 的可见窗口", file=sys.stderr)
        return 2
    hwnd, title = win[0]

    dpi = user32.GetDpiForWindow(hwnd) or 96
    scale = dpi / 96.0

    user32.SetForegroundWindow(hwnd)
    time.sleep(0.6)

    # 必须用**客户区**原点而不是窗口原点：窗口带原生标题栏，
    # 从窗口顶算会把 25px 落到标题栏上，点了个空。
    cr = wt.RECT()
    if not user32.GetClientRect(hwnd, ctypes.byref(cr)):
        print("BADRECT", file=sys.stderr)
        return 3
    pt = wt.POINT(0, 0)
    if not user32.ClientToScreen(hwnd, ctypes.byref(pt)):
        print("BADPOINT", file=sys.stderr)
        return 3
    cw = cr.right - cr.left

    x = pt.x + cw - int(a.right * scale)
    y = pt.y + int(a.y * scale)
    print(
        f"窗口 {title!r} 客户区 {cw}x{cr.bottom - cr.top}px @({pt.x},{pt.y}) "
        f"scale={scale:.2f} -> 点击 ({x},{y})＝距客户区右边 {a.right:.0f}、顶 {a.y:.0f} 逻辑像素"
    )

    user32.SetCursorPos(x, y)
    time.sleep(0.25)
    user32.mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
    time.sleep(0.06)
    user32.mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
    time.sleep(1.2)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
