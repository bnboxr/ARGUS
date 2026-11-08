//! ARGUS boot security primitives — `no_std`, no dependencies.
//!
//! Shared by two very different binaries, which is the point: the UEFI loader
//! (`bootloader/`, `no_std`, decides whether to execute the kernel) and the
//! host signing tool (`tools/sign-manifest`, ordinary `std`) compile the *same*
//! `ARGUS.MF` encoder, the same SHA-256 and the same RSA code. There is no way
//! for the signer and the verifier to drift apart in field order, endianness or
//! padding, because there is only one implementation of each.
//!
//! What is in here, and what is deliberately not:
//!
//! * [`sha256`] — SHA-256 (FIPS 180-4).
//! * [`bigint`] — 2048-bit arithmetic and Montgomery modexp.
//! * [`rsa`] — RSA-2048 PKCS#1 v1.5 signature verify (and sign, for the host
//!   tool only).
//! * [`manifest`] — the `ARGUS.MF` layout from `boot-trust-model-v1.md` §3.
//! * [`floor`] — the anti-replay floor and confirmation marker records (§4).
//! * [`crc32`] — corruption detection for the two-copy records.
//!
//! Not here: key generation, X.509, TPM, enrolment, A/B slot metadata. Slice 2a
//! covers the signed manifest and the floor; nothing else.
//!
//! `#![no_std]` everywhere except under `cargo test`, where the test harness
//! needs `std`. The crate itself never uses `alloc`.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod bigint;
pub mod crc32;
pub mod floor;
pub mod manifest;
pub mod rsa;
pub mod sha256;

/// Version of this library, reported by the host tool and greppable in logs.
pub const VERSION: &str = "0.1.0";
