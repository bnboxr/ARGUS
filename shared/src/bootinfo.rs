//! The `BootInfo` contract between the ARGUS UEFI loader and the ARGUS kernel.
//!
//! # Rules this structure obeys
//!
//! * `#[repr(C)]` — the kernel is a separate compilation unit and reads this
//!   memory through a raw pointer. Rust's default layout is not stable, so the
//!   layout is pinned explicitly.
//! * `version` and `size` are the first two fields, so a kernel compiled
//!   against an older header can refuse a newer structure instead of
//!   misreading it.
//! * Every array is fixed-size and inline. The loader copies the UEFI memory
//!   map *into* this structure, which lives in the loader's own image (static
//!   storage). Nothing here points at `AllocatePool` memory, because all of
//!   that is released the moment `ExitBootServices` returns.
//! * No pointers into boot-services memory, no function pointers. After
//!   `ExitBootServices` every one of those would be dangling.
//!
//! The loader passes a `*const BootInfo` in the first argument register
//! (`RDI`) of the x86-64 SysV ABI.

/// Bumped whenever the layout or the meaning of any field changes.
///
/// History:
/// * `1` — Phase I milestone 1: magic/version/size, kernel entry + segments,
///   memory map, system table pointer, serial port.
pub const BOOTINFO_VERSION: u32 = 1;

/// Magic value guarding against a garbage or NULL pointer.
///
/// ASCII `ARGUS` in the high bytes, then the version it was built for.
pub const BOOTINFO_MAGIC: u64 = 0x4152_4755_5300_0001; // "ARGUS" + 0x0001

/// Maximum number of `PT_LOAD` segments the loader will accept in a kernel.
/// Chosen small on purpose: a kernel image with more than 8 loadable segments
/// is a sign the linker script is wrong, not something to silently support.
pub const MAX_SEGMENTS: usize = 8;

/// Maximum number of memory-map entries copied into `BootInfo`.
///
/// OVMF on a 128 MiB VM produces ~127 descriptors, and real firmware
/// routinely produces more, so this is sized to hold a full map rather than to
/// keep the structure small: a truncated map is worse than a large struct.
/// 192 entries × 40 bytes = 7.5 KiB, which costs nothing as a static in the
/// loader image. If a map ever overflows even this, the loader sets
/// [`FLAG_MEMMAP_CLAMPED`] and says so on the wire — it never truncates
/// silently.
pub const MAX_MEMORY_REGIONS: usize = 192;

/// UEFI memory types, copied verbatim from the specification.
pub mod memory_type {
    pub const RESERVED: u32 = 0;
    pub const LOADER_CODE: u32 = 1;
    pub const LOADER_DATA: u32 = 2;
    pub const BOOT_SERVICES_CODE: u32 = 3;
    pub const BOOT_SERVICES_DATA: u32 = 4;
    pub const RUNTIME_SERVICES_CODE: u32 = 5;
    pub const RUNTIME_SERVICES_DATA: u32 = 6;
    pub const CONVENTIONAL_MEMORY: u32 = 7;
    pub const UNUSABLE_MEMORY: u32 = 8;
    pub const ACPI_RECLAIM_MEMORY: u32 = 9;
    pub const ACPI_MEMORY_NVS: u32 = 10;
    pub const MMIO: u32 = 11;
    pub const MMIO_PORT_SPACE: u32 = 12;
    pub const PAL_CODE: u32 = 13;
    pub const PERSISTENT_MEMORY: u32 = 14;

    /// Human-readable name, for the serial log.
    pub const fn name(t: u32) -> &'static str {
        match t {
            RESERVED => "Reserved",
            LOADER_CODE => "LoaderCode",
            LOADER_DATA => "LoaderData",
            BOOT_SERVICES_CODE => "BootServicesCode",
            BOOT_SERVICES_DATA => "BootServicesData",
            RUNTIME_SERVICES_CODE => "RuntimeServicesCode",
            RUNTIME_SERVICES_DATA => "RuntimeServicesData",
            CONVENTIONAL_MEMORY => "ConventionalMemory",
            UNUSABLE_MEMORY => "UnusableMemory",
            ACPI_RECLAIM_MEMORY => "ACPIReclaimMemory",
            ACPI_MEMORY_NVS => "ACPIMemoryNVS",
            MMIO => "MemoryMappedIO",
            MMIO_PORT_SPACE => "MemoryMappedIOPortSpace",
            PAL_CODE => "PalCode",
            PERSISTENT_MEMORY => "PersistentMemory",
            _ => "Undefined",
        }
    }

    /// Is this region a legitimate place to put kernel text/data?
    ///
    /// Conventional memory is the honest answer. Boot-services code/data and
    /// loader code/data are accepted too, because OVMF carves the low memory
    /// it hands to applications out of exactly those types, and a loader that
    /// refused them could not load anything on real firmware.
    pub const fn is_usable_for_kernel(t: u32) -> bool {
        matches!(
            t,
            LOADER_CODE
                | LOADER_DATA
                | BOOT_SERVICES_CODE
                | BOOT_SERVICES_DATA
                | CONVENTIONAL_MEMORY
                | PERSISTENT_MEMORY
        )
    }
}

/// One `PT_LOAD` segment of the kernel ELF, as loaded.
///
/// The kernel can use this to know where its own sections ended up without
/// re-parsing the ELF (which it no longer has, since the file buffer is gone
/// after `ExitBootServices`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SegmentInfo {
    /// Link-time virtual address from the ELF program header.
    pub vaddr: u64,
    /// Physical address the loader actually copied the bytes to.
    pub paddr: u64,
    /// Bytes copied from the file (`p_filesz`).
    pub filesz: u64,
    /// Bytes the segment occupies in memory (`p_memsz`). The region beyond
    /// `filesz` was zero-filled by the loader.
    pub memsz: u64,
    /// ELF `p_flags` (bit 0 = execute, 1 = write, 2 = read).
    pub flags: u32,
    pub _reserved: u32,
}

impl SegmentInfo {
    pub const ZERO: SegmentInfo = SegmentInfo {
        vaddr: 0,
        paddr: 0,
        filesz: 0,
        memsz: 0,
        flags: 0,
        _reserved: 0,
    };
}

/// One entry of the UEFI memory map, flattened to the fields the kernel needs.
///
/// The UEFI descriptor is variable-length (`DescriptorSize` from
/// `GetMemoryMap`), so the loader normalises it into this fixed 40-byte form.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemoryRegion {
    /// One of [`memory_type`].
    pub region_type: u32,
    pub _reserved: u32,
    /// First physical byte of the region.
    pub physical_start: u64,
    /// Length in 4 KiB pages.
    pub page_count: u64,
    /// UEFI `Attribute` bitmask, copied verbatim.
    pub attributes: u64,
}

impl MemoryRegion {
    pub const ZERO: MemoryRegion = MemoryRegion {
        region_type: 0,
        _reserved: 0,
        physical_start: 0,
        page_count: 0,
        attributes: 0,
    };

    pub const fn end(&self) -> u64 {
        self.physical_start + self.page_count * 4096
    }
}

/// Everything the loader hands the kernel.
///
/// Field-by-field, in declaration order:
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootInfo {
    /// `BOOTINFO_MAGIC`. A kernel that does not see this value was jumped to
    /// with a bad pointer and must not trust anything below.
    pub magic: u64,
    /// `BOOTINFO_VERSION`. The kernel refuses structures with a different
    /// major version rather than guessing.
    pub version: u32,
    /// `size_of::<BootInfo>()` as compiled *by the loader*. The kernel
    /// compares this with its own value; a mismatch means the two sides were
    /// built from different revisions of this file.
    pub size: u32,
    /// `RDTSC` value sampled immediately before the jump. Not a wall-clock
    /// time — there is no calibrated TSC yet — only a monotonic stamp the
    /// kernel can use to measure how long the handoff took.
    pub boot_tsc: u64,
    /// ELF `e_entry` of the kernel. The kernel can compare this with the
    /// address it believes it is running at.
    pub kernel_entry: u64,
    /// Sum of `p_filesz` over all loaded segments: how many bytes the loader
    /// actually copied out of the kernel file.
    pub kernel_image_bytes: u64,
    /// Number of valid entries in `segments`.
    pub segment_count: u32,
    /// Padding, always 0. Present so `segments` starts on an 8-byte boundary.
    pub _reserved0: u32,
    /// The kernel's `PT_LOAD` segments, in ELF program-header order.
    /// Entries `segment_count..MAX_SEGMENTS` are zeroed.
    pub segments: [SegmentInfo; MAX_SEGMENTS],
    /// Number of valid entries in `memory_regions`.
    pub memory_region_count: u32,
    /// `MAX_MEMORY_REGIONS`. If `memory_region_count` had to be clamped, the
    /// loader sets a bit in `flags` and logs a warning; it never silently
    /// drops entries without saying so.
    pub memory_region_capacity: u32,
    /// The UEFI memory map as it stood at `ExitBootServices`.
    pub memory_regions: [MemoryRegion; MAX_MEMORY_REGIONS],
    /// `EFI_SYSTEM_TABLE *`, kept for later phases (runtime services, ACPI
    /// table discovery). Valid after `ExitBootServices` only for runtime
    /// services calls.
    pub system_table: u64,
    /// I/O port of the serial console both sides log to. 0x3F8 (COM1) today.
    pub serial_port: u16,
    /// BootInfo `flags`. Bit 0 = memory map was clamped to
    /// `MAX_MEMORY_REGIONS`.
    pub flags: u16,
    pub _reserved1: u32,
}

impl BootInfo {
    /// A fully zeroed structure, used to reserve static storage in the loader.
    pub const ZERO: BootInfo = BootInfo {
        magic: 0,
        version: 0,
        size: 0,
        boot_tsc: 0,
        kernel_entry: 0,
        kernel_image_bytes: 0,
        segment_count: 0,
        _reserved0: 0,
        segments: [SegmentInfo::ZERO; MAX_SEGMENTS],
        memory_region_count: 0,
        memory_region_capacity: 0,
        memory_regions: [MemoryRegion::ZERO; MAX_MEMORY_REGIONS],
        system_table: 0,
        serial_port: 0,
        flags: 0,
        _reserved1: 0,
    };

    /// True if this structure came from a loader we can talk to.
    pub fn is_compatible(&self) -> bool {
        self.magic == BOOTINFO_MAGIC && self.version == BOOTINFO_VERSION
    }
}

/// Flag bit: the memory map did not fit in [`MAX_MEMORY_REGIONS`].
pub const FLAG_MEMMAP_CLAMPED: u16 = 1 << 0;
