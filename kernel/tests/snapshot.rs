//! 2f (Law 6): snapshot readers beside a live writer.
//!
//! The contract under test: a reader opened on a store serves the newest
//! PUBLISHED generation, byte-stable for its whole life, no matter what the
//! writer does after -- and every mutation through a reader is refused.
//! Mutation-checked: making `is_frozen` answer false (no shadowing) fails
//! `a_readers_view_is_byte_stable_while_the_writer_churns`; picking the
//! OLDER meta slot in read_latest fails `a_new_reader_sees_the_new_publish`.

use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};

fn cfg() -> Config {
    Config { budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
}

fn oracle(s: &Store) -> Vec<(Vec<u8>, Vec<u8>)> {
    s.scan(&[]).unwrap().map(|r| r.unwrap()).collect()
}

#[test]
fn a_readers_view_is_byte_stable_while_the_writer_churns() {
    let d = tempfile::TempDir::new().unwrap();
    let mut w = Store::create(d.path(), cfg()).unwrap();
    for i in 0..20_000u64 {
        w.put(&i.to_be_bytes(), &vec![(i % 251) as u8; 40 + (i % 160) as usize]).unwrap();
    }
    w.commit().unwrap();
    w.checkpoint().unwrap(); // publish generation 1

    // The reader's pool must be far smaller than the dataset, or every
    // oracle() replays from cache and never touches disk again -- a
    // no-shadowing mutation then passes this test (observed: M1 survived
    // until this was tightened). 1 MiB budget over ~4 MiB of rows forces
    // real re-reads of the published pages every round.
    let tiny = Config { budget_bytes: 1 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let r = Store::open_snapshot(d.path(), tiny).unwrap();
    let published = oracle(&r);
    assert_eq!(published.len(), 20_000);

    // The writer now churns HARD across several epochs: updates that dirty
    // published leaves (forcing shadows), deletes, inserts, more publishes.
    for round in 0..3u64 {
        for i in (0..20_000u64).step_by(3) {
            w.put(&i.to_be_bytes(), &vec![b'N'; 64 + (round as usize)]).unwrap();
        }
        for i in (1..20_000u64).step_by(7) {
            w.delete(&i.to_be_bytes()).unwrap();
        }
        for i in 0..2_000u64 {
            w.put(&(100_000 + round * 10_000 + i).to_be_bytes(), b"fresh").unwrap();
        }
        w.commit().unwrap();
        w.checkpoint().unwrap();
        // After EVERY publish, the old reader's world must be bit-identical.
        assert_eq!(oracle(&r), published,
                   "round {round}: the reader's snapshot changed under it");
        for probe in [0u64, 1, 2, 3, 6_999, 19_999] {
            assert_eq!(r.get(&probe.to_be_bytes()).unwrap(),
                       published.iter().find(|(k, _)| k == &probe.to_be_bytes().to_vec())
                           .map(|(_, v)| v.clone()),
                       "round {round}: point get diverged for {probe}");
        }
    }
}

#[test]
fn a_new_reader_sees_the_new_publish() {
    let d = tempfile::TempDir::new().unwrap();
    let mut w = Store::create(d.path(), cfg()).unwrap();
    w.put(b"k", b"old").unwrap();
    w.commit().unwrap();
    w.checkpoint().unwrap(); // gen 1
    let r1 = Store::open_snapshot(d.path(), cfg()).unwrap();

    w.put(b"k", b"new").unwrap();
    w.put(b"only-new", b"x").unwrap();
    w.commit().unwrap();
    w.checkpoint().unwrap(); // gen 2

    let r2 = Store::open_snapshot(d.path(), cfg()).unwrap();
    assert_eq!(r1.get(b"k").unwrap().as_deref(), Some(&b"old"[..]), "r1 pinned at gen 1");
    assert_eq!(r1.get(b"only-new").unwrap(), None, "r1 must not see gen 2 data");
    assert_eq!(r2.get(b"k").unwrap().as_deref(), Some(&b"new"[..]), "r2 opened at gen 2");
    assert_eq!(r2.get(b"only-new").unwrap().as_deref(), Some(&b"x"[..]));
}

#[test]
fn a_reader_refuses_every_mutation() {
    let d = tempfile::TempDir::new().unwrap();
    let mut w = Store::create(d.path(), cfg()).unwrap();
    w.put(b"k", b"v").unwrap();
    w.commit().unwrap();
    w.checkpoint().unwrap();
    drop(w);

    let mut r = Store::open_snapshot(d.path(), cfg()).unwrap();
    assert!(matches!(r.put(b"a", b"b"), Err(kernel::Error::ReadOnly)));
    assert!(matches!(r.delete(b"k"), Err(kernel::Error::ReadOnly)));
    assert!(matches!(r.commit(), Err(kernel::Error::ReadOnly)));
    assert!(matches!(r.checkpoint(), Err(kernel::Error::ReadOnly)));
    assert!(matches!(r.bulk_load(std::iter::empty()), Err(kernel::Error::ReadOnly)));
    // And the refusals changed nothing.
    assert_eq!(r.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
}

#[test]
fn a_reader_serves_the_publish_not_the_wal_tail() {
    let d = tempfile::TempDir::new().unwrap();
    {
        let mut w = Store::create(d.path(), cfg()).unwrap();
        w.put(b"published", b"yes").unwrap();
        w.commit().unwrap();
        w.checkpoint().unwrap(); // gen 1: "published" is in
        w.put(b"tail-only", b"committed but not checkpointed").unwrap();
        w.commit().unwrap();
        // crash: no checkpoint. The WAL holds "tail-only".
    }
    // The reader must serve gen 1 exactly: replaying the tail needs writes.
    let r = Store::open_snapshot(d.path(), cfg()).unwrap();
    assert_eq!(r.get(b"published").unwrap().as_deref(), Some(&b"yes"[..]));
    assert_eq!(r.get(b"tail-only").unwrap(), None,
               "a snapshot reader must not replay the WAL");
    drop(r);
    // The WRITER's reopen does replay it -- nothing was lost.
    let w = Store::open(d.path(), cfg()).unwrap();
    assert_eq!(w.get(b"tail-only").unwrap().as_deref(),
               Some(&b"committed but not checkpointed"[..]));
}

#[test]
fn a_torn_newest_slot_loses_to_the_standing_generation() {
    let d = tempfile::TempDir::new().unwrap();
    let mut w = Store::create(d.path(), cfg()).unwrap();
    w.put(b"k", b"gen1").unwrap();
    w.commit().unwrap();
    w.checkpoint().unwrap(); // gen 1 -> slot 1 (page 1)
    w.put(b"k", b"gen2").unwrap();
    w.put(b"gen2-only", b"x").unwrap();
    w.commit().unwrap();
    w.checkpoint().unwrap(); // gen 2 -> slot 0 (page 0)
    drop(w);

    // Tear the NEWEST slot (gen 2 lives in page 0: generation % 2 == 0).
    let path = d.path().join("data");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[100] ^= 0xFF; // inside page 0's content: its checksum now fails
    std::fs::write(&path, &bytes).unwrap();

    let r = Store::open_snapshot(d.path(), cfg()).unwrap();
    assert_eq!(r.get(b"k").unwrap().as_deref(), Some(&b"gen1"[..]),
               "a torn newest slot must fall back to the standing generation");
    assert_eq!(r.get(b"gen2-only").unwrap(), None);
}

/// The 2f gate in its deterministic form (D25: counters judge, wall time
/// witnesses): a snapshot reader pinned at one generation issues EXACTLY
/// the same disk reads for the same queries whether the writer is idle or
/// mid-ingest. Wall-clock interference on a shared machine is CPU and OS
/// cache physics; the ENGINE's obligation is zero added I/O, and that is
/// countable. Both readers open fresh pools at the same generation, so
/// their miss patterns must be identical to the read.
#[test]
fn a_busy_writer_adds_zero_reads_to_a_snapshot_query() {
    let d = tempfile::TempDir::new().unwrap();
    let mut w = Store::create(d.path(), cfg()).unwrap();
    for i in 0..30_000u64 {
        w.put(&i.to_be_bytes(), &vec![(i % 251) as u8; 80]).unwrap();
    }
    w.commit().unwrap();
    w.checkpoint().unwrap();

    let tiny = Config { budget_bytes: 1 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let queries = |r: &Store| -> u64 {
        let mut acc = 0u64;
        for q in 0..200u64 {
            let k = (q * 149) % 30_000;
            if r.get(&k.to_be_bytes()).unwrap().is_some() { acc += 1; }
        }
        let mut n = 0u64;
        for item in r.scan(&5_000u64.to_be_bytes()).unwrap().take(3_000) {
            item.unwrap(); n += 1;
        }
        acc + n
    };
    let reads_of = |r: &Store| r.io_stats().map(|s| s.take()).map(|(_, _, rd)| rd).unwrap_or(0);
    const ROUNDS: usize = 5;

    // Solo baseline: a fresh reader runs the query set ROUNDS times.
    let r1 = Store::open_snapshot(d.path(), tiny).unwrap();
    let mut solo_answers = Vec::new();
    for _ in 0..ROUNDS { solo_answers.push(queries(&r1)); }
    let solo_reads = reads_of(&r1);
    drop(r1);

    // Busy: same generation, same rounds -- but between every two rounds
    // the writer PUBLISHES an epoch that UPDATES the very keys being read
    // (an epochs-counter handshake proves the interleaving; churn on a
    // disjoint key range let the no-shadowing mutation survive an earlier
    // version of this test).
    let r2 = Store::open_snapshot(d.path(), tiny).unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let epochs = std::sync::atomic::AtomicU64::new(0);
    let (busy_answers, busy_reads) = std::thread::scope(|sc| {
        let (stop_ref, ep_ref) = (&stop, &epochs);
        let writer = sc.spawn(move || {
            let mut round = 0u64;
            while !stop_ref.load(std::sync::atomic::Ordering::Relaxed) {
                for i in 0..30_000u64 {
                    if i % 5 == 0 {
                        w.put(&i.to_be_bytes(), &vec![b'X'; 90 + (round % 30) as usize]).unwrap();
                    }
                }
                w.commit().unwrap();
                w.checkpoint().unwrap();
                round += 1;
                ep_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
        let mut answers = Vec::new();
        for r in 0..ROUNDS {
            let target = (r as u64) + 1;
            while epochs.load(std::sync::atomic::Ordering::Relaxed) < target {
                std::hint::spin_loop();
            }
            answers.push(queries(&r2));
        }
        let busy = reads_of(&r2);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
        (answers, busy)
    });

    assert_eq!(solo_answers, busy_answers,
               "a pinned snapshot answered differently while the writer republished its keys");
    assert_eq!(busy_reads, solo_reads,
               "a busy writer added disk reads to a pinned snapshot query");
    assert!(solo_reads > 0, "the tiny pool must actually have missed to disk");
}
