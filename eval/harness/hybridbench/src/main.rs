//! hybridbench — dense + sparse fusion in ONE sekejap engine.
//!
//! Dataset: BEIR FiQA-2018 (57,638 docs, 648 test queries, qrels) + bge-small-en-v1.5
//! 384-d embeddings. Three retrieval modes, all in a single embedded engine over the
//! same corpus, scored by the same qrels:
//!   - BM25-only      (sparse: sekejap BM25 over `text`)
//!   - dense-only     (vector: sekejap disk-first HNSW over `emb`)
//!   - hybrid (RRF)   (reciprocal-rank fusion of the two rankings)
//! Metric: nDCG@10 + recall@{10,100}. Shows hybrid beats either signal alone — the
//! multi-model payoff that elsewhere needs a search engine + a separate vector DB + glue.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

const DATA: &str = "data/prepared/search/fiqa";
const DIM: usize = 1536; // openai/text-embedding-3-small (via OpenRouter)

// ── raw f32 reader (little-endian, row-major n×DIM) ──────────────────────────
fn read_f32(path: &str) -> (Vec<f32>, usize) {
    let raw = std::fs::read(path).expect("f32 read");
    let n = raw.len() / 4;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(f32::from_le_bytes([raw[i*4], raw[i*4+1], raw[i*4+2], raw[i*4+3]]));
    }
    (out, n / DIM)
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

fn load_ids(path: &str) -> Vec<String> {
    std::fs::read_to_string(path).expect("ids txt").lines().map(|s| s.to_string()).collect()
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

/// Weighted reciprocal-rank fusion (BEIR-standard k=60). `wa`/`wb` weight the two
/// rankings; wa=wb=1 is classic RRF. Favoring the stronger signal recovers its lead
/// when the two are imbalanced.
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

fn main() {
    use sekejap::{CoreDB, VecMetric};
    let k = 10usize;
    eprintln!("[hybrid] loading FiQA + embeddings…");
    let (cemb, nc) = read_f32(&format!("{DATA}/corpus_emb.f32"));
    let cids = load_ids(&format!("{DATA}/corpus_ids.txt"));
    let (qemb, nq) = read_f32(&format!("{DATA}/queries_emb.f32"));
    let qids = load_ids(&format!("{DATA}/queries_ids.txt"));
    assert_eq!(nc, cids.len()); assert_eq!(nq, qids.len());
    let qrels = load_qrels();

    // corpus text keyed by docid
    let cs = std::fs::read_to_string(format!("{DATA}/corpus.jsonl")).unwrap();
    let mut text: HashMap<String, String> = HashMap::new();
    for line in cs.lines() {
        if line.is_empty() { continue; }
        let id = json_str_field(line, "_id");
        let t = json_str_field(line, "title"); let x = json_str_field(line, "text");
        text.insert(id, if t.is_empty() { x } else { format!("{t} {x}") });
    }
    // query text keyed by qid
    let qs = std::fs::read_to_string(format!("{DATA}/queries.jsonl")).unwrap();
    let mut qtext: HashMap<String, String> = HashMap::new();
    for line in qs.lines() {
        if line.is_empty() { continue; }
        qtext.insert(json_str_field(line, "_id"), json_str_field(line, "text"));
    }
    eprintln!("[hybrid] corpus={nc} queries={nq} qrels={}", qrels.len());

    // ── build sekejap: text (BM25) + emb (disk-first HNSW), one engine ──
    let dir = "data/runs/hybrid/sekejap";
    let _ = std::fs::remove_dir_all(dir); std::fs::create_dir_all(dir).unwrap();
    let mut db = CoreDB::open(dir).unwrap();
    let t = Instant::now();
    db.begin_bulk();
    for (row, id) in cids.iter().enumerate() {
        let body = text.get(id).cloned().unwrap_or_default();
        let esc = body.replace('\\', "\\\\").replace('"', "\\\"");
        db.put(&format!("d/{id}"), &format!(r#"{{"_collection":"d","_key":"{id}","text":"{esc}"}}"#)).unwrap();
        let mut v = cemb[row*DIM..(row+1)*DIM].to_vec();
        norm(&mut v);
        db.put_vector(&format!("d/{id}"), "emb", &v).unwrap();
    }
    db.end_bulk();
    db.build_bm25_index("text");
    db.build_hnsw_index_metric("emb", 16, 200, VecMetric::L2).unwrap();
    eprintln!("[hybrid] indexed in {:.1}s", t.elapsed().as_secs_f64());

    // dense-weight values to sweep for weighted RRF (bm25 weight fixed = 1).
    let weights = [1.0f64, 2.0, 3.0, 5.0, 8.0];
    let (mut nb, mut nd, mut rb, mut rd) = (vec![], vec![], vec![], vec![]);
    let mut nh: Vec<Vec<f64>> = weights.iter().map(|_| vec![]).collect(); // nDCG per weight
    let mut rh: Vec<Vec<f64>> = weights.iter().map(|_| vec![]).collect();
    for (row, qid) in qids.iter().enumerate() {
        let Some(rels) = qrels.get(qid) else { continue };
        let bm = db.bm25_search("text", &qtext[qid], 100);
        let sparse: Vec<String> = bm.iter().filter_map(|(h,_)| db.slug_of(*h).and_then(|s| s.strip_prefix("d/")).map(|s| s.to_string())).collect();
        let mut qv = qemb[row*DIM..(row+1)*DIM].to_vec(); norm(&mut qv);
        let qstr: String = qv.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        let hits = db.query(&format!("SELECT _key FROM d WHERE VECTOR_NEAR(emb, [{qstr}], 100)")).map(|s| s.collect()).unwrap_or_default();
        let dense: Vec<String> = hits.iter().filter_map(|h| h.slug.strip_prefix("d/").map(|s| s.to_string())).collect();

        nb.push(ndcg_at_k(&sparse, rels, k)); rb.push(recall_at_k(&sparse, rels, 10));
        nd.push(ndcg_at_k(&dense, rels, k));  rd.push(recall_at_k(&dense, rels, 10));
        for (wi, &w) in weights.iter().enumerate() {
            let hy = rrf_w(&dense, &sparse, w, 1.0, 100); // dense weight w, sparse weight 1
            nh[wi].push(ndcg_at_k(&hy, rels, k)); rh[wi].push(recall_at_k(&hy, rels, 10));
        }
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    println!("mode,dataset,n,nq,k,ndcg10,recall10");
    println!("bm25,fiqa,{nc},{},{k},{:.4},{:.4}", nb.len(), mean(&nb), mean(&rb));
    println!("dense,fiqa,{nc},{},{k},{:.4},{:.4}", nd.len(), mean(&nd), mean(&rd));
    for (wi, &w) in weights.iter().enumerate() {
        println!("hybrid_rrf_dw{w:.0},fiqa,{nc},{},{k},{:.4},{:.4}", nh[wi].len(), mean(&nh[wi]), mean(&rh[wi]));
    }
    let _ = std::fs::remove_dir_all(dir);
}
