//! hybridmulti — hybrid (dense + sparse + RRF) in ONE multi-model engine, competitors.
//!
//! Same FiQA corpus + same precomputed `text-embedding-3-small` (1536-d) embeddings +
//! same qrels as `hybridbench` (sekejap). Two engines, each doing BOTH retrieval modes
//! plus fusion inside a single system:
//!   - DuckDB               `fts` (BM25) + `vss` (HNSW) — embedded
//!   - Postgres + pgvector  tsvector/ts_rank (FTS) + pgvector HNSW — one database
//! Fusion: weighted reciprocal-rank (RRF, k=60), dense-weight swept 1..8. Metric:
//! nDCG@10 + recall@10 from ground-truth qrels (identical scorer to hybridbench).
//!
//! Usage: hybridmulti <duckdb|pg>

use std::collections::{HashMap, HashSet};
use std::time::Instant;

const DATA: &str = "data/prepared/search/fiqa";
const DIM: usize = 1536;
const M: usize = 16;
const EFC: usize = 200;
const EF: usize = 100;
const WEIGHTS: [f64; 5] = [1.0, 2.0, 3.0, 5.0, 8.0];

// ── loaders (byte-for-byte the same as hybridbench) ──────────────────────────
fn read_f32(path: &str) -> (Vec<f32>, usize) {
    let raw = std::fs::read(path).expect("f32 read");
    let n = raw.len() / 4;
    let mut out = Vec::with_capacity(n);
    for i in 0..n { out.push(f32::from_le_bytes([raw[i*4], raw[i*4+1], raw[i*4+2], raw[i*4+3]])); }
    (out, n / DIM)
}
fn load_ids(path: &str) -> Vec<String> {
    std::fs::read_to_string(path).expect("ids txt").lines().map(|s| s.to_string()).collect()
}
fn json_str_field(line: &str, key: &str) -> String {
    let pat = format!("\"{key}\":");
    let Some(mut i) = line.find(&pat) else { return String::new() };
    i += pat.len();
    let b = line.as_bytes();
    while i < b.len() && b[i] == b' ' { i += 1; }
    if i >= b.len() || b[i] != b'"' { return String::new(); }
    i += 1;
    let mut out = String::new();
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            let nx = b[i+1];
            match nx { b'"'=>out.push('"'), b'\\'=>out.push('\\'), b'n'=>out.push('\n'), b't'=>out.push('\t'), b'/'=>out.push('/'), b'r'=>{}, _=>{out.push('\\'); out.push(nx as char);} }
            i += 2; continue;
        }
        if c == b'"' { break; }
        out.push(c as char); i += 1;
    }
    out
}
fn load_qrels() -> HashMap<String, HashMap<String, u32>> {
    let s = std::fs::read_to_string(format!("{DATA}/qrels/test.tsv")).unwrap();
    let mut m: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for (i, line) in s.lines().enumerate() {
        if i == 0 || line.is_empty() { continue; }
        let mut it = line.split('\t');
        let (Some(q), Some(d), Some(r)) = (it.next(), it.next(), it.next()) else { continue };
        let rel: u32 = r.trim().parse().unwrap_or(0);
        if rel > 0 { m.entry(q.to_string()).or_default().insert(d.to_string(), rel); }
    }
    m
}
fn ndcg_at_k(ranked: &[String], rels: &HashMap<String, u32>, k: usize) -> f64 {
    let mut dcg = 0.0;
    for (i, d) in ranked.iter().take(k).enumerate() {
        if let Some(&r) = rels.get(d) { dcg += ((2f64.powi(r as i32)) - 1.0) / ((i + 2) as f64).log2(); }
    }
    let mut ideal: Vec<u32> = rels.values().copied().collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let mut idcg = 0.0;
    for (i, &r) in ideal.iter().take(k).enumerate() { idcg += ((2f64.powi(r as i32)) - 1.0) / ((i + 2) as f64).log2(); }
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}
fn recall_at_k(ranked: &[String], rels: &HashMap<String, u32>, k: usize) -> f64 {
    if rels.is_empty() { return 0.0; }
    let top: HashSet<&String> = ranked.iter().take(k).collect();
    rels.keys().filter(|d| top.contains(d)).count() as f64 / rels.len() as f64
}
fn rrf_w(a: &[String], b: &[String], wa: f64, wb: f64, k: usize) -> Vec<String> {
    let mut score: HashMap<&str, f64> = HashMap::new();
    for (r, d) in a.iter().enumerate() { *score.entry(d).or_default() += wa / (60.0 + (r + 1) as f64); }
    for (r, d) in b.iter().enumerate() { *score.entry(d).or_default() += wb / (60.0 + (r + 1) as f64); }
    let mut v: Vec<(&str, f64)> = score.into_iter().collect();
    v.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap());
    v.into_iter().take(k).map(|(d, _)| d.to_string()).collect()
}
fn norm(v: &mut [f32]) {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 { for x in v.iter_mut() { *x /= n; } }
}
fn vec_lit(v: &[f32]) -> String {
    // "[f,f,...]" — accepted by both pgvector (::vector) and DuckDB (::FLOAT[DIM]).
    let mut s = String::with_capacity(v.len() * 8);
    s.push('[');
    for (i, x) in v.iter().enumerate() { if i > 0 { s.push(','); } s.push_str(&format!("{x}")); }
    s.push(']');
    s
}

struct Loaded {
    cids: Vec<String>, cbody: Vec<String>, cemb: Vec<f32>,   // corpus (nc rows, emb nc*DIM, normalized)
    qids: Vec<String>, qtext: HashMap<String, String>, qemb: Vec<f32>,
    qrels: HashMap<String, HashMap<String, u32>>,
}
fn load_all() -> Loaded {
    let (mut cemb, nc) = read_f32(&format!("{DATA}/corpus_emb.f32"));
    let cids = load_ids(&format!("{DATA}/corpus_ids.txt"));
    let (mut qemb, nq) = read_f32(&format!("{DATA}/queries_emb.f32"));
    let qids = load_ids(&format!("{DATA}/queries_ids.txt"));
    assert_eq!(nc, cids.len(), "corpus emb rows {nc} != ids {}", cids.len());
    assert_eq!(nq, qids.len(), "query emb rows {nq} != ids {}", qids.len());
    for r in 0..nc { norm(&mut cemb[r*DIM..(r+1)*DIM]); }
    for r in 0..nq { norm(&mut qemb[r*DIM..(r+1)*DIM]); }
    // corpus text keyed by docid, then aligned to cids order
    let cs = std::fs::read_to_string(format!("{DATA}/corpus.jsonl")).unwrap();
    let mut text: HashMap<String, String> = HashMap::new();
    for line in cs.lines() {
        if line.is_empty() { continue; }
        let id = json_str_field(line, "_id");
        let t = json_str_field(line, "title"); let x = json_str_field(line, "text");
        text.insert(id, if t.is_empty() { x } else { format!("{t} {x}") });
    }
    let cbody: Vec<String> = cids.iter().map(|id| text.get(id).cloned().unwrap_or_default()).collect();
    let qs = std::fs::read_to_string(format!("{DATA}/queries.jsonl")).unwrap();
    let mut qtext: HashMap<String, String> = HashMap::new();
    for line in qs.lines() {
        if line.is_empty() { continue; }
        qtext.insert(json_str_field(line, "_id"), json_str_field(line, "text"));
    }
    let qrels = load_qrels();
    eprintln!("[hybridmulti] corpus={nc} queries={nq} qrels={}", qrels.len());
    Loaded { cids, cbody, cemb, qids, qtext, qemb, qrels }
}

// aggregate + print one engine's CSV block from per-query rankings
fn report(engine: &str, l: &Loaded, sparse_by_q: &[Vec<String>], dense_by_q: &[Vec<String>]) {
    let k = 10usize;
    let (mut nb, mut nd, mut rb, mut rd) = (vec![], vec![], vec![], vec![]);
    let mut nh: Vec<Vec<f64>> = WEIGHTS.iter().map(|_| vec![]).collect();
    let mut rh: Vec<Vec<f64>> = WEIGHTS.iter().map(|_| vec![]).collect();
    for (i, qid) in l.qids.iter().enumerate() {
        let Some(rels) = l.qrels.get(qid) else { continue };
        let sparse = &sparse_by_q[i];
        let dense = &dense_by_q[i];
        nb.push(ndcg_at_k(sparse, rels, k)); rb.push(recall_at_k(sparse, rels, 10));
        nd.push(ndcg_at_k(dense, rels, k));  rd.push(recall_at_k(dense, rels, 10));
        for (wi, &w) in WEIGHTS.iter().enumerate() {
            let hy = rrf_w(dense, sparse, w, 1.0, 100);
            nh[wi].push(ndcg_at_k(&hy, rels, k)); rh[wi].push(recall_at_k(&hy, rels, 10));
        }
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let n = l.cids.len();
    println!("{engine},bm25,fiqa,{n},{},{k},{:.4},{:.4}", nb.len(), mean(&nb), mean(&rb));
    println!("{engine},dense,fiqa,{n},{},{k},{:.4},{:.4}", nd.len(), mean(&nd), mean(&rd));
    for (wi, &w) in WEIGHTS.iter().enumerate() {
        println!("{engine},hybrid_rrf_dw{w:.0},fiqa,{n},{},{k},{:.4},{:.4}", nh[wi].len(), mean(&nh[wi]), mean(&rh[wi]));
    }
}

// ── DuckDB: fts (BM25) + vss (HNSW), one embedded connection ─────────────────
fn run_duckdb(l: &Loaded) {
    use duckdb::arrow::array::{Array, StringArray};
    let c = duckdb::Connection::open_in_memory().unwrap();
    c.execute_batch(
        "INSTALL fts; LOAD fts; INSTALL vss; LOAD vss; \
         SET hnsw_enable_experimental_persistence=false; \
         CREATE TABLE raw (docid VARCHAR, body VARCHAR, emb VARCHAR);").unwrap();
    let t = Instant::now();
    {
        let mut app = c.appender("raw").unwrap();
        for (i, id) in l.cids.iter().enumerate() {
            let lit = vec_lit(&l.cemb[i*DIM..(i+1)*DIM]);
            app.append_row(duckdb::params![id, &l.cbody[i], &lit]).unwrap();
        }
    }
    // string → FLOAT[DIM]: split the "[a,b,...]" literal into a list, cast to fixed array.
    c.execute_batch(&format!(
        "CREATE TABLE d AS SELECT docid, body, \
            CAST(string_split(trim(emb, '[]'), ',') AS FLOAT[{DIM}]) AS emb FROM raw; \
         DROP TABLE raw;")).unwrap();
    let loaded: i64 = c.query_row("SELECT count(*) FROM d", [], |r| r.get(0)).unwrap();
    assert_eq!(loaded as usize, l.cids.len(), "duckdb loaded {loaded}, expected {}", l.cids.len());
    c.execute_batch("PRAGMA create_fts_index('d', 'docid', 'body', overwrite=1);").unwrap();
    c.execute_batch(&format!(
        "CREATE INDEX idx ON d USING HNSW (emb) WITH (metric='l2sq', ef_construction={EFC}, M={M});")).unwrap();
    c.execute_batch(&format!("SET hnsw_ef_search={EF};")).ok();
    eprintln!("[duckdb] built (fts+hnsw) in {:.1}s", t.elapsed().as_secs_f64());

    let mut sparse_by_q = Vec::with_capacity(l.qids.len());
    let mut dense_by_q = Vec::with_capacity(l.qids.len());
    for (i, qid) in l.qids.iter().enumerate() {
        // sparse: BM25
        let q = l.qtext[qid].replace('\'', " ");
        let ssql = format!(
            "SELECT docid FROM (SELECT docid, fts_main_d.match_bm25(docid, '{q}') AS score FROM d) sq \
             WHERE score IS NOT NULL ORDER BY score DESC LIMIT 100");
        let mut sparse = Vec::new();
        {
            let mut st = c.prepare(&ssql).unwrap();
            let mut rows = st.query_arrow([]).unwrap();
            for batch in &mut rows {
                let ids = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
                for j in 0..batch.num_rows() { sparse.push(ids.value(j).to_string()); }
            }
        }
        // dense: HNSW over l2sq (vectors normalized ⇒ ranks as cosine)
        let lit = vec_lit(&l.qemb[i*DIM..(i+1)*DIM]);
        let dsql = format!(
            "SELECT docid FROM d ORDER BY array_distance(emb, {lit}::FLOAT[{DIM}]) LIMIT 100");
        let mut dense = Vec::new();
        {
            let mut st = c.prepare(&dsql).unwrap();
            let mut rows = st.query_arrow([]).unwrap();
            for batch in &mut rows {
                let ids = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
                for j in 0..batch.num_rows() { dense.push(ids.value(j).to_string()); }
            }
        }
        sparse_by_q.push(sparse); dense_by_q.push(dense);
    }
    report("duckdb", l, &sparse_by_q, &dense_by_q);
}

// ── Postgres + pgvector: tsvector/ts_rank (FTS) + pgvector HNSW, one database ─
fn run_pg(l: &Loaded) {
    let host = std::env::var("PGVHOST").unwrap_or_else(|_| "pgvector".to_string());
    let conn = format!("host={host} port=5432 user=postgres password=bench dbname=bench");
    let mut cl = postgres::Client::connect(&conn, postgres::NoTls).expect("pgvector connect");
    cl.batch_execute(
        "CREATE EXTENSION IF NOT EXISTS vector; \
         SET maintenance_work_mem='2GB'; \
         DROP TABLE IF EXISTS d; \
         CREATE TABLE d (docid text, body text, tsv tsvector, emb vector(1536));").unwrap();
    let t = Instant::now();
    {
        let mut w = cl.copy_in("COPY d (docid, body, emb) FROM STDIN").unwrap();
        use std::io::Write;
        for (i, id) in l.cids.iter().enumerate() {
            let body = l.cbody[i].replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r");
            let lit = vec_lit(&l.cemb[i*DIM..(i+1)*DIM]);
            w.write_all(format!("{id}\t{body}\t{lit}\n").as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }
    let loaded: i64 = cl.query_one("SELECT count(*) FROM d", &[]).unwrap().get(0);
    assert_eq!(loaded as usize, l.cids.len(), "pg loaded {loaded}, expected {}", l.cids.len());
    cl.batch_execute(
        "UPDATE d SET tsv = to_tsvector('english', body); \
         CREATE INDEX d_tsv ON d USING gin(tsv);").unwrap();
    eprintln!("[pg] copied+tsv in {:.1}s, building HNSW…", t.elapsed().as_secs_f64());
    cl.batch_execute(&format!(
        "CREATE INDEX d_emb ON d USING hnsw (emb vector_l2_ops) WITH (m={M}, ef_construction={EFC});")).unwrap();
    cl.batch_execute(&format!("SET hnsw.ef_search={EF};")).unwrap();
    eprintln!("[pg] built in {:.1}s", t.elapsed().as_secs_f64());

    let mut sparse_by_q = Vec::with_capacity(l.qids.len());
    let mut dense_by_q = Vec::with_capacity(l.qids.len());
    let ssql = "SELECT docid FROM d WHERE tsv @@ websearch_to_tsquery('english', $1) \
                ORDER BY ts_rank(tsv, websearch_to_tsquery('english', $1)) DESC LIMIT 100";
    for (i, qid) in l.qids.iter().enumerate() {
        let qt = &l.qtext[qid];
        let sparse: Vec<String> = cl.query(ssql, &[qt]).unwrap().iter().map(|r| r.get::<_, String>(0)).collect();
        let lit = vec_lit(&l.qemb[i*DIM..(i+1)*DIM]);
        let dsql = format!("SELECT docid FROM d ORDER BY emb <-> '{lit}'::vector LIMIT 100");
        let dense: Vec<String> = cl.query(&dsql, &[]).unwrap().iter().map(|r| r.get::<_, String>(0)).collect();
        sparse_by_q.push(sparse); dense_by_q.push(dense);
    }
    report("pgvector", l, &sparse_by_q, &dense_by_q);
}

fn main() {
    let engine = std::env::args().nth(1).unwrap_or_default();
    let l = load_all();
    println!("engine,mode,dataset,n,nq,k,ndcg10,recall10");
    match engine.as_str() {
        "duckdb" => run_duckdb(&l),
        "pg" | "pgvector" => run_pg(&l),
        other => { eprintln!("unknown engine '{other}' (use duckdb|pg)"); std::process::exit(2); }
    }
}
