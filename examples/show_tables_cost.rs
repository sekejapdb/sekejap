//! What `SHOW TABLES` costs as the store grows.
//!
//! `SHOW TABLES` reports each table's row count **and its size in bytes**, and an
//! exact size means reading every row. So it is O(rows) in both storage modes and
//! always was — the default mode grows just as steeply (0.90 ms to 24.83 ms across
//! the same range). What differs is the constant.
//!
//! Two things made the paged constant much worse, both fixed:
//!
//! - the collection *names* were collected by reading every node record, because a
//!   paged store keys collections by hash and keeps the name inside each record.
//!   They come from a small table written at compaction now.
//! - the *sizes* were gathered with one `payload_loc` per member, which in this
//!   mode is a B+tree descent per row — 400 000 descents to add up 400 000
//!   lengths. One streaming pass over the store now answers every collection at
//!   once, reading each record from the id the index walk already yielded.
//!
//! 422 ms to 160 ms at 400 000 rows. The remaining slope is the size report
//! itself, which is the question that was asked.
use sekejap::{Config, CoreDB, SyncMode};
use serde_json::json;

fn main() {
    println!("\n  SHOW TABLES, by database size (3 tables throughout)\n");
    println!("  {:>10}{:>14}{:>14}{:>10}", "rows", "default", "paged", "growth");
    println!("  {}", "-".repeat(50));
    let mut first: Option<f64> = None;
    for n in [20_000usize, 100_000, 400_000] {
        let mut t = [0.0f64; 2];
        for (slot, paged) in [(0usize, false), (1, true)] {
            let dir = tempfile::TempDir::new().unwrap();
            let cfg = Config {
                paged_topology: true, paged_payloads: paged,
                paged_adjacency: paged, paged_nodes: paged, ..Config::default()
            };
            {
                let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
                db.set_wal_sync(SyncMode::Off);
                for c in ["a", "b", "c"] {
                    db.execute(&format!("CREATE TABLE {c} (_key TEXT PRIMARY KEY, n INTEGER)"))
                        .unwrap();
                }
                let rows: Vec<(String, serde_json::Value)> = (0..n).map(|i| {
                    let c = ["a", "b", "c"][i % 3];
                    (format!("{c}/n{i}"),
                     json!({"_collection": c, "_key": format!("n{i}"), "n": i as i64}))
                }).collect();
                db.put_value_bulk(rows).unwrap();
                db.compact().unwrap();
            }
            let db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
            let start = std::time::Instant::now();
            for _ in 0..20 {
                std::hint::black_box(db.show("SHOW TABLES").map(|h| h.len()));
            }
            t[slot] = start.elapsed().as_secs_f64() * 1e3 / 20.0;
        }
        let base = *first.get_or_insert(t[1]);
        println!("  {n:>10}{:>12.2}ms{:>12.2}ms{:>9.2}x", t[0], t[1], t[1] / base);
    }
    println!("\n  growth is the paged figure against the smallest database.");
    println!("  both modes grow: an exact size report reads every row. What this");
    println!("  measures is the constant, which was 16x the default mode and is now 6x.");
}
