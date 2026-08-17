//! Records plus an index, both on pages — a store with no rebuild step.
//!
//! This is the three pieces put together:
//!
//! - [`RecordStore`] holds the bytes in slotted pages and returns space to a free
//!   list as records die
//! - [`BTree`] maps a key to the record holding it, and accepts an insert in place
//! - the record id is the indirection between them, so a record can move without
//!   anything that points at it moving too
//!
//! Every operation touches a bounded number of pages, so nothing accumulates and
//! there is nothing to fold back later. **There is no compaction here** — not a
//! faster one, none — which is the entire argument for the design.
//!
//! # What this is for
//!
//! The store today keeps records in one appended file and their index in a sorted
//! array. Neither can absorb a write in place: the file never reclaims space, and
//! the array cannot be inserted into at all. So writes accumulate in RAM and are
//! periodically folded back, and that fold rewrites everything — 7.6 s per million
//! records, roughly six minutes on a 48-million-record store, charged to whichever
//! ordinary write crosses the threshold.
//!
//! The measurement that matters is not how fast this is, but whether its cost
//! depends on how much data is already stored. See `writes_cost_the_same_at_every_size`.

use super::btree::BTree;
use super::recordstore::{RecordId, RecordStore};
use std::io;
use std::path::Path;

pub(crate) struct PagedStore {
    records: RecordStore,
    index: BTree,
}

impl PagedStore {
    /// Open or create a store under `dir`, using `records.bin` and `index.bin`.
    pub(crate) fn open(dir: &Path, page_size: usize) -> io::Result<Self> {
        Self::open_named(dir, "", page_size)
    }

    /// Open or create a store under `dir` whose files are `<name>.rec` and
    /// `<name>.idx`, so several independent stores can share a directory.
    pub(crate) fn open_named(dir: &Path, name: &str, page_size: usize) -> io::Result<Self> {
        let (rec, idx) = if name.is_empty() {
            ("records.bin".to_string(), "index.bin".to_string())
        } else {
            (format!("{name}.rec"), format!("{name}.idx"))
        };
        let rec_path = dir.join(rec);
        let idx_path = dir.join(idx);
        let records = match RecordStore::open(&rec_path)? {
            Some(r) => r,
            None => RecordStore::create(&rec_path, page_size)?,
        };
        let index = match BTree::open(&idx_path)? {
            Some(t) => t,
            None => BTree::create(&idx_path, page_size)?,
        };
        Ok(Self { records, index })
    }

    pub(crate) fn len(&self) -> u64 { self.index.len() }

    /// Pages held by the records and by the index.
    pub(crate) fn page_counts(&self) -> (u64, u64) {
        (self.records.page_count(), self.index.page_count())
    }

    /// Store `bytes` under `key`, replacing any previous version.
    ///
    /// The old version's space is returned *after* the new one is written, so a
    /// failure part-way leaves the previous record intact rather than neither.
    pub(crate) fn put(&mut self, key: u128, bytes: &[u8]) -> io::Result<()> {
        let previous = self.index.get(key)?;
        let id = self.records.insert(bytes)?;
        // If the index cannot record where the bytes went, the bytes are
        // unreachable — nothing points at them and nothing ever will. Leaving them
        // there leaks a record's worth of space on every such failure, so the
        // insert is undone before the error is returned. The previous version is
        // untouched either way, so the key keeps its old value rather than losing
        // both.
        if let Err(e) = self.index.insert(key, id.0) {
            let _ = self.records.delete(id);
            return Err(e);
        }
        if let Some(old) = previous {
            self.records.delete(RecordId(old))?;
        }
        Ok(())
    }

    pub(crate) fn get(&self, key: u128) -> io::Result<Option<Vec<u8>>> {
        match self.index.get(key)? {
            Some(id) => self.records.read(RecordId(id)),
            None => Ok(None),
        }
    }

    pub(crate) fn delete(&mut self, key: u128) -> io::Result<bool> {
        let Some(id) = self.index.get(key)? else { return Ok(false) };
        self.index.remove(key)?;
        self.records.delete(RecordId(id))?;
        Ok(true)
    }

    /// Every `(key, record id)` in key order, one index page at a time.
    ///
    /// Streams rather than collects: an index over a real store has too many
    /// entries to hold, and holding them is the RAM-proportional-to-the-store that
    /// Law 1 forbids. Passes ids rather than bytes so a scan that only needs keys
    /// does not read the records, which are the bulk. `f` returning `false` stops
    /// the walk.
    pub(crate) fn for_each_key(&self, f: impl FnMut(u128, u64) -> bool) -> io::Result<()> {
        self.index.for_each(f)
    }

    /// The record a key's index entry points at, given the id the index yielded.
    ///
    /// For a scan that already walked the index, this is the rest of the work:
    /// `get` would descend the tree again for a key the walk just handed over.
    pub(crate) fn read_at(&self, id: u64) -> io::Result<Option<Vec<u8>>> {
        self.records.read(RecordId(id))
    }

    pub(crate) fn sync(&mut self) -> io::Result<()> {
        self.records.sync()?;
        self.index.sync()
    }

    /// Spare header words in the *record* store — the index keeps its own root and
    /// length in the equivalent words of its file, so these are free.
    pub(crate) fn user_meta(&self) -> (u64, u64) { self.records.user_meta() }
    pub(crate) fn set_user_meta(&mut self, a: u64, b: u64) { self.records.set_user_meta(a, b) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::pagestore::DEFAULT_PAGE_SIZE;
    use std::time::Instant;

    fn store(dir: &tempfile::TempDir) -> PagedStore {
        PagedStore::open(dir.path(), DEFAULT_PAGE_SIZE).unwrap()
    }

    fn rec(i: u64) -> Vec<u8> {
        format!("{{\"_key\":\"n{i}\",\"name\":\"record {i} west java\",\"n\":{i}}}").into_bytes()
    }
    fn key(i: u64) -> u128 { i.wrapping_mul(0x9E37_79B9_7F4A_7C15) as u128 }

    #[test]
    fn records_round_trip_through_the_index() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        for i in 0..5_000 { s.put(key(i), &rec(i)).unwrap(); }
        assert_eq!(s.len(), 5_000);
        for i in 0..5_000 {
            assert_eq!(s.get(key(i)).unwrap().as_deref(), Some(rec(i).as_slice()), "key {i}");
        }
        assert_eq!(s.get(key(99_999)).unwrap(), None);
    }

    #[test]
    fn overwriting_replaces_and_reclaims() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        for i in 0..2_000 { s.put(key(i), &rec(i)).unwrap(); }
        let (rp, _) = s.page_counts();
        for round in 1..=10 {
            for i in 0..2_000 {
                s.put(key(i), &rec(i + round * 100_000)).unwrap();
            }
        }
        let (rp2, _) = s.page_counts();
        assert_eq!(s.len(), 2_000, "overwriting changed the entry count");
        assert!(rp2 <= rp * 2,
                "records grew from {rp} to {rp2} pages over ten full overwrites");
        for i in 0..2_000 {
            assert_eq!(s.get(key(i)).unwrap().as_deref(),
                       Some(rec(i + 10 * 100_000).as_slice()), "key {i}");
        }
    }

    #[test]
    fn deleting_removes_from_both_halves() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        for i in 0..1_000 { s.put(key(i), &rec(i)).unwrap(); }
        for i in (0..1_000).step_by(2) { assert!(s.delete(key(i)).unwrap()); }
        assert!(!s.delete(key(0)).unwrap(), "deleting twice reported success");
        assert_eq!(s.len(), 500);
        for i in 0..1_000 {
            let expect = if i % 2 == 0 { None } else { Some(rec(i)) };
            assert_eq!(s.get(key(i)).unwrap(), expect, "key {i}");
        }
    }

    #[test]
    fn a_store_survives_a_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        {
            let mut s = store(&dir);
            for i in 0..3_000 { s.put(key(i), &rec(i)).unwrap(); }
            s.delete(key(5)).unwrap();
            s.sync().unwrap();
        }
        let s = store(&dir);
        assert_eq!(s.len(), 2_999, "entry count did not survive");
        assert_eq!(s.get(key(7)).unwrap().as_deref(), Some(rec(7).as_slice()));
        assert_eq!(s.get(key(5)).unwrap(), None, "a deleted record came back");
    }

    /// **The measurement the whole direction exists for.**
    ///
    /// Absorbing a fixed batch of writes must cost the same whether the store
    /// already holds a little or a lot. Today it does not: the batch accumulates in
    /// RAM until a threshold, and the fold that follows rewrites everything, so the
    /// same 200 000 writes cost 1.5 s on a small store and six minutes on a
    /// 48-million-record one.
    ///
    /// Here there is no fold. The assertion is deliberately about the *shape* of
    /// the cost rather than its size — a constant factor is a tuning problem, but
    /// growth with the store is a violated principle.
    #[test]
    fn writes_cost_the_same_at_every_size() {
        let batch = 5_000u64;
        let mut timings = Vec::new();

        for &preload in &[10_000u64, 50_000, 200_000] {
            let dir = tempfile::TempDir::new().unwrap();
            let mut s = store(&dir);
            for i in 0..preload { s.put(key(i), &rec(i)).unwrap(); }

            let t = Instant::now();
            for i in preload..preload + batch { s.put(key(i), &rec(i)).unwrap(); }
            let per_write_us = t.elapsed().as_secs_f64() * 1e6 / batch as f64;
            timings.push((preload, per_write_us));
        }

        for (preload, us) in &timings {
            println!("  preloaded {preload:>7} records → {us:.2} us per write");
        }
        let smallest = timings.first().unwrap().1;
        let largest = timings.last().unwrap().1;
        assert!(
            largest < smallest * 3.0,
            "a write costs {largest:.2} us on a 200k-record store against \
             {smallest:.2} us on a 10k one — cost is tracking the size of the \
             store rather than the size of the change, which is the whole failure \
             this design exists to remove",
        );
    }
}
