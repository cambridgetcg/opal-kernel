//! Flattened Devicetree (FDT) parser — Milestone 4.
//!
//! The devicetree is the bootloader's one-page description of the machine:
//! how much RAM, where the UART is, what interrupt controller, which timer
//! interrupt. Until now Opal has *hardcoded* every one of those addresses
//! (`board::virt`) with an honest comment saying "the FDT parser is still
//! milestones away." This module is that milestone.
//!
//! ## What an FDT looks like in memory
//!
//! A DTB ("device tree blob") is a single contiguous block of bytes, laid
//! out by the devicetree specification (devicetree.org/specifications/) as
//! three regions back to back:
//!
//! ```text
//! +------------------+
//! | header (40 B)    |  10 × u32, all big-endian
//! +------------------+
//! | memory reserve   |  array of 16-byte entries (addr u64, size u64),
//! | map              |  terminated by an all-zeros entry
//! +------------------+
//! | structure block  |  tokens (u32 big-endian): BEGIN_NODE, PROP, ...
//! +------------------+
//! | string block     |  null-terminated strings, deduplicated
//! +------------------+
//! ```
//!
//! The header gives the byte offsets of each region. The structure block
//! is a flat token stream that encodes a tree: `BEGIN_NODE "name" PROP
//! "key" <bytes> ... END_NODE` nests recursively. Property names and
//! `compatible` strings live in the string block, referenced by offset —
//! the same string appears once and is shared by every property that uses
//! it.
//!
//! ## How we read it
//!
//! QEMU places the DTB at the start of RAM (`0x4000_0000`). Since M2 the
//! kernel reads all of RAM through its higher-half alias
//! (`phys_to_virt`), where the DTB window is mapped read-only Normal
//! memory. Every load goes through `read_volatile` — this memory was
//! written by QEMU, not by Rust code the compiler knows about, and the
//! spec's big-endian integers must be byte-swapped on our little-endian
//! CPU.
//!
//! ## What this parser is, and is not
//!
//! This is a *minimal* reader: walk the tree, find a node by path, read a
//! property. It does not support overlays, writing, or the full PHANDLE
//! reference resolution that a production kernel needs. It is enough to
//! replace the hardcoded addresses in `board::virt` with values QEMU
//! actually declared — which is the lesson of M4.
//!
//! ## Safety
//!
//! All offsets are validated against `header.totalsize` before any
//! dereference. The DTB lives in mapped RAM for the kernel's lifetime, so
//! once `new()` accepts the blob, every subsequent access is safe by
//! construction. The `Node` and iterator types carry no raw pointers of
//! their own — they are offsets into a `Fdtr` whose lifetime ties them to
//! the blob — so the borrow checker enforces "don't outlive the tree."

use super::mmu::phys_to_virt;
use core::ptr;

// -----------------------------------------------------------------------
// Token constants — the structure block's vocabulary
// -----------------------------------------------------------------------

/// A string of spaces for tree indentation, without allocation.
/// `no_std` has no `str::repeat` (that's in alloc), so we slice a
/// pre-made buffer. Depth is bounded by the FDT's nesting, which is
/// always small (QEMU's virt tree is 3 levels deep).
fn indent_str(depth: usize) -> &'static str {
    const SPACES: &str = "                                                                ";
    &SPACES[..(depth * 2).min(SPACES.len())]
}

/// Marks the start of a node. Followed by the node's name as a
/// null-terminated string, padded to 4-byte alignment.
const FDT_BEGIN_NODE: u32 = 0x1;

/// Marks the end of a node. The matching `BEGIN_NODE` is the most recent
/// unclosed one — the structure block is properly nested.
const FDT_END_NODE: u32 = 0x2;

/// A property. Followed by a 12-byte header (len, nameoff) and `len`
/// bytes of data, padded to 4-byte alignment. `nameoff` is an offset
/// into the string block.
const FDT_PROP: u32 = 0x3;

/// A no-op. Used for alignment or to fill gaps left by property removal.
/// The parser skips them.
const FDT_NOP: u32 = 0x4;

/// The end of the structure block. Exactly one, at the top level.
const FDT_END: u32 = 0x9;

/// The FDT magic number, stored big-endian in the first header word.
const FDT_MAGIC: u32 = 0xd00d_feed;

// -----------------------------------------------------------------------
// Header
// -----------------------------------------------------------------------

/// The 40-byte FDT header, laid out as the spec defines it. Every field
/// is a big-endian u32; we byte-swap each one on read. Some fields
/// (magic, version, ...) are read during validation but never used
/// again — they stay in the struct because the header is a single
/// thing, and caching it whole is cleaner than remembering which fields
/// we dropped.
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
struct Header {
    /// `0xd00d_feed` — the "is this really a DTB?" canary.
    magic: u32,
    /// Total size of the entire DTB blob, including the header.
    totalsize: u32,
    /// Byte offset of the structure block from the start of the blob.
    off_dt_struct: u32,
    /// Byte offset of the string block.
    off_dt_strings: u32,
    /// Byte offset of the memory reserve map.
    off_mem_rsvmap: u32,
    /// DTB format version (17 in modern tooling).
    version: u32,
    /// Last compatible version (16 — the format has been stable since).
    last_comp_version: u32,
    /// Which physical CPU ID booted. Useful for SMP init; unused here.
    boot_cpuid_phys: u32,
    /// Size of the string block in bytes.
    size_dt_strings: u32,
    /// Size of the structure block in bytes.
    size_dt_struct: u32,
}

impl Header {
    /// The header is 10 u32s = 40 bytes. Pinned with a const assert so a
    /// struct layout change is caught at compile time.
    const SIZE: usize = 40;
}

// -----------------------------------------------------------------------
// Reserve map entry
// -----------------------------------------------------------------------

/// One entry in the memory reserve map: a region of physical memory that
/// the bootloader has reserved and the OS must not touch until it has
/// read and honored the map. In practice QEMU's reserve map is empty
/// (terminated by the all-zeros entry), but a real bootloader can use
/// it to protect initrd images or the DTB itself.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct ReserveEntry {
    /// Physical address of the reserved region.
    pub address: u64,
    /// Size in bytes. Zero with a zero address means "end of map."
    pub size: u64,
}

// -----------------------------------------------------------------------
// Low-level read helpers
// -----------------------------------------------------------------------

/// Read a big-endian u32 from the DTB at byte `offset`, after bounds
/// checking against `totalsize`. Returns `None` on any out-of-bounds
/// access — the parser never panics on a corrupt blob.
///
/// # Safety
///
/// `base` must be a valid readable virtual address for at least
/// `totalsize` bytes. The caller (`Fdtr::new`) validates this once.
unsafe fn read_be_u32(base: usize, offset: usize, totalsize: usize) -> Option<u32> {
    // Use checked_add to prevent overflow: a malicious FDT could set
    // offset to a value near usize::MAX so that offset + 4 wraps to a
    // small number, bypassing the bounds check and causing an OOB read.
    let end = offset.checked_add(4)?;
    if end > totalsize {
        return None;
    }
    // SAFETY: bounds-checked above; `base` is a valid readable VA for
    // `totalsize` bytes (validated in `Fdtr::new`). Volatile because
    // this is QEMU-written memory, not Rust-written.
    let raw = unsafe { ptr::with_exposed_provenance::<u32>(base + offset).read_volatile() };
    Some(u32::from_be(raw))
}

/// Read a big-endian u64 from the DTB at byte `offset`, bounds-checked.
///
/// # Safety
///
/// Same contract as [`read_be_u32`].
#[allow(dead_code)]
unsafe fn read_be_u64(base: usize, offset: usize, totalsize: usize) -> Option<u64> {
    // Use checked_add to prevent overflow (same rationale as read_be_u32).
    let end = offset.checked_add(8)?;
    if end > totalsize {
        return None;
    }
    // SAFETY: bounds-checked above; `base` is a valid readable VA.
    let raw = unsafe { ptr::with_exposed_provenance::<u64>(base + offset).read_volatile() };
    Some(u64::from_be(raw))
}

/// Read a null-terminated string from the DTB starting at byte `offset`,
/// bounds-checked against `totalsize`. Returns a `&str` borrowed from the
/// blob's lifetime.
///
/// # Safety
///
/// `base` must be valid and readable for `totalsize` bytes. The string
/// must be null-terminated within that range; if it is not, we return
/// `None` rather than running off the end.
unsafe fn read_cstr<'a>(base: usize, offset: usize, totalsize: usize) -> Option<&'a str> {
    if offset >= totalsize {
        return None;
    }
    // Find the null terminator by scanning bytes. We must not run past
    // `totalsize` — a corrupt blob with a missing terminator would
    // otherwise read into unmapped memory and fault.
    let mut end = offset;
    while end < totalsize {
        // SAFETY: `end` is within `[offset, totalsize)` which we
        // validated against the blob's readable range.
        let byte = unsafe { ptr::with_exposed_provenance::<u8>(base + end).read_volatile() };
        if byte == 0 {
            break;
        }
        end += 1;
    }
    if end >= totalsize {
        // No null terminator found within bounds — corrupt string block.
        return None;
    }
    // SAFETY: the bytes `[offset, end)` are within the blob's readable
    // range. We build a slice and then validate UTF-8; if the bytes are
    // not valid UTF-8 (they should be — the DTB spec requires it), we
    // return None. The lifetime `'a` ties this slice to the blob —
    // the caller must not outlive the `Fdtr`.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            ptr::with_exposed_provenance::<u8>(base + offset),
            end - offset,
        )
    };
    core::str::from_utf8(bytes).ok()
}

// -----------------------------------------------------------------------
// Node — a handle into the structure block
// -----------------------------------------------------------------------

/// A devicetree node: an offset into the structure block pointing at the
/// `FDT_BEGIN_NODE` token that opens it.
///
/// `Node` is a plain `Copy` struct — it carries no raw pointer, only an
/// integer offset. Safety comes from the `Fdtr` whose lifetime borrows
/// the blob: you can only get a `Node` from an `Fdtr`, and all property
/// reads go through the `Fdtr` that created it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Node {
    /// Byte offset of the `FDT_BEGIN_NODE` token, relative to the
    /// start of the DTB blob.
    offset: usize,
}

// -----------------------------------------------------------------------
// Fdtr — the parser
// -----------------------------------------------------------------------

/// A Flattened Devicetree Reader.
///
/// Wraps the physical address of a DTB after validating its header. All
/// tree walks and property reads borrow `&self`, so the borrow checker
/// ensures no `Node` or `&[u8]` outlives the blob.
///
/// The lifetime `'a` is the blob's: it is never written as an explicit
/// parameter (the struct holds an integer, not a reference), but it
/// threads through every method that returns borrowed data, tying
/// strings and property slices to the `Fdtr` that produced them.
pub struct Fdtr {
    /// Virtual address of the DTB blob (the higher-half alias of the
    /// physical address passed to `new`).
    base: usize,
    /// Validated header — cached so we never re-read the magic.
    header: Header,
}

impl Fdtr {
    /// Create an FDT reader from a physical address.
    ///
    /// Validates the magic number, reads the full 40-byte header through
    /// the higher-half alias, and checks that the structure and string
    /// block offsets are within `totalsize`. Returns `None` if any
    /// check fails — the caller (typically `kmain`) should treat a
    /// `None` as "no devicetree here" and fall back to hardcoded values.
    ///
    /// # Safety contract
    ///
    /// `dtb_pa` must point at a DTB that lives in mapped RAM for the
    /// kernel's lifetime. On QEMU's virt board the DTB sits at the start
    /// of RAM and the kernel maps all of RAM read-only (the DTB window)
    /// and read-write (the rest) in `mmu.rs` — so the contract is
    /// satisfied by construction for any `dtb_pa` within `[RAM_BASE,
    /// RAM_BASE + RAM_SIZE)`.
    pub fn new(dtb_pa: usize) -> Option<Self> {
        let base = phys_to_virt(dtb_pa);

        // Read the header word by word. We cannot use a struct load
        // because the fields are big-endian and the blob may not be
        // aligned to 40 bytes (it is aligned to 4, but a packed struct
        // would still need individual reads for correctness). Reading
        // each u32 volatilely through the higher-half alias matches
        // the pattern in `main.rs`'s `fdt_at`.

        // First word: the magic. Use a generous bound (the header is 40
        // bytes, so reading word 0 at offset 0 needs at least 4 bytes).
        let magic = unsafe { read_be_u32(base, 0, Header::SIZE)? };
        if magic != FDT_MAGIC {
            return None;
        }

        // Now that we know the magic, read the rest of the header with
        // the blob's own `totalsize` as the bounds ceiling. But we need
        // `totalsize` first, so read it with the header-size bound.
        let totalsize = unsafe { read_be_u32(base, 4, Header::SIZE)? } as usize;

        // Read the full header, now with the real bounds.
        let header = Header {
            magic,
            totalsize: totalsize as u32,
            off_dt_struct: unsafe { read_be_u32(base, 8, totalsize)? },
            off_dt_strings: unsafe { read_be_u32(base, 12, totalsize)? },
            off_mem_rsvmap: unsafe { read_be_u32(base, 16, totalsize)? },
            version: unsafe { read_be_u32(base, 20, totalsize)? },
            last_comp_version: unsafe { read_be_u32(base, 24, totalsize)? },
            boot_cpuid_phys: unsafe { read_be_u32(base, 28, totalsize)? },
            size_dt_strings: unsafe { read_be_u32(base, 32, totalsize)? },
            size_dt_struct: unsafe { read_be_u32(base, 36, totalsize)? },
        };

        // Sanity: structure and string blocks must be within the blob.
        // Use checked_add to prevent overflow: a crafted FDT could set
        // off_dt_struct and size_dt_struct so their sum wraps to a small
        // value, bypassing the bounds check.
        let struct_end = (header.off_dt_struct as usize)
            .checked_add(header.size_dt_struct as usize)?;
        let strings_end = (header.off_dt_strings as usize)
            .checked_add(header.size_dt_strings as usize)?;
        if struct_end > totalsize || strings_end > totalsize {
            return None;
        }
        // The structure block must start after the header.
        if (header.off_dt_struct as usize) < Header::SIZE {
            return None;
        }

        Some(Self { base, header })
    }

    /// The total size of the DTB blob in bytes.
    pub fn totalsize(&self) -> usize {
        self.header.totalsize as usize
    }

    /// The boot CPU's physical ID. Useful for SMP; unused today.
    pub fn boot_cpuid(&self) -> u32 {
        self.header.boot_cpuid_phys
    }

    // --- Memory reserve map -------------------------------------------

    /// Iterate the memory reserve map. Yields entries until the
    /// all-zeros terminator (or the end of the blob, whichever comes
    /// first). Each entry is 16 bytes: a big-endian u64 address
    /// followed by a big-endian u64 size.
    #[allow(dead_code)]
    pub fn reserves(&self) -> ReserveIter<'_> {
        ReserveIter {
            fdt: self,
            offset: self.header.off_mem_rsvmap as usize,
        }
    }

    // --- Structure block primitives -----------------------------------

    /// Read a big-endian u32 from the structure block at byte offset
    /// `off` (relative to the blob start). Bounds-checked against
    /// `totalsize`. Returns `None` if out of range.
    fn struct_u32(&self, off: usize) -> Option<u32> {
        unsafe { read_be_u32(self.base, off, self.header.totalsize as usize) }
    }

    /// Read a big-endian u64 from the structure block at byte offset
    /// `off`. Bounds-checked.
    fn struct_u64(&self, off: usize) -> Option<u64> {
        unsafe { read_be_u64(self.base, off, self.header.totalsize as usize) }
    }

    /// Read a null-terminated string from the structure block at byte
    /// offset `off`. Returns a `&str` borrowed from the blob.
    fn struct_cstr(&self, off: usize) -> Option<&str> {
        unsafe { read_cstr(self.base, off, self.header.totalsize as usize) }
    }

    /// Read a null-terminated string from the *string block* at byte
    /// offset `nameoff` (relative to the blob start, i.e. already
    /// including `off_dt_strings`). Property names live here.
    fn string_at(&self, nameoff: u32) -> Option<&str> {
        // Use checked_add to prevent overflow: a malicious nameoff could
        // make off_dt_strings + nameoff wrap to a small value.
        let off = (self.header.off_dt_strings as usize).checked_add(nameoff as usize)?;
        let strings_end =
            self.header.off_dt_strings as usize + self.header.size_dt_strings as usize;
        // Use the string block's own bounds for the string scan, not
        // the full totalsize — a property name should not run past the
        // string block into the next region.
        if off >= strings_end {
            return None;
        }
        unsafe { read_cstr(self.base, off, strings_end) }
    }

    /// Read `len` bytes of property data from the structure block at
    /// byte offset `off`. Returns a `&[u8]` borrowed from the blob.
    fn struct_bytes(&self, off: usize, len: usize) -> Option<&[u8]> {
        let ts = self.header.totalsize as usize;
        // Use checked_add to prevent overflow: a crafted FDT property
        // with a huge `len` could make `off + len` wrap to a small value,
        // passing the check and yielding an out-of-bounds slice.
        let end = off.checked_add(len)?;
        if end > ts || off > ts {
            return None;
        }
        // SAFETY: `base` is a valid readable VA for `totalsize` bytes
        // (validated in `new`). `off + len` is within bounds. The
        // lifetime ties this slice to `&self` — the borrow checker
        // ensures it does not outlive the `Fdtr`.
        Some(unsafe {
            core::slice::from_raw_parts(ptr::with_exposed_provenance::<u8>(self.base + off), len)
        })
    }

    /// The end of the structure block (exclusive), as a byte offset
    /// from the blob start.
    fn struct_end(&self) -> usize {
        self.header.off_dt_struct as usize + self.header.size_dt_struct as usize
    }

    // --- Tree navigation ----------------------------------------------

    /// Return the root node — the first `FDT_BEGIN_NODE` in the
    /// structure block. Returns `None` if the structure block is empty
    /// or does not start with a `BEGIN_NODE` token.
    pub fn root(&self) -> Option<Node> {
        let off = self.header.off_dt_struct as usize;
        let token = self.struct_u32(off)?;
        if token != FDT_BEGIN_NODE {
            return None;
        }
        Some(Node { offset: off })
    }

    /// Read the name of a node — the null-terminated string immediately
    /// after the `FDT_BEGIN_NODE` token, *before* any properties or
    /// children. The name may include a unit address (`memory@40000000`);
    /// this method returns the full name including the `@addr` part.
    fn node_name_raw(&self, node: &Node) -> Option<&str> {
        // Skip the 4-byte BEGIN_NODE token, then read the C string.
        let name_off = node.offset + 4;
        self.struct_cstr(name_off)
    }

    /// The node name *without* the unit address. For `memory@40000000`
    /// this returns `"memory"`; for a bare `aliases` node it returns
    /// `"aliases"`. The unit address is the part after `@`; nodes that
    /// have no `@` return their full name.
    #[allow(dead_code)]
    pub fn name(&self, node: &Node) -> &str {
        let raw = self.node_name_raw(node).unwrap_or("");
        // Split at '@' — the part before is the bare node name.
        match raw.find('@') {
            Some(i) => &raw[..i],
            None => raw,
        }
    }

    /// The full node name including the unit address, e.g.
    /// `"memory@40000000"`. Useful for matching against DTB paths in
    /// [`find`](Self::find).
    pub fn full_name(&self, node: &Node) -> &str {
        self.node_name_raw(node).unwrap_or("")
    }

    /// The `compatible` property as a string. The DTB stores
    /// `compatible` as a null-separated list (a node can be compatible
    /// with multiple things); we return the first entry, which is the
    /// most specific. Returns an empty string if the node has no
    /// `compatible` property.
    pub fn compatible(&self, node: &Node) -> &str {
        if let Some(data) = self.prop(node, "compatible") {
            // The compatible property is a list of null-terminated
            // strings. Return just the first one.
            if let Some(end) = data.iter().position(|&b| b == 0) {
                core::str::from_utf8(&data[..end]).unwrap_or("")
            } else {
                // No null terminator — treat the whole thing as one string.
                core::str::from_utf8(data).unwrap_or("")
            }
        } else {
            ""
        }
    }

    // --- Property access ---------------------------------------------

    /// Find a property named `name` on `node`. Returns the property's
    /// raw bytes as a `&[u8]` borrowed from the blob, or `None` if the
    /// node has no such property.
    ///
    /// Walks the node's property list (the tokens between the node name
    /// and the first child `BEGIN_NODE` or the matching `END_NODE`).
    /// Each property is: `FDT_PROP` (u32) + `len` (u32) + `nameoff`
    /// (u32) + `len` bytes of data (padded to 4).
    pub fn prop(&self, node: &Node, name: &str) -> Option<&[u8]> {
        let name_off = node.offset + 4;
        let node_name = self.struct_cstr(name_off)?;
        // Skip past the node name + its null terminator, aligned to 4.
        let name_len = node_name.len() + 1; // include the null
        let name_padded = (name_len + 3) & !3;
        let mut off = name_off + name_padded;

        let struct_end = self.struct_end();

        loop {
            // Use checked_add to prevent overflow in the walking offset.
            let off_end = off.checked_add(4)?;
            if off_end > struct_end {
                return None;
            }
            let token = self.struct_u32(off)?;
            off = off.checked_add(4)?;

            match token {
                FDT_PROP => {
                    // 12-byte header after the token: len (u32) + nameoff (u32).
                    // (The token itself is the first u32; the 12-byte count
                    // in the task description includes the token.)
                    let len = self.struct_u32(off)? as usize;
                    let nameoff = self.struct_u32(off.checked_add(4)?)?;
                    off = off.checked_add(8)?; // skip len + nameoff

                    // Look up the property name in the string block.
                    let pname = self.string_at(nameoff)?;
                    let data = self.struct_bytes(off, len)?;

                    if pname == name {
                        return Some(data);
                    }

                    // Advance past the data, padded to 4 bytes.
                    let data_padded = (len.checked_add(3)? ) & !3;
                    off = off.checked_add(data_padded)?;
                }
                FDT_NOP => {
                    // No-op token, just skip.
                    continue;
                }
                FDT_BEGIN_NODE | FDT_END_NODE => {
                    // We have reached the first child or the end of
                    // this node — no more properties to check.
                    return None;
                }
                _ => {
                    // Unknown token — corrupt structure block.
                    return None;
                }
            }
        }
    }

    /// A string property: interpret the raw bytes as a null-terminated
    /// UTF-8 string and return the part before the null. Many DTB
    /// string properties are single-valued (e.g. `device_type`); the
    /// null terminator is the spec's way of saying "string ends here."
    #[allow(dead_code)]
    pub fn prop_str(&self, node: &Node, name: &str) -> Option<&str> {
        let data = self.prop(node, name)?;
        // Find the null terminator; everything before it is the string.
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        core::str::from_utf8(&data[..end]).ok()
    }

    /// A u32 property: the raw bytes are a single big-endian u32.
    /// Returns `None` if the property is missing or not exactly 4 bytes.
    #[allow(dead_code)]
    pub fn prop_u32(&self, node: &Node, name: &str) -> Option<u32> {
        let data = self.prop(node, name)?;
        if data.len() < 4 {
            return None;
        }
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&data[..4]);
        Some(u32::from_be_bytes(buf))
    }

    /// A u64 property: the raw bytes are a single big-endian u64.
    /// Returns `None` if the property is missing or not exactly 8 bytes.
    #[allow(dead_code)]
    pub fn prop_u64(&self, node: &Node, name: &str) -> Option<u64> {
        let data = self.prop(node, name)?;
        if data.len() < 8 {
            return None;
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&data[..8]);
        Some(u64::from_be_bytes(buf))
    }

    /// Iterate the u32 cells of a property. Devicetree properties like
    /// `reg` and `interrupts` are arrays of big-endian u32 "cells."
    /// This yields each cell in order; trailing bytes that do not fill
    /// a complete u32 are ignored (well-formed properties are always
    /// 4-byte multiples).
    #[allow(dead_code)]
    pub fn prop_cells(&self, node: &Node, name: &str) -> CellIter<'_> {
        let data = self.prop(node, name).unwrap_or(&[]);
        CellIter { data, pos: 0 }
    }

    // --- Tree walking -------------------------------------------------

    /// Iterate the direct children of `node`. Each child is a
    /// `FDT_BEGIN_NODE` ... `FDT_END_NODE` pair nested inside the
    /// parent. Properties of the parent are skipped; only child nodes
    /// are yielded.
    #[allow(dead_code)]
    pub fn children(&self, node: &Node) -> NodeIter<'_> {
        NodeIter {
            fdt: self,
            offset: self.first_child_offset(node).unwrap_or(0),
            depth: 0,
        }
    }

    /// Find the byte offset of the first child of `node` (the first
    /// `FDT_BEGIN_NODE` after the node's name and property list), or
    /// `None` if the node has no children.
    ///
    /// Walks past the node name and all `FDT_PROP` / `FDT_NOP` tokens
    /// until a `FDT_BEGIN_NODE` (first child) or `FDT_END_NODE` (no
    /// children) is encountered.
    fn first_child_offset(&self, node: &Node) -> Option<usize> {
        let name_off = node.offset + 4;
        let name = self.struct_cstr(name_off)?;
        let name_padded = (name.len() + 1 + 3) & !3;
        let mut off = name_off.checked_add(name_padded)?;

        let struct_end = self.struct_end();

        loop {
            let off_end = off.checked_add(4)?;
            if off_end > struct_end {
                return None;
            }
            let token = self.struct_u32(off)?;
            match token {
                FDT_PROP => {
                    // Skip the property header + data.
                    let len = self.struct_u32(off.checked_add(4)?)? as usize;
                    let data_padded = (len.checked_add(3)?) & !3;
                    // token (4) + header (8) + data
                    off = off.checked_add(4)?.checked_add(8)?.checked_add(data_padded)?;
                }
                FDT_NOP => {
                    off = off.checked_add(4)?;
                }
                FDT_BEGIN_NODE => {
                    return Some(off);
                }
                FDT_END_NODE => {
                    // No children — this node is a leaf.
                    return None;
                }
                _ => return None, // corrupt
            }
        }
    }

    /// Find a node by path, e.g. `"/memory"` or `"/intc@8000000"`.
    ///
    /// The path is `/`-separated; each component matches a child node's
    /// name. If the component includes a `@unit` address (e.g.
    /// `memory@40000000`), the match is exact against the node's full
    /// name. If it does not (e.g. `memory`), it matches any node whose
    /// name-before-`@` equals the component — so `/memory` finds
    /// `memory@40000000`, and `/intc` finds `intc@8000000`. This is
    /// how devicetree paths work in practice: the unit address is
    /// optional in a lookup, and the bare name is the common case.
    ///
    /// A simple linear search — devicetrees are small (QEMU's virt DTB
    /// is a few KiB), so a hash map would be pure overhead.
    pub fn find(&self, path: &str) -> Option<Node> {
        let mut node = self.root()?;
        // Split the path into components. A leading '/' produces an
        // empty first element; skip it.
        for component in path.split('/').filter(|s| !s.is_empty()) {
            // Search the children of `node` for one whose name matches.
            let mut found = None;
            for child in self.children(&node) {
                let full = self.full_name(&child);
                // Exact match (component has @address, or node has no @).
                if full == component {
                    found = Some(child);
                    break;
                }
                // Prefix match: component without @ matches node with @.
                // "memory" matches "memory@40000000".
                if !component.contains('@') {
                    if let Some(at_pos) = full.find('@') {
                        if &full[..at_pos] == component {
                            found = Some(child);
                            break;
                        }
                    }
                }
            }
            node = found?;
        }
        Some(node)
    }

    /// Find the first node whose `compatible` property contains `substr`.
    ///
    /// Unlike [`find`](Self::find) (which walks by path), this is a
    /// tree-wide search: it visits every node recursively and checks each
    /// one's `compatible` string for a substring match. This is how
    /// `board/apple.rs` discovers the AIC node — the devicetree does not
    /// guarantee a fixed path for interrupt controllers, but it *does*
    /// guarantee `compatible = "apple,aic"`. The FDT is small (a few KiB),
    /// so a full traversal is cheaper than building a lookup table.
    ///
    /// Returns the first matching node in pre-order, or `None` if no node
    /// has a matching `compatible`.
    pub fn find_by_compatible(&self, substr: &str) -> Option<Node> {
        self.find_by_compatible_in(&self.root()?, substr)
    }

    /// Recursive helper for [`find_by_compatible`](Self::find_by_compatible):
    /// search the subtree rooted at `node`, returning the first descendant
    /// (including `node` itself) whose `compatible` contains `substr`.
    fn find_by_compatible_in(&self, node: &Node, substr: &str) -> Option<Node> {
        // Check this node first (pre-order).
        if self.compatible(node).contains(substr) {
            return Some(*node);
        }
        // Then recurse into children.
        for child in self.children(node) {
            if let Some(found) = self.find_by_compatible_in(&child, substr) {
                return Some(found);
            }
        }
        None
    }

    // --- Full tree dump (for the `tree` monitor command) ----------------

    /// Print the entire devicetree: every node, every property, every
    /// value (heuristically formatted). A teaching tool — the `tree`
    /// monitor command uses this to show the whole shape of the world
    /// the bootloader described, in the kernel's own rendering.
    ///
    /// Property values are guessed: strings (printable ASCII ending in
    /// null) are printed quoted; u32 arrays (len % 4 == 0) as `<...>`;
    /// anything else as raw hex. The FDT carries no type information,
    /// so this is necessarily a guess — but it's a good one.
    pub fn dump(&self) {
        let root = match self.root() {
            Some(r) => r,
            None => {
                println!("(empty tree)");
                return;
            }
        };
        self.dump_node(&root, 0);
    }

    /// Print a node and its subtree at the given indentation depth.
    fn dump_node(&self, node: &Node, depth: usize) {
        let indent = indent_str(depth);
        let name = self.full_name(node);
        let label = if name.is_empty() { "/" } else { name };
        println!("{indent}{label}");

        // Print this node's properties.
        let name_off = node.offset + 4;
        let node_name = self.struct_cstr(name_off).unwrap_or("");
        let name_padded = (node_name.len() + 1 + 3) & !3;
        let mut off = name_off + name_padded;
        let struct_end = self.struct_end();

        // Collect properties first, then find children.
        while off + 4 <= struct_end {
            let token = match self.struct_u32(off) {
                Some(t) => t,
                None => break,
            };
            if token != FDT_PROP {
                break;
            }
            off += 4;
            let len = match self.struct_u32(off) {
                Some(l) => l as usize,
                None => break,
            };
            let nameoff = match self.struct_u32(off + 4) {
                Some(n) => n,
                None => break,
            };
            off += 8;
            let pname = self.string_at(nameoff).unwrap_or("?");
            let data = match self.struct_bytes(off, len) {
                Some(d) => d,
                None => break,
            };
            // Format the property value.
            let prop_indent = indent_str(depth + 1);
            print!("{prop_indent}{pname} = ");
            self.print_prop_value(data);
            off += (len + 3) & !3;
        }

        // Recurse into children.
        for child in self.children(node) {
            self.dump_node(&child, depth + 1);
        }
    }

    /// Heuristically format a property value for `dump`. The FDT has no
    /// type tags, so we guess based on the byte pattern.
    fn print_prop_value(&self, data: &[u8]) {
        if data.is_empty() {
            println!("<>");
            return;
        }
        // Try string: ends with null, all printable ASCII.
        if data.last() == Some(&0) {
            if let Ok(s) = core::str::from_utf8(&data[..data.len() - 1]) {
                if !s.is_empty() && s.bytes().all(|b| b >= 0x20 && b < 0x7f) {
                    // Check for multi-string (compatible-style).
                    let nulls = data.iter().filter(|&&b| b == 0).count();
                    if nulls > 1 {
                        let mut first = true;
                        for part in s.split('\0') {
                            if !part.is_empty() {
                                if !first {
                                    print!(", ");
                                }
                                print!("\"{part}\"");
                                first = false;
                            }
                        }
                        println!();
                    } else {
                        println!("\"{s}\"");
                    }
                    return;
                }
            }
        }
        // Try u32 array.
        if data.len() % 4 == 0 {
            let n = data.len() / 4;
            print!("<");
            for i in 0..n {
                if i > 0 {
                    print!(" ");
                }
                let mut buf = [0u8; 4];
                buf.copy_from_slice(&data[i * 4..i * 4 + 4]);
                print!("{:#x}", u32::from_be_bytes(buf));
            }
            println!(">");
            return;
        }
        // Raw hex.
        print!("<");
        for (i, &b) in data.iter().enumerate() {
            if i > 0 {
                print!(" ");
            }
            print!("{:02x}", b);
        }
        println!(">");
    }
}

// -----------------------------------------------------------------------
// Iterators
// -----------------------------------------------------------------------

/// Iterator over the memory reserve map entries.
#[allow(dead_code)]
pub struct ReserveIter<'a> {
    fdt: &'a Fdtr,
    offset: usize,
}

/// A reserve-map entry that signals "end of map" — the all-zeros pair.
#[allow(dead_code)]
const RESERVE_END_ADDR: u64 = 0;
#[allow(dead_code)]
const RESERVE_END_SIZE: u64 = 0;

impl<'a> Iterator for ReserveIter<'a> {
    type Item = ReserveEntry;

    fn next(&mut self) -> Option<Self::Item> {
        let address = self.fdt.struct_u64(self.offset)?;
        let size = self.fdt.struct_u64(self.offset + 8)?;

        if address == RESERVE_END_ADDR && size == RESERVE_END_SIZE {
            return None;
        }

        self.offset += 16;
        Some(ReserveEntry { address, size })
    }
}

/// Iterator over the u32 cells of a property. Each `next()` yields one
/// big-endian u32 decoded from the property's raw bytes.
#[allow(dead_code)]
pub struct CellIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for CellIter<'a> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.pos + 4 > self.data.len() {
            return None;
        }
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.data[self.pos..self.pos + 4]);
        self.pos += 4;
        Some(u32::from_be_bytes(buf))
    }
}

/// Iterator over the child nodes of a [`Node`].
///
/// Walks the structure block from the first child, tracking nesting
/// depth. The iterator yields only *direct* children of the starting
/// node. Grandchildren (deeper nesting) are skipped: each `BEGIN_NODE`
/// past the child level increments the depth counter, and the matching
/// `END_NODE` decrements it. When depth returns to zero we are back at
/// the child level and the next `BEGIN_NODE` is the next child.
///
/// The iterator stops when it sees `FDT_END_NODE` at depth zero — that
/// is the parent's own closing token, meaning no more children remain.
#[allow(dead_code)]
pub struct NodeIter<'a> {
    fdt: &'a Fdtr,
    /// Current byte offset in the structure block. Zero means
    /// "exhausted" (set when the iterator runs off the end or hits the
    /// parent's `FDT_END_NODE`).
    offset: usize,
    /// Current nesting depth relative to the child level.
    /// - 0: we are at the child level — the next `BEGIN_NODE` is a
    ///   direct child and should be yielded.
    /// - >0: we are inside a yielded child's subtree (its properties
    ///   and grandchildren) and must skip until depth returns to 0.
    depth: usize,
}

impl<'a> Iterator for NodeIter<'a> {
    type Item = Node;

    fn next(&mut self) -> Option<Node> {
        let struct_end = self.fdt.struct_end();

        loop {
            if self.offset == 0 || self.offset + 4 > struct_end {
                return None;
            }

            let token = self.fdt.struct_u32(self.offset)?;
            self.offset += 4;

            match token {
                FDT_BEGIN_NODE => {
                    // Skip past the node name (null-terminated, 4-aligned).
                    let name = self.fdt.struct_cstr(self.offset)?;
                    let name_padded = (name.len() + 1 + 3) & !3;
                    self.offset = self.offset.checked_add(name_padded)?;

                    if self.depth == 0 {
                        // Direct child — yield it and descend into its
                        // subtree so the next call skips over the
                        // child's contents.
                        let node = Node {
                            offset: self.offset - 4 - name_padded,
                        };
                        self.depth = 1;
                        return Some(node);
                    } else {
                        // Grandchild — track the deeper nesting.
                        self.depth += 1;
                    }
                }
                FDT_END_NODE => {
                    if self.depth == 0 {
                        // The parent's own END_NODE — no more children.
                        self.offset = 0;
                        return None;
                    }
                    self.depth -= 1;
                    // If depth is now 0, we just closed a child's
                    // subtree and the loop continues looking for the
                    // next sibling.
                }
                FDT_PROP => {
                    // Skip the property: len (u32) + nameoff (u32) + data.
                    let len = self.fdt.struct_u32(self.offset)? as usize;
                    let data_padded = (len + 3) & !3;
                    self.offset += 8 + data_padded; // header + data
                }
                FDT_NOP => {
                    // No-op, already advanced past the token.
                }
                FDT_END => {
                    self.offset = 0;
                    return None;
                }
                _ => {
                    // Unknown token — corrupt structure block.
                    self.offset = 0;
                    return None;
                }
            }
        }
    }
}
