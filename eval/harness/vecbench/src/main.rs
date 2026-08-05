//! vecbench — VECTOR benchmark: sekejap vs pgvector vs DuckDB-VSS vs Qdrant vs
//! Weaviate vs Elasticsearch. One engine per process.
//!
//! Dataset: SIFT1M (1,000,000 × 128, L2) with exact ground-truth 100-NN.
//!   prepared/vector/sift_base.parquet        (id INT64, vector LIST<FLOAT>[128])
//!   prepared/vector/sift_queries.parquet     (id INT64, vector LIST<FLOAT>[128])   10,000
//!   prepared/vector/sift_groundtruth.parquet (query_id INT64, neighbors LIST<INT32>[100])
//!
//! Metric: ANN is approximate, so we report **recall@10 vs QPS** (+ build time, RAM).
//! Tune each engine's HNSW (m / ef_construction / ef_search) to a common recall≈0.95,
//! then compare speed. CSV: engine,n,dim,m,efc,ef,k,build_ms,index_mb,recall,qps,p50_ms,p99_ms
use std::collections::HashSet;
use std::time::Instant;

const DATA: &str = "data/prepared/vector";

fn env_usize(k: &str, d: usize) -> usize { std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d) }
fn env_str(k: &str, d: &str) -> String { std::env::var(k).unwrap_or_else(|_| d.to_string()) }

fn proc_status_mb(key: &str) -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in s.lines() {
        if let Some(r) = l.strip_prefix(key) {
            return r.trim().trim_end_matches(" kB").trim().parse::<f64>().unwrap_or(0.0) / 1024.0;
        }
    }
    0.0
}
/// Peak RSS (high-water mark) — includes transient build spikes.
fn vmhwm_mb() -> f64 { proc_status_mb("VmHWM:") }
/// Current resident RSS — the STEADY-STATE footprint (what disk-first optimises).
fn vmrss_mb() -> f64 { proc_status_mb("VmRSS:") }

/// (id, 128-d vector), sorted by id. Reads the `List<Float32>` column via Arrow
/// (duckdb-rs can't map a list column to `Vec<f32>` through FromSql).
fn load_vectors(file: &str, id_col: &str, vec_col: &str, limit: usize) -> Vec<(i64, Vec<f32>)> {
    use duckdb::arrow::array::{Array, Float32Array, Int64Array, ListArray};
    let c = duckdb::Connection::open_in_memory().unwrap();
    let lim = if limit > 0 { format!(" LIMIT {limit}") } else { String::new() };
    let sql = format!("SELECT {id_col}, {vec_col} FROM read_parquet('{DATA}/{file}') ORDER BY {id_col}{lim}");
    let mut st = c.prepare(&sql).unwrap();
    let mut out = Vec::new();
    for batch in st.query_arrow([]).unwrap() {
        let ids = batch.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        let lists = batch.column(1).as_any().downcast_ref::<ListArray>().unwrap();
        for i in 0..batch.num_rows() {
            let elem = lists.value(i);
            let fa = elem.as_any().downcast_ref::<Float32Array>().unwrap();
            let v: Vec<f32> = (0..fa.len()).map(|j| fa.value(j)).collect();
            out.push((ids.value(i), v));
        }
    }
    out
}

/// ground-truth: query_id → its 100 nearest base ids (we only use the first k).
fn load_groundtruth(limit: usize) -> Vec<Vec<i64>> {
    use duckdb::arrow::array::{Array, Int32Array, ListArray};
    let c = duckdb::Connection::open_in_memory().unwrap();
    let lim = if limit > 0 { format!(" LIMIT {limit}") } else { String::new() };
    let sql = format!("SELECT query_id, neighbors FROM read_parquet('{DATA}/sift_groundtruth.parquet') ORDER BY query_id{lim}");
    let mut st = c.prepare(&sql).unwrap();
    let mut out = Vec::new();
    for batch in st.query_arrow([]).unwrap() {
        let lists = batch.column(1).as_any().downcast_ref::<ListArray>().unwrap();
        for i in 0..batch.num_rows() {
            let elem = lists.value(i);
            let ia = elem.as_any().downcast_ref::<Int32Array>().unwrap();
            out.push((0..ia.len()).map(|j| ia.value(j) as i64).collect());
        }
    }
    out
}

// index_mb   = engine's OWN index footprint (embedded: driver RSS minus the base vectors;
//              server: reported store/index size; -1 = not exposed → read from pod sampler).
// driver_rss = harness peak RSS (VmHWM); only meaningful as "engine RAM" for embedded engines.
struct Metrics { build_ms: f64, index_mb: f64, driver_rss_mb: f64, recall: f64, qps: f64, p50: f64, p99: f64 }

/// Exact L2 top-k over the LOADED base (self-contained ground truth — valid for any
/// subset, unlike SIFT's gt which references the full 1M). Diagnostic (BRUTE=1).
fn brute_force_gt(base: &[(i64, Vec<f32>)], queries: &[(i64, Vec<f32>)], k: usize) -> Vec<Vec<i64>> {
    queries.iter().map(|(_, qv)| {
        let mut d: Vec<(f32, i64)> = base.iter().map(|(id, v)| {
            let s: f32 = qv.iter().zip(v).map(|(a, b)| { let x = a - b; x * x }).sum();
            (s, *id)
        }).collect();
        d.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        d.into_iter().take(k).map(|(_, id)| id).collect()
    }).collect()
}

/// recall@k for one query = |returned[:k] ∩ truth[:k]| / k.
fn recall_at(returned: &[i64], truth: &[i64], k: usize) -> f64 {
    let t: HashSet<i64> = truth.iter().take(k).copied().collect();
    let hits = returned.iter().take(k).filter(|id| t.contains(id)).count();
    hits as f64 / k as f64
}

/// Aggregate per-query latencies + recalls into Metrics.
fn summarize(build_ms: f64, index_mb: f64, driver_rss_mb: f64, recalls: &[f64], mut times_ms: Vec<f64>) -> Metrics {
    assert!(!recalls.is_empty() && !times_ms.is_empty(), "no query results — engine produced nothing");
    let recall = recalls.iter().sum::<f64>() / recalls.len() as f64;
    let total_s: f64 = times_ms.iter().sum::<f64>() / 1000.0;
    let qps = recalls.len() as f64 / total_s.max(1e-9);
    times_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| times_ms[((times_ms.len() as f64 - 1.0) * q).round() as usize];
    Metrics { build_ms, index_mb, driver_rss_mb, recall, qps, p50: p(0.5), p99: p(0.99) }
}

/// Bytes the driver holds for the raw base vectors (subtracted from RSS to isolate the
/// engine's own footprint for EMBEDDED engines).
fn base_mb(n: usize, dim: usize) -> f64 { (n * dim * 4) as f64 / 1048576.0 }

// ── sekejap (native HNSW, L2) ────────────────────────────────────────────────
fn run_sekejap(base: &[(i64, Vec<f32>)], queries: &[(i64, Vec<f32>)], gt: &[Vec<i64>],
               m: usize, efc: usize, ef: usize, k: usize) -> Metrics {
    use sekejap::{CoreDB, VecMetric};
    // DISK=1 → disk-first int8 index (CoreDB::open + build_hnsw_index_disk): int8 codes
    // in RAM, f32 on disk, two-stage search. Else the classic in-RAM-f32 index.
    let disk = std::env::var("DISK").is_ok();
    let _tmp; // keep the temp dir alive for the whole run
    let mut db = if disk {
        let dir = "data/runs/vector/sekejap-int8";
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        _tmp = dir.to_string();
        CoreDB::open(&_tmp).expect("open disk db")
    } else {
        _tmp = String::new();
        CoreDB::new()
    };
    let tp = Instant::now();
    if disk { db.begin_bulk(); }   // one WAL fsync for the whole load, not one per put
    for (id, v) in base {
        // Node (for slug resolution) + its vector.
        db.put(&format!("sift/{id}"), &format!(r#"{{"_collection":"sift","_key":"{id}"}}"#)).unwrap();
        db.put_vector(&format!("sift/{id}"), "emb", v).unwrap();
    }
    if disk { db.end_bulk(); }
    let put_ms = tp.elapsed().as_secs_f64() * 1000.0;
    let tb = Instant::now();
    if disk {
        db.build_hnsw_index_disk("emb", m, efc, VecMetric::L2).unwrap();
    } else {
        db.build_hnsw_index_metric("emb", m, efc, VecMetric::L2).unwrap();
    }
    let hnsw_ms = tb.elapsed().as_secs_f64() * 1000.0;
    eprintln!("[sekejap] disk={disk} put_vectors={put_ms:.0}ms  build_hnsw={hnsw_ms:.0}ms");
    let build_ms = put_ms + hnsw_ms;
    // Engine footprint. Disk-first: the INSTRUMENTED engine RAM (memory_report — the
    // real data-structure bytes, excluding harness/duckdb/allocator noise, comparable
    // to how server DBs report index size). In-RAM path: peak RSS − base vectors.
    let driver_rss = vmhwm_mb();
    let index_mb = if disk {
        let rep = db.memory_report();
        let eng: usize = rep.iter().filter(|(l, _)| !l.starts_with('_')).map(|(_, b)| *b).sum();
        for (l, b) in &rep {
            if !l.starts_with('_') && *b > 0 {
                eprintln!("[mem] {:<38} {:>7.1} MB", l, *b as f64 / 1_048_576.0);
            }
        }
        eprintln!("[mem] ENGINE(instrumented)={:.1}MB  VmRSS={:.1}MB  VmHWM={:.1}MB",
            eng as f64 / 1_048_576.0, vmrss_mb(), driver_rss);
        eng as f64 / 1_048_576.0
    } else {
        driver_rss - base_mb(base.len(), 128)
    };
    let dim = base.first().map(|(_, v)| v.len()).unwrap_or(0);

    // EF_SWEEP="50,100,150,…" → build ONCE, query at each ef (only search is affected),
    // print one CSV row per ef, then exit. Lets us trace the recall↔QPS curve cheaply.
    if let Ok(sweep) = std::env::var("EF_SWEEP") {
        for efv in sweep.split(',').filter_map(|x| x.trim().parse::<usize>().ok()) {
            db.set_hnsw_ef_search(Some(efv));
            let (recalls, times) = query_sekejap(&db, queries, gt, k);
            let met = summarize(build_ms, index_mb, driver_rss, &recalls, times);
            println!("sekejap,{},{dim},{m},{efc},{efv},{k},{:.1},{:.4},{:.1},{:.4},{:.4},{:.1},{:.1}",
                base.len(), met.build_ms, met.recall, met.qps, met.p50, met.p99, met.index_mb, met.driver_rss_mb);
        }
        std::process::exit(0);
    }

    db.set_hnsw_ef_search(Some(ef));   // match the other engines' search breadth (ef_search)
    let (recalls, times) = query_sekejap(&db, queries, gt, k);
    summarize(build_ms, index_mb, driver_rss, &recalls, times)
}

/// Run the query set against a built sekejap DB → (recalls, per-query ms).
fn query_sekejap(db: &sekejap::CoreDB, queries: &[(i64, Vec<f32>)], gt: &[Vec<i64>], k: usize)
                 -> (Vec<f64>, Vec<f64>) {
    let mut recalls = Vec::with_capacity(queries.len());
    let mut times = Vec::with_capacity(queries.len());
    for (qi, (_, qv)) in queries.iter().enumerate() {
        let qstr: String = qv.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        let sql = format!("SELECT _key FROM sift WHERE VECTOR_NEAR(emb, [{qstr}], {k})");
        let t = Instant::now();
        let hits = db.query(&sql).map(|s| s.collect()).unwrap_or_default();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let got: Vec<i64> = hits.iter()
            .filter_map(|h| h.slug.strip_prefix("sift/").and_then(|s| s.parse().ok()))
            .collect();
        recalls.push(recall_at(&got, &gt[qi], k));
    }
    (recalls, times)
}

// ── DuckDB VSS (embedded HNSW, l2sq) ─────────────────────────────────────────
fn run_duckdb(base_n: usize, queries: &[(i64, Vec<f32>)], gt: &[Vec<i64>],
              m: usize, efc: usize, ef: usize, k: usize) -> Metrics {
    use duckdb::arrow::array::{Array, Int64Array};
    let c = duckdb::Connection::open_in_memory().unwrap();
    c.execute_batch("INSTALL vss; LOAD vss; SET hnsw_enable_experimental_persistence=false;").unwrap();
    let lim = if base_n > 0 { format!(" LIMIT {base_n}") } else { String::new() };
    // Build = load vectors + construct the HNSW index.
    let t = Instant::now();
    c.execute_batch(&format!(
        "CREATE TABLE v AS SELECT id, vector::FLOAT[128] AS emb FROM read_parquet('{DATA}/sift_base.parquet'){lim};")).unwrap();
    eprintln!("[duckdb] table loaded, building HNSW index…");
    c.execute_batch(&format!(
        "CREATE INDEX idx ON v USING HNSW (emb) WITH (metric='l2sq', ef_construction={efc}, M={m});")).unwrap();
    eprintln!("[duckdb] index built");
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    let driver_rss = vmhwm_mb();
    let index_mb = driver_rss - base_mb(base_n, 128);   // engine footprint = RSS − base vectors
    c.execute_batch(&format!("SET hnsw_ef_search={ef};")).ok();

    let mut recalls = Vec::with_capacity(queries.len());
    let mut times = Vec::with_capacity(queries.len());
    for (qi, (_, qv)) in queries.iter().enumerate() {
        let arr: String = qv.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        let sql = format!("SELECT id FROM v ORDER BY array_distance(emb, [{arr}]::FLOAT[128]) LIMIT {k}");
        let t = Instant::now();
        let mut got = Vec::new();
        let mut st = c.prepare(&sql).unwrap();
        for batch in st.query_arrow([]).unwrap() {
            let ids = batch.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
            for i in 0..batch.num_rows() { got.push(ids.value(i)); }
        }
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        recalls.push(recall_at(&got, &gt[qi], k));
        let _ = qi;
    }
    summarize(build_ms, index_mb, driver_rss, &recalls, times)
}

// ── HTTP helper (ureq JSON) — FAIL-LOUD ──────────────────────────────────────
// Any transport error or non-2xx status is a hard error: server engines must not be
// allowed to emit a CSV row off a partial/rejected load: a benchmark number is
// only valid when every insert was acknowledged and the index is fully built.
fn http_try(method: &str, url: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
    let req = match method { "PUT" => ureq::put(url), "POST" => ureq::post(url), "DELETE" => ureq::delete(url), _ => ureq::get(url) };
    let resp = match body { Some(b) => req.send_json(b), None => req.call() };
    match resp {
        Ok(r) => Ok(r.into_json().unwrap_or(serde_json::Value::Null)),
        Err(ureq::Error::Status(code, r)) => {
            let t = r.into_string().unwrap_or_default();
            Err(format!("HTTP {code} on {method} {url}: {}", &t[..t.len().min(500)]))
        }
        Err(e) => Err(format!("transport error on {method} {url}: {e}")),
    }
}
/// Strict: aborts the whole engine run on any failure (→ nonzero exit → no CSV row).
fn http(method: &str, url: &str, body: Option<serde_json::Value>) -> serde_json::Value {
    http_try(method, url, body).unwrap_or_else(|e| panic!("{e}"))
}
fn sleep_ms(ms: u64) { std::thread::sleep(std::time::Duration::from_millis(ms)); }

const INDEX_WAIT_CAP_MS: u64 = 1_800_000;   // 30 min — 1M HNSW builds are minutes, not seconds

// ── Qdrant (REST, Euclid = L2) ───────────────────────────────────────────────
fn run_qdrant(base: &[(i64, Vec<f32>)], queries: &[(i64, Vec<f32>)], gt: &[Vec<i64>],
              m: usize, efc: usize, ef: usize, k: usize) -> Metrics {
    use serde_json::json;
    let n = base.len();
    let h = format!("http://{}:6333", env_str("QDRANT", "qdrant"));
    let _ = http_try("DELETE", &format!("{h}/collections/sift"), None);   // ok if absent
    http("PUT", &format!("{h}/collections/sift"), Some(json!({
        "vectors": {"size": 128, "distance": "Euclid"},
        "hnsw_config": {"m": m, "ef_construct": efc},
        "optimizers_config": {"indexing_threshold": 1}   // force EVERY segment into the HNSW graph
    })));
    let t = Instant::now();
    let mut i = 0;
    while i < n {
        let end = (i + 1000).min(n);
        let pts: Vec<_> = base[i..end].iter().map(|(id, v)| json!({"id": id, "vector": v})).collect();
        let r = http("PUT", &format!("{h}/collections/sift/points?wait=true"), Some(json!({"points": pts})));
        let st = r["result"]["status"].as_str().unwrap_or("");
        assert!(st == "completed" || st == "acknowledged", "qdrant upsert [{i}..{end}) bad status: {r}");
        if end / 100_000 != i / 100_000 { eprintln!("[qdrant] uploaded {end}/{n}"); }
        i = end;
    }
    // Every point must be stored. NOTE: collection-info `points_count` is APPROXIMATE and
    // transiently overshoots right after a bulk upload — must use the exact-count endpoint.
    let stored = http("POST", &format!("{h}/collections/sift/points/count"), Some(json!({"exact": true})))
        ["result"]["count"].as_u64().unwrap_or(0) as usize;
    assert_eq!(stored, n, "qdrant stored {stored} points, expected {n}");
    // …and fully HNSW-indexed before we query (hard-fail on timeout — no partial-index rows).
    let mut waited = 0u64;
    loop {
        let s = http("GET", &format!("{h}/collections/sift"), None);
        let indexed = s["result"]["indexed_vectors_count"].as_u64().unwrap_or(0) as usize;
        let status = s["result"]["status"].as_str().unwrap_or("");
        if status == "green" && indexed >= n { break; }
        assert!(waited <= INDEX_WAIT_CAP_MS, "qdrant index INCOMPLETE after {}s: status={status} indexed={indexed}/{n}", waited/1000);
        if waited % 10_000 == 0 { eprintln!("[qdrant] indexing {indexed}/{n} (status={status}, {}s)", waited/1000); }
        sleep_ms(1000); waited += 1000;
    }
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;

    let mut recalls = Vec::with_capacity(queries.len());
    let mut times = Vec::with_capacity(queries.len());
    for (_, qv) in queries {
        let t = Instant::now();
        let r = http("POST", &format!("{h}/collections/sift/points/search"),
                     Some(json!({"vector": qv, "limit": k, "params": {"hnsw_ef": ef}})));
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let arr = r["result"].as_array().unwrap_or_else(|| panic!("qdrant search missing result: {r}"));
        assert!(!arr.is_empty(), "qdrant search returned 0 hits: {r}");
        let got: Vec<i64> = arr.iter().filter_map(|p| p["id"].as_i64()).collect();
        recalls.push(recall_at(&got, gt.get(recalls.len()).map(|v| v.as_slice()).unwrap_or(&[]), k));
    }
    // Server-engine RAM comes from the pod sampler (index_mb=-1); driver RSS is harness-only.
    summarize(build_ms, -1.0, vmhwm_mb(), &recalls, times)
}

// ── Elasticsearch (dense_vector HNSW, l2_norm) ───────────────────────────────
fn run_es(base: &[(i64, Vec<f32>)], queries: &[(i64, Vec<f32>)], gt: &[Vec<i64>],
          m: usize, efc: usize, ef: usize, k: usize) -> Metrics {
    use serde_json::json;
    let n = base.len();
    let h = format!("http://{}:9200", env_str("ESHOST", "es"));
    let _ = http_try("DELETE", &format!("{h}/sift"), None);
    http("PUT", &format!("{h}/sift"), Some(json!({
        "settings": {"index": {"number_of_shards": 1, "number_of_replicas": 0}},
        "mappings": {"properties": {
            "vid": {"type": "long"},
            "emb": {"type": "dense_vector", "dims": 128, "index": true, "similarity": "l2_norm",
                    "index_options": {"type": "hnsw", "m": m, "ef_construction": efc}}
        }}
    })));
    let t = Instant::now();
    let mut i = 0;
    while i < n {
        let end = (i + 2000).min(n);
        let mut nd = String::new();
        for (id, v) in &base[i..end] {
            nd.push_str("{\"index\":{}}\n");
            nd.push_str(&json!({"vid": id, "emb": v}).to_string());
            nd.push('\n');
        }
        let body = ureq::post(&format!("{h}/sift/_bulk")).set("Content-Type", "application/x-ndjson")
            .send_string(&nd).unwrap_or_else(|e| panic!("es bulk [{i}..{end}) transport: {e}"))
            .into_json::<serde_json::Value>().unwrap();
        // ES returns 200 even when individual docs fail — must inspect `errors`.
        if body["errors"].as_bool().unwrap_or(true) {
            let first = body["items"].as_array().and_then(|a| a.iter().find(|it| it["index"]["error"].is_object()));
            panic!("es bulk [{i}..{end}) rejected docs: {}", first.map(|x| x.to_string()).unwrap_or_default());
        }
        i = end;
    }
    http("POST", &format!("{h}/sift/_refresh"), None);
    http("POST", &format!("{h}/sift/_forcemerge?max_num_segments=1"), None);  // one complete HNSW graph
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    let count = http("GET", &format!("{h}/sift/_count"), None)["count"].as_u64().unwrap_or(0) as usize;
    assert_eq!(count, n, "es indexed {count} docs, expected {n}");
    let index_mb = http("GET", &format!("{h}/sift/_stats/store"), None)["_all"]["total"]["store"]["size_in_bytes"].as_u64().unwrap_or(0) as f64 / 1048576.0;

    let mut recalls = Vec::with_capacity(queries.len());
    let mut times = Vec::with_capacity(queries.len());
    for (_, qv) in queries {
        let t = Instant::now();
        let r = http("POST", &format!("{h}/sift/_search"),
                     Some(json!({"knn": {"field": "emb", "query_vector": qv, "k": k, "num_candidates": ef}, "_source": ["vid"], "size": k})));
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let hits = r["hits"]["hits"].as_array().unwrap_or_else(|| panic!("es search missing hits: {r}"));
        assert!(!hits.is_empty(), "es search returned 0 hits: {r}");
        let got: Vec<i64> = hits.iter().filter_map(|hit| hit["_source"]["vid"].as_i64()).collect();
        recalls.push(recall_at(&got, gt.get(recalls.len()).map(|v| v.as_slice()).unwrap_or(&[]), k));
    }
    summarize(build_ms, index_mb, vmhwm_mb(), &recalls, times)
}

// ── Weaviate (HNSW, l2-squared) ──────────────────────────────────────────────
fn run_weaviate(base: &[(i64, Vec<f32>)], queries: &[(i64, Vec<f32>)], gt: &[Vec<i64>],
                m: usize, efc: usize, ef: usize, k: usize) -> Metrics {
    use serde_json::json;
    let n = base.len();
    let h = format!("http://{}:8080", env_str("WEAVIATE", "weaviate"));
    let _ = http_try("DELETE", &format!("{h}/v1/schema/Sift"), None);
    http("POST", &format!("{h}/v1/schema"), Some(json!({
        "class": "Sift", "vectorizer": "none",
        "vectorIndexConfig": {"distance": "l2-squared", "efConstruction": efc, "maxConnections": m, "ef": ef as i64},
        "properties": [{"name": "vid", "dataType": ["int"]}]
    })));
    let t = Instant::now();
    let mut i = 0;
    while i < n {
        let end = (i + 1000).min(n);
        let objs: Vec<_> = base[i..end].iter().map(|(id, v)| json!({
            "class": "Sift", "vector": v, "properties": {"vid": id}
        })).collect();
        let r = http("POST", &format!("{h}/v1/batch/objects"), Some(json!({"objects": objs})));
        // Weaviate returns 200 with per-object status; a FAILED object is a silent data loss.
        let items = r.as_array().unwrap_or_else(|| panic!("weaviate batch [{i}..{end}) unexpected: {r}"));
        if let Some(bad) = items.iter().find(|o| o["result"]["status"].as_str() != Some("SUCCESS")) {
            panic!("weaviate batch [{i}..{end}) object failed: {}", bad["result"]);
        }
        i = end;
    }
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    // Assert every object landed before we trust any recall number.
    let agg = http("POST", &format!("{h}/v1/graphql"), Some(json!({"query": "{Aggregate{Sift{meta{count}}}}"})));
    let count = agg["data"]["Aggregate"]["Sift"][0]["meta"]["count"].as_u64().unwrap_or(0) as usize;
    assert_eq!(count, n, "weaviate imported {count} objects, expected {n}: {agg}");

    let mut recalls = Vec::with_capacity(queries.len());
    let mut times = Vec::with_capacity(queries.len());
    for (_, qv) in queries {
        let vecstr = format!("[{}]", qv.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","));
        let gql = format!("{{Get{{Sift(nearVector:{{vector:{vecstr}}} limit:{k}){{vid}}}}}}");
        let t = Instant::now();
        let r = http("POST", &format!("{h}/v1/graphql"), Some(json!({"query": gql})));
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        assert!(r["errors"].is_null(), "weaviate graphql error: {}", r["errors"]);
        let arr = r["data"]["Get"]["Sift"].as_array().unwrap_or_else(|| panic!("weaviate search missing data: {r}"));
        assert!(!arr.is_empty(), "weaviate search returned 0 hits: {r}");
        let got: Vec<i64> = arr.iter().filter_map(|o| o["vid"].as_i64()).collect();
        recalls.push(recall_at(&got, gt.get(recalls.len()).map(|v| v.as_slice()).unwrap_or(&[]), k));
    }
    summarize(build_ms, -1.0, vmhwm_mb(), &recalls, times)
}

// ── Redis Stack / RediSearch (HNSW, L2) ──────────────────────────────────────
fn rstr(v: &redis::Value) -> Option<String> {
    match v {
        redis::Value::BulkString(b) => Some(String::from_utf8_lossy(b).to_string()),
        redis::Value::SimpleString(s) => Some(s.clone()),
        redis::Value::Int(i) => Some(i.to_string()),
        _ => None,
    }
}
/// Pull one field out of FT.INFO's flat key/value array.
fn ft_info_field(con: &mut redis::Connection, field: &str) -> Option<String> {
    let v: redis::Value = redis::cmd("FT.INFO").arg("sift").query(con).ok()?;
    if let redis::Value::Array(items) = v {
        let mut it = items.iter();
        while let Some(k) = it.next() {
            let val = it.next();
            if rstr(k).as_deref() == Some(field) { return val.and_then(rstr); }
        }
    }
    None
}
fn f32le(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for x in v { b.extend_from_slice(&x.to_le_bytes()); }
    b
}
fn run_redis(base: &[(i64, Vec<f32>)], queries: &[(i64, Vec<f32>)], gt: &[Vec<i64>],
             m: usize, efc: usize, ef: usize, k: usize) -> Metrics {
    let n = base.len();
    let url = format!("redis://{}:6379", env_str("REDISHOST", "redis"));
    let client = redis::Client::open(url).expect("redis url");
    let mut con = client.get_connection().expect("redis connect");
    let _: Result<(), _> = redis::cmd("FT.DROPINDEX").arg("sift").arg("DD").query(&mut con); // ok if absent
    redis::cmd("FT.CREATE").arg("sift").arg("ON").arg("HASH").arg("PREFIX").arg(1).arg("doc:")
        .arg("SCHEMA").arg("emb").arg("VECTOR").arg("HNSW").arg(10)
        .arg("TYPE").arg("FLOAT32").arg("DIM").arg(128).arg("DISTANCE_METRIC").arg("L2")
        .arg("M").arg(m).arg("EF_CONSTRUCTION").arg(efc)
        .query::<()>(&mut con).expect("FT.CREATE");
    let t = Instant::now();
    let mut i = 0;
    while i < n {
        let end = (i + 2000).min(n);
        let mut pipe = redis::pipe();
        for (id, v) in &base[i..end] {
            pipe.cmd("HSET").arg(format!("doc:{id}")).arg("emb").arg(f32le(v)).ignore();
        }
        pipe.query::<()>(&mut con).expect("HSET pipeline");
        if end / 100_000 != i / 100_000 { eprintln!("[redis] loaded {end}/{n}"); }
        i = end;
    }
    // RediSearch indexes in the background — wait for full ingest + 100% indexed.
    let mut waited = 0u64;
    loop {
        let docs = ft_info_field(&mut con, "num_docs").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
        let pct = ft_info_field(&mut con, "percent_indexed").and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        if docs >= n && pct >= 1.0 { break; }
        assert!(waited <= INDEX_WAIT_CAP_MS, "redis index INCOMPLETE after {}s: num_docs={docs}/{n} percent_indexed={pct}", waited/1000);
        if waited % 10_000 == 0 { eprintln!("[redis] indexing docs={docs}/{n} pct={pct:.3} ({}s)", waited/1000); }
        sleep_ms(1000); waited += 1000;
    }
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;

    let mut recalls = Vec::with_capacity(queries.len());
    let mut times = Vec::with_capacity(queries.len());
    for (_, qv) in queries {
        let q = format!("*=>[KNN {k} @emb $vec EF_RUNTIME {ef} AS score]");
        let t = Instant::now();
        let r: redis::Value = redis::cmd("FT.SEARCH").arg("sift").arg(&q)
            .arg("PARAMS").arg(2).arg("vec").arg(f32le(qv))
            .arg("SORTBY").arg("score").arg("ASC").arg("NOCONTENT").arg("LIMIT").arg(0).arg(k)
            .arg("DIALECT").arg(2).query(&mut con).expect("FT.SEARCH");
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        // NOCONTENT reply = [total, key1, key2, …]; strip the "doc:" prefix.
        let got: Vec<i64> = match r {
            redis::Value::Array(items) => items.iter().skip(1)
                .filter_map(rstr).filter_map(|s| s.strip_prefix("doc:").and_then(|x| x.parse().ok())).collect(),
            _ => vec![],
        };
        assert!(!got.is_empty(), "redis search returned 0 hits");
        recalls.push(recall_at(&got, gt.get(recalls.len()).map(|v| v.as_slice()).unwrap_or(&[]), k));
    }
    summarize(build_ms, -1.0, vmhwm_mb(), &recalls, times)
}

// ── pgvector (Postgres HNSW, vector_l2_ops) ──────────────────────────────────
fn run_pgvector(base: &[(i64, Vec<f32>)], queries: &[(i64, Vec<f32>)], gt: &[Vec<i64>],
                m: usize, efc: usize, ef: usize, k: usize) -> Metrics {
    use std::io::Write;
    let conn = format!("host={} port=5432 user=postgres password=bench dbname=bench", env_str("PGVHOST", "pgvector"));
    let mut cl = postgres::Client::connect(&conn, postgres::NoTls).expect("pgvector connect");
    cl.batch_execute("CREATE EXTENSION IF NOT EXISTS vector; DROP TABLE IF EXISTS v; CREATE TABLE v (id bigint, emb vector(128));").unwrap();
    // Build = COPY vectors in + CREATE HNSW index.
    let t = Instant::now();
    {
        let mut w = cl.copy_in("COPY v (id, emb) FROM STDIN").unwrap();
        for (id, vv) in base {
            let arr: String = vv.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
            write!(w, "{}\t[{}]\n", id, arr).unwrap();
        }
        w.finish().unwrap();
    }
    eprintln!("[pgvector] copied {} rows, building HNSW index…", base.len());
    cl.batch_execute("SET maintenance_work_mem='3GB'; SET max_parallel_maintenance_workers=8;").ok();
    cl.batch_execute(&format!("CREATE INDEX ON v USING hnsw (emb vector_l2_ops) WITH (m={m}, ef_construction={efc});")).unwrap();
    eprintln!("[pgvector] index built");
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    let loaded = cl.query_one("SELECT count(*)::bigint FROM v", &[]).unwrap().get::<_, i64>(0) as usize;
    assert_eq!(loaded, base.len(), "pgvector COPY loaded {loaded} rows, expected {}", base.len());
    let index_mb = cl.query_one("SELECT pg_total_relation_size('v')::bigint", &[]).map(|r| r.get::<_, i64>(0) as f64 / 1048576.0).unwrap_or(-1.0);
    cl.batch_execute(&format!("SET hnsw.ef_search={ef};")).ok();

    let mut recalls = Vec::with_capacity(queries.len());
    let mut times = Vec::with_capacity(queries.len());
    for (_, qv) in queries {
        let arr: String = qv.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        let sql = format!("SELECT id FROM v ORDER BY emb <-> '[{arr}]' LIMIT {k}");
        let t = Instant::now();
        let rows = cl.query(&sql, &[]).unwrap();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let got: Vec<i64> = rows.iter().map(|r| r.get::<_, i64>(0)).collect();
        recalls.push(recall_at(&got, gt.get(recalls.len()).map(|v| v.as_slice()).unwrap_or(&[]), k));
    }
    summarize(build_ms, index_mb, vmhwm_mb(), &recalls, times)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let engine = args.iter().position(|a| a == "--engine")
        .and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| "sekejap".into());
    let m = env_usize("M", 16);
    let efc = env_usize("EFC", 200);
    let _ef = env_usize("EF", 100);       // ef_search (used by engines that expose it)
    let k = env_usize("K", 10);
    let n = env_usize("N", 0);            // base limit (0 = all 1M)
    let nq = env_usize("NQ", 0);          // query limit (0 = all 10K)

    eprintln!("[vecbench] loading SIFT1M (base n={n} queries nq={nq})…");
    let base = load_vectors("sift_base.parquet", "id", "vector", n);
    let queries = load_vectors("sift_queries.parquet", "id", "vector", nq);
    // BRUTE=1 → exact gt over the loaded base (valid for subsets); else SIFT's full-base gt.
    let gt = if std::env::var("BRUTE").is_ok() {
        eprintln!("[vecbench] computing brute-force ground truth over loaded base…");
        brute_force_gt(&base, &queries, k)
    } else {
        load_groundtruth(nq)
    };
    let dim = base.first().map(|(_, v)| v.len()).unwrap_or(0);
    eprintln!("[vecbench] loaded base={} queries={} gt={} dim={}", base.len(), queries.len(), gt.len(), dim);

    println!("engine,n,dim,m,efc,ef,k,build_ms,recall@{k},qps,p50_ms,p99_ms,index_mb,driver_rss_mb");
    let met = match engine.as_str() {
        "sekejap" => run_sekejap(&base, &queries, &gt, m, efc, _ef, k),
        "duckdb"  => run_duckdb(base.len(), &queries, &gt, m, efc, _ef, k),
        "pgvector" => run_pgvector(&base, &queries, &gt, m, efc, _ef, k),
        "qdrant"   => run_qdrant(&base, &queries, &gt, m, efc, _ef, k),
        "redis"    => run_redis(&base, &queries, &gt, m, efc, _ef, k),
        "es" | "elasticsearch" => run_es(&base, &queries, &gt, m, efc, _ef, k),
        "weaviate" => run_weaviate(&base, &queries, &gt, m, efc, _ef, k),
        other => { eprintln!("[vecbench] engine '{other}' not wired yet"); return; }
    };
    // index_mb: engine's own index footprint (-1 = read pod RSS from the sampler).
    // driver_rss_mb: harness peak RSS = engine RAM for EMBEDDED engines only.
    println!("{engine},{},{dim},{m},{efc},{_ef},{k},{:.1},{:.4},{:.1},{:.4},{:.4},{:.1},{:.1}",
        base.len(), met.build_ms, met.recall, met.qps, met.p50, met.p99, met.index_mb, met.driver_rss_mb);
}
