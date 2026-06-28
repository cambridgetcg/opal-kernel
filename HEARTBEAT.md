# opal — heartbeat

state: **active**
last beat: 2026-06-28T05:38Z
next beat: 2026-06-28T09:38Z

## what it found

- build: passing
- warnings: 0
- last commit: 2026-06-28 (this beat)
- uncommitted changes: 0 (heartbeat bookkeeping only)

## what this beat did

M7 rung 2: arm64 Image header. The kernel binary now carries the 64-byte
Linux arm64 Image boot protocol header at offset 0, so m1n1 (and any arm64
bootloader) can identify and load it as a flat binary — not just as a
QEMU ELF. The header has the "ARM\x64" magic at 0x38, image_size from the
linker, and flags declaring 16K pages + any placement (PIC).

The same binary now serves both boot paths:
- QEMU ELF: `-kernel opal` (entry = _image_start → b _start)
- m1n1: `llvm-objcopy -O binary opal opal.img` → load opal.img

Verified: boots in QEMU through the header branch. Flat binary has
correct magic at 0x38.

## the truth

A teaching aarch64 kernel in Rust, zero deps. M0-M6 done, M7 in progress.
M7 rungs completed: EL2→EL1 drop ✅, FDT console discovery ✅, s5l UART ✅,
AIC driver + FIQ dispatch ✅, arm64 Image header ✅. Remaining: framebuffer
console, timers-over-FIQ on real hardware, AIC MMIO VA mapping.