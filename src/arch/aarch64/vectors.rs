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
    ldp   x0,  x1,  [sp, #0]      // the temporaries, last
    add   sp,  sp,  #288          // pop the frame; SP is exactly pre-fault
    eret                          // PSTATE := SPSR_EL1, PC := ELR_EL1 — atomically
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

/// Point VBAR_EL1 at the table. Called once, first thing in `kmain` —
/// before the banner, so even banner-era bugs get reports.
pub fn install() {
    // SAFETY: __vectors is the 2048-aligned table from this file's
    // global_asm! (VBAR_EL1 bits [10:0] are RES0; linker.ld ASSERTs the
    // alignment). Writing VBAR_EL1 at EL1 is always permitted. The `isb`
    // flushes the pipeline so the very next instruction already faults
    // through the new table, not the old UNKNOWN one.
    unsafe {
        core::arch::asm!(
            "adrp {t}, __vectors",            // page of the table (PC-relative —
            "add  {t}, {t}, :lo12:__vectors", //  same habit as boot.rs)
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
        println!("  new esr: {:#018x}  new elr: {:#018x}", frame.esr, frame.elr);
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
        // Every other slot belongs to a world that does not exist yet: we
        // never select SP_EL0, and nothing runs at a lower EL until M5.
        // Reaching one of these *is* the bug being reported.
        (kind, source) => {
            report(
                frame,
                kind,
                source,
                format_args!(
                    "an exception from a context that should not exist — nothing runs \
                     on SP_EL0 or at a lower EL until M5"
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
const EC_PC_ALIGN: u64 = 0x22; // PC alignment fault
const EC_DABT_SAME_EL: u64 = 0x25; // data abort, same EL
const EC_SP_ALIGN: u64 = 0x26; // SP alignment fault
const EC_BRK64: u64 = 0x3c; // brk from AArch64

/// Synchronous exceptions from the kernel itself — the interesting ones.
/// Two recover (brk, svc); the rest are fatal until the MMU milestone
/// gives us something to repair them *with*.
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
            println!("            eret resumes there unadjusted. M5 turns this report into a");
            println!("            real system-call dispatcher.");
            println!();
        }
        EC_DABT_SAME_EL => {
            // Data-abort ISS fields (DDI 0601): WnR = bit 6 (write, not
            // read), DFSC = bits [5:0], FnV = bit 10 (FAR not valid —
            // possible only for external aborts).
            let dfsc = iss & 0x3f;
            let wnr = if iss & (1 << 6) != 0 { "write to" } else { "read from" };
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
/// ISS) into words. We name the codes the machine can produce today plus
/// the page-table family we will meet in M2; the rest print raw upstream.
fn fault_status(fsc: u64) -> &'static str {
    match fsc {
        // 0b0001LL, 0b0010LL, 0b0011LL: LL = the table walk level 0-3.
        0x04..=0x07 => "translation fault — no page-table mapping (impossible today: MMU off)",
        0x08..=0x0b => "access-flag fault — page tables again; M2 territory",
        0x0c..=0x0f => "permission fault — page tables again; M2 territory",
        0x10 => "synchronous external abort — the bus rejected the access: \
                 nothing lives at this address",
        0x14..=0x17 => "synchronous external abort during a page-table walk",
        0x21 => "alignment fault — with the MMU off every access is Device memory, \
                 and Device memory forbids unaligned access",
        _ => "a fault status this kernel does not decode (DDI 0601, ESR_EL1 has the full list)",
    }
}

// ---------------------------------------------------------------------------
// IRQ and FIQ: two demux points, deliberately separate
// ---------------------------------------------------------------------------

/// The IRQ entry point. Today nothing can legitimately raise one — DAIF.I
/// has been masked since reset and no device interrupt is enabled — so an
/// IRQ here means broken hardware or broken code, and it is fatal. M3
/// (GIC + timer) replaces this body with acknowledge → dispatch → EOI and
/// *returns*, making IRQs the first exceptions that are routine instead
/// of news.
fn handle_irq(frame: &TrapFrame, kind: Kind, source: Source) -> ! {
    report(
        frame,
        kind,
        source,
        format_args!("IRQ — but interrupts are masked and no source is enabled; impossible today"),
    );
    die()
}

/// The FIQ entry point — kept separate from IRQ on purpose, forever. On
/// QEMU's GIC nothing routes to FIQ, so this looks like dead structure;
/// on Apple Silicon the architectural timer and fast IPIs arrive over FIQ
/// (docs/02 §3, item 6). Folding FIQ into IRQ today would be a refactor
/// M7 pays for at the worst time. Keeping the split costs one vector
/// entry and this stub.
fn handle_fiq(frame: &TrapFrame, kind: Kind, source: Source) -> ! {
    report(
        frame,
        kind,
        source,
        format_args!("FIQ — nothing routes to FIQ on QEMU; on Apple Silicon this is the timer (M7)"),
    );
    die()
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
    println!("  esr     : {:#018x}  (syndrome — decoded above)", frame.esr);
    println!("  elr     : {:#018x}  (preferred return address)", frame.elr);
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
        frame.x[30],
        frame.sp
    );
}

/// The standard fatal ending. With no MMU, no tasks, and no way to undo a
/// failed bus access, "report and park" is the honest maximum — M2 (page
/// faults we can service) and M5 (kill the offending task instead of the
/// kernel) raise the ceiling.
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

/// Load eight bytes from an odd address — fatal by design. With the MMU
/// off every access is Device-nGnRnE memory, and Device memory *requires*
/// natural alignment: the CPU raises a data abort (DFSC 0x21) instead of
/// performing the load. (QEMU note: TCG only enforces this rule since
/// QEMU 9.0 — on 8.x this load silently succeeds. Real hardware faults,
/// which is the behavior worth rehearsing.)
pub fn demo_unaligned() -> ! {
    // An address that is certainly readable RAM, made certainly
    // misaligned: one past the (16-aligned) start of our own stack frame.
    // `expose_provenance`, not `addr`: the asm below genuinely accesses
    // memory through this integer, so the pointer's provenance must be
    // exposed first — the same strict-provenance discipline as every MMIO
    // access in hal/pl011.rs. (`addr()` deliberately severs provenance,
    // which would disavow exactly the access we are about to make.)
    let buf = [0u64; 2];
    let misaligned = (&raw const buf).expose_provenance() + 1;
    // SAFETY: the load never completes — the alignment fault is taken
    // first, and its handler parks. Done in assembly because Rust's read
    // methods are allowed to assume aligned pointers: we want the CPU's
    // opinion of the misalignment, not language-level UB — and thanks to
    // the exposed provenance above, an access through this address is one
    // the language permits us to attempt.
    unsafe {
        core::arch::asm!(
            "ldr {scratch}, [{addr}]",
            addr = in(reg) misaligned,
            scratch = out(reg) _,
            options(nostack),
        );
    }
    // Unreachable in practice; the signature wants a `!` and honesty
    // prefers a backstop to a lie.
    super::park()
}

/// Read from `addr`, which the caller promises is a hole in the physical
/// memory map — fatal by design. Since machine type virt-2.11, QEMU
/// faults accesses to unbacked addresses like real hardware does: the bus
/// transaction fails and comes back as a synchronous external abort
/// (DFSC 0x10). There is nothing to retry and nobody to retry it for.
pub fn demo_abort(addr: u64) -> ! {
    // SAFETY: same shape as demo_unaligned — the load never completes;
    // the external-abort handler reports and parks.
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
