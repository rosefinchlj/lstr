# -*- coding: utf-8 -*-
"""回归验证：唤起、字体、卸载程序结果、设置点击不跳、设置拖动。"""
import ctypes
import time
from ctypes import wintypes

u = ctypes.windll.user32
OUT = r"E:\use\ai\deepseek\Rust\lstr\target"


class KI(ctypes.Structure):
    _fields_ = [
        ("wVk", ctypes.c_ushort),
        ("wScan", ctypes.c_ushort),
        ("dwFlags", ctypes.c_uint),
        ("time", ctypes.c_uint),
        ("dwExtra", ctypes.c_void_p),
    ]


class INP(ctypes.Structure):
    class U(ctypes.Union):
        _fields_ = [("ki", KI)]

    _anonymous_ = ("u",)
    _fields_ = [("type", ctypes.c_uint), ("u", U)]


def key(vk: int):
    u.keybd_event(vk, 0, 0, 0)
    time.sleep(0.02)
    u.keybd_event(vk, 0, 0x0002, 0)
    time.sleep(0.03)


def uni(ch: str):
    for flags in (0x0004, 0x0004 | 0x0002):
        a = INP()
        a.type = 1
        a.ki.wScan = ord(ch)
        a.ki.dwFlags = flags
        u.SendInput(1, ctypes.byref(a), ctypes.sizeof(INP))
        time.sleep(0.02)


def rect():
    hwnd = u.FindWindowW(None, "lstr")
    r = wintypes.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    return hwnd, r


def shot(name: str):
    from PIL import ImageGrab

    ImageGrab.grab(all_screens=True).save(OUT + "\\" + name)
    print("  shot", name)


def focus_input():
    hwnd, r = rect()
    u.SetForegroundWindow(hwnd)
    time.sleep(0.15)
    u.SetCursorPos((r.left + r.right) // 2, r.top + 26)
    u.mouse_event(0x0002, 0, 0, 0, 0)
    u.mouse_event(0x0004, 0, 0, 0, 0)
    time.sleep(0.25)


def click(x, y):
    u.SetCursorPos(int(x), int(y))
    time.sleep(0.12)
    u.mouse_event(0x0002, 0, 0, 0, 0)
    time.sleep(0.06)
    u.mouse_event(0x0004, 0, 0, 0, 0)
    time.sleep(0.35)


def drag(x, y, dx, dy):
    u.SetCursorPos(int(x), int(y))
    time.sleep(0.15)
    u.mouse_event(0x0002, 0, 0, 0, 0)
    time.sleep(0.12)
    for step in range(1, 7):
        u.SetCursorPos(int(x + dx * step / 6), int(y + dy * step / 6))
        time.sleep(0.05)
    u.mouse_event(0x0004, 0, 0, 0, 0)
    time.sleep(0.4)


def main():
    # 1) 双击 Space 唤起。若已可见，先隐藏再唤起，保证窗口拿到前台焦点。
    _, r = rect()
    if r.left > -100:
        for _ in range(2):
            key(0x20)
            time.sleep(0.06)
        time.sleep(0.5)
    for _ in range(2):
        key(0x20)
        time.sleep(0.06)
    time.sleep(1.0)
    _, r = rect()
    print(f"1 唤起: ({r.left},{r.top},{r.right},{r.bottom}) on_screen={r.left > -100}, size={r.right-r.left}x{r.bottom-r.top}")
    shot("v1-show.png")

    # 2) 输入“卸载程序”
    focus_input()
    for ch in "卸载程序":
        uni(ch)
    time.sleep(1.2)
    shot("v2-uninstall.png")
    print("2 已输入 卸载程序（见 v2-uninstall.png）")

    # 3) 点齿轮开设置
    _, r0 = rect()
    click(r0.right - 30, r0.top + 22)
    time.sleep(0.5)
    _, r1 = rect()
    print(f"3 设置打开: before=({r0.left},{r0.top}) after=({r1.left},{r1.top})")
    shot("v3-settings.png")

    # 4) 点主题按钮（会触发 apply_effects）→ 不得跳
    click(r1.left + 188, r1.top + 75)   # Obsidian
    time.sleep(0.4)
    _, r2 = rect()
    print(f"4 点主题按钮: after=({r2.left},{r2.top}) jump={r2.left == 0 or r2.top == 0}")
    click(r1.left + 292, r1.top + 75)   # Frost
    time.sleep(0.4)
    _, r3 = rect()
    print(f"4b 点回 Frost: after=({r3.left},{r3.top}) jump={r3.left == 0 or r3.top == 0}")
    shot("v4-theme.png")

    # 5) 拖不透明度滑块 → 不得跳
    drag(r1.left + 150, r1.top + 115, 70, 0)
    _, r4 = rect()
    print(f"5 拖透明度: after=({r4.left},{r4.top}) jump={r4.left == 0 or r4.top == 0}")
    shot("v5-opacity.png")

    # 6) 点“预览面板”复选框 → 不得跳
    click(r1.left + 150, r1.top + 267)
    _, r5 = rect()
    print(f"6 点复选框: after=({r5.left},{r5.top}) jump={r5.left == 0 or r5.top == 0}")

    # 7) 拖设置标题条 → 窗口应移动
    before = (r5.left, r5.top)
    drag((r5.left + r5.right) // 2, r5.top + 20, -70, -60)
    _, r6 = rect()
    moved = (r6.left - before[0], r6.top - before[1])
    print(f"7 设置拖动: 位移={moved} 移动={abs(moved[0]) > 30 or abs(moved[1]) > 30}")
    shot("v6-settings-drag.png")

    # 8) Esc 关设置，再 Esc 隐藏
    key(0x1B)
    time.sleep(0.3)
    _, r7 = rect()
    print(f"8 Esc 关设置: ({r7.left},{r7.top},{r7.right},{r7.bottom})")
    shot("v7-back-main.png")


if __name__ == "__main__":
    main()
