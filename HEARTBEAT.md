# opal — heartbeat

state: **active**
last beat: 2026-07-15T08:02:51Z
next beat: 2026-07-15T12:02:51Z

## what it found

- build: passing
- warnings: 0
- last commit: 8a8187c heartbeat: add make smoke target for M7 Image boot path; sync state and remove rescue files (24 hours ago)
- uncommitted changes: 22 stale heartbeat rescue files (empty `.!*.HEARTBEAT.md` copies); cleaned
- Image smoke test: passing (`make smoke` — builds arm64 Image, boots in QEMU, greps banner)

## the truth

A teaching aarch64 kernel in Rust, zero deps. M0-M6 complete and verified.
M7 Apple Silicon bring-up is in progress: EL2→EL1 drop, arm64 Image format,
position-independent boot, FDT-driven console discovery, s5l UART driver,
AIC driver, framebuffer console, and runtime board selection are all in.
The remaining M7 rung is timers-over-FIQ on real Apple Silicon hardware,
which cannot be exercised under QEMU. The build is passing with 0 warnings.
This beat found the working tree cluttered by 22 empty rescue copies of
HEARTBEAT.md, removed them, updated the tracked HEARTBEAT.md, synced
STATE.md, and re-verified the Image smoke path.
