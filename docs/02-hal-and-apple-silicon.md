# 02 — The HAL, and the machine we're actually aiming at

Opal develops on QEMU but is aimed at bare-metal Apple Silicon. This doc
explains the thin abstraction layer that lets one kernel face both, and
then lays out — concretely, from the Asahi Linux project's documentation —
what the Apple side of that layer will have to do.

## 1. The HAL design (and why it's so small)

Two directories share the hardware work:

- **`src/hal/`** — drivers. A driver knows how a hardware *block* works
  (what the registers mean), not where it is or which machine it's in.
  `pl011.rs` is the entire HAL today.
- **`src/board/`** — boards. A board knows what hardware *exists* and
  *where*, and how to bring the machine up. `virt.rs` is three constants
  (RAM base, RAM size, UART base), a `Console` type alias plus the
  `const fn` that conjures one, and a deliberately empty `init()`.

Notice what's missing: there is no `trait Uart`, no `dyn Console`, no
generic driver registry. With one implementor, a trait is a lie about the
present dressed up as a plan for the future. The interface that the rest of
the kernel actually consumes is `core::fmt::Write` — which the standard
library already defines — plus two byte-level methods. When the second UART
driver (Apple's) exists and the two genuinely share a shape, a trait can be
*extracted* from working code. Abstractions are earned here.

Two design decisions worth defending:

- **The console type is zero-sized.** `Pl011<BASE>` carries its MMIO base
  in the type, so "creating" a console is free and possible anywhere —
  including inside the panic handler, where shared state may be mid-flight.
  There is nothing to initialize and therefore nothing to be uninitialized.
- **There is no lock, on purpose, temporarily.** Milestone 0 is one core
  with interrupts off: a second concurrent printer is impossible by
  construction. The moment milestone 1 introduces exception handlers that
  might print, this assumption dies; the console then gets a real spinlock
  (and interrupt masking around it). The zero-sized type makes that a
  type-alias change, not a hunt through call sites.

And one honesty note, again: the PL011 driver does no initialization
because *QEMU's model* transmits even when the UART is disabled. Real
PL011 hardware needs baud-rate and control-register setup. That work is
explicitly parked in `board::init()`'s future.

## 2. The real target: how a kernel gets onto Apple Silicon

Nobody — not even Asahi Linux — replaces Apple's early boot chain. The
sequence is SecureROM → iBoot1 → iBoot2, all Apple-signed, and the OS
kernel slot at the end of it is the first place non-Apple code can run.
The supported path (the same one Asahi uses in production):

1. The Mac owner creates an OS volume with **Permissive Security** — a
   per-volume policy (you can keep a Full-Security macOS next to it), and
   downgrading requires the machine owner's credentials in 1TR, the
   power-button-held recovery mode. This is sanctioned, documented
   machinery, not a jailbreak.
2. **m1n1**, the Asahi bootloader, is installed as if it were a kernel
   (`kmutil configure-boot ...`); iBoot2 then boots m1n1 with the
   **Mach-O boot protocol**: MMU off, `x0` → a `boot_args` struct, and the
   hardware described by an **ADT** — Apple's bespoke device tree format.
3. m1n1's whole purpose is to absorb that Apple-ness. It parses the
   ADT/boot_args and emits a **standard arm64 Linux boot handoff**: it
   jumps to the next stage with MMU off and `x0` → a normal **FDT**
   (the magic-`0xd00dfeed` kind Opal already sniffs for), x1–x3 zero.

So Opal's m1n1 contract is, by m1n1's deliberate design, *almost* the QEMU
Linux-protocol contract. The differences that matter:

| | QEMU virt (today) | Apple Silicon via m1n1 |
|---|---|---|
| entry EL | **EL1** | **EL2** (Apple has no EL3; must self-demote or run at EL2) |
| `x0` | 0 (ELF boot) | physical FDT pointer |
| load address | fixed by our linker script | **randomized** — image must be position-independent |
| image format | ELF | flat arm64 "Image"-format binary (Linux-style header) |
| console | PL011 at a known address | Samsung-style **s5l UART**, base discovered from the FDT (differs per SoC) |
| interrupt controller | GIC | **AIC/AICv2**, and timers arrive as **FIQs**, not IRQs |
| page granule | our choice; `-cpu max` offers 16K | **16K effectively mandatory** (DART IOMMUs are 16K-only) |
| display | (none, serial only) | iBoot-provided framebuffer, republished by m1n1 in the FDT |

The development loop is also worth knowing now, because it shapes the
milestone: with a USB-C cable, m1n1 exposes a proxy over two CDC-ACM TTYs.
`tools/chainload.py` uploads a fresh m1n1, `tools/linux.py` uploads and
boots a kernel — about seven seconds per iteration, no reflashing, and
m1n1's hypervisor mode (`run_guest.py`) can keep itself resident at EL2,
run your kernel at EL1, trap its MMIO, and forward a **virtual UART** over
USB. That hypervisor is the closest thing to QEMU semantics you can get on
the real machine, and it is how the first Apple boots of Opal will be
debugged.

## 3. What `board/apple.rs` will actually need

Translating the table into work items (this is the spec for the m1n1
milestone in ROADMAP.md):

1. **Image format + PIC.** Produce a flat binary with the arm64 Linux
   image header (`llvm-objcopy` from our ELF, plus a small header stub),
   and make the boot path genuinely position-independent — the
   `adrp`-everywhere habit from `boot.rs` becomes load-bearing.
2. **EL2 entry.** `current_el()` will print 2. The early-boot code must
   either configure the minimal EL2 state and drop to EL1, or run at EL2;
   either way the vector-table work from milestone M1 grows an EL2 case.
   One hard constraint from iBoot: the boot CPU's RVBAR is locked — don't
   touch it; `VBAR_EL2` is ours.
3. **FDT-driven discovery.** No hardcoded UART base this time: the s5l
   UART's address differs per SoC, so the minimal FDT parser (built in an
   earlier milestone against QEMU's DTB, deliberately) becomes the *only*
   way to find the console. This is why `board/virt.rs` already labels its
   constants as conveniences, not contracts.
4. **`hal/s5l_uart.rs`.** A Samsung-style UART driver (Linux binding
   `apple,s5l-uart`) — polled TX/RX first, same shape as `pl011.rs`. Under
   m1n1's hypervisor its MMIO is trapped and tunneled over USB, so the
   identical driver serves tethered debugging and real hardware.
5. **Framebuffer console.** The only display path guaranteed on every
   machine: m1n1 republishes iBoot's framebuffer as a simple-framebuffer
   node (base, width, height, stride, format — typically `x8r8g8b8`).
   Blitting a font into it is early Asahi's exact bring-up trick, and a
   genuinely fun milestone.
6. **FIQ-first interrupts, AIC later.** Apple wires the architectural
   timers and fast IPIs to the FIQ line, not to the interrupt controller —
   so the M1/M3 exception and timer work must treat FIQ as a first-class
   citizen from the start (on QEMU this costs one extra vector entry; on
   Apple it is the difference between a timer tick and silence). Device
   interrupts then come via AIC (M1-generation) or AICv2 (M1 Pro/Max and
   later, which adds a die index for multi-die parts).
7. **16 KiB pages.** The DART IOMMUs are effectively 16K-only, so Opal's
   MMU milestone (M2) uses the 16K granule *on QEMU* from day one — that
   is the entire reason `.cargo/config.toml` says `-cpu max` (which
   implements TGRAN16) instead of a named CPU like `cortex-a72` (which
   does not).

Items 6 and 7 are the quiet payoff of the VM-first strategy: they are
*Apple* requirements that we get to implement and debug *on QEMU*, months
before touching the machine that enforces them.

## Sources

All Apple Silicon facts above are from the Asahi Linux project's
documentation and source: asahilinux.org/docs (boot flow, Mach-O boot
protocol, m1n1 user guide, tethered boot, hypervisor, AIC), the m1n1
README and source (`src/kboot.c` for the handoff contract), the Linux
devicetree bindings (`apple,aic.yaml`, `apple,aic2.yaml`), and Asahi
progress reports (16K page rationale, framebuffer bring-up).
