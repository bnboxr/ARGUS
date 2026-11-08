//! ELF64 kernel image validation.
//!
//! This is the security-relevant half of the loader: everything a kernel file
//! claims is checked against the file's real length and against the firmware's
//! memory map before a single byte is copied. Each rejection has its own named
//! code so a failure on the wire says *which* check failed, not just "bad".
//!
//! The rules, in the order they are applied:
//!
//! 1. `e_ident` magic is `\x7fELF`
//! 2. `EI_CLASS` is `ELFCLASS64`
//! 3. `EI_DATA` is `ELFDATA2LSB`
//! 4. `e_machine` is `EM_X86_64` (62)
//! 5. `e_type` is `ET_EXEC` (2) — a relocatable object is not a kernel
//! 6. `e_phentsize` is 56 and `e_phnum` is in `1..=64`
//! 7. the whole program-header table lies inside the file
//! 8. each `PT_LOAD`: `p_filesz <= p_memsz`, and `p_offset + p_filesz` lies
//!    inside the file
//! 9. there is at least one `PT_LOAD` and at most `MAX_SEGMENTS` of them
//! 10. no two `PT_LOAD` segments overlap in memory
//! 11. every segment's load address and its `p_vaddr` lie wholly inside a
//!     single memory region the firmware described as usable
//! 12. `e_entry` lies inside an executable segment

use argus_shared::bootinfo::{memory_type, MemoryRegion, SegmentInfo, MAX_SEGMENTS};

const PT_LOAD: u32 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const MAX_PHDRS: u16 = 64;

/// `p_flags` bit 0 — the segment is executable.
pub const PF_X: u32 = 1;

/// Why a kernel image was rejected. Each variant maps to exactly one wire code.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// The file is shorter than an ELF64 header.
    HeaderTruncated,
    /// `e_ident[0..4]` is not `\x7fELF`.
    BadMagic,
    /// `e_ident[EI_CLASS]` is not 2 (ELFCLASS64).
    NotElf64,
    /// `e_ident[EI_DATA]` is not 1 (little-endian).
    NotLittleEndian,
    /// `e_machine` is not 62 (EM_X86_64).
    WrongMachine,
    /// `e_type` is not 2 (ET_EXEC).
    NotExecutable,
    /// `e_phentsize` is not 56.
    BadPhdrSize,
    /// `e_phnum` is 0.
    NoProgramHeaders,
    /// `e_phnum` exceeds the loader's limit of 64.
    TooManyProgramHeaders,
    /// The program-header table runs past the end of the file.
    PhdrTableTruncated,
    /// A `PT_LOAD` segment's file contents run past the end of the file.
    SegmentTruncated,
    /// `p_filesz > p_memsz`.
    SegmentSizeInvalid,
    /// No `PT_LOAD` segments at all.
    NoLoadSegments,
    /// More `PT_LOAD` segments than `MAX_SEGMENTS`.
    TooManySegments,
    /// Two `PT_LOAD` segments claim overlapping memory.
    SegmentsOverlap,
    /// A segment's address range is not inside a usable memory region.
    SegmentOutsideUsableMemory,
    /// `e_entry` is not inside any executable `PT_LOAD` segment.
    EntryOutsideText,
    /// A segment would overwrite the running loader image.
    OverlapsLoader,
}

impl ElfError {
    /// The stable, greppable code printed on the serial console and asserted
    /// by `tests/negative-tests.sh`. These strings are an interface: do not
    /// rename one without updating the tests.
    pub const fn code(self) -> &'static str {
        match self {
            ElfError::HeaderTruncated => "E_HEADER_TRUNCATED",
            ElfError::BadMagic => "E_MAGIC",
            ElfError::NotElf64 => "E_CLASS",
            ElfError::NotLittleEndian => "E_ENDIAN",
            ElfError::WrongMachine => "E_MACHINE",
            ElfError::NotExecutable => "E_TYPE",
            ElfError::BadPhdrSize => "E_PHENTSIZE",
            ElfError::NoProgramHeaders => "E_NO_PHDRS",
            ElfError::TooManyProgramHeaders => "E_TOO_MANY_PHDRS",
            ElfError::PhdrTableTruncated => "E_PHDR_TRUNCATED",
            ElfError::SegmentTruncated => "E_SEGMENT_TRUNCATED",
            ElfError::SegmentSizeInvalid => "E_SEGMENT_SIZE",
            ElfError::NoLoadSegments => "E_NO_LOAD_SEGMENTS",
            ElfError::TooManySegments => "E_TOO_MANY_SEGMENTS",
            ElfError::SegmentsOverlap => "E_SEGMENTS_OVERLAP",
            ElfError::SegmentOutsideUsableMemory => "E_VADDR_UNUSABLE",
            ElfError::EntryOutsideText => "E_ENTRY_NOT_IN_TEXT",
            ElfError::OverlapsLoader => "E_OVERLAPS_LOADER",
        }
    }

    /// One sentence a human can act on, printed next to the code.
    pub const fn detail(self) -> &'static str {
        match self {
            ElfError::HeaderTruncated => "file is smaller than an ELF64 header (64 bytes)",
            ElfError::BadMagic => "e_ident[0..4] is not \\x7fELF",
            ElfError::NotElf64 => "e_ident[EI_CLASS] is not ELFCLASS64 (2)",
            ElfError::NotLittleEndian => "e_ident[EI_DATA] is not ELFDATA2LSB (1)",
            ElfError::WrongMachine => "e_machine is not EM_X86_64 (62)",
            ElfError::NotExecutable => "e_type is not ET_EXEC (2)",
            ElfError::BadPhdrSize => "e_phentsize is not 56",
            ElfError::NoProgramHeaders => "e_phnum is 0",
            ElfError::TooManyProgramHeaders => "e_phnum exceeds the loader limit of 64",
            ElfError::PhdrTableTruncated => "program header table runs past end of file",
            ElfError::SegmentTruncated => "PT_LOAD file range runs past end of file",
            ElfError::SegmentSizeInvalid => "PT_LOAD p_filesz is greater than p_memsz",
            ElfError::NoLoadSegments => "no PT_LOAD segments",
            ElfError::TooManySegments => "more PT_LOAD segments than the loader accepts",
            ElfError::SegmentsOverlap => "two PT_LOAD segments overlap in memory",
            ElfError::SegmentOutsideUsableMemory => {
                "PT_LOAD address range is not inside a usable memory region"
            }
            ElfError::EntryOutsideText => "e_entry is not inside an executable PT_LOAD segment",
            ElfError::OverlapsLoader => "PT_LOAD destination overlaps the running loader image",
        }
    }
}

/// A validated, ready-to-load kernel image.
#[derive(Clone, Copy)]
pub struct KernelImage {
    pub entry: u64,
    /// Destination of each segment, in the form the kernel will see it.
    pub segments: [SegmentInfo; MAX_SEGMENTS],
    /// File offset of each segment's contents. Kept here rather than in
    /// `SegmentInfo` because the kernel has no use for it — the loader does
    /// the copying and then this information is dead.
    pub file_offsets: [u64; MAX_SEGMENTS],
    pub segment_count: usize,
    /// Sum of `p_filesz` — bytes actually copied.
    pub image_bytes: u64,
}

// ---------------------------------------------------------------------------
// Bounds-checked little-endian readers. Every one returns `None` instead of
// panicking, so a hostile file can only ever produce an error code.
// ---------------------------------------------------------------------------

fn u16_at(buf: &[u8], off: usize) -> Option<u16> {
    let b = buf.get(off..off + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(buf: &[u8], off: usize) -> Option<u32> {
    let b = buf.get(off..off + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(buf: &[u8], off: usize) -> Option<u64> {
    let b = buf.get(off..off + 8)?;
    Some(u64::from_le_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

/// One decoded `Elf64_Phdr`.
#[derive(Clone, Copy)]
struct Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
}

impl Phdr {
    /// Where the loader will actually put this segment. `p_paddr` when the
    /// linker set it, otherwise the virtual address — for our flat identity
    /// mapping the two are equal, but honouring `p_paddr` is what lets a
    /// future kernel link above the identity map.
    fn load_address(&self) -> u64 {
        if self.p_paddr != 0 {
            self.p_paddr
        } else {
            self.p_vaddr
        }
    }
}

/// True if `[start, start + len)` lies wholly inside one usable region.
fn range_is_usable(regions: &[MemoryRegion], start: u64, len: u64) -> bool {
    let Some(end) = start.checked_add(len) else {
        return false;
    };
    if len == 0 {
        return true;
    }
    regions.iter().any(|r| {
        memory_type::is_usable_for_kernel(r.region_type)
            && start >= r.physical_start
            && end <= r.end()
    })
}

/// Validate `file` against the firmware memory map.
///
/// `usable` is the loader's flattened memory map. `loader_range` is
/// `(base, size)` of the running loader image: a kernel that would be copied
/// on top of the code doing the copying is rejected.
///
/// Returns the validated image or the first check that failed — validation
/// stops at the first failure so the error code is unambiguous.
pub fn validate(
    file: &[u8],
    usable: &[MemoryRegion],
    loader_range: (u64, u64),
) -> Result<KernelImage, ElfError> {
    if file.len() < EHDR_SIZE {
        return Err(ElfError::HeaderTruncated);
    }

    // 1-3: e_ident.
    if &file[0..4] != b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    if file[4] != ELFCLASS64 {
        return Err(ElfError::NotElf64);
    }
    if file[5] != ELFDATA2LSB {
        return Err(ElfError::NotLittleEndian);
    }

    // 4-5: type and machine.
    let e_type = u16_at(file, 16).ok_or(ElfError::HeaderTruncated)?;
    if e_type != ET_EXEC {
        return Err(ElfError::NotExecutable);
    }
    let e_machine = u16_at(file, 18).ok_or(ElfError::HeaderTruncated)?;
    if e_machine != EM_X86_64 {
        return Err(ElfError::WrongMachine);
    }

    let e_entry = u64_at(file, 24).ok_or(ElfError::HeaderTruncated)?;
    let e_phoff = u64_at(file, 32).ok_or(ElfError::HeaderTruncated)?;
    let e_phentsize = u16_at(file, 54).ok_or(ElfError::HeaderTruncated)?;
    let e_phnum = u16_at(file, 56).ok_or(ElfError::HeaderTruncated)?;

    // 6: program header geometry.
    if e_phentsize as usize != PHDR_SIZE {
        return Err(ElfError::BadPhdrSize);
    }
    if e_phnum == 0 {
        return Err(ElfError::NoProgramHeaders);
    }
    if e_phnum > MAX_PHDRS {
        return Err(ElfError::TooManyProgramHeaders);
    }

    // 7: the whole table must be inside the file. This is the check a
    // truncated download or a half-written file hits.
    let table_len = (e_phnum as u64) * (PHDR_SIZE as u64);
    let table_end = e_phoff.checked_add(table_len).ok_or(ElfError::PhdrTableTruncated)?;
    if table_end > file.len() as u64 {
        return Err(ElfError::PhdrTableTruncated);
    }

    // Decode every PT_LOAD.
    let mut segments = [SegmentInfo::ZERO; MAX_SEGMENTS];
    let mut file_offsets = [0u64; MAX_SEGMENTS];
    let mut count = 0usize;
    let mut image_bytes = 0u64;

    for i in 0..e_phnum as usize {
        let base = e_phoff as usize + i * PHDR_SIZE;
        let ph = Phdr {
            p_type: u32_at(file, base).ok_or(ElfError::HeaderTruncated)?,
            p_flags: u32_at(file, base + 4).ok_or(ElfError::HeaderTruncated)?,
            p_offset: u64_at(file, base + 8).ok_or(ElfError::HeaderTruncated)?,
            p_vaddr: u64_at(file, base + 16).ok_or(ElfError::HeaderTruncated)?,
            p_paddr: u64_at(file, base + 24).ok_or(ElfError::HeaderTruncated)?,
            p_filesz: u64_at(file, base + 32).ok_or(ElfError::HeaderTruncated)?,
            p_memsz: u64_at(file, base + 40).ok_or(ElfError::HeaderTruncated)?,
        };

        if ph.p_type != PT_LOAD {
            continue;
        }

        // 8: file bounds and size sanity.
        if ph.p_filesz > ph.p_memsz {
            return Err(ElfError::SegmentSizeInvalid);
        }
        let seg_end = ph
            .p_offset
            .checked_add(ph.p_filesz)
            .ok_or(ElfError::SegmentTruncated)?;
        if seg_end > file.len() as u64 {
            return Err(ElfError::SegmentTruncated);
        }

        if count >= MAX_SEGMENTS {
            return Err(ElfError::TooManySegments);
        }
        segments[count] = SegmentInfo {
            vaddr: ph.p_vaddr,
            paddr: ph.load_address(),
            filesz: ph.p_filesz,
            memsz: ph.p_memsz,
            flags: ph.p_flags,
            _reserved: 0,
        };
        file_offsets[count] = ph.p_offset;
        count += 1;
        image_bytes += ph.p_filesz;
    }

    // 9: at least one loadable segment.
    if count == 0 {
        return Err(ElfError::NoLoadSegments);
    }

    // 10: no overlaps. O(n^2) over at most 8 segments.
    for i in 0..count {
        let a_start = segments[i].vaddr;
        let a_end = a_start.saturating_add(segments[i].memsz);
        for j in (i + 1)..count {
            let b_start = segments[j].vaddr;
            let b_end = b_start.saturating_add(segments[j].memsz);
            if a_start < b_end && b_start < a_end {
                return Err(ElfError::SegmentsOverlap);
            }
        }
    }

    // 11: every segment must land in memory the firmware called usable.
    // Both the load address (where the bytes go) and `p_vaddr` (what the
    // kernel believes) are checked; a kernel linked somewhere the memory map
    // does not cover would fault on the first instruction.
    for seg in segments.iter().take(count) {
        if !range_is_usable(usable, seg.paddr, seg.memsz) {
            return Err(ElfError::SegmentOutsideUsableMemory);
        }
        if !range_is_usable(usable, seg.vaddr, seg.memsz) {
            return Err(ElfError::SegmentOutsideUsableMemory);
        }
    }

    // 12: the entry point must be executable code we are about to load.
    let entry_ok = segments.iter().take(count).any(|s| {
        s.flags & PF_X != 0 && e_entry >= s.vaddr && e_entry < s.vaddr.saturating_add(s.memsz)
    });
    if !entry_ok {
        return Err(ElfError::EntryOutsideText);
    }

    // 13: the kernel must not be copied on top of the loader that is copying
    // it. Checked on the load address, which is what the copy actually writes.
    let (loader_base, loader_size) = loader_range;
    if loader_size > 0 {
        let loader_end = loader_base.saturating_add(loader_size);
        for seg in segments.iter().take(count) {
            let seg_end = seg.paddr.saturating_add(seg.memsz);
            if seg.paddr < loader_end && loader_base < seg_end {
                return Err(ElfError::OverlapsLoader);
            }
        }
    }

    Ok(KernelImage {
        entry: e_entry,
        segments,
        file_offsets,
        segment_count: count,
        image_bytes,
    })
}
