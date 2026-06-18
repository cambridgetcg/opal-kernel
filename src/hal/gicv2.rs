//! ARM Generic Interrupt Controller v2 — the default interrupt controller
//! of QEMU's `virt` machine.
//!
//! ## Why GICv2, not v3
//!
//! The roadmap said "GICv3 on virt," and we tried — but GICv3's CPU
//! interface lives in ICC_* system registers, which require
//! `ICC_SRE_EL1.SRE=1` to access. On QEMU's virt machine without
//! `-machine virtualization=on` there is no EL2 to set `ICC_SRE_EL2.SRE`,
//! and the ICC_* accesses trap at EL1 (EC 0x08). GICv2's CPU interface is
//! *memory-mapped* — no system registers, no EL2 dependency — and is
//! what QEMU's virt defaults to. The teaching kernel's job is to reach
//! the foothills, and GICv2 is the honest path on this board.
//!
//! ## What the GIC does
//!
//! The CPU has one IRQ line and one FIQ line. A machine with dozens of
//! devices needs to share that one line, and the GIC is the arbiter: every
//! device interrupt feeds into the GIC, the GIC prioritises them, and
//! presents exactly one at a time to the CPU as an IRQ. When the CPU
//! acknowledges, the GIC hands back an **interrupt ID** (the number that
//! identifies *which* device fired); when the CPU says "end of interrupt",
//! the GIC is ready to present the next one.
//!
//! The architectural timer is one of those devices — but it is wired as a
//! **Private Peripheral Interrupt (PPI)**, not a Shared Peripheral
//! Interrupt (SPI). PPIs are per-CPU: every core gets its own private timer
//! interrupt. In GICv2, PPIs and SGIs (IDs 0-31) are handled by the CPU
//! interface directly; SPIs (IDs 32+) go through the distributor.
//!
//! ## The two faces of GICv2
//!
//! 1. **Distributor (GICD)** — MMIO, one per system. Global config: which
//!    interrupts are enabled, their priority, their target cores.
//! 2. **CPU Interface (GICC)** — MMIO, one per CPU core. The per-CPU side:
//!    acknowledge (`GICC_IAR`), end-of-interrupt (`GICC_EOIR`), priority
//!    mask (`GICC_PMR`), and the enable bit (`GICC_CTLR`).
//!
//! Both are memory-mapped — no system registers, no EL2 dependency. That
//! simplicity is exactly why GICv2 is the right teaching step.
//!
//! ## Sources
//!
//! ARM GIC Architecture Specification (IHI 0048): GICv2 distributor and
//! CPU interface register maps. QEMU `hw/intc/arm_gic.c` for the virt
//! board's exact MMIO layout (0x0800_0000 distributor, 0x0801_0000 CPU
//! interface — confirmed against the DTB).

use core::ptr;

// -----------------------------------------------------------------------
// Physical addresses — from the devicetree, not folklore
// -----------------------------------------------------------------------

/// GIC Distributor base, physical. From QEMU's DTB (`intc@8000000`):
/// `reg = <0x0 0x08000000 0x0 0x10000>`. 64 KiB of MMIO.
pub const GICD_BASE: usize = 0x0800_0000;

/// GIC CPU Interface base, physical. From the DTB:
/// `reg = <0x0 0x08010000 0x0 0x10000>`. 64 KiB of MMIO.
pub const GICC_BASE: usize = 0x0801_0000;

// -----------------------------------------------------------------------
// Interrupt IDs
// -----------------------------------------------------------------------

/// The virtual timer's PPI ID. The architectural timer node in the DTB
/// lists four PPIs:
///
///   PPI 13 (IRQ 29) - secure physical timer
///   PPI 14 (IRQ 30) - non-secure physical timer
///   PPI 11 (IRQ 27) - virtual timer  <- ours
///   PPI 10 (IRQ 26) - hypervisor timer
///
/// We use the **virtual timer** because at EL1 without EL2 configured
/// (QEMU virt's default), the physical timer accesses (CNTP_*) are
/// trapped by CNTHCTL_EL2, which we cannot configure from EL1. The
/// virtual timer (CNTV_*) has no such trap - it is always accessible
/// at EL1.
pub const TIMER_IRQ: u32 = 27;

// -----------------------------------------------------------------------
// Distributor (GICD) register offsets
// -----------------------------------------------------------------------

mod gicd {
    /// Control register: bit 0 = enable.
    pub const CTLR: usize = 0x0000;
    /// Controller type register.
    pub const TYPER: usize = 0x0004;
    /// Group enable: bit 0 = group 0 enabled. GICv2 has one group.
    pub const IGROUPR: usize = 0x0080;
    /// Enable set: writing 1 to bit N enables interrupt N. One 32-bit
    /// register per 32 interrupts.
    pub const ISENABLER: usize = 0x0100;
    /// Enable clear.
    pub const ICENABLER: usize = 0x0180;
    /// Priority: one byte per interrupt.
    pub const IPRIORITYR: usize = 0x0400;
    /// Target: one byte per interrupt (CPU affinity bitmap).
    pub const ITARGETSR: usize = 0x0800;
    /// Configuration: 2 bits per interrupt. Level vs edge.
    pub const ICFGR: usize = 0x0C00;
}

// -----------------------------------------------------------------------
// CPU Interface (GICC) register offsets
// -----------------------------------------------------------------------

mod gicc {
    /// Control register: bit 0 = enable, bit 1 = FIQ enable,
    /// bit 2 = ACK ctrl, bit 3 = EOImodeS.
    pub const CTLR: usize = 0x0000;
    /// Interrupt Acknowledge Register: read returns the pending IRQ ID.
    /// Writing (EOIR) signals end of interrupt.
    pub const IAR: usize = 0x000C;
    /// End of Interrupt Register: write the acknowledged ID here.
    pub const EOIR: usize = 0x0010;
    /// Priority Mask: interrupts with priority >= PMR are masked.
    pub const PMR: usize = 0x0004;
    /// Binary Point: priority grouping. 0 = no grouping.
    pub const BPR: usize = 0x0008;
    /// Active Priority: read to know current priority.
    pub const RPR: usize = 0x0014;
    /// Highest Pending Interrupt.
    pub const HPPIR: usize = 0x0018;
}

// -----------------------------------------------------------------------
// The driver
// -----------------------------------------------------------------------

/// A GICv2 at fixed MMIO addresses. Like `Pl011`, zero-sized: the
/// addresses live in the type.
#[derive(Clone, Copy)]
pub struct GicV2 {
    gicd: usize,
    gicc: usize,
}

impl GicV2 {
    pub const fn new(gicd: usize, gicc: usize) -> Self {
        Self { gicd, gicc }
    }

    #[inline]
    unsafe fn read32(addr: usize) -> u32 {
        unsafe { ptr::with_exposed_provenance::<u32>(addr).read_volatile() }
    }

    #[inline]
    unsafe fn write32(addr: usize, val: u32) {
        unsafe { ptr::with_exposed_provenance_mut::<u32>(addr).write_volatile(val) }
    }

    /// Bring the GICv2 online. Called from `board::virt::init()`.
    pub fn init(&self) {
        let gicd = self.gicd;
        let gicc = self.gicc;

        unsafe {
            // ---- 1. Enable the distributor ----
            Self::write32(gicd + gicd::CTLR, 1); // bit 0 = enable

            // ---- 2. Configure the timer PPI ----
            // GICv2 has one interrupt group (group 0). Set the timer PPI
            // to group 0 (the default, but be explicit).
            let igrp = Self::read32(gicd + gicd::IGROUPR);
            // PPIs are IDs 16-31. Clear bit TIMER_IRQ to put it in group 0.
            Self::write32(gicd + gicd::IGROUPR, igrp & !(1u32 << TIMER_IRQ));

            // Priority: 0x80 = medium. Lower = higher priority.
            let prio_addr = gicd + gicd::IPRIORITYR + TIMER_IRQ as usize;
            ptr::with_exposed_provenance_mut::<u8>(prio_addr).write_volatile(0x80);

            // Target: CPU 0 only (bit 0). For PPIs this is usually the
            // default, but set it explicitly.
            let target_addr = gicd + gicd::ITARGETSR + TIMER_IRQ as usize;
            ptr::with_exposed_provenance_mut::<u8>(target_addr).write_volatile(0x01);

            // Edge-triggered: 2 bits per interrupt. PPI 27 is at
            // ICFGR offset 0x0C00 + (27/16)*4 = 0x0C04. Bits [13:12] for
            // PPI 11 (ID 27 in GICv2 terms: 27 - 16 = 11th PPI, at
            // bit position 2*11 = 22). Edge = 0b10.
            let icfgr_offset = gicd + gicd::ICFGR + ((TIMER_IRQ / 16) as usize) * 4;
            let icfgr_val = Self::read32(icfgr_offset);
            let shift = 2 * (TIMER_IRQ % 16) as u32;
            Self::write32(icfgr_offset, icfgr_val | (0b10u32 << shift));

            // Enable the timer PPI at the distributor.
            Self::write32(gicd + gicd::ISENABLER, 1u32 << TIMER_IRQ);

            // ---- 3. Enable the CPU interface ----
            // Set priority mask to 0xFF (allow all priorities).
            Self::write32(gicc + gicc::PMR, 0xFF);
            // Set binary point to 0 (no priority grouping).
            Self::write32(gicc + gicc::BPR, 0);
            // Enable the CPU interface: bit 0 = enable.
            Self::write32(gicc + gicc::CTLR, 1);
        }
    }

    /// Enable interrupt `id` at the GIC.
    pub fn enable_irq(&self, id: u32) {
        unsafe {
            let reg = self.gicd + gicd::ISENABLER + (id / 32) as usize * 4;
            Self::write32(reg, 1u32 << (id % 32));
        }
    }

    /// Disable interrupt `id`.
    pub fn disable_irq(&self, id: u32) {
        unsafe {
            let reg = self.gicd + gicd::ICENABLER + (id / 32) as usize * 4;
            Self::write32(reg, 1u32 << (id % 32));
        }
    }

    /// Acknowledge the current interrupt and return its ID.
    pub fn acknowledge(&self) -> u32 {
        unsafe { Self::read32(self.gicc + gicc::IAR) }
    }

    /// Signal end-of-interrupt for `id`.
    pub fn end_interrupt(&self, id: u32) {
        unsafe { Self::write32(self.gicc + gicc::EOIR, id) }
    }

    /// Read the GICD type register for the banner.
    pub fn typer(&self) -> u32 {
        unsafe { Self::read32(self.gicd + gicd::TYPER) }
    }
}
