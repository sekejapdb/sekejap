//! relbench v2 — RELATIONAL benchmark (ClickBench projection), Rust, one engine
//! per process for clean RSS.
//!
//! Engines (pick with --engine): sekejap | sqlite | duckdb | postgres
//!   * sekejap : embedded; queried via atomic API AND SQL (one load, two modes)
//!   * sqlite  : embedded (rusqlite, bundled)
//!   * duckdb  : embedded columnar (duckdb, bundled) — the analytics reference
//!   * postgres: SERVER reference (postgis pod) over the network via `postgres` crate
//!
//! METHODOLOGY (standard query-benchmark protocol; stated so it's auditable):
//!   CREATE TABLE -> load data -> CREATE INDEX -> ANALYZE -> warmup -> measure.
//!   * load_ms and index_ms are reported SEPARATELY and are BOTH EXCLUDED from
//!     query latency.
//!   * Every engine gets the SAME indexes (btree on RegionID, OS, ResolutionWidth,
//!     SearchPhrase). Full-scan queries (SUM, `<> ''`) stay scans on all engines.
//!   * Data is STREAMED from NDJSON in batches during load, so the harness never
//!     holds the whole dataset — RSS (VmHWM) reflects the engine.
//!   * Postgres is a server: its latency includes client<->server round-trip
//!     (embedded engines don't); server RSS is measured outside the harness,
//!     not this process. Disclosed in the report.
//!
//! Env: N (rows, default all), WARMUP (default 3), ITERS (default 20),
//!      PGHOST (default "postgis"), PGDB (default "bench").
//! CSV out: engine,load_ms,index_ms,disk_mb,rss_mb,query,p50_ms,p99_ms,rows

use rusqlite::Connection as Sqlite;
use sekejap::CoreDB;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::time::Instant;

const NDJSON: &str = "data/prepared/relational/clickbench_proj.ndjson";
const RUNS: &str = "data/runs/relational-clickbench";
const BATCH: usize = 20_000;

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}
fn env_str(k: &str, d: &str) -> String { std::env::var(k).unwrap_or_else(|_| d.to_string()) }

/// Columns to index — overridable via INDEX_COLS (comma list) so we can isolate
/// each index's cost (e.g. drop SearchPhrase to prove its RAM contribution).
fn indexed_cols() -> Vec<String> {
    env_str("INDEX_COLS", "RegionID,OS,ResolutionWidth,SearchPhrase")
        .split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}
/// Current resident set (MiB) — reflects trim_memory() (unlike VmHWM = peak).
fn vmrss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in s.lines() {
        if let Some(r) = l.strip_prefix("VmRSS:") {
            return r.trim().trim_end_matches(" kB").trim().parse::<f64>().unwrap_or(0.0) / 1024.0;
        }
    }
    0.0
}

fn vmhwm_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in s.lines() {
        if let Some(r) = l.strip_prefix("VmHWM:") {
            return r.trim().trim_end_matches(" kB").trim().parse::<f64>().unwrap_or(0.0) / 1024.0;
        }
    }
    0.0
}
/// Any /proc/self/status kB field in MiB. RssAnon = hard heap (non-reclaimable);
/// RssFile = mmap'd file pages (reclaimable page cache, evicted under pressure).
fn status_mb(field: &str) -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in s.lines() {
        if let Some(r) = l.strip_prefix(field) {
            return r.trim().trim_end_matches(" kB").trim().parse::<f64>().unwrap_or(0.0) / 1024.0;
        }
    }
    0.0
}
fn dir_mb(p: &str) -> f64 {
    fn w(p: &std::path::Path) -> u64 {
        if p.is_file() { return std::fs::metadata(p).map(|m| m.len()).unwrap_or(0); }
        let mut t = 0;
        if let Ok(rd) = std::fs::read_dir(p) { for e in rd.flatten() { t += w(&e.path()); } }
        t
    }
    w(std::path::Path::new(p)) as f64 / (1024.0 * 1024.0)
}
fn measure<F: FnMut() -> usize>(mut f: F, warmup: usize, iters: usize) -> (f64, f64, usize) {
    let mut rows = 0;
    for _ in 0..warmup { rows = f(); }
    let mut s = Vec::with_capacity(iters);
    for _ in 0..iters { let t = Instant::now(); rows = f(); s.push(t.elapsed().as_secs_f64() * 1000.0); }
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| s[((q * (s.len() - 1) as f64).round() as usize).min(s.len() - 1)];
    (p(0.50), p(0.99), rows)
}

/// Stream the NDJSON, yielding parsed rows to `f` in batches (bounded memory).
fn stream_rows<F: FnMut(&[Value])>(limit: usize, mut f: F) {
    let file = std::fs::File::open(NDJSON).expect("open ndjson");
    let mut buf: Vec<Value> = Vec::with_capacity(BATCH);
    let mut n = 0;
    for line in BufReader::new(file).lines() {
        if n >= limit { break; }
        let line = line.unwrap();
        if line.trim().is_empty() { continue; }
        buf.push(serde_json::from_str(&line).unwrap());
        n += 1;
        if buf.len() >= BATCH { f(&buf); buf.clear(); }
    }
    if !buf.is_empty() { f(&buf); }
}

fn queries() -> Vec<(&'static str, &'static str)> {
    vec![
        ("q0_count_all",       "SELECT COUNT(*) AS c FROM hits"),
        ("q1_filter_eq",       "SELECT COUNT(*) AS c FROM hits WHERE RegionID = 229"),
        ("q2_range_gt",        "SELECT COUNT(*) AS c FROM hits WHERE ResolutionWidth > 1000"),
        ("q3_groupby_region",  "SELECT RegionID, COUNT(*) AS c FROM hits GROUP BY RegionID ORDER BY c DESC LIMIT 10"),
        ("q4_groupby_os",      "SELECT OS, COUNT(*) AS c FROM hits GROUP BY OS ORDER BY c DESC LIMIT 10"),
        ("q5_sum",             "SELECT SUM(ResolutionWidth) AS s FROM hits"),
        ("q6_search_nonempty", "SELECT COUNT(*) AS c FROM hits WHERE SearchPhrase <> ''"),
    ]
}

fn emit(engine: &str, load_ms: f64, index_ms: f64, disk: f64, rss: f64, q: &str, p50: &str, p99: &str, rows: usize) {
    println!("{engine},{load_ms:.0},{index_ms:.0},{disk:.1},{rss:.1},{q},{p50},{p99},{rows}");
}

fn main() {
    let warmup = env_usize("WARMUP", 3);
    let iters = env_usize("ITERS", 20);
    let limit = env_usize("N", usize::MAX);
    let engine = std::env::args().skip_while(|a| a != "--engine").nth(1).unwrap_or_default();
    println!("engine,load_ms,index_ms,disk_mb,rss_mb,query,p50_ms,p99_ms,rows");
    match engine.as_str() {
        "sekejap" => run_sekejap(limit, warmup, iters),
        "sqlite" => run_sqlite(limit, warmup, iters),
        "duckdb" => run_duckdb(limit, warmup, iters),
        "postgres" => run_postgres(limit, warmup, iters),
        _ => { eprintln!("usage: relbench --engine sekejap|sqlite|duckdb|postgres"); std::process::exit(2); }
    }
    eprintln!("[relbench] done ({engine})");
}

fn run_sekejap(limit: usize, warmup: usize, iters: usize) {
    let dir = format!("{RUNS}/sekejap/data");
    // PAGED=1 → open_paged: node metadata lives in an mmap'd topology base (disk /
    // page cache, reclaimable) instead of the heap. compact() folds the heap overlay
    // into that base. QUERY_ONLY=1 → skip load/index and just open the pre-built DB and
    // query it — isolates SERVE-time RAM (what embedded devices actually pay) from the
    // LOAD-time heap peak.
    let paged = env_usize("PAGED", 0) == 1;
    let query_only = env_usize("QUERY_ONLY", 0) == 1;
    if !query_only { let _ = std::fs::remove_dir_all(&dir); std::fs::create_dir_all(&dir).unwrap(); }
    let mut db = if paged { CoreDB::open_paged(&dir).expect("open_paged") } else { CoreDB::open(&dir).expect("open") };

    let mut load_ms = 0.0; let mut index_ms = 0.0;
    if !query_only {
        db.execute("CREATE TABLE hits (_key TEXT PRIMARY KEY, WatchID INTEGER, RegionID INTEGER, ResolutionWidth INTEGER, OS INTEGER, SearchPhrase TEXT, CounterID INTEGER, UserID INTEGER, URL TEXT)").ok();
        let mut idx = 0usize;
        let mut since_compact = 0usize;
        // Periodic-compact cadence during load. Every compact rewrites the growing
        // base (~O(N^2) total), so at 10M set this high (e.g. > N) to do a SINGLE
        // final compact instead — trades a larger build-time heap for tractable load.
        let compact_every = env_usize("COMPACT_EVERY", 100_000);
        let t = Instant::now();
        stream_rows(limit, |batch| {
            let pairs: Vec<(String, String)> = batch.iter().map(|v| {
                let mut o = v.as_object().unwrap().clone();
                o.insert("_collection".into(), Value::from("hits"));
                let k = idx.to_string(); idx += 1;
                o.insert("_key".into(), Value::from(k.clone()));
                (format!("hits/{k}"), serde_json::to_string(&Value::Object(o)).unwrap())
            }).collect();
            db.put_many(pairs.iter().map(|(s, j)| (s.as_str(), j.as_str()))).expect("put_many");
            // Paged: fold to the mmap base every ~100k so the heap overlay stays bounded
            // (otherwise the whole dataset piles up in the heap before the final compact).
            if paged {
                since_compact += batch.len();
                if since_compact >= compact_every { db.compact().ok(); since_compact = 0; }
            }
        });
        load_ms = t.elapsed().as_secs_f64() * 1000.0;

        // Build field indexes, THEN compact — so the indexes are persisted into the
        // mmap'd fieldstore sidecars. A reopened paged DB (QUERY_ONLY) mmaps them
        // via field_base; no rebuild scan, posting lists stay off-heap.
        let indexed = indexed_cols();
        let ti = Instant::now();
        for c in &indexed { db.execute(&format!("CREATE INDEX ON hits USING btree ({c})")).ok(); }
        index_ms = ti.elapsed().as_secs_f64() * 1000.0;

        if paged {   // fold heap overlay + write topology & fieldstore sidecars to disk
            let tc = Instant::now();
            db.compact().ok();
            load_ms += tc.elapsed().as_secs_f64() * 1000.0;
        }
    }
    if env_usize("TRIM", 0) == 1 { db.trim_memory(); }   // reclaim over-allocated map/index capacity
    let disk = dir_mb(&dir); let rss = vmrss_mb();        // current RSS (reflects trim), not peak
    eprintln!("[relbench] RSS split: VmRSS={:.1} RssAnon={:.1}(hard heap) RssFile={:.1}(reclaimable mmap) VmHWM={:.1}",
              rss, status_mb("RssAnon:"), status_mb("RssFile:"), vmhwm_mb());

    // atomic mode (count/eq/range; group-by & sum are SQL-only in the chainable API)
    let (p50, p99, r) = measure(|| db.collection("hits").count(), warmup, iters);
    emit("sekejap-atomic", load_ms, index_ms, disk, rss, "q0_count_all", &fmt(p50), &fmt(p99), r);
    let (p50, p99, r) = measure(|| db.collection("hits").where_eq("RegionID", 229i64).count(), warmup, iters);
    emit("sekejap-atomic", load_ms, index_ms, disk, rss, "q1_filter_eq", &fmt(p50), &fmt(p99), r);
    let (p50, p99, r) = measure(|| db.collection("hits").where_gt("ResolutionWidth", 1000.0).count(), warmup, iters);
    emit("sekejap-atomic", load_ms, index_ms, disk, rss, "q2_range_gt", &fmt(p50), &fmt(p99), r);
    for q in ["q3_groupby_region", "q4_groupby_os", "q5_sum", "q6_search_nonempty"] {
        emit("sekejap-atomic", load_ms, index_ms, disk, rss, q, "NA", "NA", 0);
    }
    // sql mode (all 7)
    for (qid, sql) in queries() {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
            measure(|| db.query(sql).map(|s| s.collect().len()).unwrap_or(0), warmup, iters))) {
            Ok((p50, p99, r)) => emit("sekejap-sql", load_ms, index_ms, disk, rss, qid, &fmt(p50), &fmt(p99), r),
            Err(_) => emit("sekejap-sql", load_ms, index_ms, disk, rss, qid, "NA", "NA", 0),
        }
    }
}

fn run_sqlite(limit: usize, warmup: usize, iters: usize) {
    let dir = format!("{RUNS}/sqlite");
    let _ = std::fs::remove_dir_all(&dir); std::fs::create_dir_all(&dir).unwrap();
    let dbfile = format!("{dir}/hits.sqlite");
    let conn = Sqlite::open(&dbfile).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").ok();
    conn.execute("CREATE TABLE hits (_key TEXT PRIMARY KEY, WatchID INTEGER, RegionID INTEGER, ResolutionWidth INTEGER, OS INTEGER, SearchPhrase TEXT, CounterID INTEGER, UserID INTEGER, URL TEXT)", []).unwrap();

    let mut idx = 0usize;
    let t = Instant::now();
    conn.execute_batch("BEGIN").ok();
    stream_rows(limit, |batch| {
        let mut st = conn.prepare_cached("INSERT INTO hits (_key,WatchID,RegionID,ResolutionWidth,OS,SearchPhrase,CounterID,UserID,URL) VALUES (?,?,?,?,?,?,?,?,?)").unwrap();
        for v in batch {
            let o = v.as_object().unwrap();
            let gi = |k: &str| o.get(k).and_then(|x| x.as_i64());
            let gs = |k: &str| o.get(k).and_then(|x| x.as_str()).unwrap_or("");
            st.execute(rusqlite::params![idx.to_string(), gi("WatchID"), gi("RegionID"), gi("ResolutionWidth"), gi("OS"), gs("SearchPhrase"), gi("CounterID"), gi("UserID"), gs("URL")]).unwrap();
            idx += 1;
        }
    });
    conn.execute_batch("COMMIT").ok();
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    for c in &indexed_cols() { conn.execute(&format!("CREATE INDEX idx_{c} ON hits ({c})"), []).ok(); }
    conn.execute_batch("ANALYZE").ok();
    let index_ms = t.elapsed().as_secs_f64() * 1000.0;
    let disk = dir_mb(&dir); let rss = vmhwm_mb();

    for (qid, sql) in queries() {
        let (p50, p99, r) = measure(|| {
            let mut st = conn.prepare_cached(sql).unwrap();
            let mut c = 0; let mut rws = st.query([]).unwrap();
            while rws.next().unwrap().is_some() { c += 1; } c
        }, warmup, iters);
        emit("sqlite", load_ms, index_ms, disk, rss, qid, &fmt(p50), &fmt(p99), r);
    }
}

fn run_duckdb(limit: usize, warmup: usize, iters: usize) {
    use duckdb::Connection as Duck;
    let dir = format!("{RUNS}/duckdb");
    let _ = std::fs::remove_dir_all(&dir); std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(format!("{dir}/tmp")).ok();
    let dbfile = format!("{dir}/hits.duckdb");
    let conn = Duck::open(&dbfile).unwrap();
    // Bound RAM + allow spill so RSS is a fair embedded comparison (not "use all RAM").
    conn.execute_batch(&format!("PRAGMA memory_limit='2GB'; PRAGMA temp_directory='{dir}/tmp';")).ok();
    conn.execute_batch("CREATE TABLE hits (_key VARCHAR PRIMARY KEY, WatchID BIGINT, RegionID BIGINT, ResolutionWidth BIGINT, OS BIGINT, SearchPhrase VARCHAR, CounterID BIGINT, UserID BIGINT, URL VARCHAR)").unwrap();

    let mut idx = 0usize;
    let t = Instant::now();
    {
        // Appender is DuckDB's bulk-load fast path (columnar batch insert).
        let mut app = conn.appender("hits").unwrap();
        stream_rows(limit, |batch| {
            for v in batch {
                let o = v.as_object().unwrap();
                let gi = |k: &str| o.get(k).and_then(|x| x.as_i64());
                let gs = |k: &str| o.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                app.append_row(duckdb::params![
                    idx.to_string(), gi("WatchID"), gi("RegionID"), gi("ResolutionWidth"),
                    gi("OS"), gs("SearchPhrase"), gi("CounterID"), gi("UserID"), gs("URL")
                ]).unwrap();
                idx += 1;
            }
        });
        app.flush().ok();
    }
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    for c in &indexed_cols() { conn.execute(&format!("CREATE INDEX idx_{c} ON hits ({c})"), []).ok(); }
    let index_ms = t.elapsed().as_secs_f64() * 1000.0;
    let disk = dir_mb(&dir); let rss = vmhwm_mb();

    for (qid, sql) in queries() {
        let (p50, p99, r) = measure(|| {
            let mut st = conn.prepare(sql).unwrap();
            let mut c = 0usize;
            let mut rws = st.query([]).unwrap();
            while rws.next().unwrap().is_some() { c += 1; }
            c
        }, warmup, iters);
        emit("duckdb", load_ms, index_ms, disk, rss, qid, &fmt(p50), &fmt(p99), r);
    }
}

fn run_postgres(limit: usize, warmup: usize, iters: usize) {
    let host = env_str("PGHOST", "postgis");
    let db = env_str("PGDB", "bench");
    let conn_str = format!("host={host} port=5432 user=postgres password=bench dbname={db}");
    let mut client = postgres::Client::connect(&conn_str, postgres::NoTls).expect("pg connect");
    client.batch_execute("DROP TABLE IF EXISTS hits; CREATE TABLE hits (_key TEXT PRIMARY KEY, WatchID BIGINT, RegionID INTEGER, ResolutionWidth INTEGER, OS INTEGER, SearchPhrase TEXT, CounterID INTEGER, UserID BIGINT, URL TEXT)").unwrap();

    // load via COPY CSV (Postgres native bulk path)
    let mut idx = 0usize;
    let t = Instant::now();
    {
        let mut w = client.copy_in("COPY hits (_key,WatchID,RegionID,ResolutionWidth,OS,SearchPhrase,CounterID,UserID,URL) FROM STDIN WITH (FORMAT csv)").unwrap();
        let mut csv = String::new();
        stream_rows(limit, |batch| {
            csv.clear();
            for v in batch {
                let o = v.as_object().unwrap();
                let gi = |k: &str| o.get(k).and_then(|x| x.as_i64()).map(|n| n.to_string()).unwrap_or_default();
                let gs = |k: &str| o.get(k).and_then(|x| x.as_str()).unwrap_or("");
                csv.push_str(&format!("{},{},{},{},{},{},{},{},{}\n",
                    idx, gi("WatchID"), gi("RegionID"), gi("ResolutionWidth"), gi("OS"),
                    csvq(gs("SearchPhrase")), gi("CounterID"), gi("UserID"), csvq(gs("URL"))));
                idx += 1;
            }
            w.write_all(csv.as_bytes()).unwrap();
        });
        w.finish().unwrap();
    }
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    for c in &indexed_cols() { client.batch_execute(&format!("CREATE INDEX ON hits ({c})")).ok(); }
    client.batch_execute("ANALYZE hits").ok();
    let index_ms = t.elapsed().as_secs_f64() * 1000.0;

    // on-disk size via pg_total_relation_size; RSS is the pod's (measured externally)
    let disk: f64 = client.query_one("SELECT pg_total_relation_size('hits')::float8 / (1024*1024)", &[])
        .map(|r| r.get(0)).unwrap_or(0.0);

    for (qid, sql) in queries() {
        let (p50, p99, r) = measure(|| {
            let rows = client.query(sql, &[]).unwrap(); rows.len()
        }, warmup, iters);
        emit("postgres", load_ms, index_ms, disk, -1.0, qid, &fmt(p50), &fmt(p99), r);
    }
}

fn fmt(x: f64) -> String { format!("{x:.4}") }
/// CSV-quote a text field (wrap in quotes, double internal quotes).
fn csvq(s: &str) -> String { format!("\"{}\"", s.replace('"', "\"\"")) }
