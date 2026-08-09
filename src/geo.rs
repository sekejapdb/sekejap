//! Pure geometry functions for spatial queries.
//!
//! All coordinates: `[lon, lat]` in GeoJSON, `(lat, lon)` in function params (PostGIS convention).
//! No external crate dependencies — everything is hand-rolled.

use serde_json::Value;
use std::collections::HashMap;

const EARTH_RADIUS_KM: f64 = 6371.0;

// ── Haversine distance ───────────────────────────────────────────────────────

/// Great-circle distance between two points in kilometres.
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();
    let lat1_r = lat1.to_radians();
    let lat2_r = lat2.to_radians();

    let a = (d_lat / 2.0).sin().powi(2) + lat1_r.cos() * lat2_r.cos() * (d_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    EARTH_RADIUS_KM * c
}

// ── Geodesic core (WGS84 ellipsoid — matches PostGIS `geography`) ─────────────
//
// PostGIS `geography` measures on the WGS84 ellipsoid in METRES / SQUARE METRES.
// sekejap mirrors that exactly: distances via Vincenty's inverse formula (sub-mm
// for all non-antipodal pairs) and polygon area via the authalic-sphere spherical
// excess. All SQL-facing spatial measures (ST_Distance, ST_DWithin, ST_Perimeter,
// ST_Area) are in these units so results equal PostGIS to float precision.

/// WGS84 defining parameters.
const WGS84_A: f64 = 6_378_137.0;                 // semi-major axis (m)
const WGS84_F: f64 = 1.0 / 298.257_223_563;       // flattening
/// WGS84 first eccentricity squared, e² = f(2−f).
const WGS84_E2: f64 = WGS84_F * (2.0 - WGS84_F);
/// WGS84 authalic (equal-area) sphere radius (m) — the sphere with the same
/// surface area as the ellipsoid; used for geodesic polygon area.
const WGS84_AUTHALIC_R: f64 = 6_371_007.180_918_47;

/// Authalic latitude (radians) for a geodetic latitude — the equal-area mapping
/// onto the authalic sphere. Computing the spherical excess in authalic latitude
/// (not geodetic) is what makes the sphere-based area equal the ellipsoid's, so it
/// matches PostGIS `ST_Area(::geography)` rather than running ~0.12% low.
fn authalic_lat(phi: f64) -> f64 {
    let e2 = WGS84_E2;
    let e = e2.sqrt();
    let s = phi.sin();
    // q(φ) = (1−e²)[ sinφ/(1−e²sin²φ) − 1/(2e)·ln((1−e·sinφ)/(1+e·sinφ)) ]
    let q = |s: f64| {
        (1.0 - e2) * (s / (1.0 - e2 * s * s) - (1.0 / (2.0 * e)) * ((1.0 - e * s) / (1.0 + e * s)).ln())
    };
    let qp = q(1.0); // q at the pole (sinφ = 1)
    (q(s) / qp).clamp(-1.0, 1.0).asin()
}

/// Geodesic distance between two points in METRES on the WGS84 ellipsoid
/// (Vincenty inverse). Matches PostGIS `ST_Distance(a::geography, b::geography)`.
pub fn geodesic_distance_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let a = WGS84_A;
    let f = WGS84_F;
    let b = a * (1.0 - f);

    let l = (lon2 - lon1).to_radians();
    let u1 = ((1.0 - f) * lat1.to_radians().tan()).atan();
    let u2 = ((1.0 - f) * lat2.to_radians().tan()).atan();
    let (sin_u1, cos_u1) = (u1.sin(), u1.cos());
    let (sin_u2, cos_u2) = (u2.sin(), u2.cos());

    let mut lambda = l;
    let mut sin_sigma = 0.0;
    let mut cos_sigma = 0.0;
    let mut sigma = 0.0;
    let mut cos_sq_alpha = 0.0;
    let mut cos2_sigma_m = 0.0;

    for _ in 0..200 {
        let (sin_lambda, cos_lambda) = (lambda.sin(), lambda.cos());
        sin_sigma = ((cos_u2 * sin_lambda).powi(2)
            + (cos_u1 * sin_u2 - sin_u1 * cos_u2 * cos_lambda).powi(2))
        .sqrt();
        if sin_sigma == 0.0 {
            return 0.0; // coincident points
        }
        cos_sigma = sin_u1 * sin_u2 + cos_u1 * cos_u2 * cos_lambda;
        sigma = sin_sigma.atan2(cos_sigma);
        let sin_alpha = cos_u1 * cos_u2 * sin_lambda / sin_sigma;
        cos_sq_alpha = 1.0 - sin_alpha * sin_alpha;
        cos2_sigma_m = if cos_sq_alpha != 0.0 {
            cos_sigma - 2.0 * sin_u1 * sin_u2 / cos_sq_alpha
        } else {
            0.0 // equatorial line
        };
        let c = f / 16.0 * cos_sq_alpha * (4.0 + f * (4.0 - 3.0 * cos_sq_alpha));
        let lambda_prev = lambda;
        lambda = l
            + (1.0 - c)
                * f
                * sin_alpha
                * (sigma
                    + c * sin_sigma
                        * (cos2_sigma_m + c * cos_sigma * (-1.0 + 2.0 * cos2_sigma_m * cos2_sigma_m)));
        if (lambda - lambda_prev).abs() < 1e-12 {
            break;
        }
    }

    let u_sq = cos_sq_alpha * (a * a - b * b) / (b * b);
    let cap_a = 1.0 + u_sq / 16384.0 * (4096.0 + u_sq * (-768.0 + u_sq * (320.0 - 175.0 * u_sq)));
    let cap_b = u_sq / 1024.0 * (256.0 + u_sq * (-128.0 + u_sq * (74.0 - 47.0 * u_sq)));
    let delta_sigma = cap_b
        * sin_sigma
        * (cos2_sigma_m
            + cap_b / 4.0
                * (cos_sigma * (-1.0 + 2.0 * cos2_sigma_m * cos2_sigma_m)
                    - cap_b / 6.0
                        * cos2_sigma_m
                        * (-3.0 + 4.0 * sin_sigma * sin_sigma)
                        * (-3.0 + 4.0 * cos2_sigma_m * cos2_sigma_m)));
    b * cap_a * (sigma - delta_sigma)
}

/// Geodesic length of a `[lat, lon]` vertex path in METRES (sum of Vincenty edges).
/// For a closed ring this is the perimeter. Matches PostGIS `ST_Perimeter`/`ST_Length`.
pub fn geodesic_path_length_m(coords: &[[f64; 2]]) -> f64 {
    coords
        .windows(2)
        .map(|w| geodesic_distance_m(w[0][0], w[0][1], w[1][0], w[1][1]))
        .sum()
}

/// Geodesic area of a polygon ring (`[lat, lon]`) in SQUARE METRES, via the
/// spherical excess on the WGS84 authalic sphere. Matches PostGIS
/// `ST_Area(::geography)` to ~1e-5 relative for city-scale polygons. Sign is
/// dropped (absolute area); the ring need not be explicitly closed.
pub fn geodesic_ring_area_m2(ring: &[[f64; 2]]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    // L'Huilier / line-integral form of the spherical excess:
    //   E = Σ 2·atan2( tan(Δλ/2)·(tan(φ1/2)+tan(φ2/2)), 1 + tan(φ1/2)·tan(φ2/2) )
    let mut excess = 0.0;
    for i in 0..n {
        // Longitude stays geodetic; latitude → authalic so the excess yields the
        // ellipsoid's area (matches PostGIS geography) not the sphere's.
        let (lat1, lon1) = (authalic_lat(ring[i][0].to_radians()), ring[i][1].to_radians());
        let j = (i + 1) % n;
        let (lat2, lon2) = (authalic_lat(ring[j][0].to_radians()), ring[j][1].to_radians());
        let d_lon = lon2 - lon1;
        let t1 = (lat1 / 2.0).tan();
        let t2 = (lat2 / 2.0).tan();
        excess += 2.0 * ((d_lon / 2.0).tan() * (t1 + t2)).atan2(1.0 + t1 * t2);
    }
    (excess.abs()) * WGS84_AUTHALIC_R * WGS84_AUTHALIC_R
}

// ── Spatial measurements ─────────────────────────────────────────────────────

/// `ST_Distance(a::geography, b::geography)` — geodesic distance in METRES on the
/// WGS84 ellipsoid. Point-to-point is exact (Vincenty); for geometries with
/// vertices it returns the minimum geodesic distance between any vertex pair.
/// Returns None if either geometry is invalid.
pub fn distance_m(geom1: &Value, geom2: &Value) -> Option<f64> {
    let coords1 = extract_geojson_coords(geom1);
    let coords2 = extract_geojson_coords(geom2);
    if coords1.is_empty() || coords2.is_empty() {
        return None;
    }
    if coords1.len() == 1 && coords2.len() == 1 {
        return Some(geodesic_distance_m(
            coords1[0][0], coords1[0][1], coords2[0][0], coords2[0][1],
        ));
    }
    let mut min_dist = f64::MAX;
    for c1 in &coords1 {
        for c2 in &coords2 {
            let d = geodesic_distance_m(c1[0], c1[1], c2[0], c2[1]);
            if d < min_dist {
                min_dist = d;
            }
        }
    }
    Some(min_dist)
}

/// `ST_Length(::geography)` — geodesic length of a LINESTRING in METRES.
/// Per PostGIS, a Polygon has **zero** length (its boundary is `ST_Perimeter`);
/// returns None for non-line geometries so a callers' `> x` filter excludes them.
pub fn length_m(geom: &Value) -> Option<f64> {
    match geom.get("type").and_then(|t| t.as_str()) {
        Some("LineString") | Some("MultiLineString") => {
            let coords = extract_geojson_coords(geom);
            if coords.len() < 2 {
                return Some(0.0);
            }
            Some(geodesic_path_length_m(&coords))
        }
        // Points, Polygons, MultiPolygons: length is 0 in PostGIS.
        _ => Some(0.0),
    }
}

/// `ST_Perimeter(::geography)` — geodesic perimeter of a Polygon/MultiPolygon in
/// METRES (sum of each outer ring's closed geodesic boundary). None if not areal.
pub fn perimeter_m(geom: &Value) -> Option<f64> {
    let rings = extract_polygon_rings(geom);
    if rings.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for ring in &rings {
        total += geodesic_path_length_m(ring);
        // Close the ring if the last vertex doesn't repeat the first.
        if let (Some(first), Some(last)) = (ring.first(), ring.last()) {
            if first != last {
                total += geodesic_distance_m(last[0], last[1], first[0], first[1]);
            }
        }
    }
    Some(total)
}

/// `ST_Area(::geography)` — geodesic area of a Polygon/MultiPolygon in SQUARE
/// METRES on the WGS84 ellipsoid (sum of each part's outer ring). None if not areal.
pub fn area_m2(geom: &Value) -> Option<f64> {
    let rings = extract_polygon_rings(geom);
    if rings.is_empty() {
        return None;
    }
    Some(rings.iter().map(|r| geodesic_ring_area_m2(r)).sum())
}

// ── Geometry field discovery ─────────────────────────────────────────────────

/// Find the GeoJSON geometry value in a payload, regardless of the field name.
///
/// Prefers a field literally named `geometry` (the common case, fast path);
/// otherwise scans object fields and returns the first whose value parses as a
/// GeoJSON geometry. This lets spatial queries work on any GEO column name
/// (e.g. `geo`, `location`), not only one called `geometry`.
fn find_geometry(payload: &Value) -> Option<&Value> {
    if let Some(g) = payload.get("geometry") {
        if !extract_geojson_coords(g).is_empty() {
            return Some(g);
        }
    }
    if let Value::Object(map) = payload {
        for (name, v) in map {
            if name == "geometry" || !v.is_object() {
                continue;
            }
            if !extract_geojson_coords(v).is_empty() {
                return Some(v);
            }
        }
    }
    None
}

// ── Centroid extraction ──────────────────────────────────────────────────────

/// Extract `(lat, lon)` centroid from a node payload via GeoJSON geometry.
pub fn extract_centroid(payload: &Value) -> Option<(f64, f64)> {
    let geom = find_geometry(payload)?;
    let coords = extract_geojson_coords(geom);
    if coords.is_empty() {
        return None;
    }
    let n = coords.len() as f64;
    let lat = coords.iter().map(|c| c[0]).sum::<f64>() / n;
    let lon = coords.iter().map(|c| c[1]).sum::<f64>() / n;
    Some((lat, lon))
}

// ── Spatial metadata ─────────────────────────────────────────────────────────

/// Cached spatial metadata for a node: centroid + axis-aligned bounding box.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SpatialMeta {
    pub centroid_lat: f64,
    pub centroid_lon: f64,
    pub bbox_min_lat: f64,
    pub bbox_min_lon: f64,
    pub bbox_max_lat: f64,
    pub bbox_max_lon: f64,
}

/// Extract spatial metadata from a node payload via GeoJSON geometry.
pub fn extract_spatial_meta(payload: &Value) -> Option<SpatialMeta> {
    let geom = find_geometry(payload)?;
    let coords = extract_geojson_coords(geom);
    if coords.is_empty() {
        return None;
    }
    let n = coords.len() as f64;
    let centroid_lat = coords.iter().map(|c| c[0]).sum::<f64>() / n;
    let centroid_lon = coords.iter().map(|c| c[1]).sum::<f64>() / n;
    let mut min_lat = f64::MAX;
    let mut max_lat = f64::MIN;
    let mut min_lon = f64::MAX;
    let mut max_lon = f64::MIN;
    for c in &coords {
        min_lat = min_lat.min(c[0]);
        max_lat = max_lat.max(c[0]);
        min_lon = min_lon.min(c[1]);
        max_lon = max_lon.max(c[1]);
    }
    Some(SpatialMeta {
        centroid_lat,
        centroid_lon,
        bbox_min_lat: min_lat,
        bbox_min_lon: min_lon,
        bbox_max_lat: max_lat,
        bbox_max_lon: max_lon,
    })
}

// ── Spatial grid (spatial hashing) ──────────────────────────────────────────

/// Grid-based spatial index using spatial hashing.
/// Maps `(cell_lat, cell_lon)` → `Vec<node_hash>` for fast candidate lookup.
pub(crate) struct SpatialGrid {
    cell_size: f64,
    cells: HashMap<(i32, i32), Vec<u64>>,
    meta: HashMap<u64, SpatialMeta>,
    /// Parsed polygon rings (`[[lat,lon],…]`) cached per node so point-in-polygon
    /// tests never re-read + re-parse the GeoJSON payload from disk (the PIP hot path).
    poly_rings: HashMap<u64, Vec<Vec<[f64; 2]>>>,
    /// Disk-first (paged) base: cell index + per-node meta served from an mmap'd
    /// `spatialgrid.bin`. When present, `cells`/`meta` act as the resident write
    /// overlay and reads union the overlay with the base. `None` in heap mode.
    mapped: Option<crate::storage::spatialstore::MappedSpatialGrid>,
}

impl SpatialGrid {
    /// Build the grid from an iterator of `(node_hash, SpatialMeta)`.
    pub fn build(items: impl Iterator<Item = (u64, SpatialMeta)>) -> Self {
        let collected: Vec<(u64, SpatialMeta)> = items.collect();
        if collected.is_empty() {
            return Self {
                cell_size: 0.01,
                cells: HashMap::new(),
                meta: HashMap::new(),
                poly_rings: HashMap::new(),
                mapped: None,
            };
        }

        // Compute data extent for auto cell size
        let mut min_lat = f64::MAX;
        let mut max_lat = f64::MIN;
        let mut min_lon = f64::MAX;
        let mut max_lon = f64::MIN;
        for (_, m) in &collected {
            min_lat = min_lat.min(m.bbox_min_lat);
            max_lat = max_lat.max(m.bbox_max_lat);
            min_lon = min_lon.min(m.bbox_min_lon);
            max_lon = max_lon.max(m.bbox_max_lon);
        }
        let lat_range = max_lat - min_lat;
        let lon_range = max_lon - min_lon;
        // Occupancy-based cell size. The old `extent / 100` tied cell size to the
        // widest collection's extent, so a worldwide point set (~360° span) produced
        // ~3.6°-wide cells (~400 km) — one cell swallowed an entire metropolitan
        // cluster, and every grid lookup degenerated to a near-linear scan (a huge
        // initial kNN ring, a giant radius box). Instead size cells to the DATA
        // VOLUME: aim for a handful of nodes per cell (~n/4 non-empty cells) so a
        // neighborhood scan touches tens of nodes regardless of global spread.
        let n = collected.len().max(1);
        let divisor = ((n as f64) / 4.0).sqrt().max(1.0);
        let cell_size = (lat_range.max(lon_range) / divisor).clamp(0.0005, 1.0);

        let mut grid = Self {
            cell_size,
            cells: HashMap::new(),
            meta: HashMap::new(),
            poly_rings: HashMap::new(),
            mapped: None,
        };

        for (hash, m) in collected {
            grid.insert_into_cells(hash, &m);
            grid.meta.insert(hash, m);
        }

        grid
    }

    /// Disk-first grid for paged mode: cells + meta served from the mmap'd base;
    /// the resident `cells`/`meta` start empty and act as the write overlay.
    pub fn from_mapped(base: crate::storage::spatialstore::MappedSpatialGrid) -> Self {
        Self {
            cell_size: base.cell_size(),
            cells: HashMap::new(),
            meta: HashMap::new(),
            poly_rings: HashMap::new(),
            mapped: Some(base),
        }
    }

    /// Serialize the grid (cell index + per-node meta + cell size) to the
    /// `SKGRID01` sidecar format read by `MappedSpatialGrid`.
    pub fn write_binary<W: std::io::Write>(&self, w: &mut W) -> std::io::Result<()> {
        w.write_all(b"SKGRID01")?;
        w.write_all(&1u32.to_le_bytes())?;
        w.write_all(&self.cell_size.to_le_bytes())?;

        // Meta records, sorted by hash (binary-searchable).
        let mut metas: Vec<(&u64, &SpatialMeta)> = self.meta.iter().collect();
        metas.sort_unstable_by_key(|(h, _)| **h);
        w.write_all(&(metas.len() as u32).to_le_bytes())?;
        for (h, m) in &metas {
            w.write_all(&h.to_le_bytes())?;
            for v in [m.centroid_lat, m.centroid_lon, m.bbox_min_lat, m.bbox_min_lon, m.bbox_max_lat, m.bbox_max_lon] {
                w.write_all(&v.to_le_bytes())?;
            }
        }

        // Cell directory (sorted by (cy,cx)) + concatenated posting blob.
        let mut cells: Vec<(&(i32, i32), &Vec<u64>)> = self.cells.iter().collect();
        cells.sort_unstable_by_key(|(k, _)| **k);
        w.write_all(&(cells.len() as u32).to_le_bytes())?;
        let mut blob: Vec<u8> = Vec::new();
        let mut dir: Vec<u8> = Vec::with_capacity(cells.len() * 20);
        for ((cy, cx), hashes) in &cells {
            let off = blob.len() as u64;
            dir.extend_from_slice(&cy.to_le_bytes());
            dir.extend_from_slice(&cx.to_le_bytes());
            dir.extend_from_slice(&off.to_le_bytes());
            dir.extend_from_slice(&(hashes.len() as u32).to_le_bytes());
            for &h in hashes.iter() { blob.extend_from_slice(&h.to_le_bytes()); }
        }
        w.write_all(&dir)?;
        w.write_all(&(blob.len() as u64).to_le_bytes())?;
        w.write_all(&blob)?;
        Ok(())
    }

    /// Node hashes in cell `(cy,cx)` — resident overlay unioned with the mmap base.
    fn cell_members_at(&self, key: (i32, i32)) -> Option<Vec<u64>> {
        let overlay = self.cells.get(&key);
        let base = self.mapped.as_ref().and_then(|m| m.cell_members(key.0, key.1));
        match (overlay, base) {
            (None, None) => None,
            (Some(v), None) => Some(v.clone()),
            (None, Some(v)) => Some(v),
            (Some(o), Some(mut b)) => { b.extend_from_slice(o); Some(b) }
        }
    }

    /// Spatial metadata for a node — resident overlay first, then the mmap base.
    fn meta_at(&self, hash: u64) -> Option<SpatialMeta> {
        if let Some(m) = self.meta.get(&hash) { return Some(m.clone()); }
        self.mapped.as_ref().and_then(|m| m.node_meta(hash))
    }

    /// Whether the mmap base holds this node (used to avoid double-inserting a
    /// base node into the resident overlay on paged open).
    pub fn base_contains(&self, hash: u64) -> bool {
        self.mapped.as_ref().map_or(false, |m| m.node_meta(hash).is_some())
    }

    /// True when the cell index + meta are served from the mmap base (paged mode)
    /// rather than a resident HashMap. For tests / introspection.
    pub fn is_disk_backed(&self) -> bool {
        self.mapped.is_some()
    }

    /// Insert a node into the grid.
    pub fn insert(&mut self, hash: u64, meta: SpatialMeta) {
        self.insert_into_cells(hash, &meta);
        self.meta.insert(hash, meta);
    }

    /// Remove a node from the grid.
    pub fn remove(&mut self, hash: u64) {
        if let Some(meta) = self.meta.remove(&hash) {
            let cells = self.cells_for_bbox(&meta);
            for key in cells {
                if let Some(v) = self.cells.get_mut(&key) {
                    v.retain(|&h| h != hash);
                }
            }
        }
    }

    /// Get cached spatial metadata for a node (resident overlay or mmap base).
    pub fn get_meta(&self, hash: u64) -> Option<SpatialMeta> {
        self.meta_at(hash)
    }

    /// Number of nodes in the grid (resident overlay + mmap base).
    pub fn len(&self) -> usize {
        self.meta.len() + self.mapped.as_ref().map_or(0, |m| m.len())
    }

    /// Cache a node's parsed polygon rings (`[[lat,lon],…]`) for fast PIP.
    pub fn cache_rings(&mut self, hash: u64, rings: Vec<Vec<[f64; 2]>>) {
        if !rings.is_empty() {
            self.poly_rings.insert(hash, rings);
        }
    }

    /// `Some(true/false)` if this node has cached polygon rings — exact point-in-polygon
    /// with zero payload reads. `None` if not cached (caller falls back to the payload).
    pub fn contains_point(&self, hash: u64, lat: f64, lon: f64) -> Option<bool> {
        self.poly_rings.get(&hash).map(|rings| rings.iter().any(|r| point_in_polygon(lat, lon, r)))
    }

    /// Cached polygon rings for a node, if present (eager-cached in resident mode,
    /// or previously loaded). `None` → not cached; the caller loads from the payload.
    pub fn rings_for(&self, hash: u64) -> Option<&Vec<Vec<[f64; 2]>>> {
        self.poly_rings.get(&hash)
    }

    /// Return candidate node hashes within `km` of `(lat, lon)`.
    pub fn candidates_within_distance(&self, lat: f64, lon: f64, km: f64) -> Vec<u64> {
        // Convert km to approximate degree range (conservative)
        let deg = km / 111.0; // 1 degree ≈ 111 km
        let lat_expand = deg;
        let lon_expand = deg / (lat.to_radians().cos().abs().max(0.01));

        self.candidates_in_bbox(
            lat - lat_expand,
            lon - lon_expand,
            lat + lat_expand,
            lon + lon_expand,
        )
    }

    /// k nearest node hashes to `(lat, lon)`, ascending by haversine distance.
    ///
    /// Best-first ring expansion (the grid analog of an R-tree's Hjaltason–Samet
    /// best-first search). We visit grid cells in growing square rings around the
    /// query cell, keeping only the k closest seen so far in a bounded max-heap.
    /// After each ring we compute a LOWER BOUND on the distance to any node in an
    /// unsearched cell — the gap from the query point to the nearest edge of the
    /// searched box. Once the heap holds k nodes and the k-th is closer than that
    /// bound, no unsearched node can beat it, so we stop. Each node is scored at
    /// most once (no re-scanning), and typical queries touch a handful of cells.
    pub fn k_nearest(&self, lat: f64, lon: f64, k: usize) -> Vec<u64> {
        if k == 0 || self.len() == 0 {
            return Vec::new();
        }
        let cs = self.cell_size;
        let cy0 = (lat / cs).floor() as i32;
        let cx0 = (lon / cs).floor() as i32;
        let coslat = lat.to_radians().cos().abs().max(0.01);

        // Bounded max-heap: the root is the current farthest of the k best, so a new
        // closer node evicts it. `Dist` orders by distance (NaN sinks to "largest").
        #[derive(PartialEq)]
        struct Dist(f64);
        impl Eq for Dist {}
        impl PartialOrd for Dist { fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) } }
        impl Ord for Dist { fn cmp(&self, o: &Self) -> std::cmp::Ordering { self.0.total_cmp(&o.0) } }
        let mut heap: std::collections::BinaryHeap<(Dist, u64)> = std::collections::BinaryHeap::new();

        let mut seen = 0usize;
        let total = self.len();
        let mut r = 0i32;
        loop {
            // Bound for cell-level pruning: once we hold k candidates, a cell whose
            // nearest possible point is farther than the current k-th best cannot
            // contain a closer node — skip scoring its nodes entirely. Computed once
            // per ring (conservative: the true k-th only shrinks as we scan).
            let bound = if heap.len() >= k { heap.peek().map(|(d, _)| d.0).unwrap_or(f64::INFINITY) } else { f64::INFINITY };
            // Scan every cell at Chebyshev distance exactly `r` (the new ring).
            let visit = |cy: i32, cx: i32, heap: &mut std::collections::BinaryHeap<(Dist, u64)>, seen: &mut usize| {
                if let Some(hashes) = self.cell_members_at((cy, cx)) {
                    // Nearest possible distance from the query to this cell's box.
                    if bound.is_finite() {
                        let dlat = if lat < cy as f64 * cs { cy as f64 * cs - lat }
                                   else if lat > (cy + 1) as f64 * cs { lat - (cy + 1) as f64 * cs } else { 0.0 };
                        let dlon = if lon < cx as f64 * cs { cx as f64 * cs - lon }
                                   else if lon > (cx + 1) as f64 * cs { lon - (cx + 1) as f64 * cs } else { 0.0 };
                        // Conservative metres-per-degree (110_000 < the true WGS84
                        // minimum ≈110_574) → cell_min UNDER-estimates the geodesic
                        // distance, so a cell is only ever pruned when it truly cannot
                        // hold a closer node. Heap distances are geodesic metres.
                        let cell_min = ((dlat * 110_000.0).powi(2) + (dlon * 110_000.0 * coslat).powi(2)).sqrt();
                        if cell_min > bound { *seen += hashes.len(); return; }
                    }
                    for &h in &hashes {
                        if let Some(m) = self.meta_at(h) {
                            *seen += 1;
                            let d = geodesic_distance_m(lat, lon, m.centroid_lat, m.centroid_lon);
                            heap.push((Dist(d), h));
                            if heap.len() > k { heap.pop(); }
                        }
                    }
                }
            };
            if r == 0 {
                visit(cy0, cx0, &mut heap, &mut seen);
            } else {
                for cx in (cx0 - r)..=(cx0 + r) {
                    visit(cy0 - r, cx, &mut heap, &mut seen);
                    visit(cy0 + r, cx, &mut heap, &mut seen);
                }
                for cy in (cy0 - r + 1)..=(cy0 + r - 1) {
                    visit(cy, cx0 - r, &mut heap, &mut seen);
                    visit(cy, cx0 + r, &mut heap, &mut seen);
                }
            }

            // Lower bound on any unsearched node: distance from the query point to the
            // nearest edge of the box of cells within Chebyshev distance `r`.
            let box_min_lat = (cy0 - r) as f64 * cs;
            let box_max_lat = (cy0 + r + 1) as f64 * cs;
            let box_min_lon = (cx0 - r) as f64 * cs;
            let box_max_lon = (cx0 + r + 1) as f64 * cs;
            // Lower bound in geodesic metres (110_000 m/deg under-estimates the true
            // minimum, so we never stop before the true k-nearest are settled).
            let lb_m = ((lat - box_min_lat) * 110_000.0)
                .min((box_max_lat - lat) * 110_000.0)
                .min((lon - box_min_lon) * 110_000.0 * coslat)
                .min((box_max_lon - lon) * 110_000.0 * coslat);

            let kth = heap.peek().map(|(d, _)| d.0).unwrap_or(f64::INFINITY);
            if (heap.len() >= k && kth <= lb_m) || seen >= total || r > 2000 {
                break;
            }
            r += 1;
        }

        let mut v: Vec<(f64, u64)> = heap.into_iter().map(|(d, h)| (d.0, h)).collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        v.into_iter().map(|(_, h)| h).collect()
    }

    /// Return candidate node hashes whose bbox overlaps the query bbox.
    pub fn candidates_in_bbox(
        &self,
        min_lat: f64,
        min_lon: f64,
        max_lat: f64,
        max_lon: f64,
    ) -> Vec<u64> {
        let min_cell_lat = (min_lat / self.cell_size).floor() as i32;
        let max_cell_lat = (max_lat / self.cell_size).floor() as i32;
        let min_cell_lon = (min_lon / self.cell_size).floor() as i32;
        let max_cell_lon = (max_lon / self.cell_size).floor() as i32;

        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();

        for cy in min_cell_lat..=max_cell_lat {
            for cx in min_cell_lon..=max_cell_lon {
                if let Some(hashes) = self.cell_members_at((cy, cx)) {
                    for h in hashes {
                        if seen.insert(h) {
                            // Bbox overlap check against the node's actual bbox
                            if let Some(m) = self.meta_at(h) {
                                if m.bbox_max_lat >= min_lat
                                    && m.bbox_min_lat <= max_lat
                                    && m.bbox_max_lon >= min_lon
                                    && m.bbox_min_lon <= max_lon
                                {
                                    result.push(h);
                                }
                            }
                        }
                    }
                }
            }
        }

        result
    }

    /// Return candidate node hashes whose bbox could contain `(lat, lon)`.
    /// Checks the point's cell plus 8 neighbours, then applies bbox pre-filter.
    pub fn candidates_containing_point(&self, lat: f64, lon: f64) -> Vec<u64> {
        let cy = (lat / self.cell_size).floor() as i32;
        let cx = (lon / self.cell_size).floor() as i32;

        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();

        for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                if let Some(hashes) = self.cell_members_at((cy + dy, cx + dx)) {
                    for h in hashes {
                        if seen.insert(h) {
                            if let Some(m) = self.meta_at(h) {
                                if lat >= m.bbox_min_lat
                                    && lat <= m.bbox_max_lat
                                    && lon >= m.bbox_min_lon
                                    && lon <= m.bbox_max_lon
                                {
                                    result.push(h);
                                }
                            }
                        }
                    }
                }
            }
        }

        result
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    fn cell_key(&self, lat: f64, lon: f64) -> (i32, i32) {
        (
            (lat / self.cell_size).floor() as i32,
            (lon / self.cell_size).floor() as i32,
        )
    }

    fn cells_for_bbox(&self, meta: &SpatialMeta) -> Vec<(i32, i32)> {
        let min_cy = (meta.bbox_min_lat / self.cell_size).floor() as i32;
        let max_cy = (meta.bbox_max_lat / self.cell_size).floor() as i32;
        let min_cx = (meta.bbox_min_lon / self.cell_size).floor() as i32;
        let max_cx = (meta.bbox_max_lon / self.cell_size).floor() as i32;

        // Cap at 10,000 cells to avoid blow-up for huge polygons
        let cell_count = (max_cy - min_cy + 1) as u64 * (max_cx - min_cx + 1) as u64;
        if cell_count > 10_000 {
            // Fall back to centroid cell only
            return vec![self.cell_key(meta.centroid_lat, meta.centroid_lon)];
        }

        let mut keys = Vec::with_capacity(cell_count as usize);
        for cy in min_cy..=max_cy {
            for cx in min_cx..=max_cx {
                keys.push((cy, cx));
            }
        }
        keys
    }

    fn insert_into_cells(&mut self, hash: u64, meta: &SpatialMeta) {
        let keys = self.cells_for_bbox(meta);
        for key in keys {
            self.cells.entry(key).or_default().push(hash);
        }
    }
}

// ── Point-in-polygon (ray casting) ───────────────────────────────────────────

/// Test whether a point is inside a polygon ring using the ray-casting algorithm.
///
/// Ring format: `[[lat, lon], ...]` (internal format, NOT GeoJSON `[lon, lat]`).
pub fn point_in_polygon(lat: f64, lon: f64, ring: &[[f64; 2]]) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (yi, xi) = (ring[i][0], ring[i][1]);
        let (yj, xj) = (ring[j][0], ring[j][1]);
        if ((yi > lat) != (yj > lat)) && (lon < (xj - xi) * (lat - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Bearing (radians) from `(lat1,lon1)` to `(lat2,lon2)` along the great circle.
fn bearing_rad(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    y.atan2(x)
}

/// Minimum geodesic distance (metres) from point P to the great-circle segment
/// A→B. Spherical (consistent with [`haversine_km`]); all coords are `(lat, lon)`.
/// Cross-track distance, clamped to the segment endpoints.
pub fn point_to_segment_m(
    plat: f64, plon: f64, alat: f64, alon: f64, blat: f64, blon: f64,
) -> f64 {
    const R: f64 = 6_371_008.8; // mean Earth radius (m)
    let da = haversine_km(alat, alon, plat, plon) * 1000.0;
    let dab = haversine_km(alat, alon, blat, blon) * 1000.0;
    if dab < 1e-6 {
        return da; // degenerate segment (A == B)
    }
    let db = haversine_km(blat, blon, plat, plon) * 1000.0;
    let d13 = da / R; // angular distance A→P
    let dxt = (d13.sin()
        * (bearing_rad(alat, alon, plat, plon) - bearing_rad(alat, alon, blat, blon)).sin())
    .asin(); // cross-track angular distance
    let dat = (d13.cos() / dxt.cos()).acos(); // along-track angular distance
    if !dat.is_finite() || dat < 0.0 {
        da // foot of perpendicular is before A → nearest is A
    } else if dat > dab / R {
        db // foot is beyond B → nearest is B
    } else {
        dxt.abs() * R // perpendicular distance to the great circle
    }
}

/// Minimum geodesic distance (metres) from point P to a polygon given as rings:
/// `0.0` if P is inside any ring, else the nearest edge distance. This matches
/// PostGIS `ST_DWithin(geography)` semantics (nearest boundary, not centroid).
/// Coords are `(lat, lon)`.
pub fn min_ring_distance_m(plat: f64, plon: f64, rings: &[Vec<[f64; 2]>]) -> f64 {
    if rings.iter().any(|r| point_in_polygon(plat, plon, r)) {
        return 0.0;
    }
    let mut best = f64::MAX;
    for r in rings {
        for w in r.windows(2) {
            let d = point_to_segment_m(plat, plon, w[0][0], w[0][1], w[1][0], w[1][1]);
            if d < best {
                best = d;
            }
        }
    }
    best
}

// ── Segment intersection ─────────────────────────────────────────────────────

/// Test whether two line segments intersect.
/// Points are `[lat, lon]`.
fn segments_intersect(a1: [f64; 2], a2: [f64; 2], b1: [f64; 2], b2: [f64; 2]) -> bool {
    let d1 = cross(a1, a2, b1);
    let d2 = cross(a1, a2, b2);
    let d3 = cross(b1, b2, a1);
    let d4 = cross(b1, b2, a2);

    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
    {
        return true;
    }

    // Collinear cases
    if d1 == 0.0 && on_segment(a1, a2, b1) {
        return true;
    }
    if d2 == 0.0 && on_segment(a1, a2, b2) {
        return true;
    }
    if d3 == 0.0 && on_segment(b1, b2, a1) {
        return true;
    }
    if d4 == 0.0 && on_segment(b1, b2, a2) {
        return true;
    }

    false
}

/// Cross product of vectors (p2-p1) x (p3-p1).
fn cross(p1: [f64; 2], p2: [f64; 2], p3: [f64; 2]) -> f64 {
    (p2[0] - p1[0]) * (p3[1] - p1[1]) - (p2[1] - p1[1]) * (p3[0] - p1[0])
}

/// Check if point `p` lies on segment `a`–`b` (assuming collinear).
fn on_segment(a: [f64; 2], b: [f64; 2], p: [f64; 2]) -> bool {
    p[0] >= a[0].min(b[0])
        && p[0] <= a[0].max(b[0])
        && p[1] >= a[1].min(b[1])
        && p[1] <= a[1].max(b[1])
}

// ── GeoJSON helpers ──────────────────────────────────────────────────────────

/// Flatten any GeoJSON geometry into a list of `[lat, lon]` points.
fn extract_geojson_coords(geom: &Value) -> Vec<[f64; 2]> {
    let geo_type = match geom.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return vec![],
    };
    let coords = match geom.get("coordinates") {
        Some(c) => c,
        None => return vec![],
    };

    match geo_type {
        "Point" => {
            // [lon, lat]
            if let (Some(lon), Some(lat)) = (
                coords.get(0).and_then(|v| v.as_f64()),
                coords.get(1).and_then(|v| v.as_f64()),
            ) {
                vec![[lat, lon]]
            } else {
                vec![]
            }
        }
        "LineString" | "MultiPoint" => {
            // [[lon, lat], ...]
            flatten_coord_array(coords)
        }
        "Polygon" => {
            // [[[lon, lat], ...], ...]  — first ring is outer
            coords
                .as_array()
                .map(|rings| {
                    rings
                        .iter()
                        .flat_map(|ring| flatten_coord_array(ring))
                        .collect()
                })
                .unwrap_or_default()
        }
        "MultiLineString" => coords
            .as_array()
            .map(|lines| {
                lines
                    .iter()
                    .flat_map(|line| flatten_coord_array(line))
                    .collect()
            })
            .unwrap_or_default(),
        "MultiPolygon" => {
            // [[[[lon, lat], ...], ...], ...]
            coords
                .as_array()
                .map(|polys| {
                    polys
                        .iter()
                        .flat_map(|poly| {
                            poly.as_array()
                                .map(|rings| {
                                    rings
                                        .iter()
                                        .flat_map(|ring| flatten_coord_array(ring))
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        _ => vec![],
    }
}

/// Convert a GeoJSON coordinate array `[[lon, lat], ...]` to `Vec<[lat, lon]>`.
fn flatten_coord_array(arr: &Value) -> Vec<[f64; 2]> {
    arr.as_array()
        .map(|pts| {
            pts.iter()
                .filter_map(|p| {
                    let lon = p.get(0)?.as_f64()?;
                    let lat = p.get(1)?.as_f64()?;
                    Some([lat, lon])
                })
                .collect()
        })
        .unwrap_or_default()
}

/// For Polygon/MultiPolygon, return the outer rings in `[lat, lon]` internal format.
fn extract_polygon_rings(geom: &Value) -> Vec<Vec<[f64; 2]>> {
    let geo_type = match geom.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return vec![],
    };
    let coords = match geom.get("coordinates") {
        Some(c) => c,
        None => return vec![],
    };

    match geo_type {
        "Polygon" => {
            // First element is the outer ring
            coords
                .as_array()
                .and_then(|rings| rings.first())
                .map(|ring| vec![flatten_coord_array(ring)])
                .unwrap_or_default()
        }
        "MultiPolygon" => coords
            .as_array()
            .map(|polys| {
                polys
                    .iter()
                    .filter_map(|poly| {
                        poly.as_array()?
                            .first()
                            .map(|ring| flatten_coord_array(ring))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    }
}

/// Parse a payload's geometry into polygon rings (`[[lat,lon],…]`), for caching in the
/// spatial grid. Empty if the geometry is not a Polygon/MultiPolygon.
pub fn rings_from_payload(payload: &Value) -> Vec<Vec<[f64; 2]>> {
    find_geometry(payload).map(extract_polygon_rings).unwrap_or_default()
}

// ── High-level predicates ────────────────────────────────────────────────────

/// Node geometry contains query point (reverse geocoding).
///
/// For Polygon: point-in-polygon test.
/// For MultiPolygon: any polygon contains the point.
pub fn geom_contains_point(payload: &Value, lat: f64, lon: f64) -> bool {
    let geom = match find_geometry(payload) {
        Some(g) => g,
        None => return false,
    };
    let rings = extract_polygon_rings(geom);
    rings.iter().any(|ring| point_in_polygon(lat, lon, ring))
}

/// Node geometry completely within query polygon.
///
/// For Point: centroid inside ring.
/// For Polygon/LineString: all vertices inside ring.
pub fn geom_within_polygon(payload: &Value, ring: &[[f64; 2]]) -> bool {
    let geom = match find_geometry(payload) {
        Some(g) => g,
        None => return false,
    };
    let coords = extract_geojson_coords(geom);
    if coords.is_empty() {
        return false;
    }
    coords.iter().all(|c| point_in_polygon(c[0], c[1], ring))
}

/// Node geometry intersects query polygon.
///
/// True if: any vertex of node inside query, or any vertex of query inside node,
/// or any edge of node crosses any edge of query.
pub fn geom_intersects_polygon(payload: &Value, ring: &[[f64; 2]]) -> bool {
    let geom = match find_geometry(payload) {
        Some(g) => g,
        None => return false,
    };

    let node_coords = extract_geojson_coords(geom);
    if node_coords.is_empty() {
        return false;
    }

    // Any vertex of node geometry inside query polygon
    if node_coords
        .iter()
        .any(|c| point_in_polygon(c[0], c[1], ring))
    {
        return true;
    }

    // Any vertex of query polygon inside node geometry (if node is a polygon)
    let node_rings = extract_polygon_rings(geom);
    for nr in &node_rings {
        if ring.iter().any(|c| point_in_polygon(c[0], c[1], nr)) {
            return true;
        }
    }

    // Edge crossing: any segment of node geometry crosses any segment of query polygon
    let node_edges = edges_from_coords(&node_coords);
    let query_edges = edges_from_ring(ring);
    for (a1, a2) in &node_edges {
        for (b1, b2) in &query_edges {
            if segments_intersect(*a1, *a2, *b1, *b2) {
                return true;
            }
        }
    }

    false
}

/// Node geometry contains query polygon.
///
/// All query polygon vertices must be inside the node's geometry.
pub fn geom_contains_polygon(payload: &Value, ring: &[[f64; 2]]) -> bool {
    let geom = match find_geometry(payload) {
        Some(g) => g,
        None => return false,
    };
    let node_rings = extract_polygon_rings(geom);
    if node_rings.is_empty() || ring.is_empty() {
        return false;
    }
    // All query vertices must be inside at least one of the node's polygon rings
    ring.iter()
        .all(|c| node_rings.iter().any(|nr| point_in_polygon(c[0], c[1], nr)))
}

// ── Edge helpers ─────────────────────────────────────────────────────────────

/// Build edges from a list of coordinates (connecting consecutive pairs).
fn edges_from_coords(coords: &[[f64; 2]]) -> Vec<([f64; 2], [f64; 2])> {
    if coords.len() < 2 {
        return vec![];
    }
    coords.windows(2).map(|w| (w[0], w[1])).collect()
}

/// Build edges from a polygon ring (including the closing edge).
fn edges_from_ring(ring: &[[f64; 2]]) -> Vec<([f64; 2], [f64; 2])> {
    if ring.len() < 2 {
        return vec![];
    }
    let mut edges: Vec<([f64; 2], [f64; 2])> = ring.windows(2).map(|w| (w[0], w[1])).collect();
    // Close the ring
    if let (Some(&first), Some(&last)) = (ring.first(), ring.last()) {
        if first != last {
            edges.push((last, first));
        }
    }
    edges
}

// ── EWKB serialization (PostGIS `ST_AsEWKB`) ─────────────────────────────────

/// Serialize a GeoJSON geometry to **EWKB hex** — the text form PostGIS emits over
/// the wire (little-endian, SRID-tagged). Enables any PostGIS-aware client (DBeaver,
/// QGIS) to render sekejap geometries on a map. Returns `None` if `geom` is not a
/// recognized GeoJSON geometry (Point/LineString/Polygon and their Multi* forms,
/// plus GeometryCollection).
pub fn geojson_to_ewkb_hex(geom: &Value, srid: u32) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    write_geometry(&mut buf, geom, Some(srid))?;
    let mut out = String::with_capacity(buf.len() * 2);
    for b in &buf {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    Some(out.to_ascii_uppercase())
}

/// Write one geometry in WKB. `srid` is `Some` only for the top-level geometry
/// (EWKB tags SRID once); nested geometries in Multi*/collections pass `None`.
fn write_geometry(buf: &mut Vec<u8>, geom: &Value, srid: Option<u32>) -> Option<()> {
    let ty = geom.get("type")?.as_str()?;
    let code: u32 = match ty {
        "Point" => 1, "LineString" => 2, "Polygon" => 3,
        "MultiPoint" => 4, "MultiLineString" => 5, "MultiPolygon" => 6,
        "GeometryCollection" => 7,
        _ => return None,
    };
    buf.push(1); // NDR (little-endian)
    let flagged = if srid.is_some() { code | 0x2000_0000 } else { code };
    buf.extend_from_slice(&flagged.to_le_bytes());
    if let Some(s) = srid {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    match ty {
        "Point" => write_position(buf, geom.get("coordinates")?)?,
        "LineString" => write_position_seq(buf, geom.get("coordinates")?)?,
        "Polygon" => write_rings(buf, geom.get("coordinates")?)?,
        "MultiPoint" => {
            let pts = geom.get("coordinates")?.as_array()?;
            buf.extend_from_slice(&(pts.len() as u32).to_le_bytes());
            for p in pts { buf.push(1); buf.extend_from_slice(&1u32.to_le_bytes()); write_position(buf, p)?; }
        }
        "MultiLineString" => {
            let lines = geom.get("coordinates")?.as_array()?;
            buf.extend_from_slice(&(lines.len() as u32).to_le_bytes());
            for l in lines { buf.push(1); buf.extend_from_slice(&2u32.to_le_bytes()); write_position_seq(buf, l)?; }
        }
        "MultiPolygon" => {
            let polys = geom.get("coordinates")?.as_array()?;
            buf.extend_from_slice(&(polys.len() as u32).to_le_bytes());
            for p in polys { buf.push(1); buf.extend_from_slice(&3u32.to_le_bytes()); write_rings(buf, p)?; }
        }
        "GeometryCollection" => {
            let geoms = geom.get("geometries")?.as_array()?;
            buf.extend_from_slice(&(geoms.len() as u32).to_le_bytes());
            for g in geoms { write_geometry(buf, g, None)?; }
        }
        _ => return None,
    }
    Some(())
}

/// A single position `[x, y]` (GeoJSON `[lon, lat]`) as two little-endian f64.
fn write_position(buf: &mut Vec<u8>, pos: &Value) -> Option<()> {
    let a = pos.as_array()?;
    let x = a.first()?.as_f64()?;
    let y = a.get(1)?.as_f64()?;
    buf.extend_from_slice(&x.to_le_bytes());
    buf.extend_from_slice(&y.to_le_bytes());
    Some(())
}

/// A count-prefixed sequence of positions (a LineString or a Polygon ring).
fn write_position_seq(buf: &mut Vec<u8>, seq: &Value) -> Option<()> {
    let pts = seq.as_array()?;
    buf.extend_from_slice(&(pts.len() as u32).to_le_bytes());
    for p in pts { write_position(buf, p)?; }
    Some(())
}

/// A count-prefixed sequence of rings (a Polygon).
fn write_rings(buf: &mut Vec<u8>, rings: &Value) -> Option<()> {
    let rs = rings.as_array()?;
    buf.extend_from_slice(&(rs.len() as u32).to_le_bytes());
    for r in rs { write_position_seq(buf, r)?; }
    Some(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn geodesic_distance_vincenty_reference() {
        // Unambiguous WGS84 references (exact by construction, no DMS rounding):
        //   1° of longitude at the equator = a·π/180 = 111319.4908 m.
        let deg_lon = geodesic_distance_m(0.0, 0.0, 0.0, 1.0);
        assert!((deg_lon - 111319.4908).abs() < 0.01, "got {deg_lon}");
        //   1° of latitude at the equator (meridian arc) = 110574.389 m.
        let deg_lat = geodesic_distance_m(0.0, 0.0, 1.0, 0.0);
        assert!((deg_lat - 110574.389).abs() < 0.05, "got {deg_lat}");
        // Coincident points → 0.
        assert_eq!(geodesic_distance_m(40.0, -73.0, 40.0, -73.0), 0.0);
        // Sanity: Flinders Peak → Buninyong ≈ 54972 m (Vincenty's own test pair).
        let d = geodesic_distance_m(-37.95103, 144.42487, -37.65282, 143.92650);
        assert!((d - 54972.0).abs() < 2.0, "got {d}");
    }

    #[test]
    fn geodesic_matches_postgis_geography() {
        // Reference values captured from LIVE PostGIS `ST_*(::geography)` (WGS84).
        // A 0.01°×0.01° cell at NYC (lat 40.70) and one point-to-point distance.
        let poly = json!({"type":"Polygon","coordinates":[[
            [-74.00,40.70],[-73.99,40.70],[-73.99,40.71],[-74.00,40.71],[-74.00,40.70]]]});
        let a = area_m2(&poly).unwrap();
        assert!((a - 938459.4059114456).abs() / 938459.406 < 1e-6, "area {a}");
        let p = perimeter_m(&poly).unwrap();
        assert!((p - 3911.147957345263).abs() / 3911.148 < 1e-6, "perim {p}");
        let d = geodesic_distance_m(40.70, -74.00, 40.75, -73.95);
        assert!((d - 6976.62506433).abs() < 0.001, "dist {d}");
    }

    #[test]
    fn ewkb_point_matches_postgis() {
        // ST_AsEWKB(ST_SetSRID(ST_MakePoint(30,10),4326)) in PostGIS.
        let g = json!({ "type": "Point", "coordinates": [30.0, 10.0] });
        assert_eq!(
            geojson_to_ewkb_hex(&g, 4326).unwrap(),
            "0101000020E61000000000000000003E400000000000002440"
        );
    }

    #[test]
    fn ewkb_polygon_roundtrips_structurally() {
        let g = json!({
            "type": "Polygon",
            "coordinates": [[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,0.0]]]
        });
        let hex = geojson_to_ewkb_hex(&g, 4326).unwrap();
        // 01 (LE) + 03000020 (polygon+srid) + E6100000 (4326) + 01000000 (1 ring) + 04000000 (4 pts)
        assert!(hex.starts_with("0103000020E61000000100000004000000"));
    }

    #[test]
    fn test_haversine_melbourne_to_geelong() {
        // Melbourne CBD to Geelong ~ 65 km
        let d = haversine_km(-37.8136, 144.9631, -38.1499, 144.3617);
        assert!((d - 65.0).abs() < 5.0, "expected ~65km, got {d}km");
    }

    #[test]
    fn test_haversine_same_point() {
        let d = haversine_km(-37.8136, 144.9631, -37.8136, 144.9631);
        assert!(d < 0.001);
    }

    #[test]
    fn test_point_in_polygon_inside() {
        // Simple square around Melbourne CBD
        let ring = [
            [-37.80, 144.95],
            [-37.80, 144.98],
            [-37.83, 144.98],
            [-37.83, 144.95],
        ];
        assert!(point_in_polygon(-37.81, 144.96, &ring));
    }

    #[test]
    fn test_point_in_polygon_outside() {
        let ring = [
            [-37.80, 144.95],
            [-37.80, 144.98],
            [-37.83, 144.98],
            [-37.83, 144.95],
        ];
        // Geelong is outside
        assert!(!point_in_polygon(-38.15, 144.36, &ring));
    }

    #[test]
    fn nearest_boundary_beats_centroid() {
        // Square [lat,lon]: lat 0..0.1, lon 0..0.1; centroid ≈ (0.05, 0.05).
        let rings = vec![vec![[0.0, 0.0], [0.0, 0.1], [0.1, 0.1], [0.1, 0.0], [0.0, 0.0]]];
        // Point just west of the lon=0 edge at lat 0.05: nearest boundary ~110 m,
        // but the centroid is ~0.05° (~5.5 km) away. This is the ST_DWithin bug the
        // PostGIS comparison caught — centroid distance would wrongly exclude it.
        let (plat, plon) = (0.05, -0.001);
        let d = min_ring_distance_m(plat, plon, &rings);
        let centroid_d = geodesic_distance_m(0.05, 0.05, plat, plon);
        assert!(d < centroid_d, "boundary {d} must be < centroid {centroid_d}");
        assert!(d < 200.0, "point ~110 m from the edge, got {d} m");
        // A point inside the polygon has distance 0 (PostGIS semantics).
        assert_eq!(min_ring_distance_m(0.05, 0.05, &rings), 0.0);
    }

    #[test]
    fn test_extract_centroid_point() {
        let payload = json!({
            "geometry": {
                "type": "Point",
                "coordinates": [144.9631, -37.8136]
            }
        });
        let (lat, lon) = extract_centroid(&payload).unwrap();
        assert!((lat - (-37.8136)).abs() < 1e-4);
        assert!((lon - 144.9631).abs() < 1e-4);
    }

    #[test]
    fn test_extract_centroid_polygon() {
        let payload = json!({
            "geometry": {
                "type": "Polygon",
                "coordinates": [[
                    [144.95, -37.80],
                    [144.98, -37.80],
                    [144.98, -37.83],
                    [144.95, -37.83],
                    [144.95, -37.80]
                ]]
            }
        });
        let (lat, lon) = extract_centroid(&payload).unwrap();
        // Average of all 5 vertices (including closing = first)
        assert!((lat - (-37.812)).abs() < 0.01, "lat={lat}");
        assert!((lon - 144.962).abs() < 0.01, "lon={lon}");
    }

    #[test]
    fn test_extract_centroid_multipoint() {
        let payload = json!({
            "geometry": {
                "type": "MultiPoint",
                "coordinates": [
                    [144.9631, -37.8136],
                    [144.9700, -37.8200],
                    [144.9800, -37.8300]
                ]
            }
        });
        let (lat, lon) = extract_centroid(&payload).unwrap();
        assert!((lat - (-37.8212)).abs() < 0.001, "lat={lat}");
        assert!((lon - 144.9710).abs() < 0.001, "lon={lon}");
    }

    #[test]
    fn test_extract_centroid_multipolygon() {
        let payload = json!({
            "geometry": {
                "type": "MultiPolygon",
                "coordinates": [[
                    [[144.95, -37.80], [144.98, -37.80], [144.98, -37.83], [144.95, -37.83], [144.95, -37.80]]
                ], [
                    [[145.00, -37.85], [145.03, -37.85], [145.03, -37.88], [145.00, -37.88], [145.00, -37.85]]
                ]]
            }
        });
        let (lat, lon) = extract_centroid(&payload).unwrap();
        assert!((lat - (-37.84)).abs() < 0.01, "lat={lat}");
        assert!((lon - 144.99).abs() < 0.01, "lon={lon}");
    }

    #[test]
    fn test_extract_centroid_multilinestring() {
        let payload = json!({
            "geometry": {
                "type": "MultiLineString",
                "coordinates": [
                    [[144.95, -37.80], [144.98, -37.80]],
                    [[144.96, -37.81], [144.99, -37.81]]
                ]
            }
        });
        let (lat, lon) = extract_centroid(&payload).unwrap();
        assert!((lat - (-37.805)).abs() < 0.01, "lat={lat}");
        assert!((lon - 144.97).abs() < 0.01, "lon={lon}");
    }

    #[test]
    fn test_segments_intersect() {
        // X-shaped crossing
        assert!(segments_intersect(
            [0.0, 0.0],
            [1.0, 1.0],
            [0.0, 1.0],
            [1.0, 0.0],
        ));
    }

    #[test]
    fn test_segments_no_intersect() {
        // Parallel segments
        assert!(!segments_intersect(
            [0.0, 0.0],
            [1.0, 0.0],
            [0.0, 1.0],
            [1.0, 1.0],
        ));
    }

    #[test]
    fn test_geom_contains_point_polygon() {
        let payload = json!({
            "geometry": {
                "type": "Polygon",
                "coordinates": [[
                    [144.95, -37.80],
                    [144.98, -37.80],
                    [144.98, -37.83],
                    [144.95, -37.83],
                    [144.95, -37.80]
                ]]
            }
        });
        assert!(geom_contains_point(&payload, -37.81, 144.96));
        assert!(!geom_contains_point(&payload, -38.15, 144.36));
    }

    #[test]
    fn test_geom_within_polygon() {
        let ring = [
            [-37.80, 144.94],
            [-37.80, 144.99],
            [-37.84, 144.99],
            [-37.84, 144.94],
        ];
        // Point inside big ring
        let payload = json!({
            "geometry": {
                "type": "Point",
                "coordinates": [144.96, -37.81]
            }
        });
        assert!(geom_within_polygon(&payload, &ring));

        // Point outside big ring
        let outside = json!({
            "geometry": {
                "type": "Point",
                "coordinates": [145.50, -38.00]
            }
        });
        assert!(!geom_within_polygon(&outside, &ring));
    }

    #[test]
    fn test_geom_intersects_polygon() {
        // A line that crosses a query rectangle
        let payload = json!({
            "geometry": {
                "type": "LineString",
                "coordinates": [
                    [144.94, -37.81],
                    [144.99, -37.81]
                ]
            }
        });
        let ring = [
            [-37.80, 144.95],
            [-37.80, 144.98],
            [-37.83, 144.98],
            [-37.83, 144.95],
        ];
        assert!(geom_intersects_polygon(&payload, &ring));
    }
}
