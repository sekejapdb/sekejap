//! The shape of compaction's cost, over a 50x range of database sizes.
//!
//! `bench_light` compares two sizes and calls anything under 3x growth flat, which
//! is a screen rather than a measurement. This is the measurement: if the cost is
//! proportional to the change it should be a horizontal line, and any slope in it
//! is a structure still being rebuilt in full.
use sekejap::{Config, CoreDB, SyncMode};
use serde_json::json;

fn row(i: usize) -> String {
    json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64,
           "body":format!("the quick brown fox number {i} leaps the lazy riverbank")}).to_string()
}

fn main() {
    let sizes: Vec<usize> = std::env::args().skip(1).filter_map(|s| s.parse().ok()).collect();
    let sizes = if sizes.is_empty() { vec![20_000, 100_000, 500_000, 1_000_000] } else { sizes };

    println!("\n  compaction after 200 writes, by database size\n");
    println!("  {:>10}{:>14}{:>14}{:>12}", "rows", "rebuilt", "paged", "ratio");
    println!("  {}", "-".repeat(52));
    let mut first: Option<(f64, f64)> = None;
    for n in sizes {
        let mut times = [0.0f64; 2];
        for (slot, paged) in [(0usize, false), (1, true)] {
            let dir = tempfile::TempDir::new().unwrap();
            let cfg = Config {
                paged_topology: true, paged_adjacency: paged, paged_payloads: paged,
                paged_nodes: paged,
                ..Config::default()
            };
            {
                let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
                db.set_wal_sync(SyncMode::Off);
                db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
                for i in 0..n { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
                for i in 0..n - 1 { db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next"); }
                db.compact().unwrap();
            }
            let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
            db.set_wal_sync(SyncMode::Off);
            for i in n..n + 200 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
            let t = std::time::Instant::now();
            db.compact().unwrap();
            times[slot] = t.elapsed().as_secs_f64() * 1e3;
        }
        let base = *first.get_or_insert((times[0], times[1]));
        println!("  {n:>10}{:>12.1}ms{:>12.1}ms{:>12.2}x", times[0], times[1], times[1] / base.1);
    }
    println!("\n  the last column is growth against the smallest database.");
    println!("  a cost proportional to the change stays at 1.00x; anything above it");
    println!("  is a structure still rebuilt in full.");
}
