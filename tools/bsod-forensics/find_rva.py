# 用途：用 32 字节指纹在 dump 原始字节中定位故障指令在 ntfs.sys 内的 RVA（两个 dump 的交集作为可信命中）。
# Locate the faulting instruction's RVA inside ntfs.sys by byte-fingerprinting:
# the faulting page (param2 & 0xfff offset) should be captured in the dump.
# Format-agnostic: slides fingerprints over the raw dump file.
# Usage: python -I find_rva.py <ntfs.sys> <dmp1> <dmp2>
import struct, sys

def parse_pe_sections(path):
    d = open(path, 'rb').read()
    pe = struct.unpack_from('<I', d, 0x3C)[0]
    assert d[pe:pe+4] == b'PE\x00\x00'
    nsec = struct.unpack_from('<H', d, pe + 6)[0]
    opt_size = struct.unpack_from('<H', d, pe + 20)[0]
    opt = pe + 24
    magic = struct.unpack_from('<H', d, opt)[0]
    assert magic == 0x20B
    image_base = struct.unpack_from('<Q', d, opt + 24)[0]
    size_image = struct.unpack_from('<I', d, opt + 56)[0]
    secs = []
    so = opt + opt_size
    for i in range(nsec):
        o = so + i * 40
        name = d[o:o+8].rstrip(b'\x00').decode('ascii', 'replace')
        vsize, vaddr, rsize, raddr = struct.unpack_from('<IIII', d, o + 8)
        chars = struct.unpack_from('<I', d, o + 36)[0]
        secs.append((name, vaddr, vsize, raddr, rsize, chars))
    return d, image_base, size_image, secs

def main():
    ntfs_path, dmp1, dmp2 = sys.argv[1], sys.argv[2], sys.argv[3]
    d, image_base, size_image, secs = parse_pe_sections(ntfs_path)
    print(f'ntfs.sys: image_base=0x{image_base:x} size_image=0x{size_image:x}')
    for s in secs:
        print(f'  {s[0]:8s} va=0x{s[1]:06x} vsz=0x{s[2]:06x} raw=0x{s[3]:06x} rsz=0x{s[4]:06x} chars=0x{s[5]:08x}')

    off_in_page = 0xd3a  # param2 & 0xfff
    fps = {}  # rva -> fingerprint bytes
    for name, va, vsz, ra, rsz, chars in secs:
        if not (chars & 0x20000000):  # IMAGE_SCN_MEM_EXECUTE
            continue
        start = va
        end = min(va + vsz, va + rsz)
        o = start + (off_in_page - start % 0x1000) % 0x1000
        while o + 32 <= end:
            fp = d[ra + (o - va): ra + (o - va) + 32]
            fps[o] = fp
            o += 0x1000
    print(f'fingerprints from .text: {len(fps)} (page offset 0x{off_in_page:x}, 32 bytes)')

    hits = []
    for dmp in (dmp1, dmp2):
        raw = open(dmp, 'rb').read()
        found = {}
        for rva, fp in fps.items():
            pos = raw.find(fp)
            if pos != -1:
                # ensure unique occurrence in dump
                if raw.find(fp, pos + 1) == -1:
                    found[rva] = pos
        print(f'{dmp.split("/")[-1]}: {len(found)} candidate pages matched')
        hits.append(set(found.keys()))
        for rva, pos in sorted(found.items()):
            print(f'    rva=0x{rva:06x} @file 0x{pos:x}')

    both = hits[0] & hits[1]
    print(f'intersection (both dumps): {[hex(r) for r in sorted(both)]}')
    if both:
        rva = sorted(both)[0]
        print(f'==> fault RVA = 0x{rva:x}')
        # verify with wider context: 48 more bytes after
        print('verify: dump bytes at hit vs ntfs bytes at rva, 64B window:')
        for dmp in (dmp1, dmp2):
            raw = open(dmp, 'rb').read()
            pos = raw.find(fps[rva])
            print('  dmp :', raw[pos-16:pos+48].hex())
        d0, _, _, _ = parse_pe_sections(ntfs_path)
        for name, va, vsz, ra, rsz, chars in secs:
            if va <= rva < va + rsz:
                fo = ra + (rva - va)
                print('  ntfs:', d0[fo-16:fo+48].hex())

if __name__ == '__main__':
    main()
