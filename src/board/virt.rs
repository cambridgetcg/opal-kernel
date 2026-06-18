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
//! it has not moved in many years, and the FDT parser is still milestones
//! away (M4) — but we say so honestly: `UART0_BASE` is a convenience, not
//! a contract.
//! A later milestone parses the DTB and this constant becomes a fallback.
//!
//! Since M2 the board's addresses come in two flavors: the *physical*
//! ones below (what the bus knows, what the page tables map — boards know
//! addresses, `arch` maps them), and the higher-half aliases the running
//! kernel actually dereferences, built with [`mmu::phys_to_virt`].

use crate::arch::aarch64::mmu;
use crate::hal::pl011::Pl011;

/// Start of RAM, physical. Stable per QEMU's documentation. For a
/// bare-metal ELF boot QEMU places the devicetree blob here (because our
/// load address, 0x4020_0000, leaves it room — see linker.ld for the
/// 2 MiB story). The kernel reads it through the higher-half alias, where
/// M2's page tables map the window read-only.
pub const RAM_BASE: usize = 0x4000_0000;

/// Size of RAM. *Not* a stable board fact like `RAM_BASE`: it is whatever
/// `-m` says on the QEMU command line, so this constant must match the
/// `-m 512M` in `.cargo/config.toml` (a comment there points back here).
/// The honest source is the DTB's `/memory` node; when the FDT parser
/// lands (M4) this becomes a fallback, like `UART0_BASE` below. Two
/// things lean on it since M2: how much RAM the page tables map, and
/// where the bus-error window sits (mmu.rs maps the 32 MiB *past*
/// `RAM_BASE + RAM_SIZE` as Device memory precisely so reads there fail
/// on the bus, not in the walk — raise `-m` without raising this and
/// that window quietly becomes mappable RAM).
pub const RAM_SIZE: usize = 512 * 1024 * 1024;

/// PL011 UART0 MMIO base, physical. From QEMU's `hw/arm/virt.c` memory
/// map; the DTB is the authoritative source, this is the well-known
/// value. This is the address the page tables map (one Device-nGnRnE
/// page — mmu.rs, `l3_mmio`) and the one the boot stub's low world uses
/// raw; everything after the move upstairs goes through [`UART0_VA`].
pub const UART0_BASE: usize = 0x0900_0000;

/// The UART as the kernel sees it: the higher-half alias of
/// [`UART0_BASE`]. Forced, not optional, since `kmain` condemns the low
/// half — a console at the physical address would fault on its first
/// byte after that.
pub const UART0_VA: usize = mmu::phys_to_virt(UART0_BASE);

/// The board's console UART. Zero-sized — see `hal/pl011.rs` for why,
/// and for where M1's console lock actually lives (main.rs, around the
/// print path — not in the driver, and not in this alias).
pub type Console = Pl011<UART0_VA>;

/// Conjure the console. Free to call anywhere, even mid-panic.
pub const fn console() -> Console {
    Console::new()
}

/// Bring the board up. On QEMU virt this is a no-op — and honesty demands
/// we say so rather than perform fake work:
///
/// - the PL011 transmits without initialization under QEMU (see
///   `hal/pl011.rs`; real hardware would need baud/LCR/CR setup here);
/// - the MMU and caches are already ON when this runs: M0/M1's "off until
///   M2" debt was paid by the boot stub, which builds the page tables and
///   climbs to the higher half before any of `kmain` exists
///   (arch/aarch64/mmu.rs, docs/04);
/// - the exception vector table is installed by `kmain` (it is
///   architecture state, not board state — arch/aarch64/vectors.rs), and
///   interrupts stay *masked* until the timer milestone (M3): M1 catches
///   synchronous faults; nothing asynchronous is enabled yet.
///
/// The function exists so `kmain` already has the right shape: boot stub →
/// board init → kernel main. `board/apple.rs::init()` will not be empty.
pub fn init() {}
