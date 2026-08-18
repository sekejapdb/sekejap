//! `ST_DWithin` against distances computed in the test.
//!
//! This is an oracle rather than an agreement check: the expected answer is
//! worked out here, independently of the engine, and compared with what the query
//! returns. The other fuzzers in this suite know only that two paths must match —
//! useful, but blind to a fault both paths share.
//!
//! sekejap's spatial predicates use PostGIS `geography` semantics: metres on the
//! WGS84 ellipsoid. Reproducing Vincenty here would be reimplementing the thing
//! under test, so instead the fixture places points at distances far from the
//! radius being asked about — tens of kilometres either side — and the oracle is
//! the haversine great-circle distance, which agrees with the ellipsoid to well
//! under a percent. Anything that disagrees at that separation is wrong by
//! kilometres, not by the choice of earth model.

use sekejap::CoreDB;
use serde_json::json;

/// Great-circle distance in metres. Deliberately the simple formula: it is the
/// independent check, and it only has to be right to a fraction of a percent.
fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6_371_008.8; // mean earth radius, metres
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().asin()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// A value in `-1.0..1.0`.
    fn signed(&mut self) -> f64 { (self.next() % 20_001) as f64 / 10_000.0 - 1.0 }
}

const CENTRE: (f64, f64) = (-37.8102, 144.9631);   // (lat, lon)

fn fixture(dir: &std::path::Path, seed: u64) -> Vec<(String, f64, f64)> {
    let mut rng = Rng(seed);
    let mut db = CoreDB::open(dir).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, geometry GEO)").unwrap();
    let mut points = Vec::new();
    for i in 0..120 {
        // Spread over roughly ±1 degree — about ±110 km — so the radii below
        // have plenty of clear space either side of them.
        let lat = CENTRE.0 + rng.signed();
        let lon = CENTRE.1 + rng.signed();
        let key = format!("n{i}");
        db.put(&format!("p/{key}"), &json!({
            "_collection": "p", "_key": key,
            "geometry": {"type": "Point", "coordinates": [lon, lat]},
        }).to_string()).unwrap();
        points.push((key, lat, lon));
    }
    db.compact().unwrap();
    db.build_spatial_index();
    points
}

#[test]
fn st_dwithin_returns_what_the_distances_say() {
    let dir = tempfile::TempDir::new().unwrap();
    let points = fixture(dir.path(), 0x59A71A1);
    let db = CoreDB::open(dir.path()).unwrap();

    let mut checked = 0;
    // Chosen against the spread: the points cover roughly a 220 x 175 km box, so
    // a 5 km radius contains nothing and tests nothing. The guard at the end of
    // the loop is what caught that — it refuses a radius with fewer than three
    // rows unambiguously inside, rather than passing on an empty comparison.
    for radius_km in [30.0f64, 60.0, 90.0, 120.0] {
        let radius_m = radius_km * 1_000.0;
        // Rows the oracle says are inside, ignoring any that sit within 3 km of
        // the boundary — that band is where the earth model matters and this test
        // deliberately has no opinion.
        let mut want: Vec<String> = Vec::new();
        let mut ambiguous = 0;
        for (key, lat, lon) in &points {
            let d = haversine_m(CENTRE.0, CENTRE.1, *lat, *lon);
            if (d - radius_m).abs() < 3_000.0 { ambiguous += 1; continue }
            if d < radius_m { want.push(format!("p/{key}")) }
        }
        want.sort();

        let sql = format!(
            "SELECT _key FROM p WHERE ST_DWithin(geometry, POINT({} {}), {radius_m})",
            CENTRE.1, CENTRE.0);
        let mut got: Vec<String> = db.query(&sql)
            .unwrap_or_else(|e| panic!("`{sql}` did not run: {e:?}"))
            .collect().iter().map(|h| h.slug.clone()).collect();
        got.sort();

        // Everything the oracle is sure about must be there, and nothing the
        // oracle is sure is outside may be.
        for k in &want {
            assert!(got.contains(k),
                "{radius_km} km: {k} is {:.0} m from the centre and was not returned",
                points.iter().find(|(p, _, _)| format!("p/{p}") == *k)
                    .map(|(_, la, lo)| haversine_m(CENTRE.0, CENTRE.1, *la, *lo)).unwrap_or(0.0));
        }
        for k in &got {
            let d = points.iter().find(|(p, _, _)| format!("p/{p}") == *k)
                .map(|(_, la, lo)| haversine_m(CENTRE.0, CENTRE.1, *la, *lo))
                .unwrap_or(f64::MAX);
            assert!(d < radius_m + 3_000.0,
                "{radius_km} km: {k} was returned but is {d:.0} m away");
        }
        assert!(want.len() >= 3,
            "{radius_km} km: only {} rows are unambiguously inside ({ambiguous} near the \
             boundary) — this radius is not testing anything", want.len());
        checked += 1;
    }
    assert_eq!(checked, 4);
}

/// `ST_DISTANCE` ordering must agree with the distances too — the k nearest by
/// the engine are the k nearest by the oracle.
#[test]
fn st_distance_orders_by_actual_distance() {
    let dir = tempfile::TempDir::new().unwrap();
    let points = fixture(dir.path(), 0xD157);
    let db = CoreDB::open(dir.path()).unwrap();

    let mut by_distance: Vec<(String, f64)> = points.iter()
        .map(|(k, la, lo)| (format!("p/{k}"), haversine_m(CENTRE.0, CENTRE.1, *la, *lo)))
        .collect();
    by_distance.sort_by(|a, b| a.1.total_cmp(&b.1));

    let sql = format!(
        "SELECT _key FROM p ORDER BY ST_DISTANCE(geometry, POINT({} {})) ASC LIMIT 8",
        CENTRE.1, CENTRE.0);
    let got: Vec<String> = db.query(&sql).unwrap().collect()
        .iter().map(|h| h.slug.clone()).collect();

    assert_eq!(got.len(), 8, "expected 8 nearest, got {}", got.len());
    let want: Vec<String> = by_distance.iter().take(8).map(|(k, _)| k.clone()).collect();
    assert_eq!(got, want,
        "the engine's eight nearest are not the eight nearest by distance\n  \
         engine = {got:?}\n  oracle = {want:?}");
}
