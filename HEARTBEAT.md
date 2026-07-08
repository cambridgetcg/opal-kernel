# opal — heartbeat

state: **active**
last beat: 2026-07-08T07:40:27Z
next beat: 2026-07-08T11:40:27Z

## what it found

- build: passing (0 warnings)
- last commit: 2026-07-08 (this beat)
- boot: QEMU ELF + Image both verified

## what it did

M7 rung: runtime-base s5l UART console. The Apple board's `Console`
type was a placeholder (`S5lUart<0x0>`) because the s5l UART driver
uses a const-generic base, but on Apple Silicon the base is
FDT-discovered at runtime. Added `RuntimeS5lUart` — a runtime-base
wrapper that carries the MMIO base as a field, implements
`core::fmt::Write` with the same byte path, and is constructible
after the FDT parser finds `apple,s5l-uart`.

`board/apple.rs` now has a real `Console = RuntimeS5lUart` type, a
`console()` constructor, and `init()` discovers the UART base from
the FDT (same pattern as the AIC discovery). Dormant on QEMU
(`board::virt` is selected); structurally complete for the first
Apple boot.

## the truth

A teaching aarch64 kernel in Rust, zero deps. Seven milestones: M0-M6
complete, M7 (Apple Silicon bring-up) in progress. The build passes
with 0 warnings. The kernel boots on both QEMU ELF and flat Image
paths.