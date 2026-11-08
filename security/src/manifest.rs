//! `ARGUS.MF` — the boot manifest, exactly as `boot-trust-model-v1.md` §3
//! specifies it.
//!
//! One rule from §2: **the manifest is the only authoritative signature.** The
//! kernel's own bytes are authenticated by `kernel_digest` *inside* this signed
//! structure, not by a detached signature that could be swapped along with the
//! image.
//!
//! Layout, little-endian, fixed, packed — every field at a fixed offset so the
//! loader and the host signer cannot disagree about it, and so a byte-level
//! dump of the file can be read against this table by hand:
//!
//! | offset | size | field |
//! |--------|------|-------|
//! | 0      | 8    | `magic` = `"ARGUSMF\0"` |
//! | 8      | 4    | `fmt_version` |
//! | 12     | 4    | `flags` |
//! | 16     | 8    | `manifest_version` |
//! | 24     | 1    | `slot` |
//! | 25     | 8    | `slot_generation` |
//! | 33     | 8    | `security_version` |
//! | 41     | 32   | `kernel_digest` (SHA-256) |
//! | 73     | 8    | `kernel_len` |
//! | 81     | 32   | `initrd_digest` (SHA-256, all zero = no initrd) |
//! | 113    | 32   | `config_digest` (SHA-256) |
//! | 145    | 8    | `config_len` |
//! | 153    | 4    | `min_loader_version` |
//! | 157    | 16   | `key_id` |
//! | 173    | 8    | `issued_counter` |
//! | 181    | 32   | `payload_digest` = SHA-256 of bytes `[0, 181)` |
//! | 213    | 2    | `sig_alg` |
//! | 215    | 2    | `sig_len` |
//! | 217    | 256  | `signature` = RSA-2048 PKCS#1 v1.5 over SHA-256(`payload_digest`) |
//!
//! Total 473 bytes. `payload_digest` covers every preceding field, so a single
//! flipped bit anywhere in the payload is caught before the (much more
//! expensive) RSA operation runs; the signature covers `payload_digest` only,
//! which is what makes the two checks independent: one detects corruption, the
//! other detects forgery.

use crate::sha256;

pub const MAGIC: [u8; 8] = *b"ARGUSMF\0";
pub const FORMAT_VERSION: u32 = 1;
pub const SIG_ALG_RSA2048_PKCS1_SHA256: u16 = 1;
pub const SIG_LEN: u16 = 256;

pub const SIZE: usize = 473;
/// Bytes covered by `payload_digest`: everything before the field itself.
pub const PAYLOAD_LEN: usize = 181;
/// Offset of the signature, for a byte-level reader.
pub const SIGNATURE_OFFSET: usize = 217;

/// Boot slot identity. Phase I slice 2 has one slot; the field exists so a
/// manifest signed for the other slot is structurally invalid for this one.
pub const SLOT_A: u8 = 0;
pub const SLOT_B: u8 = 1;

/// The manifest, decoded. Fields keep the spec's names so the specification can
/// be read alongside the code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Manifest {
    pub fmt_version: u32,
    pub flags: u32,
    pub manifest_version: u64,
    pub slot: u8,
    pub slot_generation: u64,
    pub security_version: u64,
    pub kernel_digest: [u8; 32],
    pub kernel_len: u64,
    pub initrd_digest: [u8; 32],
    pub config_digest: [u8; 32],
    pub config_len: u64,
    pub min_loader_version: u32,
    pub key_id: [u8; 16],
    pub issued_counter: u64,
    pub payload_digest: [u8; 32],
    pub sig_alg: u16,
    pub sig_len: u16,
    pub signature: [u8; 256],
}

/// Why a manifest was refused. Every variant becomes its own named code on the
/// serial log; `code()` is the string the tests grep for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestError {
    /// Fewer than `SIZE` bytes: a truncated file, not a manifest.
    TooShort(usize),
    /// The first eight bytes are not `"ARGUSMF\0"`.
    BadMagic,
    /// A format version this loader does not implement.
    UnsupportedFormat(u32),
    /// Unknown `flags` bits (none are defined in format 1).
    UnsupportedFlags(u32),
    /// A signature algorithm this loader does not implement.
    UnsupportedSigAlg(u16),
    /// `sig_len` is not 256.
    BadSigLen(u16),
    /// `slot` is not one of the defined slots.
    BadSlot(u8),
    /// `payload_digest` does not match the fields it covers: the manifest was
    /// edited after it was produced, or a byte was corrupted in transit.
    PayloadDigestMismatch,
}

impl ManifestError {
    pub fn code(&self) -> &'static str {
        match self {
            ManifestError::TooShort(_) => "E_MANIFEST_FORMAT",
            ManifestError::BadMagic => "E_MANIFEST_MAGIC",
            ManifestError::UnsupportedFormat(_) => "E_MANIFEST_FORMAT",
            ManifestError::UnsupportedFlags(_) => "E_MANIFEST_FORMAT",
            ManifestError::UnsupportedSigAlg(_) => "E_MANIFEST_FORMAT",
            ManifestError::BadSigLen(_) => "E_MANIFEST_FORMAT",
            ManifestError::BadSlot(_) => "E_MANIFEST_FORMAT",
            ManifestError::PayloadDigestMismatch => "E_MANIFEST_DIGEST",
        }
    }

    /// Human-readable detail for the serial line.
    pub fn detail(&self) -> &'static str {
        match self {
            ManifestError::TooShort(_) => "truncated",
            ManifestError::BadMagic => "magic is not ARGUSMF\\0",
            ManifestError::UnsupportedFormat(_) => "unsupported fmt_version",
            ManifestError::UnsupportedFlags(_) => "unknown flag bits set",
            ManifestError::UnsupportedSigAlg(_) => "unsupported sig_alg",
            ManifestError::BadSigLen(_) => "sig_len is not 256",
            ManifestError::BadSlot(_) => "slot out of range",
            ManifestError::PayloadDigestMismatch => "payload_digest does not cover these fields",
        }
    }
}

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}
fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}
fn get_u64(buf: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}
fn get_array<const N: usize>(buf: &[u8], off: usize) -> [u8; N] {
    let mut b = [0u8; N];
    b.copy_from_slice(&buf[off..off + N]);
    b
}

impl Manifest {
    /// A manifest with every field zeroed except the format constants. The
    /// signer fills in the rest; the loader never constructs one this way.
    pub const fn blank() -> Manifest {
        Manifest {
            fmt_version: FORMAT_VERSION,
            flags: 0,
            manifest_version: 0,
            slot: SLOT_A,
            slot_generation: 0,
            security_version: 0,
            kernel_digest: [0; 32],
            kernel_len: 0,
            initrd_digest: [0; 32],
            config_digest: [0; 32],
            config_len: 0,
            min_loader_version: 0,
            key_id: [0; 16],
            issued_counter: 0,
            payload_digest: [0; 32],
            sig_alg: SIG_ALG_RSA2048_PKCS1_SHA256,
            sig_len: SIG_LEN,
            signature: [0; 256],
        }
    }

    /// Serialise to the wire format. `payload_digest` and `signature` are
    /// written as stored — computing them is the signer's job (see
    /// `compute_payload_digest` and `security::rsa`).
    pub fn encode(&self) -> [u8; SIZE] {
        let mut b = [0u8; SIZE];
        b[0..8].copy_from_slice(&MAGIC);
        put_u32(&mut b, 8, self.fmt_version);
        put_u32(&mut b, 12, self.flags);
        put_u64(&mut b, 16, self.manifest_version);
        b[24] = self.slot;
        put_u64(&mut b, 25, self.slot_generation);
        put_u64(&mut b, 33, self.security_version);
        b[41..73].copy_from_slice(&self.kernel_digest);
        put_u64(&mut b, 73, self.kernel_len);
        b[81..113].copy_from_slice(&self.initrd_digest);
        b[113..145].copy_from_slice(&self.config_digest);
        put_u64(&mut b, 145, self.config_len);
        put_u32(&mut b, 153, self.min_loader_version);
        b[157..173].copy_from_slice(&self.key_id);
        put_u64(&mut b, 173, self.issued_counter);
        b[181..213].copy_from_slice(&self.payload_digest);
        put_u16(&mut b, 213, self.sig_alg);
        put_u16(&mut b, 215, self.sig_len);
        b[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 256].copy_from_slice(&self.signature);
        b
    }

    /// SHA-256 over every field before `payload_digest`.
    pub fn compute_payload_digest(&self) -> [u8; 32] {
        let encoded = self.encode();
        sha256::digest(&encoded[..PAYLOAD_LEN])
    }

    /// The digest the RSA signature is computed over: SHA-256 of the 32-byte
    /// `payload_digest` field. Two levels, deliberately — the inner digest is
    /// cheap to check, the outer signature is the authoritative one.
    pub fn signing_digest(&self) -> [u8; 32] {
        sha256::digest(&self.payload_digest)
    }

    /// Parse and structurally validate. This does *not* check the signature and
    /// does *not* compare anything against the machine it is booting; those are
    /// separate refusals with their own codes.
    pub fn parse(bytes: &[u8]) -> Result<Manifest, ManifestError> {
        if bytes.len() < SIZE {
            return Err(ManifestError::TooShort(bytes.len()));
        }
        if bytes[0..8] != MAGIC {
            return Err(ManifestError::BadMagic);
        }

        let m = Manifest {
            fmt_version: get_u32(bytes, 8),
            flags: get_u32(bytes, 12),
            manifest_version: get_u64(bytes, 16),
            slot: bytes[24],
            slot_generation: get_u64(bytes, 25),
            security_version: get_u64(bytes, 33),
            kernel_digest: get_array(bytes, 41),
            kernel_len: get_u64(bytes, 73),
            initrd_digest: get_array(bytes, 81),
            config_digest: get_array(bytes, 113),
            config_len: get_u64(bytes, 145),
            min_loader_version: get_u32(bytes, 153),
            key_id: get_array(bytes, 157),
            issued_counter: get_u64(bytes, 173),
            payload_digest: get_array(bytes, 181),
            sig_alg: get_u16(bytes, 213),
            sig_len: get_u16(bytes, 215),
            signature: get_array(bytes, SIGNATURE_OFFSET),
        };

        if m.fmt_version != FORMAT_VERSION {
            return Err(ManifestError::UnsupportedFormat(m.fmt_version));
        }
        if m.flags != 0 {
            return Err(ManifestError::UnsupportedFlags(m.flags));
        }
        if m.sig_alg != SIG_ALG_RSA2048_PKCS1_SHA256 {
            return Err(ManifestError::UnsupportedSigAlg(m.sig_alg));
        }
        if m.sig_len != SIG_LEN {
            return Err(ManifestError::BadSigLen(m.sig_len));
        }
        if m.slot != SLOT_A && m.slot != SLOT_B {
            return Err(ManifestError::BadSlot(m.slot));
        }
        // Last, because it is the only check that touches the whole buffer.
        if m.compute_payload_digest() != m.payload_digest {
            return Err(ManifestError::PayloadDigestMismatch);
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        let mut m = Manifest::blank();
        m.manifest_version = 7;
        m.slot = SLOT_A;
        m.slot_generation = 3;
        m.security_version = 2;
        m.kernel_digest = [0x11; 32];
        m.kernel_len = 4096;
        m.config_digest = [0x22; 32];
        m.config_len = 128;
        m.min_loader_version = 2;
        m.key_id = [0x33; 16];
        m.issued_counter = 9;
        m.payload_digest = m.compute_payload_digest();
        m
    }

    #[test]
    fn size_is_the_sum_of_the_fields() {
        assert_eq!(
            8 + 4 + 4 + 8 + 1 + 8 + 8 + 32 + 8 + 32 + 32 + 8 + 4 + 16 + 8 + 32 + 2 + 2 + 256,
            SIZE
        );
        assert_eq!(sample().encode().len(), SIZE);
    }

    #[test]
    fn round_trip() {
        let m = sample();
        let bytes = m.encode();
        assert_eq!(Manifest::parse(&bytes), Ok(m));
    }

    #[test]
    fn every_payload_byte_is_covered_by_the_digest() {
        // Flip one bit in each byte of the payload; every one must be caught.
        let m = sample();
        let good = m.encode();
        for i in 0..PAYLOAD_LEN {
            let mut bad = good;
            bad[i] ^= 0x01;
            assert!(
                Manifest::parse(&bad).is_err(),
                "a flipped bit at offset {} was accepted",
                i
            );
        }
    }

    #[test]
    fn magic_is_checked_first() {
        let mut bad = sample().encode();
        bad[0] = b'X';
        assert_eq!(Manifest::parse(&bad), Err(ManifestError::BadMagic));
    }

    #[test]
    fn truncation_is_not_a_manifest() {
        let bytes = sample().encode();
        assert_eq!(
            Manifest::parse(&bytes[..SIZE - 1]),
            Err(ManifestError::TooShort(SIZE - 1))
        );
    }

    #[test]
    fn unknown_algorithm_rejected() {
        let mut m = sample();
        m.sig_alg = 99;
        // Recompute so the digest check cannot mask the algorithm check.
        m.payload_digest = m.compute_payload_digest();
        assert_eq!(
            Manifest::parse(&m.encode()),
            Err(ManifestError::UnsupportedSigAlg(99))
        );
    }

    #[test]
    fn signing_digest_is_sha256_of_payload_digest() {
        let m = sample();
        assert_eq!(m.signing_digest(), sha256::digest(&m.payload_digest));
    }
}
