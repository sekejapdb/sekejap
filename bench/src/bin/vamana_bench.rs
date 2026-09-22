//! The vamana graph against the linear quantized family, over the same rows.
//!
//! Prints one JSON object per size: build time, bytes per row in each
//! family's own keyspace, and per-query latency and recall@10 at a few search
//! list sizes. The recall oracle is brute force over the f32 lanes this
//! process generated, computed here.
//!
//! ```sh
//! cargo run -p sekejap-bench --release --bin vamana_bench -- OUT_DIR [sizes...]
//! ```
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{CollectionOptions, Database, IndexId, QuantizedVectorCandidates, VectorMetric},
    pagewal::PageWalStore,
    Kind,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    env, fs,
    path::Path,
    time::Instant,
};

const DIM: usize = 128;
const K: usize = 10;
const QUERIES: usize = 50;

fn cfg() -> Config {
    Config {
        budget_bytes: 32 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 11) as f32 / (1u64 << 53) as f32
    }
    fn vector(&mut self) -> Vec<f32> {
        (0..DIM).map(|_| self.unit() * 2.0 - 1.0).collect()
    }
}

/// Two corpus shapes, because a graph index's recall is a property of the
/// DATA as much as of the algorithm.
///
/// `uniform` is independent lanes over [-1, 1]: in 128 dimensions that is
/// near worst case for any proximity structure, since every pair of points is
/// almost equidistant and there is barely a neighbourhood to navigate.
/// `clustered` draws each row from one of 64 centres and is the shape a real
/// embedding corpus has. Both are reported; neither is the other's excuse.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Uniform,
    Clustered,
}

fn corpus(shape: Shape, rows: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    match shape {
        Shape::Uniform => (0..rows).map(|_| rng.vector()).collect(),
        Shape::Clustered => {
            let centres: Vec<Vec<f32>> = (0..64).map(|_| rng.vector()).collect();
            (0..rows)
                .map(|_| {
                    let centre = &centres[(rng.next() % 64) as usize];
                    centre
                        .iter()
                        .map(|lane| lane + (rng.unit() * 2.0 - 1.0) * 0.25)
                        .collect()
                })
                .collect()
        }
    }
}

fn l2(stored: &[f32], query: &[f32]) -> f64 {
    stored
        .iter()
        .zip(query)
        .map(|(s, q)| (f64::from(*s) - f64::from(*q)) * (f64::from(*s) - f64::from(*q)))
        .sum()
}

fn ordered(number: u64) -> Vec<u8> {
    let bytes = number.to_be_bytes();
    let start = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
    let mut out = vec![0x80 + (8 - start) as u8];
    out.extend_from_slice(&bytes[start..]);
    out
}

/// Bytes the index actually occupies in its own keyspace, key and value.
fn keyspace_bytes(path: &Path, tag: u8, index: IndexId) -> u64 {
    let raw = PageWalStore::open(path, false, 1 << 20).unwrap();
    let mut prefix = vec![tag];
    prefix.extend(ordered(index.0));
    let mut total = 0u64;
    for row in raw.range(&prefix).unwrap() {
        let (key, value) = row.unwrap();
        if !key.starts_with(&prefix) {
            break;
        }
        total += (key.len() + value.len()) as u64;
    }
    total
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let root = Path::new(args.first().expect("usage: vamana_bench OUT_DIR [sizes...]"));
    let shape = match args.get(1).map(String::as_str) {
        Some("clustered") => Shape::Clustered,
        Some("uniform") | None => Shape::Uniform,
        Some(other) => panic!("shape must be uniform|clustered, not {other}"),
    };
    let sizes: Vec<usize> = if args.len() > 2 {
        args[2..].iter().map(|n| n.parse().unwrap()).collect()
    } else {
        vec![2_000, 10_000, 50_000]
    };
    fs::create_dir_all(root).unwrap();
    for rows in sizes {
        let path = root.join(format!("n{rows}"));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        let vectors = corpus(shape, rows, 0x1234_5678_9abc_def0 ^ rows as u64);
        let mut db = Database::create(&path, cfg()).unwrap();
        let collection = db
            .create_collection(
                "points",
                vec![("embedding".into(), Kind::Vector(DIM))],
                CollectionOptions::default(),
            )
            .unwrap();
        let mut held: BTreeMap<u64, Vec<f32>> = BTreeMap::new();
        for (at, vector) in vectors.iter().enumerate() {
            let id = db
                .put(collection, &format!("k{at}"), &json!({"embedding": vector}))
                .unwrap();
            held.insert(id.sequence, vector.clone());
            if at % 2000 == 1999 {
                db.commit().unwrap();
            }
        }
        db.commit().unwrap();

        let started = Instant::now();
        let linear = db
            .create_quantized_vector_index(collection, "q", "embedding")
            .unwrap();
        db.commit().unwrap();
        while !db.build_index_step(linear, 256).unwrap() {
            db.commit().unwrap();
        }
        db.commit().unwrap();
        let linear_build = started.elapsed().as_secs_f64();

        let started = Instant::now();
        let graph = db.create_vamana_index(collection, "v", "embedding").unwrap();
        db.commit().unwrap();
        while !db.build_index_step(graph, 64).unwrap() {
            db.commit().unwrap();
        }
        db.commit().unwrap();
        let graph_build = started.elapsed().as_secs_f64();

        let probes = corpus(shape, QUERIES, 0xfeed_face_0000_0001 ^ rows as u64);
        let truth: Vec<Vec<u64>> = probes
            .iter()
            .map(|query| {
                let mut scored: Vec<(f64, u64)> = held
                    .iter()
                    .map(|(seq, stored)| (l2(stored, query), *seq))
                    .collect();
                scored.sort_by(|l, r| l.0.total_cmp(&r.0).then_with(|| l.1.cmp(&r.1)));
                scored.into_iter().take(K).map(|(_, seq)| seq).collect()
            })
            .collect();

        let mut graph_rows = Vec::new();
        for ef in [10usize, 40, 100, 200] {
            let started = Instant::now();
            let mut hit = 0usize;
            let mut examined = 0usize;
            for (at, query) in probes.iter().enumerate() {
                let result = db
                    .query_vamana_vector(graph, query, VectorMetric::SquaredL2, K, ef, usize::MAX, || false)
                    .unwrap();
                examined += result.examined;
                hit += result
                    .hits
                    .iter()
                    .filter(|h| truth[at].contains(&h.id.sequence))
                    .count();
            }
            graph_rows.push(json!({
                "ef": ef,
                "ms_per_query": started.elapsed().as_secs_f64() * 1000.0 / QUERIES as f64,
                "recall_at_10": hit as f64 / (QUERIES * K) as f64,
                "records_read_per_query": examined as f64 / QUERIES as f64,
            }));
        }
        let mut linear_rows = Vec::new();
        for ef in [10usize, 40, 100] {
            let started = Instant::now();
            let mut hit = 0usize;
            let mut examined = 0usize;
            for (at, query) in probes.iter().enumerate() {
                let result = db
                    .query_quantized_vector(
                        linear,
                        query,
                        VectorMetric::SquaredL2,
                        K,
                        ef,
                        QuantizedVectorCandidates::All,
                        usize::MAX,
                        || false,
                    )
                    .unwrap();
                examined += result.examined;
                hit += result
                    .hits
                    .iter()
                    .filter(|h| truth[at].contains(&h.id.sequence))
                    .count();
            }
            linear_rows.push(json!({
                "ef": ef,
                "ms_per_query": started.elapsed().as_secs_f64() * 1000.0 / QUERIES as f64,
                "recall_at_10": hit as f64 / (QUERIES * K) as f64,
                "records_read_per_query": examined as f64 / QUERIES as f64,
            }));
        }
        drop(db);
        let graph_bytes = keyspace_bytes(&path, 0x7D, graph);
        let linear_bytes = keyspace_bytes(&path, 0x79, linear);
        println!(
            "{}",
            serde_json::to_string(&json!({
                "rows": rows,
                "shape": if shape == Shape::Clustered { "clustered" } else { "uniform" },
                "dimension": DIM,
                "build_seconds": {"quantized": linear_build, "vamana": graph_build},
                "bytes_per_row": {
                    "quantized_0x79": linear_bytes as f64 / rows as f64,
                    "vamana_0x7D": graph_bytes as f64 / rows as f64,
                },
                "vamana": graph_rows,
                "quantized": linear_rows,
            }))
            .unwrap()
        );
        fs::remove_dir_all(&path).unwrap();
    }
}
