//! The anti-replay floor and the boot-confirmation marker.
//!
//! `boot-trust-model-v1.md` §3/§4: a monotonically increasing
//! `security_version_floor`; a manifest whose `security_version` is below it is
//! refused with `E_MANIFEST_ROLLBACK` however valid its signature is.
//!
//! **This file is a placeholder for a TPM NV index.** The authoritative home is
//! TPM 2.0 NV (atomic per index, sealed, not reachable from a disk image). On
//! this slice it lives in `\ARGUS\FLOOR` on the ESP, which anyone with write
//! access to the ESP can rewrite. That is not tamper-proof and the README says
//! so in as many words. What it *does* prove here is the logic: the refusal, the
//! monotonicity and the write ordering, all of which carry over unchanged when
//! the storage moves to TPM NV.
//!
//! Both files use the same shape as `AB.META` (§4): two copies a fixed distance
//! apart, each with its own sequence number and CRC32. A torn write damages one
//! copy; the reader takes the valid copy with the highest sequence. The writer
//! writes the *second* copy first, so the first copy stays intact until the new
//! value is already durable.
//!
//! CRC32 here detects corruption, not tampering: it is not a MAC and anyone can
//! recompute it. Authenticity of the floor's *value* comes from the fact that it
//! only ever moves up, and only to a `security_version` that a signed manifest
//! already carried.

use crate::crc32::crc32;

/// Bytes in each of the two files.
pub const FILE_SIZE: usize = 4096;
/// Distance between the two copies.
pub const COPY_STRIDE: usize = 2048;

pub const FLOOR_MAGIC: [u8; 8] = *b"ARGFLR01";
pub const CONFIRM_MAGIC: [u8; 8] = *b"ARGCONF1";

/// Outcome of reading a two-copy file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyState<T> {
    /// At least one copy was intact; this is the one with the highest sequence.
    Valid(T),
    /// The file exists and looks like ours, but neither copy passes its CRC.
    /// The caller must refuse to boot: a floor of unknown value cannot be
    /// enforced, and guessing zero would be a rollback bypass.
    Corrupt,
    /// No copy carries our magic: the file was never written (first boot).
    Absent,
}

/// `\ARGUS\FLOOR`: the anti-replay floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloorRecord {
    pub seq: u32,
    pub floor: u64,
}

/// `\ARGUS\CONFIRM`: the marker written just before handoff, read on the next
/// boot to decide whether the previous boot can be considered confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfirmRecord {
    pub seq: u32,
    pub slot: u8,
    pub security_version: u64,
    pub payload_digest: [u8; 32],
}

const FLOOR_RECORD_LEN: usize = 8 + 4 + 8 + 4; // magic, seq, floor, crc
const CONFIRM_RECORD_LEN: usize = 8 + 4 + 1 + 3 + 8 + 32 + 4; // magic, seq, slot, pad, sv, digest, crc

fn encode_floor_record(rec: &FloorRecord) -> [u8; COPY_STRIDE] {
    let mut buf = [0u8; COPY_STRIDE];
    buf[0..8].copy_from_slice(&FLOOR_MAGIC);
    buf[8..12].copy_from_slice(&rec.seq.to_le_bytes());
    buf[12..20].copy_from_slice(&rec.floor.to_le_bytes());
    let crc = crc32(&buf[..20]);
    buf[20..24].copy_from_slice(&crc.to_le_bytes());
    buf
}

fn parse_floor_record(buf: &[u8]) -> Option<(u32, FloorRecord)> {
    if buf.len() < FLOOR_RECORD_LEN || buf[0..8] != FLOOR_MAGIC {
        return None;
    }
    let crc_stored = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
    if crc32(&buf[..20]) != crc_stored {
        return None;
    }
    let seq = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let mut f = [0u8; 8];
    f.copy_from_slice(&buf[12..20]);
    Some((
        seq,
        FloorRecord {
            seq,
            floor: u64::from_le_bytes(f),
        },
    ))
}

fn encode_confirm_record(rec: &ConfirmRecord) -> [u8; COPY_STRIDE] {
    let mut buf = [0u8; COPY_STRIDE];
    buf[0..8].copy_from_slice(&CONFIRM_MAGIC);
    buf[8..12].copy_from_slice(&rec.seq.to_le_bytes());
    buf[12] = rec.slot;
    // buf[13..16] is padding, already zero.
    buf[16..24].copy_from_slice(&rec.security_version.to_le_bytes());
    buf[24..56].copy_from_slice(&rec.payload_digest);
    let crc = crc32(&buf[..56]);
    buf[56..60].copy_from_slice(&crc.to_le_bytes());
    buf
}

fn parse_confirm_record(buf: &[u8]) -> Option<(u32, ConfirmRecord)> {
    if buf.len() < CONFIRM_RECORD_LEN || buf[0..8] != CONFIRM_MAGIC {
        return None;
    }
    let crc_stored = u32::from_le_bytes([buf[56], buf[57], buf[58], buf[59]]);
    if crc32(&buf[..56]) != crc_stored {
        return None;
    }
    let seq = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let mut sv = [0u8; 8];
    sv.copy_from_slice(&buf[16..24]);
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&buf[24..56]);
    Some((
        seq,
        ConfirmRecord {
            seq,
            slot: buf[12],
            security_version: u64::from_le_bytes(sv),
            payload_digest: digest,
        },
    ))
}

/// Read the valid copy with the highest sequence number.
fn select<T: Copy>(
    data: &[u8],
    magic: &[u8; 8],
    parse: fn(&[u8]) -> Option<(u32, T)>,
) -> CopyState<T> {
    let mut best: Option<(u32, T)> = None;
    let mut saw_magic = false;
    for i in 0..2 {
        let start = i * COPY_STRIDE;
        if data.len() < start + COPY_STRIDE {
            continue;
        }
        let chunk = &data[start..start + COPY_STRIDE];
        // Distinguish "not our file" from "our file, damaged": the magic is
        // checked here so a damaged record still counts as a damaged copy.
        if chunk[..8] == magic[..] {
            saw_magic = true;
        }
        if let Some((seq, rec)) = parse(chunk) {
            match best {
                Some((best_seq, _)) if best_seq >= seq => {}
                _ => best = Some((seq, rec)),
            }
        }
    }
    match best {
        Some((_, rec)) => CopyState::Valid(rec),
        None if saw_magic => CopyState::Corrupt,
        None => CopyState::Absent,
    }
}

/// Build the 4096-byte `\ARGUS\FLOOR` image: both copies carry the same value.
pub fn encode_floor_file(rec: &FloorRecord) -> [u8; FILE_SIZE] {
    let mut out = [0u8; FILE_SIZE];
    let copy = encode_floor_record(rec);
    out[0..COPY_STRIDE].copy_from_slice(&copy);
    out[COPY_STRIDE..COPY_STRIDE * 2].copy_from_slice(&copy);
    out
}

pub fn parse_floor_file(data: &[u8]) -> CopyState<FloorRecord> {
    select(data, &FLOOR_MAGIC, parse_floor_record)
}

/// Build the 4096-byte `\ARGUS\CONFIRM` image.
pub fn encode_confirm_file(rec: &ConfirmRecord) -> [u8; FILE_SIZE] {
    let mut out = [0u8; FILE_SIZE];
    let copy = encode_confirm_record(rec);
    out[0..COPY_STRIDE].copy_from_slice(&copy);
    out[COPY_STRIDE..COPY_STRIDE * 2].copy_from_slice(&copy);
    out
}

pub fn parse_confirm_file(data: &[u8]) -> CopyState<ConfirmRecord> {
    select(data, &CONFIRM_MAGIC, parse_confirm_record)
}

/// One copy's bytes, for a writer that wants to write the second copy before
/// the first (so a power cut cannot destroy the last good value).
pub fn floor_copy_bytes(rec: &FloorRecord) -> [u8; COPY_STRIDE] {
    encode_floor_record(rec)
}

pub fn confirm_copy_bytes(rec: &ConfirmRecord) -> [u8; COPY_STRIDE] {
    encode_confirm_record(rec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floor(seq: u32, v: u64) -> FloorRecord {
        FloorRecord { seq, floor: v }
    }

    #[test]
    fn floor_round_trip() {
        let f = floor(3, 11);
        let bytes = encode_floor_file(&f);
        assert_eq!(bytes.len(), FILE_SIZE);
        assert_eq!(parse_floor_file(&bytes), CopyState::Valid(f));
    }

    #[test]
    fn absent_is_not_corrupt() {
        let zeroes = [0u8; FILE_SIZE];
        assert_eq!(parse_floor_file(&zeroes), CopyState::Absent);
        assert_eq!(parse_floor_file(&[]), CopyState::Absent);
        assert_eq!(parse_floor_file(&[0u8; 10]), CopyState::Absent);
    }

    #[test]
    fn one_torn_copy_still_reads() {
        let mut bytes = encode_floor_file(&floor(4, 5));
        // Simulate a torn write of copy 0.
        bytes[12] ^= 0xff;
        assert_eq!(parse_floor_file(&bytes), CopyState::Valid(floor(4, 5)));
    }

    #[test]
    fn highest_sequence_wins() {
        let mut bytes = encode_floor_file(&floor(1, 5));
        let newer = floor_copy_bytes(&floor(2, 9));
        bytes[COPY_STRIDE..COPY_STRIDE * 2].copy_from_slice(&newer);
        assert_eq!(parse_floor_file(&bytes), CopyState::Valid(floor(2, 9)));
        // ... and it does not matter which copy holds which sequence.
        let older = floor_copy_bytes(&floor(1, 5));
        bytes[COPY_STRIDE..COPY_STRIDE * 2].copy_from_slice(&newer);
        bytes[0..COPY_STRIDE].copy_from_slice(&older);
        assert_eq!(parse_floor_file(&bytes), CopyState::Valid(floor(2, 9)));
    }

    #[test]
    fn both_copies_bad_is_corrupt() {
        let mut bytes = encode_floor_file(&floor(4, 5));
        bytes[12] ^= 0xff;
        bytes[COPY_STRIDE + 12] ^= 0xff;
        assert_eq!(parse_floor_file(&bytes), CopyState::Corrupt);
    }

    #[test]
    fn confirm_round_trip() {
        let c = ConfirmRecord {
            seq: 1,
            slot: 0,
            security_version: 2,
            payload_digest: [0x5a; 32],
        };
        let bytes = encode_confirm_file(&c);
        assert_eq!(parse_confirm_file(&bytes), CopyState::Valid(c));
    }

    #[test]
    fn confirm_and_floor_do_not_confuse_each_other() {
        let f = encode_floor_file(&floor(1, 1));
        assert_eq!(parse_confirm_file(&f), CopyState::Absent);
        let c = encode_confirm_file(&ConfirmRecord {
            seq: 1,
            slot: 0,
            security_version: 1,
            payload_digest: [0; 32],
        });
        assert_eq!(parse_floor_file(&c), CopyState::Absent);
    }
}
