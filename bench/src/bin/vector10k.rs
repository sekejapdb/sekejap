//! VECTOR10K — one 10,000-row, 4,096-dimension corpus, seven arms, one
//! capacity-planning table.
//!
//!     vector10k [--out-dir DIR] [--work DIR] [--dsn DSN] [--ef N]
//!               [--only arm[,arm...]] [--keep]
//!
//! PURPOSE. `battle50k` asks whether two engines return the same rows on a
//! wide corpus of points, polygons and 32-dimension vectors. This asks a
//! narrower question at a shape an owner actually provisions for: ten
//! thousand people, each carrying a 4,096-dimension embedding — the size a
//! modern text embedding is — and for each candidate store, HOW MUCH DISK,
//! HOW LONG TO LOAD, HOW LONG PER QUERY, and HOW MUCH OF THE TRUE ANSWER
//! comes back.
//!
//! The raw vectors alone are 163.84 MB as f32 (10,000 × 4,096 × 4) and
//! 40.96 MB as int8. Every disk number below should be read against those
//! two, because at this dimension the embedding IS the database and
//! everything else is rounding.
//!
//! THE ARMS. Seven, all on the same corpus, the same 100 queries, the same
//! k = 10, and the same cosine metric.
//!
//!   1. `e4-atomic-exact`   an embedded `Database`, collection `person`,
//!      one `create_exact_vector_index` over `emb`, asked through the index
//!      ATOMIC `Database::query_exact_vector(.., VectorCandidates::All, ..)`.
//!      That atomic's definition is a scan: it walks every persisted locator
//!      and scores every f32 sidecar. It is exact by construction and its
//!      recall must be 1.00.
//!
//!   2. `e4-atomic-vamana`  the same database shape in its own directory,
//!      one `create_vamana_index` over `emb`, asked through
//!      `Database::query_vamana_vector(.., k, ef, ..)`: a greedy walk from
//!      the medoid over a single-layer graph of int8 codes, reranked against
//!      the f32 sidecars. Approximate; `--ef` sets the search list and the
//!      JSON carries a sweep of five.
//!
//!   3. `e4-sql-exact`      a third database, built and asked entirely in
//!      SQL: `CREATE TABLE person (...) WITH (index: none)`,
//!      `CREATE INDEX person_emb_exact ON person USING exact (emb)`,
//!      `INSERT INTO person (_key, name, age, score, emb) VALUES ($1..$5)`,
//!      and `SELECT _key FROM person ORDER BY emb <=> $1 LIMIT 10`. Same
//!      engine, same index family, one layer of compilation on top: the
//!      difference between arms 1 and 3 is what SQL costs.
//!
//!   4. `e4-sql-vamana`     a fourth database, loaded the same way, with
//!      `CREATE INDEX person_emb_vamana ON person USING vamana
//!      (emb vector_cosine_ops)`. The DDL builds the graph and the disk and
//!      build-time cells are real. THE QUERY CELL IS NOT: see deviation 1 —
//!      the SQL vector-order planner does not consider the vamana family,
//!      and the refusal it raises is reported verbatim instead of a number.
//!
//!   5. `sqlite`            an embedded SQLite 3.46 (rusqlite's bundled
//!      build), one table `person(key text primary key, name text, age int,
//!      score real, emb blob)`. SQLite HAS NO VECTOR INDEX. The embedding is
//!      16,384 bytes of little-endian f32 and the query is
//!      `SELECT key FROM person ORDER BY cosine_distance(emb, ?1) LIMIT 10`
//!      over a registered scalar function. THIS ARM IS A FULL TABLE SCAN,
//!      and that is the comparison, not a defect of the arm: it is the
//!      number a reader needs to decide whether an index is worth having at
//!      10,000 rows.
//!
//!   6. `pg-hnsw`           PostgreSQL 16 with pgvector 0.8.6, table
//!      `vec10k_hnsw("key" text primary key, name text, age int,
//!      score double precision, emb vector(4096))`, then
//!      `CREATE INDEX ... USING hnsw (emb vector_cosine_ops)`.
//!
//!   7. `pg-ivfflat`        the same table under a second name, then
//!      `CREATE INDEX ... USING ivfflat (emb vector_cosine_ops)
//!      WITH (lists = 100)`.
//!
//!      BOTH pgvector INDEX BUILDS ARE REFUSED AT THIS DIMENSION. pgvector
//!      0.8.6 caps an hnsw or an ivfflat index at 2,000 dimensions for the
//!      `vector` type (4,000 for `halfvec`), and 4,096 is over both. The
//!      table loads, the rows are all there, and the query then falls back
//!      to the sequential scan Postgres uses when no vector index exists —
//!      so arms 6 and 7 report REAL disk, REAL load and REAL query numbers
//!      with the index-build cell carrying the server's own refusal text,
//!      and a recall of 1.00 that belongs to a scan rather than to an index.
//!      The two supplementary rows below the table are the only route
//!      pgvector has to an ANN index at this width.
//!
//! SUPPLEMENTARY (not among the seven, labelled as such everywhere):
//! `pg-hnsw-sub2000` and `pg-ivfflat-sub2000` index the EXPRESSION
//! `subvector(emb, 1, 2000)::vector(2000)` and order by the same expression.
//! The index is legal because the expression is 2,000 lanes wide; the answer
//! is scored against the true 4,096-lane top-10, so their recall says what a
//! reader loses by truncating the embedding to fit the index.
//!
//! CORPUS. Deterministic from a fixed seed, so a rerun is the same corpus.
//! One xorshift64 stream, seeded `0x5EC0_FFEE_0000_1000`, draws in this
//! order: 50 CENTROIDS, each 4,096 lanes uniform on [-1, 1); then, per row,
//! a centroid index uniform over the 50, a RADIUS log-uniform on
//! [0.05, 0.60], and 4,096 jitter lanes uniform on [-radius, radius) added
//! to that centroid; then the row's `age` (18..=79) and `score` (0..1).
//! Keys are `p000000`..`p009999` in put order and names are
//! `person-<centroid>-<row>`.
//!
//! The radius is drawn per ROW rather than fixed because a fixed radius is
//! a single-scale ball, and in 4,096 dimensions every point on a
//! single-scale ball is very nearly the same distance from the centre as
//! every other — the top-10 becomes arbitrary among the cluster and recall
//! stops measuring anything. A spread of radii gives each cluster a density
//! gradient, which is the structure a real embedding corpus has and the
//! structure a proximity graph navigates.
//!
//! QUERIES. 100 vectors from a SECOND stream seeded `0x0000_0001_0D0D_0D0D`
//! over the SAME 50 centroids with the same radius law, so a query lands
//! inside a cluster rather than in empty space. Top-10 by cosine.
//!
//! RECALL. The oracle is brute force IN THIS PROCESS, in f64, over the f32
//! lanes this process generated: for each query, every row scored by
//! `1 - dot/(|a||b|)`, sorted by distance then by row ordinal, top 10 kept as
//! KEYS. `recall@10 = |returned ∩ exact| / 10`, averaged over the 100.
//! Arms 1, 3, 5, 6 and 7 are exact by construction and must score 1.00; a
//! number below 1.00 there is reported as the bug it would be, not published
//! as a result.
//!
//! DISK. The whole store per arm, never the index alone: for a sekejap arm
//! the recursive byte sum of its database directory after a `checkpoint`;
//! for SQLite the `.db` file plus `-wal`, `-shm` and `-journal`; for
//! Postgres `pg_total_relation_size` of the table, which includes the heap,
//! the TOAST relation the 16 KB embeddings live in, the primary key and any
//! vector index, measured after `VACUUM ANALYZE` and `CHECKPOINT`.
//!
//! LOAD. Wall time to insert 10,000 rows, and wall time to build the index,
//! reported SEPARATELY. Every engine here separates them: the sekejap arms
//! create the index after the last commit and drive `build_index_to_ready`,
//! SQLite and Postgres run their `CREATE INDEX` after the last transaction.
//!
//! DEVIATIONS. Every one is in the JSON's `deviations` block as well.
//!
//!  1. ARM 4 CANNOT BE ASKED ITS QUESTION IN SQL. `Compiler::vector_order`
//!     (`lang/src/compile/select.rs:400-403`) looks for a READY index in the
//!     `ExactVector` family and then in the `QuantizedVector` family, and
//!     nothing else: `IndexFamily::VamanaGraph` is not among the two. The
//!     engine underneath is willing — `prepare_approximate_vector`
//!     (`core/engine/src/query/plan.rs:495-501`) accepts a quantized OR a
//!     vamana index and `plan.rs:1673` routes the vamana one to
//!     `DriverPlan::VamanaVector` — but no SQL statement reaches it, because
//!     the compiler never names that index. So a table whose ONLY vector
//!     index is `USING vamana` refuses the order:
//!     "no vector index on `emb`: ...". The arm's row keeps its disk and its
//!     build time, which are real and were produced entirely through SQL,
//!     and carries that refusal in place of a latency and a recall. This
//!     binary does not patch `lang` to make the number appear; the brief
//!     forbids touching that layer and a benchmark that edits the thing it
//!     measures is not a measurement.
//!
//!  2. pgvector REFUSES BOTH ANN FAMILIES AT 4,096 DIMENSIONS. Stated above
//!     and carried verbatim in the table. The consequence is that arms 6
//!     and 7 measure THE SAME THING — a Postgres sequential scan — twice,
//!     and their two latencies differ only by noise. Both rows are kept
//!     because the brief asks for the row rather than the omission.
//!
//!  3. THE FOUR sekejap ARMS ARE FOUR SEPARATE DATABASES. Disk is asked per
//!     arm, and two indexes in one directory cannot be attributed to one arm
//!     each. The cost is that the 163.84 MB of f32 sidecars is paid four
//!     times on the volume; the benefit is that "disk MB" means what it says.
//!     Postgres likewise gets two tables, not one table with two indexes.
//!
//!  4. THE ATOMIC ARMS CALL THE INDEX ATOMIC; THE SQL ARMS GO THROUGH THE
//!     PAGE MACHINERY. Arms 1 and 2 call `query_exact_vector` /
//!     `query_vamana_vector` and read a `Vec<VectorHit>`, which carries an
//!     `EntityId`; the key comes back through an id-to-key vector this
//!     process holds, exactly as `battle50k` deviation 2 describes. Arms 3
//!     and 4 ask for `_key` and pay for the projection, the page assembly
//!     and the compile. Both halves of that difference are deliberate: it is
//!     what the two surfaces actually cost.
//!
//!  5. A SQL QUERY IS COMPILED EVERY TIME IT IS ASKED. The headline median
//!     for arms 3 and 4 is `prepare_sql` plus the walk, because that is what
//!     a caller who issues a statement pays. The JSON also carries
//!     `prepare_median_ms`, measured by compiling the same statement without
//!     walking it, so a reader who prepares once and rebinds can subtract it.
//!
//!  6. THE PAGE POOL IS 256 MiB FOR EVERY EMBEDDED ARM AND THE SERVER'S OWN
//!     DEFAULT FOR POSTGRES. The four sekejap arms open at
//!     `budget_bytes = 256 MiB` and the SQLite arm sets
//!     `PRAGMA cache_size = -262144`, the same 256 MiB. Postgres runs at
//!     whatever `shared_buffers` the container was started with, recorded in
//!     the JSON. None of the three is isolated from the operating system's
//!     page cache, and a 163.84 MB corpus on this machine fits in it
//!     entirely — which is the point the vamana result turns on and is
//!     stated again there.
//!
//!  7. DURABILITY IS MATCHED AT THE `fsync` CLASS, NOT BY NAME. sekejap runs
//!     `SyncMode::Normal` (`sync_data`), SQLite `journal_mode = DELETE` with
//!     `synchronous = FULL`, Postgres `synchronous_commit = on`. All three
//!     are an ordinary `fsync` per commit on this volume;
//!     `SyncMode::Full` would be `F_FULLFSYNC`, a drive-cache barrier no
//!     other arm pays, and matching the WORD while paying eight times the
//!     cost is not a matched arm (`battle50k`, `load_pg`).
//!
//!  8. THE LOAD CADENCE IS 256 ROWS PER COMMIT FOR THE EMBEDDED ARMS AND 64
//!     FOR POSTGRES. A 4,096-lane vector reaches the server as roughly 50 KB
//!     of text (`$n::text::vector`, the spelling `battle50k` uses because the
//!     `postgres` crate has no binary codec for pgvector's type), so a
//!     256-row statement would be a 13 MB query string. 64 keeps it near
//!     3 MB. The difference is named because it is a difference in what the
//!     load time measures.
//!
//!  9. THE SQLITE ARM SCORES IN A REGISTERED RUST FUNCTION, NOT IN SQL
//!     ARITHMETIC. `cosine_distance(blob, blob)` is the same `1 - cos` the
//!     other arms rank by, written in Rust against two little-endian f32
//!     BLOBs. Expressing 4,096 lanes of dot product in SQLite's own SQL
//!     would need a 4,096-way unrolled expression or a per-lane join table,
//!     and would measure SQLite's expression interpreter rather than
//!     SQLite's ability to hold and scan the data. The function is the
//!     honest form of "brute-force in the harness" the brief allows, and the
//!     scan it drives is still SQLite's.
//!
//! 10. TIES ARE BROKEN BY ROW ORDINAL IN THE ORACLE AND NOT BROKEN AT ALL IN
//!     THE ARMS. At 4,096 dimensions over continuous jitter two distances are
//!     never equal in f64, so no measured recall depends on this.
//!
//! 11. EACH ARM IS WARMED BY ONE UNTIMED PASS OVER ALL 100 QUERIES BEFORE
//!     THE TIMED PASS. The reported median is over the 100 timed wall times;
//!     `p90_ms` and `mean_ms` are beside it in the JSON.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use postgres::{types::ToSql, Client, NoTls};
use rusqlite::{functions::FunctionFlags, types::ValueRef, Connection};
use sekejap_core::{
    collections::{
        CollectionId, CollectionOptions, Database, IndexId, VectorCandidates, VectorMetric,
    },
    Kind,
};
use sekejap_lang::{prepare_sql, Param, SqlDatabase, SqlValue};
use serde_json::{json, Value};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Instant,
};

type R<T> = Result<T, Box<dyn std::error::Error>>;

// ── the shape of the question ─────────────────────────────────────────────

/// Rows in the corpus.
const ROWS: usize = 10_000;
/// Lanes per embedding. 4,096 is over pgvector's 2,000-lane index ceiling
/// and under sekejap's 16,384-lane one, which is half of what this measures.
const DIM: usize = 4_096;
/// Neighbours asked for, every arm.
const K: usize = 10;
/// Query vectors.
const QUERIES: usize = 100;
/// Cluster centres the corpus is drawn around.
const CENTROIDS: usize = 50;
/// The corpus stream's seed. Any rerun with this constant is the same corpus.
const SEED: u64 = 0x5EC0_FFEE_0000_1000;
/// The query stream's seed.
const QUERY_SEED: u64 = 0x0000_0001_0D0D_0D0D;
/// Narrowest and widest per-row jitter radius; the draw is log-uniform
/// between them.
const RADIUS_MIN: f32 = 0.05;
const RADIUS_MAX: f32 = 0.60;
/// Default vamana search list for the headline row.
const DEFAULT_EF: usize = 100;
/// The vamana search lists the JSON sweeps.
const EF_SWEEP: [usize; 5] = [10, 50, 100, 200, 400];
/// Rows per commit / per transaction, embedded arms (deviation 8).
const BATCH: usize = 256;
/// Rows per INSERT statement, Postgres (deviation 8).
const PG_BATCH: usize = 64;
/// The two page-pool budgets every sekejap arm is measured at.
///
/// The question the sweep asks is whether the pool binds differently on a
/// LINEAR scan and on a GRAPH walk: a scan reads each entry once and never
/// returns to it, so a small pool should cost it almost nothing, while a
/// graph hops and re-references its entry point and its hub nodes on every
/// query, so a pool that cannot hold that hot structure should make it fetch
/// them again and again.
///
/// 64 MiB and 256 MiB are the two the question was first asked at — 64 MiB
/// does not hold the 163.84 MB of f32 sidecars and 256 MiB does. NEITHER
/// BOUND: a first run recorded ZERO page-pool misses per query in all four
/// arms at both, because a vector query's big sidecars do not come through
/// the pooled page cache the budget bounds, and what does come through it
/// fits in 64 MiB. 8 MiB is here because of that measurement and not instead
/// of it: the vamana walk touches about ten thousand pooled pages per query,
/// some forty megabytes of them, so 8 MiB is a budget that cannot hold its
/// working set while still being far more than the scan's own fifteen
/// hundred kilobytes. Every row reports the counters it actually moved, and
/// a row whose pool did not miss says so rather than implying it was
/// starved.
const POOL_BUDGETS: [usize; 3] = [8 << 20, 64 << 20, 256 << 20];
/// The pool the stores are BUILT at, once each. See the deviation: a store
/// is a store whatever pool read it back, the load and build cells are
/// repeated across an arm's two rows and marked, and building each of the
/// four twice would have doubled a run whose vamana builds already dominate
/// it without answering a question anyone asked.
const BUILD_BUDGET: usize = 256 << 20;
/// SQLite's page cache, held at the larger of the two so its single row is
/// the well-fed one (deviation: SQLite and Postgres are not swept).
const CACHE_BYTES: usize = 256 << 20;
/// Rows per index-build step, and the floor the adaptive driver stops at.
///
/// One row per transaction is the smallest transaction a late build can be
/// driven in. If the page-WAL refuses that, the index cannot be built at this
/// width at all, and the arm fails by name rather than the loop halving a
/// number with nowhere left to go.
const BUILD_CHUNK: usize = 256;
const BUILD_CHUNK_FLOOR: usize = 1;
/// `core/engine/src/index/vector/graph.rs`: `DEGREE` (the neighbour list a
/// RobustPrune settles back to) and `BUILD_SEARCH_LIST * 2` (the nodes one
/// insert's greedy search may visit). Quoted here so the refusal message can
/// name the mechanism; they are not knobs this bench sets.
const VAMANA_DEGREE: usize = 48;
const BUILD_VISITED_NODES: usize = 200;
/// Widest page a sekejap SQL answer is assembled from.
const PAGE: usize = 8_192;
/// The lane count a pgvector ANN index can still take at this version, used
/// by the two supplementary rows.
const SUBVECTOR_LANES: usize = 2_000;

/// `--out-dir` when the flag is absent: a directory under the system temp dir.
fn default_out_dir() -> PathBuf {
    std::env::temp_dir().join("sekejap-bench50k")
}
/// `--work` when the flag is absent.
fn default_work_dir() -> PathBuf {
    default_out_dir().join("vector10k-work")
}
/// `--dsn` when the flag is absent: `SEKEJAP_BENCH_PG_DSN`, else a local
/// default server.
fn default_dsn() -> String {
    std::env::var("SEKEJAP_BENCH_PG_DSN")
        .unwrap_or_else(|_| "postgres://127.0.0.1:5432/postgres".into())
}

const ARMS: [&str; 7] = [
    "e4-atomic-exact",
    "e4-atomic-vamana",
    "e4-sql-exact",
    "e4-sql-vamana",
    "sqlite",
    "pg-hnsw",
    "pg-ivfflat",
];
const SUPPLEMENTARY: [&str; 2] = ["pg-hnsw-sub2000", "pg-ivfflat-sub2000"];

// ── the corpus ────────────────────────────────────────────────────────────

/// xorshift64, the generator `vamana_bench` uses, so two benches in this
/// crate draw the same way.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// Uniform on [0, 1).
    fn unit(&mut self) -> f32 {
        (self.next() >> 11) as f32 / (1u64 << 53) as f32
    }
    /// Uniform on [-1, 1).
    fn signed(&mut self) -> f32 {
        self.unit() * 2.0 - 1.0
    }
    fn lanes(&mut self) -> Vec<f32> {
        (0..DIM).map(|_| self.signed()).collect()
    }
    /// Log-uniform on [`RADIUS_MIN`, `RADIUS_MAX`] — deviation-free because
    /// both ends are constants of this file.
    fn radius(&mut self) -> f32 {
        RADIUS_MIN * (RADIUS_MAX / RADIUS_MIN).powf(self.unit())
    }
    /// One row or one query: a centroid plus per-lane jitter inside a radius
    /// this draw chose.
    fn around(&mut self, centre: &[f32]) -> Vec<f32> {
        let radius = self.radius();
        centre.iter().map(|l| l + self.signed() * radius).collect()
    }
}

fn centroids() -> Vec<Vec<f32>> {
    let mut rng = Rng(SEED);
    (0..CENTROIDS).map(|_| rng.lanes()).collect()
}

struct Corpus {
    keys: Vec<String>,
    names: Vec<String>,
    ages: Vec<i64>,
    scores: Vec<f64>,
    vectors: Vec<Vec<f32>>,
    cluster_of: Vec<usize>,
}

fn corpus() -> Corpus {
    let centres = centroids();
    // The centroid draw consumed the first `CENTROIDS * DIM` values of the
    // stream; the rows continue it, so nothing is re-used.
    let mut rng = Rng(SEED);
    for _ in 0..CENTROIDS * DIM {
        rng.next();
    }
    let mut out = Corpus {
        keys: Vec::with_capacity(ROWS),
        names: Vec::with_capacity(ROWS),
        ages: Vec::with_capacity(ROWS),
        scores: Vec::with_capacity(ROWS),
        vectors: Vec::with_capacity(ROWS),
        cluster_of: Vec::with_capacity(ROWS),
    };
    for row in 0..ROWS {
        let cluster = (rng.next() % CENTROIDS as u64) as usize;
        let vector = rng.around(&centres[cluster]);
        let age = 18 + (rng.next() % 62) as i64;
        let score = f64::from(rng.unit());
        out.keys.push(format!("p{row:06}"));
        out.names.push(format!("person-{cluster:02}-{row:06}"));
        out.ages.push(age);
        out.scores.push(score);
        out.vectors.push(vector);
        out.cluster_of.push(cluster);
    }
    out
}

fn query_set() -> Vec<Vec<f32>> {
    let centres = centroids();
    let mut rng = Rng(QUERY_SEED);
    (0..QUERIES)
        .map(|_| {
            let cluster = (rng.next() % CENTROIDS as u64) as usize;
            rng.around(&centres[cluster])
        })
        .collect()
}

fn norm_of(v: &[f32]) -> f64 {
    v.iter()
        .fold(0.0f64, |s, l| s + f64::from(*l) * f64::from(*l))
        .sqrt()
}

/// The oracle: exact top-`K` by cosine for every query, as KEYS. Computed
/// once and handed to every arm.
fn brute_force(corpus: &Corpus, queries: &[Vec<f32>]) -> Vec<Vec<String>> {
    let norms: Vec<f64> = corpus.vectors.iter().map(|v| norm_of(v)).collect();
    queries
        .iter()
        .map(|query| {
            let query_norm = norm_of(query);
            let mut scored: Vec<(f64, usize)> = (0..ROWS)
                .map(|row| {
                    let mut dot = 0.0f64;
                    for (x, y) in corpus.vectors[row].iter().zip(query) {
                        dot += f64::from(*x) * f64::from(*y);
                    }
                    let d = 1.0 - dot / (norms[row] * query_norm).max(1e-12);
                    (d, row)
                })
                .collect();
            scored.sort_by(|l, r| l.0.total_cmp(&r.0).then_with(|| l.1.cmp(&r.1)));
            scored
                .into_iter()
                .take(K)
                .map(|(_, row)| corpus.keys[row].clone())
                .collect()
        })
        .collect()
}

// ── measurement ───────────────────────────────────────────────────────────

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let at = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[at]
}

#[derive(Clone, Debug, Default)]
struct Measured {
    median_ms: f64,
    p90_ms: f64,
    mean_ms: f64,
    recall: f64,
    first_keys: Vec<String>,
    /// The page-pool counters the timed pass moved, per query, where the
    /// engine has them.
    pool: Option<Value>,
}

/// One untimed warm pass over all 100 queries, then one timed pass.
/// `run(i)` must return the arm's answer to query `i`, best first.
///
/// `sample` is read at the start and at the end of the TIMED pass and is the
/// engine's own `(hits, misses, evictions, sweep steps)` page-pool counters
/// where an arm has them. It is what turns "the small pool is slower" into a
/// statement about how many page fetches the pool actually had to serve;
/// arms with no such counter pass `|| None`.
fn measure(
    truth: &[Vec<String>],
    mut run: impl FnMut(usize) -> R<Vec<String>>,
    mut sample: impl FnMut() -> Option<[u64; 4]>,
) -> R<Measured> {
    for i in 0..QUERIES {
        run(i)?;
    }
    let opened = sample();
    let mut times = Vec::with_capacity(QUERIES);
    let mut hits = 0usize;
    let mut first = Vec::new();
    for i in 0..QUERIES {
        let at = Instant::now();
        let answer = run(i)?;
        times.push(at.elapsed().as_secs_f64() * 1e3);
        let mut seen: Vec<&String> = Vec::with_capacity(K);
        for key in answer.iter().take(K) {
            if !seen.contains(&key) {
                seen.push(key);
            }
        }
        hits += seen.iter().filter(|key| truth[i].contains(key)).count();
        if i == 0 {
            first = answer.into_iter().take(K).collect();
        }
    }
    let closed = sample();
    let pool = match (opened, closed) {
        (Some(a), Some(b)) => {
            let per = |at: usize| (b[at] - a[at]) as f64 / QUERIES as f64;
            Some(json!({
                "pool_hits_per_query": per(0),
                "pool_misses_per_query": per(1),
                "pool_evictions_per_query": per(2),
                "pool_sweep_steps_per_query": per(3),
            }))
        }
        _ => None,
    };
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    times.sort_by(f64::total_cmp);
    Ok(Measured {
        median_ms: percentile(&times, 0.5),
        p90_ms: percentile(&times, 0.9),
        mean_ms: mean,
        recall: hits as f64 / (QUERIES * K) as f64,
        first_keys: first,
        pool,
    })
}

/// Median wall time of a closure run once per query, with no warm pass and
/// no answer: what a SQL compile costs on its own (deviation 5).
fn median_of(mut run: impl FnMut(usize) -> R<()>) -> R<f64> {
    let mut times = Vec::with_capacity(QUERIES);
    for i in 0..QUERIES {
        let at = Instant::now();
        run(i)?;
        times.push(at.elapsed().as_secs_f64() * 1e3);
    }
    times.sort_by(f64::total_cmp);
    Ok(percentile(&times, 0.5))
}

// ── one row of the table ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct ArmRow {
    arm: String,
    /// The page pool this row was MEASURED at, where the arm has one this
    /// harness set. `None` means the engine manages its own (Postgres).
    pool_bytes: Option<usize>,
    /// What the pool cell says when `pool_bytes` is `None`.
    pool_note: String,
    /// What the arm did, in one paragraph, for the report's prose.
    did: String,
    disk_bytes: Option<u64>,
    load_seconds: Option<f64>,
    index_seconds: Option<f64>,
    /// Present when the arm answered; absent when it refused.
    measured: Option<Measured>,
    /// Why a cell is empty, in the engine's own words where there is one.
    refusal: Option<String>,
    supplementary: bool,
    extra: Value,
}

impl ArmRow {
    fn new(arm: &str) -> Self {
        Self {
            arm: arm.to_owned(),
            pool_bytes: None,
            pool_note: "—".to_owned(),
            did: String::new(),
            disk_bytes: None,
            load_seconds: None,
            index_seconds: None,
            measured: None,
            refusal: None,
            supplementary: false,
            extra: json!({}),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "arm": self.arm,
            "id": match self.pool_bytes {
                Some(bytes) => format!("{}@{}", self.arm, mib(bytes).replace(' ', "")),
                None => self.arm.clone(),
            },
            "pool_bytes": self.pool_bytes,
            "pool": match self.pool_bytes {
                Some(bytes) => mib(bytes),
                None => self.pool_note.clone(),
            },
            "supplementary": self.supplementary,
            "disk_bytes": self.disk_bytes,
            "disk_mb": self.disk_bytes.map(|b| b as f64 / 1e6),
            "load_seconds": self.load_seconds,
            "index_build_seconds": self.index_seconds,
            "query_median_ms": self.measured.as_ref().map(|m| m.median_ms),
            "query_p90_ms": self.measured.as_ref().map(|m| m.p90_ms),
            "query_mean_ms": self.measured.as_ref().map(|m| m.mean_ms),
            "recall_at_10": self.measured.as_ref().map(|m| m.recall),
            "page_pool": self.measured.as_ref().and_then(|m| m.pool.clone()),
            "first_answer": self.measured.as_ref().map(|m| m.first_keys.clone()),
            "refusal": self.refusal,
            "did": self.did,
            "extra": self.extra,
        })
    }
}

/// `10000` as `10,000`. The prose says these numbers out loud and a reader
/// should not have to count digits.
fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// `ROWS` and `DIM` as the prose says them out loud.
fn n_rows() -> String {
    thousands(ROWS)
}
fn n_dim() -> String {
    thousands(DIM)
}

/// What this row's page-pool counters actually say, in a sentence.
///
/// This is DERIVED, never predicted. A first run of this bench asserted in
/// prose that the small pool would hurt the graph and not the scan, and the
/// counters then recorded no misses at all in either — so the prose was
/// wrong and is now computed from the measurement instead of written ahead
/// of it.
fn pool_sentence(measured: &Measured, budget: usize) -> String {
    let Some(pool) = &measured.pool else {
        return String::new();
    };
    let hits = pool["pool_hits_per_query"].as_f64().unwrap_or(0.0);
    let misses = pool["pool_misses_per_query"].as_f64().unwrap_or(0.0);
    let evictions = pool["pool_evictions_per_query"].as_f64().unwrap_or(0.0);
    if misses < 0.5 {
        format!(
            "THE POOL DID NOT BIND HERE. At this {} budget the engine's own counters record \
             {hits:.0} page-pool accesses per query and {misses:.1} misses, so nothing was \
             re-fetched and this row's latency carries no read-amplification component at all. \
             It is a statement about arithmetic and page traffic that the pool was already able \
             to serve, and must not be read as one about memory pressure.",
            mib(budget)
        )
    } else {
        format!(
            "THE POOL BOUND HERE. At this {} budget the counters record {hits:.0} hits, \
             {misses:.0} misses and {evictions:.0} evictions per query, so the latency beside it \
             includes the cost of fetching again what the pool could not keep.",
            mib(budget)
        )
    }
}

impl ArmRow {
    /// Rebuild a row from the JSON an earlier run wrote. Every field the
    /// report renders comes back, `did` included — the per-arm prose was
    /// already derived from that run's own counters, so rewriting the report
    /// re-renders it rather than re-deriving it from numbers that no longer
    /// exist in this process.
    fn from_json(value: &Value) -> Option<Self> {
        let measured = value["query_median_ms"].as_f64().map(|median| Measured {
            median_ms: median,
            p90_ms: value["query_p90_ms"].as_f64().unwrap_or(f64::NAN),
            mean_ms: value["query_mean_ms"].as_f64().unwrap_or(f64::NAN),
            recall: value["recall_at_10"].as_f64().unwrap_or(f64::NAN),
            first_keys: value["first_answer"]
                .as_array()
                .map(|keys| {
                    keys.iter()
                        .filter_map(|k| k.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            pool: match &value["page_pool"] {
                Value::Null => None,
                other => Some(other.clone()),
            },
        });
        Some(Self {
            arm: value["arm"].as_str()?.to_owned(),
            pool_bytes: value["pool_bytes"].as_u64().map(|b| b as usize),
            pool_note: value["pool"].as_str().unwrap_or("—").to_owned(),
            did: value["did"].as_str().unwrap_or("").to_owned(),
            disk_bytes: value["disk_bytes"].as_u64(),
            load_seconds: value["load_seconds"].as_f64(),
            index_seconds: value["index_build_seconds"].as_f64(),
            measured,
            refusal: value["refusal"].as_str().map(str::to_owned),
            supplementary: value["supplementary"].as_bool().unwrap_or(false),
            extra: value["extra"].clone(),
        })
    }
}

fn mb(bytes: Option<u64>) -> String {
    match bytes {
        Some(b) => format!("{:.1}", b as f64 / 1e6),
        None => "—".to_owned(),
    }
}

fn secs(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{v:.2}"),
        None => "—".to_owned(),
    }
}

// ── shared helpers ────────────────────────────────────────────────────────

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

fn e4_config(budget_bytes: usize) -> Config {
    Config {
        budget_bytes,
        io: IoMode::Buffered,
        sync: SyncMode::Normal,
    }
}

/// `67108864` as `64 MiB`.
fn mib(bytes: usize) -> String {
    format!("{} MiB", bytes >> 20)
}

/// A pgvector text literal whose every lane round-trips to the same f32:
/// `{:?}` on an `f32` is Rust's shortest round-tripping form, so the bytes
/// Postgres stores are the bytes this process generated and a sequential
/// scan there must agree with the oracle here.
fn vector_literal(v: &[f32]) -> String {
    let mut out = String::with_capacity(v.len() * 10 + 2);
    out.push('[');
    for (i, lane) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{lane:?}"));
    }
    out.push(']');
    out
}

fn emb_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for lane in v {
        out.extend_from_slice(&lane.to_le_bytes());
    }
    out
}

fn deviation(subject: &str, text: &str) -> Value {
    json!({"subject": subject, "text": text})
}

fn git_commit(root: &Path) -> String {
    std::process::Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

// ── arms 1 and 2: the atomic API ──────────────────────────────────────────

/// Create the collection, stream every row in at one commit per 256, then
/// create and build ONE vector index, timed on its own.
///
/// `family` is `"exact"` or `"vamana"`; the collection and the load are
/// identical either way, so the two arms differ in exactly one call.
fn load_e4(
    dir: &Path,
    corpus: &Corpus,
    family: &str,
) -> R<(Database, CollectionId, IndexId, f64, Build)> {
    let _ = fs::remove_dir_all(dir);
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut db = Database::create(dir, e4_config(BUILD_BUDGET))?;
    let person = db.create_collection(
        "person",
        vec![
            ("name".into(), Kind::Text),
            ("age".into(), Kind::Int),
            ("score".into(), Kind::Real),
            ("emb".into(), Kind::Vector(DIM)),
        ],
        CollectionOptions::default(),
    )?;
    db.commit()?;

    eprintln!("[{family}] inserting {ROWS} rows of {DIM} lanes, commit every {BATCH} …");
    let at = Instant::now();
    for row in 0..ROWS {
        let id = db.put(
            person,
            &corpus.keys[row],
            &json!({
                "name": corpus.names[row],
                "age": corpus.ages[row],
                "score": corpus.scores[row],
                "emb": corpus.vectors[row],
            }),
        )?;
        // Deviation 4's id-to-key vector is indexed by `sequence - 1`, so the
        // put order has to be the corpus order and nothing may be skipped.
        if id.sequence != (row + 1) as u64 {
            return Err(format!(
                "row {row} landed at sequence {}, breaking the id-to-key vector",
                id.sequence
            )
            .into());
        }
        if (row + 1) % BATCH == 0 {
            db.commit()?;
        }
    }
    db.commit()?;
    let load = at.elapsed().as_secs_f64();

    eprintln!("[{family}] building the {family} index over emb …");
    let at = Instant::now();
    let index = match family {
        "exact" => db.create_exact_vector_index(person, "person_emb_exact", "emb")?,
        "vamana" => db.create_vamana_index(person, "person_emb_vamana", "emb")?,
        other => return Err(format!("unknown index family {other}").into()),
    };
    db.commit()?;
    let (mut db, build) = build_to_ready_adaptively(dir, db, index, BUILD_CHUNK, family)?;
    eprintln!(
        "[{family}] index READY in {:.2} s at {} rows per transaction (chunk search {:.2} s); \
         creation and drive together {:.2} s",
        build.seconds,
        build.rows_per_transaction,
        build.chunk_search_seconds,
        at.elapsed().as_secs_f64()
    );
    db.checkpoint()?;
    Ok((db, person, index, load, build))
}

/// Drive a late build to READY, HALVING THE CHUNK when the page-WAL refuses
/// the transaction, and report the chunk that worked.
///
/// `build_index_to_ready` already halves its own GROUP on an allowance
/// refusal, but its floor is one chunk of `chunk_rows` in one transaction,
/// and at 4,096 dimensions one chunk of 256 is over the page-WAL's 16 MiB
/// per-transaction ceiling for a vamana graph: each inserted node writes a
/// 4,096-byte int8 code AND rewrites the neighbour list of every node the
/// robust prune touched, each on its own page. So the chunk itself has to
/// come down.
///
/// THE HANDLE HAS TO BE REOPENED BETWEEN TRIES. `Database::finish`
/// (`core/engine/src/collections/mod.rs:1385`) marks the handle FAILED on any
/// error it returns, and every later call answers `Error::Failed` — which is
/// the right rule for a handle whose last operation's outcome is unknown, and
/// which means a caller cannot simply retry on the same handle. The retry
/// therefore drops it and reopens the directory. That is legal, and is the
/// point of a resumable build: the BUILDING descriptor and its cursor are
/// COMMITTED, so the reopened handle finds the index exactly where the
/// refusal left it and carries on rather than starting over.
fn build_to_ready_adaptively(
    dir: &Path,
    mut db: Database,
    index: IndexId,
    start: usize,
    label: &str,
) -> R<(Database, Build)> {
    let mut chunk = start;
    let mut search = 0.0f64;
    let mut refused = Vec::new();
    loop {
        let before = db.io_counters()?;
        let at = Instant::now();
        match db.build_index_to_ready(index, chunk) {
            Ok(entries) => {
                db.commit()?;
                let after = db.io_counters()?;
                let wal_bytes = after.wal_bytes_written - before.wal_bytes_written;
                let frames = after.wal_frames_appended - before.wal_frames_appended;
                let commits = after.commit_frames - before.commit_frames;
                let build = Build {
                    rows_per_transaction: chunk,
                    seconds: at.elapsed().as_secs_f64(),
                    chunk_search_seconds: search,
                    chunk_search_tried: start,
                    refused_at: refused,
                    entries: entries as u64,
                    wal_bytes,
                    wal_frames: frames,
                    commits,
                };
                eprintln!(
                    "[{label}] READY: {chunk} rows per transaction, {:.1} MB of page-WAL over \
                     {frames} frames and {commits} commits — {:.0} WAL bytes per inserted node, \
                     so {:.1} nodes would fill the 16 MiB a transaction may hold",
                    wal_bytes as f64 / 1e6,
                    build.wal_bytes_per_row(),
                    (16u64 << 20) as f64 / build.wal_bytes_per_row().max(1.0)
                );
                return Ok((db, build));
            }
            Err(error) if error.to_string().contains("allowance") && chunk > BUILD_CHUNK_FLOOR => {
                search += at.elapsed().as_secs_f64();
                refused.push(chunk);
                eprintln!(
                    "[{label}] the page-WAL refused a {chunk}-row build transaction ({error}); \
                     reopening and retrying at {}",
                    chunk / 2
                );
                chunk /= 2;
                drop(db);
                db = Database::open(dir, e4_config(BUILD_BUDGET))?;
            }
            // THE FLOOR. One row per transaction is the smallest transaction
            // this build can be driven in, and if the page-WAL will not take
            // that then the index cannot be built here at all. The loop stops
            // and the arm fails by name; it does not keep halving a number
            // that has nowhere left to go.
            Err(error) if error.to_string().contains("allowance") => {
                return Err(format!(
                    "{label}: the page-WAL refused even a {BUILD_CHUNK_FLOOR}-row build \
                     transaction ({error}), after refusing {refused:?}. A vamana insert is a \
                     graph SEARCH plus back-edge repair, not a row write: it visits up to \
                     {BUILD_VISITED_NODES} nodes and RobustPrune rewrites the neighbour list of \
                     up to {VAMANA_DEGREE} existing nodes, each on its own page in the 0x7D \
                     keyspace, so the transaction's page footprint is set by the SEARCH and not \
                     by the row count. The 16 MiB ceiling is `WAL_CAP` \
                     (core/engine/src/store/pagewal/mod.rs:30) and it is a constant: \
                     `wal_allowance()` at :1244 is `WAL_CAP.min(limits.1)`, so \
                     `set_runtime_limits` can only LOWER it and there is no knob that raises it. \
                     This index cannot be built at this width without a change inside core, which \
                     this bench is not permitted to make and would not make in any case."
                )
                .into());
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// What a late build cost, and at what transaction size it was admitted.
///
/// `seconds` is the SUCCESSFUL pass alone, because that is the number a
/// caller who already knows the right chunk would pay.
/// `chunk_search_seconds` is what the halving cost on top — every refused
/// try aborts on its FIRST transaction, so it is small, but it is reported
/// rather than folded in.
#[derive(Clone, Debug)]
struct Build {
    rows_per_transaction: usize,
    seconds: f64,
    chunk_search_seconds: f64,
    chunk_search_tried: usize,
    /// Every transaction size the page-WAL refused on the way down. Empty
    /// when the first try was admitted.
    refused_at: Vec<usize>,
    /// What `build_index_to_ready` reported it wrote.
    entries: u64,
    /// Page-WAL bytes, frames and commits the SUCCESSFUL pass appended. This
    /// is the measurement that explains the transaction size: a build is
    /// admitted only while `wal_bytes_per_row * rows_per_transaction` stays
    /// under the 16 MiB one transaction may hold.
    wal_bytes: u64,
    wal_frames: u64,
    commits: u64,
}

impl Build {
    fn wal_bytes_per_row(&self) -> f64 {
        self.wal_bytes as f64 / ROWS as f64
    }

    fn to_json(&self) -> Value {
        json!({
            "rows_per_transaction": self.rows_per_transaction,
            "seconds": self.seconds,
            "chunk_search_seconds": self.chunk_search_seconds,
            "chunk_search_started_at": self.chunk_search_tried,
            "transaction_sizes_the_page_wal_refused": self.refused_at,
            "entries_reported": self.entries,
            "page_wal_bytes": self.wal_bytes,
            "page_wal_frames": self.wal_frames,
            "page_wal_commits": self.commits,
            "page_wal_bytes_per_row": self.wal_bytes_per_row(),
            "rows_that_would_fill_one_transaction":
                (16u64 << 20) as f64 / self.wal_bytes_per_row().max(1.0),
            "transaction_ceiling_bytes": 16u64 << 20,
            "transaction_ceiling_is": "WAL_CAP, core/engine/src/store/pagewal/mod.rs:30, checked \
                                       at :509. wal_allowance() at :1244 is WAL_CAP.min(limits.1), \
                                       so set_runtime_limits can only LOWER it; nothing raises it.",
        })
    }
}

/// Reopen a built store at one pool budget. The store is the same bytes
/// either way; only what the pool can hold of it changes.
fn reopen(dir: &Path, budget: usize) -> R<Database> {
    Ok(Database::open(dir, e4_config(budget))?)
}

fn pool_sample(db: &Database) -> Option<[u64; 4]> {
    db.pool_counters().ok().map(|(h, m, e, s)| [h, m, e, s])
}

fn arm_e4_atomic_exact(
    dir: &Path,
    corpus: &Corpus,
    queries: &[Vec<f32>],
    truth: &[Vec<String>],
) -> R<Vec<ArmRow>> {
    let (db, _person, index, load, build) = load_e4(dir, corpus, "exact")?;
    drop(db);
    let disk = dir_bytes(dir);
    let (n_rows, n_dim) = (n_rows(), n_dim());
    let mut out = Vec::new();
    for budget in POOL_BUDGETS {
        let db = reopen(dir, budget)?;
        eprintln!("[exact] querying at a {} pool …", mib(budget));
        let measured = measure(
            truth,
            |i| {
                let hits = db.query_exact_vector(
                    index,
                    &queries[i],
                    VectorMetric::Cosine,
                    K,
                    VectorCandidates::All,
                    usize::MAX,
                    || false,
                )?;
                Ok(hits
                    .into_iter()
                    .map(|hit| corpus.keys[(hit.id.sequence - 1) as usize].clone())
                    .collect())
            },
            || pool_sample(&db),
        )?;
        eprintln!(
            "[exact] {} pool: median {:.2} ms, recall@10 {:.3}",
            mib(budget),
            measured.median_ms,
            measured.recall
        );
        drop(db);
        let mut row = ArmRow::new("e4-atomic-exact");
        row.pool_bytes = Some(budget);
        row.disk_bytes = Some(disk);
        row.load_seconds = Some(load);
        row.index_seconds = Some(build.seconds);
        row.measured = Some(measured);
        row.extra = json!({"build": build.to_json(), "built_at_pool": mib(BUILD_BUDGET)});
        row.did = format!(
            "An embedded `Database`, one collection of four declared fields, one \
             `create_exact_vector_index` over `emb`, asked through \
             `Database::query_exact_vector` with `VectorCandidates::All`, at a {} page pool. That \
             atomic's definition is a scan: it walks all {n_rows} persisted locators and scores \
             all {n_rows} f32 sidecars of {n_dim} lanes each, {:.1} MB in all, for every query. \
             Exact by construction, and by definition a scan: each sidecar is touched once and \
             never returned to. {}",
            mib(budget),
            (ROWS * DIM * 4) as f64 / 1e6,
            pool_sentence(row.measured.as_ref().expect("just measured"), budget)
        );
        out.push(row);
    }
    Ok(out)
}

fn arm_e4_atomic_vamana(
    dir: &Path,
    corpus: &Corpus,
    queries: &[Vec<f32>],
    truth: &[Vec<String>],
    ef: usize,
) -> R<Vec<ArmRow>> {
    let (db, _person, index, load, build) = load_e4(dir, corpus, "vamana")?;
    drop(db);
    let disk = dir_bytes(dir);
    let mut out = Vec::new();
    for budget in POOL_BUDGETS {
        let db = reopen(dir, budget)?;
        eprintln!("[vamana] querying at a {} pool …", mib(budget));
        let mut sweep = Vec::new();
        let mut headline = None;
        for point in EF_SWEEP {
            let measured = measure(
                truth,
                |i| {
                    let result = db.query_vamana_vector(
                        index,
                        &queries[i],
                        VectorMetric::Cosine,
                        K,
                        point,
                        usize::MAX,
                        || false,
                    )?;
                    Ok(result
                        .hits
                        .into_iter()
                        .map(|hit| corpus.keys[(hit.id.sequence - 1) as usize].clone())
                        .collect())
                },
                || pool_sample(&db),
            )?;
            // One extra walk purely to read the counters the timed pass must
            // not pay for.
            let probe = db.query_vamana_vector(
                index,
                &queries[0],
                VectorMetric::Cosine,
                K,
                point,
                usize::MAX,
                || false,
            )?;
            eprintln!(
                "[vamana] {} pool, ef={point}: median {:.3} ms, recall@10 {:.3}, examined {} on \
                 query 0",
                mib(budget),
                measured.median_ms,
                measured.recall,
                probe.examined
            );
            sweep.push(json!({
                "ef": point,
                "query_median_ms": measured.median_ms,
                "query_p90_ms": measured.p90_ms,
                "recall_at_10": measured.recall,
                "page_pool": measured.pool,
                "examined_query0": probe.examined,
                "reranked_query0": probe.reranked,
            }));
            if point == ef {
                headline = Some(measured);
            }
        }
        let headline = headline
            .ok_or_else(|| format!("--ef {ef} is not one of the swept values {EF_SWEEP:?}"))?;
        drop(db);
        let mut row = ArmRow::new("e4-atomic-vamana");
        row.pool_bytes = Some(budget);
        row.disk_bytes = Some(disk);
        row.load_seconds = Some(load);
        row.index_seconds = Some(build.seconds);
        row.measured = Some(headline);
        row.extra = json!({
            "ef": ef,
            "ef_sweep": sweep,
            "build": build.to_json(),
            "built_at_pool": mib(BUILD_BUDGET),
        });
        row.did = format!(
            "The same database shape in its own directory, one `create_vamana_index` over `emb`, \
             asked through `Database::query_vamana_vector` at ef = {ef} and a {} page pool: a \
             greedy walk from the medoid over a single-layer graph of symmetric-int8 codes, \
             reranked against the f32 sidecars. Unlike the scan, this walk RETURNS to the same \
             pages — the medoid and the hubs near it are on the path of every query — so it is \
             the arm a pool budget could help or hurt. {} The headline row is ef = {ef}; the \
             sweep {EF_SWEEP:?} is in the JSON with the nodes examined and the pool counters at \
             each point.",
            mib(budget),
            pool_sentence(row.measured.as_ref().expect("just measured"), budget)
        );
        out.push(row);
    }
    Ok(out)
}

// ── arms 3 and 4: the SQL surface ─────────────────────────────────────────

/// Build and load the same corpus through SQL alone, then create ONE vector
/// index by its SQL spelling.
///
/// `WITH (index: none)` is deliberate: `CREATE TABLE` otherwise builds an
/// automatic scalar index over every TEXT, INT and DOUBLE PRECISION column,
/// and arms 1 and 2 have no scalar index at all. Suppressing them keeps the
/// four sekejap directories comparable on disk.
fn load_e4_sql(
    dir: &Path,
    corpus: &Corpus,
    ddl: &str,
    label: &str,
) -> R<(Database, f64, Result<f64, String>)> {
    let _ = fs::remove_dir_all(dir);
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut db = Database::create(dir, e4_config(BUILD_BUDGET))?;
    db.sql(
        &format!(
            "CREATE TABLE person (name TEXT, age INT, score DOUBLE PRECISION, emb VECTOR({DIM})) \
             WITH (index: none)"
        ),
        &[],
    )?;

    eprintln!("[{label}] inserting {ROWS} rows through SQL, commit every {BATCH} …");
    let at = Instant::now();
    let insert = "INSERT INTO person (_key, name, age, score, emb) VALUES ($1, $2, $3, $4, $5)";
    db.begin_bulk()?;
    for row in 0..ROWS {
        db.sql(
            insert,
            &[
                Param::Text(corpus.keys[row].clone()),
                Param::Text(corpus.names[row].clone()),
                Param::Int(corpus.ages[row]),
                Param::Float(corpus.scores[row]),
                Param::Vector(corpus.vectors[row].clone()),
            ],
        )?;
        if (row + 1) % BATCH == 0 {
            db.end_bulk()?;
            db.begin_bulk()?;
        }
    }
    db.end_bulk()?;
    let load = at.elapsed().as_secs_f64();

    eprintln!("[{label}] {ddl}");
    let at = Instant::now();
    // The DDL's outcome is DATA here, not an error to propagate: a CREATE
    // INDEX this engine refuses at this dimension is one of the answers this
    // bench exists to report.
    let build = match db.sql(ddl, &[]) {
        Ok(_) => Ok(at.elapsed().as_secs_f64()),
        Err(error) => {
            eprintln!("[{label}] CREATE INDEX REFUSED: {error}");
            // A refused statement leaves the handle dirty with its own
            // half-written work, and `checkpoint` requires a committed one.
            // Discarding it is what the compiler's own unwind does
            // (`lang/src/compile/plan.rs::unwind_create_table`).
            db.rollback()?;
            Err(error.to_string())
        }
    };
    db.checkpoint()?;
    Ok((db, load, build))
}

/// One SQL vector order, compiled and walked, returning the keys in rank
/// order. The compile is inside the timing on purpose (deviation 5).
fn e4_sql_answer(db: &Database, sql: &str, query: &[f32]) -> R<Vec<String>> {
    let prepared = prepare_sql(db, sql, &[Param::Vector(query.to_vec())])?;
    let mut keys = Vec::with_capacity(K);
    prepared.for_each_row(db, PAGE, &mut |row| {
        if let Some(SqlValue::Text(key)) = row.values.first() {
            keys.push(key.clone());
        }
        Ok(())
    })?;
    Ok(keys)
}

fn arm_e4_sql_exact(
    dir: &Path,
    corpus: &Corpus,
    queries: &[Vec<f32>],
    truth: &[Vec<String>],
) -> R<Vec<ArmRow>> {
    let ddl = "CREATE INDEX person_emb_exact ON person USING exact (emb)";
    let (db, load, build) = load_e4_sql(dir, corpus, ddl, "sql-exact")?;
    drop(db);
    let disk = dir_bytes(dir);
    let sql = format!("SELECT _key FROM person ORDER BY emb <=> $1 LIMIT {K}");
    let n_rows = n_rows();
    let mut out = Vec::new();
    for budget in POOL_BUDGETS {
        let db = reopen(dir, budget)?;
        eprintln!("[sql-exact] querying at a {} pool …", mib(budget));
        let measured = measure(
            truth,
            |i| e4_sql_answer(&db, &sql, &queries[i]),
            || pool_sample(&db),
        )?;
        let prepare_median = median_of(|i| {
            prepare_sql(&db, &sql, &[Param::Vector(queries[i].clone())])?;
            Ok(())
        })?;
        eprintln!(
            "[sql-exact] {} pool: median {:.2} ms (compile alone {:.2} ms), recall@10 {:.3}",
            mib(budget),
            measured.median_ms,
            prepare_median,
            measured.recall
        );
        drop(db);
        let mut row = ArmRow::new("e4-sql-exact");
        row.pool_bytes = Some(budget);
        row.disk_bytes = Some(disk);
        row.load_seconds = Some(load);
        match &build {
            Ok(seconds) => row.index_seconds = Some(*seconds),
            Err(refusal) => row.refusal = Some(refusal.clone()),
        }
        row.measured = Some(measured);
        row.extra = json!({
            "statement": sql,
            "create_index": ddl,
            "prepare_median_ms": prepare_median,
            "built_at_pool": mib(BUILD_BUDGET),
        });
        row.did = format!(
            "A third database built and asked entirely in SQL, read back at a {} page pool. \
             `CREATE TABLE person (...) WITH (index: none)`, {n_rows} `INSERT INTO person (_key, \
             name, age, score, emb) VALUES ($1..$5)` under `begin_bulk`/`end_bulk` every {BATCH} \
             rows, then `{ddl}`. Every query is `{sql}`, compiled and walked. It is the same \
             engine and the same index family as arm 1, so the gap between the two arms is what \
             the SQL surface costs; the compile alone is `prepare_median_ms` in the JSON. {}",
            mib(budget),
            pool_sentence(row.measured.as_ref().expect("just measured"), budget)
        );
        out.push(row);
    }
    Ok(out)
}

fn arm_e4_sql_vamana(
    dir: &Path,
    corpus: &Corpus,
    queries: &[Vec<f32>],
    truth: &[Vec<String>],
    ef: usize,
) -> R<Vec<ArmRow>> {
    let ddl = "CREATE INDEX person_emb_vamana ON person USING vamana (emb vector_cosine_ops)";
    let (mut db, load, build) = load_e4_sql(dir, corpus, ddl, "sql-vamana")?;

    // WHAT SQL COULD DO. Whatever the DDL answered is this arm's index cell;
    // `disk_bytes` is measured HERE, over the store SQL alone produced, so it
    // is not inflated by the follow-up probe below.
    let ddl_refusal = build.clone().err();
    db.checkpoint()?;
    drop(db);
    let sql_only_bytes = dir_bytes(dir);

    // WHAT THE ENGINE COULD DO, asked as a separate, LABELLED probe rather
    // than folded into the arm's own numbers: build the same graph through
    // the atomic API at a transaction size the page-WAL admits, then ask the
    // SQL query again. It answers a question the arm's own refusal leaves
    // open — whether the SQL statement would have found the index if the SQL
    // build had succeeded — and its outcome is reported beside the arm, never
    // in place of it.
    let mut db = Database::open(dir, e4_config(BUILD_BUDGET))?;
    let person = db
        .collection("person")?
        .ok_or("the SQL load left no `person` collection behind")?;
    let mut probe = json!({"ran": false});
    let mut graph_bytes = Value::Null;
    let mut atomic_build = Value::Null;
    if ddl_refusal.is_some() {
        eprintln!("[sql-vamana] probe: building the same graph through the atomic API …");
        // The refused statement may or may not have left a BUILDING
        // descriptor committed behind it — `build_index_to_ready` commits its
        // chunks, so whether one survives depends on how far it got. Reuse
        // the descriptor if it is there rather than colliding on the name.
        let existing = db
            .list_indexes(person)?
            .into_iter()
            .find(|info| info.name == "person_emb_vamana")
            .map(|info| info.id);
        let index = match existing {
            Some(id) => {
                eprintln!("[sql-vamana] probe: resuming the descriptor the refusal left BUILDING");
                id
            }
            None => db.create_vamana_index(person, "person_emb_vamana", "emb")?,
        };
        db.commit()?;
        let (reopened, built) =
            build_to_ready_adaptively(dir, db, index, BUILD_CHUNK, "sql-vamana probe")?;
        db = reopened;
        atomic_build = json!(built.seconds);
        db.checkpoint()?;
        probe = json!({
            "ran": true,
            "build": built.to_json(),
            "why": "the SQL spelling of this index was refused, so the graph the SQL query would \
                    have needed was built through Database::create_vamana_index instead",
        });
    }
    drop(db);
    if ddl_refusal.is_some() {
        graph_bytes = json!(dir_bytes(dir));
    }

    let set = format!("SET LOCAL ef_search = {ef}");
    let sql = format!("SELECT _key FROM person ORDER BY emb <=> $1 LIMIT {K}");
    let (n_rows, n_dim) = (n_rows(), n_dim());
    let mut out = Vec::new();
    for budget in POOL_BUDGETS {
        let mut db = reopen(dir, budget)?;
        let set_outcome = match db.sql(&set, &[]) {
            Ok(_) => "accepted".to_owned(),
            Err(error) => format!("refused: {error}"),
        };
        let mut row = ArmRow::new("e4-sql-vamana");
        row.pool_bytes = Some(budget);
        row.load_seconds = Some(load);
        row.disk_bytes = Some(sql_only_bytes);
        if let Ok(seconds) = &build {
            row.index_seconds = Some(*seconds);
        }
        let query_outcome = match e4_sql_answer(&db, &sql, &queries[0]) {
            Ok(_) => {
                // The planner named the graph after all: measure it like any
                // other arm rather than reporting a refusal that is not true.
                let measured = measure(
                    truth,
                    |i| e4_sql_answer(&db, &sql, &queries[i]),
                    || pool_sample(&db),
                )?;
                row.measured = Some(measured);
                "answered".to_owned()
            }
            Err(error) => {
                let refusal = error.to_string();
                row.refusal = Some(match &ddl_refusal {
                    Some(ddl_error) => format!("CREATE INDEX: {ddl_error} | SELECT: {refusal}"),
                    None => refusal.clone(),
                });
                refusal
            }
        };
        drop(db);
        row.extra = json!({
            "create_index": ddl,
            "create_index_refusal": ddl_refusal,
            "set_local": set,
            "set_local_outcome": set_outcome,
            "statement": sql,
            "statement_outcome": query_outcome,
            "disk_bytes_sql_only": sql_only_bytes,
            "disk_bytes_with_atomic_built_graph": graph_bytes,
            "atomic_build_seconds": atomic_build,
            "atomic_probe": probe,
            "built_at_pool": mib(BUILD_BUDGET),
            "planner": "lang/src/compile/select.rs:400-403 — vector_order() looks for a READY \
                        ExactVector index and then a READY QuantizedVector index; \
                        IndexFamily::VamanaGraph is not among the two it can name.",
            "engine": "core/engine/src/query/plan.rs:495-501 accepts a quantized OR a vamana \
                       index for QueryOrder::ApproximateVector, and plan.rs:1673 routes the \
                       vamana one to DriverPlan::VamanaVector — so the engine can answer this \
                       order; no SQL statement reaches it.",
            "build_driver": "lang/src/compile/plan.rs:1277 drives every CREATE INDEX with \
                             build_index_to_ready(id, 256); that chunk size is not a parameter \
                             of the statement.",
        });
        row.did = match (&ddl_refusal, &row.measured) {
            (None, Some(_)) => format!(
                "A fourth database, loaded through SQL exactly as arm 3, with `{ddl}`, read back \
                 at a {} page pool. `{set}` then `{sql}`, measured like any other arm.",
                mib(budget)
            ),
            (None, None) => format!(
                "The DDL ran: `{ddl}` built a real graph through SQL, which is where this row's \
                 disk and index-build numbers come from. The QUERY did not. `{set}` was \
                 {set_outcome} and `{sql}` was refused — {query_outcome} — the SQL vector-order \
                 planner (`lang/src/compile/select.rs:400-403`) considers the exact and the \
                 quantized families and nothing else, so a table whose only vector index is a \
                 vamana graph has no vector index as far as a SELECT is concerned. The engine \
                 beneath it is willing — `core/engine/src/query/plan.rs:495-501` accepts a vamana \
                 index for an approximate order — but there is no statement that names it. Arm 2 \
                 is the same graph asked through the atomic API. The refusal does not depend on \
                 the page pool, so both of this arm's rows carry it."
            ),
            (Some(ddl_error), _) => format!(
                "THIS ARM IS BLOCKED TWICE OVER, and neither blocker is about how fast anything \
                 is, so neither depends on the page pool — both of this arm's rows carry the same \
                 pair of refusals. The load is real: {n_rows} rows went in through `INSERT INTO \
                 person (_key, name, age, score, emb) VALUES ($1..$5)` and the disk figure is the \
                 store those statements left. Then `{ddl}` was REFUSED: {ddl_error}. \
                 `lang/src/compile/plan.rs:1277` drives every `CREATE INDEX` with \
                 `build_index_to_ready(id, 256)`, and at {n_dim} dimensions one 256-row vamana \
                 transaction is over the page-WAL's 16 MiB per-transaction ceiling — each \
                 inserted node writes a 4,096-byte int8 code and rewrites the neighbour list of \
                 every node its robust prune touched, each on its own page. The chunk is a \
                 constant of the compiler, not a parameter of the statement, so there is no SQL \
                 spelling that builds this index at this width. Arm 2 reaches READY on the same \
                 corpus because the atomic caller can choose the transaction size, and this arm's \
                 own probe did the same thing here to answer the second question. And the second \
                 blocker is the one that would have stopped it anyway: with the graph built and \
                 READY, `{set}` was {set_outcome} and `{sql}` was still refused — {query_outcome} \
                 — and `Compiler::vector_order` (`lang/src/compile/select.rs:400-403`) looks for \
                 a READY ExactVector index and then a READY QuantizedVector index and names no \
                 third family, so `IndexFamily::VamanaGraph` is invisible to a SELECT — even \
                 though `core/engine/src/query/plan.rs:495-501` accepts exactly that index for an \
                 approximate order. Arm 2 is this graph asked through the atomic API, and it \
                 answers."
            ),
        };
        out.push(row);
    }
    Ok(out)
}

// ── arm 5: SQLite ─────────────────────────────────────────────────────────

fn lite_files(path: &Path) -> [PathBuf; 4] {
    [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
        PathBuf::from(format!("{}-journal", path.display())),
    ]
}

fn lite_disk_bytes(path: &Path) -> u64 {
    lite_files(path)
        .iter()
        .filter_map(|candidate| fs::metadata(candidate).ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

fn lite_error(message: String) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(message.into())
}

/// `cosine_distance(blob, blob)` — deviation 9. The same `1 - cos` every
/// other arm ranks by, over two little-endian f32 BLOBs.
fn lite_register(conn: &Connection) -> R<()> {
    conn.create_scalar_function(
        "cosine_distance",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let left = match ctx.get_raw(0) {
                ValueRef::Blob(bytes) => bytes,
                other => {
                    return Err(lite_error(format!(
                        "cosine_distance argument 1 must be a BLOB, not {other:?}"
                    )))
                }
            };
            let right = match ctx.get_raw(1) {
                ValueRef::Blob(bytes) => bytes,
                other => {
                    return Err(lite_error(format!(
                        "cosine_distance argument 2 must be a BLOB, not {other:?}"
                    )))
                }
            };
            if left.len() != right.len() || left.len() % 4 != 0 {
                return Err(lite_error(format!(
                    "cosine_distance over {} and {} bytes: both must be the same multiple of four",
                    left.len(),
                    right.len()
                )));
            }
            let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
            for lane in 0..left.len() / 4 {
                let at = lane * 4;
                let x = f64::from(f32::from_le_bytes([
                    left[at],
                    left[at + 1],
                    left[at + 2],
                    left[at + 3],
                ]));
                let y = f64::from(f32::from_le_bytes([
                    right[at],
                    right[at + 1],
                    right[at + 2],
                    right[at + 3],
                ]));
                dot += x * y;
                na += x * x;
                nb += y * y;
            }
            Ok(1.0 - dot / (na.sqrt() * nb.sqrt()).max(1e-12))
        },
    )?;
    Ok(())
}

fn lite_connect(path: &Path) -> R<Connection> {
    let conn = Connection::open(path)?;
    let mode: String = conn.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("delete") {
        return Err(format!("sqlite refused journal_mode=DELETE and stayed in {mode}").into());
    }
    conn.execute_batch(&format!(
        "PRAGMA synchronous = FULL;\n\
         PRAGMA cache_size = -{};",
        CACHE_BYTES / 1024
    ))?;
    lite_register(&conn)?;
    Ok(conn)
}

fn arm_sqlite(path: &Path, corpus: &Corpus, queries: &[Vec<f32>], truth: &[Vec<String>]) -> R<ArmRow> {
    let mut lite_row = ArmRow::new("sqlite");
    for candidate in lite_files(path) {
        let _ = fs::remove_file(candidate);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut conn = lite_connect(path)?;
    let sqlite_version: String =
        conn.query_row("SELECT sqlite_version()", [], |row| row.get(0))?;
    conn.execute_batch(
        "CREATE TABLE person (
             \"key\" text primary key,
             name text,
             age integer,
             score real,
             emb blob
         );",
    )?;

    eprintln!("[sqlite] inserting {ROWS} rows, one transaction per {BATCH} …");
    let at = Instant::now();
    for chunk in (0..ROWS).collect::<Vec<usize>>().chunks(BATCH) {
        let txn = conn.transaction()?;
        {
            let mut statement = txn.prepare(
                "INSERT INTO person (\"key\", name, age, score, emb) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for row in chunk {
                statement.execute(rusqlite::params![
                    corpus.keys[*row],
                    corpus.names[*row],
                    corpus.ages[*row],
                    corpus.scores[*row],
                    emb_blob(&corpus.vectors[*row]),
                ])?;
            }
        }
        txn.commit()?;
    }
    let load = at.elapsed().as_secs_f64();
    lite_row.load_seconds = Some(load);
    // There is nothing to build. A zero here would read as "instant"; the
    // cell is empty and the prose says why.
    lite_row.index_seconds = None;

    let sql = format!("SELECT \"key\" FROM person ORDER BY cosine_distance(emb, ?1) LIMIT {K}");
    let measured = {
        let mut statement = conn.prepare(&sql)?;
        measure(
            truth,
            |i| {
                let blob = emb_blob(&queries[i]);
                let mut rows = statement.query(rusqlite::params![blob])?;
                let mut keys = Vec::with_capacity(K);
                while let Some(row) = rows.next()? {
                    keys.push(row.get::<_, String>(0)?);
                }
                Ok(keys)
            },
            || None,
        )?
    };
    drop(conn);
    lite_row.pool_bytes = Some(CACHE_BYTES);
    lite_row.disk_bytes = Some(lite_disk_bytes(path));
    lite_row.measured = Some(measured);
    lite_row.extra = json!({
        "statement": sql,
        "sqlite_version": sqlite_version.clone(),
        "index_build": "none: SQLite has no vector index",
    });
    let n_rows = n_rows();
    lite_row.did = format!(
        "SQLite {sqlite_version}, rusqlite's bundled build, `journal_mode = DELETE`, \
         `synchronous = FULL`, \
         `cache_size = -{}` ({} MiB). One table, the embedding held as a {}-byte BLOB of \
         little-endian f32 lanes. THIS ARM IS A FULL TABLE SCAN AND THERE IS NO INDEX TO BUILD: \
         SQLite has no vector type and no vector index family, so every query reads all {n_rows} \
         BLOBs and scores each one in the registered `cosine_distance` function. Its index-build \
         cell is empty because there is nothing there, not because a build was skipped, and its \
         recall is 1.00 because a scan cannot miss. It is the yardstick the other rows are read \
         against. It is NOT swept over the two page-pool budgets the sekejap arms are: the sweep \
         exists to separate a graph's random re-reads from a scan's single pass, and this arm has \
         no graph. It runs at the larger cache throughout, which is the setting that flatters it.",
        CACHE_BYTES / 1024,
        CACHE_BYTES >> 20,
        DIM * 4
    );
    Ok(lite_row)
}

// ── arms 6 and 7, and the two supplementary rows: pgvector ────────────────

struct PgArm {
    /// Table name, also the row's identity on disk.
    table: &'static str,
    /// The index this arm wants, by name and DDL, or `None` for no index.
    index_name: &'static str,
    index_ddl: String,
    /// The order expression the query ranks by.
    order: String,
    /// A session knob the arm needs when its index exists, if any.
    knob: Option<String>,
    supplementary: bool,
    arm: &'static str,
}

fn pg_load(client: &mut Client, table: &str, corpus: &Corpus) -> R<f64> {
    client.batch_execute(&format!(
        "SET synchronous_commit = on;
         DROP TABLE IF EXISTS {table};
         CREATE TABLE {table} (
             \"key\" text primary key,
             name text,
             age int,
             score double precision,
             emb vector({DIM})
         );"
    ))?;
    eprintln!("[{table}] inserting {ROWS} rows, {PG_BATCH} per statement …");
    let at = Instant::now();
    for chunk in (0..ROWS).collect::<Vec<usize>>().chunks(PG_BATCH) {
        let mut sql = format!("INSERT INTO {table} (\"key\", name, age, score, emb) VALUES ");
        let mut params: Vec<Box<dyn ToSql + Sync>> = Vec::with_capacity(chunk.len() * 5);
        for (at_chunk, row) in chunk.iter().enumerate() {
            if at_chunk > 0 {
                sql.push(',');
            }
            let base = at_chunk * 5;
            sql.push_str(&format!(
                "(${},${},${},${},${}::text::vector)",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5
            ));
            params.push(Box::new(corpus.keys[*row].clone()));
            params.push(Box::new(corpus.names[*row].clone()));
            params.push(Box::new(corpus.ages[*row] as i32));
            params.push(Box::new(corpus.scores[*row]));
            params.push(Box::new(vector_literal(&corpus.vectors[*row])));
        }
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        let mut txn = client.transaction()?;
        txn.execute(sql.as_str(), &refs)?;
        txn.commit()?;
    }
    Ok(at.elapsed().as_secs_f64())
}

fn pg_total_bytes(client: &mut Client, table: &str) -> R<u64> {
    let row = client.query_one(
        // `$1::regclass` would make the server infer the PARAMETER's type as
        // `regclass`, which the client cannot serialise a string into. The
        // extra `::text` pins the parameter to text and casts afterwards.
        "SELECT pg_total_relation_size($1::text::regclass)::int8",
        &[&table],
    )?;
    let bytes: i64 = row.get(0);
    Ok(bytes as u64)
}

fn pg_size_breakdown(client: &mut Client, table: &str) -> R<Value> {
    let row = client.query_one(
        "SELECT pg_relation_size($1::text::regclass)::int8,
                pg_indexes_size($1::text::regclass)::int8,
                coalesce(pg_total_relation_size(reltoastrelid), 0)::int8
           FROM pg_class WHERE oid = $1::text::regclass",
        &[&table],
    )?;
    let heap: i64 = row.get(0);
    let indexes: i64 = row.get(1);
    let toast: i64 = row.get(2);
    Ok(json!({"heap_bytes": heap, "index_bytes": indexes, "toast_bytes": toast}))
}

fn arm_pg(
    client: &mut Client,
    spec: &PgArm,
    corpus: &Corpus,
    queries: &[Vec<f32>],
    truth: &[Vec<String>],
    shared_buffers: &str,
) -> R<ArmRow> {
    let mut row = ArmRow::new(spec.arm);
    row.supplementary = spec.supplementary;
    // Postgres manages its own buffer pool and is not swept over the two
    // budgets the sekejap arms are. The cell says what the server was set to.
    row.pool_note = format!("{shared_buffers} (server)");
    let load = pg_load(client, spec.table, corpus)?;
    row.load_seconds = Some(load);

    eprintln!("[{}] {}", spec.table, spec.index_ddl);
    let at = Instant::now();
    let built = match client.batch_execute(&spec.index_ddl) {
        Ok(()) => {
            row.index_seconds = Some(at.elapsed().as_secs_f64());
            true
        }
        Err(error) => {
            // A failed statement poisons nothing here — each is its own
            // implicit transaction — but the message is the point.
            let message = error
                .as_db_error()
                .map(|db| db.message().to_owned())
                .unwrap_or_else(|| error.to_string());
            eprintln!("[{}] index REFUSED: {message}", spec.table);
            row.refusal = Some(format!("{}: {message}", spec.index_name));
            row.index_seconds = None;
            false
        }
    };

    client.batch_execute(&format!("VACUUM ANALYZE {};", spec.table))?;
    client.batch_execute("CHECKPOINT;")?;
    row.disk_bytes = Some(pg_total_bytes(client, spec.table)?);
    let breakdown = pg_size_breakdown(client, spec.table)?;

    let mut setup = Vec::new();
    if built {
        if let Some(knob) = &spec.knob {
            setup.push(knob.clone());
        }
        // Without this an index that DOES exist may still lose to a
        // sequential scan in the planner's estimate, and the arm would
        // silently measure the scan again.
        setup.push("SET enable_seqscan = off".to_owned());
    }
    // The knobs are SESSION settings applied ONCE, not `SET LOCAL` inside a
    // transaction per query. `SET LOCAL` would be the more careful spelling,
    // but it needs a transaction around every measured statement, and BEGIN,
    // two SETs and ROLLBACK are four extra round trips on a statement an ANN
    // index answers in about a millisecond — the harness would then be
    // measuring its own protocol traffic. The measured statement is one round
    // trip in every Postgres arm, and `RESET ALL` puts the session back
    // afterwards so no arm inherits another's knobs.
    for knob in &setup {
        client.batch_execute(knob)?;
    }
    let sql = format!(
        "SELECT \"key\" FROM {} ORDER BY {} LIMIT {K}",
        spec.table, spec.order
    );
    // The plan is a DIAGNOSTIC, so it is taken with the query vector written
    // out as a literal rather than bound: `EXPLAIN` over a parameterised
    // statement makes the server infer the parameter's type from a context
    // that is not the execution's, and the client cannot always serialise
    // what it then asks for. Inlining sidesteps that, and a plan that cannot
    // be taken is RECORDED rather than allowed to end the run — no number in
    // this arm depends on it.
    let plan = {
        let literal = vector_literal(&queries[0]);
        let explained = sql.replace("$1", &format!("'{literal}'"));
        match client.query(&format!("EXPLAIN (FORMAT TEXT) {explained}"), &[]) {
            Ok(rows) => rows
                .iter()
                .map(|r| r.get::<_, String>(0))
                .collect::<Vec<String>>()
                .join(" | "),
            Err(error) => format!("EXPLAIN failed: {error}"),
        }
    };
    eprintln!("[{}] plan: {plan}", spec.table);

    let measured = measure(
        truth,
        |i| {
            let literal = vector_literal(&queries[i]);
            let rows = client.query(sql.as_str(), &[&literal as &(dyn ToSql + Sync)])?;
            Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
        },
        || None,
    )?;
    client.batch_execute("RESET ALL;")?;
    row.measured = Some(measured);
    row.extra = json!({
        "statement": sql,
        "setup": setup,
        "plan": plan,
        "index_built": built,
        "sizes": breakdown,
    });
    Ok(row)
}

fn pg_specs() -> Vec<PgArm> {
    vec![
        PgArm {
            arm: "pg-hnsw",
            table: "vec10k_hnsw",
            index_name: "vec10k_hnsw_emb_hnsw",
            index_ddl:
                "CREATE INDEX vec10k_hnsw_emb_hnsw ON vec10k_hnsw USING hnsw (emb vector_cosine_ops)"
                    .to_owned(),
            order: "emb <=> $1::text::vector".to_owned(),
            knob: Some("SET hnsw.ef_search = 100".to_owned()),
            supplementary: false,
        },
        PgArm {
            arm: "pg-ivfflat",
            table: "vec10k_ivfflat",
            index_name: "vec10k_ivfflat_emb_ivfflat",
            index_ddl: "CREATE INDEX vec10k_ivfflat_emb_ivfflat ON vec10k_ivfflat USING ivfflat \
                        (emb vector_cosine_ops) WITH (lists = 100)"
                .to_owned(),
            order: "emb <=> $1::text::vector".to_owned(),
            knob: Some("SET ivfflat.probes = 10".to_owned()),
            supplementary: false,
        },
        PgArm {
            arm: "pg-hnsw-sub2000",
            table: "vec10k_hnsw_sub",
            index_name: "vec10k_hnsw_sub_emb_hnsw",
            index_ddl: format!(
                "CREATE INDEX vec10k_hnsw_sub_emb_hnsw ON vec10k_hnsw_sub USING hnsw \
                 ((subvector(emb, 1, {SUBVECTOR_LANES})::vector({SUBVECTOR_LANES})) \
                 vector_cosine_ops)"
            ),
            order: format!(
                "subvector(emb, 1, {SUBVECTOR_LANES})::vector({SUBVECTOR_LANES}) <=> \
                 subvector($1::text::vector, 1, {SUBVECTOR_LANES})::vector({SUBVECTOR_LANES})"
            ),
            knob: Some("SET hnsw.ef_search = 100".to_owned()),
            supplementary: true,
        },
        PgArm {
            arm: "pg-ivfflat-sub2000",
            table: "vec10k_ivfflat_sub",
            index_name: "vec10k_ivfflat_sub_emb_ivfflat",
            index_ddl: format!(
                "CREATE INDEX vec10k_ivfflat_sub_emb_ivfflat ON vec10k_ivfflat_sub USING ivfflat \
                 ((subvector(emb, 1, {SUBVECTOR_LANES})::vector({SUBVECTOR_LANES})) \
                 vector_cosine_ops) WITH (lists = 100)"
            ),
            order: format!(
                "subvector(emb, 1, {SUBVECTOR_LANES})::vector({SUBVECTOR_LANES}) <=> \
                 subvector($1::text::vector, 1, {SUBVECTOR_LANES})::vector({SUBVECTOR_LANES})"
            ),
            knob: Some("SET ivfflat.probes = 10".to_owned()),
            supplementary: true,
        },
    ]
}

fn pg_did(spec: &PgArm, row: &ArmRow, server: &str) -> String {
    let n_dim = n_dim();
    let head = format!(
        "{server}, table `{}(\"key\" text primary key, name text, age int, \
         score double precision, emb vector({n_dim}))`, loaded in {PG_BATCH}-row transactions at \
         `synchronous_commit = on` with each embedding arriving as a `$n::text::vector` literal. \
         Disk is `pg_total_relation_size`, which counts the heap, the TOAST relation the 16 KB \
         embeddings live in, the primary key and any vector index, after `VACUUM ANALYZE` and \
         `CHECKPOINT`.",
        spec.table
    );
    match &row.refusal {
        Some(refusal) => format!(
            "{head} THE INDEX WAS REFUSED — {refusal}. pgvector 0.8.6 caps an hnsw or an ivfflat \
             index at 2,000 dimensions for the `vector` type (4,000 for `halfvec`), and this \
             column is {n_dim}. Nothing was \
             built, so the query fell back to the sequential scan Postgres runs when a column has \
             no vector index: the latency and the 1.00 recall in this row belong to THAT SCAN, not \
             to an ANN index. The `-sub2000` supplementary row below is the only route pgvector has \
             to an ANN index at this width."
        ),
        None => format!(
            "{head} The index built: `{}`. `EXPLAIN` is in the JSON, and `enable_seqscan = off` is \
             set for the session so a planner estimate cannot quietly substitute the scan this \
             row exists to avoid. The knobs are in `extra.setup`.",
            spec.index_ddl
        ),
    }
}

// ── the report ────────────────────────────────────────────────────────────

fn table_row(row: &ArmRow) -> String {
    let (median, recall) = match (&row.measured, &row.refusal) {
        (Some(m), _) => (format!("{:.2}", m.median_ms), format!("{:.3}", m.recall)),
        (None, Some(_)) => ("refused".to_owned(), "refused".to_owned()),
        (None, None) => ("—".to_owned(), "—".to_owned()),
    };
    let build = match (&row.index_seconds, &row.refusal, row.arm.as_str()) {
        (Some(v), _, _) => format!("{v:.2}"),
        (None, _, "sqlite") => "none".to_owned(),
        (None, Some(_), _) => "refused".to_owned(),
        (None, None, _) => "—".to_owned(),
    };
    let pool = match row.pool_bytes {
        Some(bytes) => mib(bytes),
        None => row.pool_note.clone(),
    };
    format!(
        "| `{}` | {} | {} | {} | {} | {} | {} |",
        row.arm,
        pool,
        mb(row.disk_bytes),
        secs(row.load_seconds),
        build,
        median,
        recall
    )
}

const TABLE_HEAD: &str =
    "| arm | page pool | disk MB | load s | index build s | query median ms | recall@10 |\n\
     | --- | --- | ---: | ---: | ---: | ---: | ---: |\n";

/// Where — if anywhere — the graph starts beating the linear scan.
///
/// One sentence per pool budget, comparing the two ATOMIC arms, because they
/// are the pair that differ in the index family alone. If the graph never
/// wins, that is what it says, and the reason goes with it.
fn crossover(rows: &[ArmRow]) -> String {
    let at = |arm: &str, pool: usize| -> Option<f64> {
        rows.iter()
            .find(|r| r.arm == arm && r.pool_bytes == Some(pool))
            .and_then(|r| r.measured.as_ref())
            .map(|m| m.median_ms)
    };
    let mut out = String::new();
    let mut crossed = false;
    for pool in POOL_BUDGETS {
        let (Some(scan), Some(graph)) = (
            at("e4-atomic-exact", pool),
            at("e4-atomic-vamana", pool),
        ) else {
            continue;
        };
        if graph < scan {
            crossed = true;
            out.push_str(&format!(
                "- At a {} pool the graph WINS: {graph:.2} ms against the scan's {scan:.2} ms, \
                 {:.1}x faster.\n",
                mib(pool),
                scan / graph
            ));
        } else {
            out.push_str(&format!(
                "- At a {} pool the graph LOSES: {graph:.2} ms against the scan's {scan:.2} ms, \
                 {:.1}x slower.\n",
                mib(pool),
                graph / scan
            ));
        }
    }
    if crossed && !out.is_empty() {
        // Which arm the pool actually bound on is a measurement, and the two
        // access patterns predict opposite answers, so it is read off the
        // counters rather than asserted.
        let misses = |arm: &str, pool: usize| -> f64 {
            rows.iter()
                .find(|r| r.arm == arm && r.pool_bytes == Some(pool))
                .and_then(|r| r.measured.as_ref())
                .and_then(|m| m.pool.as_ref())
                .and_then(|p| p["pool_misses_per_query"].as_f64())
                .unwrap_or(0.0)
        };
        out.push_str(&format!(
            "\nThe win is arithmetic before it is anything else. The scan's definition is {} \
             distance computations of {} lanes each per query — {:.1} MB of f32 touched every \
             time — while the graph's greedy walk scores a few thousand int8 codes and reranks \
             only its shortlist against the f32 sidecars. At {} rows the graph wins on the work \
             it AVOIDS DOING, and it pays for that win in the build rather than the query: see \
             the build column and the page-WAL figures below.\n",
            thousands(ROWS),
            thousands(DIM),
            (ROWS * DIM * 4) as f64 / 1e6,
            thousands(ROWS)
        ));
        let smallest = POOL_BUDGETS[0];
        let largest = POOL_BUDGETS[POOL_BUDGETS.len() - 1];
        let (graph_small, scan_small) = (
            misses("e4-atomic-vamana", smallest),
            misses("e4-atomic-exact", smallest),
        );
        if graph_small >= 0.5 && scan_small < 0.5 {
            out.push_str(&format!(
                "\nThe pool sweep separates the two access patterns exactly as their shapes \
                 predict, and this is the one place read amplification shows up at all. At the \
                 {} budget the GRAPH misses {:.0} page-pool pages per query and the SCAN misses \
                 {:.1} — the scan touches each sidecar once and never returns to it, while the \
                 graph re-references its medoid and its hubs on every query and cannot keep them. \
                 The graph pays {:.0}% more latency for that ({:.2} ms against {:.2} ms at the \
                 {} budget) and still wins the comparison. The scan's own latency is unmoved \
                 across every budget, which is the control that makes the graph's movement \
                 readable.\n",
                mib(smallest),
                graph_small,
                scan_small,
                (at("e4-atomic-vamana", smallest).unwrap_or(0.0)
                    / at("e4-atomic-vamana", largest).unwrap_or(1.0)
                    - 1.0)
                    * 100.0,
                at("e4-atomic-vamana", smallest).unwrap_or(f64::NAN),
                at("e4-atomic-vamana", largest).unwrap_or(f64::NAN),
                mib(largest)
            ));
        } else if graph_small < 0.5 && scan_small < 0.5 {
            out.push_str(
                "\nNo swept budget bound on either arm — zero page-pool misses per query \
                 everywhere — so none of the gap above is read amplification, and the pool column \
                 is a setting that was varied and found not to matter at this scale.\n",
            );
        }
    }
    if !crossed && !out.is_empty() {
        out.push_str(&format!(
            "\nIt does not cross at either budget, and the reason is the size rather than the \
             family. {} rows of {} lanes is a small corpus for an ANN graph: the scan it is \
             competing against reads each sidecar exactly once, in page order, and never returns \
             to it, so the scan's cost is {:.1} MB of sequential reads and {} distance \
             computations and NOTHING ELSE — no pointer chasing, no random pages, no rerank. The \
             graph's advantage over that is READ AMPLIFICATION AVOIDED, and there is not enough \
             of it here to collect: even at the smaller pool the corpus is close enough to \
             resident, and the operating system's own page cache holds the whole file besides \
             (see the deviations — a 64 MiB engine pool on this machine is a POOL miss, not a \
             disk read). What the graph does pay in full is its own shape: a greedy walk over \
             int8 codes, a rerank of the shortlist against the f32 sidecars, and a random page \
             per hop. At a corpus large enough that the scan's sequential reads become real \
             device traffic, that trade reverses; at {} rows it has not begun to.\n",
            thousands(ROWS),
            thousands(DIM),
            (ROWS * DIM * 4) as f64 / 1e6,
            thousands(ROWS),
            thousands(ROWS)
        ));
    }
    out
}

fn write_report(path: &Path, rows: &[ArmRow], meta: &Value, deviations: &[Value]) -> R<()> {
    let (n_rows, n_dim) = (n_rows(), n_dim());
    let mut out = String::new();
    out.push_str("# VECTOR10K — 10,000 rows × 4,096 dimensions, seven arms\n\n");
    out.push_str(&format!(
        "Generated {} on {} · sekejap commit `{}` · {}\n\n",
        meta["generated"].as_str().unwrap_or("?"),
        meta["platform"].as_str().unwrap_or("?"),
        meta["commit"].as_str().unwrap_or("?"),
        meta["pg_version"].as_str().unwrap_or("postgres: not reached")
    ));

    // The two refusals a reader must not miss.
    let pg_refused = rows
        .iter()
        .any(|r| !r.supplementary && r.arm.starts_with("pg-") && r.refusal.is_some());
    let vamana_sql_row = rows.iter().find(|r| r.arm == "e4-sql-vamana");
    let vamana_sql_refused = vamana_sql_row.is_some_and(|r| r.refusal.is_some());
    let vamana_build_refused = vamana_sql_row
        .is_some_and(|r| !r.extra["create_index_refusal"].is_null());
    if pg_refused || vamana_sql_refused {
        out.push_str("## Read this before the table\n\n");
        if pg_refused {
            out.push_str(
                "**pgvector cannot index 4,096 dimensions.** `hnsw` and `ivfflat` are both capped \
                 at 2,000 dimensions for the `vector` type in pgvector 0.8.6 (4,000 for \
                 `halfvec`). Arms 6 and 7 therefore hold the rows and answer correctly, but by \
                 SEQUENTIAL SCAN. Their latency and their 1.00 recall are a scan's, not an \
                 index's, and the two rows measure the same thing twice. The two supplementary \
                 rows at the bottom index a 2,000-lane prefix of the embedding instead, which is \
                 the only ANN route pgvector has at this width, and their recall says what that \
                 truncation costs.\n\n",
            );
        }
        if vamana_build_refused {
            out.push_str(
                "**There is no SQL spelling that builds a vamana index at 4,096 dimensions, and \
                 no SQL statement that would use one if there were.** Arm 4's load is real and so \
                 is its disk figure — the rows went in through `INSERT` — but `CREATE INDEX ... \
                 USING vamana` was refused by the page-WAL's 16 MiB per-transaction ceiling, \
                 because `lang/src/compile/plan.rs:1277` drives every build with \
                 `build_index_to_ready(id, 256)` and 256 vamana nodes of this width do not fit in \
                 one transaction. The atomic caller can choose a smaller transaction and does, \
                 which is why arm 2 has numbers. This arm then built the graph that way as a \
                 LABELLED PROBE and asked the SQL query again: it was refused a second time, now \
                 by the planner, which looks for an exact index and then a quantized one and \
                 names no third family. Two independent blockers, neither of them about speed. \
                 The deviations section gives both by file and line.\n\n",
            );
        } else if vamana_sql_refused {
            out.push_str(
                "**The SQL surface cannot name a vamana index.** Arm 4 builds one through SQL — \
                 its disk and build-time numbers are real — but `SELECT ... ORDER BY emb <=> $1` \
                 is refused, because the vector-order planner considers only the exact and the \
                 quantized families. Arm 2 is the same graph asked through the atomic API. The \
                 deviations section says exactly where.\n\n",
            );
        }
    }

    out.push_str("## The table\n\n");
    out.push_str(TABLE_HEAD);
    for row in rows.iter().filter(|r| !r.supplementary) {
        out.push_str(&table_row(row));
        out.push('\n');
    }
    out.push('\n');
    let supplementary: Vec<&ArmRow> = rows.iter().filter(|r| r.supplementary).collect();
    if !supplementary.is_empty() {
        out.push_str(
            "Supplementary, NOT among the seven arms: a pgvector ANN index over the 2,000-lane \
             prefix `subvector(emb, 1, 2000)`, scored against the true 4,096-lane top-10.\n\n",
        );
        out.push_str(TABLE_HEAD);
        for row in supplementary {
            out.push_str(&table_row(row));
            out.push('\n');
        }
        out.push('\n');
    }

    // Whether any swept budget actually bound is a FACT of the run, so the
    // paragraph that explains the sweep is assembled from the counters rather
    // than from what the sweep was expected to show.
    let bound: Vec<String> = rows
        .iter()
        .filter(|r| r.pool_bytes.is_some() && !r.supplementary)
        .filter_map(|r| {
            let pool = r.measured.as_ref()?.pool.as_ref()?;
            (pool["pool_misses_per_query"].as_f64().unwrap_or(0.0) >= 0.5).then(|| {
                format!("`{}` at {}", r.arm, mib(r.pool_bytes.unwrap_or(0)))
            })
        })
        .collect();
    let budgets: Vec<String> = POOL_BUDGETS.iter().map(|b| mib(*b)).collect();
    out.push_str(&format!(
        "Every sekejap arm appears once per page-pool budget ({}). The sweep asks whether the \
         pool binds differently on a linear scan and on a graph walk: a scan reads each entry \
         once and never returns to it, a graph re-references its entry point and its hubs on \
         every query. SQLite is not swept — it has no graph — and runs at {} throughout, the \
         setting that flatters it; Postgres manages its own `shared_buffers` and is not \
         comparable on this axis at all, so its cell names what the server was set to rather \
         than anything this harness chose.\n\n",
        budgets.join(", "),
        mib(CACHE_BYTES)
    ));
    if bound.is_empty() {
        out.push_str(&format!(
            "**No swept budget bound.** Every sekejap row above recorded ZERO page-pool misses \
             per query, at every budget, so none of their latencies contains a \
             read-amplification component and the differences between an arm's rows are noise. \
             Two things cause that and both are worth stating. A vector query's f32 sidecars are \
             {DIM} lanes of 4 bytes each and do not travel through the pooled page cache the \
             budget bounds — the counters show a whole 10,000-sidecar scan moving only a few \
             hundred pool pages — so the budget was never bounding the expensive part. And the \
             I/O mode is `IoMode::Buffered`, so anything the pool did miss would be served by \
             the operating system's unified buffer cache, which holds a 200 MB store on this \
             machine whatever the engine's own budget says. Read the pool column as a setting \
             that was varied and found not to matter here, not as a memory-pressure axis this \
             run explored.\n\n"
        ));
    } else {
        out.push_str(&format!(
            "The budget BOUND in {} of the swept rows — {} — and those rows' latencies include \
             the cost of re-fetching what the pool could not hold; every other row recorded zero \
             misses per query and its latency contains no read-amplification component at all. \
             Note that `IoMode::Buffered` means a pool miss is served by the operating system's \
             unified buffer cache and not by the device, so even a binding budget here measures \
             the cost of a pool miss rather than the cost of a disk read.\n\n",
            bound.len(),
            bound.join(", ")
        ));
    }

    let crossing = crossover(rows);
    if !crossing.is_empty() {
        out.push_str("## Does the graph ever beat the scan?\n\n");
        out.push_str(&crossing);
        out.push('\n');
    }

    out.push_str(&format!(
        "For scale: the raw vectors alone are {:.1} MB as f32 ({n_rows} × {n_dim} × 4) and {:.1} MB as \
         int8. Every disk figure above should be read against those two.\n\n",
        (ROWS * DIM * 4) as f64 / 1e6,
        (ROWS * DIM) as f64 / 1e6
    ));

    out.push_str("## What each arm actually did\n\n");
    for row in rows {
        out.push_str(&format!(
            "### `{}`{}\n\n{}\n\n",
            row.arm,
            if row.supplementary {
                " (supplementary)"
            } else {
                ""
            },
            row.did
        ));
    }

    out.push_str("## The corpus and the queries\n\n");
    out.push_str(&format!(
        "{n_rows} rows, deterministic from seed `{SEED:#018x}`, one xorshift64 stream. {CENTROIDS} \
         centroids are drawn first, each {n_dim} lanes uniform on [-1, 1). Each row then draws a \
         centroid index uniform over the {CENTROIDS}, a RADIUS log-uniform on \
         [{RADIUS_MIN}, {RADIUS_MAX}], and {n_dim} jitter lanes uniform on [-radius, radius) added to \
         that centroid, followed by an `age` in 18..=79 and a `score` in [0, 1). Keys are \
         `p000000`..`p{:06}` in put order.\n\n\
         The radius is per ROW rather than fixed on purpose. A fixed radius is a single-scale ball, \
         and in {n_dim} dimensions every point on such a ball is very nearly the same distance from \
         the centre as every other — the top-10 becomes arbitrary among the cluster and recall \
         stops measuring anything. A spread of radii gives each cluster a density gradient, which \
         is the structure a real embedding corpus has and the structure a proximity graph \
         navigates.\n\n\
         The {QUERIES} queries come from a second stream seeded `{QUERY_SEED:#018x}` over the SAME \
         centroids under the same radius law, so a query lands inside a cluster rather than in \
         empty space. k = {K}, metric cosine, everywhere.\n\n\
         The recall oracle is brute force in the bench process, in f64 over the f32 lanes it \
         generated: every row scored against every query, sorted by distance then row ordinal, top \
         {K} kept as keys. `recall@10 = |returned ∩ exact| / 10`, averaged over the {QUERIES}.\n\n",
        ROWS - 1
    ));

    out.push_str("## Deviations\n\n");
    for entry in deviations {
        out.push_str(&format!(
            "- **{}** — {}\n",
            entry["subject"].as_str().unwrap_or("?"),
            entry["text"].as_str().unwrap_or("?")
        ));
    }
    out.push('\n');

    fs::write(path, out)?;
    Ok(())
}

fn deviations() -> Vec<Value> {
    vec![
        deviation(
            "e4-sql-vamana is blocked at the BUILD, before the planner ever sees the query",
            "`CREATE INDEX ... USING vamana` is refused at 4,096 dimensions with \
             `Kernel(ResourceLimit(\"page-WAL managed-byte allowance\"))`. \
             `lang/src/compile/plan.rs:1277` drives EVERY `CREATE INDEX` with \
             `build_index_to_ready(id, 256)`, and that chunk is a constant of the compiler, not a \
             parameter of the statement. At this width one 256-row vamana transaction is over the \
             page-WAL's 16 MiB per-transaction ceiling (`WAL_CAP`, \
             core/engine/src/store/pagewal/mod.rs:30, checked at :509), because each inserted node \
             writes a 4,096-byte int8 code and rewrites the neighbour list of every node its \
             robust prune touched, each on its own page. `build_index_to_ready` halves its own \
             GROUP on an allowance refusal but its floor is one chunk, so it cannot rescue a chunk \
             that is itself too large. There is therefore no SQL spelling of this index at this \
             dimension. The atomic caller CAN choose the transaction size, which is what arm 2 \
             does and what this arm's labelled probe did here.",
        ),
        deviation(
            "e4-sql-vamana would have no query number even with the index built",
            "The probe built the same graph through `Database::create_vamana_index` on the arm's \
             own database, drove it to READY, and asked the SQL order again. It was refused: \
             `Compiler::vector_order` (lang/src/compile/select.rs:400-403) looks for a READY \
             ExactVector index and then a READY QuantizedVector index, and nothing else — \
             IndexFamily::VamanaGraph is not among the two. The engine beneath accepts a vamana \
             index for an approximate order (core/engine/src/query/plan.rs:495-501, routed at \
             plan.rs:1673 to DriverPlan::VamanaVector), so this is a missing spelling rather than a \
             missing capability. The arm's own disk figure is measured BEFORE the probe, over the \
             store SQL alone produced; the probe's own bytes and seconds are reported separately \
             in `extra`. This binary does not patch `lang` to make either number appear — the \
             brief forbids touching that layer, and a benchmark that edits the thing it measures \
             is not a measurement.",
        ),
        deviation(
            "what actually fills the transaction is the SEARCH, not the rows",
            "A vamana insert is not a row write. It is a greedy SEARCH — up to \
             BUILD_SEARCH_LIST * 2 = 200 nodes visited (core/engine/src/index/vector/graph.rs) — \
             followed by back-edge repair, where RobustPrune rewrites the neighbour list of up to \
             DEGREE = 48 existing nodes, each on its own page scattered across the 0x7D \
             keyspace. The transaction's page footprint therefore scales with the search and the \
             degree, not with the row count, which is why halving the batch moves it so little \
             and why the admitted size lands so far below the 256 this crate's other benches use. \
             `extra.build.page_wal_bytes_per_row` measures it rather than asserting it: it is the \
             page-WAL bytes the successful pass appended divided by the rows it built, and \
             `rows_that_would_fill_one_transaction` beside it is 16 MiB over that figure. \
             THERE IS NO KNOB THAT RAISES THE CEILING. `WAL_CAP` \
             (core/engine/src/store/pagewal/mod.rs:30) is a constant 16 MiB and `wal_allowance()` \
             at :1244 is `WAL_CAP.min(limits.1)`, so `set_runtime_limits` can only lower it. This \
             bench did not raise a limit to make the build succeed, because it could not have, \
             and the number it reports is the one the shipped default produces.",
        ),
        deviation(
            "the vamana build transaction size is itself a capacity number",
            "The atomic arms drive the build through a helper that halves the chunk on an \
             allowance refusal and reports the size that was admitted; at 4,096 dimensions that is \
             far below the 256 rows this crate's other benches use, and the JSON's \
             `extra.build.rows_per_transaction` carries it per arm. A handle is POISONED by any \
             error it returns (`Database::finish`, core/engine/src/collections/mod.rs:1385), so \
             the retry drops the handle and reopens the directory rather than calling again — \
             legal precisely because the BUILDING descriptor and its cursor are committed and the \
             build resumes from them. `extra.build.seconds` is the successful pass alone; \
             `chunk_search_seconds` is what the halving cost on top, reported rather than folded \
             in.",
        ),
        deviation(
            "pgvector refuses both ANN families at this dimension",
            "hnsw and ivfflat are capped at 2,000 dimensions for the `vector` type in pgvector \
             0.8.6 (4,000 for `halfvec`), and this column is 4,096. Arms 6 and 7 therefore measure \
             the same Postgres sequential scan twice and differ only by noise. Both rows are kept \
             rather than omitted, per the brief.",
        ),
        deviation(
            "four sekejap directories, two Postgres tables",
            "Disk is asked per arm, and two indexes in one store cannot be attributed to one arm \
             each. The 163.8 MB of f32 sidecars is therefore paid four times on the volume, and \
             `disk MB` means what it says.",
        ),
        deviation(
            "atomic arms call the index atomic; SQL arms pay the page machinery",
            "Arms 1 and 2 call `query_exact_vector` / `query_vamana_vector` and read a \
             `Vec<VectorHit>` carrying an `EntityId`; the key comes back through an id-to-key \
             vector this process holds (battle50k deviation 2). Arms 3 and 4 ask for `_key` and pay \
             for the projection, the page assembly and the compile. The gap between arm 1 and arm 3 \
             is what the SQL surface costs, which is one of the things this table is for.",
        ),
        deviation(
            "a SQL query is compiled every time it is asked",
            "The headline median for arms 3 and 4 is `prepare_sql` plus the walk, because that is \
             what a caller issuing a statement pays. `extra.prepare_median_ms` in the JSON is the \
             compile measured on its own, so a reader who prepares once and rebinds can subtract \
             it.",
        ),
        deviation(
            "every sekejap arm is measured at TWO page-pool budgets; SQLite and Postgres at one",
            "64 MiB does not hold the 163.8 MB of f32 sidecars; 256 MiB holds them and the rest \
             of the store. Both rows appear for each of the four sekejap arms, labelled. The \
             sweep exists because the two index families answer it differently: a LINEAR scan \
             touches each entry once and never comes back, so a small pool costs it almost \
             nothing, while a GRAPH walk re-references its medoid and its hub nodes on every \
             query, so a pool too small to hold that hot structure makes it fetch them again and \
             again. SQLite is not swept because it has no graph; it runs at `PRAGMA cache_size = \
             -262144` (256 MiB) throughout, the setting that flatters it. Postgres manages its \
             own buffer pool and is NOT comparable on this axis: its cell names the server's \
             `shared_buffers` rather than anything this harness chose, and the value is in \
             `meta.pg_shared_buffers`.",
        ),
        deviation(
            "a 64 MiB pool on this machine is a POOL miss, not a disk read",
            "The I/O mode is `IoMode::Buffered`, so every page the engine pool misses is served \
             by the operating system's own unified buffer cache, and a 200 MB store on a machine \
             with far more RAM than that stays resident in it for the whole run. The small-pool \
             rows therefore measure what a pool miss costs — the lookup, the copy and the \
             eviction sweep — and NOT what a device read costs. That distinction cuts against \
             the graph here rather than for it: the read amplification a graph exists to avoid \
             is device traffic, and there is none to avoid. `page_pool.pool_misses_per_query` in \
             the JSON is the honest measure of how much work the budget actually moved; a run \
             that wanted device reads would need a corpus larger than this machine's RAM, which \
             is a different bench.",
        ),
        deviation(
            "each sekejap store is BUILT once and read back at both budgets",
            "`BUILD_BUDGET` is 256 MiB for all four. A store is the same bytes whatever pool read \
             it back, so `disk MB` is a single measurement, and the load and index-build cells \
             are repeated across an arm's two rows with `extra.built_at_pool` naming the budget \
             they were produced under. They are NOT two independent measurements and should not \
             be read as one: what differs between an arm's two rows is the QUERY pass alone, \
             which is the pass the budget was swept for. Building each store twice would have \
             doubled a run whose two vamana builds already dominate it, to fill a cell that \
             answers no question that was asked.",
        ),
        deviation(
            "durability matched at the fsync class, not by name",
            "sekejap runs SyncMode::Normal (`sync_data`), SQLite `journal_mode = DELETE` with \
             `synchronous = FULL`, Postgres `synchronous_commit = on`. All three are an ordinary \
             fsync per commit on this volume. SyncMode::Full would be F_FULLFSYNC, a drive-cache \
             barrier no other arm pays.",
        ),
        deviation(
            "load cadence 256 rows embedded, 64 rows Postgres",
            "A 4,096-lane vector reaches the server as roughly 50 KB of text (`$n::text::vector`, \
             the spelling battle50k uses because the `postgres` crate has no binary codec for \
             pgvector's type), so a 256-row statement would be a 13 MB query string. 64 keeps it \
             near 3 MB.",
        ),
        deviation(
            "the SQLite arm scores in a registered Rust function",
            "`cosine_distance(blob, blob)` is the same `1 - cos` the other arms rank by, over two \
             little-endian f32 BLOBs. Expressing 4,096 lanes of dot product in SQLite's own SQL \
             would measure its expression interpreter rather than its ability to hold and scan the \
             data. The scan it drives is still SQLite's, and the arm is labelled a scan everywhere \
             it appears.",
        ),
        deviation(
            "the Postgres knobs are session settings, not SET LOCAL",
            "An ANN index answers in about a millisecond, and wrapping every measured statement \
             in BEGIN / SET LOCAL / SET LOCAL / ROLLBACK to scope its knobs would add four round \
             trips to a one-round-trip statement — the harness would be measuring its own \
             protocol traffic. `hnsw.ef_search` / `ivfflat.probes` and `enable_seqscan = off` are \
             therefore applied once per arm as session settings and cleared with `RESET ALL` when \
             the arm ends, so the measured statement is one round trip everywhere and no arm \
             inherits another's knobs. `enable_seqscan = off` is set only for an arm whose index \
             actually built: an arm with no index must not be pushed off the only plan it has.",
        ),
        deviation(
            "warm pass before the timed pass",
            "Each arm runs one untimed pass over all 100 queries before the timed one. The \
             reported median is over the 100 timed wall times; p90 and mean are beside it in the \
             JSON.",
        ),
        deviation(
            "10,000 rows is small for an ANN graph",
            "At this size the whole vamana graph and every int8 code sit in the page pool, and a \
             graph's advantage over a linear scan is read amplification it never gets to collect. \
             Whatever the vamana row says relative to the exact row here, it is a statement about \
             10,000 rows and not about the family.",
        ),
    ]
}

// ── options ───────────────────────────────────────────────────────────────

struct Options {
    /// Rebuild the MARKDOWN from a `vector10k.json` an earlier run wrote,
    /// measuring nothing. The prose around the table is derived from the
    /// numbers, so a correction to how a result is EXPLAINED must not need
    /// a fresh 45-minute run to publish — and re-running to fix a sentence
    /// would silently replace the numbers the sentence is about.
    rewrite_report: Option<PathBuf>,
    out_dir: PathBuf,
    work_dir: PathBuf,
    dsn: String,
    ef: usize,
    only: Option<Vec<String>>,
    keep: bool,
}

fn usage() -> String {
    "vector10k [--out-dir DIR] [--work DIR] [--dsn DSN] [--ef N] [--only arm[,arm...]] [--keep]\n\
     vector10k --rewrite-report <vector10k.json> [--out-dir DIR]   (no measurement)"
        .to_owned()
}

fn parse(args: &[String]) -> R<Options> {
    let mut options = Options {
        rewrite_report: None,
        out_dir: default_out_dir(),
        work_dir: default_work_dir(),
        dsn: default_dsn(),
        ef: DEFAULT_EF,
        only: None,
        keep: false,
    };
    let mut at = 0;
    while at < args.len() {
        let flag = args[at].clone();
        let value = |at: &mut usize| -> R<String> {
            *at += 1;
            args.get(*at)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value\n{}", usage()).into())
        };
        match flag.as_str() {
            "--rewrite-report" => options.rewrite_report = Some(PathBuf::from(value(&mut at)?)),
            "--out-dir" => options.out_dir = PathBuf::from(value(&mut at)?),
            "--work" => options.work_dir = PathBuf::from(value(&mut at)?),
            "--dsn" => options.dsn = value(&mut at)?,
            "--ef" => options.ef = value(&mut at)?.parse()?,
            "--only" => {
                options.only =
                    Some(value(&mut at)?.split(',').map(|s| s.trim().to_owned()).collect())
            }
            "--keep" => options.keep = true,
            other => return Err(format!("unknown flag {other}\n{}", usage()).into()),
        }
        at += 1;
    }
    // An `--only` that names nothing is a silent empty run, which reads like
    // a measurement of zero arms rather than a typo. It is refused by name.
    if let Some(list) = &options.only {
        for name in list {
            if !ARMS.contains(&name.as_str()) && !SUPPLEMENTARY.contains(&name.as_str()) {
                return Err(format!(
                    "--only {name}: the arms are {}, and the supplementary rows are {}",
                    ARMS.join(", "),
                    SUPPLEMENTARY.join(", ")
                )
                .into());
            }
        }
    }
    Ok(options)
}

fn wanted(options: &Options, arm: &str) -> bool {
    match &options.only {
        None => true,
        Some(list) => list.iter().any(|name| name == arm),
    }
}

// ── main ──────────────────────────────────────────────────────────────────

fn main() {
    if let Err(error) = run() {
        eprintln!("vector10k: {error}");
        std::process::exit(1);
    }
}

fn run() -> R<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let options = parse(&args)?;
    fs::create_dir_all(&options.out_dir)?;
    if let Some(source) = &options.rewrite_report {
        let raw: Value = serde_json::from_str(&fs::read_to_string(source)?)?;
        let rows: Vec<ArmRow> = raw["arms"]
            .as_array()
            .ok_or("that file has no `arms` array")?
            .iter()
            .filter_map(ArmRow::from_json)
            .collect();
        if rows.is_empty() {
            return Err(format!("{} carried no readable arms", source.display()).into());
        }
        // The deviations are this binary's own prose and are regenerated;
        // the meta and every number come from the file, untouched.
        let deviations = deviations();
        let report = options.out_dir.join("vector10k-report.md");
        write_report(&report, &rows, &raw["meta"], &deviations)?;
        let raw_out = options.out_dir.join("vector10k.json");
        fs::write(
            &raw_out,
            serde_json::to_string_pretty(&json!({
                "meta": raw["meta"],
                "arms": raw["arms"],
                "deviations": deviations,
                "exactness_complaints": raw["exactness_complaints"],
            }))? + "\n",
        )?;
        println!("rewrote {} from {}", report.display(), source.display());
        println!("rewrote {}", raw_out.display());
        for row in &rows {
            println!("{}", table_row(row));
        }
        return Ok(());
    }
    fs::create_dir_all(&options.work_dir)?;

    eprintln!("[corpus] generating {ROWS} rows × {DIM} lanes from seed {SEED:#018x} …");
    let at = Instant::now();
    let corpus = corpus();
    let queries = query_set();
    let corpus_seconds = at.elapsed().as_secs_f64();
    // The cluster sizes are reported rather than assumed: the centroid index
    // is drawn, not round-robined, so the 50 clusters are uneven and how
    // uneven is part of what the recall numbers below mean.
    let mut cluster_sizes = vec![0usize; CENTROIDS];
    for cluster in &corpus.cluster_of {
        cluster_sizes[*cluster] += 1;
    }
    let smallest = cluster_sizes.iter().copied().min().unwrap_or(0);
    let largest = cluster_sizes.iter().copied().max().unwrap_or(0);
    eprintln!(
        "[corpus] {corpus_seconds:.2} s; {CENTROIDS} clusters, smallest {smallest} rows, largest {largest}"
    );

    eprintln!("[oracle] brute force, {QUERIES} queries × {ROWS} rows × {DIM} lanes …");
    let at = Instant::now();
    let truth = brute_force(&corpus, &queries);
    let oracle_seconds = at.elapsed().as_secs_f64();
    eprintln!("[oracle] {oracle_seconds:.2} s");

    let mut rows: Vec<ArmRow> = Vec::new();

    if wanted(&options, "e4-atomic-exact") {
        rows.extend(arm_e4_atomic_exact(
            &options.work_dir.join("e4-atomic-exact"),
            &corpus,
            &queries,
            &truth,
        )?);
    }
    if wanted(&options, "e4-atomic-vamana") {
        rows.extend(arm_e4_atomic_vamana(
            &options.work_dir.join("e4-atomic-vamana"),
            &corpus,
            &queries,
            &truth,
            options.ef,
        )?);
    }
    if wanted(&options, "e4-sql-exact") {
        rows.extend(arm_e4_sql_exact(
            &options.work_dir.join("e4-sql-exact"),
            &corpus,
            &queries,
            &truth,
        )?);
    }
    if wanted(&options, "e4-sql-vamana") {
        rows.extend(arm_e4_sql_vamana(
            &options.work_dir.join("e4-sql-vamana"),
            &corpus,
            &queries,
            &truth,
            options.ef,
        )?);
    }
    if wanted(&options, "sqlite") {
        rows.push(arm_sqlite(
            &options.work_dir.join("sqlite.db"),
            &corpus,
            &queries,
            &truth,
        )?);
    }

    // Postgres last, so a server that cannot be reached still leaves five
    // arms measured rather than none.
    let mut pg_version = "postgres: not reached".to_owned();
    let mut pg_shared_buffers = Value::Null;
    let mut pg_vector_version = Value::Null;
    let specs = pg_specs();
    let any_pg = specs.iter().any(|spec| wanted(&options, spec.arm));
    if any_pg {
        match Client::connect(&options.dsn, NoTls) {
            Ok(mut client) => {
                pg_version = client
                    .query_one("SELECT version()", &[])
                    .map(|r| r.get::<_, String>(0))
                    .unwrap_or_else(|e| format!("version() failed: {e}"));
                pg_shared_buffers = client
                    .query_one("SHOW shared_buffers", &[])
                    .map(|r| Value::String(r.get::<_, String>(0)))
                    .unwrap_or(Value::Null);
                client.batch_execute("CREATE EXTENSION IF NOT EXISTS vector;")?;
                pg_vector_version = client
                    .query_one(
                        "SELECT extversion FROM pg_extension WHERE extname = 'vector'",
                        &[],
                    )
                    .map(|r| Value::String(r.get::<_, String>(0)))
                    .unwrap_or(Value::Null);
                // The arms name the server the way the server named itself,
                // so a prose claim about a version cannot drift from the one
                // that actually answered.
                let server_label = format!(
                    "{} with pgvector {}",
                    pg_version
                        .split(" on ")
                        .next()
                        .unwrap_or(pg_version.as_str())
                        .trim(),
                    pg_vector_version.as_str().unwrap_or("of unknown version")
                );
                for spec in &specs {
                    if !wanted(&options, spec.arm) {
                        continue;
                    }
                    let buffers = pg_shared_buffers.as_str().unwrap_or("unknown").to_owned();
                    let mut row =
                        arm_pg(&mut client, spec, &corpus, &queries, &truth, &buffers)?;
                    row.did = pg_did(spec, &row, &server_label);
                    rows.push(row);
                }
                if !options.keep {
                    for spec in &specs {
                        let _ =
                            client.batch_execute(&format!("DROP TABLE IF EXISTS {};", spec.table));
                    }
                }
            }
            Err(error) => {
                eprintln!(
                    "vector10k: POSTGRES COULD NOT BE REACHED at {}: {error}\n\
                     arms 6 and 7 are reported as unreachable; the other five are real.",
                    options.dsn
                );
                for spec in &specs {
                    if !wanted(&options, spec.arm) {
                        continue;
                    }
                    let mut row = ArmRow::new(spec.arm);
                    row.supplementary = spec.supplementary;
                    row.pool_note = "n/a".to_owned();
                    row.refusal = Some(format!("postgres unreachable at {}: {error}", options.dsn));
                    row.did = format!(
                        "NOT RUN. The server at `{}` could not be reached: {error}. No number in \
                         this row; nothing was estimated or carried over from another run.",
                        options.dsn
                    );
                    rows.push(row);
                }
            }
        }
    }

    // Arms that are exact by construction must score 1.00. A number below
    // that is a bug worth reporting, not a result worth publishing.
    let mut exactness = Vec::new();
    for row in &rows {
        let exact_by_construction = matches!(
            row.arm.as_str(),
            "e4-atomic-exact" | "e4-sql-exact" | "sqlite"
        ) || (row.arm.starts_with("pg-") && !row.supplementary && row.refusal.is_some());
        if !exact_by_construction {
            continue;
        }
        let Some(measured) = &row.measured else {
            continue;
        };
        if (measured.recall - 1.0).abs() > 1e-12 {
            let complaint = format!(
                "{}: recall@10 is {:.4} but this arm is exact by construction — a bug, not a \
                 result",
                row.arm, measured.recall
            );
            eprintln!("vector10k: {complaint}");
            exactness.push(Value::String(complaint));
        }
    }

    let meta = json!({
        "generated": chrono_like_now(),
        "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "commit": git_commit(Path::new(".")),
        "rows": ROWS,
        "dimension": DIM,
        "k": K,
        "queries": QUERIES,
        "centroids": CENTROIDS,
        "cluster_rows_smallest": smallest,
        "cluster_rows_largest": largest,
        "corpus_seconds": corpus_seconds,
        "seed": format!("{SEED:#018x}"),
        "query_seed": format!("{QUERY_SEED:#018x}"),
        "radius_min": RADIUS_MIN,
        "radius_max": RADIUS_MAX,
        "vamana_ef": options.ef,
        "pool_budgets_bytes": POOL_BUDGETS,
        "build_budget_bytes": BUILD_BUDGET,
        "sqlite_cache_bytes": CACHE_BYTES,
        "batch_rows": BATCH,
        "pg_batch_rows": PG_BATCH,
        "raw_f32_bytes": ROWS * DIM * 4,
        "raw_int8_bytes": ROWS * DIM,
        "oracle_seconds": oracle_seconds,
        "pg_version": pg_version,
        "pg_shared_buffers": pg_shared_buffers,
        "pgvector_version": pg_vector_version,
        "dsn": options.dsn,
        "work_dir": options.work_dir.display().to_string(),
    });
    let deviations = deviations();

    let report = options.out_dir.join("vector10k-report.md");
    write_report(&report, &rows, &meta, &deviations)?;
    let raw = options.out_dir.join("vector10k.json");
    fs::write(
        &raw,
        serde_json::to_string_pretty(&json!({
            "meta": meta,
            "arms": rows.iter().map(ArmRow::to_json).collect::<Vec<Value>>(),
            "deviations": deviations,
            "exactness_complaints": exactness,
        }))? + "\n",
    )?;

    println!("wrote {}", report.display());
    println!("wrote {}", raw.display());
    for row in &rows {
        println!("{}", table_row(row));
    }
    if !options.keep {
        let _ = fs::remove_dir_all(&options.work_dir);
    }
    Ok(())
}

/// The wall clock as ISO-ish text, without taking a date dependency this
/// crate does not have: seconds since the epoch, and the `date` the shell
/// would print, asked of the system once.
fn chrono_like_now() -> String {
    std::process::Command::new("date")
        .arg("-u")
        .arg("+%Y-%m-%dT%H:%M:%SZ")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

