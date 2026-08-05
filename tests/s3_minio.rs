//! Live integration test against a local MinIO instance.
//!
//! Requires:
//!   docker run -d --name minio -p 9000:9000 \
//!     -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
//!     minio/minio server /data
//!   docker exec minio mc alias set local http://localhost:9000 minioadmin minioadmin
//!   docker exec minio mc mb local/sekejap-test
//!
//! Run: cargo test --features s3 --test s3_minio -- --nocapture

#![cfg(feature = "s3")]

use std::sync::Arc;

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

#[test]
fn test_writer_reader_via_minio() {
    let store = match minio_store() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping MinIO test: {e}");
            return;
        }
    };

    // Clean any objects a previous run left under this prefix, so the
    // generation assertion below sees a fresh remote.
    {
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
                "field_table.bin",
                "field_table.bin.1",
                "field_table.bin.2",
                "nodes.bin",
                "idx.bin",
                "adj_fwd.bin",
                "adj_rev.bin",
                "slugs.bin",
                "dict.bin",
                "collections.bin",
                "edgemeta.bin",
                "spatial.bin",
                "gin.bin",
                "search.bin",
                "edges.bin",
                "edge_meta.bin",
            ] {
                let p = object_store::path::Path::from(format!("integration-test/{name}"));
                let _ = store.delete(&p).await;
            }
        });
    }

    // ── Writer: create DB, insert data, compact, upload ─────────────────
    let w_dir = tempfile::tempdir().unwrap();
    let writer_remote = sekejap::engine::remote::RemoteSync::from_store(
        store.clone(),
        "integration-test",
    )
    .unwrap();

    {
        let mut db = sekejap::CoreDB::open(w_dir.path()).unwrap();
        db.execute("CREATE TABLE cities (_key TEXT PRIMARY KEY, name TEXT, pop INTEGER)")
            .unwrap();
        db.execute("INSERT INTO cities (_key, name, pop) VALUES ('jkt', 'Jakarta', 10000000)")
            .unwrap();
        db.execute("INSERT INTO cities (_key, name, pop) VALUES ('sby', 'Surabaya', 3000000)")
            .unwrap();
        db.execute("INSERT INTO cities (_key, name, pop) VALUES ('bdg', 'Bandung', 2500000)")
            .unwrap();
        db.compact().unwrap();
    }

    // Upload to MinIO.
    writer_remote.sync_to_remote(w_dir.path()).unwrap();
    let gen = writer_remote.latest_generation().unwrap();
    assert_eq!(gen, 1);
    println!("uploaded generation {gen}");

    // ── Reader: pull from MinIO, open read-only, query ──────────────────
    let r_dir = tempfile::tempdir().unwrap();
    let reader_remote = sekejap::engine::remote::RemoteSync::from_store(
        store.clone(),
        "integration-test",
    )
    .unwrap();

    reader_remote.sync_from_remote(r_dir.path()).unwrap();
    println!("downloaded segments to {:?}", r_dir.path());

    let db = sekejap::CoreDB::open_read_only(r_dir.path()).unwrap();
    let hits = db
        .query("SELECT name, pop FROM cities ORDER BY pop DESC")
        .unwrap()
        .collect();
    println!("reader got {} rows", hits.len());
    assert_eq!(hits.len(), 3);

    let first = &hits[0];
    let payload = first.payload.as_ref().unwrap();
    assert_eq!(payload.get("name").and_then(|v| v.as_str()), Some("Jakarta"));
    drop(db);

    // ── Writer: add more data, compact, re-upload ───────────────────────
    {
        let mut db = sekejap::CoreDB::open(w_dir.path()).unwrap();
        db.execute("INSERT INTO cities (_key, name, pop) VALUES ('smg', 'Semarang', 1800000)")
            .unwrap();
        db.compact().unwrap();
    }
    writer_remote.sync_to_remote(w_dir.path()).unwrap();
    let gen2 = writer_remote.latest_generation().unwrap();
    assert_eq!(gen2, 2);
    println!("uploaded generation {gen2}");

    // ── Reader: detect new gen, refresh, verify ─────────────────────────
    let latest = reader_remote.latest_generation().unwrap();
    assert_eq!(latest, 2);

    reader_remote.sync_from_remote(r_dir.path()).unwrap();
    let db2 = sekejap::CoreDB::open_read_only(r_dir.path()).unwrap();
    let hits2 = db2.query("SELECT name FROM cities").unwrap().collect();
    assert_eq!(hits2.len(), 4);
    println!("reader refreshed: {} cities", hits2.len());

    // ── Cleanup: delete test objects from MinIO ─────────────────────────
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
            "field_table.bin",
            "field_table.bin.1",
            "field_table.bin.2",
            "nodes.bin",
            "idx.bin",
            "adj_fwd.bin",
            "adj_rev.bin",
            "slugs.bin",
            "dict.bin",
            "collections.bin",
            "edgemeta.bin",
            "spatial.bin",
            "gin.bin",
            "search.bin",
            "edges.bin",
            "edge_meta.bin",
        ] {
            let p = object_store::path::Path::from(format!("integration-test/{name}"));
            let _ = store.delete(&p).await;
        }
    });
    println!("cleaned up MinIO objects");
}
