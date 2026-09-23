//! Persisted analyzer-v1 postings and exact BM25 search.
pub mod analyzer;
use crate::collections::{
    corrupt, catalog, invalid, layout_id, ordered, read_ordered, row_key, CollectionId, Database,
    EntityId, Error, IndexFamily, IndexId, IndexInfo, IndexState, Result,
};
use crate::Kind;
use kernel::btree::RangeIter;
use serde_json::Value;
use crate::text_analyzer::{self, Analysis};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
};

pub(crate) const TEXT_FEATURE: u64 = 0x10;
pub(crate) const POSTING: u8 = 0x75;
pub(crate) const NORM: u8 = 0x76;
pub(crate) const TERM_STATS: u8 = 0x77;
pub(super) const CORPUS_STATS: u8 = 0x78;
pub(crate) const TEXT_TAGS: [u8; 4] = [POSTING, NORM, TERM_STATS, CORPUS_STATS];
/// Every tag a text index owns, including the packed segment tier.
pub(super) const ALL_TEXT_TAGS: [u8; 6] = [
    POSTING,
    NORM,
    TERM_STATS,
    CORPUS_STATS,
    segments::SEGMENT,
    segments::NORM_BLOCK,
];
pub(crate) const BM25_VERSION: u16 = 1;
const MAX_QUERY_TERMS: usize = 64;
const K1: f64 = 1.2;
const B: f64 = 0.75;

#[derive(Clone, Copy, Debug)]
pub enum TextCandidates<'a> {
    All,
    /// IDs must belong to the index collection and be strictly sorted.
    SortedUnique(&'a [EntityId]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextMatch {
    Any,
    All,
    /// Ordered, contiguous analyzer-v1 tokens. Candidate generation intersects
    /// distinct terms; authoritative primary text decides exact membership.
    Phrase,
    /// TYPO-TOLERANT, with the LAST token completed as a prefix
    /// (`docs/lang/QL_CONTRACT.md` §4.6, `search(col, 'query')`).
    ///
    /// The query's tokens are expanded against the term dictionary by the
    /// bounded Levenshtein walk in [`fuzzy`], and a document matches when
    /// EVERY token has at least one of its accepted dictionary terms present.
    /// With one accepted term per token that is exactly [`TextMatch::All`],
    /// which is the mode this one generalises; it reads no row, adds no
    /// keyspace tag and adds no feature bit, because the dictionary it walks
    /// and the postings it hands back are both already on disk.
    Search,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextHit {
    pub id: EntityId,
    pub score: f64,
}

#[derive(Clone, Copy, Debug)]
struct HeapHit(TextHit);

impl PartialEq for HeapHit {
    fn eq(&self, other: &Self) -> bool {
        self.0.score.to_bits() == other.0.score.to_bits() && self.0.id == other.0.id
    }
}
impl Eq for HeapHit {}
impl PartialOrd for HeapHit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapHit {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .0
            .score
            .total_cmp(&self.0.score)
            .then_with(|| self.0.id.cmp(&other.0.id))
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Corpus {
    pub(crate) documents: u64,
    pub(super) tokens: u64,
}

pub mod fuzzy;
pub(crate) mod segments;
mod unicode_v1;

/// Does this file's collection header admit the packed segment tier?
///
/// The bit is in memory already, so a file that never folded pays nothing for
/// the second tier: no extra seek, no extra budget unit, and the head-row
/// invariants stay exactly as strict as they were before segments existed.
pub(crate) fn segments_enabled(db: &Database) -> bool {
    db.index_header
        .is_some_and(|header| header.features & segments::SEGMENT_FEATURE != 0)
}

pub(crate) fn index_prefix(tag: u8, id: IndexId) -> Vec<u8> {
    let mut key = vec![tag];
    key.extend(ordered(id.0));
    key
}

pub(crate) fn posting_prefix(id: IndexId, term: &str) -> Vec<u8> {
    let mut key = index_prefix(POSTING, id);
    key.extend(term.as_bytes());
    key.push(0);
    key
}

pub(crate) fn posting_key(id: IndexId, term: &str, sequence: u64) -> Vec<u8> {
    let mut key = posting_prefix(id, term);
    key.extend(ordered(sequence));
    key
}

pub(crate) fn norm_key(id: IndexId, sequence: u64) -> Vec<u8> {
    let mut key = index_prefix(NORM, id);
    key.extend(ordered(sequence));
    key
}

/// What a `0x76` head row says about a document, once the packed tier exists.
///
/// The value was always four bytes. Under feature bit `0x40` it gains one more
/// shape -- the EMPTY value -- meaning "this document is not in the index".
/// That is the norm counterpart of the `tf = 0` posting tombstone, and it is
/// needed for the same reason: a delete cannot cheaply remove one document
/// from a packed block, so it records the absence at the head, where a head
/// row already overrides the block.
///
/// `length = 0` could not be borrowed for this. The design admits an explicit
/// empty or punctuation-only string as a PRESENT document with zero tokens,
/// which is exactly what a `0x76` value of 0 already means; reusing it would
/// make a deleted document and an empty one the same bytes.
pub(crate) fn decode_norm(bytes: &[u8], segments_on: bool) -> Result<Option<u32>> {
    if bytes.is_empty() {
        if !segments_on {
            return Err(corrupt("text norm value length"));
        }
        return Ok(None);
    }
    decode_u32(bytes, "text document length").map(Some)
}

/// A document's indexed length, and where it came from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Norm {
    /// The length the index answers with, after the head override is applied.
    pub(crate) length: Option<u32>,
    /// A packed block holds this document, whatever the head says about it.
    pub(super) packed: bool,
}

/// The per-query text scratch: everything one query re-reads about the same
/// documents, held once instead of once per candidate.
///
/// Three things live here, and all three exist for the same reason -- a text
/// query visits documents in ascending sequence, and the on-disk shapes it
/// reads are all *ranges* keyed by sequence, so the naive "one point read per
/// document per thing" costs O(range) work O(range) times:
///
/// * **One decoded `0x7B` norm block.** A block carries 256 lengths behind one
///   key, so reading one document's length means decoding all 256 varints.
///   255 of every 256 lookups want the block the previous lookup decoded.
/// * **The gap to the next `0x76` norm head row.** A folded index writes NO
///   head rows at all, so the head probe that overrides the block is a
///   guaranteed miss for every document -- but it still costs a point read
///   each time. Remembering *where the next head row actually is* turns those
///   misses into no read at all until the cursor reaches it.
/// * **One decoded `0x7A` packed segment per query term.** A term's whole
///   posting list can be one segment; decoding it to extract one document's
///   frequency, once per matching document, is the O(df^2) shape this cache
///   exists to remove. The window records which sequences the decoded segment
///   is authoritative for, so an ascending walk decodes each segment once.
///
/// None of the three assumes ascending order for CORRECTNESS: each lookup
/// re-checks that the cached range covers the sequence asked for, and reloads
/// when it does not. A driver that hands out sequences in some other order
/// (a scalar range cursor, a spatial cursor) therefore still gets the right
/// answer -- it just pays the reload it would have paid anyway.
///
/// Sacrifice (Law 1): one norm block (at most 256 `(slot, length)` pairs), one
/// pending head-row entry, and one decoded segment per distinct query term
/// (at most `MAX_QUERY_TERMS`, each at most `MAX_SEGMENT_BYTES` of postings),
/// resident for the life of one query page.
#[derive(Default)]
pub(crate) struct TextScratch {
    block: Option<u64>,
    entries: Vec<(usize, u32)>,
    /// No `0x76` head row exists for any sequence in `head_from ..` below
    /// `head_next`. `head_next` is the first head row at or after `head_from`,
    /// already decoded (its inner `None` is the EMPTY "not indexed" value).
    head_from: u64,
    head_next: Option<(u64, Option<u32>)>,
    head_scanned: bool,
    /// One decoded packed segment per `(index, term)` this query has touched.
    windows: Vec<TermWindow>,
}

/// What one query knows about one term's postings, across both tiers.
///
/// **The packed tier.** `lower` is the sequence whose seek produced the
/// decoded segment and `upper` is the last sequence that segment holds, so
/// every sequence in `lower ..= upper` seeks to exactly this segment: inside
/// the window the answer is a binary search, and a sequence the window does
/// not list is genuinely absent from the packed tier. An empty `entries` with
/// `upper = u64::MAX` records the other true answer -- that no segment of this
/// term reaches `lower` at all.
///
/// **The head tier.** Same shape, one row at a time: `head_next` is the first
/// `0x75` row at or after `head_from`, so every sequence below it is known to
/// have no head row without a read. A folded index has no head rows at all, so
/// one seek answers the whole query; an unfolded one pays a seek per head row
/// it actually walks past, which is what the point read cost anyway.
struct TermWindow {
    index: IndexId,
    term: String,
    /// `0x75` prefix for this term, kept so a seek does not rebuild it.
    head_prefix: Vec<u8>,
    /// `0x7A` prefix for this term.
    segment_prefix: Vec<u8>,
    /// Reusable seek key, so an amortized lookup allocates nothing.
    key: Vec<u8>,
    head_from: u64,
    head_next: Option<(u64, u32)>,
    head_scanned: bool,
    lower: u64,
    upper: u64,
    entries: Vec<(u64, u32)>,
}

impl TextScratch {
    fn packed(&mut self, db: &Database, id: IndexId, sequence: u64) -> Result<Option<u32>> {
        let block = segments::norm_block_of(sequence);
        if self.block != Some(block) {
            self.entries.clear();
            if let Some(value) = db
                .store()?
                .get(&segments::norm_block_key(id, block))?
            {
                segments::decode_norm_block(&value, &mut self.entries)?;
            }
            self.block = Some(block);
        }
        let slot = segments::norm_slot_of(sequence);
        // `decode_norm_block` yields ascending slots, so the binary search is
        // correct for any block. A block that lists EVERY document it covers
        // -- which is what a folded index of a collection with no deletes
        // produces -- has the slot at its own index, and one bounds-checked
        // load answers what eight probing iterations answered before, once per
        // scored document.
        if let Some((found, length)) = self.entries.get(slot) {
            if *found == slot {
                return Ok(Some(*length));
            }
        }
        Ok(self
            .entries
            .binary_search_by_key(&slot, |(slot, _)| *slot)
            .ok()
            .map(|at| self.entries[at].1))
    }

    /// The `0x76` head row for one document, read through the gap cache.
    ///
    /// `Ok(None)` means no head row exists (the block, if any, is
    /// authoritative); `Ok(Some(value))` is the decoded head row, whose own
    /// `None` is the EMPTY value meaning "not in the index".
    fn head_norm(
        &mut self,
        db: &Database,
        id: IndexId,
        sequence: u64,
    ) -> Result<Option<Option<u32>>> {
        let stale = !self.head_scanned
            || sequence < self.head_from
            || self.head_next.is_some_and(|(at, _)| at < sequence);
        if stale {
            self.seek_head(db, id, sequence)?;
        }
        Ok(match self.head_next {
            Some((at, value)) if at == sequence => Some(value),
            _ => None,
        })
    }

    /// Position the head gap at the first `0x76` row at or after `from`.
    fn seek_head(&mut self, db: &Database, id: IndexId, from: u64) -> Result<()> {
        let prefix = index_prefix(NORM, id);
        let mut start = prefix.clone();
        start.extend(ordered(from));
        self.head_from = from;
        self.head_scanned = true;
        self.head_next = None;
        if let Some(row) = db.store()?.range(&start)?.next() {
            let (key, value) = row?;
            if key.starts_with(&prefix) {
                let mut at = prefix.len();
                let sequence = read_ordered(&key, &mut at)?;
                if at != key.len() || sequence == 0 {
                    return Err(corrupt("text norm key identity"));
                }
                self.head_next = Some((sequence, decode_norm(&value, true)?));
            }
        }
        Ok(())
    }

    /// The window for one `(index, term)`, created on first use.
    fn window(&mut self, id: IndexId, term: &str) -> usize {
        if let Some(at) = self
            .windows
            .iter()
            .position(|window| window.index == id && window.term == term)
        {
            return at;
        }
        self.windows.push(TermWindow {
            index: id,
            term: term.to_owned(),
            head_prefix: posting_prefix(id, term),
            segment_prefix: segments::segment_prefix(id, term),
            key: Vec::new(),
            head_from: 0,
            head_next: None,
            head_scanned: false,
            // An empty window: `lower > upper` matches no sequence.
            lower: 1,
            upper: 0,
            entries: Vec::new(),
        });
        self.windows.len() - 1
    }

    /// One document's live frequency for one term, across both tiers, with
    /// each tier read through the window that the last lookup left behind.
    ///
    /// Head row first -- it overrides, and `tf = 0` is the tombstone -- then
    /// the one segment whose key proves it could hold this document.
    fn posting(
        &mut self,
        db: &Database,
        id: IndexId,
        term: &str,
        sequence: u64,
    ) -> Result<Option<u32>> {
        let at = self.window(id, term);
        let stale = {
            let window = &self.windows[at];
            !window.head_scanned
                || sequence < window.head_from
                || window.head_next.is_some_and(|(at, _)| at < sequence)
        };
        if stale {
            self.seek_posting_head(db, at, sequence)?;
        }
        if let Some((found, frequency)) = self.windows[at].head_next {
            if found == sequence {
                return Ok((frequency != 0).then_some(frequency));
            }
        }
        if sequence < self.windows[at].lower || sequence > self.windows[at].upper {
            self.seek_segment(db, at, sequence)?;
        }
        let window = &self.windows[at];
        // `decode_into` yields strictly ascending sequences.
        Ok(window
            .entries
            .binary_search_by_key(&sequence, |(sequence, _)| *sequence)
            .ok()
            .map(|at| window.entries[at].1))
    }

    /// Position the head gap at the first `0x75` row at or after `from`.
    fn seek_posting_head(&mut self, db: &Database, at: usize, from: u64) -> Result<()> {
        let window = &mut self.windows[at];
        window.key.clear();
        window.key.extend_from_slice(&window.head_prefix);
        window.key.extend(ordered(from));
        window.head_from = from;
        window.head_scanned = true;
        window.head_next = None;
        let prefix_len = window.head_prefix.len();
        let found = match db.store()?.range(&self.windows[at].key)?.next() {
            Some(row) => {
                let (key, value) = row?;
                if key.starts_with(&self.windows[at].head_prefix) {
                    let mut cursor = prefix_len;
                    let sequence = read_ordered(&key, &mut cursor)?;
                    if cursor != key.len() || sequence == 0 {
                        return Err(corrupt("text posting identity/order"));
                    }
                    Some((sequence, decode_u32(&value, "text posting frequency length")?))
                } else {
                    None
                }
            }
            None => None,
        };
        self.windows[at].head_next = found;
        Ok(())
    }

    /// Load the one packed segment that could hold `sequence`.
    fn seek_segment(&mut self, db: &Database, at: usize, sequence: u64) -> Result<()> {
        let window = &mut self.windows[at];
        window.key.clear();
        window.key.extend_from_slice(&window.segment_prefix);
        window.key.extend(ordered(sequence));
        let prefix_len = window.segment_prefix.len();
        let found = match db.store()?.range(&self.windows[at].key)?.next() {
            Some(row) => {
                let (key, value) = row?;
                if key.starts_with(&self.windows[at].segment_prefix) {
                    let mut cursor = prefix_len;
                    let last = read_ordered(&key, &mut cursor)?;
                    if cursor != key.len() {
                        return Err(corrupt("text segment key identity"));
                    }
                    Some((last, value))
                } else {
                    None
                }
            }
            None => None,
        };
        let window = &mut self.windows[at];
        window.lower = sequence;
        match found {
            Some((last, value)) => {
                segments::decode_into(&value, &mut window.entries)?;
                if window.entries.last().map(|(sequence, _)| *sequence) != Some(last) {
                    return Err(corrupt("text segment key disagrees with its postings"));
                }
                window.upper = last;
            }
            // No segment of this term reaches `sequence`, so none reaches
            // anything above it either.
            None => {
                window.entries.clear();
                window.upper = u64::MAX;
            }
        }
        Ok(())
    }
}

/// Head row first, then the packed block.
///
/// One point read for a file that never folded (the block probe is skipped
/// when the feature bit is clear). A file that did folded pays neither of the
/// two point reads per document once the scratch is warm: the head row is
/// answered from the gap the last seek proved empty, and the block from the
/// one decode its 256 documents share.
pub(crate) fn read_norm_cached(
    db: &Database,
    id: IndexId,
    sequence: u64,
    cache: &mut TextScratch,
) -> Result<Norm> {
    if !segments_enabled(db) {
        let head = db.store()?.get(&norm_key(id, sequence))?;
        return Ok(Norm {
            length: head
                .as_deref()
                .map(|bytes| decode_u32(bytes, "text document length"))
                .transpose()?,
            packed: false,
        });
    }
    let head = cache.head_norm(db, id, sequence)?;
    let packed = cache.packed(db, id, sequence)?;
    Ok(Norm {
        length: match head {
            Some(length) => length,
            None => packed,
        },
        packed: packed.is_some(),
    })
}

/// The one-shot read, for the write path. Deliberately NOT the cached reader:
/// a writer touches one document and then moves on, so a range seek for the
/// head row would cost it more than the point get it replaces.
pub(super) fn read_norm(db: &Database, id: IndexId, sequence: u64) -> Result<Norm> {
    let segments_on = segments_enabled(db);
    let head = db.store()?.get(&norm_key(id, sequence))?;
    if !segments_on {
        return Ok(Norm {
            length: head
                .as_deref()
                .map(|bytes| decode_u32(bytes, "text document length"))
                .transpose()?,
            packed: false,
        });
    }
    let packed = TextScratch::default().packed(db, id, sequence)?;
    let length = match head.as_deref() {
        Some(bytes) => decode_norm(bytes, true)?,
        None => packed,
    };
    Ok(Norm {
        length,
        packed: packed.is_some(),
    })
}

pub(crate) fn term_stats_key(id: IndexId, term: &str) -> Vec<u8> {
    let mut key = index_prefix(TERM_STATS, id);
    key.extend(term.as_bytes());
    key.push(0);
    key
}

pub(crate) fn corpus_key(id: IndexId) -> Vec<u8> {
    index_prefix(CORPUS_STATS, id)
}

pub(crate) fn descriptor(index: &IndexInfo) -> Result<()> {
    if index.family != IndexFamily::Text
        || index.kind != Kind::Text
        || index.unique
        || index.encoding_version != 1
    {
        return Err(corrupt("text descriptor family/options"));
    }
    Ok(())
}

pub(super) fn selected(document: &Value, field: &str) -> Result<Option<Analysis>> {
    let Some(value) = document.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .ok_or_else(|| invalid("indexed text field must be a string"))?;
    text_analyzer::analyze(text).map(Some).map_err(invalid)
}

pub(crate) fn decode_u32(bytes: &[u8], what: &'static str) -> Result<u32> {
    if bytes.len() != 4 {
        return Err(corrupt(what));
    }
    Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
}

pub(crate) fn decode_count(bytes: &[u8], what: &'static str) -> Result<u64> {
    if bytes.len() != 8 {
        return Err(corrupt(what));
    }
    Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
}

pub(super) fn decode_tf(bytes: &[u8]) -> Result<u32> {
    let count = decode_u32(bytes, "text posting frequency length")?;
    if count == 0 {
        return Err(corrupt("zero text posting frequency"));
    }
    Ok(count)
}

pub(crate) fn decode_corpus(bytes: &[u8]) -> Result<Corpus> {
    if bytes.len() != 16 {
        return Err(corrupt("text corpus statistics length"));
    }
    Ok(Corpus {
        documents: u64::from_be_bytes(bytes[..8].try_into().unwrap()),
        tokens: u64::from_be_bytes(bytes[8..].try_into().unwrap()),
    })
}

fn encode_corpus(corpus: Corpus) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&corpus.documents.to_be_bytes());
    bytes[8..].copy_from_slice(&corpus.tokens.to_be_bytes());
    bytes
}

pub(crate) fn read_corpus(db: &Database, id: IndexId) -> Result<Corpus> {
    let bytes = db
        .store()?
        .get(&corpus_key(id))?
        .ok_or_else(|| corrupt("missing text corpus statistics"))?;
    decode_corpus(&bytes)
}

pub(crate) fn read_df(db: &Database, id: IndexId, term: &str) -> Result<Option<u64>> {
    db.store()?
        .get(&term_stats_key(id, term))?
        .map(|bytes| {
            let count = decode_count(&bytes, "text term statistics length")?;
            if count == 0 {
                return Err(corrupt("zero persisted document frequency"));
            }
            Ok(count)
        })
        .transpose()
}

/// One term's postings, read as one ascending stream over both tiers.
///
/// A term's live posting list is the packed segments a late build wrote, plus
/// the head rows written since, with one rule between them: **a head row wins**.
/// A head row exists for `(term, document)` only because a live write put it
/// there, so it is newer than any segment; `tf = 0` is the tombstone form,
/// which a delete or an update writes when the posting it is retiring sits
/// inside a packed segment it will not rewrite.
///
/// The count invariant survives the second tier intact: exactly `df` postings
/// come out, because a tombstone cancels its segment posting at the moment the
/// merge passes over it. Nothing has to consult the norm row to decide whether
/// a posting is alive.
pub(crate) struct TermPostings<'a> {
    segments: Option<SegmentSource<'a>>,
    head: HeadSource<'a>,
    expected: u64,
    seen: u64,
    /// Did this stream open PART WAY IN (`open_from`)? Then the postings
    /// before that point were never read, so the count invariant below --
    /// exactly `df` postings come out -- cannot be checked at exhaustion. The
    /// upper half of it still is: no stream may emit MORE than `df`.
    resumed: bool,
}

struct SegmentSource<'a> {
    inner: RangeIter<'a>,
    prefix: Vec<u8>,
    buffer: Vec<(u64, u32)>,
    at: usize,
    previous: Option<u64>,
    done: bool,
    /// The document this source was opened at. A segment is decoded whole, so
    /// the FIRST one a resumed stream loads can hold documents below the
    /// resume point; `at` skips them in memory rather than emitting and
    /// charging for them.
    from: u64,
}

struct HeadSource<'a> {
    inner: RangeIter<'a>,
    prefix: Vec<u8>,
    head: Option<(u64, u32)>,
    previous: Option<u64>,
    done: bool,
    loaded: bool,
    tombstones: bool,
}

impl<'a> TermPostings<'a> {
    pub(crate) fn open(db: &'a Database, id: IndexId, term: &str, expected: u64) -> Result<Self> {
        let tombstones = segments_enabled(db);
        let segments = if tombstones {
            let prefix = segments::segment_prefix(id, term);
            Some(SegmentSource {
                inner: db.store()?.range(&prefix)?,
                prefix,
                buffer: Vec::new(),
                at: 0,
                previous: None,
                done: false,
                from: 0,
            })
        } else {
            None
        };
        let prefix = posting_prefix(id, term);
        Ok(Self {
            segments,
            head: HeadSource {
                inner: db.store()?.range(&prefix)?,
                prefix,
                head: None,
                previous: None,
                done: false,
                loaded: false,
                tombstones,
            },
            expected,
            seen: 0,
            resumed: false,
        })
    }

    /// The same stream, opened at the first live posting whose document is at
    /// or after `from`.
    ///
    /// This is what lets a PAGED text query resume. Both tiers are ordered by
    /// document and both are reached through a range cursor, so neither has to
    /// be walked to be positioned: the head rows are keyed
    /// `prefix || ordered(sequence)`, so opening at `posting_key(.., from)`
    /// lands on the first row at or after `from`; a segment is keyed by the
    /// LAST document it packs, so opening at `segment_key(.., from)` lands on
    /// the first segment that can still reach `from`, and the documents below
    /// `from` inside that one segment are skipped in the decoded buffer.
    ///
    /// The cost is one descent per tier per term, against a walk of every
    /// posting the earlier pages already emitted.
    ///
    /// `from` is INCLUSIVE: the document the previous page stopped on comes
    /// out once more, and the caller's `after` comparison drops it. One
    /// candidate per page, which is what every other resumable driver pays.
    pub(crate) fn open_from(
        db: &'a Database,
        id: IndexId,
        term: &str,
        expected: u64,
        from: u64,
    ) -> Result<Self> {
        // Sequences start at one, so a resume at or below the first possible
        // document is the whole stream -- and opening it the ordinary way
        // keeps the exact count invariant, which a seek has to give up.
        if from <= 1 {
            return Self::open(db, id, term, expected);
        }
        let tombstones = segments_enabled(db);
        let segments = if tombstones {
            let prefix = segments::segment_prefix(id, term);
            let start = segments::segment_key(id, term, from);
            Some(SegmentSource {
                inner: db.store()?.range(&start)?,
                prefix,
                buffer: Vec::new(),
                at: 0,
                previous: None,
                done: false,
                from,
            })
        } else {
            None
        };
        let prefix = posting_prefix(id, term);
        let start = posting_key(id, term, from);
        Ok(Self {
            segments,
            head: HeadSource {
                inner: db.store()?.range(&start)?,
                prefix,
                head: None,
                previous: None,
                done: false,
                loaded: false,
                tombstones,
            },
            expected,
            seen: 0,
            resumed: true,
        })
    }

    fn load_segment(&mut self) -> Result<()> {
        let Some(source) = self.segments.as_mut() else {
            return Ok(());
        };
        while !source.done && source.at >= source.buffer.len() {
            let Some(row) = source.inner.next() else {
                source.done = true;
                return Ok(());
            };
            let (key, value) = row?;
            if !key.starts_with(&source.prefix) {
                source.done = true;
                return Ok(());
            }
            let mut at = source.prefix.len();
            let last = read_ordered(&key, &mut at)?;
            if at != key.len() {
                return Err(corrupt("text segment key identity"));
            }
            segments::decode_into(&value, &mut source.buffer)?;
            let first = source.buffer.first().map(|(sequence, _)| *sequence);
            if source.buffer.last().map(|(sequence, _)| *sequence) != Some(last) {
                return Err(corrupt("text segment key disagrees with its postings"));
            }
            // Segments of one term are disjoint ascending ranges.
            if source.previous.is_some_and(|previous| first.is_none_or(|f| previous >= f)) {
                return Err(corrupt("text segments overlap"));
            }
            source.previous = Some(last);
            // Ascending, so the documents below the resume point are a prefix
            // of the buffer. Zero for a stream opened at the start.
            source.at = source
                .buffer
                .partition_point(|(sequence, _)| *sequence < source.from);
        }
        Ok(())
    }

    fn load_head(&mut self) -> Result<()> {
        if self.head.loaded || self.head.done {
            return Ok(());
        }
        let Some(row) = self.head.inner.next() else {
            self.head.done = true;
            return Ok(());
        };
        let (key, value) = row?;
        if !key.starts_with(&self.head.prefix) {
            self.head.done = true;
            return Ok(());
        }
        let mut at = self.head.prefix.len();
        let sequence = read_ordered(&key, &mut at)?;
        if at != key.len()
            || sequence == 0
            || self.head.previous.is_some_and(|previous| previous >= sequence)
        {
            return Err(corrupt("text posting identity/order"));
        }
        let frequency = if self.head.tombstones {
            decode_u32(&value, "text posting frequency length")?
        } else {
            decode_tf(&value)?
        };
        self.head.previous = Some(sequence);
        self.head.head = Some((sequence, frequency));
        self.head.loaded = true;
        Ok(())
    }

    /// Next live posting, ascending. `spend` meters one unit per posting
    /// consumed from either tier and one per exhausted tier, so a file with no
    /// segments charges exactly what the single-tier reader charged.
    pub(crate) fn next<E: From<Error>>(
        &mut self,
        spend: &mut impl FnMut() -> std::result::Result<(), E>,
    ) -> std::result::Result<Option<(u64, u32)>, E> {
        loop {
            self.load_segment().map_err(E::from)?;
            self.load_head().map_err(E::from)?;
            let segment = self
                .segments
                .as_ref()
                .filter(|source| source.at < source.buffer.len())
                .map(|source| source.buffer[source.at]);
            let head = self.head.head;
            if segment.is_none() && head.is_none() {
                spend()?;
                if !self.resumed && self.seen != self.expected {
                    return Err(E::from(corrupt(
                        "text posting count disagrees with term statistics",
                    )));
                }
                return Ok(None);
            }
            let emit = match (segment, head) {
                (Some((s, _)), Some((h, tf))) if h == s => {
                    self.segments.as_mut().unwrap().at += 1;
                    self.head.head = None;
                    self.head.loaded = false;
                    (tf != 0).then_some((h, tf))
                }
                (Some((s, tf)), None) => {
                    self.segments.as_mut().unwrap().at += 1;
                    Some((s, tf))
                }
                (Some((s, tf)), Some((h, _))) if s < h => {
                    self.segments.as_mut().unwrap().at += 1;
                    Some((s, tf))
                }
                (_, Some((h, tf))) => {
                    self.head.head = None;
                    self.head.loaded = false;
                    (tf != 0).then_some((h, tf))
                }
                (None, None) => unreachable!(),
            };
            spend()?;
            let Some((sequence, frequency)) = emit else {
                continue;
            };
            if self.seen == self.expected {
                return Err(E::from(corrupt(
                    "text posting count exceeds term statistics",
                )));
            }
            self.seen += 1;
            return Ok(Some((sequence, frequency)));
        }
    }
}

/// The live term frequency of one `(term, document)` pair, across both tiers.
/// Head row first -- it overrides, and `tf = 0` means the posting is gone --
/// then the one segment whose key proves it could hold this document.
pub(crate) fn point_posting(
    db: &Database,
    id: IndexId,
    term: &str,
    sequence: u64,
    segments_on: bool,
    scratch: &mut TextScratch,
) -> Result<Option<u32>> {
    if !segments_on {
        // One tier, one point read: exactly what this was before segments.
        let Some(value) = db.store()?.get(&posting_key(id, term, sequence))? else {
            return Ok(None);
        };
        return decode_tf(&value).map(Some);
    }
    // Both tiers, through the query's term window. Re-seeking and re-decoding
    // the whole segment here, once per document, is what made a term of
    // document frequency `df` cost O(df^2) to score.
    scratch.posting(db, id, term, sequence)
}

fn checked_add(value: u64, amount: u64, what: &'static str) -> Result<u64> {
    value.checked_add(amount).ok_or_else(|| corrupt(what))
}

fn checked_sub(value: u64, amount: u64, what: &'static str) -> Result<u64> {
    value.checked_sub(amount).ok_or_else(|| corrupt(what))
}

fn apply_transition(
    db: &mut Database,
    index: &IndexInfo,
    entity: EntityId,
    old: Option<&Analysis>,
    new: Option<&Analysis>,
) -> Result<()> {
    descriptor(index)?;
    let norm_key = norm_key(index.id, entity.sequence);
    let persisted_norm = read_norm(db, index.id, entity.sequence)?;
    let contributed = persisted_norm.length.is_some();
    if let Some(length) = persisted_norm.length {
        let old = old.ok_or_else(|| corrupt("text norm exists for absent old value"))?;
        if length != old.length {
            return Err(corrupt("text norm disagrees with primary text"));
        }
    } else if old.is_some() && index.state == IndexState::Ready {
        return Err(corrupt("ready text index is missing a document norm"));
    }

    let mut corpus = read_corpus(db, index.id)?;
    if contributed {
        let old = old.unwrap();
        if corpus.documents == 0 || corpus.tokens < u64::from(old.length) {
            return Err(corrupt("text corpus statistics underflow"));
        }
    }

    let empty = Analysis {
        length: 0,
        terms: Default::default(),
    };
    let indexed_old = if contributed { old.unwrap() } else { &empty };
    let indexed_new = new.unwrap_or(&empty);
    let mut terms = BTreeSet::new();
    terms.extend(indexed_old.terms.keys().map(String::as_str));
    terms.extend(indexed_new.terms.keys().map(String::as_str));

    let segments_on = segments_enabled(db);
    for term in terms {
        let old_tf = indexed_old.terms.get(term).copied();
        let new_tf = indexed_new.terms.get(term).copied();
        let key = posting_key(index.id, term, entity.sequence);
        let persisted = db.store()?.get(&key)?;
        // `tf = 0` is the tombstone form, admitted only where the segment
        // feature bit is set. Without it this decodes exactly as before.
        let head = persisted
            .as_deref()
            .map(|bytes| {
                if segments_on {
                    decode_u32(bytes, "text posting frequency length")
                } else {
                    decode_tf(bytes)
                }
            })
            .transpose()?;
        match (old_tf, head) {
            (Some(expected), Some(tf)) if tf == expected => {}
            (Some(_), Some(_)) => return Err(corrupt("text posting disagrees with primary text")),
            // No head row, and this file has folded: the old posting is inside
            // a packed segment. Sacrifice (Law 4): a live write no longer
            // re-verifies a folded posting against the primary text it is
            // replacing -- reading it would mean decoding the whole segment,
            // which is the read-modify-write the second tier exists to avoid.
            // `verify_index` still checks every folded posting.
            (Some(_), None) if segments_on => {}
            (Some(_), None) => return Err(corrupt("text posting is missing")),
            // A tombstone left by an earlier delete of a folded posting.
            (None, Some(0)) => {}
            (None, Some(_)) => return Err(corrupt("unexpected text posting")),
            (None, None) => {}
        }
        if old_tf != new_tf {
            match new_tf {
                Some(tf) => db.writer()?.put(&key, &tf.to_be_bytes())?,
                // A head row is this write's own earlier posting: remove it.
                None if head.is_some() => {
                    db.writer()?.delete(&key)?;
                }
                // Nothing at the head and the file has folded: the posting is
                // packed inside a segment. Retire it by recording its absence
                // at the head, which overrides the segment from now on.
                None if segments_on => db.writer()?.put(&key, &0u32.to_be_bytes())?,
                None => {}
            }
        }

        let old_present = old_tf.is_some();
        let new_present = new_tf.is_some();
        let df = read_df(db, index.id, term)?;
        if old_present && df.is_none() {
            return Err(corrupt("text posting has no term statistics"));
        }
        if let Some(df) = df {
            if df > corpus.documents {
                return Err(corrupt("text document frequency exceeds corpus"));
            }
        }
        if old_present != new_present {
            let next = if new_present {
                checked_add(df.unwrap_or(0), 1, "text document frequency overflow")?
            } else {
                checked_sub(
                    df.ok_or_else(|| corrupt("missing text term statistics"))?,
                    1,
                    "text document frequency underflow",
                )?
            };
            let key = term_stats_key(index.id, term);
            if next == 0 {
                db.writer()?.delete(&key)?;
            } else {
                db.writer()?.put(&key, &next.to_be_bytes())?;
            }
        }
    }

    let old_documents = u64::from(contributed);
    let new_documents = u64::from(new.is_some());
    corpus.documents = checked_sub(
        corpus.documents,
        old_documents,
        "text document count underflow",
    )?;
    corpus.documents = checked_add(
        corpus.documents,
        new_documents,
        "text document count overflow",
    )?;
    corpus.tokens = checked_sub(
        corpus.tokens,
        u64::from(indexed_old.length),
        "text token count underflow",
    )?;
    corpus.tokens = checked_add(
        corpus.tokens,
        u64::from(indexed_new.length),
        "text token count overflow",
    )?;

    match new {
        Some(analysis) if !contributed || analysis.length != indexed_old.length => db
            .writer()?
            .put(&norm_key, &analysis.length.to_be_bytes())?,
        Some(_) => {}
        // A folded document's length lives inside a packed block that this
        // write cannot cheaply rewrite, so its absence is recorded at the
        // head instead -- the empty value, which overrides the block from now
        // on. The test is "is it in a block", not "is there a head row":
        // an earlier update may have left a head row over a block entry, and
        // deleting that row would uncover the stale packed length.
        None if persisted_norm.packed => db.writer()?.put(&norm_key, &[])?,
        None if contributed => {
            db.writer()?.delete(&norm_key)?;
        }
        None => {}
    }
    if old_documents != new_documents || indexed_old.length != indexed_new.length {
        db.writer()?
            .put(&corpus_key(index.id), &encode_corpus(corpus))?;
    }
    Ok(())
}

pub(crate) fn maintain_text(
    db: &mut Database,
    index: &IndexInfo,
    entity: EntityId,
    old: Option<&Value>,
    new: Option<&Value>,
) -> Result<()> {
    descriptor(index)?;
    let old = old
        .map(|document| selected(document, &index.field))
        .transpose()?
        .flatten();
    let new = new
        .map(|document| selected(document, &index.field))
        .transpose()?
        .flatten();
    apply_transition(db, index, entity, old.as_ref(), new.as_ref())
}

/// Analyze one immutable dense-v3 row's indexed text field. No vector sidecar
/// is fetched; `Kind` is checked against the row's historical layout.
pub(crate) fn analyze_row_bytes(
    db: &Database,
    index: &IndexInfo,
    row: &[u8],
) -> Result<Option<Analysis>> {
    let layout_id = layout_id(row)?;
    let layout = db.layout(layout_id)?;
    if let Some((_, kind)) = layout.fields.iter().find(|(name, _)| name == &index.field) {
        if kind != &Kind::Text {
            return Err(invalid("historical indexed text field changed kind"));
        }
    }
    match crate::dense_v3::read_field(&layout, row, &index.field).map_err(corrupt)? {
        crate::dense_v3::FieldValue::Missing | crate::dense_v3::FieldValue::Null => Ok(None),
        crate::dense_v3::FieldValue::Inline(Value::String(text)) => {
            Ok(Some(text_analyzer::analyze(&text).map_err(invalid)?))
        }
        crate::dense_v3::FieldValue::Inline(_) | crate::dense_v3::FieldValue::Vector { .. } => {
            Err(invalid("historical indexed text field changed kind"))
        }
    }
}

/// One build chunk's postings, accumulated before any of them is written.
///
/// Document-at-a-time is the wrong grain for a late text build. Per distinct
/// term per document `apply_transition` reads a posting, writes a posting,
/// reads a document frequency and writes it back; per document it also reads
/// and rewrites the one shared corpus row. The corpus row is the worst of it:
/// every document in the corpus rewrites the same key, so one late build dirties
/// that single page once per row. FTS5 does not do this -- it accumulates in
/// memory and flushes a segment.
///
/// This does the same inside one bounded chunk. Every term the chunk saw is
/// held once with its `(sequence, frequency)` list, so each document frequency
/// is read once and written once per chunk instead of once per document, the
/// corpus row is read once and written once per chunk, and every key -- posting,
/// norm, term statistics, corpus -- is written in ascending key order, which is
/// the run shape the B-tree appends into rather than splits for.
///
/// The persisted bytes are unchanged: a sum of per-document deltas is the same
/// number whether it is added in the store or in a `BTreeMap` first.
///
/// Sacrifice: one chunk's distinct terms and their posting lists are resident
/// while the chunk is built. That is RAM proportional to the change, not to the
/// store (Law 1), and the chunk is bounded at 256 documents; a 256-document
/// chunk of ordinary prose holds a few thousand short strings. Crash during a
/// chunk loses that chunk, exactly as before -- the descriptor's cursor only
/// advances with the writes.
///
/// Entities already carrying a norm (a live write landed ahead of the build
/// cursor and indexed them) are handed to the single-document path, which is a
/// pure verification there and writes nothing.
pub(crate) fn build_documents(
    db: &mut Database,
    index: &IndexInfo,
    documents_in: Vec<(EntityId, Option<Analysis>)>,
) -> Result<()> {
    descriptor(index)?;
    let mut corpus = read_corpus(db, index.id)?;
    // term -> (sequence, term frequency), both ascending by construction.
    let mut postings: BTreeMap<String, Vec<(u64, u32)>> = BTreeMap::new();
    let mut norms: Vec<(u64, u32)> = Vec::new();
    let mut documents = 0u64;

    for (entity, analysis) in documents_in {
        if read_norm(db, index.id, entity.sequence)?.length.is_some() {
            // Already contributed by a live write. Verify it, write nothing.
            apply_transition(db, index, entity, analysis.as_ref(), analysis.as_ref())?;
            continue;
        }
        let Some(analysis) = analysis else { continue };
        for (term, tf) in &analysis.terms {
            if *tf == 0 {
                return Err(corrupt("zero text posting frequency"));
            }
            postings
                .entry(term.clone())
                .or_default()
                .push((entity.sequence, *tf));
        }
        norms.push((entity.sequence, analysis.length));
        documents = checked_add(documents, 1, "text document count overflow")?;
        corpus.tokens = checked_add(
            corpus.tokens,
            u64::from(analysis.length),
            "text token count overflow",
        )?;
    }
    corpus.documents = checked_add(corpus.documents, documents, "text document count overflow")?;
    if norms.is_empty() {
        return Ok(());
    }

    // Read every document frequency first, then write in one ascending pass per
    // tag: postings (0x75), norms (0x76), term statistics (0x77), corpus (0x78).
    let mut frequencies: Vec<(String, u64)> = Vec::with_capacity(postings.len());
    for (term, docs) in &postings {
        let before = read_df(db, index.id, term)?.unwrap_or(0);
        let after = checked_add(
            before,
            docs.len() as u64,
            "text document frequency overflow",
        )?;
        if after > corpus.documents {
            return Err(corrupt("text document frequency exceeds corpus"));
        }
        frequencies.push((term.clone(), after));
    }
    for (term, docs) in &postings {
        for (sequence, tf) in docs {
            let key = posting_key(index.id, term, *sequence);
            if db.store()?.get(&key)?.is_some() {
                return Err(corrupt("unexpected text posting"));
            }
            db.writer()?.put(&key, &tf.to_be_bytes())?;
        }
    }
    for (sequence, length) in &norms {
        db.writer()?
            .put(&norm_key(index.id, *sequence), &length.to_be_bytes())?;
    }
    for (term, df) in &frequencies {
        db.writer()?
            .put(&term_stats_key(index.id, term), &df.to_be_bytes())?;
    }
    db.writer()?
        .put(&corpus_key(index.id), &encode_corpus(corpus))?;
    Ok(())
}

fn any_with_prefix(db: &Database, prefix: &[u8]) -> Result<bool> {
    for row in db.store()?.range(prefix)? {
        let (key, _) = row?;
        return Ok(key.starts_with(prefix));
    }
    Ok(false)
}

/// Can this text index be built the packed way?
///
/// Only from a clean slate. A norm row means some document has already
/// contributed -- a live write that landed while the descriptor said BUILDING,
/// or an earlier `build_index_step` -- and the packed builder has no cheap way
/// to fold those documents in without double-counting them, so the chunked
/// head-row builder finishes the job instead.
///
/// Norms **and** segments together mean an earlier packed build was
/// interrupted after it had published documents. That state is refused rather
/// than guessed at (Law 3): cancel the index and create it again.
pub(crate) fn sorted_build_possible(db: &Database, i: &IndexInfo) -> Result<bool> {
    descriptor(i)?;
    if !any_with_prefix(db, &index_prefix(NORM, i.id))?
        && !any_with_prefix(db, &index_prefix(segments::NORM_BLOCK, i.id))?
    {
        return Ok(true);
    }
    if any_with_prefix(db, &index_prefix(segments::SEGMENT, i.id))? {
        return Err(invalid(
            "this text index's packed build was interrupted after it published documents; \
             cancel it with begin_drop_index and create it again",
        ));
    }
    Ok(false)
}

/// Delete the derived rows an interrupted packed build may have left.
///
/// Safe because `sorted_build_possible` has already established that no norm
/// row exists: no document has contributed, so every segment, posting and
/// term statistic here belongs to an attempt that never published anything.
/// Nothing that is still reachable is deleted (Law 3).
fn clear_unpublished(db: &mut Database, i: &IndexInfo) -> Result<()> {
    loop {
        let mut keys = Vec::new();
        for tag in [segments::SEGMENT, POSTING, TERM_STATS] {
            let prefix = index_prefix(tag, i.id);
            for row in db.store()?.range(&prefix)? {
                let (key, _) = row?;
                if !key.starts_with(&prefix) {
                    break;
                }
                keys.push(key);
                if keys.len() >= 4096 {
                    break;
                }
            }
            if keys.len() >= 4096 {
                break;
            }
        }
        if keys.is_empty() {
            break;
        }
        for key in keys {
            db.writer()?.delete(&key)?;
        }
        db.commit()?;
    }
    let corpus = read_corpus(db, i.id)?;
    if corpus.documents != 0 || corpus.tokens != 0 {
        db.writer()?.put(
            &corpus_key(i.id),
            &encode_corpus(Corpus {
                documents: 0,
                tokens: 0,
            }),
        )?;
        db.commit()?;
    }
    Ok(())
}


/// One term's state for the whole packed build: the segment it is filling and
/// how many documents it has been seen in.
struct TermAccumulator {
    packer: segments::Packer,
    documents: u64,
}

impl TermAccumulator {
    fn new() -> Self {
        Self {
            packer: segments::Packer::new(),
            documents: 0,
        }
    }
}

/// Entry bytes one write group of the packed build carries. One packed segment
/// can be 3.6 KiB where a scalar index entry is a few dozen bytes, so
/// `chunk_rows` segments would be two orders of magnitude more WAL than
/// `chunk_rows` scalar keys and would exhaust a small allowance before the
/// grouping ever got a chance to halve itself. Charge one unit per entry plus
/// one per 64 bytes written.
const BYTES_PER_UNIT: usize = 64;

/// Rows one scan run visits before the builder is handed the store back to
/// flush what that run finished.
const SCAN_RUN_ROWS: usize = 65_536;

/// Unfinished segment bytes the accumulator may hold before it gives up
/// maximal packing to stay inside a bound. See `build_sorted`'s sacrifices.
const MAX_HELD_BYTES: usize = 64 * 1024 * 1024;

/// Write one sorted run of derived entries, committing on the group boundary.
#[allow(clippy::too_many_arguments)]
fn write_run(
    db: &mut Database,
    run: &mut Vec<(Vec<u8>, Vec<u8>)>,
    chunk_rows: usize,
    group: usize,
    pending: &mut usize,
    groups: &mut usize,
    commits: &mut usize,
    enabled: &mut bool,
) -> Result<()> {
    if run.is_empty() {
        return Ok(());
    }
    if !*enabled {
        db.enable_index_feature(segments::SEGMENT_FEATURE)?;
        *enabled = true;
    }
    run.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    for (key, value) in run.drain(..) {
        db.writer()?.put(&key, &value)?;
        *pending += 1 + value.len() / BYTES_PER_UNIT;
        if *pending >= chunk_rows {
            *groups += 1;
            *pending = 0;
            if *groups % group == 0 {
                db.commit()?;
                *commits += 1;
            }
        }
    }
    Ok(())
}

/// Analyze one immutable row's indexed text field into the build's analyzer.
///
/// The same decode and the same kind checks as `analyze_row_bytes`; only the
/// analyzer differs, and `text_analyzer`'s own oracle test pins the two to the
/// same terms, frequencies, token count and refusals.
fn analyze_row_bytes_into(
    db: &Database,
    index: &IndexInfo,
    row: &[u8],
    analyzer: &mut text_analyzer::BuildAnalyzer,
) -> Result<Option<u32>> {
    let layout_id = layout_id(row)?;
    let layout = db.layout(layout_id)?;
    if let Some((_, kind)) = layout.fields.iter().find(|(name, _)| name == &index.field) {
        if kind != &Kind::Text {
            return Err(invalid("historical indexed text field changed kind"));
        }
    }
    match crate::dense_v3::read_field(&layout, row, &index.field).map_err(corrupt)? {
        crate::dense_v3::FieldValue::Missing | crate::dense_v3::FieldValue::Null => Ok(None),
        crate::dense_v3::FieldValue::Inline(Value::String(text)) => {
            Ok(Some(analyzer.analyze(&text).map_err(invalid)?))
        }
        crate::dense_v3::FieldValue::Inline(_) | crate::dense_v3::FieldValue::Vector { .. } => {
            Err(invalid("historical indexed text field changed kind"))
        }
    }
}

/// The packed late build: accumulate each term's postings as it is scanned and
/// write ONE value per term per segment instead of one entry per pair.
///
/// This is what FTS5 does and what the head tier cannot: a term's postings are
/// gathered in memory under that term and emitted already packed, so each
/// term's document frequency is written exactly once, each term's postings are
/// written as one or a few packed values, and the shared corpus row is written
/// once for the whole build rather than once per chunk.
///
/// WHY THERE IS NO SORT HERE ANY MORE. The first version of this builder sent
/// every `(term, document, frequency)` triple through `index_sort`, the
/// external sorter the scalar and spatial builds use -- SQLite's `CREATE INDEX`
/// shape, an in-memory run spilled at a byte budget and merged k-way, so the
/// consumer sees one ascending stream. It works, and it cost about 6.9
/// allocations per posting: a clone of the key and of the value into the
/// sorter, a spill of ~25 bytes per posting once the corpus passed the 8 MiB
/// budget, and a re-read of ~86% of the postings back off those spill files
/// during the merge. Measured at 200,000 rows and 1.2M postings, that sort was
/// 48% of the build's wall time.
///
/// None of it was buying anything. A sort answers "which term does this
/// posting belong to" -- and the tokenizer already knew, at index time, for
/// free. Scanning primary rows already visits documents in ascending sequence
/// order, so a term's postings arrive ascending whether or not anything sorts
/// them: the packer for a term sees exactly the stream the merge used to hand
/// it, in exactly the same order, and therefore splits its segments at exactly
/// the same postings and writes byte-identical values. The pinned digests in
/// `tests/index_build_equivalence.rs` are the proof of that, not a hope.
///
/// Sacrifices (Law 4):
/// * One unfinished segment per DISTINCT term is resident for the life of the
///   build -- at most `MAX_SEGMENT_BYTES` each, and in total no more than the
///   packed postings themselves, which is roughly two bytes per posting. That
///   replaces the old sorter's 8 MiB run plus a spill file of about 25 bytes
///   per posting: less RAM than the sort's own budget at the corpus sizes that
///   used to spill, and no scratch file at all. Past `MAX_HELD_BYTES` -- which
///   needs a vocabulary of millions of rare terms -- the builder stops packing
///   segments maximally and flushes what it holds, so the bound is hard; the
///   index is the same postings in more, smaller segments, and reads the same.
/// * Completed segments are buffered until the end of the scan run that
///   completed them, at most `SCAN_RUN_ROWS` rows' worth.
/// * Crash atomicity is the same bounded-BUILDING deal every late build makes;
///   an interrupted packed build is cancelled and recreated rather than
///   resumed in place, because its published-document marker (the norms) is
///   written last.
/// * Deleting a folded document leaves its packed posting in place behind a
///   head tombstone. Keyspace is reclaimed by an explicit rebuild, not by the
///   delete.
pub(crate) fn build_sorted(
    db: &mut Database,
    i: &mut IndexInfo,
    chunk_rows: usize,
    group: usize,
    max_commits: Option<usize>,
) -> Result<usize> {
    descriptor(i)?;
    clear_unpublished(db, i)?;

    let mut analyzer = text_analyzer::BuildAnalyzer::new();
    let mut terms: Vec<TermAccumulator> = Vec::new();
    let mut corpus = Corpus {
        documents: 0,
        tokens: 0,
    };
    // Norms are packed straight off the scan -- `scan_collection_rows_from`
    // already walks primary rows in ascending sequence, which is the order the
    // blocks want -- and held until every posting is on disk, because a norm
    // row is what says a document has contributed.
    let mut norm_packer = segments::NormPacker::new();
    let mut norm_blocks: Vec<(u64, Vec<u8>)> = Vec::new();
    // Segments this scan run completed, flushed in key order when it ends.
    let mut run: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut held = 0usize;

    let mut after = 0u64;
    let mut pending = 0usize;
    let mut groups = 0usize;
    let mut commits = 0usize;
    let mut enabled = false;

    loop {
        let (last, exhausted) = {
            let source: &Database = db;
            let index = &*i;
            let terms = &mut terms;
            let analyzer = &mut analyzer;
            let run = &mut run;
            let held = &mut held;
            let corpus = &mut corpus;
            let norm_packer = &mut norm_packer;
            let norm_blocks = &mut norm_blocks;
            source.scan_collection_rows_from(
                index.collection,
                after,
                SCAN_RUN_ROWS,
                |eid, row| {
                    let Some(length) = analyze_row_bytes_into(source, index, row, analyzer)? else {
                        return Ok(());
                    };
                    if terms.len() < analyzer.names().len() {
                        terms.resize_with(analyzer.names().len(), TermAccumulator::new);
                    }
                    for &(id, frequency) in analyzer.document() {
                        if frequency == 0 {
                            return Err(corrupt("zero text posting frequency"));
                        }
                        let slot = &mut terms[id as usize];
                        slot.documents = checked_add(
                            slot.documents,
                            1,
                            "text document frequency overflow",
                        )?;
                        let before = slot.packer.held_bytes();
                        let finished = slot.packer.push(eid.sequence, frequency)?;
                        *held = *held + slot.packer.held_bytes() - before;
                        if let Some((last, packed)) = finished {
                            run.push((
                                segments::segment_key(
                                    index.id,
                                    &analyzer.names()[id as usize],
                                    last,
                                ),
                                packed,
                            ));
                        }
                    }
                    if let Some(block) = norm_packer.push(eid.sequence, length)? {
                        norm_blocks.push(block);
                    }
                    corpus.documents =
                        checked_add(corpus.documents, 1, "text document count overflow")?;
                    corpus.tokens = checked_add(
                        corpus.tokens,
                        u64::from(length),
                        "text token count overflow",
                    )?;
                    Ok(())
                },
            )?
        };
        after = last;

        if held > MAX_HELD_BYTES {
            for (id, slot) in terms.iter_mut().enumerate() {
                if let Some((last, packed)) = slot.packer.finish()? {
                    run.push((segments::segment_key(i.id, &analyzer.names()[id], last), packed));
                }
            }
            held = 0;
        }
        write_run(
            db,
            &mut run,
            chunk_rows,
            group,
            &mut pending,
            &mut groups,
            &mut commits,
            &mut enabled,
        )?;
        if max_commits.is_some_and(|n| commits >= n) {
            // Nothing is PUBLISHED -- the norms are still unwritten -- so the
            // cursor stays at zero however far the scan got. The next call
            // clears the unpublished segments and scans again from the start;
            // and if a live write has meanwhile made the packed path
            // impossible, the chunked head-row builder that takes over reads
            // the cursor literally and must not be told that rows it has
            // never seen are behind it.
            i.state = IndexState::Building { after: 0 };
            db.save_index(i)?;
            db.commit()?;
            return Ok((after as usize).div_ceil(chunk_rows).max(1));
        }
        if exhausted {
            break;
        }
    }
    if let Some(block) = norm_packer.finish()? {
        norm_blocks.push(block);
    }

    // Every term's last segment and its document frequency, in key order.
    for (id, slot) in terms.iter_mut().enumerate() {
        if slot.documents == 0 {
            continue;
        }
        let term: &str = &analyzer.names()[id];
        if let Some((last, packed)) = slot.packer.finish()? {
            run.push((segments::segment_key(i.id, term, last), packed));
        }
        run.push((
            term_stats_key(i.id, term),
            slot.documents.to_be_bytes().to_vec(),
        ));
    }
    write_run(
        db,
        &mut run,
        chunk_rows,
        group,
        &mut pending,
        &mut groups,
        &mut commits,
        &mut enabled,
    )?;

    // Norms last: they are the marker that says a document has contributed,
    // so nothing claims publication until every packed posting is on disk.
    //
    // A corpus of documents that analyze to no terms at all writes blocks and
    // no segments, so the bit is claimed here too rather than only in the
    // posting loop: the `0x7B` keyspace is admitted by the same bit and an
    // unadmitted entry is refused on the next open.
    if !enabled && !norm_blocks.is_empty() {
        db.enable_index_feature(segments::SEGMENT_FEATURE)?;
    }
    for (block, value) in norm_blocks {
        db.writer()?
            .put(&segments::norm_block_key(i.id, block), &value)?;
        pending += 1 + value.len() / BYTES_PER_UNIT;
        if pending >= chunk_rows {
            groups += 1;
            pending = 0;
            if groups % group == 0 {
                db.commit()?;
            }
        }
    }
    db.writer()?.put(&corpus_key(i.id), &encode_corpus(corpus))?;
    i.state = IndexState::Ready;
    db.save_index(i)?;
    db.commit()?;
    Ok((after as usize).div_ceil(chunk_rows).max(1))
}

pub(crate) fn drop_batch(db: &Database, id: IndexId, batch: usize) -> Result<(Vec<Vec<u8>>, bool)> {
    let mut keys = Vec::new();
    for tag in ALL_TEXT_TAGS {
        let prefix = index_prefix(tag, id);
        for row in db.store()?.range(&prefix)? {
            let (key, _) = row?;
            if !key.starts_with(&prefix) {
                break;
            }
            if keys.len() == batch {
                return Ok((keys, false));
            }
            keys.push(key);
        }
    }
    Ok((keys, true))
}

fn validate_candidates(index: &IndexInfo, candidates: TextCandidates<'_>) -> Result<()> {
    if let TextCandidates::SortedUnique(ids) = candidates {
        let mut previous = None;
        for candidate in ids {
            if candidate.collection != index.collection
                || previous.is_some_and(|old| old >= *candidate)
            {
                return Err(invalid(
                    "filtered candidates must be same-collection, sorted and unique",
                ));
            }
            previous = Some(*candidate);
        }
    }
    Ok(())
}

fn primary_phrase_matches(
    db: &Database,
    index: &IndexInfo,
    entity: EntityId,
    phrase: &[String],
    length: u32,
    terms: &[&str],
    frequencies: &[u32],
    examined: &mut usize,
    max_examined: usize,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<bool> {
    spend(examined, max_examined, cancelled)?;
    let row = db
        .store()?
        .get(&row_key(entity))?
        .ok_or_else(|| corrupt("text posting points to missing primary row"))?;
    let layout_id = layout_id(&row)?;
    let layout = db.layout(layout_id)?;
    let text = match crate::dense_v3::read_field(&layout, &row, &index.field).map_err(corrupt)? {
        crate::dense_v3::FieldValue::Inline(Value::String(text)) => text,
        crate::dense_v3::FieldValue::Missing | crate::dense_v3::FieldValue::Null => {
            return Err(corrupt("text posting points to absent primary text"));
        }
        crate::dense_v3::FieldValue::Inline(_) | crate::dense_v3::FieldValue::Vector { .. } => {
            return Err(corrupt("text posting points to non-text primary field"));
        }
    };
    let scanned = text_analyzer::analyze_phrase_document(&text, phrase, |event| match event {
        text_analyzer::PhraseScanEvent::Poll => {
            if cancelled() {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        }
        text_analyzer::PhraseScanEvent::Token => spend(examined, max_examined, cancelled),
    });
    let (analysis, matched) = match scanned {
        Ok(result) => result,
        Err(text_analyzer::PhraseScanError::Analysis(error)) => {
            return Err(corrupt(format!(
                "authoritative text violates analyzer bounds: {error}"
            )));
        }
        Err(text_analyzer::PhraseScanError::Callback(error)) => return Err(error),
    };
    if analysis.length != length {
        return Err(corrupt("text norm disagrees with authoritative primary text"));
    }
    for (term, frequency) in terms.iter().zip(frequencies) {
        if analysis.terms.get(*term).copied() != Some(*frequency) {
            return Err(corrupt(
                "text posting frequency disagrees with authoritative primary text",
            ));
        }
    }
    Ok(matched)
}

fn spend(
    examined: &mut usize,
    max_examined: usize,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<()> {
    if cancelled() {
        return Err(Error::Cancelled);
    }
    if *examined == max_examined {
        return Err(Error::Kernel(kernel::Error::ResourceLimit(
            "text max_examined exceeded",
        )));
    }
    *examined += 1;
    Ok(())
}

/// The corpus half of a BM25 score, settled before the query sees a document.
///
/// Only the AVERAGE document length lives here, and the arithmetic below is
/// written exactly as it always was. Folding the length normalisation into
/// `flat + per_token * length` is algebraically the same number and is NOT the
/// same float: `text_score_cost` compares scores with the definition bit for
/// bit, and it caught the rewrite. A rounding change is a behaviour change,
/// and this loop had no business making one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bm25Weights {
    average: f64,
}

/// The corpus half of the score, once per query.
pub(crate) fn bm25_weights(corpus: Corpus) -> Result<Bm25Weights> {
    // An EMPTY corpus is a new or emptied table, not a damaged one: it has no
    // postings, so these weights score nothing, and a ranked query over it is
    // zero rows. It used to be refused as Corrupt, which a caller cannot tell
    // from real damage. Documents without tokens, or tokens without
    // documents, is still a contradiction and still refused.
    if corpus.documents == 0 && corpus.tokens == 0 {
        return Ok(Bm25Weights { average: 1.0 });
    }
    if corpus.documents == 0 || corpus.tokens == 0 {
        return Err(corrupt("text corpus statistics cannot score postings"));
    }
    Ok(Bm25Weights {
        average: corpus.tokens as f64 / corpus.documents as f64,
    })
}

/// One term's inverse document frequency, once per query TERM.
///
/// This is a natural logarithm, and it was taken once per term PER DOCUMENT --
/// `text/bm25_common` scores 20,000 documents, so 20,000 logs whose arguments
/// were all the same number.
pub(crate) fn bm25_idf(corpus: Corpus, df: u64) -> Result<f64> {
    if corpus.documents == 0 || df > corpus.documents {
        return Err(corrupt("text document frequency exceeds the corpus"));
    }
    let n = corpus.documents as f64;
    Ok((1.0 + (n - df as f64 + 0.5) / (df as f64 + 0.5)).ln())
}

/// The per-document half: one multiply-add and one divide per matching term.
pub(crate) fn bm25_scored(
    weights: Bm25Weights,
    length: u32,
    frequencies: &[u32],
    idfs: &[f64],
) -> Result<f64> {
    if frequencies.len() != idfs.len() {
        return Err(corrupt("text corpus statistics cannot score postings"));
    }
    let normalised = K1 * (1.0 - B + B * f64::from(length) / weights.average);
    let mut score = 0.0;
    for (&frequency, &idf) in frequencies.iter().zip(idfs) {
        if frequency == 0 || frequency > length {
            return Err(corrupt("text posting/statistics scoring bounds"));
        }
        let tf = f64::from(frequency);
        score += idf * (tf * (K1 + 1.0)) / (tf + normalised);
    }
    if !score.is_finite() || score <= 0.0 {
        return Err(corrupt("non-finite text score"));
    }
    Ok(score)
}

pub(super) fn bm25(
    corpus: Corpus,
    length: u32,
    frequencies: &[u32],
    dfs: &[u64],
) -> Result<f64> {
    if corpus.documents == 0 || corpus.tokens == 0 || frequencies.len() != dfs.len() {
        return Err(corrupt("text corpus statistics cannot score postings"));
    }
    let weights = bm25_weights(corpus)?;
    let mut idfs = Vec::with_capacity(dfs.len());
    for &df in dfs {
        if df == 0 || df > corpus.documents {
            return Err(corrupt("text posting/statistics scoring bounds"));
        }
        idfs.push(bm25_idf(corpus, df)?);
    }
    bm25_scored(weights, length, frequencies, &idfs)
}

fn push_hit(heap: &mut BinaryHeap<HeapHit>, k: usize, hit: TextHit) {
    heap.push(HeapHit(hit));
    if heap.len() > k {
        heap.pop();
    }
}

impl Database {
    pub fn create_text_index(
        &mut self,
        collection: CollectionId,
        name: &str,
        field: &str,
    ) -> Result<IndexId> {
        self.ready_write()?;
        let info = self.collection_info(collection)?;
        let kind = info
            .layout
            .fields
            .iter()
            .find(|(candidate, _)| candidate == field)
            .map(|(_, kind)| kind.clone())
            .ok_or_else(|| invalid("index field must be declared"))?;
        if kind != Kind::Text {
            return Err(invalid("text index requires a Text field"));
        }
        let id = self.create_index(
            collection,
            name,
            field,
            kind,
            false,
            IndexFamily::Text,
            TEXT_FEATURE,
        )?;
        let result = self.writer()?.put(
            &corpus_key(id),
            &encode_corpus(Corpus {
                documents: 0,
                tokens: 0,
            }),
        );
        self.finish(result.map_err(Error::from))?;
        Ok(id)
    }

    /// What `search(col, 'query')` expands to against this index's term
    /// dictionary, WITHOUT preparing a query.
    ///
    /// The one reason it exists: a truncated walk must reach the client as a
    /// NOTICE rather than as a short answer that looks complete, and the
    /// notice is written where a statement is compiled
    /// (`sekejap-lang`), one layer above the prepared query. The walk is
    /// bounded by [`fuzzy::MAX_VISITED`] dictionary entries, so asking is
    /// cheap and asking twice is bounded twice.
    pub fn search_expansion(&self, id: IndexId, query: &str) -> Result<fuzzy::SearchExpansion> {
        let index = self.index_info(id)?;
        descriptor(&index)?;
        if index.state != IndexState::Ready {
            return Err(invalid("index is not ready"));
        }
        let expansion = fuzzy::expand(self, id, query, &mut || false)?;
        Ok(fuzzy::SearchExpansion {
            tokens: expansion.tokens.len(),
            terms: expansion.terms.len(),
            visited: expansion.visited,
            truncated: expansion.truncated,
            visit_cap: fuzzy::MAX_VISITED,
            term_cap: fuzzy::MAX_TERMS,
        })
    }

    /// Exact BM25 over distinct analyzer-v1 query terms. `max_examined`
    /// counts posting advances and terminal prefix probes in merge mode.
    /// Filtered mode counts one candidate/norm probe plus one point probe per
    /// distinct query term. Phrase refinement additionally counts one primary
    /// point-read and one unit per authoritative document token.
    pub fn query_text(
        &self,
        id: IndexId,
        query: &str,
        matching: TextMatch,
        k: usize,
        candidates: TextCandidates<'_>,
        max_examined: usize,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Vec<TextHit>> {
        if k > catalog::MAX_RESULTS {
            return Err(invalid("query result limit exceeds 65536"));
        }
        let index = self.index_info(id)?;
        descriptor(&index)?;
        if index.state != IndexState::Ready {
            return Err(invalid("index is not ready"));
        }
        validate_candidates(&index, candidates)?;
        let (analysis, phrase) = if matching == TextMatch::Phrase {
            let phrase = text_analyzer::analyze_phrase_query(query).map_err(invalid)?;
            (phrase.analysis, Some(phrase.sequence))
        } else {
            (text_analyzer::analyze(query).map_err(invalid)?, None)
        };
        if analysis.terms.len() > MAX_QUERY_TERMS {
            return Err(invalid("text query exceeds 64 distinct terms"));
        }
        if cancelled() {
            return Err(Error::Cancelled);
        }
        if k == 0 || analysis.terms.is_empty() {
            return Ok(Vec::new());
        }
        // The typo-tolerant mode replaces the query's own tokens with the
        // dictionary terms the bounded walk accepted for them, and keeps the
        // groups: a document matches when every token has one of its terms.
        let expansion = if matching == TextMatch::Search {
            Some(fuzzy::expand(self, id, query, &mut cancelled)?)
        } else {
            None
        };
        if expansion.as_ref().is_some_and(|e| e.terms.is_empty()) {
            return Ok(Vec::new());
        }
        let owned: Vec<&str> = match &expansion {
            Some(expansion) => expansion.terms.iter().map(String::as_str).collect(),
            None => analysis.terms.keys().map(String::as_str).collect(),
        };
        let terms = owned;
        let corpus = read_corpus(self, id)?;
        let mut dfs = Vec::with_capacity(terms.len());
        for term in &terms {
            let df = read_df(self, id, term)?.unwrap_or(0);
            if df > corpus.documents {
                return Err(corrupt("text document frequency exceeds corpus"));
            }
            dfs.push(df);
        }
        let groups = expansion.as_ref().map(|expansion| expansion.groups.clone());
        let satisfied = |present: &[bool]| -> bool {
            match (&groups, matching) {
                (Some(groups), _) => groups
                    .iter()
                    .all(|group| group.iter().any(|accepted| present[accepted.term])),
                (None, TextMatch::All | TextMatch::Phrase) => present.iter().all(|held| *held),
                (None, _) => present.iter().any(|held| *held),
            }
        };
        let mut examined = 0usize;
        let segments_on = segments_enabled(self);
        let mut norms = TextScratch::default();
        let mut heap = BinaryHeap::with_capacity(k.min(1024));
        match candidates {
            TextCandidates::SortedUnique(ids) => {
                for entity in ids {
                    spend(&mut examined, max_examined, &mut cancelled)?;
                    let Some(length) =
                        read_norm_cached(self, id, entity.sequence, &mut norms)?.length
                    else {
                        continue;
                    };
                    let mut frequencies = Vec::new();
                    let mut matched_dfs = Vec::new();
                    let mut present = vec![false; terms.len()];
                    for (at, (term, &df)) in terms.iter().zip(&dfs).enumerate() {
                        spend(&mut examined, max_examined, &mut cancelled)?;
                        if let Some(frequency) =
                            point_posting(self, id, term, entity.sequence, segments_on, &mut norms)?
                        {
                            present[at] = true;
                            frequencies.push(frequency);
                            matched_dfs.push(df);
                        }
                    }
                    if frequencies.is_empty() || !satisfied(&present) {
                        continue;
                    }
                    if let Some(phrase) = phrase.as_deref() {
                        if !primary_phrase_matches(
                            self,
                            &index,
                            *entity,
                            phrase,
                            length,
                            &terms,
                            &frequencies,
                            &mut examined,
                            max_examined,
                            &mut cancelled,
                        )? {
                            continue;
                        }
                    }
                    push_hit(
                        &mut heap,
                        k,
                        TextHit {
                            id: *entity,
                            score: bm25(corpus, length, &frequencies, &matched_dfs)?,
                        },
                    );
                }
            }
            TextCandidates::All => {
                let mut streams = Vec::new();
                for (term_index, (term, &df)) in terms.iter().zip(&dfs).enumerate() {
                    let mut postings = TermPostings::open(self, id, term, df)?;
                    let head = postings
                        .next(&mut || spend(&mut examined, max_examined, &mut cancelled))?;
                    streams.push((term_index, postings, head));
                }
                if matches!(matching, TextMatch::All | TextMatch::Phrase) && dfs.contains(&0) {
                    return Ok(Vec::new());
                }
                while let Some(sequence) =
                    streams.iter().filter_map(|s| s.2.map(|h| h.0)).min()
                {
                    let mut frequencies = vec![0; terms.len()];
                    for (term_index, postings, head) in &mut streams {
                        if head.is_some_and(|entry| entry.0 == sequence) {
                            frequencies[*term_index] = head.unwrap().1;
                            *head = postings
                                .next(&mut || spend(&mut examined, max_examined, &mut cancelled))?;
                        }
                    }
                    let matched = frequencies.iter().filter(|tf| **tf != 0).count();
                    let present: Vec<bool> = frequencies.iter().map(|tf| *tf != 0).collect();
                    if !satisfied(&present) {
                        continue;
                    }
                    let length = read_norm_cached(self, id, sequence, &mut norms)?
                        .length
                        .ok_or_else(|| corrupt("text posting has no document norm"))?;
                    let mut matched_tf = Vec::with_capacity(matched);
                    let mut matched_df = Vec::with_capacity(matched);
                    for (&tf, &df) in frequencies.iter().zip(&dfs) {
                        if tf != 0 {
                            matched_tf.push(tf);
                            matched_df.push(df);
                        }
                    }
                    if let Some(phrase) = phrase.as_deref() {
                        if !primary_phrase_matches(
                            self,
                            &index,
                            EntityId {
                                collection: index.collection,
                                sequence,
                            },
                            phrase,
                            length,
                            &terms,
                            &matched_tf,
                            &mut examined,
                            max_examined,
                            &mut cancelled,
                        )? {
                            continue;
                        }
                    }
                    push_hit(
                        &mut heap,
                        k,
                        TextHit {
                            id: EntityId {
                                collection: index.collection,
                                sequence,
                            },
                            score: bm25(corpus, length, &matched_tf, &matched_df)?,
                        },
                    );
                }
            }
        }
        let mut hits: Vec<_> = heap.into_iter().map(|hit| hit.0).collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        Ok(hits)
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    use kernel::{io::IoMode, store::{Config, SyncMode}};
    use serde_json::json;

    fn cfg() -> Config {
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        }
    }

    #[test]
    fn merge_refuses_df_smaller_than_postings_and_missing_df_with_postings() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
        let collection = db
            .create_collection(
                "docs",
                vec![("body".into(), Kind::Text)],
                Default::default(),
            )
            .unwrap();
        db.put(collection, "a", &json!({"body":"term"})).unwrap();
        db.put(collection, "b", &json!({"body":"term"})).unwrap();
        let index = db.create_text_index(collection, "body", "body").unwrap();
        assert!(db.build_index_step(index, 16).unwrap());
        db.commit().unwrap();

        let stats = term_stats_key(index, "term");
        db.writer()
            .unwrap()
            .put(&stats, &1u64.to_be_bytes())
            .unwrap();
        assert!(matches!(
            db.query_text(
                index,
                "term",
                TextMatch::Any,
                8,
                TextCandidates::All,
                8,
                || false,
            ),
            Err(Error::Corrupt(_))
        ));
        db.rollback().unwrap();

        db.writer().unwrap().delete(&stats).unwrap();
        assert!(matches!(
            db.query_text(
                index,
                "term",
                TextMatch::Any,
                8,
                TextCandidates::All,
                8,
                || false,
            ),
            Err(Error::Corrupt(_))
        ));
    }
}

#[cfg(test)]
#[path = "../../faults/text_fault_tests.rs"]
mod fault_tests;
