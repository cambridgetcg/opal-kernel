//! The MMU: how the kernel maps its own world.
//!
//! Until this milestone every address was a physical address: the CPU put
//! each load and store straight on the bus, every byte of RAM was writable,
//! executable, and Device-strict, and the only protection anywhere was
//! politeness. This file replaces that with the architecture's translation
//! machinery: page tables that the kernel builds for itself, in ordinary
//! memory, and then hands to the hardware walker.
//!
//! ## The map we want
//!
//! One rule generates the whole layout: **high virtual address = physical
//! address + `KERNEL_BASE`**. Linux calls this the linear map; we get the
//! kernel image, the DTB, the UART, and all of RAM through one offset:
//!
//! ```text
//! 0xFFFF_0000_0900_0000  UART          16 KiB   Device-nGnRnE  (L3 page)
//! 0xFFFF_0000_4000_0000  DTB window     2 MiB   Normal, read-only
//! 0xFFFF_0000_4020_0000  kernel .text           Normal, read+execute
//!                        kernel .rodata         Normal, read-only
//!                        .data/.bss             Normal, read+write
//!                        --- guard ---  16 KiB  UNMAPPED (M0's debt, paid)
//!                        boot stack     64 KiB  Normal, read+write
//! 0xFFFF_0000_4200_0000  rest of RAM  480 MiB   Normal, rw (32 MiB blocks)
//! 0xFFFF_0000_6000_0000  bus-error window       Device, mapped, UNBACKED
//! ```
//!
//! Everything else — including the entire low half once
//! [`condemn_low_half`] runs — is unmapped, and touching it produces a
//! *translation fault* with a report, where M0 would have silently hung.
//!
//! ## One tree, two trunks
//!
//! `KERNEL_BASE` is `0xFFFF_0000_0000_0000`: all-ones in bits [63:48] and
//! zero in bits [47:0]. That makes every high VA's low 48 bits *equal* its
//! PA — so the table tree that maps the higher half, walked starting at
//! those 48 bits, is **also** an identity map of the same physical space.
//! During the boot transition we point both TTBR0 (low half) and TTBR1
//! (high half) at the one tree: the same descriptors serve the old world
//! and the new, attributes identical by construction, while the kernel
//! climbs from one to the other. m1n1 boots Apple Silicon with exactly
//! this trick (src/memory.c); Linux builds a separate throwaway idmap
//! (`init_idmap_pg_dir`) for the same moment — see docs/04 for why each
//! chooses what it does. Once the kernel lives upstairs, TTBR0 is swapped
//! to a permanently-empty root and the ground floor stops translating.
//!
//! ## The low world
//!
//! Two functions here run **below the kernel's link address**: the boot
//! stub calls them at their physical addresses, before the MMU is on.
//! They carry a `LOW WORLD` banner and obey the contract in the section
//! comment below — the short version is *no formatting, no panics that
//! fire, nothing that reads an absolute address out of data*. Everything
//! else in this file is ordinary high-world Rust.
//!
//! The full story — descriptor bits, the enable cliff, the move upstairs —
//! is docs/04-virtual-memory.md. Read it with this file open.

use crate::board::virt::{RAM_BASE, RAM_SIZE, UART0_BASE};

// ---------------------------------------------------------------------------
// The constants the hardware reads
// ---------------------------------------------------------------------------

/// Where the kernel lives, virtually. Three properties are load-bearing:
///
/// 1. bits [63:48] are all ones — with `T1SZ=16` the TTBR1 region is
///    exactly the addresses whose top 16 bits are ones, so this is the
///    *bottom* of the high half, not a random point inside it;
/// 2. bits [47:0] are all zeros — `high VA = PA + KERNEL_BASE` therefore
///    preserves the low 48 bits, which is what lets one table tree serve
///    both TTBRs (the walk only ever sees VA[47:0]);
/// 3. it is ≡ 0 mod 64 KiB — rust-lld keeps file offsets congruent to
///    VMAs mod the max page size, so a ragged base would bloat the ELF
///    with padding.
pub const KERNEL_BASE: usize = 0xFFFF_0000_0000_0000;

// Tie KERNEL_BASE to T1SZ=16 forever. With T1SZ=17 the TTBR1 region
// shrinks to 0xFFFF_8000_...-up, KERNEL_BASE falls into the unmapped hole
// between the halves, and every high access takes a level-0 translation
// fault — the exact silent-boot-death M2 exists to abolish. If someone
// retunes T1SZ without rethinking KERNEL_BASE, this stops the build.
const _: () = assert!(
    KERNEL_BASE == !0xFFFF_FFFF_FFFFusize,
    "KERNEL_BASE must be the bottom of the T1SZ=16 TTBR1 region"
);

/// One translation granule: 16 KiB. The page size Apple's DART IOMMUs
/// demand, the reason `.cargo/config.toml` runs `-cpu max`, and the
/// constant behind every `ALIGN(16K)` in linker.ld.
pub const GRANULE: usize = 16 << 10;

/// Physical address -> the kernel's virtual alias for it (Linux: `__va`).
pub const fn phys_to_virt(pa: usize) -> usize {
    KERNEL_BASE + pa
}

/// The kernel's virtual alias -> physical address (Linux: `__pa`). Only
/// meaningful for addresses in the linear map — which, after
/// [`condemn_low_half`], is every address the kernel can legally touch.
pub const fn virt_to_phys(va: usize) -> usize {
    va - KERNEL_BASE
}

/// TCR_EL1, the translation control register, minus its one runtime field
/// (IPS — see [`opal_mmu_enable`]). Field by field, low to high:
///
/// - `T0SZ` [5:0] = 16 and `T1SZ` [21:16] = 16: each half translates
///   2^(64-16) = 48 bits of VA — a four-level walk starting at a 2-entry
///   level 0. m1n1 runs 16K/48-bit on real M1 hardware; so do Asahi's
///   kernels; so do we.
/// - `IRGN0/ORGN0` [9:8]/[11:10] and `IRGN1/ORGN1` [25:24]/[27:26] =
///   0b01: the *walker's* own reads of the tables are inner/outer
///   write-back read-allocate write-allocate — matching the cacheability
///   of the memory the tables live in.
/// - `SH0` [13:12] / `SH1` [29:28] = 0b11: walks are Inner Shareable —
///   single-core today, but this is the posture multi-core (M6) inherits.
/// - `TG0` [15:14] = **0b10** = 16 KiB. On this field 0b00 is 4K and
///   0b01 is 64K.
/// - `TG1` [31:30] = **0b01** = 16 KiB. A DIFFERENT encoding for the same
///   granule: on *this* field 0b10 means 4K. Programming TG0's encoding
///   into TG1 is the classic trap — the machine boots, prints, runs for
///   as long as you stay low, and translation-faults on the first
///   high-half access. The const asserts below make each field check the
///   other's documentation.
/// - `IPS` [34:32]: the physical-address ceiling — OR'd in at runtime
///   from `ID_AA64MMFR0_EL1.PARange`, because hardcoding what the CPU
///   implements is exactly the lie ID registers exist to prevent.
/// - Everything else 0: no ASID games (`A1`), no top-byte-ignore
///   (`TBI0/1` — tagged pointers would make fault addresses lie), no
///   hardware access-flag updates (`HA/HD` — Apple M1 lacks FEAT_HAFDBS,
///   so leaning on it would work on QEMU and break on the real target;
///   we set AF in software instead, see the descriptor constructors),
///   and `DS`=0 (no FEAT_LPA2 on M1 — which is also why 16K has **no
///   level-1 blocks**: 32 MiB at level 2 is the only block size we get).
pub const TCR_EL1_BASE: u64 = 0x0000_0000_7510_B510;

// Each TG field's assert quotes the OTHER field's scale, because the trap
// is precisely "I remembered the encoding — from the wrong field".
const _: () = assert!(
    (TCR_EL1_BASE >> 14) & 0b11 == 0b10,
    "TCR.TG0 must be 0b10 = 16K (it is TG1 where 0b01 means 16K)"
);
const _: () = assert!(
    (TCR_EL1_BASE >> 30) & 0b11 == 0b01,
    "TCR.TG1 must be 0b01 = 16K (it is TG0 where 0b10 means 16K)"
);

/// MAIR_EL1: the eight memory-attribute slots that descriptors pick from
/// by index (AttrIndx). We define two:
///
/// - **index 0 = 0xFF**: Normal memory, write-back, read- and
///   write-allocate, inner and outer. Index zero on purpose: it makes
///   kernel-RAM descriptors the quiet common case in a `walk` dump.
/// - **index 1 = 0x00**: Device-nGnRnE — no gathering, no reordering,
///   no early write acknowledgement. *Not* nGnRE: Apple SoCs answer
///   posted (early-acked) MMIO writes with an SError — it is why Linux
///   grew `ioremap_np()` during the M1 bring-up. QEMU forgives either;
///   the real target does not, so we rehearse the strict one.
///
/// The six unused slots stay 0x00, which *decodes as* Device-nGnRnE: a
/// descriptor with a buggy AttrIndx lands on the strictest memory type
/// instead of a permissive one. (A third slot — Normal non-cacheable —
/// has no user until M7's framebuffer; adding an index later rewrites no
/// existing descriptor, so it waits.)
pub const MAIR_EL1_VALUE: u64 = 0x0000_0000_0000_00FF;

/// SCTLR_EL1 as one from-scratch value — deliberately *not* read-modify-
/// write: when m1n1 hands us the machine at EL2, SCTLR_EL1 (a lower EL's
/// register at that moment) is architecturally UNKNOWN, so there is
/// nothing trustworthy to modify. Linux solves it the same way
/// (`INIT_SCTLR_EL1_MMU_ON`: a constant, written whole).
///
/// Bits set — the three that are the milestone: `M` [0] (MMU on), `C` [2]
/// (data caches on), `I` [12] (instruction caches on). The supporting
/// cast: `SA`/`SA0` [3]/[4] (SP-alignment checking — a misaligned SP
/// faults instead of corrupting), `ITD`/`SED` [7]/[8] and `nTWI`/`nTWE`
/// [16]/[18], `TSCXT` [20], `nTLSMD`/`LSMAOE` [28]/[29], `EOS`/`EIS`
/// [11]/[22], `SPAN` [23] — each is either RES1 on a machine without the
/// corresponding feature (AArch32 at EL0, trapped WFI/WFE, ...) or the
/// architecture's "no surprises" default; docs/04 §6 item 5 walks the
/// full list, one bit at a time.
///
/// Bits deliberately **clear**:
/// - `A` [1]: alignment checking stays OFF. The milestone's best teaching
///   beat is that unaligned loads to Normal memory simply *work* now;
///   A=1 would re-criminalize them and hide the lesson.
/// - `WXN` [19]: write-implies-never-execute, globally. Tempting, but the
///   per-descriptor PXN/UXN bits are the single source of truth we teach;
///   a second, global mechanism that silently overrides the first is a
///   debugging trap. Revisit when M5 hardens the userspace boundary.
/// - `EE`/`E0E` [25]/[24]: endianness. `EE` governs EL1 data accesses
///   *and the walker's own descriptor fetches* — EE=1 would byte-swap
///   every table walk, exactly the kind of UNKNOWN a from-scratch write
///   must pin down. `E0E` covers only EL0 data access (academic until
///   M5, pinned to zero now anyway).
pub const SCTLR_EL1_VALUE: u64 = 0x0000_0000_30D5_199D;

/// A known value at a known symbol, interrogated by the boot stub the
/// instant translation turns on — in two steps, because the two failure
/// shapes differ. First `AT S1E1R` asks whether the canary's high
/// address translates *at all*, reporting failure in PAR_EL1 instead of
/// faulting. That is the shape the TG1 trap produces (wrong granule →
/// the walker misreads the tree's geometry → translation fault at level
/// 1), and a shape a raw load could only express as a fault nobody can
/// report: the reporter's format strings are ABS64 data, high, and
/// exactly as unreachable as the canary itself. Then the load-and-
/// compare: translating *somewhere* is not translating to the right
/// frames. Either failure is a '?' on the UART and a park — a located
/// death at the enable site, instead of "boots, then dies hours later".
/// The value spells "OPALM2OK".
#[unsafe(no_mangle)] // boot.rs materializes this symbol's address by name
pub static MMU_CANARY: u64 = 0x4F50_414C_4D32_4F4B;

// ---------------------------------------------------------------------------
// Geometry: the 16 KiB four-level walk
// ---------------------------------------------------------------------------
//
// A 16 KiB granule means a 14-bit page offset, and 16 KiB of 8-byte
// entries per table means 11 bits of index per level:
//
//   VA bits   [47]      [46:36]   [35:25]   [24:14]   [13:0]
//             L0 (2!)   L1        L2        L3        offset
//
// Level 0 indexes a single bit: a 2-entry, 16-*byte* table at the root of
// 256 TiB. Levels that can stop early: only L2 (32 MiB "block" entries) —
// at 16K without FEAT_LPA2 there are NO level-1 blocks, however loudly
// 4K-granule muscle memory ("1 GiB at L1") insists otherwise.
//
// Worked indices for the addresses we map (provenance for the table
// inventory below — and note the 4K reflexes give 72 and 512 here):
//   UART  0x0900_0000: L2 = 0x0900_0000 >> 25 = 4,  L3 = (>>14) & 0x7FF = 1024
//   RAM   0x4000_0000: L2 = 0x4000_0000 >> 25 = 32, L3 = 0
//   hole  0x6000_0000: L2 = 0x6000_0000 >> 25 = 48  (first byte past 512 MiB)

/// Index helpers, each named for its level. `& 0x7FF` keeps the compiler
/// able to see the result fits a 2048-entry table.
const fn l0i(pa: usize) -> usize {
    (pa >> 47) & 0x1
}
const fn l1i(pa: usize) -> usize {
    (pa >> 36) & 0x7FF
}
const fn l2i(pa: usize) -> usize {
    (pa >> 25) & 0x7FF
}
const fn l3i(pa: usize) -> usize {
    (pa >> 14) & 0x7FF
}

/// One translation table: 2048 descriptors, 16 KiB, aligned to its own
/// size. The alignment matters twice: the architecture requires tables
/// aligned to their size, and a misaligned base would not fault — the
/// hardware would silently mask the low bits and walk garbage.
#[repr(C, align(16384))]
struct PageTable([u64; 2048]);

const _: () = assert!(core::mem::size_of::<PageTable>() == GRANULE);
const _: () = assert!(core::mem::align_of::<PageTable>() == GRANULE);

/// The kernel's six tables, one contiguous static (96 KiB of .bss — the
/// boot stub zeroes them with everything else, which is also what keeps
/// every unwritten descriptor INVALID). One struct, not six statics, so
/// the whole tree is a single nameable object: one base, one size, for
/// `walk`'s bounds check and for your mental model alike.
#[repr(C)]
struct Tables {
    /// Level 0 root — both TTBRs during the transition, TTBR1 forever.
    /// Sixteen bytes of architectural content in a 16 KiB allocation —
    /// two 8-byte descriptors, and only one ever written (every PA we
    /// map has bit 47 clear). A 2-entry table only *needs* 16-byte
    /// alignment, but a whole granule costs nothing in zeroed .bss and
    /// removes a class of "is the root aligned enough" questions forever.
    l0: PageTable,
    /// Level 1: a single live entry ([0] -> l2; all our PAs are < 2^36).
    l1: PageTable,
    /// Level 2: the map's backbone — two table entries (UART, kernel),
    /// fifteen 32 MiB RAM blocks, one Device block (the bus-error window).
    l2: PageTable,
    /// Level 3 for the kernel's 32 MiB: where W^X and the guard live.
    l3_kernel: PageTable,
    /// Level 3 for the UART's 32 MiB of MMIO space: one live page.
    l3_mmio: PageTable,
    /// All-zero, written never: TTBR0's destination when the low half is
    /// condemned. Linux has the same object under the name
    /// `reserved_pg_dir`. If a single descriptor ever appears in here,
    /// the ground floor has silently reopened — the monitor's `low`
    /// command is the standing detector.
    empty_root: PageTable,
}

/// The tables themselves. `static mut` + raw pointers only (edition 2024
/// forbids references into `static mut`, and rightly: the hardware walker
/// is a second reader the borrow checker cannot see).
static mut TABLES: Tables = Tables {
    l0: PageTable([0; 2048]),
    l1: PageTable([0; 2048]),
    l2: PageTable([0; 2048]),
    l3_kernel: PageTable([0; 2048]),
    l3_mmio: PageTable([0; 2048]),
    empty_root: PageTable([0; 2048]),
};

// ---------------------------------------------------------------------------
// Descriptors: never hand-write a literal
// ---------------------------------------------------------------------------
//
// Every entry in a table is one u64 descriptor. Bits [1:0] say what it is
// — but the encoding is level-dependent in a way that has hurt people:
//
//   bits [1:0]   at L0..L2          at L3
//   0b00, 0b10   invalid            invalid
//   0b01         BLOCK (leaf)       *reserved, invalid*
//   0b11         table (descend)    PAGE (leaf)
//
// So a "block" and a "page" — both leaves — use *different* type bits,
// and copying an L2 block pattern into an L3 slot yields not a mapping
// but a hole. Hence two constructors that cannot be mixed up, and a
// table constructor that adds nothing but the two type bits: the
// hierarchical-control bits ([62:59]: APTable and friends) stay zero so
// that the leaf is the one and only place permissions live.

/// The four attribute bundles M2's map needs. Each picks a MAIR index,
/// an access-permission encoding (AP[7:6]: 0b00 = EL1 read-write,
/// 0b10 = EL1 read-only; the odd values would open EL0, which does not
/// exist until M5), shareability (SH[9:8]: 0b11 Inner for Normal memory;
/// 0b00 for Device, where shareability is meaningless and ignored), and
/// the execute-never pair (PXN [53], UXN [54]).
#[derive(Clone, Copy)]
enum Attr {
    /// Normal, read-only, executable (PXN=0) — .text. UXN is still set:
    /// nothing should ever execute kernel code from EL0.
    TextRx,
    /// Normal, read-only, never executable — .rodata, the DTB window.
    Ro,
    /// Normal, read-write, never executable — .data, .bss, stacks, RAM.
    Rw,
    /// Device-nGnRnE, read-write, never executable. The XN bits are not
    /// pedantry here: a speculative instruction fetch from MMIO can wedge
    /// real hardware with no fault to report (QEMU's TCG never
    /// speculates, so only the real target would have caught it).
    Device,
}

impl Attr {
    /// The lower+upper attribute bits this bundle contributes to a leaf.
    /// AF (bit 10) is baked in unconditionally and is therefore
    /// *unconstructible-off*: with TCR.HA=0 (no hardware updates — M1
    /// has none), a leaf with AF=0 faults on first touch, in a way that
    /// reads exactly like "the MMU didn't work at all".
    const fn leaf_bits(self) -> u64 {
        const AF: u64 = 1 << 10;
        const PXN: u64 = 1 << 53;
        const UXN: u64 = 1 << 54;
        const SH_INNER: u64 = 0b11 << 8;
        const AP_RO: u64 = 0b10 << 6;
        const ATTR_DEVICE: u64 = 1 << 2; // AttrIndx = 1 (MAIR slot 1)
        match self {
            Attr::TextRx => AF | SH_INNER | AP_RO | UXN,
            Attr::Ro => AF | SH_INNER | AP_RO | PXN | UXN,
            Attr::Rw => AF | SH_INNER | PXN | UXN,
            Attr::Device => AF | ATTR_DEVICE | PXN | UXN,
        }
    }
}

/// A table descriptor: the PA of the next-level table, the two type bits,
/// and *nothing else* — see the hierarchical-bits note above.
const fn table_desc(pa: u64) -> u64 {
    pa | 0b11
}

/// A level-2 BLOCK descriptor: a 32 MiB leaf. Type bits 0b01 — valid
/// only at L2 (at 16K/DS=0 there are no L1 blocks, and at L3 this very
/// bit pattern decodes as *invalid*; use [`page_desc`] there).
const fn block_desc(pa: u64, a: Attr) -> u64 {
    // OA[47:25]: a block's output address must be block-aligned — the low
    // 25 bits of `pa` land in attribute-bit territory, so a ragged PA
    // would not map "slightly wrong", it would corrupt the descriptor.
    // Assert rather than mask: masking would hide the bug. (Unfireable
    // today — both call sites pass `slot << 25`-shaped addresses, and a
    // low-world panic must never fire; the assert exists for the day a
    // refactor breaks that, to be caught in identity-linked rehearsal
    // builds where panics still print.)
    debug_assert!(pa & (L2_BLOCK as u64 - 1) == 0);
    pa | a.leaf_bits() | 0b01
}

/// A level-3 PAGE descriptor: a 16 KiB leaf. Type bits 0b11.
const fn page_desc(pa: u64, a: Attr) -> u64 {
    pa | a.leaf_bits() | 0b11
}

// The worked values from the design, pinned: if a refactor shifts a bit,
// the build fails with the expected constant in the message.
const _: () = assert!(page_desc(0, Attr::TextRx) == 0x783 | 1 << 54);
const _: () = assert!(page_desc(0, Attr::Ro) == 0x783 | 1 << 53 | 1 << 54);
const _: () = assert!(page_desc(0, Attr::Rw) == 0x703 | 1 << 53 | 1 << 54);
const _: () = assert!(page_desc(0, Attr::Device) == 0x407 | 1 << 53 | 1 << 54);
const _: () = assert!(block_desc(0, Attr::Rw) == 0x701 | 1 << 53 | 1 << 54);
const _: () = assert!(block_desc(0, Attr::Device) == 0x405 | 1 << 53 | 1 << 54);
/// One L2 block covers 2^25 bytes = 32 MiB.
const L2_BLOCK: usize = 1 << 25;
const _: () = assert!(L2_BLOCK == 32 * 1024 * 1024);

// ---------------------------------------------------------------------------
// LOW WORLD — code the boot stub runs below the kernel's link address
// ---------------------------------------------------------------------------
//
// The two `extern "C"` functions below are called by boot.rs at their
// *physical* addresses, MMU off, while the image is linked to run high.
// All *code* the compiler emits is PC-relative (adrp/adr/bl) and runs
// correctly displaced; what breaks is *data* holding absolute addresses
// (R_AARCH64_ABS64): format-machinery vtables, `panic::Location` strings,
// function pointers, statics containing references. Hence the contract:
//
//   1. No `println!`/`format_args!`, no trait objects, no function
//      pointers, no reference-holding statics or consts.
//   2. No panic may FIRE. The compiled-in checks are harmless (their
//      `bl` is PC-relative) as long as they never trigger: so raw-pointer
//      writes instead of slice indexing, and arithmetic that provably
//      cannot overflow. A pre-banner hang's prime suspect is always
//      "a panic path fired down here".
//   3. Linker symbols read via `&raw` at a low PC yield PHYSICAL
//      addresses for free — adrp's link-time displacement cancels at run
//      time. The same line executed high yields the virtual address. The
//      builder below leans on this: it works purely in PAs, which is
//      exactly the currency the hardware walker spends.
//   4. The one sanctioned debug channel is the zero-sized PL011 at its
//      physical base: raw bytes, no formatting, no statics.
//
// Nothing enforces this but convention and tripwires (the SIZEOF
// ASSERT in linker.ld, the post-link audit in docs/04 §5, and the staged
// bring-up that debugged this code in the unrestricted identity world
// first). Stable Rust with zero crates cannot make it a build error;
// we make it a loud comment instead.

/// Emergency console for the low world: the PL011 at its physical base.
/// A ZST with no statics behind it — see hal/pl011.rs for why that makes
/// it callable from anywhere, including here.
type LowConsole = crate::hal::pl011::Pl011<UART0_BASE>;

/// Build the kernel's page tables. LOW WORLD — see the contract above.
///
/// Fills [`TABLES`] from linker symbols and board constants, and returns
/// the root's physical address for [`opal_mmu_enable`] / TTBR0 / TTBR1.
///
/// # Safety
/// Call exactly once, from the boot stub (step 6), at a low PC, with the
/// MMU off and `.bss` already zeroed (the zeroing is what makes every
/// descriptor we *don't* write INVALID).
#[unsafe(no_mangle)] // boot.rs reaches this symbol through a literal pool
pub unsafe extern "C" fn opal_build_tables() -> u64 {
    // ---- 0. The -cpu max receipt -----------------------------------------
    // .cargo/config.toml promised the 16 KiB granule; ID_AA64MMFR0_EL1 is
    // where the CPU itself answers. TGran16 (bits [23:20]) is 0b0000 when
    // 16K is absent — anything else means present (0b0001 = plain
    // support, which M1 reports; 0b0010 = 16K+LPA2, which QEMU max
    // reports; testing `== 0b0001` would reject QEMU. And TGran4/TGran64
    // use an *inverted* convention where 0b0000 means present — never
    // share a decoder between these fields). No 16K granule = nothing
    // below can work: say so in raw bytes and park.
    let mmfr0: u64;
    // SAFETY: reading an ID register is always legal and side-effect-free.
    unsafe {
        core::arch::asm!("mrs {v}, ID_AA64MMFR0_EL1", v = out(reg) mmfr0,
                         options(nomem, nostack, preserves_flags));
    }
    if (mmfr0 >> 20) & 0xF == 0b0000 {
        // "MMU?G": MMU bring-up failed, Granule missing. Five raw bytes —
        // the most a world without format machinery owes anyone. Linux's
        // __enable_mmu parks with a status code for the same reason.
        let uart = LowConsole::new();
        for b in *b"MMU?G" {
            uart.write_byte(b);
        }
        super::park();
    }

    // ---- 1. Where the kernel's pieces are --------------------------------
    // Linker symbols, read at a low PC: physical addresses (contract
    // rule 3). The W^X boundaries are 16 KiB-aligned by linker.ld ASSERTs.
    unsafe extern "C" {
        static __text_end: u8;
        static __rodata_end: u8;
        static __stack_guard_bottom: u8;
        static __stack_bottom: u8;
        static __stack_top: u8;
    }
    // `expose_provenance`, not `as usize`: the hardware walker will read
    // table memory through these integers' descendants, and `walk` later
    // re-derives pointers from descriptor PAs — both want the provenance
    // exposed, the same discipline as every MMIO access in hal/pl011.rs.
    // (Taking a symbol's address is safe; only reading *through* an
    // extern static would need unsafe, and we never do.)
    let text_end = (&raw const __text_end).expose_provenance();
    let rodata_end = (&raw const __rodata_end).expose_provenance();
    let guard_bottom = (&raw const __stack_guard_bottom).expose_provenance();
    let stack_bottom = (&raw const __stack_bottom).expose_provenance();
    let stack_top = (&raw const __stack_top).expose_provenance();

    // The kernel image must fit the one L3-refined 32 MiB; linker.ld
    // ASSERTs the same thing at link time with better error messages.
    let image_base = RAM_BASE; // the L2 slot the kernel's L3 table refines
    // 0x20_0000 is linker.ld's DTB room below the load address — the
    // "2 MiB story" in its header. Change them together.
    let text_start = image_base + 0x20_0000;

    // ---- 2. Leaves first: the kernel's 32 MiB, 16 KiB at a time ----------
    // 2048 pages over [0x4000_0000, 0x4200_0000), each picked by where it
    // falls in the linker's layout. Raw-pointer writes (contract rule 2);
    // the cursor arithmetic cannot overflow (2048 * 16 KiB added to a
    // 30-bit base).
    // SAFETY (this and every `&raw` projection into TABLES below): single
    // core, MMU off, exactly one caller (the boot stub) — no aliasing
    // reader exists yet except the hardware walker, which only starts
    // looking after opal_mmu_enable's barriers publish the finished tree.
    let l3k = unsafe { &raw mut TABLES.l3_kernel.0 } as *mut u64;
    let mut i = 0usize;
    while i < 2048 {
        let pa = image_base + (i << 14);
        let desc = if pa < text_start {
            // The DTB window: QEMU's devicetree, read-only — corrupting
            // the boot evidence should take effort. (M4 reads it for real.)
            page_desc(pa as u64, Attr::Ro)
        } else if pa < text_end {
            page_desc(pa as u64, Attr::TextRx)
        } else if pa < rodata_end {
            page_desc(pa as u64, Attr::Ro)
        } else if pa < guard_bottom {
            page_desc(pa as u64, Attr::Rw)
        } else if pa < stack_bottom {
            // THE GUARD: the 16 KiB below the stack stays exactly as the
            // boot stub left it — an all-zero, INVALID descriptor. Not a
            // permission, an absence: overflow now has somewhere to fault
            // into instead of eating .bss. M0's known debt, paid.
            i += 1;
            continue;
        } else {
            // Stack, then everything to the end of the 32 MiB: plain RAM.
            page_desc(pa as u64, Attr::Rw)
        };
        unsafe { l3k.add(i).write(desc) };
        i += 1;
    }
    // The guard hole is real only if the linker put the symbols where the
    // map expects them: one page wide, directly below the stack.
    // (Contract rule 2 — these cannot fire; they are here for the day a
    // linker.ld edit makes them fire in the *identity-linked* debug
    // worlds of stage A/B, where panics still print.)
    debug_assert!(stack_bottom - guard_bottom == GRANULE);
    debug_assert!(stack_top - image_base <= L2_BLOCK);

    // ---- 3. The UART's L3 table: one live page ---------------------------
    // Map what we use, not the neighborhood: one Device-nGnRnE page at
    // the PL011. (M3's GIC at 0x0800_0000 lands in this same table —
    // a few more entries then, zero new tables.)
    let l3m = unsafe { &raw mut TABLES.l3_mmio.0 } as *mut u64;
    unsafe { l3m.add(l3i(UART0_BASE)).write(page_desc(UART0_BASE as u64, Attr::Device)) };

    // ---- 4. The L2 backbone ----------------------------------------------
    let l2 = unsafe { &raw mut TABLES.l2.0 } as *mut u64;
    // The two refined regions descend to L3 tables...
    let l3k_pa = unsafe { &raw const TABLES.l3_kernel }.expose_provenance();
    let l3m_pa = unsafe { &raw const TABLES.l3_mmio }.expose_provenance();
    unsafe {
        l2.add(l2i(UART0_BASE)).write(table_desc(l3m_pa as u64));
        l2.add(l2i(image_base)).write(table_desc(l3k_pa as u64));
    }
    // ...the rest of RAM is 32 MiB blocks, the whole point of blocks...
    let mut slot = l2i(image_base) + 1;
    while slot < l2i(RAM_BASE + RAM_SIZE) {
        let pa = (slot << 25) as u64;
        unsafe { l2.add(slot).write(block_desc(pa, Attr::Rw)) };
        slot += 1;
    }
    // ...and one block PAST the end of RAM: the bus-error window. Mapped
    // (the walk succeeds) but unbacked (the bus does not answer), it
    // preserves M1's `abort` demo in a world where unmapped addresses
    // die politely at translation: reads here come back as a synchronous
    // external abort, DFSC 0x10 — the bus's "no", as opposed to the
    // page-table's. Device, so nothing prefetches into it; derived from
    // RAM_BASE + RAM_SIZE, so it tracks the -m flag it leans on (a
    // labeled simulator-ism: real RAM ends wherever the DTB says).
    unsafe {
        l2.add(l2i(RAM_BASE + RAM_SIZE))
            .write(block_desc((RAM_BASE + RAM_SIZE) as u64, Attr::Device));
    }

    // ---- 5. The trunk ------------------------------------------------------
    let l2_pa = unsafe { &raw const TABLES.l2 }.expose_provenance();
    let l1_pa = unsafe { &raw const TABLES.l1 }.expose_provenance();
    let l1 = unsafe { &raw mut TABLES.l1.0 } as *mut u64;
    let l0 = unsafe { &raw mut TABLES.l0.0 } as *mut u64;
    unsafe {
        l1.add(l1i(RAM_BASE)).write(table_desc(l2_pa as u64)); // l1[0]
        l0.add(l0i(RAM_BASE)).write(table_desc(l1_pa as u64)); // l0[0]
    }

    // The root, as the integer the TTBRs will hold. Bits [63:48] of a
    // TTBR are the ASID (zero until M5 wants one) and bit 0 is CnP
    // (zero: no shared-TLB pact until there is a second core to share
    // with) — for us, exactly the table's PA.
    unsafe { &raw const TABLES.l0 }.expose_provenance() as u64
}

/// Turn the MMU and caches on. LOW WORLD — see the contract above.
///
/// One `asm!` block, because the sequence is the lesson and nothing may
/// wander between its lines — least of all between the SCTLR write and
/// its ISB. Returns with translation live, still executing at the low PC
/// through the identity trunk of the shared tree.
///
/// # Safety
/// `root_pa` must be the value [`opal_build_tables`] returned, this call
/// must follow it, and VBAR_EL1 should already hold the (low) vector
/// table so that a fault after the enabling ISB gets M1's reporter
/// instead of a hang.
#[unsafe(no_mangle)] // boot.rs reaches this symbol through a literal pool
pub unsafe extern "C" fn opal_mmu_enable(root_pa: u64) {
    // IPS, the one runtime field: clamp the CPU's reported physical range
    // to 48 bits, because with DS=0 the translation system cannot emit
    // more than 48 anyway. PARange lives in ID_AA64MMFR0_EL1[3:0] with an
    // IRREGULAR encoding — compare encodings, never bit-counts:
    //   0b000=32  0b001=36 (M1)  0b010=40  0b011=42 (not 44!)
    //   0b100=44  0b101=48      0b110=52 (QEMU -cpu max)
    // The ARM ARM lets an over-large IPS *work* on most cores but ends
    // the paragraph "software must not rely on this" — so we clamp.
    let mmfr0: u64;
    // SAFETY: ID register read; always legal, no side effects.
    unsafe {
        core::arch::asm!("mrs {v}, ID_AA64MMFR0_EL1", v = out(reg) mmfr0,
                         options(nomem, nostack, preserves_flags));
    }
    let parange = mmfr0 & 0xF;
    let ips = if parange < 0b101 { parange } else { 0b101 };
    let tcr = TCR_EL1_BASE | (ips << 32);

    // The invalidation sweep's bounds: everything the MMU-off world wrote
    // that the MMU-on world reads back. That is all of .bss — the stub
    // zeroed it, and the page tables live in it — plus the guard and the
    // boot stack: this very function's prologue may spill x30 there with
    // the caches off (debug builds do), and its epilogue reloads it
    // *through the cache* after the enable. The linker lays those out
    // contiguously, [__bss_start, __stack_top); read at a low PC the
    // symbols are PAs. (.data needs no sweep of ours: the Linux boot
    // protocol m1n1 follows obliges the *loader* to clean the image it
    // copied to the point of coherency, and QEMU writes guest RAM from
    // the host side, no guest cache involved.)
    unsafe extern "C" {
        static __bss_start: u8;
        static __stack_top: u8;
    }
    let sweep_base = (&raw const __bss_start).expose_provenance();
    let sweep_end = (&raw const __stack_top).expose_provenance();

    // SAFETY: the ceremony. Every register written here is EL1-writable;
    // the tables are fully built (caller contract); the instruction after
    // the final ISB fetches through the identity trunk, which maps this
    // very code RX. The asm block is not `nomem`: it must be a compiler
    // barrier, so every table store above is materialized before the
    // walker can look.
    unsafe {
        core::arch::asm!(
            // ---- 0. Make the MMU-off world's writes unshadowable -----------
            // Two observers need this memory to be exactly what we stored:
            // the walker (a second reader with its own path to the tables)
            // and our own post-enable selves (the first cached read of a
            // zeroed static — or of this function's spilled return address
            // — must not hit a leftover line). Every byte in the sweep was
            // written with SCTLR.C=0, straight to memory; any cache line
            // over it predates us — firmware leftovers, architecturally
            // UNKNOWN at reset, the same distrust that motivates the
            // `ic iallu` and `tlbi` below. `dc ivac` (invalidate by VA to
            // point of coherency) discards them. Invalidate, NOT clean-
            // and-invalidate: cleaning would write those stale leftovers
            // back OVER our live stores — for a *dirty* stale line, CIVAC
            // here is precisely the corruption it sounds like it prevents.
            // (Linux's dcache_inval_poc uses civac only at unaligned
            // region *edges*; our bounds are granule-aligned linker
            // symbols, so the edge case cannot arise.) A labeled
            // simulator-ism: TCG implements every cache op as a NOP, so
            // QEMU passing this loop proves nothing — the first real test
            // of these five lines is M7.
            "dmb sy",                    // order table stores before dc ops
            "mrs {t}, CTR_EL0",
            "ubfx {t}, {t}, #16, #4",    // DminLine: log2(line / 4 bytes)
            "mov {s}, #4",
            "lsl {s}, {s}, {t}",         // stride = line size in bytes
            "mov {p}, {base}",
            "2:",
            "dc ivac, {p}",
            "add {p}, {p}, {s}",
            "cmp {p}, {end}",
            "b.lo 2b",
            "dsb sy",                    // dc ops complete before anything else
            // ---- 1. Program the translation machinery ---------------------
            // Order among these four is free: they are direct writes that
            // the walker's indirect reads only observe at the next context
            // synchronization event (the ISB below).
            "msr MAIR_EL1, {mair}",
            "msr TCR_EL1, {tcr}",
            "msr TTBR0_EL1, {root}",     // one tree, two trunks: the identity
            "msr TTBR1_EL1, {root}",     // alias and the kernel's home
            // ---- 2. Flush the TLB's reset junk ----------------------------
            // TLB contents out of reset are UNKNOWN; translation must not
            // start against leftovers. (QEMU masks this whole class — TCG
            // flushes its TLB on the SCTLR write — so, again, passing
            // here is not evidence.)
            "dsb ishst",                 // table stores visible before TLBI
            "tlbi vmalle1",              // this PE, EL1&0, all VAs. Local on
                                         // purpose: each core flushes its own
                                         // reset junk (M6 keeps this line)
            "dsb nsh",                   // TLBI is only *done* after a DSB
            "isb",                       // LOAD-BEARING, not folklore: without
                                         // a synchronization event here, the
                                         // fetch path may see M=1 while still
                                         // holding reset-UNKNOWN TTBR/TCR
                                         // values — and refill the TLB from
                                         // garbage AFTER our flush. ATF, m1n1
                                         // and Linux all carry this ISB.
            // ---- 3. Instruction-cache hygiene ------------------------------
            // Cold-boot I-cache state is UNKNOWN too, and I-fetch can hit
            // it the moment SCTLR.I goes high. Self-reliance over trust
            // in any loader's leftovers: invalidate it all.
            "ic iallu",
            "dsb nsh",
            "isb",
            // ---- 4. The cliff ----------------------------------------------
            // M | C | I, one write, from scratch. The very next fetch —
            // the ISB's successor, at this same low PC — already
            // translates: through TTBR0, the identity trunk, into the
            // page that linker.ld maps RX. That sentence is the whole
            // reason the shared tree exists.
            "msr SCTLR_EL1, {sctlr}",
            "isb",
            t = out(reg) _,
            s = out(reg) _,
            p = out(reg) _,
            base = in(reg) sweep_base,
            end = in(reg) sweep_end,
            mair = in(reg) MAIR_EL1_VALUE,
            tcr = in(reg) tcr,
            root = in(reg) root_pa,
            sctlr = in(reg) SCTLR_EL1_VALUE,
            // No preserves_flags: the dc loop's `cmp` is honest work.
            options(nostack),
        );
    }
}

// ---------------------------------------------------------------------------
// High world: condemnation, oracles, and read-back
// ---------------------------------------------------------------------------

/// Swap TTBR0 to the permanently-empty root: from this line on, the low
/// half of the address space does not translate, and the kernel lives
/// only upstairs. The transition scaffolding — one tree, two trunks —
/// loses its second trunk.
///
/// This is also the kernel's first *live* translation change, and it is
/// deliberately the gentle kind: a whole-root replacement plus TLB flush
/// (the same shape as M5's per-task TTBR0 switch, rehearsed here once),
/// not an edit of an in-use entry (which would need break-before-make —
/// docs/04 §8 explains why we teach BBM but never need it in M2).
///
/// Caller contract (kmain's ordering is load-bearing): by the time this
/// runs, PC, SP, and VBAR_EL1 are all high, and nothing after it may
/// dereference a low VA — the DTB pointer survives only as a PA *value*
/// to feed [`phys_to_virt`].
pub fn condemn_low_half() {
    // The empty root's PA, derived from its high alias: this code runs
    // upstairs, so `&raw` yields a VA and virt_to_phys does the rest.
    // SAFETY: address-taking only; nothing reads or writes the table.
    let empty = virt_to_phys(unsafe { &raw const TABLES.empty_root }.expose_provenance()) as u64;
    // SAFETY: writing TTBR0_EL1 at EL1 is always legal; the new root is a
    // valid (all-invalid) table; nothing executes or loads through TTBR0
    // anymore (caller contract above). The TLBI evicts every cached
    // low-half translation; dsb nsh completes it before the ISB lets
    // anything new fetch. Not `nomem`: the compiler must not float memory
    // accesses across a translation-regime change.
    unsafe {
        core::arch::asm!(
            "msr TTBR0_EL1, {root}",
            "isb",
            "tlbi vmalle1",
            "dsb nsh",
            "isb",
            root = in(reg) empty,
            options(nostack, preserves_flags),
        );
    }
}

/// Ask the hardware to translate `va` exactly as a kernel read would be:
/// `AT S1E1R` runs the real walk (TLB included) and parks the answer in
/// PAR_EL1 — translation as a *question* instead of a leap of faith.
/// `Ok(pa)`, or `Err(raw PAR)` whose FST field (bits [6:1]) speaks the
/// same DFSC language as the fault reports.
pub fn translate(va: u64) -> Result<u64, u64> {
    let par: u64;
    // SAFETY: AT S1E1R is legal at EL1 for any VA and faults nothing —
    // failure is a bit in PAR_EL1, not an exception. The ISB pins the
    // PAR read after the walk; both stay in one asm block so no other
    // PAR-writing operation can interleave. Not `nomem` (AT walks the
    // tables, which are memory the compiler knows we wrote).
    unsafe {
        core::arch::asm!(
            "at s1e1r, {va}",
            "isb",
            "mrs {par}, PAR_EL1",
            va = in(reg) va,
            par = out(reg) par,
            options(nostack, preserves_flags),
        );
    }
    if par & 1 == 0 {
        // PA[47:12] from PAR, the rest of the offset from the VA. (PAR
        // reports 4 KiB frames regardless of granule — bits [13:12] of a
        // 16K mapping simply arrive via the same field.)
        Ok((par & 0x0000_FFFF_FFFF_F000) | (va & 0xFFF))
    } else {
        Err(par)
    }
}

/// Walk the live tables in software and narrate every step: which root,
/// each level's index, the raw descriptor, and its decode — then
/// cross-check the destination against [`translate`], the hardware's
/// answer. Two implementations of the same walk cannot drift silently
/// when one is printed next to the other.
pub fn walk(va: u64) {
    // Which trunk? Bits [63:48] pick the regime — and nothing else does.
    let (name, ttbr) = match va >> 48 {
        0xFFFF => ("TTBR1", ttbr1()),
        0x0000 => ("TTBR0", ttbr0()),
        _ => {
            println!(
                "  {va:#018x} is non-canonical: bits [63:48] are neither all-ones \
                 nor all-zeros, so no trunk claims it — translation faults at level 0"
            );
            return;
        }
    };
    // A TTBR holds more than a pointer: ASID in [63:48], CnP in bit 0.
    // Both are zero in M2, but the mask documents the difference between
    // "the register" and "the root's address".
    let mut table_pa = ttbr & 0x0000_FFFF_FFFF_FFFE;
    println!("  {name} -> root table at PA {table_pa:#x}");

    // Table memory is only readable through its high alias now. We hand
    // out pointers derived from the descriptors' PAs, but only after
    // checking they stay inside our six tables — a wild table pointer
    // here means the tables are corrupt, and reporting that beats
    // faulting on it.
    let tables_pa = virt_to_phys((&raw const TABLES).expose_provenance()) as u64;
    let tables_len = core::mem::size_of::<Tables>() as u64;

    let idx = [
        ((va >> 47) & 0x1, "L0"),
        ((va >> 36) & 0x7FF, "L1"),
        ((va >> 25) & 0x7FF, "L2"),
        ((va >> 14) & 0x7FF, "L3"),
    ];
    for (level, (i, lname)) in idx.iter().enumerate() {
        if table_pa < tables_pa || table_pa >= tables_pa + tables_len {
            println!("  {lname}: table PA {table_pa:#x} is outside the kernel's six tables — corrupt descriptor?");
            return;
        }
        // SAFETY: in range of TABLES (checked above), whose provenance
        // was exposed when the builder created it; read through the high
        // alias because the low one no longer translates. Volatile: the
        // hardware walker writes nothing here (HA=0), but this read must
        // happen for the narration to be evidence, not optimization fruit.
        let desc = unsafe {
            core::ptr::with_exposed_provenance::<u64>(phys_to_virt((table_pa + i * 8) as usize))
                .read_volatile()
        };
        let pa_field = desc & 0x0000_FFFF_FFFF_C000;
        match (desc & 0b11, level) {
            (0b00, _) | (0b10, _) => {
                println!("  {lname}[{i}] = {desc:#018x} — INVALID: the walk ends here in a translation fault (level {level})");
                return;
            }
            (0b01, 3) => {
                println!("  {lname}[{i}] = {desc:#018x} — bits[1:0]=0b01 at L3 is RESERVED-invalid (a block pattern in a page slot): translation fault");
                return;
            }
            (0b01, 2) => {
                // A block's output address starts at bit 25, not 14 —
                // print the block mask, not the page mask.
                let block_pa = desc & 0x0000_FFFF_FE00_0000;
                println!("  {lname}[{i}] = {desc:#018x} — 32 MiB block at PA {block_pa:#x} {}", decode_leaf(desc));
                finish_walk(va, block_pa | (va & 0x1FF_FFFF));
                return;
            }
            (0b01, _) => {
                println!("  {lname}[{i}] = {desc:#018x} — a block descriptor at {lname}?! 16K/DS=0 has no blocks above L2: reserved, translation fault");
                return;
            }
            (0b11, 3) => {
                println!("  {lname}[{i}] = {desc:#018x} — 16 KiB page at PA {pa_field:#x} {}", decode_leaf(desc));
                finish_walk(va, pa_field | (va & 0x3FFF));
                return;
            }
            (_, _) => {
                println!("  {lname}[{i}] = {desc:#018x} — table: descend to PA {pa_field:#x}");
                table_pa = pa_field;
            }
        }
    }
}

/// The tail of a successful software walk: print our PA, then ask the
/// hardware the same question and print whether the answers agree.
fn finish_walk(va: u64, soft_pa: u64) {
    match translate(va) {
        Ok(hw_pa) if hw_pa == soft_pa => {
            println!("  software walk says PA {soft_pa:#x}; AT S1E1R agrees — the narration above is the real mechanism");
        }
        Ok(hw_pa) => {
            println!("  software walk says PA {soft_pa:#x}, but AT S1E1R says {hw_pa:#x} — ONE OF THEM IS WRONG; trust the hardware, debug the narrator");
        }
        Err(par) => {
            println!("  software walk says PA {soft_pa:#x}, but AT S1E1R FAULTS (PAR {par:#018x}) — the walker sees something the narrator missed");
        }
    }
}

/// Decode a leaf descriptor's attribute bits for the walk narration.
fn decode_leaf(desc: u64) -> &'static str {
    let device = desc & (0b111 << 2) == 1 << 2; // AttrIndx == 1
    let ro = desc & (0b11 << 6) == 0b10 << 6;
    let px = desc & (1 << 53) == 0;
    match (device, ro, px) {
        (true, _, _) => "[Device-nGnRnE RW XN AF]",
        (false, true, true) => "[Normal RO +execute AF]",
        (false, true, false) => "[Normal RO XN AF]",
        (false, false, false) => "[Normal RW XN AF]",
        (false, false, true) => "[Normal RW +execute AF — should not exist in this kernel]",
    }
}

/// The `mrs` boilerplate, once. A macro instead of a function taking the
/// register name because `mrs` encodes its operand in the instruction —
/// there is no register-indirect form for the assembler to fall back on.
/// (Defined above its users: `macro_rules!` is visible only to code that
/// comes after it in source order — main.rs learned this in M1.)
macro_rules! read_sysreg {
    ($name:literal) => {{
        let v: u64;
        // SAFETY: reading any of the registers this module passes here
        // (SCTLR/TCR/TTBRn/ID_*) at EL1 is legal and side-effect-free.
        unsafe {
            core::arch::asm!(concat!("mrs {v}, ", $name), v = out(reg) v,
                             options(nomem, nostack, preserves_flags));
        }
        v
    }};
}

/// Read SCTLR_EL1 back, for the banner: print what the register holds,
/// not what we hope the enable ceremony wrote. (Same habit as `vbar()`.)
pub fn sctlr() -> u64 {
    read_sysreg!("SCTLR_EL1")
}

/// Read TCR_EL1 back, for the banner.
pub fn tcr() -> u64 {
    read_sysreg!("TCR_EL1")
}

/// Read TTBR0_EL1 back: after [`condemn_low_half`], the empty root.
pub fn ttbr0() -> u64 {
    read_sysreg!("TTBR0_EL1")
}

/// Read TTBR1_EL1 back: the kernel's tree, as a physical address — the
/// walker speaks PA, and the banner says so.
pub fn ttbr1() -> u64 {
    read_sysreg!("TTBR1_EL1")
}

/// Read ID_AA64MMFR0_EL1, for the banner's PARange line.
pub fn id_aa64mmfr0() -> u64 {
    read_sysreg!("ID_AA64MMFR0_EL1")
}
