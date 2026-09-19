//! Pure geometry functions over [`kernel::spatial::Geom`], unit-matched to
//! PostGIS. See `docs/SPATIAL_FUNCTIONS.md` for the full decision table
//! (which PostGIS call each function matches, and why some are planar and
//! some spheroidal). Short version:
//!
//! - `area_m2`, `length_m`, `perimeter_m`, `distance_m`, `centroid`,
//!   `centroid_distance_m`, `intersects`, `dwithin_m` are **spheroidal**
//!   (WGS84 ellipsoid, metres/m²) — they match PostGIS's `::geography` casts.
//! - `within`, `contains`, `covers`, `crosses`, `intersects_planar` are
//!   **planar** (raw lon/lat treated as Cartesian) — PostGIS has no
//!   `geography` overload for `ST_Within`/`ST_Contains`/`ST_Covers`/
//!   `ST_Crosses` at all; it silently casts to `geometry` and answers on the
//!   flat plane, and these match that behaviour exactly.
//!
//! Coordinates throughout are `[lon, lat]` (GeoJSON / `Geom` order), degrees,
//! WGS84. Point-to-point distances use `geographiclib-rs`'s Karney inverse
//! (already an E4 dependency, via [`crate::spatial_math::wgs84_distance_metres`]-
//! style calls) rather than e1's Vincenty inverse, which E4 had already
//! replaced for the same reason: Vincenty fails to converge on a handful of
//! nearly-antipodal pairs (classic case: GeodSolve76) where Karney does not.
//! Point-to-*segment* distances (used when neither endpoint of the nearest
//! pair is a shared vertex) do NOT use e1's analytic spherical cross-track
//! formula — that formula differences great-circle *bearings*, which are
//! numerically unstable near the poles and were caught giving a ~50x-too-small
//! answer on a near-pole fixture case. Instead this does a golden-section
//! search along the great-circle arc between the segment's endpoints,
//! minimizing the exact Karney distance to the query point at each sample —
//! no bearings, no pole singularity, ellipsoid-accurate at convergence.
//!
//! POSTGIS CONFORMANCE, `intersects` (spheroidal). A `geography` edge is the
//! GEODESIC between its two vertices, which bows off the straight lon/lat
//! line through them by up to `BULGE_METRES_PER_DEGREE_SQUARED · span²`
//! metres. `segments_intersect_geodesic` used to test edge crossings on the
//! flat lon/lat plane, which is the `geometry` edge model, not the
//! `geography` one — so on the 50,000-row battle50k corpus E4 and PostGIS
//! disagreed on eleven (query, row) pairs of `plot_intersects`, in both
//! directions. Seven were planar-disjoint pairs the geodesic closes
//! (`ST_Relate` `FF2FF1212`, `ST_Distance(geography)` 0 m): q0/p0029206,
//! q28/p0046799, q35/p0029158, q39/p0007895, q43/p0001572, q43/p0010630,
//! q45/p0018906. Four were planar slivers (down to 1.05 m²,
//! `ST_Relate` `212101212`) the geodesic does not cut, with the two
//! boundaries 0.63 m – 2.18 m apart on the spheroid: q32/p0016989,
//! q32/p0022998, q42/p0018219, q46/p0007969. PostGIS is right on all
//! eleven against this module's own documented semantics; the edge test is
//! now a great-circle arc crossing (`great_circle_arcs_cross`) and all
//! eleven agree. Fixtures: `tests/spatial_geometry_intersects_edge.rs`.
//!
//! Both halves of the test had the same flaw. Fixing only the edge crossing
//! recovered the seven, and left the four: their sliver is cut by a polygon
//! VERTEX, and the vertex-in-ring half (`point_in_ring_geodesic`) was
//! ray-casting over straight lon/lat edges too, placing that vertex inside a
//! ring it is geodesically outside. The two rings differ only in the lens
//! between each straight edge and its own geodesic, so the ray cast is now
//! followed by `ring_lens_parity`, which flips it for each lens the point is
//! actually in — arithmetic-only for every point that is in none, which is
//! every point but a handful.
//!
//! Verified end to end on the battery: `plot_intersects` returns 35,189 rows
//! over the fifty query instances, key for key the same set PostGIS returns,
//! on all fifty; `plot_dwithin_1km` is unchanged at 289, also key for key.
//!
//! POSTGIS CONFORMANCE, loop 2. A 1,020-pair adversarial fixture
//! (`tests/spatial_postgis_conformance.rs`, PostGIS 3.6.4) found four more
//! classes, all outside the city scale the 50K corpus exercises:
//!
//! - A point exactly on a ring's straight CHORD had no planar verdict for
//!   `ring_lens_parity` to correct, so the answer was whatever the
//!   degenerate ray-cast comparisons gave. The midpoint of a
//!   constant-latitude edge is exactly that point, and the sign matters in
//!   both directions: a north edge's geodesic bulges OUT of the ring, so its
//!   chord midpoint is 2.7 cm INSIDE; a south edge's bulges IN, so its chord
//!   midpoint is 2.7 cm OUTSIDE. `point_in_ring_geodesic` now answers such a
//!   point from its side of that edge's own great circle against the ring's
//!   winding.
//! - `point_pair_lower_bound_m` was not a lower bound at long range: a
//!   geodesic 178.8° wide in longitude climbs 78° poleward of its endpoints,
//!   and a bound built on the endpoints' own cosine OVER-states it, so
//!   `nearest_within` pruned the nearest vertex pair of an antimeridian
//!   strip and answered 15,827,449.8 m where PostGIS said 15,744,708.6 m.
//!   `min_lon_scale` now takes the span and uses [`max_bulge_deg`].
//! - `BULGE_METRES_PER_DEGREE_SQUARED` is a small-angle series and stops
//!   bounding the truth near a 30° span (2.45× under at 179.9°), so every
//!   bulge allowance in the module now goes through [`max_bulge_deg`], which
//!   is exact above [`EXACT_GEODESIC_SPAN_DEG`].
//! - A ring with an edge wider than that, or one that encloses a pole, is
//!   not answerable by a lon/lat ray cast at all. `point_in_ring_geodesic`
//!   sends those to [`ring_winding_about`], the exact spherical winding,
//!   which also identifies longitude ±180 with no seam case.
//!
//! The one thing loop 2 did NOT change is the edge model. A `geography`
//! edge is the GREAT CIRCLE through its two vertices, with `(lon, lat)` read
//! straight onto a sphere — not the WGS84 geodesic — and only distances
//! between POINTS are spheroidal. `ST_ClosestPoint` on a 117.48°-wide edge
//! returns a point on the great circle to fourteen decimals of a degree
//! (`documented_postgis_edges_are_great_circles`), and replacing
//! [`slerp_lonlat`] with Karney's direct solution moves that answer 39% off
//! the server. `plot_intersects` still returns 35,189 rows and
//! `plot_dwithin_1km` 289, key for key.

use geographiclib_rs::{Geodesic, InverseGeodesic, PolygonArea, Winding};
use kernel::spatial::Geom;

/// Planar-predicate boundary tolerance, in degrees. The fixture's boundary
/// cases (a point placed exactly at a polygon vertex or edge midpoint) are
/// typed as exact `f64` literals, so this only needs to absorb float noise.
const BOUNDARY_EPS_DEG: f64 = 1e-9;
/// "Touching" tolerance for the geodesic overlap test, in metres.
const GEODESIC_TOUCH_EPS_M: f64 = 1e-6;

/// Exact WGS84 ellipsoidal distance in metres (Karney inverse). `(lon, lat)`
/// order, matching `Geom`.
fn geodesic_m(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    Geodesic::wgs84().inverse(lat1, lon1, lat2, lon2)
}

// ── Ring-level geodesic area / perimeter (matches ST_Area/ST_Perimeter(geography)) ──

/// Unsigned geodesic area and perimeter of one ring (`[lon,lat]`, implicitly
/// closed), in m² and metres. Uses `geographiclib`'s `PolygonArea` — the
/// exact ellipsoidal geodesic-polygon algorithm (Karney's), the same family
/// PostGIS's own `ST_Area(geography)` implementation is built on, not e1's
/// authalic-sphere spherical-excess approximation. That approximation
/// matched PostGIS to 1e-6 relative on e1's own (sub-km²) test polygon but
/// drifted to ~2.7e-6 on this module's 4°×4° fixture polygon — the authalic
/// sphere preserves the ellipsoid's *total* surface area, not its local area
/// density, so the approximation's error grows with polygon size. `add_point`
/// takes `(lat, lon)`; `PolygonArea` auto-closes the ring, so it doesn't
/// matter whether the caller's ring already repeats its first vertex.
/// Antimeridian-crossing rings need no special-casing (`LONG_UNROLL`
/// capability tracks crossings internally).
fn ring_area_and_perimeter_m(ring: &[[f64; 2]]) -> (f64, f64) {
    if ring.len() < 3 {
        return (0.0, 0.0);
    }
    let geod = Geodesic::wgs84();
    let mut pa = PolygonArea::new(&geod, Winding::CounterClockwise);
    for p in ring {
        pa.add_point(p[1], p[0]);
    }
    let (perimeter, area, _num) = pa.compute(true);
    (area.abs(), perimeter)
}

/// A single Polygon part's rings: `[outer, hole, hole, ...]`. Area subtracts
/// holes **by ring position**, not by winding: PostGIS `ST_Area(geography)`
/// does the same — verified live: a hole ring re-wound the opposite direction
/// from the outer still subtracts identically (`docs/SPATIAL_FUNCTIONS.md`).
fn polygon_area_m2(rings: &[Vec<[f64; 2]>]) -> f64 {
    let Some(outer) = rings.first() else { return 0.0 };
    let mut area = ring_area_and_perimeter_m(outer).0;
    for hole in &rings[1..] {
        area -= ring_area_and_perimeter_m(hole).0;
    }
    area
}

/// A polygon's perimeter is the sum of **every** ring's boundary (outer +
/// holes) — verified live: `ST_Perimeter` of a polygon-with-hole equals the
/// outer ring's perimeter plus the hole's, not their difference.
fn polygon_perimeter_m(rings: &[Vec<[f64; 2]>]) -> f64 {
    rings.iter().map(|r| ring_area_and_perimeter_m(r).1).sum()
}

/// Polygon parts as ring-sets: one for `Polygon`, one per part of
/// `MultiPolygon`, none otherwise.
fn polygon_ring_sets(g: &Geom) -> Vec<&[Vec<[f64; 2]>]> {
    match g {
        Geom::Polygon(rings) => vec![rings.as_slice()],
        Geom::MultiPolygon(parts) => parts.iter().map(|rs| rs.as_slice()).collect(),
        _ => Vec::new(),
    }
}

fn is_areal(g: &Geom) -> bool {
    matches!(g, Geom::Polygon(_) | Geom::MultiPolygon(_))
}

// ── Flattening ───────────────────────────────────────────────────────────────

/// Every vertex in the geometry, in `Geom`'s native `[lon, lat]` order.
fn all_coords(g: &Geom) -> Vec<[f64; 2]> {
    match g {
        Geom::Point(x, y) => vec![[*x, *y]],
        Geom::LineString(c) | Geom::MultiPoint(c) => c.clone(),
        Geom::Polygon(rs) | Geom::MultiLineString(rs) => rs.iter().flatten().copied().collect(),
        Geom::MultiPolygon(ps) => ps.iter().flatten().flatten().copied().collect(),
    }
}

fn ring_edges(ring: &[[f64; 2]]) -> Vec<([f64; 2], [f64; 2])> {
    let n = ring.len();
    if n < 2 {
        return Vec::new();
    }
    let mut edges: Vec<_> = ring.windows(2).map(|w| (w[0], w[1])).collect();
    if ring.first() != ring.last() {
        edges.push((ring[n - 1], ring[0]));
    }
    edges
}

/// Every line segment: `LineString`/`MultiLineString` edges, plus every
/// `Polygon`/`MultiPolygon` ring's boundary edges (outer and holes — a hole's
/// boundary is part of the geometry's boundary too). Empty for `Point`/`MultiPoint`.
fn all_edges(g: &Geom) -> Vec<([f64; 2], [f64; 2])> {
    match g {
        Geom::Point(..) | Geom::MultiPoint(_) => Vec::new(),
        Geom::LineString(c) => c.windows(2).map(|w| (w[0], w[1])).collect(),
        Geom::MultiLineString(rs) => rs.iter().flat_map(|c| c.windows(2).map(|w| (w[0], w[1]))).collect(),
        Geom::Polygon(rs) => rs.iter().flat_map(|r| ring_edges(r)).collect(),
        Geom::MultiPolygon(ps) => ps.iter().flat_map(|rs| rs.iter().flat_map(|r| ring_edges(r))).collect(),
    }
}

// ── Public measures ──────────────────────────────────────────────────────────

/// `ST_Area(g::geography)` — square metres on the WGS84 ellipsoid. `0.0` for
/// non-areal geometries (Point/LineString/MultiPoint/MultiLineString), never
/// `None` — that's what PostGIS itself returns for those, not `NULL`.
pub fn area_m2(g: &Geom) -> f64 {
    polygon_ring_sets(g).iter().map(|rings| polygon_area_m2(rings)).sum()
}

/// `ST_Perimeter(g::geography)` — metres. `0.0` for non-areal geometries.
pub fn perimeter_m(g: &Geom) -> f64 {
    polygon_ring_sets(g).iter().map(|rings| polygon_perimeter_m(rings)).sum()
}

/// `ST_Length(g::geography)` — metres. `0.0` for Point/Polygon/MultiPoint/
/// MultiPolygon, matching PostGIS (length lives on `ST_Perimeter` for areal
/// geometries, not `ST_Length`).
pub fn length_m(g: &Geom) -> f64 {
    match g {
        Geom::LineString(c) => ring_perimeter_m_open(c),
        Geom::MultiLineString(rs) => rs.iter().map(|c| ring_perimeter_m_open(c)).sum(),
        _ => 0.0,
    }
}

/// Open-path length (no implicit closing edge) — the `LineString` sibling of
/// [`ring_perimeter_m`], which always closes.
fn ring_perimeter_m_open(coords: &[[f64; 2]]) -> f64 {
    coords.windows(2).map(|w| geodesic_m(w[0][0], w[0][1], w[1][0], w[1][1])).sum()
}

/// `ST_Centroid(g::geography)` — `(lon, lat)` degrees. `None` only for a
/// geometry with zero coordinates (empty ring / empty coordinate list) or a
/// `MultiPoint`/`MultiLineString` whose points are exactly antipodal (the
/// spherical mean is undefined there); callers must not panic on either.
///
/// Point/MultiPoint/LineString/MultiLineString centroids are a **spherical**
/// mean (unit-vector average, matching how PostGIS computes a geography
/// centroid on the sphere). Polygon/MultiPolygon centroids use a **planar**
/// area-weighted formula (2D shoelace centroid over raw lon/lat) — a good
/// approximation for city- to region-scale polygons, but not bit-exact to
/// PostGIS's sphere-native algorithm for large or near-polar polygons; see
/// the fixture conformance test and the final report for the observed
/// deviation.
pub fn centroid(g: &Geom) -> Option<(f64, f64)> {
    match g {
        Geom::Point(x, y) => Some((*x, *y)),
        Geom::MultiPoint(c) => spherical_mean_points(c),
        Geom::LineString(c) => line_centroid(c),
        Geom::MultiLineString(rs) => {
            let all: Vec<[f64; 2]> = rs.iter().flatten().copied().collect();
            let segs: Vec<&[[f64; 2]]> = rs.iter().map(|c| c.as_slice()).collect();
            line_centroid_multi(&segs).or_else(|| spherical_mean_points(&all))
        }
        Geom::Polygon(rings) => polygon_planar_centroid(rings),
        Geom::MultiPolygon(ps) => multipolygon_centroid(ps),
    }
}

/// `ST_Distance(ST_Centroid(a::geography)::geography, ST_Centroid(b::geography)::geography)`.
/// `None` if either centroid is undefined (see [`centroid`]).
pub fn centroid_distance_m(a: &Geom, b: &Geom) -> Option<f64> {
    let (alon, alat) = centroid(a)?;
    let (blon, blat) = centroid(b)?;
    Some(geodesic_m(alon, alat, blon, blat))
}

/// `(lon, lat)` degrees to a unit vector on the sphere.
fn lonlat_to_vec(lon: f64, lat: f64) -> (f64, f64, f64) {
    let (lo, la) = (lon.to_radians(), lat.to_radians());
    (la.cos() * lo.cos(), la.cos() * lo.sin(), la.sin())
}

fn spherical_mean_points(points: &[[f64; 2]]) -> Option<(f64, f64)> {
    if points.is_empty() {
        return None;
    }
    let (mut x, mut y, mut z) = (0.0, 0.0, 0.0);
    for p in points {
        let (vx, vy, vz) = lonlat_to_vec(p[0], p[1]);
        x += vx;
        y += vy;
        z += vz;
    }
    vector_to_lonlat(x, y, z)
}

fn vector_to_lonlat(x: f64, y: f64, z: f64) -> Option<(f64, f64)> {
    let norm = (x * x + y * y + z * z).sqrt();
    if norm < 1e-15 {
        return None; // antipodal cancellation: undefined mean
    }
    Some(((y / norm).atan2(x / norm).to_degrees(), (z / norm).asin().to_degrees()))
}

/// Length-weighted spherical mean of a single line's segment midpoints.
fn line_centroid(coords: &[[f64; 2]]) -> Option<(f64, f64)> {
    line_centroid_multi(&[coords]).or_else(|| coords.first().map(|p| (p[0], p[1])))
}

/// Length-weighted spherical mean across every segment of every part (for
/// `MultiLineString`, so weight is shared correctly across parts). Each
/// segment's midpoint is the *great-circle* midpoint (`normalize(v_a + v_b)`
/// on the unit sphere) — a naive `(lon_a+lon_b)/2` midpoint is wrong for any
/// segment crossing the antimeridian (e.g. 179.5° to −179.5°, whose true
/// midpoint is near 180°, not 0°) since longitude isn't linear there.
fn line_centroid_multi(parts: &[&[[f64; 2]]]) -> Option<(f64, f64)> {
    let (mut acc_x, mut acc_y, mut acc_z, mut total_len) = (0.0, 0.0, 0.0, 0.0);
    for coords in parts {
        for w in coords.windows(2) {
            let seg_len = geodesic_m(w[0][0], w[0][1], w[1][0], w[1][1]);
            if seg_len < 1e-9 {
                continue;
            }
            let (x0, y0, z0) = lonlat_to_vec(w[0][0], w[0][1]);
            let (x1, y1, z1) = lonlat_to_vec(w[1][0], w[1][1]);
            let (mx, my, mz) = (x0 + x1, y0 + y1, z0 + z1);
            let norm = (mx * mx + my * my + mz * mz).sqrt();
            if norm < 1e-15 {
                continue; // antipodal segment endpoints: midpoint undefined
            }
            acc_x += seg_len * mx / norm;
            acc_y += seg_len * my / norm;
            acc_z += seg_len * mz / norm;
            total_len += seg_len;
        }
    }
    if total_len < 1e-9 {
        return None;
    }
    vector_to_lonlat(acc_x, acc_y, acc_z)
}

/// Signed planar shoelace centroid and signed area of one ring.
fn ring_centroid_and_signed_area(ring: &[[f64; 2]]) -> (f64, f64, f64) {
    let n = ring.len();
    if n < 3 {
        let (sx, sy) = ring.iter().fold((0.0, 0.0), |(sx, sy), p| (sx + p[0], sy + p[1]));
        let n = n.max(1) as f64;
        return (sx / n, sy / n, 0.0);
    }
    let (mut a, mut cx, mut cy) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let j = (i + 1) % n;
        let (x0, y0) = (ring[i][0], ring[i][1]);
        let (x1, y1) = (ring[j][0], ring[j][1]);
        let cross = x0 * y1 - x1 * y0;
        a += cross;
        cx += (x0 + x1) * cross;
        cy += (y0 + y1) * cross;
    }
    let signed_area = a / 2.0;
    if signed_area.abs() < 1e-18 {
        let n = n as f64;
        let (sx, sy) = ring.iter().fold((0.0, 0.0), |(sx, sy), p| (sx + p[0], sy + p[1]));
        return (sx / n, sy / n, 0.0);
    }
    (cx / (6.0 * signed_area), cy / (6.0 * signed_area), signed_area)
}

/// `ring_centroid_and_signed_area`, but with every longitude first expressed
/// relative to `ref_lon` (via [`wrap180`]) — so a ring crossing the
/// antimeridian (e.g. `poly_dateline`) gets a correct *local* centroid
/// instead of one dragged 180° off by naively averaging `179°` with `−179°`.
/// The caller un-rotates the result.
fn ring_centroid_and_signed_area_rel(ring: &[[f64; 2]], ref_lon: f64) -> (f64, f64, f64) {
    let shifted: Vec<[f64; 2]> = ring.iter().map(|p| [wrap180(p[0] - ref_lon), p[1]]).collect();
    ring_centroid_and_signed_area(&shifted)
}

fn polygon_planar_centroid(rings: &[Vec<[f64; 2]>]) -> Option<(f64, f64)> {
    let outer = rings.first()?;
    let ref_lon = outer.first()?[0];
    let (ocx, ocy, oa) = ring_centroid_and_signed_area_rel(outer, ref_lon);
    let oa_abs = oa.abs();
    if rings.len() == 1 || oa_abs < 1e-18 {
        return Some((wrap180(ocx + ref_lon), ocy));
    }
    let (mut wx, mut wy, mut wtotal) = (ocx * oa_abs, ocy * oa_abs, oa_abs);
    for hole in &rings[1..] {
        let (hcx, hcy, ha) = ring_centroid_and_signed_area_rel(hole, ref_lon);
        let ha_abs = ha.abs();
        wx -= hcx * ha_abs;
        wy -= hcy * ha_abs;
        wtotal -= ha_abs;
    }
    if wtotal.abs() < 1e-18 {
        return Some((wrap180(ocx + ref_lon), ocy));
    }
    Some((wrap180(wx / wtotal + ref_lon), wy / wtotal))
}

fn multipolygon_centroid(parts: &[Vec<Vec<[f64; 2]>>]) -> Option<(f64, f64)> {
    if parts.is_empty() {
        return None;
    }
    let (mut wx, mut wy, mut wtotal) = (0.0, 0.0, 0.0);
    let mut fallback: Vec<(f64, f64)> = Vec::new();
    for rings in parts {
        if let Some((cx, cy)) = polygon_planar_centroid(rings) {
            let area = polygon_area_m2(rings).abs();
            wx += cx * area;
            wy += cy * area;
            wtotal += area;
            fallback.push((cx, cy));
        }
    }
    if wtotal > 1e-6 {
        return Some((wx / wtotal, wy / wtotal));
    }
    if fallback.is_empty() {
        return None;
    }
    let n = fallback.len() as f64;
    let (sx, sy) = fallback.iter().fold((0.0, 0.0), |(sx, sy), p| (sx + p.0, sy + p.1));
    Some((sx / n, sy / n))
}

// ── Planar point-in-ring / on-segment (within/contains/covers/crosses) ─────

fn point_on_segment(px: f64, py: f64, a: [f64; 2], b: [f64; 2], eps: f64) -> bool {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    if len2 < 1e-20 {
        return (px - a[0]).abs() < eps && (py - a[1]).abs() < eps;
    }
    let t = ((px - a[0]) * dx + (py - a[1]) * dy) / len2;
    if !(-1e-9..=1.0 + 1e-9).contains(&t) {
        return false;
    }
    let t = t.clamp(0.0, 1.0);
    let (cx, cy) = (a[0] + t * dx, a[1] + t * dy);
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() < eps
}

/// Ray-casting point-in-ring. `boundary_inclusive` additionally checks every
/// edge with [`point_on_segment`] first (a point exactly on the ring counts
/// as "in").
fn point_in_ring(lon: f64, lat: f64, ring: &[[f64; 2]], boundary_inclusive: bool) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    if boundary_inclusive {
        for i in 0..n {
            let j = (i + 1) % n;
            if point_on_segment(lon, lat, ring[i], ring[j], BOUNDARY_EPS_DEG) {
                return true;
            }
        }
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (ring[i][0], ring[i][1]);
        let (xj, yj) = (ring[j][0], ring[j][1]);
        if ((yi > lat) != (yj > lat)) && (lon < (xj - xi) * (lat - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn on_ring_boundary(ring: &[[f64; 2]], lon: f64, lat: f64) -> bool {
    let n = ring.len();
    (0..n).any(|i| point_on_segment(lon, lat, ring[i], ring[(i + 1) % n], BOUNDARY_EPS_DEG))
}

/// Strictly inside `ring`'s interior — NOT the same as `point_in_ring(..,
/// false)` alone: a plain ray-cast is only reliable for interior points.
/// Exactly AT a vertex, its "strictly greater than"/"strictly less than"
/// comparisons can register the vertex as inside by the accident of which
/// edges happen to straddle it (verified: `(0,0)` sitting exactly on a unit
/// square's own corner ray-cast as "inside" under the naive test). Checking
/// the boundary explicitly first removes that ambiguity instead of relying
/// on it resolving favorably.
fn strictly_inside_ring(ring: &[[f64; 2]], lon: f64, lat: f64) -> bool {
    !on_ring_boundary(ring, lon, lat) && point_in_ring(lon, lat, ring, false)
}

fn in_or_on_ring(ring: &[[f64; 2]], lon: f64, lat: f64) -> bool {
    on_ring_boundary(ring, lon, lat) || point_in_ring(lon, lat, ring, false)
}

/// Boundary-inclusive polygon membership (outer ring inclusive, holes'
/// *interiors* excluded but a hole's boundary still counts as covered).
fn covers_point_polygon(rings: &[Vec<[f64; 2]>], lon: f64, lat: f64) -> bool {
    let Some(outer) = rings.first() else { return false };
    if !in_or_on_ring(outer, lon, lat) {
        return false;
    }
    !rings[1..].iter().any(|hole| strictly_inside_ring(hole, lon, lat))
}

/// Strict-interior polygon membership (a point on the outer boundary, or on
/// a hole's boundary, is excluded).
fn contains_point_polygon(rings: &[Vec<[f64; 2]>], lon: f64, lat: f64) -> bool {
    let Some(outer) = rings.first() else { return false };
    if !strictly_inside_ring(outer, lon, lat) {
        return false;
    }
    !rings[1..].iter().any(|hole| in_or_on_ring(hole, lon, lat))
}

fn point_on_polyline(pt: [f64; 2], coords: &[[f64; 2]]) -> bool {
    coords.windows(2).any(|w| point_on_segment(pt[0], pt[1], w[0], w[1], BOUNDARY_EPS_DEG))
}

fn points_equal(a: [f64; 2], b: [f64; 2]) -> bool {
    (a[0] - b[0]).abs() < BOUNDARY_EPS_DEG && (a[1] - b[1]).abs() < BOUNDARY_EPS_DEG
}

/// Is `pt` inside-or-on `b` (planar)? The base primitive for `within`/`covers`.
fn covered_by(pt: [f64; 2], b: &Geom) -> bool {
    match b {
        Geom::Point(x, y) => points_equal(pt, [*x, *y]),
        Geom::MultiPoint(c) => c.iter().any(|q| points_equal(pt, *q)),
        Geom::LineString(c) => point_on_polyline(pt, c),
        Geom::MultiLineString(rs) => rs.iter().any(|c| point_on_polyline(pt, c)),
        Geom::Polygon(rings) => covers_point_polygon(rings, pt[0], pt[1]),
        Geom::MultiPolygon(ps) => ps.iter().any(|rings| covers_point_polygon(rings, pt[0], pt[1])),
    }
}

/// Is `pt` in the strict interior of `b`? Only areal geometries have an
/// interior distinct from their boundary in this module's scope.
fn strictly_interior(pt: [f64; 2], b: &Geom) -> bool {
    match b {
        Geom::Polygon(rings) => contains_point_polygon(rings, pt[0], pt[1]),
        Geom::MultiPolygon(ps) => ps.iter().any(|rings| contains_point_polygon(rings, pt[0], pt[1])),
        _ => false,
    }
}

/// `ST_Within(a::geometry, b::geometry)` — planar. Every point of `a` lies
/// inside-or-on `b`, and (when `b` is areal) at least one point of `a` lies
/// strictly inside `b`'s interior — otherwise `a` merely touches `b`'s
/// boundary, which PostGIS does not count as `Within`.
pub fn within(a: &Geom, b: &Geom) -> bool {
    let coords = all_coords(a);
    if coords.is_empty() || !coords.iter().all(|c| covered_by(*c, b)) {
        return false;
    }
    if is_areal(b) {
        coords.iter().any(|c| strictly_interior(*c, b))
    } else {
        true
    }
}

/// `ST_Contains(a::geometry, b::geometry)` — planar. `contains(a, b) == within(b, a)`.
pub fn contains(a: &Geom, b: &Geom) -> bool {
    within(b, a)
}

/// `ST_Covers(a::geometry, b::geometry)` — planar, boundary-inclusive
/// `contains` (no interior-overlap requirement, so a `b` sitting entirely on
/// `a`'s boundary still counts).
pub fn covers(a: &Geom, b: &Geom) -> bool {
    let coords = all_coords(b);
    !coords.is_empty() && coords.iter().all(|c| covered_by(*c, a))
}

/// Strict planar segment crossing (excludes any endpoint-touching /
/// collinear case) — ported from e1's `segments_intersect`, keeping only the
/// proper-crossing branch.
fn cross(p1: [f64; 2], p2: [f64; 2], p3: [f64; 2]) -> f64 {
    (p2[0] - p1[0]) * (p3[1] - p1[1]) - (p2[1] - p1[1]) * (p3[0] - p1[0])
}

fn segments_cross_transversally(a1: [f64; 2], a2: [f64; 2], b1: [f64; 2], b2: [f64; 2]) -> bool {
    let d1 = cross(a1, a2, b1);
    let d2 = cross(a1, a2, b2);
    let d3 = cross(b1, b2, a1);
    let d4 = cross(b1, b2, a2);
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0)) && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

/// Full planar segment intersection (proper crossing OR endpoint/collinear
/// touch) — ported from e1's `segments_intersect`.
fn segments_intersect(a1: [f64; 2], a2: [f64; 2], b1: [f64; 2], b2: [f64; 2]) -> bool {
    if segments_cross_transversally(a1, a2, b1, b2) {
        return true;
    }
    let d1 = cross(a1, a2, b1);
    let d2 = cross(a1, a2, b2);
    let d3 = cross(b1, b2, a1);
    let d4 = cross(b1, b2, a2);
    let on = |a: [f64; 2], b: [f64; 2], p: [f64; 2]| {
        p[0] >= a[0].min(b[0]) && p[0] <= a[0].max(b[0]) && p[1] >= a[1].min(b[1]) && p[1] <= a[1].max(b[1])
    };
    (d1 == 0.0 && on(a1, a2, b1))
        || (d2 == 0.0 && on(a1, a2, b2))
        || (d3 == 0.0 && on(b1, b2, a1))
        || (d4 == 0.0 && on(b1, b2, a2))
}

/// `ST_Crosses(a::geometry, b::geometry)` — planar. Scoped to the practically
/// relevant OGC cases: area/area pairs are always `false` (matches PostGIS —
/// same-dimension inputs essentially never satisfy the DE-9IM crosses
/// pattern), 0-dimensional inputs (`Point`/`MultiPoint`) are always `false`,
/// and line/polygon or line/line pairs are `true` iff some edge of `a`
/// transversally crosses some edge of `b` (a proper penetration, not a mere
/// touch — verified against a line that starts exactly on a polygon's
/// boundary and exits, which PostGIS also scores `false`).
pub fn crosses(a: &Geom, b: &Geom) -> bool {
    if (is_areal(a) && is_areal(b)) || matches!(a, Geom::Point(..) | Geom::MultiPoint(_)) || matches!(b, Geom::Point(..) | Geom::MultiPoint(_)) {
        return false;
    }
    let (ea, eb) = (all_edges(a), all_edges(b));
    ea.iter().any(|(a1, a2)| eb.iter().any(|(b1, b2)| segments_cross_transversally(*a1, *a2, *b1, *b2)))
}

/// `ST_Intersects(a::geometry, b::geometry)` — planar: any vertex of either
/// geometry inside-or-on the other, or any edge pair touching/crossing.
pub fn intersects_planar(a: &Geom, b: &Geom) -> bool {
    let (ca, cb) = (all_coords(a), all_coords(b));
    if ca.iter().any(|c| covered_by(*c, b)) || cb.iter().any(|c| covered_by(*c, a)) {
        return true;
    }
    let (ea, eb) = (all_edges(a), all_edges(b));
    ea.iter().any(|(a1, a2)| eb.iter().any(|(b1, b2)| segments_intersect(*a1, *a2, *b1, *b2)))
}

// ── Geodesic (spheroidal) overlap / intersects / distance ──────────────────

/// Wrap a longitude difference (or absolute longitude) into `(-180, 180]`.
/// The antimeridian-safe primitive: a ring's own edges are always short in
/// longitude once re-expressed relative to one of its own vertices, so
/// rotating into this frame before ray-casting reproduces PostGIS's
/// dateline-aware geography behaviour without any seam special-casing —
/// verified against a live-server point/polygon pair straddling 180°E
/// (`docs/SPATIAL_FUNCTIONS.md`).
fn wrap180(v: f64) -> f64 {
    let mut w = (v + 180.0) % 360.0;
    if w < 0.0 {
        w += 360.0;
    }
    w - 180.0
}

// ── Cheap conservative bounds (the planar stage of the staged predicate) ──
//
// Every exact call in this module -- one Karney inverse per point pair, and
// ~122 of them per point/segment golden-section search -- is now gated by a
// closed-form bound that can only UNDER-state the true geodesic distance.
// The exact routine runs when the bound falls inside a band; outside the
// band the answer is settled by the bound alone. The band is stated at
// `NEAR_BAND_M` together with why it cannot change an answer.

/// Metres in one degree of latitude at its global WGS84 MINIMUM (the
/// equator): the meridional radius of curvature there is `a(1-e²)` =
/// 6,335,439 m, i.e. 110,574 m per degree. Every other latitude is larger,
/// and one degree of longitude at latitude φ is `N cos φ` per radian with
/// `N ≥ a`, i.e. at least `111,319 cos φ` > `110,574 cos φ` metres per
/// degree. So the flat metric `110574 · sqrt(dφ² + cos²φ · dλ²)` (degrees)
/// is pointwise dominated by the true WGS84 metric, and a straight-line
/// distance measured in it is a lower bound on any path's true length.
const MIN_METRES_PER_DEGREE: f64 = 110_574.0;

/// How far a geodesic edge may bulge poleward of the straight lon/lat line
/// between its own endpoints, in metres per (degree of span)². A great
/// circle through two points of longitude span `Δλ` reaches
/// `tan φ_max = tan φ / cos(Δλ/2)`, so the excursion is at most
/// `(Δλ_rad/2)²/4` radians = `0.001091 · Δλ_deg²` degrees, i.e. under
/// `122 · Δλ_deg²` metres. Subtracting it keeps the flat bound below the
/// true distance to the CURVED edge, not merely to its straight chord.
const BULGE_METRES_PER_DEGREE_SQUARED: f64 = 122.0;

/// Metres in one degree of latitude at its global WGS84 MAXIMUM (a pole):
/// the meridional radius of curvature there is `a²/b` = 6,399,594 m, i.e.
/// 111,694 m per degree. Rounded up, so a poleward excursion stated in
/// degrees is never UNDER-stated once converted to metres — the direction a
/// bulge allowance has to err in to stay an allowance.
const MAX_METRES_PER_DEGREE: f64 = 111_700.0;

/// The lon/lat span, in degrees, up to which the quadratic
/// [`BULGE_METRES_PER_DEGREE_SQUARED`] is still an OVER-estimate of the true
/// geodesic bulge, and up to which [`min_lon_scale`]'s one degree of slack
/// still covers that bulge. Above it, both are computed exactly.
///
/// The exact maximum poleward excursion of a great circle whose endpoints
/// are `Δλ` apart in longitude is `atan(1/√c) − atan(√c)` degrees, with
/// `c = cos(Δλ/2)`: maximising `atan(tan φ / c) − φ` over `φ` gives
/// `tan φ = √c`. Against the quadratic `0.001091·Δλ²` rad-series form that
/// is 1.0002× at 5°, 1.0011× at 10°, 1.0114× at 30°, 1.23× at 117°, and
/// 2.45× at 179.9°, so the series stops bounding the truth near 30° (the
/// 122 m/deg² constant carries 1.1% of headroom over it). Five degrees is
/// six-fold margin on that, and is small enough that no ring in a
/// city-scale corpus ever leaves the cheap path: `plot_intersects`'s
/// query rings and plot rings span hundredths of a degree.
const EXACT_GEODESIC_SPAN_DEG: f64 = 5.0;

/// An upper bound, in DEGREES of latitude, on how far a geodesic bows
/// poleward of the straight lon/lat line between its endpoints, for an edge
/// whose lon/lat span is `span` degrees. Quadratic below
/// [`EXACT_GEODESIC_SPAN_DEG`] (and 1.1% over, see there); the exact
/// great-circle maximum above it, so the value stays an upper bound out to a
/// 179.9° span instead of under-stating it by 2.45×.
fn max_bulge_deg(span: f64) -> f64 {
    if span <= EXACT_GEODESIC_SPAN_DEG {
        // `BULGE_METRES_PER_DEGREE_SQUARED / MIN_METRES_PER_DEGREE`, up.
        return 0.0011 * span * span;
    }
    let c = (span.min(180.0).to_radians() / 2.0).cos();
    if c <= 0.0 {
        return 90.0;
    }
    let r = c.sqrt();
    ((1.0 / r).atan() - r.atan()).to_degrees()
}

/// [`max_bulge_deg`] in METRES, at the largest metres-per-degree the
/// ellipsoid has anywhere. Bit-identical to the old `122 · span²` below
/// [`EXACT_GEODESIC_SPAN_DEG`].
fn max_bulge_m(span: f64) -> f64 {
    if span <= EXACT_GEODESIC_SPAN_DEG {
        return BULGE_METRES_PER_DEGREE_SQUARED * span * span;
    }
    MAX_METRES_PER_DEGREE * max_bulge_deg(span)
}

/// The staging band, in metres. Below it the exact spheroidal routine runs;
/// at or above it the cheap bound answers on its own.
///
/// It cannot change an answer because every decision the bound short-circuits
/// is taken at [`GEODESIC_TOUCH_EPS_M`] = 1e-6 m, a MILLION times smaller:
/// the bound never over-states the true distance (see
/// [`MIN_METRES_PER_DEGREE`] and [`BULGE_METRES_PER_DEGREE_SQUARED`]), so a
/// pair whose true distance is under 1e-6 m always bounds under 1e-6 m, is
/// always inside the band, and is always settled by the exact routine. The
/// only thing the band trades is work: a pair between 1e-6 m and 1 m apart
/// pays for an exact answer it did not strictly need.
///
/// `dwithin_m` passes its own radius instead of this constant, for the same
/// reason and with the same guarantee: the bound is a lower bound, so a pair
/// the bound puts beyond the radius is beyond it.
const NEAR_BAND_M: f64 = 1.0;

/// The smallest metres-per-degree-of-longitude factor that can apply
/// anywhere on a path of lon/lat span `span` between these latitudes: `cos`
/// of the poleward extreme, with slack for the geodesic's own poleward
/// bulge -- one degree, or [`max_bulge_deg`] when the span makes that
/// larger. Clamped at zero, which degrades the bound to its latitude term
/// alone -- still a valid lower bound, never an invalid one.
///
/// The slack is not cosmetic. A geodesic between two points 178.8° apart in
/// longitude climbs 78° poleward of them, where a degree of longitude is a
/// fifth of what it is at the endpoints; a "bound" built on the endpoints'
/// own cosine then OVER-states the true distance and
/// [`nearest_within`] prunes the genuinely nearest vertex pair. That is the
/// whole of the 0.5% antimeridian-distance class: E4 reported the distance
/// to a strip's SOUTH vertices (15,827,449.799 m) because the bound had
/// discarded its north ones (15,744,708.609 m, PostGIS's answer) as too far.
fn min_lon_scale(lat_a: f64, lat_b: f64, span: f64) -> f64 {
    let poleward = (lat_a.abs().max(lat_b.abs()) + max_bulge_deg(span).max(1.0)).min(90.0);
    poleward.to_radians().cos().max(0.0)
}

/// A lower bound, in metres, on the true WGS84 geodesic distance between two
/// lon/lat points. Never above the true distance.
fn point_pair_lower_bound_m(a: [f64; 2], b: [f64; 2]) -> f64 {
    let dlat = a[1] - b[1];
    let raw = wrap180(a[0] - b[0]);
    let dlon = raw * min_lon_scale(a[1], b[1], raw.abs().max(dlat.abs()));
    MIN_METRES_PER_DEGREE * dlon.hypot(dlat)
}

/// The flat point-to-segment distance from `p` to `a → b` in the
/// conservative scaled frame (never above the true planar distance), and
/// that edge's own maximum poleward bulge — both in metres.
fn flat_point_segment_and_bulge_m(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> (f64, f64) {
    // Two `wrap180` calls, both reused below: this is the hottest arithmetic
    // in the module (`ring_lens_parity` runs it once per ring edge per
    // candidate) and `wrap180` carries an `%`.
    let prel = wrap180(p[0] - a[0]);
    let brel = wrap180(b[0] - a[0]);
    let edge_span = brel.abs().max((b[1] - a[1]).abs());
    // The span the SCALE has to survive is the whole configuration's: the
    // path whose length is bounded runs from `p` to the edge, so a longitude
    // gap between them bulges exactly as an edge of that span does. `p`'s own
    // reach plus the edge's covers every point of that path without a third
    // `wrap180`.
    let reach = prel.abs().max((p[1] - a[1]).abs()).max((p[1] - b[1]).abs());
    let scale = min_lon_scale(a[1].abs().max(b[1].abs()), p[1], edge_span + reach);
    let px = prel * scale;
    let py = p[1] - a[1];
    let bx = brel * scale;
    let by = b[1] - a[1];
    let len2 = bx * bx + by * by;
    let (dx, dy) = if len2 <= 0.0 {
        (px, py)
    } else {
        let t = ((px * bx + py * by) / len2).clamp(0.0, 1.0);
        (px - t * bx, py - t * by)
    };
    (MIN_METRES_PER_DEGREE * dx.hypot(dy), max_bulge_m(edge_span))
}

/// A lower bound, in metres, on the true WGS84 geodesic distance from `p` to
/// the geodesic edge `a → b`: the flat point-to-segment distance in the
/// conservative scaled frame, less the edge's maximum poleward bulge.
fn point_segment_lower_bound_m(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (flat, bulge) = flat_point_segment_and_bulge_m(p, a, b);
    (flat - bulge).max(0.0)
}

/// [`point_to_segment_geodesic_m`], but only when the cheap bound puts the
/// pair inside `band`; otherwise the bound itself, which is already enough to
/// answer "further away than `band`".
fn point_to_segment_within(p: [f64; 2], a: [f64; 2], b: [f64; 2], band: f64) -> f64 {
    let bound = point_segment_lower_bound_m(p, a, b);
    if bound >= band {
        return bound;
    }
    point_to_segment_geodesic_m(p[0], p[1], a[0], a[1], b[0], b[1])
}

/// [`geodesic_m`], but only when the cheap bound puts the pair inside `band`.
fn point_pair_within(a: [f64; 2], b: [f64; 2], band: f64) -> f64 {
    let bound = point_pair_lower_bound_m(a, b);
    if bound >= band {
        return bound;
    }
    geodesic_m(a[0], a[1], b[0], b[1])
}

// ── Great-circle edge crossing (3D, no trig per test beyond the vertices) ──

fn cross3(u: [f64; 3], v: [f64; 3]) -> [f64; 3] {
    [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ]
}

fn dot3(u: [f64; 3], v: [f64; 3]) -> f64 {
    u[0] * v[0] + u[1] * v[1] + u[2] * v[2]
}

fn unit3(p: [f64; 2]) -> [f64; 3] {
    let (x, y, z) = lonlat_to_vec(p[0], p[1]);
    [x, y, z]
}

fn normalize3(v: [f64; 3]) -> Option<[f64; 3]> {
    let n = dot3(v, v).sqrt();
    if n < 1e-18 {
        None
    } else {
        Some([v[0] / n, v[1] / n, v[2] / n])
    }
}

/// Is the unit vector `p` on the minor arc `u1 → u2` whose plane normal is
/// `n = u1 × u2`? Endpoints count, so a touch is an intersection.
fn on_minor_arc(p: [f64; 3], u1: [f64; 3], u2: [f64; 3], n: [f64; 3]) -> bool {
    dot3(cross3(u1, p), n) >= 0.0 && dot3(cross3(p, u2), n) >= 0.0
}

/// Do the two lon/lat boxes of these edges stay apart even after each edge is
/// allowed its full poleward bulge? Then no crossing is possible and the 3D
/// test is skipped. Longitudes are compared in a frame rotated to `a1`, so
/// the antimeridian needs no special case.
fn edge_boxes_disjoint(a1: [f64; 2], a2: [f64; 2], b1: [f64; 2], b2: [f64; 2]) -> bool {
    let rel = |p: [f64; 2]| [wrap180(p[0] - a1[0]), p[1]];
    let (a1r, a2r, b1r, b2r) = (rel(a1), rel(a2), rel(b1), rel(b2));
    let pad = |p: [f64; 2], q: [f64; 2]| max_bulge_deg((q[0] - p[0]).abs().max((q[1] - p[1]).abs()));
    let (pa, pb) = (pad(a1r, a2r), pad(b1r, b2r));
    let (axl, axh) = (a1r[0].min(a2r[0]) - pa, a1r[0].max(a2r[0]) + pa);
    let (ayl, ayh) = (a1r[1].min(a2r[1]) - pa, a1r[1].max(a2r[1]) + pa);
    let (bxl, bxh) = (b1r[0].min(b2r[0]) - pb, b1r[0].max(b2r[0]) + pb);
    let (byl, byh) = (b1r[1].min(b2r[1]) - pb, b1r[1].max(b2r[1]) + pb);
    axh < bxl || bxh < axl || ayh < byl || byh < ayl
}

/// Do the two GREAT-CIRCLE arcs `a1→a2` and `b1→b2` cross or touch?
///
/// This is the spheroidal edge model PostGIS `geography` uses: an edge
/// between two vertices is the geodesic between them, not the straight
/// lon/lat line. Two great circles meet at an antipodal pair; the crossing
/// exists iff one of that pair lies on both minor arcs. Nearly coplanar
/// circles (`normalize3` failing, or a degenerate zero-length edge) return
/// `false` here and are left to the touch-distance tests in
/// [`geodesic_overlap`], which answer the collinear-overlap case directly.
/// How many of a ring's LENSES does this point fall inside, in parity?
///
/// A ray cast over straight lon/lat edges answers for the wrong ring: a
/// `geography` ring's edges are geodesics. The two rings enclose exactly the
/// same area except in the lens between each straight edge and its own
/// geodesic — a sliver at most `BULGE_METRES_PER_DEGREE_SQUARED · span²`
/// metres wide, bounded by the two curves and closed at the edge's own
/// endpoints. A point inside an odd number of those lenses is on the
/// opposite side of the true ring from the planar answer, and a point inside
/// none is classified identically by both, so the planar ray cast plus this
/// parity IS the geodesic answer.
///
/// It is the cheap way round: the correction walks the ring once with a flat
/// distance test that rejects every edge the point is not already within a
/// few metres of, so a point anywhere but in a lens pays only arithmetic —
/// where projecting the whole ring gnomonically about the point (the
/// straightforward exact route) would pay four transcendentals per ring
/// vertex for every candidate the walk ever looked at.
///
/// This is what closed the four remaining `plot_intersects` rows: PostGIS
/// scored a planar sliver of 1.05 m² as no intersection at all, because the
/// polygon vertex cutting it lies in the lens of the query ring's edge and
/// is OUTSIDE the geodesic ring even though it is inside the straight one.
///
/// The decomposition needs the point to be OFF the straight ring, because
/// it corrects a planar verdict and a point on the straight boundary has
/// none: the ray cast's answer there is whatever the degenerate comparisons
/// happen to give. That is not a corner case — it is every "point at the
/// arithmetic midpoint of a constant-latitude edge" the fixture asks about.
/// So the second return value carries the geodesic side of the chord the
/// point is ON (`dot3(p, n)` for that edge's great circle, positive to the
/// LEFT of a→b, the same sense as [`cross`]) whenever the point lies exactly
/// on one, and the caller answers from the ring's own winding instead of
/// from a parity correction to a verdict that does not exist.
fn ring_lens_parity(lon: f64, lat: f64, ring: &[[f64; 2]]) -> (bool, Option<f64>) {
    let p = [lon, lat];
    let n = ring.len();
    let mut flipped = false;
    let mut on_chord: Option<f64> = None;
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        let (flat, bulge) = flat_point_segment_and_bulge_m(p, a, b);
        if flat > bulge {
            continue;
        }
        // Rotated to `a`'s longitude so the antimeridian needs no case.
        let (ar, br, pr) = (
            [0.0, a[1]],
            [wrap180(b[0] - a[0]), b[1]],
            [wrap180(p[0] - a[0]), p[1]],
        );
        let (bx, by) = (br[0] - ar[0], br[1] - ar[1]);
        let len2 = bx * bx + by * by;
        if len2 <= 0.0 {
            continue;
        }
        // A lens is closed at its edge's endpoints: past either of them the
        // straight line and the geodesic have already crossed back, and the
        // region between their extensions is some OTHER edge's business.
        let t = ((pr[0] - ar[0]) * bx + (pr[1] - ar[1]) * by) / len2;
        if !(0.0..=1.0).contains(&t) {
            continue;
        }
        let planar = cross(ar, br, pr);
        let Some(normal) = normalize3(cross3(unit3(a), unit3(b))) else {
            continue;
        };
        let geodesic = dot3(unit3(p), normal);
        if planar == 0.0 {
            // On the chord. A meridian edge has `geodesic == 0.0` too (a
            // meridian IS its own geodesic) and needs no correction at all.
            if geodesic != 0.0 && on_chord.is_none() {
                on_chord = Some(geodesic);
            }
            continue;
        }
        if geodesic != 0.0 && (planar > 0.0) != (geodesic > 0.0) {
            flipped = !flipped;
        }
    }
    (flipped, on_chord)
}

fn great_circle_arcs_cross(a1: [f64; 2], a2: [f64; 2], b1: [f64; 2], b2: [f64; 2]) -> bool {
    let (u1, u2, v1, v2) = (unit3(a1), unit3(a2), unit3(b1), unit3(b2));
    let (Some(na), Some(nb)) = (normalize3(cross3(u1, u2)), normalize3(cross3(v1, v2))) else {
        return false;
    };
    let Some(p) = normalize3(cross3(na, nb)) else {
        return false;
    };
    let q = [-p[0], -p[1], -p[2]];
    (on_minor_arc(p, u1, u2, na) && on_minor_arc(p, v1, v2, nb))
        || (on_minor_arc(q, u1, u2, na) && on_minor_arc(q, v1, v2, nb))
}

/// Signed winding of `ring` about the point, in radians: the sum of the
/// signed angles `∠(Vᵢ, P, Vᵢ₊₁)` measured in P's own tangent plane. `0` when
/// the ring does not enclose P, `±2π` when it does.
///
/// This is the exact `geography` answer, and it is exact for the two things
/// the lon/lat ray cast plus [`ring_lens_parity`] cannot represent at all:
///
/// - an edge of large longitude span, whose geodesic leaves the straight
///   lon/lat line by degrees rather than by the millimetres the lens
///   correction is scaled for — a 117°-wide edge at latitude 29.5° tops out
///   at latitude 47.5°, eighteen degrees off its own chord;
/// - a ring that encloses a POLE, which no lon/lat ray cast can see: the
///   three vertices of a 120°-spaced triangle at latitude 71° enclose the
///   north pole on the sphere and enclose nothing on the lon/lat plane.
///
/// It also identifies longitude ±180 without a seam case, because both map
/// to the same unit vector — which is exactly how PostGIS `geography` reads
/// a GeoJSON rectangle spanning −180..180 as a polar sliver rather than a
/// world cap.
///
/// `None` when a vertex coincides with P or with P's antipode, where the
/// tangent-plane direction is undefined. The caller then falls back to the
/// boundary test, which is the right answer for a vertex hit.
fn ring_winding_about(p: [f64; 2], ring: &[[f64; 2]]) -> Option<f64> {
    let up = unit3(p);
    let tangent = |v: [f64; 2]| {
        let u = unit3(v);
        let d = dot3(u, up);
        normalize3([u[0] - d * up[0], u[1] - d * up[1], u[2] - d * up[2]])
    };
    let n = ring.len();
    let mut prev = tangent(ring[n - 1])?;
    let mut sum = 0.0;
    for v in ring.iter() {
        let cur = tangent(*v)?;
        sum += dot3(up, cross3(prev, cur)).atan2(dot3(prev, cur));
        prev = cur;
    }
    Some(sum)
}

/// Is the point within [`GEODESIC_TOUCH_EPS_M`] of the ring's own geodesic
/// boundary? Staged behind [`NEAR_BAND_M`], a million times wider, which
/// cannot move that verdict.
fn ring_boundary_touch(lon: f64, lat: f64, ring: &[[f64; 2]]) -> bool {
    let point = [lon, lat];
    let n = ring.len();
    (0..n).any(|i| {
        point_to_segment_within(point, ring[i], ring[(i + 1) % n], NEAR_BAND_M) < GEODESIC_TOUCH_EPS_M
    })
}

fn point_in_ring_geodesic(lon: f64, lat: f64, ring: &[[f64; 2]], boundary_inclusive: bool) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    // Ray-casting first, because the result is an OR and this half is pure
    // arithmetic while the other half was, until this loop was reordered and
    // gated, ~122 Karney inverses PER RING EDGE for every candidate the walk
    // ever looked at — paid even by a point sitting a kilometre inside the
    // ring. Ray-casting still needs the antimeridian-safe rotated frame (see
    // `wrap180`'s doc).
    let ref_lon = ring[0][0];
    let rel = |x: f64| wrap180(x - ref_lon);
    let px = rel(lon);
    let mut inside = false;
    let (mut xlo, mut xhi) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut ylo, mut yhi) = (f64::INFINITY, f64::NEG_INFINITY);
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (rel(ring[i][0]), ring[i][1]);
        let (xj, yj) = (rel(ring[j][0]), ring[j][1]);
        xlo = xlo.min(xi);
        xhi = xhi.max(xi);
        ylo = ylo.min(yi);
        yhi = yhi.max(yi);
        if ((yi > lat) != (yj > lat)) && (px < (xj - xi) * (lat - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    // Two rings are outside what a lon/lat ray cast plus a lens correction
    // can answer at all: one with an edge spanning more than
    // `EXACT_GEODESIC_SPAN_DEG` (past there the lens is degrees wide, not
    // metres, and `flat > bulge` stops even looking at it), and one that
    // encloses a POLE, which the plane cannot see. The ring's own extent in
    // the rotated frame decides both, at four comparisons per vertex and no
    // extra `wrap180`: no edge can span more than the extent, and a ring
    // whose longitudes all sit inside a window that narrow has a longitude
    // turn of exactly zero (the differences telescope), so it encloses no
    // pole. The test is one-sided — it can send a wide ring of short edges
    // down the exact path, which is correct, only slower — and every ring in
    // a city-scale corpus stays on the arithmetic-only path below.
    if (xhi - xlo).max(yhi - ylo) > EXACT_GEODESIC_SPAN_DEG {
        if ring_winding_about([lon, lat], ring).is_some_and(|w| w.abs() > std::f64::consts::PI) {
            return true;
        }
        return boundary_inclusive && ring_boundary_touch(lon, lat, ring);
    }
    // The ray cast just answered for the STRAIGHT-edged ring; this is the
    // correction to the geodesic one (see `ring_lens_parity`).
    let (flipped, on_chord) = ring_lens_parity(lon, lat, ring);
    if let Some(geodesic_side) = on_chord {
        // The point is ON one straight edge, so there is no planar verdict to
        // correct: answer from that edge alone. A ring's interior is to the
        // LEFT of every directed edge when the ring winds counter-clockwise,
        // and `geodesic_side` is positive to the left of the edge's own great
        // circle — so a constant-latitude north edge, whose geodesic bulges
        // poleward and therefore OUT of the ring, leaves its chord's midpoint
        // 2.7 cm inside; a south edge's chord midpoint is 2.7 cm outside.
        // Both verified against PostGIS 3.6 (`docs/SPATIAL_FUNCTIONS.md`).
        let ccw = ring_centroid_and_signed_area_rel(ring, ref_lon).2 > 0.0;
        if (geodesic_side > 0.0) == ccw {
            return true;
        }
        return boundary_inclusive && ring_boundary_touch(lon, lat, ring);
    }
    if inside != flipped {
        return true;
    }
    if !boundary_inclusive {
        return false;
    }
    // The boundary check uses the TRUE great-circle edge (via
    // `point_to_segment_geodesic_m`), not a straight lon/lat line: PostGIS
    // `geography` edges are geodesics, and a point at the arithmetic
    // lon/lat midpoint of two same-latitude vertices is measurably (here,
    // ~1.2cm) off the true edge, which bulges toward the pole — verified
    // live: PostGIS itself reports that pair's distance as ~0.012m, not 0,
    // and `ST_Intersects(geography)` as `false`. No antimeridian rotation
    // is needed here since `slerp`/Karney already work in 3D. Every call is
    // staged behind `NEAR_BAND_M`, which cannot move the 1e-6 m verdict.
    ring_boundary_touch(lon, lat, ring)
}

fn covers_point_polygon_geodesic(rings: &[Vec<[f64; 2]>], lon: f64, lat: f64) -> bool {
    let Some(outer) = rings.first() else { return false };
    if !point_in_ring_geodesic(lon, lat, outer, true) {
        return false;
    }
    !rings[1..].iter().any(|hole| point_in_ring_geodesic(lon, lat, hole, false))
}

/// Do two `geography` edges meet? The edges are GEODESICS, so this is a
/// great-circle arc crossing, not a straight-lon/lat-line crossing.
///
/// It used to be the latter: `segments_intersect` on the raw lon/lat plane,
/// rotated to `a1`'s longitude for antimeridian safety. That is the planar
/// `ST_Intersects(geometry)` edge model, and using it inside a predicate
/// documented as `ST_Intersects(geography)` is what made E4 disagree with
/// PostGIS on eleven rows of the 50K corpus — in BOTH directions, since a
/// geodesic edge bows off the straight lon/lat line by up to
/// `BULGE_METRES_PER_DEGREE_SQUARED · span²` metres and can therefore either
/// close a planar gap or open a planar crossing. See this module's
/// "PostGIS conformance" note for the ten keys.
///
/// A cheap lon/lat box test with each edge's own bulge allowance rejects
/// almost every pair before any 3D work.
fn segments_intersect_geodesic(a1: [f64; 2], a2: [f64; 2], b1: [f64; 2], b2: [f64; 2]) -> bool {
    if edge_boxes_disjoint(a1, a2, b1, b2) {
        return false;
    }
    great_circle_arcs_cross(a1, a2, b1, b2)
}

/// Spherical linear interpolation between two `(lon, lat)` points at `t ∈ [0,1]`
/// along the great circle through them (the geodesic PostGIS `geography`
/// treats a straight edge as, to first order). `t=0`/`t=1` return `a`/`b` exactly.
fn slerp_lonlat(a: [f64; 2], b: [f64; 2], t: f64) -> [f64; 2] {
    let va = lonlat_to_vec(a[0], a[1]);
    let vb = lonlat_to_vec(b[0], b[1]);
    let dot = (va.0 * vb.0 + va.1 * vb.1 + va.2 * vb.2).clamp(-1.0, 1.0);
    let theta = dot.acos();
    if theta < 1e-12 {
        return a;
    }
    let sin_theta = theta.sin();
    let wa = ((1.0 - t) * theta).sin() / sin_theta;
    let wb = (t * theta).sin() / sin_theta;
    let (x, y, z) = (wa * va.0 + wb * vb.0, wa * va.1 + wb * vb.1, wa * va.2 + wb * vb.2);
    match vector_to_lonlat(x, y, z) {
        Some((lon, lat)) => [lon, lat],
        None => a,
    }
}

/// Minimum geodesic distance (metres) from point P to segment A→B.
///
/// Earlier used e1's analytic spherical cross-track/along-track formula
/// (`point_to_segment_m`), which relies on great-circle *bearings* — those
/// become numerically unstable close to the poles (the bearing from a point
/// near 89°N essentially anywhere is close to due-south, so a small error in
/// the bearing difference produces a large error in the derived along-track
/// position). Caught by the `poly_near_pole`/`p_near_np` fixture case: the
/// analytic formula reported ~1.9 km for an edge whose true nearest point was
/// ~100 km away. This instead does a golden-section search for the `t` that
/// minimizes the exact Karney distance from P to [`slerp_lonlat`]`(A, B, t)`
/// — no bearing arithmetic, so no pole singularity, and ellipsoid-exact
/// (not spherical) at convergence. Assumes the distance is unimodal along
/// the arc, true for any segment shorter than a hemisphere (every realistic
/// polygon/line edge).
fn point_to_segment_geodesic_m(plon: f64, plat: f64, alon: f64, alat: f64, blon: f64, blat: f64) -> f64 {
    let (a, b) = ([alon, alat], [blon, blat]);
    // The sample walks the GREAT CIRCLE, not the ellipsoidal geodesic, and
    // that is not an approximation: a PostGIS `geography` edge IS the great
    // circle through its two vertices, with `(lon, lat)` read straight onto
    // the sphere, and only the distances between POINTS are spheroidal.
    // Measured, not assumed: for the 117.48°-wide edge at latitude
    // 29.503844920576796 in `tests/spatial_postgis_conformance.rs`
    // (`documented_postgis_edges_are_great_circles`), the great circle tops
    // out at latitude 47.475329 — `ST_ClosestPoint(...::geography)` on this
    // server returns `POINT(80.86411847825508 47.475329257577954)` — while
    // the WGS84 geodesic between the same two vertices tops out at
    // 47.566990, 10.2 km further north. Interpolating along the geodesic
    // instead moves the answer from PostGIS's 25,246.58 m to 35,223.56 m.
    let f = |t: f64| {
        let q = slerp_lonlat(a, b, t);
        geodesic_m(plon, plat, q[0], q[1])
    };
    const GR: f64 = 0.618_033_988_749_895; // (sqrt(5)-1)/2
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    let mut c = hi - GR * (hi - lo);
    let mut d = lo + GR * (hi - lo);
    let (mut fc, mut fd) = (f(c), f(d));
    for _ in 0..60 {
        if fc < fd {
            hi = d;
            d = c;
            fd = fc;
            c = hi - GR * (hi - lo);
            fc = f(c);
        } else {
            lo = c;
            c = d;
            fc = fd;
            d = lo + GR * (hi - lo);
            fd = f(d);
        }
        if hi - lo < 1e-15 {
            break;
        }
    }
    f((lo + hi) / 2.0).min(f(0.0)).min(f(1.0))
}

/// True if `a` and `b` touch or overlap on the sphere: any vertex of one is
/// geodesically inside-or-on a polygon part of the other, any edge pair
/// crosses (antimeridian-safe), or any point/point or point/edge pair is
/// within [`GEODESIC_TOUCH_EPS_M`] of each other (covers the point-vs-point
/// and point-vs-line cases, which have no polygon ring or edge crossing to
/// detect a touch with).
fn geodesic_overlap(a: &Geom, b: &Geom) -> bool {
    // Nothing below can fire across a gap the two bounding boxes already
    // prove is wider than the touch epsilon, and a box test is four
    // comparisons against O(n·m) edge work.
    if boxes_apart_by_more_than(a, b, GEODESIC_TOUCH_EPS_M) {
        return false;
    }
    let (ca, cb) = (all_coords(a), all_coords(b));
    for rings in polygon_ring_sets(a) {
        if cb.iter().any(|c| covers_point_polygon_geodesic(rings, c[0], c[1])) {
            return true;
        }
    }
    for rings in polygon_ring_sets(b) {
        if ca.iter().any(|c| covers_point_polygon_geodesic(rings, c[0], c[1])) {
            return true;
        }
    }
    let (ea, eb) = (all_edges(a), all_edges(b));
    if ea.iter().any(|(a1, a2)| eb.iter().any(|(b1, b2)| segments_intersect_geodesic(*a1, *a2, *b1, *b2))) {
        return true;
    }
    // The three touch tests below decide at `GEODESIC_TOUCH_EPS_M`, so every
    // exact call is staged behind `NEAR_BAND_M` (a million times wider).
    if ca.iter().any(|pa| cb.iter().any(|pb| point_pair_within(*pa, *pb, NEAR_BAND_M) < GEODESIC_TOUCH_EPS_M)) {
        return true;
    }
    if ca.iter().any(|pa| eb.iter().any(|(b1, b2)| point_to_segment_within(*pa, *b1, *b2, NEAR_BAND_M) < GEODESIC_TOUCH_EPS_M)) {
        return true;
    }
    if cb.iter().any(|pb| ea.iter().any(|(a1, a2)| point_to_segment_within(*pb, *a1, *a2, NEAR_BAND_M) < GEODESIC_TOUCH_EPS_M)) {
        return true;
    }
    false
}

/// Is every point of `a` further than `metres` from every point of `b`, on
/// the evidence of their lon/lat bounding boxes alone? A conservative lower
/// bound on the box-to-box separation, so `true` is always sound; `false`
/// only means the boxes are close enough to need the real test.
fn boxes_apart_by_more_than(a: &Geom, b: &Geom, metres: f64) -> bool {
    let (Some(ba), Some(bb)) = (a.bbox(), b.bbox()) else {
        return false;
    };
    let (axl, axh, ayl, ayh) = ba;
    let (bxl, bxh, byl, byh) = bb;
    // A box whose raw longitude span exceeds 180° is an antimeridian
    // artefact, not a real extent; do not reject on it.
    if axh - axl > 180.0 || bxh - bxl > 180.0 {
        return false;
    }
    let dlat = (byl - ayh).max(ayl - byh).max(0.0);
    let dlon = (wrap180(bxl - axh)).max(wrap180(axl - bxh)).max(0.0);
    // Each side may bulge poleward by up to this much; the boxes are built
    // from vertices, so allow both. `span` also has to reach across the gap:
    // the path being bounded runs from one box to the other.
    let span = (axh - axl)
        .max(ayh - ayl)
        .max(bxh - bxl)
        .max(byh - byl)
        .max(dlon)
        .max(dlat);
    let scale = min_lon_scale(ayl.abs().max(ayh.abs()), byl.abs().max(byh.abs()), span);
    let bound = MIN_METRES_PER_DEGREE * (dlon * scale).hypot(dlat) - max_bulge_m(span);
    bound > metres
}

/// `ST_Intersects(a::geography, b::geography)` — spheroidal, antimeridian-aware.
/// The one PostGIS predicate in this module's set that *does* have a real
/// `geography` overload (see `docs/SPATIAL_FUNCTIONS.md`); this is the one
/// E4's plain `intersects` matches, not the planar cast.
pub fn intersects(a: &Geom, b: &Geom) -> bool {
    geodesic_overlap(a, b)
}

/// `ST_Distance(a::geography, b::geography)` — metres, minimum over every
/// vertex/vertex and vertex/edge pair (the closest pair between two disjoint
/// polylines/polygons is always at a vertex, never edge-interior-to-edge-interior
/// only). `0.0` if the geometries touch or overlap. `f64::INFINITY` if either
/// geometry has no coordinates at all (never panics).
pub fn distance_m(a: &Geom, b: &Geom) -> f64 {
    if geodesic_overlap(a, b) {
        return 0.0;
    }
    nearest_within(a, b, f64::INFINITY).1
}

/// The shared engine of [`distance_m`] and [`dwithin_m`]: the minimum
/// vertex/vertex and vertex/edge geodesic distance, with `(answered, value)`
/// telling the caller whether some pair came in at or under `ceiling`.
///
/// Two economies, neither of which can move an answer. Every pair is first
/// bounded by the closed-form lower bound: one already beyond `ceiling` is
/// skipped, which is sound because the bound never over-states. And the
/// vertex/vertex pairs (one Karney inverse each) run before the vertex/edge
/// pairs (a golden-section search, ~122 inverses each), so a `dwithin`
/// answered by a vertex never pays for a segment.
fn nearest_within(a: &Geom, b: &Geom, ceiling: f64) -> (bool, f64) {
    let (ca, cb) = (all_coords(a), all_coords(b));
    let (ea, eb) = (all_edges(a), all_edges(b));
    // With a finite ceiling the walk may stop at the first pair inside it;
    // without one it must see every pair, and the running minimum is what
    // prunes instead.
    let bounded = ceiling.is_finite();
    let mut best = f64::INFINITY;
    for pa in &ca {
        for pb in &cb {
            let cutoff = if bounded { ceiling } else { best };
            if point_pair_lower_bound_m(*pa, *pb) > cutoff {
                continue;
            }
            best = best.min(geodesic_m(pa[0], pa[1], pb[0], pb[1]));
            if bounded && best <= ceiling {
                return (true, best);
            }
        }
    }
    for (points, edges) in [(&ca, &eb), (&cb, &ea)] {
        for p in points.iter() {
            for (e1, e2) in edges.iter() {
                let cutoff = if bounded { ceiling } else { best };
                if point_segment_lower_bound_m(*p, *e1, *e2) > cutoff {
                    continue;
                }
                best = best.min(point_to_segment_geodesic_m(p[0], p[1], e1[0], e1[1], e2[0], e2[1]));
                if bounded && best <= ceiling {
                    return (true, best);
                }
            }
        }
    }
    (best <= ceiling, best)
}

/// `ST_DWithin(a::geography, b::geography, radius_metres)`. Asks only the
/// question it needs: it stops at the first pair inside the radius instead of
/// finishing [`distance_m`]'s full minimum, and it never looks at a pair the
/// cheap bound already places outside.
pub fn dwithin_m(a: &Geom, b: &Geom, radius_metres: f64) -> bool {
    if boxes_apart_by_more_than(a, b, radius_metres) {
        return false;
    }
    if geodesic_overlap(a, b) {
        return true;
    }
    nearest_within(a, b, radius_metres).0
}


#[cfg(test)]
mod tests {
    use super::*;

    fn poly(rings: Vec<Vec<[f64; 2]>>) -> Geom {
        Geom::Polygon(rings)
    }

    /// **Degenerate inputs never panic: an empty ring.**
    #[test]
    fn an_empty_ring_never_panics() {
        let empty = poly(vec![vec![]]);
        let pt = Geom::Point(0.0, 0.0);
        assert_eq!(area_m2(&empty), 0.0);
        assert_eq!(perimeter_m(&empty), 0.0);
        assert_eq!(length_m(&empty), 0.0);
        assert!(centroid(&empty).is_none());
        assert!(!within(&pt, &empty));
        assert!(!contains(&empty, &pt));
        assert!(!covers(&empty, &pt));
        assert!(!crosses(&pt, &empty));
        assert!(!intersects(&pt, &empty));
        assert!(!intersects_planar(&pt, &empty));
        assert_eq!(distance_m(&empty, &pt), f64::INFINITY);
    }

    /// **Degenerate inputs never panic: a two-point "polygon".**
    #[test]
    fn a_two_point_polygon_never_panics() {
        let degenerate = poly(vec![vec![[0.0, 0.0], [1.0, 1.0]]]);
        let pt = Geom::Point(0.5, 0.5);
        assert_eq!(area_m2(&degenerate), 0.0, "fewer than 3 vertices can't enclose any area");
        assert!(perimeter_m(&degenerate).is_finite());
        assert!(centroid(&degenerate).is_some());
        assert!(!within(&pt, &degenerate));
        assert!(distance_m(&degenerate, &pt).is_finite());
    }

    /// **Degenerate inputs never panic: NaN coordinates.**
    ///
    /// `Geom` itself does no validation (that lives in the ingestion layer,
    /// `src/lib.rs::geo()`, which rejects non-finite coordinates before a
    /// `Geom` is ever built) — but every function here must still not panic
    /// if one somehow arrives, since a library boundary shouldn't trust its
    /// caller to have already checked.
    #[test]
    fn nan_coordinates_never_panic() {
        let g = Geom::Polygon(vec![vec![[0.0, 0.0], [f64::NAN, 1.0], [1.0, 1.0], [0.0, 1.0]]]);
        let pt = Geom::Point(f64::NAN, 0.0);
        let _ = area_m2(&g);
        let _ = perimeter_m(&g);
        let _ = length_m(&g);
        let _ = centroid(&g);
        let _ = within(&pt, &g);
        let _ = contains(&g, &pt);
        let _ = covers(&g, &pt);
        let _ = crosses(&pt, &g);
        let _ = intersects(&pt, &g);
        let _ = intersects_planar(&pt, &g);
        let _ = distance_m(&g, &pt);
        let _ = dwithin_m(&g, &pt, 1000.0);
    }

    /// **Antipodal points: the geodesic distance is the half-circumference,
    /// not zero or infinite, and the reverse pair agrees.**
    #[test]
    fn antipodal_points_distance_is_the_half_circumference() {
        let a = Geom::Point(0.0, 0.0);
        let b = Geom::Point(180.0, 0.0);
        let d = distance_m(&a, &b);
        assert!((d - 20_003_931.4).abs() < 1.0, "got {d}");
        assert_eq!(distance_m(&a, &b), distance_m(&b, &a));
    }

    /// **Pole-to-pole distance is the full meridional half-circumference,
    /// independent of longitude (every meridian meets at the poles).**
    #[test]
    fn pole_to_pole_distance_is_independent_of_longitude() {
        let north = Geom::Point(37.0, 90.0);
        let south = Geom::Point(-142.0, -90.0);
        let d = distance_m(&north, &south);
        assert!((d - 20_003_931.4).abs() < 1.0, "got {d}");
    }

    /// **A point just either side of the antimeridian is close, not
    /// ~40,000km away** — the naive `lon2 - lon1` distance a non-geodesic
    /// implementation would compute.
    #[test]
    fn dateline_adjacent_points_are_close_not_almost_the_circumference() {
        let a = Geom::Point(179.9, 10.0);
        let b = Geom::Point(-179.9, 10.0);
        let d = distance_m(&a, &b);
        assert!(d < 50_000.0, "got {d} — should be ~22km, not a trip around the world");
    }

    /// **A hole excludes its own area from the polygon's area.**
    #[test]
    fn a_hole_excludes_its_area() {
        let with_hole = poly(vec![
            vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]],
            vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]],
        ]);
        let without_hole = poly(vec![vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]]]);
        assert!(area_m2(&with_hole) < area_m2(&without_hole));
        // A point inside the hole is NOT part of the polygon.
        assert!(!within(&Geom::Point(1.5, 1.5), &with_hole));
        // The same point IS part of the polygon once the hole is removed.
        assert!(within(&Geom::Point(1.5, 1.5), &without_hole));
    }

    /// **`within`/`contains` duality: `contains(a, b) == within(b, a)` for
    /// every ordered pair, by construction — but re-checking with concrete
    /// geometries catches a copy-paste flip of the arguments.**
    #[test]
    fn within_and_contains_are_dual() {
        let small = Geom::Point(1.0, 1.0);
        let big = poly(vec![vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]]]);
        assert!(within(&small, &big));
        assert!(contains(&big, &small));
        assert_eq!(within(&small, &big), contains(&big, &small));
        assert_eq!(within(&big, &small), contains(&small, &big));
    }

    /// **`intersects` (geography) is symmetric: order of arguments never
    /// changes the answer.**
    #[test]
    fn intersects_is_symmetric() {
        let cases: [(Geom, Geom); 3] = [
            (Geom::Point(1.0, 1.0), poly(vec![vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]]])),
            (Geom::Point(100.0, 45.0), Geom::LineString(vec![[0.0, 0.0], [1.0, 1.0]])),
            (Geom::LineString(vec![[-1.0, 0.5], [5.0, 0.5]]), poly(vec![vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]]])),
        ];
        for (a, b) in &cases {
            assert_eq!(intersects(a, b), intersects(b, a), "asymmetric for {a:?}/{b:?}");
            assert_eq!(intersects_planar(a, b), intersects_planar(b, a), "asymmetric (planar) for {a:?}/{b:?}");
        }
    }

    /// **`crosses` is false when one geometry contains the other** — full
    /// containment is not a crossing, per OGC (and verified against a
    /// containment case pulled from the PostGIS fixture, where the wholly
    /// interior `ln_multi`/`poly_popsim_large` pair reports `crosses = false`).
    #[test]
    fn crosses_is_false_for_containment() {
        let inner = Geom::LineString(vec![[1.5, 1.5], [2.5, 2.5]]);
        let outer = poly(vec![vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]]]);
        assert!(within(&inner, &outer));
        assert!(!crosses(&inner, &outer));
        assert!(!crosses(&outer, &inner));
        // And two polygons (area/area) are never `crosses`, regardless of overlap.
        let overlapping = poly(vec![vec![[2.0, 2.0], [6.0, 2.0], [6.0, 6.0], [2.0, 6.0], [2.0, 2.0]]]);
        assert!(!crosses(&outer, &overlapping));
    }

    /// **The popsim centre-to-corner distance equals `wgs84_distance_metres`
    /// exactly** — `distance_m` for a point/point pair must be the same
    /// Karney call [`crate::spatial_math::wgs84_distance_metres`] already
    /// uses, not a second, potentially-diverging implementation.
    #[test]
    fn popsim_centre_to_corner_distance_matches_wgs84_distance_metres_exactly() {
        let centre = Geom::Point(107.6, -6.9);
        let corner = Geom::Point(108.5, -5.5);
        let via_geometry = distance_m(&centre, &corner);
        let via_spatial_math = crate::spatial_math::wgs84_distance_metres(
            crate::spatial_math::Point::new(107.6, -6.9).unwrap(),
            crate::spatial_math::Point::new(108.5, -5.5).unwrap(),
        );
        assert_eq!(via_geometry, via_spatial_math);
    }
}
