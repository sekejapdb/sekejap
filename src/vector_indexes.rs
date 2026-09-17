//! Persisted exact-vector locators and bounded exact search.
//! Vector coordinates remain authoritative in the immutable `0x60` sidecars.
use super::*;
use std::{cmp::Ordering, collections::BinaryHeap};

pub(super) const VECTOR_FEATURE: u64 = 0x04;
pub(super) const VECTOR_ENTRY: u8 = 0x73;
pub(super) const MAX_DIM: usize = 16_384;

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

pub(super) fn locator_prefix(id: IndexId) -> Vec<u8> {
    let mut key = vec![VECTOR_ENTRY];
    key.extend(ordered(id.0));
    key
}

pub(super) fn locator_key(id: IndexId, sequence: u64) -> Vec<u8> {
    let mut key = locator_prefix(id);
    key.extend(ordered(sequence));
    key
}

pub(super) fn encode_locator(layout: u32, ordinal: usize) -> Result<[u8; 6]> {
    let ordinal = u16::try_from(ordinal).map_err(|_| corrupt("vector field ordinal overflow"))?;
    let mut value = [0; 6];
    value[..4].copy_from_slice(&layout.to_be_bytes());
    value[4..].copy_from_slice(&ordinal.to_be_bytes());
    Ok(value)
}

pub(super) fn decode_locator(value: &[u8]) -> Result<(u32, usize)> {
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
struct SidecarCursor<'a> {
    prefix: Vec<u8>,
    iter: kernel::btree::RangeIter<'a>,
    done: bool,
}

impl<'a> SidecarCursor<'a> {
    fn new(store: &'a Backend, c: CollectionId) -> Result<Self> {
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
    fn seek(&mut self, target: &[u8]) -> Result<Option<&[u8]>> {
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

fn write_ordered(dst: &mut [u8], n: u64) -> usize {
    let bytes = n.to_be_bytes();
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(7);
    dst[0] = 0x80 + (8 - start) as u8;
    let width = 8 - start;
    dst[1..1 + width].copy_from_slice(&bytes[start..]);
    1 + width
}

/// Sidecar key `0x60 || collection || sequence || field` in a stack buffer.
fn write_vector_key(buf: &mut [u8; 32], id: EntityId, field: usize) -> &[u8] {
    buf[0] = 0x60;
    let mut n = 1;
    n += write_ordered(&mut buf[n..], u64::from(id.collection.0));
    n += write_ordered(&mut buf[n..], id.sequence);
    n += write_ordered(&mut buf[n..], field as u64);
    &buf[..n]
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
    validate_vector(bytes, dimension)?;
    let mut dot = 0.0f64;
    let mut stored_norm = 0.0f64;
    let mut squared_l2 = 0.0f64;
    for (at, (lane, query_lane)) in bytes.chunks_exact(4).zip(query.iter()).enumerate() {
        if at % 256 == 0 && cancelled() {
            return Err(Error::Cancelled);
        }
        let stored = f64::from(f32::from_le_bytes(lane.try_into().unwrap()));
        let query_lane = f64::from(*query_lane);
        dot += stored * query_lane;
        stored_norm += stored * stored;
        let delta = stored - query_lane;
        squared_l2 += delta * delta;
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
    Ok(Some(VectorHit { id, distance }))
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

pub(super) fn dimension(i: &IndexInfo) -> Result<usize> {
    match (&i.family, &i.kind) {
        (IndexFamily::ExactVector, Kind::Vector(d)) if (1..=MAX_DIM).contains(d) => Ok(*d),
        _ => Err(corrupt("exact vector descriptor family/kind")),
    }
}

pub(super) fn validate_vector(bytes: &[u8], dimension: usize) -> Result<()> {
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

pub(super) fn maintain_locator(
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
pub(super) fn build_locator(
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
        if k > indexes::MAX_RESULTS {
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
            if ids.len() > indexes::MAX_RESULTS {
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

        let mut examined = 0usize;
        let mut heap = BinaryHeap::with_capacity(k.min(1024));
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

        match candidates {
            VectorCandidates::All => {
                // Two cursors, not one cursor and N descents. The locator scan
                // still decides WHICH rows are candidates and which ordinal
                // each one's vector lives at -- that is the authoritative set
                // and it is unchanged -- but the sidecar bytes now arrive from
                // a second forward scan running in the same key order, so a
                // whole-collection top-k costs two range scans instead of one
                // range scan plus a root-to-leaf descent per row.
                let prefix = locator_prefix(id);
                let store = self.store()?;
                let mut sidecars = SidecarCursor::new(store, index.collection)?;
                let mut failure = None;
                store.range(&prefix)?.for_each_ref(|key, value| {
                    if !key.starts_with(&prefix) {
                        return false;
                    }
                    if let Err(e) = (|| -> Result<()> {
                        spend()?;
                        let mut at = prefix.len();
                        let sequence = read_ordered(key, &mut at)?;
                        if at != key.len() || sequence == 0 {
                            return Err(corrupt("exact vector locator key"));
                        }
                        let entity = EntityId {
                            collection: index.collection,
                            sequence,
                        };
                        let ordinal = self.locator_ordinal(&index, value, dimension)?;
                        let mut target_buf = [0u8; 32];
                        let target = write_vector_key(&mut target_buf, entity, ordinal);
                        let hit = match sidecars.seek(target)? {
                            Some(bytes) => score_vector_bytes(
                                entity,
                                bytes,
                                dimension,
                                query,
                                query_norm,
                                metric,
                                &mut || false,
                            )?,
                            // Not where the scan is: the sidecar is absent or
                            // damaged. Pay the descent for this one row and let
                            // the point-read path produce its diagnosis.
                            None => self
                                .score_locator(&index, entity, value, query, query_norm, metric)?,
                        };
                        admit(&mut heap, k, hit);
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
            }
            VectorCandidates::SortedUnique(ids) => {
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
    pub(super) fn score_locator_cancelled(
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
#[path = "vector_fault_tests.rs"]
mod fault_tests;
