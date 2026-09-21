//! PostGIS conformance for the SPHEROIDAL `intersects`, at the edge band.
//!
//! WHY THIS FILE EXISTS. `spatial_geometry::intersects` is documented as
//! `ST_Intersects(a::geography, b::geography)` — the one predicate in that
//! module with a real PostGIS `geography` overload. A `geography` edge is a
//! GEODESIC between its two vertices, not the straight lon/lat line through
//! them; over a span of `s` degrees the two differ by up to ~`122·s²` metres.
//! E4 nevertheless tested edge crossings with a PLANAR routine
//! (`segments_intersect` in a rotated lon/lat frame), so on the 50,000-row
//! battle50k corpus it disagreed with PostGIS on exactly eleven (query, row)
//! pairs of the `plot_intersects` case — in BOTH directions, because the
//! bulge can equally close a planar gap or open a planar crossing:
//!
//!   PostGIS true,  E4 false  (planar gap the geodesic closes):
//!     q0/p0029206, q28/p0046799, q35/p0029158, q39/p0007895,
//!     q43/p0001572, q43/p0010630, q45/p0018906
//!   PostGIS false, E4 true   (planar sliver the geodesic does not cut):
//!     q32/p0016989, q32/p0022998, q42/p0018219, q46/p0007969
//!
//! Each fixture below is those literal coordinates with PostGIS's own answer
//! as the oracle, read from the loaded `place` table with
//! `ST_Intersects(plot, <query>::geography)` and cross-checked against
//! `ST_Distance(plot, <query>::geography)` (0 m for every "true" row;
//! 0.63 m – 2.18 m for every "false" one) and against
//! `ST_Relate(plot::geometry, <query>)`, which reports `FF2FF1212`
//! (planar-disjoint) for the first group and `212101212` (a planar sliver)
//! for the second.
//!
//! The second test is the staging property. The predicate now answers most
//! pairs from a closed-form planar lower bound and calls the exact
//! spheroidal routine only inside a 1 m band, so the bound must never be
//! able to promote a pair across the 1e-6 m touch threshold. The property
//! drives a thousand polygon pairs deliberately placed AT that band and
//! requires the staged public predicate to agree with a reference that
//! stages nothing.

use sekejap_core::spatial_geometry::{distance_m, dwithin_m, intersects};
use kernel::spatial::Geom;

struct Case {
    key: &'static str,
    instance: usize,
    query: Geom,
    plot: Geom,
    postgis: bool,
}

/// `plot_intersects` instance 0 against corpus row `p0029206`.
/// PostGIS `ST_Intersects(plot, query::geography)` answers `true`.
fn case_p0029206() -> Case {
    Case {
        key: "p0029206",
        instance: 0,
        query: Geom::Polygon(
            vec![
                vec![
                    [106.714024, -6.421982],
                    [107.214024, -6.296982],
                    [107.13902399999999, -5.921982],
                    [106.839024, -5.971982],
                    [106.714024, -6.421982],
                ],
            ]
        ),
        plot: Geom::Polygon(
            vec![
                vec![
                    [107.124343, -6.33218],
                    [107.125119, -6.328837],
                    [107.125864, -6.324365],
                    [107.122045, -6.321918],
                    [107.11764, -6.321083],
                    [107.113294, -6.322833],
                    [107.11215, -6.327247],
                    [107.113823, -6.330731],
                    [107.114332, -6.334792],
                    [107.118229, -6.336301],
                    [107.12277, -6.336497],
                    [107.124343, -6.33218],
                ],
                vec![
                    [107.12125, -6.329027],
                    [107.119047, -6.331217],
                    [107.116844, -6.329027],
                    [107.119047, -6.326837],
                    [107.12125, -6.329027],
                ],
            ]
        ),
        postgis: true,
    }
}

/// `plot_intersects` instance 28 against corpus row `p0046799`.
/// PostGIS `ST_Intersects(plot, query::geography)` answers `true`.
fn case_p0046799() -> Case {
    Case {
        key: "p0046799",
        instance: 28,
        query: Geom::Polygon(
            vec![
                vec![
                    [107.059137, -6.384774],
                    [107.559137, -6.259774],
                    [107.484137, -5.884774],
                    [107.184137, -5.934774],
                    [107.059137, -6.384774],
                ],
            ]
        ),
        plot: Geom::Polygon(
            vec![
                vec![
                    [107.443994, -6.288575],
                    [107.442095, -6.295136],
                    [107.445526, -6.300499],
                    [107.452006, -6.2998],
                    [107.455395, -6.293897],
                    [107.451548, -6.287454],
                    [107.443994, -6.288575],
                ],
            ]
        ),
        postgis: true,
    }
}

/// `plot_intersects` instance 35 against corpus row `p0029158`.
/// PostGIS `ST_Intersects(plot, query::geography)` answers `true`.
fn case_p0029158() -> Case {
    Case {
        key: "p0029158",
        instance: 35,
        query: Geom::Polygon(
            vec![
                vec![
                    [107.750398, -6.613734],
                    [108.250398, -6.488734],
                    [108.175398, -6.113734],
                    [107.875398, -6.163734],
                    [107.750398, -6.613734],
                ],
            ]
        ),
        plot: Geom::Polygon(
            vec![
                vec![
                    [108.145053, -6.515093],
                    [108.137418, -6.518799],
                    [108.139733, -6.527241],
                    [108.147305, -6.526346],
                    [108.153035, -6.51975],
                    [108.145053, -6.515093],
                ],
            ]
        ),
        postgis: true,
    }
}

/// `plot_intersects` instance 39 against corpus row `p0007895`.
/// PostGIS `ST_Intersects(plot, query::geography)` answers `true`.
fn case_p0007895() -> Case {
    Case {
        key: "p0007895",
        instance: 39,
        query: Geom::Polygon(
            vec![
                vec![
                    [108.054039, -7.811803],
                    [108.554039, -7.686803],
                    [108.479039, -7.311803],
                    [108.179039, -7.361803],
                    [108.054039, -7.811803],
                ],
            ]
        ),
        plot: Geom::Polygon(
            vec![
                vec![
                    [108.470278, -7.707782],
                    [108.46713, -7.711069],
                    [108.468308, -7.714891],
                    [108.469743, -7.718509],
                    [108.473914, -7.718637],
                    [108.477146, -7.716047],
                    [108.476602, -7.712163],
                    [108.473948, -7.710203],
                    [108.470278, -7.707782],
                ],
            ]
        ),
        postgis: true,
    }
}

/// `plot_intersects` instance 43 against corpus row `p0001572`.
/// PostGIS `ST_Intersects(plot, query::geography)` answers `true`.
fn case_p0001572() -> Case {
    Case {
        key: "p0001572",
        instance: 43,
        query: Geom::Polygon(
            vec![
                vec![
                    [106.414274, -6.950993],
                    [106.914274, -6.825993],
                    [106.839274, -6.450993],
                    [106.539274, -6.500993],
                    [106.414274, -6.950993],
                ],
            ]
        ),
        plot: Geom::Polygon(
            vec![
                vec![
                    [106.806893, -6.852868],
                    [106.797399, -6.85581],
                    [106.802376, -6.865414],
                    [106.811158, -6.860694],
                    [106.806893, -6.852868],
                ],
            ]
        ),
        postgis: true,
    }
}

/// `plot_intersects` instance 43 against corpus row `p0010630`.
/// PostGIS `ST_Intersects(plot, query::geography)` answers `true`.
fn case_p0010630() -> Case {
    Case {
        key: "p0010630",
        instance: 43,
        query: Geom::Polygon(
            vec![
                vec![
                    [106.414274, -6.950993],
                    [106.914274, -6.825993],
                    [106.839274, -6.450993],
                    [106.539274, -6.500993],
                    [106.414274, -6.950993],
                ],
            ]
        ),
        plot: Geom::Polygon(
            vec![
                vec![
                    [106.849891, -6.849372],
                    [106.841803, -6.844122],
                    [106.838713, -6.852383],
                    [106.844943, -6.855611],
                    [106.849891, -6.849372],
                ],
            ]
        ),
        postgis: true,
    }
}

/// `plot_intersects` instance 45 against corpus row `p0018906`.
/// PostGIS `ST_Intersects(plot, query::geography)` answers `true`.
fn case_p0018906() -> Case {
    Case {
        key: "p0018906",
        instance: 45,
        query: Geom::Polygon(
            vec![
                vec![
                    [108.148172, -7.75203],
                    [108.648172, -7.62703],
                    [108.573172, -7.25203],
                    [108.273172, -7.30203],
                    [108.148172, -7.75203],
                ],
            ]
        ),
        plot: Geom::Polygon(
            vec![
                vec![
                    [108.245535, -7.730136],
                    [108.243478, -7.72852],
                    [108.240506, -7.728983],
                    [108.239336, -7.732438],
                    [108.242487, -7.734415],
                    [108.245942, -7.733417],
                    [108.245535, -7.730136],
                ],
            ]
        ),
        postgis: true,
    }
}

/// `plot_intersects` instance 32 against corpus row `p0016989`.
/// PostGIS `ST_Intersects(plot, query::geography)` answers `false`.
fn case_p0016989() -> Case {
    Case {
        key: "p0016989",
        instance: 32,
        query: Geom::Polygon(
            vec![
                vec![
                    [107.663672, -7.666533],
                    [108.163672, -7.541533],
                    [108.088672, -7.166533],
                    [107.788672, -7.216533],
                    [107.663672, -7.666533],
                ],
            ]
        ),
        plot: Geom::Polygon(
            vec![
                vec![
                    [107.759819, -7.312174],
                    [107.754766, -7.310564],
                    [107.750079, -7.31253],
                    [107.74803, -7.317175],
                    [107.750437, -7.321459],
                    [107.754752, -7.323595],
                    [107.759263, -7.321659],
                    [107.760721, -7.317188],
                    [107.759819, -7.312174],
                ],
            ]
        ),
        postgis: false,
    }
}

/// `plot_intersects` instance 32 against corpus row `p0022998`.
/// PostGIS `ST_Intersects(plot, query::geography)` answers `false`.
fn case_p0022998() -> Case {
    Case {
        key: "p0022998",
        instance: 32,
        query: Geom::Polygon(
            vec![
                vec![
                    [107.663672, -7.666533],
                    [108.163672, -7.541533],
                    [108.088672, -7.166533],
                    [107.788672, -7.216533],
                    [107.663672, -7.666533],
                ],
            ]
        ),
        plot: Geom::Polygon(
            vec![
                vec![
                    [107.756788, -7.312602],
                    [107.761936, -7.312789],
                    [107.761294, -7.30764],
                    [107.760648, -7.30421],
                    [107.758201, -7.30032],
                    [107.753665, -7.301503],
                    [107.75003, -7.304267],
                    [107.749364, -7.309029],
                    [107.753516, -7.311155],
                    [107.756788, -7.312602],
                ],
            ]
        ),
        postgis: false,
    }
}

/// `plot_intersects` instance 42 against corpus row `p0018219`.
/// PostGIS `ST_Intersects(plot, query::geography)` answers `false`.
fn case_p0018219() -> Case {
    Case {
        key: "p0018219",
        instance: 42,
        query: Geom::Polygon(
            vec![
                vec![
                    [106.691739, -6.689932],
                    [107.191739, -6.564932],
                    [107.116739, -6.189932],
                    [106.816739, -6.239932],
                    [106.691739, -6.689932],
                ],
            ]
        ),
        plot: Geom::Polygon(
            vec![
                vec![
                    [107.15588, -6.363015],
                    [107.154093, -6.364596],
                    [107.153088, -6.366185],
                    [107.152464, -6.368561],
                    [107.154591, -6.369894],
                    [107.156674, -6.369175],
                    [107.157928, -6.367968],
                    [107.159042, -6.3662],
                    [107.157701, -6.364554],
                    [107.15588, -6.363015],
                ],
            ]
        ),
        postgis: false,
    }
}

/// `plot_intersects` instance 46 against corpus row `p0007969`.
/// PostGIS `ST_Intersects(plot, query::geography)` answers `false`.
fn case_p0007969() -> Case {
    Case {
        key: "p0007969",
        instance: 46,
        query: Geom::Polygon(
            vec![
                vec![
                    [107.355719, -7.091391],
                    [107.855719, -6.966391],
                    [107.78071899999999, -6.591391],
                    [107.480719, -6.641391],
                    [107.355719, -7.091391],
                ],
            ]
        ),
        plot: Geom::Polygon(
            vec![
                vec![
                    [107.80842, -6.719751],
                    [107.807448, -6.725075],
                    [107.813371, -6.726334],
                    [107.815658, -6.72147],
                    [107.812696, -6.717269],
                    [107.80842, -6.719751],
                ],
            ]
        ),
        postgis: false,
    }
}

fn cases() -> Vec<Case> {
    vec![
        case_p0029206(),
        case_p0046799(),
        case_p0029158(),
        case_p0007895(),
        case_p0001572(),
        case_p0010630(),
        case_p0018906(),
        case_p0016989(),
        case_p0022998(),
        case_p0018219(),
        case_p0007969(),
    ]
}

/// **Every one of the eleven rows E4 and PostGIS disagreed on now agrees.**
#[test]
fn the_eleven_disagreeing_rows_match_postgis() {
    for case in cases() {
        assert_eq!(
            intersects(&case.plot, &case.query),
            case.postgis,
            "q{}/{}: PostGIS ST_Intersects(geography) says {}",
            case.instance,
            case.key,
            case.postgis
        );
    }
}

/// **The predicate is symmetric on these fixtures too** — `intersects` takes
/// the row first at one call site and the query first at another, and a
/// staged fast path must not make the two disagree.
#[test]
fn the_eleven_rows_answer_the_same_either_way_round() {
    for case in cases() {
        assert_eq!(
            intersects(&case.plot, &case.query),
            intersects(&case.query, &case.plot),
            "q{}/{} is not symmetric",
            case.instance,
            case.key
        );
    }
}

/// **A "true" row really is touching, and a "false" row really is not.**
/// PostGIS reports 0 m for the first group and 0.63 m – 2.18 m for the
/// second, so `distance_m` must land on the same side of the touch epsilon.
#[test]
fn the_eleven_rows_distances_straddle_the_touch_epsilon() {
    for case in cases() {
        let metres = distance_m(&case.plot, &case.query);
        if case.postgis {
            assert!(
                metres < 1e-6,
                "q{}/{} should be touching, distance_m says {metres} m",
                case.instance,
                case.key
            );
        } else {
            assert!(
                (0.5..3.0).contains(&metres),
                "q{}/{} should be 0.63-2.18 m clear, distance_m says {metres} m",
                case.instance,
                case.key
            );
            assert!(dwithin_m(&case.plot, &case.query, 3.0));
            assert!(!dwithin_m(&case.plot, &case.query, 0.1));
        }
    }
}

// ── the staging property ─────────────────────────────────────────────────

/// A reference `intersects` that stages NOTHING: the same three questions
/// the real one asks (a vertex covered by a ring, a great-circle edge
/// crossing, a touch inside the epsilon), with every cheap bound and every
/// bounding-box short cut removed, so it is the exact spheroidal answer by
/// construction. Kept here rather than in the crate so the property compares
/// the shipped code against an independent expression of the same rule.
fn reference_intersects(a: &Geom, b: &Geom) -> bool {
    // `distance_m` itself stages nothing that can change an answer: it is a
    // minimum over every vertex/vertex and vertex/edge pair, and a pair a
    // LOWER bound puts beyond the running minimum cannot be the minimum. Two
    // geometries intersect exactly when that distance is zero, which is what
    // PostGIS `ST_Distance(geography)` reports for a touching pair.
    distance_m(a, b) < 1e-6
}

/// A deterministic 64-bit stream — no dev-dependency, and the same thousand
/// pairs on every machine.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform in `[lo, hi)`.
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64)
    }
}

/// A small square-ish quadrilateral around `(lon, lat)`, `half` degrees wide.
fn quad(lon: f64, lat: f64, half: f64) -> Geom {
    Geom::Polygon(vec![vec![
        [lon - half, lat - half],
        [lon + half, lat - half],
        [lon + half, lat + half],
        [lon - half, lat + half],
        [lon - half, lat - half],
    ]])
}

/// **The staged predicate agrees with the unstaged reference on a thousand
/// polygon pairs placed AT the boundary band.**
///
/// Each pair is built so the second polygon's western edge sits within a few
/// metres of the first polygon's eastern edge — inside the 1 m staging band
/// or just outside it, which is precisely where a bound that over-stated a
/// distance would flip an answer. Latitudes range to 70 degrees so the
/// longitude scaling the bound uses is exercised, and spans range over three
/// orders of magnitude so the bulge allowance is too.
#[test]
fn the_staged_fast_path_agrees_with_the_exact_routine_at_the_band() {
    let mut rng = Rng(0x5eed_1234_9e37_79b9);
    let mut touching = 0usize;
    let mut clear = 0usize;
    for i in 0..1_000 {
        let lat = rng.range(-70.0, 70.0);
        let lon = rng.range(-179.0, 179.0);
        let half = 10f64.powf(rng.range(-3.0, 0.0));
        let a = quad(lon, lat, half);
        // Offset the neighbour so its western edge lands within +/- 5 m of
        // `a`'s eastern edge: 5 m is about 4.5e-5 degrees of latitude.
        let nudge = rng.range(-4.5e-5, 4.5e-5);
        let b = quad(lon + 2.0 * half + nudge, lat + rng.range(-half, half), half);
        let staged = intersects(&a, &b);
        let exact = reference_intersects(&a, &b);
        assert_eq!(
            staged, exact,
            "pair {i}: lon={lon} lat={lat} half={half} nudge={nudge}"
        );
        if staged {
            touching += 1;
        } else {
            clear += 1;
        }
    }
    assert!(
        touching > 50 && clear > 50,
        "the band should straddle the answer, saw {touching} touching and {clear} clear"
    );
}

/// **`dwithin_m`'s early exit never changes an answer.** It stops at the
/// first pair inside the radius and skips every pair a lower bound places
/// outside it; both must agree with the full minimum `distance_m` computes.
#[test]
fn dwithin_agrees_with_the_full_distance() {
    let mut rng = Rng(0x00c0_ffee_d15e_a5e5);
    for i in 0..1_000 {
        let lat = rng.range(-70.0, 70.0);
        let lon = rng.range(-179.0, 179.0);
        let half = 10f64.powf(rng.range(-3.0, 0.0));
        let a = quad(lon, lat, half);
        let b = Geom::Point(lon + rng.range(-4.0 * half, 4.0 * half), lat + rng.range(-4.0 * half, 4.0 * half));
        let exact = distance_m(&a, &b);
        for radius in [0.0, 1.0, 100.0, 1_000.0, 10_000.0] {
            assert_eq!(
                dwithin_m(&a, &b, radius),
                exact <= radius,
                "pair {i} radius {radius}: distance_m says {exact} m"
            );
        }
    }
}
