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

    // ---- 0. arm64 Image header (64 bytes) --------------------------------
    // The Linux arm64 "Image" boot protocol requires a 64-byte header at
    // the very start of the binary. m1n1 (and any arm64 bootloader) reads
    // the magic at offset 0x38 to identify the format, then uses
    // text_offset / image_size / flags to place the kernel in RAM.
    //
    // On QEMU ELF boot: ENTRY(_image_start) = the load PA, so QEMU jumps
    // here, executes the `b _start` (a 0x40-byte forward branch), and lands
    // at the real boot code — costing one branch instruction, nothing else.
    // On m1n1 Image boot: m1n1 loads the flat binary, validates the magic,
    // and jumps to offset 0, where the same `b _start` fires.
    //
    // Header layout (Linux Documentation/arm64/booting.rst):
    //
    //   0x00  code0        b _start (branch to real entry, 0x40 ahead)
    //   0x04  code1        0 (unused)
    //   0x08  text_offset  u64 = 0 (header IS the start of text; PIC)
    //   0x10  image_size   u64 = __image_size_bytes (linker-computed)
    //   0x18  flags        u64 = 0xA (LE | 16K pages | any placement)
    //   0x20  res2/res3/res4  3 × u64 = 0
    //   0x38  magic        u32 = 0x644d5241 ("ARM\x64", little-endian)
    //   0x3c  res5         u32 = 0
    //
    // flags encoding (arch/arm64/include/asm/image.h):
    //   bit 0    : 0 = LE, 1 = BE          → 0 (we are LE)
    //   bits 1-2 : 0 = 4K, 1 = 16K, 2 = 64K → 1 (Opal is 16K since M2)
    //   bit 3    : 0 = 2MB aligned, 1 = any  → 1 (PIC, load anywhere)
    //   = 0b1010 = 0xA
    .global _image_start
_image_start:
    b       _start                  // code0: branch to real entry (0x40 ahead)
    .zero   4                        // code1: unused
    .quad   0                        // text_offset: 0 (header is the text start)
    .quad   __image_size_bytes        // image_size (linker-computed, incl. bss)
    .quad   0xA                      // flags: LE | 16K pages | any placement
    .zero   24                       // res2, res3, res4 (3 × u64)
    .word   0x644d5241               // magic: "ARM\x64" (LE u32)
    .word   0                        // res5
    // 64 bytes of header done. _start follows at offset 0x40.

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
    // it back exactly as the calls found it. Stash it early: the EL2 drop
    // below uses eret, and x19 survives eret (it's a general register,
    // not banked across EL).
    mov   x19, x0

    // ---- 2b. Discover our own load address -------------------------------
    // The arm64 Image header claims "any placement" (flags bit 3 = 1), but
    // the rest of this stub still assumes PA 0x4020_0000. This `adr`
    // captures where we *actually* landed — the foundation for genuine PIC
    // boot. `adr` is PC-relative: the assembler encodes the link-time offset
    // from here to _image_start, and at run time the result is the *actual*
    // address of _image_start, wherever the loader placed us. On QEMU it
    // reads 0x4020_0000 (matching the link address); on Apple Silicon via
    // m1n1 it reads wherever m1n1 chose. Stashed in x22 (callee-saved,
    // survives the two `blr`s and the `eret`) and passed to _start_rust as
    // the second argument, so kmain can report the kernel's own placement
    // honestly — and compute the relocation delta the full PIC fix will need.
    adr   x22, _image_start     // x22 = actual PA of image start (load PA)

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
    //
    // Computed before the EL2 drop so the drop can use x21 to find
    // __vectors' physical address for VBAR_EL2. x21 survives eret.
    movz  x3, #0xFFFF, lsl #48    // KERNEL_BASE, the local belief
    ldr   x4, =__kernel_va_base   // KERNEL_BASE, the linker's truth
    cmp   x3, x4
    b.ne  .L_halt                 // disagree? nothing can be trusted; park
    mov   x21, x3                 // x21 = KERNEL_BASE, for the whole climb

    // ---- 3b. Drop from EL2 to EL1 if we entered at EL2 -------------------
    // QEMU's virt machine without virtualization extensions boots us at
    // EL1. m1n1 on Apple Silicon boots us at EL2 (Apple implements no
    // EL3; iBoot's last act is to drop to EL2). Every EL1 system register
    // this stub touches below — VBAR_EL1, SCTLR_EL1, TTBR1_EL1 — traps
    // to EL2 if we try it from EL2. So before anything else, detect the
    // exception level and, if it's 2, configure the minimal EL2 state
    // that lets EL1 run the kernel, then eret down to EL1.
    //
    // What we configure at EL2 (and why):
    //
    // - VBAR_EL2: point at our own exception vectors at their physical
    //   address (x21 = KERNEL_BASE is already computed; pool + subtract
    //   gives the PA). An EL2 trap during the drop would otherwise vector
    //   into nowhere. This is a safety net — kmain re-points VBAR_EL1
    //   once it runs at EL1.
    //
    // - HCR_EL2: RW=1 (EL1 is AArch64, not AArch32), and we clear
    //   everything else so IRQ/FIQ/Abort are NOT trapped to EL2 — they
    //   go to EL1 where M1's vector table catches them. The simplest
    //   correct HCR for a guest that IS the kernel.
    //
    // - CNTHCTL_EL2: EL2 controls whether EL1 can use the virtual timer.
    //   Bit 0 (EVNTI) = 1 lets EL1 access CNTV_CTL_EL0 / CNTV_CVAL_EL0
    //   without trapping. Without this, M3's timer — the heartbeat —
    //   would trap to EL2 on every arm/rearm. (QEMU starts at EL1 so
    //   CNTHCTL was never our problem; on Apple it is.)
    //
    // - CNTVOFF_EL2: set to 0 so the virtual timer count equals the
    //   physical count — same assumption M3's timer code makes today.
    //
    // - SPSR_EL2: the EL1 return state: EL1h (SP_EL1, not SP_EL0),
    //   IRQ+FIQ masked — matching QEMU's EL1 reset. DAIF [9:6] = 0b1111,
    //   mode [4:0] = 0b00101 (EL1h). SPSR_EL2 = 0x3C5.
    //
    // - ELR_EL2: where eret returns to — the .L_el1_entry label below,
    //   at its physical address (adr is PC-relative, MMU-off safe).
    //
    // After eret: EL1, MMU off, caches off, x0/x19/x21 intact — the same
    // state QEMU hands us natively. The rest of the stub is unchanged.
    mrs   x1, CurrentEL
    lsr   x1, x1, #2
    and   x1, x1, #3
    cmp   x1, #2
    b.ne  .L_el1_entry            // already at EL1 (QEMU) — skip the drop

    // We are at EL2. Configure the minimum EL2 state and eret to EL1.
    ldr   x9, =__vectors
    sub   x9, x9, x21             // __vectors PA (x21 = KERNEL_BASE)
    msr   VBAR_EL2, x9

    movz  x9, #0x8000, lsl #16    // HCR_EL2: RW bit (31) = 1, rest 0
    movk  x9, #0x0000, lsl #0     //   (0x8000_0000: AArch64 EL1, no trapping)
    msr   HCR_EL2, x9

    mov   x9, #1                  // CNTHCTL_EL2: EVNTI=1 (EL1 can use CNTV)
    msr   CNTHCTL_EL2, x9

    msr   CNTVOFF_EL2, xzr        // virtual offset = 0 (vcount == pcount)

    movz  x9, #0x3C5              // SPSR_EL2: DAIF=0b1111 | EL1h=0b0101
    msr   SPSR_EL2, x9

    adr   x9, .L_el1_entry        // ELR_EL2: PA of the EL1 entry label
    msr   ELR_EL2, x9

    eret                          // → EL1, PC = .L_el1_entry, x0/x19/x21 intact

.L_el1_entry:
    // Now at EL1 (whether we came from EL2 or started here). The rest of
    // the stub runs exactly as it did before M7 — the EL2 drop is a no-op
    // on QEMU, and a one-time stair-step on Apple Silicon.

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
    // expects. x1 carries the load PA from x22 — the kernel's own physical
    // placement, discovered by `adr` at step 2b — so kmain can report it
    // and compute the relocation delta the full PIC fix will lean on.
    // From the fetch after this one, the kernel lives upstairs.
    mov   x0, x19
    mov   x1, x22
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
/// `x1` is the kernel's own load physical address, discovered by `adr
/// _image_start` in the boot stub (step 2b). On QEMU it matches the link
/// address 0x4020_0000; on Apple Silicon via m1n1 it is wherever m1n1
/// placed the image. kmain reports this honestly — the foundation for
/// genuine position-independent boot.
///
/// # Safety
/// Must only be reached from `_start` above — it assumes the environment
/// that stub created.
#[unsafe(no_mangle)] // edition 2024: no_mangle is an unsafe attribute,
                     // because colliding symbol names break linkage soundness
pub unsafe extern "C" fn _start_rust(x0: u64, load_pa: u64) -> ! {
    crate::kmain(x0, load_pa)
}
