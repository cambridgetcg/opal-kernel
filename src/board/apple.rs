//! Board: Apple Silicon (M1 and later), booted via m1n1.
//!
//! This is the board `board/virt.rs` has been rehearsing for since M2.
//! Every architectural decision M7 depends on — 16 KiB pages, FDT-driven
//! discovery, EL2 entry, FIQ-first interrupts — was made milestones ago on
//! QEMU. This file is where those rehearsals meet the real machine.
//!
//! ## What this board knows (today)
//!
//! - **The console is an s5l UART**, discovered from the FDT (its base
//!   address differs per SoC — no constant here). The driver in
//!   `hal/s5l_uart.rs` is complete; this board wires it up once the
//!   FDT parser hands us a base address.
//! - **Entry is at EL2.** The boot stub (`arch/aarch64/boot.rs`) already
//!   detects EL2, configures the minimal EL2 state (VBAR_EL2, HCR_EL2,
//!   CNTHCTL_EL2, CNTVOFF_EL2), and `eret`s down to EL1 before anything
//!   else runs. This board's `init()` therefore always runs at EL1.
//! - **Interrupts arrive as FIQs**, not through a GIC. The architectural
//!   timer fires on the FIQ line; device interrupts come through AIC
//!   (M1) or AICv2 (M1 Pro/Max+). The FIQ handler in `vectors.rs` checks
//!   `CNTV_CTL_EL0.ISTATUS` and services the timer; the AIC dispatch is
//!   the next piece of M7 work.
//!
//! ## What this board does NOT know (yet)
//!
//! - **AIC register layout.** The Apple Interrupt Controller is a
//!   proprietary design (documented by the Asahi Linux project); its
//!   driver will live in `hal/aic.rs` and be instantiated here.
//! - **Framebuffer.** m1n1 republishes iBoot's framebuffer as a
//!   simple-framebuffer FDT node; a text console blitting into it is a
//!   future M7 piece (`hal/fb.rs`).
//! - **PMGR / clocks / SMC.** Power management, clock gating, and the
//!   system management controller are each their own research project —
//!   labeled mountains, not refused, just beyond this teaching kernel's
//!   current horizon.
//!
//! ## Why this file exists before any Apple boot
//!
//! The same reason `hal/s5l_uart.rs` exists before any Apple boot: the
//! shape is knowable from public documentation (Asahi's docs, Linux's
//! devicetree bindings, the m1n1 source), and having the skeleton in
//! the tree forces every assumption to be written down where the build
//! can see it. When the first USB-C cable is plugged in, the gap between
//! "compiles on QEMU" and "boots on Apple" narrows to driver bugs, not
//! architecture surprises.
//!
//! See docs/02-hal-and-apple-silicon.md §3 for the full spec this file
//! is drawn from.

use crate::arch::aarch64::mmu;

// ---------------------------------------------------------------------------
// The console — s5l UART, FDT-discovered base
// ---------------------------------------------------------------------------

/// The s5l UART base address, discovered from the FDT at boot.
///
/// On QEMU's virt board the UART base is a compile-time constant
/// (`board::virt::UART0_BASE`). On Apple Silicon it differs per SoC
/// (M1 vs M1 Pro vs M2...), so the only honest source is the devicetree
/// that m1n1 hands us. This static is set by [`init`] after the FDT
/// parser finds the `compatible = "apple,s5l-uart"` node.
///
/// Zero means "not yet discovered" — before `init` runs, no console
/// exists. The boot stub's early output (if any) would go through m1n1's
/// proxy hypervisor, which traps MMIO and tunnels it over USB.
static S5L_UART_BASE: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Set the s5l UART base, discovered from the FDT by `kmain` (or the
/// board's own init, once the FDT parser is wired in). Called before
/// [`init`] so the console is live for the banner.
pub fn set_uart_base(pa: usize) {
    S5L_UART_BASE.store(pa, core::sync::atomic::Ordering::Relaxed);
}

/// The console type: an s5l UART whose base comes from the FDT, mapped
/// at its higher-half virtual address.
///
/// Today this is a placeholder — the base is a runtime value, not a
/// compile-time constant, so the zero-sized `S5lUart<const BASE>` pattern
/// from `board/virt.rs` does not directly apply. The real instantiation
/// will either use a `const`-generic parameter set by a const-evaluated
/// FDT lookup, or — more likely given that the FDT is inherently runtime
/// data — a small wrapper that reads `S5L_UART_BASE` and writes through a
/// raw pointer, matching the `fmt::Write` contract. The driver in
/// `hal/s5l_uart.rs` already implements `fmt::Write` for a const base;
/// bridging the runtime base is the next refinement.
pub type Console = crate::hal::s5l_uart::S5lUart<0x0>; // placeholder;

/// Bring the Apple board up.
///
/// Today this is a skeleton: the boot stub has already dropped to EL1
/// (the EL2→EL1 drop is in `boot.rs`), and the FDT parser (M4) can find
/// the UART node. What remains is:
///
/// 1. Discover the s5l UART base from the FDT and call `set_uart_base`.
/// 2. Initialize the AIC interrupt controller (`hal::aic::Aic`).
/// 3. Enable the architectural timer on the FIQ line (the FIQ handler
///    in `vectors.rs` already checks `CNTV_CTL_EL0.ISTATUS`).
///
/// On QEMU this function is never called — `kmain` selects
/// `board::virt` based on the FDT's `/compatible` or the known RAM base.
/// It exists so the compile-time question "does the Apple board module
/// build?" is answered before the first real boot.
pub fn init() {
    // The timer will fire on FIQ. The boot stub already configured
    // CNTHCTL_EL2.EVNTI=1 (at EL2) so EL1 can access CNTV_CTL/CVAL.
    // The FIQ handler in vectors.rs checks CNTV_CTL_EL0.ISTATUS and
    // services the tick. Nothing to do here yet — AIC init is the next
    // piece.
    //
    // When AIC lands, this is where we'd call:
    //   let mut aic = crate::hal::aic::Aic::new(aic_base);
    //   aic.init();
    //   aic.unmask_irq(AIC_TIMER_IRQ);
}

/// RAM base on Apple Silicon. Unlike QEMU's fixed 0x4000_0000, Apple's
/// RAM base is discovered from the FDT's `/memory` node. This constant
/// is a fallback for cross-checking, not a truth.
///
/// Apple Silicon Macs map DRAM at a higher physical address than QEMU's
/// virt board — the exact base varies by SoC generation. The FDT parser
/// (M4) reads it at boot; see `arch/aarch64/fdt.rs`.
pub const RAM_BASE_FALLBACK: usize = 0x8000_0000; // typical M1, not universal

/// The higher-half virtual alias of a physical address, using the same
/// KERNEL_BASE offset as `board::virt`. This keeps the VA<->PA mapping
/// uniform across boards.
#[allow(dead_code)] // no Apple board is active on QEMU
pub fn phys_to_virt(pa: usize) -> usize {
    mmu::phys_to_virt(pa)
}