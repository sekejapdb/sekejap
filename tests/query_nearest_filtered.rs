//! `QueryOrder::Distance` under a LIMIT with a non-driving equality filter.
//!
//! Source only -- this file is not executed here.
//!
//! The question is which walk drives. A `kind = 'cafe'` equality over eight
//! evenly spread values admits one row in eight; driving from THAT posting
//! hands the ranking 1/8 of the collection in entity order, and a distance
//! rank cannot be read off a scalar posting, so every one of those candidates
//! costs a primary row read. Driving from the spatial index instead walks
//! outward in the order the answer is already in, stops at `k`, and answers
//! the equality from its own posting with no row at all.
//!
//! Three claims, each counted rather than reasoned about:
//!
//!   1. the answer equals the brute-force geodesic sort of the rows the
//!      filter admits -- the plan change must not move a single key;
//!   2. `QueryWork::primary_reads == 0` under `Projection::Ids`;
//!   3. the spatial postings examined stay within `20 * k / acceptance`,
//!      where `acceptance` is the fraction of rows the filter admits (1/8
//!      here), so the bound asserted is `20 * K * KINDS` postings. The factor
//!      of twenty is the ring walk's slack: a Hilbert cover is coarse, a ring
//!      is examined whole before anything leaves it, and the first probe
//!      around the centre cell is paid before any ring at all.
use e4_prototype::{
    collections::{
        CandidateDriver, CollectionOptions, Database, Projection, QueryBudget, QueryDriver,
        QueryFilter, QueryOrder, QueryRequest, QueryWork, ScalarFilter, ScalarValue,
        SortDirection,
    },
    spatial_math::{wgs84_distance_metres, Point},
    Kind,
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

const ROWS: u64 = 4_000;
const K: usize = 10;
const KINDS: u64 = 8;
const CATS: [&str; 8] = [
    "cafe", "bar", "gym", "clinic", "school", "museum", "park", "hotel",
];

fn geojson(lon: f64, lat: f64) -> Value {
    json!({"type":"Point","coordinates":[lon,lat]})
}

/// A deterministic scatter over a degree box, uncorrelated with `kind`: the
/// sequence walks longitude and latitude on two different strides, so the
/// eight kinds are interleaved in space rather than clustered.
fn place(i: u64) -> (f64, f64) {
    let lon = 9.0 + ((i * 37) % 1_000) as f64 / 1_000.0;
    let lat = 45.0 + ((i * 53) % 1_000) as f64 / 1_000.0;
    (lon, lat)
}

struct Fixture {
    db: Database,
    rows: e4_prototype::collections::CollectionId,
    kind: e4_prototype::collections::IndexId,
    loc: e4_prototype::collections::IndexId,
}

fn fixture(dir: &std::path::Path) -> Fixture {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let rows = db
        .create_collection(
            "place",
            vec![("kind".into(), Kind::Text), ("loc".into(), Kind::Point)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    for i in 1..=ROWS {
        let (lon, lat) = place(i);
        db.put(
            rows,
            &format!("k{i:08}"),
            &json!({ "kind": CATS[(i % KINDS) as usize], "loc": geojson(lon, lat) }),
        )
        .unwrap();
        if i % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let kind = db
        .create_scalar_index(rows, "kind_idx", "kind", false)
        .unwrap();
    db.build_index_to_ready(kind, 256).unwrap();
    let loc = db.create_point_index(rows, "loc_idx", "loc").unwrap();
    db.build_index_to_ready(loc, 256).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    Fixture {
        db,
        rows,
        kind,
        loc,
    }
}

/// The answer as a sort over the whole corpus: every row the filter admits,
/// ordered by true geodesic distance, ties broken by sequence the way the
/// engine's rank key breaks them.
fn brute_force(center: Point, cat: &str, k: usize) -> Vec<String> {
    let mut all: Vec<(f64, u64)> = (1..=ROWS)
        .filter(|i| CATS[(i % KINDS) as usize] == cat)
        .map(|i| {
            let (lon, lat) = place(i);
            (
                wgs84_distance_metres(center, Point::new(lon, lat).unwrap()),
                i,
            )
        })
        .collect();
    all.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    all.into_iter()
        .take(k)
        .map(|(_, i)| format!("k{i:08}"))
        .collect()
}

/// Drain one nearest-k page and hand back its keys plus the work it charged.
fn nearest_filtered(
    fixture: &Fixture,
    center: Point,
    cat: &str,
    k: usize,
) -> (Vec<String>, QueryWork, QueryDriver) {
    let filters = [QueryFilter::Scalar {
        index: fixture.kind,
        predicate: ScalarFilter::Eq(ScalarValue::Text(cat.into())),
    }];
    let mut prepared = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.rows,
            filters: &filters,
            order: QueryOrder::Distance {
                index: fixture.loc,
                center,
                direction: SortDirection::Ascending,
            },
            projection: Projection::Ids,
            total_limit: Some(k),
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut keys = Vec::new();
    let mut work = QueryWork::default();
    let mut driver = QueryDriver::Entities;
    loop {
        let page = prepared
            .next_page(k, QueryBudget::unlimited(), || false)
            .unwrap();
        work.candidates += page.work.candidates;
        work.primary_reads += page.work.primary_reads;
        work.scalar_postings += page.work.scalar_postings;
        work.spatial_postings += page.work.spatial_postings;
        driver = page.driver;
        // The put order is the file order, so a row's sequence is the `i` its
        // key was built from.
        for row in &page.rows {
            let i = row.id.sequence;
            keys.push(format!("k{i:08}"));
        }
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    (keys, work, driver)
}

/// Nearest ten under an equality filter equals the brute-force geodesic sort
/// of the rows that filter admits.
#[test]
fn nearest_ten_under_an_equality_equals_the_brute_force_sort() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(dir.path());
    for (n, cat) in CATS.iter().enumerate() {
        let center = Point::new(9.0 + n as f64 / 8.0 * 0.9, 45.4).unwrap();
        let (keys, _, _) = nearest_filtered(&fixture, center, cat, K);
        assert_eq!(
            keys,
            brute_force(center, cat, K),
            "nearest {K} of kind {cat} around {center:?}"
        );
    }
}

/// `Projection::Ids` under this plan reads no primary row: the distance comes
/// off the spatial posting the walk is standing on, and the equality is
/// answered from its own posting.
#[test]
fn nearest_ten_under_an_equality_reads_no_primary_row() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(dir.path());
    for (n, cat) in CATS.iter().enumerate() {
        let center = Point::new(9.0 + n as f64 / 8.0 * 0.9, 45.4).unwrap();
        let (_, work, _) = nearest_filtered(&fixture, center, cat, K);
        assert_eq!(work.primary_reads, 0, "primary reads for kind {cat}");
        // One membership probe per candidate the spatial walk offered, and
        // nothing else touches the scalar index.
        assert!(
            work.scalar_postings <= work.candidates,
            "scalar postings {} exceeded candidates {} for kind {cat}",
            work.scalar_postings,
            work.candidates
        );
    }
}

/// The ring walk's examined postings stay proportional to `k / acceptance`.
///
/// Acceptance here is `1 / KINDS`, so the walk must find its ten inside
/// `20 * K * KINDS` = 1,600 examined postings -- 40% of the collection, and
/// well under the 4,000 a world scan would examine. The plan that drove from
/// the equality posting instead offered `ROWS / KINDS` = 500 candidates and
/// read 500 rows; this bound is the statement that the spatial walk is what
/// stops at ten.
#[test]
fn nearest_ten_under_an_equality_walks_a_bounded_number_of_postings() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(dir.path());
    let bound = (20 * K as u64) * KINDS;
    for (n, cat) in CATS.iter().enumerate() {
        let center = Point::new(9.0 + n as f64 / 8.0 * 0.9, 45.4).unwrap();
        let (keys, work, _) = nearest_filtered(&fixture, center, cat, K);
        assert_eq!(keys.len(), K);
        assert!(
            work.spatial_postings <= bound,
            "spatial postings {} exceeded {bound} for kind {cat}",
            work.spatial_postings
        );
    }
}

/// The driver the planner actually chose, named rather than inferred from a
/// count: a distance order under a limit whose only other filter is an
/// equality is driven by the spatial index.
#[test]
fn a_limited_distance_order_under_an_equality_is_driven_by_the_spatial_index() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(dir.path());
    let (_, _, driver) = nearest_filtered(&fixture, Point::new(9.5, 45.5).unwrap(), "cafe", K);
    assert_eq!(driver, QueryDriver::Nearest { index: fixture.loc });
}

/// Without a limit there is no stop condition, so the walk would cross the
/// whole world paying a membership probe on every posting of it. The equality
/// keeps the drive.
#[test]
fn an_unlimited_distance_order_under_an_equality_keeps_the_equality_driver() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(dir.path());
    let filters = [QueryFilter::Scalar {
        index: fixture.kind,
        predicate: ScalarFilter::Eq(ScalarValue::Text("cafe".into())),
    }];
    let mut prepared = fixture
        .db
        .prepare_query(QueryRequest {
            collection: fixture.rows,
            filters: &filters,
            order: QueryOrder::Distance {
                index: fixture.loc,
                center: Point::new(9.5, 45.5).unwrap(),
                direction: SortDirection::Ascending,
            },
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = prepared
        .next_page(K, QueryBudget::unlimited(), || false)
        .unwrap();
    assert_eq!(page.driver, QueryDriver::Scalar(fixture.kind));
}
