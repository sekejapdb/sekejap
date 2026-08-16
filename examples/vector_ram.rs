// Vector (HNSW int8 + CSR) index residency + latency: resident vs paged (disk-first).
// Resident open holds the compact index (int8 codes + CSR graph) in heap; paged open
// mmaps it (served off the map, f32 re-rank from the mmap'd f32 store). Counting
// allocator measures net live heap; best-of-N measures query latency; top-k identity
// confirms disk-first is semantically transparent.
//
//   cargo run --release --example vector_ram [N] [DIM]
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};
use sekejap::CoreDB;

struct Counting;
static ALLOCED: AtomicUsize = AtomicUsize::new(0);
static FREED: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 { let p = System.alloc(l); if !p.is_null() { ALLOCED.fetch_add(l.size(), Relaxed); } p }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { FREED.fetch_add(l.size(), Relaxed); System.dealloc(p, l); }
}
#[global_allocator]
static GA: Counting = Counting;

fn live_kb() -> f64 { (ALLOCED.load(Relaxed) as i64 - FREED.load(Relaxed) as i64) as f64 / 1024.0 }

fn qvec(dim: usize) -> String {
    let v: Vec<String> = (0..dim).map(|d| format!("{:.3}", (d * 13 % 97) as f32 * 0.01)).collect();
    format!("[{}]", v.join(","))
}

fn best_of(db: &CoreDB, q: &str, iters: usize) -> (Duration, Vec<String>) {
    for _ in 0..2 { let _ = db.query(q).map(|s| s.collect()); }
    let mut best = Duration::from_secs(999);
    let mut keys = Vec::new();
    for _ in 0..iters {
        let t = Instant::now();
        let hits = db.query(q).unwrap().collect();
        best = best.min(t.elapsed());
        keys = hits.iter().map(|h| h.slug.clone()).collect();
    }
    (best, keys)
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let dim: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(128);
    let dir = std::env::temp_dir().join(format!("sk-vecram-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mut db = CoreDB::open(&dir).unwrap();
        for i in 0..n {
            db.put(&format!("v/n{i}"), &format!(r#"{{"_collection":"v","_key":"n{i}"}}"#)).unwrap();
            let vec: Vec<f32> = (0..dim).map(|d| ((i * 31 + d * 7) % 997) as f32 * 0.001).collect();
            db.put_vector(&format!("v/n{i}"), "emb", &vec).unwrap();
        }
        db.compact().unwrap();
        db.build_hnsw_index_disk("emb", 16, 200, sekejap::VecMetric::L2).unwrap();
    }
    let vb = std::fs::metadata(dir.join("vecidx.bin")).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
    let fb = std::fs::metadata(dir.join("emb.bin")).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
    println!("corpus: {n} vectors, dim {dim}   vecidx.bin {:.1} MB   emb.bin(f32) {:.1} MB\n", vb, fb);

    let q = format!("SELECT _key FROM v ORDER BY VECTOR_L2(emb, {}) ASC LIMIT 10", qvec(dim));

    let (rt, rkeys, rheap) = {
        let h = live_kb();
        let db = CoreDB::open(&dir).unwrap();
        let heap = live_kb() - h;
        let (t, keys) = best_of(&db, &q, 20);
        (t, keys, heap)
    };
    let h = live_kb();
    let db = CoreDB::open_paged(&dir).unwrap();
    let pheap = live_kb() - h;
    let (pt, pkeys) = best_of(&db, &q, 20);

    println!("compact vector index (int8 + CSR), top-10 by VECTOR_L2, best-of-20:");
    println!("  resident : {:>10?}   heap +{:>9.1} KB", rt, rheap);
    println!("  paged    : {:>10?}   heap +{:>9.1} KB", pt, pheap);
    println!("  slowdown : {:.2}x", pt.as_secs_f64() / rt.as_secs_f64().max(1e-9));
    if pheap > 0.0 { println!("  heap reduction: {:.1}x", rheap / pheap.max(0.001)); }
    println!("  same top-10: {}", rkeys == pkeys);
    let _ = std::fs::remove_dir_all(&dir);
    std::hint::black_box(&db);
}
