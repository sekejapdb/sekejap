//! Does turning on `paged_payloads` for an existing flat database destroy it?
//!
//! It did. `payloads.bin` went from 23 216 bytes to 4 096 on the reopen, before a
//! single query ran, and every row in the database went with it.
//!
//! The mechanism was two correct-looking pieces meeting. Every paged store opens
//! with `open(path)? else create(path)?`, and `RecordStore::open` politely declines
//! a file it does not recognise — a flat payload file is not a page store, so it
//! returned `None`. `create` then opened the same path with `truncate(true)`.
//!
//! `PageStore::create` refuses now: a file that exists, is not empty, and does not
//! carry the page-store magic is somebody's data, and creating a store over it is
//! not something to do on the way to a reopen. This example is kept as the
//! demonstration, and asserts the refusal.
use sekejap::{Config, CoreDB};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::TempDir::new()?;
    {
        let mut db = CoreDB::open(dir.path())?;
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)")?;
        for i in 0..500 {
            db.put(&format!("p/n{i}"), &json!({"_collection":"p","_key":format!("n{i}"),"n":i}).to_string())?;
        }
        db.compact()?;
    }
    let before = std::fs::metadata(dir.path().join("payloads.bin"))?.len();
    println!("flat database built: payloads.bin is {before} bytes");

    let opened = CoreDB::open_with_config(dir.path(), Config {
        paged_payloads: true, ..Config::default() });
    let after = std::fs::metadata(dir.path().join("payloads.bin"))?.len();
    println!("reopened with paged_payloads = true: payloads.bin is {after} bytes");
    match &opened {
        Err(e) => println!("refused, as it must be:\n  {e}"),
        Ok(_) => println!("ACCEPTED — which means the truncation guard is gone"),
    }
    assert_eq!(after, before, "payloads.bin was modified by a reopen that failed");
    assert!(opened.is_err(),
            "turning on paged payloads for a flat database was allowed; it destroys it");

    // The database is untouched: it still opens the way it was written.
    let db = CoreDB::open(dir.path())?;
    let rows = db.query("SELECT _key FROM p")?.collect().len();
    println!("still readable the original way: {rows} rows, p/n7 = {:?}", db.get("p/n7"));
    assert_eq!(rows, 500, "the database did not survive the refused reopen");
    Ok(())
}
