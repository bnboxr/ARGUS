//! ARGUS UEFI loader — Phase I, milestone 1.
//!
//! What this program does, in order, and prints each step to COM1 as it goes:
//!
//! 1. bring up the serial port and print a version banner
//! 2. ask the firmware for the memory map and print all of it
//! 3. read `\EFI\BOOT\ARGUSKRN.ELF` from the boot volume
//! 4. validate it (see `elf.rs` for the twelve rules) — reject with a named
//!    code, or accept
//! 5. copy each `PT_LOAD` segment to its physical address, zero-filling the
//!    tail of each segment
//! 6. fill in `BootInfo`
//! 7. `ExitBootServices`
//! 8. jump to `e_entry` with a `*const BootInfo` in `RDI` (x86-64 SysV ABI)
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
use core::ptr::{addr_of_mut, null_mut};

use argus_shared::bootinfo::{self, BootInfo, MemoryRegion};
use argus_shared::serial::{Serial, COM1};

use efi::*;

/// Version of this loader, printed in the banner and easy to grep.
const LOADER_VERSION: &str = "0.1.0";

/// Where the kernel ELF lives on the boot volume. Matches `build.sh`.
const KERNEL_PATH: &str = "\\ARGUSKRN.ELF";

/// The same file as an 8.3 FAT directory entry: 11 bytes, space padded, used
/// by the block-level FAT reader when the firmware offers no filesystem.
const KERNEL_NAME_83: [u8; 11] = *b"ARGUSKRNELF";

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

/// Entry point. The UEFI target links this as the PE32+ entry (`efi_main`).
#[no_mangle]
pub extern "efiapi" fn efi_main(image_handle: Handle, system_table: *mut SystemTable) -> Status {
    let mut ser = Serial::new(COM1);
    let serial_present = ser.init();

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
        "[ARGUS-LOADER] stage=banner event=start name=ARGUS loader=uefi-x86_64 version={} bootinfo_version={}",
        LOADER_VERSION,
        bootinfo::BOOTINFO_VERSION
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
        let end = d.physical_start + d.number_of_pages * 4096;
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=memmap entry={} type={} name={} start=0x{:x} pages={} end=0x{:x} attr=0x{:x}",
            i,
            d.region_type,
            argus_shared::bootinfo::memory_type::name(d.region_type),
            d.physical_start,
            d.number_of_pages,
            end,
            d.attribute
        );
        if argus_shared::bootinfo::memory_type::is_usable_for_kernel(d.region_type) {
            usable_bytes += d.number_of_pages * 4096;
        }

        // SAFETY: `BOOT_INFO` is a static we own; no other reference to it
        // exists at this point, so taking a mutable reference here is sound.
        let bi = unsafe { &mut *addr_of_mut!(BOOT_INFO) };
        if region_count < bootinfo::MAX_MEMORY_REGIONS {
            bi.memory_regions[region_count] = MemoryRegion {
                region_type: d.region_type,
                _reserved: 0,
                physical_start: d.physical_start,
                page_count: d.number_of_pages,
                attributes: d.attribute,
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

    let (kernel_buf, kernel_len) = match read_kernel_file(bs, li, KERNEL_PATH, &mut ser) {
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

    // ---- stage: ELF validation ------------------------------------------
    // SAFETY: `kernel_buf` holds `kernel_len` initialised bytes read from the
    // file above; building a slice over exactly that range is sound.
    let file = unsafe { core::slice::from_raw_parts(kernel_buf, kernel_len) };

    // The kernel must not land on top of the running loader.
    let loader_range = (li.image_base as u64, li.image_size);

    let image = match elf::validate(file, &bootinfo_regions(), loader_range) {
        Ok(img) => {
            let _ = writeln!(
                ser,
                "[ARGUS-LOADER] stage=elf-validate result=ACCEPT entry=0x{:x} segments={} image_bytes={}",
                img.entry, img.segment_count, img.image_bytes
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
    // SAFETY: `BOOT_INFO` is a static we own and nothing else holds a
    // reference to it.
    let bi = unsafe { &mut *addr_of_mut!(BOOT_INFO) };
    bi.magic = bootinfo::BOOTINFO_MAGIC;
    bi.version = bootinfo::BOOTINFO_VERSION;
    bi.size = core::mem::size_of::<BootInfo>() as u32;
    bi.boot_tsc = read_tsc();
    bi.kernel_entry = image.entry;
    bi.kernel_image_bytes = image.image_bytes;
    bi.segment_count = image.segment_count as u32;
    bi.segments = image.segments;
    bi.memory_region_count = region_count as u32;
    bi.memory_region_capacity = bootinfo::MAX_MEMORY_REGIONS as u32;
    bi.system_table = system_table as u64;
    bi.serial_port = COM1;
    bi.flags = if clamped { bootinfo::FLAG_MEMMAP_CLAMPED } else { 0 };

    let bootinfo_ptr: *const BootInfo = addr_of_mut!(BOOT_INFO);
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=bootinfo event=filled address=0x{:x} magic=0x{:x} version={} size={} segments={} regions={} entry=0x{:x}",
        bootinfo_ptr as usize,
        bi.magic,
        bi.version,
        bi.size,
        bi.segment_count,
        bi.memory_region_count,
        bi.kernel_entry
    );

    // ---- stage: exit boot services --------------------------------------
    let exit_status = exit_boot_services(bs, image_handle, &mut map_key, &mut ser);
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=exit-boot-services result={} status=0x{:x}",
        if exit_status == EFI_SUCCESS { "EFI_SUCCESS" } else { "FAILED" },
        exit_status
    );
    if exit_status != EFI_SUCCESS {
        let _ = writeln!(ser, "[ARGUS-LOADER] stage=handoff result=REFUSED reason=exit_boot_services_failed");
        halt();
    }

    // ---- stage: handoff --------------------------------------------------
    // Nothing below this line may call a boot service: they are gone.
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=handoff event=jump entry=0x{:x} bootinfo=0x{:x} abi=sysv-x86_64 arg0=rdi",
        image.entry, bootinfo_ptr as usize
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

/// Snapshot of the memory map copied into `BootInfo`, for validation.
fn bootinfo_regions() -> [MemoryRegion; bootinfo::MAX_MEMORY_REGIONS] {
    // SAFETY: `BOOT_INFO` is a static; reading it here happens after the copy
    // loop finished and before any other access, with no live mutable borrow.
    unsafe { (*addr_of_mut!(BOOT_INFO)).memory_regions }
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

/// How many volumes we are willing to probe while looking for the kernel.
const MAX_VOLUMES: usize = 8;

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

fn push_volume(out: &mut [*mut SimpleFileSystem], n: &mut usize, p: *mut SimpleFileSystem) {
    if p.is_null() || *n >= out.len() {
        return;
    }
    if out[..*n].iter().any(|&v| v == p) {
        return;
    }
    out[*n] = p;
    *n += 1;
}

/// Collect every volume that offers a filesystem, most-specific route first.
///
/// Why three routes: `LoadedImage.DeviceHandle` is documented as "the device
/// the image was loaded from", but for an image loaded from a file OVMF
/// reports the *controller* (the virtio disk), which does not publish
/// SimpleFileSystem — measured, status `EFI_UNSUPPORTED`. `LocateDevicePath`
/// on the image's own path is the canonical route and also comes up empty on
/// this firmware — measured, status `EFI_NOT_FOUND`. So the loader falls back
/// to enumerating every SimpleFileSystem handle in the system, which always
/// works, and then tries the kernel path on each volume in turn.
fn collect_volumes(
    bs: &BootServices,
    li: &LoadedImage,
    ser: &mut Serial,
) -> ([*mut SimpleFileSystem; MAX_VOLUMES], usize) {
    let mut out = [null_mut::<SimpleFileSystem>(); MAX_VOLUMES];
    let mut n = 0usize;

    // --- route 0: rebuild the device stack ---------------------------------
    //
    // Measured on OVMF (QEMU/OVMF_CODE_4M.fd): when our image starts, there is
    // no EFI_SIMPLE_FILE_SYSTEM_PROTOCOL handle anywhere in the system (0 of
    // 238 handles), and our own EFI_LOADED_IMAGE_PROTOCOL carries only a
    // relative FilePath node (\EFI\BOOT\BOOTX64.EFI) over a bare
    // PciRoot(0x0)/Pci(0x3,0x0) device handle. BDS expanded the boot option,
    // read the file, and disconnected the drivers again — so the volume we
    // were loaded from has no filesystem driver bound to it any more.
    //
    // A plain ConnectController is not enough: it returns EFI_NOT_FOUND
    // because the drivers are *still bound to the controller*, only their
    // children are gone. Disconnect first, then connect recursively, and the
    // chain VirtioBlkDxe -> PartitionDxe -> FatDxe is rebuilt from scratch.
    //
    // SAFETY: `li.device_handle` is our own image's device handle. Disconnecting
    // it cannot free our code — the image itself lives in EfiLoaderCode pages
    // owned by the image handle, not by this controller.
    let disconnect = unsafe {
        (bs.disconnect_controller)(li.device_handle, core::ptr::null(), core::ptr::null())
    };
    // SAFETY: same handle; NULL driver list and NULL remaining path mean
    // "all drivers, whole device", recursive.
    let connect = unsafe {
        (bs.connect_controller)(li.device_handle, core::ptr::null(), core::ptr::null(), 1)
    };
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=kernel-read event=reconnect handle=0x{:x} disconnect=0x{:x} connect=0x{:x}",
        li.device_handle as usize, disconnect, connect
    );

    // --- route 1: the image's own device path ------------------------------
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
            push_volume(&mut out, &mut n, v);
        }
    }

    // --- route 2: the device handle the firmware reported -------------------
    let via_handle = probe_sfs(bs, li.device_handle);
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=kernel-read event=volume_probe route=loaded_image_device_handle found={}",
        via_handle.is_some() as u8
    );
    if let Some(v) = via_handle {
        push_volume(&mut out, &mut n, v);
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
    if !is_error(s) && !buf.is_null() {
        for i in 0..count {
            // SAFETY: the firmware returned `count` handles in `buf`, so every
            // index below `count` is a valid, initialised `Handle`.
            let h = unsafe { *buf.add(i) };
            if let Some(v) = probe_sfs(bs, h) {
                push_volume(&mut out, &mut n, v);
            }
        }
        // SAFETY: `buf` was allocated by `locate_handle_buffer` from pool
        // memory; the caller owns it and must free it exactly once.
        unsafe { (bs.free_pool)(buf as *mut u8) };
    }
    let _ = writeln!(
        ser,
        "[ARGUS-LOADER] stage=kernel-read event=volume_probe route=locate_handle_buffer status=0x{:x} handles={} volumes={}",
        s, count, n
    );

    (out, n)
}

/// Read `KERNEL_PATH` off the volume this loader booted from.
///
/// Returns the pool buffer (owned by the caller, never freed — the process is
/// about to stop being a UEFI application) and the exact byte length.
fn read_kernel_file(
    bs: &BootServices,
    li: &LoadedImage,
    path: &str,
    ser: &mut Serial,
) -> Result<(*mut u8, usize), &'static str> {
    let (volumes, count) = collect_volumes(bs, li, ser);

    for i in 0..count {
        let _ = writeln!(
            ser,
            "[ARGUS-LOADER] stage=kernel-read event=volume_open index={} of={} sfs=0x{:x}",
            i,
            count,
            volumes[i] as usize
        );
        match read_file_from_volume(bs, volumes[i], path) {
            Ok(v) => return Ok(v),
            Err(e) => {
                let _ = writeln!(
                    ser,
                    "[ARGUS-LOADER] stage=kernel-read event=volume_skip index={} reason={}",
                    i, e
                );
            }
        }
    }

    // --- fallback: read the FAT ourselves ----------------------------------
    // Reached when the firmware exposes no usable filesystem driver on this
    // volume (see `fat.rs` for the measured reason). This path does not
    // depend on any firmware filesystem support at all.
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
            Ok((buf, len)) => {
                let _ = writeln!(
                    ser,
                    "[ARGUS-LOADER] stage=kernel-read event=block_read_ok volume={} bytes={}",
                    i, len
                );
                return Ok((buf, len));
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

    Err("no_simple_file_system_protocol_and_no_fat16_volume")
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
