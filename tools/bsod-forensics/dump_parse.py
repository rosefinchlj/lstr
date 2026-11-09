# 用途：一站式解析 Windows x64 小型内核 dump（PAGEDU64）：BugCheck 参数、异常记录、CONTEXT64、模块表（ntfs 基址/大小/故障 RVA）与初步栈走查。
# Parse a Windows x64 "small memory dump" (PAGEDU64 format, DUMP_HEADER64).
# Read-only forensics: header, bugcheck params, module list, crash thread stack.
# Usage: python -I dump_parse.py <dump_path>
import struct, sys, json

def u16(b, o): return struct.unpack_from('<H', b, o)[0]
def u32(b, o): return struct.unpack_from('<I', b, o)[0]
def u64(b, o): return struct.unpack_from('<Q', b, o)[0]

class Dump:
    def __init__(self, path, header_size=0x2000):
        self.data = open(path, 'rb').read()
        h = self.data
        assert h[0:4] == b'PAGE' and h[4:8] == b'DU64', 'not a kernel small dump'
        self.hdr = header_size
        self.major = u32(h, 0x08)
        self.minor = u32(h, 0x0C)
        self.cr3 = u64(h, 0x10)
        self.pfn_db = u64(h, 0x18)
        self.ps_loaded = u64(h, 0x20)
        self.ps_active = u64(h, 0x28)
        self.machine = u32(h, 0x30)
        self.bugcheck = u32(h, 0x38)
        self.params = [u64(h, 0x40 + i * 8) for i in range(4)]
        self.version_user = h[0x60:0x80].split(b'\x00')[0].decode('ascii', 'replace')
        nruns = u32(h, 0x90)
        self.total_pages = u64(h, 0x98)
        self.runs = []
        off = 0xA0
        for _ in range(nruns):
            base = u64(h, off); cnt = u64(h, off + 8)
            self.runs.append((base, cnt))
            off += 16
        # precompute run file-offset table (data follows header page(s) sequentially)
        self.run_file_off = []
        acc = self.hdr
        for base, cnt in self.runs:
            self.run_file_off.append(acc)
            acc += cnt * 0x1000

    def pa_to_off(self, pa):
        page = pa >> 12
        for (base, cnt), foff in zip(self.runs, self.run_file_off):
            if base <= page < base + cnt:
                return foff + (page - base) * 0x1000 + (pa & 0xFFF)
        return None

    def read_phys(self, pa, n=8):
        off = self.pa_to_off(pa)
        if off is None or off + n > len(self.data):
            return None
        return self.data[off:off + n]

    def read_phys_qword(self, pa):
        b = self.read_phys(pa, 8)
        return None if b is None else struct.unpack('<Q', b)[0]

    def va_to_pa(self, va):
        cr3 = self.cr3 & ~0xFFF
        e = self.read_phys_qword(cr3 + ((va >> 39) & 0x1FF) * 8)
        if e is None or not e & 1: return None
        e = self.read_phys_qword((e & 0xFFFFFFFFF000) + ((va >> 30) & 0x1FF) * 8)
        if e is None or not e & 1: return None
        if e & 0x80: return (e & 0xFFFFC0000000) + (va & 0x3FFFFFFF)
        e = self.read_phys_qword((e & 0xFFFFFFFFF000) + ((va >> 21) & 0x1FF) * 8)
        if e is None or not e & 1: return None
        if e & 0x80: return (e & 0xFFFFFFFE00000) + (va & 0x1FFFFF)
        e = self.read_phys_qword((e & 0xFFFFFFFFF000) + ((va >> 12) & 0x1FF) * 8)
        if e is None or not e & 1: return None
        return (e & 0xFFFFFFFFF000) + (va & 0xFFF)

    def read_va(self, va, n):
        out = bytearray()
        while n > 0:
            pa = self.va_to_pa(va)
            if pa is None: return None
            chunk = min(n, 0x1000 - (va & 0xFFF))
            b = self.read_phys(pa, chunk)
            if b is None: return None
            out += b
            va += chunk; n -= chunk
        return bytes(out)

    def read_va_qword(self, va):
        b = self.read_va(va, 8)
        return None if b is None else struct.unpack('<Q', b)[0]

    def read_va_dword(self, va):
        b = self.read_va(va, 4)
        return None if b is None else struct.unpack('<I', b)[0]

    def read_va_word(self, va):
        b = self.read_va(va, 2)
        return None if b is None else struct.unpack('<H', b)[0]

    def read_va_ustr(self, va, maxchars=64):
        b = self.read_va(va, maxchars * 2)
        if b is None: return None
        try:
            s = b.decode('utf-16-le', 'replace')
            return s.split('\x00')[0]
        except Exception:
            return None

    def modules(self):
        mods = []
        head = self.ps_loaded
        flink = self.read_va_qword(head)
        guard = 0
        while flink and flink != head and guard < 1024:
            guard += 1
            entry = flink
            dll_base = self.read_va_qword(entry + 0x30)
            size = self.read_va_dword(entry + 0x40)
            nlen = self.read_va_word(entry + 0x58)
            nbuf = self.read_va_qword(entry + 0x60)
            name = self.read_va_ustr(nbuf, (nlen or 0) // 2 or 32) if nbuf else None
            if dll_base:
                mods.append({'name': name or '?', 'base': dll_base, 'size': size or 0})
            flink = self.read_va_qword(entry)
        return mods

    def find_exception_record(self):
        # EXCEPTION_RECORD64: code(4) flags(4) record(8) addr(8) nparams(4)+align(4) params[15]
        target = self.params[1]
        for o in range(0, self.hdr - 0xA0, 8):
            if u32(self.data, o) == 0xC0000005 and u64(self.data, o + 0x10) == target:
                n = u32(self.data, o + 0x18)
                infos = [u64(self.data, o + 0x20 + i * 8) for i in range(min(n, 15))]
                return {'offset': o, 'code': 0xC0000005, 'address': target, 'info': infos}
        return None

    def find_context(self):
        # CONTEXT64: Rip at +0xF8, Rsp at +0x98; locate by Rip == param2
        target = self.params[1]
        for o in range(0, self.hdr - 0x4D0, 8):
            if u64(self.data, o + 0xF8) == target:
                rsp = u64(self.data, o + 0x98)
                if 0xFFFF000000000000 <= rsp < 0xFFFFFFFFFFFFFFFF and rsp & 0xF == 0:
                    return {'offset': o, 'rsp': rsp, 'rip': target,
                            'rax': u64(self.data, o + 0x78), 'rbx': u64(self.data, o + 0x90),
                            'rcx': u64(self.data, o + 0x80), 'rdx': u64(self.data, o + 0x88),
                            'rbp': u64(self.data, o + 0xA0), 'rsi': u64(self.data, o + 0xA8),
                            'rdi': u64(self.data, o + 0xB0)}
        return None

def main():
    d = Dump(sys.argv[1])
    print(f'build: {d.version_user}  machine=0x{d.machine:x}  cr3=0x{d.cr3:x}')
    print(f'bugcheck: 0x{d.bugcheck:x} params: ' + ' '.join(f'0x{p:016x}' for p in d.params))
    print(f'phys runs: {len(d.runs)} total_pages={d.total_pages}')
    exc = d.find_exception_record()
    print(f'exception record: {exc}')
    mods = d.modules()
    print(f'modules: {len(mods)}')
    ntfs = None
    for m in mods:
        if (m['name'] or '').lower() == 'ntfs.sys':
            ntfs = m
    if ntfs:
        rva = d.params[1] - ntfs['base']
        print(f"NTFS: base=0x{ntfs['base']:x} size=0x{ntfs['size']:x}  fault RVA=0x{rva:x}")
    else:
        print('NTFS module NOT found in list')
        for m in mods[:8]:
            print(f"  {m['name']:24s} 0x{m['base']:x} 0x{m['size']:x}")
    ctx = d.find_context()
    print(f'context: {ctx}')
    if ctx:
        rsp = ctx['rsp']
        print('stack walk (qwords pointing into loaded modules):')
        for i in range(0, 0x200):
            va = rsp + i * 8
            v = d.read_va_qword(va)
            if v is None: break
            for m in mods:
                if m['base'] <= v < m['base'] + m['size']:
                    print(f'  [rsp+0x{i*8:04x}] 0x{v:016x}  {m["name"]}+0x{v - m["base"]:x}')
                    break

if __name__ == '__main__':
    main()
