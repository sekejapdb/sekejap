use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};

fn cfg() -> Config { Config { budget_bytes: 16 << 20, io: IoMode::Buffered, sync: SyncMode::Off } }

/// Damage EVERY file the store writes, discovered from the directory rather than
/// listed, so a file added later is covered automatically.
///
/// Corruption positions are weighted toward the start of each page, because that
/// is where headers keep their counts, and counts are what turn a flipped byte
/// into an out-of-bounds read. A sweep that only flipped bytes at 3, 41, 97, 211,
/// 457 and 971 found zero crashes in the previous engine; weighting toward the
/// start found twelve immediately.
///
/// WHAT THIS ESTABLISHES, and what it does not. A pass means: across 200 trials
/// covering fifteen within-page offsets, both files the store writes, both
/// checkpointed and un-checkpointed states, and the open/get/scan/recover paths,
/// no single flipped byte aborted the process. It does NOT establish that no
/// byte anywhere can abort it — the offsets are a fixed list, not a random
/// walk — and it establishes nothing at all about the other half of Law 5:
/// whether what survived is correct, and whether the loss was named. Those are
/// the recovery tests' subject. Report this result as crash-safety evidence
/// only; a "zero aborts" headline that implies more than that is the same
/// overclaim this project keeps finding.
#[test]
fn no_single_flipped_byte_can_abort_the_process() {
    /// The store writes exactly two files, `data` and `wal`. Named so the
    /// checkpoint decision — made before the directory is read — can stay
    /// independent of which file the trial will go on to damage.
    const FILES_PER_STORE: usize = 2;

    let mut applied = 0usize;
    // Tracked separately by which file the trial damaged, because the two
    // files now answer damage differently ON PURPOSE (Task 19). A damaged
    // `data` page is skipped and rebuilt, so those stores still open. A
    // damaged `wal` frame with committed frames behind it makes `open`
    // REFUSE -- the alternative, which shipped until Task 19, was to
    // truncate the log at the damage and open cleanly having destroyed
    // every committed frame behind it (measured: 1,499 rows of 3,000 for
    // one flipped bit). The refusal is only defensible because `recover()`
    // clears it, which is what `recovered` and the in-loop reopen assert.
    // Measured on this fixture: 0 of the 50 log trials open unaided (its log
    // holds ~3,000 committed frames, so a flipped byte essentially always
    // has committed frames behind it) and 50 of 50 open after `recover()`.
    // Before Task 19 it was 50 of 50 unaided, each having silently thrown
    // away the frames behind the damage.
    let mut applied_data = 0usize;
    let mut still_opened_data = 0usize;
    let mut recovered = 0usize;

    // 3,000 distinct keys, EACH WRITTEN EXACTLY ONCE, value derived from the
    // key rather than a single shared constant -- see the comment at the
    // value assertion below for what that buys and what it cannot buy.
    for trial in 0..200usize {
        let d = tempfile::tempdir().unwrap();
        { let mut s = Store::create(d.path(), cfg()).unwrap();
          // Committed every 50 rows, not once at the very end: a single
          // trailing commit makes the whole log one enormous transaction,
          // so a flipped byte anywhere in it lands in a region that was
          // never committed and is correctly discarded either way. Many
          // small transactions put COMMITTED frames in front of, and
          // behind, wherever the damage falls -- which is the only shape in
          // which "what does a reader do with the frames BEHIND the damage"
          // is a question at all.
          for i in 0..3000u64 {
              s.put(&i.to_be_bytes(), format!("v-{i}").as_bytes()).unwrap();
              if i % 50 == 49 { s.commit().unwrap(); }
          }
          s.commit().unwrap();
          // FILES_PER_STORE, not files.len(): the directory has not been read
          // yet at this point. The store writes `data` and `wal`, so dividing
          // by 2 before taking the parity makes checkpoint state advance once
          // per full cycle through the files, giving all four combinations
          // instead of two.
          if (trial / FILES_PER_STORE) % 2 == 0 { s.checkpoint().unwrap(); } }

        let mut files: Vec<_> = std::fs::read_dir(d.path()).unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file()).collect();
        files.sort();
        if files.is_empty() { continue; }

        // File and checkpoint state must vary INDEPENDENTLY. The store writes
        // exactly two files, so keying both off `trial % 2` made them co-vary in
        // lockstep: the data file would only ever be damaged in a checkpointed
        // store and the log only in an un-checkpointed one, leaving half the
        // combinations — a log that survived a checkpoint, a data file before
        // one — never tested at all.
        let target = &files[trial % files.len()];
        let mut bytes = std::fs::read(target).unwrap();
        if bytes.is_empty() { continue; }

        // Page-relative offsets, weighted to the first 64 bytes where headers
        // keep their counts. PAGE_SIZE comes from the kernel rather than a
        // literal: a bare 4096 here would silently stop targeting page starts
        // the day the page size changed, and nothing would fail to say so.
        let offsets = [1usize, 6, 9, 10, 12, 16, 20, 24, 36, 37, 41, 64, 200, 1000, 4095];
        let pages = (bytes.len() / kernel::page::PAGE_SIZE).max(1);
        let page = (trial * 7) % pages;
        let pos = page * kernel::page::PAGE_SIZE + offsets[trial % offsets.len()];
        // Skip rather than clamp. Clamping to the last byte collapses the 200,
        // 1000 and 4095 offsets onto the same position for any file under a
        // page, which silently removes most of the diversity exactly where the
        // files are smallest.
        if pos >= bytes.len() { continue; }
        let damaged_the_log = target.file_name().is_some_and(|n| n == "wal");
        let before = bytes[pos];
        bytes[pos] ^= 0xff;
        // Count a corruption only if the byte on disk actually changed. With a
        // constant 0xff mask this guard can never fire — no byte equals its
        // own bitwise complement — so it is not what keeps the real sweep
        // honest. It exists for two other reasons: it is what makes the
        // mandated falsification below (`^= 0x00`) actually distinguishable
        // from a real run instead of reporting the same counts either way,
        // and it stands guard for a future variable mask, where "changed" and
        // "attempted" genuinely can diverge. Do not delete it as dead code.
        if bytes[pos] != before {
            applied += 1;
            if !damaged_the_log { applied_data += 1; }
        }
        std::fs::write(target, &bytes).unwrap();

        // Neither opening nor scanning nor recovering may abort the process.
        if let Ok(s) = Store::open(d.path(), cfg()) {
            if !damaged_the_log { still_opened_data += 1; }
            let _ = s.get(&1u64.to_be_bytes());
            // Every key in this fixture is written EXACTLY ONCE, to
            // `format!("v-{key}")` -- so any entry a scan returns must carry
            // the value derived from ITS OWN key. What this catches, that a
            // single shared constant value across all keys could not, is a
            // scan returning the wrong VALUE FOR A GIVEN KEY: a slot
            // directory or key/value pairing corrupted in a way that still
            // passes the page's own checksum by misattributing bytes rather
            // than garbling them. What it cannot catch is anything about a
            // key whose value was overwritten, because this fixture never
            // overwrites one -- and a fixture that did would make ordinary,
            // LEGITIMATE loss of an uncommitted trailing write
            // observationally identical, from a post-hoc value check alone,
            // to a wrong value being resurrected. Measured, when that shape
            // was tried: 2,960 "mismatches" across 200 trials against a
            // correct implementation. A value check on a single-byte-flip
            // sweep has no ground truth to separate the two; hand-built
            // byte layouts are what prove those properties.
            if let Ok(it) = s.scan(&[]) {
                for (k, v) in it.flatten() {
                    if k.len() != 8 { continue; } // a garbled key can't be checked against anything
                    let key = u64::from_be_bytes(k.clone().try_into().unwrap());
                    assert_eq!(
                        v, format!("v-{key}").as_bytes(),
                        "trial {trial}: key {key} read back a value that was never written for it"
                    );
                }
            }
        }
        // A store that refused to open must be REPAIRABLE, and the repair
        // must be enough: `recover()` and then open again. Law 5's second
        // half -- a state that is preserved and permanently unusable is as
        // unrecoverable as one that was deleted -- and the half this sweep
        // could not see before, because it never reopened after recovering:
        // a permanently unopenable store was indistinguishable here from
        // one that merely declined a damaged open. Measured at the time
        // (Task 17 final review, F2): 29 of 400 flipped bits produced
        // stores that would never open again, and this sweep passed
        // through that without noticing.
        let repaired = kernel::recover::recover(d.path(), cfg());
        if repaired.is_ok() {
            recovered += 1;
            if let Err(e) = Store::open(d.path(), cfg()) {
                panic!("trial {trial}: recover() reported success and the store still \
                        will not open: {e:?}");
            }
        }
    }

    // Guard against passing vacuously.
    assert!(applied >= 100, "only {applied} corruptions applied; the sweep is not exercising anything");
    assert_eq!(recovered, applied,
               "only {recovered}/{applied} damaged stores could be repaired at all -- and every \
                repair is followed, in the loop above, by an open that must succeed");
    // MEASURED, and worth recording because the number moved for a reason
    // rather than by drift. Before Task 19 this sweep asserted that half of
    // ALL damaged stores still opened, and got 98/150 -- but 50 of those 98
    // were WAL trials that "opened" by truncating the log at the damage and
    // discarding every committed frame behind it. Stores with a damaged
    // DATA page were 48 of 100 then and are 48 of 100 now: unchanged, and
    // the only half of that old figure that ever meant what it said.
    assert!(still_opened_data >= 45,
            "only {still_opened_data}/{applied_data} stores with a damaged DATA page opened \
             unaided (48 when this bar was set) — the format is too brittle to call recoverable");
}
