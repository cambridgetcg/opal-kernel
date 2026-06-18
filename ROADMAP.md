# Opal roadmap

The ladder from "prints a banner" to "runs programs on a MacBook." Each
milestone is small enough to hold in your head and leaves the kernel in a
demonstrably working state. Difficulty ratings are honest; the dragons are
labeled.

## M0 — boots and speaks ✅

Boot stub (park secondaries, stack, zero `.bss`), linker script, polled
PL011 console, `println!`, banner (exception level, `x0`, devicetree
sniffing), interactive echo loop, printing panic handler. QEMU virt only.

*What it deliberately lacked:* any way to survive a CPU fault, any memory
protection, any interrupt. The first two of those debts have since been
paid — faults one rung down (M1), memory protection two (M2); interrupts
remain M3's.

## M1 — exceptions and vectors ✅

A full EL1 vector table (`VBAR_EL1`): the 16 entries, register-save frames,
and a readable "what faulted, where, and why" report instead of M0's
silent hang — turning the worst debugging experience in OS development into
a mediocre one, which is a huge upgrade. Includes `ESR/FAR` decoding and a
deliberate test fault. **FIQ handled as a first-class citizen, not an
afterthought** — on Apple Silicon the timer arrives over FIQ, so the demux
structure is built now. The console grows its promised spinlock here, the
moment a second execution context (a handler) can print.

Difficulty: moderate. Mostly bookkeeping, but the bookkeeping must be
exactly right, and bugs in it corrupt the evidence of themselves.

## M2 — MMU with the 16 KiB granule ✅ (this milestone)

Page tables (built by the kernel, in readable Rust), the kernel
identity-mapped for the climb and then moved to a **higher-half** home at
`0xFFFF_0000_0000_0000 + PA`, MMIO mapped as Device-nGnRnE,
`SCTLR_EL1.{M,C,I}` on — without the machine vanishing, thanks to a
staged bring-up, a boot-stub canary, and W^X per-section permissions
proven by five new fatal monitor commands. **16 KiB granule from day
one** — Apple's DART IOMMUs are 16K-only, and `-cpu max` was chosen in M0
exactly so QEMU could rehearse this (the builder now verifies TGran16 in
`ID_AA64MMFR0_EL1` at boot). The guard page under the boot stack pays off
M0's known debt; M1's `unaligned` killer becomes a survivor the moment
RAM turns into Normal memory. The full story is docs/04-virtual-memory.md.

Difficulty: was indeed a **spike** — the "works or hangs with zero
feedback" cliff is real, and M1's fault reporting plus the rehearse-
under-identity staging are what made it climbable.

*What it deliberately lacked:* no fault is *serviced* — reports got
richer, several faults became preventable, but recovery still means
"brk/svc only" until M5. No demand paging; no ASIDs (TLB flushes are the
broad kind until M5 wants finer). Break-before-make is documented, never
exercised — M5 edits live tables and inherits it. The D-cache/I-cache
maintenance in the enable path is reviewed against Linux/m1n1 but
unprovable on TCG (every cache op is a NOP there); its first real test
is M7's silicon. `SCTLR.WXN` deferred to M5's hardening pass.

## M3 — time and interrupts

The architectural timer (`CNTP_*`), the GICv3 on virt: enable, route,
acknowledge, EOI. A timer tick, interrupt-driven UART RX replacing the
monitor's polled read loop. Design note attached to every line of GIC code: *the GIC
is virt-only* — Apple uses AIC (and timers-as-FIQ), so the interface
between "an interrupt arrived" and "the kernel reacts" stays
controller-agnostic, sized for exactly two implementors.

Difficulty: moderate; the GICv3 has more registers than ideas.

## M4 — devicetree, for real

A minimal in-tree FDT parser (header, memreserve, structure block, strings
— it's a simple format, and we already sniff its magic): find `/memory`,
the UART, the timer interrupt. From here on, board constants are
*fallbacks*, not truths. This is not optional polish: on Apple Silicon the
UART base genuinely differs per SoC and the FDT is the only honest source.

Difficulty: easy-moderate. A pleasant breather; mostly careful byte-pushing
against a well-specified format.

## M5 — EL0 and syscalls

A userspace: drop to EL0 into a tiny embedded program, take `svc` traps,
a handful of syscalls (write, exit, yield). Separate address spaces via
`TTBR0`/`TTBR1` split. The moment Opal stops being a program and becomes
an operating system.

Difficulty: **spike.** The EL1↔EL0 boundary concentrates everything sharp:
exception returns, address-space switching, the first "untrusted memory"
handling.

## M6 — scheduler and IPC

Cooperative round-robin first, then preemptive off the M3 timer tick.
Context switch (callee-saved registers + `TTBR0` + per-task kernel stack),
a few tasks, and a minimal message-passing IPC primitive. Multi-core can
start here too (PSCI `CPU_ON` for the cores M0 parked) — or be deferred;
single-core scheduling is plenty educational.

Difficulty: moderate-hard. Conceptually clean, but the first preemption
bugs are heisenbugs by nature.

## M7 — Apple Silicon bring-up (the m1n1 milestone)

Everything docs/02-hal-and-apple-silicon.md specifies: arm64 Image-format
flat binary, position-independent boot, EL2 entry handling, FDT-driven
console discovery, `hal/s5l_uart.rs`, `board/apple.rs`, framebuffer
text console, AIC driver + timers-over-FIQ. Dev loop: m1n1 proxy over
USB-C (`chainload.py` / `linux.py` / `run_guest.py`), with m1n1's
hypervisor providing virtual-UART output and MMIO tracing for debugging.

Difficulty: **spike**, but a *bounded* one — every architectural decision
it dictates (16K, FIQ, FDT discovery, PIC boot) was made milestones ago on
QEMU. The new work is drivers and entry-EL plumbing, debugged over a
7-second USB upload loop.

## Beyond the ladder — here be dragons 🐉

Honesty about what is *not* scheduled, because the difficulty changes
species past this point:

- **RTKit** — Apple's coprocessor mailbox protocol. Nearly every
  interesting peripheral (NVMe, USB, display, GPU) hides behind a
  firmware coprocessor speaking it. Reverse-engineered, documented mostly
  in Asahi's source code, and unavoidable for...
- **NVMe (internal storage)** — not a standard NVMe controller: it sits
  behind RTKit + SART and has Apple-specific queue behavior. Until then,
  Opal's "disk" is whatever we preload into RAM.
- **USB** — a DWC3 controller behind Type-C PHY/mux management (and on
  the way there, I²C, SPMI, and friends). "Plug in a keyboard" is a
  multi-driver epic, which is precisely why the m1n1 *virtual* UART is the
  M7 console rather than any local device.
- **Power/clocks/PMGR, SMC, display pipeline, GPU** — each its own
  research project; Asahi's drivers are the map, and the map says "months."

These aren't refusals — they're labeled mountains. The teaching kernel's
job is to reach their foothills with a reader who still understands every
line behind them.
