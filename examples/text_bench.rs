// SEARCH latency: resident (heap) vs paged (disk-first / mmap). Same DB, same
// queries, best-of-N after warmup. Shows the cost of serving the FST + postings +
// field/position bitmaps from the memory map instead of the heap.
//
//   cargo run --release --example text_bench [N]
use std::time::{Duration, Instant};
use sekejap::CoreDB;

fn best_of(db: &CoreDB, q: &str, iters: usize) -> (Duration, usize) {
    for _ in 0..3 { let _ = db.query(q).map(|s| s.collect()); } // warm
    let mut best = Duration::from_secs(999);
    let mut rows = 0;
    for _ in 0..iters {
        let t = Instant::now();
        rows = db.query(q).map(|s| s.collect().len()).unwrap_or(0);
        best = best.min(t.elapsed());
    }
    (best, rows)
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let dir = std::env::temp_dir().join(format!("sk-textbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let common = ["rust", "python", "graph", "vector", "spatial", "embedded"];
    let vocab = 20_000usize;
    {
        let mut db = CoreDB::open(&dir).unwrap();
        db.execute("CREATE TABLE docs (title TEXT, body TEXT)").unwrap();
        for i in 0..n {
            let title = format!("{} t{} t{}", common[i % common.len()], i % vocab, (i * 7) % vocab);
            let body = format!("t{} t{} t{} t{} {}",
                (i * 3) % vocab, (i * 11) % vocab, (i * 13) % vocab, (i * 17) % vocab,
                common[(i / 4) % common.len()]);
            db.execute(&format!(
                "INSERT INTO docs (_key, title, body) VALUES ('d{i}', '{title}', '{body}')"
            )).unwrap();
        }
        db.execute("CREATE INDEX ON docs USING search (title, body)").unwrap();
        db.compact().unwrap();
    }
    println!("corpus: {n} docs, ~{vocab} vocab\n");

    let queries: [(&str, &str); 4] = [
        ("exact term",   "SELECT _key FROM docs WHERE SEARCH('rust')"),
        ("multi-term",   "SELECT _key FROM docs WHERE SEARCH('rust spatial')"),
        ("fuzzy",        "SELECT _key FROM docs WHERE SEARCH('spatal')"),
        ("ranked score", "SELECT _key FROM docs WHERE SEARCH('rust') ORDER BY SEARCH_SCORE('rust spatial') DESC LIMIT 10"),
    ];
    let iters = 20;

    // Single-writer lock → open sequentially: measure resident, drop, then paged.
    let resident: Vec<(Duration, usize)> = {
        let db = CoreDB::open(&dir).unwrap();
        queries.iter().map(|(_, q)| best_of(&db, q, iters)).collect()
    };
    let paged: Vec<(Duration, usize)> = {
        let db = CoreDB::open_paged(&dir).unwrap();
        queries.iter().map(|(_, q)| best_of(&db, q, iters)).collect()
    };

    println!("{:<14} {:>12} {:>12} {:>10}   {}", "query", "resident", "paged(mmap)", "slowdown", "rows");
    println!("{}", "-".repeat(66));
    for (i, (label, _)) in queries.iter().enumerate() {
        let (rt, rr) = resident[i];
        let (pt, pr) = paged[i];
        let ratio = pt.as_secs_f64() / rt.as_secs_f64().max(1e-9);
        let rows = if rr == pr { format!("{rr}") } else { format!("{rr} vs {pr} !!") };
        println!("{:<14} {:>12} {:>12} {:>9.2}x   {}",
            label, format!("{:?}", rt), format!("{:?}", pt), ratio, rows);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
