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
//!     popsim e4|sqlite <rows> <fresh-dir> [--batch N] [--only substr]
//!                                        [--reps N] [--case-budget SECS]
//!                                        [--cache-bytes N]
//!
//! The arm writes its database under `<fresh-dir>/<arm>` and its results to
//! `<fresh-dir>/popsim-<arm>.json`. `.insert-loop/p2final/popsim_compare.py`
//! reads the two JSONs and prints the comparison.
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
        SortDirection, TextMatch,
    },
    spatial_math::{radius_candidate_bounds, wgs84_distance_metres, Bounds, Point},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use rusqlite::{functions::FunctionFlags, Connection};
use serde_json::{json, Value};
use std::{
    fs,
    ops::Bound,
    path::{Path, PathBuf},
    time::{Duration, Instant},
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
    QueryFilter::Point {
        index,
        predicate: PointFilter::Radius {
            center: center(),
            radius_metres: metres,
        },
    }
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
                QueryOrder::EntityId,
                Projection::Ids,
                None,
                CandidateDriver::Entities,
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
    let b = radius_candidate_bounds(center(), metres).expect("benchmark radius is valid");
    format!(
        "SELECT p._key FROM person_geo g JOIN person p ON p.rowid=g.id WHERE \
         g.maxlon>={:?} AND g.minlon<={:?} AND g.maxlat>={:?} AND g.minlat<={:?} \
         AND geodist(p.lon,p.lat,{:?},{:?})<={:?}{extra}",
        b.west(), b.east(), b.south(), b.north(), CENTER_LON, CENTER_LAT, metres
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

fn load_lite(root: &Path, rows: u64, batch: u64, cache_bytes: usize) -> R<(Connection, Value)> {
    fs::create_dir_all(root)?;
    let overall = Instant::now();

    let at = Instant::now();
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
         PRAGMA cache_size=-{}; PRAGMA temp_store=FILE;
         CREATE TABLE person(_key TEXT PRIMARY KEY, fullname TEXT, born INTEGER,
                             born_year INTEGER, lon REAL, lat REAL);",
        cache_bytes / 1024
    ))?;
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
}

impl Arm {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "e4" => Some(Self::E4),
            "sqlite" => Some(Self::Sqlite),
            _ => None,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::E4 => "e4",
            Self::Sqlite => "sqlite",
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
        }
    }
}

/// Run one arm end to end and return its report. The report is also written to
/// `<root>/popsim-<arm>.json` so the two processes can be compared afterwards.
pub fn run_arm(options: &Options) -> R<Value> {
    let arm = options.arm.label();
    let db_root = options.root.join(arm);
    // The arm owns this subdirectory by name; a rerun starts from nothing.
    let _ = fs::remove_dir_all(&db_root);
    fs::create_dir_all(&options.root)?;

    let selected = |name: &str| {
        options
            .only
            .as_deref()
            .is_none_or(|needle| name.contains(needle))
    };

    let mut measured = Vec::new();
    let stages;
    match options.arm {
        Arm::E4 => {
            let (ctx, s) = load_e4(&db_root, options.rows, options.batch, options.cache_bytes)?;
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
        }
        Arm::Sqlite => {
            let (connection, s) =
                load_lite(&db_root, options.rows, options.batch, options.cache_bytes)?;
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
        }
    }

    // Bytes on disk AFTER CLOSE, so a WAL that is only folded away on close is
    // counted in whichever arm does that.
    let bytes = dir_bytes(&db_root);
    let report = json!({
        "arm": arm,
        "rows": options.rows,
        "batch": options.batch,
        "cache_bytes": options.cache_bytes,
        "reps": options.reps,
        "stages": stages,
        "bytes_on_disk": bytes,
        "bytes_per_row": bytes as f64 / options.rows.max(1) as f64,
        "files": file_map(&db_root),
        "queries": measured,
        "unsupported": UNSUPPORTED.iter()
            .map(|(name, why)| json!({"name": name, "reason": why}))
            .collect::<Vec<_>>(),
    });
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
    "usage: popsim e4|sqlite <rows> <fresh-dir> [--batch N] [--only substr] \
     [--reps N] [--case-budget SECS] [--cache-bytes N]"
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
