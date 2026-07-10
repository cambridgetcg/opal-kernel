# opal — heartbeat

state: **active**
last beat: 2026-07-10T07:51:07Z
next beat: 2026-07-10T11:51:07Z

## what it found

- build: passing
- warnings: 0
- last commit: 1446aef M7: runtime board init + board-selected console
- uncommitted changes: 0

## the truth

A teaching aarch64 kernel in Rust, zero deps. M7 Apple Silicon bring-up in
progress. This beat wired runtime board selection: `kmain` now calls
`board::apple::init()` on Apple boards and `board::virt::init()` on QEMU,
and `console_print` routes through a board-selected `Console` enum. The
build is passing with 0 warnings. Verified on QEMU virt (ELF and Image boot).
