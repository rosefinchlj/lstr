# 用途：全 dump 扫描指向 ntfs.sys / ntoskrnl 地址范围的 qword，按页聚类重建崩溃线程内核栈返回地址链。
# Find the crash thread's kernel stack inside the dump by clustering qwords
# that point into ntfs.sys code range, then list them as a return-address chain.
# Usage: python -I stack_scan.py <dmp> <ntfs_base_hex> <ntfs_size_hex> [nt_base_hex nt_size_hex]
import struct, sys

dmp = sys.argv[1]
ntfs_base = int(sys.argv[2], 16)
ntfs_size = int(sys.argv[3], 16)
nt_base = int(sys.argv[4], 16) if len(sys.argv) > 4 else None
nt_size = int(sys.argv[5], 16) if len(sys.argv) > 5 else 0
raw = open(dmp, 'rb').read()

hits = {}  # file page -> [(fileoff, rva, val)]
allq = []
for off in range(0, len(raw) - 8, 8):
    v = struct.unpack_from('<Q', raw, off)[0]
    if ntfs_base <= v < ntfs_base + ntfs_size:
        pg = off & ~0xFFF
        hits.setdefault(pg, []).append((off, v - ntfs_base))
        allq.append((off, v - ntfs_base))

print(f'ntfs-pointing qwords: {len(allq)} on {len(hits)} pages')
# rank pages by hit count
ranked = sorted(hits.items(), key=lambda kv: -len(kv[1]))
for pg, lst in ranked[:6]:
    print(f'\npage @file 0x{pg:06x}: {len(lst)} ntfs pointers')
    # print in ascending file order (= ascending stack VA if this is a stack page)
    for off, rva in sorted(lst):
        print(f'  [+0x{off & 0xFFF:03x}] ntfs+0x{rva:06x}')

# also scan for ntoskrnl pointers near those pages (call chain into the kernel)
if nt_base:
    print(f'\n-- ntoskrnl pointers on the top pages --')
    for pg, lst in ranked[:3]:
        seq = []
        for off in range(pg, pg + 0x1000, 8):
            v = struct.unpack_from('<Q', raw, off)[0]
            if nt_base <= v < nt_base + nt_size:
                seq.append((off - pg, v - nt_base))
            elif ntfs_base <= v < ntfs_base + ntfs_size:
                seq.append((off - pg, ('ntfs', v - ntfs_base)))
        print(f'page @file 0x{pg:06x} combined chain:')
        for o, item in seq:
            if isinstance(item, tuple):
                print(f'  [+0x{o:03x}] ntfs+0x{item[1]:06x}')
            else:
                print(f'  [+0x{o:03x}] nt  +0x{item:07x}')
