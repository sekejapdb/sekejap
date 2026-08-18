//! A whole database running on paged payloads.
//!
//! `Config::paged_payloads` puts record bytes in slotted pages with a free list, so
//! an updated or deleted row returns its space immediately. That is the write path
//! SQLite, LMDB and DuckDB use, and the reason none of them has a compaction step.
//!
//! Two things have to be true before it can become the default: it must answer
//! exactly what the append-only store answers, and space must genuinely come back.

use sekejap::{Config, CoreDB};
use serde_json::json;

fn paged() -> Config {
    Config { paged_payloads: true, ..Config::resident() }
}

/// One field of a stored record, so comparisons ignore the wall-clock timestamps
/// a payload carries.
fn field(db: &CoreDB, slug: &str, key: &str) -> String {
    db.get(slug)
        .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
        .and_then(|v| v.get(key).and_then(|f| f.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| "<absent>".into())
}

fn row(i: usize, tag: &str) -> String {
    json!({
        "_collection": "p", "_key": format!("n{i}"),
        "name": format!("{tag} {i}"),
        "body": format!("the quick brown fox number {i} leaps the lazy riverbank"),
        "n": i as i64,
    }).to_string()
}

/// Everything a caller can observe must match the append-only store exactly. If
/// these ever diverge, the paged path is quietly returning different data.
#[test]
fn a_paged_database_answers_the_same_as_a_flat_one() {
    let work = |dir: &std::path::Path, cfg: Config| -> Vec<String> {
        let mut db = CoreDB::open_with_config(dir, cfg).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT, body TEXT, n INTEGER)").unwrap();
        for i in 0..300 { db.put(&format!("p/n{i}"), &row(i, "original")).unwrap(); }
        db.execute("CREATE INDEX ON p USING btree (n)").unwrap();
        db.execute("CREATE INDEX ON p USING bm25 (body)").unwrap();
        for i in 0..50 { db.put(&format!("p/n{i}"), &row(i, "rewritten")).unwrap(); }
        db.execute("DELETE FROM p WHERE n >= 250").unwrap();
        for i in 0..40 { db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next"); }

        let q = |db: &CoreDB, sql: &str| -> String {
            let mut k: Vec<String> = db.query(sql).unwrap().collect()
                .iter().map(|h| h.slug.clone()).collect();
            k.sort();
            k.join(",")
        };
        let mut out = vec![
            db.node_count().to_string(),
            q(&db, "SELECT _key FROM p"),
            q(&db, "SELECT _key FROM p WHERE n > 100 AND n < 120"),
            q(&db, "SELECT _key FROM p ORDER BY n DESC LIMIT 5"),
            q(&db, "SELECT _key FROM p WHERE body ILIKE '%fox%'"),
            q(&db, "SELECT _key FROM p WHERE BM25(body,'riverbank') > 0"),
            q(&db, "SELECT b._key FROM MATCH (a:p)-[:next]->(b:p)"),
            // The stored name, not the whole record: a payload carries
            // _created_unix and _updated_unix, which are wall-clock and so differ
            // between two runs for reasons that have nothing to do with storage.
            field(&db, "p/n7", "name"),
            field(&db, "p/n270", "name"),
        ];
        // And again after a compaction and a reopen.
        db.compact().unwrap();
        out.push(q(&db, "SELECT _key FROM p"));
        out.push(field(&db, "p/n7", "name"));
        out
    };

    let d1 = tempfile::TempDir::new().unwrap();
    let d2 = tempfile::TempDir::new().unwrap();
    let flat = work(d1.path(), Config::resident());
    let pagd = work(d2.path(), paged());

    for (i, (f, p)) in flat.iter().zip(&pagd).enumerate() {
        assert_eq!(f, p, "paged and flat disagree on observation {i}");
    }
    assert!(!flat[1].is_empty(), "the baseline produced nothing to compare");
}

/// The property the whole direction exists for, end to end: a workload that
/// deletes as fast as it inserts must stop growing the payload file.
///
/// The append-only store cannot do this — its file grows with every write forever,
/// and only a full rewrite reclaims anything. That difference is the six-minute
/// stall on a 48-million-record store.
#[test]
fn a_rolling_workload_stops_growing_the_payload_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT, body TEXT, n INTEGER)").unwrap();

    let size = || std::fs::metadata(dir.path().join("payloads.bin")).map(|m| m.len()).unwrap_or(0);

    let window = 400usize;
    for i in 0..window { db.put(&format!("p/n{i}"), &row(i, "seed")).unwrap(); }
    let settled = size();
    assert!(settled > 0, "nothing was written");

    // Replace the window many times over. Without reclamation the file would grow
    // by roughly its own size on every pass.
    for pass in 1..=6 {
        for i in 0..window {
            let old = (pass - 1) * window + i;
            let new = pass * window + i;
            db.put(&format!("p/n{new}"), &row(new, "rolling")).unwrap();
            db.remove(&format!("p/n{old}"));
        }
    }
    let after = size();
    assert!(after <= settled * 2,
            "payload file grew from {settled} to {after} bytes over six full \
             replacements — space is not coming back");

    // And the data is still right.
    let live = db.query("SELECT _key FROM p").unwrap().collect().len();
    assert_eq!(live, window, "expected {window} live rows, found {live}");
    assert!(db.get(&format!("p/n{}", 6 * window)).is_some(), "a live row is missing");
    assert!(db.get("p/n0").is_none(), "an expired row came back");
}

#[test]
fn paged_payloads_survive_a_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT, body TEXT, n INTEGER)").unwrap();
        for i in 0..200 { db.put(&format!("p/n{i}"), &row(i, "kept")).unwrap(); }
        db.remove("p/n5");
        db.compact().unwrap();
    }
    let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
    assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), 199);
    assert!(db.get("p/n7").unwrap().contains("kept"), "a record did not survive");
    assert!(db.get("p/n5").is_none(), "a deleted record came back");
}

/// Large records span several pages; they must round-trip through a real database.
#[test]
fn oversized_payloads_round_trip_in_a_paged_database() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, blob TEXT)").unwrap();

    for (i, len) in [1_000usize, 4_096, 40_000, 400_000].iter().enumerate() {
        let blob = "x".repeat(*len);
        db.put(&format!("p/big{i}"),
               &json!({"_collection":"p","_key":format!("big{i}"),"blob":blob}).to_string()).unwrap();
    }
    for (i, len) in [1_000usize, 4_096, 40_000, 400_000].iter().enumerate() {
        let got = db.get(&format!("p/big{i}")).expect("a large record vanished");
        assert!(got.contains(&"x".repeat(*len)), "a {len}-byte payload came back wrong");
    }
}

// ── reaching a paged store through the service API ──────────────────────────
//
// `Engine` is how a long-running process opens the database, and until now its
// builder could ask for exactly two layouts: plain, or `paged_topology` alone.
// Every other paged store was unreachable — and worse than unreachable, because
// opening one without its flags served an empty database and then overwrote it.

/// A store written with paged payloads must open through the service API and
/// still hold its rows.
#[test]
fn a_paged_store_opens_as_a_service() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
        for i in 0..50 {
            db.put(&format!("p/n{i}"),
                   &json!({"_collection":"p","_key":format!("n{i}"),"name":"kept"}).to_string())
              .unwrap();
        }
    }

    let eng = sekejap::engine::Engine::builder(dir.path().to_str().unwrap())
        .config(paged())
        .build()
        .expect("a paged store was refused by the service API");
    assert_eq!(eng.query("SELECT _key FROM p").unwrap().len(), 50,
               "the service saw a different database than the one on disk");
    assert!(eng.get("p/n7").is_some(), "a row vanished behind the service API");
}

/// And it must open through the *plain* service entry point too, which names no
/// config at all. The files say what they are; the caller does not have to know.
#[test]
fn open_as_service_adopts_the_layout_already_on_disk() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
        for i in 0..50 {
            db.put(&format!("p/n{i}"),
                   &json!({"_collection":"p","_key":format!("n{i}"),"name":"kept"}).to_string())
              .unwrap();
        }
    }

    {
        let eng = sekejap::open_as_service(dir.path().to_str().unwrap())
            .expect("open_as_service refused a paged store");
        assert_eq!(eng.query("SELECT _key FROM p").unwrap().len(), 50,
                   "open_as_service served an empty database over a paged store");
        eng.execute("INSERT INTO p (_key, name) VALUES ('later', 'written')").unwrap();
    }

    // The write went to the paged store, not to a flat file that replaced it.
    let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
    assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), 51,
               "the service wrote somewhere the paged store cannot see");
    assert!(db.get("p/n7").is_some(), "the service overwrote what was already there");
}

/// The reverse direction: a flat store must not be re-read as paged because the
/// caller asked for paged. Same rule, other way round — the bytes decide.
#[test]
fn a_flat_store_is_not_reopened_as_paged_on_request() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
        for i in 0..20 {
            db.put(&format!("p/n{i}"),
                   &json!({"_collection":"p","_key":format!("n{i}"),"name":"kept"}).to_string())
              .unwrap();
        }
    }
    let db = CoreDB::open_with_config(dir.path(), paged())
        .expect("asking for paged over a flat store was refused instead of corrected");
    assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), 20,
               "a flat store lost its rows when opened with paged_payloads");
}

/// The full paged layout — payloads, nodes, adjacency, topology — through the
/// plain service entry point, graph included. If snapshots are unavailable in
/// this shape the engine must take the lock and answer correctly, not answer
/// less.
#[test]
fn the_full_paged_layout_serves_graph_queries_as_a_service() {
    let full = || Config {
        paged_payloads: true, paged_nodes: true,
        paged_adjacency: true, paged_topology: true,
        ..Default::default()
    };
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), full()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
        for i in 0..30 {
            db.put(&format!("p/n{i}"),
                   &json!({"_collection":"p","_key":format!("n{i}"),"name":"kept"}).to_string())
              .unwrap();
        }
        for i in 0..29 { db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next"); }
    }

    let eng = sekejap::open_as_service(dir.path().to_str().unwrap())
        .expect("open_as_service refused a fully paged store");
    assert_eq!(eng.query("SELECT _key FROM p").unwrap().len(), 30,
               "the service saw fewer rows than the store holds");
    let hops = eng.query("SELECT b._key FROM MATCH (a:p)-[:next]->(b:p) WHERE a._key = 'n0'")
        .expect("a graph query failed behind the service API");
    assert_eq!(hops.len(), 1, "the paged adjacency was invisible to the service");
}

/// **`snapshot.json` is metadata, not data.**
///
/// It records schemas and index parameters. The rows live in the files written
/// beside it — that is what a v3 *manifest* snapshot means, and it is the reason
/// opening a database does not cost more as the database grows.
///
/// The default layout broke this without anyone noticing, because the switch is
/// `PayloadStore::is_disk()` and a paged store answered "not on disk" about files
/// it had just written. It therefore took the branch meant for in-memory
/// databases, where the JSON *is* the only durable copy, and embedded every node
/// with its whole payload: 39.8 MB for 200 000 rows, larger than the payload file
/// it duplicated, and 81% of the time an open took.
///
/// So the property is asserted directly rather than inferred from a timing: the
/// snapshot must not grow with the row count. Ten times the rows, the same
/// handful of bytes.
#[test]
fn the_snapshot_does_not_grow_with_the_store() {
    let size_at = |rows: usize| -> u64 {
        let dir = tempfile::TempDir::new().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
            for i in 0..rows {
                db.put(&format!("p/n{i}"), &json!({
                    "_collection": "p", "_key": format!("n{i}"), "n": i as i64,
                    "body": format!("record {i} on the lazy riverbank"),
                }).to_string()).unwrap();
            }
            db.compact().unwrap();
        }
        let bytes = std::fs::metadata(dir.path().join("snapshot.json"))
            .map(|m| m.len())
            .unwrap_or(0);
        // The rows must still be there — a snapshot can always be made small by
        // losing them, and that is the failure this guards against becoming.
        let db = CoreDB::open(dir.path()).unwrap();
        assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), rows,
                   "the store lost rows");
        bytes
    };

    let small = size_at(200);
    let large = size_at(2_000);
    assert!(large < small + 4096,
            "snapshot.json grew from {small} to {large} bytes for 10x the rows — it is \
             carrying row data again, which makes every open cost more as the database \
             grows");
}

/// **A flipped byte inside the right record is refused, not served.**
///
/// Every other record type in a paged store was already protected. Node records
/// carry a CRC and their slug; adjacency records carry a CRC and their owner. The
/// payload record — the part holding the user's data — carried a four-byte owner
/// tag and nothing else.
///
/// The tag answers "which row is this", so a read that lands on a *different*
/// record is refused. It says nothing about whether the bytes are the bytes that
/// were written, so a flipped bit inside the correct record passed every check the
/// store had and came back as truth: the row returned a wrong value and nothing
/// anywhere raised. The fuzzer says so in as many words — "records carry no
/// checksum, so the store cannot know" — which was the store admitting the gap
/// rather than closing it.
///
/// A store written by an earlier version has no checksums and keeps behaving
/// exactly as it did; the flag lives in the page store's header and is set when a
/// store is created, so there is nothing to migrate and nothing that can break.
#[test]
fn a_damaged_payload_is_refused_rather_than_returned() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("payloads.bin");
    {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
        for i in 0..40 {
            db.put(&format!("p/n{i}"), &json!({
                "_collection": "p", "_key": format!("n{i}"), "n": i as i64,
                "body": format!("record {i} on the lazy riverbank"),
            }).to_string()).unwrap();
        }
        // Compacted, so `payloads.bin` is the authority and the log is empty.
        // Without this the reopen replays the log and rebuilds the payloads from
        // it, and damage to the file is simply not read — the test would pass
        // while proving nothing about the checksum.
        db.compact().unwrap();
    }

    // Flip a byte that is provably *inside a record*, not in the page header, the
    // slot directory or free space — the case identity alone cannot detect. Found
    // by searching for a row's own text rather than by picking an offset, because
    // a store this small is mostly slack and an arbitrary offset lands in it.
    let mut bytes = std::fs::read(&path).unwrap();
    let needle = b"record 7 on the lazy riverbank";
    let at = bytes.windows(needle.len()).position(|w| w == needle)
        .expect("row 7's text is not in the payload file");
    bytes[at + 7] ^= 0x40;      // inside the text, well past any header
    std::fs::write(&path, &bytes).unwrap();

    let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();

    // Whatever survives must be *correct*. A row either reads as its own stored
    // value or does not read at all; what it must never do is hand back a value
    // that was never written.
    let mut readable = 0usize;
    for i in 0..40 {
        let slug = format!("p/n{i}");
        let Some(raw) = db.get(&slug) else { continue };
        readable += 1;
        let v: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("{slug} came back as invalid JSON: {e}"));
        assert_eq!(v["n"].as_i64(), Some(i as i64),
                   "{slug} came back holding another row's value — a damaged record \
                    was served instead of refused");
        assert_eq!(v["body"].as_str(), Some(format!("record {i} on the lazy riverbank").as_str()),
                   "{slug} came back with damaged text presented as its own");
    }
    assert_eq!(readable, 39,
               "a single flipped byte inside one record left {readable} of 40 rows \
                readable — it should cost exactly the record it landed in, no more \
                and no less");
    assert!(db.get("p/n7").is_none(),
            "the row whose bytes were damaged still reads — its checksum did not \
             catch the flip, so a wrong value is being served as its own");
}
