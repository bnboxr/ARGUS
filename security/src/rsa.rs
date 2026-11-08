//! RSA-2048 PKCS#1 v1.5 signature verification with SHA-256 (RFC 8017 §8.2.2).
//!
//! Only the operations the boot path needs: verify with a public exponent, and
//! (for the host signing tool) produce a signature with a private exponent.
//! There is no key generation, no PSS, no OAEP and no other digest here on
//! purpose — a second code path in a `no_std` loader is a second place to be
//! wrong. `boot-trust-model-v1.md` §2 forces RSA-2048 PKCS#1 v1.5 / SHA-256
//! because that is what firmware `db` verification implements.

use crate::bigint::{Modulus, U2048, BYTES};
use crate::sha256::{self, DIGEST_LEN};

/// The DER prefix of `DigestInfo` for SHA-256 (RFC 8017 §9.2, note 1):
/// `SEQUENCE { SEQUENCE { OID id-sha256, NULL }, OCTET STRING(32) }`.
pub const SHA256_DIGEST_INFO_PREFIX: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

/// Length of the encoded message: one modulus wide.
pub const EM_LEN: usize = BYTES;

/// An RSA public key, exactly as it appears in `ARGUS.MF`-adjacent metadata.
#[derive(Clone, Copy)]
pub struct PublicKey {
    /// Modulus, big-endian, 256 bytes.
    pub n: [u8; BYTES],
    /// Public exponent. Always 65537 for keys this project generates.
    pub e: u32,
}

impl PublicKey {
    /// The 16-byte `key_id` used in the manifest: the first 16 bytes of
    /// SHA-256 over the big-endian modulus. Deriving it from the key material
    /// rather than storing it by hand means the id cannot drift from the key.
    pub fn key_id(&self) -> [u8; 16] {
        let d = sha256::digest(&self.n);
        let mut id = [0u8; 16];
        id.copy_from_slice(&d[..16]);
        id
    }
}

/// Why a signature was refused. Each variant maps to a serial-log `reason=`
/// so a failed boot names the failure instead of printing "bad signature".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// The signature is not exactly one modulus wide.
    BadSignatureLength(usize),
    /// The signature integer is >= n. Never produced by a real signer.
    SignatureOutOfRange,
    /// The modulus is even or zero, so it is not an RSA modulus.
    BadModulus,
    /// The recovered encoded message is not the PKCS#1 v1.5 encoding of this
    /// digest: wrong padding, wrong DigestInfo, or a different digest.
    BadEncoding,
}

impl VerifyError {
    pub fn reason(&self) -> &'static str {
        match self {
            VerifyError::BadSignatureLength(_) => "signature_length",
            VerifyError::SignatureOutOfRange => "signature_out_of_range",
            VerifyError::BadModulus => "modulus_invalid",
            VerifyError::BadEncoding => "encoding_mismatch",
        }
    }
}

/// The PKCS#1 v1.5 `EMSA-PKCS1-v1_5` encoding of a SHA-256 digest (RFC 8017
/// §9.2): `0x00 || 0x01 || PS(0xff...) || 0x00 || DigestInfo`.
///
/// Verification compares the *whole* encoded block against this function's
/// output rather than parsing the padding. That removes the entire class of
/// "lenient parser accepts a malformed block" bugs (Bleichenbacher-style
/// signature forgeries) without needing to be clever.
pub fn encode_pkcs1v15_sha256(digest: &[u8; DIGEST_LEN]) -> [u8; EM_LEN] {
    let mut em = [0u8; EM_LEN];
    let t_len = SHA256_DIGEST_INFO_PREFIX.len() + DIGEST_LEN; // 51
    let ps_len = EM_LEN - 3 - t_len; // 202 for a 2048-bit modulus

    em[0] = 0x00;
    em[1] = 0x01;
    for b in em.iter_mut().skip(2).take(ps_len) {
        *b = 0xff;
    }
    em[2 + ps_len] = 0x00;
    em[3 + ps_len..3 + ps_len + SHA256_DIGEST_INFO_PREFIX.len()]
        .copy_from_slice(&SHA256_DIGEST_INFO_PREFIX);
    em[3 + ps_len + SHA256_DIGEST_INFO_PREFIX.len()..].copy_from_slice(digest);
    em
}

/// Verify a PKCS#1 v1.5 signature over a 32-byte SHA-256 digest.
///
/// Returns `Ok(())` only when the recovered block is byte-for-byte the
/// expected encoding. Any deviation is `BadEncoding`.
pub fn verify_pkcs1v15_sha256(
    key: &PublicKey,
    digest: &[u8; DIGEST_LEN],
    signature: &[u8],
) -> Result<(), VerifyError> {
    if signature.len() != BYTES {
        return Err(VerifyError::BadSignatureLength(signature.len()));
    }

    let mut sig = [0u8; BYTES];
    sig.copy_from_slice(signature);
    let s = U2048::from_be_bytes(&sig);

    let n = U2048::from_be_bytes(&key.n);
    if s.gte(&n) {
        // RSAVP1 step 1: a signature representative must be in [0, n-1].
        return Err(VerifyError::SignatureOutOfRange);
    }
    let modulus = Modulus::new(n).ok_or(VerifyError::BadModulus)?;

    // m = s^e mod n
    let m = modulus.pow_u32(&s, key.e);
    let em = m.to_be_bytes();

    // Compare in a way that does not return early on the first differing byte.
    let want = encode_pkcs1v15_sha256(digest);
    let mut diff = 0u8;
    for i in 0..EM_LEN {
        diff |= em[i] ^ want[i];
    }
    if diff != 0 {
        return Err(VerifyError::BadEncoding);
    }
    Ok(())
}

/// Produce a PKCS#1 v1.5 signature (host-side signing tool only).
///
/// `d` is the private exponent, big-endian. The private operation is a plain
/// modexp — no CRT, no blinding. That is acceptable here because signing
/// happens offline on a host, with a throwaway test key, once per build; it
/// would not be acceptable in a product signer and this comment is the reason
/// the function lives behind an obvious name.
pub fn sign_pkcs1v15_sha256(
    d_be: &[u8; BYTES],
    n_be: &[u8; BYTES],
    digest: &[u8; DIGEST_LEN],
) -> [u8; BYTES] {
    let n = U2048::from_be_bytes(n_be);
    let modulus = Modulus::new(n).expect("private key modulus must be odd");
    let em = encode_pkcs1v15_sha256(digest);
    let m = U2048::from_be_bytes(&em);
    modulus.pow_be(&m, d_be).to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn unhex(s: &str) -> Vec<u8> {
        let clean: Vec<u8> = s.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
        (0..clean.len() / 2)
            .map(|i| {
                let hi = (clean[i * 2] as char).to_digit(16).unwrap() as u8;
                let lo = (clean[i * 2 + 1] as char).to_digit(16).unwrap() as u8;
                (hi << 4) | lo
            })
            .collect()
    }

    /// The modulus of `keys/test/manifest-key.pem`, i.e. the test key the
    /// loader embeds. Any change to the committed key breaks this test loudly.
    const TEST_N: &str = "c25a125e43c644f32db38775bc2aa15bc63028c548a3062c9c8215d2a19aea7246846be26327e19b75275774f9ca2230f2fb6bfeee07bff0707373a06ec5d0ee0c94d93e029a8e5930ea1175fc31d60c03635deebb2fae8fc0460811f08f9fe429cafe63c349fc451af86a7f04451327874215a59ff09f2325843496beaa9ee9bfca7b71fe87574e17984b8ae27a1408c0844dfcbf1260d7a26bb9ae8f9002de8554be38951b8f57f376ae9a00e2cdb625e421841707ea32fe0b63405becff005bd32bfae7955773863436c9cfc15154499404fca0c3ab71bd9c4711c92c0ad013d68dc247c148c89dd2ac5cf9898278726ca997bf663c961201d9e9c71ea8cf";

    /// `openssl dgst -sha256 -sign keys/test/manifest-key.pem` over the ASCII
    /// string "ARGUS known-answer test", produced outside this codebase.
    const OPENSSL_SIG: &str = "8aea4b63586748b2dd5815fa1aae88a5b9a079ccee5f9bededcb64eb9fd03961a0ccbf6db4fffb4e1a1446ec0c4e7d1a1bc7e51e030b69adca6e0001d89672485ee4ef98ca93a14e2f2ba20f6b9d532fa8b0d548a0a2c237f3a67c569729044c940c1bcffba49dbb37acceacf3e0b65729874e12d00bcc2efb56b8e2f923880f5bbab9ba69b336090a544e023a04bcaf0826a385d5c713344cdb5a301e42a9c5ec3786bf225fcd98902935fe6e7ec7588ea2accce1cc08ae2b39ccb98437da71f559f36bcb1a8b98242d679278a371cc8e812497718789764b1b006fa105f6b785a900f0886cf08e562969bc0d81a5d86475f177b41c85296148eb5c56d859cb";

    const KAT_MESSAGE: &[u8] = b"ARGUS known-answer test";

    #[test]
    fn openssl_signature_verifies() {
        let n = unhex(TEST_N);
        let mut modulus = [0u8; BYTES];
        modulus.copy_from_slice(&n);
        let key = PublicKey { n: modulus, e: 65537 };
        let digest = sha256::digest(KAT_MESSAGE);
        let sig = unhex(OPENSSL_SIG);
        assert_eq!(sig.len(), 256);
        assert_eq!(verify_pkcs1v15_sha256(&key, &digest, &sig), Ok(()));
    }

    #[test]
    fn openssl_signature_rejects_every_single_bit_flip() {
        let n = unhex(TEST_N);
        let mut modulus = [0u8; BYTES];
        modulus.copy_from_slice(&n);
        let key = PublicKey { n: modulus, e: 65537 };
        let digest = sha256::digest(KAT_MESSAGE);
        let sig = unhex(OPENSSL_SIG);
        // Every byte, one bit each: 256 negative cases for the price of one.
        for i in 0..sig.len() {
            let mut bad = sig.clone();
            bad[i] ^= 0x01;
            assert!(
                verify_pkcs1v15_sha256(&key, &digest, &bad).is_err(),
                "flipping byte {} still verified",
                i
            );
        }
    }

    #[test]
    fn wrong_digest_rejected() {
        let n = unhex(TEST_N);
        let mut modulus = [0u8; BYTES];
        modulus.copy_from_slice(&n);
        let key = PublicKey { n: modulus, e: 65537 };
        let sig = unhex(OPENSSL_SIG);
        let digest = sha256::digest(b"a different message");
        assert_eq!(
            verify_pkcs1v15_sha256(&key, &digest, &sig),
            Err(VerifyError::BadEncoding)
        );
    }

    #[test]
    fn wrong_length_rejected() {
        let key = PublicKey {
            n: [0xff; BYTES],
            e: 65537,
        };
        let digest = [0u8; 32];
        assert_eq!(
            verify_pkcs1v15_sha256(&key, &digest, &[0u8; 255]),
            Err(VerifyError::BadSignatureLength(255))
        );
    }

    #[test]
    fn encoding_has_the_rfc_shape() {
        let digest = [0xab; 32];
        let em = encode_pkcs1v15_sha256(&digest);
        assert_eq!(em[0], 0x00);
        assert_eq!(em[1], 0x01);
        assert_eq!(em[2], 0xff);
        assert_eq!(em[2 + 202 - 1], 0xff);
        assert_eq!(em[2 + 202], 0x00);
        assert_eq!(em[3 + 202..3 + 202 + 19], SHA256_DIGEST_INFO_PREFIX);
        assert_eq!(em[3 + 202 + 19..], digest[..]);
    }
}

