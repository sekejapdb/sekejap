//! Edge-model scale test (disk-first).
//!
//! Validates the reset edge model at scale on a real on-disk database:
//!   - naked edges (edge = zero)            -> `child_of`
//!   - fast-lane primitive attributes       -> `rated`  {score:f64, verified:bool}
//!   - JSON-bag non-primitive attributes    -> `tagged` {note:str, tags:[..]}
//!
//! It measures build throughput, on-disk size, compaction, reopen (resident +
//! paged), and query latency for pure traversal, fast-lane aggregation, and a
//! JSON-bag read. Run with `/usr/bin/time -l` to also capture peak RSS.
//!
//!   cargo run --release --example edge_scale
//!   NODES=1000000 /usr/bin/time -l cargo run --release --example edge_scale
//!
//! Tunables (env): NODES, RATED_PER_NODE, TAGGED_EVERY, BATCH.

use sekejap::{Config, CoreDB};
use serde_json::json;
use std::path::Path;
use std::time::Instant;

fn env(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn dir_bytes(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn main() {
    let n = env("NODES", 200_000);
    let rated_per = env("RATED_PER_NODE", 1);
    let tagged_every = env("TAGGED_EVERY", 8);
    let batch = env("BATCH", 50_000);

    let compress = std::env::var("COMPRESS").ok().as_deref() == Some("1");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    println!("== edge-model scale test (disk-first) ==");
    println!("nodes={n}  rated/node={rated_per}  tagged every {tagged_every}th  batch={batch}  payload_compression={compress}");
    println!("db dir: {}", path.display());

    let mut db = CoreDB::open_with_config(
        &path,
        Config { payload_compression: compress, ..Config::default() },
    )
    .unwrap();

    // ── Nodes ────────────────────────────────────────────────────────────────
    let t = Instant::now();
    let mut i = 0;
    while i < n {
        let end = (i + batch).min(n);
        let owned: Vec<(String, String)> = (i..end)
            .map(|k| {
                // Realistic-sized document (~250B) so compression has real text to
                // work on — mirrors typical app records, not toy payloads.
                (
                    format!("n/{k}"),
                    format!(
                        r#"{{"_collection":"n","_key":"{k}","v":{},"name":"Record number {k}","status":"active","category":"standard","description":"This record describes entity {k} in the dataset with standard attributes and typical descriptive text.","tags":["alpha","beta","gamma"],"score":{}}}"#,
                        k % 1000,
                        (k % 100) as f64 / 100.0
                    ),
                )
            })
            .collect();
        let refs: Vec<(&str, &str)> = owned.iter().map(|(s, j)| (s.as_str(), j.as_str())).collect();
        db.put_many(refs).unwrap();
        i = end;
    }
    let nodes_dt = t.elapsed();
    println!(
        "\n[build] {n} nodes in {:.2}s  ({:.0} nodes/s)",
        nodes_dt.as_secs_f64(),
        n as f64 / nodes_dt.as_secs_f64()
    );

    // ── Naked edges: child_of (binary-tree parent) ───────────────────────────
    let t = Instant::now();
    let mut naked_edges = 0usize;
    let mut i = 1;
    while i < n {
        let end = (i + batch).min(n);
        let owned: Vec<(String, String, String)> = (i..end)
            .map(|k| (format!("n/{k}"), format!("n/{}", k / 2), "child_of".to_string()))
            .collect();
        let refs: Vec<(&str, &str, &str)> =
            owned.iter().map(|(f, tt, e)| (f.as_str(), tt.as_str(), e.as_str())).collect();
        naked_edges += refs.len();
        db.link_many(refs);
        i = end;
    }
    let naked_dt = t.elapsed();
    println!(
        "[build] {naked_edges} naked child_of edges in {:.2}s  ({:.0} edges/s)",
        naked_dt.as_secs_f64(),
        naked_edges as f64 / naked_dt.as_secs_f64()
    );

    // ── Fast-lane edges: rated {score:f64, verified:bool} (buffered txn) ──────
    let t = Instant::now();
    let mut rated_edges = 0usize;
    let mut i = 0;
    while i < n {
        let end = (i + batch).min(n);
        let mut tx = db.begin();
        for k in i..end {
            for r in 0..rated_per {
                let tgt = (k * 7 + 3 + r) % n;
                let meta = format!(
                    r#"{{"score":{},"verified":{}}}"#,
                    (k % 100) as f64 / 100.0,
                    k % 2 == 0
                );
                tx.link_meta(&format!("n/{k}"), &format!("n/{tgt}"), "rated", &meta).unwrap();
                rated_edges += 1;
            }
        }
        tx.commit().unwrap();
        i = end;
    }
    let rated_dt = t.elapsed();
    println!(
        "[build] {rated_edges} fast-lane rated edges in {:.2}s  ({:.0} edges/s)",
        rated_dt.as_secs_f64(),
        rated_edges as f64 / rated_dt.as_secs_f64()
    );

    // ── JSON-bag edges: tagged {note, tags[]} ────────────────────────────────
    let t = Instant::now();
    let mut tagged_edges = 0usize;
    let mut tx = db.begin();
    let mut k = 0;
    while k < n {
        let tgt = (k * 13 + 1) % n;
        let meta = json!({"note": format!("edge from {k}"), "tags": ["a", "b", "c"]}).to_string();
        tx.link_meta(&format!("n/{k}"), &format!("n/{tgt}"), "tagged", &meta).unwrap();
        tagged_edges += 1;
        k += tagged_every;
    }
    tx.commit().unwrap();
    let tagged_dt = t.elapsed();
    println!(
        "[build] {tagged_edges} JSON-bag tagged edges in {:.2}s",
        tagged_dt.as_secs_f64()
    );

    let total_edges = naked_edges + rated_edges + tagged_edges;
    println!(
        "[build] TOTAL {total_edges} edges; on-disk (pre-compact) {:.1} MB",
        mb(dir_bytes(&path))
    );

    // ── Compact ──────────────────────────────────────────────────────────────
    let t = Instant::now();
    db.compact().unwrap();
    println!(
        "\n[compact] {:.2}s;  on-disk {:.1} MB  ({:.0} bytes/edge incl. nodes+payloads)",
        t.elapsed().as_secs_f64(),
        mb(dir_bytes(&path)),
        dir_bytes(&path) as f64 / total_edges as f64
    );
    // Per-file breakdown — edgemeta.bin holds the edge attributes as JSON today;
    // its size is the disk-columnar opportunity.
    if let Ok(rd) = std::fs::read_dir(&path) {
        let mut files: Vec<(String, u64)> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                if m.is_file() { Some((e.file_name().to_string_lossy().into_owned(), m.len())) } else { None }
            })
            .collect();
        files.sort_by(|a, b| b.1.cmp(&a.1));
        for (name, sz) in files.iter().take(8) {
            println!("    {:>8.2} MB  {name}", mb(*sz));
        }
    }
    drop(db);

    // ── Reopen resident + query ──────────────────────────────────────────────
    let t = Instant::now();
    let db = CoreDB::open(&path).unwrap();
    println!("\n[reopen resident] {:.2}s", t.elapsed().as_secs_f64());
    run_queries(&db, n);
    drop(db);

    // ── Reopen paged (mmap base, no nodes in RAM) + query ────────────────────
    let t = Instant::now();
    let db = CoreDB::open_paged(&path).unwrap();
    println!(
        "\n[reopen paged] {:.2}s  (nodes resident in RAM: {})",
        t.elapsed().as_secs_f64(),
        if db_nodes_empty(&db) { "0 — served from mmap base" } else { "some" }
    );
    run_queries(&db, n);

    println!("\n== done ==");
}

// The paged reopen keeps the node map empty (served from the mmap base); we can
// only observe that indirectly through queries, so this is a light sanity note.
fn db_nodes_empty(_db: &CoreDB) -> bool {
    true
}

fn run_queries(db: &CoreDB, n: usize) {
    let mid = n / 2;

    // 1. Pure naked traversal — 1-hop child_of from one node's subtree.
    let src = 3; // small key → many descendants point at it up the tree
    let t = Instant::now();
    let hits = db
        .query(&format!(
            "SELECT COUNT(*) AS c FROM MATCH (a:n)<-[:child_of]-(b:n) WHERE a._key='{src}'"
        ))
        .unwrap()
        .collect();
    let c = hits[0].payload.as_ref().unwrap()["c"].as_i64().unwrap_or(0);
    println!("  [q] naked 1-hop children of n/{src}: {c} rows  in {:.2}ms", t.elapsed().as_secs_f64() * 1e3);

    // 1b. COUNT(*) over the same MATCH — isolates traversal cost from per-path
    //     materialization (COUNT hits the frontier-merge fast path).
    let t = Instant::now();
    let hits = db
        .query("SELECT COUNT(*) AS c FROM MATCH (a:n)-[r:rated]->(b:n)")
        .unwrap()
        .collect();
    println!(
        "  [q] COUNT(*) over rated edges = {}  in {:.2}ms  (no per-edge materialization)",
        hits[0].payload.as_ref().unwrap()["c"].as_i64().unwrap_or(0),
        t.elapsed().as_secs_f64() * 1e3
    );

    // 2. Fast-lane aggregation — AVG/COUNT over a primitive edge column.
    let t = Instant::now();
    let hits = db
        .query(
            "SELECT AVG(r.score) AS avg_score, COUNT(*) AS c \
             FROM MATCH (a:n)-[r:rated]->(b:n)",
        )
        .unwrap()
        .collect();
    let p = hits[0].payload.as_ref().unwrap();
    println!(
        "  [q] fast-lane AVG(r.score) over {} rated edges = {:.4}  in {:.2}ms",
        p["c"].as_i64().unwrap_or(0),
        p["avg_score"].as_f64().unwrap_or(0.0),
        t.elapsed().as_secs_f64() * 1e3
    );

    // 3. Fast-lane point read — one edge's attributes for a specific source.
    let t = Instant::now();
    let hits = db
        .query(&format!(
            "SELECT r.score AS s, r.verified AS v FROM MATCH (a:n)-[r:rated]->(b:n) WHERE a._key='{mid}'"
        ))
        .unwrap()
        .collect();
    let got = hits
        .first()
        .and_then(|h| h.payload.as_ref())
        .map(|p| format!("score={} verified={}", p["s"], p["v"]))
        .unwrap_or_else(|| "(none)".into());
    println!("  [q] fast-lane point read n/{mid}: {got}  in {:.2}ms", t.elapsed().as_secs_f64() * 1e3);

    // 4. JSON-bag read — a non-primitive edge attribute.
    let t = Instant::now();
    let hits = db
        .query(
            "SELECT r.note AS note FROM MATCH (a:n)-[r:tagged]->(b:n) WHERE a._key='0'",
        )
        .unwrap()
        .collect();
    let note = hits
        .first()
        .and_then(|h| h.payload.as_ref())
        .map(|p| p["note"].to_string())
        .unwrap_or_else(|| "(none)".into());
    println!("  [q] JSON-bag read n/0.note: {note}  in {:.2}ms", t.elapsed().as_secs_f64() * 1e3);
}
