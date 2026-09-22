//! `ORDER BY <vector column> <=> $1` answered through SQL by a VAMANA GRAPH
//! index, and `CREATE INDEX ... USING vamana` building one at a width no
//! other index in the comparison reaches.
//!
//! WHAT WAS MISSING. The engine has answered an approximate vector order
//! from either the quantized family or the vamana graph since the graph
//! landed (`core/engine/src/query/plan.rs::prepare_approximate_vector`), but
//! the SQL compiler looked for an exact index, then a quantized one, and
//! named no third family -- so a graph built through SQL could be built and
//! then never used, and the order was refused with "no vector index". That
//! is a missing SPELLING, not a missing capability, and this file is what
//! says it is spelled now.
//!
//! THE ORACLE IS HELD IN THIS PROCESS. Every vector is generated here by a
//! deterministic generator and kept in a `Vec`; the true nearest neighbour of
//! each probe is computed here from those f32 lanes under the cosine
//! distance written out below. No assertion compares one engine call against
//! another.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{Database, IndexFamily, IndexState};
use sekejap_lang::{Param, SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

/// The narrow case: a whole node fits well inside a page.
const NARROW: usize = 8;
/// The wide case, and the one that matters. 4,096 lanes is the owner's real
/// width; pgvector indexes neither of its families past 2,000.
const WIDE: usize = 4_096;

fn config() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// DEFAULT resource limits: no policy is installed, so the only bound on a
/// build transaction is the page-WAL's own fixed 16 MiB allowance.
fn open() -> (TempDir, Database) {
    let dir = TempDir::new().unwrap();
    let db = Database::create(dir.path().join("db"), config()).unwrap();
    (dir, db)
}

fn run(db: &mut Database, text: &str) -> SqlResult {
    db.sql(text, &[])
        .unwrap_or_else(|e| panic!("`{text}` was refused: {e:?}"))
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 11) as f32 / (1u64 << 53) as f32
    }
    fn vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.unit() * 2.0 - 1.0).collect()
    }
}

fn corpus(rows: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    let centres: Vec<Vec<f32>> = (0..16).map(|_| rng.vector(dim)).collect();
    (0..rows)
        .map(|_| {
            let centre = &centres[(rng.next() % 16) as usize];
            centre
                .iter()
                .map(|lane| lane + (rng.unit() - 0.5) * 0.2)
                .collect()
        })
        .collect()
}

/// Cosine distance, written out here from the definition in
/// `core/engine/src/index/vector/quant.rs` rather than called.
fn cosine(stored: &[f32], query: &[f32]) -> f64 {
    let wide = |v: &[f32]| v.iter().map(|l| f64::from(*l)).collect::<Vec<f64>>();
    let (s, q) = (wide(stored), wide(query));
    let dot: f64 = s.iter().zip(&q).map(|(a, b)| a * b).sum();
    let sn: f64 = s.iter().map(|a| a * a).sum::<f64>().sqrt();
    let qn: f64 = q.iter().map(|b| b * b).sum::<f64>().sqrt();
    1.0 - dot / (sn * qn)
}

/// Brute force, here: the `k` nearest keys of the corpus this process made.
fn truth(vectors: &[Vec<f32>], query: &[f32], k: usize) -> Vec<String> {
    let mut scored: Vec<(f64, usize)> = vectors
        .iter()
        .enumerate()
        .map(|(at, stored)| (cosine(stored, query), at))
        .collect();
    scored.sort_by(|l, r| l.0.total_cmp(&r.0).then_with(|| l.1.cmp(&r.1)));
    scored
        .into_iter()
        .take(k)
        .map(|(_, at)| format!("k{at}"))
        .collect()
}

fn keys(db: &mut Database, text: &str, params: &[Param]) -> Vec<String> {
    match db
        .sql(text, params)
        .unwrap_or_else(|e| panic!("`{text}` was refused: {e:?}"))
    {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| match &row.values[0] {
                SqlValue::Text(t) => t.clone(),
                other => panic!("column 0 is {other:?}, not text"),
            })
            .collect(),
        other => panic!("`{text}` answered {other:?} rather than rows"),
    }
}

fn explain(db: &mut Database, text: &str) -> String {
    match run(db, &format!("EXPLAIN {text}")) {
        SqlResult::Explain(plan) => plan,
        other => panic!("EXPLAIN answered {other:?}"),
    }
}

fn literal(vector: &[f32]) -> String {
    let lanes: Vec<String> = vector.iter().map(|lane| format!("{lane}")).collect();
    format!("[{}]", lanes.join(","))
}

/// Create the table, write the rows and build the graph, all through SQL.
fn load(db: &mut Database, dim: usize, vectors: &[Vec<f32>]) {
    run(db, &format!("CREATE TABLE vec (emb VECTOR({dim}))"));
    for (at, vector) in vectors.iter().enumerate() {
        db.sql(
            "INSERT INTO vec (_key, emb) VALUES ($1, $2)",
            &[Param::Text(format!("k{at}")), Param::Vector(vector.clone())],
        )
        .unwrap();
    }
    run(db, "COMMIT");
    // The statement under test on the write side: the build runs to READY
    // inside this one statement, in transactions the compiler sizes.
    run(db, "CREATE INDEX vec_emb_vamana ON vec USING vamana (emb)");
}

fn family_of(db: &Database, name: &str) -> (IndexFamily, IndexState) {
    let c = db.collection("vec").unwrap().expect("the collection");
    let info = db
        .list_indexes(c)
        .unwrap()
        .into_iter()
        .find(|info| info.name == name)
        .expect("the index");
    (info.family, info.state)
}

// ── the tests ─────────────────────────────────────────────────────────────

/// The SQL half, at a narrow vector: the graph is built by `CREATE INDEX`,
/// the order is ANSWERED rather than refused, the answer is the brute-force
/// nearest neighbour this process computed, and the notice says which family
/// answered and that the answer is approximate.
#[test]
fn a_vector_order_is_answered_by_a_vamana_index_built_through_sql() {
    let (_dir, mut db) = open();
    let vectors = corpus(600, NARROW, 0x11a2_b3c4_d5e6_f708);
    load(&mut db, NARROW, &vectors);
    assert_eq!(
        family_of(&db, "vec_emb_vamana"),
        (IndexFamily::VamanaGraph, IndexState::Ready),
        "CREATE INDEX ... USING vamana must leave a READY graph"
    );
    let probes = corpus(12, NARROW, 0x9f8e_7d6c_5b4a_3928);
    for probe in &probes {
        let got = keys(
            &mut db,
            "SELECT _key FROM vec ORDER BY emb <=> $1::vector LIMIT 1",
            &[Param::Vector(probe.clone())],
        );
        assert_eq!(got, truth(&vectors, probe, 1), "the graph answered the wrong row");
    }
    // The same statement with the vector written as pgvector's text literal,
    // because that is how a caller who is porting writes it.
    let probe = &probes[0];
    let got = keys(
        &mut db,
        &format!(
            "SELECT _key FROM vec ORDER BY emb <=> '{}'::vector LIMIT 3",
            literal(probe)
        ),
        &[],
    );
    assert_eq!(got, truth(&vectors, probe, 3));

    // What the caller is told: approximate, and by which family.
    let plan = explain(
        &mut db,
        &format!(
            "SELECT _key FROM vec ORDER BY emb <=> '{}'::vector LIMIT 3",
            literal(probe)
        ),
    );
    assert!(
        plan.contains("APPROXIMATE") && plan.contains("vamana graph index"),
        "the plan must say the answer is approximate and name the graph: {plan}"
    );
}

/// The SQL half of the ACCEPTANCE, at the width that broke: the same
/// statements at 4,096 lanes, where a node's codes are larger than a page.
/// Before the adjacency moved into its own keyspace the `CREATE INDEX` here
/// could not complete at all -- every build transaction was refused by the
/// page-WAL's managed-byte allowance -- and the order that follows it had no
/// spelling in the compiler.
#[test]
fn a_four_thousand_lane_graph_builds_and_answers_through_sql_inside_the_default_limits() {
    let (_dir, mut db) = open();
    let vectors = corpus(400, WIDE, 0x22b3_c4d5_e6f7_0819);
    load(&mut db, WIDE, &vectors);
    assert_eq!(
        family_of(&db, "vec_emb_vamana"),
        (IndexFamily::VamanaGraph, IndexState::Ready)
    );
    let probes = corpus(4, WIDE, 0x8e7d_6c5b_4a39_2817);
    for probe in &probes {
        let got = keys(
            &mut db,
            "SELECT _key FROM vec ORDER BY emb <=> $1::vector LIMIT 1",
            &[Param::Vector(probe.clone())],
        );
        assert_eq!(got, truth(&vectors, probe, 1), "the graph answered the wrong row");
    }
    // And every row of the corpus is its own nearest neighbour, which is the
    // claim that says the whole corpus is in the graph and not part of it.
    for at in (0..vectors.len()).step_by(40) {
        let got = keys(
            &mut db,
            "SELECT _key FROM vec ORDER BY emb <=> $1::vector LIMIT 1",
            &[Param::Vector(vectors[at].clone())],
        );
        assert_eq!(got, vec![format!("k{at}")], "row {at} is not its own nearest");
    }
}

/// A column with BOTH approximate families answers from the GRAPH, and the
/// notice says so. The two mean the same thing -- an `ef`-bounded shortlist
/// of int8 candidates, reranked exactly -- and differ in what they cost, so
/// the one a caller had to ask for by name is the one that answers.
#[test]
fn a_column_with_both_approximate_families_is_answered_by_the_graph() {
    let (_dir, mut db) = open();
    let vectors = corpus(400, NARROW, 0x33c4_d5e6_f708_192a);
    load(&mut db, NARROW, &vectors);
    run(&mut db, "CREATE INDEX vec_emb_quantized ON vec USING quantized (emb)");
    let probe = corpus(1, NARROW, 0x7d6c_5b4a_3928_1706).remove(0);
    let statement = format!(
        "SELECT _key FROM vec ORDER BY emb <=> '{}'::vector LIMIT 5",
        literal(&probe)
    );
    let plan = explain(&mut db, &statement);
    assert!(
        plan.contains("vamana graph index") && !plan.contains("quantized index"),
        "with both families present the graph answers: {plan}"
    );
    assert_eq!(keys(&mut db, &statement, &[]), truth(&vectors, &probe, 5));
}

/// An EXACT index still wins when the caller has not asked for a shortlist
/// bound, and `SET LOCAL ef_search` is still what turns the order
/// approximate. The graph joins that rule; it does not change it.
#[test]
fn an_exact_index_still_answers_until_ef_search_asks_for_a_shortlist() {
    let (_dir, mut db) = open();
    let vectors = corpus(300, NARROW, 0x44d5_e6f7_0819_2a3b);
    load(&mut db, NARROW, &vectors);
    run(&mut db, "CREATE INDEX vec_emb_exact ON vec USING exact (emb)");
    let probe = corpus(1, NARROW, 0x6c5b_4a39_2817_0645).remove(0);
    let statement = format!(
        "SELECT _key FROM vec ORDER BY emb <=> '{}'::vector LIMIT 5",
        literal(&probe)
    );
    let exact_plan = explain(&mut db, &statement);
    assert!(
        !exact_plan.contains("APPROXIMATE"),
        "with no ef_search the exact index answers: {exact_plan}"
    );
    assert_eq!(keys(&mut db, &statement, &[]), truth(&vectors, &probe, 5));
    run(&mut db, "SET LOCAL ef_search = 120");
    let graph_plan = explain(&mut db, &statement);
    assert!(
        graph_plan.contains("APPROXIMATE") && graph_plan.contains("vamana graph index"),
        "ef_search names the shortlist, and the graph is what bounds it: {graph_plan}"
    );
    assert_eq!(keys(&mut db, &statement, &[]), truth(&vectors, &probe, 5));
}
