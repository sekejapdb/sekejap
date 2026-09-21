//! # The BM25 index — building it and scoring queries
//!
//! This ties the BM25 pieces together (see the `bm25` module doc for what BM25
//! *is*): it tokenizes documents, builds the term dictionary + postings, tracks
//! per-document lengths, and computes the relevance score at query time. The
//! `DocLens`/`DocIdx` types are the disk-first split — the O(N) per-doc arrays can
//! be memory-mapped so a reopened index costs little RAM, while the small term
//! dictionary stays resident as the deliberate accelerator.
//!
//! BM25 full-text search index.
//!
//! Lightweight and Pi-friendly: the only allocations are the postings
//! blob, the term dictionary, the doc-length array, and the doc-ID
//! lookup map.  No background threads, no allocator pressure.
//!
//! # Storage layout per indexed field (1 M docs)
//!
//! | Structure | Approximate size |
//! |---|---|
//! | `postings_bytes` | 90–450 MB (scales with field length) |
//! | `doc_id_to_idx` HashMap | ~19 MB |
//! | `doc_lengths` Vec | 4 MB |
//! | `sum_doc_len` counter | 8 bytes |
//!
//! # Deletion model
//!
//! Deletion is **zero-copy**: [`Bm25Index::delete`] removes the
//! document's entry from `doc_id_to_idx` and updates the running stats
//! (`num_docs`, `sum_doc_len`) but leaves the corresponding
//! `doc_lengths` slot as an inert orphan (4 bytes).  Orphans are
//! reclaimed only on a full rebuild.
//!
//! Because `doc_id_to_idx` is the authority for which documents are
//! live, a deleted document's postings entries can never contribute to
//! a search score — `search` skips any posting whose `doc_id` has no
//! entry in `doc_id_to_idx`.
//!
//! Callers should also apply a secondary guard through the live node
//! map (see [`Bm25Index`] struct-level docs) to catch any edge cases
//! between index operations.
//!
//! # BM25 parameters
//!
//! | Parameter | Value | Effect |
//! |---|---|---|
//! | `k1` | 1.2 | Term-frequency saturation |
//! | `b` | 0.75 | Length normalisation strength |

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use std::sync::Arc;
use crate::storage::mmap::MmapView;
use super::dict::TermDict;
use super::postings::{decode_postings_from_bytes, encode_postings_to_file, Posting};

/// Per-doc token counts. Resident `Vec<u32>` (heap mode — no regression) or a mmap'd
/// u32 array (paged, O(N)·4 B off heap).
#[derive(Clone)]
pub enum DocLens {
    Owned(Vec<u32>),
    Mapped { view: Arc<MmapView>, off: usize, count: usize },
}
impl DocLens {
    #[inline]
    pub fn get(&self, idx: usize) -> u32 {
        match self {
            DocLens::Owned(v) => v.get(idx).copied().unwrap_or(0),
            DocLens::Mapped { view, off, count } => {
                if idx >= *count { return 0; }
                view.slice(off + idx * 4, 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).unwrap_or(0)
            }
        }
    }
    #[inline]
    pub fn len(&self) -> usize { match self { DocLens::Owned(v) => v.len(), DocLens::Mapped { count, .. } => *count } }
}

/// doc_id (node hash) → slot index. Resident `HashMap` (heap mode) or a mmap'd sorted
/// `(doc_id:u64, idx:u32)` array (paged, binary-searched) — the largest O(N) BM25 piece.
#[derive(Clone)]
pub enum DocIdx {
    Owned(HashMap<u64, usize>),
    Mapped { view: Arc<MmapView>, off: usize, count: usize },
}
impl DocIdx {
    #[inline]
    pub fn get(&self, doc_id: u64) -> Option<usize> {
        match self {
            DocIdx::Owned(m) => m.get(&doc_id).copied(),
            DocIdx::Mapped { view, off, count } => {
                let data = view.slice(*off, count * 12)?;
                let (mut lo, mut hi) = (0isize, *count as isize - 1);
                while lo <= hi {
                    let mid = ((lo + hi) / 2) as usize;
                    let o = mid * 12;
                    let id = u64::from_le_bytes(data[o..o + 8].try_into().ok()?);
                    if id == doc_id {
                        return Some(u32::from_le_bytes(data[o + 8..o + 12].try_into().ok()?) as usize);
                    } else if id < doc_id { lo = mid as isize + 1; } else { hi = mid as isize - 1; }
                }
                None
            }
        }
    }
    pub fn remove(&mut self, doc_id: u64) { if let DocIdx::Owned(m) = self { m.remove(&doc_id); } }
    pub fn len(&self) -> usize { match self { DocIdx::Owned(m) => m.len(), DocIdx::Mapped { count, .. } => *count } }
    pub fn sorted(&self) -> Vec<(u64, u32)> {
        match self {
            DocIdx::Owned(m) => {
                let mut v: Vec<(u64, u32)> = m.iter().map(|(&k, &i)| (k, i as u32)).collect();
                v.sort_unstable_by_key(|(k, _)| *k);
                v
            }
            // Read the mapped array straight through. Returning an empty list here
            // was catastrophic: merge_delta builds the surviving document set from
            // this, so on a paged database it rebuilt the index out of the delta
            // alone and silently discarded every document already in the base —
            // BM25 dropped from nine matches to two at the first compaction and
            // degraded further from there.
            DocIdx::Mapped { view, off, count } => {
                let Some(data) = view.slice(*off, count * 12) else { return Vec::new() };
                (0..*count)
                    .filter_map(|i| {
                        let o = i * 12;
                        let id = u64::from_le_bytes(data[o..o + 8].try_into().ok()?);
                        let slot = u32::from_le_bytes(data[o + 8..o + 12].try_into().ok()?);
                        Some((id, slot))
                    })
                    .collect()
            }
        }
    }

    /// Drop a document from the mapped array by materialising it first.
    ///
    /// `remove` was a no-op for the mapped variant, so deleting a document from a
    /// paged BM25 index did nothing at all.
    pub fn materialize(&mut self) {
        if let DocIdx::Mapped { .. } = self {
            let owned: HashMap<u64, usize> =
                self.sorted().into_iter().map(|(id, slot)| (id, slot as usize)).collect();
            *self = DocIdx::Owned(owned);
        }
    }
}

/// The compressed postings blob — the index's one large structure. Held either
/// in RAM (ephemeral DBs) or **on disk** (disk-first): term ranges are read via
/// `pread` at query time, so the blob never occupies process RAM. The term
/// dictionary + doc arrays stay resident either way (small, needed for scoring).
#[derive(Clone)]
enum PostingsBlob {
    Memory(Vec<u8>),
    #[cfg(unix)]
    Disk { file: std::sync::Arc<std::fs::File>, len: u64 },
}

impl PostingsBlob {
    /// Read `[offset, offset+len)` of the blob. RAM: slice; disk: `pread` into an
    /// owned buffer (kernel page cache, not process RSS).
    fn read(&self, offset: u64, len: u32) -> Vec<u8> {
        let (start, end) = (offset as usize, offset as usize + len as usize);
        match self {
            PostingsBlob::Memory(b) => {
                if end > b.len() { return Vec::new(); }
                b[start..end].to_vec()
            }
            #[cfg(unix)]
            PostingsBlob::Disk { file, len: total } => {
                if offset + len as u64 > *total { return Vec::new(); }
                use std::os::unix::fs::FileExt;
                let mut buf = vec![0u8; len as usize];
                if file.read_exact_at(&mut buf, offset).is_err() { return Vec::new(); }
                buf
            }
        }
    }
    fn mem_bytes(&self) -> usize {
        match self {
            PostingsBlob::Memory(b) => b.capacity(),
            #[cfg(unix)]
            PostingsBlob::Disk { .. } => 0, // on disk — not resident
        }
    }
}
use super::tokenizer::tokenize;

/// BM25 term-frequency saturation factor.
///
/// Controls how quickly additional occurrences of a term stop adding
/// to the document score.  Higher values reward repeated terms more.
const BM25_K1: f64 = 1.2;

/// BM25 length-normalisation factor.
///
/// `b = 1.0` fully normalises by document length; `b = 0.0` disables
/// length normalisation entirely.  `0.75` is the standard default.
const BM25_B: f64 = 0.75;

/// Orphan ratio above which a full rebuild is recommended.
///
/// Each [`Bm25Index::delete`] call leaves one 4-byte orphan slot in
/// `doc_lengths`.  Once the orphan fraction exceeds this threshold the
/// dead-weight entries in the postings blob are worth reclaiming.
///
/// At 20 % with 1 M docs the orphan footprint is ≤ 800 KB — safe on
/// a Raspberry Pi — while keeping full rebuilds infrequent.
pub const DEFAULT_REBUILD_THRESHOLD: f64 = 0.20;

/// Snapshot of collection-level statistics persisted alongside the
/// index for diagnostics and offline inspection.
///
/// `avg_doc_len` is stored here for serialisation compatibility, but
/// live scoring uses [`Bm25Index::avg_doc_len`] which derives the
/// value from the running `sum_doc_len` counter and stays accurate
/// after incremental deletions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bm25Meta {
    /// Number of live (non-deleted) documents tracked by this index.
    pub num_docs: u64,
    /// Average token count per document at build time.
    ///
    /// Kept for serialisation; use [`Bm25Index::avg_doc_len`] for
    /// accurate post-deletion values during search.
    pub avg_doc_len: f64,
    /// Name of the indexed field, e.g. `"body"`.
    pub field: String,
}

/// A single ranked result from a BM25 search.
#[derive(Clone, Debug)]
pub struct Bm25Hit {
    /// Node hash (`sk_hash(slug)`) of the matching document.
    pub doc_id: u64,
    /// BM25 relevance score; higher is more relevant.
    pub score: f64,
}

/// How many documents may accumulate in the delta before it is folded into the
/// base. Bounds the per-query cost of scanning the delta, and makes the
/// amortised cost of a merge (`O(corpus)` divided by the threshold, per insert)
/// negligible: at 20 k documents that is single-digit microseconds per write.
/// The merge threshold used to be scaled with the corpus, to make a full
/// rebuild affordable by performing it less often: `clamp(N/64, 4096, 262144)`.
///
/// It is gone because the rebuild is gone. Scaling was only linear inside a
/// band — below 262 144 documents the floor bound and merges came every 4096
/// rows over a growing corpus, and above 16 777 216 the ceiling bound and they
/// came every 262 144 rows over a corpus larger still. Both ends were quadratic,
/// and a fifty-million-row load spends most of its life above the ceiling, in
/// the worse of the two.
///
/// A flush now writes a segment and touches nothing that exists, so how often it
/// happens no longer trades against how much work it does. What remains is
/// [`DELTA_FLUSH_DOCS`], which bounds only how much the delta may hold in RAM.

/// Documents indexed since the last full build, held as a small in-RAM inverted
/// index.
///
/// # Why this exists
///
/// The base is a *contiguous* structure: every term owns one byte range in
/// `postings_bytes`, so adding a document to a term means rewriting that term's
/// range. Doing that per write made a single INSERT cost `O(corpus)` — 134 ms at
/// 20 000 rows, and growing. Rather than fight the layout, new documents land
/// here and queries read both segments; the base stays untouched until a merge
/// folds the delta in.
///
/// This is the same base-plus-overlay shape the storage engine uses for nodes,
/// and it has the same rule: **every read path must consult both halves.**
/// Consulting only the base is how documents silently vanish from search.
#[derive(Clone, Debug, Default)]
struct Bm25Delta {
    /// term -> postings for delta documents only, kept sorted by `doc_id`.
    terms: std::collections::BTreeMap<String, Vec<Posting>>,
    /// Token count per delta document. Also the liveness set for the delta:
    /// absent means deleted, exactly as `doc_id_to_idx` works for the base.
    doc_lengths: HashMap<u64, u32>,
    /// Running total of `doc_lengths`, so `avg_doc_len` stays O(1).
    sum_doc_len: u64,
}

impl Bm25Delta {
    fn is_empty(&self) -> bool { self.doc_lengths.is_empty() }
    fn len(&self) -> usize { self.doc_lengths.len() }

    /// Drop a document. Its postings are left in place and gated by
    /// `doc_lengths`, mirroring how the base gates on `doc_id_to_idx`.
    fn remove(&mut self, doc_id: u64) -> bool {
        match self.doc_lengths.remove(&doc_id) {
            Some(dl) => { self.sum_doc_len -= dl as u64; true }
            None => false,
        }
    }

    fn insert(&mut self, doc_id: u64, text: &str) {
        self.remove(doc_id);
        let tokens = tokenize(text);
        self.doc_lengths.insert(doc_id, tokens.len() as u32);
        self.sum_doc_len += tokens.len() as u64;

        let mut freqs: HashMap<String, u32> = HashMap::new();
        for t in tokens { *freqs.entry(t).or_default() += 1; }
        for (term, freq) in freqs {
            let list = self.terms.entry(term).or_default();
            let p = Posting { doc_id, freq };
            match list.binary_search_by_key(&doc_id, |p| p.doc_id) {
                Ok(i) => list[i] = p,
                Err(i) => list.insert(i, p),
            }
        }
    }

    /// Live postings for `term` — deleted delta docs are filtered out here,
    /// since their entries stay in `terms`.
    fn postings(&self, term: &str) -> Vec<Posting> {
        match self.terms.get(term) {
            Some(list) => list.iter()
                .filter(|p| self.doc_lengths.contains_key(&p.doc_id))
                .cloned()
                .collect(),
            None => Vec::new(),
        }
    }

    fn mem_bytes(&self) -> usize {
        let terms: usize = self.terms.iter()
            .map(|(t, v)| t.len() + 32 + v.len() * std::mem::size_of::<Posting>())
            .sum();
        terms + self.doc_lengths.len() * 16
    }
}

/// Lightweight BM25 index for a single text field.
///
/// # Deletion without a tombstone set
///
/// Rather than maintaining a separate tombstone `HashSet`, deletion is
/// handled by removing the entry from `doc_id_to_idx`.  Because
/// `search` gates every posting lookup through `doc_id_to_idx`, a
/// deleted document can never appear in results — even before the next
/// full rebuild — without any additional allocation.
///
/// The caller in `lib.rs` additionally filters results through
/// `self.nodes.contains_key(&hit.doc_id)` as a belt-and-suspenders
/// guard for the window between a node deletion and the BM25 index
/// update.
///
/// # Rebuild trigger
///
/// After many deletions the `doc_lengths` Vec accumulates orphan slots
/// (4 bytes each).  Call [`needs_rebuild`] periodically; when it
/// returns `true` drop the index and call `build_bm25_index` again.
///
/// [`needs_rebuild`]: Bm25Index::needs_rebuild
/// Magic at the head of a persisted segment's metadata file.
const SEG_MAGIC: &[u8; 8] = b"SKB25SEG";
const SEG_VERSION: u32 = 1;
/// Where the checksum-trailer flag lives in that header.
const SEG_FLAGS_OFF: usize = 12;
/// Fixed prefix before the doc-length array.
const SEG_HEADER: usize = 48;

/// How many flushed segments may share a level before they are merged into one
/// segment at the next level up.
///
/// This is the whole cost model. Each document is rewritten about once per
/// level, and there are `log_FANOUT(N)` levels, so a load costs O(N log N) with
/// a small constant instead of the O(N) full rebuild per flush that the
/// monolithic base required. Raising it means fewer merges and more segments to
/// consult per query; lowering it, the reverse.
const SEG_FANOUT: usize = 8;

/// How many documents accumulate in the RAM delta before it is flushed to a
/// segment.
///
/// Fixed, and small, on purpose. Flushing is O(delta) — it writes a new segment
/// and touches nothing that already exists — so there is no longer any reason to
/// scale this with the corpus. Scaling it was an attempt to make a full rebuild
/// affordable by doing it less often; once the rebuild is gone, the only thing
/// this number controls is how much RAM the delta may hold, and Law 1 wants that
/// bounded by the change rather than the store.
const DELTA_FLUSH_DOCS: usize = 4_096;

/// An immutable slice of the index covering a disjoint set of documents.
///
/// # Why this exists
///
/// The base is one contiguous structure, so folding new documents into it means
/// rewriting all of it: every live document re-sorted, every dictionary term
/// walked, the whole postings blob decoded and re-encoded. Doing that on the
/// write path made a load quadratic — measured at N^1.86 — because the trigger
/// was a few thousand documents while the work was the entire store. That is
/// Law 2 inverted.
///
/// A flush now writes one of these instead: built from the delta alone, costing
/// what the delta costs, touching nothing that already exists. Segments are
/// merged with each other by level, so the base is never rewritten on the write
/// path at all. This is the shape SQLite's FTS5 uses (`fts5IndexMerge`) and
/// Lucene's `TieredMergePolicy`.
///
/// **Every read path must consult the base, every segment, and the delta.**
/// Consulting fewer is how documents silently vanish from search.
#[derive(Clone)]
struct Bm25Segment {
    /// Merge generation. `0` is a fresh flush; `SEG_FANOUT` segments at one
    /// level become a single segment at the next.
    level: u32,
    /// Identifies this segment's files. Monotonic, never reused, so a half-written
    /// segment can never be mistaken for a live one.
    id: u64,
    dict: TermDict,
    /// Same types the base uses, so a persisted segment is **served from disk**
    /// rather than held resident. A segment that lived only in RAM would put the
    /// store back in memory the moment segments carried most of the data.
    postings: PostingsBlob,
    doc_lengths: DocLens,
    /// Liveness for this segment, exactly as `doc_id_to_idx` is for the base:
    /// absent means deleted.
    doc_id_to_idx: DocIdx,
    sum_doc_len: u64,
    num_docs: u64,
    /// Where this segment's metadata file lives once written. `None` while it is
    /// still RAM-only and would be lost on close.
    persisted: Option<std::path::PathBuf>,
}

impl Bm25Segment {
    /// Build a segment from the delta. O(delta) — this is the write path.
    fn from_delta(delta: &Bm25Delta) -> Option<Bm25Segment> {
        if delta.is_empty() {
            return None;
        }
        let mut live: Vec<(u64, u32)> =
            delta.doc_lengths.iter().map(|(k, v)| (*k, *v)).collect();
        live.sort_unstable_by_key(|(d, _)| *d);

        let mut doc_lengths = Vec::with_capacity(live.len());
        let mut doc_id_to_idx = HashMap::with_capacity(live.len());
        let mut sum_doc_len = 0u64;
        for (doc_id, dl) in &live {
            doc_id_to_idx.insert(*doc_id, doc_lengths.len());
            doc_lengths.push(*dl);
            sum_doc_len += *dl as u64;
        }

        let mut dict = TermDict::new();
        let mut blob: Vec<u8> = Vec::new();
        let mut offset: u64 = 0;
        for term in delta.terms.keys() {
            let postings = delta.postings(term);
            if postings.is_empty() {
                continue;
            }
            let bytes = encode_postings_to_file(&postings);
            while offset % 8 != 0 { blob.push(0); offset += 1; }
            dict.insert(term.clone(), offset, bytes.len() as u32);
            blob.extend_from_slice(&bytes);
            offset += bytes.len() as u64;
        }
        Some(Bm25Segment {
            level: 0,
            id: 0,
            dict,
            postings: PostingsBlob::Memory(blob),
            doc_lengths: DocLens::Owned(doc_lengths),
            doc_id_to_idx: DocIdx::Owned(doc_id_to_idx),
            sum_doc_len,
            num_docs: live.len() as u64,
            persisted: None,
        })
    }

    /// Live postings for `term` in this segment.
    fn postings_for(&self, term: &str) -> Vec<Posting> {
        let entry = match self.dict.get(term) {
            Some(e) => e,
            None => return Vec::new(),
        };
        let bytes = self.postings.read(entry.postings_offset, entry.postings_len);
        if bytes.is_empty() {
            return Vec::new();
        }
        decode_postings_from_bytes(&bytes)
            .into_iter()
            .filter(|p| self.doc_id_to_idx.get(p.doc_id).is_some())
            .collect()
    }

    fn live_doc_len(&self, doc_id: u64) -> Option<u32> {
        self.doc_id_to_idx.get(doc_id).map(|i| self.doc_lengths.get(i))
    }

    /// Retire a document. The postings stay and are gated by `doc_id_to_idx`,
    /// which is what keeps the segment immutable.
    fn remove(&mut self, doc_id: u64) -> bool {
        if self.doc_id_to_idx.get(doc_id).is_none() {
            return false;
        }
        // The mapped form cannot be edited, so materialise it — the same thing
        // the base does for the same reason.
        self.doc_id_to_idx.materialize();
        match self.doc_id_to_idx.get(doc_id) {
            Some(i) => {
                let dl = self.doc_lengths.get(i) as u64;
                self.sum_doc_len = self.sum_doc_len.saturating_sub(dl);
                self.num_docs = self.num_docs.saturating_sub(1);
                self.doc_id_to_idx.remove(doc_id);
                true
            }
            None => false,
        }
    }

    /// All live `(doc_id, doc_len)` in this segment, for a merge.
    fn live_docs(&self) -> Vec<(u64, u32)> {
        self.doc_id_to_idx
            .sorted()
            .into_iter()
            .map(|(doc_id, idx)| (doc_id, self.doc_lengths.get(idx as usize)))
            .collect()
    }

    fn mem_bytes(&self) -> usize {
        self.postings.mem_bytes()
            + self.dict.mem_bytes()
            + match &self.doc_lengths {
                DocLens::Owned(v) => v.capacity() * 4,
                DocLens::Mapped { .. } => 0,
            }
            + match &self.doc_id_to_idx {
                DocIdx::Owned(m) => m.capacity() * 24,
                DocIdx::Mapped { .. } => 0,
            }
    }

    /// Write this segment to disk and switch to serving it from there.
    ///
    /// # Why segments must persist
    ///
    /// Flushing to a segment removed the full rebuild from the *write* path, but
    /// compaction still collapsed every segment back into the monolithic base,
    /// because the base format had nowhere to put them. Measured at one million
    /// rows: 221.55 s with auto-compaction against 15.42 s without, and the
    /// difference is entirely that collapse. Persisting a segment is what lets
    /// compaction leave it alone.
    ///
    /// Two files, mirroring the base exactly: metadata (`dict` + doc arrays)
    /// mmap-served, postings `pread` from their own file. After this call the
    /// segment holds no bulk RAM — which is the point, since segments come to
    /// hold most of the index.
    ///
    /// Sacrifice: two inodes per segment, and the metadata file is verified in
    /// full at open rather than per block, so open cost is O(segment metadata).
    /// The postings file carries no checksum yet — the same status the base's
    /// postings file has today.
    fn write_to(&mut self, dir: &std::path::Path, field: &str, id: u64) -> std::io::Result<()> {
        use std::io::Write;
        let meta_path = dir.join(format!("bm25_{field}.s{id}.meta"));
        let post_path = dir.join(format!("bm25_{field}.s{id}.post"));

        let live = self.live_docs();
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(SEG_MAGIC);
        buf.extend_from_slice(&SEG_VERSION.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // flags, filled by blockcrc
        buf.extend_from_slice(&self.level.to_le_bytes());
        buf.extend_from_slice(&(live.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(self.dict.num_terms() as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
        buf.extend_from_slice(&self.num_docs.to_le_bytes());
        buf.extend_from_slice(&self.sum_doc_len.to_le_bytes());
        // Slots are renumbered densely, so the file carries no orphans.
        for (i, (_, dl)) in live.iter().enumerate() {
            let _ = i;
            buf.extend_from_slice(&dl.to_le_bytes());
        }
        for (i, (doc_id, _)) in live.iter().enumerate() {
            buf.extend_from_slice(&doc_id.to_le_bytes());
            buf.extend_from_slice(&(i as u32).to_le_bytes());
        }
        let terms: Vec<(String, super::dict::TermEntry)> = self
            .dict
            .iter()
            .map(|(t, e)| (t.to_string(), e.clone()))
            .collect();
        for (term, e) in &terms {
            let tb = term.as_bytes();
            buf.extend_from_slice(&(tb.len() as u16).to_le_bytes());
            buf.extend_from_slice(tb);
            buf.extend_from_slice(&e.postings_offset.to_le_bytes());
            buf.extend_from_slice(&e.postings_len.to_le_bytes());
        }
        crate::storage::blockcrc::append(&mut buf, SEG_FLAGS_OFF);

        // Postings first, then metadata: metadata is what makes a segment
        // visible, so it is written last and renamed into place. A crash between
        // the two leaves an orphan postings file, which is inert.
        let blob = self.postings_all();
        {
            let tmp = post_path.with_extension("post.tmp");
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&blob)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &post_path)?;
        }
        {
            let tmp = meta_path.with_extension("meta.tmp");
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&buf)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &meta_path)?;
        }

        self.id = id;
        self.persisted = Some(meta_path.clone());
        // Serve from disk now — this is what frees the RAM the segment held.
        let reopened = Bm25Segment::open_from(&meta_path, &post_path)?;
        if let Some(seg) = reopened {
            self.dict = seg.dict;
            self.postings = seg.postings;
            self.doc_lengths = seg.doc_lengths;
            self.doc_id_to_idx = seg.doc_id_to_idx;
        }
        Ok(())
    }

    /// The whole postings blob, however it is currently held.
    fn postings_all(&self) -> Vec<u8> {
        match &self.postings {
            PostingsBlob::Memory(v) => v.clone(),
            #[cfg(unix)]
            PostingsBlob::Disk { file, len } => {
                use std::os::unix::fs::FileExt;
                let mut out = vec![0u8; *len as usize];
                if file.read_exact_at(&mut out, 0).is_err() {
                    return Vec::new();
                }
                out
            }
        }
    }

    /// Read a segment back. `None` when the file is not a segment or fails its
    /// checksum — the caller rebuilds rather than serves what it cannot vouch for.
    fn open_from(
        meta_path: &std::path::Path,
        post_path: &std::path::Path,
    ) -> std::io::Result<Option<Bm25Segment>> {
        let raw = std::fs::read(meta_path)?;
        if raw.len() < SEG_HEADER || &raw[0..8] != SEG_MAGIC {
            return Ok(None);
        }
        // Verified whole: a segment's doc arrays and dictionary are what every
        // read of it addresses through, so serving them unchecked would answer
        // with another document's statistics.
        let Some(b) = crate::storage::blockcrc::verified_payload(&raw, SEG_FLAGS_OFF) else {
            return Ok(None);
        };
        if u32::from_le_bytes(b[8..12].try_into().unwrap()) != SEG_VERSION {
            return Ok(None);
        }
        let level = u32::from_le_bytes(b[16..20].try_into().unwrap());
        let n_docs = u32::from_le_bytes(b[20..24].try_into().unwrap()) as usize;
        let n_terms = u32::from_le_bytes(b[24..28].try_into().unwrap()) as usize;
        let num_docs = u64::from_le_bytes(b[32..40].try_into().unwrap());
        let sum_doc_len = u64::from_le_bytes(b[40..48].try_into().unwrap());

        // Capped by the file before anything is sized from them.
        let room = b.len().saturating_sub(SEG_HEADER);
        if n_docs.saturating_mul(16) > room {
            return Ok(None);
        }
        let dl_off = SEG_HEADER;
        let id_off = dl_off + n_docs * 4;
        let mut p = id_off + n_docs * 12;

        let mut dict = TermDict::new();
        for _ in 0..n_terms {
            let Some(tl) = b.get(p..p + 2) else { break };
            let tlen = u16::from_le_bytes(tl.try_into().unwrap()) as usize;
            p += 2;
            let Some(tb) = b.get(p..p + tlen) else { break };
            let Ok(term) = std::str::from_utf8(tb) else { break };
            p += tlen;
            let Some(ob) = b.get(p..p + 8) else { break };
            let off = u64::from_le_bytes(ob.try_into().unwrap());
            p += 8;
            let Some(lb) = b.get(p..p + 4) else { break };
            let len = u32::from_le_bytes(lb.try_into().unwrap());
            p += 4;
            dict.insert(term.to_string(), off, len);
        }

        let file = std::fs::File::open(post_path)?;
        let plen = file.metadata()?.len();
        #[cfg(unix)]
        let postings = PostingsBlob::Disk { file: std::sync::Arc::new(file), len: plen };
        #[cfg(not(unix))]
        let postings = { let _ = (file, plen); PostingsBlob::Memory(std::fs::read(post_path)?) };

        // The doc arrays are served from the map, exactly as the base's are.
        let mfile = std::fs::File::open(meta_path)?;
        let mlen = mfile.metadata()?.len() as usize;
        let view = match MmapView::try_new(&mfile, mlen) {
            Some(v) => Arc::new(v),
            None => return Ok(None),
        };
        Ok(Some(Bm25Segment {
            level,
            id: 0,
            dict,
            postings,
            doc_lengths: DocLens::Mapped { view: view.clone(), off: dl_off, count: n_docs },
            doc_id_to_idx: DocIdx::Mapped { view, off: id_off, count: n_docs },
            sum_doc_len,
            num_docs,
            persisted: Some(meta_path.to_path_buf()),
        }))
    }

    /// Fold several segments into one. Costs what the inputs cost — never the
    /// store — which is what keeps the merge policy inside Law 2.
    fn merge(parts: Vec<Bm25Segment>) -> Option<Bm25Segment> {
        if parts.is_empty() {
            return None;
        }
        let mut live: Vec<(u64, u32)> = Vec::new();
        for p in &parts {
            live.extend(p.live_docs());
        }
        if live.is_empty() {
            return None;
        }
        live.sort_unstable_by_key(|(d, _)| *d);

        let mut doc_lengths = Vec::with_capacity(live.len());
        let mut doc_id_to_idx = HashMap::with_capacity(live.len());
        let mut sum_doc_len = 0u64;
        for (doc_id, dl) in &live {
            doc_id_to_idx.insert(*doc_id, doc_lengths.len());
            doc_lengths.push(*dl);
            sum_doc_len += *dl as u64;
        }

        let mut terms: Vec<String> = Vec::new();
        for p in &parts {
            terms.extend(p.dict.iter().map(|(t, _)| t.to_string()));
        }
        terms.sort();
        terms.dedup();

        let mut dict = TermDict::new();
        let mut blob: Vec<u8> = Vec::new();
        let mut offset: u64 = 0;
        for term in terms {
            let mut postings: Vec<Posting> = Vec::new();
            for p in &parts {
                postings.extend(p.postings_for(&term));
            }
            if postings.is_empty() {
                continue;
            }
            // Segments hold disjoint documents — `insert_doc` retires the old
            // copy everywhere before writing the new one — so this is a sort of
            // already-sorted runs, not a deduplication.
            postings.sort_unstable_by_key(|p| p.doc_id);
            let bytes = encode_postings_to_file(&postings);
            while offset % 8 != 0 { blob.push(0); offset += 1; }
            dict.insert(term, offset, bytes.len() as u32);
            blob.extend_from_slice(&bytes);
            offset += bytes.len() as u64;
        }
        let level = parts.iter().map(|p| p.level).max().unwrap_or(0) + 1;
        Some(Bm25Segment {
            level,
            id: 0,
            dict,
            postings: PostingsBlob::Memory(blob),
            doc_lengths: DocLens::Owned(doc_lengths),
            doc_id_to_idx: DocIdx::Owned(doc_id_to_idx),
            sum_doc_len,
            num_docs: live.len() as u64,
            persisted: None,
        })
    }
}

#[derive(Clone)]
pub struct Bm25Index {
    /// Collection-level metadata (document count, field name).
    meta: Bm25Meta,
    /// Sorted term dictionary mapping each term to its byte range in
    /// `postings_bytes`.
    dict: TermDict,
    /// Concatenated, delta-encoded, varint-compressed postings for
    /// every indexed term.  Never rewritten during incremental
    /// operations; dead entries are reclaimed only on a full rebuild.
    postings: PostingsBlob,
    /// Token count for each document, addressed by the slot index
    /// stored in `doc_id_to_idx`.  Slots belonging to deleted
    /// documents become unreachable orphans; each wastes exactly
    /// 4 bytes until the next rebuild.
    doc_lengths: DocLens,
    /// Maps a node hash (`sk_hash(slug)`) to its slot index in
    /// `doc_lengths`.
    ///
    /// This is the single source of truth for document liveness:
    /// removing an entry here is sufficient to exclude the document
    /// from all future search results.
    doc_id_to_idx: DocIdx,
    /// Running total of token counts across **live** documents only.
    ///
    /// Decremented by [`delete`] so that [`avg_doc_len`] stays
    /// accurate without touching the postings blob.
    ///
    /// [`delete`]: Bm25Index::delete
    /// [`avg_doc_len`]: Bm25Index::avg_doc_len
    sum_doc_len: u64,
    /// Documents written since the last merge. See [`Bm25Delta`] — every read
    /// path must consult this as well as the base.
    delta: Bm25Delta,
    /// Files of segments a level merge has absorbed. Deleted only once the
    /// segment that replaced them is safely on disk — write the new state,
    /// verify it, then drop the old, which is Law 3.
    retired_segment_files: Vec<std::path::PathBuf>,
    /// Monotonic, never reused: a half-written segment can never be mistaken for
    /// a live one.
    next_segment_id: u64,
    /// Immutable segments written by flushing the delta, newest last.
    ///
    /// The base above is never rewritten on the write path; these absorb every
    /// write and are merged with each other by level. Empty in an index that has
    /// only ever been built or freshly opened, which is why an index with no
    /// segments behaves exactly as it did before they existed.
    segments: Vec<Bm25Segment>,
}

impl Bm25Index {
    /// Approximate resident RAM held by this index, in bytes: the compressed
    /// postings blob + term dictionary + per-doc length array + id→slot map.
    pub fn mem_bytes(&self) -> usize {
        self.postings.mem_bytes()
            + self.dict.mem_bytes()
            + match &self.doc_lengths { DocLens::Owned(v) => v.capacity() * 4, DocLens::Mapped { .. } => 0 }
            + match &self.doc_id_to_idx { DocIdx::Owned(m) => m.capacity() * 24, DocIdx::Mapped { .. } => 0 }
            + self.delta.mem_bytes()
            + self.segments.iter().map(|s| s.mem_bytes()).sum::<usize>()
    }

    /// Spill the postings blob to `path` and switch to disk-backed reads,
    /// freeing the in-RAM blob. Makes the index **disk-first**: only the term
    /// dictionary + doc arrays stay resident; postings are `pread` at query time.
    #[cfg(unix)]
    pub fn spill_to_disk(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        // The blob about to be written must be the whole index — fold in anything
        // still sitting in the delta first.
        self.merge_delta();
        if let PostingsBlob::Memory(blob) = &self.postings {
            // Write a NEW file and rename it into place rather than truncating the
            // existing one. A snapshot holds its own descriptor onto this file
            // (PostingsBlob::Disk shares it by Arc), and on unix that descriptor keeps
            // reading the old inode across a rename — so an existing snapshot stays
            // valid. Truncating in place would rewrite the bytes underneath every live
            // snapshot and make their BM25 queries return nothing.
            let tmp = path.with_extension("postings.tmp");
            {
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&tmp)?;
                f.write_all(blob)?;
                f.sync_all()?;
            }
            std::fs::rename(&tmp, path)?;
            let f = std::fs::OpenOptions::new().read(true).open(path)?;
            let len = blob.len() as u64;
            self.postings = PostingsBlob::Disk { file: std::sync::Arc::new(f), len };
        }
        Ok(())
    }

    /// Serialize the resident metadata (dict + doc_lengths + doc_id_to_idx + counters)
    /// to the bm25.bin container. Postings stay in the separate `bm25_<field>.postings`
    /// file (already spilled). Format is documented in `open_mapped`.
    pub(crate) fn write_binary<W: std::io::Write>(&self, w: &mut W) -> std::io::Result<()> {
        // Guard rail. The on-disk format stores one contiguous segment and has no
        // room for a delta, so persisting an unmerged index would silently drop
        // every recently-written document from search on the next open. Callers
        // must merge_delta() first; refusing here makes that impossible to forget.
        // Segments are no longer a reason to refuse: they live in their own
        // files and are reloaded by `load_segments`. An unflushed delta still is,
        // because it lives nowhere.
        if !self.delta.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "bm25: refusing to serialise an index with an unmerged delta",
            ));
        }
        let fb = self.meta.field.as_bytes();
        w.write_all(&(fb.len() as u16).to_le_bytes())?;
        w.write_all(fb)?;
        w.write_all(&self.meta.num_docs.to_le_bytes())?;
        w.write_all(&self.sum_doc_len.to_le_bytes())?;
        // doc_lengths (u32 array)
        let dc = self.doc_lengths.len();
        w.write_all(&(dc as u32).to_le_bytes())?;
        for i in 0..dc { w.write_all(&self.doc_lengths.get(i).to_le_bytes())?; }
        // doc_id_to_idx (sorted (u64,u32) array)
        let idmap = self.doc_id_to_idx.sorted();
        w.write_all(&(idmap.len() as u32).to_le_bytes())?;
        for (id, idx) in &idmap {
            w.write_all(&id.to_le_bytes())?;
            w.write_all(&idx.to_le_bytes())?;
        }
        // dict (term → postings location), loaded resident on open
        let terms: Vec<(&str, &super::dict::TermEntry)> = self.dict.iter().collect();
        w.write_all(&(terms.len() as u32).to_le_bytes())?;
        for (term, e) in &terms {
            let tb = term.as_bytes();
            w.write_all(&(tb.len() as u16).to_le_bytes())?;
            w.write_all(tb)?;
            w.write_all(&e.postings_offset.to_le_bytes())?;
            w.write_all(&e.postings_len.to_le_bytes())?;
        }
        Ok(())
    }

    /// Disk-first open of one field's BM25 index from an mmap'd bm25.bin at `base`.
    /// The two O(N) doc arrays are served from the map; the dict is loaded resident
    /// (the deliberate accelerator — sub-linear in N); postings are `pread` from the
    /// spilled `bm25_<field>.postings` file. Returns the index + bytes consumed.
    /// Layout: [u16 field_len][field][u64 num_docs][u64 sum_doc_len]
    ///   [u32 doc_count][doc_count × u32 lengths]
    ///   [u32 idmap_count][idmap_count × (u64 id, u32 idx)]
    ///   [u32 term_count][term_count × (u16 tlen, term, u64 off, u32 len)]
    pub(crate) fn open_mapped(view: &Arc<MmapView>, base: usize, dir: &std::path::Path) -> std::io::Result<(Self, usize)> {
        let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m);
        let total = view.len();
        let b = view.slice(base, total.saturating_sub(base)).ok_or_else(|| bad("bm25 blob range"))?;
        // Every number below comes out of the file, and each one advances `p` by
        // `count * stride` before the next is read — so a corrupt count jumps `p`
        // far past the blob and the next read indexes out of range. These helpers
        // indexed directly: one flipped byte in `bm25.bin` panicked at open, which
        // aborts the process instead of reporting a corrupt index.
        //
        // Reading zero past the end turns that into an ordinary failure: the
        // lengths and counts then describe a table this code cannot find data for,
        // and the `get(..).ok_or_else(bad(..))` checks below report it as the
        // corrupt index it is.
        let ru16 = |b: &[u8], p: usize| -> u16 {
            b.get(p..p + 2).map_or(0, |x| u16::from_le_bytes([x[0], x[1]]))
        };
        let ru32 = |b: &[u8], p: usize| -> u32 {
            b.get(p..p + 4).map_or(0, |x| u32::from_le_bytes(x.try_into().unwrap()))
        };
        let ru64 = |b: &[u8], p: usize| -> u64 {
            b.get(p..p + 8).map_or(0, |x| u64::from_le_bytes(x.try_into().unwrap()))
        };

        let mut p = 0usize;
        let flen = ru16(b, p) as usize; p += 2;
        let field = std::str::from_utf8(b.get(p..p + flen).ok_or_else(|| bad("bm25 field"))?)
            .map_err(|_| bad("bm25 field utf8"))?.to_string();
        p += flen;
        let num_docs = ru64(b, p); p += 8;
        let sum_doc_len = ru64(b, p); p += 8;

        // Each count is capped by what the blob could hold at its stride, so a
        // corrupt one cannot walk `p` off the end or make the term loop below run
        // for billions of iterations.
        let room = |p: usize, stride: usize| -> usize {
            b.len().saturating_sub(p) / stride
        };
        let doc_count = (ru32(b, p) as usize).min(room(p + 4, 4)); p += 4;
        let dl_off = base + p; p += doc_count * 4;

        let idmap_count = (ru32(b, p) as usize).min(room(p + 4, 12)); p += 4;
        let idmap_off = base + p; p += idmap_count * 12;

        // A term needs at least its two length bytes plus an offset and a length.
        let term_count = (ru32(b, p) as usize).min(room(p + 4, 14)); p += 4;
        let mut dict = TermDict::new();
        for _ in 0..term_count {
            let tlen = ru16(b, p) as usize; p += 2;
            let term = std::str::from_utf8(b.get(p..p + tlen).ok_or_else(|| bad("bm25 term"))?)
                .map_err(|_| bad("bm25 term utf8"))?;
            p += tlen;
            let off = ru64(b, p); p += 8;
            let len = ru32(b, p); p += 4;
            dict.insert(term.to_string(), off, len);
        }

        // Postings: pread from the spilled per-field file.
        let ppath = dir.join(format!("bm25_{field}.postings"));
        let pfile = std::fs::File::open(&ppath)?;
        let plen = pfile.metadata()?.len();
        // `#[cfg(unix)]` binds to the *statement that follows it*, and it followed
        // only the `let needed = …` line. So on Windows `needed` did not exist
        // while the `if needed > plen` below it still compiled, and
        // `PostingsBlob::Disk` — a unix-only variant — was named unconditionally.
        // Two errors, and the crate has not built for Windows since. The whole
        // unix branch is one block now, so the attribute covers what it means to.
        #[cfg(unix)]
        let postings = {
            // Sanity check: the dictionary and the postings file must belong to
            // the same generation. If a merge ever rewrote one without the other,
            // every offset here would address the wrong bytes; refusing makes the
            // caller rebuild from the data instead of serving nonsense.
            let needed = dict
                .iter()
                .map(|(_, e)| e.postings_offset + e.postings_len as u64)
                .max()
                .unwrap_or(0);
            if needed > plen {
                return Err(bad("bm25 postings file is older than its dictionary"));
            }
            PostingsBlob::Disk { file: std::sync::Arc::new(pfile), len: plen }
        };
        #[cfg(not(unix))]
        let postings = { let _ = (pfile, plen); PostingsBlob::Memory(std::fs::read(&ppath)?) };

        let avg = if num_docs == 0 { 1.0 } else { sum_doc_len as f64 / num_docs as f64 };
        let index = Bm25Index {
            meta: Bm25Meta { num_docs, avg_doc_len: avg, field },
            dict,
            postings,
            doc_lengths: DocLens::Mapped { view: view.clone(), off: dl_off, count: doc_count },
            doc_id_to_idx: DocIdx::Mapped { view: view.clone(), off: idmap_off, count: idmap_count },
            sum_doc_len,
            delta: Bm25Delta::default(),
            segments: Vec::new(),
            retired_segment_files: Vec::new(),
            next_segment_id: 0,
        };
        Ok((index, p))
    }

    /// True when the doc arrays are served from the mmap (paged, disk-first).
    pub(crate) fn is_disk_backed(&self) -> bool {
        matches!(self.doc_lengths, DocLens::Mapped { .. })
    }

    /// The indexed field name.
    pub(crate) fn field_name(&self) -> &str {
        &self.meta.field
    }

    /// Build a BM25 index from a document iterator.
    ///
    /// `docs` yields `(doc_id, text)` pairs where `doc_id` is
    /// `sk_hash(slug)`.  All tokenisation, postings compression, and
    /// dictionary construction happen in one pass.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let pairs = nodes.iter()
    ///     .filter_map(|(&h, n)| n.payload["body"].as_str().map(|t| (h, t)));
    /// let index = Bm25Index::build("body", pairs);
    /// ```
    pub fn build<'a>(field: &str, docs: impl Iterator<Item = (u64, &'a str)>) -> Self {
        let mut term_doc_freqs: HashMap<String, HashMap<u64, u32>> = HashMap::new();
        let mut doc_lengths: Vec<u32> = Vec::new();
        let mut doc_ids: Vec<u64> = Vec::new();
        let mut doc_id_to_idx: HashMap<u64, usize> = HashMap::new();

        // First pass: tokenise every document and accumulate
        // per-term document-frequency maps.
        let mut sum_doc_len: u64 = 0;
        for (doc_id, text) in docs {
            let idx = doc_ids.len();
            doc_ids.push(doc_id);
            doc_id_to_idx.insert(doc_id, idx);

            let terms = tokenize(text);
            let doc_len = terms.len() as u32;
            doc_lengths.push(doc_len);
            sum_doc_len += doc_len as u64;

            for term in terms {
                let entry = term_doc_freqs.entry(term).or_default();
                *entry.entry(doc_id).or_default() += 1;
            }
        }

        let num_docs = doc_ids.len() as u64;
        let avg_doc_len = if num_docs > 0 {
            sum_doc_len as f64 / num_docs as f64
        } else {
            1.0
        };

        // Build sorted postings lists per term.
        let mut postings_map: HashMap<String, Vec<Posting>> = HashMap::new();
        for (term, doc_freqs) in term_doc_freqs {
            let mut postings: Vec<Posting> = doc_freqs
                .into_iter()
                .map(|(doc_id, freq)| Posting { doc_id, freq })
                .collect();
            postings.sort_by_key(|p| p.doc_id);
            postings_map.insert(term, postings);
        }

        // Serialise postings into one contiguous byte buffer and build
        // the term dictionary (term → byte offset + length).
        let mut dict = TermDict::new();
        let mut all_postings: Vec<u8> = Vec::new();
        let mut offset: u64 = 0;

        let mut terms: Vec<_> = postings_map.keys().cloned().collect();
        terms.sort();

        for term in terms {
            let postings = postings_map.get(&term).unwrap();
            let postings_bytes = encode_postings_to_file(postings);
            let len = postings_bytes.len() as u32;

            // Align each postings list to an 8-byte boundary so that
            // future mmap reads stay aligned.
            while offset % 8 != 0 {
                all_postings.push(0);
                offset += 1;
            }

            let data_offset = offset;
            dict.insert(term.clone(), data_offset, len);

            all_postings.extend_from_slice(&postings_bytes);
            offset += postings_bytes.len() as u64;
        }

        let meta = Bm25Meta {
            num_docs,
            avg_doc_len,
            field: field.to_string(),
        };

        Self {
            meta,
            dict,
            postings: PostingsBlob::Memory(all_postings),
            doc_lengths: DocLens::Owned(doc_lengths),
            doc_id_to_idx: DocIdx::Owned(doc_id_to_idx),
            sum_doc_len,
            delta: Bm25Delta::default(),
            segments: Vec::new(),
            retired_segment_files: Vec::new(),
            next_segment_id: 0,
        }
    }

    /// Search the index and return the top-`top_k` documents ranked by
    /// BM25 score (highest first).
    ///
    /// Deleted documents are automatically excluded because their
    /// entries were removed from `doc_id_to_idx` by [`delete`]; no
    /// extra filtering is required inside this method.
    ///
    /// [`delete`]: Bm25Index::delete
    pub fn search(&self, query: &str, top_k: usize) -> Vec<Bm25Hit> {
        let mut hits = self.score_all(query);
        hits.truncate(top_k);
        hits
    }

    /// Like [`search`](Self::search) but returns EVERY matching document (no
    /// top-k cap). Use for filters (`BM25(f, q) > x`) and score maps, where
    /// truncating both drops legitimate matches and — on tied scores — does so
    /// non-deterministically (different rows each run).
    pub fn search_all(&self, query: &str) -> Vec<Bm25Hit> {
        self.score_all(query)
    }

    /// Score every document matching `query`, ordered by score DESC then doc_id
    /// ASC — a deterministic total order (HashMap iteration order alone is not,
    /// so ties would otherwise vary between runs).
    fn score_all(&self, query: &str) -> Vec<Bm25Hit> {
        let query_terms = tokenize(query);
        if query_terms.is_empty() {
            return Vec::new();
        }

        // Pre-compute IDF for each query term.
        // Terms absent from the dictionary contribute nothing.
        let idf: HashMap<&str, f64> = query_terms
            .iter()
            .filter_map(|t| {
                // Document frequency spans both segments. Taking it from the base
                // alone would score a term as rarer than it is the moment any
                // document holding it lands in the delta.
                let postings = self.term_postings(t);
                if !postings.is_empty() {
                    let df = postings.len() as f64;
                    // Smoothed IDF (Lucene / BM25+ variant): `ln(1 + …)` stays
                    // positive even when a term appears in >50% of docs, instead
                    // of the classic RSJ form which floors to 0 for common terms.
                    let idf = (1.0 + (self.num_docs() as f64 - df + 0.5) / (df + 0.5)).ln();
                    Some((t.as_str(), idf.max(0.0)))
                } else {
                    None
                }
            })
            .collect();

        if idf.is_empty() {
            return Vec::new();
        }

        // Accumulate BM25 scores.  The `doc_id_to_idx` lookup acts as
        // the liveness gate: deleted docs have no entry and are skipped
        // via the `None => continue` arm.
        let avg_dl = self.avg_doc_len();
        let mut scores: HashMap<u64, f64> = HashMap::new();

        for (term, term_idf) in &idf {
            for posting in self.term_postings(term) {
                // Liveness gate, spanning both segments: the base drops deleted
                // documents from doc_id_to_idx, the delta from its doc_lengths.
                let doc_len = match self.live_doc_len(posting.doc_id) {
                    Some(dl) => dl as f64,
                    None => continue,
                };
                let tf = posting.freq as f64;

                // Standard BM25 formula:
                //   score += IDF × (tf × (k1 + 1)) / (tf + k1 × (1 − b + b × dl / avg_dl))
                let numerator = tf * (BM25_K1 + 1.0);
                let denominator =
                    tf + BM25_K1 * (1.0 - BM25_B + BM25_B * doc_len / avg_dl);
                let score = term_idf * numerator / denominator;

                *scores.entry(posting.doc_id).or_insert(0.0) += score;
            }
        }

        // Sort descending by score and truncate to top_k.
        let mut hits: Vec<Bm25Hit> = scores
            .into_iter()
            .map(|(doc_id, score)| Bm25Hit { doc_id, score })
            .collect();
        // Deterministic total order: score DESC, ties broken by doc_id ASC.
        hits.sort_by(|a, b| {
            // `total_cmp`, not `partial_cmp(..).unwrap()`. A NaN score panicked
            // the query outright, and a NaN is reachable: scores are arithmetic
            // over term statistics, and the comparator is not the place to
            // discover that one came out undefined. Total by construction, so an
            // undefined score sorts to one end instead of crashing or scrambling
            // the ones around it.
            b.score.total_cmp(&a.score).then(a.doc_id.cmp(&b.doc_id))
        });
        hits
    }

    /// Remove a document from the index without rewriting the postings
    /// blob.
    ///
    /// # What this does
    ///
    /// 1. Looks up `doc_id` in `doc_id_to_idx`.  Returns `false`
    ///    immediately if not found (already deleted or never indexed).
    /// 2. Reads the document's token count and subtracts it from
    ///    `sum_doc_len` so that [`avg_doc_len`] stays accurate.
    /// 3. Decrements `meta.num_docs`.
    /// 4. Removes the entry from `doc_id_to_idx`, which is the only
    ///    step needed to make the document invisible to [`search`]:
    ///    the posting-loop liveness gate skips any `doc_id` with no
    ///    `doc_id_to_idx` entry.
    ///
    /// # What this does NOT do
    ///
    /// The postings blob (`postings_bytes`) is **never rewritten**.
    /// The deleted document's posting entries remain in the compressed
    /// stream as inert bytes until the next full rebuild.  Rewriting
    /// every affected postings list would cost O(unique terms in the
    /// document) allocations — equivalent to a partial rebuild and
    /// unacceptable on a Pi.
    ///
    /// The `doc_lengths` slot at the freed index becomes an orphan
    /// (4 bytes, unreachable via `doc_id_to_idx`).  Monitor orphan
    /// accumulation with [`orphan_count`] and schedule a full rebuild
    /// when [`needs_rebuild`] returns `true`.
    ///
    /// # Returns
    ///
    /// `true` if the document was found and removed; `false` if it was
    /// not indexed (already deleted or never present).
    ///
    /// [`avg_doc_len`]: Bm25Index::avg_doc_len
    /// [`search`]: Bm25Index::search
    /// [`orphan_count`]: Bm25Index::orphan_count
    /// [`needs_rebuild`]: Bm25Index::needs_rebuild
    pub fn delete(&mut self, doc_id: u64) -> bool {
        // A document can live in either segment (never both — insert_doc retires
        // the base copy first), so try the delta as well.
        if self.delta.remove(doc_id) {
            return true;
        }
        // A document lives in exactly one place, because `insert_doc` retires the
        // previous copy before writing the new one.
        for seg in &mut self.segments {
            if seg.remove(doc_id) {
                return true;
            }
        }
        // The liveness map is the only thing that excludes a document, and the
        // mapped form cannot be edited — materialise it so the removal sticks.
        if self.doc_id_to_idx.get(doc_id).is_some() {
            self.doc_id_to_idx.materialize();
        }
        if let Some(idx) = self.doc_id_to_idx.get(doc_id) {
            let doc_len = self.doc_lengths.get(idx) as u64;
            self.sum_doc_len = self.sum_doc_len.saturating_sub(doc_len);
            self.meta.num_docs = self.meta.num_docs.saturating_sub(1);
            self.doc_id_to_idx.remove(doc_id);
            // doc_lengths[idx] becomes an unreachable orphan slot.
            true
        } else {
            false
        }
    }

    /// Current average token count across **live** documents.
    ///
    /// Derived from the running `sum_doc_len` counter so the value
    /// stays accurate after incremental [`delete`] calls, without
    /// touching the postings blob.
    ///
    /// Returns `1.0` when no live documents remain to avoid
    /// division-by-zero in the BM25 length-normalisation term.
    ///
    /// [`delete`]: Bm25Index::delete
    #[inline]
    pub fn avg_doc_len(&self) -> f64 {
        let n = self.num_docs();
        if n == 0 {
            1.0
        } else {
            let seg: u64 = self.segments.iter().map(|s| s.sum_doc_len).sum();
            (self.sum_doc_len + seg + self.delta.sum_doc_len) as f64 / n as f64
        }
    }

    /// Number of `doc_lengths` slots that belong to deleted documents.
    ///
    /// Each [`delete`] call leaves one 4-byte orphan slot.  Orphans
    /// are reclaimed only on a full rebuild triggered by
    /// [`needs_rebuild`].
    ///
    /// # Pi footprint
    ///
    /// At the default 20 % rebuild threshold with 1 M documents the
    /// maximum orphan footprint is 200 K × 4 = 800 KB — well within
    /// Pi constraints.
    ///
    /// [`delete`]: Bm25Index::delete
    /// [`needs_rebuild`]: Bm25Index::needs_rebuild
    pub fn orphan_count(&self) -> usize {
        self.doc_lengths.len().saturating_sub(self.doc_id_to_idx.len())
    }

    /// Returns `true` when the orphan ratio exceeds `threshold` and a
    /// full rebuild is recommended.
    ///
    /// The orphan ratio is `orphan_count / doc_lengths.len()`.  Once
    /// it is high the postings blob carries significant dead weight and
    /// a rebuild both reclaims memory and restores full scoring
    /// accuracy.
    ///
    /// # Recommended threshold
    ///
    /// Use [`DEFAULT_REBUILD_THRESHOLD`] (`0.20`) for most workloads.
    /// Lower values rebuild more aggressively (better accuracy, more
    /// I/O); higher values tolerate more dead weight in exchange for
    /// fewer rebuilds.
    ///
    /// # Example
    ///
    /// ```ignore
    /// if bm25_idx.needs_rebuild(DEFAULT_REBUILD_THRESHOLD) {
    ///     db.build_bm25_index("body");
    /// }
    /// ```
    pub fn needs_rebuild(&self, threshold: f64) -> bool {
        let total = self.doc_lengths.len();
        if total == 0 {
            return false;
        }
        self.orphan_count() as f64 / total as f64 > threshold
    }

    /// Total number of live (non-deleted) documents in the index.
    pub fn num_docs(&self) -> u64 {
        self.meta.num_docs + self.segment_docs() + self.delta.len() as u64
    }

    /// Number of unique terms across the dictionary and the delta.
    pub fn num_terms(&self) -> usize {
        let mut extra: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for seg in &self.segments {
            extra.extend(seg.dict.iter().map(|(t, _)| t).filter(|t| self.dict.get(t).is_none()));
        }
        extra.extend(
            self.delta.terms.keys().map(|t| t.as_str()).filter(|t| self.dict.get(t).is_none()),
        );
        self.dict.num_terms() + extra.len()
    }

    /// How many documents are waiting in the delta. Exposed so callers can
    /// decide when to [`merge_delta`]; the index also merges itself once the
    /// delta passes [`delta_merge_threshold`].
    ///
    /// [`merge_delta`]: Bm25Index::merge_delta
    pub fn delta_len(&self) -> usize { self.delta.len() }

    /// Index one document without rebuilding the corpus.
    ///
    /// Cost is proportional to the length of `text`, not to the size of the
    /// index — that is the whole point. Re-inserting an existing `doc_id`
    /// replaces it: the base copy is retired through [`delete`] and the new
    /// content lands in the delta.
    ///
    /// [`delete`]: Bm25Index::delete
    pub fn insert_doc(&mut self, doc_id: u64, text: &str) {
        self.delete(doc_id);
        self.delta.insert(doc_id, text);
        if self.delta.len() >= DELTA_FLUSH_DOCS {
            self.flush_delta_to_segment();
        }
    }

    /// Fold the delta into the base, producing a single contiguous segment.
    ///
    /// This is a purely structural merge: postings are decoded, combined and
    /// re-encoded, so it needs no access to the original documents. It also
    /// reclaims the orphan slots left behind by [`delete`], which is why it
    /// doubles as the rebuild that [`needs_rebuild`] asks for.
    ///
    /// [`delete`]: Bm25Index::delete
    /// [`needs_rebuild`]: Bm25Index::needs_rebuild
    /// Write the delta out as a new segment. **O(delta)** — nothing that already
    /// exists is read or rewritten.
    ///
    /// This is what replaced calling [`merge_delta`] from the insert path. That
    /// rebuilt the entire index every few thousand documents, which measured
    /// N^1.86 over a 20k/50k/150k ladder: a change-sized trigger doing
    /// store-sized work, Law 2 exactly backwards.
    ///
    /// [`merge_delta`]: Bm25Index::merge_delta
    fn flush_delta_to_segment(&mut self) {
        if let Some(seg) = Bm25Segment::from_delta(&self.delta) {
            self.segments.push(seg);
            self.delta = Bm25Delta::default();
            self.merge_segment_levels();
        }
    }

    /// Merge segments upward whenever `SEG_FANOUT` of them share a level.
    ///
    /// Segment count stays at `O(SEG_FANOUT x log(N))` rather than growing with
    /// the number of flushes, which is what keeps a query's per-segment
    /// dictionary probes bounded. Each merge costs what its inputs cost, never
    /// what the store costs.
    ///
    /// The sacrifice: a query consults every segment, so this trades a little
    /// read cost for removing the rebuild from the write path entirely.
    fn merge_segment_levels(&mut self) {
        loop {
            let mut counts: HashMap<u32, usize> = HashMap::new();
            for seg in &self.segments {
                *counts.entry(seg.level).or_default() += 1;
            }
            // Lowest full level first, so merges stay cheap and the work is
            // spread rather than arriving all at once at the top.
            let level = match counts
                .iter()
                .filter(|(_, c)| **c >= SEG_FANOUT)
                .map(|(l, _)| *l)
                .min()
            {
                Some(l) => l,
                None => break,
            };
            let (to_merge, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.segments)
                .into_iter()
                .partition(|seg| seg.level == level);
            self.segments = rest;
            // Their files outlive them until the replacement is durable.
            for seg in &to_merge {
                if let Some(path) = &seg.persisted {
                    self.retired_segment_files.push(path.clone());
                }
            }
            if let Some(merged) = Bm25Segment::merge(to_merge) {
                self.segments.push(merged);
            }
        }
    }

    /// Delete every persisted segment file for `field`.
    ///
    /// # Why a full rebuild must call this
    ///
    /// `build_bm25_index` walks the whole store and produces a base covering
    /// every document, then replaces the index — an index with no segments. The
    /// segment *files* survive that, and `load_segments` then finds them and adds
    /// them back, so every document they hold is counted **twice**: once in the
    /// rebuilt base and once in the segment.
    ///
    /// That is not a lost row, which a count would catch. It inflates `num_docs`,
    /// which moves every IDF, so the index answers with the right documents in
    /// the wrong order. Measured: a corpus of 33 documents reading as 45 after a
    /// compaction, and BM25 scores diverging from every other storage mode.
    ///
    /// The files are derived data — the rebuild can always be redone from the
    /// store — so removing them cannot lose anything the store still holds.
    pub fn remove_segment_files(dir: &std::path::Path, field: &str) {
        let prefix = format!("bm25_{field}.s");
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(rest) = name.strip_prefix(&prefix) else { continue };
            if rest.ends_with(".meta") || rest.ends_with(".post") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Segments that exist only in RAM, and would be lost on close.
    pub fn unpersisted_segments(&self) -> usize {
        self.segments.iter().filter(|s| s.persisted.is_none()).count()
    }

    /// Fold the delta into a segment and write every segment that is not yet on
    /// disk. **O(change)** — a segment already on disk is not rewritten.
    ///
    /// This is what replaced collapsing everything into the base at compaction
    /// time. That collapse was O(corpus) and ran repeatedly during a load: one
    /// million rows cost 221.55 s with it and 15.42 s without.
    pub fn persist(&mut self, dir: &std::path::Path) -> std::io::Result<()> {
        self.flush_delta_to_segment();
        let field = self.meta.field.clone();
        for i in 0..self.segments.len() {
            if self.segments[i].persisted.is_some() {
                continue;
            }
            let id = self.next_segment_id;
            self.next_segment_id += 1;
            self.segments[i].write_to(dir, &field, id)?;
        }
        // Only now — the replacements are durable, so what they replaced can go.
        for path in std::mem::take(&mut self.retired_segment_files) {
            let post = path.with_extension("post");
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&post);
        }
        Ok(())
    }

    /// Load every persisted segment for this field back from `dir`.
    ///
    /// Segments are found by **scanning the directory**, not from a manifest: a
    /// single index of everything is a single point of total loss, and Law 5 asks
    /// that recovery not require the thing that was damaged.
    pub fn load_segments(&mut self, dir: &std::path::Path) -> std::io::Result<()> {
        let field = self.meta.field.clone();
        let prefix = format!("bm25_{field}.s");
        let mut found: Vec<(u64, std::path::PathBuf)> = Vec::new();
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(rest) = name.strip_prefix(&prefix) else { continue };
            let Some(idstr) = rest.strip_suffix(".meta") else { continue };
            let Ok(id) = idstr.parse::<u64>() else { continue };
            found.push((id, entry.path()));
        }
        // Oldest first, so slot order matches the order they were written.
        found.sort_by_key(|(id, _)| *id);
        for (id, meta_path) in found {
            let post_path = dir.join(format!("bm25_{field}.s{id}.post"));
            match Bm25Segment::open_from(&meta_path, &post_path) {
                Ok(Some(mut seg)) => {
                    seg.id = id;
                    self.next_segment_id = self.next_segment_id.max(id + 1);
                    self.segments.push(seg);
                }
                // A segment that cannot be vouched for is left out rather than
                // served. It names what was lost by simply not being there.
                Ok(None) | Err(_) => {}
            }
        }
        Ok(())
    }

    /// Documents held in flushed segments, i.e. not in the base and not in the
    /// delta.
    pub fn segment_docs(&self) -> u64 {
        self.segments.iter().map(|s| s.num_docs).sum()
    }

    /// How many segments a query currently has to consult beyond the base.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    pub fn merge_delta(&mut self) {
        if self.delta.is_empty() && self.segments.is_empty() && self.orphan_count() == 0 {
            return;
        }

        // Gather every live document length first, so the merged segment is
        // compact (no orphan slots) and slot indices can be reassigned.
        let mut live: Vec<(u64, u32)> = self.doc_id_to_idx.sorted()
            .into_iter()
            .map(|(doc_id, idx)| (doc_id, self.doc_lengths.get(idx as usize)))
            .collect();
        for seg in &self.segments {
            live.extend(seg.live_docs());
        }
        for (&doc_id, &dl) in &self.delta.doc_lengths {
            live.push((doc_id, dl));
        }
        live.sort_unstable_by_key(|(d, _)| *d);

        let mut doc_lengths: Vec<u32> = Vec::with_capacity(live.len());
        let mut doc_id_to_idx: HashMap<u64, usize> = HashMap::with_capacity(live.len());
        let mut sum_doc_len: u64 = 0;
        for (doc_id, dl) in &live {
            doc_id_to_idx.insert(*doc_id, doc_lengths.len());
            doc_lengths.push(*dl);
            sum_doc_len += *dl as u64;
        }

        // Union of base and delta terms, in sorted order (the dictionary is
        // sorted, and BTreeMap iterates sorted, so this stays cheap).
        let mut terms: Vec<String> = self.dict.iter().map(|(t, _)| t.to_string()).collect();
        for seg in &self.segments {
            terms.extend(seg.dict.iter().map(|(t, _)| t.to_string()));
        }
        for t in self.delta.terms.keys() {
            terms.push(t.clone());
        }
        terms.sort();
        terms.dedup();

        let mut dict = TermDict::new();
        let mut blob: Vec<u8> = Vec::new();
        let mut offset: u64 = 0;

        // A merge folds in a delta of a few thousand documents, but the loop
        // below rewrites the WHOLE dictionary. For the terms the delta never
        // mentions, that work is: read the bytes, decode them into postings,
        // filter nothing out, and encode them back to the identical bytes.
        //
        // A delta touches a small fraction of a real dictionary, so this is the
        // bulk of a merge, and copying the encoded range verbatim skips all of
        // it. The output blob is byte-identical either way.
        //
        // Only sound while nothing has been deleted from the base: a deletion
        // has to be filtered out of every posting list, which means every list
        // genuinely does have to be decoded. `orphan_count` is exactly that
        // question, and a merge leaves it at zero.
        //
        // The sacrifice: none in output, one extra `orphan_count` call, and the
        // copied range keeps whatever encoding it already had rather than being
        // re-encoded under any newer scheme. A version bump forces a full
        // rebuild anyway, which is where re-encoding belongs.
        // Segments carry postings for terms the delta may not mention, so the
        // verbatim shortcut is only sound when there are none. Copying a base
        // range while a segment held newer postings for the same term would drop
        // those documents from the term, silently.
        let untouched_keep_their_bytes =
            self.orphan_count() == 0 && self.segments.is_empty();

        for term in terms {
            let delta_postings = self.delta.postings(&term);

            if untouched_keep_their_bytes && delta_postings.is_empty() {
                let raw = match self.dict.get(&term) {
                    Some(entry) => self.postings.read(entry.postings_offset, entry.postings_len),
                    None => continue,
                };
                if raw.is_empty() {
                    continue;
                }
                while offset % 8 != 0 { blob.push(0); offset += 1; }
                dict.insert(term, offset, raw.len() as u32);
                blob.extend_from_slice(&raw);
                offset += raw.len() as u64;
                continue;
            }

            let mut postings: Vec<Posting> = match self.dict.get(&term) {
                Some(entry) => self.get_postings(entry)
                    .into_iter()
                    .filter(|p| doc_id_to_idx.contains_key(&p.doc_id))
                    .collect(),
                None => Vec::new(),
            };
            for seg in &self.segments {
                for p in seg.postings_for(&term) {
                    match postings.binary_search_by_key(&p.doc_id, |q| q.doc_id) {
                        Ok(i) => postings[i] = p,
                        Err(i) => postings.insert(i, p),
                    }
                }
            }
            for p in delta_postings {
                match postings.binary_search_by_key(&p.doc_id, |q| q.doc_id) {
                    Ok(i) => postings[i] = p,
                    Err(i) => postings.insert(i, p),
                }
            }
            if postings.is_empty() { continue; }

            let bytes = encode_postings_to_file(&postings);
            // Keep every list 8-byte aligned, as `build` does, so the blob stays
            // mmap-friendly.
            while offset % 8 != 0 { blob.push(0); offset += 1; }
            dict.insert(term, offset, bytes.len() as u32);
            blob.extend_from_slice(&bytes);
            offset += bytes.len() as u64;
        }

        self.meta.num_docs = doc_lengths.len() as u64;
        self.meta.avg_doc_len = if doc_lengths.is_empty() {
            1.0
        } else {
            sum_doc_len as f64 / doc_lengths.len() as f64
        };
        self.dict = dict;
        self.postings = PostingsBlob::Memory(blob);
        self.doc_lengths = DocLens::Owned(doc_lengths);
        self.doc_id_to_idx = DocIdx::Owned(doc_id_to_idx);
        self.sum_doc_len = sum_doc_len;
        self.delta = Bm25Delta::default();
        // Everything they held is now in the base, so their files are dead.
        for seg in &self.segments {
            if let Some(path) = &seg.persisted {
                self.retired_segment_files.push(path.clone());
            }
        }
        self.segments.clear();
    }

    // ── Private helpers ───────────────────────────────────────────────

    /// Decode the postings list for a term dictionary entry.
    /// Live postings for `term` across the base and the delta.
    ///
    /// This is the one place the two segments are joined, so every scoring path
    /// that goes through it is automatically delta-aware. Base postings for
    /// deleted documents are filtered here; the delta filters its own.
    fn term_postings(&self, term: &str) -> Vec<Posting> {
        let mut postings: Vec<Posting> = match self.dict.get(term) {
            Some(entry) => self.get_postings(entry)
                .into_iter()
                .filter(|p| self.doc_id_to_idx.get(p.doc_id).is_some())
                .collect(),
            None => Vec::new(),
        };
        // Every segment, then the delta. A read path that skips one of these
        // does not error — it silently returns fewer documents.
        for seg in &self.segments {
            for p in seg.postings_for(term) {
                match postings.binary_search_by_key(&p.doc_id, |q| q.doc_id) {
                    Ok(i) => postings[i] = p,
                    Err(i) => postings.insert(i, p),
                }
            }
        }
        if !self.delta.is_empty() {
            for p in self.delta.postings(term) {
                match postings.binary_search_by_key(&p.doc_id, |q| q.doc_id) {
                    Ok(i) => postings[i] = p,
                    Err(i) => postings.insert(i, p),
                }
            }
        }
        postings
    }

    /// Token count for a live document, whichever segment holds it.
    /// `None` means deleted or never indexed.
    fn live_doc_len(&self, doc_id: u64) -> Option<u32> {
        if let Some(idx) = self.doc_id_to_idx.get(doc_id) {
            return Some(self.doc_lengths.get(idx));
        }
        for seg in &self.segments {
            if let Some(dl) = seg.live_doc_len(doc_id) {
                return Some(dl);
            }
        }
        self.delta.doc_lengths.get(&doc_id).copied()
    }

    fn get_postings(&self, entry: &super::dict::TermEntry) -> Vec<Posting> {
        let bytes = self.postings.read(entry.postings_offset, entry.postings_len);
        if bytes.is_empty() { return Vec::new(); }
        decode_postings_from_bytes(&bytes)
    }

    /// Look up the `doc_lengths` slot index for a given node hash.
    /// Returns `None` if the document has been deleted or was never
    /// indexed.
    fn doc_id_to_index(&self, doc_id: u64) -> Option<usize> {
        self.doc_id_to_idx.get(doc_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_index() -> Bm25Index {
        let docs = vec![
            (1u64, "Rust is a systems programming language"),
            (2u64, "Python is great for beginners"),
            (3u64, "Rust async runtime and performance"),
            (4u64, "Learning Rust programming"),
            (5u64, "Go is a modern systems language"),
        ];
        Bm25Index::build("content", docs.into_iter())
    }

    #[test]
    fn test_bm25_build_and_search() {
        let index = sample_index();
        assert_eq!(index.num_docs(), 5);
        assert!(index.num_terms() > 0);

        let hits = index.search("rust", 10);
        assert!(!hits.is_empty(), "expected hits for 'rust'");
        assert!(hits.iter().any(|h| h.doc_id == 1));
        assert!(hits.iter().any(|h| h.doc_id == 3));
        assert!(hits.iter().any(|h| h.doc_id == 4));

        let hits = index.search("python", 10);
        assert!(hits.iter().any(|h| h.doc_id == 2));

        let hits = index.search("xyzabc", 10);
        assert!(hits.is_empty());
    }

    /// Deleted doc must not appear in search results.
    /// The liveness gate in `search` (doc_id_to_idx lookup) makes the
    /// deleted document invisible immediately — no rebuild required.
    #[test]
    fn test_delete_removes_from_results() {
        let mut index = sample_index();
        assert_eq!(index.num_docs(), 5);

        assert!(index.delete(1), "doc 1 should be found and removed");
        assert_eq!(index.num_docs(), 4);
        assert_eq!(index.orphan_count(), 1);

        let hits = index.search("rust", 10);
        assert!(!hits.iter().any(|h| h.doc_id == 1), "deleted doc must not appear");
        assert!(hits.iter().any(|h| h.doc_id == 3), "doc 3 must still appear");
        assert!(hits.iter().any(|h| h.doc_id == 4), "doc 4 must still appear");
    }

    /// Deleting a doc that was never indexed (or already deleted) must
    /// return false and leave the index unchanged.
    #[test]
    fn test_delete_nonexistent_returns_false() {
        let mut index = sample_index();
        assert!(!index.delete(999));
        assert_eq!(index.num_docs(), 5);
        assert_eq!(index.orphan_count(), 0);
    }

    /// `needs_rebuild` must cross the threshold only after enough
    /// deletions accumulate.
    #[test]
    fn test_needs_rebuild_threshold() {
        // 10 docs; threshold 0.20 triggers when orphan_count > 2
        let docs: Vec<(u64, &str)> = (1u64..=10)
            .map(|i| (i, "Melbourne suburb Fitzroy artist live music"))
            .collect();
        let mut index = Bm25Index::build("content", docs.into_iter());

        index.delete(1);
        index.delete(2);
        // 2/10 = 0.20 — exactly at threshold, not above it
        assert!(!index.needs_rebuild(0.20));

        index.delete(3);
        // 3/10 = 0.30 — now above threshold
        assert!(index.needs_rebuild(0.20));
    }

    /// `avg_doc_len` must reflect only live documents after deletions.
    #[test]
    fn test_avg_doc_len_accurate_after_delete() {
        let docs = vec![
            // 10 tokens (counted after tokenise: ≥3-char filter applies)
            (1u64, "one two three four five six seven eight nine ten"),
            // 1 token
            (2u64, "one"),
        ];
        let mut index = Bm25Index::build("content", docs.into_iter());

        // tokenize keeps terms ≥3 chars; "one" passes, short words may not —
        // just verify the relative change, not exact values.
        let avg_before = index.avg_doc_len();
        assert!(avg_before > 0.0);

        // Delete the short doc; avg should rise (long doc now alone).
        index.delete(2);
        let avg_after = index.avg_doc_len();
        assert!(
            avg_after >= avg_before,
            "avg_doc_len should be >= before after removing the shorter doc: before={avg_before} after={avg_after}"
        );
    }
}
