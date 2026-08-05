//! RAM-reduction training harness for the disk-first int8 vector index.
//!
//! Isolated (no CoreDB/WAL/payload noise): builds the HNSW graph + int8 store for
//! N synthetic 128-d vectors, prints a byte-exact memory breakdown, and verifies
//! search recall so no RAM win is bought with accuracy. Deterministic → runs fast
//! locally, no /proc needed. `cargo run --release --example ram_profile [N] [ef]`.

use std::collections::HashSet;
use sekejap::vector::{CompactDiskIndex, HnswGraph, L2Distance, QuantizedField, ScalarQuantizer};

fn vec_for(i: usize, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|j| {
            let mut x = (i as u64)
                .wrapping_mul(6364136223846793005)
                .wrapping_add((j as u64).wrapping_mul(1442695040888963407));
            x ^= x >> 12; x ^= x << 25; x ^= x >> 27;
            ((x.wrapping_mul(0x2545F4914F6CDD1D) >> 33) as f32) / (1u64 << 31) as f32 * 100.0
        })
        .collect()
}
fn l2(a: &[f32], b: &[f32]) -> f32 { a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum() }

// A simple in-RAM QuantAccess over a flat code buffer (mirrors QuantizedField access).
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let ef: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    let dim = 128usize;
    let k = 10usize;

    let base: Vec<Vec<f32>> = (0..n).map(|i| vec_for(i, dim)).collect();

    // Build graph on f32 (pass the flat buffer + dense ids directly).
    let ids: Vec<u64> = (0..n as u64).collect();
    let mut flat = Vec::with_capacity(n * dim);
    for v in &base { flat.extend_from_slice(v); }
    let graph = HnswGraph::build_dense_parallel::<L2Distance>(&flat, dim, &ids, 16, 200);

    // Quantize to int8.
    let mut sample: Vec<f32> = flat.iter().copied().step_by((flat.len() / 200_000).max(1)).collect();
    let quantizer = ScalarQuantizer::calibrate(&mut sample);
    let mut qf = QuantizedField::with_capacity(quantizer, dim, n);
    for (i, v) in base.iter().enumerate() { qf.insert(i as u64, v); }

    let graph_b = graph.mem_bytes();
    let int8_b = qf.mem_bytes();
    let mb = |b: usize| b as f64 / 1_048_576.0;

    // Recall check (int8 traversal + f32 rescore, oversample 8).
    let qn = 100usize;
    let mut hits = 0usize;
    for qi in 0..qn {
        // In-distribution query: a base vector perturbed slightly (spread across base).
        let src = (qi * 5077) % n;
        let mut q = base[src].clone();
        for (j, x) in q.iter_mut().enumerate() {
            *x += ((qi + j) % 7) as f32 - 3.0; // small deterministic perturbation
        }
        let mut all: Vec<(usize, f32)> = base.iter().enumerate().map(|(i, v)| (i, l2(&q, v))).collect();
        all.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let truth: HashSet<usize> = all.iter().take(k).map(|(i, _)| *i).collect();
        let qcode = qf.quantize_query(&q);
        let approx = graph.search_quant(&qcode, &qf, k * 8, ef.max(k * 8));
        let mut resc: Vec<(usize, f32)> = approx.iter().map(|&id| (id as usize, l2(&q, &base[id as usize]))).collect();
        resc.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let got: HashSet<usize> = resc.iter().take(k).map(|(i, _)| *i).collect();
        hits += truth.intersection(&got).count();
    }
    let recall = hits as f64 / (qn * k) as f64;

    println!("N={n} dim={dim} ef={ef}");
    println!("graph_bytes = {} ({:.1} MB)  [{:.0} B/node]", graph_b, mb(graph_b), graph_b as f64 / n as f64);
    println!("int8_bytes  = {} ({:.1} MB)  [{:.0} B/node]", int8_b, mb(int8_b), int8_b as f64 / n as f64);
    println!("ENGINE_TOTAL = {:.1} MB  (graph {:.0}% + int8 {:.0}%)",
        mb(graph_b + int8_b),
        graph_b as f64 / (graph_b + int8_b) as f64 * 100.0,
        int8_b as f64 / (graph_b + int8_b) as f64 * 100.0);
    println!("recall@{k} = {recall:.4}  (fat graph+int8)");
    println!("PROJECTED_1M(fat graph+int8) = {:.0} MB", mb((graph_b + int8_b) / n * 1_000_000));

    // ── COMPACT index: convert graph+int8 → CSR slot-indexed, measure + recall ──
    let compact = CompactDiskIndex::from_hnsw(&graph, &qf, dim);
    let comp_b = compact.mem_bytes();
    let mut chits = 0usize;
    for qi in 0..qn {
        let src = (qi * 5077) % n;
        let mut q = base[src].clone();
        for (j, x) in q.iter_mut().enumerate() { *x += ((qi + j) % 7) as f32 - 3.0; }
        let mut all: Vec<(usize, f32)> = base.iter().enumerate().map(|(i, v)| (i, l2(&q, v))).collect();
        all.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let truth: HashSet<usize> = all.iter().take(k).map(|(i, _)| *i).collect();
        let qcode = compact.quantize_query(&q);
        let approx = compact.search(&qcode, k * 8, ef.max(k * 8));
        let mut resc: Vec<(usize, f32)> = approx.iter().map(|&id| (id as usize, l2(&q, &base[id as usize]))).collect();
        resc.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let got: HashSet<usize> = resc.iter().take(k).map(|(i, _)| *i).collect();
        chits += truth.intersection(&got).count();
    }
    let crecall = chits as f64 / (qn * k) as f64;
    println!("\n=== COMPACT (CSR slot-indexed) ===");
    println!("compact_bytes = {} ({:.1} MB)  [{:.0} B/node]  vs fat {:.0} B/node", comp_b, mb(comp_b), comp_b as f64 / n as f64, (graph_b + int8_b) as f64 / n as f64);
    println!("compact recall@{k} = {crecall:.4}  (was {recall:.4})");
    println!("COMPACT_PROJECTED_1M = {:.0} MB  (was {:.0} MB)", mb(comp_b / n * 1_000_000), mb((graph_b + int8_b) / n * 1_000_000));

    // ── Full CoreDB breakdown (the REAL engine RAM, incl. node/payload metadata) ──
    println!("\n=== FULL CoreDB memory_report (disk-first, N={n}) ===");
    let dir = std::env::temp_dir().join(format!("ramprof-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut db = sekejap::CoreDB::open(&dir).unwrap();
    db.begin_bulk();
    for i in 0..n {
        db.put(&format!("sift/{i}"), &format!(r#"{{"_collection":"sift","_key":"{i}"}}"#)).unwrap();
        db.put_vector(&format!("sift/{i}"), "emb", &base[i]).unwrap();
    }
    db.end_bulk();
    db.build_hnsw_index_disk("emb", 16, 200, sekejap::VecMetric::L2).unwrap();
    let report = db.memory_report();
    let engine_total: usize = report.iter()
        .filter(|(l, _)| !l.starts_with('_'))
        .map(|(_, b)| *b).sum();
    for (label, bytes) in &report {
        if label.starts_with('_') { println!("  {label} = {bytes} bytes"); continue; }
        println!("  {:<40} {:>8.1} MB  ({:.0} B/node)", label, mb(*bytes), *bytes as f64 / n as f64);
    }
    println!("  {:-<40} {:>8.1} MB", "ENGINE TOTAL", mb(engine_total));
    println!("  PROJECTED_1M(full engine) = {:.0} MB", mb(engine_total / n * 1_000_000));
    let _ = std::fs::remove_dir_all(&dir);
}
