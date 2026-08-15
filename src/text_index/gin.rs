//! # GIN trigram index — the in-memory builder
//!
//! This is the in-RAM form of the trigram index whose on-disk twin is
//! `storage/ginstore.rs` (read that file's docs for what trigrams and postings
//! are). GIN ("Generalized Inverted Index") stores, for each trigram, the *exact*
//! set of documents that contain it, as a compressed bitmap. "Exact postings"
//! distinguishes it from GiST below (which uses lossy signatures) — but either
//! way a query still does a final substring re-check, because a document can hold
//! all of a word's trigrams without holding the contiguous word.
//!
//! ## GIN Trigram Index (Exact Postings)
//!
//! GIN (Generalized Inverted Index) for trigrams using exact postings.
//!
//! Unlike GiST (lossy signatures), GIN stores exact trigram→docIDs mappings.
//! This means no verification step needed, but uses more memory.
//!
//! ### Memory Usage
//!
//! - Per trigram: RoaringBitmap of doc IDs
//! - Dense trigrams (e.g., " the" in 70% of docs): ~88KB compressed
//! - Sparse trigrams (e.g., "xyz" in 0.001% of docs): ~12 bytes
//! - Total: ~100MB/1M docs average
//!
//! ### How It Works
//!
//! **Index structure:**
//! ```text
//! trigram_hash -> RoaringBitmap<doc_ids>
//! ```
//!
//! **Query "%Alpha%":**
//! ```text
//! 1. Extract trigrams: [" al", " alp", "alp", "lph", "pha", "ha "]
//! 2. Look up each trigram → get RoaringBitmaps
//! 3. Intersect all bitmaps → candidates (documents with ALL trigrams)
//! 4. Return candidates (exact — no verification needed)
//! ```
//!
//! ### When to Use GIN vs GiST
//!
//! | Scenario | GiST | GIN |
//! |---------|------|-----|
//! | Memory-constrained (Pi) | ✅ | ❌ |
//! | Need exact match (no verify) | ❌ | ✅ |
//! | Large result sets | ⚠️ | ✅ |
//! | Short strings (varchar < 50) | ⚠️ | ⚠️ |
//! | Long text (body, description) | ✅ | ✅ |
//!
//! ### Why RoaringBitmap?
//!
//! - Pure Rust (no C deps)
//! - Compressed bitmap for doc IDs
//! - Fast intersection via `&` operator
//! - Used by Lucene/Tantivy internally

use crate::text_index::trigram::{
    dedup_trigrams, extract_pattern_trigrams, extract_trigrams, hash_trigram,
};
use std::collections::HashMap;

/// A GIN trigram index using exact postings with RoaringBitmaps.
///
/// Each trigram maps to a RoaringBitmap of document IDs that contain it.
/// Querying intersects the bitmaps to find documents with ALL required trigrams.
#[derive(Clone)]
pub struct GINIndex {
    /// Inverted index: trigram_hash -> RoaringBitmap of slot indices
    postings: HashMap<u32, roaring::RoaringBitmap>,
    /// Slot index → original u64 node hash.
    /// Needed because RoaringBitmap only stores u32; node hashes are u64.
    id_map: Vec<u64>,
    /// Total documents indexed
    doc_count: usize,
    /// Field name being indexed
    field: String,
    /// Disk-first (paged) base: postings + id map served from an mmap'd `gin.bin`.
    /// When present the resident `postings`/`id_map` are empty (attached only on a
    /// clean paged reopen, no post-compact writes). `None` in heap mode.
    mapped: Option<crate::storage::ginstore::MappedGin>,
    /// Slots whose document has been deleted, or superseded by an update.
    ///
    /// Trigram bitmaps are append-only and may be mmap-backed, so a document
    /// cannot be erased from them. Removal is recorded here and applied in
    /// [`slot_hash`], the same liveness gate the search index uses — which is what
    /// lets a delete or an update cost `O(text)` instead of a full rebuild.
    ///
    /// [`slot_hash`]: GINIndex::slot_hash
    dead_slots: roaring::RoaringBitmap,
    /// `doc_id` -> its one live slot.
    ///
    /// Built on demand, because the mmap'd base stores slot → hash and offers no
    /// reverse lookup; walking it per mutation would put every insert and delete
    /// back to `O(documents)`, which is the cost this whole change exists to
    /// remove. Built once on the first mutation, maintained from then on.
    slot_of: Option<HashMap<u64, u32>>,
}

impl GINIndex {
    /// Build a new GIN index by iterating over documents.
    ///
    /// # Arguments
    /// * `docs` - Iterator of (doc_id, text) pairs
    /// * `field` - Field name being indexed (for statistics)
    ///
    /// # Returns
    /// * `Self` - The built index
    pub fn build<'a>(docs: impl Iterator<Item = (u64, &'a str)>, field: &str) -> Self {
        let mut postings: HashMap<u32, roaring::RoaringBitmap> = HashMap::new();
        let mut id_map: Vec<u64> = Vec::new();
        let mut slot_map: HashMap<u64, u32> = HashMap::new();
        let mut doc_count = 0;

        for (doc_id, text) in docs {
            let trigrams = extract_trigrams(text);
            if !trigrams.is_empty() {
                let slot = *slot_map.entry(doc_id).or_insert_with(|| {
                    let s = id_map.len() as u32;
                    id_map.push(doc_id);
                    s
                });
                for trigram in &trigrams {
                    let h = hash_trigram(trigram);
                    postings
                        .entry(h)
                        .or_insert_with(roaring::RoaringBitmap::new)
                        .insert(slot);
                }
                doc_count += 1;
            }
        }

        Self {
            postings,
            id_map,
            doc_count,
            field: field.to_string(),
            mapped: None,
            dead_slots: roaring::RoaringBitmap::new(),
            slot_of: None,
        }
    }

    /// True when postings + id map are served from the mmap base (paged, disk-first).
    pub fn is_disk_backed(&self) -> bool {
        self.mapped.is_some()
    }

    /// True when writes have accumulated on top of the mmap base.
    ///
    /// `gin.bin` stores one flat segment, so the resident overlay has nowhere to be
    /// written and would be lost on reopen. The database rebuilds these before
    /// persisting; this is how it knows which ones need it.
    pub fn has_pending_overlay(&self) -> bool {
        self.mapped.is_some() && (!self.id_map.is_empty() || !self.dead_slots.is_empty())
    }

    /// Disk-first GIN for paged mode: postings + id map served from the mmap base;
    /// resident maps stay empty. Attached only on a clean reopen (no overlay).
    pub(crate) fn from_mapped(base: crate::storage::ginstore::MappedGin) -> Self {
        Self {
            postings: HashMap::new(),
            id_map: Vec::new(),
            doc_count: base.doc_count(),
            dead_slots: roaring::RoaringBitmap::new(),
            slot_of: None,
            field: base.field().to_string(),
            mapped: Some(base),
        }
    }

    /// Posting bitmap for a trigram hash — resident overlay or the mmap base.
    /// Slots `0 .. base_slots()` belong to the mmap'd base; resident writes are
    /// numbered from there. Sharing one flat slot space is what keeps the two
    /// halves addressable by a single bitmap.
    #[inline]
    fn base_slots(&self) -> u32 {
        self.mapped.as_ref().map_or(0, |m| m.doc_count() as u32)
    }

    fn trigram_postings(&self, hash: u32) -> Option<roaring::RoaringBitmap> {
        // Union, not fallback. Returning the resident bitmap alone would hide every
        // base document carrying the same trigram — one write after a paged reopen
        // would silently erase them from ILIKE results.
        let resident = self.postings.get(&hash);
        let base = self.mapped.as_ref().and_then(|m| m.trigram_bitmap(hash));
        match (resident, base) {
            (Some(r), Some(b)) => Some(r | b),
            (Some(r), None) => Some(r.clone()),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// Node hash for a slot — the mmap base below `base_slots()`, the resident
    /// overlay above it. `None` for a slot whose document has been retired.
    fn slot_hash(&self, slot: u32) -> Option<u64> {
        if self.dead_slots.contains(slot) {
            return None;
        }
        let base = self.base_slots();
        if slot < base {
            return self.mapped.as_ref().and_then(|m| m.slot_hash(slot));
        }
        self.id_map.get((slot - base) as usize).copied()
    }

    /// Retire every slot holding `doc_id`, so it stops matching.
    ///
    /// Scans the slot table, which costs a pass over an array of `u64` — orders of
    /// magnitude below the full rebuild this replaces, and only on a delete.
    pub fn delete(&mut self, doc_id: u64) -> bool {
        self.ensure_slot_map();
        let slot = match self.slot_of.as_mut().and_then(|m| m.remove(&doc_id)) {
            Some(s) => s,
            None => return false,
        };
        self.dead_slots.insert(slot);
        self.doc_count = self.doc_count.saturating_sub(1);
        true
    }

    /// Populate the reverse slot map, once, by walking both halves of the slot
    /// space. Later slots win, so a document written twice resolves to its newest.
    fn ensure_slot_map(&mut self) {
        if self.slot_of.is_some() {
            return;
        }
        let base = self.base_slots();
        let mut map: HashMap<u64, u32> = HashMap::with_capacity(
            base as usize + self.id_map.len(),
        );
        for slot in 0..base {
            if self.dead_slots.contains(slot) {
                continue;
            }
            if let Some(h) = self.mapped.as_ref().and_then(|m| m.slot_hash(slot)) {
                map.insert(h, slot);
            }
        }
        for (i, &h) in self.id_map.iter().enumerate() {
            let slot = base + i as u32;
            if !self.dead_slots.contains(slot) {
                map.insert(h, slot);
            }
        }
        self.slot_of = Some(map);
    }

    /// Query the index for documents matching an ILIKE pattern.
    ///
    /// Returns doc IDs that match (exact — no verification needed).
    ///
    /// # Arguments
    /// * `pattern` - ILIKE pattern (e.g., "%Alpha%")
    /// * `limit` - Maximum results to return (None for all)
    ///
    /// # Returns
    /// * `Vec<u64>` - Matching doc IDs
    pub fn ilike(&self, pattern: &str, limit: Option<usize>) -> Vec<u64> {
        // Extract trigrams from pattern
        let pattern_trigrams = extract_pattern_trigrams(pattern);
        if pattern_trigrams.is_empty() {
            // Degenerate pattern (all wildcards) — matches every indexed doc.
            return (0..self.base_slots() + self.id_map.len() as u32)
                .filter_map(|slot| self.slot_hash(slot))
                .take(limit.unwrap_or(usize::MAX))
                .collect();
        }

        // Deduplicate trigrams
        let trigrams = dedup_trigrams(&pattern_trigrams);

        // Start with first trigram's bitmap, intersect with rest
        let first_h = hash_trigram(&trigrams[0]);
        let mut result = match self.trigram_postings(first_h) {
            Some(bm) => bm,
            None => return vec![], // No documents have first trigram
        };

        for trigram in &trigrams[1..] {
            let h = hash_trigram(trigram);
            match self.trigram_postings(h) {
                Some(bm) => result &= bm,
                None => return vec![], // This trigram doesn't exist in any document
            }
            if result.is_empty() {
                return vec![]; // Early exit if intersection empty
            }
        }

        // Apply limit — map slot indices back to original u64 node hashes
        result
            .iter()
            .filter_map(|slot| self.slot_hash(slot))
            .take(limit.unwrap_or(usize::MAX))
            .collect()
    }

    /// Incrementally add a single document to the index.
    ///
    /// O(trigrams_in_text) — safe to call per-insert for new documents.
    /// For updates (doc already indexed), remove the old entry first by
    /// calling `build_gin_index()` for a full rebuild.
    pub fn insert_doc(&mut self, doc_id: u64, text: &str) {
        // Retire any earlier copy first, so re-indexing a document replaces it
        // rather than leaving the old text matching alongside the new.
        self.delete(doc_id);
        let trigrams = extract_trigrams(text);
        if !trigrams.is_empty() {
            // Numbered above the mmap base — using id_map.len() alone would collide
            // with a base slot and make one existing document unreachable.
            let slot = self.base_slots() + self.id_map.len() as u32;
            self.id_map.push(doc_id);
            if let Some(map) = self.slot_of.as_mut() {
                map.insert(doc_id, slot);
            }
            for trigram in &trigrams {
                let h = hash_trigram(trigram);
                self.postings
                    .entry(h)
                    .or_insert_with(roaring::RoaringBitmap::new)
                    .insert(slot);
            }
            self.doc_count += 1;
        }
    }

    /// Reconstruct a GINIndex directly from serialized parts (used by snapshot restore).
    ///
    /// * `id_map`   – slot → node hash
    /// * `postings` – (trigram_hash, sorted slot list) pairs; slot lists are rebuilt
    ///                into RoaringBitmaps
    /// * `field`    – field name
    pub fn from_parts(id_map: Vec<u64>, postings: Vec<(u32, Vec<u32>)>, field: &str) -> Self {
        let doc_count = id_map.len();
        let postings_map: HashMap<u32, roaring::RoaringBitmap> = postings
            .into_iter()
            .map(|(h, slots)| {
                let mut bm = roaring::RoaringBitmap::new();
                bm.extend(slots.into_iter());
                (h, bm)
            })
            .collect();
        Self {
            postings: postings_map,
            id_map,
            doc_count,
            field: field.to_string(),
            mapped: None,
            dead_slots: roaring::RoaringBitmap::new(),
            slot_of: None,
        }
    }

    /// Return a copy of the id_map (slot → node hash).
    pub fn id_map_cloned(&self) -> Vec<u64> {
        self.id_map.clone()
    }

    /// Return all postings as (trigram_hash, sorted_slot_list) pairs.
    pub fn postings_as_vecs(&self) -> Vec<(u32, Vec<u32>)> {
        self.postings
            .iter()
            .map(|(&h, bm)| (h, bm.iter().collect()))
            .collect()
    }

    /// Write this GIN index to a binary stream.
    ///
    /// Format (all integers little-endian):
    ///   [u16 field_name_len][field_name_bytes]
    ///   [u32 GIN_INDEX_VERSION]
    ///   [u64 id_map_len][u64 × id_map_len]
    ///   [u32 postings_count]
    ///   per posting: [u32 trigram_hash][u32 bitmap_byte_len][bitmap_bytes]
    pub fn write_binary<W: std::io::Write>(&self, w: &mut W, version: u32) -> std::io::Result<()> {
        let field_bytes = self.field.as_bytes();
        w.write_all(&(field_bytes.len() as u16).to_le_bytes())?;
        w.write_all(field_bytes)?;
        w.write_all(&version.to_le_bytes())?;
        w.write_all(&(self.id_map.len() as u64).to_le_bytes())?;
        for &h in &self.id_map {
            w.write_all(&h.to_le_bytes())?;
        }
        // Sorted trigram directory + concatenated bitmap blob (binary-searchable
        // directly on the mmap by MappedGin). dir record = [u32 hash][u64 off][u32 len].
        let mut entries: Vec<(u32, Vec<u8>)> = Vec::with_capacity(self.postings.len());
        for (&h, bm) in &self.postings {
            let mut bytes = Vec::new();
            bm.serialize_into(&mut bytes)?;
            entries.push((h, bytes));
        }
        entries.sort_unstable_by_key(|(h, _)| *h);
        w.write_all(&(entries.len() as u32).to_le_bytes())?;
        let mut blob: Vec<u8> = Vec::new();
        let mut dir: Vec<u8> = Vec::with_capacity(entries.len() * 16);
        for (h, bytes) in &entries {
            let off = blob.len() as u64;
            dir.extend_from_slice(&h.to_le_bytes());
            dir.extend_from_slice(&off.to_le_bytes());
            dir.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            blob.extend_from_slice(bytes);
        }
        w.write_all(&dir)?;
        w.write_all(&(blob.len() as u64).to_le_bytes())?;
        w.write_all(&blob)?;
        Ok(())
    }

    /// Read one GIN index from a binary stream (written by `write_binary`).
    /// Returns `(field_name, index)`. Returns `Err` on any parse/IO failure.
    pub fn read_binary<R: std::io::Read>(r: &mut R, expected_version: u32) -> std::io::Result<(String, Self)> {
        use std::io::{Error, ErrorKind};
        let mut u16buf = [0u8; 2];
        r.read_exact(&mut u16buf)?;
        let field_len = u16::from_le_bytes(u16buf) as usize;
        let mut field_bytes = vec![0u8; field_len];
        r.read_exact(&mut field_bytes)?;
        let field = String::from_utf8(field_bytes)
            .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;

        let mut u32buf = [0u8; 4];
        r.read_exact(&mut u32buf)?;
        let version = u32::from_le_bytes(u32buf);
        if version != expected_version {
            return Err(Error::new(ErrorKind::InvalidData,
                format!("gin.bin version {version} != expected {expected_version}")));
        }

        let mut u64buf = [0u8; 8];
        r.read_exact(&mut u64buf)?;
        let id_map_len = u64::from_le_bytes(u64buf) as usize;
        let mut id_map = Vec::with_capacity(id_map_len);
        for _ in 0..id_map_len {
            r.read_exact(&mut u64buf)?;
            id_map.push(u64::from_le_bytes(u64buf));
        }

        // Sorted trigram directory + bitmap blob (see write_binary). Read the dir,
        // then slice each bitmap out of the blob.
        r.read_exact(&mut u32buf)?;
        let trigram_count = u32::from_le_bytes(u32buf) as usize;
        let mut dir: Vec<(u32, u64, u32)> = Vec::with_capacity(trigram_count);
        for _ in 0..trigram_count {
            r.read_exact(&mut u32buf)?;
            let hash = u32::from_le_bytes(u32buf);
            r.read_exact(&mut u64buf)?;
            let off = u64::from_le_bytes(u64buf);
            r.read_exact(&mut u32buf)?;
            let len = u32::from_le_bytes(u32buf);
            dir.push((hash, off, len));
        }
        r.read_exact(&mut u64buf)?;
        let blob_len = u64::from_le_bytes(u64buf) as usize;
        let mut blob = vec![0u8; blob_len];
        r.read_exact(&mut blob)?;
        let mut postings = HashMap::with_capacity(trigram_count);
        for (hash, off, len) in dir {
            let (o, l) = (off as usize, len as usize);
            let bytes = blob.get(o..o + l)
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "gin blob offset out of range"))?;
            let bm = roaring::RoaringBitmap::deserialize_from(bytes)
                .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
            postings.insert(hash, bm);
        }

        let doc_count = id_map.len();
        Ok((field.clone(), Self { postings, id_map, doc_count, field, mapped: None, dead_slots: roaring::RoaringBitmap::new(), slot_of: None }))
    }

    /// Get the number of unique trigrams indexed.
    pub fn trigram_count(&self) -> usize {
        self.postings.len()
    }

    /// Get index statistics.
    pub fn stats(&self) -> GINStats {
        let total_postings: usize = self.postings.values().map(|bm| bm.len() as usize).sum();
        GINStats {
            doc_count: self.doc_count,
            field: self.field.clone(),
            trigram_count: self.postings.len(),
            total_postings,
        }
    }
}

/// Statistics about a GIN index.
pub struct GINStats {
    pub doc_count: usize,
    pub field: String,
    pub trigram_count: usize,
    pub total_postings: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ilike_pattern_extraction() {
        // %Alpha% — both sides wildcarded: only interior trigrams, no space padding
        let pattern = "%Alpha%";
        let trigrams = extract_pattern_trigrams(pattern);
        assert!(!trigrams.is_empty());
        assert!(trigrams.contains(&"alp".to_string()));
        assert!(trigrams.contains(&"lph".to_string()));
        assert!(trigrams.contains(&"pha".to_string()));
        assert!(!trigrams.contains(&" al".to_string()), "space padding must not appear with leading %");
    }

    #[test]
    fn test_gin_build_and_query() {
        let docs = vec![
            (1u64, "Hello World"),
            (2u64, "The Vines"),
            (3u64, "Alpha Beta"),
            (4u64, "hello"),
        ];
        let index = GINIndex::build(docs.into_iter(), "text");

        // Test exact match
        let results = index.ilike("%hello%", None);
        assert!(results.contains(&1) || results.contains(&4)); // case insensitive

        // Test AND of trigrams
        let results = index.ilike("%Alpha Beta%", None);
        assert!(results.contains(&3));
    }

    /// GIN must return the original u64 hash unmodified even when it exceeds u32::MAX.
    /// Previously, hashes were truncated to u32 during build and zero-extended on
    /// query, producing wrong IDs and empty results.
    #[test]
    fn test_gin_large_hashes() {
        // Hashes above u32::MAX — would be silently truncated by the old `doc_id as u32`.
        let big_id_a: u64 = u64::from(u32::MAX) + 1;   // 4_294_967_296
        let big_id_b: u64 = u64::from(u32::MAX) + 999;  // 4_294_968_294

        let docs = vec![
            (big_id_a, "Melbourne Fitzroy"),
            (big_id_b, "Maribyrnong flooding event"),
            (1u64, "something else entirely"),
        ];
        let index = GINIndex::build(docs.into_iter(), "name");

        // Query for "Fitzroy" — must return big_id_a, not 0 (the truncated form).
        let results = index.ilike("%fitzroy%", None);
        assert_eq!(results, vec![big_id_a], "large hash must not be truncated");

        // Query for "Maribyrnong" — must return big_id_b.
        let results = index.ilike("%maribyrnong%", None);
        assert_eq!(results, vec![big_id_b], "second large hash must round-trip correctly");

        // Ensure the small-ID doc is still reachable.
        let results = index.ilike("%something%", None);
        assert_eq!(results, vec![1u64]);
    }
}
