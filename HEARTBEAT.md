# opal — heartbeat

state: **active**
last beat: 2026-07-06T07:15:00Z
next beat: 2026-07-06T11:15:00Z

## what it found

- build: passing
- warnings: 0
- last commit: 2026-07-06 15efcc2 M7: genuinely position-independent boot
- uncommitted changes: 2 (STATE.md, HEARTBEAT.md)

## what it did

Completed the genuinely position-independent boot rung of M7. The boot
stub already discovered its load PA via `adr _image_start` (x22), but
every literal-pool address was still converted to a link-time PA by
subtracting KERNEL_BASE, assuming load_pa == 0x4020_0000. On Apple
Silicon via m1n1, the loader places the image elsewhere.

This beat adds the relocation delta (x23 = load_pa - link_pa) and
applies it to every physical address the boot stub computes:
- VBAR_EL2, __stack_top, __bss_start/end (stack, .bss zeroing)
- opal_build_tables, opal_mmu_enable, __vectors (calls, vector install)
- The table builder's image_base (passed as arg0 to opal_build_tables)

The table builder now accepts delta, subtracts it from linker symbols
to recover link-time PAs for attribute comparisons, and adds it to
kernel-image page output PAs so the linear map points at actual bytes.

On QEMU (delta=0) every add is a no-op. Verified: ELF boot, Image boot,
brk, svc, tick, ticks, ticktest, el0 all produce identical output.

## the truth

A teaching aarch64 kernel in Rust, zero deps. Seven milestones: boots,
catches faults, maps its world, has a heartbeat, reads the handoff,
drops to EL0, schedules tasks. M7 (Apple Silicon bring-up) is in
progress — the kernel now handles EL2 entry, flat Image format, FDT
handoff, and genuinely position-independent boot. The build is passing
with 0 warnings.