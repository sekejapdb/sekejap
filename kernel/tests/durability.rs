use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};

fn cfg() -> Config { Config { budget_bytes: 32 << 20, io: IoMode::Buffered, sync: SyncMode::Full } }

#[test]
fn a_committed_write_is_there_after_reopening() {
    let d = tempfile::tempdir().unwrap();
    { let mut s = Store::create(d.path(), cfg()).unwrap();
      for i in 0..5000u64 { s.put(&i.to_be_bytes(), &i.to_le_bytes()).unwrap(); }
      s.commit().unwrap(); }

    let s = Store::open(d.path(), cfg()).unwrap();
    for i in 0..5000u64 {
        assert_eq!(s.get(&i.to_be_bytes()).unwrap().as_deref(), Some(&i.to_le_bytes()[..]));
    }
}

#[test]
fn an_uncommitted_write_is_not_there_after_reopening() {
    let d = tempfile::tempdir().unwrap();
    { let mut s = Store::create(d.path(), cfg()).unwrap();
      s.put(b"committed", b"1").unwrap();
      s.commit().unwrap();
      s.put(b"dangling", b"2").unwrap();   // no commit
      drop(s); }                           // no Drop implementation, no flush — crash-equivalent

    let s = Store::open(d.path(), cfg()).unwrap();
    assert_eq!(s.get(b"committed").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(s.get(b"dangling").unwrap(), None);
}

/// Task 17 re-review, R1. `BTree::insert` refuses a record over
/// `page::MAX_RECORD_LEN`; before this fix, `Store::put` had already
/// appended the frame to the WAL by the time that refusal came back, so a
/// crash right after left a genuinely oversized-but-real frame sitting in
/// the log with nothing that had applied it. Reproduces the re-review's
/// exact probe shape: a small committed row, an oversized `put` that must
/// fail, another small row committed right after, then a crash. The
/// oversized attempt must be as if it never happened -- not merely absent
/// itself, but with no effect whatsoever on what the commit right after it
/// can durably keep.
#[test]
fn a_rejected_oversized_put_has_no_effect_on_a_commit_right_after_it() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path(), cfg()).unwrap();
    s.put(b"a", b"1").unwrap();
    s.commit().unwrap();

    // A 20KB VALUE is legal now (overflow chains): it must be durable and
    // exact across the crash, not refused. The thing that must still be
    // refused pre-WAL is an unappliable KEY.
    let oversized = vec![9u8; 20_000];
    s.put(b"oversized", &oversized).unwrap();
    let huge_key = vec![b'k'; kernel::page::MAX_RECORD_LEN];
    match s.put(&huge_key, b"v") {
        Err(kernel::Error::TooLarge) => {}
        other => panic!("expected Err(TooLarge), got {other:?}"),
    }

    s.put(b"z", b"2").unwrap();
    s.commit().unwrap();
    drop(s);               // no Drop implementation, no flush — crash-equivalent

    let s2 = Store::open(d.path(), cfg()).unwrap();
    assert_eq!(s2.get(b"a").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(
        s2.get(b"z").unwrap().as_deref(), Some(&b"2"[..]),
        "a row committed right after a rejected oversized put must survive a crash -- the \
         rejected attempt must not have touched the WAL at all"
    );
    // The big value crossed the crash intact -- WAL replay rebuilt its
    // overflow chain through the same insert choke point.
    assert_eq!(s2.get(b"oversized").unwrap().as_deref(), Some(&vec![9u8; 20_000][..]));
}

/// A checkpoint must place the barrier its mode calls for. Nothing tested
/// this: hard-coding `Barrier::None` in `checkpoint` left all fifty tests
/// passing, so a checkpoint could have stopped making anything durable and
/// the suite would have said it was fine. The counters live on the pool
/// because they are incremented where the barrier is *issued* -- asserting
/// on what `checkpoint` passed would only prove `checkpoint` passed it, not
/// that `flush_all` used it.
#[test]
fn a_checkpoint_places_the_barrier_its_mode_calls_for() {
    // 2f: a checkpoint is TWO barriers -- one making the epoch's pages
    // durable, one making the meta-slot flip durable strictly after them.
    // One combined barrier would let the flip reach the medium before the
    // pages its roots name. Off places neither (Off promises no barrier,
    // and the dual-slot fallback still recovers the previous generation).
    for (mode, want_data, want_full) in [
        (SyncMode::Full,   0u64, 2u64),
        (SyncMode::Normal, 2,    0),
        (SyncMode::Off,    0,    0),
    ] {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config { budget_bytes: 16 << 20, io: IoMode::Buffered, sync: mode };
        let mut s = Store::create(d.path(), cfg).unwrap();
        let before = s.pool_stats();
        s.put(b"k", b"v").unwrap();
        s.commit().unwrap();
        s.checkpoint().unwrap();
        let after = s.pool_stats();
        assert_eq!(after.sync_data_calls - before.sync_data_calls, want_data,
                   "{mode:?}: wrong number of data barriers at checkpoint");
        assert_eq!(after.sync_full_calls - before.sync_full_calls, want_full,
                   "{mode:?}: wrong number of full barriers at checkpoint");
    }
}

#[test]
fn reopening_after_a_checkpoint_finds_everything() {
    let d = tempfile::tempdir().unwrap();
    { let mut s = Store::create(d.path(), cfg()).unwrap();
      for i in 0..20_000u64 { s.put(&i.to_be_bytes(), b"v").unwrap(); }
      s.commit().unwrap(); s.checkpoint().unwrap();
      for i in 20_000..21_000u64 { s.put(&i.to_be_bytes(), b"v").unwrap(); }
      s.commit().unwrap(); }
    let s = Store::open(d.path(), cfg()).unwrap();
    assert_eq!(s.scan(&[]).unwrap().count(), 21_000);
}

// The checkpoint-ordering test (a_checkpoint_rotates_the_log_only_after_the_
// pages_are_durable), the LSN-survives-rotation test
// (a_rotation_followed_by_a_reopen_does_not_reissue_lsn_1), and the barrier
// test (the_three_durability_modes_issue_different_barriers) all need
// Store::checkpoint_trace/next_lsn/barriers, which are `#[cfg(test)]`-gated
// inspection methods. An integration test here links the library built
// WITHOUT `--cfg test`, so those methods would be absent, not merely
// private. They live in kernel/src/store.rs's own `#[cfg(test)] mod tests`
// instead, where the gate actually applies.
