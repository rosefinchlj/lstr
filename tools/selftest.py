# -*- coding: utf-8 -*-
"""lstr 端到端自测：唤出/主题/预览/设置/拖动/缩放/隐藏，全程断言窗口矩形。"""
import ctypes
import time
from ctypes import wintypes

u = ctypes.windll.user32
KEYUP = 0x0002
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


def uni(ch: str):
    a = INP()
    a.type = 1
    a.ki.wScan = ord(ch)
    a.ki.dwFlags = 0x0004
    b = INP()
    b.type = 1
    b.ki.wScan = ord(ch)
    b.ki.dwFlags = 0x0004 | 0x0002
    u.SendInput(1, ctypes.byref(a), ctypes.sizeof(INP))
    time.sleep(0.02)
    u.SendInput(1, ctypes.byref(b), ctypes.sizeof(INP))
    time.sleep(0.04)


def key(vk: int):
    u.keybd_event(vk, 0, 0, 0)
    time.sleep(0.02)
    u.keybd_event(vk, 0, KEYUP, 0)
    time.sleep(0.03)


def rect_of() -> wintypes.RECT:
    hwnd = u.FindWindowW(None, "lstr")
    rect = wintypes.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(rect))
    return rect


def shot(name: str):
    from PIL import ImageGrab

    ImageGrab.grab(all_screens=True).save(OUT + "\\" + name)
    print("  shot", name)


def mouse_down(x, y):
    u.SetCursorPos(x, y)
    time.sleep(0.1)
    u.mouse_event(0x0002, 0, 0, 0, 0)
    time.sleep(0.1)


def mouse_to(x, y):
    u.SetCursorPos(x, y)
    time.sleep(0.05)


def mouse_up():
    u.mouse_event(0x0004, 0, 0, 0, 0)
    time.sleep(0.1)


def focus_input():
    hwnd = u.FindWindowW(None, "lstr")
    u.SetForegroundWindow(hwnd)
    time.sleep(0.15)
    rect = wintypes.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(rect))
    u.SetCursorPos((rect.left + rect.right) // 2, rect.top + 30)
    u.mouse_event(0x0002, 0, 0, 0, 0)
    u.mouse_event(0x0004, 0, 0, 0, 0)
    time.sleep(0.2)


def main():
    steps = []

    # 1) 双击 Space 唤出（居中）
    for _ in range(2):
        key(0x20)
        time.sleep(0.06)
    time.sleep(1.0)
    rect = rect_of()
    on_screen = rect.left > -100 and rect.top > -100
    steps.append(f"1 唤出: rect=({rect.left},{rect.top},{rect.right},{rect.bottom}) on_screen={on_screen}")
    shot("t1-show.png")
    assert on_screen, "唤出后窗口不在屏幕上"

    # 2) 输入 wx（浅色主题+预览+图标）
    focus_input()
    for ch in "wx":
        uni(ch)
    time.sleep(1.0)
    shot("t2-wx-light.png")
    steps.append("2 搜索 wx: 浅色+预览+图标（见截图）")

    # 3) 设置浮层：Ctrl+Comma（0xBC）
    key(0x11)
    time.sleep(0.03)
    key(0xBC)
    time.sleep(0.03)
    key(0x11)
    time.sleep(0.03)
    key(0x11)
    time.sleep(0.8)
    rect = rect_of()
    on_screen = rect.left > -100
    steps.append(f"3 设置浮层(Ctrl+,): rect=({rect.left},{rect.top}) on_screen={on_screen}")
    shot("t3-settings.png")

    # 4) 拖不透明度滑块 → 窗口不得跳 (0,0)
    hwnd = u.FindWindowW(None, "lstr")
    rect = wintypes.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(rect))
    slider_x = rect.left + 20 + 120 + 60
    slider_y = rect.top + 62 + 42 + 11
    before = (rect.left, rect.top)
    mouse_down(slider_x, slider_y)
    for dx in (20, 40, 60):
        mouse_to(slider_x + dx, slider_y)
    mouse_up()
    time.sleep(0.6)
    rect = rect_of()
    jumped = rect.left == 0 and rect.top == 0
    steps.append(f"4 拖透明度: 前={before} 后=({rect.left},{rect.top}) 跳左上角={jumped}")
    shot("t4-opacity.png")
    assert not jumped, "窗口跳回了左上角"

    # 5) Esc 关设置
    key(0x1B)
    time.sleep(0.5)

    # 6) 拖顶部横条移动窗口
    rect = rect_of()
    cx, cy = (rect.left + rect.right) // 2, rect.top + 6
    mouse_down(cx, cy)
    for x, y in [(cx - 40, cy - 40), (cx - 80, cy - 80), (cx - 120, cy - 120)]:
        mouse_to(x, y)
    mouse_up()
    time.sleep(0.5)
    rect2 = rect_of()
    moved = (rect2.left - rect.left, rect2.top - rect.top)
    steps.append(f"6 拖动: 位移={moved} 移动={abs(moved[0]) > 50}")
    shot("t5-drag.png")
    assert abs(moved[0]) > 50, "窗口没有被拖动"

    # 7) 拖右边缘缩放
    rect = rect_of()
    ex, ey = rect.right - 3, (rect.top + rect.bottom) // 2
    w0 = rect.right - rect.left
    mouse_down(ex, ey)
    for dx in (40, 80, 120):
        mouse_to(ex + dx, ey)
    mouse_up()
    time.sleep(0.6)
    rect = rect_of()
    grew = (rect.right - rect.left) > w0 + 60
    steps.append(f"7 缩放: 宽 {w0} -> {rect.right - rect.left} 变宽={grew}")
    shot("t6-resize.png")
    assert grew, "窗口没有被缩放"

    # 8) Esc 隐藏
    focus_input()
    key(0x1B)
    time.sleep(0.6)
    rect = rect_of()
    hidden = rect.left <= -20000
    steps.append(f"8 Esc 隐藏: rect=({rect.left},{rect.top}) offscreen={hidden}")
    shot("t7-hidden.png")
    assert hidden, "Esc 没有隐藏窗口"

    print("\n".join(steps))


if __name__ == "__main__":
    main()
