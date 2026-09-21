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
/// STATUS (sweep 2026-09-20): this cursor is not on any live path. Filtered
/// exact scoring point-gets each candidate's sidecar
/// (`score_locator_cancelled`), and the unfiltered order walks the sidecar
/// leaves in page order (`scan_exact_all`). The sacrifice the earlier note
/// described -- walking every vector field of the collection -- is therefore
/// not paid by any query today. The type stays as the reference shape for a
/// sequential filtered walk if a measurement ever asks for one.
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

/// STATUS (sweep 2026-09-20): not called from `SidecarCursor::seek`, which
/// takes an already-built key, or from anywhere else -- it exists only to
/// feed `write_vector_key` below, which is itself unused (see the
/// `SidecarCursor` STATUS note above). Kept with it as the same reference
/// shape.
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
///
/// STATUS (sweep 2026-09-20): unused -- see the `SidecarCursor` STATUS note
/// above, which this key format was built for.
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
/// scan uses this table so it never does a per-vector locator lookup.
///
/// Each locator is put through exactly the check the filtered path applies
/// before it believes a row: the layout it names must exist and must declare
/// THIS index's field, at that ordinal, with this index's dimension. Without
/// it a locator whose ordinal was rewritten points the join at another
/// field's sidecar -- the hidden external-key slot at ordinal zero, for one --
/// and the scan scores those bytes as a vector instead of reporting damage
/// (Law 5). The layout get is a single-slot cache hit for every locator after
/// the first of its layout, so the walk still reads only locator pages.
/// SACRIFICE: RAM proportional to the candidate set (8 bytes per locator),
/// not to k.
fn collect_locator_ordinals(
    db: &Database,
    index: &IndexInfo,
    dimension: usize,
    max_examined: usize,
    examined: &mut usize,
    progress: ScanProgress<'_>,
) -> Result<Vec<(u64, u16)>> {
    let store = db.store()?;
    let prefix = locator_prefix(index.id);
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
            let ordinal = db.locator_ordinal(index, value, dimension)?;
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
/// once and every matching vector on it is scored in a tight loop, filtered
/// by an ordinal table read from the locator pages in one page-order pass.
///
/// The locator table is not an optimization that a single-vector layout can
/// skip: it is the index's own record of WHICH rows this index holds and of
/// where each one's vector lives. A scan that reads only the `0x60` sidecars
/// answers from the ROWS, so a locator that is malformed, points at another
/// field, or names a sidecar that is gone is neither reported nor felt -- the
/// query returns a wrong answer instead of `Corrupt` (Law 5). Every locator is
/// therefore decoded and checked against the layout it names before the
/// sidecar it points at is believed.
/// SACRIFICE: RAM proportional to the locator set (8 bytes per locator), not
/// to k.
///
/// The two passes have two ceilings because they spend two different
/// resources. `max_locators` bounds the locator table read out of the `0x73`
/// keyspace and `max_examined` bounds the `0x60` sidecars scored; a caller
/// metering a budget derives each from the resource that pass actually
/// charges, so the charge that names the exhausted resource is the one that
/// stops the walk. A caller with a single allowance passes it as both.
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
    max_locators: usize,
    max_examined: usize,
    progress: ScanProgress<'_>,
) -> Result<Vec<VectorHit>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let dimension = dimension(index)?;
    let query_wide: Vec<f64> = query.iter().map(|lane| f64::from(*lane)).collect();
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

    let mut examined = 0usize;
    let ordinals =
        collect_locator_ordinals(db, index, dimension, max_locators, &mut examined, progress)?;
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
            if sidecars as usize == max_examined {
                // The ceiling stops the walk INSIDE it, at one record past
                // what the allowance holds, and the outstanding batch is
                // charged BEFORE the stop is reported: that charge is the one
                // that names the resource which ran out, and it lands at
                // exactly `limit + 1` because `sidecars` has never been
                // allowed past `max_examined`, so no batch can carry more
                // than the allowance into a single charge.
                progress(ScanStep::Scored(std::mem::take(&mut pending)))?;
                return Err(Error::Kernel(kernel::Error::ResourceLimit(
                    "exact vector max_examined exceeded",
                )));
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
                // The public API states ONE allowance and calls it locator
                // probes, so it bounds both passes: a caller that asked for
                // `n` probes gets no more than `n` locators and no more than
                // `n` sidecars scored out of them.
                return Ok(scan_exact_all(
                    self,
                    &index,
                    query,
                    query_norm,
                    metric,
                    k,
                    None,
                    max_examined,
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
    pub(super) fn locator_ordinal(
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
