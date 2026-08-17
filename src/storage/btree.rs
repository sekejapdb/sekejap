//! A B+tree of pages — the index that can be inserted into without a rebuild.
//!
//! # Why
//!
//! `idx.bin` maps a record's hash to its identity, and it is a **sorted array**. A
//! sorted array cannot be inserted into without shifting everything after the
//! insertion point, so new records cannot go into it at all. That is the reason the
//! RAM overlay exists, the reason the overlay has to be folded back periodically,
//! and therefore the reason compaction exists: the fold is a rebuild of the whole
//! array, costing `O(store)` for `O(change)` input.
//!
//! Paged payloads removed compaction's largest phase, but not this one. An index
//! that accepts an insert in place removes the rest.
//!
//! A B+tree is the standard answer and the one SQLite uses for every table. An
//! insert descends to a leaf, writes there, and splits only when a page is full —
//! touching `O(log n)` pages, never the whole structure. Nothing is deferred, so
//! nothing accumulates, so there is no batch to run later.
//!
//! # Shape
//!
//! A key is a `u128` and a value a `u64`. The wide key is what lets a *composite*
//! key — `(collection, node)`, or `(node, sequence)` — be a single ordered key, so
//! "every member of this collection" or "every edge from this node" is a range scan
//! rather than a structure of its own. Fixed width keeps a page a plain array, so a
//! lookup within one is a binary search over `count` entries rather than a walk.
//!
//! ```text
//!   internal:  [ hdr | child₀ | k₀ child₁ | k₁ child₂ | … ]   descend on key < kᵢ
//!   leaf:      [ hdr | k₀ v₀ | k₁ v₁ | … | next ]            sorted, linked
//! ```
//!
//! Leaves are linked so an ordered scan does not have to descend repeatedly.
//!
//! # What this version does not do
//!
//! Deletion removes the entry from its leaf but does not merge underfull leaves.
//! A page returns to the free list only when it empties completely. Under a rolling
//! workload — the case this is for — keys arrive and expire in similar order, so
//! leaves do empty. A workload that deletes scattered keys leaves sparse leaves
//! behind, which costs space and lengthens scans but never returns wrong answers.
//! Merging is a refinement of this, not a redesign.

use super::pagestore::PageStore;
use std::io;

const KIND_LEAF: u8 = 0;
const KIND_INTERNAL: u8 = 1;

/// kind(1) pad(1) count(2) pad(4) next(8)
const HDR: usize = 16;
const ENTRY: usize = 24; // u128 key + u64 value/child

fn rd8(p: &[u8], at: usize) -> u64 { u64::from_le_bytes(p[at..at + 8].try_into().unwrap()) }
fn wr8(p: &mut [u8], at: usize, v: u64) { p[at..at + 8].copy_from_slice(&v.to_le_bytes()); }
fn rd16b(p: &[u8], at: usize) -> u128 { u128::from_le_bytes(p[at..at + 16].try_into().unwrap()) }
fn wr16b(p: &mut [u8], at: usize, v: u128) { p[at..at + 16].copy_from_slice(&v.to_le_bytes()); }
/// How many entries this page claims, capped at how many it could physically hold.
///
/// The count comes off disk and drives every index into the page, so an uncapped
/// one reads wherever it likes: a page whose header bytes happen to say 60 000
/// sends `leaf_key` 1.4 MB into a 4 KB buffer. The same shape of bug in the record
/// store's slot count sent a read 51 962 bytes into a 4 096-byte page. Capping
/// makes a nonsense page read as a page full of nonsense entries, which the tree
/// then fails to find anything in — a wrong answer is recoverable, a panic in the
/// middle of a query is not.
///
/// The cap is the leaf capacity, which is the larger of the two layouts; the
/// internal accessors bound themselves, so both are safe without either being
/// short by an entry. Capping at the *internal* capacity instead cost a full leaf
/// its last entry, which the tree's own tests caught immediately — a bound that
/// changes correct behaviour is not a bound, it is a bug with a rationale.
fn count(p: &[u8]) -> usize {
    let max = p.len().saturating_sub(HDR) / ENTRY;
    (u16::from_le_bytes([p[2], p[3]]) as usize).min(max)
}
fn set_count(p: &mut [u8], n: usize) { p[2..4].copy_from_slice(&(n as u16).to_le_bytes()); }
fn kind(p: &[u8]) -> u8 { p[0] }
fn next_leaf(p: &[u8]) -> u64 { rd8(p, 8) }
fn set_next_leaf(p: &mut [u8], v: u64) { wr8(p, 8, v) }

// Leaf entry i: key at HDR + i*ENTRY, value 16 bytes further in.
//
// Both bound themselves against the page. `count` is capped, so a well-formed
// page never reaches these guards; a page whose bytes are anything at all cannot
// walk out of the buffer through them either. A key read past the end comes back
// as the maximum, which sorts after everything and so is simply not found.
fn leaf_key(p: &[u8], i: usize) -> u128 {
    let at = HDR + i * ENTRY;
    if at + 16 > p.len() { return u128::MAX }
    rd16b(p, at)
}
fn leaf_val(p: &[u8], i: usize) -> u64 {
    let at = HDR + i * ENTRY + 16;
    if at + 8 > p.len() { return 0 }
    rd8(p, at)
}
fn set_leaf(p: &mut [u8], i: usize, k: u128, v: u64) {
    wr16b(p, HDR + i * ENTRY, k);
    wr8(p, HDR + i * ENTRY + 16, v);
}

// Internal node: child0 at HDR, then (key, child) pairs from HDR+8.
fn child0(p: &[u8]) -> u64 { rd8(p, HDR) }
fn set_child0(p: &mut [u8], v: u64) { wr8(p, HDR, v) }
fn int_key(p: &[u8], i: usize) -> u128 {
    let at = HDR + 8 + i * ENTRY;
    if at + 16 > p.len() { return u128::MAX }
    rd16b(p, at)
}
fn int_child(p: &[u8], i: usize) -> u64 {
    let at = HDR + 8 + i * ENTRY + 16;
    if at + 8 > p.len() { return NO_CHILD }
    rd8(p, at)
}

/// A child pointer that is not a page. Page 0 is the store's header, so a descent
/// that reaches it reads a page that is not a tree node and finds nothing — wrong,
/// bounded, and reachable only from a damaged page.
const NO_CHILD: u64 = 0;
fn set_int(p: &mut [u8], i: usize, k: u128, c: u64) {
    wr16b(p, HDR + 8 + i * ENTRY, k);
    wr8(p, HDR + 8 + i * ENTRY + 16, c);
}

/// What an insert reports back up the descent when a child had to split.
struct Split { key: u128, right: u64 }

pub(crate) struct BTree {
    pages: PageStore,
    root: u64,
    len: u64,
}

impl BTree {
    fn leaf_cap(&self) -> usize { (self.pages.page_size() - HDR) / ENTRY }
    fn int_cap(&self) -> usize { (self.pages.page_size() - HDR - 8) / ENTRY }

    pub(crate) fn create(path: &std::path::Path, page_size: usize) -> io::Result<Self> {
        let mut pages = PageStore::create(path, page_size)?;
        let root = pages.alloc()?;
        let mut blank = vec![0u8; page_size];
        blank[0] = KIND_LEAF;
        pages.write(root, &blank)?;
        let mut t = Self { pages, root, len: 0 };
        t.sync()?;
        Ok(t)
    }

    /// Record the root and entry count in the store's header words, so reopening
    /// needs no scan.
    ///
    /// This does NOT fsync. Syncing on every insert cost an fsync per key — 3 ms
    /// each, turning 50 000 inserts into three minutes — which would have made the
    /// structure useless for the write path it exists to serve. Durability is the
    /// caller's decision, taken at `sync()`.
    fn note_meta(&mut self) {
        self.pages.set_user_meta(self.root, self.len);
    }

    pub(crate) fn open(path: &std::path::Path) -> io::Result<Option<Self>> {
        Ok(PageStore::open(path)?.map(|pages| {
            let (root, len) = pages.user_meta();
            Self { pages, root, len }
        }))
    }

    pub(crate) fn len(&self) -> u64 { self.len }
    pub(crate) fn page_count(&self) -> u64 { self.pages.page_count() }
    pub(crate) fn sync(&mut self) -> io::Result<()> {
        self.note_meta();
        self.pages.sync()
    }

    fn read_page(&self, page: u64) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; self.pages.page_size()];
        self.pages.read(page, &mut buf)?;
        Ok(buf)
    }

    /// A page for reading, borrowed from the mapping when it can be.
    ///
    /// Descending a tree touches three or four pages and every one of them was a
    /// fresh `Vec` plus a 4 KB copy. Borrowing costs neither. The owned fallback
    /// stays for pages the mapping does not reach — anything allocated since the
    /// last sync — so this is a speed-up with no behaviour attached to it.
    fn page_for_read(&self, page: u64) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        if let Some(b) = self.pages.page_slice(page) {
            return Ok(std::borrow::Cow::Borrowed(b));
        }
        Ok(std::borrow::Cow::Owned(self.read_page(page)?))
    }

    /// Index of the first entry with key >= `key`.
    fn lower_bound_leaf(p: &[u8], key: u128) -> usize {
        let (mut lo, mut hi) = (0usize, count(p));
        while lo < hi {
            let mid = (lo + hi) / 2;
            if leaf_key(p, mid) < key { lo = mid + 1 } else { hi = mid }
        }
        lo
    }

    /// Which child to descend into for `key`.
    fn descend_to(p: &[u8], key: u128) -> u64 {
        let n = count(p);
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if int_key(p, mid) <= key { lo = mid + 1 } else { hi = mid }
        }
        if lo == 0 { child0(p) } else { int_child(p, lo - 1) }
    }

    /// The most pages any single descent or leaf walk may touch.
    ///
    /// A tree's structure is pointers stored in its own pages, so damage makes them
    /// point anywhere — including back at a page already visited. `range` and
    /// `iter_all` followed `next_leaf` with no bound at all, so one corrupted
    /// pointer forming a cycle looped forever, collecting into a `Vec` until memory
    /// ran out. A descent has the same exposure through `int_child`.
    ///
    /// No walk can legitimately visit more pages than the store contains, so that
    /// is the bound. It cannot cut a healthy walk short, and it turns an unbounded
    /// one into a truncated answer.
    fn walk_budget(&self) -> u64 { self.pages.page_count().saturating_add(1) }

    pub(crate) fn get(&self, key: u128) -> io::Result<Option<u64>> {
        let mut page = self.root;
        let mut budget = self.walk_budget();
        loop {
            budget = match budget.checked_sub(1) { Some(b) => b, None => return Ok(None) };
            let buf = self.page_for_read(page)?;
            if kind(&buf) == KIND_LEAF {
                let i = Self::lower_bound_leaf(&buf, key);
                if i < count(&buf) && leaf_key(&buf, i) == key {
                    return Ok(Some(leaf_val(&buf, i)));
                }
                return Ok(None);
            }
            page = Self::descend_to(&buf, key);
        }
    }

    pub(crate) fn insert(&mut self, key: u128, value: u64) -> io::Result<()> {
        let root = self.root;
        if let Some(split) = self.insert_into(root, key, value)? {
            // The root split: a new root above the old one keeps the tree balanced,
            // and is the only moment the tree gets taller.
            let new_root = self.pages.alloc()?;
            let mut buf = vec![0u8; self.pages.page_size()];
            buf[0] = KIND_INTERNAL;
            set_count(&mut buf, 1);
            set_child0(&mut buf, root);
            set_int(&mut buf, 0, split.key, split.right);
            self.pages.write(new_root, &buf)?;
            self.root = new_root;
        }
        self.note_meta();
        Ok(())
    }

    fn insert_into(&mut self, page: u64, key: u128, value: u64) -> io::Result<Option<Split>> {
        let mut buf = self.read_page(page)?;
        if kind(&buf) == KIND_LEAF {
            let n = count(&buf);
            let i = Self::lower_bound_leaf(&buf, key);
            if i < n && leaf_key(&buf, i) == key {
                set_leaf(&mut buf, i, key, value); // replace in place
                self.pages.write(page, &buf)?;
                return Ok(None);
            }
            if n < self.leaf_cap() {
                // Shift the tail right by one entry and drop the new one in.
                let from = HDR + i * ENTRY;
                let to = HDR + (i + 1) * ENTRY;
                let bytes = (n - i) * ENTRY;
                buf.copy_within(from..from + bytes, to);
                set_leaf(&mut buf, i, key, value);
                set_count(&mut buf, n + 1);
                self.pages.write(page, &buf)?;
                self.len += 1;
                return Ok(None);
            }
            return self.split_leaf(page, buf, key, value).map(Some);
        }

        let child = Self::descend_to(&buf, key);
        let Some(split) = self.insert_into(child, key, value)? else { return Ok(None) };

        // A child split; record its separator here, splitting in turn if full.
        let mut buf = self.read_page(page)?;
        let n = count(&buf);
        let mut at = 0usize;
        while at < n && int_key(&buf, at) < split.key { at += 1 }
        if n < self.int_cap() {
            let from = HDR + 8 + at * ENTRY;
            let to = HDR + 8 + (at + 1) * ENTRY;
            let bytes = (n - at) * ENTRY;
            buf.copy_within(from..from + bytes, to);
            set_int(&mut buf, at, split.key, split.right);
            set_count(&mut buf, n + 1);
            self.pages.write(page, &buf)?;
            return Ok(None);
        }
        self.split_internal(page, buf, at, split).map(Some)
    }

    fn split_leaf(&mut self, page: u64, mut buf: Vec<u8>, key: u128, value: u64)
        -> io::Result<Split>
    {
        let n = count(&buf);
        let mid = n / 2;
        let right = self.pages.alloc()?;
        let mut rbuf = vec![0u8; self.pages.page_size()];
        rbuf[0] = KIND_LEAF;
        let moved = n - mid;
        for j in 0..moved {
            set_leaf(&mut rbuf, j, leaf_key(&buf, mid + j), leaf_val(&buf, mid + j));
        }
        set_count(&mut rbuf, moved);
        set_next_leaf(&mut rbuf, next_leaf(&buf));
        set_count(&mut buf, mid);
        set_next_leaf(&mut buf, right);

        let sep = leaf_key(&rbuf, 0);
        self.pages.write(page, &buf)?;
        self.pages.write(right, &rbuf)?;
        // Place the new entry in whichever half now owns its range.
        if key < sep {
            self.insert_into(page, key, value)?;
        } else {
            self.insert_into(right, key, value)?;
        }
        Ok(Split { key: sep, right })
    }

    fn split_internal(&mut self, page: u64, mut buf: Vec<u8>, at: usize, pending: Split)
        -> io::Result<Split>
    {
        // Rebuild the full separator list including the pending one, then halve it.
        let n = count(&buf);
        let mut keys: Vec<(u128, u64)> = Vec::with_capacity(n + 1);
        for i in 0..n { keys.push((int_key(&buf, i), int_child(&buf, i))); }
        keys.insert(at, (pending.key, pending.right));

        let mid = keys.len() / 2;
        let sep = keys[mid].0;
        let right = self.pages.alloc()?;
        let mut rbuf = vec![0u8; self.pages.page_size()];
        rbuf[0] = KIND_INTERNAL;
        set_child0(&mut rbuf, keys[mid].1);
        for (j, &(k, c)) in keys[mid + 1..].iter().enumerate() {
            set_int(&mut rbuf, j, k, c);
        }
        set_count(&mut rbuf, keys.len() - mid - 1);

        for (i, &(k, c)) in keys[..mid].iter().enumerate() {
            set_int(&mut buf, i, k, c);
        }
        set_count(&mut buf, mid);

        self.pages.write(page, &buf)?;
        self.pages.write(right, &rbuf)?;
        Ok(Split { key: sep, right })
    }

    pub(crate) fn remove(&mut self, key: u128) -> io::Result<bool> {
        let mut page = self.root;
        let mut budget = self.walk_budget();
        loop {
            budget = match budget.checked_sub(1) { Some(b) => b, None => return Ok(false) };
            let mut buf = self.read_page(page)?;
            if kind(&buf) == KIND_LEAF {
                let n = count(&buf);
                let i = Self::lower_bound_leaf(&buf, key);
                if i >= n || leaf_key(&buf, i) != key {
                    return Ok(false);
                }
                let from = HDR + (i + 1) * ENTRY;
                let to = HDR + i * ENTRY;
                let bytes = (n - i - 1) * ENTRY;
                buf.copy_within(from..from + bytes, to);
                set_count(&mut buf, n - 1);
                self.pages.write(page, &buf)?;
                self.len -= 1;
                self.note_meta();
                return Ok(true);
            }
            page = Self::descend_to(&buf, key);
        }
    }

    /// Every entry whose key falls in `lo ..= hi`, in order.
    ///
    /// With a composite key this is a prefix scan: all members of one collection,
    /// or all edges from one node, are a contiguous run.
    pub(crate) fn range(&self, lo: u128, hi: u128) -> io::Result<Vec<(u128, u64)>> {
        let mut budget = self.walk_budget();
        let mut page = self.root;
        loop {
            budget = match budget.checked_sub(1) { Some(b) => b, None => return Ok(Vec::new()) };
            let buf = self.page_for_read(page)?;
            if kind(&buf) == KIND_LEAF { break }
            page = Self::descend_to(&buf, lo);
        }
        let mut out = Vec::new();
        let mut budget = self.walk_budget();
        while page != 0 {
            budget = match budget.checked_sub(1) { Some(b) => b, None => return Ok(out) };
            let buf = self.page_for_read(page)?;
            for i in 0..count(&buf) {
                let k = leaf_key(&buf, i);
                if k > hi { return Ok(out) }
                if k >= lo { out.push((k, leaf_val(&buf, i))) }
            }
            page = next_leaf(&buf);
        }
        Ok(out)
    }

    /// Every entry in key order, one leaf page at a time.
    ///
    /// This is the form a full scan should take. Collecting the tree into a `Vec`
    /// first costs 24 bytes per entry with nothing bounding it — 1.2 GB on a
    /// 48-million-entry index — which is exactly the "RAM proportional to the
    /// store" that Law 1 forbids. Here the resident cost is one page, whatever the
    /// index holds.
    ///
    /// `f` returning `false` stops the walk, so a search does not have to read
    /// what it no longer needs.
    pub(crate) fn for_each(&self, mut f: impl FnMut(u128, u64) -> bool) -> io::Result<()> {
        let mut budget = self.walk_budget();
        let mut page = self.root;
        loop {
            budget = match budget.checked_sub(1) { Some(b) => b, None => return Ok(()) };
            let buf = self.page_for_read(page)?;
            if kind(&buf) == KIND_LEAF { break }
            page = child0(&buf);
        }
        let mut budget = self.walk_budget();
        while page != 0 {
            budget = match budget.checked_sub(1) { Some(b) => b, None => return Ok(()) };
            let buf = self.page_for_read(page)?;
            for i in 0..count(&buf) {
                if !f(leaf_key(&buf, i), leaf_val(&buf, i)) { return Ok(()) }
            }
            page = next_leaf(&buf);
        }
        Ok(())
    }

    /// Every entry in key order, materialised.
    ///
    /// Convenience for tests and for indexes small enough that holding them is not
    /// a question. Anything that scans a real store should use [`for_each`], which
    /// is bounded by one page rather than by the number of entries.
    ///
    /// [`for_each`]: Self::for_each
    pub(crate) fn iter_all(&self) -> io::Result<Vec<(u128, u64)>> {
        let mut out = Vec::with_capacity(self.len as usize);
        self.for_each(|k, v| { out.push((k, v)); true })?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::pagestore::DEFAULT_PAGE_SIZE;

    fn tree(dir: &tempfile::TempDir) -> BTree {
        BTree::create(&dir.path().join("idx.bin"), DEFAULT_PAGE_SIZE).unwrap()
    }

    /// Keys arriving in the worst possible order for a sorted array — random — must
    /// still be found. This is the property a sorted array cannot offer without a
    /// rebuild, and the reason this structure exists.
    #[test]
    fn randomly_ordered_inserts_are_all_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut t = tree(&dir);
        // Deterministic scatter — no dependency on a random source.
        let keys: Vec<u128> = (0..5_000u64).map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15) as u128).collect();
        for (v, &k) in keys.iter().enumerate() {
            t.insert(k, v as u64).unwrap();
        }
        assert_eq!(t.len(), keys.len() as u64);
        for (v, &k) in keys.iter().enumerate() {
            assert_eq!(t.get(k).unwrap(), Some(v as u64), "key {k} lost");
        }
        assert_eq!(t.get(12345u128).unwrap(), None, "an absent key was found");
    }

    #[test]
    fn ascending_inserts_work_too() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut t = tree(&dir);
        for i in 0..5_000u64 { t.insert(i as u128, i * 2).unwrap(); }
        for i in 0..5_000u64 { assert_eq!(t.get(i as u128).unwrap(), Some(i * 2), "key {i}"); }
        let all = t.iter_all().unwrap();
        assert_eq!(all.len(), 5_000, "ordered walk lost entries");
        assert!(all.windows(2).all(|w| w[0].0 < w[1].0), "ordered walk is not ordered");
    }

    #[test]
    fn reinserting_a_key_replaces_its_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut t = tree(&dir);
        for i in 0..1_000u64 { t.insert(i as u128, i).unwrap(); }
        for i in 0..1_000u64 { t.insert(i as u128, i + 7_000).unwrap(); }
        assert_eq!(t.len(), 1_000, "replacing a key changed the entry count");
        for i in 0..1_000u64 { assert_eq!(t.get(i as u128).unwrap(), Some(i + 7_000)); }
    }

    #[test]
    fn removed_keys_are_gone_and_the_rest_survive() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut t = tree(&dir);
        for i in 0..2_000u64 { t.insert(i as u128, i).unwrap(); }
        for i in (0..2_000u64).step_by(2) { assert!(t.remove(i as u128).unwrap()); }
        assert!(!t.remove(0u128).unwrap(), "removing twice reported success");
        assert_eq!(t.len(), 1_000);
        for i in 0..2_000u64 {
            let expect = if i % 2 == 0 { None } else { Some(i) };
            assert_eq!(t.get(i as u128).unwrap(), expect, "key {i}");
        }
    }

    /// The cost that matters: an insert must touch a handful of pages, not the
    /// whole index. A sorted array would have to rewrite everything after the
    /// insertion point.
    #[test]
    fn the_tree_stays_shallow() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut t = tree(&dir);
        for i in 0..50_000u64 {
            t.insert(i.wrapping_mul(0x9E37_79B9_7F4A_7C15) as u128, i).unwrap();
        }
        // A 4 KB page holds (4096-16)/24 = 170 entries. Splitting down the middle
        // leaves each page around half full under random keys, so 50k entries want
        // roughly 600 leaves plus a thin spine above them. The bound below is
        // deliberately loose: what it catches is pages that are not filling at all.
        assert!(t.page_count() < 50_000 / 50,
                "{} pages for 50k entries suggests pages are not filling", t.page_count());
        println!("  50k entries → {} pages ({:.0} entries per page)",
                 t.page_count(), 50_000.0 / t.page_count() as f64);
    }

    #[test]
    fn a_tree_survives_a_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("idx.bin");
        {
            let mut t = BTree::create(&path, DEFAULT_PAGE_SIZE).unwrap();
            for i in 0..3_000u64 { t.insert(i.wrapping_mul(2_654_435_761) as u128, i).unwrap(); }
            t.sync().unwrap();
        }
        let t = BTree::open(&path).unwrap().expect("tree should reopen");
        assert_eq!(t.len(), 3_000, "entry count did not survive");
        for i in 0..3_000u64 {
            assert_eq!(t.get(i.wrapping_mul(2_654_435_761) as u128).unwrap(), Some(i), "key {i}");
        }
    }

    /// `range` against a `BTreeMap` that cannot be wrong, over every awkward
    /// boundary: a window that starts before the first key, one that ends after the
    /// last, one that lands between two keys, and empty ones.
    #[test]
    fn range_matches_a_btreemap_oracle() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut t = tree(&dir);
        let mut oracle = std::collections::BTreeMap::new();
        for i in 0..20_000u64 {
            let k = (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) % 100_000) as u128;
            t.insert(k, i).unwrap();
            oracle.insert(k, i);
        }
        for i in (0..20_000u64).step_by(7) {
            let k = (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) % 100_000) as u128;
            t.remove(k).unwrap();
            oracle.remove(&k);
        }
        let windows: &[(u128, u128)] = &[
            (0, u128::MAX), (0, 0), (u128::MAX, u128::MAX), (5, 4),
            (0, 500), (99_500, 200_000), (40_000, 40_100), (12_345, 12_345),
        ];
        for &(lo, hi) in windows {
            let got = t.range(lo, hi).unwrap();
            let want: Vec<(u128, u64)> = if lo > hi {
                Vec::new()
            } else {
                oracle.range(lo..=hi).map(|(k, v)| (*k, *v)).collect()
            };
            assert_eq!(got, want, "range({lo}, {hi}) disagreed with the oracle");
        }
    }

    /// The shape the topology needs: a composite key `(owner, member)` turns "every
    /// member of one owner" into one contiguous run, so a collection's rows or a
    /// node's edges are a scan rather than a separate structure.
    #[test]
    fn a_composite_key_makes_a_prefix_scan() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut t = tree(&dir);
        let compose = |owner: u64, member: u64| ((owner as u128) << 64) | member as u128;

        for owner in 0..50u64 {
            for member in 0..200u64 {
                t.insert(compose(owner, member), owner * 1_000 + member).unwrap();
            }
        }
        for owner in 0..50u64 {
            let got = t.range(compose(owner, 0), compose(owner, u64::MAX)).unwrap();
            assert_eq!(got.len(), 200, "owner {owner} scanned {} members", got.len());
            for (i, (k, v)) in got.iter().enumerate() {
                assert_eq!(*k, compose(owner, i as u64), "owner {owner} member {i} out of order");
                assert_eq!(*v, owner * 1_000 + i as u64);
            }
        }
        // Removing one owner entirely must not disturb its neighbours, which is the
        // case that matters when a collection is dropped.
        for member in 0..200u64 { t.remove(compose(7, member)).unwrap(); }
        assert!(t.range(compose(7, 0), compose(7, u64::MAX)).unwrap().is_empty());
        assert_eq!(t.range(compose(6, 0), compose(6, u64::MAX)).unwrap().len(), 200);
        assert_eq!(t.range(compose(8, 0), compose(8, u64::MAX)).unwrap().len(), 200);
    }

    /// A damaged tree page must not read outside itself.
    ///
    /// Every index into a page is driven by a count read from that page, and a page
    /// can hold anything after a torn write. This walks a real tree, corrupts one
    /// page at a time in the ways that matter, and requires every operation to
    /// return *something* — right, wrong, or absent — rather than panic.
    #[test]
    fn a_damaged_page_never_reads_outside_itself() {
        use std::io::{Seek, SeekFrom, Write};
        let corruptions: &[(&str, usize, Vec<u8>)] = &[
            ("count says 65535",        2, u16::MAX.to_le_bytes().to_vec()),
            ("count says 60000",        2, 60_000u16.to_le_bytes().to_vec()),
            ("kind byte is garbage",    0, vec![0xFF]),
            ("next-leaf points far away", 8, u64::MAX.to_le_bytes().to_vec()),
            ("header all ones",         0, vec![0xFF; 16]),
        ];
        for (name, offset, bytes) in corruptions {
            for target_page in [1u64, 2, 3] {
                let dir = tempfile::TempDir::new().unwrap();
                let path = dir.path().join("t.bin");
                {
                    let mut t = BTree::create(&path, DEFAULT_PAGE_SIZE).unwrap();
                    for i in 0..4_000u64 {
                        t.insert(i.wrapping_mul(0x9E37_79B9_7F4A_7C15) as u128, i).unwrap();
                    }
                    t.sync().unwrap();
                }
                if target_page * DEFAULT_PAGE_SIZE as u64
                    >= std::fs::metadata(&path).unwrap().len() { continue }
                {
                    let mut fh = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                    fh.seek(SeekFrom::Start(
                        target_page * DEFAULT_PAGE_SIZE as u64 + *offset as u64)).unwrap();
                    fh.write_all(bytes).unwrap();
                }
                let Some(t) = BTree::open(&path).unwrap() else { continue };
                // Lookups, a range and a full walk: none may panic.
                for i in 0..500u64 {
                    let _ = t.get(i.wrapping_mul(0x9E37_79B9_7F4A_7C15) as u128);
                }
                let _ = t.range(0, u128::MAX);
                let _ = t.iter_all();
                let mut n = 0usize;
                let _ = t.for_each(|_, _| { n += 1; n < 100_000 });
                assert!(n <= 100_000, "{name} on page {target_page}: the walk did not terminate");
            }
        }
    }
}
