//! Boot security for Phase I, slice 2a: the signed manifest and the anti-replay
//! floor.
//!
//! Everything in here runs **before** a single byte of the kernel is executed,
//! and the first thing it does is refuse to continue if it cannot prove what it
//! is about to run. The order is deliberate and is the whole design:
//!
//! 1. read `\ARGUS\ARGUS.MF` and parse it (`argus_security::manifest`)
//! 2. check `key_id`, then the RSA-2048 PKCS#1 v1.5 signature over
//!    `payload_digest` with the public key compiled into this image
//! 3. check `min_loader_version` against our own, then `slot` against the slot
//!    actually booting
//! 4. read the floor and the confirmation marker, and refuse a manifest whose
//!    `security_version` is below either (`E_MANIFEST_ROLLBACK`)
//! 5. only then read the kernel, and check the digest *inside the signed
//!    manifest* against the exact bytes loaded — the manifest is authoritative,
//!    never a detached signature
//! 6. check the boot config the same way
//! 7. just before handoff, write the confirmation marker and, if the previous
//!    boot's marker allows it, raise the floor
//!
//! Each refusal has its own named code and its own serial line, because "boot
//! failed" is not a diagnosis. `tests/manifest-tests.sh` asserts on the codes.
//!
//! What this does *not* do, and the README says so: no enrolment, no A/B slots,
//! no TPM. The floor lives in a file anyone with the disk can rewrite.

use core::fmt::{self, Write};
use core::ptr::null_mut;

use argus_security::floor::{
    self, ConfirmRecord, CopyState, FloorRecord, FILE_SIZE,
};
use argus_security::manifest::{Manifest, SLOT_A};
use argus_security::rsa::{self, PublicKey};
use argus_security::sha256;
use argus_shared::fbcon::Line;
use argus_shared::screen;
use argus_shared::serial::Serial;

use crate::efi::*;
use crate::test_key;

/// The loader's own version, as a number, for `min_loader_version`.
///
/// Bumped whenever the manifest format or the checks change in a way an older
/// loader would get wrong. A manifest that demands more than this refuses to
/// boot (`E_LOADER_TOO_OLD`) instead of being misread.
pub const LOADER_VERSION_NUM: u32 = 3;

/// Where the signed manifest lives. 8.3-safe, so the fallback FAT reader could
/// reach it if it ever grows directory support.
pub const MANIFEST_PATH: &str = "\\ARGUS\\ARGUS.MF";
/// The kernel ELF, authenticated by `kernel_digest` in the signed manifest.
pub const KERNEL_PATH: &str = "\\ARGUSKRN.ELF";

/// The boot config, covered by `config_digest`.
pub const CONFIG_PATH: &str = "\\ARGUS\\BOOT.CFG";
/// The anti-replay floor. **Placeholder for a TPM NV index** — see README.
pub const FLOOR_PATH: &str = "\\ARGUS\\FLOOR";
/// The "a boot of this manifest is in progress" marker.
pub const CONFIRM_PATH: &str = "\\ARGUS\\CONFIRM";

/// The slot this image boots. Phase I slice 2a has one slot; the field exists
/// so a manifest signed for another slot is structurally invalid here.
pub const BOOT_SLOT: u8 = SLOT_A;

/// Upper bound on a file we read into pool memory. The manifest is 473 bytes
/// and the config is a few hundred; 64 KiB is generous and bounded.
const MAX_SMALL_FILE: usize = 64 * 1024;

/// The public key the loader trusts, compiled into the image.
///
/// **This is the test key.** It is committed to the repository along with its
/// private half. See `test_key.rs` and `keys/test/README.md`; the owner has not
/// chosen the real trust root yet (`boot-trust-model-v1.md` §5).
const MANIFEST_KEY: PublicKey = PublicKey {
    n: test_key::TEST_MANIFEST_PUBKEY_N,
    e: test_key::TEST_MANIFEST_PUBKEY_E,
};

/// A refusal: which stage refused and which named code it used.
#[derive(Clone, Copy)]
pub struct Refusal {
    pub stage: &'static str,
    pub code: &'static str,
}

/// Print a byte slice as lowercase hex without allocating.
pub struct Hex<'a>(pub &'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// File helpers
// ---------------------------------------------------------------------------

/// Open `path` on `sfs` and read the whole file into pool memory.
///
/// The caller owns the buffer; the firmware allocated it and it stays valid
/// until `ExitBootServices`.
pub fn read_file(
    bs: &BootServices,
    sfs: *mut SimpleFileSystem,
    path: &str,
    max_len: usize,
) -> Result<(*mut u8, usize), &'static str> {
    if sfs.is_null() {
        return Err("no_volume");
    }

    let mut root: *mut File = null_mut();
    // SAFETY: `sfs` is a protocol pointer the firmware returned and that we have
    // already used; `open_volume` writes the root handle into `root`.
    let s = unsafe { ((*sfs).open_volume)(sfs, &mut root as *mut *mut File) };
    if is_error(s) || root.is_null() {
        return Err("open_volume_failed");
    }

    let mut wide = [0u16; 128];
    let mut n = 0usize;
    for unit in path.encode_utf16() {
        if n + 1 >= wide.len() {
            return Err("path_too_long");
        }
        wide[n] = unit;
        n += 1;
    }
    wide[n] = 0;

    let mut file: *mut File = null_mut();
    // SAFETY: `root` is live, `wide` is a NUL-terminated UCS-2 path, and mode
    // READ with attributes 0 is the documented read-only open.
    let s = unsafe {
        ((*root).open)(
            root,
            &mut file as *mut *mut File,
            wide.as_ptr(),
            EFI_FILE_MODE_READ,
            0,
        )
    };
    if is_error(s) || file.is_null() {
        return Err("not_found");
    }

    // Two-call idiom: a null buffer returns the required size.
    let mut info_size: usize = 0;
    // SAFETY: documented two-call idiom; a null buffer returns the size.
    let s = unsafe { ((*file).get_info)(file, &FILE_INFO_GUID, &mut info_size, null_mut()) };
    if s != EFI_BUFFER_TOO_SMALL && is_error(s) {
        return Err("get_info_failed");
    }
    if info_size < 16 || info_size > 4096 {
        return Err("get_info_size_implausible");
    }

    let mut info_buf: *mut u8 = null_mut();
    // SAFETY: allocating `info_size` bytes for the firmware to fill.
    let s = unsafe {
        (bs.allocate_pool)(EFI_LOADER_DATA, info_size, &mut info_buf as *mut *mut u8)
    };
    if is_error(s) || info_buf.is_null() {
        return Err("allocate_info_buffer_failed");
    }
    // SAFETY: `info_buf` has `info_size` writable bytes.
    let s = unsafe { ((*file).get_info)(file, &FILE_INFO_GUID, &mut info_size, info_buf) };
    if is_error(s) {
        return Err("get_info_failed");
    }
    // EFI_FILE_INFO: Size at 0, FileSize at 8.
    // SAFETY: at least 16 bytes were filled, checked above.
    let file_size = unsafe { core::ptr::read_unaligned((info_buf as *const u64).add(1)) } as usize;
    // SAFETY: `info_buf` came from `allocate_pool`; we own it.
    unsafe { (bs.free_pool)(info_buf) };

    if file_size == 0 || file_size > max_len {
        return Err("file_size_implausible");
    }

    let mut buf: *mut u8 = null_mut();
    // SAFETY: allocating `file_size` bytes for the file contents.
    let s = unsafe {
        (bs.allocate_pool)(EFI_LOADER_DATA, file_size, &mut buf as *mut *mut u8)
    };
    if is_error(s) || buf.is_null() {
        return Err("allocate_file_buffer_failed");
    }

    let mut to_read = file_size;
    // SAFETY: `buf` has `file_size` writable bytes and `to_read` never exceeds
    // that; the firmware writes at most `to_read` bytes and shrinks it.
    let s = unsafe { ((*file).read)(file, &mut to_read as *mut usize, buf) };
    // SAFETY: `file` is a live protocol pointer we opened above.
    unsafe { ((*file).close)(file) };
    if is_error(s) {
        return Err("read_failed");
    }
    if to_read != file_size {
        return Err("short_read");
    }
    Ok((buf, to_read))
}

/// Write a two-copy record file (`FLOOR`, `CONFIRM`).
///
/// The second copy is written first, so a power cut in the middle leaves the
/// first copy holding the previous value and the reader still finds a valid
/// record with the highest sequence number. That ordering is the entire reason
/// these files are 4096 bytes instead of 24.
pub fn write_two_copy_file(
    _bs: &BootServices,
    sfs: *mut SimpleFileSystem,
    path: &str,
    data: &[u8; FILE_SIZE],
) -> Result<(), &'static str> {
    if sfs.is_null() {
        return Err("no_volume");
    }

    let mut root: *mut File = null_mut();
    // SAFETY: as in `read_file`.
    let s = unsafe { ((*sfs).open_volume)(sfs, &mut root as *mut *mut File) };
    if is_error(s) || root.is_null() {
        return Err("open_volume_failed");
    }

    let mut wide = [0u16; 128];
    let mut n = 0usize;
    for unit in path.encode_utf16() {
        if n + 1 >= wide.len() {
            return Err("path_too_long");
        }
        wide[n] = unit;
        n += 1;
    }
    wide[n] = 0;

    let mut file: *mut File = null_mut();
    // SAFETY: `root` is live, `wide` is a NUL-terminated path, and
    // READ|WRITE|CREATE is the documented read-write-create open.
    let s = unsafe {
        ((*root).open)(
            root,
            &mut file as *mut *mut File,
            wide.as_ptr(),
            EFI_FILE_MODE_READ | EFI_FILE_MODE_WRITE | EFI_FILE_MODE_CREATE,
            EFI_FILE_ATTRIBUTE_ARCHIVE,
        )
    };
    if is_error(s) || file.is_null() {
        return Err("open_for_write_failed");
    }

    // Copy 1 (the second one) first, then copy 0.
    let half = FILE_SIZE / 2;
    for offset in [half, 0usize] {
        // SAFETY: `file` is live; set_position takes a plain u64 offset.
        let s = unsafe { ((*file).set_position)(file, offset as u64) };
        if is_error(s) {
            // SAFETY: `file` is live and ours to close.
            unsafe { ((*file).close)(file) };
            return Err("set_position_failed");
        }
        let mut to_write = half;
        // SAFETY: `data[offset..]` holds at least `half` bytes and `to_write`
        // says exactly how many the firmware may consume.
        let s = unsafe {
            ((*file).write)(file, &mut to_write as *mut usize, data.as_ptr().add(offset) as *mut u8)
        };
        if is_error(s) || to_write != half {
            // SAFETY: as above.
            unsafe { ((*file).close)(file) };
            return Err("write_failed");
        }
    }

    // SAFETY: `file` is live and ours to close. Close flushes on FAT.
    let s = unsafe { ((*file).close)(file) };
    if is_error(s) {
        return Err("close_failed");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// Read and fully verify `\ARGUS\ARGUS.MF`.
///
/// On success the manifest is authentic and is for *this* slot, *this* loader
/// and a version this loader is allowed to run. Nothing about the kernel has
/// been checked yet — that needs the kernel bytes.
pub fn enforce_manifest(
    bs: &BootServices,
    sfs: *mut SimpleFileSystem,
    ser: &mut Serial,
) -> Result<Manifest, Refusal> {
    let refuse = |code: &'static str| Refusal {
        stage: "manifest",
        code,
    };

    let (buf, len) = match read_file(bs, sfs, MANIFEST_PATH, MAX_SMALL_FILE) {
        Ok(v) => v,
        Err(reason) => {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=manifest result=REFUSED code=E_MANIFEST_ABSENT detail=\"cannot read {}\" reason={}",
                MANIFEST_PATH, reason
            );
            return Err(refuse("E_MANIFEST_ABSENT"));
        }
    };

    // SAFETY: `read_file` returned a pool buffer of exactly `len` initialised
    // bytes that stays valid until ExitBootServices.
    let bytes = unsafe { core::slice::from_raw_parts(buf, len) };

    let manifest = match Manifest::parse(bytes) {
        Ok(m) => m,
        Err(e) => {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=manifest result=REFUSED code={} detail=\"{}\" bytes={}",
                e.code(),
                e.detail(),
                len
            );
            return Err(refuse(e.code()));
        }
    };

    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=manifest event=parsed bytes={} fmt_version={} manifest_version={} slot={} slot_generation={} security_version={} min_loader_version={} issued_counter={}",
        len,
        manifest.fmt_version,
        manifest.manifest_version,
        slot_name(manifest.slot),
        manifest.slot_generation,
        manifest.security_version,
        manifest.min_loader_version,
        manifest.issued_counter
    );
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=manifest event=digests payload_digest={} kernel_digest={} config_digest={} initrd_digest={}",
        Hex(&manifest.payload_digest),
        Hex(&manifest.kernel_digest),
        Hex(&manifest.config_digest),
        Hex(&manifest.initrd_digest)
    );

    {
        let mut detail = Line::new();
        let _ = write!(
            detail,
            "slot {} security_version {} version {}",
            slot_name(manifest.slot),
            manifest.security_version,
            manifest.manifest_version
        );
        screen::stage("manifest parsed", detail.as_str());
    }

    // --- key id, then signature -------------------------------------------
    // The id is checked first so a manifest signed by some other key is
    // reported as such. A manifest that claims *our* key id but carries a
    // signature made with another key falls through to the signature check and
    // is refused as E_SIG_INVALID, which is the interesting case.
    let expected_id = MANIFEST_KEY.key_id();
    // Defence in depth: the id is derived from the modulus, and the committed
    // constant is the same value checked by `tests/crypto-kat.sh`. If the two
    // ever disagree, something replaced the key without replacing the id.
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=manifest event=key expected_id={} constant_id={} constant_matches={}",
        Hex(&expected_id),
        Hex(&test_key::TEST_MANIFEST_KEY_ID),
        (expected_id == test_key::TEST_MANIFEST_KEY_ID) as u8
    );
    if manifest.key_id != expected_id {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=manifest result=REFUSED code=E_KEY_UNKNOWN detail=\"key_id does not match the embedded key\" key_id={} expected={}",
            Hex(&manifest.key_id),
            Hex(&expected_id)
        );
        return Err(refuse("E_KEY_UNKNOWN"));
    }

    match rsa::verify_pkcs1v15_sha256(&MANIFEST_KEY, &manifest.signing_digest(), &manifest.signature)
    {
        Ok(()) => {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=manifest event=signature result=VALID alg=rsa2048-pkcs1v15-sha256 key_id={} sig_len={}",
                Hex(&manifest.key_id),
                manifest.sig_len
            );
            screen::stage("signature valid", "RSA-2048 PKCS#1 v1.5, SHA-256");
        }
        Err(e) => {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=manifest result=REFUSED code=E_SIG_INVALID detail=\"{}\" alg=rsa2048-pkcs1v15-sha256 key_id={}",
                e.reason(),
                Hex(&manifest.key_id)
            );
            return Err(refuse("E_SIG_INVALID"));
        }
    }

    // --- loader version ----------------------------------------------------
    if manifest.min_loader_version > LOADER_VERSION_NUM {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=manifest result=REFUSED code=E_LOADER_TOO_OLD detail=\"manifest needs loader >= {} but this loader is {}\"",
            manifest.min_loader_version, LOADER_VERSION_NUM
        );
        return Err(refuse("E_LOADER_TOO_OLD"));
    }

    // --- slot --------------------------------------------------------------
    if manifest.slot != BOOT_SLOT {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=manifest result=REFUSED code=E_SLOT_MISMATCH detail=\"manifest is for slot {} but this image boots slot {}\"",
            slot_name(manifest.slot),
            slot_name(BOOT_SLOT)
        );
        return Err(refuse("E_SLOT_MISMATCH"));
    }

    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=manifest event=verified result=ACCEPT slot={} security_version={} manifest_version={}",
        slot_name(manifest.slot),
        manifest.security_version,
        manifest.manifest_version
    );
    Ok(manifest)
}

pub fn slot_name(slot: u8) -> &'static str {
    match slot {
        SLOT_A => "A",
        1 => "B",
        _ => "?",
    }
}

// ---------------------------------------------------------------------------
// Floor and confirmation
// ---------------------------------------------------------------------------

pub struct BootState {
    pub floor: FloorRecord,
    /// `true` when the floor file was actually present on the volume.
    pub floor_present: bool,
    pub confirm: Option<ConfirmRecord>,
}

/// Read the floor and the confirmation marker and refuse a rollback.
///
/// The *effective* floor is `max(floor_file, last_booted.security_version)`.
/// The confirmation marker is what raises the floor one boot later: see
/// `commit_boot_state`. Reading both here means a manifest that is below either
/// one never reaches the kernel.
pub fn enforce_rollback(
    bs: &BootServices,
    sfs: *mut SimpleFileSystem,
    manifest: &Manifest,
    ser: &mut Serial,
) -> Result<BootState, Refusal> {
    let refuse = |stage: &'static str, code: &'static str| Refusal { stage, code };

    // --- floor -------------------------------------------------------------
    let floor_record = match read_file(bs, sfs, FLOOR_PATH, MAX_SMALL_FILE) {
        Ok((buf, len)) => {
            // SAFETY: pool buffer of `len` initialised bytes.
            let bytes = unsafe { core::slice::from_raw_parts(buf, len) };
            match floor::parse_floor_file(bytes) {
                CopyState::Valid(r) => {
                    let _ = writeln!(
                        ser,
                        "[ARGUS-LOADER] stage=floor event=read result=OK floor={} seq={} path={}",
                        r.floor, r.seq, FLOOR_PATH
                    );
                    (r, true)
                }
                CopyState::Corrupt => {
                    let _ = writeln!(
                        ser,
                        "[ARGUS-LOADER] stage=floor result=REFUSED code=E_FLOOR_CORRUPT detail=\"neither copy passes its CRC; the floor value is unknown\" path={} bytes={}",
                        FLOOR_PATH, len
                    );
                    return Err(refuse("floor", "E_FLOOR_CORRUPT"));
                }
                CopyState::Absent => {
                    let _ = writeln!(
                        ser,
                        "[ARGUS-LOADER] stage=floor event=absent floor=0 detail=\"no floor record yet (first boot)\" path={}",
                        FLOOR_PATH
                    );
                    (FloorRecord { seq: 0, floor: 0 }, false)
                }
            }
        }
        Err(reason) => {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=floor event=absent floor=0 detail=\"floor file not readable\" reason={}",
                reason
            );
            (FloorRecord { seq: 0, floor: 0 }, false)
        }
    };

    // --- confirmation marker ----------------------------------------------
    let confirm = match read_file(bs, sfs, CONFIRM_PATH, MAX_SMALL_FILE) {
        Ok((buf, len)) => {
            // SAFETY: pool buffer of `len` initialised bytes.
            let bytes = unsafe { core::slice::from_raw_parts(buf, len) };
            match floor::parse_confirm_file(bytes) {
                CopyState::Valid(c) => {
                    let _ = writeln!(
                        ser,
                        "[ARGUS-LOADER] stage=confirm event=found slot={} security_version={} seq={} payload_digest={}",
                        slot_name(c.slot),
                        c.security_version,
                        c.seq,
                        Hex(&c.payload_digest)
                    );
                    Some(c)
                }
                CopyState::Corrupt => {
                    // A damaged marker must not raise anything, and must not
                    // stop the boot: it is not authoritative, the floor is.
                    let _ = writeln!(
                        ser,
                        "[ARGUS-LOADER] stage=confirm event=corrupt detail=\"neither copy passes its CRC; ignoring\""
                    );
                    None
                }
                CopyState::Absent => {
                    let _ = writeln!(ser, "[ARGUS-LOADER] stage=confirm event=absent");
                    None
                }
            }
        }
        Err(_) => {
            let _ = writeln!(ser, "[ARGUS-LOADER] stage=confirm event=absent");
            None
        }
    };

    // --- the check ---------------------------------------------------------
    let pending_floor = confirm.map(|c| c.security_version).unwrap_or(0);
    let effective = core::cmp::max(floor_record.0.floor, pending_floor);
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=floor event=enforced floor={} pending={} effective={} manifest_security_version={}",
        floor_record.0.floor,
        pending_floor,
        effective,
        manifest.security_version
    );

    {
        let mut detail = Line::new();
        let _ = write!(
            detail,
            "floor {} pending {} security_version {}",
            floor_record.0.floor,
            pending_floor,
            manifest.security_version
        );
        screen::stage("floor value", detail.as_str());
    }

    if manifest.security_version < effective {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=manifest result=REFUSED code=E_MANIFEST_ROLLBACK detail=\"security_version {} is below the floor {}\" floor={} pending={}",
            manifest.security_version, effective, floor_record.0.floor, pending_floor
        );
        return Err(refuse("manifest", "E_MANIFEST_ROLLBACK"));
    }

    Ok(BootState {
        floor: floor_record.0,
        floor_present: floor_record.1,
        confirm,
    })
}

/// Persist the confirmation marker for this boot, and raise the floor if the
/// *previous* boot's marker allows it.
///
/// The rule, and its cost, stated plainly:
///
/// * the floor rises **only after a manifest has booted once and the machine
///   came back up to boot again**, never at install time and never before the
///   handoff. A power cut between handoff and the next boot leaves the floor
///   where it was: no false floor, which is the failure mode that matters.
/// * the price is a one-boot window: between the first boot of a new version
///   and the next boot, an attacker with disk access can still present the
///   previous version. `boot-trust-model-v1.md` §4 accepts the same window for
///   the same reason.
///
/// Failure to write is reported and does not stop the boot — the machine still
/// runs the version it just verified, it just will not raise the floor. That is
/// logged as `event=write_failed` so it can never pass silently.
pub fn commit_boot_state(
    bs: &BootServices,
    sfs: *mut SimpleFileSystem,
    state: &BootState,
    manifest: &Manifest,
    ser: &mut Serial,
) {
    let pending = state.confirm.map(|c| c.security_version).unwrap_or(0);

    if pending > state.floor.floor {
        let raised = FloorRecord {
            seq: state.floor.seq.wrapping_add(1),
            floor: pending,
        };
        let bytes = floor::encode_floor_file(&raised);
        match write_two_copy_file(bs, sfs, FLOOR_PATH, &bytes) {
            Ok(()) => {
                let _ = writeln!(
                    ser,
                    "[ARGUS-LOADER] stage=floor event=raised from={} to={} seq={} trigger=confirmed_boot path={}",
                    state.floor.floor, raised.floor, raised.seq, FLOOR_PATH
                );
            }
            Err(reason) => {
                let _ = writeln!(
                    ser,
                    "[ARGUS-LOADER] stage=floor event=write_failed from={} to={} reason={} detail=\"floor not raised; the next boot will retry\"",
                    state.floor.floor, raised.floor, reason
                );
            }
        }
    } else {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=floor event=stable floor={} pending={} detail=\"nothing to raise\"",
            state.floor.floor, pending
        );
    }

    let next_seq = state.confirm.map(|c| c.seq).unwrap_or(0).wrapping_add(1);
    let record = ConfirmRecord {
        seq: next_seq,
        slot: manifest.slot,
        security_version: manifest.security_version,
        payload_digest: manifest.payload_digest,
    };
    let bytes = floor::encode_confirm_file(&record);
    match write_two_copy_file(bs, sfs, CONFIRM_PATH, &bytes) {
        Ok(()) => {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=confirm event=pending seq={} slot={} security_version={} payload_digest={} path={}",
                record.seq,
                slot_name(record.slot),
                record.security_version,
                Hex(&record.payload_digest),
                CONFIRM_PATH
            );
        }
        Err(reason) => {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=confirm event=write_failed seq={} reason={} detail=\"the floor will not rise on the next boot\"",
                record.seq, reason
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Kernel, config and initrd digests
// ---------------------------------------------------------------------------

/// Compare the kernel's own bytes against the digest inside the signed manifest.
///
/// This is the load-bearing check of the whole slice: the manifest is the only
/// authoritative signature (§2), so the kernel is authenticated by being hashed
/// and compared against a field that the signature already covered.
pub fn verify_kernel(manifest: &Manifest, bytes: &[u8], ser: &mut Serial) -> Result<(), Refusal> {
    let refuse = |code: &'static str| Refusal {
        stage: "kernel-digest",
        code,
    };

    if manifest.kernel_len != bytes.len() as u64 {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=kernel-digest result=REFUSED code=E_KERNEL_LEN detail=\"manifest says {} bytes, loaded {}\"",
            manifest.kernel_len,
            bytes.len()
        );
        return Err(refuse("E_KERNEL_LEN"));
    }

    let digest = sha256::digest(bytes);
    if digest != manifest.kernel_digest {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=kernel-digest result=REFUSED code=E_KERNEL_DIGEST detail=\"loaded kernel does not match the signed digest\" computed={} signed={} bytes={}",
            Hex(&digest),
            Hex(&manifest.kernel_digest),
            bytes.len()
        );
        return Err(refuse("E_KERNEL_DIGEST"));
    }

    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=kernel-digest result=MATCH sha256={} bytes={} source=signed_manifest",
        Hex(&digest),
        bytes.len()
    );
    {
        // First 16 hex characters are plenty for a human to compare against the
        // serial log; the full digest stays there.
        let mut hex = Line::new();
        let _ = write!(hex, "{}", Hex(&digest));
        let hex = hex.as_str();
        let short = if hex.len() >= 16 { &hex[..16] } else { hex };
        let mut detail = Line::new();
        let _ = write!(detail, "sha256 {}... {} bytes", short, bytes.len());
        screen::stage("kernel digest", detail.as_str());
    }
    Ok(())
}

/// Check the boot config against `config_digest`.
pub fn verify_config(
    bs: &BootServices,
    sfs: *mut SimpleFileSystem,
    manifest: &Manifest,
    ser: &mut Serial,
) -> Result<(), Refusal> {
    let refuse = |code: &'static str| Refusal {
        stage: "config",
        code,
    };

    let (buf, len) = match read_file(bs, sfs, CONFIG_PATH, MAX_SMALL_FILE) {
        Ok(v) => v,
        Err(reason) => {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=config result=REFUSED code=E_CONFIG_DIGEST detail=\"cannot read the boot config\" path={} reason={}",
                CONFIG_PATH, reason
            );
            return Err(refuse("E_CONFIG_DIGEST"));
        }
    };
    // SAFETY: pool buffer of `len` initialised bytes.
    let bytes = unsafe { core::slice::from_raw_parts(buf, len) };

    if manifest.config_len != len as u64 {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=config result=REFUSED code=E_CONFIG_LEN detail=\"manifest says {} bytes, loaded {}\"",
            manifest.config_len, len
        );
        return Err(refuse("E_CONFIG_LEN"));
    }
    let digest = sha256::digest(bytes);
    if digest != manifest.config_digest {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=config result=REFUSED code=E_CONFIG_DIGEST detail=\"loaded config does not match the signed digest\" computed={} signed={}",
            Hex(&digest),
            Hex(&manifest.config_digest)
        );
        return Err(refuse("E_CONFIG_DIGEST"));
    }
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=config result=MATCH path={} sha256={} bytes={}",
        CONFIG_PATH,
        Hex(&digest),
        len
    );
    Ok(())
}

/// Check the initrd against `initrd_digest`. An all-zero digest means the
/// manifest says there is no initrd, and then the volume must not carry one.
pub fn verify_initrd(manifest: &Manifest, ptr: u64, len: u64, ser: &mut Serial) -> Result<(), Refusal> {
    let refuse = Refusal {
        stage: "initrd",
        code: "E_INITRD_DIGEST",
    };
    let none_in_manifest = manifest.initrd_digest == [0u8; 32];

    if none_in_manifest {
        if len == 0 {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=initrd-digest event=absent detail=\"manifest declares no initrd\""
            );
            return Ok(());
        }
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=initrd-digest result=REFUSED code=E_INITRD_DIGEST detail=\"volume has an initrd the signed manifest does not cover\" len={}",
            len
        );
        return Err(refuse);
    }

    if len == 0 || ptr == 0 {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=initrd-digest result=REFUSED code=E_INITRD_DIGEST detail=\"manifest covers an initrd that was not loaded\""
        );
        return Err(refuse);
    }

    // SAFETY: the loader placed the initrd in pages it allocated and read
    // `len` bytes into them; that range stays mapped until the kernel takes
    // over, which has not happened yet.
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let digest = sha256::digest(bytes);
    if digest != manifest.initrd_digest {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=initrd-digest result=REFUSED code=E_INITRD_DIGEST detail=\"loaded initrd does not match the signed digest\" computed={} signed={}",
            Hex(&digest),
            Hex(&manifest.initrd_digest)
        );
        return Err(refuse);
    }
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=initrd-digest result=MATCH sha256={} bytes={}",
        Hex(&digest),
        len
    );
    Ok(())
}
