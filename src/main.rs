//! Opal — milestone 2: maps its own world.
//!
//! This file is still most of the "operating system": print macros backed
//! by a lock, the banner (which now reads the MMU's registers back as
//! receipts), a monitor whose commands poke the new address space from
//! every angle, and a panic handler. The journey from power-on to `kmain`
//! — which since M2 includes building page tables and climbing to the
//! higher half — is docs/01-boot-flow.md and docs/04-virtual-memory.md;
//! the page tables and the enable ceremony are arch/aarch64/mmu.rs; the
//! exception machinery is arch/aarch64/vectors.rs and docs/03.

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
mod sync;

use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

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

/// The plumbing under `print!`/`println!`: render `format_args!` output
/// straight into the board console. No allocation, no buffering — but as
/// of M1, a lock (and its emergency bypass) where M0 honestly had none.
#[doc(hidden)]
fn console_print(args: core::fmt::Arguments) {
    // The console is zero-sized and infallible; ignore the fmt::Result.
    if OOPS.load(Ordering::Relaxed) {
        // Emergency path: conjure an unlocked console and write. The ZST
        // design pays off here — there is no shared state to corrupt.
        let _ = board::virt::console().write_fmt(args);
        return;
    }
    CONSOLE_LOCK.with(|| {
        let _ = board::virt::console().write_fmt(args);
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
/// the bootloader left it.
///
/// Returns `!`: there is nothing to return *to*. The call stack below us
/// is three frames of assembly ending in a parking loop.
fn kmain(x0: u64) -> ! {
    // Catching faults is the first ability worth having: install the
    // vector table before printing anything, so that even a banner-era
    // bug gets a report instead of M0's silent hang. (The boot stub
    // pre-armed VBAR already; this is the third, PC-relative derivation
    // of the same address — the banner reads it back below.)
    arch::aarch64::vectors::install();

    board::virt::init();

    // Pull up the ladder: TTBR0 gets the empty root, and the low half —
    // the identity world the boot stub climbed through — stops
    // translating. ORDER IS LOAD-BEARING in this function: everything
    // before this line may still lean on low addresses; nothing after it
    // may. (The console const moved upstairs with us: board::virt::UART0_VA.)
    arch::aarch64::mmu::condemn_low_half();

    println!();
    println!("opal — milestone 2: maps its own world");
    println!("--------------------------------------");

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
    println!("guard      : 16 KiB unmapped below the stack — overflow now faults instead of eating .bss (M0's debt, paid)");

    // What did the bootloader hand us? Under the Linux boot protocol (and
    // m1n1) x0 is a physical FDT pointer. QEMU sets NO registers for
    // non-Linux ELF payloads, so here we expect 0 — and we report whatever
    // we actually find rather than what folklore says we should find.
    println!("x0 at entry: {x0:#x}");
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
        println!("fdt at RAM base (PA {ram_base:#x}, read via its higher-half alias): yes — QEMU's bare-metal DTB placement");
    } else {
        println!("fdt at RAM base (PA {ram_base:#x}, read via its higher-half alias): no — unexpected, check linker.ld load address");
    }

    println!();
    println!("monitor ready — 'help' lists commands. Ctrl-A X quits QEMU.");
    println!();

    // M0's echo loop, grown into a one-line-at-a-time monitor: same polled
    // byte I/O (terminal -> QEMU -> PL011 RX FIFO -> read_byte), but Enter
    // now runs the buffered line as a command. The commands exist to make
    // the kernel fault on purpose — see run_command below.
    let uart = board::virt::console();
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
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u64::from_str_radix(s, 16).ok()
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
fn run_command(line: &[u8]) {
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
            println!("  --- FATAL (one fault syndrome each) ---");
            println!("  guard           write below the stack: translation fault, level 3");
            println!("  wx              write to .rodata: permission fault");
            println!("  noexec          execute from .data: instruction abort (PXN)");
            println!("  low             read M1's home address: the low half is condemned (level 0)");
            println!("  abort           read past RAM: the bus-error window, external abort");
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
            let window = arch::aarch64::mmu::phys_to_virt(
                board::virt::RAM_BASE + board::virt::RAM_SIZE,
            ) as u64;
            arch::aarch64::vectors::demo_abort(window);
        }
        other => println!("unknown command {other:?} — try 'help'"),
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
