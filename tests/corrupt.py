#!/usr/bin/env python3
"""Corrupt a copy of the kernel ELF three ways, one per output file."""
import struct, sys, pathlib

src = pathlib.Path(sys.argv[1]).read_bytes()
out = pathlib.Path(sys.argv[2]); out.mkdir(parents=True, exist_ok=True)

def save(name, data):
    (out / name).write_bytes(data)

# 1. bad e_ident magic -> expect E_MAGIC
b = bytearray(src); b[0:4] = b"XXXX"; save("bad-magic.elf", b)

# 2. truncated program header table -> expect E_PHDR_TRUNCATED
#    e_phoff at 0x20, e_phnum at 0x38. Cut the file just after the first phdr.
e_phoff = struct.unpack_from("<Q", src, 0x20)[0]
save("truncated-phdr.elf", src[: e_phoff + 56])

# 3. p_vaddr outside usable RAM -> expect E_VADDR_UNUSABLE
#    First PT_LOAD's p_vaddr (phdr+0x10) and p_paddr (phdr+0x18).
b = bytearray(src)
for i in range(struct.unpack_from("<H", src, 0x38)[0]):
    base = e_phoff + i * 56
    if struct.unpack_from("<I", b, base)[0] == 1:   # PT_LOAD
        struct.pack_into("<Q", b, base + 0x10, 0xFFFF_FFFF_0000_0000)
        struct.pack_into("<Q", b, base + 0x18, 0xFFFF_FFFF_0000_0000)
        break
save("vaddr-unusable.elf", b)
print("wrote 3 corrupted kernels to", out)
