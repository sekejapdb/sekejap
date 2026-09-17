//! Explicit quantized-vector recall/cost evidence. Runtime is Linux-only.
//!
//! The independent oracle scores the generated f32 corpus directly. The
//! existing exact index is checked against that oracle before its timings are
//! used as the paired reference for the opt-in symmetric-int8 scan.

use crc32c::crc32c_append;
use e4_prototype::{
    collections::{
        ApproxVectorMethod, CollectionOptions, Database, EntityId, IndexId,
        QuantizedVectorCandidates, VectorCandidates, VectorHit, VectorMetric,
    },
    pagewal::create_compact_cells,
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::Path,
    time::Instant,
};

type R<T> = Result<T, Box<dyn std::error::Error>>;

const CACHE_BYTES: usize = 8 << 20;
const BATCH: usize = 256;
const REPETITIONS: usize = 3;
const MAX_ROWS: usize = 1_000_000;
const MAX_SOURCE_LANES: usize = 128_000_000;

#[derive(Clone)]
struct QueryCase {
    name: &'static str,
    vector: Vec<f32>,
}

fn config() -> Config {
    Config {
        budget_bytes: CACHE_BYTES,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn mix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn signed(seed: u64, lane: usize) -> f32 {
    let bits = (mix64(seed ^ (lane as u64).wrapping_mul(0xd6e8_feb8_6659_fd93)) >> 40) as i32;
    (bits - (1 << 23)) as f32 / (1 << 23) as f32
}

fn normalized(seed: u64, dimension: usize) -> Vec<f32> {
    let mut vector = (0..dimension)
        .map(|lane| signed(seed, lane))
        .collect::<Vec<_>>();
    let norm = vector
        .iter()
        .fold(0.0f64, |sum, lane| sum + f64::from(*lane).powi(2))
        .sqrt();
    for lane in &mut vector {
        *lane = (f64::from(*lane) / norm) as f32;
    }
    vector
}

fn generated_vector(row: usize, dimension: usize) -> Vec<f32> {
    if row == 0 || (row > 4 && row % 997 == 0) {
        return vec![0.0; dimension];
    }
    if row == 1 || row == 2 {
        return (0..dimension)
            .map(|lane| if lane % 2 == 0 { 1024.0 } else { -1024.0 })
            .collect();
    }
    if row == 3 {
        return normalized(0x554e_4954, dimension);
    }
    if row == 4 {
        return (0..dimension)
            .map(|lane| signed(0x4e4f_4e55_4e49_54, lane) * 3.75)
            .collect();
    }
    match row % 3 {
        0 => normalized(0x1000_0000 ^ row as u64, dimension),
        1 => {
            let scale = 0.25 + (row % 29) as f32 * 0.25;
            (0..dimension)
                .map(|lane| signed(0x2000_0000 ^ row as u64, lane) * scale)
                .collect()
        }
        _ => {
            let cluster = row % 16;
            (0..dimension)
                .map(|lane| {
                    let center = signed(0x3000_0000 ^ cluster as u64, lane) * 0.75;
                    let noise = signed(0x4000_0000 ^ row as u64, lane) * 0.025;
                    center + noise
                })
                .collect()
        }
    }
}

fn dataset(rows: usize, dimension: usize) -> (Vec<Vec<f32>>, Vec<QueryCase>, String, Value) {
    let mut crc = 0;
    crc = crc32c_append(crc, &(rows as u64).to_le_bytes());
    crc = crc32c_append(crc, &(dimension as u64).to_le_bytes());
    let mut vectors = Vec::with_capacity(rows);
    let mut counts = [0usize; 4];
    for row in 0..rows {
        let vector = generated_vector(row, dimension);
        let class = if vector.iter().all(|lane| *lane == 0.0) {
            0
        } else if row == 1 || row == 2 || row % 3 == 1 {
            2
        } else if row % 3 == 2 {
            3
        } else {
            1
        };
        counts[class] += 1;
        crc = crc32c_append(crc, &(row as u64).to_le_bytes());
        for lane in &vector {
            crc = crc32c_append(crc, &lane.to_bits().to_le_bytes());
        }
        vectors.push(vector);
    }
    let cases = [
        ("unit", 3usize),
        ("nonunit", 4usize),
        ("clustered", 5usize),
        ("exact_tie", 1usize),
    ]
    .into_iter()
    .map(|(name, row)| QueryCase {
        name,
        vector: vectors[row].clone(),
    })
    .collect();
    (
        vectors,
        cases,
        format!("crc32c:{crc:08x}"),
        json!({
            "zero": counts[0],
            "unit": counts[1],
            "nonunit_or_tie": counts[2],
            "clustered": counts[3],
            "exact_tie_source_rows": [1, 2]
        }),
    )
}

fn metric_name(metric: VectorMetric) -> &'static str {
    match metric {
        VectorMetric::Cosine => "cosine",
        VectorMetric::SquaredL2 => "squared_l2",
        VectorMetric::NegativeDot => "negative_dot",
    }
}

fn independent_score(stored: &[f32], query: &[f32], metric: VectorMetric) -> Option<f64> {
    let mut dot = 0.0f64;
    let mut stored_norm = 0.0f64;
    let mut query_norm = 0.0f64;
    let mut squared_l2 = 0.0f64;
    for (&stored, &query) in stored.iter().zip(query) {
        let stored = f64::from(stored);
        let query = f64::from(query);
        dot += stored * query;
        stored_norm += stored * stored;
        query_norm += query * query;
        let difference = stored - query;
        squared_l2 += difference * difference;
    }
    let distance = match metric {
        VectorMetric::SquaredL2 => squared_l2,
        VectorMetric::NegativeDot => -dot,
        VectorMetric::Cosine if stored_norm == 0.0 => return None,
        VectorMetric::Cosine => 1.0 - dot / (stored_norm.sqrt() * query_norm.sqrt()),
    };
    Some(if distance == 0.0 { 0.0 } else { distance })
}

fn oracle(
    vectors: &[Vec<f32>],
    ids: &[EntityId],
    query: &[f32],
    metric: VectorMetric,
    k: usize,
) -> Vec<VectorHit> {
    let mut hits = vectors
        .iter()
        .zip(ids)
        .filter_map(|(stored, id)| {
            independent_score(stored, query, metric).map(|distance| VectorHit { id: *id, distance })
        })
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then(left.id.cmp(&right.id))
    });
    hits.truncate(k);
    hits
}

fn assert_exact(actual: &[VectorHit], expected: &[VectorHit]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(actual.id, expected.id);
        assert_eq!(actual.distance.to_bits(), expected.distance.to_bits());
    }
}

fn assert_sorted(hits: &[VectorHit]) {
    assert!(hits.windows(2).all(|pair| {
        pair[0].distance.total_cmp(&pair[1].distance).is_lt()
            || (pair[0].distance.to_bits() == pair[1].distance.to_bits() && pair[0].id < pair[1].id)
    }));
}

fn hit_json(hits: &[VectorHit]) -> Vec<Value> {
    hits.iter()
        .map(|hit| {
            json!({
                "collection": hit.id.collection.0,
                "sequence": hit.id.sequence,
                "distance": hit.distance,
                "distance_bits": format!("{:016x}", hit.distance.to_bits())
            })
        })
        .collect()
}

fn recall(actual: &[VectorHit], expected: &[VectorHit]) -> f64 {
    let expected = expected.iter().map(|hit| hit.id).collect::<BTreeSet<_>>();
    let found = actual
        .iter()
        .filter(|hit| expected.contains(&hit.id))
        .count();
    found as f64 / expected.len() as f64
}

fn tie_pairs(hits: &[VectorHit]) -> usize {
    hits.windows(2)
        .filter(|pair| pair[0].distance.to_bits() == pair[1].distance.to_bits())
        .count()
}

fn finish_build(
    db: &mut Database,
    index: IndexId,
    root: &Path,
    peak: &mut (u64, u64),
) -> R<(usize, usize)> {
    let mut steps = 0;
    let mut commits = 0;
    loop {
        let ready = db.build_index_step(index, BATCH)?;
        steps += 1;
        db.commit()?;
        commits += 1;
        sample(root, peak)?;
        if ready {
            return Ok((steps, commits));
        }
    }
}

fn sizes(root: &Path) -> R<(u64, u64)> {
    fn visit(path: &Path, totals: &mut (u64, u64)) -> R<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                visit(&entry.path(), totals)?;
            } else if metadata.is_file() {
                totals.0 += metadata.len();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    totals.1 += metadata.blocks() * 512;
                }
                #[cfg(not(unix))]
                {
                    totals.1 += metadata.len();
                }
            }
        }
        Ok(())
    }
    let mut totals = (0, 0);
    visit(root, &mut totals)?;
    Ok(totals)
}

fn sample(root: &Path, peak: &mut (u64, u64)) -> R<()> {
    let current = sizes(root)?;
    peak.0 = peak.0.max(current.0);
    peak.1 = peak.1.max(current.1);
    Ok(())
}

fn bytes_json(bytes: (u64, u64)) -> Value {
    json!({"logical": bytes.0, "allocated": bytes.1})
}

fn delta_json(after: (u64, u64), before: (u64, u64)) -> Value {
    json!({
        "logical": after.0 as i64 - before.0 as i64,
        "allocated": after.1 as i64 - before.1 as i64
    })
}

fn proc_memory() -> Value {
    let mut values = BTreeMap::new();
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if ["VmRSS:", "VmHWM:"]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        {
            let mut parts = line.split_whitespace();
            let name = parts.next().unwrap().trim_end_matches(':');
            if let Some(kib) = parts.next().and_then(|value| value.parse::<u64>().ok()) {
                values.insert(name, kib * 1024);
            }
        }
    }
    json!(values)
}

fn feature_json() -> Value {
    let mut features = Vec::new();
    if cfg!(feature = "compact-cells") {
        features.push("compact-cells");
    }
    if cfg!(feature = "sqlite-balance") {
        features.push("sqlite-balance");
    }
    if cfg!(feature = "keyspace-append") {
        features.push("keyspace-append");
    }
    if cfg!(feature = "slotref-split") {
        features.push("slotref-split");
    }
    json!({
        "cargo": features,
        "create_compact_cells": create_compact_cells(),
        "engine_revision": option_env!("E4_ENGINE_REVISION").unwrap_or("unrecorded"),
        "package_version": env!("CARGO_PKG_VERSION")
    })
}

fn parse_efs(argument: Option<String>, k: usize) -> R<Vec<usize>> {
    let values = if let Some(argument) = argument {
        argument
            .split(',')
            .map(str::parse)
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![
            k,
            k.saturating_mul(4),
            k.saturating_mul(16),
            k.saturating_mul(64),
        ]
    };
    let unique = values.into_iter().collect::<BTreeSet<_>>();
    if unique.len() < 2 || unique.iter().any(|ef| *ef < k || *ef > 65_536) {
        return Err("ef list needs at least two unique values with k <= ef <= 65536".into());
    }
    Ok(unique.into_iter().collect())
}

fn run() -> R<Value> {
    if env::consts::OS != "linux" {
        return Err("phase2_quantized_bench runtime is Linux-only".into());
    }
    let mut arguments = env::args().skip(1);
    let rows: usize = arguments.next().ok_or("missing ROWS")?.parse()?;
    let dimension: usize = arguments.next().ok_or("missing DIMENSION")?.parse()?;
    let root = arguments
        .next()
        .ok_or("missing NEW_DIR")
        .map(String::from)?;
    let k: usize = arguments
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(10);
    let efs = parse_efs(arguments.next(), k)?;
    if arguments.next().is_some() {
        return Err("usage: phase2_quantized_bench ROWS DIMENSION NEW_DIR [K] [EF_CSV]".into());
    }
    if !(64..=MAX_ROWS).contains(&rows) || !(2..=65_536).contains(&k) || k > rows {
        return Err("require 64 <= ROWS <= 1000000 and 2 <= K <= min(65536, ROWS)".into());
    }
    if !(1..=16_384).contains(&dimension) {
        return Err("dimension must be 1..=16384".into());
    }
    let lanes = rows
        .checked_mul(dimension)
        .ok_or("source lane count overflow")?;
    if lanes > MAX_SOURCE_LANES {
        return Err(format!(
            "source corpus exceeds controlled {MAX_SOURCE_LANES}-lane memory limit"
        )
        .into());
    }
    let root = Path::new(&root);
    if root.exists() {
        return Err("NEW_DIR must not exist".into());
    }

    let generator_started = Instant::now();
    let (vectors, cases, input_digest, distribution) = dataset(rows, dimension);
    let nonzero_vectors = vectors
        .iter()
        .filter(|vector| vector.iter().any(|lane| *lane != 0.0))
        .count();
    let generator_seconds = generator_started.elapsed().as_secs_f64();

    let load_started = Instant::now();
    let mut db = Database::create(root, config())?;
    let collection = db.create_collection(
        "vectors",
        vec![("embedding".into(), Kind::Vector(dimension))],
        CollectionOptions::default(),
    )?;
    let mut ids = Vec::with_capacity(rows);
    let mut peak = (0, 0);
    let mut load_commits = 0;
    for base in (0..rows).step_by(BATCH) {
        for row in base..(base + BATCH).min(rows) {
            ids.push(db.put(
                collection,
                &format!("vector/{row:09}"),
                &json!({"embedding": &vectors[row]}),
            )?);
        }
        db.commit()?;
        load_commits += 1;
        sample(root, &mut peak)?;
    }
    let load_seconds = load_started.elapsed().as_secs_f64();
    let checkpoint_started = Instant::now();
    if !db.checkpoint()? {
        return Err("load checkpoint was deferred without an expected reader".into());
    }
    let load_checkpoint_seconds = checkpoint_started.elapsed().as_secs_f64();
    sample(root, &mut peak)?;
    let loaded_bytes = sizes(root)?;
    let loaded_store_bytes = db.storage_bytes()?;
    let memory_after_load = proc_memory();

    let exact_started = Instant::now();
    let exact_index = db.create_exact_vector_index(collection, "embedding_exact", "embedding")?;
    let (exact_steps, exact_commits) = finish_build(&mut db, exact_index, root, &mut peak)?;
    let exact_build_seconds = exact_started.elapsed().as_secs_f64();
    let checkpoint_started = Instant::now();
    if !db.checkpoint()? {
        return Err("exact-index checkpoint was deferred without an expected reader".into());
    }
    let exact_checkpoint_seconds = checkpoint_started.elapsed().as_secs_f64();
    sample(root, &mut peak)?;
    let exact_bytes = sizes(root)?;
    let exact_store_bytes = db.storage_bytes()?;
    let memory_after_exact = proc_memory();

    let quantized_started = Instant::now();
    let quantized_index =
        db.create_quantized_vector_index(collection, "embedding_int8", "embedding")?;
    let (quantized_steps, quantized_commits) =
        finish_build(&mut db, quantized_index, root, &mut peak)?;
    let quantized_build_seconds = quantized_started.elapsed().as_secs_f64();
    let checkpoint_started = Instant::now();
    if !db.checkpoint()? {
        return Err("quantized-index checkpoint was deferred without an expected reader".into());
    }
    let quantized_checkpoint_seconds = checkpoint_started.elapsed().as_secs_f64();
    sample(root, &mut peak)?;
    let quantized_bytes = sizes(root)?;
    let quantized_store_bytes = db.storage_bytes()?;
    let memory_after_quantized = proc_memory();

    let positions = ids
        .iter()
        .enumerate()
        .map(|(position, id)| (*id, position))
        .collect::<BTreeMap<_, _>>();
    let tie_ids = [ids[1], ids[2]];
    let metrics = [
        VectorMetric::Cosine,
        VectorMetric::SquaredL2,
        VectorMetric::NegativeDot,
    ];
    let mut query_records = Vec::new();
    for case in &cases {
        for metric in metrics {
            let expected = oracle(&vectors, &ids, &case.vector, metric, k);
            if case.name == "exact_tie" {
                let first = expected
                    .iter()
                    .position(|hit| hit.id == tie_ids[0])
                    .unwrap();
                let second = expected
                    .iter()
                    .position(|hit| hit.id == tie_ids[1])
                    .unwrap();
                assert_eq!(first + 1, second);
                assert_eq!(
                    expected[first].distance.to_bits(),
                    expected[second].distance.to_bits()
                );
            }
            for &ef in &efs {
                for repetition in 0..REPETITIONS {
                    let exact_first = repetition % 2 == 0;
                    let (exact_seconds, exact_hits, approx_seconds, approximate) = if exact_first {
                        let started = Instant::now();
                        let exact = db.query_exact_vector(
                            exact_index,
                            &case.vector,
                            metric,
                            k,
                            VectorCandidates::All,
                            rows,
                            || false,
                        )?;
                        let exact_seconds = started.elapsed().as_secs_f64();
                        let started = Instant::now();
                        let approximate = db.query_quantized_vector(
                            quantized_index,
                            &case.vector,
                            metric,
                            k,
                            ef,
                            QuantizedVectorCandidates::All,
                            rows,
                            || false,
                        )?;
                        (
                            exact_seconds,
                            exact,
                            started.elapsed().as_secs_f64(),
                            approximate,
                        )
                    } else {
                        let started = Instant::now();
                        let approximate = db.query_quantized_vector(
                            quantized_index,
                            &case.vector,
                            metric,
                            k,
                            ef,
                            QuantizedVectorCandidates::All,
                            rows,
                            || false,
                        )?;
                        let approx_seconds = started.elapsed().as_secs_f64();
                        let started = Instant::now();
                        let exact = db.query_exact_vector(
                            exact_index,
                            &case.vector,
                            metric,
                            k,
                            VectorCandidates::All,
                            rows,
                            || false,
                        )?;
                        (
                            started.elapsed().as_secs_f64(),
                            exact,
                            approx_seconds,
                            approximate,
                        )
                    };
                    assert_exact(&exact_hits, &expected);
                    assert_eq!(approximate.method, ApproxVectorMethod::SymmetricInt8ScanV1);
                    assert_eq!(approximate.ef, ef);
                    assert_eq!(approximate.examined, rows);
                    let eligible = if metric == VectorMetric::Cosine {
                        nonzero_vectors
                    } else {
                        rows
                    };
                    assert_eq!(approximate.reranked, ef.min(eligible));
                    assert_sorted(&approximate.hits);
                    for hit in &approximate.hits {
                        let source = &vectors[*positions.get(&hit.id).unwrap()];
                        let distance = independent_score(source, &case.vector, metric).unwrap();
                        assert_eq!(hit.distance.to_bits(), distance.to_bits());
                    }
                    query_records.push(json!({
                        "case": case.name,
                        "metric": metric_name(metric),
                        "k": k,
                        "ef": ef,
                        "repetition": repetition,
                        "execution_order": if exact_first {"exact_then_quantized"} else {"quantized_then_exact"},
                        "exact_seconds": exact_seconds,
                        "quantized_seconds": approx_seconds,
                        "recall_at_k": recall(&approximate.hits, &expected),
                        "exact_examined": rows,
                        "quantized_examined": approximate.examined,
                        "quantized_reranked": approximate.reranked,
                        "method": "SymmetricInt8ScanV1",
                        "oracle_tie_pairs": tie_pairs(&expected),
                        "returned_tie_pairs": tie_pairs(&approximate.hits),
                        "oracle": hit_json(&expected),
                        "exact": hit_json(&exact_hits),
                        "quantized": hit_json(&approximate.hits)
                    }));
                }
            }
        }
    }
    sample(root, &mut peak)?;
    let memory_after_queries = proc_memory();

    Ok(json!({
        "format": "phase2-quantized-vector-bench-v1",
        "status": "candidate-evidence-not-phase2-acceptance",
        "rows": rows,
        "dimension": dimension,
        "k": k,
        "efs": efs,
        "repetitions": REPETITIONS,
        "input_digest": input_digest,
        "distribution": distribution,
        "generator_seconds": generator_seconds,
        "generator_in_timed_load": false,
        "runtime": {
            "cache_bytes": CACHE_BYTES,
            "sync": "FULL",
            "io": "Buffered",
            "batch": BATCH,
            "source_lane_limit": MAX_SOURCE_LANES,
            "features": feature_json()
        },
        "load": {
            "seconds": load_seconds,
            "commits": load_commits,
            "checkpoint_seconds": load_checkpoint_seconds,
            "checkpointed_tree_bytes": bytes_json(loaded_bytes),
            "checkpointed_store_data_wal_bytes": {"data": loaded_store_bytes.0, "wal": loaded_store_bytes.1},
            "memory": memory_after_load
        },
        "exact_index_build": {
            "order": 1,
            "policy": "bounded 256-row steps, each durably committed",
            "seconds": exact_build_seconds,
            "steps": exact_steps,
            "commits": exact_commits,
            "checkpoint_seconds": exact_checkpoint_seconds,
            "checkpointed_tree_bytes": bytes_json(exact_bytes),
            "incremental_tree_bytes": delta_json(exact_bytes, loaded_bytes),
            "checkpointed_store_data_wal_bytes": {"data": exact_store_bytes.0, "wal": exact_store_bytes.1},
            "memory": memory_after_exact
        },
        "quantized_index_build": {
            "order": 2,
            "policy": "bounded 256-row steps, each durably committed",
            "seconds": quantized_build_seconds,
            "steps": quantized_steps,
            "commits": quantized_commits,
            "checkpoint_seconds": quantized_checkpoint_seconds,
            "checkpointed_tree_bytes": bytes_json(quantized_bytes),
            "incremental_tree_bytes": delta_json(quantized_bytes, exact_bytes),
            "checkpointed_store_data_wal_bytes": {"data": quantized_store_bytes.0, "wal": quantized_store_bytes.1},
            "memory": memory_after_quantized
        },
        "sampled_peak_tree_bytes": bytes_json(peak),
        "peak_limit": "sampled after durable commits; transient and allocator peaks between samples can be missed",
        "memory_after_queries": memory_after_queries,
        "queries": query_records,
        "qualification_limits": [
            "linear quantized scan, not a sublinear-search claim",
            "incremental index bytes use a fixed exact-then-quantized build order",
            "single process run; native driver must alternate whole arms across repetitions",
            "no SQLite ANN comparison"
        ]
    }))
}

fn main() {
    match run() {
        Ok(report) => println!("{}", serde_json::to_string(&report).unwrap()),
        Err(error) => {
            eprintln!("phase2_quantized_bench: {error}");
            std::process::exit(1);
        }
    }
}
