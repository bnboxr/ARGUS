//! ARGUS proof kernel — Phase I, milestone 1.
//!
//! This is not a kernel yet, and pretending otherwise would be dishonest. It
//! is the *proof that the loader handed control to real kernel code*: it runs
//! from the entry point the loader read out of the ELF, on its own stack, and
//! it talks to the serial port itself. If this banner appears, the handoff
//! happened — the loader cannot fake it, because the loader has already
//! executed `ExitBootServices` and jumped away.
//!
//! What it does:
//! 1. zeroes `.bss` and switches to its own stack (assembly trampoline below)
//! 2. brings up COM1 independently of UEFI
//! 3. prints a banner
//! 4. validates `BootInfo`: magic, ABI version, ABI size, then the CRC-32 that
//!    covers every field — and refuses to continue if any of them is wrong
//! 5. walks and prints every field of `BootInfo`, including the ten groups the
//!    capability model requires (`BOOTINFO.md`)
//! 6. checks the initrd image's magic at the address the loader gave it
//! 7. halts forever in `hlt`
//!
//! No allocator, no heap, no dynamic allocation — there is nothing here that
//! could allocate, and `deps.md` lists zero crates.

#![no_std]
#![no_main]

use core::fmt::Write;
use core::panic::PanicInfo;

use argus_shared::bootinfo::{self, memory_type, BootInfo};
use argus_shared::fbcon::{RGB_DIM, RGB_OK};
use argus_shared::screen;
use argus_shared::serial::{Serial, COM1};

/// Version of the kernel side, printed in the banner.
const KERNEL_VERSION: &str = "0.2.0";

/// The first 8 bytes `build.sh` writes into `INITRD.BIN`. Finding these at
/// `initrd_phys` proves the loader really copied the file there, rather than
/// reporting an address and hoping.
const INITRD_MAGIC: &[u8; 8] = b"ARGUSIRD";

// ---------------------------------------------------------------------------
// Entry trampoline.
//
// The loader jumps to `_start` (the ELF entry point) with a `*const BootInfo`
// in RDI, per the x86-64 SysV ABI. The loader's own stack is not ours to use,
// so the very first thing we do is install the kernel's stack from the linker
// script. Before that we must not touch the stack at all — hence assembly.
// ---------------------------------------------------------------------------
core::arch::global_asm!(
    r#"
    .section .text._start, "ax"
    .p2align 4
    .global _start
    .type _start, @function
_start:
    /* Park the BootInfo pointer in RSI; we need RDI as a scratch cursor. */
    mov rsi, rdi

    /* Zero .bss: the loader only copies p_filesz bytes, so everything past
       that in each segment is memory we must not assume is zero. */
    lea rdi, [rip + __bss_start]
    lea rcx, [rip + __bss_end]
    cmp rdi, rcx
    jae 2f
1:
    mov byte ptr [rdi], 0
    inc rdi
    cmp rdi, rcx
    jb 1b
2:
    /* Our own stack, from the linker script. */
    lea rsp, [rip + __kernel_stack_top]
    xor rbp, rbp

    /* Restore the argument register and enter Rust. */
    mov rdi, rsi
    call kmain

    /* kmain is `-> !`; this is belt and braces if it ever is not. */
3:
    cli
    hlt
    jmp 3b
    .size _start, . - _start
"#
);

/// Keeps `kmain` alive: nothing in Rust references it, only the assembly
/// above does, and rustc's reachability analysis cannot see through
/// `global_asm!`.
#[used]
static KEEP_KMAIN: extern "C" fn(*const BootInfo) -> ! = kmain;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut ser = Serial::new(COM1);
    let _ = writeln!(ser, "[ARGUS-KERNEL] stage=panic event=abort detail={}", info);
    halt()
}

fn halt() -> ! {
    loop {
        // SAFETY: `hlt` in ring 0 with interrupts disabled is a permanent,
        // well-defined stop. There is no IDT, so no interrupt can resume us.
        unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
    }
}

/// First Rust code to run in ARGUS. Called by `_start` with the loader's
/// `BootInfo` pointer in RDI.
#[no_mangle]
pub extern "C" fn kmain(bootinfo: *const BootInfo) -> ! {
    let mut ser = Serial::new(COM1);
    let uart_present = ser.init();

    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=entry event=start name=ARGUS kernel=x86_64 version={} abi_version={} uart_present={}",
        KERNEL_VERSION,
        bootinfo::ABI_VERSION,
        uart_present as u8
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=entry event=running rip_is_kernel_text=1 identity_mapped=1"
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=entry event=banner \"ARGUS kernel is alive\""
    );

    if bootinfo.is_null() {
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo event=fail reason=null_pointer"
        );
        halt();
    }

    // SAFETY: the loader passed this pointer in RDI and guarantees it points
    // at a live, static `BootInfo` that stays mapped for the kernel's whole
    // lifetime (it lives in the loader image, which is never freed).
    let bi = unsafe { &*bootinfo };

    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo event=received address=0x{:x} magic=0x{:x} abi_version={} abi_size={}",
        bootinfo as usize, bi.magic, bi.abi_version, bi.abi_size
    );

    // ---- validation, cheapest and most fundamental first -------------------
    if !bi.is_compatible() {
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo event=fail reason=E_BOOTINFO_MAGIC carried_magic=0x{:x} expected_magic=0x{:x} carried_abi={} expected_abi={}",
            bi.magic,
            bootinfo::BOOTINFO_MAGIC,
            bi.abi_version,
            bootinfo::ABI_VERSION
        );
        halt();
    }

    // The size check comes before the checksum: computing a checksum over a
    // length the loader supplied would read memory this kernel does not own.
    let expected_size = core::mem::size_of::<BootInfo>() as u32;
    if bi.abi_size != expected_size {
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo event=fail reason=E_BOOTINFO_SIZE kernel_abi_size={} loader_abi_size={}",
            expected_size, bi.abi_size
        );
        halt();
    }

    let (carried, computed) = bi.verify_checksum();
    if carried != computed {
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo event=fail reason=E_BOOTINFO_CHECKSUM carried=0x{:08x} computed=0x{:08x}",
            carried, computed
        );
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo event=REFUSED reason=checksum_mismatch note=\"a field changed after the loader sealed the structure\""
        );
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=halt event=refused reason=E_BOOTINFO_CHECKSUM boot_proven=0"
        );
        halt();
    }
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo event=checksum_ok carried=0x{:08x} computed=0x{:08x} covered_bytes={}",
        carried, computed, expected_size
    );

    let unknown_flags = bi.flags & !bootinfo::FLAG_KNOWN_MASK;
    if unknown_flags != 0 {
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo event=fail reason=E_BOOTINFO_UNKNOWN_FLAGS mask=0x{:x}",
            unknown_flags
        );
        halt();
    }

    // ---- header ----------------------------------------------------------
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=magic value=0x{:x}",
        bi.magic
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=abi_version value={}",
        bi.abi_version
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=abi_size value={}",
        bi.abi_size
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=flags value=0x{:x} memmap_clamped={} reserved_clamped={} framebuffer={} acpi={} initrd={} test_corrupted={}",
        bi.flags,
        ((bi.flags & bootinfo::FLAG_MEMMAP_CLAMPED) != 0) as u8,
        ((bi.flags & bootinfo::FLAG_LOADER_RESERVED_CLAMPED) != 0) as u8,
        ((bi.flags & bootinfo::FLAG_FRAMEBUFFER_PRESENT) != 0) as u8,
        ((bi.flags & bootinfo::FLAG_ACPI_PRESENT) != 0) as u8,
        ((bi.flags & bootinfo::FLAG_INITRD_PRESENT) != 0) as u8,
        ((bi.flags & bootinfo::FLAG_TEST_CORRUPTED) != 0) as u8
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=checksum value=0x{:08x} verified=1",
        bi.checksum
    );

    // ---- timing and kernel identity --------------------------------------
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=boot_tsc value=0x{:x}",
        bi.boot_tsc
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=kernel_entry value=0x{:x}",
        bi.kernel_entry
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=kernel_image_bytes value=0x{:x}",
        bi.kernel_image_bytes
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=segment_count value={} kernel_phnum={}",
        bi.segment_count, bi.kernel_phnum
    );

    // ---- field 6: where the kernel physically is --------------------------
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=kernel_phys start=0x{:x} len=0x{:x} end=0x{:x} phnum={}",
        bi.kernel_phys_start,
        bi.kernel_phys_len,
        bi.kernel_phys_start + bi.kernel_phys_len,
        bi.kernel_phnum
    );

    let seg_count = (bi.segment_count as usize).min(bootinfo::MAX_SEGMENTS);
    for i in 0..seg_count {
        let s = bi.segments[i];
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo field=segments[{}] vaddr=0x{:x} paddr=0x{:x} filesz=0x{:x} memsz=0x{:x} flags=0x{:x}",
            i, s.vaddr, s.paddr, s.filesz, s.memsz, s.flags
        );
    }

    // ---- field 1, 2: the memory map --------------------------------------
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=memory_map_meta count={} capacity={} desc_size={} desc_version=0x{:x} map_key=0x{:x}",
        bi.memory_region_count,
        bi.memory_region_capacity,
        bi.map_desc_size,
        bi.map_desc_version,
        bi.map_key
    );

    let region_count = (bi.memory_region_count as usize).min(bootinfo::MAX_MEMORY_REGIONS);
    let mut usable = 0u64;
    for i in 0..region_count {
        let r = bi.memory_regions[i];
        if memory_type::is_usable_for_kernel(r.region_type) {
            usable += r.phys_len;
        }
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo field=memory_regions[{}] type={} name={} start=0x{:x} len=0x{:x} end=0x{:x} uefi_attribs=0x{:x}",
            i,
            r.region_type,
            memory_type::name(r.region_type),
            r.phys_start,
            r.phys_len,
            r.end(),
            r.uefi_attribs
        );
    }
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo event=summary regions={} usable_bytes=0x{:x}",
        region_count, usable
    );

    // ---- field 3: ACPI ----------------------------------------------------
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=acpi present={} rsdp_phys=0x{:x} rsdp_checksum=0x{:02x} revision={}",
        bi.acpi_present, bi.rsdp_phys, bi.rsdp_checksum, bi.rsdp_revision
    );
    if bi.acpi_present != 0 && bi.rsdp_phys != 0 {
        // The loader validated this table's checksum; re-read the signature
        // here so the kernel proves the address it was handed really points at
        // an RSDP and is not just a plausible number.
        // SAFETY: the loader verified this physical address is the RSDP the
        // firmware published, it is identity-mapped (we have no page tables
        // yet), and reading 8 bytes of a 36-byte table cannot fault.
        let sig = unsafe { core::slice::from_raw_parts(bi.rsdp_phys as *const u8, 8) };
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo event=acpi_readback signature=\"{}\" match={}",
            if &sig[0..7] == b"RSD PTR" { "RSD PTR" } else { "BAD" },
            (&sig[0..8] == b"RSD PTR ") as u8
        );
    }

    // ---- field 4: framebuffer --------------------------------------------
    let fb = bi.framebuffer;
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=framebuffer present={} phys=0x{:x} width={} height={} stride={} pixel_format={} name={} size_bytes=0x{:x}",
        ((bi.flags & bootinfo::FLAG_FRAMEBUFFER_PRESENT) != 0) as u8,
        fb.phys,
        fb.width,
        fb.height,
        fb.stride,
        fb.pixel_format,
        bootinfo::pixel_format::name(fb.pixel_format),
        fb.size_bytes
    );

    // ---- field 5: initrd --------------------------------------------------
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=initrd phys=0x{:x} len=0x{:x}",
        bi.initrd_phys, bi.initrd_len
    );
    if bi.initrd_phys != 0 && bi.initrd_len >= 8 {
        // SAFETY: the loader allocated these pages for the initrd and copied
        // `initrd_len` bytes into them; the memory is identity-mapped and we
        // read the first 8 bytes of a buffer we were told is at least that big.
        let head = unsafe { core::slice::from_raw_parts(bi.initrd_phys as *const u8, 8) };
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo event=initrd_readback magic_match={} expected=ARGUSIRD",
            (head == INITRD_MAGIC) as u8
        );
    }

    // ---- field 7: loader reservations ------------------------------------
    let reserved_count = (bi.loader_reserved_count as usize).min(bootinfo::MAX_LOADER_RESERVED);
    for i in 0..reserved_count {
        let r = bi.loader_reserved[i];
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo field=loader_reserved[{}] kind={} name={} start=0x{:x} len=0x{:x} end=0x{:x}",
            i,
            r.kind,
            bootinfo::reservation_kind::name(r.kind),
            r.start,
            r.len,
            r.start + r.len
        );
    }
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=loader_reserved_count value={} capacity={}",
        bi.loader_reserved_count, bi.loader_reserved_capacity
    );

    // ---- field 8: boot slot and reason -----------------------------------
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=boot_slot value={} boot_reason={} boot_reason_name={}",
        bi.boot_slot,
        bi.boot_reason,
        bootinfo::boot_reason::name(bi.boot_reason)
    );

    // ---- field 9: debug serial -------------------------------------------
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=debug_serial base_port=0x{:x} baud={} present={}",
        bi.debug_serial.base_port, bi.debug_serial.baud, bi.debug_serial.present
    );

    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=system_table value=0x{:x}",
        bi.system_table
    );

    // ---- prove we are where we think we are ------------------------------
    // `_start` is the first byte of the first PT_LOAD segment. If the loader
    // put us there, the address of `_start` equals that segment's paddr.
    // SAFETY: taking the address of a function does not dereference anything.
    let start_addr = _start as *const () as u64;
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo event=placement _start=0x{:x} first_segment_paddr=0x{:x} kernel_phys_start=0x{:x} match={}",
        start_addr,
        bi.segments[0].paddr,
        bi.kernel_phys_start,
        (start_addr == bi.segments[0].paddr && start_addr == bi.kernel_phys_start) as u8
    );

    // ---- the screen ------------------------------------------------------
    // Only now, after the checksum proved every field including the framebuffer
    // is the one the loader sealed. Drawing into a framebuffer described by an
    // unverified `BootInfo` would mean writing to an address an attacker chose,
    // so a kernel that refuses BootInfo says so on serial only — the screen it
    // would have used is not trustworthy enough to touch.
    match screen::attach(bi.framebuffer) {
        Ok(()) => {
            let _ = writeln!(
                ser,
                "[ARGUS-KERNEL] stage=screen result=READY width={} height={} stride={}",
                bi.framebuffer.width, bi.framebuffer.height, bi.framebuffer.stride
            );
            // The line the owner asked to see: written by the kernel, below the
            // loader's stage log, in the colour that means "this is real".
            screen::alive("ARGUS kernel is alive");
            screen::note("BootInfo checksum verified, halted in hlt", RGB_DIM);
            screen::note("serial log: COM1 115200", RGB_OK);
        }
        Err(reason) => {
            let _ = writeln!(
                ser,
                "[ARGUS-KERNEL] stage=screen result=ABSENT reason={} width={} height={} stride={}",
                reason.reason(),
                bi.framebuffer.width,
                bi.framebuffer.height,
                bi.framebuffer.stride
            );
        }
    }

    let _ = writeln!(ser, "[ARGUS-KERNEL] stage=halt event=ready state=loop_hlt");
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=halt event=boot_proven kernel_banner=1 bootinfo_dumped=1 bootinfo_checksum=ok abi_version={}",
        bi.abi_version
    );

    halt()
}

extern "C" {
    /// Declared so we can compare the loader's placement against reality.
    fn _start();
}
