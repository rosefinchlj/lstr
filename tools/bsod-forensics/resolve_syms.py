# 用途：经 dbghelp.dll + 微软符号服务器把 ntfs.sys 的 RVA 符号化为 函数名+偏移。
# Resolve ntfs.sys RVAs to public symbol names via dbghelp + Microsoft symbol server.
# Usage: python -I resolve_syms.py <ntfs.sys> <rva_hex> [rva_hex ...]
import ctypes, sys, os
from ctypes import wintypes as W

dbg = ctypes.WinDLL('dbghelp.dll')
k32 = ctypes.WinDLL('kernel32.dll')

SymSetOptions = dbg.SymSetOptions
SymSetOptions.argtypes = [W.DWORD]
SymInitialize = dbg.SymInitialize
SymInitialize.argtypes = [W.HANDLE, W.LPCSTR, W.BOOL]
SymLoadModuleEx = dbg.SymLoadModuleExW
SymLoadModuleEx.argtypes = [W.HANDLE, W.HANDLE, W.LPCWSTR, W.LPCWSTR, ctypes.c_ulonglong, W.DWORD, W.LPVOID, W.DWORD]
SymLoadModuleEx.restype = ctypes.c_ulonglong
SymFromAddr = dbg.SymFromAddr
SymFromAddr.argtypes = [W.HANDLE, ctypes.c_ulonglong, ctypes.POINTER(ctypes.c_ulonglong), W.LPVOID]
SymCleanup = dbg.SymCleanup
SymCleanup.argtypes = [W.HANDLE]

MAX_SYM_NAME = 1024
class SYMBOL_INFO(ctypes.Structure):
    _fields_ = [
        ('SizeOfStruct', W.ULONG), ('TypeIndex', W.ULONG),
        ('Reserved', ctypes.c_ulonglong * 2), ('Index', W.ULONG), ('Size', W.ULONG),
        ('ModBase', ctypes.c_ulonglong), ('Flags', W.ULONG), ('Value', ctypes.c_ulonglong),
        ('Address', ctypes.c_ulonglong), ('Register', W.ULONG), ('Scope', W.ULONG),
        ('Tag', W.ULONG), ('NameLen', W.ULONG), ('MaxNameLen', W.ULONG),
        ('Name', ctypes.c_char * MAX_SYM_NAME),
    ]

def main():
    img = sys.argv[1]
    rvas = [int(a, 16) for a in sys.argv[2:]]
    os.makedirs(r'C:\symbols', exist_ok=True)
    h = k32.GetCurrentProcess()
    SymSetOptions(0x10 | 0x800000 | 0x400000)  # SYMOPT_UNDNAME | SYMOPT_DEBUG... use undname
    if not SymInitialize(h, b'SRV*C:\\symbols*https://msdl.microsoft.com/download/symbols', False):
        print('SymInitialize failed', ctypes.GetLastError()); return
    base = 0x100000000
    mb = SymLoadModuleEx(h, None, img, None, base, 0, None, 0)
    if not mb:
        print('SymLoadModuleEx failed', ctypes.GetLastError()); return
    print(f'module loaded at 0x{mb:x}')
    for rva in rvas:
        si = SYMBOL_INFO()
        si.SizeOfStruct = 88
        si.MaxNameLen = MAX_SYM_NAME
        disp = ctypes.c_ulonglong(0)
        addr = base + rva
        ok = SymFromAddr(h, addr, ctypes.byref(disp), ctypes.byref(si))
        if ok:
            name = si.Name.decode('utf-8', 'replace')
            print(f'  0x{rva:06x}: {name} + 0x{disp.value:x}')
        else:
            print(f'  0x{rva:06x}: <no symbol> err={ctypes.GetLastError()}')
    SymCleanup(h)

if __name__ == '__main__':
    main()
