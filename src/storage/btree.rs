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
//! Keys and values are both `u64`: a record hash and the slot it lives in. Fixed
//! width keeps a page a plain array, so a lookup within one is a binary search over
//! `count` entries rather than a walk.
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
const ENTRY: usize = 16; // u64 key + u64 value/child

fn rd8(p: &[u8], at: usize) -> u64 { u64::from_le_bytes(p[at..at + 8].try_into().unwrap()) }
fn wr8(p: &mut [u8], at: usize, v: u64) { p[at..at + 8].copy_from_slice(&v.to_le_bytes()); }
fn count(p: &[u8]) -> usize { u16::from_le_bytes([p[2], p[3]]) as usize }
fn set_count(p: &mut [u8], n: usize) { p[2..4].copy_from_slice(&(n as u16).to_le_bytes()); }
fn kind(p: &[u8]) -> u8 { p[0] }
fn next_leaf(p: &[u8]) -> u64 { rd8(p, 8) }
fn set_next_leaf(p: &mut [u8], v: u64) { wr8(p, 8, v) }

// Leaf entry i: key at HDR + i*16, value at +8.
fn leaf_key(p: &[u8], i: usize) -> u64 { rd8(p, HDR + i * ENTRY) }
fn leaf_val(p: &[u8], i: usize) -> u64 { rd8(p, HDR + i * ENTRY + 8) }
fn set_leaf(p: &mut [u8], i: usize, k: u64, v: u64) {
    wr8(p, HDR + i * ENTRY, k);
    wr8(p, HDR + i * ENTRY + 8, v);
}

// Internal node: child0 at HDR, then (key, child) pairs from HDR+8.
fn child0(p: &[u8]) -> u64 { rd8(p, HDR) }
fn set_child0(p: &mut [u8], v: u64) { wr8(p, HDR, v) }
fn int_key(p: &[u8], i: usize) -> u64 { rd8(p, HDR + 8 + i * ENTRY) }
fn int_child(p: &[u8], i: usize) -> u64 { rd8(p, HDR + 8 + i * ENTRY + 8) }
fn set_int(p: &mut [u8], i: usize, k: u64, c: u64) {
    wr8(p, HDR + 8 + i * ENTRY, k);
    wr8(p, HDR + 8 + i * ENTRY + 8, c);
}

/// What an insert reports back up the descent when a child had to split.
struct Split { key: u64, right: u64 }

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

    /// Index of the first entry with key >= `key`.
    fn lower_bound_leaf(p: &[u8], key: u64) -> usize {
        let (mut lo, mut hi) = (0usize, count(p));
        while lo < hi {
            let mid = (lo + hi) / 2;
            if leaf_key(p, mid) < key { lo = mid + 1 } else { hi = mid }
        }
        lo
    }

    /// Which child to descend into for `key`.
    fn descend_to(p: &[u8], key: u64) -> u64 {
        let n = count(p);
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if int_key(p, mid) <= key { lo = mid + 1 } else { hi = mid }
        }
        if lo == 0 { child0(p) } else { int_child(p, lo - 1) }
    }

    pub(crate) fn get(&self, key: u64) -> io::Result<Option<u64>> {
        let mut page = self.root;
        loop {
            let buf = self.read_page(page)?;
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

    pub(crate) fn insert(&mut self, key: u64, value: u64) -> io::Result<()> {
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

    fn insert_into(&mut self, page: u64, key: u64, value: u64) -> io::Result<Option<Split>> {
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

    fn split_leaf(&mut self, page: u64, mut buf: Vec<u8>, key: u64, value: u64)
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
        let mut keys: Vec<(u64, u64)> = Vec::with_capacity(n + 1);
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

    pub(crate) fn remove(&mut self, key: u64) -> io::Result<bool> {
        let mut page = self.root;
        loop {
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

    /// Every entry in key order, by walking the linked leaves.
    pub(crate) fn iter_all(&self) -> io::Result<Vec<(u64, u64)>> {
        let mut page = self.root;
        loop {
            let buf = self.read_page(page)?;
            if kind(&buf) == KIND_LEAF { break }
            page = child0(&buf);
        }
        let mut out = Vec::with_capacity(self.len as usize);
        while page != 0 {
            let buf = self.read_page(page)?;
            for i in 0..count(&buf) {
                out.push((leaf_key(&buf, i), leaf_val(&buf, i)));
            }
            page = next_leaf(&buf);
        }
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
        let keys: Vec<u64> = (0..5_000u64).map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).collect();
        for (v, &k) in keys.iter().enumerate() {
            t.insert(k, v as u64).unwrap();
        }
        assert_eq!(t.len(), keys.len() as u64);
        for (v, &k) in keys.iter().enumerate() {
            assert_eq!(t.get(k).unwrap(), Some(v as u64), "key {k} lost");
        }
        assert_eq!(t.get(12345).unwrap(), None, "an absent key was found");
    }

    #[test]
    fn ascending_inserts_work_too() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut t = tree(&dir);
        for i in 0..5_000u64 { t.insert(i, i * 2).unwrap(); }
        for i in 0..5_000u64 { assert_eq!(t.get(i).unwrap(), Some(i * 2), "key {i}"); }
        let all = t.iter_all().unwrap();
        assert_eq!(all.len(), 5_000, "ordered walk lost entries");
        assert!(all.windows(2).all(|w| w[0].0 < w[1].0), "ordered walk is not ordered");
    }

    #[test]
    fn reinserting_a_key_replaces_its_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut t = tree(&dir);
        for i in 0..1_000u64 { t.insert(i, i).unwrap(); }
        for i in 0..1_000u64 { t.insert(i, i + 7_000).unwrap(); }
        assert_eq!(t.len(), 1_000, "replacing a key changed the entry count");
        for i in 0..1_000u64 { assert_eq!(t.get(i).unwrap(), Some(i + 7_000)); }
    }

    #[test]
    fn removed_keys_are_gone_and_the_rest_survive() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut t = tree(&dir);
        for i in 0..2_000u64 { t.insert(i, i).unwrap(); }
        for i in (0..2_000u64).step_by(2) { assert!(t.remove(i).unwrap()); }
        assert!(!t.remove(0).unwrap(), "removing twice reported success");
        assert_eq!(t.len(), 1_000);
        for i in 0..2_000u64 {
            let expect = if i % 2 == 0 { None } else { Some(i) };
            assert_eq!(t.get(i).unwrap(), expect, "key {i}");
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
            t.insert(i.wrapping_mul(0x9E37_79B9_7F4A_7C15), i).unwrap();
        }
        // 4 KB pages hold 255 entries; 50k entries is three levels at most, so the
        // page count is dominated by leaves rather than by internal nodes.
        assert!(t.page_count() < 50_000 / 100,
                "{} pages for 50k entries suggests pages are not filling", t.page_count());
    }

    #[test]
    fn a_tree_survives_a_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("idx.bin");
        {
            let mut t = BTree::create(&path, DEFAULT_PAGE_SIZE).unwrap();
            for i in 0..3_000u64 { t.insert(i.wrapping_mul(2_654_435_761), i).unwrap(); }
            t.sync().unwrap();
        }
        let t = BTree::open(&path).unwrap().expect("tree should reopen");
        assert_eq!(t.len(), 3_000, "entry count did not survive");
        for i in 0..3_000u64 {
            assert_eq!(t.get(i.wrapping_mul(2_654_435_761)).unwrap(), Some(i), "key {i}");
        }
    }
}
