//! TWO WAYS — the same question asked two ways, in one process, on identical
//! data:
//!
//!     E4      this prototype's embedded API   (collections + index catalog
//!                                              + prepare_query + graph)
//!     SQLITE  the reference
//!
//! This is the e3 `three_ways` benchmark with its middle arm removed. E4 has
//! no SQL front end yet (that is Phase 3), so there is no "what a user types"
//! arm to measure: only the engine and the reference. Case names, families
//! and ranking are e3's, so the two repositories can be read side by side.
//!
//! Ranked worst-first against SQLite so an anomaly cannot hide behind an
//! average. Row counts are compared on every case, and the key SEQUENCE on
//! every ordered or limited case: a speed win that is really a correctness
//! loss prints DISAGREE.
//!
//! WHAT E4 CANNOT SAY. A case E4's API cannot express is printed in the
//! UNSUPPORTED section with the reason, never silently dropped and never
//! emulated in the harness to produce a number. An aggregate answered by
//! materialising every row is not an aggregate, and an OFFSET answered by
//! throwing rows away in the benchmark is not an OFFSET.
//!
//! FAIRNESS, stated because a write benchmark against mismatched sync levels
//! is not a measurement but a fiction:
//!
//!   * DURABILITY IS MATCHED AND ON, not off. E4's page-WAL publishes every
//!     commit with a FULL barrier — `File::sync_data`, which is
//!     `fcntl(F_FULLFSYNC)` on macOS — and refuses any other sync mode.
//!     SQLite is therefore run WAL + `synchronous=FULL` + `fullfsync=ON`,
//!     which is the same barrier on this platform. Both caches are 8 MiB.
//!   * COMMIT CADENCE IS MATCHED. Both arms commit every 256 rows during the
//!     load, so both pay the same NUMBER of barriers. (e3's three_ways loaded
//!     SQLite in one transaction; that is a different question.)
//!   * THE INDEX SET IS MATCHED where both engines have the family: ordered
//!     rating and price, equality cat and area, full text on note, and the
//!     edge table. E4 additionally builds a point index and an exact vector
//!     index, which SQLite has no equivalent for; those cases run with no
//!     SQLite arm and the load numbers say so.
//!   * EVERY ORDERED CASE NAMES ITS TIE-BREAK. E4's rank key is (value,
//!     ascending entity id), so each SQLite ORDER BY ends in `, k ASC`.
//!     Without that the two arms would agree on every row and differ only in
//!     the order of ties, and every LIMIT case would read as a DISAGREE.
//!   * Keys are zero padded (`k00000777`) so SQLite's lexicographic key order
//!     and E4's entity-id order are the same order.
//!
//! LEAN: every case is capped. Default 20k rows finishes quickly; `--scale`
//! adds a second size (100k) so an O(N) defect shows as an EXPONENT rather
//! than just a bigger number.
//!
//!     cargo run --release --features compact-cells,sqlite-balance,\
//!         keyspace-append,slotref-split --bin two_ways -- [rows] [--scale] [--only substr]

use sekejap_core::{
    collections::{
        BfsRequest, CandidateDriver, CollectionId, CollectionOptions, Database, Direction,
        EdgeTypeId, EntityId, GraphContextId, IndexId, NeighborRequest, PointFilter, Projection,
        QueryBudget, QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue, ScoreExpr,
        SortDirection, TextMatch, VectorMetric,
    },
    spatial_math::Point,
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use rusqlite::Connection;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    ops::Bound,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

type R<T> = Result<T, Box<dyn std::error::Error>>;
/// Every E4 case answers with the entity sequences it found, in result order.
type Rows = Result<Vec<u64>, String>;

const CATS: [&str; 8] = [
    "cafe", "bar", "gym", "clinic", "school", "museum", "park", "hotel",
];
const AREAS: [&str; 6] = ["north", "south", "east", "west", "central", "riverside"];
const WORDS: [&str; 10] = [
    "railway",
    "signal",
    "platform",
    "junction",
    "siding",
    "tunnel",
    "viaduct",
    "depot",
    "carriage",
    "timetable",
];

/// Rows per commit, both arms. 256 is the prototype's own load batch.
const BATCH: u64 = 256;
/// Cache budget, both arms. E4 refuses less; SQLite is given the same.
const CACHE_BYTES: usize = 8 << 20;
/// E4's maximum page. A complete answer is assembled from repeated pages and
/// that assembly is part of what the case costs.
const PAGE: usize = 8192;
/// The seed node for every graph case, as in e3 (`k777`).
const SEED: u64 = 777;
/// Hard ceiling per case. Nothing may take longer; a case that would is the
/// finding, not an excuse to wait.
const CASE_BUDGET: Duration = Duration::from_secs(20);
/// A timed loop runs at least this many times AND at least this long.
const MIN_SAMPLES: usize = 20;
const MIN_TIME: Duration = Duration::from_millis(200);

fn word(i: u64) -> &'static str {
    WORDS[(i % 10) as usize]
}
fn key(i: u64) -> String {
    format!("k{i:08}")
}
/// `rating` and `price` are built by integer division rather than repeated
/// addition of 0.1 so that both arms hold the SAME double and an equality
/// predicate on 2.5 can match. (e3 formatted sekejap's value to one decimal
/// and handed SQLite `1.0 + n*0.1`; those are different numbers.)
fn rating(i: u64) -> f64 {
    (10 + i % 40) as f64 / 10.0
}
fn price(i: u64) -> f64 {
    10.0 + (i % 49) as f64 * 10.0
}
fn note(i: u64) -> String {
    format!("{} {} number {}", word(i), word(i / 7), i)
}
fn longitude(i: u64) -> f64 {
    (i % 360) as f64 * 0.01
}
fn latitude(i: u64) -> f64 {
    (i % 170) as f64 * 0.01
}
fn embedding(i: u64) -> [f32; 3] {
    [
        (i % 100) as f32 / 100.0,
        (i % 71) as f32 / 71.0,
        (i % 37) as f32 / 37.0,
    ]
}
/// e3's edge rule, unchanged: three quarters of the nodes carry one outgoing
/// `near` edge to a multiplicatively hashed destination.
fn edge_destination(i: u64, rows: u64) -> Option<u64> {
    if i % 4 == 0 {
        return None;
    }
    let d = 1 + (i.wrapping_mul(2_654_435_761) % rows);
    (d != i).then_some(d)
}

// ── the E4 arm's handles ──────────────────────────────────────────────────

struct Ctx {
    db: Database,
    v: CollectionId,
    cat: IndexId,
    area: IndexId,
    rating: IndexId,
    price: IndexId,
    note: IndexId,
    loc: IndexId,
    emb: IndexId,
    near: EdgeTypeId,
    rows: u64,
}

impl Ctx {
    fn entity(&self, i: u64) -> EntityId {
        EntityId {
            collection: self.v,
            sequence: i,
        }
    }
}

fn run_query(
    c: &Ctx,
    filters: &[QueryFilter<'_>],
    order: QueryOrder<'_>,
    projection: Projection<'_>,
    total_limit: Option<usize>,
    driver: CandidateDriver,
) -> Rows {
    let mut prepared = c
        .db
        .prepare_query(QueryRequest {
            collection: c.v,
            filters,
            order,
            projection,
            total_limit,
            driver,
        })
        .map_err(|e| e.to_string())?;
    let mut ids = Vec::new();
    loop {
        let page = prepared
            .next_page(PAGE, QueryBudget::unlimited(), || false)
            .map_err(|e| e.to_string())?;
        for row in &page.rows {
            for (_, value) in &row.projected {
                std::hint::black_box(value);
            }
            ids.push(row.id.sequence);
        }
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    Ok(ids)
}

fn ids(c: &Ctx, filters: &[QueryFilter<'_>], order: QueryOrder<'_>, limit: Option<usize>) -> Rows {
    run_query(c, filters, order, Projection::Ids, limit, CandidateDriver::Auto)
}

fn eq_text(index: IndexId, value: &str) -> QueryFilter<'_> {
    QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Eq(ScalarValue::Text(value)),
    }
}

fn eq_real(index: IndexId, value: f64) -> QueryFilter<'static> {
    QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Eq(ScalarValue::F64(value)),
    }
}

fn real_range(index: IndexId, lower: Bound<f64>, upper: Bound<f64>) -> QueryFilter<'static> {
    let map = |bound: Bound<f64>| match bound {
        Bound::Included(value) => Bound::Included(ScalarValue::F64(value)),
        Bound::Excluded(value) => Bound::Excluded(ScalarValue::F64(value)),
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

fn text_range<'a>(index: IndexId, lower: Bound<&'a str>, upper: Bound<&'a str>) -> QueryFilter<'a> {
    let map = |bound: Bound<&'a str>| match bound {
        Bound::Included(value) => Bound::Included(ScalarValue::Text(value)),
        Bound::Excluded(value) => Bound::Excluded(ScalarValue::Text(value)),
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

fn text_filter(index: IndexId, query: &str, matching: TextMatch) -> QueryFilter<'_> {
    QueryFilter::Text {
        index,
        query,
        matching,
    }
}

fn order_by(index: IndexId, ascending: bool) -> QueryOrder<'static> {
    QueryOrder::Scalar {
        index,
        direction: if ascending {
            SortDirection::Ascending
        } else {
            SortDirection::Descending
        },
    }
}

/// A BFS whose bounds are set from the graph's size, so nothing is silently
/// cut: E4's traversal is complete-or-error.
fn bfs(c: &Ctx, seed: u64, max_depth: usize) -> BfsRequest<'static> {
    let ceiling = c.rows as usize + 1;
    BfsRequest {
        seed: c.entity(seed),
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(c.near),
        min_depth: 1,
        max_depth,
        include_seed: false,
        max_visited: ceiling,
        max_edges: ceiling,
        result_limit: ceiling,
        edge_where: &[],
        node_where: &[],
    }
}

fn hops(c: &Ctx, depth: usize) -> Rows {
    let mut found = c
        .db
        .traverse_bfs(bfs(c, SEED, depth))
        .map_err(|e| e.to_string())?
        .nodes
        .iter()
        .map(|node| node.entity.sequence)
        .collect::<Vec<_>>();
    found.sort_unstable();
    Ok(found)
}

// ── the case list ─────────────────────────────────────────────────────────

enum Arm {
    /// The E4 API can ask this question.
    Run(fn(&Ctx) -> Rows),
    /// It cannot, and this is why.
    Unsupported(&'static str),
}

struct Case {
    family: &'static str,
    name: &'static str,
    e4: Arm,
    /// SQLite's spelling. The FIRST projected column is always `k`, so the two
    /// arms can be compared key for key.
    lite: Option<&'static str>,
    /// Compare the key SEQUENCE, not just the set. Set for every case whose
    /// answer has a defined order or a limit.
    ordered: bool,
}

fn case(
    family: &'static str,
    name: &'static str,
    e4: Arm,
    lite: Option<&'static str>,
    ordered: bool,
) -> Case {
    Case {
        family,
        name,
        e4,
        lite,
        ordered,
    }
}

fn cases() -> Vec<Case> {
    let run = |family, name, f: fn(&Ctx) -> Rows, lite, ordered| {
        case(family, name, Arm::Run(f), lite, ordered)
    };
    let no = |family, name, why, lite| case(family, name, Arm::Unsupported(why), lite, false);

    const NO_AGGREGATE: &str =
        "no aggregate API: QueryRequest has no grouping and Projection has no COUNT/MIN/MAX/SUM/AVG; \
         counting rows in the harness would measure a full materialisation, not an aggregate";
    const NO_OFFSET: &str = "QueryRequest has no offset/skip; taking limit+offset rows and \
                             discarding in the harness would not be the engine's pagination";
    const NO_DISJUNCTION: &str = "filters are conjunctive only: QueryFilter has no OR, IN or NOT";
    const NO_UNORDERED_SORT: &str =
        "QueryOrder::Scalar needs a ready scalar index; there is no sort over an unindexed field";

    vec![
    // ── filter: predicate shape and selectivity ────────────────────────────
    run("filter", "eq_indexed_one", |c| {
        // A point read by key, which is what SQLite's TEXT PRIMARY KEY does.
        Ok(c.db.get(c.v, &key(SEED)).map_err(|e| e.to_string())?
            .map(|entity| entity.id.sequence).into_iter().collect())
    }, Some("SELECT k FROM v WHERE k='k00000777'"), true),
    run("filter", "eq_indexed_many", |c| ids(c, &[eq_text(c.cat, "cafe")], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE cat='cafe'"), false),
    run("filter", "eq_no_match", |c| ids(c, &[eq_text(c.cat, "nowhere")], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE cat='nowhere'"), false),
    no("filter", "eq_unindexed",
        "JsonEq refuses a declared non-Json field, and `note` must be declared Text to carry the \
         text index; there is no equality predicate for a declared column with no index",
        Some("SELECT k FROM v WHERE note='no such note'")),
    run("filter", "range_closed", |c| ids(c,
        &[real_range(c.price, Bound::Excluded(100.0), Bound::Included(300.0))], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE price>100 AND price<=300"), false),
    run("filter", "range_two_sided", |c| ids(c,
        &[real_range(c.price, Bound::Included(490.0), Bound::Excluded(500.0))], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE price>=490 AND price<500"), false),
    run("filter", "range_open", |c| ids(c,
        &[real_range(c.price, Bound::Excluded(400.0), Bound::Unbounded)], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE price>400"), false),
    run("filter", "and_both_indexed", |c| ids(c,
        &[eq_text(c.cat, "cafe"), eq_text(c.area, "north")], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE cat='cafe' AND area='north'"), false),
    run("filter", "and_half_indexed", |c| ids(c,
        &[eq_text(c.cat, "cafe"), real_range(c.rating, Bound::Excluded(3.0), Bound::Unbounded)],
        QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE cat='cafe' AND rating>3.0"), false),
    no("filter", "or_both_indexed", NO_DISJUNCTION,
        Some("SELECT k FROM v WHERE cat='cafe' OR area='north'")),
    no("filter", "in_list", NO_DISJUNCTION,
        Some("SELECT k FROM v WHERE cat IN ('cafe','bar','gym')")),
    no("filter", "not_in", NO_DISJUNCTION,
        Some("SELECT k FROM v WHERE cat NOT IN ('cafe','bar')")),
    no("filter", "neq", NO_DISJUNCTION, Some("SELECT k FROM v WHERE cat<>'cafe'")),
    run("filter", "between", |c| ids(c,
        &[real_range(c.price, Bound::Included(100.0), Bound::Included(300.0))], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE price BETWEEN 100 AND 300"), false),
    run("filter", "text_gt", |c| ids(c,
        &[text_range(c.cat, Bound::Excluded("g"), Bound::Unbounded)], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE cat>'g'"), false),
    run("filter", "real_eq", |c| ids(c, &[eq_real(c.rating, 2.5)], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE rating=2.5"), false),
    run("filter", "two_ranges", |c| ids(c,
        &[real_range(c.price, Bound::Excluded(100.0), Bound::Unbounded),
          real_range(c.rating, Bound::Unbounded, Bound::Excluded(3.0))], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE price>100 AND rating<3.0"), false),
    run("filter", "three_and", |c| ids(c,
        &[eq_text(c.cat, "cafe"), eq_text(c.area, "north"),
          real_range(c.rating, Bound::Excluded(2.0), Bound::Unbounded)], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE cat='cafe' AND area='north' AND rating>2.0"), false),
    run("filter", "selective_1pct", |c| ids(c,
        &[real_range(c.price, Bound::Excluded(570.0), Bound::Unbounded)], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE price>570"), false),
    run("filter", "selective_half", |c| ids(c,
        &[real_range(c.price, Bound::Excluded(250.0), Bound::Unbounded)], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE price>250"), false),

    // ── project: the per-row materialisation cost ──────────────────────────
    run("project", "key_only", |c| ids(c, &[eq_text(c.cat, "cafe")], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE cat='cafe'"), false),
    run("project", "one_column", |c| run_query(c, &[eq_text(c.cat, "cafe")], QueryOrder::EntityId,
        Projection::Fields(&["rating"]), None, CandidateDriver::Auto),
        Some("SELECT k, rating FROM v WHERE cat='cafe'"), false),
    run("project", "five_columns", |c| run_query(c, &[eq_text(c.cat, "cafe")], QueryOrder::EntityId,
        Projection::Fields(&["cat", "area", "rating", "price"]), None, CandidateDriver::Auto),
        Some("SELECT k, cat, area, rating, price FROM v WHERE cat='cafe'"), false),
    run("project", "star", |c| run_query(c, &[eq_text(c.cat, "cafe")], QueryOrder::EntityId,
        Projection::Fields(&["cat", "area", "rating", "price", "note"]), None, CandidateDriver::Auto),
        Some("SELECT k, cat, area, rating, price, note FROM v WHERE cat='cafe'"), false),

    // ── order ──────────────────────────────────────────────────────────────
    run("order", "indexed_asc", |c| ids(c, &[], order_by(c.rating, true), None),
        Some("SELECT k FROM v ORDER BY rating ASC, k ASC"), true),
    run("order", "indexed_desc", |c| ids(c, &[], order_by(c.rating, false), None),
        Some("SELECT k FROM v ORDER BY rating DESC, k ASC"), true),
    no("order", "unindexed", NO_UNORDERED_SORT, Some("SELECT k FROM v ORDER BY note ASC, k ASC")),
    run("order", "after_filter", |c| ids(c, &[eq_text(c.cat, "cafe")], order_by(c.rating, false), None),
        Some("SELECT k FROM v WHERE cat='cafe' ORDER BY rating DESC, k ASC"), true),
    no("order", "two_columns", "QueryOrder carries one key; there is no compound sort",
        Some("SELECT k FROM v ORDER BY cat ASC, rating DESC, k ASC")),
    run("order", "by_key", |c| ids(c, &[], QueryOrder::EntityId, None),
        Some("SELECT k FROM v ORDER BY k ASC"), true),
    run("order", "filtered_limit10", |c| ids(c, &[eq_text(c.area, "north")], order_by(c.price, true), Some(10)),
        Some("SELECT k FROM v WHERE area='north' ORDER BY price ASC, k ASC LIMIT 10"), true),

    // ── limit: pushdown ────────────────────────────────────────────────────
    run("limit", "one_no_order", |c| ids(c, &[eq_text(c.cat, "cafe")], QueryOrder::EntityId, Some(1)),
        Some("SELECT k FROM v WHERE cat='cafe' ORDER BY k ASC LIMIT 1"), true),
    run("limit", "one_with_order", |c| ids(c, &[], order_by(c.price, true), Some(1)),
        Some("SELECT k FROM v ORDER BY price ASC, k ASC LIMIT 1"), true),
    run("limit", "fifty", |c| ids(c, &[], order_by(c.rating, false), Some(50)),
        Some("SELECT k FROM v ORDER BY rating DESC, k ASC LIMIT 50"), true),
    no("limit", "with_offset", NO_OFFSET,
        Some("SELECT k FROM v ORDER BY rating DESC, k ASC LIMIT 50 OFFSET 500")),
    no("limit", "offset_deep", NO_OFFSET,
        Some("SELECT k FROM v ORDER BY rating DESC, k ASC LIMIT 10 OFFSET 5000")),
    run("limit", "bigger_than_all", |c| ids(c, &[], QueryOrder::EntityId, Some(999_999)),
        Some("SELECT k FROM v ORDER BY k ASC LIMIT 999999"), true),
    run("limit", "ten_no_order", |c| ids(c, &[], QueryOrder::EntityId, Some(10)),
        Some("SELECT k FROM v ORDER BY k ASC LIMIT 10"), true),

    // ── aggregate: E4 has no aggregate API at all ──────────────────────────
    no("aggregate", "count_all", NO_AGGREGATE, Some("SELECT count(*) FROM v")),
    no("aggregate", "count_filtered", NO_AGGREGATE, Some("SELECT count(*) FROM v WHERE cat='cafe'")),
    no("aggregate", "min_indexed", NO_AGGREGATE, Some("SELECT min(price) FROM v")),
    no("aggregate", "max_indexed", NO_AGGREGATE, Some("SELECT max(price) FROM v")),
    no("aggregate", "sum", NO_AGGREGATE, Some("SELECT sum(price) FROM v")),
    no("aggregate", "avg", NO_AGGREGATE, Some("SELECT avg(rating) FROM v")),
    no("aggregate", "group_low_card", NO_AGGREGATE, Some("SELECT cat,count(*) FROM v GROUP BY cat")),
    no("aggregate", "group_having", NO_AGGREGATE,
        Some("SELECT cat,count(*) FROM v GROUP BY cat HAVING count(*)>100")),
    no("aggregate", "group_high_card", NO_AGGREGATE, Some("SELECT price,count(*) FROM v GROUP BY price")),
    no("aggregate", "group_two_aggs", NO_AGGREGATE,
        Some("SELECT cat,count(*),avg(rating) FROM v GROUP BY cat")),
    no("aggregate", "distinct", NO_AGGREGATE, Some("SELECT DISTINCT cat FROM v")),
    no("aggregate", "count_distinct", NO_AGGREGATE, Some("SELECT count(DISTINCT cat) FROM v")),
    no("aggregate", "min_filtered", NO_AGGREGATE, Some("SELECT min(price) FROM v WHERE cat='cafe'")),
    no("aggregate", "min_unindexed", NO_AGGREGATE, Some("SELECT min(note) FROM v")),

    // ── text ───────────────────────────────────────────────────────────────
    // E4's text index is token based, so a token query is compared against
    // FTS5 and a LIKE is not expressible at all.
    run("text", "bm25_one_term", |c| run_query(c, &[text_filter(c.note, "junction", TextMatch::Any)],
        QueryOrder::Bm25 { index: c.note, query: "junction", matching: TextMatch::Any },
        Projection::Ids, None, CandidateDriver::Auto),
        Some("SELECT v.k FROM v JOIN v_fts ON v_fts.rowid=v.rowid WHERE v_fts MATCH 'junction'"), false),
    run("text", "bm25_two_terms", |c| run_query(c,
        &[text_filter(c.note, "railway junction", TextMatch::Any)],
        QueryOrder::Bm25 { index: c.note, query: "railway junction", matching: TextMatch::Any },
        Projection::Ids, None, CandidateDriver::Auto),
        Some("SELECT v.k FROM v JOIN v_fts ON v_fts.rowid=v.rowid WHERE v_fts MATCH 'railway OR junction'"), false),
    run("text", "bm25_rare", |c| run_query(c, &[text_filter(c.note, "timetable", TextMatch::Any)],
        QueryOrder::Bm25 { index: c.note, query: "timetable", matching: TextMatch::Any },
        Projection::Ids, None, CandidateDriver::Auto),
        Some("SELECT v.k FROM v JOIN v_fts ON v_fts.rowid=v.rowid WHERE v_fts MATCH 'timetable'"), false),
    run("text", "bm25_common", |c| run_query(c, &[text_filter(c.note, "number", TextMatch::Any)],
        QueryOrder::Bm25 { index: c.note, query: "number", matching: TextMatch::Any },
        Projection::Ids, None, CandidateDriver::Auto),
        Some("SELECT v.k FROM v JOIN v_fts ON v_fts.rowid=v.rowid WHERE v_fts MATCH 'number'"), false),
    run("text", "match_all", |c| ids(c,
        &[text_filter(c.note, "railway junction", TextMatch::All)], QueryOrder::EntityId, None),
        Some("SELECT v.k FROM v JOIN v_fts ON v_fts.rowid=v.rowid WHERE v_fts MATCH 'railway AND junction'"), false),
    run("text", "match_phrase", |c| ids(c,
        &[text_filter(c.note, "railway signal", TextMatch::Phrase)], QueryOrder::EntityId, None),
        Some("SELECT v.k FROM v JOIN v_fts ON v_fts.rowid=v.rowid WHERE v_fts MATCH '\"railway signal\"'"), false),
    no("text", "like_prefix", "no LIKE or substring predicate; the text index matches tokens",
        Some("SELECT k FROM v WHERE note LIKE 'railway%'")),
    no("text", "like_substring", "no LIKE or substring predicate; the text index matches tokens",
        Some("SELECT k FROM v WHERE note LIKE '%junction%'")),
    no("text", "like_suffix", "no LIKE or substring predicate; the text index matches tokens",
        Some("SELECT k FROM v WHERE note LIKE '%7'")),
    no("text", "like_two_terms", "no LIKE or substring predicate; the text index matches tokens",
        Some("SELECT k FROM v WHERE note LIKE '%railway%' AND note LIKE '%signal%'")),
    no("text", "search_typo", "no fuzzy or typo-tolerant search index", None),

    // ── graph ──────────────────────────────────────────────────────────────
    run("graph", "hop1", |c| {
        let mut found = c.db.neighbors(NeighborRequest {
            entity: c.entity(SEED), direction: Direction::Outgoing,
            context: GraphContextId::BASE, edge_type: Some(c.near), limit: 256,
        }).map_err(|e| e.to_string())?
            .iter().map(|edge| edge.key.destination.sequence).collect::<Vec<_>>();
        found.sort_unstable();
        Ok(found)
    }, Some("SELECT dst FROM e WHERE src='k00000777'"), false),
    run("graph", "hop1_in", |c| {
        let mut found = c.db.neighbors(NeighborRequest {
            entity: c.entity(SEED), direction: Direction::Incoming,
            context: GraphContextId::BASE, edge_type: Some(c.near), limit: 256,
        }).map_err(|e| e.to_string())?
            .iter().map(|edge| edge.key.source.sequence).collect::<Vec<_>>();
        found.sort_unstable();
        Ok(found)
    }, Some("SELECT src FROM e WHERE dst='k00000777'"), false),
    run("graph", "hop2", |c| hops(c, 2),
        Some("WITH RECURSIVE t(id,d) AS (SELECT 'k00000777',0 UNION SELECT e.dst,t.d+1 \
              FROM e JOIN t ON e.src=t.id WHERE t.d<2) SELECT DISTINCT id FROM t WHERE id<>'k00000777'"), false),
    run("graph", "hop3", |c| hops(c, 3),
        Some("WITH RECURSIVE t(id,d) AS (SELECT 'k00000777',0 UNION SELECT e.dst,t.d+1 \
              FROM e JOIN t ON e.src=t.id WHERE t.d<3) SELECT DISTINCT id FROM t WHERE id<>'k00000777'"), false),
    run("graph", "hop5", |c| hops(c, 5),
        Some("WITH RECURSIVE t(id,d) AS (SELECT 'k00000777',0 UNION SELECT e.dst,t.d+1 \
              FROM e JOIN t ON e.src=t.id WHERE t.d<5) SELECT DISTINCT id FROM t WHERE id<>'k00000777'"), false),
    run("graph", "hop2_dest_filter", |c| ids(c,
        &[QueryFilter::Graph(bfs(c, SEED, 2)), eq_text(c.cat, "cafe")], QueryOrder::EntityId, None),
        Some("WITH RECURSIVE t(id,d) AS (SELECT 'k00000777',0 UNION SELECT e.dst,t.d+1 FROM e \
              JOIN t ON e.src=t.id WHERE t.d<2) SELECT DISTINCT t.id FROM t JOIN v ON v.k=t.id \
              WHERE t.id<>'k00000777' AND v.cat='cafe'"), false),
    run("graph", "hop1_project", |c| run_query(c, &[QueryFilter::Graph(bfs(c, SEED, 1))],
        QueryOrder::EntityId, Projection::Fields(&["rating"]), None, CandidateDriver::Auto),
        Some("SELECT e.dst, v.rating FROM e JOIN v ON v.k=e.dst WHERE e.src='k00000777'"), false),

    // ── win: where a multi-model engine should be untouchable ──────────────
    // SQLite has no vector or spatial index; giving it one would not be the
    // same index set, so these run with no reference arm.
    run("win", "vector_k1", |c| ids(c, &[],
        QueryOrder::ExactVector { index: c.emb, query: &[0.5, 0.5, 0.5], metric: VectorMetric::SquaredL2 },
        Some(1)), None, true),
    run("win", "vector_knn", |c| ids(c, &[],
        QueryOrder::ExactVector { index: c.emb, query: &[0.5, 0.5, 0.5], metric: VectorMetric::SquaredL2 },
        Some(10)), None, true),
    run("win", "vector_k100", |c| ids(c, &[],
        QueryOrder::ExactVector { index: c.emb, query: &[0.5, 0.5, 0.5], metric: VectorMetric::SquaredL2 },
        Some(100)), None, true),
    run("win", "spatial_radius", |c| ids(c, &[QueryFilter::Point {
        index: c.loc,
        predicate: PointFilter::Radius {
            center: Point::new(1.0, 0.5).expect("benchmark centre is a valid point"),
            radius_metres: 50_000.0,
        },
    }], QueryOrder::EntityId, None), None, false),
    run("win", "spatial_tiny", |c| ids(c, &[QueryFilter::Point {
        index: c.loc,
        predicate: PointFilter::Radius {
            center: Point::new(1.0, 0.5).expect("benchmark centre is a valid point"),
            radius_metres: 2_000.0,
        },
    }], QueryOrder::EntityId, None), None, false),
    run("win", "spatial_bbox", |c| ids(c, &[QueryFilter::Point {
        index: c.loc,
        predicate: PointFilter::Bbox(
            sekejap_core::spatial_math::Bounds::new(0.5, 2.0, 0.2, 1.2)
                .expect("benchmark bounds are valid"),
        ),
    }], QueryOrder::EntityId, None), None, false),
    no("win", "spatial_poly", "PointFilter is bbox or radius only; there is no polygon predicate", None),
    run("win", "text_then_vector", |c| run_query(c,
        &[text_filter(c.note, "railway", TextMatch::Any)],
        QueryOrder::ExactVector { index: c.emb, query: &[0.5, 0.5, 0.5], metric: VectorMetric::SquaredL2 },
        Projection::Ids, Some(50), CandidateDriver::Filter(0)), None, true),
    run("win", "hybrid", |c| {
        let query = [0.5f32, 0.5, 0.5];
        let half = ScoreExpr::Lit(0.5);
        let bm25 = ScoreExpr::Bm25 {
            index: c.note,
            query: "railway",
            matching: TextMatch::Any,
        };
        let vec = ScoreExpr::VectorSimilarity {
            index: c.emb,
            query: &query,
            metric: VectorMetric::SquaredL2,
        };
        let text_term = ScoreExpr::Mul(&half, &bm25);
        let vec_term = ScoreExpr::Mul(&half, &vec);
        let expr = ScoreExpr::Add(&text_term, &vec_term);
        ids(
            c,
            &[],
            QueryOrder::Score {
                expr: &expr,
                direction: SortDirection::Descending,
            },
            Some(10),
        )
    }, None, true),

    // ── mixed: the shapes real applications write ──────────────────────────
    run("mixed", "filter_order_limit", |c| ids(c, &[eq_text(c.cat, "cafe")], order_by(c.rating, false), Some(10)),
        Some("SELECT k FROM v WHERE cat='cafe' ORDER BY rating DESC, k ASC LIMIT 10"), true),
    run("mixed", "paginate_page1", |c| ids(c, &[eq_text(c.cat, "cafe")], order_by(c.rating, false), Some(20)),
        Some("SELECT k FROM v WHERE cat='cafe' ORDER BY rating DESC, k ASC LIMIT 20"), true),
    no("mixed", "paginate_page9", NO_OFFSET,
        Some("SELECT k FROM v WHERE cat='cafe' ORDER BY rating DESC, k ASC LIMIT 20 OFFSET 180")),
    run("mixed", "filter_then_text", |c| ids(c,
        &[eq_text(c.cat, "cafe"), text_filter(c.note, "junction", TextMatch::Any)], QueryOrder::EntityId, None),
        Some("SELECT k FROM v WHERE cat='cafe' AND rowid IN \
              (SELECT rowid FROM v_fts WHERE v_fts MATCH 'junction')"), false),
    no("mixed", "filter_group", NO_AGGREGATE,
        Some("SELECT area,count(*) FROM v WHERE rating>2.0 GROUP BY area")),
    no("mixed", "count_of_filtered_join", NO_AGGREGATE,
        Some("SELECT count(*) FROM v WHERE area='north' AND rating>2.0")),

    // ── scan: the floor, no predicate at all ───────────────────────────────
    run("scan", "full_keys", |c| run_query(c, &[], QueryOrder::EntityId, Projection::Ids, None,
        CandidateDriver::Entities), Some("SELECT k FROM v"), false),
    run("scan", "full_one_col", |c| run_query(c, &[], QueryOrder::EntityId,
        Projection::Fields(&["rating"]), None, CandidateDriver::Entities),
        Some("SELECT k, rating FROM v"), false),
    run("scan", "full_star", |c| run_query(c, &[], QueryOrder::EntityId,
        Projection::Fields(&["cat", "area", "rating", "price", "note"]), None, CandidateDriver::Entities),
        Some("SELECT k, cat, area, rating, price, note FROM v"), false),
    ]
}

// ── measurement ───────────────────────────────────────────────────────────

/// Warm once untimed, then time until BOTH a sample floor and a time floor are
/// met, and report the MEDIAN. The median is reported rather than the mean
/// because one page fault in one iteration should not become the case's
/// number.
fn bench(mut f: impl FnMut() -> usize) -> (f64, usize) {
    std::hint::black_box(f()); // warm, untimed
    let mut samples = Vec::with_capacity(MIN_SAMPLES);
    let start = Instant::now();
    let mut rows;
    loop {
        let at = Instant::now();
        rows = f();
        samples.push(at.elapsed().as_secs_f64() * 1e6);
        let elapsed = start.elapsed();
        if elapsed >= CASE_BUDGET {
            break;
        }
        if samples.len() >= MIN_SAMPLES && elapsed >= MIN_TIME {
            break;
        }
    }
    samples.sort_by(f64::total_cmp);
    (samples[samples.len() / 2], rows)
}

/// SQLite's arm. Every projected column is materialised, not just the first,
/// so a projection case measures projection on both sides. The first column is
/// always the key.
fn lite_keys(connection: &Connection, sql: &str) -> Vec<String> {
    let mut statement = connection.prepare_cached(sql).expect("benchmark SQL prepares");
    let columns = statement.column_count();
    let mut rows = statement.query([]).expect("benchmark SQL runs");
    let mut out = Vec::new();
    while let Some(row) = rows.next().expect("benchmark SQL steps") {
        for column in 1..columns {
            std::hint::black_box(
                row.get::<_, rusqlite::types::Value>(column)
                    .expect("column reads"),
            );
        }
        out.push(row.get::<_, String>(0).expect("key column reads"));
    }
    out
}

struct Res {
    family: &'static str,
    name: &'static str,
    e4: Option<f64>,
    lite: Option<f64>,
    e4_rows: Option<usize>,
    disagree: Vec<String>,
    unsupported: Option<&'static str>,
    lite_only: bool,
}

impl Res {
    fn ratio(&self) -> Option<f64> {
        match (self.e4, self.lite) {
            (Some(e4), Some(lite)) => Some(e4 / lite.max(1e-9)),
            _ => None,
        }
    }
}

struct Report {
    rows: u64,
    load_e4: f64,
    load_lite: f64,
    bytes_e4: (u64, u64),
    bytes_lite: (u64, u64),
    edges: u64,
    results: Vec<Res>,
}

// ── the load ──────────────────────────────────────────────────────────────

fn tree_bytes(root: &Path) -> u64 {
    fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.metadata().ok())
                .map(|meta| if meta.is_file() { meta.len() } else { 0 })
                .sum()
        })
        .unwrap_or(0)
}

fn load_e4(root: &Path, rows: u64) -> R<(Ctx, f64, u64)> {
    // `Database::create` makes the directory itself and refuses an existing one.
    fs::create_dir_all(root.parent().unwrap_or(root))?;
    let start = Instant::now();
    let mut db = Database::create(
        root,
        Config {
            budget_bytes: CACHE_BYTES,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )?;
    let v = db.create_collection(
        "v",
        vec![
            ("cat".into(), Kind::Text),
            ("area".into(), Kind::Text),
            ("rating".into(), Kind::Real),
            ("price".into(), Kind::Real),
            ("note".into(), Kind::Text),
            ("loc".into(), Kind::Point),
            ("emb".into(), Kind::Vector(3)),
        ],
        CollectionOptions::default(),
    )?;
    db.enable_graph()?;
    let near = db.create_edge_type("near")?;
    db.commit()?;

    for i in 1..=rows {
        let id = db.put(
            v,
            &key(i),
            &json!({
                "cat": CATS[(i % 8) as usize],
                "area": AREAS[(i % 6) as usize],
                "rating": rating(i),
                "price": price(i),
                "note": note(i),
                "loc": {"type": "Point", "coordinates": [longitude(i), latitude(i)]},
                "emb": embedding(i),
            }),
        )?;
        // The whole comparison rests on key `k{i:08}` being entity sequence i.
        assert_eq!(id.sequence, i, "entity sequence must follow insertion order");
        if i % BATCH == 0 {
            db.commit()?;
        }
    }
    db.commit()?;

    let mut edges = 0u64;
    for i in 1..=rows {
        if let Some(destination) = edge_destination(i, rows) {
            db.put_edge(
                GraphContextId::BASE,
                EntityId {
                    collection: v,
                    sequence: i,
                },
                near,
                EntityId {
                    collection: v,
                    sequence: destination,
                },
                &json!({}),
            )?;
            edges += 1;
            if edges % BATCH == 0 {
                db.commit()?;
            }
        }
    }
    db.commit()?;

    let cat = db.create_scalar_index(v, "cat_idx", "cat", false)?;
    db.build_index_to_ready(cat, BATCH as usize)?;
    let area = db.create_scalar_index(v, "area_idx", "area", false)?;
    db.build_index_to_ready(area, BATCH as usize)?;
    let rating = db.create_scalar_index(v, "rating_idx", "rating", false)?;
    db.build_index_to_ready(rating, BATCH as usize)?;
    let price = db.create_scalar_index(v, "price_idx", "price", false)?;
    db.build_index_to_ready(price, BATCH as usize)?;
    let note = db.create_text_index(v, "note_text", "note")?;
    db.build_index_to_ready(note, BATCH as usize)?;
    let loc = db.create_point_index(v, "loc_point", "loc")?;
    db.build_index_to_ready(loc, BATCH as usize)?;
    let emb = db.create_exact_vector_index(v, "emb_exact", "emb")?;
    db.build_index_to_ready(emb, BATCH as usize)?;
    db.commit()?;
    db.checkpoint()?;
    let seconds = start.elapsed().as_secs_f64();

    Ok((
        Ctx {
            db,
            v,
            cat,
            area,
            rating,
            price,
            note,
            loc,
            emb,
            near,
            rows,
        },
        seconds,
        edges,
    ))
}

fn load_lite(root: &Path, rows: u64) -> R<(Connection, f64)> {
    fs::create_dir_all(root)?;
    let start = Instant::now();
    let connection = Connection::open(root.join("s.db"))?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON;
         PRAGMA cache_size=-8192; PRAGMA temp_store=FILE;
         CREATE TABLE v(k TEXT PRIMARY KEY, cat TEXT, area TEXT, rating REAL, price REAL, note TEXT);
         CREATE VIRTUAL TABLE v_fts USING fts5(note, content='v', content_rowid='rowid');
         CREATE TABLE e(src TEXT, dst TEXT, PRIMARY KEY(src,dst)) WITHOUT ROWID;",
    )?;
    connection.execute_batch("BEGIN")?;
    {
        let mut insert = connection.prepare("INSERT INTO v VALUES (?,?,?,?,?,?)")?;
        let mut fts = connection.prepare("INSERT INTO v_fts(rowid,note) VALUES (?,?)")?;
        for i in 1..=rows {
            let text = note(i);
            insert.execute(rusqlite::params![
                key(i),
                CATS[(i % 8) as usize],
                AREAS[(i % 6) as usize],
                rating(i),
                price(i),
                &text
            ])?;
            fts.execute(rusqlite::params![connection.last_insert_rowid(), &text])?;
            if i % BATCH == 0 {
                connection.execute_batch("COMMIT; BEGIN")?;
            }
        }
    }
    connection.execute_batch("COMMIT; BEGIN")?;
    {
        let mut link = connection.prepare("INSERT OR IGNORE INTO e VALUES (?,?)")?;
        let mut edges = 0u64;
        for i in 1..=rows {
            if let Some(destination) = edge_destination(i, rows) {
                link.execute(rusqlite::params![key(i), key(destination)])?;
                edges += 1;
                if edges % BATCH == 0 {
                    connection.execute_batch("COMMIT; BEGIN")?;
                }
            }
        }
    }
    connection.execute_batch(
        "COMMIT;
         CREATE INDEX ic ON v(cat); CREATE INDEX ia ON v(area);
         CREATE INDEX ir ON v(rating); CREATE INDEX ip ON v(price);
         CREATE INDEX e_dst ON e(dst);
         PRAGMA wal_checkpoint(TRUNCATE);",
    )?;
    Ok((connection, start.elapsed().as_secs_f64()))
}

// ── one whole run at one size ─────────────────────────────────────────────

fn run(rows: u64, only: Option<&str>, root: &Path) -> R<Report> {
    let e4_root = root.join("e4");
    let lite_root = root.join("sqlite");
    eprint!("loading {rows} rows into E4 … ");
    let (ctx, load_e4_seconds, edges) = load_e4(&e4_root, rows)?;
    eprint!("{load_e4_seconds:.2}s; into SQLite … ");
    let (connection, load_lite_seconds) = load_lite(&lite_root, rows)?;
    eprintln!("{load_lite_seconds:.2}s");
    let bytes_e4 = (tree_bytes(&e4_root), 0);
    let bytes_lite = (tree_bytes(&lite_root), 0);

    // One full pass over BOTH engines before anything is timed, so the first
    // measured case does not absorb the first touch of the whole file.
    {
        let _ = run_query(
            &ctx,
            &[],
            QueryOrder::EntityId,
            Projection::Ids,
            None,
            CandidateDriver::Entities,
        )?;
        let _ = lite_keys(&connection, "SELECT k FROM v");
    }

    let all = cases();
    let selected = all
        .iter()
        .filter(|c| only.is_none_or(|s| c.name.contains(s) || c.family.contains(s)));

    let mut results = Vec::new();
    for c in selected {
        let unsupported = match &c.e4 {
            Arm::Unsupported(why) => Some(*why),
            Arm::Run(_) => None,
        };
        let Arm::Run(f) = c.e4 else {
            results.push(Res {
                family: c.family,
                name: c.name,
                e4: None,
                lite: None,
                e4_rows: None,
                disagree: Vec::new(),
                unsupported,
                lite_only: c.lite.is_some(),
            });
            continue;
        };

        // Verification pass, untimed: the two arms must answer the SAME
        // question before either is timed answering it.
        let e4_keys = f(&ctx)
            .map_err(|e| format!("{}/{}: {e}", c.family, c.name))?
            .into_iter()
            .map(key)
            .collect::<Vec<_>>();
        let mut disagree = Vec::new();
        if let Some(sql) = c.lite {
            let lite = lite_keys(&connection, sql);
            if lite.len() != e4_keys.len() {
                disagree.push(format!("rows E4/SQLITE {}/{}", e4_keys.len(), lite.len()));
            } else if c.ordered {
                if let Some(at) = (0..lite.len()).find(|i| lite[*i] != e4_keys[*i]) {
                    disagree.push(format!(
                        "key sequence at {at}: E4 {} vs SQLITE {}",
                        e4_keys[at], lite[at]
                    ));
                }
            } else {
                let mut a = e4_keys.clone();
                let mut b = lite.clone();
                a.sort();
                b.sort();
                if let Some(at) = (0..a.len()).find(|i| a[*i] != b[*i]) {
                    disagree.push(format!("key set at {at}: E4 {} vs SQLITE {}", a[at], b[at]));
                }
            }
        }

        let (e4_micros, e4_rows) = bench(|| f(&ctx).map(|v| v.len()).unwrap_or(usize::MAX));
        let lite_micros = c
            .lite
            .map(|sql| bench(|| lite_keys(&connection, sql).len()).0);

        results.push(Res {
            family: c.family,
            name: c.name,
            e4: Some(e4_micros),
            lite: lite_micros,
            e4_rows: Some(e4_rows),
            disagree,
            unsupported: None,
            lite_only: false,
        });
    }

    // Bytes on disk AFTER CLOSE: both handles are dropped first, so a WAL that
    // is only checkpointed on close is counted.
    drop(ctx);
    drop(connection);
    let bytes_e4 = (bytes_e4.0, tree_bytes(&e4_root));
    let bytes_lite = (bytes_lite.0, tree_bytes(&lite_root));

    Ok(Report {
        rows,
        load_e4: load_e4_seconds,
        load_lite: load_lite_seconds,
        bytes_e4,
        bytes_lite,
        edges,
        results,
    })
}

// ── reporting ─────────────────────────────────────────────────────────────

fn mib(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

fn print_report(report: &Report) {
    let Report {
        rows,
        load_e4,
        load_lite,
        bytes_e4,
        bytes_lite,
        edges,
        results,
    } = report;
    println!("\n══ LOAD ({rows} rows, {edges} edges) ══  matched shape, matched durability");
    println!(
        "{:<8} {:>10} {:>16} {:>16}",
        "arm", "load s", "bytes open", "bytes closed"
    );
    println!("{}", "-".repeat(54));
    println!(
        "{:<8} {load_e4:>10.3} {:>15.2}M {:>15.2}M",
        "E4",
        mib(bytes_e4.0),
        mib(bytes_e4.1)
    );
    println!(
        "{:<8} {load_lite:>10.3} {:>15.2}M {:>15.2}M",
        "SQLITE",
        mib(bytes_lite.0),
        mib(bytes_lite.1)
    );
    println!(
        "load ratio E4/SQLITE {:.2}x  ·  disk ratio {:.2}x",
        load_e4 / load_lite.max(1e-9),
        bytes_e4.1 as f64 / (bytes_lite.1.max(1) as f64)
    );
    println!(
        "index set: BOTH build ordered rating + ordered price + equality cat + equality area \
         + full text on note + the edge table."
    );
    println!(
        "           E4 ALSO builds a point index on loc and an exact vector index on emb; \
         SQLite has no equivalent, so its load is for a strictly smaller index set."
    );
    println!(
        "           E4 maintains the reverse edge index itself, so SQLite is given the \
         matching e(dst) index rather than left to scan."
    );
    println!(
        "durability: E4 page-WAL publishes every commit with a FULL barrier (F_FULLFSYNC on \
         macOS) and refuses any other mode;"
    );
    println!(
        "           SQLite runs WAL + synchronous=FULL + fullfsync=ON, the same barrier. \
         Both commit every {BATCH} rows. Both caches 8 MiB."
    );

    let mut ran: Vec<&Res> = results.iter().filter(|r| r.e4.is_some()).collect();
    ran.sort_by(|a, b| {
        b.ratio()
            .unwrap_or(-1.0)
            .total_cmp(&a.ratio().unwrap_or(-1.0))
    });

    println!("\n══ CASES ({rows} rows) ══  median µs per execution, worst-first\n");
    println!(
        "{:<10} {:<18} {:>12} {:>12} {:>11} {:>8}",
        "family", "case", "E4 µs", "SQLITE µs", "E4/SQLITE", "rows"
    );
    println!("{}", "-".repeat(78));
    let (mut worse, mut better) = (0, 0);
    let mut disagreements = 0;
    for r in &ran {
        let e4 = r.e4.map(|v| format!("{v:12.1}")).unwrap_or_default();
        let lite = r
            .lite
            .map(|v| format!("{v:12.1}"))
            .unwrap_or_else(|| format!("{:>12}", "-"));
        let ratio = match r.ratio() {
            Some(x) => {
                if x >= 1.0 {
                    worse += 1
                } else {
                    better += 1
                }
                if x >= 1000.0 {
                    format!("{x:9.0}x!!")
                } else if x >= 10.0 {
                    format!("{x:9.0}x!")
                } else if x < 1.0 {
                    format!("{x:9.2}x+")
                } else {
                    format!("{x:9.2}x ")
                }
            }
            None => format!("{:>11}", "-"),
        };
        let note = if r.disagree.is_empty() {
            String::new()
        } else {
            disagreements += 1;
            format!("  DISAGREE {}", r.disagree.join(", "))
        };
        println!(
            "{:<10} {:<18} {e4} {lite} {ratio} {:>8}{note}",
            r.family,
            r.name,
            r.e4_rows.unwrap_or(0)
        );
    }
    println!(
        "!! 1000x+   ! 10x+   + E4 is faster   ·  {worse} slower, {better} faster, {} \
         with no SQLite arm",
        ran.iter().filter(|r| r.lite.is_none()).count()
    );
    println!("disagreements: {disagreements}");
    println!(
        "a row-count or key-sequence mismatch is a CORRECTNESS finding, not a tuning result: \n\
         the timing of a case that returned the wrong rows means nothing until it is triaged."
    );

    let unsupported: Vec<&Res> = results.iter().filter(|r| r.unsupported.is_some()).collect();
    println!("\n══ UNSUPPORTED ({}) ══  E4's API cannot express these; nothing here is faked or dropped\n", unsupported.len());
    for r in &unsupported {
        println!(
            "{:<10} {:<18} {}{}",
            r.family,
            r.name,
            r.unsupported.unwrap_or(""),
            if r.lite_only {
                ""
            } else {
                "  (no SQLite arm either)"
            }
        );
    }

    let mut coverage: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for r in results {
        let entry = coverage.entry(r.family).or_default();
        if r.unsupported.is_some() {
            entry.1 += 1;
        } else {
            entry.0 += 1;
        }
    }
    println!();
    for (family, (ran, unsupported)) in coverage {
        println!("coverage {family} ran={ran} unsupported={unsupported}");
    }
}

fn print_scale(a: &Report, b: &Report) {
    println!(
        "\n══ SCALE {} → {} rows ══  exponent k in t ∝ N^k, per arm\n",
        a.rows, b.rows
    );
    let ratio = (b.rows as f64 / a.rows as f64).ln();
    let exponent = |small: Option<f64>, large: Option<f64>| match (small, large) {
        (Some(small), Some(large)) if small > 0.0 && large > 0.0 => {
            format!("{:>8.2}", (large / small).ln() / ratio)
        }
        _ => format!("{:>8}", "-"),
    };
    println!(
        "{:<10} {:<18} {:>8} {:>8} {:>12} {:>12}",
        "family", "case", "k E4", "k SQLITE", "E4 µs", "SQLITE µs"
    );
    println!("{}", "-".repeat(72));
    let index: BTreeMap<&str, &Res> = b.results.iter().map(|r| (r.name, r)).collect();
    let mut joined: Vec<(&Res, &Res)> = a
        .results
        .iter()
        .filter(|r| r.e4.is_some())
        .filter_map(|small| index.get(small.name).map(|large| (small, *large)))
        .collect();
    joined.sort_by(|(_, x), (_, y)| {
        y.ratio()
            .unwrap_or(-1.0)
            .total_cmp(&x.ratio().unwrap_or(-1.0))
    });
    for (small, large) in joined {
        println!(
            "{:<10} {:<18} {} {} {:>12.1} {:>12}",
            large.family,
            large.name,
            exponent(small.e4, large.e4),
            exponent(small.lite, large.lite),
            large.e4.unwrap_or(0.0),
            large
                .lite
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "-".into())
        );
    }
    println!(
        "\nload exponent: E4 {:>5.2}  ·  SQLITE {:>5.2}",
        (b.load_e4 / a.load_e4.max(1e-9)).ln() / ratio,
        (b.load_lite / a.load_lite.max(1e-9)).ln() / ratio
    );
}

fn scratch(tag: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("two_ways-{tag}-{}-{stamp}", std::process::id()))
}

fn main() -> R<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "two_ways [rows] [--scale] [--only substr]\n\n\
             Two arms on identical data in one process: E4's embedded API and SQLite, ranked\n\
             worst-ratio-first. Durability is MATCHED AND ON in both arms (E4 publishes every\n\
             commit with a FULL barrier and refuses anything else; SQLite runs WAL +\n\
             synchronous=FULL + fullfsync=ON). Cases E4's API cannot express are listed as\n\
             UNSUPPORTED with a reason; none is faked.\n\n\
             --scale   also run at 100,000 rows and print the exponent per case\n\
             --only s  keep cases whose family or name contains s"
        );
        return Ok(());
    }
    let rows: u64 = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    let only = args
        .iter()
        .position(|a| a == "--only")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let scale = args.iter().any(|a| a == "--scale");

    let root = scratch("a");
    let report = run(rows, only.as_deref(), &root)?;
    print_report(&report);
    let _ = fs::remove_dir_all(&root);

    if scale {
        let big = 100_000;
        let root = scratch("b");
        let large = run(big, only.as_deref(), &root)?;
        print_report(&large);
        print_scale(&report, &large);
        let _ = fs::remove_dir_all(&root);
    }
    Ok(())
}
