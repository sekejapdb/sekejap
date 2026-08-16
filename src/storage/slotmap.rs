//! `slots.bin` — the indirection that lets a record move without every reference
//! to it having to move too.
//!
//! # Why this file exists
//!
//! A graph traversal is: neighbour's identity → that neighbour's record location.
//! Sekejap made identity *be* location — a node's id multiplied by the record size
//! is the byte offset — which makes a hop pure arithmetic, and is why hops are
//! fast. It also means **a record can never move**. Reorganising storage therefore
//! requires rewriting everything and renumbering, which is why compaction costs
//! `O(store)` for `O(change)` input, and why mutation has to happen in place,
//! which is where data-loss bugs live.
//!
//! Putting one level of indirection between them — an id names a *slot*, and this
//! table says where that slot's record currently is — buys the ability to move a
//! record without touching anything that points at it. That is what makes
//! append-only writes and copy-on-write possible at all.
//!
//! # Layout
//!
//! Header, then one `u64` per slot:
//!
//! ```text
//!   bits 48..64   segment ordinal (which segment holds the record)
//!   bits  0..48   local id within that segment
//! ```
//!
//! `u64::MAX` marks a free slot. 48 bits of local id is 281 trillion records per
//! segment, and 16 bits is 65 535 segments; both are far past anything reachable.
//!
//! # Cost
//!
//! Eight bytes per live slot, and it is the one `O(N)` structure the design
//! accepts: a million records is 8 MB, three billion is 24 GB. It is memory-mapped,
//! so what is resident is the working set rather than the whole array — but it is
//! not free, and it is the single structure whose corruption loses the store.

use super::mmap::MmapView;
use std::sync::Arc;

const MAGIC: [u8; 8] = *b"SKSLOT\0\0";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 16;

/// A slot with no record — either never allocated, or freed by a delete.
///
/// This costs exactly one representable location: segment `u16::MAX` holding local
/// id `2^48 - 1` would encode to the same bits, so that one pair is reserved. The
/// alternative — a separate liveness bitmap — costs a bit per slot forever to save
/// a location nothing can reach. `MAX_LOCAL_ID` is the honest upper bound.
pub(crate) const FREE: u64 = u64::MAX;

/// Largest local id that can be encoded. One below `2^48` so the reserved pair
/// above cannot be produced by accident.
pub(crate) const MAX_LOCAL_ID: u64 = (1u64 << 48) - 2;

#[inline]
pub(crate) fn pack(segment: u16, local_id: u64) -> u64 {
    debug_assert!(local_id <= MAX_LOCAL_ID, "local id exceeds what a slot entry can hold");
    let packed = ((segment as u64) << 48) | (local_id & 0x0000_FFFF_FFFF_FFFF);
    debug_assert_ne!(packed, FREE, "a real location must never encode as the free marker");
    packed
}

#[inline]
pub(crate) fn unpack(entry: u64) -> Option<(u16, u64)> {
    if entry == FREE {
        return None;
    }
    Some(((entry >> 48) as u16, entry & 0x0000_FFFF_FFFF_FFFF))
}

/// Serialise a slot table. `entries[slot]` is the packed location of that slot.
pub(crate) fn write(entries: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + 8 + entries.len() * 8);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved, keeps the header 16 B
    out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for e in entries {
        out.extend_from_slice(&e.to_le_bytes());
    }
    out
}

/// The slot table, served from the mmap.
pub(crate) struct MappedSlots {
    view: Arc<MmapView>,
    off: usize,
    count: usize,
}

impl MappedSlots {
    pub(crate) fn open(path: &std::path::Path) -> std::io::Result<Option<Self>> {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len() as usize;
        if len < HEADER_LEN + 8 {
            return Ok(None);
        }
        let Some(view) = MmapView::try_new(&file, len) else { return Ok(None) };
        let view = Arc::new(view);
        let Some(hdr) = view.slice(0, HEADER_LEN + 8) else { return Ok(None) };
        if hdr[0..8] != MAGIC {
            return Ok(None);
        }
        let ver = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
        if ver > VERSION {
            return Ok(None); // written by a newer sekejap — rebuild rather than guess
        }
        let count = u64::from_le_bytes(hdr[16..24].try_into().unwrap()) as usize;
        if len < HEADER_LEN + 8 + count * 8 {
            return Ok(None); // truncated — caller rebuilds
        }
        Ok(Some(Self { view, off: HEADER_LEN + 8, count }))
    }

    pub(crate) fn len(&self) -> usize {
        self.count
    }

    /// Where slot `slot`'s record lives, or `None` if the slot is free.
    #[inline]
    pub(crate) fn locate(&self, slot: u64) -> Option<(u16, u64)> {
        let k = slot as usize;
        if k >= self.count {
            return None;
        }
        let b = self.view.slice(self.off + k * 8, 8)?;
        unpack(u64::from_le_bytes(b.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_round_trips_and_marks_free_slots() {
        assert_eq!(unpack(pack(0, 0)), Some((0, 0)));
        assert_eq!(unpack(pack(3, 123_456)), Some((3, 123_456)));
        // The largest encodable pair. One below 2^48 — the pair above it collides
        // with the free marker and is reserved, which this pins.
        assert_eq!(unpack(pack(u16::MAX, MAX_LOCAL_ID)), Some((u16::MAX, MAX_LOCAL_ID)));
        assert_ne!(pack(u16::MAX, MAX_LOCAL_ID), FREE);
        assert_eq!(unpack(FREE), None, "a free slot must not resolve to a location");
    }

    #[test]
    fn a_written_table_reads_back_identically() {
        let entries: Vec<u64> = (0..1000u64).map(|i| pack(0, i)).collect();
        let bytes = write(&entries);

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("slots.bin");
        std::fs::write(&path, &bytes).unwrap();

        let m = MappedSlots::open(&path).unwrap().expect("table should open");
        assert_eq!(m.len(), 1000);
        for i in 0..1000u64 {
            assert_eq!(m.locate(i), Some((0, i)), "slot {i}");
        }
        assert_eq!(m.locate(1000), None, "past the end is not a location");
    }

    #[test]
    fn free_slots_and_a_missing_file_are_not_errors() {
        let entries = vec![pack(0, 0), FREE, pack(1, 7)];
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("slots.bin");
        std::fs::write(&path, write(&entries)).unwrap();

        let m = MappedSlots::open(&path).unwrap().unwrap();
        assert_eq!(m.locate(0), Some((0, 0)));
        assert_eq!(m.locate(1), None, "a freed slot resolves to nothing");
        assert_eq!(m.locate(2), Some((1, 7)), "a slot in a later segment");

        assert!(MappedSlots::open(&dir.path().join("absent.bin")).unwrap().is_none());
    }

    #[test]
    fn a_truncated_table_is_refused_rather_than_half_read() {
        let entries: Vec<u64> = (0..100u64).map(|i| pack(0, i)).collect();
        let mut bytes = write(&entries);
        bytes.truncate(bytes.len() - 40); // lose the last five entries

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("slots.bin");
        std::fs::write(&path, &bytes).unwrap();

        assert!(
            MappedSlots::open(&path).unwrap().is_none(),
            "a short table must be refused so the caller rebuilds, not served with \
             whatever bytes happen to follow",
        );
    }
}
