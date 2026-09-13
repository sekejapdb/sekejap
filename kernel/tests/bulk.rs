use kernel::bulk::ExternalSort;

/// A run is the temporary, sorted file that feeds the page packer.  Changing
/// a value without adding or removing a record leaves every row-count check
/// satisfied, so the run's own checksum is the only boundary that can stop
/// the changed bytes before they become a published leaf value.
#[test]
fn a_flipped_scratch_value_is_refused_before_packing() {
    let d = tempfile::tempdir().unwrap();
    let mut sort = ExternalSort::new(d.path(), 1).unwrap();
    let value = b"scratch-value-byte-flip".to_vec();
    sort.push(b"only-key".to_vec(), value.clone()).unwrap();
    let mut runs = sort.finish().unwrap();

    let run = std::fs::read_dir(d.path()).unwrap().next().unwrap().unwrap().path();
    let mut bytes = std::fs::read(&run).unwrap();
    let at = bytes.windows(value.len()).position(|w| w == value.as_slice())
        .expect("the value must be present in the scratch run");
    bytes[at + value.len() / 2] ^= 0x40;
    std::fs::write(&run, bytes).unwrap();

    let mut merged = runs.iter().unwrap();
    assert!(matches!(merged.next(), Some(Err(kernel::Error::Io(ref e)))
        if e.kind() == std::io::ErrorKind::InvalidData),
        "a value-only byte flip must be caught by scratch framing before pack_tree can publish it");
}

/// Exact end-of-file means no byte of another record exists.  A few bytes of
/// the next header are evidence of a torn scratch write and must not be read
/// as the clean end of the run.
#[test]
fn a_partial_scratch_header_is_truncation_not_end_of_file() {
    let d = tempfile::tempdir().unwrap();
    let mut sort = ExternalSort::new(d.path(), 1 << 20).unwrap();
    let first = b"first-key".to_vec();
    let second = b"second-key".to_vec();
    sort.push(first.clone(), b"first-value".to_vec()).unwrap();
    sort.push(second.clone(), b"second-value".to_vec()).unwrap();
    let mut runs = sort.finish().unwrap();

    let run = std::fs::read_dir(d.path()).unwrap().next().unwrap().unwrap().path();
    let bytes = std::fs::read(&run).unwrap();
    let second_key = bytes.windows(second.len()).position(|w| w == second.as_slice())
        .expect("the second key must be present in the scratch run");
    // Leave part of the second record's fixed header, but none of its key.
    std::fs::OpenOptions::new().write(true).open(&run).unwrap()
        .set_len((second_key - 4) as u64).unwrap();

    let mut merged = runs.iter().unwrap();
    assert_eq!(merged.next().unwrap().unwrap().0, first);
    assert!(matches!(merged.next(), Some(Err(kernel::Error::Io(ref e)))
        if e.kind() == std::io::ErrorKind::UnexpectedEof),
        "a partial header must be truncation, not a clean end-of-file");
}

#[test]
fn a_scratch_length_is_bounded_before_it_allocates() {
    let d = tempfile::tempdir().unwrap();
    let mut sort = ExternalSort::new(d.path(), 1).unwrap();
    sort.push(b"bounded-key".to_vec(), b"value".to_vec()).unwrap();
    let mut runs = sort.finish().unwrap();

    let run = std::fs::read_dir(d.path()).unwrap().next().unwrap().unwrap().path();
    let mut bytes = std::fs::read(&run).unwrap();
    bytes[..4].copy_from_slice(&1024u32.to_le_bytes());
    std::fs::write(&run, bytes).unwrap();

    assert!(matches!(runs.iter().unwrap().next(), Some(Err(kernel::Error::Io(ref e)))
        if e.kind() == std::io::ErrorKind::InvalidData),
        "a disk length above the writer's trusted maximum must be refused before allocation");
}

#[test]
fn truncation_on_a_record_boundary_is_not_a_clean_complete_run() {
    let d = tempfile::tempdir().unwrap();
    let mut sort = ExternalSort::new(d.path(), 1 << 20).unwrap();
    let first = b"first-boundary-key".to_vec();
    let second = b"second-boundary-key".to_vec();
    sort.push(first.clone(), b"first-value".to_vec()).unwrap();
    sort.push(second.clone(), b"second-value".to_vec()).unwrap();
    let mut runs = sort.finish().unwrap();

    let run = std::fs::read_dir(d.path()).unwrap().next().unwrap().unwrap().path();
    let bytes = std::fs::read(&run).unwrap();
    let second_key = bytes.windows(second.len()).position(|w| w == second.as_slice()).unwrap();
    std::fs::OpenOptions::new().write(true).open(&run).unwrap()
        .set_len((second_key - 12) as u64).unwrap();

    let mut merged = runs.iter().unwrap();
    assert_eq!(merged.next().unwrap().unwrap().0, first);
    assert!(matches!(merged.next(), Some(Err(kernel::Error::Io(ref e)))
        if e.kind() == std::io::ErrorKind::InvalidData),
        "the trusted item count must expose a whole-record truncation");
}

#[test]
fn a_sort_far_larger_than_its_arena_comes_out_ordered() {
    let d = tempfile::tempdir().unwrap();
    // 1 MiB arena, ~24 MiB of input: at least 20 runs, so the merge is real.
    let mut s = ExternalSort::new(d.path(), 1 << 20).unwrap();
    let n = 300_000u64;
    let scatter = |i: u64| i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for i in 0..n { s.push(scatter(i).to_be_bytes().to_vec(), b"v".to_vec()).unwrap(); }
    let mut runs = s.finish().unwrap();
    assert!(runs.run_count() > 10, "arena must have forced many runs, got {}", runs.run_count());

    let mut prev: Option<Vec<u8>> = None;
    let mut count = 0u64;
    for item in runs.iter().unwrap() {
        let (k, _, _) = item.unwrap();
        if let Some(p) = &prev { assert!(p <= &k, "merge emitted out of order"); }
        prev = Some(k); count += 1;
    }
    assert_eq!(count, n);
}

#[test]
fn bulk_load_and_point_lookup_agree() {
    let d = tempfile::tempdir().unwrap();
    let cfg = kernel::store::Config {
        budget_bytes: 16 << 20, io: kernel::io::IoMode::Buffered,
        sync: kernel::store::SyncMode::Full };
    let mut s = kernel::store::Store::create(d.path(), cfg).unwrap();
    let n = 200_000u64;
    let scatter = |i: u64| i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    s.bulk_load((0..n).map(|i| (scatter(i).to_be_bytes().to_vec(), i.to_le_bytes().to_vec()))).unwrap();
    s.commit().unwrap();
    s.checkpoint().unwrap();

    for i in (0..n).step_by(97) {
        assert_eq!(s.get(&scatter(i).to_be_bytes()).unwrap().as_deref(),
                   Some(&i.to_le_bytes()[..]), "key {i} missing after bulk load");
    }
    assert_eq!(s.scan(&[]).unwrap().count() as u64, n);
}

/// A bulk load writes no WAL records, so if it does not make itself durable the
/// caller is told Ok and loses everything on the next crash.
#[test]
fn a_bulk_load_survives_a_crash_without_a_later_checkpoint() {
    let d = tempfile::tempdir().unwrap();
    let cfg = kernel::store::Config {
        budget_bytes: 16 << 20, io: kernel::io::IoMode::Buffered,
        sync: kernel::store::SyncMode::Full };
    let n = 50_000u64;
    {
        let mut s = kernel::store::Store::create(d.path(), cfg).unwrap();
        s.bulk_load((0..n).map(|i| (i.to_be_bytes().to_vec(), b"v".to_vec()))).unwrap();
        // No commit or extra checkpoint. Store has no Drop flush; dropping
        // closes the descriptor as a real process death does.
        drop(s);
    }
    let s = kernel::store::Store::open(d.path(), cfg).unwrap();
    assert_eq!(s.scan(&[]).unwrap().count() as u64, n,
               "a bulk load must be durable when it returns");
}

/// Duplicates must be refused, not packed. Packed, one of them becomes
/// unreachable by `get` while still visible to `range` — a divergence between
/// point lookup and scan, which is the worst kind of wrong answer.
#[test]
fn a_bulk_load_refuses_duplicate_keys() {
    let d = tempfile::tempdir().unwrap();
    let cfg = kernel::store::Config {
        budget_bytes: 16 << 20, io: kernel::io::IoMode::Buffered,
        sync: kernel::store::SyncMode::Off };
    let mut s = kernel::store::Store::create(d.path(), cfg).unwrap();
    let items = vec![
        (1u64.to_be_bytes().to_vec(), b"a".to_vec()),
        (2u64.to_be_bytes().to_vec(), b"b".to_vec()),
        (2u64.to_be_bytes().to_vec(), b"c".to_vec()),
    ];
    let err = s.bulk_load(items.into_iter()).unwrap_err();
    assert!(matches!(err, kernel::Error::DuplicateKey),
            "a duplicate key must be refused as DuplicateKey, not silently made unreachable \
             or reported as some other error: {err:?}");
}

/// Merge memory must be bounded by a chosen fanout, not by the input. With a
/// tiny arena this produces far more runs than MAX_FANOUT, so the multi-pass
/// path is genuinely exercised.
#[test]
fn a_merge_with_far_more_runs_than_the_fanout_still_completes_in_order() {
    let d = tempfile::tempdir().unwrap();
    let mut srt = kernel::bulk::ExternalSort::new(d.path(), 64 << 10).unwrap();
    let n = 200_000u64;
    let scatter = |i: u64| i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for i in 0..n { srt.push(scatter(i).to_be_bytes().to_vec(), b"v".to_vec()).unwrap(); }
    let mut runs = srt.finish().unwrap();
    assert!(runs.run_count() > kernel::bulk::SortedRuns::MAX_FANOUT,
            "the arena must force more runs than the fanout, got {}", runs.run_count());

    let mut prev: Option<Vec<u8>> = None;
    let mut count = 0u64;
    for item in runs.iter().unwrap() {
        let (k, _, _) = item.unwrap();
        if let Some(p) = &prev { assert!(p < &k, "multi-pass merge emitted out of order"); }
        prev = Some(k); count += 1;
    }
    assert_eq!(count, n, "multi-pass merge lost records");
}

/// A missing run file must surface as an error, not abort the process.
///
/// This is the regression that matters: the filename-collision bug was
/// dangerous precisely because a vanished run file panicked instead of
/// returning. Deleting a file is portable and deterministic -- no permissions,
/// no platform-specific fault injection -- so this can be a standing test rather
/// than a scratch experiment.
#[test]
fn a_missing_run_file_is_an_error_not_a_panic() {
    let d = tempfile::tempdir().unwrap();
    let scratch = d.path().join("sort");
    let mut srt = kernel::bulk::ExternalSort::new(&scratch, 64 << 10).unwrap();
    for i in 0..50_000u64 { srt.push(i.to_be_bytes().to_vec(), b"v".to_vec()).unwrap(); }
    let mut runs = srt.finish().unwrap();
    assert!(runs.run_count() > 1, "the fixture needs more than one run");

    // Remove one run behind the merge's back.
    let victim = std::fs::read_dir(&scratch).unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|x| x == "tmp"))
        .expect("a run file to delete");
    std::fs::remove_file(&victim).unwrap();

    assert!(runs.iter().is_err(), "a missing run must be reported, not panicked on");
}

/// A sort dropped without `finish()` must not leave its scratch behind.
///
/// `Store::bulk_load` returns through `?` when a `push` fails -- before
/// `finish()` produces the `SortedRuns` whose `Drop` would clean up -- so without
/// this every failed load leaks a directory for the life of the machine.
#[test]
fn a_sort_dropped_without_finishing_removes_its_scratch() {
    let d = tempfile::tempdir().unwrap();
    let scratch = d.path().join("sort");
    {
        let mut srt = kernel::bulk::ExternalSort::new(&scratch, 64 << 10).unwrap();
        for i in 0..50_000u64 { srt.push(i.to_be_bytes().to_vec(), b"v".to_vec()).unwrap(); }
        assert!(scratch.exists(), "the fixture needs the scratch to exist first");
    }
    assert!(!scratch.exists(), "an unfinished sort must remove its own scratch");
}

#[test]
fn bulk_and_split_density_matches_the_balance_policy() {
    // Bulk packing beats independent splits. Neighbor redistribution now
    // reaches the packer's density on this workload without whole-tree sorting.
    let mk = |bulk: bool| -> u64 {
        let d = tempfile::tempdir().unwrap();
        let cfg = kernel::store::Config {
            budget_bytes: 16 << 20, io: kernel::io::IoMode::Buffered,
            sync: kernel::store::SyncMode::Off };
        let mut s = kernel::store::Store::create(d.path(), cfg).unwrap();
        let n = 100_000u64;
        let scatter = |i: u64| i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        if bulk {
            s.bulk_load((0..n).map(|i| (scatter(i).to_be_bytes().to_vec(), b"v".to_vec()))).unwrap();
        } else {
            for i in 0..n { s.put(&scatter(i).to_be_bytes(), b"v").unwrap(); }
        }
        s.commit().unwrap(); s.checkpoint().unwrap();
        std::fs::metadata(d.path().join("data")).unwrap().len()
    };
    let packed = mk(true);
    let split = mk(false);
    #[cfg(feature = "sqlite-balance")]
    assert!(split <= packed, "neighbor-balanced {split} should fit within packed {packed}");
    #[cfg(not(feature = "sqlite-balance"))]
    assert!(packed < split, "packed {packed} should be smaller than split-built {split}");
}

/// `merge_down` runs its own outer pass more than once when a single pass
/// still leaves more than `MAX_FANOUT` runs -- true once the raw run count
/// exceeds `MAX_FANOUT^2`. This is the case a same-named-output-as-input
/// collision surfaces in: a later pass's group 0 writes to the same path an
/// earlier pass's group 0 already produced, which typically sits at
/// position 0 of that later group's own inputs. Forces exactly that with a
/// tiny arena and checks every record still arrives, once, in order.
#[test]
fn a_merge_needing_more_than_one_merge_down_pass_loses_nothing() {
    let d = tempfile::tempdir().unwrap();
    let mut s = kernel::bulk::ExternalSort::new(d.path(), 512).unwrap();
    let n = 60_000u64;
    let scatter = |i: u64| i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for i in 0..n { s.push(scatter(i).to_be_bytes().to_vec(), b"v".to_vec()).unwrap(); }
    let mut runs = s.finish().unwrap();
    let fanout = kernel::bulk::SortedRuns::MAX_FANOUT;
    assert!(runs.run_count() > fanout * fanout,
            "the arena must force more than one merge_down pass (need > {}), got {}",
            fanout * fanout, runs.run_count());

    let mut prev: Option<Vec<u8>> = None;
    let mut count = 0u64;
    for item in runs.iter().unwrap() {
        let (k, _, _) = item.unwrap();
        if let Some(p) = &prev { assert!(p <= &k, "multi-pass merge_down emitted out of order"); }
        prev = Some(k); count += 1;
    }
    assert_eq!(count, n, "multi-pass merge_down lost or duplicated records");
}

/// Two deaths at two different watermarks exercise resume-of-resume, not just
/// the easier one-shot reopen. The manifest is the commit point: records after
/// it may be replayed, records at or below it must appear exactly once.
#[test]
fn a_durable_sort_resumes_twice_from_its_last_watermark() {
    let d = tempfile::tempdir().unwrap();
    let scratch = d.path().join("durable-sort");
    let generation = 41;
    {
        let mut sort = ExternalSort::new_durable(&scratch, 256, generation).unwrap();
        for i in 0..100u64 {
            sort.push(i.to_be_bytes().to_vec(), i.to_le_bytes().to_vec()).unwrap();
        }
        sort.checkpoint(99).unwrap();
        // Simulated kill: Drop must retain the published manifest and runs.
    }
    {
        let (mut sort, watermark) =
            ExternalSort::reopen_durable(&scratch, 256, generation).unwrap();
        assert_eq!(watermark, 99);
        for i in 100..175u64 {
            sort.push(i.to_be_bytes().to_vec(), i.to_le_bytes().to_vec()).unwrap();
        }
        sort.checkpoint(174).unwrap();
        // A second kill proves reopening a reopened build is not special.
    }
    let (mut sort, watermark) =
        ExternalSort::reopen_durable(&scratch, 256, generation).unwrap();
    assert_eq!(watermark, 174);
    for i in 175..250u64 {
        sort.push(i.to_be_bytes().to_vec(), i.to_le_bytes().to_vec()).unwrap();
    }
    sort.checkpoint(249).unwrap();
    let mut runs = sort.finish().unwrap();
    let rows = runs.iter().unwrap().map(|row| row.unwrap()).collect::<Vec<_>>();
    assert_eq!(rows.len(), 250);
    for (i, (key, value, marker)) in rows.into_iter().enumerate() {
        assert_eq!(key, (i as u64).to_be_bytes());
        assert_eq!(value, (i as u64).to_le_bytes());
        assert!(!marker);
    }
    runs.discard().unwrap();
    assert!(!scratch.exists(), "successful publication cleanup must reclaim durable runs");
}

#[test]
fn durable_sort_refuses_stale_generation_and_torn_manifest() {
    let d = tempfile::tempdir().unwrap();
    let scratch = d.path().join("durable-sort");
    {
        let mut sort = ExternalSort::new_durable(&scratch, 256, 7).unwrap();
        sort.push(b"a".to_vec(), b"one".to_vec()).unwrap();
        sort.checkpoint(1).unwrap();
    }
    assert!(ExternalSort::reopen_durable(&scratch, 256, 8).is_err(),
        "a later build must not adopt a prior generation's runs");

    // A torn next candidate cannot displace the last renamed manifest.
    std::fs::write(scratch.join("manifest.tmp"), b"torn").unwrap();
    let (sort, watermark) = ExternalSort::reopen_durable(&scratch, 256, 7).unwrap();
    assert_eq!(watermark, 1);
    sort.discard_durable().unwrap();
}

#[test]
fn durable_sort_refuses_a_changed_published_manifest() {
    let d = tempfile::tempdir().unwrap();
    let scratch = d.path().join("durable-sort");
    {
        let mut sort = ExternalSort::new_durable(&scratch, 256, 9).unwrap();
        sort.push(b"a".to_vec(), b"one".to_vec()).unwrap();
        sort.checkpoint(1).unwrap();
    }
    let manifest = scratch.join("manifest");
    let mut bytes = std::fs::read(&manifest).unwrap();
    bytes[16] ^= 0x40;
    std::fs::write(&manifest, bytes).unwrap();
    assert!(ExternalSort::reopen_durable(&scratch, 256, 9).is_err(),
        "a changed durable manifest must be refused before any run is read");
}
