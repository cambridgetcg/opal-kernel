//! The very first code in the kernel — and, since M2, the only code that
//! runs where it is loaded.
//!
//! State of the machine when QEMU jumps to `_start` (ELF entry point):
//!
//! - EL1 (kernel privilege; QEMU virt has no EL2/EL3 by default)
//! - MMU off, caches off — every address is a physical address
//! - SP is **undefined** — we may not touch the stack until we set one
//! - `.bss` is **not zeroed** — ELF says "this region is zero", but with no
//!   loader runtime, *we* are the ones who have to make that true
//! - x0..x30 are at reset values. QEMU sets no registers for non-Linux ELF
//!   payloads (the famous "x0 = DTB pointer" contract applies only to the
//!   Linux boot protocol). We preserve x0 anyway: when m1n1 boots us on
//!   real Apple Silicon, x0 *will* carry the devicetree pointer, and the
//!   boot stub should not care which loader ran.
//!
//! New since M2: the kernel is linked to live in the higher half (VMA =
//! `KERNEL_BASE` + PA — see linker.ld and mmu.rs), but it still *loads*
//! and *starts* down here at 0x4020_0000. This stub is the climb between
//! those worlds: stack, zeroed .bss, page tables, MMU on, and only then
//! the jump upstairs into Rust.
//!
//! ## The two-address-space rules this file lives by
//!
//! Every symbol in the kernel (except this section's own labels) now has
//! a high address. From a low PC there are exactly two honest ways to
//! name one, and `adrp` — the M0/M1 habit — is **neither**:
//!
//! - `adrp` is PC-relative with ±4 GiB of reach. Aimed from 0x4020_xxxx
//!   at 0xFFFF_..., it does not even link (relocation overflow). Worse,
//!   high code that adrp's a high symbol *executed low* silently yields
//!   the low address — right answer for data that exists at both
//!   addresses, silent lie the moment the identity alias goes away.
//! - `ldr xN, =symbol` reads the full 64-bit address from a literal pool
//!   the assembler builds inside this very section (the `.ltorg` at the
//!   bottom). That names the *virtual* home; subtracting `KERNEL_BASE`
//!   (cross-checked against the linker's `__kernel_va_base` at step 3)
//!   turns it into the physical address the MMU-off world can touch.
//! - calls across the split must be `blr` through a register. A direct
//!   `bl high_symbol` from here makes lld synthesize a silent veneer (an
//!   `__AArch64AbsLongThunk_*`) that jumps to the raw high address and
//!   wedges pre-MMU; linker.ld's SIZEOF(.text.boot) ASSERT is the
//!   tripwire for that mistake.
//!
//! Why assembly at all? Until SP is valid and `.bss` is zeroed, Rust code
//! is unsound to run: the compiler is free to spill to the stack or read a
//! static at any moment. (The two Rust functions this stub *does* call
//! before the jump — the table builder and the enable ceremony — run
//! under the LOW WORLD contract spelled out in mmu.rs.)

use core::arch::global_asm;

global_asm!(
    r#"
.section .text.boot, "ax"       // "ax" = allocatable + executable
.global _start

_start:
    // ---- 1. Park every core except core 0 -------------------------------
    // We boot with -smp 1 today, but this stub should survive the day we
    // don't. MPIDR_EL1 identifies the core by four *affinity* fields:
    // Aff0 (bits 7:0) is the core within a cluster, Aff1 (15:8) the
    // cluster, Aff2 (23:16) and Aff3 (39:32) higher groupings. Testing
    // Aff0 alone is a classic trap: on any multi-cluster machine the
    // first core of *every* cluster reads Aff0 == 0 (Apple Silicon's
    // P-cluster is Aff1=1, Aff0=0..3) — it would sail past the check,
    // point SP at core 0's stack, and re-zero .bss under live statics.
    // So the boot core is the one whose *whole* affinity is zero.
    //
    // The combined mask 0xff_00ff_ffff is not encodable as one logical
    // immediate (bits 31:24 — MT, U, and the RES1 bit 31, none of them
    // affinity — punch a hole in the bit pattern), so build it in two
    // steps. x2 is scratch; only x0 is precious here.
    mrs   x1, MPIDR_EL1           // x1 = this core's topology ID
    mov   x2, #0xffffff           // mask = Aff2 | Aff1 | Aff0
    movk  x2, #0xff, lsl #32      // mask |= Aff3
    and   x1, x1, x2              // keep all four affinity fields
    cbz   x1, .L_core0            // core 0.0.0.0 continues; others park

.L_park:
    wfe                           // wait-for-event: sleep until poked
    b     .L_park                 // poked? not our turn yet — sleep again

.L_core0:
    // ---- 2. Stash the loader's gift --------------------------------------
    // x0 may carry a devicetree pointer (m1n1 / Linux protocol; QEMU ELF
    // boot leaves 0). It must survive two function calls before reaching
    // _start_rust, and AAPCS64 says x19 is callee-saved — Rust will give
    // it back exactly as the calls found it.
    mov   x19, x0

    // ---- 3. KERNEL_BASE, twice, compared, then parked somewhere safe ------
    // The VA<->PA offset is the one number this whole file leans on, so we
    // materialize it two independent ways — an immediate built here, and
    // the linker's __kernel_va_base from the literal pool — and refuse to
    // run if they disagree. A silent mismatch (say, linker.ld edited and
    // this file forgotten) would otherwise become an unmappable hang
    // three steps from now. The agreed value then moves to x21: two `blr`s
    // into compiled Rust stand between here and its last use, x3 is a
    // caller-saved temporary the callees may clobber freely, and a kernel
    // that boots because the register allocator happened not to pick x3
    // is a kernel one rustc upgrade away from a silent hang. Callee-saved
    // x21 is the same AAPCS64 logic that parks the DTB pointer in x19.
    movz  x3, #0xFFFF, lsl #48    // KERNEL_BASE, the local belief
    ldr   x4, =__kernel_va_base   // KERNEL_BASE, the linker's truth
    cmp   x3, x4
    b.ne  .L_halt                 // disagree? nothing can be trusted; park
    mov   x21, x3                 // x21 = KERNEL_BASE, for the whole climb

    // ---- 4. Give ourselves a stack (at its physical address) -------------
    // __stack_top is a high VMA now; the machine is not. Pool + subtract.
    ldr   x1, =__stack_top
    sub   x1, x1, x21
    mov   sp, x1                  // stack is live; calls are now legal

    // ---- 5. Zero .bss (at its physical addresses) -------------------------
    // Every zero-initialized static lives in [__bss_start, __bss_end).
    // Until this loop runs, that memory is whatever the DRAM woke up as.
    // Since M2 this matters double: the page tables live in .bss, and a
    // descriptor we never write must read as INVALID — that is, zero.
    ldr   x1, =__bss_start
    sub   x1, x1, x21             // x1 = cursor (PA)
    ldr   x2, =__bss_end
    sub   x2, x2, x21             // x2 = one-past-the-end (PA)

.L_bss_clear:
    cmp   x1, x2                  // done?
    b.eq  .L_bss_done
    stp   xzr, xzr, [x1], #16     // store 16 zero bytes, cursor += 16
                                  // (both symbols are 16-aligned, so this
                                  //  never overshoots)
    b     .L_bss_clear

.L_bss_done:
    // ---- 6. Build the page tables -----------------------------------------
    // Rust, called at its physical address (LOW WORLD contract: mmu.rs).
    // Returns the root table's PA in x0; stash it in callee-saved x20.
    ldr   x9, =opal_build_tables
    sub   x9, x9, x21
    blr   x9
    mov   x20, x0

    // ---- 7. Pre-arm the vector table, low ----------------------------------
    // The identity alias of __vectors, so that from the enabling ISB
    // onward a fault gets M1's full reporter instead of a hang. (Honesty:
    // a fault *before* M=1 still cannot report — the reporter's format
    // strings live at unmapped-until-then high addresses. That window is
    // covered by having debugged steps 6 and 8 in the identity-linked
    // rehearsal world first; M5's dedicated exception stack is the
    // structural fix.)
    ldr   x9, =__vectors
    sub   x9, x9, x21
    msr   VBAR_EL1, x9
    isb

    // ---- 8. The cliff -------------------------------------------------------
    // MMU and caches on (the ceremony lives in mmu.rs, one asm block,
    // heavily annotated). Returns here, to this same low PC — now
    // *translated* through the identity trunk of the shared tree.
    mov   x0, x20
    ldr   x9, =opal_mmu_enable
    sub   x9, x9, x21
    blr   x9

    // ---- 9. The canary: prove the high half exists, or say so and stop ----
    // Two checks, because the two failure shapes differ. First, AT S1E1R:
    // ask the walker to translate the canary's high address *without
    // touching it*. A raw load of an untranslatable VA would fault, and a
    // fault here cannot speak — VBAR points at M1's reporter, but the
    // reporter's format strings are ABS64 data, high, and (if TTBR1 is
    // broken) exactly as unreachable as the canary; the result would be a
    // silent nested-fault loop. AT instead reports failure as a *value*,
    // PAR_EL1 bit 0. This is the check that catches the TG1 trap (its 16K
    // encoding differs from TG0's!): under the wrong granule the walker
    // misreads the tree's geometry and the walk dies at level 1 —
    // demonstrated live during M2's design. Second, the load-and-compare:
    // AT only proves the address translates SOMEWHERE; the canary's value
    // proves it translates to the right frames. Either failure: a '?' on
    // the UART through the still-live identity Device mapping, then park
    // — immediate and located, instead of "boots, then dies hours later".
    ldr   x9, =MMU_CANARY         // the high VMA, straight from the pool
    at    s1e1r, x9               // walk it; the answer is a value, never a fault
    isb                           // PAR_EL1 is valid only after a CSE
    mrs   x10, PAR_EL1
    tbnz  x10, #0, .L_canary_dead // PAR.F=1: the high half does not translate
    ldr   x10, [x9]               // <- the first higher-half load
    movz  x11, #0x4F4B            // "OPALM2OK", rebuilt 16 bits at a time
    movk  x11, #0x4D32, lsl #16
    movk  x11, #0x414C, lsl #32
    movk  x11, #0x4F50, lsl #48
    cmp   x10, x11
    b.eq  .L_canary_ok

.L_canary_dead:
    movz  x12, #0x0900, lsl #16   // PL011 base, physical (board/virt.rs);
    mov   w13, #0x3F              // '?' — the identity Device mapping is
    str   w13, [x12]              // still up, so this byte gets out
    b     .L_halt

.L_canary_ok:
    // ---- 10. Re-point the vector table, high --------------------------------
    // From here even a broken jump upstairs gets a full report through
    // TTBR1. (kmain's vectors::install() re-derives the same address
    // PC-relatively once it runs high — reading VBAR back in the banner
    // then confirms both agree.)
    ldr   x9, =__vectors
    msr   VBAR_EL1, x9
    isb

    // ---- 11. Move the stack upstairs -----------------------------------------
    // Same physical memory, new name. x29 is zeroed because the frame
    // chain ends here: a low frame pointer would be a dangling lie the
    // moment the low half is condemned (and we never return anyway).
    mov   x10, sp
    add   x10, x10, x21
    mov   sp, x10
    mov   x29, xzr

    // ---- 12. The move ----------------------------------------------------------
    // An absolute branch: `br` through the pooled high address — the only
    // instruction that can cross the 4 GiB adrp horizon. x0 carries the
    // loader's DTB pointer out of x19, per the AAPCS64 the next function
    // expects. From the fetch after this one, the kernel lives upstairs.
    mov   x0, x19
    ldr   x9, =_start_rust
    br    x9

    // ---- 13. Should-not-happen net ----------------------------------------------
    // Reached only by a failed cross-check (step 3) or a dead canary
    // (step 9): the world is broken in a way that printing can't be
    // trusted to describe. Park this core forever.
.L_halt:
    wfe
    b     .L_halt

    // The literal pool: every `ldr xN, =symbol` above reads its 64-bit
    // value from here. Placed explicitly after the terminal loop so the
    // pool is data past the end of execution, never accidental
    // fall-through "instructions".
    .ltorg
"#
);

/// First Rust in the higher half. Stack is valid (and high), `.bss` is
/// zeroed, the MMU and caches are on, and the canary has proven TTBR1
/// translates; the identity half is still mapped and stays so until
/// `kmain` condemns it.
///
/// `x0` is whatever was in register x0 at kernel entry: 0 under QEMU's
/// ELF boot, a physical FDT pointer under m1n1 / the Linux boot protocol.
///
/// # Safety
/// Must only be reached from `_start` above — it assumes the environment
/// that stub created.
#[unsafe(no_mangle)] // edition 2024: no_mangle is an unsafe attribute,
                     // because colliding symbol names break linkage soundness
pub unsafe extern "C" fn _start_rust(x0: u64) -> ! {
    crate::kmain(x0)
}
