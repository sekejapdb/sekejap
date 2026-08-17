//! Verify eight reported query-layer bugs against the actual engine.
use sekejap::CoreDB;
use serde_json::{json, Value};

fn db_with(rows: Vec<Value>, index_on: Option<&str>, cols: &str) -> (CoreDB, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute(&format!("CREATE TABLE t (_key TEXT PRIMARY KEY, {cols})")).unwrap();
    for (i, r) in rows.iter().enumerate() {
        let mut v = r.clone();
        v["_collection"] = json!("t");
        v["_key"] = json!(format!("k{i}"));
        db.put(&format!("t/k{i}"), &v.to_string()).unwrap();
    }
    if let Some(f) = index_on {
        db.execute(&format!("CREATE INDEX ON t USING btree ({f})")).unwrap();
    }
    (db, dir)
}
fn n(db: &CoreDB, sql: &str) -> String {
    match db.query(sql) { Ok(s) => {
        let h = s.collect();
        if h.len() == 1 && h[0].payload.is_some() && h[0].slug.is_empty() {
            return format!("{}", h[0].payload.as_ref().unwrap());
        }
        format!("{} rows", h.len())
    }, Err(e) => format!("ERR {e}") }
}
fn keys(db: &CoreDB, sql: &str) -> String {
    match db.query(sql) { Ok(s) => { let mut v: Vec<String> =
        s.collect().iter().map(|h| h.slug.clone()).collect(); v.sort(); v.join(",") },
      Err(e) => format!("ERR {e}") }
}

fn main() {
    println!("\n=== 2. COUNT(col) ===");
    let (db, _d) = db_with(vec![json!({"name":"amy"}), json!({"name":"bob"}), json!({"name":"carl"})],
                           None, "name TEXT");
    println!("  COUNT(name) expect 3  -> {}", n(&db, "SELECT COUNT(name) AS c FROM t"));

    println!("=== 3. ORDER BY indexed LIMIT/OFFSET ===");
    let rows: Vec<Value> = (1..=20).map(|v| json!({"v": v})).collect();
    let (db, _d) = db_with(rows.clone(), Some("v"), "v INTEGER");
    println!("  indexed  LIMIT 5 OFFSET 3 expect 5 -> {}", n(&db, "SELECT _key FROM t ORDER BY v ASC LIMIT 5 OFFSET 3"));
    let (db2, _d2) = db_with(rows, None, "v INTEGER");
    println!("  no index LIMIT 5 OFFSET 3 expect 5 -> {}", n(&db2, "SELECT _key FROM t ORDER BY v ASC LIMIT 5 OFFSET 3"));

    println!("=== 4. index vs scan, mixed types ===");
    let mixed = vec![json!({"age":30}), json!({"age":"25"})];
    let (db, _d) = db_with(mixed.clone(), Some("age"), "age INTEGER");
    println!("  with index    age>20 expect 1 -> {}", keys(&db, "SELECT _key FROM t WHERE age > 20"));
    let (db2, _d2) = db_with(mixed, None, "age INTEGER");
    println!("  without index age>20 expect 1 -> {}", keys(&db2, "SELECT _key FROM t WHERE age > 20"));

    println!("=== 5. DISTINCT + LIMIT ===");
    let (db, _d) = db_with(vec![json!({"x":"a"}), json!({"x":"a"}), json!({"x":"b"}), json!({"x":"c"})],
                           None, "x TEXT");
    println!("  DISTINCT x ORDER BY x LIMIT 2 expect 2 -> {}", n(&db, "SELECT DISTINCT x FROM t ORDER BY x ASC LIMIT 2"));

    println!("=== 6. != and NOT IN with missing field ===");
    let (db, _d) = db_with(vec![json!({"v":5}), json!({"name":"no-v"})], None, "v INTEGER, name TEXT");
    println!("  v != 5      expect 0 rows -> {}", keys(&db, "SELECT _key FROM t WHERE v != 5"));
    println!("  v NOT IN(1) expect 0 rows -> {}", keys(&db, "SELECT _key FROM t WHERE v NOT IN (1)"));

    println!("=== 7. SUM over empty / all-null ===");
    let (db, _d) = db_with(vec![], None, "x INTEGER");
    println!("  empty  SUM/AVG/MIN -> {}", n(&db, "SELECT SUM(x) AS s, AVG(x) AS a, MIN(x) AS m FROM t"));

    println!("=== 8. GROUP BY index vs scan ===");
    let g = vec![json!({"g":1}), json!({"g":1.0}), json!({"other":true})];
    let (db, _d) = db_with(g.clone(), Some("g"), "g INTEGER");
    println!("  with index    -> {}", n(&db, "SELECT g, COUNT(*) AS c FROM t GROUP BY g"));
    let (db2, _d2) = db_with(g, None, "g INTEGER");
    println!("  without index -> {}", n(&db2, "SELECT g, COUNT(*) AS c FROM t GROUP BY g"));
}
