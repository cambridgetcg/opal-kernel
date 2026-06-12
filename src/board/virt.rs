//! Board: QEMU aarch64 `virt` machine.
//!
//! QEMU's contract for this board is unusually small. Only two addresses
//! are documented as stable across QEMU versions:
//!
//! - flash at `0x0000_0000`
//! - RAM at `0x4000_0000`
//!
//! Everything else is *supposed* to be discovered from the devicetree blob
//! QEMU leaves at the start of RAM. We hardcode the UART base anyway —
//! it has not moved in many years, and milestone 0 has no FDT parser —
//! but we say so honestly: `UART0_BASE` is a convenience, not a contract.
//! A later milestone parses the DTB and this constant becomes a fallback.

use crate::hal::pl011::Pl011;

/// Start of RAM. Stable per QEMU's documentation. For a bare-metal ELF
/// boot QEMU places the devicetree blob here (because our link address,
/// 0x4020_0000, leaves it room — see linker.ld for the 2 MiB story).
pub const RAM_BASE: usize = 0x4000_0000;

/// Size of RAM. *Not* a stable board fact like `RAM_BASE`: it is whatever
/// `-m` says on the QEMU command line, so this constant must match the
/// `-m 512M` in `.cargo/config.toml` (a comment there points back here).
/// The honest source is the DTB's `/memory` node; when the FDT parser
/// lands (M4) this becomes a fallback, like `UART0_BASE` below.
pub const RAM_SIZE: usize = 512 * 1024 * 1024;

/// PL011 UART0 MMIO base. From QEMU's `hw/arm/virt.c` memory map; the DTB
/// is the authoritative source, this is the well-known value.
pub const UART0_BASE: usize = 0x0900_0000;

/// The board's console UART. Zero-sized — see `hal/pl011.rs` for why.
pub type Console = Pl011<UART0_BASE>;

/// Conjure the console. Free to call anywhere, even mid-panic.
pub const fn console() -> Console {
    Console::new()
}

/// Bring the board up. On QEMU virt this is a no-op — and honesty demands
/// we say so rather than perform fake work:
///
/// - the PL011 transmits without initialization under QEMU (see
///   `hal/pl011.rs`; real hardware would need baud/LCR/CR setup here);
/// - the MMU and caches stay off until the MMU milestone (M2);
/// - interrupts stay masked until the exceptions milestone (M1).
///
/// The function exists so `kmain` already has the right shape: boot stub →
/// board init → kernel main. `board/apple.rs::init()` will not be empty.
pub fn init() {}
