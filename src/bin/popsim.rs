//! POPSIM — population-scale simulation, one arm per process.
//!
//! `N` synthetic person records over a rectangular region, streamed in, then
//! three indexes built LATE, then a battery of realistic queries, each timed.
//!
//!     record: _key, fullname (ranked full text), born yyyymmdd (ordered
//!             scalar), born_year (stored, unindexed), addr point (spatial)
//!
//! This is e3's `bench/popsim.rs` + `bench/popsim_sqlite.rs` rebuilt as ONE
//! program with two arms, so the 48M-row comparison the previous engine ran
//! can be rerun here. Each arm is a separate process, because a 48M run is two
//! cluster jobs, not one.
//!
//!     popsim e4|sqlite|postgres <rows> <fresh-dir> [--batch N] [--only substr]
//!                                        [--reps N] [--case-budget SECS]
//!                                        [--cache-bytes N] [--dsn URL]
//!
//! The arm writes its database under `<fresh-dir>/<arm>` and its results to
//! `<fresh-dir>/popsim-<arm>.json`. `.insert-loop/p2final/popsim_compare.py`
//! reads the JSONs and prints the comparison (two or three arms).
//!
//! THE THIRD ARM: POSTGRES/POSTGIS
//!
//!   `postgres` targets a real server (default DSN
//!   `postgres://127.0.0.1:55432/postgres`, override with
//!   `--dsn`). `<fresh-dir>` holds only the JSON report; the data lives in a
//!   FRESH per-run database (`popsim_<rows>_<unix-secs>`), created off the
//!   `--dsn` connection so repeated runs never see a stale index or a leftover
//!   row from a previous size. Schema, index set and load cadence mirror the
//!   other two arms as closely as Postgres's own idiom allows:
//!
//!     * `person(k text primary key, fullname text, born bigint,
//!        born_year int, addr geography(Point,4326))`.
//!     * Indexes built LATE, same as the other two arms: a GIN index on
//!       `to_tsvector('simple', fullname)` for text, a btree on `born`, a
//!       GiST index on `addr` for spatial.
//!     * Text uses the `simple` search configuration, not `english` —
//!       `simple` does no stemming, which is the closer match to E4's
//!       tokenizer (case-fold, alphanumeric-run tokens, no stemming). `Any`
//!       (OR) queries become `to_tsquery('simple', 'term1 | term2')`; the
//!       ranked case orders by `ts_rank_cd` (a different formula from E4's
//!       BM25, so, like the other two arms, it is compared on row count only,
//!       never on result order).
//!     * Radius uses `ST_DWithin(addr, point::geography, metres, true)` —
//!       `use_spheroid => true` explicitly, so Postgres measures the same
//!       ellipsoidal (not spherical) distance model E4's Karney routine does.
//!       Boundary rows can still disagree in the last metre; see the report.
//!     * Bbox uses the GiST `&&` overlap operator as the index-accelerated
//!       candidate filter, refined by an exact `ST_X`/`ST_Y` closed-interval
//!       check against the same doubles the other two arms compare against —
//!       the same candidate-then-refine shape SQLite's R*Tree case uses.
//!     * Load batches 256 rows into one multi-row `INSERT ... VALUES (...),
//!       (...), ...` per transaction — the idiomatic shape for this cadence
//!       from a real Postgres client, not 256 single-row round trips.
//!       `synchronous_commit` and `fsync` are Postgres's defaults (both ON);
//!       `synchronous_commit` is also set explicitly so the load pays the
//!       same per-commit durability barrier the other two arms do.
//!
//! WHAT IS THE SAME AS e3
//!
//!   * The population. Same xorshift `rng`, same 16 syllables, same two-word
//!     name rule, same `1940 + r % 86` birth year, same month/day, same
//!     106.4..108.8 x -7.8..-5.9 bounding box, same `r % 100_000 / 100_000`
//!     placement, same `_key` ordinal. Row `i` here holds the same person row
//!     `i` held there.
//!   * The query shapes: point lookup, count, one-term and two-term full text,
//!     ranked top ten, date ranges, radius, radius AND date, text AND date,
//!     oldest ten.
//!
//! WHAT DEVIATES FROM e3, AND WHY
//!
//!   1. KEYS ARE ZERO PADDED. e3 wrote `person/p{i}`; this writes `p{i:09}`.
//!      The collection is a real collection here so the prefix is redundant,
//!      and the padding makes SQLite's lexicographic `_key` order and E4's
//!      entity-sequence order the SAME order — without it every ordered case
//!      would read as a disagreement about ties. (`two_ways` sets the same
//!      rule for the same reason.)
//!   2. COORDINATES ARE ROUNDED NUMERICALLY, not through `{:.5}` text. Both
//!      arms take the identical `f64` from the identical expression, which is
//!      what a radius boundary comparison needs; against e3 a coordinate may
//!      differ in the last bit.
//!   3. NO `born_year_u` TWIN. e3 carried an unindexed copy of `born_year` to
//!      contrast the indexed aggregate path with the unindexed grouped scan.
//!      E4 has no aggregate API at all, so neither case can run; `born_year`
//!      is kept as a stored, unindexed column and the twin is dropped.
//!   4. THE INDEX SET IS THREE, MATCHED. Text on `fullname`, ordered scalar on
//!      `born`, point on `addr`; SQLite gets FTS5, `CREATE INDEX`, and an
//!      R*Tree. e3 additionally indexed `born_year` on both sides.
//!   5. COUNTS ARE ENUMERATED, NOT AGGREGATED. e3 asked `SELECT COUNT(*)`
//!      through sekejap's SQL front end. E4 has no aggregate API, and counting
//!      rows in the harness would measure a full materialisation rather than
//!      an aggregate — so BOTH arms enumerate the matching rows and the
//!      reported `rows` is how many were enumerated. SQLite is given the
//!      enumerating spelling (`SELECT _key FROM …`), never `COUNT(*)`, so
//!      neither arm gets a shortcut the other cannot have. The cases e4 cannot
//!      express at all (GROUP BY, AVG, MIN/MAX, OFFSET) are listed in the
//!      JSON's `unsupported` block with the reason, never silently dropped and
//!      never emulated to produce a number.
//!
//!      As of item KD, `count_all` also asks E4 the SAME question SQLite's
//!      `SELECT _key FROM person` already asks: an enumeration of the
//!      `(_key, rowid)` covering index, not the row table. E4's counterpart is
//!      `CandidateDriver::Keys` over the external-key mapping keyspace
//!      (`collections.rs:382`), asked in `QueryOrder::Driver` (key order,
//!      ascending — the mapping keyspace has no descending walk, see item
//!      KD's report). Both arms now enumerate their key index in key order;
//!      `key(sequence)`'s zero-padding happens to make that the same order
//!      `EntityId` gave before, so this is an accounting change, not a
//!      behaviour change, for THIS fixture's row count or contents.
//!   6. DURABILITY IS MATCHED AND ON. e3's SQLite arm ran `journal_mode=OFF`,
//!      `synchronous=OFF`, one transaction. Here both arms commit every 256
//!      rows and both pay a FULL barrier per commit — E4's page-WAL publishes
//!      with `fcntl(F_FULLFSYNC)` and refuses any other mode, so SQLite is run
//!      WAL + `synchronous=FULL` + `fullfsync=ON`. Both caches are 8 MiB.
//!   7. SQLITE IS GIVEN A GEODESIC DISTANCE FUNCTION. E4's radius is an exact
//!      WGS84 geodesic; SQLite has no such function, and a haversine refine
//!      would disagree with E4 about every point near the boundary. `geodist`
//!      is registered from the SAME routine E4 uses, so the two arms accept
//!      exactly the same points. The R*Tree supplies the candidate box (from
//!      E4's own `radius_candidate_bounds`, as constants) and the refine call
//!      is SQLite's per-candidate cost, inside the timed statement.
//!   8. THE DATE-RANGE CASES ARE ORDERED BY THE DRIVING DATE INDEX, NOT BY
//!      KEY. `SELECT _key FROM person WHERE born >= ? AND born < ?` returns
//!      SQLite's index order -- `(born, rowid)` -- and sorts nothing. Asking
//!      E4 for the same rows in ENTITY order was asking a different question:
//!      the driving index is walked by value, so the answer has to be
//!      re-ranked by id, and a paged answer that cannot resume re-walks the
//!      whole posting range per page. `born_decade`, `born_between`,
//!      `born_ge_open`, `born_one_year` and `born_one_day` therefore order by
//!      `born` ascending, which is the order SQLite's plan produces and the
//!      order the index is already in. The comparison is unaffected: these
//!      cases are compared on ROW COUNT, never on key order (the compare
//!      script's `ORDERED` set is the point lookup, the full scan and the two
//!      ordered limits, and none of these is in it).
//!
//!      The text cases keep `EntityId`: the term merge ascends by document,
//!      so entity order IS the order the driving structure produces, and
//!      SQLite's FTS5 join returns rowid order, which is the same thing.
//!
//!   9. THE SPATIAL CASES ARE ASKED IN THE DRIVING INDEX'S OWN ORDER.
//!      `SELECT p._key FROM person p JOIN addr_rt ... WHERE geodist(...)<=?`
//!      returns SQLite's R*Tree join order and sorts nothing. E4 walks its
//!      point index in HILBERT CELL order, which is neither entity order nor
//!      any value order -- so asking for the same rows in `EntityId` was
//!      asking a question with a sort in it that SQLite was never asked to
//!      pay, and, worse, one that no page could resume: the walk had to see
//!      every posting in the envelope before it knew its first row, and the
//!      next page had to see them all again. At 48M rows `radius_50km`
//!      returned 6,762,672 rows in 1,648 s -- 244 us/row, 46 passes over the
//!      cells with a geodesic refine on each.
//!
//!      `radius_2km`, `radius_50km`, `bbox` and `radius_and_born` therefore
//!      ask for `QueryOrder::Driver`: the rows in the order the cell walk
//!      produces them. NEITHER arm returns an order the other could match --
//!      SQLite's is the R*Tree's, E4's is the Hilbert curve's, and both are
//!      unsorted -- and these cases are compared on ROW COUNT, never on key
//!      order (they are not in the compare script's `ORDERED` set).
//!
//! MEMORY. Generation is streaming: one row exists at a time, and no case
//! accumulates its answer — rows are counted as they arrive and only the first
//! sixteen keys are kept, so a 48M scan costs a page, not a result set.

use e4_prototype::{
    collections::{
        CandidateDriver, CollectionId, CollectionOptions, Database, IndexId, PointFilter,
        Projection, QueryBudget, QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue,
        SortDirection, SpatialCandidates, TextMatch,
    },
    spatial_math::{radius_candidate_bounds, wgs84_distance_metres, Bounds, Point},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use postgres::{
    types::{ToSql, Type},
    Client, NoTls,
};
use rusqlite::{functions::FunctionFlags, Connection};
use serde_json::{json, Value};
use std::{
    fs,
    ops::Bound,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type R<T> = Result<T, Box<dyn std::error::Error>>;

// ── the population ────────────────────────────────────────────────────────

const SYL: [&str; 16] = [
    "sa", "ri", "bu", "di", "an", "wa", "ti", "ja", "ka", "ma", "la", "ni", "pra", "yu", "dew",
    "har",
];
const BBOX_LON: (f64, f64) = (106.4, 108.8);
const BBOX_LAT: (f64, f64) = (-7.8, -5.9);

/// e3's xorshift, unchanged.
fn rng(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// e3's name rule, unchanged: two words, each two or three syllables, the
/// first syllable of each capitalised.
fn name(i: u64) -> String {
    let mut r = rng(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut out = String::new();
    for w in 0..2 {
        if w > 0 {
            out.push(' ');
        }
        let syls = 2 + (r % 2) as usize;
        r = rng(r);
        for s in 0..syls {
            let syl = SYL[(r % 16) as usize];
            r = rng(r);
            if s == 0 {
                let mut c = syl.chars();
                out.extend(c.next().expect("syllable is non-empty").to_uppercase());
                out.push_str(c.as_str());
            } else {
                out.push_str(syl);
            }
        }
    }
    out
}

/// Five decimal places, taken numerically so both arms hold the identical
/// `f64`. e3 rounded through `{:.5}` text; see deviation 2.
fn round5(x: f64) -> f64 {
    (x * 100_000.0).round() / 100_000.0
}

fn key(i: u64) -> String {
    format!("p{i:09}")
}

/// One person. Generated on demand; never collected.
struct Person {
    key: String,
    fullname: String,
    born: i64,
    born_year: i64,
    lon: f64,
    lat: f64,
}

fn person(i: u64) -> Person {
    let mut r = rng(i.wrapping_mul(0xA24B_AED4_963E_E407) | 1);
    let year = 1940 + (r % 86) as i64;
    r = rng(r);
    let month = 1 + (r % 12) as i64;
    r = rng(r);
    let day = 1 + (r % 28) as i64;
    r = rng(r);
    let lon = BBOX_LON.0 + (r % 100_000) as f64 / 100_000.0 * (BBOX_LON.1 - BBOX_LON.0);
    r = rng(r);
    let lat = BBOX_LAT.0 + (r % 100_000) as f64 / 100_000.0 * (BBOX_LAT.1 - BBOX_LAT.0);
    Person {
        key: key(i),
        fullname: name(i),
        born: year * 10_000 + month * 100 + day,
        born_year: year,
        lon: round5(lon),
        lat: round5(lat),
    }
}

// ── the battery's constants ───────────────────────────────────────────────

/// e3's `POINT(107.6 -6.9)` — the centre every radius case is measured from.
const CENTER_LON: f64 = 107.6;
const CENTER_LAT: f64 = -6.9;
/// A box roughly 22 km on a side around that centre. e3 had no bbox case;
/// F12 says to check spatial separately from the radius refine, and a bbox is
/// the shape where both engines' index answers without a distance call.
const BOX_WEST: f64 = 107.5;
const BOX_EAST: f64 = 107.7;
const BOX_SOUTH: f64 = -7.0;
const BOX_NORTH: f64 = -6.8;

/// Default rows per commit, both arms — the Phase 2 load batch.
const BATCH: u64 = 256;
/// Default cache budget, both arms. E4 refuses less; SQLite is given the same.
const CACHE_BYTES: usize = 8 << 20;
/// E4's maximum page. A complete answer is assembled from repeated pages and
/// that assembly is part of what a case costs.
const PAGE: usize = 8192;
/// Keys kept per answer, for the cross-arm spot check. Everything beyond this
/// is counted and dropped, so a 48M scan does not become a 48M vector.
const KEY_SAMPLE: usize = 16;
/// Progress line cadence on stderr, so a cluster log shows movement.
const PROGRESS: u64 = 1_000_000;

fn center() -> Point {
    Point::new(CENTER_LON, CENTER_LAT).expect("benchmark centre is a valid point")
}

fn query_box() -> Bounds {
    Bounds::new(BOX_WEST, BOX_EAST, BOX_SOUTH, BOX_NORTH).expect("benchmark box is valid")
}

// ── what a case answers with ──────────────────────────────────────────────

/// The number of rows the case produced, plus the first few keys in result
/// order so the two arms can be spot checked beyond their counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Answer {
    pub rows: usize,
    pub keys: Vec<String>,
}

impl Answer {
    fn push(&mut self, key: String) {
        if self.keys.len() < KEY_SAMPLE {
            self.keys.push(key);
        }
        self.rows += 1;
    }
}

/// A case E4's API cannot express, with the reason. Listed, never emulated.
const UNSUPPORTED: [(&str, &str); 6] = [
    (
        "min_max_born",
        "no aggregate API: Projection has no MIN/MAX and QueryRequest has no grouping; \
         `oldest_10` covers the ordered-index seek this would have measured",
    ),
    (
        "group_by_year",
        "no aggregate API: QueryRequest has no GROUP BY",
    ),
    (
        "avg_birth_year",
        "no aggregate API: Projection has no AVG",
    ),
    (
        "group_by_year_scan",
        "no aggregate API, and e3's unindexed `born_year_u` twin exists only to \
         contrast two aggregate paths E4 does not have",
    ),
    (
        "avg_year_scan",
        "no aggregate API, and e3's unindexed `born_year_u` twin is not carried",
    ),
    (
        "page_deep",
        "QueryRequest has no offset/skip; taking limit+offset rows and discarding \
         them in the harness would not be the engine's pagination",
    ),
];

// ── the E4 arm ────────────────────────────────────────────────────────────

pub struct E4Ctx {
    db: Database,
    person: CollectionId,
    fullname: IndexId,
    born: IndexId,
    addr: IndexId,
    rows: u64,
}

fn e4_run(
    c: &E4Ctx,
    filters: &[QueryFilter<'_>],
    order: QueryOrder<'_>,
    projection: Projection<'_>,
    total_limit: Option<usize>,
    driver: CandidateDriver,
) -> R<Answer> {
    let mut prepared = c.db.prepare_query(QueryRequest {
        collection: c.person,
        filters,
        order,
        projection,
        total_limit,
        driver,
    })?;
    let mut answer = Answer::default();
    loop {
        let page = prepared.next_page(PAGE, QueryBudget::unlimited(), || false)?;
        for row in &page.rows {
            for (_, value) in &row.projected {
                std::hint::black_box(value);
            }
            answer.push(key(row.id.sequence - 1));
        }
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    Ok(answer)
}

fn e4_ids(
    c: &E4Ctx,
    filters: &[QueryFilter<'_>],
    order: QueryOrder<'_>,
    limit: Option<usize>,
) -> R<Answer> {
    e4_run(c, filters, order, Projection::Ids, limit, CandidateDriver::Auto)
}

fn born_range(index: IndexId, lower: Bound<i64>, upper: Bound<i64>) -> QueryFilter<'static> {
    let map = |bound: Bound<i64>| match bound {
        Bound::Included(v) => Bound::Included(ScalarValue::I64(v)),
        Bound::Excluded(v) => Bound::Excluded(ScalarValue::I64(v)),
        Bound::Unbounded => Bound::Unbounded,
    };
    QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Range {
            lower: map(lower),
            upper: map(upper),
        },
    }
}

/// The order SQLite's plan for a `born` range produces: the index's own,
/// `(born, rowid)` ascending, with no sort step. See deviation 8.
fn born_order(index: IndexId) -> QueryOrder<'static> {
    QueryOrder::Scalar {
        index,
        direction: SortDirection::Ascending,
    }
}

fn text(index: IndexId, query: &str, matching: TextMatch) -> QueryFilter<'_> {
    QueryFilter::Text {
        index,
        query,
        matching,
    }
}

fn radius(index: IndexId, metres: f64) -> QueryFilter<'static> {
    radius_at(index, center(), metres)
}

fn radius_at(index: IndexId, center: Point, metres: f64) -> QueryFilter<'static> {
    QueryFilter::Point {
        index,
        predicate: PointFilter::Radius {
            center,
            radius_metres: metres,
        },
    }
}

fn bbox(index: IndexId, bounds: Bounds) -> QueryFilter<'static> {
    QueryFilter::Point {
        index,
        predicate: PointFilter::Bbox(bounds),
    }
}

// ── the spatial battery's extra geometry ─────────────────────────────────
//
// The population is uniform over BBOX_LON x BBOX_LAT (about 265 x 210 km), so a
// radius at the centre sees the full density, a radius at the south-west
// corner sees a quarter circle at the data's edge, and a radius 100 km west of
// the box sees nothing at all -- the empty answer is where index pruning is
// the whole cost.
const CORNER_LON: f64 = 106.45;
const CORNER_LAT: f64 = -7.75;
const FAR_LON: f64 = 105.0;
const FAR_LAT: f64 = -6.9;

fn corner() -> Point {
    Point::new(CORNER_LON, CORNER_LAT).expect("corner is a valid point")
}

fn far_away() -> Point {
    Point::new(FAR_LON, FAR_LAT).expect("far point is valid")
}

/// About 2 km on a side around the centre: a box smaller than one cover cell
/// at most levels, so the answer is a handful of postings.
fn small_box() -> Bounds {
    Bounds::new(CENTER_LON - 0.009, CENTER_LON + 0.009, CENTER_LAT - 0.009, CENTER_LAT + 0.009)
        .expect("small box is valid")
}

/// A strip 0.5 degrees of longitude wide (about 55 km) and 0.01 degrees of
/// latitude tall (about 1.1 km): the shape a square-cell cover handles worst,
/// because the box is long and thin and every cell along it is mostly outside.
fn strip_box() -> Bounds {
    Bounds::new(CENTER_LON - 0.25, CENTER_LON + 0.25, CENTER_LAT - 0.005, CENTER_LAT + 0.005)
        .expect("strip box is valid")
}

/// E4's k nearest: an exhaustive pass over the point index's postings (every
/// posting carries its coordinates) keeping the k closest. There is no
/// ordered walk from the centre outward yet, so this costs a full index scan
/// however small k is -- the case exists to keep that cost visible next to
/// PostGIS's KNN-GiST walk.
fn e4_knn(c: &E4Ctx, k: usize) -> R<Answer> {
    let hits = c.db.query_point_nearest(
        c.addr,
        center(),
        k,
        SpatialCandidates::All,
        usize::MAX,
        || false,
    )?;
    let mut answer = Answer::default();
    for hit in hits {
        answer.push(key(hit.id.sequence - 1));
    }
    Ok(answer)
}

/// e3's battery, in e3's order, as this engine's API spells it.
fn e4_cases() -> Vec<(&'static str, fn(&E4Ctx) -> R<Answer>)> {
    vec![
        ("point_lookup", |c| {
            let mut answer = Answer::default();
            if let Some(entity) = c.db.get(c.person, &key(c.rows / 2))? {
                answer.push(key(entity.id.sequence - 1));
            }
            Ok(answer)
        }),
        ("count_all", |c| {
            e4_run(
                c,
                &[],
                QueryOrder::Driver,
                Projection::Ids,
                None,
                CandidateDriver::Keys,
            )
        }),
        ("name_fulltext", |c| {
            e4_ids(
                c,
                &[text(c.fullname, "sari", TextMatch::Any)],
                QueryOrder::EntityId,
                None,
            )
        }),
        ("name_two_terms", |c| {
            e4_ids(
                c,
                &[text(c.fullname, "sari wati", TextMatch::Any)],
                QueryOrder::EntityId,
                None,
            )
        }),
        ("name_top10", |c| {
            e4_run(
                c,
                &[text(c.fullname, "diwa", TextMatch::Any)],
                QueryOrder::Bm25 {
                    index: c.fullname,
                    query: "diwa",
                    matching: TextMatch::Any,
                },
                Projection::Ids,
                Some(10),
                CandidateDriver::Auto,
            )
        }),
        // F11's three spellings of one date range, at a size where a scan is
        // distinguishable from a seek.
        ("born_decade", |c| {
            e4_ids(
                c,
                &[born_range(
                    c.born,
                    Bound::Included(19_900_101),
                    Bound::Excluded(20_000_101),
                )],
                born_order(c.born),
                None,
            )
        }),
        ("born_between", |c| {
            e4_ids(
                c,
                &[born_range(
                    c.born,
                    Bound::Included(19_900_101),
                    Bound::Included(19_991_231),
                )],
                born_order(c.born),
                None,
            )
        }),
        ("born_ge_open", |c| {
            e4_ids(
                c,
                &[born_range(c.born, Bound::Included(20_100_101), Bound::Unbounded)],
                born_order(c.born),
                None,
            )
        }),
        ("born_one_year", |c| {
            e4_ids(
                c,
                &[born_range(
                    c.born,
                    Bound::Included(19_870_101),
                    Bound::Excluded(19_880_101),
                )],
                born_order(c.born),
                None,
            )
        }),
        // One day. If time tracks rows SCANNED rather than rows matched, this
        // costs the same as the decade — that is what F11 caught at 48M.
        ("born_one_day", |c| {
            e4_ids(
                c,
                &[born_range(
                    c.born,
                    Bound::Included(19_870_615),
                    Bound::Excluded(19_870_616),
                )],
                born_order(c.born),
                None,
            )
        }),
        ("radius_2km", |c| {
            e4_ids(c, &[radius(c.addr, 2_000.0)], QueryOrder::Driver, None)
        }),
        ("radius_50km", |c| {
            e4_ids(c, &[radius(c.addr, 50_000.0)], QueryOrder::Driver, None)
        }),
        ("bbox", |c| {
            e4_ids(
                c,
                &[QueryFilter::Point {
                    index: c.addr,
                    predicate: PointFilter::Bbox(query_box()),
                }],
                QueryOrder::Driver,
                None,
            )
        }),
        ("radius_500m", |c| {
            e4_ids(c, &[radius(c.addr, 500.0)], QueryOrder::Driver, None)
        }),
        ("radius_10km", |c| {
            e4_ids(c, &[radius(c.addr, 10_000.0)], QueryOrder::Driver, None)
        }),
        ("radius_corner_10km", |c| {
            e4_ids(c, &[radius_at(c.addr, corner(), 10_000.0)], QueryOrder::Driver, None)
        }),
        ("radius_far_empty", |c| {
            e4_ids(c, &[radius_at(c.addr, far_away(), 20_000.0)], QueryOrder::Driver, None)
        }),
        ("bbox_2km", |c| {
            e4_ids(c, &[bbox(c.addr, small_box())], QueryOrder::Driver, None)
        }),
        ("bbox_strip", |c| {
            e4_ids(c, &[bbox(c.addr, strip_box())], QueryOrder::Driver, None)
        }),
        ("bbox_and_born", |c| {
            e4_ids(
                c,
                &[
                    bbox(c.addr, query_box()),
                    born_range(
                        c.born,
                        Bound::Included(19_900_101),
                        Bound::Excluded(20_000_101),
                    ),
                ],
                QueryOrder::Driver,
                None,
            )
        }),
        ("radius_and_name", |c| {
            e4_ids(
                c,
                &[radius(c.addr, 10_000.0), text(c.fullname, "sari", TextMatch::Any)],
                QueryOrder::Driver,
                None,
            )
        }),
        ("radius_top10", |c| {
            e4_ids(c, &[radius(c.addr, 10_000.0)], QueryOrder::Driver, Some(10))
        }),
        ("knn_10", |c| e4_knn(c, 10)),
        ("knn_100", |c| e4_knn(c, 100)),
        ("radius_and_born", |c| {
            e4_ids(
                c,
                &[
                    radius(c.addr, 10_000.0),
                    born_range(
                        c.born,
                        Bound::Included(19_900_101),
                        Bound::Excluded(20_000_101),
                    ),
                ],
                // The spatial filter drives this one too, so its order is the
                // cell walk's. See deviation 9.
                QueryOrder::Driver,
                None,
            )
        }),
        ("name_and_born", |c| {
            e4_ids(
                c,
                &[
                    text(c.fullname, "sari", TextMatch::Any),
                    born_range(
                        c.born,
                        Bound::Included(19_800_101),
                        Bound::Excluded(19_900_101),
                    ),
                ],
                QueryOrder::EntityId,
                None,
            )
        }),
        ("oldest_10", |c| {
            e4_ids(
                c,
                &[],
                QueryOrder::Scalar {
                    index: c.born,
                    direction: SortDirection::Ascending,
                },
                Some(10),
            )
        }),
        ("youngest_10", |c| {
            e4_ids(
                c,
                &[],
                QueryOrder::Scalar {
                    index: c.born,
                    direction: SortDirection::Descending,
                },
                Some(10),
            )
        }),
    ]
}

fn dir_bytes(root: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_file() => meta.len(),
            Ok(meta) if meta.is_dir() => dir_bytes(&entry.path()),
            _ => 0,
        })
        .sum()
}

fn file_map(root: &Path) -> Value {
    let mut map = serde_json::Map::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(meta) if meta.is_file() => {
                    map.insert(entry.file_name().to_string_lossy().into(), json!(meta.len()));
                }
                Ok(meta) if meta.is_dir() => {
                    map.insert(
                        format!("{}/", entry.file_name().to_string_lossy()),
                        json!(dir_bytes(&entry.path())),
                    );
                }
                _ => {}
            }
        }
    }
    Value::Object(map)
}

fn load_e4(root: &Path, rows: u64, batch: u64, cache_bytes: usize) -> R<(E4Ctx, Value)> {
    fs::create_dir_all(root.parent().unwrap_or(root))?;
    let overall = Instant::now();

    let at = Instant::now();
    let mut db = Database::create(
        root,
        Config {
            budget_bytes: cache_bytes,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )?;
    let person_c = db.create_collection(
        "person",
        vec![
            ("fullname".into(), Kind::Text),
            ("born".into(), Kind::Int),
            ("born_year".into(), Kind::Int),
            ("addr".into(), Kind::Point),
        ],
        CollectionOptions::default(),
    )?;
    db.commit()?;
    let open_s = at.elapsed().as_secs_f64();

    eprintln!("[e4] inserting {rows} rows, commit every {batch} …");
    let at = Instant::now();
    for i in 0..rows {
        let p = person(i);
        let id = db.put(
            person_c,
            &p.key,
            &json!({
                "fullname": p.fullname,
                "born": p.born,
                "born_year": p.born_year,
                "addr": {"type": "Point", "coordinates": [p.lon, p.lat]},
            }),
        )?;
        // The whole comparison rests on key `p{i:09}` being entity sequence
        // i + 1; every key-to-sequence translation in this file assumes it.
        debug_assert_eq!(id.sequence, i + 1, "entity sequence must follow insertion");
        if (i + 1) % batch == 0 {
            db.commit()?;
        }
        if (i + 1) % PROGRESS == 0 {
            eprintln!(
                "[e4]   … {} rows in ({:.0}s)",
                i + 1,
                at.elapsed().as_secs_f64()
            );
        }
    }
    db.commit()?;
    let load_s = at.elapsed().as_secs_f64();
    eprintln!("[e4] {rows} rows in {load_s:.2}s; building three indexes …");

    // Late build, as SQLite's CREATE INDEX / FTS5 rebuild / R*Tree populate
    // are late. A commit follows each create because a build refuses to start
    // with user writes pending.
    let chunk = batch as usize;
    let at = Instant::now();
    let fullname = db.create_text_index(person_c, "fullname_text", "fullname")?;
    db.commit()?;
    db.build_index_to_ready(fullname, chunk)?;
    let index_text_s = at.elapsed().as_secs_f64();

    let at = Instant::now();
    let born = db.create_scalar_index(person_c, "born_idx", "born", false)?;
    db.commit()?;
    db.build_index_to_ready(born, chunk)?;
    let index_scalar_s = at.elapsed().as_secs_f64();

    let at = Instant::now();
    let addr = db.create_point_index(person_c, "addr_point", "addr")?;
    db.commit()?;
    db.build_index_to_ready(addr, chunk)?;
    let index_point_s = at.elapsed().as_secs_f64();

    db.commit()?;
    let at = Instant::now();
    db.checkpoint()?;
    let checkpoint_s = at.elapsed().as_secs_f64();

    let stages = json!({
        "open_s": open_s,
        "load_s": load_s,
        "index_text_s": index_text_s,
        "index_scalar_s": index_scalar_s,
        "index_point_s": index_point_s,
        "index_total_s": index_text_s + index_scalar_s + index_point_s,
        "checkpoint_s": checkpoint_s,
        "total_s": overall.elapsed().as_secs_f64(),
    });
    eprintln!(
        "[e4] indexes built in {:.2}s; checkpoint {checkpoint_s:.2}s",
        index_text_s + index_scalar_s + index_point_s
    );
    Ok((
        E4Ctx {
            db,
            person: person_c,
            fullname,
            born,
            addr,
            rows,
        },
        stages,
    ))
}

/// Reopen a database `load_e4` built earlier and find its collection and
/// three indexes by NAME, the way a program that did not build the file has
/// to. Every stage but `open_s` is `null`: nothing was loaded or built here,
/// and a zero would read as "instant".
fn open_e4(root: &Path, rows: u64, cache_bytes: usize) -> R<(E4Ctx, Value)> {
    let at = Instant::now();
    let db = Database::open(
        root,
        Config {
            budget_bytes: cache_bytes,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )?;
    let person = db.collection("person")?.ok_or("--reuse: no `person` collection")?;
    let (mut fullname, mut born, mut addr) = (None, None, None);
    for n in 1..=8u64 {
        let Ok(info) = db.index_info(IndexId(n)) else {
            continue;
        };
        match info.name.as_str() {
            "fullname_text" => fullname = Some(info.id),
            "born_idx" => born = Some(info.id),
            "addr_point" => addr = Some(info.id),
            _ => {}
        }
    }
    let ctx = E4Ctx {
        db,
        person,
        fullname: fullname.ok_or("--reuse: no fullname_text index")?,
        born: born.ok_or("--reuse: no born_idx index")?,
        addr: addr.ok_or("--reuse: no addr_point index")?,
        rows,
    };
    let open_s = at.elapsed().as_secs_f64();
    eprintln!("[e4] reopened {} in {open_s:.3}s; queries only", root.display());
    Ok((ctx, reused_stages(open_s)))
}

/// The stage block of a query-only pass: the open is real, the rest did not
/// happen.
fn reused_stages(open_s: f64) -> Value {
    json!({
        "open_s": open_s,
        "load_s": Value::Null,
        "index_text_s": Value::Null,
        "index_scalar_s": Value::Null,
        "index_point_s": Value::Null,
        "index_total_s": Value::Null,
        "checkpoint_s": Value::Null,
        "total_s": Value::Null,
    })
}

// ── the SQLite arm ────────────────────────────────────────────────────────

/// Step the statement, materialise every projected column, count the rows and
/// keep only the first few keys. Column 0 is always the key.
fn lite_answer(connection: &Connection, sql: &str) -> R<Answer> {
    let mut statement = connection.prepare_cached(sql)?;
    let columns = statement.column_count();
    let mut rows = statement.query([])?;
    let mut answer = Answer::default();
    while let Some(row) = rows.next()? {
        for column in 1..columns {
            std::hint::black_box(row.get::<_, rusqlite::types::Value>(column)?);
        }
        answer.push(row.get::<_, String>(0)?);
    }
    Ok(answer)
}

/// The R*Tree overlap constraint plus the exact refine against `person`'s own
/// REAL columns. The R*Tree stores 32-bit floats and rounds its boxes OUTWARD,
/// so its answer is a superset and the refine is not optional.
fn lite_box_clause(b: Bounds) -> String {
    format!(
        "g.maxlon>={:?} AND g.minlon<={:?} AND g.maxlat>={:?} AND g.minlat<={:?} \
         AND p.lon>={:?} AND p.lon<={:?} AND p.lat>={:?} AND p.lat<={:?}",
        b.west(), b.east(), b.south(), b.north(),
        b.west(), b.east(), b.south(), b.north()
    )
}

fn lite_radius_sql(metres: f64, extra: &str) -> String {
    lite_radius_sql_at(center(), metres, extra)
}

fn lite_radius_sql_at(at: Point, metres: f64, extra: &str) -> String {
    let b = radius_candidate_bounds(at, metres).expect("benchmark radius is valid");
    format!(
        "SELECT p._key FROM person_geo g JOIN person p ON p.rowid=g.id WHERE \
         g.maxlon>={:?} AND g.minlon<={:?} AND g.maxlat>={:?} AND g.minlat<={:?} \
         AND geodist(p.lon,p.lat,{:?},{:?})<={:?}{extra}",
        b.west(), b.east(), b.south(), b.north(), at.longitude(), at.latitude(), metres
    )
}

/// SQLite has no spatial nearest-neighbour operator (the R*Tree module
/// answers overlap only), so its k nearest is the spelling a SQLite user
/// would write: order the whole table by the geodesic distance.
fn lite_knn_sql(k: usize) -> String {
    format!(
        "SELECT _key FROM person ORDER BY geodist(lon,lat,{CENTER_LON:?},{CENTER_LAT:?}), _key LIMIT {k}"
    )
}

/// The same battery, in the same order, as SQLite spells it.
fn lite_cases(rows: u64) -> Vec<(&'static str, String)> {
    let mid = key(rows / 2);
    vec![
        (
            "point_lookup",
            format!("SELECT _key, fullname, born FROM person WHERE _key='{mid}'"),
        ),
        ("count_all", "SELECT _key FROM person".into()),
        (
            "name_fulltext",
            "SELECT p._key FROM person p JOIN person_fts ON person_fts.rowid=p.rowid \
             WHERE person_fts MATCH 'sari'"
                .into(),
        ),
        (
            "name_two_terms",
            "SELECT p._key FROM person p JOIN person_fts ON person_fts.rowid=p.rowid \
             WHERE person_fts MATCH 'sari OR wati'"
                .into(),
        ),
        (
            "name_top10",
            "SELECT p._key FROM person p JOIN person_fts ON person_fts.rowid=p.rowid \
             WHERE person_fts MATCH 'diwa' ORDER BY bm25(person_fts), p._key LIMIT 10"
                .into(),
        ),
        (
            "born_decade",
            "SELECT _key FROM person WHERE born>=19900101 AND born<20000101".into(),
        ),
        (
            "born_between",
            "SELECT _key FROM person WHERE born BETWEEN 19900101 AND 19991231".into(),
        ),
        (
            "born_ge_open",
            "SELECT _key FROM person WHERE born>=20100101".into(),
        ),
        (
            "born_one_year",
            "SELECT _key FROM person WHERE born>=19870101 AND born<19880101".into(),
        ),
        (
            "born_one_day",
            "SELECT _key FROM person WHERE born>=19870615 AND born<19870616".into(),
        ),
        ("radius_2km", lite_radius_sql(2_000.0, "")),
        ("radius_50km", lite_radius_sql(50_000.0, "")),
        (
            "bbox",
            format!(
                "SELECT p._key FROM person_geo g JOIN person p ON p.rowid=g.id WHERE {}",
                lite_box_clause(query_box())
            ),
        ),
        ("radius_500m", lite_radius_sql(500.0, "")),
        ("radius_10km", lite_radius_sql(10_000.0, "")),
        ("radius_corner_10km", lite_radius_sql_at(corner(), 10_000.0, "")),
        ("radius_far_empty", lite_radius_sql_at(far_away(), 20_000.0, "")),
        (
            "bbox_2km",
            format!(
                "SELECT p._key FROM person_geo g JOIN person p ON p.rowid=g.id WHERE {}",
                lite_box_clause(small_box())
            ),
        ),
        (
            "bbox_strip",
            format!(
                "SELECT p._key FROM person_geo g JOIN person p ON p.rowid=g.id WHERE {}",
                lite_box_clause(strip_box())
            ),
        ),
        (
            "bbox_and_born",
            format!(
                "SELECT p._key FROM person_geo g JOIN person p ON p.rowid=g.id WHERE {} \
                 AND p.born>=19900101 AND p.born<20000101",
                lite_box_clause(query_box())
            ),
        ),
        (
            "radius_and_name",
            format!(
                "SELECT p._key FROM person_geo g JOIN person p ON p.rowid=g.id \
                 JOIN person_fts ON person_fts.rowid=p.rowid WHERE {} AND person_fts MATCH 'sari'",
                lite_radius_sql(10_000.0, "").split_once("WHERE ").expect("radius sql has WHERE").1
            ),
        ),
        ("radius_top10", format!("{} LIMIT 10", lite_radius_sql(10_000.0, ""))),
        ("knn_10", lite_knn_sql(10)),
        ("knn_100", lite_knn_sql(100)),
        (
            "radius_and_born",
            lite_radius_sql(10_000.0, " AND p.born>=19900101 AND p.born<20000101"),
        ),
        (
            "name_and_born",
            "SELECT p._key FROM person p JOIN person_fts ON person_fts.rowid=p.rowid \
             WHERE person_fts MATCH 'sari' AND p.born>=19800101 AND p.born<19900101"
                .into(),
        ),
        (
            "oldest_10",
            "SELECT _key, born FROM person ORDER BY born ASC, _key ASC LIMIT 10".into(),
        ),
        (
            "youngest_10",
            "SELECT _key, born FROM person ORDER BY born DESC, _key ASC LIMIT 10".into(),
        ),
    ]
}

/// Open (or create) the SQLite file with the arm's pragmas and the geodesic
/// function; the schema is the caller's business.
fn lite_connect(root: &Path, cache_bytes: usize) -> R<Connection> {
    let connection = Connection::open(root.join("person.db"))?;
    // E4's radius is an exact WGS84 geodesic. SQLite is given the SAME routine
    // rather than a haversine that would disagree at the boundary.
    connection.create_scalar_function(
        "geodist",
        4,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let a = Point::new(ctx.get::<f64>(0)?, ctx.get::<f64>(1)?)
                .map_err(|e| rusqlite::Error::UserFunctionError(e.into()))?;
            let b = Point::new(ctx.get::<f64>(2)?, ctx.get::<f64>(3)?)
                .map_err(|e| rusqlite::Error::UserFunctionError(e.into()))?;
            Ok(wgs84_distance_metres(a, b))
        },
    )?;
    connection.execute_batch(&format!(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON;
         PRAGMA cache_size=-{}; PRAGMA temp_store=FILE;",
        cache_bytes / 1024
    ))?;
    Ok(connection)
}

/// Reopen a file `load_lite` built earlier: queries only, stages `null`.
fn open_lite(root: &Path, cache_bytes: usize) -> R<(Connection, Value)> {
    let at = Instant::now();
    let connection = lite_connect(root, cache_bytes)?;
    let tables: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name IN ('person','person_fts','person_geo','idx_born')",
        [],
        |r| r.get(0),
    )?;
    if tables != 4 {
        return Err(format!("--reuse: {} of the 4 expected SQLite objects present", tables).into());
    }
    let open_s = at.elapsed().as_secs_f64();
    eprintln!("[sqlite] reopened {} in {open_s:.3}s; queries only", root.display());
    Ok((connection, reused_stages(open_s)))
}

fn load_lite(root: &Path, rows: u64, batch: u64, cache_bytes: usize) -> R<(Connection, Value)> {
    fs::create_dir_all(root)?;
    let overall = Instant::now();

    let at = Instant::now();
    let connection = lite_connect(root, cache_bytes)?;
    connection.execute_batch(
        "CREATE TABLE person(_key TEXT PRIMARY KEY, fullname TEXT, born INTEGER,
                             born_year INTEGER, lon REAL, lat REAL);",
    )?;
    let open_s = at.elapsed().as_secs_f64();

    eprintln!("[sqlite] inserting {rows} rows, commit every {batch} …");
    let at = Instant::now();
    connection.execute_batch("BEGIN")?;
    {
        let mut insert =
            connection.prepare("INSERT INTO person VALUES (?1,?2,?3,?4,?5,?6)")?;
        for i in 0..rows {
            let p = person(i);
            insert.execute(rusqlite::params![
                p.key,
                p.fullname,
                p.born,
                p.born_year,
                p.lon,
                p.lat
            ])?;
            if (i + 1) % batch == 0 {
                connection.execute_batch("COMMIT; BEGIN")?;
            }
            if (i + 1) % PROGRESS == 0 {
                eprintln!(
                    "[sqlite]   … {} rows in ({:.0}s)",
                    i + 1,
                    at.elapsed().as_secs_f64()
                );
            }
        }
    }
    connection.execute_batch("COMMIT")?;
    let load_s = at.elapsed().as_secs_f64();
    eprintln!("[sqlite] {rows} rows in {load_s:.2}s; building three indexes …");

    // All three indexes are built AFTER the load, so this arm pays the same
    // late-build cost E4's `build_index_to_ready` does.
    let at = Instant::now();
    connection.execute_batch(
        "CREATE VIRTUAL TABLE person_fts USING fts5(fullname, content='person',
                                                    content_rowid='rowid');
         INSERT INTO person_fts(person_fts) VALUES('rebuild');",
    )?;
    let index_text_s = at.elapsed().as_secs_f64();

    let at = Instant::now();
    connection.execute_batch("CREATE INDEX idx_born ON person(born);")?;
    let index_scalar_s = at.elapsed().as_secs_f64();

    let at = Instant::now();
    connection.execute_batch(
        "CREATE VIRTUAL TABLE person_geo USING rtree(id, minlon, maxlon, minlat, maxlat);
         INSERT INTO person_geo SELECT rowid, lon, lon, lat, lat FROM person;",
    )?;
    let index_point_s = at.elapsed().as_secs_f64();

    let at = Instant::now();
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let checkpoint_s = at.elapsed().as_secs_f64();

    let stages = json!({
        "open_s": open_s,
        "load_s": load_s,
        "index_text_s": index_text_s,
        "index_scalar_s": index_scalar_s,
        "index_point_s": index_point_s,
        "index_total_s": index_text_s + index_scalar_s + index_point_s,
        "checkpoint_s": checkpoint_s,
        "total_s": overall.elapsed().as_secs_f64(),
    });
    eprintln!(
        "[sqlite] indexes built in {:.2}s; checkpoint {checkpoint_s:.2}s",
        index_text_s + index_scalar_s + index_point_s
    );
    Ok((connection, stages))
}

// ── the postgres arm ──────────────────────────────────────────────────────

const DEFAULT_PG_DSN: &str = "postgres://127.0.0.1:55432/postgres";

/// Swap the trailing `/database` segment of a DSN for a freshly created one.
fn dsn_with_db(dsn: &str, db: &str) -> String {
    match dsn.rfind('/') {
        Some(idx) => format!("{}/{db}", &dsn[..idx]),
        None => format!("{dsn}/{db}"),
    }
}

/// Column 0 is always the key. Extra columns are touched (materialised) the
/// same way `lite_answer` touches SQLite's, so both arms pay for pulling the
/// row apart, not just for finding it. The battery only ever selects `TEXT`
/// or `INT8` beyond column 0, so those are the only types handled.
fn pg_touch(row: &postgres::Row, idx: usize) -> R<()> {
    match row.columns()[idx].type_().clone() {
        Type::TEXT | Type::VARCHAR => {
            std::hint::black_box(row.try_get::<_, String>(idx)?);
        }
        Type::INT8 => {
            std::hint::black_box(row.try_get::<_, i64>(idx)?);
        }
        Type::INT4 => {
            std::hint::black_box(row.try_get::<_, i32>(idx)?);
        }
        other => return Err(format!("pg_touch: unexpected column type {other}").into()),
    }
    Ok(())
}

/// Every case's SQL is a literal-embedded string, same convention `lite_cases`
/// uses for its radius/bbox constants — the values are fixed Rust constants,
/// never user input, so binding them as query parameters would add ceremony
/// without adding safety. `query_raw` with no parameters still gives a
/// streamed row-by-row iterator, which is what `count_all` needs: this arm
/// must never spell `SELECT COUNT(*)`, so Postgres cannot answer from an
/// index-only fast count instead of a real per-row enumeration — see the
/// `count_all` deviation note the other two arms already carry.
fn pg_answer(client: &mut Client, sql: &str) -> R<Answer> {
    use postgres::fallible_iterator::FallibleIterator;
    let mut answer = Answer::default();
    let mut rows = client.query_raw(sql, std::iter::empty::<i32>())?;
    while let Some(row) = rows.next()? {
        for idx in 1..row.len() {
            pg_touch(&row, idx)?;
        }
        answer.push(row.try_get::<_, String>(0)?);
    }
    Ok(answer)
}

/// The exact WGS84-geodesic candidate: `ST_DWithin` on a `geography` column
/// with `use_spheroid => true` explicitly passed (it is also the default),
/// so Postgres measures the same ellipsoidal model E4's Karney routine does,
/// not the cheaper great-circle sphere model. Boundary rows can still
/// disagree by the last metre between Karney's convergent series and
/// PostGIS's own ellipsoidal inverse; see the report for a boundary count.
fn pg_radius_sql(metres: f64, extra: &str) -> String {
    pg_radius_sql_at(center(), metres, extra)
}

fn pg_radius_sql_at(at: Point, metres: f64, extra: &str) -> String {
    format!(
        "SELECT k FROM person WHERE ST_DWithin(addr, \
         ST_SetSRID(ST_MakePoint({:?},{:?}),4326)::geography, {metres:?}, true){extra}",
        at.longitude(),
        at.latitude()
    )
}

/// PostGIS's k nearest: the KNN-GiST ordered walk (`<->` on geography is the
/// spheroidal distance in metres), ties broken by key like the other arms.
fn pg_knn_sql(k: usize) -> String {
    format!(
        "SELECT k FROM person ORDER BY addr <-> \
         ST_SetSRID(ST_MakePoint({CENTER_LON:?},{CENTER_LAT:?}),4326)::geography, k LIMIT {k}"
    )
}

/// The GiST `&&` overlap operator supplies the index-accelerated candidate
/// set (a bounding-box test, like SQLite's R*Tree join); the `ST_X`/`ST_Y`
/// clause is the exact refine against the same closed interval
/// `lite_box_clause` tests (`>=` west/south, `<=` east/north, all four edges
/// inclusive) — the geography type's own internal box representation is a
/// float4 approximation, so the refine is not optional here either.
fn pg_bbox_sql(b: Bounds) -> String {
    format!(
        "SELECT k FROM person WHERE addr && ST_MakeEnvelope({:?},{:?},{:?},{:?},4326)::geography \
         AND ST_X(addr::geometry) BETWEEN {:?} AND {:?} \
         AND ST_Y(addr::geometry) BETWEEN {:?} AND {:?}",
        b.west(), b.south(), b.east(), b.north(),
        b.west(), b.east(), b.south(), b.north()
    )
}

/// The same battery, in the same order, as Postgres spells it. Every case not
/// named in the module doc (the plain `born` ranges, `oldest_10`/
/// `youngest_10`) is the same predicate/ORDER BY the SQLite arm uses, `_key`
/// renamed to `k` and nothing else changed.
fn pg_cases(rows: u64) -> Vec<(&'static str, String)> {
    let mid = key(rows / 2);
    vec![
        (
            "point_lookup",
            format!("SELECT k, fullname, born FROM person WHERE k='{mid}'"),
        ),
        ("count_all", "SELECT k FROM person".into()),
        (
            "name_fulltext",
            "SELECT k FROM person WHERE to_tsvector('simple', fullname) \
             @@ to_tsquery('simple', 'sari')"
                .into(),
        ),
        (
            "name_two_terms",
            // TextMatch::Any is an OR of terms (E4 merges by taking the min
            // doc across all term posting streams); `sari | wati` is the
            // `simple`-config equivalent.
            "SELECT k FROM person WHERE to_tsvector('simple', fullname) \
             @@ to_tsquery('simple', 'sari | wati')"
                .into(),
        ),
        (
            "name_top10",
            // ts_rank_cd, not BM25 — a different ranking formula, so (like
            // the other two arms) this case is compared on row count only.
            "SELECT k FROM person WHERE to_tsvector('simple', fullname) \
             @@ to_tsquery('simple', 'diwa') \
             ORDER BY ts_rank_cd(to_tsvector('simple', fullname), to_tsquery('simple', 'diwa')) DESC, \
             k ASC LIMIT 10"
                .into(),
        ),
        (
            "born_decade",
            "SELECT k FROM person WHERE born>=19900101 AND born<20000101".into(),
        ),
        (
            "born_between",
            "SELECT k FROM person WHERE born BETWEEN 19900101 AND 19991231".into(),
        ),
        (
            "born_ge_open",
            "SELECT k FROM person WHERE born>=20100101".into(),
        ),
        (
            "born_one_year",
            "SELECT k FROM person WHERE born>=19870101 AND born<19880101".into(),
        ),
        (
            "born_one_day",
            "SELECT k FROM person WHERE born>=19870615 AND born<19870616".into(),
        ),
        ("radius_2km", pg_radius_sql(2_000.0, "")),
        ("radius_50km", pg_radius_sql(50_000.0, "")),
        ("bbox", pg_bbox_sql(query_box())),
        ("radius_500m", pg_radius_sql(500.0, "")),
        ("radius_10km", pg_radius_sql(10_000.0, "")),
        ("radius_corner_10km", pg_radius_sql_at(corner(), 10_000.0, "")),
        ("radius_far_empty", pg_radius_sql_at(far_away(), 20_000.0, "")),
        ("bbox_2km", pg_bbox_sql(small_box())),
        ("bbox_strip", pg_bbox_sql(strip_box())),
        (
            "bbox_and_born",
            format!("{} AND born>=19900101 AND born<20000101", pg_bbox_sql(query_box())),
        ),
        (
            "radius_and_name",
            pg_radius_sql(
                10_000.0,
                " AND to_tsvector('simple', fullname) @@ to_tsquery('simple', 'sari')",
            ),
        ),
        ("radius_top10", format!("{} LIMIT 10", pg_radius_sql(10_000.0, ""))),
        ("knn_10", pg_knn_sql(10)),
        ("knn_100", pg_knn_sql(100)),
        (
            "radius_and_born",
            pg_radius_sql(10_000.0, " AND born>=19900101 AND born<20000101"),
        ),
        (
            "name_and_born",
            "SELECT k FROM person WHERE to_tsvector('simple', fullname) \
             @@ to_tsquery('simple', 'sari') AND born>=19800101 AND born<19900101"
                .into(),
        ),
        (
            "oldest_10",
            "SELECT k, born FROM person ORDER BY born ASC, k ASC LIMIT 10".into(),
        ),
        (
            "youngest_10",
            "SELECT k, born FROM person ORDER BY born DESC, k ASC LIMIT 10".into(),
        ),
    ]
}

/// One multi-row `INSERT ... VALUES (...), (...), ...` per batch — the
/// idiomatic shape a real Postgres client uses at this commit cadence, not
/// 256 single-row round trips. Bound as real parameters (not literal-embedded
/// like the read side): `fullname` is generator output, and binding is the
/// straightforward way to hand a dynamic-length batch to `postgres::Client`
/// without hand-escaping text.
fn insert_batch(client: &mut Client, people: &[Person]) -> R<()> {
    let mut sql = String::from("INSERT INTO person (k, fullname, born, born_year, addr) VALUES ");
    let mut params: Vec<Box<dyn ToSql + Sync>> = Vec::with_capacity(people.len() * 6);
    for (i, p) in people.iter().enumerate() {
        if i > 0 {
            sql.push(',');
        }
        let base = i * 6;
        sql.push_str(&format!(
            "(${},${},${},${},ST_SetSRID(ST_MakePoint(${},${}),4326)::geography)",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6
        ));
        params.push(Box::new(p.key.clone()));
        params.push(Box::new(p.fullname.clone()));
        params.push(Box::new(p.born));
        params.push(Box::new(p.born_year as i32));
        params.push(Box::new(p.lon));
        params.push(Box::new(p.lat));
    }
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
    // Explicit BEGIN/COMMIT per batch, matching the other two arms' commit
    // cadence documentation, though a lone multi-row statement would already
    // be its own implicit transaction under Postgres's autocommit.
    let mut txn = client.transaction()?;
    txn.execute(sql.as_str(), &refs)?;
    txn.commit()?;
    Ok(())
}

/// Reopen the database an earlier `load_pg` pass built. The Postgres arm has
/// no file of its own to find: the database's name travelled in the report
/// the build pass wrote under `<root>/popsim-postgres.json`, so that is what
/// is read back, and the server named by `--dsn` must still hold it with the
/// table and its three indexes. Stages other than `open_s` are `null`.
fn open_pg(root: &Path, dsn_base: &str) -> R<(Client, Value, String, String)> {
    let at = Instant::now();
    // `run_arm` writes the report beside the arm directories, not inside
    // this arm's own one.
    let report_path = root.parent().unwrap_or(root).join("popsim-postgres.json");
    let report: Value = serde_json::from_str(&fs::read_to_string(&report_path).map_err(|e| {
        format!("--reuse: no earlier report at {}: {e}", report_path.display())
    })?)?;
    let db_name = report["database"]
        .as_str()
        .ok_or("--reuse: the earlier report names no database")?
        .to_string();
    let dsn = dsn_with_db(dsn_base, &db_name);
    let mut client = Client::connect(&dsn, NoTls)?;
    let indexes: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_indexes WHERE tablename = 'person'",
            &[],
        )?
        .get(0);
    if indexes < 3 {
        return Err(format!("--reuse: {db_name} has {indexes} indexes on person, expected 3").into());
    }
    let open_s = at.elapsed().as_secs_f64();
    eprintln!("[postgres] reopened {db_name} in {open_s:.3}s; queries only");
    Ok((client, reused_stages(open_s), dsn, db_name))
}

fn load_pg(
    root: &Path,
    rows: u64,
    batch: u64,
    dsn_base: &str,
) -> R<(Client, Value, String, String)> {
    fs::create_dir_all(root)?;
    let overall = Instant::now();

    let at = Instant::now();
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let db_name = format!("popsim_{rows}_{stamp}");
    // A fresh database per run, so a rerun never sees a stale index or a
    // leftover row from a previous size — the same "start from nothing" rule
    // `run_arm` already applies to the other two arms' directories.
    let mut admin = Client::connect(dsn_base, NoTls)?;
    admin.batch_execute(&format!("CREATE DATABASE \"{db_name}\""))?;
    drop(admin);

    let dsn = dsn_with_db(dsn_base, &db_name);
    let mut client = Client::connect(&dsn, NoTls)?;
    client.batch_execute("CREATE EXTENSION IF NOT EXISTS postgis;")?;
    // synchronous_commit and fsync are both ON by default on a stock Postgres
    // server; synchronous_commit is set explicitly anyway so the load pays
    // the same per-commit durability barrier the other two arms document.
    client.batch_execute(
        "SET synchronous_commit = on;
         CREATE TABLE person (
             k text primary key,
             fullname text,
             born bigint,
             born_year int,
             addr geography(Point,4326)
         );",
    )?;
    let open_s = at.elapsed().as_secs_f64();

    eprintln!("[postgres] inserting {rows} rows, commit every {batch} …");
    let at = Instant::now();
    let mut pending: Vec<Person> = Vec::with_capacity(batch as usize);
    for i in 0..rows {
        pending.push(person(i));
        if pending.len() as u64 == batch {
            insert_batch(&mut client, &pending)?;
            pending.clear();
        }
        if (i + 1) % PROGRESS == 0 {
            eprintln!(
                "[postgres]   … {} rows in ({:.0}s)",
                i + 1,
                at.elapsed().as_secs_f64()
            );
        }
    }
    if !pending.is_empty() {
        insert_batch(&mut client, &pending)?;
    }
    let load_s = at.elapsed().as_secs_f64();
    eprintln!("[postgres] {rows} rows in {load_s:.2}s; building three indexes …");

    // Late build, same as the other two arms.
    let at = Instant::now();
    client.batch_execute(
        "CREATE INDEX person_fts_idx ON person USING gin (to_tsvector('simple', fullname));",
    )?;
    let index_text_s = at.elapsed().as_secs_f64();

    let at = Instant::now();
    client.batch_execute("CREATE INDEX person_born_idx ON person (born);")?;
    let index_scalar_s = at.elapsed().as_secs_f64();

    let at = Instant::now();
    client.batch_execute("CREATE INDEX person_addr_gix ON person USING gist (addr);")?;
    let index_point_s = at.elapsed().as_secs_f64();

    // Postgres's analog of the other two arms' checkpoint: ANALYZE so the
    // planner has fresh stats for the indexes just built, then CHECKPOINT so
    // dirty buffers are forced to disk — both folded into one timed stage.
    let at = Instant::now();
    client.batch_execute("ANALYZE person; CHECKPOINT;")?;
    let checkpoint_s = at.elapsed().as_secs_f64();

    let stages = json!({
        "open_s": open_s,
        "load_s": load_s,
        "index_text_s": index_text_s,
        "index_scalar_s": index_scalar_s,
        "index_point_s": index_point_s,
        "index_total_s": index_text_s + index_scalar_s + index_point_s,
        "checkpoint_s": checkpoint_s,
        "total_s": overall.elapsed().as_secs_f64(),
    });
    eprintln!(
        "[postgres] indexes built in {:.2}s; analyze+checkpoint {checkpoint_s:.2}s",
        index_text_s + index_scalar_s + index_point_s
    );
    Ok((client, stages, dsn, db_name))
}

/// `pg_total_relation_size` (table + indexes + TOAST + free-space/visibility
/// maps — confirmed against the Postgres docs) alongside
/// `pg_database_size(current_database())`. `bytes_on_disk` in the report is
/// the DATABASE size, for comparability with the other two arms' `dir_bytes`
/// (which sums an entire directory, not just one collection's own pages);
/// the relation-only number is kept alongside it, not dropped.
fn pg_bytes(client: &mut Client) -> R<(u64, u64)> {
    let row = client.query_one(
        "SELECT pg_total_relation_size('person'), pg_database_size(current_database())",
        &[],
    )?;
    let relation: i64 = row.try_get(0)?;
    let database: i64 = row.try_get(1)?;
    Ok((relation as u64, database as u64))
}

/// Container memory via `docker stats`, since the server runs in Docker; not
/// a host-process RSS read from `/proc`. `None` if the container name isn't
/// running under `docker` on this host (report omits the field, never fakes
/// it).
fn pg_container_rss_bytes(container: &str) -> Option<u64> {
    let output = std::process::Command::new("docker")
        .args(["stats", "--no-stream", "--format", "{{.MemUsage}}", container])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let used = text.split('/').next()?.trim();
    parse_docker_mem(used)
}

fn parse_docker_mem(s: &str) -> Option<u64> {
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic())?);
    let value: f64 = num.trim().parse().ok()?;
    let mult = match unit.trim() {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "KB" => 1000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        _ => return None,
    };
    Some((value * mult) as u64)
}

// ── measurement ───────────────────────────────────────────────────────────

/// Warm once untimed, then time until BOTH the sample floor and a short time
/// floor are met, and report the MEDIAN — one page fault in one iteration
/// should not become the case's number. `budget` is the hard ceiling: a case
/// slow enough to hit it reports the samples it managed, never fewer than one.
fn bench(
    reps: usize,
    budget: Duration,
    mut f: impl FnMut() -> R<Answer>,
) -> R<(f64, usize, Answer)> {
    f()?; // warm, untimed
    let mut samples = Vec::with_capacity(reps);
    let start = Instant::now();
    let answer = loop {
        let at = Instant::now();
        let answer = f()?;
        samples.push(at.elapsed().as_secs_f64() * 1e6);
        let elapsed = start.elapsed();
        let enough = samples.len() >= reps && elapsed >= Duration::from_millis(200);
        if enough || elapsed >= budget {
            break answer;
        }
    };
    samples.sort_by(f64::total_cmp);
    Ok((samples[samples.len() / 2], samples.len(), answer))
}

// ── one arm, end to end ───────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    E4,
    Sqlite,
    Postgres,
}

impl Arm {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "e4" => Some(Self::E4),
            "sqlite" => Some(Self::Sqlite),
            "postgres" => Some(Self::Postgres),
            _ => None,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::E4 => "e4",
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub arm: Arm,
    pub rows: u64,
    pub root: PathBuf,
    pub batch: u64,
    pub only: Option<String>,
    pub reps: usize,
    pub case_budget: Duration,
    pub cache_bytes: usize,
    /// Open the arm's existing database and run only the queries: no wipe, no
    /// load, no index build. The load and build stages are then reported as
    /// `null` and `"reused": true`, never as zero.
    pub reuse: bool,
    /// Postgres arm only: the DSN of the maintenance connection used to
    /// `CREATE DATABASE` a fresh per-run database. Ignored by the other arms.
    pub dsn: Option<String>,
}

impl Options {
    pub fn new(arm: Arm, rows: u64, root: impl Into<PathBuf>) -> Self {
        Self {
            arm,
            rows,
            root: root.into(),
            batch: BATCH,
            only: None,
            reps: 5,
            case_budget: Duration::from_secs(300),
            cache_bytes: CACHE_BYTES,
            reuse: false,
            dsn: None,
        }
    }
}

/// Run one arm end to end and return its report. The report is also written to
/// `<root>/popsim-<arm>.json` so the two processes can be compared afterwards.
pub fn run_arm(options: &Options) -> R<Value> {
    let arm = options.arm.label();
    let db_root = options.root.join(arm);
    if options.reuse {
        // A query-only pass over a database an earlier run left behind. The
        // file is the whole point, so its absence is an error, not a rebuild.
        if !db_root.is_dir() {
            return Err(format!("--reuse: no {arm} database at {}", db_root.display()).into());
        }
    } else {
        // The arm owns this subdirectory by name; a rerun starts from nothing.
        let _ = fs::remove_dir_all(&db_root);
        fs::create_dir_all(&options.root)?;
    }

    let selected = |name: &str| {
        options
            .only
            .as_deref()
            .is_none_or(|needle| name.contains(needle))
    };

    let mut measured = Vec::new();
    let stages;
    // Bytes/files are computed per arm below: the first two are a directory
    // sum, taken AFTER CLOSE so a WAL only folded away on close is counted;
    // the postgres arm has no local directory worth summing (the data lives
    // on the server), so it reports server-side sizes instead.
    let bytes;
    let files;
    let mut extra = serde_json::Map::new();
    match options.arm {
        Arm::E4 => {
            let (ctx, s) = if options.reuse {
                open_e4(&db_root, options.rows, options.cache_bytes)?
            } else {
                load_e4(&db_root, options.rows, options.batch, options.cache_bytes)?
            };
            stages = s;
            for (name, run) in e4_cases() {
                if !selected(name) {
                    continue;
                }
                let (micros, samples, answer) =
                    bench(options.reps, options.case_budget, || run(&ctx))
                        .map_err(|e| format!("case {name}: {e}"))?;
                eprintln!("[e4] {name:<16} {micros:>12.1} us  rows={}", answer.rows);
                measured.push(json!({
                    "name": name, "micros": micros, "samples": samples,
                    "rows": answer.rows, "keys": answer.keys,
                }));
            }
            drop(ctx);
            bytes = dir_bytes(&db_root);
            files = file_map(&db_root);
        }
        Arm::Sqlite => {
            let (connection, s) = if options.reuse {
                open_lite(&db_root, options.cache_bytes)?
            } else {
                load_lite(&db_root, options.rows, options.batch, options.cache_bytes)?
            };
            stages = s;
            for (name, sql) in lite_cases(options.rows) {
                if !selected(name) {
                    continue;
                }
                let (micros, samples, answer) =
                    bench(options.reps, options.case_budget, || {
                        lite_answer(&connection, &sql)
                    })
                    .map_err(|e| format!("case {name}: {e}"))?;
                eprintln!("[sqlite] {name:<16} {micros:>12.1} us  rows={}", answer.rows);
                measured.push(json!({
                    "name": name, "micros": micros, "samples": samples,
                    "rows": answer.rows, "keys": answer.keys,
                }));
            }
            drop(connection);
            bytes = dir_bytes(&db_root);
            files = file_map(&db_root);
        }
        Arm::Postgres => {
            let dsn_base = options.dsn.as_deref().unwrap_or(DEFAULT_PG_DSN);
            let (mut client, s, dsn, db_name) = if options.reuse {
                open_pg(&db_root, dsn_base)?
            } else {
                load_pg(&db_root, options.rows, options.batch, dsn_base)?
            };
            stages = s;
            for (name, sql) in pg_cases(options.rows) {
                if !selected(name) {
                    continue;
                }
                let (micros, samples, answer) =
                    bench(options.reps, options.case_budget, || pg_answer(&mut client, &sql))
                        .map_err(|e| format!("case {name}: {e}"))?;
                eprintln!("[postgres] {name:<16} {micros:>12.1} us  rows={}", answer.rows);
                measured.push(json!({
                    "name": name, "micros": micros, "samples": samples,
                    "rows": answer.rows, "keys": answer.keys,
                }));
            }
            let (relation, database) = pg_bytes(&mut client)?;
            drop(client);
            bytes = database;
            files = json!({
                "pg_total_relation_size_person": relation,
                "pg_database_size": database,
            });
            extra.insert("dsn".into(), json!(dsn));
            extra.insert("database".into(), json!(db_name));
            match pg_container_rss_bytes("e4-pg") {
                Some(rss) => {
                    extra.insert("server_rss_bytes".into(), json!(rss));
                }
                None => {
                    extra.insert(
                        "server_rss_bytes_note".into(),
                        json!("not obtainable: no 'e4-pg' docker container reachable via `docker stats`"),
                    );
                }
            }
        }
    }

    let mut report = json!({
        "arm": arm,
        "rows": options.rows,
        "batch": options.batch,
        "cache_bytes": options.cache_bytes,
        "reps": options.reps,
        "reused": options.reuse,
        "stages": stages,
        "bytes_on_disk": bytes,
        "bytes_per_row": bytes as f64 / options.rows.max(1) as f64,
        "files": files,
        "queries": measured,
        "unsupported": UNSUPPORTED.iter()
            .map(|(name, why)| json!({"name": name, "reason": why}))
            .collect::<Vec<_>>(),
    });
    if let Value::Object(map) = &mut report {
        map.extend(extra);
    }
    let out = options.root.join(format!("popsim-{arm}.json"));
    fs::write(&out, serde_json::to_string_pretty(&report)?)?;
    eprintln!(
        "[{arm}] {} bytes on disk ({:.1} bytes/row); report at {}",
        bytes,
        bytes as f64 / options.rows.max(1) as f64,
        out.display()
    );
    Ok(report)
}

fn usage() -> String {
    "usage: popsim e4|sqlite|postgres <rows> <fresh-dir> [--batch N] [--only substr] \
     [--reps N] [--case-budget SECS] [--cache-bytes N] [--reuse] [--dsn URL]"
        .into()
}

fn parse(args: &[String]) -> R<Options> {
    if args.len() < 3 {
        return Err(usage().into());
    }
    let arm = Arm::parse(&args[0]).ok_or_else(usage)?;
    let rows: u64 = args[1].parse().map_err(|_| usage())?;
    let mut options = Options::new(arm, rows, &args[2]);
    let mut rest = args[3..].iter();
    while let Some(flag) = rest.next() {
        let mut value = || rest.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--batch" => options.batch = value()?.parse()?,
            "--only" => options.only = Some(value()?),
            "--reps" => options.reps = value()?.parse()?,
            "--case-budget" => options.case_budget = Duration::from_secs(value()?.parse()?),
            "--cache-bytes" => options.cache_bytes = value()?.parse()?,
            "--reuse" => options.reuse = true,
            "--dsn" => options.dsn = Some(value()?),
            other => return Err(format!("unknown flag {other}\n{}", usage()).into()),
        }
    }
    if options.batch == 0 || options.reps == 0 {
        return Err("--batch and --reps must be at least 1".into());
    }
    Ok(options)
}

fn main() -> R<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = parse(&args)?;
    let report = run_arm(&options)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
