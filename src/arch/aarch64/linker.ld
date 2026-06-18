/* Opal linker script — QEMU aarch64 "virt" machine.
 *
 * On bare metal there is no loader deciding where code lives; this file is
 * that decision. Since M2 it answers two questions per section instead of
 * one: where the bytes LOAD (the LMA — a physical address, what QEMU's
 * -kernel loader obeys), and where the code believes it LIVES (the VMA —
 * the addresses baked into every adrp, every literal, every symbol). For
 * M0 and M1 the two were equal everywhere. Now only the boot stub keeps
 * that property; everything else loads on the ground floor and lives
 * upstairs, at KERNEL_BASE + its physical address, and the MMU (mmu.rs)
 * is what makes "upstairs" real before the first high instruction runs.
 *
 * The physical map we are loading against (QEMU virt, guaranteed stable):
 *
 *   0x0000_0000  flash (128 MiB)
 *   0x0900_0000  PL011 UART (from the DTB; stable in practice, not contract)
 *   0x4000_0000  RAM base  <-- QEMU puts the DTB here for bare-metal ELF boot
 *   0x4020_0000  Opal      <-- us, leaving 2 MiB below for that DTB
 *
 * Why not load at exactly 0x4000_0000? QEMU only places the DTB at the
 * start of RAM if our lowest load address leaves room for it there.
 *
 * Why a whole 2 MiB of room? Hard-won detail: QEMU pre-allocates the virt
 * DTB in a 1 MiB buffer (FDT_MAX_SIZE) and checks *that* size against the
 * gap — if it doesn't fit, arm_load_dtb() silently returns 0 and you get
 * no DTB at all (we found out the empirical way; a 512 KiB gap boots fine
 * but wakes up devicetree-less). 2 MiB covers it with margin and matches
 * Linux's own text_offset alignment habits. mmu.rs maps that window
 * read-only and calls the offset by name; change the two together.
 */

ENTRY(_start)

/* Where the kernel lives, virtually. Must match mmu::KERNEL_BASE (the
 * Rust side const-asserts its copy against T1SZ; boot.rs cross-checks a
 * third copy against __kernel_va_base at run time) — and the ASSERTs
 * below pin its alignment. */
KERNEL_BASE = 0xFFFF000000000000;

/* The boot stub turns high symbols into physical addresses by subtracting
 * this — exported so boot.rs can cross-check its hardcoded copy against
 * the linker's truth at run time (two independent materializations must
 * agree, or we park before touching anything). */
__kernel_va_base = KERNEL_BASE;

SECTIONS
{
    . = 0x40200000;

    /* The boot stub: the ONLY code whose VMA equals its LMA. QEMU sets
     * PC to the raw ELF entry point with the MMU off, so _start must be
     * fetchable at its physical address — and _start stays the first
     * byte of the image (entry point == load address, asserted below).
     * Everything here runs displaced-free; the literal pools (.ltorg)
     * that boot.rs uses to name high symbols also land in this section.
     *
     * KEEP: the boot stub must survive section GC even though nothing
     * references it by name. */
    .text.boot : {
        KEEP(*(.text.boot))
    }
    __boot_end_pa = .;

    /* Everything below this line lives upstairs: VMA = KERNEL_BASE + PA.
     * The AT() on each section keeps the LMAs physically contiguous after
     * the boot stub, so the loaded image is one dense strip of RAM
     * regardless of where the sections believe they live. (lld grammar
     * note: AT() must come before ALIGN() in an output-section header.) */
    . = KERNEL_BASE + __boot_end_pa;

    /* Code. The exception vector table leads: VBAR_EL1 ignores its low
     * 11 bits, so the table must sit on a 2048-byte boundary. The .balign
     * in vectors.rs requests it; the ASSERT below verifies the linker
     * delivered. With the boot stub capped at 0x400 bytes (also asserted)
     * the table lands at offset 0x800 exactly — same as M1, one floor up.
     *
     * The trailing ALIGN(16K): .text ends on a granule edge, because the
     * next section changes permissions (RX -> RO) and the MMU can only
     * draw lines between 16 KiB pages — see mmu.rs. */
    .text : AT(ADDR(.text) - KERNEL_BASE) {
        KEEP(*(.text.vectors))
        *(.text .text.*)
        . = ALIGN(16K);
    }
    __text_end = .;

    /* Read-only data: string literals, const tables, format strings.
     * Ends on a granule edge: RO -> RW boundary. */
    .rodata : AT(ADDR(.rodata) - KERNEL_BASE) {
        *(.rodata .rodata.*)
        . = ALIGN(16K);
    }
    __rodata_end = .;

    /* Initialized read-write data. */
    .data : AT(ADDR(.data) - KERNEL_BASE) ALIGN(8) {
        *(.data .data.*)
    }

    /* Zero-initialized data. NOLOAD: the ELF records only where it goes
     * and how big it is — no bytes in the file. Nobody zeroes memory for
     * us, so boot.rs clears [__bss_start, __bss_end) by hand before any
     * Rust code that might touch a static runs. The page tables (mmu.rs,
     * 96 KiB, 16 KiB-aligned via repr) live here; the boot stub's zeroing
     * is what makes their every unwritten descriptor INVALID.
     *
     * Both symbols are 16-aligned so the clearing loop can use `stp`
     * (a 16-byte store pair) without edge cases. */
    .bss (NOLOAD) : AT(ADDR(.bss) - KERNEL_BASE) ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    /* The boot stack — and, since M2, the guard below it. Both 16 KiB-
     * aligned and NOLOAD (reserved addresses, no file bytes; the guard
     * costs the ELF nothing).
     *
     * The guard is not a section with special contents — it is 16 KiB of
     * address space whose page-table entry mmu.rs deliberately never
     * writes. A stack overflow's first touch below __stack_bottom now
     * takes a translation fault with a report, where M0/M1 would have
     * silently eaten the top of .bss (a debt docs/01 has carried since
     * M0; paid here).
     *
     * AArch64 stacks grow downward, so the symbol the boot stub loads is
     * the *top*. AAPCS64 demands SP stay 16-byte aligned; 16 KiB-aligned
     * symbols and 16 KiB-multiple sizes keep every boundary compliant. */
    .stack (NOLOAD) : AT(ADDR(.stack) - KERNEL_BASE) ALIGN(16K) {
        __stack_guard_bottom = .;
        . += 16K;               /* the guard: mapped never, faults always */
        __stack_bottom = .;
        . += 64K;               /* the stack itself */
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

/* The vector table's alignment, double-checked at link time: VBAR_EL1's
 * low 11 bits are RES0, so a misaligned table would be silently truncated
 * to the wrong address and every fault would jump into the void — the
 * exact failure mode M1 exists to abolish. */
ASSERT((__vectors & 0x7FF) == 0, "exception vector table is not 2048-byte aligned (VBAR_EL1[10:0] are RES0)")

/* QEMU starts the CPU at the raw ELF entry with the MMU off. (Current
 * QEMU happens to rescue a high entry VMA by translating it back to its
 * LMA; m1n1 will not — never lean on it.) */
ASSERT(ABSOLUTE(_start) == 0x40200000, "entry must be the load PA — _start belongs at the first byte of .text.boot")

/* If the boot stub outgrows 0x400 bytes, the likeliest cause is not new
 * code but a silent lld thunk: a `bl`/`b` from the low section to a high
 * symbol makes lld synthesize an __AArch64AbsLongThunk_* veneer that
 * wedges pre-MMU. Cross-split calls must go through registers (see
 * boot.rs); this is the tripwire. */
ASSERT(SIZEOF(.text.boot) <= 0x400, "boot stub grew past 0x400 bytes — check llvm-nm for an AbsLongThunk (a bl across the low/high split?)")

/* rust-lld keeps ELF file offsets congruent to VMAs modulo the max page
 * size; a KERNEL_BASE that is ragged mod 64K would bloat the file with
 * alignment padding. */
ASSERT((KERNEL_BASE & 0xFFFF) == 0, "KERNEL_BASE must be 64 KiB-congruent or lld pads the ELF")

/* The MMU draws permission lines only between 16 KiB pages: every W^X
 * boundary and both edges of the stack guard must sit on granule edges,
 * or mmu.rs quietly rounds a boundary into the wrong region. */
ASSERT((__text_end & 0x3FFF) == 0 && (__rodata_end & 0x3FFF) == 0
    && (__stack_guard_bottom & 0x3FFF) == 0 && (__stack_bottom & 0x3FFF) == 0,
    "W^X / guard boundaries must sit on 16 KiB granule edges")

/* mmu.rs refines exactly one 32 MiB region (one L2 slot) with per-page
 * permissions; image plus stack must stay inside it. */
ASSERT(__stack_top - KERNEL_BASE <= 0x42000000, "image + stack outgrew the L3-mapped first 32 MiB — grow the map in mmu.rs")
