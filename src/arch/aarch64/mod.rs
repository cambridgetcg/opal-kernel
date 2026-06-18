//! AArch64-specific code: the boot stub, the exception vectors, the MMU,
//! and small CPU helpers.
//!
//! Everything in this directory is allowed to use `core::arch::asm!` and
//! to know AArch64 system-register names. Nothing outside it is. That is
//! why the DAIF helpers below exist at all: `sync.rs` wants to mask
//! interrupts around a critical section, and this is the only place
//! allowed to know how.

pub mod boot;
pub mod fdt;
pub mod mmu;
pub mod timer;
pub mod vectors;

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

/// Mask asynchronous interrupts (IRQ and FIQ) and return the previous
/// PSTATE.DAIF, so the caller can put things back exactly as they were.
///
/// Save-and-restore rather than mask-and-enable matters: if interrupts
/// were already masked (today they always are — nothing has unmasked them
/// since reset), a nested critical section must not accidentally enable
/// them on the way out. `DAIFSet` is a write-1-to-mask register; the
/// immediate's four bits are D, A, I, F from high to low, so #3 = I | F.
/// D (debug) and A (SError) stay as they are.
pub fn daif_mask_save() -> u64 {
    let daif: u64;
    // SAFETY: reading DAIF is side-effect-free; masking I and F at EL1 is
    // always legal and cannot fault. No `nomem`: the compiler must not
    // float memory accesses across an interrupt-masking boundary.
    unsafe {
        core::arch::asm!(
            "mrs {prev}, DAIF",   // remember the old mask bits
            "msr DAIFSet, #3",    // mask IRQ (I) and FIQ (F)
            prev = out(reg) daif,
            options(nostack, preserves_flags)
        );
    }
    daif
}

/// Restore PSTATE.DAIF from a value previously returned by
/// [`daif_mask_save`]. The pair brackets every spinlock critical section.
pub fn daif_restore(daif: u64) {
    // SAFETY: writing DAIF at EL1 is always legal; the value round-trips
    // from the matching `mrs` (bits [9:6] carry D/A/I/F in both
    // directions). No `nomem`, same reason as above.
    unsafe {
        core::arch::asm!(
            "msr DAIF, {prev}",
            prev = in(reg) daif,
            options(nostack, preserves_flags)
        );
    }
}

/// Where is this code executing *right now*? `adr` computes an address
/// PC-relatively, so the answer is the truth about the program counter —
/// since M2, the banner uses it to prove kmain really runs in the higher
/// half, rather than asserting it.
pub fn here() -> u64 {
    let pc: u64;
    // SAFETY: adr is pure address arithmetic on the PC; it cannot fault
    // or change state.
    unsafe {
        core::arch::asm!("adr {pc}, .", pc = out(reg) pc,
                         options(nomem, nostack, preserves_flags));
    }
    pc
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
