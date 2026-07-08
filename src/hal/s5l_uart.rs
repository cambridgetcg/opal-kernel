//! Samsung-style s5l UART driver — the serial port of Apple Silicon.
//!
//! Apple's SoCs (M1 and later) expose a UART compatible with the
//! Samsung S3C serial block (Linux binding `apple,s5l-uart`). Like
//! the PL011, it is memory-mapped and a polled driver needs only two
//! operations: check whether the transmit buffer can accept a byte, and
//! check whether a received byte is waiting.
//!
//! ## Register layout (offsets from base)
//!
//! The Samsung UART register map (from the S3C TRM and Linux's
//! `drivers/tty/serial/samsung_tty.c`):
//!
//! | offset | name      | role                                              |
//! |--------|-----------|---------------------------------------------------|
//! | 0x10   | UTRSTAT   | status: bit 1 = TX buffer empty, bit 0 = RX ready |
//! | 0x20   | UTXH      | data: write = transmit (low 8 bits)               |
//! | 0x24   | URXH      | data: read = receive (low 8 bits)                 |
//!
//! The full register set (ULCON, UCON, UFCON, UBRDIV, ...) exists but is
//! not needed for a polled driver: under m1n1's hypervisor the UART is
//! already initialized by iBoot and republished as-is, so the kernel's
//! first boot does no baud-rate or FIFO configuration — the same luxury
//! QEMU's PL011 model grants us on virt.
//!
//! ## Polarity difference from PL011
//!
//! The PL011's flag register uses "full/empty" semantics (TXFF = 1 means
//! *cannot* write, RXFE = 1 means *cannot* read). The s5l UART's UTRSTAT
//! uses the opposite convention: TX-empty = 1 means *can* write, RX-ready
//! = 1 means *can* read. The driver inverts nothing — each is clear about
//! which bit it tests.
//!
//! ## MMIO ground rules
//!
//! Identical to `pl011.rs`: `read_volatile`/`write_volatile` on raw
//! pointers built via provenance APIs, no `&`/`&mut` to device memory,
//! 32-bit access width for the status register and 8-bit for data
//! registers (matching the Samsung hardware's byte-wide data path).
//!
//! ## Why this file exists before any Apple board does
//!
//! `pl011.rs` was written against QEMU and proven before the MMU was on.
//! This driver is written against the Samsung S3C specification (which
//! `apple,s5l-uart` declares compatibility with) and the m1n1 source's
//! UART usage, before the first Apple boot. It compiles on QEMU today;
//! its first real test is a USB-C cable and m1n1's proxy. The register
//! offsets and bit positions are from the public Samsung S3C UART
//! documentation and Linux's `samsung_tty.c`; if the Apple variant
//! diverges, the divergence will be a small edit here, not an
//! architecture change.

use core::fmt;
use core::ptr;

/// A Samsung-style s5l UART at base address `BASE`.
///
/// Zero-sized and `Copy`, mirroring `Pl011<BASE>`: the base lives in the
/// type so a console can be conjured anywhere — including the panic
/// handler, where shared state may be mid-flight.
#[derive(Clone, Copy)]
#[allow(dead_code)] // no Apple board wires this up yet — M7 continues
pub struct S5lUart<const BASE: usize>;

#[allow(dead_code)] // no Apple board wires this up yet — M7 continues
impl<const BASE: usize> S5lUart<BASE> {
    const UTRSTAT: usize = BASE + 0x10;
    const UTXH: usize = BASE + 0x20;
    const URXH: usize = BASE + 0x24;

    const UTRSTAT_TX_EMPTY: u32 = 1 << 1; // TX buffer empty — can write
    const UTRSTAT_RX_READY: u32 = 1 << 0; // RX data ready — can read

    pub const fn new() -> Self {
        Self
    }

    /// Read the transmit/receive status register.
    #[inline]
    fn status(self) -> u32 {
        // SAFETY: UTRSTAT is a readable 32-bit device register. BASE is
        // whichever alias the instantiator may legally touch — on Apple
        // Silicon, the higher-half VA that the Apple board's page tables
        // map Device-nGnRnE.
        unsafe { ptr::with_exposed_provenance::<u32>(Self::UTRSTAT).read_volatile() }
    }

    /// Transmit one byte. Spins while the TX buffer is non-empty.
    pub fn write_byte(self, b: u8) {
        while self.status() & Self::UTRSTAT_TX_EMPTY == 0 {
            core::hint::spin_loop();
        }
        // SAFETY: UTXH is a writable device register; TX_EMPTY was set,
        // so the buffer can accept a byte.
        unsafe { ptr::with_exposed_provenance_mut::<u8>(Self::UTXH).write_volatile(b) }
    }

    /// Receive one byte if one is waiting, else `None`. Never blocks.
    pub fn try_read_byte(self) -> Option<u8> {
        if self.status() & Self::UTRSTAT_RX_READY == 0 {
            return None;
        }
        // SAFETY: URXH is a readable device register; RX_READY was set,
        // so a byte is in the buffer. Reading pops it — volatile.
        let data = unsafe { ptr::with_exposed_provenance::<u8>(Self::URXH).read_volatile() };
        Some(data)
    }

    /// Receive one byte, spinning until one arrives.
    pub fn read_byte(self) -> u8 {
        loop {
            if let Some(b) = self.try_read_byte() {
                return b;
            }
            core::hint::spin_loop();
        }
    }
}

impl<const BASE: usize> Default for S5lUart<BASE> {
    fn default() -> Self {
        Self::new()
    }
}

/// Same `core::fmt::Write` hook as PL011 — so `println!` works identically
/// regardless of which UART is behind the console type alias.
impl<const BASE: usize> fmt::Write for S5lUart<BASE> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            // Serial terminals want CRLF; translate here so callers
            // never think about it — same convention as pl011.rs.
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------
// Runtime-base wrapper — the Apple Silicon console path
// -----------------------------------------------------------------------

/// A Samsung-style s5l UART whose base address is a *runtime* value.
///
/// `S5lUart<const BASE>` carries its MMIO base in the type — perfect for
/// QEMU's virt board, where the UART base is a compile-time constant. On
/// Apple Silicon the base is FDT-discovered and differs per SoC, so a
/// const generic cannot name it. `RuntimeS5lUart` carries the base as a
/// field instead, making it constructible after the FDT parser hands us
/// the address.
///
/// The same *spirit* as `Pl011`/`S5lUart`: the struct is one word (the
/// base), and a sentinel `base = 0` is the honest pre-`init` state. The
/// panic handler can still conjure one — but with base 0 every write
/// spins (the status check reads from address 0x10, which on Apple is
/// unmapped and faults). In practice the console is configured before
/// any code that can panic runs, matching how `board::virt::init()` works.
///
/// The register layout and bit semantics are identical to `S5lUart` —
/// this is the same hardware, just named at runtime. See the module docs
/// above for the register map and polarity conventions.
#[derive(Clone, Copy)]
pub struct RuntimeS5lUart {
    base: usize,
}

#[allow(dead_code)] // no Apple board wires this up yet — M7 continues
impl RuntimeS5lUart {
    /// Construct an s5l UART at the given virtual base address.
    ///
    /// The caller is responsible for ensuring `base` is the kernel's
    /// higher-half VA of the FDT-discovered physical base (via
    /// `phys_to_virt`), mapped Device-nGnRnE. Constructing with `base=0`
    /// yields a sentinel "not yet configured" instance; its methods are
    /// safe to call but will spin (status reads from address 0x10, which
    /// faults) — so set the real base before any print path runs.
    pub const fn new(base: usize) -> Self {
        Self { base }
    }

    /// The "not yet configured" sentinel. Methods on this instance are
    /// safe to call but will not produce output — they spin on a status
    /// read from an unmapped address. The boot stub's early output (if
    /// any) goes through m1n1's proxy hypervisor, not this console.
    pub const fn unconfigured() -> Self {
        Self { base: 0 }
    }

    /// Set the base address after FDT discovery. Called by
    /// `board::apple::init` (or the kmain path that wires the Apple
    /// console) once the FDT parser finds `compatible = "apple,s5l-uart"`.
    pub fn set_base(&mut self, base: usize) {
        self.base = base;
    }

    /// The current base address (0 if not yet configured).
    pub fn base(&self) -> usize {
        self.base
    }

    // Register offsets — same layout as S5lUart<const BASE>.
    const UTRSTAT_OFF: usize = 0x10;
    const UTXH_OFF: usize = 0x20;
    const URXH_OFF: usize = 0x24;

    const UTRSTAT_TX_EMPTY: u32 = 1 << 1;
    const UTRSTAT_RX_READY: u32 = 1 << 0;

    #[inline]
    fn status(&self) -> u32 {
        // SAFETY: UTRSTAT is a readable 32-bit device register at
        // base + 0x10. If base is 0 (unconfigured), this reads from
        // 0x10 — which faults on Apple (unmapped) and is benign on
        // QEMU (the UART page is at 0x0900_0000, so 0x10 is low RAM,
        // readable but not a UART). The caller contract is that the
        // base is set before the first print; the unconfigured
        // sentinel exists for static initialization, not for use.
        unsafe {
            ptr::with_exposed_provenance::<u32>(self.base + Self::UTRSTAT_OFF)
                .read_volatile()
        }
    }

    /// Transmit one byte. Spins while the TX buffer is non-empty.
    pub fn write_byte(self, b: u8) {
        while self.status() & Self::UTRSTAT_TX_EMPTY == 0 {
            core::hint::spin_loop();
        }
        // SAFETY: UTXH is a writable device register; TX_EMPTY was set.
        unsafe {
            ptr::with_exposed_provenance_mut::<u8>(self.base + Self::UTXH_OFF)
                .write_volatile(b)
        }
    }

    /// Receive one byte if one is waiting, else `None`. Never blocks.
    pub fn try_read_byte(self) -> Option<u8> {
        if self.status() & Self::UTRSTAT_RX_READY == 0 {
            return None;
        }
        // SAFETY: URXH is a readable device register; RX_READY was set.
        let data = unsafe {
            ptr::with_exposed_provenance::<u8>(self.base + Self::URXH_OFF)
                .read_volatile()
        };
        Some(data)
    }

    /// Receive one byte, spinning until one arrives.
    pub fn read_byte(self) -> u8 {
        loop {
            if let Some(b) = self.try_read_byte() {
                return b;
            }
            core::hint::spin_loop();
        }
    }
}

/// `core::fmt::Write` so `println!` works through the runtime-base console.
/// The Apple board's `Console` type alias points here once the FDT
/// parser has discovered the s5l UART base.
impl fmt::Write for RuntimeS5lUart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
        Ok(())
    }
}