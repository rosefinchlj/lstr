# -*- coding: utf-8 -*-
"""复现：文件对话框里空输入显示搜索技巧面板时，底部提示条应贴在面板下方，
不能叠在帮助内容中间。截图 + 打印窗口/对话框矩形供人工比对。"""
import ctypes
import os
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
PS1 = ROOT + r"\tools\show_open_dialog.ps1"
OUT = ROOT + r"\target\help_banner.png"
# 用独立 APPDATA 启动：全新历史为空，对话框模式下空输入 → 无结果 →
# 显示搜索技巧面板（复现用户截图：0 项 + 帮助面板 + 对话框提示条）。
TMP_APPDATA = ROOT + r"\target\tmp-appdata-verify"
KEYUP = 0x0002
VK_CONTROL = 0x11


def key(vk, hold=0.03):
    u.keybd_event(vk, 0, 0, 0)
    time.sleep(hold)
    u.keybd_event(vk, 0, KEYUP, 0)
    time.sleep(0.05)


def find_window(cls=None, title=None):
    result = []

    @ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
    def cb(hwnd, _):
        if not u.IsWindowVisible(hwnd):
            return True
        buf = ctypes.create_unicode_buffer(256)
        u.GetWindowTextW(hwnd, buf, 256)
        if title is not None and title not in buf.value:
            return True
        if cls is not None:
            cbuf = ctypes.create_unicode_buffer(256)
            u.GetClassNameW(hwnd, cbuf, 256)
            if cbuf.value != cls:
                return True
        result.append(hwnd)
        return True

    u.EnumWindows(cb, 0)
    return result[0] if result else None


def rect(hwnd):
    r = wintypes.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    return (r.left, r.top, r.right, r.bottom)


def force_foreground(hwnd):
    """跨进程抢前台：AttachThreadInput 到前台线程后 SetForegroundWindow。"""
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
    time.sleep(0.2)
    return ok, u.GetForegroundWindow() == hwnd


def shot(path):
    from PIL import ImageGrab
    img = ImageGrab.grab()
    img.save(path)
    return img


def kill_lstr():
    subprocess.run(["taskkill", "/F", "/IM", "lstr.exe"], capture_output=True)
    for _ in range(20):
        time.sleep(0.25)
        out = subprocess.run(
            ["tasklist", "/FI", "IMAGENAME eq lstr.exe"],
            capture_output=True,
            encoding="mbcs",
            errors="ignore",
        )
        if "lstr.exe" not in (out.stdout or ""):
            return
    print("warn: lstr.exe 仍在运行")


def patch_config():
    """lstr 退出时会把内存里的配置写回，所以必须在 kill 之后再改。"""
    path = os.path.join(TMP_APPDATA, "lstr", "config.toml")
    if not os.path.exists(path):
        return
    text = open(path, encoding="utf-8").read()
    text = (
        text.replace("width = 640.0", "width = 877.0")
        .replace("preview = true", "preview = false")
        .replace("show_history = true", "show_history = false")
    )
    open(path, "w", encoding="utf-8").write(text)
    print("config:", [l for l in text.splitlines() if l.startswith(("width", "preview", "show_history"))])


def main():
    kill_lstr()
    os.makedirs(TMP_APPDATA, exist_ok=True)
    patch_config()
    env = dict(os.environ)
    env["APPDATA"] = TMP_APPDATA
    subprocess.Popen([EXE], env=env)
    time.sleep(3.0)
    dlg_proc = subprocess.Popen(
        ["pwsh", "-NoProfile", "-STA", "-File", PS1],
        creationflags=subprocess.CREATE_NO_WINDOW,
    )
    dlg = None
    for _ in range(40):
        time.sleep(0.25)
        dlg = find_window(cls="#32770")
        if dlg:
            break
    print("dialog_hwnd=", dlg, "rect=", rect(dlg) if dlg else None)
    if dlg:
        ok, is_fg = force_foreground(dlg)
        print("force_foreground ok=", ok, "is_fg=", is_fg)
    # 等自动唤出（WinEvent → 主循环 poll）
    lstr = None
    for _ in range(30):
        time.sleep(0.3)
        lstr = find_window(title="lstr")
        if lstr and rect(lstr)[0] > -10000:
            break
    print("lstr_hwnd=", lstr, "rect=", rect(lstr) if lstr else None)
    if lstr is None or rect(lstr)[0] < -10000:
        print("auto-show did not happen, fallback double-ctrl (plain mode)")
        key(VK_CONTROL)
        key(VK_CONTROL)
        time.sleep(1.5)
        lstr = find_window(title="lstr")
        print("after double-ctrl rect=", rect(lstr) if lstr else None)
    # 对话框模式默认 scope=文件夹；Quick Switch 会把已打开的 Explorer
    # 文件夹置顶。Tab 切到下一个类别（程序）并保持空输入 → 0 项，
    # 正好复现用户截图（0 项 + 搜索技巧 + 对话框提示条）。
    if lstr and rect(lstr)[0] > -10000:
        key(0x09)  # VK_TAB
        time.sleep(1.0)
    time.sleep(0.5)
    img = shot(OUT)
    print("shot=", OUT, img.size)
    if lstr:
        l, t, r, b = rect(lstr)
        if l > -10000:
            crop = img.crop((l, t, r, b))
            crop.save(OUT.replace(".png", "_win.png"))
            print("win_shot=", OUT.replace(".png", "_win.png"), crop.size)
    dlg_proc.terminate()


if __name__ == "__main__":
    main()
