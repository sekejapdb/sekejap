//! BATTLE50K — E4 against PostGIS + pgvector/pgvectorscale on one 50,000-row
//! corpus, one arm per process, modelled on `src/bin/popsim.rs`.
//!
//!     battle50k <arm: e4|postgres> --data <jsonl> --queries <json>
//!               --out <report.json> [--db-dir <dir>] [--dsn <dsn>]
//!               [--only <case-substring>] [--reuse] [--dump <dir>]
//!     battle50k compare <a.json> <b.json>
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

use e4_prototype::{
    collections::{
        Accumulator, AggValue, AggregateFn, AggregateInput, AggregateRequest, BfsRequest,
        CandidateDriver, Cmp, CollectionId, CollectionOptions, Database, Direction,
        EdgePredicate, EdgeTypeId, EntityId, Geom, GeometryFilter, GraphContextId, GroupCmp,
        GroupKey, GroupOrder, GroupPredicate, IndexId, OwnedScalarValue,
        PointFilter, Projection, QueryBudget, QueryFilter, QueryOrder, QueryRequest, ScalarFilter,
        ScalarValue, ScoreExpr, SortDirection, TextMatch, VectorMetric,
    },
    spatial_math::{wgs84_distance_metres, Bounds, Point},
    sql::{prepare_sql, Param, SqlError},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use postgres::{types::ToSql, Client, NoTls};
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

const DEFAULT_DB_DIR: &str = "<scratch>";
const DEFAULT_DSN: &str = "postgres://127.0.0.1:5433/e4_bench";

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
}

impl CaseKind {
    fn label(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::Ranked => "ranked",
            Self::Approx => "approx",
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
pub const BATTERY: [CaseSpec; 36] = [
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

/// The weight `docs/GRAPH_CONTRACT.md`'s battery cases rank and prune by:
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
    for (n, edge) in edges.iter().enumerate() {
        ctx.db.put_edge(
            GraphContextId::BASE,
            ids[edge.source],
            related,
            ids[edge.destination],
            &json!({"weight": edge.weight, "since": edge.since}),
        )?;
        if (n + 1) % BATCH == 0 {
            ctx.db.commit()?;
        }
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
            sync: SyncMode::Full,
        },
    )?;
    let place = db.create_collection(
        "place",
        vec![
            ("key".into(), Kind::Text),
            ("name".into(), Kind::Text),
            ("desc".into(), Kind::Text),
            ("text".into(), Kind::Text),
            ("born".into(), Kind::Int),
            ("kind".into(), Kind::Text),
            ("loc".into(), Kind::Point),
            ("plot".into(), Kind::Geo),
            ("emb".into(), Kind::Vector(DIM)),
        ],
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
            sync: SyncMode::Full,
        },
    )?;
    let place = db
        .collection("place")?
        .ok_or("--reuse: no `place` collection in this database")?;
    let mut found: Vec<(String, IndexId)> = Vec::new();
    for n in 1..=32u64 {
        if let Ok(info) = db.index_info(IndexId(n)) {
            found.push((info.name.clone(), info.id));
        }
    }
    let by_name = |wanted: &str| -> R<IndexId> {
        found
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, id)| *id)
            .ok_or_else(|| format!("--reuse: no `{wanted}` index in this database").into())
    };
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
    stages.push(skipped_stage("checkpoint"));
    eprintln!("[e4] reopened {}; queries only", dir.display());
    Ok((ctx, stages))
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
fn e4sql_run(ctx: &E4Ctx, keys: &[String], sql: &str, params: &[Param]) -> R<Answer> {
    let t0 = Instant::now();
    let prepared = prepare_sql(&ctx.db, sql, params)?;
    let mut answer = Answer {
        prepare_us: t0.elapsed().as_secs_f64() * 1e6,
        ..Answer::default()
    };
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
    let mut answer = Answer {
        prepare_us: t0.elapsed().as_secs_f64() * 1e6,
        ..Answer::default()
    };
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
    e4sql_run(ctx, &corpus.keys, &sql, &params)
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
    // explicitly anyway so the load pays the same per-commit durability
    // barrier E4's page-WAL pays with `SyncMode::Full`.
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

// ── the report ────────────────────────────────────────────────────────────

/// Takes `name`/`kind` directly rather than `&CaseSpec` so both the static
/// `BATTERY` entries and the dynamically-named approximate sweep points (no
/// `'static` name to point a `CaseSpec` at) build the same JSON shape.
fn case_json(name: &str, kind: CaseKind, result: Option<&CaseResult>, recall: Option<f64>, note: &str) -> Value {
    let k = match kind {
        CaseKind::Filter => Value::Null,
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
         `ST_X/ST_Y BETWEEN`. Both are Tier 2 in docs/QL_CONTRACT.md -- `&&` with \
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
}

impl Arm {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "e4" => Some(Self::E4),
            "e4-sql" => Some(Self::E4Sql),
            "postgres" => Some(Self::Postgres),
            _ => None,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::E4 => "e4",
            Self::E4Sql => "e4-sql",
            Self::Postgres => "postgres",
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
}

impl Options {
    pub fn new(arm: Arm, data: impl Into<PathBuf>, queries: impl Into<PathBuf>, out: impl Into<PathBuf>) -> Self {
        Self {
            arm,
            data: data.into(),
            queries: queries.into(),
            out: out.into(),
            db_dir: PathBuf::from(DEFAULT_DB_DIR),
            dsn: DEFAULT_DSN.into(),
            only: None,
            reuse: false,
            graph: false,
            dump: None,
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
                let result = measure(|i| e4sql_answer(&ctx, &corpus, &queries, spec.name, i))
                    .map_err(|e| format!("case {}: {e}", spec.name))?;
                let parse_us = e4sql_parse_cost(&ctx, &corpus, &queries, spec.name)
                    .map_err(|e| format!("case {} parse cost: {e}", spec.name))?;
                eprintln!(
                    "[e4-sql] {:<20} {:>12.1} us  rows={}  parse={:.1} us",
                    spec.name, result.median_us, result.total_rows, parse_us
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
                    &format!("parse+compile {parse_us:.1} us/statement"),
                ));
            }
            for &base in &APPROX_BASES {
                for &ef in &EF_SWEEP {
                    let name = format!("{base}@ef{ef}");
                    if !selected(&name) {
                        continue;
                    }
                    let result = measure(|i| e4sql_answer(&ctx, &corpus, &queries, &name, i))
                        .map_err(|e| format!("case {name}: {e}"))?;
                    let parse_us = e4sql_parse_cost(&ctx, &corpus, &queries, &name)
                        .map_err(|e| format!("case {name} parse cost: {e}"))?;
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
                        &format!("ef={ef}; parse+compile {parse_us:.1} us/statement"),
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
    }

    stages.push(json!({"name": "disk_bytes", "bytes": disk_bytes}));
    let report = json!({
        "arm": arm,
        "commit": git_commit(),
        "rows": corpus.rows.len(),
        "data": options.data.display().to_string(),
        "queries": options.queries.display().to_string(),
        "stages": stages,
        "cases": cases,
        "deviations": match options.arm {
            Arm::E4 => e4_deviations(),
            Arm::E4Sql => e4sql_deviations(),
            Arm::Postgres => pg_deviations(),
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

    let arms: Vec<(&str, &Value)> = match &sql {
        Some(sql) => vec![("e4", &e4), ("pg", &pg), ("e4-sql", sql)],
        None => vec![("e4", &e4), ("pg", &pg)],
    };

    print!("{:<22} {:<7}", "case", "kind");
    for (label, _) in &arms {
        print!(" {:>12}", format!("{label}_us"));
    }
    print!(" {:>9}", "e4/pg");
    if sql.is_some() {
        print!(" {:>10}", "sql/e4");
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
        let sql_ratio = match (medians.get(2).copied().flatten(), medians[0]) {
            (Some(a), Some(b)) if b > 0.0 => format!("{:.2}", a / b),
            _ => "-".into(),
        };
        let verdict = match spec.kind {
            CaseKind::Filter => {
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
                    if let Some(case) = found.get(2).copied().flatten() {
                        let ks = first_keys(Some(case));
                        let hits = ks.iter().filter(|k| shared.contains(k.as_str())).count();
                        text.push_str(&format!("; e4-sql vs e4 {hits}/{}", ke.len().max(1)));
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
        if sql.is_some() {
            print!(" {sql_ratio:>10}");
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
    "usage: battle50k <e4|e4-sql|postgres> --data <jsonl> --queries <json> --out <report.json> \
     [--db-dir <dir>] [--dsn <dsn>] [--only <case-substring>] [--reuse] \
     [--graph] [--dump <dir>]\n\
     \x20      battle50k compare <a.json> <b.json> [<c.json>]"
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
    Ok(options)
}

fn main() -> R<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("compare") {
        if args.len() < 3 || args.len() > 4 {
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
