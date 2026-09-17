//! Persisted exact WGS84 point postings and bounded exact queries.
//! Primary dense-v3 rows remain authoritative; postings duplicate exact `f64`
//! coordinates so unfiltered bbox/radius/nearest queries need no row fetch.

use super::*;
use crate::spatial_math::{
    bounds_hilbert_ranges, point_hilbert, radius_candidate_bounds, Bounds, Point,
};
use geographiclib_rs::{Geodesic, InverseGeodesic};
use std::{cmp::Ordering, collections::BinaryHeap};

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
    let mut key = posting_prefix(id);
    key.extend((point_hilbert(point) as u32).to_be_bytes());
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
        || i.encoding_version != 1
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
    i: &IndexInfo,
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
            db.writer()?.put(&new.key, &new.value)?;
        }
        (old, new) => {
            if let Some(old) = old {
                db.writer()?.delete(&old.key)?;
            }
            if let Some(new) = new {
                db.writer()?.put(&new.key, &new.value)?;
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

    /// Exact nearest points. `All` deliberately scans the complete index;
    /// filtered mode probes only the supplied primary entities before top-k.
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
                    for &(lo, hi) in ranges {
                        let mut start = prefix.clone();
                        start.extend((lo as u32).to_be_bytes());
                        for row in self.store()?.range(&start)? {
                            let (key, value) = row?;
                            if !key.starts_with(&prefix) {
                                break;
                            }
                            let hilbert = key
                                .get(prefix.len()..prefix.len() + 4)
                                .ok_or_else(|| corrupt("spatial point posting key"))?;
                            let hilbert = u32::from_be_bytes(hilbert.try_into().unwrap());
                            if u64::from(hilbert) > hi {
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
                } else {
                    for row in self.store()?.range(&prefix)? {
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
                        .store()?
                        .get(&entry.key)?
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
}

fn distance(geodesic: &Geodesic, a: Point, b: Point) -> f64 {
    geodesic.inverse(a.latitude(), a.longitude(), b.latitude(), b.longitude())
}

#[cfg(test)]
#[path = "spatial_fault_tests.rs"]
mod fault_tests;
