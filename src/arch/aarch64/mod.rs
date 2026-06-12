//! AArch64-specific code: the boot stub and small CPU helpers.
//!
//! Everything in this directory is allowed to use `core::arch::asm!` and
//! to know AArch64 system-register names. Nothing outside it is.

pub mod boot;

/// Which exception level are we running at? Reads the `CurrentEL` system
/// register; the EL number lives in bits [3:2].
///
/// Expected answers: 1 on QEMU virt (no EL2/EL3 by default), 2 on Apple
/// Silicon via m1n1 (Apple implements no EL3; iBoot enters at EL2).
pub fn current_el() -> u64 {
    let el: u64;
    // SAFETY: reading CurrentEL has no side effects and is always legal
    // at EL1 and above.
    unsafe {
        core::arch::asm!(
            "mrs {el}, CurrentEL",
            el = out(reg) el,
            options(nomem, nostack, preserves_flags)
        );
    }
    (el >> 2) & 0b11
}

/// Stop this core forever, in the lowest-power way available.
///
/// `wfe` (wait-for-event) sleeps the core until something pokes it; the
/// loop is because events (or spurious wakeups) do arrive and we have
/// nowhere to go. Used by the panic handler and as the end of the world.
pub fn park() -> ! {
    loop {
        // SAFETY: wfe is a hint instruction; it cannot fault or change state.
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}
