/*
 * ARGUS kernel linker script — Phase I.
 *
 * The kernel is linked at a fixed physical address and runs on the identity
 * map the firmware already set up. That is a deliberate simplification for
 * this milestone: there are no page tables of our own yet, so virtual address
 * == physical address for every byte the kernel touches.
 *
 * KERNEL_BASE must land inside a region the UEFI memory map calls usable.
 * The loader verifies this before copying a single byte, and rejects the image
 * with E_VADDR_UNUSABLE if it does not hold.
 */

OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

KERNEL_BASE = 0x01000000;   /* 16 MiB */

SECTIONS
{
    . = KERNEL_BASE;

    .text : ALIGN(16)
    {
        KEEP(*(.text._start))
        *(.text .text.*)
    }

    .rodata : ALIGN(16)
    {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(16)
    {
        *(.data .data.*)
    }

    .bss : ALIGN(16)
    {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    /*
     * The kernel's own stack. Separate from .bss so zeroing .bss does not
     * touch it, and NOLOAD so it costs nothing in the file. 16 KiB is plenty
     * for a polled serial dump.
     */
    .stack (NOLOAD) : ALIGN(16)
    {
        __kernel_stack_bottom = .;
        . += 0x4000;
        __kernel_stack_top = .;
    }

    /DISCARD/ :
    {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
    }
}
