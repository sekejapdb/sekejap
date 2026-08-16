// BM25 residency + open time: rebuild (heap) vs mmap-load (paged, disk-first).
// The paged path mmaps bm25.bin (doc arrays off heap, dict resident) — instant open;
// the heap path rebuilds the index from text — O(N) open. Counting allocator measures
// net live heap; Instant measures open/build time.
//
//   cargo run --release --example bm25_ram [N]
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;
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

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let dir = std::env::temp_dir().join(format!("sk-bm25ram-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let words = ["rust","python","coffee","systems","async","runtime","fast","melbourne",
                 "roasters","language","learning","performance","vector","graph","spatial","query"];
    {
        let mut db = CoreDB::open(&dir).unwrap();
        db.execute("CREATE TABLE docs (body TEXT)").unwrap();
        for i in 0..n {
            let body = format!("{} {} {} {} {}",
                words[i % words.len()], words[(i / 3) % words.len()], words[(i / 7) % words.len()],
                words[(i / 11) % words.len()], words[(i / 13) % words.len()]);
            db.execute(&format!("INSERT INTO docs (_key, body) VALUES ('d{i}', '{body}')")).unwrap();
        }
        db.compact().unwrap();
        db.build_bm25_index("body");          // build + spill + save bm25.bin
    }
    let bb = std::fs::metadata(dir.join("bm25.bin")).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
    let pb = std::fs::metadata(dir.join("bm25_body.postings")).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
    println!("corpus: {n} docs   bm25.bin {:.2} MB   postings {:.2} MB\n", bb, pb);
    let q = "SELECT _key FROM docs WHERE BM25(body, 'coffee') > 0.0";

    // Heap: reopen + REBUILD the index from text (the current heap-mode path).
    let h0 = live_kb();
    let mut hdb = CoreDB::open(&dir).unwrap();
    let t = Instant::now();
    hdb.build_bm25_index("body");
    let heap_build_ms = t.elapsed();
    let heap = live_kb() - h0;
    let h_hits = hdb.query(q).unwrap().collect().len();
    drop(hdb);

    // Paged: open_paged MMAPS bm25.bin — no rebuild.
    let h1 = live_kb();
    let t = Instant::now();
    let pdb = CoreDB::open_paged(&dir).unwrap();
    let paged_open_ms = t.elapsed();
    let paged = live_kb() - h1;
    let p_hits = pdb.query(q).unwrap().collect().len();

    println!("open/build time:");
    println!("  heap  : rebuild index = {:?}", heap_build_ms);
    println!("  paged : mmap open     = {:?}", paged_open_ms);
    println!("net live heap held:");
    println!("  heap  : {:>9.1} KB   (BM25 'coffee' -> {h_hits} hits)", heap);
    println!("  paged : {:>9.1} KB   (BM25 'coffee' -> {p_hits} hits, doc arrays off heap)", paged);
    if paged > 0.0 { println!("  heap reduction: {:.1}x", heap / paged.max(0.001)); }
    println!("  same hits: {}", h_hits == p_hits);
    let _ = std::fs::remove_dir_all(&dir);
    std::hint::black_box(&pdb);
}
