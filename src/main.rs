//! Opal — milestone 1: catches its own faults.
//!
//! This file is still most of the "operating system": print macros (now
//! backed by a lock), the banner, a tiny fault-demo monitor, and a panic
//! handler. The journey from power-on to `kmain` is docs/01-boot-flow.md;
//! the new exception machinery — vector table, trap frames, fault reports
//! — is arch/aarch64/vectors.rs and docs/03-exceptions.md.

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

/// Does `addr` plausibly point at a devicetree? We only dereference
/// addresses inside RAM: with the MMU off every access goes straight to
/// the bus, and reading a hole in the physical map takes a synchronous
/// external abort. In M0 that meant a silent hang; since M1 it would at
/// least die with a full report (try the monitor's `abort` command) — but
/// a banner that says "no devicetree" still beats one that dies saying
/// why, so the guard stays. The 4-alignment check is the same lesson in
/// miniature: with the MMU off every access is Device memory, so a
/// *misaligned* u32 read would not return garbage — it would itself take
/// an alignment fault (DFSC 0x21), exactly the abort the monitor's
/// `unaligned` command demonstrates.
fn fdt_at(addr: u64) -> bool {
    const RAM_END: u64 = (board::virt::RAM_BASE + board::virt::RAM_SIZE) as u64;
    if addr < board::virt::RAM_BASE as u64 || addr >= RAM_END || addr % 4 != 0 {
        return false;
    }
    // SAFETY: addr is 4-aligned and inside RAM, which is identity-
    // accessible while the MMU is off. Volatile, because this memory was
    // written by QEMU, not by Rust code the compiler knows about.
    let raw = unsafe { core::ptr::with_exposed_provenance::<u32>(addr as usize).read_volatile() };
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
    board::virt::init();

    // Catching faults is the first ability worth having: install the
    // vector table before printing anything, so that even a banner-era
    // bug gets a report instead of M0's silent hang.
    arch::aarch64::vectors::install();

    println!();
    println!("opal — milestone 1: catches its own faults");
    println!("------------------------------------------");

    // Privilege check. QEMU virt without virtualization=on/secure=on has
    // no EL2/EL3, so we expect EL1. (On Apple Silicon via m1n1: EL2.)
    println!("current EL : EL{}", arch::aarch64::current_el());

    // The M1 line: read VBAR_EL1 back and print what the register holds,
    // not what we hope install() wrote.
    println!(
        "vectors    : VBAR_EL1 = {:#x} (16-entry table live)",
        arch::aarch64::vectors::vbar()
    );

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
    // provided our link address left room for it (we link at 0x4020_0000;
    // QEMU wants >= 1 MiB of room — see linker.ld).
    let ram_base = board::virt::RAM_BASE as u64;
    if fdt_at(ram_base) {
        println!("fdt at RAM base {ram_base:#x}: yes — QEMU's bare-metal DTB placement");
    } else {
        println!("fdt at RAM base {ram_base:#x}: no — unexpected, check linker.ld load address");
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

/// The monitor's command table. Two commands prove the kernel survives
/// its own faults (`brk`, `svc`); two prove it dies *informatively*
/// (`unaligned`, `abort`). That contrast — recoverable versus fatal, and
/// the kernel knowing which is which — is milestone 1.
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
            println!("  help       this text");
            println!("  brk        breakpoint: fault on purpose, report, recover, carry on");
            println!("  svc <n>    supervisor call with n in x8: report, recover, carry on");
            println!("  unaligned  FATAL: unaligned load (MMU off = Device memory rules)");
            println!("  abort      FATAL: read from a hole in the physical address map");
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
            println!("goodbye — this fault has no fix until the MMU milestone (M2):");
            arch::aarch64::vectors::demo_unaligned();
        }
        "abort" => {
            println!("goodbye — reading where no memory lives; the bus will object:");
            // First byte past the end of RAM: QEMU backs [RAM_BASE,
            // RAM_BASE + RAM_SIZE) and nothing above it for gigabytes.
            let hole = (board::virt::RAM_BASE + board::virt::RAM_SIZE) as u64;
            arch::aarch64::vectors::demo_abort(hole);
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
