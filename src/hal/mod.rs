//! Opal's hardware abstraction layer — deliberately tiny.
//!
//! A "HAL" here means: drivers for specific hardware blocks, written so
//! that the rest of the kernel never needs a hardware manual open. It does
//! NOT mean trait soup. We will grow abstractions when a second
//! implementation forces us to, and not one milestone earlier. Today there
//! is exactly one driver:
//!
//! - [`pl011`] — the ARM PL011 UART found on QEMU's virt board.
//!
//! When the Apple Silicon port lands (see ROADMAP.md), this directory
//! gains `s5l_uart.rs` (Samsung-style UART, Linux binding `apple,s5l-uart`)
//! and `aic.rs` (Apple Interrupt Controller). At that point — with two
//! real UARTs in hand — a minimal `Console` trait may earn its keep.

pub mod aic;
pub mod gicv2;
pub mod pl011;
pub mod s5l_uart;
