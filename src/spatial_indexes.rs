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
/// The most a reported acceptance rate may multiply a ring's target by. A
/// caller whose filter has rejected every hit so far would otherwise ask for
/// a cover the size of its sample, one ring at a time; this bounds the guess
/// at the same place the planner's own crossover sits.
const NEAREST_ACCEPTANCE_CAP: usize = 64;

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

/// One posting the nearest walk is ready to yield, already in ascending
/// `(distance, id)` order relative to the rest of its ring.
#[derive(Clone, Copy, Debug)]
pub(super) struct NearestHit {
    pub id: EntityId,
    pub point: Point,
    pub distance_metres: f64,
}

/// Resumable outward-ring walk that yields point postings in ascending
/// geodesic distance from `center`. Shared by [`Database::query_point_nearest`]
/// and the query engine's `QueryOrder::Distance` driver.
///
/// Each ring is fully examined before anything in it is emitted, and only
/// postings with distance `<=` that ring's radius leave: Hilbert covers are
/// coarse, so a posting in ring `i`'s ranges can still be farther than `r_i`,
/// and emitting it now would invert the order. Those wait for a later ring.
/// The probe around the centre's cell is kept; it is not itself an ordered
/// stream, so its hits sit in `held` until the first ring completes.
pub(super) struct NearestWalk {
    index: IndexInfo,
    center: Point,
    geodesic: Geodesic,
    prefix: Vec<u8>,
    inner_ranges: Vec<(u64, u64)>,
    radius: f64,
    radius_cap: Option<f64>,
    hint_k: usize,
    max_examined: usize,
    examined: usize,
    probed: bool,
    /// True after the first ring has been walked (so the next fill grows).
    ring_started: bool,
    finished: bool,
    held: Vec<NearestHit>,
    ready: Vec<NearestHit>,
    ready_at: usize,
    /// Postings the most recently completed ring examined, for density growth.
    last_seen: usize,
    /// Hits the walk has handed over, and how many of them the caller's own
    /// filters kept. See [`NearestWalk::note`].
    offered: usize,
    accepted: usize,
}

impl NearestWalk {
    pub(super) fn new(
        index: IndexInfo,
        center: Point,
        hint_k: usize,
        radius_cap: Option<f64>,
        max_examined: usize,
    ) -> Self {
        Self {
            prefix: posting_prefix(index.id),
            index,
            center,
            geodesic: Geodesic::wgs84(),
            inner_ranges: Vec::new(),
            radius: NEAREST_START_RADIUS_METRES,
            radius_cap,
            hint_k,
            max_examined,
            examined: 0,
            probed: false,
            ring_started: false,
            finished: false,
            held: Vec::new(),
            ready: Vec::new(),
            ready_at: 0,
            last_seen: 0,
            offered: 0,
            accepted: 0,
        }
    }

    /// Tell the walk whether the hit it just handed over survived the
    /// caller's filters.
    ///
    /// The walk sizes each ring to hold about `hint_k` postings, because
    /// without this the only thing it knows about its caller is how many rows
    /// the caller asked for. Under a filter that is the wrong target: if one
    /// candidate in eight is kept, a ring holding `k` postings yields `k/8`
    /// answers, and the walk grows ring by ring -- re-covering, re-seeking and
    /// re-differencing the same space eight times over to reach a radius one
    /// ring could have named. Feeding the acceptance back makes the NEXT
    /// ring's target `k / acceptance` instead, so the ring the answer lives in
    /// is the ring the walk builds.
    ///
    /// It is a hint and only a hint: it moves no boundary and admits nothing.
    /// Each ring is still examined whole and still emits only what is within
    /// its radius, so the order this walk yields is the same whatever the
    /// caller reports here, or if it reports nothing at all.
    pub(super) fn note(&mut self, accepted: bool) {
        self.offered += 1;
        if accepted {
            self.accepted += 1;
        }
    }

    /// How many postings the next ring should aim to hold: `hint_k` when
    /// nothing is known about acceptance, and `hint_k / acceptance` once the
    /// caller has reported on a ring's worth of hits.
    ///
    /// The estimate is deliberately blunt -- it is a radius guess, and a ring
    /// that overshoots costs sorting, not correctness. `offered` below
    /// `hint_k` is too little evidence to act on (one unlucky rejection would
    /// multiply the target by the sample size), and the factor is capped so a
    /// filter that has rejected EVERYTHING so far grows the cover by a bounded
    /// amount rather than jumping to the world.
    fn ring_target(&self) -> usize {
        let k = self.hint_k.max(1);
        if self.offered < k {
            return k + 2;
        }
        let factor = (self.offered / self.accepted.max(1)).min(NEAREST_ACCEPTANCE_CAP);
        k.saturating_mul(factor).saturating_add(2)
    }

    /// Next posting in ascending `(distance, id)`, or `None` at the end of
    /// the walk (the world, or `radius_cap` if one was set).
    ///
    /// `extra` runs once per examined posting, before the walk's own
    /// `max_examined` spend: the query engine uses it to charge
    /// `work.spatial_postings`.
    pub(super) fn next(
        &mut self,
        db: &Database,
        cancelled: &mut impl FnMut() -> bool,
        extra: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Option<NearestHit>> {
        loop {
            if self.ready_at < self.ready.len() {
                let hit = self.ready[self.ready_at];
                self.ready_at += 1;
                return Ok(Some(hit));
            }
            if self.finished {
                return Ok(None);
            }
            self.ready.clear();
            self.ready_at = 0;
            self.fill_one_ring(db, cancelled, extra)?;
        }
    }

    /// Reposition the walk just past `(distance, id)`, the last hit the
    /// previous page returned. A page keeps one hit beyond what it returns to
    /// learn whether more exist, and a cursor that re-opens at the page's last
    /// key re-yields that hit; this walk carries its state instead, so the
    /// hit it handed over past the page would otherwise be lost. Every hit
    /// yielded so far lies in the current ready buffer (a fill only replaces
    /// the buffer once it is drained), so the buffer is where to rewind.
    pub(super) fn rewind_past(&mut self, distance_metres: f64, id: EntityId) {
        self.ready_at = self.ready.partition_point(|hit| {
            hit.distance_metres
                .total_cmp(&distance_metres)
                .then_with(|| hit.id.cmp(&id))
                != std::cmp::Ordering::Greater
        });
    }

    fn push_held(&mut self, entity: EntityId, point: Point) {
        self.held.push(NearestHit {
            id: entity,
            point,
            distance_metres: distance(&self.geodesic, self.center, point),
        });
    }

    fn initial_radius(&self) -> f64 {
        let mut radius = if self.hint_k > 0 && self.held.len() >= self.hint_k {
            let mut distances: Vec<f64> = self.held.iter().map(|hit| hit.distance_metres).collect();
            distances.sort_by(|a, b| a.total_cmp(b));
            distances[self.hint_k - 1].max(1.0)
        } else {
            NEAREST_START_RADIUS_METRES
        };
        if let Some(cap) = self.radius_cap {
            radius = radius.min(cap);
        }
        radius
    }

    fn at_cap(&self) -> bool {
        self.radius_cap.is_some_and(|cap| self.radius >= cap)
    }

    fn world_radius(&self) -> bool {
        self.radius / WGS84_MIN_CURVATURE_RADIUS_METRES >= core::f64::consts::PI
    }

    fn grow_radius(&mut self) {
        let seen = self.last_seen as f64;
        let target = self.ring_target();
        let jump = if seen > 0.0 {
            let density = seen / (core::f64::consts::PI * self.radius * self.radius);
            ((target as f64) / (core::f64::consts::PI * density)).sqrt() * 1.25
        } else {
            0.0
        };
        // The growth ceiling rises with the target for the same reason the
        // target does: a filter that keeps one candidate in eight needs a
        // cover about eight times wider in AREA, which is under three times
        // wider in radius, and clamping that back to 4x per ring would just
        // spread the same growth over more re-seeks.
        let ceiling = NEAREST_GROWTH_FACTOR
            * ((target as f64) / ((self.hint_k.max(1) + 2) as f64))
                .sqrt()
                .max(1.0);
        let mut radius = jump.max(self.radius * 2.0).min(self.radius * ceiling);
        if let Some(cap) = self.radius_cap {
            radius = radius.min(cap);
        }
        self.radius = radius;
    }

    fn probe(
        &mut self,
        db: &Database,
        cancelled: &mut impl FnMut() -> bool,
        extra: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let centre_cell = point_hilbert(self.center) as u32;
        let mut probe_lo = u64::from(centre_cell);
        let mut probe_hi = u64::from(centre_cell);
        let mut probed = false;
        let mut examined = self.examined;
        let max_examined = self.max_examined;
        let mut hits = Vec::new();
        {
            let start = posting_key_at(self.index.id, centre_cell, 0);
            let mut seen = 0usize;
            let mut current: Option<u32> = None;
            for row in db.index_range(&self.index, &start)?.into_iter().flatten() {
                let (key, value) = row?;
                if !key.starts_with(&self.prefix) {
                    break;
                }
                let (cell, sequence, point) = decode_posting(&self.prefix, &key, &value)?;
                if current.is_some_and(|c| c != cell) && seen >= NEAREST_PROBE {
                    break;
                }
                current = Some(cell);
                extra()?;
                spend(&mut examined, max_examined, cancelled)?;
                hits.push((
                    EntityId {
                        collection: self.index.collection,
                        sequence,
                    },
                    point,
                ));
                seen += 1;
                probe_hi = probe_hi.max(u64::from(cell));
                probed = true;
            }
        }
        {
            let to = posting_key_at(self.index.id, centre_cell, 0);
            let mut seen = 0usize;
            let mut current: Option<u32> = None;
            if let Some(mut it) = db.index_range_reverse(&self.index, &to)? {
                loop {
                    let (cell, sequence, point) = {
                        let Some((key, value)) = it.peek_ref()? else {
                            break;
                        };
                        if !key.starts_with(&self.prefix) {
                            break;
                        }
                        decode_posting(&self.prefix, key, value)?
                    };
                    if current.is_some_and(|c| c != cell) && seen >= NEAREST_PROBE {
                        break;
                    }
                    current = Some(cell);
                    extra()?;
                    spend(&mut examined, max_examined, cancelled)?;
                    hits.push((
                        EntityId {
                            collection: self.index.collection,
                            sequence,
                        },
                        point,
                    ));
                    seen += 1;
                    probe_lo = probe_lo.min(u64::from(cell));
                    probed = true;
                    it.step();
                }
            }
        }
        self.examined = examined;
        for (entity, point) in hits {
            self.push_held(entity, point);
        }
        if probed {
            self.inner_ranges.push((probe_lo, probe_hi));
        }
        Ok(())
    }

    fn fill_one_ring(
        &mut self,
        db: &Database,
        cancelled: &mut impl FnMut() -> bool,
        extra: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        if !self.probed {
            self.probe(db, cancelled, extra)?;
            self.probed = true;
            self.radius = self.initial_radius();
        } else if self.ring_started {
            self.grow_radius();
        }
        let world = self.world_radius();
        let bounds =
            radius_candidate_bounds(self.center, self.radius).expect("finite non-negative radius");
        let outer_ranges = bounds_hilbert_ranges_bounded(bounds, NEAREST_COVER_RANGES);
        let ring_ranges = ranges_difference(&outer_ranges, &self.inner_ranges);
        let examined_before = self.examined;
        if std::env::var("E4_DEBUG_RING").is_ok() {
            eprintln!(
                "radius={} outer_len={} ring_len={}",
                self.radius,
                outer_ranges.len(),
                ring_ranges.len()
            );
        }
        let index = self.index.clone();
        let prefix = self.prefix.clone();
        let mut examined = self.examined;
        let max_examined = self.max_examined;
        let mut new_hits = Vec::new();
        db.visit_ranges(
            &index,
            &prefix,
            &ring_ranges,
            || {
                extra()?;
                spend(&mut examined, max_examined, cancelled)
            },
            |entity, point| {
                new_hits.push((entity, point));
                Ok(())
            },
        )?;
        self.examined = examined;
        for (entity, point) in new_hits {
            self.push_held(entity, point);
        }
        let emit_upto = if world || self.at_cap() {
            self.radius_cap.unwrap_or(f64::INFINITY)
        } else {
            self.radius
        };
        self.held.sort_by(|a, b| {
            a.distance_metres
                .total_cmp(&b.distance_metres)
                .then_with(|| a.id.cmp(&b.id))
        });
        let split = self
            .held
            .partition_point(|hit| hit.distance_metres <= emit_upto);
        self.ready = self.held.drain(..split).collect();
        self.ready_at = 0;
        self.ring_started = true;
        self.last_seen = self.examined.saturating_sub(examined_before);
        if world || self.at_cap() {
            self.finished = true;
            self.held.clear();
        } else {
            self.inner_ranges = ranges_union(&self.inner_ranges, &outer_ranges);
        }
        Ok(())
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
        match candidates {
            SpatialCandidates::All => {
                let mut walk = NearestWalk::new(index, center, k, None, max_examined);
                let mut extra = || -> Result<()> { Ok(()) };
                let mut out = Vec::with_capacity(k.min(1024));
                while out.len() < k {
                    match walk.next(self, &mut cancelled, &mut extra)? {
                        None => break,
                        Some(hit) => out.push(SpatialHit {
                            id: hit.id,
                            distance_metres: hit.distance_metres,
                        }),
                    }
                }
                return Ok(out);
            }
            SpatialCandidates::SortedUnique(_) => {
                let geodesic = Geodesic::wgs84();
                let mut heap = BinaryHeap::with_capacity(k.min(1024));
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
                let mut out: Vec<_> = heap.into_iter().map(|hit| hit.0).collect();
                out.sort_by(|a, b| {
                    a.distance_metres
                        .total_cmp(&b.distance_metres)
                        .then_with(|| a.id.cmp(&b.id))
                });
                Ok(out)
            }
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
                        || spend(&mut examined, max_examined, cancelled),
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
    /// [`NearestWalk`], which calls this once per ring. `charge` runs once
    /// per examined posting so `max_examined` and `work.spatial_postings`
    /// bound the whole walk, not one ring.
    fn visit_ranges(
        &self,
        index: &IndexInfo,
        prefix: &[u8],
        ranges: &[(u64, u64)],
        mut charge: impl FnMut() -> Result<()>,
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
                charge()?;
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
/// call site in [`NearestWalk`] for why this must be a true running union
/// rather than just the previous ring's cover.
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
