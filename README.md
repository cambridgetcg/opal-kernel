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
architecture, same exception model, even the same 16 KiB page-size ambitions
(`-cpu max` implements the 16K granule precisely so we can rehearse for
Apple's IOMMUs). See [docs/02-hal-and-apple-silicon.md](docs/02-hal-and-apple-silicon.md)
for the full story of how the two boards relate.

**Current state: Milestone 1 — "catches its own faults."** The kernel
boots to EL1, installs a full exception vector table, prints a banner over
the serial port, and then runs a tiny monitor whose commands make the CPU
fault *on purpose*: a breakpoint and a supervisor call that the kernel
reports and survives, and two genuinely fatal faults that it reports
before parking — "what faulted, where, and why" instead of M0's silent
hang. The console grew the spinlock M0 promised the moment a second
execution context (a fault handler) could print.

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
opal — milestone 1: catches its own faults
------------------------------------------
current EL : EL1
vectors    : VBAR_EL1 = 0x40200800 (16-entry table live)
x0 at entry: 0x0
fdt at x0  : no  (expected under QEMU ELF boot: x0 is just QEMU's reset zero)
fdt at RAM base 0x40000000: yes — QEMU's bare-metal DTB placement

monitor ready — 'help' lists commands. Ctrl-A X quits QEMU.

>
```

Now make the kernel hurt itself: type `brk` and Enter. The CPU takes a
breakpoint exception, the kernel prints a full report — cause, decoded
syndrome, every register — then steps past the breakpoint and *returns to
the prompt*:

```
> brk

*** exception: synchronous, from current EL on SP_ELx ***
  cause   : BRK #0xf00d — a software breakpoint (EC 0x3c)
  ...
  verdict : recovered — ELR pointed AT the brk (its preferred return
            address); we advanced it one instruction so eret resumes
            just past the breakpoint.

...and we're back — the kernel caught its own fault and lived.
>
```

`svc 7` demonstrates the same survival for a supervisor call (and why
*its* return address needs no fixing), while `unaligned` and `abort` show
the honest other half: faults nothing can repair yet, reported in full
and then parked. **Ctrl-A X** quits QEMU (**Ctrl-A C** toggles the QEMU
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
      linker.ld             memory layout: where the kernel lives in RAM
      boot.rs               _start: park cores, set SP, zero .bss, call Rust
      vectors.rs            exception vector table, trap frames, fault reports
      mod.rs                CurrentEL reader, DAIF helpers, park()
  hal/
    mod.rs                  what "HAL" means here: drivers, not trait soup
    pl011.rs                polled PL011 UART driver (the only driver so far)
  board/
    mod.rs                  boards know addresses; drivers know devices
    virt.rs                 QEMU virt board: addresses + (trivial) init
docs/
  01-boot-flow.md           power-on to banner, line by line
  02-hal-and-apple-silicon.md  the HAL design and the real target
  03-exceptions.md          the vector table, fault reports, and the console lock
ROADMAP.md                  the milestone ladder, with honest difficulty notes
```

Suggested reading order: this file → `docs/01-boot-flow.md` alongside
`linker.ld`, `build.rs`, and `boot.rs` → `hal/pl011.rs` → `main.rs` →
`docs/03-exceptions.md` alongside `arch/aarch64/vectors.rs` and `sync.rs`
→ `docs/02-hal-and-apple-silicon.md` → `ROADMAP.md`.

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
