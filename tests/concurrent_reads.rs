//! Readers and a writer on one engine, at the same time.
//!
//! The engine exists so reads do not queue behind writes. Two `thread::spawn`
//! calls in the whole test suite covered that, neither of them here — so the
//! central promise of the service mode had no test under actual concurrency.
//!
//! What is checked is not throughput but *coherence*: every row carries a field
//! derived from another (`check == n * 7 + 3`), so a read that catches a row
//! mid-update — a torn read, or a snapshot spliced from two moments — is
//! detectable from the row alone. A reader may legitimately see an **old** row;
//! it may never see an **inconsistent** one.
//!
//! Run under both settings, because they are different read paths: served from a
//! published snapshot, and served under the shared lock.

use sekejap::engine::Engine;
use sekejap::CoreDB;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

const ROWS: usize = 40;

fn row_json(i: usize, gen: usize) -> String {
    let n = i * 100 + gen;
    serde_json::json!({
        "_collection": "p",
        "_key": format!("n{i}"),
        "n": n as i64,
        "check": (n * 7 + 3) as i64,      // derived: the coherence invariant
        "body": format!("row {n} riverbank"),
    })
    .to_string()
}

/// `Ok(())`, or a description of the row that was not internally consistent.
fn check_row(raw: &str) -> Result<(), String> {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return Err(format!("unparseable row: {e}: {raw}")),
    };
    let (n, check) = (v["n"].as_i64(), v["check"].as_i64());
    match (n, check) {
        (Some(n), Some(c)) if c == n * 7 + 3 => {}
        _ => return Err(format!("torn row: n={n:?} check={check:?} in {raw}")),
    }
    // The body is derived from the same `n`, so it catches a splice the numbers
    // would not.
    let want = format!("row {} riverbank", n.unwrap_or(-1));
    match v["body"].as_str() {
        Some(b) if b == want => Ok(()),
        other => Err(format!("spliced row: body={other:?} want {want:?}")),
    }
}

fn run(snapshot_reads: bool) {
    let dir = tempfile::TempDir::new().unwrap();
    {
        // Resident: the layout a snapshot can share. With the paged default the
        // engine falls back to locked reads, which is the other half of this test
        // and is covered by the `false` case.
        let mut db = CoreDB::open_with_config(dir.path(), sekejap::Config::resident()).unwrap();
        db.execute(
            "CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, check INTEGER, body TEXT)"
        ).ok();
        for i in 0..ROWS {
            db.put(&format!("p/n{i}"), &row_json(i, 0)).unwrap();
        }
        db.compact().unwrap();
    }

    let engine = Arc::new(
        Engine::builder(dir.path().to_str().unwrap())
            .snapshot_reads(snapshot_reads)
            .build()
            .unwrap(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let bad = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let reads = Arc::new(AtomicUsize::new(0));
    let compactions = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..4 {
        let (e, stop, bad, reads) =
            (engine.clone(), stop.clone(), bad.clone(), reads.clone());
        handles.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                for i in 0..ROWS {
                    if let Some(raw) = e.get(&format!("p/n{i}")) {
                        reads.fetch_add(1, Ordering::Relaxed);
                        if let Err(why) = check_row(&raw) {
                            bad.lock().unwrap().push(why);
                            return;
                        }
                    }
                }
                // The scan path too — a different read route to the same rows.
                for raw in e.scan("p") {
                    reads.fetch_add(1, Ordering::Relaxed);
                    if let Err(why) = check_row(&raw) {
                        bad.lock().unwrap().push(why);
                        return;
                    }
                }
            }
        }));
    }

    // One writer, advancing every row through generations.
    {
        let (e, stop) = (engine.clone(), stop.clone());
        handles.push(std::thread::spawn(move || {
            let mut gen = 1usize;
            while !stop.load(Ordering::Relaxed) && gen < 40 {
                for i in 0..ROWS {
                    // Through SQL, which is the engine's write surface: the whole
                    // row is replaced in one statement, so a reader seeing half of
                    // it would be seeing a torn write rather than a torn API call.
                    let n = i * 100 + gen;
                    let _ = e.execute(&format!(
                        "UPDATE p SET n = {n}, check = {}, body = 'row {n} riverbank' \
                         WHERE _key = 'n{i}'", n * 7 + 3));
                }
                gen += 1;
            }
        }));
    }

    // …and a compactor, which is the dangerous one. Compaction rewrites the
    // durable half and swaps it in while readers are holding the old one. If a
    // reader can be handed a base that is being replaced underneath it, this is
    // where it shows: as a torn row, a vanished row, or a panic in a reader
    // thread.
    {
        let (e, stop, compactions) = (engine.clone(), stop.clone(), compactions.clone());
        handles.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if e.compact().is_ok() { compactions.fetch_add(1, Ordering::Relaxed); }
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
        }));
    }

    std::thread::sleep(std::time::Duration::from_millis(1_500));
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().expect("a thread panicked — a read or write path is not thread-safe");
    }

    let failures = bad.lock().unwrap();
    assert!(failures.is_empty(),
            "snapshot_reads={snapshot_reads}: {} incoherent read(s), first: {}",
            failures.len(), failures[0]);
    assert!(compactions.load(Ordering::Relaxed) > 0,
            "snapshot_reads={snapshot_reads}: no compaction completed during the run, \
             so nothing was read while the base was being replaced");
    assert!(reads.load(Ordering::Relaxed) > 100,
            "snapshot_reads={snapshot_reads}: only {} reads completed — the readers \
             were starved or blocked, so this proved nothing",
            reads.load(Ordering::Relaxed));

    // And the database is still whole afterwards.
    assert_eq!(engine.count("p"), ROWS, "rows went missing under concurrent load");
    let mut advanced = 0;
    for i in 0..ROWS {
        let raw = engine.get(&format!("p/n{i}")).expect("a row vanished");
        check_row(&raw).expect("a row is incoherent after the load stopped");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        if v["n"].as_i64() != Some((i * 100) as i64) { advanced += 1 }
    }
    // Otherwise the readers were reading a database nobody was writing to, and
    // "no torn reads" would be true of any store at all.
    assert!(advanced > 0,
            "snapshot_reads={snapshot_reads}: no row ever advanced past its initial              generation — the writer did no work, so nothing was read concurrently              with a write");
}

#[test]
fn readers_never_see_a_half_written_row_under_a_snapshot() {
    run(true);
}

#[test]
fn readers_never_see_a_half_written_row_under_the_lock() {
    run(false);
}
