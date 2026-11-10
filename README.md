# ARGUS — Phase I, milestone 1: Boot Foundation

A UEFI loader that validates an ELF64 kernel, fills a versioned BootInfo
structure, calls `ExitBootServices`, and hands control to a kernel that prints
its own banner and proves it arrived. Written from scratch in Rust for
`x86_64-unknown-uefi` and `x86_64-unknown-none`. No Linux, no GRUB, no EDK2 code
linked in.

**Status: milestone 1 complete.** `./build.sh` and `./run.sh` exit 0 from a clean
tree, the serial log shows the whole ordered trace, and every negative test in
`tests/` fails the boot in the way it is supposed to.

```
./build.sh     # builds loader + kernel, assembles ESP, makes dist/argus-disk.img
./run.sh       # boots it in QEMU; exit 0 only on the kernel's halt marker
./tests/run-all.sh   # everything below, ~2 minutes
```

The serial log of the last run is `artifacts/boot.log`; measured timings are in
`artifacts/timing.txt`.

## Toolchain

Rust lives at `/opt/rust` on the build machine and `/etc/profile.d/rust.sh` puts
it on `PATH` for new shells. If you are in a shell that predates that file:

```sh
export RUSTUP_HOME=/opt/rust/rustup CARGO_HOME=/opt/rust/cargo
export PATH=/opt/rust/cargo/bin:$PATH
export CARGO_TARGET_DIR=/opt/argus-target   # /home is a 300 MB volume
```

Targets: `x86_64-unknown-uefi`, `x86_64-unknown-none`. Also needs `qemu-system-x86_64`,
OVMF, `dosfstools`, `mtools`.

## The loading path that is actually active

This was the blocker for two sessions, and the cause was not the firmware.

**Route 1 — `LoadedImage->DeviceHandle` + `OpenProtocol(SIMPLE_FILE_SYSTEM)`.
This is the path in use.** The loader asks the firmware for the handle it was
itself loaded from and opens a filesystem on that exact handle. It does not
enumerate handles and does not resolve a device path, so it cannot be confused by
a device path that the firmware reports differently than expected.

Route 2 (`LocateDevicePath`) and route 3 (`LocateHandleBuffer`) are kept as
ordered fallbacks, each logging its own result, so the log says which one worked:

```
[ARGUS-LOADER] stage=volume route=loaded_image_device_handle found=1 handles=1
[ARGUS-LOADER] stage=volume route=locate_handle_buffer found=1 handles=1
```

Then `\ARGUSKRN.ELF` and `\INITRD.BIN` are read through `EFI_FILE_PROTOCOL`.

If no SimpleFileSystem appears on any route, the loader falls back to `BlockIo`
on the same device handle plus its own FAT16 reader (`bootloader/src/fat.rs`),
and only if that is also absent does it refuse the boot with
`reason=kernel_file_not_found_on_any_route`. The embedded-kernel fallback
described in the brief was **not needed and is not implemented** — the canonical
route works.

### Why it looked like "the firmware has no storage driver"

`EFI_SIMPLE_FILE_SYSTEM_PROTOCOL` was written in `bootloader/src/efi.rs` with one
byte wrong:

```
spec:  964E5B22-6459-11D2-8E39-00A0C969723B
ours:  964E5B22-6459-11D2-8E9A-00A0C969723B   <- 0x9A where 0x39 belongs
```

A GUID is an opaque 128-bit value. A wrong one does not fail; `LocateHandleBuffer`
simply returns zero handles, which reads exactly like "this firmware has no
filesystem driver". Correcting that single byte made the kernel boot, with no
other change to the enumeration code.

`tests/protocol-guid.py` now checks every GUID constant in `efi.rs` against the
UEFI/ACPI specification text, and `tests/run-all.sh` runs it first because it
costs nothing and would have caught this immediately.

## Firmware × drive-interface matrix

One change at a time, plain `OVMF_CODE_4M.fd` unless stated. `sfs` = the
`LoadedImage->DeviceHandle` route found a filesystem. `loader` = our banner
reached the serial port at all (i.e. the firmware chose to run us). `kernel` =
the kernel's `boot_proven` halt marker.

| case | drive | firmware | sfs | loader | kernel | run.sh |
|---|---|---|---|---|---|---|
| plain-virtio | `if=virtio` | `OVMF_CODE_4M.fd` | **yes** | yes | **yes** | 0 |
| plain-ide | `if=ide` | `OVMF_CODE_4M.fd` | **yes** | yes | **yes** | 0 |
| plain-nvme | `if=nvme` | `OVMF_CODE_4M.fd` | no | no | no | 1 |
| secureboot-virtio | `if=virtio` | `OVMF_CODE_4M.ms.fd` | no | no | no | 1 |
| plain-sata | `if=sata` | — | — | — | — | not run |

Reproduce with `./tests/firmware-matrix.sh`; it writes
`artifacts/firmware-matrix.txt` and keeps each serial log.

Reading the table honestly:

* **virtio and ide both work end to end.** The canonical route finds the volume on
  both, and the kernel boots on both. That is the claim this milestone makes.
* **nvme never reaches us.** The firmware enumerates the device and tries to boot
  it, then refuses:
  `BdsDxe: failed to load Boot0001 "UEFI Misc Device" from PciRoot(0x0)/Pci(0x3,0x0): Access Denied`,
  after which it falls through to PXE. Our code never runs, so this says nothing
  about the loader — it is a firmware BDS policy question, and it is the reason
  "we support NVMe" cannot be claimed yet.
* **Secure Boot refuses the unsigned loader.** Expected: signing and key
  enrolment are Phase I slice 2. The row is here so that when slice 2 lands, the
  same command shows the change.
* **`if=sata` is not a QEMU bus type** — QEMU 8.2 rejects it (`unsupported bus
  type 'sata'`), so that row is a harness limitation, not a firmware finding.
  Testing AHCI needs an explicit `-device ahci` plus a drive on its bus. Left as a
  follow-up rather than reported as a result.
* Only the 4 MiB OVMF blobs exist on this machine; there is no 2 MiB
  `OVMF_CODE.fd` to test.

## BootInfo v1

Frozen. Full field-by-field ABI, offsets, checksum definition and the reason each
field cannot be added later: **[BOOTINFO.md](BOOTINFO.md)**.

Header: `magic`, `abi_version=1`, `abi_size=7040`, `flags`, `checksum` (CRC-32 over
the whole structure with those 4 bytes zeroed). The kernel checks magic, version,
size, then checksum, then unknown flags — in that order — and refuses to continue
on any mismatch.

The loader seals **once**, after `ExitBootServices` has succeeded and `map_key` is
final. An earlier version sealed twice, and the second seal silently repaired
corruption injected between them; `tests/bootinfo-checksum.sh` caught that, and
the fix is the ordering you see now.

## Measured timings

From `artifacts/timing.txt`, wall clock on this machine, not estimates:

| event | ms from QEMU launch |
|---|---|
| loader banner | ~3150 |
| `ExitBootServices` returned | ~3370 |
| jump to kernel | ~3370 |
| kernel banner | ~3373 |
| kernel halt marker | ~3550 |

`./build.sh` takes **0.75 s** for an incremental release build (clean: ~40 s).
Almost all of the boot time is OVMF itself; our loader accounts for roughly
200 ms of it.

## What you see on screen

The loader draws the same story it writes to COM1, because the owner's laptop
has no serial port. Nothing here is a boot manager or a visual identity yet —
it is diagnostic text on the framebuffer, and the visual language is a later
slice with its own contract.

The screen has three fixed bands, and no band can be drawn into by another:

- **Header, fixed at the top.** `ARGUS v0.1 phase-I` on the left and the live
  mode on the right (`1920x1080  120x33 cells  scale 2`), with a rule under
  both. The stage list scrolls *below* this band, so the header stays put
  however long the list gets.
- **Stage list, scrolling.** One line per completed step, in the order the
  loader does them:
  1. volume found — the ESP mounted, `\ARGUS\ARGUS.MF` being read
  2. manifest parsed — slot, security version, manifest version
  3. signature valid — RSA-2048 PKCS#1 v1.5, SHA-256
  4. floor value — the anti-rollback floor read and compared
  5. kernel digest — SHA-256 of the kernel, truncated, and its length
  6. ELF validation — segment count and entry point
  7. `ExitBootServices` — firmware released, map key
  8. handoff — the address the kernel was jumped to, and the BootInfo pointer
- **Kernel block, fixed at the bottom.** A rule, `ARGUS kernel is alive` at 2x,
  then `BootInfo checksum verified, halted in hlt` and `serial log: COM1
  115200`. These rows are reserved before the stage list starts, so the two can
  never overlap at any resolution.

A refused boot clears the lower half of the screen and prints `BOOT REFUSED`
with the named code (e.g. `E_MANIFEST_ABSENT`) large and centred, leaving the
stage list above it visible: the code says *what*, the lines above say *where*.

`tests/screens.sh` captures all three states as PNGs in `artifacts/screens/`.
The resolution in each file name is the one the loader reported on serial for
that boot, and the test fails if the PNG header disagrees with it.

**Secure Boot must be off for this build.** The loader is not signed with a key
the firmware trusts, so with Secure Boot enabled the firmware refuses to start
it and nothing is drawn at all. The trust chain inside the loader (manifest
signature, anti-rollback floor, kernel digest) is a separate thing and is
checked regardless; measured boot and a signed loader are later slices.

## Tests

| test | what it proves |
|---|---|
| `tests/protocol-guid.py` | every EFI GUID matches the specification (7/7) |
| `./run.sh` | the positive boot, end to end |
| `tests/negative-tests.sh` | `E_MAGIC`, `E_PHDR_TRUNCATED`, `E_VADDR_UNUSABLE`, kernel-missing — each distinct, each refuses to jump |
| `tests/break-kernel.sh` | `run.sh` exits 1 on a non-ELF kernel and on a missing kernel |
| `tests/bootinfo-checksum.sh` | a BootInfo corrupted after sealing is refused (`E_BOOTINFO_CHECKSUM`) |
| `tests/firmware-matrix.sh` | the table above |

Each test boots a private copy of the disk image under `/opt/argus-test`
(override with `ARGUS_TEST_WORK`), so `dist/` is never modified and the 300 MB
`/home` volume is not filled by 16 MiB images.

## Repository layout

```
bootloader/     UEFI application: efi.rs (protocol bindings), elf.rs (validation),
                fat.rs (BlockIo fallback), serial.rs, main.rs (the boot flow)
kernel/         x86-64 freestanding kernel: entry, serial, BootInfo validation
shared/         bootinfo.rs — the ABI, compiled into both sides
build.sh        builds both, assembles the ESP, builds INITRD.BIN, makes the image
run.sh          QEMU harness with per-line timestamps; exit status is the verdict
tests/          the tests above
BOOTINFO.md     the frozen ABI
```

## Security findings for the boot trust root

These go to the security architect; they are recorded here because they were
learned the hard way in this milestone.

1. **The loader inherits the firmware's storage stack and cannot verify it.** Every
   byte of the kernel image is read through `EFI_FILE_PROTOCOL` on a filesystem
   driver the firmware provides. On a machine whose firmware is compromised, the
   loader's ELF validation and BootInfo checksum are both operating on
   attacker-chosen bytes. Nothing in Phase I slice 1 establishes any trust in that
   stack. This is precisely what measured boot plus signature verification in
   slice 2 has to close, and it is an argument for measuring the loader *before*
   it reads anything, not after.
2. **A missing protocol cannot be inferred from an empty result.** Two sessions
   concluded "the firmware ships no storage driver" from `LocateHandleBuffer`
   returning zero handles, when the real cause was a single wrong byte in a GUID.
   Any security decision of the form "the firmware does not offer X, therefore we
   fall back to Y" needs the negative case proven against a known-good constant
   first, or it becomes an attacker-controlled downgrade path. Concretely: the
   BlockIo/FAT16 fallback in this loader must never be reachable by an adversary
   simply withholding a protocol.
3. **The firmware chose whether to run us at all.** The nvme row shows the
   firmware declining to load our image (`Access Denied`) and moving on. Boot
   policy is the firmware's, not ours, until we own the boot manager.
4. **`ExitBootServices` is not a boundary we control.** Between the last
   `GetMemoryMap` and a successful `ExitBootServices`, the map can change and the
   call fails; the loader retries by re-reading the map. That window is a place
   where a hostile runtime-services implementation could influence what the kernel
   is told about memory.

## Not done yet (Phase I, next slices)

* Image and manifest signing, keys, anti-rollback, TPM 2.0 measured boot (slice 2).
* Boot entries, A/B slots with confirmation and rollback, recovery (slice 3).
  `boot_slot` and `boot_reason` exist in BootInfo and are honestly constant today.
* Renderer, Boot Manager UI, cinematic boot animation (slice 4).
* Real-hardware boot on the owner's PC. Everything above is QEMU + OVMF.
* AHCI/SATA coverage, and whatever makes OVMF's BDS accept the NVMe device.

## Phase I slice 2a — the signed boot manifest

The loader now refuses to load anything before it has verified a signed manifest.

**The flow, in order** (`bootloader/src/security.rs`):

1. find the boot volume; no volume means no manifest, which means no boot
2. read `\ARGUS\ARGUS.MF` (473 bytes, little-endian, `ARGUS.MF` magic)
3. check `key_id` against the key compiled into the loader
4. verify the RSA-2048 PKCS#1 v1.5 / SHA-256 signature over `payload_digest`
5. check `min_loader_version` against the loader's own, and `slot` against the
   slot actually booting
6. read `\ARGUS\FLOOR` and `\ARGUS\CONFIRM`; refuse if `security_version` is
   below either
7. read the boot config and check `config_digest`
8. read the kernel and check `kernel_digest` — the digest **inside the signed
   manifest** is authoritative; there is no detached signature
9. check `initrd_digest`
10. write `\ARGUS\CONFIRM`, and raise `\ARGUS\FLOOR` if the previous boot's
    confirmation allows it
11. `ExitBootServices` and handoff

Each refusal has a named code and its own serial line, and `run.sh` exits
non-zero on any of them: `E_MANIFEST_ABSENT`, `E_MANIFEST_MAGIC`,
`E_MANIFEST_FORMAT`, `E_MANIFEST_DIGEST`, `E_KEY_UNKNOWN`, `E_SIG_INVALID`,
`E_LOADER_TOO_OLD`, `E_SLOT_MISMATCH`, `E_MANIFEST_ROLLBACK`,
`E_FLOOR_CORRUPT`, `E_CONFIG_LEN`, `E_CONFIG_DIGEST`, `E_KERNEL_UNREADABLE`,
`E_KERNEL_LEN`, `E_KERNEL_DIGEST`, `E_INITRD_DIGEST`.

**The keys are test keys, and the target hierarchy is not built yet.** The owner
ratified the hybrid model (`boot-trust-model-v1.md` §5): the user owns K1 (`db`),
K2 (kernel) and K3 (manifest); ARGUS owns only K5 (recovery), which the owner can
delete at enrolment (`recovery_key = none`) and lose fleet-level recovery with it.
Private keys never touch the ARGUS machine — signing happens on an offline host,
and the loader only ever holds public keys. None of that is implemented: there is
no enrolment, no `db` write path and no K5. The key committed here with its
private half is a test key; a boot proven with it proves the mechanism, not the
security, and it is not a trust root. See `keys/test/README.md`.

**The floor is a placeholder, and it is not tamper-proof.** `\ARGUS\FLOOR` is a
4096-byte file on the ESP with two CRC-32-protected copies at offsets 0 and 2048;
the second copy is written first so a power cut leaves the older value readable.
CRC-32 detects corruption, **not** an attacker: anyone who can write the disk can
rewrite both copies and the CRC. This belongs in a TPM NV counter with an
auth policy, and moves there when measured boot lands. Until then the honest
claim is: *the floor stops an accidental or naive downgrade, not a determined
one.*

**The one-boot window.** The floor rises only after a manifest has booted and the
machine came back up to boot again — never at install time, never before
handoff. So between the first boot of a new version and the next boot, an
attacker with disk access can still present the previous version. That is the
same window `boot-trust-model-v1.md` §4 accepts, for the same reason: raising the
floor before the handoff risks a false floor on a machine that never booted.

**Build and verification.** `./build.sh` writes the FAT volume, reads every file
back out of it and compares byte for byte, composes the MBR disk image, extracts
the partition and compares it against the volume that was written, then reads the
files out of the extracted partition. `./build.sh --check` runs the same
verification on an existing `dist/argus-disk.img` without building. Concurrent
builds are serialised with `flock`, because two builds writing `dist/` at once is
what made this script flaky.

**`run.sh` boots whatever is in `dist/` and does not rebuild it.** If a build
failed, the previous image is still there and `run.sh` will boot it and exit 0 —
correctly for the image it was given, misleadingly for the change you just made.
Always `./build.sh && ./run.sh`, and check build.sh's exit status first.
