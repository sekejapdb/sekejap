//! Persisted exact-vector locators and bounded exact search.
//! Vector coordinates remain authoritative in the immutable `0x60` sidecars.
use crate::collections::{
    corrupt, catalog, invalid, layout_id, ordered, prefix, read_ordered, vector_key, CollectionId,
    Database, EntityId, Error, IndexFamily, IndexId, IndexInfo, IndexState, Result, VectorCells,
};
use crate::store::Backend;
use crate::{Kind, Layout};
use std::{cmp::Ordering, collections::BinaryHeap};

pub(crate) const VECTOR_FEATURE: u64 = 0x04;
pub(crate) const VECTOR_ENTRY: u8 = 0x73;
pub(crate) const MAX_DIM: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorMetric {
    Cosine,
    SquaredL2,
    NegativeDot,
}

#[derive(Clone, Copy, Debug)]
pub enum VectorCandidates<'a> {
    All,
    /// Entity IDs must belong to the index collection and be strictly sorted.
    SortedUnique(&'a [EntityId]),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VectorHit {
    pub id: EntityId,
    pub distance: f64,
}

/// The `(distance, entity)` a page must start strictly after: the rank key
/// the previous page ended on.
///
/// A bounded top-k that is taken over the WHOLE corpus and filtered by the
/// cursor afterwards returns the first page again, minus the rows already
/// emitted -- which is how page two of a vector query used to lose rows. The
/// cursor therefore goes INTO the scan: a candidate the cursor does not admit
/// never enters the heap, so a heap of k holds the next k rows and not the
/// first k.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VectorAfter {
    pub distance: f64,
    pub id: EntityId,
}

impl VectorAfter {
    /// The query engine's own score comparison, lane for lane: ascending by
    /// distance, entity id breaking ties, NaN last under both directions.
    #[inline]
    pub(super) fn admits(&self, distance: f64, id: EntityId) -> bool {
        let order = match (distance.is_nan(), self.distance.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => distance.total_cmp(&self.distance),
        };
        match order {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => id > self.id,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct HeapHit(VectorHit);

impl PartialEq for HeapHit {
    fn eq(&self, other: &Self) -> bool {
        self.0.distance.to_bits() == other.0.distance.to_bits() && self.0.id == other.0.id
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
        self.0
            .distance
            .total_cmp(&other.0.distance)
            .then_with(|| self.0.id.cmp(&other.0.id))
    }
}

pub(crate) fn locator_prefix(id: IndexId) -> Vec<u8> {
    let mut key = vec![VECTOR_ENTRY];
    key.extend(ordered(id.0));
    key
}

pub(crate) fn locator_key(id: IndexId, sequence: u64) -> Vec<u8> {
    let mut key = locator_prefix(id);
    key.extend(ordered(sequence));
    key
}

pub(crate) fn encode_locator(layout: u32, ordinal: usize) -> Result<[u8; 6]> {
    let ordinal = u16::try_from(ordinal).map_err(|_| corrupt("vector field ordinal overflow"))?;
    let mut value = [0; 6];
    value[..4].copy_from_slice(&layout.to_be_bytes());
    value[4..].copy_from_slice(&ordinal.to_be_bytes());
    Ok(value)
}

pub(crate) fn decode_locator(value: &[u8]) -> Result<(u32, usize)> {
    if value.len() != 6 {
        return Err(corrupt("exact vector locator length"));
    }
    let layout = u32::from_be_bytes(value[..4].try_into().unwrap());
    let ordinal = usize::from(u16::from_be_bytes(value[4..].try_into().unwrap()));
    if layout == 0 {
        return Err(corrupt("exact vector locator layout"));
    }
    Ok((layout, ordinal))
}

/// Every sidecar of one collection, in `(sequence, ordinal)` order.
pub(super) fn sidecar_prefix(c: CollectionId) -> Vec<u8> {
    prefix(0x60, c)
}

/// A one-way cursor over a collection's `0x60` sidecar range.
///
/// Sidecars sort by `(sequence, ordinal)` and `All`-candidate search walks
/// locators in ascending sequence, one locator per sequence per index, so the
/// sidecar keys it asks for are strictly ascending WHATEVER ordinal each row's
/// layout puts the field at. That is what makes a single forward scan enough:
/// the cursor is dragged forward to each requested key instead of paying a
/// fresh root-to-leaf descent per row, and a population split across two
/// layouts is walked, not re-descended.
///
/// The cursor still reports a miss for any key it did not land on -- a sidecar
/// that is absent, or a caller asking out of order -- and the caller falls
/// back to a point read, so the answer never depends on the argument above
/// holding. It is a speed claim with a correct slow path underneath it, not a
/// correctness claim.
///
/// SACRIFICE (Law 4): the cursor walks EVERY vector field of the collection,
/// not just the indexed one, so a collection with several vector fields pays
/// a longer walk than the locators strictly need. It is still one sequential
/// pass over pages the file already has to hold, against one root-to-leaf
/// descent per candidate row before. It holds one sidecar in RAM at a time --
/// RAM proportional to a single vector, not to the collection.
#[allow(dead_code)]
pub(super) struct SidecarCursor<'a> {
    prefix: Vec<u8>,
    iter: kernel::btree::RangeIter<'a>,
    done: bool,
}

#[allow(dead_code)]
impl<'a> SidecarCursor<'a> {
    pub(super) fn new(store: &'a Backend, c: CollectionId) -> Result<Self> {
        let prefix = sidecar_prefix(c);
        let iter = store.range(&prefix)?;
        Ok(Self {
            prefix,
            iter,
            done: false,
        })
    }

    /// Advance to `target` and return its stored bytes, or `None` when the
    /// cursor is already beyond it. Slices are borrowed from the pinned leaf.
    pub(super) fn seek(&mut self, target: &[u8]) -> Result<Option<&[u8]>> {
        if self.done {
            return Ok(None);
        }
        match self.iter.peek_at_or_after(target)? {
            Some((key, value)) if key.starts_with(&self.prefix) && key == target => Ok(Some(value)),
            Some((key, _)) if key.starts_with(&self.prefix) => Ok(None),
            _ => {
                self.done = true;
                Ok(None)
            }
        }
    }
}

#[allow(dead_code)]
fn write_ordered(dst: &mut [u8], n: u64) -> usize {
    let bytes = n.to_be_bytes();
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(7);
    dst[0] = 0x80 + (8 - start) as u8;
    let width = 8 - start;
    dst[1..1 + width].copy_from_slice(&bytes[start..]);
    1 + width
}

/// Sidecar key `0x60 || collection || sequence || field` in a stack buffer.
#[allow(dead_code)]
pub(super) fn write_vector_key(buf: &mut [u8; 32], id: EntityId, field: usize) -> &[u8] {
    buf[0] = 0x60;
    let mut n = 1;
    n += write_ordered(&mut buf[n..], u64::from(id.collection.0));
    n += write_ordered(&mut buf[n..], id.sequence);
    n += write_ordered(&mut buf[n..], field as u64);
    &buf[..n]
}

#[inline(always)]
fn load_f32_le(bytes: &[u8], off: usize) -> f32 {
    // The lane is f32 LITTLE-ENDIAN on disk whatever the host is, so the byte
    // order is named here rather than inherited from the target. On a
    // little-endian target this is still one unaligned four-byte load.
    let raw = unsafe { std::ptr::read_unaligned(bytes.as_ptr().add(off) as *const [u8; 4]) };
    f32::from_le_bytes(raw)
}

/// f32 sidecar distance against a pre-widened query. Eight-lane chunks, no
/// bounds checks, sequential f64 accumulation so Cosine/L2/dot match the
/// previous per-lane zip.
pub(super) fn score_f32_pre(
    bytes: &[u8],
    query: &[f64],
    query_norm: f64,
    metric: VectorMetric,
) -> Result<Option<f64>> {
    let dim = query.len();
    if bytes.len() != dim * 4 {
        return Err(corrupt("exact vector sidecar length"));
    }
    let mut dot = 0.0f64;
    let mut stored_norm = 0.0f64;
    let mut squared_l2 = 0.0f64;
    let mut at = 0usize;
    while at + 8 <= dim {
        unsafe {
            for j in 0..8 {
                let stored = f64::from(load_f32_le(bytes, (at + j) * 4));
                if !stored.is_finite() {
                    return Err(corrupt("non-finite exact vector sidecar"));
                }
                let query_lane = *query.get_unchecked(at + j);
                dot += stored * query_lane;
                stored_norm += stored * stored;
                let delta = stored - query_lane;
                squared_l2 += delta * delta;
            }
        }
        at += 8;
    }
    while at < dim {
        unsafe {
            let stored = f64::from(load_f32_le(bytes, at * 4));
            if !stored.is_finite() {
                return Err(corrupt("non-finite exact vector sidecar"));
            }
            let query_lane = *query.get_unchecked(at);
            dot += stored * query_lane;
            stored_norm += stored * stored;
            let delta = stored - query_lane;
            squared_l2 += delta * delta;
        }
        at += 1;
    }
    let mut distance = match metric {
        VectorMetric::SquaredL2 => squared_l2,
        VectorMetric::NegativeDot => -dot,
        VectorMetric::Cosine if stored_norm == 0.0 => return Ok(None),
        VectorMetric::Cosine => 1.0 - dot / (stored_norm.sqrt() * query_norm.sqrt()),
    };
    if distance == 0.0 {
        distance = 0.0;
    }
    Ok(Some(distance))
}

/// f32 sidecar distance. Widens the query once then scores.
pub(super) fn score_f32_distance(
    bytes: &[u8],
    query: &[f32],
    query_norm: f64,
    metric: VectorMetric,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<f64>> {
    if cancelled() {
        return Err(Error::Cancelled);
    }
    let wide: Vec<f64> = query.iter().map(|lane| f64::from(*lane)).collect();
    score_f32_pre(bytes, &wide, query_norm, metric)
}

/// Score one already-loaded sidecar. Shared by the scanned and the
/// point-read paths so both produce identical f64 arithmetic.
fn score_vector_bytes(
    id: EntityId,
    bytes: &[u8],
    dimension: usize,
    query: &[f32],
    query_norm: f64,
    metric: VectorMetric,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<VectorHit>> {
    if query.len() != dimension {
        return Err(corrupt("exact vector sidecar length"));
    }
    let Some(distance) = score_f32_distance(bytes, query, query_norm, metric, cancelled)? else {
        return Ok(None);
    };
    Ok(Some(VectorHit { id, distance }))
}

/// Locator ordinals in sequence order. One page-order walk; the sidecar
/// scan uses this table so it never does a per-vector locator lookup or
/// layout get. SACRIFICE: RAM proportional to the candidate set (8 bytes
/// per locator), not to k.
fn collect_locator_ordinals(
    store: &Backend,
    index_id: IndexId,
    max_examined: usize,
    examined: &mut usize,
    progress: ScanProgress<'_>,
) -> Result<Vec<(u64, u16)>> {
    let prefix = locator_prefix(index_id);
    let mut out = Vec::new();
    let mut failure = None;
    let mut pending = 0u64;
    store.range(&prefix)?.for_each_ref(|key, value| {
        if !key.starts_with(&prefix) {
            return false;
        }
        if let Err(e) = (|| -> Result<()> {
            if pending == SCAN_STEP {
                progress(ScanStep::Locators(std::mem::take(&mut pending)))?;
            }
            if *examined == max_examined {
                progress(ScanStep::Locators(std::mem::take(&mut pending)))?;
                return Err(Error::Kernel(kernel::Error::ResourceLimit(
                    "exact vector max_examined exceeded",
                )));
            }
            *examined += 1;
            pending += 1;
            let mut at = prefix.len();
            let sequence = read_ordered(key, &mut at)?;
            if at != key.len() || sequence == 0 {
                return Err(corrupt("exact vector locator key"));
            }
            let (_, ordinal) = decode_locator(value)?;
            let ordinal =
                u16::try_from(ordinal).map_err(|_| corrupt("vector field ordinal overflow"))?;
            out.push((sequence, ordinal));
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
    progress(ScanStep::Locators(pending))?;
    Ok(out)
}

/// How many records a page-order scan may read between two `progress` calls.
///
/// The bound this buys is the reason it is small: a budget is charged, and a
/// cancellation is polled, once per this many records rather than once per
/// scan, so a query with a candidate budget of 100 over 50M rows stops inside
/// the walk instead of reading all 50M and reporting the overrun afterwards.
pub(super) const SCAN_STEP: u64 = 256;

/// What a page-order scan has just read, handed back WHILE it is still
/// walking so the caller can charge and cancel it in flight.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ScanStep {
    /// Locator records read out of the `0x73` table.
    Locators(u64),
    /// Vector records scored: `0x60` sidecars for the exact scan, `0x79`
    /// compact entries for the quantized one.
    Scored(u64),
}

/// The charge-and-cancel hook a page-order scan calls every [`SCAN_STEP`]
/// records. Returning `Err` stops the walk: `Error::Cancelled` is the caller
/// saying stop, and a caller that is metering a budget stashes its own richer
/// error and returns `Cancelled` too.
pub(super) type ScanProgress<'a> = &'a mut dyn FnMut(ScanStep) -> Result<()>;

/// The progress hook a plain cancellation callback becomes: no budget, poll
/// on the same cadence as before.
pub(super) fn cancel_only(cancelled: &mut impl FnMut() -> bool) -> impl FnMut(ScanStep) -> Result<()> + '_ {
    move |_| {
        if cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// One layout descriptor by id, or None when that id was never minted.
///
/// `Database::layout` cannot say "absent": it folds a missing id into the
/// same corrupt-metadata error as a damaged one, and it keeps a single-slot
/// cache that a sweep over every layout would evict on every call. This reads
/// the three replicas directly and leaves that cache alone.
fn layout_if_present(db: &Database, id: u32) -> Result<Option<Layout>> {
    let store = db.store()?;
    let mut present = false;
    for copy in 0..3u8 {
        let Some(bytes) = store.get(&crate::collections::layout_key(id, copy))? else {
            continue;
        };
        present = true;
        if let Ok(layout) = Layout::from_descriptor(&bytes) {
            return Ok(Some(layout));
        }
    }
    if present {
        return Err(corrupt("vector index layout descriptor"));
    }
    Ok(None)
}

/// Does this layout agree that `ordinal` is exactly this index's vector
/// field, and does it put nothing else there?
///
/// Two ways to agree. Either the layout declares the indexed field at that
/// ordinal with the indexed dimension -- its rows write their sidecar exactly
/// where the scan will look -- or the layout has no vector of ours at all AND
/// no other vector field at that ordinal, so its rows contribute no sidecar
/// the scan could either miss or mistake for one of ours.
fn layout_agrees(layout: &Layout, field: &str, dim: usize, ordinal: usize) -> bool {
    let ours = layout
        .fields
        .iter()
        .position(|(name, kind)| name == field && matches!(kind, Kind::Vector(_)));
    match ours {
        Some(at) => at == ordinal && matches!(layout.fields.get(at), Some((_, Kind::Vector(d))) if *d == dim),
        None => !matches!(layout.fields.get(ordinal), Some((_, Kind::Vector(_)))),
    }
}

/// The one ordinal EVERY layout puts this index's vector field at, when there
/// is one: the sidecar ordinal is then unique, so the locator table is not
/// needed to decide which sidecar to score.
///
/// The current layout alone is not enough to ask. `alter_collection` mints a
/// NEW layout id and rewrites no rows, and `validate_indexed_layout` only
/// requires the indexed field's name and kind to survive -- not its ordinal.
/// So a collection whose vector field moved from ordinal 1 to ordinal 2 has
/// its population split across two layouts, and a scan that filtered on the
/// current ordinal alone would score only the rows written after the alter
/// and report them, with no error, as the whole top-k. Every layout in the
/// database is checked rather than only this collection's, because the
/// catalog records one layout id and not a history: the extra layouts belong
/// to other collections and can only push this answer onto the locator path,
/// never off it.
fn single_vector_ordinal(db: &Database, index: &IndexInfo, dim: usize) -> Result<Option<u16>> {
    let catalog = db.catalog(index.collection)?;
    let current = db.layout(catalog.layout)?;
    let mut found = None;
    for (ordinal, (name, kind)) in current.fields.iter().enumerate() {
        if matches!(kind, Kind::Vector(d) if *d == dim) {
            if found.is_some() || name != &index.field {
                return Ok(None);
            }
            found = Some(ordinal);
        }
    }
    let Some(only) = found else {
        return Ok(None);
    };
    let (_, next_layout) = db.header()?;
    for id in 0..next_layout {
        if id == catalog.layout {
            continue;
        }
        let Some(layout) = layout_if_present(db, id)? else {
            continue;
        };
        if !layout_agrees(&layout, &index.field, dim, only) {
            return Ok(None);
        }
    }
    Ok(Some(
        u16::try_from(only).map_err(|_| corrupt("vector field ordinal overflow"))?,
    ))
}

fn finish_exact_heap(heap: BinaryHeap<HeapHit>) -> Vec<VectorHit> {
    let mut hits: Vec<_> = heap.into_iter().map(|hit| hit.0).collect();
    hits.sort_by(|a, b| {
        a.distance
            .total_cmp(&b.distance)
            .then_with(|| a.id.cmp(&b.id))
    });
    hits
}

/// Inlined on purpose: this is the per-row body of the page-order scan, and
/// the cursor it now carries is then an argument the caller keeps in a
/// register across the walk rather than one it pushes per vector.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn admit_scored(
    heap: &mut BinaryHeap<HeapHit>,
    k: usize,
    collection: CollectionId,
    sequence: u64,
    bytes: &[u8],
    query: &[f64],
    query_norm: f64,
    metric: VectorMetric,
    after: Option<VectorAfter>,
) -> Result<()> {
    let Some(distance) = score_f32_pre(bytes, query, query_norm, metric)? else {
        return Ok(());
    };
    let id = EntityId {
        collection,
        sequence,
    };
    // The page cursor is applied HERE, before the bounded heap, so a heap of
    // k holds the next k rows in rank order and not the first k with the
    // already-returned ones cut out of them.
    if after.is_some_and(|after| !after.admits(distance, id)) {
        return Ok(());
    }
    admit(heap, k, Some(VectorHit { id, distance }));
    Ok(())
}

/// Page-order exact top-k: sidecar pages once. Each sidecar leaf is decoded
/// once and every matching vector on it is scored in a tight loop. When the
/// current layout has a single vector field, locators are not read at all
/// (they only map winners back, and the sidecar key already carries the
/// entity id). Otherwise a locator-page ordinal table filters mixed fields.
/// SACRIFICE of the mixed path: RAM proportional to the locator set.
///
/// `after` is the page cursor and is applied per candidate, before the heap.
/// `progress` is called every [`SCAN_STEP`] SIDECAR RECORDS -- every record
/// the walk touches, not only the ones the ordinal filter keeps, so a long
/// run of another field's sidecars cannot starve the cancellation poll.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_exact_all(
    db: &Database,
    index: &IndexInfo,
    query: &[f32],
    query_norm: f64,
    metric: VectorMetric,
    k: usize,
    after: Option<VectorAfter>,
    max_examined: usize,
    progress: ScanProgress<'_>,
) -> Result<Vec<VectorHit>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let dimension = dimension(index)?;
    let query_wide: Vec<f64> = query.iter().map(|lane| f64::from(*lane)).collect();
    let only = single_vector_ordinal(db, index, dimension)?;
    let store = db.store()?;
    let prefix = sidecar_prefix(index.collection);
    let mut heap = BinaryHeap::with_capacity(k.min(1024));
    let mut sidecars = 0u64;
    // Every sidecar record the walk sees, whatever its ordinal. The poll
    // cadence rides on this and not on the kept count: a collection whose
    // matching sidecars are followed by a long run of another field's would
    // otherwise leave the kept count at a non-multiple of the step and never
    // poll again.
    let mut visited = 0u64;
    // Kept records not yet handed to `progress`.
    let mut pending = 0u64;
    let mut failure = None;

    if let Some(only) = only {
        store.range(&prefix)?.for_each_ref(|key, bytes| {
            if !key.starts_with(&prefix) {
                return false;
            }
            if let Err(e) = (|| -> Result<()> {
                visited += 1;
                if visited % SCAN_STEP == 0 {
                    progress(ScanStep::Scored(std::mem::take(&mut pending)))?;
                }
                let mut cur = prefix.len();
                let sequence = read_ordered(key, &mut cur)?;
                let ordinal = read_ordered(key, &mut cur)?;
                if cur != key.len() || sequence == 0 {
                    return Err(corrupt("exact vector sidecar key"));
                }
                if ordinal != u64::from(only) {
                    return Ok(());
                }
                if sidecars as usize == max_examined {
                    // Charge what is outstanding BEFORE reporting the limit:
                    // when the limit came from a budget, that charge is the
                    // one that names the resource that ran out.
                    progress(ScanStep::Scored(std::mem::take(&mut pending)))?;
                    return Err(Error::Kernel(kernel::Error::ResourceLimit(
                        "exact vector max_examined exceeded",
                    )));
                }
                sidecars += 1;
                pending += 1;
                admit_scored(
                    &mut heap,
                    k,
                    index.collection,
                    sequence,
                    bytes,
                    &query_wide,
                    query_norm,
                    metric,
                    after,
                )
            })() {
                failure = Some(e);
                return false;
            }
            true
        })?;
        if let Some(e) = failure {
            return Err(e);
        }
        progress(ScanStep::Scored(pending))?;
        return Ok(finish_exact_heap(heap));
    }

    let mut examined = 0usize;
    let ordinals =
        collect_locator_ordinals(store, index.id, max_examined, &mut examined, progress)?;
    let mut at = 0usize;
    store.range(&prefix)?.for_each_ref(|key, bytes| {
        if !key.starts_with(&prefix) {
            return false;
        }
        if let Err(e) = (|| -> Result<()> {
            visited += 1;
            if visited % SCAN_STEP == 0 {
                progress(ScanStep::Scored(std::mem::take(&mut pending)))?;
            }
            let mut cur = prefix.len();
            let sequence = read_ordered(key, &mut cur)?;
            let ordinal = read_ordered(key, &mut cur)?;
            if cur != key.len() || sequence == 0 {
                return Err(corrupt("exact vector sidecar key"));
            }
            if at < ordinals.len() && ordinals[at].0 < sequence {
                return Err(corrupt("indexed vector sidecar is missing"));
            }
            if at >= ordinals.len() {
                return Ok(());
            }
            if ordinals[at].0 != sequence || u64::from(ordinals[at].1) != ordinal {
                return Ok(());
            }
            at += 1;
            sidecars += 1;
            pending += 1;
            admit_scored(
                &mut heap,
                k,
                index.collection,
                sequence,
                bytes,
                &query_wide,
                query_norm,
                metric,
                after,
            )
        })() {
            failure = Some(e);
            return false;
        }
        true
    })?;
    if let Some(e) = failure {
        return Err(e);
    }
    if at < ordinals.len() {
        return Err(corrupt("indexed vector sidecar is missing"));
    }
    progress(ScanStep::Scored(pending))?;
    Ok(finish_exact_heap(heap))
}

fn admit(heap: &mut BinaryHeap<HeapHit>, k: usize, hit: Option<VectorHit>) {
    let Some(hit) = hit else {
        return;
    };
    let candidate = HeapHit(hit);
    if heap.len() < k {
        heap.push(candidate);
        return;
    }
    if candidate < *heap.peek().unwrap() {
        heap.pop();
        heap.push(candidate);
    }
}

pub(crate) fn dimension(i: &IndexInfo) -> Result<usize> {
    match (&i.family, &i.kind) {
        (IndexFamily::ExactVector, Kind::Vector(d)) if (1..=MAX_DIM).contains(d) => Ok(*d),
        _ => Err(corrupt("exact vector descriptor family/kind")),
    }
}

pub(crate) fn validate_vector(bytes: &[u8], dimension: usize) -> Result<()> {
    if bytes.len() != dimension * 4 {
        return Err(corrupt("exact vector sidecar length"));
    }
    for lane in bytes.chunks_exact(4) {
        if !f32::from_le_bytes(lane.try_into().unwrap()).is_finite() {
            return Err(corrupt("non-finite exact vector sidecar"));
        }
    }
    Ok(())
}

fn desired_locator(
    i: &IndexInfo,
    layout: &Layout,
    vectors: &VectorCells,
) -> Result<Option<[u8; 6]>> {
    let expected = dimension(i)?;
    let Some((ordinal, (_, kind))) = layout
        .fields
        .iter()
        .enumerate()
        .find(|(_, (name, _))| name == &i.field)
    else {
        return Err(corrupt("current indexed vector field is absent"));
    };
    if kind != &Kind::Vector(expected) {
        return Err(corrupt("current indexed vector field changed kind"));
    }
    let Some((_, bytes)) = vectors.iter().find(|(field, _)| *field == ordinal) else {
        return Ok(None);
    };
    validate_vector(bytes, expected)?;
    let layout_id = u32::try_from(layout.id).map_err(corrupt)?;
    Ok(Some(encode_locator(layout_id, ordinal)?))
}

pub(crate) fn maintain_locator(
    db: &mut Database,
    i: &IndexInfo,
    id: EntityId,
    new: Option<(&Layout, &VectorCells)>,
) -> Result<()> {
    let key = locator_key(i.id, id.sequence);
    let desired = new
        .map(|(layout, vectors)| desired_locator(i, layout, vectors))
        .transpose()?
        .flatten();
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

/// Derive one physical locator from an immutable row and validate its primary
/// sidecar. Missing/null/historically absent fields intentionally emit no key.
pub(crate) fn build_locator(
    db: &Database,
    i: &IndexInfo,
    id: EntityId,
    row: &[u8],
) -> Result<Option<[u8; 6]>> {
    let expected = dimension(i)?;
    let lid = layout_id(row)?;
    let layout = db.layout(lid)?;
    let ordinal =
        crate::dense_v3::locate_vector(&layout, row, &i.field, expected).map_err(|e| {
            let message = e.to_string();
            if message.contains("historical vector field") {
                invalid(message)
            } else {
                corrupt(message)
            }
        })?;
    let Some(ordinal) = ordinal else {
        return Ok(None);
    };
    let bytes = db
        .store()?
        .get(&vector_key(id, ordinal))?
        .ok_or_else(|| corrupt("indexed vector sidecar is missing"))?;
    validate_vector(&bytes, expected)?;
    Ok(Some(encode_locator(lid, ordinal)?))
}

impl Database {
    pub fn create_exact_vector_index(
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
        if !matches!(kind, Kind::Vector(d) if (1..=MAX_DIM).contains(&d)) {
            return Err(invalid("exact vector index requires a vector field"));
        }
        self.create_index(
            collection,
            name,
            field,
            kind,
            false,
            IndexFamily::ExactVector,
            VECTOR_FEATURE,
        )
    }

    /// Exact search over persisted locators. `max_examined` counts locator
    /// probes (candidate probes in filtered mode) and returns no partial result.
    pub fn query_exact_vector(
        &self,
        id: IndexId,
        query: &[f32],
        metric: VectorMetric,
        k: usize,
        candidates: VectorCandidates<'_>,
        max_examined: usize,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Vec<VectorHit>> {
        if k > catalog::MAX_RESULTS {
            return Err(invalid("query result limit exceeds 65536"));
        }
        let index = self.index_info(id)?;
        if index.family != IndexFamily::ExactVector {
            return Err(invalid("index is not an exact vector index"));
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
        if let VectorCandidates::SortedUnique(ids) = candidates {
            if ids.len() > catalog::MAX_RESULTS {
                return Err(invalid("filtered candidate limit exceeds 65536"));
            }
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
        if k == 0 {
            return Ok(Vec::new());
        }

        let mut heap = BinaryHeap::with_capacity(k.min(1024));

        match candidates {
            VectorCandidates::All => {
                let mut progress = cancel_only(&mut cancelled);
                return Ok(scan_exact_all(
                    self,
                    &index,
                    query,
                    query_norm,
                    metric,
                    k,
                    None,
                    max_examined,
                    &mut progress,
                )?);
            }
            VectorCandidates::SortedUnique(ids) => {
                let mut examined = 0usize;
                let mut spend = || -> Result<()> {
                    if cancelled() {
                        return Err(Error::Cancelled);
                    }
                    if examined == max_examined {
                        return Err(Error::Kernel(kernel::Error::ResourceLimit(
                            "exact vector max_examined exceeded",
                        )));
                    }
                    examined += 1;
                    Ok(())
                };
                for candidate in ids {
                    spend()?;
                    if let Some(locator) =
                        self.store()?.get(&locator_key(id, candidate.sequence))?
                    {
                        let entity = EntityId {
                            collection: index.collection,
                            sequence: candidate.sequence,
                        };
                        let hit = self
                            .score_locator(&index, entity, &locator, query, query_norm, metric)?;
                        admit(&mut heap, k, hit);
                    }
                }
            }
        }
        let mut out: Vec<_> = heap.into_iter().map(|hit| hit.0).collect();
        out.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    pub(super) fn score_locator(
        &self,
        index: &IndexInfo,
        id: EntityId,
        locator: &[u8],
        query: &[f32],
        query_norm: f64,
        metric: VectorMetric,
    ) -> Result<Option<VectorHit>> {
        self.score_locator_cancelled(index, id, locator, query, query_norm, metric, &mut || false)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn score_locator_cancelled(
        &self,
        index: &IndexInfo,
        id: EntityId,
        locator: &[u8],
        query: &[f32],
        query_norm: f64,
        metric: VectorMetric,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<VectorHit>> {
        let dimension = dimension(index)?;
        let ordinal = self.locator_ordinal(index, locator, dimension)?;
        let bytes = self
            .store()?
            .get(&vector_key(id, ordinal))?
            .ok_or_else(|| corrupt("exact vector locator points to missing sidecar"))?;
        score_vector_bytes(id, &bytes, dimension, query, query_norm, metric, cancelled)
    }

    /// Validate a locator against the layout it names and return the physical
    /// sidecar ordinal it points at. Split out of `score_locator_cancelled` so
    /// the scanned path performs exactly the same check before believing a row.
    fn locator_ordinal(
        &self,
        index: &IndexInfo,
        locator: &[u8],
        dimension: usize,
    ) -> Result<usize> {
        let (layout_id, ordinal) = decode_locator(locator)?;
        let layout = self.layout(layout_id)?;
        let field_matches = matches!(
            layout.fields.get(ordinal),
            Some((name, Kind::Vector(found))) if name == &index.field && *found == dimension
        );
        if layout.id != u64::from(layout_id) || !field_matches {
            return Err(corrupt("exact vector locator field/layout mismatch"));
        }
        Ok(ordinal)
    }
}

#[cfg(test)]
#[path = "../../faults/vector_fault_tests.rs"]
mod fault_tests;
