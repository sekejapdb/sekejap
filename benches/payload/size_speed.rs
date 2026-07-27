//! Real-dataset payload benchmark: normal (raw JSON) vs SKBIN vs SKBIN+zstd.
//! Loads records ONCE, ingests them into three identically-built DBs under
//! different payload policies, then reports on-disk size + query latency.
//!
//! Usage (env-driven):
//!   DATASET_MODE=geojsonl DATASET_PATH=/path/file.geojsonl.json COLLECTION=villages \
//!   LIMIT=15000 [FILTER="PROVINSI = 'Jawa Barat'"] \
//!     cargo bench --bench payload_size_speed
//!
//!   DATASET_MODE=jsonarray DATASET_PATH=/path/classroom_list.json COLLECTION=classes \
//!   LIMIT=40000 cargo bench --bench payload_size_speed

use sekejap::{Config, CoreDB};
use serde_json::Value;
use std::io::BufRead;
use std::time::Instant;

fn env(k: &str) -> Option<String> { std::env::var(k).ok() }
fn mb(b: u64) -> f64 { b as f64 / (1024.0 * 1024.0) }

/// Load up to `limit` records as (slug, json). Each record gets `_collection`
/// and `_key` injected so collection scans / filters work.
fn load(mode: &str, path: &str, coll: &str, limit: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut push = |i: usize, v: Value| {
        if let Value::Object(mut m) = v {
            let key = format!("v{i}");
            m.insert("_collection".into(), Value::String(coll.into()));
            m.insert("_key".into(), Value::String(key.clone()));
            out.push((format!("{coll}/{key}"), serde_json::to_string(&Value::Object(m)).unwrap()));
        }
    };
    match mode {
        "geojsonl" => {
            let f = std::fs::File::open(path).expect("open dataset");
            for (i, line) in std::io::BufReader::new(f).lines().enumerate() {
                if i >= limit { break; }
                let line = line.unwrap();
                if line.trim().is_empty() { continue; }
                if let Ok(v) = serde_json::from_str::<Value>(&line) { push(i, v); }
            }
        }
        "jsonarray" => {
            let bytes = std::fs::read(path).expect("read dataset");
            let v: Value = serde_json::from_slice(&bytes).expect("parse json array");
            if let Value::Array(a) = v {
                for (i, e) in a.into_iter().enumerate() {
                    if i >= limit { break; }
                    push(i, e);
                }
            }
        }
        other => panic!("unknown DATASET_MODE {other}"),
    }
    out
}

/// Policy triple: (skbin, per-field zstd on strings, whole-record zstd).
type Policy = (bool, bool, bool);
const NORMAL: Policy = (false, false, false);
const SKBIN: Policy = (true, false, false);
const SKBIN_ZSTD: Policy = (true, true, false);   // per-field zstd on long strings
const RAW_ZSTD: Policy = (false, false, true);     // whole-record zstd (compresses numbers too)

fn cfg_of((binary, zstd, whole): Policy) -> Config {
    Config { payload_binary: binary, payload_zstd: zstd, payload_compression: whole, ..Config::default() }
}

fn cfg_paged((binary, zstd, whole): Policy) -> Config {
    Config { payload_binary: binary, payload_zstd: zstd, payload_compression: whole,
             paged_topology: true, ..Config::default() }
}

/// Time a query opened in PAGED mode (topology served from the mmap base — the
/// "big data on small RAM" path). Nodes are NOT loaded into RAM.
fn time_query_paged(dir: &std::path::Path, pol: Policy, sql: &str, reps: usize) -> (usize, f64) {
    let db = CoreDB::open_with_config(dir, cfg_paged(pol)).unwrap();
    let n = db.query(sql).unwrap().collect().len();
    let t = Instant::now();
    for _ in 0..reps { std::hint::black_box(db.query(sql).unwrap().collect().len()); }
    (n, t.elapsed().as_secs_f64() * 1e3 / reps as f64)
}

fn build(dir: &std::path::Path, recs: &[(String, String)], pol: Policy) {
    let mut db = CoreDB::open_with_config(dir, cfg_of(pol)).unwrap();
    for chunk in recs.chunks(5_000) {
        db.put_many(chunk.iter().map(|(s, j)| (s.as_str(), j.as_str()))).unwrap();
    }
    db.compact().unwrap();
}

fn time_query(dir: &std::path::Path, pol: Policy, sql: &str, reps: usize) -> (usize, f64) {
    let db = CoreDB::open_with_config(dir, cfg_of(pol)).unwrap();
    let n = db.query(sql).unwrap().collect().len();
    let t = Instant::now();
    for _ in 0..reps { std::hint::black_box(db.query(sql).unwrap().collect().len()); }
    (n, t.elapsed().as_secs_f64() * 1e3 / reps as f64)
}

fn main() {
    let mode = env("DATASET_MODE").expect("set DATASET_MODE=geojsonl|jsonarray");
    let path = env("DATASET_PATH").expect("set DATASET_PATH");
    let coll = env("COLLECTION").unwrap_or_else(|| "records".into());
    let limit = env("LIMIT").and_then(|v| v.parse().ok()).unwrap_or(15_000);
    let filter = env("FILTER");

    println!("== payload benchmark: normal vs SKBIN vs SKBIN+zstd ==");
    println!("dataset={path}\n  mode={mode} collection={coll} limit={limit}");

    let recs = load(&mode, &path, &coll, limit);
    let raw_bytes: u64 = recs.iter().map(|(_, j)| j.len() as u64).sum();
    let avg = if recs.is_empty() { 0 } else { raw_bytes as usize / recs.len() };
    println!("  loaded {} records, {:.1} MB raw JSON (avg {} B)\n", recs.len(), mb(raw_bytes), avg);

    let d_raw = tempfile::tempdir().unwrap();
    let d_bin = tempfile::tempdir().unwrap();
    let d_zst = tempfile::tempdir().unwrap();
    let d_wz  = tempfile::tempdir().unwrap();
    print!("  building normal…");           build(d_raw.path(), &recs, NORMAL);     println!(" done");
    print!("  building SKBIN…");            build(d_bin.path(), &recs, SKBIN);      println!(" done");
    print!("  building SKBIN+zstd…");       build(d_zst.path(), &recs, SKBIN_ZSTD); println!(" done");
    print!("  building raw+zstd(whole)…");  build(d_wz.path(),  &recs, RAW_ZSTD);   println!(" done");

    let ps = |d: &std::path::Path| std::fs::metadata(d.join("payloads.bin")).unwrap().len();
    let (praw, pbin, pzst, pwz) = (ps(d_raw.path()), ps(d_bin.path()), ps(d_zst.path()), ps(d_wz.path()));

    println!("\n[SIZE] payloads.bin");
    println!("  normal              {:>8.1} MB   1.00x", mb(praw));
    println!("  SKBIN               {:>8.1} MB   {:.2}x", mb(pbin), praw as f64 / pbin as f64);
    println!("  SKBIN+zstd(strings) {:>8.1} MB   {:.2}x", mb(pzst), praw as f64 / pzst as f64);
    println!("  raw+zstd(whole rec) {:>8.1} MB   {:.2}x", mb(pwz),  praw as f64 / pwz  as f64);

    let mut queries: Vec<(String, String, usize)> = vec![
        ("full scan (SELECT *)".into(),     format!("SELECT * FROM {coll}"), 3),
        ("projection (SELECT _key)".into(), format!("SELECT _key FROM {coll}"), 3),
        ("count (COUNT(*))".into(),         format!("SELECT COUNT(*) FROM {coll}"), 5),
    ];
    if let Some(f) = &filter {
        queries.push((format!("filter (WHERE {f})"), format!("SELECT * FROM {coll} WHERE {f}"), 3));
    }

    println!("\n[SPEED] query latency (ms avg) — normal | SKBIN | SKBIN+zstd | raw+zstd(whole)");
    for (label, sql, reps) in &queries {
        let (rn, rt) = time_query(d_raw.path(), NORMAL,     sql, *reps);
        let (bn, bt) = time_query(d_bin.path(), SKBIN,      sql, *reps);
        let (zn, zt) = time_query(d_zst.path(), SKBIN_ZSTD, sql, *reps);
        let (wn, wt) = time_query(d_wz.path(),  RAW_ZSTD,   sql, *reps);
        assert_eq!(rn, bn, "SKBIN row mismatch on {sql}");
        assert_eq!(rn, zn, "zstd row mismatch on {sql}");
        assert_eq!(rn, wn, "raw+zstd row mismatch on {sql}");
        println!("  {label:<26} rows={rn:<7} {rt:>7.1} | {bt:>7.1} ({:.2}x) | {zt:>7.1} ({:.2}x) | {wt:>7.1} ({:.2}x)",
            rt / bt, rt / zt, rt / wt);
    }
    // ── PAGED mode: the "big data on small RAM" path (nodes served from mmap) ──
    // Compare raw-paged vs SKBIN-paged — the question that matters for paged use.
    println!("\n[PAGED SPEED] query latency (ms avg) — normal-paged | SKBIN-paged");
    for (label, sql, reps) in &queries {
        let (rn, rt) = time_query_paged(d_raw.path(), NORMAL, sql, *reps);
        let (bn, bt) = time_query_paged(d_bin.path(), SKBIN,  sql, *reps);
        assert_eq!(rn, bn, "paged SKBIN row mismatch on {sql}");
        println!("  {label:<26} rows={rn:<7} {rt:>8.1} | {bt:>8.1} ({:.2}x {})",
            rt / bt, if bt <= rt { "faster" } else { "slower" });
    }

    println!("\n== done: identical results across all policies (resident + paged) ==");
}
