#!/usr/bin/env python
"""向 AOX GUI 窗口输入一段文本，可选末尾回车（开发期验证界面用）。

    python tools/gui_type.py --left 600 --y 727 --text "你好，先看看目录" --enter

为什么不直接用 SendKeys: `System.Windows.Forms.SendKeys` 走的是虚拟键码，
中文（尤其非 BMP 字符）会被吃掉或变成乱码。这里用 SendInput + KEYEVENTF_UNICODE
逐个字符投递，绕开键盘布局，任何语言都能原样送进去。

坐标语义与 gui_click.py 完全一致：窗口**客户区**左上角为原点、单位是逻辑像素，
脚本按 DPI 换算成物理像素，所以不受显示器缩放影响。
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
INPUT_KEYBOARD = 1
KEYEVENTF_KEYUP = 0x0002
KEYEVENTF_UNICODE = 0x0004

# 常用虚拟键码，按名字给，免得调用方记 0x1B 这种数
VK = {"enter": 0x0D, "esc": 0x1B, "tab": 0x09, "space": 0x20, "backspace": 0x08}


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [
        ("wVk", wt.WORD),
        ("wScan", wt.WORD),
        ("dwFlags", wt.DWORD),
        ("time", wt.DWORD),
        ("dwExtraInfo", ctypes.c_void_p),
    ]


class INPUT(ctypes.Structure):
    """注意 sizeof(INPUT)==40（x64）：type 4 字节 + 4 字节对齐填充 + 32 字节联合体。

    联合体在 x64 上由 MOUSEINPUT 决定大小（32），只按 KEYBDINPUT（24）来填
    会让 cbSize 少 8 字节，SendInput 直接返回 0 且 GetLastError 报参数错误。
    """

    class _U(ctypes.Union):
        _fields_ = [("ki", KEYBDINPUT), ("padding", ctypes.c_byte * 32)]

    _anonymous_ = ("u",)
    _fields_ = [("type", wt.DWORD), ("u", _U)]


user32.SendInput.argtypes = (wt.UINT, ctypes.POINTER(INPUT), ctypes.c_int)
user32.SendInput.restype = wt.UINT


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


def send_unicode(ch: str) -> None:
    """把单个 UTF-16 码元按 unicode 扫描码送进去（按下 + 抬起）。"""
    code = ord(ch)
    for flags in (KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP):
        inp = INPUT()
        inp.type = INPUT_KEYBOARD
        inp.ki = KEYBDINPUT(0, code, flags, 0, None)
        if user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT)) != 1:
            raise OSError(f"SendInput 失败，字符 {ch!r} 未送达")


def send_vk(vk: int) -> None:
    for flags in (0, KEYEVENTF_KEYUP):
        inp = INPUT()
        inp.type = INPUT_KEYBOARD
        inp.ki = KEYBDINPUT(vk, 0, flags, 0, None)
        if user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT)) != 1:
            raise OSError(f"SendInput 失败，键码 {vk:#x} 未送达")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--title", default="AOX")
    ap.add_argument("--left", type=float, help="先点击：距客户区左边多少逻辑像素")
    ap.add_argument("--right", type=float, help="先点击：距客户区右边多少逻辑像素")
    ap.add_argument("--y", type=float, default=0.0, help="先点击：距客户区顶部多少逻辑像素")
    ap.add_argument("--text", default="", help="要输入的文本")
    ap.add_argument("--enter", action="store_true", help="输入完按一次回车")
    ap.add_argument(
        "--key",
        choices=sorted(VK),
        help="输入文本后再按一个功能键（enter/esc/tab/space/backspace）",
    )
    ap.add_argument("--delay", type=float, default=0.02, help="逐字间隔秒数")
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

    cr = wt.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(cr))
    pt = wt.POINT(0, 0)
    user32.ClientToScreen(hwnd, ctypes.byref(pt))
    cw = cr.right - cr.left

    # 先把焦点给到目标控件：GUI 的输入框要靠点击才会拿到键盘焦点。
    if a.left is not None or a.right is not None:
        x = pt.x + (int(a.left * scale) if a.left is not None else cw - int(a.right * scale))
        y = pt.y + int(a.y * scale)
        user32.SetCursorPos(x, y)
        time.sleep(0.25)
        user32.mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
        time.sleep(0.06)
        user32.mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
        time.sleep(0.5)

    for ch in a.text:
        send_unicode(ch)
        time.sleep(a.delay)
    if a.enter:
        time.sleep(0.2)
        send_vk(0x0D)  # VK_RETURN
    if a.key:
        time.sleep(0.2)
        send_vk(VK[a.key])

    tail = ""
    if a.enter:
        tail += "，已回车"
    if a.key:
        tail += f"，已按 {a.key}"
    print(f"OK title={title!r} scale={scale:.2f} 输入 {len(a.text)} 字{tail}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
