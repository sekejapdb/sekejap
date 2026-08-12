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
        let mut pairs: Vec<(u64, u32)> = (0..self.id_map.count())
            .map(|slot| (self.id_map.get(slot).unwrap_or(0), slot as u32)).collect();
        pairs.sort_unstable_by_key(|(h, _)| *h);
        w.write_all(&(pairs.len() as u32).to_le_bytes())?;
        for (hash, slot) in &pairs {
            w.write_all(&hash.to_le_bytes())?;
            w.write_all(&slot.to_le_bytes())?;
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
