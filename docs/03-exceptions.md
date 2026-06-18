# 03 — Exceptions: the kernel catches its own faults

This is the story of milestone 1. By the end you will know what happens,
cycle by cycle, between a bad load and the register dump on your terminal
— and why the same machinery makes `brk` survivable but an unaligned load
fatal. Keep `src/arch/aarch64/vectors.rs` and `src/sync.rs` open next to
this.

## 1. The problem: a machine that dies without a word

In milestone 0, any CPU exception — a read from a hole in the memory map,
an undefined instruction, a misaligned access — simply stopped the
machine. No message, no register dump, nothing. The reason is brutally
simple: when AArch64 takes an exception it jumps to an address derived
from `VBAR_EL1`, the Vector Base Address Register, and we had never
written it. Its reset value is architecturally UNKNOWN, so the CPU jumped
through garbage, usually faulting again at the destination, forever.

This is the worst debugging experience in OS development, because the
machine destroys the evidence of what killed it. M1's whole job is the
upgrade from *silent* to *informative*: every fault now produces a report
— what faulted, where, and why — and the recoverable ones don't even
stop the kernel.

## 2. Sixteen entries, and the one column that matters

The architecture routes every exception through a table of **sixteen
entries of 0x80 bytes each** at `VBAR_EL1`. Two things about it surprise
people:

- The entries are **code, not pointers**. The CPU jumps *into* the slot;
  each handler gets 32 instructions of runway before it must branch away.
- `VBAR_EL1`'s low 11 bits are RES0, so the table must sit on a 2048-byte
  boundary. Our assembly requests that with `.balign 2048` and `linker.ld`
  ASSERTs the linker delivered — a misaligned table would be silently
  truncated to a wrong address, recreating exactly the wild jump M1
  abolishes.

Why sixteen? Four exception *kinds* times four *origins* (Arm's "AArch64
Exception Model" guide, document 102412, table 5-4):

| kind                              | current EL, SP_EL0 | current EL, SP_ELx | lower EL, AArch64 | lower EL, AArch32 |
|-----------------------------------|-----|-----|-----|-----|
| Synchronous (faults, `brk`, `svc`)| 0x000 | **0x200** | 0x400 | 0x600 |
| IRQ                               | 0x080 | **0x280** | 0x480 | 0x680 |
| FIQ                               | 0x100 | **0x300** | 0x500 | 0x700 |
| SError                            | 0x180 | **0x380** | 0x580 | 0x780 |

The kernel has run on SP_EL1 since boot (`SPSel=1` is the reset state;
the boot stub's `mov sp, x1` set that stack), so the kernel's own faults
arrive in the bolded **SP_ELx column at 0x200** — wiring up offset 0x000
instead and wondering why your handler never runs is a rite of passage we
can skip. The SP_EL0 column would mean we had switched stack selection
(we never do), and the lower-EL columns stay empty until M5 builds a
userspace. We populate all sixteen anyway: an "impossible" exception that
prints a report is a bug found; an impossible exception that hangs is a
bug lost.

## 3. What the hardware does, and what it pointedly does not

When an exception fires, the CPU atomically:

1. saves the return address in `ELR_EL1` and the current PSTATE in
   `SPSR_EL1`;
2. records *why* in `ESR_EL1` (the syndrome) and — for faults with an
   address — *where* in `FAR_EL1`;
3. masks all asynchronous exceptions (sets PSTATE.DAIF);
4. selects SP_EL1 and jumps to `VBAR_EL1 + offset`.

What it does **not** do: save even one general-purpose register. That is
the handler's job, and it is why each vector slot is twenty-nine
instructions — most of them `stp` — before any Rust runs.

The sharpest edge in this list is that ELR/SPSR/ESR/FAR are *single
registers*, not a stack. The next exception — and synchronous exceptions
**cannot be masked** — overwrites all four. Hence two design rules in
`vectors.rs`: the assembly stub copies all four into the frame
immediately, and the dispatcher keeps a re-entry tripwire (the `oops`
flag): if a second fault arrives while a first is being reported, the
first's evidence is already gone, and we say exactly that and park rather
than loop.

The tripwire has one blind spot, and M1 owns it rather than hiding it:
the tripwire is Rust, and Rust runs only after the stub's whole save
sequence — eighteen `stp`s, every one of them storing *through SP*. If the
fault being handled is SP itself being unusable (misaligned — the very
EC 0x26 the decoder names — or pointing at unwritable memory), the
stub's first store re-faults, re-enters the same vector slot, and loops
with no output at all: the one fault M1 cannot turn into a report. The
standard cure is a stack the handler owns — run kernel threads on
SP_EL0 and reserve SP_EL1 as a known-good exception stack (the SPSel
split, M5's territory). Until then, a silent hang where a report was
promised has one prime suspect: SP.

## 4. The trap frame: one struct, two authors

The stub builds a 288-byte frame on the stack — x0–x30, the interrupted
SP, then ELR, SPSR, ESR, FAR — and passes its address to Rust as the
first argument:

```text
sub  sp, sp, #288          // SP stays 16-aligned: 288 = 18 × 16
stp  x0,  x1,  [sp, #0]    // == TrapFrame.x[0], x[1]
...
stp  x30, x0,  [sp, #240]  // x[30], then sp (reconstructed: old SP = SP + 288)
mrs/stp ELR, SPSR          // [sp, #256]: elr, spsr
mrs/stp ESR, FAR           // [sp, #272]: esr, far
mov  x0, sp                // arg 0: &mut TrapFrame
mov  x1, #kind             // arg 1: sync/IRQ/FIQ/SError  (0–3)
mov  x2, #source           // arg 2: which origin column   (0–3)
bl   exception_dispatch
```

The same 288 bytes are `struct TrapFrame` on the Rust side, and the two
descriptions must agree byte for byte. They are kept honest by
`#[repr(C)]` plus a row of `const` asserts (`size_of` == 288,
`offset_of!(TrapFrame, elr)` == 256, and so on): move a field and the
kernel refuses to *build* — much better than a register dump that lies.

Three details worth noticing:

- **Why save all 31 registers** when the C ABI says a callee may only
  clobber x0–x18? Because a fault report must show the interrupted code's
  registers — all of them, untouched — and because a frame the whole
  kernel can trust is the foundation M6's context switch gets built on.
  What we *don't* save: FP/SIMD registers. We compile softfloat
  (`.cargo/config.toml`), so kernel code never touches them — that's 512
  bytes per exception we simply don't owe until M5 lets userspace use
  floating point.
- **The `.org` trick.** Each table entry is pinned to its offset with
  `.org 0x080`, `.org 0x100`, ... Because `.org` may only move the
  location counter *forward*, an entry that outgrows its 0x80-byte slot
  fails the assembly loudly instead of silently shifting its fifteen
  neighbors. The slot budget is 32 instructions; ours use 29.
- **Returning is `eret`**, which restores PSTATE from `SPSR_EL1` and
  jumps to `ELR_EL1` in one atomic step. The epilogue writes the frame's
  (possibly *edited* — next section) elr/spsr back into the system
  registers, reloads x0–x30, pops the frame, and `eret`s.

## 5. Reading the syndrome: ESR and FAR

`ESR_EL1` packs the cause into bit fields: **EC** (bits 31:26) is the
exception class, and **ISS** (bits 24:0) is per-class detail. The
dispatcher decodes the classes this kernel can actually produce — data
and instruction aborts, alignment faults, `svc`, `brk`, undefined
instructions — and prints anything else as raw hex with a pointer to the
Arm manual (DDI 0601). The full EC table is fifty rows; transcribing it
would be noise.

For aborts, ISS bits [5:0] hold the fault status code (DFSC for data,
IFSC for instruction fetches), which is where the report's `status` line
comes from. When M1 shipped, the monitor could demonstrate exactly two:

- **0x10, synchronous external abort** — the bus itself rejected the
  access; nothing lives at that address. (QEMU's virt board has faulted
  bad addresses like real hardware since machine type virt-2.11.) Still
  demonstrable — but since M2 it takes deliberate staging: the walk
  rejects unmapped addresses before they ever reach the bus, so the
  `abort` command now goes through a mapped-but-unbacked Device window
  built for the purpose (docs/04 §2).
- **0x21, alignment fault** — with the MMU off, every address was
  Device-nGnRnE memory, and Device memory forbids unaligned access. This
  surprises everyone once — and M2 delivered the predicted second
  surprise: the same load is perfectly legal on Normal memory now that
  the MMU is on, and the `unaligned` command demonstrates the *survival*
  where it used to demonstrate the death. A 0x21 today means a genuinely
  misaligned MMIO access.

M2 grew the demonstrable set by a family: translation faults at level 3
(`guard`) and level 0 (`low`), and permission faults for data (`wx`) and
fetches (`noexec`) — and for the page-table families the decoder now
prints the failing walk level on its own `level` line, under `status`.
docs/04 §10 scripts all five.

`FAR_EL1` holds the faulting *address* for aborts and alignment faults.
It is not valid for every class — for external aborts the CPU may decline
to report it (ESR's FnV bit, which the decoder checks) — and it is
meaningless for `svc`/`brk`, which is why the report annotates the line
"aborts/alignment only".

## 6. `brk` versus `svc`: a tale of two return addresses

The monitor's two survivable commands exist to teach one subtle rule, the
**preferred exception return address**:

- For **`svc`** (a deliberate call *into* the kernel), `ELR_EL1` points
  at the instruction *after* the `svc` — like a function return address.
  The handler changes nothing and `eret` resumes cleanly. M5 turns this
  path into the real system-call interface.
- For **`brk`** (and faults generally), `ELR_EL1` points **at the
  faulting instruction itself**. That is what you want for a page fault —
  fix the page tables, retry the same load — but for a breakpoint it
  means `eret` would re-execute the `brk` and bounce straight back,
  forever. The handler must *edit the frame*: `frame.elr += 4` — and +4
  means exactly "one instruction" because AArch64 instructions are
  fixed-width, four bytes every one — skip the breakpoint, resume after
  it.

That frame edit is the entire recovery mechanism, and it is why the
dispatcher takes `&mut TrapFrame`: handlers repair the world by mutating
the saved copy of it, and the epilogue makes the mutation real.

The fatal commands complete the lesson from the other side: a failed
access has nothing to retry and no state to repair, so the honest
maximum is a full report and `wfe`. No fake recovery that would resume
Rust code mid-broken-expression. (M1 wrote here that "M2 — page faults
we can service — raises this ceiling"; M2 arrived and the honest
accounting is narrower: it made faults *preventable* — the guard, W^X —
and the reports richer, but services none. Its own `unaligned` command
even defected from the fatal list to the survivors. The first fault
with a real fix is M5's, where the answer becomes "kill the offending
task, not the kernel".)

## 7. The console lock, and the deadlock we refused to ship

M0's console had no lock, with a written promise that M1 would pay the
debt the moment a second execution context could print. That moment is
now: a fault handler can interrupt `kmain` *mid-`println!`*.

Where the lock sits matters as much as what it is: it wraps each
`print!` *call* (`console_print` in main.rs), not the driver underneath.
A lock inside `Pl011` itself would also wrap the monitor's blocking
`read_byte` loop — and a context that holds the console lock while
waiting indefinitely for a keystroke is the classic way to deadlock a
console. hal/pl011.rs tells the same story from the driver's side.

The lock itself (`src/sync.rs`) is the smallest real spinlock: one
`AtomicBool`, compare-exchange/`Acquire` to take it, `Release` store to
free it, and — crucially — IRQ/FIQ masked while held. On one core, a
handler that interrupts the lock holder and then waits for the same lock
waits forever; masking interrupts around the critical section makes that
impossible *for asynchronous exceptions*. It takes effect for real in M3
when interrupts unmask; building it now means M3 cannot forget.

But synchronous faults don't honor masks — code holding the lock can
*fault*, and the fault report must not queue politely on a lock whose
owner is frozen beneath it. So the print path keeps an escape hatch: an
`OOPS` flag (named for Linux's `oops_in_progress`, set by its
`bust_spinlocks()`) that the dispatcher and the panic handler flip before
printing. While it's set, `console_print` skips the lock and writes
directly to a freshly conjured zero-sized UART handle. Possibly-garbled
output beats guaranteed silence — the same trade Linux's serial drivers
make during an oops. Recoverable faults flip the flag back on exit;
panics never return, so they don't bother.

One honest footnote lived in `sync.rs`, and has since been retired on
schedule: with the MMU and caches off, real AArch64 hardware does not
guarantee the exclusive-access instructions inside `compare_exchange`
make progress, so from M1 to M2 the lock was sound only because QEMU's
TCG doesn't model the restriction — a labeled simulator-ism, like the
PL011's missing init. M2 mapped this lock's memory Normal Inner-Shareable
write-back, exactly as promised, and the guarantee is the architecture's
now, not the simulator's (sync.rs keeps the retired note as history).

## 8. Why FIQ gets a real seat at the table

QEMU's GIC routes nothing to FIQ, so a lazy M1 would fold the FIQ slots
into the IRQ handler and move on. We don't, because the real target
won't let us: on Apple Silicon the architectural timer and fast IPIs
arrive over **FIQ**, not IRQ (docs/02 §3, item 6). The dispatcher
therefore demuxes IRQ and FIQ into two separate entry points from day
one. Today both are report-and-park stubs — any IRQ or FIQ with
interrupts masked means broken hardware or broken code, which deserves a
report — but M3 fills in the IRQ side (GIC: acknowledge → dispatch → EOI)
and M7 fills in the FIQ side (Apple timers), each into a slot that
already exists. The structure costs one vector entry and one stub now;
retrofitting it during the Apple bring-up would cost the same work at the
least convenient time imaginable.

## 9. Try it

`cargo run`, then ask the kernel to hurt itself:

```
> brk

*** exception: synchronous, from current EL on SP_ELx ***
  cause   : BRK #0xf00d — a software breakpoint (EC 0x3c)
  esr     : 0x00000000f200f00d  (syndrome — decoded above)
  elr     : 0xffff000040207c68  (preferred return address)
  ...
  verdict : recovered — ELR pointed AT the brk (its preferred return
            address); we advanced it one instruction so eret resumes
            just past the breakpoint.

...and we're back — the kernel caught its own fault and lived.
>
```

(That `elr` starting `0xffff…` is M2's fingerprint — fault addresses
have been higher-half virtual since the kernel moved upstairs.)

Then `svc 7` (recovers with *no* ELR adjustment — section 6), and when
you're ready to say goodbye, any of the M2 fatal five (`guard`, `wx`,
`noexec`, `low`, `abort` — docs/04 §10 predicts each syndrome) for a
fatal report with the full register table and an honest verdict. `help`
reminds you of the menu; the prompt surviving a `brk` is milestone 1 in
one line of transcript.

## Sources

Arm, "Learn the architecture: AArch64 Exception Model" (102412): vector
table layout, exception entry/return, preferred return addresses, DAIF.
Arm Architecture Registers (DDI 0601): ESR_EL1/FAR_EL1/VBAR_EL1 field
encodings. AAPCS64 (ARM-software/abi-aa): caller/callee-saved register
split. QEMU source: hw/arm/virt.c (bad-address faulting since virt-2.11),
target/arm (BRK return-address semantics; MMU-off alignment checking
since 9.0). Linux: `oops_in_progress`/`bust_spinlocks()` and the serial
drivers' trylock-during-oops pattern, the model for our `OOPS` flag.

Next: [04-virtual-memory.md](04-virtual-memory.md) — M2 turns on the
MMU, the milestone these fault reports were built to survive (and they
did: they covered its enable cliff and now narrate its page-table
faults).
