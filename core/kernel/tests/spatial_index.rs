//! 2i: the spatial index vs brute-force oracles with independent math.
//! Oracle distance = its own haversine (a different formula family from
//! the engine's Vincenty; assertions use safe margins so formula skew
//! can never flip a verdict).

use kernel::graph::Graph;
use kernel::io::IoMode;
use kernel::spatial::Geom;
use kernel::store::{Config, Store, SyncMode};

fn cfg() -> Config {
    Config { budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
}

const F: u64 = 1;

fn hav_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6_371_000.0;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dp, dl) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn uni(&mut self) -> f64 { (self.next() % 1_000_000) as f64 / 1_000_000.0 }
}

/// 3000 points scattered over a ~2x2 degree region (city scale).
fn points(seed: u64) -> Vec<(u64, f64, f64)> {
    let mut r = Rng(seed);
    (1..=3000u64).map(|i| {
        (i, -38.5 + r.uni() * 2.0, 144.0 + r.uni() * 2.0) // (id, lat, lon)
    }).collect()
}

#[test]
fn radius_query_agrees_with_bruteforce() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    let pts = points(0x5EED);
    for (id, lat, lon) in &pts {
        g.set_geo(F, *id, &Geom::Point(*lon, *lat)).unwrap();
    }
    g.commit().unwrap();

    for (qlat, qlon, radius) in [(-37.9, 145.1, 5_000.0), (-38.0, 144.5, 20_000.0),
                                 (-37.6, 145.9, 2_000.0), (-38.4, 144.1, 50_000.0)] {
        let got: std::collections::HashSet<u64> =
            g.within_radius(F, qlat, qlon, radius, 10_000).unwrap()
                .into_iter().map(|(id, _)| id).collect();
        // margin guard: haversine vs vincenty differ < 0.6%; require the
        // oracle only for points clearly inside/outside that band.
        for (id, lat, lon) in &pts {
            let d = hav_m(qlat, qlon, *lat, *lon);
            if d < radius * 0.99 {
                assert!(got.contains(id),
                        "id {id} at {d:.0}m missing from {radius:.0}m radius");
            }
            if d > radius * 1.01 {
                assert!(!got.contains(id),
                        "id {id} at {d:.0}m wrongly inside {radius:.0}m radius");
            }
        }
    }
}

#[test]
fn knn_matches_bruteforce_ranking() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    let pts = points(0xCAFE);
    for (id, lat, lon) in &pts {
        g.set_geo(F, *id, &Geom::Point(*lon, *lat)).unwrap();
    }
    g.commit().unwrap();
    let (qlat, qlon) = (-37.8136, 144.9631);
    let got: Vec<u64> = g.knn_geo(F, qlat, qlon, 10).unwrap()
        .into_iter().map(|(id, _)| id).collect();
    let mut want: Vec<(f64, u64)> = pts.iter()
        .map(|(id, lat, lon)| (hav_m(qlat, qlon, *lat, *lon), *id)).collect();
    want.sort_by(|a, b| a.0.total_cmp(&b.0));
    let want10: Vec<u64> = want.iter().take(10).map(|(_, id)| *id).collect();
    // allow boundary swaps from formula skew only at the tail
    assert_eq!(got[..8], want10[..8], "top-8 must match exactly");
    assert_eq!(got.len(), 10);
}

#[test]
fn polygons_containment_and_update_delete() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    // two adjacent squares (lon lat rings)
    let west = Geom::Polygon(vec![vec![
        [144.90, -37.85], [144.95, -37.85], [144.95, -37.80], [144.90, -37.80], [144.90, -37.85]]]);
    let east = Geom::Polygon(vec![vec![
        [144.95, -37.85], [145.00, -37.85], [145.00, -37.80], [144.95, -37.80], [144.95, -37.85]]]);
    g.set_geo(F, 1, &west).unwrap();
    g.set_geo(F, 2, &east).unwrap();
    g.commit().unwrap();

    assert_eq!(g.contains_point(F, -37.82, 144.92).unwrap(), vec![1]);
    assert_eq!(g.contains_point(F, -37.82, 144.97).unwrap(), vec![2]);
    assert!(g.contains_point(F, -37.90, 144.92).unwrap().is_empty());

    // update: move west out of the way; postings must follow
    g.set_geo(F, 1, &Geom::Point(150.0, -30.0)).unwrap();
    g.commit().unwrap();
    assert!(g.contains_point(F, -37.82, 144.92).unwrap().is_empty(),
            "stale postings after re-set_geo");

    // delete: east vanishes from every query, survives crash replay
    assert!(g.delete_geo(F, 2).unwrap());
    g.commit().unwrap();
    drop(g);
    let g = Graph::new(Store::open(d.path(), cfg()).unwrap()).unwrap();
    assert!(g.contains_point(F, -37.82, 144.97).unwrap().is_empty());
    assert!(g.get_geo(F, 2).unwrap().is_none());
    assert_eq!(g.get_geo(F, 1).unwrap(), Some(Geom::Point(150.0, -30.0)));
}

#[test]
fn big_geometries_fall_to_coarse_and_still_answer() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    // a state-sized polygon (~5 degrees): must post coarse, still be found
    let big = Geom::Polygon(vec![vec![
        [141.0, -39.0], [148.0, -39.0], [148.0, -34.0], [141.0, -34.0], [141.0, -39.0]]]);
    g.set_geo(F, 99, &big).unwrap();
    for i in 1..=50u64 {
        g.set_geo(F, i, &Geom::Point(144.0 + (i as f64) * 0.01, -37.5)).unwrap();
    }
    g.commit().unwrap();
    assert!(g.contains_point(F, -37.0, 145.0).unwrap().contains(&99));
    let bbox_ids = g.in_bbox(F, 144.0, 145.0, -38.0, -37.0).unwrap();
    assert!(bbox_ids.contains(&99), "coarse-level geometry missing from bbox");
    assert!(bbox_ids.len() > 50 / 2, "points missing from bbox");
}

/// Axis order is (lat, lon) everywhere it matters: an ASYMMETRIC point
/// (the coordinates differ wildly) queried from itself must be found at
/// ~0 m, and from its axis-swapped twin must NOT be found. This exists
/// because a lat/lon swap in the point fast path survived the region
/// oracle (its lats and lons were numerically too similar), and because
/// Vincenty fed an out-of-range latitude returned 0.0.
#[test]
fn axis_order_is_lat_lon_everywhere() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    g.set_geo(F, 1, &Geom::Point(50.0, 10.0)).unwrap(); // lon=50, lat=10
    g.set_geo(F, 2, &Geom::Point(10.0, 50.0)).unwrap(); // lon=10, lat=50
    g.commit().unwrap();
    let at_first = g.within_radius(F, 10.0, 50.0, 1_000.0, 5).unwrap();
    assert_eq!(at_first.len(), 1, "exactly the co-located point: {at_first:?}");
    assert_eq!(at_first[0].0, 1);
    assert!(at_first[0].1 < 1.0, "self-distance must be ~0");
    let at_second = g.within_radius(F, 50.0, 10.0, 1_000.0, 5).unwrap();
    assert_eq!(at_second.len(), 1);
    assert_eq!(at_second[0].0, 2);

    // invalid coordinates are refused before the WAL
    assert!(g.set_geo(F, 3, &Geom::Point(200.0, 10.0)).is_err());
    assert!(g.set_geo(F, 3, &Geom::Point(10.0, 95.0)).is_err());
    assert!(g.set_geo(F, 3, &Geom::Point(f64::NAN, 0.0)).is_err());
}

/// A spatial query on a pinned snapshot answers identically while the
/// writer keeps inserting geometries (Law 6 composing with 2i).
#[test]
fn snapshot_readers_see_stable_geo_answers() {
    let d = tempfile::TempDir::new().unwrap();
    let mut w = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    for (id, lat, lon) in points(0xD00D).into_iter().take(500) {
        w.set_geo(F, id, &Geom::Point(lon, lat)).unwrap();
    }
    w.commit().unwrap();
    w.store().checkpoint().unwrap();

    let r = Graph::new(Store::open_snapshot(d.path(), cfg()).unwrap()).unwrap();
    let before = r.within_radius(F, -37.9, 145.0, 30_000.0, 100).unwrap();
    for (id, lat, lon) in points(0xBEEF).into_iter().take(500) {
        w.set_geo(F, id + 10_000, &Geom::Point(lon, lat)).unwrap();
    }
    w.commit().unwrap();
    w.store().checkpoint().unwrap();
    let after = r.within_radius(F, -37.9, 145.0, 30_000.0, 100).unwrap();
    assert_eq!(before, after, "a pinned reader's spatial answers moved");
}
