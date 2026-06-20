//! M6 — scheduler and IPC: the kernel learns to share.
//!
//! M5 proved the EL0 boundary: one task, one address space, syscalls
//! that trap and return. M6 adds the *plural*: multiple tasks, each with
//! its own user page tables and saved register state, and a scheduler
//! that decides who runs next.
//!
//! ## This first beat: the Task control block
//!
//! This file defines the data structures the scheduler is built on — no
//! context switch assembly yet, no wiring into the syscall handler. The
//! next beat writes the `eret`-based context switch that swaps TTBR0
//! and restores a saved TrapFrame; the beat after that wires `yield`
//! to actually switch tasks. Building the scaffolding first means the
//! switch code has something concrete to operate on.
//!
//! ## The design
//!
//! - **Array, not linked list.** No allocator. `MAX_TASKS` is a compile
//!   constant; the task table is `.bss`, zeroed by boot. A task's "TID"
//!   is its index in this array.
//!
//! - **Saved context is a TrapFrame.** The same 288-byte struct the
//!   exception stubs build — it already holds x0..x30, sp, elr, spsr.
//!   Context switch is "save the current TrapFrame, load the next one,
//!   eret" — the same hardware path M5 uses for the single-task yield,
//!   just with a *different* frame.
//!
//! - **Per-task user page tables.** Each Task holds the PA of its user
//!   L0 root (the value for TTBR0_EL1). Context switch swaps TTBR0.
//!   The kernel's TTBR1 tree is shared and never changes.
//!
//! - **State machine: Ready → Running → (Exited).** A simple lifecycle.
//!   M6's IPC may add Blocked; preemptive scheduling (M6's second half)
//!   adds the timer-driven Ready→Running transition.

use crate::arch::aarch64::vectors::TrapFrame;

// ---------------------------------------------------------------------------
// Task lifecycle
// ---------------------------------------------------------------------------

/// Where a task is in its life.
///
/// The transitions a single-task kernel makes are trivial (one task,
/// always Running), but writing them down now means the scheduler's
/// logic is honest about what it's doing when tasks multiply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    /// The task has called `exit` or was killed on a fault. Its slot
    /// in the table is available for reuse — the scheduler will zero
    /// the Task on allocation, so no stale state leaks.
    ///
    /// This variant is deliberately first (discriminant 0) so that a
    /// zeroed `.bss` — what the boot stub produces — reads as Exited,
    /// and `alloc_task()` finds free slots without any explicit init.
    Exited,
    /// The task exists and can run, but is not currently running.
    /// It is in the ready queue, waiting to be picked.
    Ready,
    /// The task is currently executing on the CPU. At most one task
    /// is Running at any time (single-core).
    Running,
    /// The task is waiting on something (a timer, an IPC message).
    /// Not in the ready queue; the thing it waits for will wake it
    /// back to Ready. Unused until M6's IPC half, but present so the
    /// scheduler can talk about "why isn't this runnable?" honestly.
    Blocked,
}

// ---------------------------------------------------------------------------
// The Task control block
// ---------------------------------------------------------------------------

/// One task's complete state — everything the scheduler needs to pause
/// it and resume it later.
///
/// The `saved` field is the heart of context switch: it holds the
/// register file, SP, ELR, and SPSR exactly as the exception stub left
/// them when the task last `svc`'d. To resume the task, the context
/// switch loads this frame and `eret`s — the same path M5's single-task
/// yield takes, just from a different frame.
///
/// `ttbr0_pa` is the physical address of the task's user L0 page-table
/// root. Context switch writes this to TTBR0_EL1, giving each task its
/// own address space. The kernel's TTBR1 is shared and untouched.
#[repr(C)]
pub struct Task {
    /// The saved CPU state — what to restore on resume. This is the
    /// same TrapFrame the exception stubs build, so the context switch
    /// is "store the current frame here, load this one, eret."
    pub saved: TrapFrame,
    /// The PA of this task's user page-table root (for TTBR0_EL1).
    /// Zero means "no user space" (a kernel-only task, if we ever
    /// make one).
    pub ttbr0_pa: u64,
    /// Where this task is in its lifecycle.
    pub state: TaskState,
    /// A tiny, human-readable name for the task — set at creation,
    /// printed in scheduler diagnostics. 8 bytes so it fits in two
    /// stores and never needs an allocator.
    pub name: [u8; 8],
}

impl Task {
    /// Create a Task in the Exited state — a free slot. The scheduler
    /// allocates by finding an Exited slot and overwriting it.
    pub const fn empty() -> Self {
        Task {
            saved: TrapFrame::empty(),
            ttbr0_pa: 0,
            state: TaskState::Exited,
            name: [0; 8],
        }
    }

    /// Set the task's name from a byte slice. Truncates to 8 bytes.
    pub fn set_name(&mut self, s: &[u8]) {
        let n = s.len().min(8);
        self.name[..n].copy_from_slice(&s[..n]);
        for i in n..8 {
            self.name[i] = 0;
        }
    }

    /// Get the task's name as a &str (up to the first NUL).
    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(8);
        core::str::from_utf8(&self.name[..end]).unwrap_or("?")
    }
}

// ---------------------------------------------------------------------------
// The task table and the scheduler
// ---------------------------------------------------------------------------

/// Maximum number of concurrent tasks. Small — this is a teaching
/// kernel, and the table is statically allocated in .bss. Each Task is
/// 288 (TrapFrame) + 8 + 1 + 8 = ~305 bytes; 8 tasks is ~2.4 KiB.
pub const MAX_TASKS: usize = 8;

/// The global task table. Statically allocated, zeroed by boot. Index
/// in this array IS the task's TID (Task ID). Slot 0 is reserved for the
/// kernel itself (it never appears in the ready queue); user tasks
/// start at TID 1.
static mut TASK_TABLE: [Task; MAX_TASKS] = [const { Task::empty() }; MAX_TASKS];

/// The currently-running task's TID, or 0 if the kernel is running
/// (no user task active). The scheduler reads this to know who to
/// save context into.
static mut CURRENT_TID: usize = 0;

/// The scheduler itself. Holds the ready queue as a simple ring of
/// TIDs — no linked lists, no allocator. The scheduler picks the next
/// Ready task in round-robin order.
pub struct Scheduler {
    /// TIDs of tasks that are Ready, in the order they became ready.
    /// A simple FIFO ring buffer.
    ready_queue: [usize; MAX_TASKS],
    /// Index of the head (next to dequeue).
    head: usize,
    /// Number of tasks currently in the queue.
    count: usize,
}

impl Scheduler {
    /// Create an empty scheduler.
    pub const fn new() -> Self {
        Scheduler {
            ready_queue: [0; MAX_TASKS],
            head: 0,
            count: 0,
        }
    }

    /// Enqueue a task's TID as Ready. The task must be in the Exited or
    /// Running state (transitioning to Ready). Returns false if the
    /// queue is full.
    pub fn enqueue(&mut self, tid: usize) -> bool {
        if self.count >= MAX_TASKS {
            return false;
        }
        let tail = (self.head + self.count) % MAX_TASKS;
        self.ready_queue[tail] = tid;
        self.count += 1;
        true
    }

    /// Dequeue the next Ready task's TID, or None if the queue is empty.
    /// This is the round-robin pick.
    pub fn dequeue(&mut self) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let tid = self.ready_queue[self.head];
        self.head = (self.head + 1) % MAX_TASKS;
        self.count -= 1;
        Some(tid)
    }

    /// How many tasks are waiting.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Is the ready queue empty?
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// The one and only scheduler instance.
static mut SCHEDULER: Scheduler = Scheduler::new();

// ---------------------------------------------------------------------------
// Public scheduler API
// ---------------------------------------------------------------------------

/// Get a mutable reference to the global scheduler.
///
/// # Safety
/// Single-core, no preemption yet — there is exactly one execution
/// context that can touch the scheduler. Once M6 adds preemptive
/// scheduling (timer-driven), this must be called with interrupts
/// masked.
pub unsafe fn scheduler() -> &'static mut Scheduler {
    // SAFETY: single-core, no preemption yet. See the safety note above.
    unsafe { &mut *(&raw mut SCHEDULER) }
}

/// Get a reference to a task by TID, or None if the TID is out of range
/// or the slot is Exited.
///
/// # Safety
/// Same single-core invariant as [`scheduler`].
pub unsafe fn task(tid: usize) -> Option<&'static mut Task> {
    if tid >= MAX_TASKS {
        return None;
    }
    // SAFETY: tid is in bounds; single-core, no concurrent access.
    let t = unsafe { &raw mut TASK_TABLE[tid] };
    // SAFETY: dereferencing the in-bounds pointer per the single-core rule.
    if unsafe { (*t).state } == TaskState::Exited {
        return None;
    }
    // SAFETY: same invariant; returning a mutable alias to a slot that is
    // not Exited — the caller owns it for the duration of its use.
    Some(unsafe { &mut *t })
}

/// Allocate a new task slot. Returns the TID of the freshly-allocated
/// (Exited-state) task, or None if the table is full.
///
/// # Safety
/// Single-core, interrupts masked (the caller — syscall handler —
/// runs with DAIF set).
pub unsafe fn alloc_task() -> Option<usize> {
    for tid in 1..MAX_TASKS {
        // SAFETY: tid is in bounds; single-core, no concurrent access.
        let t = unsafe { &raw mut TASK_TABLE[tid] };
        // SAFETY: dereferencing the in-bounds pointer.
        if unsafe { (*t).state } == TaskState::Exited {
            // SAFETY: same invariant; we own this slot now.
            unsafe { *t = Task::empty() };
            return Some(tid);
        }
    }
    None
}

/// The TID of the currently-running task, or 0 for "kernel" (no user
/// task active).
pub fn current_tid() -> usize {
    // SAFETY: read of a usize — atomic enough on aarch64 for this read,
    // and single-core means no tearing. The static is only written from
    // the scheduler, which is single-threaded.
    unsafe { core::ptr::read_volatile(&raw const CURRENT_TID) }
}

/// Set the current TID. Called by the context switch.
///
/// # Safety
/// Must be called with interrupts masked and the old task's context
/// already saved.
pub unsafe fn set_current_tid(tid: usize) {
    unsafe { core::ptr::write_volatile(&raw mut CURRENT_TID, tid) }
}

/// Spawn a new user task: allocate a slot, set its name, user page-table
/// root, and initial saved register state (entry point, SP, SPSR for EL0).
/// The task starts in the Ready state and is enqueued.
///
/// Returns the TID, or None if the table is full.
///
/// # Safety
/// Single-core, interrupts masked (called from the monitor before
/// dropping to EL0).
pub unsafe fn spawn(name: &[u8], ttbr0_pa: u64, entry: u64, sp: u64) -> Option<usize> {
    let tid = unsafe { alloc_task()? };
    // SAFETY: tid just allocated, in bounds. alloc_task left the slot
    // in the Exited state; we set it to Ready below. We can't use
    // task() here because task() returns None for Exited slots.
    let t = unsafe { &mut *(&raw mut TASK_TABLE[tid]) };
    t.saved = TrapFrame::empty();
    t.saved.elr = entry; // eret resumes here in EL0
    t.saved.sp = sp; // SP_EL0
    // SPSR for EL0: M=0b0000, bit[4]=0 (AArch64), DAIF=0b1111 at [9:6].
    // Same value as user.rs's drop_to_el0_with.
    t.saved.spsr = 0x3C0;
    t.ttbr0_pa = ttbr0_pa;
    t.state = TaskState::Ready;
    t.set_name(name);
    unsafe { scheduler() }.enqueue(tid);
    Some(tid)
}

// ---------------------------------------------------------------------------
// M6 context switch — the heart of the scheduler
// ---------------------------------------------------------------------------

/// Save the current task's register state and switch to the next Ready task.
///
/// This is the M6 context switch. It is called from the `yield` syscall
/// handler ([`crate::arch::aarch64::user::handle_svc_from_el0`]) with the
/// trap frame that the exception stub built on the kernel stack — the
/// frame that `__vectors_restore` is about to reload and `eret` from.
///
/// The switch is, mechanically, three copies and a TTBR0 swap:
///
/// 1. **Save**: copy the frame *off* the stack into the current task's
///    `saved` field. The frame on the stack is about to be overwritten.
/// 2. **Pick**: dequeue the next Ready task (round-robin). If there is
///    no other task, return — the current task just keeps running (the
///    single-task yield, same as M5).
/// 3. **Load**: copy the *next* task's `saved` frame *onto* the stack
///    slot, overwriting the current frame in place. When
///    `__vectors_restore` runs, it loads *these* registers and `eret`s
///    to the new task — exactly the same code path as a normal syscall
///    return, just with a different frame.
/// 4. **Swap address space**: write the new task's `ttbr0_pa` to
///    TTBR0_EL1. The kernel's TTBR1 is shared and untouched.
///
/// No new assembly. The existing `__vectors_restore` stub is the
/// context-switch epilogue; this function is its prologue. The beauty
/// is that the switch reuses the *same* hardware return path M5 built:
/// load registers from a frame, `eret`. The only new idea is "a
/// *different* task's frame."
///
/// # Safety
///
/// `frame` points to the TrapFrame the vector stub pushed on the kernel
/// stack. The caller (syscall handler) holds a `&mut` to it for the
/// duration. We copy bytes in and out of it; the restore stub reads
/// exactly this memory. Single-core, interrupts masked (we are in an
/// exception handler — DAIF is set by the exception entry).
pub unsafe fn save_and_switch(frame: &mut TrapFrame) -> bool {
    let cur = current_tid();

    // If there is a current task (not the kernel, TID 0), save its
    // context and mark it Ready to run again later.
    if cur != 0 {
        // SAFETY: cur is in bounds (set only by set_current_tid, which
        // we call below with a value from alloc_task — also in bounds).
        // Single-core, no concurrent access.
        if let Some(t) = unsafe { task(cur) } {
            t.saved = *frame;
            if t.state == TaskState::Running {
                t.state = TaskState::Ready;
                // Re-enqueue so it gets picked again later.
                unsafe { scheduler() }.enqueue(cur);
            }
        }
    }

    // Pick the next task.
    let next = match unsafe { scheduler() }.dequeue() {
        Some(tid) => tid,
        // Nobody else to run — return false, caller returns to EL0
        // normally (the single-task no-op yield, same as M5).
        None => return false,
    };

    // Load the next task's saved context into the stack frame slot.
    // SAFETY: next is from the scheduler's ready queue, which only
    // holds valid TIDs of Ready tasks.
    let next_task = match unsafe { task(next) } {
        Some(t) => t,
        None => return false, // shouldn't happen, but honest
    };

    // The frame copy: overwrite the on-stack frame with the next
    // task's saved state. When __vectors_restore runs, it will load
    // THESE registers and eret to the new task.
    *frame = next_task.saved;
    next_task.state = TaskState::Running;

    // Swap the address space: the new task's user pages.
    if next_task.ttbr0_pa != 0 {
        crate::arch::aarch64::mmu::set_user_ttbr0(next_task.ttbr0_pa);
    }

    // Remember who is running now.
    unsafe { set_current_tid(next) };

    true
}

/// Dump the task table to the console — a diagnostic for the monitor.
/// Shows each non-Exited task's TID, name, state, and TTBR0.
pub fn dump_tasks() {
    println!("--- task table ---");
    for tid in 0..MAX_TASKS {
        // SAFETY: read-only diagnostic; single-core, no concurrent writer.
        let t = unsafe { &*(&raw const TASK_TABLE[tid]) };
        if t.state == TaskState::Exited {
            continue;
        }
        println!(
            "  TID {tid}: {:8}  state={:?}  ttbr0={:#x}",
            t.name_str(),
            t.state,
            t.ttbr0_pa
        );
    }
    let s = unsafe { &*(&raw const SCHEDULER) };
    println!(
        "  scheduler: {} ready, current=TID {}",
        s.count,
        current_tid()
    );
}

// ---------------------------------------------------------------------------
// TrapFrame::empty — the zeroed context for a fresh task
// ---------------------------------------------------------------------------

impl TrapFrame {
    /// A zeroed trap frame — the starting context for a new task.
    /// The task's real entry point and SP are written in by the task
    /// creator (setting `elr` to the entry, `sp` to the stack top, etc.).
    pub const fn empty() -> Self {
        TrapFrame {
            x: [0; 31],
            sp: 0,
            elr: 0,
            spsr: 0,
            esr: 0,
            far: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests (run at compile time via const eval where possible)
// ---------------------------------------------------------------------------

const _: () = {
    // The scheduler ring buffer must be correctly sized.
    assert!(MAX_TASKS > 0);
    // A Task must be a reasonable size (no accidental bloat).
    assert!(core::mem::size_of::<Task>() < 400);
};