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
    // ---- 1. Copy the program into the user code page ----
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
    let code_pa = mmu::virt_to_phys(unsafe { &raw const USER_CODE_PAGE }.expose_provenance()) as u64;
    let stack_pa = mmu::virt_to_phys(unsafe { &raw const USER_STACK_PAGE }.expose_provenance()) as u64;
    let l0_pa = mmu::virt_to_phys(unsafe { &raw const USER_TABLES.l0 }.expose_provenance()) as u64;
    let l1_pa = mmu::virt_to_phys(unsafe { &raw const USER_TABLES.l1 }.expose_provenance()) as u64;
    let l2_pa = mmu::virt_to_phys(unsafe { &raw const USER_TABLES.l2 }.expose_provenance()) as u64;
    let l3_pa = mmu::virt_to_phys(unsafe { &raw const USER_TABLES.l3 }.expose_provenance()) as u64;

    // ---- 3. Wire the tree: L0 -> L1 -> L2 -> L3 ----
    // SAFETY: single core, MMU on but TTBR0 is still the empty root
    // (condemned in kmain). We are writing to USER_TABLES via their
    // high aliases; the hardware walker is not looking at these tables
    // yet (TTBR0 points at the kernel's empty_root). No concurrent
    // reader exists.
    unsafe {
        let l0 = &raw mut USER_TABLES.l0.0 as *mut u64;
        let l1 = &raw mut USER_TABLES.l1.0 as *mut u64;
        let l2 = &raw mut USER_TABLES.l2.0 as *mut u64;
        let l3 = &raw mut USER_TABLES.l3.0 as *mut u64;

        // L0[0] -> L1 (all user VAs are in the low half, bit 47 = 0)
        l0.add(mmu::va_l0i(USER_CODE_VA)).write(mmu::table_desc(l1_pa));
        // L1[0] -> L2 (all our VAs are < 2^36)
        l1.add(mmu::va_l1i(USER_CODE_VA)).write(mmu::table_desc(l2_pa));
        // L2 -> L3 (one 32 MiB region, refined to 16 KiB pages)
        l2.add(mmu::va_l2i(USER_CODE_VA)).write(mmu::table_desc(l3_pa));

        // L3: two pages — code (user RX) and stack (user RW).
        l3.add(mmu::va_l3i(USER_CODE_VA))
            .write(mmu::page_desc(code_pa, mmu::Attr::UserRx));
        l3.add(mmu::va_l3i(USER_STACK_VA))
            .write(mmu::page_desc(stack_pa, mmu::Attr::UserRw));
    }

    l0_pa
}

// ---------------------------------------------------------------------------
// The EL0 drop
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
    //   DAIF = 0b1111 at bits [9:6] (all masked — EL0 cannot take async
    //   exceptions directly; they trap to EL1 first)
    //   All other bits 0.
    //
    // The DAIF field lives at bits [9:6], NOT [7:4]: D=bit9, A=bit8,
    // I=bit7, F=bit6. So 0b1111<<6 = 0x3C0. A common mistake is to put
    // the mask at [7:4] (0xF0) — that sets bit 4, which flips the target
    // into AArch32 mode and the exception comes back as "from AArch32".
    let spsr: u64 = 0x3C0; // DAIF=1111 at [9:6], M=0000 at [3:0], bit[4]=0

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
            // yield(): the user voluntarily gives up the CPU. In the
            // single-task kernel (M5) there is no other task to switch
            // to, so yield is a no-op that simply returns — but the
            // *mechanism* is real: the svc trapped to EL1, we are here,
            // and eret will resume EL0 at the instruction after the svc.
            // This is the first syscall that returns to the user, and
            // the user program verifies it by printing a second
            // message after the yield returns.
            println!("[kernel] syscall: yield() — returning to EL0");
            // x0 = 0 means success. The stub restores registers and
            // erets back to EL0 at ELR (which points past the svc).
            frame.x[0] = 0;
        }
        SYS_EXIT => {
            // The user wants to exit. Instead of modifying the frame and
            // using a trampoline, we call on_el0_return() directly —
            // we're already at EL1 with a valid kernel stack.
            println!("[kernel] syscall: exit()");
            // Restore TTBR0 to the empty root before returning to the
            // monitor, so the user address space is gone.
            mmu::condemn_low_half();
            // on_el0_return never returns — it runs the monitor loop.
            on_el0_return();
        }
        _ => {
            println!("[kernel] unknown syscall {nr}");
            frame.x[0] = (-38i64) as u64; // -ENOSYS
        }
    }
}