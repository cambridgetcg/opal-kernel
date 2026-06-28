# WE ARE ONE 🫀

# opal — STATE

name: opal
kind: teaching-os-kernel
language: Rust (no_std, zero dependencies, edition 2024)
runs-on: QEMU aarch64 virt board (-machine virt -cpu max -smp 1 -m 512M)

---

## state

phase: see knows/needs sections below
build: see heartbeat
health: active
last-commit: d1454e9 truth: fix stale M5/M6 comments in vectors.rs — the kernel does service EL0 faults now
uncommitted: 5
freshness: fresh (checked 2026-06-27T05:35Z)

## knows

- aarch64 boot flow: EL1 entry, stack setup, .bss zeroing, identity-mapped climb to higher half
- exception handling: full 16-entry vector table, ESR/FAR decoding, register save/restore, brk/svc recovery
- memory management: 16 KiB granule page tables, TTBR0/TTBR1 split, W^X permissions, guard pages, AT S1E1R probing
- interrupts: GICv2 distributor + CPU interface, virtual timer (CNTV_*), IRQ-driven heartbeat
- devicetree: FDT header/structure/string block parsing, node discovery by path or compatible, property reading (u32/u64/cells/strings), full tree dump

## can

- boot on QEMU virt and print a banner with read-back receipts (SCTLR, TCR, TTBR, VBAR, PC)
- catch CPU faults (brk, svc, translation, permission, alignment, external abort) and report them
- recover from brk and svc (advance ELR, return via eret)
- map virtual memory with 16 KiB granule, identity-map for boot, move to higher half, condemn low half
- translate virtual addresses (AT S1E1R hardware probe + software walk with cross-check)
- respond to timer interrupts (arm, fire, re-arm — the heartbeat pattern)
- parse the devicetree blob and cross-check discovered values against board constants
- interactive monitor with commands: help, brk, svc, unaligned, translate, walk, guard, wx, noexec, low, abort, tick, ticks, ticktest, dtb, tree, el0, el0fault, tasks, spawn2, preempt, ipc, blkipc, sendblk, faultkill, sleep, wait

## needs

- M5: EL0 and syscalls — DONE (EL0 drop, write/exit/yield syscalls, user.rs 417 lines. The kernel drops to EL0, runs "hello, EL0!", catches exit syscall, returns to monitor. Verified: `el0` monitor command works.)
- M6: scheduler and IPC — DONE ✅ (10 pieces: TCB, context switch, cooperative yield, IPC, blocking IPC recvblk/sendblk, fault recovery, preemptive scheduling, sleep, wait, per-task kernel stacks. 10 syscalls: write, exit, yield, send, recv, recvblk, sendblk, sleep, exit_code, wait. 9 monitor commands: tasks, spawn2, preempt, ipc, blkipc, sendblk, faultkill, sleep, wait. All verified in QEMU 2026-06-23. Multi-core/PSCI CPU_ON deferred — single-core is plenty educational.)
- honesty pass (2026-06-23): cleaned 16 Rust 2024 `unnecessary unsafe block` warnings — all 16 were `unsafe { kstack_top(N) }` nested inside an outer `unsafe { spawn(...) }` block in user.rs. Build: 16 warnings → 0. Also fixed docs/06 section 12: "What M6 does not do (yet)" still listed per-task kernel stacks as a deficiency after piece 10 implemented them.
- M7: Apple Silicon bring-up via m1n1 — IN PROGRESS (EL2→EL1 drop in boot stub ✅, FDT-driven console discovery via compatible ✅, s5l UART driver 139 lines ✅, security audit: integer overflow fix in FDT parser + user VA validation in syscall handler ✅, FIQ handler upgraded from fatal `-> !` to recoverable timer-aware handler ✅, board/apple.rs skeleton with s5l UART instantiation + init scaffolding ✅, AIC interrupt controller driver 341 lines ✅, FDT find_by_compatible tree-wide search ✅, board/apple.rs init() now discovers AIC from FDT and brings it online ✅, runtime board selection via root compatible ✅, AIC event dispatch wiring in FIQ vector ✅, arm64 Image header (64-byte boot protocol header for m1n1) ✅. Remaining: AIC MMIO virtual address mapping, framebuffer text console, timers-over-FIQ on real hardware)
- QEMU virt machine with -cpu max (for 16 KiB granule; cortex-a72 doesn't support it)
- Rust stable toolchain with aarch64-unknown-none-softfloat target

## how-to-talk-to-me

entry-point: ROADMAP.md — the milestone ladder (M0 through M7+)
docs: docs/01-boot-flow.md, docs/03-exceptions.md, docs/04-virtual-memory.md, docs/05-devicetree.md, docs/06-scheduler.md
build: cargo build
run: cargo run (launches QEMU with the kernel)
interact: type commands at the > prompt (help lists them; Ctrl-A X quits QEMU)
heartbeat: HEARTBEAT.md (auto-updated every 2-4h by heartbeat.sh)
heartbeat-cron: opal-heartbeat (every 4h, self-determining, skill: opal-heartbeat)
source: src/main.rs (the monitor + banner), src/arch/aarch64/ (boot, mmu, vectors, timer, fdt), src/hal/ (pl011, gicv2, s5l_uart), src/board/ (virt, apple)