//! # Persisting the search index — write it once, mmap it back
//!
//! The sibling `index.rs` builds the positional search index in memory; this file
//! makes it durable. It serializes the index (FST term dictionary, postings,
//! per-doc norms, the slot↔hash maps) into a single `search.bin` file, and on a
//! paged reopen it *memory-maps* that file and serves the index straight from it
//! — no rebuild, near-zero heap. A leading `[magic][version]` header lets an
//! incompatible or older file be detected and rebuilt instead of mis-read.
//!
//! This is the search index's half of the disk-first bargain: the bulk term data
//! lives on disk (`Bytes::Mapped` slices of the mmap), and only small structures
//! stay resident.

use std::collections::HashMap;
use std::io::{self, Read, Seek, Write};
use std::sync::Arc;
use super::index::{Bytes, IdMap, MappedPostings, Norms, SearchIndex, SlotIndex};
use crate::storage::mmap::MmapView;

const MAGIC: &[u8; 8] = b"SKSRCH02";
// v4: append a sorted (hash,slot) reverse index so paged mode serves hash→slot from
// the mmap (no resident id_to_slot HashMap). Older files fail the check and rebuild.
pub const SEARCH_INDEX_VERSION: u32 = 4;

impl SearchIndex {
    pub fn write_binary<W: Write>(&self, w: &mut W) -> io::Result<()> {
        // Guard rail, matching BM25: the on-disk format stores one segment, so
        // serialising an index that still holds a delta would drop the newest
        // documents from search on the next open. Callers rebuild first.
        // Persisted segments live in their own files and are reloaded by
        // `load_segments`, so they are no longer a reason to refuse. A segment
        // that is still RAM-only is, because it lives nowhere else.
        if self.unpersisted_segments() > 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "search: refusing to serialise an index with an unpersisted segment",
            ));
        }
        w.write_all(MAGIC)?;
        w.write_all(&SEARCH_INDEX_VERSION.to_le_bytes())?;

        // Fields
        w.write_all(&(self.fields.len() as u16).to_le_bytes())?;
        for f in &self.fields {
            let bytes = f.as_bytes();
            w.write_all(&(bytes.len() as u16).to_le_bytes())?;
            w.write_all(bytes)?;
        }

        // ID map
        w.write_all(&self.doc_count.to_le_bytes())?;
        for slot in 0..self.id_map.count() {
            w.write_all(&self.id_map.get(slot).unwrap_or(0).to_le_bytes())?;
        }

        // Doc field lengths (doc-major, num_fields u16 each)
        for slot in 0..self.doc_count as usize {
            if let Some(lengths) = self.doc_field_lengths.doc_lengths(slot) {
                for &l in lengths.iter() {
                    w.write_all(&l.to_le_bytes())?;
                }
            }
        }

        // FST data blob
        w.write_all(&(self.fst_data.len() as u64).to_le_bytes())?;
        w.write_all(self.fst_data.as_slice())?;

        // Postings data blob
        w.write_all(&(self.postings_data.len() as u64).to_le_bytes())?;
        w.write_all(self.postings_data.as_slice())?;

        // Field-scoped postings (FST + bitmap blob)
        write_blob(w, self.field_post.fst.as_slice())?;
        write_blob(w, self.field_post.blob.as_slice())?;

        // Position/proximity postings (FST + bitmap blob)
        write_blob(w, self.position_post.fst.as_slice())?;
        write_blob(w, self.position_post.blob.as_slice())?;

        // Sorted (hash:u64, slot:u32) reverse index — lets paged mode binary-search
        // hash→slot off the mmap instead of holding the id_to_slot HashMap resident.
        //
        // When the index is already served from a mapping, this section exists in
        // that mapping, sorted, in exactly the layout written here — 8 bytes of
        // hash then 4 of slot, no padding. Copy it through.
        //
        // Rebuilding it instead was the entire cost of folding a search index: a
        // `Vec<(u64, u32)>` is 16 bytes an element after alignment, one element
        // per document, so the fold allocated 16 bytes a row and then sorted it.
        // Measured at 7.6 MB for 500 000 rows and 15.2 MB for a million — the
        // number rose with the store while the change was a thousand rows, and it
        // was there even when nothing had changed at all.
        match &self.id_to_slot {
            crate::search::index::SlotIndex::Mapped(b) => {
                let data = b.as_slice();
                let n = (data.len() / 12) as u32;
                w.write_all(&n.to_le_bytes())?;
                w.write_all(data)?;
            }
            crate::search::index::SlotIndex::Resident(_) => {
                // The first build, where there is no mapping to copy from. Costs
                // what building an index over existing data costs, once.
                let mut pairs: Vec<(u64, u32)> = (0..self.id_map.count())
                    .map(|slot| (self.id_map.get(slot).unwrap_or(0), slot as u32)).collect();
                pairs.sort_unstable_by_key(|(h, _)| *h);
                w.write_all(&(pairs.len() as u32).to_le_bytes())?;
                for (hash, slot) in &pairs {
                    w.write_all(&hash.to_le_bytes())?;
                    w.write_all(&slot.to_le_bytes())?;
                }
            }
        }

        Ok(())
    }

    pub fn read_binary<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad search index magic"));
        }

        let mut ver = [0u8; 4];
        r.read_exact(&mut ver)?;
        let version = u32::from_le_bytes(ver);
        if version != SEARCH_INDEX_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "search index version mismatch"));
        }

        // Fields
        let num_fields = read_u16(r)? as usize;
        let mut fields = Vec::with_capacity(num_fields);
        for _ in 0..num_fields {
            fields.push(read_string(r)?);
        }

        // ID map
        let doc_count = read_u32(r)?;
        let mut id_map = Vec::with_capacity(doc_count as usize);
        let mut id_to_slot = HashMap::with_capacity(doc_count as usize);
        for slot in 0..doc_count {
            let hash = read_u64(r)?;
            id_to_slot.insert(hash, slot);
            id_map.push(hash);
        }

        // Doc field lengths
        let mut doc_field_lengths = Vec::with_capacity(doc_count as usize);
        for _ in 0..doc_count {
            let mut lengths = Vec::with_capacity(num_fields);
            for _ in 0..num_fields {
                lengths.push(read_u16(r)?);
            }
            doc_field_lengths.push(lengths);
        }

        // FST data blob + postings blob
        let fst_data = Bytes::Owned(read_blob(r)?);
        let postings_data = Bytes::Owned(read_blob(r)?);

        // Field-scoped postings (FST + blob)
        let field_post = MappedPostings {
            fst: Bytes::Owned(read_blob(r)?),
            blob: Bytes::Owned(read_blob(r)?),
        };
        // Position/proximity postings (FST + blob)
        let position_post = MappedPostings {
            fst: Bytes::Owned(read_blob(r)?),
            blob: Bytes::Owned(read_blob(r)?),
        };

        // Sorted (hash,slot) reverse index — resident mode keeps the HashMap built
        // above from id_map, so just consume this section to advance the stream.
        let sorted_count = read_u32(r)? as usize;
        let mut skip = vec![0u8; sorted_count * 12];
        r.read_exact(&mut skip)?;

        Ok(SearchIndex {
            fields,
            id_map: IdMap::Owned(id_map),
            id_to_slot: SlotIndex::Resident(id_to_slot),
            doc_count,
            doc_field_lengths: Norms::Owned(doc_field_lengths),
            fst_data,
            postings_data,
            field_post,
            position_post,
            deltas: Vec::new(),
            retired_segment_files: Vec::new(),
            next_segment_id: 0,
        })
    }

    /// Disk-first open of one `SKSRCH02` blob starting at byte `base` in an mmap'd
    /// `search.bin`. The two bulk blobs (FST term dict + postings) are served from
    /// the memory map (`Bytes::Mapped`) — never read into RAM; only the scalars,
    /// id map, norms, and the field/position bitmaps stay resident. Returns the
    /// index plus the number of bytes the blob consumed, so a container loop can
    /// advance to the next entry. Several blobs share one `Arc<MmapView>`.
    pub(crate) fn open_mapped(view: &Arc<MmapView>, base: usize) -> io::Result<(SearchIndex, usize)> {
        let total = view.len();
        let bytes = view.slice(base, total.saturating_sub(base))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "search blob out of range"))?;
        let mut r = io::Cursor::new(bytes);

        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad search index magic"));
        }
        let version = read_u32(&mut r)?;
        if version != SEARCH_INDEX_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "search index version mismatch"));
        }

        // Fields (resident)
        let num_fields = read_u16(&mut r)? as usize;
        let mut fields = Vec::with_capacity(num_fields);
        for _ in 0..num_fields {
            fields.push(read_string(&mut r)?);
        }

        // ID map → mmap slice (slot→hash, 8 B/doc), served off the map. The reverse
        // hash→slot index is the sorted section below.
        let doc_count = read_u32(&mut r)?;
        let id_map_off = base + r.position() as usize;
        r.seek(io::SeekFrom::Current(doc_count as i64 * 8))?;
        let id_map = IdMap::Mapped { view: view.clone(), off: id_map_off, count: doc_count as usize };

        // Doc field lengths / norms (resident)
        // Norms → mmap slice (doc_count × num_fields u16), served off the map.
        let norms_off = base + r.position() as usize;
        r.seek(io::SeekFrom::Current(doc_count as i64 * num_fields as i64 * 2))?;
        let doc_field_lengths = Norms::Mapped {
            view: view.clone(), off: norms_off, doc_count: doc_count as usize, num_fields,
        };

        // Each remaining blob → an mmap slice (skip its bytes, don't copy).
        let map_blob = |r: &mut io::Cursor<&[u8]>| -> io::Result<Bytes> {
            let len = read_u64(r)? as usize;
            let off = base + r.position() as usize;
            r.seek(io::SeekFrom::Current(len as i64))?;
            Ok(Bytes::Mapped { view: view.clone(), off, len })
        };
        let fst_data = map_blob(&mut r)?;
        let postings_data = map_blob(&mut r)?;
        let field_post = MappedPostings { fst: map_blob(&mut r)?, blob: map_blob(&mut r)? };
        let position_post = MappedPostings { fst: map_blob(&mut r)?, blob: map_blob(&mut r)? };

        // Sorted (hash,slot) reverse index → mmap slice (12 B/rec), binary-searched.
        let sorted_count = read_u32(&mut r)? as usize;
        let sorted_off = base + r.position() as usize;
        let sorted_len = sorted_count * 12;
        r.seek(io::SeekFrom::Current(sorted_len as i64))?;
        let id_to_slot = SlotIndex::Mapped(Bytes::Mapped { view: view.clone(), off: sorted_off, len: sorted_len });

        let consumed = r.position() as usize;
        Ok((SearchIndex {
            fields,
            id_map,
            id_to_slot,
            doc_count,
            doc_field_lengths,
            fst_data,
            postings_data,
            field_post,
            position_post,
            deltas: Vec::new(),
            retired_segment_files: Vec::new(),
            next_segment_id: 0,
        }, consumed))
    }
}

/// Write a length-prefixed byte blob (`[len:u64 LE][bytes]`).
fn write_blob<W: Write>(w: &mut W, data: &[u8]) -> io::Result<()> {
    w.write_all(&(data.len() as u64).to_le_bytes())?;
    w.write_all(data)
}

/// Read a length-prefixed byte blob written by [`write_blob`].
fn read_blob<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let len = read_u64(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_u16<R: Read>(r: &mut R) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_string<R: Read>(r: &mut R) -> io::Result<String> {
    let len = read_u16(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::index::DocFields;

    #[test]
    fn roundtrip() {
        let idx = SearchIndex::build(
            vec!["title".into(), "body".into()],
            vec![
                DocFields { hash: 1, field_values: vec!["hello world".into(), "rust programming".into()] },
                DocFields { hash: 2, field_values: vec!["python guide".into(), "easy language".into()] },
            ].into_iter(),
        );

        let mut buf = Vec::new();
        idx.write_binary(&mut buf).unwrap();

        let mut cursor = io::Cursor::new(&buf);
        let loaded = SearchIndex::read_binary(&mut cursor).unwrap();

        assert_eq!(loaded.fields, idx.fields);
        assert_eq!(loaded.id_map.count(), idx.id_map.count());
        for s in 0..idx.id_map.count() {
            assert_eq!(loaded.id_map.get(s), idx.id_map.get(s));
        }
        assert_eq!(loaded.doc_count, idx.doc_count);
        for s in 0..idx.doc_count as usize {
            assert_eq!(loaded.doc_field_lengths.doc_lengths(s), idx.doc_field_lengths.doc_lengths(s));
        }
        assert_eq!(loaded.fst_data.as_slice(), idx.fst_data.as_slice());
        assert_eq!(loaded.postings_data.as_slice(), idx.postings_data.as_slice());

        // Verify search still works after roundtrip
        let results = loaded.search("rust");
        assert!(results.contains(0));
        assert!(!results.contains(1));
    }

    #[test]
    fn roundtrip_fuzzy() {
        let idx = SearchIndex::build(
            vec!["title".into()],
            vec![
                DocFields { hash: 1, field_values: vec!["Rust Programming Language".into()] },
                DocFields { hash: 2, field_values: vec!["Python Scripting".into()] },
            ].into_iter(),
        );

        let mut buf = Vec::new();
        idx.write_binary(&mut buf).unwrap();

        let mut cursor = io::Cursor::new(&buf);
        let loaded = SearchIndex::read_binary(&mut cursor).unwrap();

        // Fuzzy match should work after roundtrip
        let results = loaded.search("programing");
        assert!(results.contains(0), "fuzzy should work after disk roundtrip");
    }
}

// ── Segments on disk ──────────────────────────────────────────────────────────
//
// A flush becomes an immutable segment; persisting it is what lets compaction
// leave it alone instead of rebuilding the collection's whole index. Measured
// before this existed: the search index was O(N^1.77) over a 500k/1M/2M ladder,
// because `merge_search_deltas` called `rebuild_search_for_collection` — a full
// rebuild — on every compaction.
//
// One file per segment: the index exactly as `write_binary` lays it out, then the
// source rows. The rows are needed only to merge segments later, so they sit past
// the index and are read back on demand rather than held resident.

use super::index::{DocFields, SearchSegment, SegDocs, SEARCH_MERGE_MAX_DOCS, SEARCH_SEG_FANOUT};
use std::path::Path;

/// `[n u32]` then per row `[hash u64][nfields u16][(len u32, bytes)...]`.
pub(crate) fn encode_docs(docs: &[DocFields], out: &mut Vec<u8>) {
    out.extend_from_slice(&(docs.len() as u32).to_le_bytes());
    for d in docs {
        out.extend_from_slice(&d.hash.to_le_bytes());
        out.extend_from_slice(&(d.field_values.len() as u16).to_le_bytes());
        for v in &d.field_values {
            let b = v.as_bytes();
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
    }
}

/// Every length here comes out of the file, so each one is bounds-checked before
/// it is used. A truncated or damaged tail yields the rows read so far rather
/// than a panic.
pub(crate) fn decode_docs(b: &[u8], mut p: usize) -> Vec<DocFields> {
    let rd32 = |b: &[u8], p: usize| b.get(p..p + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap()));
    let rd16 = |b: &[u8], p: usize| b.get(p..p + 2).map(|x| u16::from_le_bytes(x.try_into().unwrap()));
    let rd64 = |b: &[u8], p: usize| b.get(p..p + 8).map(|x| u64::from_le_bytes(x.try_into().unwrap()));
    let Some(n) = rd32(b, p) else { return Vec::new() };
    p += 4;
    // Each row needs at least 10 bytes, so the file bounds how many there can be.
    let n = (n as usize).min(b.len().saturating_sub(p) / 10);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let Some(hash) = rd64(b, p) else { break };
        p += 8;
        let Some(nf) = rd16(b, p) else { break };
        p += 2;
        let mut field_values = Vec::with_capacity(nf as usize);
        for _ in 0..nf {
            let Some(len) = rd32(b, p) else { return out };
            p += 4;
            let Some(raw) = b.get(p..p + len as usize) else { return out };
            p += len as usize;
            field_values.push(String::from_utf8_lossy(raw).into_owned());
        }
        out.push(DocFields { hash, field_values });
    }
    out
}

/// `[u32 level][u32 reserved]` in front of a segment file.
///
/// # Why the level must be on disk
///
/// A merge level is the only thing that says a segment is already large and
/// should not be merged again until seven more of its size exist. Compaction
/// reloads the index from `search.bin`, and a loader that cannot read the level
/// has to guess — guessing zero demotes every merged segment to the bottom, so
/// the next eight flushes absorb it and rewrite it whole. That turns a level into
/// a monolithic base: measured at 16 MB rewritten on the first compaction, 178 MB
/// by the tenth, growing linearly, which is O(N^2) over a load.
const SEG_HEADER: usize = 8;

fn seg_path(dir: &Path, key: &str, id: u64) -> std::path::PathBuf {
    // Hashed, because a collection name is user text and this is a filename.
    dir.join(format!("search_{:016x}.s{}.bin", crate::sk_hash(key), id))
}

impl SearchIndex {
    /// Write every segment not yet on disk. **O(change)** — one already written
    /// is left alone, and the base is not touched.
    pub(crate) fn persist_segments(&mut self, dir: &Path, key: &str) -> io::Result<()> {
        for i in 0..self.deltas.len() {
            if self.deltas[i].persisted.is_some() {
                continue;
            }
            let id = self.next_segment_id;
            self.next_segment_id += 1;
            let path = seg_path(dir, key, id);

            let mut buf: Vec<u8> = Vec::new();
            buf.extend_from_slice(&self.deltas[i].level.to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes()); // reserved, keeps the index 8-aligned
            self.deltas[i].index.write_binary(&mut buf)?;
            let docs_off = buf.len();
            encode_docs(&self.deltas[i].docs.load(), &mut buf);

            let tmp = path.with_extension("bin.tmp");
            {
                let mut f = std::fs::File::create(&tmp)?;
                f.write_all(&buf)?;
                f.sync_all()?;
            }
            std::fs::rename(&tmp, &path)?;

            // Serve it from the map now, which is what frees the RAM it held.
            if let Some((mapped, _, _)) = open_segment(&path)? {
                self.deltas[i].index = Box::new(mapped);
            }
            self.deltas[i].id = id;
            self.deltas[i].docs = SegDocs::OnDisk { path: path.clone(), off: docs_off };
            self.deltas[i].persisted = Some(path);
        }
        // Only now: the replacements are durable, so what they replaced can go.
        for path in std::mem::take(&mut self.retired_segment_files) {
            let _ = std::fs::remove_file(&path);
        }
        Ok(())
    }

    /// Load persisted segments for `key` back from `dir`.
    ///
    /// Found by scanning the directory rather than from a manifest: a single
    /// index of everything is a single point of total loss.
    pub(crate) fn load_segments(&mut self, dir: &Path, key: &str) -> io::Result<()> {
        let prefix = format!("search_{:016x}.s", crate::sk_hash(key));
        let Ok(rd) = std::fs::read_dir(dir) else { return Ok(()) };
        let mut found: Vec<(u64, std::path::PathBuf)> = Vec::new();
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(rest) = name.strip_prefix(&prefix) else { continue };
            let Some(idstr) = rest.strip_suffix(".bin") else { continue };
            let Ok(id) = idstr.parse::<u64>() else { continue };
            found.push((id, entry.path()));
        }
        found.sort_by_key(|(id, _)| *id);
        for (id, path) in found {
            // A segment that cannot be read is left out rather than served.
            if let Ok(Some((index, docs_off, level))) = open_segment(&path) {
                self.next_segment_id = self.next_segment_id.max(id + 1);
                self.deltas.push(SearchSegment {
                    // From the file. Defaulting to 0 here demoted every merged
                    // segment on every reload — see `SEG_HEADER`.
                    level,
                    // Recomputed rather than stored: it is a function of size
                    // against the current cap, so a cap change takes effect.
                    sealed: index.doc_count > SEARCH_MERGE_MAX_DOCS / SEARCH_SEG_FANOUT as u32,
                    id,
                    index: Box::new(index),
                    docs: SegDocs::OnDisk { path: path.clone(), off: docs_off },
                    persisted: Some(path),
                });
            }
        }
        Ok(())
    }

    /// Delete every persisted segment file for `key`.
    ///
    /// A full rebuild produces an index covering every document, so segment files
    /// left beside it are duplicates of part of it. Loading both counts those
    /// documents twice — which for search means a slot space that no longer
    /// matches the corpus. The same trap bit BM25 first.
    pub(crate) fn remove_segment_files(dir: &Path, key: &str) {
        let prefix = format!("search_{:016x}.s", crate::sk_hash(key));
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.ends_with(".bin") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// mmap one segment file: the index, where its source rows begin, and its level.
fn open_segment(path: &Path) -> io::Result<Option<(SearchIndex, usize, u32)>> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len < SEG_HEADER {
        return Ok(None);
    }
    let Some(view) = MmapView::try_new(&file, len) else { return Ok(None) };
    let view = Arc::new(view);
    let level = match view.slice(0, 4) {
        Some(b) => u32::from_le_bytes(b.try_into().unwrap()),
        None => return Ok(None),
    };
    match SearchIndex::open_mapped(&view, SEG_HEADER) {
        Ok((ix, consumed)) => Ok(Some((ix, SEG_HEADER + consumed, level))),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod segment_level_tests {
    use super::*;
    use crate::search::index::DocFields;

    fn doc(h: u64) -> DocFields {
        DocFields { hash: h, field_values: vec![format!("heron riverbank n{h}")] }
    }

    /// A segment's merge level must survive being written and read back.
    ///
    /// The level is the only thing that says a segment is already large and must
    /// not be merged again until seven more of its size exist. It was not stored,
    /// so every reload demoted merged segments to level 0 — and compaction
    /// reloads. The next eight flushes then absorbed the big segment and rewrote
    /// it whole, every time: 16 MB on the first compaction, 178 MB by the tenth,
    /// growing linearly. A level turned into a monolithic base, which is O(N^2)
    /// over a load and the exact shape segments exist to remove.
    #[test]
    fn a_segments_merge_level_survives_a_reload() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = "coll:t";

        let mut ix = SearchIndex::build(vec!["body".into()], std::iter::empty());
        ix.insert_docs((0..50).map(doc));
        assert_eq!(ix.deltas.len(), 1, "one flush, one segment");
        // Stand in for a segment that has already been merged upward twice.
        ix.deltas[0].level = 2;
        ix.persist_segments(dir.path(), key).unwrap();

        let mut reopened = SearchIndex::build(vec!["body".into()], std::iter::empty());
        reopened.load_segments(dir.path(), key).unwrap();
        assert_eq!(reopened.deltas.len(), 1, "the segment was not found again");
        assert_eq!(
            reopened.deltas[0].level, 2,
            "the level came back as {} — a demoted segment is re-merged and \
             rewritten on every compaction",
            reopened.deltas[0].level
        );
    }

    /// The rows a segment was built from must come back too, or a later merge
    /// silently drops every document it holds.
    #[test]
    fn a_segments_source_rows_survive_a_reload() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = "coll:t";
        let mut ix = SearchIndex::build(vec!["body".into()], std::iter::empty());
        ix.insert_docs((0..50).map(doc));
        ix.persist_segments(dir.path(), key).unwrap();

        let mut reopened = SearchIndex::build(vec!["body".into()], std::iter::empty());
        reopened.load_segments(dir.path(), key).unwrap();
        let rows = reopened.deltas[0].docs.load();
        assert_eq!(rows.len(), 50, "source rows were lost");
        assert!(rows.iter().any(|d| d.hash == 7), "a specific row went missing");
    }
}
