//! SKBIN must be a first-class payload format across the WHOLE surface — every
//! DML/DDL path must return correct field VALUES (not just correct row counts).
//! These tests force SKBIN by compacting under `payload_binary`, then assert the
//! actual values that the raw-JSON-only fast paths silently drop.

use sekejap::{Config, CoreDB};
use serde_json::Value;

fn skbin_cfg() -> Config {
    Config { payload_binary: true, ..Config::default() }
}

/// Insert varied records, link them, and compact so payloads are SKBIN on disk.
fn seed(dir: &std::path::Path) {
    seed_cfg(dir, skbin_cfg());
}

fn seed_cfg(dir: &std::path::Path, cfg: Config) {
    let mut db = CoreDB::open_with_config(dir, cfg).unwrap();
    for i in 0..200 {
        let status = if i % 2 == 0 { "shipped" } else { "pending" };
        db.put(
            &format!("orders/o{i:04}"),
            &format!(
                r#"{{"_collection":"orders","_key":"o{i:04}","qty":{},"customer":"cust-{:02}","status":"{}","amount":{},"active":{},"note":"order {i} handled at the warehouse facility today"}}"#,
                i % 10, i % 20, status, i * 3, i % 2 == 0
            ),
        )
        .unwrap();
    }
    for i in 0..199 {
        db.link(&format!("orders/o{i:04}"), &format!("orders/o{:04}", i + 1), "next");
    }
    db.compact().unwrap(); // records are now SKBIN on disk
}

#[test]
fn skbin_projection_returns_real_values() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    let hits = db
        .query("SELECT _key, qty, customer, status, amount FROM orders WHERE _key = 'o0005'")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["qty"], 5, "projection must return real qty, not empty");
    assert_eq!(p["customer"], "cust-05");
    assert_eq!(p["status"], "pending");
    assert_eq!(p["amount"], 15);
}

#[test]
fn skbin_multi_row_projection_returns_real_values() {
    // Multi-row projection uses the batched small-payload path (raw_map) — the
    // one that previously byte-searched SKBIN bytes and returned empty fields.
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    let hits = db.query("SELECT _key, customer, qty FROM orders WHERE qty = 5").unwrap().collect();
    assert!(hits.len() >= 2, "expected many rows with qty=5");
    for h in &hits {
        let p = h.payload.as_ref().unwrap();
        assert!(p["customer"].is_string(), "batched projection must return real customer");
        assert_eq!(p["qty"], 5);
    }
}

#[test]
fn skbin_order_by_sorts_by_real_values() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    let hits = db
        .query("SELECT _key, qty FROM orders ORDER BY qty DESC, _key ASC LIMIT 3")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 3);
    // qty ranges 0..9; top must be 9, and the sort key must be real (not empty).
    assert_eq!(hits[0].payload.as_ref().unwrap()["qty"], 9, "ORDER BY must see real qty");
    assert_eq!(hits[1].payload.as_ref().unwrap()["qty"], 9);
    assert_eq!(hits[2].payload.as_ref().unwrap()["qty"], 9);
}

#[test]
fn skbin_group_by_counts_real_values() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    let hits = db
        .query("SELECT status, COUNT(*) AS n FROM orders GROUP BY status")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 2, "two status groups");
    for h in &hits {
        let p = h.payload.as_ref().unwrap();
        assert!(p["status"].is_string(), "GROUP BY key must be a real value");
        assert_eq!(p["n"], 100, "each status has 100 rows");
    }
}

#[test]
fn skbin_match_projects_real_dest_fields() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    // Project NON-_key fields of the destination — these come from the payload,
    // so they exercise the MATCH payload-extraction path (not the slug).
    let hits = db
        .query("SELECT b.customer AS c, b.qty AS q, b.status AS s FROM MATCH (a:orders)-[:next]->(b:orders) WHERE a._key = 'o0005'")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["c"], "cust-06", "MATCH must project real dest customer");
    assert_eq!(p["q"], 6);
    assert_eq!(p["s"], "shipped");
}

#[test]
fn skbin_match_where_on_dest_field() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    // A hop condition on a destination payload field must evaluate correctly.
    let hits = db
        .query("SELECT a._key AS ak FROM MATCH (a:orders)-[:next]->(b:orders) WHERE b.status = 'shipped'")
        .unwrap()
        .collect();
    // b is shipped when its index is even; a→b for a=o0001,o0003,... (odd a → even b)
    assert!(!hits.is_empty(), "dest-field hop predicate must match real values");
}

#[test]
fn skbin_filters_all_types() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    assert_eq!(db.query("SELECT * FROM orders WHERE qty >= 5").unwrap().collect().len(), 100);
    assert_eq!(db.query("SELECT * FROM orders WHERE active = true").unwrap().collect().len(), 100);
    assert_eq!(db.query("SELECT * FROM orders WHERE status = 'shipped'").unwrap().collect().len(), 100);
    assert_eq!(db.query("SELECT * FROM orders WHERE customer = 'cust-05'").unwrap().collect().len(), 10);
}

#[test]
fn skbin_update_delete_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let mut db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    // UPDATE a SKBIN-stored record, then read the new value back. (SQL numeric
    // literals are stored as f64 — that's pre-existing behaviour, not SKBIN.)
    db.execute("UPDATE orders SET qty = 999 WHERE _key = 'o0010'").unwrap();
    let v: Value = serde_json::from_str(&db.get("orders/o0010").unwrap()).unwrap();
    assert_eq!(v["qty"].as_f64(), Some(999.0));
    // DELETE and confirm gone.
    db.execute("DELETE FROM orders WHERE _key = 'o0011'").unwrap();
    assert!(db.get("orders/o0011").is_none());
    // Recompact (mixed SKBIN + fresh raw writes) and re-verify the update survives.
    db.compact().unwrap();
    let v: Value = serde_json::from_str(&db.get("orders/o0010").unwrap()).unwrap();
    assert_eq!(v["qty"].as_f64(), Some(999.0), "update must survive recompaction");
}

#[test]
fn trim_memory_is_safe_and_preserves_data() {
    // trim_memory() reclaims excess map/index capacity on demand. It must NEVER
    // drop data or indexes: every query result and stored value stays identical,
    // and the DB stays fully writable afterwards.
    let dir = tempfile::tempdir().unwrap();
    let mut db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    // Churn: insert 5000, delete 4000 — leaves the internal maps/Vecs heavily
    // over-allocated (large capacity, small len), which is exactly what trim reclaims.
    for i in 0..5000u32 {
        db.put(
            &format!("t/k{i:05}"),
            &format!(r#"{{"_collection":"t","_key":"k{i:05}","v":{i}}}"#),
        )
        .unwrap();
    }
    for i in 0..4000u32 {
        db.execute(&format!("DELETE FROM t WHERE _key = 'k{i:05}'")).unwrap();
    }
    let count = |db: &CoreDB| -> i64 {
        let hits = db.query("SELECT COUNT(*) AS n FROM t").unwrap().collect();
        hits[0].payload.as_ref().unwrap()["n"].as_i64().unwrap()
    };
    assert_eq!(count(&db), 1000, "1000 rows survive the churn");

    db.trim_memory(); // must not panic, must not change anything

    assert_eq!(count(&db), 1000, "trim_memory must not lose data");
    // A specific surviving row still reads back its exact value.
    let v: Value = serde_json::from_str(&db.get("t/k04500").unwrap()).unwrap();
    assert_eq!(v["v"].as_i64(), Some(4500));
    // A filtered query still returns the right rows post-trim.
    assert_eq!(
        db.query("SELECT _key FROM t WHERE v = 4500").unwrap().collect().len(),
        1,
        "field index still works after trim"
    );
    // Still fully writable after trimming.
    db.put(r#"t/knew"#, r#"{"_collection":"t","_key":"knew","v":42}"#).unwrap();
    assert_eq!(count(&db), 1001, "DB remains writable after trim");
    // compact() now auto-trims at the end — must also stay correct.
    db.compact().unwrap();
    assert_eq!(count(&db), 1001, "compact (with auto-trim) preserves data");
}

#[test]
fn skbin_alter_table_add_column() {
    // ALTER needs a declared schema, so build this collection via CREATE TABLE.
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
        db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, qty INTEGER, note TEXT)").unwrap();
        for i in 0..50 {
            db.execute(&format!(
                "INSERT INTO items (_key, qty, note) VALUES ('i{i:04}', {}, 'note for item {i}')",
                i % 10
            ))
            .unwrap();
        }
        db.compact().unwrap(); // items now SKBIN
    }
    let mut db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    // DDL that scans + rewrites SKBIN payloads.
    db.execute("ALTER TABLE items ADD COLUMN priority TEXT").unwrap();
    let hits = db.query("SELECT _key, qty FROM items WHERE _key = 'i0007'").unwrap().collect();
    assert_eq!(hits[0].payload.as_ref().unwrap()["qty"].as_f64(), Some(7.0), "reads intact after ALTER over SKBIN");
}

#[test]
fn skbin_match_medium_records_project_dest_fields() {
    // Records > 512 B so the MATCH tail-slice fast path is taken: a tail slice has
    // no SKBIN header, so this exercises the try_skbin_node_fields dispatch.
    let dir = tempfile::tempdir().unwrap();
    let pad = "x".repeat(900);
    {
        let mut db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
        for i in 0..60 {
            db.put(
                &format!("n/n{i:03}"),
                &format!(r#"{{"_collection":"n","_key":"n{i:03}","label":"node-{i}","pad":"{pad}"}}"#),
            )
            .unwrap();
        }
        for i in 0..59 {
            db.link(&format!("n/n{i:03}"), &format!("n/n{:03}", i + 1), "next");
        }
        db.compact().unwrap();
    }
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    let hits = db
        .query("SELECT b.label AS l FROM MATCH (a:n)-[:next]->(b:n) WHERE a._key = 'n005'")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap()["l"], "node-6", "MATCH over medium SKBIN records");
}

#[test]
fn skbin_bm25_index_over_binary_payloads() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let mut db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    db.execute("CREATE INDEX ON orders USING bm25 (note)").unwrap();
    let hits = db.query("SELECT _key, BM25('note', 'warehouse facility') AS s FROM orders ORDER BY s DESC LIMIT 5").unwrap().collect();
    assert_eq!(hits.len(), 5, "BM25 over SKBIN payloads must rank real matches");
}

#[test]
fn skbin_paged_topology_reads_real_values() {
    // Paged topology serves payloads/nodes from the mmap base (self.nodes empty).
    // Every read must resolve payload offsets via the base AND decode SKBIN.
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let cfg = Config { payload_binary: true, paged_topology: true, ..Config::default() };
    let db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
    assert!(db.query("SELECT COUNT(*) AS n FROM orders").unwrap().collect()[0]
        .payload.as_ref().unwrap()["n"].as_i64().unwrap() >= 1, "paged base must enumerate collection");

    // Filter + projection real values.
    let hits = db.query("SELECT _key, customer, qty FROM orders WHERE qty = 5").unwrap().collect();
    assert!(hits.len() >= 2, "paged scan must return rows");
    for h in &hits {
        let p = h.payload.as_ref().unwrap();
        assert!(p["customer"].is_string(), "paged+SKBIN must return real values, not empty");
        assert_eq!(p["qty"], 5);
    }
    // ORDER BY over paged SKBIN.
    let sorted = db.query("SELECT _key, qty FROM orders ORDER BY qty DESC, _key ASC LIMIT 3").unwrap().collect();
    assert_eq!(sorted[0].payload.as_ref().unwrap()["qty"], 9, "paged ORDER BY must see real qty");
    // MATCH over paged SKBIN, projecting a dest payload field.
    let m = db.query("SELECT b.customer AS c FROM MATCH (a:orders)-[:next]->(b:orders) WHERE a._key = 'o0005'").unwrap().collect();
    assert_eq!(m[0].payload.as_ref().unwrap()["c"], "cust-06", "paged MATCH must project real dest field");
}

#[test]
fn skbin_gin_index_over_binary_payloads() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let mut db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    // Building a text index must scan SKBIN payloads correctly.
    db.execute("CREATE INDEX ON orders USING gin (note)").unwrap();
    let n = db.query("SELECT * FROM orders WHERE note LIKE '%warehouse%'").unwrap().collect().len();
    assert_eq!(n, 200, "GIN over SKBIN payloads must find the indexed term in every row");
}

// ===== Full query-surface coverage over SKBIN (post-compact) ==================
// seed(): 200 orders, qty=i%10, customer=cust-(i%20), amount=i*3, status even→
// shipped, active=(i even); linked as a chain o0000→o0001→…→o0199 via 'next'.

#[test]
fn skbin_all_comparison_and_range_filters() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    let cnt = |sql: &str| db.query(sql).unwrap().collect().len();
    assert_eq!(cnt("SELECT * FROM orders WHERE qty > 5"), 80);       // 6,7,8,9
    assert_eq!(cnt("SELECT * FROM orders WHERE qty < 3"), 60);       // 0,1,2
    assert_eq!(cnt("SELECT * FROM orders WHERE qty >= 5"), 100);     // 5..9
    assert_eq!(cnt("SELECT * FROM orders WHERE qty <= 2"), 60);      // 0,1,2
    assert_eq!(cnt("SELECT * FROM orders WHERE qty != 5"), 180);
    assert_eq!(cnt("SELECT * FROM orders WHERE qty BETWEEN 3 AND 6"), 80); // 3,4,5,6
    assert_eq!(cnt("SELECT * FROM orders WHERE qty IN (1, 2, 3)"), 60);
    assert_eq!(cnt("SELECT * FROM orders WHERE customer NOT IN ('cust-00')"), 190);
    assert_eq!(cnt("SELECT * FROM orders WHERE note LIKE '%warehouse%'"), 200); // scan, no index
}

#[test]
fn skbin_aggregations_and_having() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    let hits = db.query("SELECT COUNT(*) AS n, SUM(amount) AS s, AVG(qty) AS a, MIN(qty) AS mn, MAX(qty) AS mx FROM orders").unwrap().collect();
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["n"].as_i64(), Some(200));
    assert_eq!(p["s"].as_f64(), Some(59700.0)); // 3*(0+..+199)
    assert_eq!(p["a"].as_f64(), Some(4.5));      // mean of 0..9
    assert_eq!(p["mn"].as_f64(), Some(0.0));
    assert_eq!(p["mx"].as_f64(), Some(9.0));
    let g = db.query("SELECT status, COUNT(*) AS n FROM orders GROUP BY status HAVING COUNT(*) > 50").unwrap().collect();
    assert_eq!(g.len(), 2, "both status groups have 100 > 50");
}

#[test]
fn skbin_distinct_case_pagination() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    assert_eq!(db.query("SELECT DISTINCT customer FROM orders").unwrap().collect().len(), 20);
    let c = db.query("SELECT _key, CASE WHEN status = 'shipped' THEN 'done' ELSE 'wip' END AS kind FROM orders WHERE _key = 'o0002'").unwrap().collect();
    assert_eq!(c[0].payload.as_ref().unwrap()["kind"], "done"); // o0002 even → shipped
    let page = db.query("SELECT _key FROM orders ORDER BY _key ASC LIMIT 5 OFFSET 10").unwrap().collect();
    assert_eq!(page.len(), 5);
    assert_eq!(page[0].payload.as_ref().unwrap()["_key"], "o0010");
}

#[test]
fn bulk_value_insert_correct_and_update_preserves_created() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    // Bulk insert (the IoT fast path) — identity/timestamps must be filled in.
    let rows: Vec<(String, Value)> = (0..100)
        .map(|i| (format!("sensors/s{i}"), serde_json::json!({"_collection":"sensors","_key":format!("s{i}"),"v": i})))
        .collect();
    assert_eq!(db.put_value_bulk(rows).unwrap(), 100);

    let hits = db.query("SELECT _key, _id, v FROM sensors WHERE _key = 's5'").unwrap().collect();
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["_key"], "s5");
    assert_eq!(p["_id"], "sensors/s5");
    assert_eq!(p["v"], 5);
    let full: Value = serde_json::from_str(&db.get("sensors/s5").unwrap()).unwrap();
    let created = full["_created_unix"].as_i64().expect("_created_unix set");
    assert!(full["_updated_unix"].is_i64(), "_updated_unix set");
    let count = |db: &CoreDB| db.query("SELECT COUNT(*) AS n FROM sensors").unwrap().collect()[0]
        .payload.as_ref().unwrap()["n"].as_i64().unwrap();
    assert_eq!(count(&db), 100);

    // Update via bulk: value changes, _created preserved, no duplicate membership.
    std::thread::sleep(std::time::Duration::from_millis(3));
    db.put_value_bulk(vec![(format!("sensors/s5"), serde_json::json!({"_collection":"sensors","_key":"s5","v":999}))]).unwrap();
    let full2: Value = serde_json::from_str(&db.get("sensors/s5").unwrap()).unwrap();
    assert_eq!(full2["v"], 999);
    assert_eq!(full2["_created_unix"].as_i64().unwrap(), created, "update must preserve _created_unix");
    assert_eq!(count(&db), 100, "update must not add a duplicate collection member");

    // Survives compaction + reopen.
    db.compact().unwrap();
    drop(db); // release the file lock before reopening
    let db2 = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    assert_eq!(count(&db2), 100);
    let v: Value = serde_json::from_str(&db2.get("sensors/s5").unwrap()).unwrap();
    assert_eq!(v["v"], 999);
}

#[test]
fn skbin_plain_select_union_errors_not_silent() {
    // Plain SELECT UNION is unsupported and must ERROR (not silently drop the
    // second branch and return a partial result).
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    let r = db.query("SELECT _key FROM orders WHERE qty = 1 UNION SELECT _key FROM orders WHERE qty = 2");
    assert!(r.is_err(), "plain SELECT UNION must be rejected, not silently return one branch");
}

#[test]
fn skbin_match_union_matches_raw() {
    // Supported UNION (between MATCH patterns) must return identical results on
    // raw and SKBIN — set algebra is payload-format-independent.
    let sql = "SELECT b._key AS k FROM MATCH (a:orders)-[:next]->(b:orders) WHERE a._key = 'o0000' \
               UNION \
               SELECT b._key AS k FROM MATCH (a:orders)-[:next]->(b:orders) WHERE a._key = 'o0001'";
    let run = |binary: bool| -> usize {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config { payload_binary: binary, ..Config::default() };
        seed_cfg(dir.path(), cfg.clone());
        CoreDB::open_with_config(dir.path(), cfg).unwrap().query(sql).unwrap().collect().len()
    };
    let (raw, bin) = (run(false), run(true));
    assert_eq!(raw, bin, "MATCH UNION must match raw vs SKBIN");
    assert_eq!(bin, 2, "o0000→o0001 UNION o0001→o0002 = {{o0001, o0002}}");
}

#[test]
fn skbin_graph_backward_and_bfs() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    let b = db.query("SELECT b._key AS k FROM MATCH (a:orders)<-[:next]-(b:orders) WHERE a._key = 'o0005'").unwrap().collect();
    assert_eq!(b[0].payload.as_ref().unwrap()["k"], "o0004", "backward hop");
    let bfs = db.query("SELECT DISTINCT x._key AS k FROM MATCH (a:orders)-[:next*1..3]->(x:orders) WHERE a._key = 'o0000'").unwrap().collect();
    assert_eq!(bfs.len(), 3, "BFS 1..3 hops reaches o0001,o0002,o0003");
}

#[test]
fn skbin_shortest_path() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    let s = db.query("SELECT length(r) AS hops FROM MATCH SHORTEST (a:orders)-[r*]->(b:orders) WHERE a._key = 'o0000' AND b._key = 'o0005'").unwrap().collect();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].payload.as_ref().unwrap()["hops"].as_i64(), Some(5));
}

#[test]
fn skbin_transactions_commit_and_rollback() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let mut db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE orders SET qty = 777 WHERE _key = 'o0001'").unwrap();
    db.execute("COMMIT").unwrap();
    let v: Value = serde_json::from_str(&db.get("orders/o0001").unwrap()).unwrap();
    assert_eq!(v["qty"].as_f64(), Some(777.0), "commit persists over SKBIN");
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE orders SET qty = 888 WHERE _key = 'o0001'").unwrap();
    db.execute("ROLLBACK").unwrap();
    let v2: Value = serde_json::from_str(&db.get("orders/o0001").unwrap()).unwrap();
    assert_eq!(v2["qty"].as_f64(), Some(777.0), "rollback reverts over SKBIN");
}

/// Geometry + vectors + indexes, compacted to SKBIN.
fn seed_geo(dir: &std::path::Path) {
    let mut db = CoreDB::open_with_config(dir, skbin_cfg()).unwrap();
    for i in 0..500 {
        let slug = format!("places/p{i:04}");
        db.put(&slug, &format!(
            r#"{{"_collection":"places","_key":"p{i:04}","name":"place {i}","geometry":{{"type":"Point","coordinates":[{}, {}]}}}}"#,
            144.96 + (i % 50) as f64 * 0.001, -37.81 + (i % 50) as f64 * 0.001
        )).unwrap();
        db.put_vector(&slug, "emb", &[(i % 10) as f32 / 10.0, 1.0 - (i % 10) as f32 / 10.0]).unwrap();
    }
    db.build_spatial_index();
    db.build_hnsw_index("emb", 16, 200).unwrap();
    db.compact().unwrap();
}

#[test]
fn skbin_spatial_projects_real_fields() {
    let dir = tempfile::tempdir().unwrap();
    seed_geo(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    let hits = db.query("SELECT _key, name FROM places WHERE ST_DWithin(geometry, POINT(144.96 -37.81), 1000.0)").unwrap().collect();
    assert!(!hits.is_empty(), "spatial filter must match over SKBIN geometry");
    assert!(hits[0].payload.as_ref().unwrap()["name"].is_string(), "spatial result projects real name");
}

#[test]
fn skbin_vector_near_projects_real_fields() {
    let dir = tempfile::tempdir().unwrap();
    seed_geo(dir.path());
    let db = CoreDB::open_with_config(dir.path(), skbin_cfg()).unwrap();
    let hits = db.query("SELECT _key, name FROM places WHERE VECTOR_NEAR(emb, [1.0, 0.0], 10)").unwrap().collect();
    assert_eq!(hits.len(), 10, "vector top-10 over SKBIN");
    assert!(hits[0].payload.as_ref().unwrap()["name"].is_string(), "vector result projects real name");
}
