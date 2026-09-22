//! Opt-in symmetric-int8 approximate scan with authoritative f32 reranking.
//!
//! This is a bounded linear scan over compact independently rebuildable
//! entries. It is not an ANN graph or HNSW, and it never changes the existing
//! exact-vector family or immutable f32 sidecars.
use crate::collections::{
    corrupt, catalog, invalid, layout_id, ordered, read_ordered, vector_key, CollectionId, Database,
    EntityId, Error, IndexFamily, IndexId, IndexInfo, IndexState, Result, VectorCells,
};
use crate::index::vector::exact::{VectorHit, VectorMetric};
use crate::{Kind, Layout};
use crate::vector_quant::{self, Metric as QuantMetric};
use std::{cmp::Ordering, collections::BinaryHeap};

pub(crate) const QUANTIZED_VECTOR_FEATURE: u64 = 0x20;
pub(crate) const QUANTIZED_VECTOR_ENTRY: u8 = 0x79;
pub(crate) const QUANTIZER_VERSION: u8 = 1;
pub(crate) const OPTIONS: u8 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApproxVectorMethod {
    SymmetricInt8ScanV1,
    /// The Vamana/DiskANN graph walk of `crate::index::vector::graph`: a
    /// greedy best-first search over the `0x7D` adjacency, then the same f32
    /// rerank this family does.
    VamanaGraphV1,
}

#[derive(Clone, Copy)]
pub enum QuantizedVectorCandidates<'a> {
    All,
    /// IDs must belong to the index collection and be strictly sorted.
    SortedUnique(&'a [EntityId]),
    /// The same page-order walk as [`QuantizedVectorCandidates::All`] over
    /// every compact entry of the index, with one test in front of the
    /// scoring: an entry whose SEQUENCE this predicate refuses is stepped
    /// over without its int8 codes being decoded and without a distance being
    /// computed for it.
    ///
    /// What the predicate must be is INDEX-SIDE: a membership test the caller
    /// has already established from some other index's postings. Nothing here
    /// reads a row to decide one, so a predicate that needs the row belongs
    /// on the per-candidate path instead.
    ///
    /// The shortlist is the best `ef` among the entries that PASSED -- the
    /// same `ef` semantics `All` has over the whole index, taken over the
    /// admitted subset.
    Admitted(&'a dyn Fn(u64) -> bool),
}

impl std::fmt::Debug for QuantizedVectorCandidates<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::All => f.write_str("All"),
            Self::SortedUnique(ids) => f.debug_tuple("SortedUnique").field(ids).finish(),
            Self::Admitted(_) => f.write_str("Admitted(..)"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApproxVectorResult {
    /// Authoritative f32 vectors, accumulated as f64 in lane order.
    pub hits: Vec<VectorHit>,
    pub method: ApproxVectorMethod,
    /// Requested maximum approximate shortlist size.
    pub ef: usize,
    /// Quantized index entries or filtered IDs SCORED: the candidates the
    /// approximate stage decoded and ranked. Under
    /// [`QuantizedVectorCandidates::Admitted`] the entries the walk read but
    /// the filters refused are not counted here -- they are charged to the
    /// query's work as candidates and compact reads -- so the number means the
    /// same thing on every plan.
    pub examined: usize,
    /// Authoritative vectors reranked exactly.
    pub reranked: usize,
}

/// One entry on the `ef`-bounded shortlist.
///
/// `entry` is the compact record's own bytes, carried forward from the
/// page-order scan that already read them. The rerank has to compare the
/// persisted entry against a re-encode of the authoritative sidecar (Law 5;
/// see `rerank_sorted`), and it used to fetch those bytes a SECOND time with
/// a point get keyed by the same sequence the scan had just walked past.
/// Holding them instead costs `ef * (14 + dim)` bytes -- 920 bytes at the
/// `ef = 20`, 32-lane shape the 50,000-row battery measures, and bounded by
/// `ef` at every shape -- and removes one root-to-leaf descent per shortlist
/// winner. The comparison is against the same bytes either way: the scan and
/// the rerank read one transaction's committed state, so a second get of the
/// same key can only return what the first one did.
#[derive(Clone, Debug)]
struct ApproxCandidate {
    id: EntityId,
    distance: f64,
    locator: [u8; 6],
    entry: Vec<u8>,
}

impl PartialEq for ApproxCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.distance.to_bits() == other.distance.to_bits() && self.id == other.id
    }
}
impl Eq for ApproxCandidate {}
impl PartialOrd for ApproxCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ApproxCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.id.cmp(&other.id))
    }
}

#[derive(Clone, Copy, Debug)]
struct ExactHit(VectorHit);

impl PartialEq for ExactHit {
    fn eq(&self, other: &Self) -> bool {
        self.0.distance.to_bits() == other.0.distance.to_bits() && self.0.id == other.0.id
    }
}
impl Eq for ExactHit {}
impl PartialOrd for ExactHit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ExactHit {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .distance
            .total_cmp(&other.0.distance)
            .then_with(|| self.0.id.cmp(&other.0.id))
    }
}

pub(crate) fn entry_prefix(id: IndexId) -> Vec<u8> {
    let mut key = vec![QUANTIZED_VECTOR_ENTRY];
    key.extend(ordered(id.0));
    key
}

pub(crate) fn entry_key(id: IndexId, sequence: u64) -> Vec<u8> {
    let mut key = entry_prefix(id);
    key.extend(ordered(sequence));
    key
}

pub(crate) fn dimension(index: &IndexInfo) -> Result<usize> {
    match (&index.family, &index.kind) {
        (IndexFamily::QuantizedVector, Kind::Vector(dimension))
            if (1..=vector_quant::MAX_DIMENSION).contains(dimension) =>
        {
            Ok(*dimension)
        }
        _ => Err(corrupt("quantized vector descriptor family/kind")),
    }
}

fn metric(metric: VectorMetric) -> QuantMetric {
    match metric {
        VectorMetric::Cosine => QuantMetric::Cosine,
        VectorMetric::SquaredL2 => QuantMetric::SquaredL2,
        VectorMetric::NegativeDot => QuantMetric::NegativeDot,
    }
}

fn raw_lanes(bytes: &[u8], dimension: usize) -> Result<Vec<f32>> {
    crate::index::vector::exact::validate_vector(bytes, dimension)?;
    Ok(bytes
        .chunks_exact(4)
        .map(|lane| f32::from_le_bytes(lane.try_into().unwrap()))
        .collect())
}

pub(crate) fn encode_entry(locator: [u8; 6], raw: &[u8], dimension: usize) -> Result<Vec<u8>> {
    let lanes = raw_lanes(raw, dimension)?;
    let quantized = vector_quant::encode(&lanes)
        .map_err(|_| corrupt("valid f32 vector could not be quantized"))?;
    let mut value = Vec::with_capacity(6 + quantized.len());
    value.extend(locator);
    value.extend(quantized);
    Ok(value)
}

pub(crate) fn decode_entry(
    value: &[u8],
    dimension: usize,
) -> Result<([u8; 6], vector_quant::Decoded<'_>)> {
    if value.len() != 6 + 8 + dimension {
        return Err(corrupt("quantized vector entry length"));
    }
    let locator: [u8; 6] = value[..6].try_into().unwrap();
    crate::index::vector::exact::decode_locator(&locator)?;
    let decoded = vector_quant::decode(&value[6..], dimension)
        .map_err(|_| corrupt("quantized vector entry codec"))?;
    Ok((locator, decoded))
}

pub(crate) fn validate_locator(
    db: &Database,
    index: &IndexInfo,
    locator: &[u8; 6],
) -> Result<usize> {
    let expected = dimension(index)?;
    let (layout_id, ordinal) = crate::index::vector::exact::decode_locator(locator)?;
    let layout = db.layout(layout_id)?;
    if layout.id != u64::from(layout_id)
        || !matches!(
            layout.fields.get(ordinal),
            Some((name, Kind::Vector(found))) if name == &index.field && *found == expected
        )
    {
        return Err(corrupt("quantized vector locator field/layout mismatch"));
    }
    Ok(ordinal)
}

fn desired_entry(
    index: &IndexInfo,
    layout: &Layout,
    vectors: &VectorCells,
) -> Result<Option<Vec<u8>>> {
    let expected = dimension(index)?;
    let Some((ordinal, (_, kind))) = layout
        .fields
        .iter()
        .enumerate()
        .find(|(_, (name, _))| name == &index.field)
    else {
        return Err(corrupt("current indexed vector field is absent"));
    };
    if kind != &Kind::Vector(expected) {
        return Err(corrupt("current indexed vector field changed kind"));
    }
    let Some((_, raw)) = vectors.iter().find(|(field, _)| *field == ordinal) else {
        return Ok(None);
    };
    let layout_id = u32::try_from(layout.id).map_err(corrupt)?;
    let locator = crate::index::vector::exact::encode_locator(layout_id, ordinal)?;
    encode_entry(locator, raw, expected).map(Some)
}

/// `fresh` says the row is an INSERT; see [`crate::index::vector::exact::maintain_locator`]
/// for why that makes the existence probe below answerable without a read.
pub(crate) fn maintain_entry(
    db: &mut Database,
    index: &IndexInfo,
    id: EntityId,
    new: Option<(&Layout, &VectorCells)>,
    fresh: bool,
) -> Result<()> {
    let key = entry_key(index.id, id.sequence);
    let desired = new
        .map(|(layout, vectors)| desired_entry(index, layout, vectors))
        .transpose()?
        .flatten();
    if fresh {
        if let Some(value) = desired {
            db.writer()?.put(&key, &value)?;
        }
        return Ok(());
    }
    let existing = db.store()?.get(&key)?;
    match (existing.as_deref(), desired) {
        (Some(old), Some(value)) if old == value => {}
        (_, Some(value)) => db.writer()?.put(&key, &value)?,
        (Some(_), None) => {
            db.writer()?.delete(&key)?;
        }
        (None, None) => {}
    }
    Ok(())
}

/// Derive and validate one entry from an immutable row and its authoritative
/// vector sidecar. Missing/null/historically absent fields emit no entry.
pub(crate) fn build_entry(
    db: &Database,
    index: &IndexInfo,
    id: EntityId,
    row: &[u8],
) -> Result<Option<Vec<u8>>> {
    let expected = dimension(index)?;
    let layout_id = layout_id(row)?;
    let layout = db.layout(layout_id)?;
    let ordinal =
        crate::dense_v3::locate_vector(&layout, row, &index.field, expected).map_err(|error| {
            let message = error.to_string();
            if message.contains("historical vector field") {
                invalid(message)
            } else {
                corrupt(message)
            }
        })?;
    let Some(ordinal) = ordinal else {
        return Ok(None);
    };
    let raw = db
        .store()?
        .get(&vector_key(id, ordinal))?
        .ok_or_else(|| corrupt("indexed vector sidecar is missing"))?;
    let locator = crate::index::vector::exact::encode_locator(layout_id, ordinal)?;
    encode_entry(locator, &raw, expected).map(Some)
}

fn validate_candidates(index: &IndexInfo, candidates: QuantizedVectorCandidates<'_>) -> Result<()> {
    if let QuantizedVectorCandidates::SortedUnique(ids) = candidates {
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

pub(crate) fn exact_score(
    raw: &[u8],
    dimension: usize,
    query: &[f32],
    query_norm: f64,
    metric: VectorMetric,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<f64>> {
    if query.len() != dimension {
        return Err(corrupt("exact vector sidecar length"));
    }
    crate::index::vector::exact::score_f32_distance(raw, query, query_norm, metric, cancelled)
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
            "quantized vector max_examined exceeded",
        )));
    }
    *examined += 1;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn probe_approx(
    db: &Database,
    index: &IndexInfo,
    entity: EntityId,
    value: &[u8],
    dimension: usize,
    query: &[f32],
    metric_value: VectorMetric,
    ef: usize,
    shortlist: &mut BinaryHeap<ApproxCandidate>,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<()> {
    let (locator, decoded) = decode_entry(value, dimension)?;
    validate_locator(db, index, &locator)?;
    let distance = match decoded.score(query, metric(metric_value), cancelled) {
        Ok(Some(distance)) => distance,
        Ok(None) => return Ok(()),
        Err(vector_quant::Error::Cancelled) => return Err(Error::Cancelled),
        Err(vector_quant::Error::Invalid(_)) => {
            return Err(corrupt("quantized vector approximate scoring"));
        }
    };
    shortlist.push(ApproxCandidate {
        id: entity,
        distance,
        locator,
        entry: value.to_vec(),
    });
    if shortlist.len() > ef {
        shortlist.pop();
    }
    Ok(())
}

/// Admit one scored entry to the `ef`-bounded shortlist, or step over it.
///
/// The heap is a MAX heap on `(distance, id)`, so its peek is the worst
/// entry currently held and a candidate no better than that one cannot
/// belong. That test is made FIRST, before anything is built: a 50,000-entry
/// scan admits on the order of `ef * ln(n / ef)` times, so the overwhelming
/// majority of entries never need a candidate at all. When a candidate does
/// displace the worst one, the displaced candidate's entry buffer is REUSED
/// rather than freed and reallocated, which keeps the whole scan's
/// allocations at `ef` instead of one per admission.
fn admit_approx(
    shortlist: &mut BinaryHeap<ApproxCandidate>,
    ef: usize,
    id: EntityId,
    distance: f64,
    entry: &[u8],
) {
    if shortlist.len() < ef {
        let locator: [u8; 6] = entry[..6].try_into().unwrap();
        shortlist.push(ApproxCandidate {
            id,
            distance,
            locator,
            entry: entry.to_vec(),
        });
        return;
    }
    // `ApproxCandidate`'s order is `(distance, id)` and nothing else reads
    // the entry bytes, so the comparison is made on those two alone and the
    // record is assembled only if it wins.
    let worst = shortlist.peek().unwrap();
    let better = distance
        .total_cmp(&worst.distance)
        .then_with(|| id.cmp(&worst.id))
        .is_lt();
    if !better {
        return;
    }
    let mut displaced = shortlist.pop().unwrap();
    displaced.id = id;
    displaced.distance = distance;
    displaced.locator = entry[..6].try_into().unwrap();
    displaced.entry.clear();
    displaced.entry.extend_from_slice(entry);
    shortlist.push(displaced);
}

fn score_compact_entry(
    value: &[u8],
    dimension: usize,
    query: &[f64],
    query_norm: f64,
    metric_value: VectorMetric,
) -> Result<Option<f64>> {
    // Length only: codec canonical-form checks wait until rerank, so the
    // 50K-entry scan is a tight int8 loop over already-trusted pages. The
    // locator is not copied here either: it is six bytes of a record the
    // caller still holds, and only an ADMITTED candidate ever needs them.
    if value.len() != 6 + 8 + dimension {
        return Err(corrupt("quantized vector entry length"));
    }
    let scale = f64::from_le_bytes(value[6..14].try_into().unwrap());
    let codes = &value[14..];
    Ok(vector_quant::score_i8_hot(
        scale,
        codes,
        query,
        query_norm,
        metric(metric_value),
    ))
}

fn rerank_sorted(
    db: &Database,
    index: &IndexInfo,
    mut shortlist: Vec<ApproxCandidate>,
    query: &[f32],
    query_norm: f64,
    metric: VectorMetric,
    k: usize,
    after: Option<crate::index::vector::exact::VectorAfter>,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Vec<VectorHit>> {
    shortlist.sort_unstable_by(|left, right| {
        left.id
            .sequence
            .cmp(&right.id.sequence)
            .then_with(|| left.locator.cmp(&right.locator))
            .then_with(|| left.id.cmp(&right.id))
    });
    let dimension = dimension(index)?;
    let store = db.store()?;
    // WHY NOT A FORWARD CURSOR. The shortlist is sorted by sequence, so its
    // sidecar keys ascend, and dragging one cursor across them instead of
    // descending per winner looks like the obvious saving. It is a
    // PESSIMISATION at this shape and the number says why: `ef` winners are
    // scattered over the whole corpus, so a cursor dragged from one to the
    // next steps through every sidecar between them -- 50,000 records of
    // 128 bytes at the battery's shape -- and those 6.4 MB evict the 2.3 MB
    // of compact entries the NEXT query's scan wants out of an 8 MiB pool.
    // Measured at ef=20 over 50,000 rows: the rerank's sidecar reads went
    // from 1.33 ms to 185 ms per fifty queries and the candidate scan behind
    // them from 128 ms to 217 ms. `ef` point gets of `ef` scattered keys is
    // the right shape; page order only pays when the candidates ARE the
    // pages, which is what `scan_exact_all` does and this is not.
    let mut exact = BinaryHeap::with_capacity(k.min(1024));
    for candidate in shortlist {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        // The shortlist is ef entries, not the corpus, so the locator each
        // winner carries is validated against its layout, field and dimension
        // before it names a sidecar: a locator pointing at ANOTHER vector
        // field of the same dimension would otherwise be scored and returned
        // as a hit. The sacrifice below is the re-encode, not this.
        let ordinal = validate_locator(db, index, &candidate.locator)?;
        let raw = store
            .get(&vector_key(candidate.id, ordinal))?
            .ok_or_else(|| corrupt("quantized vector locator points to missing sidecar"))?;
        // The compact entry is a DERIVED copy of that sidecar, so the pair has
        // exactly one agreeing form and the ef winners are where a query can
        // still afford to check it. The page-order scan reads an entry's
        // LENGTH and nothing else -- canonical scale, canonical lanes and the
        // locator the entry carries are all unchecked there -- so without this
        // re-encode a compact entry whose scale, lanes or locator were
        // rewritten is scored as an approximation of a vector it no longer
        // approximates, and the damage reaches the caller as an answer instead
        // of as `Corrupt` (Law 5). COST: one `get` and one quantize per
        // shortlist winner, bounded by `ef` and never by the corpus.
        if encode_entry(candidate.locator, &raw, dimension)? != candidate.entry {
            return Err(corrupt(
                "quantized vector entry differs from authoritative sidecar",
            ));
        }
        let scored = exact_score(&raw, dimension, query, query_norm, metric, cancelled)?;
        let Some(distance) = scored
        else {
            continue;
        };
        // Paging is over the RERANKED order, so the cursor is applied to the
        // exact distance and not to the approximate one the shortlist was
        // built from. `ef` still bounds the whole result set: what this drops
        // is only what earlier pages already returned.
        if after.is_some_and(|after| !after.admits(distance, candidate.id)) {
            continue;
        }
        exact.push(ExactHit(VectorHit {
            id: candidate.id,
            distance,
        }));
        if exact.len() > k {
            exact.pop();
        }
    }
    let mut hits: Vec<_> = exact.into_iter().map(|hit| hit.0).collect();
    hits.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(hits)
}

impl Database {
    pub fn create_quantized_vector_index(
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
        if !matches!(kind, Kind::Vector(d) if (1..=vector_quant::MAX_DIMENSION).contains(&d)) {
            return Err(invalid("quantized vector index requires a vector field"));
        }
        self.create_index(
            collection,
            name,
            field,
            kind,
            false,
            IndexFamily::QuantizedVector,
            QUANTIZED_VECTOR_FEATURE,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn query_quantized_vector(
        &self,
        id: IndexId,
        query: &[f32],
        metric: VectorMetric,
        k: usize,
        ef: usize,
        candidates: QuantizedVectorCandidates<'_>,
        max_examined: usize,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<ApproxVectorResult> {
        let mut progress = crate::index::vector::exact::cancel_only(&mut cancelled);
        self.scan_quantized(
            id,
            query,
            metric,
            k,
            ef,
            candidates,
            None,
            max_examined,
            usize::MAX,
            &mut progress,
        )
    }

    /// The paged form of [`Database::query_quantized_vector`]: the same walk,
    /// with the page cursor applied to the RERANKED order and a progress hook
    /// the caller charges its budget through while the scan is still running.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn scan_quantized(
        &self,
        id: IndexId,
        query: &[f32],
        metric: VectorMetric,
        k: usize,
        ef: usize,
        candidates: QuantizedVectorCandidates<'_>,
        after: Option<crate::index::vector::exact::VectorAfter>,
        max_examined: usize,
        max_scored: usize,
        progress: crate::index::vector::exact::ScanProgress<'_>,
    ) -> Result<ApproxVectorResult> {
        if k == 0 || k > ef || ef > catalog::MAX_RESULTS {
            return Err(invalid(
                "quantized vector search requires 1 <= k <= ef <= 65536",
            ));
        }
        let index = self.index_info(id)?;
        if index.family != IndexFamily::QuantizedVector {
            return Err(invalid("index is not a quantized vector index"));
        }
        if index.state != IndexState::Ready {
            return Err(invalid("index is not ready"));
        }
        let dimension = dimension(&index)?;
        if query.len() != dimension || query.iter().any(|lane| !lane.is_finite()) {
            return Err(invalid(
                "query vector has wrong dimension or non-finite lane",
            ));
        }
        let query_norm = query.iter().fold(0.0f64, |sum, lane| {
            sum + f64::from(*lane) * f64::from(*lane)
        });
        if metric == VectorMetric::Cosine && query_norm == 0.0 {
            return Err(invalid("cosine query vector must have nonzero norm"));
        }
        validate_candidates(&index, candidates)?;
        progress(crate::index::vector::exact::ScanStep::Scored(0))?;

        let mut examined = 0usize;
        // Entries the approximate stage actually scored. On the `All` walk
        // and the probe list this equals `examined`; under an admission
        // predicate the refused entries are read but never scored.
        let mut scored_total = 0usize;
        let mut shortlist = BinaryHeap::with_capacity(ef.min(1024));
        let (query_wide, wide_norm) = vector_quant::widen_query(query)
            .map_err(|_| invalid("query vector has wrong dimension or non-finite lane"))?;

        match candidates {
            QuantizedVectorCandidates::All | QuantizedVectorCandidates::Admitted(_) => {
                let admit = match candidates {
                    QuantizedVectorCandidates::Admitted(admit) => Some(admit),
                    _ => None,
                };
                let prefix = entry_prefix(id);
                let mut failure = None;
                // Compact entries READ, and of those the ones decoded and
                // SCORED, since the last progress call. The two are the same
                // records when every entry is a candidate -- `All` reports
                // only the scored count, exactly as it always did -- and an
                // admission predicate is what makes them differ: a refused
                // entry was read but never scored, and each count is charged
                // in its own currency.
                let mut pending_read = 0u64;
                let mut pending_scored = 0u64;
                // Entries read since the last progress call, whether scored
                // or refused, so the poll cadence stays one call per
                // `SCAN_STEP` records of actual walking.
                let mut since_poll = 0u64;
                // The lane budget bounds the entries SCORED, not the entries
                // read, so an admission predicate that refuses most of the
                // index cannot let the scan score past what the lanes allow
                // before the charge that names them is made.
                self.store()?.range(&prefix)?.for_each_ref(|key, value| {
                    if !key.starts_with(&prefix) {
                        return false;
                    }
                    if let Err(e) = (|| -> Result<()> {
                        if since_poll == crate::index::vector::exact::SCAN_STEP {
                            since_poll = 0;
                            if pending_read > 0 {
                                progress(crate::index::vector::exact::ScanStep::Locators(
                                    std::mem::take(&mut pending_read),
                                ))?;
                            }
                            progress(crate::index::vector::exact::ScanStep::Scored(std::mem::take(
                                &mut pending_scored,
                            )))?;
                        }
                        if examined == max_examined {
                            // Charge what is outstanding before reporting the
                            // limit: when the limit came from a budget, that
                            // charge names the resource that ran out.
                            if pending_read > 0 {
                                progress(crate::index::vector::exact::ScanStep::Locators(
                                    std::mem::take(&mut pending_read),
                                ))?;
                            }
                            progress(crate::index::vector::exact::ScanStep::Scored(std::mem::take(
                                &mut pending_scored,
                            )))?;
                            return Err(Error::Kernel(kernel::Error::ResourceLimit(
                                "quantized vector max_examined exceeded",
                            )));
                        }
                        examined += 1;
                        since_poll += 1;
                        let mut at = prefix.len();
                        let sequence = read_ordered(key, &mut at)?;
                        if at != key.len() || sequence == 0 {
                            return Err(corrupt("quantized vector entry key"));
                        }
                        match admit {
                            Some(admit) => {
                                pending_read += 1;
                                if !admit(sequence) {
                                    return Ok(());
                                }
                            }
                            None => {}
                        }
                        if scored_total == max_scored {
                            if pending_read > 0 {
                                progress(crate::index::vector::exact::ScanStep::Locators(
                                    std::mem::take(&mut pending_read),
                                ))?;
                            }
                            progress(crate::index::vector::exact::ScanStep::Scored(std::mem::take(
                                &mut pending_scored,
                            )))?;
                            return Err(Error::Kernel(kernel::Error::ResourceLimit(
                                "quantized vector max_scored exceeded",
                            )));
                        }
                        scored_total += 1;
                        pending_scored += 1;
                        let Some(distance) = score_compact_entry(
                            value,
                            dimension,
                            &query_wide,
                            wide_norm,
                            metric,
                        )?
                        else {
                            return Ok(());
                        };
                        admit_approx(
                            &mut shortlist,
                            ef,
                            EntityId {
                                collection: index.collection,
                                sequence,
                            },
                            distance,
                            value,
                        );
                        Ok(())
                    })() {
                        failure = Some(e);
                        return false;
                    }
                    true
                })?;
                if let Some(e) = failure {
                    return Err(e);
                }
                if pending_read > 0 {
                    progress(crate::index::vector::exact::ScanStep::Locators(pending_read))?;
                }
                progress(crate::index::vector::exact::ScanStep::Scored(pending_scored))?;
            }
            QuantizedVectorCandidates::SortedUnique(ids) => {
                // A filtered probe list is bounded by the caller's own
                // candidate slice and is never the paged driver's path, so
                // this poll charges nothing and only asks whether to stop.
                let mut cancelled =
                    || progress(crate::index::vector::exact::ScanStep::Scored(0)).is_err();
                for entity in ids {
                    spend(&mut examined, max_examined, &mut cancelled)?;
                    if let Some(value) = self.store()?.get(&entry_key(id, entity.sequence))? {
                        probe_approx(
                            self,
                            &index,
                            *entity,
                            &value,
                            dimension,
                            query,
                            metric,
                            ef,
                            &mut shortlist,
                            &mut cancelled,
                        )?;
                    }
                }
            }
        }

        let reranked = shortlist.len();
        // The rerank is `ef` records at most, so its poll charges nothing:
        // the caller has already been charged for the shortlist, and the
        // sidecars it reads are counted by `reranked`.
        let mut cancelled = || progress(crate::index::vector::exact::ScanStep::Scored(0)).is_err();
        let hits = rerank_sorted(
            self,
            &index,
            shortlist.into_vec(),
            query,
            query_norm,
            metric,
            k,
            after,
            &mut cancelled,
        )?;
        Ok(ApproxVectorResult {
            hits,
            method: ApproxVectorMethod::SymmetricInt8ScanV1,
            ef,
            examined: match candidates {
                QuantizedVectorCandidates::All | QuantizedVectorCandidates::Admitted(_) => {
                    scored_total
                }
                QuantizedVectorCandidates::SortedUnique(_) => examined,
            },
            reranked,
        })
    }
}

#[cfg(test)]
mod fault_tests {
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

    fn rows(db: &Database) -> Vec<(Vec<u8>, Vec<u8>)> {
        db.store()
            .unwrap()
            .range(&[])
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    fn ids(db: &Database, index: IndexId) -> Vec<EntityId> {
        db.query_quantized_vector(
            index,
            &[0.0, 0.0],
            VectorMetric::SquaredL2,
            16,
            16,
            QuantizedVectorCandidates::All,
            16,
            || false,
        )
        .unwrap()
        .hits
        .into_iter()
        .map(|hit| hit.id)
        .collect()
    }

    #[derive(Clone, Copy, Debug)]
    enum Operation {
        Create,
        Insert,
        Update,
        Delete,
        BuildProgress,
        BuildPublish,
        DropBegin,
        DropProgress,
        DropFinish,
    }

    struct Fixture {
        collection: CollectionId,
        a: EntityId,
        b: EntityId,
        index: Option<IndexId>,
    }

    fn fixture(db: &mut Database, operation: Operation) -> Fixture {
        let collection = db
            .create_collection(
                "rows",
                vec![("v".into(), Kind::Vector(2))],
                Default::default(),
            )
            .unwrap();
        let a = db.put(collection, "a", &json!({"v":[1.0,0.0]})).unwrap();
        let b = db.put(collection, "b", &json!({"v":[2.0,0.0]})).unwrap();
        db.commit().unwrap();
        if matches!(operation, Operation::Create) {
            return Fixture {
                collection,
                a,
                b,
                index: None,
            };
        }
        let index = db
            .create_quantized_vector_index(collection, "v_int8", "v")
            .unwrap();
        if !matches!(
            operation,
            Operation::BuildProgress | Operation::BuildPublish
        ) {
            assert!(db.build_index_step(index, 16).unwrap());
        }
        db.commit().unwrap();
        Fixture {
            collection,
            a,
            b,
            index: Some(index),
        }
    }

    fn apply(operation: Operation, db: &mut Database, fixture: &Fixture) -> Result<()> {
        match operation {
            Operation::Create => db
                .create_quantized_vector_index(fixture.collection, "v_int8", "v")
                .map(|_| ()),
            Operation::Insert => db
                .put(fixture.collection, "new", &json!({"v":[3.0,0.0]}))
                .map(|_| ()),
            Operation::Update => db
                .update(fixture.collection, "a", &json!({"v":[3.0,0.0]}))
                .map(|_| ()),
            Operation::Delete => db
                .delete(fixture.collection, "a")
                .map(|deleted| assert!(deleted)),
            Operation::BuildProgress => db
                .build_index_step(fixture.index.unwrap(), 1)
                .map(|done| assert!(!done)),
            Operation::BuildPublish => db
                .build_index_step(fixture.index.unwrap(), 16)
                .map(|done| assert!(done)),
            Operation::DropBegin => db.begin_drop_index(fixture.index.unwrap()),
            Operation::DropProgress => db
                .drop_index_step(fixture.index.unwrap(), 1)
                .map(|done| assert!(!done)),
            Operation::DropFinish => db
                .drop_index_step(fixture.index.unwrap(), 16)
                .map(|done| assert!(done)),
        }
    }

    fn committed_semantics(db: &Database, operation: Operation, fixture: &Fixture) {
        match operation {
            Operation::Create => {
                let indexes = db.list_indexes(fixture.collection).unwrap();
                assert_eq!(indexes.len(), 1);
                assert_eq!(indexes[0].family, IndexFamily::QuantizedVector);
            }
            Operation::Insert => assert_eq!(ids(db, fixture.index.unwrap()).len(), 3),
            Operation::Update => {
                assert_eq!(ids(db, fixture.index.unwrap()), vec![fixture.b, fixture.a]);
            }
            Operation::Delete => {
                assert!(db.get_by_id(fixture.a).unwrap().is_none());
                assert_eq!(ids(db, fixture.index.unwrap()), vec![fixture.b]);
            }
            Operation::BuildProgress => assert!(matches!(
                db.index_info(fixture.index.unwrap()).unwrap().state,
                IndexState::Building { after } if after == fixture.a.sequence
            )),
            Operation::BuildPublish => {
                assert_eq!(
                    db.index_info(fixture.index.unwrap()).unwrap().state,
                    IndexState::Ready
                );
                assert_eq!(ids(db, fixture.index.unwrap()), vec![fixture.a, fixture.b]);
            }
            Operation::DropBegin | Operation::DropProgress => assert_eq!(
                db.index_info(fixture.index.unwrap()).unwrap().state,
                IndexState::Dropping
            ),
            Operation::DropFinish => assert!(matches!(
                db.index_info(fixture.index.unwrap()),
                Err(Error::NotFound("index"))
            )),
        }
    }

    #[test]
    fn quantized_catalog_crud_build_and_drop_are_atomic_at_every_key_write() {
        for operation in [
            Operation::Create,
            Operation::Insert,
            Operation::Update,
            Operation::Delete,
            Operation::BuildProgress,
            Operation::BuildPublish,
            Operation::DropBegin,
            Operation::DropProgress,
            Operation::DropFinish,
        ] {
            let mut reached_success = false;
            for fail_at in 0..96 {
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("db");
                let mut db = Database::create(&path, cfg()).unwrap();
                let fixture = fixture(&mut db, operation);
                if matches!(operation, Operation::DropProgress | Operation::DropFinish) {
                    db.begin_drop_index(fixture.index.unwrap()).unwrap();
                    db.commit().unwrap();
                }
                let pinned = Database::open_snapshot(&path, cfg()).unwrap();
                let pinned_rows = rows(&pinned);
                let baseline = rows(&db);
                db.store().unwrap().arm_write_fault(fail_at);
                let result = apply(operation, &mut db, &fixture);
                if result.is_ok() {
                    assert!(fail_at > 0, "{operation:?} performed no writes");
                    assert_eq!(rows(&pinned), pinned_rows);
                    drop(db);
                    assert_eq!(rows(&Database::open(&path, cfg()).unwrap()), baseline);
                    reached_success = true;
                    break;
                }

                assert!(db.commit().is_err(), "{operation:?}/{fail_at}");
                assert_eq!(rows(&pinned), pinned_rows);
                assert_eq!(
                    rows(&Database::open_snapshot(&path, cfg()).unwrap()),
                    baseline,
                    "partial publication at {operation:?}/{fail_at}"
                );
                db.rollback().unwrap();
                assert_eq!(rows(&db), baseline);
                apply(operation, &mut db, &fixture).unwrap();
                db.commit().unwrap();
                committed_semantics(&db, operation, &fixture);
                let committed = rows(&db);
                drop(db);
                let reopened = Database::open(&path, cfg()).unwrap();
                assert_eq!(rows(&reopened), committed);
                committed_semantics(&reopened, operation, &fixture);
            }
            assert!(reached_success, "{operation:?} exceeded 96 key writes");
        }
    }
}
