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
/// `ttbr0_pa` is the physical address of the task's user page-table
/// root. Context switch writes this to TTBR0_EL1, giving each task its
/// own address space. The kernel's TTBR1 is shared and untouched.
///
/// `mailbox` is the task's IPC inbox — a fixed-size kernel buffer where
/// messages sent to this task land. The `send` syscall copies into it;
/// the `recv` syscall copies out of it. No shared memory: tasks talk
/// through the kernel, not past it. `mailbox_len` is 0 when the mailbox
/// is empty; `mailbox_from` is the sender's TID (0 = never received).
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
    /// IPC mailbox: up to 32 bytes of message data. Zeroed by boot
    /// and by Task::empty(). The `send` syscall writes into it; the
    /// `recv` syscall reads from it. One message at a time — if the
    /// mailbox is full, send returns -EAGAIN (the sender can yield
    /// and retry). This is the simplest possible message passing:
    /// no channels, no queues, just one slot per task. Enough to
    /// prove the idea; a real OS would add buffering, blocking, and
    /// typed channels.
    pub mailbox: [u8; 32],
    /// How many bytes are in the mailbox. 0 = empty, >0 = message
    /// waiting. Set by `send`, cleared by `recv`.
    pub mailbox_len: usize,
    /// The TID of the task that sent the current mailbox message.
    /// 0 = no sender (mailbox empty or never received). Returned to
    /// the receiver so it knows who said hello.
    pub mailbox_from: usize,
    /// If this task is Blocked in a `sendblk` (blocking send), this is
    /// the TID of the receiver it was trying to send to. 0 = not
    /// blocked on a send. Set by the SYS_SENDBLK handler before
    /// calling `block_and_switch`; cleared by `wake_blocked_senders`
    /// when the receiver drains its mailbox and wakes us. The
    /// receiver's `recv` scans the task table for Blocked tasks with
    /// `blocked_send_dst` == its own TID and wakes them — the same
    /// "the kernel is the intermediary" pattern as recvblk, just
    /// mirrored: the sender sleeps on "mailbox full" instead of the
    /// receiver sleeping on "mailbox empty."
    pub blocked_send_dst: usize,
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
            mailbox: [0; 32],
            mailbox_len: 0,
            mailbox_from: 0,
            blocked_send_dst: 0,
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
    // SPSR for EL0: M=0b0000, bit[4]=0 (AArch64), DAIF=0b0000 at [9:6]
    // (all unmasked — interrupts enabled so the timer can preempt).
    // Same value as user.rs's drop_to_el0_with.
    t.saved.spsr = 0x000;
    t.ttbr0_pa = ttbr0_pa;
    t.state = TaskState::Ready;
    t.set_name(name);
    unsafe { scheduler() }.enqueue(tid);
    Some(tid)
}

// ---------------------------------------------------------------------------
// M6 IPC — message passing between tasks
// ---------------------------------------------------------------------------

/// The maximum message size: the mailbox is 32 bytes. Messages longer
/// than this are truncated to this length. Small — this is a teaching
/// kernel, and 32 bytes is enough to say "hello" or pass a number.
pub const MSG_MAX: usize = 32;

/// Send a message to task `dst_tid`. Copies up to 32 bytes from `data`
/// into the receiver's mailbox.
///
/// Returns `Ok(())` on success, or an error code as a negative i64:
///   - `-ESRCH` (3): no such task (TID out of range or Exited)
///   - `-EAGAIN` (11): mailbox full (the receiver hasn't read the last
///     message yet — the sender can yield and retry)
///
/// This is non-blocking: if the mailbox is full, the sender gets
/// `-EAGAIN` and can decide to yield (letting the receiver run and
/// drain) or try again later. A blocking send would set the sender's
/// state to Blocked and wake it when the mailbox empties — that's
/// the next step, but the non-blocking version proves the copy path
/// first.
///
/// # Safety
/// Single-core, interrupts masked (called from the SVC handler at
/// EL1, DAIF set by exception entry).
pub unsafe fn ipc_send(dst_tid: usize, data: &[u8]) -> Result<(), i64> {
    const ESRCH: i64 = 3;
    const EAGAIN: i64 = 11;

    if dst_tid == 0 || dst_tid >= MAX_TASKS {
        return Err(ESRCH);
    }
    // SAFETY: single-core, exception context, DAIF set.
    let t = unsafe { &mut *(&raw mut TASK_TABLE[dst_tid]) };
    if t.state == TaskState::Exited {
        return Err(ESRCH);
    }
    if t.mailbox_len > 0 {
        return Err(EAGAIN);
    }
    let n = data.len().min(MSG_MAX);
    t.mailbox[..n].copy_from_slice(&data[..n]);
    t.mailbox_len = n;
    t.mailbox_from = current_tid();

    // M6 blocking IPC: if the receiver is Blocked (sitting in a recv
    // that found an empty mailbox), the message it was waiting for has
    // just arrived. Wake it — set it to Ready and enqueue it so the
    // scheduler picks it up. When it runs, it retries the recv svc and
    // finds this message. The sender is the alarm clock; the kernel is
    // the intermediary.
    if t.state == TaskState::Blocked {
        t.state = TaskState::Ready;
        // SAFETY: t is a raw-pointer deref of TASK_TABLE[dst_tid];
        // SCHEDULER is a separate static. No aliasing conflict.
        // Single-core, DAIF-set exception context.
        unsafe { scheduler() }.enqueue(dst_tid);
    }

    Ok(())
}

/// Wake a Blocked task: set it to Ready and enqueue it. Returns true if
/// the task was Blocked (and is now woken), false otherwise. Currently
/// called only from `ipc_send` (inlined above), but exposed as a public
/// API for future blocking primitives (e.g. blocking send, sleep, wait).
///
/// # Safety
/// Single-core, interrupts masked (called from the SVC handler at
/// EL1, DAIF set by exception entry).
pub unsafe fn wake(tid: usize) -> bool {
    if tid == 0 || tid >= MAX_TASKS {
        return false;
    }
    // SAFETY: tid is in bounds; single-core, no concurrent access.
    let t = unsafe { &raw mut TASK_TABLE[tid] };
    if unsafe { (*t).state } != TaskState::Blocked {
        return false;
    }
    unsafe { (*t).state = TaskState::Ready };
    unsafe { scheduler() }.enqueue(tid);
    true
}

/// Receive a message into `buf`. If the mailbox is empty, returns
/// `-EAGAIN` (the caller can yield and retry, or spin). If a message
/// is waiting, copies it into `buf` (up to `buf.len()` bytes), clears
/// the mailbox, and returns `Ok((len, sender_tid))`.
///
/// Returns `Ok((usize, usize))` = (bytes copied, sender TID) on
/// success, or an error code as a negative i64:
///   - `-EAGAIN` (11): no message waiting
///
/// # Safety
/// Single-core, interrupts masked (called from the SVC handler).
pub unsafe fn ipc_recv(buf: &mut [u8]) -> Result<(usize, usize), i64> {
    const EAGAIN: i64 = 11;

    let cur = current_tid();
    if cur == 0 || cur >= MAX_TASKS {
        return Err(EAGAIN);
    }
    // SAFETY: single-core, exception context.
    let t = unsafe { &mut *(&raw mut TASK_TABLE[cur]) };
    if t.mailbox_len == 0 {
        return Err(EAGAIN);
    }
    let n = t.mailbox_len.min(buf.len());
    buf[..n].copy_from_slice(&t.mailbox[..n]);
    let from = t.mailbox_from;
    t.mailbox_len = 0;
    t.mailbox_from = 0;

    // M6 blocking send: now that the mailbox is drained, wake any task
    // that was blocked in a sendblk waiting for this mailbox to empty.
    // The sender retries its syscall on wake (ELR was rewound by 4) and
    // finds the mailbox empty — its message fits this time. This is the
    // mirror image of recvblk: there the sender wakes the receiver; here
    // the receiver wakes the sender. The kernel is the intermediary in
    // both directions.
    unsafe { wake_blocked_senders(cur) };

    Ok((n, from))
}

/// Scan the task table for tasks Blocked on a `sendblk` to `recv_tid` and
/// wake them (Ready, enqueued). Called by `ipc_recv` after it drains the
/// mailbox — the space is now free, so a blocked sender's retry will
/// succeed.
///
/// A blocked sender's `blocked_send_dst` names the receiver whose mailbox
/// was full; we wake it if that receiver is `recv_tid` (the task that just
/// drained its mailbox). Multiple senders could be blocked on the same
/// receiver (rare with 8 task slots, but honest to handle); we wake all of
/// them — the first to run gets the empty mailbox, the rest re-block if
/// it fills again. This is the classic "thundering herd" trade-off in
/// miniature; a real OS would queue them, but here waking-all-then-re-block
/// is simpler and the scale is tiny.
///
/// # Safety
/// Single-core, interrupts masked (called from the SVC handler at
/// EL1, DAIF set by exception entry).
pub unsafe fn wake_blocked_senders(recv_tid: usize) {
    for tid in 1..MAX_TASKS {
        // SAFETY: tid is in bounds; single-core, no concurrent access.
        let t = unsafe { &raw mut TASK_TABLE[tid] };
        // SAFETY: dereferencing in-bounds pointer; read of two usizes.
        if unsafe { (*t).state } == TaskState::Blocked
            && unsafe { (*t).blocked_send_dst } == recv_tid
        {
            // SAFETY: same invariant; clearing the flag and transitioning
            // to Ready. The task will retry its sendblk syscall on resume.
            unsafe {
                (*t).blocked_send_dst = 0;
                (*t).state = TaskState::Ready;
            }
            // SAFETY: SCHEDULER is a separate static; single-core.
            unsafe { scheduler() }.enqueue(tid);
        }
    }
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

/// Block the current task and switch to the next Ready one. This is the
/// M6 blocking-IPC primitive: a task that calls a blocking `recv` on an
/// empty mailbox voluntarily removes itself from the ready queue until
/// something wakes it (a `send` to its mailbox, which sets it back to
/// Ready and enqueues it).
///
/// Unlike [`save_and_switch`], which re-enqueues the current task as
/// Ready (cooperative yield — "I'll run again later"), this sets the
/// current task to [`TaskState::Blocked`] and does *not* enqueue it. The
/// task sits dormant until a specific event wakes it. The caller is
/// responsible for setting up the wake condition *before* calling this
/// (e.g. the recv handler has already noted that this task is blocked on
/// its mailbox; the send handler will check `state == Blocked` and wake).
///
/// `frame` is the on-stack TrapFrame the exception stub built. The caller
/// should adjust `frame.elr` (e.g. rewind it by 4 to re-execute the svc)
/// *before* calling if the task should retry its syscall on wake — this
/// is the standard "the handler hasn't produced a return value yet, so
/// re-enter it" pattern. The frame is copied into the TCB's `saved`
/// field; the next task's saved frame is copied onto the stack in its
/// place, and `__vectors_restore` erets to the next task.
///
/// Returns `true` if a switch happened, `false` if there is no other
/// Ready task (in which case the current task is left Blocked and the
/// caller must handle the "nobody to switch to" case — typically by
/// returning to the monitor, since a Blocked task with no other runners
/// is a deadlock).
///
/// # Safety
/// `frame` points to the on-stack TrapFrame. Single-core, interrupts
/// masked (exception handler, DAIF set). See [`save_and_switch`].
pub unsafe fn block_and_switch(frame: &mut TrapFrame) -> bool {
    let cur = current_tid();
    if cur == 0 {
        // The kernel itself can't block — there's no TCB for TID 0.
        return false;
    }

    // Save the current task's context (with the caller's ELR adjustment)
    // and mark it Blocked — not Ready, not enqueued. It sleeps until a
    // specific wake (e.g. ipc_send to its mailbox).
    // SAFETY: cur is in bounds; single-core, no concurrent access.
    if let Some(t) = unsafe { task(cur) } {
        t.saved = *frame;
        t.state = TaskState::Blocked;
    }

    // Pick the next task. If nobody is ready, we have a potential
    // deadlock — return false so the caller can fall back (e.g. return
    // to the monitor rather than hang).
    let next = match unsafe { scheduler() }.dequeue() {
        Some(tid) => tid,
        None => return false,
    };

    // Load the next task's saved context into the stack frame slot.
    // SAFETY: next is from the scheduler's ready queue — a valid TID.
    let next_task = match unsafe { task(next) } {
        Some(t) => t,
        None => return false,
    };
    *frame = next_task.saved;
    next_task.state = TaskState::Running;

    if next_task.ttbr0_pa != 0 {
        crate::arch::aarch64::mmu::set_user_ttbr0(next_task.ttbr0_pa);
    }
    unsafe { set_current_tid(next) };
    true
}

/// Kill the current task on a fault and switch to the next Ready task.
///
/// This is the M6 evolution of M5's `kill_task_on_fault`: instead of
/// abandoning all tasks and returning to the monitor when a user task
/// faults, the kernel marks the faulting task Exited and resumes the
/// scheduler. If there is another Ready task, we switch to it (the OS
/// keeps running). If there are no more tasks, we return `false` so
/// the caller can fall back to the monitor — the honest "all tasks
/// dead" case.
///
/// `frame` is the on-stack TrapFrame the exception stub built. If we
/// switch to a new task, this function returns `true` and the caller
/// (the exception handler) lets `__vectors_restore` eret to the new
/// task. If there is no other task, this function returns `false` and
/// the caller must handle the return to the monitor itself (as before).
///
/// This function also clears any blocked-send state the task may have
/// had (a faulting task shouldn't be woken by a receiver it'll never
/// hear from again).
///
/// # Safety
/// Single-core, interrupts masked (exception handler, DAIF set).
pub unsafe fn kill_current_task(frame: &mut TrapFrame) -> bool {
    let cur = current_tid();

    if cur == 0 || cur >= MAX_TASKS {
        // Kernel itself faulted (no user task active) — can't kill
        // a task that doesn't exist. Return false so the caller
        // falls back to the monitor.
        return false;
    }

    // Mark the faulting task as Exited and clear any blocked-send
    // state. The slot is now free for reuse.
    // SAFETY: cur is in bounds; single-core, no concurrent access.
    if let Some(t) = unsafe { task(cur) } {
        t.state = TaskState::Exited;
        t.blocked_send_dst = 0;
        t.mailbox_len = 0;
        t.mailbox_from = 0;
    }

    // Try to switch to the next Ready task.
    let next = match unsafe { scheduler() }.dequeue() {
        Some(tid) => tid,
        None => return false, // no more tasks — caller returns to monitor
    };

    // Load the next task's context and switch.
    // SAFETY: next is from the ready queue — a valid TID.
    let next_task = match unsafe { task(next) } {
        Some(t) => t,
        None => return false,
    };
    *frame = next_task.saved;
    next_task.state = TaskState::Running;

    if next_task.ttbr0_pa != 0 {
        crate::arch::aarch64::mmu::set_user_ttbr0(next_task.ttbr0_pa);
    }
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
            "  TID {tid}: {:8}  state={:?}  ttbr0={:#x}  mbox={}b from={}",
            t.name_str(),
            t.state,
            t.ttbr0_pa,
            t.mailbox_len,
            t.mailbox_from,
        );
    }
    let s = unsafe { &*(&raw const SCHEDULER) };
    println!(
        "  scheduler: {} ready, current=TID {}, preempts={}",
        s.count,
        current_tid(),
        preempts()
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

// ---------------------------------------------------------------------------
// M6 preemptive scheduling — the timer drives the switch
// ---------------------------------------------------------------------------

/// Is preemptive scheduling enabled? When true, the timer IRQ handler
/// calls [`save_and_switch`] on each tick when a user task is running,
/// preempting it in favor of the next Ready task. Default off: the
/// cooperative `yield` syscall is the only context switch until this is
/// turned on (by the monitor's `preempt` command).
///
/// The flag is the boundary between M6's two halves: cooperative
/// scheduling (yield-driven, already working) and preemptive scheduling
/// (timer-driven, this piece). The wiring is the same `save_and_switch`
/// either way — the only question is *who pulls the trigger*: the user
/// code (svc) or the timer (IRQ).
static PREEMPT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// How many times the timer has preempted a running task. A diagnostic
/// counter, separate from the cooperative yield count — the two between
/// them tell you whether switches were voluntary or involuntary. Printed
/// by `dump_tasks` and the `preempt` monitor command.
static PREEMPTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Is preemptive scheduling currently enabled?
pub fn preempt_enabled() -> bool {
    PREEMPT.load(core::sync::atomic::Ordering::Relaxed)
}

/// Enable preemptive scheduling. After this, the timer IRQ will call
/// [`save_and_switch`] on each tick when a user task is running. The
/// caller is responsible for arming the timer and unmasking IRQ.
pub fn preempt_on() {
    PREEMPT.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// Disable preemptive scheduling. Called when the last task exits and
/// we return to the monitor — the timer may still be armed, but the
/// IRQ handler will no longer switch tasks (it checks this flag before
/// calling [`save_and_switch`]).
pub fn preempt_off() {
    PREEMPT.store(false, core::sync::atomic::Ordering::Relaxed);
}

/// How many preemptions have occurred (timer-driven context switches).
pub fn preempts() -> u64 {
    PREEMPTS.load(core::sync::atomic::Ordering::Relaxed)
}

/// Increment the preemption counter. Called from the IRQ handler after
/// a successful timer-driven context switch.
pub fn bump_preempts() {
    PREEMPTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}
