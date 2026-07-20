//! Test: remote-only mode — payloads stay on S3, fetched via block cache.
//!
//! Writer creates DB, compacts, uploads to MinIO.
//! Reader opens with `open_s3` — downloads only the snapshot, payloads
//! are fetched block-by-block from S3 on demand.
//!
//! Requires MinIO running (see s3_minio.rs for setup).
//! Run: cargo test --features s3 --test s3_remote_only -- --nocapture

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
fn test_remote_only_query() {
    let store = match minio_store() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };

    let prefix = "remote-only-test";
    cleanup_minio(&store, prefix);

    // ── Writer: create DB with 100 rows, compact, upload ────────────────
    let w_dir = tempfile::tempdir().unwrap();
    {
        let mut db = sekejap::CoreDB::open(w_dir.path()).unwrap();
        db.execute("CREATE TABLE cities (_key TEXT PRIMARY KEY, name TEXT, pop INTEGER, country TEXT)")
            .unwrap();

        for i in 0..100 {
            db.execute(&format!(
                "INSERT INTO cities (_key, name, pop, country) VALUES ('c{i}', 'City {i}', {pop}, '{country}')",
                i = i,
                pop = 100_000 + i * 5000,
                country = if i % 3 == 0 { "ID" } else if i % 3 == 1 { "MY" } else { "SG" },
            ))
            .unwrap();
        }
        db.compact().unwrap();
    }

    let remote = sekejap::engine::remote::RemoteSync::from_store(store.clone(), prefix).unwrap();
    remote.sync_to_remote(w_dir.path()).unwrap();
    println!("uploaded to MinIO");

    // Show what was uploaded.
    let manifest = remote.get_manifest().unwrap().unwrap();
    for seg in &manifest.segments {
        println!("  segment: {} ({} bytes)", seg.name, seg.size);
    }

    // ── Reader: open_s3 — NO local download ─────────────────────────
    let t0 = Instant::now();
    let db = sekejap::CoreDB::open_s3(
        &remote,
        sekejap::engine::cache::CacheBudget::new(256 * 1024 * 1024), // 256 MB RAM cache
        None,
    ).unwrap();
    println!("open_s3 took: {:.2} ms", t0.elapsed().as_secs_f64() * 1000.0);

    // ── Query: filter + sort ────────────────────────────────────────────
    let t1 = Instant::now();
    let hits = db
        .query("SELECT name, pop, country FROM cities WHERE country = 'ID' ORDER BY pop DESC LIMIT 5")
        .unwrap()
        .collect();
    println!("query took: {:.2} ms ({} rows)", t1.elapsed().as_secs_f64() * 1000.0, hits.len());

    assert_eq!(hits.len(), 5);
    let first = &hits[0];
    let payload = first.payload.as_ref().unwrap();
    assert_eq!(payload.get("country").and_then(|v| v.as_str()), Some("ID"));
    // Highest pop ID city should be c99 (pop=595000) since 99%3==0.
    let first_pop = payload
        .get("pop")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap();
    println!("first result: {} pop={}", payload.get("name").unwrap(), first_pop);
    assert!(first_pop > 500_000);

    // ── Second query: different filter (should hit cache) ───────────────
    let t2 = Instant::now();
    let hits2 = db
        .query("SELECT name FROM cities WHERE country = 'SG'")
        .unwrap()
        .collect();
    println!(
        "second query took: {:.2} ms ({} rows, mostly cached)",
        t2.elapsed().as_secs_f64() * 1000.0,
        hits2.len()
    );
    assert_eq!(hits2.len(), 33); // indices 2,5,8,...,98 => 33 items

    // ── Count query ─────────────────────────────────────────────────────
    let t3 = Instant::now();
    let count_hits = db
        .query("SELECT COUNT(*) FROM cities")
        .unwrap()
        .collect();
    println!(
        "count query took: {:.2} ms",
        t3.elapsed().as_secs_f64() * 1000.0
    );
    assert!(!count_hits.is_empty());
    let cp = count_hits[0].payload.as_ref().unwrap();
    let count = cp
        .get("COUNT(*)")
        .or_else(|| cp.get("count"))
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or_else(|| {
            panic!("unexpected COUNT payload: {:?}", cp);
        });
    assert_eq!(count, 100);

    cleanup_minio(&store, prefix);
    println!("done — all queries succeeded via block cache, no local payloads.bin");
}
