# -*- coding: utf-8 -*-
"""PrintWindow 抓 lstr 窗口（锁屏时屏幕截图拿不到窗口，用窗口自绘表面）。"""
import ctypes
import os
import sys
from ctypes import wintypes

u = ctypes.windll.user32
g32 = ctypes.windll.gdi32
try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except Exception:
    pass

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target", "mask-op", "printwindow.png")
PW_RENDERFULLCONTENT = 0x00000002


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


def main():
    from PIL import Image

    hwnd = find_window()
    if not hwnd:
        print("no window")
        return 1
    r = wintypes.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    w, h = r.right - r.left, r.bottom - r.top
    print("hwnd", hwnd, "size", w, h)
    hdc = u.GetWindowDC(hwnd)
    memdc = g32.CreateCompatibleDC(hdc)
    bmp = g32.CreateCompatibleBitmap(hdc, w, h)
    g32.SelectObject(memdc, bmp)
    ok = u.PrintWindow(hwnd, memdc, PW_RENDERFULLCONTENT)
    print("PrintWindow =", ok)

    class BITMAPINFOHEADER(ctypes.Structure):
        _fields_ = [
            ("biSize", wintypes.DWORD),
            ("biWidth", wintypes.LONG),
            ("biHeight", wintypes.LONG),
            ("biPlanes", wintypes.WORD),
            ("biBitCount", wintypes.WORD),
            ("biCompression", wintypes.DWORD),
            ("biSizeImage", wintypes.DWORD),
            ("biXPelsPerMeter", wintypes.LONG),
            ("biYPelsPerMeter", wintypes.LONG),
            ("biClrUsed", wintypes.DWORD),
            ("biClrImportant", wintypes.DWORD),
        ]

    bi = BITMAPINFOHEADER()
    bi.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    bi.biWidth = w
    bi.biHeight = -h
    bi.biPlanes = 1
    bi.biBitCount = 32
    bi.biCompression = 0
    buf = ctypes.create_string_buffer(w * h * 4)
    g32.GetDIBits(memdc, bmp, 0, h, buf, ctypes.byref(bi), 0)
    img = Image.frombuffer("RGBA", (w, h), buf.raw, "raw", "BGRA", 0, 1)
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    img.convert("RGB").save(OUT)
    print("saved", OUT)
    g32.DeleteObject(bmp)
    g32.DeleteDC(memdc)
    u.ReleaseDC(hwnd, hdc)
    return 0


if __name__ == "__main__":
    sys.exit(main())
