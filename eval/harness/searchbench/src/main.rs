//! searchbench — full-text/BM25 SEARCH benchmark: sekejap vs Elasticsearch vs Solr
//! vs Meilisearch vs Postgres FTS vs DuckDB FTS.
//!
//! Dataset: BEIR **FiQA-2018** — 57,638 financial-domain docs, 648 test queries,
//! human relevance judgments (qrels). Metric: **nDCG@10 + recall@{10,100}** (real
//! quality, from qrels) vs latency (p50/p99), QPS, build time, RAM.
//! CSV: engine,n,nq,k,build_ms,ndcg10,recall10,recall100,qps,p50_ms,p99_ms,index_mb

use std::collections::{HashMap, HashSet};
use std::time::Instant;

const DATA: &str = "data/prepared/search/fiqa";

fn env_str(k: &str, d: &str) -> String { std::env::var(k).unwrap_or_else(|_| d.to_string()) }
fn vmrss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in s.lines() {
        if let Some(r) = l.strip_prefix("VmRSS:") {
            return r.trim().trim_end_matches(" kB").trim().parse::<f64>().unwrap_or(0.0) / 1024.0;
        }
    }
    0.0
}

/// Minimal JSON string-field extractor for `{"_id": "..","title":"..","text":".."}`
/// (BEIR jsonl). Avoids a serde derive; values may contain escaped quotes.
fn json_str_field(line: &str, key: &str) -> String {
    let pat = format!("\"{key}\":");
    let Some(mut i) = line.find(&pat) else { return String::new() };
    i += pat.len();
    let b = line.as_bytes();
    while i < b.len() && (b[i] == b' ') { i += 1; }
    if i >= b.len() || b[i] != b'"' { return String::new(); }
    i += 1;
    let mut out = String::new();
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            let n = b[i + 1];
            match n { b'"' => out.push('"'), b'\\' => out.push('\\'), b'n' => out.push('\n'),
                      b't' => out.push('\t'), b'/' => out.push('/'), b'r' => {}, _ => { out.push('\\'); out.push(n as char); } }
            i += 2; continue;
        }
        if c == b'"' { break; }
        out.push(c as char);
        i += 1;
    }
    out
}

/// Corpus → Vec<(id, "title text")>.
fn load_corpus(limit: usize) -> Vec<(String, String)> {
    let s = std::fs::read_to_string(format!("{DATA}/corpus.jsonl")).expect("corpus.jsonl");
    let mut out = Vec::new();
    for line in s.lines() {
        if line.is_empty() { continue; }
        let id = json_str_field(line, "_id");
        let title = json_str_field(line, "title");
        let text = json_str_field(line, "text");
        let body = if title.is_empty() { text } else { format!("{title} {text}") };
        out.push((id, body));
        if limit > 0 && out.len() >= limit { break; }
    }
    out
}

/// qrels/test.tsv → qid → {docid: rel}. Skips the header line.
fn load_qrels() -> HashMap<String, HashMap<String, u32>> {
    let s = std::fs::read_to_string(format!("{DATA}/qrels/test.tsv")).expect("qrels/test.tsv");
    let mut m: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for (i, line) in s.lines().enumerate() {
        if i == 0 || line.is_empty() { continue; } // header
        let mut it = line.split('\t');
        let (Some(q), Some(d), Some(r)) = (it.next(), it.next(), it.next()) else { continue };
        let rel: u32 = r.trim().parse().unwrap_or(0);
        if rel > 0 { m.entry(q.to_string()).or_default().insert(d.to_string(), rel); }
    }
    m
}

/// Test queries = those with qrels. Returns Vec<(id, text)>.
fn load_test_queries(qrels: &HashMap<String, HashMap<String, u32>>) -> Vec<(String, String)> {
    let s = std::fs::read_to_string(format!("{DATA}/queries.jsonl")).expect("queries.jsonl");
    let mut out = Vec::new();
    for line in s.lines() {
        if line.is_empty() { continue; }
        let id = json_str_field(line, "_id");
        if !qrels.contains_key(&id) { continue; }
        out.push((id.clone(), json_str_field(line, "text")));
    }
    out
}

// ── IR metrics ────────────────────────────────────────────────────────────────
/// nDCG@k with exponential gain 2^rel−1 (pytrec_eval / BEIR convention).
fn ndcg_at_k(ranked: &[String], rels: &HashMap<String, u32>, k: usize) -> f64 {
    let mut dcg = 0.0;
    for (i, doc) in ranked.iter().take(k).enumerate() {
        if let Some(&r) = rels.get(doc) {
            dcg += ((2f64.powi(r as i32)) - 1.0) / ((i + 2) as f64).log2();
        }
    }
    let mut ideal: Vec<u32> = rels.values().copied().collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let mut idcg = 0.0;
    for (i, &r) in ideal.iter().take(k).enumerate() {
        idcg += ((2f64.powi(r as i32)) - 1.0) / ((i + 2) as f64).log2();
    }
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}
fn recall_at_k(ranked: &[String], rels: &HashMap<String, u32>, k: usize) -> f64 {
    if rels.is_empty() { return 0.0; }
    let top: HashSet<&String> = ranked.iter().take(k).collect();
    let hit = rels.keys().filter(|d| top.contains(d)).count();
    hit as f64 / rels.len() as f64
}

// p50/p99/qps = wall-clock (incl. HTTP round-trip for networked engines).
// server_p50 = engine's own reported search time (Meili processingTimeMs / ES took /
// Solr QTime) — isolates pure search speed from network; -1 if the engine doesn't report it.
struct Metrics { build_ms: f64, ndcg10: f64, recall10: f64, recall100: f64, qps: f64, p50: f64, p99: f64, server_p50: f64, index_mb: f64 }

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() { return -1.0; }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}
fn summarize(build_ms: f64, index_mb: f64, ndcgs: &[f64], r10: &[f64], r100: &[f64], mut times: Vec<f64>, server: Vec<f64>) -> Metrics {
    assert!(!ndcgs.is_empty(), "no queries evaluated");
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let total_s: f64 = times.iter().sum::<f64>() / 1000.0;
    let qps = ndcgs.len() as f64 / total_s.max(1e-9);
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| times[((times.len() as f64 - 1.0) * q).round() as usize];
    Metrics { build_ms, ndcg10: mean(ndcgs), recall10: mean(r10), recall100: mean(r100), qps, p50: p(0.5), p99: p(0.99), server_p50: median(server), index_mb }
}

// ── sekejap BM25 ──────────────────────────────────────────────────────────────
fn run_sekejap(corpus: &[(String, String)], queries: &[(String, String)],
               qrels: &HashMap<String, HashMap<String, u32>>, k: usize) -> Metrics {
    use sekejap::CoreDB;
    let dir = "data/runs/search/sekejap";
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let mut db = CoreDB::open(dir).expect("open");

    let t = Instant::now();
    db.begin_bulk();
    for (id, body) in corpus {
        let esc = body.replace('\\', "\\\\").replace('"', "\\\"");
        db.put(&format!("fiqa/{id}"), &format!(r#"{{"_collection":"fiqa","_key":"{id}","text":"{esc}"}}"#)).unwrap();
    }
    db.end_bulk();
    db.build_bm25_index("text");
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    // Disk-first BM25: postings on disk, only dict + doc arrays in RAM. Report the
    // instrumented BM25 index RAM (byte-exact) + process RSS for reference.
    for (l, b) in db.memory_report() {
        if !l.starts_with('_') && b > 0 { eprintln!("[mem] {:<34} {:>7.1} MB", l, b as f64 / 1_048_576.0); }
    }
    let bm25_mb = db.memory_report().iter().find(|(l, _)| l.starts_with("bm25")).map(|(_, b)| *b as f64 / 1_048_576.0).unwrap_or(-1.0);
    eprintln!("[mem] bm25_index(instrumented)={bm25_mb:.1}MB  VmRSS={:.1}MB", vmrss_mb());
    let index_mb = bm25_mb;

    let (mut ndcgs, mut r10, mut r100, mut times) = (vec![], vec![], vec![], vec![]);
    for (qid, qtext) in queries {
        let rels = &qrels[qid];
        let t = Instant::now();
        let hits = db.bm25_search("text", qtext, 100);
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let ranked: Vec<String> = hits.iter()
            .filter_map(|(h, _)| db.slug_of(*h).and_then(|s| s.strip_prefix("fiqa/")).map(|s| s.to_string()))
            .collect();
        ndcgs.push(ndcg_at_k(&ranked, rels, k));
        r10.push(recall_at_k(&ranked, rels, 10));
        r100.push(recall_at_k(&ranked, rels, 100));
    }
    let _ = std::fs::remove_dir_all(dir);
    let sv = times.clone(); // embedded: no network, server time = wall-clock
    summarize(build_ms, index_mb, &ndcgs, &r10, &r100, times, sv)
}

// ── HTTP helper (fail-loud) ──────────────────────────────────────────────────
fn http_auth(method: &str, url: &str, body: Option<serde_json::Value>, bearer: Option<&str>) -> Result<serde_json::Value, String> {
    let mut req = match method { "PUT" => ureq::put(url), "POST" => ureq::post(url), "DELETE" => ureq::delete(url), "PATCH" => ureq::request("PATCH", url), _ => ureq::get(url) };
    if let Some(b) = bearer { req = req.set("Authorization", &format!("Bearer {b}")); }
    let resp = match body { Some(b) => req.send_json(b), None => req.call() };
    match resp {
        Ok(r) => Ok(r.into_json().unwrap_or(serde_json::Value::Null)),
        Err(ureq::Error::Status(c, r)) => Err(format!("HTTP {c} {method} {url}: {}", r.into_string().unwrap_or_default().chars().take(300).collect::<String>())),
        Err(e) => Err(format!("transport {method} {url}: {e}")),
    }
}
fn http_try(method: &str, url: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
    http_auth(method, url, body, None)
}
fn http(method: &str, url: &str, body: Option<serde_json::Value>) -> serde_json::Value {
    http_try(method, url, body).unwrap_or_else(|e| panic!("{e}"))
}
fn sleep_ms(ms: u64) { std::thread::sleep(std::time::Duration::from_millis(ms)); }

// ── Elasticsearch (BM25 default `similarity`) ────────────────────────────────
fn run_es(corpus: &[(String, String)], queries: &[(String, String)],
          qrels: &HashMap<String, HashMap<String, u32>>, k: usize) -> Metrics {
    use serde_json::json;
    let h = format!("http://{}:9200", env_str("ESHOST", "es"));
    let _ = http_try("DELETE", &format!("{h}/fiqa"), None);
    http("PUT", &format!("{h}/fiqa"), Some(json!({
        "settings": {"index": {"number_of_shards": 1, "number_of_replicas": 0}},
        "mappings": {"properties": {"docid": {"type": "keyword"}, "text": {"type": "text"}}}
    })));
    let t = Instant::now();
    let mut i = 0;
    while i < corpus.len() {
        let end = (i + 2000).min(corpus.len());
        let mut nd = String::new();
        for (id, body) in &corpus[i..end] {
            nd.push_str("{\"index\":{}}\n");
            nd.push_str(&json!({"docid": id, "text": body}).to_string());
            nd.push('\n');
        }
        let body = ureq::post(&format!("{h}/fiqa/_bulk")).set("Content-Type", "application/x-ndjson")
            .send_string(&nd).unwrap_or_else(|e| panic!("es bulk: {e}")).into_json::<serde_json::Value>().unwrap();
        if body["errors"].as_bool().unwrap_or(true) { panic!("es bulk rejected docs at [{i}..{end})"); }
        i = end;
    }
    http("POST", &format!("{h}/fiqa/_refresh"), None);
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    let cnt = http("GET", &format!("{h}/fiqa/_count"), None)["count"].as_u64().unwrap_or(0) as usize;
    assert_eq!(cnt, corpus.len(), "es indexed {cnt}, expected {}", corpus.len());
    let index_mb = http("GET", &format!("{h}/fiqa/_stats/store"), None)["_all"]["total"]["store"]["size_in_bytes"].as_u64().unwrap_or(0) as f64 / 1048576.0;

    let (mut ndcgs, mut r10, mut r100, mut times, mut server) = (vec![], vec![], vec![], vec![], vec![]);
    for (qid, qtext) in queries {
        let t = Instant::now();
        let r = http("POST", &format!("{h}/fiqa/_search"),
            Some(json!({"query": {"match": {"text": qtext}}, "_source": ["docid"], "size": 100})));
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        server.push(r["took"].as_f64().unwrap_or(-1.0)); // ES server-side ms
        let ranked: Vec<String> = r["hits"]["hits"].as_array().map(|a| a.iter()
            .filter_map(|h| h["_source"]["docid"].as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
        let rels = &qrels[qid];
        ndcgs.push(ndcg_at_k(&ranked, rels, k)); r10.push(recall_at_k(&ranked, rels, 10)); r100.push(recall_at_k(&ranked, rels, 100));
    }
    summarize(build_ms, index_mb, &ndcgs, &r10, &r100, times, server)
}

// ── Meilisearch (BM25-ranked) ─────────────────────────────────────────────────
fn run_meili(corpus: &[(String, String)], queries: &[(String, String)],
             qrels: &HashMap<String, HashMap<String, u32>>, k: usize) -> Metrics {
    use serde_json::json;
    let h = format!("http://{}:7700", env_str("MEILI", "meili"));
    let key = env_str("MEILI_KEY", "sekejapBenchmarkMasterKey2026");
    let ky = Some(key.as_str());
    let ma = |m: &str, u: &str, b: Option<serde_json::Value>| http_auth(m, u, b, ky).unwrap_or_else(|e| panic!("{e}"));
    let _ = http_auth("DELETE", &format!("{h}/indexes/fiqa"), None, ky);
    sleep_ms(500);
    ma("POST", &format!("{h}/indexes"), Some(json!({"uid": "fiqa", "primaryKey": "docid"})));
    sleep_ms(500);
    // Only "text" is searchable (not docid), and rank by relevance for a fair BM25-ish
    // comparison (Meili's default ranking rules still apply — it targets instant-search).
    ma("PATCH", &format!("{h}/indexes/fiqa/settings"), Some(json!({
        "searchableAttributes": ["text"], "displayedAttributes": ["docid"]
    })));
    sleep_ms(500);
    let t = Instant::now();
    let mut i = 0;
    while i < corpus.len() {
        let end = (i + 5000).min(corpus.len());
        let docs: Vec<_> = corpus[i..end].iter().map(|(id, b)| json!({"docid": id, "text": b})).collect();
        let r = ma("POST", &format!("{h}/indexes/fiqa/documents"), Some(json!(docs)));
        let _ = r["taskUid"].as_u64().or(r["uid"].as_u64()).expect("meili enqueue");
        i = end;
    }
    // Wait for all enqueued indexing tasks to succeed.
    let mut waited = 0;
    loop {
        let s = ma("GET", &format!("{h}/indexes/fiqa/stats"), None);
        let done = !s["isIndexing"].as_bool().unwrap_or(true);
        let cnt = s["numberOfDocuments"].as_u64().unwrap_or(0) as usize;
        if done && cnt >= corpus.len() { break; }
        assert!(waited <= 600_000, "meili indexing incomplete: {cnt}/{}", corpus.len());
        sleep_ms(500); waited += 500;
    }
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;

    let (mut ndcgs, mut r10, mut r100, mut times, mut server) = (vec![], vec![], vec![], vec![], vec![]);
    for (qid, qtext) in queries {
        let t = Instant::now();
        let r = ma("POST", &format!("{h}/indexes/fiqa/search"), Some(json!({"q": qtext, "limit": 100, "attributesToRetrieve": ["docid"]})));
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        server.push(r["processingTimeMs"].as_f64().unwrap_or(-1.0)); // Meili server-side ms
        let ranked: Vec<String> = r["hits"].as_array().map(|a| a.iter()
            .filter_map(|h| h["docid"].as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
        let rels = &qrels[qid];
        ndcgs.push(ndcg_at_k(&ranked, rels, k)); r10.push(recall_at_k(&ranked, rels, 10)); r100.push(recall_at_k(&ranked, rels, 100));
    }
    summarize(build_ms, -1.0, &ndcgs, &r10, &r100, times, server)
}

// ── Solr (BM25 default since 6.0) ─────────────────────────────────────────────
fn run_solr(corpus: &[(String, String)], queries: &[(String, String)],
            qrels: &HashMap<String, HashMap<String, u32>>, k: usize) -> Metrics {
    use serde_json::json;
    let h = format!("http://{}:8983/solr", env_str("SOLR", "solr"));
    // (Re)create a core named fiqa via the cores API is heavy; assume a "fiqa" core
    // exists or use the default. Clear it first.
    let _ = http_try("POST", &format!("{h}/fiqa/update?commit=true"), Some(json!({"delete": {"query": "*:*"}})));
    let t = Instant::now();
    let mut i = 0;
    while i < corpus.len() {
        let end = (i + 5000).min(corpus.len());
        let docs: Vec<_> = corpus[i..end].iter().map(|(id, b)| json!({"id": id, "text": b})).collect();
        http("POST", &format!("{h}/fiqa/update"), Some(json!(docs)));
        i = end;
    }
    http("POST", &format!("{h}/fiqa/update?commit=true"), Some(json!({})));
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    let cnt = http("GET", &format!("{h}/fiqa/select?q=*:*&rows=0"), None)["response"]["numFound"].as_u64().unwrap_or(0) as usize;
    assert_eq!(cnt, corpus.len(), "solr indexed {cnt}, expected {}", corpus.len());

    let (mut ndcgs, mut r10, mut r100, mut times, mut server) = (vec![], vec![], vec![], vec![], vec![]);
    for (qid, qtext) in queries {
        // sanitize query for Solr edismax: strip special syntax chars.
        let q: String = qtext.chars().map(|c| if "+-&|!(){}[]^\"~*?:\\/".contains(c) { ' ' } else { c }).collect();
        let t = Instant::now();
        let r = http("POST", &format!("{h}/fiqa/select"),
            Some(json!({"query": q, "params": {"defType": "edismax", "qf": "text", "fl": "id", "rows": 100}})));
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        server.push(r["responseHeader"]["QTime"].as_f64().unwrap_or(-1.0)); // Solr server-side ms
        let ranked: Vec<String> = r["response"]["docs"].as_array().map(|a| a.iter()
            .filter_map(|d| d["id"].as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
        let rels = &qrels[qid];
        ndcgs.push(ndcg_at_k(&ranked, rels, k)); r10.push(recall_at_k(&ranked, rels, 10)); r100.push(recall_at_k(&ranked, rels, 100));
    }
    summarize(build_ms, -1.0, &ndcgs, &r10, &r100, times, server)
}

// ── Postgres FTS (tsvector + GIN, ts_rank_cd) ────────────────────────────────
fn run_pgfts(corpus: &[(String, String)], queries: &[(String, String)],
             qrels: &HashMap<String, HashMap<String, u32>>, k: usize) -> Metrics {
    use std::io::Write;
    let conn = format!("host={} port=5432 user=postgres password=bench dbname=bench", env_str("PGHOST", "pgvector"));
    let mut cl = postgres::Client::connect(&conn, postgres::NoTls).expect("pg connect");
    cl.batch_execute("DROP TABLE IF EXISTS fiqa; CREATE TABLE fiqa (docid text, body text, tsv tsvector);").unwrap();
    let t = Instant::now();
    {
        let mut w = cl.copy_in("COPY fiqa (docid, body) FROM STDIN").unwrap();
        for (id, b) in corpus {
            let clean = b.replace('\\', " ").replace('\t', " ").replace('\n', " ").replace('\r', " ");
            write!(w, "{}\t{}\n", id, clean).unwrap();
        }
        w.finish().unwrap();
    }
    cl.batch_execute("UPDATE fiqa SET tsv = to_tsvector('english', body);").unwrap();
    cl.batch_execute("CREATE INDEX fiqa_gin ON fiqa USING gin(tsv);").unwrap();
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    let loaded = cl.query_one("SELECT count(*)::bigint FROM fiqa", &[]).unwrap().get::<_, i64>(0) as usize;
    assert_eq!(loaded, corpus.len(), "pg loaded {loaded}, expected {}", corpus.len());
    let index_mb = cl.query_one("SELECT pg_total_relation_size('fiqa')::bigint", &[]).map(|r| r.get::<_, i64>(0) as f64 / 1048576.0).unwrap_or(-1.0);

    let (mut ndcgs, mut r10, mut r100, mut times) = (vec![], vec![], vec![], vec![]);
    for (qid, qtext) in queries {
        // websearch_to_tsquery is the idiomatic PG FTS entry point: handles free-text
        // queries robustly (OR of terms, no manual escaping). ts_rank ranks the matches.
        let sql = "SELECT docid FROM fiqa WHERE tsv @@ websearch_to_tsquery('english', $1) \
                   ORDER BY ts_rank(tsv, websearch_to_tsquery('english', $1)) DESC LIMIT 100";
        let t = Instant::now();
        let rows = cl.query(sql, &[qtext]).unwrap();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let ranked: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
        let rels = &qrels[qid];
        ndcgs.push(ndcg_at_k(&ranked, rels, k)); r10.push(recall_at_k(&ranked, rels, 10)); r100.push(recall_at_k(&ranked, rels, 100));
    }
    let sv = times.clone(); // local socket: server time ≈ wall-clock
    summarize(build_ms, index_mb, &ndcgs, &r10, &r100, times, sv)
}

// ── DuckDB FTS (fts extension, match_bm25) ───────────────────────────────────
fn run_duckdb(corpus: &[(String, String)], queries: &[(String, String)],
              qrels: &HashMap<String, HashMap<String, u32>>, k: usize) -> Metrics {
    use duckdb::arrow::array::{Array, StringArray, Float64Array};
    let c = duckdb::Connection::open_in_memory().unwrap();
    c.execute_batch("INSTALL fts; LOAD fts; CREATE TABLE fiqa (docid VARCHAR, body VARCHAR);").unwrap();
    let t = Instant::now();
    {
        let mut app = c.appender("fiqa").unwrap();
        for (id, b) in corpus { app.append_row(duckdb::params![id, b]).unwrap(); }
    }
    c.execute_batch("PRAGMA create_fts_index('fiqa', 'docid', 'body', overwrite=1);").unwrap();
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;

    let (mut ndcgs, mut r10, mut r100, mut times) = (vec![], vec![], vec![], vec![]);
    for (qid, qtext) in queries {
        let q = qtext.replace('\'', " ");
        let sql = format!(
            "SELECT docid FROM (SELECT docid, fts_main_fiqa.match_bm25(docid, '{q}') AS score FROM fiqa) sq \
             WHERE score IS NOT NULL ORDER BY score DESC LIMIT 100");
        let t = Instant::now();
        let mut ranked = Vec::new();
        let mut st = c.prepare(&sql).unwrap();
        let mut rows = st.query_arrow([]).unwrap();
        for batch in &mut rows {
            let ids = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
            for i in 0..batch.num_rows() { ranked.push(ids.value(i).to_string()); }
        }
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let _ = std::any::type_name::<Float64Array>();
        let rels = &qrels[qid];
        ndcgs.push(ndcg_at_k(&ranked, rels, k)); r10.push(recall_at_k(&ranked, rels, 10)); r100.push(recall_at_k(&ranked, rels, 100));
    }
    let sv = times.clone(); // embedded: no network, server time = wall-clock
    summarize(build_ms, -1.0, &ndcgs, &r10, &r100, times, sv)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let engine = args.iter().position(|a| a == "--engine")
        .and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| "sekejap".into());
    let k = 10usize;
    let n = env_str("N", "0").parse().unwrap_or(0);

    eprintln!("[searchbench] loading FiQA…");
    let corpus = load_corpus(n);
    let qrels = load_qrels();
    let queries = load_test_queries(&qrels);
    eprintln!("[searchbench] corpus={} queries={} qrels={}", corpus.len(), queries.len(), qrels.len());

    println!("engine,n,nq,k,build_ms,ndcg10,recall10,recall100,qps,p50_ms,p99_ms,server_p50_ms,index_mb");
    let m = match engine.as_str() {
        "sekejap" => run_sekejap(&corpus, &queries, &qrels, k),
        "es" | "elasticsearch" => run_es(&corpus, &queries, &qrels, k),
        "meili" | "meilisearch" => run_meili(&corpus, &queries, &qrels, k),
        "solr" => run_solr(&corpus, &queries, &qrels, k),
        "pgfts" | "postgres" => run_pgfts(&corpus, &queries, &qrels, k),
        "duckdb" => run_duckdb(&corpus, &queries, &qrels, k),
        other => { eprintln!("[searchbench] engine '{other}' not wired yet"); return; }
    };
    println!("{engine},{},{},{k},{:.1},{:.4},{:.4},{:.4},{:.1},{:.4},{:.4},{:.4},{:.1}",
        corpus.len(), queries.len(), m.build_ms, m.ndcg10, m.recall10, m.recall100, m.qps, m.p50, m.p99, m.server_p50, m.index_mb);
}
