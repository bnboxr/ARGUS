//! Code shared between the ARGUS UEFI loader and the ARGUS kernel.
//!
//! Three things only:
//! * [`bootinfo`] — the versioned `BootInfo` structure handed from loader to kernel.
//! * [`serial`]  — a polled 16550 UART driver for COM1, so both sides can talk
//!   to the same wire without depending on the UEFI console (which disappears
//!   at `ExitBootServices`).
//! * [`fbcon`]   — the on-screen console: an 8x16 bitmap font drawn straight
//!   into the GOP framebuffer, so a boot is visible on a machine with no serial
//!   port. [`font8x16`] is the vendored glyph table it draws from.

#![no_std]

pub mod bootinfo;
pub mod fbcon;
pub mod font8x16;
pub mod screen;
pub mod serial;
