//! Synchronization primitives — born in M1, the first milestone with two
//! execution contexts.
//!
//! Until now the whole machine was one thread of control, and "no locks"
//! was simply true (hal/pl011.rs said so in writing). An exception
//! handler changes that: it is effectively a second thread that can
//! preempt the first between any two instructions, and the console is the
//! first thing both contexts want at once. So: a lock. One lock, the
//! smallest real one we can write, because the zero-dependency rule means
//! every spinlock Opal will ever have is hand-rolled right here on
//! `core::sync::atomic`.
//!
//! ## The lock itself
//!
//! One `AtomicBool`. Acquire by compare-exchanging false→true with
//! `Acquire` ordering (everything after the lock in program order is also
//! after it in memory order); release with a plain `Release` store (the
//! mirror image). Between failed attempts we spin on a plain *load* — the
//! "test-and-test-and-set" pattern — so a contended lock burns no
//! exclusive-access bus traffic while it waits.
//!
//! ## Why interrupts are masked while the lock is held
//!
//! A spinlock on one core has a textbook deadlock: `kmain` takes the
//! lock; an interrupt arrives; the handler wants the same lock; the
//! handler spins forever, because the only context that could ever
//! release the lock is the one frozen underneath it. The fix is equally
//! textbook: while you hold a lock that interrupt handlers also take, you
//! must not be interruptible — so [`SpinLock::with`] masks IRQ and FIQ
//! around the critical section. In M1 this is technically vacuous (DAIF
//! has been masked since reset; nothing unmasks it until M3's timer), but
//! a lock that is only correct until someone flips a bit in another file
//! is a trap, not a primitive.
//!
//! ## What masking cannot fix — the escape hatch
//!
//! Synchronous exceptions do not honor DAIF: if the code *holding* this
//! lock faults, the fault handler runs immediately, mid-critical-section.
//! If that handler then queued politely on the same lock, the report of
//! the fault would deadlock — the worst possible time to go silent. The
//! console therefore keeps an unlocked emergency path for exactly that
//! case (the `OOPS` flag in main.rs; Linux does the same thing with
//! `oops_in_progress` and `bust_spinlocks()`). Possibly-garbled output
//! beats guaranteed silence.
//!
//! ## An honesty note: atomics with the MMU off
//!
//! `compare_exchange` compiles to load/store-exclusive (LDXR/STXR) on
//! this target, and on *real* AArch64 hardware exclusives are only
//! guaranteed to make progress on cacheable memory — with the MMU and
//! caches off they may fail forever. QEMU's TCG does not model that
//! restriction, so this lock works fine today, on the simulator. That is
//! a labeled simulator-ism, exactly like the PL011's missing init: M2
//! turns the MMU on long before this code meets real silicon in M7.

use core::sync::atomic::{AtomicBool, Ordering};

/// A minimal spinlock with a closure API: `LOCK.with(|| ...)`.
///
/// No data inside and no RAII guard, on purpose: the only user so far
/// (the console path in main.rs) guards a zero-sized device handle, so a
/// generic `SpinLock<T>` would be an abstraction nobody has earned yet.
/// When a second user with actual data shows up — the scheduler's run
/// queue, at the latest — the guard version gets written against a real
/// need instead of an imagined one.
pub struct SpinLock {
    locked: AtomicBool,
}

impl SpinLock {
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
        }
    }

    /// Run `f` with the lock held and asynchronous interrupts masked.
    ///
    /// Interrupt state is saved and *restored*, not blindly re-enabled:
    /// if interrupts were already masked on entry (today they always
    /// are), they stay masked on exit.
    pub fn with<R>(&self, f: impl FnOnce() -> R) -> R {
        // Mask first, then take the lock — the other order leaves a
        // window where we hold the lock and can still be interrupted.
        let daif = crate::arch::aarch64::daif_mask_save();

        // The *weak* compare-exchange may fail spuriously on LL/SC
        // machines like this one — fine, it sits in a retry loop anyway,
        // and the weak form is exactly one LDXR/STXR pair instead of a
        // nested loop. Failure ordering is Relaxed: a failed acquire
        // synchronizes with nothing.
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Wait with cheap read-only loads, retry the exclusive pair
            // only once the lock looks free.
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }

        let result = f();

        // Release: pairs with the Acquire above, so everything written
        // inside the critical section is visible to the next acquirer.
        // Order matters here too, mirroring the acquire side: drop the
        // lock BEFORE restoring interrupts. Restoring first would reopen
        // the single-core deadlock the moment M3 unmasks interrupts — a
        // handler arriving in that window finds the lock still held, by
        // the very context frozen beneath it.
        self.locked.store(false, Ordering::Release);
        crate::arch::aarch64::daif_restore(daif);
        result
    }
}
