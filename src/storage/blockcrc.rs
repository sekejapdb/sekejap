//! # Block checksums — making damage locatable in a mapped file
//!
//! Law 5 of the contract says no corruption may be unrecoverable: damage must be
//! *locatable*, must not *propagate*, and nothing read from a damaged region may
//! be believed. This module is the mechanism the mapped formats use to satisfy
//! that.
//!
//! ## The trailer
//!
//! A finished buffer is divided into fixed 4 KiB blocks, one CRC-32 per block,
//! and the table is **appended**:
//!
//! ```text
//! [ ... existing payload, byte for byte unchanged ... ]
//! [ crc32 per block, u32 each ]
//! [ nblocks u32 ]
//! [ "SKCRC\0\0\0" ]
//! ```
//!
//! Appending matters. These formats address their contents by arithmetic —
//! `record(k) = data_start + k*24` — and some store absolute byte offsets, so
//! growing a header would move every one of them. A trailer moves nothing, which
//! is why an existing store can gain checksums without its layout changing.
//!
//! The trailer is found by reading backwards from the end, so a damaged payload
//! cannot hide its own checksums.
//!
//! ## Lazy verification
//!
//! These files are read on the hop — the database's primary operation. Verifying
//! the whole file at open would cost O(store) latency, trading Laws 1 and 2 away
//! to satisfy Law 5. Verifying on every access would tax the hop forever. So a
//! block is checksummed the **first time it is touched** and the answer is
//! remembered: first touch pays one CRC over 4 KiB, every later touch pays one
//! bit test.
//!
//! **Sacrifice:** resident state proportional to the store, at two bits per
//! 4 KiB — 3.2 MB for a 50 GB store, 0.006%. Law 1 says no RAM proportional to
//! the database, and this is proportional, with a constant of 1/16384. It was
//! taken knowingly over the two alternatives above.
//!
//! **Blast radius:** one corrupt byte costs one 4 KiB block of one file.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Header flag bit meaning "this file carries a checksum trailer".
pub(crate) const FLAG_CHECKSUMMED: u32 = 1;
/// Checksum granularity: small enough that one bad byte costs little, large
/// enough that the table is 0.1% of the file.
pub(crate) const CHECK_BLOCK: usize = 4096;
/// Sits at the very end, so the trailer is findable without trusting the payload.
pub(crate) const TRAILER_MAGIC: [u8; 8] = *b"SKCRC\0\0\0";
/// `[nblocks u32][TRAILER_MAGIC 8]`, after the table itself.
pub(crate) const TRAILER_TAIL: usize = 4 + 8;

fn rd_u32(b: &[u8], o: usize) -> u32 {
    match b.get(o..o + 4) {
        Some(x) => u32::from_le_bytes(x.try_into().unwrap()),
        None => 0,
    }
}

/// Append a checksum trailer and set the flag at `flags_off`.
///
/// The flag goes in **first**, because it lives inside block 0. Set afterwards it
/// would change a byte the checksum had already covered, so block 0 would fail
/// its own CRC and every valid read would be refused. That bug was written once
/// and caught only by a test asserting undamaged data still reads.
pub(crate) fn append(buf: &mut Vec<u8>, flags_off: usize) {
    if let Some(slot) = buf.get_mut(flags_off..flags_off + 4) {
        let flags = u32::from_le_bytes(slot.try_into().unwrap()) | FLAG_CHECKSUMMED;
        slot.copy_from_slice(&flags.to_le_bytes());
    }
    let payload_len = buf.len();
    let nblocks = payload_len.div_ceil(CHECK_BLOCK);
    let mut table: Vec<u8> = Vec::with_capacity(nblocks * 4);
    for i in 0..nblocks {
        let start = i * CHECK_BLOCK;
        let end = (start + CHECK_BLOCK).min(payload_len);
        let mut h = crc32fast::Hasher::new();
        h.update(&buf[start..end]);
        table.extend_from_slice(&h.finalize().to_le_bytes());
    }
    buf.extend_from_slice(&table);
    buf.extend_from_slice(&(nblocks as u32).to_le_bytes());
    buf.extend_from_slice(&TRAILER_MAGIC);
}

/// Per-block verification state for one file. See the module docs.
pub(crate) struct BlockChecks {
    pub(crate) payload_len: usize,
    table_off: usize,
    nblocks: usize,
    /// Examined, and failed. Two bitsets rather than one, so a block that failed
    /// is not re-checksummed on every subsequent read.
    checked: Vec<AtomicU64>,
    bad: Vec<AtomicU64>,
}

impl BlockChecks {
    /// Read the trailer backwards from the end. `None` when the file has none.
    pub(crate) fn parse(all: &[u8], flags: u32) -> Option<BlockChecks> {
        if flags & FLAG_CHECKSUMMED == 0 || all.len() < TRAILER_TAIL {
            return None;
        }
        let tail = all.len() - TRAILER_TAIL;
        if all[tail + 4..] != TRAILER_MAGIC {
            return None;
        }
        let nblocks = rd_u32(all, tail) as usize;
        let table_off = tail.checked_sub(nblocks.checked_mul(4)?)?;
        // The table must describe exactly the payload in front of it, or it is
        // not this file's table.
        if table_off.div_ceil(CHECK_BLOCK) != nblocks {
            return None;
        }
        let words = nblocks.div_ceil(64);
        Some(BlockChecks {
            payload_len: table_off,
            table_off,
            nblocks,
            checked: (0..words).map(|_| AtomicU64::new(0)).collect(),
            bad: (0..words).map(|_| AtomicU64::new(0)).collect(),
        })
    }

    fn bit(set: &[AtomicU64], i: usize) -> bool {
        set.get(i / 64).is_some_and(|w| w.load(Relaxed) & (1u64 << (i % 64)) != 0)
    }
    fn set_bit(set: &[AtomicU64], i: usize) {
        if let Some(w) = set.get(i / 64) {
            w.fetch_or(1u64 << (i % 64), Relaxed);
        }
    }

    /// True when every block covering `[off, off+len)` passes.
    pub(crate) fn ok(&self, all: &[u8], off: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let end = match off.checked_add(len) {
            Some(e) if e <= self.payload_len => e,
            _ => return false,
        };
        for b in (off / CHECK_BLOCK)..=((end - 1) / CHECK_BLOCK) {
            if b >= self.nblocks {
                return false;
            }
            if Self::bit(&self.checked, b) {
                if Self::bit(&self.bad, b) {
                    return false;
                }
                continue;
            }
            let start = b * CHECK_BLOCK;
            let stop = (start + CHECK_BLOCK).min(self.payload_len);
            let mut h = crc32fast::Hasher::new();
            h.update(&all[start..stop]);
            let good = h.finalize() == rd_u32(all, self.table_off + b * 4);
            if !good {
                Self::set_bit(&self.bad, b);
            }
            Self::set_bit(&self.checked, b);
            if !good {
                return false;
            }
        }
        true
    }
}

/// Strip the trailer and verify the whole file, for small tables parsed once at
/// open rather than read on a hot path.
///
/// `Some(payload)` when there is no trailer (old format, unchanged behaviour) or
/// there is one and it passes. `None` means it failed — the caller's cue to
/// report what was lost rather than parse bytes it cannot vouch for.
pub(crate) fn verified_payload(b: &[u8], flags_off: usize) -> Option<&[u8]> {
    let flags = rd_u32(b, flags_off);
    match BlockChecks::parse(b, flags) {
        None => Some(b),
        Some(c) => {
            let len = c.payload_len;
            if c.ok(b, 0, len) { b.get(..len) } else { None }
        }
    }
}
