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