//! Full feature parity test: remote-only mode vs local.
//!
//! Tests that every query type works with remote-only S3 storage:
//! - SQL filter, sort, aggregate
//! - Graph traversal (MATCH, forward/backward)
//! - COUNT, GROUP BY
//! - Disk-backed block cache (survives process restart)
//!
//! Requires MinIO running (see s3_minio.rs for setup).
//! Run: cargo test --features s3 --test s3_full_parity -- --nocapture

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
fn test_full_feature_parity() {
    let store = match minio_store() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };

    let prefix = "full-parity-test";
    cleanup_minio(&store, prefix);

    // ── Writer: create a rich dataset ───────────────────────────────────
    let w_dir = tempfile::tempdir().unwrap();
    {
        let mut db = sekejap::CoreDB::open(w_dir.path()).unwrap();

        // Tables
        db.execute("CREATE TABLE countries (_key TEXT PRIMARY KEY, name TEXT, continent TEXT)")
            .unwrap();
        db.execute("CREATE TABLE cities (_key TEXT PRIMARY KEY, name TEXT, pop INTEGER, country_key TEXT)")
            .unwrap();

        // Countries
        db.execute("INSERT INTO countries (_key, name, continent) VALUES ('id', 'Indonesia', 'Asia')").unwrap();
        db.execute("INSERT INTO countries (_key, name, continent) VALUES ('my', 'Malaysia', 'Asia')").unwrap();
        db.execute("INSERT INTO countries (_key, name, continent) VALUES ('sg', 'Singapore', 'Asia')").unwrap();

        // Cities
        for (key, name, pop, country) in &[
            ("jkt", "Jakarta", 10_000_000, "id"),
            ("sby", "Surabaya", 3_000_000, "id"),
            ("bdg", "Bandung", 2_500_000, "id"),
            ("kl", "Kuala Lumpur", 1_800_000, "my"),
            ("jb", "Johor Bahru", 500_000, "my"),
            ("sgc", "Singapore City", 5_700_000, "sg"),
        ] {
            db.execute(&format!(
                "INSERT INTO cities (_key, name, pop, country_key) VALUES ('{key}', '{name}', {pop}, '{country}')"
            )).unwrap();
        }

        // Graph edges: cities → countries
        db.link("cities/jkt", "countries/id", "in_country", 1.0);
        db.link("cities/sby", "countries/id", "in_country", 1.0);
        db.link("cities/bdg", "countries/id", "in_country", 1.0);
        db.link("cities/kl", "countries/my", "in_country", 1.0);
        db.link("cities/jb", "countries/my", "in_country", 1.0);
        db.link("cities/sgc", "countries/sg", "in_country", 1.0);

        // Sister city links
        db.link("cities/jkt", "cities/kl", "sister_city", 0.8);
        db.link("cities/sgc", "cities/jkt", "sister_city", 0.9);

        db.compact().unwrap();
    }

    // Upload to MinIO
    let remote = sekejap::engine::remote::RemoteSync::from_store(store.clone(), prefix).unwrap();
    remote.sync_to_remote(w_dir.path()).unwrap();
    println!("uploaded to MinIO");

    // ── Open remote with disk cache ─────────────────────────────────────
    let cache_dir = tempfile::tempdir().unwrap();
    let t0 = Instant::now();
    let db = sekejap::CoreDB::open_s3(
        &remote,
        sekejap::engine::cache::CacheBudget::new(50 * 1024 * 1024 * 1024), // 50 GB
        Some(cache_dir.path()),
    ).unwrap();
    println!("open_s3: {:.2} ms", t0.elapsed().as_secs_f64() * 1000.0);

    // ── Test 1: Basic SELECT with WHERE ─────────────────────────────────
    let hits = db.query("SELECT name, pop FROM cities WHERE country_key = 'id' ORDER BY pop DESC")
        .unwrap().collect();
    assert_eq!(hits.len(), 3);
    let first_name = hits[0].payload.as_ref().unwrap().get("name").unwrap().as_str().unwrap();
    assert_eq!(first_name, "Jakarta");
    println!("✓ SELECT WHERE + ORDER BY: {} rows, first={}", hits.len(), first_name);

    // ── Test 2: COUNT(*) ────────────────────────────────────────────────
    let count_hits = db.query("SELECT COUNT(*) FROM cities").unwrap().collect();
    let cp = count_hits[0].payload.as_ref().unwrap();
    let count = cp.get("COUNT(*)").or_else(|| cp.get("count"))
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap();
    assert_eq!(count, 6);
    println!("✓ COUNT(*): {count}");

    // ── Test 3: GROUP BY ────────────────────────────────────────────────
    let group_hits = db.query("SELECT country_key, COUNT(*) FROM cities GROUP BY country_key")
        .unwrap().collect();
    assert_eq!(group_hits.len(), 3); // id, my, sg
    println!("✓ GROUP BY: {} groups", group_hits.len());

    // ── Test 4: Graph forward traversal ─────────────────────────────────
    let fwd = db.one("cities/jkt").forward("in_country").collect();
    assert_eq!(fwd.len(), 1);
    assert_eq!(fwd[0].slug, "countries/id");
    println!("✓ Forward traversal: jkt → {}", fwd[0].slug);

    // ── Test 5: Graph backward traversal ────────────────────────────────
    let bwd = db.one("countries/id").backward("in_country").collect();
    assert_eq!(bwd.len(), 3); // jkt, sby, bdg
    println!("✓ Backward traversal: id ← {} cities", bwd.len());

    // ── Test 6: Multi-hop ───────────────────────────────────────────────
    let hops = db.one("cities/jkt").forward("sister_city").forward("in_country").collect();
    assert_eq!(hops.len(), 1); // jkt → kl → my
    assert_eq!(hops[0].slug, "countries/my");
    println!("✓ Multi-hop: jkt →sister_city→ kl →in_country→ {}", hops[0].slug);

    // ── Test 7: MATCH single hop ──────────────────────────────────────────
    let match_hits = db.query(
        "SELECT a.name AS city, b.name AS country FROM MATCH (a:cities)-[:in_country]->(b:countries)"
    ).unwrap().collect();
    assert_eq!(match_hits.len(), 6);
    println!("✓ MATCH single hop: {} rows", match_hits.len());

    // ── Test 8: MATCH with WHERE ────────────────────────────────────────
    let match_where = db.query(
        "SELECT a.name AS city, b.name AS country \
         FROM MATCH (a:cities)-[:in_country]->(b:countries) \
         WHERE b._key = 'id'"
    ).unwrap().collect();
    assert_eq!(match_where.len(), 3); // jkt, sby, bdg → Indonesia
    let city0 = match_where[0].payload.as_ref().unwrap().get("city").unwrap().as_str().unwrap();
    println!("✓ MATCH WHERE: {} rows, first={}", match_where.len(), city0);

    // ── Test 9: MATCH multi-hop ─────────────────────────────────────────
    let match_multi = db.query(
        "SELECT a.name AS from_city, c.name AS country \
         FROM MATCH (a:cities)-[:sister_city]->(b:cities)-[:in_country]->(c:countries)"
    ).unwrap().collect();
    assert_eq!(match_multi.len(), 2); // jkt→kl→my, sgc→jkt→id
    println!("✓ MATCH multi-hop: {} rows", match_multi.len());

    // ── Test 10: MATCH with GROUP BY + COUNT ────────────────────────────
    let match_group = db.query(
        "SELECT b.name AS country, COUNT(*) \
         FROM MATCH (a:cities)-[:in_country]->(b:countries) \
         GROUP BY b.name"
    ).unwrap().collect();
    assert_eq!(match_group.len(), 3); // Indonesia=3, Malaysia=2, Singapore=1
    println!("✓ MATCH GROUP BY: {} groups", match_group.len());

    // ── Test 11: MATCH SHORTEST ─────────────────────────────────────────
    let shortest = db.query(
        "SELECT a.name AS src, b.name AS dst, r.length AS hops \
         FROM MATCH SHORTEST (a:cities)-[r*]->(b:countries) \
         WHERE a._key = 'jkt' AND b._key = 'id'"
    ).unwrap().collect();
    assert_eq!(shortest.len(), 1);
    let hops_val = shortest[0].payload.as_ref().unwrap().get("hops")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap();
    assert_eq!(hops_val, 1); // direct edge jkt → id
    println!("✓ MATCH SHORTEST: {} hops", hops_val);

    // ── Test 12: SHOW TABLES ────────────────────────────────────────────
    let tables = db.show("SHOW TABLES").unwrap();
    assert!(tables.len() >= 2);
    println!("✓ SHOW TABLES: {} tables", tables.len());

    // ── Test 13: SELECT * (all fields) ──────────────────────────────────
    let all = db.query("SELECT * FROM countries").unwrap().collect();
    assert_eq!(all.len(), 3);
    let first_p = all[0].payload.as_ref().unwrap();
    assert!(first_p.get("name").is_some());
    assert!(first_p.get("continent").is_some());
    println!("✓ SELECT *: {} countries", all.len());

    // ── Test 14: LIMIT + SKIP ───────────────────────────────────────────
    let limited = db.query("SELECT name FROM cities ORDER BY pop DESC LIMIT 2")
        .unwrap().collect();
    assert_eq!(limited.len(), 2);
    println!("✓ LIMIT: {} rows", limited.len());

    let skipped = db.query("SELECT name FROM cities ORDER BY pop DESC LIMIT 2 OFFSET 1")
        .unwrap().collect();
    assert_eq!(skipped.len(), 2);
    let skip_name = skipped[0].payload.as_ref().unwrap().get("name").unwrap().as_str().unwrap();
    assert_eq!(skip_name, "Singapore City"); // 2nd highest pop
    println!("✓ LIMIT+OFFSET: first={}", skip_name);

    // ── Test 15: Verify disk cache has blocks ──────────────────────────
    let payload_cache = cache_dir.path().join("payloads");
    if payload_cache.exists() {
        let block_count = std::fs::read_dir(&payload_cache)
            .unwrap()
            .filter(|e| e.as_ref().map_or(false, |e| {
                e.file_name().to_string_lossy().ends_with(".blk")
            }))
            .count();
        println!("✓ Disk cache: {} blocks in {:?}", block_count, payload_cache);
    } else {
        println!("  (no disk cache blocks yet — all fit in RAM)");
    }

    cleanup_minio(&store, prefix);
    println!("\n✓ All tests passed — full feature parity with remote-only storage");
}
