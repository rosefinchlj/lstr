import ctypes
import time
from ctypes import wintypes

u = ctypes.windll.user32
kernel32 = ctypes.windll.kernel32

# --- SendInput structures -------------------------------------------------
class KEYBDINPUT(ctypes.Structure):
    _fields_ = [
        ("wVk", wintypes.WORD),
        ("wScan", wintypes.WORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ctypes.POINTER(wintypes.ULONG)),
    ]


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [
        ("dx", wintypes.LONG),
        ("dy", wintypes.LONG),
        ("mouseData", wintypes.DWORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ctypes.POINTER(wintypes.ULONG)),
    ]


class HARDWAREINPUT(ctypes.Structure):
    _fields_ = [
        ("uMsg", wintypes.DWORD),
        ("wParamL", wintypes.WORD),
        ("wParamH", wintypes.WORD),
    ]


class INPUT_UNION(ctypes.Union):
    _fields_ = [("ki", KEYBDINPUT), ("mi", MOUSEINPUT), ("hi", HARDWAREINPUT)]


class INPUT(ctypes.Structure):
    _anonymous_ = ("u",)
    _fields_ = [("type", wintypes.DWORD), ("u", INPUT_UNION)]


KEYEVENTF_KEYUP = 0x0002
KEYEVENTF_UNICODE = 0x0004


def send_unicode(ch: str, keyup: bool = False):
    inp = INPUT()
    inp.type = 1  # INPUT_KEYBOARD
    inp.ki.wVk = 0
    inp.ki.wScan = ord(ch)
    inp.ki.dwFlags = KEYEVENTF_UNICODE | (KEYEVENTF_KEYUP if keyup else 0)
    u.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))


def type_text(text: str):
    for ch in text:
        send_unicode(ch, False)
        time.sleep(0.015)
        send_unicode(ch, True)
        time.sleep(0.04)


def press_vk(vk: int):
    u.keybd_event(vk, 0, 0, 0)
    time.sleep(0.02)
    u.keybd_event(vk, 0, KEYEVENTF_KEYUP, 0)


def focus_window() -> int:
    hwnd = u.FindWindowW(None, "lstr")
    if not hwnd:
        print("lstr window not found")
        return 0
    u.ShowWindow(hwnd, 9)  # SW_RESTORE
    fg = u.GetForegroundWindow()
    fg_thread = u.GetWindowThreadProcessId(fg, None) if fg else 0
    cur = kernel32.GetCurrentThreadId()
    attached = bool(fg_thread and fg_thread != cur)
    if attached:
        u.AttachThreadInput(cur, fg_thread, True)
    u.SetForegroundWindow(hwnd)
    u.SetFocus(hwnd)
    if attached:
        u.AttachThreadInput(cur, fg_thread, False)

    rect = wintypes.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(rect))
    x = (rect.left + rect.right) // 2
    y = rect.top + 35
    u.SetCursorPos(x, y)
    u.mouse_event(0x0002, 0, 0, 0, 0)
    u.mouse_event(0x0004, 0, 0, 0, 0)
    time.sleep(0.25)
    return hwnd


if __name__ == "__main__":
    import sys

    focus_window()
    if len(sys.argv) > 1:
        type_text(sys.argv[1])
    print("done")
