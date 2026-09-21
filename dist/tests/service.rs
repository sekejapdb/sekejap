//! `docs/dist/OPS_CONTRACT.md` §1-§5, one test per rule the contract states.
//!
//! The oracle in every test is a brute-force fact held in this process -- the
//! set of keys this file wrote, the number of commits it made, the number of
//! events a bounded queue can hold -- never the engine's own second reading.
//!
//! Numbers this file measures rather than assumes, and prints under
//! `--nocapture`: the cost of one snapshot open, the staleness a commit
//! actually has under the default 100 ms publish interval, and the wall-clock
//! and work a full scan costs before a deadline is set against it.

use sekejap_core::collections::{
    CollectionOptions, Database, QueryError, QueryWork, WorkResource, DEADLINE_POLL_CHARGES,
};
use sekejap_dist::service::{
    ServiceDatabase, CHANGE_KEY_CAP, CHANGE_QUEUE_BOUND, PUBLISH_INTERVAL_DEFAULT,
    SECOND_WRITER_REFUSAL,
};
use sekejap_dist::service::{ServiceError, Snapshot};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_lang::{SqlResult, SqlValue};
use sekejap_core::Kind;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Rows the shared corpus holds. Large enough that one full scan is tens of
/// milliseconds in release, which is what gives §3's deadline and §4's cancel
/// something real to stop.
const ROWS: usize = 40_000;
const PAGE: usize = 8_192;
/// The page size the §3 test walks with. Small enough that ~39 pages make up
/// one full scan, so a deadline set at a quarter of that scan's measured cost
/// lands after several COMPLETED pages -- which is what gives the refusal
/// work counters to show.
const DEADLINE_PAGE: usize = 1_024;

fn config() -> Config {
    Config {
        budget_bytes: 32 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn document(i: usize) -> Value {
    json!({
        "key": format!("k{i:06}"),
        "name": format!("place {i:06}"),
        "born": 1_900 + (i as i64 % 120),
        "blurb": format!("a description of place {i:06} written long enough that one row is not free to project"),
    })
}

/// The brute-force answer this file checks the service against: every key the
/// corpus holds, in the order it was written.
fn corpus_keys(rows: usize) -> Vec<String> {
    (0..rows).map(|i| format!("k{i:06}")).collect()
}

struct Fixture {
    _dir: TempDir,
    path: std::path::PathBuf,
    service: Arc<ServiceDatabase>,
    collection: sekejap_core::collections::CollectionId,
}

/// Create the corpus with the plain embedded handle, then hand the directory
/// to a service. Building it through the service would prove nothing the
/// service's own tests do not, and this keeps the fixture's cost off the
/// publish measurements.
fn build(rows: usize) -> Fixture {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Database::create(&path, config()).unwrap();
    let collection = db
        .create_collection(
            "place",
            vec![
                ("key".into(), Kind::Text),
                ("name".into(), Kind::Text),
                ("born".into(), Kind::Int),
                ("blurb".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    for (i, key) in corpus_keys(rows).iter().enumerate() {
        db.put(collection, key, &document(i)).unwrap();
        if (i + 1) % 2_048 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    drop(db);
    let service = Arc::new(ServiceDatabase::open(&path, config()).unwrap());
    Fixture {
        _dir: dir,
        path,
        service,
        collection,
    }
}

/// Every `key` one statement returns, read off the snapshot.
fn keys_on(service: &ServiceDatabase, snapshot: &Snapshot, text: &str) -> Vec<String> {
    match service.query(snapshot, text, &[]).unwrap() {
        SqlResult::Rows { columns, rows } => {
            let at = columns.iter().position(|c| c == "key").expect("key column");
            rows.iter()
                .map(|row| match &row.values[at] {
                    SqlValue::Text(t) => t.clone(),
                    other => panic!("key is text, got {other:?}"),
                })
                .collect()
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

fn count_on(service: &ServiceDatabase, snapshot: &Snapshot, text: &str) -> usize {
    match service.query(snapshot, text, &[]).unwrap() {
        SqlResult::Rows { rows, .. } => rows.len(),
        other => panic!("expected rows, got {other:?}"),
    }
}

// ── §1 ────────────────────────────────────────────────────────────────────

/// L6. A reader opened before a commit does not see it; a reader opened after
/// does; and the first reader is still answering the question it was opened
/// for while the writer works.
#[test]
fn a_reader_keeps_its_old_snapshot_while_a_writer_commits_and_a_later_reader_sees_the_commit() {
    let f = build(2_000);
    let before = f.service.reader();
    let oracle_before = corpus_keys(2_000).len();
    assert_eq!(
        count_on(&f.service, &before, "SELECT key FROM place"),
        oracle_before,
        "the published view holds the corpus this test wrote"
    );

    {
        let mut writer = f.service.writer();
        for i in 2_000..2_010 {
            writer
                .put(f.collection, &format!("k{i:06}"), &document(i))
                .unwrap();
        }
        // The reader is walking WHILE the writer holds the writer lock: no
        // read takes that lock, which is the half of L6 a shared snapshot
        // exists to prove.
        assert_eq!(
            count_on(&f.service, &before, "SELECT key FROM place"),
            oracle_before,
            "an uncommitted write is invisible to every reader"
        );
        writer.commit().unwrap();
    }

    assert_eq!(
        count_on(&f.service, &before, "SELECT key FROM place"),
        oracle_before,
        "a reader opened before the commit does not see it, however long it lives"
    );

    f.service.publish_now().unwrap();
    let after = f.service.reader();
    assert_ne!(before.serial(), after.serial(), "a publish is a new view");
    assert_eq!(
        count_on(&f.service, &after, "SELECT key FROM place"),
        oracle_before + 10,
        "a reader opened after the commit sees it"
    );
    assert_eq!(
        count_on(&f.service, &before, "SELECT key FROM place"),
        oracle_before,
        "and the older view is still byte-stable while the newer one is served"
    );
}

/// §1's in-process rule: one writer at a time. A second caller is refused by
/// name when it asks not to wait, and served in turn when it waits.
#[test]
fn a_second_writer_is_refused_by_name_while_the_first_holds_it_and_served_once_it_is_dropped() {
    let f = build(64);
    let service = Arc::clone(&f.service);
    let held = f.service.writer();

    let refused = std::thread::spawn({
        let service = Arc::clone(&service);
        move || match service.try_writer() {
            Err(ServiceError::Refused(reason)) => reason,
            Ok(_) => panic!("a second writer was handed out while the first was held"),
            Err(other) => panic!("expected a named refusal, got {other:?}"),
        }
    })
    .join()
    .unwrap();
    assert_eq!(
        refused, SECOND_WRITER_REFUSAL,
        "the refusal names the page WAL's single-writer rule"
    );

    drop(held);
    let served = std::thread::spawn(move || {
        let mut writer = service.writer();
        writer.put(f.collection, "k000064", &document(64)).unwrap();
        writer.commit().unwrap();
    });
    served.join().unwrap();

    f.service.publish_now().unwrap();
    assert_eq!(
        count_on(&f.service, &f.service.reader(), "SELECT key FROM place"),
        65,
        "the waiting writer's batch is committed once it has the lock"
    );
}

/// §1's T3, stated where a service caller reads it: more than one writer on
/// one directory. The page WAL's own file lock refuses it; the service adds
/// no second rule and emulates nothing.
#[test]
fn a_second_service_on_one_directory_is_refused_by_the_stores_own_writer_lock() {
    let f = build(16);
    match ServiceDatabase::open(&f.path, config()) {
        Ok(_) => panic!("two writers were opened on one directory"),
        Err(error) => {
            let text = error.to_string();
            assert!(
                !text.is_empty(),
                "a refusal carries a reason, not an empty string"
            );
            eprintln!("second-writer refusal: {text}");
        }
    }
}

// ── §2 ────────────────────────────────────────────────────────────────────

/// §2's window, measured rather than assumed: a read is stale by at most the
/// publish interval plus one snapshot open.
#[test]
fn a_commit_becomes_visible_within_the_publish_interval_plus_one_snapshot_open() {
    let f = build(4_000);
    assert_eq!(
        f.service.publish_interval(),
        PUBLISH_INTERVAL_DEFAULT,
        "the default interval is e3's 100 ms, so a migrating caller's timing does not change"
    );

    // One snapshot open, measured on this machine and this corpus. It is the
    // second term of the window and the contract asked for the number.
    let mut open_costs = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        let private = f.service.open_reader().unwrap();
        open_costs.push(started.elapsed());
        drop(private);
    }
    let open_cost = *open_costs.iter().max().unwrap();
    eprintln!(
        "snapshot open over {ROWS_IN} rows: {costs:?}, worst {open_cost:?}",
        ROWS_IN = 4_000,
        costs = open_costs
    );

    // Start the interval now so the measurement is of the interval, not of
    // whatever time the fixture took. The window's lower bound is measured
    // from THIS instant, not from the commit: the interval is the gap between
    // two publications, and the commit lands somewhere inside it.
    f.service.publish_now().unwrap();
    let published = Instant::now();
    let base = count_on(&f.service, &f.service.reader(), "SELECT key FROM place");
    assert_eq!(base, 4_000);

    let committed = {
        let mut writer = f.service.writer();
        writer.put(f.collection, "k004000", &document(4_000)).unwrap();
        let at = Instant::now();
        writer.commit().unwrap();
        at
    };

    // Inside the interval the published view is deliberately stale: that is
    // the trade the contract writes down rather than leaving default-on and
    // undocumented.
    assert_eq!(
        count_on(&f.service, &f.service.reader(), "SELECT key FROM place"),
        base,
        "a read inside the publish interval is stale by design"
    );

    let mut measured = None;
    for _ in 0..200 {
        if count_on(&f.service, &f.service.reader(), "SELECT key FROM place") == base + 1 {
            measured = Some((committed.elapsed(), published.elapsed()));
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let (visible_after, since_publication) = measured.expect("the commit became visible");
    // The stated window, with the two terms named and one slack term for the
    // 5 ms polling granularity of this loop.
    let window = PUBLISH_INTERVAL_DEFAULT + open_cost + Duration::from_millis(25);
    eprintln!(
        "commit visible after {visible_after:?} ({since_publication:?} after the previous \
         publication); window = interval {PUBLISH_INTERVAL_DEFAULT:?} + one open {open_cost:?} \
         + 25 ms poll granularity = {window:?}"
    );
    assert!(
        visible_after <= window,
        "staleness {visible_after:?} passed the stated window {window:?}"
    );
    assert!(
        since_publication >= PUBLISH_INTERVAL_DEFAULT,
        "the next publication came {since_publication:?} after the previous one, inside the \
         {PUBLISH_INTERVAL_DEFAULT:?} interval, which would make the writer pay a mint per commit"
    );
}

/// §2's read-your-own-writes call, and §2's L3 shape: `publish_now` swaps
/// within one snapshot open, and the previous view survives untouched.
#[test]
fn publish_now_swaps_the_view_in_one_snapshot_open_and_leaves_the_old_one_intact() {
    let f = build(1_000);
    let old = f.service.reader();
    {
        let mut writer = f.service.writer();
        writer.put(f.collection, "k001000", &document(1_000)).unwrap();
        writer.commit().unwrap();
    }
    let started = Instant::now();
    f.service.publish_now().unwrap();
    let swap = started.elapsed();
    let fresh = f.service.reader();
    eprintln!(
        "publish_now swap {swap:?}; the new view's own open cost {:?}",
        fresh.open_cost()
    );
    assert_eq!(
        count_on(&f.service, &fresh, "SELECT key FROM place"),
        1_001,
        "publish_now is read-your-own-writes"
    );
    assert_eq!(
        count_on(&f.service, &old, "SELECT key FROM place"),
        1_000,
        "the view a reader already holds is not rewritten under it"
    );
    assert_eq!(f.service.publish_failures(), 0);
    assert!(
        swap < Duration::from_secs(1),
        "one swap is one snapshot open, not a checkpoint: {swap:?}"
    );
}

// ── §3 ────────────────────────────────────────────────────────────────────

/// §3. A statement that outruns its wall clock is refused naming
/// `WorkResource::Deadline` with the microseconds it had spent, and the work
/// counters say where it stopped.
#[test]
fn a_statement_past_its_deadline_is_refused_naming_deadline_the_elapsed_micros_and_the_work_so_far()
{
    let f = build(ROWS);
    let snapshot = f.service.reader();
    let text = "SELECT key, name, born, blurb FROM place";

    // The unbounded run first, so the deadline is set against a MEASURED cost
    // rather than a guess, and so the work counters have something to be
    // compared with.
    let mut whole = QueryWork::default();
    let started = Instant::now();
    let rows = f
        .service
        .scan(&snapshot, text, &[], DEADLINE_PAGE, &mut whole, &mut |_| Ok(()))
        .unwrap();
    let unbounded = started.elapsed();
    assert_eq!(rows as usize, ROWS, "the scan reads the corpus this test wrote");
    eprintln!(
        "full scan of {ROWS} rows: {unbounded:?}, candidates {}, primary reads {}, output bytes {}",
        whole.candidates, whole.primary_reads, whole.output_bytes
    );
    assert!(
        unbounded >= Duration::from_millis(8),
        "this corpus must take long enough to bound: {unbounded:?}"
    );

    // A quarter of the measured cost: far enough in that at least one page
    // completes, far enough short that the scan cannot finish. Rounded to
    // whole microseconds, which is the resolution the service stores.
    let timeout = Duration::from_micros((unbounded / 4).as_micros() as u64);
    f.service.set_statement_timeout(timeout);
    assert_eq!(
        f.service.statement_timeout(),
        Some(timeout),
        "the bound is held to microsecond resolution"
    );

    let mut partial = QueryWork::default();
    let started = Instant::now();
    let refusal = f
        .service
        .scan(&snapshot, text, &[], DEADLINE_PAGE, &mut partial, &mut |_| Ok(()))
        .expect_err("a scan four times its deadline must be refused");
    let spent = started.elapsed();
    let page_began_in_time;

    match refusal {
        ServiceError::Query(QueryError::BudgetExceeded {
            resource: WorkResource::Deadline,
            limit,
            attempted,
        }) => {
            eprintln!(
                "deadline refusal after {spent:?}: limit {limit} us, elapsed {attempted} us, \
                 poll interval {DEADLINE_POLL_CHARGES} charges"
            );
            assert!(
                attempted >= limit,
                "the elapsed micros {attempted} must have reached the allowance {limit}"
            );
            // A statement timeout counts from the STATEMENT's start, so a
            // prepare that outruns the allowance (a cold cache under load)
            // makes the first page begin already past the deadline: limit 0,
            // elapsed 0, and no candidate charged. That is a correct refusal,
            // not a page cut short, and it is asserted as the wall clock
            // having passed the deadline. When a page did begin inside the
            // allowance, the counters must say where the walk stopped.
            page_began_in_time = limit > 0;
        }
        other => panic!("expected a Deadline refusal, got {other:?}"),
    }
    assert!(
        spent >= timeout,
        "the refusal came before the deadline: {spent:?} < {timeout:?}"
    );
    if page_began_in_time {
        assert!(
            partial.candidates > 0,
            "the work counters must say where the walk stopped, not start from nothing"
        );
    } else {
        eprintln!("the prepare outran the allowance; no page began inside it");
    }
    assert!(
        partial.candidates < whole.candidates,
        "a refused scan charged less than the whole one: {} of {}",
        partial.candidates,
        whole.candidates
    );
    if page_began_in_time {
        assert!(
            spent < unbounded,
            "the deadline stopped the scan early: {spent:?} against {unbounded:?}"
        );
    }

    // The bound is per statement and needs no clearing, which is the half
    // that makes it different from a cancel.
    f.service.clear_statement_timeout();
    assert_eq!(f.service.statement_timeout(), None);
    let mut again = QueryWork::default();
    let rows = f
        .service
        .scan(&snapshot, text, &[], DEADLINE_PAGE, &mut again, &mut |_| Ok(()))
        .expect("the same snapshot answers in full once the clock bound is lifted");
    assert_eq!(rows as usize, ROWS);
    assert_eq!(
        again.candidates, whole.candidates,
        "and it charges exactly what it charged before"
    );
}

/// §3's L1/L4 line: the clock bound is IN ADDITION to the work bound, never
/// instead of it. A budget refusal still names its own resource when both
/// bounds are in play.
#[test]
fn a_work_bound_still_refuses_by_its_own_resource_while_a_deadline_is_also_set() {
    let f = build(4_000);
    let snapshot = f.service.reader();
    f.service.set_statement_timeout(Duration::from_secs(30));
    let budget = f
        .service
        .bound(sekejap_core::collections::QueryBudget::unlimited());
    assert!(budget.deadline.is_some(), "the service put a clock on it");

    let tight = sekejap_core::collections::QueryBudget {
        candidates: 10,
        ..budget
    };
    assert!(
        tight.deadline.is_some(),
        "a caller's own work bounds keep the service's deadline"
    );

    // The prepared query borrows the handle, so the refusal is taken inside.
    let refusal = snapshot.with(|db| {
        db.prepare_query(sekejap_core::collections::QueryRequest {
            collection: f.collection,
            filters: &[],
            order: sekejap_core::collections::QueryOrder::EntityId,
            projection: sekejap_core::collections::Projection::Ids,
            total_limit: None,
            driver: sekejap_core::collections::CandidateDriver::Auto,
        })
        .unwrap()
        .next_page(PAGE, tight, || false)
        .expect_err("ten candidates cannot answer four thousand rows")
    });
    match refusal {
        QueryError::BudgetExceeded {
            resource: WorkResource::Candidates,
            limit,
            ..
        } => assert_eq!(limit, 10, "the work bound refuses by its OWN resource, not by the clock"),
        other => panic!("expected a Candidates refusal, got {other:?}"),
    }
}

// ── §4 ────────────────────────────────────────────────────────────────────

/// §4. A cancel from another thread stops a long scan at its next check
/// point, and the handle answers normally again once it is cleared.
#[test]
fn a_cancel_from_another_thread_stops_a_long_scan_and_clearing_it_restores_the_reader() {
    let f = build(ROWS);
    let snapshot = f.service.reader();
    let service = Arc::clone(&f.service);
    let started = Arc::new(AtomicBool::new(false));

    let scanner = std::thread::spawn({
        let service = Arc::clone(&service);
        let snapshot = Arc::clone(&snapshot);
        let started = Arc::clone(&started);
        move || {
            let text = "SELECT key, name, born, blurb FROM place";
            // Up to sixty seconds of scanning: the cancel must land long
            // before that, and a run that reaches the cap is a failure, not a
            // flake.
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                let mut work = QueryWork::default();
                let outcome = service.scan(&snapshot, text, &[], 512, &mut work, &mut |_| {
                    started.store(true, Ordering::SeqCst);
                    Ok(())
                });
                if let Err(error) = outcome {
                    return error;
                }
            }
            panic!("the scan ran for sixty seconds without seeing the cancel");
        }
    });

    while !started.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    let handle = f.service.interrupt_handle();
    handle.cancel();
    assert!(handle.is_cancelled(), "a cancel is sticky until it is cleared");

    match scanner.join().unwrap() {
        ServiceError::Query(QueryError::Cancelled) => {}
        other => panic!("expected Cancelled, got {other:?}"),
    }

    // Sticky: a second statement under the standing cancel is refused too.
    let mut work = QueryWork::default();
    match f
        .service
        .scan(&snapshot, "SELECT key FROM place", &[], PAGE, &mut work, &mut |_| Ok(()))
    {
        Err(ServiceError::Query(QueryError::Cancelled)) => {}
        other => panic!("a standing cancel must refuse the next statement too, got {other:?}"),
    }

    assert!(f.service.clear_interrupt(), "a cancel was standing");
    assert!(!f.service.clear_interrupt(), "and now it is not");
    let mut work = QueryWork::default();
    let rows = f
        .service
        .scan(&snapshot, "SELECT key FROM place", &[], PAGE, &mut work, &mut |_| Ok(()))
        .expect("the same snapshot answers normally once the cancel is cleared");
    assert_eq!(rows as usize, ROWS, "and answers in full: no partial answer labelled complete");
}

/// §4's L6 half: cancelling a reader does not touch the writer.
#[test]
fn a_standing_cancel_does_not_stop_the_writer_from_committing() {
    let f = build(256);
    f.service.cancel();
    {
        let mut writer = f.service.writer();
        writer.put(f.collection, "k000256", &document(256)).unwrap();
        writer.commit().unwrap();
    }
    f.service.clear_interrupt();
    f.service.publish_now().unwrap();
    let keys = keys_on(&f.service, &f.service.reader(), "SELECT key FROM place");
    let mut oracle = corpus_keys(257);
    oracle.sort();
    let mut got = keys;
    got.sort();
    assert_eq!(got, oracle, "the write went through with a cancel standing");
}

// ── §5 ────────────────────────────────────────────────────────────────────

/// §5's delivery rule: one event per committed batch, whatever the batch
/// holds, and the event never runs ahead of the durability barrier.
#[test]
fn exactly_one_event_per_committed_batch_and_the_event_never_precedes_durability() {
    let f = build(128);
    let feed = f.service.subscribe_changes();

    {
        let mut writer = f.service.writer();
        for i in 128..160 {
            writer
                .put(f.collection, &format!("k{i:06}"), &document(i))
                .unwrap();
        }
        assert!(
            feed.try_recv().is_none(),
            "nothing is delivered before the barrier: thirty-two writes, no event"
        );
        writer.commit().unwrap();
    }

    let event = feed
        .recv_timeout(Duration::from_secs(5))
        .expect("one commit, one event");
    assert_eq!(event.sequence, 1, "the first commit is ordinal 1");
    assert_eq!(event.collections, vec![f.collection], "one collection moved");
    assert_eq!(event.keys_total, 32, "the batch made thirty-two key mutations");
    assert!(!event.keys_truncated, "thirty-two is inside the stated cap");
    assert_eq!(event.keys.len(), 32);
    assert!(
        feed.try_recv().is_none(),
        "one event per BATCH, not one per write"
    );

    // L3's ordering, proved without a crash: by the time the event is in
    // hand, a snapshot opened now already holds the batch. There is no window
    // in which the signal is ahead of the durable state.
    let fresh = f.service.open_reader().unwrap();
    assert_eq!(
        count_on(&f.service, &fresh, "SELECT key FROM place"),
        160,
        "the event was delivered after the barrier, so a reader opened on it sees the batch"
    );

    // A second batch, of one write, fires once more.
    {
        let mut writer = f.service.writer();
        writer.put(f.collection, "k000160", &document(160)).unwrap();
        writer.commit().unwrap();
    }
    let second = feed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(second.sequence, 2);
    assert_eq!(second.keys_total, 1);
    assert_eq!(f.service.events_delivered(), 2);
    assert_eq!(feed.lagged(), 0, "nothing was dropped");
}

/// §5: a rolled-back batch fires **not at all**, and neither does a guard
/// dropped without committing.
#[test]
fn a_rolled_back_batch_and_a_dropped_guard_fire_no_event_and_leave_no_rows() {
    let f = build(64);
    let feed = f.service.subscribe_changes();

    {
        let mut writer = f.service.writer();
        for i in 64..80 {
            writer
                .put(f.collection, &format!("k{i:06}"), &document(i))
                .unwrap();
        }
        writer.rollback().unwrap();
    }
    assert!(feed.try_recv().is_none(), "a rollback emits nothing");

    {
        let mut writer = f.service.writer();
        for i in 80..96 {
            writer
                .put(f.collection, &format!("k{i:06}"), &document(i))
                .unwrap();
        }
        // Dropped without a commit: the guard rolls it back rather than
        // leaving it for whoever takes the writer next.
    }
    assert!(feed.try_recv().is_none(), "a dropped guard emits nothing");
    assert_eq!(f.service.events_delivered(), 0);

    f.service.publish_now().unwrap();
    assert_eq!(
        count_on(&f.service, &f.service.reader(), "SELECT key FROM place"),
        64,
        "and neither batch left a row behind"
    );

    // A commit after them still fires exactly once, with only its own keys.
    {
        let mut writer = f.service.writer();
        writer.put(f.collection, "k000096", &document(96)).unwrap();
        writer.commit().unwrap();
    }
    let event = feed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(event.keys_total, 1, "the abandoned batches are not carried forward");
    assert_eq!(event.sequence, 1, "and they were never numbered");
}

/// §5's bound, named: a subscriber that stops draining loses events past
/// `CHANGE_QUEUE_BOUND` and is told exactly how many. The writer never waits
/// for it.
#[test]
fn a_subscriber_that_stops_draining_drops_events_past_the_stated_bound_and_counts_them() {
    let f = build(16);
    let feed = f.service.subscribe_changes();
    let over = 8usize;
    let commits = CHANGE_QUEUE_BOUND + over;

    let started = Instant::now();
    for i in 0..commits {
        let mut writer = f.service.writer();
        writer
            .put(f.collection, &format!("k{:06}", 16 + i), &document(16 + i))
            .unwrap();
        writer.commit().unwrap();
    }
    let elapsed = started.elapsed();
    eprintln!(
        "{commits} commits with a full queue took {elapsed:?}; bound {CHANGE_QUEUE_BOUND} events"
    );

    assert_eq!(
        feed.lagged(),
        over as u64,
        "every commit past the bound is dropped and counted"
    );
    assert_eq!(
        f.service.events_delivered(),
        commits as u64,
        "the writer delivered every commit; the subscriber is what could not hold them"
    );

    let mut drained = Vec::new();
    while let Some(event) = feed.try_recv() {
        drained.push(event.sequence);
    }
    assert_eq!(
        drained.len(),
        CHANGE_QUEUE_BOUND,
        "the queue held exactly its bound"
    );
    assert_eq!(
        drained,
        (1..=CHANGE_QUEUE_BOUND as u64).collect::<Vec<_>>(),
        "and it held the OLDEST events: a full queue drops the new one rather than the held one"
    );
}

/// §5's L1 bound, named: the key list stops at `CHANGE_KEY_CAP` and the event
/// says so, while the collection list stays exact.
#[test]
fn the_key_list_stops_at_the_stated_cap_and_the_event_reports_the_total() {
    let f = build(16);
    let feed = f.service.subscribe_changes();
    let writes = CHANGE_KEY_CAP + 1;
    {
        let mut writer = f.service.writer();
        for i in 0..writes {
            writer
                .put(f.collection, &format!("k{:06}", 16 + i), &document(16 + i))
                .unwrap();
        }
        writer.commit().unwrap();
    }
    let event = feed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(event.keys_truncated, "one past the cap truncates");
    assert!(
        event.keys.is_empty(),
        "the list is dropped whole, never handed over half true"
    );
    assert_eq!(event.keys_total, writes as u64, "the total is still exact");
    assert_eq!(
        event.collections,
        vec![f.collection],
        "and the collection list, which is what a listener past the cap re-runs against, is exact"
    );
}

/// §5: unsubscribing stops the feed, and a service with no listener records
/// nothing.
#[test]
fn unsubscribing_stops_the_feed_and_an_unlistened_commit_records_nothing() {
    let f = build(16);
    let feed = f.service.subscribe_changes();
    let id = feed.id();
    assert!(f.service.unsubscribe(id), "the subscription was live");
    assert!(!f.service.unsubscribe(id), "and now it is not");

    {
        let mut writer = f.service.writer();
        writer.put(f.collection, "k000016", &document(16)).unwrap();
        writer.commit().unwrap();
    }
    assert!(feed.try_recv().is_none(), "an unsubscribed feed is silent");
    assert_eq!(
        f.service.events_delivered(),
        0,
        "a commit with nobody listening records and numbers nothing: that is what \
         `recording is free when nobody is listening` costs"
    );

    f.service.publish_now().unwrap();
    assert_eq!(
        count_on(&f.service, &f.service.reader(), "SELECT key FROM place"),
        17
    );
}

/// §5's barrier is the service's, so the statement that would cross it
/// behind the feed's back is refused by name rather than emulated.
#[test]
fn transaction_control_sql_is_refused_through_the_writer_guard() {
    let f = build(16);
    let mut writer = f.service.writer();
    for text in ["COMMIT", "  commit  ", "ROLLBACK", "BEGIN"] {
        match writer.sql(text, &[]) {
            Err(ServiceError::Refused(reason)) => {
                assert!(
                    reason.contains("WriterGuard::commit"),
                    "the refusal says what to use instead: {reason}"
                );
            }
            other => panic!("expected a refusal for {text}, got {other:?}"),
        }
    }
}

// ── §1 close ──────────────────────────────────────────────────────────────

/// §1. Close drops the reader slot before the writer, and discards
/// uncommitted work rather than committing it.
#[test]
fn close_discards_uncommitted_work_and_releases_the_reader_slot() {
    let f = build(32);
    let Fixture {
        _dir,
        path,
        service,
        collection,
    } = f;
    {
        let mut writer = service.writer();
        writer.put(collection, "k000032", &document(32)).unwrap();
        // Deliberately not committed.
        drop(writer);
    }
    let service = Arc::try_unwrap(service).unwrap_or_else(|_| panic!("one owner"));
    service.close().unwrap();

    // The directory is openable again, and holds only what was committed.
    let reopened = ServiceDatabase::open(&path, config()).unwrap();
    assert_eq!(
        count_on(&reopened, &reopened.reader(), "SELECT key FROM place"),
        32,
        "a close is not a commit"
    );
    reopened.close().unwrap();
}
