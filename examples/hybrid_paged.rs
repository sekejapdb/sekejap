// Cross-model single-pass query — resident vs paged (disk-first). The differentiator:
// scalar ∩ spatial ∩ text ∩ vector resolved in ONE statement, one index pass. This
// checks the paged (mmap) path returns the SAME answer as resident and measures the
// RAM held (net live heap + RSS) — the "Beyond RAM" claim for the full hybrid stack.
//
//   cargo run --release --example hybrid_paged [N]
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};
use sekejap::CoreDB;

struct Counting;
static ALLOCED: AtomicUsize = AtomicUsize::new(0);
static FREED: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() { ALLOCED.fetch_add(l.size(), Relaxed); }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        FREED.fetch_add(l.size(), Relaxed);
        System.dealloc(p, l);
    }
}
#[global_allocator]
static GA: Counting = Counting;

fn live_kb() -> f64 {
    (ALLOCED.load(Relaxed) as i64 - FREED.load(Relaxed) as i64) as f64 / 1024.0
}
fn rss_mb() -> f64 {
    let pid = std::process::id().to_string();
    std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|kb| kb / 1024.0).unwrap_or(0.0)
}

const Q: &str = "SELECT _key FROM venues \
    WHERE category = 'cafe' \
      AND ST_DWithin(geometry, POINT(106.5 -6.4), 30000) \
      AND SEARCH('coffee') \
    ORDER BY VECTOR_COSINE(emb, [0.0,0.1,0.2,0.3,0.4,0.5,0.6,0.7]) DESC \
    LIMIT 10";

fn best_of(db: &CoreDB, iters: usize) -> (Duration, Vec<String>) {
    for _ in 0..2 { let _ = db.query(Q).map(|s| s.collect()); }
    let mut best = Duration::from_secs(999);
    let mut keys = Vec::new();
    for _ in 0..iters {
        let t = Instant::now();
        let hits = db.query(Q).unwrap().collect();
        best = best.min(t.elapsed());
        keys = hits.iter().map(|h| h.slug.clone()).collect();
    }
    (best, keys)
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let dir = std::env::temp_dir().join(format!("sk-hybridpaged-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let cats = ["cafe", "resto", "bar", "bakery", "hotel"];
    let words = ["coffee", "brunch", "wine", "grilled", "vegan", "spicy", "seafood", "dessert"];
    {
        let mut db = CoreDB::open(&dir).unwrap();
        // venues collection needs a schema for the SEARCH index declaration.
        db.execute("CREATE TABLE venues (category TEXT, price INTEGER, content TEXT, geometry GEO)").unwrap();
        let mut pairs = Vec::with_capacity(n);
        for i in 0..n {
            // Cluster around the query center (±~0.2° ≈ ±22 km) via decorrelated
            // hashes of i, so spatial ∩ scalar ∩ text is non-empty.
            let hx = (i.wrapping_mul(2654435761) % 4000) as f64 - 2000.0;
            let hy = (i.wrapping_mul(40503) % 4000) as f64 - 2000.0;
            let lon = 106.5 + hx * 0.0001;
            let lat = -6.4 + hy * 0.0001;
            let content = format!("{} {} great place",
                words[i.wrapping_mul(3) % words.len()], words[i.wrapping_mul(5).wrapping_add(2) % words.len()]);
            pairs.push((format!("venues/v{i}"), format!(
                r#"{{"_collection":"venues","_key":"v{i}","category":"{}","price":{},"content":"{content}","geometry":{{"type":"Point","coordinates":[{lon},{lat}]}}}}"#,
                cats[i % cats.len()], i % 500)));
        }
        db.put_many(pairs.iter().map(|(s, p)| (s.as_str(), p.as_str()))).unwrap();
        for i in 0..n {
            // Unique per-doc cosine direction (monotonic in i) so top-k is
            // well-defined and resident/paged rankings match exactly — no ties.
            let v: Vec<f32> = vec![1.0, i as f32 * 1e-4, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
            db.put_vector(&format!("venues/v{i}"), "emb", &v).unwrap();
        }
        // Four facet indexes — all with on-disk sidecars after compact().
        db.build_field_index("venues", "category");
        db.build_spatial_index();
        db.build_hnsw_index("emb", 16, 200);
        db.execute("CREATE INDEX ON venues USING search (content)").unwrap();
        db.compact().unwrap();
    }
    let disk_mb = |f: &str| std::fs::metadata(dir.join(f)).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
    println!("corpus: {n} venues");
    println!("on disk: search.bin {:.1}MB  spatial.bin {:.1}MB  emb.bin {:.1}MB  payloads.bin {:.1}MB\n",
        disk_mb("search.bin"), disk_mb("spatial.bin"), disk_mb("emb.bin"), disk_mb("payloads.bin"));

    // Resident.
    let h0 = live_kb();
    let (rt, rkeys, rheap, rrss) = {
        let db = CoreDB::open(&dir).unwrap();
        let heap = live_kb() - h0;
        let (t, keys) = best_of(&db, 10);
        (t, keys, heap, rss_mb())
    };

    // Paged (disk-first).
    let h1 = live_kb();
    let db = CoreDB::open_paged(&dir).unwrap();
    let pheap = live_kb() - h1;
    let (pt, pkeys) = best_of(&db, 10);
    let prss = rss_mb();

    println!("cross-model single-pass query (scalar ∩ spatial ∩ text ∩ vector), best-of-10:");
    println!("  resident : {:>10?}   heap +{:>8.1} KB   RSS {:.1} MB   rows {}", rt, rheap, rrss, rkeys.len());
    println!("  paged    : {:>10?}   heap +{:>8.1} KB   RSS {:.1} MB   rows {}", pt, pheap, prss, pkeys.len());
    println!();
    println!("  same answer (ordered): {}", rkeys == pkeys);
    if rkeys != pkeys {
        println!("    resident: {:?}", rkeys);
        println!("    paged   : {:?}", pkeys);
    }
    if pheap > 0.0 {
        println!("  heap reduction: {:.1}x", rheap / pheap.max(0.001));
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::hint::black_box(&db);
}
