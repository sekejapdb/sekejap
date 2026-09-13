//! One page format for the whole file.
//!
//! Layout, all little-endian:
//!   0   magic      u32   0x53454B32
//!   4   version    u16
//!   6   kind       u16
//!   8   tree_id    u16
//!   10  nentries   u16
//!   12  page_no    u32   its own number — proves this is the page asked for
//!   16  free_ptr   u16   lowest payload byte in use; payloads grow downward
//!   18  reserved   u16
//!   20  next_leaf  u32   LEAF: right sibling. INTERIOR: leftmost child
//!                        (child0). 0 = none.
//!   24  lsn        u64   RESERVED. Always 0 in Phase 1 — every production
//!                        call site passes `finalise(0)`. The field exists so
//!                        the format need not change when it is wired up, and
//!                        for no other reason. Two hazards attach to it, both
//!                        real: (1) nothing may compare a page's lsn against a
//!                        WAL record's to skip replay — log sequence numbers
//!                        restart after a rotation unless the superblock's
//!                        floor is applied (see Task 4); (2) recovery may not
//!                        use it to choose between two leaves claiming the
//!                        same key — a bulk-packed page and an insert-built
//!                        page both carry 0. A field documented as meaning
//!                        something it does not is the same defect as a
//!                        durability label that does not correspond to
//!                        behaviour.
//!   32  reserved2  u32
//!   36  crc32c     u32   over [0..36] ++ [40..PAGE_SIZE]
//!   40  slot directory: (u16 offset, u16 len) per entry, growing forward
//!       ... free space ...
//!       payloads, growing backward from PAGE_SIZE
//!
//! SACRIFICE (Law 4): 8 bytes of every page are identity and checksum, and every
//! read pays a CRC over 4 KiB. Bought: no damaged page is ever decoded and served,
//! and no page can be silently substituted for another.

use crate::{Error, Result};

pub const PAGE_SIZE: usize = 4096;
pub const HEADER_LEN: usize = 40;
const MAGIC: u32 = 0x5345_4B32;
const VERSION: u16 = 1;
const SLOT_LEN: usize = 4;

/// The largest a leaf record (`BTree`'s key+value encoding, `SLOT_LEN` bytes
/// of directory overhead included) can be and still fit an empty leaf.
/// `BTree::insert` enforces exactly this bound; it is public so a caller
/// that wants to reject an oversized record BEFORE doing anything
/// consequential with it -- `Store::put` logging it to the WAL, say -- can
/// check against the identical number instead of a second copy that could
/// drift (Task 17 re-review, R1: a record the WAL logged and the tree then
/// refused is exactly how a legitimate frame ended up sitting in the log
/// with nothing there to apply it, which is not a case any reader should
/// have to characterise after the fact).
pub const MAX_RECORD_LEN: usize = PAGE_SIZE - HEADER_LEN - SLOT_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum PageKind { Free = 0, Meta = 1, Leaf = 2, Interior = 3, Overflow = 4 }

impl PageKind {
    fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            0 => PageKind::Free, 1 => PageKind::Meta, 2 => PageKind::Leaf,
            3 => PageKind::Interior, 4 => PageKind::Overflow, _ => return None,
        })
    }
}

fn rd_u16(b: &[u8], at: usize) -> u16 { u16::from_le_bytes([b[at], b[at + 1]]) }
fn rd_u32(b: &[u8], at: usize) -> u32 { u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) }
fn rd_u64(b: &[u8], at: usize) -> u64 { u64::from_le_bytes(b[at..at + 8].try_into().unwrap()) }

/// Checksum over everything except the checksum field itself.
pub fn checksum(b: &[u8]) -> u32 {
    #[cfg(feature = "write-trace")]
    let trace_started = crate::write_trace::active().then(std::time::Instant::now);
    let mut h = crc32c::crc32c(&b[0..36]);
    h = crc32c::crc32c_append(h, &b[40..PAGE_SIZE]);
    #[cfg(feature = "write-trace")]
    if let Some(started) = trace_started {
        crate::write_trace::add(crate::write_trace::Field::PageCrc, started.elapsed());
        crate::write_trace::page_crc();
    }
    h
}

pub struct PageMut<'a> { b: &'a mut [u8] }

impl<'a> PageMut<'a> {
    pub fn init(b: &'a mut [u8], kind: PageKind, tree_id: u16, page_no: u32) -> Self {
        assert_eq!(b.len(), PAGE_SIZE);
        b.fill(0);
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4..6].copy_from_slice(&VERSION.to_le_bytes());
        b[6..8].copy_from_slice(&(kind as u16).to_le_bytes());
        b[8..10].copy_from_slice(&tree_id.to_le_bytes());
        b[12..16].copy_from_slice(&page_no.to_le_bytes());
        b[16..18].copy_from_slice(&(PAGE_SIZE as u16).to_le_bytes());
        PageMut { b }
    }

    /// Reopen an already-initialised buffer for modification.
    pub fn reopen(b: &'a mut [u8]) -> Self { PageMut { b } }

    fn nentries(&self) -> usize { rd_u16(self.b, 10) as usize }
    /// Number of entries currently in a page under construction. Public
    /// because `btree.rs`'s splits, and `pack_tree` in a later task, need to
    /// know where the next `insert_slot` call will land while building a
    /// fresh page from a collected `Vec` of records.
    pub fn nentries_pub(&self) -> usize { self.nentries() }
    fn free_ptr(&self) -> usize { rd_u16(self.b, 16) as usize }

    pub fn free_space(&self) -> usize {
        self.free_ptr().saturating_sub(HEADER_LEN + self.nentries() * SLOT_LEN)
    }

    pub fn set_next_leaf(&mut self, p: u32) { self.b[20..24].copy_from_slice(&p.to_le_bytes()); }
    /// Same field, named for its meaning on an interior page.
    pub fn set_child0(&mut self, p: u32) { self.set_next_leaf(p) }

    /// Insert `rec` so that it becomes entry `at`, shifting later slots right.
    pub fn insert_slot(&mut self, at: usize, rec: &[u8]) -> Result<()> {
        let n = self.nentries();
        if at > n { return Err(Error::TooLarge); }
        if rec.len() + SLOT_LEN > self.free_space() { return Err(Error::TooLarge); }

        let new_ptr = self.free_ptr() - rec.len();
        self.b[new_ptr..new_ptr + rec.len()].copy_from_slice(rec);
        self.b[16..18].copy_from_slice(&(new_ptr as u16).to_le_bytes());

        let dir = HEADER_LEN;
        let from = dir + at * SLOT_LEN;
        let to = dir + n * SLOT_LEN;
        self.b.copy_within(from..to, from + SLOT_LEN);
        self.b[from..from + 2].copy_from_slice(&(new_ptr as u16).to_le_bytes());
        self.b[from + 2..from + 4].copy_from_slice(&(rec.len() as u16).to_le_bytes());
        self.b[10..12].copy_from_slice(&((n + 1) as u16).to_le_bytes());
        Ok(())
    }

    /// Remove entry `at`. The payload bytes are abandoned in place; they are
    /// reclaimed only when the page is compacted.
    ///
    /// SACRIFICE (Law 4): a page can hold dead payload bytes it cannot reuse
    /// until compaction. Bought: deletion touches only the slot directory.
    pub fn remove_slot(&mut self, at: usize) {
        let n = self.nentries();
        assert!(at < n);
        let dir = HEADER_LEN;
        let from = dir + (at + 1) * SLOT_LEN;
        let to = dir + n * SLOT_LEN;
        self.b.copy_within(from..to, from - SLOT_LEN);
        self.b[10..12].copy_from_slice(&((n - 1) as u16).to_le_bytes());
    }

    /// Rewrite every live payload packed against the end of the page, in slot
    /// order, and reset `free_ptr` to reclaim whatever `remove_slot` left
    /// abandoned. This is the reclamation `remove_slot`'s doc comment
    /// promises; without it that comment was a cheque the code could not
    /// cash, and a caller that relied on it (a replace on an already-tight
    /// leaf) would corrupt the page instead.
    ///
    /// Bounded by one page: the snapshot below is at most `PAGE_SIZE` bytes,
    /// so this is RAM proportional to the page, not the store (Law 1).
    pub fn compact(&mut self) {
        let n = self.nentries();
        // Snapshot every live payload before any byte moves, since the
        // write-back below overwrites the very region these slices point
        // into.
        let payloads: Vec<Vec<u8>> = (0..n)
            .map(|i| {
                let base = HEADER_LEN + i * SLOT_LEN;
                let off = rd_u16(self.b, base) as usize;
                let len = rd_u16(self.b, base + 2) as usize;
                self.b[off..off + len].to_vec()
            })
            .collect();

        let mut ptr = PAGE_SIZE;
        for (i, payload) in payloads.iter().enumerate() {
            ptr -= payload.len();
            self.b[ptr..ptr + payload.len()].copy_from_slice(payload);
            let base = HEADER_LEN + i * SLOT_LEN;
            self.b[base..base + 2].copy_from_slice(&(ptr as u16).to_le_bytes());
            // The length half of the slot entry is already correct — a
            // payload's length never changes across a compaction.
        }
        self.b[16..18].copy_from_slice(&(ptr as u16).to_le_bytes());
    }

    /// Close out a mutation: stamp the LSN. The checksum is stamped by
    /// [`seal`] immediately before a page is WRITTEN -- a page mutated fifty
    /// times before eviction needs one checksum, not fifty (measured: 12.34
    /// full-page CRCs per row, 10 of them re-verifying resident pages, 34% of
    /// the write path). A dirty resident page's checksum field is meaningless;
    /// nothing reads it, and the only path to disk seals first.
    pub fn finalise(&mut self, lsn: u64) {
        self.b[24..32].copy_from_slice(&lsn.to_le_bytes());
    }
}

/// Stamp the checksum over current contents. Called by the pool immediately
/// before write_at, and nowhere else: checksummed going to the medium,
/// verified coming back, in between it is just memory (DuckDB: block manager;
/// SQLite: cksumvfs -- both at the I/O boundary, never per pin).
/// Stamp the publishing generation into the (formerly reserved) lsn field,
/// then checksum. Called at the ONLY two places bytes leave for disk
/// (flush_all, eviction), so every on-disk page carries the generation of
/// the epoch that wrote it -- the ordering signal recovery's duplicate-key
/// collapse needs once page numbers are recycled (2n). In-memory
/// `finalise(0)` call sites are untouched: the stamp happens on the way out.
pub fn seal(b: &mut [u8], gen: u64) {
    b[24..32].copy_from_slice(&gen.to_le_bytes());
    let c = checksum(b);
    b[36..40].copy_from_slice(&c.to_le_bytes());
}

#[derive(Debug)]
pub struct PageRef<'a> { b: &'a [u8] }

impl<'a> PageRef<'a> {
    /// Verify and open. `want` is the page number the caller asked the pool for.
    /// Verify and open a page just off the medium (checksum included). Call
    /// exactly where a page arrives from disk: BufferPool::load, and the raw
    /// buffers recover.rs reads itself.
    pub fn open(b: &'a [u8], want: u32) -> Result<Self> {
        Self::open_inner(b, want, true)
    }

    /// Open a resident pool page: every structural check, no CRC -- the pool
    /// verified it at load and nothing has been believed off the medium since.
    /// NOT a Law 5 relaxation: the law binds the medium boundary. Bounds
    /// checks stay per-pin (cheap, and they stop wild reads).
    pub fn open_resident(b: &'a [u8], want: u32) -> Result<Self> {
        Self::open_inner(b, want, false)
    }

    fn open_inner(b: &'a [u8], want: u32, verify_crc: bool) -> Result<Self> {
        let bad = |why| Err(Error::Corrupt { page_no: want, why });
        if b.len() != PAGE_SIZE { return bad("wrong buffer length"); }
        if rd_u32(b, 0) != MAGIC { return bad("bad magic"); }
        if rd_u16(b, 4) != VERSION { return bad("unknown format version"); }
        if verify_crc && rd_u32(b, 36) != checksum(b) { return bad("checksum mismatch"); }
        if rd_u32(b, 12) != want { return bad("page_no mismatch"); }
        if PageKind::from_u16(rd_u16(b, 6)).is_none() { return bad("unknown page kind"); }

        let n = rd_u16(b, 10) as usize;
        let dir_end = HEADER_LEN + n * SLOT_LEN;
        if dir_end > PAGE_SIZE { return bad("nentries exceeds page"); }
        let free_ptr = rd_u16(b, 16) as usize;
        if free_ptr < dir_end || free_ptr > PAGE_SIZE {
            return bad("free pointer is outside the page payload area");
        }
        for i in 0..n {
            let off = rd_u16(b, HEADER_LEN + i * SLOT_LEN) as usize;
            let len = rd_u16(b, HEADER_LEN + i * SLOT_LEN + 2) as usize;
            if off < free_ptr || off.checked_add(len).is_none_or(|end| end > PAGE_SIZE) {
                return bad("slot out of bounds");
            }
        }
        Ok(PageRef { b })
    }

    /// Open a page whose slot directory was ALREADY validated once during
    /// this residency (the pool's per-frame `validated` bit). Skips the
    /// O(entries) slot-bounds loop; keeps the constant-time identity checks
    /// (magic, version, page_no, kind) because they also guard against a
    /// caller-side page-number mixup, not just against disk damage. The
    /// authoritative validation boundary is `load` (D8): content can only
    /// have changed since via our own `PageMut` writes, which maintain the
    /// slot invariants by construction.
    pub fn open_resident_validated(b: &'a [u8], want: u32) -> Result<Self> {
        let bad = |why| Err(Error::Corrupt { page_no: want, why });
        if b.len() != PAGE_SIZE { return bad("wrong buffer length"); }
        if rd_u32(b, 0) != MAGIC { return bad("bad magic"); }
        if rd_u16(b, 4) != VERSION { return bad("unknown format version"); }
        if rd_u32(b, 12) != want { return bad("page_no mismatch"); }
        if PageKind::from_u16(rd_u16(b, 6)).is_none() { return bad("unknown page kind"); }
        Ok(PageRef { b })
    }

    pub fn kind(&self) -> PageKind { PageKind::from_u16(rd_u16(self.b, 6)).unwrap() }
    pub fn tree_id(&self) -> u16 { rd_u16(self.b, 8) }
    pub fn nentries(&self) -> usize { rd_u16(self.b, 10) as usize }
    pub fn page_no(&self) -> u32 { rd_u32(self.b, 12) }
    pub fn next_leaf(&self) -> u32 { rd_u32(self.b, 20) }
    /// Same field, named for its meaning on an interior page.
    pub fn child0(&self) -> u32 { self.next_leaf() }
    pub fn lsn(&self) -> u64 { rd_u64(self.b, 24) }

    /// The real free-space figure — identical to `PageMut::free_space` —
    /// available without taking a write pin. A caller that instead sums
    /// live entry lengths gets a number that silently diverges from this
    /// one the moment anything on the page was ever deleted, because a
    /// deletion reclaims only its slot-directory entry until `compact` runs.
    pub fn free_space(&self) -> usize {
        let free_ptr = rd_u16(self.b, 16) as usize;
        free_ptr.saturating_sub(HEADER_LEN + self.nentries() * SLOT_LEN)
    }

    pub fn slot(&self, i: usize) -> &'a [u8] {
        let off = rd_u16(self.b, HEADER_LEN + i * SLOT_LEN) as usize;
        let len = rd_u16(self.b, HEADER_LEN + i * SLOT_LEN + 2) as usize;
        &self.b[off..off + len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf() -> Vec<u8> { vec![0u8; PAGE_SIZE] }

    #[test]
    fn a_finalised_page_reads_back_its_identity() {
        let mut b = buf();
        let mut p = PageMut::init(&mut b, PageKind::Leaf, 7, 12345);
        p.finalise(99);
        seal(&mut b, 99); // seal's stamp is authoritative for the on-disk lsn
        let r = PageRef::open(&b, 12345).expect("verifies");
        assert_eq!(r.kind(), PageKind::Leaf);
        assert_eq!(r.tree_id(), 7);
        assert_eq!(r.page_no(), 12345);
        assert_eq!(r.lsn(), 99);
        assert_eq!(r.nentries(), 0);
    }

    #[test]
    fn entries_round_trip_in_slot_order() {
        let mut b = buf();
        let mut p = PageMut::init(&mut b, PageKind::Leaf, 1, 4);
        p.insert_slot(0, b"alpha").unwrap();
        p.insert_slot(1, b"beta").unwrap();
        p.insert_slot(1, b"between").unwrap(); // inserted between the two
        p.finalise(1);
        seal(&mut b, 7);

        let r = PageRef::open(&b, 4).unwrap();
        assert_eq!(r.nentries(), 3);
        assert_eq!(r.slot(0), b"alpha");
        assert_eq!(r.slot(1), b"between");
        assert_eq!(r.slot(2), b"beta");
    }

    #[test]
    fn a_free_pointer_outside_its_page_is_refused() {
        let mut b = buf();
        let mut p = PageMut::init(&mut b, PageKind::Leaf, 1, 4);
        p.finalise(1);
        b[16..18].copy_from_slice(&((PAGE_SIZE + 1) as u16).to_le_bytes());
        seal(&mut b, 7);

        assert!(matches!(
            PageRef::open(&b, 4),
            Err(Error::Corrupt { page_no: 4, .. })
        ));
    }

    #[test]
    fn a_page_asked_for_by_the_wrong_number_is_refused() {
        let mut b = buf();
        let mut p = PageMut::init(&mut b, PageKind::Leaf, 1, 500);
        p.finalise(1);
        seal(&mut b, 7);
        // The page is intact, but it is not the page that was requested.
        match PageRef::open(&b, 501) {
            Err(crate::Error::Corrupt { page_no: 501, why }) => assert_eq!(why, "page_no mismatch"),
            other => panic!("expected identity refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_single_flipped_byte_anywhere_is_refused() {
        let mut b = buf();
        let mut p = PageMut::init(&mut b, PageKind::Leaf, 1, 3);
        p.insert_slot(0, b"payload-bytes").unwrap();
        p.finalise(1);
        seal(&mut b, 7);
        assert!(PageRef::open(&b, 3).is_ok());

        for pos in [0usize, 9, 21, 37, 41, 100, PAGE_SIZE - 1] {
            let mut damaged = b.clone();
            damaged[pos] ^= 0xff;
            assert!(
                PageRef::open(&damaged, 3).is_err(),
                "corruption at byte {pos} was not detected"
            );
        }
    }

    #[test]
    fn a_corrupt_entry_count_cannot_read_past_the_page() {
        let mut b = buf();
        let mut p = PageMut::init(&mut b, PageKind::Leaf, 1, 3);
        p.insert_slot(0, b"x").unwrap();
        p.finalise(1);
        seal(&mut b, 7);
        // Forge a huge nentries and re-checksum, so only the bound check can save us.
        b[10..12].copy_from_slice(&60_000u16.to_le_bytes());
        recrc(&mut b);
        match PageRef::open(&b, 3) {
            Err(crate::Error::Corrupt { why, .. }) => assert_eq!(why, "nentries exceeds page"),
            other => panic!("expected bound refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_record_larger_than_the_page_is_refused_not_panicked() {
        let mut b = buf();
        let mut p = PageMut::init(&mut b, PageKind::Leaf, 1, 3);
        assert!(matches!(p.insert_slot(0, &vec![0u8; PAGE_SIZE]), Err(crate::Error::TooLarge)));
    }

    /// `compact` is the reclamation `remove_slot`'s doc comment promises.
    /// Pinned directly: remove the middle entry, compact, and confirm both
    /// that free space grows back by the removed entry's true size (not
    /// just its slot-directory entry) and that the surviving entries still
    /// read back correctly in order.
    #[test]
    fn compact_reclaims_a_removed_entrys_payload() {
        let mut b = buf();
        let mut p = PageMut::init(&mut b, PageKind::Leaf, 1, 5);
        p.insert_slot(0, b"alpha").unwrap();
        p.insert_slot(1, b"between-bytes").unwrap();
        p.insert_slot(2, b"beta").unwrap();
        let free_before_remove = p.free_space();

        p.remove_slot(1); // drop "between-bytes"; its payload is only abandoned
        let free_after_remove = p.free_space();
        // remove_slot reclaims just the 4-byte slot-directory entry — not
        // the 13 payload bytes of "between-bytes".
        assert_eq!(free_after_remove, free_before_remove + 4);

        p.compact();
        let free_after_compact = p.free_space();
        assert_eq!(
            free_after_compact,
            free_after_remove + "between-bytes".len(),
            "compact must reclaim the abandoned payload, not just the slot"
        );
        p.finalise(1);
        seal(&mut b, 7);

        let r = PageRef::open(&b, 5).unwrap();
        assert_eq!(r.nentries(), 2);
        assert_eq!(r.slot(0), b"alpha");
        assert_eq!(r.slot(1), b"beta");
    }

    /// `child0`/`set_child0` are the header field the whole interior-page
    /// convention hangs off: the leftmost child has no separator key and
    /// lives here instead of in the slot array. Load-bearing, so it gets its
    /// own direct round-trip rather than only incidental coverage from btree
    /// tests.
    #[test]
    fn child0_round_trips_through_an_interior_page() {
        let mut b = buf();
        let mut p = PageMut::init(&mut b, PageKind::Interior, 1, 9);
        p.set_child0(4242);
        p.finalise(1);
        seal(&mut b, 7);

        let r = PageRef::open(&b, 9).unwrap();
        assert_eq!(r.kind(), PageKind::Interior);
        assert_eq!(r.child0(), 4242);
    }

    fn recrc(b: &mut [u8]) {
        let c = crate::page::checksum(b);
        b[36..40].copy_from_slice(&c.to_le_bytes());
    }
}
