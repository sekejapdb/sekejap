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
            DocIdx::Mapped { .. } => Vec::new(),
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
}

impl Bm25Index {
    /// Approximate resident RAM held by this index, in bytes: the compressed
    /// postings blob + term dictionary + per-doc length array + id→slot map.
    pub fn mem_bytes(&self) -> usize {
        self.postings.mem_bytes()
            + self.dict.mem_bytes()
            + match &self.doc_lengths { DocLens::Owned(v) => v.capacity() * 4, DocLens::Mapped { .. } => 0 }
            + match &self.doc_id_to_idx { DocIdx::Owned(m) => m.capacity() * 24, DocIdx::Mapped { .. } => 0 }
    }

    /// Spill the postings blob to `path` and switch to disk-backed reads,
    /// freeing the in-RAM blob. Makes the index **disk-first**: only the term
    /// dictionary + doc arrays stay resident; postings are `pread` at query time.
    #[cfg(unix)]
    pub fn spill_to_disk(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        if let PostingsBlob::Memory(blob) = &self.postings {
            let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
            f.write_all(blob)?;
            f.sync_all()?;
            let len = blob.len() as u64;
            self.postings = PostingsBlob::Disk { file: std::sync::Arc::new(f), len };
        }
        Ok(())
    }

    /// Serialize the resident metadata (dict + doc_lengths + doc_id_to_idx + counters)
    /// to the bm25.bin container. Postings stay in the separate `bm25_<field>.postings`
    /// file (already spilled). Format is documented in `open_mapped`.
    pub(crate) fn write_binary<W: std::io::Write>(&self, w: &mut W) -> std::io::Result<()> {
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
        let ru16 = |b: &[u8], p: usize| u16::from_le_bytes([b[p], b[p + 1]]);
        let ru32 = |b: &[u8], p: usize| u32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]);
        let ru64 = |b: &[u8], p: usize| u64::from_le_bytes(b[p..p + 8].try_into().unwrap());

        let mut p = 0usize;
        let flen = ru16(b, p) as usize; p += 2;
        let field = std::str::from_utf8(b.get(p..p + flen).ok_or_else(|| bad("bm25 field"))?)
            .map_err(|_| bad("bm25 field utf8"))?.to_string();
        p += flen;
        let num_docs = ru64(b, p); p += 8;
        let sum_doc_len = ru64(b, p); p += 8;

        let doc_count = ru32(b, p) as usize; p += 4;
        let dl_off = base + p; p += doc_count * 4;

        let idmap_count = ru32(b, p) as usize; p += 4;
        let idmap_off = base + p; p += idmap_count * 12;

        let term_count = ru32(b, p) as usize; p += 4;
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
        #[cfg(unix)]
        let postings = PostingsBlob::Disk { file: std::sync::Arc::new(pfile), len: plen };
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
                if let Some(entry) = self.dict.get(t) {
                    let postings = self.get_postings(entry);
                    let df = postings.len() as f64;
                    // Smoothed IDF (Lucene / BM25+ variant): `ln(1 + …)` stays
                    // positive even when a term appears in >50% of docs, instead
                    // of the classic RSJ form which floors to 0 for common terms.
                    let idf = (1.0 + (self.meta.num_docs as f64 - df + 0.5) / (df + 0.5)).ln();
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
            let entry = self.dict.get(term).unwrap();
            let postings = self.get_postings(entry);

            for posting in postings {
                let doc_idx = match self.doc_id_to_index(posting.doc_id) {
                    Some(idx) => idx,
                    // Document was deleted (entry removed from doc_id_to_idx).
                    None => continue,
                };

                let doc_len = self.doc_lengths.get(doc_idx) as f64;
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
            b.score.partial_cmp(&a.score).unwrap().then(a.doc_id.cmp(&b.doc_id))
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
        if self.meta.num_docs == 0 {
            1.0
        } else {
            self.sum_doc_len as f64 / self.meta.num_docs as f64
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
        self.meta.num_docs
    }

    /// Number of unique terms in the dictionary.
    pub fn num_terms(&self) -> usize {
        self.dict.num_terms()
    }

    // ── Private helpers ───────────────────────────────────────────────

    /// Decode the postings list for a term dictionary entry.
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
