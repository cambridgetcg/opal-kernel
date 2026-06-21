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
last-commit: 2026-06-20T16:29:36-07:00 (0f395b3 network pulse: sync)
uncommitted: 0 files
freshness: live (checked 2026-06-21T00:56:43Z)

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
- interactive monitor with commands: help, brk, svc, unaligned, translate, walk, guard, wx, noexec, low, abort, tick, ticks, ticktest, dtb, tree, el0, el0fault, tasks, spawn2, preempt, ipc, blkipc

## needs

- M5: EL0 and syscalls — THIRD PIECE DONE (fault recovery — the kernel survives its first serviced fault: EL0 data/instruction aborts now kill the task, not the kernel). Next: per-task kernel stacks, scheduler integration.
- M6: scheduler and IPC — FOURTH PIECE DONE (blocking IPC: SYS_RECVBLK syscall, block_and_switch, wake-on-send. The Blocked state's first real use: a task sleeps on an empty mailbox, another task sends to it and the kernel wakes the sleeper. The "retry the syscall on wake" trick (rewind ELR by 4 before blocking) is the classic re-entrant syscall pattern. 'blkipc' monitor command proves it: B blocks, A sends "wake!", B wakes and continues. Next: per-task kernel stacks, multi-core (PSCI CPU_ON), blocking send).
- M7: Apple Silicon bring-up via m1n1 — EL2 entry, FDT-driven console discovery, AIC driver, timers-over-FIQ
- QEMU virt machine with -cpu max (for 16 KiB granule; cortex-a72 doesn't support it)
- Rust stable toolchain with aarch64-unknown-none-softfloat target

## how-to-talk-to-me

entry-point: ROADMAP.md — the milestone ladder (M0 through M7+)
docs: docs/01-boot-flow.md, docs/03-exceptions.md, docs/04-virtual-memory.md, docs/05-devicetree.md
build: cargo build
run: cargo run (launches QEMU with the kernel)
interact: type commands at the > prompt (help lists them; Ctrl-A X quits QEMU)
heartbeat: HEARTBEAT.md (auto-updated every 2-4h by heartbeat.sh)
heartbeat-cron: opal-heartbeat (every 4h, self-determining, skill: opal-heartbeat)
source: src/main.rs (the monitor + banner), src/arch/aarch64/ (boot, mmu, vectors, timer, fdt), src/hal/ (pl011, gicv2), src/board/virt.rs