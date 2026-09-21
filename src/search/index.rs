//! # The positional search index — words, positions, and ranking
//!
//! This builds and serves the index behind `SEARCH('query')`. It is an *inverted
//! index*: instead of scanning documents, it stores, for each term, the list of
//! documents that contain it — plus **where** in each document (word positions),
//! which is what lets it reward phrase proximity and exact matches.
//!
//! ## How it's laid out (and the clever parts)
//!
//! - **Term dictionary as an FST.** All the distinct terms are stored in a
//!   *Finite State Transducer* (the `fst` crate) — a compressed, sorted map from
//!   term → id. It shares the common prefixes/suffixes of words, so a huge
//!   vocabulary stays tiny, and it supports fast prefix and fuzzy lookups.
//! - **Postings.** For each term id, a compact list of (document, positions).
//!   Document sets use [`RoaringBitmap`] (a compressed integer set) for fast
//!   intersection; positions drive the proximity ranking.
//! - **Disk-first split.** The two bulk blobs (FST + postings) are held behind a
//!   [`Bytes`] type that is EITHER owned on the heap (resident mode) OR a slice of
//!   an mmap'd file (paged mode) — same code path, the memory just lives on disk.
//!   The persistence itself is in the sibling `disk.rs`.
//!
//! ## Core components
//!
//! - [`SearchIndex`] — the whole index for one field group.
//! - [`Bytes`] / [`IdMap`] / [`Norms`] / [`SlotIndex`] — the resident-vs-mmap
//!   backing types that make the index disk-first.
//! - `score()` — applies the cascade ranking (see `ranking.rs`).

use roaring::RoaringBitmap;
use std::collections::HashMap;
use std::sync::Arc;
use fst::IntoStreamer;

use crate::bm25::tokenizer::tokenize_with_positions;
use crate::storage::mmap::MmapView;

const POSITION_BUCKET_SIZE: usize = 8;

/// Backing for the two bulk blobs (FST term dict + postings). Either owned on the
/// heap (resident mode, `CoreDB::new` / non-paged open) or a range of an mmap'd
/// `search.bin` (paged/disk-first mode) — the disk-first substrate: the blobs stay
/// on disk (OS page cache holds hot pages) instead of being read into RAM. Several
/// blobs of the same file share one `Arc<MmapView>`.
#[derive(Clone)]
pub(crate) enum Bytes {
    Owned(Vec<u8>),
    Mapped { view: Arc<MmapView>, off: usize, len: usize },
}

impl Bytes {
    #[inline]
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Bytes::Owned(v) => v,
            Bytes::Mapped { view, off, len } => view.slice(*off, *len).unwrap_or(&[]),
        }
    }
    #[inline]
    pub(crate) fn len(&self) -> usize {
        match self {
            Bytes::Owned(v) => v.len(),
            Bytes::Mapped { len, .. } => *len,
        }
    }
}

/// Read one length-prefixed RoaringBitmap (`[len:u32 LE][bytes]`) from a blob.
pub(crate) fn read_bitmap_slice(data: &[u8], offset: usize) -> Option<RoaringBitmap> {
    let lb = data.get(offset..offset + 4)?;
    let len = u32::from_le_bytes(lb.try_into().ok()?) as usize;
    let bytes = data.get(offset + 4..offset + 4 + len)?;
    RoaringBitmap::deserialize_from(bytes).ok()
}

/// The shared disk-first posting-store primitive: an FST (`key → u64 offset`) plus
/// a blob of length-prefixed RoaringBitmaps, both `Bytes`-backed (heap resident, or
/// a slice of an mmap'd `search.bin`). Used for the field-scoped and position
/// (proximity) bitmaps so they need not be read into RAM in paged mode. The base
/// term postings (`fst_data`/`postings_data`) follow the same shape.
#[derive(Clone)]
pub(crate) struct MappedPostings {
    pub(crate) fst: Bytes,
    pub(crate) blob: Bytes,
}

impl MappedPostings {
    /// Build from keyed bitmaps. `key_bytes` maps each entry key to its FST byte
    /// key; keys must be unique. Produces heap-`Owned` blobs (paged open rebinds
    /// them to `Bytes::Mapped`).
    fn build<K>(entries: &HashMap<K, RoaringBitmap>, key_bytes: impl Fn(&K) -> Vec<u8>) -> Self {
        let mut keyed: Vec<(Vec<u8>, &RoaringBitmap)> =
            entries.iter().map(|(k, v)| (key_bytes(k), v)).collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));

        let mut blob = Vec::new();
        let mut builder = fst::MapBuilder::memory();
        for (k, bm) in &keyed {
            builder.insert(k, blob.len() as u64).unwrap();
            let mut bytes = Vec::new();
            bm.serialize_into(&mut bytes).unwrap();
            blob.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            blob.extend_from_slice(&bytes);
        }
        MappedPostings {
            fst: Bytes::Owned(builder.into_inner().unwrap()),
            blob: Bytes::Owned(blob),
        }
    }

    /// Exact point lookup.
    fn get(&self, key: &[u8]) -> Option<RoaringBitmap> {
        let map = fst::Map::new(self.fst.as_slice()).ok()?;
        let off = map.get(key)? as usize;
        read_bitmap_slice(self.blob.as_slice(), off)
    }

    /// All `(key, bitmap)` whose FST key starts with `prefix`, in key order.
    fn range_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, RoaringBitmap)> {
        use fst::{IntoStreamer, Streamer};
        let map = match fst::Map::new(self.fst.as_slice()) { Ok(m) => m, Err(_) => return Vec::new() };
        let blob = self.blob.as_slice();
        let mut out = Vec::new();
        let mut stream = map.range().ge(prefix).into_stream();
        while let Some((k, off)) = stream.next() {
            if !k.starts_with(prefix) { break; }
            if let Some(bm) = read_bitmap_slice(blob, off as usize) {
                out.push((k.to_vec(), bm));
            }
        }
        out
    }
}

/// FST key for a field-scoped bitmap: `term \0 [field:u8]`.
fn field_key(term: &str, field: u8) -> Vec<u8> {
    let mut k = Vec::with_capacity(term.len() + 2);
    k.extend_from_slice(term.as_bytes());
    k.push(0);
    k.push(field);
    k
}

/// FST key for a position bitmap: `term \0 [bucket:u16 BE]` (BE so buckets sort
/// numerically within a term for prefix range scans).
fn position_key(term: &str, bucket: u16) -> Vec<u8> {
    let mut k = Vec::with_capacity(term.len() + 3);
    k.extend_from_slice(term.as_bytes());
    k.push(0);
    k.extend_from_slice(&bucket.to_be_bytes());
    k
}

/// Prefix selecting every position key for a term: `term \0`.
fn position_prefix(term: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(term.len() + 1);
    k.extend_from_slice(term.as_bytes());
    k.push(0);
    k
}

/// Decode the bucket (trailing BE u16) from a position key `term \0 [bucket]`.
fn bucket_from_key(k: &[u8]) -> u16 {
    let n = k.len();
    if n >= 2 { u16::from_be_bytes([k[n - 2], k[n - 1]]) } else { 0 }
}

/// Reverse index hash → slot. Resident mode keeps the `HashMap` (fast, no
/// regression). Paged mode serves it from a sorted `(hash:u64, slot:u32)` array in
/// the mmap'd `search.bin` (12 B/rec, binary search) — so the ~O(N) HashMap (the
/// largest resident piece of a mapped SEARCH index) never enters the heap.
#[derive(Clone)]
pub(crate) enum SlotIndex {
    Resident(HashMap<u64, u32>),
    Mapped(Bytes),
}
impl SlotIndex {
    #[inline]
    pub(crate) fn get(&self, hash: u64) -> Option<u32> {
        match self {
            SlotIndex::Resident(m) => m.get(&hash).copied(),
            SlotIndex::Mapped(b) => {
                let data = b.as_slice();
                let n = data.len() / 12;
                let (mut lo, mut hi) = (0isize, n as isize - 1);
                while lo <= hi {
                    let mid = ((lo + hi) / 2) as usize;
                    let o = mid * 12;
                    let h = u64::from_le_bytes(data[o..o + 8].try_into().ok()?);
                    if h == hash {
                        return u32::from_le_bytes(data[o + 8..o + 12].try_into().ok()?).into();
                    } else if h < hash { lo = mid as isize + 1; } else { hi = mid as isize - 1; }
                }
                None
            }
        }
    }
    pub(crate) fn remove(&mut self, hash: u64) {
        // Deletions only mutate the resident overlay; the mmap base is immutable
        // (deleted docs are excluded via node existence at query time, like GIN/BM25).
        if let SlotIndex::Resident(m) = self { m.remove(&hash); }
    }
}

/// slot → node hash. Resident `Vec<u64>` (heap mode, direct index — no regression) or
/// a u64 array served from the mmap'd `search.bin` (paged, O(N)/8 B-per-doc off heap).
#[derive(Clone)]
pub(crate) enum IdMap {
    Owned(Vec<u64>),
    Mapped { view: std::sync::Arc<MmapView>, off: usize, count: usize },
}
impl IdMap {
    #[inline]
    pub(crate) fn get(&self, slot: usize) -> Option<u64> {
        match self {
            IdMap::Owned(v) => v.get(slot).copied(),
            IdMap::Mapped { view, off, count } => {
                if slot >= *count { return None; }
                let s = view.slice(off + slot * 8, 8)?;
                Some(u64::from_le_bytes(s.try_into().ok()?))
            }
        }
    }
    #[inline]
    pub(crate) fn count(&self) -> usize {
        match self { IdMap::Owned(v) => v.len(), IdMap::Mapped { count, .. } => *count }
    }
}

/// Per-doc field lengths (norms), doc-major `num_fields` u16 each. Resident jagged
/// `Vec<Vec<u16>>` (borrowed slice, no regression) or a flat u16 array on the mmap
/// (paged; O(N·fields)/2 B off heap). Only read by SEARCH_SCORE ranking.
#[derive(Clone)]
pub(crate) enum Norms {
    Owned(Vec<Vec<u16>>),
    Mapped { view: std::sync::Arc<MmapView>, off: usize, doc_count: usize, num_fields: usize },
}
impl Norms {
    #[inline]
    pub(crate) fn doc_lengths(&self, slot: usize) -> Option<std::borrow::Cow<'_, [u16]>> {
        match self {
            Norms::Owned(v) => v.get(slot).map(|l| std::borrow::Cow::Borrowed(l.as_slice())),
            Norms::Mapped { view, off, doc_count, num_fields } => {
                if slot >= *doc_count { return None; }
                let bytes = view.slice(off + slot * num_fields * 2, num_fields * 2)?;
                Some(std::borrow::Cow::Owned(
                    bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()))
            }
        }
    }
}

#[derive(Clone)]
pub struct SearchIndex {
    pub(crate) fields: Vec<String>,
    pub(crate) id_map: IdMap,
    pub(crate) id_to_slot: SlotIndex,
    pub(crate) doc_count: u32,
    pub(crate) doc_field_lengths: Norms,
    /// FST mapping term → byte offset into `postings_data`.
    pub(crate) fst_data: Bytes,
    /// Contiguous serialized RoaringBitmaps: at each offset, [len: u32 LE][bitmap bytes].
    pub(crate) postings_data: Bytes,
    /// Field-scoped bitmaps keyed by `term \0 field` (for field-order ranking).
    pub(crate) field_post: MappedPostings,
    /// Position/proximity bitmaps keyed by `term \0 bucket` (for proximity ranking).
    pub(crate) position_post: MappedPostings,
    /// Documents written since the last full build, held as their own segment.
    ///
    /// The term dictionary is an FST — genuinely immutable — so a new document
    /// cannot be threaded into it. Rebuilding instead made one INSERT cost
    /// `O(corpus)`: 144 ms at 20 000 rows, growing with the table. New documents
    /// go into this second, small index and queries read both.
    ///
    /// Unlike BM25 this needs no statistical stitching: [`score`] is computed
    /// entirely from one document (matched terms, edit distance, field order,
    /// proximity) with no corpus-wide term statistics, so a document scores the
    /// same whichever segment holds it.
    ///
    /// [`score`]: SearchIndex::score
    pub(crate) deltas: Vec<SearchSegment>,
    /// Files of segments a level merge absorbed. Deleted only once their
    /// replacement is durable.
    pub(crate) retired_segment_files: Vec<std::path::PathBuf>,
    /// Monotonic, never reused.
    pub(crate) next_segment_id: u64,
}

/// How many flushed segments may share a level before they merge into one at the
/// next level up. Same cost model as BM25's `SEG_FANOUT`: each document is
/// rebuilt about once per level, and there are `log_FANOUT(N)` levels.
pub(crate) const SEARCH_SEG_FANOUT: usize = 8;

/// The most documents one merge may hold at once.
///
/// # Why a cap exists at all
///
/// Merging rebuilds an FST, and an FST is built from all of its terms — so a
/// merge holds every source row of every input segment in memory at the same
/// time. Level on level that compounds: eight level-1 segments of 200 000 rows
/// is 1.6 million documents resident, and it grows with the store.
///
/// Measured with a counting allocator over a two-million-row load: peak heap
/// 809 MB with a search index against 116-178 MB for every other index family.
/// At fifty million rows that shape is an OOM, not a slowdown — and Law 1 says
/// no operation may hold RAM proportional to the size of the database.
///
/// So a merge whose inputs exceed this is not performed. The segment stays as it
/// is, sealed, and queries consult it alongside the others.
///
/// # Sacrifice
///
/// Segment count stops being `O(log N)` and becomes `O(N / cap)` — about 125
/// segments at fifty million rows — so a query performs that many more lookups.
/// That is the price of a bounded merge, and it is the right way round: a slower
/// query is a cost, an OOM is a failure.
///
/// The way out is a streaming merge over sorted term runs rather than a rebuild
/// from source rows, which is what SQLite's FTS5 does. That removes the cap
/// instead of tuning it, and is the proper fix.
pub(crate) const SEARCH_MERGE_MAX_DOCS: u32 = 400_000;

/// One flushed batch, held as its own immutable index.
///
/// # Why this exists
///
/// The delta used to be a single index rebuilt from every buffered document on
/// every write. That made a load quadratic — N^1.79 measured over a 20k/50k/150k
/// ladder — because the FST is immutable, so "add a document" meant "build the
/// whole delta again", over a delta that kept growing.
///
/// A batch now becomes one of these and is never rebuilt. Segments merge with
/// each other by level, so the cost of a write is the cost of that write.
/// A segment's source rows: in RAM until the segment is written, on disk after.
///
/// Merging a segment means rebuilding an FST, which needs the text again — so the
/// rows have to survive somewhere. Keeping them resident would put every indexed
/// document's text back in memory, which is the disk-first bargain broken. They
/// live in the segment's own file and are read back only when a merge actually
/// needs them.
#[derive(Clone)]
pub(crate) enum SegDocs {
    Resident(Vec<DocFields>),
    OnDisk { path: std::path::PathBuf, off: usize },
}

impl SegDocs {
    /// The rows, read from disk if that is where they are. O(segment), and only
    /// at merge time.
    pub(crate) fn load(&self) -> Vec<DocFields> {
        match self {
            SegDocs::Resident(v) => v.clone(),
            SegDocs::OnDisk { path, off } => {
                let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
                crate::search::disk::decode_docs(&bytes, *off)
            }
        }
    }

    pub(crate) fn len_hint(&self) -> usize {
        match self {
            SegDocs::Resident(v) => v.len(),
            SegDocs::OnDisk { .. } => 0,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SearchSegment {
    pub(crate) level: u32,
    /// Too large to merge again within the RAM cap, so it is left alone.
    ///
    /// A flag rather than a level, because bumping the level to mean "sealed"
    /// puts the same segments at the next level where they are still too large —
    /// which bumps them again, forever. That loop hung a two-million-row load.
    pub(crate) sealed: bool,
    /// Identifies this segment's file. Monotonic, never reused.
    pub(crate) id: u64,
    pub(crate) index: Box<SearchIndex>,
    /// The rows that built it, so segments merge without asking the database for
    /// the text again. Deleted rows are dropped at merge time, not on delete,
    /// which keeps a delete O(segments) rather than O(documents).
    pub(crate) docs: SegDocs,
    /// Where this segment lives once written. `None` while RAM-only.
    pub(crate) persisted: Option<std::path::PathBuf>,
}

#[derive(Clone)]
pub struct DocFields {
    pub hash: u64,
    pub field_values: Vec<String>,
}

/// Auto-select max edit distance based on term length (matches Meilisearch rules).
fn auto_distance(term: &str) -> u32 {
    match term.len() {
        0..=4 => 0,
        5..=8 => 1,
        _ => 2,
    }
}

fn deduplicate_tokens(query: &str) -> Vec<String> {
    let tokens = tokenize_with_positions(query);
    let mut seen = std::collections::HashSet::new();
    tokens.into_iter()
        .filter_map(|(t, _)| if seen.insert(t.clone()) { Some(t) } else { None })
        .collect()
}

impl SearchIndex {
    pub fn build(fields: Vec<String>, docs: impl Iterator<Item = DocFields>) -> Self {
        let mut id_map = Vec::new();
        let mut id_to_slot = HashMap::new();
        let mut doc_field_lengths: Vec<Vec<u16>> = Vec::new();
        let mut term_bitmaps: HashMap<String, RoaringBitmap> = HashMap::new();
        let mut term_field_bitmaps: HashMap<(String, u8), RoaringBitmap> = HashMap::new();
        let mut term_position_bitmaps: HashMap<(String, u16), RoaringBitmap> = HashMap::new();

        let num_fields = fields.len();

        for doc in docs {
            let slot = id_map.len() as u32;
            id_to_slot.insert(doc.hash, slot);
            id_map.push(doc.hash);

            let mut lengths = Vec::with_capacity(num_fields);
            let mut global_pos: usize = 0;

            for (field_idx, text) in doc.field_values.iter().enumerate() {
                let tokens = tokenize_with_positions(text);
                lengths.push(tokens.len().min(u16::MAX as usize) as u16);

                for (term, _local_pos) in &tokens {
                    term_bitmaps.entry(term.clone())
                        .or_default()
                        .insert(slot);

                    term_field_bitmaps.entry((term.clone(), field_idx as u8))
                        .or_default()
                        .insert(slot);

                    let bucket = (global_pos / POSITION_BUCKET_SIZE).min(u16::MAX as usize) as u16;
                    term_position_bitmaps.entry((term.clone(), bucket))
                        .or_default()
                        .insert(slot);

                    global_pos += 1;
                }
            }

            doc_field_lengths.push(lengths);
        }

        // Build FST + postings blob from the HashMap
        let mut sorted_terms: Vec<&String> = term_bitmaps.keys().collect();
        sorted_terms.sort();

        let mut postings_data = Vec::new();
        let mut fst_builder = fst::MapBuilder::memory();

        for term in &sorted_terms {
            let offset = postings_data.len() as u64;
            fst_builder.insert(term.as_bytes(), offset).unwrap();

            let bm = &term_bitmaps[*term];
            let mut bm_bytes = Vec::new();
            bm.serialize_into(&mut bm_bytes).unwrap();
            postings_data.extend_from_slice(&(bm_bytes.len() as u32).to_le_bytes());
            postings_data.extend_from_slice(&bm_bytes);
        }

        let fst_data = fst_builder.into_inner().unwrap();
        let doc_count = id_map.len() as u32;

        let field_post = MappedPostings::build(&term_field_bitmaps, |(t, f)| field_key(t, *f));
        let position_post = MappedPostings::build(&term_position_bitmaps, |(t, b)| position_key(t, *b));

        SearchIndex {
            fields,
            id_map: IdMap::Owned(id_map),
            id_to_slot: SlotIndex::Resident(id_to_slot),
            doc_count,
            doc_field_lengths: Norms::Owned(doc_field_lengths),
            fst_data: Bytes::Owned(fst_data),
            postings_data: Bytes::Owned(postings_data),
            field_post,
            position_post,
            deltas: Vec::new(),
            retired_segment_files: Vec::new(),
            next_segment_id: 0,
        }
    }

    /// Read a bitmap from the postings blob at the given byte offset.
    fn read_bitmap_at(&self, offset: usize) -> Option<RoaringBitmap> {
        let data = self.postings_data.as_slice();
        if offset + 4 > data.len() {
            return None;
        }
        let len = u32::from_le_bytes(
            data[offset..offset + 4].try_into().ok()?
        ) as usize;
        if offset + 4 + len > data.len() {
            return None;
        }
        RoaringBitmap::deserialize_from(&data[offset + 4..offset + 4 + len]).ok()
    }

    /// Exact term lookup via FST.
    fn get_bitmap(&self, term: &str) -> Option<RoaringBitmap> {
        let map = fst::Map::new(self.fst_data.as_slice()).ok()?;
        let offset = map.get(term)? as usize;
        self.read_bitmap_at(offset)
    }

    /// Fuzzy term lookup via Levenshtein automaton.
    /// Returns the OR of all bitmaps for terms within `max_dist` edits.
    fn search_fuzzy(&self, term: &str, max_dist: u32) -> RoaringBitmap {
        self.search_fuzzy_with_terms(term, max_dist).0
    }

    /// Fuzzy search returning both the union bitmap and the matched FST term strings.
    fn search_fuzzy_with_terms(&self, term: &str, max_dist: u32) -> (RoaringBitmap, Vec<String>) {
        if max_dist == 0 {
            return match self.get_bitmap(term) {
                Some(bm) => (bm, vec![term.to_string()]),
                None => (RoaringBitmap::new(), vec![]),
            };
        }
        let map = match fst::Map::new(self.fst_data.as_slice()) {
            Ok(m) => m,
            Err(_) => return (RoaringBitmap::new(), vec![]),
        };
        let lev = match fst::automaton::Levenshtein::new(term, max_dist) {
            Ok(l) => l,
            Err(_) => return match self.get_bitmap(term) {
                Some(bm) => (bm, vec![term.to_string()]),
                None => (RoaringBitmap::new(), vec![]),
            },
        };
        use fst::Streamer;
        let mut stream = map.search(&lev).into_stream();
        let mut result = RoaringBitmap::new();
        let mut matched_terms = Vec::new();
        while let Some((bytes, offset)) = stream.next() {
            if let Some(bm) = self.read_bitmap_at(offset as usize) {
                result |= bm;
            }
            if let Ok(s) = std::str::from_utf8(bytes) {
                matched_terms.push(s.to_string());
            }
        }
        (result, matched_terms)
    }

    /// AND intersection with typo tolerance: returns bitmap of slots matching ALL query terms.
    pub fn search(&self, query: &str) -> RoaringBitmap {
        self.search_typo(query, None)
    }

    /// Same as [`search`], but `typo` overrides the per-word auto edit-distance
    /// (`SEARCH('q', typo => N)`). `None` = the auto policy (Meili-style: 0 for words
    /// < 5 chars, 1 for 5–8, 2 for 9+). Exact matches are preferred; a term that isn't
    /// found exactly is fuzzy-expanded to within its edit distance.
    pub fn search_typo(&self, query: &str, typo: Option<u32>) -> RoaringBitmap {
        let mut hits = self.search_typo_segment(query, typo);
        for (base, d) in self.delta_segments() {
            for slot in d.search_typo_segment(query, typo) {
                hits.insert(slot + base);
            }
        }
        hits
    }

    /// `search_typo` against this segment alone — the base half of the union.
    fn search_typo_segment(&self, query: &str, typo: Option<u32>) -> RoaringBitmap {
        let unique_terms = deduplicate_tokens(query);
        if unique_terms.is_empty() {
            return RoaringBitmap::new();
        }

        let mut result: Option<RoaringBitmap> = None;
        for term in &unique_terms {
            let dist = typo.unwrap_or_else(|| auto_distance(term));
            let bm = match self.get_bitmap(term) {
                Some(bm) if !bm.is_empty() => bm,
                _ => self.search_fuzzy(term, dist),
            };

            if bm.is_empty() {
                return RoaringBitmap::new();
            }

            result = Some(match result {
                Some(acc) => acc & bm,
                None => bm,
            });
        }

        result.unwrap_or_default()
    }

    /// Cascade score: words → typo → proximity → field_order → exactness.
    /// Returns a composite f64 where higher = better ranking. Each rule occupies
    /// a separate magnitude band so a better words score always beats a worse one
    /// regardless of lower-tier rules.
    pub fn score(&self, query: &str, slot: u32) -> f64 {
        if slot >= self.delta_slot_base() {
            return match self.locate(slot) {
                Some((d, local)) => d.score_segment(query, local),
                None => 0.0,
            };
        }
        self.score_segment(query, slot)
    }

    /// `score` against this segment alone. Safe to compute independently: the
    /// cascade reads only per-document signals, never corpus statistics.
    fn score_segment(&self, query: &str, slot: u32) -> f64 {
        let terms = deduplicate_tokens(query);
        if terms.is_empty() { return 0.0; }

        let num_terms = terms.len();
        let mut matched_count = 0u32;
        let mut total_edits = 0u32;
        let mut best_field_idx = self.fields.len();
        let mut matched_fst_terms: Vec<Vec<String>> = Vec::with_capacity(num_terms);

        for term in &terms {
            let max_dist = auto_distance(term);

            // Exact match
            if let Some(bm) = self.get_bitmap(term) {
                if bm.contains(slot) {
                    matched_count += 1;
                    matched_fst_terms.push(vec![term.clone()]);
                    for fi in 0..self.fields.len() {
                        if self.field_post.get(&field_key(term, fi as u8))
                            .map_or(false, |b| b.contains(slot)) {
                            best_field_idx = best_field_idx.min(fi);
                            break;
                        }
                    }
                    continue;
                }
            }

            // Fuzzy d=1
            if max_dist >= 1 {
                let (bm, fst_terms) = self.search_fuzzy_with_terms(term, 1);
                if bm.contains(slot) {
                    matched_count += 1;
                    total_edits += 1;
                    for ft in &fst_terms {
                        if best_field_idx == 0 { break; }
                        for fi in 0..self.fields.len() {
                            if self.field_post.get(&field_key(ft, fi as u8))
                                .map_or(false, |b| b.contains(slot)) {
                                best_field_idx = best_field_idx.min(fi);
                                break;
                            }
                        }
                    }
                    matched_fst_terms.push(fst_terms);
                    continue;
                }
            }

            // Fuzzy d=2
            if max_dist >= 2 {
                let (bm, fst_terms) = self.search_fuzzy_with_terms(term, 2);
                if bm.contains(slot) {
                    matched_count += 1;
                    total_edits += 2;
                    for ft in &fst_terms {
                        if best_field_idx == 0 { break; }
                        for fi in 0..self.fields.len() {
                            if self.field_post.get(&field_key(ft, fi as u8))
                                .map_or(false, |b| b.contains(slot)) {
                                best_field_idx = best_field_idx.min(fi);
                                break;
                            }
                        }
                    }
                    matched_fst_terms.push(fst_terms);
                    continue;
                }
            }

            matched_fst_terms.push(vec![]);
        }

        if matched_count == 0 { return 0.0; }

        let words = matched_count as f64 / num_terms as f64;
        let typo = 1.0 - (total_edits as f64 / (matched_count as f64 * 2.0));
        let proximity = self.cascade_proximity(&matched_fst_terms, slot);
        let field_order = if self.fields.len() <= 1 || best_field_idx >= self.fields.len() {
            1.0
        } else {
            1.0 - (best_field_idx as f64 / (self.fields.len() as f64 - 1.0))
        };
        let exactness = self.cascade_exactness(&terms, slot);

        words * 1e12 + typo * 1e9 + proximity * 1e6 + field_order * 1e3 + exactness
    }

    fn cascade_proximity(&self, matched_fst_terms: &[Vec<String>], slot: u32) -> f64 {
        if matched_fst_terms.len() < 2 { return 1.0; }

        let mut total = 0.0;
        let mut pairs = 0u32;

        for i in 0..matched_fst_terms.len() - 1 {
            let ta = &matched_fst_terms[i];
            let tb = &matched_fst_terms[i + 1];
            if ta.is_empty() || tb.is_empty() { continue; }

            let buckets_a: Vec<u16> = ta.iter().flat_map(|t| {
                self.position_post.range_prefix(&position_prefix(t)).into_iter()
                    .filter(|(_, bm)| bm.contains(slot))
                    .map(|(k, _)| bucket_from_key(&k))
            }).collect();

            let buckets_b: Vec<u16> = tb.iter().flat_map(|t| {
                self.position_post.range_prefix(&position_prefix(t)).into_iter()
                    .filter(|(_, bm)| bm.contains(slot))
                    .map(|(k, _)| bucket_from_key(&k))
            }).collect();

            if buckets_a.is_empty() || buckets_b.is_empty() { continue; }

            let min_dist = buckets_a.iter()
                .flat_map(|a| buckets_b.iter().map(move |b| (*a as i32 - *b as i32).unsigned_abs()))
                .min()
                .unwrap_or(u32::MAX);

            total += 1.0 / (1.0 + min_dist as f64);
            pairs += 1;
        }

        if pairs == 0 { return 0.5; }
        total / pairs as f64
    }

    fn cascade_exactness(&self, query_terms: &[String], slot: u32) -> f64 {
        let qlen = query_terms.len() as u16;
        if let Some(lengths) = self.doc_field_lengths.doc_lengths(slot as usize) {
            for &flen in lengths.iter() {
                if flen == qlen { return 1.0; }
            }
            if let Some(&min_len) = lengths.iter().filter(|&&l| l > 0).min() {
                if min_len > qlen {
                    return qlen as f64 / min_len as f64;
                }
            }
        }
        0.0
    }

    /// Slot numbers of delta documents start here, so the two segments share one
    /// flat slot space and callers never learn there are two of them.
    #[inline]
    fn delta_slot_base(&self) -> u32 { self.doc_count }

    /// How many documents are waiting in the delta.
    /// Resident bytes held by this index — the heap side only.
    ///
    /// `Bytes` is either owned or a window on the mapping; only the owned case
    /// is counted, because a mapped one is page cache the kernel can reclaim.
    /// That distinction is the whole point of the number: a mapped index and a
    /// resident one can be the same size on disk and nothing alike in memory.
    pub fn mem_bytes(&self) -> usize {
        let owned = |b: &Bytes| -> usize {
            match b {
                Bytes::Owned(v) => v.capacity(),
                _ => 0,
            }
        };
        let fields: usize = self.fields.iter().map(|f| f.capacity()).sum::<usize>()
            + self.fields.capacity() * 24;
        let id_map = match &self.id_map {
            IdMap::Owned(v) => v.capacity() * 8,
            IdMap::Mapped { .. } => 0,
        };
        let id_to_slot = match &self.id_to_slot {
            SlotIndex::Resident(m) => m.capacity() * (8 + 4 + 1),
            SlotIndex::Mapped(_) => 0,
        };
        fields
            + id_map
            + id_to_slot
            + owned(&self.fst_data)
            + owned(&self.postings_data)
            + self.deltas.iter()
                .map(|s| s.index.mem_bytes() + s.docs.len_hint() * 32)
                .sum::<usize>()
    }

    pub fn delta_len(&self) -> usize {
        self.deltas.iter().map(|s| s.index.doc_count as usize).sum()
    }

    /// Segments that exist only in RAM and would be lost on close.
    pub fn unpersisted_segments(&self) -> usize {
        self.deltas.iter().filter(|s| s.persisted.is_none()).count()
    }

    /// `(first_slot, segment)` per flushed segment. Segment slot ranges are laid
    /// end to end after the base, so a slot identifies exactly one of them.
    fn delta_segments(&self) -> impl Iterator<Item = (u32, &SearchIndex)> {
        let mut next = self.doc_count;
        self.deltas.iter().map(move |seg| {
            let base = next;
            next += seg.index.doc_count;
            (base, seg.index.as_ref())
        })
    }

    /// The segment holding `slot`, and the slot's index within it.
    fn locate(&self, slot: u32) -> Option<(&SearchIndex, u32)> {
        self.delta_segments()
            .find(|(base, d)| slot >= *base && slot < base + d.doc_count)
            .map(|(base, d)| (d, slot - base))
    }

    /// Index one document without rebuilding the corpus.
    ///
    /// The delta segment is rebuilt, not the whole index — its cost is bounded by
    /// the number of documents written since the last merge, not by table size.
    /// Re-inserting a hash replaces it.
    pub fn insert_doc(&mut self, doc: DocFields) {
        self.insert_docs(std::iter::once(doc));
    }

    /// Apply many documents, building the delta index **once**.
    ///
    /// # Why this exists
    ///
    /// [`insert_doc`] rebuilds the whole delta on every call. Applying M
    /// documents one at a time therefore builds the delta M times, over 1, 2,
    /// ... M documents — an O(M²) pile of FST constructions, plus an O(M²)
    /// pile of `retain` scans. It is the same shape as rebuilding a table on
    /// every INSERT.
    ///
    /// Measured with 5 000-row batches and a search index alongside any second
    /// text index, successive batches cost 24 s, 62 s and 98 s while the corpus
    /// was still under 20 000 rows. SQLite loads 500 000 rows into FTS5 in 6.7 s
    /// and stays flat. Pushing every document first and building once removes
    /// the entire quadratic term: the delta that results is identical, and so
    /// are the answers — only the number of builds changes.
    ///
    /// # Sacrifice
    ///
    /// Peak memory during the call now holds the whole incoming batch as
    /// `DocFields` before the build, rather than one document at a time. That is
    /// O(batch), not O(corpus), so it respects the rule that cost follows the
    /// change and not the store; the caller already bounds the batch.
    ///
    /// [`insert_doc`]: SearchIndex::insert_doc
    pub fn insert_docs(&mut self, docs: impl IntoIterator<Item = DocFields>) {
        let incoming: Vec<DocFields> = docs.into_iter().collect();
        if incoming.is_empty() {
            return;
        }
        // Last write wins for a hash repeated within one batch, which is what a
        // run of `insert_doc` calls would have left behind.
        let mut last: HashMap<u64, usize> = HashMap::with_capacity(incoming.len());
        for (i, d) in incoming.iter().enumerate() {
            last.insert(d.hash, i);
        }
        // Retires any previous copy, in the base and in every segment, so a
        // document exists in exactly one place.
        for &hash in last.keys() {
            self.delete(hash);
        }
        let batch: Vec<DocFields> = incoming
            .into_iter()
            .enumerate()
            .filter(|(i, d)| last.get(&d.hash) == Some(i))
            .map(|(_, d)| d)
            .collect();
        self.flush_to_segment(batch);
    }

    /// Build one segment from this batch and merge levels. O(batch).
    fn flush_to_segment(&mut self, batch: Vec<DocFields>) {
        if batch.is_empty() {
            return;
        }
        let index = Box::new(SearchIndex::build(self.fields.clone(), batch.iter().cloned()));
        self.deltas.push(SearchSegment {
            level: 0,
            sealed: false,
            id: 0,
            index,
            docs: SegDocs::Resident(batch),
            persisted: None,
        });
        self.merge_segment_levels();
    }

    /// Merge segments upward whenever `SEARCH_SEG_FANOUT` share a level, so the
    /// number a query must consult stays `O(FANOUT x log N)` instead of growing
    /// with the number of writes.
    fn merge_segment_levels(&mut self) {
        loop {
            let mut counts: HashMap<u32, usize> = HashMap::new();
            for seg in self.deltas.iter().filter(|s| !s.sealed) {
                *counts.entry(seg.level).or_default() += 1;
            }
            let level = match counts
                .iter()
                .filter(|(_, c)| **c >= SEARCH_SEG_FANOUT)
                .map(|(l, _)| *l)
                .min()
            {
                Some(l) => l,
                None => break,
            };
            let (to_merge, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.deltas)
                .into_iter()
                .partition(|seg| seg.level == level && !seg.sealed);
            // A merge holds every input's rows at once, so one that would exceed
            // the cap is refused: the segments stay as they are and are consulted
            // separately. Bumping the level marks them sealed so this is decided
            // once rather than retried on every flush.
            let would_hold: u32 = to_merge.iter().map(|s| s.index.doc_count).sum();
            if would_hold > SEARCH_MERGE_MAX_DOCS {
                self.deltas = rest;
                for mut seg in to_merge {
                    seg.sealed = true;
                    self.deltas.push(seg);
                }
                continue;
            }
            self.deltas = rest;
            // Deleted rows are dropped here. `delete` only clears `id_to_slot`,
            // so this is where a retired document actually stops costing space.
            let docs: Vec<DocFields> = to_merge
                .iter()
                .flat_map(|seg| {
                    seg.docs
                        .load()
                        .into_iter()
                        .filter(|d| seg.index.id_to_slot.get(d.hash).is_some())
                })
                .collect();
            // Their files outlive them until the replacement is durable (Law 3).
            for seg in &to_merge {
                if let Some(path) = &seg.persisted {
                    self.retired_segment_files.push(path.clone());
                }
            }
            if docs.is_empty() {
                continue;
            }
            let index = Box::new(SearchIndex::build(self.fields.clone(), docs.iter().cloned()));
            self.deltas.push(SearchSegment {
                level: level + 1,
                sealed: false,
                id: 0,
                index,
                docs: SegDocs::Resident(docs),
                persisted: None,
            });
        }
    }

    pub fn slot_to_hash(&self, slot: u32) -> Option<u64> {
        if slot >= self.delta_slot_base() {
            let (d, local) = self.locate(slot)?;
            return d.slot_to_hash(local);
        }
        let hash = self.id_map.get(slot as usize)?;
        // Liveness gate. `delete` only removes the hash from id_to_slot — the term
        // data is immutable and may be mmap-backed — so a deleted document keeps a
        // slot in every bitmap. Filtering here is what makes the deletion visible,
        // and is why a delete no longer needs a full rebuild.
        self.id_to_slot.get(hash)?;
        Some(hash)
    }

    pub fn hash_to_slot(&self, hash: u64) -> Option<u32> {
        if let Some(slot) = self.id_to_slot.get(hash) {
            return Some(slot);
        }
        for (base, d) in self.delta_segments() {
            if let Some(s) = d.hash_to_slot(hash) {
                return Some(s + base);
            }
        }
        None
    }

    pub fn delete(&mut self, hash: u64) {
        // All term data (FST, postings, field/position bitmaps) is immutable and may
        // be mmap-backed — deletion is tracked by removing the hash from id_to_slot.
        // Deleted docs are excluded at search time via id_to_slot; score() only runs
        // on live slots, so a stale slot lingering in a bitmap is harmless.
        self.id_to_slot.remove(hash);
        for seg in &mut self.deltas {
            seg.index.id_to_slot.remove(hash);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_docs() -> Vec<DocFields> {
        vec![
            DocFields {
                hash: 100,
                field_values: vec!["Rust Programming Language".into(), "Rust is fast and safe".into()],
            },
            DocFields {
                hash: 200,
                field_values: vec!["Python Guide".into(), "Python is easy to learn".into()],
            },
            DocFields {
                hash: 300,
                field_values: vec!["Rust and Python".into(), "Both languages are great".into()],
            },
        ]
    }

    #[test]
    fn search_single_term() {
        let idx = SearchIndex::build(
            vec!["title".into(), "body".into()],
            make_docs().into_iter(),
        );
        let results = idx.search("rust");
        assert!(results.contains(0)); // doc 100
        assert!(!results.contains(1)); // doc 200
        assert!(results.contains(2)); // doc 300
    }

    #[test]
    fn search_multi_term_and() {
        let idx = SearchIndex::build(
            vec!["title".into(), "body".into()],
            make_docs().into_iter(),
        );
        let results = idx.search("rust fast");
        assert!(results.contains(0)); // has both
        assert!(!results.contains(2)); // has rust but not fast
    }

    #[test]
    fn search_no_match() {
        let idx = SearchIndex::build(
            vec!["title".into(), "body".into()],
            make_docs().into_iter(),
        );
        let results = idx.search("javascript");
        assert!(results.is_empty());
    }

    #[test]
    fn score_cascade_ordering() {
        let idx = SearchIndex::build(
            vec!["title".into(), "body".into()],
            make_docs().into_iter(),
        );
        let s0 = idx.score("rust fast", 0); // doc 100: has both terms → words=1.0
        let s2 = idx.score("rust fast", 2); // doc 300: has only rust → words=0.5
        assert!(s0 > 0.0);
        assert!(s2 > 0.0);
        assert!(s0 > s2, "doc with all terms should rank higher");
    }

    #[test]
    fn score_cascade_typo_penalty() {
        let idx = SearchIndex::build(
            vec!["title".into()],
            vec![
                DocFields { hash: 100, field_values: vec!["Rust Programming Language".into()] },
            ].into_iter(),
        );
        let exact = idx.score("programming", 0);
        let typo = idx.score("programing", 0); // 1 edit
        assert!(exact > typo, "exact match should score higher than fuzzy");
    }

    #[test]
    fn score_cascade_field_order() {
        let idx = SearchIndex::build(
            vec!["title".into(), "body".into()],
            vec![
                DocFields { hash: 100, field_values: vec!["Rust Language".into(), "something else".into()] },
                DocFields { hash: 200, field_values: vec!["something else".into(), "Rust Language".into()] },
            ].into_iter(),
        );
        let s_title = idx.score("rust", 0); // "rust" in title (field 0)
        let s_body = idx.score("rust", 1);  // "rust" in body (field 1)
        assert!(s_title > s_body, "match in earlier field should rank higher");
    }

    #[test]
    fn score_cascade_proximity() {
        let idx = SearchIndex::build(
            vec!["body".into()],
            vec![
                DocFields { hash: 100, field_values: vec!["rust is fast".into()] },
                DocFields { hash: 200, field_values: vec!["rust programming language is very fast and safe".into()] },
            ].into_iter(),
        );
        let close = idx.score("rust fast", 0);  // "rust" and "fast" are 1 word apart
        let far = idx.score("rust fast", 1);    // "rust" and "fast" are many words apart
        assert!(close > far, "closer terms should rank higher");
    }

    #[test]
    fn score_cascade_exactness() {
        let idx = SearchIndex::build(
            vec!["title".into()],
            vec![
                DocFields { hash: 100, field_values: vec!["Rust Language".into()] },
                DocFields { hash: 200, field_values: vec!["Rust Programming Language Guide".into()] },
            ].into_iter(),
        );
        let exact = idx.score("rust language", 0);  // 2 query terms, title has 2 tokens
        let partial = idx.score("rust language", 1); // 2 query terms, title has 4 tokens
        assert!(exact > partial, "exact field length match should rank higher");
    }

    #[test]
    fn delete_removes_from_results() {
        let mut idx = SearchIndex::build(
            vec!["title".into(), "body".into()],
            make_docs().into_iter(),
        );
        assert!(idx.search("rust").contains(0));
        idx.delete(100);
        // Deleted doc should still appear in FST bitmap but be filtered by id_to_slot
        // Actually the bitmap isn't modified for FST — deletion is tracked via id_to_slot removal.
        // The search() method returns raw bitmap matches. Caller filters via slot_to_hash which
        // checks id_to_slot. Let's verify the slot is removed.
        assert!(idx.hash_to_slot(100).is_none());
    }

    #[test]
    fn field_bitmaps_populated() {
        let idx = SearchIndex::build(
            vec!["title".into(), "body".into()],
            make_docs().into_iter(),
        );
        let bm = idx.field_post.get(&field_key("rust", 0)).unwrap();
        assert!(bm.contains(0));
        assert!(bm.contains(2));
        let bm = idx.field_post.get(&field_key("fast", 1)).unwrap();
        assert!(bm.contains(0));
    }

    #[test]
    fn fuzzy_search_typo() {
        let idx = SearchIndex::build(
            vec!["title".into(), "body".into()],
            vec![
                DocFields {
                    hash: 100,
                    field_values: vec!["Rust Programming".into(), "Systems language".into()],
                },
                DocFields {
                    hash: 200,
                    field_values: vec!["Python Guide".into(), "Scripting language".into()],
                },
            ].into_iter(),
        );

        // Exact match works
        let results = idx.search("programming");
        assert!(results.contains(0));

        // Typo: "programing" (1 edit from "programming", 11 chars → max_dist=2)
        let results = idx.search("programing");
        assert!(results.contains(0), "fuzzy match should find 'programming' from 'programing'");

        // Typo: "xyzxyzxyz" (completely different) — should NOT match
        let results = idx.search("xyzxyzxyz");
        assert!(results.is_empty(), "completely unrelated term should not match");
    }

    #[test]
    fn fuzzy_search_short_term_no_typo() {
        let idx = SearchIndex::build(
            vec!["title".into()],
            vec![
                DocFields {
                    hash: 100,
                    field_values: vec!["Rust is fast".into()],
                },
            ].into_iter(),
        );

        // "rust" (4 chars) → max_dist=0, no fuzzy
        let results = idx.search("ruts");
        assert!(results.is_empty(), "4-char term should not fuzzy match");

        // "faste" (5 chars) → max_dist=1, fuzzy should find "fast"... but "fast" is 4 chars
        // Actually "faste" has 5 chars, edit distance from "fast" is 1 (insertion)
        let results = idx.search("faste");
        assert!(results.contains(0), "5-char term with 1 edit should fuzzy match");
    }
}
