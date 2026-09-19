//! A NON-DRIVING point filter answers from its own index, not from the row.
//!
//! A text-driven query with a second, spatial predicate used to pay one
//! primary read per text candidate: the point index's cover ranges were
//! walked only when the point filter DROVE, so a non-driving one had nothing
//! but the stored row to read the coordinates out of. The cover walk is now
//! done once into the same id set a non-driving scalar range already uses
//! (`MembershipSet`), and the postings carry the coordinates, so the exact
//! radius test needs no row at all.
//!
//! Two claims, both counted rather than reasoned about:
//!
//!   * the rows are the brute-force conjunction, computed here from the
//!     fixture's own generator through the same `within_radius` the engine
//!     calls -- so the set is neither wider nor narrower than the predicate;
//!   * `QueryWork.primary_reads` is 0 for `Projection::Ids`.
//!
//! The fixture deliberately contains rows the two paths could disagree about:
//! an explicit JSON `null` position, an absent position, and rows deleted
//! after every index reached READY.
use e4_prototype::{
    collections::{
        CandidateDriver, CollectionId, CollectionOptions, Database, IndexId, PointFilter,
        Projection, QueryBudget, QueryFilter, QueryOrder, QueryRequest, QueryWork, TextMatch,
    },
    spatial_math::{within_radius, Point},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};

const ROWS: u64 = 8_000;
/// Longitude/latitude grid: 80 columns by 100 rows of half-degree steps,
/// centred on the null island the radius is taken around.
const COLUMNS: u64 = 80;
const STEP_DEGREES: f64 = 0.5;
/// Every seventh row carries the term the text driver asks for. Seven is
/// coprime with the grid's 80 columns, so the term does not land on one
/// column of longitude and make the conjunction all-or-nothing.
const TERM_STRIDE: u64 = 7;
/// Rows removed after every index was built.
const DELETE_STRIDE: u64 = 97;
/// About 3.6 degrees at the equator: 15 of the grid's columns by 15 of its
/// latitude bands, so the radius admits a real slice of the index rather than
/// one cell or the whole world.
const RADIUS_METRES: f64 = 400_000.0;
const PAGE: usize = 128;

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn centre() -> Point {
    Point::new(0.0, 0.0).unwrap()
}

fn lon(i: u64) -> f64 {
    ((i - 1) % COLUMNS) as f64 * STEP_DEGREES - (COLUMNS / 2) as f64 * STEP_DEGREES
}

fn lat(i: u64) -> f64 {
    ((i - 1) / COLUMNS) as f64 * STEP_DEGREES - 25.0
}

/// Whether row `i` carries a position at all. Every 41st is present but
/// explicit JSON `null`; every other 37th is entirely absent. Neither has a
/// posting, and neither can satisfy a radius on the row either.
fn has_position(i: u64) -> bool {
    i % 41 != 0 && i % 37 != 0
}

fn tag(i: u64) -> &'static str {
    if i % TERM_STRIDE == 1 {
        "alpha widgets travel far"
    } else {
        "beta gadgets stay put"
    }
}

fn deleted(i: u64) -> bool {
    i % DELETE_STRIDE == 0
}

fn row_value(i: u64) -> Value {
    let mut obj = serde_json::Map::new();
    if i % 41 == 0 {
        obj.insert("position".to_string(), Value::Null);
    } else if i % 37 != 0 {
        obj.insert(
            "position".to_string(),
            json!({"type": "Point", "coordinates": [lon(i), lat(i)]}),
        );
    }
    obj.insert("tag".to_string(), json!(tag(i)));
    Value::Object(obj)
}

struct Fixture {
    db: Database,
    rows: CollectionId,
    position: IndexId,
    tag: IndexId,
}

fn fixture(dir: &std::path::Path) -> Fixture {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let rows = db
        .create_collection(
            "r",
            vec![
                ("position".into(), Kind::Point),
                ("tag".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    for i in 1..=ROWS {
        db.put(rows, &format!("k{i:08}"), &row_value(i)).unwrap();
        if i % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let position = db
        .create_point_index(rows, "position_idx", "position")
        .unwrap();
    db.build_index_to_ready(position, 256).unwrap();
    let tag = db.create_text_index(rows, "tag_idx", "tag").unwrap();
    db.build_index_to_ready(tag, 256).unwrap();
    db.commit().unwrap();
    for i in (DELETE_STRIDE..=ROWS).step_by(DELETE_STRIDE as usize) {
        db.delete(rows, &format!("k{i:08}")).unwrap();
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    Fixture {
        db,
        rows,
        position,
        tag,
    }
}

fn text_filter(tag: IndexId) -> QueryFilter<'static> {
    QueryFilter::Text {
        index: tag,
        query: "alpha",
        matching: TextMatch::Any,
    }
}

fn radius_filter(position: IndexId) -> QueryFilter<'static> {
    QueryFilter::Point {
        index: position,
        predicate: PointFilter::Radius {
            center: centre(),
            radius_metres: RADIUS_METRES,
        },
    }
}

/// Rows the text driver offers before the radius is asked anything.
fn text_candidates() -> Vec<u64> {
    (1..=ROWS)
        .filter(|&i| i % TERM_STRIDE == 1 && !deleted(i))
        .collect()
}

/// The brute-force conjunction, from the generator, through the same geodesic
/// test the engine uses.
fn oracle() -> Vec<u64> {
    (1..=ROWS)
        .filter(|&i| {
            i % TERM_STRIDE == 1
                && !deleted(i)
                && has_position(i)
                && within_radius(centre(), Point::new(lon(i), lat(i)).unwrap(), RADIUS_METRES)
                    .unwrap()
        })
        .collect()
}

/// A radius wide enough to reach every row. `point_ranges` calls that a
/// world cover, the position is left `Ineligible`, and every candidate takes
/// the row-read path a non-driving point filter had before the cover set
/// existed -- so this is that path, still answering.
const WORLD_RADIUS_METRES: f64 = 20_100_000.0;

fn world_radius_filter(position: IndexId) -> QueryFilter<'static> {
    QueryFilter::Point {
        index: position,
        predicate: PointFilter::Radius {
            center: centre(),
            radius_metres: WORLD_RADIUS_METRES,
        },
    }
}

fn drain(
    db: &Database,
    rows: CollectionId,
    filters: &[QueryFilter<'_>],
    driver: CandidateDriver,
) -> (Vec<u64>, QueryWork) {
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection: rows,
            filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver,
        })
        .unwrap();
    let mut ids = Vec::new();
    let mut work = QueryWork::default();
    loop {
        let page = prepared
            .next_page(PAGE, QueryBudget::unlimited(), || false)
            .unwrap();
        for row in &page.rows {
            ids.push(row.id.sequence);
        }
        work.candidates += page.work.candidates;
        work.primary_reads += page.work.primary_reads;
        work.spatial_postings += page.work.spatial_postings;
        work.text_postings += page.work.text_postings;
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    (ids, work)
}

/// The two claims this file exists for.
#[test]
fn a_text_driven_pages_non_driving_radius_reads_no_row() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());
    let filters = [text_filter(f.tag), radius_filter(f.position)];
    let candidates = text_candidates();
    let expected = oracle();
    assert!(
        (1_000..1_300).contains(&candidates.len()),
        "the fixture should offer about 1,130 text candidates, offered {}",
        candidates.len()
    );
    assert!(
        !expected.is_empty() && expected.len() < candidates.len(),
        "the fixture must have a real, partial answer: {} of {} candidates",
        expected.len(),
        candidates.len()
    );

    let (ids, work) = drain(&f.db, f.rows, &filters, CandidateDriver::Auto);
    assert_eq!(ids, expected);
    assert_eq!(
        work.primary_reads, 0,
        "the text merge certifies its own filter and the radius answers from its cover set"
    );
}

/// The cover-built set answers exactly what the point index answers when it
/// DRIVES: the same cover ranges, the same per-posting geodesic test, only
/// the side of the conjunction that walks them is different.
#[test]
fn the_cover_built_set_agrees_with_the_driving_walk() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());
    let filters = [text_filter(f.tag), radius_filter(f.position)];

    let (text_driven, _) = drain(&f.db, f.rows, &filters, CandidateDriver::Filter(0));
    let (point_driven, _) = drain(&f.db, f.rows, &filters, CandidateDriver::Filter(1));
    assert_eq!(text_driven, oracle());
    assert_eq!(
        text_driven, point_driven,
        "the cover-built set must answer exactly what the driving cover walk answers"
    );
}

/// The row-read path a non-driving point filter keeps for a world-wide
/// cover -- the one shape the set is not built for, because a set that
/// admits every posting in the index buys nothing -- still answers.
#[test]
fn a_world_wide_cover_still_answers_from_the_row() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());
    let filters = [text_filter(f.tag), world_radius_filter(f.position)];
    let expected: Vec<u64> = (1..=ROWS)
        .filter(|&i| {
            i % TERM_STRIDE == 1
                && !deleted(i)
                && has_position(i)
                && within_radius(
                    centre(),
                    Point::new(lon(i), lat(i)).unwrap(),
                    WORLD_RADIUS_METRES,
                )
                .unwrap()
        })
        .collect();
    assert!(!expected.is_empty());

    let (ids, _) = drain(&f.db, f.rows, &filters, CandidateDriver::Auto);
    assert_eq!(ids, expected);
}

/// The point filter as the DRIVER is untouched: it certifies itself from its
/// own posting exactly as it did.
#[test]
fn a_point_driven_page_is_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());
    let filters = [radius_filter(f.position)];
    let expected: Vec<u64> = (1..=ROWS)
        .filter(|&i| {
            !deleted(i)
                && has_position(i)
                && within_radius(centre(), Point::new(lon(i), lat(i)).unwrap(), RADIUS_METRES)
                    .unwrap()
        })
        .collect();
    assert!(!expected.is_empty());

    let (ids, work) = drain(&f.db, f.rows, &filters, CandidateDriver::Auto);
    assert_eq!(ids, expected);
    assert_eq!(work.primary_reads, 0);
}
