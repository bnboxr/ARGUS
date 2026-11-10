//! Hand-written UEFI bindings — only the pieces ARGUS Phase I actually uses.
//!
//! Why not the `uefi` crate: the whole point of Phase I is that the loader is
//! ours and readable end to end. This file is ~330 lines of declarations taken
//! straight from the UEFI 2.10 specification; the alternative was a dependency
//! tree we would have to audit before trusting it in the boot path. `deps.md`
//! therefore lists zero third-party crates.
//!
//! Conventions follow the specification exactly (`EFI_*` naming, `UINTN` is
//! `usize`, `EFI_STATUS` is `usize`), except that every struct field the
//! specification marks as a function pointer is typed as the real function
//! pointer type where we call it, and as `usize` where we do not. That keeps
//! the offsets right while avoiding ~40 unused function-pointer declarations.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use core::ffi::c_void;

pub type Handle = *mut c_void;
pub type Status = usize;
pub type Tpl = usize;
pub type PhysAddr = u64;

pub const EFI_SUCCESS: Status = 0;
pub const EFI_INVALID_PARAMETER: Status = 0x8000_0000_0000_0002;
pub const EFI_BUFFER_TOO_SMALL: Status = 0x8000_0000_0000_0005;
pub const EFI_NOT_FOUND: Status = 0x8000_0000_0000_000E;
pub const EFI_LOAD_ERROR: Status = 0x8000_0000_0000_0001;

pub fn is_error(status: Status) -> bool {
    status & (1usize << 63) != 0
}

/// `EFI_GUID`. Laid out exactly as the specification defines it: a `GUID` on
/// the wire, 16 bytes, little-endian in the first three fields.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub d1: u32,
    pub d2: u16,
    pub d3: u16,
    pub d4: [u8; 8],
}

impl Guid {
    pub const fn new(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Self {
        Guid { d1, d2, d3, d4 }
    }
}

/// `EFI_LOADED_IMAGE_PROTOCOL`.
pub const LOADED_IMAGE_PROTOCOL: Guid = Guid::new(
    0x5B1B31A1,
    0x9562,
    0x11d2,
    [0x8E, 0x3F, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B],
);

/// `EFI_SIMPLE_FILE_SYSTEM_PROTOCOL` — 964E5B22-6459-11D2-8E39-00A0C969723B.
///
/// This constant was wrong for two sessions (`0x9A` where the specification
/// has `0x39`), and that single byte is why `LocateHandleBuffer` reported zero
/// filesystem handles on a volume the firmware had just read our image from.
/// A GUID is an opaque 128-bit value: a typo in it does not fail loudly, it
/// just matches nothing. `tests/protocol-guid.sh` now checks every GUID in
/// this file against the specification text so it cannot happen silently
/// again.
pub const SIMPLE_FILE_SYSTEM_PROTOCOL: Guid = Guid::new(
    0x964E5B22,
    0x6459,
    0x11d2,
    [0x8E, 0x39, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B],
);

/// `EFI_FILE_INFO_ID`.
pub const FILE_INFO_GUID: Guid = Guid::new(
    0x09576E92,
    0x6D3F,
    0x11d2,
    [0x8E, 0x39, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B],
);

/// `EFI_TABLE_HEADER` — 24 bytes, at the front of every UEFI table.
#[repr(C)]
pub struct TableHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub crc32: u32,
    pub reserved: u32,
}

/// `EFI_SYSTEM_TABLE`. Only the fields we read are declared; the structure is
/// always reached through a firmware-owned pointer, so a shorter declaration
/// cannot corrupt anything as long as we only touch declared fields.
#[repr(C)]
pub struct SystemTable {
    pub hdr: TableHeader,
    pub firmware_vendor: *mut u16,
    pub firmware_revision: u32,
    pub console_in_handle: Handle,
    pub con_in: *mut c_void,
    pub console_out_handle: Handle,
    pub con_out: *mut c_void,
    pub standard_error_handle: Handle,
    pub std_err: *mut c_void,
    pub runtime_services: *mut c_void,
    pub boot_services: *mut BootServices,
    pub number_of_table_entries: usize,
    pub configuration_table: *mut c_void,
}

/// `EFI_BOOT_SERVICES`.
///
/// Field order is load-bearing: it mirrors the specification's member order
/// exactly, because the firmware's table is a flat array of pointers and we
/// read it through this struct. Every field between `create_event` and
/// `set_mem` that we do not call is declared as `usize` to hold the slot.
#[repr(C)]
pub struct BootServices {
    pub hdr: TableHeader, // 24 bytes
    pub raise_tpl: unsafe extern "efiapi" fn(Tpl) -> Tpl,
    pub restore_tpl: unsafe extern "efiapi" fn(Tpl),
    pub allocate_pages:
        unsafe extern "efiapi" fn(u32, u32, usize, *mut PhysAddr) -> Status,
    pub free_pages: unsafe extern "efiapi" fn(PhysAddr, usize) -> Status,
    pub get_memory_map: unsafe extern "efiapi" fn(
        *mut usize,
        *mut MemoryDescriptor,
        *mut usize,
        *mut usize,
        *mut u32,
    ) -> Status,
    pub allocate_pool: unsafe extern "efiapi" fn(u32, usize, *mut *mut u8) -> Status,
    pub free_pool: unsafe extern "efiapi" fn(*mut u8) -> Status,
    pub create_event: usize,
    pub set_timer: usize,
    pub wait_for_event: usize,
    pub signal_event: usize,
    pub close_event: usize,
    pub check_event: usize,
    pub install_protocol_interface: usize,
    pub reinstall_protocol_interface: usize,
    pub uninstall_protocol_interface: usize,
    pub handle_protocol:
        unsafe extern "efiapi" fn(Handle, *const Guid, *mut *mut c_void) -> Status,
    pub reserved: usize,
    pub register_protocol_notify: usize,
    pub locate_handle: usize,
    pub locate_device_path:
        unsafe extern "efiapi" fn(*const Guid, *mut *mut c_void, *mut Handle) -> Status,
    pub install_configuration_table: usize,
    pub load_image: usize,
    pub start_image: usize,
    pub exit: usize,
    pub unload_image: usize,
    pub exit_boot_services: unsafe extern "efiapi" fn(Handle, usize) -> Status,
    pub get_next_monotonic_count: usize,
    pub stall: unsafe extern "efiapi" fn(usize) -> Status,
    pub set_watchdog_timer:
        unsafe extern "efiapi" fn(usize, u64, usize, *const u16) -> Status,
    pub connect_controller:
        unsafe extern "efiapi" fn(Handle, *const Handle, *const c_void, u8) -> Status,
    pub disconnect_controller:
        unsafe extern "efiapi" fn(Handle, *const Handle, *const Handle) -> Status,
    pub open_protocol: usize,
    pub close_protocol: usize,
    pub open_protocol_information: usize,
    pub protocols_per_handle: usize,
    pub locate_handle_buffer:
        unsafe extern "efiapi" fn(u32, *const Guid, *mut c_void, *mut usize, *mut *mut Handle) -> Status,
    pub locate_protocol: usize,
    pub install_multiple_protocol_interfaces: usize,
    pub uninstall_multiple_protocol_interfaces: usize,
    pub calculate_crc32: usize,
    pub copy_mem: unsafe extern "efiapi" fn(*mut u8, *const u8, usize),
    pub set_mem: unsafe extern "efiapi" fn(*mut u8, usize, u8),
}

/// `EFI_MEMORY_DESCRIPTOR` — 40 bytes. `GetMemoryMap` may report a larger
/// `DescriptorSize`; callers must stride by that value, not by
/// `size_of::<MemoryDescriptor>()`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemoryDescriptor {
    pub region_type: u32,
    pub pad: u32,
    pub physical_start: u64,
    pub virtual_start: u64,
    pub number_of_pages: u64,
    pub attribute: u64,
}

/// `EFI_LOADED_IMAGE_PROTOCOL`. Only `device_handle` is used.
#[repr(C)]
pub struct LoadedImage {
    pub revision: u32,
    pub parent_handle: Handle,
    pub system_table: *mut SystemTable,
    pub device_handle: Handle,
    pub file_path: *mut c_void,
    pub reserved: *mut c_void,
    pub load_options_size: u32,
    pub load_options: *mut c_void,
    pub image_base: *mut c_void,
    pub image_size: u64,
    pub image_code_type: u32,
    pub image_data_type: u32,
    pub unload: usize,
}

/// `EFI_SIMPLE_FILE_SYSTEM_PROTOCOL`.
#[repr(C)]
pub struct SimpleFileSystem {
    pub revision: u64,
    pub open_volume: unsafe extern "efiapi" fn(*mut SimpleFileSystem, *mut *mut File) -> Status,
}

/// `EFI_FILE_PROTOCOL`. Truncated after `get_info` — we never call the rest,
/// and the protocol object is firmware-owned, so a short declaration is safe.
#[repr(C)]
pub struct File {
    pub revision: u64,
    pub open: unsafe extern "efiapi" fn(
        *mut File,
        *mut *mut File,
        *const u16,
        u64,
        u64,
    ) -> Status,
    pub close: unsafe extern "efiapi" fn(*mut File) -> Status,
    pub delete: usize,
    pub read: unsafe extern "efiapi" fn(*mut File, *mut usize, *mut u8) -> Status,
    pub write: unsafe extern "efiapi" fn(*mut File, *mut usize, *mut u8) -> Status,
    pub get_position: usize,
    pub set_position: unsafe extern "efiapi" fn(*mut File, u64) -> Status,
    pub get_info: unsafe extern "efiapi" fn(
        *mut File,
        *const Guid,
        *mut usize,
        *mut u8,
    ) -> Status,
}

/// `EFI_FILE_INFO`. `file_size` is at offset 8; we do not declare the
/// timestamp structures we never read.
#[repr(C)]
pub struct FileInfo {
    pub size: u64,
    pub file_size: u64,
    pub physical_size: u64,
    // EFI_TIME create/access/modify (16 bytes each) and Attribute follow; we
    // never read them, and the buffer is allocated by the firmware.
}


/// `EFI_DEVICE_PATH_PROTOCOL` (diagnostic only).
pub const DEVICE_PATH_PROTOCOL: Guid = Guid::new(
    0x09576E91,
    0x6D3F,
    0x11d2,
    [0x8E, 0x39, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B],
);

/// `EFI_FILE_MODE_READ`.
pub const EFI_FILE_MODE_READ: u64 = 0x0000_0000_0000_0001;
/// `EFI_FILE_MODE_WRITE` — required to write the anti-replay floor.
pub const EFI_FILE_MODE_WRITE: u64 = 0x0000_0000_0000_0002;
/// `EFI_FILE_MODE_CREATE` — create the file if it is not there yet.
pub const EFI_FILE_MODE_CREATE: u64 = 0x8000_0000_0000_0000;
/// `EFI_FILE_ATTRIBUTE_ARCHIVE`, used only when creating.
pub const EFI_FILE_ATTRIBUTE_ARCHIVE: u64 = 0x0000_0000_0000_0020;

/// `EFI_LOCATE_SEARCH_TYPE::ByProtocol`.
pub const EFI_LOCATE_BY_PROTOCOL: u32 = 2;

// Memory type constants used when allocating our own buffers.
pub const EFI_LOADER_DATA: u32 = 2;

/// `EFI_ALLOCATE_TYPE::AllocateAnyPages` — "anywhere you like".
pub const EFI_ALLOCATE_ANY_PAGES: u32 = 0;

// ---------------------------------------------------------------------------
// ACPI configuration tables.
//
// The firmware publishes the ACPI root (RSDP) through the system table's
// configuration table array rather than through a protocol, because ACPI is
// not a UEFI protocol. Two GUIDs exist: the ACPI 2.0+ one, which points at a
// full RSDP with an XSDT, and the legacy 1.0 one. We prefer the former and
// fall back to the latter, recording which we got in `rsdp_revision`.
// ---------------------------------------------------------------------------

/// `EFI_ACPI_TABLE_GUID` (ACPI 1.0 RSDP, `Revision` 0, 20-byte checksum).
pub const ACPI_TABLE_GUID: Guid = Guid::new(
    0xEB9D2D30,
    0x2D88,
    0x11D3,
    [0x9A, 0x16, 0x00, 0x90, 0x27, 0x3F, 0xC1, 0x4D],
);

/// `EFI_ACPI_20_TABLE_GUID` (ACPI 2.0+ RSDP, `Revision` >= 2, 36-byte checksum).
pub const ACPI_20_TABLE_GUID: Guid = Guid::new(
    0x8868E871,
    0xE4F1,
    0x11D3,
    [0xBC, 0x22, 0x00, 0x80, 0xC7, 0x3C, 0x88, 0x81],
);

/// `EFI_CONFIGURATION_TABLE` — one entry per configuration table GUID.
#[repr(C)]
pub struct ConfigurationTable {
    pub vendor_guid: Guid,
    pub vendor_table: *mut c_void,
}

/// `EFI_GRAPHICS_OUTPUT_PROTOCOL_GUID`.
pub const GRAPHICS_OUTPUT_PROTOCOL: Guid = Guid::new(
    0x9042A9DE,
    0x23DC,
    0x4A38,
    [0x96, 0xFB, 0x7A, 0xDE, 0xD0, 0x80, 0x51, 0x6A],
);

/// `EFI_GRAPHICS_OUTPUT_PROTOCOL_MODE`.
#[repr(C)]
pub struct GopMode {
    pub max_mode: u32,
    pub mode: u32,
    pub info: *mut GopModeInfo,
    pub size_of_info: usize,
    pub framebuffer_base: PhysAddr,
    pub framebuffer_size: usize,
}

/// `EFI_GRAPHICS_OUTPUT_MODE_INFORMATION`. Only the fields we report.
#[repr(C)]
pub struct GopModeInfo {
    pub version: u32,
    pub horizontal_resolution: u32,
    pub vertical_resolution: u32,
    /// `EFI_GRAPHICS_PIXEL_FORMAT`.
    pub pixel_format: u32,
    /// `EFI_PIXEL_BITMASK` (16 bytes), only meaningful for `BitMask`.
    pub pixel_bitmask: [u32; 4],
    pub pixels_per_scan_line: u32,
}

/// `EFI_GRAPHICS_OUTPUT_PROTOCOL.QueryMode`.
///
/// Allocates an `EFI_GRAPHICS_OUTPUT_MODE_INFORMATION` with `AllocatePool` and
/// hands ownership to the caller, who must `FreePool` it. Getting that wrong
/// leaks a mode-info block per mode queried, which at 128 MiB of guest RAM and a
/// few dozen modes is survivable but wrong; we free.
pub type GopQueryMode = unsafe extern "efiapi" fn(
    this: *mut c_void,
    mode_number: u32,
    size_of_info: *mut usize,
    info: *mut *mut GopModeInfo,
) -> Status;

/// `EFI_GRAPHICS_OUTPUT_PROTOCOL.SetMode`.
pub type GopSetMode =
    unsafe extern "efiapi" fn(this: *mut c_void, mode_number: u32) -> Status;

/// `EFI_GRAPHICS_OUTPUT_PROTOCOL`. `Blt` is declared as a `usize` slot because
/// we never call it — only the offsets matter for a field we do not touch.
#[repr(C)]
pub struct GraphicsOutput {
    pub query_mode: GopQueryMode,
    pub set_mode: GopSetMode,
    pub blt: usize,
    pub mode: *mut GopMode,
}
