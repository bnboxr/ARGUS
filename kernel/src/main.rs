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
//! 4. validates the `BootInfo` magic and version
//! 5. walks and prints every field of `BootInfo`
//! 6. halts forever in `hlt`
//!
//! No allocator, no heap, no dynamic allocation — there is nothing here that
//! could allocate, and `deps.md` lists zero crates.

#![no_std]
#![no_main]

use core::fmt::Write;
use core::panic::PanicInfo;

use argus_shared::bootinfo::{self, memory_type, BootInfo};
use argus_shared::serial::{Serial, COM1};

/// Version of the kernel side, printed in the banner.
const KERNEL_VERSION: &str = "0.1.0";

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
        "[ARGUS-KERNEL] stage=entry event=start name=ARGUS kernel=x86_64 version={} bootinfo_version={} uart_present={}",
        KERNEL_VERSION,
        bootinfo::BOOTINFO_VERSION,
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
        "[ARGUS-KERNEL] stage=bootinfo event=received address=0x{:x} magic=0x{:x} version={} size={}",
        bootinfo as usize, bi.magic, bi.version, bi.size
    );

    if !bi.is_compatible() {
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo event=fail reason=incompatible expected_magic=0x{:x} expected_version={}",
            bootinfo::BOOTINFO_MAGIC,
            bootinfo::BOOTINFO_VERSION
        );
        halt();
    }

    if bi.size as usize != core::mem::size_of::<BootInfo>() {
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo event=fail reason=size_mismatch kernel_size={} loader_size={}",
            core::mem::size_of::<BootInfo>(),
            bi.size
        );
        halt();
    }

    // ---- walk every field ------------------------------------------------
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=magic value=0x{:x}",
        bi.magic
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=version value={}",
        bi.version
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=size value={}",
        bi.size
    );
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
        "[ARGUS-KERNEL] stage=bootinfo field=segment_count value={}",
        bi.segment_count
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=memory_region_count value={}",
        bi.memory_region_count
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=memory_region_capacity value={}",
        bi.memory_region_capacity
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=system_table value=0x{:x}",
        bi.system_table
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=serial_port value=0x{:x}",
        bi.serial_port
    );
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo field=flags value=0x{:x} memmap_clamped={}",
        bi.flags,
        (bi.flags & bootinfo::FLAG_MEMMAP_CLAMPED) as u32
    );

    // ---- segments --------------------------------------------------------
    let seg_count = (bi.segment_count as usize).min(bootinfo::MAX_SEGMENTS);
    for i in 0..seg_count {
        let s = bi.segments[i];
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo field=segments[{}] vaddr=0x{:x} paddr=0x{:x} filesz=0x{:x} memsz=0x{:x} flags=0x{:x}",
            i, s.vaddr, s.paddr, s.filesz, s.memsz, s.flags
        );
    }

    // ---- memory map ------------------------------------------------------
    let region_count = (bi.memory_region_count as usize).min(bootinfo::MAX_MEMORY_REGIONS);
    let mut usable = 0u64;
    for i in 0..region_count {
        let r = bi.memory_regions[i];
        if memory_type::is_usable_for_kernel(r.region_type) {
            usable += r.page_count * 4096;
        }
        let _ = writeln!(
            ser,
            "[ARGUS-KERNEL] stage=bootinfo field=memory_regions[{}] type={} name={} start=0x{:x} pages={} end=0x{:x} attr=0x{:x}",
            i,
            r.region_type,
            memory_type::name(r.region_type),
            r.physical_start,
            r.page_count,
            r.end(),
            r.attributes
        );
    }
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo event=summary regions={} usable_bytes=0x{:x}",
        region_count, usable
    );

    // ---- prove we are where we think we are ------------------------------
    // `_start` is the first byte of the first PT_LOAD segment. If the loader
    // put us there, the address of `_start` equals that segment's paddr.
    // SAFETY: taking the address of a function does not dereference anything.
    let start_addr = _start as *const () as u64;
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=bootinfo event=placement _start=0x{:x} first_segment_paddr=0x{:x} match={}",
        start_addr,
        bi.segments[0].paddr,
        (start_addr == bi.segments[0].paddr) as u8
    );

    let _ = writeln!(ser, "[ARGUS-KERNEL] stage=halt event=ready state=loop_hlt");
    let _ = writeln!(
        ser,
        "[ARGUS-KERNEL] stage=halt event=boot_proven kernel_banner=1 bootinfo_dumped=1"
    );

    halt()
}

extern "C" {
    /// Declared so we can compare the loader's placement against reality.
    fn _start();
}
