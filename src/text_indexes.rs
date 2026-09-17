//! Persisted analyzer-v1 postings and exact BM25 search.
use super::*;
use crate::text_analyzer::{self, Analysis};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
};

pub(super) const TEXT_FEATURE: u64 = 0x10;
pub(super) const POSTING: u8 = 0x75;
pub(super) const NORM: u8 = 0x76;
pub(super) const TERM_STATS: u8 = 0x77;
pub(super) const CORPUS_STATS: u8 = 0x78;
pub(super) const TEXT_TAGS: [u8; 4] = [POSTING, NORM, TERM_STATS, CORPUS_STATS];
pub(super) const BM25_VERSION: u16 = 1;
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
pub(super) struct Corpus {
    pub(super) documents: u64,
    pub(super) tokens: u64,
}

pub(super) fn index_prefix(tag: u8, id: IndexId) -> Vec<u8> {
    let mut key = vec![tag];
    key.extend(ordered(id.0));
    key
}

pub(super) fn posting_prefix(id: IndexId, term: &str) -> Vec<u8> {
    let mut key = index_prefix(POSTING, id);
    key.extend(term.as_bytes());
    key.push(0);
    key
}

pub(super) fn posting_key(id: IndexId, term: &str, sequence: u64) -> Vec<u8> {
    let mut key = posting_prefix(id, term);
    key.extend(ordered(sequence));
    key
}

pub(super) fn norm_key(id: IndexId, sequence: u64) -> Vec<u8> {
    let mut key = index_prefix(NORM, id);
    key.extend(ordered(sequence));
    key
}

pub(super) fn term_stats_key(id: IndexId, term: &str) -> Vec<u8> {
    let mut key = index_prefix(TERM_STATS, id);
    key.extend(term.as_bytes());
    key.push(0);
    key
}

pub(super) fn corpus_key(id: IndexId) -> Vec<u8> {
    index_prefix(CORPUS_STATS, id)
}

pub(super) fn descriptor(index: &IndexInfo) -> Result<()> {
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

pub(super) fn decode_u32(bytes: &[u8], what: &'static str) -> Result<u32> {
    if bytes.len() != 4 {
        return Err(corrupt(what));
    }
    Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
}

pub(super) fn decode_count(bytes: &[u8], what: &'static str) -> Result<u64> {
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

pub(super) fn decode_corpus(bytes: &[u8]) -> Result<Corpus> {
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

pub(super) fn read_corpus(db: &Database, id: IndexId) -> Result<Corpus> {
    let bytes = db
        .store()?
        .get(&corpus_key(id))?
        .ok_or_else(|| corrupt("missing text corpus statistics"))?;
    decode_corpus(&bytes)
}

pub(super) fn read_df(db: &Database, id: IndexId, term: &str) -> Result<Option<u64>> {
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
    let persisted_norm = db.store()?.get(&norm_key)?;
    let contributed = persisted_norm.is_some();
    if let Some(bytes) = persisted_norm.as_deref() {
        let length = decode_u32(bytes, "text document length")?;
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

    for term in terms {
        let old_tf = indexed_old.terms.get(term).copied();
        let new_tf = indexed_new.terms.get(term).copied();
        let key = posting_key(index.id, term, entity.sequence);
        let persisted = db.store()?.get(&key)?;
        match (old_tf, persisted.as_deref()) {
            (Some(expected), Some(bytes)) if decode_tf(bytes)? == expected => {}
            (Some(_), Some(_)) => return Err(corrupt("text posting disagrees with primary text")),
            (Some(_), None) => return Err(corrupt("text posting is missing")),
            (None, Some(_)) => return Err(corrupt("unexpected text posting")),
            (None, None) => {}
        }
        if old_tf != new_tf {
            match new_tf {
                Some(tf) => db.writer()?.put(&key, &tf.to_be_bytes())?,
                None => {
                    db.writer()?.delete(&key)?;
                }
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

pub(super) fn maintain_text(
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
pub(super) fn analyze_row_bytes(
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
pub(super) fn build_documents(
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
        if db
            .store()?
            .get(&norm_key(index.id, entity.sequence))?
            .is_some()
        {
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

pub(super) fn drop_batch(db: &Database, id: IndexId, batch: usize) -> Result<(Vec<Vec<u8>>, bool)> {
    let mut keys = Vec::new();
    for tag in TEXT_TAGS {
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

struct PostingStream<'a> {
    inner: RangeIter<'a>,
    prefix: Vec<u8>,
    term: usize,
    expected: u64,
    seen: u64,
    previous: Option<u64>,
    head: Option<(u64, u32)>,
}

impl PostingStream<'_> {
    fn advance(
        &mut self,
        examined: &mut usize,
        max_examined: usize,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<()> {
        // Terminal probes are metered too. The prefix boundary, rather than
        // persisted df, decides exhaustion; df is checked against what exists.
        spend(examined, max_examined, cancelled)?;
        let Some(row) = self.inner.next() else {
            if self.seen != self.expected {
                return Err(corrupt("text posting count disagrees with term statistics"));
            }
            self.head = None;
            return Ok(());
        };
        let (key, value) = row?;
        if !key.starts_with(&self.prefix) {
            if self.seen != self.expected {
                return Err(corrupt("text posting count disagrees with term statistics"));
            }
            self.head = None;
            return Ok(());
        }
        if self.seen == self.expected {
            return Err(corrupt("text posting count exceeds term statistics"));
        }
        let mut at = self.prefix.len();
        let sequence = read_ordered(&key, &mut at)?;
        if at != key.len()
            || sequence == 0
            || self.previous.is_some_and(|previous| previous >= sequence)
        {
            return Err(corrupt("text posting identity/order"));
        }
        self.seen += 1;
        self.previous = Some(sequence);
        self.head = Some((sequence, decode_tf(&value)?));
        Ok(())
    }
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

pub(super) fn bm25(
    corpus: Corpus,
    length: u32,
    frequencies: &[u32],
    dfs: &[u64],
) -> Result<f64> {
    if corpus.documents == 0 || corpus.tokens == 0 || frequencies.len() != dfs.len() {
        return Err(corrupt("text corpus statistics cannot score postings"));
    }
    let n = corpus.documents as f64;
    let average = corpus.tokens as f64 / n;
    let mut score = 0.0;
    for (&frequency, &df) in frequencies.iter().zip(dfs) {
        if frequency == 0 || df == 0 || df > corpus.documents || frequency > length {
            return Err(corrupt("text posting/statistics scoring bounds"));
        }
        let tf = f64::from(frequency);
        let idf = (1.0 + (n - df as f64 + 0.5) / (df as f64 + 0.5)).ln();
        let denominator = tf + K1 * (1.0 - B + B * f64::from(length) / average);
        score += idf * (tf * (K1 + 1.0)) / denominator;
    }
    if !score.is_finite() || score <= 0.0 {
        return Err(corrupt("non-finite text score"));
    }
    Ok(score)
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
        if k > indexes::MAX_RESULTS {
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
        let terms: Vec<_> = analysis.terms.keys().map(String::as_str).collect();
        let corpus = read_corpus(self, id)?;
        let mut dfs = Vec::with_capacity(terms.len());
        for term in &terms {
            let df = read_df(self, id, term)?.unwrap_or(0);
            if df > corpus.documents {
                return Err(corrupt("text document frequency exceeds corpus"));
            }
            dfs.push(df);
        }
        let mut examined = 0usize;
        let mut heap = BinaryHeap::with_capacity(k.min(1024));
        match candidates {
            TextCandidates::SortedUnique(ids) => {
                for entity in ids {
                    spend(&mut examined, max_examined, &mut cancelled)?;
                    let Some(norm) = self.store()?.get(&norm_key(id, entity.sequence))? else {
                        continue;
                    };
                    let length = decode_u32(&norm, "text document length")?;
                    let mut frequencies = Vec::new();
                    let mut matched_dfs = Vec::new();
                    for (term, &df) in terms.iter().zip(&dfs) {
                        spend(&mut examined, max_examined, &mut cancelled)?;
                        if let Some(value) =
                            self.store()?.get(&posting_key(id, term, entity.sequence))?
                        {
                            frequencies.push(decode_tf(&value)?);
                            matched_dfs.push(df);
                        }
                    }
                    if frequencies.is_empty()
                        || (matches!(matching, TextMatch::All | TextMatch::Phrase)
                            && frequencies.len() != terms.len())
                    {
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
                    let prefix = posting_prefix(id, term);
                    let mut stream = PostingStream {
                        inner: self.store()?.range(&prefix)?,
                        prefix,
                        term: term_index,
                        expected: df,
                        seen: 0,
                        previous: None,
                        head: None,
                    };
                    stream.advance(&mut examined, max_examined, &mut cancelled)?;
                    streams.push(stream);
                }
                if matches!(matching, TextMatch::All | TextMatch::Phrase) && dfs.contains(&0) {
                    return Ok(Vec::new());
                }
                while let Some(sequence) = streams.iter().filter_map(|s| s.head.map(|h| h.0)).min()
                {
                    let mut frequencies = vec![0; terms.len()];
                    for stream in &mut streams {
                        if stream.head.is_some_and(|head| head.0 == sequence) {
                            frequencies[stream.term] = stream.head.unwrap().1;
                            stream.advance(&mut examined, max_examined, &mut cancelled)?;
                        }
                    }
                    let matched = frequencies.iter().filter(|tf| **tf != 0).count();
                    if matches!(matching, TextMatch::All | TextMatch::Phrase)
                        && matched != terms.len()
                    {
                        continue;
                    }
                    let norm = self
                        .store()?
                        .get(&norm_key(id, sequence))?
                        .ok_or_else(|| corrupt("text posting has no document norm"))?;
                    let length = decode_u32(&norm, "text document length")?;
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
    use kernel::{io::IoMode, store::SyncMode};
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
#[path = "text_fault_tests.rs"]
mod fault_tests;
