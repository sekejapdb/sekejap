//! Crash and damage behaviour of the paged storage configuration.
//!
//! The paged stores are a different durability story from the append-only ones,
//! and none of the existing crash tests touched them — `crash_recovery.rs`,
//! `persistence.rs` and `stress.rs` do not mention a single paged flag. That gap
//! is what this file closes, because "is it safe to make paged the default" is a
//! question about torn writes and kills, not about throughput.
//!
//! # What is different about these stores
//!
//! A page store keeps its header — high water mark, free-list head — in page 0,
//! and writes it only on `sync`. That is deliberate: a page write per allocation
//! would defeat the point. The claimed consequence is that a crash loses *the free
//! list*, not data: pages freed since the last sync stay allocated and are never
//! reused, which leaks space but cannot corrupt, because a leaked page is one
//! nothing points at.
//!
//! That claim is exactly the kind that is true in the design and false in the
//! code. These tests try to falsify it.
//!
//! # The standard every case is held to
//!
//! Damage may cost data that was not yet durable. It may **never**:
//!
//! - panic, or read outside a buffer
//! - report success while serving less than it holds
//! - turn a row into a *different* row
//!
//! A store that cannot be read must say so. A store that can be read must be right.

use sekejap::{Config, CoreDB};
use serde_json::json;
use std::path::Path;

fn paged() -> Config {
    Config {
        paged_topology: true,
        paged_payloads: true,
        paged_adjacency: true,
        paged_nodes: true,
        ..Config::default()
    }
}

fn row(i: usize) -> String {
    json!({
        "_collection": "p",
        "_key": format!("n{i}"),
        "n": i as i64,
        "body": format!("record {i} on the lazy riverbank"),
    })
    .to_string()
}

/// Build a paged store with `n` rows and `n/2` edges, compacted.
fn build(dir: &Path, n: usize) {
    let mut db = CoreDB::open_with_config(dir, paged()).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
    for i in 0..n {
        db.put(&format!("p/n{i}"), &row(i)).unwrap();
    }
    for i in 0..n / 2 {
        db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next");
    }
    db.compact().unwrap();
}

fn rows(db: &CoreDB) -> usize {
    db.query("SELECT _key FROM p").unwrap().collect().len()
}

/// Every paged file, so a test can damage each in turn without naming them by hand
/// — a file added later is then covered automatically rather than silently missed.
fn paged_files(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| {
            n.starts_with("nodesp") || n.starts_with("adjp") || n == "payloads.bin"
        })
        .collect();
    v.sort();
    v
}

// ── the ordinary paths ───────────────────────────────────────────────────────

/// Writes that were never compacted must come back from the WAL.
#[test]
fn uncompacted_writes_replay_into_the_paged_stores() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 60);
    {
        // No compact, no graceful anything: this is a process death.
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        for i in 60..90 {
            db.put(&format!("p/n{i}"), &row(i)).unwrap();
        }
        db.remove("p/n5");
        db.link("p/n61", "p/n62", "next");
        drop(db);
    }
    let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
    assert_eq!(rows(&db), 89, "WAL replay lost writes that were never compacted");
    assert!(db.get("p/n80").is_some(), "a replayed row is missing");
    assert!(db.get("p/n5").is_none(), "a replayed delete came back");
    assert_eq!(db.one("p/n61").forward("next").collect().len(), 1,
               "a replayed edge is missing");
}

/// The same, but the writes were folded into the paged stores by a compaction and
/// the log truncated — so recovery reads them out of the pages, not the log.
#[test]
fn compacted_writes_survive_a_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 200);
    {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        for i in 200..260 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
        db.execute("DELETE FROM p WHERE n < 10").unwrap();
        db.compact().unwrap();
    }
    let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
    assert_eq!(rows(&db), 250, "a compacted paged store did not reopen intact");
    assert!(db.get("p/n255").is_some());
    assert!(db.get("p/n3").is_none(), "a deleted row survived the fold");
    assert_eq!(db.one("p/n40").forward("next").collect().len(), 1,
               "edges did not survive the fold");
}

/// Repeated crash-and-reopen, each round adding writes without a graceful close.
/// A leak that only shows up after several rounds shows up here.
#[test]
fn many_crash_cycles_stay_consistent() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 40);
    let mut expected = 40usize;
    for round in 0..8 {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        let base = 100 + round * 20;
        for i in base..base + 20 {
            db.put(&format!("p/n{i}"), &row(i)).unwrap();
        }
        expected += 20;
        // Compact on alternate rounds; the others die with a dirty log.
        if round % 2 == 1 {
            db.compact().unwrap();
        }
        assert_eq!(rows(&db), expected, "round {round}: live count wrong before the crash");
        drop(db);

        let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        assert_eq!(rows(&db), expected, "round {round}: writes lost across a crash");
        assert!(db.get(&format!("p/n{}", base + 5)).is_some(),
                "round {round}: a row from this round is missing");
    }
}

// ── damage ───────────────────────────────────────────────────────────────────

/// Truncating any paged file, at any point, must not panic and must not invent
/// data. Losing rows is allowed — the file was cut in half — but a store that
/// answers at all has to answer truthfully.
#[test]
fn a_truncated_paged_file_never_panics_or_invents() {
    let dir0 = tempfile::TempDir::new().unwrap();
    build(dir0.path(), 120);
    let files = paged_files(dir0.path());
    drop(dir0);

    for file in files {
        for fraction in [0usize, 1, 3, 7] {
            let dir = tempfile::TempDir::new().unwrap();
            build(dir.path(), 120);
            let path = dir.path().join(&file);
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let len = meta.len();
            if len == 0 { continue }
            // 0 = empty, then progressively more of the file kept.
            let keep = len * fraction as u64 / 8;
            let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.set_len(keep).unwrap();
            drop(f);

            // Opening may fail — that is a legitimate answer to a destroyed file.
            let Ok(db) = CoreDB::open_with_config(dir.path(), paged()) else { continue };
            let got = rows(&db);
            assert!(got <= 120,
                    "{file} cut to {keep}/{len} bytes reported {got} rows, more than the \
                     120 the database ever held — damage invented data");
            // Whatever it does return has to be real rows, readable and correct.
            for hit in db.query("SELECT _key FROM p").unwrap().collect() {
                let Some(raw) = db.get(&hit.slug) else { continue };
                let v: serde_json::Value = serde_json::from_str(&raw)
                    .unwrap_or_else(|e| panic!("{file}@{keep}: unparseable payload: {e}"));
                let key = v["_key"].as_str().unwrap_or("");
                assert!(hit.slug.ends_with(key),
                        "{file} cut to {keep} bytes: index says {} but the record says {key} \
                         — damage turned one row into another", hit.slug);
            }
        }
    }
}

/// Garbage written over a paged file must be declined, not misread.
#[test]
fn a_paged_file_full_of_garbage_is_declined() {
    let dir0 = tempfile::TempDir::new().unwrap();
    build(dir0.path(), 60);
    let files = paged_files(dir0.path());
    drop(dir0);

    for file in files {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), 60);
        let path = dir.path().join(&file);
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        std::fs::write(&path, vec![0xA5u8; meta.len() as usize]).unwrap();

        match CoreDB::open_with_config(dir.path(), paged()) {
            Err(_) => {} // refusing is correct
            Ok(db) => {
                let got = rows(&db);
                assert!(got <= 60,
                        "{file} filled with garbage produced {got} rows out of a 60-row store");
                for hit in db.query("SELECT _key FROM p").unwrap().collect() {
                    if let Some(raw) = db.get(&hit.slug) {
                        let v: serde_json::Value = serde_json::from_str(&raw)
                            .unwrap_or_else(|e| panic!("{file}: garbage became a payload: {e}"));
                        let key = v["_key"].as_str().unwrap_or("");
                        assert!(hit.slug.ends_with(key),
                                "{file}: garbage turned {} into {key}", hit.slug);
                    }
                }
            }
        }
    }
}

/// **The free-list claim, tested.** The page-store header is written on sync, so a
/// crash between a free and a sync leaves the header pointing at an older free
/// list. The design says that leaks pages and cannot corrupt. This reverts the
/// header to a previous copy and checks that the store still reads correctly.
#[test]
fn a_stale_page_header_leaks_space_but_never_data() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 150);

    // Capture the header of each record file as it stood, then churn hard enough to
    // free and reuse pages, then put the old header back — the state a crash
    // between a free and the next sync produces.
    let files: Vec<String> = paged_files(dir.path())
        .into_iter()
        .filter(|f| f.ends_with(".rec") || f == "payloads.bin")
        .collect();

    let mut headers: Vec<(String, Vec<u8>)> = Vec::new();
    for f in &files {
        let path = dir.path().join(f);
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() >= 64 { headers.push((f.clone(), bytes[..64].to_vec())) }
        }
    }
    assert!(!headers.is_empty(), "no paged record files to test");

    {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        for i in 0..150 { db.put(&format!("p/n{i}"), &row(i + 10_000)).unwrap(); }
        db.execute("DELETE FROM p WHERE n >= 10100").unwrap();
        db.compact().unwrap();
    }

    for (f, header) in &headers {
        use std::io::{Seek, SeekFrom, Write};
        let mut fh = std::fs::OpenOptions::new().write(true).open(dir.path().join(f)).unwrap();
        fh.seek(SeekFrom::Start(0)).unwrap();
        fh.write_all(header).unwrap();
    }

    // Every row the store still claims to have must be readable and be itself.
    let Ok(db) = CoreDB::open_with_config(dir.path(), paged()) else {
        return; // declining a store with a rewound header is also a valid answer
    };
    for hit in db.query("SELECT _key FROM p").unwrap().collect() {
        let Some(raw) = db.get(&hit.slug) else { continue };
        let v: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("stale header in {files:?}: unparseable payload: {e}"));
        let key = v["_key"].as_str().unwrap_or("");
        assert!(hit.slug.ends_with(key),
                "a rewound page header turned {} into {key}", hit.slug);
    }
}

/// Stale `.tmp` files a process killed mid-compaction could leave, including the
/// paged ones. Reopening must ignore them entirely.
#[test]
fn stale_temp_files_do_not_affect_a_paged_reopen() {
    for leftover in [
        "payloads.bin.tmp", "snapshot.json.tmp", "nodes.bin.tmp",
        "nodesp.rec.tmp", "nodesp.idx.tmp", "adjp_fwd.rec.tmp", "edge_types.tmp",
    ] {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), 50);
        std::fs::write(dir.path().join(leftover), b"PARTIAL GARBAGE \x00\x01\x02").unwrap();

        let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        assert_eq!(rows(&db), 50, "{leftover}: a stale temp file changed what was served");
        assert!(db.get("p/n7").is_some(), "{leftover}: payloads unreadable after a stale temp");
        drop(db);

        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        db.compact().unwrap();
        assert_eq!(rows(&db), 50, "{leftover}: compaction after a crash lost rows");
    }
}

/// A crash during the fold — the paged stores have taken some of the overlay but
/// the compaction never finished — must leave a store that reopens with everything,
/// because the log was not truncated.
#[test]
fn a_crash_during_a_fold_loses_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 100);
    {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        for i in 100..200 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
        // Drop mid-flight: the writes are in the WAL and the overlay, and the fold
        // that would have made them durable in the pages never ran.
        drop(db);
    }
    let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
    assert_eq!(rows(&db), 200, "writes were lost when the fold never ran");
    for i in [0usize, 99, 100, 199] {
        assert!(db.get(&format!("p/n{i}")).is_some(), "row {i} missing after an interrupted fold");
    }
}

/// The two halves of a damaged store must not *contradict* each other.
///
/// The first version of this required the index and the records to agree exactly,
/// and that standard is wrong. They live in different files. Truncating one and
/// not the other makes them disagree by definition — the key really is still in
/// the index, and the record really is gone, and both statements are true. Any
/// store built from more than one file has this property; demanding otherwise
/// would be demanding that partial damage be impossible.
///
/// What must hold is weaker and actually meaningful:
///
/// - nothing readable exceeds what is listed (no rows conjured from damage)
/// - every row that *can* be read is the row it was indexed as
/// - an edge that survives points at a row that survives, or is dropped
/// - and on an **undamaged** store the two halves agree exactly, so the tolerance
///   above is spent on damage rather than hiding an everyday inconsistency
#[test]
fn a_damaged_store_never_contradicts_itself() {
    // The baseline first: with nothing damaged, listed and readable must match.
    {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), 100);
        let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        let listed = db.query("SELECT _key FROM p").unwrap().collect();
        let readable = listed.iter().filter(|h| db.get(&h.slug).is_some()).count();
        assert_eq!(readable, listed.len(),
                   "an undamaged paged store lists {} rows but can read {readable}",
                   listed.len());
        assert_eq!(listed.len(), 100, "an undamaged paged store lost rows");
    }

    let dir0 = tempfile::TempDir::new().unwrap();
    build(dir0.path(), 100);
    let files = paged_files(dir0.path());
    drop(dir0);

    for file in files {
        for keep_eighths in [2usize, 5, 7] {
            let dir = tempfile::TempDir::new().unwrap();
            build(dir.path(), 100);
            let path = dir.path().join(&file);
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if meta.len() == 0 { continue }
            let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.set_len(meta.len() * keep_eighths as u64 / 8).unwrap();
            drop(f);

            let Ok(db) = CoreDB::open_with_config(dir.path(), paged()) else { continue };
            let listed = db.query("SELECT _key FROM p").unwrap().collect();
            assert!(listed.len() <= 100,
                    "{file}@{keep_eighths}/8 listed {} rows in a 100-row store",
                    listed.len());
            let mut readable = 0usize;
            for h in &listed {
                let Some(raw) = db.get(&h.slug) else { continue };
                readable += 1;
                let v: serde_json::Value = serde_json::from_str(&raw)
                    .unwrap_or_else(|e| panic!("{file}@{keep_eighths}/8: unparseable: {e}"));
                let key = v["_key"].as_str().unwrap_or("");
                assert!(h.slug.ends_with(key),
                        "{file}@{keep_eighths}/8: listed {} but the record is {key} — \
                         damage turned one row into another", h.slug);
            }
            assert!(readable <= listed.len(), "more readable than listed, which is impossible");
            // Every edge that is still reported must point at a row still present.
            for h in listed.iter().take(30) {
                for e in db.edges_from(&h.slug) {
                    if let Some(to) = &e.to_slug {
                        assert!(db.get(to).is_some() || db.get(&h.slug).is_none(),
                                "{file}@{keep_eighths}/8: edge {} -> {to} points at a row \
                                 that is not there", h.slug);
                    }
                }
            }
        }
    }
}
