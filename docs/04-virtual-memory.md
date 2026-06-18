# 04 — Virtual memory: the kernel maps its own world

This is the complete story of milestone 2. By the end you will know what
every descriptor bit does, why the machine did not vanish when we flipped
`SCTLR_EL1.M`, and what it means that the kernel now lives at an address
that did not exist while it was being loaded. Keep
`src/arch/aarch64/mmu.rs`, `boot.rs`, and `linker.ld` open next to this.

## 1. The flat world and its lies

For two milestones every address was a physical address, and the
simplicity was paid for in strange rules. With the MMU off, AArch64
treats every access as **Device-nGnRnE** — the strictest memory type, the
one meant for hardware registers — so an unaligned load of plain RAM was
a *fatal fault* (M1's `unaligned` demo died demonstrating it). Caches
stayed off, so every instruction fetch went to DRAM. Nothing was
read-only: a wild store could rewrite `.text` as easily as a buffer. And
the stack could overflow straight into `.bss`, silently, because no
boundary existed anywhere that hardware would enforce.

The MMU replaces that flat world with a *described* one. Every access is
looked up in tables the kernel writes — and the description says where
each address really goes, what kind of memory answers, and what you are
allowed to do to it. The catch: the lookup machinery has to be programmed
with the machine running, and a mistake doesn't fail politely — it takes
the instruction-fetch path down with it. That cliff is section 6.

## 2. The map we want

One rule generates the whole layout: **high virtual address = physical
address + `KERNEL_BASE`** (`0xFFFF_0000_0000_0000`). Linux calls this a
linear map; we get the kernel image, the DTB, the UART, and all of RAM
through one offset, and `phys_to_virt`/`virt_to_phys` are one addition:

```
VA (= 0xFFFF_0000_0000_0000 + PA)        PA           what                attrs
0xFFFF_0000_0900_0000                    0x0900_0000  UART (one page)     Device-nGnRnE, RW, XN
0xFFFF_0000_4000_0000                    0x4000_0000  DTB window (2 MiB)  Normal, read-only
0xFFFF_0000_4020_0000                    0x4020_0000  .text.boot + .text  Normal, read+EXECUTE
                                                      .rodata             Normal, read-only
                                                      .data, .bss         Normal, read+write
                                                      —— guard, 16 KiB —— UNMAPPED
                                                      boot stack, 64 KiB  Normal, read+write
                                                      rest of the 32 MiB  Normal, read+write
0xFFFF_0000_4200_0000                    0x4200_0000  RAM (15×32 MiB)     Normal, RW (blocks)
0xFFFF_0000_6000_0000                    0x6000_0000  bus-error window    Device, RW — UNBACKED
```

Everything else — and after section 8, the entire low half — is
unmapped: touching it earns a translation fault with a full M1-style
report. Three deliberate oddities, each a lesson:

- **The guard** is not a mapping, it is a *hole*: 16 KiB below the stack
  whose descriptor is never written. Stack overflow's first touch now
  faults loudly instead of eating `.bss` (the debt docs/01 §3 has carried
  since M0).
- **The bus-error window** is the opposite trick — a mapping to nowhere.
  With everything-unmapped-faults-in-the-walk, the *bus's* own failure
  mode (M1's `abort` demo) would become undemonstrable; so the 32 MiB
  past RAM is mapped Device on purpose, and a read there sails through
  translation and dies on the bus (DFSC 0x10). Page-table "no" versus
  bus "no", one monitor command each.
- **The kernel moves upstairs** instead of staying identity-mapped where
  it loaded. The ground floor (TTBR0's low half) is being saved for M5's
  userspace — and a kernel that *shares* the bottom of the address space
  with user pointers is one missing check away from confusing them.

## 3. The walk as a story

A 16 KiB granule means a 14-bit page offset, and 16 KiB of 8-byte
descriptors per table means **11 bits of index per level**:

```
VA bit:   47 | 46........36 | 35........25 | 24........14 | 13........0
          L0 |      L1      |      L2      |      L3      |   offset
```

The walk starts at the table `TTBR0_EL1` or `TTBR1_EL1` names — bits
[63:48] of the VA choose which (all-zeros: TTBR0; all-ones: TTBR1;
anything else faults instantly) — and resolves 11 bits per level until it
hits a leaf. Two shapes surprise people coming from 4K-granule habits:

- **L0 has two entries.** 48 bits of VA minus 33 bits resolved by
  L1+L2+L3 and 14 of offset leaves *one* bit for level 0 — a 16-byte
  table at the root of 256 TiB. (We give it a whole zeroed granule
  anyway; .bss is free and alignment questions die forever.)
- **Blocks exist only at L2.** A "block" is a leaf above L3 — at 16K,
  an L2 block maps 32 MiB. The 4K reflex says "and a 1 GiB block at L1" —
  **no**: without FEAT_LPA2 the 16K granule has *no L1 blocks at all*.
  QEMU's `-cpu max` implements LPA2 and would forgive `TCR_EL1.DS=1`
  games; Apple M1 hardware does not have LPA2. `DS` stays 0 forever and
  the rule stays absolute. This is the VM-first strategy working as
  designed: the simulator offers a convenience, the real target's limits
  veto it, and the veto is recorded here instead of discovered in M7.

Why 16 KiB at all? Apple's DART IOMMUs speak 16K, effectively mandating
it kernel-wide (docs/02 §3 item 7) — and `-cpu max` is in
`.cargo/config.toml` precisely because it implements TGran16. M2 cashes
that promise at boot: the table builder reads `ID_AA64MMFR0_EL1` and
refuses to continue on a CPU without the granule (raw `MMU?G` on the
UART, then park), rather than programming a format the walker would
ignore.

## 4. Descriptors, bit by bit

Every entry is a u64. Bits [1:0] are the type — and the encoding is
level-dependent in a way that has hurt people:

| bits [1:0] | at L0..L2          | at L3              |
|------------|--------------------|--------------------|
| `x0`       | invalid            | invalid            |
| `01`       | **block** (leaf) — at 16K/DS=0, *L2 only*; reserved-invalid at L0 and L1 | *reserved* = invalid |
| `11`       | table (descend)    | **page** (leaf)    |

A block and a page are both leaves with *different* type bits, and
copying an L2 block pattern into an L3 slot yields not a mapping but a
hole. `mmu.rs` therefore has two constructors that cannot be mixed up —
and no hand-written descriptor literals anywhere.

A leaf's remaining bits, the ones M2 uses (full table: DDI 0487 D8):

- **AttrIndx [4:2]** — an index into `MAIR_EL1`, which holds eight
  8-bit memory-type recipes. We define two: index 0 = `0xFF`, Normal
  write-back read/write-allocate; index 1 = `0x00`, Device-nGnRnE. Not
  nGnRE: Apple SoCs answer *posted* (early-acknowledged) MMIO writes
  with an SError — it is why Linux grew `ioremap_np()` during the M1
  bring-up — so we rehearse the strict variant QEMU can't distinguish.
- **AP [7:6]** — access permissions: `00` = kernel read-write, `10` =
  kernel read-only (the odd encodings open EL0, which doesn't exist
  until M5).
- **SH [9:8]** — shareability: `11` (Inner) for Normal memory, the
  posture M6's second core will inherit; ignored for Device.
- **AF [10]** — the access flag, and the trap of the chapter: with
  hardware AF-updates absent (`TCR.HA=0` — Apple M1 lacks FEAT_HAFDBS,
  so we don't lean on QEMU having it), a leaf with AF=0 **faults on
  first touch**, in a way that reads exactly like "the MMU didn't work".
  Our constructors bake AF=1 in unconditionally; an AF=0 leaf cannot be
  built, and `fault_status` says whom to suspect if one appears anyway.
- **PXN [53], UXN [54]** — privileged/unprivileged execute-never. Only
  `.text` clears PXN; *everything* sets UXN until M5; Device memory sets
  both, because a speculative instruction fetch from MMIO can wedge real
  hardware without so much as a fault (TCG never speculates — another
  labeled simulator-ism).

> **A trap we hit so you don't have to:** `TCR_EL1` configures each
> half's granule separately — `TG0` for TTBR0, `TG1` for TTBR1 — and the
> two fields use **different encodings for the same granule**: 16K is
> `0b10` in TG0 but `0b01` in TG1 (where `0b10` means 4K). During M2's
> design this was demonstrated live: a kernel with TG1 wrong boots,
> prints, and runs perfectly — right up to the first higher-half access,
> which may be minutes of debugging later. `mmu.rs` const-asserts each
> field with a message that quotes the *other* field's scale, and the
> boot stub's canary (section 6) converts any surviving mistake into an
> immediate, located failure.

## 5. Rust below its link address

Here is the puzzle the boot flow has to solve. The kernel proper is
linked to run at `0xFFFF_0000_4020_0800`-and-up but *loaded* at
`0x4020_0800`; the CPU starts at the raw ELF entry, `0x4020_0000` — the
boot stub, the one section linked where it loads — with the MMU off.
Until the tables exist and the MMU is on, the high addresses *do not
resolve* — yet somebody has to build those tables, and we would rather
it be readable Rust than three pages of assembly.

It works because of how this target compiles code. With
`relocation-model=static` on AArch64, **code** reaches everything
PC-relatively: `adrp`+`add` for statics, `bl` for calls — displacement
arithmetic that is *correct at any load address*. Run a high-linked
function at a low PC and `&raw const SYMBOL` quietly yields the symbol's
**physical** address — exactly the currency a page-table builder spends.
What breaks is **data**: absolute 64-bit addresses (`R_AARCH64_ABS64`
relocations) baked into `.rodata`/`.data` at link time. The format
machinery's vtables, `panic::Location` strings, function pointers,
statics holding references — all hold high VMAs that mean nothing until
translation is on.

So the two functions the boot stub calls early (`opal_build_tables`,
`opal_mmu_enable`) operate under a written **LOW WORLD contract**
(mmu.rs): no `println!`, no trait objects or function pointers, no panic
may *fire*, raw-pointer writes instead of indexing, and the zero-static
PL011 as the only debug channel. Honesty requires saying it plainly:
stable Rust with zero crates cannot make these rules a compile error.
They are enforced by convention, by the staged bring-up (the builder was
debugged in an identity-linked rehearsal world where `println!` still
worked), and by tripwires — the post-link audit below, and linker.ld's
`SIZEOF(.text.boot)` ASSERT, which exists because a stray `bl` from the
low boot stub to a high symbol makes lld synthesize a silent
`__AArch64AbsLongThunk_*` veneer that jumps to an untranslatable address.

The post-link audit (run it after any boot-path change):

```sh
# rustup component llvm-tools provides these (path spelled out, not
# globbed — zsh does not expand globs inside parameter expansions):
TOOLS=$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/host: //p')/bin
$TOOLS/llvm-readobj --file-header kernel.elf | grep Entry   # must be 0x40200000
$TOOLS/llvm-nm kernel.elf | grep -i thunk                   # must be empty
```

The boot stub itself (boot.rs — read its step comments alongside this)
crosses the divide with **literal pools**: `ldr x9, =symbol` reads a full
64-bit address the assembler parked in `.text.boot`, and subtracting
`KERNEL_BASE` turns the virtual name into a physical one. `adrp` cannot
do this job — its reach is ±4 GiB, and 0xFFFF… is rather farther than
that from 0x4020… The stub even cross-checks its hardcoded KERNEL_BASE
against the linker's `__kernel_va_base` and refuses to boot if the two
ever drift.

## 6. The cliff

The roadmap called the first MMU enable "works or hangs with zero
feedback," and the ceremony in `opal_mmu_enable` is shaped entirely by
that cliff. It is one `asm!` block — nothing may wander between its
lines — and every line is annotated in mmu.rs; the short version:

1. **Make the MMU-off world's writes unshadowable.** The walker is a
   second observer with its own path to memory — and post-enable code is
   a third: the first *cached* read of anything written with the caches
   off must not hit a stale leftover line. `dc ivac` over everything the
   MMU-off world wrote and the MMU-on world reads back — all of `.bss`
   (the tables live there) plus the boot stack — discards firmware-era
   lines. *Invalidate*, not clean-and-invalidate: a stale **dirty** line
   CIVAC'd would write its stale bytes back **over** the live stores.
   (Labeled simulator-ism: every cache op is a NOP on TCG. QEMU passing
   this loop is zero evidence; the code is reviewed against Linux's
   `dcache_inval_poc` and m1n1, and its first real test is M7.)
2. **Program MAIR, TCR, TTBR0, TTBR1.** Order among the four is free —
   they're direct writes, published to the translation machinery only at
   the next context-synchronization event. TCR's IPS field is clamped at
   runtime from `ID_AA64MMFR0_EL1.PARange` (an *irregular* encoding —
   `0b011` is 42 bits, not 44; compare encodings, never bit counts).
   QEMU max advertises 52 bits where M1 implements 36; with `DS=0` the
   output caps at 48 regardless, and the spec ends its oversized-IPS
   forgiveness with "software must not rely on this".
3. **`tlbi vmalle1`** — TLB contents out of reset are UNKNOWN, and
   translation must not begin against leftovers. Bracketed by `dsb`
   (a TLBI is only *complete* after one) and an `isb` that is
   **load-bearing, not folklore**: without it, the fetch path may
   observe M=1 while still holding stale TTBR/TCR values — and refill
   the TLB from garbage *after* our flush. ATF, m1n1, and Linux all
   carry this ISB.
4. **`ic iallu`** — cold-boot I-cache state is also UNKNOWN, and I-fetch
   may hit it the moment `SCTLR.I` goes high. Self-reliance over faith
   in loaders.
5. **`msr SCTLR_EL1` — M, C, I in one write, from scratch.** Not
   read-modify-write: entered from m1n1 at EL2, SCTLR_EL1 is
   architecturally UNKNOWN, so there is nothing trustworthy to modify
   (Linux ships the same answer as `INIT_SCTLR_EL1_MMU_ON`). The bits
   deliberately left clear teach as much as the bits set — `A` stays 0
   so unaligned Normal-memory loads are *legal* (the monitor's
   `unaligned` command is M1's killer turned no-op), and `WXN` stays 0
   because the per-leaf PXN/UXN bits are the single source of truth.

   The supporting cast in `0x30D5_199D`, bit by bit (the constant's doc
   in mmu.rs defers to this list): `SA`/`SA0` [3]/[4] — SP-alignment
   checking, so a misaligned stack pointer faults (EC 0x26) instead of
   corrupting. `ITD`/`SED` [7]/[8] — AArch32-at-EL0 instruction
   restrictions; on an AArch64-only implementation (M1, and virt under
   `-cpu max` for EL0) they are "Reserved, RES1", so writing them 1 is
   the honest spelling of "no AArch32 here". `EOS`/`EIS` [11]/[22] —
   exception entry and return are context-synchronizing; v8.5 lets an
   implementation relax that, and we decline the relaxation. `nTWI`/
   `nTWE` [16]/[18] — EL0's WFI/WFE not trapped (no EL0 until M5; the
   no-surprises default). `TSCXT` [20] — trap EL0 access to SCXTNUM_EL0
   (same story). `SPAN` [23] — leave PSTATE.PAN alone on exception
   entry; PAN itself becomes interesting when M5 gives "privileged
   access to user memory" a meaning. `nTLSMD`/`LSMAOE` [28]/[29] —
   AArch32 load/store-multiple-to-Device behaviors, RES1 without
   AArch32 at EL0, written 1 for the same reason as ITD/SED.
6. **`isb`** — and the very next fetch, at the same low PC, is
   *translated*. Which only works because…

…both TTBRs point at **the same tree**. `KERNEL_BASE`'s low 48 bits are
zero, so a high VA and its PA share bits [47:0] — and the walk only ever
sees bits [47:0]. One tree therefore answers identically for
`0x4020_0800` (via TTBR0) and `0xFFFF_0000_4020_0800` (via TTBR1): same
descriptors, same attributes, aliasing rules satisfied by construction.
The kernel keeps executing low through the identity trunk, proves the
high trunk works (the canary in boot.rs step 9: first an `AT S1E1R`
probe — "does this even translate?", answered in PAR_EL1 as a value, the
shape a TG1-class mistake actually produces — then the first deliberate
TTBR1 load, checked against an immediate; either failure is a raw `?` on
the UART and a park), re-points VBAR high, rebases SP, and only then
takes the absolute branch upstairs.

When the cliff *does* claim a boot — it will — the ropes, in order: the
canary localizes TG1-class failures to the enable site (and probes with
AT precisely because a raw fault there could not report — the reporter's
format data is high and unreachable under a broken TTBR1); VBAR is
pre-armed (low before the enable, high after the canary) so faults past
the enabling ISB get M1's full reporter; and QEMU will narrate what the
CPU saw: insert `-D /tmp/qemu.log -d int` into the runner string in
`.cargo/config.toml` *before* its trailing `-kernel` (cargo appends the
ELF path to the end, so the runner must keep ending with `-kernel`),
then read the **first** "Taking exception" block in the log — the first,
because a vector-table fault loops and the log will happily bury the
cause under eleven million identical descendants.

## 7. The move upstairs

The jump itself is three instructions and one idea: PC-relative reach
cannot get there (±4 GiB), so the stub loads `_start_rust`'s full high
address from its literal pool and `br`s through the register. Before it:
SP is rewritten to the same stack's high alias (same physical memory,
new name) and `x29` is zeroed — the frame chain *ends* at the world
boundary, because a low frame pointer would be a dangling lie the moment
the low half stops translating. After it: every PC-relative habit that
served the low world now serves the high one — `vectors::install()`'s
`adrp` executed at a high PC yields the high table address, which is why
kmain can re-derive and re-affirm what the stub pre-armed.

## 8. Condemning the ground floor

The shared tree is transition scaffolding, and kmain's first real act is
to take it down: `condemn_low_half()` points TTBR0 at the empty root
(`TABLES.empty_root` in mmu.rs — a 16 KiB table of zeros that is never
written) and flushes the TLB. From
that line, `0x4020_0000` does not translate; a NULL dereference faults at
level 0; the monitor's `low` command produces the only level-0 report in
the kernel; and the banner's AT-probe line *verifies* the condemnation
rather than asserting it (`AT S1E1R` + `PAR_EL1`: ask the hardware to
translate, read its refusal, no fault involved — the same oracle behind
the `translate` command).

This is also, quietly, a rehearsal: replacing TTBR0's root and flushing
is *exactly* the shape of M5's per-task address-space switch. And note
what it is **not**: we never modify a live mapping — tables were built
with the MMU off, and the one change after enable is a whole-root swap.
The rule we therefore never needed is **break-before-make**: to change a
live translation you must write an invalid descriptor, TLBI, and only
then write the new one — skip it and two TLB entries for one VA can
coexist, with UNPREDICTABLE results QEMU is perfectly capable of
exhibiting too. M5, which edits live tables, inherits that rule; M2
just wrote it down.

> **Sidebar: one tree or three?** Linux at this moment juggles three
> table trees: a throwaway identity map (`init_idmap_pg_dir`) for the
> transition, the real kernel map (`swapper_pg_dir`), and an all-empty
> `reserved_pg_dir` that TTBR0 parks on whenever no user task is mapped
> — and Opal's `empty_root` is that last idea with the serial numbers
> filed back on. m1n1, meanwhile, uses the shared-tree trick (its
> `memory.c` keeps VA==PA aliases in one tree) and *never* condemns,
> because a bootloader's job is to hand the low world over intact. Opal
> uses m1n1's trick and Linux's ending: shared tree for the climb,
> reserved root forever after. Same problem, three honest answers.

## 9. What got safer, and what got stranger

Paid in full, with receipts in the monitor:

- **The guard** (`guard`): overflow faults at level 3 instead of eating
  `.bss` — docs/01's oldest outstanding debt.
- **W^X** (`wx`, `noexec`): `.text` cannot be written, data cannot be
  executed. The `noexec` demo is *falsifiable* — it branches at a real
  `ret` parked in `.data`, so if PXN ever stops working the demo
  returns and says so, instead of a `-> !` signature hiding the bug.
- **The exclusives footnote** (sync.rs): the spinlock's LDXR/STXR now
  run on cacheable memory and their progress is architecture, not
  simulator generosity.
- **Unaligned access** (`unaligned`): not a fault anymore — the same
  load that killed M1 returns its bytes.

Stranger, and worth internalizing:

- **External aborts got demoted.** Before M2, *every* bad address was a
  bus fault. Now the walk rejects almost everything first; only the
  deliberate bus-error window still reaches the bus to die (DFSC 0x10
  versus 0x04–0x07 — `abort` versus `low`, run both).
- **Addresses in reports are virtual now.** `elr`, `far`, `sp` all read
  `0xffff_0000_…` — when cross-referencing a fault against
  `llvm-objdump`, the disassembly speaks VMA natively; subtract
  `KERNEL_BASE` only when you need the load offset.
- **The DTB is read-only.** `fdt_at` reads it through the higher-half
  alias, and corrupting the boot evidence now takes deliberate effort.

What M2 deliberately did **not** do, so nobody mistakes the milestone
for more than it is: no fault is *serviced* — every report still ends in
recovery-or-park, and the first fault with a fix (kill the task, not the
kernel) is M5's. No demand paging, no ASIDs (every TLBI is the broad
kind until M5 wants finer), no hardware AF/DB management, and the cache
maintenance ships reviewed-but-unproven until real silicon in M7.

## 10. Try it

`cargo run`, and predict each syndrome before you hit Enter:

```
> unaligned                      ← M1's killer: now returns 0x0011223344556677
> translate 0xffff000040200000   ← Ok: PA 0x40200000
> translate 0x40200000           ← FST 0x04: nothing mapped (condemned)
> walk 0xffff000009000000        ← L0→L1→L2→L3, ends in a Device page; AT agrees
> walk 0xffff000043001234        ← same, but ends in a 32 MiB block at L2
> guard                          ← FATAL: DFSC 0x07, level 3, WnR=1, FAR in the guard
```

…then reboot and try `wx` (DFSC 0x0F), `noexec` (IFSC 0x0F, ELR == FAR
in `.data`), `low` (DFSC 0x04, **level 0**), and `abort` (DFSC 0x10).
Five fatal commands, five different syndromes, every one predicted by
the map in section 2.

## Sources

Arm ARM (DDI 0487) D8: translation regimes, 16K-granule geometry, block
levels, descriptor formats, mismatched-attribute aliasing; D7: barriers
and context synchronization. Arm Architecture Registers (DDI 0601):
TCR/SCTLR/MAIR/TTBRn/PAR/ID_AA64MMFR0 fields. Arm 100940 (barrier
litmus): the enable-sequence skeleton (TLBI flavor adjusted for EL1 —
the whitepaper's `ALLE1` is UNDEFINED there). Linux arm64: `head.S`,
`proc.S` (`__cpu_setup`, `INIT_SCTLR_EL1_MMU_ON`), `cache.S`
(`dcache_inval_poc`), `mmu_context.h` (`cpu_uninstall_idmap`,
`reserved_pg_dir`). m1n1 `src/memory.c` (16K/48-bit shared-tree setup on
real M1). Asahi Linux progress reports (16K rationale, nGnRnE vs nGnRE
and `ioremap_np`). QEMU: `target/arm/ptw.c` (the walker),
`hw/arm/boot.c` + `elf_ops.h.inc` (ELF loading at p_paddr, raw e_entry).

Next: [ROADMAP.md](../ROADMAP.md) — M3 gives this kernel a heartbeat:
the timer, the GIC, and the first interrupt that is routine instead of
news.
