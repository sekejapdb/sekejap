//! Standalone process-RSS confirmation for the disk-first int8 vector index.
//!
//! No DuckDB, no benchmark harness: generate N synthetic 128-d vectors, ingest to a
//! disk-backed CoreDB, build the disk-first index, **free the input vectors**, then
//! read the process's own `/proc/self/status` VmRSS. This measures the engine's true
//! resident footprint as OS RSS — the number the investigator asked for, confirming
//! (or refuting) the instrumented `memory_report()` figure.
//!
//! RAM is independent of vector *values*, so synthetic data is valid here (we measure
//! bytes, not recall). `cargo run --release --example rss_standalone [N]` (Linux).

use sekejap::{CoreDB, VecMetric};

fn rss_kb(key: &str) -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in s.lines() {
        if let Some(r) = l.strip_prefix(key) {
            return r.trim().trim_end_matches(" kB").trim().parse::<f64>().unwrap_or(0.0);
        }
    }
    0.0
}
fn mb(kb: f64) -> f64 { kb / 1024.0 }

fn vec_for(i: usize, dim: usize) -> Vec<f32> {
    (0..dim).map(|j| {
        let mut x = (i as u64).wrapping_mul(6364136223846793005)
            .wrapping_add((j as u64).wrapping_mul(1442695040888963407));
        x ^= x >> 12; x ^= x << 25; x ^= x >> 27;
        ((x.wrapping_mul(0x2545F4914F6CDD1D) >> 33) as f32) / (1u64 << 31) as f32 * 100.0
    }).collect()
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let dim = 128usize;

    let dir = std::env::temp_dir().join("rss-standalone");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut db = CoreDB::open(&dir).unwrap();

    // Generate + ingest (bulk WAL defer). Keep base in a Vec we will explicitly free.
    let base: Vec<Vec<f32>> = (0..n).map(|i| vec_for(i, dim)).collect();
    db.begin_bulk();
    for (i, v) in base.iter().enumerate() {
        db.put(&format!("sift/{i}"), &format!(r#"{{"_collection":"sift","_key":"{i}"}}"#)).unwrap();
        db.put_vector(&format!("sift/{i}"), "emb", v).unwrap();
    }
    db.end_bulk();
    db.build_hnsw_index_disk("emb", 16, 200, VecMetric::L2).unwrap();

    // Free the input vectors — the engine now holds int8 codes + CSR graph; f32 is on disk.
    drop(base);
    #[cfg(target_os = "linux")]
    { extern "C" { fn malloc_trim(pad: usize) -> std::os::raw::c_int; } unsafe { malloc_trim(0); } }

    let eng: usize = db.memory_report().iter()
        .filter(|(l, _)| !l.starts_with('_')).map(|(_, b)| *b).sum();
    println!("N={n}");
    println!("instrumented engine (memory_report) = {:.1} MB", eng as f64 / 1_048_576.0);
    println!("PROCESS VmRSS  (true resident)       = {:.1} MB", mb(rss_kb("VmRSS:")));
    println!("PROCESS VmHWM  (peak, incl. build)   = {:.1} MB", mb(rss_kb("VmHWM:")));
    // Query once to keep the index live past the measurement (prevents any lazy drop).
    let q = vec_for(0, dim);
    let qs: String = q.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
    let hits = db.query(&format!("SELECT _key FROM sift WHERE VECTOR_NEAR(emb, [{qs}], 10)"))
        .map(|s| s.collect()).unwrap_or_default();
    eprintln!("(sanity: query returned {} hits)", hits.len());
    let _ = std::fs::remove_dir_all(&dir);
}
