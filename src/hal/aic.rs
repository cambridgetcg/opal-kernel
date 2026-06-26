#![allow(dead_code)] // No Apple board is active on QEMU; this skeleton
                     // exists so the compile-time question "does the AIC
                     // driver build?" is answered before the first real boot.
                     //
//! Apple Interrupt Controller (AIC) — the interrupt controller of
//! Apple Silicon (M1 and later).
//!
//! Where QEMU's `virt` board uses a GIC (see `hal/gicv2.rs`), Apple's SoCs
//! use a proprietary interrupt controller: AIC on M1-generation parts,
//! AICv2 on M1 Pro/Max and later (multi-die), and AICv3 on newer chips.
//! All three share the same fundamental model — the event/ack register —
//! and this skeleton targets AIC v1, the simplest and the one m1n1's
//! virtual-UART development loop exercises first.
//!
//! ## How AIC differs from the GIC
//!
//! The GIC separates "which interrupt fired" (GICC_IAR) from "I'm done"
//! (GICC_EOIR). AIC folds both into a single **event register**: reading
//! `AIC_EVENT` atomically acknowledges *and* masks the highest-priority
//! pending interrupt, returning its type and number. There is no separate
//! EOIR write — the read *is* the ack. To re-enable the interrupt, you
//! write to `AIC_MASK_CLR`.
//!
//! This is simpler in one way (one register, not two) and harder in
//! another (the read-side ack means you must handle the interrupt before
//! unmasking, or you re-enter immediately). The GICv2 driver's
//! `acknowledge()` + `end_interrupt()` pair becomes a single
//! `event()` read here.
//!
//! ## Register layout (AIC v1, offsets from base)
//!
//! From the Asahi Linux driver (`drivers/irqchip/irq-apple-aic.c`)
//! and the DT binding (`apple,aic.yaml`):
//!
//! ```text
//!  0x0004  AIC_INFO       — NR_IRQ in bits [15:0] (read-only)
//!  0x0010  AIC_CONFIG     — global config (v1: reserved/implementation-defined)
//!  0x2000  AIC_WHOAMI     — which CPU is reading (per-CPU register view)
//!  0x2004  AIC_EVENT      — read: ack + return event; 0 = nothing pending
//!                           bits [23:16] = TYPE, [15:0] = NUM
//!  0x2008  AIC_IPI_SEND   — send an IPI
//!  0x200c  AIC_IPI_ACK    — acknowledge an IPI
//!  0x2024  AIC_IPI_MASK_SET
//!  0x2028  AIC_IPI_MASK_CLR
//!  0x3000  AIC_TARGET_CPU — per-IRQ target CPU bitmap (u32 per IRQ)
//! ```
//!
//! After `AIC_TARGET_CPU` (u32 × NR_IRQ), the layout continues with
//! the bit-bank arrays: `SW_SET`, `SW_CLR`, `MASK_SET`, `MASK_CLR`,
//! `HW_STATE` — each `NR_IRQ/32` u32s, per die. For AIC v1 there is
//! one die, so no stride.
//!
//! ## Event encoding
//!
//! ```text
//!  AIC_EVENT_TYPE  bits [23:16]:  0 = FIQ (software use), 1 = IRQ, 4 = IPI
//!  AIC_EVENT_NUM   bits [15:0]:   IRQ number, or IPI ID, or FIQ index
//! ```
//!
//! For a teaching kernel, the first interrupt to handle is the
//! **architectural timer**. But on Apple Silicon, the timer arrives as a
//! **FIQ**, not an AIC IRQ — so AIC's event register returns `type=1`
//! (device IRQ), while the timer comes in through the FIQ vector
//! (already handled in `vectors.rs` via `CNTV_CTL_EL0.ISTATUS`).
//! AIC is for *device* interrupts; FIQs bypass it entirely.
//!
//! ## Sources
//!
//! Asahi Linux `drivers/irqchip/irq-apple-aic.c` (register defines,
//! event-loop logic, init sequence). DT binding
//! `Documentation/devicetree/bindings/interrupt-controller/apple,aic.yaml`.
//! The AIC comment block at the top of the Linux driver is the clearest
//! description of the controller's model in existence.

use core::ptr;

// -----------------------------------------------------------------------
// Register offsets — AIC v1, from the Linux driver
// -----------------------------------------------------------------------

/// Number of IRQs supported. Read from `AIC_INFO` bits [15:0].
/// The Linux driver caps at `AIC_MAX_IRQ = 0x400` (1024); M1 reports
/// fewer. This constant is a fallback for cross-checking, not a truth.
pub const AIC_MAX_IRQ: u32 = 0x400;

/// AIC_INFO: read NR_IRQ from bits [15:0].
mod info {
    pub const INFO: usize = 0x0004;
    pub const NR_IRQ: u32 = 0xFFFF; // bits [15:0]
}

/// AIC_CONFIG: global enable/config. On v1, largely implementation-
/// defined; on v2, bit 0 = enable. We touch it on init to be explicit.
mod config {
    pub const CONFIG: usize = 0x0010;
}

/// Per-CPU event registers. These are at a fixed offset on AIC v1;
/// the CPU interface auto-selects "this CPU" based on WHOAMI.
mod event {
    pub const WHOAMI: usize = 0x2000;
    /// Read: atomically acknowledge and return the highest-pending event.
    /// `0` means nothing pending.
    pub const EVENT: usize = 0x2004;
    pub const IPI_SEND: usize = 0x2008;
    pub const IPI_ACK: usize = 0x200c;
    pub const IPI_MASK_SET: usize = 0x2024;
    pub const IPI_MASK_CLR: usize = 0x2028;
}

/// Per-IRQ target CPU register: u32 per IRQ, starting at offset 0x3000.
mod target {
    pub const TARGET_CPU: usize = 0x3000;
}

// -----------------------------------------------------------------------
// Event decoding — the bit layout of the AIC_EVENT register
// -----------------------------------------------------------------------

/// Event type field: bits [23:16].
pub const EVENT_TYPE_SHIFT: u32 = 16;
/// Event number field: bits [15:0].
pub const EVENT_NUM_MASK: u32 = 0xFFFF;

/// FIQ event type (software-assigned; the timer FIQ bypasses AIC).
pub const EVENT_TYPE_FIQ: u32 = 0;
/// Hardware IRQ event type.
pub const EVENT_TYPE_IRQ: u32 = 1;
/// IPI event type.
pub const EVENT_TYPE_IPI: u32 = 4;

/// Decode the type field from an AIC_EVENT word.
pub fn event_type(event: u32) -> u32 {
    (event >> EVENT_TYPE_SHIFT) & 0xFF
}

/// Decode the number field from an AIC_EVENT word.
pub fn event_num(event: u32) -> u32 {
    event & EVENT_NUM_MASK
}

// -----------------------------------------------------------------------
// The driver
// -----------------------------------------------------------------------
/// An AIC v1 at a given MMIO base. Unlike `GicV2` (which is zero-sized
/// via const generics), `Aic` carries its base as a runtime field because
/// the address is FDT-discovered and differs per SoC. It also caches
/// `nr_irq`, read once from `AIC_INFO` during [`init`](Self::init), so
/// that [`unmask_irq`](Self::unmask_irq) and [`mask_irq`](Self::mask_irq)
/// do not re-read the register on every call — under m1n1 each MMIO
/// access is a hypervisor trap tunneled over USB.
///
/// The base address is **FDT-discovered** (it differs per SoC), so in
/// practice this is constructed at boot after the FDT parser finds the
/// `compatible = "apple,aic"` node. See `board/apple.rs::init`.
#[derive(Clone, Copy)]
pub struct Aic {
    base: usize,
    /// Cached `AIC_INFO` NR_IRQ (bits [15:0]). Zero until [`init`](Self::init)
    /// reads it from hardware; all bit-bank offset arithmetic derives from
    /// this value. Calling [`unmask_irq`](Self::unmask_irq) or
    /// [`mask_irq`](Self::mask_irq) before `init` is a bug — `nr_irq` would
    /// be 0 and the computed offsets wrong.
    nr_irq: u32,
}

impl Aic {
    /// Construct an AIC at the given physical base address.
    /// The caller is responsible for mapping this to a virtual address
    /// before any MMIO access (the kernel's `phys_to_virt` helper).
    /// `nr_irq` is left as 0 until [`init`](Self::init) reads it.
    pub const fn new(base: usize) -> Self {
        Self { base, nr_irq: 0 }
    }

    #[inline]
    unsafe fn read32(&self, off: usize) -> u32 {
        unsafe {
            ptr::with_exposed_provenance::<u32>(self.base + off).read_volatile()
        }
    }

    #[inline]
    unsafe fn write32(&self, off: usize, val: u32) {
        unsafe {
            ptr::with_exposed_provenance_mut::<u32>(self.base + off)
                .write_volatile(val)
        }
    }

    // --- Capability ---

    /// Read `AIC_INFO` and extract `NR_IRQ` (bits [15:0]).
    /// This is the number of hardware IRQs the controller supports on
    /// this SoC. The M1 reports ~896; the cap is 1024.
    ///
    /// This is a live MMIO read. After [`init`](Self::init) caches the
    /// value, prefer [`cached_nr_irq`](Self::cached_nr_irq) to avoid
    /// the extra register access (under m1n1, a hypervisor trap).
    pub fn nr_irq(&self) -> u32 {
        unsafe { self.read32(info::INFO) & info::NR_IRQ }
    }

    /// Return the cached NR_IRQ, read once during [`init`](Self::init).
    /// Zero before `init` has run.
    pub fn cached_nr_irq(&self) -> u32 {
        self.nr_irq
    }

    // --- Bit-bank offset arithmetic (private, one source of truth) ---

    /// Number of u32 bit-banks needed to cover `nr_irq` bits.
    /// `ceil(nr_irq / 32)` — the AIC packs 32 IRQs per bank register.
    #[inline]
    fn num_banks(nr_irq: usize) -> usize {
        (nr_irq + 31) / 32
    }

    /// Offset of the `MASK_SET` bit-bank array from the AIC base.
    ///
    /// Layout after `TARGET_CPU` (u32 × nr_irq):
    /// `SW_SET`, `SW_CLR`, `MASK_SET`, `MASK_CLR`, `HW_STATE` —
    /// each `num_banks` × u32. `MASK_SET` is the third array.
    #[inline]
    fn mask_set_off(nr_irq: usize) -> usize {
        let nb = Self::num_banks(nr_irq);
        target::TARGET_CPU + nr_irq * 4 + nb * 4 * 2 // skip SW_SET, SW_CLR
    }

    /// Offset of the `MASK_CLR` bit-bank array — one array past `MASK_SET`.
    #[inline]
    fn mask_clr_off(nr_irq: usize) -> usize {
        Self::mask_set_off(nr_irq) + Self::num_banks(nr_irq) * 4
    }

    // --- Init ---

    /// Bring the AIC online. Mirrors the Linux init sequence:
    ///
    /// 1. Read `AIC_INFO` and cache `NR_IRQ` (all offset arithmetic
    ///    derives from this value; `unmask_irq`/`mask_irq` use the cache
    ///    to avoid re-reading the register on every call).
    /// 2. Mask all IRQs (write `MASK_SET` to all-ones for every bit-bank).
    /// 3. Clear all software-triggered IRQs (write `SW_CLR` to all-ones).
    /// 4. Target all IRQs at CPU 0 (write 1 to each `TARGET_CPU` slot).
    ///
    /// This deliberately starts with everything masked: the caller then
    /// enables specific IRQs with `unmask_irq`. Starting unmasked would
    /// deliver spurious interrupts before handlers are registered.
    ///
    /// **Note:** On AIC v1, there is no global enable bit in `CONFIG`
    /// (that's AICv2's `AIC2_CONFIG_ENABLE`). The controller is live
    /// as soon as MMIO is reachable. We read `CONFIG` for the banner
    /// but do not write it on v1.
    pub fn init(&mut self) {
        // Read NR_IRQ once and cache it. The field is `&mut self` here
        // because this is the only point that writes it; everywhere else
        // uses `&self` against the cached value.
        self.nr_irq = unsafe { self.read32(info::INFO) & info::NR_IRQ };
        let nr = self.nr_irq as usize;
        let num_banks = Self::num_banks(nr);

        // Compute the bit-bank offsets via the shared helpers.
        let sw_set = target::TARGET_CPU + nr * 4;
        let sw_clr = sw_set + num_banks * 4;
        let mask_set = Self::mask_set_off(nr);

        unsafe {
            // 1. Mask all IRQs.
            for i in 0..num_banks {
                self.write32(mask_set + i * 4, 0xFFFF_FFFF);
            }
            // 2. Clear all software-triggered IRQs.
            for i in 0..num_banks {
                self.write32(sw_clr + i * 4, 0xFFFF_FFFF);
            }
            // 3. Target all IRQs at CPU 0.
            for i in 0..nr {
                self.write32(target::TARGET_CPU + i * 4, 1);
            }
        }
    }

    // --- Per-IRQ control ---

    /// Unmask (enable) interrupt `irq`. Writes the corresponding bit in
    /// `MASK_CLR`. On AIC, reading the event register auto-masks the
    /// delivered IRQ; this is how you re-enable it after handling.
    ///
    /// Uses the cached `nr_irq` — no `AIC_INFO` read here.
    pub fn unmask_irq(&self, irq: u32) {
        let bank = (irq / 32) as usize;
        let bit = 1u32 << (irq % 32);
        let mask_clr = Self::mask_clr_off(self.nr_irq as usize);
        unsafe {
            self.write32(mask_clr + bank * 4, bit);
        }
    }

    /// Mask (disable) interrupt `irq`. Writes the corresponding bit in
    /// `MASK_SET`.
    ///
    /// Uses the cached `nr_irq` — no `AIC_INFO` read here.
    pub fn mask_irq(&self, irq: u32) {
        let bank = (irq / 32) as usize;
        let bit = 1u32 << (irq % 32);
        let mask_set = Self::mask_set_off(self.nr_irq as usize);
        unsafe {
            self.write32(mask_set + bank * 4, bit);
        }
    }

    // --- Event handling ---

    /// Read the AIC event register. This **atomically acknowledges and
    /// masks** the highest-priority pending interrupt. Returns `0` if
    /// nothing is pending.
    ///
    /// The caller decodes the result with [`event_type`] and
    /// [`event_num`], dispatches to the handler, then calls
    /// [`unmask_irq`] to re-enable the source.
    ///
    /// This is the AIC equivalent of GICv2's `acknowledge()` — but
    /// folded with the implicit EOI. There is no `end_interrupt` here.
    pub fn event(&self) -> u32 {
        unsafe { self.read32(event::EVENT) }
    }

    /// Read `WHOAMI` — the CPU index of the reader. Useful for the
    /// banner and for per-CPU register views on multi-core.
    pub fn whoami(&self) -> u32 {
        unsafe { self.read32(event::WHOAMI) }
    }

    // --- Banner / diagnostics ---

    /// Read the raw `AIC_CONFIG` register. On v1 this is
    /// implementation-defined; on v2, bit 0 = global enable.
    pub fn config(&self) -> u32 {
        unsafe { self.read32(config::CONFIG) }
    }
}