//! Insertion scaling probe — the measurement that decides whether the write
//! path is linear.
//!
//! The question is not "how fast is an insert". It is "does the cost of an
//! insert stay the same as the store grows". A store that rebuilds itself
//! periodically has insertion cost O(N^2/K): every rebuild is O(store), and
//! there are O(N/K) of them. That curve is invisible at 100k rows and fatal at
//! 100M.
//!
//! So this reports time-per-insert at increasing scale. Flat means the shape is
//! linear. Rising means it is not, and no constant-factor work will save it.
//!
//! Run:  cargo run --release --example insert_scaling -- [total] [batch]

use std::time::Instant;

/// Resident set size in MB, read from `ps`. Crude, but it does not perturb the
/// allocator the way an in-process probe would.
fn rss_mb() -> u64 {
    let pid = std::process::id();
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let total: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let batch: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50_000);

    let dir = std::env::temp_dir().join(format!("sk_insert_scaling_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut db = sekejap::open(&dir).expect("open");

    println!("target {} rows, batch {}", total, batch);
    println!();
    println!("{:>12} {:>12} {:>12} {:>12} {:>10}", "rows", "batch_ms", "us/insert", "worst_ms", "rss_mb");
    println!("{}", "-".repeat(62));

    let t_all = Instant::now();
    let mut done = 0usize;
    // The worst single batch is where a rebuild shows up: a stall that a mean
    // would hide entirely.
    let mut worst_batch_ms = 0f64;

    while done < total {
        let n = batch.min(total - done);
        let t = Instant::now();
        for i in done..done + n {
            let key = format!("items/{}", i);
            let doc = format!(
                r#"{{"_collection":"items","_key":"{}","n":{},"name":"item {}","tag":"t{}"}}"#,
                i, i, i, i % 97
            );
            db.put(&key, &doc).expect("put");
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if ms > worst_batch_ms {
            worst_batch_ms = ms;
        }
        done += n;
        println!(
            "{:>12} {:>12.1} {:>12.2} {:>12.1} {:>10}",
            done,
            ms,
            (ms * 1000.0) / n as f64,
            worst_batch_ms,
            rss_mb()
        );
    }

    let secs = t_all.elapsed().as_secs_f64();
    println!();
    println!("total       {:.2} s for {} rows", secs, total);
    println!("mean        {:.2} us/insert", secs * 1_000_000.0 / total as f64);
    println!("worst batch {:.1} ms  ({:.2} us/insert in that batch)", worst_batch_ms, worst_batch_ms * 1000.0 / batch as f64);
    println!("rss         {} MB", rss_mb());
    println!();
    println!("VERDICT: compare us/insert of the FIRST batch against the LAST.");
    println!("  flat   -> cost is proportional to the change. linear. 100M reachable.");
    println!("  rising -> cost is proportional to the store. quadratic. 100M unreachable.");

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
