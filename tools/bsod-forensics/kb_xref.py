# 用途：反汇编 kernelbase.dll 全部可执行节，定位哪些导出函数内部调用 ntdll!NtQueryInformationByName（用户态入口 xref）。
# Find which kernelbase.dll exported functions call ntdll!NtQueryInformationByName.
# Usage: python kb_xref.py
import struct
from capstone import Cs, CS_ARCH_X86, CS_MODE_64

PATH = 'C:/Windows/System32/kernelbase.dll'
d = open(PATH, 'rb').read()
pe = struct.unpack_from('<I', d, 0x3C)[0]
opt_size = struct.unpack_from('<H', d, pe + 20)[0]
opt = pe + 24
dd = lambda i: struct.unpack_from('<II', d, opt + 112 + i * 8)
nsec = struct.unpack_from('<H', d, pe + 6)[0]
secs = []
for i in range(nsec):
    o = opt + opt_size + i * 40
    name = d[o:o+8].rstrip(b'\x00').decode()
    vsz, va, rsz, ra = struct.unpack_from('<IIII', d, o + 8)
    chars = struct.unpack_from('<I', d, o + 36)[0]
    secs.append((name, va, vsz, rsz, ra, chars))
def r2o(r):
    for n, va, vsz, rsz, ra, c in secs:
        if va <= r < va + rsz: return ra + (r - va)

# 1) IAT slots for NtQueryInformationByName
imp_rva, _ = dd(1)
off = r2o(imp_rva)
slots = []
i = 0
while True:
    ilt, ts, fwd, name_rva, ft = struct.unpack_from('<IIIII', d, off + i * 20)
    if ilt == 0 and name_rva == 0: break
    no = r2o(name_rva)
    dll = d[no:d.find(b'\x00', no)].decode()
    toff = r2o(ilt or ft)
    j = 0
    while True:
        tv = struct.unpack_from('<Q', d, toff + j * 8)[0]
        if tv == 0: break
        if not (tv >> 63):
            ho = r2o(tv & 0x7FFFFFFF)
            fn = d[ho + 2:d.find(b'\x00', ho + 2)].decode()
            if fn == 'NtQueryInformationByName':
                slots.append(ft + j * 8)
        j += 1
    i += 1
print('IAT slots (RVA):', [hex(s) for s in slots])

# 2) export table: name -> rva (sorted) for nearest-export attribution
exp_rva, _ = dd(0)
eo = r2o(exp_rva)
n_funcs, n_names = struct.unpack_from('<II', d, eo + 20)
addr_funcs = struct.unpack_from('<I', d, eo + 28)[0]
addr_names = struct.unpack_from('<I', d, eo + 32)[0]
addr_ord = struct.unpack_from('<I', d, eo + 36)[0]
fo, noo, oo = r2o(addr_funcs), r2o(addr_names), r2o(addr_ord)
exports = []
for i in range(n_names):
    nrva = struct.unpack_from('<I', d, noo + i * 4)[0]
    ordv = struct.unpack_from('<H', d, oo + i * 2)[0]
    frva = struct.unpack_from('<I', d, fo + ordv * 4)[0]
    nm = d[r2o(nrva):d.find(b'\x00', r2o(nrva))].decode('ascii', 'replace')
    exports.append((frva, nm))
exports.sort()
def nearest_export(rva):
    lo, hi = 0, len(exports) - 1
    best = None
    while lo <= hi:
        mid = (lo + hi) // 2
        if exports[mid][0] <= rva:
            best = mid; lo = mid + 1
        else:
            hi = mid - 1
    return exports[best] if best is not None else (0, '?')

# 3) scan executable sections for call/jmp qword [rip+disp] hitting those slots
md = Cs(CS_ARCH_X86, CS_MODE_64)
md.detail = True
targets = set(slots)
from capstone.x86 import X86_OP_MEM
for n, va, vsz, rsz, ra, chars in secs:
    if not (chars & 0x20000000): continue
    code = d[ra:ra + rsz]
    for ins in md.disasm(code, va):
        if ins.mnemonic.startswith(('call','jmp')) and ins.operands and ins.operands[0].type == X86_OP_MEM:
            m = ins.operands[0].mem
            if m.base == 41:
                slot = ins.address + ins.size + m.disp
                if slot in targets:
                    er, en = nearest_export(ins.address)
                    print(f'call @0x{ins.address:06x} -> NtQueryInformationByName  (in {en} +0x{ins.address - er:x})')
