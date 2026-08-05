//! graphbench — GRAPH benchmark: sekejap (atomic + MATCH) vs Neo4j (Cypher/HTTP) vs
//! ArangoDB (AQL/HTTP). One engine per process for clean RSS.
//!
//! Datasets (--dataset): ldbc (9.9k nodes / 180k KNOWS edges, labeled) | amazon
//! (~262k nodes / 926k co-purchase edges, topology-only).
//!
//! Queries — directed traversal from a fixed high-degree SEED (chosen by the harness
//! so it's the same node for every engine), plus a shortest path to a fixed far DST:
//!   q1 1hop  = # distinct nodes WITHIN 1 hop (BFS dist 1..1)
//!   q2 2hop  = ... within 2 hops (dist 1..2)
//!   q3 3hop  = ... within 3 hops (dist 1..3)   [within-k neighborhood, not exactly-k]
//!   q4 spath = shortest-path length SEED->DST (directed)
//! The harness computes SEED/DST and the EXPECTED answers itself (frontier expansion +
//! BFS on an in-memory adjacency built from the CSV) so every engine is checked for
//! correctness, not just speed.
//!
//! METHODOLOGY: CREATE schema -> load nodes+edges -> CREATE INDEX -> warmup -> measure
//! p50/p99. load_ms/index_ms reported separately, excluded from query latency. Neo4j and
//! Arango are SERVERS: their latency includes client<->pod HTTP round-trip and their RSS
//! is the pod's (not this process) — disclosed in the report.
//!
//! CSV out: engine,dataset,load_ms,index_ms,rss_mb,query,p50_ms,p99_ms,result

use sekejap::CoreDB;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Set true if ANY query's result != the harness-computed expected answer. A run
/// with a mismatch exits nonzero so a wrong answer can never be treated as a valid
/// benchmark result (audit requirement — correctness gates timing).
static MISMATCH: AtomicBool = AtomicBool::new(false);

const GRAPH_DIR: &str = "data/prepared/graph";
const RUNS: &str = "data/runs/graph";

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}
fn env_str(k: &str, d: &str) -> String { std::env::var(k).unwrap_or_else(|_| d.to_string()) }

fn vmrss_mb() -> f64 { status_mb("VmRSS:") }
fn vmhwm_mb() -> f64 { status_mb("VmHWM:") }
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
        let mut t = 0;
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let m = e.metadata().unwrap();
                t += if m.is_dir() { w(&e.path()) } else { m.len() };
            }
        }
        t
    }
    w(std::path::Path::new(p)) as f64 / 1_048_576.0
}

fn fmt(v: f64) -> String { format!("{v:.4}") }
fn emit(engine: &str, ds: &str, load: f64, index: f64, rss: f64, q: &str, p50: &str, p99: &str, res: i64) {
    println!("{engine},{ds},{load:.1},{index:.1},{rss:.1},{q},{p50},{p99},{res}");
}

/// Run `f` warmup+iters times; return (p50, p99, last_result).
fn measure<F: FnMut() -> i64>(mut f: F, warmup: usize, iters: usize) -> (f64, f64, i64) {
    for _ in 0..warmup { f(); }
    let mut ts = Vec::with_capacity(iters);
    let mut last = 0;
    for _ in 0..iters {
        let t = Instant::now();
        last = f();
        ts.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| ts[((ts.len() as f64 - 1.0) * q).round() as usize];
    (p(0.5), p(0.99), last)
}

// ── Harness-side graph (for picking params + expected answers) ─────────────────
struct Graph {
    nodes: Vec<u64>,
    edges: Vec<(u64, u64)>,
    adj: HashMap<u64, Vec<u64>>,
}

fn read_lines(path: &str) -> impl Iterator<Item = String> {
    let f = std::fs::File::open(path).unwrap_or_else(|_| panic!("open {path}"));
    BufReader::new(f).lines().map_while(Result::ok)
}

fn load_graph(dataset: &str, limit: usize) -> Graph {
    let mut edges: Vec<(u64, u64)> = Vec::new();
    let mut nodeset: HashSet<u64> = HashSet::new();
    match dataset {
        "ldbc" => {
            for (i, line) in read_lines(&format!("{GRAPH_DIR}/ldbc_knows.csv")).enumerate() {
                if i == 0 { continue; } // header
                if edges.len() >= limit { break; }
                let mut it = line.split(',');
                if let (Some(a), Some(b)) = (it.next(), it.next()) {
                    if let (Ok(a), Ok(b)) = (a.trim().parse(), b.trim().parse()) {
                        edges.push((a, b)); nodeset.insert(a); nodeset.insert(b);
                    }
                }
            }
            for (i, line) in read_lines(&format!("{GRAPH_DIR}/ldbc_person.csv")).enumerate() {
                if i == 0 { continue; }
                if let Some(id) = line.split(',').next() {
                    if let Ok(id) = id.trim().parse() { nodeset.insert(id); }
                }
            }
        }
        "amazon" => {
            for (i, line) in read_lines(&format!("{GRAPH_DIR}/snap_amazon_edges.csv")).enumerate() {
                if i == 0 { continue; }
                if edges.len() >= limit { break; }
                let mut it = line.split(',');
                if let (Some(a), Some(b)) = (it.next(), it.next()) {
                    if let (Ok(a), Ok(b)) = (a.trim().parse(), b.trim().parse()) {
                        edges.push((a, b)); nodeset.insert(a); nodeset.insert(b);
                    }
                }
            }
        }
        _ => panic!("unknown dataset {dataset}"),
    }
    let mut adj: HashMap<u64, Vec<u64>> = HashMap::new();
    for &(a, b) in &edges { adj.entry(a).or_default().push(b); }
    let mut nodes: Vec<u64> = nodeset.into_iter().collect();
    nodes.sort_unstable();
    Graph { nodes, edges, adj }
}

/// SEED = max out-degree node. exp_k = # distinct nodes within k hops (BFS distance
/// 1..=k) — the standard k-hop neighborhood (visited-set BFS, no path explosion).
/// DST = farthest node from SEED; exp_path = its BFS distance (shortest path length).
struct Params { seed: u64, dst: u64, exp1: i64, exp2: i64, exp3: i64, exp_path: i64 }

fn pick_params(g: &Graph) -> Params {
    let seed = *g.adj.iter().max_by_key(|(_, v)| v.len()).map(|(k, _)| k).unwrap();
    let empty: Vec<u64> = Vec::new();
    // Full BFS from seed → shortest-path distance to every reachable node.
    let mut dist: HashMap<u64, i64> = HashMap::from([(seed, 0)]);
    let mut q = VecDeque::from([seed]);
    let (mut dst, mut best) = (seed, 0i64);
    while let Some(n) = q.pop_front() {
        let d = dist[&n];
        for &m in g.adj.get(&n).unwrap_or(&empty) {
            if !dist.contains_key(&m) {
                dist.insert(m, d + 1);
                if d + 1 > best { best = d + 1; dst = m; }
                q.push_back(m);
            }
        }
    }
    // within-k neighborhood = nodes with 1 <= dist <= k.
    let within = |k: i64| dist.values().filter(|&&d| d >= 1 && d <= k).count() as i64;
    Params { seed, dst, exp1: within(1), exp2: within(2), exp3: within(3), exp_path: best }
}

fn node_label(ds: &str) -> &'static str { let _ = ds; "N" }

fn main() {
    let engine = std::env::args().skip_while(|a| a != "--engine").nth(1).unwrap_or_default();
    let dataset = std::env::args().skip_while(|a| a != "--dataset").nth(1).unwrap_or_else(|| "ldbc".into());
    let warmup = env_usize("WARMUP", 2);
    let iters = env_usize("ITERS", 5);
    let limit = env_usize("N", usize::MAX);

    eprintln!("[graphbench] loading {dataset} (harness adjacency)…");
    let g = load_graph(&dataset, limit);
    let p = pick_params(&g);
    eprintln!("[graphbench] {} nodes, {} edges; SEED={} DST={} exp1={} exp2={} exp3={} exp_path={}",
              g.nodes.len(), g.edges.len(), p.seed, p.dst, p.exp1, p.exp2, p.exp3, p.exp_path);
    println!("engine,dataset,load_ms,index_ms,rss_mb,query,p50_ms,p99_ms,result");

    match engine.as_str() {
        "sekejap" => run_sekejap(&dataset, &g, &p, warmup, iters),
        "neo4j"   => run_neo4j(&dataset, &g, &p, warmup, iters),
        "arango"  => run_arango(&dataset, &g, &p, warmup, iters),
        _ => { eprintln!("usage: graphbench --engine sekejap|neo4j|arango --dataset ldbc|amazon"); std::process::exit(2); }
    }
    if MISMATCH.load(Ordering::Relaxed) {
        eprintln!("[graphbench] FAIL: at least one result != expected — run is INVALID");
        std::process::exit(1);
    }
    eprintln!("[graphbench] done ({engine}/{dataset}) — all results verified correct");
}

// ── sekejap ────────────────────────────────────────────────────────────────────
fn run_sekejap(ds: &str, g: &Graph, p: &Params, warmup: usize, iters: usize) {
    let dir = format!("{RUNS}/sekejap-{ds}");
    let _ = std::fs::remove_dir_all(&dir); std::fs::create_dir_all(&dir).unwrap();
    let mut db = CoreDB::open(&dir).expect("open");
    db.execute("CREATE TABLE N (_key TEXT PRIMARY KEY)").ok();

    let t = Instant::now();
    // nodes
    let node_pairs: Vec<(String, String)> = g.nodes.iter().map(|id| {
        (format!("N/{id}"), format!("{{\"_collection\":\"N\",\"_key\":\"{id}\"}}"))
    }).collect();
    db.put_many(node_pairs.iter().map(|(s, j)| (s.as_str(), j.as_str()))).expect("put nodes");
    // edges — bulk link_many (defers WAL fsync, flushes once) is the import fast lane.
    let edge_tuples: Vec<(String, String)> = g.edges.iter()
        .map(|(a, b)| (format!("N/{a}"), format!("N/{b}"))).collect();
    db.link_many(edge_tuples.iter().map(|(a, b)| (a.as_str(), b.as_str(), "E")));
    // LOAD-COMPLETENESS GATE: prove the FULL graph is present (the BFS oracle only checks
    // the seed component; this catches edges/nodes missing anywhere). Checked before spill,
    // while edge_count() still reads the in-RAM adjacency.
    assert_eq!(db.node_count(), g.nodes.len(), "sekejap loaded {} nodes, expected {}", db.node_count(), g.nodes.len());
    assert_eq!(db.edge_count(), g.edges.len(), "sekejap loaded {} edges, expected {}", db.edge_count(), g.edges.len());
    eprintln!("[load] nodes={} edges={} (full-graph asserts passed)", db.node_count(), db.edge_count());
    // DISK-FIRST graph: spill adjacency to mmap'd CSR, freeing the in-RAM HashMaps.
    let adj = |db: &CoreDB| db.memory_report().iter().find(|(l, _)| l.starts_with("edge_adjacency")).map(|(_, b)| *b as f64 / 1_048_576.0).unwrap_or(0.0);
    let adj_before = adj(&db);
    db.spill_edges_to_disk().ok();
    let adj_after = adj(&db);
    eprintln!("[mem] edge_adjacency {adj_before:.1}MB -> {adj_after:.1}MB (disk-first spill); VmRSS={:.1}MB", vmrss_mb());
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    let index_ms = 0.0; // edges are the native adjacency; no separate index build
    let disk = dir_mb(&dir); let rss = vmhwm_mb();

    let seed = p.seed.to_string();
    // Distinct destinations WITHIN k hops (1..k neighborhood) = # rows of a GROUP BY on the
    // dest key (sekejap's tested MATCH GROUP-BY fast path, the child_of*N shape). Matches the
    // BFS oracle `within(k)` = nodes with 1 <= dist <= k (not exactly-k).
    let khop = |k: usize| -> i64 {
        let sql = format!(
            "SELECT b._key AS k FROM MATCH (a:N)-[:E*1..{k}]->(b:N) WHERE a._key='{seed}' GROUP BY b._key");
        db.query(&sql).map(|s| s.collect().len() as i64).unwrap_or(-1)
    };
    for (qid, k, exp) in [("q1_1hop", 1usize, p.exp1), ("q2_2hop", 2, p.exp2), ("q3_3hop", 3, p.exp3)] {
        let (p50, p99, r) = measure(|| khop(k), warmup, iters);
        emit_chk("sekejap", ds, load_ms, index_ms, rss, qid, p50, p99, r, exp);
    }
    // shortest path
    let dst = p.dst.to_string();
    let spath = || -> i64 {
        let sql = format!(
            "SELECT length(r) AS l FROM MATCH SHORTEST (a:N)-[r:E*]->(b:N) WHERE a._key='{seed}' AND b._key='{dst}'");
        db.query(&sql).ok()
            .and_then(|s| s.collect().into_iter().next())
            .and_then(|h| h.payload)
            .and_then(|v| v.get("l").and_then(|x| x.as_i64()))
            .unwrap_or(-1)
    };
    let (p50, p99, r) = measure(spath, warmup, iters);
    emit_chk("sekejap", ds, load_ms, index_ms, rss, "q4_spath", p50, p99, r, p.exp_path);
}

fn emit_chk(engine: &str, ds: &str, load: f64, index: f64, rss: f64, q: &str, p50: f64, p99: f64, res: i64, exp: i64) {
    if res != exp {
        eprintln!("[graphbench] MISMATCH {engine}/{ds} {q}: got {res} expected {exp}");
        MISMATCH.store(true, Ordering::Relaxed);
    }
    emit(engine, ds, load, index, rss, q, &fmt(p50), &fmt(p99), res);
}

// ── Neo4j (HTTP transaction endpoint, Cypher) ──────────────────────────────────
fn neo4j_url() -> String { format!("http://{}:7474/db/neo4j/tx/commit", env_str("NEO4J_HOST", "neo4j")) }
fn neo4j_run(stmt: &str, params: Value) -> Value {
    let auth = format!("Basic {}", base64(&format!("neo4j:{}", env_str("NEO4J_PASS", "benchmarks"))));
    ureq::post(&neo4j_url())
        .set("Authorization", &auth)
        .set("Content-Type", "application/json")
        .send_json(json!({"statements":[{"statement":stmt,"parameters":params}]}))
        .map(|r| r.into_json::<Value>().unwrap_or(Value::Null))
        .unwrap_or_else(|e| { eprintln!("[neo4j] {e}"); Value::Null })
}
/// First scalar of the first row of the first result.
fn neo4j_scalar(v: &Value) -> i64 {
    v.get("results").and_then(|r| r.get(0))
        .and_then(|r| r.get("data")).and_then(|d| d.get(0))
        .and_then(|d| d.get("row")).and_then(|r| r.get(0))
        .and_then(|x| x.as_i64()).unwrap_or(-1)
}

fn run_neo4j(ds: &str, g: &Graph, p: &Params, warmup: usize, iters: usize) {
    // clean slate — BATCHED delete. A single `MATCH (n) DETACH DELETE n` on a large
    // graph exceeds Neo4j's transaction limits and silently leaves data behind, so a
    // re-run loads on top of leftovers → duplicate nodes → exploded edges → wrong
    // counts. Delete relationships first (bulk, cheap), then nodes; big batches.
    loop {
        neo4j_run("MATCH ()-[r]->() WITH r LIMIT 200000 DELETE r", json!({}));
        if neo4j_scalar(&neo4j_run("MATCH ()-[r]->() RETURN count(r) AS c", json!({}))) == 0 { break; }
    }
    loop {
        neo4j_run("MATCH (n) WITH n LIMIT 200000 DELETE n", json!({}));
        if neo4j_scalar(&neo4j_run("MATCH (n) RETURN count(n) AS c", json!({}))) == 0 { break; }
    }
    neo4j_run("CREATE INDEX n_id IF NOT EXISTS FOR (n:N) ON (n.id)", json!({}));
    let t = Instant::now();
    // nodes in batches
    for chunk in g.nodes.chunks(20_000) {
        let ids: Vec<i64> = chunk.iter().map(|&x| x as i64).collect();
        neo4j_run("UNWIND $ids AS id CREATE (:N {id:id})", json!({"ids": ids}));
    }
    // edges in batches (index makes the MATCH fast)
    for chunk in g.edges.chunks(20_000) {
        let rows: Vec<[i64; 2]> = chunk.iter().map(|&(a, b)| [a as i64, b as i64]).collect();
        neo4j_run("UNWIND $rows AS r MATCH (a:N {id:r[0]}),(b:N {id:r[1]}) CREATE (a)-[:E]->(b)", json!({"rows": rows}));
    }
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    let index_ms = 0.0;
    let rss = -1.0; // server; measured via pod cgroup separately

    let seed = p.seed as i64; let dst = p.dst as i64;
    for (qid, k, exp) in [("q1_1hop", 1, p.exp1), ("q2_2hop", 2, p.exp2), ("q3_3hop", 3, p.exp3)] {
        let stmt = format!("MATCH (a:N {{id:$s}})-[:E*1..{k}]->(b) RETURN count(DISTINCT b) AS c");
        let (p50, p99, r) = measure(|| neo4j_scalar(&neo4j_run(&stmt, json!({"s": seed}))), warmup, iters);
        emit_chk("neo4j", ds, load_ms, index_ms, rss, qid, p50, p99, r, exp);
    }
    let stmt = "MATCH (a:N {id:$s}),(b:N {id:$d}) MATCH pth=shortestPath((a)-[:E*]->(b)) RETURN length(pth) AS l";
    let (p50, p99, r) = measure(|| neo4j_scalar(&neo4j_run(stmt, json!({"s": seed, "d": dst}))), warmup, iters);
    emit_chk("neo4j", ds, load_ms, index_ms, rss, "q4_spath", p50, p99, r, p.exp_path);
}

// ── ArangoDB (HTTP cursor, AQL) ────────────────────────────────────────────────
fn arango_host() -> String { format!("http://{}:8529", env_str("ARANGO_HOST", "arango")) }
fn arango_auth() -> String { format!("Basic {}", base64(&format!("root:{}", env_str("ARANGO_PASS", "bench")))) }
fn arango_post(path: &str, body: Value) -> Value {
    ureq::post(&format!("{}{}", arango_host(), path))
        .set("Authorization", &arango_auth())
        .set("Content-Type", "application/json")
        .send_json(body)
        .map(|r| r.into_json::<Value>().unwrap_or(Value::Null))
        .unwrap_or_else(|e| {
            // 409 (exists) etc come back as Err with the response; ignore for setup.
            if let ureq::Error::Status(_, r) = e { r.into_json::<Value>().unwrap_or(Value::Null) } else { Value::Null }
        })
}
fn arango_aql(query: &str, bind: Value) -> Value {
    arango_post("/_api/cursor", json!({"query": query, "bindVars": bind}))
}
fn arango_scalar(v: &Value) -> i64 {
    v.get("result").and_then(|r| r.get(0)).and_then(|x| x.as_i64()).unwrap_or(-1)
}

fn run_arango(ds: &str, g: &Graph, p: &Params, warmup: usize, iters: usize) {
    // drop + recreate collections (nodes doc, edges edge-type=3)
    arango_post("/_api/collection/nodes", json!({})); // no-op if missing; delete below
    arango_post("/_api/collection", json!({"name":"nodes"}));
    arango_post("/_api/collection", json!({"name":"edges","type":3}));
    arango_aql("FOR n IN nodes REMOVE n IN nodes", json!({}));
    arango_aql("FOR e IN edges REMOVE e IN edges", json!({}));

    let t = Instant::now();
    // bulk import nodes (ndjson lines of {"_key":"id"})
    for chunk in g.nodes.chunks(50_000) {
        let body: String = chunk.iter().map(|id| format!("{{\"_key\":\"{id}\"}}\n")).collect();
        import(&format!("/_api/import?collection=nodes&type=documents"), body);
    }
    for chunk in g.edges.chunks(50_000) {
        let body: String = chunk.iter()
            .map(|(a, b)| format!("{{\"_from\":\"nodes/{a}\",\"_to\":\"nodes/{b}\"}}\n")).collect();
        import(&format!("/_api/import?collection=edges&type=documents"), body);
    }
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    let index_ms = 0.0;
    let rss = -1.0;

    let seed = format!("nodes/{}", p.seed);
    let dst = format!("nodes/{}", p.dst);
    for (qid, k, exp) in [("q1_1hop", 1, p.exp1), ("q2_2hop", 2, p.exp2), ("q3_3hop", 3, p.exp3)] {
        let q = format!("RETURN LENGTH(FOR v IN 1..{k} OUTBOUND @s edges OPTIONS {{uniqueVertices:'global', bfs:true}} RETURN 1)");
        let (p50, p99, r) = measure(|| arango_scalar(&arango_aql(&q, json!({"s": seed}))), warmup, iters);
        emit_chk("arango", ds, load_ms, index_ms, rss, qid, p50, p99, r, exp);
    }
    let q = "RETURN LENGTH(FOR v IN OUTBOUND SHORTEST_PATH @s TO @d edges RETURN 1) - 1";
    let (p50, p99, r) = measure(|| arango_scalar(&arango_aql(q, json!({"s": seed, "d": dst}))), warmup, iters);
    emit_chk("arango", ds, load_ms, index_ms, rss, "q4_spath", p50, p99, r, p.exp_path);
}

fn import(path: &str, ndjson: String) {
    let _ = ureq::post(&format!("{}{}", arango_host(), path))
        .set("Authorization", &arango_auth())
        .set("Content-Type", "text/plain")
        .send_string(&ndjson);
}

// tiny base64 (no dep)
fn base64(s: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | (*c.get(2).unwrap_or(&0) as u32);
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}
