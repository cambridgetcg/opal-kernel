//! Board: QEMU aarch64 `virt` machine.
//!
//! QEMU's contract for this board is unusually small. Only two addresses
//! are documented as stable across QEMU versions:
//!
//! - flash at `0x0000_0000`
//! - RAM at `0x4000_0000`
//!
//! Everything else is *supposed* to be discovered from the devicetree blob
//! QEMU leaves at the start of RAM. We hardcode the UART base anyway - it
//! has not moved in many years, and the constants serve as cross-checkable
//! fallbacks (M4's FDT parser verifies them at boot; see
//! `arch/aarch64/fdt.rs`).
//!
//! Since M3 the board also knows the GIC's address. QEMU's virt defaults
//! to GICv2 (`arm,cortex-a15-gic`); the GICv2 distributor is at
//! 0x0800_0000 and the CPU interface at 0x0801_0000, both from the DTB.

use crate::arch::aarch64::mmu;
use crate::hal::gicv2::{GICC_BASE, GICD_BASE, GicV2, TIMER_IRQ};
use crate::hal::pl011::Pl011;

/// Start of RAM, physical.
pub const RAM_BASE: usize = 0x4000_0000;
pub const RAM_SIZE: usize = 512 * 1024 * 1024;
pub const UART0_BASE: usize = 0x0900_0000;
pub const UART0_VA: usize = mmu::phys_to_virt(UART0_BASE);

pub const GICD_VA: usize = mmu::phys_to_virt(GICD_BASE);
pub const GICC_VA: usize = mmu::phys_to_virt(GICC_BASE);

pub type Console = Pl011<UART0_VA>;

pub const fn console() -> Console {
    Console::new()
}

pub fn gic() -> GicV2 {
    GicV2::new(GICD_VA, GICC_VA)
}

/// Bring the board up. The GIC is initialized here: distributor enabled,
/// timer PPI configured and enabled, CPU interface brought online.
/// Interrupts stay masked at the CPU (DAIF.I) until the monitor's `tick`
/// command explicitly unmasks them.
pub fn init() {
    let gic = gic();
    gic.init();
    gic.enable_irq(TIMER_IRQ);
}
