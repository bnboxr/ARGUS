//! Code shared between the ARGUS UEFI loader and the ARGUS kernel.
//!
//! Two things only:
//! * [`bootinfo`] — the versioned `BootInfo` structure handed from loader to kernel.
//! * [`serial`]  — a polled 16550 UART driver for COM1, so both sides can talk
//!   to the same wire without depending on the UEFI console (which disappears
//!   at `ExitBootServices`).

#![no_std]

pub mod bootinfo;
pub mod serial;
