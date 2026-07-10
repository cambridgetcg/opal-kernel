//! Opal — milestone 7: Apple Silicon bring-up (in progress).
//!
//! This file is still most of the "operating system": print macros backed
//! by a lock, the banner (which now reads the MMU's registers back as
//! receipts AND cross-checks the devicetree against board constants), a
//! monitor whose commands poke the address space from every angle, and a
//! panic handler. The journey from power-on to `kmain` — which since M2
//! includes building page tables and climbing to the higher half — is
//! docs/01-boot-flow.md and docs/04-virtual-memory.md; the page tables
//! and the enable ceremony are arch/aarch64/mmu.rs; the exception
//! machinery is arch/aarch64/vectors.rs and docs/03; the devicetree
//! parser is arch/aarch64/fdt.rs and docs/05-devicetree.md.

// A kernel cannot use `std` — std assumes an OS underneath (files, threads,
// an allocator, a way to exit). We *are* the OS. `core` is the dependency-
// free heart of the standard library and is all we get.
#![no_std]
// And there is no `fn main()`: nothing would call it. Our real entry point
// is `_start` in arch/aarch64/boot.rs, chosen by ENTRY(_start) in linker.ld.
#![no_main]

// ---------------------------------------------------------------------------
// Print macros
// ---------------------------------------------------------------------------
//
// These sit ABOVE the `mod` declarations on purpose: `macro_rules!` macros
// are visible only to code that comes *after* them in source order — and
// that includes the files behind `mod arch;` and friends. In M0 they lived
// at the bottom and nobody noticed, because only this file printed. M1's
// exception handlers (arch/aarch64/vectors.rs) are the second user, so the
// macros move up where the whole crate can see them.

/// `print!` — exactly like std's, but the bytes go to the UART.
macro_rules! print {
    ($($arg:tt)*) => { $crate::console_print(format_args!($($arg)*)) };
}

/// `println!` — `print!` plus a newline (the UART driver adds the `\r`).
macro_rules! println {
    () => { print!("\n") };
    ($($arg:tt)*) => { print!("{}\n", format_args!($($arg)*)) };
}

mod arch;
mod board;
mod hal;
mod sched;
mod sync;

use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

// ---------------------------------------------------------------------------
// Printing: one lock, one escape hatch
// ---------------------------------------------------------------------------

/// The console lock, fulfilling the promise hal/pl011.rs made in M0. One
/// `print!` = one critical section, so two printers can interleave whole
/// messages but never the bytes inside them. The lock wraps the *call*,
/// not the driver — locking inside `Pl011` would also drag the monitor's
/// blocking `read_byte` loop into a critical section, which is the classic
/// way to deadlock a console (see docs/03-exceptions.md).
static CONSOLE_LOCK: sync::SpinLock = sync::SpinLock::new();

/// Are we mid-catastrophe? Set while an exception or panic is being
/// reported. While true, `console_print` skips the lock entirely: the
/// interrupted code may *hold* it, and a fault report that deadlocks
/// waiting for its victim is the worst possible failure mode. Linux plays
/// the identical trick under the name `oops_in_progress` (set by
/// `bust_spinlocks()`): during an oops, console drivers try the lock and
/// write anyway if they can't get it — possibly-garbled output beats
/// guaranteed silence.
///
/// `Relaxed` ordering throughout: this flag guards no data, it only picks
/// which print path runs, and there is exactly one core to see it.
static OOPS: AtomicBool = AtomicBool::new(false);

/// Enter emergency-print mode. Returns whether it was already on — the
/// exception dispatcher uses that as its nested-fault tripwire.
fn oops_enter() -> bool {
    OOPS.swap(true, Ordering::Relaxed)
}

/// Leave emergency-print mode (recovered faults only; panics never do).
fn oops_exit() {
    OOPS.store(false, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Board-selected console
// ---------------------------------------------------------------------------

/// The board kind selected at boot from the FDT root `compatible`.
/// 0 = Virt (default), 1 = Apple. Stored as an integer because the two
/// board modules have different console types and we cannot store an enum
/// of different-sized structs in a plain static without `OnceCell`.
static BOARD_KIND: AtomicU8 = AtomicU8::new(0);

/// Remember which board we are running on. Called once in `kmain` after
/// the FDT parser decides.
fn set_board_kind(kind: board::BoardKind) {
    let v = match kind {
        board::BoardKind::Virt => 0,
        board::BoardKind::Apple => 1,
    };
    BOARD_KIND.store(v, Ordering::Relaxed);
}

/// The console type selected by the current board. Both implement
/// `core::fmt::Write` and have `read_byte` / `write_byte`.
#[derive(Clone, Copy)]
enum Console {
    Virt(board::virt::Console),
    Apple(board::apple::Console),
}

impl core::fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        match self {
            Console::Virt(c) => c.write_str(s),
            Console::Apple(c) => c.write_str(s),
        }
    }
}

impl Console {
    fn write_byte(self, b: u8) {
        match self {
            Console::Virt(c) => c.write_byte(b),
            Console::Apple(c) => c.write_byte(b),
        }
    }

    fn read_byte(self) -> u8 {
        match self {
            Console::Virt(c) => c.read_byte(),
            Console::Apple(c) => c.read_byte(),
        }
    }
}

/// Construct the board-selected console. After `set_board_kind` this
/// returns the right driver; before that it returns the Virt console,
/// which is the safe fallback on QEMU.
fn console() -> Console {
    match BOARD_KIND.load(Ordering::Relaxed) {
        1 => Console::Apple(board::apple::console()),
        _ => Console::Virt(board::virt::console()),
    }
}

/// The plumbing under `print!`/`println!`: render `format_args!` output
/// straight into the board-selected console. No allocation, no buffering.
#[doc(hidden)]
fn console_print(args: core::fmt::Arguments) {
    if OOPS.load(Ordering::Relaxed) {
        let _ = console().write_fmt(args);
        return;
    }
    CONSOLE_LOCK.with(|| {
        let _ = console().write_fmt(args);
    });
}

// ---------------------------------------------------------------------------
// Devicetree sniffing
// ---------------------------------------------------------------------------

/// Every flattened devicetree (FDT/DTB) begins with this magic number,
/// stored big-endian in memory.
const FDT_MAGIC: u32 = 0xd00d_feed;

/// Does `addr` (a PHYSICAL address) plausibly point at a devicetree? We
/// only dereference addresses inside RAM — but the reason changed in M2.
/// M0/M1 guarded against the *bus* (reading an unbacked physical address
/// took an external abort); now the page tables guard for us, and only
/// mapped RAM is dereferenceable at all. The kernel reads the candidate
/// through its higher-half alias, where the DTB window is mapped
/// read-only Normal memory.
///
/// The 4-alignment check survives with its meaning inverted: since M2 a
/// misaligned u32 read of Normal memory would *succeed* (the monitor's
/// `unaligned` command now proves that daily), so alignment is no longer
/// a survival rule here — it stays because the devicetree spec requires
/// a 4-aligned blob, making a ragged address disqualifying on its own.
fn fdt_at(addr: u64) -> bool {
    const RAM_END: u64 = (board::virt::RAM_BASE + board::virt::RAM_SIZE) as u64;
    if addr < board::virt::RAM_BASE as u64 || addr >= RAM_END || addr % 4 != 0 {
        return false;
    }
    // SAFETY: addr is inside RAM, whose higher-half alias is mapped for
    // the kernel's lifetime (read-only is plenty: this is a read).
    // Volatile, because this memory was written by QEMU, not by Rust
    // code the compiler knows about.
    let va = arch::aarch64::mmu::phys_to_virt(addr as usize);
    let raw = unsafe { core::ptr::with_exposed_provenance::<u32>(va).read_volatile() };
    // The FDT stores all integers big-endian; we are little-endian.
    u32::from_be(raw) == FDT_MAGIC
}

// ---------------------------------------------------------------------------
// Kernel main
// ---------------------------------------------------------------------------

/// First kernel function. Called by `_start_rust` with `x0` exactly as
/// the bootloader left it, and `load_pa` — the kernel's own physical load
/// address, discovered by `adr _image_start` in the boot stub.
///
/// Returns `!`: there is nothing to return *to*. The call stack below us
/// is three frames of assembly ending in a parking loop.
fn kmain(x0: u64, load_pa: u64) -> ! {
    // Catching faults is the first ability worth having: install the
    // vector table before printing anything, so that even a banner-era
    // bug gets a report instead of M0's silent hang. (The boot stub
    // pre-armed VBAR already; this is the third, PC-relative derivation
    // of the same address — the banner reads it back below.)
    arch::aarch64::vectors::install();

    // Board init is deferred until after the FDT is parsed and
    // board::which selects the right module (virt vs apple). See the
    // FDT block below.

    // Pull up the ladder: TTBR0 gets the empty root, and the low half —
    // the identity world the boot stub climbed through — stops
    // translating. ORDER IS LOAD-BEARING in this function: everything
    // before this line may still lean on low addresses; nothing after it
    // may. (The console const moved upstairs with us: board::virt::UART0_VA.)
    arch::aarch64::mmu::condemn_low_half();

    println!();
    println!("opal — milestone 7: Apple Silicon bring-up ⚙️");
    println!("--------------------------------------------------");

    // Privilege check. QEMU virt without virtualization=on/secure=on has
    // no EL2/EL3, so we expect EL1. (On Apple Silicon via m1n1: EL2.)
    println!("current EL : EL{}", arch::aarch64::current_el());

    // Every MMU line below is a READ-BACK: the register's testimony, not
    // the enable ceremony's intentions.
    println!(
        "mmu        : on — SCTLR_EL1 = {:#x} (M, C, I — read back, not assumed)",
        arch::aarch64::mmu::sctlr()
    );
    let tcr = arch::aarch64::mmu::tcr();
    println!(
        "granule    : 16 KiB, 48-bit VA — TCR_EL1 = {tcr:#x} (TG0=16K, TG1=16K: different encodings, both checked)"
    );
    // PARange as the CPU reports it vs IPS as the ceremony programmed it:
    // QEMU's -cpu max offers more physical bits than DS=0 can emit, so
    // the clamp to 48 is visible right here (on real M1: 36-bit, no clamp).
    let parange = arch::aarch64::mmu::id_aa64mmfr0() & 0xF;
    let ips = (tcr >> 32) & 0b111;
    println!(
        "pa range   : PARange 0b{parange:03b} ({} bits) -> IPS 0b{ips:03b} ({} bits; DS=0 caps the output at 48)",
        parange_bits(parange),
        parange_bits(ips)
    );
    println!(
        "ttbr1      : {:#x} — the kernel's tree (a physical address: the walker speaks PA)",
        arch::aarch64::mmu::ttbr1()
    );
    // The condemnation, verified rather than asserted: ask the hardware
    // to translate M1's home address and print its refusal.
    let low_probe = match arch::aarch64::mmu::translate(0x4020_0000) {
        Err(_) => "0x40200000 no longer translates",
        Ok(_) => "0x40200000 STILL TRANSLATES — condemnation failed?!",
    };
    println!(
        "ttbr0      : {:#x} — empty root; ground floor condemned (AT probe: {low_probe})",
        arch::aarch64::mmu::ttbr0()
    );
    // The move, proven by the program counter itself.
    println!(
        "pc         : {:#x} — kmain itself runs in the higher half",
        arch::aarch64::here()
    );
    println!(
        "vectors    : VBAR_EL1 = {:#x} (16-entry table live, upstairs)",
        arch::aarch64::vectors::vbar()
    );
    println!(
        "guard      : 16 KiB unmapped below the stack - overflow now faults instead of eating .bss (M0's debt, paid)"
    );

    // M3's heartbeat: the architectural timer.
    let freq = arch::aarch64::timer::frequency();
    println!(
        "timer      : CNTV @ {freq} Hz - the virtual timer (PPI {})",
        hal::gicv2::TIMER_IRQ
    );

    // Board-specific interrupt controller info is printed after the FDT
    // parser selects the board (see below).

    // What did the bootloader hand us? Under the Linux boot protocol (and
    // m1n1) x0 is a physical FDT pointer. QEMU sets NO registers for
    // non-Linux ELF payloads, so here we expect 0 — and we report whatever
    // we actually find rather than what folklore says we should find.
    println!("x0 at entry: {x0:#x}");

    // M7: where did the loader actually place us? The boot stub captures
    // this with `adr _image_start` (step 2b) — PC-relative, so it reads
    // the *actual* load PA, not the link address. On QEMU it matches
    // 0x4020_0000; on Apple Silicon via m1n1 it is wherever m1n1 chose.
    // The Image header claims "any placement" (flags bit 3 = 1); this line
    // is the honest report of whether that claim is true yet. When
    // load_pa != link_pa, the kernel knows it is displaced — the
    // foundation for the full PIC fix (relocating the literal pools,
    // adjusting the table builder's image_base).
    const LINK_PA: u64 = 0x4020_0000;
    let placement = if load_pa == LINK_PA {
        "matches link address (0x4020_0000) — PIC not yet exercised"
    } else {
        "DISPLACED from link address — PIC boot required (not yet implemented)"
    };
    println!("load PA   : {load_pa:#x} — {placement}");

    // M7: the relocation delta, computed honestly. When the loader placed
    // us at the link address, delta is 0 and every `add xN, xN, x23` in
    // the boot stub was a no-op. When displaced, delta is what the boot
    // stub added to every literal-pool address and what the table builder
    // added to every kernel-image page's output PA — the difference
    // between mapping the real bytes and mapping the link-time ghosts.
    let delta = load_pa.wrapping_sub(LINK_PA);
    if delta != 0 {
        println!("reloc delta: {delta:#x} — boot stub relocated {delta:#x} bytes from link PA");
    }

    if fdt_at(x0) {
        println!("fdt at x0  : yes — magic {FDT_MAGIC:#x} found (Linux-protocol style handoff)");
    } else {
        println!("fdt at x0  : no  (expected under QEMU ELF boot: x0 is just QEMU's reset zero)");
    }

    // QEMU's bare-metal ELF convention instead: DTB at the start of RAM,
    // provided our lowest load address left room for it (we load at
    // 0x4020_0000; QEMU wants >= 1 MiB of room — see linker.ld). Read via
    // the higher-half alias: the PA itself stopped translating above.
    let ram_base = board::virt::RAM_BASE as u64;
    if fdt_at(ram_base) {
        println!(
            "fdt at RAM base (PA {ram_base:#x}, read via its higher-half alias): yes — QEMU's bare-metal DTB placement"
        );
    } else {
        println!(
            "fdt at RAM base (PA {ram_base:#x}, read via its higher-half alias): no — unexpected, check linker.ld load address"
        );
    }

    // M4: parse the devicetree for real. The FDT is the honest source for
    // every address the kernel uses — on Apple Silicon (M7) the UART base
    // genuinely differs per SoC, and the FDT is the only way to know.
    //
    // Where is the FDT? Two conventions, both honest:
    //
    // - **Linux boot protocol / m1n1 / QEMU Image boot**: x0 holds the
    //   physical address of the FDT. This is the path that will be live on
    //   Apple Silicon — m1n1 emits a standard arm64 Linux handoff.
    // - **QEMU ELF boot (the daily dev path)**: x0 is 0 (QEMU sets no
    //   registers for non-Linux ELF payloads); the DTB sits at the start
    //   of RAM (0x4000_0000), placed there by QEMU's arm_load_dtb.
    //
    // We prefer x0 (the protocol path) when it points at a valid FDT, and
    // fall back to RAM base (the QEMU-ELF path) otherwise. This is the
    // bridge: the same code that parses QEMU's ELF-boot DTB today will
    // parse m1n1's handoff tomorrow, with no special case for "Apple."
    let fdt_pa = if fdt_at(x0) { x0 } else { ram_base };
    if fdt_at(fdt_pa) {
        if let Some(fdt) = arch::aarch64::fdt::Fdtr::new(fdt_pa as usize) {
            println!(
                "dtree     : parsed — {} bytes, boot CPU {}",
                fdt.totalsize(),
                fdt.boot_cpuid()
            );

            // M7: runtime board selection. The root node's `compatible`
            // string declares what this machine is. We read it and select
            // the board module — today this is a diagnostic (kmain still
            // uses board::virt for everything), but the selection is the
            // bridge: once the Apple board's console wiring is complete,
            // this is where the fork happens.
            let board_kind = board::which(Some(&fdt));
            set_board_kind(board_kind);
            let root_compat = fdt.root().map(|n| fdt.compatible(&n)).unwrap_or("");
            println!(
                "board     : {:?} (root compatible: \"{}\")",
                board_kind, root_compat
            );

            // Initialize the selected board: GIC on virt, AIC + s5l UART
            // (+ framebuffer) on Apple. From here on console_print routes
            // through the board-selected console.
            match board_kind {
                board::BoardKind::Virt => board::virt::init(),
                board::BoardKind::Apple => board::apple::init(Some(&fdt)),
            }

            // Board-specific interrupt controller report.
            match board_kind {
                board::BoardKind::Virt => {
                    let gic_typer = board::virt::gic().typer();
                    println!(
                        "gic        : GICv2 - GICD at {:#x}, GICC at {:#x} (TYPER={:#x})",
                        board::virt::GICD_VA,
                        board::virt::GICC_VA,
                        gic_typer
                    );
                }
                board::BoardKind::Apple => {
                    let aic_base = board::apple::aic_base();
                    if aic_base != 0 {
                        let aic = crate::hal::aic::Aic::new(aic_base);
                        println!(
                            "aic        : Apple AIC at {:#x} — NR_IRQ {}, WHOAMI {}, CONFIG {:#x}",
                            aic_base,
                            aic.cached_nr_irq(),
                            aic.whoami(),
                            aic.config()
                        );
                    } else {
                        println!("aic        : Apple AIC not discovered");
                    }
                }
            }

            // /memory — the machine's RAM, as QEMU declares it. Cross-
            // check against board::virt's hardcoded RAM_BASE/RAM_SIZE.
            if let Some(mem) = fdt.find("/memory") {
                if let Some(reg) = fdt.prop(&mem, "reg") {
                    if reg.len() >= 16 {
                        let base = u64::from_be_bytes(reg[0..8].try_into().unwrap());
                        let size = u64::from_be_bytes(reg[8..16].try_into().unwrap());
                        let base_match = base as usize == board::virt::RAM_BASE;
                        let size_match = size as usize == board::virt::RAM_SIZE;
                        println!(
                            "  /memory : base {base:#x} ({}), size {size:#x} ({})",
                            if base_match {
                                "matches board const"
                            } else {
                                "DIFFERS from board const!"
                            },
                            if size_match {
                                "matches board const"
                            } else {
                                "DIFFERS from board const!"
                            },
                        );
                    }
                }
            } else {
                println!("  /memory : not found");
            }

            // The interrupt controller. The `compatible` string tells
            // us which GIC driver to use; the `reg` property gives its
            // MMIO addresses. Cross-check against our GICv2 constants.
            if let Some(gic) = fdt.find("/intc") {
                let compat = fdt.compatible(&gic);
                if !compat.is_empty() {
                    // Read the GIC's reg property: two (addr, size) pairs
                    // for GICD and GICC. With #address-cells=2 and
                    // #size-cells=2, each pair is 16 bytes.
                    if let Some(reg) = fdt.prop(&gic, "reg") {
                        if reg.len() >= 32 {
                            let gicd_base = u64::from_be_bytes(reg[0..8].try_into().unwrap());
                            let gicc_base = u64::from_be_bytes(reg[16..24].try_into().unwrap());
                            let gicd_match = gicd_base as usize == hal::gicv2::GICD_BASE;
                            let gicc_match = gicc_base as usize == hal::gicv2::GICC_BASE;
                            println!(
                                "  /intc   : \"{compat}\" — GICD {gicd_base:#x} ({}), GICC {gicc_base:#x} ({})",
                                if gicd_match { "matches" } else { "DIFFERS!" },
                                if gicc_match { "matches" } else { "DIFFERS!" },
                            );
                        }
                    } else {
                        println!("  /intc   : \"{compat}\" (reg not found)");
                    }
                }
            } else {
                println!("  /intc   : not found");
            }

            // The UART — our PL011. Cross-check the address.
            if let Some(uart) = fdt.find("/pl011") {
                let compat = fdt.compatible(&uart);
                if let Some(reg) = fdt.prop(&uart, "reg") {
                    if reg.len() >= 16 {
                        let uart_base = u64::from_be_bytes(reg[0..8].try_into().unwrap());
                        let uart_match = uart_base as usize == board::virt::UART0_BASE;
                        println!(
                            "  /pl011  : \"{compat}\" — base {uart_base:#x} ({})",
                            if uart_match {
                                "matches board const"
                            } else {
                                "DIFFERS!"
                            },
                        );
                    }
                } else if !compat.is_empty() {
                    println!("  /pl011  : \"{compat}\" (reg not found)");
                }
            } else {
                println!("  /pl011  : not found");
            }

            // The architectural timer. Its `interrupts` property lists
            // four PPIs; we use the virtual timer (PPI 27, the third
            // entry: type=1 PPI, number=11, flags=0x104).
            if let Some(timer) = fdt.find("/timer") {
                let compat = fdt.compatible(&timer);
                if !compat.is_empty() {
                    println!("  /timer  : \"{compat}\"");
                }
                // The interrupts property has 4 entries × 3 cells = 12 u32s.
                // Each entry: (type, number, flags). PPI type=1.
                // Entry 0: (1, 13, ...) secure physical  -> IRQ 29
                // Entry 1: (1, 14, ...) non-secure physical -> IRQ 30
                // Entry 2: (1, 11, ...) virtual -> IRQ 27  <- ours
                // Entry 3: (1, 10, ...) hypervisor -> IRQ 26
                // GICv2 PPI IRQ = 16 + PPI_number.
                if let Some(intr) = fdt.prop(&timer, "interrupts") {
                    if intr.len() >= 32 {
                        // Entry 2 (virtual timer): cell index 7 = byte
                        // offset 28. (type at 24, number at 28, flags at 32)
                        let ppi_num = u32::from_be_bytes(intr[28..32].try_into().unwrap());
                        let irq = 16 + ppi_num;
                        let irq_match = irq == hal::gicv2::TIMER_IRQ;
                        println!(
                            "           virtual timer PPI {ppi_num} -> IRQ {irq} ({})",
                            if irq_match {
                                "matches TIMER_IRQ"
                            } else {
                                "DIFFERS!"
                            },
                        );
                    }
                }
            } else {
                println!("  /timer  : not found");
            }
        } else {
            println!("dtree     : magic found but header invalid — using board constants");
        }
    }

    println!();
    println!("monitor ready — 'help' lists commands. Ctrl-A X quits QEMU.");
    println!();

    // M0's echo loop, grown into a one-line-at-a-time monitor: same polled
    // byte I/O (terminal -> QEMU -> PL011 RX FIFO -> read_byte), but Enter
    // now runs the buffered line as a command. The commands exist to make
    // the kernel fault on purpose — see run_command below.
    let uart = console();
    let mut buf = [0u8; 64];
    let mut len = 0usize;
    print!("> ");
    loop {
        let b = uart.read_byte();
        match b {
            // Enter: finish the line and run it. Interactive terminals
            // send '\r'; anything piped in (tests, scripts) ends lines
            // with '\n'. Accept both, so the monitor is scriptable.
            b'\r' | b'\n' => {
                println!();
                run_command(&buf[..len]);
                len = 0;
                print!("> ");
            }
            // Backspace (0x08) or DEL (0x7f, what most terminals send):
            // drop a buffered byte and erase it on screen.
            0x08 | 0x7f => {
                if len > 0 {
                    len -= 1;
                    print!("\x08 \x08");
                }
            }
            // Printable ASCII: buffer (if room) and echo, as in M0.
            0x20..=0x7e => {
                if len < buf.len() {
                    buf[len] = b;
                    len += 1;
                    uart.write_byte(b);
                }
            }
            // Anything else (arrow keys, control chars) is shown as hex so
            // it is never invisible — but it does not enter the buffer.
            other => print!("<{other:#04x}>"),
        }
    }
}

/// Decode a PARange/IPS encoding into a bit count — for the banner only.
/// The encoding is IRREGULAR (0b011 is 42 bits, not 44); this table is
/// the spec's, verbatim (DDI 0487, ID_AA64MMFR0_EL1.PARange). The MMU
/// code itself never converts: it compares and clamps raw encodings
/// (mmu.rs explains why); turning them into human numbers is banner work.
fn parange_bits(enc: u64) -> u64 {
    match enc {
        0b000 => 32,
        0b001 => 36,
        0b010 => 40,
        0b011 => 42,
        0b100 => 44,
        0b101 => 48,
        0b110 => 52,
        _ => 0, // 0b111 is 56-bit FEAT_D128 territory; print 0 = "unknown here"
    }
}

/// Parse a monitor hex argument: `0x` prefix optional, case-insensitive.
fn parse_hex(s: &str) -> Option<u64> {
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u64::from_str_radix(s, 16).ok()
}

/// M7 bridge: discover the console UART from the FDT and write to it at
/// the discovered address.
///
/// Walks the devicetree looking for a node whose `compatible` property
/// contains `"arm,pl011"` — not by hardcoded path, but by what the node
/// *is*. This is how `board/apple.rs` will find its s5l UART (by
/// `"apple,s5l-uart"` compatibility) when the path and address are both
/// unknown. Once found, reads the `reg` property for the physical base,
/// maps it through `phys_to_virt`, and writes a message directly to the
/// PL011 at that virtual address — proving the FDT address is real, not
/// just a cross-check against a compiled constant.
fn fdt_console_write(fdt: &arch::aarch64::fdt::Fdtr) {
    // Walk every node in the tree looking for a PL011 by compatible
    // string. On QEMU's virt board this finds `/pl011`; on Apple Silicon
    // the path is different and unknown ahead of time — compatible
    // matching is the only portable way.
    let root = match fdt.root() {
        Some(r) => r,
        None => {
            println!("fdtcons: no root node");
            return;
        }
    };

    let mut found_uart: Option<u64> = None;
    let mut found_name: &str = "";

    for node in fdt.children(&root) {
        let compat = fdt.compatible(&node);
        if compat.contains("arm,pl011") {
            found_name = fdt.full_name(&node);
            if let Some(reg) = fdt.prop(&node, "reg") {
                if reg.len() >= 8 {
                    found_uart = Some(u64::from_be_bytes(reg[0..8].try_into().unwrap()));
                }
            }
            break;
        }
    }

    let pa = match found_uart {
        Some(pa) => pa,
        None => {
            println!("fdtcons: no node with compatible \"arm,pl011\" found");
            return;
        }
    };

    let va = arch::aarch64::mmu::phys_to_virt(pa as usize);

    println!("fdtcons: found \"{found_name}\" — PA {pa:#x}, VA {va:#x}",);

    // Ensure a dedicated Device-nGnRnE page maps this MMIO region. On
    // QEMU the PL011 happens to sit inside the boot stub's pre-mapped MMIO
    // slot, but on Apple Silicon the s5l UART lands elsewhere; calling
    // ioremap_device here makes the bridge exercise the runtime mapping
    // path and proves the FDT address is translatable before we touch it.
    match arch::aarch64::mmu::ioremap_device(va, pa as usize) {
        Ok(()) => println!("fdtcons: mapped as Device page"),
        Err(e) => println!("fdtcons: ioremap failed ({e:?}) — continuing, may fault"),
    }

    // Write directly to the PL011 at the FDT-discovered virtual address.
    // The PL011 register layout: DR at +0x00, FR at +0x18, FR.TXFF = bit 5.
    // This mirrors exactly what Pl011<BASE>::write_byte does internally,
    // but with a runtime address instead of a const generic.
    let msg = b"hello from the FDT-discovered console!\n";
    let dr: usize = va;
    let fr: usize = va + 0x18;
    const FR_TXFF: u32 = 1 << 5;

    for &b in msg {
        // Spin until the TX FIFO has room.
        loop {
            let flags = unsafe { core::ptr::with_exposed_provenance::<u32>(fr).read_volatile() };
            if flags & FR_TXFF == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        // Translate \n to CRLF for the serial terminal.
        if b == b'\n' {
            unsafe {
                core::ptr::with_exposed_provenance_mut::<u32>(dr).write_volatile(b'\r' as u32);
            }
            // Wait again for the CR to clear the FIFO.
            loop {
                let flags =
                    unsafe { core::ptr::with_exposed_provenance::<u32>(fr).read_volatile() };
                if flags & FR_TXFF == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
        }
        unsafe {
            core::ptr::with_exposed_provenance_mut::<u32>(dr).write_volatile(b as u32);
        }
    }

    println!();
    println!(
        "fdtcons: wrote {} bytes to the FDT-discovered console address.",
        msg.len()
    );
}

/// The monitor's command table, M2 edition. Three groups, and the
/// grouping is the lesson:
///
/// - *recoverable* — M1's survivors, plus its old killer: `unaligned`
///   was fatal when RAM was effectively Device memory, and stopped even
///   being a fault the day RAM became Normal. Same instruction, new
///   world; the kernel knowing the difference is milestone 2.
/// - *oracles* — `translate` asks the hardware where an address really
///   goes (no fault, just an answer); `walk` re-derives the same answer
///   in software, narrating every level, and cross-checks the two.
/// - *FATAL* — five commands, five distinct fault syndromes, each
///   reported in full and then parked: the page-table "no" at two
///   different levels (`low`, `guard`), the permission "no" for data and
///   for code (`wx`, `noexec`), and the bus's own "no" (`abort`).
pub fn run_command(line: &[u8]) {
    // The monitor loop only buffers printable ASCII, so this never
    // actually fails; "" on the impossible path beats an unwrap.
    let line = core::str::from_utf8(line).unwrap_or("").trim();
    let (cmd, arg) = match line.split_once(' ') {
        Some((cmd, arg)) => (cmd, arg.trim()),
        None => (line, ""),
    };

    match cmd {
        "" => {}
        "help" => {
            println!("commands:");
            println!("  help            this text");
            println!("  --- recoverable ---");
            println!("  brk             breakpoint: fault on purpose, report, recover, carry on");
            println!("  svc <n>         supervisor call with n in x8: report, recover, carry on");
            println!("  unaligned       M1's killer, defanged: Normal memory tolerates it now");
            println!("  --- oracles ---");
            println!("  translate <va>  ask AT S1E1R where a virtual address really goes");
            println!("  walk <va>       narrate the page-table walk, then cross-check the hardware");
            println!("  mapdev <pa>     map a 16 KiB Device page at KERNEL_BASE+pa -> pa (M7 runtime ioremap)");
            println!("  --- FATAL (one fault syndrome each) ---");
            println!("  guard           write below the stack: translation fault, level 3");
            println!("  wx              write to .rodata: permission fault");
            println!("  noexec          execute from .data: instruction abort (PXN)");
            println!(
                "  low             read M1's home address: the low half is condemned (level 0)"
            );
            println!("  abort           read past RAM: the bus-error window, external abort");
            println!("  --- M3: heartbeat ---");
            println!(
                "  tick            start the timer: IRQ 27 fires every second, tick counter increments"
            );
            println!(
                "  ticks           read the tick counter (how many timer interrupts have fired)"
            );
            println!("  ticktest        arm timer, spin 3 seconds, report tick count (self-test)");
            println!("  --- M4: devicetree ---");
            println!(
                "  dtb             show parsed devicetree: header, /memory, /intc, /pl011, /timer"
            );
            println!("  tree            dump the full devicetree tree (nodes, properties, values)");
            println!("  --- M7: FDT-driven discovery ---");
            println!(
                "  fdtcons         discover the UART from the FDT (by compatible), write to it at the discovered address"
            );
            println!("  --- M5: EL0 and syscalls ---");
            println!(
                "  el0             drop to EL0, run the user program (syscalls: write, yield, exit)"
            );
            println!(
                "  el0fault        drop to EL0, run a program that faults — test fault recovery"
            );
            println!("  --- M6: scheduler and IPC ---");
            println!("  tasks           dump the task table (scheduler diagnostics)");
            println!(
                "  spawn2          M6: spawn two tasks, drop to EL0 with the scheduler active"
            );
            println!(
                "  preempt         M6: preemptive scheduling — two spinning tasks, timer-driven switch"
            );
            println!("  ipc             M6: IPC demo — sender sends a message, receiver gets it");
            println!(
                "  blkipc          M6: blocking IPC — receiver blocks on recvblk, sender wakes it"
            );
            println!(
                "  sendblk         M6: blocking send — sender blocks on sendblk, receiver wakes it"
            );
            println!(
                "  faultkill       M6: scheduler-aware fault recovery — task faults, kernel kills it, OS keeps running"
            );
            println!(
                "  sleep           M6: timer-driven blocking — task sleeps for N ticks, timer wakes it"
            );
            println!(
                "  wait            M6: task-lifecycle blocking — parent blocks until child exits, gets exit code"
            );
        }
        "brk" => {
            arch::aarch64::vectors::demo_brk();
            // Reaching this line is the entire point of the demo:
            println!("...and we're back — the kernel caught its own fault and lived.");
        }
        "svc" => match arg.parse::<u64>() {
            Ok(n) => {
                arch::aarch64::vectors::demo_svc(n);
                println!("...back from the 'syscall' — M5 gives svc a real job.");
            }
            Err(_) => println!("usage: svc <decimal number>     e.g.  svc 7"),
        },
        "unaligned" => {
            // M1 printed "goodbye" here and meant it. The same load now
            // returns a value — bytes 1..9 of a known pattern.
            let v = arch::aarch64::vectors::demo_unaligned();
            println!("read {v:#018x} from an odd address — no fault: Normal memory");
            println!("tolerates this (and SCTLR_EL1.A stays clear so it may). This exact");
            println!("load was M1's \"goodbye\".");
        }
        "translate" => match parse_hex(arg) {
            Some(va) => match arch::aarch64::mmu::translate(va) {
                Ok(pa) => println!("  {va:#x} -> PA {pa:#x}"),
                Err(par) => {
                    // PAR_EL1.FST (bits [6:1]) speaks the same language
                    // as the fault reports' DFSC — same decoder, then.
                    let fst = (par >> 1) & 0x3F;
                    println!(
                        "  {va:#x} does not translate: FST {fst:#04x} — {}",
                        arch::aarch64::vectors::fault_status(fst)
                    );
                }
            },
            None => println!("usage: translate <hex va>     e.g.  translate 0xffff000040200000"),
        },
        "walk" => match parse_hex(arg) {
            Some(va) => arch::aarch64::mmu::walk(va),
            None => println!("usage: walk <hex va>          e.g.  walk 0xffff000009000000"),
        },
        "mapdev" => match parse_hex(arg) {
            Some(pa) => {
                let va = arch::aarch64::mmu::phys_to_virt(pa as usize) as u64;
                match arch::aarch64::mmu::ioremap_device(va as usize, pa as usize) {
                    Ok(()) => println!(
                        "mapdev: PA {pa:#x} mapped as Device page at VA {va:#x}; use 'walk {va:#x}' to inspect"
                    ),
                    Err(e) => println!("mapdev: failed to map PA {pa:#x} -> VA {va:#x}: {e:?}"),
                }
            }
            None => println!("usage: mapdev <hex pa>        e.g.  mapdev 0x10000000"),
        },
        "guard" => {
            println!("goodbye — writing 8 bytes below the stack; the guard page has no mapping:");
            arch::aarch64::vectors::demo_guard();
        }
        "wx" => {
            println!("goodbye — writing to .rodata; the mapping exists but says read-only:");
            arch::aarch64::vectors::demo_wx();
        }
        "noexec" => {
            println!("branching into .data, which holds a real `ret` — if PXN works, the");
            println!("fetch faults before the ret can run; if it returns, W^X is broken:");
            arch::aarch64::vectors::demo_noexec();
        }
        "low" => {
            println!("goodbye — reading 0x40200000, M1's home address; TTBR0 is the empty root:");
            arch::aarch64::vectors::demo_low();
        }
        "abort" => {
            println!("goodbye — reading the bus-error window; mapped Device, nothing answers:");
            // The first byte past RAM, through its higher-half alias:
            // mmu.rs maps this 32 MiB Device precisely so the walk
            // succeeds and the BUS gets to say no (DFSC 0x10), the way
            // every bad address failed before M2.
            let window =
                arch::aarch64::mmu::phys_to_virt(board::virt::RAM_BASE + board::virt::RAM_SIZE)
                    as u64;
            arch::aarch64::vectors::demo_abort(window);
        }
        "tick" => {
            // Arm the timer and unmask IRQ. One tick per second.
            let freq = arch::aarch64::timer::frequency();
            let period = freq; // 1 second worth of ticks
            arch::aarch64::timer::TICK_PERIOD.store(period, core::sync::atomic::Ordering::Relaxed);
            arch::aarch64::timer::arm(period);
            // Unmask IRQ at the CPU: clear DAIF.I.
            unsafe {
                core::arch::asm!(
                    "msr DAIFClr, #2", // clear IRQ mask (I bit)
                    options(nostack, preserves_flags),
                );
            }
            // Read back DAIF to verify I is clear.
            let daif: u64;
            unsafe {
                core::arch::asm!(
                    "mrs {daif}, DAIF",
                    daif = out(reg) daif,
                    options(nomem, nostack, preserves_flags),
                );
            }
            let irq_masked = (daif >> 7) & 1;
            println!("timer armed: 1 tick/sec ({freq} Hz). DAIF.I={irq_masked} (0=unmasked)");
            println!("the timer will fire on the next second. Type 'ticks' to check.");
        }
        "ticks" => {
            let n = arch::aarch64::timer::ticks();
            println!("tick counter: {n}");
        }
        "ticktest" => {
            // Self-test: arm timer, spin for ~3 seconds of real time
            // (spinning lets interrupts fire), then report the count.
            let freq = arch::aarch64::timer::frequency();
            let period = freq; // 1 second
            arch::aarch64::timer::TICK_PERIOD.store(period, core::sync::atomic::Ordering::Relaxed);
            arch::aarch64::timer::arm(period);
            // Unmask IRQ
            unsafe {
                core::arch::asm!("msr DAIFClr, #2", options(nostack, preserves_flags),);
            }
            println!("ticktest: timer armed, spinning 3 seconds...");
            // Spin for 3 seconds worth of counter ticks. Interrupts
            // fire during the spin and bump the tick counter.
            let start = arch::aarch64::timer::counter();
            let target = start + 3 * freq;
            while arch::aarch64::timer::counter() < target {
                core::hint::spin_loop();
            }
            let n = arch::aarch64::timer::ticks();
            println!("ticktest: {n} ticks in 3 seconds (expect ~3)");
        }
        "dtb" => {
            // Re-parse the FDT and print a summary: the bootloader's
            // honest description of the machine, cross-checked against
            // the board constants we compiled with.
            let ram_base = board::virt::RAM_BASE as u64;
            if !fdt_at(ram_base) {
                println!("no FDT at RAM base ({ram_base:#x})");
            } else if let Some(fdt) = arch::aarch64::fdt::Fdtr::new(ram_base as usize) {
                println!(
                    "FDT at {ram_base:#x} — {} bytes, boot CPU {}",
                    fdt.totalsize(),
                    fdt.boot_cpuid()
                );
                println!("reservations:");
                let mut rsv_count = 0;
                for r in fdt.reserves() {
                    println!(
                        "  [{rsv_count}] addr={:#018x} size={:#018x}",
                        r.address, r.size
                    );
                    rsv_count += 1;
                }
                if rsv_count == 0 {
                    println!("  (none)");
                }
                // /memory
                if let Some(mem) = fdt.find("/memory") {
                    let name = fdt.full_name(&mem);
                    if let Some(reg) = fdt.prop(&mem, "reg") {
                        if reg.len() >= 16 {
                            let base = u64::from_be_bytes(reg[0..8].try_into().unwrap());
                            let size = u64::from_be_bytes(reg[8..16].try_into().unwrap());
                            println!(
                                "{name}: base {base:#x}, size {size:#x} ({} MiB)",
                                size / (1024 * 1024)
                            );
                        }
                    }
                }
                // /intc
                if let Some(gic) = fdt.find("/intc") {
                    let compat = fdt.compatible(&gic);
                    println!("intc: compatible \"{compat}\"");
                    if let Some(reg) = fdt.prop(&gic, "reg") {
                        if reg.len() >= 32 {
                            let gicd = u64::from_be_bytes(reg[0..8].try_into().unwrap());
                            let gicc = u64::from_be_bytes(reg[16..24].try_into().unwrap());
                            println!("      GICD {gicd:#x}, GICC {gicc:#x}");
                        }
                    }
                }
                // /pl011
                if let Some(uart) = fdt.find("/pl011") {
                    let compat = fdt.compatible(&uart);
                    println!("pl011: compatible \"{compat}\"");
                    if let Some(reg) = fdt.prop(&uart, "reg") {
                        if reg.len() >= 16 {
                            let base = u64::from_be_bytes(reg[0..8].try_into().unwrap());
                            println!("      base {base:#x}");
                        }
                    }
                }
                // /timer
                if let Some(timer) = fdt.find("/timer") {
                    let compat = fdt.compatible(&timer);
                    println!("timer: compatible \"{compat}\"");
                    if let Some(intr) = fdt.prop(&timer, "interrupts") {
                        let cells = intr.len() / 4;
                        print!("      interrupts: {cells} cells <",);
                        for i in 0..cells {
                            if i > 0 {
                                print!(" ");
                            }
                            let v = u32::from_be_bytes(intr[i * 4..i * 4 + 4].try_into().unwrap());
                            print!("{v:#x}");
                        }
                        println!(">");
                    }
                }
            } else {
                println!("FDT magic found but header invalid");
            }
        }
        "tree" => {
            // Dump the full devicetree tree. This is the raw view of
            // what QEMU declared — every node, every property, every
            // value. A teaching tool: compare this with `dtc -I dtb -O
            // dts /tmp/virt.dtb` to see the same data in a different
            // rendering.
            let ram_base = board::virt::RAM_BASE as u64;
            if !fdt_at(ram_base) {
                println!("no FDT at RAM base ({ram_base:#x})");
            } else if let Some(fdt) = arch::aarch64::fdt::Fdtr::new(ram_base as usize) {
                println!("=== devicetree dump ({} bytes) ===", fdt.totalsize());
                fdt.dump();
                println!("=== end ===");
            } else {
                println!("FDT magic found but header invalid");
            }
        }
        "fdtcons" => {
            // M7 bridge: discover the console from the FDT and write to
            // it at the discovered address. Until now the FDT was a
            // diagnostic — the kernel read the UART base and cross-
            // checked it, but the console always used the compiled-in
            // constant. This command proves the FDT address is real by
            // writing directly to the PL011 at the runtime-discovered
            // physical address (mapped through phys_to_virt, the same
            // higher-half alias the compiled console uses).
            //
            // On Apple Silicon the UART base genuinely varies per SoC,
            // and the FDT is the only way to find it. This is the first
            // step of M7's FDT-driven discovery, tested on QEMU where
            // we know the answer.
            let ram_base = board::virt::RAM_BASE as u64;
            if !fdt_at(ram_base) {
                println!("no FDT at RAM base ({ram_base:#x})");
            } else if let Some(fdt) = arch::aarch64::fdt::Fdtr::new(ram_base as usize) {
                fdt_console_write(&fdt);
            } else {
                println!("FDT magic found but header invalid");
            }
        }
        "el0" => {
            // M5: drop to EL0 and run the user program. This does not
            // return to this monitor loop — it returns via the
            // __el0_return trampoline -> on_el0_return(), which runs
            // its own monitor loop. So this match arm never completes
            // normally; drop_to_el0() erets and the next code that
            // runs is the user program at EL0.
            arch::aarch64::user::drop_to_el0();
        }
        "el0fault" => {
            // M5: drop to EL0 and run a program that *deliberately*
            // faults (stores to an unmapped address). The data abort
            // traps to EL1, handle_sync_from_el0 reports it and calls
            // kill_task_on_fault — the kernel survives and returns to
            // the monitor. This is the kernel's first fault *recovery*:
            // M1 taught it to report faults, M5 teaches it to survive
            // a user's. Like `el0`, this does not return to this loop.
            arch::aarch64::user::drop_to_el0_fault();
        }
        "tasks" => {
            // M6: dump the task table and scheduler state. Until tasks
            // are actually created and the context switch is wired, this
            // shows the empty table (all slots Exited) and the idle
            // scheduler — but it proves the data structures are live and
            // gives the next beat something to fill.
            sched::dump_tasks();
        }
        "spawn2" => {
            // M6: spawn two tasks (A and B), each with its own user
            // address space, and drop to EL0 with the scheduler active.
            // When task A yields, the scheduler switches to task B;
            // when B yields, it switches back. The output interleaves,
            // proving two independent tasks share one CPU. Like `el0`,
            // this does not return to this monitor loop.
            arch::aarch64::user::drop_to_el0_scheduled();
        }
        "preempt" => {
            // M6: preemptive scheduling. Spawn two tasks that SPIN
            // forever (no yield, no exit), arm the timer, enable
            // preemption. The timer IRQ fires every second and calls
            // save_and_switch from the IRQ handler, preempting
            // whichever task is spinning. Both "A" and "B" appear on
            // the console — proof that the timer, not user code, drove
            // the context switch. Like `spawn2`, this does not return
            // to this monitor loop (the tasks never exit; Ctrl-A X to
            // quit QEMU).
            arch::aarch64::user::drop_to_el0_preempt();
        }
        "ipc" => {
            // M6: IPC demo. Spawn a sender (task A) and a receiver
            // (task B), each with its own address space. A sends
            // "hello B!" to B's mailbox via SYS_SEND, then yields. B
            // calls SYS_RECV, gets the message from its mailbox, and
            // writes "B: got msg!" — proving the message passed
            // through the kernel from one task to another. Like
            // `spawn2`, this does not return to this monitor loop.
            arch::aarch64::user::drop_to_el0_ipc();
        }
        "blkipc" => {
            // M6: blocking IPC demo. Like `ipc` but the receiver calls
            // SYS_RECVBLK (blocking receive) instead of SYS_RECV. When
            // B's mailbox is empty, the kernel puts B to sleep (Blocked
            // state) and switches to A. A sends "wake!" — the send path
            // sees B is Blocked, wakes it (Ready, enqueued), and the
            // scheduler resumes B, which re-enters the recvblk svc,
            // finds the message, and continues. This is the first use
            // of the Blocked task state: a task sleeps on a condition
            // and another task wakes it — the same pattern a real OS
            // uses for blocking read()/wait()/futex. Like `ipc`, this
            // does not return to this monitor loop.
            arch::aarch64::user::drop_to_el0_blkipc();
        }
        "sendblk" => {
            // M6: blocking send demo. Like `ipc` but the sender calls
            // SYS_SENDBLK (blocking send, syscall 7) instead of SYS_SEND.
            // A sends "first" (succeeds), then immediately sends "second"
            // — but B's mailbox is full, so A blocks. The scheduler
            // switches to B, which calls recv (drains "first"), waking A.
            // B yields, A retries sendblk — the mailbox is empty now, so
            // "second" lands. A writes "A: sent2" and exits; B recv's
            // "second", writes "B: got2" and exits. This is the symmetric
            // counterpart to blkipc: there the receiver slept on
            // "mailbox empty"; here the sender sleeps on "mailbox full."
            // Together they give M6's IPC a complete blocking pair.
            // Like `blkipc`, this does not return to this monitor loop.
            arch::aarch64::user::drop_to_el0_sendblk();
        }
        "faultkill" => {
            // M6: scheduler-aware fault recovery. Spawn two tasks where
            // task A deliberately faults (stores to unmapped VA 0). The
            // kernel kills task A and resumes the scheduler, which runs
            // task B. Task B writes "B: ok" and exits - proof that the
            // OS survived task A's death. This is the M6 evolution of
            // M5's `el0fault`: there, the single task's fault killed
            // the "OS"; here, one task's fault is just that task's
            // problem, and the scheduler keeps running.
            arch::aarch64::user::drop_to_el0_faultkill();
        }
        "sleep" => {
            // M6: timer-driven blocking. Spawn a sleeper (task A) and
            // a runner (task B). Task A calls sleep(3) — blocking for
            // 3 timer ticks. While A sleeps, B runs (yielding twice);
            // when A's deadline passes, the timer IRQ's wake_sleepers
            // wakes A and the scheduler resumes it. This is the third
            // blocking primitive: recvblk/sendblk sleep on another
            // task; sleep sleeps on the passage of time. Like `spawn2`,
            // this does not return to this monitor loop.
            arch::aarch64::user::drop_to_el0_sleep();
        }
        "wait" => {
            // M6: wait demo. Spawn a parent (task A) and a child (task B).
            // Task A writes "A: wait" and calls wait(2) — blocking until
            // task B exits. Task B writes "B: hi", yields, writes "B:
            // bye", and exits with code 42. The exit handler stores
            // exit_code=42 and calls wake_waiters, which wakes A. A
            // retries the wait svc, finds B Exited, gets 42 in x0,
            // and writes "A: woke!". This is the fourth blocking
            // primitive: recvblk/sendblk sleep on IPC, sleep sleeps on
            // the timer, wait sleeps on a child's exit. Like `spawn2`,
            // this does not return to this monitor loop.
            arch::aarch64::user::drop_to_el0_wait();
        }
        other => println!("unknown command {other:?} - try 'help'"),
    }
}

// ---------------------------------------------------------------------------
// Panic handler
// ---------------------------------------------------------------------------

/// Rust demands exactly one `#[panic_handler]` in a `no_std` binary: this
/// is where every `panic!`, failed `assert!`, and arithmetic-overflow check
/// lands. Signature is fixed by the language: `fn(&PanicInfo) -> !`.
///
/// We print what happened, then park the core in a low-power loop. No
/// unwinding (panic = "abort" in Cargo.toml), no reboot: for a teaching
/// kernel a frozen machine with a message beats a reboot loop that eats
/// the message.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // A panic may strike while the console lock is *held* — possibly by
    // the very code that panicked. Flip the OOPS flag so the prints below
    // bypass the lock (and never flip it back; there is no recovering
    // from here). The console itself is a fresh zero-sized handle, so no
    // mid-flight shared state can hurt us.
    oops_enter();
    println!();
    println!("*** KERNEL PANIC ***");
    println!("{info}");
    arch::aarch64::park()
}
