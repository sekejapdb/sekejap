//! 2i: PostGIS-geography math, ported from e1 (already calibrated equal to
//! PostGIS to float precision there). Distances = Vincenty's inverse
//! formula on the WGS84 ellipsoid, in METRES; areas = spherical excess on
//! the authalic sphere, in SQUARE METRES; point-in-polygon = planar
//! even-odd crossing in coordinate space (the named subset deviation:
//! correct away from poles/antimeridian; geography-PostGIS itself offers
//! ST_Covers for the sphere-true predicate). Functions take (lat, lon) --
//! PostGIS textual order; GeoJSON stores [lon, lat] and converters own
//! the flip. Rings use the internal [[lat, lon], ...] layout.


/// WGS84 defining parameters.
const WGS84_A: f64 = 6_378_137.0;                 // semi-major axis (m)
const WGS84_F: f64 = 1.0 / 298.257_223_563;       // flattening
/// WGS84 first eccentricity squared, e² = f(2−f).
const WGS84_E2: f64 = WGS84_F * (2.0 - WGS84_F);
/// WGS84 authalic (equal-area) sphere radius (m) — the sphere with the same
/// surface area as the ellipsoid; used for geodesic polygon area.
const WGS84_AUTHALIC_R: f64 = 6_371_007.180_918_47;
/// Great-circle (sphere) distance in KILOMETRES -- the cheap pre-filter:
/// diverges from Vincenty/WGS84 by < 0.56%, so any comparison further
/// than that band from a threshold can be decided here without the
/// iterative formula.
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6371.0;
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();
    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
    R * 2.0 * a.sqrt().asin()
}


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

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference values captured from LIVE PostGIS `ST_*(::geography)`
    /// (WGS84) -- carried over from the e1 calibration suite. These pin
    /// the port bit-for-bit against the already-verified math.
    #[test]
    fn vincenty_matches_postgis_geography() {
        // Vincenty's own published test pair (Flinders Peak -> Buninyong).
        let d = geodesic_distance_m(-37.95103, 144.42487, -37.65282, 143.92650);
        assert!((d - 54972.0).abs() < 2.0, "got {d}");
        // Live-PostGIS point pair at NYC.
        let d = geodesic_distance_m(40.70, -74.00, 40.75, -73.95);
        assert!((d - 6976.62506433).abs() < 0.001, "dist {d}");
        assert_eq!(geodesic_distance_m(40.0, -73.0, 40.0, -73.0), 0.0);
    }

    #[test]
    fn ring_area_and_perimeter_match_postgis() {
        // A 0.01x0.01 degree cell at NYC, rings as [lat, lon].
        let ring = vec![
            [40.70, -74.00], [40.70, -73.99], [40.71, -73.99],
            [40.71, -74.00], [40.70, -74.00],
        ];
        let a = geodesic_ring_area_m2(&ring);
        assert!((a - 938459.4059114456).abs() / 938459.406 < 1e-6, "area {a}");
        let p = geodesic_path_length_m(&ring);
        assert!((p - 3911.147957345263).abs() / 3911.148 < 1e-6, "perimeter {p}");
    }

    #[test]
    fn point_in_polygon_in_and_out() {
        let ring = [
            [-37.80, 144.95], [-37.80, 144.98],
            [-37.83, 144.98], [-37.83, 144.95],
        ];
        assert!(point_in_polygon(-37.81, 144.96, &ring));
        assert!(!point_in_polygon(-38.15, 144.36, &ring));
    }
}
