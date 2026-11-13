# -*- coding: utf-8 -*-
"""验证：size/date 分段遮罩里比较符可以直接改（打 < > =、← 到比较符段 ↑↓ 换档）。

用法：先启动 release 版 lstr 并唤出窗口，再跑本脚本。脚本只发按键并截图，
不读也不改用户配置/历史。文本字符用 PostMessage(WM_CHAR) 投递，绕过输入法。
"""
import ctypes
import os
import time
from ctypes import wintypes

u = ctypes.windll.user32
k32 = ctypes.windll.kernel32
try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except Exception:
    pass

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT_DIR = os.path.join(ROOT, "target", "mask-op")
VK_RETURN = 0x0D
VK_LEFT = 0x25
VK_UP = 0x26
VK_CONTROL = 0x11
VK_DELETE = 0x2E
WM_CHAR = 0x0102
WM_KEYDOWN = 0x0100
WM_KEYUP = 0x0101
KEYUP = 0x0002


def find_window(title="lstr"):
    result = []

    @ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
    def cb(hwnd, _):
        if not u.IsWindowVisible(hwnd):
            return True
        buf = ctypes.create_unicode_buffer(256)
        u.GetWindowTextW(hwnd, buf, 256)
        if buf.value == title:
            result.append(hwnd)
        return True

    u.EnumWindows(cb, 0)
    return result[0] if result else None


def rect(hwnd):
    r = wintypes.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    return (r.left, r.top, r.right, r.bottom)


def force_foreground(hwnd):
    fg = u.GetForegroundWindow()
    cur = k32.GetCurrentThreadId()
    tid_fg = u.GetWindowThreadProcessId(fg, None)
    tid_t = u.GetWindowThreadProcessId(hwnd, None)
    if tid_fg:
        u.AttachThreadInput(cur, tid_fg, True)
    if tid_t:
        u.AttachThreadInput(cur, tid_t, True)
    u.BringWindowToTop(hwnd)
    ok = u.SetForegroundWindow(hwnd)
    u.SetFocus(hwnd)
    if tid_t:
        u.AttachThreadInput(cur, tid_t, False)
    if tid_fg:
        u.AttachThreadInput(cur, tid_fg, False)
    time.sleep(0.25)
    return ok, u.GetForegroundWindow() == hwnd


def char(hwnd, ch):
    """投递一个字符（绕过输入法组合）。"""
    u.PostMessageW(hwnd, WM_CHAR, ord(ch), 1)
    time.sleep(0.06)


def key(hwnd, vk, hold=0.04):
    u.PostMessageW(hwnd, WM_KEYDOWN, vk, 0x00000001)
    time.sleep(hold)
    u.PostMessageW(hwnd, WM_KEYUP, vk, 0xC0000001)
    time.sleep(0.10)


def ctrl_key(hwnd, vk):
    u.PostMessageW(hwnd, WM_KEYDOWN, VK_CONTROL, 1)
    time.sleep(0.03)
    u.PostMessageW(hwnd, WM_KEYDOWN, vk, 1)
    time.sleep(0.04)
    u.PostMessageW(hwnd, WM_KEYUP, vk, 0xC0000001)
    u.PostMessageW(hwnd, WM_KEYUP, VK_CONTROL, 0xC0000001)
    time.sleep(0.12)


def shot(name):
    from PIL import ImageGrab

    os.makedirs(OUT_DIR, exist_ok=True)
    path = os.path.join(OUT_DIR, name + ".png")
    ImageGrab.grab().save(path)
    print("shot:", path)
    return path


def main():
    hwnd = find_window()
    if not hwnd:
        raise SystemExit("lstr 窗口没找到（先启动 release 版并唤出）")
    print("hwnd:", hwnd, "rect:", rect(hwnd))
    print("force_foreground:", force_foreground(hwnd))
    time.sleep(0.4)

    # 清空输入：Ctrl+A + Delete，然后逐字打 size> （模拟联想 Enter 后的状态）。
    ctrl_key(hwnd, 0x41)
    key(hwnd, VK_DELETE)
    for ch in "size":
        char(hwnd, ch)
    time.sleep(0.6)
    shot("01-suggest")
    key(hwnd, VK_RETURN)
    time.sleep(0.6)
    shot("02-mask-default")

    # 直接打 `<`：比较符就地改，不退出遮罩。
    char(hwnd, "<")
    time.sleep(0.5)
    shot("03-op-typed-lt")

    # ← 到比较符段，↑ 换档到 `<=`。
    key(hwnd, VK_LEFT)
    time.sleep(0.4)
    shot("04-op-focused")
    key(hwnd, VK_UP)
    time.sleep(0.4)
    shot("05-op-cycled-le")

    # 打 `=` 直接换成等值（`>=` → `=` 的路径这次用 `>`+`=` 验证）。
    char(hwnd, ">")
    char(hwnd, "=")
    time.sleep(0.5)
    shot("06-op-typed-ge")

    key(hwnd, VK_RETURN)
    time.sleep(0.9)
    shot("07-committed-chip")

    # 清理：输入框为空时 Ctrl+Backspace 删左邻 chip。
    ctrl_key(hwnd, 0x08)
    time.sleep(0.8)
    shot("08-cleaned")


if __name__ == "__main__":
    main()
