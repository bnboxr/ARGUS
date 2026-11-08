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

## Phase I slice 2a (signed manifest)

**No third-party crates were added.** The verifier is in this repository:

| Crate | Kind | Dependencies | Why |
|---|---|---|---|
| `security/` (`argus-security`) | in-repo, `no_std`, no alloc, `forbid(unsafe_code)` | none | SHA-256, U2048 Montgomery arithmetic, RSA-2048 PKCS#1 v1.5 verify (and sign, host only), `ARGUS.MF` encode/parse, CRC-32, FLOOR/CONFIRM records. Written here because the loader is `no_std` with no allocator and the smallest credible Rust RSA crate still pulls in `num-bigint` and `alloc`. |
| `tools/sign-manifest` | in-repo, host std binary | `argus-security` only | Signs and verifies manifests on the build host, so the loader and the signer share one implementation of the format and of RSA. |

A home-made RSA verifier is only acceptable if it is proven, so it is proven
four ways, all run by `tests/crypto-kat.sh` and `cargo test -p argus-security`:
FIPS 180 SHA-256 vectors; modexp answers computed by Python's `pow`; a signature
produced by `openssl dgst -sha256 -sign` verifying inside the crate; and the
signature this repository produces being opened by `openssl pkeyutl
-verifyrecover`. That cross-check is what caught the one real bug in the
arithmetic (see `security/src/bigint.rs`, `mont_mul`).

Signing (`sign_pkcs1v15_sha256`) exists **only in the host tool** — no CRT, no
blinding, not constant-time — and must never be linked into the loader or used
for anything but test manifests.
