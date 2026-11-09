# 用途：capstone 反汇编驱动函数区间，按 .idata 注释导入调用（call/jmp [rip+disp]），可高亮故障指令。
# Disassemble a ntfs.sys function with import-name annotation via .idata.
# Usage: python annotate_func.py <ntfs.sys> <begin_rva_hex> <end_rva_hex> [highlight_rva_hex]
import struct, sys
from capstone import Cs, CS_ARCH_X86, CS_MODE_64
from capstone.x86 import X86_OP_MEM

def load_pe(path):
    d = open(path, 'rb').read()
    pe = struct.unpack_from('<I', d, 0x3C)[0]
    nsec = struct.unpack_from('<H', d, pe + 6)[0]
    opt_size = struct.unpack_from('<H', d, pe + 20)[0]
    opt = pe + 24
    dd_rva = lambda i: struct.unpack_from('<I', d, opt + 96 + i * 8)[0]
    secs = []
    so = opt + opt_size
    for i in range(nsec):
        o = so + i * 40
        name = d[o:o+8].rstrip(b'\x00').decode('ascii', 'replace')
        vsize, vaddr, rsize, raddr = struct.unpack_from('<IIII', d, o + 8)
        secs.append((name, vaddr, vsize, raddr, rsize))
    return d, secs, dd_rva

class Img:
    def __init__(self, path):
        self.d, self.secs, self.dd = load_pe(path)
    def rva2off(self, rva):
        for n, va, vsz, ra, rsz in self.secs:
            if va <= rva < va + rsz:
                return ra + (rva - va)
        return None
    def read(self, rva, n):
        o = self.rva2off(rva)
        return None if o is None else self.d[o:o+n]

def imports(img):
    # IMAGE_DIRECTORY_ENTRY_IMPORT = 1, IAT = 12
    imp_rva = img.dd(1)
    iat_rva = img.dd(12)
    out = {}
    off = img.rva2off(imp_rva)
    if off is None: return out
    i = 0
    while True:
        ilt, ts, fwd, name_rva, ft = struct.unpack_from('<IIIII', img.d, off + i * 20)
        if ilt == 0 and name_rva == 0: break
        noff = img.rva2off(name_rva)
        dll = img.d[noff:img.d.find(b'\x00', noff)].decode('ascii', 'replace')
        # walk thunks
        thunk_rva = ilt or ft
        toff = img.rva2off(thunk_rva)
        j = 0
        while True:
            tv = struct.unpack_from('<Q', img.d, toff + j * 8)[0]
            if tv == 0: break
            if not (tv >> 63):  # not ordinal
                hoff = img.rva2off(tv & 0x7FFFFFFF)
                if hoff:
                    fname = img.d[hoff+2:img.d.find(b'\x00', hoff+2)].decode('ascii', 'replace')
                    out[ft + j * 8] = f'{dll}!{fname}'
            j += 1
        i += 1
    return out

def main():
    img = Img(sys.argv[1])
    begin, end = int(sys.argv[2], 16), int(sys.argv[3], 16)
    hi = int(sys.argv[4], 16) if len(sys.argv) > 4 else -1
    imp = imports(img)
    md = Cs(CS_ARCH_X86, CS_MODE_64)
    md.detail = True
    code = img.read(begin, end - begin)
    for ins in md.disasm(code, begin):
        ann = ''
        if ins.mnemonic.startswith('call') or ins.mnemonic.startswith('jmp'):
            if ins.operands and ins.operands[0].type == X86_OP_MEM:
                m = ins.operands[0].mem
                if m.base == 41:  # RIP
                    slot = ins.address + ins.size + m.disp
                    if slot in imp:
                        ann = f'  ; {imp[slot]}'
        mark = ' <<<<' if ins.address == hi else ''
        print(f'0x{ins.address:06x}: {ins.mnemonic:8s} {ins.op_str}{ann}{mark}')

if __name__ == '__main__':
    main()
