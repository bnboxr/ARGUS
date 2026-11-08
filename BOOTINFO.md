# ARGUS BootInfo — ABI version 1 (frozen)

The contract between the ARGUS UEFI loader and the ARGUS kernel. Declared in
`shared/src/bootinfo.rs`, both sides compiled from that one file.

**Status: frozen at ABI version 1**, 2026-11-08, Phase I milestone 1. Every field
below is implemented, filled by the loader, printed by the kernel, and visible in
`artifacts/boot.log`. The ten groups are the ones `capability-model-v1.md` §5
requires to exist *before* the kernel phase starts, because none of them can be
reconstructed afterwards.

## How it is passed

The loader jumps to the kernel's `e_entry` with `RDI = *const BootInfo`, per the
x86-64 SysV ABI. `RDI` is the only argument. The structure lives in the loader's
own image (a `static`), never in `AllocatePool` memory: everything from the pool
is released the moment `ExitBootServices` returns, so a `BootInfo` in pool memory
would be dangling before the kernel read a byte of it.

`BootInfo` is `#[repr(C)]`, 7040 bytes, no pointers into boot-services memory, no
function pointers, no variable-length arrays.

## Validation order — the kernel must follow it exactly

Each step depends on the previous one; doing them out of order means trusting a
number before it has been checked.

1. `bootinfo != NULL`
2. `magic == 0x4152475553000001`
3. `abi_version == 1`
4. `abi_size == size_of::<BootInfo>()` — **before** any checksum computation,
   because computing a CRC over a length the loader supplied would read memory
   the kernel does not own
5. `checksum` matches a recomputation over `[0, abi_size)`
6. `flags & !FLAG_KNOWN_MASK == 0`
7. only now read the fields

A failure at steps 2–6 prints `stage=bootinfo event=fail reason=E_...` and halts
**without** the `boot_proven` marker, so `run.sh` exits non-zero. That is a
tested path, not an intention: `tests/bootinfo-checksum.sh`.

## Header

| field | offset | bytes | meaning |
|---|---|---|---|
| `magic` | 0 | 8 | `0x4152475553000001` — ASCII `ARGUS`, then the ABI version. Guards against a garbage or NULL pointer. |
| `abi_version` | 8 | 4 | `1`. |
| `abi_size` | 12 | 4 | `size_of::<BootInfo>()` **as compiled by the loader**. |
| `flags` | 16 | 4 | Bitmask, see below. |
| `checksum` | 20 | 4 | CRC-32 over the whole structure with these 4 bytes taken as zero. |

**Why it cannot be added later:** a kernel has no other way to tell "this loader
is from the same revision I was built for" from "this loader wrote a different
layout and the numbers I am reading are fields at the wrong offsets". Without
`abi_size` the kernel cannot even bound its own checksum read. Without the
checksum, a single flipped bit in the memory map silently becomes a page the
kernel believes is usable — and a kernel that hands out the wrong physical frame
has no way to notice.

**Checksum algorithm, exactly:** CRC-32 (IEEE 802.3, reflected polynomial
`0xEDB88320`, initial value `0xFFFFFFFF`, final XOR `0xFFFFFFFF`), computed over
the `abi_size` bytes starting at the first byte of the structure, with the four
`checksum` bytes treated as zero. Implemented in `BootInfo::seal()` and
`BootInfo::verify_checksum()`, the only two places it exists.

The loader seals **once**, immediately before the jump, after `ExitBootServices`
has succeeded and `map_key` is final. An earlier seal would mean the checksum the
kernel verifies was not computed over the bytes the kernel reads — a bug this
milestone actually had, and one that `tests/bootinfo-checksum.sh` caught.

## Flags

| bit | name | meaning |
|---|---|---|
| 0 | `FLAG_MEMMAP_CLAMPED` | the firmware map had more entries than `MAX_MEMORY_REGIONS` (192) |
| 1 | `FLAG_LOADER_RESERVED_CLAMPED` | more loader reservations than `MAX_LOADER_RESERVED` (16) |
| 2 | `FLAG_FRAMEBUFFER_PRESENT` | `framebuffer` describes a real GOP mode |
| 3 | `FLAG_ACPI_PRESENT` | `rsdp_phys` points at an RSDP whose signature and checksum held |
| 4 | `FLAG_INITRD_PRESENT` | `initrd_phys`/`initrd_len` describe a loaded image |
| 15 | `FLAG_TEST_CORRUPTED` | **test only.** Set by a loader built with `--cfg argus_test_corrupt_bootinfo`, after sealing, to prove the kernel refuses a stale checksum. Never present in a normal build. |

Bits 5–14 and 16–31 are reserved and must be 0. The kernel refuses a structure
with any unknown bit set: a flag it does not understand is a loader it does not
understand.

## The ten field groups

### 1. `memory_regions[]` — the UEFI memory map, with attributes

| field | offset | bytes | meaning |
|---|---|---|---|
| `memory_region_count` | 376 | 4 | valid entries |
| `memory_region_capacity` | 380 | 4 | `192` |
| `memory_regions[i].region_type` | 400 + 32·i | 4 | UEFI `EFI_MEMORY_TYPE` |
| `memory_regions[i].phys_start` | +8 | 8 | first physical byte |
| `memory_regions[i].phys_len` | +16 | 8 | length in **bytes** |
| `memory_regions[i].uefi_attribs` | +24 | 8 | the firmware's `Attribute` word, verbatim |

**Why not later:** after `ExitBootServices` the firmware's map is gone. The
`Attribute` word carries cacheability (write-back vs uncacheable) that cannot be
re-derived by inspection, and getting it wrong on MMIO or ACPI-reclaim memory
produces faults that look like hardware failure.

### 2. `map_desc_size`, `map_key`

| field | offset | bytes | meaning |
|---|---|---|---|
| `map_desc_size` | 384 | 4 | `DescriptorSize` from `GetMemoryMap` (48 on OVMF, 40 is the minimum) |
| `map_desc_version` | 388 | 4 | `DescriptorVersion` (`1`) |
| `map_key` | 392 | 8 | the key the **successful** `ExitBootServices` used |

**Why not later:** `map_key` is a one-shot token. It is valid only for the map it
was issued with, and re-querying requires boot services, which no longer exist.
The kernel needs it to know which map generation its copy describes — and, in
Phase V, to prove the map it is using is the one the boot chain measured.

### 3. `rsdp_phys`, `rsdp_checksum`

| field | offset | bytes | meaning |
|---|---|---|---|
| `rsdp_phys` | 6544 | 8 | physical address of the ACPI RSDP (XSDT root), 0 if absent |
| `rsdp_checksum` | 6552 | 1 | the RSDP's own 8-bit sum over its covered length; **0 means valid** |
| `acpi_present` | 6553 | 1 | 1 if signature and checksum both held |
| `rsdp_revision` | 6554 | 1 | 0 = ACPI 1.0 (20-byte checksum), ≥2 = ACPI 2.0+ (36-byte) |

**Why not later:** the RSDP is published through the system table's
*configuration table*, which is boot-services memory. The kernel cannot look it up
again. Without ACPI there is no interrupt controller, no timer, and no way to
enumerate the platform.

### 4. `framebuffer`

| field | offset | bytes | meaning |
|---|---|---|---|
| `framebuffer.phys` | 6560 | 8 | physical base of the linear framebuffer, 0 if absent |
| `framebuffer.width` | 6568 | 4 | horizontal pixels |
| `framebuffer.height` | 6572 | 4 | vertical pixels |
| `framebuffer.stride` | 6576 | 4 | pixels per scan line (not always `width`) |
| `framebuffer.pixel_format` | 6580 | 4 | 0 `RGBx8`, 1 `BGRx8`, 2 `BitMask`, 3 `BltOnly` |
| `framebuffer.size_bytes` | 6584 | 8 | `FrameBufferSize` as reported |

**Why not later:** the GOP handle dies at `ExitBootServices`, and with it the mode
information. The framebuffer itself keeps working; the description of it does not.

### 5. `initrd_phys`, `initrd_len`

| field | offset | bytes | meaning |
|---|---|---|---|
| `initrd_phys` | 6592 | 8 | physical address of the loaded initrd image, 0 if none |
| `initrd_len` | 6600 | 8 | length in bytes |

**Why not later:** the kernel cannot read files. The loader is the only component
that ever has a filesystem, so the service images userspace will run must be
placed in memory and described here, or they do not exist. In this milestone the
image is a placeholder (`INITRD.BIN`, magic `ARGUSIRD`) — and the kernel proves it
by reading that magic back at `initrd_phys`:
`stage=bootinfo event=initrd_readback magic_match=1`.

### 6. `kernel_phys_start`, `kernel_phys_len`, `kernel_phnum`

| field | offset | bytes | meaning |
|---|---|---|---|
| `kernel_phnum` | 52 | 4 | ELF `e_phnum` — program headers in the file |
| `kernel_phys_start` | 6608 | 8 | lowest physical address of the kernel image |
| `kernel_phys_len` | 6616 | 8 | span from `kernel_phys_start` to the highest segment end |
| `segment_count` | 48 | 4 | `PT_LOAD` segments actually loaded |
| `segments[8]` | 56 | 320 | per-segment `vaddr`, `paddr`, `filesz`, `memsz`, `flags` |

**Why not later:** the kernel builds its own page tables in Phase II. It must know
which physical range is *itself*, both to map it and to exclude it from the frame
pool. Guessing the extent from linker symbols is exactly the assumption that
breaks the day the linker script changes.

### 7. `loader_reserved[]`

| field | offset | bytes | meaning |
|---|---|---|---|
| `loader_reserved_count` | 6624 | 4 | valid entries |
| `loader_reserved_capacity` | 6628 | 4 | `16` |
| `loader_reserved[i].start` | 6632 + 24·i | 8 | first byte |
| `loader_reserved[i].len` | +8 | 8 | length |
| `loader_reserved[i].kind` | +16 | 4 | 1 loader image, 2 BootInfo, 3 jump stack, 4 memory-map buffer, 5 kernel file buffer, 6 initrd |

**Why not later:** these are the only regions the kernel must never hand out as
frames, and the list is knowable only while the loader is running. A kernel that
does not know its own `BootInfo` page is live will eventually allocate it and
corrupt the structure it is still reading.

### 8. `boot_slot`, `boot_reason`

| field | offset | bytes | meaning |
|---|---|---|---|
| `boot_slot` | 7016 | 4 | 0 = A, 1 = B. **Always 0 in this milestone** — A/B arrives in slice 3. |
| `boot_reason` | 7020 | 4 | 1 cold boot, 2 warm reboot, 3 slot fallback, 4 recovery. **Always 1 here.** |

**Why not later:** the kernel's confirmation protocol (Phase I §5: the kernel
confirms a successful boot so the loader can cancel a rollback) has to report
which slot it came up from and why. A kernel that starts trusting a slot number
cannot be retrofitted with one — every consumer of the value would have to change
at once. The fields are here, honest about their current constant value, rather
than added later under pressure.

### 9. `debug_serial`

| field | offset | bytes | meaning |
|---|---|---|---|
| `debug_serial.base_port` | 7024 | 2 | I/O port base, `0x3F8` (COM1) |
| `debug_serial.present` | 7026 | 1 | 1 if the loader's probe of that port succeeded |
| `debug_serial.baud` | 7028 | 4 | bits per second as programmed, `115200` |

**Why not later:** the kernel must log *before* it has any driver, including
during the failure of its own early init. It cannot discover the port the loader
used; it has to be told. This is also the channel every test in this repo reads.

### 10. The header — see above

`abi_version`, `abi_size`, `flags`, `checksum`.

## Also present (not one of the ten, but part of the ABI)

| field | offset | bytes | meaning |
|---|---|---|---|
| `boot_tsc` | 24 | 8 | `RDTSC` sampled just before the jump. Not wall-clock: there is no calibrated TSC yet, only a monotonic stamp. |
| `kernel_entry` | 32 | 8 | ELF `e_entry`, so the kernel can check where it thinks it is |
| `kernel_image_bytes` | 40 | 8 | sum of `p_filesz` actually copied |
| `system_table` | 7032 | 8 | `EFI_SYSTEM_TABLE *`, for runtime services only |

## What v1 deliberately does **not** carry

* **No rights, no capabilities.** `capability-model-v1.md` §4: "BootInfo nu
  transportă drepturi — doar resurse." The capability table is issued by the
  kernel at `kmain` from a static table.
* **No page tables, no NX/W^X policy.** The kernel runs on the identity map the
  firmware left behind. Phase II builds its own.
* **No signatures, no measurements.** Phase I slice 2. When they arrive they go
  in a *new* ABI version with the header bumped, not bolted on.
* **No string paths or filenames.** Everything is physical addresses and lengths.

## Changing this ABI

1. Any change to layout or meaning → bump `ABI_VERSION` and `BOOTINFO_MAGIC`.
2. Add the field, update this document, update the kernel's dump in the same
   commit. A field the kernel does not print is a field nobody can verify.
3. `run.sh` requires every field group to appear in the log, so a new field that
   is not printed fails the boot verdict rather than passing silently.
