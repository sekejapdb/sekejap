//! Benchmark: direct local file vs S3 (MinIO) sync+open+query.
//!
//! Requires MinIO running (see s3_minio.rs for setup).
//! Run: cargo test --features s3 --test s3_bench -- --nocapture

#![cfg(feature = "s3")]

use std::sync::Arc;
use std::time::Instant;

fn minio_store() -> Result<Arc<dyn object_store::ObjectStore>, String> {
    use object_store::aws::AmazonS3Builder;
    let store = AmazonS3Builder::new()
        .with_bucket_name("sekejap-test")
        .with_endpoint("http://localhost:9000")
        .with_access_key_id("minioadmin")
        .with_secret_access_key("minioadmin")
        .with_region("us-east-1")
        .with_allow_http(true)
        .build()
        .map_err(|e| format!("MinIO init: {e}"))?;
    Ok(Arc::new(store))
}

fn cleanup_minio(store: &Arc<dyn object_store::ObjectStore>, prefix: &str) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        use object_store::{ObjectStore, ObjectStoreExt};
        for name in &[
            "manifest.json",
            "snapshot.json",
            "payloads.bin",
            "gin.bin",
            "search.bin",
            "edges.bin",
            "edge_meta.bin",
        ] {
            let p = object_store::path::Path::from(format!("{prefix}/{name}"));
            let _ = store.delete(&p).await;
        }
    });
}

#[test]
fn bench_local_vs_s3() {
    let store = match minio_store() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };

    let prefix = "bench-local-vs-s3";
    cleanup_minio(&store, prefix);

    // ── Prepare: create a DB with realistic data ────────────────────────
    let w_dir = tempfile::tempdir().unwrap();
    {
        let mut db = sekejap::CoreDB::open(w_dir.path()).unwrap();
        db.execute("CREATE TABLE cities (_key TEXT PRIMARY KEY, name TEXT, pop INTEGER, country TEXT)")
            .unwrap();

        // Insert 500 rows
        for i in 0..500 {
            let sql = format!(
                "INSERT INTO cities (_key, name, pop, country) VALUES ('c{i}', 'City {i}', {pop}, 'ID')",
                i = i,
                pop = 100_000 + i * 1000,
            );
            db.execute(&sql).unwrap();
        }
        db.compact().unwrap();
    }

    // Show segment sizes
    let segment_total: u64 = ["snapshot.json", "payloads.bin", "edges.bin", "edge_meta.bin"]
        .iter()
        .filter_map(|n| std::fs::metadata(w_dir.path().join(n)).ok())
        .map(|m| m.len())
        .sum();
    println!("\n=== Benchmark: Direct File vs S3 (MinIO localhost) ===");
    println!("dataset: 500 rows, segment total: {:.1} KB\n", segment_total as f64 / 1024.0);

    // Upload to MinIO
    let remote = sekejap::engine::remote::RemoteSync::from_store(store.clone(), prefix).unwrap();
    let t = Instant::now();
    remote.sync_to_remote(w_dir.path()).unwrap();
    println!("upload to MinIO:       {:>8.2} ms", t.elapsed().as_secs_f64() * 1000.0);

    // ── Benchmark: Direct local open + query ────────────────────────────
    const RUNS: usize = 5;
    let query = "SELECT name, pop FROM cities WHERE pop > 300000 ORDER BY pop DESC LIMIT 10";

    println!("\n--- Direct local file (open + query) ---");
    let mut local_open_ms = Vec::new();
    let mut local_query_ms = Vec::new();
    let mut local_total_ms = Vec::new();

    for i in 0..RUNS {
        let t0 = Instant::now();
        let db = sekejap::CoreDB::open_read_only(w_dir.path()).unwrap();
        let open_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let hits = db.query(query).unwrap().collect();
        let query_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;

        if i > 0 {
            // skip warmup
            local_open_ms.push(open_ms);
            local_query_ms.push(query_ms);
            local_total_ms.push(total_ms);
        }
        println!(
            "  run {i}: open={open_ms:>8.2}ms  query={query_ms:>8.2}ms  total={total_ms:>8.2}ms  rows={}",
            hits.len()
        );
        drop(db);
    }

    // ── Benchmark: S3 sync + open + query (cold — empty cache dir) ──────
    println!("\n--- S3 cold sync + open + query (fresh dir each time) ---");
    let mut s3_sync_ms = Vec::new();
    let mut s3_open_ms = Vec::new();
    let mut s3_query_ms = Vec::new();
    let mut s3_total_ms = Vec::new();

    for i in 0..RUNS {
        let r_dir = tempfile::tempdir().unwrap();
        let reader = sekejap::engine::remote::RemoteSync::from_store(store.clone(), prefix).unwrap();

        let t0 = Instant::now();
        reader.sync_from_remote(r_dir.path()).unwrap();
        let sync_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let db = sekejap::CoreDB::open_read_only(r_dir.path()).unwrap();
        let open_ms = t1.elapsed().as_secs_f64() * 1000.0;

        let t2 = Instant::now();
        let hits = db.query(query).unwrap().collect();
        let query_ms = t2.elapsed().as_secs_f64() * 1000.0;
        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;

        if i > 0 {
            s3_sync_ms.push(sync_ms);
            s3_open_ms.push(open_ms);
            s3_query_ms.push(query_ms);
            s3_total_ms.push(total_ms);
        }
        println!(
            "  run {i}: sync={sync_ms:>8.2}ms  open={open_ms:>8.2}ms  query={query_ms:>8.2}ms  total={total_ms:>8.2}ms  rows={}",
            hits.len()
        );
        drop(db);
    }

    // ── Benchmark: S3 warm sync (files already cached) ──────────────────
    println!("\n--- S3 warm sync + open + query (files already present) ---");
    let warm_dir = tempfile::tempdir().unwrap();
    let warm_reader = sekejap::engine::remote::RemoteSync::from_store(store.clone(), prefix).unwrap();
    warm_reader.sync_from_remote(warm_dir.path()).unwrap(); // pre-populate

    let mut warm_sync_ms = Vec::new();
    let mut warm_open_ms = Vec::new();
    let mut warm_query_ms = Vec::new();
    let mut warm_total_ms = Vec::new();

    for i in 0..RUNS {
        let t0 = Instant::now();
        warm_reader.sync_from_remote(warm_dir.path()).unwrap();
        let sync_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let db = sekejap::CoreDB::open_read_only(warm_dir.path()).unwrap();
        let open_ms = t1.elapsed().as_secs_f64() * 1000.0;

        let t2 = Instant::now();
        let hits = db.query(query).unwrap().collect();
        let query_ms = t2.elapsed().as_secs_f64() * 1000.0;
        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;

        if i > 0 {
            warm_sync_ms.push(sync_ms);
            warm_open_ms.push(open_ms);
            warm_query_ms.push(query_ms);
            warm_total_ms.push(total_ms);
        }
        println!(
            "  run {i}: sync={sync_ms:>8.2}ms  open={open_ms:>8.2}ms  query={query_ms:>8.2}ms  total={total_ms:>8.2}ms  rows={}",
            hits.len()
        );
        drop(db);
    }

    // ── Summary ─────────────────────────────────────────────────────────
    let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let min = |v: &[f64]| v.iter().cloned().fold(f64::INFINITY, f64::min);

    println!("\n=== Summary (excluding warmup run 0) ===");
    println!("                          avg ms     min ms");
    println!("  local  open:          {:>8.2}   {:>8.2}", avg(&local_open_ms), min(&local_open_ms));
    println!("  local  query:         {:>8.2}   {:>8.2}", avg(&local_query_ms), min(&local_query_ms));
    println!("  local  total:         {:>8.2}   {:>8.2}", avg(&local_total_ms), min(&local_total_ms));
    println!();
    println!("  s3 cold sync:         {:>8.2}   {:>8.2}", avg(&s3_sync_ms), min(&s3_sync_ms));
    println!("  s3 cold open:         {:>8.2}   {:>8.2}", avg(&s3_open_ms), min(&s3_open_ms));
    println!("  s3 cold query:        {:>8.2}   {:>8.2}", avg(&s3_query_ms), min(&s3_query_ms));
    println!("  s3 cold total:        {:>8.2}   {:>8.2}", avg(&s3_total_ms), min(&s3_total_ms));
    println!();
    println!("  s3 warm sync:         {:>8.2}   {:>8.2}", avg(&warm_sync_ms), min(&warm_sync_ms));
    println!("  s3 warm open:         {:>8.2}   {:>8.2}", avg(&warm_open_ms), min(&warm_open_ms));
    println!("  s3 warm query:        {:>8.2}   {:>8.2}", avg(&warm_query_ms), min(&warm_query_ms));
    println!("  s3 warm total:        {:>8.2}   {:>8.2}", avg(&warm_total_ms), min(&warm_total_ms));
    println!();
    println!(
        "  overhead cold: {:>+8.2} ms ({:.1}x slower)",
        avg(&s3_total_ms) - avg(&local_total_ms),
        avg(&s3_total_ms) / avg(&local_total_ms)
    );
    println!(
        "  overhead warm: {:>+8.2} ms ({:.1}x slower)",
        avg(&warm_total_ms) - avg(&local_total_ms),
        avg(&warm_total_ms) / avg(&local_total_ms)
    );

    // ── Benchmark: Remote-only (no local payloads.bin, block cache) ────
    println!("\n--- Remote-only (block cache from S3, no local files) ---");
    let mut ro_open_ms = Vec::new();
    let mut ro_query_ms = Vec::new();
    let mut ro_total_ms = Vec::new();

    for i in 0..RUNS {
        let t0 = Instant::now();
        let db = sekejap::CoreDB::open_s3(
            &remote,
            sekejap::engine::cache::CacheBudget::new(256 * 1024 * 1024),
            None,
        ).unwrap();
        let open_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let hits = db.query(query).unwrap().collect();
        let query_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;

        if i > 0 {
            ro_open_ms.push(open_ms);
            ro_query_ms.push(query_ms);
            ro_total_ms.push(total_ms);
        }
        println!(
            "  run {i}: open={open_ms:>8.2}ms  query={query_ms:>8.2}ms  total={total_ms:>8.2}ms  rows={}",
            hits.len()
        );
        drop(db);
    }

    // ── Updated Summary ─────────────────────────────────────────────────
    println!("\n=== Summary (excluding warmup run 0) ===");
    println!("                          avg ms     min ms");
    println!("  local  open:          {:>8.2}   {:>8.2}", avg(&local_open_ms), min(&local_open_ms));
    println!("  local  query:         {:>8.2}   {:>8.2}", avg(&local_query_ms), min(&local_query_ms));
    println!("  local  total:         {:>8.2}   {:>8.2}", avg(&local_total_ms), min(&local_total_ms));
    println!();
    println!("  s3 cold sync:         {:>8.2}   {:>8.2}", avg(&s3_sync_ms), min(&s3_sync_ms));
    println!("  s3 cold open:         {:>8.2}   {:>8.2}", avg(&s3_open_ms), min(&s3_open_ms));
    println!("  s3 cold query:        {:>8.2}   {:>8.2}", avg(&s3_query_ms), min(&s3_query_ms));
    println!("  s3 cold total:        {:>8.2}   {:>8.2}", avg(&s3_total_ms), min(&s3_total_ms));
    println!();
    println!("  s3 warm sync:         {:>8.2}   {:>8.2}", avg(&warm_sync_ms), min(&warm_sync_ms));
    println!("  s3 warm open:         {:>8.2}   {:>8.2}", avg(&warm_open_ms), min(&warm_open_ms));
    println!("  s3 warm query:        {:>8.2}   {:>8.2}", avg(&warm_query_ms), min(&warm_query_ms));
    println!("  s3 warm total:        {:>8.2}   {:>8.2}", avg(&warm_total_ms), min(&warm_total_ms));
    println!();
    println!("  remote-only open:     {:>8.2}   {:>8.2}", avg(&ro_open_ms), min(&ro_open_ms));
    println!("  remote-only query:    {:>8.2}   {:>8.2}", avg(&ro_query_ms), min(&ro_query_ms));
    println!("  remote-only total:    {:>8.2}   {:>8.2}", avg(&ro_total_ms), min(&ro_total_ms));
    println!();
    println!(
        "  overhead cold:    {:>+8.2} ms ({:.1}x)",
        avg(&s3_total_ms) - avg(&local_total_ms),
        avg(&s3_total_ms) / avg(&local_total_ms)
    );
    println!(
        "  overhead warm:    {:>+8.2} ms ({:.1}x)",
        avg(&warm_total_ms) - avg(&local_total_ms),
        avg(&warm_total_ms) / avg(&local_total_ms)
    );
    println!(
        "  overhead remote:  {:>+8.2} ms ({:.1}x)",
        avg(&ro_total_ms) - avg(&local_total_ms),
        avg(&ro_total_ms) / avg(&local_total_ms)
    );

    cleanup_minio(&store, prefix);
    println!("\ncleaned up");
}
