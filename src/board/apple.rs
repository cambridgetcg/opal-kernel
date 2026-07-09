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
//!   `hal/s5l_uart.rs` is complete; this board's `init()` discovers the
//!   base via `find_by_compatible("apple,s5l-uart")`, translates it to
//!   the kernel VA, and stores it — `console()` constructs a
//!   `RuntimeS5lUart` for `println!`.
//! - **Entry is at EL2.** The boot stub (`arch/aarch64/boot.rs`) already
//!   detects EL2, configures the minimal EL2 state (VBAR_EL2, HCR_EL2,
//!   CNTHCTL_EL2, CNTVOFF_EL2), and `eret`s down to EL1 before anything
//!   else runs. This board's `init()` therefore always runs at EL1.
//! - **Interrupts arrive as FIQs**, not through a GIC. The architectural
//!   timer fires on the FIQ line; device interrupts come through AIC
//!   (M1) or AICv2 (M1 Pro/Max+). The FIQ handler in `vectors.rs` checks
//!   `CNTV_CTL_EL0.ISTATUS` and services the timer; the AIC dispatch is
//!   the next piece of M7 work.
//! - **The AIC interrupt controller driver exists** (`hal/aic.rs`, 341
//!   lines). This board's `init()` discovers the AIC base from the FDT
//!   (via `find_by_compatible("apple,aic")`), constructs an `Aic`,
//!   initializes it (masks all IRQs, targets CPU 0, caches NR_IRQ), and
//!   stores it in a static for the FIQ handler to use. The FIQ handler
//!   in `vectors.rs` now reads `AIC_EVENT` when the timer didn't fire,
//!   dispatches by type (IRQ/IPI/FIQ), and re-unmasks device IRQs —
//!   the dispatch wiring is complete (dormant on QEMU, live on Apple).
//!
//! ## What this board does NOT know (yet)
//!
//! - **AIC MMIO virtual address mapping.** ✅ Fixed: [`init`] now
//!   translates the FDT-discovered physical base to the kernel's
//!   higher-half virtual alias via `phys_to_virt` before constructing
//!   the `Aic`, and stores that virtual base for the FIQ handler. The
//!   linear map covers all MMIO space with Device-nGnRnE attributes.
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

use crate::arch::aarch64::fdt::Fdtr;
use crate::arch::aarch64::mmu;
use crate::hal::aic::Aic;

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
static S5L_UART_BASE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Set the s5l UART base, discovered from the FDT by `kmain` (or the
/// board's own init, once the FDT parser is wired in). Called before
/// [`init`] so the console is live for the banner.
pub fn set_uart_base(pa: usize) {
    S5L_UART_BASE.store(pa, core::sync::atomic::Ordering::Relaxed);
}

/// The console type: a runtime-base s5l UART.
///
/// `S5lUart<const BASE>` (used by `board/virt.rs`) is zero-sized and
/// perfect for a *compile-time-known* UART base. On Apple Silicon the
/// base is FDT-discovered at runtime — so the console carries it as a
/// field. `RuntimeS5lUart` implements `core::fmt::Write` with the same
/// byte path as the const-generic driver; the board's `init()` sets the
/// base after the FDT parser finds `compatible = "apple,s5l-uart"`.
///
/// Before `init` runs, the console is `RuntimeS5lUart::unconfigured()`
/// (base 0); writing through it would spin on an unmapped address. In
/// practice the boot stub's early output (if any) goes through m1n1's
/// proxy hypervisor, and this console is configured before any Rust
/// print path runs.
pub type Console = crate::hal::s5l_uart::RuntimeS5lUart;

/// Construct the board console. Mirrors `board::virt::console()`:
/// returns a `Console` the caller can `write_fmt` into. Before `init`
/// has set the UART base, this returns an unconfigured instance (base 0).
pub fn console() -> Console {
    let base = S5L_UART_BASE.load(core::sync::atomic::Ordering::Relaxed);
    Console::new(base)
}

// ---------------------------------------------------------------------------
// The interrupt controller — AIC, FDT-discovered base
// ---------------------------------------------------------------------------

/// The AIC instance, initialized during `init()` and stored for the
/// FIQ/IRQ dispatch loop to read events from. `None` until `init`
/// discovers the AIC node in the FDT and brings the controller online.
///
/// Under m1n1, each MMIO access is a hypervisor trap tunneled over USB,
/// so the `Aic` caches `NR_IRQ` (read once during `init`) to avoid
/// re-reading `AIC_INFO` on every `unmask_irq` / `mask_irq` call.
static AIC: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Return the AIC base address (virtual), or 0 if not yet initialized.
/// The FIQ dispatch loop uses this to construct an `Aic` reference and
/// call `event()`. The stored base is the higher-half virtual alias of
/// the FDT-discovered physical address — set by [`init`] via
/// `phys_to_virt`, so the handler's MMIO reads translate correctly.
#[allow(dead_code)]
pub fn aic_base() -> usize {
    AIC.load(core::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Board init
// ---------------------------------------------------------------------------

/// Bring the Apple board up.
///
/// Called by `kmain` when `board::which` returns [`BoardKind::Apple`].
/// At this point the boot stub has already dropped to EL1 (the EL2→EL1
/// drop is in `boot.rs`), and the FDT parser (M4) is available.
///
/// What this function does today:
///
/// 1. **Discovers the console UART** from the FDT by
///    `compatible = "apple,s5l-uart"`, reads its `reg` property (two
///    u64 cells: base + size), translates the base to the kernel's
///    higher-half VA, and stores it in `S5L_UART_BASE` so `console()`
///    can construct a `RuntimeS5lUart` for `println!`.
/// 2. **Discovers the AIC** from the FDT by `compatible = "apple,aic"`,
///    reads its `reg` property (two u64 cells: base + size), and
///    constructs an [`Aic`] at that physical address.
/// 3. **Initializes the AIC**: masks all IRQs, clears software-triggered
///    IRQs, targets all IRQs at CPU 0, caches `NR_IRQ` from `AIC_INFO`.
///    The controller starts with everything masked — specific IRQs are
///    enabled later by whichever driver asks for them.
/// 4. **Stores the AIC base** in a static so the FIQ dispatch loop can
///    find it when it needs to read `AIC_EVENT`.
///
/// What this function does NOT do yet:
///
/// - **Map the AIC's MMIO region with Device attributes.** ✅ Fixed:
///   the base is now translated to the kernel's virtual alias via
///   `phys_to_virt` before any MMIO access. The linear map grants
///   Device-nGnRnE attributes to MMIO regions the boot tables mark
///   Device; on Apple the FDT-reported AIC base lands in a region
///   the boot tables do not yet know about, so a dedicated
///   `ioremap`-style mapping (a fresh L3 page with Attr::Device) is
///   the next refinement — but the address translation itself is
///   honest now.
///
/// The AIC event dispatch in the FIQ vector (`vectors.rs::handle_fiq`)
/// is now wired: when the timer didn't fire and the AIC base is
/// non-zero, the handler reads `AIC_EVENT` and dispatches by type.
/// That path is dormant on QEMU (this function is never called) but
/// structurally complete for the first Apple boot.
///
/// On QEMU this function is never called — `kmain` selects
/// `board::virt` based on the FDT's `/compatible` or the known RAM base.
#[allow(dead_code)]
pub fn init(fdt: Option<&Fdtr>) {
    // The timer will fire on FIQ. The boot stub already configured
    // CNTHCTL_EL2.EVNTI=1 (at EL2) so EL1 can access CNTV_CTL/CVAL.
    // The FIQ handler in vectors.rs checks CNTV_CTL_EL0.ISTATUS and
    // services the tick.

    // Discover the console UART and the AIC interrupt controller from
    // the FDT. Both bases are FDT-discovered (they differ per SoC) and
    // translated to the kernel's higher-half virtual alias before any
    // MMIO access.
    if let Some(fdt) = fdt {
        // ---- Console: s5l UART ----
        // Find the serial node by `compatible = "apple,s5l-uart"`. The
        // `reg` property is two u64 cells (base, size) on Apple
        // devicetrees (#address-cells=2, #size-cells=2). We read the
        // first two u32 cells as the base address (high 32, low 32),
        // translate to the kernel VA, and store it so `console()` can
        // construct a `RuntimeS5lUart` for `println!`.
        if let Some(uart_node) = fdt.find_by_compatible("apple,s5l-uart") {
            let mut cells = fdt.prop_cells(&uart_node, "reg");
            let base_hi = cells.next().unwrap_or(0) as u64;
            let base_lo = cells.next().unwrap_or(0) as u64;
            let uart_pa = ((base_hi << 32) | base_lo) as usize;
            let uart_va = phys_to_virt(uart_pa);
            match mmu::ioremap_device(uart_va, uart_pa) {
                Ok(()) => {
                    S5L_UART_BASE.store(uart_va, core::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    // This is a boot-time fatal condition on Apple
                    // hardware: the console UART must be mapped before
                    // any print can reach it. Report it honestly.
                    println!(
                        "apple::init: failed to map s5l UART PA {uart_pa:#x} -> VA {uart_va:#x}: {e:?}"
                    );
                }
            }
        }

        // ---- Interrupt controller: AIC ----
        if let Some(aic_node) = fdt.find_by_compatible("apple,aic") {
            // The `reg` property is an array of u32 cells. On Apple
            // devicetrees the root has #address-cells=2 and #size-cells=2,
            // so `reg` is: base_hi, base_lo, size_hi, size_lo — four u32s
            // forming two u64s. We read the first two cells as the base
            // address (high 32 bits, low 32 bits).
            let mut cells = fdt.prop_cells(&aic_node, "reg");
            let base_hi = cells.next().unwrap_or(0) as u64;
            let base_lo = cells.next().unwrap_or(0) as u64;
            let aic_pa = ((base_hi << 32) | base_lo) as usize;

            let aic_va = phys_to_virt(aic_pa);
            match mmu::ioremap_device(aic_va, aic_pa) {
                Ok(()) => {
                    let mut aic = Aic::new(aic_va);
                    aic.init();
                    AIC.store(aic_va, core::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    println!(
                        "apple::init: failed to map AIC PA {aic_pa:#x} -> VA {aic_va:#x}: {e:?}"
                    );
                }
            }
        }
    }
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
