# 用途：在 PAGEDU64 头部区域实证定位关键结构偏移（物理内存描述符 / 异常记录 / CONTEXT64）。
# Empirically locate structures inside a PAGEDU64 small-dump header.
# Usage: python -I scan_header.py <dump_path>
import struct, sys

d = open(sys.argv[1], 'rb').read()
def u32(o): return struct.unpack_from('<I', d, o)[0]
def u64(o): return struct.unpack_from('<Q', d, o)[0]

print('file size:', hex(len(d)))
print('0x60 VersionUser:', d[0x60:0x80])

p2 = u64(0x48)
print(f'param2 (faulting RIP) = 0x{p2:016x}')

# 1) scan first 0x4000 bytes for a plausible PHYSICAL_MEMORY_DESCRIPTOR64
#    NumberOfRuns (4) small, pad(4), NumberOfPages(8), then runs {base,count} ascending
print('\n-- physical memory descriptor candidates --')
for o in range(0x88, 0x4000 - 0x20, 8):
    nruns = u32(o)
    if 1 <= nruns <= 64:
        npages = u64(o + 8)
        if npages == 0 or npages > 0x10000000:
            continue
        ok = True
        prev = 0
        for i in range(min(nruns, 8)):
            base = u64(o + 16 + i * 16); cnt = u64(o + 24 + i * 16)
            if cnt == 0 or cnt > 0x10000000 or base < prev or base > 0x1000000000:
                ok = False; break
            prev = base
        if ok:
            print(f'  offset 0x{o:x}: nruns={nruns} npages={npages} '
                  f'run0=({u64(o+16):#x},{u64(o+24):#x}) run1=({u64(o+32):#x},{u64(o+40):#x})')

# 2) exception record: code 0xC0000005, ExceptionAddress == p2
print('\n-- exception record candidates --')
for o in range(0, 0x4000 - 0x98, 8):
    if u32(o) == 0xC0000005 and u64(o + 0x10) == p2:
        n = u32(o + 0x18)
        infos = [u64(o + 0x20 + i * 8) for i in range(min(n, 4))]
        print(f'  offset 0x{o:x}: nparams={n} info={[hex(x) for x in infos]}')

# 3) CONTEXT: Rip==p2 at +0xF8, Rsp kernel at +0x98
print('\n-- context candidates --')
for o in range(0, 0x4000 - 0x4D0, 8):
    if u64(o + 0xF8) == p2:
        rsp = u64(o + 0x98)
        if rsp >> 40 == 0xFFFFFF:
            print(f'  offset 0x{o:x}: rsp=0x{rsp:016x} rbp=0x{u64(o+0xA0):016x} '
                  f'flags=0x{u32(o+0x30):x} rax=0x{u64(o+0x78):016x} rcx=0x{u64(o+0x80):016x} '
                  f'rdx=0x{u64(o+0x88):016x} r8=0x{u64(o+0xB8):016x} r9=0x{u64(o+0xC0):016x}')
