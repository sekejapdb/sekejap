//! # The on-disk trigram index — making `ILIKE '%foo%'` fast without using RAM
//!
//! This file is the on-disk half of sekejap's GIN index, the structure that lets
//! a substring search like `name ILIKE '%vine%'` skip scanning every row.
//!
//! ## What a trigram index is (the clever foundational idea)
//!
//! A *trigram* is just three consecutive characters. Break `"vine"` into its
//! trigrams `vin`, `ine`. The key insight: a piece of text can only contain the
//! substring `"vine"` if it contains **all** of `"vine"`'s trigrams. So if, for
//! every trigram, we keep the set of documents that contain it (that set is
//! called a *postings list*), then answering `ILIKE '%vine%'` becomes:
//! "intersect the postings of `vin` and `ine`". That turns a full scan into a
//! few set intersections. (It can over-match — a doc might have both trigrams
//! without the contiguous word — so the caller re-checks survivors; see
//! `text_index/gin.rs` and the ILIKE path in `query.rs`.)
//!
//! Each postings list is stored as a [`RoaringBitmap`] — a compressed set of
//! integers that is small and fast to intersect. The integers are *slot* numbers
//! (0, 1, 2, …), and a separate **id map** turns a slot back into the real node
//! hash.
//!
//! ## Why "disk-first": mmap instead of loading into RAM
//!
//! The whole index for a field is written as one flat file (`gin.bin`) and
//! *memory-mapped* (see [`MmapView`]) rather than parsed into heap objects. So a
//! reopen is instant and costs almost no RAM: only the trigrams a query actually
//! touches fault in from disk, and the OS page cache holds the hot set. This
//! mirrors how PostgreSQL's GIN and SQLite's FTS keep their indexes on disk.
//!
//! ## Core components
//!
//! - [`MappedGin`] — a reader over one field's blob inside the file. It stores
//!   only offsets, not data; every read slices bytes out of the shared mmap.
//! - [`MappedGin::open_mapped`] — parses one blob's header (offsets/counts).
//! - [`MappedGin::trigram_bitmap`] — binary-searches the sorted directory for a
//!   trigram and decodes its postings bitmap.
//! - [`MappedGin::slot_hash`] — turns a bitmap slot back into a node hash.
//!
//! ## On-disk layout
//!
//! One blob per field inside the `SKGIN001` container; several blobs share one
//! `Arc<MmapView>`. Per-blob layout (little-endian, version checked by the caller):
//!   [u16 field_len][field]
//!   [u32 version]
//!   [u64 doc_count]  id_map: doc_count × u64
//!   [u32 trigram_count]  dir: trigram_count × [u32 hash | u64 off | u32 len], sorted by hash
//!   [u64 blob_len]  blob: concatenated RoaringBitmap bytes (off relative to blob start)
use crate::storage::mmap::MmapView;
use roaring::RoaringBitmap;
use std::sync::Arc; // Arc = Atomically Reference-Counted: a shared, cheaply-cloned
                    // handle so many field readers can share one mmap of the file.

/// Size of one directory record: `u32 hash + u64 offset + u32 length` = 16 bytes.
const DIR_REC: usize = 4 + 8 + 4;

/// A read-only view over one field's trigram index inside the mmap'd file.
///
/// Notice there is **no data** in here — only the shared mmap handle plus a set
/// of byte offsets and counts into it. Every lookup slices the bytes it needs
/// straight out of the map, so constructing one of these is nearly free and
/// holds no heap. The three regions it points at are the *id map* (slot → node
/// hash), the *directory* (sorted trigram entries), and the *blob* (the packed
/// bitmaps).
#[derive(Clone)]
pub(crate) struct MappedGin {
    view: Arc<MmapView>,     // shared handle to the whole mmap'd gin.bin
    field: String,           // which column this index is for (e.g. "name")
    id_map_off: usize,       // byte offset where the slot→hash u64 array begins
    doc_count: usize,        // number of documents = length of the id map
    dir_off: usize,          // byte offset of the sorted trigram directory
    trigram_count: usize,    // number of directory entries
    blob_off: usize,         // byte offset where the concatenated bitmaps begin
    blob_len: usize,         // total length of the bitmap blob region
}

// Tiny helpers to read a little-endian integer at byte offset `o`. `try_into()`
// converts a slice into a fixed-size array `[u8; 8]`; it returns a `Result`, and
// `.unwrap()` is safe here because callers always pass an in-bounds 8-byte slice.
fn rd_u32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
fn rd_u64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

impl MappedGin {
    /// Parse one field's blob starting at byte `base` in the mmap'd file.
    ///
    /// This does not copy or decode anything heavy — it just *walks the header*
    /// with a moving cursor `p`, recording where each region lives, and validates
    /// that the file isn't truncated. `expected_version` must match the version
    /// stamped in the blob (a mismatch means the on-disk format changed and the
    /// index must be rebuilt). Returns the reader plus how many bytes this blob
    /// occupied, so the container loop can jump to the next field's blob.
    pub(crate) fn open_mapped(
        view: &Arc<MmapView>,
        base: usize,
        expected_version: u32,
    ) -> std::io::Result<(Self, usize)> {
        // `bad` builds a corruption error; `need` checks that `n` more bytes exist
        // before we read them. These are closures — small inline functions that can
        // capture surrounding variables (here `b`). The `?` operator after a
        // fallible call returns early with the error, keeping the happy path flat.
        let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m);
        let total = view.len();
        // Slice from `base` to end-of-file; `saturating_sub` avoids underflow if
        // base somehow exceeds total. `ok_or_else` turns the `Option` into a Result.
        let b = view.slice(base, total.saturating_sub(base)).ok_or_else(|| bad("gin blob range"))?;
        let need = |p: usize, n: usize| -> std::io::Result<()> {
            if p + n > b.len() { Err(bad("truncated gin blob")) } else { Ok(()) }
        };

        // Field name: a length prefix, then that many UTF-8 bytes.
        need(0, 2)?;
        let field_len = u16::from_le_bytes([b[0], b[1]]) as usize;
        let mut p = 2; // cursor: bytes consumed so far within this blob
        need(p, field_len)?;
        let field = std::str::from_utf8(&b[p..p + field_len]).map_err(|_| bad("gin field utf8"))?.to_string();
        p += field_len;

        // Version gate: refuse a blob written by an incompatible index format.
        need(p, 4)?;
        if rd_u32(b, p) != expected_version { return Err(bad("gin version mismatch")); }
        p += 4;

        // id map: `doc_count` u64 slot→hash entries. We only record where it starts
        // and skip over it — individual entries are read lazily by `slot_hash`.
        need(p, 8)?;
        let doc_count = rd_u64(b, p) as usize;
        p += 8;
        let id_map_off = base + p; // note: absolute offset in the file, not in `b`
        p += doc_count * 8;

        // directory: `trigram_count` fixed-size records, sorted by trigram hash.
        need(p, 4)?;
        let trigram_count = rd_u32(b, p) as usize;
        p += 4;
        let dir_off = base + p;
        p += trigram_count * DIR_REC;

        // blob: all the RoaringBitmap bytes packed back-to-back.
        need(p, 8)?;
        let blob_len = rd_u64(b, p) as usize;
        p += 8;
        let blob_off = base + p;
        p += blob_len;
        if p > b.len() { return Err(bad("gin blob overruns file")); } // final sanity check

        // `view.clone()` bumps the Arc's reference count — cheap, no bytes copied.
        Ok((Self { view: view.clone(), field, id_map_off, doc_count, dir_off, trigram_count, blob_off, blob_len }, p))
    }

    /// The column this index covers (e.g. `"name"`).
    pub(crate) fn field(&self) -> &str { &self.field }
    /// How many documents are indexed (the id map length).
    pub(crate) fn doc_count(&self) -> usize { self.doc_count }

    /// Translate a bitmap `slot` (0, 1, 2, …) back into the real node hash.
    ///
    /// Postings bitmaps store compact slot numbers, not node hashes, to stay
    /// small. This reads the one 8-byte entry at `slot` in the id map. `None` if
    /// the slot is out of range or the mmap read fails.
    pub(crate) fn slot_hash(&self, slot: u32) -> Option<u64> {
        if slot as usize >= self.doc_count { return None; } // guard against a bad slot
        let s = self.view.slice(self.id_map_off + slot as usize * 8, 8)?; // one u64 read
        Some(u64::from_le_bytes(s.try_into().ok()?))
    }

    /// Find the postings bitmap for a trigram, or `None` if no document has it.
    ///
    /// The directory is sorted by trigram hash, so this is a **binary search**:
    /// repeatedly halve the search window (`lo..=hi`) until the hash is found or
    /// the window is empty — O(log n) instead of scanning all trigrams. On a hit,
    /// the record gives the bitmap's `(offset, length)` inside the blob, and we
    /// decode just those bytes into a fresh `RoaringBitmap`. The returned bitmap
    /// is owned (a copy), so the caller can freely intersect it with others.
    pub(crate) fn trigram_bitmap(&self, hash: u32) -> Option<RoaringBitmap> {
        let dir = self.view.slice(self.dir_off, self.trigram_count * DIR_REC)?;
        let (mut lo, mut hi) = (0isize, self.trigram_count as isize - 1); // inclusive window
        while lo <= hi {
            let mid = ((lo + hi) / 2) as usize; // midpoint record
            let o = mid * DIR_REC;
            let h = rd_u32(dir, o); // this record's trigram hash
            if h == hash {
                // Found it: read (offset, length) and decode the bitmap from the blob.
                let off = rd_u64(dir, o + 4) as usize;
                let len = rd_u32(dir, o + 12) as usize;
                let blob = self.view.slice(self.blob_off, self.blob_len)?;
                let bytes = blob.get(off..off + len)?; // just this bitmap's bytes
                return RoaringBitmap::deserialize_from(bytes).ok();
            } else if h < hash { lo = mid as isize + 1; } else { hi = mid as isize - 1; } // discard half
        }
        None // hash not present → no document contains this trigram
    }
}
