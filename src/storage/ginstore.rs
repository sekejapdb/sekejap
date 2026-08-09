//! Disk-first (mmap-served) GIN trigram index. Serves `trigram → RoaringBitmap`
//! postings and the slot→hash id map straight from a memory-mapped `gin.bin`, so
//! a paged reopen need not load the ~100 MB/1M-doc index into heap. This mirrors
//! PostgreSQL's on-disk GIN / SQLite FTS: only the trigrams a `LIKE` query touches
//! fault in; resident ≈ working set, bounded by the OS page cache.
//!
//! One blob per field inside the `SKGIN001` container; several share one
//! `Arc<MmapView>`. Per-blob layout (little-endian, version-gated by the caller):
//!   [u16 field_len][field]
//!   [u32 version]
//!   [u64 doc_count]  id_map: doc_count × u64
//!   [u32 trigram_count]  dir: trigram_count × [u32 hash | u64 off | u32 len], sorted by hash
//!   [u64 blob_len]  blob: concatenated RoaringBitmap bytes (off relative to blob start)
use crate::storage::mmap::MmapView;
use roaring::RoaringBitmap;
use std::sync::Arc;

const DIR_REC: usize = 4 + 8 + 4; // hash + off + len = 16

pub(crate) struct MappedGin {
    view: Arc<MmapView>,
    field: String,
    id_map_off: usize,
    doc_count: usize,
    dir_off: usize,
    trigram_count: usize,
    blob_off: usize,
    blob_len: usize,
}

fn rd_u32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
fn rd_u64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

impl MappedGin {
    /// Parse one GIN blob starting at byte `base` in the mmap'd container, serving
    /// its postings + id map from the map. `expected_version` must match the blob's
    /// version. Returns the reader plus the number of bytes consumed so the
    /// container loop can advance to the next field's blob.
    pub(crate) fn open_mapped(
        view: &Arc<MmapView>,
        base: usize,
        expected_version: u32,
    ) -> std::io::Result<(Self, usize)> {
        let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m);
        let total = view.len();
        let b = view.slice(base, total.saturating_sub(base)).ok_or_else(|| bad("gin blob range"))?;
        let need = |p: usize, n: usize| -> std::io::Result<()> {
            if p + n > b.len() { Err(bad("truncated gin blob")) } else { Ok(()) }
        };

        need(0, 2)?;
        let field_len = u16::from_le_bytes([b[0], b[1]]) as usize;
        let mut p = 2;
        need(p, field_len)?;
        let field = std::str::from_utf8(&b[p..p + field_len]).map_err(|_| bad("gin field utf8"))?.to_string();
        p += field_len;

        need(p, 4)?;
        if rd_u32(b, p) != expected_version { return Err(bad("gin version mismatch")); }
        p += 4;

        need(p, 8)?;
        let doc_count = rd_u64(b, p) as usize;
        p += 8;
        let id_map_off = base + p;
        p += doc_count * 8;

        need(p, 4)?;
        let trigram_count = rd_u32(b, p) as usize;
        p += 4;
        let dir_off = base + p;
        p += trigram_count * DIR_REC;

        need(p, 8)?;
        let blob_len = rd_u64(b, p) as usize;
        p += 8;
        let blob_off = base + p;
        p += blob_len;
        if p > b.len() { return Err(bad("gin blob overruns file")); }

        Ok((Self { view: view.clone(), field, id_map_off, doc_count, dir_off, trigram_count, blob_off, blob_len }, p))
    }

    pub(crate) fn field(&self) -> &str { &self.field }
    pub(crate) fn doc_count(&self) -> usize { self.doc_count }

    /// Node hash for a slot — one mmap u64 read.
    pub(crate) fn slot_hash(&self, slot: u32) -> Option<u64> {
        if slot as usize >= self.doc_count { return None; }
        let s = self.view.slice(self.id_map_off + slot as usize * 8, 8)?;
        Some(u64::from_le_bytes(s.try_into().ok()?))
    }

    /// Posting bitmap for a trigram hash — binary search the sorted dir, decode the
    /// bitmap from the blob. Transient owned bitmap.
    pub(crate) fn trigram_bitmap(&self, hash: u32) -> Option<RoaringBitmap> {
        let dir = self.view.slice(self.dir_off, self.trigram_count * DIR_REC)?;
        let (mut lo, mut hi) = (0isize, self.trigram_count as isize - 1);
        while lo <= hi {
            let mid = ((lo + hi) / 2) as usize;
            let o = mid * DIR_REC;
            let h = rd_u32(dir, o);
            if h == hash {
                let off = rd_u64(dir, o + 4) as usize;
                let len = rd_u32(dir, o + 12) as usize;
                let blob = self.view.slice(self.blob_off, self.blob_len)?;
                let bytes = blob.get(off..off + len)?;
                return RoaringBitmap::deserialize_from(bytes).ok();
            } else if h < hash { lo = mid as isize + 1; } else { hi = mid as isize - 1; }
        }
        None
    }
}
