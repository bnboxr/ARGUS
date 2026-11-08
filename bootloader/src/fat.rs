//! Minimal read-only FAT16 reader over `EFI_BLOCK_IO_PROTOCOL`.
//!
//! Why this exists: the firmware is not obliged to leave a
//! `EFI_SIMPLE_FILE_SYSTEM_PROTOCOL` bound to the volume we were booted from,
//! and on the OVMF build used here it does not — measured, zero filesystem
//! handles among 238, even after disconnecting and reconnecting the
//! controller. Rather than depend on a driver being present, the loader reads
//! the sectors itself. That is also the right shape for a system that intends
//! to own its own storage stack.
//!
//! Scope: FAT16 only, read only, 8.3 names only, no long names, no FAT12/32,
//! no fragmented-directory support beyond a single root directory. That is
//! exactly enough to find `\EFI\BOOT\ARGUSKRN.ELF` on an ESP we built
//! ourselves, and it says so instead of pretending to be a general FAT driver.

use crate::efi::*;
use core::ffi::c_void;
use core::ptr::null_mut;

/// `EFI_BLOCK_IO_PROTOCOL`.
pub const BLOCK_IO_PROTOCOL: Guid = Guid::new(
    0x964E5B21,
    0x6459,
    0x11d2,
    [0x8E, 0x9A, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B],
);

#[repr(C)]
pub struct BlockIoMedia {
    pub media_id: u32,
    pub removable_media: u8,
    pub media_present: u8,
    pub logical_partition: u8,
    pub read_only: u8,
    pub write_caching: u8,
    pub block_size: u32,
    pub io_align: u32,
    pub last_block: u64,
}

#[repr(C)]
pub struct BlockIo {
    pub revision: u64,
    pub media: *mut BlockIoMedia,
    pub reset: unsafe extern "efiapi" fn(*mut BlockIo, u8) -> Status,
    pub read_blocks: unsafe extern "efiapi" fn(*mut BlockIo, u32, u64, usize, *mut u8) -> Status,
    pub write_blocks: usize,
    pub flush_blocks: usize,
}

/// Why a FAT read failed. Each variant is reported verbatim on the wire.
#[derive(Clone, Copy)]
pub enum FatError {
    NoBlockIo,
    NotFat,
    BadBpb,
    FileNotFound,
    ReadFailed,
    TooLarge,
    CorruptChain,
}

impl FatError {
    pub const fn code(self) -> &'static str {
        match self {
            FatError::NoBlockIo => "no_block_io_device",
            FatError::NotFat => "no_fat16_volume_found",
            FatError::BadBpb => "bad_fat_bpb",
            FatError::FileNotFound => "kernel_file_not_found",
            FatError::ReadFailed => "block_read_failed",
            FatError::TooLarge => "kernel_file_too_large",
            FatError::CorruptChain => "corrupt_fat_chain",
        }
    }
}

/// A FAT16 volume found on some block device.
#[derive(Clone, Copy)]
pub struct Volume {
    bio: *mut BlockIo,
    /// LBA where the FAT filesystem starts (partition offset, in sectors).
    base_lba: u64,
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    reserved_sectors: u32,
    num_fats: u32,
    fat_sectors: u32,
    root_entries: u32,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl Volume {
    fn first_data_sector(&self) -> u64 {
        let root_dir_sectors =
            (self.root_entries as u64 * 32 + self.bytes_per_sector as u64 - 1) / self.bytes_per_sector as u64;
        self.base_lba
            + self.reserved_sectors as u64
            + self.num_fats as u64 * self.fat_sectors as u64
            + root_dir_sectors
    }

    fn read_sectors(&self, lba: u64, count: usize, out: *mut u8) -> Result<(), FatError> {
        // SAFETY: `self.bio` is a live EFI_BLOCK_IO_PROTOCOL we obtained from
        // the firmware; `out` points at `count * block_size` writable bytes,
        // which every caller allocates. MediaId 0 is the "ignore" value.
        let s = unsafe { ((*self.bio).read_blocks)(self.bio, 0, lba, count, out) };
        if is_error(s) {
            Err(FatError::ReadFailed)
        } else {
            Ok(())
        }
    }
}

/// Try to interpret the block device as an MBR-partitioned FAT16 volume.
fn probe(bio: *mut BlockIo) -> Option<Volume> {
    // SAFETY: `bio` came from the firmware's protocol database.
    let media = unsafe { &*(*bio).media };
    if media.block_size == 0 || media.block_size > 4096 {
        return None;
    }
    let bps = media.block_size;
    let mut sector = [0u8; 4096];
    let count = 1usize;

    let read = |lba: u64, buf: *mut u8| -> bool {
        // SAFETY: `bio` is live and `buf` has at least `bps` writable bytes
        // (callers pass `sector`, which is 4096 bytes and bps <= 4096).
        !is_error(unsafe { ((*bio).read_blocks)(bio, 0, lba, count, buf) })
    };

    if !read(0, sector.as_mut_ptr()) {
        return None;
    }

    // Decide where the filesystem starts. Either sector 0 is an MBR with a
    // FAT partition entry, or sector 0 *is* the FAT boot sector.
    let mut base_lba = 0u64;
    let mbr_sig = sector[510] == 0x55 && sector[511] == 0xAA;
    let has_bpb_at_zero = mbr_sig && (&sector[54..58] == b"FAT1" || &sector[82..86] == b"FAT1");

    if mbr_sig && !has_bpb_at_zero {
        // Look at the four partition entries for a FAT type byte.
        for i in 0..4 {
            let e = 446 + i * 16;
            let ptype = sector[e + 4];
            let is_fat = matches!(ptype, 0x01 | 0x04 | 0x06 | 0x0B | 0x0C | 0x0E);
            if is_fat {
                base_lba = u32_at(&sector, e + 8) as u64;
                break;
            }
        }
        if base_lba == 0 {
            return None;
        }
        if !read(base_lba, sector.as_mut_ptr()) {
            return None;
        }
    }

    // Now `sector` holds the FAT boot sector.
    if !(sector[510] == 0x55 && sector[511] == 0xAA) {
        return None;
    }
    let bytes_per_sector = u16_at(&sector, 11) as u32;
    let sectors_per_cluster = sector[13] as u32;
    let reserved_sectors = u16_at(&sector, 14) as u32;
    let num_fats = sector[16] as u32;
    let root_entries = u16_at(&sector, 17) as u32;
    let fat_sectors = u16_at(&sector, 22) as u32; // FAT16 only

    if bytes_per_sector != bps
        || !(1..=128).contains(&sectors_per_cluster)
        || reserved_sectors == 0
        || num_fats == 0
        || fat_sectors == 0
        || root_entries == 0
    {
        return None;
    }

    Some(Volume {
        bio,
        base_lba,
        bytes_per_sector,
        sectors_per_cluster,
        reserved_sectors,
        num_fats,
        fat_sectors,
        root_entries,
    })
}

/// Find every FAT16 volume the firmware's block devices expose.
pub fn find_volumes(bs: &BootServices, out: &mut [Option<Volume>]) -> usize {
    let mut count = 0usize;
    let mut buf: *mut Handle = null_mut();
    let mut n_handles = 0usize;
    // SAFETY: `locate_handle_buffer` allocates the handle array with pool
    // memory and reports its length; we free it with the matching `free_pool`.
    let s = unsafe {
        (bs.locate_handle_buffer)(
            EFI_LOCATE_BY_PROTOCOL,
            &BLOCK_IO_PROTOCOL,
            null_mut(),
            &mut n_handles,
            &mut buf,
        )
    };
    if is_error(s) || buf.is_null() {
        return 0;
    }
    for i in 0..n_handles {
        if count >= out.len() {
            break;
        }
        // SAFETY: the firmware returned `n_handles` handles in `buf`.
        let h = unsafe { *buf.add(i) };
        let mut p: *mut c_void = null_mut();
        // SAFETY: probing a firmware handle for BlockIo.
        let st = unsafe { (bs.handle_protocol)(h, &BLOCK_IO_PROTOCOL, &mut p as *mut *mut c_void) };
        if is_error(st) || p.is_null() {
            continue;
        }
        if let Some(v) = probe(p as *mut BlockIo) {
            out[count] = Some(v);
            count += 1;
        }
    }
    // SAFETY: allocated by `locate_handle_buffer`, freed exactly once.
    unsafe { (bs.free_pool)(buf as *mut u8) };
    count
}

/// Read `\EFI\BOOT\<NAME>` (8.3, uppercase, no extension dot needed) from the
/// volume into freshly allocated pool memory.
///
/// Returns the buffer and its exact length.
pub fn read_root_file(
    bs: &BootServices,
    vol: &Volume,
    name83: &[u8; 11],
) -> Result<(*mut u8, usize), FatError> {
    let bps = vol.bytes_per_sector as usize;
    let cluster_bytes = bps * vol.sectors_per_cluster as usize;

    // --- root directory ---
    let root_dir_sectors = (vol.root_entries as usize * 32 + bps - 1) / bps;
    let root_lba = vol.base_lba
        + vol.reserved_sectors as u64
        + vol.num_fats as u64 * vol.fat_sectors as u64;

    let mut dir = [0u8; 4096];
    if root_dir_sectors * bps > dir.len() {
        return Err(FatError::BadBpb);
    }
    vol.read_sectors(root_lba, root_dir_sectors, dir.as_mut_ptr())?;

    // --- find the file ---
    let mut first_cluster = 0u32;
    let mut file_size = 0u32;
    for i in 0..(root_dir_sectors * bps / 32) {
        let e = i * 32;
        if dir[e] == 0x00 {
            break; // end of directory
        }
        if dir[e] == 0xE5 || dir[e + 11] & 0x08 != 0 {
            continue; // deleted, or a volume label
        }
        if &dir[e..e + 11] == name83 {
            first_cluster = u16_at(&dir, e + 26) as u32;
            file_size = u32_at(&dir, e + 28);
            break;
        }
    }
    if first_cluster == 0 {
        return Err(FatError::FileNotFound);
    }
    if file_size == 0 || file_size > 8 * 1024 * 1024 {
        return Err(FatError::TooLarge);
    }

    // --- allocate the destination ---
    let mut out: *mut u8 = null_mut();
    // SAFETY: allocating `file_size` bytes of pool memory for the file.
    let s = unsafe {
        (bs.allocate_pool)(EFI_LOADER_DATA, file_size as usize, &mut out as *mut *mut u8)
    };
    if is_error(s) || out.is_null() {
        return Err(FatError::ReadFailed);
    }

    // --- walk the FAT chain, copying clusters ---
    let fat_lba = vol.base_lba + vol.reserved_sectors as u64;
    let mut fat_sector = [0u8; 4096];
    let mut cluster = first_cluster;
    let mut written = 0usize;
    let mut cluster_buf: *mut u8 = null_mut();
    // SAFETY: allocating one cluster of scratch space.
    let s = unsafe {
        (bs.allocate_pool)(EFI_LOADER_DATA, cluster_bytes, &mut cluster_buf as *mut *mut u8)
    };
    if is_error(s) || cluster_buf.is_null() {
        return Err(FatError::ReadFailed);
    }

    let clusters_needed = (file_size as usize + cluster_bytes - 1) / cluster_bytes;
    for _ in 0..clusters_needed {
        if cluster < 2 {
            return Err(FatError::CorruptChain);
        }
        let lba = vol.first_data_sector() + (cluster as u64 - 2) * vol.sectors_per_cluster as u64;
        vol.read_sectors(lba, vol.sectors_per_cluster as usize, cluster_buf)?;

        let remaining = file_size as usize - written;
        let take = remaining.min(cluster_bytes);
        // SAFETY: `cluster_buf` holds `cluster_bytes` valid bytes and `out`
        // holds `file_size` bytes of which `written` are already filled, so
        // `take <= remaining` stays inside both buffers.
        unsafe {
            core::ptr::copy_nonoverlapping(cluster_buf, out.add(written), take);
        }
        written += take;
        if written >= file_size as usize {
            break;
        }

        // Next cluster: FAT16 entry = 2 bytes at cluster*2.
        let fat_offset = cluster as usize * 2;
        let sector_in_fat = fat_offset / bps;
        let off = fat_offset % bps;
        vol.read_sectors(fat_lba + sector_in_fat as u64, 1, fat_sector.as_mut_ptr())?;
        cluster = u16_at(&fat_sector, off) as u32;
    }

    // SAFETY: both buffers were allocated by us from pool memory above.
    unsafe {
        (bs.free_pool)(cluster_buf);
    }

    if written != file_size as usize {
        return Err(FatError::CorruptChain);
    }
    Ok((out, written))
}
