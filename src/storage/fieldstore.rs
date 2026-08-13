//! # Scalar (btree) field index — fast `WHERE x = / < / BETWEEN` and `ORDER BY`
//!
//! When you filter or sort on a column, scanning every row is slow. A btree index
//! keeps that column's values in *sorted* order mapping value → the ids of rows
//! that have it, so an equality or range lookup is a quick search and `ORDER BY`
//! is just reading the tree in order. This file is the on-disk, memory-mapped form
//! of that index: one file per `(collection, field)`, with the bulk posting lists
//! (linear in the row count) living in the mmap — reclaimable OS page cache —
//! instead of a heap `BTreeMap`. So a reopened paged database answers indexed
//! queries with bounded RAM no matter how big the table is.
//!
//! One file per `(collection, field)`: `fieldidx_<coll_hash>_<field>.bin`. The
//! posting lists (the linear-in-N bulk) live in the mmap — reclaimable page
//! cache — instead of the heap `BTreeMap`, so a reopened paged DB serves indexed
//! queries with bounded RAM regardless of dataset size.
//!
//! ## File format
//! ```text
//! [0..8)    magic  "SKFIDX\0\0"
//! [8..16)   nkeys  u64
//! [16..16+nkeys*24)  key directory (24 B/entry), sorted by key ascending:
//!     key_off  u64   absolute offset of the encoded key
//!     key_len  u32   encoded-key length
//!     post_off u64   absolute offset of the posting run (8-aligned)
//!     post_cnt u32   number of u64 postings
//! keys blob:      FieldKey::encode() bytes, back to back
//! postings blob:  raw little-endian u64 node hashes (sorted), 8-aligned start
//! ```
//! Keys are recovered by decode-and-compare during binary search, so the byte
//! layout need not be order-preserving.
#![allow(dead_code)] // wired into compact()/open()/query in a follow-up edit

use std::io::{self, Write};
use std::ops::Bound;
use std::path::Path;

use crate::FieldKey;

const MAGIC: [u8; 8] = *b"SKFIDX\0\0";
const HEADER_LEN: usize = 16; // magic(8) + nkeys(8)
const DIR_ENTRY: usize = 24; // key_off u64, key_len u32, post_off u64, post_cnt u32

#[inline]
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
#[inline]
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// mmap-or-owned backing (mirrors the private `Backing` in `topology.rs`).
///
/// `Clone`-able so a read snapshot can share this field index: the mmap is shared
/// (an `Arc` bump via [`MmapView`]) and the retained fd is shared via `Arc<File>`
/// (kept only to hold the file open; the mapping itself outlives it on unix).
#[derive(Clone)]
enum Backing {
    #[cfg(unix)]
    Map {
        _file: std::sync::Arc<std::fs::File>,
        map: super::mmap::MmapView,
    },
    Owned(Vec<u8>),
}

impl Backing {
    fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let file = std::fs::File::open(path)?;
            let len = file.metadata()?.len() as usize;
            if let Some(map) = super::mmap::MmapView::try_new(&file, len) {
                return Ok(Backing::Map { _file: std::sync::Arc::new(file), map });
            }
        }
        Ok(Backing::Owned(std::fs::read(path)?))
    }

    fn bytes(&self) -> &[u8] {
        match self {
            #[cfg(unix)]
            Backing::Map { map, .. } => map.slice(0, map.len()).unwrap_or(&[]),
            Backing::Owned(v) => v.as_slice(),
        }
    }
}

/// Serialize a heap btree (`FieldKey -> sorted node hashes`) to the on-disk
/// format at `path`, via a temp file + atomic rename.
pub(crate) fn write(
    path: &Path,
    btree: &std::collections::BTreeMap<FieldKey, Vec<u64>>,
) -> io::Result<()> {
    let nkeys = btree.len();
    let dir_start = HEADER_LEN;
    let keys_start = dir_start + nkeys * DIR_ENTRY;

    // Pass 1: build the keys blob and remember each key's absolute offset/len.
    let mut keys_blob: Vec<u8> = Vec::new();
    let mut key_locs: Vec<(u64, u32)> = Vec::with_capacity(nkeys);
    for k in btree.keys() {
        let off = keys_start as u64 + keys_blob.len() as u64;
        k.encode(&mut keys_blob);
        let len = (keys_start as u64 + keys_blob.len() as u64 - off) as u32;
        key_locs.push((off, len));
    }

    // Postings blob starts 8-aligned so raw u64 reads never straddle awkwardly.
    let mut post_start = keys_start + keys_blob.len();
    post_start += (8 - (post_start % 8)) % 8;
    let mut post_blob: Vec<u8> = Vec::new();
    let mut post_locs: Vec<(u64, u32)> = Vec::with_capacity(nkeys);
    for ids in btree.values() {
        // Each run begins at a multiple of 8 (blob start is aligned, runs are 8*n).
        let off = post_start as u64 + post_blob.len() as u64;
        for &id in ids {
            post_blob.extend_from_slice(&id.to_le_bytes());
        }
        post_locs.push((off, ids.len() as u32));
    }

    let mut dir: Vec<u8> = Vec::with_capacity(nkeys * DIR_ENTRY);
    for i in 0..nkeys {
        let (koff, klen) = key_locs[i];
        let (poff, pcnt) = post_locs[i];
        dir.extend_from_slice(&koff.to_le_bytes());
        dir.extend_from_slice(&klen.to_le_bytes());
        dir.extend_from_slice(&poff.to_le_bytes());
        dir.extend_from_slice(&pcnt.to_le_bytes());
    }

    let tmp = path.with_extension("bin.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&MAGIC)?;
        f.write_all(&(nkeys as u64).to_le_bytes())?;
        f.write_all(&dir)?;
        f.write_all(&keys_blob)?;
        // pad up to the aligned postings start
        let pad = post_start - (keys_start + keys_blob.len());
        if pad > 0 {
            f.write_all(&vec![0u8; pad])?;
        }
        f.write_all(&post_blob)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// A memory-mapped btree field index. Lookups decode postings into owned `Vec`s
/// (transient — dropped after use); the retained bytes are the reclaimable mmap.
#[derive(Clone)]
pub(crate) struct MappedFieldStore {
    backing: Backing,
    nkeys: usize,
}

impl MappedFieldStore {
    /// Open `path` if it exists and has a valid header; `Ok(None)` otherwise.
    pub(crate) fn open_disk(path: &Path) -> io::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let backing = Backing::open(path)?;
        let b = backing.bytes();
        if b.len() < HEADER_LEN || b[0..8] != MAGIC {
            return Ok(None);
        }
        let nkeys = rd_u64(b, 8) as usize;
        Ok(Some(Self { backing, nkeys }))
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.nkeys
    }

    fn key_at(&self, i: usize) -> FieldKey {
        let b = self.backing.bytes();
        let e = HEADER_LEN + i * DIR_ENTRY;
        let koff = rd_u64(b, e) as usize;
        let klen = rd_u32(b, e + 8) as usize;
        FieldKey::decode(&b[koff..koff + klen])
    }

    fn postings_at(&self, i: usize) -> Vec<u64> {
        let b = self.backing.bytes();
        let e = HEADER_LEN + i * DIR_ENTRY;
        let poff = rd_u64(b, e + 12) as usize;
        let cnt = rd_u32(b, e + 20) as usize;
        (0..cnt).map(|j| rd_u64(b, poff + j * 8)).collect()
    }

    /// Binary search for `target`; `Ok(i)` exact, `Err(i)` insertion point.
    fn search(&self, target: &FieldKey) -> Result<usize, usize> {
        let (mut lo, mut hi) = (0usize, self.nkeys);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.key_at(mid).cmp(target) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(lo)
    }

    /// Postings for an exact key match.
    pub(crate) fn get_eq(&self, k: &FieldKey) -> Option<Vec<u64>> {
        self.search(k).ok().map(|i| self.postings_at(i))
    }

    /// The `[start, end)` index window covered by `(lo, hi)` bounds.
    fn window(&self, lo: Bound<&FieldKey>, hi: Bound<&FieldKey>) -> (usize, usize) {
        let start = match lo {
            Bound::Unbounded => 0,
            Bound::Included(k) => match self.search(k) {
                Ok(i) | Err(i) => i,
            },
            Bound::Excluded(k) => match self.search(k) {
                Ok(i) => i + 1,
                Err(i) => i,
            },
        };
        let end = match hi {
            Bound::Unbounded => self.nkeys,
            Bound::Included(k) => match self.search(k) {
                Ok(i) => i + 1,
                Err(i) => i,
            },
            Bound::Excluded(k) => match self.search(k) {
                Ok(i) | Err(i) => i,
            },
        };
        (start, end.max(start))
    }

    /// Concatenated postings for all keys in `(lo, hi)`.
    pub(crate) fn range_postings(&self, lo: Bound<&FieldKey>, hi: Bound<&FieldKey>) -> Vec<u64> {
        let (start, end) = self.window(lo, hi);
        let mut out = Vec::new();
        for i in start..end {
            out.extend(self.postings_at(i));
        }
        out
    }

    /// All `(key, postings)` pairs in ascending key order (for GROUP BY / DISTINCT
    /// / ORDER BY index scans). `rev` walks descending.
    pub(crate) fn iter_kv(&self, rev: bool) -> Vec<(FieldKey, Vec<u64>)> {
        let idxs: Vec<usize> = if rev {
            (0..self.nkeys).rev().collect()
        } else {
            (0..self.nkeys).collect()
        };
        idxs.into_iter()
            .map(|i| (self.key_at(i), self.postings_at(i)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldKey;
    use std::collections::BTreeMap;

    fn num(f: f64) -> FieldKey {
        FieldKey::from_f64(f)
    }
    fn s(x: &str) -> FieldKey {
        FieldKey::Str(x.to_string())
    }

    /// Round-trip a key through encode/decode.
    fn rt(k: FieldKey) -> FieldKey {
        let mut buf = Vec::new();
        k.encode(&mut buf);
        FieldKey::decode(&buf)
    }

    #[test]
    fn codec_roundtrip_all_variants() {
        for k in [
            FieldKey::Null,
            FieldKey::Bool(true),
            FieldKey::Bool(false),
            num(0.0),
            num(-1.5),
            num(3.14159265358979),
            num(f64::MAX),
            num(f64::MIN),
            num(1e300),
            num(-0.0),
            s(""),
            s("hits"),
            s("a string with spaces and 日本語 🚀"),
            s(&"x".repeat(5000)),
        ] {
            assert_eq!(rt(k.clone()), k, "roundtrip failed for {k:?}");
        }
    }

    #[test]
    fn codec_ordering_across_types() {
        // Null < Bool < Number < Str, preserved through encode/decode.
        let ordered = [
            FieldKey::Null,
            FieldKey::Bool(false),
            FieldKey::Bool(true),
            num(-100.0),
            num(0.0),
            num(100.0),
            s("a"),
            s("b"),
        ];
        for w in ordered.windows(2) {
            assert!(rt(w[0].clone()) < rt(w[1].clone()), "{:?} !< {:?}", w[0], w[1]);
        }
    }

    fn build(entries: &[(FieldKey, Vec<u64>)]) -> (tempfile::TempDir, MappedFieldStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fieldidx_test.bin");
        let mut bt: BTreeMap<FieldKey, Vec<u64>> = BTreeMap::new();
        for (k, v) in entries {
            bt.insert(k.clone(), v.clone());
        }
        write(&path, &bt).unwrap();
        let store = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        (dir, store)
    }

    #[test]
    fn empty_index() {
        let (_d, store) = build(&[]);
        assert_eq!(store.len(), 0);
        assert_eq!(store.get_eq(&num(1.0)), None);
        assert!(store.range_postings(Bound::Unbounded, Bound::Unbounded).is_empty());
        assert!(store.iter_kv(false).is_empty());
    }

    #[test]
    fn single_key() {
        let (_d, store) = build(&[(num(42.0), vec![7, 8, 9])]);
        assert_eq!(store.len(), 1);
        assert_eq!(store.get_eq(&num(42.0)), Some(vec![7, 8, 9]));
        assert_eq!(store.get_eq(&num(41.0)), None);
        assert_eq!(store.get_eq(&num(43.0)), None);
    }

    #[test]
    fn get_eq_hits_and_misses() {
        let (_d, store) = build(&[
            (num(1.0), vec![10]),
            (num(5.0), vec![20, 21]),
            (num(9.0), vec![30, 31, 32]),
            (s("zebra"), vec![99]),
        ]);
        assert_eq!(store.get_eq(&num(1.0)), Some(vec![10]));
        assert_eq!(store.get_eq(&num(5.0)), Some(vec![20, 21]));
        assert_eq!(store.get_eq(&num(9.0)), Some(vec![30, 31, 32]));
        assert_eq!(store.get_eq(&s("zebra")), Some(vec![99]));
        // misses
        assert_eq!(store.get_eq(&num(2.0)), None);
        assert_eq!(store.get_eq(&num(100.0)), None);
        assert_eq!(store.get_eq(&s("aardvark")), None);
        assert_eq!(store.get_eq(&FieldKey::Null), None);
    }

    /// The authoritative oracle: range_postings must equal the same window over
    /// the source BTreeMap. Exhaustively checks every bound combination.
    #[test]
    fn range_matches_btreemap_oracle() {
        let entries: Vec<(FieldKey, Vec<u64>)> = (0..40)
            .map(|i| (num(i as f64), vec![i as u64 * 1000, i as u64 * 1000 + 1]))
            .collect();
        let mut bt: BTreeMap<FieldKey, Vec<u64>> = BTreeMap::new();
        for (k, v) in &entries {
            bt.insert(k.clone(), v.clone());
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fieldidx_range.bin");
        write(&path, &bt).unwrap();
        let store = MappedFieldStore::open_disk(&path).unwrap().unwrap();

        let probes = [-1.0, 0.0, 0.5, 5.0, 20.0, 39.0, 39.5, 100.0];
        let check = |lb: Bound<&FieldKey>, hb: Bound<&FieldKey>, ctx: &str| {
            let expected: Vec<u64> = bt
                .range((lb, hb))
                .flat_map(|(_, v)| v.iter().copied())
                .collect();
            let got = store.range_postings(lb, hb);
            assert_eq!(got, expected, "mismatch {ctx}");
        };
        for &v in &probes {
            let k = num(v);
            // Single-sided ranges are always valid for BTreeMap regardless of value.
            check(Bound::Excluded(&k), Bound::Unbounded, &format!("> {v}"));
            check(Bound::Included(&k), Bound::Unbounded, &format!(">= {v}"));
            check(Bound::Unbounded, Bound::Excluded(&k), &format!("< {v}"));
            check(Bound::Unbounded, Bound::Included(&k), &format!("<= {v}"));
        }
        check(Bound::Unbounded, Bound::Unbounded, "full");
        // Two-sided ranges only when lo < hi (BTreeMap panics on start > end;
        // range_postings tolerates it, but the executor never emits such ranges).
        for &lo in &probes {
            for &hi in &probes {
                if lo >= hi {
                    continue;
                }
                let (lk, hk) = (num(lo), num(hi));
                check(Bound::Included(&lk), Bound::Excluded(&hk), &format!("[{lo},{hi})"));
                check(Bound::Excluded(&lk), Bound::Included(&hk), &format!("({lo},{hi}]"));
                check(Bound::Included(&lk), Bound::Included(&hk), &format!("[{lo},{hi}]"));
                check(Bound::Excluded(&lk), Bound::Excluded(&hk), &format!("({lo},{hi})"));
            }
        }
        // Degenerate lo>hi against the store directly (oracle would panic): empty.
        let (a, b) = (num(30.0), num(5.0));
        assert!(store.range_postings(Bound::Included(&a), Bound::Excluded(&b)).is_empty());
    }

    #[test]
    fn iter_kv_order_and_rev() {
        let (_d, store) = build(&[
            (num(3.0), vec![3]),
            (num(1.0), vec![1]),
            (num(2.0), vec![2]),
        ]);
        let fwd = store.iter_kv(false);
        assert_eq!(fwd.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(), vec![num(1.0), num(2.0), num(3.0)]);
        let rev = store.iter_kv(true);
        assert_eq!(rev.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(), vec![num(3.0), num(2.0), num(1.0)]);
    }

    #[test]
    fn mixed_type_ordering_on_disk() {
        let (_d, store) = build(&[
            (s("b"), vec![5]),
            (num(2.0), vec![3]),
            (FieldKey::Bool(true), vec![2]),
            (FieldKey::Null, vec![1]),
            (s("a"), vec![4]),
        ]);
        // On-disk order follows FieldKey Ord: Null < Bool < Number < Str.
        let keys: Vec<FieldKey> = store.iter_kv(false).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![FieldKey::Null, FieldKey::Bool(true), num(2.0), s("a"), s("b")]);
    }

    /// Varying key lengths make keys_blob a non-multiple of 8, stressing the
    /// postings-blob alignment padding. Large u64s exercise full-width reads.
    #[test]
    fn alignment_and_large_u64() {
        let entries: Vec<(FieldKey, Vec<u64>)> = vec![
            (s("x"), vec![u64::MAX, 0, 1]),
            (s("yy"), vec![u64::MAX - 1]),
            (s("zzz"), vec![1 << 63, (1 << 63) + 7]),
            (s("wwww"), vec![12345678901234567, 9]),
            (num(1.0), vec![u64::MAX / 2]),
        ];
        let (_d, store) = build(&entries);
        assert_eq!(store.get_eq(&s("x")), Some(vec![u64::MAX, 0, 1]));
        assert_eq!(store.get_eq(&s("zzz")), Some(vec![1 << 63, (1 << 63) + 7]));
        assert_eq!(store.get_eq(&num(1.0)), Some(vec![u64::MAX / 2]));
    }

    #[test]
    fn large_posting_lists() {
        let big: Vec<u64> = (0..50_000u64).map(|i| i.wrapping_mul(2654435761)).collect();
        let (_d, store) = build(&[(num(1.0), big.clone()), (num(2.0), vec![42])]);
        assert_eq!(store.get_eq(&num(1.0)), Some(big));
        assert_eq!(store.get_eq(&num(2.0)), Some(vec![42]));
    }

    #[test]
    fn persistence_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fieldidx_persist.bin");
        let mut bt: BTreeMap<FieldKey, Vec<u64>> = BTreeMap::new();
        bt.insert(num(1.0), vec![1, 2, 3]);
        bt.insert(s("hi"), vec![9]);
        write(&path, &bt).unwrap();
        drop(bt);
        // Fresh open, no in-memory state.
        let store = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        assert_eq!(store.get_eq(&num(1.0)), Some(vec![1, 2, 3]));
        assert_eq!(store.get_eq(&s("hi")), Some(vec![9]));
    }

    #[test]
    fn missing_and_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        // missing → Ok(None)
        let missing = dir.path().join("nope.bin");
        assert!(MappedFieldStore::open_disk(&missing).unwrap().is_none());
        // corrupt/short → Ok(None)
        let bad = dir.path().join("bad.bin");
        std::fs::write(&bad, b"not a real header").unwrap();
        assert!(MappedFieldStore::open_disk(&bad).unwrap().is_none());
        // empty file → Ok(None)
        let empty = dir.path().join("empty.bin");
        std::fs::write(&empty, b"").unwrap();
        assert!(MappedFieldStore::open_disk(&empty).unwrap().is_none());
    }
}
