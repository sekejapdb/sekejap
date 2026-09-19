//! Pure WGS84 point predicates and bounded Hilbert candidate covers.
//!
//! The Hilbert ranges in this module are candidate filters. Callers must apply
//! [`Bounds::contains`] or [`within_radius`] to the exact stored `f64` point.

use geographiclib_rs::{Geodesic, InverseGeodesic};
use kernel::spatial::{cell_hilbert, cell_of, cover_ranges};

/// Persisted spatial point grid: 16 bits for each axis, 32 Hilbert bits total.
pub const HILBERT_BITS: u8 = 16;
/// Maximum number of disjoint ranges returned by one candidate cover.
pub const MAX_HILBERT_RANGES: usize = 64;
pub const MAX_HILBERT_VALUE: u64 = (1u64 << (2 * HILBERT_BITS)) - 1;

// The minimum WGS84 meridional curvature radius is a(1-e^2), at the equator.
// It also globally lower-bounds the prime-vertical curvature radius.
pub const WGS84_MIN_CURVATURE_RADIUS_METRES: f64 = 6_335_439.327_292_819_5;
const OUTWARD_DEGREES: f64 = 1e-10;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    lon: f64,
    lat: f64,
}

impl Point {
    pub fn new(lon: f64, lat: f64) -> Result<Self, &'static str> {
        validate_lon(lon)?;
        validate_lat(lat)?;
        Ok(Self { lon, lat })
    }

    pub fn longitude(self) -> f64 {
        self.lon
    }

    pub fn latitude(self) -> f64 {
        self.lat
    }
}

/// An inclusive WGS84 rectangle. `west > east` explicitly crosses the
/// dateline. Longitude endpoints `-180` and `180` denote the same meridian.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    west: f64,
    east: f64,
    south: f64,
    north: f64,
}

impl Bounds {
    pub fn new(west: f64, east: f64, south: f64, north: f64) -> Result<Self, &'static str> {
        validate_lon(west)?;
        validate_lon(east)?;
        validate_lat(south)?;
        validate_lat(north)?;
        if south > north {
            return Err("south latitude exceeds north latitude");
        }
        Ok(Self {
            west,
            east,
            south,
            north,
        })
    }

    pub fn west(self) -> f64 {
        self.west
    }

    pub fn east(self) -> f64 {
        self.east
    }

    pub fn south(self) -> f64 {
        self.south
    }

    pub fn north(self) -> f64 {
        self.north
    }

    pub fn crosses_dateline(self) -> bool {
        self.west > self.east
    }

    pub fn contains(self, point: Point) -> bool {
        if point.lat < self.south || point.lat > self.north {
            return false;
        }
        let longitude_match = if self.crosses_dateline() {
            point.lon >= self.west || point.lon <= self.east
        } else {
            point.lon >= self.west && point.lon <= self.east
        };
        longitude_match
            || (point.lon == 180.0 && self.contains_negative_dateline())
            || (point.lon == -180.0 && self.contains_positive_dateline())
    }

    fn contains_negative_dateline(self) -> bool {
        if self.crosses_dateline() {
            self.east >= -180.0
        } else {
            self.west == -180.0
        }
    }

    fn contains_positive_dateline(self) -> bool {
        if self.crosses_dateline() {
            self.west <= 180.0
        } else {
            self.east == 180.0
        }
    }

    fn is_world(self) -> bool {
        !self.crosses_dateline() && self.west == -180.0 && self.east == 180.0
    }
}

fn validate_lon(lon: f64) -> Result<(), &'static str> {
    if !lon.is_finite() {
        return Err("longitude must be finite");
    }
    if !(-180.0..=180.0).contains(&lon) {
        return Err("longitude is outside [-180, 180]");
    }
    Ok(())
}

fn validate_lat(lat: f64) -> Result<(), &'static str> {
    if !lat.is_finite() {
        return Err("latitude must be finite");
    }
    if !(-90.0..=90.0).contains(&lat) {
        return Err("latitude is outside [-90, 90]");
    }
    Ok(())
}

fn validate_radius(radius_metres: f64) -> Result<(), &'static str> {
    if !radius_metres.is_finite() {
        return Err("radius must be finite");
    }
    if radius_metres < 0.0 {
        return Err("radius must be non-negative");
    }
    Ok(())
}

/// Exact WGS84 ellipsoidal distance in metres using Karney's convergent
/// inverse geodesic implementation.
pub fn wgs84_distance_metres(a: Point, b: Point) -> f64 {
    let geodesic = Geodesic::wgs84();
    geodesic.inverse(a.lat, a.lon, b.lat, b.lon)
}

/// Exact inclusive radius acceptance. Candidate range results must be refined
/// with this predicate.
pub fn within_radius(
    center: Point,
    point: Point,
    radius_metres: f64,
) -> Result<bool, &'static str> {
    validate_radius(radius_metres)?;
    Ok(wgs84_distance_metres(center, point) <= radius_metres)
}

/// Conservative longitude/latitude envelope for a WGS84 geodesic radius.
///
/// The WGS84 metric coefficients are both at least
/// `WGS84_MIN_CURVATURE_RADIUS_METRES`, so every ellipsoidal path is at least
/// that radius times its spherical central angle in geodetic coordinates.
/// The spherical cap with angle `radius / M_min` therefore contains every
/// exact WGS84 hit. A tiny degree margin is applied only to this candidate box.
pub fn radius_candidate_bounds(center: Point, radius_metres: f64) -> Result<Bounds, &'static str> {
    validate_radius(radius_metres)?;
    let angular = radius_metres / WGS84_MIN_CURVATURE_RADIUS_METRES;
    if angular >= core::f64::consts::PI {
        return Bounds::new(-180.0, 180.0, -90.0, 90.0);
    }

    let latitude = center.lat.to_radians();
    let south = (center.lat - angular.to_degrees() - OUTWARD_DEGREES).max(-90.0);
    let north = (center.lat + angular.to_degrees() + OUTWARD_DEGREES).min(90.0);
    if latitude - angular <= -core::f64::consts::FRAC_PI_2
        || latitude + angular >= core::f64::consts::FRAC_PI_2
    {
        return Bounds::new(-180.0, 180.0, south, north);
    }

    let ratio = (angular.sin() / latitude.cos()).clamp(-1.0, 1.0);
    let longitude_delta = ratio.asin().to_degrees() + OUTWARD_DEGREES;
    let west = normalize_longitude(center.lon - longitude_delta);
    let east = normalize_longitude(center.lon + longitude_delta);
    Bounds::new(west, east, south, north)
}

fn normalize_longitude(longitude: f64) -> f64 {
    let mut normalized = (longitude + 180.0).rem_euclid(360.0) - 180.0;
    // Preserve a positive dateline endpoint. This makes a cap ending exactly
    // at +180 display naturally; candidate generation handles both aliases.
    if normalized == -180.0 && longitude > 0.0 {
        normalized = 180.0;
    }
    normalized
}

pub fn point_hilbert(point: Point) -> u64 {
    let (x, y) = cell_of(point.lon, point.lat, HILBERT_BITS);
    cell_hilbert(x, y, HILBERT_BITS)
}

/// Bounded Hilbert candidate cover for a validated rectangle. Ranges are
/// sorted, merged, inclusive, and within the persisted 32-bit Hilbert space.
/// If the split covers cannot fit the fixed budget, the world range is used.
pub fn bounds_hilbert_ranges(bounds: Bounds) -> Vec<(u64, u64)> {
    bounds_hilbert_ranges_bounded(bounds, MAX_HILBERT_RANGES)
}

/// [`bounds_hilbert_ranges`] with its own range budget. A predicate wants a
/// tight cover (the default budget), because every covered posting outside
/// the box is examined and thrown away; an outward nearest-neighbour ring
/// wants a COARSE cover, because each range is a tree descent and a small
/// ring is dominated by seeks, not by postings. The budget is clamped to the
/// default; the world fallback rules are unchanged.
pub fn bounds_hilbert_ranges_bounded(bounds: Bounds, max_ranges: usize) -> Vec<(u64, u64)> {
    let max_ranges = max_ranges.clamp(4, MAX_HILBERT_RANGES);
    if bounds.is_world() {
        return world_range();
    }

    let mut longitude_strips = Vec::with_capacity(4);
    if bounds.crosses_dateline() {
        longitude_strips.push((bounds.west, 180.0));
        longitude_strips.push((-180.0, bounds.east));
    } else {
        longitude_strips.push((bounds.west, bounds.east));
        // Stored points preserve their original +/-180 spelling and occupy
        // opposite edge cells. Cover the alias cell for a touching rectangle.
        if bounds.west == -180.0 {
            longitude_strips.push((180.0, 180.0));
        }
        if bounds.east == 180.0 {
            longitude_strips.push((-180.0, -180.0));
        }
    }

    let mut ranges = Vec::new();
    for (west, east) in longitude_strips {
        ranges.extend(cover_ranges(
            west,
            east,
            bounds.south,
            bounds.north,
            HILBERT_BITS,
            max_ranges,
        ));
        if ranges.len() > MAX_HILBERT_RANGES.saturating_mul(2) {
            return world_range();
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        if lo > hi || hi > MAX_HILBERT_VALUE {
            return world_range();
        }
        match merged.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    if merged.len() > MAX_HILBERT_RANGES {
        world_range()
    } else {
        merged
    }
}

pub fn radius_hilbert_ranges(
    center: Point,
    radius_metres: f64,
) -> Result<Vec<(u64, u64)>, &'static str> {
    Ok(bounds_hilbert_ranges(radius_candidate_bounds(
        center,
        radius_metres,
    )?))
}

fn world_range() -> Vec<(u64, u64)> {
    vec![(0, MAX_HILBERT_VALUE)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(lon: f64, lat: f64) -> Point {
        Point::new(lon, lat).unwrap()
    }

    fn covered(ranges: &[(u64, u64)], point: Point) -> bool {
        let h = point_hilbert(point);
        ranges.iter().any(|&(lo, hi)| lo <= h && h <= hi)
    }

    fn close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "actual {actual}, expected {expected} +/- {tolerance}"
        );
    }

    #[test]
    fn geographiclib_wgs84_distance_goldens() {
        close(
            wgs84_distance_metres(p(0.0, 0.0), p(1.0, 0.0)),
            111_319.490_793_273_57,
            1e-6,
        );
        close(
            wgs84_distance_metres(p(0.0, 0.0), p(0.0, 1.0)),
            110_574.388_557_798_78,
            1e-6,
        );
        // GeographicLib GeodSolve76: a classic Vincenty failure.
        close(
            wgs84_distance_metres(
                p(174.0 + 49.0 / 60.0, -(41.0 + 19.0 / 60.0)),
                p(-(5.0 + 30.0 / 60.0), 40.0 + 58.0 / 60.0),
            ),
            19_960_543.857_179,
            1e-6,
        );
        close(
            wgs84_distance_metres(p(0.0, 90.0), p(0.0, -90.0)),
            20_003_931.458_625_447,
            1e-6,
        );
        close(
            wgs84_distance_metres(p(-180.0, 90.0), p(180.0, 90.0)),
            0.0,
            1e-8,
        );
    }

    #[test]
    fn inclusive_boxes_handle_dateline_and_aliases() {
        let wrapping = Bounds::new(170.0, -170.0, -5.0, 5.0).unwrap();
        for point in [
            p(170.0, -5.0),
            p(180.0, 0.0),
            p(-180.0, 0.0),
            p(-170.0, 5.0),
        ] {
            assert!(wrapping.contains(point));
        }
        assert!(!wrapping.contains(p(0.0, 0.0)));

        let negative_edge = Bounds::new(-180.0, -175.0, 1.0, 2.0).unwrap();
        assert!(negative_edge.contains(p(-180.0, 1.5)));
        assert!(negative_edge.contains(p(180.0, 1.5)));
        let positive_edge = Bounds::new(175.0, 180.0, 1.0, 2.0).unwrap();
        assert!(positive_edge.contains(p(-180.0, 1.5)));
        assert!(positive_edge.contains(p(180.0, 1.5)));

        assert!(covered(
            &bounds_hilbert_ranges(negative_edge),
            p(180.0, 1.5)
        ));
        assert!(covered(
            &bounds_hilbert_ranges(positive_edge),
            p(-180.0, 1.5)
        ));
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        for longitude in [f64::NAN, f64::INFINITY, -180.000_001, 180.000_001] {
            assert!(Point::new(longitude, 0.0).is_err());
        }
        for latitude in [f64::NAN, f64::NEG_INFINITY, -90.000_001, 90.000_001] {
            assert!(Point::new(0.0, latitude).is_err());
        }
        assert!(Bounds::new(0.0, 1.0, 2.0, 1.0).is_err());
        for radius in [f64::NAN, f64::INFINITY, -1.0] {
            assert!(radius_candidate_bounds(p(0.0, 0.0), radius).is_err());
            assert!(within_radius(p(0.0, 0.0), p(0.0, 0.0), radius).is_err());
        }
    }

    #[test]
    fn zero_and_world_radius_are_conservative_and_exact() {
        let center = p(180.0, 20.0);
        let zero = radius_candidate_bounds(center, 0.0).unwrap();
        assert!(zero.contains(center));
        assert!(covered(
            &radius_hilbert_ranges(center, 0.0).unwrap(),
            center
        ));
        assert!(within_radius(center, center, 0.0).unwrap());
        assert!(!within_radius(center, p(179.999, 20.0), 0.0).unwrap());

        let ranges = radius_hilbert_ranges(
            center,
            core::f64::consts::PI * WGS84_MIN_CURVATURE_RADIUS_METRES,
        )
        .unwrap();
        assert_eq!(ranges, world_range());
        let world = radius_candidate_bounds(center, 100_000_000.0).unwrap();
        assert_eq!(world, Bounds::new(-180.0, 180.0, -90.0, 90.0).unwrap());
    }

    #[test]
    fn bbox_covers_every_included_grid_and_random_point() {
        let boxes = [
            Bounds::new(-1.2, 2.4, -3.0, 4.0).unwrap(),
            Bounds::new(170.0, -165.0, -20.0, 30.0).unwrap(),
            Bounds::new(-180.0, -179.5, -90.0, 90.0).unwrap(),
            Bounds::new(179.5, 180.0, 80.0, 90.0).unwrap(),
            Bounds::new(-180.0, 180.0, -90.0, 90.0).unwrap(),
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for bounds in boxes {
            let ranges = bounds_hilbert_ranges(bounds);
            assert!(!ranges.is_empty());
            assert!(ranges.len() <= MAX_HILBERT_RANGES);
            for &(lo, hi) in &ranges {
                assert!(lo <= hi && hi <= MAX_HILBERT_VALUE);
            }
            for lat_step in -18..=18 {
                for lon_step in -36..=36 {
                    let point = p(lon_step as f64 * 5.0, lat_step as f64 * 5.0);
                    if bounds.contains(point) {
                        assert!(
                            covered(&ranges, point),
                            "missed grid point {point:?} in {bounds:?}"
                        );
                    }
                }
            }
            for _ in 0..2_000 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let lon = -180.0 + 360.0 * ((seed >> 11) as f64 / ((1u64 << 53) as f64));
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let lat = -90.0 + 180.0 * ((seed >> 11) as f64 / ((1u64 << 53) as f64));
                let point = p(lon, lat);
                if bounds.contains(point) {
                    assert!(
                        covered(&ranges, point),
                        "missed random point {point:?} in {bounds:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn radius_envelope_contains_every_exact_grid_and_random_hit() {
        let cases = [
            (p(0.0, 0.0), 0.0),
            (p(179.8, 0.0), 80_000.0),
            (p(-179.8, -35.0), 500_000.0),
            (p(40.0, 88.0), 400_000.0),
            (p(-70.0, -89.0), 2_000_000.0),
            (p(10.0, 15.0), 12_000_000.0),
        ];
        let mut seed = 0xd1b5_4a32_d192_ed03u64;
        for (center, radius) in cases {
            let bounds = radius_candidate_bounds(center, radius).unwrap();
            let ranges = radius_hilbert_ranges(center, radius).unwrap();
            assert!(ranges.len() <= MAX_HILBERT_RANGES);
            for lat_step in -18..=18 {
                for lon_step in -36..=36 {
                    let point = p(lon_step as f64 * 5.0, lat_step as f64 * 5.0);
                    if within_radius(center, point, radius).unwrap() {
                        assert!(
                            bounds.contains(point),
                            "radius box missed {point:?} from {center:?}"
                        );
                        assert!(
                            covered(&ranges, point),
                            "radius cover missed {point:?} from {center:?}"
                        );
                    }
                }
            }
            for _ in 0..3_000 {
                seed = seed
                    .wrapping_mul(2862933555777941757)
                    .wrapping_add(3037000493);
                let lon = -180.0 + 360.0 * ((seed >> 11) as f64 / ((1u64 << 53) as f64));
                seed = seed
                    .wrapping_mul(2862933555777941757)
                    .wrapping_add(3037000493);
                let lat = -90.0 + 180.0 * ((seed >> 11) as f64 / ((1u64 << 53) as f64));
                let point = p(lon, lat);
                if within_radius(center, point, radius).unwrap() {
                    assert!(
                        bounds.contains(point),
                        "radius box missed random {point:?} from {center:?}"
                    );
                    assert!(
                        covered(&ranges, point),
                        "radius cover missed random {point:?} from {center:?}"
                    );
                }
            }
        }
    }
}
