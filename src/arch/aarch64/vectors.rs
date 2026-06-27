//! The EL1 exception vector table: how the kernel catches its own faults.
//!
//! Milestone 0's worst property was silence. `VBAR_EL1` — the register
//! that tells the CPU where exception handlers live — is architecturally
//! UNKNOWN at reset, and we never wrote it. Any fault sent the core
//! through a wild pointer and the machine simply stopped, taking the
//! evidence with it. This file replaces that with the classic kernel
//! bargain: every exception lands in code that saves the interrupted
//! world, prints "what faulted, where, and why", and then either repairs
//! the situation and resumes, or says so honestly and parks.
//!
//! ## The table the architecture demands
//!
//! AArch64 vectors every exception to `VBAR_EL1 + a fixed offset`. The
//! table has **sixteen entries of 0x80 bytes each**, and the entries are
//! *instructions*, not addresses: the CPU jumps straight into the slot,
//! giving each handler 32 instructions of runway before it must branch
//! out. Sixteen because four kinds times four origins (Arm 102412,
//! "AArch64 Exception Model", table 5-4):
//!
//! | kind        | current EL, SP_EL0 | current EL, SP_ELx | lower EL, AArch64 | lower EL, AArch32 |
//! |-------------|--------------------|--------------------|-------------------|-------------------|
//! | Synchronous | 0x000              | 0x200              | 0x400             | 0x600             |
//! | IRQ         | 0x080              | 0x280              | 0x480             | 0x680             |
//! | FIQ         | 0x100              | 0x300              | 0x500             | 0x700             |
//! | SError      | 0x180              | 0x380              | 0x580             | 0x780             |
//!
//! Which column is *ours*? The kernel has run on SP_EL1 since boot
//! (SPSel=1 is the reset state, and the boot stub's `mov sp, x1` set the
//! EL1 stack), so the kernel's own faults arrive in the **SP_ELx column,
//! offset 0x200** — a classic "my handler never runs" trap is wiring up
//! 0x000 instead. The SP_EL0 column would mean we somehow switched stack
//! selection (we never do), and the lower-EL columns stay empty until M5
//! gives the machine a userspace. We fill all sixteen anyway: an
//! "impossible" exception with a report beats an impossible exception
//! with a hang — that asymmetry is this whole milestone.
//!
//! ## What the hardware does before our first instruction
//!
//! On any exception taken to EL1, the CPU atomically: stashes the return
//! address in `ELR_EL1` and the old PSTATE in `SPSR_EL1`; records the
//! cause in `ESR_EL1` (and the faulting address, when there is one, in
//! `FAR_EL1`); masks all asynchronous exceptions (PSTATE.DAIF = all set);
//! selects SP_EL1; and jumps into the table. Note what it does *not* do:
//! save a single general-purpose register. That is the handler's job, and
//! the reason for the assembly below.
//!
//! One sharp edge shapes everything here: ELR/SPSR/ESR/FAR are *single
//! registers*, not a stack. The next exception overwrites all four. So
//! the stub copies them into the frame immediately, and the dispatcher
//! refuses to nest (see `exception_dispatch`): a second fault while
//! reporting a first has already destroyed the first's evidence, and we
//! say so rather than loop silently.
//!
//! That tripwire has one blind spot, named honestly: it lives in Rust,
//! on the far side of the stub's stores — all of which go *through SP*.
//! If the fault being handled is SP itself being unusable (misaligned,
//! or aimed at unwritable memory), the stub's first `stp` re-faults
//! before any evidence is saved, re-enters this same slot, and loops
//! with no output. The cure is a stack the handler owns — kernel
//! threads on SP_EL0, SP_EL1 reserved as a known-good exception stack
//! (the SPSel split, M5) — not anything M1 can buy. See the notes at
//! step 4 of the stub and at the `EC_SP_ALIGN` arm below.

use core::fmt;

// ---------------------------------------------------------------------------
// The vector table and the save/restore assembly
// ---------------------------------------------------------------------------

core::arch::global_asm!(
    r#"
// The table lives in its own section so linker.ld can place it explicitly
// (after .text.boot — _start must stay the first byte of the image) and
// ASSERT on its alignment. "ax" = allocatable + executable.
.section .text.vectors, "ax"

// ---- One exception entry, as a macro ---------------------------------------
//
// Expanded sixteen times below, once per slot. Budget: 32 instructions
// per slot; this is 29. The .org directives after each expansion are the
// enforcement — see the table below.
//
// The #offsets here and `struct TrapFrame`'s field offsets are the same
// numbers by construction, and Rust const-asserts them so the two views
// cannot drift apart silently.
//
// kind:   0 = synchronous, 1 = IRQ, 2 = FIQ, 3 = SError
// source: 0 = current EL on SP_EL0, 1 = current EL on SP_ELx,
//         2 = lower EL (AArch64),   3 = lower EL (AArch32)
.macro VECTOR_ENTRY kind, source
    // ---- 1. Make room for one TrapFrame ---------------------------------
    // 288 bytes = 18 * 16, so SP stays 16-byte aligned: AAPCS64 demands
    // it at every call, and the hardware can be told to fault otherwise.
    sub   sp,  sp,  #288

    // ---- 2. Save x0..x29, in pairs ---------------------------------------
    // Why all of them, when the C ABI lets a callee clobber only x0..x18?
    // Because the report must show the interrupted code's registers — all
    // of them, untouched — and because a frame the whole kernel can trust
    // is what M6's context switch will be built on. No FP/SIMD registers:
    // we compile softfloat (see .cargo/config.toml), so kernel code never
    // touches them and there is nothing of ours to save.
    stp   x0,  x1,  [sp, #0]
    stp   x2,  x3,  [sp, #16]
    stp   x4,  x5,  [sp, #32]
    stp   x6,  x7,  [sp, #48]
    stp   x8,  x9,  [sp, #64]
    stp   x10, x11, [sp, #80]
    stp   x12, x13, [sp, #96]
    stp   x14, x15, [sp, #112]
    stp   x16, x17, [sp, #128]
    stp   x18, x19, [sp, #144]
    stp   x20, x21, [sp, #160]
    stp   x22, x23, [sp, #176]
    stp   x24, x25, [sp, #192]
    stp   x26, x27, [sp, #208]
    stp   x28, x29, [sp, #224]

    // ---- 3. Save x30 and the interrupted SP -------------------------------
    // "Old SP = ours before step 1" is exactly true only in the SP_ELx
    // column, where the interrupted code was already on this stack. The
    // other twelve slots arrive from some *other* stack (SP_EL0, or a
    // lower EL's), so for them this records the handler stack instead —
    // report() marks the sp line accordingly rather than let it lie.
    add   x0,  sp,  #288
    stp   x30, x0,  [sp, #240]

    // ---- 4. Capture the four per-exception system registers --------------
    // Immediately, before anything *else* can fault: they are single
    // registers, and a nested exception would overwrite all four. "Else"
    // is doing honest work in that sentence — steps 1-3 already stored
    // through SP. If SP itself was the fault (misaligned, or pointing at
    // unwritable memory), we never got here: the first stp re-faulted,
    // re-entered this same slot, and the machine is looping silently
    // right now, beyond the reach of the Rust-side tripwire. M1 accepts
    // that hole; M5's SPSel split (a dedicated, known-good SP_EL1
    // exception stack) is the real fix.
    mrs   x0,  ELR_EL1            // where eret would resume
    mrs   x1,  SPSR_EL1           // the interrupted PSTATE
    stp   x0,  x1,  [sp, #256]
    mrs   x0,  ESR_EL1            // why: the syndrome
    mrs   x1,  FAR_EL1            // where: the faulting address (when valid)
    stp   x0,  x1,  [sp, #272]

    // ---- 5. Into Rust ------------------------------------------------------
    // exception_dispatch(&mut frame, kind, source)
    mov   x0,  sp                 // arg 0: the frame we just built
    mov   x1,  #\kind             // arg 1: which of the four kinds
    mov   x2,  #\source           // arg 2: which of the four origins
    bl    exception_dispatch

    // ---- 6. If the dispatcher returns, the fault was repaired -------------
    // Restore the world and resume. (Shared tail: slots are size-limited,
    // and sixteen copies of the epilogue would teach nothing.)
    b     __vectors_restore
.endm

// ---- The table itself --------------------------------------------------------
//
// .balign 2048: VBAR_EL1's low 11 bits are RES0, so the table must sit on
// a 2 KiB boundary (linker.ld ASSERTs that the linker really delivered).
// The .org directives pin each entry to its architectural offset — and
// because .org can only move the location counter *forward*, an entry
// that outgrows its 0x80-byte slot fails the build loudly instead of
// silently shifting its fifteen neighbors.

.balign 2048
.global __vectors
__vectors:

// -- from the current EL, on SP_EL0: we never select SP_EL0, so any of
//    these firing is itself a bug — which is exactly worth a report ------------
.org 0x000
    VECTOR_ENTRY 0, 0             // synchronous
.org 0x080
    VECTOR_ENTRY 1, 0             // IRQ
.org 0x100
    VECTOR_ENTRY 2, 0             // FIQ
.org 0x180
    VECTOR_ENTRY 3, 0             // SError

// -- from the current EL, on SP_ELx: the live column — our own faults ---------
.org 0x200
    VECTOR_ENTRY 0, 1             // synchronous: brk, svc, aborts (M1's stars)
.org 0x280
    VECTOR_ENTRY 1, 1             // IRQ: nothing until M3 unmasks the timer
.org 0x300
    VECTOR_ENTRY 2, 1             // FIQ: silent on QEMU; Apple's timer (M7)
.org 0x380
    VECTOR_ENTRY 3, 1             // SError

// -- from a lower EL, AArch64: EL0 — empty until M5 builds a userspace --------
.org 0x400
    VECTOR_ENTRY 0, 2
.org 0x480
    VECTOR_ENTRY 1, 2
.org 0x500
    VECTOR_ENTRY 2, 2
.org 0x580
    VECTOR_ENTRY 3, 2

// -- from a lower EL, AArch32: Opal will never run 32-bit code, but the
//    architecture reserves the slots, so they get reporters too ---------------
.org 0x600
    VECTOR_ENTRY 0, 3
.org 0x680
    VECTOR_ENTRY 1, 3
.org 0x700
    VECTOR_ENTRY 2, 3
.org 0x780
    VECTOR_ENTRY 3, 3

// One final fence: if the sixteenth entry overflowed its slot, this errors.
.org 0x800

// ---- The shared return path ---------------------------------------------------
//
// Mirror image of the save sequence. ELR/SPSR go back into their system
// registers first — the dispatcher may have *changed* elr; that is how a
// brk is skipped — and x0/x1 are reloaded last because they are the
// temporaries here. esr/far are not "restored": they are evidence, not
// state. And sp needs no load — adding the frame size back recreates it.

__vectors_restore:
    ldp   x0,  x1,  [sp, #256]    // saved ELR, SPSR (possibly edited by Rust)
    msr   ELR_EL1, x0
    msr   SPSR_EL1, x1
    ldp   x2,  x3,  [sp, #16]
    ldp   x4,  x5,  [sp, #32]
    ldp   x6,  x7,  [sp, #48]
    ldp   x8,  x9,  [sp, #64]
    ldp   x10, x11, [sp, #80]
    ldp   x12, x13, [sp, #96]
    ldp   x14, x15, [sp, #112]
    ldp   x16, x17, [sp, #128]
    ldp   x18, x19, [sp, #144]
    ldp   x20, x21, [sp, #160]
    ldp   x22, x23, [sp, #176]
    ldp   x24, x25, [sp, #192]
    ldp   x26, x27, [sp, #208]
    ldp   x28, x29, [sp, #224]
    ldr   x30, [sp, #240]
    // x0 and x1 are dead (used as ELR/SPSR temps above). x2..x30 are
    // now restored. We restore x0/x1 LAST — they're the only scratch
    // registers available for the per-task kernel-stack switch below.
    //
    // M6 per-task kernel stacks: the context-switch functions (in Rust)
    // set PENDING_KSTACK_SP to the next task's kernel-stack top when
    // they switch. This assembly epilogue reads that global *after*
    // all Rust call frames are popped (so switching SP can't corrupt
    // them) and *before* eret (so the next exception lands on the
    // right stack). The global is in .bss (TTBR1 space), accessible
    // regardless of which SP is active.
    //
    // The challenge: after restoring x2..x30, only x0 and x1 are free.
    // We need them to: (1) save user's x0/x1 to SAVED_GP (a kernel
    // static, accessible after SP switch), (2) pop the frame, (3)
    // read and check PENDING_KSTACK_SP, (4) switch SP if needed, (5)
    // restore user's x0/x1 from SAVED_GP. Only x0/x1 are clobbered.
    ldr   x0, [sp, #0]            // x0 = user's x0 (from frame slot 0)
    ldr   x1, [sp, #8]            // x1 = user's x1 (from frame slot 8)
    adrp  x2, SAVED_GP            // x2 = page of SAVED_GP — x2 is free
    add   x2, x2, :lo12:SAVED_GP // x2 = &SAVED_GP[0]
    str   x0, [x2]                // SAVED_GP[0] = user's x0
    str   x1, [x2, #8]            // SAVED_GP[8] = user's x1
    // x2 is now dead — it was restored from the frame at [sp, #16].
    // But we just clobbered it. We need to re-restore x2 from the frame
    // *before* popping SP. The frame is still at [sp], pre-pop.
    ldr   x2, [sp, #16]           // re-restore x2 from frame slot
    // Now pop the frame and check for a pending kernel-stack switch.
    add   sp,  sp,  #288          // pop the frame; SP is exactly pre-fault
    adrp  x0, PENDING_KSTACK_SP
    add   x0, x0, :lo12:PENDING_KSTACK_SP
    ldr   x0, [x0]                // x0 = pending kernel-stack SP
    cbz   x0, .L_no_kstack_switch // 0 = no switch pending
    // Switch SP to the new task's kernel stack and clear the flag.
    mov   sp, x0
    adrp  x1, PENDING_KSTACK_SP
    add   x1, x1, :lo12:PENDING_KSTACK_SP
    str   xzr, [x1]               // clear: no switch next time
.L_no_kstack_switch:
    // Restore user's x0 and x1 from SAVED_GP (x0 address is consumed).
    adrp  x0, SAVED_GP
    add   x0, x0, :lo12:SAVED_GP
    ldr   x1, [x0, #8]           // x1 = user's x1
    ldr   x0, [x0]                // x0 = user's x0 (address consumed)
    eret                          // PSTATE := SPSR_EL1, PC := ELR_EL1 — atomically

// ---- M5: the EL0 return trampoline --------------------------------------
//
// When the exit() syscall handler wants to return to EL1 (instead of
// EL0), it sets frame.spsr = EL1h mode and frame.elr = the address of
// __el0_return below. The restore stub then erets here — at EL1,
// on the kernel's stack — and this trampoline calls on_el0_return()
// in Rust, which restores the monitor.

.section .text.vectors, "ax"
.global __el0_return
__el0_return:
    // We arrive here at EL1h (SP_ELx), on the exception stack. The
    // registers were restored by __vectors_restore before the eret,
    // so x0..x30 are the user's values. We don't care about user
    // registers here — we're returning to the monitor. SP_EL1 is
    // still valid, so we can call Rust normally.
    bl    on_el0_return
    // on_el0_return is -> !, so we never reach here.
1:  wfe
    b     1b
"#
);

// ---------------------------------------------------------------------------
// The trap frame
// ---------------------------------------------------------------------------

/// Everything the CPU was at the moment of the exception.
///
/// Built by the assembly above (which knows these offsets as `#N`
/// literals), consumed by the dispatcher as an ordinary Rust struct. The
/// two views must agree byte for byte: `#[repr(C)]` pins the layout, the
/// const asserts below pin the numbers.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TrapFrame {
    /// General-purpose registers x0..x30, exactly as the interrupted code
    /// left them. `x[30]` is the link register.
    pub x: [u64; 31],
    /// The interrupted stack pointer — exact for the SP_ELx slots; for
    /// the other twelve sources the stub's arithmetic yields the handler
    /// stack instead (see step 3 in the stub; report() flags it).
    /// Information, not control: the return path reconstructs SP by
    /// arithmetic and ignores this field.
    pub sp: u64,
    /// ELR_EL1 — where `eret` will resume. Mutating this is *the*
    /// recovery mechanism: it is how the brk handler steps past the
    /// breakpoint.
    pub elr: u64,
    /// SPSR_EL1 — the interrupted PSTATE, restored wholesale by `eret`.
    pub spsr: u64,
    /// ESR_EL1 — the syndrome: what happened. Read-only evidence.
    pub esr: u64,
    /// FAR_EL1 — the faulting address; meaningful only for some classes
    /// (aborts, alignment). Read-only evidence.
    pub far: u64,
}

// The contract between the assembly and the struct, enforced at compile
// time. If a field ever moves, the kernel does not build — much better
// than a register dump that lies.
const _: () = assert!(core::mem::size_of::<TrapFrame>() == 288);
const _: () = assert!(core::mem::offset_of!(TrapFrame, x) == 0);
const _: () = assert!(core::mem::offset_of!(TrapFrame, sp) == 248);
const _: () = assert!(core::mem::offset_of!(TrapFrame, elr) == 256);
const _: () = assert!(core::mem::offset_of!(TrapFrame, spsr) == 264);
const _: () = assert!(core::mem::offset_of!(TrapFrame, esr) == 272);
const _: () = assert!(core::mem::offset_of!(TrapFrame, far) == 280);

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// Point VBAR_EL1 at the table. Called first thing in `kmain` — before
/// the banner, so even banner-era bugs get reports. Not the first writer
/// anymore, though: the M2 boot stub pre-arms VBAR twice (low before the
/// MMU enable, high after the canary), so by the time this runs it is a
/// re-affirmation — and a useful one, because it derives the address a
/// *third* way and the banner reads the register back to show all three
/// agreed.
pub fn install() {
    // SAFETY: __vectors is the 2048-aligned table from this file's
    // global_asm! (VBAR_EL1 bits [10:0] are RES0; linker.ld ASSERTs the
    // alignment). Writing VBAR_EL1 at EL1 is always permitted. The adrp
    // is PC-relative: executed at kmain's higher-half PC it yields the
    // table's higher-half address — the same value the boot stub loaded
    // from its literal pool at step 10, arrived at by a different route.
    // The `isb` flushes the pipeline so the very next instruction already
    // faults through the new table.
    unsafe {
        core::arch::asm!(
            "adrp {t}, __vectors",            // page of the table (PC-relative)
            "add  {t}, {t}, :lo12:__vectors",
            "msr  VBAR_EL1, {t}",             // the table is now the law
            "isb",                            // ...starting with the next fetch
            t = out(reg) _,
            options(nostack, preserves_flags),
        );
    }
}

/// Read VBAR_EL1 back, for the banner: print what the register actually
/// holds, not what we hope we wrote.
pub fn vbar() -> u64 {
    let v: u64;
    // SAFETY: reading VBAR_EL1 at EL1 is always legal and side-effect-free.
    unsafe {
        core::arch::asm!(
            "mrs {v}, VBAR_EL1",
            v = out(reg) v,
            options(nomem, nostack, preserves_flags)
        );
    }
    v
}

/// The virtual address of the `__el0_return` trampoline. M5's exit
/// syscall handler writes this into ELR_EL1 so the `eret` returns to
/// EL1 (the monitor) instead of EL0.
#[allow(dead_code)] // superseded by return_to_monitor(); kept as a public symbol address API
pub fn el0_return_addr() -> u64 {
    let addr: u64;
    // SAFETY: adrp+add computes the PC-relative address of a known
    // linker symbol; no memory access, no side effects.
    unsafe {
        core::arch::asm!(
            "adrp {a}, __el0_return",
            "add  {a}, {a}, :lo12:__el0_return",
            a = out(reg) addr,
            options(nomem, nostack, preserves_flags),
        );
    }
    addr
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// The four exception kinds, as the vector stubs encode them (argument x1).
#[derive(Clone, Copy)]
enum Kind {
    Synchronous,
    Irq,
    Fiq,
    SError,
}

impl Kind {
    /// The stubs can only pass 0..=3 — see the `.macro` arguments above.
    fn from_raw(v: u64) -> Self {
        match v {
            0 => Self::Synchronous,
            1 => Self::Irq,
            2 => Self::Fiq,
            _ => Self::SError,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Synchronous => "synchronous",
            Self::Irq => "IRQ",
            Self::Fiq => "FIQ",
            Self::SError => "SError",
        }
    }
}

/// The four origins (which row of vector slots fired; argument x2).
#[derive(Clone, Copy)]
enum Source {
    CurrentSpEl0,
    CurrentSpElx,
    LowerAArch64,
    LowerAArch32,
}

impl Source {
    fn from_raw(v: u64) -> Self {
        match v {
            0 => Self::CurrentSpEl0,
            1 => Self::CurrentSpElx,
            2 => Self::LowerAArch64,
            _ => Self::LowerAArch32,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::CurrentSpEl0 => "current EL on SP_EL0",
            Self::CurrentSpElx => "current EL on SP_ELx",
            Self::LowerAArch64 => "a lower EL (AArch64)",
            Self::LowerAArch32 => "a lower EL (AArch32)",
        }
    }
}

/// Every one of the sixteen vector entries lands here (via `bl` in the
/// stub), carrying the frame and two small integers saying which slot
/// fired.
///
/// Returning from this function means "resume the interrupted code": the
/// stub restores every register from the frame and `eret`s. The fatal
/// paths never return — they park inside [`die`].
#[unsafe(no_mangle)] // the assembly stub must be able to name this symbol
extern "C" fn exception_dispatch(frame: &mut TrapFrame, kind: u64, source: u64) {
    // Re-entry guard, doubling as the console-lock bypass (the `oops`
    // machinery lives in main.rs). If a second exception arrives while we
    // are reporting a first, ESR/ELR/FAR have already been overwritten —
    // the first report's evidence is gone — and the normal locked print
    // path could deadlock besides. Report the little we still know, then
    // stop digging.
    if crate::oops_enter() {
        println!();
        println!("*** nested exception while reporting an exception ***");
        println!("  the first report's ESR/ELR were just overwritten; trusting nothing.");
        println!(
            "  new esr: {:#018x}  new elr: {:#018x}",
            frame.esr, frame.elr
        );
        super::park();
    }

    let kind = Kind::from_raw(kind);
    let source = Source::from_raw(source);

    match (kind, source) {
        // The one column that is supposed to be live: our own EL, our own
        // stack. Synchronous faults may recover; the others cannot (yet).
        (Kind::Synchronous, Source::CurrentSpElx) => handle_sync(frame, kind, source),
        (Kind::Irq, Source::CurrentSpElx) => handle_irq(frame, kind, source),
        (Kind::Fiq, Source::CurrentSpElx) => handle_fiq(frame, kind, source),
        (Kind::SError, Source::CurrentSpElx) => {
            report(
                frame,
                kind,
                source,
                format_args!(
                    "SError — an asynchronous external abort: something failed after \
                     the fact (e.g. a buffered write to a bad address)"
                ),
            );
            die()
        }
        // M5/M6: a synchronous exception from EL0 — this is a syscall (svc)
        // or a user fault (data/instruction abort). The SVC case is the
        // whole point of M5; faults are serviced by killing the task (M6
        // gave them a real handler — kill_task_from_el0, verified).
        (Kind::Synchronous, Source::LowerAArch64) => handle_sync_from_el0(frame, kind, source),
        // IRQ/FIQ/SError from EL0: same handlers as the kernel's own —
        // the source doesn't change what the GIC or the timer need.
        (Kind::Irq, Source::LowerAArch64) => handle_irq(frame, kind, source),
        (Kind::Fiq, Source::LowerAArch64) => handle_fiq(frame, kind, source),
        (Kind::SError, Source::LowerAArch64) => {
            report(
                frame,
                kind,
                source,
                format_args!(
                    "SError from EL0 — an asynchronous external abort from userspace"
                ),
            );
            die()
        }
        // Every other slot belongs to a world that does not exist yet:
        // we never select SP_EL0, and nothing runs at a lower EL in
        // AArch32 (Opal never runs 32-bit code). Reaching one of these
        // *is* the bug being reported.
        (kind, source) => {
            report(
                frame,
                kind,
                source,
                format_args!(
                    "an exception from a context that should not exist — nothing runs \
                     on SP_EL0 or in AArch32"
                ),
            );
            die()
        }
    }

    // Exactly two paths return out of handle_sync: a recovered brk and a
    // recovered svc. Stand down the emergency console and resume.
    crate::oops_exit();
}

// ---------------------------------------------------------------------------
// Synchronous exceptions: the decoder
// ---------------------------------------------------------------------------

// ESR_EL1 exception-class values (bits [31:26]; Arm DDI 0601). Only the
// classes this kernel can actually produce today get names — the full EC
// table is fifty rows, and transcribing it would be noise, not teaching.
// Anything else prints raw and honest in the default arm.
const EC_UNKNOWN: u64 = 0x00; // undefined/disabled instruction & friends
const EC_SVC64: u64 = 0x15; // svc from AArch64
const EC_IABT_SAME_EL: u64 = 0x21; // instruction abort, same EL
const EC_IABT_LOWER_EL: u64 = 0x20; // instruction abort, lower EL (EL0)
const EC_PC_ALIGN: u64 = 0x22; // PC alignment fault
const EC_DABT_SAME_EL: u64 = 0x25; // data abort, same EL
const EC_DABT_LOWER_EL: u64 = 0x24; // data abort, lower EL (EL0)
const EC_SP_ALIGN: u64 = 0x26; // SP alignment fault
const EC_BRK64: u64 = 0x3c; // brk from AArch64

/// Synchronous exceptions from the kernel itself — the interesting ones.
/// Two recover (brk, svc); the rest are fatal. M2 changed their *texture*
/// — the page-table fault families are reachable now (and demonstrable:
/// the monitor's `guard`/`wx`/`noexec`/`low` commands produce one each),
/// and the worst overflows became preventable. M5/M6 extended fault
/// handling to EL0: user-space faults are now serviced (the task is
/// killed, the kernel survives). Kernel-space faults remain fatal —
/// `die()` is still the honest answer when the kernel itself faults.
fn handle_sync(frame: &mut TrapFrame, kind: Kind, source: Source) {
    let ec = (frame.esr >> 26) & 0x3f; // exception class: bits [31:26]
    let iss = frame.esr & 0x01ff_ffff; // instruction-specific syndrome: bits [24:0]

    match ec {
        EC_BRK64 => {
            // The brk immediate travels in ISS bits [15:0].
            let imm = iss & 0xffff;
            report(
                frame,
                kind,
                source,
                format_args!("BRK #{imm:#x} — a software breakpoint (EC {EC_BRK64:#04x})"),
            );
            // The architecture's "preferred return address" for brk is the
            // brk instruction ITSELF — like an undefined instruction, and
            // unlike svc. `eret` with ELR untouched would re-execute it:
            // an infinite breakpoint loop. To resume, step over it by hand.
            // +4 means exactly "one instruction" because AArch64
            // instructions are fixed-width: four bytes, every one of them,
            // brk and its immediate included.
            frame.elr += 4;
            println!("  verdict : recovered — ELR pointed AT the brk (its preferred return");
            println!("            address); we advanced it one instruction so eret resumes");
            println!("            just past the breakpoint.");
            println!();
        }
        EC_SVC64 => {
            // The svc immediate is in ISS bits [15:0], but real ABIs pass
            // the syscall number in a register (x8, on AArch64 Linux)
            // precisely because an immediate cannot be chosen at runtime.
            let imm = iss & 0xffff;
            report(
                frame,
                kind,
                source,
                format_args!(
                    "SVC #{imm:#x} with x8 = {} — a supervisor call (EC {EC_SVC64:#04x})",
                    frame.x[8]
                ),
            );
            println!("  verdict : recovered — and note: nothing to fix. For svc, ELR already");
            println!("            points past the instruction (a call, not an accident), so");
            println!("            eret resumes there unadjusted. M5 turned this report into");
            println!("            a real system-call dispatcher (see handle_sync_from_el0).");
            println!();
        }
        EC_DABT_SAME_EL => {
            // Data-abort ISS fields (DDI 0601): WnR = bit 6 (write, not
            // read), DFSC = bits [5:0], FnV = bit 10 (FAR not valid —
            // possible only for external aborts).
            let dfsc = iss & 0x3f;
            let wnr = if iss & (1 << 6) != 0 {
                "write to"
            } else {
                "read from"
            };
            if iss & (1 << 10) == 0 {
                report(
                    frame,
                    kind,
                    source,
                    format_args!(
                        "data abort — a {wnr} {:#x} failed (EC {EC_DABT_SAME_EL:#04x}, \
                         DFSC {dfsc:#04x})",
                        frame.far
                    ),
                );
            } else {
                report(
                    frame,
                    kind,
                    source,
                    format_args!(
                        "data abort — a {wnr} an address the CPU declines to report \
                         (FnV=1; EC {EC_DABT_SAME_EL:#04x}, DFSC {dfsc:#04x})"
                    ),
                );
            }
            println!("  status  : {}", fault_status(dfsc));
            print_walk_level(dfsc);
            die()
        }
        EC_IABT_SAME_EL => {
            let ifsc = iss & 0x3f; // same encoding as DFSC, for fetches
            report(
                frame,
                kind,
                source,
                format_args!(
                    "instruction abort — fetching code from {:#x} failed \
                     (EC {EC_IABT_SAME_EL:#04x}, IFSC {ifsc:#04x})",
                    frame.far
                ),
            );
            println!("  status  : {}", fault_status(ifsc));
            print_walk_level(ifsc);
            die()
        }
        EC_PC_ALIGN => {
            report(
                frame,
                kind,
                source,
                format_args!(
                    "PC alignment fault — a branch to misaligned address {:#x} \
                     (EC {EC_PC_ALIGN:#04x})",
                    frame.far
                ),
            );
            die()
        }
        EC_SP_ALIGN => {
            // Honesty about reachability: for the kernel's *own* faults
            // this arm is dead code today. An SP-alignment fault leaves SP
            // misaligned, the vector stub's first store through SP then
            // re-faults before reaching Rust, and the machine loops with
            // no report (see step 4 in the stub). The arm earns its keep
            // when exceptions start arriving from a *different* stack —
            // lower-EL faults in M5, or the SPSel split's dedicated
            // exception stack.
            report(
                frame,
                kind,
                source,
                format_args!(
                    "SP alignment fault — the stack pointer was not 16-byte aligned \
                     when used (EC {EC_SP_ALIGN:#04x})"
                ),
            );
            die()
        }
        EC_UNKNOWN => {
            report(
                frame,
                kind,
                source,
                format_args!(
                    "unknown reason (EC {EC_UNKNOWN:#04x}) — usually an undefined or \
                     disabled instruction; ELR points at the culprit"
                ),
            );
            die()
        }
        _ => {
            report(
                frame,
                kind,
                source,
                format_args!(
                    "exception class {ec:#04x}, ISS {iss:#09x} — a class this kernel does \
                     not decode yet (the full table: Arm DDI 0601, ESR_EL1.EC)"
                ),
            );
            die()
        }
    }
}

/// Translate a DFSC/IFSC fault-status code (the low six bits of an abort
/// ISS) into words. M1 wrote this table for faults it could not yet
/// cause ("M2 territory", it said); M2 made every family below reachable
/// on purpose — each has a monitor command that demonstrates it.
/// `pub(crate)` since M2: the monitor's `translate` command reuses it to
/// decode PAR_EL1.FST, which speaks the same encoding.
pub(crate) fn fault_status(fsc: u64) -> &'static str {
    match fsc {
        // 0b0001LL, 0b0010LL, 0b0011LL: LL = the table walk level 0-3
        // (printed separately by print_walk_level).
        0x04..=0x07 => {
            "translation fault — the walk hit an INVALID descriptor: \
                 nothing is mapped at this address"
        }
        0x08..=0x0b => {
            "access-flag fault — a mapping with AF=0. mmu.rs's constructors \
                 bake AF in, so suspect a hand-written descriptor"
        }
        0x0c..=0x0f => {
            "permission fault — mapped, but the descriptor forbids this \
                 access; W^X doing its job"
        }
        0x10 => {
            "synchronous external abort — the translation SUCCEEDED and the bus \
                 said no: mapped, but nothing lives there"
        }
        0x14..=0x17 => "synchronous external abort during a page-table walk",
        0x21 => {
            "alignment fault — Device memory (MMIO) forbids unaligned access \
                 (Normal memory has tolerated it since M2)"
        }
        _ => "a fault status this kernel does not decode (DDI 0601, ESR_EL1 has the full list)",
    }
}

/// The page-table fault families encode where the walk failed in their
/// low two bits; saying "level 3" out loud turns a hex code into a
/// pointer at the guilty table.
fn print_walk_level(fsc: u64) {
    if (0x04..=0x0f).contains(&fsc) {
        println!("  level   : the walk failed at level {}", fsc & 0b11);
    }
}

// ---------------------------------------------------------------------------
// M5: Synchronous exceptions from EL0 — syscalls and user faults
// ---------------------------------------------------------------------------

/// Handle a synchronous exception that arrived from EL0 (the "lower EL,
/// AArch64" vector slot, offset 0x400). This is the M5 milestone: the
/// kernel's first real exception *service*.
///
/// Two cases:
/// - **SVC** (EC 0x15): a syscall. Dispatch to [`user::handle_svc_from_el0`],
///   which reads x8 for the syscall number, handles it, and either
///   returns (eret back to EL0) or sets up a return to EL1 (for exit).
/// - **Anything else**: a user fault (data abort, instruction abort,
///   alignment, etc.). Report it, then call [`user::kill_task_on_fault`]
///   — the kernel condemns the user address space and returns to the
///   monitor. This is the first fault the kernel *recovers from* rather
///   than merely reporting: M1 taught it to say what happened; M5 teaches
///   it to survive it. M6 will replace the kill with a real scheduler-aware
///   task teardown.
fn handle_sync_from_el0(frame: &mut TrapFrame, kind: Kind, source: Source) {
    let ec = (frame.esr >> 26) & 0x3f;
    match ec {
        EC_SVC64 => {
            let imm = (frame.esr & 0x01ff_ffff) & 0xffff;
            // Route to the M5 syscall handler. It may modify the frame
            // (x0 for return value, or SPSR/ELR for the exit path).
            super::user::handle_svc_from_el0(frame);
            // The handler returns; exception_dispatch's tail calls
            // oops_exit and the stub erets. For write, eret goes to
            // EL0 (ELR points past the svc). For exit, eret goes to
            // the trampoline at EL1.
            let _ = (kind, source, imm);
        }
        EC_DABT_LOWER_EL => {
            // A data abort from EL0 — the user touched a bad address.
            // EC 0x24 is "Data Abort from a lower EL": the ISS/DFSC
            // encoding is the same as same-EL (0x25), but the syndrome
            // also carries the access size and sign in bits [15:11] for
            // lower-EL aborts — we ignore those; DFSC (bits [5:0]) is
            // all we need to classify the fault.
            let dfsc = frame.esr & 0x3f;
            let wnr = if frame.esr & (1 << 6) != 0 {
                "write to"
            } else {
                "read from"
            };
            report(
                frame,
                kind,
                source,
                format_args!(
                    "EL0 data abort — a userspace {wnr} {:#x} failed (EC {EC_DABT_LOWER_EL:#04x}, DFSC {dfsc:#04x})",
                    frame.far
                ),
            );
            println!("  status  : {}", fault_status(dfsc));
            println!("  verdict : user task killed — the kernel survives its first serviced fault.");
            println!("            (In a real OS this would kill the task, not the kernel. Now it does.)");
            kill_task_from_el0(frame);
            return;
        }
        EC_IABT_LOWER_EL => {
            let ifsc = frame.esr & 0x3f;
            report(
                frame,
                kind,
                source,
                format_args!(
                    "EL0 instruction abort — fetching code from {:#x} failed (EC {EC_IABT_LOWER_EL:#04x}, IFSC {ifsc:#04x})",
                    frame.far
                ),
            );
            println!("  status  : {}", fault_status(ifsc));
            println!("  verdict : user task killed — attempted to execute an unmapped/forbidden address.");
            kill_task_from_el0(frame);
            return;
        }
        _ => {
            report(
                frame,
                kind,
                source,
                format_args!(
                    "EL0 exception class {ec:#04x}, ISS {:#09x} — not handled yet",
                    frame.esr & 0x01ff_ffff
                ),
            );
            println!("  verdict : user task killed — unhandled EL0 exception.");
            kill_task_from_el0(frame);
            return;
        }
    }
}

/// Kill the faulting EL0 task and either resume the scheduler (if there
/// is another Ready task) or return to the monitor (if all tasks are
/// dead). This is the M6 evolution of M5's `kill_task_on_fault`:
///
/// - M5: one task faults → kernel returns to monitor (kills the whole
///   "OS" because there was only one task).
/// - M6: one task faults → kernel kills *that task*, marks its slot
///   Exited, and switches to the next Ready task. The OS keeps running.
///
/// If `kill_current_task` returns `true`, the frame now holds the next
/// task's saved context and `__vectors_restore` will eret to it. If it
/// returns `false`, there are no more tasks — we disable preemption,
/// condemn TTBR0, and return to the monitor via `on_el0_return`.
///
/// The `oops_exit()` call in `exception_dispatch`'s tail runs on the
/// normal return path (after `kill_task_from_el0` returns). When we
/// switch to a new task, we return through the same path — `oops_exit`
/// is safe because the emergency flag is per-CPU and we're still at EL1
/// in the exception handler.
fn kill_task_from_el0(frame: &mut TrapFrame) {
    // SAFETY: we are in an exception handler (DAIF set by hardware on
    // exception entry), single-core, no concurrent access. The frame
    // is the on-stack TrapFrame the stub built.
    let switched = unsafe { crate::sched::kill_current_task(frame) };
    if !switched {
        // No more tasks — return to the monitor. Disable preemption
        // (timer may still be armed) and condemn the user address space.
        crate::sched::preempt_off();
        crate::arch::aarch64::mmu::condemn_low_half();
        crate::arch::aarch64::user::return_to_monitor();
    }
    // If switched: the frame now holds the next task's context. We
    // return through exception_dispatch → oops_exit → __vectors_restore,
    // which erets to the new task. The OS keeps running.
}

// ---------------------------------------------------------------------------
// IRQ and FIQ: two demux points, deliberately separate
// ---------------------------------------------------------------------------

/// The IRQ entry point. Since M3, this is the heartbeat: the GIC
/// presents a pending interrupt, we acknowledge it (getting the ID),
/// dispatch on the ID, signal end-of-interrupt, and *return*. This is
/// the first exception in Opal that is routine — it runs, does its
/// job, and resumes the interrupted code, every time.
///
/// The dispatch is a `match` on the interrupt ID, with the timer (30)
/// as the one case today. An unknown ID is reported and dropped (the
/// EOI still runs, so the GIC does not get stuck) — honest about a
/// surprise, but not fatal. M6's scheduler will add the timer-tick
/// preemption path here; M3 just proves the pipe works.
fn handle_irq(frame: &mut TrapFrame, kind: Kind, source: Source) {
    // Since M3, this handler acknowledges the GIC, dispatches on the
    // interrupt ID, and returns. M6 adds the preemptive path: on a
    // timer tick, if a user task is running and preempt is enabled,
    // we call save_and_switch to preempt it in favor of the next
    // Ready task. The frame is the interrupted EL0 context (for
    // timer-from-EL0) or the kernel's own context (for timer-from-EL1);
    // save_and_switch only switches if there is another task Ready.
    let _ = (kind, source);

    let gic = crate::board::virt::gic();
    let id = gic.acknowledge();

    // 1022-1023 are special "spurious" IDs: the GIC returns them when
    // nothing is actually pending (e.g. two interrupts raced and the
    // other context already ack'd). The standard response is to EOI
    // normally for 1022, and to *not* EOI for 1023 (the GIC drops it).
    // We treat both as "nothing to do" — a spurious IRQ on a kernel
    // with one interrupt source is worth a line, not a panic.
    if id >= 1020 {
        if id == 1020 {
            gic.end_interrupt(id);
        }
        return;
    }

    match id {
        crate::hal::gicv2::TIMER_IRQ => {
            crate::arch::aarch64::timer::on_tick();
            // Re-arm the timer for the next period. The period is
            // stored in a static set by kmain (see `start_ticking`).
            let period = crate::arch::aarch64::timer::TICK_PERIOD
                .load(core::sync::atomic::Ordering::Relaxed);
            if period > 0 {
                crate::arch::aarch64::timer::rearm(period);
            }

            // M6: wake sleeping tasks whose deadline has elapsed.
            // This is the timer-driven wake condition — the third
            // blocking primitive after recvblk and sendblk. A task
            // that called sleep(ticks) is Blocked with a wake_tick
            // deadline; if that deadline has passed, wake it (Ready,
            // enqueued) so the scheduler picks it up. The scan is
            // O(MAX_TASKS) — tiny (8 slots), and only sleeping tasks
            // have wake_tick != 0, so the common case is 8 cheap
            // comparisons.
            // SAFETY: IRQ handler, DAIF set, single-core.
            unsafe { crate::sched::wake_sleepers() };

            // M6: preemptive scheduling. If preempt is enabled and a
            // user task is currently running, the timer tick is our
            // chance to switch to the next Ready task. save_and_switch
            // saves the interrupted context (the frame the vector stub
            // built — which is the user's EL0 register state, since
            // the timer interrupted EL0) into the current task's TCB,
            // picks the next Ready task, and overwrites the frame with
            // its saved state. When __vectors_restore runs, it erets
            // to the *new* task instead of the one that was running.
            //
            // If there is no other task Ready (only one task running,
            // or no tasks at all — the kernel was interrupted), the
            // switch returns false and the interrupted code resumes
            // exactly where it stopped. This is the same no-op path
            // the cooperative yield takes when alone.
            //
            // SAFETY: We are in an IRQ exception handler. DAIF is set
            // (the CPU masks async exceptions on exception entry), so
            // the scheduler state is safe to touch — no re-entrant IRQ
            // can fire. `frame` is the on-stack TrapFrame the stub
            // built; save_and_switch copies bytes in and out of it,
            // and the restore stub reads it only after we return.
            if crate::sched::preempt_enabled() && crate::sched::current_tid() != 0 {
                if unsafe { crate::sched::save_and_switch(frame) } {
                    crate::sched::bump_preempts();
                }
            }
        }
        other => {
            // Unknown interrupt: report and move on. The EOI is
            // important — without it the GIC never clears the active
            // state and the interrupt (or the whole priority band)
            // is stuck.
            println!();
            println!("*** IRQ: unknown interrupt ID {other} — acknowledging and continuing ***");
        }
    }

    // End of interrupt: tell the GIC we are done with this ID. Must
    // match the acknowledged ID exactly. This is the last GIC
    // operation before returning — after it, the GIC can present the
    // next pending interrupt (which will be taken after `eret`).
    gic.end_interrupt(id);
}

/// The FIQ entry point — kept separate from IRQ on purpose, forever. On
/// QEMU's GIC nothing routes to FIQ, so this looks like dead structure;
/// on Apple Silicon the architectural timer and fast IPIs arrive over FIQ
/// (docs/02 §3, item 6). Folding FIQ into IRQ today would be a refactor
/// M7 pays for at the worst time. Keeping the split costs one vector
/// entry and this stub.
///
/// ## What FIQ looks like on Apple Silicon
///
/// Unlike the GIC path (where `handle_irq` acknowledges an interrupt ID
/// from the controller, dispatches on it, then signals EOI), the Apple
/// timer arrives as a *direct* FIQ — there is no interrupt controller
/// in the path. The handler must:
///
/// 1. Check `CNTV_CTL_EL0.ISTATUS` (bit 1): did the virtual timer fire?
///    If so, service the tick: bump the counter, rearm the comparator.
/// 2. On future hardware, check AIC registers for device FIQs.
/// 3. Return — `eret` resumes the interrupted code.
///
/// On QEMU this handler is never entered (the GIC routes the timer to
/// IRQ, not FIQ). But the structure is here, verified by the build, and
/// the first Apple boot fills in the AIC branch.
fn handle_fiq(frame: &mut TrapFrame, kind: Kind, source: Source) {
    let _ = (kind, source);

    // Check whether the virtual timer fired. On Apple Silicon the
    // architectural timer arrives as FIQ; on QEMU the GIC delivers it
    // as IRQ, so this branch is dormant — but structurally correct.
    if crate::arch::aarch64::timer::fired() {
        crate::arch::aarch64::timer::on_tick();
        let period = crate::arch::aarch64::timer::TICK_PERIOD
            .load(core::sync::atomic::Ordering::Relaxed);
        if period > 0 {
            crate::arch::aarch64::timer::rearm(period);
        }

        // M6: wake sleeping tasks and preempt, same as the IRQ path.
        // SAFETY: FIQ handler, DAIF set, single-core.
        unsafe { crate::sched::wake_sleepers() };
        if crate::sched::preempt_enabled() && crate::sched::current_tid() != 0 {
            if unsafe { crate::sched::save_and_switch(frame) } {
                crate::sched::bump_preempts();
            }
        }
        return;
    }

    // The timer didn't fire. On Apple Silicon, a FIQ can also be an
    // AIC event — the AIC delivers device IRQs and IPIs on the FIQ
    // line (the timer is the one exception: it arrives directly, not
    // through AIC). On QEMU nothing routes to FIQ at all, so the AIC
    // base is zero (board::apple::init is never called) and this
    // branch is dormant.
    let aic_base = crate::board::apple::aic_base();
    if aic_base != 0 {
        // SAFETY: constructing an Aic from the cached base is safe —
        // Aic::new is const and stores the field; the MMIO reads in
        // event() are volatile reads from a mapped device region. We
        // are in a FIQ handler with DAIF set, single-core, so there is
        // no re-entrancy on the AIC registers.
        let aic = crate::hal::aic::Aic::new(aic_base);
        let ev = aic.event();
        if ev != 0 {
            let et = crate::hal::aic::event_type(ev);
            let num = crate::hal::aic::event_num(ev);
            match et {
                crate::hal::aic::EVENT_TYPE_IRQ => {
                    // Device IRQ. AIC's event read auto-masks the IRQ;
                    // after handling, re-enable it. Today there are
                    // no device drivers registered — this is the
                    // dispatch point they will plug into. Report so
                    // the first Apple boot shows the event arrived.
                    report(
                        frame,
                        kind,
                        source,
                        format_args!(
                            "FIQ — AIC device IRQ {num}; no handler registered \
                             (M7 dispatch point). Re-unmasking."
                        ),
                    );
                    aic.unmask_irq(num);
                    return;
                }
                crate::hal::aic::EVENT_TYPE_IPI => {
                    // Inter-processor interrupt. On AIC v1, IPIs use a
                    // separate ack/mask path. For a single-core
                    // teaching kernel this is structurally present
                    // but dormant — SMP (PSCI CPU_ON) is M6-deferred.
                    report(
                        frame,
                        kind,
                        source,
                        format_args!(
                            "FIQ — AIC IPI {num}; single-core kernel, no IPI \
                             handlers yet (SMP deferred from M6)."
                        ),
                    );
                    return;
                }
                crate::hal::aic::EVENT_TYPE_FIQ => {
                    // AIC can also route certain sources as FIQ-type
                    // events. On M1 the architectural timer bypasses
                    // AIC, so this is for other FIQ sources. Report
                    // and continue — the timer path above already
                    // handled the common case.
                    report(
                        frame,
                        kind,
                        source,
                        format_args!(
                            "FIQ — AIC FIQ-type event {num}; no handler (M7)."
                        ),
                    );
                    return;
                }
                _ => {
                    report(
                        frame,
                        kind,
                        source,
                        format_args!(
                            "FIQ — AIC event {ev:#x}: unknown type {et}, num {num}."
                        ),
                    );
                    return;
                }
            }
        }
    }

    // No timer, no AIC event. On QEMU this is the only path (nothing
    // fires FIQ at all). Report and recover — a spurious FIQ is worth
    // a line, not a kernel panic.
    report(
        frame,
        kind,
        source,
        format_args!(
            "FIQ — not the virtual timer; on QEMU nothing routes to FIQ at all. \
             On Apple Silicon this is the timer or an AIC device interrupt (M7)."
        ),
    );
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// The top half of every report: which slot fired, the cause, the four
/// captured system registers, and the full register table. The caller
/// finishes with a verdict line — recovered or fatal.
fn report(frame: &TrapFrame, kind: Kind, source: Source, cause: fmt::Arguments<'_>) {
    println!();
    println!("*** exception: {}, from {} ***", kind.name(), source.name());
    println!("  cause   : {cause}");
    println!(
        "  esr     : {:#018x}  (syndrome — decoded above)",
        frame.esr
    );
    println!(
        "  elr     : {:#018x}  (preferred return address)",
        frame.elr
    );
    println!("  spsr    : {:#018x}  (interrupted PSTATE)", frame.spsr);
    println!(
        "  far     : {:#018x}  (fault address — aborts/alignment only)",
        frame.far
    );
    println!("  registers at the moment of the exception:");
    for i in (0..30).step_by(2) {
        println!(
            "    x{:<2} {:#018x}    x{:<2} {:#018x}",
            i,
            frame.x[i],
            i + 1,
            frame.x[i + 1]
        );
    }
    // The frame's sp is handler-SP + frame size (stub, step 3), which is
    // the interrupted SP only when the interrupted code was already on
    // SP_EL1 — the SP_ELx column. For every other source, say so rather
    // than present the handler's stack as the victim's.
    let sp_note = match source {
        Source::CurrentSpElx => "",
        Source::CurrentSpEl0 => "  (handler stack — interrupted context ran on SP_EL0)",
        Source::LowerAArch64 | Source::LowerAArch32 => {
            "  (handler stack — interrupted context ran at a lower EL)"
        }
    };
    println!(
        "    x30 {:#018x}    sp  {:#018x}{sp_note}",
        frame.x[30], frame.sp
    );
}

/// The standard fatal ending. With no demand paging and no kernel-space
/// fault recovery, "report and park" is still the honest maximum for
/// kernel-level faults. M2 raised the *quality* of the reports (fault
/// families that name the failing walk level) and made the worst faults
/// preventable (the guard, W^X). M5/M6 added user-space fault recovery
/// (kill_task_from_el0 — the task dies, the kernel lives). But a fault
/// in the kernel itself still means the kernel is broken; parking is
/// the truthful response until (if ever) kernel fault recovery arrives.
fn die() -> ! {
    println!("  verdict : FATAL — the kernel cannot repair this yet; parking core 0.");
    println!("            (QEMU is still alive, just idle: Ctrl-A X to leave.)");
    println!();
    super::park()
}

// ---------------------------------------------------------------------------
// Deliberate faults — the monitor's demo commands
// ---------------------------------------------------------------------------

/// Hit a breakpoint on purpose. The handler reports, steps ELR past the
/// brk, and execution continues — this function *returns*, which is the
/// whole demonstration.
pub fn demo_brk() {
    // SAFETY: brk raises a synchronous exception (EC 0x3c) that
    // handle_sync recovers from by advancing ELR past this instruction.
    // The handler restores every register, so resuming here is sound.
    unsafe { core::arch::asm!("brk #0xf00d", options(nostack)) };
}

/// Make a supervisor call with `arg` in x8 — the register AArch64 Linux
/// uses for the syscall number, a convention M5 will adopt.
pub fn demo_svc(arg: u64) {
    // SAFETY: svc raises EC 0x15; handle_sync reports and returns without
    // touching ELR (an svc's return address already points past it).
    unsafe { core::arch::asm!("svc #0", in("x8") arg, options(nostack)) };
}

/// Load eight bytes from an odd address — and, since M2, **return them**.
///
/// This exact load was M1's "goodbye": with the MMU off every access was
/// Device-nGnRnE, and Device memory *requires* natural alignment — the
/// CPU raised a data abort (DFSC 0x21) instead of performing it. Since M2
/// the stack is Normal write-back memory and SCTLR_EL1.A is deliberately
/// clear, so the same instruction simply... loads. The signature changed
/// from `-> !` to `-> u64` the day the fault stopped: the old
/// "unreachable" park-backstop became *reachable*, and a survivable demo
/// that silently parks is exactly the species of silence this kernel
/// exists to abolish.
pub fn demo_unaligned() -> u64 {
    // Recognizable bytes, so the loaded value proves which ones moved:
    // an unaligned read at +1 sees bytes 1..8 of the first word and
    // byte 0 of the second — little-endian, that is 0x0011223344556677.
    // `expose_provenance`, not `addr`: the asm below genuinely accesses
    // memory through this integer, so the pointer's provenance must be
    // exposed first — the same strict-provenance discipline as every MMIO
    // access in hal/pl011.rs. (`addr()` deliberately severs provenance,
    // which would disavow exactly the access we are about to make.)
    let buf = [0x1122_3344_5566_7788u64, 0x99AA_BBCC_DDEE_FF00];
    let misaligned = (&raw const buf).expose_provenance() + 1;
    let loaded: u64;
    // SAFETY: the load completes now and reads bytes 1..9 of `buf` —
    // inside a live, exposed allocation. Done in assembly because Rust's
    // typed reads are allowed to *assume* alignment: we want the CPU's
    // opinion of a misaligned access, not language-level UB. (M1's
    // version of this comment said "the load never completes"; M2
    // falsified it, which is the whole demonstration.)
    unsafe {
        core::arch::asm!(
            "ldr {v}, [{addr}]",
            addr = in(reg) misaligned,
            v = out(reg) loaded,
            options(nostack, preserves_flags),
        );
    }
    loaded
}

/// Read from `addr`, which the caller promises is inside the bus-error
/// window — fatal by design, but for a *different* reason than M1's
/// version of this demo.
///
/// M1 read past the end of RAM unmapped and raw; since M2 every unmapped
/// address dies politely in the walk (DFSC 0x04..0x07) and could never
/// reach the bus. To keep the bus's own "no" demonstrable, mmu.rs maps
/// the 32 MiB past RAM as Device memory on purpose: the walk *succeeds*,
/// the access goes out on the bus, nothing answers, and the failure comes
/// back as a synchronous external abort (DFSC 0x10 — since machine type
/// virt-2.11, QEMU models this like real hardware). Page-table "no"
/// versus bus "no", one command each — that contrast is the lesson.
pub fn demo_abort(addr: u64) -> ! {
    // SAFETY: the load never completes — the external-abort handler
    // reports and parks.
    unsafe {
        core::arch::asm!(
            "ldr {scratch}, [{addr}]",
            addr = in(reg) addr,
            scratch = out(reg) _,
            options(nostack),
        );
    }
    super::park()
}

/// Write just below the stack — fatal by design: the 16 KiB under
/// `__stack_bottom` is the guard, the page mmu.rs deliberately never
/// maps. A real overflow's first touch is a *spill*, so the demo writes
/// rather than reads; expect a data abort, WnR=1, DFSC 0x07 (translation
/// fault, level 3), FAR pointing into the guard. This is M0's oldest
/// debt being seen to pay: overflow used to eat the top of .bss in
/// silence; now it has somewhere to fault *into*.
pub fn demo_guard() -> ! {
    unsafe extern "C" {
        static __stack_bottom: u8;
    }
    // Eight bytes below the stack's lowest legal address — inside the
    // guard. (Taking a linker symbol's address is safe; it is the store
    // that the MMU will, by design, refuse.)
    let target = (&raw const __stack_bottom).expose_provenance() - 8;
    // SAFETY: the store never completes — the guard page is unmapped and
    // the translation-fault handler parks.
    unsafe {
        core::arch::asm!(
            "str xzr, [{addr}]",
            addr = in(reg) target,
            options(nostack),
        );
    }
    super::park()
}

/// What `wx` writes at: a value pinned in read-only memory. A plain
/// `static` of plain data lands in .rodata, which mmu.rs maps AP=read-
/// only — the descriptor, not the Rust type system, is what objects here.
static RODATA_SENTINEL: u64 = 0x524F_4F4E_4C59; // "ROONLY", as bytes

/// Write to .rodata — fatal by design: the mapping exists (the walk
/// succeeds) but its descriptor says read-only, so expect a data abort,
/// WnR=1, DFSC 0x0F (permission fault, level 3). Compare with `guard`:
/// same EC, different family — "no mapping" versus "mapping says no".
pub fn demo_wx() -> ! {
    let target = (&raw const RODATA_SENTINEL).expose_provenance();
    // SAFETY: the store never completes — the permission-fault handler
    // parks. (If it ever DID complete, the const-eval'd sentinel would
    // make the corruption visible, but the report should come first.)
    unsafe {
        core::arch::asm!(
            "str xzr, [{addr}]",
            addr = in(reg) target,
            options(nostack),
        );
    }
    super::park()
}

/// What `noexec` jumps at: one valid AArch64 `ret` (0xD65F03C0), parked
/// in .data. `static mut` so it cannot be promoted to .rodata — the
/// demonstration needs it in writable-therefore-PXN memory.
static mut DATA_CODE: [u32; 1] = [0xD65F_03C0];

/// Branch into .data — fatal by design, and *falsifiably* so: the target
/// holds a genuine `ret`, so if PXN were broken this function would
/// quietly RETURN, and the caller prints the betrayal. With PXN doing its
/// job, the fetch itself faults: an instruction abort, IFSC 0x0F
/// (permission fault, level 3), with ELR and FAR both naming the .data
/// address. A `-> !` signature here would be a lie that hides exactly
/// the bug the demo exists to catch.
pub fn demo_noexec() {
    let target = (&raw const DATA_CODE).expose_provenance();
    // SAFETY: with correct page tables the branch faults before executing
    // anything; with broken ones it executes a single `ret`. Either way
    // no memory is touched — x30 is the only state the call clobbers.
    unsafe {
        core::arch::asm!(
            "blr {addr}",
            addr = in(reg) target,
            out("x30") _,
            options(nostack),
        );
    }
    println!("  executed data as code and RETURNED — W^X is broken; the report");
    println!("  above this line should have been an instruction abort and was not.");
}

/// Read M1's home address — fatal by design: 0x4020_0000 is where this
/// kernel *loads*, and until `condemn_low_half` it was mapped through
/// TTBR0's identity trunk. Now TTBR0 holds the empty root, so the walk
/// dies at the first step: DFSC 0x04, translation fault, **level 0** —
/// the only demo that can produce a level-0 report, because every other
/// command lives in the half whose tree is populated.
pub fn demo_low() -> ! {
    // The integer is the point: no Rust object lives at this address
    // anymore — same shape as M1's demo_abort, an address conjured to
    // prove nothing answers there.
    let target = 0x4020_0000u64;
    // SAFETY: the load never completes — TTBR0 is the empty root and the
    // translation-fault handler parks.
    unsafe {
        core::arch::asm!(
            "ldr {scratch}, [{addr}]",
            addr = in(reg) target,
            scratch = out(reg) _,
            options(nostack),
        );
    }
    super::park()
}
