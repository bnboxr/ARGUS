//! QEMU `fw_cfg` reader, used for one thing: a display-mode hint from the
//! hypervisor.
//!
//! Why this exists: the boot console prefers 1920x1080 and falls back to the
//! largest mode the firmware offers, which on QEMU's default VGA is always
//! 1920x1080. Proving that the renderer also works on a 640x480 surface
//! therefore needs a way to ask for 640x480 on a machine that could give us
//! more. QEMU's own answer to "pass a small string to the guest firmware" is
//! `fw_cfg`:
//!
//! ```text
//! qemu-system-x86_64 ... -fw_cfg name=opt/argus/screen,string=640x480
//! ```
//!
//! This is not a boot-policy knob and nothing security-relevant depends on it:
//! it can only ever *lower* the resolution we draw at, it is read after the
//! signed manifest has already been verified, and the serial log records
//! whether it was honoured. On real hardware the I/O ports do not exist, every
//! read returns `0xff`, the signature check fails, and this module returns
//! `None` — the loader then uses its normal preference order.
//!
//! Reference: QEMU `docs/specs/fw_cfg.txt` — selector is a big-endian u16
//! written to 0x510, data is read one byte at a time from 0x511. Selector 0x0000
//! is the `"QEMU"` signature and selector 0x0001 is the file directory: a u32
//! count followed by that many 64-byte entries, read sequentially from the same
//! selector.

use core::arch::asm;

/// `FW_CFG_IO_SELECTOR`.
const IO_SELECTOR: u16 = 0x510;
/// `FW_CFG_IO_DATA`.
const IO_DATA: u16 = 0x511;

/// `FW_CFG_SIGNATURE`: the four bytes `"QEMU"` as a big-endian u32.
const SIGNATURE: u32 = 0x5145_4D55;
/// `FW_CFG_FILE_DIR`: the directory blob — a u32 count followed by that many
/// 64-byte entries, all read sequentially from this one selector.
///
/// `0x0019`, not `0x0001`: selector 1 is `FW_CFG_ID`, the interface version,
/// and reading it returns a small integer that parses as "no files at all".
const SEL_FILE_DIR: u16 = 0x0019;
/// Length of one directory name field, including the NUL.
const NAME_LEN: usize = 56;

/// The name this loader looks for.
const SCREEN_FILE: &[u8] = b"opt/argus/screen";

/// Write one word to an I/O port.
///
/// # Safety
///
/// Privileged instruction, ring 0 only, constant port.
unsafe fn outw(port: u16, value: u16) {
    // SAFETY: see above.
    unsafe {
        asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags));
    }
}

/// Read one byte from an I/O port.
///
/// # Safety
///
/// Privileged instruction, ring 0 only, constant port.
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: see above.
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Point `fw_cfg` at one entry.
///
/// # Safety
///
/// Privileged I/O; the port is a constant and the value is a selector the
/// caller already validated against the directory.
unsafe fn select(selector: u16) {
    // No byte swap here. The selector is big-endian *on the bus*, and `outw`
    // already puts the high byte at 0x510 and the low byte at 0x511, which is
    // exactly what the firmware expects to read back. Swapping first selects
    // 0x1900 instead of 0x0019 and every read after the first returns zero —
    // which looks like "this machine has no fw_cfg files", not like a bug.
    // SAFETY: see above.
    unsafe { outw(IO_SELECTOR, selector) };
}

/// Read one byte of the selected entry.
///
/// # Safety
///
/// Privileged I/O on a constant port.
unsafe fn read_u8() -> u8 {
    // SAFETY: see above.
    unsafe { inb(IO_DATA) }
}

/// Read a big-endian u32 from the selected entry.
///
/// # Safety
///
/// Privileged I/O on constant ports.
unsafe fn read_be32() -> u32 {
    let mut v = 0u32;
    for _ in 0..4 {
        // SAFETY: see above.
        v = (v << 8) | unsafe { read_u8() } as u32;
    }
    v
}

/// A requested display mode, as `WxH`.
pub struct ModeHint {
    pub width: u32,
    pub height: u32,
}

/// Ask `fw_cfg` for `opt/argus/screen`.
///
/// Returns `None` on real hardware (no fw_cfg), when QEMU was not given the
/// file, or when the string is not two numbers separated by `x`.
pub fn screen_hint() -> Option<ModeHint> {
    // SAFETY: every call below is a privileged port access at ring 0 on the two
    // constants above; the loader runs before ExitBootServices on the
    // firmware's own page tables, so I/O is available.
    unsafe {
        select(0x0000);
        if read_be32() != SIGNATURE {
            return None;
        }
        select(SEL_FILE_DIR);
        let count = read_be32();
        if count == 0 || count > 256 {
            return None;
        }

        // The directory is one blob: after the count, each entry is
        // `u32 size; u16 select; u16 reserved; char name[56]`, read back to back
        // from the same selector. Re-selecting between entries would restart at
        // the count and read the first entry forever.
        let mut name = [0u8; NAME_LEN];
        for _ in 0..count {
            let size = read_be32() as usize;
            // `select` and `reserved` arrive together; the selector is the high
            // half of the big-endian word.
            let file_selector = (read_be32() >> 16) as u16;
            for byte in name.iter_mut() {
                *byte = read_u8();
            }
            if !name.starts_with(SCREEN_FILE) || size == 0 || size > 32 {
                continue;
            }

            select(file_selector);
            let mut text = [0u8; 32];
            for byte in text.iter_mut().take(size) {
                *byte = read_u8();
            }
            return parse_mode(&text[..size]);
        }
    }
    None
}

/// Parse `WxH` (or `WxH\n`) out of the fw_cfg string.
fn parse_mode(bytes: &[u8]) -> Option<ModeHint> {
    let mut width = 0u32;
    let mut height = 0u32;
    let mut seen_x = false;
    // Digits in the number being read, not in the whole string: "640x480" is
    // three digits on each side, and a shared counter rejects it as too long.
    let mut digits = 0u32;
    for &b in bytes {
        match b {
            b'0'..=b'9' => {
                let d = (b - b'0') as u32;
                digits += 1;
                if digits > 5 {
                    return None;
                }
                if seen_x {
                    height = height.checked_mul(10)?.checked_add(d)?;
                } else {
                    width = width.checked_mul(10)?.checked_add(d)?;
                }
            }
            b'x' | b'X' if !seen_x && digits > 0 => {
                seen_x = true;
                digits = 0;
            }
            b'\n' | b'\r' | b'\0' | b' ' => break,
            _ => return None,
        }
    }
    if !seen_x || width == 0 || height == 0 {
        return None;
    }
    Some(ModeHint { width, height })
}
