# Opal

A teaching operating system kernel in Rust for 64-bit ARM. Opal exists to be
*read*: every file fits in one sitting, every magic number has a paragraph
explaining where it comes from, and — beyond `core` and `compiler_builtins`,
which ship with the compiler — there is not a single external crate: every
other line of code that runs is in this repository.

The destination is bare metal on Apple Silicon, booted by
[m1n1](https://github.com/AsahiLinux/m1n1) (the Asahi Linux bootloader).
The daily development board is QEMU's aarch64 `virt` machine, because a
ten-second edit-boot-test loop on the laptop you already own beats anything
involving real hardware. The two targets are closer than they look — same
architecture, same exception model, even the same 16 KiB pages (an
ambition until M2; a running fact since — `-cpu max` implements the 16K
granule precisely so we could rehearse for Apple's IOMMUs). See
[docs/02-hal-and-apple-silicon.md](docs/02-hal-and-apple-silicon.md)
for the full story of how the two boards relate.

**Current state: Milestone 2 — "maps its own world."** The kernel builds
its own page tables (16 KiB granule, four-level walk), turns on the MMU
and caches, and moves to the higher half — it loads at physical
`0x4020_0000` and *runs* at `0xFFFF_0000_4020_0000`-and-up, with the low
half deliberately condemned behind it. The image is W^X (code can't be
written, data can't be executed), a 16 KiB guard page below the stack
turns silent overflow into a loud report (M0's oldest debt, paid), and
the monitor grew oracles (`translate`, `walk` — ask the hardware where an
address goes, or narrate the table walk yourself) plus five fatal
commands that each demonstrate a distinct fault syndrome. M1's fatal
`unaligned` demo is now a *survivor*: the same load that used to kill the
kernel just returns its bytes, because RAM is finally Normal memory.

## Quickstart

You need a Mac (or any host with QEMU) and rustup.

```sh
brew install qemu        # provides qemu-system-aarch64
git clone <this repo> && cd opal
cargo run                # that's it
```

`rust-toolchain.toml` makes rustup install the pinned toolchain and the
`aarch64-unknown-none-softfloat` target automatically on first build.
`.cargo/config.toml` makes `cargo run` launch QEMU headless with the fresh
kernel. You should see:

```
opal — milestone 2: maps its own world
--------------------------------------
current EL : EL1
mmu        : on — SCTLR_EL1 = 0x30d5199d (M, C, I — read back, not assumed)
granule    : 16 KiB, 48-bit VA — TCR_EL1 = 0x57510b510 (TG0=16K, TG1=16K: different encodings, both checked)
pa range   : PARange 0b110 (52 bits) -> IPS 0b101 (48 bits; DS=0 caps the output at 48)
ttbr1      : 0x4021c000 — the kernel's tree (a physical address: the walker speaks PA)
ttbr0      : 0x40230000 — empty root; ground floor condemned (AT probe: 0x40200000 no longer translates)
pc         : 0xffff000040204d80 — kmain itself runs in the higher half
vectors    : VBAR_EL1 = 0xffff000040200800 (16-entry table live, upstairs)
guard      : 16 KiB unmapped below the stack — overflow now faults instead of eating .bss (M0's debt, paid)
x0 at entry: 0x0
fdt at x0  : no  (expected under QEMU ELF boot: x0 is just QEMU's reset zero)
fdt at RAM base (PA 0x40000000, read via its higher-half alias): yes — QEMU's bare-metal DTB placement

monitor ready — 'help' lists commands. Ctrl-A X quits QEMU.

>
```

Every `mmu`/`granule`/`ttbr`/`pc` line is a *read-back* — the register's
own testimony, not the boot code's intentions. Now make the kernel hurt
itself: type `brk` and Enter. The CPU takes a breakpoint exception, the
kernel prints a full report — cause, decoded syndrome, every register —
then steps past the breakpoint and *returns to the prompt*:

```
> brk

*** exception: synchronous, from current EL on SP_ELx ***
  cause   : BRK #0xf00d — a software breakpoint (EC 0x3c)
  ...
  elr     : 0xffff000040207c68  (preferred return address)
  ...
  verdict : recovered — ELR pointed AT the brk (its preferred return
            address); we advanced it one instruction so eret resumes
            just past the breakpoint.

...and we're back — the kernel caught its own fault and lived.
>
```

`svc 7` demonstrates the same survival for a supervisor call, and
`unaligned` — fatal in M1 — now just *works* and says so: the same load
that killed the M1 kernel returns its bytes, because RAM became Normal
memory. `translate <va>` and `walk <va>` are the address-space oracles
(the hardware's answer, and a narrated software walk cross-checked
against it). Then the fatal five, one fault syndrome each: `guard`
(stack overflow hits the guard page — translation fault, level 3), `wx`
(write to read-only memory — permission fault), `noexec` (execute from
.data — instruction abort), `low` (the condemned low half — translation
fault, level **0**), and `abort` (the bus-error window — a mapped read
that the bus itself rejects). docs/04 §10 predicts every syndrome before
you trigger it. **Ctrl-A X** quits QEMU (**Ctrl-A C** toggles the QEMU
monitor if you're curious).

## Repository map

```
Cargo.toml                  crate definition; zero dependencies, abort-on-panic
build.rs                    hands linker.ld to the linker, tracked as a build input
rust-toolchain.toml         pinned toolchain + target, installed by rustup
.cargo/config.toml          default target, QEMU runner
src/
  main.rs                   kmain, print!/println! + console lock, banner,
                            fault-demo monitor, panic handler
  sync.rs                   hand-rolled spinlock (and the deadlock honesty notes)
  arch/
    mod.rs                  one architecture, no cfg maze — and why
    aarch64/
      linker.ld             memory layout: VAs upstairs, PAs on the ground
                            (the AT() split), W^X boundaries, the stack guard
      boot.rs               _start: park cores, stack, zero .bss, build the
                            page tables, MMU on, prove it, jump to the high half
      mmu.rs                the page tables: descriptors bit by bit, the enable
                            ceremony, condemnation, translate/walk oracles
      vectors.rs            exception vector table, trap frames, fault reports
      mod.rs                CurrentEL reader, DAIF helpers, here(), park()
  hal/
    mod.rs                  what "HAL" means here: drivers, not trait soup
    pl011.rs                polled PL011 UART driver (the only driver so far)
  board/
    mod.rs                  boards know addresses; drivers know devices
    virt.rs                 QEMU virt board: physical addresses, the console's
                            higher-half alias, (trivial) init
docs/
  01-boot-flow.md           power-on to banner, line by line
  02-hal-and-apple-silicon.md  the HAL design and the real target
  03-exceptions.md          the vector table, fault reports, and the console lock
  04-virtual-memory.md      the 16K walk, the enable cliff, the move upstairs
ROADMAP.md                  the milestone ladder, with honest difficulty notes
```

Suggested reading order: this file → `docs/01-boot-flow.md` alongside
`linker.ld`, `build.rs`, and `boot.rs` → `hal/pl011.rs` → `main.rs` →
`docs/03-exceptions.md` alongside `arch/aarch64/vectors.rs` and `sync.rs`
→ `docs/04-virtual-memory.md` alongside `arch/aarch64/mmu.rs` (and a
second pass over `linker.ld` and `boot.rs`, which it rewrote) →
`docs/02-hal-and-apple-silicon.md` → `ROADMAP.md`.

## Philosophy

- **Readable over clever.** Code is written to teach. If a trick saves ten
  lines but needs a manual to decode, the ten lines stay.
- **Zero dependencies.** Not as dogma but as pedagogy: when the only crate
  is this one, there is no "and then magic happens" layer. Every volatile
  write, every linker symbol, every instruction is on the page.
- **Honest about the environment.** QEMU forgives things real hardware
  won't (our UART driver works without init *only* because QEMU's model is
  lenient — and the comment says so). Where we lean on a simulator-ism, we
  label it and note which milestone pays the debt.
- **Abstractions must be earned.** There is no `trait Uart` with one
  implementor. When the Apple Silicon UART arrives and two drivers genuinely
  share a shape, *then* a trait appears.
- **VM-first, bare-metal-later.** Milestones land on QEMU where iteration
  is seconds and a debugger is a flag away. The Apple Silicon bring-up is a
  scheduled milestone with its own doc, not a vague aspiration — the design
  decisions that it dictates (16 KiB pages, FIQ handling, devicetree-driven
  discovery) are taken early, while they're cheap.
