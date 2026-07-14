# opal — heartbeat

state: **active**
last beat: 2026-07-14T08:01:52Z
next beat: 2026-07-14T12:01:52Z

## what it found

- build: passing
- warnings: 0
- last commit: 14af0dd heartbeat: sync STATE.md and remove stale rescue files (24 hours ago)
- uncommitted changes: 4 (heartbeat state files only)
- Image smoke test: passing (`make smoke` — builds arm64 Image, boots in QEMU, greps banner)

## the truth

A teaching aarch64 kernel in Rust, zero deps. M0-M6 complete and verified.
M7 Apple Silicon bring-up is in progress: EL2→EL1 drop, arm64 Image format,
position-independent boot, FDT-driven console discovery, s5l UART driver,
AIC driver, framebuffer console, and runtime board selection are all in.
The remaining M7 rung is timers-over-FIQ on real Apple Silicon hardware,
which cannot be exercised under QEMU. The build is passing with 0 warnings.
This beat added an automated `make smoke` target so every future beat can
verify the M7 Image boot path in QEMU before declaring it healthy.
