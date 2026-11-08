//! The `BootInfo` ABI between the ARGUS UEFI loader and the ARGUS kernel.
//!
//! **This structure is frozen at ABI version 1.** `BOOTINFO.md` documents every
//! field, its width, its meaning, and why it could not have been added later.
//! Read that document before changing a byte here.
//!
//! # Rules this structure obeys
//!
//! * `#[repr(C)]` — the kernel is a separate compilation unit and reads this
//!   memory through a raw pointer. Rust's default layout is not stable, so the
//!   layout is pinned explicitly.
//! * `magic`, `abi_version`, `abi_size` and `checksum` come first. A kernel
//!   built against a different revision refuses the structure instead of
//!   misreading it, and refuses it *before* trusting any length in it.
//! * `checksum` covers every byte of the structure, with the checksum field
//!   itself taken as zero. It is the only integrity check that catches silent
//!   corruption of a field nobody thought to validate.
//! * Every array is fixed-size and inline. Nothing here points at
//!   `AllocatePool` memory, because all of that is released the moment
//!   `ExitBootServices` returns.
//! * No pointers into boot-services memory, no function pointers. After
//!   `ExitBootServices` every one of those would be dangling.
//!
//! The loader passes a `*const BootInfo` in the first argument register
//! (`RDI`) of the x86-64 SysV ABI.

/// ABI version. Bumped whenever the layout or the meaning of any field changes.
///
/// History:
/// * `1` — Phase I milestone 1. Frozen 2026-11-08: header with checksum,
///   memory map with `uefi_attribs` + `map_desc_size`/`map_key`, ACPI RSDP,
///   framebuffer, initrd, kernel physical placement, loader reservations,
///   boot slot/reason, debug serial.
pub const ABI_VERSION: u32 = 1;

/// Magic value guarding against a garbage or NULL pointer.
///
/// ASCII `ARGUS` in the high bytes, then the ABI version it was built for.
pub const BOOTINFO_MAGIC: u64 = 0x4152_4755_5300_0001; // "ARGUS" + 0x0001

/// Maximum number of `PT_LOAD` segments the loader will accept in a kernel.
/// Chosen small on purpose: a kernel image with more than 8 loadable segments
/// is a sign the linker script is wrong, not something to silently support.
pub const MAX_SEGMENTS: usize = 8;

/// Maximum number of memory-map entries copied into `BootInfo`.
///
/// OVMF on a 128 MiB VM produces ~129 descriptors, and real firmware
/// routinely produces more, so this is sized to hold a full map rather than to
/// keep the structure small: a truncated map is worse than a large struct.
/// 192 entries × 32 bytes = 6 KiB, which costs nothing as a static in the
/// loader image. If a map ever overflows even this, the loader sets
/// [`FLAG_MEMMAP_CLAMPED`] and says so on the wire — it never truncates
/// silently.
pub const MAX_MEMORY_REGIONS: usize = 192;

/// Maximum number of loader reservations carried to the kernel.
pub const MAX_LOADER_RESERVED: usize = 16;

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

/// Pixel formats from `EFI_GRAPHICS_PIXEL_FORMAT`.
pub mod pixel_format {
    pub const RED_GREEN_BLUE_RESERVED_8BIT: u32 = 0;
    pub const BLUE_GREEN_RED_RESERVED_8BIT: u32 = 1;
    pub const BIT_MASK: u32 = 2;
    pub const BLT_ONLY: u32 = 3;

    pub const fn name(f: u32) -> &'static str {
        match f {
            RED_GREEN_BLUE_RESERVED_8BIT => "RGBx8",
            BLUE_GREEN_RED_RESERVED_8BIT => "BGRx8",
            BIT_MASK => "BitMask",
            BLT_ONLY => "BltOnly",
            _ => "Undefined",
        }
    }
}

/// What each loader reservation holds, so the kernel does not have to guess
/// which of the regions it must keep its hands off.
pub mod reservation_kind {
    pub const LOADER_IMAGE: u32 = 1;
    pub const BOOTINFO: u32 = 2;
    pub const JUMP_STACK: u32 = 3;
    pub const MEMMAP_BUFFER: u32 = 4;
    pub const KERNEL_FILE_BUFFER: u32 = 5;
    pub const INITRD: u32 = 6;

    pub const fn name(k: u32) -> &'static str {
        match k {
            LOADER_IMAGE => "loader_image",
            BOOTINFO => "bootinfo",
            JUMP_STACK => "jump_stack",
            MEMMAP_BUFFER => "memmap_buffer",
            KERNEL_FILE_BUFFER => "kernel_file_buffer",
            INITRD => "initrd",
            _ => "unknown",
        }
    }
}

/// Why this boot happened. Filled in from NVRAM bookkeeping once A/B slots and
/// rollback exist (Phase I slice 3); until then it is always
/// [`boot_reason::COLD_BOOT`] and says so on the wire.
pub mod boot_reason {
    pub const COLD_BOOT: u32 = 1;
    pub const WARM_REBOOT: u32 = 2;
    pub const SLOT_FALLBACK: u32 = 3;
    pub const RECOVERY: u32 = 4;

    pub const fn name(r: u32) -> &'static str {
        match r {
            COLD_BOOT => "cold_boot",
            WARM_REBOOT => "warm_reboot",
            SLOT_FALLBACK => "slot_fallback",
            RECOVERY => "recovery",
            _ => "unknown",
        }
    }
}

/// Boot slot. `0` = A, `1` = B. Phase I milestone 1 has a single image, so this
/// is always 0 — the field exists now because a kernel that starts trusting a
/// slot number cannot be retrofitted with one.
pub const SLOT_A: u32 = 0;

/// One `PT_LOAD` segment of the kernel ELF, as loaded.
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

    pub const fn end(&self) -> u64 {
        self.paddr + self.memsz
    }
}

/// One entry of the UEFI memory map.
///
/// The UEFI descriptor is variable-length (`DescriptorSize` from
/// `GetMemoryMap`), so the loader normalises it into this fixed 32-byte form.
/// `uefi_attribs` is the firmware's own `Attribute` word, copied verbatim: it
/// carries the cacheability (WB/UC) that the kernel cannot recover once the
/// firmware map is gone.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemoryRegion {
    /// One of [`memory_type`].
    pub region_type: u32,
    pub _reserved: u32,
    /// First physical byte of the region.
    pub phys_start: u64,
    /// Length in bytes.
    pub phys_len: u64,
    /// UEFI `Attribute` bitmask, copied verbatim.
    pub uefi_attribs: u64,
}

impl MemoryRegion {
    pub const ZERO: MemoryRegion = MemoryRegion {
        region_type: 0,
        _reserved: 0,
        phys_start: 0,
        phys_len: 0,
        uefi_attribs: 0,
    };

    pub const fn end(&self) -> u64 {
        self.phys_start + self.phys_len
    }

    pub const fn pages(&self) -> u64 {
        self.phys_len / 4096
    }
}

/// The linear framebuffer the firmware's GOP left behind.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FramebufferInfo {
    /// Physical base address of the framebuffer. 0 when absent.
    pub phys: u64,
    pub width: u32,
    pub height: u32,
    /// Pixels per scan line. Not always equal to `width`.
    pub stride: u32,
    /// One of [`pixel_format`].
    pub pixel_format: u32,
    /// `FrameBufferSize` as the firmware reported it.
    pub size_bytes: u64,
}

impl FramebufferInfo {
    pub const ZERO: FramebufferInfo = FramebufferInfo {
        phys: 0,
        width: 0,
        height: 0,
        stride: 0,
        pixel_format: 0,
        size_bytes: 0,
    };
}

/// A region the loader occupies and the kernel must never hand out as a frame.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LoaderReserved {
    pub start: u64,
    pub len: u64,
    /// One of [`reservation_kind`].
    pub kind: u32,
    pub _reserved: u32,
}

impl LoaderReserved {
    pub const ZERO: LoaderReserved = LoaderReserved {
        start: 0,
        len: 0,
        kind: 0,
        _reserved: 0,
    };
}

/// The serial port the loader used, so the kernel can log before it has any
/// driver of its own.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DebugSerial {
    /// I/O port base, 0x3F8 for COM1.
    pub base_port: u16,
    /// 1 if the loader's probe of this port succeeded.
    pub present: u8,
    pub _reserved: u8,
    /// Bits per second as programmed.
    pub baud: u32,
}

impl DebugSerial {
    pub const ZERO: DebugSerial = DebugSerial {
        base_port: 0,
        present: 0,
        _reserved: 0,
        baud: 0,
    };
}

/// Everything the loader hands the kernel.
///
/// Field-by-field documentation lives in `BOOTINFO.md`; the short comments
/// here are the same contract in one line each.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootInfo {
    /// [`BOOTINFO_MAGIC`].
    pub magic: u64,
    /// [`ABI_VERSION`].
    pub abi_version: u32,
    /// `size_of::<BootInfo>()` as compiled by the *loader*.
    pub abi_size: u32,
    /// Bitmask of `FLAG_*`.
    pub flags: u32,
    /// CRC-32 (IEEE 802.3) over the whole structure with this field zeroed.
    pub checksum: u32,

    /// `RDTSC` sampled immediately before the jump.
    pub boot_tsc: u64,
    /// ELF `e_entry` of the kernel.
    pub kernel_entry: u64,
    /// Sum of `p_filesz` over all loaded segments.
    pub kernel_image_bytes: u64,

    /// Number of valid entries in `segments`.
    pub segment_count: u32,
    /// ELF `e_phnum` — how many program headers the kernel file had.
    pub kernel_phnum: u32,
    /// The kernel's `PT_LOAD` segments, in program-header order.
    pub segments: [SegmentInfo; MAX_SEGMENTS],

    /// Number of valid entries in `memory_regions`.
    pub memory_region_count: u32,
    /// [`MAX_MEMORY_REGIONS`].
    pub memory_region_capacity: u32,
    /// `DescriptorSize` reported by `GetMemoryMap` (40 on OVMF, may grow).
    pub map_desc_size: u32,
    /// `DescriptorVersion` reported by `GetMemoryMap`.
    pub map_desc_version: u32,
    /// The `MapKey` the successful `ExitBootServices` used.
    pub map_key: u64,
    /// The UEFI memory map as it stood at `ExitBootServices`.
    pub memory_regions: [MemoryRegion; MAX_MEMORY_REGIONS],

    /// Physical address of the ACPI RSDP (XSDT root), 0 if absent.
    pub rsdp_phys: u64,
    /// The RSDP's own 8-bit checksum sum; must be 0 for a valid table.
    pub rsdp_checksum: u8,
    /// 1 if `rsdp_phys` points at a table whose signature and checksum held.
    pub acpi_present: u8,
    /// RSDP `Revision`: 0 = ACPI 1.0 (20 bytes), 2 = ACPI 2.0+ (36 bytes).
    pub rsdp_revision: u8,
    pub _reserved_acpi: u8,

    /// The GOP framebuffer, or all zeroes with [`FLAG_FRAMEBUFFER_PRESENT`] clear.
    pub framebuffer: FramebufferInfo,

    /// Physical address of the initrd image, 0 if none was loaded.
    pub initrd_phys: u64,
    /// Length of the initrd image in bytes.
    pub initrd_len: u64,

    /// Lowest physical address occupied by the kernel image.
    pub kernel_phys_start: u64,
    /// Total span of the kernel image, from `kernel_phys_start` to the highest
    /// segment end. The kernel excludes this range from its own frame pool.
    pub kernel_phys_len: u64,

    /// Number of valid entries in `loader_reserved`.
    pub loader_reserved_count: u32,
    /// [`MAX_LOADER_RESERVED`].
    pub loader_reserved_capacity: u32,
    /// Regions the loader owns; never hand these out as frames.
    pub loader_reserved: [LoaderReserved; MAX_LOADER_RESERVED],

    /// Which A/B slot booted. [`SLOT_A`] until slice 3.
    pub boot_slot: u32,
    /// One of [`boot_reason`].
    pub boot_reason: u32,

    /// The port the loader logged to.
    pub debug_serial: DebugSerial,

    /// `EFI_SYSTEM_TABLE *`, for runtime services calls after handoff.
    pub system_table: u64,
}

impl BootInfo {
    /// A fully zeroed structure, used to reserve static storage in the loader.
    pub const ZERO: BootInfo = BootInfo {
        magic: 0,
        abi_version: 0,
        abi_size: 0,
        flags: 0,
        checksum: 0,
        boot_tsc: 0,
        kernel_entry: 0,
        kernel_image_bytes: 0,
        segment_count: 0,
        kernel_phnum: 0,
        segments: [SegmentInfo::ZERO; MAX_SEGMENTS],
        memory_region_count: 0,
        memory_region_capacity: 0,
        map_desc_size: 0,
        map_desc_version: 0,
        map_key: 0,
        memory_regions: [MemoryRegion::ZERO; MAX_MEMORY_REGIONS],
        rsdp_phys: 0,
        rsdp_checksum: 0,
        acpi_present: 0,
        rsdp_revision: 0,
        _reserved_acpi: 0,
        framebuffer: FramebufferInfo::ZERO,
        initrd_phys: 0,
        initrd_len: 0,
        kernel_phys_start: 0,
        kernel_phys_len: 0,
        loader_reserved_count: 0,
        loader_reserved_capacity: 0,
        loader_reserved: [LoaderReserved::ZERO; MAX_LOADER_RESERVED],
        boot_slot: 0,
        boot_reason: 0,
        debug_serial: DebugSerial::ZERO,
        system_table: 0,
    };

    /// Byte offset of the `checksum` field within the structure.
    ///
    /// Computed from the live object rather than hard-coded, so it stays
    /// correct if a field is ever inserted before it.
    fn checksum_offset(&self) -> usize {
        let base = self as *const BootInfo as usize;
        let field = core::ptr::addr_of!(self.checksum) as usize;
        field - base
    }

    /// CRC-32 over the structure with the checksum field treated as zero.
    ///
    /// `len` is a parameter rather than `size_of` so the caller can be explicit
    /// that it has already validated `abi_size` against its own idea of the
    /// structure — computing a checksum over an unvalidated length would read
    /// memory the kernel does not own.
    fn crc_with_field_zeroed(&self, len: usize) -> u32 {
        let base = self as *const BootInfo as *const u8;
        let off = self.checksum_offset();
        let mut crc = 0xFFFF_FFFFu32;
        // SAFETY: `self` points at a live `BootInfo` and the caller guarantees
        // `len <= size_of::<BootInfo>()`, so every byte read is inside it.
        let bytes = unsafe { core::slice::from_raw_parts(base, len) };
        for (i, byte) in bytes.iter().enumerate() {
            let b = if (off..off + 4).contains(&i) { 0 } else { *byte };
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    /// Stamp `abi_size` and `checksum`. Call this last, after every other
    /// field is final: any write after this point invalidates the checksum.
    pub fn seal(&mut self) {
        self.abi_size = core::mem::size_of::<BootInfo>() as u32;
        self.checksum = 0;
        let len = self.abi_size as usize;
        self.checksum = self.crc_with_field_zeroed(len);
    }

    /// Recompute the checksum and compare it with the one carried.
    ///
    /// Returns `(carried, computed)`. The caller must have already checked
    /// `abi_size == size_of::<BootInfo>()`.
    pub fn verify_checksum(&self) -> (u32, u32) {
        let len = core::mem::size_of::<BootInfo>();
        (self.checksum, self.crc_with_field_zeroed(len))
    }

    /// True if the magic and ABI version match what this build expects.
    pub fn is_compatible(&self) -> bool {
        self.magic == BOOTINFO_MAGIC && self.abi_version == ABI_VERSION
    }
}

/// The memory map did not fit in [`MAX_MEMORY_REGIONS`].
pub const FLAG_MEMMAP_CLAMPED: u32 = 1 << 0;
/// [`BootInfo::loader_reserved`] overflowed [`MAX_LOADER_RESERVED`].
pub const FLAG_LOADER_RESERVED_CLAMPED: u32 = 1 << 1;
/// `framebuffer` describes a real GOP mode.
pub const FLAG_FRAMEBUFFER_PRESENT: u32 = 1 << 2;
/// `rsdp_phys` points at a validated ACPI RSDP.
pub const FLAG_ACPI_PRESENT: u32 = 1 << 3;
/// `initrd_phys`/`initrd_len` describe a loaded initrd image.
pub const FLAG_INITRD_PRESENT: u32 = 1 << 4;
/// Test hook: the loader deliberately corrupted one byte after sealing, so the
/// kernel's checksum check has something to reject. Never set in a normal
/// build; `tests/bootinfo-checksum.sh` is the only thing that produces it.
pub const FLAG_TEST_CORRUPTED: u32 = 1 << 15;

/// Every flag bit this ABI defines. A kernel that sees a bit outside this mask
/// is talking to a loader it does not understand.
pub const FLAG_KNOWN_MASK: u32 = FLAG_MEMMAP_CLAMPED
    | FLAG_LOADER_RESERVED_CLAMPED
    | FLAG_FRAMEBUFFER_PRESENT
    | FLAG_ACPI_PRESENT
    | FLAG_INITRD_PRESENT
    | FLAG_TEST_CORRUPTED;
