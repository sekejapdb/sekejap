//! What ONE vamana insert dirties, at a dimension whose codes exceed a page.
//!
//! The question this answers is not recall and not latency. It is the number
//! the page-WAL enforces: how many bytes of WAL one transaction that links a
//! node into the graph appends. A node's codes at 4,096 lanes are 4,096
//! bytes, larger than the 4,096-byte page, so the layout decides whether an
//! edge append rewrites a few hundred bytes or a whole overflow chain, and a
//! build either fits the 16 MiB allowance or is refused by it.
//!
//! ```sh
//! cargo run -p sekejap-bench --release --bin vamana_lanes -- ROWS DIM BATCH [sql]
//! ```
//!
//! With `sql` the same corpus is loaded, indexed and QUERIED through the SQL
//! surface instead -- `CREATE INDEX ... USING vamana` drives the build and
//! `ORDER BY emb <=> $1` has to be answered by it -- so the two halves of the
//! acceptance are measured by one binary.
//!
//! Prints one JSON object: whether the build completed, its wall time, the
//! WAL bytes the whole build appended, and the WAL bytes ONE further live
//! insert appended on top of a ready graph.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{CollectionOptions, Database, IndexFamily, IndexState},
    Kind,
};
use sekejap_lang::{Param, SqlDatabase, SqlResult, SqlValue};
use serde_json::json;
use std::{env, time::Instant};

fn cfg() -> Config {
    Config {
        budget_bytes: 32 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
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

/// Clustered rows: 64 centres with a little noise, the shape an embedding
/// corpus has and the one the family's other measurements use.
fn corpus(rows: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    let centres: Vec<Vec<f32>> = (0..64).map(|_| rng.vector(dim)).collect();
    (0..rows)
        .map(|_| {
            let centre = &centres[(rng.next() % 64) as usize];
            centre
                .iter()
                .map(|lane| lane + (rng.unit() - 0.5) * 0.2)
                .collect()
        })
        .collect()
}

/// The SQL half: the same corpus through `CREATE TABLE`, `INSERT`,
/// `CREATE INDEX ... USING vamana` and `ORDER BY emb <=> $1`, with the
/// nearest neighbour of a row checked against the row itself.
fn sql_half(rows: usize, dim: usize, vectors: &[Vec<f32>]) {
    let dir = std::env::temp_dir().join(format!(
        "vamana-lanes-sql-{rows}-{dim}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut db = Database::create(&dir, cfg()).unwrap();
    db.sql(&format!("CREATE TABLE vec (emb VECTOR({dim}))"), &[])
        .unwrap();
    for (at, vector) in vectors.iter().take(rows).enumerate() {
        db.sql(
            "INSERT INTO vec (_key, emb) VALUES ($1, $2)",
            &[Param::Text(format!("k{at}")), Param::Vector(vector.clone())],
        )
        .unwrap();
        if at % 256 == 255 {
            db.sql("COMMIT", &[]).unwrap();
        }
    }
    db.sql("COMMIT", &[]).unwrap();
    let start = Instant::now();
    db.sql("CREATE INDEX vec_emb_vamana ON vec USING vamana (emb)", &[])
        .unwrap();
    let build_seconds = start.elapsed().as_secs_f64();
    let collection = db.collection("vec").unwrap().unwrap();
    let info = db
        .list_indexes(collection)
        .unwrap()
        .into_iter()
        .find(|info| info.name == "vec_emb_vamana")
        .expect("the index");
    assert_eq!(info.family, IndexFamily::VamanaGraph);
    assert_eq!(info.state, IndexState::Ready);
    // The oracle is each probe row itself: its own exact nearest neighbour is
    // itself, at distance 0. The index is APPROXIMATE, so a miss is a recall
    // number and not a failure -- what would be a failure is an order that
    // does not answer at all, which is what this arm exists to prove cannot
    // happen any more. The search list is the knob, so both ends are
    // measured: the compiler's default and one the caller names.
    let mut rows_out = Vec::new();
    for ef in [None, Some(400usize)] {
        if let Some(ef) = ef {
            db.sql(&format!("SET LOCAL ef_search = {ef}"), &[]).unwrap();
        }
        let start = Instant::now();
        let mut answered = 0usize;
        let mut exact = 0usize;
        let probes = 20usize;
        for at in (0..rows).step_by(rows / probes) {
            let result = db
                .sql(
                    "SELECT _key FROM vec ORDER BY emb <=> $1::vector LIMIT 1",
                    &[Param::Vector(vectors[at].clone())],
                )
                .unwrap();
            let SqlResult::Rows { rows: got, .. } = result else {
                panic!("the order did not answer rows");
            };
            assert_eq!(got.len(), 1, "the order answered nothing at row {at}");
            let SqlValue::Text(key) = &got[0].values[0] else {
                panic!("_key is not text");
            };
            answered += 1;
            if key == &format!("k{at}") {
                exact += 1;
            }
        }
        let query_ms = start.elapsed().as_secs_f64() * 1000.0 / answered as f64;
        rows_out.push(json!({
            "ef": ef.map_or("default(100)".to_owned(), |ef| ef.to_string()),
            "orders_answered": answered,
            "recall_at_1": exact as f64 / answered as f64,
            "ms_per_order": (query_ms * 1000.0).round() / 1000.0,
        }));
    }
    db.sql("COMMIT", &[]).unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "{}",
        json!({
            "arm": "sql",
            "rows": rows,
            "dimension": dim,
            "create_index_seconds": (build_seconds * 1000.0).round() / 1000.0,
            "orders": rows_out,
        })
    );
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let rows: usize = args.first().map_or(10_000, |a| a.parse().unwrap());
    let dim: usize = args.get(1).map_or(4_096, |a| a.parse().unwrap());
    let batch: usize = args.get(2).map_or(256, |a| a.parse().unwrap());
    // `sql` runs both arms; `sqlonly` runs the SQL arm alone.
    let mode = args.get(3).map(String::as_str);
    let sql = matches!(mode, Some("sql") | Some("sqlonly"));
    if mode == Some("sqlonly") {
        let vectors = corpus(rows + 1, dim, 0x51de_0f0f_1234_5678);
        sql_half(rows, dim, &vectors);
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "vamana-lanes-{rows}-{dim}-{batch}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // DEFAULT resource limits: no policy is installed, so the only bound is
    // the page-WAL's own fixed 16 MiB managed-byte allowance.
    let mut db = Database::create(&dir, cfg()).unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![("embedding".into(), Kind::Vector(dim))],
            CollectionOptions::default(),
        )
        .unwrap();
    let vectors = corpus(rows + 1, dim, 0x51de_0f0f_1234_5678);
    for (at, vector) in vectors.iter().take(rows).enumerate() {
        db.put(collection, &format!("k{at}"), &json!({"embedding": vector}))
            .unwrap();
        if at % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let index = db
        .create_vamana_index(collection, "embedding_vamana", "embedding")
        .unwrap();
    db.commit().unwrap();

    // The build, with NO retry loop: every step is driven at the batch the
    // caller asked for, and a refusal is reported rather than worked around.
    let before = db.io_counters().unwrap().wal_bytes_written;
    let start = Instant::now();
    let mut refused: Option<String> = None;
    let mut steps = 0u64;
    loop {
        match db.build_index_step(index, batch) {
            Ok(done) => {
                steps += 1;
                if let Err(error) = db.commit() {
                    refused = Some(format!("commit: {error:?}"));
                    break;
                }
                if done {
                    break;
                }
            }
            Err(error) => {
                refused = Some(format!("step: {error:?}"));
                break;
            }
        }
    }
    let build_seconds = start.elapsed().as_secs_f64();
    let build_wal = db
        .io_counters()
        .map(|c| c.wal_bytes_written - before)
        .unwrap_or_default();

    // One further LIVE insert on top of the ready graph: the per-insert
    // dirtied-byte footprint, measured rather than estimated.
    let mut insert_wal = 0u64;
    if refused.is_none() {
        let before = db.io_counters().unwrap().wal_bytes_written;
        db.put(
            collection,
            "one-more",
            &json!({"embedding": vectors[rows]}),
        )
        .unwrap();
        db.commit().unwrap();
        insert_wal = db.io_counters().unwrap().wal_bytes_written - before;
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "{}",
        json!({
            "arm": "atomic",
            "rows": rows,
            "dimension": dim,
            "batch": batch,
            "build_completed": refused.is_none(),
            "refused": refused,
            "build_steps": steps,
            "build_seconds": (build_seconds * 1000.0).round() / 1000.0,
            "build_wal_bytes": build_wal,
            "one_insert_wal_bytes": insert_wal,
        })
    );
    if sql {
        sql_half(rows, dim, &vectors);
    }
}
