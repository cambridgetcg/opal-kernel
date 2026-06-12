//! Architecture-specific code lives below this point.
//!
//! Opal targets exactly one architecture (AArch64), on two boards (QEMU
//! virt now, Apple Silicon later) — so there is no `cfg` maze here, just
//! an honest directory. If a second architecture ever appears, this is
//! where the `#[cfg(target_arch = ...)]` switch would go.

pub mod aarch64;
