//! Opal — milestone 0: boots and speaks.
//!
//! This file is the whole "operating system" so far: print macros, the
//! banner, an echo loop, and a panic handler. The interesting journey of
//! how the CPU even *gets* to `kmain` is told in docs/01-boot-flow.md.

// A kernel cannot use `std` — std assumes an OS underneath (files, threads,
// an allocator, a way to exit). We *are* the OS. `core` is the dependency-
// free heart of the standard library and is all we get.
#![no_std]
// And there is no `fn main()`: nothing would call it. Our real entry point
// is `_start` in arch/aarch64/boot.rs, chosen by ENTRY(_start) in linker.ld.
#![no_main]

mod arch;
mod board;
mod hal;

use core::fmt::Write;
use core::panic::PanicInfo;

// ---------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------

/// The plumbing under `print!`/`println!`: render `format_args!` output
/// straight into the board console. No allocation, no buffering, no locks —
/// see hal/pl011.rs for why lock-free is honest *today* and when it stops
/// being so (M1: interrupts).
#[doc(hidden)]
fn console_print(args: core::fmt::Arguments) {
    // The console is zero-sized and infallible; ignore the fmt::Result.
    let _ = board::virt::console().write_fmt(args);
}

/// `print!` — exactly like std's, but the bytes go to the UART.
macro_rules! print {
    ($($arg:tt)*) => { $crate::console_print(format_args!($($arg)*)) };
}

/// `println!` — `print!` plus a newline (the UART driver adds the `\r`).
macro_rules! println {
    () => { print!("\n") };
    ($($arg:tt)*) => { print!("{}\n", format_args!($($arg)*)) };
}

// ---------------------------------------------------------------------------
// Devicetree sniffing
// ---------------------------------------------------------------------------

/// Every flattened devicetree (FDT/DTB) begins with this magic number,
/// stored big-endian in memory.
const FDT_MAGIC: u32 = 0xd00d_feed;

/// Does `addr` plausibly point at a devicetree? We only dereference
/// addresses inside RAM: with the MMU off every access goes straight to
/// the bus, and reading a hole in the physical map would fault — milestone
/// 0 has no exception vectors, so a fault would be an instant silent hang.
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

/// First (and so far only) kernel function. Called by `_start_rust` with
/// `x0` exactly as the bootloader left it.
///
/// Returns `!`: there is nothing to return *to*. The call stack below us
/// is three frames of assembly ending in a parking loop.
fn kmain(x0: u64) -> ! {
    board::virt::init();

    println!();
    println!("opal — milestone 0: boots and speaks");
    println!("------------------------------------");

    // Privilege check. QEMU virt without virtualization=on/secure=on has
    // no EL2/EL3, so we expect EL1. (On Apple Silicon via m1n1: EL2.)
    println!("current EL : EL{}", arch::aarch64::current_el());

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
    println!("echo console ready — type and the kernel answers. Ctrl-A X quits QEMU.");
    println!();

    // The interactive payoff: a polled echo loop. Every byte you type goes
    // terminal -> QEMU -> PL011 RX FIFO -> read_byte -> write_byte -> back.
    // The kernel is doing I/O in both directions, which is milestone 0's
    // entire job.
    let uart = board::virt::console();
    loop {
        let b = uart.read_byte();
        match b {
            // Terminals send '\r' for Enter; answer with a full newline.
            b'\r' => print!("\n"),
            // Echo printable ASCII and tab as-is; show anything else (arrow
            // keys, control chars) as its hex value so it is never invisible.
            0x20..=0x7e | b'\t' => uart.write_byte(b),
            other => print!("<{other:#04x}>"),
        }
    }
}

// ---------------------------------------------------------------------------
// Panic handler
// ---------------------------------------------------------------------------

/// Rust demands exactly one `#[panic_handler]` in a `no_std` binary: this
/// is where every `panic!`, failed `assert!`, and arithmetic-overflow check
/// lands. Signature is fixed by the language: `fn(&PanicInfo) -> !`.
///
/// We print what happened — conjuring a fresh zero-sized console, since a
/// panic may strike while any shared state is mid-flight — then park the
/// core in a low-power loop. No unwinding (panic = "abort" in Cargo.toml),
/// no reboot: for a teaching kernel a frozen machine with a message beats
/// a reboot loop that eats the message.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!();
    println!("*** KERNEL PANIC ***");
    println!("{info}");
    arch::aarch64::park()
}
