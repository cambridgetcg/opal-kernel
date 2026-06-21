//! M5 — EL0 and syscalls: the kernel becomes an operating system.
//!
//! Everything before this milestone ran at EL1: the kernel WAS the only
//! program. This file builds the other side of that boundary — a tiny
//! userspace that runs at EL0, with its own address space (TTBR0), and
//! talks to the kernel through `svc` traps.
//!
//! ## The smallest possible userspace
//!
//! One code page, one stack page. The "program" is a handful of AArch64
//! instructions embedded in a static byte array: write a byte to the UART
//! (via a syscall), then exit (via a syscall). It is not loaded from
//! disk — it lives in the kernel image, copied to a user page at boot.
//! The point is not the program; the point is the boundary.
//!
//! ## The drop
//!
//! `eret` is the only way to EL0. We set up SPSR_EL1 (the PSTATE EL0
//! will see), ELR_EL1 (where it starts executing), SP_EL0 (its stack),
//! install the user TTBR0, and `eret`. The CPU atomically drops
//! privilege, switches SP, and jumps — all at once.
//!
//! ## The return
//!
//! Every `svc` from EL0 traps to the vector table's "lower EL,
//! AArch64" synchronous slot (offset 0x400). M1 filled that slot with
//! a reporter that says "this shouldn't exist yet" — M5 replaces that
//! with a real syscall dispatcher that reads x8 (the syscall number),
//! handles it, and `eret`s back to EL0 (or, for `exit`, back to EL1).
//!
//! ## What this first beat does
//!
//! - Defines the syscall ABI: `write(fd, buf, len)` via x8=1, `exit()`
//!   via x8=2. The kernel handler reads the trap frame's registers,
//!   performs the action, and returns a value in x0.
//! - Drops to EL0, runs one program that calls `write` then `exit`.
//! - On `exit`, returns to the monitor via a trampoline that erets
//!   back to EL1.
//!
//! The next beat adds per-task kernel stacks, a real scheduler, and
//! the `yield` syscall. This beat proves the boundary works.

use crate::arch::aarch64::mmu;
use crate::board::virt;

use core::arch::asm;

// ---------------------------------------------------------------------------
// User address space layout
// ---------------------------------------------------------------------------

/// Where the user code page lives, virtually. The low half (TTBR0
/// region) starts at VA 0; we pick a simple, page-aligned address.
/// One page of code at 1 MiB into the user address space.
pub const USER_CODE_VA: usize = 0x0000_0000_0010_0000;
/// One page of stack, one page above the code. Stacks grow down, so
/// the stack page is placed above the code page and SP starts at its
/// top.
pub const USER_STACK_VA: usize = 0x0000_0000_0020_0000;
/// The initial SP for EL0: top of the stack page (one granule above
/// USER_STACK_VA).
pub const USER_STACK_TOP: usize = USER_STACK_VA + mmu::GRANULE;

// ---------------------------------------------------------------------------
// Syscall numbers — the M5 ABI
// ---------------------------------------------------------------------------

/// `write(fd, buf, len)` — write `len` bytes from `buf` to `fd`.
/// Args: x0=fd, x1=buf (user VA), x2=len. Returns: x0=bytes written.
/// Only fd=1 (stdout, i.e. the UART) is supported.
pub const SYS_WRITE: u64 = 1;
/// `exit()` — terminate the user program. Does not return to EL0;
/// the kernel returns to the monitor via the EL1 trampoline.
pub const SYS_EXIT: u64 = 2;
/// `yield()` — voluntarily surrender the CPU. In the single-task kernel
/// (M5), yield is a no-op: it traps to EL1, the kernel prints a receipt,
/// and `eret`s straight back to EL0. The point is not the scheduling —
/// that's M6 — but proving the round-trip: EL0 calls `svc`, the kernel
/// services it, and execution *continues* in EL0 at the instruction
/// after the `svc`. This is the first syscall that returns to the user,
/// and the user program verifies it by printing a second message
/// after the yield returns.
pub const SYS_YIELD: u64 = 3;
/// `send(tid, buf, len)` — send a message (up to 32 bytes) to task
/// `tid`. Args: x0=dst_tid, x1=buf (user VA), x2=len. Returns: x0=0
/// on success, negative errno on failure (-ESRCH=3 no such task,
/// -EAGAIN=11 mailbox full). Non-blocking: if the mailbox is full,
/// the sender gets -EAGAIN and can yield and retry.
pub const SYS_SEND: u64 = 4;
/// `recv(buf, len)` — receive a message into `buf`. Args: x0=buf
/// (user VA), x1=buf_len. Returns: x0=bytes received (>=0) or
/// -EAGAIN (11) if no message is waiting. The sender's TID is
/// returned in x1. Non-blocking: if the mailbox is empty, the
/// receiver gets -EAGAIN and can yield and retry.
pub const SYS_RECV: u64 = 5;
/// `recvblk(buf, len)` — blocking receive: like `recv` but if the
/// mailbox is empty, the task is put to sleep (Blocked) instead of
/// returning -EAGAIN. When a `send` arrives at the mailbox, the kernel
/// wakes the task (Ready) and the scheduler resumes it; it re-enters
/// the syscall and finds the message waiting. Args: x0=buf (user VA),
/// x1=buf_len. Returns: x0=bytes received (>=0), x1=sender TID.
///
/// This is the M6 blocking-IPC primitive — the `Blocked` state's first
/// real use. It demonstrates that the scheduler can put a task to sleep
/// on a condition and wake it when the condition is met, the same
/// pattern a real OS uses for blocking reads, waits, and futexes.
pub const SYS_RECVBLK: u64 = 6;
/// `sendblk(tid, buf, len)` — blocking send: like `send` but if the
/// receiver's mailbox is full, the sender is put to sleep (Blocked)
/// instead of returning -EAGAIN. When the receiver drains its mailbox
/// via `recv`/`recvblk`, the kernel wakes the blocked sender and it
/// retries — its message now fits. Args: x0=dst_tid, x1=buf (user VA),
/// x2=len. Returns: x0=0 on success, negative errno on failure.
///
/// This is the symmetric counterpart to `recvblk`: there the receiver
/// sleeps on "mailbox empty," here the sender sleeps on "mailbox full."
/// Together they give M6's IPC a full blocking pair — a task can wait
/// in either direction without spinning or yielding-and-retrying.
pub const SYS_SENDBLK: u64 = 7;

// ---------------------------------------------------------------------------
// The user page tables
// ---------------------------------------------------------------------------

/// The user's page tables: a four-level tree for TTBR0, mapping two
/// pages (code + stack) into the low half. This is a separate tree
/// from the kernel's TTBR1 tree — the first time the two TTBRs point
/// at different roots.
///
/// 64 KiB of .bss (4 tables × 16 KiB), zeroed by the boot stub, so every
/// unwritten descriptor is INVALID — the same property the kernel tree
/// relies on.
#[repr(C)]
struct UserTables {
    l0: mmu::PageTable,
    l1: mmu::PageTable,
    l2: mmu::PageTable,
    l3: mmu::PageTable,
}

static mut USER_TABLES: UserTables = UserTables {
    l0: mmu::PageTable([0; 2048]),
    l1: mmu::PageTable([0; 2048]),
    l2: mmu::PageTable([0; 2048]),
    l3: mmu::PageTable([0; 2048]),
};

/// The user code page: 16 KiB of RAM holding the embedded userspace
/// program. This is the physical backing for the USER_CODE_VA mapping.
/// The boot stub zeroes .bss (which includes this); `build_user_space`
/// copies the program into it.
#[repr(C, align(16384))]
struct UserCodePage([u8; mmu::GRANULE]);
static mut USER_CODE_PAGE: UserCodePage = UserCodePage([0; mmu::GRANULE]);

/// The user stack page: 16 KiB, zeroed by boot. Mapped user-read-write
/// at USER_STACK_VA.
#[repr(C, align(16384))]
struct UserStackPage([u8; mmu::GRANULE]);
static mut USER_STACK_PAGE: UserStackPage = UserStackPage([0; mmu::GRANULE]);

// ---------------------------------------------------------------------------
// M6: per-task user address spaces
// ---------------------------------------------------------------------------
//
// M5 had one user address space (one set of page tables, one code page,
// one stack page). M6 needs *multiple* tasks, each with its own user
// space. We cannot allocate, so we statically reserve a fixed number
// of per-task address spaces. Each `UserTaskMem` is 64 KiB of .bss
// (4 page tables × 16 KiB) + 32 KiB (code + stack pages) = 96 KiB.
// Two tasks = 192 KiB — small for a teaching kernel with 512 MiB RAM.

/// A complete per-task user address space: four levels of page tables
/// plus a code page and a stack page. All zeroed by boot; `spawn_task`
/// fills them in.
#[repr(C)]
struct UserTaskMem {
    l0: mmu::PageTable,
    l1: mmu::PageTable,
    l2: mmu::PageTable,
    l3: mmu::PageTable,
    code: UserCodePage,
    stack: UserStackPage,
}

/// Two per-task address spaces, enough for the M6 two-task demo.
/// More can be added; MAX_TASKS is 8 but we only need 2 for the
/// round-robin proof.
static mut TASK_MEM_A: UserTaskMem = UserTaskMem {
    l0: mmu::PageTable([0; 2048]),
    l1: mmu::PageTable([0; 2048]),
    l2: mmu::PageTable([0; 2048]),
    l3: mmu::PageTable([0; 2048]),
    code: UserCodePage([0; mmu::GRANULE]),
    stack: UserStackPage([0; mmu::GRANULE]),
};
static mut TASK_MEM_B: UserTaskMem = UserTaskMem {
    l0: mmu::PageTable([0; 2048]),
    l1: mmu::PageTable([0; 2048]),
    l2: mmu::PageTable([0; 2048]),
    l3: mmu::PageTable([0; 2048]),
    code: UserCodePage([0; mmu::GRANULE]),
    stack: UserStackPage([0; mmu::GRANULE]),
};

/// Get the UserTaskMem for a given task slot (0 or 1). Returns a raw
/// pointer so the caller can build the page tables and copy the program.
///
/// # Safety
/// `slot` must be 0 or 1. Single-core, no concurrent access.
unsafe fn task_mem(slot: usize) -> *mut UserTaskMem {
    match slot {
        0 => &raw mut TASK_MEM_A,
        1 => &raw mut TASK_MEM_B,
        _ => unreachable!("only 2 per-task address spaces exist"),
    }
}

// ---------------------------------------------------------------------------
// The userspace program
// ---------------------------------------------------------------------------

/// The complete userspace program as raw bytes. 14 instructions (56
/// bytes), two strings, and padding. The program exercises the full
/// syscall round-trip: write a message, yield (trap and return), write
/// a second message, then exit. If the yield returns correctly, the
/// second write executes — proving the kernel can service a syscall
/// and resume EL0 at the instruction after the `svc`.
///
/// ```asm
/// [0x00] mov  x8, #1          ; SYS_WRITE
/// [0x04] mov  x0, #1          ; fd = stdout
/// [0x08] adr  x1, +0x30       ; buf -> 0x38 ("hello, EL0!\n\r")
/// [0x0c] mov  x2, #13        ; len
/// [0x10] svc  #0              ; syscall: write
/// [0x14] mov  x8, #3          ; SYS_YIELD
/// [0x18] svc  #0              ; syscall: yield (returns to EL0)
/// [0x1c] mov  x8, #1          ; SYS_WRITE (again)
/// [0x20] mov  x0, #1          ; fd = stdout
/// [0x24] adr  x1, +0x24       ; buf -> 0x48 ("back from yield\n\r")
/// [0x28] mov  x2, #17         ; len = 17
/// [0x2c] svc  #0              ; syscall: write
/// [0x30] mov  x8, #2          ; SYS_EXIT
/// [0x34] svc  #0              ; syscall: exit
/// [0x38] "hello, EL0!\n\r\0"  ; first message (13 bytes + NUL)
/// [0x48] "back from yield\n\r\0" ; second message (17 bytes + NUL)
/// ```
const USER_PROGRAM_BYTES: [u8; 128] = {
    let mut buf = [0u8; 128];
    // [0x00] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[0] = 0x28; buf[1] = 0x00; buf[2] = 0x80; buf[3] = 0xD2;
    // [0x04] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[4] = 0x20; buf[5] = 0x00; buf[6] = 0x80; buf[7] = 0xD2;
    // [0x08] adr x1, +0x30 (target 0x38) = 0x10000181
    buf[8] = 0x81; buf[9] = 0x01; buf[10] = 0x00; buf[11] = 0x10;
    // [0x0c] mov x2, #13 = MOVZ x2, #13 = 0xD28001A2
    buf[12] = 0xA2; buf[13] = 0x01; buf[14] = 0x80; buf[15] = 0xD2;
    // [0x10] svc #0 = 0xD4000001
    buf[16] = 0x01; buf[17] = 0x00; buf[18] = 0x00; buf[19] = 0xD4;
    // [0x14] mov x8, #3 = MOVZ x8, #3 = 0xD2800068
    buf[20] = 0x68; buf[21] = 0x00; buf[22] = 0x80; buf[23] = 0xD2;
    // [0x18] svc #0 = 0xD4000001
    buf[24] = 0x01; buf[25] = 0x00; buf[26] = 0x00; buf[27] = 0xD4;
    // [0x1c] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[28] = 0x28; buf[29] = 0x00; buf[30] = 0x80; buf[31] = 0xD2;
    // [0x20] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[32] = 0x20; buf[33] = 0x00; buf[34] = 0x80; buf[35] = 0xD2;
    // [0x24] adr x1, +0x24 (target 0x48) = 0x10000121
    buf[36] = 0x21; buf[37] = 0x01; buf[38] = 0x00; buf[39] = 0x10;
    // [0x28] mov x2, #17 = MOVZ x2, #17 = 0xD2800222
    buf[40] = 0x22; buf[41] = 0x02; buf[42] = 0x80; buf[43] = 0xD2;
    // [0x2c] svc #0 = 0xD4000001
    buf[44] = 0x01; buf[45] = 0x00; buf[46] = 0x00; buf[47] = 0xD4;
    // [0x30] mov x8, #2 = MOVZ x8, #2 = 0xD2800048
    buf[48] = 0x48; buf[49] = 0x00; buf[50] = 0x80; buf[51] = 0xD2;
    // [0x34] svc #0 = 0xD4000001
    buf[52] = 0x01; buf[53] = 0x00; buf[54] = 0x00; buf[55] = 0xD4;
    // [0x38..0x45] "hello, EL0!\n\r\0" (13 bytes + NUL = 14, pad to 0x48)
    buf[56] = b'h'; buf[57] = b'e'; buf[58] = b'l'; buf[59] = b'l';
    buf[60] = b'o'; buf[61] = b','; buf[62] = b' '; buf[63] = b'E';
    buf[64] = b'L'; buf[65] = b'0'; buf[66] = b'!'; buf[67] = b'\n';
    buf[68] = b'\r'; buf[69] = 0x00;
    // [0x48..0x5A] "back from yield\n\r\0" (17 bytes + NUL)
    buf[72] = b'b'; buf[73] = b'a'; buf[74] = b'c'; buf[75] = b'k';
    buf[76] = b' '; buf[77] = b'f'; buf[78] = b'r'; buf[79] = b'o';
    buf[80] = b'm'; buf[81] = b' '; buf[82] = b'y'; buf[83] = b'i';
    buf[84] = b'e'; buf[85] = b'l'; buf[86] = b'd'; buf[87] = b'\n';
    buf[88] = b'\r'; buf[89] = 0x00;
    buf
};

/// A userspace program that *deliberately* faults — the M5 fault-recovery
/// test. Two instructions: set x0 = 0 (an unmapped user VA — the user
/// address space starts at `USER_CODE_VA = 0x100000`, so VA 0 has no
/// mapping), then `str x1, [x0]` — a store to that unmapped address.
///
/// The store takes a data abort from EL0. `handle_sync_from_el0` catches
/// it, calls `kill_task_on_fault`, and the kernel returns to the monitor
/// instead of parking. This is the first fault the kernel *services*
/// (recovers from) rather than merely reports.
///
/// ```asm
/// [0x00] mov  x0, #0          ; x0 = 0 (unmapped user VA)
/// [0x04] str  x1, [x0]        ; store to unmapped VA → data abort
/// ```
const USER_FAULT_PROGRAM_BYTES: [u8; 16] = {
    let mut buf = [0u8; 16];
    // [0x00] mov x0, #0 = MOVZ x0, #0 = 0xD2800000
    buf[0] = 0x00; buf[1] = 0x00; buf[2] = 0x80; buf[3] = 0xD2;
    // [0x04] str x1, [x0] = STR x1, [x0, #0] = 0xF9000001
    buf[4] = 0x01; buf[5] = 0x00; buf[6] = 0x00; buf[7] = 0xF9;
    // bytes 8..15 are padding (zeroed) — never executed
    buf
};

/// The M6 two-task demo: a *second* userspace program for task B.
///
/// Task A is `USER_PROGRAM_BYTES` (prints "hello, EL0!", yields, prints
/// "back from yield", exits). Task B is this program: prints "task B!",
/// yields, prints "B done!", exits. With the scheduler's round-robin
/// `yield`, the output interleaves A and B — proving two independent
/// tasks in two independent address spaces are sharing one CPU.
///
/// ```asm
/// [0x00] mov  x8, #1          ; SYS_WRITE
/// [0x04] mov  x0, #1          ; fd = stdout
/// [0x08] adr  x1, +0x30       ; buf -> 0x38 ("task B!\n\r")
/// [0x0c] mov  x2, #8          ; len
/// [0x10] svc  #0              ; syscall: write
/// [0x14] mov  x8, #3          ; SYS_YIELD
/// [0x18] svc  #0              ; syscall: yield (switches to task A)
/// [0x1c] mov  x8, #1          ; SYS_WRITE (again)
/// [0x20] mov  x0, #1          ; fd = stdout
/// [0x24] adr  x1, +0x14       ; buf -> 0x38 ("B done!\n\r")
/// [0x28] mov  x2, #8          ; len
/// [0x2c] svc  #0              ; syscall: write
/// [0x30] mov  x8, #2          ; SYS_EXIT
/// [0x34] svc  #0              ; syscall: exit
/// [0x38] "task B!\n\r\0"      ; 9 bytes + NUL
/// [0x48] "B done!\n\r\0"      ; 9 bytes + NUL
/// ```
const TASK_B_PROGRAM_BYTES: [u8; 128] = {
    let mut buf = [0u8; 128];
    // [0x00] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[0] = 0x28; buf[1] = 0x00; buf[2] = 0x80; buf[3] = 0xD2;
    // [0x04] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[4] = 0x20; buf[5] = 0x00; buf[6] = 0x80; buf[7] = 0xD2;
    // [0x08] adr x1, +0x30 (target 0x38) = offset 0x38-0x08 = 0x30 = 48
    //   immlo = 48 & 0x3 = 0, immhi = 48 >> 2 = 12 = 0xC
    //   = 0x10000000 | (0 << 29) | (0xC << 5) | 1 = 0x10000181
    buf[8] = 0x81; buf[9] = 0x01; buf[10] = 0x00; buf[11] = 0x10;
    // [0x0c] mov x2, #8 = MOVZ x2, #8 = 0xD2800102
    buf[12] = 0x02; buf[13] = 0x01; buf[14] = 0x80; buf[15] = 0xD2;
    // [0x10] svc #0 = 0xD4000001
    buf[16] = 0x01; buf[17] = 0x00; buf[18] = 0x00; buf[19] = 0xD4;
    // [0x14] mov x8, #3 = MOVZ x8, #3 = 0xD2800068
    buf[20] = 0x68; buf[21] = 0x00; buf[22] = 0x80; buf[23] = 0xD2;
    // [0x18] svc #0 = 0xD4000001
    buf[24] = 0x01; buf[25] = 0x00; buf[26] = 0x00; buf[27] = 0xD4;
    // [0x1c] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[28] = 0x28; buf[29] = 0x00; buf[30] = 0x80; buf[31] = 0xD2;
    // [0x20] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[32] = 0x20; buf[33] = 0x00; buf[34] = 0x80; buf[35] = 0xD2;
    // [0x24] adr x1, +0x24 (target 0x48) = offset 0x48-0x24 = 0x24 = 36
    //   immlo = 36 & 0x3 = 0, immhi = 36 >> 2 = 9
    //   = 0x10000000 | (0 << 29) | (9 << 5) | 1 = 0x10000121
    buf[36] = 0x21; buf[37] = 0x01; buf[38] = 0x00; buf[39] = 0x10;
    // [0x28] mov x2, #8 = MOVZ x2, #8 = 0xD2800102
    buf[40] = 0x02; buf[41] = 0x01; buf[42] = 0x80; buf[43] = 0xD2;
    // [0x2c] svc #0 = 0xD4000001
    buf[44] = 0x01; buf[45] = 0x00; buf[46] = 0x00; buf[47] = 0xD4;
    // [0x30] mov x8, #2 = MOVZ x8, #2 = 0xD2800048
    buf[48] = 0x48; buf[49] = 0x00; buf[50] = 0x80; buf[51] = 0xD2;
    // [0x34] svc #0 = 0xD4000001
    buf[52] = 0x01; buf[53] = 0x00; buf[54] = 0x00; buf[55] = 0xD4;
    // [0x38] "task B!\n\r\0" (9 bytes + NUL)
    buf[56] = b't'; buf[57] = b'a'; buf[58] = b's'; buf[59] = b'k';
    buf[60] = b' '; buf[61] = b'B'; buf[62] = b'!'; buf[63] = b'\n';
    buf[64] = b'\r'; buf[65] = 0x00;
    // [0x48] "B done!\n\r\0" (9 bytes + NUL)
    buf[72] = b'B'; buf[73] = b' '; buf[74] = b'd'; buf[75] = b'o';
    buf[76] = b'n'; buf[77] = b'e'; buf[78] = b'!'; buf[79] = b'\n';
    buf[80] = b'\r'; buf[81] = 0x00;
    buf
};

/// The M6 preemptive scheduling demo: two programs that write one
/// character and then **spin forever** — no `yield` syscall, no
/// `exit`. The only way the other task can run is if the **timer
/// preempts** the spinning task. This is the proof that preemption
/// works: if both "A" and "B" appear on the console, the timer IRQ
/// switched tasks without any cooperation from the user programs.
///
/// Task A writes "A\n" then branches to itself (`b .` — an infinite
/// loop at a single instruction). Task B writes "B\n" then does the
/// same. Neither calls `yield` or `exit`. The scheduler's
/// `save_and_switch` is called from the timer IRQ handler
/// (`handle_irq` in vectors.rs), preempting whichever task is
/// spinning and eret-ing into the other.
///
/// ```asm
/// [0x00] mov  x8, #1          ; SYS_WRITE
/// [0x04] mov  x0, #1          ; fd = stdout
/// [0x08] adr  x1, +0x14       ; buf -> 0x1C (the message)
/// [0x0c] mov  x2, #2          ; len = 2
/// [0x10] svc  #0              ; write "A\n"
/// [0x14] b    0x14            ; spin forever — the timer must preempt
/// [0x18] 0, 0, 0, 0           ; padding
/// [0x1c] "A\n\0"              ; the message
/// ```
const TASK_PREEMPT_A_BYTES: [u8; 32] = {
    let mut buf = [0u8; 32];
    // [0x00] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[0] = 0x28; buf[1] = 0x00; buf[2] = 0x80; buf[3] = 0xD2;
    // [0x04] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[4] = 0x20; buf[5] = 0x00; buf[6] = 0x80; buf[7] = 0xD2;
    // [0x08] adr x1, +0x14 (target 0x1C, offset = 0x14 = 20)
    //   immlo = 20 & 0x3 = 0, immhi = 20 >> 2 = 5
    //   = 0x10000000 | (0 << 29) | (5 << 5) | 1 = 0x100000A1
    buf[8] = 0xA1; buf[9] = 0x00; buf[10] = 0x00; buf[11] = 0x10;
    // [0x0c] mov x2, #2 = MOVZ x2, #2 = 0xD2800042
    buf[12] = 0x42; buf[13] = 0x00; buf[14] = 0x80; buf[15] = 0xD2;
    // [0x10] svc #0 = 0xD4000001
    buf[16] = 0x01; buf[17] = 0x00; buf[18] = 0x00; buf[19] = 0xD4;
    // [0x14] b . (branch to self, offset 0) = 0x14000000
    buf[20] = 0x00; buf[21] = 0x00; buf[22] = 0x00; buf[23] = 0x14;
    // [0x18] padding (4 bytes, zeroed)
    // [0x1c] "A\n\0"
    buf[28] = b'A'; buf[29] = b'\n'; buf[30] = 0x00;
    buf
};

/// Task B for the preemptive demo: writes "B\n" then spins. See
/// [`TASK_PREEMPT_A_BYTES`] for the design.
const TASK_PREEMPT_B_BYTES: [u8; 32] = {
    let mut buf = [0u8; 32];
    // [0x00] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[0] = 0x28; buf[1] = 0x00; buf[2] = 0x80; buf[3] = 0xD2;
    // [0x04] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[4] = 0x20; buf[5] = 0x00; buf[6] = 0x80; buf[7] = 0xD2;
    // [0x08] adr x1, +0x14 (target 0x1C) = 0x100000A1
    buf[8] = 0xA1; buf[9] = 0x00; buf[10] = 0x00; buf[11] = 0x10;
    // [0x0c] mov x2, #2 = 0xD2800042
    buf[12] = 0x42; buf[13] = 0x00; buf[14] = 0x80; buf[15] = 0xD2;
    // [0x10] svc #0 = 0xD4000001
    buf[16] = 0x01; buf[17] = 0x00; buf[18] = 0x00; buf[19] = 0xD4;
    // [0x14] b . = 0x14000000
    buf[20] = 0x00; buf[21] = 0x00; buf[22] = 0x00; buf[23] = 0x14;
    // [0x1c] "B\n\0"
    buf[28] = b'B'; buf[29] = b'\n'; buf[30] = 0x00;
    buf
};

/// The M6 IPC demo: two tasks that exchange a message.
///
/// Task A (the sender) writes "A: sending", sends the 8-byte message
/// "hello B!" to task B (TID 2) via `SYS_SEND`, yields so B can run,
/// writes "A: sent", and exits.
///
/// Task B (the receiver) writes "B: waiting", yields so A can send,
/// calls `SYS_RECV` with a buffer on its stack page (writable), then
/// writes "B: got msg!" and exits. If the IPC path works, the console
/// shows both tasks' messages and the round-trip is proven.
///
/// ```asm
/// [0x00] mov  x8, #1          ; SYS_WRITE
/// [0x04] mov  x0, #1          ; fd = stdout
/// [0x08] adr  x1, +0x44       ; buf -> 0x4c ("A: sending\n\r")
/// [0x0c] mov  x2, #12         ; len
/// [0x10] svc  #0              ; write "A: sending"
/// [0x14] mov  x8, #4          ; SYS_SEND
/// [0x18] mov  x0, #2          ; dst_tid = 2 (task B)
/// [0x1c] adr  x1, +0x48       ; buf -> 0x64 ("hello B!")
/// [0x20] mov  x2, #8          ; len = 8
/// [0x24] svc  #0              ; send "hello B!" to task B
/// [0x28] mov  x8, #3          ; SYS_YIELD
/// [0x2c] svc  #0              ; yield (let B run and receive)
/// [0x30] mov  x8, #1          ; SYS_WRITE
/// [0x34] mov  x0, #1          ; fd = stdout
/// [0x38] adr  x1, +0x34       ; buf -> 0x6c ("A: sent\n\r")
/// [0x3c] mov  x2, #8          ; len = 8
/// [0x40] svc  #0              ; write "A: sent"
/// [0x44] mov  x8, #2          ; SYS_EXIT
/// [0x48] svc  #0              ; exit
/// [0x4c] "A: sending\n\r\0"  ; 13 bytes
/// [0x64] "hello B!\0"          ; 9 bytes
/// [0x6c] "A: sent\n\r\0"      ; 9 bytes
/// ```
const TASK_IPC_SENDER_BYTES: [u8; 128] = {
    let mut buf = [0u8; 128];
    // [0x00] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[0] = 0x28; buf[1] = 0x00; buf[2] = 0x80; buf[3] = 0xD2;
    // [0x04] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[4] = 0x20; buf[5] = 0x00; buf[6] = 0x80; buf[7] = 0xD2;
    // [0x08] adr x1, +0x44 (target 0x4c, offset 0x44 = 68)
    //   immlo = 68 & 3 = 0, immhi = 68 >> 2 = 17
    //   = 0x10000000 | (0 << 29) | (17 << 5) | 1 = 0x10000221
    buf[8] = 0x21; buf[9] = 0x02; buf[10] = 0x00; buf[11] = 0x10;
    // [0x0c] mov x2, #12 = MOVZ x2, #12 = 0xD2800182
    buf[12] = 0x82; buf[13] = 0x01; buf[14] = 0x80; buf[15] = 0xD2;
    // [0x10] svc #0 = 0xD4000001
    buf[16] = 0x01; buf[17] = 0x00; buf[18] = 0x00; buf[19] = 0xD4;
    // [0x14] mov x8, #4 = MOVZ x8, #4 = 0xD2800088
    buf[20] = 0x88; buf[21] = 0x00; buf[22] = 0x80; buf[23] = 0xD2;
    // [0x18] mov x0, #2 = MOVZ x0, #2 = 0xD2800040
    buf[24] = 0x40; buf[25] = 0x00; buf[26] = 0x80; buf[27] = 0xD2;
    // [0x1c] adr x1, +0x48 (target 0x64, offset 0x48 = 72)
    //   immlo = 72 & 3 = 0, immhi = 72 >> 2 = 18
    //   = 0x10000000 | (0 << 29) | (18 << 5) | 1 = 0x10000241
    buf[28] = 0x41; buf[29] = 0x02; buf[30] = 0x00; buf[31] = 0x10;
    // [0x20] mov x2, #8 = MOVZ x2, #8 = 0xD2800102
    buf[32] = 0x02; buf[33] = 0x01; buf[34] = 0x80; buf[35] = 0xD2;
    // [0x24] svc #0 = 0xD4000001
    buf[36] = 0x01; buf[37] = 0x00; buf[38] = 0x00; buf[39] = 0xD4;
    // [0x28] mov x8, #3 = MOVZ x8, #3 = 0xD2800068
    buf[40] = 0x68; buf[41] = 0x00; buf[42] = 0x80; buf[43] = 0xD2;
    // [0x2c] svc #0 = 0xD4000001
    buf[44] = 0x01; buf[45] = 0x00; buf[46] = 0x00; buf[47] = 0xD4;
    // [0x30] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[48] = 0x28; buf[49] = 0x00; buf[50] = 0x80; buf[51] = 0xD2;
    // [0x34] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[52] = 0x20; buf[53] = 0x00; buf[54] = 0x80; buf[55] = 0xD2;
    // [0x38] adr x1, +0x34 (target 0x6c, offset 0x34 = 52)
    //   immlo = 52 & 3 = 0, immhi = 52 >> 2 = 13
    //   = 0x10000000 | (0 << 29) | (13 << 5) | 1 = 0x100001A1
    buf[56] = 0xA1; buf[57] = 0x01; buf[58] = 0x00; buf[59] = 0x10;
    // [0x3c] mov x2, #8 = MOVZ x2, #8 = 0xD2800102
    buf[60] = 0x02; buf[61] = 0x01; buf[62] = 0x80; buf[63] = 0xD2;
    // [0x40] svc #0 = 0xD4000001
    buf[64] = 0x01; buf[65] = 0x00; buf[66] = 0x00; buf[67] = 0xD4;
    // [0x44] mov x8, #2 = MOVZ x8, #2 = 0xD2800048
    buf[68] = 0x48; buf[69] = 0x00; buf[70] = 0x80; buf[71] = 0xD2;
    // [0x48] svc #0 = 0xD4000001
    buf[72] = 0x01; buf[73] = 0x00; buf[74] = 0x00; buf[75] = 0xD4;
    // [0x4c] "A: sending\n\r\0" (13 bytes)
    buf[76] = b'A'; buf[77] = b':'; buf[78] = b' '; buf[79] = b's';
    buf[80] = b'e'; buf[81] = b'n'; buf[82] = b'd'; buf[83] = b'i';
    buf[84] = b'n'; buf[85] = b'g'; buf[86] = b'\n'; buf[87] = b'\r';
    buf[88] = 0x00;
    // [0x64] "hello B!\0" (9 bytes)
    buf[100] = b'h'; buf[101] = b'e'; buf[102] = b'l'; buf[103] = b'l';
    buf[104] = b'o'; buf[105] = b' '; buf[106] = b'B'; buf[107] = b'!';
    buf[108] = 0x00;
    // [0x6c] "A: sent\n\r\0" (9 bytes)
    buf[108] = b'A'; buf[109] = b':'; buf[110] = b' '; buf[111] = b's';
    buf[112] = b'e'; buf[113] = b'n'; buf[114] = b't'; buf[115] = b'\n';
    buf[116] = b'\r'; buf[117] = 0x00;
    buf
};

/// Task B (the receiver) for the M6 IPC demo.
///
/// Writes "B: waiting", yields so A can send, calls `SYS_RECV` with a
/// buffer on the stack page (writable, at VA 0x200100), then writes
/// "B: got msg!" and exits. If A's message arrived, the kernel copies
/// it into the recv buffer; B doesn't print the message contents
/// (that would need a second write syscall with the recv buffer as
/// the source — this beat proves the path, not the payload).
///
/// ```asm
/// [0x00] mov  x8, #1          ; SYS_WRITE
/// [0x04] mov  x0, #1          ; fd = stdout
/// [0x08] adr  x1, +0x44       ; buf -> 0x4c ("B: waiting\n\r")
/// [0x0c] mov  x2, #12         ; len
/// [0x10] svc  #0              ; write "B: waiting"
/// [0x14] mov  x8, #3          ; SYS_YIELD
/// [0x18] svc  #0              ; yield (let A send)
/// [0x1c] mov  x8, #5          ; SYS_RECV
/// [0x20] movz x0, #0x100      ; buf_va low 16 bits = 0x100
/// [0x24] movk x0, #0x20, lsl #16 ; buf_va = 0x200100 (stack page, writable)
/// [0x28] mov  x1, #32         ; buf_len = 32
/// [0x2c] svc  #0              ; recv -> x0=bytes, x1=sender_tid
/// [0x30] mov  x8, #1          ; SYS_WRITE
/// [0x34] mov  x0, #1          ; fd = stdout
/// [0x38] adr  x1, +0x2c       ; buf -> 0x64 ("B: got msg!\n\r")
/// [0x3c] mov  x2, #13         ; len = 13
/// [0x40] svc  #0              ; write "B: got msg!"
/// [0x44] mov  x8, #2          ; SYS_EXIT
/// [0x48] svc  #0              ; exit
/// [0x4c] "B: waiting\n\r\0"  ; 13 bytes
/// [0x64] "B: got msg!\n\r\0" ; 13 bytes
/// ```
const TASK_IPC_RECEIVER_BYTES: [u8; 128] = {
    let mut buf = [0u8; 128];
    // [0x00] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[0] = 0x28; buf[1] = 0x00; buf[2] = 0x80; buf[3] = 0xD2;
    // [0x04] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[4] = 0x20; buf[5] = 0x00; buf[6] = 0x80; buf[7] = 0xD2;
    // [0x08] adr x1, +0x44 (target 0x4c, offset 0x44 = 68)
    //   immlo = 0, immhi = 17 = 0x10000221
    buf[8] = 0x21; buf[9] = 0x02; buf[10] = 0x00; buf[11] = 0x10;
    // [0x0c] mov x2, #12 = MOVZ x2, #12 = 0xD2800182
    buf[12] = 0x82; buf[13] = 0x01; buf[14] = 0x80; buf[15] = 0xD2;
    // [0x10] svc #0 = 0xD4000001
    buf[16] = 0x01; buf[17] = 0x00; buf[18] = 0x00; buf[19] = 0xD4;
    // [0x14] mov x8, #3 = MOVZ x8, #3 = 0xD2800068
    buf[20] = 0x68; buf[21] = 0x00; buf[22] = 0x80; buf[23] = 0xD2;
    // [0x18] svc #0 = 0xD4000001
    buf[24] = 0x01; buf[25] = 0x00; buf[26] = 0x00; buf[27] = 0xD4;
    // [0x1c] mov x8, #5 = MOVZ x8, #5 = 0xD28000A8
    buf[28] = 0xA8; buf[29] = 0x00; buf[30] = 0x80; buf[31] = 0xD2;
    // [0x20] movz x0, #0x100 = MOVZ x0, #0x100 = 0xD2802000
    buf[32] = 0x00; buf[33] = 0x20; buf[34] = 0x80; buf[35] = 0xD2;
    // [0x24] movk x0, #0x20, lsl #16 = MOVK x0, #0x20, LSL #16 = 0xF2A00400
    buf[36] = 0x00; buf[37] = 0x04; buf[38] = 0xA0; buf[39] = 0xF2;
    // [0x28] mov x1, #32 = MOVZ x1, #32 = 0xD2800C21
    buf[40] = 0x21; buf[41] = 0x0C; buf[42] = 0x80; buf[43] = 0xD2;
    // [0x2c] svc #0 = 0xD4000001
    buf[44] = 0x01; buf[45] = 0x00; buf[46] = 0x00; buf[47] = 0xD4;
    // [0x30] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[48] = 0x28; buf[49] = 0x00; buf[50] = 0x80; buf[51] = 0xD2;
    // [0x34] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[52] = 0x20; buf[53] = 0x00; buf[54] = 0x80; buf[55] = 0xD2;
    // [0x38] adr x1, +0x2c (target 0x64, offset 0x2c = 44)
    //   immlo = 0, immhi = 11 = 0x10000161
    buf[56] = 0x61; buf[57] = 0x01; buf[58] = 0x00; buf[59] = 0x10;
    // [0x3c] mov x2, #13 = MOVZ x2, #13 = 0xD28001A2
    buf[60] = 0xA2; buf[61] = 0x01; buf[62] = 0x80; buf[63] = 0xD2;
    // [0x40] svc #0 = 0xD4000001
    buf[64] = 0x01; buf[65] = 0x00; buf[66] = 0x00; buf[67] = 0xD4;
    // [0x44] mov x8, #2 = MOVZ x8, #2 = 0xD2800048
    buf[68] = 0x48; buf[69] = 0x00; buf[70] = 0x80; buf[71] = 0xD2;
    // [0x48] svc #0 = 0xD4000001
    buf[72] = 0x01; buf[73] = 0x00; buf[74] = 0x00; buf[75] = 0xD4;
    // [0x4c] "B: waiting\n\r\0" (13 bytes)
    buf[76] = b'B'; buf[77] = b':'; buf[78] = b' '; buf[79] = b'w';
    buf[80] = b'a'; buf[81] = b'i'; buf[82] = b't'; buf[83] = b'i';
    buf[84] = b'n'; buf[85] = b'g'; buf[86] = b'\n'; buf[87] = b'\r';
    buf[88] = 0x00;
    // [0x64] "B: got msg!\n\r\0" (13 bytes)
    buf[100] = b'B'; buf[101] = b':'; buf[102] = b' '; buf[103] = b'g';
    buf[104] = b'o'; buf[105] = b't'; buf[106] = b' '; buf[107] = b'm';
    buf[108] = b's'; buf[109] = b'g'; buf[110] = b'!'; buf[111] = b'\n';
    buf[112] = b'\r'; buf[113] = 0x00;
    buf
};

/// The M6 blocking-IPC demo: a sender and a receiver where the receiver
/// *blocks* on recvblk instead of yielding and retrying.
///
/// Task A (the sender) writes "A: send", yields (so B can run and block
/// on its recvblk), then sends "wake!" to task B (TID 2) via SYS_SEND,
/// writes "A: done", and exits. The yield is the key ordering step: it
/// ensures B has called recvblk and gone Blocked *before* A sends. When
/// A sends, ipc_send sees B is Blocked, wakes it (Ready, enqueued), and
/// the scheduler resumes B — which re-enters the recvblk svc, finds the
/// message waiting, and returns it.
///
/// Task B (the receiver) writes "B: block", then calls SYS_RECVBLK
/// (syscall 6) with a buffer on its stack page. The mailbox is empty
/// (A hasn't sent yet), so the kernel blocks B (Blocked state, not
/// enqueued) and switches to A. When A sends and wakes B, B resumes,
/// re-enters the svc, finds "wake!" in its mailbox, and continues to
/// write "B: woke!" and exit.
///
/// This proves the Blocked state end-to-end: a task sleeps on a
/// condition (empty mailbox), another task satisfies the condition
/// (sends a message), and the kernel wakes the sleeper — the same
/// pattern a real OS uses for blocking read(), wait(), and futex.
///
/// Layout (code 0x00..0x4c, data 0x4c onward — no overlap):
/// ```asm
/// [0x00] mov  x8, #1          ; SYS_WRITE
/// [0x04] mov  x0, #1          ; fd = stdout
/// [0x08] adr  x1, +0x44       ; buf -> 0x4c ("A: send\n\r")
/// [0x0c] mov  x2, #9          ; len
/// [0x10] svc  #0              ; write "A: send"
/// [0x14] mov  x8, #3          ; SYS_YIELD
/// [0x18] svc  #0              ; yield (let B block on recvblk first)
/// [0x1c] mov  x8, #4          ; SYS_SEND
/// [0x20] mov  x0, #2          ; dst_tid = 2 (task B)
/// [0x24] adr  x1, +0x38       ; buf -> 0x5c ("wake!")
/// [0x28] mov  x2, #5          ; len = 5
/// [0x2c] svc  #0              ; send "wake!" — wakes B if Blocked
/// [0x30] mov  x8, #1          ; SYS_WRITE
/// [0x34] mov  x0, #1          ; fd = stdout
/// [0x38] adr  x1, +0x34       ; buf -> 0x6c ("A: done\n\r")
/// [0x3c] mov  x2, #9          ; len = 9
/// [0x40] svc  #0              ; write "A: done"
/// [0x44] mov  x8, #2          ; SYS_EXIT
/// [0x48] svc  #0              ; exit
/// [0x4c] "A: send\n\r\0"      ; 9 bytes
/// [0x5c] "wake!\0"            ; 6 bytes
/// [0x6c] "A: done\n\r\0"      ; 9 bytes
/// ```
const TASK_BLKIPC_SENDER_BYTES: [u8; 128] = {
    let mut buf = [0u8; 128];
    // [0x00] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[0] = 0x28; buf[1] = 0x00; buf[2] = 0x80; buf[3] = 0xD2;
    // [0x04] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[4] = 0x20; buf[5] = 0x00; buf[6] = 0x80; buf[7] = 0xD2;
    // [0x08] adr x1, +0x44 (target 0x4c, offset 68)
    //   immlo = 0, immhi = 17 = 0x10000221
    buf[8] = 0x21; buf[9] = 0x02; buf[10] = 0x00; buf[11] = 0x10;
    // [0x0c] mov x2, #9 = MOVZ x2, #9 = 0xD2800122
    buf[12] = 0x22; buf[13] = 0x01; buf[14] = 0x80; buf[15] = 0xD2;
    // [0x10] svc #0 = 0xD4000001
    buf[16] = 0x01; buf[17] = 0x00; buf[18] = 0x00; buf[19] = 0xD4;
    // [0x14] mov x8, #3 = MOVZ x8, #3 = 0xD2800068
    buf[20] = 0x68; buf[21] = 0x00; buf[22] = 0x80; buf[23] = 0xD2;
    // [0x18] svc #0 = 0xD4000001
    buf[24] = 0x01; buf[25] = 0x00; buf[26] = 0x00; buf[27] = 0xD4;
    // [0x1c] mov x8, #4 = MOVZ x8, #4 = 0xD2800088
    buf[28] = 0x88; buf[29] = 0x00; buf[30] = 0x80; buf[31] = 0xD2;
    // [0x20] mov x0, #2 = MOVZ x0, #2 = 0xD2800040
    buf[32] = 0x40; buf[33] = 0x00; buf[34] = 0x80; buf[35] = 0xD2;
    // [0x24] adr x1, +0x38 (target 0x5c, offset 56)
    //   immlo = 0, immhi = 14 = 0x100001C1
    buf[36] = 0xC1; buf[37] = 0x01; buf[38] = 0x00; buf[39] = 0x10;
    // [0x28] mov x2, #5 = MOVZ x2, #5 = 0xD28000A2
    buf[40] = 0xA2; buf[41] = 0x00; buf[42] = 0x80; buf[43] = 0xD2;
    // [0x2c] svc #0 = 0xD4000001
    buf[44] = 0x01; buf[45] = 0x00; buf[46] = 0x00; buf[47] = 0xD4;
    // [0x30] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[48] = 0x28; buf[49] = 0x00; buf[50] = 0x80; buf[51] = 0xD2;
    // [0x34] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[52] = 0x20; buf[53] = 0x00; buf[54] = 0x80; buf[55] = 0xD2;
    // [0x38] adr x1, +0x34 (target 0x6c, offset 52)
    //   immlo = 0, immhi = 13 = 0x100001A1
    buf[56] = 0xA1; buf[57] = 0x01; buf[58] = 0x00; buf[59] = 0x10;
    // [0x3c] mov x2, #9 = MOVZ x2, #9 = 0xD2800122
    buf[60] = 0x22; buf[61] = 0x01; buf[62] = 0x80; buf[63] = 0xD2;
    // [0x40] svc #0 = 0xD4000001
    buf[64] = 0x01; buf[65] = 0x00; buf[66] = 0x00; buf[67] = 0xD4;
    // [0x44] mov x8, #2 = MOVZ x8, #2 = 0xD2800048
    buf[68] = 0x48; buf[69] = 0x00; buf[70] = 0x80; buf[71] = 0xD2;
    // [0x48] svc #0 = 0xD4000001
    buf[72] = 0x01; buf[73] = 0x00; buf[74] = 0x00; buf[75] = 0xD4;
    // [0x4c] "A: send\n\r\0" (9 bytes) at offset 76
    buf[76] = b'A'; buf[77] = b':'; buf[78] = b' '; buf[79] = b's';
    buf[80] = b'e'; buf[81] = b'n'; buf[82] = b'd'; buf[83] = b'\n';
    buf[84] = b'\r'; buf[85] = 0x00;
    // [0x5c] "wake!\0" (6 bytes) at offset 92
    buf[92] = b'w'; buf[93] = b'a'; buf[94] = b'k'; buf[95] = b'e';
    buf[96] = b'!'; buf[97] = 0x00;
    // [0x6c] "A: done\n\r\0" (9 bytes) at offset 108
    buf[108] = b'A'; buf[109] = b':'; buf[110] = b' '; buf[111] = b'd';
    buf[112] = b'o'; buf[113] = b'n'; buf[114] = b'e'; buf[115] = b'\n';
    buf[116] = b'\r'; buf[117] = 0x00;
    buf
};

/// Task B (receiver) for the blocking-IPC demo.
///
/// Writes "B: block", then calls SYS_RECVBLK (6) with a buffer on its
/// stack page. If the mailbox is empty, the kernel blocks B and switches
/// to A. When A sends and wakes B, B re-enters the recvblk svc, finds
/// the message, and continues to write "B: woke!" and exit.
///
/// Layout (code 0x00..0x44, data 0x44 onward):
/// ```asm
/// [0x00] mov  x8, #1          ; SYS_WRITE
/// [0x04] mov  x0, #1          ; fd = stdout
/// [0x08] adr  x1, +0x3c       ; buf -> 0x44 ("B: block\n\r")
/// [0x0c] mov  x2, #10         ; len = 10
/// [0x10] svc  #0              ; write "B: block"
/// [0x14] mov  x8, #6          ; SYS_RECVBLK
/// [0x18] movz x0, #0x100      ; buf_va low 16 bits
/// [0x1c] movk x0, #0x20, lsl #16 ; buf_va = 0x200100 (stack page)
/// [0x20] mov  x1, #32         ; buf_len = 32
/// [0x24] svc  #0              ; recvblk — blocks if empty, returns msg
/// [0x28] mov  x8, #1          ; SYS_WRITE
/// [0x2c] mov  x0, #1          ; fd = stdout
/// [0x30] adr  x1, +0x34       ; buf -> 0x64 ("B: woke!\n\r")
/// [0x34] mov  x2, #10         ; len = 10
/// [0x38] svc  #0              ; write "B: woke!"
/// [0x3c] mov  x8, #2          ; SYS_EXIT
/// [0x40] svc  #0              ; exit
/// [0x44] "B: block\n\r\0"     ; 10 bytes
/// [0x64] "B: woke!\n\r\0"     ; 10 bytes
/// ```
const TASK_BLKIPC_RECEIVER_BYTES: [u8; 128] = {
    let mut buf = [0u8; 128];
    // [0x00] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[0] = 0x28; buf[1] = 0x00; buf[2] = 0x80; buf[3] = 0xD2;
    // [0x04] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[4] = 0x20; buf[5] = 0x00; buf[6] = 0x80; buf[7] = 0xD2;
    // [0x08] adr x1, +0x3c (target 0x44, offset 60)
    //   immlo = 0, immhi = 15 = 0x100001E1
    buf[8] = 0xE1; buf[9] = 0x01; buf[10] = 0x00; buf[11] = 0x10;
    // [0x0c] mov x2, #10 = MOVZ x2, #10 = 0xD2800142
    buf[12] = 0x42; buf[13] = 0x01; buf[14] = 0x80; buf[15] = 0xD2;
    // [0x10] svc #0 = 0xD4000001
    buf[16] = 0x01; buf[17] = 0x00; buf[18] = 0x00; buf[19] = 0xD4;
    // [0x14] mov x8, #6 = MOVZ x8, #6 = 0xD28000C8
    buf[20] = 0xC8; buf[21] = 0x00; buf[22] = 0x80; buf[23] = 0xD2;
    // [0x18] movz x0, #0x100 = MOVZ x0, #0x100 = 0xD2802000
    buf[24] = 0x00; buf[25] = 0x20; buf[26] = 0x80; buf[27] = 0xD2;
    // [0x1c] movk x0, #0x20, lsl #16 = MOVK x0, #0x20, LSL #16 = 0xF2A00400
    buf[28] = 0x00; buf[29] = 0x04; buf[30] = 0xA0; buf[31] = 0xF2;
    // [0x20] mov x1, #32 = MOVZ x1, #32 = 0xD2800C21
    buf[32] = 0x21; buf[33] = 0x0C; buf[34] = 0x80; buf[35] = 0xD2;
    // [0x24] svc #0 = 0xD4000001
    buf[36] = 0x01; buf[37] = 0x00; buf[38] = 0x00; buf[39] = 0xD4;
    // [0x28] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[40] = 0x28; buf[41] = 0x00; buf[42] = 0x80; buf[43] = 0xD2;
    // [0x2c] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[44] = 0x20; buf[45] = 0x00; buf[46] = 0x80; buf[47] = 0xD2;
    // [0x30] adr x1, +0x34 (target 0x64, offset 52)
    //   immlo = 0, immhi = 13 = 0x100001A1
    buf[48] = 0xA1; buf[49] = 0x01; buf[50] = 0x00; buf[51] = 0x10;
    // [0x34] mov x2, #10 = MOVZ x2, #10 = 0xD2800142
    buf[52] = 0x42; buf[53] = 0x01; buf[54] = 0x80; buf[55] = 0xD2;
    // [0x38] svc #0 = 0xD4000001
    buf[56] = 0x01; buf[57] = 0x00; buf[58] = 0x00; buf[59] = 0xD4;
    // [0x3c] mov x8, #2 = MOVZ x8, #2 = 0xD2800048
    buf[60] = 0x48; buf[61] = 0x00; buf[62] = 0x80; buf[63] = 0xD2;
    // [0x40] svc #0 = 0xD4000001
    buf[64] = 0x01; buf[65] = 0x00; buf[66] = 0x00; buf[67] = 0xD4;
    // [0x44] "B: block\n\r\0" (10 bytes) at offset 68
    buf[68] = b'B'; buf[69] = b':'; buf[70] = b' '; buf[71] = b'b';
    buf[72] = b'l'; buf[73] = b'o'; buf[74] = b'c'; buf[75] = b'k';
    buf[76] = b'\n'; buf[77] = b'\r'; buf[78] = 0x00;
    // [0x64] "B: woke!\n\r\0" (10 bytes) at offset 100
    buf[100] = b'B'; buf[101] = b':'; buf[102] = b' '; buf[103] = b'w';
    buf[104] = b'o'; buf[105] = b'k'; buf[106] = b'e'; buf[107] = b'!';
    buf[108] = b'\n'; buf[109] = b'\r'; buf[110] = 0x00;
    buf
};

/// The M6 blocking-send demo: a sender that calls `sendblk` (SYS_SENDBLK,
/// syscall 7) and a receiver that drains its mailbox, waking the blocked
/// sender. This is the symmetric counterpart to the blocking-recv demo
/// (`blkipc`): there the receiver slept on "mailbox empty"; here the
/// sender sleeps on "mailbox full."
///
/// Task A (the sender) writes "A: send2", sends "first" to task B via
/// `sendblk` (succeeds — B's mailbox is empty), then immediately sends
/// "second" via `sendblk` again. B's mailbox is *full* (it hasn't read
/// "first" yet), so A **blocks** — the kernel puts A to sleep and
/// switches to B. B writes "B: recv2", calls `recv` (drains the mailbox,
/// which wakes A), yields back to A. A retries the `sendblk` — the
/// mailbox is now empty, so "second" lands. A writes "A: sent2" and
/// exits. B runs again, `recv`s "second", writes "B: got2" and exits.
///
/// The expected console output (interleaved by the scheduler):
/// ```
/// A: send2
/// B: recv2
/// A: sent2
/// B: got2
/// ```
/// The key moment: A blocks on the second `sendblk`, B's `recv` wakes
/// it, and A's retry succeeds — the "retry the syscall on wake" pattern
/// working in the send direction for the first time.
///
/// Layout (code 0x00..0x58, data 0x5c onward — no overlap):
/// ```asm
/// [0x00] mov  x8, #1          ; SYS_WRITE
/// [0x04] mov  x0, #1          ; fd = stdout
/// [0x08] adr  x1, +0x54       ; buf -> 0x5c ("A: send2\n\r")
/// [0x0c] mov  x2, #10         ; len
/// [0x10] svc  #0              ; write "A: send2"
/// [0x14] mov  x8, #7          ; SYS_SENDBLK
/// [0x18] mov  x0, #2          ; dst_tid = 2 (task B)
/// [0x1c] adr  x1, +0x44       ; buf -> 0x60 ("first")
/// [0x20] mov  x2, #5          ; len = 5
/// [0x24] svc  #0              ; sendblk "first" — succeeds (mailbox empty)
/// [0x28] mov  x8, #7          ; SYS_SENDBLK (again)
/// [0x2c] mov  x0, #2          ; dst_tid = 2
/// [0x30] adr  x1, +0x38       ; buf -> 0x68 ("second")
/// [0x34] mov  x2, #6          ; len = 6
/// [0x38] svc  #0              ; sendblk "second" — BLOCKS (mailbox full)
/// ;; --- A is woken when B drains its mailbox via recv ---
/// [0x3c] mov  x8, #1          ; SYS_WRITE
/// [0x40] mov  x0, #1          ; fd = stdout
/// [0x44] adr  x1, +0x30       ; buf -> 0x74 ("A: sent2\n\r")
/// [0x48] mov  x2, #10         ; len
/// [0x4c] svc  #0              ; write "A: sent2"
/// [0x50] mov  x8, #2          ; SYS_EXIT
/// [0x54] svc  #0              ; exit
/// [0x5c] "A: send2\n\r\0"    ; 10 bytes
/// [0x60] "first\0"            ; 6 bytes
/// [0x68] "second\0"           ; 7 bytes
/// [0x74] "A: sent2\n\r\0"    ; 10 bytes
/// ```
const TASK_SENDBLK_SENDER_BYTES: [u8; 128] = {
    let mut buf = [0u8; 128];
    // [0x00] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[0] = 0x28; buf[1] = 0x00; buf[2] = 0x80; buf[3] = 0xD2;
    // [0x04] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[4] = 0x20; buf[5] = 0x00; buf[6] = 0x80; buf[7] = 0xD2;
    // [0x08] adr x1, +0x50 (target 0x58, offset 80)
    //   immlo = 0, immhi = 20 = 0x10000281
    buf[8] = 0x81; buf[9] = 0x02; buf[10] = 0x00; buf[11] = 0x10;
    // [0x0c] mov x2, #10 = MOVZ x2, #10 = 0xD2800142
    buf[12] = 0x42; buf[13] = 0x01; buf[14] = 0x80; buf[15] = 0xD2;
    // [0x10] svc #0 = 0xD4000001
    buf[16] = 0x01; buf[17] = 0x00; buf[18] = 0x00; buf[19] = 0xD4;
    // [0x14] mov x8, #7 = MOVZ x8, #7 = 0xD28000E8
    buf[20] = 0xE8; buf[21] = 0x00; buf[22] = 0x80; buf[23] = 0xD2;
    // [0x18] mov x0, #2 = MOVZ x0, #2 = 0xD2800040
    buf[24] = 0x40; buf[25] = 0x00; buf[26] = 0x80; buf[27] = 0xD2;
    // [0x1c] adr x1, +0x48 (target 0x64, offset 72)
    //   immlo = 0, immhi = 18 = 0x10000241
    buf[28] = 0x41; buf[29] = 0x02; buf[30] = 0x00; buf[31] = 0x10;
    // [0x20] mov x2, #5 = MOVZ x2, #5 = 0xD28000A2
    buf[32] = 0xA2; buf[33] = 0x00; buf[34] = 0x80; buf[35] = 0xD2;
    // [0x24] svc #0 = 0xD4000001
    buf[36] = 0x01; buf[37] = 0x00; buf[38] = 0x00; buf[39] = 0xD4;
    // [0x28] mov x8, #7 = MOVZ x8, #7 = 0xD28000E8
    buf[40] = 0xE8; buf[41] = 0x00; buf[42] = 0x80; buf[43] = 0xD2;
    // [0x2c] mov x0, #2 = MOVZ x0, #2 = 0xD2800040
    buf[44] = 0x40; buf[45] = 0x00; buf[46] = 0x80; buf[47] = 0xD2;
    // [0x30] adr x1, +0x3c (target 0x6c, offset 60)
    //   immlo = 0, immhi = 15 = 0x100001E1
    buf[48] = 0xE1; buf[49] = 0x01; buf[50] = 0x00; buf[51] = 0x10;
    // [0x34] mov x2, #6 = MOVZ x2, #6 = 0xD28000C2
    buf[52] = 0xC2; buf[53] = 0x00; buf[54] = 0x80; buf[55] = 0xD2;
    // [0x38] svc #0 — sendblk "second": BLOCKS if mailbox full
    buf[56] = 0x01; buf[57] = 0x00; buf[58] = 0x00; buf[59] = 0xD4;
    // [0x3c] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[60] = 0x28; buf[61] = 0x00; buf[62] = 0x80; buf[63] = 0xD2;
    // [0x40] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[64] = 0x20; buf[65] = 0x00; buf[66] = 0x80; buf[67] = 0xD2;
    // [0x44] adr x1, +0x30 (target 0x74, offset 48)
    //   immlo = 0, immhi = 12 = 0x10000181
    buf[68] = 0x81; buf[69] = 0x01; buf[70] = 0x00; buf[71] = 0x10;
    // [0x48] mov x2, #10 = MOVZ x2, #10 = 0xD2800142
    buf[72] = 0x42; buf[73] = 0x01; buf[74] = 0x80; buf[75] = 0xD2;
    // [0x4c] svc #0 = 0xD4000001
    buf[76] = 0x01; buf[77] = 0x00; buf[78] = 0x00; buf[79] = 0xD4;
    // [0x50] mov x8, #2 = MOVZ x8, #2 = 0xD2800048
    buf[80] = 0x48; buf[81] = 0x00; buf[82] = 0x80; buf[83] = 0xD2;
    // [0x54] svc #0 = 0xD4000001
    buf[84] = 0x01; buf[85] = 0x00; buf[86] = 0x00; buf[87] = 0xD4;
    // Data section — no overlaps:
    // [0x58] "A: send2\n\r\0" (10 bytes + NUL) at offset 88
    buf[88] = b'A'; buf[89] = b':'; buf[90] = b' '; buf[91] = b's';
    buf[92] = b'e'; buf[93] = b'n'; buf[94] = b'd'; buf[95] = b'2';
    buf[96] = b'\n'; buf[97] = b'\r'; buf[98] = 0x00;
    // [0x64] "first\0" (5 bytes + NUL) at offset 100
    buf[100] = b'f'; buf[101] = b'i'; buf[102] = b'r'; buf[103] = b's';
    buf[104] = b't'; buf[105] = 0x00;
    // [0x6c] "second\0" (6 bytes + NUL) at offset 108
    buf[108] = b's'; buf[109] = b'e'; buf[110] = b'c'; buf[111] = b'o';
    buf[112] = b'n'; buf[113] = b'd'; buf[114] = 0x00;
    // [0x74] "A: sent2\n\r\0" (10 bytes + NUL) at offset 116
    buf[116] = b'A'; buf[117] = b':'; buf[118] = b' '; buf[119] = b's';
    buf[120] = b'e'; buf[121] = b'n'; buf[122] = b't'; buf[123] = b'2';
    buf[124] = b'\n'; buf[125] = b'\r'; buf[126] = 0x00;
    buf
};

/// Task B (receiver) for the blocking-send demo.
///
/// Writes "B: recv2", calls `recv` (gets "first", drains the mailbox,
/// which wakes the blocked sender A), yields (letting A retry its
/// sendblk), calls `recv` again (gets "second"), writes "B: got2"
/// and exits.
///
/// Layout (code 0x00..0x60, data 0x60 onward — no overlap):
/// ```asm
/// [0x00] mov  x8, #1          ; SYS_WRITE
/// [0x04] mov  x0, #1          ; fd = stdout
/// [0x08] adr  x1, +0x58       ; buf -> 0x60 ("B: recv2\n\r")
/// [0x0c] mov  x2, #10         ; len
/// [0x10] svc  #0              ; write "B: recv2"
/// [0x14] mov  x8, #5          ; SYS_RECV
/// [0x18] movz x0, #0x100      ; buf_va low 16 bits
/// [0x1c] movk x0, #0x20, lsl #16 ; buf_va = 0x200100 (stack page)
/// [0x20] mov  x1, #32         ; buf_len = 32
/// [0x24] svc  #0              ; recv — gets "first", wakes blocked sender A
/// [0x28] mov  x8, #3          ; SYS_YIELD
/// [0x2c] svc  #0              ; yield — let A retry sendblk "second"
/// [0x30] mov  x8, #5          ; SYS_RECV (again)
/// [0x34] movz x0, #0x100      ; buf_va low 16 bits
/// [0x38] movk x0, #0x20, lsl #16
/// [0x3c] mov  x1, #32         ; buf_len = 32
/// [0x40] svc  #0              ; recv — gets "second"
/// [0x44] mov  x8, #1          ; SYS_WRITE
/// [0x48] mov  x0, #1          ; fd = stdout
/// [0x4c] adr  x1, +0x24       ; buf -> 0x70 ("B: got2\n\r")
/// [0x50] mov  x2, #9          ; len = 9
/// [0x54] svc  #0              ; write "B: got2"
/// [0x58] mov  x8, #2          ; SYS_EXIT
/// [0x5c] svc  #0              ; exit
/// [0x60] "B: recv2\n\r\0"    ; 10 bytes
/// [0x70] "B: got2\n\r\0"     ; 9 bytes
/// ```
const TASK_SENDBLK_RECEIVER_BYTES: [u8; 128] = {
    let mut buf = [0u8; 128];
    // [0x00] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[0] = 0x28; buf[1] = 0x00; buf[2] = 0x80; buf[3] = 0xD2;
    // [0x04] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[4] = 0x20; buf[5] = 0x00; buf[6] = 0x80; buf[7] = 0xD2;
    // [0x08] adr x1, +0x58 (target 0x60, offset 88)
    //   immlo = 88 & 3 = 0, immhi = 88 >> 2 = 22
    //   = 0x10000000 | (0 << 29) | (22 << 5) | 1 = 0x100002C1
    buf[8] = 0xC1; buf[9] = 0x02; buf[10] = 0x00; buf[11] = 0x10;
    // [0x0c] mov x2, #10 = MOVZ x2, #10 = 0xD2800142
    buf[12] = 0x42; buf[13] = 0x01; buf[14] = 0x80; buf[15] = 0xD2;
    // [0x10] svc #0 = 0xD4000001
    buf[16] = 0x01; buf[17] = 0x00; buf[18] = 0x00; buf[19] = 0xD4;
    // [0x14] mov x8, #5 = MOVZ x8, #5 = 0xD28000A8
    buf[20] = 0xA8; buf[21] = 0x00; buf[22] = 0x80; buf[23] = 0xD2;
    // [0x18] movz x0, #0x100 = MOVZ x0, #0x100 = 0xD2802000
    buf[24] = 0x00; buf[25] = 0x20; buf[26] = 0x80; buf[27] = 0xD2;
    // [0x1c] movk x0, #0x20, lsl #16 = MOVK x0, #0x20, LSL #16 = 0xF2A00400
    buf[28] = 0x00; buf[29] = 0x04; buf[30] = 0xA0; buf[31] = 0xF2;
    // [0x20] mov x1, #32 = MOVZ x1, #32 = 0xD2800C21
    buf[32] = 0x21; buf[33] = 0x0C; buf[34] = 0x80; buf[35] = 0xD2;
    // [0x24] svc #0 = 0xD4000001
    buf[36] = 0x01; buf[37] = 0x00; buf[38] = 0x00; buf[39] = 0xD4;
    // [0x28] mov x8, #3 = MOVZ x8, #3 = 0xD2800068
    buf[40] = 0x68; buf[41] = 0x00; buf[42] = 0x80; buf[43] = 0xD2;
    // [0x2c] svc #0 = 0xD4000001
    buf[44] = 0x01; buf[45] = 0x00; buf[46] = 0x00; buf[47] = 0xD4;
    // [0x30] mov x8, #5 = MOVZ x8, #5 = 0xD28000A8
    buf[48] = 0xA8; buf[49] = 0x00; buf[50] = 0x80; buf[51] = 0xD2;
    // [0x34] movz x0, #0x100 = MOVZ x0, #0x100 = 0xD2802000
    buf[52] = 0x00; buf[53] = 0x20; buf[54] = 0x80; buf[55] = 0xD2;
    // [0x38] movk x0, #0x20, lsl #16 = 0xF2A00400
    buf[56] = 0x00; buf[57] = 0x04; buf[58] = 0xA0; buf[59] = 0xF2;
    // [0x3c] mov x1, #32 = MOVZ x1, #32 = 0xD2800C21
    buf[60] = 0x21; buf[61] = 0x0C; buf[62] = 0x80; buf[63] = 0xD2;
    // [0x40] svc #0 = 0xD4000001
    buf[64] = 0x01; buf[65] = 0x00; buf[66] = 0x00; buf[67] = 0xD4;
    // [0x44] mov x8, #1 = MOVZ x8, #1 = 0xD2800028
    buf[68] = 0x28; buf[69] = 0x00; buf[70] = 0x80; buf[71] = 0xD2;
    // [0x48] mov x0, #1 = MOVZ x0, #1 = 0xD2800020
    buf[72] = 0x20; buf[73] = 0x00; buf[74] = 0x80; buf[75] = 0xD2;
    // [0x4c] adr x1, +0x24 (target 0x70, offset 36)
    //   immlo = 0, immhi = 9 = 0x10000121
    buf[76] = 0x21; buf[77] = 0x01; buf[78] = 0x00; buf[79] = 0x10;
    // [0x50] mov x2, #9 = MOVZ x2, #9 = 0xD2800122
    buf[80] = 0x22; buf[81] = 0x01; buf[82] = 0x80; buf[83] = 0xD2;
    // [0x54] svc #0 = 0xD4000001
    buf[84] = 0x01; buf[85] = 0x00; buf[86] = 0x00; buf[87] = 0xD4;
    // [0x58] mov x8, #2 = MOVZ x8, #2 = 0xD2800048
    buf[88] = 0x48; buf[89] = 0x00; buf[90] = 0x80; buf[91] = 0xD2;
    // [0x5c] svc #0 = 0xD4000001
    buf[92] = 0x01; buf[93] = 0x00; buf[94] = 0x00; buf[95] = 0xD4;
    // [0x60] "B: recv2\n\r\0" (10 bytes + NUL) at offset 96
    buf[96] = b'B'; buf[97] = b':'; buf[98] = b' '; buf[99] = b'r';
    buf[100] = b'e'; buf[101] = b'c'; buf[102] = b'v'; buf[103] = b'2';
    buf[104] = b'\n'; buf[105] = b'\r'; buf[106] = 0x00;
    // [0x70] "B: got2\n\r\0" (9 bytes + NUL) at offset 112
    buf[112] = b'B'; buf[113] = b':'; buf[114] = b' '; buf[115] = b'g';
    buf[116] = b'o'; buf[117] = b't'; buf[118] = b'2'; buf[119] = b'\n';
    buf[120] = b'\r'; buf[121] = 0x00;
    buf
};

// ---------------------------------------------------------------------------
// Build the user address space
// ---------------------------------------------------------------------------

/// Build the user page tables and populate the user code page. Called
/// from the monitor's `el0` command (high world, MMU on) before dropping
/// to EL0.
///
/// Returns the physical address of the user L0 root — the value to
/// load into TTBR0_EL1.
pub fn build_user_space() -> u64 {
    build_user_space_with(&USER_PROGRAM_BYTES)
}

/// Build the user page tables with a *different* program — the M5
/// fault-recovery test. Same address space layout, same page tables,
/// but the code page holds `USER_FAULT_PROGRAM_BYTES` instead: a program
/// that deliberately stores to an unmapped address. Used by the
/// monitor's `el0fault` command.
pub fn build_fault_user_space() -> u64 {
    build_user_space_with(&USER_FAULT_PROGRAM_BYTES)
}

/// The shared builder: copy `prog` into the user code page, wire the
/// four-level page table tree, return the L0 root PA. Both the normal
/// `el0` program and the faulting `el0fault` program share this path —
/// the only difference is which bytes land in the code page.
fn build_user_space_with(prog: &[u8]) -> u64 {
    // SAFETY: single core, no concurrent access. USER_CODE_PAGE is
    // .bss (zeroed by boot); we write the program bytes into it.
    unsafe {
        let page = &raw mut USER_CODE_PAGE.0 as *mut u8;
        let mut i = 0;
        while i < prog.len() {
            page.add(i).write_volatile(prog[i]);
            i += 1;
        }
        // The rest of the page stays zeroed (already done by boot).
    }

    // ---- 2. Physical addresses of the user pages and tables ----
    // SAFETY: address-taking only (no reads/writes) on static muts.
    let code_pa = mmu::virt_to_phys(unsafe { &raw const USER_CODE_PAGE } as *const _ as usize) as u64;
    let stack_pa = mmu::virt_to_phys(unsafe { &raw const USER_STACK_PAGE } as *const _ as usize) as u64;
    let l0_pa = mmu::virt_to_phys(unsafe { &raw const USER_TABLES.l0 } as *const _ as usize) as u64;
    let l1_pa = mmu::virt_to_phys(unsafe { &raw const USER_TABLES.l1 } as *const _ as usize) as u64;
    let l2_pa = mmu::virt_to_phys(unsafe { &raw const USER_TABLES.l2 } as *const _ as usize) as u64;
    let l3_pa = mmu::virt_to_phys(unsafe { &raw const USER_TABLES.l3 } as *const _ as usize) as u64;

    // SAFETY: same invariant — we write the tables via their high
    // aliases; the walker is not looking at them.
    unsafe { wire_user_tables(l0_pa, l1_pa, l2_pa, l3_pa, code_pa, stack_pa) };

    l0_pa
}

// ---------------------------------------------------------------------------
// M6: per-task address space builder
// ---------------------------------------------------------------------------

/// Build a per-task user address space in the given `UserTaskMem` slot.
/// Copies `prog` into the task's code page, wires its four-level page
/// table tree (same geometry as the M5 single-task tables), and returns
/// the PA of its L0 root — the value for TTBR0_EL1 when this task runs.
///
/// Each task gets its own completely independent user address space:
/// its own L0/L1/L2/L3 tables, its own code page, its own stack page.
/// The kernel's TTBR1 tree is shared and never changes.
///
/// # Safety
/// `slot` must be 0 or 1 (selecting TASK_MEM_A or TASK_MEM_B).
/// Single-core, no concurrent access.
unsafe fn build_task_user_space(slot: usize, prog: &[u8]) -> u64 {
    let mem = unsafe { task_mem(slot) };

    // ---- 1. Copy the program into the task's code page ----
    // SAFETY: mem is a valid pointer to a UserTaskMem; single-core.
    unsafe {
        let page = core::ptr::addr_of_mut!((*mem).code.0) as *mut u8;
        let mut i = 0;
        while i < prog.len() {
            page.add(i).write_volatile(prog[i]);
            i += 1;
        }
    }

    // ---- 2. Physical addresses of the task's pages and tables ----
    // SAFETY: address-taking only (no reads/writes) on the UserTaskMem
    // fields, through a valid raw pointer. The `addr_of!` does not
    // dereference — it computes the field's address — but Rust 2024's
    // `unsafe_op_in_unsafe_fn` lint still requires an `unsafe` block
    // because `*mem` is a raw-pointer dereference in the expression.
    let code_pa = mmu::virt_to_phys(unsafe { core::ptr::addr_of!((*mem).code) } as usize) as u64;
    let stack_pa = mmu::virt_to_phys(unsafe { core::ptr::addr_of!((*mem).stack) } as usize) as u64;
    let l0_pa = mmu::virt_to_phys(unsafe { core::ptr::addr_of!((*mem).l0) } as usize) as u64;
    let l1_pa = mmu::virt_to_phys(unsafe { core::ptr::addr_of!((*mem).l1) } as usize) as u64;
    let l2_pa = mmu::virt_to_phys(unsafe { core::ptr::addr_of!((*mem).l2) } as usize) as u64;
    let l3_pa = mmu::virt_to_phys(unsafe { core::ptr::addr_of!((*mem).l3) } as usize) as u64;

    // SAFETY: same invariant as build_user_space_with — we write the
    // tables via their high aliases; the walker is not looking at them.
    unsafe { wire_user_tables(l0_pa, l1_pa, l2_pa, l3_pa, code_pa, stack_pa) };

    l0_pa
}

/// Wire a four-level user page table tree: L0→L1→L2→L3, with one code
/// page (UserRx) and one stack page (UserRw) at the standard user VAs.
/// Shared by the M5 single-task builder and the M6 per-task builder —
/// the geometry is identical, only the backing pages differ.
///
/// # Safety
/// The caller must provide valid PAs of 16 KiB-aligned tables and
/// pages. Single-core, MMU on but TTBR0 not pointing at these tables
/// yet (we write via their high aliases).
unsafe fn wire_user_tables(
    l0_pa: u64,
    l1_pa: u64,
    l2_pa: u64,
    l3_pa: u64,
    code_pa: u64,
    stack_pa: u64,
) {
    // Recover the high-half VAs from the PAs so we can write the tables.
    let l0 = mmu::phys_to_virt(l0_pa as usize) as *mut u64;
    let l1 = mmu::phys_to_virt(l1_pa as usize) as *mut u64;
    let l2 = mmu::phys_to_virt(l2_pa as usize) as *mut u64;
    let l3 = mmu::phys_to_virt(l3_pa as usize) as *mut u64;

    // SAFETY: we write through the high aliases of the table pages;
    // the hardware walker is not looking at these tables yet (TTBR0
    // still points at whatever was there before). Single-core.
    unsafe {
        // L0[0] -> L1 (all user VAs are in the low half, bit 47 = 0)
        l0.add(mmu::va_l0i(USER_CODE_VA))
            .write(mmu::table_desc(l1_pa));
        // L1[0] -> L2 (all our VAs are < 2^36)
        l1.add(mmu::va_l1i(USER_CODE_VA))
            .write(mmu::table_desc(l2_pa));
        // L2 -> L3 (one 32 MiB region, refined to 16 KiB pages)
        l2.add(mmu::va_l2i(USER_CODE_VA))
            .write(mmu::table_desc(l3_pa));

        // L3: two pages — code (user RX) and stack (user RW).
        l3.add(mmu::va_l3i(USER_CODE_VA))
            .write(mmu::page_desc(code_pa, mmu::Attr::UserRx));
        l3.add(mmu::va_l3i(USER_STACK_VA))
            .write(mmu::page_desc(stack_pa, mmu::Attr::UserRw));
    }
}
// ---------------------------------------------------------------------------

/// Drop to EL0 and run the normal user program. When the program calls
/// `exit` (syscall 2), the syscall handler returns to the monitor.
pub fn drop_to_el0() {
    drop_to_el0_with(build_user_space());
}

/// Drop to EL0 and run the *faulting* user program — the M5 fault-recovery
/// test. The program deliberately stores to an unmapped address, taking a
/// data abort from EL0. `handle_sync_from_el0` catches it, calls
/// [`kill_task_on_fault`], and the kernel returns to the monitor instead
/// of parking. Used by the monitor's `el0fault` command.
pub fn drop_to_el0_fault() {
    drop_to_el0_with(build_fault_user_space());
}

/// The shared drop: set TTBR0 to `root_pa`, configure SPSR/ELR/SP for EL0,
/// and `eret`. Both the normal and faulting drops share this path — the
/// only difference is which user page tables (and thus which program) are
/// installed.
fn drop_to_el0_with(root_pa: u64) {
    mmu::set_user_ttbr0(root_pa);

    // SPSR_EL1: the PSTATE for EL0.
    //   M[3:0] = 0b0000 = EL0
    //   bit [4] = 0 = AArch64 (NOT AArch32: bit 4 set means AArch32!)
    //   DAIF = 0b0000 at bits [9:6] (all unmasked — EL0 runs with
    //   interrupts enabled so the timer can preempt. The kernel's
    //   SVC/fault handlers run at EL1 with DAIF set by hardware on
    //   exception entry, so unmasking here is safe: the moment an
    //   IRQ fires, the CPU drops to EL1 and DAIF is auto-set.)
    //   All other bits 0.
    //
    // The DAIF field lives at bits [9:6], NOT [7:4]: D=bit9, A=bit8,
    // I=bit7, F=bit6. So 0b0000<<6 = 0x000. A common mistake is to put
    // the mask at [7:4] (0xF0) — that sets bit 4, which flips the target
    // into AArch32 mode and the exception comes back as "from AArch32".
    //
    // M6 changed this from 0x3C0 (DAIF=all masked) to 0x000 (DAIF=all
    // unmasked): preemptive scheduling requires the timer IRQ to fire
    // while EL0 is running, and DAIF.I=1 at EL0 masks it. The cooperative
    // scheduler doesn't care (it isn't armed), but the preemptive one
    // is dead on arrival without this.
    let spsr: u64 = 0x000; // DAIF=0000 at [9:6], M=0000 at [3:0], bit[4]=0

    println!(
        "[kernel] dropping to EL0 — user code at VA {USER_CODE_VA:#x}, SP {USER_STACK_TOP:#x}"
    );

    // SAFETY: the eret is the privilege-drop. Before it, we set up
    // SPSR_EL1, ELR_EL1, and SP_EL0 — the three things eret reads.
    // After eret, we are at EL0; the next instruction is fetched from
    // USER_CODE_VA through TTBR0. The user program will svc, which
    // traps back to EL1 vector slot 0x400 (lower EL, synchronous).
    // We use explicit registers (x0, x1, x2) to avoid any ambiguity
    // about which register holds which value — the compiler's register
    // allocator could otherwise put the SPSR value in a register that
    // shadows a user register, corrupting the user's initial state.
    unsafe {
        let spsr_v = spsr;
        let entry_v = USER_CODE_VA as u64;
        let sp_v = USER_STACK_TOP as u64;
        asm!(
            "msr  SPSR_EL1, {0}",
            "msr  ELR_EL1, {1}",
            "msr  SP_EL0, {2}",
            // Zero the user's GP registers so no kernel state leaks
            // into userspace and no leftover values confuse the first
            // instructions. The eret reads SPSR/ELR/SP, not GP regs.
            "mov  x0,  xzr",
            "mov  x1,  xzr",
            "mov  x2,  xzr",
            "mov  x3,  xzr",
            "mov  x4,  xzr",
            "mov  x5,  xzr",
            "mov  x6,  xzr",
            "mov  x7,  xzr",
            "mov  x8,  xzr",
            "mov  x9,  xzr",
            "mov  x10, xzr",
            "mov  x11, xzr",
            "mov  x12, xzr",
            "mov  x13, xzr",
            "mov  x14, xzr",
            "mov  x15, xzr",
            "mov  x16, xzr",
            "mov  x17, xzr",
            "mov  x18, xzr",
            "eret",
            in(reg) spsr_v,
            in(reg) entry_v,
            in(reg) sp_v,
            options(nostack),
        );
    }
}

// ---------------------------------------------------------------------------
// M6: scheduled multi-task drop
// ---------------------------------------------------------------------------

/// Drop to EL0 with the M6 scheduler active: spawn two tasks (A and B),
/// each with its own independent user address space, and eret into task A.
///
/// Task A runs `USER_PROGRAM_BYTES` (prints "hello, EL0!", yields, prints
/// "back from yield", exits). Task B runs `TASK_B_PROGRAM_BYTES` (prints
/// "task B!", yields, prints "B done!", exits). When A yields, the
/// scheduler switches to B; when B yields, it switches back to A. The
/// output interleaves, proving two tasks in two address spaces share one
/// CPU — the kernel is now an operating system in the plural sense.
///
/// The first eret into task A is the same `eret` M5 uses; the difference
/// is that CURRENT_TID is now 1 (task A), and the scheduler has task B
/// queued Ready. When A's `svc yield` traps, `save_and_switch` swaps
/// the frames and TTBR0, and the return `eret` resumes B instead.
///
/// Like `drop_to_el0`, this does not return to the caller's monitor loop
/// — control comes back via `on_el0_return` (when the last task exits)
/// or `kill_task_on_fault` (if a task faults).
pub fn drop_to_el0_scheduled() {
    // ---- 1. Build two independent user address spaces ----
    // SAFETY: single-core, MMU on, TTBR0 still the empty root. We build
    // the tables via their high aliases; the walker is not looking at
    // them yet. slot 0 = task A, slot 1 = task B.
    let ttbr0_a = unsafe { build_task_user_space(0, &USER_PROGRAM_BYTES) };
    let ttbr0_b = unsafe { build_task_user_space(1, &TASK_B_PROGRAM_BYTES) };

    // ---- 2. Spawn both tasks into the scheduler ----
    // Task A: TID 1, the "hello, EL0!" program.
    // Task B: TID 2, the "task B!" program.
    // SAFETY: single-core, interrupts masked (monitor context).
    let tid_a = unsafe {
        crate::sched::spawn(b"A", ttbr0_a, USER_CODE_VA as u64, USER_STACK_TOP as u64)
    };
    let tid_b = unsafe {
        crate::sched::spawn(b"B", ttbr0_b, USER_CODE_VA as u64, USER_STACK_TOP as u64)
    };

    match (tid_a, tid_b) {
        (Some(a), Some(b)) => {
            println!(
                "[kernel] M6 scheduler: spawned task A (TID {a}) and task B (TID {b})"
            );
        }
        _ => {
            println!("[kernel] scheduler: failed to spawn tasks (table full?)");
            return;
        }
    }

    // ---- 3. Drop into task A ----
    // The scheduler has A at the head of the ready queue. We dequeue it,
    // mark it Running, set CURRENT_TID, install its TTBR0, and eret.
    //
    // SAFETY: single-core, monitor context. The dequeue gives us TID 1
    // (task A was enqueued first).
    let first = unsafe { crate::sched::scheduler() }.dequeue();
    let first = match first {
        Some(tid) => tid,
        None => {
            println!("[kernel] scheduler: ready queue empty after spawn?!");
            return;
        }
    };

    // SAFETY: first is a valid TID from the scheduler.
    let t = match unsafe { crate::sched::task(first) } {
        Some(t) => t,
        None => {
            println!("[kernel] scheduler: task {first} vanished?!");
            return;
        }
    };
    t.state = crate::sched::TaskState::Running;
    let root_pa = t.ttbr0_pa;
    unsafe { crate::sched::set_current_tid(first) };

    println!(
        "[kernel] dropping to EL0 — task {:?} (TID {first}), user code at VA {USER_CODE_VA:#x}",
        t.name_str()
    );

    // The eret into task A is identical to M5's drop_to_el0_with: set
    // TTBR0, SPSR, ELR, SP, eret. The only difference is that
    // CURRENT_TID is now set, so the yield handler will call the
    // scheduler instead of no-op-ing.
    drop_to_el0_with(root_pa);
}

/// Drop to EL0 with the M6 preemptive scheduler active: spawn two
/// tasks that **spin forever** (no yield, no exit), arm the timer,
/// and enable preemption. The timer IRQ fires every second and calls
/// `save_and_switch` from the IRQ handler, preempting whichever task
/// is spinning and eret-ing into the other. Both tasks' characters
/// appear on the console — proof that the timer, not the user code,
/// drove the context switch.
///
/// This is the M6 preemptive milestone: the kernel takes the CPU away
/// from a running task without the task's cooperation. The cooperative
/// `spawn2` proved the context switch mechanism; this proves it works
/// when the *timer* pulls the trigger.
///
/// The tasks never exit (they spin), so the only way back to the
/// monitor is Ctrl-A X (quit QEMU). In a real OS, a kill syscall or
/// a timeout would tear down a spinning task — M6's IPC or a future
/// beat will add that.
pub fn drop_to_el0_preempt() {
    // ---- 1. Build two independent user address spaces ----
    // SAFETY: single-core, MMU on, TTBR0 still the empty root.
    let ttbr0_a = unsafe { build_task_user_space(0, &TASK_PREEMPT_A_BYTES) };
    let ttbr0_b = unsafe { build_task_user_space(1, &TASK_PREEMPT_B_BYTES) };

    // ---- 2. Spawn both tasks ----
    // SAFETY: single-core, interrupts masked (monitor context).
    let tid_a = unsafe {
        crate::sched::spawn(b"preA", ttbr0_a, USER_CODE_VA as u64, USER_STACK_TOP as u64)
    };
    let tid_b = unsafe {
        crate::sched::spawn(b"preB", ttbr0_b, USER_CODE_VA as u64, USER_STACK_TOP as u64)
    };

    match (tid_a, tid_b) {
        (Some(a), Some(b)) => {
            println!(
                "[kernel] M6 preempt: spawned task A (TID {a}) and task B (TID {b})"
            );
        }
        _ => {
            println!("[kernel] preempt: failed to spawn tasks (table full?)");
            return;
        }
    }

    // ---- 3. Arm the timer and enable preemption ----
    // The timer fires once per second; each tick, the IRQ handler
    // calls save_and_switch if preempt is enabled and a task is
    // running. We arm *before* dropping to EL0 so the first tick
    // can preempt task A as soon as it starts spinning.
    let freq = crate::arch::aarch64::timer::frequency();
    let period = freq; // 1 second
    crate::arch::aarch64::timer::TICK_PERIOD
        .store(period, core::sync::atomic::Ordering::Relaxed);
    crate::arch::aarch64::timer::arm(period);
    crate::sched::preempt_on();
    println!(
        "[kernel] preempt: timer armed (1 tick/sec), preemption enabled. Tasks will spin."
    );

    // ---- 4. Drop into task A ----
    // Same path as spawn2: dequeue the first task, mark it Running,
    // set CURRENT_TID, install its TTBR0, eret.
    let first = unsafe { crate::sched::scheduler() }.dequeue();
    let first = match first {
        Some(tid) => tid,
        None => {
            println!("[kernel] preempt: ready queue empty after spawn?!");
            return;
        }
    };

    // SAFETY: first is a valid TID from the scheduler.
    let t = match unsafe { crate::sched::task(first) } {
        Some(t) => t,
        None => {
            println!("[kernel] preempt: task {first} vanished?!");
            return;
        }
    };
    t.state = crate::sched::TaskState::Running;
    let root_pa = t.ttbr0_pa;
    unsafe { crate::sched::set_current_tid(first) };

    // Unmask IRQ at the CPU so the timer can fire. This must happen
    // before the eret — the timer is armed, preemption is on, but
    // the CPU won't take the IRQ until DAIF.I is clear.
    unsafe {
        core::arch::asm!(
            "msr DAIFClr, #2",
            options(nostack, preserves_flags),
        );
    }

    println!(
        "[kernel] dropping to EL0 — task {:?} (TID {first}), preemptive scheduling live",
        t.name_str()
    );

    drop_to_el0_with(root_pa);
}

/// Drop to EL0 with the M6 IPC demo: spawn a sender (task A) and a
/// receiver (task B), each with its own user address space, and eret
/// into task A. The sender writes "A: sending", sends "hello B!" to
/// task B via `SYS_SEND`, yields, then writes "A: sent" and exits.
/// The receiver writes "B: waiting", yields, calls `SYS_RECV` (which
/// copies A's message into B's stack-page buffer), writes "B: got
/// msg!" and exits.
///
/// The message never touches shared memory: it goes from A's user
/// buffer → kernel mailbox (in B's TCB) → B's user buffer. The
/// kernel is the intermediary — that's the whole point of message
/// passing as an IPC primitive.
///
/// Like `spawn2`, this does not return to the monitor loop — control
/// comes back via `on_el0_return` when the last task exits.
pub fn drop_to_el0_ipc() {
    // ---- 1. Build two independent user address spaces ----
    // SAFETY: single-core, MMU on, TTBR0 still the empty root.
    let ttbr0_a = unsafe { build_task_user_space(0, &TASK_IPC_SENDER_BYTES) };
    let ttbr0_b = unsafe { build_task_user_space(1, &TASK_IPC_RECEIVER_BYTES) };

    // ---- 2. Spawn both tasks ----
    // Task A (sender): TID 1. Task B (receiver): TID 2.
    // SAFETY: single-core, interrupts masked (monitor context).
    let tid_a =
        unsafe { crate::sched::spawn(b"ipcA", ttbr0_a, USER_CODE_VA as u64, USER_STACK_TOP as u64) };
    let tid_b =
        unsafe { crate::sched::spawn(b"ipcB", ttbr0_b, USER_CODE_VA as u64, USER_STACK_TOP as u64) };

    match (tid_a, tid_b) {
        (Some(a), Some(b)) => {
            println!(
                "[kernel] M6 IPC: spawned sender (TID {a}) and receiver (TID {b})"
            );
        }
        _ => {
            println!("[kernel] IPC: failed to spawn tasks (table full?)");
            return;
        }
    }

    // ---- 3. Drop into task A (the sender) ----
    let first = unsafe { crate::sched::scheduler() }.dequeue();
    let first = match first {
        Some(tid) => tid,
        None => {
            println!("[kernel] IPC: ready queue empty after spawn?!");
            return;
        }
    };

    // SAFETY: first is a valid TID from the scheduler.
    let t = match unsafe { crate::sched::task(first) } {
        Some(t) => t,
        None => {
            println!("[kernel] IPC: task {first} vanished?!");
            return;
        }
    };
    t.state = crate::sched::TaskState::Running;
    let root_pa = t.ttbr0_pa;
    unsafe { crate::sched::set_current_tid(first) };

    println!(
        "[kernel] dropping to EL0 — task {:?} (TID {first}), IPC demo",
        t.name_str()
    );

    drop_to_el0_with(root_pa);
}

/// Drop to EL0 with the M6 blocking-IPC demo: spawn a sender (task A)
/// and a receiver (task B), each with its own user address space, and
/// eret into task A. The sender writes "A: send", yields (so B runs and
/// calls recvblk — blocking because its mailbox is empty), then sends
/// "wake!" to B. The `ipc_send` path sees B is Blocked, wakes it (Ready,
/// enqueued), and the scheduler resumes B — which re-enters the recvblk
/// svc, finds "wake!" waiting, and continues to write "B: woke!" and
/// exit. Meanwhile A writes "A: done" and exits.
///
/// This is the first use of the `Blocked` task state: a task sleeps on
/// a condition (empty mailbox) and another task wakes it by satisfying
/// the condition (sending a message). The kernel is the intermediary —
/// the same pattern a real OS uses for blocking read(), wait(), and
/// futex. The "retry the syscall on wake" trick (rewind ELR by 4 before
/// blocking) is the classic re-entrant syscall pattern.
///
/// Like `ipc`, this does not return to the monitor loop — control comes
/// back via `on_el0_return` when the last task exits.
pub fn drop_to_el0_blkipc() {
    // ---- 1. Build two independent user address spaces ----
    // SAFETY: single-core, MMU on, TTBR0 still the empty root.
    let ttbr0_a = unsafe { build_task_user_space(0, &TASK_BLKIPC_SENDER_BYTES) };
    let ttbr0_b = unsafe { build_task_user_space(1, &TASK_BLKIPC_RECEIVER_BYTES) };

    // ---- 2. Spawn both tasks ----
    // Task A (sender): TID 1. Task B (receiver): TID 2.
    // SAFETY: single-core, interrupts masked (monitor context).
    let tid_a = unsafe {
        crate::sched::spawn(b"blkA", ttbr0_a, USER_CODE_VA as u64, USER_STACK_TOP as u64)
    };
    let tid_b = unsafe {
        crate::sched::spawn(b"blkB", ttbr0_b, USER_CODE_VA as u64, USER_STACK_TOP as u64)
    };

    match (tid_a, tid_b) {
        (Some(a), Some(b)) => {
            println!(
                "[kernel] M6 blocking IPC: spawned sender (TID {a}) and receiver (TID {b})"
            );
        }
        _ => {
            println!("[kernel] blocking IPC: failed to spawn tasks (table full?)");
            return;
        }
    }

    // ---- 3. Drop into task A (the sender) ----
    let first = unsafe { crate::sched::scheduler() }.dequeue();
    let first = match first {
        Some(tid) => tid,
        None => {
            println!("[kernel] blocking IPC: ready queue empty after spawn?!");
            return;
        }
    };

    // SAFETY: first is a valid TID from the scheduler.
    let t = match unsafe { crate::sched::task(first) } {
        Some(t) => t,
        None => {
            println!("[kernel] blocking IPC: task {first} vanished?!");
            return;
        }
    };
    t.state = crate::sched::TaskState::Running;
    let root_pa = t.ttbr0_pa;
    unsafe { crate::sched::set_current_tid(first) };

    println!(
        "[kernel] dropping to EL0 — task {:?} (TID {first}), blocking IPC demo",
        t.name_str()
    );

    drop_to_el0_with(root_pa);
}

/// Drop to EL0 with the M6 blocking-send demo: spawn a sender (task A)
/// and a receiver (task B), each with its own user address space, and
/// eret into task A. The sender sends "first" via `sendblk` (succeeds),
/// then immediately sends "second" via `sendblk` — but B's mailbox is
/// full, so A **blocks**. The scheduler switches to B. B calls `recv`,
/// drains the mailbox (getting "first"), which wakes the blocked sender
/// A. B yields, A retries its `sendblk` — the mailbox is now empty, so
/// "second" lands. A writes "A: sent2" and exits. B runs, `recv`s
/// "second", writes "B: got2" and exits.
///
/// This is the symmetric counterpart to `blkipc`: there the receiver
/// slept on "mailbox empty" and the sender woke it; here the sender
/// sleeps on "mailbox full" and the receiver wakes it. Together,
/// `sendblk` and `recvblk` give M6's IPC a complete blocking pair — a
/// task can wait in either direction without spinning or
/// yield-and-retry. The `sendblk` monitor command proves it.
pub fn drop_to_el0_sendblk() {
    // ---- 1. Build two independent user address spaces ----
    // SAFETY: single-core, MMU on, TTBR0 still the empty root.
    let ttbr0_a = unsafe { build_task_user_space(0, &TASK_SENDBLK_SENDER_BYTES) };
    let ttbr0_b = unsafe { build_task_user_space(1, &TASK_SENDBLK_RECEIVER_BYTES) };

    // ---- 2. Spawn both tasks ----
    // Task A (sender): TID 1. Task B (receiver): TID 2.
    // SAFETY: single-core, interrupts masked (monitor context).
    let tid_a = unsafe {
        crate::sched::spawn(b"sdkA", ttbr0_a, USER_CODE_VA as u64, USER_STACK_TOP as u64)
    };
    let tid_b = unsafe {
        crate::sched::spawn(b"sdkB", ttbr0_b, USER_CODE_VA as u64, USER_STACK_TOP as u64)
    };

    match (tid_a, tid_b) {
        (Some(a), Some(b)) => {
            println!(
                "[kernel] M6 blocking send: spawned sender (TID {a}) and receiver (TID {b})"
            );
        }
        _ => {
            println!("[kernel] blocking send: failed to spawn tasks (table full?)");
            return;
        }
    }

    // ---- 3. Drop into task A (the sender) ----
    let first = unsafe { crate::sched::scheduler() }.dequeue();
    let first = match first {
        Some(tid) => tid,
        None => {
            println!("[kernel] blocking send: ready queue empty after spawn?!");
            return;
        }
    };

    // SAFETY: first is a valid TID from the scheduler.
    let t = match unsafe { crate::sched::task(first) } {
        Some(t) => t,
        None => {
            println!("[kernel] blocking send: task {first} vanished?!");
            return;
        }
    };
    t.state = crate::sched::TaskState::Running;
    let root_pa = t.ttbr0_pa;
    unsafe { crate::sched::set_current_tid(first) };

    println!(
        "[kernel] dropping to EL0 — task {:?} (TID {first}), blocking send demo",
        t.name_str()
    );

    drop_to_el0_with(root_pa);
}

/// Called when the user program exits and we return to EL1/monitor.
/// The `__el0_return` trampoline in vectors.rs jumps here.
#[unsafe(no_mangle)]
extern "C" fn on_el0_return() -> ! {
    // Restore TTBR0 to the empty root (condemn the user space).
    mmu::condemn_low_half();
    println!("[kernel] returned from EL0 — user program exited cleanly.");
    println!("[kernel] monitor resumed.");
    print!("> ");
    // Re-enter the monitor loop. We cannot return to kmain's loop
    // (it never returns), so we run our own copy of the same loop.
    let uart = virt::console();
    let mut buf = [0u8; 64];
    let mut len = 0usize;
    loop {
        let b = uart.read_byte();
        match b {
            b'\r' | b'\n' => {
                println!();
                crate::run_command(&buf[..len]);
                len = 0;
                print!("> ");
            }
            0x08 | 0x7f => {
                if len > 0 {
                    len -= 1;
                    print!("\x08 \x08");
                }
            }
            0x20..=0x7e => {
                if len < buf.len() {
                    buf[len] = b;
                    len += 1;
                    uart.write_byte(b);
                }
            }
            other => print!("<{other:#04x}>"),
        }
    }
}

/// Kill the current user task on a fault and return to the monitor.
///
/// This is M5's fault *service*: the kernel's first real recovery from
/// an exception, not just a report. Before this, every EL0 fault (data
/// abort, instruction abort, alignment) called `park()` — killing the
/// whole kernel. Now the kernel condemns the user address space (restores
/// TTBR0 to the empty root) and re-enters the monitor, exactly as a clean
/// `exit` does. The fault is reported by the caller; this function just
/// performs the recovery.
///
/// M6 will replace this with a real task-kill that cleans up per-task
/// state (kernel stack, scheduler entry). For the single-task kernel the
/// distinction is invisible: there is one task, it faulted, it is gone.
pub fn kill_task_on_fault() -> ! {
    // Tear down the user address space: restore TTBR0 to the empty root
    // so no stale user translations linger. Same path as a clean exit.
    crate::sched::preempt_off();
    mmu::condemn_low_half();
    println!("[kernel] user task killed on fault — returning to monitor.");
    // Re-enter the monitor via the same return path as a clean exit.
    // We are at EL1 with a valid kernel stack, so we call on_el0_return
    // directly (just as the exit syscall handler does).
    on_el0_return();
}

// ---------------------------------------------------------------------------
// The syscall handler — called from the vector table's lower-EL slot
// ---------------------------------------------------------------------------

/// Handle an SVC from EL0. Called from `exception_dispatch` when a
/// synchronous exception arrives from the "lower EL, AArch64" slot
/// (vector offset 0x400) with EC = SVC64.
///
/// `frame` is the trap frame the stub built; its registers are the
/// user's register state at the moment of the `svc`. The handler:
/// - Reads x8 for the syscall number.
/// - Dispatches: write, exit.
/// - For write: reads the user's buffer (from the user VA in x1),
///   writes to the UART, sets x0 = bytes written.
/// - For exit: modifies SPSR/ELR so the eret returns to EL1 (the
///   trampoline), not EL0.
/// - Returns: the stub restores (possibly modified) registers and
///   erets. For write, eret goes back to EL0. For exit, eret goes
///   to the trampoline at EL1.
pub fn handle_svc_from_el0(frame: &mut super::vectors::TrapFrame) {
    let nr = frame.x[8];
    match nr {
        SYS_WRITE => {
            let fd = frame.x[0];
            let buf_va = frame.x[1] as usize;
            let len = frame.x[2] as usize;
            if fd != 1 {
                frame.x[0] = (-1i64) as u64; // -EBADF
                return;
            }
            if len > 256 {
                frame.x[0] = (-7i64) as u64; // -E2BIG
                return;
            }
            let mut bytes_written = 0u64;
            let uart = virt::console();
            for i in 0..len {
                // SAFETY: buf_va is a user VA mapped in the user tables.
                // We are at EL1, so we can read through TTBR0. If the VA
                // is unmapped, this faults — a data abort from the
                // handler. For M5's first beat we trust the user program
                // (it's our own code). M6 will add fault-around-syscall
                // handling.
                let byte = unsafe {
                    core::ptr::with_exposed_provenance::<u8>(buf_va + i).read_volatile()
                };
                if byte == b'\n' {
                    uart.write_byte(b'\r');
                }
                uart.write_byte(byte);
                bytes_written += 1;
            }
            frame.x[0] = bytes_written;
        }
        SYS_YIELD => {
            // yield(): the user voluntarily gives up the CPU. In M5 this
            // was a no-op (no other task to switch to). In M6, the
            // scheduler's save_and_switch saves the current task's
            // context, picks the next Ready task, and overwrites the
            // stack frame with the next task's saved state — so when
            // __vectors_restore runs, it erets to a *different* task.
            //
            // If there is no other task (single-task mode, or the last
            // task running), save_and_switch returns false and yield
            // falls back to the M5 no-op: eret straight back.
            let cur = crate::sched::current_tid();
            let switched = if cur != 0 {
                // SAFETY: we are in an exception handler (DAIF set),
                // single-core, frame is the on-stack TrapFrame.
                unsafe { crate::sched::save_and_switch(frame) }
            } else {
                false
            };
            if switched {
                // We are now running the *next* task. Its x0 will be
                // whatever it was when it last yielded — don't touch
                // it. The frame already holds the new task's state.
                // (The println below would interleave with the task's
                // output, so we stay silent on the switch path.)
            } else {
                println!("[kernel] syscall: yield() — no other task, returning to EL0");
                frame.x[0] = 0;
            }
        }
        SYS_SEND => {
            // send(dst_tid, buf, len): copy up to 32 bytes from the
            // user buffer into the receiver's mailbox. Non-blocking:
            // -EAGAIN if the mailbox is full, -ESRCH if no such task.
            let dst_tid = frame.x[0] as usize;
            let buf_va = frame.x[1] as usize;
            let len = frame.x[2] as usize;

            if len == 0 {
                frame.x[0] = 0;
                return;
            }
            let len = len.min(crate::sched::MSG_MAX);

            // Read the user buffer byte-by-byte through TTBR0. We are
            // at EL1; the user VA is valid in the user's address space.
            // SAFETY: buf_va is a user VA mapped in the user tables.
            let mut msg = [0u8; crate::sched::MSG_MAX];
            for i in 0..len {
                msg[i] = unsafe {
                    core::ptr::with_exposed_provenance::<u8>(buf_va + i).read_volatile()
                };
            }

            // SAFETY: exception context, single-core, DAIF set.
            match unsafe { crate::sched::ipc_send(dst_tid, &msg[..len]) } {
                Ok(()) => {
                    frame.x[0] = 0; // success
                }
                Err(e) => {
                    frame.x[0] = e as u64; // negative errno
                }
            }
        }
        SYS_RECV => {
            // recv(buf, buf_len): copy a message from the mailbox into
            // the user buffer. Returns bytes received in x0 (>=0) or
            // -EAGAIN if empty. The sender's TID is returned in x1.
            let buf_va = frame.x[0] as usize;
            let buf_len = frame.x[1] as usize;

            if buf_len == 0 {
                frame.x[0] = (-22i64) as u64; // -EINVAL
                return;
            }
            let buf_len = buf_len.min(crate::sched::MSG_MAX);

            let mut msg = [0u8; crate::sched::MSG_MAX];
            // SAFETY: exception context, single-core, DAIF set.
            match unsafe { crate::sched::ipc_recv(&mut msg[..buf_len]) } {
                Ok((n, from)) => {
                    // Write the message to the user buffer through TTBR0.
                    for i in 0..n {
                        unsafe {
                            core::ptr::with_exposed_provenance_mut::<u8>(buf_va + i)
                                .write_volatile(msg[i]);
                        }
                    }
                    frame.x[0] = n as u64; // bytes received
                    frame.x[1] = from as u64; // sender TID
                }
                Err(e) => {
                    frame.x[0] = e as u64; // negative errno
                }
            }
        }
        SYS_RECVBLK => {
            // recvblk(buf, buf_len): blocking receive. Like recv, but if
            // the mailbox is empty, the task is put to sleep (Blocked)
            // instead of returning -EAGAIN. When a send arrives, the
            // sender's ipc_send wakes this task (Ready, enqueued); the
            // scheduler resumes it and it re-enters the syscall — which
            // this time finds the message waiting and returns it.
            //
            // The "re-enter the syscall" trick: we rewind ELR by 4 (one
            // instruction — the svc) before blocking, so when the task
            // is woken and erets, it re-executes the svc and comes back
            // here. The second time through, the mailbox is non-empty
            // (the sender filled it before waking us), so we take the
            // Ok path and return the message. This is the classic
            // "retry the syscall on wake" pattern — the same shape a
            // real OS uses for blocking read()/wait()/futex.
            let buf_va = frame.x[0] as usize;
            let buf_len = frame.x[1] as usize;

            if buf_len == 0 {
                frame.x[0] = (-22i64) as u64; // -EINVAL
                return;
            }
            let buf_len = buf_len.min(crate::sched::MSG_MAX);

            let mut msg = [0u8; crate::sched::MSG_MAX];
            // SAFETY: exception context, single-core, DAIF set.
            match unsafe { crate::sched::ipc_recv(&mut msg[..buf_len]) } {
                Ok((n, from)) => {
                    // Mailbox had a message — copy it out and return.
                    for i in 0..n {
                        unsafe {
                            core::ptr::with_exposed_provenance_mut::<u8>(buf_va + i)
                                .write_volatile(msg[i]);
                        }
                    }
                    frame.x[0] = n as u64; // bytes received
                    frame.x[1] = from as u64; // sender TID
                }
                Err(_eagain) => {
                    // Mailbox empty — block. Rewind ELR by 4 so the
                    // svc re-executes when we're woken, then call
                    // block_and_switch. If there's no other task to
                    // run (nobody to send to us), we have a deadlock:
                    // fall back to returning -EAGAIN rather than hang
                    // — the user program can yield and retry, or exit.
                    frame.elr = frame.elr.wrapping_sub(4);
                    // SAFETY: exception context, single-core, DAIF set.
                    let switched = unsafe { crate::sched::block_and_switch(frame) };
                    if !switched {
                        // Nobody else to run — can't block. Restore
                        // ELR (we're not actually retrying) and return
                        // -EAGAIN. This is the single-task case: the
                        // blocking recv degrades to non-blocking when
                        // there's no other task to produce a message.
                        frame.elr = frame.elr.wrapping_add(4);
                        frame.x[0] = (-11i64) as u64; // -EAGAIN
                    }
                    // If switched: we're now running the next task.
                    // When this task is woken and rescheduled, it
                    // re-enters the svc (ELR was rewound), comes back
                    // here, and finds the message waiting. Don't touch
                    // the frame — it holds the next task's context.
                }
            }
        }
        SYS_SENDBLK => {
            // sendblk(dst_tid, buf, len): blocking send. Like SYS_SEND
            // but if the receiver's mailbox is full, the sender is put
            // to sleep (Blocked) instead of returning -EAGAIN. When the
            // receiver drains its mailbox (via recv or recvblk), the
            // kernel wakes the blocked sender (wake_blocked_senders)
            // and it retries — its message now fits.
            //
            // The "retry the syscall on wake" trick is the same as
            // recvblk: we rewind ELR by 4 before blocking so the svc
            // re-executes on wake, and the second time through the
            // mailbox is empty (the receiver drained it) so the send
            // succeeds. This is the mirror image of recvblk: there the
            // receiver sleeps on "empty," here the sender sleeps on
            // "full." Together they form a complete blocking IPC pair.
            let dst_tid = frame.x[0] as usize;
            let buf_va = frame.x[1] as usize;
            let len = frame.x[2] as usize;

            if len == 0 {
                frame.x[0] = 0;
                return;
            }
            let len = len.min(crate::sched::MSG_MAX);

            // Read the user buffer into a kernel-local copy. We must
            // read it *before* blocking, because the user VA is in the
            // *sender's* address space — once we switch to the receiver
            // (and possibly other tasks), TTBR0 changes and the VA is
            // no longer valid. The kernel keeps the message in the
            // sender's TCB (on the stack here, then in blocked_send_dst's
            // "pending" if we block) until it lands in the mailbox.
            // We can't stash it in the TCB yet (no field for the pending
            // payload), so we re-read from the user buffer on retry —
            // but on retry we're back in the sender's address space, so
            // the VA is valid again. The read below is always from the
            // sender's own space.
            let mut msg = [0u8; crate::sched::MSG_MAX];
            for i in 0..len {
                msg[i] =
                    unsafe { core::ptr::with_exposed_provenance::<u8>(buf_va + i).read_volatile() };
            }

            // Try the non-blocking send first.
            // SAFETY: exception context, single-core, DAIF set.
            match unsafe { crate::sched::ipc_send(dst_tid, &msg[..len]) } {
                Ok(()) => {
                    frame.x[0] = 0; // success
                }
                Err(e) if e == -11 => {
                    // -EAGAIN: mailbox full. Block the sender and
                    // switch to the next task. Record which receiver
                    // we're blocked on so wake_blocked_senders can
                    // find us when it drains.
                    let cur = crate::sched::current_tid();
                    if cur == 0 {
                        // Kernel can't block — return the error.
                        frame.x[0] = e as u64;
                        return;
                    }
                    // Record the blocked-send destination and mark
                    // the task as Blocked-on-send. The recv handler
                    // scans for this.
                    // SAFETY: exception context, single-core.
                    if let Some(t) = unsafe { crate::sched::task(cur) } {
                        t.blocked_send_dst = dst_tid;
                    }

                    // Rewind ELR by 4 so the svc re-executes on wake.
                    frame.elr = frame.elr.wrapping_sub(4);

                    // SAFETY: exception context, single-core, DAIF set.
                    let switched = unsafe { crate::sched::block_and_switch(frame) };
                    if !switched {
                        // Nobody else to run — can't block. Restore
                        // ELR, clear the flag, return -EAGAIN.
                        frame.elr = frame.elr.wrapping_add(4);
                        if let Some(t) = unsafe { crate::sched::task(cur) } {
                            t.blocked_send_dst = 0;
                        }
                        frame.x[0] = (-11i64) as u64; // -EAGAIN
                    }
                    // If switched: we're running the next task now.
                    // When this task is woken (the receiver drained its
                    // mailbox), it re-enters the svc, re-reads the user
                    // buffer (from its own address space, now valid
                    // again), and retries the send — which succeeds
                    // because the mailbox is now empty.
                }
                Err(e) => {
                    // -ESRCH: no such task. Return the error.
                    frame.x[0] = e as u64;
                }
            }
        }
        SYS_EXIT => {
            // The user wants to exit. Mark the current task as Exited
            // (M6: the scheduler needs to know this slot is free), then
            // return to the monitor — or, if there is another task
            // Ready, switch to it instead of returning to the monitor.
            let cur = crate::sched::current_tid();
            println!("[kernel] syscall: exit() (TID {cur})");
            if cur != 0 {
                // SAFETY: single-core, exception context.
                if let Some(t) = unsafe { crate::sched::task(cur) } {
                    t.state = crate::sched::TaskState::Exited;
                }
            }
            // Is there another task to run? If so, switch to it.
            // SAFETY: same exception-context invariant.
            let switched = if cur != 0 {
                unsafe { crate::sched::save_and_switch(frame) }
            } else {
                false
            };
            if !switched {
                // No other task — return to the monitor.
                // Disable preemption: the timer may still be armed,
                // but with no tasks running the IRQ handler would
                // otherwise call save_and_switch with CURRENT_TID=0
                // (the no-op path, but we make it explicit).
                crate::sched::preempt_off();
                mmu::condemn_low_half();
                on_el0_return();
            }
            // else: we switched to the next task; its frame is loaded
            // and __vectors_restore will eret to it. The monitor is not
            // re-entered — the CPU stays in EL0, running tasks.
        }
        _ => {
            println!("[kernel] unknown syscall {nr}");
            frame.x[0] = (-38i64) as u64; // -ENOSYS
        }
    }
}