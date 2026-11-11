# -*- coding: utf-8 -*-
"""验证：记事本 Ctrl+O 自动唤出 lstr（Win11 XAML 宿主），
并检查对话框 Quick Switch 里收藏的应用显示为应用而不是文件夹。"""
import ctypes
import subprocess
import time
from ctypes import wintypes

u = ctypes.windll.user32
k32 = ctypes.windll.kernel32
try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except Exception:
    pass

ROOT = r"E:\use\ai\deepseek\Rust\lstr"
EXE = ROOT + r"\target\release\lstr.exe"
OUT = ROOT + r"\target\dialog_quick.png"


def info(h):
    if not h:
        return None
    b = ctypes.create_unicode_buffer(256)
    u.GetWindowTextW(h, b, 256)
    c = ctypes.create_unicode_buffer(256)
    u.GetClassNameW(h, c, 256)
    r = wintypes.RECT()
    u.GetWindowRect(h, ctypes.byref(r))
    return (hex(h), c.value, b.value, (r.left, r.top, r.right, r.bottom))


def force(h):
    fg = u.GetForegroundWindow()
    cur = k32.GetCurrentThreadId()
    tf = u.GetWindowThreadProcessId(fg, None)
    tt = u.GetWindowThreadProcessId(h, None)
    if tf:
        u.AttachThreadInput(cur, tf, True)
    if tt:
        u.AttachThreadInput(cur, tt, True)
    u.BringWindowToTop(h)
    ok = u.SetForegroundWindow(h)
    u.SetFocus(h)
    if tt:
        u.AttachThreadInput(cur, tt, False)
    if tf:
        u.AttachThreadInput(cur, tf, False)
    time.sleep(0.4)
    return ok, u.GetForegroundWindow() == h


def key(vk):
    u.keybd_event(vk, 0, 0, 0)
    time.sleep(0.04)
    u.keybd_event(vk, 0, 2, 0)
    time.sleep(0.08)


def ctrl(vk):
    u.keybd_event(0x11, 0, 0, 0)
    time.sleep(0.04)
    key(vk)
    u.keybd_event(0x11, 0, 2, 0)
    time.sleep(0.15)


def find(title_kw, cls=None):
    out = []

    @ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
    def cb(h, _):
        if u.IsWindowVisible(h):
            b = ctypes.create_unicode_buffer(256)
            u.GetWindowTextW(h, b, 256)
            c = ctypes.create_unicode_buffer(256)
            u.GetClassNameW(h, c, 256)
            if title_kw in b.value and (cls is None or c.value == cls):
                out.append(h)
        return True

    u.EnumWindows(cb, 0)
    return out[0] if out else None


def visible(h):
    return bool(h) and bool(u.IsWindowVisible(h)) and info(h)[3][0] > -10000


def running():
    out = subprocess.run(
        ["tasklist", "/FI", "IMAGENAME eq lstr.exe"],
        capture_output=True,
        encoding="mbcs",
        errors="ignore",
    )
    return "lstr.exe" in (out.stdout or "")


def main():
    print("fg at start", info(u.GetForegroundWindow()))
    if not running():
        subprocess.Popen([EXE])
        time.sleep(3.0)
    # lstr 可见时先收起：自动唤出只在隐藏时触发（不打扰正在输入的用户）。
    lstr = find("lstr")
    if visible(lstr):
        force(lstr)
        key(0x1B)
        time.sleep(0.6)
    print("lstr before", info(find("lstr")), "visible", visible(find("lstr")))
    np = find("记事本") or find("Notepad")
    if not np:
        subprocess.Popen(["notepad.exe"])
        time.sleep(2.0)
        np = find("记事本") or find("Notepad")
    print("notepad", info(np))
    u.ShowWindow(np, 9)  # SW_RESTORE：之前测试可能把它最小化了
    time.sleep(0.5)
    # 关掉可能残留的「打开」对话框
    for _ in range(3):
        dlg = find("打开", "#32770")
        if not dlg:
            break
        force(dlg)
        key(0x1B)
        time.sleep(0.4)
    print("force np", force(np))
    ctrl(0x4F)
    # 等自动唤出（最多 5s）
    lstr = None
    for _ in range(25):
        time.sleep(0.2)
        lstr = find("lstr")
        if visible(lstr):
            break
    print("dialog", info(find("打开", "#32770")))
    print("lstr", info(lstr), "visible", visible(lstr))
    time.sleep(2.0)  # 等异步图标加载
    if visible(lstr):
        l, t, r, b = info(lstr)[3]
        from PIL import ImageGrab
        img = ImageGrab.grab(bbox=(l, t, r, b))
        img.save(OUT)
        print("shot", OUT, img.size)


if __name__ == "__main__":
    main()
