//! ARGUS UEFI loader — Phase I, milestone 1.
//!
//! What this program does, in order, and prints each step to COM1 as it goes:
//!
//! 1. bring up the serial port and print a version banner
//! 2. ask the firmware for the memory map and print all of it
//! 3. find the ACPI RSDP and the GOP framebuffer while boot services still exist
//! 4. read `\ARGUSKRN.ELF` from the volume this image was loaded from
//! 5. validate it (see `elf.rs`) — reject with a named code, or accept
//! 6. read `\INITRD.BIN`, allocate pages for it, and copy it there
//! 7. copy each `PT_LOAD` segment to its physical address, zero-filling the
//!    tail of each segment
//! 8. fill in `BootInfo`, then seal it (size + checksum)
//! 9. `ExitBootServices`
//! 10. jump to `e_entry` with a `*const BootInfo` in `RDI` (x86-64 SysV ABI)
//!
//! It never returns. On any failure it prints the reason and halts, so a
//! failed boot leaves a complete, readable trace on the wire.
//!
//! Deliberately out of scope for this milestone: A/B slots, Secure Boot,
//! signature checking, page-table setup (the kernel runs on the identity map
//! UEFI already established), and any graphical output.

#![no_std]
#![no_main]

mod efi;
mod elf;
mod fat;

use core::ffi::c_void;
use core::fmt::Write;
use core::panic::PanicInfo;
use core::ptr::{addr_of, addr_of_mut, null_mut};

use argus_shared::bootinfo::{
    self, BootInfo, DebugSerial, FramebufferInfo, LoaderReserved, MemoryRegion,
};
use argus_shared::serial::{Serial, COM1};

use efi::*;

/// Version of this loader, printed in the banner and easy to grep.
const LOADER_VERSION: &str = "0.2.0";

/// Where the kernel ELF lives on the boot volume. Matches `build.sh`.
const KERNEL_PATH: &str = "\\ARGUSKRN.ELF";

/// The same file as an 8.3 FAT directory entry: 11 bytes, space padded, used
/// by the block-level FAT reader when the firmware offers no filesystem.
const KERNEL_NAME_83: [u8; 11] = *b"ARGUSKRNELF";

/// The initrd image, if `build.sh` put one on the volume.
const INITRD_PATH: &str = "\\INITRD.BIN";

/// `BootInfo` lives in the loader's own image (static storage), so it stays
/// mapped and valid after `ExitBootServices`. Putting it in `AllocatePool`
/// memory would hand the kernel a pointer into pages the firmware just freed.
static mut BOOT_INFO: BootInfo = BootInfo::ZERO;

/// A private stack used only for the last few instructions before the jump,
/// so the loader does not depend on the firmware's stack surviving
/// `ExitBootServices`.
static mut JUMP_STACK: [u8; 8192] = [0; 8192];

/// Scratch buffer for the raw UEFI memory map. 64 KiB is far more than OVMF
/// or any laptop firmware produces (typically 4-16 KiB), and it is allocated
/// before boot services go away.
const MEMMAP_BUFFER_SIZE: usize = 64 * 1024;

/// Hard ceiling on the initrd, so a corrupt file header cannot make us ask the
/// firmware for a gigabyte of pages.
const MAX_INITRD_BYTES: usize = 16 * 1024 * 1024;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut ser = Serial::new(COM1);
    let _ = writeln!(ser, "[ARGUS-LOADER] stage=panic event=abort detail={}", info);
    halt()
}

/// Stop the CPU forever. Used for every failure path.
fn halt() -> ! {
    loop {
        // SAFETY: `hlt` is unprivileged-safe in ring 0 and is the only way to
        // idle this core without burning it. There is no interrupt handler
        // installed, so this is a genuine, permanent stop — which is exactly
        // what a failed boot should be: visible, not a reboot loop.
        unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
    }
}

/// The regions the loader occupies, collected as we go and published to the
/// kernel in `BootInfo::loader_reserved`.
///
/// Without this the kernel has no way to tell "memory the firmware freed" from
/// "memory the loader is still living in", and would hand its own BootInfo page
/// to the first allocator that asked.
struct Reservations {
    items: [LoaderReserved; bootinfo::MAX_LOADER_RESERVED],
    count: usize,
    clamped: bool,
}

impl Reservations {
    const fn new() -> Self {
        Reservations {
            items: [LoaderReserved::ZERO; bootinfo::MAX_LOADER_RESERVED],
            count: 0,
            clamped: false,
        }
    }

    fn add(&mut self, start: usize, len: usize, kind: u32, ser: &mut Serial) {
        if len == 0 {
            return;
        }
        if self.count >= bootinfo::MAX_LOADER_RESERVED {
            self.clamped = true;
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=reserve event=clamped kind={} start=0x{:x} len=0x{:x}",
                bootinfo::reservation_kind::name(kind),
                start,
                len
            );
            return;
        }
        self.items[self.count] = LoaderReserved {
            start: start as u64,
            len: len as u64,
            kind,
            _reserved: 0,
        };
        self.count += 1;
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=reserve index={} kind={} start=0x{:x} len=0x{:x} end=0x{:x}",
            self.count - 1,
            bootinfo::reservation_kind::name(kind),
            start,
            len,
            start as u64 + len as u64
        );
    }

    /// Does `[start, start+len)` intersect anything the loader already owns?
    fn overlaps(&self, start: u64, len: u64) -> Option<u32> {
        let end = start.saturating_add(len);
        self.items[..self.count]
            .iter()
            .find(|r| start < r.start + r.len && r.start < end)
            .map(|r| r.kind)
    }
}

/// Entry point. The UEFI target links this as the PE32+ entry (`efi_main`).
#[no_mangle]
pub extern "efiapi" fn efi_main(image_handle: Handle, system_table: *mut SystemTable) -> Status {
    let mut ser = Serial::new(COM1);
    let serial_present = ser.init();
    let mut reserved = Reservations::new();

    // ---- stage: serial -------------------------------------------------
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=serial event=init port=0x{:x} baud=115200 uart_present={}",
        COM1,
        serial_present as u8
    );

    // ---- stage: banner -------------------------------------------------
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=banner event=start name=ARGUS loader=uefi-x86_64 version={} abi_version={}",
        LOADER_VERSION,
        bootinfo::ABI_VERSION
    );

    if system_table.is_null() {
        let _ = writeln!(ser, "[ARGUS-LOADER] stage=banner event=fail reason=null_system_table");
        halt();
    }

    // SAFETY: `system_table` is the pointer the firmware passed to `efi_main`;
    // it is non-null (checked above) and points at a valid EFI_SYSTEM_TABLE for
    // the whole lifetime of this application.
    let st = unsafe { &*system_table };
    if st.boot_services.is_null() {
        let _ = writeln!(ser, "[ARGUS-LOADER] stage=banner event=fail reason=null_boot_services");
        halt();
    }
    // SAFETY: same reasoning — `boot_services` is a firmware-owned table that
    // is valid until `ExitBootServices`, which we have not called yet.
    let bs = unsafe { &*st.boot_services };

    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=banner event=system_table revision=0x{:x} config_entries={} boot_services=0x{:x}",
        st.hdr.revision,
        st.number_of_table_entries,
        st.boot_services as usize
    );

    // ---- firmware hygiene ----------------------------------------------
    // A pending watchdog timer would reboot us mid-handoff. Disable it.
    // SAFETY: `set_watchdog_timer(0,0,0,NULL)` is the documented "disable"
    // call; null is explicitly permitted for the optional description string.
    let wd = unsafe { (bs.set_watchdog_timer)(0, 0, 0, core::ptr::null()) };
    let _ = writeln!(ser, "[ARGUS-LOADER] stage=banner event=watchdog_disabled status=0x{:x}", wd);

    // ---- stage: memory map ---------------------------------------------
    let mut map_buf: *mut u8 = null_mut();
    // SAFETY: AllocatePool writes a fresh pointer into `map_buf`; EfiLoaderData
    // memory stays mapped for the loader's lifetime.
    let alloc = unsafe {
        (bs.allocate_pool)(
            efi::EFI_LOADER_DATA,
            MEMMAP_BUFFER_SIZE,
            &mut map_buf as *mut *mut u8,
        )
    };
    if is_error(alloc) || map_buf.is_null() {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=memmap event=fail reason=allocate_pool status=0x{:x}",
            alloc
        );
        halt();
    }
    reserved.add(
        map_buf as usize,
        MEMMAP_BUFFER_SIZE,
        bootinfo::reservation_kind::MEMMAP_BUFFER,
        &mut ser,
    );

    let mut map_size: usize = MEMMAP_BUFFER_SIZE;
    let mut map_key: usize = 0;
    let mut desc_size: usize = 0;
    let mut desc_version: u32 = 0;
    // SAFETY: `map_buf` points at `MEMMAP_BUFFER_SIZE` writable bytes, and the
    // four out-parameters are valid locals. The firmware writes at most
    // `map_size` bytes into the buffer and reports the real length back.
    let mm_status = unsafe {
        (bs.get_memory_map)(
            &mut map_size,
            map_buf as *mut MemoryDescriptor,
            &mut map_key,
            &mut desc_size,
            &mut desc_version,
        )
    };
    if is_error(mm_status) {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=memmap event=fail reason=get_memory_map status=0x{:x}",
            mm_status
        );
        halt();
    }

    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=memmap event=received entries={} bytes={} descriptor_size={} descriptor_version=0x{:x} map_key=0x{:x}",
        map_size / desc_size,
        map_size,
        desc_size,
        desc_version,
        map_key
    );

    // Flatten into BootInfo's fixed-size form and print every entry.
    // SAFETY: `map_buf` holds `map_size` bytes of descriptors written by the
    // firmware above, each `desc_size` bytes apart, and we only read the first
    // 40 bytes of each (EFI_MEMORY_DESCRIPTOR is 40 bytes by specification and
    // `desc_size` is never smaller than that).
    let mut region_count = 0usize;
    let mut clamped = false;
    let mut usable_bytes = 0u64;
    let entry_count = map_size / desc_size;
    for i in 0..entry_count {
        // Stride by the firmware's own `desc_size`, not by
        // `size_of::<MemoryDescriptor>()`: UEFI explicitly allows the
        // descriptor to grow, and OVMF's is larger than the 40-byte minimum.
        // SAFETY: `i < entry_count == map_size / desc_size`, so
        // `map_buf + i*desc_size` is the start of a descriptor the firmware
        // just wrote, and at least `desc_size >= 40` bytes remain there.
        let d = unsafe { &*(map_buf.add(i * desc_size) as *const MemoryDescriptor) };
        let bytes = d.number_of_pages * 4096;
        let end = d.physical_start + bytes;
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=memmap entry={} type={} name={} start=0x{:x} pages={} end=0x{:x} attr=0x{:x}",
            i,
            d.region_type,
            bootinfo::memory_type::name(d.region_type),
            d.physical_start,
            d.number_of_pages,
            end,
            d.attribute
        );
        if bootinfo::memory_type::is_usable_for_kernel(d.region_type) {
            usable_bytes += bytes;
        }

        // SAFETY: `BOOT_INFO` is a static we own; no other reference to it
        // exists at this point, so taking a mutable reference here is sound.
        let bi = unsafe { &mut *addr_of_mut!(BOOT_INFO) };
        if region_count < bootinfo::MAX_MEMORY_REGIONS {
            bi.memory_regions[region_count] = MemoryRegion {
                region_type: d.region_type,
                _reserved: 0,
                phys_start: d.physical_start,
                phys_len: bytes,
                uefi_attribs: d.attribute,
            };
            region_count += 1;
        } else {
            clamped = true;
        }
    }

    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=memmap event=summary entries={} copied={} usable_bytes=0x{:x} clamped={}",
        entry_count,
        region_count,
        usable_bytes,
        clamped as u8
    );

    // ---- stage: ACPI -----------------------------------------------------
    // The RSDP must be found now: it is a configuration table, and the whole
    // configuration table array is boot-services memory that disappears at
    // handoff. The kernel cannot re-discover it, which is why `rsdp_phys` is
    // in the ABI rather than left to the kernel to look up.
    let acpi = find_acpi(&st, &mut ser);

    // ---- stage: framebuffer ---------------------------------------------
    let fb = find_framebuffer(bs, &mut ser);

    // ---- read the kernel file -------------------------------------------
    let loaded_image = match get_loaded_image(bs, image_handle) {
        Some(li) => li,
        None => {
            let _ = writeln!(ser, "[ARGUS-LOADER] stage=kernel-read event=fail reason=no_loaded_image_protocol");
            halt();
        }
    };
    // SAFETY: the firmware just returned a valid EFI_LOADED_IMAGE_PROTOCOL for
    // our own image handle.
    let li = unsafe { &*loaded_image };
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=kernel-read event=loaded_image base=0x{:x} size=0x{:x} device_handle=0x{:x}",
        li.image_base as usize, li.image_size, li.device_handle as usize
    );
    reserved.add(
        li.image_base as usize,
        li.image_size as usize,
        bootinfo::reservation_kind::LOADER_IMAGE,
        &mut ser,
    );
    reserved.add(
        addr_of!(BOOT_INFO) as usize,
        core::mem::size_of::<BootInfo>(),
        bootinfo::reservation_kind::BOOTINFO,
        &mut ser,
    );
    reserved.add(
        addr_of!(JUMP_STACK) as usize,
        core::mem::size_of::<[u8; 8192]>(),
        bootinfo::reservation_kind::JUMP_STACK,
        &mut ser,
    );

    let (volume, kernel_buf, kernel_len) = match open_kernel_file(bs, li, &mut ser) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=kernel-read event=fail path={} reason={}",
                KERNEL_PATH, e
            );
            halt();
        }
    };
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=kernel-read event=ok path={} bytes={} buffer=0x{:x}",
        KERNEL_PATH, kernel_len, kernel_buf as usize
    );
    reserved.add(
        kernel_buf as usize,
        kernel_len,
        bootinfo::reservation_kind::KERNEL_FILE_BUFFER,
        &mut ser,
    );

    // ---- stage: ELF validation ------------------------------------------
    // SAFETY: `kernel_buf` holds `kernel_len` initialised bytes read from the
    // file above; building a slice over exactly that range is sound.
    let file = unsafe { core::slice::from_raw_parts(kernel_buf, kernel_len) };

    // The kernel must not land on top of the running loader.
    let loader_range = (li.image_base as u64, li.image_size);

    // SAFETY: `BOOT_INFO` is a static we own; this borrow ends before the
    // mutable borrow taken when filling it below.
    let regions = unsafe { &(*addr_of!(BOOT_INFO)).memory_regions };
    let image = match elf::validate(file, regions, loader_range) {
        Ok(img) => {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=elf-validate result=ACCEPT entry=0x{:x} segments={} phnum={} image_bytes={}",
                img.entry, img.segment_count, img.phnum, img.image_bytes
            );
            img
        }
        Err(e) => {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=elf-validate result=REJECT code={} detail=\"{}\"",
                e.code(),
                e.detail()
            );
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=handoff result=REFUSED reason=invalid_kernel code={}",
                e.code()
            );
            halt();
        }
    };

    // ---- stage: initrd ---------------------------------------------------
    // Loaded before the kernel segments are copied, so the page allocator can
    // be checked against the kernel's target range while nothing is written
    // yet. A mis-placed initrd is a load failure, never a silent overwrite.
    let initrd = load_initrd(bs, volume, &image, loader_range, &reserved, &mut ser);

    // ---- stage: load segments -------------------------------------------
    for i in 0..image.segment_count {
        let seg = image.segments[i];
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=elf-load segment={} paddr=0x{:x} vaddr=0x{:x} file_offset=0x{:x} filesz=0x{:x} memsz=0x{:x} flags=0x{:x}",
            i, seg.paddr, seg.vaddr, image.file_offsets[i], seg.filesz, seg.memsz, seg.flags
        );
    }

    copy_segments(file, &image, &mut ser);

    // ---- stage: BootInfo ------------------------------------------------
    let kernel_phys_start = image.segments[..image.segment_count]
        .iter()
        .map(|s| s.paddr)
        .min()
        .unwrap_or(0);
    let kernel_phys_end = image.segments[..image.segment_count]
        .iter()
        .map(|s| s.end())
        .max()
        .unwrap_or(0);

    // SAFETY: `BOOT_INFO` is a static we own and nothing else holds a
    // reference to it.
    let bi = unsafe { &mut *addr_of_mut!(BOOT_INFO) };
    bi.magic = bootinfo::BOOTINFO_MAGIC;
    bi.abi_version = bootinfo::ABI_VERSION;
    bi.boot_tsc = read_tsc();
    bi.kernel_entry = image.entry;
    bi.kernel_image_bytes = image.image_bytes;
    bi.segment_count = image.segment_count as u32;
    bi.kernel_phnum = image.phnum;
    bi.segments = image.segments;
    bi.memory_region_count = region_count as u32;
    bi.memory_region_capacity = bootinfo::MAX_MEMORY_REGIONS as u32;
    bi.map_desc_size = desc_size as u32;
    bi.map_desc_version = desc_version;
    bi.map_key = map_key as u64;
    bi.rsdp_phys = acpi.phys;
    bi.rsdp_checksum = acpi.checksum;
    bi.acpi_present = acpi.present;
    bi.rsdp_revision = acpi.revision;
    bi.framebuffer = fb;
    bi.initrd_phys = initrd.phys;
    bi.initrd_len = initrd.len;
    bi.kernel_phys_start = kernel_phys_start;
    bi.kernel_phys_len = kernel_phys_end - kernel_phys_start;
    bi.loader_reserved_count = reserved.count as u32;
    bi.loader_reserved_capacity = bootinfo::MAX_LOADER_RESERVED as u32;
    bi.loader_reserved = reserved.items;
    bi.boot_slot = bootinfo::SLOT_A;
    bi.boot_reason = bootinfo::boot_reason::COLD_BOOT;
    bi.debug_serial = DebugSerial {
        base_port: COM1,
        present: serial_present as u8,
        _reserved: 0,
        baud: 115200,
    };
    bi.system_table = system_table as u64;

    let mut flags = 0u32;
    if clamped {
        flags |= bootinfo::FLAG_MEMMAP_CLAMPED;
    }
    if reserved.clamped {
        flags |= bootinfo::FLAG_LOADER_RESERVED_CLAMPED;
    }
    if fb.phys != 0 && fb.width != 0 {
        flags |= bootinfo::FLAG_FRAMEBUFFER_PRESENT;
    }
    if acpi.present != 0 {
        flags |= bootinfo::FLAG_ACPI_PRESENT;
    }
    if initrd.len != 0 {
        flags |= bootinfo::FLAG_INITRD_PRESENT;
    }
    bi.flags = flags;

    // The structure is *not* sealed yet: `map_key` is only final after
    // `ExitBootServices` succeeds, and sealing twice would mean the checksum
    // the kernel verifies is not the one computed over the fields it read.
    // One seal, immediately before the jump.

    // Print the fields the kernel will read, so the two traces can be compared
    // line for line.
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=bootinfo field=memory_map count={} capacity={} desc_size={} map_key=0x{:x}",
        bi.memory_region_count, bi.memory_region_capacity, bi.map_desc_size, bi.map_key
    );
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=bootinfo field=acpi present={} rsdp=0x{:x} revision={} checksum=0x{:02x}",
        bi.acpi_present, bi.rsdp_phys, bi.rsdp_revision, bi.rsdp_checksum
    );
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=bootinfo field=framebuffer phys=0x{:x} width={} height={} stride={} format={} size=0x{:x}",
        bi.framebuffer.phys,
        bi.framebuffer.width,
        bi.framebuffer.height,
        bi.framebuffer.stride,
        bootinfo::pixel_format::name(bi.framebuffer.pixel_format),
        bi.framebuffer.size_bytes
    );
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=bootinfo field=initrd phys=0x{:x} len=0x{:x}",
        bi.initrd_phys, bi.initrd_len
    );
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=bootinfo field=kernel_phys start=0x{:x} len=0x{:x} phnum={}",
        bi.kernel_phys_start, bi.kernel_phys_len, bi.kernel_phnum
    );
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=bootinfo field=loader_reserved count={}",
        bi.loader_reserved_count
    );
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=bootinfo field=boot_slot value={} boot_reason={}",
        bi.boot_slot,
        bootinfo::boot_reason::name(bi.boot_reason)
    );
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=bootinfo field=debug_serial port=0x{:x} baud={} present={}",
        bi.debug_serial.base_port, bi.debug_serial.baud, bi.debug_serial.present
    );

    // ---- stage: exit boot services --------------------------------------
    let exit_status = exit_boot_services(bs, image_handle, &mut map_key, &mut ser);
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=exit-boot-services result={} status=0x{:x} map_key=0x{:x}",
        if exit_status == EFI_SUCCESS { "EFI_SUCCESS" } else { "FAILED" },
        exit_status,
        map_key
    );
    if exit_status != EFI_SUCCESS {
        let _ = writeln!(ser, "[ARGUS-LOADER] stage=handoff result=REFUSED reason=exit_boot_services_failed");
        halt();
    }

    // The map key that actually worked is the one the kernel needs to reason
    // about the map it was handed, so record it before sealing.
    bi.map_key = map_key as u64;

    // The single seal. Every field above is final from here on, because the
    // checksum covers all of them.
    bi.seal();
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=bootinfo event=sealed address=0x{:x} magic=0x{:x} abi_version={} abi_size={} checksum=0x{:08x} flags=0x{:x}",
        addr_of!(BOOT_INFO) as usize,
        bi.magic,
        bi.abi_version,
        bi.abi_size,
        bi.checksum,
        bi.flags
    );

    // Test hook: `tests/bootinfo-checksum.sh` builds the loader with
    // `--cfg argus_test_corrupt_bootinfo` and this flips a flag bit *after*
    // sealing, which is exactly the silent-corruption case the kernel's
    // checksum check exists for. Never compiled into a normal build.
    #[cfg(argus_test_corrupt_bootinfo)]
    {
        bi.flags |= bootinfo::FLAG_TEST_CORRUPTED;
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=bootinfo event=test_corruption injected=1 field=flags bit=15 checksum_now_stale=1"
        );
    }

    // ---- stage: handoff --------------------------------------------------
    // Nothing below this line may call a boot service: they are gone.
    let bootinfo_ptr: *const BootInfo = addr_of!(BOOT_INFO);
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=handoff event=jump entry=0x{:x} bootinfo=0x{:x} abi=sysv-x86_64 arg0=rdi checksum=0x{:08x}",
        image.entry, bootinfo_ptr as usize, bi.checksum
    );

    // SAFETY: the entry point was validated to lie inside an executable
    // PT_LOAD segment that we just copied, in memory the firmware marked
    // usable, with no overlap against this loader. `bootinfo_ptr` points at a
    // static that outlives this function. This never returns by construction.
    unsafe { jump_to_kernel(bootinfo_ptr, image.entry) }
}

/// Hand control to the kernel. Never returns.
///
/// # Safety
///
/// `entry` must be a validated executable address inside a segment that has
/// already been copied into memory, and `bootinfo` must point at a live
/// `BootInfo` that remains mapped for the kernel's lifetime.
unsafe fn jump_to_kernel(bootinfo: *const BootInfo, entry: u64) -> ! {
    let stack_top = addr_of_mut!(JUMP_STACK) as usize + 8192;
    // SAFETY: we are about to stop being a UEFI application. Switching RSP to
    // our own static stack means the following instructions do not rely on
    // firmware-owned memory. `rdi` is the x86-64 SysV first argument register,
    // which is where the kernel's `_start` expects the BootInfo pointer.
    // `options(noreturn)` is correct: the target never returns to this frame.
    unsafe {
        core::arch::asm!(
            "mov rsp, {stack}",
            "mov rdi, {bi}",
            "jmp {entry}",
            stack = in(reg) stack_top,
            bi = in(reg) bootinfo as usize,
            entry = in(reg) entry,
            options(noreturn)
        );
    }
}

fn read_tsc() -> u64 {
    // SAFETY: `rdtsc` reads a monotonically increasing cycle counter. It has
    // no memory side effects and cannot fault in ring 0.
    unsafe { core::arch::x86_64::_rdtsc() }
}

// ---------------------------------------------------------------------------
// ACPI
// ---------------------------------------------------------------------------

struct AcpiInfo {
    phys: u64,
    checksum: u8,
    present: u8,
    revision: u8,
}

/// Find the ACPI RSDP in the system table's configuration table array.
///
/// ACPI 2.0+ is preferred (36-byte RSDP, XSDT); the 1.0 entry is the fallback.
/// The checksum is computed here and stored, so the kernel can prove the table
/// it was handed is the one the firmware published without trusting us.
fn find_acpi(st: &SystemTable, ser: &mut Serial) -> AcpiInfo {
    let none = AcpiInfo {
        phys: 0,
        checksum: 0,
        present: 0,
        revision: 0,
    };

    if st.configuration_table.is_null() || st.number_of_table_entries == 0 {
        let _ = writeln!(ser, "[ARGUS-LOADER] stage=acpi event=absent reason=no_configuration_table");
        return none;
    }

    // Two passes: the first takes ACPI 2.0+, the second accepts 1.0.
    for (pass, want) in [(1u8, ACPI_20_TABLE_GUID), (2u8, ACPI_TABLE_GUID)] {
        for i in 0..st.number_of_table_entries {
            // SAFETY: the firmware reported `number_of_table_entries` entries
            // at `configuration_table`; every index below that count is a valid
            // EFI_CONFIGURATION_TABLE.
            let entry = unsafe { &*(st.configuration_table as *const ConfigurationTable).add(i) };
            if entry.vendor_guid != want || entry.vendor_table.is_null() {
                continue;
            }
            let phys = entry.vendor_table as u64;
            // SAFETY: the configuration table points at an RSDP the firmware
            // published; it is at least 20 bytes and lives in memory that is
            // mapped until ExitBootServices, which we have not called.
            let bytes = unsafe { core::slice::from_raw_parts(entry.vendor_table as *const u8, 36) };
            if &bytes[0..8] != b"RSD PTR " {
                let _ = writeln!(
                    ser,
                    "[ARGUS-LOADER] stage=acpi event=bad_signature pass={} phys=0x{:x}",
                    pass, phys
                );
                continue;
            }
            let revision = bytes[15];
            let len = if revision >= 2 { 36 } else { 20 };
            let sum = bytes[..len].iter().fold(0u8, |a, b| a.wrapping_add(*b));
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=acpi event=found pass={} phys=0x{:x} revision={} length={} checksum_sum=0x{:02x} valid={}",
                pass,
                phys,
                revision,
                len,
                sum,
                (sum == 0) as u8
            );
            if sum != 0 {
                // A bad checksum is a finding, not something to paper over:
                // report the table but refuse to mark ACPI as present.
                return AcpiInfo {
                    phys,
                    checksum: sum,
                    present: 0,
                    revision,
                };
            }
            return AcpiInfo {
                phys,
                checksum: sum,
                present: 1,
                revision,
            };
        }
    }

    let _ = writeln!(ser, "[ARGUS-LOADER] stage=acpi event=absent reason=no_rsdp_in_configuration_table");
    none
}

// ---------------------------------------------------------------------------
// Framebuffer
// ---------------------------------------------------------------------------

/// Read the current GOP mode into the `BootInfo` form.
///
/// With `-display none` there is still a VGA device, so OVMF normally has a
/// GOP; if it does not, the fields stay zero and the flag stays clear. A missing
/// GOP must never stop a boot — the serial console is the diagnostic channel
/// (Phase I §9: "Pornirea fără interfață grafică rămâne posibilă").
fn find_framebuffer(bs: &BootServices, ser: &mut Serial) -> FramebufferInfo {
    let mut count = 0usize;
    let mut buf: *mut Handle = null_mut();
    // SAFETY: `locate_handle_buffer` allocates the array with pool memory and
    // reports its length in `count`; we free it below with the matching
    // `free_pool`.
    let s = unsafe {
        (bs.locate_handle_buffer)(
            EFI_LOCATE_BY_PROTOCOL,
            &GRAPHICS_OUTPUT_PROTOCOL,
            null_mut(),
            &mut count,
            &mut buf,
        )
    };
    if is_error(s) || buf.is_null() || count == 0 {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=gop event=absent status=0x{:x} handles={}",
            s, count
        );
        return FramebufferInfo::ZERO;
    }

    let mut proto: *mut c_void = null_mut();
    // SAFETY: `buf[0]` is a handle the firmware just returned; `handle_protocol`
    // either writes a valid protocol pointer into `proto` or returns an error.
    let hs = unsafe {
        (bs.handle_protocol)(*buf, &GRAPHICS_OUTPUT_PROTOCOL, &mut proto as *mut *mut c_void)
    };
    // SAFETY: `buf` came from `locate_handle_buffer` and is ours to free.
    unsafe { (bs.free_pool)(buf as *mut u8) };
    if is_error(hs) || proto.is_null() {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=gop event=absent reason=handle_protocol status=0x{:x}",
            hs
        );
        return FramebufferInfo::ZERO;
    }

    // SAFETY: `proto` is a live EFI_GRAPHICS_OUTPUT_PROTOCOL; `Mode` and its
    // `Info` are firmware-owned and valid until ExitBootServices.
    let gop = unsafe { &*(proto as *const GraphicsOutput) };
    if gop.mode.is_null() {
        let _ = writeln!(ser, "[ARGUS-LOADER] stage=gop event=absent reason=null_mode");
        return FramebufferInfo::ZERO;
    }
    // SAFETY: as above — `mode` is non-null and firmware-owned.
    let mode = unsafe { &*gop.mode };
    if mode.info.is_null() || mode.size_of_info < core::mem::size_of::<GopModeInfo>() {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=gop event=absent reason=no_mode_info size={}",
            mode.size_of_info
        );
        return FramebufferInfo::ZERO;
    }
    // SAFETY: `info` points at a GopModeInfo the firmware filled in, and we just
    // checked that at least that many bytes are there.
    let info = unsafe { &*mode.info };

    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=gop event=present mode={} max_mode={} base=0x{:x} size=0x{:x} resolution={}x{} stride={} format={}",
        mode.mode,
        mode.max_mode,
        mode.framebuffer_base,
        mode.framebuffer_size,
        info.horizontal_resolution,
        info.vertical_resolution,
        info.pixels_per_scan_line,
        bootinfo::pixel_format::name(info.pixel_format)
    );

    FramebufferInfo {
        phys: mode.framebuffer_base,
        width: info.horizontal_resolution,
        height: info.vertical_resolution,
        stride: info.pixels_per_scan_line,
        pixel_format: info.pixel_format,
        size_bytes: mode.framebuffer_size as u64,
    }
}

// ---------------------------------------------------------------------------
// Initrd
// ---------------------------------------------------------------------------

struct Initrd {
    phys: u64,
    len: u64,
}

/// Read `INITRD.BIN` off the boot volume and place it in freshly allocated
/// pages, or report that there is none.
///
/// The pages are allocated *before* the kernel segments are copied so the
/// overlap check can run against the kernel's final target range while that
/// range is still untouched memory. If the firmware hands back pages that
/// would collide with the kernel, the initrd is refused rather than silently
/// corrupting the kernel image.
fn load_initrd(
    bs: &BootServices,
    volume: *mut SimpleFileSystem,
    image: &elf::KernelImage,
    loader_range: (u64, u64),
    reserved: &Reservations,
    ser: &mut Serial,
) -> Initrd {
    let absent = Initrd { phys: 0, len: 0 };

    let mut root: *mut File = null_mut();
    // SAFETY: `volume` is a protocol pointer the firmware returned and that we
    // already used successfully to read the kernel.
    let s = unsafe { ((*volume).open_volume)(volume, &mut root as *mut *mut File) };
    if is_error(s) || root.is_null() {
        let _ = writeln!(ser, "[ARGUS-LOADER] stage=initrd event=absent reason=open_volume_failed");
        return absent;
    }

    let mut wide = [0u16; 64];
    let mut n = 0usize;
    for unit in INITRD_PATH.encode_utf16() {
        if n + 1 >= wide.len() {
            let _ = writeln!(ser, "[ARGUS-LOADER] stage=initrd event=absent reason=path_too_long");
            return absent;
        }
        wide[n] = unit;
        n += 1;
    }
    wide[n] = 0;

    let mut file: *mut File = null_mut();
    // SAFETY: `root` is live, `wide` is a NUL-terminated UCS-2 path, and mode 0
    // with attributes 0 is the documented read-only open.
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
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=initrd event=absent reason=not_on_volume path={}",
            INITRD_PATH
        );
        return absent;
    }

    // --- size ---
    let mut info_size: usize = 0;
    // SAFETY: documented two-call idiom; a null buffer returns the size.
    let s = unsafe { ((*file).get_info)(file, &FILE_INFO_GUID, &mut info_size, null_mut()) };
    if s != EFI_BUFFER_TOO_SMALL && is_error(s) {
        let _ = writeln!(ser, "[ARGUS-LOADER] stage=initrd event=absent reason=get_info_failed");
        return absent;
    }
    let mut info_buf: *mut u8 = null_mut();
    // SAFETY: allocating `info_size` bytes for the firmware to fill.
    let s = unsafe {
        (bs.allocate_pool)(efi::EFI_LOADER_DATA, info_size, &mut info_buf as *mut *mut u8)
    };
    if is_error(s) || info_buf.is_null() {
        let _ = writeln!(ser, "[ARGUS-LOADER] stage=initrd event=absent reason=allocate_info_buffer_failed");
        return absent;
    }
    // SAFETY: `info_buf` has `info_size` writable bytes.
    let s = unsafe { ((*file).get_info)(file, &FILE_INFO_GUID, &mut info_size, info_buf) };
    if is_error(s) {
        let _ = writeln!(ser, "[ARGUS-LOADER] stage=initrd event=absent reason=get_info_failed");
        return absent;
    }
    // SAFETY: EFI_FILE_INFO is at least 16 bytes and we checked the size.
    let file_size = unsafe { core::ptr::read_unaligned((info_buf as *const u64).add(1)) } as usize;
    // SAFETY: `info_buf` came from `allocate_pool`; we own it.
    unsafe { (bs.free_pool)(info_buf) };

    if file_size == 0 || file_size > MAX_INITRD_BYTES {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=initrd event=absent reason=implausible_size bytes={}",
            file_size
        );
        return absent;
    }

    // --- pages ---
    let pages = file_size.div_ceil(4096);
    let mut phys: PhysAddr = 0;
    // SAFETY: `allocate_pages` writes the chosen address into `phys` and
    // returns an error status instead of failing silently.
    let s = unsafe {
        (bs.allocate_pages)(
            EFI_ALLOCATE_ANY_PAGES,
            efi::EFI_LOADER_DATA,
            pages,
            &mut phys as *mut PhysAddr,
        )
    };
    if is_error(s) || phys == 0 {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=initrd event=absent reason=allocate_pages_failed status=0x{:x}",
            s
        );
        return absent;
    }

    let span = (pages * 4096) as u64;
    let end = phys + span;

    // --- the overlap check that makes this safe ---
    let kernel_hit = image.segments[..image.segment_count]
        .iter()
        .find(|seg| phys < seg.end() && seg.paddr < end);
    if let Some(seg) = kernel_hit {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=initrd event=REFUSED reason=overlaps_kernel phys=0x{:x} len=0x{:x} segment_paddr=0x{:x}",
            phys, span, seg.paddr
        );
        // SAFETY: these are the pages we just allocated, and we are giving
        // them straight back.
        unsafe { (bs.free_pages)(phys, pages) };
        return absent;
    }
    if phys < loader_range.0 + loader_range.1 && loader_range.0 < end {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=initrd event=REFUSED reason=overlaps_loader phys=0x{:x} len=0x{:x}",
            phys, span
        );
        // SAFETY: as above.
        unsafe { (bs.free_pages)(phys, pages) };
        return absent;
    }
    if let Some(kind) = reserved.overlaps(phys, span) {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=initrd event=REFUSED reason=overlaps_reserved kind={} phys=0x{:x}",
            bootinfo::reservation_kind::name(kind),
            phys
        );
        // SAFETY: as above.
        unsafe { (bs.free_pages)(phys, pages) };
        return absent;
    }

    // --- read straight into the destination pages ---
    let mut to_read = file_size;
    // SAFETY: the pages are ours, `to_read` never exceeds `file_size` which is
    // the size we allocated rounded up, and the firmware writes at most that.
    let s = unsafe { ((*file).read)(file, &mut to_read as *mut usize, phys as *mut u8) };
    if is_error(s) || to_read != file_size {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=initrd event=absent reason=read_failed status=0x{:x} got={} want={}",
            s, to_read, file_size
        );
        // SAFETY: as above.
        unsafe { (bs.free_pages)(phys, pages) };
        return absent;
    }
    // SAFETY: `file` is a live protocol pointer we opened above.
    unsafe { ((*file).close)(file) };

    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=initrd event=loaded path={} phys=0x{:x} len=0x{:x} pages={}",
        INITRD_PATH, phys, file_size, pages
    );
    Initrd {
        phys,
        len: file_size as u64,
    }
}

// ---------------------------------------------------------------------------
// Kernel loading
// ---------------------------------------------------------------------------

/// Copy the validated segments into place.
fn copy_segments(file: &[u8], image: &elf::KernelImage, ser: &mut Serial) {
    for i in 0..image.segment_count {
        let seg = image.segments[i];
        let offset = image.file_offsets[i];

        // SAFETY: `validate` established that the source range
        // [offset, offset+filesz) lies inside `file`, that the destination
        // range [paddr, paddr+memsz) lies inside a usable firmware region, and
        // that the destination does not overlap the loader's own image. Both
        // ranges are therefore valid, and they cannot overlap each other
        // because the file buffer is pool memory, distinct from the kernel's
        // link-time region.
        unsafe {
            let dst = seg.paddr as *mut u8;
            if seg.filesz > 0 {
                core::ptr::copy_nonoverlapping(
                    file.as_ptr().add(offset as usize),
                    dst,
                    seg.filesz as usize,
                );
            }
            let tail = (seg.memsz - seg.filesz) as usize;
            if tail > 0 {
                core::ptr::write_bytes(dst.add(seg.filesz as usize), 0, tail);
            }
        }
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=elf-load segment={} result=COPIED dst=0x{:x} from_file=0x{:x} zero_filled=0x{:x}",
            i,
            seg.paddr,
            offset,
            seg.memsz - seg.filesz
        );
    }
}

/// `ExitBootServices`, retried with a fresh map key when the firmware says the
/// map changed underneath us (the normal case on real hardware, where timers
/// and DMA descriptors are still being retired).
fn exit_boot_services(
    bs: &BootServices,
    image_handle: Handle,
    map_key: &mut usize,
    ser: &mut Serial,
) -> Status {
    for attempt in 1..=4 {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=exit-boot-services event=attempt n={} map_key=0x{:x}",
            attempt, *map_key
        );
        // SAFETY: `image_handle` is our own handle and `map_key` is the key
        // returned by the most recent GetMemoryMap.
        let status = unsafe { (bs.exit_boot_services)(image_handle, *map_key) };
        if status == EFI_SUCCESS {
            return status;
        }
        // The map changed. Re-read it to get a current key. Note we do NOT
        // re-copy it into BootInfo: the kernel is already loaded, and the
        // first map is the one that described the memory we used.
        let mut size: usize = MEMMAP_BUFFER_SIZE;
        let mut buf: *mut u8 = null_mut();
        // SAFETY: as above — fresh buffer, valid out-parameters.
        unsafe {
            if is_error((bs.allocate_pool)(efi::EFI_LOADER_DATA, size, &mut buf)) {
                return status;
            }
            let mut desc_size = 0usize;
            let mut version = 0u32;
            let s = (bs.get_memory_map)(
                &mut size,
                buf as *mut MemoryDescriptor,
                map_key,
                &mut desc_size,
                &mut version,
            );
            if is_error(s) {
                return status;
            }
        }
    }
    // Fall through with the last error status.
    0x8000_0000_0000_0002 // EFI_INVALID_PARAMETER
}

/// Fetch `EFI_LOADED_IMAGE_PROTOCOL` for our own image handle.
fn get_loaded_image(bs: &BootServices, image_handle: Handle) -> Option<*mut LoadedImage> {
    let mut ptr: *mut c_void = null_mut();
    // SAFETY: `handle_protocol` writes a protocol pointer into `ptr` when it
    // succeeds; the GUID is a valid static describing the protocol we want.
    let status = unsafe {
        (bs.handle_protocol)(image_handle, &LOADED_IMAGE_PROTOCOL, &mut ptr as *mut *mut c_void)
    };
    if is_error(status) || ptr.is_null() {
        None
    } else {
        Some(ptr as *mut LoadedImage)
    }
}

/// Ask one handle whether it publishes a filesystem.
fn probe_sfs(bs: &BootServices, handle: Handle) -> Option<*mut SimpleFileSystem> {
    let mut p: *mut c_void = null_mut();
    // SAFETY: `handle` is a firmware handle; `handle_protocol` either writes a
    // valid protocol pointer into `p` or returns an error, and both are
    // checked before `p` is used.
    let s = unsafe {
        (bs.handle_protocol)(handle, &SIMPLE_FILE_SYSTEM_PROTOCOL, &mut p as *mut *mut c_void)
    };
    if is_error(s) || p.is_null() {
        None
    } else {
        Some(p as *mut SimpleFileSystem)
    }
}

/// Find the filesystem this loader was booted from and read the kernel off it.
///
/// Route order, most specific first, and each route is logged with its status
/// so a failure names the route that failed:
///
/// 1. `LoadedImage->DeviceHandle` + `OpenProtocol(SimpleFileSystem)` — the
///    canonical route from the UEFI specification. On OVMF this is the one that
///    works, and it needs neither enumeration nor device-path resolution.
/// 2. `LocateDevicePath` on `LoadedImage->FilePath` — kept because some
///    firmware resolves the volume only this way.
/// 3. Every `SimpleFileSystem` handle in the system — the last resort, and the
///    only route that can pick the *wrong* volume, which is why it is last.
///
/// Returns the volume that worked along with the file, so the initrd can be
/// read from the same volume.
fn open_kernel_file(
    bs: &BootServices,
    li: &LoadedImage,
    ser: &mut Serial,
) -> Result<(*mut SimpleFileSystem, *mut u8, usize), &'static str> {
    let mut tried = 0u32;

    // --- route 1: the device handle the firmware reported -------------------
    // This is `OpenProtocol` on `DeviceHandle`, per the specification's own
    // description of how an image finds the volume it came from.
    let via_handle = probe_sfs(bs, li.device_handle);
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=kernel-read event=volume_probe route=loaded_image_device_handle handle=0x{:x} found={}",
        li.device_handle as usize,
        via_handle.is_some() as u8
    );
    if let Some(v) = via_handle {
        tried += 1;
        if let Ok((buf, len)) = read_file_from_volume(bs, v, KERNEL_PATH) {
            return Ok((v, buf, len));
        }
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=kernel-read event=volume_skip route=loaded_image_device_handle reason=file_not_found"
        );
    }

    // --- route 2: the image's own device path -------------------------------
    if !li.file_path.is_null() {
        let mut dp: *mut c_void = li.file_path;
        let mut dev: Handle = null_mut();
        // SAFETY: `dp` is the firmware-owned device path of our own image;
        // `locate_device_path` only reads it and writes the matched handle.
        let s = unsafe { (bs.locate_device_path)(&SIMPLE_FILE_SYSTEM_PROTOCOL, &mut dp, &mut dev) };
        let found = if !is_error(s) && !dev.is_null() {
            probe_sfs(bs, dev)
        } else {
            None
        };
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=kernel-read event=volume_probe route=locate_device_path status=0x{:x} found={}",
            s,
            found.is_some() as u8
        );
        if let Some(v) = found {
            tried += 1;
            if let Ok((buf, len)) = read_file_from_volume(bs, v, KERNEL_PATH) {
                return Ok((v, buf, len));
            }
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=kernel-read event=volume_skip route=locate_device_path reason=file_not_found"
            );
        }
    }

    // --- route 3: every SimpleFileSystem handle in the system ---------------
    let mut count = 0usize;
    let mut buf: *mut Handle = null_mut();
    // SAFETY: `locate_handle_buffer` allocates the handle array with pool
    // memory and reports its length in `count`; we free it below with the
    // matching `free_pool`, as the specification requires.
    let s = unsafe {
        (bs.locate_handle_buffer)(
            EFI_LOCATE_BY_PROTOCOL,
            &SIMPLE_FILE_SYSTEM_PROTOCOL,
            null_mut(),
            &mut count,
            &mut buf,
        )
    };
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=kernel-read event=volume_probe route=locate_handle_buffer status=0x{:x} handles={}",
        s, count
    );
    if !is_error(s) && !buf.is_null() {
        for i in 0..count {
            // SAFETY: the firmware returned `count` handles in `buf`, so every
            // index below `count` is a valid, initialised `Handle`.
            let h = unsafe { *buf.add(i) };
            if let Some(v) = probe_sfs(bs, h) {
                tried += 1;
                if let Ok((fbuf, len)) = read_file_from_volume(bs, v, KERNEL_PATH) {
                    // SAFETY: `buf` came from `locate_handle_buffer`.
                    unsafe { (bs.free_pool)(buf as *mut u8) };
                    return Ok((v, fbuf, len));
                }
                let _ = writeln!(
                    ser,
                    "[ARGUS-LOADER] stage=kernel-read event=volume_skip index={} reason=file_not_found",
                    i
                );
            }
        }
        // SAFETY: `buf` was allocated by `locate_handle_buffer` from pool
        // memory; the caller owns it and must free it exactly once.
        unsafe { (bs.free_pool)(buf as *mut u8) };
    }

    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=kernel-read event=no_volume routes_tried={}",
        tried
    );

    // --- fallback: read the FAT ourselves ----------------------------------
    // Reached only when the firmware exposes no usable filesystem driver on
    // this volume. This path does not depend on any firmware filesystem
    // support at all, but it cannot hand back a protocol pointer, so it
    // returns a null volume and the initrd is skipped.
    let mut block_vols: [Option<fat::Volume>; 8] = [None; 8];
    let n = fat::find_volumes(bs, &mut block_vols);
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=kernel-read event=block_probe fat16_volumes={}",
        n
    );
    for i in 0..n {
        let Some(vol) = block_vols[i] else { continue };
        match fat::read_root_file(bs, &vol, &KERNEL_NAME_83) {
            Ok((fbuf, len)) => {
                let _ = writeln!(
                    ser,
                    "[ARGUS-LOADER] stage=kernel-read event=block_read_ok volume={} bytes={}",
                    i, len
                );
                return Ok((null_mut(), fbuf, len));
            }
            Err(e) => {
                let _ = writeln!(
                    ser,
                    "[ARGUS-LOADER] stage=kernel-read event=block_skip volume={} reason={}",
                    i,
                    e.code()
                );
            }
        }
    }

    // Every route was tried and none produced the file. Name that, rather
    // than blaming a missing filesystem driver: on OVMF the driver is present
    // and this message used to send readers after the wrong bug.
    Err("kernel_file_not_found_on_any_route")
}

/// Open `path` on one volume and read the whole file into pool memory.
fn read_file_from_volume(
    bs: &BootServices,
    sfs: *mut SimpleFileSystem,
    path: &str,
) -> Result<(*mut u8, usize), &'static str> {
    // --- volume root ---
    let mut root: *mut File = null_mut();
    // SAFETY: `sfs` is a protocol pointer the firmware just returned;
    // `open_volume` writes the root handle into `root`.
    let s = unsafe { ((*sfs).open_volume)(sfs, &mut root as *mut *mut File) };
    if is_error(s) || root.is_null() {
        return Err("open_volume_failed");
    }

    // --- open the file ---
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
    // SAFETY: `root` is a live EFI_FILE_PROTOCOL, `wide` is a NUL-terminated
    // UCS-2 path on the stack, and mode 0 with attributes 0 is the documented
    // read-only open. `open` writes the new protocol pointer into `file`.
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
        return Err("file_not_found");
    }

    // --- how big is it? ---
    let mut info_size: usize = 0;
    // SAFETY: the documented two-call idiom: a zero-sized buffer returns
    // EFI_BUFFER_TOO_SMALL and the required size in `info_size`.
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
        (bs.allocate_pool)(efi::EFI_LOADER_DATA, info_size, &mut info_buf as *mut *mut u8)
    };
    if is_error(s) || info_buf.is_null() {
        return Err("allocate_info_buffer_failed");
    }
    // SAFETY: `info_buf` has `info_size` writable bytes; the firmware writes
    // at most that many.
    let s = unsafe { ((*file).get_info)(file, &FILE_INFO_GUID, &mut info_size, info_buf) };
    if is_error(s) {
        return Err("get_info_failed");
    }
    // EFI_FILE_INFO.file_size is at offset 8 (Size: u64, FileSize: u64, ...).
    // SAFETY: `info_size` is at least 16 as checked above, so bytes 8..16 are
    // inside the buffer the firmware just filled.
    let file_size = unsafe { core::ptr::read_unaligned((info_buf as *const u64).add(1)) };
    if file_size == 0 || file_size > 16 * 1024 * 1024 {
        return Err("file_size_implausible");
    }

    // --- read it ---
    let mut buf: *mut u8 = null_mut();
    // SAFETY: allocating `file_size` bytes for the file contents.
    let s = unsafe {
        (bs.allocate_pool)(
            efi::EFI_LOADER_DATA,
            file_size as usize,
            &mut buf as *mut *mut u8,
        )
    };
    if is_error(s) || buf.is_null() {
        return Err("allocate_file_buffer_failed");
    }

    let mut to_read = file_size as usize;
    // SAFETY: `buf` has `file_size` writable bytes and `to_read` never exceeds
    // that. The firmware writes at most `to_read` bytes and shrinks it.
    let s = unsafe { ((*file).read)(file, &mut to_read as *mut usize, buf) };
    if is_error(s) {
        return Err("read_failed");
    }
    // SAFETY: `file` is a live protocol pointer we opened above.
    unsafe { ((*file).close)(file) };

    if to_read == 0 {
        return Err("read_zero_bytes");
    }
    Ok((buf, to_read))
}
