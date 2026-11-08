//! Polled 16550-compatible UART driver for COM1 (I/O port 0x3F8).
//!
//! Both the loader and the kernel use this. It talks to the chip through
//! `in`/`out` instructions, so it keeps working across `ExitBootServices` —
//! unlike the UEFI console, which the firmware tears down.
//!
//! Polled, not interrupt-driven: at this stage there is no IDT and no
//! interrupt controller configured, and a boot log that can be written from
//! anywhere (including a panic handler) is worth more than throughput.

use core::arch::asm;
use core::fmt;

/// Base I/O port of COM1.
pub const COM1: u16 = 0x3F8;

// Register offsets from the base port.
const REG_DATA: u16 = 0; // DLAB=1: divisor low byte
const REG_IER: u16 = 1; // DLAB=1: divisor high byte
const REG_FCR: u16 = 2; // write: FIFO control
const REG_LCR: u16 = 3; // line control
const REG_MCR: u16 = 4; // modem control
const REG_LSR: u16 = 5; // line status

const LSR_THR_EMPTY: u8 = 1 << 5;

/// Write one byte to an I/O port.
///
/// # Safety
///
/// This is a privileged instruction. It is sound here because both callers
/// (a UEFI application and a kernel) run at ring 0, and `port` is always one
/// of the constants above, so no attacker-controlled port number is involved.
#[inline]
unsafe fn outb(port: u16, value: u8) {
    // SAFETY: see the function's safety comment — ring 0, constant port.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

/// Read one byte from an I/O port.
///
/// # Safety
///
/// Same reasoning as [`outb`]: ring 0 only, constant port.
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: see the function's safety comment — ring 0, constant port.
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    value
}

/// A serial port. Cheap to copy; it is just a port number.
#[derive(Clone, Copy)]
pub struct Serial {
    base: u16,
}

impl Serial {
    pub const fn new(base: u16) -> Self {
        Serial { base }
    }

    /// Program the UART to 115200 8N1 with a 14-byte FIFO.
    ///
    /// Returns `false` if the port does not look like a real UART (a floating
    /// bus reads back 0xFF from the line-status register). We do not abort on
    /// that — the caller decides whether a missing console is fatal.
    pub fn init(&self) -> bool {
        // SAFETY: all accesses below are to the constant COM1 register block
        // at ring 0; `self.base` is set from `COM1` by the only constructor
        // callers use.
        unsafe {
            outb(self.base + REG_IER, 0x00); // no interrupts yet
            outb(self.base + REG_LCR, 0x80); // set DLAB
            outb(self.base + REG_DATA, 0x01); // divisor = 1 -> 115200 baud
            outb(self.base + REG_IER, 0x00);
            outb(self.base + REG_LCR, 0x03); // 8 bits, no parity, 1 stop
            outb(self.base + REG_FCR, 0xC7); // enable + clear FIFOs, 14-byte trigger
            outb(self.base + REG_MCR, 0x0B); // DTR, RTS, OUT2

            // A present UART reports something other than all-ones here.
            inb(self.base + REG_LSR) != 0xFF
        }
    }

    /// Block until the transmit holding register is empty, then send `byte`.
    pub fn write_byte(&self, byte: u8) {
        // SAFETY: constant COM1 register block, ring 0. The loop is bounded in
        // practice by the UART shifting a byte out (~87 us at 115200); there is
        // no interrupt to wake us, so polling is the only option.
        unsafe {
            let mut spins: u32 = 0;
            while inb(self.base + REG_LSR) & LSR_THR_EMPTY == 0 {
                spins += 1;
                if spins > 10_000_000 {
                    return; // give up rather than hang the boot
                }
            }
            outb(self.base + REG_DATA, byte);
        }
    }

    /// Send a string, translating `\n` into `\r\n` so the log reads correctly
    /// on a raw terminal.
    pub fn write_str(&self, s: &str) {
        for b in s.bytes() {
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
    }

    /// Write a 64-bit value as lowercase hex, zero-padded to 16 digits.
    pub fn write_hex(&self, value: u64) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        self.write_str("0x");
        for shift in (0..16u32).rev() {
            let nibble = ((value >> (shift * 4)) & 0xF) as usize;
            self.write_byte(DIGITS[nibble]);
        }
    }

    /// Write a 64-bit value in decimal.
    pub fn write_dec(&self, mut value: u64) {
        if value == 0 {
            self.write_byte(b'0');
            return;
        }
        let mut buf = [0u8; 20];
        let mut i = buf.len();
        while value > 0 {
            i -= 1;
            buf[i] = b'0' + (value % 10) as u8;
            value /= 10;
        }
        for &b in &buf[i..] {
            self.write_byte(b);
        }
    }
}

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        Serial::write_str(self, s);
        Ok(())
    }
}
