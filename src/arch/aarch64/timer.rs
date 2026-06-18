//! The AArch64 architectural timer — the kernel's heartbeat.
//!
//! Every AArch64 core has a *generic timer*: a counter that ticks at a
//! fixed frequency, plus a comparator that fires an interrupt when the
//! counter reaches a programmed value. There are actually four timers
//! per core (secure physical, non-secure physical, virtual, and
//! hypervisor), and we use the **non-secure physical timer** — the one
//! the DTB lists as PPI 30, and the one Linux uses for EL1 ticking.
//!
//! ## The three registers that matter
//!
//! - `CNTFRQ_EL0` — the counter frequency in Hz. Read-only at EL1 if
//!   the firmware programmed it; on QEMU it is 62.5 MHz. We read it at
//!   init to set the tick interval, never hardcode it.
//! - `CNTP_CTL_EL0` — the timer's control: bit 0 = enable, bit 1 = the
//!   interrupt status (read-only: 1 = fired, and the timer auto-clears
//!   the condition when you write a new compare value), bit 2 = the
//!   "auto-reload" mode (0 = one-shot, 1 = periodic via `CNTKPTVAL`).
//! - `CNTP_CVAL_EL0` — the compare value. When `CNTVCT_EL0` >= this,
//!   the timer fires. For a periodic tick, the handler writes
//!   `current_count + period` after each fire.
//!
//! (There is also `CNTP_TVAL_EL0` — a *relative* timer that holds the
//! number of ticks until the next fire, but `CVAL` is the cleaner model:
//! one absolute comparison, no "how much time elapsed during the
//! handler" arithmetic.)
//!
//! ## Why the physical timer, not the virtual timer
//!
//! At EL1 both work. The virtual timer (`CNTV_*`) counts against the
//! virtual offset, which is zero until a hypervisor sets it — so at
//! EL1 without virtualization, virtual == physical. Linux uses the
//! virtual timer under a hypervisor (so the host controls time) and
//! the non-secure physical timer in bare metal. We are bare metal (or
//! under m1n1 which runs us at EL2/EL1 without timer virtualization),
//! so the non-secure physical timer is the honest choice. The DTB
//! confirms: PPI 30 is `arm,arch-timer`'s "non-secure physical" entry.
//!
//! ## The interrupt path
//!
//! The timer fires → GIC presents IRQ 30 as a PPI → the CPU takes an
//! IRQ exception → `handle_irq` in vectors.rs acknowledges the GIC
//! → dispatches to [`on_tick`] here → [`end_interrupt`] on the GIC →
//! `eret` back to interrupted code. The handler is the first exception
//! in Opal that is **routine** — it runs, does its job, and returns,
//! every time.

/// The timer's PPI interrupt ID, as the GIC sees it. Matches
/// `hal::gicv3::TIMER_IRQ`; duplicated here so this file is
/// self-contained (the GIC constant is the contract; this is the
/// local copy the timer references in comments).
pub const TIMER_IRQ: u32 = 30;

/// Read the counter frequency, in Hz. Set by firmware (QEMU: 62.5 MHz;
/// real hardware: the bootloader, or the DTB's `clock-frequency`).
pub fn frequency() -> u64 {
    let freq: u64;
    // SAFETY: reading CNTFRQ_EL0 is side-effect-free and always legal.
    unsafe {
        core::arch::asm!(
            "mrs {freq}, CNTFRQ_EL0",
            freq = out(reg) freq,
            options(nomem, nostack, preserves_flags),
        );
    }
    freq
}

/// Read the current virtual count. This is the free-running counter
/// that the virtual timer compares against. We use CNTVCT_EL0 (the
/// virtual count) because we arm CNTV_CVAL_EL0 — the virtual timer's
/// compare register. At EL1 without a hypervisor, the virtual offset
/// is zero, so the virtual count equals the physical count.
pub fn counter() -> u64 {
    let cnt: u64;
    // SAFETY: reading CNTVCT_EL0 is side-effect-free. The `isb` first
    // ensures we get a synchronized read.
    unsafe {
        core::arch::asm!(
            "isb",
            "mrs {cnt}, CNTVCT_EL0",
            cnt = out(reg) cnt,
            options(nomem, nostack, preserves_flags),
        );
    }
    cnt
}

/// Disable the virtual timer. Clears CNTV_CTL_EL0. Called before
/// reprogramming so the timer does not fire mid-setup.
pub fn disable() {
    // SAFETY: writing CNTV_CTL_EL0 is the standard timer control.
    unsafe {
        core::arch::asm!(
            "msr CNTV_CTL_EL0, {val}",
            val = in(reg) 0u64,
            options(nostack, preserves_flags),
        );
    }
}

/// Arm the virtual timer to fire after `ticks` from now. Sets the
/// compare value to `current_count + ticks` and enables the timer.
pub fn arm(ticks: u64) {
    let next = counter() + ticks;
    // SAFETY: writing CNTV_CVAL_EL0 sets the compare value; writing
    // CNTV_CTL_EL0 with bit 0 set enables the timer. Program CVAL
    // before enabling, so the timer does not fire against a stale
    // compare value.
    unsafe {
        core::arch::asm!(
            "msr CNTV_CVAL_EL0, {next}",
            next = in(reg) next,
            options(nostack, preserves_flags),
        );
        // Bit layout of CNTV_CTL_EL0:
        //   [0] ENABLE
        //   [1] IMASK  (1 = mask the interrupt)
        //   [2] ISTATUS (read-only: 1 = condition met)
        // We want ENABLE=1, IMASK=0.
        core::arch::asm!(
            "msr CNTV_CTL_EL0, {ctl}",
            ctl = in(reg) 1u64,
            options(nostack, preserves_flags),
        );
    }
}

/// Re-arm the virtual timer for the next tick. Called from the IRQ
/// handler after each fire: write a new compare value `period` ticks
/// in the future.
pub fn rearm(period: u64) {
    let next = counter() + period;
    // SAFETY: same as `arm`, but the timer is already enabled. Writing
    // CVAL is sufficient — the ISTATUS bit clears automatically when the
    // comparator accepts a future value.
    unsafe {
        core::arch::asm!(
            "msr CNTV_CVAL_EL0, {next}",
            next = in(reg) next,
            options(nostack, preserves_flags),
        );
    }
}

/// The kernel's tick counter — incremented by every timer interrupt.
/// Reading it tells the monitor (and any future scheduler) how many
/// ticks have elapsed since interrupts were enabled.
static TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The timer period in counter ticks, set by `kmain` before it arms the
/// timer and unmask IRQs. The IRQ handler reads this to re-arm the next
/// tick. Zero means "not ticking yet" — the handler skips the re-arm,
/// which is correct during GIC init (before the timer is configured).
pub static TICK_PERIOD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Called from the IRQ handler on every timer interrupt. Bumps the
/// tick counter and re-arms the timer for the next period. This is the
/// "heartbeat" — the first code in Opal that runs because the hardware
/// poked it, not because the monitor loop did.
pub fn on_tick() {
    TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// How many ticks have elapsed since the timer was started.
pub fn ticks() -> u64 {
    TICKS.load(core::sync::atomic::Ordering::Relaxed)
}
