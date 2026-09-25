//! `GeometryFilter::Overlaps`, the `&&` operator: planar bounding boxes
//! intersect, answered from the geometry index and checked against a
//! brute-force oracle over every stored shape.
//!
//! The oracle is `spatial_geometry::bbox_overlaps`, which
//! `tests/spatial_io.rs` pins to PostGIS's own `&&` answers. What this file
//! adds is that the INDEX walk returns exactly the rows the oracle admits --
//! none missed at an edge, none extra -- and that it reads candidates from
//! the index rather than walking the collection.
use sekejap_core::{
    collections::{
        CandidateDriver, CollectionOptions, Database, EntityId, Geom, GeometryFilter, Projection,
        QueryBudget, QueryFilter, QueryOrder, QueryRequest,
    },
    spatial_geometry, Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn geom_json(g: &Geom) -> Value {
    match g {
        Geom::Point(x, y) => json!({"type": "Point", "coordinates": [x, y]}),
        Geom::LineString(c) => json!({"type": "LineString", "coordinates": c}),
        Geom::Polygon(rs) => json!({"type": "Polygon", "coordinates": rs}),
        Geom::MultiPoint(c) => json!({"type": "MultiPoint", "coordinates": c}),
        Geom::MultiLineString(rs) => json!({"type": "MultiLineString", "coordinates": rs}),
        Geom::MultiPolygon(ps) => json!({"type": "MultiPolygon", "coordinates": ps}),
    }
}

fn envelope(w: f64, s: f64, e: f64, n: f64) -> Geom {
    Geom::Polygon(vec![vec![[w, s], [e, s], [e, n], [w, n], [w, s]]])
}

/// Shapes on a grid, of every kind, plus the edge cases `&&` is defined by:
/// a point exactly on an envelope edge, a point a hair past it, a line whose
/// planar box spans the map, and a shape at the corner of the world.
fn shapes() -> Vec<Geom> {
    let mut out = Vec::new();
    for i in 0..12 {
        for j in 0..12 {
            let (x, y) = (f64::from(i) * 2.0 - 12.0, f64::from(j) * 2.0 - 12.0);
            out.push(match (i + j) % 4 {
                0 => Geom::Point(x, y),
                1 => Geom::LineString(vec![[x, y], [x + 0.7, y + 0.3]]),
                2 => envelope(x, y, x + 0.5, y + 0.5),
                _ => Geom::MultiPoint(vec![[x, y], [x + 1.5, y - 0.5]]),
            });
        }
    }
    out.push(Geom::Point(1.0, 0.5));
    out.push(Geom::Point(1.000_000_000_1, 0.5));
    out.push(Geom::Point(1.001, 0.5));
    out.push(Geom::LineString(vec![[170.0, 0.0], [-170.0, 1.0]]));
    out.push(envelope(-180.0, -90.0, -179.0, -89.0));
    out
}

#[test]
fn overlaps_returns_exactly_the_rows_whose_box_meets_the_envelope() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection(
            "shapes",
            vec![("shape".into(), Kind::Geo)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    let mut stored: Vec<(EntityId, Geom)> = Vec::new();
    for (at, shape) in shapes().into_iter().enumerate() {
        let id = db
            .put(c, &format!("s{at:04}"), &json!({"shape": geom_json(&shape)}))
            .unwrap();
        stored.push((id, shape));
    }
    db.commit().unwrap();
    let index = db.create_geometry_index(c, "by_shape", "shape").unwrap();
    db.build_index_to_ready(index, 64).unwrap();
    db.commit().unwrap();

    let windows = [
        envelope(0.0, 0.0, 1.0, 1.0),
        envelope(-3.0, -3.0, 3.0, 3.0),
        envelope(-0.1, -0.1, 0.1, 0.1),
        envelope(100.0, 40.0, 120.0, 50.0),
        envelope(-180.0, -90.0, 180.0, 90.0),
        envelope(-180.0, -90.0, -179.5, -89.5),
        Geom::Point(4.0, 4.0),
        Geom::LineString(vec![[-5.0, -5.0], [-4.0, 6.0]]),
    ];
    let mut checked = 0;
    for window in windows {
        let mut expected: Vec<EntityId> = stored
            .iter()
            .filter(|(_, g)| spatial_geometry::bbox_overlaps(g, &window))
            .map(|(id, _)| *id)
            .collect();
        expected.sort();
        let filter = QueryFilter::Geometry {
            index,
            predicate: GeometryFilter::Overlaps(window.clone()),
        };
        let mut prepared = db
            .prepare_query(QueryRequest {
                collection: c,
                filters: &[filter],
                order: QueryOrder::EntityId,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .unwrap();
        let page = prepared
            .next_page(8192, QueryBudget::unlimited(), || false)
            .unwrap();
        let mut got: Vec<EntityId> = page.rows.iter().map(|r| r.id).collect();
        got.sort();
        assert_eq!(got, expected, "{window:?}");
        checked += expected.len();
        // A small window reads candidates from the index, not the whole
        // collection. The world window is the one that may read them all.
        if !matches!(&window, Geom::Polygon(r) if r[0][0] == [-180.0, -90.0] && r[0][2] == [180.0, 90.0])
        {
            assert!(
                page.work.candidates < stored.len() as u64 / 2,
                "{window:?} read {} candidates of {}",
                page.work.candidates,
                stored.len()
            );
        }
    }
    assert!(checked > 20, "the windows must admit rows, or the test proves nothing");
}

/// The float4 edge: a point one ten-billionth of a degree past the envelope
/// overlaps it, exactly as PostGIS answers, and one a thousandth past does
/// not.
#[test]
fn a_point_a_hair_past_the_edge_overlaps_and_one_a_thousandth_past_does_not() {
    let window = envelope(0.0, 0.0, 1.0, 1.0);
    assert!(spatial_geometry::bbox_overlaps(&Geom::Point(1.0, 0.5), &window));
    assert!(spatial_geometry::bbox_overlaps(&Geom::Point(1.000_000_000_1, 0.5), &window));
    assert!(!spatial_geometry::bbox_overlaps(&Geom::Point(1.001, 0.5), &window));
}
