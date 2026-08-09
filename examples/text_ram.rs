// Text-index residency: heap held by the full-text (SEARCH) index when opened
// resident vs paged (disk-first). Resident open reads the FST + posting blobs
// into RAM; paged open mmaps them (served from the OS page cache, not the heap).
// A counting global allocator measures net live heap attributable to each open.
//
//   cargo run --release --example text_ram [N]
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
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

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let dir = std::env::temp_dir().join(format!("sk-textram-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // Build a text corpus with a SEARCH index, then compact so search.bin exists.
    let words = ["rust", "python", "graph", "vector", "spatial", "embedded", "query",
                 "index", "memory", "disk", "fast", "safe", "bounded", "engine", "edge"];
    {
        let mut db = CoreDB::open(&dir).unwrap();
        db.execute("CREATE TABLE docs (title TEXT, body TEXT)").unwrap();
        for i in 0..n {
            let title = format!("{} {}", words[i % words.len()], words[(i / 3) % words.len()]);
            let body = format!("{} {} {} {}",
                words[(i / 2) % words.len()], words[(i / 5) % words.len()],
                words[(i / 7) % words.len()], words[(i / 11) % words.len()]);
            db.execute(&format!(
                "INSERT INTO docs (_key, title, body) VALUES ('d{i}', '{title}', '{body}')"
            )).unwrap();
        }
        db.execute("CREATE INDEX ON docs USING search (title, body)").unwrap();
        db.compact().unwrap();
    }
    let search_bin = std::fs::metadata(dir.join("search.bin")).map(|m| m.len()).unwrap_or(0);
    println!("corpus: {n} docs   search.bin on disk: {:.2} MB", search_bin as f64 / 1e6);

    // Resident open — FST + postings read into the heap.
    let h0 = live_kb();
    let resident = CoreDB::open(&dir).unwrap();
    let resident_heap = live_kb() - h0;
    let _ = resident.query("SELECT _key FROM docs WHERE SEARCH('rust')").unwrap().collect();
    let resident_rss = rss_mb();
    drop(resident);

    // Paged open — FST + postings served from the mmap (not heap-resident).
    let h1 = live_kb();
    let paged = CoreDB::open_paged(&dir).unwrap();
    let paged_heap = live_kb() - h1;
    // Same query works off the mmap'd index.
    let hits = paged.query("SELECT _key FROM docs WHERE SEARCH('rust')").unwrap().collect().len();
    let paged_rss = rss_mb();

    println!();
    println!("net live heap held by open():");
    println!("  resident open : {:>9.1} KB", resident_heap);
    println!("  paged open    : {:>9.1} KB   (SEARCH('rust') → {hits} hits, served from mmap)", paged_heap);
    if paged_heap > 0.0 {
        println!("  heap reduction: {:.1}x", resident_heap / paged_heap.max(0.001));
    }
    println!("process RSS: resident {:.1} MB   paged {:.1} MB", resident_rss, paged_rss);
    let _ = std::fs::remove_dir_all(&dir);
    std::hint::black_box(&paged);
}
