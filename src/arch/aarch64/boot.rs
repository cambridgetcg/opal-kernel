//! The very first code in the kernel.
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
//! Why assembly at all? Until SP is valid and `.bss` is zeroed, Rust code
//! is unsound to run: the compiler is free to spill to the stack or read a
//! static at any moment. So a few hand-written instructions create the
//! world that Rust assumes, then jump in.

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
    // ---- 2. Give ourselves a stack --------------------------------------
    // __stack_top comes from the linker script (top of a 64 KiB NOLOAD
    // region, 16-byte aligned as AAPCS64 demands). adrp+add builds the
    // address PC-relatively: adrp gets the 4 KiB page, :lo12: adds the
    // offset within it. No literal pools, works at any load address.
    adrp  x1, __stack_top         // x1 = page address of __stack_top
    add   x1, x1, :lo12:__stack_top // x1 = exact address of __stack_top
    mov   sp, x1                  // stack is live; calls are now legal

    // ---- 3. Zero .bss ----------------------------------------------------
    // Every zero-initialized static lives in [__bss_start, __bss_end).
    // Until this loop runs, that memory is whatever the DRAM woke up as.
    adrp  x1, __bss_start
    add   x1, x1, :lo12:__bss_start // x1 = cursor
    adrp  x2, __bss_end
    add   x2, x2, :lo12:__bss_end   // x2 = one-past-the-end

.L_bss_clear:
    cmp   x1, x2                  // done?
    b.eq  .L_bss_done
    stp   xzr, xzr, [x1], #16     // store 16 zero bytes, cursor += 16
                                  // (both symbols are 16-aligned, so this
                                  //  never overshoots)
    b     .L_bss_clear

.L_bss_done:
    // ---- 4. Into Rust ----------------------------------------------------
    // x0 has not been touched since QEMU/m1n1 handed control to us; per
    // the AAPCS64 calling convention it is the first argument register,
    // so `_start_rust(x0)` receives exactly what the loader gave us.
    bl    _start_rust

    // ---- 5. Should-not-happen net ---------------------------------------
    // _start_rust is declared `-> !`. If it returns anyway (a bug), don't
    // run off into uninitialized memory — park this core forever.
.L_halt:
    wfe
    b     .L_halt
"#
);

/// First Rust on the machine. Stack is valid, `.bss` is zeroed; nothing
/// else has been set up.
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
