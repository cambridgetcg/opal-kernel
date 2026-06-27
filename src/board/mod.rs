//! Board definitions: which hardware exists, and where.
//!
//! A *board* is the thing that knows addresses. Drivers in `hal/` know how
//! a device block works; the board knows that this machine has one, where
//! it is, and how to bring the machine up. Today there are two board
//! modules:
//!
//! - [`virt`] — QEMU's aarch64 `virt` machine, our daily driver.
//! - [`apple`] — Apple Silicon (M1+), booted via m1n1 (M7 bring-up).
//!
//! [`which`] is the runtime selector: it reads the root `compatible`
//! string from the devicetree and returns which board module to use.
//! On QEMU the root is `linux,qemu-virt`; on Apple Silicon via m1n1 it
//! is `apple,<chip>` (e.g. `apple,t8103`). If the FDT is unreadable or
//! neither string matches, we fall back to `virt` — the honest default,
//! since every QEMU boot has a readable FDT, and an unreadable one means
//! we are on something we don't yet understand, not something we should
//! guess about.
//!
//! `board/apple.rs`'s job is noticeably harder than virt's: the UART base
//! must come from the devicetree (it differs per SoC) rather than a
//! constant, the console is a Samsung-style s5l UART, and init runs at
//! EL2 with a locked RVBAR. That asymmetry is exactly why addresses live
//! here and not in drivers.

pub mod virt;

// `apple` — the Apple Silicon board (M7). Compiled but not active on
// QEMU; `kmain` selects `board::virt` for the QEMU virt machine. The
// Apple board's `init()` discovers the AIC from the FDT and brings it
// online (masks all IRQs, targets CPU 0, caches NR_IRQ). The s5l UART
// driver is complete (`hal/s5l_uart.rs`), the EL2→EL1 boot stub drop is
// in `boot.rs`, and the FIQ handler checks the virtual timer. Remaining:
// AIC→handler dispatch wiring, framebuffer console. See
// docs/02-hal-and-apple-silicon.md §3.
#[allow(dead_code)]
pub mod apple;

/// Which board this machine is, discovered from the FDT's root
/// `compatible` string.
///
/// The decision is made once, early in `kmain`, before any board-specific
/// init runs. The caller passes an `Fdtr` (already parsed from the FDT
/// blob that the bootloader handed us). If the FDT is missing or the root
/// `compatible` doesn't match a known board, we return [`BoardKind::Virt`]
/// — the fallback, not a guess. Every QEMU boot has a readable FDT; a
/// missing one means something we don't yet understand, and the virt
/// board's cross-checks will immediately report the mismatch rather than
/// silently running the wrong init.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoardKind {
    /// QEMU's `virt` machine — root `compatible` contains `qemu,virt` or
    /// `linux,qemu-virt`. The daily driver, with compile-time addresses.
    Virt,
    /// Apple Silicon — root `compatible` starts with `apple,`. The FDT
    /// is the honest source for all addresses (UART, AIC, RAM base).
    Apple,
}

/// Read the root node's `compatible` string and decide which board this is.
///
/// Called after the FDT parser is initialized (see `arch::aarch64::fdt`).
/// The root node's `compatible` is the machine's most general declaration
/// of identity: `linux,qemu-virt` on QEMU, `apple,t8103` on M1, etc.
///
/// Detection logic:
/// - If the compatible string contains `qemu` → [`BoardKind::Virt`].
/// - If it starts with `apple,` → [`BoardKind::Apple`].
/// - Otherwise (including no FDT, no root node, no compatible property)
///   → [`BoardKind::Virt`] (the fallback, not a guess).
pub fn which(fdt: Option<&crate::arch::aarch64::fdt::Fdtr>) -> BoardKind {
    if let Some(fdt) = fdt {
        if let Some(root) = fdt.root() {
            let compat = fdt.compatible(&root);
            if compat.contains("qemu") {
                return BoardKind::Virt;
            }
            if compat.starts_with("apple,") {
                return BoardKind::Apple;
            }
        }
    }
    BoardKind::Virt
}
