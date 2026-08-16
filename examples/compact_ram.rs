//! Peak RSS during a paged compaction, at scale. One size per process run.
use sekejap::{CoreDB, SyncMode};
use serde_json::json;
use std::time::Instant;

fn rss_mb() -> f64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|kb| kb / 1024.0).unwrap_or(0.0)
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.set_wal_sync(SyncMode::Off);
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        for i in 0..n {
            db.put(&format!("p/n{i}"), &json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64}).to_string()).unwrap();
        }
        for i in 0..n - 1 { db.link(&format!("p/n{i}"), &format!("p/n{}", i+1), "next"); }
        db.compact().unwrap();
    }
    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    db.set_wal_sync(SyncMode::Off);
    let base = rss_mb();
    for i in n..n + 1000 {
        db.put(&format!("p/n{i}"), &json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64}).to_string()).unwrap();
    }
    let t = Instant::now();
    db.compact().unwrap();
    let secs = t.elapsed().as_secs_f64();
    let peak = rss_mb();
    println!("{n} nodes: compact {secs:.1}s   RSS {base:.0} MB -> {peak:.0} MB  (+{:.0} MB)", peak - base);
    let rows = db.query("SELECT _key FROM p").unwrap().collect().len();
    let edges = db.query("SELECT _key FROM MATCH (a:p)-[:next]->(b:p)").unwrap().collect().len();
    println!("  intact: {rows} rows (want {}), {edges} edges (want {})", n + 1000, n - 1);
}
