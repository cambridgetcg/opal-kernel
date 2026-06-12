/* Opal linker script — QEMU aarch64 "virt" machine.
 *
 * On bare metal there is no loader deciding where code lives; this file is
 * that decision. The linker reads it and produces an ELF whose program
 * headers say "load these bytes at these physical addresses". QEMU's
 * -kernel option is a tiny ELF loader that obeys them.
 *
 * The memory map we are linking against (QEMU virt, guaranteed stable):
 *
 *   0x0000_0000  flash (128 MiB)
 *   0x0900_0000  PL011 UART (from the DTB; stable in practice, not contract)
 *   0x4000_0000  RAM base  <-- QEMU puts the DTB here for bare-metal ELF boot
 *   0x4020_0000  Opal      <-- us, leaving 2 MiB below for that DTB
 *
 * Why not link at exactly 0x4000_0000? QEMU only places the DTB at the
 * start of RAM if our lowest load address leaves room for it there.
 *
 * Why a whole 2 MiB of room? Hard-won detail: QEMU pre-allocates the virt
 * DTB in a 1 MiB buffer (FDT_MAX_SIZE) and checks *that* size against the
 * gap — if it doesn't fit, arm_load_dtb() silently returns 0 and you get
 * no DTB at all (we found out the empirical way; a 512 KiB gap boots fine
 * but wakes up devicetree-less). 2 MiB covers it with margin and matches
 * Linux's own text_offset alignment habits.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x40200000;

    /* Code. KEEP(.text.boot) pins the boot stub even under section GC,
     * and listing it first guarantees _start is the first byte of the
     * image — nice for sanity (entry point == load address). */
    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    /* Read-only data: string literals, const tables, format strings. */
    .rodata : ALIGN(8) {
        *(.rodata .rodata.*)
    }

    /* Initialized read-write data. */
    .data : ALIGN(8) {
        *(.data .data.*)
    }

    /* Zero-initialized data. NOLOAD: the ELF records only where it goes
     * and how big it is — no bytes in the file. Nobody zeroes memory for
     * us, so boot.rs clears [__bss_start, __bss_end) by hand before any
     * Rust code that might touch a static runs.
     *
     * Both symbols are 16-aligned so the clearing loop can use `stp`
     * (a 16-byte store pair) without edge cases. */
    .bss (NOLOAD) : ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    /* The boot stack: 64 KiB of reserved (NOLOAD) space. AArch64 stacks
     * grow downward, so the symbol we export is the *top*. The AAPCS64 ABI
     * requires SP to be 16-byte aligned at every function call — ALIGN(16)
     * plus a 64 KiB size keeps __stack_top compliant. */
    .stack (NOLOAD) : ALIGN(16) {
        __stack_bottom = .;
        . += 64K;
        __stack_top = .;
    }

    /* Sections we never want in a kernel image:
     *   .eh_frame*  unwind tables — we are panic=abort, nothing unwinds
     *   .comment    toolchain version strings
     *   .note*      build-id and friends */
    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr)
        *(.comment)
        *(.note .note.*)
    }
}

/* Static, non-relocatable code should need no Global Offset Table. If this
 * ever fires, some code is being compiled position-independent and we want
 * to know about it loudly, at link time. */
ASSERT(SIZEOF(.got) == 0, "unexpected GOT — kernel must be statically linked")
