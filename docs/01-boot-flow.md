# 01 — Boot flow: from `qemu -kernel` to the banner

This is the complete story of milestone 0. By the end you will know what
every instruction between power-on and `println!` does and why it has to
exist. Keep `src/arch/aarch64/linker.ld` and `src/arch/aarch64/boot.rs`
open next to this.

## 1. What `qemu -kernel kernel.elf` actually does

`cargo run` launches:

```
qemu-system-aarch64 -machine virt -cpu max -smp 1 -m 512M -nographic -kernel <our ELF>
```

The `virt` machine is QEMU's idealized ARM board — no real-world quirks, a
clean memory map, and a devicetree describing everything. Only two
addresses are *documented as stable* across QEMU versions:

| address       | what                                |
|---------------|-------------------------------------|
| `0x0000_0000` | flash (128 MiB)                     |
| `0x4000_0000` | start of RAM (`-m` decides the size)|

Everything else — UART, interrupt controller, RTC — is supposed to be
discovered from the devicetree blob (DTB). We'll get to where that blob is.

`-kernel` accepts two very different kinds of file, and QEMU's loader
(`hw/arm/boot.c`) decides by format, with a comment that says it all:
*"Assume that raw images are linux kernels, and ELF images are not."*

- A **raw binary** is booted with the **Linux boot protocol**: QEMU
  pretends to be a Linux bootloader, puts a DTB pointer in `x0`, places the
  DTB carefully, the works.
- An **ELF** — what cargo produces — is booted as **bare metal**: QEMU
  reads the ELF program headers, copies each segment to its stated physical
  address, and sets the program counter to the ELF entry point. **Nothing
  else.** No registers are set up; `x0` holds a general-purpose register's
  reset value — architecturally UNKNOWN, zero in practice under QEMU. You
  asked to run an arbitrary kernel; QEMU obliges, literally.

The bare-metal path does do one favor: if the ELF's lowest load address
leaves room at the base of RAM, QEMU drops the DTB at `0x4000_0000`.

> **A trap we hit so you don't have to:** "leaves room" is checked against
> the DTB's *pre-allocated buffer size* — 1 MiB (`FDT_MAX_SIZE`) — not its
> real packed size (~100 KiB). And if the check fails, `arm_load_dtb()`
> returns success *without loading anything*. Our first draft linked the
> kernel at `0x4008_0000` (512 KiB of room): it booted perfectly and
> silently had no devicetree. That is why `linker.ld` links at
> `0x4020_0000` — 2 MiB of headroom, comfortably more than QEMU wants.

## 2. Machine state at the first instruction

When QEMU jumps to our entry point, the CPU is in the architectural reset
state at the highest implemented exception level. ARM privilege levels go
EL0 (user) → EL1 (kernel) → EL2 (hypervisor) → EL3 (firmware). The default
`virt` machine implements neither EL3 (`secure=off`) nor EL2
(`virtualization=off`), so we wake at **EL1** — ordinary kernel privilege.
(On Apple Silicon via m1n1 we will wake at EL2; the banner prints the level
precisely so that difference is visible the day it happens.)

The full inventory at instruction zero:

- **MMU off** (`SCTLR_EL1.M = 0`): every address is a physical address.
- **Caches off** (`SCTLR_EL1.C/I = 0`): slow, but blissfully simple.
- **SP undefined**: touching the stack before setting SP is memory
  corruption roulette.
- **`.bss` not zeroed**: the ELF format says "this region is zeros" but
  ships no bytes for it; an OS loader would zero it. There is no OS. DRAM
  contains whatever it contains.
- **All general registers at reset values** (zero under QEMU). In
  particular `x0` does *not* hold a DTB pointer — that contract belongs to
  the Linux protocol we are not using.

Rust code cannot run in this state. The compiler assumes a valid stack at
all times (it may spill registers in any function, including the first) and
assumes statics are initialized. Creating those conditions is the job of
some twenty assembly instructions. Everything in OS bring-up follows this
shape:
*each layer's job is to build the world the next layer assumes.*

## 3. `linker.ld` — deciding where things live

A userspace program never thinks about addresses; the OS and dynamic loader
decide. A kernel *is* the decider, and the linker script is where it writes
the decision down. The script reaches the linker through `build.rs`, which
emits `cargo::rustc-link-arg=-T<script>` with an absolute path built from
`CARGO_MANIFEST_DIR`, so the build works from any directory. It also emits
`cargo::rerun-if-changed` for the script, so editing the memory layout
actually triggers a relink — `build.rs`'s own comment tells the story of
the staleness bug that earned it the job. Walk through
`src/arch/aarch64/linker.ld`:

- **`ENTRY(_start)`** — records in the ELF header where execution begins.
  QEMU reads exactly this field.

- **`. = 0x40200000;`** — the location counter: "lay out everything from
  here." RAM base plus 2 MiB (see the trap above).

- **`.text`** with `KEEP(*(.text.boot))` first — our boot stub asks to be
  placed in a section called `.text.boot`; listing it first makes `_start`
  the literal first byte of the image, and `KEEP` protects it if the linker
  ever garbage-collects unreferenced sections (nothing *calls* `_start`, so
  to the linker it looks dead).

- **`.rodata`, `.data`** — string literals and initialized statics. Nothing
  exotic.

- **`.bss (NOLOAD)`** — zero-initialized statics. `NOLOAD` means the ELF
  records the region's address and size but contains no bytes for it. The
  script exports `__bss_start` and `__bss_end`, both 16-byte aligned, as
  symbols — the boot stub will loop over exactly that range. This is the
  classic linker-script trick: *the script computes addresses, code
  consumes them as if they were extern variables.*

- **`.stack (NOLOAD)`** — 64 KiB of reserved space, with `__stack_top`
  exported. AArch64 stacks grow downward and the AAPCS64 calling convention
  requires SP ≡ 0 (mod 16) at every call, hence the alignment. Why 64 KiB?
  Generous for a kernel that doesn't recurse; small enough not to care.
  There is no guard page yet — a stack overflow walks silently into `.bss`.
  The MMU milestone (M2) fixes that properly.

- **`/DISCARD/`** — sections we refuse to carry: unwind tables (`.eh_frame`,
  dead weight under `panic=abort`), toolchain version notes.

- **`ASSERT(SIZEOF(.got) == 0, ...)`** — a tripwire: statically linked
  kernel code should need no Global Offset Table; if one appears, fail the
  *link* loudly rather than boot something subtly position-dependent.

## 4. `boot.rs` — twenty-one instructions to Rust

`boot.rs` contains one `global_asm!` block (assembly compiled into the
binary as-is, in section `.text.boot`) and one Rust shim. The assembly, in
order:

**Park the secondaries.** We boot `-smp 1` today, but the stub is written
for the day we don't. `MPIDR_EL1` identifies a core by four *affinity*
fields — Aff0 (bits 7:0) is the core within a cluster, Aff1 (15:8) the
cluster, Aff2 and Aff3 higher groupings — and the stub masks out and tests
all four. Testing Aff0 alone is a classic trap: the first core of *every*
cluster reads Aff0 == 0 (on Apple Silicon the P-cluster starts at Aff1=1,
Aff0=0), would believe it is the boot core, and would race core 0 onto the
same stack while re-zeroing `.bss` under it. Only the core whose whole
affinity is zero proceeds; every other core drops into a `wfe`
(wait-for-event) loop — a polite, low-power "sleep until someone pokes
you." Waking them properly (via PSCI on QEMU) is a later milestone.

**Set the stack pointer.** `adrp`/`add` build `__stack_top`'s address
PC-relatively (`adrp` finds the 4 KiB page, `:lo12:` adds the page offset).
PC-relative matters: it works no matter where the image was loaded — a
habit we'll be grateful for on Apple Silicon, where m1n1 loads the kernel
at a randomized address. After `mov sp, x1`, function calls are legal.

**Zero `.bss`.** A four-instruction loop storing 16 zero bytes per
iteration (`stp xzr, xzr, [x1], #16` — store a pair of zero registers,
post-incrementing the cursor) from `__bss_start` to `__bss_end`. The linker
script's 16-byte alignment of both symbols is what lets the loop be this
naive. After this, Rust's assumption that statics start initialized is
true.

**`bl _start_rust`.** One detail carries the whole bootloader interface:
**`x0` has not been touched** since QEMU handed over control. AAPCS64 puts
a function's first argument in `x0` — so simply *not clobbering it* means
`_start_rust(x0)` receives whatever the loader left there. Under QEMU
that's 0; under m1n1 it will be the devicetree pointer. Same stub, both
worlds.

**The net.** `_start_rust` is declared `-> !` (never returns). If a bug
ever violates that, the next instructions are a `wfe` parking loop, not a
march into uninitialized memory.

The Rust side is three lines: `_start_rust` is `#[unsafe(no_mangle)]`
(assembly must be able to name it — and edition 2024 marks the attribute
`unsafe` because symbol collisions break linkage soundness) and
`extern "C"` (so the AAPCS64 register convention, x0 = first argument,
actually applies). It calls `kmain`.

## 5. `kmain` — the banner, explained

`kmain(x0)` in `src/main.rs` runs on a real stack with initialized statics
— ordinary Rust, minus everything `std` would need an OS for. It prints
each line of the banner by *checking*, not assuming:

- **`current EL`** — reads the `CurrentEL` system register (bits [3:2]
  hold the level). Expect `EL1` on QEMU, `EL2` the day this boots via m1n1.
- **`x0 at entry`** — the raw value, straight from the boot stub. Expect 0
  under QEMU's ELF boot; a pointer under any Linux-protocol loader.
- **`fdt at x0` / `fdt at RAM base`** — every flattened devicetree begins
  with magic `0xd00dfeed` stored big-endian. We check both places a DTB
  could plausibly be and report what we *find*, not what docs promise. One
  guard: we only dereference addresses inside RAM — with the MMU off and
  no exception vectors installed (that's milestone 1), reading a hole in
  the physical map would hang the machine with no diagnostics whatsoever.

Then the echo loop: poll the UART for a byte, write it back (`\r` from your
Enter key becomes a real newline; unprintable bytes are shown as `<0xNN>`
so nothing is invisible). It's three lines of code, but it closes the loop:
input and output both work, interactively, on a kernel you can read from
`_start` to `loop`.

How a character actually gets out: `println!` → `format_args!` renders into
`&str`s → our `fmt::Write` impl feeds bytes to the PL011 driver → a
volatile read of the flag register until the TX FIFO isn't full → a
volatile write of the data register at `0x0900_0000` → QEMU's PL011 model
hands the byte to your terminal. The driver's file
(`src/hal/pl011.rs`) documents the MMIO rules that make those two volatile
accesses correct — and the simulator-isms it currently leans on.

## 6. What we deliberately did not do

Milestone 0 is honest about its debts. No exception vectors (any CPU fault
is a silent hang — M1). No MMU, no caches (everything physical, slow —
M2). Interrupts never enabled; all I/O is polling (M3). One core (the
others are parked in `wfe`, to be woken via PSCI `CPU_ON` — M6). No locking on the console, which is
*sound* today exactly because of the previous two sentences, and becomes
unsound the moment M1 lands — the comment in `pl011.rs` and the roadmap
both carry that flag.

Next: [ROADMAP.md](../ROADMAP.md) for where this goes, and
[02-hal-and-apple-silicon.md](02-hal-and-apple-silicon.md) for the real
target this is all rehearsal for.
