//! Variable-length records stored in slotted pages, with space returned on delete.
//!
//! # Why
//!
//! [`PageStore`] hands out fixed-size pages. Records are not fixed size — a row is
//! typically 60–300 bytes, so one record per 4 KB page would waste around 40× — and
//! a page-level free list alone cannot reclaim a single dead record.
//!
//! A *slotted page* is the standard answer, used by PostgreSQL, SQLite and every
//! disk-based engine that stores variable-length rows. A page holds many records
//! plus a small directory saying where each one is. Because a record is addressed
//! by its directory *slot* rather than by a byte offset, it can be moved within the
//! page — which is what makes space reclaimable without touching anything that
//! points at it.
//!
//! ```text
//!   ┌──────────┬─────────────────┬───────────────┬────────────────────────┐
//!   │  header  │ slot directory →│  free space   │← records               │
//!   └──────────┴─────────────────┴───────────────┴────────────────────────┘
//!    live/slot   (offset, len)      shrinks from    grow downward from the
//!    counts      per record         both ends       end of the page
//! ```
//!
//! # What this buys, and what it does not
//!
//! Deleting a record marks its slot dead and adds its bytes to the page's free
//! count. When a page's last live record goes, the page returns to the page store's
//! free list and the next allocation takes it back. **A workload that deletes as
//! fast as it inserts stops growing the file**, with no rewrite of anything.
//!
//! The limit worth stating plainly: this version appends into one open page at a
//! time and reclaims at *page* granularity. A rolling retention window — delete the
//! oldest, insert the newest — empties whole pages and reclaims perfectly, because
//! records written together sit together. A workload of scattered updates across a
//! large store leaves partly-used pages behind that are not revisited, so space is
//! reclaimed more slowly. Fixing that needs a free-space map of partly-filled pages;
//! it is a refinement of this, not a redesign of it.

use super::pagestore::PageStore;
use std::io;

/// Bytes of per-page bookkeeping before the slot directory.
const PAGE_HEADER: usize = 8;
/// Bytes per directory entry: offset (u16) and length (u16).
const SLOT_SIZE: usize = 4;

/// Slot number marking a record too large for one page, held instead in a chain
/// of pages beginning at the id's page. A real slot number cannot reach this: a
/// 4 KB page has room for a few hundred at most.
const OVERFLOW_SLOT: u16 = u16::MAX;

/// Per-page bookkeeping in an overflow chain: next page (u64), a marker (u32),
/// then bytes-in-this-page (u32).
///
/// The marker sits *after* the next-page field deliberately. Freeing a page writes
/// the free-list link over its first eight bytes, which would leave the rest of a
/// chain page looking entirely valid — a deleted record would still read, and worse,
/// would follow the free list as though it were the rest of its data. Deletion
/// clears the marker before freeing, and a read that does not find it returns
/// nothing rather than whatever the bytes happen to say.
const OVERFLOW_HEADER: usize = 16;
const OVERFLOW_MAGIC: u32 = 0x534B_4F56; // "SKOV"

/// Where a record lives: page number and directory slot, packed.
///
/// 48 bits of page number at 4 KB a page is a petabyte of addressable store; 16
/// bits of slot is far more records than fit in a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RecordId(pub u64);

impl RecordId {
    #[inline]
    pub(crate) fn new(page: u64, slot: u16) -> Self {
        debug_assert!(page < (1u64 << 48), "page number exceeds 48 bits");
        RecordId((page << 16) | slot as u64)
    }
    #[inline]
    pub(crate) fn page(self) -> u64 { self.0 >> 16 }
    #[inline]
    pub(crate) fn slot(self) -> u16 { (self.0 & 0xFFFF) as u16 }
}

/// Slotted-page record storage over a [`PageStore`].
pub(crate) struct RecordStore {
    pages: PageStore,
    /// The page currently being appended to, if any.
    open_page: Option<u64>,
    /// Scratch buffer, reused so a read or write does not allocate.
    buf: Vec<u8>,
}

// ── page accessors ───────────────────────────────────────────────────────────
// header: slot_count u16 | live_count u16 | heap_start u16 | reserved u16
// `heap_start` is where record bytes begin, growing downward from the page end.

fn rd16(p: &[u8], at: usize) -> u16 { u16::from_le_bytes([p[at], p[at + 1]]) }
fn wr16(p: &mut [u8], at: usize, v: u16) { p[at..at + 2].copy_from_slice(&v.to_le_bytes()); }

/// How many slots this page claims, capped at how many it could physically hold.
///
/// The count comes off disk, so it can be anything — a page that was freed holds a
/// free-list pointer in these bytes, and a file in some other format holds whatever
/// it holds. Uncapped, a claim of 19 283 slots sent `slot_entry` reading 51 962
/// bytes into a 4 096-byte page. Capping it means a nonsense page reads as empty or
/// as garbage records, never as a panic and never past the end of the buffer.
fn slot_count(p: &[u8]) -> usize {
    let max = (p.len().saturating_sub(PAGE_HEADER)) / SLOT_SIZE;
    (rd16(p, 0) as usize).min(max)
}
fn live_count(p: &[u8]) -> usize { rd16(p, 2) as usize }
fn heap_start(p: &[u8]) -> usize { rd16(p, 4) as usize }

/// A live slot's `(offset, length)`, or `None` if the slot is dead.
///
/// The stored length is the real length **plus one**, so zero can mean "dead"
/// without colliding with a genuinely empty record. Using 0 for both made an empty
/// payload read as deleted — which the comparison against the flat store caught,
/// and which would otherwise have surfaced as a row silently losing its contents.
fn slot_entry(p: &[u8], i: usize) -> Option<(usize, usize)> {
    let at = PAGE_HEADER + i * SLOT_SIZE;
    match rd16(p, at + 2) {
        0 => None,
        encoded => Some((rd16(p, at) as usize, encoded as usize - 1)),
    }
}

fn set_slot(p: &mut [u8], i: usize, off: usize, len: usize) {
    let at = PAGE_HEADER + i * SLOT_SIZE;
    wr16(p, at, off as u16);
    wr16(p, at + 2, (len + 1) as u16);
}

fn kill_slot(p: &mut [u8], i: usize) {
    let at = PAGE_HEADER + i * SLOT_SIZE;
    wr16(p, at, 0);
    wr16(p, at + 2, 0);
}

impl RecordStore {
    pub(crate) fn create(path: &std::path::Path, page_size: usize) -> io::Result<Self> {
        Ok(Self { pages: PageStore::create(path, page_size)?, open_page: None, buf: Vec::new() })
    }

    pub(crate) fn open(path: &std::path::Path) -> io::Result<Option<Self>> {
        Ok(PageStore::open(path)?
            .map(|pages| Self { pages, open_page: None, buf: Vec::new() }))
    }

    pub(crate) fn page_count(&self) -> u64 { self.pages.page_count() }
    pub(crate) fn free_page_count(&self) -> u64 { self.pages.free_count() }
    pub(crate) fn sync(&mut self) -> io::Result<()> { self.pages.sync() }

    #[cfg(test)]
    fn pages_for_test(&self) -> &PageStore { &self.pages }

    /// Two words in the page header that this store does not use, for whoever owns
    /// it to keep a tally in. Written when the header is, which is on `sync`.
    pub(crate) fn user_meta(&self) -> (u64, u64) { self.pages.user_meta() }
    pub(crate) fn set_user_meta(&mut self, a: u64, b: u64) { self.pages.set_user_meta(a, b) }

    /// Largest record that fits in a page, allowing for the header and its slot.
    pub(crate) fn max_record_len(&self) -> usize {
        self.pages.page_size() - PAGE_HEADER - SLOT_SIZE
    }

    /// An empty record is a real record, distinct from a deleted one.
    #[cfg(test)]
    fn _empty_is_representable() {}

    fn fresh_page(&mut self) -> io::Result<u64> {
        let page = self.pages.alloc()?;
        let ps = self.pages.page_size();
        let mut blank = vec![0u8; ps];
        wr16(&mut blank, 0, 0);          // slot_count
        wr16(&mut blank, 2, 0);          // live_count
        wr16(&mut blank, 4, ps as u16);  // heap_start — empty heap begins at the end
        self.pages.write(page, &blank)?;
        Ok(page)
    }

    /// Does `page` have room for a record of `len` bytes plus a new slot?
    fn fits(&self, p: &[u8], len: usize) -> bool {
        let dir_end = PAGE_HEADER + (slot_count(p) + 1) * SLOT_SIZE;
        heap_start(p).saturating_sub(len) >= dir_end
    }

    /// Bytes held by records whose slot is dead.
    fn dead_bytes(&self, p: &[u8]) -> usize {
        let live: usize = (0..slot_count(p)).filter_map(|i| slot_entry(p, i)).map(|(_, l)| l).sum();
        (self.pages.page_size() - heap_start(p)).saturating_sub(live)
    }

    /// Slide the live records of a page together, reclaiming the gaps left by
    /// deleted ones.
    ///
    /// This is the payoff of addressing a record by its directory slot rather than
    /// by a byte position: the bytes move, the slots are rewritten to follow them,
    /// and every reference from outside the page stays valid. Without it, space
    /// inside a page comes back only when its *last* record dies — which measured
    /// as a 73 % larger payload file on a store that had been fully overwritten.
    fn compact_page(&self, p: &mut [u8]) {
        let ps = p.len();
        let n = slot_count(p);
        // Bounded, because `off` and `len` come off disk. `read` already refuses a
        // slot that does not describe real bytes; this path did not, so a page whose
        // directory said "offset 4000, length 200" panicked here instead — reached
        // from `insert` whenever a page looks full and its dead space looks
        // reclaimable. A slot that cannot be believed is dropped rather than moved.
        let live: Vec<(usize, Vec<u8>)> = (0..n)
            .filter_map(|i| {
                let (off, len) = slot_entry(p, i)?;
                if off.checked_add(len)? > ps { return None }
                Some((i, p[off..off + len].to_vec()))
            })
            .collect();
        let mut cursor = ps;
        for (i, bytes) in &live {
            cursor -= bytes.len();
            p[cursor..cursor + bytes.len()].copy_from_slice(bytes);
            set_slot(p, *i, cursor, bytes.len());
        }
        wr16(p, 4, cursor as u16);
    }

    /// Bytes of payload an overflow page carries.
    fn overflow_capacity(&self) -> usize {
        self.pages.page_size() - OVERFLOW_HEADER
    }

    /// Store a record too large for one page as a chain of pages.
    ///
    /// Written back to front so each page can record the one that follows it,
    /// which means the chain is complete the moment its head exists — there is no
    /// window where a reader could follow a half-built chain.
    fn insert_overflow(&mut self, bytes: &[u8]) -> io::Result<RecordId> {
        let cap = self.overflow_capacity();
        let chunks: Vec<&[u8]> = bytes.chunks(cap).collect();
        let mut next: u64 = 0;
        let mut head: u64 = 0;
        for chunk in chunks.iter().rev() {
            let page = self.pages.alloc()?;
            let mut buf = vec![0u8; self.pages.page_size()];
            buf[0..8].copy_from_slice(&next.to_le_bytes());
            buf[8..12].copy_from_slice(&OVERFLOW_MAGIC.to_le_bytes());
            buf[12..16].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
            buf[OVERFLOW_HEADER..OVERFLOW_HEADER + chunk.len()].copy_from_slice(chunk);
            self.pages.write(page, &buf)?;
            next = page;
            head = page;
        }
        Ok(RecordId::new(head, OVERFLOW_SLOT))
    }

    fn read_overflow(&self, head: u64) -> io::Result<Option<Vec<u8>>> {
        let ps = self.pages.page_size();
        let mut out = Vec::new();
        let mut page = head;
        let mut buf = vec![0u8; ps];
        // A chain is pointers stored in the pages it links, so damage makes it point
        // anywhere — including back at a page already visited. Following that with no
        // bound appends forever until memory runs out. No chain can legitimately
        // visit more pages than the store holds, so that is the bound; the B+tree
        // walks are bounded the same way.
        let mut budget = self.pages.page_count().saturating_add(1);
        while page != 0 {
            budget = match budget.checked_sub(1) { Some(b) => b, None => return Ok(None) };
            if self.pages.read(page, &mut buf).is_err() {
                return Ok(None);
            }
            let next = u64::from_le_bytes(buf[0..8].try_into().unwrap());
            if u32::from_le_bytes(buf[8..12].try_into().unwrap()) != OVERFLOW_MAGIC {
                return Ok(None); // freed, or never a chain page — do not read on
            }
            let n = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
            if n > ps - OVERFLOW_HEADER {
                return Ok(None);
            }
            out.extend_from_slice(&buf[OVERFLOW_HEADER..OVERFLOW_HEADER + n]);
            page = next;
        }
        Ok(Some(out))
    }

    fn delete_overflow(&mut self, head: u64) -> io::Result<bool> {
        let ps = self.pages.page_size();
        let mut buf = vec![0u8; ps];
        let mut page = head;
        let mut freed = false;
        while page != 0 {
            if self.pages.read(page, &mut buf).is_err() {
                break;
            }
            let next = u64::from_le_bytes(buf[0..8].try_into().unwrap());
            if u32::from_le_bytes(buf[8..12].try_into().unwrap()) != OVERFLOW_MAGIC {
                break; // already freed — stop rather than walk the free list
            }
            // Clear the marker first, so the page cannot be mistaken for live data
            // once the free list has written its link over the next-page field.
            buf[8..12].copy_from_slice(&0u32.to_le_bytes());
            self.pages.write(page, &buf)?;
            self.pages.free(page)?;
            freed = true;
            page = next;
        }
        Ok(freed)
    }

    /// Store `bytes` and return where it went. Records larger than a page are
    /// held in a chain of pages instead of being refused.
    pub(crate) fn insert(&mut self, bytes: &[u8]) -> io::Result<RecordId> {
        if bytes.len() > self.max_record_len() {
            return self.insert_overflow(bytes);
        }
        let ps = self.pages.page_size();
        self.buf.resize(ps, 0);

        // Use the open page if the record fits, otherwise start a new one.
        let page = match self.open_page {
            Some(p) => {
                self.pages.read(p, &mut self.buf)?;
                // Reclaim the gaps before giving up on this page — a page that
                // looks full may be mostly dead records.
                if !self.fits(&self.buf, bytes.len()) && self.dead_bytes(&self.buf) >= bytes.len() {
                    let mut page_buf = std::mem::take(&mut self.buf);
                    self.compact_page(&mut page_buf);
                    self.pages.write(p, &page_buf)?;
                    self.buf = page_buf;
                }
                if self.fits(&self.buf, bytes.len()) {
                    p
                } else {
                    let np = self.fresh_page()?;
                    self.pages.read(np, &mut self.buf)?;
                    self.open_page = Some(np);
                    np
                }
            }
            None => {
                let np = self.fresh_page()?;
                self.pages.read(np, &mut self.buf)?;
                self.open_page = Some(np);
                np
            }
        };

        let n = slot_count(&self.buf);
        let off = heap_start(&self.buf) - bytes.len();
        self.buf[off..off + bytes.len()].copy_from_slice(bytes);
        set_slot(&mut self.buf, n, off, bytes.len());
        wr16(&mut self.buf, 0, (n + 1) as u16);
        let live = live_count(&self.buf) + 1;
        wr16(&mut self.buf, 2, live as u16);
        wr16(&mut self.buf, 4, off as u16);
        let page_bytes = std::mem::take(&mut self.buf);
        self.pages.write(page, &page_bytes)?;
        self.buf = page_bytes;
        Ok(RecordId::new(page, n as u16))
    }

    /// Reads take `&self` so they can serve an immutable caller, and allocate a
    /// page-sized buffer rather than sharing the scratch one used by writes.
    pub(crate) fn read(&self, id: RecordId) -> io::Result<Option<Vec<u8>>> {
        if id.slot() == OVERFLOW_SLOT {
            return self.read_overflow(id.page());
        }
        let ps = self.pages.page_size();
        let mut buf = vec![0u8; ps];
        if self.pages.read(id.page(), &mut buf).is_err() {
            return Ok(None);
        }
        let i = id.slot() as usize;
        if i >= slot_count(&buf) {
            return Ok(None);
        }
        let Some((off, len)) = slot_entry(&buf, i) else {
            return Ok(None); // dead slot
        };
        if off + len > ps {
            return Ok(None); // does not describe real bytes
        }
        Ok(Some(buf[off..off + len].to_vec()))
    }

    /// Delete a record. When a page's last live record goes, the page itself is
    /// returned to the free list — which is what stops the file growing.
    pub(crate) fn delete(&mut self, id: RecordId) -> io::Result<bool> {
        if id.slot() == OVERFLOW_SLOT {
            return self.delete_overflow(id.page());
        }
        let ps = self.pages.page_size();
        self.buf.resize(ps, 0);
        if self.pages.read(id.page(), &mut self.buf).is_err() {
            return Ok(false);
        }
        let i = id.slot() as usize;
        if i >= slot_count(&self.buf) {
            return Ok(false);
        }
        if slot_entry(&self.buf, i).is_none() {
            return Ok(false); // already dead
        }
        kill_slot(&mut self.buf, i);
        // Saturating, because `live_count` is a number off disk like every other:
        // a page whose header says zero live records while a slot still reads as
        // live would otherwise underflow to `usize::MAX` and write `0xFFFF` back as
        // the count, turning one damaged page into a permanently wrong one.
        let live = live_count(&self.buf).saturating_sub(1);
        wr16(&mut self.buf, 2, live as u16);
        let page_bytes = std::mem::take(&mut self.buf);
        self.pages.write(id.page(), &page_bytes)?;
        self.buf = page_bytes;

        if live == 0 {
            if self.open_page == Some(id.page()) {
                self.open_page = None;
            }
            self.pages.free(id.page())?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::pagestore::DEFAULT_PAGE_SIZE;

    fn store(dir: &tempfile::TempDir) -> RecordStore {
        RecordStore::create(&dir.path().join("records.bin"), DEFAULT_PAGE_SIZE).unwrap()
    }

    fn rec(i: usize) -> Vec<u8> {
        format!("{{\"_key\":\"n{i}\",\"name\":\"record {i} west java\",\"n\":{i}}}").into_bytes()
    }

    #[test]
    fn records_read_back_exactly() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let ids: Vec<RecordId> = (0..500).map(|i| s.insert(&rec(i)).unwrap()).collect();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(s.read(*id).unwrap().as_deref(), Some(rec(i).as_slice()), "record {i}");
        }
    }

    #[test]
    fn many_records_share_a_page() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        for i in 0..50 { s.insert(&rec(i)).unwrap(); }
        // ~50 bytes each into 4 KB pages: a handful of pages, not fifty.
        assert!(s.page_count() < 5,
                "records are not sharing pages: {} pages for 50 records", s.page_count());
    }

    #[test]
    fn a_deleted_record_stops_reading() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let a = s.insert(b"alpha").unwrap();
        let b = s.insert(b"bravo").unwrap();
        assert!(s.delete(a).unwrap());
        assert_eq!(s.read(a).unwrap(), None, "a deleted record still reads");
        assert_eq!(s.read(b).unwrap().as_deref(), Some(&b"bravo"[..]), "neighbour was disturbed");
        assert!(!s.delete(a).unwrap(), "deleting twice should report nothing done");
    }

    #[test]
    fn emptying_a_page_returns_it_to_the_free_list() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let ids: Vec<RecordId> = (0..200).map(|i| s.insert(&rec(i)).unwrap()).collect();
        assert_eq!(s.free_page_count(), 0);
        for id in &ids { s.delete(*id).unwrap(); }
        assert!(s.free_page_count() > 0,
                "no page came back after every record in it was deleted");
    }

    /// The property this whole direction exists for: delete as fast as you insert
    /// and the file stops growing, with nothing rewritten.
    #[test]
    fn a_rolling_window_stops_growing_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let per_round = 500usize;
        let window = 5usize;

        let mut rounds: Vec<Vec<RecordId>> = Vec::new();
        let mut early = 0u64;
        for round in 0..40 {
            let batch: Vec<RecordId> =
                (0..per_round).map(|i| s.insert(&rec(round * per_round + i)).unwrap()).collect();
            rounds.push(batch);
            if rounds.len() > window {
                for id in rounds.remove(0) { s.delete(id).unwrap(); }
            }
            if round == 15 { early = s.page_count(); }
        }
        let late = s.page_count();

        // Bounded, not exactly constant. A page holding records from two rounds is
        // only freed once BOTH have expired, so the steady state sits a page or two
        // above the ideal — that boundary effect is inherent, not a leak. What must
        // not happen is growth proportional to the number of rounds: without
        // reclamation these 24 further rounds would have added ~24 x 7 pages.
        assert!(late <= early + 3,
                "file grew from {early} to {late} pages over 24 further rounds — \
                 space is not coming back");

        let live = window * per_round;
        assert!(late < (live / 60) as u64 + 20,
                "{late} pages is far more than {live} live records should need");
    }

    #[test]
    fn records_survive_a_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("records.bin");
        let ids: Vec<RecordId>;
        {
            let mut s = RecordStore::create(&path, DEFAULT_PAGE_SIZE).unwrap();
            ids = (0..300).map(|i| s.insert(&rec(i)).unwrap()).collect();
            s.sync().unwrap();
        }
        let s = RecordStore::open(&path).unwrap().expect("store should reopen");
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(s.read(*id).unwrap().as_deref(), Some(rec(i).as_slice()), "record {i}");
        }
    }

    /// Records larger than a page are chained across pages, including the awkward
    /// sizes either side of the boundary and a multi-megabyte one.
    #[test]
    fn oversized_records_are_chained_across_pages() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let max = s.max_record_len();
        for len in [max, max + 1, DEFAULT_PAGE_SIZE, DEFAULT_PAGE_SIZE * 3 + 17, 2_000_000] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let id = s.insert(&payload).unwrap();
            assert_eq!(s.read(id).unwrap().as_deref(), Some(payload.as_slice()),
                       "a {len}-byte record did not read back intact");
        }
    }

    #[test]
    fn deleting_a_chained_record_returns_every_page() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let payload = vec![b'z'; DEFAULT_PAGE_SIZE * 10];
        let before = s.page_count();
        let id = s.insert(&payload).unwrap();
        assert!(s.page_count() > before + 5, "a 10-page record used too few pages");

        assert!(s.delete(id).unwrap());
        assert_eq!(s.read(id).unwrap(), None, "a deleted chained record still reads");
        assert!(s.free_page_count() >= 10,
                "only {} pages came back from a ten-page record", s.free_page_count());

        // And the space is genuinely reusable.
        let high_water = s.page_count();
        let id2 = s.insert(&payload).unwrap();
        assert_eq!(s.page_count(), high_water,
                   "the file grew instead of reusing the freed chain");
        assert_eq!(s.read(id2).unwrap().map(|v| v.len()), Some(payload.len()));
    }

    /// A page that is not a slotted page must read as empty, not as a panic.
    ///
    /// Pages come off disk. A freed one holds a free-list pointer in the bytes the
    /// header occupies, and a file in another format holds whatever it holds — so
    /// the slot count is not a number this code may trust. Uncapped, a page
    /// claiming 19 283 slots sent a read 51 962 bytes into a 4 096-byte buffer.
    #[test]
    fn a_page_that_is_not_a_slotted_page_reads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("rec.bin");
        let mut s = RecordStore::create(&path, DEFAULT_PAGE_SIZE).unwrap();
        let good = s.insert(b"a real record").unwrap();

        // Slots far past what a page can hold, on the live page and on pages that
        // do not exist. None may panic and none may read past the end of a buffer.
        // Slot 0 of the live page is left alone: deleting it would succeed, which
        // is correct behaviour and not what is under test.
        for page in [good.page(), good.page() + 1, u64::MAX >> 16] {
            for slot in [1u16, 1_021, 1_022, 4_000, 19_282, u16::MAX - 1] {
                assert!(s.read(RecordId::new(page, slot)).unwrap().is_none(),
                        "page {page} slot {slot} produced a record out of nothing");
                assert!(!s.delete(RecordId::new(page, slot)).unwrap(),
                        "page {page} slot {slot} reported deleting something");
            }
        }
        // The real record is still there and still right.
        assert_eq!(s.read(good).unwrap().as_deref(), Some(b"a real record".as_slice()));

        // And the same against a page holding a free-list pointer rather than a
        // slotted header, which is what a freed page actually looks like. Its own
        // store, and one record big enough to have the page to itself, so deleting
        // it really does return the page to the free list.
        let solo_path = dir.path().join("solo.bin");
        let mut solo = RecordStore::create(&solo_path, DEFAULT_PAGE_SIZE).unwrap();
        let filler = vec![b'z'; solo.max_record_len()];
        let victim = solo.insert(&filler).unwrap();
        assert!(solo.delete(victim).unwrap());
        assert!(solo.free_page_count() > 0, "the page did not reach the free list");
        for slot in [0u16, 1, 1_022, 19_282] {
            assert!(solo.read(RecordId::new(victim.page(), slot)).unwrap().is_none(),
                    "a freed page produced a record at slot {slot}");
        }
    }

    /// Three page states that a damaged store can be in, each of which used to
    /// break a path that the read path already defended against.
    ///
    /// Found by an independent review rather than by fuzzing: they need a *specific*
    /// page shape, and random byte flips reach them only by luck. All three are the
    /// same mistake as the ones fuzzing did find — a number off disk used without
    /// asking whether it could be one — in the three places that had been missed.
    #[test]
    fn damaged_pages_do_not_break_the_write_paths() {
        use std::io::{Seek, SeekFrom, Write};

        // (1) A slot claiming bytes past the end of its page. `read` refuses such a
        // slot; `compact_page` sliced with it, and is reached from `insert` whenever
        // a page looks full and its dead space looks reclaimable.
        {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("r.bin");
            let mut s = RecordStore::create(&path, DEFAULT_PAGE_SIZE).unwrap();
            let first = s.insert(&vec![b'a'; 200]).unwrap();
            for _ in 0..8 { s.insert(&vec![b'b'; 300]).unwrap(); }
            s.sync().unwrap();
            drop(s);
            // Slot 0 of that page: offset 4000, length 200 — 4200 > 4096.
            {
                let mut fh = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                let at = first.page() * DEFAULT_PAGE_SIZE as u64 + PAGE_HEADER as u64;
                fh.seek(SeekFrom::Start(at)).unwrap();
                fh.write_all(&4000u16.to_le_bytes()).unwrap();
                fh.write_all(&201u16.to_le_bytes()).unwrap(); // stored length + 1
            }
            let mut s = RecordStore::open(&path).unwrap().unwrap();
            // Inserts must keep working rather than panicking in compaction.
            for i in 0..40 {
                s.insert(&vec![b'c'; 300]).unwrap_or_else(|e| panic!("insert {i} failed: {e}"));
            }
        }

        // (2) An overflow chain that points at itself. Following it appended
        // forever, filling memory; every other walk in this crate is bounded.
        {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("o.bin");
            let mut s = RecordStore::create(&path, DEFAULT_PAGE_SIZE).unwrap();
            let big = s.insert(&vec![b'z'; DEFAULT_PAGE_SIZE * 3]).unwrap();
            s.sync().unwrap();
            drop(s);
            {
                // Point the head page's `next` back at itself.
                let mut fh = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                fh.seek(SeekFrom::Start(big.page() * DEFAULT_PAGE_SIZE as u64)).unwrap();
                fh.write_all(&big.page().to_le_bytes()).unwrap();
            }
            let s = RecordStore::open(&path).unwrap().unwrap();
            let got = s.read(big).unwrap();
            // Whatever it returns, it must return: bounded, not endless.
            assert!(got.map_or(true, |v| v.len() <= DEFAULT_PAGE_SIZE * 64),
                    "a self-referencing overflow chain produced an unbounded record");
        }

        // (3) A page whose header says nothing is live while a slot still reads as
        // live. `live_count - 1` underflowed to usize::MAX and wrote 0xFFFF back.
        {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("d.bin");
            let mut s = RecordStore::create(&path, DEFAULT_PAGE_SIZE).unwrap();
            let id = s.insert(b"a record").unwrap();
            s.sync().unwrap();
            drop(s);
            {
                let mut fh = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                fh.seek(SeekFrom::Start(id.page() * DEFAULT_PAGE_SIZE as u64 + 2)).unwrap();
                fh.write_all(&0u16.to_le_bytes()).unwrap(); // live_count = 0
            }
            let mut s = RecordStore::open(&path).unwrap().unwrap();
            let _ = s.delete(id); // must not panic, must not write 0xFFFF back
            let mut probe = vec![0u8; DEFAULT_PAGE_SIZE];
            s.pages_for_test().read(id.page(), &mut probe).unwrap();
            assert!(rd16(&probe, 2) < 1000,
                    "deleting from a page with a zeroed live count wrote {} back",
                    rd16(&probe, 2));
        }
    }
}
