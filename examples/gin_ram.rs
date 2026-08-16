// GIN (trigram / LIKE index) residency: heap held resident vs paged (disk-first).
// Resident open loads gin.bin into heap; paged open mmaps it (postings served off
// the map). Counting allocator measures net live heap attributable to each open.
//
//   cargo run --release --example gin_ram [N]
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
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
    let dir = std::env::temp_dir().join(format!("sk-ginram-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let words = ["bakery","cafe","coffee","bistro","roasters","kitchen","grill","deli","tavern","lounge"];
    {
        let mut db = CoreDB::open(&dir).unwrap();
        db.execute("CREATE TABLE docs (name TEXT)").unwrap();
        for i in 0..n {
            let name = format!("{} {} {}", words[i % words.len()], words[(i / 7) % words.len()], i);
            db.execute(&format!("INSERT INTO docs (_key, name) VALUES ('d{i}', '{name}')")).unwrap();
        }
        db.execute("CREATE INDEX ON docs USING gin (name)").unwrap();
        db.compact().unwrap();
    }
    let gin_mb = std::fs::metadata(dir.join("gin.bin")).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
    println!("corpus: {n} docs   gin.bin on disk: {:.2} MB\n", gin_mb);

    let h0 = live_kb();
    let resident = CoreDB::open(&dir).unwrap();
    let resident_heap = live_kb() - h0;
    let r_hits = resident.query("SELECT _key FROM docs WHERE name ILIKE '%coffee%'").unwrap().collect().len();
    drop(resident);

    let h1 = live_kb();
    let paged = CoreDB::open_paged(&dir).unwrap();
    let paged_heap = live_kb() - h1;
    let p_hits = paged.query("SELECT _key FROM docs WHERE name ILIKE '%coffee%'").unwrap().collect().len();

    println!("net live heap held by open():");
    println!("  resident open : {:>9.1} KB   (ILIKE '%coffee%' → {r_hits} hits)", resident_heap);
    println!("  paged open    : {:>9.1} KB   (ILIKE '%coffee%' → {p_hits} hits, served from mmap)", paged_heap);
    if paged_heap > 0.0 { println!("  heap reduction: {:.1}x", resident_heap / paged_heap.max(0.001)); }
    println!("  same hits: {}", r_hits == p_hits);
    let _ = std::fs::remove_dir_all(&dir);
    std::hint::black_box(&paged);
}
