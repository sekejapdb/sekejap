//! BATTLE50K — E4 against PostGIS + pgvector/pgvectorscale on one 50,000-row
//! corpus, one arm per process, modelled on `bench/src/bin/popsim.rs`.
//!
//!     battle50k <arm: e4|e4-sql|postgres|sqlite> --data <jsonl> --queries <json>
//!               --out <report.json> [--db-dir <dir|sqlite .db file>] [--dsn <dsn>]
//!               [--only <case-substring>] [--reuse] [--graph] [--dump <dir>]
//!     battle50k compare <a.json> <b.json> [<c.json>] [<d.json>]
//!
//! PURPOSE. `popsim` compares E4 against SQLite and Postgres on a generated
//! population of points and squares. This program asks a narrower question on
//! a FIXED corpus that lives on disk: given the same 50,000 rows — a name, a
//! description, a date, a category, a point, a polygon with holes and a
//! 32-dimensional unit vector each — do E4 and a PostGIS + pgvector +
//! pgvectorscale Postgres return the SAME ROWS, and at what cost per query?
//! Row-count agreement on every filter case is the precondition; a latency
//! ratio between two arms that answered different questions is not a
//! measurement.
//!
//! ARMS.
//!
//!   `e4`        an embedded `Database` under `--db-dir`, collection `place`,
//!               seven indexes: text over the concatenated `text` field,
//!               ordered scalar `born`, Text scalar `kind`, point `loc`,
//!               geometry `plot`, exact vector `emb`, quantized vector `emb`.
//!               Loaded with one `commit` every 256 rows, the cadence
//!               `popsim` uses; every index is built LATE through
//!               `build_index_to_ready` and each build is its own timed stage.
//!               `disk_bytes` is the sum of every file size under `--db-dir`.
//!
//!   `postgres`  a real server at `--dsn`, table
//!               `place(key text primary key, name text, descr text,
//!               born int, kind text, loc geography(Point,4326),
//!               plot geography(Polygon,4326), emb vector(32))`, six indexes:
//!               a GIN index on `to_tsvector('simple', name || ' ' || descr)`,
//!               btrees on `born` and `kind`, GiST on `loc` and on `plot`, and
//!               `USING diskann (emb vector_cosine_ops)` from pgvectorscale.
//!               Loaded in 256-row transactions with `synchronous_commit=on`;
//!               each `CREATE INDEX` is its own timed stage. `disk_bytes` is
//!               `pg_total_relation_size('place')`.
//!
//!   `sqlite`    an embedded SQLite 3.46 (rusqlite's bundled build) in the
//!               FILE named by `--db-dir`, one table
//!               `place("key" text primary key, name, descr, kind, born int,
//!               lon real, lat real, emb blob, plot text, born_ts int)` plus
//!               an FTS5 index over `name || ' ' || descr`, TWO R*Trees (the
//!               point's box and the plot's box) and five ordinary indexes
//!               including the expression index on `lower(kind)`. SQLite has
//!               no geometry type, no geodesic, no vector type and no
//!               traversal atomic, so each of those is a REGISTERED SCALAR
//!               FUNCTION calling sekejap-core itself — `geo_dist_m` is
//!               `spatial_math::wgs84_distance_metres`, the geometry
//!               predicates are `spatial_geometry::{within, contains,
//!               intersects, dwithin_m}` — which makes the ORACLE the same
//!               maths in both arms and the row counts comparable. Loaded in
//!               256-row transactions with `journal_mode=DELETE` and
//!               `synchronous=FULL`; every index build is its own timed
//!               stage. `disk_bytes` is the `.db` file and anything beside
//!               it. A case SQLite cannot express is reported as
//!               `n/a: <reason>`, never skipped.
//!
//! BATTERY. The same twenty cases, in the same order, in both arms, fifty
//! query instances each, driven by `queries.json` (`points`, `boxes`,
//! `polygons`, `radii`, `vectors`, `terms`, fifty of each). Thirteen FILTER
//! cases return every matching row and are compared on row count:
//! `pt_radius`, `pt_bbox`, `plot_within_box`, `plot_contains_pt`,
//! `plot_intersects`, `plot_dwithin_1km`, `plot_vs_poly_within`, `text_one`,
//! `text_two`, `text_and_kind`, `born_range`, `kind_eq`, `radius_and_born`.
//! Seven RANKED cases return ten rows and are compared on top-ten overlap,
//! never on order: `knn_10`, `knn_10_kind`, `text_top10`, `vec_exact_10`,
//! `vec_exact_radius`, `hybrid_10`, `hybrid_blend_10`.
//!
//! APPROXIMATE SWEEP. `vec_ann_10` and `vec_ann_10_kind` are not single
//! cases: each is a SWEEP of cases, one per point on a recall-vs-latency
//! curve, because `ef = 100` and `diskann.query_search_list_size = 100` are
//! equal numbers but not the same knob (deviation 12 below), so comparing
//! one point from each at "the same" setting compares nothing. E4 sweeps
//! `ef` over `EF_SWEEP` (20, 50, 100, 200, 400), naming each point
//! `vec_ann_10@ef<N>` / `vec_ann_10_kind@ef<N>`; Postgres sweeps
//! `diskann.query_search_list_size` over `SLS_SWEEP` (50, 100, 200, 400,
//! 800), naming each point `vec_ann_10@sls<N>` / `vec_ann_10_kind@sls<N>`,
//! plus one extra point per base at the ef=100-matching `sls=100` with
//! `diskann.query_rescore` raised from this server's default to
//! `RESCORE_PROBE` (400), named `..@sls100+resc400`, run only when the
//! server actually exposes that GUC (`pg_has_query_rescore`, probed by
//! `SET LOCAL`, not trusted from `pg_settings` — see the deviation on
//! `place_emb_ann`'s build defaults). Every sweep point is still `kind:
//! approx` with its own `recall_at_k` and `median_us`, measured against the
//! same in-arm exact twin the old fixed-ef case used
//! (`vec_exact_10` / `vec_ann_10_kind:exact`). `compare` and
//! `tools/battle50k_compare.py` print the full sweep table for both arms and
//! then the HEADLINE: each arm's cheapest point with `recall_at_k >= 0.95`
//! and the E4/PG ratio of their `median_us` AT THAT RECALL — the number that
//! actually means something for approximate vector search, unlike a ratio at
//! two knobs that merely share a numeral. An arm that never reaches 0.95
//! reports its best recall instead of a ratio.
//!
//! Each case is warmed by one untimed pass over all fifty instances and then
//! measured by one timed pass; `median_us` and `p90_us` are over the fifty
//! per-instance wall times and `total_rows` is their sum. `first_keys` is
//! instance 0's answer, in result order, at most ten keys.
//!
//! DEVIATIONS. Every one of these is reported in the JSON's `deviations`
//! block as well; none of them is emulated silently.
//!
//!   1. A TEXT INDEX SPANS ONE FIELD, NOT TWO. `Database::create_text_index`
//!      (`src/text_indexes.rs:1845`) takes a single declared `Kind::Text`
//!      field and refuses anything else, so there is no E4 index over
//!      `name` AND `desc`. The loader therefore stores a third Text field,
//!      `text`, holding `name` + " " + `desc`, and indexes THAT; Postgres
//!      indexes the expression `to_tsvector('simple', name || ' ' || descr)`
//!      and every Postgres text query repeats the identical expression, so
//!      the planner can use the expression index. The consequence for the
//!      disk comparison is named rather than hidden: E4 stores `name`, `desc`
//!      AND their concatenation, Postgres stores `name` and `descr` only.
//!
//!   2. KEYS COME BACK THROUGH AN ID-TO-KEY VECTOR IN THE E4 ARM. The brief
//!      asks the filter cases for `Projection::Ids`, and a `QueryRow` carries
//!      an `EntityId`, not the external key. `popsim` recomputes the key from
//!      the sequence because it generates its own keys; this corpus's keys
//!      come from the file, so the loader keeps them in a `Vec<String>`
//!      indexed by `sequence - 1` (the put order is the file order, so that
//!      index is exact) and the arm translates ids through it. The Postgres
//!      arm selects its `key` column directly. Neither translation is inside
//!      a timed statement in the Postgres arm; in the E4 arm the `Vec` lookup
//!      IS inside the timed pass, which costs one indexed read per returned
//!      row and is the closest available match to Postgres returning the
//!      column.
//!
//!   3. `queries.json`'s BOXES ARE `[minlon, maxlon, minlat, maxlat]`. The
//!      file's actual order is longitude, longitude, latitude, latitude —
//!      not the lon/lat/lon/lat the brief's prose assumed. Both arms read the
//!      file's real order, so both build the same rectangle.
//!
//!   4. `queries.json` CARRIES NO `kinds` ARRAY. The eight categories the
//!      `*_kind` cases need are derived instead: the sorted distinct `kind`
//!      values of the data file. Both arms derive them from the same file by
//!      the same rule, and an arm refuses to run if that set is not exactly
//!      eight values.
//!
//!   5. THE BLEND IS SPELT DIFFERENTLY. E4 asks `hybrid_blend_10` through
//!      `QueryOrder::Score` (`0.5 * Bm25 + 0.5 * (1 + VectorSimilarity)`,
//!      cosine), Postgres through `ORDER BY 0.5 * ts_rank_cd(..) + 0.5 *
//!      (1 - (emb <=> v))`. The vector halves are the same cosine; the text
//!      halves are BM25 against ts_rank_cd (deviation 7), so the case is
//!      compared on top-ten overlap, never on order. Neither engine can
//!      answer an arithmetic ORDER BY from an index in order: both rank
//!      every candidate the two filters admit.
//!
//!   6. POSTGRES HAS NO EXACT VECTOR INDEX. `ORDER BY emb <=> v LIMIT 10`
//!      over the diskann index is approximate, and bounding
//!      `diskann.query_search_list_size` does not make it exact. The three
//!      exact vector cases therefore run inside a transaction with
//!      `SET LOCAL enable_indexscan = off; SET LOCAL enable_bitmapscan = off`,
//!      which is a sequential scan with an exact distance per row. That also
//!      removes the GiST and GIN indexes from the filters of
//!      `vec_exact_radius` and `hybrid_10`, so those two Postgres cases are
//!      full sequential scans and their latency is not an index measurement.
//!      E4's exact arm is a real index (`emb_exact`), so the E4 report has an
//!      `index:place_emb_exact` build stage that the Postgres report does not.
//!
//!   7. THE RANKING FORMULAS DIFFER. E4 ranks text by BM25, Postgres by
//!      `ts_rank_cd`. Like `popsim`'s `name_top10`, `text_top10` is compared
//!      on row count and on top-ten overlap, never on order.
//!
//!   8. K-NEAREST TIE-BREAKING DIFFERS. `QueryOrder::Distance` breaks ties by
//!      entity id (`src/query.rs:169`); the Postgres spelling the brief asks
//!      for, `ORDER BY loc <-> point LIMIT 10`, has no tiebreak, because
//!      adding one would take the ordered KNN-GiST walk away from the
//!      planner. The comparison of `first_keys` is therefore SET overlap.
//!
//!   9. GEOMETRY UNITS ARE MATCHED THE WAY `GeometryFilter` DOCUMENTS THEM
//!      (`src/query.rs:100`): `Intersects` and `DWithin` are spheroidal, so
//!      Postgres gets `ST_Intersects` / `ST_DWithin(..., true)` on
//!      `geography`; `Within` and `Contains` are planar, so Postgres gets
//!      `ST_Within` / `ST_Contains` on `plot::geometry`, which is what PostGIS
//!      itself does for lack of a geography overload. What remains is the
//!      routine, not the model: E4 refines with `spatial_geometry`, Postgres
//!      with PostGIS's own predicates, and rows within a metre of a spheroidal
//!      boundary can be decided differently by the two.
//!
//!  10. `born` IS `int` IN POSTGRES AND `Kind::Int` (i64) IN E4. Every value
//!      in this corpus is a yyyymmdd that fits in int4, so no value is
//!      truncated; the width is still different on the two sides.
//!
//!  11. `--reuse` STILL READS THE JSONL. It loads nothing and builds nothing —
//!      that is the point — but the E4 arm needs the id-to-key vector of
//!      deviation 2 and both arms need the eight categories of deviation 4,
//!      and both come from the data file. The stage block reports `null` for
//!      every stage that did not happen, never zero.
//!
//!  12. `ef` AND `diskann.query_search_list_size` ARE BOTH 100. They are not
//!      the same knob: E4's `ef` bounds a compact shortlist that is then
//!      reranked from f32 sidecars, and pgvectorscale's search list size
//!      bounds a graph beam. Setting both to 100 matches the number, not the
//!      algorithm, which is why recall is computed INSIDE each arm against
//!      that arm's own exact answer and never across arms.
//!
//!  13. THE KEY IS STORED TWICE IN BOTH ARMS, AND THAT IS DELIBERATE. E4
//!      declares a `key` Text field alongside the external key that
//!      `Database::put` already maps, because the brief's schema names it;
//!      Postgres holds `key` in the heap tuple and again in the primary-key
//!      btree. Neither arm is given the smaller footprint the other cannot
//!      have.
//!
//!  14. THE PLANAR GEOMETRY CASES GET A `&&` CANDIDATE IN POSTGRES.
//!      `ST_Within` and `ST_Contains` run on `plot::geometry`, and no index on
//!      a `geography` column can serve a geometry cast, so `plot_within_box`,
//!      `plot_contains_pt` and `plot_vs_poly_within` each carry a leading
//!      `plot && <candidate>::geography` bounding-box term that lets the GiST
//!      index narrow the scan before the exact refine. A bounding box that
//!      fails to overlap cannot contain or be contained, so the term is a
//!      superset test for both predicates: it changes the cost, never the
//!      answer. It is the same candidate-then-refine shape `pt_bbox` uses on
//!      both sides, and `popsim`'s SQLite arm uses throughout.
//!
//!  15. THE SMOKE TEST IS NOT COMPILED BY THE RELEASE BUILD.
//!      `tests/battle50k_smoke.rs` includes this file by `#[path]`, the way
//!      `tests/popsim_smoke.rs` includes `popsim.rs`, so what it exercises is
//!      what a run would execute. `cargo build --release` does not build test
//!      targets, so the release build proves this binary compiles and says
//!      nothing about that file.

use sekejap_core::{
    collections::{
        Accumulator, AggValue, AggregateFn, AggregateInput, AggregateRequest, BfsRequest,
        CandidateDriver, Cmp, CollectionId, CollectionOptions, Database, Direction,
        EdgePredicate, EdgeTypeId, EntityId, Geom, GeometryFilter, GraphContextId, GroupCmp,
        GroupKey, GroupOrder, GroupPredicate, IndexExpr, IndexId, IndexState, NewEdge,
        OwnedScalarValue,
        PointFilter, ProjectedValue, Projection, QueryBudget, QueryFilter, QueryOrder,
        QueryRequest, ScalarFilter, ScalarValue, ScoreExpr, SortDirection, TextMatch,
        VectorMetric,
    },
    spatial_geometry,
    spatial_math::{radius_candidate_bounds, wgs84_distance_metres, Bounds, Point},
};
use sekejap_lang::{prepare_sql, Param, PreparedSql, SqlDatabase, SqlError, SqlValue};
use sekejap_core::{
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use postgres::{types::ToSql, Client, NoTls};
use rusqlite::{
    functions::FunctionFlags,
    params_from_iter,
    types::{Value as LiteValue, ValueRef},
    Connection,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    io::{BufRead, BufReader},
    ops::Bound,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

type R<T> = Result<T, Box<dyn std::error::Error>>;

// ── constants ─────────────────────────────────────────────────────────────

/// Query instances per case, one per entry of every array in `queries.json`.
pub const INSTANCES: usize = 50;
/// Rows per commit / per transaction, both arms — `popsim`'s load cadence.
pub const BATCH: usize = 256;
/// Top-k for every ranked and approximate case.
pub const K: usize = 10;
/// E4's approximate shortlist bound, and pgvectorscale's search list size.
/// Superseded as the single approx-case setting by `EF_SWEEP` /
/// `SLS_SWEEP` below; kept because it is still the ef=100 / sls=100 point
/// both sweeps carry, the one the two knobs share by number alone.
pub const EF: usize = 100;
/// E4 `ef` values the approximate cases sweep, tracing a recall-vs-latency
/// curve instead of comparing recall at one arbitrary shortlist width.
pub const EF_SWEEP: [usize; 5] = [20, 50, 100, 200, 400];
/// pgvectorscale `diskann.query_search_list_size` values the approximate
/// cases sweep — the Postgres axis of the same curve, not the same knob as
/// `EF_SWEEP` (deviation 12): E4's `ef` bounds a compact int8 shortlist that
/// is then reranked from f32 sidecars, this bounds a graph beam.
pub const SLS_SWEEP: [usize; 5] = [50, 100, 200, 400, 800];
/// `diskann.query_rescore` value tried at the sweep's ef=100-matching
/// midpoint (`sls = 100`), on top of the server's own default, to see
/// whether rescoring more candidates recovers recall without widening the
/// search list itself. Only tried when `pg_has_query_rescore` confirms the
/// GUC exists on the server actually running.
pub const RESCORE_PROBE: usize = 400;
/// The base case names whose single approximate point becomes a sweep.
pub const APPROX_BASES: [&str; 2] = ["vec_ann_10", "vec_ann_10_kind"];
/// The recall threshold the sweep's headline ratio is measured at.
pub const HEADLINE_RECALL: f64 = 0.95;
/// E4's maximum page. A complete answer is assembled from repeated pages and
/// that assembly is part of what a case costs.
const PAGE: usize = 8192;
/// Cache budget for the E4 arm, the same 8 MiB `popsim` gives every arm.
const CACHE_BYTES: usize = 8 << 20;
/// Keys reported per case, for instance 0 only.
pub const FIRST_KEYS: usize = 10;
/// Categories the `*_kind` cases cycle through. Derived, see deviation 4.
pub const KINDS: usize = 8;
/// Vector width of the `emb` field.
pub const DIM: usize = 32;

/// Rows one instance of a `vec_bulk_write_1k*` case writes, as one batch
/// under one commit.
pub const VEC_BULK_ROWS: usize = 1_000;
/// The scratch collection / table every write case builds and throws away.
pub const VEC_BULK_OBJECT: &str = "place_bulk";

/// `--db-dir` when the flag is absent: a directory under the system temp dir.
fn default_db_dir() -> PathBuf {
    std::env::temp_dir().join("sekejap-bench50k").join("e4-db")
}
/// `--dsn` when the flag is absent: `SEKEJAP_BENCH_PG_DSN`, else a local
/// default server.
fn default_dsn() -> String {
    std::env::var("SEKEJAP_BENCH_PG_DSN")
        .unwrap_or_else(|_| "postgres://127.0.0.1:5432/postgres".into())
}

// ── the corpus ────────────────────────────────────────────────────────────

/// One row of `places-50000.jsonl`, held as both arms need it: E4 wants a
/// `Geom` and an `f32` slice, Postgres wants GeoJSON text and a `vector`
/// literal, so the row carries both spellings rather than converting inside a
/// timed statement.
pub struct Row {
    pub key: String,
    pub name: String,
    pub desc: String,
    pub born: i64,
    pub kind: String,
    pub lon: f64,
    pub lat: f64,
    pub plot: Geom,
    pub plot_json: String,
    pub emb: Vec<f32>,
    pub emb_literal: String,
}

impl Row {
    /// The concatenation E4's single-field text index is built over. See
    /// deviation 1.
    fn text(&self) -> String {
        format!("{} {}", self.name, self.desc)
    }
}

pub struct Corpus {
    pub rows: Vec<Row>,
    /// The file's keys in file order, which is put order, which is entity
    /// sequence order. Index `sequence - 1`. See deviation 2.
    pub keys: Vec<String>,
    /// The sorted distinct `kind` values. See deviation 4.
    pub kinds: Vec<String>,
}

fn str_at(value: &Value, field: &str) -> R<String> {
    Ok(value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("row field `{field}` is not a string"))?
        .to_owned())
}

fn geom_from_json(value: &Value) -> R<Geom> {
    let ty = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or("geometry has no type")?;
    let coordinates = value
        .get("coordinates")
        .cloned()
        .ok_or("geometry has no coordinates")?;
    Ok(match ty {
        "Point" => {
            let p: [f64; 2] = serde_json::from_value(coordinates)?;
            Geom::Point(p[0], p[1])
        }
        "LineString" => Geom::LineString(serde_json::from_value(coordinates)?),
        "Polygon" => Geom::Polygon(serde_json::from_value(coordinates)?),
        "MultiPoint" => Geom::MultiPoint(serde_json::from_value(coordinates)?),
        "MultiLineString" => Geom::MultiLineString(serde_json::from_value(coordinates)?),
        "MultiPolygon" => Geom::MultiPolygon(serde_json::from_value(coordinates)?),
        other => return Err(format!("unsupported geometry type {other}").into()),
    })
}

fn geom_to_json(geom: &Geom) -> Value {
    match geom {
        Geom::Point(x, y) => json!({"type": "Point", "coordinates": [x, y]}),
        Geom::LineString(c) => json!({"type": "LineString", "coordinates": c}),
        Geom::Polygon(rings) => json!({"type": "Polygon", "coordinates": rings}),
        Geom::MultiPoint(c) => json!({"type": "MultiPoint", "coordinates": c}),
        Geom::MultiLineString(rs) => json!({"type": "MultiLineString", "coordinates": rs}),
        Geom::MultiPolygon(ps) => json!({"type": "MultiPolygon", "coordinates": ps}),
    }
}

/// pgvector's text input form: `[a,b,c]`, cast to `vector` at the call site.
fn vector_literal(v: &[f32]) -> String {
    let mut out = String::with_capacity(v.len() * 10 + 2);
    out.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{x:?}"));
    }
    out.push(']');
    out
}

pub fn load_corpus(path: &Path) -> R<Corpus> {
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut rows = Vec::new();
    for (lineno, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line)
            .map_err(|e| format!("{}:{}: {e}", path.display(), lineno + 1))?;
        let loc = geom_from_json(value.get("loc").ok_or("row has no loc")?)?;
        let Geom::Point(lon, lat) = loc else {
            return Err(format!("{}:{}: loc is not a Point", path.display(), lineno + 1).into());
        };
        let plot = geom_from_json(value.get("plot").ok_or("row has no plot")?)?;
        let emb: Vec<f32> = value
            .get("emb")
            .and_then(Value::as_array)
            .ok_or("row has no emb array")?
            .iter()
            .map(|x| x.as_f64().map(|x| x as f32).ok_or("emb holds a non-number"))
            .collect::<Result<_, _>>()?;
        if emb.len() != DIM {
            return Err(format!(
                "{}:{}: emb has {} dimensions, expected {DIM}",
                path.display(),
                lineno + 1,
                emb.len()
            )
            .into());
        }
        rows.push(Row {
            key: str_at(&value, "key")?,
            name: str_at(&value, "name")?,
            desc: str_at(&value, "desc")?,
            born: value
                .get("born")
                .and_then(Value::as_i64)
                .ok_or("row field `born` is not an integer")?,
            kind: str_at(&value, "kind")?,
            lon,
            lat,
            plot_json: geom_to_json(&plot).to_string(),
            plot,
            emb_literal: vector_literal(&emb),
            emb,
        });
    }
    if rows.is_empty() {
        return Err(format!("{} holds no rows", path.display()).into());
    }
    let keys = rows.iter().map(|r| r.key.clone()).collect();
    let kinds: Vec<String> = rows
        .iter()
        .map(|r| r.kind.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if kinds.len() != KINDS {
        return Err(format!(
            "{} holds {} distinct kinds, expected {KINDS}; the *_kind cases index kinds[i % {KINDS}]",
            path.display(),
            kinds.len()
        )
        .into());
    }
    Ok(Corpus { rows, keys, kinds })
}

// ── the query instances ───────────────────────────────────────────────────

/// `queries.json`, read as the file actually spells it. `boxes` are
/// `[minlon, maxlon, minlat, maxlat]`; see deviation 3.
pub struct Queries {
    pub points: Vec<[f64; 2]>,
    pub boxes: Vec<[f64; 4]>,
    pub polygons: Vec<Geom>,
    pub polygon_json: Vec<String>,
    pub radii: Vec<[f64; 3]>,
    pub vectors: Vec<Vec<f32>>,
    pub vector_literals: Vec<String>,
    pub terms: Vec<String>,
    /// The key of the row nearest to `points[i]`, one per instance -- the
    /// seed every graph case starts its traversal at. Filled in by
    /// [`Queries::resolve_seeds`] once the corpus is read, and empty until
    /// then, because `queries.json` names points and the graph names keys.
    pub seed_keys: Vec<String>,
}

fn array_at<'a>(value: &'a Value, field: &str) -> R<&'a Vec<Value>> {
    value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("queries.json has no `{field}` array").into())
}

pub fn load_queries(path: &Path) -> R<Queries> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: Value = serde_json::from_str(&text)?;
    let points: Vec<[f64; 2]> = serde_json::from_value(Value::Array(array_at(&value, "points")?.clone()))?;
    let boxes: Vec<[f64; 4]> = serde_json::from_value(Value::Array(array_at(&value, "boxes")?.clone()))?;
    let radii: Vec<[f64; 3]> = serde_json::from_value(Value::Array(array_at(&value, "radii")?.clone()))?;
    let vectors: Vec<Vec<f32>> = serde_json::from_value(Value::Array(array_at(&value, "vectors")?.clone()))?;
    let terms: Vec<String> = serde_json::from_value(Value::Array(array_at(&value, "terms")?.clone()))?;
    let polygons: Vec<Geom> = array_at(&value, "polygons")?
        .iter()
        .map(geom_from_json)
        .collect::<R<_>>()?;
    let polygon_json = polygons.iter().map(|g| geom_to_json(g).to_string()).collect();
    let vector_literals = vectors.iter().map(|v| vector_literal(v)).collect();
    for (name, len) in [
        ("points", points.len()),
        ("boxes", boxes.len()),
        ("polygons", polygons.len()),
        ("radii", radii.len()),
        ("vectors", vectors.len()),
        ("terms", terms.len()),
    ] {
        if len < INSTANCES {
            return Err(format!("queries.json `{name}` holds {len} entries, need {INSTANCES}").into());
        }
    }
    for (i, v) in vectors.iter().enumerate() {
        if v.len() != DIM {
            return Err(format!("queries.json vectors[{i}] has {} dimensions, expected {DIM}", v.len()).into());
        }
    }
    Ok(Queries {
        points,
        boxes,
        polygons,
        polygon_json,
        radii,
        vectors,
        vector_literals,
        terms,
        seed_keys: Vec::new(),
    })
}

impl Queries {
    /// The rectangle `boxes[i]` names, as E4's `Bounds`.
    pub fn bounds(&self, i: usize) -> R<Bounds> {
        let b = self.boxes[i];
        Bounds::new(b[0], b[1], b[2], b[3]).map_err(|e| format!("boxes[{i}]: {e}").into())
    }

    /// The same rectangle as a closed polygon ring, for the geometry cases.
    pub fn box_polygon(&self, i: usize) -> Geom {
        let b = self.boxes[i];
        Geom::Polygon(vec![vec![
            [b[0], b[2]],
            [b[1], b[2]],
            [b[1], b[3]],
            [b[0], b[3]],
            [b[0], b[2]],
        ]])
    }

    pub fn point(&self, i: usize) -> R<Point> {
        let p = self.points[i];
        Point::new(p[0], p[1]).map_err(|e| format!("points[{i}]: {e}").into())
    }

    pub fn radius_centre(&self, i: usize) -> R<Point> {
        let r = self.radii[i];
        Point::new(r[0], r[1]).map_err(|e| format!("radii[{i}]: {e}").into())
    }

    pub fn radius_metres(&self, i: usize) -> f64 {
        self.radii[i][2]
    }

    /// `born between 19500101 and 19500101 + 10000*(i%7)` — an inclusive
    /// range that is one day wide when `i % 7` is zero and roughly six
    /// decades wide when it is six.
    pub fn born_range(&self, i: usize) -> (i64, i64) {
        (19_500_101, 19_500_101 + 10_000 * (i as i64 % 7))
    }

    /// The year `fn_year_eq` asks for. The corpus's `born` spans 1940-01-01
    /// to 2025-12-28, so every instance names a year that is in it.
    pub fn fn_year(&self, i: usize) -> i64 {
        1_940 + (i as i64 % 80)
    }

    /// The `[first month, last month]` window `fn_trunc_month_range` asks
    /// for, as two `date_trunc('month', ...)` literals. Six months wide.
    pub fn fn_month_window(&self, i: usize) -> (String, String) {
        let year = self.fn_year(i);
        (format!("{year:04}-01-01"), format!("{year:04}-06-01"))
    }

    /// The two-letter name prefix `fn_like_prefix` asks for. Each of these
    /// holds roughly 3,000 of the corpus's 50,000 names.
    pub fn name_prefix(&self, i: usize) -> &'static str {
        const PREFIXES: [&str; 8] = ["Ti", "Ni", "Ri", "La", "Bu", "De", "Yu", "An"];
        PREFIXES[i % PREFIXES.len()]
    }

    /// The key of the row nearest to `points[i]`, which every graph case
    /// seeds its traversal at.
    ///
    /// It is computed ONCE, outside every timed pass, and handed to all
    /// three arms as a KEY -- not as an id, not as a coordinate -- because a
    /// pattern seeds on a key equality and a `related` row names its
    /// endpoints by key. An arm that found its own seed would be answering a
    /// different question wherever two rows tie on distance.
    pub fn seed_key(&self, i: usize) -> R<&str> {
        self.seed_keys
            .get(i)
            .map(String::as_str)
            .ok_or_else(|| "graph seed keys were not computed for this run".into())
    }

    /// Fill in [`Queries::seed_keys`] from the corpus, with the same geodesic
    /// distance `QueryOrder::Distance` ranks by, ties broken by file order.
    pub fn resolve_seeds(&mut self, corpus: &Corpus) -> R<()> {
        self.seed_keys = Vec::with_capacity(INSTANCES);
        for i in 0..INSTANCES {
            let centre = self.point(i)?;
            let mut best: Option<(f64, usize)> = None;
            for (at, row) in corpus.rows.iter().enumerate() {
                let metres = wgs84_distance_metres(centre, Point::new(row.lon, row.lat)?);
                if best.is_none_or(|(d, _)| metres < d) {
                    best = Some((metres, at));
                }
            }
            let (_, at) = best.ok_or("the corpus is empty")?;
            self.seed_keys.push(corpus.rows[at].key.clone());
        }
        Ok(())
    }
}

// ── the battery ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseKind {
    Filter,
    Ranked,
    Approx,
    /// A WRITE case. It returns no rows from the database: `total_rows` is
    /// the rows it PUT there, and two arms agree when they wrote the same
    /// number of them, which is the same agreement a filter case is judged
    /// on. Every write case builds its own scratch state, measures one
    /// batch, and leaves the corpus it was measured beside untouched.
    Write,
}

impl CaseKind {
    fn label(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::Ranked => "ranked",
            Self::Approx => "approx",
            Self::Write => "write",
        }
    }
}

pub struct CaseSpec {
    pub name: &'static str,
    pub kind: CaseKind,
}

/// The same names in the same order in both arms. `compare` walks this list,
/// not either report's own ordering, so a missing case is a visible hole.
/// `vec_ann_10` and `vec_ann_10_kind` are NOT here: they are the two
/// approximate sweeps (`APPROX_BASES`), generated at runtime, one case per
/// `EF_SWEEP` / `SLS_SWEEP` point, because there is no longer one fixed-ef
/// case for either to be a fixed entry of.
pub const BATTERY: [CaseSpec; 43] = [
    CaseSpec { name: "pt_radius", kind: CaseKind::Filter },
    CaseSpec { name: "pt_bbox", kind: CaseKind::Filter },
    CaseSpec { name: "plot_within_box", kind: CaseKind::Filter },
    CaseSpec { name: "plot_contains_pt", kind: CaseKind::Filter },
    CaseSpec { name: "plot_intersects", kind: CaseKind::Filter },
    CaseSpec { name: "plot_dwithin_1km", kind: CaseKind::Filter },
    CaseSpec { name: "plot_vs_poly_within", kind: CaseKind::Filter },
    CaseSpec { name: "text_one", kind: CaseKind::Filter },
    CaseSpec { name: "text_two", kind: CaseKind::Filter },
    CaseSpec { name: "text_and_kind", kind: CaseKind::Filter },
    CaseSpec { name: "born_range", kind: CaseKind::Filter },
    CaseSpec { name: "kind_eq", kind: CaseKind::Filter },
    CaseSpec { name: "radius_and_born", kind: CaseKind::Filter },
    // ── the boolean battery (QL_CONTRACT §3) ────────────────────────────
    // Every one of these is ONE membership set: a union of equalities, a
    // union across two families, a complement inside one index, a union of
    // two covers, the complement of the nullish key, and a semi-join set.
    CaseSpec { name: "bool_kind_in3", kind: CaseKind::Filter },
    CaseSpec { name: "bool_born_or_kind", kind: CaseKind::Filter },
    CaseSpec { name: "bool_not_kind", kind: CaseKind::Filter },
    CaseSpec { name: "bool_radius_or_radius", kind: CaseKind::Filter },
    CaseSpec { name: "bool_not_null_born", kind: CaseKind::Filter },
    // Needs the `related` edge set, so `--graph` selects it exactly as it
    // selects the four `graph_` cases.
    CaseSpec { name: "bool_exists_related", kind: CaseKind::Filter },
    CaseSpec { name: "knn_10", kind: CaseKind::Ranked },
    CaseSpec { name: "knn_10_kind", kind: CaseKind::Ranked },
    CaseSpec { name: "text_top10", kind: CaseKind::Ranked },
    CaseSpec { name: "vec_exact_10", kind: CaseKind::Ranked },
    CaseSpec { name: "vec_exact_radius", kind: CaseKind::Ranked },
    CaseSpec { name: "hybrid_10", kind: CaseKind::Ranked },
    CaseSpec { name: "hybrid_blend_10", kind: CaseKind::Ranked },
    // ── the aggregate battery (QL_CONTRACT §4.7) ────────────────────────
    // Filter-kind, because a folded answer is compared the way a filter
    // answer is: every arm must return the SAME NUMBER OF GROUPS, and each
    // arm formats every group into one text line, so `--dump` diffs the
    // VALUES key by key and not only the count.
    CaseSpec { name: "agg_count_all", kind: CaseKind::Filter },
    CaseSpec { name: "agg_count_kind", kind: CaseKind::Filter },
    CaseSpec { name: "agg_sum_born_by_kind", kind: CaseKind::Filter },
    CaseSpec { name: "agg_distinct_kind", kind: CaseKind::Filter },
    CaseSpec { name: "agg_count_radius_by_kind", kind: CaseKind::Filter },
    CaseSpec { name: "agg_born_decade", kind: CaseKind::Filter },
    // ── the function battery (QL_CONTRACT §4.1, §4.2) ───────────────────
    // Four index-side RANGE REWRITES and one projection-only case. The
    // rewrites are filter-kind because that is what they are: a function in
    // a WHERE folded into scalar bounds at prepare, answered by the same
    // walk an ordinary range is answered by. `fn_project_strings` is
    // filter-kind too -- its WHERE is an ordinary Tier-1 range and the
    // functions are all in the projection, which is the case that shows the
    // row-function cost on its own.
    CaseSpec { name: "fn_year_eq", kind: CaseKind::Filter },
    CaseSpec { name: "fn_trunc_month_range", kind: CaseKind::Filter },
    CaseSpec { name: "fn_lower_eq", kind: CaseKind::Filter },
    CaseSpec { name: "fn_like_prefix", kind: CaseKind::Filter },
    CaseSpec { name: "fn_project_strings", kind: CaseKind::Filter },
    // ── the vector WRITE battery ────────────────────────────────────────
    // A typical embedding application writes vectors in BULK and queries them
    // approximately; the battery above only ever asked the second half. Each
    // of these writes VEC_BULK_ROWS rows of a DIM-lane embedding as ONE
    // batch with ONE commit, into a scratch collection that is rebuilt from
    // nothing before every instance, so instance 50 pays what instance 1
    // paid and the 50,000-row corpus the rest of the battery measures is
    // never written to. The pair is the measurement: the first has both
    // vector index families LIVE before the first row lands, the second has
    // no index at all, and the DIFFERENCE is what index maintenance costs
    // per row.
    CaseSpec { name: "vec_bulk_write_1k", kind: CaseKind::Write },
    CaseSpec { name: "vec_bulk_write_1k_noindex", kind: CaseKind::Write },
    // The four graph cases. They need the `related` edge set, so they are
    // selected only when `--graph` was given (`needs_graph`).
    CaseSpec { name: "graph_2hop", kind: CaseKind::Filter },
    CaseSpec { name: "graph_2hop_weight", kind: CaseKind::Filter },
    CaseSpec { name: "graph_2hop_born", kind: CaseKind::Filter },
    CaseSpec { name: "graph_1hop_weight_top10", kind: CaseKind::Ranked },
];

/// One group of an aggregate case, as a line every arm writes the same way:
/// the group key, then `|name=value` per accumulator. Counts, sums and
/// extremes are whole numbers in every arm; `avg` is compared as
/// `floor(avg)`, because a bigint floor is a number two engines can agree on
/// and a printed float is not.
fn agg_line(key: &str, fields: &[(&str, i64)]) -> String {
    let mut out = key.to_owned();
    for (name, value) in fields {
        out.push('|');
        out.push_str(name);
        out.push('=');
        out.push_str(&value.to_string());
    }
    out
}

/// Is this case a folded answer rather than a row answer?
fn is_aggregate_case(name: &str) -> bool {
    name.starts_with("agg_")
}

/// Is this case a WRITE rather than a question? A write case builds its own
/// scratch state and never touches the corpus the rest of the battery reads.
pub fn is_write_case(name: &str) -> bool {
    name.starts_with("vec_bulk_write_")
}

/// Does this write case want the two vector index families LIVE while it
/// writes? `..._noindex` does not; the pair's difference is the maintenance.
pub fn write_case_is_indexed(name: &str) -> bool {
    name == "vec_bulk_write_1k"
}

/// The key and the embedding of row `at` of write instance `i`.
///
/// The EMBEDDING comes from the corpus, so every arm writes the same lanes in
/// the same order and the distribution is the corpus's own rather than a
/// generator's; the KEY is minted from the instance and the offset, so fifty
/// instances of a thousand rows never collide with each other and never
/// collide with the corpus's own keys. Reading the corpus modulo its length
/// is what lets the smoke test drive the same case over two hundred rows.
pub fn bulk_row(corpus: &Corpus, i: usize, at: usize) -> R<(String, &[f32])> {
    if corpus.rows.is_empty() {
        return Err("a write case needs a non-empty corpus to take embeddings from".into());
    }
    let source = (i * VEC_BULK_ROWS + at) % corpus.rows.len();
    Ok((
        format!("bulk-{i:02}-{at:06}"),
        corpus.rows[source].emb.as_slice(),
    ))
}

/// True for a case that reads the `related` edge set, which only a `--graph`
/// load writes. Without it the case has nothing to answer from and is
/// skipped rather than answered with zero rows.
pub fn needs_graph(name: &str) -> bool {
    name.starts_with("graph_") || name == "bool_exists_related"
}

/// How many nearest neighbours by `loc` each row is linked to.
pub const RELATED_DEGREE: usize = 3;
/// The `related` edge type's name in E4, and the `related` TABLE's name in
/// Postgres.
pub const RELATED: &str = "related";

/// The exact counterpart each approximate case's recall is measured against,
/// inside the SAME arm. See deviation 12. Sweep-point names
/// (`vec_ann_10@ef20`, `vec_ann_10_kind@sls100+resc400`, ...) carry the same
/// base before the `@`, so the twin is resolved from the base alone.
fn exact_twin(name: &str) -> Option<&'static str> {
    let base = name.split('@').next().unwrap_or(name);
    match base {
        "vec_ann_10" => Some("vec_exact_10"),
        "vec_ann_10_kind" => Some("vec_ann_10_kind:exact"),
        _ => None,
    }
}

/// The shape of one approximate-sweep case name, parsed back out of the
/// generated string so `e4_case` / `pg_case` can dispatch it without a
/// combinatorial match arm per sweep point. `by_kind` mirrors the `_kind`
/// suffix on the base name (the case still filters on `kinds[i % KINDS]`
/// exactly as the old fixed-ef case did); the arm-specific knob is `ef` for
/// E4 names and `sls` (plus an optional `rescore`) for Postgres names.
struct ApproxSweepPoint {
    by_kind: bool,
    ef: Option<usize>,
    sls: Option<usize>,
    rescore: Option<usize>,
}

fn parse_approx_sweep(name: &str) -> Option<ApproxSweepPoint> {
    let (base, suffix) = name.split_once('@')?;
    let by_kind = match base {
        "vec_ann_10" => false,
        "vec_ann_10_kind" => true,
        _ => return None,
    };
    if let Some(n) = suffix.strip_prefix("ef") {
        return Some(ApproxSweepPoint {
            by_kind,
            ef: Some(n.parse().ok()?),
            sls: None,
            rescore: None,
        });
    }
    if let Some(rest) = suffix.strip_prefix("sls") {
        let (sls_part, rescore) = match rest.split_once("+resc") {
            Some((s, r)) => (s, Some(r.parse().ok()?)),
            None => (rest, None),
        };
        return Some(ApproxSweepPoint {
            by_kind,
            ef: None,
            sls: Some(sls_part.parse().ok()?),
            rescore,
        });
    }
    None
}

// ── what a case answers with ──────────────────────────────────────────────

/// The number of rows the case produced plus the first ten keys in result
/// order. A filter case over 50,000 rows counts as it goes and keeps ten
/// keys, so no case materialises its whole answer in this process.
#[derive(Clone, Debug, Default)]
pub struct Answer {
    pub rows: u64,
    pub keys: Vec<String>,
    /// Microseconds this answer spent OUTSIDE the engine before its query ran
    /// -- the `e4-sql` arm's parse and compile. Zero for every other arm.
    /// `measure` subtracts it per instance so the arm's run cost is compared
    /// to the `e4` arm's on the same footing, instance by instance, instead
    /// of as a difference of two medians over a skewed spread.
    pub prepare_us: f64,
}

/// While set, a case keeps EVERY key it returned instead of only the first
/// [`FIRST_KEYS`], so `--dump` can write the whole answer out. It is off for
/// the warm pass and for the timed pass, so no measured number ever pays for
/// the extra pushes; `run_arm` turns it on for one extra untimed pass.
static DUMP_ALL_KEYS: AtomicBool = AtomicBool::new(false);

fn dumping() -> bool {
    DUMP_ALL_KEYS.load(Ordering::Relaxed)
}

impl Answer {
    fn push(&mut self, key: String) {
        if dumping() || self.keys.len() < FIRST_KEYS {
            self.keys.push(key);
        }
        self.rows += 1;
    }
}

/// `<dir>/<case>/<i>.keys` — one key per line, sorted, for one query
/// instance of one case. Written by the extra untimed `--dump` pass so the
/// two arms' answers can be diffed key by key rather than by row count.
fn write_dump(dir: &Path, case: &str, i: usize, keys: &[String]) -> R<()> {
    let case_dir = dir.join(case);
    fs::create_dir_all(&case_dir)?;
    let mut sorted: Vec<&str> = keys.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut body = String::new();
    for key in sorted {
        body.push_str(key);
        body.push('\n');
    }
    fs::write(case_dir.join(format!("{i}.keys")), body)?;
    Ok(())
}

/// `<dir>/<case>/statement.sql` — the exact statement one instance of the
/// case ran, written beside the keys it returned so a reader of the dump can
/// see WHAT was asked and not only what came back. Written by the `sqlite`
/// arm, whose whole subject is how a case has to be spelt without the types
/// the other engines have.
fn write_statement(dir: &Path, case: &str, sql: &str) -> R<()> {
    let case_dir = dir.join(case);
    fs::create_dir_all(&case_dir)?;
    fs::write(case_dir.join("statement.sql"), format!("{sql}\n"))?;
    Ok(())
}

/// One untimed pass over all fifty instances of one case, writing every
/// instance's full key set under `dir`.
fn dump_case(dir: &Path, case: &str, mut run: impl FnMut(usize) -> R<Answer>) -> R<()> {
    DUMP_ALL_KEYS.store(true, Ordering::Relaxed);
    let result = (|| {
        for i in 0..INSTANCES {
            let answer = run(i)?;
            write_dump(dir, case, i, &answer.keys)?;
        }
        Ok(())
    })();
    DUMP_ALL_KEYS.store(false, Ordering::Relaxed);
    result
}

#[derive(Clone, Debug)]
pub struct CaseResult {
    pub median_us: f64,
    pub p90_us: f64,
    /// Median over instances of (wall - prepare): the engine's own cost. Equal
    /// to `median_us` for arms that prepare nothing.
    pub run_median_us: f64,
    /// Median over instances of the prepare cost; 0 for arms that prepare nothing.
    pub prepare_median_us: f64,
    /// Median over instances of the whole call when the statement is
    /// PREPARED ONCE and re-bound per instance (`--prepared`, `e4-sql`
    /// only). `None` when the run did not ask for it, or when the case's
    /// statement TEXT is not the same for every instance, in which case a
    /// prepared statement would be a different statement each time.
    pub prepared_median_us: Option<f64>,
    /// Median over instances of the BIND alone, beside
    /// [`CaseResult::prepared_median_us`].
    pub prepared_bind_median_us: Option<f64>,
    pub total_rows: u64,
    pub first_keys: Vec<String>,
}

/// Nearest-rank percentile over the fifty samples: `p90` is the 45th smallest.
fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    let n = sorted.len();
    let rank = ((n as f64) * fraction).ceil() as usize;
    sorted[rank.clamp(1, n) - 1]
}

/// One untimed warm pass over all fifty instances, then one timed pass. The
/// reported numbers are the timed pass's.
fn measure(mut run: impl FnMut(usize) -> R<Answer>) -> R<CaseResult> {
    for i in 0..INSTANCES {
        run(i)?;
    }
    let mut micros = Vec::with_capacity(INSTANCES);
    let mut run_micros = Vec::with_capacity(INSTANCES);
    let mut prepare_micros = Vec::with_capacity(INSTANCES);
    let mut total_rows = 0u64;
    let mut first_keys = Vec::new();
    for i in 0..INSTANCES {
        let at = Instant::now();
        let answer = run(i)?;
        let wall = at.elapsed().as_secs_f64() * 1e6;
        micros.push(wall);
        run_micros.push((wall - answer.prepare_us).max(0.0));
        prepare_micros.push(answer.prepare_us);
        total_rows += answer.rows;
        if i == 0 {
            first_keys = answer.keys.clone();
        }
    }
    let mut sorted = micros;
    sorted.sort_by(f64::total_cmp);
    run_micros.sort_by(f64::total_cmp);
    prepare_micros.sort_by(f64::total_cmp);
    Ok(CaseResult {
        median_us: percentile(&sorted, 0.5),
        p90_us: percentile(&sorted, 0.9),
        run_median_us: percentile(&run_micros, 0.5),
        prepare_median_us: percentile(&prepare_micros, 0.5),
        prepared_median_us: None,
        prepared_bind_median_us: None,
        total_rows,
        first_keys,
    })
}

/// One untimed warm pass then one timed pass, for a case that has to REBUILD
/// its state before each instance.
///
/// [`measure`] times the whole closure, which is right for a question and
/// wrong for a write: the reset that gives instance `i` the same empty
/// collection instance 0 had is setup, not the thing being measured. So the
/// closure here does both and reports the micros of the TIMED half itself,
/// and this function does nothing but collect them. `prepare_median_us` is
/// zero for the same reason it is zero in the `e4` arm -- nothing is parsed.
fn measure_write(mut once: impl FnMut(usize) -> R<(f64, Answer)>) -> R<CaseResult> {
    for i in 0..INSTANCES {
        once(i)?;
    }
    let mut micros = Vec::with_capacity(INSTANCES);
    let mut total_rows = 0u64;
    let mut first_keys = Vec::new();
    for i in 0..INSTANCES {
        let (wall, answer) = once(i)?;
        micros.push(wall);
        total_rows += answer.rows;
        if i == 0 {
            first_keys = answer.keys.clone();
        }
    }
    let mut sorted = micros;
    sorted.sort_by(f64::total_cmp);
    Ok(CaseResult {
        median_us: percentile(&sorted, 0.5),
        p90_us: percentile(&sorted, 0.9),
        run_median_us: percentile(&sorted, 0.5),
        prepare_median_us: 0.0,
        // A write case prepares nothing per instance; the prepared column is
        // a query's.
        prepared_median_us: None,
        prepared_bind_median_us: None,
        total_rows,
        first_keys,
    })
}

/// `|approx ∩ exact| / k`, on keys, for one query instance.
fn overlap_at_k(approx: &[String], exact: &[String], k: usize) -> f64 {
    let truth: HashSet<&str> = exact.iter().map(String::as_str).collect();
    let hits = approx.iter().filter(|key| truth.contains(key.as_str())).count();
    hits as f64 / k as f64
}

/// The mean of `overlap_at_k` over all fifty instances, run untimed.
fn mean_recall(
    mut approx: impl FnMut(usize) -> R<Answer>,
    mut exact: impl FnMut(usize) -> R<Answer>,
) -> R<f64> {
    let mut total = 0.0;
    for i in 0..INSTANCES {
        let a = approx(i)?;
        let e = exact(i)?;
        total += overlap_at_k(&a.keys, &e.keys, K);
    }
    Ok(total / INSTANCES as f64)
}

// ── the E4 arm ────────────────────────────────────────────────────────────

/// Index names, shared with the Postgres arm so the two reports' `stages`
/// blocks line up name for name. Postgres has no `place_emb_exact`; see
/// deviation 6.
const IX_TEXT: &str = "place_text";
const IX_BORN: &str = "place_born";
const IX_KIND: &str = "place_kind";
const IX_LOC: &str = "place_loc";
const IX_PLOT: &str = "place_plot";
const IX_EMB_EXACT: &str = "place_emb_exact";
const IX_EMB_ANN: &str = "place_emb_ann";
/// The three objects the §4.1 / §4.2 function battery names. They are new
/// with that battery, so a database an earlier pass built does not have them
/// and `provision_functions` adds them once on reopen.
const IX_BORN_TS: &str = "place_born_ts";
const IX_KIND_LOWER: &str = "place_kind_lower";
const IX_NAME: &str = "place_name";
/// The declared TIMESTAMPTZ column `fn_year_eq` and `fn_trunc_month_range`
/// range over: `born` (a yyyymmdd integer) as UTC microseconds.
const BORN_TS: &str = "born_ts";

pub struct E4Ctx {
    db: Database,
    place: CollectionId,
    /// The `related` edge type, once a `--graph` load has written it.
    related: Option<EdgeTypeId>,
    text: IndexId,
    born: IndexId,
    kind: IndexId,
    loc: IndexId,
    plot: IndexId,
    emb_exact: IndexId,
    emb_ann: IndexId,
    born_ts: IndexId,
    kind_lower: IndexId,
    name: IndexId,
}

/// Midnight UTC of a proleptic-Gregorian date, in microseconds since the
/// epoch. Howard Hinnant's `days_from_civil`, written out because this binary
/// takes no date dependency and the two SQL arms must fold the same literal
/// to the same integer.
fn micros_of(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400_000_000
}

/// The corpus's `born` (a yyyymmdd integer) as the instant the declared
/// `born_ts` column stores. Every `born` in `places-50000.jsonl` is a valid
/// date whose day is at most 28, so this conversion is total on the corpus
/// and a value it is not total on is an error, never a silent clamp.
fn born_ts_micros(born: i64) -> R<i64> {
    let (year, month, day) = (born / 10_000, ((born / 100) % 100) as u32, (born % 100) as u32);
    if !(1..=12).contains(&month) || !(1..=28).contains(&day) || !(1..=9999).contains(&year) {
        return Err(format!("born {born} is not a yyyymmdd date this battery can convert").into());
    }
    Ok(micros_of(year, month, day))
}

/// A half-open range over `born_ts`, which is what every §4.2 rewrite folds
/// to: `[start, end)`.
fn e4_born_ts_filter(index: IndexId, start: i64, end: i64) -> QueryFilter<'static> {
    QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::I64(start)),
            upper: Bound::Excluded(ScalarValue::I64(end)),
        },
    }
}

/// A text-key prefix range, which is what `LIKE 'abc%'` and
/// `starts_with(col, 'abc')` fold to.
fn e4_prefix_filter<'a>(index: IndexId, prefix: &'a str, successor: &'a str) -> QueryFilter<'a> {
    QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::Text(prefix)),
            upper: Bound::Excluded(ScalarValue::Text(successor)),
        },
    }
}

/// The smallest string above every string that starts with `prefix`.
fn prefix_successor(prefix: &str) -> R<String> {
    let mut bytes = prefix.as_bytes().to_vec();
    while let Some(last) = bytes.pop() {
        if last != 0xFF {
            bytes.push(last + 1);
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    Err(format!("`{prefix}` has no successor in byte order").into())
}

fn e4_kind_filter(index: IndexId, kind: &str) -> QueryFilter<'_> {
    QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Eq(ScalarValue::Text(kind)),
    }
}

fn e4_born_filter(index: IndexId, lower: i64, upper: i64) -> QueryFilter<'static> {
    QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::I64(lower)),
            upper: Bound::Included(ScalarValue::I64(upper)),
        },
    }
}

fn e4_radius_filter(index: IndexId, center: Point, radius_metres: f64) -> QueryFilter<'static> {
    QueryFilter::Point {
        index,
        predicate: PointFilter::Radius {
            center,
            radius_metres,
        },
    }
}

fn e4_text_filter(index: IndexId, query: &str, matching: TextMatch) -> QueryFilter<'_> {
    QueryFilter::Text {
        index,
        query,
        matching,
    }
}

fn e4_geometry_filter(index: IndexId, predicate: GeometryFilter) -> QueryFilter<'static> {
    QueryFilter::Geometry { index, predicate }
}

/// Prepare, page to exhaustion, and translate every returned `EntityId` back
/// to the corpus key through the load-time vector of deviation 2.
fn e4_run(
    ctx: &E4Ctx,
    keys: &[String],
    filters: &[QueryFilter<'_>],
    order: QueryOrder<'_>,
    total_limit: Option<usize>,
) -> R<Answer> {
    let mut prepared = ctx.db.prepare_query(QueryRequest {
        collection: ctx.place,
        filters,
        order,
        projection: Projection::Ids,
        total_limit,
        driver: CandidateDriver::Auto,
    })?;
    let mut answer = Answer::default();
    loop {
        let page = prepared.next_page(PAGE, QueryBudget::unlimited(), || false)?;
        for row in &page.rows {
            let ordinal = (row.id.sequence - 1) as usize;
            let key = keys
                .get(ordinal)
                .ok_or("entity sequence falls outside the corpus's key vector")?;
            if dumping() || answer.keys.len() < FIRST_KEYS {
                answer.keys.push(key.clone());
            }
            answer.rows += 1;
        }
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    Ok(answer)
}

/// `SELECT upper(name), length(desc) ... WHERE <range>` through the direct
/// API: the same walk `born_range` runs, with the two §4.1 row functions
/// computed over the values the page projected.
///
/// The functions are computed and dropped rather than returned, exactly as
/// the two SQL arms compute and return them: what the three arms compare is
/// the ROW SET, and what this arm has to pay for is the projection plus the
/// per-row work. `lang/tests/sql_functions.rs` is where the VALUES are checked
/// against Rust's own computation.
fn e4_project_strings(ctx: &E4Ctx, keys: &[String], filters: &[QueryFilter<'_>]) -> R<Answer> {
    let fields = ["name", "desc"];
    let mut prepared = ctx.db.prepare_query(QueryRequest {
        collection: ctx.place,
        filters,
        order: QueryOrder::Driver,
        projection: Projection::Fields(&fields),
        total_limit: None,
        driver: CandidateDriver::Auto,
    })?;
    let mut answer = Answer::default();
    let mut sink = 0u64;
    loop {
        let page = prepared.next_page(PAGE, QueryBudget::unlimited(), || false)?;
        for row in &page.rows {
            let ordinal = (row.id.sequence - 1) as usize;
            let key = keys
                .get(ordinal)
                .ok_or("entity sequence falls outside the corpus's key vector")?;
            for (at, (_, value)) in row.projected.iter().enumerate() {
                let ProjectedValue::Value(Value::String(text)) = value else {
                    continue;
                };
                sink = sink.wrapping_add(if at == 0 {
                    text.to_uppercase().len() as u64
                } else {
                    text.chars().count() as u64
                });
            }
            if dumping() || answer.keys.len() < FIRST_KEYS {
                answer.keys.push(key.clone());
            }
            answer.rows += 1;
        }
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    // The compiler must not delete the work the case exists to measure.
    std::hint::black_box(sink);
    Ok(answer)
}

/// Prepare an aggregate, page every group, and write one line per group.
fn e4_agg_run(
    ctx: &E4Ctx,
    filters: &[QueryFilter<'_>],
    group: Option<GroupKey<'_>>,
    accumulators: &[Accumulator<'_>],
    having: &[GroupPredicate],
    fields: &[&str],
) -> R<Answer> {
    let mut prepared = ctx.db.prepare_aggregate(AggregateRequest {
        collection: ctx.place,
        filters,
        group,
        accumulators,
        having,
        order: GroupOrder::Key,
        driver: CandidateDriver::Auto,
        total_limit: None,
    })?;
    let mut answer = Answer::default();
    loop {
        let page = prepared.next_page(PAGE, QueryBudget::unlimited(), || false)?;
        for row in &page.groups {
            let key = match &row.key {
                None => String::new(),
                Some(OwnedScalarValue::Text(text)) => text.clone(),
                Some(OwnedScalarValue::I64(value)) => value.to_string(),
                Some(OwnedScalarValue::F64(value)) => format!("{value}"),
                Some(OwnedScalarValue::Bool(value)) => value.to_string(),
                Some(OwnedScalarValue::Nullish) => "NULL".to_owned(),
            };
            let mut numbers = Vec::with_capacity(row.values.len());
            for (at, name) in fields.iter().enumerate() {
                let value = row.values.get(at).ok_or("an aggregate lost an accumulator")?;
                numbers.push((*name, agg_number(value)?));
            }
            answer.push(agg_line(&key, &numbers));
        }
        if page.done || page.groups.is_empty() {
            break;
        }
    }
    Ok(answer)
}

/// One accumulator's value as the whole number every arm compares. `avg` is
/// floored here for the reason `agg_line` gives.
fn agg_number(value: &AggValue) -> R<i64> {
    Ok(match value {
        AggValue::Count(n) => *n as i64,
        AggValue::I64(v) => *v,
        AggValue::F64(v) => v.floor() as i64,
        other => return Err(format!("battle50k: an aggregate produced {other:?}").into()),
    })
}

/// The six aggregate cases, as E4's API spells them.
fn e4_agg_case(ctx: &E4Ctx, corpus: &Corpus, q: &Queries, name: &str, i: usize) -> R<Answer> {
    let _ = corpus;
    let count_star = [Accumulator {
        function: AggregateFn::CountStar,
        input: None,
    }];
    match name {
        // count(*) with no filter: one group, and the key-order driver.
        "agg_count_all" => e4_agg_run(ctx, &[], None, &count_star, &[], &["n"]),
        // GROUP BY an indexed Text column: streaming, off the posting.
        "agg_count_kind" => e4_agg_run(
            ctx,
            &[],
            Some(GroupKey::Index(ctx.kind)),
            &count_star,
            &[],
            &["n"],
        ),
        // Five accumulators over `born`, which is NOT the driving index, so
        // each of them reads the row; HAVING filters the finished groups.
        "agg_sum_born_by_kind" => {
            let accumulators = [
                Accumulator {
                    function: AggregateFn::CountStar,
                    input: None,
                },
                Accumulator {
                    function: AggregateFn::Sum,
                    input: Some(AggregateInput::Index(ctx.born)),
                },
                Accumulator {
                    function: AggregateFn::Min,
                    input: Some(AggregateInput::Index(ctx.born)),
                },
                Accumulator {
                    function: AggregateFn::Max,
                    input: Some(AggregateInput::Index(ctx.born)),
                },
                Accumulator {
                    function: AggregateFn::Avg,
                    input: Some(AggregateInput::Index(ctx.born)),
                },
            ];
            let having = [GroupPredicate {
                accumulator: 0,
                op: GroupCmp::Gt,
                value: 100.0,
            }];
            e4_agg_run(
                ctx,
                &[],
                Some(GroupKey::Index(ctx.kind)),
                &accumulators,
                &having,
                &["n", "s", "lo", "hi", "mean"],
            )
        }
        // DISTINCT: a group with no accumulators.
        "agg_distinct_kind" => e4_agg_run(ctx, &[], Some(GroupKey::Index(ctx.kind)), &[], &[], &[]),
        // The radius drives, so the group key is not the walk's own value and
        // the shape is hashed.
        "agg_count_radius_by_kind" => e4_agg_run(
            ctx,
            &[e4_radius_filter(ctx.loc, q.radius_centre(i)?, q.radius_metres(i))],
            Some(GroupKey::Index(ctx.kind)),
            &count_star,
            &[],
            &["n"],
        ),
        // The expression group key, computed index-side from the Int posting.
        "agg_born_decade" => e4_agg_run(
            ctx,
            &[],
            Some(GroupKey::IndexDiv {
                index: ctx.born,
                divisor: 10_000,
            }),
            &count_star,
            &[],
            &["n"],
        ),
        other => Err(format!("battle50k: no E4 spelling for aggregate case `{other}`").into()),
    }
}

/// One case, one query instance, as E4's API spells it. `hybrid_blend_10`
/// rides `QueryOrder::Score` (deviation 5 names what still differs).
fn e4_case(ctx: &E4Ctx, corpus: &Corpus, q: &Queries, name: &str, i: usize) -> R<Answer> {
    let keys = &corpus.keys;
    let kind = &corpus.kinds[i % KINDS];

    if is_aggregate_case(name) {
        return e4_agg_case(ctx, corpus, q, name, i);
    }

    // Approximate sweep points (`vec_ann_10@ef<N>`, `vec_ann_10_kind@ef<N>`):
    // dispatched here rather than as one match arm per `EF_SWEEP` value.
    if let Some(sweep) = parse_approx_sweep(name) {
        let ef = sweep
            .ef
            .ok_or_else(|| format!("battle50k: `{name}` has no ef=... suffix for the E4 arm"))?;
        let filters: Vec<QueryFilter> = if sweep.by_kind {
            vec![e4_kind_filter(ctx.kind, kind)]
        } else {
            Vec::new()
        };
        return e4_run(
            ctx,
            keys,
            &filters,
            QueryOrder::ApproximateVector {
                index: ctx.emb_ann,
                query: &q.vectors[i],
                metric: VectorMetric::Cosine,
                ef,
            },
            Some(K),
        );
    }

    match name {
        // ── filters: every matching row, in the driver's own walk order ──
        "pt_radius" => e4_run(
            ctx,
            keys,
            &[e4_radius_filter(ctx.loc, q.radius_centre(i)?, q.radius_metres(i))],
            QueryOrder::Driver,
            None,
        ),
        "pt_bbox" => e4_run(
            ctx,
            keys,
            &[QueryFilter::Point {
                index: ctx.loc,
                predicate: PointFilter::Bbox(q.bounds(i)?),
            }],
            QueryOrder::Driver,
            None,
        ),
        "plot_within_box" => e4_run(
            ctx,
            keys,
            &[e4_geometry_filter(
                ctx.plot,
                GeometryFilter::Within(q.box_polygon(i)),
            )],
            QueryOrder::Driver,
            None,
        ),
        "plot_contains_pt" => e4_run(
            ctx,
            keys,
            &[e4_geometry_filter(
                ctx.plot,
                GeometryFilter::Contains(Geom::Point(q.points[i][0], q.points[i][1])),
            )],
            QueryOrder::Driver,
            None,
        ),
        "plot_intersects" => e4_run(
            ctx,
            keys,
            &[e4_geometry_filter(
                ctx.plot,
                GeometryFilter::Intersects(q.polygons[i].clone()),
            )],
            QueryOrder::Driver,
            None,
        ),
        "plot_dwithin_1km" => e4_run(
            ctx,
            keys,
            &[e4_geometry_filter(
                ctx.plot,
                GeometryFilter::DWithin {
                    geometry: Geom::Point(q.points[i][0], q.points[i][1]),
                    metres: 1_000.0,
                },
            )],
            QueryOrder::Driver,
            None,
        ),
        "plot_vs_poly_within" => e4_run(
            ctx,
            keys,
            &[e4_geometry_filter(
                ctx.plot,
                GeometryFilter::Within(q.polygons[i].clone()),
            )],
            QueryOrder::Driver,
            None,
        ),
        "text_one" => e4_run(
            ctx,
            keys,
            &[e4_text_filter(ctx.text, &q.terms[i], TextMatch::Any)],
            QueryOrder::Driver,
            None,
        ),
        "text_two" => {
            let pair = format!("{} {}", q.terms[i], q.terms[(i + 1) % INSTANCES]);
            e4_run(
                ctx,
                keys,
                &[e4_text_filter(ctx.text, &pair, TextMatch::All)],
                QueryOrder::Driver,
                None,
            )
        }
        "text_and_kind" => e4_run(
            ctx,
            keys,
            &[
                e4_text_filter(ctx.text, &q.terms[i], TextMatch::Any),
                e4_kind_filter(ctx.kind, kind),
            ],
            QueryOrder::Driver,
            None,
        ),
        "born_range" => {
            let (lower, upper) = q.born_range(i);
            e4_run(
                ctx,
                keys,
                &[e4_born_filter(ctx.born, lower, upper)],
                QueryOrder::Driver,
                None,
            )
        }
        "kind_eq" => e4_run(
            ctx,
            keys,
            &[e4_kind_filter(ctx.kind, kind)],
            QueryOrder::Driver,
            None,
        ),
        "radius_and_born" => {
            let (lower, upper) = q.born_range(i);
            e4_run(
                ctx,
                keys,
                &[
                    e4_radius_filter(ctx.loc, q.radius_centre(i)?, q.radius_metres(i)),
                    e4_born_filter(ctx.born, lower, upper),
                ],
                QueryOrder::Driver,
                None,
            )
        }

        // ── boolean: one membership set per case (QL_CONTRACT §3) ────────
        "bool_kind_in3" => {
            let leaves = [
                e4_kind_filter(ctx.kind, &corpus.kinds[i % KINDS]),
                e4_kind_filter(ctx.kind, &corpus.kinds[(i + 1) % KINDS]),
                e4_kind_filter(ctx.kind, &corpus.kinds[(i + 2) % KINDS]),
            ];
            e4_run(
                ctx,
                keys,
                &[QueryFilter::Any(&leaves)],
                QueryOrder::Driver,
                None,
            )
        }
        // ── the function battery (QL_CONTRACT §4.1, §4.2) ───────────────
        // The direct API is where the rewrite ENDS UP, so this arm writes
        // the range the two SQL arms fold their function into. That is the
        // point of the comparison: if the folding is wrong, the SQL arms
        // return a different row set than the range written by hand here.
        "fn_year_eq" => {
            let year = q.fn_year(i);
            e4_run(
                ctx,
                keys,
                &[e4_born_ts_filter(
                    ctx.born_ts,
                    micros_of(year, 1, 1),
                    micros_of(year + 1, 1, 1),
                )],
                QueryOrder::Driver,
                None,
            )
        }
        "bool_born_or_kind" => {
            let (lower, upper) = q.born_range(i);
            let leaves = [
                e4_born_filter(ctx.born, lower, upper),
                e4_kind_filter(ctx.kind, kind),
            ];
            e4_run(
                ctx,
                keys,
                &[QueryFilter::Any(&leaves)],
                QueryOrder::Driver,
                None,
            )
        }
        "bool_not_kind" => {
            let equality = e4_kind_filter(ctx.kind, kind);
            e4_run(
                ctx,
                keys,
                &[QueryFilter::Not(&equality)],
                QueryOrder::Driver,
                None,
            )
        }
        "bool_radius_or_radius" => {
            let other = (i + 1) % INSTANCES;
            let leaves = [
                e4_radius_filter(ctx.loc, q.radius_centre(i)?, q.radius_metres(i)),
                e4_radius_filter(ctx.loc, q.radius_centre(other)?, q.radius_metres(other)),
            ];
            e4_run(
                ctx,
                keys,
                &[QueryFilter::Any(&leaves)],
                QueryOrder::Driver,
                None,
            )
        }
        "bool_not_null_born" => {
            let nullish = QueryFilter::Scalar {
                index: ctx.born,
                predicate: ScalarFilter::IsNull,
            };
            e4_run(
                ctx,
                keys,
                &[QueryFilter::Not(&nullish)],
                QueryOrder::Driver,
                None,
            )
        }
        "bool_exists_related" => {
            let related = ctx.related.ok_or(GRAPH_NEEDS_LOAD)?;
            let sources = ctx.db.edge_endpoints(
                ctx.place,
                GraphContextId::BASE,
                related,
                Direction::Outgoing,
                usize::MAX,
                usize::MAX,
                QueryBudget::unlimited(),
                || false,
            )?;
            e4_run(
                ctx,
                keys,
                &[QueryFilter::Ids(&sources)],
                QueryOrder::Driver,
                None,
            )
        }
        "fn_trunc_month_range" => {
            // `date_trunc('month', t) BETWEEN 'Y-01-01' AND 'Y-06-01'` is the
            // half-open range from the first month's start to the month AFTER
            // the last one's -- one range, not six.
            let year = q.fn_year(i);
            e4_run(
                ctx,
                keys,
                &[e4_born_ts_filter(
                    ctx.born_ts,
                    micros_of(year, 1, 1),
                    micros_of(year, 7, 1),
                )],
                QueryOrder::Driver,
                None,
            )
        }
        "fn_lower_eq" => {
            let folded = kind.to_lowercase();
            e4_run(
                ctx,
                keys,
                &[e4_kind_filter(ctx.kind_lower, &folded)],
                QueryOrder::Driver,
                None,
            )
        }
        "fn_like_prefix" => {
            let prefix = q.name_prefix(i);
            let successor = prefix_successor(prefix)?;
            e4_run(
                ctx,
                keys,
                &[e4_prefix_filter(ctx.name, prefix, &successor)],
                QueryOrder::Driver,
                None,
            )
        }
        // A projection-only case: the WHERE is an ordinary Tier-1 range and
        // every function is in the SELECT list, so what this measures is the
        // ROW-function cost on top of a walk the battery already times
        // (`born_range`).
        "fn_project_strings" => {
            let (lower, upper) = q.born_range(i);
            e4_project_strings(ctx, keys, &[e4_born_filter(ctx.born, lower, upper)])
        }

        // ── ranked: ten rows, one order per request ──────────────────────
        "knn_10" => e4_run(
            ctx,
            keys,
            &[],
            QueryOrder::Distance {
                index: ctx.loc,
                center: q.point(i)?,
                direction: SortDirection::Ascending,
            },
            Some(K),
        ),
        "knn_10_kind" => e4_run(
            ctx,
            keys,
            &[e4_kind_filter(ctx.kind, kind)],
            QueryOrder::Distance {
                index: ctx.loc,
                center: q.point(i)?,
                direction: SortDirection::Ascending,
            },
            Some(K),
        ),
        "text_top10" => e4_run(
            ctx,
            keys,
            &[e4_text_filter(ctx.text, &q.terms[i], TextMatch::Any)],
            QueryOrder::Bm25 {
                index: ctx.text,
                query: &q.terms[i],
                matching: TextMatch::Any,
            },
            Some(K),
        ),
        "vec_exact_10" => e4_run(
            ctx,
            keys,
            &[],
            QueryOrder::ExactVector {
                index: ctx.emb_exact,
                query: &q.vectors[i],
                metric: VectorMetric::Cosine,
            },
            Some(K),
        ),
        "vec_exact_radius" => e4_run(
            ctx,
            keys,
            &[e4_radius_filter(ctx.loc, q.radius_centre(i)?, q.radius_metres(i))],
            QueryOrder::ExactVector {
                index: ctx.emb_exact,
                query: &q.vectors[i],
                metric: VectorMetric::Cosine,
            },
            Some(K),
        ),
        "hybrid_10" => e4_run(
            ctx,
            keys,
            &[
                e4_text_filter(ctx.text, &q.terms[i], TextMatch::Any),
                e4_radius_filter(ctx.loc, q.radius_centre(i)?, q.radius_metres(i)),
            ],
            QueryOrder::ExactVector {
                index: ctx.emb_exact,
                query: &q.vectors[i],
                metric: VectorMetric::Cosine,
            },
            Some(K),
        ),

        // ── the exact twin the approximate sweep's recall is measured on ─
        // (`vec_ann_10`'s own twin is `vec_exact_10`, already a case above;
        // `vec_ann_10` and `vec_ann_10_kind` are no longer match arms here
        // themselves — every point of their sweep goes through
        // `parse_approx_sweep` at the top of this function.)
        "vec_ann_10_kind:exact" => e4_run(
            ctx,
            keys,
            &[e4_kind_filter(ctx.kind, kind)],
            QueryOrder::ExactVector {
                index: ctx.emb_exact,
                query: &q.vectors[i],
                metric: VectorMetric::Cosine,
            },
            Some(K),
        ),

        // The blend, as `QueryOrder::Score` spells it: 0.5 * BM25 + 0.5 * cos.
        // `VectorSimilarity` under Cosine is `-(1 - cos)`, so `1 + leaf` is the
        // cosine itself, the same quantity Postgres writes as `1 - (emb <=> v)`.
        // The text halves differ by formula (BM25 against ts_rank_cd; deviation
        // 7), so this case is compared on top-ten overlap, never on order.
        "hybrid_blend_10" => {
            let half = ScoreExpr::Lit(0.5);
            let one = ScoreExpr::Lit(1.0);
            let bm25 = ScoreExpr::Bm25 {
                index: ctx.text,
                query: &q.terms[i],
                matching: TextMatch::Any,
            };
            let sim = ScoreExpr::VectorSimilarity {
                index: ctx.emb_exact,
                query: &q.vectors[i],
                metric: VectorMetric::Cosine,
            };
            let cos = ScoreExpr::Add(&one, &sim);
            let text_term = ScoreExpr::Mul(&half, &bm25);
            let vec_term = ScoreExpr::Mul(&half, &cos);
            let expr = ScoreExpr::Add(&text_term, &vec_term);
            e4_run(
                ctx,
                keys,
                &[
                    e4_text_filter(ctx.text, &q.terms[i], TextMatch::Any),
                    e4_radius_filter(ctx.loc, q.radius_centre(i)?, q.radius_metres(i)),
                ],
                QueryOrder::Score {
                    expr: &expr,
                    direction: SortDirection::Descending,
                },
                Some(K),
            )
        }

        // ── the graph cases (GRAPH_CONTRACT 4.2, 4.3) ────────────────────
        //
        // Every one seeds at `seed_key(i)` -- the row nearest `points[i]`,
        // resolved once, outside the timed pass, and handed to all three
        // arms as a key. The key-to-id lookup IS inside the timed pass,
        // because the other two arms pay for it too: the `e4-sql` arm's
        // pattern resolves the same key while it compiles, and Postgres
        // matches `related.source = <key>` in the join.
        "graph_2hop" | "graph_2hop_weight" | "graph_2hop_born" => {
            let (seed, prepare_us) = e4_graph_seed(ctx, q.seed_key(i)?)?;
            let related = ctx.related.ok_or(GRAPH_NEEDS_LOAD)?;
            let edge_where = [EdgePredicate {
                property: "weight",
                op: Cmp::Gt,
                value: ScalarValue::F64(0.5),
            }];
            let (born_lower, born_upper) = q.born_range(i);
            let node_where = [e4_born_filter(ctx.born, born_lower, born_upper)];
            let filters = [QueryFilter::Graph(BfsRequest {
                seed,
                direction: Direction::Outgoing,
                context: GraphContextId::BASE,
                edge_type: Some(related),
                min_depth: 1,
                max_depth: 2,
                include_seed: false,
                max_visited: 1 << 16,
                max_edges: 1 << 18,
                result_limit: 1 << 16,
                edge_where: if name == "graph_2hop_weight" {
                    &edge_where
                } else {
                    &[]
                },
                node_where: if name == "graph_2hop_born" {
                    &node_where
                } else {
                    &[]
                },
            })];
            let mut answer = e4_run(ctx, keys, &filters, QueryOrder::Driver, None)?;
            answer.prepare_us = prepare_us;
            Ok(answer)
        }
        "graph_1hop_weight_top10" => {
            let (seed, prepare_us) = e4_graph_seed(ctx, q.seed_key(i)?)?;
            let related = ctx.related.ok_or(GRAPH_NEEDS_LOAD)?;
            let filters = [QueryFilter::Graph(BfsRequest {
                seed,
                direction: Direction::Incoming,
                context: GraphContextId::BASE,
                edge_type: Some(related),
                min_depth: 1,
                max_depth: 1,
                include_seed: false,
                max_visited: 1 << 16,
                max_edges: 1 << 18,
                result_limit: 1 << 16,
                edge_where: &[],
                node_where: &[],
            })];
            let mut answer = e4_run(
                ctx,
                keys,
                &filters,
                QueryOrder::Edge {
                    property: "weight",
                    direction: SortDirection::Descending,
                },
                Some(K),
            )?;
            answer.prepare_us = prepare_us;
            Ok(answer)
        }
        other => Err(format!("battle50k: no E4 spelling for case `{other}`").into()),
    }
}

/// What a graph case says when the database it is pointed at has no
/// `related` edges. The cases are skipped without `--graph`; this is the
/// message for a run that got past that by naming one directly.
const GRAPH_NEEDS_LOAD: &str =
    "this case needs the `related` edge set: run the arm once with --graph";

/// The entity a graph case seeds at, from the key every arm shares, and what
/// that lookup cost in microseconds.
///
/// The cost is reported as the case's PREPARE, not as part of its run, so the
/// three arms compare like with like: the `e4-sql` arm's pattern resolves the
/// same key while it compiles (inside its own `prepare_us`), and the Postgres
/// arm matches `related.source = <key>` inside its statement. Without this
/// split the API arm would carry a point-get the SQL arm's run does not, and
/// the two run medians would differ by exactly that.
// ── the vector write cases, E4 and e4-sql ─────────────────────────────────

/// Where a write case builds its scratch database: a SIBLING of `--db-dir`,
/// never inside it.
///
/// Two reasons, both of them rules rather than taste. The 50,000-row corpus
/// is REUSED across runs and across workers, so a case that wrote into it
/// would change what every later `--reuse` measures; and `disk_bytes` is the
/// sum of the files under `--db-dir`, so a scratch collection living there
/// would be counted as corpus. The directory is removed when the case ends.
pub fn e4_bulk_dir(db_dir: &Path) -> PathBuf {
    let mut name = db_dir.file_name().unwrap_or_default().to_os_string();
    name.push("-bulk");
    db_dir.with_file_name(name)
}

/// One scratch database with one empty collection, and the two vector index
/// families made READY on it BEFORE any row lands when `indexed`.
///
/// An index created on an empty collection reaches READY in one build step
/// that walks nothing, so what the rows below meet is a LIVE index doing
/// per-row maintenance -- not a late build over an already-written corpus,
/// which is what every `index:*` stage of the load measures instead.
fn e4_bulk_reset(dir: &Path, indexed: bool) -> R<(Database, CollectionId)> {
    let _ = fs::remove_dir_all(dir);
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut db = Database::create(
        dir,
        Config {
            budget_bytes: CACHE_BYTES,
            io: IoMode::Buffered,
            sync: SyncMode::Normal,
        },
    )?;
    let collection = db.create_collection_declared(
        VEC_BULK_OBJECT,
        vec![
            ("key".into(), Kind::Text),
            ("emb".into(), Kind::Vector(DIM)),
        ],
        Vec::new(),
        CollectionOptions::default(),
    )?;
    db.commit()?;
    if indexed {
        let exact = db.create_exact_vector_index(collection, "bulk_emb_exact", "emb")?;
        db.commit()?;
        db.build_index_to_ready(exact, BATCH)?;
        db.commit()?;
        let ann = db.create_quantized_vector_index(collection, "bulk_emb_ann", "emb")?;
        db.commit()?;
        db.build_index_to_ready(ann, BATCH)?;
        db.commit()?;
    }
    Ok((db, collection))
}

/// Reset (untimed), then TIME one batch of [`VEC_BULK_ROWS`] puts closed by
/// one commit. `begin_bulk`/`end_bulk` is the scope OPS_CONTRACT §7 names:
/// the outermost close commits, so the whole thousand is one durability
/// point rather than a thousand.
fn e4_bulk_write(dir: &Path, corpus: &Corpus, name: &str, i: usize) -> R<(f64, Answer)> {
    let batch: Vec<(String, &[f32])> = (0..VEC_BULK_ROWS)
        .map(|at| bulk_row(corpus, i, at))
        .collect::<R<Vec<_>>>()?;
    let (mut db, collection) = e4_bulk_reset(dir, write_case_is_indexed(name))?;
    let started = Instant::now();
    db.begin_bulk()?;
    for (key, emb) in &batch {
        db.put(collection, key, &json!({"key": key, "emb": emb}))?;
    }
    db.end_bulk()?;
    let micros = started.elapsed().as_secs_f64() * 1e6;
    let mut answer = Answer::default();
    for (key, _) in &batch {
        answer.push(key.clone());
    }
    drop(db);
    Ok((micros, answer))
}

/// The same batch asked in SQL, one `INSERT` per row through the same engine.
/// The vector is a bound `Param::Vector`, not a literal, so the measured
/// statement is the one an application would issue.
fn e4sql_bulk_write(dir: &Path, corpus: &Corpus, name: &str, i: usize) -> R<(f64, Answer)> {
    let batch: Vec<(String, Vec<f32>)> = (0..VEC_BULK_ROWS)
        .map(|at| bulk_row(corpus, i, at).map(|(key, emb)| (key, emb.to_vec())))
        .collect::<R<Vec<_>>>()?;
    let (mut db, _) = e4_bulk_reset(dir, write_case_is_indexed(name))?;
    let sql = format!("INSERT INTO {VEC_BULK_OBJECT} (key, emb) VALUES ($1, $2)");
    let started = Instant::now();
    db.begin_bulk()?;
    for (key, emb) in &batch {
        db.sql(
            &sql,
            &[Param::Text(key.clone()), Param::Vector(emb.clone())],
        )?;
    }
    db.end_bulk()?;
    let micros = started.elapsed().as_secs_f64() * 1e6;
    let mut answer = Answer::default();
    for (key, _) in &batch {
        answer.push(key.clone());
    }
    drop(db);
    Ok((micros, answer))
}

fn e4_graph_seed(ctx: &E4Ctx, key: &str) -> R<(EntityId, f64)> {
    let at = Instant::now();
    let id = ctx
        .db
        .get(ctx.place, key)?
        .ok_or("the graph seed key is not in the database")?
        .id;
    Ok((id, at.elapsed().as_secs_f64() * 1e6))
}

// ── the `related` edge set (--graph) ──────────────────────────────────────

/// One edge of the `related` graph: source row, destination row, and the two
/// properties every arm stores.
#[derive(Clone, Copy, Debug)]
pub struct Related {
    pub source: usize,
    pub destination: usize,
    pub weight: f64,
    pub since: i64,
}

/// The weight `docs/core/GRAPH_CONTRACT.md`'s battery cases rank and prune by:
/// `1 / (1 + metres/1000)`, so an edge a kilometre long weighs 0.5 and a
/// coincident pair weighs 1.0.
fn related_weight(metres: f64) -> f64 {
    1.0 / (1.0 + metres / 1_000.0)
}

/// Every row's three nearest OTHER rows by `loc`, ascending by distance,
/// ties broken by row ordinal.
///
/// DEVIATION, stated rather than hidden. The brief asks for this to be
/// computed from E4's point index at load. It is computed here instead, from
/// the corpus's own coordinates, with `wgs84_distance_metres` -- the very
/// function `QueryOrder::Distance` ranks by and `NearestWalk` stops on. The
/// reason is the comparison: the Postgres arm has no access to E4's index, so
/// an index-driven neighbour list there would be PostGIS's `<->` on
/// `geography`, and two rows whose distances differ in the last bits would be
/// ordered differently by the two. The battery would then compare two
/// different graphs and call the disagreement a bug in the engine.
/// `graph_neighbours_agree_with_the_point_index` checks a sample of this list
/// against the point index's own answer at load time, so the claim that they
/// are the same list is measured, not assumed.
///
/// A longitude/latitude grid makes it one pass rather than 2.5 billion
/// distance computations: the cell is sized from the corpus's own extent so
/// a cell holds about one row, and a row's candidates are its own cell and
/// the rings around it, grown until the ring's own distance exceeds the
/// third-best found.
fn related_edges(corpus: &Corpus) -> Vec<Related> {
    let n = corpus.rows.len();
    let (mut west, mut east, mut south, mut north) =
        (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for row in &corpus.rows {
        west = west.min(row.lon);
        east = east.max(row.lon);
        south = south.min(row.lat);
        north = north.max(row.lat);
    }
    // About one row per cell. A cell smaller than this would cost more ring
    // steps than it saves; a cell larger would put thousands of rows in each.
    let span = ((east - west).max(1e-6) * (north - south).max(1e-6) / n.max(1) as f64).sqrt();
    let cell = span.max(1e-5);
    // The shortest a cell's side can be, in metres: a degree of latitude is
    // never shorter than 110,574 m, and a degree of longitude at the
    // corpus's highest latitude is 111,320*cos(lat).
    let worst_lat = south.abs().max(north.abs()).min(89.0).to_radians();
    let metres_per_cell = cell * 110_574.0_f64.min(111_320.0 * worst_lat.cos()).max(1.0);
    let cell_of = |lon: f64, lat: f64| -> (i32, i32) {
        ((lon / cell).floor() as i32, (lat / cell).floor() as i32)
    };
    let mut grid: std::collections::HashMap<(i32, i32), Vec<usize>> =
        std::collections::HashMap::with_capacity(n * 2);
    for (i, row) in corpus.rows.iter().enumerate() {
        grid.entry(cell_of(row.lon, row.lat)).or_default().push(i);
    }
    let mut out = Vec::with_capacity(n * RELATED_DEGREE);
    let mut best: Vec<(f64, usize)> = Vec::new();
    for i in 0..n {
        let row = &corpus.rows[i];
        let here = Point::new(row.lon, row.lat).expect("a corpus point is valid");
        let (cx, cy) = cell_of(row.lon, row.lat);
        best.clear();
        let mut ring = 0i32;
        loop {
            for dx in -ring..=ring {
                for dy in -ring..=ring {
                    // Only the ring's own shell; the inside was done already.
                    if ring > 0 && dx.abs() != ring && dy.abs() != ring {
                        continue;
                    }
                    let Some(bucket) = grid.get(&(cx + dx, cy + dy)) else {
                        continue;
                    };
                    for j in bucket {
                        if *j == i {
                            continue;
                        }
                        let other = &corpus.rows[*j];
                        let there =
                            Point::new(other.lon, other.lat).expect("a corpus point is valid");
                        best.push((wgs84_distance_metres(here, there), *j));
                    }
                }
            }
            best.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            best.dedup_by_key(|entry| entry.1);
            best.truncate(RELATED_DEGREE);
            // Everything within `ring` whole cells of this one has been seen,
            // so a row further out than that cannot beat the third-best.
            let covered = f64::from(ring) * metres_per_cell;
            if best.len() == RELATED_DEGREE && best[RELATED_DEGREE - 1].0 <= covered {
                break;
            }
            // The corpus is finite: a ring wider than its whole extent has
            // nothing left to find.
            if f64::from(ring) * cell > (east - west) + (north - south) {
                break;
            }
            ring += 1;
        }
        for (metres, j) in &best {
            out.push(Related {
                source: i,
                destination: *j,
                weight: related_weight(*metres),
                // The SOURCE row's `born`, so an edge's `since` is a property
                // of the relationship's origin and both arms store the same
                // number.
                since: row.born,
            });
        }
    }
    out
}

/// Check a sample of [`related_edges`] against E4's own point index, so the
/// deviation above is a measured claim.
fn graph_neighbours_agree_with_the_point_index(
    ctx: &E4Ctx,
    corpus: &Corpus,
    edges: &[Related],
    sample: usize,
) -> R<()> {
    let by_source: std::collections::HashMap<usize, Vec<usize>> =
        edges.iter().fold(std::collections::HashMap::new(), |mut map, edge| {
            map.entry(edge.source).or_default().push(edge.destination);
            map
        });
    for i in 0..sample.min(corpus.rows.len()) {
        let row = &corpus.rows[i];
        let centre = Point::new(row.lon, row.lat)?;
        let mut prepared = ctx.db.prepare_query(QueryRequest {
            collection: ctx.place,
            filters: &[],
            order: QueryOrder::Distance {
                index: ctx.loc,
                center: centre,
                direction: SortDirection::Ascending,
            },
            projection: Projection::Ids,
            total_limit: Some(RELATED_DEGREE + 1),
            driver: CandidateDriver::Auto,
        })?;
        let page = prepared.next_page(PAGE, QueryBudget::unlimited(), || false)?;
        let index_answer: Vec<usize> = page
            .rows
            .iter()
            .map(|r| (r.id.sequence - 1) as usize)
            .filter(|ordinal| *ordinal != i)
            .take(RELATED_DEGREE)
            .collect();
        let mut mine = by_source.get(&i).cloned().unwrap_or_default();
        let mut theirs = index_answer;
        mine.sort_unstable();
        theirs.sort_unstable();
        if mine != theirs {
            return Err(format!(
                "row {i}: the grid's three nearest {mine:?} are not the point index's {theirs:?}"
            )
            .into());
        }
    }
    Ok(())
}

/// Write the `related` edges into an E4 database, as a timed stage.
fn load_graph_e4(ctx: &mut E4Ctx, corpus: &Corpus, edges: &[Related]) -> R<Value> {
    let at = Instant::now();
    ctx.db.enable_graph()?;
    ctx.db.commit()?;
    let related = match ctx.db.edge_type(RELATED)? {
        Some(id) => id,
        None => {
            let id = ctx.db.create_edge_type(RELATED)?;
            ctx.db.commit()?;
            id
        }
    };
    let ids: Vec<EntityId> = (0..corpus.rows.len())
        .map(|i| {
            ctx.db
                .get(ctx.place, &corpus.rows[i].key)
                .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })
                .and_then(|row| {
                    row.map(|row| row.id)
                        .ok_or_else(|| "a corpus row is missing from the database".into())
                })
        })
        .collect::<R<Vec<_>>>()?;
    // One `link_many` per commit, so the arm's transaction boundary and its
    // batch boundary are the same 256 the row load uses. Inside a batch the
    // engine proves each DISTINCT endpoint once and writes the two keyspaces
    // as two ascending runs instead of alternating between them per edge
    // (`core/engine/src/index/graph/mod.rs` `link_many`); nothing about the
    // commit cadence, the edge set or the properties changes.
    let mut batch: Vec<NewEdge> = Vec::with_capacity(BATCH);
    for edge in edges {
        batch.push(NewEdge {
            source: ids[edge.source],
            destination: ids[edge.destination],
            properties: json!({"weight": edge.weight, "since": edge.since}),
        });
        if batch.len() == BATCH {
            ctx.db.link_many(GraphContextId::BASE, related, &batch)?;
            batch.clear();
            ctx.db.commit()?;
        }
    }
    if !batch.is_empty() {
        ctx.db.link_many(GraphContextId::BASE, related, &batch)?;
    }
    ctx.db.commit()?;
    ctx.related = Some(related);
    graph_neighbours_agree_with_the_point_index(ctx, corpus, edges, 200)?;
    eprintln!("[e4] {} `related` edges written", edges.len());
    Ok(stage("graph", at.elapsed().as_secs_f64()))
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

fn stage(name: &str, seconds: f64) -> Value {
    json!({"name": name, "ms": seconds * 1e3})
}

fn skipped_stage(name: &str) -> Value {
    json!({"name": name, "ms": Value::Null})
}

/// Create the collection, stream every row in with one commit per 256, then
/// build all seven indexes LATE, each its own timed stage.
fn load_e4(dir: &Path, corpus: &Corpus) -> R<(E4Ctx, Vec<Value>)> {
    let _ = fs::remove_dir_all(dir);
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut stages = Vec::new();

    let at = Instant::now();
    let mut db = Database::create(
        dir,
        Config {
            budget_bytes: CACHE_BYTES,
            io: IoMode::Buffered,
            sync: SyncMode::Normal,
        },
    )?;
    // `born_ts` is `born` as the instant it names: the same date, stored the
    // way QL_CONTRACT §4.2 stores one (Int microseconds, UTC), with the
    // DECLARED type recorded in the catalog descriptor so a statement can
    // fold a date/time function over it (QL_CONTRACT §5 deviation 8).
    let place = db.create_collection_declared(
        "place",
        vec![
            ("key".into(), Kind::Text),
            ("name".into(), Kind::Text),
            ("desc".into(), Kind::Text),
            ("text".into(), Kind::Text),
            ("born".into(), Kind::Int),
            (BORN_TS.into(), Kind::Int),
            ("kind".into(), Kind::Text),
            ("loc".into(), Kind::Point),
            ("plot".into(), Kind::Geo),
            ("emb".into(), Kind::Vector(DIM)),
        ],
        vec![(BORN_TS.to_owned(), "TIMESTAMPTZ".to_owned())],
        CollectionOptions::default(),
    )?;
    db.commit()?;
    stages.push(stage("open", at.elapsed().as_secs_f64()));

    eprintln!("[e4] inserting {} rows, commit every {BATCH} …", corpus.rows.len());
    let at = Instant::now();
    for (i, row) in corpus.rows.iter().enumerate() {
        let id = db.put(
            place,
            &row.key,
            &json!({
                "key": row.key,
                "name": row.name,
                "desc": row.desc,
                "text": row.text(),
                "born": row.born,
                BORN_TS: born_ts_micros(row.born)?,
                "kind": row.kind,
                "loc": {"type": "Point", "coordinates": [row.lon, row.lat]},
                "plot": geom_to_json(&row.plot),
                "emb": row.emb,
            }),
        )?;
        // Deviation 2's key vector is indexed by `sequence - 1`, so the put
        // order has to be the file order and nothing may be skipped.
        if id.sequence != (i + 1) as u64 {
            return Err(format!(
                "row {i} landed at sequence {}, breaking the id-to-key vector",
                id.sequence
            )
            .into());
        }
        if (i + 1) % BATCH == 0 {
            db.commit()?;
        }
    }
    db.commit()?;
    stages.push(stage("load", at.elapsed().as_secs_f64()));

    // Every build is explicit and late, the way `popsim` builds its four and
    // the way Postgres's CREATE INDEX runs after the load. A commit follows
    // each create because a build refuses to start with user writes pending.
    let mut build = |db: &mut Database, name: &str, id: IndexId| -> R<()> {
        let at = Instant::now();
        db.commit()?;
        db.build_index_to_ready(id, BATCH)?;
        db.commit()?;
        stages.push(stage(&format!("index:{name}"), at.elapsed().as_secs_f64()));
        Ok(())
    };

    let text = db.create_text_index(place, IX_TEXT, "text")?;
    build(&mut db, IX_TEXT, text)?;
    let born = db.create_scalar_index(place, IX_BORN, "born", false)?;
    build(&mut db, IX_BORN, born)?;
    let kind = db.create_scalar_index(place, IX_KIND, "kind", false)?;
    build(&mut db, IX_KIND, kind)?;
    let loc = db.create_point_index(place, IX_LOC, "loc")?;
    build(&mut db, IX_LOC, loc)?;
    let plot = db.create_geometry_index(place, IX_PLOT, "plot")?;
    build(&mut db, IX_PLOT, plot)?;
    let emb_exact = db.create_exact_vector_index(place, IX_EMB_EXACT, "emb")?;
    build(&mut db, IX_EMB_EXACT, emb_exact)?;
    let emb_ann = db.create_quantized_vector_index(place, IX_EMB_ANN, "emb")?;
    build(&mut db, IX_EMB_ANN, emb_ann)?;
    // The three objects the §4.1 / §4.2 function battery names. The last is
    // an EXPRESSION index: an ordinary scalar index whose stored value is
    // `lower(kind)`, which is what makes `lower(kind) = x` a range.
    let born_ts = db.create_scalar_index(place, IX_BORN_TS, BORN_TS, false)?;
    build(&mut db, IX_BORN_TS, born_ts)?;
    let name = db.create_scalar_index(place, IX_NAME, "name", false)?;
    build(&mut db, IX_NAME, name)?;
    let kind_lower =
        db.create_expression_index(place, IX_KIND_LOWER, "kind", IndexExpr::Lower, false)?;
    build(&mut db, IX_KIND_LOWER, kind_lower)?;

    let at = Instant::now();
    db.checkpoint()?;
    stages.push(stage("checkpoint", at.elapsed().as_secs_f64()));

    Ok((
        E4Ctx {
            db,
            place,
            related: None,
            text,
            born,
            kind,
            loc,
            plot,
            emb_exact,
            emb_ann,
            born_ts,
            name,
            kind_lower,
        },
        stages,
    ))
}

/// Reopen a database an earlier pass built and find its collection and seven
/// indexes BY NAME, the way a process that did not build the file has to.
/// Every stage but the open is `null`: nothing was loaded or built here, and
/// a zero would read as "instant".
fn open_e4(dir: &Path) -> R<(E4Ctx, Vec<Value>)> {
    let at = Instant::now();
    let db = Database::open(
        dir,
        Config {
            budget_bytes: CACHE_BYTES,
            io: IoMode::Buffered,
            sync: SyncMode::Normal,
        },
    )?;
    let place = db
        .collection("place")?
        .ok_or("--reuse: no `place` collection in this database")?;
    let found = indexes_of(&db);
    // The §4.1 / §4.2 function battery names one column and three indexes
    // that predate no earlier pass: a database built before this battery
    // existed does not have them. They are added ONCE here, on the reused
    // copy, and the cost is reported as its own stage rather than hidden --
    // it is a load, not a reuse, and calling it a reuse would be a lie about
    // where the time went.
    let mut db = db;
    let at_provision = Instant::now();
    let provisioned = provision_functions(&mut db, place, &found, &provision_marker(dir))?;
    let counted = provision_row_counts(&mut db)?;
    // The graph ENDPOINT SETS are the same kind of object: a database an
    // earlier pass built has edges that predate them, so it keeps the old
    // walk until one bounded pass builds them. The pass is idempotent and
    // costs two range seeks on a database that already has them.
    let at_endpoints = Instant::now();
    let endpoints = provision_endpoint_sets(&mut db)?;
    if let Some((edges, keys)) = endpoints {
        eprintln!(
            "[e4] built the graph endpoint sets: {keys} keys over {edges} edges in {:.1}s",
            at_endpoints.elapsed().as_secs_f64()
        );
    }
    let provision_seconds = at_provision.elapsed().as_secs_f64();
    let found = indexes_of(&db);
    let by_name = |wanted: &str| -> R<IndexId> {
        found
            .iter()
            .find(|(name, _, _)| name == wanted)
            .map(|(_, id, _)| *id)
            .ok_or_else(|| format!("--reuse: no `{wanted}` index in this database").into())
    };
    let db = db;
    let ctx = E4Ctx {
        place,
        // `graph_header` refuses a database with no graph feature, which is
        // every one a `--graph` load has not touched: that is `None` here,
        // not an error.
        related: db.edge_type(RELATED).ok().flatten(),
        text: by_name(IX_TEXT)?,
        born: by_name(IX_BORN)?,
        kind: by_name(IX_KIND)?,
        loc: by_name(IX_LOC)?,
        plot: by_name(IX_PLOT)?,
        emb_exact: by_name(IX_EMB_EXACT)?,
        emb_ann: by_name(IX_EMB_ANN)?,
        born_ts: by_name(IX_BORN_TS)?,
        name: by_name(IX_NAME)?,
        kind_lower: by_name(IX_KIND_LOWER)?,
        db,
    };
    let mut stages = vec![stage("open", at.elapsed().as_secs_f64()), skipped_stage("load")];
    for name in [
        IX_TEXT,
        IX_BORN,
        IX_KIND,
        IX_LOC,
        IX_PLOT,
        IX_EMB_EXACT,
        IX_EMB_ANN,
    ] {
        stages.push(skipped_stage(&format!("index:{name}")));
    }
    for name in [IX_BORN_TS, IX_NAME, IX_KIND_LOWER] {
        stages.push(if provisioned {
            stage(&format!("provision:{name}"), provision_seconds / 3.0)
        } else {
            skipped_stage(&format!("index:{name}"))
        });
    }
    stages.push(skipped_stage("checkpoint"));
    if provisioned {
        eprintln!(
            "[e4] reopened {}; added `{BORN_TS}`, {IX_BORN_TS}, {IX_NAME} and {IX_KIND_LOWER} in {provision_seconds:.1}s (the function battery's own objects)",
            dir.display()
        );
    } else if counted {
        eprintln!("[e4] reopened {}; queries only (live row counts backfilled)", dir.display());
    } else {
        eprintln!("[e4] reopened {}; queries only", dir.display());
    }
    Ok((ctx, stages))
}

/// Give a reused database its LIVE ROW COUNT records, the same way
/// `provision_functions` gives it `born_ts`: a database built before the
/// records existed has none, and `count(*)` would take the walk it always
/// took while a freshly loaded one reads a record. The two arms have to be
/// asked the same question about the same representation.
///
/// `Database::backfill_row_counts` is bounded and resumable, so this is a
/// loop with a commit per step; on a database that already has its records
/// the first call answers `done` and walks nothing. `true` when it wrote any.
fn provision_row_counts(db: &mut Database) -> R<bool> {
    let mut written = 0u64;
    let mut walked = 0u64;
    loop {
        let progress = db.backfill_row_counts(BATCH)?;
        db.commit()?;
        written += progress.records_written;
        walked += progress.rows_walked;
        if progress.done {
            break;
        }
    }
    if written > 0 {
        eprintln!("[e4] backfilled {written} live row-count record(s) over {walked} row(s)");
    }
    Ok(written > 0)
}

/// Build the graph ENDPOINT SETS on a reused database that predates them.
///
/// `Database::backfill_endpoint_sets` is bounded and resumable, so this is a
/// loop with a commit per step: the whole pass in one transaction would be a
/// page-WAL allowance the bench has no reason to ask for. It returns the
/// edges walked and the keys written, or `None` when the database already
/// declares the feature (the ordinary case after the first run on one copy).
fn provision_endpoint_sets(db: &mut Database) -> R<Option<(u64, u64)>> {
    if db.endpoint_sets_present() {
        return Ok(None);
    }
    let mut edges = 0u64;
    let mut keys = 0u64;
    loop {
        let progress = db.backfill_endpoint_sets(BATCH * 16)?;
        edges += progress.edges_seen;
        keys += progress.keys_written;
        db.commit()?;
        if progress.done {
            break;
        }
    }
    if !db.endpoint_sets_present() {
        // No graph in this database: nothing to build and no bit to set.
        return Ok(None);
    }
    Ok(Some((edges, keys)))
}

/// Where the resumable provisioning cursor lives: BESIDE the database
/// directory, never inside it, so the database's own file inventory is
/// untouched.
fn provision_marker(dir: &Path) -> PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "e4-db".to_owned());
    dir.with_file_name(format!("{name}.provision.json"))
}

/// The cursor a previous, possibly killed, run left behind.
///
/// It is a HINT, never a proof: it is written only after the batch it
/// describes has committed, so it can be behind the data but never ahead of
/// it. A missing, unreadable or foreign marker simply means "start at the
/// beginning", which re-scans and fills nothing that is already filled.
fn read_marker(marker: &Path, place: CollectionId) -> (Option<EntityId>, bool) {
    let Ok(text) = fs::read_to_string(marker) else {
        return (None, false);
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return (None, false);
    };
    if value["collection"].as_u64() != Some(u64::from(place.0)) {
        return (None, false);
    }
    let after = value["after"].as_u64().map(|sequence| EntityId {
        collection: place,
        sequence,
    });
    (after, value["complete"].as_bool() == Some(true))
}

fn write_marker(
    marker: &Path,
    place: CollectionId,
    after: Option<EntityId>,
    complete: bool,
    rows: u64,
) -> R<()> {
    let value = json!({
        "collection": place.0,
        "after": after.map(|id| id.sequence),
        "complete": complete,
        "rows": rows,
    });
    fs::write(marker, serde_json::to_vec(&value)?)?;
    Ok(())
}

/// Count the collection's rows, and the rows whose `born_ts` is present and
/// not null.
///
/// This is the resume PROOF. The marker can be stale and the layout can say
/// the column exists while no row carries a value, so the only statement
/// worth acting on is the one read back off the rows themselves.
fn born_ts_counts(db: &Database, place: CollectionId) -> R<(u64, u64)> {
    let mut rows = 0u64;
    let mut filled = 0u64;
    let mut after: Option<EntityId> = None;
    loop {
        let mut seen = 0usize;
        let mut last = after;
        for entity in db.scan(place, after)? {
            let entity = entity?;
            last = Some(entity.id);
            seen += 1;
            rows += 1;
            if entity
                .document
                .get(BORN_TS)
                .is_some_and(|value| !value.is_null())
            {
                filled += 1;
            }
            if seen == BATCH {
                break;
            }
        }
        if seen == 0 {
            break;
        }
        after = last;
    }
    Ok((rows, filled))
}

/// Fill `born_ts` from `born` for every row still missing it, in bounded
/// batches, recording a resumable cursor beside the database.
///
/// The predicate is the ROW's own value, never the layout's: a row that
/// already has the column is skipped, so the walk is idempotent and a lost
/// or stale cursor costs a re-scan and nothing else. The marker is written
/// only AFTER its batch has committed, so it is behind the data or exactly
/// on it, never ahead.
fn fill_born_ts(
    db: &mut Database,
    place: CollectionId,
    marker: &Path,
    from: Option<EntityId>,
) -> R<u64> {
    eprintln!("[e4] filling `{BORN_TS}` from `born` …");
    let mut filled = 0u64;
    let mut after = from;
    loop {
        // `Database::scan` is the stable id-order walk with an exclusive
        // cursor, so the fill is bounded per batch and resumes where the
        // last commit left it rather than holding the collection.
        let mut batch: Vec<(String, i64)> = Vec::with_capacity(BATCH);
        let mut seen = 0usize;
        let mut last = after;
        for entity in db.scan(place, after)? {
            let entity = entity?;
            last = Some(entity.id);
            seen += 1;
            if entity
                .document
                .get(BORN_TS)
                .is_some_and(|value| !value.is_null())
            {
                continue;
            }
            let born = entity
                .document
                .get("born")
                .and_then(Value::as_i64)
                .ok_or("--reuse: a row has no integer `born` to derive born_ts from")?;
            batch.push((entity.key.clone(), born));
            if seen == BATCH {
                break;
            }
        }
        // A batch can be EMPTY while rows remain -- every row it saw was
        // already filled -- so the walk ends on the SCAN, not on the batch.
        if seen == 0 {
            break;
        }
        for (key, born) in &batch {
            db.update(place, key, &json!({ BORN_TS: born_ts_micros(*born)? }))?;
            filled += 1;
        }
        db.commit()?;
        after = last;
        write_marker(marker, place, after, false, 0)?;
    }
    eprintln!("[e4] filled {filled} rows");
    Ok(filled)
}

/// Add the §4.1 / §4.2 function battery's column and indexes to a database
/// that does not have them. `false` when they are all already there and the
/// side marker says the fill finished, which is every run after the first on
/// one copy.
///
/// CRASH SAFETY. The column is added with `alter_collection_declared`, which
/// writes a new immutable `Layout` and rewrites no row (QL_CONTRACT §2), so
/// every existing row reads MISSING for it until it is filled. That ALTER
/// commits before the first fill batch does, so a kill in between leaves a
/// database whose layout has the column and whose rows do not. Three things
/// keep that from becoming a silent wrong answer:
///
/// 1. The fill is driven by the ROW, not by the layout: only a row still
///    missing `born_ts` is written, so a resumed run is idempotent and a
///    lost cursor costs a re-scan and nothing else.
/// 2. The cursor lives in a side marker written only AFTER its batch has
///    committed, so it is behind the data or exactly on it, never ahead.
/// 3. An index left BUILDING by a kill is rebuilt rather than used: a
///    Building index is refused at `src/query/plan.rs`, and a rebuilt one is
///    the only kind the cases may read.
///
/// And the load-bearing check: `count(born_ts is not null) == count(*)` over
/// the rows, after everything above. A run that cannot say that refuses to
/// measure, because every e4-side arm would otherwise read the same
/// truncated index and agree with itself.
fn provision_functions(
    db: &mut Database,
    place: CollectionId,
    found: &[(String, IndexId, IndexState)],
    marker: &Path,
) -> R<bool> {
    let has = |wanted: &str| found.iter().any(|(name, _, _)| name == wanted);
    let ready = |wanted: &str| {
        found
            .iter()
            .any(|(name, _, state)| name == wanted && *state == IndexState::Ready)
    };
    let wanted = [IX_BORN_TS, IX_NAME, IX_KIND_LOWER];
    let (resume_at, complete) = read_marker(marker, place);
    if complete && wanted.iter().all(|name| ready(name)) {
        return Ok(false);
    }
    if wanted.iter().all(|name| has(name)) && wanted.iter().all(|name| ready(name)) {
        // Everything is here and Ready but no marker says the fill finished.
        // Prove it from the rows; if it holds, record the proof and skip.
        let (rows, filled) = born_ts_counts(db, place)?;
        if rows == filled && rows > 0 {
            write_marker(marker, place, None, true, rows)?;
            return Ok(false);
        }
    }
    let info = db.collection_info(place)?;
    if !info.layout.fields.iter().any(|(n, _)| n == BORN_TS) {
        let mut fields: Vec<(String, Kind)> = info.layout.fields.clone();
        fields.push((BORN_TS.to_owned(), Kind::Int));
        let declared = vec![(BORN_TS.to_owned(), "TIMESTAMPTZ".to_owned())];
        db.alter_collection_declared(place, fields, declared)?;
        db.commit()?;
    }
    fill_born_ts(db, place, marker, resume_at)?;
    // The proof, before anything is built over the column. A marker from a
    // different run could have started the walk past a row that was never
    // filled, so a count that does not add up is answered with ONE full pass
    // from the beginning -- and only a second failure is a refusal.
    let (rows, filled) = born_ts_counts(db, place)?;
    if rows != filled {
        eprintln!("[e4] {} rows still missing `{BORN_TS}`; refilling from the start", rows - filled);
        fill_born_ts(db, place, marker, None)?;
    }
    let build = |db: &mut Database, id: IndexId| -> R<()> {
        db.commit()?;
        db.build_index_to_ready(id, BATCH)?;
        db.commit()?;
        Ok(())
    };
    // An index a kill left BUILDING is finished before it is used -- any
    // index, not only this battery's three. Without this every later run
    // hard-fails at `src/query/plan.rs`, and an index built over rows that
    // were still MISSING their value would answer from a truncated posting
    // set. A DROPPING index is left alone: a drop in flight is resumed by
    // the drop, not by a build.
    for (name, id, state) in found {
        if matches!(state, IndexState::Building { .. }) {
            eprintln!("[e4] rebuilding `{name}`, left BUILDING by an earlier run");
            build(db, *id)?;
        }
    }
    if !has(IX_BORN_TS) {
        let id = db.create_scalar_index(place, IX_BORN_TS, BORN_TS, false)?;
        build(db, id)?;
    }
    if !has(IX_NAME) {
        let id = db.create_scalar_index(place, IX_NAME, "name", false)?;
        build(db, id)?;
    }
    if !has(IX_KIND_LOWER) {
        let id = db.create_expression_index(place, IX_KIND_LOWER, "kind", IndexExpr::Lower, false)?;
        build(db, id)?;
    }
    db.commit()?;
    // The assertion. Nothing below this line runs the cases on a database
    // that cannot answer for its own column.
    let (rows, filled) = born_ts_counts(db, place)?;
    if rows == 0 {
        return Err("--reuse: `place` holds no rows".into());
    }
    if rows != filled {
        return Err(format!(
            "--reuse: provisioning left {} of {rows} rows without a `{BORN_TS}` value; refusing to run the function cases, because every e4-side arm would read the same truncated index and agree with itself",
            rows - filled
        )
        .into());
    }
    for (name, id, _) in indexes_of(db) {
        if wanted.contains(&name.as_str()) {
            let state = db.index_info(id)?.state;
            if state != IndexState::Ready {
                return Err(format!("--reuse: `{name}` is not READY after provisioning").into());
            }
        }
    }
    write_marker(marker, place, None, true, rows)?;
    Ok(true)
}

/// Every index this database holds, by name, id and state.
fn indexes_of(db: &Database) -> Vec<(String, IndexId, IndexState)> {
    let mut found = Vec::new();
    for n in 1..=64u64 {
        if let Ok(info) = db.index_info(IndexId(n)) {
            found.push((info.name.clone(), info.id, info.state));
        }
    }
    found
}

// ── the Postgres arm ──────────────────────────────────────────────────────

/// A SQL string literal with its single quotes doubled. Terms and category
/// names come from files, not from constants in this program, so they are
/// escaped rather than trusted.
fn quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A `geography` point literal at `points[i]`-style coordinates.
fn pg_point(lon: f64, lat: f64) -> String {
    format!("ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography")
}

/// The `to_tsvector` expression the GIN index is built over. Every text query
/// repeats it verbatim so the planner recognises the expression index.
const PG_TSVECTOR: &str = "to_tsvector('simple', name || ' ' || descr)";

fn pg_tsquery(terms: &str) -> String {
    format!("to_tsquery('simple', {})", quoted(terms))
}

/// Rows come back one column wide — the `key` — so nothing else is
/// materialised per row in either arm. `query_raw` streams instead of
/// buffering, so a 50,000-row filter case costs a page, not a result set.
fn pg_answer(client: &mut Client, setup: &[String], sql: &str) -> R<Answer> {
    use postgres::fallible_iterator::FallibleIterator;
    let mut txn = client.transaction()?;
    for statement in setup {
        txn.batch_execute(statement)?;
    }
    let mut answer = Answer::default();
    {
        let mut rows = txn.query_raw(sql, std::iter::empty::<i32>())?;
        while let Some(row) = rows.next()? {
            answer.push(row.try_get::<_, String>(0)?);
        }
    }
    txn.commit()?;
    Ok(answer)
}

/// Exactness by planner knob: with both index-scan paths off, the vector
/// order is a sort over a sequential scan, which is the exact answer. It also
/// takes the GIN and GiST indexes away from any filter in the same statement;
/// see deviation 6.
fn pg_exact_setup() -> Vec<String> {
    vec![
        "SET LOCAL enable_indexscan = off".into(),
        "SET LOCAL enable_bitmapscan = off".into(),
    ]
}


// ── the statement fragments both SQL arms share ───────────────────────────

/// How a value reaches a statement: INLINED as a literal, which is how the
/// Postgres arm builds its SQL text, or BOUND as `$n`, which is how the
/// `e4-sql` arm hands values to `Database::sql`. Every clause below is
/// written once against this trait, so the two arms cannot drift into asking
/// different questions -- the whole point of a three-arm comparison.
trait Values {
    fn number(&mut self, value: f64) -> String;
    fn int(&mut self, value: i64) -> String;
    fn text(&mut self, value: &str) -> String;
    /// A 32-dimensional query vector, as pgvector's text form or as a bound
    /// `Param::Vector`.
    fn vector(&mut self, literal: &str, value: &[f32]) -> String;
}

/// The Postgres spelling: the value itself, escaped. `{:?}` on an `f64` is
/// Rust's shortest round-tripping form, which is what this file has always
/// written into these statements.
struct Inline;

impl Values for Inline {
    fn number(&mut self, value: f64) -> String {
        format!("{value:?}")
    }
    fn int(&mut self, value: i64) -> String {
        value.to_string()
    }
    fn text(&mut self, value: &str) -> String {
        quoted(value)
    }
    fn vector(&mut self, literal: &str, _value: &[f32]) -> String {
        quoted(literal)
    }
}

/// The `e4-sql` spelling: a placeholder, and the value on the side. Named
/// `Binder` because `std::ops::Bound` is already in scope here.
#[derive(Default)]
struct Binder {
    params: Vec<Param>,
}

impl Binder {
    fn mark(&mut self) -> String {
        format!("${}", self.params.len())
    }
}

impl Values for Binder {
    fn number(&mut self, value: f64) -> String {
        self.params.push(Param::Float(value));
        self.mark()
    }
    fn int(&mut self, value: i64) -> String {
        self.params.push(Param::Int(value));
        self.mark()
    }
    fn text(&mut self, value: &str) -> String {
        self.params.push(Param::Text(value.to_owned()));
        self.mark()
    }
    fn vector(&mut self, _literal: &str, value: &[f32]) -> String {
        self.params.push(Param::Vector(value.to_vec()));
        self.mark()
    }
}

/// `ST_SetSRID(ST_MakePoint(lon, lat), 4326)`, optionally cast to geography.
fn sql_point(v: &mut dyn Values, lon: f64, lat: f64, geography: bool) -> String {
    let lon = v.number(lon);
    let lat = v.number(lat);
    format!(
        "ST_SetSRID(ST_MakePoint({lon},{lat}),4326){}",
        if geography { "::geography" } else { "" }
    )
}

/// `queries.json`'s box, which is `[minlon, maxlon, minlat, maxlat]`
/// (deviation 3), as PostGIS's `(minlon, minlat, maxlon, maxlat)` envelope.
fn sql_envelope(v: &mut dyn Values, b: [f64; 4]) -> String {
    let minlon = v.number(b[0]);
    let minlat = v.number(b[2]);
    let maxlon = v.number(b[1]);
    let maxlat = v.number(b[3]);
    format!("ST_MakeEnvelope({minlon},{minlat},{maxlon},{maxlat},4326)")
}

fn sql_polygon(v: &mut dyn Values, json: &str) -> String {
    let json = v.text(json);
    format!("ST_SetSRID(ST_GeomFromGeoJSON({json}),4326)")
}

/// `ST_DWithin(loc, <centre>, <metres>, true)` -- spheroidal, which is what
/// `PointFilter::Radius` and `GeometryFilter::DWithin` are (deviation 9).
fn sql_dwithin(v: &mut dyn Values, column: &str, centre: String, metres: f64) -> String {
    let metres = v.number(metres);
    format!("ST_DWithin({column}, {centre}, {metres}, true)")
}

fn sql_born(v: &mut dyn Values, lower: i64, upper: i64) -> String {
    let lower = v.int(lower);
    let upper = v.int(upper);
    format!("born BETWEEN {lower} AND {upper}")
}

fn sql_kind(v: &mut dyn Values, kind: &str) -> String {
    let kind = v.text(kind);
    format!("kind = {kind}")
}

/// `<tsvector expression> @@ to_tsquery('simple', <terms>)`. The tsvector
/// expression differs per arm and only per arm: Postgres indexes
/// `name || ' ' || descr`, E4 indexes the stored concatenation `text`,
/// because a text index spans ONE declared field (deviation 1).
fn sql_text_match(v: &mut dyn Values, tsvector: &str, terms: &str) -> String {
    let terms = v.text(terms);
    format!("{tsvector} @@ to_tsquery('simple', {terms})")
}

fn sql_vector(v: &mut dyn Values, literal: &str, value: &[f32]) -> String {
    let vector = v.vector(literal, value);
    format!("{vector}::vector")
}

/// One case, one query instance, as Postgres spells it: the `SET LOCAL`
/// statements the case needs and the statement itself.
fn pg_case(q: &Queries, kinds: &[String], name: &str, i: usize) -> R<(Vec<String>, String)> {
    let v = &mut Inline;
    let kind = quoted(&kinds[i % KINDS]);
    let point = q.points[i];
    let radius = q.radii[i];
    let b = q.boxes[i];
    let envelope = sql_envelope(v, b);
    let polygon = sql_polygon(v, &q.polygon_json[i]);
    let radius_centre = sql_point(v, radius[0], radius[1], true);
    let radius_clause = sql_dwithin(v, "loc", radius_centre, radius[2]);
    let text_clause = sql_text_match(v, PG_TSVECTOR, &q.terms[i]);
    let (born_lower, born_upper) = q.born_range(i);
    let born_clause = sql_born(v, born_lower, born_upper);
    let vector = sql_vector(v, &q.vector_literals[i], &q.vectors[i]);
    let knn_point = sql_point(v, point[0], point[1], true);

    let plain = |sql: String| -> R<(Vec<String>, String)> { Ok((Vec::new(), sql)) };

    // Approximate sweep points (`vec_ann_10@sls<N>`,
    // `vec_ann_10_kind@sls<N>`, and the `..@sls100+resc400` rescore probe):
    // dispatched here rather than as one match arm per `SLS_SWEEP` value.
    if let Some(sweep) = parse_approx_sweep(name) {
        let sls = sweep
            .sls
            .ok_or_else(|| format!("battle50k: `{name}` has no sls=... suffix for the Postgres arm"))?;
        let mut setup = vec![format!("SET LOCAL diskann.query_search_list_size = {sls}")];
        if let Some(rescore) = sweep.rescore {
            setup.push(format!("SET LOCAL diskann.query_rescore = {rescore}"));
        }
        let sql = if sweep.by_kind {
            format!("SELECT \"key\" FROM place WHERE kind = {kind} ORDER BY emb <=> {vector} LIMIT {K}")
        } else {
            format!("SELECT \"key\" FROM place ORDER BY emb <=> {vector} LIMIT {K}")
        };
        return Ok((setup, sql));
    }

    // ── the aggregate battery (QL_CONTRACT §4.7) ────────────────────────
    // Every group comes back as ONE text column, formatted exactly as
    // `agg_line` formats it in the two E4 arms, so the three reports are
    // diffable group by group. `floor(avg(born))::bigint` is the avg both
    // sides can agree on; a printed float is not.
    if is_aggregate_case(name) {
        return match name {
            "agg_count_all" => plain("SELECT '|n=' || count(*)::text FROM place".to_owned()),
            "agg_count_kind" => plain(
                "SELECT kind || '|n=' || count(*)::text FROM place GROUP BY kind ORDER BY kind"
                    .to_owned(),
            ),
            "agg_sum_born_by_kind" => plain(
                "SELECT kind || '|n=' || count(*)::text || '|s=' || sum(born)::text \
                 || '|lo=' || min(born)::text || '|hi=' || max(born)::text \
                 || '|mean=' || floor(avg(born))::bigint::text \
                 FROM place GROUP BY kind HAVING count(*) > 100 ORDER BY kind"
                    .to_owned(),
            ),
            "agg_distinct_kind" => {
                plain("SELECT DISTINCT kind FROM place ORDER BY kind".to_owned())
            }
            "agg_count_radius_by_kind" => plain(format!(
                "SELECT kind || '|n=' || count(*)::text FROM place \
                 WHERE {radius_clause} GROUP BY kind ORDER BY kind"
            )),
            "agg_born_decade" => plain(
                "SELECT (born / 10000)::text || '|n=' || count(*)::text FROM place \
                 GROUP BY born / 10000 ORDER BY born / 10000"
                    .to_owned(),
            ),
            other => Err(format!(
                "battle50k: no Postgres spelling for aggregate case `{other}`"
            )
            .into()),
        };
    }

    match name {
        // ── filters ─────────────────────────────────────────────────────
        "pt_radius" => plain(format!("SELECT \"key\" FROM place WHERE {radius_clause}")),
        "pt_bbox" => plain(format!(
            "SELECT \"key\" FROM place WHERE loc && {envelope}::geography \
             AND ST_X(loc::geometry) BETWEEN {:?} AND {:?} \
             AND ST_Y(loc::geometry) BETWEEN {:?} AND {:?}",
            b[0], b[1], b[2], b[3]
        )),
        // The planar predicates run on `plot::geometry`, which no index on a
        // `geography` column can serve; the `&&` term in front of each is the
        // geodetic bounding-box candidate (a superset for Within and for
        // Contains alike), so the GiST index still narrows the scan before
        // the exact refine. Same candidate-then-refine shape `pt_bbox` uses.
        "plot_within_box" => plain(format!(
            "SELECT \"key\" FROM place WHERE plot && {envelope}::geography \
             AND ST_Within(plot::geometry, {envelope})"
        )),
        "plot_contains_pt" => plain(format!(
            "SELECT \"key\" FROM place WHERE plot && {} \
             AND ST_Contains(plot::geometry, ST_SetSRID(ST_MakePoint({:?},{:?}),4326))",
            pg_point(point[0], point[1]),
            point[0],
            point[1]
        )),
        "plot_intersects" => plain(format!(
            "SELECT \"key\" FROM place WHERE ST_Intersects(plot, {polygon}::geography)"
        )),
        "plot_dwithin_1km" => plain(format!(
            "SELECT \"key\" FROM place WHERE ST_DWithin(plot, {}, 1000, true)",
            pg_point(point[0], point[1])
        )),
        "plot_vs_poly_within" => plain(format!(
            "SELECT \"key\" FROM place WHERE plot && {polygon}::geography \
             AND ST_Within(plot::geometry, {polygon})"
        )),
        "text_one" => plain(format!("SELECT \"key\" FROM place WHERE {text_clause}")),
        "text_two" => plain(format!(
            "SELECT \"key\" FROM place WHERE {PG_TSVECTOR} @@ {}",
            pg_tsquery(&format!("{} & {}", q.terms[i], q.terms[(i + 1) % INSTANCES]))
        )),
        "text_and_kind" => plain(format!(
            "SELECT \"key\" FROM place WHERE {text_clause} AND kind = {kind}"
        )),
        "born_range" => plain(format!("SELECT \"key\" FROM place WHERE {born_clause}")),
        "kind_eq" => plain(format!("SELECT \"key\" FROM place WHERE kind = {kind}")),
        "radius_and_born" => plain(format!(
            "SELECT \"key\" FROM place WHERE {radius_clause} AND {born_clause}"
        )),

        // ── boolean (QL_CONTRACT §3) ────────────────────────────────────
        "bool_kind_in3" => plain(format!(
            "SELECT \"key\" FROM place WHERE kind IN ({}, {}, {})",
            quoted(&kinds[i % KINDS]),
            quoted(&kinds[(i + 1) % KINDS]),
            quoted(&kinds[(i + 2) % KINDS])
        )),
        "bool_born_or_kind" => plain(format!(
            "SELECT \"key\" FROM place WHERE ({born_clause}) OR kind = {kind}"
        )),
        "bool_not_kind" => plain(format!(
            "SELECT \"key\" FROM place WHERE kind <> {kind}"
        )),
        "bool_radius_or_radius" => {
            let other = q.radii[(i + 1) % INSTANCES];
            let second_centre = sql_point(v, other[0], other[1], true);
            let second = sql_dwithin(v, "loc", second_centre, other[2]);
            plain(format!(
                "SELECT \"key\" FROM place WHERE {radius_clause} OR {second}"
            ))
        }
        "bool_not_null_born" => plain(
            "SELECT \"key\" FROM place WHERE born IS NOT NULL".to_owned()
        ),
        "bool_exists_related" => plain(format!(
            "SELECT \"key\" FROM place \
             WHERE EXISTS (SELECT 1 FROM {RELATED} r WHERE r.source = place.\"key\")"
        )),
        // ── the function battery (QL_CONTRACT §4.1, §4.2) ───────────────
        // These need a `born_ts timestamptz` column with a btree, a btree on
        // `name` and an expression index on `lower(kind)`. The DDL is in
        // `tools/battle50k_pg_cases.sql`; a database that has not run it
        // cannot run these five cases.
        "fn_year_eq" => plain(format!(
            "SELECT \"key\" FROM place WHERE {BORN_TS} >= make_timestamptz({}, 1, 1, 0, 0, 0, 'UTC') AND {BORN_TS} < make_timestamptz({}, 1, 1, 0, 0, 0, 'UTC')",
            q.fn_year(i),
            q.fn_year(i) + 1
        )),
        "fn_trunc_month_range" => {
            let (from, to) = q.fn_month_window(i);
            plain(format!(
                "SELECT \"key\" FROM place WHERE date_trunc('month', {BORN_TS}) BETWEEN {} AND {}",
                quoted(&from),
                quoted(&to)
            ))
        }
        "fn_lower_eq" => plain(format!(
            "SELECT \"key\" FROM place WHERE lower(kind) = {}",
            quoted(&kinds[i % KINDS].to_lowercase())
        )),
        "fn_like_prefix" => plain(format!(
            "SELECT \"key\" FROM place WHERE name LIKE {}",
            quoted(&format!("{}%", q.name_prefix(i)))
        )),
        // The projection is the case; the key column keeps the three arms
        // comparing the same row set.
        "fn_project_strings" => plain(format!(
            "SELECT \"key\", upper(name), length(descr) FROM place WHERE {born_clause}"
        )),

        // ── ranked ──────────────────────────────────────────────────────
        // No tiebreak after the distance: a second sort key would take the
        // ordered KNN-GiST walk away from the planner. See deviation 8.
        "knn_10" => plain(format!(
            "SELECT \"key\" FROM place ORDER BY loc <-> {knn_point} LIMIT {K}"
        )),
        "knn_10_kind" => plain(format!(
            "SELECT \"key\" FROM place WHERE kind = {kind} ORDER BY loc <-> {knn_point} LIMIT {K}"
        )),
        "text_top10" => plain(format!(
            "SELECT \"key\" FROM place WHERE {text_clause} \
             ORDER BY ts_rank_cd({PG_TSVECTOR}, {}) DESC LIMIT {K}",
            pg_tsquery(&q.terms[i])
        )),
        "vec_exact_10" => Ok((
            pg_exact_setup(),
            format!("SELECT \"key\" FROM place ORDER BY emb <=> {vector} LIMIT {K}"),
        )),
        "vec_exact_radius" => Ok((
            pg_exact_setup(),
            format!(
                "SELECT \"key\" FROM place WHERE {radius_clause} \
                 ORDER BY emb <=> {vector} LIMIT {K}"
            ),
        )),
        "hybrid_10" => Ok((
            pg_exact_setup(),
            format!(
                "SELECT \"key\" FROM place WHERE {text_clause} AND {radius_clause} \
                 ORDER BY emb <=> {vector} LIMIT {K}"
            ),
        )),
        // The blended score E4 cannot express. The ORDER BY is an arithmetic
        // expression, so no vector index could answer it in order anyway and
        // no planner knob is needed to keep it exact.
        "hybrid_blend_10" => plain(format!(
            "SELECT \"key\" FROM place WHERE {text_clause} AND {radius_clause} \
             ORDER BY 0.5 * ts_rank_cd({PG_TSVECTOR}, {}) + 0.5 * (1 - (emb <=> {vector})) \
             DESC LIMIT {K}",
            pg_tsquery(&q.terms[i])
        )),

        // ── the exact twin the approximate sweep's recall is measured on ─
        // (`vec_ann_10` and `vec_ann_10_kind` are no longer match arms here
        // themselves — every point of their sweep goes through
        // `parse_approx_sweep` at the top of this function.)
        "vec_ann_10_kind:exact" => Ok((
            pg_exact_setup(),
            format!(
                "SELECT \"key\" FROM place WHERE kind = {kind} ORDER BY emb <=> {vector} LIMIT {K}"
            ),
        )),


        // ── the graph cases ──────────────────────────────────────────────
        //
        // Recursive-free, `related` joined twice: the shape a planner
        // generates for a bounded two-hop pattern, and what Postgres 19
        // would produce for the same SQL/PGQ text. The UNION is the ACYCLIC
        // rule E4's BFS applies for nothing: a node found at one hop is not
        // returned again at two, and the seed is never its own answer.
        "graph_2hop" => {
            let seed = quoted(q.seed_key(i)?);
            plain(format!(
                "SELECT d FROM ( \
                   SELECT r1.destination AS d FROM {RELATED} r1 WHERE r1.source = {seed} \
                   UNION \
                   SELECT r2.destination FROM {RELATED} r1 \
                     JOIN {RELATED} r2 ON r2.source = r1.destination \
                    WHERE r1.source = {seed} \
                 ) t WHERE d <> {seed}"
            ))
        }
        "graph_2hop_weight" => {
            let seed = quoted(q.seed_key(i)?);
            plain(format!(
                "SELECT d FROM ( \
                   SELECT r1.destination AS d FROM {RELATED} r1 \
                    WHERE r1.source = {seed} AND r1.weight > 0.5 \
                   UNION \
                   SELECT r2.destination FROM {RELATED} r1 \
                     JOIN {RELATED} r2 ON r2.source = r1.destination AND r2.weight > 0.5 \
                    WHERE r1.source = {seed} AND r1.weight > 0.5 \
                 ) t WHERE d <> {seed}"
            ))
        }
        "graph_2hop_born" => {
            let seed = quoted(q.seed_key(i)?);
            plain(format!(
                "SELECT d FROM ( \
                   SELECT b.\"key\" AS d FROM {RELATED} r1 \
                     JOIN place b ON b.\"key\" = r1.destination \
                      AND b.born BETWEEN {born_lower} AND {born_upper} \
                    WHERE r1.source = {seed} \
                   UNION \
                   SELECT c.\"key\" FROM {RELATED} r1 \
                     JOIN place b ON b.\"key\" = r1.destination \
                      AND b.born BETWEEN {born_lower} AND {born_upper} \
                     JOIN {RELATED} r2 ON r2.source = b.\"key\" \
                     JOIN place c ON c.\"key\" = r2.destination \
                      AND c.born BETWEEN {born_lower} AND {born_upper} \
                    WHERE r1.source = {seed} \
                 ) t WHERE d <> {seed}"
            ))
        }
        "graph_1hop_weight_top10" => {
            let seed = quoted(q.seed_key(i)?);
            plain(format!(
                "SELECT r.source FROM {RELATED} r WHERE r.destination = {seed} \
                 ORDER BY r.weight DESC LIMIT {K}"
            ))
        }
        other => Err(format!("battle50k: no Postgres spelling for case `{other}`").into()),
    }
}

// ── the e4-sql arm ────────────────────────────────────────────────────────
//
// The same twenty-two cases as the `e4` arm, asked in SQL. It reuses the E4
// database, the E4 loader and the E4 key vector unchanged, and differs from
// the `e4` arm in exactly one thing: the request is written as text and
// parsed, instead of being built as a `QueryRequest` in Rust. That is what
// makes the two arms' medians comparable -- their difference is the parser.

/// The tsvector expression the E4 text index is built over: ONE declared
/// field, the stored concatenation (deviation 1). Postgres's `PG_TSVECTOR`
/// is the two-column expression its expression index is built over.
const E4_TSVECTOR: &str = "to_tsvector('simple', text)";

thread_local! {
    /// The `ef_search` this arm last set on the session, so the `SET LOCAL`
    /// is issued only when the value actually changes. Fifty instances of one
    /// case therefore pay for it once, which keeps the timed pass measuring
    /// the SELECT rather than a knob that did not move.
    static E4SQL_EF: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Set (or clear) the session's approximate shortlist bound. `SET LOCAL` is
/// settled while the statement COMPILES, so preparing it is enough.
fn e4sql_set_ef(ctx: &E4Ctx, want: Option<usize>) -> R<()> {
    if E4SQL_EF.with(std::cell::Cell::get) == want {
        return Ok(());
    }
    let statement = match want {
        Some(ef) => format!("SET LOCAL ef_search = {ef}"),
        None => "SET LOCAL ef_search = DEFAULT".to_owned(),
    };
    prepare_sql(&ctx.db, &statement, &[])?;
    E4SQL_EF.with(|cell| cell.set(want));
    Ok(())
}

/// One case, one query instance, as `e4-sql` spells it: the statement and the
/// parameters it binds, plus the `ef` the session must be at.
fn e4sql_case(q: &Queries, kinds: &[String], name: &str, i: usize) -> R<(String, Vec<Param>, Option<usize>)> {
    let mut bound = Binder::default();
    let v = &mut bound;
    let kind_value = kinds[i % KINDS].clone();
    let point = q.points[i];
    let radius = q.radii[i];
    let b = q.boxes[i];
    let (born_lower, born_upper) = q.born_range(i);

    // Approximate sweep points: the same `ef` axis the `e4` arm sweeps, said
    // in SQL as pgvector's own knob.
    if let Some(sweep) = parse_approx_sweep(name) {
        let ef = sweep
            .ef
            .ok_or_else(|| format!("battle50k: `{name}` has no ef=... suffix for the e4-sql arm"))?;
        let vector = sql_vector(v, &q.vector_literals[i], &q.vectors[i]);
        let sql = if sweep.by_kind {
            let kind = sql_kind(v, &kind_value);
            format!("SELECT _id FROM place WHERE {kind} ORDER BY emb <=> {vector} LIMIT {K}")
        } else {
            format!("SELECT _id FROM place ORDER BY emb <=> {vector} LIMIT {K}")
        };
        return Ok((sql, bound.params, Some(ef)));
    }

    // ── the aggregate battery (QL_CONTRACT §4.7), in SQL ────────────────
    // E4's `||` is Tier 2 (a row function on projected values), so these
    // statements return the group key and the accumulators as COLUMNS and
    // `e4sql_agg_run` formats the line the Postgres arm concatenates in SQL.
    // Same question, same groups, same text.
    if is_aggregate_case(name) {
        let sql = match name {
            "agg_count_all" => "SELECT count(*) AS n FROM place".to_owned(),
            "agg_count_kind" => "SELECT kind, count(*) AS n FROM place GROUP BY kind".to_owned(),
            "agg_sum_born_by_kind" => "SELECT kind, count(*) AS n, sum(born) AS s, \
                 min(born) AS lo, max(born) AS hi, avg(born) AS mean \
                 FROM place GROUP BY kind HAVING count(*) > 100"
                .to_owned(),
            "agg_distinct_kind" => "SELECT DISTINCT kind FROM place".to_owned(),
            "agg_count_radius_by_kind" => {
                let centre = sql_point(v, radius[0], radius[1], true);
                let clause = sql_dwithin(v, "loc", centre, radius[2]);
                format!("SELECT kind, count(*) AS n FROM place WHERE {clause} GROUP BY kind")
            }
            "agg_born_decade" => {
                "SELECT born / 10000 AS decade, count(*) AS n FROM place GROUP BY born / 10000"
                    .to_owned()
            }
            other => {
                return Err(
                    format!("battle50k: no e4-sql spelling for aggregate case `{other}`").into(),
                )
            }
        };
        return Ok((sql, bound.params, None));
    }

    let sql = match name {
        // ── filters ─────────────────────────────────────────────────────
        "pt_radius" => {
            let centre = sql_point(v, radius[0], radius[1], true);
            let clause = sql_dwithin(v, "loc", centre, radius[2]);
            format!("SELECT _id FROM place WHERE {clause}")
        }
        // Postgres reaches a lon/lat rectangle through `&&` plus ST_X/ST_Y,
        // both Tier 2 here; the Tier-1 spelling of the same rectangle is
        // ST_Within against an envelope, which IS `PointFilter::Bbox`.
        "pt_bbox" => {
            let envelope = sql_envelope(v, b);
            format!("SELECT _id FROM place WHERE ST_Within(loc::geometry, {envelope})")
        }
        "plot_within_box" => {
            let envelope = sql_envelope(v, b);
            format!("SELECT _id FROM place WHERE ST_Within(plot::geometry, {envelope})")
        }
        "plot_contains_pt" => {
            let centre = sql_point(v, point[0], point[1], false);
            format!("SELECT _id FROM place WHERE ST_Contains(plot::geometry, {centre})")
        }
        "plot_intersects" => {
            let polygon = sql_polygon(v, &q.polygon_json[i]);
            format!("SELECT _id FROM place WHERE ST_Intersects(plot, {polygon}::geography)")
        }
        "plot_dwithin_1km" => {
            let centre = sql_point(v, point[0], point[1], true);
            let clause = sql_dwithin(v, "plot", centre, 1_000.0);
            format!("SELECT _id FROM place WHERE {clause}")
        }
        "plot_vs_poly_within" => {
            let polygon = sql_polygon(v, &q.polygon_json[i]);
            format!("SELECT _id FROM place WHERE ST_Within(plot::geometry, {polygon})")
        }
        "text_one" => {
            let clause = sql_text_match(v, E4_TSVECTOR, &q.terms[i]);
            format!("SELECT _id FROM place WHERE {clause}")
        }
        "text_two" => {
            let pair = format!("{} & {}", q.terms[i], q.terms[(i + 1) % INSTANCES]);
            let clause = sql_text_match(v, E4_TSVECTOR, &pair);
            format!("SELECT _id FROM place WHERE {clause}")
        }
        "text_and_kind" => {
            let text = sql_text_match(v, E4_TSVECTOR, &q.terms[i]);
            let kind = sql_kind(v, &kind_value);
            format!("SELECT _id FROM place WHERE {text} AND {kind}")
        }
        "born_range" => {
            let born = sql_born(v, born_lower, born_upper);
            format!("SELECT _id FROM place WHERE {born}")
        }
        "kind_eq" => {
            let kind = sql_kind(v, &kind_value);
            format!("SELECT _id FROM place WHERE {kind}")
        }
        "radius_and_born" => {
            let centre = sql_point(v, radius[0], radius[1], true);
            let clause = sql_dwithin(v, "loc", centre, radius[2]);
            let born = sql_born(v, born_lower, born_upper);
            format!("SELECT _id FROM place WHERE {clause} AND {born}")
        }

        // ── boolean (QL_CONTRACT §3) ────────────────────────────────────
        "bool_kind_in3" => {
            let first = v.text(&kind_value);
            let second = v.text(&kinds[(i + 1) % KINDS]);
            let third = v.text(&kinds[(i + 2) % KINDS]);
            format!("SELECT _id FROM place WHERE kind IN ({first}, {second}, {third})")
        }
        "bool_born_or_kind" => {
            let born = sql_born(v, born_lower, born_upper);
            let kind = sql_kind(v, &kind_value);
            format!("SELECT _id FROM place WHERE ({born}) OR {kind}")
        }
        "bool_not_kind" => {
            let kind = v.text(&kind_value);
            format!("SELECT _id FROM place WHERE kind <> {kind}")
        }
        "bool_radius_or_radius" => {
            let centre = sql_point(v, radius[0], radius[1], true);
            let first = sql_dwithin(v, "loc", centre, radius[2]);
            let other = q.radii[(i + 1) % INSTANCES];
            let second_centre = sql_point(v, other[0], other[1], true);
            let second = sql_dwithin(v, "loc", second_centre, other[2]);
            format!("SELECT _id FROM place WHERE {first} OR {second}")
        }
        "bool_not_null_born" => {
            "SELECT _id FROM place WHERE born IS NOT NULL".to_owned()
        }
        "bool_exists_related" => format!(
            "SELECT _id FROM place WHERE EXISTS (SELECT 1 FROM {RELATED} WHERE source = _key)"
        ),
        // ── the function battery (QL_CONTRACT §4.1, §4.2) ───────────────
        // Written as Postgres writes them. Each WHERE below compiles to ONE
        // scalar range on the index the column carries -- `EXPLAIN` prints it
        // under `range rewrites` -- so what the timing compares is a fold at
        // prepare against the hand-written range in the `e4` arm.
        "fn_year_eq" => {
            let year = q.fn_year(i);
            format!("SELECT _id FROM place WHERE EXTRACT(YEAR FROM {BORN_TS}) = {year}")
        }
        "fn_trunc_month_range" => {
            let (from, to) = q.fn_month_window(i);
            let from = v.text(&from);
            let to = v.text(&to);
            format!(
                "SELECT _id FROM place WHERE date_trunc('month', {BORN_TS}) BETWEEN {from} AND {to}"
            )
        }
        "fn_lower_eq" => {
            let folded = v.text(&kind_value.to_lowercase());
            format!("SELECT _id FROM place WHERE lower(kind) = {folded}")
        }
        "fn_like_prefix" => {
            let pattern = v.text(&format!("{}%", q.name_prefix(i)));
            format!("SELECT _id FROM place WHERE name LIKE {pattern}")
        }
        "fn_project_strings" => {
            let born = sql_born(v, born_lower, born_upper);
            format!("SELECT upper(name), length(desc) FROM place WHERE {born}")
        }

        // ── ranked ──────────────────────────────────────────────────────
        "knn_10" => {
            let centre = sql_point(v, point[0], point[1], true);
            format!("SELECT _id FROM place ORDER BY loc <-> {centre} LIMIT {K}")
        }
        "knn_10_kind" => {
            let kind = sql_kind(v, &kind_value);
            let centre = sql_point(v, point[0], point[1], true);
            format!("SELECT _id FROM place WHERE {kind} ORDER BY loc <-> {centre} LIMIT {K}")
        }
        "text_top10" => {
            let clause = sql_text_match(v, E4_TSVECTOR, &q.terms[i]);
            let rank = sql_text_match(v, E4_TSVECTOR, &q.terms[i]);
            // `ts_rank_cd(tsvector, tsquery)` is the Bm25 order; the WHERE
            // repeats the same match, exactly as the Postgres statement does.
            let rank = rank.replace(" @@ ", ", ");
            format!(
                "SELECT _id FROM place WHERE {clause} ORDER BY ts_rank_cd({rank}) DESC LIMIT {K}"
            )
        }
        // Exactness here is which vector index the column has, not a planner
        // knob: with `ef_search` unset, the exact family answers.
        "vec_exact_10" => {
            let vector = sql_vector(v, &q.vector_literals[i], &q.vectors[i]);
            format!("SELECT _id FROM place ORDER BY emb <=> {vector} LIMIT {K}")
        }
        "vec_exact_radius" => {
            let centre = sql_point(v, radius[0], radius[1], true);
            let clause = sql_dwithin(v, "loc", centre, radius[2]);
            let vector = sql_vector(v, &q.vector_literals[i], &q.vectors[i]);
            format!("SELECT _id FROM place WHERE {clause} ORDER BY emb <=> {vector} LIMIT {K}")
        }
        "hybrid_10" => {
            let text = sql_text_match(v, E4_TSVECTOR, &q.terms[i]);
            let centre = sql_point(v, radius[0], radius[1], true);
            let clause = sql_dwithin(v, "loc", centre, radius[2]);
            let vector = sql_vector(v, &q.vector_literals[i], &q.vectors[i]);
            format!(
                "SELECT _id FROM place WHERE {text} AND {clause} ORDER BY emb <=> {vector} LIMIT {K}"
            )
        }
        // The blend Postgres writes with ts_rank_cd, written with bm25():
        // `1 - (emb <=> v)` is the cosine itself, the same quantity the `e4`
        // arm builds as `1 + VectorSimilarity`.
        "hybrid_blend_10" => {
            let text = sql_text_match(v, E4_TSVECTOR, &q.terms[i]);
            let centre = sql_point(v, radius[0], radius[1], true);
            let clause = sql_dwithin(v, "loc", centre, radius[2]);
            let term = v.text(&q.terms[i]);
            let vector = sql_vector(v, &q.vector_literals[i], &q.vectors[i]);
            format!(
                "SELECT _id FROM place WHERE {text} AND {clause} \
                 ORDER BY 0.5 * bm25(text, {term}) + 0.5 * (1 - (emb <=> {vector})) DESC LIMIT {K}"
            )
        }
        "vec_ann_10_kind:exact" => {
            let kind = sql_kind(v, &kind_value);
            let vector = sql_vector(v, &q.vector_literals[i], &q.vectors[i]);
            format!("SELECT _id FROM place WHERE {kind} ORDER BY emb <=> {vector} LIMIT {K}")
        }

        // ── the graph cases, as SQL/PGQ writes them ──────────────────────
        //
        // The inline element WHEREs are the per-hop prunes of
        // GRAPH_CONTRACT 4.3, not post-filters: the edge one compiles to
        // `edge_where`, the far node's to `node_where`, and EXPLAIN prints
        // both. `COLUMNS (r.weight AS w)` projects the reaching edge (4.2),
        // and `ORDER BY w DESC` ranks by it.
        "graph_2hop" => {
            let seed = v.text(q.seed_key(i)?);
            format!(
                "SELECT k FROM GRAPH_TABLE (base MATCH \
                 (a:place WHERE a._key = {seed})-[r:{RELATED}]->{{1,2}}(b:place) \
                 COLUMNS (b._id AS k))"
            )
        }
        "graph_2hop_weight" => {
            let seed = v.text(q.seed_key(i)?);
            format!(
                "SELECT k FROM GRAPH_TABLE (base MATCH \
                 (a:place WHERE a._key = {seed})-[r:{RELATED} WHERE r.weight > 0.5]->{{1,2}}\
                 (b:place) COLUMNS (b._id AS k))"
            )
        }
        "graph_2hop_born" => {
            let seed = v.text(q.seed_key(i)?);
            let lower = v.int(born_lower);
            let upper = v.int(born_upper);
            format!(
                "SELECT k FROM GRAPH_TABLE (base MATCH \
                 (a:place WHERE a._key = {seed})-[r:{RELATED}]->{{1,2}}\
                 (b:place WHERE b.born BETWEEN {lower} AND {upper}) COLUMNS (b._id AS k))"
            )
        }
        "graph_1hop_weight_top10" => {
            let seed = v.text(q.seed_key(i)?);
            format!(
                "SELECT k, w FROM GRAPH_TABLE (base MATCH \
                 (a:place WHERE a._key = {seed})<-[r:{RELATED}]-(b:place) \
                 COLUMNS (b._id AS k, r.weight AS w)) ORDER BY w DESC LIMIT {K}"
            )
        }
        other => return Err(format!("battle50k: no e4-sql spelling for case `{other}`").into()),
    };
    Ok((sql, bound.params, None))
}

/// Parse, compile and page one statement to exhaustion, translating every
/// returned `EntityId` through the load-time key vector of deviation 2 --
/// the identical translation the `e4` arm does.
/// The `e4-sql` arm of a PROJECTION-ONLY case: the statement's own columns,
/// row functions included, assembled the way `Database::sql` assembles them.
///
/// `e4sql_run` pages the engine's `QueryRow`, which never evaluates a §4.1 /
/// §4.2 row function; a case whose whole point is that per-row cost has to
/// pay it, or the arm would be timing a projection the statement did not ask
/// for. The answer key still comes from the row's entity sequence, so the
/// three arms compare the same ROW SET; the VALUES are checked against Rust's
/// own computation in `lang/tests/sql_functions.rs`.
fn e4sql_project_run(ctx: &E4Ctx, keys: &[String], sql: &str, params: &[Param]) -> R<Answer> {
    let t0 = Instant::now();
    let prepared = prepare_sql(&ctx.db, sql, params)?;
    let mut answer = e4sql_project_page(ctx, keys, &prepared)?;
    answer.prepare_us = t0.elapsed().as_secs_f64() * 1e6;
    Ok(answer)
}

/// The paging half of [`e4sql_project_run`], over a statement that is
/// already compiled. `--prepared` calls this after a REBIND.
fn e4sql_project_page(ctx: &E4Ctx, keys: &[String], prepared: &PreparedSql) -> R<Answer> {
    let mut answer = Answer::default();
    let mut sink = 0u64;
    prepared.for_each_row(&ctx.db, PAGE, &mut |row| {
        let ordinal = (row.id.sequence - 1) as usize;
        let key = keys.get(ordinal).ok_or_else(|| {
            SqlError::Engine("entity sequence falls outside the corpus's key vector".to_owned())
        })?;
        for value in &row.values {
            sink = sink.wrapping_add(match value {
                SqlValue::Text(text) => text.len() as u64,
                SqlValue::Int(n) => *n as u64,
                _ => 0,
            });
        }
        if dumping() || answer.keys.len() < FIRST_KEYS {
            answer.keys.push(key.clone());
        }
        answer.rows += 1;
        Ok(())
    })?;
    std::hint::black_box(sink);
    Ok(answer)
}

fn e4sql_run(ctx: &E4Ctx, keys: &[String], sql: &str, params: &[Param]) -> R<Answer> {
    let t0 = Instant::now();
    let prepared = prepare_sql(&ctx.db, sql, params)?;
    let mut answer = e4sql_page(ctx, keys, &prepared)?;
    answer.prepare_us = t0.elapsed().as_secs_f64() * 1e6;
    Ok(answer)
}

/// The paging half of [`e4sql_run`], over a statement that is already
/// compiled.
fn e4sql_page(ctx: &E4Ctx, keys: &[String], prepared: &PreparedSql) -> R<Answer> {
    let mut answer = Answer::default();
    prepared.with_query(&ctx.db, &mut |query| {
        loop {
            let page = query.next_page(PAGE, QueryBudget::unlimited(), || false)?;
            for row in &page.rows {
                let ordinal = (row.id.sequence - 1) as usize;
                let key = keys.get(ordinal).ok_or_else(|| {
                    SqlError::Engine(
                        "entity sequence falls outside the corpus's key vector".to_owned(),
                    )
                })?;
                if dumping() || answer.keys.len() < FIRST_KEYS {
                    answer.keys.push(key.clone());
                }
                answer.rows += 1;
            }
            if page.done || page.rows.is_empty() {
                break;
            }
        }
        Ok(())
    })?;
    Ok(answer)
}

/// The accumulator column names of one aggregate case, in request order --
/// the same names `agg_line` writes and the Postgres statement concatenates.
fn agg_fields(name: &str) -> &'static [&'static str] {
    match name {
        "agg_distinct_kind" => &[],
        "agg_sum_born_by_kind" => &["n", "s", "lo", "hi", "mean"],
        _ => &["n"],
    }
}

/// Parse, compile and page one aggregate statement, writing the same line per
/// group the `e4` arm writes.
fn e4sql_agg_run(ctx: &E4Ctx, sql: &str, params: &[Param], fields: &[&str]) -> R<Answer> {
    let t0 = Instant::now();
    let prepared = prepare_sql(&ctx.db, sql, params)?;
    let mut answer = e4sql_agg_page(ctx, &prepared, fields)?;
    answer.prepare_us = t0.elapsed().as_secs_f64() * 1e6;
    Ok(answer)
}

/// The paging half of [`e4sql_agg_run`], over a statement that is already
/// compiled.
fn e4sql_agg_page(ctx: &E4Ctx, prepared: &PreparedSql, fields: &[&str]) -> R<Answer> {
    let mut answer = Answer::default();
    prepared.with_aggregate(&ctx.db, &mut |aggregate| {
        loop {
            let page = aggregate.next_page(PAGE, QueryBudget::unlimited(), || false)?;
            for row in &page.groups {
                let key = match &row.key {
                    None => String::new(),
                    Some(OwnedScalarValue::Text(text)) => text.clone(),
                    Some(OwnedScalarValue::I64(value)) => value.to_string(),
                    Some(OwnedScalarValue::F64(value)) => format!("{value}"),
                    Some(OwnedScalarValue::Bool(value)) => value.to_string(),
                    Some(OwnedScalarValue::Nullish) => "NULL".to_owned(),
                };
                let mut numbers = Vec::with_capacity(fields.len());
                for (at, field) in fields.iter().enumerate() {
                    let value = row.values.get(at).ok_or_else(|| {
                        SqlError::Engine("an aggregate lost an accumulator".to_owned())
                    })?;
                    numbers.push((
                        *field,
                        match value {
                            AggValue::Count(n) => *n as i64,
                            AggValue::I64(v) => *v,
                            AggValue::F64(v) => v.floor() as i64,
                            other => {
                                return Err(SqlError::Engine(format!(
                                    "an aggregate produced {other:?}"
                                )))
                            }
                        },
                    ));
                }
                answer.push(agg_line(&key, &numbers));
            }
            if page.done || page.groups.is_empty() {
                break;
            }
        }
        Ok(())
    })?;
    Ok(answer)
}

fn e4sql_answer(ctx: &E4Ctx, corpus: &Corpus, q: &Queries, name: &str, i: usize) -> R<Answer> {
    let (sql, params, ef) = e4sql_case(q, &corpus.kinds, name, i)?;
    e4sql_set_ef(ctx, ef)?;
    if is_aggregate_case(name) {
        return e4sql_agg_run(ctx, &sql, &params, agg_fields(name));
    }
    if name == "fn_project_strings" {
        return e4sql_project_run(ctx, &corpus.keys, &sql, &params);
    }
    e4sql_run(ctx, &corpus.keys, &sql, &params)
}

/// One case measured with its statement PREPARED ONCE and RE-BOUND per
/// instance: the median whole call, the median bind on its own, and whether
/// the compiled form was rebindable (a refused rebind compiles again from
/// the statement parsed once, which is still one parse for the case).
///
/// `None` when the case's statement TEXT is not the same for every instance:
/// `fn_year_eq` and `agg_born_decade` write their number INTO the statement
/// rather than binding it, so there is no one statement to prepare, and the
/// report says so rather than timing fifty different prepares and calling it
/// a prepared statement.
fn e4sql_prepared_cost(
    ctx: &E4Ctx,
    corpus: &Corpus,
    q: &Queries,
    name: &str,
) -> R<Option<(f64, f64, bool)>> {
    let (first_sql, first_params, first_ef) = e4sql_case(q, &corpus.kinds, name, 0)?;
    for i in 1..INSTANCES {
        let (sql, _, ef) = e4sql_case(q, &corpus.kinds, name, i)?;
        if sql != first_sql || ef != first_ef {
            return Ok(None);
        }
    }
    e4sql_set_ef(ctx, first_ef)?;
    let mut prepared = prepare_sql(&ctx.db, &first_sql, &first_params)?;
    let rebindable = prepared.rebindable();
    let aggregate = is_aggregate_case(name);
    let project = name == "fn_project_strings";
    let fields = agg_fields(name);

    // One untimed warm pass, then one timed pass -- the same shape `measure`
    // uses, so the two medians are comparable.
    let once = |prepared: &mut PreparedSql, i: usize| -> R<(f64, f64, u64)> {
        let (_, params, _) = e4sql_case(q, &corpus.kinds, name, i)?;
        let at = Instant::now();
        prepared.bind(&ctx.db, &params)?;
        let bind = at.elapsed().as_secs_f64() * 1e6;
        let answer = if aggregate {
            e4sql_agg_page(ctx, prepared, fields)?
        } else if project {
            e4sql_project_page(ctx, &corpus.keys, prepared)?
        } else {
            e4sql_page(ctx, &corpus.keys, prepared)?
        };
        Ok((at.elapsed().as_secs_f64() * 1e6, bind, answer.rows))
    };
    for i in 0..INSTANCES {
        once(&mut prepared, i)?;
    }
    let mut walls = Vec::with_capacity(INSTANCES);
    let mut binds = Vec::with_capacity(INSTANCES);
    for i in 0..INSTANCES {
        let (wall, bind, _) = once(&mut prepared, i)?;
        walls.push(wall);
        binds.push(bind);
    }
    walls.sort_by(f64::total_cmp);
    binds.sort_by(f64::total_cmp);
    Ok(Some((
        percentile(&walls, 0.5),
        percentile(&binds, 0.5),
        rebindable,
    )))
}

/// Every prepared case answers what the unprepared one answers. Run once per
/// case, untimed, so `--prepared` cannot report a faster number for a
/// different question.
fn e4sql_prepared_agrees(ctx: &E4Ctx, corpus: &Corpus, q: &Queries, name: &str) -> R<bool> {
    let (sql, params, ef) = e4sql_case(q, &corpus.kinds, name, 0)?;
    // A case whose TEXT carries the value has no one statement to prepare,
    // so there is nothing here to agree or disagree with: it is measured
    // unprepared and reported as such.
    for i in 1..INSTANCES {
        let (other, _, other_ef) = e4sql_case(q, &corpus.kinds, name, i)?;
        if other != sql || other_ef != ef {
            return Ok(true);
        }
    }
    e4sql_set_ef(ctx, ef)?;
    let mut prepared = prepare_sql(&ctx.db, &sql, &params)?;
    let aggregate = is_aggregate_case(name);
    let project = name == "fn_project_strings";
    let fields = agg_fields(name);
    for i in 0..INSTANCES {
        let (sql, params, ef) = e4sql_case(q, &corpus.kinds, name, i)?;
        e4sql_set_ef(ctx, ef)?;
        prepared.bind(&ctx.db, &params)?;
        let bound = if aggregate {
            e4sql_agg_page(ctx, &prepared, fields)?
        } else if project {
            e4sql_project_page(ctx, &corpus.keys, &prepared)?
        } else {
            e4sql_page(ctx, &corpus.keys, &prepared)?
        };
        let fresh = e4sql_answer(ctx, corpus, q, name, i)?;
        if bound.rows != fresh.rows || bound.keys != fresh.keys {
            return Ok(false);
        }
        let _ = &sql;
    }
    Ok(true)
}

/// The median parse-and-compile cost of one case's statement, in
/// microseconds, over all fifty instances. It is the ONLY cost the `e4-sql`
/// arm has that the `e4` arm does not, so it is measured on its own rather
/// than inferred from the difference of two medians.
fn e4sql_parse_cost(ctx: &E4Ctx, corpus: &Corpus, q: &Queries, name: &str) -> R<f64> {
    let mut micros = Vec::with_capacity(INSTANCES);
    for i in 0..INSTANCES {
        let (sql, params, ef) = e4sql_case(q, &corpus.kinds, name, i)?;
        e4sql_set_ef(ctx, ef)?;
        let at = Instant::now();
        let prepared = prepare_sql(&ctx.db, &sql, &params)?;
        micros.push(at.elapsed().as_secs_f64() * 1e6);
        drop(prepared);
    }
    micros.sort_by(f64::total_cmp);
    Ok(percentile(&micros, 0.5))
}

/// One multi-row `INSERT ... VALUES (...), (...), ...` inside its own
/// transaction, the idiomatic shape a real client uses at this cadence.
fn pg_insert_batch(client: &mut Client, rows: &[Row]) -> R<()> {
    let mut sql = String::from(
        "INSERT INTO place (\"key\", name, descr, born, kind, loc, plot, emb) VALUES ",
    );
    let mut params: Vec<Box<dyn ToSql + Sync>> = Vec::with_capacity(rows.len() * 9);
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            sql.push(',');
        }
        let base = i * 9;
        sql.push_str(&format!(
            "(${},${},${},${},${},ST_SetSRID(ST_MakePoint(${},${}),4326)::geography,\
             ST_SetSRID(ST_GeomFromGeoJSON(${}),4326)::geography,${}::text::vector)",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6,
            base + 7,
            base + 8,
            base + 9
        ));
        params.push(Box::new(row.key.clone()));
        params.push(Box::new(row.name.clone()));
        params.push(Box::new(row.desc.clone()));
        params.push(Box::new(row.born as i32));
        params.push(Box::new(row.kind.clone()));
        params.push(Box::new(row.lon));
        params.push(Box::new(row.lat));
        params.push(Box::new(row.plot_json.clone()));
        params.push(Box::new(row.emb_literal.clone()));
    }
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
    let mut txn = client.transaction()?;
    txn.execute(sql.as_str(), &refs)?;
    txn.commit()?;
    Ok(())
}

fn load_pg(dsn: &str, corpus: &Corpus) -> R<(Client, Vec<Value>)> {
    let mut stages = Vec::new();
    let at = Instant::now();
    let mut client = Client::connect(dsn, NoTls)?;
    client.batch_execute(
        "CREATE EXTENSION IF NOT EXISTS postgis;
         CREATE EXTENSION IF NOT EXISTS vector;
         CREATE EXTENSION IF NOT EXISTS vectorscale;",
    )?;
    // `synchronous_commit` is ON by default on a stock server; it is set
    // explicitly anyway so the load pays a per-commit durability barrier.
    // Postgres issues an ordinary `fsync` for it, NOT `fcntl(F_FULLFSYNC)`,
    // which is why the E4 arm runs at `SyncMode::Normal` (`sync_data`): the
    // same class of primitive. E4's `SyncMode::Full` is the drive-cache
    // barrier and costs 11.9 ms against 1.45 ms on this volume; matching the
    // NAME of the setting while paying a barrier eight times dearer is not a
    // matched arm.
    client.batch_execute(
        "SET synchronous_commit = on;
         DROP TABLE IF EXISTS place;
         CREATE TABLE place (
             \"key\" text primary key,
             name text,
             descr text,
             born int,
             kind text,
             loc geography(Point,4326),
             plot geography(Polygon,4326),
             emb vector(32)
         );",
    )?;
    stages.push(stage("open", at.elapsed().as_secs_f64()));

    eprintln!("[postgres] inserting {} rows, commit every {BATCH} …", corpus.rows.len());
    let at = Instant::now();
    for chunk in corpus.rows.chunks(BATCH) {
        pg_insert_batch(&mut client, chunk)?;
    }
    stages.push(stage("load", at.elapsed().as_secs_f64()));

    // Late build, same as the E4 arm. There is no `place_emb_exact` here:
    // Postgres's exact vector answer is a sequential scan, not an index.
    for (name, ddl) in [
        (
            IX_TEXT,
            format!("CREATE INDEX {IX_TEXT} ON place USING gin ({PG_TSVECTOR})"),
        ),
        (IX_BORN, format!("CREATE INDEX {IX_BORN} ON place (born)")),
        (IX_KIND, format!("CREATE INDEX {IX_KIND} ON place (kind)")),
        (
            IX_LOC,
            format!("CREATE INDEX {IX_LOC} ON place USING gist (loc)"),
        ),
        (
            IX_PLOT,
            format!("CREATE INDEX {IX_PLOT} ON place USING gist (plot)"),
        ),
        (
            IX_EMB_ANN,
            format!("CREATE INDEX {IX_EMB_ANN} ON place USING diskann (emb vector_cosine_ops)"),
        ),
    ] {
        let at = Instant::now();
        client.batch_execute(&ddl)?;
        stages.push(stage(&format!("index:{name}"), at.elapsed().as_secs_f64()));
    }

    // Postgres's analog of E4's checkpoint: fresh planner statistics for the
    // indexes just built, then dirty buffers forced to disk.
    let at = Instant::now();
    client.batch_execute("ANALYZE place; CHECKPOINT;")?;
    stages.push(stage("checkpoint", at.elapsed().as_secs_f64()));
    Ok((client, stages))
}

/// Reopen a table an earlier pass loaded. `--reuse` is refused unless the
/// table already holds every row of the corpus, because a query-only pass
/// over a partial load would be a measurement of a different corpus.
fn open_pg(dsn: &str, expected_rows: usize) -> R<(Client, Vec<Value>)> {
    let at = Instant::now();
    let mut client = Client::connect(dsn, NoTls)?;
    let count: i64 = client.query_one("SELECT count(*) FROM place", &[])?.get(0);
    if count != expected_rows as i64 {
        return Err(format!(
            "--reuse: place holds {count} rows, expected {expected_rows}; load without --reuse"
        )
        .into());
    }
    let mut stages = vec![stage("open", at.elapsed().as_secs_f64()), skipped_stage("load")];
    for name in [IX_TEXT, IX_BORN, IX_KIND, IX_LOC, IX_PLOT, IX_EMB_ANN] {
        stages.push(skipped_stage(&format!("index:{name}")));
    }
    stages.push(skipped_stage("checkpoint"));
    eprintln!("[postgres] reopened {count} rows; queries only");
    Ok((client, stages))
}

/// Write the same `related` edges into Postgres, as a timed stage.
///
/// The table is `related(source text, destination text, weight real,
/// since int)` with `btree(source)`, which is the one index the two-hop
/// statements need to reach a row's neighbours. A second btree on
/// `destination` is created for the same reason in the other direction --
/// `graph_1hop_weight_top10` walks the fan-in, and E4's reverse mirror
/// (`GRAPH_CONTRACT` 2.2) is exactly that index, always written.
fn load_graph_pg(client: &mut Client, corpus: &Corpus, edges: &[Related]) -> R<Value> {
    let at = Instant::now();
    client.batch_execute(&format!(
        "DROP TABLE IF EXISTS {RELATED};
         CREATE TABLE {RELATED} (
             source text NOT NULL,
             destination text NOT NULL,
             weight real NOT NULL,
             since int NOT NULL
         );"
    ))?;
    eprintln!("[postgres] inserting {} `related` edges …", edges.len());
    for chunk in edges.chunks(BATCH) {
        let mut sql = format!("INSERT INTO {RELATED} (source, destination, weight, since) VALUES ");
        let mut params: Vec<Box<dyn ToSql + Sync>> = Vec::with_capacity(chunk.len() * 4);
        for (i, edge) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            let base = i * 4;
            sql.push_str(&format!(
                "(${},${},${}::float8::real,${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4
            ));
            params.push(Box::new(corpus.rows[edge.source].key.clone()));
            params.push(Box::new(corpus.rows[edge.destination].key.clone()));
            params.push(Box::new(edge.weight));
            params.push(Box::new(edge.since as i32));
        }
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(AsRef::as_ref).collect();
        let mut tx = client.transaction()?;
        tx.execute(sql.as_str(), &refs)?;
        tx.commit()?;
    }
    client.batch_execute(&format!(
        "CREATE INDEX {RELATED}_source ON {RELATED} USING btree (source);
         CREATE INDEX {RELATED}_destination ON {RELATED} USING btree (destination);
         ANALYZE {RELATED};"
    ))?;
    Ok(stage("graph", at.elapsed().as_secs_f64()))
}

/// The Postgres write case: a scratch table rebuilt before every instance,
/// with the diskann index LIVE on it before the first row lands.
///
/// It is a SEPARATE table, not `place`, for the same two reasons the E4 arm
/// uses a separate database: `place` is reused across runs, and
/// `pg_total_relation_size('place')` is the arm's `disk_bytes`. The table is
/// dropped when the case ends.
fn pg_bulk_reset(client: &mut Client, indexed: bool) -> R<()> {
    client.batch_execute(&format!("DROP TABLE IF EXISTS {VEC_BULK_OBJECT}"))?;
    client.batch_execute(&format!(
        "CREATE TABLE {VEC_BULK_OBJECT} (key text primary key, emb vector({DIM}))"
    ))?;
    if indexed {
        client.batch_execute(&format!(
            "CREATE INDEX {VEC_BULK_OBJECT}_emb_ann ON {VEC_BULK_OBJECT} \
             USING diskann (emb vector_cosine_ops)"
        ))?;
    }
    Ok(())
}

fn pg_bulk_write(client: &mut Client, corpus: &Corpus, name: &str, i: usize) -> R<(f64, Answer)> {
    let batch: Vec<(String, String)> = (0..VEC_BULK_ROWS)
        .map(|at| bulk_row(corpus, i, at).map(|(key, emb)| (key, vector_literal(emb))))
        .collect::<R<Vec<_>>>()?;
    pg_bulk_reset(client, write_case_is_indexed(name))?;
    // `$2::text::vector` for the same reason `pg_insert_batch` spells it that
    // way: a bare `$2::vector` makes the server resolve the placeholder as
    // `vector`, and the Rust client has no `ToSql` for that type.
    let statement = client.prepare(&format!(
        "INSERT INTO {VEC_BULK_OBJECT} (key, emb) VALUES ($1, $2::text::vector)"
    ))?;
    let started = Instant::now();
    client.batch_execute("BEGIN")?;
    for (key, literal) in &batch {
        client.execute(&statement, &[key, literal])?;
    }
    client.batch_execute("COMMIT")?;
    let micros = started.elapsed().as_secs_f64() * 1e6;
    let mut answer = Answer::default();
    for (key, _) in &batch {
        answer.push(key.clone());
    }
    Ok((micros, answer))
}

fn pg_disk_bytes(client: &mut Client) -> R<u64> {
    let size: i64 = client
        .query_one("SELECT pg_total_relation_size('place')", &[])?
        .get(0);
    Ok(size as u64)
}

/// Whether this server's diskann build exposes `diskann.query_rescore`,
/// probed by trying `SET LOCAL` and catching an "unrecognized configuration
/// parameter" error, INSIDE its own transaction so the probe rolls back
/// rather than either persisting the setting or poisoning a later statement
/// with an aborted transaction. `pg_settings` is not trusted for this: on
/// this loop's own server it holds zero `diskann.%` rows until the diskann
/// index has actually been touched once by an index-using statement in the
/// current backend, because the GUCs are registered when pgvectorscale's
/// library loads, not merely when `CREATE EXTENSION` runs (see the
/// deviation on `place_emb_ann`'s build parameters).
fn pg_has_query_rescore(client: &mut Client) -> bool {
    let mut txn = match client.transaction() {
        Ok(txn) => txn,
        Err(_) => return false,
    };
    let ok = txn.batch_execute("SET LOCAL diskann.query_rescore = 50").is_ok();
    drop(txn); // uncommitted, so this rolls back rather than sticking.
    ok
}

// ── the sqlite arm ────────────────────────────────────────────────────────
//
// The same forty-one cases asked of an embedded SQLite 3.46 (rusqlite's
// bundled build) holding one `place` table, an FTS5 index over the same
// concatenation E4 indexes, two R*Trees, five ordinary indexes and — under
// `--graph` — the same `related` edge set. SQLite has no geometry type, no
// geodesic, no vector index and no traversal atomic, so every one of those
// is a REGISTERED SCALAR FUNCTION that calls the very routine E4's own
// refine calls: the oracle is the same maths, and the row counts can
// therefore be compared rather than excused. Where SQLite cannot express a
// case at all the arm reports `n/a: <reason>`; it never skips one.

/// The names the stages block uses, so the four reports line up stage for
/// stage. `place_text` is the FTS5 table, `place_loc` the point R*Tree and
/// `place_plot` the plot-bounding-box R*Tree.
const LITE_FTS: &str = "place_fts";
const LITE_POINT_RT: &str = "place_rt";
const LITE_PLOT_RT: &str = "plot_rt";

/// How much wider than the true box an R*Tree row is stored and queried.
///
/// An `rtree` column holds a 32-bit float, whose relative precision is
/// 6.0e-8, so a box written or read at f64 precision can be rounded INWARD
/// by up to one f32 ulp — about 8 m in longitude at this corpus's 107°E.
/// A candidate box that lost a row to rounding would make this arm answer a
/// different question than E4, so every stored box and every query box is
/// widened by 1.0e-6 relative (about 0.12 m at 107°, sixteen times the ulp)
/// plus 1.0e-9 absolute for values near zero. Widening a CANDIDATE changes
/// the cost and never the answer: every one is followed by an exact refine.
const RT_EPS_REL: f64 = 1e-6;
const RT_EPS_ABS: f64 = 1e-9;

/// The radii the k-nearest ladder tries, in metres, smallest first. SQLite's
/// R*Tree has no nearest-neighbour cursor, so `knn_10` asks for the ten
/// nearest inside a box of this radius and accepts the answer only when it
/// holds ten rows AND the tenth is no further than the radius — at which
/// point no row outside the box can displace one inside it. A run that
/// exhausts the ladder falls back to a whole-corpus scan.
const KNN_RING_METRES: [f64; 7] = [
    250.0, 1_000.0, 4_000.0, 16_000.0, 64_000.0, 256_000.0, 1_024_000.0,
];

/// SQLite's default page size in the bundled 3.46 build, recorded so the
/// disk comparison names it rather than implying the four arms agreed on one.
const LITE_PAGE_BYTES: i64 = 4_096;

fn rt_pad(value: f64) -> f64 {
    value.abs() * RT_EPS_REL + RT_EPS_ABS
}

/// `[west, east, south, north]` widened outward, in the order an R*Tree
/// OVERLAP constraint binds them.
fn rt_overlap(west: f64, east: f64, south: f64, north: f64) -> [LiteValue; 4] {
    [
        LiteValue::Real(west - rt_pad(west)),
        LiteValue::Real(east + rt_pad(east)),
        LiteValue::Real(south - rt_pad(south)),
        LiteValue::Real(north + rt_pad(north)),
    ]
}

fn bbox_ring(ring: &[[f64; 2]], into: &mut (f64, f64, f64, f64)) {
    for p in ring {
        into.0 = into.0.min(p[0]);
        into.1 = into.1.max(p[0]);
        into.2 = into.2.min(p[1]);
        into.3 = into.3.max(p[1]);
    }
}

/// `(west, east, south, north)` of any geometry, which is the box the plot
/// R*Tree stores and the box a geometry predicate's candidate must overlap.
/// Every predicate in the battery is monotone in this box — a geometry that
/// is within, contains, intersects or is near another has a bounding box
/// that overlaps the other's (grown by the distance, for `DWithin`) — so the
/// overlap test is a true superset of each one.
fn geom_bbox(g: &Geom) -> (f64, f64, f64, f64) {
    let mut b = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    match g {
        Geom::Point(x, y) => bbox_ring(&[[*x, *y]], &mut b),
        Geom::LineString(c) | Geom::MultiPoint(c) => bbox_ring(c, &mut b),
        Geom::Polygon(rings) | Geom::MultiLineString(rings) => {
            for ring in rings {
                bbox_ring(ring, &mut b);
            }
        }
        Geom::MultiPolygon(polygons) => {
            for polygon in polygons {
                for ring in polygon {
                    bbox_ring(ring, &mut b);
                }
            }
        }
    }
    b
}

/// The 32 lanes of an `emb` as the BLOB the `place` table stores: little-endian
/// f32, exactly the lanes E4's dense-v3 vector keyspace holds.
fn emb_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// `1 - cos(a, b)` over two little-endian f32 BLOBs — the same quantity
/// pgvector writes `a <=> b` and `VectorMetric::Cosine` ranks by.
fn cosine_distance(a: &[u8], b: &[u8]) -> Result<f64, String> {
    if a.len() != b.len() || a.len() % 4 != 0 {
        return Err(format!(
            "cosine over {} and {} bytes: both must be the same multiple of four",
            a.len(),
            b.len()
        ));
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for lane in 0..a.len() / 4 {
        let at = lane * 4;
        let x = f64::from(f32::from_le_bytes([a[at], a[at + 1], a[at + 2], a[at + 3]]));
        let y = f64::from(f32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denominator = (na.sqrt() * nb.sqrt()).max(1e-12);
    Ok(1.0 - dot / denominator)
}

fn lite_error(message: String) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(message.into())
}

/// The geometry a constant argument names, parsed ONCE per statement.
///
/// SQLite keeps auxiliary data only for arguments it proved constant at
/// prepare time, and every call below passes the query geometry as a BOUND
/// PARAMETER, so the parse is paid once per statement rather than once per
/// row. The stored `plot` column is not constant and is parsed per row; that
/// cost is a named deviation, not a hidden one.
fn lite_geom_arg(
    ctx: &rusqlite::functions::Context<'_>,
    at: usize,
) -> rusqlite::Result<std::sync::Arc<Geom>> {
    if let Some(cached) = ctx.get_aux::<Geom>(at as std::os::raw::c_int)? {
        return Ok(cached);
    }
    let geom = lite_geom_value(ctx, at)?;
    ctx.set_aux(at as std::os::raw::c_int, geom)
}

fn lite_geom_value(ctx: &rusqlite::functions::Context<'_>, at: usize) -> rusqlite::Result<Geom> {
    let text = ctx.get_raw(at).as_str()?;
    let value: Value =
        serde_json::from_str(text).map_err(|e| lite_error(format!("geometry is not JSON: {e}")))?;
    geom_from_json(&value).map_err(|e| lite_error(format!("geometry is not GeoJSON: {e}")))
}

/// Register the six functions every case that SQLite cannot express natively
/// is written against. Each one calls the routine E4's own refine calls, so
/// the two arms agree on the maths and any row-count difference is a real
/// difference and not a second implementation of a predicate.
fn lite_register(conn: &Connection) -> R<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    conn.create_scalar_function("geo_dist_m", 4, flags, |ctx| {
        let a = Point::new(ctx.get::<f64>(0)?, ctx.get::<f64>(1)?)
            .map_err(|e| lite_error(format!("geo_dist_m first point: {e}")))?;
        let b = Point::new(ctx.get::<f64>(2)?, ctx.get::<f64>(3)?)
            .map_err(|e| lite_error(format!("geo_dist_m second point: {e}")))?;
        Ok(wgs84_distance_metres(a, b))
    })?;
    conn.create_scalar_function("vec_cos_dist", 2, flags, |ctx| {
        let a = ctx.get_raw(0).as_blob()?;
        let b = ctx.get_raw(1).as_blob()?;
        cosine_distance(a, b).map_err(lite_error)
    })?;
    conn.create_scalar_function("geom_within", 2, flags, |ctx| {
        let a = lite_geom_value(ctx, 0)?;
        let b = lite_geom_arg(ctx, 1)?;
        Ok(spatial_geometry::within(&a, &b))
    })?;
    conn.create_scalar_function("geom_contains", 2, flags, |ctx| {
        let a = lite_geom_value(ctx, 0)?;
        let b = lite_geom_arg(ctx, 1)?;
        Ok(spatial_geometry::contains(&a, &b))
    })?;
    conn.create_scalar_function("geom_intersects", 2, flags, |ctx| {
        let a = lite_geom_value(ctx, 0)?;
        let b = lite_geom_arg(ctx, 1)?;
        Ok(spatial_geometry::intersects(&a, &b))
    })?;
    conn.create_scalar_function("geom_dwithin", 3, flags, |ctx| {
        let a = lite_geom_value(ctx, 0)?;
        let b = lite_geom_arg(ctx, 1)?;
        let metres: f64 = ctx.get(2)?;
        Ok(spatial_geometry::dwithin_m(&a, &b, metres))
    })?;
    Ok(())
}

/// How a value reaches a SQLite statement: bound as `?n`, never inlined, so
/// the fifty instances of one case share ONE statement text and therefore one
/// entry in the prepared-statement cache. `Binder` above is the `e4-sql`
/// arm's `$n` spelling; this is SQLite's.
#[derive(Default)]
struct LiteBinder {
    params: Vec<LiteValue>,
}

impl LiteBinder {
    fn mark(&mut self) -> String {
        format!("?{}", self.params.len())
    }
    fn real(&mut self, value: f64) -> String {
        self.params.push(LiteValue::Real(value));
        self.mark()
    }
    fn int(&mut self, value: i64) -> String {
        self.params.push(LiteValue::Integer(value));
        self.mark()
    }
    fn text(&mut self, value: &str) -> String {
        self.params.push(LiteValue::Text(value.to_owned()));
        self.mark()
    }
    fn blob(&mut self, value: Vec<u8>) -> String {
        self.params.push(LiteValue::Blob(value));
        self.mark()
    }
}

/// `rt.<overlaps the widened box>` — the R*Tree candidate every spatial case
/// starts from. OVERLAP, never containment: an R*Tree row is a box rounded
/// outward, so `maxlon >= west AND minlon <= east AND ...` can only ever
/// admit too much, which the exact refine then removes.
fn lite_rt_clause(v: &mut LiteBinder, west: f64, east: f64, south: f64, north: f64) -> String {
    let values = rt_overlap(west, east, south, north);
    let mut marks = Vec::with_capacity(4);
    for value in values {
        v.params.push(value);
        marks.push(v.mark());
    }
    format!(
        "rt.maxlon >= {} AND rt.minlon <= {} AND rt.maxlat >= {} AND rt.minlat <= {}",
        marks[0], marks[1], marks[2], marks[3]
    )
}

/// The candidate box and the exact refine of a geodesic radius over `loc`.
/// The box is `radius_candidate_bounds` — the very cover E4's point index
/// walks — so the candidate set is E4's candidate set and the refine is
/// E4's refine.
fn lite_radius(v: &mut LiteBinder, centre: Point, metres: f64) -> R<(String, String)> {
    let bounds = radius_candidate_bounds(centre, metres)
        .map_err(|e| format!("radius candidate bounds: {e}"))?;
    let rt = lite_rt_clause(
        v,
        bounds.west(),
        bounds.east(),
        bounds.south(),
        bounds.north(),
    );
    let lon = v.real(centre.longitude());
    let lat = v.real(centre.latitude());
    let limit = v.real(metres);
    Ok((rt, format!("geo_dist_m(p.lon, p.lat, {lon}, {lat}) <= {limit}")))
}

/// The R*Tree candidate for a geometry predicate: the query geometry's own
/// bounding box.
fn lite_geom_rt(v: &mut LiteBinder, g: &Geom) -> String {
    let (west, east, south, north) = geom_bbox(g);
    lite_rt_clause(v, west, east, south, north)
}

/// An FTS5 match expression over the one indexed column. Terms are quoted so
/// a corpus word that happened to be an FTS5 keyword would still be a term.
fn lite_fts_query(terms: &[&str]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// What one case, at one query instance, asks SQLite.
enum LitePlan {
    /// One statement whose first column is the answer — a key, or the text
    /// line an aggregate group folds to.
    Rows { sql: String, params: Vec<LiteValue> },
    /// A projection-only case: the first column is the key and every other
    /// column is read, because the per-row function cost IS the case.
    Project { sql: String, params: Vec<LiteValue> },
    /// k-nearest by a widening R*Tree ring, with a whole-corpus scan behind
    /// it. Two statements, because SQLite has no nearest-neighbour cursor.
    Nearest {
        ring_sql: String,
        scan_sql: String,
        centre: [f64; 2],
        kind: Option<String>,
    },
    /// A case SQLite cannot express. The reason is reported as the case's
    /// `n/a` note; the case is never silently dropped.
    NotExpressible(String),
}

/// One case, one query instance, as SQLite spells it.
fn lite_case(q: &Queries, kinds: &[String], name: &str, i: usize) -> R<LitePlan> {
    let mut bound = LiteBinder::default();
    let v = &mut bound;
    let kind_value = kinds[i % KINDS].clone();
    let point = q.points[i];
    let b = q.boxes[i];
    let (born_lower, born_upper) = q.born_range(i);

    // The approximate sweep's own axes. E4 sweeps `ef` and Postgres sweeps
    // `diskann.query_search_list_size`; SQLite has neither, because it has no
    // approximate vector index at all. Naming the refusal is the point.
    if parse_approx_sweep(name).is_some() || name.ends_with("@ann") {
        return Ok(LitePlan::NotExpressible(
            "SQLite has no approximate vector index and no knob to sweep: there is no ANN \
             family in the bundled build and no extension is loaded. The arm answers the \
             same question by whole-corpus scan instead, reported as the `@scan` point of \
             this sweep, whose recall against this arm's own exact answer is 1.000 by \
             construction."
                .to_owned(),
        ));
    }

    // `<base>@scan`: the exact answer, standing in for the approximate point
    // the other two arms sweep.
    if let Some(base) = name.strip_suffix("@scan") {
        let vector = v.blob(emb_blob(&q.vectors[i]));
        let sql = match base {
            "vec_ann_10" => {
                format!(
                    "SELECT \"key\" FROM place ORDER BY vec_cos_dist(emb, {vector}), rowid \
                     LIMIT {K}"
                )
            }
            "vec_ann_10_kind" => {
                let kind = v.text(&kind_value);
                format!(
                    "SELECT \"key\" FROM place WHERE kind = {kind} \
                     ORDER BY vec_cos_dist(emb, {vector}), rowid LIMIT {K}"
                )
            }
            other => return Err(format!("battle50k: no SQLite scan point for `{other}`").into()),
        };
        return Ok(LitePlan::Rows { sql, params: bound.params });
    }

    // ── the aggregate battery (QL_CONTRACT §4.7) ────────────────────────
    // One text column per group, formatted exactly as `agg_line` formats it
    // in the two E4 arms and as the Postgres statement concatenates it, so
    // `--dump` diffs the VALUES and not only the group count. `avg` is
    // `CAST(avg(born) AS INTEGER)`: SQLite's bundled build has no `floor()`
    // (SQLITE_ENABLE_MATH_FUNCTIONS is not defined) and every `born` is
    // positive, so the cast's truncation toward zero IS the floor.
    if is_aggregate_case(name) {
        let sql = match name {
            "agg_count_all" => "SELECT '|n=' || count(*) FROM place".to_owned(),
            "agg_count_kind" => {
                "SELECT kind || '|n=' || count(*) FROM place GROUP BY kind ORDER BY kind".to_owned()
            }
            "agg_sum_born_by_kind" => "SELECT kind || '|n=' || count(*) || '|s=' || sum(born) \
                 || '|lo=' || min(born) || '|hi=' || max(born) \
                 || '|mean=' || CAST(avg(born) AS INTEGER) \
                 FROM place GROUP BY kind HAVING count(*) > 100 ORDER BY kind"
                .to_owned(),
            "agg_distinct_kind" => "SELECT DISTINCT kind FROM place ORDER BY kind".to_owned(),
            "agg_count_radius_by_kind" => {
                let centre = q.radius_centre(i)?;
                let (rt, refine) = lite_radius(v, centre, q.radius_metres(i))?;
                format!(
                    "SELECT p.kind || '|n=' || count(*) \
                     FROM {LITE_POINT_RT} rt JOIN place p ON p.rowid = rt.id \
                     WHERE {rt} AND {refine} GROUP BY p.kind ORDER BY p.kind"
                )
            }
            "agg_born_decade" => "SELECT (born / 10000) || '|n=' || count(*) FROM place \
                 GROUP BY born / 10000 ORDER BY born / 10000"
                .to_owned(),
            other => {
                return Err(
                    format!("battle50k: no SQLite spelling for aggregate case `{other}`").into(),
                )
            }
        };
        return Ok(LitePlan::Rows { sql, params: bound.params });
    }

    let sql = match name {
        // ── filters ─────────────────────────────────────────────────────
        "pt_radius" => {
            let (rt, refine) = lite_radius(v, q.radius_centre(i)?, q.radius_metres(i))?;
            format!(
                "SELECT p.\"key\" FROM {LITE_POINT_RT} rt JOIN place p ON p.rowid = rt.id \
                 WHERE {rt} AND {refine}"
            )
        }
        // The R*Tree candidate is the box rounded outward; the refine is the
        // inclusive lon/lat comparison `Bounds::contains` makes.
        "pt_bbox" => {
            let rt = lite_rt_clause(v, b[0], b[1], b[2], b[3]);
            let west = v.real(b[0]);
            let east = v.real(b[1]);
            let south = v.real(b[2]);
            let north = v.real(b[3]);
            format!(
                "SELECT p.\"key\" FROM {LITE_POINT_RT} rt JOIN place p ON p.rowid = rt.id \
                 WHERE {rt} AND p.lon BETWEEN {west} AND {east} \
                 AND p.lat BETWEEN {south} AND {north}"
            )
        }
        "plot_within_box" => {
            let box_polygon = q.box_polygon(i);
            let rt = lite_geom_rt(v, &box_polygon);
            let json = v.text(&geom_to_json(&box_polygon).to_string());
            format!(
                "SELECT p.\"key\" FROM {LITE_PLOT_RT} rt JOIN place p ON p.rowid = rt.id \
                 WHERE {rt} AND geom_within(p.plot, {json})"
            )
        }
        "plot_contains_pt" => {
            let probe = Geom::Point(point[0], point[1]);
            let rt = lite_geom_rt(v, &probe);
            let json = v.text(&geom_to_json(&probe).to_string());
            format!(
                "SELECT p.\"key\" FROM {LITE_PLOT_RT} rt JOIN place p ON p.rowid = rt.id \
                 WHERE {rt} AND geom_contains(p.plot, {json})"
            )
        }
        "plot_intersects" => {
            let rt = lite_geom_rt(v, &q.polygons[i]);
            let json = v.text(&q.polygon_json[i]);
            format!(
                "SELECT p.\"key\" FROM {LITE_PLOT_RT} rt JOIN place p ON p.rowid = rt.id \
                 WHERE {rt} AND geom_intersects(p.plot, {json})"
            )
        }
        // The candidate is the point's own 1 km cover, because a plot within
        // a kilometre of the point has a bounding box that meets that cover.
        "plot_dwithin_1km" => {
            let probe = Geom::Point(point[0], point[1]);
            let centre = q.point(i)?;
            let bounds = radius_candidate_bounds(centre, 1_000.0)
                .map_err(|e| format!("plot_dwithin_1km candidate bounds: {e}"))?;
            let rt = lite_rt_clause(
                v,
                bounds.west(),
                bounds.east(),
                bounds.south(),
                bounds.north(),
            );
            let json = v.text(&geom_to_json(&probe).to_string());
            format!(
                "SELECT p.\"key\" FROM {LITE_PLOT_RT} rt JOIN place p ON p.rowid = rt.id \
                 WHERE {rt} AND geom_dwithin(p.plot, {json}, 1000.0)"
            )
        }
        "plot_vs_poly_within" => {
            let rt = lite_geom_rt(v, &q.polygons[i]);
            let json = v.text(&q.polygon_json[i]);
            format!(
                "SELECT p.\"key\" FROM {LITE_PLOT_RT} rt JOIN place p ON p.rowid = rt.id \
                 WHERE {rt} AND geom_within(p.plot, {json})"
            )
        }
        "text_one" => {
            let query = v.text(&lite_fts_query(&[&q.terms[i]]));
            format!(
                "SELECT p.\"key\" FROM {LITE_FTS} JOIN place p ON p.rowid = {LITE_FTS}.rowid \
                 WHERE {LITE_FTS} MATCH {query}"
            )
        }
        "text_two" => {
            let query = v.text(&lite_fts_query(&[
                &q.terms[i],
                &q.terms[(i + 1) % INSTANCES],
            ]));
            format!(
                "SELECT p.\"key\" FROM {LITE_FTS} JOIN place p ON p.rowid = {LITE_FTS}.rowid \
                 WHERE {LITE_FTS} MATCH {query}"
            )
        }
        "text_and_kind" => {
            let query = v.text(&lite_fts_query(&[&q.terms[i]]));
            let kind = v.text(&kind_value);
            format!(
                "SELECT p.\"key\" FROM {LITE_FTS} JOIN place p ON p.rowid = {LITE_FTS}.rowid \
                 WHERE {LITE_FTS} MATCH {query} AND p.kind = {kind}"
            )
        }
        "born_range" => {
            let lower = v.int(born_lower);
            let upper = v.int(born_upper);
            format!("SELECT \"key\" FROM place WHERE born BETWEEN {lower} AND {upper}")
        }
        "kind_eq" => {
            let kind = v.text(&kind_value);
            format!("SELECT \"key\" FROM place WHERE kind = {kind}")
        }
        "radius_and_born" => {
            let (rt, refine) = lite_radius(v, q.radius_centre(i)?, q.radius_metres(i))?;
            let lower = v.int(born_lower);
            let upper = v.int(born_upper);
            format!(
                "SELECT p.\"key\" FROM {LITE_POINT_RT} rt JOIN place p ON p.rowid = rt.id \
                 WHERE {rt} AND {refine} AND p.born BETWEEN {lower} AND {upper}"
            )
        }

        // ── boolean (QL_CONTRACT §3) ────────────────────────────────────
        "bool_kind_in3" => {
            let first = v.text(&kind_value);
            let second = v.text(&kinds[(i + 1) % KINDS]);
            let third = v.text(&kinds[(i + 2) % KINDS]);
            format!("SELECT \"key\" FROM place WHERE kind IN ({first}, {second}, {third})")
        }
        "bool_born_or_kind" => {
            let lower = v.int(born_lower);
            let upper = v.int(born_upper);
            let kind = v.text(&kind_value);
            format!(
                "SELECT \"key\" FROM place WHERE (born BETWEEN {lower} AND {upper}) \
                 OR kind = {kind}"
            )
        }
        "bool_not_kind" => {
            let kind = v.text(&kind_value);
            format!("SELECT \"key\" FROM place WHERE kind <> {kind}")
        }
        // Two R*Tree candidate sets, each exactly refined, UNIONed: the union
        // of two covers, which is what QueryFilter::Any over two radius
        // leaves is. UNION, not UNION ALL, because a row inside both circles
        // is one row of one membership set.
        "bool_radius_or_radius" => {
            let (first_rt, first_refine) =
                lite_radius(v, q.radius_centre(i)?, q.radius_metres(i))?;
            let other = (i + 1) % INSTANCES;
            let (second_rt, second_refine) =
                lite_radius(v, q.radius_centre(other)?, q.radius_metres(other))?;
            format!(
                "SELECT \"key\" FROM ( \
                   SELECT p.\"key\" AS \"key\" FROM {LITE_POINT_RT} rt \
                     JOIN place p ON p.rowid = rt.id WHERE {first_rt} AND {first_refine} \
                   UNION \
                   SELECT p.\"key\" FROM {LITE_POINT_RT} rt \
                     JOIN place p ON p.rowid = rt.id WHERE {second_rt} AND {second_refine} \
                 )"
            )
        }
        "bool_not_null_born" => "SELECT \"key\" FROM place WHERE born IS NOT NULL".to_owned(),
        "bool_exists_related" => format!(
            "SELECT p.\"key\" FROM place p \
             WHERE EXISTS (SELECT 1 FROM {RELATED} r WHERE r.source = p.\"key\")"
        ),

        // ── the function battery (QL_CONTRACT §4.1, §4.2) ───────────────
        // SQLite has no date type and no `EXTRACT`, so the two date cases are
        // written as the RANGE the other three arms fold their function into.
        // That is a deviation in the SPELLING and not in the question: the
        // bounds are `micros_of` in every arm.
        "fn_year_eq" => {
            let year = q.fn_year(i);
            let lower = v.int(micros_of(year, 1, 1));
            let upper = v.int(micros_of(year + 1, 1, 1));
            format!("SELECT \"key\" FROM place WHERE {BORN_TS} >= {lower} AND {BORN_TS} < {upper}")
        }
        "fn_trunc_month_range" => {
            let year = q.fn_year(i);
            let lower = v.int(micros_of(year, 1, 1));
            let upper = v.int(micros_of(year, 7, 1));
            format!("SELECT \"key\" FROM place WHERE {BORN_TS} >= {lower} AND {BORN_TS} < {upper}")
        }
        "fn_lower_eq" => {
            let folded = v.text(&kind_value.to_lowercase());
            format!("SELECT \"key\" FROM place WHERE lower(kind) = {folded}")
        }
        "fn_like_prefix" => {
            let pattern = v.text(&format!("{}%", q.name_prefix(i)));
            format!("SELECT \"key\" FROM place WHERE name LIKE {pattern}")
        }
        "fn_project_strings" => {
            let lower = v.int(born_lower);
            let upper = v.int(born_upper);
            return Ok(LitePlan::Project {
                sql: format!(
                    "SELECT \"key\", upper(name), length(descr) FROM place \
                     WHERE born BETWEEN {lower} AND {upper}"
                ),
                params: bound.params,
            });
        }

        // ── ranked ──────────────────────────────────────────────────────
        "knn_10" | "knn_10_kind" => {
            let by_kind = name == "knn_10_kind";
            let kind_clause = if by_kind { " AND p.kind = ?7" } else { "" };
            let scan_kind = if by_kind { " WHERE kind = ?3" } else { "" };
            return Ok(LitePlan::Nearest {
                ring_sql: format!(
                    "SELECT p.\"key\", geo_dist_m(p.lon, p.lat, ?1, ?2) AS d \
                     FROM {LITE_POINT_RT} rt JOIN place p ON p.rowid = rt.id \
                     WHERE rt.maxlon >= ?3 AND rt.minlon <= ?4 AND rt.maxlat >= ?5 \
                     AND rt.minlat <= ?6{kind_clause} ORDER BY d, p.rowid LIMIT {K}"
                ),
                scan_sql: format!(
                    "SELECT \"key\", geo_dist_m(lon, lat, ?1, ?2) AS d FROM place{scan_kind} \
                     ORDER BY d, rowid LIMIT {K}"
                ),
                centre: point,
                kind: by_kind.then(|| kind_value.clone()),
            });
        }
        // FTS5's bm25() is negative-better, so ascending IS best-first.
        "text_top10" => {
            let query = v.text(&lite_fts_query(&[&q.terms[i]]));
            format!(
                "SELECT p.\"key\" FROM {LITE_FTS} JOIN place p ON p.rowid = {LITE_FTS}.rowid \
                 WHERE {LITE_FTS} MATCH {query} ORDER BY bm25({LITE_FTS}), p.rowid LIMIT {K}"
            )
        }
        "vec_exact_10" => {
            let vector = v.blob(emb_blob(&q.vectors[i]));
            format!(
                "SELECT \"key\" FROM place ORDER BY vec_cos_dist(emb, {vector}), rowid LIMIT {K}"
            )
        }
        "vec_exact_radius" => {
            let (rt, refine) = lite_radius(v, q.radius_centre(i)?, q.radius_metres(i))?;
            let vector = v.blob(emb_blob(&q.vectors[i]));
            format!(
                "SELECT p.\"key\" FROM {LITE_POINT_RT} rt JOIN place p ON p.rowid = rt.id \
                 WHERE {rt} AND {refine} \
                 ORDER BY vec_cos_dist(p.emb, {vector}), p.rowid LIMIT {K}"
            )
        }
        // The FTS5 match is the driver here and the radius is an exact refine
        // with no R*Tree in front of it: a term narrows this corpus far
        // harder than a radius does, and two virtual tables in one statement
        // would make SQLite materialise whichever it did not drive from.
        "hybrid_10" => {
            let query = v.text(&lite_fts_query(&[&q.terms[i]]));
            let centre = q.radius_centre(i)?;
            let lon = v.real(centre.longitude());
            let lat = v.real(centre.latitude());
            let metres = v.real(q.radius_metres(i));
            let vector = v.blob(emb_blob(&q.vectors[i]));
            format!(
                "SELECT p.\"key\" FROM {LITE_FTS} JOIN place p ON p.rowid = {LITE_FTS}.rowid \
                 WHERE {LITE_FTS} MATCH {query} \
                 AND geo_dist_m(p.lon, p.lat, {lon}, {lat}) <= {metres} \
                 ORDER BY vec_cos_dist(p.emb, {vector}), p.rowid LIMIT {K}"
            )
        }
        "hybrid_blend_10" => {
            let query = v.text(&lite_fts_query(&[&q.terms[i]]));
            let centre = q.radius_centre(i)?;
            let lon = v.real(centre.longitude());
            let lat = v.real(centre.latitude());
            let metres = v.real(q.radius_metres(i));
            let vector = v.blob(emb_blob(&q.vectors[i]));
            format!(
                "SELECT p.\"key\" FROM {LITE_FTS} JOIN place p ON p.rowid = {LITE_FTS}.rowid \
                 WHERE {LITE_FTS} MATCH {query} \
                 AND geo_dist_m(p.lon, p.lat, {lon}, {lat}) <= {metres} \
                 ORDER BY 0.5 * (-bm25({LITE_FTS})) \
                 + 0.5 * (1.0 - vec_cos_dist(p.emb, {vector})) DESC, p.rowid LIMIT {K}"
            )
        }
        "vec_ann_10_kind:exact" => {
            let kind = v.text(&kind_value);
            let vector = v.blob(emb_blob(&q.vectors[i]));
            format!(
                "SELECT \"key\" FROM place WHERE kind = {kind} \
                 ORDER BY vec_cos_dist(emb, {vector}), rowid LIMIT {K}"
            )
        }

        // ── the graph cases, as a recursive CTE ──────────────────────────
        //
        // `WITH RECURSIVE` is the only bounded traversal SQLite can write:
        // there is no traversal atomic and no pattern syntax. The per-hop
        // predicates are inside the recursive term, never a post-filter over
        // a completed walk, because GRAPH_CONTRACT 4.3 says a failing edge is
        // never followed and a failing node is never expanded. `depth < 2`
        // bounds the walk at two hops, `UNION` keeps the queue acyclic, and
        // `node <> <seed>` is the rule that a seed is never its own answer.
        "graph_2hop" | "graph_2hop_weight" | "graph_2hop_born" => {
            let seed = v.text(q.seed_key(i)?);
            let edge = if name == "graph_2hop_weight" {
                " AND r.weight > 0.5"
            } else {
                ""
            };
            let node = if name == "graph_2hop_born" {
                let lower = v.int(born_lower);
                let upper = v.int(born_upper);
                format!(
                    " JOIN place b ON b.\"key\" = r.destination \
                     AND b.born BETWEEN {lower} AND {upper}"
                )
            } else {
                String::new()
            };
            let tail = v.text(q.seed_key(i)?);
            format!(
                "WITH RECURSIVE walk(node, depth) AS ( \
                   SELECT {seed}, 0 \
                   UNION \
                   SELECT r.destination, walk.depth + 1 FROM walk \
                     JOIN {RELATED} r ON r.source = walk.node{edge}{node} \
                    WHERE walk.depth < 2 \
                 ) SELECT DISTINCT node FROM walk WHERE depth > 0 AND node <> {tail}"
            )
        }
        "graph_1hop_weight_top10" => {
            let seed = v.text(q.seed_key(i)?);
            format!(
                "SELECT r.source FROM {RELATED} r WHERE r.destination = {seed} \
                 ORDER BY r.weight DESC LIMIT {K}"
            )
        }
        other => return Err(format!("battle50k: no SQLite spelling for case `{other}`").into()),
    };
    Ok(LitePlan::Rows { sql, params: bound.params })
}

pub struct SqliteCtx {
    conn: Connection,
}

/// One statement, one instance: every row's first column pushed as the answer.
fn lite_run(ctx: &SqliteCtx, sql: &str, params: &[LiteValue]) -> R<Answer> {
    let mut statement = ctx.conn.prepare_cached(sql)?;
    let mut answer = Answer::default();
    let mut rows = statement.query(params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        answer.push(row.get::<_, String>(0)?);
    }
    Ok(answer)
}

/// A projection-only case: the key comes from column 0 and every other column
/// is READ, because the per-row function cost is what the case measures.
fn lite_project_run(ctx: &SqliteCtx, sql: &str, params: &[LiteValue]) -> R<Answer> {
    let mut statement = ctx.conn.prepare_cached(sql)?;
    let columns = statement.column_count();
    let mut answer = Answer::default();
    let mut sink = 0u64;
    let mut rows = statement.query(params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        for at in 1..columns {
            sink = sink.wrapping_add(match row.get_ref(at)? {
                ValueRef::Text(text) => text.len() as u64,
                ValueRef::Integer(value) => value as u64,
                _ => 0,
            });
        }
        answer.push(row.get::<_, String>(0)?);
    }
    std::hint::black_box(sink);
    Ok(answer)
}

/// One ranked pass that also reports the largest distance it returned, which
/// is what tells the k-nearest ladder whether its box was wide enough.
fn lite_ranked(ctx: &SqliteCtx, sql: &str, params: &[LiteValue]) -> R<(Answer, f64)> {
    let mut statement = ctx.conn.prepare_cached(sql)?;
    let mut answer = Answer::default();
    let mut worst = 0.0f64;
    let mut rows = statement.query(params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        answer.push(row.get::<_, String>(0)?);
        worst = worst.max(row.get::<_, f64>(1)?);
    }
    Ok((answer, worst))
}

/// k-nearest over an R*Tree that has no nearest-neighbour cursor: ask inside
/// a box of radius r, and accept the answer only when it holds K rows whose
/// worst distance is at most r. Every row outside that box is further than r
/// — `radius_candidate_bounds` is a cover of the r-ball — so no row outside
/// can displace one inside, and the answer is the exact k-nearest.
fn lite_nearest(
    ctx: &SqliteCtx,
    ring_sql: &str,
    scan_sql: &str,
    centre: [f64; 2],
    kind: Option<&str>,
) -> R<Answer> {
    let point = Point::new(centre[0], centre[1])?;
    for metres in KNN_RING_METRES {
        let bounds = radius_candidate_bounds(point, metres)
            .map_err(|e| format!("knn candidate bounds: {e}"))?;
        let mut params = vec![LiteValue::Real(centre[0]), LiteValue::Real(centre[1])];
        params.extend(rt_overlap(
            bounds.west(),
            bounds.east(),
            bounds.south(),
            bounds.north(),
        ));
        if let Some(kind) = kind {
            params.push(LiteValue::Text(kind.to_owned()));
        }
        let (answer, worst) = lite_ranked(ctx, ring_sql, &params)?;
        if answer.rows >= K as u64 && worst <= metres {
            return Ok(answer);
        }
    }
    let mut params = vec![LiteValue::Real(centre[0]), LiteValue::Real(centre[1])];
    if let Some(kind) = kind {
        params.push(LiteValue::Text(kind.to_owned()));
    }
    Ok(lite_ranked(ctx, scan_sql, &params)?.0)
}

fn lite_answer(ctx: &SqliteCtx, corpus: &Corpus, q: &Queries, name: &str, i: usize) -> R<Answer> {
    match lite_case(q, &corpus.kinds, name, i)? {
        LitePlan::Rows { sql, params } => lite_run(ctx, &sql, &params),
        LitePlan::Project { sql, params } => lite_project_run(ctx, &sql, &params),
        LitePlan::Nearest {
            ring_sql,
            scan_sql,
            centre,
            kind,
        } => lite_nearest(ctx, &ring_sql, &scan_sql, centre, kind.as_deref()),
        LitePlan::NotExpressible(reason) => {
            Err(format!("battle50k: `{name}` is not expressible in SQLite: {reason}").into())
        }
    }
}

/// The statement instance 0 of a case runs, for the report's `sql` field and
/// for `--dump`; `Err(reason)` for a case this arm cannot express, which the
/// report prints as `n/a: <reason>` rather than dropping.
fn lite_statement(q: &Queries, kinds: &[String], name: &str) -> R<Result<String, String>> {
    Ok(match lite_case(q, kinds, name, 0)? {
        LitePlan::Rows { sql, .. } | LitePlan::Project { sql, .. } => Ok(sql),
        LitePlan::Nearest {
            ring_sql, scan_sql, ..
        } => Ok(format!(
            "-- widening R*Tree rings ({} m … {} m), each:\n{ring_sql}\n\
             -- whole-corpus fallback when the ladder is exhausted:\n{scan_sql}",
            KNN_RING_METRES[0],
            KNN_RING_METRES[KNN_RING_METRES.len() - 1]
        )),
        LitePlan::NotExpressible(reason) => Err(reason),
    })
}

/// Open the file and register everything a case needs, whether the arm is
/// loading it or reusing it.
fn lite_connect(path: &Path) -> R<Connection> {
    let conn = Connection::open(path)?;
    // `journal_mode` answers with a row, so it cannot go through
    // `execute_batch`. DELETE plus FULL is the rollback-journal durability
    // that matches Postgres's `synchronous_commit = on` and E4's
    // `SyncMode::Normal`: every commit reaches the operating system before the
    // next statement runs. SQLite leaves `PRAGMA fullfsync` OFF by default, so
    // on macOS this is `fsync`, not `fcntl(F_FULLFSYNC)`; the E4 arm issues
    // `sync_data`, the same class of primitive.
    let mode: String = conn.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("delete") {
        return Err(format!("sqlite refused journal_mode=DELETE and stayed in {mode}").into());
    }
    conn.execute_batch(&format!(
        "PRAGMA synchronous = FULL;\n\
         PRAGMA page_size = {LITE_PAGE_BYTES};\n\
         PRAGMA cache_size = -{};\n\
         PRAGMA case_sensitive_like = ON;",
        CACHE_BYTES / 1024
    ))?;
    lite_register(&conn)?;
    Ok(conn)
}

/// The database file and anything SQLite keeps beside it.
fn lite_files(path: &Path) -> [PathBuf; 4] {
    [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
        PathBuf::from(format!("{}-journal", path.display())),
    ]
}

/// The whole database on disk: one file holding the table, the primary key,
/// the FTS5 index, both R*Trees, every ordinary index and the `related` edges.
/// Where the SQLite write case builds its scratch file: beside the corpus
/// `.db`, never in it, and removed when the case ends. `lite_disk_bytes`
/// names four exact paths, so a sibling is not counted as corpus.
pub fn lite_bulk_path(db: &Path) -> PathBuf {
    let mut name = db.file_name().unwrap_or_default().to_os_string();
    name.push("-bulk.db");
    db.with_file_name(name)
}

/// The SQLite write case. SQLite has no vector type and no vector index, so
/// the embedding is a BLOB of `DIM` little-endian f32 lanes -- the same bytes
/// `emb_blob` gives the corpus loader -- and NEITHER write case has an index
/// on it. The pair therefore measures the same statement twice in this arm,
/// which is the honest answer rather than a fabricated difference; see the
/// arm's deviation.
fn lite_bulk_write(path: &Path, corpus: &Corpus, i: usize) -> R<(f64, Answer)> {
    let batch: Vec<(String, Vec<u8>)> = (0..VEC_BULK_ROWS)
        .map(|at| bulk_row(corpus, i, at).map(|(key, emb)| (key, emb_blob(emb))))
        .collect::<R<Vec<_>>>()?;
    lite_remove(path);
    let conn = lite_connect(path)?;
    conn.execute_batch(&format!(
        "CREATE TABLE {VEC_BULK_OBJECT} (\"key\" text primary key, emb blob)"
    ))?;
    let started = Instant::now();
    conn.execute_batch("BEGIN")?;
    {
        let mut statement =
            conn.prepare(&format!("INSERT INTO {VEC_BULK_OBJECT} (\"key\", emb) VALUES (?, ?)"))?;
        for (key, blob) in &batch {
            statement.execute(rusqlite::params![key, blob])?;
        }
    }
    conn.execute_batch("COMMIT")?;
    let micros = started.elapsed().as_secs_f64() * 1e6;
    let mut answer = Answer::default();
    for (key, _) in &batch {
        answer.push(key.clone());
    }
    drop(conn);
    lite_remove(path);
    Ok((micros, answer))
}

fn lite_disk_bytes(path: &Path) -> u64 {
    lite_files(path)
        .iter()
        .filter_map(|candidate| fs::metadata(candidate).ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

/// Delete a database a previous pass left, so a fresh load is a fresh load.
fn lite_remove(path: &Path) {
    for candidate in lite_files(path) {
        let _ = fs::remove_file(candidate);
    }
}

/// Stream every row in with one transaction per 256, then build the FTS5
/// index, the two R*Trees and the five ordinary indexes LATE, each its own
/// timed stage — the same shape the other three arms load in.
fn load_sqlite(path: &Path, corpus: &Corpus) -> R<(SqliteCtx, Vec<Value>)> {
    lite_remove(path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut stages = Vec::new();

    let at = Instant::now();
    let conn = lite_connect(path)?;
    conn.execute_batch(
        "CREATE TABLE place (\
           \"key\" TEXT PRIMARY KEY, \
           name TEXT NOT NULL, \
           descr TEXT NOT NULL, \
           kind TEXT NOT NULL, \
           born INTEGER NOT NULL, \
           lon REAL NOT NULL, \
           lat REAL NOT NULL, \
           emb BLOB NOT NULL, \
           plot TEXT NOT NULL, \
           born_ts INTEGER NOT NULL \
         );",
    )?;
    stages.push(stage("open", at.elapsed().as_secs_f64()));

    eprintln!(
        "[sqlite] inserting {} rows, commit every {BATCH} …",
        corpus.rows.len()
    );
    let at = Instant::now();
    // The rowid IS the file ordinal, which is E4's entity sequence, which is
    // what the FTS5 rows and both R*Tree rows are keyed by. Nothing may be
    // skipped or reordered, or the three keyspaces would name different rows.
    let mut first = 0usize;
    while first < corpus.rows.len() {
        let last = (first + BATCH).min(corpus.rows.len());
        conn.execute_batch("BEGIN")?;
        {
            let mut statement = conn.prepare_cached(
                "INSERT INTO place (rowid, \"key\", name, descr, kind, born, lon, lat, emb, \
                 plot, born_ts) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            )?;
            for ordinal in first..last {
                let row = &corpus.rows[ordinal];
                let values = [
                    LiteValue::Integer(ordinal as i64 + 1),
                    LiteValue::Text(row.key.clone()),
                    LiteValue::Text(row.name.clone()),
                    LiteValue::Text(row.desc.clone()),
                    LiteValue::Text(row.kind.clone()),
                    LiteValue::Integer(row.born),
                    LiteValue::Real(row.lon),
                    LiteValue::Real(row.lat),
                    LiteValue::Blob(emb_blob(&row.emb)),
                    LiteValue::Text(row.plot_json.clone()),
                    LiteValue::Integer(born_ts_micros(row.born)?),
                ];
                statement.execute(params_from_iter(values.iter()))?;
            }
        }
        conn.execute_batch("COMMIT")?;
        first = last;
    }
    stages.push(stage("load", at.elapsed().as_secs_f64()));

    // ── the FTS5 index (`index:place_text`) ─────────────────────────────
    // One contentless column holding `name || ' ' || descr`: the SAME
    // concatenation E4 stores as its `text` field and indexes, so the two
    // arms tokenise the same string (deviation 1). Contentless, because the
    // words are already in `place` and storing them twice would put a
    // duplicate of the corpus in the disk number.
    let at = Instant::now();
    conn.execute_batch(&format!(
        "BEGIN;\
         CREATE VIRTUAL TABLE {LITE_FTS} USING fts5(body, content='', tokenize='unicode61');\
         INSERT INTO {LITE_FTS}(rowid, body) SELECT rowid, name || ' ' || descr FROM place;\
         COMMIT;"
    ))?;
    stages.push(stage(&format!("index:{IX_TEXT}"), at.elapsed().as_secs_f64()));

    // ── the point R*Tree (`index:place_loc`) ────────────────────────────
    let at = Instant::now();
    conn.execute_batch(&format!(
        "BEGIN;\
         CREATE VIRTUAL TABLE {LITE_POINT_RT} USING rtree(id, minlon, maxlon, minlat, maxlat);\
         INSERT INTO {LITE_POINT_RT}(id, minlon, maxlon, minlat, maxlat) SELECT rowid, \
           lon - (abs(lon) * {RT_EPS_REL:e} + {RT_EPS_ABS:e}), \
           lon + (abs(lon) * {RT_EPS_REL:e} + {RT_EPS_ABS:e}), \
           lat - (abs(lat) * {RT_EPS_REL:e} + {RT_EPS_ABS:e}), \
           lat + (abs(lat) * {RT_EPS_REL:e} + {RT_EPS_ABS:e}) FROM place;\
         COMMIT;"
    ))?;
    stages.push(stage(&format!("index:{IX_LOC}"), at.elapsed().as_secs_f64()));

    // ── the plot-bounding-box R*Tree (`index:place_plot`) ───────────────
    // The brief names ONE R*Tree, over the point. A geometry predicate
    // cannot take its candidates from that one: a plot can meet a query
    // polygon while its own row's point does not, so the point tree is not a
    // cover for `plot_*` and using it would drop rows. This second tree over
    // the plot's own bounding box is the SQLite spelling of the
    // `plot && <candidate>` term the Postgres arm carries (its deviation 14).
    let at = Instant::now();
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE {LITE_PLOT_RT} USING rtree(id, minlon, maxlon, minlat, maxlat);"
    ))?;
    let mut first = 0usize;
    while first < corpus.rows.len() {
        let last = (first + BATCH).min(corpus.rows.len());
        conn.execute_batch("BEGIN")?;
        {
            let mut statement = conn.prepare_cached(&format!(
                "INSERT INTO {LITE_PLOT_RT}(id, minlon, maxlon, minlat, maxlat) \
                 VALUES (?1,?2,?3,?4,?5)"
            ))?;
            for ordinal in first..last {
                let (west, east, south, north) = geom_bbox(&corpus.rows[ordinal].plot);
                let mut values = vec![LiteValue::Integer(ordinal as i64 + 1)];
                values.extend(rt_overlap(west, east, south, north));
                statement.execute(params_from_iter(values.iter()))?;
            }
        }
        conn.execute_batch("COMMIT")?;
        first = last;
    }
    stages.push(stage(&format!("index:{IX_PLOT}"), at.elapsed().as_secs_f64()));

    // ── the five ordinary indexes ───────────────────────────────────────
    for (name, ddl) in [
        (IX_BORN, format!("CREATE INDEX {IX_BORN} ON place(born)")),
        (IX_KIND, format!("CREATE INDEX {IX_KIND} ON place(kind)")),
        (
            IX_BORN_TS,
            format!("CREATE INDEX {IX_BORN_TS} ON place({BORN_TS})"),
        ),
        (IX_NAME, format!("CREATE INDEX {IX_NAME} ON place(name)")),
        (
            IX_KIND_LOWER,
            format!("CREATE INDEX {IX_KIND_LOWER} ON place(lower(kind))"),
        ),
    ] {
        let at = Instant::now();
        conn.execute_batch(&ddl)?;
        stages.push(stage(&format!("index:{name}"), at.elapsed().as_secs_f64()));
    }

    // SQLite's analog of E4's checkpoint and Postgres's ANALYZE/CHECKPOINT:
    // planner statistics over everything just built. In DELETE journal mode
    // every committed page is already on the platter, so there is nothing
    // else to force.
    //
    // THE CONNECTION IS THEN CLOSED AND REOPENED, and that is not tidiness.
    // SQLite reads `sqlite_stat1` when it PARSES a schema, and a connection
    // that CREATED the schema object by object has never parsed one, so the
    // statistics `ANALYZE` just wrote are not in force on it. Measured on
    // this corpus: `plot_dwithin_1km` is 93,472 us per instance on the
    // loading connection and 31.6 us on a connection that read the same
    // file — a factor of 2,958 — because without statistics the planner
    // drives the five plot_* cases from a whole-table scan with a GeoJSON
    // parse per row instead of from the R*Tree. Measuring that would be
    // measuring SQLite with its statistics switched off, which is not
    // SQLite. The reopen is inside the `checkpoint` stage, where the cost
    // belongs.
    let at = Instant::now();
    conn.execute_batch("ANALYZE;")?;
    drop(conn);
    let conn = lite_connect(path)?;
    stages.push(stage("checkpoint", at.elapsed().as_secs_f64()));

    Ok((SqliteCtx { conn }, stages))
}

/// Reopen a file an earlier pass built. `--reuse` is refused unless the table
/// already holds every row of the corpus: a query-only pass over a partial
/// load would be a measurement of a different corpus.
fn open_sqlite(path: &Path, expected_rows: usize) -> R<(SqliteCtx, Vec<Value>)> {
    let at = Instant::now();
    let conn = lite_connect(path)?;
    let count: i64 = conn.query_row("SELECT count(*) FROM place", [], |row| row.get(0))?;
    if count != expected_rows as i64 {
        return Err(format!(
            "--reuse: place holds {count} rows, expected {expected_rows}; load without --reuse"
        )
        .into());
    }
    let mut stages = vec![stage("open", at.elapsed().as_secs_f64()), skipped_stage("load")];
    for name in [
        IX_TEXT,
        IX_LOC,
        IX_PLOT,
        IX_BORN,
        IX_KIND,
        IX_BORN_TS,
        IX_NAME,
        IX_KIND_LOWER,
    ] {
        stages.push(skipped_stage(&format!("index:{name}")));
    }
    stages.push(skipped_stage("checkpoint"));
    eprintln!("[sqlite] reopened {count} rows; queries only");
    Ok((SqliteCtx { conn }, stages))
}

/// Write the same `related` edges into SQLite, as a timed stage.
///
/// `related(source text, destination text, weight real, since integer)` with
/// an index in each direction, exactly as the Postgres arm writes it: the
/// forward index is what the recursive CTE's hop reads and the reverse one is
/// E4's always-written reverse posting (GRAPH_CONTRACT 2.2), which
/// `graph_1hop_weight_top10` walks.
fn load_graph_sqlite(ctx: &SqliteCtx, corpus: &Corpus, edges: &[Related]) -> R<Value> {
    let at = Instant::now();
    ctx.conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS {RELATED};\
         CREATE TABLE {RELATED} (\
           source TEXT NOT NULL, \
           destination TEXT NOT NULL, \
           weight REAL NOT NULL, \
           since INTEGER NOT NULL \
         );"
    ))?;
    eprintln!("[sqlite] inserting {} `related` edges …", edges.len());
    let mut first = 0usize;
    while first < edges.len() {
        let last = (first + BATCH).min(edges.len());
        ctx.conn.execute_batch("BEGIN")?;
        {
            let mut statement = ctx.conn.prepare_cached(&format!(
                "INSERT INTO {RELATED} (source, destination, weight, since) VALUES (?1,?2,?3,?4)"
            ))?;
            for edge in &edges[first..last] {
                let values = [
                    LiteValue::Text(corpus.rows[edge.source].key.clone()),
                    LiteValue::Text(corpus.rows[edge.destination].key.clone()),
                    LiteValue::Real(edge.weight),
                    LiteValue::Integer(edge.since),
                ];
                statement.execute(params_from_iter(values.iter()))?;
            }
        }
        ctx.conn.execute_batch("COMMIT")?;
        first = last;
    }
    ctx.conn.execute_batch(&format!(
        "CREATE INDEX {RELATED}_source ON {RELATED}(source);\
         CREATE INDEX {RELATED}_destination ON {RELATED}(destination);\
         ANALYZE {RELATED};"
    ))?;
    Ok(stage("graph", at.elapsed().as_secs_f64()))
}

/// Every limitation this arm has, as data. Law 4: a limitation is NAMED,
/// never emulated behind a number that looks like the other arms'.
fn sqlite_deviations() -> Vec<Value> {
    vec![
        deviation(
            "vec_bulk_write_1k",
            "SQLITE HAS NO VECTOR TYPE AND NO VECTOR INDEX, so both write cases are the \
             SAME statement in this arm: a 32-lane little-endian f32 BLOB inserted into a \
             scratch table with no index on it, 1,000 rows in one transaction with \
             journal_mode=DELETE and synchronous=FULL. `vec_bulk_write_1k` is therefore \
             not an indexed write here and the pair's difference is zero by construction, \
             which is stated rather than hidden: the number this arm contributes is the \
             floor -- what writing a thousand embeddings costs when nothing maintains an \
             index over them. The scratch file is a sibling of the corpus `.db` and is \
             removed when the case ends, so it never enters `disk_bytes`.",
        ),
        deviation(
            "*",
            "SQLITE HAS NO GEOMETRY TYPE, NO GEODESIC AND NO VECTOR TYPE. `plot` is GeoJSON \
             TEXT, `loc` is two REAL columns and `emb` is a BLOB of 32 little-endian f32 \
             lanes. Every predicate over them is a REGISTERED SCALAR FUNCTION calling \
             sekejap-core itself: geo_dist_m is spatial_math::wgs84_distance_metres (Karney's \
             geodesic on the WGS84 ellipsoid, not the haversine the brief names — a haversine \
             would answer a DIFFERENT radius question than E4 and the row counts could not be \
             compared), and geom_within / geom_contains / geom_intersects / geom_dwithin are \
             spatial_geometry::{within, contains, intersects, dwithin_m}. The ORACLE is \
             therefore the same routine in both arms and a row-count difference would be a \
             real difference; what is measured is SQLite's cost of reaching that routine, \
             never a second implementation of the predicate.",
        ),
        deviation(
            "*",
            "A GEOMETRY PREDICATE PARSES GeoJSON PER ROW. The query geometry is a bound \
             parameter, so SQLite's constant-argument auxiliary data parses it ONCE per \
             statement; the stored `plot` is a TEXT column and is parsed once per candidate \
             row. E4 reads a decoded geometry straight out of the row, and PostGIS reads \
             WKB. This is the largest single cost in the five plot_* cases and it is a \
             property of storing geometry as text, which is the only geometry SQLite has.",
        ),
        deviation(
            "pt_radius",
            "THE CANDIDATE IS E4'S OWN COVER. Every radius case takes its R*Tree box from \
             spatial_math::radius_candidate_bounds — the identical conservative envelope E4's \
             point index walks — and then refines with geo_dist_m <= metres, which is E4's \
             `within_radius`. Same candidate set, same refine, so `pt_radius`, \
             `radius_and_born`, `bool_radius_or_radius`, `agg_count_radius_by_kind` and \
             `vec_exact_radius` compare like with like.",
        ),
        deviation(
            "*",
            "AN R*TREE COLUMN IS A 32-BIT FLOAT. Every stored box and every query box is \
             widened outward by 1.0e-6 relative plus 1.0e-9 absolute (about 0.12 m at this \
             corpus's 107 degrees east, roughly sixteen f32 ulps) so rounding can only ever \
             admit too much. The constraint is an OVERLAP test, never a containment test, for \
             the same reason. Both choices change the candidate count and never the answer: \
             every candidate is exactly refined.",
        ),
        deviation(
            "plot_within_box",
            "THE BRIEF NAMES ONE R*TREE, OVER THE POINT; THIS ARM BUILDS TWO. A plot can meet \
             a query geometry while the row's own point does not, so the point tree is not a \
             cover for the five plot_* cases and driving them from it would DROP rows. The \
             second tree, `plot_rt`, holds each plot's own bounding box and is the SQLite \
             spelling of the `plot && <candidate>::geography` term the Postgres arm carries. \
             It is a real extra index on disk and it is in the disk_bytes number.",
        ),
        deviation(
            "text_one",
            "FTS5 OVER ONE CONTENTLESS COLUMN HOLDING `name || ' ' || descr` — the same \
             concatenation E4 stores as its `text` field and indexes (deviation 1 of the E4 \
             arm), tokenised by unicode61, which case-folds and splits on non-alphanumerics \
             the way E4's analyzer does. Contentless (`content=''`), because the words are \
             already in `place`: storing them a second time would put a duplicate of the \
             corpus into disk_bytes.",
        ),
        deviation(
            "text_top10",
            "FTS5's bm25() IS NEGATIVE-BETTER, so the order is ASCENDING bm25, and its \
             parameters (k1=1.2, b=0.75 over one column) are not E4's BM25 parameters or \
             Postgres's ts_rank_cd. Compared on row count and top-ten overlap, never on order.",
        ),
        deviation(
            "knn_10",
            "SQLITE'S R*TREE HAS NO NEAREST-NEIGHBOUR CURSOR. k-nearest is a LADDER: ask for \
             the ten nearest inside a box of radius r (250 m, then 1 km, 4 km, 16 km, 64 km, \
             256 km, 1024 km) and accept the answer only when it holds ten rows whose worst \
             distance is at most r — at which point no row outside the box can displace one \
             inside, because the box is a cover of the r-ball. A ladder that is exhausted \
             falls back to a whole-corpus scan. The reported median is the whole ladder, \
             which is the honest cost of k-nearest here; E4 walks outward from the centre in \
             ONE cursor and PostGIS uses an ordered KNN-GiST walk.",
        ),
        deviation(
            "knn_10",
            "Ties are broken by rowid, which is the file ordinal, which is E4's entity \
             sequence — the same tiebreak `QueryOrder::Distance` applies. The Postgres arm \
             has no tiebreak at all (its deviation 8), so this arm agrees with E4 on ties \
             where Postgres need not.",
        ),
        deviation(
            "vec_exact_10",
            "THERE IS NO VECTOR INDEX IN SQLITE AND NONE IS LOADED. Every vector case is a \
             FULL SCAN with vec_cos_dist (1 - cosine over the two BLOBs) evaluated per row. \
             E4 answers the same question from `place_emb_exact`, and Postgres from a \
             sequential scan forced by planner knobs, so the exact-vector row of this table \
             compares an index against two scans.",
        ),
        deviation(
            "vec_ann_10",
            "NO ANN INDEX, FULL SCAN. E4 sweeps `ef` and Postgres sweeps \
             diskann.query_search_list_size; SQLite has neither knob and no approximate \
             family, so the sweep has exactly two points here: `@ann`, reported as \
             `n/a: <reason>` because there is nothing to measure, and `@scan`, the \
             whole-corpus exact answer, whose recall against this arm's own exact twin is \
             1.000 by construction. A 1.000 recall at scan cost is not a competitive \
             approximate result; it is the absence of one.",
        ),
        deviation(
            "hybrid_10",
            "THE FTS5 MATCH IS THE DRIVER AND THE RADIUS IS AN EXACT REFINE WITH NO R*TREE IN \
             FRONT OF IT. A term narrows this corpus far harder than a radius does, and two \
             virtual tables in one statement would make SQLite materialise whichever it did \
             not drive from. hybrid_blend_10 is the same shape.",
        ),
        deviation(
            "hybrid_blend_10",
            "ORDER BY 0.5 * (-bm25(place_fts)) + 0.5 * (1 - vec_cos_dist(emb, v)). The vector \
             half is the same cosine as every other arm; the text half is FTS5's bm25 on a \
             different scale from E4's BM25 and Postgres's ts_rank_cd, so the blend is \
             compared on top-ten overlap, never on order.",
        ),
        deviation(
            "fn_year_eq",
            "SQLITE HAS NO DATE TYPE AND NO `EXTRACT`. `born_ts` is INTEGER microseconds, and \
             the two date cases are written as the half-open RANGE the other three arms fold \
             their function into, with the identical bounds (`micros_of`). This arm therefore \
             shows no fold cost at all: it is handed the answer the fold produces. \
             fn_trunc_month_range is the same.",
        ),
        deviation(
            "fn_like_prefix",
            "`PRAGMA case_sensitive_like = ON` is set on every connection. SQLite's LIKE is \
             ASCII-case-INSENSITIVE by default, which would match a different row set than \
             E4's and Postgres's case-sensitive prefix range. With the pragma on, `name LIKE \
             'Ti%'` is both case-sensitive and eligible for SQLite's LIKE optimization \
             against the BINARY-collated index on `name`.",
        ),
        deviation(
            "agg_sum_born_by_kind",
            "`CAST(avg(born) AS INTEGER)` stands in for `floor(avg(born))`: the bundled build \
             defines no SQLITE_ENABLE_MATH_FUNCTIONS, so there is no floor(). Every `born` is \
             positive, so the cast's truncation toward zero IS the floor, and the line matches \
             the other three arms character for character.",
        ),
        deviation(
            "graph_2hop",
            "THERE IS NO TRAVERSAL ATOMIC HERE EITHER, SO THE PATTERN IS A RECURSIVE CTE. \
             `WITH RECURSIVE walk(node, depth)` bounded by `depth < 2`, `UNION` (not UNION \
             ALL) to keep the queue acyclic, and `node <> <seed>` for the rule that a seed is \
             never its own answer. The per-hop predicates are INSIDE the recursive term — the \
             edge weight on the join, the node's `born` as a join to `place` on the \
             destination — because GRAPH_CONTRACT 4.3 says a failing edge is never followed \
             and a failing node is never expanded; a post-filter over a finished walk would \
             return more rows. The Postgres arm writes the same bounded walk as a self-join \
             instead, because a recursive CTE there would measure the recursion machinery.",
        ),
        deviation(
            "*",
            "THE LOADING CONNECTION IS CLOSED AFTER `ANALYZE` AND THE BATTERY RUNS ON A FRESH \
             ONE. SQLite reads `sqlite_stat1` when it PARSES a schema, and a connection that \
             created the schema object by object has never parsed one, so the statistics \
             ANALYZE just wrote are not in force on it. Measured on this corpus: \
             plot_dwithin_1km is 93,472 us per instance on the loading connection and 31.6 us \
             on a connection that read the same file, a factor of 2,958, because without \
             statistics the planner drives the five plot_* cases from a whole-table scan with \
             a GeoJSON parse per row instead of from the R*Tree. The reopen is timed inside \
             the `checkpoint` stage. Neither of the other two engines needs it: Postgres's \
             ANALYZE is server-side and visible to the session that ran it, and E4 has no \
             planner statistics.",
        ),
        deviation(
            "*",
            "DURABILITY AND CACHE ARE MATCHED, PAGE SIZE IS NOT. `journal_mode = DELETE` with \
             `synchronous = FULL` is the rollback-journal equivalent of E4's SyncMode::Normal \
             and Postgres's synchronous_commit = on -- all three issue an ordinary fsync-class \
             barrier per commit, none of them the macOS drive-cache barrier `F_FULLFSYNC`, \
             which SQLite leaves off by default and E4 reaches with SyncMode::Full, and `cache_size = -8192` is the same 8 \
             MiB budget the E4 arm is given. The page is SQLite's 4096-byte default against \
             E4's own page size and Postgres's 8 KiB block; no arm was retuned for this \
             corpus.",
        ),
        deviation(
            "*",
            "disk_bytes IS THE .db FILE (plus any journal beside it): the table, the primary \
             key, the FTS5 index, BOTH R*Trees, the five ordinary indexes and — under \
             --graph — the `related` table and its two indexes, all in one file. That matches \
             the E4 arm, whose edges are in the same directory, and differs from the Postgres \
             arm's pg_total_relation_size('place'), which excludes its `related` table.",
        ),
        deviation(
            "*",
            "EVERY CASE IS ONE PREPARED STATEMENT WITH BOUND `?n` PARAMETERS, cached by text, \
             so the fifty instances of a case share one compiled program and the median is a \
             measurement of the walk rather than of SQLite's parser. `knn_10` and \
             `knn_10_kind` are the exception by construction: the ladder runs one prepared \
             statement per ring, and every ring is inside the timed pass.",
        ),
    ]
}

// ── the report ────────────────────────────────────────────────────────────

/// Takes `name`/`kind` directly rather than `&CaseSpec` so both the static
/// `BATTERY` entries and the dynamically-named approximate sweep points (no
/// `'static` name to point a `CaseSpec` at) build the same JSON shape.
fn case_json(name: &str, kind: CaseKind, result: Option<&CaseResult>, recall: Option<f64>, note: &str) -> Value {
    let k = match kind {
        CaseKind::Filter | CaseKind::Write => Value::Null,
        _ => Value::from(K as u64),
    };
    let recall = recall.map_or(Value::Null, Value::from);
    match result {
        Some(r) => json!({
            "name": name,
            "kind": kind.label(),
            "queries": INSTANCES,
            "median_us": r.median_us,
            "p90_us": r.p90_us,
            "run_median_us": r.run_median_us,
            "prepare_median_us": r.prepare_median_us,
            "prepared_median_us": r
                .prepared_median_us
                .map_or(Value::Null, Value::from),
            "prepared_bind_median_us": r
                .prepared_bind_median_us
                .map_or(Value::Null, Value::from),
            "total_rows": r.total_rows,
            "first_keys": r.first_keys,
            "k": k,
            "recall_at_k": recall,
            "note": note,
        }),
        None => json!({
            "name": name,
            "kind": kind.label(),
            "queries": INSTANCES,
            "median_us": Value::Null,
            "p90_us": Value::Null,
            "run_median_us": Value::Null,
            "prepare_median_us": Value::Null,
            "prepared_median_us": Value::Null,
            "prepared_bind_median_us": Value::Null,
            "total_rows": 0,
            "first_keys": Vec::<String>::new(),
            "k": k,
            "recall_at_k": Value::Null,
            "note": note,
        }),
    }
}

fn deviation(case: &str, text: &str) -> Value {
    json!({"case": case, "text": text})
}

/// The prose list at the top of this file, as data. Entries that belong to
/// one case name it; the rest are arm-wide and use `*`.
fn e4_deviations() -> Vec<Value> {
    vec![
        deviation(
            "vec_bulk_write_1k",
            "A WRITE CASE BUILDS ITS OWN DATABASE, BESIDE THE CORPUS AND NEVER IN IT. \
             The 50,000-row file is reused across runs and its `disk_bytes` is the sum of \
             the files under --db-dir, so a scratch collection living there would change \
             what every later --reuse measured and would be counted as corpus. Each \
             instance therefore removes and recreates a sibling directory (`<db-dir>-bulk`), \
             creates one collection of `key` Text and `emb` Vector(32), and -- for \
             `vec_bulk_write_1k` -- creates the exact and the quantized vector index on it \
             while it is still EMPTY, so both reach READY in one build step that walks \
             nothing and the thousand rows meet LIVE per-row maintenance rather than a \
             late build. That reset is untimed. What is timed is `begin_bulk`, 1,000 \
             `put`s and `end_bulk`, whose outermost close commits (OPS_CONTRACT 7): one \
             batch, one durability point. The scratch directory is removed when the case \
             ends and its size after one instance is reported in the case note. The rows \
             are the corpus's own 32 lanes under minted keys (`bulk-<instance>-<offset>`), \
             so every arm writes the same distribution in the same order and fifty \
             instances collide neither with each other nor with the corpus.",
        ),
        deviation(
            "*",
            "Text index spans ONE field: `Database::create_text_index` (src/text_indexes.rs:1845) \
             takes a single declared Kind::Text field, so `name` and `desc` are indexed through a \
             third stored field `text` holding `name` + ' ' + `desc`. Postgres indexes the \
             expression to_tsvector('simple', name || ' ' || descr) instead. E4 therefore stores \
             name, desc and their concatenation; Postgres stores name and descr only.",
        ),
        deviation(
            "*",
            "Keys are returned through an id-to-key vector: the cases ask for Projection::Ids and \
             a QueryRow carries an EntityId, not the external key, so the loader keeps the file's \
             keys in a Vec indexed by `sequence - 1` and the arm translates. The Postgres arm \
             selects its `key` column directly.",
        ),
        deviation(
            "*",
            "queries.json's `boxes` are [minlon, maxlon, minlat, maxlat], not the \
             lon/lat/lon/lat order the brief's prose assumed; both arms read the file's real order.",
        ),
        deviation(
            "*",
            "queries.json carries no `kinds` array: the eight categories are the sorted distinct \
             `kind` values of the data file, derived the same way in both arms, and an arm refuses \
             to run if there are not exactly eight.",
        ),
        deviation(
            "hybrid_blend_10",
            "E4 spells the blend as QueryOrder::Score (0.5 * Bm25 + 0.5 * (1 + VectorSimilarity)); \
             Postgres as ORDER BY 0.5 * ts_rank_cd + 0.5 * (1 - (emb <=> v)). The vector halves are \
             the same cosine; the text halves are BM25 against ts_rank_cd, so the case is compared \
             on top-ten overlap, never on order. Score never drives: E4 ranks every candidate the \
             two filters admit, as Postgres does for an arithmetic ORDER BY.",
        ),
        deviation(
            "text_top10",
            "E4 ranks by BM25, Postgres by ts_rank_cd. Different formulas, so this case is \
             compared on row count and top-ten overlap, never on order.",
        ),
        deviation(
            "knn_10",
            "QueryOrder::Distance breaks distance ties by entity id (src/query.rs:169); the \
             Postgres spelling `ORDER BY loc <-> point LIMIT 10` has no tiebreak, because adding \
             one would take the ordered KNN-GiST walk away from the planner. first_keys is \
             therefore compared as a set.",
        ),
        deviation(
            "*",
            "Geometry units follow GeometryFilter's own documentation (src/query.rs:100): \
             Intersects and DWithin spheroidal, Within and Contains planar. What differs is the \
             routine, not the model: E4 refines with `spatial_geometry`, Postgres with PostGIS's \
             predicates, and a row within a metre of a spheroidal boundary can be decided \
             differently by the two.",
        ),
        deviation(
            "plot_intersects",
            "The eleven rows the two arms used to answer differently were E4's, not PostGIS's: \
             `spatial_geometry` tested `geography` edge crossings on the flat lon/lat plane, and \
             a geodesic edge bows off that line by up to 122 metres per (degree of span) squared \
             -- enough to close a planar gap (seven rows, ST_Relate FF2FF1212 yet \
             ST_Distance(geography) = 0 m: q0/p0029206, q28/p0046799, q35/p0029158, q39/p0007895, \
             q43/p0001572, q43/p0010630, q45/p0018906) or to open a planar sliver the spheroid \
             does not cut (four rows, ST_Relate 212101212 with the boundaries 0.63-2.18 m apart: \
             q32/p0016989, q32/p0022998, q42/p0018219, q46/p0007969). Both halves of the \
             spheroidal test carried the flaw: the edge test is now a great-circle arc \
             crossing, and the vertex-in-ring ray cast is now corrected for the lens between \
             each straight lon/lat edge and its own geodesic. All eleven agree, the case's \
             total rises from 35,186 to PostGIS's 35,189, and every one of the fifty query \
             instances now returns the same KEY SET as PostGIS, not merely the same count. \
             plot_dwithin_1km is unchanged at 289 rows, also key for key.",
        ),
        deviation(
            "*",
            "`born` is Kind::Int (i64) here and `int` (int4) in Postgres. Every value in this \
             corpus is a yyyymmdd that fits in int4, so nothing is truncated; the width still \
             differs.",
        ),
        deviation(
            "*",
            "--reuse still reads the jsonl: it loads and builds nothing, but the id-to-key vector \
             and the eight categories both come from the data file. Stages that did not happen \
             report null, never zero.",
        ),
        deviation(
            "vec_ann_10",
            "ef and diskann.query_search_list_size are not the same knob — one bounds a compact \
             shortlist that is then reranked from f32 sidecars, the other a graph beam — so equal \
             numbers compare nothing. Both sides sweep instead: vec_ann_10@ef20 through \
             vec_ann_10@ef400 (EF_SWEEP), with recall computed inside this arm against its own \
             exact answer (vec_exact_10) at every point. vec_ann_10_kind sweeps the same way \
             against vec_ann_10_kind:exact. `compare` and battle50k_compare.py print the full \
             sweep and the headline: each arm's cheapest point with recall_at_k >= 0.95 and the \
             E4/PG ratio of their median_us AT THAT RECALL.",
        ),
        deviation(
            "*",
            "The key is stored twice in both arms: E4 declares a `key` Text field alongside the \
             external key `Database::put` already maps, and Postgres holds `key` in the heap \
             tuple and again in the primary-key btree.",
        ),
        deviation(
            "graph_2hop",
            "THE CORPUS HAS NO EDGES, SO `--graph` WRITES THEM. Every row is linked to its three \
             nearest OTHER rows by loc, with properties {weight: 1/(1+metres/1000), since: the \
             SOURCE row's born}, in context 0 under the edge type `related`. The same edge list \
             goes into all three arms; without --graph the four graph cases are skipped rather \
             than answered with zero rows.",
        ),
        deviation(
            "graph_2hop",
            "THE NEIGHBOUR LIST IS COMPUTED FROM THE CORPUS, NOT FROM AN INDEX. It uses \
             `wgs84_distance_metres`, the function QueryOrder::Distance ranks by, over a one- \
             degree grid. The reason is the comparison: Postgres cannot see E4's point index, so \
             an index-driven list there would be PostGIS's `<->` on geography, and two rows whose \
             distances differ in the last bits would be ordered differently by the two arms -- \
             the battery would then compare two different graphs. The E4 load CHECKS the first \
             200 rows of the list against the point index's own k-nearest answer and refuses to \
             continue if they differ, so the claim that it is the same list is measured.",
        ),
        deviation(
            "graph_2hop",
            "THE SEED IS A KEY, RESOLVED ONCE. Each instance seeds at the row nearest points[i], \
             found before any timed pass with the same geodesic distance, and all three arms are \
             handed that KEY. An arm that found its own seed would answer a different question \
             wherever two rows tie. The key-to-id lookup stays inside the timed pass in every \
             arm: E4's API arm calls `get`, the SQL arm's pattern resolves the key while it \
             compiles, and Postgres matches `related.source = <key>` in the join.",
        ),
        deviation(
            "graph_1hop_weight_top10",
            "ONE INCOMING HOP, RANKED BY THE REACHING EDGE. The rows that name the seed among \
             their three nearest; a row's fan-in is whatever the geometry gives it, so LIMIT 10 \
             binds on some instances and not on others. E4 reads `weight` out of the edge \
             posting and never opens a row for it (GRAPH_CONTRACT 4.2) -- but an INCOMING \
             posting is a reverse marker with no properties, so the authoritative primary \
             posting of the same edge is read back, one edge-keyspace point read per candidate \
             edge, charged to graph_edges. Compared on top-ten overlap, never on order: E4 \
             breaks a weight tie by entity id and the Postgres statement has no tiebreak.",
        ),
    ]
}

/// The `e4` arm's deviations, plus the ones that belong to asking the same
/// twenty-two questions in SQL rather than in Rust.
fn e4sql_deviations() -> Vec<Value> {
    let mut list = e4_deviations();
    list.push(deviation(
        "vec_bulk_write_1k",
        "THE e4-sql ARM WRITES THE SAME BATCH AS ONE `INSERT` STATEMENT PER ROW, with \
         the embedding bound as a parameter (`Param::Vector`) rather than spelled as a \
         literal, which is the statement an application issues. The scratch database, \
         the live indexes and the single-commit bulk scope are the `e4` arm's; what \
         this arm adds is the parse and compile of 1,000 statements, and it is INSIDE \
         the timed batch because that is what the application pays.",
    ));
    list.push(deviation(
        "*",
        "THE SELECT LIST IS `_id`, NOT `\"key\"`. E4's projection refuses the reserved field the \
         external key lives in (`collections::reserved`), so the only atomic that hands a key \
         back is `get_by_id`, one point-get per RETURNED row. This arm asks for the row identity \
         and translates it through the same load-time key vector the `e4` arm uses (deviation \
         2), which is what makes the two arms' medians comparable. The WHERE, the ORDER BY and \
         the LIMIT are the shared clause builders, byte for byte with the Postgres arm's own \
         except where a Tier-2 construct is named below.",
    ));
    list.push(deviation(
        "pt_bbox",
        "Postgres reaches the lon/lat rectangle through `loc && envelope::geography` plus \
         `ST_X/ST_Y BETWEEN`. Both are Tier 2 in docs/lang/QL_CONTRACT.md -- `&&` with \
         ST_MakeEnvelope is the Bbox filter of p3-geometry-io, and ST_X/ST_Y are pure I/O \
         functions -- so this arm writes the Tier-1 spelling of the same rectangle, \
         `ST_Within(loc::geometry, ST_MakeEnvelope(...))`, which compiles to \
         PointFilter::Bbox. Same rectangle, same rows.",
    ));
    list.push(deviation(
        "*",
        "THE `&&` CANDIDATE TERMS OF DEVIATION 14 ARE POSTGRES-ONLY. They narrow a scan before \
         an exact refine and change the cost, never the answer; there is no planner to hint \
         here, so `plot_within_box`, `plot_contains_pt` and `plot_vs_poly_within` write the \
         predicate alone.",
    ));
    list.push(deviation(
        "*",
        "THERE ARE NO PLANNER KNOBS. `SET LOCAL enable_indexscan = off` and \
         `enable_bitmapscan = off`, which are how the Postgres arm forces an exact vector \
         answer (deviation 6), parse here and change nothing, and the statement says so in a \
         notice. Exactness is which vector index the column has: with `ef_search` unset the \
         exact family answers, and `SET LOCAL ef_search = <ef>` selects the quantized one -- \
         which is how the approximate sweep is driven.",
    ));
    list.push(deviation(
        "hybrid_blend_10",
        "THE BLEND IS EXPRESSIBLE IN TIER-1 SQL. `ts_rank_cd(...)` is BM25 here (deviation 7), \
         so the statement writes `0.5 * bm25(text, term) + 0.5 * (1 - (emb <=> v))`. The vector \
         half lowers to `1 - (-VectorSimilarity)`, which is the cosine itself -- the same \
         quantity the `e4` arm builds as `0.5 * (1 + VectorSimilarity)`. The whole expression \
         is ONE ORDER BY key, the Score atomic (QL_CONTRACT deviation 3).",
    ));
    list.push(deviation(
        "*",
        "PARAMETERS ARE BOUND, NOT INLINED. The Postgres arm builds its statement text with the \
         values in it; this arm binds `$n`. Binding a 32-dimensional query vector copies it \
         once per statement, inside the timed pass.",
    ));
    list.push(deviation(
        "*",
        "EVERY TIMED INSTANCE PARSES AND COMPILES ITS OWN STATEMENT. Nothing is cached between \
         instances, because a cache would measure the cache. Each case's `note` carries the \
         median parse-and-compile cost of its statement in microseconds, measured on its own in \
         an untimed pass, so the difference from the `e4` arm can be attributed rather than \
         guessed at.",
    ));
    list.push(deviation(
        "graph_2hop",
        "THE GRAPH CASES ARE WRITTEN AS SQL/PGQ PATTERNS. `GRAPH_TABLE (base MATCH (a:place \
         WHERE a._key = $1)-[r:related]->{1,2}(b:place) COLUMNS (b._id AS k))`, with the \
         per-hop predicates written INLINE in the element they belong to: `[r:related WHERE \
         r.weight > 0.5]` compiles to the traversal's edge predicates and `(b:place WHERE b.born \
         BETWEEN ...)` to its node predicates (QL_CONTRACT §4.3, now Tier 1). Neither is a \
         post-filter: a WHERE written after the pattern would keep a node in the frontier that \
         GRAPH_CONTRACT 4.3 says must never be expanded, and would answer a different question \
         at two hops. `COLUMNS (r.weight AS w)` projects the reaching edge and `ORDER BY w DESC` \
         ranks by it; both read the bag the hop already decoded, not a row. EXPLAIN prints the \
         edge predicates and the node membership sets.",
    ));
    list.push(deviation(
        "graph_2hop",
        "THE PATTERN COMPILES THE SEED KEY AT PREPARE TIME. `a._key = $1` is a point lookup the \
         compiler makes while it builds the plan, so the `e4-sql` arm's parse+compile cost for \
         a graph case carries one `get` the `e4` arm pays inside its own timed pass instead. \
         The case note's parse figure is where to find it.",
    ));
    list
}

fn pg_deviations() -> Vec<Value> {
    let mut list = vec![
        deviation(
            "vec_bulk_write_1k",
            "THE POSTGRES WRITE CASE USES A SCRATCH TABLE, NOT `place`. `place` is reused \
             across runs and `pg_total_relation_size('place')` is this arm's `disk_bytes`, \
             so each instance instead drops and recreates `place_bulk(key text primary \
             key, emb vector(32))` and, for `vec_bulk_write_1k`, creates \
             `USING diskann (emb vector_cosine_ops)` on it while it is empty, so the index \
             is LIVE before the first row lands. That reset is untimed. What is timed is \
             BEGIN, 1,000 prepared INSERTs with synchronous_commit=on, COMMIT. The table is \
             dropped when the case ends and its size after one instance is in the note. \
             Postgres has ONE vector index family here where E4 has two, so the E4 arm's \
             maintenance is of an exact locator AND a quantized entry and this arm's is of \
             a diskann graph: the pair of cases still measures each engine's own \
             index-maintenance cost against its own no-index write, which is what the \
             difference is for.",
        ),
        deviation(
            "*",
            "The GIN index is over the expression to_tsvector('simple', name || ' ' || descr) and \
             every text query repeats that expression verbatim, so the planner recognises the \
             expression index. E4 indexes a stored concatenated `text` field instead.",
        ),
        deviation(
            "*",
            "Postgres has no exact vector index: `ORDER BY emb <=> v LIMIT 10` over diskann is \
             approximate, and bounding diskann.query_search_list_size does not make it exact. \
             vec_exact_10, vec_exact_radius and hybrid_10 therefore run with \
             `SET LOCAL enable_indexscan = off; SET LOCAL enable_bitmapscan = off`, which is a \
             sequential scan with an exact distance per row.",
        ),
        deviation(
            "vec_exact_radius",
            "Those two planner knobs also take the GiST index away from this case's radius \
             filter, so this is a full sequential scan and its latency is not an index \
             measurement. The E4 side keeps its point index.",
        ),
        deviation(
            "hybrid_10",
            "Same as vec_exact_radius: with both index-scan paths off, the GIN and GiST indexes \
             behind the text and radius filters are unavailable, so this is a full sequential \
             scan.",
        ),
        deviation(
            "*",
            "There is no `index:place_emb_exact` stage in this arm — the exact vector answer is a \
             scan, not an index — so this report carries six index stages where E4 carries seven.",
        ),
        deviation(
            "plot_within_box",
            "The planar predicates run on `plot::geometry`, which no index on a `geography` \
             column can serve, so each is preceded by a `plot && <candidate>::geography` \
             bounding-box term that lets the GiST index narrow the scan before the exact refine. \
             The `&&` term is a superset test for Within and for Contains alike, so it changes \
             the cost, never the answer. plot_contains_pt and plot_vs_poly_within do the same.",
        ),
        deviation(
            "text_top10",
            "ts_rank_cd, not BM25. Compared on row count and top-ten overlap, never on order.",
        ),
        deviation(
            "knn_10",
            "`ORDER BY loc <-> point LIMIT 10` has no tiebreak, so equal-distance rows come back \
             in an arbitrary order; E4 breaks the same ties by entity id.",
        ),
        deviation(
            "*",
            "`born` is int4 here and Kind::Int (i64) in E4.",
        ),
        deviation(
            "vec_ann_10",
            "diskann.query_search_list_size mirrors E4's ef by number, not by algorithm, so both \
             sides sweep it instead of comparing one shared numeral: vec_ann_10@sls50 through \
             vec_ann_10@sls800 (SLS_SWEEP), plus vec_ann_10@sls100+resc400 when this server's \
             diskann build exposes diskann.query_rescore. Recall is measured against this arm's \
             own sequential-scan exact answer at every point. The `SET LOCAL \
             diskann.query_search_list_size` for a point is issued in the same transaction as its \
             `ORDER BY emb <=> v LIMIT 10`, so it is in force for that LIMIT; pgvectorscale's beam \
             search only returns useful candidates once the search list size is at least the \
             LIMIT, which every SLS_SWEEP point (>= 50) clears by a wide margin over LIMIT 10, so \
             that interaction never actually binds in this sweep.",
        ),
        deviation(
            "vec_ann_10",
            "place_emb_ann's `CREATE INDEX ... USING diskann (emb vector_cosine_ops)` carries no \
             WITH clause — `pg_get_indexdef` and `pg_class.reloptions` both show no explicit build \
             parameters — so the index was built entirely on pgvectorscale's own defaults: \
             num_neighbors=50, (build-time) search_list_size=100, max_alpha=1.2, \
             storage_layout='memory_optimized' (the project's documented defaults; not \
             independently re-derived here, and this loop does NOT rebuild the index to confirm \
             them empirically or to try others, per the brief). A fairer build for this corpus's \
             32-dimensional cosine geometry would likely raise num_neighbors (e.g. 64-100) and the \
             build-time search_list_size (e.g. 200-300) to grow the graph's connectivity before any \
             query-time knob is touched, at the cost of a slower CREATE INDEX and a larger index on \
             disk — a change that needs its own timed stage and its own loop, not a note.",
        ),
        deviation(
            "*",
            "disk_bytes is pg_total_relation_size('place') — the table with its indexes, TOAST \
             and maps — against the E4 arm's sum of every file under --db-dir. Neither number \
             includes the server's own WAL or catalogs.",
        ),
    ];
    list.push(deviation(
        "hybrid_blend_10",
        "ORDER BY 0.5 * ts_rank_cd + 0.5 * (1 - (emb <=> v)); E4 spells the same blend as \
         QueryOrder::Score with BM25 in place of ts_rank_cd, so this case is compared on \
         top-ten overlap, never on order.",
    ));
    list.push(deviation(
        "graph_2hop",
        "THERE IS NO TRAVERSAL ATOMIC HERE, SO THE PATTERN IS A JOIN. Each two-hop case is the \
         recursive-free form a planner produces for a bounded pattern: `related` joined to \
         itself, UNIONed with the one-hop arm, over btree(source). The UNION does what E4's \
         ACYCLIC rule does for nothing (a node found at one hop is not returned again at two) \
         and `d <> $1` is the rule that a seed is never its own answer. WITH RECURSIVE is \
         deliberately not used: the depth is a constant, and a recursive CTE would measure the \
         recursion machinery rather than the two range reads. The statements are in \
         tools/battle50k_pg_cases.sql.",
    ));
    list.push(deviation(
        "graph_2hop_weight",
        "THE PER-HOP PREDICATES ARE REPEATED ON EVERY JOIN, NOT APPLIED ONCE AT THE END. \
         GRAPH_CONTRACT 4.3 says a failing edge is never followed and a failing node is never \
         expanded, so a path through a refused first hop does not exist; a post-filter over the \
         completed two-hop join would keep exactly those paths and return more rows. \
         graph_2hop_born joins `place` for the INTERMEDIATE node for the same reason.",
    ));
    list.push(deviation(
        "graph_2hop",
        "`related` IS A NEW TABLE IN e4_bench, WRITTEN BY `--graph`. \
         related(source text, destination text, weight real, since int) with btree(source) and \
         btree(destination); `place` is untouched and gains no column or index. The second \
         btree mirrors E4's reverse edge posting, which GRAPH_CONTRACT 2.2 writes always, and \
         is what graph_1hop_weight_top10's incoming hop reads. disk_bytes still reports \
         pg_total_relation_size('place') alone, so the edge table is NOT in that number on \
         either side -- E4's is, because its edges live in the same file.",
    ));
    list
}

fn git_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

// ── one arm, end to end ───────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    E4,
    /// The same E4 database and the same twenty-two cases, asked in SQL
    /// through `Database::sql` instead of as a `QueryRequest`. Same CLI as
    /// `e4`, `--reuse` included.
    E4Sql,
    Postgres,
    /// An embedded SQLite under `--db-dir` (which names the `.db` FILE, not
    /// a directory), asked the same forty-one cases in SQLite SQL. Every
    /// predicate SQLite has no type for is a registered function calling
    /// sekejap-core, so the ORACLE is the same maths in both arms.
    Sqlite,
}

impl Arm {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "e4" => Some(Self::E4),
            "e4-sql" => Some(Self::E4Sql),
            "postgres" => Some(Self::Postgres),
            "sqlite" => Some(Self::Sqlite),
            _ => None,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::E4 => "e4",
            Self::E4Sql => "e4-sql",
            Self::Postgres => "postgres",
            Self::Sqlite => "sqlite",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub arm: Arm,
    pub data: PathBuf,
    pub queries: PathBuf,
    pub out: PathBuf,
    pub db_dir: PathBuf,
    pub dsn: String,
    pub only: Option<String>,
    pub reuse: bool,
    /// Write (or rewrite) the `related` edge set before the battery runs,
    /// and select the graph cases. Without it those cases are skipped: the
    /// 50,000-row corpus has no edges of its own.
    pub graph: bool,
    /// When set, every selected case writes `<dir>/<case>/<i>.keys` in one
    /// extra untimed pass after its timed pass.
    pub dump: Option<PathBuf>,
    /// `e4-sql` only: prepare each case's statement ONCE and RE-BIND it per
    /// instance, and report that median beside the per-call one. The battery
    /// itself still runs unprepared, so the report carries BOTH numbers and
    /// the comparison shows what the parse and the compile were costing.
    pub prepared: bool,
}

impl Options {
    pub fn new(arm: Arm, data: impl Into<PathBuf>, queries: impl Into<PathBuf>, out: impl Into<PathBuf>) -> Self {
        Self {
            arm,
            data: data.into(),
            queries: queries.into(),
            out: out.into(),
            db_dir: default_db_dir(),
            dsn: default_dsn(),
            only: None,
            reuse: false,
            graph: false,
            dump: None,
            prepared: false,
        }
    }
}

/// Run one arm over the whole battery and write its report. The returned
/// `Value` is exactly what lands in `--out`.
pub fn run_arm(options: &Options) -> R<Value> {
    let corpus = load_corpus(&options.data)?;
    let mut queries = load_queries(&options.queries)?;
    // The graph cases seed at a KEY, and `queries.json` names points, so the
    // fifty seeds are resolved here -- once, outside every timed pass, the
    // same list for all three arms.
    queries.resolve_seeds(&corpus)?;
    let queries = queries;
    // The `related` edge set, computed once from the corpus and written by
    // whichever arm is loading. See `related_edges` for the deviation.
    let edges = if options.graph {
        let at = Instant::now();
        let edges = related_edges(&corpus);
        eprintln!(
            "[{}] {} `related` edges computed in {:.1} s",
            options.arm.label(),
            edges.len(),
            at.elapsed().as_secs_f64()
        );
        edges
    } else {
        Vec::new()
    };
    let selected = |name: &str| {
        if needs_graph(name) && !options.graph {
            return false;
        }
        options
            .only
            .as_deref()
            .is_none_or(|needle| name.contains(needle))
    };
    let arm = options.arm.label();
    let mut cases = Vec::new();
    let mut stages;
    let disk_bytes;
    // What the arm actually measured, as the engine itself reports it. A
    // frozen reference is only a reference while the version behind it is
    // written down, so the arm writes it rather than the reader guessing.
    let mut engine = Value::Null;

    match options.arm {
        Arm::E4 => {
            let (mut ctx, mut built) = if options.reuse {
                open_e4(&options.db_dir)?
            } else {
                load_e4(&options.db_dir, &corpus)?
            };
            if options.graph {
                built.push(load_graph_e4(&mut ctx, &corpus, &edges)?);
            } else {
                built.push(skipped_stage("graph"));
            }
            let ctx = ctx;
            stages = built;
            for spec in &BATTERY {
                if !selected(spec.name) {
                    continue;
                }
                if is_write_case(spec.name) {
                    let dir = e4_bulk_dir(&options.db_dir);
                    let result =
                        measure_write(|i| e4_bulk_write(&dir, &corpus, spec.name, i))
                            .map_err(|e| format!("case {}: {e}", spec.name))?;
                    let grown = dir_bytes(&dir);
                    let _ = fs::remove_dir_all(&dir);
                    eprintln!(
                        "[e4] {:<20} {:>12.1} us  rows={}  scratch={grown} bytes",
                        spec.name, result.median_us, result.total_rows
                    );
                    cases.push(case_json(
                        spec.name,
                        spec.kind,
                        Some(&result),
                        None,
                        &format!(
                            "{VEC_BULK_ROWS} rows/batch, one commit; scratch database {grown} bytes after one instance"
                        ),
                    ));
                    continue;
                }
                let result = measure(|i| e4_case(&ctx, &corpus, &queries, spec.name, i))
                    .map_err(|e| format!("case {}: {e}", spec.name))?;
                eprintln!(
                    "[e4] {:<20} {:>12.1} us  rows={}",
                    spec.name, result.median_us, result.total_rows
                );
                if let Some(dir) = options.dump.as_deref() {
                    dump_case(dir, spec.name, |i| e4_case(&ctx, &corpus, &queries, spec.name, i))
                        .map_err(|e| format!("case {} dump: {e}", spec.name))?;
                }
                cases.push(case_json(spec.name, spec.kind, Some(&result), None, ""));
            }
            // The approximate recall-vs-latency sweep: EF_SWEEP points for
            // each of vec_ann_10 and vec_ann_10_kind. See "APPROXIMATE
            // SWEEP" at the top of this file.
            for &base in &APPROX_BASES {
                for &ef in &EF_SWEEP {
                    let name = format!("{base}@ef{ef}");
                    if !selected(&name) {
                        continue;
                    }
                    let result = measure(|i| e4_case(&ctx, &corpus, &queries, &name, i))
                        .map_err(|e| format!("case {name}: {e}"))?;
                    let twin = exact_twin(&name)
                        .ok_or_else(|| format!("case {name}: no exact twin"))?;
                    let recall = mean_recall(
                        |i| e4_case(&ctx, &corpus, &queries, &name, i),
                        |i| e4_case(&ctx, &corpus, &queries, twin, i),
                    )
                    .map_err(|e| format!("case {name} recall: {e}"))?;
                    eprintln!(
                        "[e4] {:<20} {:>12.1} us  rows={}  recall={:.3}",
                        name, result.median_us, result.total_rows, recall
                    );
                    if let Some(dir) = options.dump.as_deref() {
                        dump_case(dir, &name, |i| e4_case(&ctx, &corpus, &queries, &name, i))
                            .map_err(|e| format!("case {name} dump: {e}"))?;
                    }
                    let note = format!("ef={ef}");
                    cases.push(case_json(&name, CaseKind::Approx, Some(&result), Some(recall), &note));
                }
            }
            drop(ctx);
            disk_bytes = dir_bytes(&options.db_dir);
        }
        Arm::E4Sql => {
            let (mut ctx, mut built) = if options.reuse {
                open_e4(&options.db_dir)?
            } else {
                load_e4(&options.db_dir, &corpus)?
            };
            if options.graph {
                built.push(load_graph_e4(&mut ctx, &corpus, &edges)?);
            } else {
                built.push(skipped_stage("graph"));
            }
            let ctx = ctx;
            stages = built;
            for spec in &BATTERY {
                if !selected(spec.name) {
                    continue;
                }
                if is_write_case(spec.name) {
                    let dir = e4_bulk_dir(&options.db_dir);
                    let result =
                        measure_write(|i| e4sql_bulk_write(&dir, &corpus, spec.name, i))
                            .map_err(|e| format!("case {}: {e}", spec.name))?;
                    let grown = dir_bytes(&dir);
                    let _ = fs::remove_dir_all(&dir);
                    eprintln!(
                        "[e4-sql] {:<20} {:>12.1} us  rows={}  scratch={grown} bytes",
                        spec.name, result.median_us, result.total_rows
                    );
                    cases.push(case_json(
                        spec.name,
                        spec.kind,
                        Some(&result),
                        None,
                        &format!(
                            "{VEC_BULK_ROWS} INSERT statements/batch, one commit; scratch database {grown} bytes after one instance"
                        ),
                    ));
                    continue;
                }
                let mut result = measure(|i| e4sql_answer(&ctx, &corpus, &queries, spec.name, i))
                    .map_err(|e| format!("case {}: {e}", spec.name))?;
                let parse_us = e4sql_parse_cost(&ctx, &corpus, &queries, spec.name)
                    .map_err(|e| format!("case {} parse cost: {e}", spec.name))?;
                let mut prepared_note = String::new();
                if options.prepared {
                    if !e4sql_prepared_agrees(&ctx, &corpus, &queries, spec.name)
                        .map_err(|e| format!("case {} prepared check: {e}", spec.name))?
                    {
                        return Err(format!(
                            "case {}: the prepared statement answered a different question than the freshly compiled one",
                            spec.name
                        )
                        .into());
                    }
                    match e4sql_prepared_cost(&ctx, &corpus, &queries, spec.name)
                        .map_err(|e| format!("case {} prepared cost: {e}", spec.name))?
                    {
                        Some((wall, bind, rebindable)) => {
                            result.prepared_median_us = Some(wall);
                            result.prepared_bind_median_us = Some(bind);
                            prepared_note = format!(
                                "; prepared once, re-bound per instance: {wall:.1} us/call, bind {bind:.1} us, rebind={}",
                                if rebindable { "yes" } else { "no (compiled again from the parsed statement)" }
                            );
                        }
                        None => {
                            prepared_note =
                                "; --prepared: this case writes its value INTO the statement, so there is no one statement to prepare"
                                    .to_owned();
                        }
                    }
                }
                eprintln!(
                    "[e4-sql] {:<20} {:>12.1} us  rows={}  parse={:.1} us{}",
                    spec.name,
                    result.median_us,
                    result.total_rows,
                    parse_us,
                    result
                        .prepared_median_us
                        .map_or(String::new(), |p| format!("  prepared={p:.1} us"))
                );
                if let Some(dir) = options.dump.as_deref() {
                    dump_case(dir, spec.name, |i| {
                        e4sql_answer(&ctx, &corpus, &queries, spec.name, i)
                    })
                    .map_err(|e| format!("case {} dump: {e}", spec.name))?;
                }
                cases.push(case_json(
                    spec.name,
                    spec.kind,
                    Some(&result),
                    None,
                    &format!("parse+compile {parse_us:.1} us/statement{prepared_note}"),
                ));
            }
            for &base in &APPROX_BASES {
                for &ef in &EF_SWEEP {
                    let name = format!("{base}@ef{ef}");
                    if !selected(&name) {
                        continue;
                    }
                    let mut result = measure(|i| e4sql_answer(&ctx, &corpus, &queries, &name, i))
                        .map_err(|e| format!("case {name}: {e}"))?;
                    let parse_us = e4sql_parse_cost(&ctx, &corpus, &queries, &name)
                        .map_err(|e| format!("case {name} parse cost: {e}"))?;
                    let mut prepared_note = String::new();
                    if options.prepared {
                        if let Some((wall, bind, rebindable)) =
                            e4sql_prepared_cost(&ctx, &corpus, &queries, &name)
                                .map_err(|e| format!("case {name} prepared cost: {e}"))?
                        {
                            result.prepared_median_us = Some(wall);
                            result.prepared_bind_median_us = Some(bind);
                            prepared_note = format!(
                                "; prepared {wall:.1} us/call, bind {bind:.1} us, rebind={}",
                                if rebindable { "yes" } else { "no" }
                            );
                        }
                    }
                    let twin = exact_twin(&name)
                        .ok_or_else(|| format!("case {name}: no exact twin"))?;
                    let recall = mean_recall(
                        |i| e4sql_answer(&ctx, &corpus, &queries, &name, i),
                        |i| e4sql_answer(&ctx, &corpus, &queries, twin, i),
                    )
                    .map_err(|e| format!("case {name} recall: {e}"))?;
                    eprintln!(
                        "[e4-sql] {:<20} {:>12.1} us  rows={}  recall={:.3}  parse={:.1} us",
                        name, result.median_us, result.total_rows, recall, parse_us
                    );
                    if let Some(dir) = options.dump.as_deref() {
                        dump_case(dir, &name, |i| {
                            e4sql_answer(&ctx, &corpus, &queries, &name, i)
                        })
                        .map_err(|e| format!("case {name} dump: {e}"))?;
                    }
                    cases.push(case_json(
                        &name,
                        CaseKind::Approx,
                        Some(&result),
                        Some(recall),
                        &format!("ef={ef}; parse+compile {parse_us:.1} us/statement{prepared_note}"),
                    ));
                }
            }
            drop(ctx);
            disk_bytes = dir_bytes(&options.db_dir);
        }
        Arm::Postgres => {
            let (mut client, mut built) = if options.reuse {
                open_pg(&options.dsn, corpus.rows.len())?
            } else {
                load_pg(&options.dsn, &corpus)?
            };
            if options.graph {
                built.push(load_graph_pg(&mut client, &corpus, &edges)?);
            } else {
                built.push(skipped_stage("graph"));
            }
            stages = built;
            for spec in &BATTERY {
                if !selected(spec.name) {
                    continue;
                }
                if is_write_case(spec.name) {
                    let result =
                        measure_write(|i| pg_bulk_write(&mut client, &corpus, spec.name, i))
                            .map_err(|e| format!("case {}: {e}", spec.name))?;
                    let grown: i64 = client
                        .query_one(
                            &format!("SELECT pg_total_relation_size('{VEC_BULK_OBJECT}')::bigint"),
                            &[],
                        )?
                        .get(0);
                    client.batch_execute(&format!("DROP TABLE IF EXISTS {VEC_BULK_OBJECT}"))?;
                    eprintln!(
                        "[postgres] {:<20} {:>12.1} us  rows={}  scratch={grown} bytes",
                        spec.name, result.median_us, result.total_rows
                    );
                    cases.push(case_json(
                        spec.name,
                        spec.kind,
                        Some(&result),
                        None,
                        &format!(
                            "{VEC_BULK_ROWS} rows in one transaction with synchronous_commit=on; scratch table {grown} bytes after one instance"
                        ),
                    ));
                    continue;
                }
                let result = measure(|i| {
                    let (setup, sql) = pg_case(&queries, &corpus.kinds, spec.name, i)?;
                    pg_answer(&mut client, &setup, &sql)
                })
                .map_err(|e| format!("case {}: {e}", spec.name))?;
                eprintln!(
                    "[postgres] {:<20} {:>12.1} us  rows={}",
                    spec.name, result.median_us, result.total_rows
                );
                if let Some(dir) = options.dump.as_deref() {
                    dump_case(dir, spec.name, |i| {
                        let (setup, sql) = pg_case(&queries, &corpus.kinds, spec.name, i)?;
                        pg_answer(&mut client, &setup, &sql)
                    })
                    .map_err(|e| format!("case {} dump: {e}", spec.name))?;
                }
                cases.push(case_json(spec.name, spec.kind, Some(&result), None, ""));
            }
            // The approximate recall-vs-latency sweep: SLS_SWEEP points for
            // each of vec_ann_10 and vec_ann_10_kind, plus one
            // diskann.query_rescore probe per base at sls=100 when the
            // server exposes that GUC. See "APPROXIMATE SWEEP" at the top
            // of this file.
            let rescore_supported = pg_has_query_rescore(&mut client);
            for &base in &APPROX_BASES {
                let mut points: Vec<(String, String)> = SLS_SWEEP
                    .iter()
                    .map(|sls| (format!("{base}@sls{sls}"), format!("diskann.query_search_list_size={sls}")))
                    .collect();
                if rescore_supported {
                    points.push((
                        format!("{base}@sls100+resc{RESCORE_PROBE}"),
                        format!(
                            "diskann.query_search_list_size=100, diskann.query_rescore={RESCORE_PROBE}"
                        ),
                    ));
                }
                for (name, note) in points {
                    if !selected(&name) {
                        continue;
                    }
                    let result = measure(|i| {
                        let (setup, sql) = pg_case(&queries, &corpus.kinds, &name, i)?;
                        pg_answer(&mut client, &setup, &sql)
                    })
                    .map_err(|e| format!("case {name}: {e}"))?;
                    let twin = exact_twin(&name)
                        .ok_or_else(|| format!("case {name}: no exact twin"))?;
                    let mut total = 0.0;
                    for i in 0..INSTANCES {
                        let (setup, sql) = pg_case(&queries, &corpus.kinds, &name, i)?;
                        let approximate = pg_answer(&mut client, &setup, &sql)?;
                        let (setup, sql) = pg_case(&queries, &corpus.kinds, twin, i)?;
                        let exact = pg_answer(&mut client, &setup, &sql)?;
                        total += overlap_at_k(&approximate.keys, &exact.keys, K);
                    }
                    let recall = total / INSTANCES as f64;
                    eprintln!(
                        "[postgres] {:<20} {:>12.1} us  rows={}  recall={:.3}",
                        name, result.median_us, result.total_rows, recall
                    );
                    if let Some(dir) = options.dump.as_deref() {
                        dump_case(dir, &name, |i| {
                            let (setup, sql) = pg_case(&queries, &corpus.kinds, &name, i)?;
                            pg_answer(&mut client, &setup, &sql)
                        })
                        .map_err(|e| format!("case {name} dump: {e}"))?;
                    }
                    cases.push(case_json(&name, CaseKind::Approx, Some(&result), Some(recall), &note));
                }
            }
            disk_bytes = pg_disk_bytes(&mut client)?;
            drop(client);
        }
        Arm::Sqlite => {
            let (ctx, mut built) = if options.reuse {
                open_sqlite(&options.db_dir, corpus.rows.len())?
            } else {
                load_sqlite(&options.db_dir, &corpus)?
            };
            let version: String =
                ctx.conn.query_row("SELECT sqlite_version()", [], |row| row.get(0))?;
            eprintln!("[sqlite] SQLite {version} (rusqlite's bundled build)");
            engine = json!({"sqlite_version": version});
            if options.graph {
                built.push(load_graph_sqlite(&ctx, &corpus, &edges)?);
            } else {
                built.push(skipped_stage("graph"));
            }
            stages = built;
            for spec in &BATTERY {
                if !selected(spec.name) {
                    continue;
                }
                if is_write_case(spec.name) {
                    let path = lite_bulk_path(&options.db_dir);
                    let result = measure_write(|i| lite_bulk_write(&path, &corpus, i))
                        .map_err(|e| format!("case {}: {e}", spec.name))?;
                    eprintln!(
                        "[sqlite] {:<20} {:>12.1} us  rows={}",
                        spec.name, result.median_us, result.total_rows
                    );
                    let mut entry = case_json(
                        spec.name,
                        spec.kind,
                        Some(&result),
                        None,
                        &format!(
                            "{VEC_BULK_ROWS} rows in one transaction, journal_mode=DELETE, synchronous=FULL; the embedding is a {DIM}-lane f32 BLOB and SQLite has NO vector index, so this arm's two write cases are the same statement"
                        ),
                    );
                    entry["sql"] = Value::from(format!(
                        "INSERT INTO {VEC_BULK_OBJECT} (\"key\", emb) VALUES (?, ?)"
                    ));
                    cases.push(entry);
                    continue;
                }
                // A case SQLite cannot express is reported as `n/a: <reason>`
                // with no timing at all — never as a zero, never dropped.
                let sql = match lite_statement(&queries, &corpus.kinds, spec.name)? {
                    Ok(sql) => sql,
                    Err(reason) => {
                        eprintln!("[sqlite] {:<20} n/a: {reason}", spec.name);
                        cases.push(case_json(
                            spec.name,
                            spec.kind,
                            None,
                            None,
                            &format!("n/a: {reason}"),
                        ));
                        continue;
                    }
                };
                let result = measure(|i| lite_answer(&ctx, &corpus, &queries, spec.name, i))
                    .map_err(|e| format!("case {}: {e}", spec.name))?;
                eprintln!(
                    "[sqlite] {:<20} {:>12.1} us  rows={}",
                    spec.name, result.median_us, result.total_rows
                );
                if let Some(dir) = options.dump.as_deref() {
                    dump_case(dir, spec.name, |i| {
                        lite_answer(&ctx, &corpus, &queries, spec.name, i)
                    })
                    .map_err(|e| format!("case {} dump: {e}", spec.name))?;
                    write_statement(dir, spec.name, &sql)?;
                }
                let mut entry = case_json(spec.name, spec.kind, Some(&result), None, "");
                entry["sql"] = Value::from(sql);
                cases.push(entry);
            }
            // The only approximate "sweep" this arm has: one refusal, because
            // there is no ANN family and no knob to sweep, and one whole-corpus
            // scan whose recall against its own exact twin is 1.000 by
            // construction. See the `vec_ann_10` deviation.
            for &base in &APPROX_BASES {
                let refused = format!("{base}@ann");
                if selected(&refused) {
                    match lite_statement(&queries, &corpus.kinds, &refused)? {
                        Ok(_) => {
                            return Err(format!(
                                "battle50k: `{refused}` must be reported as not expressible"
                            )
                            .into())
                        }
                        Err(reason) => {
                            eprintln!("[sqlite] {refused:<20} n/a: no ANN index in SQLite");
                            cases.push(case_json(
                                &refused,
                                CaseKind::Approx,
                                None,
                                None,
                                &format!("n/a: {reason}"),
                            ));
                        }
                    }
                }
                let name = format!("{base}@scan");
                if !selected(&name) {
                    continue;
                }
                let sql = lite_statement(&queries, &corpus.kinds, &name)?
                    .map_err(|reason| format!("case {name}: {reason}"))?;
                let result = measure(|i| lite_answer(&ctx, &corpus, &queries, &name, i))
                    .map_err(|e| format!("case {name}: {e}"))?;
                let twin =
                    exact_twin(&name).ok_or_else(|| format!("case {name}: no exact twin"))?;
                let recall = mean_recall(
                    |i| lite_answer(&ctx, &corpus, &queries, &name, i),
                    |i| lite_answer(&ctx, &corpus, &queries, twin, i),
                )
                .map_err(|e| format!("case {name} recall: {e}"))?;
                eprintln!(
                    "[sqlite] {:<20} {:>12.1} us  rows={}  recall={:.3}",
                    name, result.median_us, result.total_rows, recall
                );
                if let Some(dir) = options.dump.as_deref() {
                    dump_case(dir, &name, |i| lite_answer(&ctx, &corpus, &queries, &name, i))
                        .map_err(|e| format!("case {name} dump: {e}"))?;
                    write_statement(dir, &name, &sql)?;
                }
                let mut entry = case_json(
                    &name,
                    CaseKind::Approx,
                    Some(&result),
                    Some(recall),
                    "no index, full scan",
                );
                entry["sql"] = Value::from(sql);
                cases.push(entry);
            }
            drop(ctx);
            disk_bytes = lite_disk_bytes(&options.db_dir);
        }
    }

    stages.push(json!({"name": "disk_bytes", "bytes": disk_bytes}));
    let report = json!({
        "arm": arm,
        "commit": git_commit(),
        "rows": corpus.rows.len(),
        "engine": engine,
        "data": options.data.display().to_string(),
        "queries": options.queries.display().to_string(),
        "stages": stages,
        "cases": cases,
        "deviations": match options.arm {
            Arm::E4 => e4_deviations(),
            Arm::E4Sql => e4sql_deviations(),
            Arm::Postgres => pg_deviations(),
            Arm::Sqlite => sqlite_deviations(),
        },
    });
    if let Some(parent) = options.out.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(&options.out, serde_json::to_string_pretty(&report)?)?;
    eprintln!(
        "[{arm}] {disk_bytes} bytes on disk; report at {}",
        options.out.display()
    );
    Ok(report)
}

// ── agreement ─────────────────────────────────────────────────────────────

fn case_of<'a>(report: &'a Value, name: &str) -> Option<&'a Value> {
    report
        .get("cases")?
        .as_array()?
        .iter()
        .find(|case| case.get("name").and_then(Value::as_str) == Some(name))
}

fn number(case: Option<&Value>, field: &str) -> Option<f64> {
    case?.get(field)?.as_f64()
}

fn row_count(case: Option<&Value>) -> Option<u64> {
    case?.get("total_rows")?.as_u64()
}

fn first_keys(case: Option<&Value>) -> Vec<String> {
    case.and_then(|c| c.get("first_keys"))
        .and_then(Value::as_array)
        .map(|keys| {
            keys.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn micros(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), |v| format!("{v:.1}"))
}

/// One point of an approximate sweep, extracted from a report's `cases` by
/// stripping `<base>@` off the case name (`vec_ann_10@ef100` -> `ef100`).
struct SweepPoint {
    label: String,
    median_us: Option<f64>,
    p90_us: Option<f64>,
    recall: Option<f64>,
}

fn sweep_points(report: &Value, base: &str) -> Vec<SweepPoint> {
    let prefix = format!("{base}@");
    report
        .get("cases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let name = c.get("name").and_then(Value::as_str)?;
            let label = name.strip_prefix(&prefix)?;
            Some(SweepPoint {
                label: label.to_string(),
                median_us: c.get("median_us").and_then(Value::as_f64),
                p90_us: c.get("p90_us").and_then(Value::as_f64),
                recall: c.get("recall_at_k").and_then(Value::as_f64),
            })
        })
        .collect()
}

/// The cheapest (lowest `median_us`) point at `recall_at_k >= threshold`,
/// paired with `true`; if none clears the threshold, the point with the
/// HIGHEST recall instead (ties broken by lower `median_us`), paired with
/// `false` so the caller can say the threshold was never reached. `None`
/// only when the sweep produced no points with a recall at all.
fn cheapest_at_recall(points: &[SweepPoint], threshold: f64) -> Option<(&SweepPoint, bool)> {
    let mut hit: Vec<&SweepPoint> = points
        .iter()
        .filter(|p| p.recall.is_some_and(|r| r >= threshold) && p.median_us.is_some())
        .collect();
    hit.sort_by(|a, b| a.median_us.unwrap().total_cmp(&b.median_us.unwrap()));
    if let Some(best) = hit.first() {
        return Some((best, true));
    }
    let mut scored: Vec<&SweepPoint> = points.iter().filter(|p| p.recall.is_some()).collect();
    scored.sort_by(|a, b| {
        b.recall.unwrap().total_cmp(&a.recall.unwrap()).then_with(|| {
            a.median_us
                .unwrap_or(f64::INFINITY)
                .total_cmp(&b.median_us.unwrap_or(f64::INFINITY))
        })
    });
    scored.first().map(|p| (*p, false))
}

/// Print the cross-arm table and say whether every filter case agreed.
///
/// Takes TWO or THREE reports. Two is the original `e4` against `postgres`.
/// Three adds `e4-sql` -- the same E4 database asked in SQL -- and then the
/// filter-agreement precondition is agreement across ALL THREE arms, not two:
/// a parser that answered a different question than the API it compiles to
/// would be invisible in a two-arm table.
///
/// Ranked cases report top-ten overlap and approximate cases each arm's own
/// recall; neither is a pass/fail, because the arms rank by different
/// formulas and approximate by different algorithms.
pub fn compare(paths: &[&Path]) -> R<bool> {
    let read = |path: &Path| -> R<Value> {
        Ok(serde_json::from_str(&fs::read_to_string(path).map_err(|e| {
            format!("{}: {e}", path.display())
        })?)?)
    };
    let mut e4: Option<Value> = None;
    let mut pg: Option<Value> = None;
    let mut sql: Option<Value> = None;
    let mut lite: Option<Value> = None;
    for path in paths {
        let report = read(path)?;
        let arm = report
            .get("arm")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let slot = match arm.as_str() {
            "e4" => &mut e4,
            "e4-sql" => &mut sql,
            "postgres" => &mut pg,
            "sqlite" => &mut lite,
            other => {
                return Err(format!("{}: unknown arm `{other}`", path.display()).into());
            }
        };
        if slot.is_some() {
            return Err(format!("two reports name the arm `{arm}`").into());
        }
        *slot = Some(report);
    }
    let e4 = e4.ok_or("no `e4` report among the arguments")?;
    let pg = pg.ok_or("no `postgres` report among the arguments")?;

    // e4 and pg are always columns 0 and 1; the optional arms follow in a
    // fixed order, and their COLUMN INDEX is remembered rather than assumed,
    // because a table that read the sqlite column as the e4-sql one would
    // print a ratio between two arms that never met.
    let mut arms: Vec<(&str, &Value)> = vec![("e4", &e4), ("pg", &pg)];
    let mut sql_at: Option<usize> = None;
    let mut lite_at: Option<usize> = None;
    if let Some(report) = &sql {
        sql_at = Some(arms.len());
        arms.push(("e4-sql", report));
    }
    if let Some(report) = &lite {
        lite_at = Some(arms.len());
        arms.push(("sqlite", report));
    }
    let arms = arms;

    print!("{:<22} {:<7}", "case", "kind");
    for (label, _) in &arms {
        print!(" {:>12}", format!("{label}_us"));
    }
    print!(" {:>9}", "e4/pg");
    if sql_at.is_some() {
        print!(" {:>10}", "sql/e4");
    }
    if lite_at.is_some() {
        print!(" {:>10}", "e4/lite");
    }
    for (label, _) in &arms {
        print!(" {:>10}", format!("{label}_rows"));
    }
    println!("  agreement");

    let mut disagreements = 0usize;
    for spec in &BATTERY {
        let found: Vec<Option<&Value>> = arms
            .iter()
            .map(|(_, report)| case_of(report, spec.name))
            .collect();
        if found.iter().all(Option::is_none) {
            continue;
        }
        let medians: Vec<Option<f64>> = found
            .iter()
            .map(|case| number(*case, "median_us"))
            .collect();
        let rows: Vec<Option<u64>> = found.iter().map(|case| row_count(*case)).collect();
        let ratio = match (medians[0], medians[1]) {
            (Some(a), Some(b)) if b > 0.0 => format!("{:.2}", a / b),
            _ => "-".into(),
        };
        let sql_ratio = match (sql_at.and_then(|at| medians[at]), medians[0]) {
            (Some(a), Some(b)) if b > 0.0 => format!("{:.2}", a / b),
            _ => "-".into(),
        };
        let lite_ratio = match (medians[0], lite_at.and_then(|at| medians[at])) {
            (Some(a), Some(b)) if b > 0.0 => format!("{:.2}", a / b),
            _ => "-".into(),
        };
        let verdict = match spec.kind {
            // A write case agrees the way a filter case does: every arm has
            // to have written the same number of rows.
            CaseKind::Filter | CaseKind::Write => {
                let present: Vec<u64> = rows.iter().filter_map(|r| *r).collect();
                if present.len() != arms.len() || medians.iter().any(Option::is_none) {
                    "not run in every arm".to_string()
                } else if present.iter().all(|r| *r == present[0]) {
                    "AGREE".to_string()
                } else {
                    disagreements += 1;
                    format!(
                        "DISAGREE ({})",
                        present
                            .iter()
                            .map(u64::to_string)
                            .collect::<Vec<_>>()
                            .join(" vs ")
                    )
                }
            }
            CaseKind::Ranked | CaseKind::Approx => {
                if medians.iter().take(2).any(Option::is_none) {
                    "not run in both arms".to_string()
                } else {
                    let ke = first_keys(found[0]);
                    let kp = first_keys(found[1]);
                    let shared: HashSet<&str> = ke.iter().map(String::as_str).collect();
                    let hits = kp.iter().filter(|k| shared.contains(k.as_str())).count();
                    let width = ke.len().max(kp.len()).max(1);
                    let mut text = format!("e4/pg top-10 overlap {hits}/{width}");
                    for (label, at) in [("e4-sql", sql_at), ("sqlite", lite_at)] {
                        let Some(case) = at.and_then(|at| found[at]) else {
                            continue;
                        };
                        let ks = first_keys(Some(case));
                        let hits = ks.iter().filter(|k| shared.contains(k.as_str())).count();
                        text.push_str(&format!("; {label} vs e4 {hits}/{}", ke.len().max(1)));
                    }
                    if spec.kind == CaseKind::Approx {
                        text.push_str(&format!(
                            "; recall e4={} pg={}",
                            number(found[0], "recall_at_k")
                                .map_or_else(|| "-".into(), |v| format!("{v:.3}")),
                            number(found[1], "recall_at_k")
                                .map_or_else(|| "-".into(), |v| format!("{v:.3}")),
                        ));
                    }
                    text
                }
            }
        };
        print!("{:<22} {:<7}", spec.name, spec.kind.label());
        for median in &medians {
            print!(" {:>12}", micros(*median));
        }
        print!(" {ratio:>9}");
        if sql_at.is_some() {
            print!(" {sql_ratio:>10}");
        }
        if lite_at.is_some() {
            print!(" {lite_ratio:>10}");
        }
        for count in &rows {
            print!(
                " {:>10}",
                count.map_or_else(|| "-".into(), |v| v.to_string())
            );
        }
        println!("  {verdict}");
    }

    // The approximate recall-vs-latency sweep: the full table for every arm,
    // then the headline -- each arm's cheapest point with
    // recall_at_k >= HEADLINE_RECALL and the E4/PG ratio of their median_us
    // AT THAT RECALL. Equal ef / search_list_size numerals are not comparable
    // (deviation 12), so this ratio, not the row above, is the number that
    // means something for approximate vector search.
    for &base in &APPROX_BASES {
        println!("\n{base} recall-vs-latency sweep");
        println!(
            "{:<7} {:<18} {:>8} {:>12} {:>12}",
            "arm", "point", "recall", "median_us", "p90_us"
        );
        let mut per_arm: Vec<(&str, Vec<SweepPoint>)> = Vec::new();
        for (label, report) in &arms {
            let points = sweep_points(report, base);
            for p in &points {
                println!(
                    "{:<7} {:<18} {:>8} {:>12} {:>12}",
                    label,
                    p.label,
                    p.recall.map_or_else(|| "-".into(), |r| format!("{r:.3}")),
                    micros(p.median_us),
                    micros(p.p90_us),
                );
            }
            per_arm.push((label, points));
        }
        let e4_points = &per_arm[0].1;
        let pg_points = &per_arm[1].1;
        let e4_best = cheapest_at_recall(e4_points, HEADLINE_RECALL);
        let pg_best = cheapest_at_recall(pg_points, HEADLINE_RECALL);
        match (&e4_best, &pg_best) {
            (Some((e, true)), Some((p, true))) => {
                let e_us = e.median_us.unwrap_or(f64::NAN);
                let p_us = p.median_us.unwrap_or(f64::NAN);
                println!(
                    "HEADLINE {base}: recall>={HEADLINE_RECALL} -- e4 {} = {e_us:.1} us; \
                     pg {} = {p_us:.1} us; e4/pg = {:.2}",
                    e.label,
                    p.label,
                    e_us / p_us,
                );
            }
            _ => {
                let describe = |best: &Option<(&SweepPoint, bool)>, name: &str| -> String {
                    match best {
                        Some((point, true)) => format!(
                            "{name} cheapest at recall>={HEADLINE_RECALL}: {} (recall={:.3}, \
                             median_us={})",
                            point.label,
                            point.recall.unwrap_or(0.0),
                            micros(point.median_us),
                        ),
                        Some((point, false)) => format!(
                            "{name} never reached recall>={HEADLINE_RECALL}; best is {} \
                             (recall={:.3}, median_us={})",
                            point.label,
                            point.recall.unwrap_or(0.0),
                            micros(point.median_us),
                        ),
                        None => format!("{name}: no sweep points in this report"),
                    }
                };
                println!(
                    "HEADLINE {base}: no cross-arm ratio at recall>={HEADLINE_RECALL} -- {}; {}",
                    describe(&e4_best, "e4"),
                    describe(&pg_best, "pg"),
                );
            }
        }
    }

    let bytes = |report: &Value| -> Option<u64> {
        report
            .get("stages")?
            .as_array()?
            .iter()
            .find(|s| s.get("name").and_then(Value::as_str) == Some("disk_bytes"))?
            .get("bytes")?
            .as_u64()
    };
    print!("\ndisk_bytes");
    for (label, report) in &arms {
        print!(
            "  {label}={}",
            bytes(report).map_or_else(|| "-".into(), |v| v.to_string())
        );
    }
    println!();
    for (label, report) in &arms {
        if let Some(list) = report.get("deviations").and_then(Value::as_array) {
            println!("\n{label} deviations ({}):", list.len());
            for entry in list {
                println!(
                    "  [{}] {}",
                    entry.get("case").and_then(Value::as_str).unwrap_or("*"),
                    entry.get("text").and_then(Value::as_str).unwrap_or("")
                );
            }
        }
    }
    if disagreements > 0 {
        println!(
            "\n{disagreements} filter case(s) DISAGREE across {} arms: they answered different questions.",
            arms.len()
        );
    }
    Ok(disagreements == 0)
}

// ── command line ──────────────────────────────────────────────────────────

fn usage() -> String {
    "usage: battle50k <e4|e4-sql|postgres|sqlite> --data <jsonl> --queries <json> \
     --out <report.json> [--db-dir <dir|sqlite .db file>] [--dsn <dsn>] \
     [--only <case-substring>] [--reuse] [--graph] [--dump <dir>] [--prepared]\n\
     \x20      battle50k compare <a.json> <b.json> [<c.json>] [<d.json>]"
        .into()
}

fn parse(args: &[String]) -> R<Options> {
    let arm = args
        .first()
        .and_then(|a| Arm::parse(a))
        .ok_or_else(usage)?;
    let mut data: Option<PathBuf> = None;
    let mut queries: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut db_dir: Option<PathBuf> = None;
    let mut dsn: Option<String> = None;
    let mut only: Option<String> = None;
    let mut reuse = false;
    let mut graph = false;
    let mut dump: Option<PathBuf> = None;
    let mut prepared = false;
    let mut rest = args[1..].iter();
    while let Some(flag) = rest.next() {
        let mut value = || {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--data" => data = Some(PathBuf::from(value()?)),
            "--queries" => queries = Some(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--db-dir" => db_dir = Some(PathBuf::from(value()?)),
            "--dsn" => dsn = Some(value()?),
            "--only" => only = Some(value()?),
            "--reuse" => reuse = true,
            "--graph" => graph = true,
            "--dump" => dump = Some(PathBuf::from(value()?)),
            "--prepared" => prepared = true,
            other => return Err(format!("unknown flag {other}\n{}", usage()).into()),
        }
    }
    let mut options = Options::new(
        arm,
        data.ok_or_else(|| format!("--data is required\n{}", usage()))?,
        queries.ok_or_else(|| format!("--queries is required\n{}", usage()))?,
        out.ok_or_else(|| format!("--out is required\n{}", usage()))?,
    );
    if let Some(dir) = db_dir {
        options.db_dir = dir;
    }
    if let Some(url) = dsn {
        options.dsn = url;
    }
    options.only = only;
    options.reuse = reuse;
    options.graph = graph;
    options.dump = dump;
    options.prepared = prepared;
    if prepared && !matches!(arm, Arm::E4Sql) {
        return Err(format!(
            "--prepared is the `e4-sql` arm's flag: it prepares each case's statement once and re-binds it per instance, and no other arm here has a statement of its own to prepare\n{}",
            usage()
        )
        .into());
    }
    Ok(options)
}

fn main() -> R<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("compare") {
        if args.len() < 3 || args.len() > 5 {
            return Err(usage().into());
        }
        let paths: Vec<&Path> = args[1..].iter().map(Path::new).collect();
        let agreed = compare(&paths)?;
        if !agreed {
            std::process::exit(1);
        }
        return Ok(());
    }
    let options = parse(&args)?;
    run_arm(&options)?;
    Ok(())
}
