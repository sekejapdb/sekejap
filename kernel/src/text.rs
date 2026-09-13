//! 2h: full-text search as key discipline (FACT-03).
//!
//! A newcomer's map: an inverted index answers "which documents contain
//! this word?" -- it inverts documents into (word -> documents). Here the
//! btree IS that index: the sorted term keys are the dictionary (prefix
//! search = a range scan), and postings live in two shapes:
//!
//!  - the HEAD (segment 0): one row per (term, doc), written BLIND at
//!    index time -- searchable the instant it lands, no build step;
//!  - FOLDED segments (1..): one value per term with packed
//!    (docid-delta, tf) varints -- compact, immutable, produced by
//!    folding the head (2h fold, bulk-dock shape).
//!
//! BM25 in one breath: score = idf(term) * tf / (tf + k1*(1-b+b*|d|/avg)).
//! idf rewards rare words, the denominator saturates repeated words and
//! normalises by document length. Everything it needs lives in keyed rows:
//! per-(field,doc) token counts (0x0D) and per-(field,seg) totals (0x0E).

use crate::graph::Graph;
use crate::keys;
use crate::{Error, Result};
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};

pub const BM25_K1: f32 = 1.2;
pub const BM25_B: f32 = 0.75;

/// Lowercase alphanumeric tokens; everything else separates. Deliberately
/// simple (no stemming, no stopwords) -- linguistic layers stack on later
/// without changing the keyspace.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() { cur.push(lc); }
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() { out.push(cur); }
    out
}

/// Varint (LEB128) helpers for packed postings.
pub fn write_varint(v: &mut Vec<u8>, mut x: u64) {
    loop {
        let b = (x & 0x7F) as u8;
        x >>= 7;
        if x == 0 { v.push(b); break; }
        v.push(b | 0x80);
    }
}
pub fn read_varint(b: &[u8], pos: &mut usize) -> Option<u64> {
    let mut x = 0u64; let mut shift = 0;
    loop {
        let byte = *b.get(*pos)?;
        *pos += 1;
        x |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 { return Some(x); }
        shift += 7;
        if shift > 63 { return None; }
    }
}

const SEG_MAGIC: &[u8] = b"TSEG2";
const FIELD_MAGIC: &[u8] = b"TFM3";
const NORM_MAGIC: &[u8] = b"TN3";
const POSTING_BLOCK: usize = 128;
const TEXT_BUILD_BLOCK_MAGIC: &[u8] = b"TB1";
const TEXT_BUILD_OPEN_BUDGET: usize = 32 << 20;
const TEXT_BUILD_OUTPUT_BUDGET: usize = 16 << 20;
const MERGE_FANOUT: usize = 8;
const BUILD_TERM_CACHE: usize = 4096;

struct PostingSource<'a> {
    scan: Option<crate::btree::RangeIter<'a>>,
    prefix: Vec<u8>,
    pending: VecDeque<(u64, u64, u64)>,
    dead: HashSet<u64>,
    head: bool,
    blocks_read: u64,
    postings_decoded: u64,
}

impl PostingSource<'_> {
    fn next_live(&mut self) -> Result<Option<(u64, u64, u64)>> {
        loop {
            while let Some(posting) = self.pending.pop_front() {
                if !self.dead.contains(&posting.0) { return Ok(Some(posting)); }
            }
            let Some(scan) = self.scan.as_mut() else { return Ok(None) };
            let Some(item) = scan.next() else { self.scan = None; return Ok(None) };
            let (key, value) = item?;
            if !key.starts_with(&self.prefix) || key.len() != self.prefix.len() + 8 {
                self.scan = None;
                return Ok(None);
            }
            self.blocks_read += 1;
            if self.head {
                let doc = u64::from_be_bytes(key[self.prefix.len()..].try_into().unwrap());
                let mut pos = 0;
                let tf = required_varint(&value, &mut pos, "head posting frequency is truncated")?;
                let dl = required_varint(&value, &mut pos, "head posting length is truncated")?;
                if pos != value.len() || tf == 0 {
                    return Err(corrupt("head posting has invalid trailing bytes or frequency"));
                }
                self.postings_decoded += 1;
                if !self.dead.contains(&doc) { return Ok(Some((doc, tf, dl))); }
            } else {
                let posts = decode_postings(&value)?;
                if posts.len() > POSTING_BLOCK {
                    return Err(corrupt("text posting block exceeds its bound"));
                }
                self.postings_decoded += posts.len() as u64;
                self.pending.extend(posts);
            }
        }
    }
}

struct TextPostingCursor<'a> {
    sources: Vec<PostingSource<'a>>,
    heads: Vec<Option<(u64, u64, u64)>>,
}

impl TextPostingCursor<'_> {
    fn next(&mut self) -> Result<Option<(u64, u64, u64)>> {
        let Some(doc) = self.heads.iter().flatten().map(|posting| posting.0).min() else {
            return Ok(None);
        };
        let mut chosen = None;
        for i in 0..self.sources.len() {
            if self.heads[i].is_some_and(|posting| posting.0 == doc) {
                // Head is appended last and therefore wins the only legitimate
                // cross-source duplicate: a replacement published beside its
                // not-yet-retired old segment.
                chosen = self.heads[i];
                self.heads[i] = self.sources[i].next_live()?;
            }
        }
        Ok(chosen)
    }

    fn counters(&self) -> (u64, u64) {
        self.sources.iter().fold((0, 0), |(blocks, postings), source| {
            (blocks + source.blocks_read, postings + source.postings_decoded)
        })
    }
}

struct RankedText { id: u64, score: f64 }
impl PartialEq for RankedText {
    fn eq(&self, other: &Self) -> bool { self.id == other.id && self.score == other.score }
}
impl Eq for RankedText {}
impl Ord for RankedText {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // The worst retained hit is the max-heap root: lower score, then larger id.
        other.score.total_cmp(&self.score).then(self.id.cmp(&other.id))
    }
}
impl PartialOrd for RankedText {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}

/// A fully sorted and logically verified text segment that has not touched the
/// shared tree. It is Send because it owns only run-file descriptors and
/// scalar manifests; D26 publication still requires a caller-owned Graph.
pub struct PackedTextCandidate {
    field: u64,
    posting_runs: crate::bulk::SortedRuns,
    norm_runs: crate::bulk::SortedRuns,
    membership_runs: crate::bulk::SortedRuns,
    dictionary_runs: crate::bulk::SortedRuns,
    expected: SegMeta,
    doc_count: u64,
    total_tokens: u64,
    posting_rows: u64,
    emissions: u64,
    partials: u64,
    block_rows: u64,
    block_min: Option<Vec<u8>>,
    block_max: Option<Vec<u8>>,
    norm_rows: u64,
    norm_min: Option<Vec<u8>>,
    norm_max: Option<Vec<u8>>,
    member_rows: u64,
    member_min: Option<Vec<u8>>,
    member_max: Option<Vec<u8>>,
    dict_rows: u64,
    dict_min: Option<Vec<u8>>,
    dict_max: Option<Vec<u8>>,
    scan_stage: std::time::Duration,
    source_finish: std::time::Duration,
    posting_merge: std::time::Duration,
    block_term_finish: std::time::Duration,
    dictionary_stage: std::time::Duration,
    dictionary_finish: std::time::Duration,
    validate: std::time::Duration,
    prepare_total: std::time::Duration,
    posting_scratch: u64,
    norm_scratch: u64,
    membership_scratch: u64,
    term_scratch: u64,
    dictionary_scratch: u64,
    partial_block_scratch: u64,
    posting_input_runs: usize,
    norm_input_runs: usize,
    membership_input_runs: usize,
    term_input_runs: usize,
    dictionary_input_runs: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct PreparedPackedTextDoc {
    doc: u64,
    dl: u64,
    terms: Vec<(String, u64)>,
    norm_terms: Vec<(u64, u64)>,
}

fn prepare_packed_text_doc(doc: u64, text: &str) -> Result<PreparedPackedTextDoc> {
    let tokens = tokenize(text);
    let dl = tokens.len() as u64;
    let mut tf = std::collections::BTreeMap::<String, u64>::new();
    for term in tokens { *tf.entry(term).or_insert(0) += 1; }

    let terms: Vec<_> = tf.into_iter().collect();
    let mut norm_terms: Vec<_> = terms.iter()
        .map(|(term, count)| (term_number(term.as_bytes()), *count))
        .collect();
    norm_terms.sort_unstable_by_key(|&(id, _)| id);
    if norm_terms.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(corrupt("packed text document contains colliding term numbers"));
    }
    Ok(PreparedPackedTextDoc { doc, dl, terms, norm_terms })
}

fn text_prepare_worker_limit() -> usize {
    std::env::var("SEKEJAP_INDEX_BUILD_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from))
        .clamp(1, 8)
}

#[cfg(test)]
mod parallel_prepare_tests {
    use super::*;

    #[test]
    fn packed_document_transform_is_deterministic() {
        let text = "Railway railway junction café";
        assert_eq!(
            prepare_packed_text_doc(17, text).unwrap(),
            prepare_packed_text_doc(17, text).unwrap(),
        );
    }

    #[test]
    fn packed_document_transform_preserves_document_identity_and_length() {
        let prepared = prepare_packed_text_doc(9_223_372_036_854_775_000, "one two two").unwrap();
        assert_eq!(prepared.doc, 9_223_372_036_854_775_000);
        assert_eq!(prepared.dl, 3);
        assert_eq!(prepared.terms, vec![("one".into(), 1), ("two".into(), 2)]);
    }

    #[test]
    fn packed_document_transform_canonicalizes_norm_term_order() {
        let prepared = prepare_packed_text_doc(1, "zulu alpha beta alpha").unwrap();
        assert!(prepared.norm_terms.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(prepared.norm_terms.iter().map(|(_, count)| count).sum::<u64>(), 4);
    }
}

/// A fixed-size, direct-mapped accelerator used only while initially
/// backfilling an index.  It never grows with the corpus; long words bypass it
/// so its retained memory is below one MiB.
pub struct TextBuildCache {
    slots: Vec<Option<(String, u64)>>,
}

/// Disk-first accumulator for an initial index build.  A fixed 4 MiB table
/// combines repeated `(raw term number, exact word)` records before a fixed
/// 4 MiB external sort. Finishing resolves the vanishingly rare number
/// collision exactly and writes dictionary rows in key order.
pub struct TextBuildAccumulator {
    sort: Option<crate::bulk::ExternalSort>,
    scratch: std::path::PathBuf,
    grouped: HashMap<Vec<u8>, u64>,
    grouped_bytes: usize,
    docs: u64,
    tokens: u64,
}

impl TextBuildAccumulator {
    pub fn new() -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = std::env::temp_dir().join(format!("text-build-{}-{}",
            std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        Ok(Self { sort: None, scratch, grouped: HashMap::new(), grouped_bytes: 0,
            docs: 0, tokens: 0 })
    }

    fn document(&mut self, total: u64) -> Result<()> {
        self.docs = self.docs.checked_add(1).ok_or(Error::TooLarge)?;
        self.tokens = self.tokens.checked_add(total).ok_or(Error::TooLarge)?;
        Ok(())
    }

    fn term(&mut self, term: &str, _docid: u64) -> Result<u64> {
        let id = term_number(term.as_bytes());
        let mut key = Vec::with_capacity(term.len() + 8);
        key.extend_from_slice(&id.to_be_bytes()); key.extend_from_slice(term.as_bytes());
        if let Some(count) = self.grouped.get_mut(&key) {
            *count = count.checked_add(1).ok_or(Error::TooLarge)?;
            return Ok(id);
        }
        if self.grouped_bytes.saturating_add(key.len() + 64) > (4 << 20) {
            self.flush()?;
        }
        self.grouped_bytes = self.grouped_bytes.saturating_add(key.len() + 64);
        self.grouped.insert(key, 1);
        Ok(id)
    }

    fn flush(&mut self) -> Result<()> {
        if self.sort.is_none() {
            self.sort = Some(crate::bulk::ExternalSort::new(&self.scratch, 4 << 20)?);
        }
        let sort = self.sort.as_mut().unwrap();
        for (key, count) in self.grouped.drain() {
            sort.push(key, count.to_be_bytes().to_vec())?;
        }
        self.grouped_bytes = 0;
        Ok(())
    }
}

impl Default for TextBuildCache {
    fn default() -> Self { Self { slots: (0..BUILD_TERM_CACHE).map(|_| None).collect() } }
}

impl TextBuildCache {
    fn slot(term: &str) -> usize { term_number(term.as_bytes()) as usize & (BUILD_TERM_CACHE - 1) }
    fn get(&self, term: &str) -> Option<u64> {
        self.slots[Self::slot(term)].as_ref()
            .and_then(|(stored, id)| (stored == term).then_some(*id))
    }
    fn insert(&mut self, term: &str, id: u64) {
        if term.len() <= 128 { self.slots[Self::slot(term)] = Some((term.to_owned(), id)); }
    }
}

fn corrupt(why: &'static str) -> Error { Error::Corrupt { page_no: 0, why } }

fn required_varint(v: &[u8], pos: &mut usize, why: &'static str) -> Result<u64> {
    read_varint(v, pos).ok_or_else(|| corrupt(why))
}

/// Per-(field, segment) manifest.  The logical hashes are computed from the
/// source postings before the candidate is written and recomputed by the
/// independently reopened verifier.  Counts catch truncation; the two
/// differently seeded hashes catch changed/reordered logical records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegMeta {
    pub doc_count: u64,
    pub total_tokens: u64,
    /// Token occurrences belonging to dead docs, captured from the norm
    /// row at delete time -- so avg document length is identical before
    /// and after a fold physically drops the dead (score-drift bug).
    pub dead_tokens: u64,
    pub dead: Vec<u64>,
    pub level: u32,
    pub term_rows: u64,
    pub posting_count: u64,
    pub logical_xor: u64,
    pub logical_sum: u64,
}
impl SegMeta {
    pub fn decode(v: &[u8]) -> Result<SegMeta> {
        if !v.starts_with(SEG_MAGIC) {
            // Read compatibility for pre-manifest stores.  New writes always
            // use the exact format below; malformed mandatory legacy fields
            // are refused rather than silently becoming zero.
            let mut pos = 0usize;
            let doc_count = required_varint(v, &mut pos, "text segment has no document count")?;
            let total_tokens = required_varint(v, &mut pos, "text segment has no token total")?;
            let dead_tokens = required_varint(v, &mut pos, "text segment has no dead-token total")?;
            let mut dead = Vec::new();
            let mut last = 0u64;
            while pos < v.len() {
                let delta = required_varint(v, &mut pos, "text segment dead list is truncated")?;
                last = last.checked_add(delta).ok_or_else(|| corrupt("text segment dead id overflows"))?;
                dead.push(last);
            }
            return Ok(SegMeta { doc_count, total_tokens, dead_tokens, dead, level: 0,
                term_rows: 0, posting_count: 0, logical_xor: 0, logical_sum: 0 });
        }
        let mut pos = SEG_MAGIC.len();
        let doc_count = required_varint(v, &mut pos, "text segment has no document count")?;
        let total_tokens = required_varint(v, &mut pos, "text segment has no token total")?;
        let dead_tokens = required_varint(v, &mut pos, "text segment has no dead-token total")?;
        let level = required_varint(v, &mut pos, "text segment has no level")?;
        let term_rows = required_varint(v, &mut pos, "text segment has no term-row count")?;
        let posting_count = required_varint(v, &mut pos, "text segment has no posting count")?;
        let logical_xor = required_varint(v, &mut pos, "text segment has no xor manifest")?;
        let logical_sum = required_varint(v, &mut pos, "text segment has no sum manifest")?;
        let dead_count = required_varint(v, &mut pos, "text segment has no dead count")?;
        if level > u32::MAX as u64 || dead_count > usize::MAX as u64 {
            return Err(corrupt("text segment metadata exceeds format bounds"));
        }
        let mut dead = Vec::with_capacity(dead_count as usize);
        let mut last = 0u64;
        for _ in 0..dead_count {
            let delta = required_varint(v, &mut pos, "text segment dead list is truncated")?;
            last = last.checked_add(delta).ok_or_else(|| corrupt("text segment dead id overflows"))?;
            dead.push(last);
        }
        if pos != v.len() { return Err(corrupt("text segment metadata has trailing bytes")); }
        Ok(SegMeta { doc_count, total_tokens, dead_tokens, dead, level: level as u32,
            term_rows, posting_count, logical_xor, logical_sum })
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut v = SEG_MAGIC.to_vec();
        write_varint(&mut v, self.doc_count);
        write_varint(&mut v, self.total_tokens);
        write_varint(&mut v, self.dead_tokens);
        write_varint(&mut v, self.level as u64);
        write_varint(&mut v, self.term_rows);
        write_varint(&mut v, self.posting_count);
        write_varint(&mut v, self.logical_xor);
        write_varint(&mut v, self.logical_sum);
        let mut sorted = self.dead.clone();
        sorted.sort_unstable(); sorted.dedup();
        write_varint(&mut v, sorted.len() as u64);
        let mut last = 0u64;
        for d in sorted { write_varint(&mut v, d - last); last = d; }
        v
    }
}

#[derive(Clone, Debug)]
struct FieldMeta {
    live_docs: u64,
    total_tokens: u64,
    next_seg: u32,
    generation: u64,
    active: Vec<(u32, u32)>, // (segment, level)
    head_redirect: Option<u32>,
    redirects: Vec<(u32, u32)>,
    term_stats_ready: bool,
}

impl Default for FieldMeta {
    fn default() -> Self {
        Self { live_docs: 0, total_tokens: 0, next_seg: 1, generation: 0,
            active: Vec::new(), head_redirect: None, redirects: Vec::new(), term_stats_ready: true }
    }
}

impl FieldMeta {
    fn decode(v: &[u8]) -> Result<Self> {
        if !v.starts_with(FIELD_MAGIC) { return Err(corrupt("text field manifest has an unknown format")); }
        let mut pos = FIELD_MAGIC.len();
        let live_docs = required_varint(v, &mut pos, "text field manifest has no document count")?;
        let total_tokens = required_varint(v, &mut pos, "text field manifest has no token total")?;
        let next_seg = required_varint(v, &mut pos, "text field manifest has no next segment")?;
        let generation = required_varint(v, &mut pos, "text field manifest has no generation")?;
        let term_stats_ready = required_varint(v, &mut pos, "text field manifest has no term-stat state")?;
        if term_stats_ready > 1 { return Err(corrupt("text field term-stat state is invalid")); }
        let head = required_varint(v, &mut pos, "text field manifest has no head state")?;
        let nactive = required_varint(v, &mut pos, "text field manifest has no active count")?;
        if next_seg >= keys::TEXT_TERM_STATS_SEG as u64 || nactive > 1024 {
            return Err(corrupt("text field manifest exceeds format bounds"));
        }
        let mut active = Vec::with_capacity(nactive as usize);
        for _ in 0..nactive {
            let seg = required_varint(v, &mut pos, "text field active segment is truncated")?;
            let level = required_varint(v, &mut pos, "text field active level is truncated")?;
            if seg == 0 || seg >= keys::TEXT_TERM_STATS_SEG as u64 || level > u32::MAX as u64 {
                return Err(corrupt("text field active segment is out of bounds"));
            }
            active.push((seg as u32, level as u32));
        }
        let nr = required_varint(v, &mut pos, "text field manifest has no redirect count")?;
        if nr > 1024 { return Err(corrupt("text field redirect count exceeds its bound")); }
        let mut redirects = Vec::with_capacity(nr as usize);
        for _ in 0..nr {
            let old = required_varint(v, &mut pos, "text field redirect is truncated")?;
            let new = required_varint(v, &mut pos, "text field redirect is truncated")?;
            if old >= keys::TEXT_TERM_STATS_SEG as u64 || new >= keys::TEXT_TERM_STATS_SEG as u64 {
                return Err(corrupt("text field redirect is out of bounds"));
            }
            redirects.push((old as u32, new as u32));
        }
        if pos != v.len() { return Err(corrupt("text field manifest has trailing bytes")); }
        Ok(FieldMeta { live_docs, total_tokens, next_seg: next_seg as u32, generation,
            active, head_redirect: if head == 0 { None } else { Some((head - 1) as u32) }, redirects,
            term_stats_ready: term_stats_ready == 1 })
    }

    fn encode(&self) -> Vec<u8> {
        let mut v = FIELD_MAGIC.to_vec();
        write_varint(&mut v, self.live_docs);
        write_varint(&mut v, self.total_tokens);
        write_varint(&mut v, self.next_seg as u64);
        write_varint(&mut v, self.generation);
        write_varint(&mut v, self.term_stats_ready as u64);
        write_varint(&mut v, self.head_redirect.map_or(0, |s| s as u64 + 1));
        write_varint(&mut v, self.active.len() as u64);
        for &(seg, level) in &self.active { write_varint(&mut v, seg as u64); write_varint(&mut v, level as u64); }
        write_varint(&mut v, self.redirects.len() as u64);
        for &(old, new) in &self.redirects { write_varint(&mut v, old as u64); write_varint(&mut v, new as u64); }
        v
    }

    fn resolve_owner(&self, owner: u32) -> u32 {
        if owner == 0 {
            if let Some(seg) = self.head_redirect { return seg; }
        }
        self.redirects.iter().find_map(|&(old, new)| (old == owner).then_some(new)).unwrap_or(owner)
    }
}

#[derive(Clone, Debug)]
struct Norm {
    total: u64,
    owner: u32,
    /// Sorted `(field-local term number, frequency)` pairs.  The word itself
    /// is interned once per field, rather than repeated in every document.
    terms: Vec<(u64, u64)>,
}

impl Norm {
    fn decode(v: &[u8]) -> Result<Self> {
        if !v.starts_with(NORM_MAGIC) {
            let mut pos = 0;
            let total = required_varint(v, &mut pos, "text norm has no token count")?;
            let owner = required_varint(v, &mut pos, "text norm has no owner")?;
            if pos != v.len() || owner >= keys::TEXT_TERM_STATS_SEG as u64 {
                return Err(corrupt("legacy text norm is malformed"));
            }
            return Ok(Norm { total, owner: owner as u32, terms: Vec::new() });
        }
        let mut pos = NORM_MAGIC.len();
        let total = required_varint(v, &mut pos, "text norm has no token count")?;
        let owner = required_varint(v, &mut pos, "text norm has no owner")?;
        let n = required_varint(v, &mut pos, "text norm has no term count")?;
        if owner >= keys::TEXT_TERM_STATS_SEG as u64 || n > u32::MAX as u64 {
            return Err(corrupt("text norm exceeds format bounds"));
        }
        let mut terms = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let term = required_varint(v, &mut pos, "text norm term number is truncated")?;
            let tf = required_varint(v, &mut pos, "text norm term frequency is truncated")?;
            if term == 0 || tf == 0 || terms.last().is_some_and(|(previous, _)| *previous >= term) {
                return Err(corrupt("text norm terms are invalid or out of order"));
            }
            terms.push((term, tf));
        }
        if pos != v.len() { return Err(corrupt("text norm has trailing bytes")); }
        Ok(Norm { total, owner: owner as u32, terms })
    }

    fn encode(&self) -> Vec<u8> {
        let mut v = NORM_MAGIC.to_vec();
        write_varint(&mut v, self.total);
        write_varint(&mut v, self.owner as u64);
        write_varint(&mut v, self.terms.len() as u64);
        for (term, tf) in &self.terms {
            write_varint(&mut v, *term); write_varint(&mut v, *tf);
        }
        v
    }
}

fn posting_hash(term: &[u8], docid: u64, tf: u64, dl: u64) -> u64 {
    let mut v = Vec::with_capacity(term.len() + 24);
    v.extend_from_slice(term); v.extend_from_slice(&docid.to_be_bytes());
    v.extend_from_slice(&tf.to_be_bytes()); v.extend_from_slice(&dl.to_be_bytes());
    let lo = crc32c::crc32c(&v) as u64;
    v.push(0xA5);
    lo | ((crc32c::crc32c(&v) as u64) << 32)
}

fn decode_postings(v: &[u8]) -> Result<Vec<(u64, u64, u64)>> {
    let mut out = Vec::new();
    let (mut pos, mut last) = (0usize, 0u64);
    while pos < v.len() {
        let delta = required_varint(v, &mut pos, "text posting doc delta is truncated")?;
        let tf = required_varint(v, &mut pos, "text posting term frequency is truncated")?;
        let dl = required_varint(v, &mut pos, "text posting document length is truncated")?;
        if tf == 0 { return Err(corrupt("text posting has zero term frequency")); }
        last = last.checked_add(delta).ok_or_else(|| corrupt("text posting document id overflows"))?;
        if out.last().is_some_and(|(previous, _, _)| *previous >= last) {
            return Err(corrupt("text postings are not strictly ordered"));
        }
        out.push((last, tf, dl));
    }
    Ok(out)
}

#[derive(Default)]
struct TextOpenBlock {
    rows: Vec<(u64, u64, u64)>,
    last_seen: u64,
}

impl TextOpenBlock {
    fn new(doc: u64, tf: u64, dl: u64) -> Self {
        let mut rows = Vec::with_capacity(POSTING_BLOCK);
        rows.push((doc, tf, dl));
        Self { rows, last_seen: doc }
    }
}

fn encode_text_build_block(block: &TextOpenBlock) -> Vec<u8> {
    debug_assert!(!block.rows.is_empty() && block.rows.len() <= POSTING_BLOCK);
    let mut out = Vec::with_capacity(TEXT_BUILD_BLOCK_MAGIC.len() + 1 + block.rows.len() * 24);
    out.extend_from_slice(TEXT_BUILD_BLOCK_MAGIC);
    out.push(block.rows.len() as u8);
    for &(doc, tf, dl) in &block.rows {
        out.extend_from_slice(&doc.to_le_bytes());
        write_varint(&mut out, tf);
        write_varint(&mut out, dl);
    }
    out
}

fn decode_text_build_block(bytes: &[u8]) -> Result<Vec<(u64, u64, u64)>> {
    if !bytes.starts_with(TEXT_BUILD_BLOCK_MAGIC) || bytes.len() < 4 {
        return Err(corrupt("text accumulator block has an invalid header"));
    }
    let count = bytes[TEXT_BUILD_BLOCK_MAGIC.len()] as usize;
    if count == 0 || count > POSTING_BLOCK {
        return Err(corrupt("text accumulator block has an invalid posting count"));
    }
    let mut rows = Vec::with_capacity(count);
    let mut pos = 4usize;
    for _ in 0..count {
        let end = pos.checked_add(8).ok_or(Error::TooLarge)?;
        let raw = bytes.get(pos..end)
            .ok_or_else(|| corrupt("text accumulator document id is truncated"))?;
        let doc = u64::from_le_bytes(raw.try_into().unwrap());
        pos = end;
        let tf = required_varint(bytes, &mut pos, "text accumulator frequency is truncated")?;
        let dl = required_varint(bytes, &mut pos, "text accumulator length is truncated")?;
        if tf == 0 { return Err(corrupt("text accumulator has zero term frequency")); }
        rows.push((doc, tf, dl));
    }
    if pos != bytes.len() || rows.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(corrupt("text accumulator block is malformed or unordered"));
    }
    Ok(rows)
}

/// Fixed-memory posting accumulator for initial BM25/SEARCH builds. Documents
/// arrive in increasing id order, so only the term streams interleave. One
/// open 128-document block per hot term removes the row-per-posting scratch
/// materialisation. If the vocabulary exceeds the arena, the coldest partial
/// block is emitted and the final merge re-forms canonical blocks.
struct TextPostingAccumulator {
    blocks: HashMap<Vec<u8>, TextOpenBlock>,
    recency: BTreeSet<(u64, Vec<u8>)>,
    tracking_recency: bool,
    used: usize,
    open_budget: usize,
    output: crate::bulk::ExternalSort,
    postings: u64,
    emissions: u64,
    partials: u64,
    partial_framed_bytes: u64,
    /// Diagnostic ablation: emit one scratch fragment per posting, reproducing
    /// the pre-accumulator materialisation without changing final bytes.
    direct_fragments: bool,
}

impl TextPostingAccumulator {
    fn new(path: &std::path::Path, open_budget: usize, output_budget: usize) -> Result<Self> {
        Ok(Self {
            blocks: HashMap::new(),
            recency: BTreeSet::new(),
            tracking_recency: false,
            used: 0,
            open_budget,
            output: crate::bulk::ExternalSort::new(path, output_budget)?,
            postings: 0,
            emissions: 0,
            partials: 0,
            partial_framed_bytes: 0,
            direct_fragments: std::env::var_os("SEKEJAP_ABLATE_BM25_ACCUMULATOR").is_some(),
        })
    }

    fn entry_bytes(term: &[u8]) -> usize {
        term.len() * 2 + POSTING_BLOCK * std::mem::size_of::<(u64, u64, u64)>() + 192
    }

    fn emit(&mut self, term: Vec<u8>, block: TextOpenBlock, partial: bool) -> Result<()> {
        let mut key = Vec::with_capacity(term.len() + 9);
        key.extend_from_slice(&term);
        key.push(0);
        key.extend_from_slice(&block.rows[0].0.to_be_bytes());
        let value = encode_text_build_block(&block);
        if partial {
            self.partial_framed_bytes = self.partial_framed_bytes
                .checked_add((12 + key.len() + value.len()) as u64)
                .ok_or(Error::TooLarge)?;
        }
        self.output.push(key, value)?;
        self.emissions = self.emissions.checked_add(1).ok_or(Error::TooLarge)?;
        if partial { self.partials = self.partials.checked_add(1).ok_or(Error::TooLarge)?; }
        Ok(())
    }

    fn evict_coldest(&mut self) -> Result<()> {
        if !self.tracking_recency {
            self.recency.extend(self.blocks.iter()
                .map(|(term, block)| (block.last_seen, term.clone())));
            self.tracking_recency = true;
        }
        let Some((last_seen, term)) = self.recency.iter().next().cloned() else {
            return Err(Error::TooLarge);
        };
        self.recency.remove(&(last_seen, term.clone()));
        let block = self.blocks.remove(&term).ok_or(Error::DuplicateKey)?;
        self.used = self.used.saturating_sub(Self::entry_bytes(&term));
        self.emit(term, block, true)
    }

    fn push(&mut self, term: Vec<u8>, doc: u64, tf: u64, dl: u64) -> Result<()> {
        self.postings = self.postings.checked_add(1).ok_or(Error::TooLarge)?;
        if self.direct_fragments {
            return self.emit(term, TextOpenBlock::new(doc, tf, dl), false);
        }
        if let Some(block) = self.blocks.get_mut(&term) {
            if doc <= block.last_seen || tf == 0 { return Err(Error::DuplicateKey); }
            if self.tracking_recency { self.recency.remove(&(block.last_seen, term.clone())); }
            block.rows.push((doc, tf, dl));
            block.last_seen = doc;
            if self.tracking_recency { self.recency.insert((doc, term.clone())); }
            if block.rows.len() == POSTING_BLOCK {
                let block = self.blocks.remove(&term).unwrap();
                if self.tracking_recency { self.recency.remove(&(doc, term.clone())); }
                self.used = self.used.saturating_sub(Self::entry_bytes(&term));
                self.emit(term, block, false)?;
            }
            return Ok(());
        }
        if tf == 0 { return Err(corrupt("text accumulator has zero term frequency")); }
        let bytes = Self::entry_bytes(&term);
        if bytes > self.open_budget { return Err(Error::TooLarge); }
        while self.used.saturating_add(bytes) > self.open_budget { self.evict_coldest()?; }
        self.used += bytes;
        if self.tracking_recency { self.recency.insert((doc, term.clone())); }
        self.blocks.insert(term, TextOpenBlock::new(doc, tf, dl));
        Ok(())
    }

    fn profile(&self) -> (u64, u64, u64, (u64, u64, usize)) {
        (self.postings, self.emissions + self.blocks.len() as u64, self.partials,
         self.output.profile())
    }

    fn flush_run(&mut self) -> Result<()> { self.output.flush_run() }

    fn finish(mut self) -> Result<(crate::bulk::SortedRuns, u64, u64, u64, u64)> {
        let blocks = std::mem::take(&mut self.blocks);
        for (term, block) in blocks { self.emit(term, block, false)?; }
        let partial_scratch = self.partial_framed_bytes;
        Ok((self.output.finish()?, self.postings, self.emissions, self.partials,
            partial_scratch))
    }
}

struct TextBlockIter {
    input: crate::bulk::MergeIter,
    field: u64,
    pending: Option<(Vec<u8>, Vec<(u64, u64, u64)>, usize)>,
    previous_term: Option<Vec<u8>>,
    done: bool,
}

impl TextBlockIter {
    fn new(input: crate::bulk::MergeIter, field: u64) -> Self {
        Self { input, field, pending: None, previous_term: None, done: false }
    }

    fn parse(key: Vec<u8>, value: Vec<u8>, marker: bool)
        -> Result<(Vec<u8>, Vec<(u64, u64, u64)>, usize)>
    {
        if marker || key.len() < 10 || key[key.len() - 9] != 0 {
            return Err(corrupt("text accumulator sort emitted a malformed key"));
        }
        let term = key[..key.len() - 9].to_vec();
        let first = u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap());
        let rows = decode_text_build_block(&value)?;
        if rows.first().map(|row| row.0) != Some(first) {
            return Err(corrupt("text accumulator key disagrees with its block"));
        }
        Ok((term, rows, 0))
    }
}

impl Iterator for TextBlockIter {
    type Item = Result<(Vec<u8>, Vec<u8>, bool)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done { return None; }
        let (term, chunk, mut at) = match self.pending.take() {
            Some(item) => item,
            None => match self.input.next()? {
                Ok((key, value, marker)) => match Self::parse(key, value, marker) {
                    Ok(item) => item,
                    Err(error) => { self.done = true; return Some(Err(error)); }
                },
                Err(error) => { self.done = true; return Some(Err(error)); }
            },
        };
        let first_for_term = self.previous_term.as_deref() != Some(term.as_slice());
        let mut rows = Vec::with_capacity(POSTING_BLOCK);
        while at < chunk.len() && rows.len() < POSTING_BLOCK {
            rows.push(chunk[at]); at += 1;
        }
        if at < chunk.len() { self.pending = Some((term.clone(), chunk, at)); }
        while rows.len() < POSTING_BLOCK && self.pending.is_none() {
            let Some(item) = self.input.next() else { break };
            let (next_term, next_rows, mut next_at) = match item {
                Ok((key, value, marker)) => match Self::parse(key, value, marker) {
                    Ok(item) => item,
                    Err(error) => { self.done = true; return Some(Err(error)); }
                },
                Err(error) => { self.done = true; return Some(Err(error)); }
            };
            if next_term != term {
                self.pending = Some((next_term, next_rows, next_at));
                break;
            }
            while next_at < next_rows.len() && rows.len() < POSTING_BLOCK {
                if next_rows[next_at].0 <= rows.last().unwrap().0 {
                    self.done = true;
                    return Some(Err(corrupt("text accumulator merge regressed")));
                }
                rows.push(next_rows[next_at]); next_at += 1;
            }
            if next_at < next_rows.len() {
                self.pending = Some((next_term, next_rows, next_at));
            }
        }
        let mut value = Vec::new();
        let mut previous = 0u64;
        for &(doc, tf, dl) in &rows {
            write_varint(&mut value, doc - previous);
            write_varint(&mut value, tf);
            write_varint(&mut value, dl);
            previous = doc;
        }
        let key = if first_for_term { keys::text_seg_key(self.field, 1, &term) }
            else { keys::text_seg_block_key(self.field, 1, &term, rows[0].0) };
        self.previous_term = Some(term);
        Some(Ok((key, value, false)))
    }
}

fn term_number(term: &[u8]) -> u64 {
    let lo = crc32c::crc32c(term) as u64;
    let mut salted = Vec::with_capacity(term.len() + 1);
    salted.extend_from_slice(term); salted.push(0x5D);
    let id = lo | ((crc32c::crc32c(&salted) as u64) << 32);
    id.max(1)
}

fn next_term_probe(id: u64) -> u64 {
    id.wrapping_add(0x9E37_79B9_7F4A_7C15).max(1)
}

fn decode_term_row(v: &[u8]) -> Result<(u64, &[u8])> {
    let mut pos = 0usize;
    let n = required_varint(v, &mut pos, "text term count is truncated")?;
    let len = required_varint(v, &mut pos, "text term length is truncated")? as usize;
    let end = pos.checked_add(len).ok_or_else(|| corrupt("text term boundary overflows"))?;
    let term = v.get(pos..end).ok_or_else(|| corrupt("text term crosses its row"))?;
    if term.is_empty() || end != v.len() || std::str::from_utf8(term).is_err() {
        return Err(corrupt("text term dictionary row is malformed"));
    }
    Ok((n, term))
}

fn encode_term_row(term: &[u8], count: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(term.len() + 16);
    write_varint(&mut v, count); write_varint(&mut v, term.len() as u64); v.extend_from_slice(term);
    v
}

impl Graph {
    fn field_meta(&self, field: u64) -> Result<Option<FieldMeta>> {
        self.store_ref().get(&keys::text_field_meta_key(field))?
            .map(|v| FieldMeta::decode(&v)).transpose()
    }

    fn legacy_segment_metas(&self, field: u64) -> Result<Vec<(u32, SegMeta)>> {
        let mut out = Vec::new();
        let mut failure = None;
        let from = keys::text_meta_key(field, 0);
        self.store_ref().scan(&from)?.for_each_ref(|key, val| {
            if key.first() != Some(&keys::TAG_TEXTMETA) || key.len() != 13 { return false; }
            if u64::from_be_bytes(key[1..9].try_into().unwrap()) != field { return false; }
            let seg = u32::from_be_bytes(key[9..13].try_into().unwrap());
            if seg < keys::TEXT_TERM_STATS_SEG {
                match SegMeta::decode(val) {
                    Ok(meta) => out.push((seg, meta)),
                    Err(e) => { failure = Some(e); return false; }
                }
            }
            true
        })?;
        if let Some(e) = failure { return Err(e); }
        Ok(out)
    }

    fn ensure_field_meta(&mut self, field: u64) -> Result<FieldMeta> {
        if let Some(mut meta) = self.field_meta(field)? {
            // Resume a publication that crashed after the one-row manifest
            // flip but before retirement. Reads are already correct through
            // redirects; the next writer finishes the idempotent cleanup
            // before admitting fresh head rows.
            if let Some(new_seg) = meta.head_redirect {
                self.rewrite_norm_owners(field, &[0], new_seg)?;
                self.store().delete_prefix(&keys::text_seg_key(field, 0, b""))?;
                self.store().delete(&keys::text_meta_key(field, 0))?;
                self.commit()?; self.checkpoint()?;
                meta.head_redirect = None;
                self.put_field_meta(field, &meta)?;
                self.commit()?; self.checkpoint()?;
            }
            if !meta.redirects.is_empty() {
                let new_seg = meta.redirects[0].1;
                if meta.redirects.iter().any(|&(_, new)| new != new_seg) {
                    return Err(corrupt("text manifest contains redirects to multiple pending merges"));
                }
                let old: Vec<u32> = meta.redirects.iter().map(|&(old, _)| old).collect();
                self.rewrite_norm_owners(field, &old, new_seg)?;
                for &source in &old {
                    self.store().delete_prefix(&keys::text_seg_key(field, source, b""))?;
                    self.store().delete_prefix(&keys::text_seg_doc_prefix(field, source))?;
                    self.store().delete(&keys::text_meta_key(field, source))?;
                }
                self.commit()?; self.checkpoint()?;
                meta.redirects.clear();
                self.put_field_meta(field, &meta)?;
                self.commit()?; self.checkpoint()?;
            }
            return Ok(meta);
        }
        let legacy = self.legacy_segment_metas(field)?;
        let mut meta = FieldMeta { next_seg: 1, ..FieldMeta::default() };
        if !legacy.is_empty() { meta.term_stats_ready = false; }
        for (seg, m) in legacy {
            let live = m.doc_count.checked_sub(m.dead.len() as u64)
                .ok_or_else(|| corrupt("text segment has more dead documents than documents"))?;
            meta.live_docs = meta.live_docs.checked_add(live).ok_or(Error::TooLarge)?;
            meta.total_tokens = meta.total_tokens
                .checked_add(m.total_tokens.checked_sub(m.dead_tokens)
                    .ok_or_else(|| corrupt("text segment dead tokens exceed its token total"))?)
                .ok_or(Error::TooLarge)?;
            if seg == 0 { continue; }
            meta.next_seg = meta.next_seg.max(seg.checked_add(1).ok_or(Error::TooLarge)?);
            meta.active.push((seg, m.level));
        }
        if meta.next_seg >= keys::TEXT_TERM_STATS_SEG { return Err(Error::TooLarge); }
        self.store().put(&keys::text_field_meta_key(field), &meta.encode())?;
        Ok(meta)
    }

    fn put_field_meta(&mut self, field: u64, meta: &FieldMeta) -> Result<()> {
        self.store().put(&keys::text_field_meta_key(field), &meta.encode())
    }

    fn term_info(&self, field: u64, term: &str) -> Result<Option<(u64, u64)>> {
        let mut id = term_number(term.as_bytes());
        for _ in 0..1024 {
            let Some(v) = self.store_ref().get(&keys::text_term_id_key(field, id))? else {
                return Ok(None);
            };
            let (count, stored) = decode_term_row(&v)?;
            if stored == term.as_bytes() { return Ok(Some((count, id))); }
            id = next_term_probe(id);
        }
        Err(corrupt("text term collision chain exceeds its bound"))
    }

    fn term_by_id(&self, field: u64, id: u64) -> Result<String> {
        let v = self.store_ref().get(&keys::text_term_id_key(field, id))?
            .ok_or_else(|| corrupt("text term number has no dictionary entry"))?;
        let (_, term) = decode_term_row(&v)?;
        Ok(std::str::from_utf8(term).unwrap().to_owned())
    }

    /// Adjust one word's live-document count and return its compact,
    /// field-local number.  A new word is interned exactly once; document
    /// norms then store only the number and frequency.
    fn adjust_term_df(&mut self, field: u64, term: &str, delta: i8) -> Result<u64> {
        let mut id = term_number(term.as_bytes());
        for _ in 0..1024 {
            let key = keys::text_term_id_key(field, id);
            match self.store_ref().get(&key)? {
                Some(v) => {
                    let (old, stored) = decode_term_row(&v)?;
                    if stored != term.as_bytes() { id = next_term_probe(id); continue; }
                    let new = match delta {
                        1 => old.checked_add(1).ok_or(Error::TooLarge)?,
                        -1 => old.checked_sub(1)
                            .ok_or_else(|| corrupt("text term document count underflows"))?,
                        _ => return Err(Error::TooLarge),
                    };
                    self.store().put(&key, &encode_term_row(term.as_bytes(), new))?;
                    let lex = keys::text_term_lex_key(field, term.as_bytes());
                    if new == 0 { self.store().delete(&lex)?; }
                    else if old == 0 { self.store().put(&lex, &[])?; }
                    return Ok(id);
                }
                None if delta == 1 => {
                    self.store().put(&key, &encode_term_row(term.as_bytes(), 1))?;
                    self.store().put(&keys::text_term_lex_key(field, term.as_bytes()), &[])?;
                    return Ok(id);
                }
                None => return Err(corrupt("text term document count is missing")),
            }
        }
        Err(corrupt("text term collision chain exceeds its bound"))
    }

    /// Initial backfills write a new word's row once with count one, but do
    /// not rewrite it for every later document.  `finish_text_build` obtains
    /// the exact repeated counts by an external sort over compact norm ids.
    fn intern_term_for_build(&mut self, field: u64, term: &str) -> Result<u64> {
        let mut id = term_number(term.as_bytes());
        for _ in 0..1024 {
            let key = keys::text_term_id_key(field, id);
            match self.store_ref().get(&key)? {
                Some(v) => {
                    let (_, stored) = decode_term_row(&v)?;
                    if stored == term.as_bytes() { return Ok(id); }
                    id = next_term_probe(id);
                }
                None => {
                    self.store().put(&key, &encode_term_row(term.as_bytes(), 1))?;
                    self.store().put(&keys::text_term_lex_key(field, term.as_bytes()), &[])?;
                    return Ok(id);
                }
            }
        }
        Err(corrupt("text term collision chain exceeds its bound"))
    }

    fn segment_meta(&self, field: u64, seg: u32) -> Result<SegMeta> {
        let v = self.store_ref().get(&keys::text_meta_key(field, seg))?
            .ok_or_else(|| corrupt("active text segment has no metadata"))?;
        SegMeta::decode(&v)
    }

    /// Remove every posting, norm and statistics row owned by one text
    /// field. A rebuild starts from an empty corpus; otherwise its document
    /// counters and folded terms describe both the old and new contents.
    pub fn clear_text(&mut self, field: u64) -> Result<()> {
        self.store().delete_prefix(&keys::text_prefix(field))?;
        self.store().delete_prefix(&keys::text_norm_prefix(field))?;
        self.store().delete_prefix(&keys::text_meta_prefix(field))?;
        Ok(())
    }

    /// Index `text` for `(field, docid)`, blind writes only: one head row
    /// per distinct term, the norm row, and the head meta counters. Cost
    /// O(distinct terms) -- never touches other documents (Law 2).
    /// Re-indexing the same (field, doc) must be preceded by delete_text.
    pub fn index_text(&mut self, field: u64, docid: u64, text: &str) -> Result<()> {
        self.index_text_inner(field, docid, text, true, None, None)
    }

    /// Backfill variant: corpus/document metadata and postings are still
    /// complete after every row, but repeated term-count rewrites are deferred
    /// to one bounded external aggregation in `finish_text_build`.
    pub fn index_text_build(&mut self, field: u64, docid: u64, text: &str) -> Result<()> {
        self.index_text_inner(field, docid, text, false, None, None)
    }

    pub fn index_text_build_cached(&mut self, field: u64, docid: u64, text: &str,
                                   cache: &mut TextBuildCache) -> Result<()> {
        self.index_text_inner(field, docid, text, false, Some(cache), None)
    }

    pub fn index_text_build_accum(&mut self, field: u64, docid: u64, text: &str,
                                  accumulator: &mut TextBuildAccumulator) -> Result<()> {
        self.index_text_inner(field, docid, text, false, None, Some(accumulator))
    }

    pub fn begin_text_build(&mut self, field: u64) -> Result<()> {
        let mut meta = self.ensure_field_meta(field)?;
        meta.live_docs = 0; meta.total_tokens = 0; meta.term_stats_ready = false;
        self.put_field_meta(field, &meta)?;
        self.put_head_build_meta(field, 0, 0)
    }

    /// Build a fresh text field directly as one immutable segment. The input
    /// is a replayable external sort of `(docid_be, utf8_text)` records. No
    /// row-per-posting head is ever installed: term/doc records are grouped
    /// into 128-document values before their range reaches `graft_sorted_range`.
    /// Live writes after publication continue to use segment zero unchanged.
    pub fn prepare_text_packed(
        field: u64,
        docs: &mut crate::bulk::SortedRuns,
        scratch: &std::path::Path,
    ) -> Result<PackedTextCandidate> {
        Self::prepare_text_packed_with_workers(
            field, docs, scratch, text_prepare_worker_limit(),
        )
    }

    pub fn prepare_text_packed_with_workers(
        field: u64,
        docs: &mut crate::bulk::SortedRuns,
        scratch: &std::path::Path,
        worker_limit: usize,
    ) -> Result<PackedTextCandidate> {
        let trace = std::env::var_os("SEKEJAP_LOAD_BREAKDOWN").is_some();
        let total_started = trace.then(std::time::Instant::now);
        let scan_started = trace.then(std::time::Instant::now);
        let mut postings = TextPostingAccumulator::new(
            &scratch.join(format!("text-{field}-posting-blocks")),
            TEXT_BUILD_OPEN_BUDGET, TEXT_BUILD_OUTPUT_BUDGET)?;
        let mut norms = crate::bulk::ExternalSort::new(
            &scratch.join(format!("text-{field}-norms")), 32 << 20)?;
        let mut memberships = crate::bulk::ExternalSort::new(
            &scratch.join(format!("text-{field}-members")), 32 << 20)?;
        let mut doc_count = 0u64;
        let mut total_tokens = 0u64;
        let mut norm_min = None;
        let mut norm_max = None;
        let mut previous_doc = None;

        let mut stage = |prepared: PreparedPackedTextDoc| -> Result<()> {
            for (term, count) in prepared.terms {
                postings.push(term.into_bytes(), prepared.doc, count, prepared.dl)?;
            }
            let norm_key = keys::text_norm_key(field, prepared.doc);
            if norm_min.is_none() { norm_min = Some(norm_key.clone()); }
            norm_max = Some(norm_key.clone());
            norms.push(norm_key, Norm {
                total: prepared.dl, owner: 1, terms: prepared.norm_terms,
            }.encode())?;
            let mut length = Vec::new(); write_varint(&mut length, prepared.dl);
            memberships.push(keys::text_seg_doc_key(field, 1, prepared.doc), length)?;
            doc_count = doc_count.checked_add(1).ok_or(Error::TooLarge)?;
            total_tokens = total_tokens.checked_add(prepared.dl).ok_or(Error::TooLarge)?;
            if doc_count % 8192 == 0 {
                postings.flush_run()?;
                norms.flush_run()?;
                memberships.flush_run()?;
            }
            Ok(())
        };

        let worker_limit = worker_limit.clamp(1, 8);
        let parallel_tokenize = std::env::var_os("SEKEJAP_INDEX_BUILD_SERIAL").is_none()
            && std::env::var_os("SEKEJAP_NO_PARALLEL_TEXT_PREP").is_none()
            && worker_limit > 1;
        if parallel_tokenize {
            const DOC_CHUNK: usize = 512;
            let worker_count = worker_limit;
            std::thread::scope(|scope| -> Result<()> {
                let mut inputs = Vec::with_capacity(worker_count);
                let mut outputs = Vec::with_capacity(worker_count);
                let mut handles = Vec::with_capacity(worker_count);
                for _ in 0..worker_count {
                    let (input_tx, input_rx) = std::sync::mpsc::sync_channel::<
                        Option<Vec<(u64, String)>>
                    >(1);
                    let (output_tx, output_rx) = std::sync::mpsc::channel();
                    inputs.push(input_tx);
                    outputs.push(output_rx);
                    handles.push(scope.spawn(move || {
                        while let Ok(Some(chunk)) = input_rx.recv() {
                            let result = chunk.into_iter().map(|(doc, text)|
                                prepare_packed_text_doc(doc, &text)
                            ).collect::<Result<Vec<_>>>();
                            if output_tx.send(result).is_err() { break; }
                        }
                    }));
                }

                let mut input = docs.iter()?;
                loop {
                    let mut active = 0usize;
                    for worker in 0..worker_count {
                        let mut chunk = Vec::with_capacity(DOC_CHUNK);
                        while chunk.len() < DOC_CHUNK {
                            let Some(item) = input.next() else { break };
                            let (doc_key, text, marker) = item?;
                            if marker || doc_key.len() != 8 {
                                return Err(corrupt("packed text source has a malformed document key"));
                            }
                            let doc = u64::from_be_bytes(doc_key.try_into().unwrap());
                            if previous_doc.is_some_and(|previous| previous >= doc) {
                                return Err(corrupt("packed text documents are not strictly ordered"));
                            }
                            previous_doc = Some(doc);
                            let text = String::from_utf8(text)
                                .map_err(|_| corrupt("packed text source is not UTF-8"))?;
                            chunk.push((doc, text));
                        }
                        if chunk.is_empty() { break; }
                        inputs[worker].send(Some(chunk))
                            .map_err(|_| corrupt("packed text worker stopped before input"))?;
                        active += 1;
                    }
                    if active == 0 { break; }
                    for output in outputs.iter().take(active) {
                        let prepared = output.recv()
                            .map_err(|_| corrupt("packed text worker stopped before output"))??;
                        for document in prepared { stage(document)?; }
                    }
                }
                for input in &inputs { let _ = input.send(None); }
                for handle in handles {
                    handle.join().map_err(|_| corrupt("packed text worker panicked"))?;
                }
                Ok(())
            })?;
        } else {
            for item in docs.iter()? {
                let (doc_key, text, marker) = item?;
                if marker || doc_key.len() != 8 {
                    return Err(corrupt("packed text source has a malformed document key"));
                }
                let doc = u64::from_be_bytes(doc_key.try_into().unwrap());
                if previous_doc.is_some_and(|previous| previous >= doc) {
                    return Err(corrupt("packed text documents are not strictly ordered"));
                }
                previous_doc = Some(doc);
                let text = std::str::from_utf8(&text)
                    .map_err(|_| corrupt("packed text source is not UTF-8"))?;
                stage(prepare_packed_text_doc(doc, text)?)?;
            }
        }

        let scan_stage =
            scan_started.map_or(std::time::Duration::ZERO, |started| started.elapsed());
        let posting_profile = postings.profile();
        let norm_profile = norms.profile();
        let membership_profile = memberships.profile();
        let finish_sources_started = trace.then(std::time::Instant::now);
        let (mut posting_runs, posting_rows, emissions, partials, partial_block_scratch) =
            postings.finish()?;
        let mut norm_runs = norms.finish()?;
        let mut membership_runs = memberships.finish()?;
        let source_finish =
            finish_sources_started.map_or(std::time::Duration::ZERO, |started| started.elapsed());
        let posting_input_runs = posting_runs.run_count();
        let norm_input_runs = norm_runs.run_count();
        let membership_input_runs = membership_runs.run_count();

        let mut terms = crate::bulk::ExternalSort::new(
            &scratch.join(format!("text-{field}-terms")), 16 << 20)?;
        let mut expected = SegMeta {
            doc_count, total_tokens, dead_tokens: 0, dead: Vec::new(), level: 0,
            term_rows: 0, posting_count: 0, logical_xor: 0, logical_sum: 0,
        };
        let mut block_min: Option<Vec<u8>> = None;
        let mut block_max: Option<Vec<u8>> = None;
        let mut previous_key: Option<Vec<u8>> = None;
        let mut current_term: Option<Vec<u8>> = None;
        let mut term_docs = 0u64;
        let posting_merge_started = trace.then(std::time::Instant::now);
        for item in TextBlockIter::new(posting_runs.iter()?, field) {
            let (key, value, marker) = item?;
            if marker || previous_key.as_ref().is_some_and(|old| old >= &key) {
                return Err(corrupt("packed text block stream is not strictly ordered"));
            }
            let prefix_len = 1 + 8 + 4;
            if key.len() <= prefix_len { return Err(corrupt("packed text block key has no term")); }
            let body = &key[prefix_len..];
            let term_end = body.iter().position(|byte| *byte == 0).unwrap_or(body.len());
            let term = &body[..term_end];
            if term.is_empty() { return Err(corrupt("packed text block has an empty term")); }
            if current_term.as_deref().is_some_and(|old| old != term) {
                let old = current_term.take().unwrap();
                let mut term_key = term_number(&old).to_be_bytes().to_vec();
                term_key.extend_from_slice(&old);
                terms.push(term_key, term_docs.to_be_bytes().to_vec())?;
                term_docs = 0;
            }
            if current_term.is_none() { current_term = Some(term.to_vec()); }
            for (doc, tf, dl) in decode_postings(&value)? {
                expected.posting_count = expected.posting_count.checked_add(1).ok_or(Error::TooLarge)?;
                let hash = posting_hash(term, doc, tf, dl);
                expected.logical_xor ^= hash;
                expected.logical_sum = expected.logical_sum.wrapping_add(hash);
                term_docs = term_docs.checked_add(1).ok_or(Error::TooLarge)?;
            }
            expected.term_rows = expected.term_rows.checked_add(1).ok_or(Error::TooLarge)?;
            if block_min.is_none() { block_min = Some(key.clone()); }
            block_max = Some(key.clone());
            previous_key = Some(key);
        }
        if let Some(term) = current_term.take() {
            let mut term_key = term_number(&term).to_be_bytes().to_vec();
            term_key.extend_from_slice(&term);
            terms.push(term_key, term_docs.to_be_bytes().to_vec())?;
        }
        if expected.posting_count != posting_rows {
            return Err(corrupt("text accumulator lost or duplicated a posting"));
        }
        let block_rows = expected.term_rows;
        let posting_merge =
            posting_merge_started.map_or(std::time::Duration::ZERO, |started| started.elapsed());
        let term_profile = terms.profile();
        let block_term_finish_started = trace.then(std::time::Instant::now);
        let mut term_runs = terms.finish()?;
        let block_term_finish = block_term_finish_started
            .map_or(std::time::Duration::ZERO, |started| started.elapsed());
        let term_input_runs = term_runs.run_count();
        let mut dictionary = crate::bulk::ExternalSort::new(
            &scratch.join(format!("text-{field}-dictionary")), 16 << 20)?;
        let mut dict_rows = 0u64;
        let mut previous_actual = None;
        let mut current_raw = None;
        let mut probe_offset = 0u32;
        let dictionary_started = trace.then(std::time::Instant::now);
        for item in term_runs.iter()? {
            let (key, count, marker) = item?;
            if marker || key.len() < 9 || count.len() != 8 {
                return Err(corrupt("packed text term sort emitted a malformed row"));
            }
            let raw = u64::from_be_bytes(key[..8].try_into().unwrap());
            let term = &key[8..];
            if current_raw != Some(raw) { current_raw = Some(raw); probe_offset = 0; }
            let mut actual = raw;
            for _ in 0..probe_offset { actual = next_term_probe(actual); }
            probe_offset = probe_offset.checked_add(1).ok_or(Error::TooLarge)?;
            if probe_offset > 1024 { return Err(corrupt("packed text term collision chain exceeds its bound")); }
            if previous_actual == Some(actual) {
                return Err(corrupt("packed text term assignment produced a duplicate id"));
            }
            previous_actual = Some(actual);
            let df = u64::from_be_bytes(count.try_into().unwrap());
            dictionary.push(keys::text_term_id_key(field, actual), encode_term_row(term, df))?;
            dictionary.push(keys::text_term_lex_key(field, term), Vec::new())?;
            dict_rows = dict_rows.checked_add(2).ok_or(Error::TooLarge)?;
            if actual != raw {
                // The double-CRC term number makes this astronomically rare.
                // Refuse rather than publish norms whose ids need a corpus
                // rewrite; a future collision fixture can justify the extra
                // external join without charging every ordinary build today.
                return Err(corrupt("packed text build encountered a term-number collision"));
            }
        }
        let dictionary_stage =
            dictionary_started.map_or(std::time::Duration::ZERO, |started| started.elapsed());
        let dictionary_profile = dictionary.profile();
        let dictionary_finish_started = trace.then(std::time::Instant::now);
        let mut dictionary_runs = dictionary.finish()?;
        let dictionary_finish = dictionary_finish_started
            .map_or(std::time::Duration::ZERO, |started| started.elapsed());
        let dictionary_input_runs = dictionary_runs.run_count();

        // Validate every replayable stream, including duplicate keys, before
        // the first graft can publish any of the three physical ranges.
        fn validate_sorted(
            iter: impl Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>>,
        ) -> Result<(u64, Option<Vec<u8>>, Option<Vec<u8>>)> {
            let mut rows = 0u64; let mut min = None; let mut max = None;
            for item in iter {
                let (key, _, _) = item?;
                if max.as_ref().is_some_and(|old: &Vec<u8>| old >= &key) {
                    return Err(Error::DuplicateKey);
                }
                if min.is_none() { min = Some(key.clone()); }
                max = Some(key); rows = rows.checked_add(1).ok_or(Error::TooLarge)?;
            }
            Ok((rows, min, max))
        }
        let validate_started = trace.then(std::time::Instant::now);
        let (norm_rows, checked_norm_min, checked_norm_max) = validate_sorted(norm_runs.iter()?)?;
        if norm_rows != doc_count || checked_norm_min != norm_min || checked_norm_max != norm_max {
            return Err(corrupt("packed text norm manifest disagrees with its stream"));
        }
        let (member_rows, member_min, member_max) = validate_sorted(membership_runs.iter()?)?;
        let (checked_dict_rows, dict_min, dict_max) = validate_sorted(dictionary_runs.iter()?)?;
        if member_rows != doc_count || checked_dict_rows != dict_rows {
            return Err(corrupt("packed text metadata manifest disagrees with its streams"));
        }
        let validate =
            validate_started.map_or(std::time::Duration::ZERO, |started| started.elapsed());

        Ok(PackedTextCandidate {
            field,
            posting_runs,
            norm_runs,
            membership_runs,
            dictionary_runs,
            expected,
            doc_count,
            total_tokens,
            posting_rows,
            emissions,
            partials,
            block_rows,
            block_min,
            block_max,
            norm_rows,
            norm_min: checked_norm_min,
            norm_max: checked_norm_max,
            member_rows,
            member_min,
            member_max,
            dict_rows,
            dict_min,
            dict_max,
            scan_stage,
            source_finish,
            posting_merge,
            block_term_finish,
            dictionary_stage,
            dictionary_finish,
            validate,
            prepare_total: total_started
                .map_or(std::time::Duration::ZERO, |started| started.elapsed()),
            posting_scratch: posting_profile.3.1,
            norm_scratch: norm_profile.1,
            membership_scratch: membership_profile.1,
            term_scratch: term_profile.1,
            dictionary_scratch: dictionary_profile.1,
            partial_block_scratch,
            posting_input_runs,
            norm_input_runs,
            membership_input_runs,
            term_input_runs,
            dictionary_input_runs,
        })
    }

    /// Graft one privately built candidate in the caller's deterministic
    /// single-writer order, then publish and independently verify it.
    pub fn publish_text_packed(
        &mut self,
        mut candidate: PackedTextCandidate,
        scratch: &std::path::Path,
    ) -> Result<()> {
        let trace = std::env::var_os("SEKEJAP_LOAD_BREAKDOWN").is_some();
        let graft_started = trace.then(std::time::Instant::now);
        if candidate.block_rows != 0 {
            if trace { eprintln!("consumer BM25 postings graft phases:"); }
            self.store().graft_sorted_range(
                TextBlockIter::new(candidate.posting_runs.iter()?, candidate.field),
                candidate.block_rows,
                candidate.block_min.take().unwrap(),
                candidate.block_max.take().unwrap(),
                scratch,
            )?;
        }
        if candidate.norm_rows != 0 {
            if trace { eprintln!("consumer BM25 norms graft phases:"); }
            self.store().graft_sorted_range(
                candidate.norm_runs.iter()?,
                candidate.norm_rows,
                candidate.norm_min.take().unwrap(),
                candidate.norm_max.take().unwrap(),
                scratch,
            )?;
        }
        if candidate.member_rows != 0 {
            if trace { eprintln!("consumer BM25 memberships graft phases:"); }
            self.store().graft_sorted_range(
                candidate.membership_runs.iter()?,
                candidate.member_rows,
                candidate.member_min.take().unwrap(),
                candidate.member_max.take().unwrap(),
                scratch,
            )?;
        }
        if candidate.dict_rows != 0 {
            if trace { eprintln!("consumer BM25 dictionary graft phases:"); }
            self.store().graft_sorted_range(
                candidate.dictionary_runs.iter()?,
                candidate.dict_rows,
                candidate.dict_min.take().unwrap(),
                candidate.dict_max.take().unwrap(),
                scratch,
            )?;
        }
        let graft = graft_started.map_or(std::time::Duration::ZERO, |started| started.elapsed());

        let publish_started = trace.then(std::time::Instant::now);
        let field_meta = FieldMeta {
            live_docs: candidate.doc_count,
            total_tokens: candidate.total_tokens,
            next_seg: 2,
            generation: 1,
            active: vec![(1, 0)],
            head_redirect: None,
            redirects: Vec::new(),
            term_stats_ready: true,
        };
        self.store().put(&keys::text_meta_key(candidate.field, 1), &candidate.expected.encode())?;
        self.store().put(&keys::text_field_meta_key(candidate.field), &field_meta.encode())?;
        self.commit()?;
        self.checkpoint()?;

        let cfg = crate::store::Config {
            budget_bytes: 16 * crate::page::PAGE_SIZE,
            io: self.store_ref().io_mode(),
            sync: crate::store::SyncMode::Off,
        };
        let snapshot = crate::store::Store::open_snapshot(self.store_ref().dir(), cfg)?;
        let result = Graph::new(snapshot)?
            .verify_text_segment(candidate.field, 1, &candidate.expected);
        let publish_verify = publish_started
            .map_or(std::time::Duration::ZERO, |started| started.elapsed());
        if trace {
            let mb = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
            let doc_count = candidate.doc_count;
            eprintln!(
                "\nBM25 packed build detail ({} documents, {} term/doc postings)",
                doc_count, candidate.posting_rows,
            );
            eprintln!("{:<34} {:>11} {:>12}", "item", "seconds", "ns/doc");
            eprintln!("{}", "-".repeat(61));
            let print = |name: &str, elapsed: std::time::Duration| eprintln!(
                "{name:<34} {:>11.6} {:>12.1}", elapsed.as_secs_f64(),
                elapsed.as_secs_f64() * 1e9 / doc_count.max(1) as f64);
            print("tokenize + stage raw rows", candidate.scan_stage);
            print("finish raw sorters", candidate.source_finish);
            print("merge postings + form blocks", candidate.posting_merge);
            print("finish block + term sorters", candidate.block_term_finish);
            print("build dictionary rows", candidate.dictionary_stage);
            print("finish dictionary sorter", candidate.dictionary_finish);
            print("validate all replay streams", candidate.validate);
            print("pack + verify + graft 4 ranges", graft);
            print("publish + logical reopen verify", publish_verify);
            print("BM25 packed total", candidate.prepare_total + graft + publish_verify);
            eprintln!(
                "postings: {} rows, {:.2}/doc -> {} packed rows ({:.2} postings/block)",
                candidate.posting_rows,
                candidate.posting_rows as f64 / doc_count.max(1) as f64,
                candidate.block_rows,
                candidate.posting_rows as f64 / candidate.block_rows.max(1) as f64,
            );
            eprintln!(
                "accumulator emissions: {} ({} partial spills), open arena {} MiB, finished-block sorter {} MiB",
                candidate.emissions, candidate.partials,
                TEXT_BUILD_OPEN_BUDGET >> 20, TEXT_BUILD_OUTPUT_BUDGET >> 20,
            );
            eprintln!(
                "accumulator churn: evictions={} partial-spills={} merge-fragments={} (emitted fragments minus canonical blocks)",
                candidate.partials,
                candidate.partials,
                candidate.emissions.saturating_sub(candidate.block_rows),
            );
            eprintln!(
                "dictionary: {} distinct terms, {} rows; norms: {}; memberships: {}",
                candidate.dict_rows / 2, candidate.dict_rows,
                candidate.norm_rows, candidate.member_rows,
            );
            eprintln!(
                "scratch first-pass: posting blocks {:.2} MiB/{} runs; norms {:.2} MiB/{}; members {:.2} MiB/{}; terms {:.2} MiB/{}; dictionary {:.2} MiB/{}",
                mb(candidate.posting_scratch), candidate.posting_input_runs,
                mb(candidate.norm_scratch), candidate.norm_input_runs,
                mb(candidate.membership_scratch), candidate.membership_input_runs,
                mb(candidate.term_scratch), candidate.term_input_runs,
                mb(candidate.dictionary_scratch), candidate.dictionary_input_runs,
            );
            eprintln!(
                "partial-block scratch subset: {:.2} MiB ({} evicted fragments; included in posting blocks)",
                mb(candidate.partial_block_scratch), candidate.partials,
            );
        }
        result
    }

    pub fn build_text_packed(
        &mut self,
        field: u64,
        docs: &mut crate::bulk::SortedRuns,
        scratch: &std::path::Path,
    ) -> Result<()> {
        let candidate = Self::prepare_text_packed(field, docs, scratch)?;
        self.publish_text_packed(candidate, scratch)
    }

    /// Copy a quiescent packed initial segment to another physical field id.
    /// This is how a BM25 build and a later SEARCH build over the same source
    /// text share tokenisation without sharing physical indexes. A field with
    /// any live head or merge history is refused so CREATE INDEX falls back to
    /// its ordinary source scan rather than copying a mutable shape.
    pub fn clone_packed_text(
        &mut self,
        source: u64,
        target: u64,
        scratch: &std::path::Path,
    ) -> Result<bool> {
        let trace = std::env::var_os("SEKEJAP_LOAD_BREAKDOWN").is_some();
        let total_started = trace.then(std::time::Instant::now);
        let mut scan_elapsed = std::time::Duration::ZERO;
        let mut graft_elapsed = std::time::Duration::ZERO;
        let Some(meta) = self.field_meta(source)? else {
            return Ok(false);
        };
        if meta.active.as_slice() != [(1, 0)]
            || meta.head_redirect.is_some()
            || !meta.redirects.is_empty()
            || !meta.term_stats_ready
        {
            return Ok(false);
        }
        let head_prefix = keys::text_seg_key(source, 0, b"");
        if let Some(row) = self.store_ref().scan(&head_prefix)?.next() {
            let (key, _) = row?;
            if key.starts_with(&head_prefix) { return Ok(false); }
        }

        if std::env::var_os("SEKEJAP_ABLATE_SEARCH_CLONE").is_some() {
            let ablate_started = trace.then(std::time::Instant::now);
            let stage = |prefix: Vec<u8>,
                         name: &str|
             -> Result<(
                crate::bulk::ExternalSort,
                u64,
                Option<Vec<u8>>,
                Option<Vec<u8>>,
            )> {
                let mut sort = crate::bulk::ExternalSort::new(
                    &scratch.join(format!("clone-{target}-{name}")),
                    32 << 20,
                )?;
                let mut rows = 0u64;
                let mut min = None;
                let mut max = None;
                let mut failure = None;
                self.store_ref().scan(&prefix)?.for_each_ref(|key, value| {
                    if !key.starts_with(&prefix) {
                        return false;
                    }
                    let mut rewritten = key.to_vec();
                    rewritten[1..9].copy_from_slice(&target.to_be_bytes());
                    if min.is_none() {
                        min = Some(rewritten.clone());
                    }
                    max = Some(rewritten.clone());
                    rows += 1;
                    match sort.push(rewritten, value.to_vec()) {
                        Ok(()) => true,
                        Err(error) => {
                            failure = Some(error);
                            false
                        }
                    }
                })?;
                if let Some(error) = failure {
                    return Err(error);
                }
                Ok((sort, rows, min, max))
            };
            let (text, text_rows, text_min, text_max) =
                stage(keys::text_prefix(source), "postings")?;
            let (norms, norm_rows, norm_min, norm_max) =
                stage(keys::text_norm_prefix(source), "norms")?;
            let (metadata, meta_rows, meta_min, meta_max) =
                stage(keys::text_meta_prefix(source), "metadata")?;
            let mut text = text.finish()?;
            let mut norms = norms.finish()?;
            let mut metadata = metadata.finish()?;
            if text_rows != 0 {
                self.store().graft_sorted_range(
                    text.iter()?,
                    text_rows,
                    text_min.unwrap(),
                    text_max.unwrap(),
                    scratch,
                )?;
            }
            if norm_rows != 0 {
                self.store().graft_sorted_range(
                    norms.iter()?,
                    norm_rows,
                    norm_min.unwrap(),
                    norm_max.unwrap(),
                    scratch,
                )?;
            }
            if meta_rows != 0 {
                self.store().graft_sorted_range(
                    metadata.iter()?,
                    meta_rows,
                    meta_min.unwrap(),
                    meta_max.unwrap(),
                    scratch,
                )?;
            }
            let expected = self.segment_meta(target, 1)?;
            let cfg = crate::store::Config {
                budget_bytes: 16 * crate::page::PAGE_SIZE,
                io: self.store_ref().io_mode(),
                sync: crate::store::SyncMode::Off,
            };
            let snapshot = crate::store::Store::open_snapshot(self.store_ref().dir(), cfg)?;
            Graph::new(snapshot)?.verify_text_segment(target, 1, &expected)?;
            if trace {
                eprintln!(
                    "consumer SEARCH re-sort ablation total: {:.6}s",
                    ablate_started.unwrap().elapsed().as_secs_f64()
                );
            }
            return Ok(true);
        }

        let cfg = crate::store::Config {
            budget_bytes: 16 * crate::page::PAGE_SIZE,
            io: self.store_ref().io_mode(),
            sync: crate::store::SyncMode::Off,
        };
        // The source was just logically verified and published. Read it from
        // an independent snapshot so its range iterator can remain live while
        // this writer packs the target. Replacing the fixed-width field id in
        // every key preserves key order exactly, so sorting the clone again
        // only wrote and reread the whole segment as scratch.
        let source_snapshot = crate::store::Store::open_snapshot(self.store_ref().dir(), cfg)?;
        let source_graph = Graph::new(source_snapshot)?;
        let mut graft_prefix = |name: &str, prefix: Vec<u8>| -> Result<()> {
            let scan_started = trace.then(std::time::Instant::now);
            let mut rows = 0u64;
            let mut min = None;
            let mut max = None;
            let mut count_error = None;
            source_graph
                .store_ref()
                .scan(&prefix)?
                .for_each_ref(|key, _| {
                    if !key.starts_with(&prefix) {
                        return false;
                    }
                    let mut rewritten = key.to_vec();
                    rewritten[1..9].copy_from_slice(&target.to_be_bytes());
                    if min.is_none() {
                        min = Some(rewritten.clone());
                    }
                    max = Some(rewritten);
                    match rows.checked_add(1) {
                        Some(next) => rows = next,
                        None => {
                            count_error = Some(Error::TooLarge);
                            return false;
                        }
                    }
                    true
                })?;
            if let Some(started) = scan_started {
                scan_elapsed += started.elapsed();
            }
            if let Some(error) = count_error {
                return Err(error);
            }
            if rows == 0 {
                return Ok(());
            }

            let mut source_rows = source_graph.store_ref().scan(&prefix)?;
            let iter_prefix = prefix.clone();
            let iter = std::iter::from_fn(move || match source_rows.next() {
                Some(Ok((mut key, value))) if key.starts_with(&iter_prefix) => {
                    key[1..9].copy_from_slice(&target.to_be_bytes());
                    Some(Ok((key, value, false)))
                }
                Some(Ok(_)) | None => None,
                Some(Err(error)) => Some(Err(error)),
            });
            if trace {
                eprintln!("consumer SEARCH clone {name} graft phases:");
            }
            let graft_started = trace.then(std::time::Instant::now);
            let result =
                self.store()
                    .graft_sorted_range(iter, rows, min.unwrap(), max.unwrap(), scratch);
            if let Some(started) = graft_started {
                graft_elapsed += started.elapsed();
            }
            result
        };
        graft_prefix("postings", keys::text_prefix(source))?;
        graft_prefix("norms", keys::text_norm_prefix(source))?;
        graft_prefix("metadata", keys::text_meta_prefix(source))?;
        drop(graft_prefix);
        drop(source_graph);

        let logical_verify_started = trace.then(std::time::Instant::now);
        let expected = self.segment_meta(target, 1)?;
        let snapshot = crate::store::Store::open_snapshot(self.store_ref().dir(), cfg)?;
        Graph::new(snapshot)?.verify_text_segment(target, 1, &expected)?;
        if trace {
            eprintln!(
                "consumer SEARCH order-preserving clone: accumulate/rewrite-scan={:.6}s sort=0.000000s pack+verify+graft={:.6}s logical-verify={:.6}s total={:.6}s",
                scan_elapsed.as_secs_f64(),
                graft_elapsed.as_secs_f64(),
                logical_verify_started.unwrap().elapsed().as_secs_f64(),
                total_started.unwrap().elapsed().as_secs_f64()
            );
        }
        Ok(true)
    }

    fn index_text_inner(&mut self, field: u64, docid: u64, text: &str,
                        maintain_term_counts: bool, mut build_cache: Option<&mut TextBuildCache>,
                        mut accumulator: Option<&mut TextBuildAccumulator>) -> Result<()> {
        let tokens = tokenize(text);
        let total = tokens.len() as u64;
        if let Some(accumulator) = accumulator.as_deref_mut() { accumulator.document(total)?; }
        if maintain_term_counts {
            if let Some(v) = self.store_ref().get(&keys::text_norm_key(field, docid))? {
                let old = Norm::decode(&v)?;
                return self.replace_norm(field, docid, old, text);
            }
        }
        let mut meta = if maintain_term_counts { Some(self.ensure_field_meta(field)?) } else { None };
        let mut tf: HashMap<String, u64> = HashMap::new();
        for t in tokens { *tf.entry(t).or_insert(0) += 1; }
        let mut norm_terms = Vec::with_capacity(tf.len());
        for (term, count) in &tf {
            let mut v = Vec::new();
            write_varint(&mut v, *count);
            // the doc's token count rides in EVERY posting (2h A3): BM25
            // then needs zero length lookups -- the 1M term query paid 200K
            // cold point-gets for lengths and took 10.8s before this.
            write_varint(&mut v, total);
            self.store().put(&keys::text_head_key(field, term.as_bytes(), docid), &v)?;
            let id = if let Some(accumulator) = accumulator.as_deref_mut() {
                accumulator.term(term, docid)?
            } else if maintain_term_counts {
                self.adjust_term_df(field, term, 1)?
            } else if let Some(id) = build_cache.as_deref().and_then(|cache| cache.get(term)) {
                id
            } else {
                let id = self.intern_term_for_build(field, term)?;
                if let Some(cache) = build_cache.as_deref_mut() { cache.insert(term, id); }
                id
            };
            norm_terms.push((id, *count));
        }
        let old_head_len = if maintain_term_counts {
            self.store_ref().get(&keys::text_head_doc_key(field, docid))?
                .map(|v| { let mut p = 0; required_varint(&v, &mut p, "head document length is truncated") })
                .transpose()?
        } else {
            // A backfill follows clear_text, so no head-length row can exist.
            None
        };
        if total == 0 {
            let mut dv = Vec::new(); write_varint(&mut dv, total);
            self.store().put(&keys::text_head_doc_key(field, docid), &dv)?;
        } else if old_head_len.is_some() {
            self.store().delete(&keys::text_head_doc_key(field, docid))?;
        }
        norm_terms.sort_unstable_by_key(|&(id, _)| id);
        let norm = Norm { total, owner: 0, terms: norm_terms };
        self.store().put(&keys::text_norm_key(field, docid), &norm.encode())?;
        if maintain_term_counts {
            // Ordinary writes keep this one small row current. A known-empty
            // initial backfill publishes its accumulated totals once at the
            // end instead of rewriting the same page for every document.
            let mk = keys::text_meta_key(field, 0);
            let mut m = self.store().get(&mk)?.map(|v| SegMeta::decode(&v)).transpose()?
                .unwrap_or(SegMeta { doc_count: 0, total_tokens: 0, dead_tokens: 0, dead: Vec::new(),
                    level: 0, term_rows: 0, posting_count: 0, logical_xor: 0, logical_sum: 0 });
            let resurrecting_head = m.dead.contains(&docid);
            if resurrecting_head {
                let old_tokens = old_head_len.unwrap_or(0);
                m.total_tokens = m.total_tokens.saturating_sub(old_tokens) + total;
                m.dead_tokens = m.dead_tokens.saturating_sub(old_tokens);
                m.dead.retain(|&d| d != docid);
            } else {
                m.doc_count += 1;
                m.total_tokens += total;
                m.dead.retain(|&d| d != docid);
            }
            self.store().put(&mk, &m.encode())?;
            let meta = meta.as_mut().unwrap();
            meta.live_docs = meta.live_docs.checked_add(1).ok_or(Error::TooLarge)?;
            meta.total_tokens = meta.total_tokens.checked_add(total).ok_or(Error::TooLarge)?;
            self.put_field_meta(field, meta)?;
        }
        Ok(())
    }

    fn put_head_build_meta(&mut self, field: u64, docs: u64, tokens: u64) -> Result<()> {
        self.store().put(&keys::text_meta_key(field, 0), &SegMeta {
            doc_count: docs, total_tokens: tokens, dead_tokens: 0, dead: Vec::new(),
            level: 0, term_rows: 0, posting_count: 0, logical_xor: 0, logical_sum: 0,
        }.encode())
    }

    /// Finish an initial backfill by grouping compact `(term id, exact word)`
    /// counts outside the database. The accumulator owns 8 MiB regardless of
    /// corpus size and spills runs to scratch; only repeated terms need a
    /// second dictionary write because one-document terms already hold `1`.
    pub fn finish_text_build(&mut self, field: u64) -> Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = std::env::temp_dir().join(format!("text-stats-{}-{}",
            std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let mut sort = crate::bulk::ExternalSort::new(&scratch, 8 << 20)?;
        let prefix = keys::text_norm_prefix(field);
        let mut failure = None; let mut docs = 0u64; let mut tokens = 0u64;
        self.store_ref().scan(&prefix)?.for_each_ref(|key, val| {
            if !key.starts_with(&prefix) { return false; }
            match Norm::decode(val) {
                Ok(norm) => {
                    docs = match docs.checked_add(1) { Some(n) => n, None => { failure = Some(Error::TooLarge); return false; } };
                    tokens = match tokens.checked_add(norm.total) { Some(n) => n, None => { failure = Some(Error::TooLarge); return false; } };
                    for (id, _) in norm.terms {
                        if let Err(e) = sort.push(id.to_be_bytes().to_vec(), Vec::new()) {
                            failure = Some(e); return false;
                        }
                    }
                }
                Err(e) => { failure = Some(e); return false; }
            }
            true
        })?;
        if let Some(e) = failure { return Err(e); }
        let mut runs = sort.finish()?;
        let mut iter = runs.iter()?;
        let mut current = None; let mut count = 0u64;
        while let Some(item) = iter.next() {
            let (key, _, _) = item?;
            if key.len() != 8 { return Err(corrupt("text statistic sort emitted an invalid key")); }
            let id = u64::from_be_bytes(key.try_into().unwrap());
            if current.is_some_and(|old| old != id) {
                self.publish_built_term_count(field, current.unwrap(), count)?;
                count = 0;
            }
            current = Some(id); count = count.checked_add(1).ok_or(Error::TooLarge)?;
        }
        if let Some(id) = current { self.publish_built_term_count(field, id, count)?; }
        self.put_head_build_meta(field, docs, tokens)?;
        let mut meta = self.ensure_field_meta(field)?;
        meta.live_docs = docs;
        meta.total_tokens = tokens;
        meta.term_stats_ready = true;
        self.put_field_meta(field, &meta)
    }

    pub fn finish_text_build_accum(&mut self, field: u64,
                                   mut accumulator: TextBuildAccumulator) -> Result<()> {
        if accumulator.sort.is_none() {
            let mut grouped: Vec<_> = accumulator.grouped.drain().collect();
            grouped.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            let mut lex_keys = Vec::with_capacity(grouped.len());
            for (key, count) in grouped {
                if key.len() < 9 || count == 0 {
                    return Err(corrupt("text build accumulator contains an invalid term record"));
                }
                let raw = u64::from_be_bytes(key[..8].try_into().unwrap());
                let term = &key[8..];
                if term.is_empty() || std::str::from_utf8(term).is_err() {
                    return Err(corrupt("text build accumulator contains an invalid word"));
                }
                let actual = self.reserve_built_term(field, raw, term)?;
                self.publish_built_term_count(field, actual, count)?;
                if actual != raw { self.rewrite_built_norm_terms(field, term, raw, actual)?; }
                lex_keys.push(keys::text_term_lex_key(field, term));
            }
            lex_keys.sort_unstable();
            for batch in lex_keys.chunks(64) { self.store().put_empty_batch(batch)?; }
            self.put_head_build_meta(field, accumulator.docs, accumulator.tokens)?;
            let mut meta = self.ensure_field_meta(field)?;
            meta.live_docs = accumulator.docs;
            meta.total_tokens = accumulator.tokens;
            meta.term_stats_ready = true;
            return self.put_field_meta(field, &meta);
        }
        accumulator.flush()?;
        let TextBuildAccumulator { sort, docs, tokens, .. } = accumulator;
        let sort = sort.unwrap();
        let mut runs = sort.finish()?;
        let mut iter = runs.iter()?;
        let mut group: Option<(u64, Vec<u8>, u64, u64)> = None; // raw, term, actual, count
        let mut lex_batch = Vec::with_capacity(64);
        while let Some(item) = iter.next() {
            let (key, value, _) = item?;
            if key.len() < 9 || value.len() != 8 {
                return Err(corrupt("text build sort emitted an invalid term record"));
            }
            let raw = u64::from_be_bytes(key[..8].try_into().unwrap());
            let term = &key[8..];
            if term.is_empty() || std::str::from_utf8(term).is_err() {
                return Err(corrupt("text build sort emitted an invalid word"));
            }
            let occurrences = u64::from_be_bytes(value.try_into().unwrap());
            if occurrences == 0 { return Err(corrupt("text build sort emitted a zero count")); }
            let changed = group.as_ref().is_some_and(|(old_raw, old_term, _, _)|
                *old_raw != raw || old_term.as_slice() != term);
            if changed {
                let (old_raw, old_term, actual, count) = group.take().unwrap();
                self.publish_built_term_count(field, actual, count)?;
                if actual != old_raw {
                    self.rewrite_built_norm_terms(field, &old_term, old_raw, actual)?;
                }
                lex_batch.push(keys::text_term_lex_key(field, &old_term));
                if lex_batch.len() == 64 {
                    lex_batch.sort_unstable();
                    self.store().put_empty_batch(&lex_batch)?;
                    lex_batch.clear();
                }
            }
            if group.is_none() {
                let actual = self.reserve_built_term(field, raw, term)?;
                group = Some((raw, term.to_vec(), actual, 0));
            }
            let (_, _, _, count) = group.as_mut().unwrap();
            *count = count.checked_add(occurrences).ok_or(Error::TooLarge)?;
        }
        if let Some((raw, term, actual, count)) = group {
            self.publish_built_term_count(field, actual, count)?;
            if actual != raw { self.rewrite_built_norm_terms(field, &term, raw, actual)?; }
            lex_batch.push(keys::text_term_lex_key(field, &term));
        }
        if !lex_batch.is_empty() {
            lex_batch.sort_unstable();
            self.store().put_empty_batch(&lex_batch)?;
        }
        self.put_head_build_meta(field, docs, tokens)?;
        let mut meta = self.ensure_field_meta(field)?;
        meta.live_docs = docs; meta.total_tokens = tokens; meta.term_stats_ready = true;
        self.put_field_meta(field, &meta)
    }

    fn reserve_built_term(&mut self, field: u64, raw: u64, term: &[u8]) -> Result<u64> {
        let mut id = raw;
        for _ in 0..1024 {
            let key = keys::text_term_id_key(field, id);
            match self.store_ref().get(&key)? {
                Some(v) => {
                    let (_, stored) = decode_term_row(&v)?;
                    if stored == term { return Ok(id); }
                    id = next_term_probe(id);
                }
                None => {
                    self.store().put(&key, &encode_term_row(term, 1))?;
                    return Ok(id);
                }
            }
        }
        Err(corrupt("built text term collision chain exceeds its bound"))
    }

    fn rewrite_built_norm_term(&mut self, field: u64, docid: u64,
                               raw: u64, actual: u64) -> Result<()> {
        let key = keys::text_norm_key(field, docid);
        let v = self.store_ref().get(&key)?
            .ok_or_else(|| corrupt("colliding built term has no document norm"))?;
        let mut norm = Norm::decode(&v)?;
        let pos = norm.terms.binary_search_by_key(&raw, |&(id, _)| id)
            .map_err(|_| corrupt("colliding built term is absent from its document norm"))?;
        norm.terms[pos].0 = actual;
        norm.terms.sort_unstable_by_key(|&(id, _)| id);
        if norm.terms.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(corrupt("resolved built term numbers are not unique"));
        }
        self.store().put(&key, &norm.encode())
    }

    /// A collision is discovered only after the external aggregation has
    /// grouped exact words.  The exact-word posting range is already present,
    /// so it supplies precisely the affected documents without retaining a
    /// corpus-sized side list during the build.
    fn rewrite_built_norm_terms(&mut self, field: u64, term: &[u8],
                                raw: u64, actual: u64) -> Result<()> {
        let mut prefix = keys::text_head_key(field, term, 0);
        prefix.truncate(prefix.len() - 8);
        let mut docs = Vec::new();
        self.store_ref().scan(&prefix)?.for_each_ref(|key, _| {
            if !key.starts_with(&prefix) { return false; }
            if key.len() != prefix.len() + 8 { return false; }
            docs.push(u64::from_be_bytes(key[prefix.len()..].try_into().unwrap()));
            true
        })?;
        for docid in docs { self.rewrite_built_norm_term(field, docid, raw, actual)?; }
        Ok(())
    }

    fn publish_built_term_count(&mut self, field: u64, id: u64, count: u64) -> Result<()> {
        if count <= 1 { return Ok(()); }
        let key = keys::text_term_id_key(field, id);
        let v = self.store_ref().get(&key)?
            .ok_or_else(|| corrupt("built text term has no dictionary row"))?;
        let (_, term) = decode_term_row(&v)?;
        let term = term.to_vec();
        self.store().put(&key, &encode_term_row(&term, count))
    }

    /// Replace one doc's text (the UPDATE path). The head is the mutable
    /// tier, so the old HEAD rows are removed physically -- dead-marking
    /// cannot express "these terms changed" -- while folded copies in
    /// segments are dead-marked as usual and dropped at the next fold.
    /// (Dead-mark-only replacement resurrected the doc's old head rows:
    /// "stale term still matches", caught by the ported e1 suite.)
    pub fn replace_text(&mut self, field: u64, docid: u64,
                        _old_text: Option<&str>, new_text: &str) -> Result<()> {
        let Some(v) = self.store_ref().get(&keys::text_norm_key(field, docid))? else {
            return self.index_text(field, docid, new_text);
        };
        self.replace_norm(field, docid, Norm::decode(&v)?, new_text)
    }

    fn replace_norm(&mut self, field: u64, docid: u64, old: Norm, new_text: &str) -> Result<()> {
        let mut meta = self.ensure_field_meta(field)?;
        let old_owner = meta.resolve_owner(old.owner);
        let mut new_tf: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for term in tokenize(new_text) { *new_tf.entry(term).or_insert(0) += 1; }
        let new_total: u64 = new_tf.values().copied().sum();

        // Build the complete new head copy first.  Old segment/head rows remain
        // authoritative until the norm and owner metadata below move.
        for (term, tf) in &new_tf {
            let mut v = Vec::new(); write_varint(&mut v, *tf); write_varint(&mut v, new_total);
            self.store().put(&keys::text_head_key(field, term.as_bytes(), docid), &v)?;
        }
        if new_total == 0 {
            let mut dv = Vec::new(); write_varint(&mut dv, new_total);
            self.store().put(&keys::text_head_doc_key(field, docid), &dv)?;
        } else {
            self.store().delete(&keys::text_head_doc_key(field, docid))?;
        }

        let old_ids: HashSet<u64> = old.terms.iter().map(|&(id, _)| id).collect();
        let mut new_norm_terms = Vec::with_capacity(new_tf.len());
        for (term, tf) in &new_tf {
            let id = match self.term_info(field, term)? {
                Some((_, id)) if old_ids.contains(&id) => id,
                Some(_) | None => self.adjust_term_df(field, term, 1)?,
            };
            new_norm_terms.push((id, *tf));
        }
        new_norm_terms.sort_unstable_by_key(|&(id, _)| id);
        let new_ids: HashSet<u64> = new_norm_terms.iter().map(|&(id, _)| id).collect();
        let mut removed_terms = Vec::new();
        for &(id, _) in old.terms.iter().filter(|(id, _)| !new_ids.contains(id)) {
            let term = self.term_by_id(field, id)?;
            self.adjust_term_df(field, &term, -1)?;
            removed_terms.push(term);
        }

        let head_key = keys::text_meta_key(field, 0);
        let mut head = self.store_ref().get(&head_key)?.map(|v| SegMeta::decode(&v)).transpose()?
            .unwrap_or(SegMeta { doc_count: 0, total_tokens: 0, dead_tokens: 0, dead: Vec::new(),
                level: 0, term_rows: 0, posting_count: 0, logical_xor: 0, logical_sum: 0 });
        if old_owner == 0 {
            head.total_tokens = head.total_tokens.checked_sub(old.total)
                .and_then(|n| n.checked_add(new_total)).ok_or_else(|| corrupt("head token accounting overflows"))?;
            for term in &removed_terms {
                self.store().delete(&keys::text_head_key(field, term.as_bytes(), docid))?;
            }
        } else {
            let mut owner = self.segment_meta(field, old_owner)?;
            if !owner.dead.contains(&docid) {
                owner.dead.push(docid);
                owner.dead_tokens = owner.dead_tokens.checked_add(old.total).ok_or(Error::TooLarge)?;
                self.store().put(&keys::text_meta_key(field, old_owner), &owner.encode())?;
            }
            head.doc_count = head.doc_count.checked_add(1).ok_or(Error::TooLarge)?;
            head.total_tokens = head.total_tokens.checked_add(new_total).ok_or(Error::TooLarge)?;
        }
        head.dead.retain(|&id| id != docid);
        self.store().put(&head_key, &head.encode())?;
        meta.total_tokens = meta.total_tokens.checked_sub(old.total)
            .and_then(|n| n.checked_add(new_total)).ok_or_else(|| corrupt("field token accounting overflows"))?;
        let norm = Norm { total: new_total, owner: 0, terms: new_norm_terms };
        self.store().put(&keys::text_norm_key(field, docid), &norm.encode())?;
        self.put_field_meta(field, &meta)
    }

    /// The deletion discipline (alive/dead bitmap, one value per segment):
    /// mark `docid` dead in the metadata row that owns its corpus counts.
    /// Postings are NOT touched -- folds drop dead docs physically (Law 3 shape).
    pub fn delete_text(&mut self, field: u64, docid: u64) -> Result<bool> {
        let Some(v) = self.store_ref().get(&keys::text_norm_key(field, docid))? else { return Ok(false); };
        let norm = Norm::decode(&v)?;
        let mut field_meta = self.ensure_field_meta(field)?;
        let owner = field_meta.resolve_owner(norm.owner);
        let mk = keys::text_meta_key(field, owner);
        let mut segment = self.segment_meta(field, owner)?;
        if segment.dead.contains(&docid) { return Ok(false); }
        segment.dead.push(docid);
        segment.dead_tokens = segment.dead_tokens.checked_add(norm.total).ok_or(Error::TooLarge)?;
        self.store().put(&mk, &segment.encode())?;
        if owner == 0 {
            // Ordinary non-empty head documents need no membership row: the
            // merge derives it from their postings.  A deletion keeps this
            // one small length row so a later resurrection can balance the
            // head's token totals after the norm is retired.
            let mut dv = Vec::new(); write_varint(&mut dv, norm.total);
            self.store().put(&keys::text_head_doc_key(field, docid), &dv)?;
        }
        for &(id, _) in &norm.terms {
            let term = self.term_by_id(field, id)?;
            self.adjust_term_df(field, &term, -1)?;
        }
        field_meta.live_docs = field_meta.live_docs.checked_sub(1)
            .ok_or_else(|| corrupt("text field document count underflows"))?;
        field_meta.total_tokens = field_meta.total_tokens.checked_sub(norm.total)
            .ok_or_else(|| corrupt("text field token count underflows"))?;
        self.put_field_meta(field, &field_meta)?;
        self.store().delete(&keys::text_norm_key(field, docid))?;
        Ok(true)
    }

    fn text_posting_cursor(&self, field: u64, term: &str) -> Result<Option<TextPostingCursor<'_>>> {
        let Some(manifest) = self.field_meta(field)? else { return Ok(None) };
        let mut sources = Vec::new();
        for (seg, _) in manifest.active {
            let meta = self.segment_meta(field, seg)?;
            let dead = meta.dead.into_iter().collect();
            let exact_key = keys::text_seg_key(field, seg, term.as_bytes());
            let exact = self.store_ref().get(&exact_key)?;
            let mut pending = VecDeque::new();
            let mut blocks_read = 0;
            let mut postings_decoded = 0;
            let scan_more = if let Some(value) = exact {
                let posts = decode_postings(&value)?;
                blocks_read = 1;
                postings_decoded = posts.len() as u64;
                let more = posts.len() == POSTING_BLOCK;
                pending.extend(posts);
                more
            } else {
                true
            };
            let prefix = keys::text_seg_block_prefix(field, seg, term.as_bytes());
            let scan = scan_more.then(|| self.store_ref().scan(&prefix)).transpose()?;
            sources.push(PostingSource {
                scan, prefix, pending, dead, head: false, blocks_read,
                postings_decoded,
            });
        }
        if manifest.head_redirect.is_none() {
            if let Some(value) = self.store_ref().get(&keys::text_meta_key(field, 0))? {
                let meta = SegMeta::decode(&value)?;
                let mut prefix = keys::text_head_key(field, term.as_bytes(), 0);
                prefix.truncate(prefix.len() - 8);
                sources.push(PostingSource {
                    scan: Some(self.store_ref().scan(&prefix)?),
                    prefix,
                    pending: VecDeque::new(),
                    dead: meta.dead.into_iter().collect(),
                    head: true,
                    blocks_read: 0,
                    postings_decoded: 0,
                });
            }
        }
        let mut cursor = TextPostingCursor {
            heads: vec![None; sources.len()],
            sources,
        };
        for i in 0..cursor.sources.len() {
            cursor.heads[i] = cursor.sources[i].next_live()?;
        }
        Ok(Some(cursor))
    }

    /// Count the union of exact query-term postings with one bounded block per
    /// active segment and term. This is the COUNT counterpart of ranked top-k:
    /// it never owns the matching document set.
    pub fn text_match_count(&self, field: u64, query: &str) -> Result<Option<u64>> {
        let diag = std::env::var_os("TEXT_DIAG").is_some();
        let started = std::time::Instant::now();
        let pages_before = self.store_ref().pool_stats();
        let mut terms = tokenize(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() { return Ok(Some(0)) }
        let mut cursors = Vec::with_capacity(terms.len());
        for term in &terms {
            let Some(cursor) = self.text_posting_cursor(field, term)? else { return Ok(None) };
            cursors.push(cursor);
        }
        let mut heads = Vec::with_capacity(cursors.len());
        for cursor in &mut cursors { heads.push(cursor.next()?); }
        let mut count = 0u64;
        while let Some(doc) = heads.iter().flatten().map(|posting| posting.0).min() {
            count = count.checked_add(1).ok_or(Error::TooLarge)?;
            for i in 0..cursors.len() {
                if heads[i].is_some_and(|posting| posting.0 == doc) {
                    heads[i] = cursors[i].next()?;
                }
            }
        }
        if diag {
            let (blocks, postings) = cursors.iter().fold((0u64, 0u64), |acc, cursor| {
                let current = cursor.counters();
                (acc.0 + current.0, acc.1 + current.1)
            });
            let after = self.store_ref().pool_stats();
            eprintln!(
                "TEXT_DIAG streaming_count={query:?} total_ms={:.3} blocks_read={} postings_scanned={} matched={} pool_logical={} pool_misses={}",
                started.elapsed().as_secs_f64() * 1000.0, blocks, postings, count,
                after.hits.saturating_add(after.misses)
                    .saturating_sub(pages_before.hits.saturating_add(pages_before.misses)),
                after.misses.saturating_sub(pages_before.misses),
            );
        }
        Ok(Some(count))
    }

    /// First `limit` BM25 matches in document-id order. Collection membership
    /// uses that same order, so an unordered SQL filter with OFFSET/LIMIT can
    /// stop its posting merge without changing which rows the old collection
    /// scan returned. Scores are computed while the term heads are resident.
    pub fn text_match_doc_limit(&self, field: u64, query: &str,
                                min_score: f64, limit: usize)
        -> Result<Option<Vec<(u64, f64)>>>
    {
        let diag = std::env::var_os("TEXT_DIAG").is_some();
        let started = std::time::Instant::now();
        let pages_before = self.store_ref().pool_stats();
        if !self.field_meta(field)?.is_some_and(|meta| meta.term_stats_ready) {
            return Ok(None);
        }
        if limit == 0 { return Ok(Some(Vec::new())) }
        let mut terms = tokenize(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() { return Ok(Some(Vec::new())) }
        let (n_docs, avg_len) = self.text_stats(field)?;
        let mut cursors = Vec::new();
        let mut idfs = Vec::new();
        for term in &terms {
            let Some((df, _)) = self.term_info(field, term)? else { continue };
            if df == 0 { continue }
            let Some(cursor) = self.text_posting_cursor(field, term)? else { return Ok(None) };
            cursors.push(cursor);
            let df = df as f64;
            idfs.push(((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln());
        }
        let mut heads = Vec::with_capacity(cursors.len());
        for cursor in &mut cursors { heads.push(cursor.next()?); }
        let mut out = Vec::with_capacity(limit.min(4096));
        let mut matches_scored = 0u64;
        while out.len() < limit {
            let Some(doc) = heads.iter().flatten().map(|posting| posting.0).min()
            else { break };
            let mut score = 0.0;
            for i in 0..cursors.len() {
                if let Some((posting_doc, tf, dl)) = heads[i] {
                    if posting_doc == doc {
                        let tf = tf as f64;
                        let dl = dl as f64;
                        let denom = tf + BM25_K1 as f64
                            * (1.0 - BM25_B as f64
                                + BM25_B as f64 * dl / avg_len.max(1.0));
                        score += idfs[i] * tf * (BM25_K1 as f64 + 1.0) / denom;
                        heads[i] = cursors[i].next()?;
                    }
                }
            }
            matches_scored += 1;
            if score > min_score { out.push((doc, score)); }
        }
        if diag {
            let (blocks, postings) = cursors.iter().fold((0u64, 0u64), |acc, cursor| {
                let current = cursor.counters();
                (acc.0 + current.0, acc.1 + current.1)
            });
            let after = self.store_ref().pool_stats();
            eprintln!(
                "TEXT_DIAG streaming_doc_limit={query:?} total_ms={:.3} blocks_read={} postings_scanned={} matches_scored={} returned={} pool_misses={}",
                started.elapsed().as_secs_f64() * 1000.0, blocks, postings,
                matches_scored, out.len(),
                after.misses.saturating_sub(pages_before.misses),
            );
        }
        Ok(Some(out))
    }

    /// All (docid, tf) postings for one exact term in `field`, across the
    /// head rows and every folded segment, minus dead docs. O(matches).
    pub fn text_postings(&self, field: u64, term: &str) -> Result<Vec<(u64, u64, u64)>> {
        let diag = std::env::var_os("TEXT_DIAG").is_some();
        let started = std::time::Instant::now();
        let pages_before = self.store_ref().pool_stats();
        let mut blocks_read = 0u64;
        let mut postings_decoded = 0u64;
        let manifest = self.field_meta(field)?;
        let segs: Vec<u32> = match &manifest {
            Some(m) => m.active.iter().map(|&(seg, _)| seg).collect(),
            None => self.legacy_segment_metas(field)?.into_iter()
                .filter_map(|(seg, _)| (seg != 0).then_some(seg)).collect(),
        };
        let mut out: Vec<(u64, u64, u64)> = Vec::new();
        for seg in segs {
            let meta = self.segment_meta(field, seg)?;
            let dead: HashSet<u64> = meta.dead.into_iter().collect();

            // The first bounded block keeps the historical exact-term key.
            // A rare term is therefore one point read; only a full first block
            // can have continuation rows and needs a prefix cursor.
            let exact = self.store_ref().get(&keys::text_seg_key(field, seg, term.as_bytes()))?;
            let scan_more = if let Some(v) = exact {
                blocks_read += 1;
                let posts = decode_postings(&v)?;
                postings_decoded += posts.len() as u64;
                let more = posts.len() == POSTING_BLOCK;
                for (doc, tf, dl) in posts {
                    if !dead.contains(&doc) { out.push((doc, tf, dl)); }
                }
                more
            } else { true }; // compatibility with the first block layout used during migration
            if scan_more {
                let prefix = keys::text_seg_block_prefix(field, seg, term.as_bytes());
                let mut failure = None;
                self.store_ref().scan(&prefix)?.for_each_ref(|key, val| {
                    if !key.starts_with(&prefix) { return false; }
                    if key.len() != prefix.len() + 8 { return false; }
                    match decode_postings(val) {
                        Ok(posts) => {
                            blocks_read += 1;
                            postings_decoded += posts.len() as u64;
                            for (doc, tf, dl) in posts {
                                if !dead.contains(&doc) { out.push((doc, tf, dl)); }
                            }
                        },
                        Err(e) => { failure = Some(e); return false; }
                    }
                    true
                })?;
                if let Some(e) = failure { return Err(e); }
            }
        }

        // A published head fold sets head_redirect until the old head range is
        // retired, so an interrupted merge never exposes both copies.
        let mut head = Vec::new();
        let head_meta = if manifest.as_ref().is_none_or(|m| m.head_redirect.is_none()) {
            self.store_ref().get(&keys::text_meta_key(field, 0))?
                .map(|v| SegMeta::decode(&v)).transpose()?
        } else { None };
        if let Some(head_meta) = head_meta {
            let dead: HashSet<u64> = head_meta.dead.into_iter().collect();
            let mut prefix = keys::text_head_key(field, term.as_bytes(), 0);
            prefix.truncate(prefix.len() - 8);
            let mut failure = None;
            self.store_ref().scan(&prefix)?.for_each_ref(|key, val| {
                if !key.starts_with(&prefix) { return false; }
                if key.len() != prefix.len() + 8 { return false; }
                let doc = u64::from_be_bytes(key[prefix.len()..].try_into().unwrap());
                let mut pos = 0;
                let decoded = required_varint(val, &mut pos, "head posting frequency is truncated")
                    .and_then(|tf| required_varint(val, &mut pos, "head posting length is truncated")
                        .map(|dl| (tf, dl)));
                match decoded {
                    Ok((tf, dl)) if pos == val.len() && tf > 0 => {
                        blocks_read += 1;
                        postings_decoded += 1;
                        if !dead.contains(&doc) { head.push((doc, tf, dl)); }
                    }
                    Ok(_) => { failure = Some(corrupt("head posting has invalid trailing bytes or frequency")); return false; }
                    Err(e) => { failure = Some(e); return false; }
                }
                true
            })?;
            if let Some(e) = failure { return Err(e); }
        }
        if out.is_empty() {
            if diag {
                let after = self.store_ref().pool_stats();
                eprintln!(
                    "TEXT_DIAG term={term:?} posting_ms={:.3} blocks_read={} postings_decoded={} live_postings={} pool_misses={}",
                    started.elapsed().as_secs_f64() * 1000.0, blocks_read,
                    postings_decoded, head.len(), after.misses.saturating_sub(pages_before.misses),
                );
            }
            return Ok(head);
        }
        if !head.is_empty() {
            let head_ids: HashSet<u64> = head.iter().map(|&(doc, _, _)| doc).collect();
            out.retain(|(doc, _, _)| !head_ids.contains(doc));
            out.extend(head);
        }
        out.sort_unstable_by_key(|&(doc, _, _)| doc);
        out.dedup_by_key(|posting| posting.0);
        if diag {
            let after = self.store_ref().pool_stats();
            eprintln!(
                "TEXT_DIAG term={term:?} posting_ms={:.3} blocks_read={} postings_decoded={} live_postings={} pool_misses={}",
                started.elapsed().as_secs_f64() * 1000.0, blocks_read,
                postings_decoded, out.len(), after.misses.saturating_sub(pages_before.misses),
            );
        }
        Ok(out)
    }

    /// BM25 statistics for `field`: (doc_count, avg_len) across segments,
    /// dead docs excluded from the count.
    fn text_stats(&self, field: u64) -> Result<(f64, f64)> {
        if let Some(meta) = self.field_meta(field)? {
            let docs = meta.live_docs.max(1);
            return Ok((docs as f64, meta.total_tokens as f64 / docs as f64));
        }
        let mut docs = 0u64; let mut tokens = 0u64;
        let mut dead = 0u64; let mut dead_tokens = 0u64;
        for seg in self.text_segments(field)? {
            if let Some(v) = self.store_ref().get(&keys::text_meta_key(field, seg))? {
                let m = SegMeta::decode(&v)?;
                docs += m.doc_count;
                tokens += m.total_tokens;
                dead += m.dead.len() as u64;
                dead_tokens += m.dead_tokens;
            }
        }
        let live = docs.saturating_sub(dead).max(1);
        let live_tokens = tokens.saturating_sub(dead_tokens);
        Ok((live as f64, live_tokens as f64 / live as f64))
    }

    /// Incremental live corpus counters used by BM25.  This is a point read.
    pub fn text_live_stats(&self, field: u64) -> Result<(u64, u64)> {
        if let Some(meta) = self.field_meta(field)? { return Ok((meta.live_docs, meta.total_tokens)); }
        let (docs, avg) = self.text_stats(field)?;
        Ok((docs as u64, (docs * avg).round() as u64))
    }

    /// Slow diagnostic oracle: recount current per-document norm rows from
    /// scratch.  Production scoring never calls it.
    pub fn text_recount_stats(&self, field: u64) -> Result<(u64, u64)> {
        let prefix = keys::text_norm_prefix(field);
        let mut docs = 0u64; let mut tokens = 0u64;
        let mut failure = None;
        self.store_ref().scan(&prefix)?.for_each_ref(|key, val| {
            if !key.starts_with(&prefix) { return false; }
            match Norm::decode(val) {
                Ok(norm) => {
                    docs = match docs.checked_add(1) { Some(n) => n, None => { failure = Some(Error::TooLarge); return false; } };
                    tokens = match tokens.checked_add(norm.total) { Some(n) => n, None => { failure = Some(Error::TooLarge); return false; } };
                }
                Err(e) => { failure = Some(e); return false; }
            }
            true
        })?;
        if let Some(e) = failure { return Err(e); }
        Ok((docs, tokens))
    }

    /// Slow diagnostic oracle for one word's live-document count.  It scans
    /// per-document norms from scratch; production ranking uses the dictionary
    /// point row maintained by writes instead.
    pub fn text_recount_term_doc_freq(&self, field: u64, term: &str) -> Result<u64> {
        let Some((_, id)) = self.term_info(field, term)? else { return Ok(0); };
        let prefix = keys::text_norm_prefix(field);
        let mut docs = 0u64; let mut failure = None;
        self.store_ref().scan(&prefix)?.for_each_ref(|key, val| {
            if !key.starts_with(&prefix) { return false; }
            match Norm::decode(val) {
                Ok(norm) if norm.terms.binary_search_by_key(&id, |&(term_id, _)| term_id).is_ok() => {
                    docs = match docs.checked_add(1) {
                        Some(n) => n,
                        None => { failure = Some(Error::TooLarge); return false; }
                    };
                }
                Ok(_) => {}
                Err(e) => { failure = Some(e); return false; }
            }
            true
        })?;
        if let Some(e) = failure { return Err(e); }
        Ok(docs)
    }

    pub fn text_term_doc_freq(&self, field: u64, term: &str) -> Result<Option<u64>> {
        if self.field_meta(field)?.is_some_and(|meta| !meta.term_stats_ready) { return Ok(None); }
        Ok(self.term_info(field, term)?.map(|(count, _)| count))
    }

    /// BM25 for an already chosen candidate slice.  Current-format stores do
    /// one field-stat point read, one term-frequency point read per distinct
    /// query term, and one norm point read per candidate.  No posting extent
    /// is opened, so ten candidates cost ten document reads whether the term
    /// occurs in one hundred or ten million documents.
    pub fn text_score_candidates(&self, field: u64, query: &str, cands: &[u64]) -> Result<Vec<f32>> {
        let mut terms = tokenize(query);
        terms.sort(); terms.dedup();
        let mut out = vec![0.0f32; cands.len()];
        if terms.is_empty() || cands.is_empty() { return Ok(out); }
        let (n_docs, avg_len) = self.text_stats(field)?;
        let stats_ready = !self.field_meta(field)?.is_some_and(|meta| !meta.term_stats_ready);
        let mut scored_terms = Vec::with_capacity(terms.len());
        for term in terms {
            let info = if stats_ready { self.term_info(field, &term)? } else { None };
            let df = match info {
                Some((df, _)) => df,
                None => self.text_postings(field, &term)?.len() as u64, // legacy compatibility only
            } as f64;
            if df > 0.0 {
                scored_terms.push((term, info.map(|(_, id)| id),
                    ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln()));
            }
        }
        let mut norms: Vec<Option<Norm>> = (0..cands.len()).map(|_| None).collect();
        if cands.len().saturating_mul(16) >= n_docs as usize {
            // Dense candidate batches are still candidate-proportional when a
            // sequential norm walk visits at most sixteen rows per candidate.
            // This avoids N random descents without ever falling back to the
            // term's posting union.
            let index: HashMap<u64, usize> = cands.iter().enumerate().map(|(i, &id)| (id, i)).collect();
            let prefix = keys::text_norm_prefix(field);
            let mut failure = None;
            self.store_ref().scan(&prefix)?.for_each_ref(|key, val| {
                if !key.starts_with(&prefix) { return false; }
                if key.len() != prefix.len() + 8 { failure = Some(corrupt("text norm key has an invalid length")); return false; }
                let doc = u64::from_be_bytes(key[prefix.len()..].try_into().unwrap());
                if let Some(&i) = index.get(&doc) {
                    match Norm::decode(val) { Ok(norm) => norms[i] = Some(norm), Err(e) => { failure = Some(e); return false; } }
                }
                true
            })?;
            if let Some(e) = failure { return Err(e); }
        } else {
            for (i, &docid) in cands.iter().enumerate() {
                if let Some(v) = self.store_ref().get(&keys::text_norm_key(field, docid))? {
                    norms[i] = Some(Norm::decode(&v)?);
                }
            }
        }
        for (i, norm) in norms.into_iter().enumerate() {
            let Some(norm) = norm else { continue; };
            let docid = cands[i];
            if norm.terms.is_empty() || scored_terms.iter().any(|(_, id, _)| id.is_none()) {
                // Old norms did not carry frequencies; preserve readability
                // without putting this database-sized fallback on new data.
                for (term, _, term_idf) in &scored_terms {
                    if let Some((_, tf, dl)) = self.text_postings(field, term)?.into_iter().find(|p| p.0 == docid) {
                        let (tf, dl) = (tf as f64, dl as f64);
                        let denom = tf + BM25_K1 as f64 *
                            (1.0 - BM25_B as f64 + BM25_B as f64 * dl / avg_len.max(1.0));
                        out[i] += (*term_idf * tf
                            * (BM25_K1 as f64 + 1.0) / denom) as f32;
                    }
                }
                continue;
            }
            for (_, term_id, term_idf) in &scored_terms {
                let Some(term_id) = term_id else { continue; };
                let Ok(pos) = norm.terms.binary_search_by_key(term_id, |&(id, _)| id) else { continue; };
                let tf = norm.terms[pos].1;
                let (tf, dl) = (tf as f64, norm.total as f64);
                let denom = tf + BM25_K1 as f64 *
                    (1.0 - BM25_B as f64 + BM25_B as f64 * dl / avg_len.max(1.0));
                out[i] += (*term_idf * tf
                    * (BM25_K1 as f64 + 1.0) / denom) as f32;
            }
        }
        Ok(out)
    }

    /// BM25 top-k for a multi-term query in one field. Current stores merge
    /// packed posting cursors in document order and retain only a k-entry heap.
    /// Each cursor owns at most one decoded 128-document block. Legacy stores
    /// without maintained term statistics use the materialising compatibility
    /// implementation below until they are rebuilt.
    pub fn text_search(&self, field: u64, query: &str, k: usize) -> Result<Vec<(u64, f64)>> {
        if self.field_meta(field)?.is_some_and(|meta| meta.term_stats_ready) {
            return self.text_search_streaming(field, query, k);
        }
        let diag = std::env::var_os("TEXT_DIAG").is_some();
        let total_started = std::time::Instant::now();
        let pages_before = self.store_ref().pool_stats();
        let terms = tokenize(query);
        if terms.is_empty() { return Ok(Vec::new()); }
        let stats_started = std::time::Instant::now();
        let (n_docs, avg_len) = self.text_stats(field)?;
        let stats_elapsed = stats_started.elapsed();
        let mut acc: std::collections::HashMap<u64, f64> = std::collections::HashMap::new();
        let mut seen: std::collections::HashSet<&String> = std::collections::HashSet::new();
        let mut posting_elapsed = std::time::Duration::ZERO;
        let mut scoring_elapsed = std::time::Duration::ZERO;
        let mut postings_scanned = 0usize;
        for term in &terms {
            if !seen.insert(term) { continue; } // repeated query words count once
            let posting_started = std::time::Instant::now();
            let posts = self.text_postings(field, term)?;
            posting_elapsed += posting_started.elapsed();
            if posts.is_empty() { continue; }
            postings_scanned += posts.len();
            let df = posts.len() as f64;
            let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
            let scoring_started = std::time::Instant::now();
            for (docid, tf, dl) in posts {
                let dl = dl as f64;
                let tf = tf as f64;
                let denom = tf + (BM25_K1 as f64) * (1.0 - BM25_B as f64
                    + (BM25_B as f64) * dl / avg_len.max(1.0));
                *acc.entry(docid).or_insert(0.0) += idf * tf * (BM25_K1 as f64 + 1.0) / denom;
            }
            scoring_elapsed += scoring_started.elapsed();
        }
        let matched = acc.len();
        let rank_started = std::time::Instant::now();
        let mut ranked: Vec<(u64, f64)> = acc.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(k);
        let rank_elapsed = rank_started.elapsed();
        if diag {
            let after = self.store_ref().pool_stats();
            eprintln!(
                "TEXT_DIAG search={query:?} total_ms={:.3} stats_ms={:.3} postings_ms={:.3} scoring_ms={:.3} ranking_ms={:.3} postings_scanned={} matched={} returned={} pool_misses={}",
                total_started.elapsed().as_secs_f64() * 1000.0,
                stats_elapsed.as_secs_f64() * 1000.0,
                posting_elapsed.as_secs_f64() * 1000.0,
                scoring_elapsed.as_secs_f64() * 1000.0,
                rank_elapsed.as_secs_f64() * 1000.0,
                postings_scanned, matched, ranked.len(),
                after.misses.saturating_sub(pages_before.misses),
            );
        }
        Ok(ranked)
    }

    fn text_search_streaming(&self, field: u64, query: &str, k: usize)
        -> Result<Vec<(u64, f64)>>
    {
        let diag = std::env::var_os("TEXT_DIAG").is_some();
        let started = std::time::Instant::now();
        let pages_before = self.store_ref().pool_stats();
        let mut terms = tokenize(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() || k == 0 { return Ok(Vec::new()) }
        let (n_docs, avg_len) = self.text_stats(field)?;
        let mut cursors = Vec::new();
        let mut idfs = Vec::new();
        for term in &terms {
            let Some((df, _)) = self.term_info(field, term)? else { continue };
            if df == 0 { continue }
            let Some(cursor) = self.text_posting_cursor(field, term)? else {
                return Err(corrupt("current text manifest disappeared during search"));
            };
            cursors.push(cursor);
            let df = df as f64;
            idfs.push(((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln());
        }
        let mut heads = Vec::with_capacity(cursors.len());
        for cursor in &mut cursors { heads.push(cursor.next()?); }
        let mut heap = BinaryHeap::new();
        let mut matches = 0u64;
        while let Some(doc) = heads.iter().flatten().map(|posting| posting.0).min() {
            let mut score = 0.0;
            for i in 0..cursors.len() {
                if let Some((posting_doc, tf, dl)) = heads[i] {
                    if posting_doc == doc {
                        let tf = tf as f64;
                        let dl = dl as f64;
                        let denom = tf + BM25_K1 as f64 *
                            (1.0 - BM25_B as f64 + BM25_B as f64 * dl / avg_len.max(1.0));
                        score += idfs[i] * tf * (BM25_K1 as f64 + 1.0) / denom;
                        heads[i] = cursors[i].next()?;
                    }
                }
            }
            matches += 1;
            let hit = RankedText { id: doc, score };
            if heap.len() < k {
                heap.push(hit);
            } else {
                let worst = heap.peek().unwrap();
                if score > worst.score || (score == worst.score && doc < worst.id) {
                    *heap.peek_mut().unwrap() = hit;
                }
            }
        }
        let mut ranked: Vec<(u64, f64)> = heap.into_iter()
            .map(|hit| (hit.id, hit.score)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        if diag {
            let (blocks, postings) = cursors.iter().fold((0u64, 0u64), |acc, cursor| {
                let current = cursor.counters();
                (acc.0 + current.0, acc.1 + current.1)
            });
            let after = self.store_ref().pool_stats();
            eprintln!(
                "TEXT_DIAG streaming_search={query:?} total_ms={:.3} blocks_read={} postings_scanned={} matched={} returned={} pool_misses={}",
                started.elapsed().as_secs_f64() * 1000.0, blocks, postings,
                matches, ranked.len(), after.misses.saturating_sub(pages_before.misses),
            );
        }
        Ok(ranked)
    }

    /// Fold the mutable head, then run size-tiered levels.  At most seven
    /// segments remain at any level: the eighth is k-way merged into the next
    /// level.  Every candidate is checkpointed and independently reopened and
    /// counted before the one-row active manifest can publish it.
    pub fn fold_text(&mut self, field: u64) -> Result<()> {
        self.fold_text_with_before_publish(field, |_, _| Ok(()))
    }

    fn fold_text_with_before_publish<F>(&mut self, field: u64, before_publish: F) -> Result<()>
    where F: FnOnce(&mut Graph, u32) -> Result<()> {
        if self.store_ref().get(&keys::text_meta_key(field, 0))?.is_none() { return Ok(()); }
        let mut manifest = self.ensure_field_meta(field)?;
        let new_seg = manifest.next_seg;
        manifest.next_seg = manifest.next_seg.checked_add(1).filter(|n| *n < keys::TEXT_TERM_STATS_SEG)
            .ok_or(Error::TooLarge)?;
        // Reserve the namespace before any candidate row is written.  A crash
        // before publication may leak that inactive range, but can never make
        // a later merge reuse it and mix stale rows into a replacement.
        self.put_field_meta(field, &manifest)?;
        self.commit()?;
        let built = self.build_text_segment(field, &[0], new_seg, 0)?;
        before_publish(self, new_seg)?;

        // PUBLISH: old head remains on disk but is ignored while redirect is
        // present.  A crash on either side of this checkpoint therefore sees
        // exactly one complete copy.
        manifest.active.push((new_seg, 0));
        manifest.head_redirect = Some(new_seg);
        manifest.generation = manifest.generation.checked_add(1).ok_or(Error::TooLarge)?;
        self.put_field_meta(field, &manifest)?;
        self.commit()?; self.checkpoint()?;

        self.rewrite_norm_owners(field, &[0], new_seg)?;
        let head_prefix = keys::text_seg_key(field, 0, b"");
        self.store().delete_prefix(&head_prefix)?;
        self.store().delete(&keys::text_meta_key(field, 0))?;
        self.commit()?; self.checkpoint()?;
        manifest.head_redirect = None;
        self.put_field_meta(field, &manifest)?;
        self.commit()?; self.checkpoint()?;
        debug_assert_eq!(built.doc_count, self.segment_meta(field, new_seg)?.doc_count);

        loop {
            let mut choice = None;
            let max_level = manifest.active.iter().map(|&(_, level)| level).max().unwrap_or(0);
            for level in 0..=max_level {
                let same: Vec<u32> = manifest.active.iter().filter_map(|&(seg, l)| (l == level).then_some(seg)).collect();
                if same.len() >= MERGE_FANOUT { choice = Some((level, same[..MERGE_FANOUT].to_vec())); break; }
            }
            let Some((level, sources)) = choice else { break; };
            manifest = self.merge_text_segments(field, manifest, &sources, level + 1)?;
        }
        Ok(())
    }

    fn build_text_segment(&mut self, field: u64, sources: &[u32], new_seg: u32, level: u32) -> Result<SegMeta> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = std::env::temp_dir().join(format!("text-merge-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let mut sort = crate::bulk::ExternalSort::new(&scratch, 8 << 20)?;
        let mut expected = SegMeta { doc_count: 0, total_tokens: 0, dead_tokens: 0, dead: Vec::new(),
            level, term_rows: 0, posting_count: 0, logical_xor: 0, logical_sum: 0 };

        for &source in sources {
            let source_meta = self.segment_meta(field, source)?;
            let dead: HashSet<u64> = source_meta.dead.into_iter().collect();
            let push_doc = |sort: &mut crate::bulk::ExternalSort, doc: u64, dl: u64| -> Result<()> {
                let mut k = vec![0]; k.extend_from_slice(&doc.to_be_bytes());
                let mut v = Vec::new(); write_varint(&mut v, dl); sort.push(k, v)
            };
            if source != 0 {
                let doc_prefix = keys::text_seg_doc_prefix(field, source);
                let mut doc_failure = None;
                self.store_ref().scan(&doc_prefix)?.for_each_ref(|key, val| {
                    if !key.starts_with(&doc_prefix) { return false; }
                    let result = if key.len() != doc_prefix.len() + 8 {
                        Err(corrupt("segment document key has an invalid length"))
                    } else {
                        let doc = u64::from_be_bytes(key[doc_prefix.len()..].try_into().unwrap());
                        let mut p = 0;
                        required_varint(val, &mut p, "segment document length is truncated").and_then(|dl|
                            if p == val.len() { push_doc(&mut sort, doc, dl) }
                            else { Err(corrupt("segment document length has trailing bytes")) })
                    };
                    if let Err(e) = result { doc_failure = Some(e); return false; }
                    true
                })?;
                if let Some(e) = doc_failure { return Err(e); }
            }
            let prefix = keys::text_seg_key(field, source, b"");
            let mut failure = None;
            self.store_ref().scan(&prefix)?.for_each_ref(|key, val| {
                if !key.starts_with(&prefix) { return false; }
                let body = &key[prefix.len()..];
                let push_post = |sort: &mut crate::bulk::ExternalSort, expected: &mut SegMeta,
                                 term: &[u8], doc: u64, tf: u64, dl: u64| -> Result<()> {
                    let mut k = Vec::with_capacity(term.len() + 10);
                    k.push(1); k.extend_from_slice(term); k.push(0); k.extend_from_slice(&doc.to_be_bytes());
                    let mut v = Vec::new(); write_varint(&mut v, tf); write_varint(&mut v, dl);
                    sort.push(k, v)?;
                    expected.posting_count = expected.posting_count.checked_add(1).ok_or(Error::TooLarge)?;
                    let h = posting_hash(term, doc, tf, dl);
                    expected.logical_xor ^= h; expected.logical_sum = expected.logical_sum.wrapping_add(h);
                    Ok(())
                };
                let result = if body.len() == 9 && body[0] == 0 {
                    let doc = u64::from_be_bytes(body[1..].try_into().unwrap());
                    if dead.contains(&doc) { Ok(()) } else {
                        let mut p = 0; required_varint(val, &mut p, "segment document length is truncated")
                            .and_then(|dl| if p == val.len() { push_doc(&mut sort, doc, dl) }
                                else { Err(corrupt("segment document length has trailing bytes")) })
                    }
                } else if source == 0 {
                    if body.len() < 10 || body[body.len() - 9] != 0 { Err(corrupt("head posting key is malformed")) } else {
                        let term = &body[..body.len() - 9];
                        let doc = u64::from_be_bytes(body[body.len() - 8..].try_into().unwrap());
                        if dead.contains(&doc) { Ok(()) } else {
                            let mut p = 0;
                            required_varint(val, &mut p, "head posting frequency is truncated").and_then(|tf|
                                required_varint(val, &mut p, "head posting length is truncated").and_then(|dl|
                                    if p == val.len() {
                                        push_doc(&mut sort, doc, dl)?;
                                        push_post(&mut sort, &mut expected, term, doc, tf, dl)
                                    }
                                    else { Err(corrupt("head posting has trailing bytes")) }))
                        }
                    }
                } else {
                    let (term, is_block) = if body.len() >= 10 && body[body.len() - 9] == 0 {
                        (&body[..body.len() - 9], true)
                    } else { (body, false) };
                    decode_postings(val).and_then(|posts| {
                        if is_block && posts.len() > POSTING_BLOCK { return Err(corrupt("text posting block exceeds its bound")); }
                        for (doc, tf, dl) in posts {
                            if !dead.contains(&doc) { push_post(&mut sort, &mut expected, term, doc, tf, dl)?; }
                        }
                        Ok(())
                    })
                };
                if let Err(e) = result { failure = Some(e); return false; }
                true
            })?;
            if let Some(e) = failure { return Err(e); }
        }

        let mut runs = sort.finish()?;
        let mut iter = runs.iter()?;
        let mut last_doc = None;
        let mut block_term: Vec<u8> = Vec::new();
        let mut block: Vec<(u64, u64, u64)> = Vec::with_capacity(POSTING_BLOCK);
        let mut blocks_for_term = 0usize;
        while let Some(item) = iter.next() {
            let (key, val, _) = item?;
            if key.first() == Some(&0) {
                if key.len() != 9 { return Err(corrupt("merged document key is malformed")); }
                let doc = u64::from_be_bytes(key[1..].try_into().unwrap());
                let mut p = 0; let dl = required_varint(&val, &mut p, "merged document length is truncated")?;
                if p != val.len() { return Err(corrupt("merged document length has trailing bytes")); }
                if last_doc == Some(doc) { continue; }
                if last_doc.is_some_and(|previous| previous > doc) { return Err(corrupt("merged segment document order regressed")); }
                self.store().put(&keys::text_seg_doc_key(field, new_seg, doc), &val)?;
                expected.doc_count = expected.doc_count.checked_add(1).ok_or(Error::TooLarge)?;
                expected.total_tokens = expected.total_tokens.checked_add(dl).ok_or(Error::TooLarge)?;
                last_doc = Some(doc);
                continue;
            }
            if key.first() != Some(&1) || key.len() < 11 || key[key.len() - 9] != 0 {
                return Err(corrupt("merged posting key is malformed"));
            }
            let term = &key[1..key.len() - 9];
            let doc = u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap());
            let mut p = 0; let tf = required_varint(&val, &mut p, "merged posting frequency is truncated")?;
            let dl = required_varint(&val, &mut p, "merged posting length is truncated")?;
            if p != val.len() { return Err(corrupt("merged posting has trailing bytes")); }
            if !block.is_empty() && (block_term.as_slice() != term || block.len() == POSTING_BLOCK) {
                let changed_term = block_term.as_slice() != term;
                expected.term_rows = expected.term_rows.checked_add(1).ok_or(Error::TooLarge)?;
                self.write_posting_block(field, new_seg, &block_term, &block, blocks_for_term == 0)?;
                blocks_for_term += 1;
                block.clear();
                if changed_term { blocks_for_term = 0; }
            }
            if block.is_empty() { block_term = term.to_vec(); }
            if block.last().is_some_and(|(previous, _, _)| *previous >= doc) {
                return Err(corrupt("merged term contains duplicate documents"));
            }
            block.push((doc, tf, dl));
        }
        if !block.is_empty() {
            expected.term_rows = expected.term_rows.checked_add(1).ok_or(Error::TooLarge)?;
            self.write_posting_block(field, new_seg, &block_term, &block, blocks_for_term == 0)?;
        }
        self.store().put(&keys::text_meta_key(field, new_seg), &expected.encode())?;
        self.commit()?; self.checkpoint()?;

        let cfg = crate::store::Config { budget_bytes: 16 * crate::page::PAGE_SIZE,
            io: self.store_ref().io_mode(), sync: crate::store::SyncMode::Off };
        let snapshot = crate::store::Store::open_snapshot(self.store_ref().dir(), cfg)?;
        let verifier = Graph::new(snapshot)?;
        verifier.verify_text_segment(field, new_seg, &expected)?;
        Ok(expected)
    }

    fn write_posting_block(&mut self, field: u64, seg: u32, term: &[u8],
                           block: &[(u64, u64, u64)], first: bool) -> Result<()> {
        if block.is_empty() || block.len() > POSTING_BLOCK { return Err(Error::TooLarge); }
        let mut value = Vec::new(); let mut last = 0u64;
        for &(doc, tf, dl) in block {
            write_varint(&mut value, doc.checked_sub(last).ok_or_else(|| corrupt("posting block order regressed"))?);
            write_varint(&mut value, tf); write_varint(&mut value, dl); last = doc;
        }
        let key = if first { keys::text_seg_key(field, seg, term) }
            else { keys::text_seg_block_key(field, seg, term, block[0].0) };
        self.store().put(&key, &value)
    }

    fn verify_text_segment(&self, field: u64, seg: u32, expected: &SegMeta) -> Result<()> {
        let actual = self.segment_meta(field, seg)?;
        if &actual != expected { return Err(corrupt("reopened text segment metadata differs from its builder manifest")); }
        let mut got = SegMeta { doc_count: 0, total_tokens: 0, dead_tokens: 0, dead: Vec::new(),
            level: expected.level, term_rows: 0, posting_count: 0, logical_xor: 0, logical_sum: 0 };
        let mut failure = None;
        let doc_prefix = keys::text_seg_doc_prefix(field, seg);
        self.store_ref().scan(&doc_prefix)?.for_each_ref(|key, val| {
            if !key.starts_with(&doc_prefix) { return false; }
            let result = if key.len() != doc_prefix.len() + 8 {
                Err(corrupt("verified segment document key has an invalid length"))
            } else {
                let mut p = 0;
                required_varint(val, &mut p, "verified segment document length is truncated").and_then(|dl| {
                    if p != val.len() { return Err(corrupt("verified segment document length has trailing bytes")); }
                    got.doc_count = got.doc_count.checked_add(1).ok_or(Error::TooLarge)?;
                    got.total_tokens = got.total_tokens.checked_add(dl).ok_or(Error::TooLarge)?; Ok(())
                })
            };
            if let Err(e) = result { failure = Some(e); return false; }
            true
        })?;
        if let Some(e) = failure.take() { return Err(e); }

        let prefix = keys::text_seg_key(field, seg, b"");
        self.store_ref().scan(&prefix)?.for_each_ref(|key, val| {
            if !key.starts_with(&prefix) { return false; }
            let body = &key[prefix.len()..];
            let result = if body.len() >= 10 && body[body.len() - 9] == 0 {
                let term = &body[..body.len() - 9];
                let first = u64::from_be_bytes(body[body.len() - 8..].try_into().unwrap());
                decode_postings(val).and_then(|posts| {
                    if posts.is_empty() || posts.len() > POSTING_BLOCK || posts[0].0 != first {
                        return Err(corrupt("verified text posting block manifest is invalid"));
                    }
                    got.term_rows = got.term_rows.checked_add(1).ok_or(Error::TooLarge)?;
                    for (doc, tf, dl) in posts {
                        got.posting_count = got.posting_count.checked_add(1).ok_or(Error::TooLarge)?;
                        let h = posting_hash(term, doc, tf, dl); got.logical_xor ^= h; got.logical_sum = got.logical_sum.wrapping_add(h);
                    }
                    Ok(())
                })
            } else if !body.is_empty() {
                let term = body;
                decode_postings(val).and_then(|posts| {
                    if posts.is_empty() || posts.len() > POSTING_BLOCK {
                        return Err(corrupt("verified exact text posting block exceeds its bound"));
                    }
                    got.term_rows = got.term_rows.checked_add(1).ok_or(Error::TooLarge)?;
                    for (doc, tf, dl) in posts {
                        got.posting_count = got.posting_count.checked_add(1).ok_or(Error::TooLarge)?;
                        let h = posting_hash(term, doc, tf, dl); got.logical_xor ^= h; got.logical_sum = got.logical_sum.wrapping_add(h);
                    }
                    Ok(())
                })
            } else { Err(corrupt("verified text segment contains an unknown row shape")) };
            if let Err(e) = result { failure = Some(e); return false; }
            true
        })?;
        if let Some(e) = failure { return Err(e); }
        if got != *expected { return Err(corrupt("reopened text segment counts or logical checksum differ")); }
        Ok(())
    }

    fn rewrite_norm_owners(&mut self, field: u64, old: &[u32], new_seg: u32) -> Result<()> {
        let prefix = keys::text_seg_doc_prefix(field, new_seg);
        let mut cursor = prefix.clone();
        loop {
            let mut docs = Vec::with_capacity(8192);
            self.store_ref().scan(&cursor)?.for_each_ref(|key, _| {
                if !key.starts_with(&prefix) || key.len() != prefix.len() + 8 { return false; }
                docs.push(u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap()));
                docs.len() < 8192
            })?;
            if docs.is_empty() { break; }
            for &doc in &docs {
                let nk = keys::text_norm_key(field, doc);
                if let Some(v) = self.store_ref().get(&nk)? {
                    let mut norm = Norm::decode(&v)?;
                    if old.contains(&norm.owner) { norm.owner = new_seg; self.store().put(&nk, &norm.encode())?; }
                }
            }
            let Some(next) = docs.last().copied().and_then(|doc| doc.checked_add(1)) else { break; };
            cursor = keys::text_seg_doc_key(field, new_seg, next);
            if docs.len() < 8192 { break; }
        }
        Ok(())
    }

    fn merge_text_segments(&mut self, field: u64, mut manifest: FieldMeta,
                           sources: &[u32], level: u32) -> Result<FieldMeta> {
        let new_seg = manifest.next_seg;
        manifest.next_seg = manifest.next_seg.checked_add(1).filter(|n| *n < keys::TEXT_TERM_STATS_SEG)
            .ok_or(Error::TooLarge)?;
        self.put_field_meta(field, &manifest)?;
        self.commit()?;
        self.build_text_segment(field, sources, new_seg, level)?;
        manifest.active.retain(|(seg, _)| !sources.contains(seg));
        manifest.active.push((new_seg, level));
        manifest.redirects = sources.iter().map(|&old| (old, new_seg)).collect();
        manifest.generation = manifest.generation.checked_add(1).ok_or(Error::TooLarge)?;
        self.put_field_meta(field, &manifest)?;
        self.commit()?; self.checkpoint()?; // PUBLISH before retirement
        self.rewrite_norm_owners(field, sources, new_seg)?;
        for &source in sources {
            self.store().delete_prefix(&keys::text_seg_key(field, source, b""))?;
            self.store().delete_prefix(&keys::text_seg_doc_prefix(field, source))?;
            self.store().delete(&keys::text_meta_key(field, source))?;
        }
        self.commit()?; self.checkpoint()?;
        manifest.redirects.clear();
        self.put_field_meta(field, &manifest)?;
        self.commit()?; self.checkpoint()?;
        Ok(manifest)
    }

    /// Enumerate every distinct term of `field` starting at `from` (bytes),
    /// across the head (deduped) and every folded segment, in NO global
    /// order (per-source order only) -- callers union/dedupe. The callback
    /// returns false to stop that source's walk.
    fn for_each_term(&self, field: u64, from: &[u8],
                     mut f: impl FnMut(&[u8]) -> bool) -> Result<()> {
        if self.field_meta(field)?.is_some_and(|meta| meta.term_stats_ready) {
            let prefix = keys::text_term_lex_prefix(field);
            let mut start = prefix.clone();
            start.extend_from_slice(from);
            self.store_ref().scan(&start)?.for_each_ref(|key, _| {
                if !key.starts_with(&prefix) { return false; }
                f(&key[prefix.len()..])
            })?;
            return Ok(());
        }
        let segs = self.text_segments(field)?;
        for &seg in &segs {
            if seg == 0 {
                let mut prefix = keys::text_head_key(field, b"", 0);
                prefix.truncate(prefix.len() - 9);
                let mut start = prefix.clone();
                start.extend_from_slice(from);
                let it = self.store_ref().scan(&start)?;
                let mut last: Vec<u8> = Vec::new();
                let mut go = true;
                it.for_each_ref(|key, _| {
                    if !key.starts_with(&prefix) { return false; }
                    let body = &key[prefix.len()..];
                    if body.len() < 10 || body[body.len() - 9] != 0x00 { return true; }
                    let term = &body[..body.len() - 9];
                    if term != last.as_slice() {
                        last = term.to_vec();
                        go = f(term);
                    }
                    go
                })?;
            } else {
                let prefix = keys::text_seg_key(field, seg, b"");
                let mut start = prefix.clone();
                start.extend_from_slice(from);
                let it = self.store_ref().scan(&start)?;
                let mut last = Vec::new();
                it.for_each_ref(|key, _| {
                    if !key.starts_with(&prefix) { return false; }
                    let body = &key[prefix.len()..];
                    if body.len() == 9 && body[0] == 0 { return true; }
                    let term = if body.len() >= 10 && body[body.len() - 9] == 0 {
                        &body[..body.len() - 9]
                    } else { body };
                    if term == last.as_slice() { return true; }
                    last = term.to_vec();
                    f(term)
                })?;
            }
        }
        Ok(())
    }

    /// All terms of `field` starting with `prefix`, clipped to the
    /// `limit` most frequent (Manticore's expansion rule: the rare tail of
    /// an expansion is mostly misspellings; keep the popular head).
    pub fn text_prefix_terms(&self, field: u64, prefix: &str, limit: usize)
        -> Result<Vec<String>>
    {
        let p = prefix.as_bytes();
        let mut terms: Vec<String> = Vec::new();
        self.for_each_term(field, p, |t| {
            if !t.starts_with(p) { return false; }
            if let Ok(s) = std::str::from_utf8(t) { terms.push(s.to_string()); }
            true
        })?;
        terms.sort_unstable(); terms.dedup();
        if terms.len() > limit {
            let mut by_df: Vec<(usize, String)> = terms.into_iter()
                .map(|t| {
                    let df = self.text_term_doc_freq(field, &t).ok().flatten()
                        .map(|n| n as usize)
                        .unwrap_or_else(|| self.text_postings(field, &t).map(|p| p.len()).unwrap_or(0));
                    (df, t)
                })
                .collect();
            by_df.sort_by(|a, b| b.0.cmp(&a.0));
            by_df.truncate(limit);
            terms = by_df.into_iter().map(|(_, t)| t).collect();
        }
        Ok(terms)
    }

    /// Terms of `field` within `max_edits` (Levenshtein) of `word` -- the
    /// typo walk. Terms are visited in sorted order per source; a full DP
    /// row per term with shared-prefix reuse is O(|term| * |word|) worst
    /// case but the row's min bound prunes: when every cell of the row
    /// exceeds max_edits the whole SUBTREE of terms sharing that prefix is
    /// dead, and the walk seeks past it (restart scan at prefix-successor)
    /// instead of visiting each term (typesense's trie-DP on our sorted
    /// keys; caps: typos by length 0/<5, 1/<9, 2 else).
    pub fn text_fuzzy_terms(&self, field: u64, word: &str, max_edits: u32, limit: usize)
        -> Result<Vec<(String, u32)>>
    {
        let w: Vec<char> = word.chars().collect();
        let n = w.len();
        let mut found: Vec<(String, u32)> = Vec::new();
        // DP over rows: row[j] = edits between term-prefix and word[..j]
        let dp_next = |row: &Vec<u32>, ch: char| -> Vec<u32> {
            let mut nr = vec![row[0] + 1];
            for j in 1..=n {
                let cost = if w[j - 1] == ch { 0 } else { 1 };
                nr.push((row[j] + 1).min(nr[j - 1] + 1).min(row[j - 1] + cost));
            }
            nr
        };
        let mut visit = |term: &[u8]| -> Vec<u8> /* seek-to, empty = continue */ {
            let Ok(t) = std::str::from_utf8(term) else { return Vec::new() };
            let mut row: Vec<u32> = (0..=n as u32).collect();
            let mut alive_prefix = 0usize; // chars consumed while any cell <= max
            let mut bytes_at_alive = 0usize;
            for (ci, ch) in t.chars().enumerate() {
                row = dp_next(&row, ch);
                if row.iter().min().copied().unwrap_or(u32::MAX) > max_edits {
                    // dead prefix: everything sharing t[..=ci] is dead; seek
                    // to the successor of that byte prefix.
                    let dead_bytes = t.char_indices().nth(ci + 1)
                        .map(|(i, _)| i).unwrap_or(t.len());
                    let mut succ = term[..dead_bytes].to_vec();
                    while let Some(last) = succ.pop() {
                        if last < 0xFE { succ.push(last + 1); break; }
                    }
                    return succ;
                }
                alive_prefix = ci + 1;
                bytes_at_alive = t.char_indices().nth(ci + 1).map(|(i, _)| i).unwrap_or(t.len());
            }
            let _ = (alive_prefix, bytes_at_alive);
            if row[n] <= max_edits {
                found.push((t.to_string(), row[n]));
            }
            Vec::new()
        };
        // walk each source with seek-restarts
        let mut cursor: Vec<u8> = Vec::new();
        loop {
            let mut seek: Option<Vec<u8>> = None;
            self.for_each_term(field, &cursor, |t| {
                if t.as_ref() < cursor.as_slice() { return true; } // other source lag
                let s = visit(t);
                if s.is_empty() { true } else { seek = Some(s); false }
            })?;
            match seek {
                Some(sk) if sk > cursor => cursor = sk,
                _ => break,
            }
        }
        found.sort();
        found.dedup();
        found.sort_by(|a, b| a.1.cmp(&b.1));
        found.truncate(limit);
        Ok(found)
    }

    /// Instant search (2h, the as-you-type shape): every token exact-or-typo
    /// expanded, the LAST token also prefix-expanded, documents must match
    /// ALL tokens (AND), ranked by (fewest edits used, then BM25 over the
    /// matched expansions). Typo budget by token length: 0 under 5 chars,
    /// 1 under 9, else 2.
    pub fn text_search_instant(&self, field: u64, query: &str, k: usize)
        -> Result<Vec<(u64, f64)>>
    {
        self.text_search_instant_typo(field, query, k, None)
    }

    /// The instant walk with the edit budget FORCED (SQL's `typo => n`): the
    /// length ladder is a default, not a floor -- a 4-char token gets 0 edits
    /// by default, so "warz" can only reach "wars" when the caller raises it.
    pub fn text_search_instant_typo(&self, field: u64, query: &str, k: usize,
                                    forced_edits: Option<u32>)
        -> Result<Vec<(u64, f64)>>
    {
        let tokens = tokenize(query);
        if tokens.is_empty() { return Ok(Vec::new()); }
        let budget = |t: &str| -> u32 {
            if let Some(f) = forced_edits { return f; }
            let l = t.chars().count();
            if l < 5 { 0 } else if l < 9 { 1 } else { 2 }
        };
        let mut per_token: Vec<Vec<(String, u32)>> = Vec::new();
        for (i, tok) in tokens.iter().enumerate() {
            let last = i + 1 == tokens.len();
            let mut cands: Vec<(String, u32)> = Vec::new();
            if last {
                cands.extend(self.text_prefix_terms(field, tok, 50)?
                    .into_iter().map(|t| (t, 0u32)));
            }
            if cands.is_empty() || !last {
                let b = budget(tok);
                cands.extend(self.text_fuzzy_terms(field, tok, b, 50)?);
            }
            if cands.is_empty() { return Ok(Vec::new()); } // AND semantics
            cands.sort(); cands.dedup();
            per_token.push(cands);
        }
        // gather per-token doc sets with best edit cost
        let (n_docs, _avg) = self.text_stats(field)?;
        let mut doc_sets: Vec<std::collections::HashMap<u64, (u32, f64)>> = Vec::new();
        for cands in &per_token {
            let mut m: std::collections::HashMap<u64, (u32, f64)> = std::collections::HashMap::new();
            for (term, edits) in cands {
                let posts = self.text_postings(field, term)?;
                if posts.is_empty() { continue; }
                let df = posts.len() as f64;
                let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
                for (docid, _tf, _dl) in posts {
                    let e = m.entry(docid).or_insert((*edits, idf));
                    if *edits < e.0 { *e = (*edits, idf); }
                }
            }
            doc_sets.push(m);
        }
        // AND-intersect, score = sum over tokens of (2 - edits)*1000 + idf
        let (first, rest) = doc_sets.split_first().unwrap();
        let mut out: Vec<(u64, f64)> = Vec::new();
        'doc: for (&docid, &(e0, idf0)) in first {
            let mut score = (2.0 - e0 as f64) * 1000.0 + idf0;
            for m in rest {
                let Some(&(e, idf)) = m.get(&docid) else { continue 'doc };
                score += (2.0 - e as f64) * 1000.0 + idf;
            }
            out.push((docid, score));
        }
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out.truncate(k);
        Ok(out)
    }

    /// Every segment id present for `field` (0 = head, if it has meta).
    pub fn text_segments(&self, field: u64) -> Result<Vec<u32>> {
        if let Some(meta) = self.field_meta(field)? {
            let mut segs = Vec::new();
            if meta.head_redirect.is_none()
                && self.store_ref().get(&keys::text_meta_key(field, 0))?.is_some()
            { segs.push(0); }
            segs.extend(meta.active.into_iter().map(|(seg, _)| seg));
            segs.sort_unstable();
            return Ok(segs);
        }
        let mut segs = Vec::new();
        let from = keys::text_meta_key(field, 0);
        let it = self.store_ref().scan(&from)?;
        it.for_each_ref(|key, _| {
            if key.first() != Some(&keys::TAG_TEXTMETA) || key.len() != 13 { return false; }
            let f = u64::from_be_bytes(key[1..9].try_into().unwrap());
            if f != field { return false; }
            let seg = u32::from_be_bytes(key[9..13].try_into().unwrap());
            if seg < keys::TEXT_TERM_STATS_SEG { segs.push(seg); }
            true
        })?;
        Ok(segs)
    }
}

#[cfg(test)]
mod merge_interruption_tests {
    use super::*;
    use crate::io::IoMode;
    use crate::store::{Config, Store, SyncMode};

    fn cfg() -> Config {
        Config { budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
    }

    #[test]
    fn an_interrupted_k_way_merge_leaves_every_old_batch_findable() {
        let d = tempfile::TempDir::new().unwrap();
        let before;
        {
            let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
            for round in 0..3u64 {
                for i in 0..20u64 {
                    g.index_text(1, round * 100 + i + 1,
                        &format!("common batch{round} token{i}")).unwrap();
                }
                g.fold_text(1).unwrap();
            }
            before = g.text_search(1, "common token7", 100).unwrap();
            let manifest = g.field_meta(1).unwrap().unwrap();
            let sources: Vec<u32> = manifest.active.iter().map(|&(seg, _)| seg).collect();

            // This is the exact pre-publication boundary: the candidate has
            // been built, checkpointed, reopened and verified, but the active
            // manifest still names only the old batches. Simulate process loss
            // by dropping the writer here.
            g.build_text_segment(1, &sources, manifest.next_seg, 1).unwrap();
        }
        let g = Graph::new(Store::open(d.path(), cfg()).unwrap()).unwrap();
        assert_eq!(g.text_search(1, "common token7", 100).unwrap(), before);
        assert_eq!(g.text_segments(1).unwrap().len(), 3,
            "an unpublished candidate must not displace any old batch");
    }

    #[test]
    fn an_interruption_after_publish_reads_the_verified_replacement() {
        let d = tempfile::TempDir::new().unwrap();
        let before;
        {
            let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
            for round in 0..3u64 {
                for i in 0..20u64 {
                    g.index_text(1, round * 100 + i + 1,
                        &format!("common batch{round} token{i}")).unwrap();
                }
                g.fold_text(1).unwrap();
            }
            before = g.text_search(1, "common token7", 100).unwrap();
            let mut manifest = g.field_meta(1).unwrap().unwrap();
            let sources: Vec<u32> = manifest.active.iter().map(|&(seg, _)| seg).collect();
            let replacement = manifest.next_seg;
            manifest.next_seg += 1;
            g.put_field_meta(1, &manifest).unwrap();
            g.commit().unwrap();
            g.build_text_segment(1, &sources, replacement, 1).unwrap();

            // Publish the independently verified replacement, then simulate a
            // crash before owner rewrites and old-batch retirement.  Redirects
            // keep updates valid; reads use the complete replacement.
            manifest.active.retain(|(seg, _)| !sources.contains(seg));
            manifest.active.push((replacement, 1));
            manifest.redirects = sources.iter().map(|&old| (old, replacement)).collect();
            g.put_field_meta(1, &manifest).unwrap();
            g.commit().unwrap(); g.checkpoint().unwrap();
        }
        let g = Graph::new(Store::open(d.path(), cfg()).unwrap()).unwrap();
        assert_eq!(g.text_search(1, "common token7", 100).unwrap(), before);
        assert_eq!(g.text_segments(1).unwrap(), vec![4],
            "a published verified replacement must be the sole visible batch");
    }
}

#[cfg(test)]
mod build_accumulator_tests {
    use super::*;

    fn build(open_budget: usize) -> Vec<(Vec<u8>, Vec<u8>, bool)> {
        let dir = tempfile::TempDir::new().unwrap();
        let mut accumulator = TextPostingAccumulator::new(
            &dir.path().join("blocks"), open_budget, 1024).unwrap();
        for doc in 1..=600u64 {
            accumulator.push(b"common".to_vec(), doc, 1, 4).unwrap();
            accumulator.push(format!("bucket{}", doc % 11).into_bytes(), doc, 2, 4).unwrap();
            accumulator.push(format!("unique{doc}").into_bytes(), doc, 1, 4).unwrap();
        }
        let (mut runs, postings, _, _, _) = accumulator.finish().unwrap();
        let rows: Vec<_> = TextBlockIter::new(runs.iter().unwrap(), 77)
            .collect::<Result<Vec<_>>>().unwrap();
        let decoded = rows.iter().map(|(_, value, _)| decode_postings(value).unwrap().len() as u64)
            .sum::<u64>();
        assert_eq!(decoded, postings);
        rows
    }

    #[test]
    fn forced_spill_is_byte_identical_to_unspilled_posting_blocks() {
        let one = TextPostingAccumulator::entry_bytes(b"unique600");
        let spilled = build(one * 3);
        let unspilled = build(8 << 20);
        assert_eq!(spilled, unspilled);
    }
}
