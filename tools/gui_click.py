#!/usr/bin/env python
"""在窗口内点一次（开发期验证界面用）。

    python tools/gui_click.py --title AOX --right 146 --y 25     # 距右边 146
    python tools/gui_click.py --title AOX --left 38  --y 25      # 距左边 38

    # 只悬停不点击（验证悬停才出现的面板），停 1.2s 后顺手截图：
    python tools/gui_click.py --title AOX --left 45 --y 25 --move --hold 1.2 --out gui/_hover.png

坐标以窗口**逻辑像素**给出（以客户区左上角为原点），脚本会按 DPI 换算成物理像素，
所以不受显示器缩放影响。与 gui_shot.py 一样用 ctypes 直连 Win32，
避免 Add-Type 被安全策略拦掉。

左 / 右二选一：界面右侧的元素（顶栏图标、弹窗按钮）按右边量最稳，
左侧的元素（历史侧边栏、左侧行）按左边量最直观。

同时开着「AOX」与「AOX 活动流」时**要加 --exact**：子串匹配可能抓到活动流那个。
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


def find_window(title_substr, exact=False, pid=0):
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
    ap.add_argument("--title", default="AOX")
    ap.add_argument(
        "--exact",
        action="store_true",
        help="标题必须完全相等（默认子串匹配）。界面同时开着「AOX」与「AOX 活动流」"
        "两个窗口时，子串匹配可能抓到活动流那个（EnumWindows 的顺序不保证），"
        "点主窗口时加这个开关。",
    )
    ap.add_argument(
        "--pid",
        type=int,
        default=0,
        help="按进程 ID 找窗口（取该进程可见窗口中面积最大的）。主窗口标题改成空串后，"
        "按标题找不到它了 —— 用 tasklist 拿到 aox-gui.exe 的 pid 再传进来。",
    )
    ap.add_argument("--right", type=float, help="距客户区右边多少逻辑像素")
    ap.add_argument("--left", type=float, help="距客户区左边多少逻辑像素")
    ap.add_argument("--y", type=float, default=25.0, help="距客户区顶部多少逻辑像素")
    ap.add_argument(
        "--move",
        action="store_true",
        help="只把指针移过去、**不点击** —— 验证「悬停才出现」的东西"
        "（例如顶栏 Σ 用量面板；点下去反而会把它关掉）。",
    )
    ap.add_argument("--out", default="", help="移动后截图的输出路径（配合 --move）")
    ap.add_argument("--hold", type=float, default=1.0, help="移动后停几秒再截图")
    a = ap.parse_args()
    if a.left is None and a.right is None:
        print("必须给出 --left 或 --right", file=sys.stderr)
        return 2

    win = find_window(a.title, a.exact, a.pid)
    if not win:
        print(f"NOTFOUND: 没有标题包含 {a.title!r} 的可见窗口", file=sys.stderr)
        return 2
    hwnd, title = win[0]

    dpi = user32.GetDpiForWindow(hwnd) or 96
    scale = dpi / 96.0

    user32.SetForegroundWindow(hwnd)
    # 只 SetForegroundWindow 是不够的：从非交互 shell 启动的进程常常被系统拒绝
    # 抢前台，于是窗口仍被别的窗口（典型的是「AOX 活动流」那个子窗口）压着 ——
    # 点在屏幕坐标上的点击会落到压在上面的那个窗口里，表现为「点了没反应」。
    # 临时置顶一次再还原，与 gui_shot.py 的做法一致。
    HWND_TOPMOST = wt.HWND(-1)
    HWND_NOTOPMOST = wt.HWND(-2)
    SWP_NOMOVE_NOSIZE = 0x0003
    user32.SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE_NOSIZE)
    time.sleep(0.4)

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
    ch = cr.bottom - cr.top
    logical_w = cw / scale

    if a.left is not None:
        x = pt.x + int(a.left * scale)
        where = f"距左 {a.left:.0f}"
    else:
        x = pt.x + cw - int(a.right * scale)
        where = f"距右 {a.right:.0f}"
    y = pt.y + int(a.y * scale)
    print(
        f"窗口 {title!r} 客户区 {cw}x{ch}px（逻辑 {logical_w:.0f}x{ch / scale:.0f}）"
        f"scale={scale:.2f} -> 点击 ({x},{y})＝{where}、顶 {a.y:.0f} 逻辑像素"
    )

    user32.SetCursorPos(x, y)
    if a.move:
        # 悬停类控件（顶栏 Σ 用量面板那种）靠**指针位置**触发，点下去反而会把它关掉；
        # 而且要停够时间再截图：egui / winit 是先收到 WM_MOUSEMOVE **才出下一帧**的，
        # 立刻截图会拍到「面板还没出来」的上一帧。
        time.sleep(a.hold)
        if a.out:
            from PIL import ImageGrab

            r = wt.RECT()
            user32.GetWindowRect(hwnd, ctypes.byref(r))
            ImageGrab.grab(bbox=(r.left, r.top, r.right, r.bottom), all_screens=True).save(a.out)
            print(f"已截图 -> {a.out}")
        user32.SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE_NOSIZE)
        return 0
    time.sleep(0.25)
    user32.mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
    time.sleep(0.06)
    user32.mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
    time.sleep(1.2)
    user32.SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE_NOSIZE)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

