#!/usr/bin/env python3
"""Check every EFI GUID in bootloader/src/efi.rs against the specification.

Why this exists: `EFI_SIMPLE_FILE_SYSTEM_PROTOCOL` was written with one byte
wrong (0x9A instead of 0x39). A GUID is an opaque 128-bit value, so a typo does
not fail loudly — `LocateHandleBuffer` simply returns zero handles, and two
sessions concluded the firmware shipped no storage driver when in fact the
constant was wrong. This check makes that class of bug fail loudly instead.

Expected values are the canonical `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` forms
from UEFI 2.10 / ACPI 6.5, expanded to the 16-byte on-disk order that
`EFI_GUID` uses (first three fields little-endian).
"""

import re
import sys
import pathlib

EXPECTED = {
    "LOADED_IMAGE_PROTOCOL":          "a1:31:1b:5b:62:95:d2:11:8e:3f:00:a0:c9:69:72:3b",
    "SIMPLE_FILE_SYSTEM_PROTOCOL":    "22:5b:4e:96:59:64:d2:11:8e:39:00:a0:c9:69:72:3b",
    "FILE_INFO_GUID":                 "92:6e:57:09:3f:6d:d2:11:8e:39:00:a0:c9:69:72:3b",
    "DEVICE_PATH_PROTOCOL":           "91:6e:57:09:3f:6d:d2:11:8e:39:00:a0:c9:69:72:3b",
    "ACPI_TABLE_GUID":                "30:2d:9d:eb:88:2d:d3:11:9a:16:00:90:27:3f:c1:4d",
    "ACPI_20_TABLE_GUID":             "71:e8:68:88:f1:e4:d3:11:bc:22:00:80:c7:3c:88:81",
    "GRAPHICS_OUTPUT_PROTOCOL":       "de:a9:42:90:dc:23:38:4a:96:fb:7a:de:d0:80:51:6a",
}

PATTERN = re.compile(
    r"pub const (?P<name>[A-Z_0-9]+): Guid = Guid::new\(\s*"
    r"(?P<d1>0x[0-9A-Fa-f_]+),\s*(?P<d2>0x[0-9A-Fa-f_]+),\s*(?P<d3>0x[0-9A-Fa-f_]+),\s*"
    r"\[(?P<d4>[^\]]+)\]",
    re.S,
)


def to_bytes(d1, d2, d3, d4):
    d1 = int(d1.replace("_", ""), 16)
    d2 = int(d2.replace("_", ""), 16)
    d3 = int(d3.replace("_", ""), 16)
    tail = [int(x.strip().replace("_", ""), 16) for x in d4.split(",") if x.strip()]
    if len(tail) != 8:
        raise ValueError(f"expected 8 trailing bytes, got {len(tail)}")
    return (
        d1.to_bytes(4, "little")
        + d2.to_bytes(2, "little")
        + d3.to_bytes(2, "little")
        + bytes(tail)
    )


def main():
    src = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "bootloader/src/efi.rs").read_text()
    found = {m.group("name"): to_bytes(m.group("d1"), m.group("d2"), m.group("d3"), m.group("d4"))
             for m in PATTERN.finditer(src)}

    failures = 0
    for name, want_hex in EXPECTED.items():
        want = bytes(int(x, 16) for x in want_hex.split(":"))
        if name not in found:
            print(f"FAIL {name}: constant not found in efi.rs")
            failures += 1
            continue
        got = found[name]
        if got == want:
            print(f"PASS {name}: {got.hex(':')}")
        else:
            print(f"FAIL {name}: efi.rs has {got.hex(':')}, specification says {want.hex(':')}")
            failures += 1

    extra = set(found) - set(EXPECTED)
    if extra:
        print(f"note: unchecked GUID constants: {', '.join(sorted(extra))}")

    print(f"\n{len(EXPECTED) - failures}/{len(EXPECTED)} GUIDs match the specification")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
