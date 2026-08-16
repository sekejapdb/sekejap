// Idle memory footprint of an open, empty sekejap database — the fair analogue of
// SQLite's "~80 KB minimum" claim (net heap bytes, measured with a counting
// global allocator), plus coarse process RSS delta for context.
//
//   cargo run --release --example idle_ram
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
fn rss_kb() -> f64 {
    let pid = std::process::id().to_string();
    std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn main() {
    // In-memory engine (pure structures, no file buffers).
    let base_heap = live_kb();
    let base_rss = rss_kb();
    let mem = CoreDB::new();
    let mem_heap = live_kb() - base_heap;
    std::hint::black_box(&mem);

    // Disk open of a fresh empty directory (adds file/WAL buffers + mmap setup).
    let dir = std::env::temp_dir().join(format!("sk-idle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let h0 = live_kb();
    let disk = CoreDB::open(&dir).unwrap();
    let disk_heap = live_kb() - h0;
    let after_rss = rss_kb();
    std::hint::black_box(&disk);

    println!("idle footprint (empty database):");
    println!("  in-memory engine  heap delta : {:>8.1} KB", mem_heap);
    println!("  disk open         heap delta : {:>8.1} KB", disk_heap);
    println!("  net live heap (both open)     : {:>8.1} KB", live_kb() - base_heap);
    println!("  process RSS (baseline→open)   : {:>8.1} KB → {:.1} KB (Δ {:.1} KB)",
             base_rss, after_rss, after_rss - base_rss);
    println!("(SQLite claims ~80 KB minimum; note RSS includes the Rust runtime baseline.)");
    let _ = std::fs::remove_dir_all(&dir);
}
