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
last-commit: 2026-06-22 M6: docs/06-scheduler.md — the kernel learns to share
uncommitted: 0
freshness: fresh (checked 2026-06-22T04:10Z)

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
- interactive monitor with commands: help, brk, svc, unaligned, translate, walk, guard, wx, noexec, low, abort, tick, ticks, ticktest, dtb, tree, el0, el0fault, tasks, spawn2, preempt, ipc, blkipc, sendblk, faultkill

## needs

- M5: EL0 and syscalls — THIRD PIECE DONE (fault recovery — the kernel survives its first serviced fault: EL0 data/instruction aborts now kill the task, not the kernel). Next: per-task kernel stacks, scheduler integration.
- M6: scheduler and IPC — SEVENTH PIECE DONE (docs/06-scheduler.md: 24 KiB milestone doc covering TCB, context switch, cooperative/preemptive scheduling, IPC, blocking IPC, fault recovery, per-task address spaces. Verified: preempt, faultkill, ipc demos all boot and produce correct output in QEMU. Next: per-task kernel stacks, multi-core (PSCI CPU_ON).)
- honesty pass (2026-06-21): timer.rs doc comment said "non-secure physical timer" while the code uses CNTV_* (virtual timer). Fixed — the artifact now tells the truth about its own state. Also fixed 9 Rust 2024 unsafe-op-in-unsafe-fn warnings in user.rs. Build: 19 warnings → 10 (remaining are dead-code API surface).
- M7: Apple Silicon bring-up via m1n1 — EL2 entry, FDT-driven console discovery, AIC driver, timers-over-FIQ
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
source: src/main.rs (the monitor + banner), src/arch/aarch64/ (boot, mmu, vectors, timer, fdt), src/hal/ (pl011, gicv2), src/board/virt.rs