# ARGUS — Phase I, milestone 1

A UEFI loader written from scratch in Rust that reads an ELF64 kernel off the
boot volume, validates it, loads it, exits boot services and jumps to it — with
every step logged to the serial port.

**STATUS: NOT COMPLETE.** The loader builds, boots under QEMU/OVMF and prints
its banner, the full UEFI memory map and the volume-probe trace. It does **not**
yet reach the kernel: on this OVMF build the firmware exposes no
`EFI_SIMPLE_FILE_SYSTEM_PROTOCOL` *and* no `EFI_BLOCK_IO_PROTOCOL` handle by the
time our image runs, so neither the firmware-filesystem path nor the built-in
FAT16 reader can find `\ARGUSKRN.ELF`. See "Firmware notes" below for the
measurements. Everything after `stage=kernel-read` is written but unproven.

Nothing in this repo is claimed to work because it compiles. The serial log in
`artifacts/boot.log` is the only evidence that counts.

## Build

    CARGO_TARGET_DIR=/opt/argus-target ./build.sh

One command. Produces:

    dist/BOOTX64.EFI      the UEFI loader (PE32+)
    dist/ARGUSKRN.ELF     the kernel (freestanding ELF64, ET_EXEC)
    dist/argus-disk.img   16 MiB MBR disk, one 15 MiB FAT16 ESP
    dist/esp/             the same tree as plain files

`CARGO_TARGET_DIR` is only there because `/home` on the build machine is a
300 MB volume; on a normal machine `./build.sh` alone is enough.

Host prerequisites: `rustup target add x86_64-unknown-uefi x86_64-unknown-none`,
`apt-get install -y qemu-system-x86 ovmf dosfstools mtools`.

## Run

    ./run.sh

Boots `dist/argus-disk.img` under QEMU with OVMF, `-serial stdio`, no display.
The full serial log is captured to `artifacts/boot.log`. Exit status:

* `0` — the kernel printed its own banner and reached its halt marker
* `1` — it did not; the log says where the boot stopped
* other — QEMU itself failed to start (passed through unchanged)

The VM is stopped by `timeout` (default 25 s) because the kernel ends in
`loop { hlt }` by design; QEMU has no reason to exit on its own.

Exact command, as printed by `run.sh`:

    qemu-system-x86_64 -machine q35 -m 128M -cpu max \
      -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
      -drive if=pflash,format=raw,unit=1,file=artifacts/OVMF_VARS.run.fd \
      -drive file=dist/argus-disk.img,format=raw,if=virtio \
      -serial stdio -display none -no-reboot -no-shutdown

Firmware: `/usr/share/OVMF/OVMF_CODE_4M.fd` (3653632 bytes) with a throwaway
copy of `/usr/share/OVMF/OVMF_VARS_4M.fd`. The system VARS file is never
written to, so runs are reproducible.

## What each stage prints

Stages are `<stage> <event>` pairs and appear in this order.

| stage | meaning |
|-------|---------|
| `serial init` | COM1 programmed to 115200 8N1; `uart_present=` says whether the port answered |
| `banner start` | loader name, version, BootInfo version |
| `banner system_table` | UEFI revision, config table entry count |
| `memmap received` / `memmap entry=N` / `memmap summary` | every UEFI descriptor, then a summary |
| `kernel-read loaded_image` | our own image base/size and device handle |
| `kernel-read reconnect` | DisconnectController/ConnectController status |
| `kernel-read volume_probe` | three independent attempts to find a filesystem |
| `kernel-read block_probe` | block-level FAT16 volume scan |
| `elf-validate ACCEPT/REJECT` | validation result; a rejection carries a named code |
| `elf-load segment=N` | destination address of each `PT_LOAD` |
| `bootinfo filled` | address, magic, version, size, counts |
| `exit-boot-services attempt/result` | `EFI_SUCCESS` or a status |
| `handoff jump` | entry point and BootInfo pointer, ABI note |
| `[ARGUS-KERNEL] entry banner` | the kernel's own first line — the proof |
| `[ARGUS-KERNEL] bootinfo field=...` | every BootInfo field, walked |
| `[ARGUS-KERNEL] halt boot_proven` | final marker before `loop { hlt }` |

## ELF validation — named rejection codes

`bootloader/src/elf.rs`, applied in order, first failure wins:

`E_HEADER_TRUNCATED` `E_MAGIC` `E_CLASS` `E_ENDIAN` `E_MACHINE` `E_TYPE`
`E_PHENTSIZE` `E_NO_PHDRS` `E_TOO_MANY_PHDRS` `E_PHDR_TRUNCATED`
`E_SEGMENT_TRUNCATED` `E_SEGMENT_SIZE` `E_NO_LOAD_SEGMENTS` `E_TOO_MANY_SEGMENTS`
`E_SEGMENTS_OVERLAP` `E_VADDR_UNUSABLE` `E_ENTRY_NOT_IN_TEXT` `E_OVERLAPS_LOADER`

`tests/negative-tests.sh` corrupts a copy of the kernel three ways and asserts
the matching code appears and the kernel banner does not.

## BootInfo

`shared/src/bootinfo.rs`. `#[repr(C)]`, `version` and `size` first so a kernel
built against an older header refuses a newer structure instead of misreading
it. Every field is documented at its declaration. It lives in the loader's own
image (a static), never in `AllocatePool` memory, because all of that is
released at `ExitBootServices`.

## Firmware notes — the current blocker

Measured on QEMU 8.2.2 + OVMF_CODE_4M.fd, and reproducible with `./run.sh`:

* 238 handles exist; 102 carry `EFI_LOADED_IMAGE_PROTOCOL`, 34 carry
  `EFI_DEVICE_PATH_PROTOCOL`, **0 carry `EFI_SIMPLE_FILE_SYSTEM_PROTOCOL`**.
  The search itself is fine: `LocateHandleBuffer(ByProtocol, DevicePath)`
  returns 34, so the GUID and the table offsets are right.
* `LoadedImage.FilePath` is a bare `MEDIA_FILEPATH_DP` node
  (`\EFI\BOOT\BOOTX64.EFI`) over `PciRoot(0x0)/Pci(0x3,0x0)` — a *relative*
  path, so `LocateDevicePath` cannot resolve a volume from it either.
* `DisconnectController` + recursive `ConnectController` on the device handle
  both return `EFI_SUCCESS`, and the filesystem still does not appear.
* `EFI_BLOCK_IO_PROTOCOL` enumeration also finds nothing, which is why the
  built-in FAT16 reader (`bootloader/src/fat.rs`) cannot help either.

So the firmware read our file and then left us with no route back to the
volume. Next steps, in the order worth trying:

1. Reproduce with the plain `OVMF_CODE.fd` (2 MiB, non-secure-boot) and with
   `-drive if=ide`, to separate "OVMF build quirk" from "virtio path".
2. If the drivers are genuinely absent, embed the kernel path in the loader's
   own device-path lookup by walking `SimpleFileSystem` from
   `gEfiLoadedImageDevicePathGuid`, or read the volume through
   `EFI_PCI_IO_PROTOCOL` directly.
3. Failing all that, this is a firmware/driver-availability finding worth
   recording for the security audit: the boot trust root currently depends on
   a firmware driver the loader cannot re-obtain.

## Layout

    shared/      BootInfo contract + COM1 UART driver (#![no_std], no deps)
    bootloader/  UEFI application: efi.rs (bindings), elf.rs (validation),
                 fat.rs (FAT16 reader), main.rs (the flow)
    kernel/      proof kernel: asm trampoline, bss zeroing, own stack,
                 banner, BootInfo dump, loop { hlt }
    tests/       negative ELF tests, broken-kernel test
    build.sh     one command, exits non-zero on any failure
    run.sh       one command, exits non-zero if the kernel never proves itself
    deps.md      third-party crates: none

> **Not yet verified.** Both test scripts are written but have never produced
> a PASS line, because the boot does not yet reach ELF validation. They must be
> run and pasted into the report before this milestone can be called done.
