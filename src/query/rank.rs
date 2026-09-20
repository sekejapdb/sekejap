//! Ranking: the rank key a candidate carries, the winners heap, and the
//! scores (text BM25, exact and approximate vector, distance) a rank key is
//! built from. See docs/QL_CONTRACT.md,
//! "Contract and public shape".
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RankValue {
    Entity,
    Scalar(Vec<u8>),
    Score(u64),
    /// One spatial posting's Hilbert cell. Four bytes in the key, and the
    /// value they hold rather than the bytes: a spatial answer can be
    /// millions of rows, and a `Vec` per row to carry a `u32` is an
    /// allocation per row.
    Cell(u32),
    /// One geometry posting's `(level, cell)` — the first posting that
    /// admitted the entity. Level precedes cell, matching the on-disk key.
    GeomCell { level: u8, cell: u32 },
    /// One mapping entry's external-key bytes.
    Key(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RankKey {
    pub(super) value: RankValue,
    pub(super) id: EntityId,
}

/// How much of the query's rank order the driver's own walk already provides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RankWalk {
    /// None of it: the walk order and the rank order are unrelated.
    No,
    /// All of it, key for key. A page may stop the moment its heap is full,
    /// and the next page may resume at this page's last key.
    Exact,
    /// The VALUE half only, and monotonically. A descending scalar order is
    /// ranked (value descending, entity id ASCENDING) while the reverse walk
    /// hands a tie group over id descending, so a full heap is not yet an
    /// answer: the walk has to finish the boundary value's tie group before
    /// anything later can be ruled out. It still stops long before the end,
    /// and it still resumes rather than restarting.
    ByValue,
}

/// One ranked candidate the page is still holding, and -- when the page is
/// going to project fields out of it -- the primary row the walk already read.
///
/// Carrying the bytes is what stops a projected scan reading every row twice:
/// once to walk it and once to fetch the winner back. Bounded by the heap's
/// capacity, so it is the page's own bound, not the collection's.
pub(super) struct HeapEntry {
    pub(super) key: RankKey,
    pub(super) descending: bool,
    /// Boxed on purpose. Every query pays this struct's size on every heap
    /// push, pop and sift -- a key-only scan of 20,000 rows sifts an 8,192
    /// entry heap -- so the pointer stays here and the row lives off to one
    /// side. Inlining `RowData` here cost a measured 25-30% on `scan/full_keys`
    /// and `filter/eq_indexed_many`, which carry no rows at all.
    pub(super) row: Option<Box<RowData>>,
}

/// The carried row plays no part in the ordering, so equality is the rank
/// comparison and nothing else.
impl Eq for HeapEntry {}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_rank(&self.key, &other.key, self.descending)
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The candidates one page is still holding.
///
/// A page needs a HEAP only once it is full: until then every candidate is
/// kept, so the ordering the heap maintains on the way in is thrown away by
/// the final sort. Pushing 8,192 entries into a max-heap in ASCENDING order --
/// which is exactly what a key-only scan does -- is the heap's worst case, one
/// full sift to the root per row; measured at 61.9 ns/row against 3.6 ns/row
/// for pushing the same entries onto a `Vec`.
///
/// So the page fills a `Vec`, and becomes a heap in one `O(n)` heapify the
/// first time it has to name its WORST held entry -- which can only happen
/// once it is full and a candidate has to displace something. A page whose
/// answer fits (every non-ranked case here, and every ranked one under 8,192
/// hits) never heapifies at all.
///
/// The `Vec` also reserves nothing until its first entry. `capacity` is the
/// page size, not the answer size, so an EMPTY answer used to allocate 8,193
/// entries -- 459 KB -- and drop them untouched, which was most of what
/// `filter/eq_no_match` cost.
pub(super) enum Winners {
    Filling(Vec<HeapEntry>),
    Full(BinaryHeap<HeapEntry>),
}

impl Winners {
    pub(super) fn new() -> Self {
        Self::Filling(Vec::new())
    }

    pub(super) fn len(&self) -> usize {
        match self {
            Self::Filling(kept) => kept.len(),
            Self::Full(heap) => heap.len(),
        }
    }

    /// Keep one more. Only ever called with fewer than `capacity` held, or
    /// straight after [`Winners::pop_worst`].
    #[inline]
    pub(super) fn push(&mut self, capacity: usize, entry: HeapEntry) {
        match self {
            Self::Filling(kept) => {
                if kept.capacity() == 0 {
                    kept.reserve_exact(capacity);
                }
                kept.push(entry);
            }
            Self::Full(heap) => heap.push(entry),
        }
    }

    /// The worst entry held, which is the one a better candidate displaces.
    /// Asking the question is what turns the page into a heap.
    pub(super) fn worst(&mut self) -> Option<&HeapEntry> {
        if let Self::Filling(kept) = self {
            *self = Self::Full(BinaryHeap::from(std::mem::take(kept)));
        }
        match self {
            Self::Full(heap) => heap.peek(),
            Self::Filling(_) => unreachable!("the page was just heapified"),
        }
    }

    pub(super) fn pop_worst(&mut self) {
        if let Self::Full(heap) = self {
            heap.pop();
        }
    }

    /// True once the page has had to order itself, so its entries are in heap
    /// order and not in the order the walk handed them over.
    pub(super) fn heaped(&self) -> bool {
        matches!(self, Self::Full(_))
    }

    pub(super) fn into_vec(self) -> Vec<HeapEntry> {
        match self {
            Self::Filling(kept) => kept,
            Self::Full(heap) => heap.into_vec(),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ApproxHeapEntry {
    pub(super) distance: f64,
    pub(super) id: EntityId,
    pub(super) locator: [u8; 6],
}

impl PartialEq for ApproxHeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.distance.to_bits() == other.distance.to_bits() && self.id == other.id
    }
}

impl Eq for ApproxHeapEntry {}

impl PartialOrd for ApproxHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ApproxHeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.id.cmp(&other.id))
    }
}

#[inline(always)]
pub(super) fn compare_rank(a: &RankKey, b: &RankKey, descending: bool) -> Ordering {
    compare_rank_value(&a.value, &b.value, descending).then_with(|| a.id.cmp(&b.id))
}

/// The VALUE half of the rank comparison, without the entity-id tie-break.
/// A descending walk is monotone in this and not in the whole key, so this is
/// what decides whether anything still ahead of the cursor can outrank what
/// the page already holds.
#[inline(always)]
pub(super) fn compare_rank_value(a: &RankValue, b: &RankValue, descending: bool) -> Ordering {
    match (a, b) {
        (RankValue::Entity, RankValue::Entity) => Ordering::Equal,
        (RankValue::Scalar(a), RankValue::Scalar(b)) => {
            if descending {
                b.cmp(a)
            } else {
                a.cmp(b)
            }
        }
        (RankValue::Cell(a), RankValue::Cell(b)) => {
            if descending {
                b.cmp(a)
            } else {
                a.cmp(b)
            }
        }
        (
            RankValue::GeomCell {
                level: la,
                cell: ca,
            },
            RankValue::GeomCell {
                level: lb,
                cell: cb,
            },
        ) => {
            let a = (*la, *ca);
            let b = (*lb, *cb);
            if descending {
                b.cmp(&a)
            } else {
                a.cmp(&b)
            }
        }
        (RankValue::Key(a), RankValue::Key(b)) => {
            if descending {
                b.cmp(a)
            } else {
                a.cmp(b)
            }
        }
        (RankValue::Score(a), RankValue::Score(b)) => {
            let a = f64::from_bits(*a);
            let b = f64::from_bits(*b);
            match (a.is_nan(), b.is_nan()) {
                (true, true) => Ordering::Equal,
                // NaN is last under both directions: it is always worse than
                // a number, so the heap peeks it first to displace and a
                // page sort puts it after every finite score.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    if descending {
                        b.total_cmp(&a)
                    } else {
                        a.total_cmp(&b)
                    }
                }
            }
        }
        _ => unreachable!("prepared order creates one rank-key kind"),
    }
}

pub(super) fn text_score<'a, C: FnMut() -> bool>(
    db: &'a Database,
    rows: &mut PrimaryRows<'a>,
    prepared: &PreparedText,
    id: EntityId,
    driven: Option<&TextFrequencies>,
    row: &mut Option<RowData>,
    encoded: &mut Option<Vec<u8>>,
    scratch: &mut RowScratch,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<f64>> {
    if prepared.terms.is_empty() {
        return Ok(None);
    }
    meter.charge(WorkResource::TextPostings, 1)?;
    // Head row first, then the packed `0x7B` block -- the same lookup, and the
    // same decoding of the EMPTY head value, that every other norm reader
    // performs. A text index built after its corpus writes NO head row at all,
    // so reading `norm_key` alone drops every document it scores.
    let Some(length) =
        crate::index::text::read_norm_cached(db, prepared.info.id, id.sequence, &mut scratch.norms)?
            .length
    else {
        return Ok(None);
    };
    // Did the candidate driver already decode these very frequencies? It did
    // whenever the driver is the merge over THIS query's terms: `driven` on
    // the prepared query names that site, and the candidate carries the site
    // it came from. Anything else -- a scalar or spatial driver, or a text
    // driver over different terms -- reads its own, through the per-query
    // term window so that each segment is decoded once rather than once per
    // document.
    let merged = driven.filter(|frequencies| {
        prepared.driven == Some(frequencies.source)
            && usize::from(frequencies.len) == prepared.terms.len()
            && usize::from(frequencies.len) <= INLINE_TEXT_TERMS
    });
    // The page's own buffers, not this document's: a pair of heap vectors per
    // scored document would be one allocation per document per query, and a
    // pair of 64-slot stack arrays -- which is what this was -- is 768 bytes
    // zeroed per document to hold, usually, one term.
    if prepared.terms.len() > MAX_TEXT_TERMS {
        return Err(corrupt_query("prepared text query exceeds its term bound"));
    }
    scratch.text.frequencies.clear();
    scratch.text.idfs.clear();
    let segments_on = crate::index::text::segments_enabled(db);
    for (position, (term, &idf)) in prepared.terms.iter().zip(&prepared.idfs).enumerate() {
        let frequency = match merged {
            // Already charged to `TextPostings` by the merge that decoded it.
            Some(merged) => {
                let frequency = merged.slots[position];
                (frequency != 0).then_some(frequency)
            }
            None => {
                meter.charge(WorkResource::TextPostings, 1)?;
                crate::index::text::point_posting(
                    db,
                    prepared.info.id,
                    term,
                    id.sequence,
                    segments_on,
                    &mut scratch.norms,
                )?
            }
        };
        if let Some(frequency) = frequency {
            scratch.text.frequencies.push(frequency);
            scratch.text.idfs.push(idf);
        }
    }
    let frequencies = scratch.text.frequencies.as_slice();
    let idfs = scratch.text.idfs.as_slice();
    let matched = frequencies.len();
    if matched == 0
        || (matches!(prepared.matching, TextMatch::All | TextMatch::Phrase)
            && matched != prepared.terms.len())
    {
        return Ok(None);
    }
    if let Some(phrase) = prepared.phrase.as_deref() {
        // Through the PAGE's reader, not a fresh point-get. The text merge
        // hands documents over in ascending sequence, so the rows a phrase
        // re-reads ascend with it and one forward cursor serves the page: this
        // was 8.0 pager accesses per candidate on `text/match_phrase`, most of
        // them a root-to-leaf descent for a row the cursor was standing near.
        ensure_row_seq(db, rows, id, row, encoded, meter)?;
        let held = row.as_ref().expect("the row was just ensured");
        // Borrowed out of the row, not copied out of it: an owned `String` per
        // candidate was an allocation and a memcpy of the whole field for text
        // that is scanned once and dropped.
        let text = match dense_v3::read_text_field_in(&held.layout, &held.bytes, &prepared.info.field)
            .map_err(|error| corrupt_query(format!("dense-v3 row: {error}")))?
        {
            dense_v3::TextFieldRef::Text(text) => text,
            dense_v3::TextFieldRef::Missing | dense_v3::TextFieldRef::Null => {
                return Err(corrupt_query(
                    "text posting points to absent authoritative primary text",
                ));
            }
            // A text index over a field this layout does not declare as Text:
            // the value is in the extras object, so it is read the general way
            // and the same three cases decide.
            dense_v3::TextFieldRef::Elsewhere => {
                match selected_field(held, &prepared.info.field)? {
                    dense_v3::FieldValue::Inline(Value::String(_)) => {
                        return Err(corrupt_query(
                            "text index field is not a declared text column",
                        ));
                    }
                    dense_v3::FieldValue::Missing | dense_v3::FieldValue::Null => {
                        return Err(corrupt_query(
                            "text posting points to absent authoritative primary text",
                        ));
                    }
                    _ => {
                        return Err(corrupt_query(
                            "text posting points to non-text authoritative primary field",
                        ));
                    }
                }
            }
        };
        let scanned = crate::text_analyzer::scan_phrase_document(
            text,
            phrase,
            &prepared.phrase_prefix,
            &prepared.terms,
            &mut scratch.seen,
            &mut scratch.token,
            |event| match event {
                crate::text_analyzer::PhraseScanEvent::Poll => meter.check_cancelled(),
                crate::text_analyzer::PhraseScanEvent::Token => {
                    meter.charge(WorkResource::TextTokens, 1)
                }
            },
        );
        let (scanned_length, matched) = match scanned {
            Ok(result) => result,
            Err(crate::text_analyzer::PhraseScanError::Analysis(error)) => {
                return Err(corrupt_query(format!(
                    "authoritative text violates analyzer bounds: {error}"
                )));
            }
            Err(crate::text_analyzer::PhraseScanError::Callback(error)) => return Err(error),
        };
        if scanned_length != length {
            return Err(corrupt_query(
                "text norm disagrees with authoritative primary text",
            ));
        }
        // The same cross-check as before, positionally: `frequencies` holds
        // the MATCHED terms in prepared order, and a phrase requires every
        // prepared term to have matched, so the two run in step.
        if frequencies.len() != prepared.terms.len() || scratch.seen.len() != prepared.terms.len() {
            return Err(corrupt_query(
                "text posting frequency disagrees with authoritative primary text",
            ));
        }
        for (at, frequency) in frequencies.iter().enumerate() {
            if scratch.seen[at] != *frequency {
                return Err(corrupt_query(
                    "text posting frequency disagrees with authoritative primary text",
                ));
            }
        }
        if !matched {
            return Ok(None);
        }
    }
    crate::index::text::bm25_scored(prepared.weights, length, frequencies, idfs)
        .map(Some)
        .map_err(QueryError::from)
}

pub(super) fn vector_score<C: FnMut() -> bool>(
    db: &Database,
    candidate: &Candidate,
    info: &IndexInfo,
    query: &[f32],
    query_norm: f64,
    metric: VectorMetric,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<f64>> {
    let locator = candidate.vector(info.id).map(<[u8]>::to_vec);
    let locator = match locator {
        Some(locator) => locator,
        None => {
            meter.charge(WorkResource::VectorLocators, 1)?;
            let Some(locator) = db.store()?.get(&crate::index::vector::exact::locator_key(
                info.id,
                candidate.id.sequence,
            ))?
            else {
                return Ok(None);
            };
            locator
        }
    };
    meter.charge(WorkResource::VectorSidecars, 1)?;
    meter.charge(
        WorkResource::VectorLanes,
        u64::try_from(crate::index::vector::exact::dimension(info)?).map_err(invalid_query)?,
    )?;
    db.score_locator_cancelled(
        info,
        candidate.id,
        &locator,
        query,
        query_norm,
        metric,
        &mut || (meter.cancelled)(),
    )
    .map(|hit| hit.map(|hit| hit.distance))
    .map_err(QueryError::from)
}

fn quantized_metric(metric: VectorMetric) -> crate::vector_quant::Metric {
    match metric {
        VectorMetric::Cosine => crate::vector_quant::Metric::Cosine,
        VectorMetric::SquaredL2 => crate::vector_quant::Metric::SquaredL2,
        VectorMetric::NegativeDot => crate::vector_quant::Metric::NegativeDot,
    }
}

pub(super) fn approximate_vector_score<C: FnMut() -> bool>(
    db: &Database,
    candidate: &Candidate,
    info: &IndexInfo,
    query: &[f32],
    metric: VectorMetric,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<(f64, [u8; 6])>> {
    let value = candidate.quantized(info.id).map(<[u8]>::to_vec);
    let value = match value {
        Some(value) => value,
        None => {
            meter.charge(WorkResource::VectorLocators, 1)?;
            let Some(value) = db
                .store()?
                .get(&crate::index::vector::quantized::entry_key(
                    info.id,
                    candidate.id.sequence,
                ))?
            else {
                return Ok(None);
            };
            value
        }
    };
    let dimension = crate::index::vector::quantized::dimension(info)?;
    meter.charge(
        WorkResource::VectorLanes,
        u64::try_from(dimension).map_err(invalid_query)?,
    )?;
    let (locator, decoded) = crate::index::vector::quantized::decode_entry(&value, dimension)?;
    crate::index::vector::quantized::validate_locator(db, info, &locator)?;
    let score = decoded.score(query, quantized_metric(metric), || (meter.cancelled)());
    match score {
        Ok(Some(distance)) => Ok(Some((distance, locator))),
        Ok(None) => Ok(None),
        Err(crate::vector_quant::Error::Cancelled) => Err(QueryError::Cancelled),
        Err(crate::vector_quant::Error::Invalid(_)) => {
            Err(corrupt_query("quantized vector approximate scoring"))
        }
    }
}

pub(super) fn rerank_quantized_vector<C: FnMut() -> bool>(
    db: &Database,
    info: &IndexInfo,
    candidate: &ApproxHeapEntry,
    query: &[f32],
    query_norm: f64,
    metric: VectorMetric,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<f64>> {
    let dimension = crate::index::vector::quantized::dimension(info)?;
    // This probe covers historical layout/ordinal validation and the compact
    // entry re-read used for the authoritative consistency check.
    meter.charge(WorkResource::VectorLocators, 1)?;
    let ordinal = crate::index::vector::quantized::validate_locator(db, info, &candidate.locator)?;
    meter.charge(WorkResource::VectorSidecars, 1)?;
    let raw = db
        .store()?
        .get(&vector_key(candidate.id, ordinal))?
        .ok_or_else(|| corrupt_query("quantized vector locator points to missing sidecar"))?;
    let persisted = db
        .store()?
        .get(&crate::index::vector::quantized::entry_key(
            info.id,
            candidate.id.sequence,
        ))?
        .ok_or_else(|| corrupt_query("quantized shortlist entry disappeared"))?;
    meter.charge(
        WorkResource::VectorLanes,
        u64::try_from(dimension).map_err(invalid_query)?,
    )?;
    if crate::index::vector::quantized::encode_entry(candidate.locator, &raw, dimension)?
        != persisted
    {
        return Err(corrupt_query(
            "quantized vector entry differs from authoritative sidecar",
        ));
    }
    meter.charge(
        WorkResource::VectorLanes,
        u64::try_from(dimension).map_err(invalid_query)?,
    )?;
    crate::index::vector::quantized::exact_score(
        &raw,
        dimension,
        query,
        query_norm,
        metric,
        &mut || (meter.cancelled)(),
    )
    .map_err(QueryError::from)
}

pub(super) fn scalar_key_to_score(info: &IndexInfo, key: &[u8]) -> QueryResult<f64> {
    match scalar_order_value(info, key)? {
        OwnedScalarValue::Nullish => Ok(0.0),
        OwnedScalarValue::Bool(value) => Ok(if value { 1.0 } else { 0.0 }),
        OwnedScalarValue::I64(value) => Ok(value as f64),
        OwnedScalarValue::F64(value) => Ok(value),
        OwnedScalarValue::Text(_) => Err(corrupt_query("score scalar is not numeric")),
    }
}

pub(super) fn rank_candidate<'a, C: FnMut() -> bool>(
    db: &'a Database,
    rows: &mut PrimaryRows<'a>,
    order: &CompiledOrder,
    candidate: &Candidate,
    row: &mut Option<RowData>,
    encoded: &mut Option<Vec<u8>>,
    scratch: &mut RowScratch,
    meter: &mut WorkMeter<'_, C>,
) -> QueryResult<Option<RankKey>> {
    let value = match order {
        CompiledOrder::EntityId => RankValue::Entity,
        CompiledOrder::Scalar { info, .. } => {
            let key = candidate.scalar(info.id).map(<[u8]>::to_vec);
            let key = match key {
                Some(key) => key,
                None => {
                    ensure_row_seq(db, rows, candidate.id, row, encoded, meter)?;
                    meter.note_row_decode();
                    persisted_scalar_key(info, selected_field(row.as_ref().unwrap(), &info.field)?)?
                        .ok_or_else(|| corrupt_query("missing scalar order key"))?
                }
            };
            RankValue::Scalar(key)
        }
        CompiledOrder::ExactVector {
            info,
            query,
            query_norm,
            metric,
        } => {
            let Some(distance) =
                vector_score(db, candidate, info, query, *query_norm, *metric, meter)?
            else {
                return Ok(None);
            };
            RankValue::Score(distance.to_bits())
        }
        CompiledOrder::ApproximateVector { .. } => {
            unreachable!("approximate order uses shortlist then exact rerank")
        }
        CompiledOrder::Distance { info, center } => {
            let distance = if let Some(distance) = candidate.distance_metres(info.id) {
                distance
            } else if let Some(point) = candidate.point(info.id) {
                wgs84_distance_metres(*center, point)
            } else {
                ensure_row_seq(db, rows, candidate.id, row, encoded, meter)?;
                meter.note_row_decode();
                let Some(point) =
                    point_from_field(selected_field(row.as_ref().unwrap(), &info.field)?)?
                else {
                    return Ok(None);
                };
                wgs84_distance_metres(*center, point)
            };
            RankValue::Score(distance.to_bits())
        }
        // Driver order takes the key the driver's own walk is sorted by, and
        // every one of them is already in hand: the candidate's id, the
        // scalar posting's value key, or the spatial posting's cell. Ranking
        // a driver-ordered page reads nothing and allocates nothing.
        CompiledOrder::Driver(DriverKey::Entity) => RankValue::Entity,
        CompiledOrder::Driver(DriverKey::Scalar(index)) => RankValue::Scalar(
            candidate
                .scalar(*index)
                .ok_or_else(|| corrupt_query("driver order lost its scalar posting key"))?
                .to_vec(),
        ),
        CompiledOrder::Driver(DriverKey::Cell(index)) => RankValue::Cell(
            candidate
                .cell(*index)
                .ok_or_else(|| corrupt_query("driver order lost its spatial cell"))?,
        ),
        CompiledOrder::Driver(DriverKey::GeomCell(index)) => {
            let (level, cell) = candidate
                .geom_cell(*index)
                .ok_or_else(|| corrupt_query("driver order lost its geometry cell"))?;
            RankValue::GeomCell { level, cell }
        }
        CompiledOrder::Driver(DriverKey::Key) => RankValue::Key(
            candidate
                .key()
                .ok_or_else(|| corrupt_query("driver order lost its mapping key"))?
                .to_vec(),
        ),
        CompiledOrder::Bm25(prepared) => {
            let Some(score) = text_score(
                db,
                rows,
                prepared,
                candidate.id,
                candidate.text.as_ref(),
                row,
                encoded,
                scratch,
                meter,
            )? else {
                return Ok(None);
            };
            RankValue::Score(score.to_bits())
        }
        CompiledOrder::Score { expr, .. } => RankValue::Score(
            eval_score_expr(
                db, rows, expr, candidate, row, encoded, scratch, meter,
            )?
            .to_bits(),
        ),
    };
    Ok(Some(RankKey {
        value,
        id: candidate.id,
    }))
}

pub(super) fn scalar_order_value(info: &IndexInfo, key: &[u8]) -> QueryResult<OwnedScalarValue> {
    let (value, consumed) = scalar_key::decode(&info.kind, key)?;
    if consumed != key.len() {
        return Err(corrupt_query("scalar order key has a suffix"));
    }
    match (&info.kind, value) {
        (_, Value::Null) => Ok(OwnedScalarValue::Nullish),
        (Kind::Bool, Value::Bool(value)) => Ok(OwnedScalarValue::Bool(value)),
        (Kind::Int, Value::Number(value)) => value
            .as_i64()
            .map(OwnedScalarValue::I64)
            .ok_or_else(|| corrupt_query("scalar integer order value")),
        (Kind::Real, Value::Number(value)) => value
            .as_f64()
            .map(OwnedScalarValue::F64)
            .ok_or_else(|| corrupt_query("scalar real order value")),
        (Kind::Text, Value::String(value)) => Ok(OwnedScalarValue::Text(value)),
        _ => Err(corrupt_query("scalar order value kind")),
    }
}
