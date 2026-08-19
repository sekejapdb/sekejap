//! What does the first write to an indexed column cost in RAM?
//!
//! `field_index_ref` consults the heap overlay and the mmap'd base in preference
//! order rather than merging them, so a write has to hoist the whole base into
//! the heap before it can change anything — otherwise the write lands nowhere
//! and reads keep answering from the stale mapping.
//!
//! That makes the cost of *one* write proportional to the *whole column*. This
//! measures it: build an indexed store, reopen so the index is served from the
//! mapping, then write a single row and watch the resident set.

use sekejap::{AutoCompact, Config, CoreDB, SyncMode};

fn rss_mb() -> f64 {
    let pid = std::process::id();
    std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()])
        .output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|kb| kb / 1024.0).unwrap_or(0.0)
}

fn load(db: &mut CoreDB, from: usize, n: usize) {
    const CHUNK: usize = 25_000;
    let mut done = 0;
    while done < n {
        let take = CHUNK.min(n - done);
        let rows: Vec<(String, serde_json::Value)> = (from + done..from + done + take)
            .map(|i| (format!("items/n{i}"),
                 serde_json::json!({"_collection":"items","_key":format!("n{i}"),"n":i})))
            .collect();
        db.put_value_bulk(rows).unwrap();
        done += take;
    }
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(500_000);
    let dir = std::env::temp_dir().join(format!("sk_idxram_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let cfg = || Config {
        wal_sync: SyncMode::Off,
        auto_compact: AutoCompact::Off,
        ..Config::default()
    };

    // Build the store, index the column, and fold it all to disk.
    {
        let mut db = CoreDB::open_with_config(&dir, cfg()).unwrap();
        db.execute("CREATE TABLE items (n INTEGER)").unwrap();
        load(&mut db, 0, n);
        db.execute("CREATE INDEX ON items USING btree (n)").unwrap();
        db.compact().unwrap();
        println!("built {} rows with a btree index on `n`", n);
    }

    // Reopen: the index is now served from the mapping, nothing on the heap.
    let mut db = CoreDB::open_with_config(&dir, cfg()).unwrap();
    println!("after reopen:                                  {:.1} MB", rss_mb());

    // Read through the index first, so the read side cannot be blamed for the
    // jump below. Deliberately selective: a wide predicate would materialise
    // half a million result rows and drown the measurement in its own output.
    let hits = db.query("SELECT * FROM items WHERE n = 12345").unwrap().collect().len();
    let before = rss_mb();
    println!("after an indexed read ({} row matched):         {:.1} MB", hits, before);

    // One row. One.
    db.put_value("items/zzz", serde_json::json!({"_collection":"items","_key":"zzz","n":1})).unwrap();
    let after = rss_mb();

    println!("after writing ONE row:                        {:.1} MB", after);
    println!();
    println!("one write cost {:+.1} MB of RAM", after - before);
    println!("that is {:.0} bytes per indexed row already in the store", (after - before) * 1_048_576.0 / n as f64);
    println!();
    println!("RAM proportional to the store, paid by a single write, is Law 1.");

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
