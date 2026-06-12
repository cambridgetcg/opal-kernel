//! Board definitions: which hardware exists, and where.
//!
//! A *board* is the thing that knows addresses. Drivers in `hal/` know how
//! a device block works; the board knows that this machine has one, where
//! it is, and how to bring the machine up. Today there is one board:
//!
//! - [`virt`] — QEMU's aarch64 `virt` machine, our daily driver.
//!
//! `board/apple.rs` will join it for the m1n1 bring-up milestone (see
//! ROADMAP.md and docs/02-hal-and-apple-silicon.md). Its job will be
//! noticeably harder than virt's: the UART base must come from the
//! devicetree (it differs per SoC) rather than a constant, the console is
//! a Samsung-style s5l UART, and init runs at EL2 with a locked RVBAR.
//! That asymmetry is exactly why addresses live here and not in drivers.

pub mod virt;
