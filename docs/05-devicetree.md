# 05 — Devicetree: the bootloader's handoff

This is the complete story of milestone 4. By the end you will know how
Opal reads the devicetree that QEMU (or m1n1, or U-Boot) leaves in RAM,
and why every board constant in `board/virt.rs` is now a fallback rather
than a truth.

Keep `src/arch/aarch64/fdt.rs` open while you read this — the code and
the prose are companions, and neither makes full sense alone.

---

## 1. What is a devicetree, and why do we need it?

Until M4, Opal hardcoded every hardware address:

```rust
pub const RAM_BASE: usize   = 0x4000_0000;
pub const UART0_BASE: usize = 0x0900_0000;
pub const GICD_BASE: usize  = 0x0800_0000;
```

The comments said so honestly: *"the FDT parser is still milestones
away."* M4 is that milestone.

A **devicetree** is a binary blob the bootloader leaves in RAM at boot.
It describes what hardware exists: how much RAM and at what address,
where the UART is, what interrupt controller, which timers, how many
CPUs. It is the honest answer to *"what world am I running on?"*

On QEMU's `virt` board the addresses never change, so hardcoding them
works. On Apple Silicon the UART base differs per SoC — the only honest
source is the devicetree. The principle holds even when the practice
doesn't bite yet: the constants become *fallbacks*, and the FDT is the
truth.

---

## 2. The format (Devicetree Specification §5)

A DTB ("device tree blob") is one contiguous block of bytes, laid out as
four regions back to back:

```
+------------------+
| header (40 B)    |  10 × u32, all big-endian
+------------------+
| memory reserve   |  array of 16-byte entries (addr u64, size u64),
| map              |  terminated by an all-zeros entry
+------------------+
| structure block  |  tokens (u32 big-endian): BEGIN_NODE, PROP, ...
+------------------+
| string block     |  null-terminated strings, deduplicated
+------------------+
```

The **header** gives the byte offsets of each region. The **structure
block** is a flat token stream that encodes a tree. The **string block**
is a string-interning table: property names live here, referenced by
offset, so the same name (`"reg"`, `"compatible"`, `"interrupts"`) is
stored once and shared by every property that uses it.

All integers are big-endian. All offsets are from the start of the blob.
The structure block is 4-aligned; property data is padded to 4 bytes.

### The structure block's vocabulary

Five tokens (each a big-endian u32):

| token | value | meaning |
|-------|-------|---------|
| `FDT_BEGIN_NODE` | 0x1 | start of a node; followed by null-terminated name, padded to 4 |
| `FDT_END_NODE`   | 0x2 | end of the most recent unclosed node |
| `FDT_PROP`       | 0x3 | a property; followed by len (u32), nameoff (u32), and `len` bytes of data |
| `FDT_NOP`        | 0x4 | no-op (skip) |
| `FDT_END`        | 0x9 | end of the structure block |

The tree is depth-first: a node's properties come first, then its
children, then `FDT_END_NODE`. Node names may include a `@unit` address
suffix (e.g. `memory@40000000`).

---

## 3. How Opal reads it

QEMU places the DTB at the start of RAM (`0x4000_0000`). Since M2 the
kernel reads all of RAM through its higher-half alias (`phys_to_virt`),
where the DTB window is mapped read-only Normal memory. Every load goes
through `read_volatile` — this memory was written by QEMU, not by Rust
code the compiler knows about.

### The safety contract

The blob lives in mapped RAM for the kernel's lifetime. `Fdtr::new`
validates the header once (magic, bounds, offsets within totalsize) and
then caches the virtual address. From that point on, all reads are
bounds-checked slice indexing — the one unsafe act (conjuring a
reference to bootloader-written RAM) is concentrated in `new`, with a
clear safety argument.

### The `Fdtr` struct

```rust
pub struct Fdtr {
    base: usize,      // virtual address of the DTB blob
    header: Header,   // validated, cached
}
```

A `Node` is just an offset into the structure block — a plain `Copy`
struct with no raw pointer. Safety comes from the borrow checker: you
can only get a `Node` from an `Fdtr`, and all property reads borrow
`&self`.

### Navigation

- `root()` — the first `FDT_BEGIN_NODE` in the structure block.
- `find("/memory")` — find a node by path. Path components match node
  names with or without the `@unit` suffix: `/memory` matches
  `memory@40000000`, `/intc` matches `intc@8000000`.
- `children(&node)` — iterate direct children.
- `prop(&node, "reg")` — get a property's raw bytes.
- `compatible(&node)` — the first entry of the `compatible` property
  (a null-separated list of strings).

---

## 4. What the kernel does with it

At boot, `kmain` parses the FDT and cross-checks what it says against
the board constants:

```
dtree     : parsed — 1048576 bytes, boot CPU 0
  /memory : base 0x40000000 (matches board const), size 0x20000000 (matches board const)
  /intc   : "arm,cortex-a15-gic" — GICD 0x8000000 (matches), GICC 0x8010000 (matches)
  /pl011  : "arm,pl011" — base 0x9000000 (matches board const)
  /timer  : "arm,armv8-timer"
           virtual timer PPI 11 -> IRQ 27 (matches TIMER_IRQ)
```

Every constant agrees with the devicetree. On QEMU's virt this is
expected — but the cross-check is the point: when Opal meets a different
board (Apple Silicon, M7), the same code will report whether the
constants still hold or need to come from the FDT instead.

### The `dtb` and `tree` monitor commands

Two new commands let you explore the devicetree interactively:

- `dtb` — a structured summary: header, reservations, /memory, /intc,
  /pl011, /timer with their addresses and compatible strings.
- `tree` — a full dump of every node and property, heuristically
  formatted (strings as `"..."`, u32 arrays as `<...>`, raw hex
  otherwise). Compare with `dtc -I dtb -O dts /tmp/virt.dtb` to see
  the same data in a different rendering.

---

## 5. What this parser deliberately doesn't do (yet)

- **No `#address-cells` / `#size-cells` propagation.** The root's
  values (`#address-cells=2`, `#size-cells=2` on QEMU's virt) are
  assumed; `reg` properties are interpreted as pairs of u64s
  accordingly. A production parser walks the tree to inherit these
  values per node. We don't need that yet — there's one board.
- **No interrupt mapping.** The `interrupts` property is read as raw
  cells; interpreting them (which PPI, which IRQ, which flags) is the
  caller's job. A full interrupt specifier resolver needs the
  interrupt-parent phandle chain, which is complexity without a second
  board to justify it.
- **No aliases resolution.** `/aliases` maps friendly names like
  `serial0` to paths; we search by `compatible` string instead, which
  is how Linux finds devices in practice.
- **No overlays.** Dynamic device tree overlays (adding nodes at runtime)
  are a feature for hot-pluggable hardware. Not relevant here.

These grow when a second board (Apple Silicon, M7) forces them — the
same principle as the HAL: grow abstractions when a second implementation
demands them, and not one milestone earlier.

---

## 6. The honest caveat

QEMU's DTB is 1 MiB — padded with zeros after the actual data (the
structure block is ~7 KiB, the strings ~462 bytes). The `totalsize`
field says 1 MiB because QEMU rounds up, and `Fdtr::new` accepts the
full blob. This is correct: the spec says `totalsize` is the size of the
entire blob, including padding. The bounds checks in every read method
use `totalsize` as the ceiling, so the padding is never dereferenced as
structure tokens — it's just harmless space that the parser never walks
into.