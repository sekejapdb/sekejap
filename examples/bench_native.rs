//! Native-Rust baseline for the cross-wrapper micro-benchmark: N point-lookup
//! queries against a disk-backed DB. Each wrapper runs the SAME workload and the
//! gap vs this number is the binding's FFI + serialization overhead.
//!
//!   cargo run --release --example bench_native      (env N=<iters>, default 50000)

use sekejap::CoreDB;
use std::time::Instant;

fn main() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)").unwrap();
    for i in 0..1000 {
        db.execute(&format!("INSERT INTO t (_key, v) VALUES ('k{i}', {i})")).unwrap();
    }

    let n: usize = std::env::var("N").ok().and_then(|v| v.parse().ok()).unwrap_or(50_000);
    let sql = "SELECT v FROM t WHERE _key = 'k500'";
    let _ = db.query(sql).unwrap().collect(); // warm

    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(db.query(sql).unwrap().collect().len());
    }
    let el = t.elapsed().as_secs_f64();
    // Machine-parseable line: "<lang> <ops_per_sec> <us_per_op>"
    println!("rust {:.0} {:.3}", n as f64 / el, el * 1e6 / n as f64);
}
