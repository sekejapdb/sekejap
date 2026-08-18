//! Crash / recovery tests for the write path, especially the bulk fast path.
//! We simulate a crash by dropping the DB WITHOUT a graceful compaction (process
//! death right after a durable write) and by physically damaging the WAL, then
//! reopening. Contrasting scenarios: clean recover, torn tail, corrupt frame,
//! post-compaction tail, and SKBIN-vs-raw parity — all must recover the intact
//! prefix and NEVER panic or serve corrupted data.

use sekejap::{Config, CoreDB};
use serde_json::{json, Value};

fn cfg(binary: bool) -> Config {
    Config { payload_binary: binary, ..Config::default() }
}

fn count(db: &CoreDB, coll: &str) -> i64 {
    db.query(&format!("SELECT COUNT(*) AS n FROM {coll}"))
        .unwrap().collect()[0].payload.as_ref().unwrap()["n"].as_i64().unwrap()
}

/// Bulk write, then "crash" (drop, no compact) → the WAL must replay every row.
#[test]
fn bulk_write_recovers_from_wal_after_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), cfg(true)).unwrap();
        let rows: Vec<(String, Value)> = (0..1000)
            .map(|i| (format!("s/k{i:04}"), json!({"_collection":"s","_key":format!("k{i:04}"),"v":i})))
            .collect();
        db.put_value_bulk(rows).unwrap();
        drop(db); // no compact() — simulate death right after the durable write
    }
    let db = CoreDB::open_with_config(dir.path(), cfg(true)).unwrap();
    assert_eq!(count(&db, "s"), 1000, "all bulk rows recovered from WAL");
    let v: Value = serde_json::from_str(&db.get("s/k0500").unwrap()).unwrap();
    assert_eq!(v["v"], 500, "recovered value is correct");
}

/// Contrast: single durable puts recover the same way as a bulk write.
#[test]
fn single_durable_writes_recover_after_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), cfg(true)).unwrap();
        for i in 0..300 {
            db.put(&format!("s/k{i:04}"), &json!({"_collection":"s","_key":format!("k{i:04}"),"v":i}).to_string()).unwrap();
        }
        drop(db);
    }
    let db = CoreDB::open_with_config(dir.path(), cfg(true)).unwrap();
    assert_eq!(count(&db, "s"), 300);
}

/// Contrast: data written BEFORE a compaction (folded into the snapshot) plus a
/// tail written AFTER it (only in the WAL) must both recover after a crash.
#[test]
fn recovery_spans_snapshot_and_post_compact_wal_tail() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), cfg(true)).unwrap();
        db.put_value_bulk((0..400).map(|i| (format!("s/a{i:04}"), json!({"_collection":"s","_key":format!("a{i:04}"),"v":i}))).collect()).unwrap();
        db.compact().unwrap(); // fold first half into the snapshot, truncate WAL
        db.put_value_bulk((0..400).map(|i| (format!("s/b{i:04}"), json!({"_collection":"s","_key":format!("b{i:04}"),"v":i}))).collect()).unwrap();
        drop(db); // crash: second half lives only in the fresh WAL
    }
    let db = CoreDB::open_with_config(dir.path(), cfg(true)).unwrap();
    assert_eq!(count(&db, "s"), 800, "snapshot + post-compact WAL tail both recover");
    assert_eq!(serde_json::from_str::<Value>(&db.get("s/a0001").unwrap()).unwrap()["v"], 1);
    assert_eq!(serde_json::from_str::<Value>(&db.get("s/b0399").unwrap()).unwrap()["v"], 399);
}

/// A torn WAL tail (crash mid-append) must recover the intact prefix, no panic.
#[test]
fn torn_wal_tail_recovers_prefix_without_panic() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), cfg(true)).unwrap();
        for i in 0..200 {
            db.put(&format!("s/k{i:04}"), &json!({"_collection":"s","_key":format!("k{i:04}"),"v":i}).to_string()).unwrap();
        }
        drop(db);
    }
    // Chop bytes off the end of the WAL — a half-written final frame.
    let wal = dir.path().join("wal.log");
    let len = std::fs::metadata(&wal).unwrap().len();
    assert!(len > 40);
    let f = std::fs::OpenOptions::new().write(true).open(&wal).unwrap();
    f.set_len(len - 17).unwrap();
    drop(f);

    let db = CoreDB::open_with_config(dir.path(), cfg(true)).unwrap();
    let n = count(&db, "s");
    assert!((0..=200).contains(&n), "torn tail: recovered a valid prefix ({n}), no panic");
    // Whatever survived must read back correctly (never garbage).
    if db.get("s/k0000").is_some() {
        assert_eq!(serde_json::from_str::<Value>(&db.get("s/k0000").unwrap()).unwrap()["v"], 0);
    }
}

/// A corrupted WAL frame (bit-rot) must be rejected on replay — never served as a
/// wrong value — and the records before it must still recover.
#[test]
fn corrupt_wal_frame_is_rejected_not_served() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir.path(), cfg(true)).unwrap();
        for i in 0..150 {
            db.put(&format!("s/k{i:04}"), &json!({"_collection":"s","_key":format!("k{i:04}"),"v":i}).to_string()).unwrap();
        }
        drop(db);
    }
    let wal = dir.path().join("wal.log");
    let mut bytes = std::fs::read(&wal).unwrap();
    let idx = bytes.len() - 12; // flip a byte in a late frame
    bytes[idx] ^= 0xff;
    std::fs::write(&wal, &bytes).unwrap();

    let db = CoreDB::open_with_config(dir.path(), cfg(true)).unwrap();
    let n = count(&db, "s");
    assert!((0..=150).contains(&n), "corrupt frame rejected, prefix recovered ({n})");
    // Every recovered record must be internally consistent (v == key index).
    for i in 0..n {
        if let Some(raw) = db.get(&format!("s/k{i:04}")) {
            let v: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(v["v"], i, "recovered record must be correct, never garbage");
        }
    }
}

/// Contrast: recovery is payload-format-independent — SKBIN and raw recover the
/// exact same rows from the same crash scenario.
#[test]
fn skbin_and_raw_recover_identically() {
    let run = |binary: bool| -> i64 {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = CoreDB::open_with_config(dir.path(), cfg(binary)).unwrap();
            db.put_value_bulk((0..500).map(|i| (format!("s/k{i:04}"), json!({"_collection":"s","_key":format!("k{i:04}"),"v":i}))).collect()).unwrap();
            drop(db);
        }
        let db = CoreDB::open_with_config(dir.path(), cfg(binary)).unwrap();
        count(&db, "s")
    };
    assert_eq!(run(true), 500, "SKBIN recovers all rows");
    assert_eq!(run(false), 500, "raw recovers all rows");
    assert_eq!(run(true), run(false), "SKBIN and raw recover identically");
}

/// Contrast of a DIFFERENT KIND — the batch-durability contract of the buffered
/// Engine path: a FLUSHED batch survives a crash, an UN-FLUSHED one is lost by
/// design, and the database stays fully consistent + writable afterward.
///
/// This carried `#[cfg(feature = "engine")]` and had therefore not run since that
/// feature was removed — see the note in `Cargo.toml`, which explains that the
/// engine code is always compiled now because gating zero-dependency code bought
/// nothing. The gate outlived the flag, so the test compiled to nothing and
/// reported success by not existing. The suite counted one fewer test and nobody
/// counts.
#[test]
fn buffered_flushed_survives_unflushed_lost_db_stays_consistent() {
    use sekejap::engine::Engine;
    let dir = tempfile::tempdir().unwrap();
    {
        // Huge buffer so the un-flushed batch never auto-flushes at threshold.
        let engine = Engine::builder(dir.path().to_str().unwrap()).buffer_size(100_000).build().unwrap();
        engine.execute("CREATE TABLE s (_key TEXT PRIMARY KEY, v INTEGER)").unwrap();
        engine.flush().unwrap();
        let stmt = engine.prepare_insert("s", &["_key", "v"]).unwrap();
        for i in 0..100 { engine.insert_prepared(&stmt, &[json!(format!("f{i:03}")), json!(i)]).unwrap(); }
        engine.flush().unwrap();                       // this batch is durable
        for i in 0..50 { engine.insert_prepared(&stmt, &[json!(format!("u{i:03}")), json!(i)]).unwrap(); }
        drop(engine);                                  // crash before flush: batch lost by design
    }
    let engine = Engine::builder(dir.path().to_str().unwrap()).build().unwrap();
    let n = engine.query("SELECT COUNT(*) AS n FROM s").unwrap()[0]
        .payload.as_ref().unwrap()["n"].as_i64().unwrap();
    assert_eq!(n, 100, "flushed batch durable; un-flushed batch lost (batch-durability contract)");
    assert_eq!(engine.query("SELECT v FROM s WHERE _key='f050'").unwrap()[0]
        .payload.as_ref().unwrap()["v"], 50, "recovered row correct");
    assert_eq!(engine.query("SELECT v FROM s WHERE _key='u000'").unwrap().len(), 0, "un-flushed row absent");
    // DB is not wedged — it accepts and durably persists new writes after recovery.
    engine.execute("INSERT INTO s (_key, v) VALUES ('post', 1)").unwrap();
    engine.flush().unwrap();
    assert_eq!(engine.query("SELECT COUNT(*) AS n FROM s").unwrap()[0]
        .payload.as_ref().unwrap()["n"].as_i64().unwrap(), 101);
}


/// **P0.S6 — crash injection around compaction.**
///
/// The compaction guard rail (S4) catches a rewrite that *loses* nodes, but not one
/// interrupted half-way. Compaction is designed to be crash-safe by writing every
/// new file to a `.tmp` path and renaming it into place, so a crash at any point
/// leaves either the old store or the new one — never a blend.
///
/// These simulate a crash by leaving the intermediate files a killed process would
/// have left behind, then reopening.
#[test]
fn compaction_survives_a_crash_at_every_stage() {
    use sekejap::CoreDB;
    use serde_json::json;

    // Stale intermediates a process killed mid-compaction could leave behind.
    let leftovers = [
        "payloads.bin.tmp",
        "snapshot.json.tmp",
        "nodes.bin.tmp",
        "collections.bin.tmp",
    ];

    for (i, leftover) in leftovers.iter().enumerate() {
        for paged in [false, true] {
            let dir = tempfile::TempDir::new().unwrap();
            {
                let mut db = CoreDB::open(dir.path()).unwrap();
                for n in 0..20 {
                    db.put(&format!("v/n{n}"),
                        &json!({"_collection":"v","_key":format!("n{n}"),"i":n}).to_string()).unwrap();
                }
                db.compact().unwrap();
            }

            // Simulate the crash: a partial file of garbage left at the temp path.
            std::fs::write(dir.path().join(leftover), b"PARTIAL GARBAGE \x00\x01\x02").unwrap();

            // Reopening must ignore it and serve the committed data intact.
            let db = if paged {
                CoreDB::open_paged(dir.path()).unwrap()
            } else {
                CoreDB::open(dir.path()).unwrap()
            };
            let rows = db.query("SELECT _key FROM v").unwrap().collect().len();
            assert_eq!(rows, 20,
                "case {i} ({leftover}, paged={paged}): stale temp file must not affect recovery");
            assert!(db.get("v/n7").is_some(), "case {i}: payloads still readable");
            assert_eq!(db.node_count(), 20, "case {i}: node_count intact");
            drop(db);

            // And a real compaction afterwards must still be lossless.
            let mut db = CoreDB::open(dir.path()).unwrap();
            db.compact().unwrap();
            assert_eq!(db.node_count(), 20, "case {i}: compaction after a crash is lossless");
        }
    }
}

/// A crash *between* writes — the WAL has entries the last compaction did not
/// include — must replay them, in both storage modes.
#[test]
fn uncompacted_writes_survive_a_crash_in_both_modes() {
    use sekejap::CoreDB;
    use serde_json::json;

    for paged in [false, true] {
        let dir = tempfile::TempDir::new().unwrap();
        {
            let mut db = CoreDB::open(dir.path()).unwrap();
            for n in 0..10 {
                db.put(&format!("v/n{n}"), &json!({"_collection":"v","_key":format!("n{n}")}).to_string()).unwrap();
            }
            db.compact().unwrap();
        }
        {
            // Writes after the compaction, then "crash" (drop without compacting).
            let mut db = if paged {
                CoreDB::open_paged(dir.path()).unwrap()
            } else {
                CoreDB::open(dir.path()).unwrap()
            };
            for n in 10..15 {
                db.put(&format!("v/n{n}"), &json!({"_collection":"v","_key":format!("n{n}")}).to_string()).unwrap();
            }
            db.remove("v/n0"); // includes a delete against the base
            // Drop without compacting — the WAL is already durable per write, so this
            // is what a killed process leaves behind (mem::forget would leak the file
            // lock inside this same process and block the reopen).
            drop(db);
        }
        let db = CoreDB::open(dir.path()).unwrap();
        assert_eq!(db.query("SELECT _key FROM v").unwrap().collect().len(), 14,
            "paged={paged}: 10 + 5 new - 1 removed must be recovered from the WAL");
        assert!(db.get("v/n0").is_none(), "paged={paged}: the delete replayed too");
        assert!(db.get("v/n14").is_some(), "paged={paged}: the last write replayed");
    }
}

/// Every `Engine` write path must be on disk before it returns.
///
/// In service mode the engine takes over the log's fsync so that writers
/// committing at the same time can share one — the fsync moves out of the write
/// lock, but nothing is deferred and no write is acknowledged early. If any write
/// path were to miss the commit, its records would sit in the log unsynced and a
/// power cut would lose data the caller was told was written.
#[test]
fn every_engine_write_path_is_durable() {
    use sekejap::engine::Engine;
    use serde_json::json;

    let dir = tempfile::TempDir::new().unwrap();
    {
        let eng = Engine::open_as_service(dir.path().to_str().unwrap()).unwrap();
        eng.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();

        eng.execute("INSERT INTO d (_key, body) VALUES ('a', 'via execute')").unwrap();
        eng.execute_params("INSERT INTO d (_key, body) VALUES ($1, $2)",
                           &[json!("b"), json!("via params")]).unwrap();
        eng.execute("UPDATE d SET body = 'updated' WHERE _key = 'a'").unwrap();
        eng.execute("INSERT INTO d (_key, body) VALUES ('c', 'to be deleted')").unwrap();
        eng.execute("DELETE FROM d WHERE _key = 'c'").unwrap();
        // Dropped without compacting — what a killed process leaves behind.
        std::mem::drop(eng);
    }

    let db = sekejap::CoreDB::open(dir.path()).unwrap();
    assert!(db.get("d/a").unwrap().contains("updated"), "execute + UPDATE lost");
    assert!(db.get("d/b").unwrap().contains("via params"), "execute_params lost");
    assert!(db.get("d/c").is_none(), "DELETE lost");
}

/// Compaction replaces the log with a new file, restarting its record count.
///
/// A coordinator that did not notice would keep syncing a descriptor onto the
/// removed inode, and — because the count restarts at zero — would conclude every
/// later write was already durable and stop syncing altogether. Auto-compaction
/// runs on its own, so this is the ordinary path, not an edge case.
#[test]
fn writes_stay_durable_across_a_compaction() {
    use sekejap::engine::Engine;

    let dir = tempfile::TempDir::new().unwrap();
    {
        let eng = Engine::open_as_service(dir.path().to_str().unwrap()).unwrap();
        eng.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        for i in 0..5 {
            eng.execute(&format!("INSERT INTO d (_key, n) VALUES ('before{i}', {i})")).unwrap();
        }
        eng.compact().unwrap();                       // log generation advances
        for i in 0..5 {
            eng.execute(&format!("INSERT INTO d (_key, n) VALUES ('after{i}', {i})")).unwrap();
        }
        eng.compact().unwrap();                       // and again
        for i in 0..5 {
            eng.execute(&format!("INSERT INTO d (_key, n) VALUES ('later{i}', {i})")).unwrap();
        }
        std::mem::drop(eng);                          // no final compaction
    }

    let db = sekejap::CoreDB::open(dir.path()).unwrap();
    for prefix in ["before", "after", "later"] {
        for i in 0..5 {
            assert!(db.get(&format!("d/{prefix}{i}")).is_some(),
                    "{prefix}{i} lost — a write after a compaction was not synced");
        }
    }
}

/// Concurrent writers share an fsync; every one of their writes must still land.
#[test]
fn concurrent_writers_all_land() {
    use sekejap::engine::Engine;
    use std::sync::Arc;

    let dir = tempfile::TempDir::new().unwrap();
    {
        let eng = Arc::new(Engine::open_as_service(dir.path().to_str().unwrap()).unwrap());
        eng.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, t INTEGER)").unwrap();
        let mut handles = Vec::new();
        for t in 0..8 {
            let eng = Arc::clone(&eng);
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    eng.execute(&format!(
                        "INSERT INTO d (_key, t) VALUES ('t{t}r{i}', {t})"
                    )).unwrap();
                }
            }));
        }
        for h in handles { h.join().unwrap(); }
        std::mem::drop(eng);
    }

    let db = sekejap::CoreDB::open(dir.path()).unwrap();
    for t in 0..8 {
        for i in 0..20 {
            assert!(db.get(&format!("d/t{t}r{i}")).is_some(),
                    "t{t}r{i} lost under concurrent commit");
        }
    }
}

/// **Killed in the middle of a compaction, the store still holds every row.**
///
/// Process death, not power loss — see "What this does not prove" below.
///
/// Compaction rewrites the whole durable half and publishes it by renaming files
/// into place. Whether that moment is atomic is not something the other tests
/// here can ask: they crash the process between writes, and `compact()` is
/// synchronous — a panic inside it still unwinds and still runs destructors,
/// which a power cut does not.
///
/// So this spawns a real process, lets it get into a compaction, and sends it
/// SIGKILL at a different point each round. What must hold afterwards is simple
/// and absolute: the rows committed before it started are all still there. A
/// compaction adds nothing and removes nothing; if a reopen finds fewer, the
/// publication was not atomic and the crash landed inside it.
///
/// # What this does not prove
///
/// It does **not** verify the directory fsync, and it is worth being exact about
/// why. `SIGKILL` destroys a process; it does not touch the page cache. Every
/// write and every rename the victim made is still visible to the next opener
/// whether or not it ever reached the platter. Only a power cut or a kernel crash
/// loses that, and neither is reachable from a test.
///
/// Confirmed by removing the `fsync_dir` calls and re-running this: it still
/// passes. So the fsync's value is argued from the write ordering — a rename is a
/// change to the directory, and an unsynced directory entry can be rolled back by
/// recovery — and not from this test.
///
/// What it does prove is the other half, and the half a test can reach: no
/// half-applied in-memory state, no reliance on destructors or a final flush, and
/// no logical torn state between the base files, the snapshot and the log at any
/// point in the sequence.
#[cfg(unix)]
#[test]
fn a_compaction_killed_part_way_through_loses_nothing() {
    const ROWS: usize = 1_500;
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        for i in 0..ROWS {
            db.put(&format!("p/n{i}"),
                   &format!(r#"{{"_collection":"p","_key":"n{i}","n":{i}}}"#)).unwrap();
        }
        db.compact().unwrap();
    }

    // The example binary sits beside this test's own directory:
    // `target/<profile>/examples/compact_victim`. `CARGO_BIN_EXE_` covers `[[bin]]`
    // targets only, and this is deliberately an example — it exists to be killed,
    // not to be shipped.
    let victim = std::env::current_exe()
        .ok()
        .and_then(|p| Some(p.parent()?.parent()?.join("examples").join("compact_victim")))
        .filter(|p| p.exists());
    let Some(victim) = victim else {
        eprintln!("compact_victim was not built; skipping the mid-compaction crash test");
        return;
    };
    // A spread of delays, so the kill lands at a different phase each time —
    // mid-write, between renames, during the log rotation.
    // How many rounds actually killed a process that was still running. A test
    // that passes because the victim had already exited every time is not testing
    // anything, and would look identical from the outside.
    let mut killed_live = 0;
    for (round, delay_ms) in [40u64, 90, 150, 220, 300, 420, 600].into_iter().enumerate() {
        let mut child = match std::process::Command::new(&victim)
            .arg(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => { eprintln!("could not spawn the victim ({e}); skipping"); return }
        };
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        // SIGKILL, not SIGTERM: no unwinding, no destructors, no final flush.
        // `try_wait` first, so a victim that had already exited on its own is not
        // counted as a crash that was survived.
        let still_running = matches!(child.try_wait(), Ok(None));
        let _ = child.kill();
        let _ = child.wait();
        if still_running { killed_live += 1 }

        let db = CoreDB::open(dir.path()).unwrap_or_else(|e| {
            panic!("round {round} (killed after {delay_ms}ms): the store would not \
                    reopen after a compaction was interrupted: {e}")
        });
        let live: std::collections::HashSet<String> = db
            .query("SELECT _key FROM p")
            .unwrap_or_else(|e| panic!("round {round}: the store would not answer: {e:?}"))
            .collect()
            .iter()
            .map(|h| h.slug.clone())
            .collect();
        for i in 0..ROWS {
            let slug = format!("p/n{i}");
            assert!(live.contains(&slug),
                    "round {round} (killed after {delay_ms}ms): {slug} was committed \
                     before the compaction started and is not there afterwards — the \
                     rewrite was published without being atomic");
        }
    }
    assert!(killed_live >= 3,
            "only {killed_live} of 7 rounds killed a process that was still running — \
             the victim is exiting before the kill lands, so this test is not \
             interrupting a compaction and is proving nothing");
}

/// **A read-only handle refuses writes rather than swallowing them.**
///
/// `read_only` lived on `Config` and stopped there — it decided whether to take
/// the directory lock and whether to open a log writer, and nothing downstream
/// learned about it. So a write went through the ordinary path, changed the
/// in-memory maps, found no log to append to, and returned `Ok`.
///
/// The documented behaviour was "writes silently skip WAL persistence". What
/// actually happened was worse: `put` reported success, `DELETE` reported the
/// number of rows it had removed, and reads in the same session answered from the
/// changed overlay — so the handle disagreed with the file it was supposedly
/// reading, and everything vanished at close with nothing raised at any point.
///
/// A dropped write is worse than a failed one. The caller carries on believing
/// the row is there, reads it back in the same session, and finds out at the next
/// open if anyone is looking.
#[test]
fn a_read_only_database_refuses_writes() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
        db.put("p/a", r#"{"_collection":"p","_key":"a","n":1}"#).unwrap();
        db.link("p/a", "p/a", "self");
        db.compact().unwrap();
    }

    {
        let mut ro = CoreDB::open_read_only(dir.path()).unwrap();
        assert!(ro.is_read_only(), "the handle does not know it is read-only");

        // Reads still work — that is the whole point of the mode.
        assert_eq!(ro.query("SELECT _key FROM p").unwrap().collect().len(), 1);
        assert!(ro.get("p/a").is_some());

        // Writes are refused, and say so.
        assert!(ro.put("p/b", r#"{"_collection":"p","_key":"b","n":2}"#).is_err(),
                "put was accepted on a read-only database");
        assert!(ro.execute("INSERT INTO p (_key, n) VALUES ('c', 3)").is_err(),
                "INSERT was accepted on a read-only database");
        assert!(ro.execute("DELETE FROM p WHERE n = 1").is_err(),
                "DELETE was accepted on a read-only database — it used to report \
                 the row count it had removed");
        assert!(ro.execute("UPDATE p SET n = 9 WHERE n = 1").is_err(),
                "UPDATE was accepted on a read-only database");
        assert!(ro.compact().is_err(), "compact was accepted on a read-only database");

        // The attributed-edge writers, which the first sweep of guards missed.
        // They are a separate family — `link_meta`, `link_attr` and `update_edge`
        // carry edge attributes and do not share a chokepoint with `link` — and
        // every one of them wrote through: the graph went from 0 edges to 2 on a
        // handle that is not allowed to write, and `update_edge` returned the
        // number it claimed to have changed.
        assert!(ro.link_meta("p/a", "p/a", "t", r#"{"w":1}"#).is_err(),
                "link_meta was accepted on a read-only database");
        ro.link_attr("p/a", "p/a", "t2", vec![("w".to_string(), serde_json::json!(1))]);
        assert_eq!(ro.update_edge("p/a", "p/a", "t", "{}", r#"{"w":9}"#), 0,
                   "update_edge reported changing edges on a read-only database");

        // The ones whose signature cannot return an error do nothing and record it.
        ro.remove("p/a");
        ro.link("p/a", "p/a", "another");
        assert!(ro.write_error().is_some(),
                "a refused `remove`/`link` left no trace for the caller to find");

        // And after all of that, the handle still agrees with the file.
        assert_eq!(ro.query("SELECT _key FROM p").unwrap().collect().len(), 1,
                   "a refused write still changed what this handle reports");
        assert_eq!(ro.edges_from("p/a").len(), 1,
                   "a refused link/unlink/link_meta/link_attr still changed the graph");
    }

    // Nothing reached the disk.
    let db = CoreDB::open(dir.path()).unwrap();
    assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), 1,
               "a read-only session changed the stored database");
    assert!(db.get("p/a").is_some(), "a read-only session deleted a row");
    assert_eq!(db.edges_from("p/a").len(), 1, "a read-only session changed the graph");
}

/// **Closing an engine does not throw away what it accepted.**
///
/// The write buffer is opt-in and trades durability for speed: a buffered
/// statement is not applied until `flush()`. That is the caller's choice and it
/// is documented.
///
/// `into_inner()` is different, because it *consumes* the engine. There is no
/// later opportunity to flush, so dropping the buffer there turns "not yet
/// durable" into "gone" — for writes the engine already reported as accepted.
///
/// Plain `drop` still discards them: `Engine` has no `Drop` impl, and adding one
/// means running a flush inside a destructor that may be unwinding through the
/// same lock. That gap is documented on `into_inner` rather than papered over.
#[test]
fn closing_an_engine_flushes_what_it_buffered() {
    use sekejap::engine::Engine;

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_str().unwrap().to_string();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
    }
    {
        let engine = Engine::builder(&path)
            .buffer_size(1_000)          // large enough that nothing auto-flushes
            .build()
            .unwrap();
        for i in 0..20 {
            engine.execute(&format!("INSERT INTO p (_key, n) VALUES ('n{i}', {i})"))
                .unwrap();
        }
        // Handed back as a CoreDB — the buffer must have been applied first.
        let db = engine.into_inner().expect("into_inner refused to flush");
        assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), 20,
                   "into_inner returned a database missing the writes the engine \
                    had accepted");
    }
    let db = CoreDB::open(dir.path()).unwrap();
    assert_eq!(db.query("SELECT _key FROM p").unwrap().collect().len(), 20,
               "the writes did not reach disk");
}

