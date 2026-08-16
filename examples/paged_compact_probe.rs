// Does paged compaction preserve base nodes, with NO deletes involved?
use sekejap::CoreDB;
use serde_json::json;
fn main() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        for i in 0..5 { db.put(&format!("v/n{i}"), &json!({"_collection":"v","_key":format!("n{i}")}).to_string()).unwrap(); }
        db.compact().unwrap();
    }
    let mut db = CoreDB::open_paged(dir.path()).unwrap();
    println!("paged open        : count={}", db.query("SELECT _key FROM v").unwrap().collect().len());
    db.compact().unwrap();                       // compact with an EMPTY overlay
    println!("after compact     : count={}", db.query("SELECT _key FROM v").unwrap().collect().len());
    drop(db);
    let db = CoreDB::open_paged(dir.path()).unwrap();
    println!("reopen after that : count={}", db.query("SELECT _key FROM v").unwrap().collect().len());

    // And with an overlay write present:
    let dir2 = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir2.path()).unwrap();
        for i in 0..5 { db.put(&format!("v/n{i}"), &json!({"_collection":"v","_key":format!("n{i}")}).to_string()).unwrap(); }
        db.compact().unwrap();
    }
    let mut db = CoreDB::open_paged(dir2.path()).unwrap();
    db.put("v/extra", &json!({"_collection":"v","_key":"extra"}).to_string()).unwrap();
    println!("\npaged + 1 write   : count={}", db.query("SELECT _key FROM v").unwrap().collect().len());
    db.compact().unwrap();
    drop(db);
    let db = CoreDB::open_paged(dir2.path()).unwrap();
    println!("compact + reopen  : count={}", db.query("SELECT _key FROM v").unwrap().collect().len());
}
