//! Persisted exact WGS84 point postings and bounded exact queries.
//! Primary dense-v3 rows remain authoritative; postings duplicate exact `f64`
//! coordinates so unfiltered bbox/radius/nearest queries need no row fetch.

use super::*;
use crate::spatial_math::{
    bounds_hilbert_ranges, bounds_hilbert_ranges_bounded, point_hilbert, radius_candidate_bounds,
    Bounds, Point,
    WGS84_MIN_CURVATURE_RADIUS_METRES,
};
use geographiclib_rs::{Geodesic, InverseGeodesic};
use std::{cmp::Ordering, collections::BinaryHeap};

/// First ring radius of `query_point_nearest`'s outward walk.
const NEAREST_START_RADIUS_METRES: f64 = 250.0;
/// Each ring's outer radius is this many times the previous ring's.
const NEAREST_GROWTH_FACTOR: f64 = 4.0;
/// Range budget of one ring's cover: a ring is paid for in tree descents,
/// one per range, so its cover is deliberately coarse.
const NEAREST_COVER_RANGES: usize = 8;
/// Postings the probe reads on each side of the centre's cell (whole cells).
const NEAREST_PROBE: usize = 32;

pub(super) const SPATIAL_FEATURE: u64 = 0x08;
pub(super) const POINT_ENTRY: u8 = 0x74;
pub(super) const GRID_BITS: u8 = 16;
pub(super) const CRS_WGS84: u8 = 1;
pub(super) const METRIC_KARNEY_V1: u8 = 1;

#[derive(Clone, Copy, Debug)]
pub enum SpatialCandidates<'a> {
    All,
    /// IDs must belong to the index collection and be strictly sorted.
    SortedUnique(&'a [EntityId]),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpatialHit {
    pub id: EntityId,
    pub distance_metres: f64,
}

#[derive(Clone, Copy, Debug)]
struct HeapHit(SpatialHit);

impl PartialEq for HeapHit {
    fn eq(&self, other: &Self) -> bool {
        self.0.distance_metres.to_bits() == other.0.distance_metres.to_bits()
            && self.0.id == other.0.id
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
            .distance_metres
            .total_cmp(&other.0.distance_metres)
            .then_with(|| self.0.id.cmp(&other.0.id))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PointEntry {
    pub key: Vec<u8>,
    pub value: [u8; 16],
}

pub(super) fn posting_prefix(id: IndexId) -> Vec<u8> {
    let mut key = vec![POINT_ENTRY];
    key.extend(ordered(id.0));
    key
}

pub(super) fn posting_key(id: IndexId, point: Point, sequence: u64) -> Vec<u8> {
    posting_key_at(id, point_hilbert(point) as u32, sequence)
}

/// The posting key of one `(cell, sequence)` pair. `posting_key` derives the
/// cell from a point; a resumed cell walk already holds the cell and has no
/// point to derive it from again.
pub(super) fn posting_key_at(id: IndexId, cell: u32, sequence: u64) -> Vec<u8> {
    let mut key = posting_prefix(id);
    key.extend(cell.to_be_bytes());
    key.extend(ordered(sequence));
    key
}

pub(super) fn encode_point(point: Point) -> [u8; 16] {
    let mut value = [0; 16];
    value[..8].copy_from_slice(&point.longitude().to_le_bytes());
    value[8..].copy_from_slice(&point.latitude().to_le_bytes());
    value
}

pub(super) fn decode_point(value: &[u8]) -> Result<Point> {
    if value.len() != 16 {
        return Err(corrupt("spatial point posting length"));
    }
    Point::new(
        f64::from_le_bytes(value[..8].try_into().unwrap()),
        f64::from_le_bytes(value[8..].try_into().unwrap()),
    )
    .map_err(corrupt)
}

pub(super) fn descriptor(i: &IndexInfo) -> Result<()> {
    if i.family != IndexFamily::SpatialPoint
        || i.kind != Kind::Point
        || i.unique
        || !matches!((i.encoding_version, i.tree), (1, None) | (2, Some(_)))
    {
        return Err(corrupt("spatial point descriptor family/options"));
    }
    Ok(())
}

pub(super) fn selected_point(document: &Value, field: &str) -> Result<Option<Point>> {
    let Some(value) = document.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let object = value
        .as_object()
        .ok_or_else(|| invalid("indexed point must be a GeoJSON Point"))?;
    if object.len() != 2 || object.get("type").and_then(Value::as_str) != Some("Point") {
        return Err(invalid("indexed point must be a GeoJSON Point"));
    }
    let coordinates = object
        .get("coordinates")
        .and_then(Value::as_array)
        .filter(|coordinates| coordinates.len() == 2)
        .ok_or_else(|| invalid("indexed point requires two coordinates"))?;
    let lon = coordinates[0]
        .as_f64()
        .ok_or_else(|| invalid("indexed point longitude must be numeric"))?;
    let lat = coordinates[1]
        .as_f64()
        .ok_or_else(|| invalid("indexed point latitude must be numeric"))?;
    Point::new(lon, lat).map(Some).map_err(invalid)
}

pub(super) fn point_entry(i: &IndexInfo, id: EntityId, point: Point) -> PointEntry {
    PointEntry {
        key: posting_key(i.id, point, id.sequence),
        value: encode_point(point),
    }
}

/// Derive one posting from an immutable dense-v3 row without fetching or
/// rendering vector sidecars. A historically absent/null point emits no key.
pub(super) fn build_point_entry(
    db: &Database,
    i: &IndexInfo,
    id: EntityId,
    row: &[u8],
) -> Result<Option<PointEntry>> {
    descriptor(i)?;
    let layout_id = layout_id(row)?;
    let layout = db.layout(layout_id)?;
    if let Some((_, historical_kind)) = layout.fields.iter().find(|(name, _)| name == &i.field) {
        if historical_kind != &Kind::Point {
            return Err(invalid("historical indexed point field changed kind"));
        }
    }
    let document = crate::dense_v3::decode_with_vector_values(&layout, row, |_, _| Ok(None))
        .map_err(corrupt)?;
    selected_point(&document, &i.field).map(|point| point.map(|point| point_entry(i, id, point)))
}

/// Maintain one point posting inside the caller's entity transaction. Invalid
/// new coordinates are rejected before the primary row is published.
pub(super) fn maintain_point(
    db: &mut Database,
    i: &mut IndexInfo,
    id: EntityId,
    old: Option<&Value>,
    new: Option<&Value>,
) -> Result<()> {
    descriptor(i)?;
    let old = old
        .map(|document| selected_point(document, &i.field))
        .transpose()?
        .flatten()
        .map(|point| point_entry(i, id, point));
    let new = new
        .map(|document| selected_point(document, &i.field))
        .transpose()?
        .flatten()
        .map(|point| point_entry(i, id, point));
    if old == new {
        return Ok(());
    }
    match (old, new) {
        (Some(old), Some(new)) if old.key == new.key => {
            db.index_put(i, &new.key, &new.value)?;
        }
        (old, new) => {
            if let Some(old) = old {
                db.index_delete(i, &old.key)?;
            }
            if let Some(new) = new {
                db.index_put(i, &new.key, &new.value)?;
            }
        }
    }
    Ok(())
}

pub(super) fn decode_posting(prefix: &[u8], key: &[u8], value: &[u8]) -> Result<(u32, u64, Point)> {
    if !key.starts_with(prefix) || key.len() < prefix.len() + 5 {
        return Err(corrupt("spatial point posting key"));
    }
    let hilbert = u32::from_be_bytes(key[prefix.len()..prefix.len() + 4].try_into().unwrap());
    let mut at = prefix.len() + 4;
    let sequence = read_ordered(key, &mut at)?;
    if at != key.len() || sequence == 0 {
        return Err(corrupt("spatial point posting identity"));
    }
    let point = decode_point(value)?;
    if point_hilbert(point) != u64::from(hilbert) {
        return Err(corrupt("spatial point posting Hilbert mismatch"));
    }
    Ok((hilbert, sequence, point))
}

fn validate_candidates(index: &IndexInfo, candidates: SpatialCandidates<'_>) -> Result<()> {
    if let SpatialCandidates::SortedUnique(ids) = candidates {
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
    Ok(())
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
            "spatial max_examined exceeded",
        )));
    }
    *examined += 1;
    Ok(())
}

impl Database {
    pub fn create_point_index(
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
        if kind != Kind::Point {
            return Err(invalid("spatial point index requires a Point field"));
        }
        self.create_index(
            collection,
            name,
            field,
            kind,
            false,
            IndexFamily::SpatialPoint,
            SPATIAL_FEATURE,
        )
    }

    /// Inclusive exact bbox query. Results are the smallest matching stable IDs
    /// up to `limit`, returned in ID order. `max_examined` counts every posting
    /// or filtered candidate before its exact predicate is applied.
    pub fn query_point_bbox(
        &self,
        id: IndexId,
        bounds: Bounds,
        limit: usize,
        candidates: SpatialCandidates<'_>,
        max_examined: usize,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Vec<EntityId>> {
        let index = self.ready_spatial_index(id)?;
        validate_candidates(&index, candidates)?;
        if limit > indexes::MAX_RESULTS {
            return Err(invalid("query result limit exceeds 65536"));
        }
        if cancelled() {
            return Err(Error::Cancelled);
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let ranges = bounds_hilbert_ranges(bounds);
        let mut heap = BinaryHeap::with_capacity(limit.min(1024));
        self.visit_spatial_points(
            &index,
            Some(&ranges),
            candidates,
            max_examined,
            &mut cancelled,
            |entity, point| {
                if bounds.contains(point) {
                    heap.push(entity);
                    if heap.len() > limit {
                        heap.pop();
                    }
                }
                Ok(())
            },
        )?;
        let mut out = heap.into_vec();
        out.sort_unstable();
        Ok(out)
    }

    /// Inclusive exact WGS84 radius query. Results use stable ID order; a
    /// conservative Hilbert envelope is always refined with Karney distance.
    pub fn query_point_radius(
        &self,
        id: IndexId,
        center: Point,
        radius_metres: f64,
        limit: usize,
        candidates: SpatialCandidates<'_>,
        max_examined: usize,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Vec<EntityId>> {
        let candidate_bounds = radius_candidate_bounds(center, radius_metres).map_err(invalid)?;
        let index = self.ready_spatial_index(id)?;
        validate_candidates(&index, candidates)?;
        if limit > indexes::MAX_RESULTS {
            return Err(invalid("query result limit exceeds 65536"));
        }
        if cancelled() {
            return Err(Error::Cancelled);
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let ranges = bounds_hilbert_ranges(candidate_bounds);
        let geodesic = Geodesic::wgs84();
        let mut heap = BinaryHeap::with_capacity(limit.min(1024));
        self.visit_spatial_points(
            &index,
            Some(&ranges),
            candidates,
            max_examined,
            &mut cancelled,
            |entity, point| {
                if distance(&geodesic, center, point) <= radius_metres {
                    heap.push(entity);
                    if heap.len() > limit {
                        heap.pop();
                    }
                }
                Ok(())
            },
        )?;
        let mut out = heap.into_vec();
        out.sort_unstable();
        Ok(out)
    }

    /// Exact nearest points. `All` walks Hilbert-ordered rings outward from
    /// `center`, so the postings examined are bounded by the density around
    /// the k nearest hits, not the size of the index; filtered mode probes
    /// only the supplied primary entities before top-k.
    pub fn query_point_nearest(
        &self,
        id: IndexId,
        center: Point,
        k: usize,
        candidates: SpatialCandidates<'_>,
        max_examined: usize,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Vec<SpatialHit>> {
        let index = self.ready_spatial_index(id)?;
        validate_candidates(&index, candidates)?;
        if k > indexes::MAX_RESULTS {
            return Err(invalid("query result limit exceeds 65536"));
        }
        if cancelled() {
            return Err(Error::Cancelled);
        }
        if k == 0 {
            return Ok(Vec::new());
        }
        let geodesic = Geodesic::wgs84();
        let mut heap = BinaryHeap::with_capacity(k.min(1024));
        match candidates {
            SpatialCandidates::All => {
                self.ring_walk_nearest(
                    &index,
                    center,
                    k,
                    max_examined,
                    &mut cancelled,
                    &geodesic,
                    &mut heap,
                )?;
            }
            SpatialCandidates::SortedUnique(_) => {
                self.visit_spatial_points(
                    &index,
                    None,
                    candidates,
                    max_examined,
                    &mut cancelled,
                    |entity, point| {
                        heap.push(HeapHit(SpatialHit {
                            id: entity,
                            distance_metres: distance(&geodesic, center, point),
                        }));
                        if heap.len() > k {
                            heap.pop();
                        }
                        Ok(())
                    },
                )?;
            }
        }
        let mut out: Vec<_> = heap.into_iter().map(|hit| hit.0).collect();
        out.sort_by(|a, b| {
            a.distance_metres
                .total_cmp(&b.distance_metres)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    /// Outward concentric-ring walk for [`Database::query_point_nearest`]'s
    /// `SpatialCandidates::All` path. Ring `i` is the annulus between radius
    /// `r_{i-1}` (0 for the first ring) and `r_i`; it is covered by
    /// `radius_candidate_bounds(center, r_i)`'s Hilbert ranges minus every
    /// ring examined so far, so every posting is examined at most once. (The
    /// cover is not guaranteed to grow monotonically with `r_i` -- see
    /// `ranges_union`'s use below -- so "every ring so far" must be an
    /// accumulated union, not just the immediately preceding ring.) The walk
    /// stops as soon as the heap holds `k` hits whose worst exact
    /// distance is `<= r_i`: [`radius_candidate_bounds`] guarantees every
    /// point within `r_i` of `center` lies in some ring `<= i`'s cover, so
    /// nothing unexamined can beat the current top-k. The last possible ring
    /// is the whole world (once `r_i` reaches the antipodal bound); the walk
    /// always accepts whatever the heap holds after that ring, exact answer
    /// or not, since no larger radius exists to keep searching.
    fn ring_walk_nearest(
        &self,
        index: &IndexInfo,
        center: Point,
        k: usize,
        max_examined: usize,
        cancelled: &mut impl FnMut() -> bool,
        geodesic: &Geodesic,
        heap: &mut BinaryHeap<HeapHit>,
    ) -> Result<()> {
        let prefix = posting_prefix(index.id);
        let mut examined = 0usize;
        // The PROBE: before any ring, read the postings nearest to the centre
        // in KEY order -- whole cells on either side of the centre's own cell
        // until at least `NEAREST_PROBE` postings have been seen. Hilbert
        // order keeps neighbours in space mostly neighbours in the key, so
        // this one descent (plus one reverse) usually yields k candidates and
        // an UPPER BOUND on the k-th distance: the first ring is then the
        // circle of that radius, and after it the answer is exact, because
        // every point closer than the k-th candidate lies inside that circle
        // and the circle has been examined in full. The cells the probe read
        // completely are recorded so no ring examines a posting twice.
        let mut inner_ranges: Vec<(u64, u64)> = Vec::new();
        let centre_cell = point_hilbert(center) as u32;
        let mut push = |heap: &mut BinaryHeap<HeapHit>, entity: EntityId, point: Point| {
            heap.push(HeapHit(SpatialHit {
                id: entity,
                distance_metres: distance(geodesic, center, point),
            }));
            if heap.len() > k {
                heap.pop();
            }
        };
        let (mut probe_lo, mut probe_hi) = (u64::from(centre_cell), u64::from(centre_cell));
        let mut probed = false;
        // Forward: cells >= the centre's, whole cells only.
        {
            let start = posting_key_at(index.id, centre_cell, 0);
            let mut seen = 0usize;
            let mut current: Option<u32> = None;
            for row in self.index_range(index, &start)?.into_iter().flatten() {
                let (key, value) = row?;
                if !key.starts_with(&prefix) {
                    break;
                }
                let (cell, sequence, point) = decode_posting(&prefix, &key, &value)?;
                if current.is_some_and(|c| c != cell) && seen >= NEAREST_PROBE {
                    break;
                }
                current = Some(cell);
                spend(&mut examined, max_examined, cancelled)?;
                push(
                    heap,
                    EntityId {
                        collection: index.collection,
                        sequence,
                    },
                    point,
                );
                seen += 1;
                probe_hi = probe_hi.max(u64::from(cell));
                probed = true;
            }
            // `current` was fully read unless the loop ended on a cell change,
            // in which case the last complete cell is the one before it: the
            // break happens BEFORE the new cell's first posting is counted, so
            // `probe_hi` already names the last complete cell.
        }
        // Backward: cells strictly below the centre's, whole cells only.
        {
            let to = posting_key_at(index.id, centre_cell, 0);
            let mut seen = 0usize;
            let mut current: Option<u32> = None;
            // The reverse iterator is a pull cursor (peek, then step).
            if let Some(mut it) = self.index_range_reverse(index, &to)? {
                loop {
                    let (cell, sequence, point) = {
                        let Some((key, value)) = it.peek_ref()? else {
                            break;
                        };
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        decode_posting(&prefix, key, value)?
                    };
                    if current.is_some_and(|c| c != cell) && seen >= NEAREST_PROBE {
                        break;
                    }
                    current = Some(cell);
                    spend(&mut examined, max_examined, cancelled)?;
                    push(
                        heap,
                        EntityId {
                            collection: index.collection,
                            sequence,
                        },
                        point,
                    );
                    seen += 1;
                    probe_lo = probe_lo.min(u64::from(cell));
                    probed = true;
                    it.step();
                }
            }
        }
        if probed {
            inner_ranges.push((probe_lo, probe_hi));
        }
        // With k candidates in hand the k-th distance bounds the first ring;
        // otherwise start small and let the rings grow.
        let mut radius = match heap.peek() {
            Some(worst) if heap.len() >= k => worst.0.distance_metres.max(1.0),
            _ => NEAREST_START_RADIUS_METRES,
        };
        loop {
            let world = radius / WGS84_MIN_CURVATURE_RADIUS_METRES >= core::f64::consts::PI;
            let bounds =
                radius_candidate_bounds(center, radius).expect("finite non-negative radius");
            let outer_ranges = bounds_hilbert_ranges_bounded(bounds, NEAREST_COVER_RANGES);
            let ring_ranges = ranges_difference(&outer_ranges, &inner_ranges);
            let examined_before = examined;
            if std::env::var("E4_DEBUG_RING").is_ok() {
                eprintln!(
                    "radius={radius} outer_len={} ring_len={}",
                    outer_ranges.len(),
                    ring_ranges.len()
                );
            }
            self.visit_ranges(
                index,
                &prefix,
                &ring_ranges,
                &mut examined,
                max_examined,
                cancelled,
                |entity, point| {
                    push(heap, entity, point);
                    Ok(())
                },
            )?;
            if world {
                return Ok(());
            }
            if heap.len() >= k && heap.peek().is_some_and(|worst| worst.0.distance_metres <= radius)
            {
                return Ok(());
            }
            // `outer_ranges` is not guaranteed to grow monotonically with
            // `radius`: `bounds_hilbert_ranges` falls back to the world range
            // whenever a box's cover fragments past `MAX_HILBERT_RANGES`
            // (common near the poles), and a later, larger, less-fragmented
            // box can then cover fewer cells than that fallback. Accumulate
            // the union of every ring's cover so far, not just the last
            // ring's, or a later smaller cover would "forget" cells the
            // fallback already visited and revisit them.
            inner_ranges = ranges_union(&inner_ranges, &outer_ranges);
            // The postings this ring examined estimate the local density, and
            // the density says how far out the k-th neighbour should be: jump
            // straight to that radius (with a margin) instead of stepping by
            // the fixed factor, so a walk over uniform data ends in two rings.
            // Growth never falls below x2, so the walk still terminates.
            let seen = (examined - examined_before) as f64;
            let jump = if seen > 0.0 {
                let density = seen / (core::f64::consts::PI * radius * radius);
                (((k + 2) as f64) / (core::f64::consts::PI * density)).sqrt() * 1.25
            } else {
                0.0
            };
            radius = jump.max(radius * 2.0).min(radius * NEAREST_GROWTH_FACTOR);
        }
    }

    fn ready_spatial_index(&self, id: IndexId) -> Result<IndexInfo> {
        let index = self.index_info(id)?;
        if index.family != IndexFamily::SpatialPoint {
            return Err(invalid("index is not a spatial point index"));
        }
        descriptor(&index)?;
        if index.state != IndexState::Ready {
            return Err(invalid("index is not ready"));
        }
        Ok(index)
    }

    fn visit_spatial_points(
        &self,
        index: &IndexInfo,
        ranges: Option<&[(u64, u64)]>,
        candidates: SpatialCandidates<'_>,
        max_examined: usize,
        cancelled: &mut impl FnMut() -> bool,
        mut visit: impl FnMut(EntityId, Point) -> Result<()>,
    ) -> Result<()> {
        let mut examined = 0usize;
        match candidates {
            SpatialCandidates::All => {
                let prefix = posting_prefix(index.id);
                if let Some(ranges) = ranges {
                    self.visit_ranges(
                        index,
                        &prefix,
                        ranges,
                        &mut examined,
                        max_examined,
                        cancelled,
                        visit,
                    )?;
                } else {
                    for row in self.index_range(index, &prefix)?.into_iter().flatten() {
                        let (key, value) = row?;
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        spend(&mut examined, max_examined, cancelled)?;
                        let (_, sequence, point) = decode_posting(&prefix, &key, &value)?;
                        visit(
                            EntityId {
                                collection: index.collection,
                                sequence,
                            },
                            point,
                        )?;
                    }
                }
            }
            SpatialCandidates::SortedUnique(ids) => {
                for entity in ids {
                    spend(&mut examined, max_examined, cancelled)?;
                    let Some(row) = self.store()?.get(&row_key(*entity))? else {
                        continue;
                    };
                    let Some(entry) = build_point_entry(self, index, *entity, &row)? else {
                        continue;
                    };
                    let stored = self
                        .index_get(index, &entry.key)?
                        .ok_or_else(|| corrupt("ready spatial point posting is missing"))?;
                    if stored.as_slice() != entry.value {
                        return Err(corrupt("ready spatial point posting differs from primary"));
                    }
                    let point = decode_point(&entry.value)?;
                    visit(*entity, point)?;
                }
            }
        }
        Ok(())
    }

    /// Visit every posting of `index` whose Hilbert cell falls in `ranges`,
    /// in Hilbert order. Shared by the exhaustive `All` scan and by
    /// [`Database::ring_walk_nearest`], which calls this once per ring with
    /// one `examined` counter threaded across calls so `max_examined` bounds
    /// the whole walk, not one ring.
    fn visit_ranges(
        &self,
        index: &IndexInfo,
        prefix: &[u8],
        ranges: &[(u64, u64)],
        examined: &mut usize,
        max_examined: usize,
        cancelled: &mut impl FnMut() -> bool,
        mut visit: impl FnMut(EntityId, Point) -> Result<()>,
    ) -> Result<()> {
        for &(lo, hi) in ranges {
            let mut start = prefix.to_vec();
            start.extend((lo as u32).to_be_bytes());
            for row in self.index_range(index, &start)?.into_iter().flatten() {
                let (key, value) = row?;
                if !key.starts_with(prefix) {
                    break;
                }
                let hilbert = key
                    .get(prefix.len()..prefix.len() + 4)
                    .ok_or_else(|| corrupt("spatial point posting key"))?;
                let hilbert = u32::from_be_bytes(hilbert.try_into().unwrap());
                if u64::from(hilbert) > hi {
                    break;
                }
                spend(examined, max_examined, cancelled)?;
                let (_, sequence, point) = decode_posting(prefix, &key, &value)?;
                visit(
                    EntityId {
                        collection: index.collection,
                        sequence,
                    },
                    point,
                )?;
            }
        }
        Ok(())
    }
}

fn distance(geodesic: &Geodesic, a: Point, b: Point) -> f64 {
    geodesic.inverse(a.latitude(), a.longitude(), b.latitude(), b.longitude())
}

/// `outer` minus `inner`: both are sorted, merged, disjoint inclusive
/// ranges (the shape [`bounds_hilbert_ranges`] returns); the result is the
/// same. Used to turn a ring's outer Hilbert cover into just the cells not
/// already covered by every previous ring's (unioned) cover.
fn ranges_difference(outer: &[(u64, u64)], inner: &[(u64, u64)]) -> Vec<(u64, u64)> {
    if inner.is_empty() {
        return outer.to_vec();
    }
    let mut result = Vec::new();
    let mut j = 0;
    for &(lo, hi) in outer {
        let mut cur = lo;
        while cur <= hi {
            while j < inner.len() && inner[j].1 < cur {
                j += 1;
            }
            let Some(&(ilo, ihi)) = inner.get(j) else {
                result.push((cur, hi));
                break;
            };
            if ilo > hi {
                result.push((cur, hi));
                break;
            }
            if ilo > cur {
                result.push((cur, ilo - 1));
            }
            if ihi >= hi {
                break;
            }
            cur = ihi + 1;
        }
    }
    result
}

/// The union of two sorted, merged, disjoint inclusive range lists (the
/// shape [`bounds_hilbert_ranges`] returns), itself sorted and merged. Used
/// to accumulate every ring's Hilbert cover so far: see the comment at its
/// call site in [`Database::ring_walk_nearest`] for why this must be a true
/// running union rather than just the previous ring's cover.
fn ranges_union(a: &[(u64, u64)], b: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut all: Vec<(u64, u64)> = a.iter().chain(b.iter()).copied().collect();
    all.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(all.len());
    for (lo, hi) in all {
        match merged.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

#[cfg(test)]
#[path = "spatial_fault_tests.rs"]
mod fault_tests;
