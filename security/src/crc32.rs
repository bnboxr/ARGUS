//! CRC32 (IEEE 802.3, polynomial 0xEDB88320, reflected).
//!
//! Used only as a *corruption* check on the anti-replay floor file, never as
//! authentication: anyone with write access to the ESP can recompute a CRC.
//! The README says so in plain words. Integrity against a deliberate attacker
//! comes from the fact that the value is only ever compared against a signed
//! `security_version`; the CRC only catches torn or truncated writes.

/// CRC-32 of `data`, the same value `cksum`-style tools and Python's
/// `zlib.crc32` produce for the same bytes.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answers() {
        // The canonical check value for CRC-32/ISO-HDLC.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0x0000_0000);
        // zlib.crc32(b"argus") == 0x759ab167
        assert_eq!(crc32(b"argus"), 0x759A_B167);
    }

    #[test]
    fn single_bit_change_detected() {
        let a = crc32(b"security_version=7");
        let mut b = *b"security_version=7";
        b[3] ^= 0x01;
        assert_ne!(a, crc32(&b));
    }
}
