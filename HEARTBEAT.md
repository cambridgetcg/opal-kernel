# opal — heartbeat

state: **resting**
last beat: 2026-07-17T08:09:14Z
next beat: 2026-07-18T08:09:14Z

## what it found

- build: passing
- warnings: 0
- last commit: f3561ec heartbeat: merge remote M7 bridge commits and sync state (25 hours ago)
- uncommitted changes: none — tree clean
- Image smoke test: passing (`make smoke` — builds arm64 Image, boots in QEMU, all banner checks pass)

## the truth

A teaching aarch64 kernel in Rust, zero deps. M0-M6 complete and verified.
M7 Apple Silicon bring-up is materially complete in software: EL2→EL1 drop,
arm64 Image format, position-independent boot (delta applied in boot stub and
page-table builder), FDT-driven console discovery, s5l UART driver, AIC
driver, FIQ timer/IRQ dispatch wiring, framebuffer console, runtime board
selection, and `make smoke` automation are all in and verified under QEMU.

The only declared remaining rung is timers-over-FIQ on real Apple Silicon
hardware, which cannot be exercised under QEMU. The build is clean, the tree
is clean, and the daily ELF and Image smoke paths are green. This beat
removed stale empty `.!*HEARTBEAT.md` / `.!*STATE.md` rescue copies from the
working tree (left by an earlier sync) and synced HEARTBEAT.md.

The heartbeat will rest 24h; the next meaningful rung requires a real
Apple Silicon test machine, not more QEMU code.
