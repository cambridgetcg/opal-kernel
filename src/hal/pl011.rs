//! ARM PL011 UART driver — the serial port of QEMU's virt board.
//!
//! A UART turns bytes into voltage wiggles on a wire; QEMU turns those
//! wiggles into characters on your terminal. Two registers are enough for
//! a polled driver (offsets from the ARM PL011 TRM, DDI 0183):
//!
//! | offset | name   | role                                          |
//! |--------|--------|-----------------------------------------------|
//! | 0x000  | UARTDR | data: write = transmit, read = receive        |
//! | 0x018  | UARTFR | flags: bit 5 TXFF (TX full), bit 4 RXFE (RX empty) |
//!
//! ## MMIO ground rules (read once, they apply to every driver we write)
//!
//! Device registers are *memory-mapped*: they live at physical addresses,
//! but they are not memory. Reading can have side effects (reading DR pops
//! the RX FIFO); writes must actually happen, in order. So:
//!
//! - every access is `read_volatile` / `write_volatile` — the compiler may
//!   not elide, merge, or reorder volatile accesses relative to each other;
//! - we keep **raw pointers** end to end and never create `&`/`&mut` to
//!   device memory (a Rust reference promises "this is dereferenceable
//!   plain memory and nobody else is touching it" — both false for MMIO);
//! - pointers are built with the provenance APIs
//!   (`ptr::with_exposed_provenance*`), the sanctioned way to conjure a
//!   pointer from an integer address. MMIO is outside any Rust allocation,
//!   so provenance is documented to be irrelevant here;
//! - access width matches the register: PL011 registers are 32-bit, so we
//!   move `u32`s even when only the low byte matters.
//!
//! ## Why is there *still* no lock in this file?
//!
//! In M0 the honest answer was "nothing to lock against": one core,
//! interrupts off, concurrency impossible by construction. M1 made the
//! threat real — an exception handler can print while `kmain` is printing
//! — and the promised lock landed, but one level *up*: `console_print` in
//! main.rs takes a `SpinLock` (with interrupt masking) around each whole
//! `write_fmt`, so messages interleave but bytes within them never do.
//! (M0's comment predicted "a type alias, not every call site" would
//! change; the truth was even smaller — one function changed.) The driver
//! itself stays lock-free and zero-sized, deliberately:
//!
//! - locking per *message* beats locking per *byte* — and a lock inside
//!   `Pl011` would drag the monitor's blocking `read_byte` loop into a
//!   critical section, the classic way to deadlock a console;
//! - the fault/panic path must be able to print while the lock is held by
//!   the very code that faulted. A bare, conjurable-anywhere `Pl011` is
//!   exactly that escape hatch (the `OOPS` flag in main.rs, and the whole
//!   deadlock story in docs/03-exceptions.md).
//!
//! ## Why is there no init code?
//!
//! QEMU's PL011 model transmits even with the UART disabled (its source
//! keeps this behavior because too much guest code depends on it), and the
//! FIFO flags reset to "empty". Real PL011 hardware absolutely requires
//! baud-rate/LCR/CR setup — that lives in `board::init()`'s future, not
//! here, and must not be forgotten if Opal ever meets a physical PL011.

use core::fmt;
use core::ptr;

/// A PL011 at base address `BASE`.
///
/// Zero-sized and `Copy`: the base address lives in the *type*, so a
/// console can be conjured anywhere — including inside the panic handler,
/// where we cannot rely on any shared state being intact.
#[derive(Clone, Copy)]
pub struct Pl011<const BASE: usize>;

impl<const BASE: usize> Pl011<BASE> {
    const DR: usize = BASE; // data register
    const FR: usize = BASE + 0x18; // flag register

    const FR_TXFF: u32 = 1 << 5; // TX FIFO full — do not write DR
    const FR_RXFE: u32 = 1 << 4; // RX FIFO empty — nothing to read

    pub const fn new() -> Self {
        Self
    }

    /// Read the flag register.
    #[inline]
    fn flags(self) -> u32 {
        // SAFETY: FR is a readable 32-bit device register on this board,
        // reachable at BASE for the program's lifetime. (M0/M1 said "MMU
        // off: physical == virtual" here; since M2, BASE is whichever
        // alias the instantiator may legally touch — the higher-half VA
        // that mmu.rs maps Device-nGnRnE for the kernel proper, or the
        // raw physical base for the boot stub's pre-condemnation world.)
        unsafe { ptr::with_exposed_provenance::<u32>(Self::FR).read_volatile() }
    }

    /// Transmit one byte. Spins while the TX FIFO is full.
    pub fn write_byte(self, b: u8) {
        while self.flags() & Self::FR_TXFF != 0 {
            core::hint::spin_loop();
        }
        // SAFETY: DR is a writable 32-bit device register; TXFF was clear,
        // so the FIFO has room.
        unsafe { ptr::with_exposed_provenance_mut::<u32>(Self::DR).write_volatile(b as u32) }
    }

    /// Receive one byte if one is waiting, else `None`. Never blocks.
    pub fn try_read_byte(self) -> Option<u8> {
        if self.flags() & Self::FR_RXFE != 0 {
            return None;
        }
        // SAFETY: DR is readable; RXFE was clear, so the FIFO has a byte.
        // Reading DR pops it — a side effect, hence volatile.
        let data = unsafe { ptr::with_exposed_provenance::<u32>(Self::DR).read_volatile() };
        Some(data as u8) // low 8 bits are the character; 11:8 are error flags
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

impl<const BASE: usize> Default for Pl011<BASE> {
    fn default() -> Self {
        Self::new()
    }
}

/// Hooking into `core::fmt` is what makes `println!("{x:#x}")` work: the
/// format machinery does all the rendering and hands us plain `&str`s.
impl<const BASE: usize> fmt::Write for Pl011<BASE> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            // Serial terminals want CRLF ("\r\n") line endings; Rust code
            // writes "\n". Translate here so no caller ever thinks about it.
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
        Ok(())
    }
}
