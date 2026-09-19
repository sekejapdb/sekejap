//! Persisted geometry bbox postings (family `SpatialGeometry`) and their
//! build/maintain/decode primitives. The query filters over this family are
//! a later item (X1b); this file is index-only: format, build, maintenance,
//! and the raw scan the verifier and X1b's oracle test rely on. No query API.
//!
//! DESIGN. PostGIS's GiST keeps one bounding box per leaf entry and refines
//! with the full geometry from the heap. This index's disk-first analogue: a
//! geometry posts its own bbox at every cell of a fixed-level grid its bbox
//! covers, choosing the FINEST level in a fixed ladder
//! (`kernel::spatial::LEVEL_FINE` / `LEVEL_COARSE` / `LEVEL_WORLD`) whose
//! cover does not exceed `MAX_CELLS` cells -- so write cost is O(1) postings
//! per geometry, ever (Law 2, bounded at 8), and a query knows which levels
//! exist without reading the geometry itself. `LEVEL_WORLD`'s grid is 1x1,
//! so it always fits: a geometry too large for the coarse ladder rung still
//! posts exactly one entry, in the bucket every world-spanning query already
//! has to visit (sound, bounded, cheap -- the same shape as PostGIS's own
//! oversized-object band in an R-tree). One accepted consequence: a bbox
//! computed from raw (unwrapped) longitude does not special-case the
//! antimeridian, so a dateline-crossing polygon's bbox spans nearly the
//! whole globe and almost always falls through to the world bucket. Sound
//! (the posting still contains the true bbox), just pessimistic; unwrapping
//! antimeridian-crossing geometries is left to a later item if it matters.
//!
//! Each posting's value is the geometry's OWN bbox (not the cell's, which is
//! implied by the key), stored once per posting as a `BoxF` (16 bytes,
//! outward-rounded f32): a candidate is admitted or rejected by `BoxF`
//! overlap alone, with no payload read, exactly as PostGIS's `box2df` does.
use super::*;
use kernel::spatial::{self, BoxF, Geom};

pub(super) const GEOMETRY_FEATURE: u64 = 0x100;
pub(super) const GEOM_ENTRY: u8 = 0x7c;
/// The fixed ladder, spelled out here (not just read from `kernel::spatial`)
/// so the descriptor bytes below pin them: a future change to the ladder's
/// levels or cell budget must bump the descriptor's encoding version, exactly
/// as `spatial_indexes::GRID_BITS`/`CRS_WGS84`/`METRIC_KARNEY_V1` pin the
/// point family's grid.
pub(super) const LEVEL_FINE: u8 = spatial::LEVEL_FINE;
pub(super) const LEVEL_COARSE: u8 = spatial::LEVEL_COARSE;
pub(super) const LEVEL_WORLD: u8 = spatial::LEVEL_WORLD;
pub(super) const MAX_CELLS: u8 = spatial::MAX_CELLS as u8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GeometryEntry {
    pub key: Vec<u8>,
    pub value: [u8; 16],
}

pub(super) fn posting_prefix(id: IndexId) -> Vec<u8> {
    let mut key = vec![GEOM_ENTRY];
    key.extend(ordered(id.0));
    key
}

/// One posting's key: `prefix || level:u8 || cell:u32be || sequence`. The
/// level comes first (before the cell) so a query for one ladder level is a
/// single contiguous key range, never interleaved with another level's cells.
pub(super) fn posting_key_at(id: IndexId, level: u8, cell: u32, sequence: u64) -> Vec<u8> {
    let mut key = posting_prefix(id);
    key.push(level);
    key.extend(cell.to_be_bytes());
    key.extend(ordered(sequence));
    key
}

pub(super) fn descriptor(i: &IndexInfo) -> Result<()> {
    if i.family != IndexFamily::SpatialGeometry
        || i.kind != Kind::Geo
        || i.unique
        || i.encoding_version != 1
        || i.tree.is_some()
    {
        return Err(corrupt("spatial geometry descriptor family/options"));
    }
    Ok(())
}

/// Parse a GeoJSON geometry object into `Geom`. Coordinate range/finiteness
/// is already enforced for every `Kind::Geo` value at write time
/// (`crate::geo`, called from `dense_v3::encode` regardless of indexing), so
/// this only needs to reject the shapes that function does not check:
/// unsupported types and geometry extensions.
fn geom_from_value(value: &Value) -> Result<Geom> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("indexed geometry must be a GeoJSON object"))?;
    if object.len() != 2 {
        return Err(invalid("indexed geometry extensions are unsupported"));
    }
    let coordinates = object
        .get("coordinates")
        .cloned()
        .ok_or_else(|| invalid("indexed geometry requires coordinates"))?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("indexed geometry requires a type"))?;
    fn bad(e: impl std::fmt::Display) -> Error {
        invalid(format!("indexed geometry coordinates: {e}"))
    }
    Ok(match kind {
        "Point" => {
            let p: [f64; 2] = serde_json::from_value(coordinates).map_err(bad)?;
            Geom::Point(p[0], p[1])
        }
        "LineString" => {
            Geom::LineString(serde_json::from_value(coordinates).map_err(bad)?)
        }
        "Polygon" => Geom::Polygon(serde_json::from_value(coordinates).map_err(bad)?),
        "MultiPoint" => {
            Geom::MultiPoint(serde_json::from_value(coordinates).map_err(bad)?)
        }
        "MultiLineString" => {
            Geom::MultiLineString(serde_json::from_value(coordinates).map_err(bad)?)
        }
        "MultiPolygon" => {
            Geom::MultiPolygon(serde_json::from_value(coordinates).map_err(bad)?)
        }
        _ => return Err(invalid("unsupported indexed geometry type")),
    })
}

/// The document's geometry at `field`, or `None` for an absent/null value.
/// An empty geometry (zero coordinates) is refused with a clear error rather
/// than silently posting nothing: it has no bounding box, so there is no
/// sound posting to write for it, and PostGIS itself refuses
/// `ST_Envelope`/an empty geography the same way.
pub(super) fn selected_geometry(document: &Value, field: &str) -> Result<Option<Geom>> {
    let Some(value) = document.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let geometry = geom_from_value(value)?;
    if geometry.bbox().is_none() {
        return Err(invalid("indexed geometry must not be empty"));
    }
    Ok(Some(geometry))
}

/// The finest ladder level whose cover of `bbox` fits `MAX_CELLS`, and that
/// cover's cells. `LEVEL_WORLD`'s grid is 1x1, so this never fails.
fn cover_cells_for(bbox: (f64, f64, f64, f64)) -> (u8, Vec<(u32, u32)>) {
    let (xmin, xmax, ymin, ymax) = bbox;
    for level in [
        spatial::LEVEL_FINE,
        spatial::LEVEL_COARSE,
        spatial::LEVEL_WORLD,
    ] {
        if let Some(cells) =
            spatial::cover_cells(xmin, xmax, ymin, ymax, level, spatial::MAX_CELLS)
        {
            return (level, cells);
        }
    }
    unreachable!("LEVEL_WORLD's 1x1 grid always fits MAX_CELLS")
}

/// The bounded posting set for one geometry: at most `MAX_CELLS` (8)
/// entries, one per covered cell at the chosen ladder level, each carrying
/// the SAME value -- the geometry's own outward-rounded bbox. Sorted by key
/// so build and maintain diff identically (and so two calls for the same
/// geometry always compare equal).
pub(super) fn geometry_entries(
    i: &IndexInfo,
    id: EntityId,
    geom: &Geom,
) -> Result<Vec<GeometryEntry>> {
    let (xmin, xmax, ymin, ymax) = geom
        .bbox()
        .ok_or_else(|| invalid("indexed geometry must not be empty"))?;
    let value = BoxF::from_f64(xmin, xmax, ymin, ymax).encode();
    let (level, cells) = cover_cells_for((xmin, xmax, ymin, ymax));
    let mut entries: Vec<GeometryEntry> = cells
        .into_iter()
        .map(|(cx, cy)| GeometryEntry {
            key: posting_key_at(
                i.id,
                level,
                spatial::cell_hilbert(cx, cy, level) as u32,
                id.sequence,
            ),
            value,
        })
        .collect();
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(entries)
}

/// Derive one entity's posting set from an immutable dense-v3 row, without
/// fetching vector sidecars. Mirrors `spatial_indexes::build_point_entry`.
pub(super) fn build_geometry_entries(
    db: &Database,
    i: &IndexInfo,
    id: EntityId,
    row: &[u8],
) -> Result<Vec<GeometryEntry>> {
    descriptor(i)?;
    let layout_id = layout_id(row)?;
    let layout = db.layout(layout_id)?;
    if let Some((_, historical_kind)) = layout.fields.iter().find(|(name, _)| name == &i.field) {
        if historical_kind != &Kind::Geo {
            return Err(invalid("historical indexed geometry field changed kind"));
        }
    }
    let document = crate::dense_v3::decode_with_vector_values(&layout, row, |_, _| Ok(None))
        .map_err(corrupt)?;
    match selected_geometry(&document, &i.field)? {
        Some(geometry) => geometry_entries(i, id, &geometry),
        None => Ok(Vec::new()),
    }
}

/// Maintain one entity's geometry postings inside the caller's entity
/// transaction: every posting the OLD geometry wrote that the NEW geometry
/// does not still write is retired here, in the SAME transaction as the row
/// -- so a later key-only scan can trust a surviving posting exactly as
/// `spatial_indexes::maintain_point` relies on for points. Invalid new
/// geometry (including an empty one) is rejected before the primary row is
/// published, via `selected_geometry`'s error propagating up through
/// `maintain_indexes`.
pub(super) fn maintain_geometry(
    db: &mut Database,
    i: &mut IndexInfo,
    id: EntityId,
    old: Option<&Value>,
    new: Option<&Value>,
) -> Result<()> {
    descriptor(i)?;
    let old_geometry = old
        .map(|document| selected_geometry(document, &i.field))
        .transpose()?
        .flatten();
    let new_geometry = new
        .map(|document| selected_geometry(document, &i.field))
        .transpose()?
        .flatten();
    let old_entries = match &old_geometry {
        Some(g) => geometry_entries(i, id, g)?,
        None => Vec::new(),
    };
    let new_entries = match &new_geometry {
        Some(g) => geometry_entries(i, id, g)?,
        None => Vec::new(),
    };
    if old_entries == new_entries {
        return Ok(());
    }
    // Both lists are sorted by key (`geometry_entries` guarantees it): a
    // linear merge finds exactly the keys to delete (in old, not in new),
    // the keys to insert (in new, not in old) and the keys held in both
    // (left untouched, since their value never differs -- the geometry's
    // bbox is the same value at every cell it posts to).
    let (mut oi, mut ni) = (0usize, 0usize);
    while oi < old_entries.len() || ni < new_entries.len() {
        match (old_entries.get(oi), new_entries.get(ni)) {
            (Some(o), Some(n)) if o.key == n.key => {
                oi += 1;
                ni += 1;
            }
            (Some(o), Some(n)) if o.key < n.key => {
                db.index_delete(i, &o.key)?;
                oi += 1;
            }
            (Some(_), Some(n)) => {
                db.index_put(i, &n.key, &n.value)?;
                ni += 1;
            }
            (Some(o), None) => {
                db.index_delete(i, &o.key)?;
                oi += 1;
            }
            (None, Some(n)) => {
                db.index_put(i, &n.key, &n.value)?;
                ni += 1;
            }
            (None, None) => unreachable!(),
        }
    }
    Ok(())
}

/// Decode one posting: `(level, cell, sequence, bbox)`. `level` is checked
/// against the fixed ladder; `cell` and `bbox` are returned as stored -- a
/// geometry's bbox spans several cells, so unlike a point posting there is
/// no cheap self-consistency check between `cell` and `bbox` to make here.
/// The verifier instead recomputes the whole posting set from the primary
/// row and compares it byte-for-byte (`index_verifier::verify_actual`).
pub(super) fn decode_posting(
    prefix: &[u8],
    key: &[u8],
    value: &[u8],
) -> Result<(u8, u32, u64, BoxF)> {
    if !key.starts_with(prefix) || key.len() < prefix.len() + 5 {
        return Err(corrupt("spatial geometry posting key"));
    }
    let level = key[prefix.len()];
    if !matches!(
        level,
        spatial::LEVEL_FINE | spatial::LEVEL_COARSE | spatial::LEVEL_WORLD
    ) {
        return Err(corrupt("spatial geometry posting level"));
    }
    let cell = u32::from_be_bytes(key[prefix.len() + 1..prefix.len() + 5].try_into().unwrap());
    let mut at = prefix.len() + 5;
    let sequence = read_ordered(key, &mut at)?;
    if at != key.len() || sequence == 0 {
        return Err(corrupt("spatial geometry posting identity"));
    }
    let bbox = BoxF::decode(value).ok_or_else(|| corrupt("spatial geometry posting value"))?;
    Ok((level, cell, sequence, bbox))
}

impl Database {
    pub fn create_geometry_index(
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
        if kind != Kind::Geo {
            return Err(invalid("spatial geometry index requires a Geo field"));
        }
        self.create_index(
            collection,
            name,
            field,
            kind,
            false,
            IndexFamily::SpatialGeometry,
            GEOMETRY_FEATURE,
        )
    }

    fn ready_geometry_index(&self, id: IndexId) -> Result<IndexInfo> {
        let index = self.index_info(id)?;
        if index.family != IndexFamily::SpatialGeometry {
            return Err(invalid("index is not a spatial geometry index"));
        }
        descriptor(&index)?;
        if index.state != IndexState::Ready {
            return Err(invalid("index is not ready"));
        }
        Ok(index)
    }

    /// Every entity whose posted bbox overlaps `query`, found by an
    /// EXHAUSTIVE scan of every posting at every ladder level -- no range
    /// pruning by cell. This is the oracle a bounded, level-aware query
    /// (item X1b) will be checked against, not a query path of its own:
    /// `pub(crate)`, no public API, and its cost is the index's own size
    /// rather than the query's selectivity (Law 1 does not apply to a test
    /// oracle the way it applies to the build or a real query).
    pub(crate) fn geometry_bbox_overlap(&self, id: IndexId, query: BoxF) -> Result<Vec<EntityId>> {
        let index = self.ready_geometry_index(id)?;
        let prefix = posting_prefix(index.id);
        let mut hits = std::collections::BTreeSet::new();
        for row in self.index_range(&index, &prefix)?.into_iter().flatten() {
            let (key, value) = row?;
            if !key.starts_with(&prefix) {
                break;
            }
            let (_, _, sequence, bbox) = decode_posting(&prefix, &key, &value)?;
            if bbox.intersects(&query) {
                hits.insert(EntityId {
                    collection: index.collection,
                    sequence,
                });
            }
        }
        Ok(hits.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::{
        io::IoMode,
        store::{Config, SyncMode},
    };
    use serde_json::json;

    fn cfg() -> Config {
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        }
    }

    /// DO(7): `geometry_bbox_overlap` (the oracle a bounded, level-aware
    /// query will be checked against) must return exactly the entities whose
    /// bbox overlaps the query, against an independent brute-force
    /// computation over the fixture -- not against this file's own cover/
    /// posting logic, so a bug shared between the index and the "oracle"
    /// cannot hide from this test.
    #[test]
    fn geometry_bbox_overlap_matches_an_independent_brute_force_oracle() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let mut db = Database::create(&path, cfg()).unwrap();
        let collection = db
            .create_collection(
                "shapes",
                vec![("shape".into(), Kind::Geo)],
                CollectionOptions::default(),
            )
            .unwrap();
        let fixture: Vec<(&str, Geom)> = vec![
            ("a", Geom::Point(0.0, 0.0)),
            ("b", Geom::Point(5.0, 5.0)),
            (
                "c",
                Geom::Polygon(vec![vec![
                    [1.0, 1.0],
                    [3.0, 1.0],
                    [3.0, 3.0],
                    [1.0, 3.0],
                    [1.0, 1.0],
                ]]),
            ),
            (
                "d",
                Geom::LineString(vec![[10.0, 10.0], [11.0, 12.0], [12.0, 10.5]]),
            ),
            (
                "e",
                Geom::MultiPolygon(vec![
                    vec![vec![[-5.0, -5.0], [-4.0, -5.0], [-4.0, -4.0], [-5.0, -4.0], [-5.0, -5.0]]],
                    vec![vec![[20.0, 20.0], [21.0, 20.0], [21.0, 21.0], [20.0, 21.0], [20.0, 20.0]]],
                ]),
            ),
        ];
        let mut boxes = Vec::new();
        for (key, geom) in &fixture {
            let value = json!({ "type": geo_type(geom), "coordinates": geo_coordinates(geom) });
            let id = db.put(collection, key, &json!({"shape": value})).unwrap();
            let (xmin, xmax, ymin, ymax) = geom.bbox().unwrap();
            boxes.push((id, BoxF::from_f64(xmin, xmax, ymin, ymax)));
        }
        db.commit().unwrap();
        let index = db
            .create_geometry_index(collection, "by_shape", "shape")
            .unwrap();
        db.build_index_to_ready(index, 8).unwrap();
        db.commit().unwrap();

        let queries = [
            BoxF::from_f64(-1.0, 1.0, -1.0, 1.0),
            BoxF::from_f64(0.0, 4.0, 0.0, 4.0),
            BoxF::from_f64(9.0, 13.0, 9.0, 13.0),
            BoxF::from_f64(-180.0, 180.0, -90.0, 90.0),
            BoxF::from_f64(50.0, 60.0, 50.0, 60.0),
        ];
        for query in queries {
            let mut expected: Vec<_> = boxes
                .iter()
                .filter(|(_, bbox)| bbox.intersects(&query))
                .map(|(id, _)| *id)
                .collect();
            expected.sort();
            let actual = db.geometry_bbox_overlap(index, query).unwrap();
            assert_eq!(actual, expected, "mismatch for query {query:?}");
        }
    }

    fn geo_type(g: &Geom) -> &'static str {
        match g {
            Geom::Point(..) => "Point",
            Geom::LineString(_) => "LineString",
            Geom::Polygon(_) => "Polygon",
            Geom::MultiPoint(_) => "MultiPoint",
            Geom::MultiLineString(_) => "MultiLineString",
            Geom::MultiPolygon(_) => "MultiPolygon",
        }
    }
    fn geo_coordinates(g: &Geom) -> serde_json::Value {
        match g {
            Geom::Point(x, y) => json!([x, y]),
            Geom::LineString(c) | Geom::MultiPoint(c) => json!(c),
            Geom::Polygon(rs) | Geom::MultiLineString(rs) => json!(rs),
            Geom::MultiPolygon(ps) => json!(ps),
        }
    }
}
