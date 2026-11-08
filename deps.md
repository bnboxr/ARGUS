# Third-party dependencies

**Zero.** Not one crate outside the Rust toolchain itself, in either the loader
or the kernel.

| crate | version | why |
|-------|---------|-----|
| *(none)* | — | — |

What that costs, and why it was accepted:

* `bootloader/src/efi.rs` is a hand-written transcription of the UEFI 2.10
  structures we call (~330 lines). The alternative was `uefi` + `uefi-services`,
  which is a dependency tree in the boot path of an operating system that
  claims "made from zero". Auditing that tree is work we would have to do
  before trusting it, and it is work we chose to spend on the bindings instead.
* `bootloader/src/elf.rs` and `bootloader/src/fat.rs` are ours: no `goblin`,
  no `xmas-elf`, no `fatfs`.
* `shared/` (BootInfo + COM1 UART) is `#![no_std]` with no dependencies.

Toolchain (not crates, listed so the build is reproducible):

* `rustc` / `cargo` 1.99.1, targets `x86_64-unknown-uefi` and `x86_64-unknown-none`
* `rust-lld` (ships with rustc) — the linker for both targets
* host tools for the disk image: `mkfs.vfat` (dosfstools), `mmd`/`mcopy`/`mdir` (mtools)
* `qemu-system-x86_64` 8.2.2, OVMF (`OVMF_CODE_4M.fd` + `OVMF_VARS_4M.fd`)
