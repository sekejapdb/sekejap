//! Does the GIN index itself contain the rows, or is ILIKE just scanning?
//!
//! `gin_ilike` goes straight to the index with no scan fallback, so it answers
//! the question a SQL `ILIKE` cannot: whether index maintenance actually ran.

use sekejap::CoreDB;

fn main() {
    let dir = std::env::temp_dir().join(format!("sk_ginchk_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = CoreDB::open(&dir).unwrap();
    db.execute("CREATE TABLE t (body TEXT)").unwrap();
    // ORDER=after creates the index once rows exist; the default creates it on an
    // empty table, which is the ordinary way a schema is set up.
    let after = std::env::var("ORDER").map(|v| v == "after").unwrap_or(false);
    if !after {
        db.execute("CREATE INDEX ON t USING gin (body)").unwrap();
    }

    let rows: Vec<(String, serde_json::Value)> = (0..300)
        .map(|i| (format!("t/n{i}"),
             serde_json::json!({"_collection":"t","_key":format!("n{i}"),
                                "body": format!("alpha{} vine gamma", i)})))
        .collect();
    db.put_value_bulk(rows).unwrap();
    if after {
        db.execute("CREATE INDEX ON t USING gin (body)").unwrap();
    }

    println!("after bulk insert, before compact:");
    println!("  index says   {} rows contain 'vine'", db.gin_ilike("body", "%vine%", None).len());
    println!("  SQL says     {} rows", db.query("SELECT _key FROM t WHERE body ILIKE '%vine%'").unwrap().collect().len());

    db.compact().unwrap();
    println!("after compact:");
    println!("  index says   {} rows", db.gin_ilike("body", "%vine%", None).len());
    println!("  SQL says     {} rows", db.query("SELECT _key FROM t WHERE body ILIKE '%vine%'").unwrap().collect().len());

    drop(db);
    let db = CoreDB::open(&dir).unwrap();
    println!("after reopen:");
    println!("  index says   {} rows", db.gin_ilike("body", "%vine%", None).len());
    println!("  SQL says     {} rows", db.query("SELECT _key FROM t WHERE body ILIKE '%vine%'").unwrap().collect().len());
    println!();
    println!("expected 300 everywhere. anything less means index maintenance did not run.");

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
