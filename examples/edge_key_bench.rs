//! Edge-key model benchmark: baseline (additive, keyless) vs keyed-upsert, across
//! the dimensions that decide billion-scale viability.
//!
//!   1. Per-edge MEMORY by key representation (the RAM story at 1B)
//!   2. Key-HASH throughput by key type (u64 / short str / uuid)
//!   3. UPSERT insert cost: append vs scan vs index — realistic + super-node
//!   4. Real sekejap TRAVERSAL: naked vs keyed edges (read-neutrality)
//!
//! Sections 1-3 are isolated microbenches (we control the data structures, so we
//! can measure memory + the O(k) find precisely). Section 4 uses the real engine.

use sekejap::CoreDB;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use std::time::Instant;

fn h64<T: Hash>(t: &T) -> u64 {
    let mut s = DefaultHasher::new();
    t.hash(&mut s);
    s.finish()
}

fn main() {
    section1_memory();
    section2_hash();
    section3_upsert();
    section4_traversal();
}

// ── 1. Per-edge memory by key representation ─────────────────────────────────

fn section1_memory() {
    println!("\n=== 1. PER-EDGE MEMORY (extrapolated to 1e9 edges) ===");
    // current hot Edge: other:u64 + edge_type:u64 + meta_id:u32  → 24 B (8-aligned)
    let naked = 24usize;
    let key_u64 = 24 + 8; // key hashed to u64, stored inline
    let key_str_short = 24 + size_of::<String>() + 13; // "user_00012345"
    let key_str_uuid = 24 + size_of::<String>() + 36;  // full UUID text
    // keyed-by-u64 via a side index instead of inline: edge stays 24 B, index entry
    // ≈ (from,type,key)=24 B key + (pos) 8 B + hashmap overhead ~1.4x load
    let idx_entry = ((24 + 8) as f64 * 1.4) as usize;

    let rows = [
        ("naked edge (today)", naked),
        ("+ u64 key inline", key_u64),
        ("+ short String key inline", key_str_short),
        ("+ UUID String key inline", key_str_uuid),
        ("u64 key via side index (edge stays 24B)", naked + idx_entry),
    ];
    let gb = |b: usize| b as f64 * 1e9 / 1024f64.powi(3);
    println!("{:<44} {:>10} {:>12}", "representation", "bytes/edge", "@1e9 (GB)");
    for (name, b) in rows {
        println!("{:<44} {:>10} {:>12.1}", name, b, gb(b));
    }
    println!("note: raw UUID *strings* per edge ≈ {:.0}GB at 1e9 vs {:.0}GB naked / {:.0}GB u64-side-index",
        gb(key_str_uuid), gb(naked), gb(naked + idx_entry));
}

// ── 2. Key-hash throughput by type ───────────────────────────────────────────

fn section2_hash() {
    println!("\n=== 2. KEY-HASH THROUGHPUT (identity = hash the key to u64) ===");
    let n = 10_000_000u64;

    let t = Instant::now();
    let mut acc = 0u64;
    for i in 0..n { acc ^= h64(&i); }
    let e_u64 = t.elapsed();

    let shorts: Vec<String> = (0..1000).map(|i| format!("user_{:08}", i)).collect();
    let t = Instant::now();
    for i in 0..n { acc ^= h64(&shorts[(i as usize) % shorts.len()]); }
    let e_short = t.elapsed();

    let uuids: Vec<String> = (0..1000)
        .map(|i| format!("550e8400-e29b-41d4-a716-{:012}", i)).collect();
    let t = Instant::now();
    for i in 0..n { acc ^= h64(&uuids[(i as usize) % uuids.len()]); }
    let e_uuid = t.elapsed();

    std::hint::black_box(acc);
    let mps = |e: std::time::Duration| n as f64 / e.as_secs_f64() / 1e6;
    println!("  u64 key   : {:>7.1} M hash/s  ({:?})", mps(e_u64), e_u64);
    println!("  short str : {:>7.1} M hash/s  ({:?})", mps(e_short), e_short);
    println!("  uuid str  : {:>7.1} M hash/s  ({:?})", mps(e_uuid), e_uuid);
    println!("  → once hashed to u64, the index/compare is type-independent");
}

// ── 3. Upsert insert cost: append vs scan vs index ───────────────────────────

fn section3_upsert() {
    println!("\n=== 3. UPSERT INSERT COST (find-before-add) ===");
    const TYPE: u64 = 42;

    // Realistic: N edges spread over many nodes (low per-node degree).
    let n = 1_000_000usize;
    let nodes = 100_000u64; // avg degree 10

    // 3a. baseline APPEND (today's behavior, no dedup)
    let mut adj: HashMap<u64, Vec<(u64, u64, u64)>> = HashMap::new();
    let t = Instant::now();
    for i in 0..n {
        let from = (i as u64) % nodes;
        adj.entry(from).or_default().push((i as u64, TYPE, h64(&i)));
    }
    println!("  [spread, deg~10] append (baseline) : {:>7.2} M ins/s", tp(n, t.elapsed()));

    // 3b. SCAN upsert (no index): find (type,key) in the node's list
    let mut adj: HashMap<u64, Vec<(u64, u64, u64)>> = HashMap::new();
    let t = Instant::now();
    for i in 0..n {
        let from = (i as u64) % nodes;
        let key = h64(&i);
        let list = adj.entry(from).or_default();
        if let Some(e) = list.iter_mut().find(|e| e.1 == TYPE && e.2 == key) {
            e.0 = i as u64; // upsert
        } else {
            list.push((i as u64, TYPE, key));
        }
    }
    println!("  [spread, deg~10] scan-upsert       : {:>7.2} M ins/s", tp(n, t.elapsed()));

    // 3c. INDEX upsert: HashMap<(from,type,key)->pos>
    let mut adj: HashMap<u64, Vec<(u64, u64, u64)>> = HashMap::new();
    let mut idx: HashMap<(u64, u64, u64), usize> = HashMap::new();
    let t = Instant::now();
    for i in 0..n {
        let from = (i as u64) % nodes;
        let key = h64(&i);
        match idx.entry((from, TYPE, key)) {
            std::collections::hash_map::Entry::Occupied(o) => {
                adj.get_mut(&from).unwrap()[*o.get()].0 = i as u64;
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                let list = adj.entry(from).or_default();
                v.insert(list.len());
                list.push((i as u64, TYPE, key));
            }
        }
    }
    println!("  [spread, deg~10] index-upsert      : {:>7.2} M ins/s", tp(n, t.elapsed()));

    // Super-node: all edges on ONE node → scan is O(N^2).
    println!("  -- super-node (all edges on one node) --");
    let sn = 20_000usize;
    let mut list: Vec<(u64, u64, u64)> = Vec::new();
    let t = Instant::now();
    for i in 0..sn {
        let key = h64(&i);
        if let Some(e) = list.iter_mut().find(|e| e.1 == TYPE && e.2 == key) { e.0 = i as u64; }
        else { list.push((i as u64, TYPE, key)); }
    }
    println!("  [super-node] scan-upsert  N={:>7}: {:>7.2} M ins/s  (O(N^2)!)", sn, tp(sn, t.elapsed()));

    let sn2 = 1_000_000usize;
    let mut list: Vec<(u64, u64, u64)> = Vec::new();
    let mut idx: HashMap<u64, usize> = HashMap::new();
    let t = Instant::now();
    for i in 0..sn2 {
        let key = h64(&i);
        match idx.entry(key) {
            std::collections::hash_map::Entry::Occupied(o) => { list[*o.get()].0 = i as u64; }
            std::collections::hash_map::Entry::Vacant(v) => { v.insert(list.len()); list.push((i as u64, TYPE, key)); }
        }
    }
    println!("  [super-node] index-upsert N={:>7}: {:>7.2} M ins/s  (O(N))", sn2, tp(sn2, t.elapsed()));
}

fn tp(n: usize, e: std::time::Duration) -> f64 { n as f64 / e.as_secs_f64() / 1e6 }

// ── 4. Real sekejap traversal: naked vs keyed edges ──────────────────────────

fn section4_traversal() {
    println!("\n=== 4. TRAVERSAL read-neutrality (real engine) ===");
    let m = 200_000u64;

    let mut db = CoreDB::new();
    build_hub(&mut db, m, false);
    let naked_proj = best(&db, PROJ);
    let naked_cnt = best(&db, CNT);

    let mut db = CoreDB::new();
    build_hub(&mut db, m, true);
    let keyed_proj = best(&db, PROJ);
    let keyed_cnt = best(&db, CNT);

    println!("  project b._key (reads node payloads):");
    println!("    naked {:?}   keyed {:?}   ratio {:.3}x", naked_proj, keyed_proj,
        keyed_proj.as_secs_f64() / naked_proj.as_secs_f64());
    println!("  COUNT(*) (pure edge walk, no node payloads):");
    println!("    naked {:?}   keyed {:?}   ratio {:.3}x", naked_cnt, keyed_cnt,
        keyed_cnt.as_secs_f64() / naked_cnt.as_secs_f64());
    println!("  → COUNT ratio ≈1.0 = keys don't touch the edge walk; any gap is meta materialization");
}

const PROJ: &str = "SELECT b._key AS k FROM MATCH (a:hub)-[:rel]->(b:t) WHERE a._key='h'";
const CNT: &str = "SELECT COUNT(*) FROM MATCH (a:hub)-[:rel]->(b:t) WHERE a._key='h'";

/// Best of 3 runs (warm cache, min noise).
fn best(db: &CoreDB, sql: &str) -> std::time::Duration {
    let mut b = std::time::Duration::from_secs(999);
    for _ in 0..3 {
        let t = Instant::now();
        let _ = db.query(sql).unwrap().collect().len();
        b = b.min(t.elapsed());
    }
    b
}

fn build_hub(db: &mut CoreDB, m: u64, keyed: bool) {
    // hub node + m targets
    let mut nodes: Vec<(String, String)> = Vec::with_capacity(m as usize + 1);
    nodes.push(("hub/h".into(), r#"{"_collection":"hub","_key":"h"}"#.into()));
    for i in 0..m {
        nodes.push((format!("t/{i}"), format!(r#"{{"_collection":"t","_key":"{i}"}}"#)));
    }
    db.put_many(nodes.iter().map(|(s, j)| (s.as_str(), j.as_str()))).unwrap();

    if keyed {
        let metas: Vec<(String, String, String, String)> = (0..m)
            .map(|i| ("hub/h".into(), format!("t/{i}"), "rel".into(), format!(r#"{{"_key":"k{i}"}}"#)))
            .collect();
        db.link_meta_many(metas.iter().map(|(f, t, ty, m)| (f.as_str(), t.as_str(), ty.as_str(), Some(m.as_str())))).unwrap();
    } else {
        let edges: Vec<(String, String, String)> = (0..m)
            .map(|i| ("hub/h".into(), format!("t/{i}"), "rel".into())).collect();
        db.link_many(edges.iter().map(|(f, t, ty)| (f.as_str(), t.as_str(), ty.as_str())));
    }
}

