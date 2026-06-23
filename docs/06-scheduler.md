# 06 — Scheduler and IPC: the kernel learns to share

This is the story of milestone 6. By the end you will know how Opal goes
from *one* user task to *many*: how tasks are born, how the CPU changes
hands between them, how they talk to each other through the kernel, how
a fault kills the task instead of the kernel, and how the timer takes
the CPU away. Keep `src/sched.rs`, `src/arch/aarch64/user.rs`, and
`src/arch/aarch64/vectors.rs` open next to this — the prose and the code
are companions, and neither makes full sense alone.

M5 proved the EL0 boundary: one task, one address space, syscalls that
trap and return. M6 adds the *plural*. The kernel now manages a table of
tasks, each with its own user page tables and saved register state, and
a scheduler that decides who runs next. The key insight is that context
switch is not new machinery — it is the *same* `eret` path M5 built, just
with a different task's frame.

---

## 1. The problem: one task is not an operating system

M5's kernel could drop to EL0, run a program, catch its `exit`, and
return to the monitor. But it was a single-task kernel: one user
program at a time, no way to run two, no way for them to talk, and a
user fault killed the whole "OS" (really, it just returned to the
monitor). That is the boundary between "the kernel becomes an operating
system" (M5) and "the kernel learns to share" (M6).

An operating system is defined by multiplexing: many tasks, one CPU,
and a scheduler that decides who runs when. M6 builds that. The pieces,
in the order the heartbeat built them:

1. **Task control block** — the data structure the scheduler is built on
2. **Context switch** — `save_and_switch`: the same `eret` path, a different frame
3. **Cooperative scheduling** — the `yield` syscall drives the switch
4. **IPC** — tasks talk through the kernel via mailboxes
5. **Blocking IPC** — `recvblk` and `sendblk`: the Blocked state
6. **Fault recovery** — the OS survives a task's death
7. **Preemptive scheduling** — the timer takes the CPU away
8. **Sleep** — `sleep(ticks)`: timer-driven blocking, the timer as the satisfier
9. **Wait** — `wait(tid)`: task-lifecycle blocking, a child's exit as the satisfier

Each piece was one heartbeat beat — one file, one feature, tested in
QEMU, committed. The rhythm is the same as the timer interrupt M3 built:
arm, fire, re-arm.

---

## 2. The Task control block

The scheduler's foundation is a table of tasks. No allocator means no
linked lists and no dynamic growth — the table is a fixed array in
`.bss`, zeroed by the boot stub. A task's "TID" (Task ID) is its index
in this array.

```rust
pub const MAX_TASKS: usize = 8;

static mut TASK_TABLE: [Task; MAX_TASKS] = [const { Task::empty() }; MAX_TASKS];
```

Each `Task` is a control block holding everything the scheduler needs to
pause a task and resume it later:

```rust
#[repr(C)]
pub struct Task {
    pub saved: TrapFrame,       // 288 bytes — the register file at last svc
    pub ttbr0_pa: u64,           // PA of this task's user page-table root
    pub state: TaskState,        // Ready, Running, Blocked, or Exited
    pub name: [u8; 8],           // human-readable name for diagnostics
    pub mailbox: [u8; 32],       // IPC inbox — one message at a time
    pub mailbox_len: usize,      // 0 = empty, >0 = message waiting
    pub mailbox_from: usize,     // sender's TID
    pub blocked_send_dst: usize, // TID we're blocked sending to (0 = not)
    pub wake_tick: u64,          // tick deadline for sleep (0 = not sleeping)
    pub exit_code: i64,          // exit code for wait() (0 = not exited)
    pub waiting_on: usize,       // TID we're waiting for in wait() (0 = not)
}
```

Three things to notice:

**The saved context is a TrapFrame.** The same 288-byte struct the
exception stubs build on the kernel stack — `x0..x30`, `sp`, `elr`,
`spsr`, `esr`, `far`. Context switch is "save the current frame into the
TCB, load a different TCB's frame onto the stack, `eret`." No new
assembly; the existing `__vectors_restore` stub is the context-switch
epilogue. This is the beauty of M6: the switch reuses the *same* hardware
return path M5 built for a single syscall return. The only new idea is
"a *different* task's frame."

**`ttbr0_pa` is the address-space handle.** Each task has its own user
page tables (four levels, 64 KiB of `.bss` per task). Context switch
writes this to `TTBR0_EL1`, giving each task its own low-half view.
The kernel's `TTBR1` is shared and never changes — all tasks see the
same kernel.

**`TaskState::Exited` is discriminant 0, deliberately.** A zeroed `.bss`
slot reads as `Exited`, which means "free." `alloc_task()` scans for
`Exited` slots without any explicit initialization — the boot stub's
zeroing *is* the initialization.

```
Exited  ──spawn──▶  Ready  ──dispatch──▶  Running
  ▲                   │                      │
  │                   │                      │ yield/irq
  └──exit/fault───────┴──save_and_switch─────┘
                      │
                      ▼
                   Blocked ──wake──▶ Ready
```

The `Blocked` state is M6's IPC and timer half: a task sleeping on a
condition — mailbox empty for `recvblk`, mailbox full for `sendblk`,
timer deadline for `sleep`, child exit for `wait` — waiting for another
task or the timer to wake it. Before M6's IPC, the state machine was just
`Ready → Running → Exited`.

---

## 3. The scheduler: a ring, not a linked list

The ready queue is a simple FIFO ring buffer — no linked lists, no
allocator, no priority. Round-robin: whoever became ready first runs
next.

```rust
pub struct Scheduler {
    ready_queue: [usize; MAX_TASKS],  // TIDs of Ready tasks
    head: usize,                       // next to dequeue
    count: usize,                      // how many waiting
}
```

`enqueue(tid)` appends to the tail; `dequeue()` pops the head. Both
are O(1). The scheduler is a static singleton:

```rust
static mut SCHEDULER: Scheduler = Scheduler::new();
```

Access is through `unsafe fn scheduler() -> &'static mut Scheduler`,
which is safe only because the kernel is single-core and (for now)
cooperative: there is exactly one execution context that can touch the
scheduler, so there is no need for locking. The safety comment on
`scheduler()` is explicit about the invariant and names the thing that
will break it:

> Single-core, no preemption yet — there is exactly one execution
> context that can touch the scheduler. Once M6 adds preemptive
> scheduling (timer-driven), this must be called with interrupts masked.

And indeed, once preemption is enabled, every scheduler access happens
in an exception handler where DAIF is set by the architecture — the
timer IRQ cannot nest. The invariant holds.

---

## 4. Context switch: the same `eret`, a different frame

`save_and_switch` is the heart of M6. It is called from the `yield`
syscall handler and (later) the timer IRQ handler, with the trap frame
that the exception stub built on the kernel stack — the frame that
`__vectors_restore` is about to reload and `eret` from.

The switch is, mechanically, three copies and a TTBR0 swap:

```rust
pub unsafe fn save_and_switch(frame: &mut TrapFrame) -> bool {
    let cur = current_tid();

    // 1. SAVE: copy the on-stack frame into the current task's TCB.
    if cur != 0 {
        if let Some(t) = unsafe { task(cur) } {
            t.saved = *frame;
            if t.state == TaskState::Running {
                t.state = TaskState::Ready;
                unsafe { scheduler() }.enqueue(cur);  // re-enqueue
            }
        }
    }

    // 2. PICK: dequeue the next Ready task (round-robin).
    let next = match unsafe { scheduler() }.dequeue() {
        Some(tid) => tid,
        None => return false,  // nobody else → keep running
    };

    // 3. LOAD: overwrite the on-stack frame with the next task's saved state.
    let next_task = match unsafe { task(next) } {
        Some(t) => t,
        None => return false,
    };
    *frame = next_task.saved;
    next_task.state = TaskState::Running;

    // 4. SWAP ADDRESS SPACE: the new task's user pages.
    if next_task.ttbr0_pa != 0 {
        crate::arch::aarch64::mmu::set_user_ttbr0(next_task.ttbr0_pa);
    }

    unsafe { set_current_tid(next) };
    true
}
```

The trick: `*frame = next_task.saved` overwrites the frame *in place* on
the kernel stack. When `__vectors_restore` runs (it is the very next
thing after the Rust handler returns), it loads *these* registers and
`eret`s — to the *new* task. The CPU never knows a switch happened: it
just sees a frame, loads it, and erets. The switch is invisible because
it reuses the existing return path.

No new assembly. No callee-saved register dance. No separate context
switch routine. The 288-byte TrapFrame the exception stubs already build
*is* the context. The switch is a memory copy.

If there is no other task (single-task mode, or the last task running),
`dequeue()` returns `None` and `save_and_switch` returns `false` — the
caller falls back to the M5 no-op `eret` straight back. The cooperative
`yield` syscall handler handles this:

```rust
SYS_YIELD => {
    let cur = crate::sched::current_tid();
    let switched = if cur != 0 {
        unsafe { crate::sched::save_and_switch(frame) }
    } else { false };
    if !switched {
        println!("[kernel] syscall: yield() — no other task, returning to EL0");
        frame.x[0] = 0;
    }
}
```

The `spawn2` monitor command spawns two tasks and drops to EL0 with the
scheduler active. When task A yields, the scheduler switches to task B;
when B yields, it switches back. The output interleaves — proof that two
independent tasks share one CPU.

---

## 5. Per-task address spaces

Each task needs its own user page tables. M5 had one set; M6 statically
reserves a fixed number. Each `UserTaskMem` is 64 KiB of `.bss` (four
page tables × 16 KiB) plus 32 KiB (code + stack pages) = 96 KiB. Two
tasks = 192 KiB — small for a kernel with 512 MiB of RAM.

```rust
#[repr(C)]
struct UserTaskMem {
    l0: mmu::PageTable,
    l1: mmu::PageTable,
    l2: mmu::PageTable,
    l3: mmu::PageTable,
    code: UserCodePage,    // 16 KiB
    stack: UserStackPage,  // 16 KiB
}

static mut TASK_MEM_A: UserTaskMem = /* zeroed */;
static mut TASK_MEM_B: UserTaskMem = /* zeroed */;
```

`build_task_user_space(slot, program_bytes)` fills the page tables for
task `slot` (0 or 1), copies the program into the code page, and returns
the PA of the L0 root — the value for `TTBR0_EL1`. Context switch writes
this PA to `TTBR0_EL1`, and the hardware walker translates user VAs
through the new task's tree. The kernel's `TTBR1` tree is shared and
never touched.

This is the first time `TTBR0` and `TTBR1` point at *different* roots
that *change* — M5 set `TTBR0` once and left it. M6 swaps it on every
context switch, and each task sees only its own pages.

---

## 6. IPC: tasks talk through the kernel

M6's IPC is message passing: tasks send and receive through the kernel,
not through shared memory. Each task has a 32-byte mailbox in its TCB.
The `send` syscall copies into it; the `recv` syscall copies out. One
message at a time — simple enough to prove the idea, small enough to fit
in a teaching kernel.

```rust
pub unsafe fn ipc_send(dst_tid: usize, data: &[u8]) -> Result<(), i64> {
    // ...
    let t = unsafe { &mut *(&raw mut TASK_TABLE[dst_tid]) };
    if t.state == TaskState::Exited { return Err(ESRCH); }
    if t.mailbox_len > 0 { return Err(EAGAIN); }  // full
    let n = data.len().min(MSG_MAX);
    t.mailbox[..n].copy_from_slice(&data[..n]);
    t.mailbox_len = n;
    t.mailbox_from = current_tid();
    Ok(())
}
```

The syscall ABI:

| syscall | x8 | args | returns |
|---------|----|------|---------|
| `write`  | 1 | x0=fd, x1=buf, x2=len | x0=bytes written |
| `exit`   | 2 | — | does not return |
| `yield`  | 3 | — | x0=0 |
| `send`   | 4 | x0=dst_tid, x1=buf, x2=len | x0=0 / -ESRCH / -EAGAIN |
| `recv`   | 5 | x0=buf, x1=buf_len | x0=bytes, x1=sender_tid |
| `recvblk`| 6 | x0=buf, x1=buf_len | x0=bytes, x1=sender_tid |
| `sendblk`| 7 | x0=dst_tid, x1=buf, x2=len | x0=0 / -errno |
| `sleep`  | 8 | x0=ticks | x0=0 on wake |
| `exit_code` | 9 | x0=code | does not return |
| `wait`   | 10 | x0=child_tid | x0=exit_code / -ESRCH |

Non-blocking `send` returns `-EAGAIN` if the mailbox is full; the sender
can `yield` and retry. Non-blocking `recv` returns `-EAGAIN` if empty.
The `ipc` monitor command demonstrates this: task A sends "hello B!" and
yields; task B receives and writes "B: got msg!" — proving the message
passed through the kernel from one address space to another.

The kernel reads the user buffer through `TTBR0` (the user's address
space) while running at `EL1` (which can see both `TTBR0` and `TTBR1`).
This is the same path `write` uses to read user strings. The message
never lives in user-visible shared memory — it transits through the
kernel's stack and the receiver's TCB. Tasks talk *through* the kernel,
not *past* it.

---

## 7. Blocking IPC: the Blocked state

Non-blocking IPC is correct but annoying: a receiver must poll
(repeatedly `recv` and `yield`) until a message arrives. M6 adds
blocking variants that put the task to sleep instead:

- `recvblk` (SYS_RECVBLK, x8=6): if the mailbox is empty, the task
  enters the `Blocked` state and the scheduler switches to the next
  task. When a `send` arrives, the sender's `ipc_send` sees the receiver
  is `Blocked`, wakes it (`Ready`, enqueued), and the scheduler resumes
  it — it re-enters the `svc` and finds the message waiting.

- `sendblk` (SYS_SENDBLK, x8=7): if the receiver's mailbox is full, the
  sender enters `Blocked` and the scheduler switches. When the receiver
  drains its mailbox via `recv`/`recvblk`, `wake_blocked_senders` wakes
  the blocked sender, which retries and succeeds.

The "retry the syscall on wake" trick is the same for both: the handler
rewinds `frame.elr` by 4 (one instruction — the `svc`) before blocking,
so when the task is woken and `eret`s, it re-executes the `svc` and
re-enters the handler. The second time through, the condition is met
(message waiting / mailbox empty) and the handler returns normally.

```rust
SYS_RECVBLK => {
    match unsafe { crate::sched::ipc_recv(&mut msg[..buf_len]) } {
        Ok((n, from)) => { /* copy out, return */ }
        Err(_eagain) => {
            // Mailbox empty — block.
            frame.elr = frame.elr.wrapping_sub(4);  // rewind to svc
            let switched = unsafe { crate::sched::block_and_switch(frame) };
            if !switched {
                frame.elr = frame.elr.wrapping_add(4);  // restore
                frame.x[0] = (-11i64) as u64;  // -EAGAIN, no deadlock
            }
        }
    }
}
```

`block_and_switch` is `save_and_switch`'s sibling: it saves the current
task's frame into its TCB, marks it `Blocked` (not `Ready`, not
enqueued), and switches to the next task. The task sits dormant until a
specific event wakes it. This is the same pattern a real OS uses for
blocking `read()`, `wait()`, and futexes: sleep on a condition, wake
when another task satisfies it.

The two blocking primitives form a symmetric pair:

| | receiver sleeps | sender sleeps |
|---|---|---|
| **empty mailbox** | `recvblk` | — |
| **full mailbox** | — | `sendblk` |

The `blkipc` monitor command demonstrates `recvblk` (B blocks, A's
`send` wakes it). The `sendblk` command demonstrates `sendblk` (A blocks
on the second send, B's `recv` wakes it). Together they give M6's IPC a
complete blocking pair — a task can wait in either direction without
spinning.

---

## 8. Fault recovery: the OS survives a task's death

M5 taught the kernel to survive a user fault: the `el0fault` command
runs a program that stores to unmapped VA 0, the data abort traps to
EL1, the handler reports it and returns to the monitor. But in M5,
that killed the *whole OS* — there was only one task, and its death
meant returning to the monitor.

M6 adds the plural case: when one task faults, the kernel kills *that
task* and resumes the scheduler. The other tasks keep running.

```rust
pub unsafe fn kill_current_task(frame: &mut TrapFrame) -> bool {
    let cur = current_tid();
    if cur == 0 || cur >= MAX_TASKS { return false; }

    // Mark the faulting task as Exited, clear IPC state.
    if let Some(t) = unsafe { task(cur) } {
        t.state = TaskState::Exited;
        t.blocked_send_dst = 0;
        t.mailbox_len = 0;
        t.mailbox_from = 0;
    }

    // Switch to the next Ready task (if any).
    let next = match unsafe { scheduler() }.dequeue() {
        Some(tid) => tid,
        None => return false,  // no more tasks — caller returns to monitor
    };
    // ... load next task's frame, swap TTBR0, set_current_tid ...
    true
}
```

The `faultkill` monitor command demonstrates this: task A deliberately
faults (stores to VA 0). The kernel reports the data abort, kills A,
and switches to task B. B writes "B: ok" and exits — proof that the
scheduler kept running after A's death. One task's fault is just that
task's problem. The OS survives.

This is the M6 evolution of M5's `el0fault`: there, the single task's
fault killed the "OS"; here, one task's fault is just that task's
problem, and the scheduler keeps running.

---

## 9. Preemptive scheduling: the timer takes the CPU away

Cooperative scheduling requires tasks to `yield` voluntarily. A task
that spins forever starves everyone else. M6's second half adds
*preemptive* scheduling: the timer IRQ fires every second and calls
`save_and_switch` from the IRQ handler, preempting whichever task is
running.

The wiring is the same `save_and_switch` either way — the only question
is *who pulls the trigger*: the user code (`svc`) or the timer (IRQ).

```rust
fn handle_irq(frame: &mut TrapFrame, kind: Kind, source: Source) {
    let gic = crate::board::virt::gic();
    let id = gic.acknowledge();
    // ...
    match id {
        crate::hal::gicv2::TIMER_IRQ => {
            crate::arch::aarch64::timer::on_tick();
            let period = crate::arch::aarch64::timer::TICK_PERIOD
                .load(core::sync::atomic::Ordering::Relaxed);
            if period > 0 { crate::arch::aarch64::timer::rearm(period); }

            // M6: preemptive scheduling.
            if crate::sched::preempt_enabled() && crate::sched::current_tid() != 0 {
                if unsafe { crate::sched::save_and_switch(frame) } {
                    crate::sched::bump_preempts();
                }
            }
        }
        // ...
    }
    gic.end_interrupt(id);
}
```

The `frame` here is the interrupted EL0 context — the timer trapped from
EL0, so the vector stub saved the user's registers. `save_and_switch`
copies that frame into the current task's TCB (same as a cooperative
yield), picks the next Ready task, and overwrites the frame. When
`__vectors_restore` runs, it `eret`s to the *new* task. The timer, not
the user code, drove the switch.

A flag gates this:

```rust
static PREEMPT: core::sync::atomic::AtomicBool = AtomicBool::new(false);
```

`preempt_off()` is the default (cooperative only). The `preempt` monitor
command spawns two tasks that *spin forever* (no `yield`, no `exit`),
arms the timer, and calls `preempt_on()`. The timer IRQ fires every
second and preempts whichever task is spinning. Both "A" and "B" appear
on the console — proof that the timer, not user code, drove the context
switch. Without preemption, a spinning task would lock the CPU forever.

The flag is the boundary between M6's two halves:

- **Cooperative** (yield-driven): already working. Tasks voluntarily
  surrender via `svc yield`. The scheduler is a courtesy.
- **Preemptive** (timer-driven): the timer IRQ is the authority. Tasks
  cannot hold the CPU past the next tick.

The two between them give a complete scheduler: tasks that yield run
cooperatively; tasks that don't get preempted. The `preempts()` counter
distinguishes voluntary from involuntary switches — printed by
`dump_tasks` and the `preempt` command.

---

## 10. Sleep and wait: the timer and lifecycle blocking primitives

Preemption proves the timer can take the CPU away. Sleep and wait prove
the timer and task lifecycle can *put a task to sleep* — the Blocked state's
third and fourth conditions.

### Sleep (SYS_SLEEP, x8=8)

`sleep(ticks)` blocks the calling task for `ticks` timer ticks. The handler
sets the TCB's `wake_tick` deadline to `now + ticks`, sets `x0=0` (the
return value) *before* blocking, and calls `block_and_switch`. Unlike
`recvblk`/`sendblk`, sleep does **not** rewind ELR — there is no condition
to re-check on wake. The timer is the satisfier, and when it fires, the
sleep is simply done. The frame (with `x0=0`, ELR pointing past the svc)
is saved into the TCB; when `wake_sleepers` wakes the task, the scheduler
restores this frame and `eret` resumes after the svc.

```rust
SYS_SLEEP => {
    let ticks = frame.x[0];
    let now = crate::arch::aarch64::timer::ticks();
    if let Some(t) = unsafe { crate::sched::task(cur) } {
        t.wake_tick = now + ticks;
    }
    frame.x[0] = 0;  // return value set before blocking
    unsafe { crate::sched::block_and_switch(frame) };
}
```

The timer IRQ handler's `wake_sleepers` scans the task table for Blocked
tasks whose `wake_tick` has elapsed, sets them Ready, and enqueues them:

```rust
pub unsafe fn wake_sleepers() {
    let now = crate::arch::aarch64::timer::ticks();
    for tid in 1..MAX_TASKS {
        let t = unsafe { &raw mut TASK_TABLE[tid] };
        let wt = unsafe { (*t).wake_tick };
        if wt == 0 || wt > now { continue; }
        if unsafe { (*t).state } != TaskState::Blocked {
            unsafe { (*t).wake_tick = 0 };
            continue;
        }
        unsafe { (*t).wake_tick = 0; (*t).state = TaskState::Ready; }
        unsafe { scheduler() }.enqueue(tid);
    }
}
```

The `sleep` monitor command demonstrates this: task A calls `sleep(3)`,
task B runs while A sleeps, and when 3 ticks elapse the timer wakes A.

### Wait (SYS_WAIT, x8=10)

`wait(child_tid)` blocks the calling task until task `child_tid` exits,
then returns its exit code. If the child has already exited, `try_wait`
returns the code immediately. If not, the handler sets `waiting_on =
child_tid`, rewinds ELR by 4 (the retry-on-wake trick), and calls
`block_and_switch`. When the child calls `exit(code)`, the exit handler
stores `exit_code` in the child's TCB and calls `wake_waiters`, which
scans for Blocked tasks with `waiting_on == exiting_tid`, wakes them,
and the parent re-enters the `wait` svc — this time `try_wait` finds
the child Exited and returns the code.

```rust
SYS_WAIT => {
    match unsafe { crate::sched::try_wait(child_tid) } {
        Ok(code) => { frame.x[0] = code as u64; }
        Err(EAGAIN) => {
            if let Some(t) = unsafe { crate::sched::task(cur) } {
                t.waiting_on = child_tid;
            }
            frame.elr = frame.elr.wrapping_sub(4);  // retry on wake
            unsafe { crate::sched::block_and_switch(frame) };
        }
        Err(e) => { frame.x[0] = e as u64; }
    }
}
```

`exit(code)` (SYS_EXIT_CODE, x8=9) is the companion: it stores `x0` in
the TCB's `exit_code` field, wakes any waiting parents via
`wake_waiters`, then switches to the next task or returns to the
monitor. The original `exit` (x8=2) and `exit_code` (x8=9) share the
same handler — the only difference is whether the program intended `x0`
as a code.

The four blocking conditions form a complete set:

| | condition | satisfier | wake function |
|---|---|---|---|
| **recvblk** | mailbox empty | another task sends | `ipc_send` (inline) |
| **sendblk** | mailbox full | receiver drains | `wake_blocked_senders` |
| **sleep** | tick deadline | timer reaches deadline | `wake_sleepers` |
| **wait** | child alive | child exits | `wake_waiters` |

All four use the same `block_and_switch` → Blocked → wake → Ready →
enqueue → resume path. The only difference is who pulls the trigger:
another task (IPC), the timer (sleep), or a child's lifecycle (wait).

---

## 11. The exit path: returning to the monitor

When the last task calls `exit`, `save_and_switch` returns `false` —
there is no other task to run. The `exit` handler disables preemption
(to stop the timer from firing into a taskless kernel), condemns the
low half (empties `TTBR0`), and calls `on_el0_return()` — the
trampoline that `eret`s back to EL1 and resumes the monitor loop:

```rust
SYS_EXIT => {
    let cur = crate::sched::current_tid();
    println!("[kernel] syscall: exit() (TID {cur})");
    if cur != 0 {
        if let Some(t) = unsafe { crate::sched::task(cur) } {
            t.state = crate::sched::TaskState::Exited;
        }
    }
    let switched = if cur != 0 {
        unsafe { crate::sched::save_and_switch(frame) }
    } else { false };
    if !switched {
        crate::sched::preempt_off();
        mmu::condemn_low_half();
        on_el0_return();
    }
    // else: switched to next task; its frame is loaded, eret to it.
}
```

If there *is* another task, the exit handler switches to it instead of
returning to the monitor — the CPU stays in EL0, running tasks, and the
monitor is not re-entered until the last task exits. This is the
multi-task exit: one task's death does not end the session.

---

## 12. What M6 does not do (yet)

M6 is a teaching scheduler, not a production one. The honest list of
what is missing:

- **Per-task kernel stacks.** All tasks share the one boot stack
  (`SP_EL1`). This works because the kernel is single-core and
  cooperative (or preemptive with DAIF-set exception handlers), so only
  one task's exception frame is on the stack at a time. A real OS gives
  each task its own kernel stack so a stack overflow in one task's
  syscall handler does not corrupt another's. M6's `save_and_switch`
  copies the frame *off* the shared stack into the TCB, so the stack is
  clean for the next task — but the stack itself is shared. Per-task
  kernel stacks are the next rung.

- **Multi-core.** The roadmap mentions PSCI `CPU_ON` for the cores M0
  parked. M6 is single-core only; `smp 1` in the QEMU command. The
  scheduler's `unsafe fn scheduler()` is safe only under that
  invariant. Multi-core would require per-core run queues or locking.

- **Priority.** Round-robin is the only policy. No priorities, no
  real-time classes, no fair queuing. The ready queue is a FIFO.

- **Memory protection between kernel and task.** The kernel reads user
  buffers through `TTBR0` with `read_volatile`. If the user passes a
  bad pointer, the read faults — a data abort from the handler. M6
  trusts the user program (it is our own embedded code). A real OS
  would copy from user space with fault-around handling.

- **FP/SIMD.** We compile softfloat, so kernel code never touches
  FP/SIMD registers and the TrapFrame does not save them. If userspace
  ever uses floating point, the context switch must save/restore the
  FP register file — 512 bytes per task.

---

## 13. The monitor commands

M6 added several monitor commands, each demonstrating one piece:

| command | what it demonstrates |
|---------|----------------------|
| `tasks`     | dump the task table and scheduler state |
| `spawn2`    | two tasks, cooperative round-robin via `yield` |
| `preempt`   | two spinning tasks, timer-driven preemption |
| `ipc`       | non-blocking `send`/`recv` between two tasks |
| `blkipc`    | blocking `recvblk` — B sleeps, A's `send` wakes it |
| `sendblk`   | blocking `sendblk` — A sleeps on full mailbox, B's `recv` wakes it |
| `faultkill` | scheduler-aware fault recovery — A faults, B survives |
| `sleep`     | timer-driven blocking — A sleeps for N ticks, timer wakes it |
| `wait`      | task-lifecycle blocking — A waits for B to exit, gets exit code |

Each is a one-shot demo: spawn tasks, drop to EL0, run until the last
task exits, return to the monitor. They prove their piece and get out
of the way.

---

## 14. The principle

M6's deepest idea is that context switch is not new machinery — it is
the *same* `eret` path M5 built for a single syscall return, just with
a different task's frame. The 288-byte TrapFrame the exception stubs
build on the kernel stack *is* the context. The switch is a memory
copy: save the current frame into the TCB, load the next TCB's frame
onto the stack, `eret`. The CPU never knows a switch happened.

This is why M6 could be built one beat at a time: each piece (TCB,
switch, cooperative yield, IPC, blocking IPC, fault recovery, preemption,
sleep, wait) plugs into the same return path. The scheduler does not
invent a new mechanism for the timer to drive; it reuses the one the
yield syscall already exercises. The timer IRQ handler calls the same
`save_and_switch` the yield handler does — the only difference is *who
pulled the trigger*.

The timer re-arms itself. Each beat determines the next. The heartbeat
that built M6 is the meta-layer of the same idea: arm, fire, re-arm.