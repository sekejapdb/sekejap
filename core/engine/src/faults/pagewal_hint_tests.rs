//! Publication correctness: a reader is pointed only at a prefix whose
//! barrier returned and whose hint the writer wrote; damaged, stale or
//! foreign hints refuse; failed publication resolves through durable-truth
//! rollback; quiescent readers derive state strictly and create nothing
//! before validation.
use super::*;
use std::{fs, sync::atomic::AtomicBool};

/// `fail_at_write`: the n-th `write_at` (1-based) fails; 0 = never.
struct Flaky { inner: Arc<dyn FileIo>, fail_at_write: AtomicUsize, writes: AtomicUsize, fail_sync: AtomicBool }
impl Flaky {
    fn new(inner: Arc<dyn FileIo>) -> Arc<Self> {
        Arc::new(Self { inner, fail_at_write: AtomicUsize::new(0), writes: AtomicUsize::new(0), fail_sync: AtomicBool::new(false) })
    }
}
fn eio() -> Error { std::io::Error::from_raw_os_error(5).into() }
impl FileIo for Flaky {
    fn requires_alignment(&self) -> bool { false }
    fn read_at(&self, b: &mut [u8], off: u64) -> Result<()> { self.inner.read_at(b, off) }
    fn write_at(&self, b: &[u8], off: u64) -> Result<()> {
        let n = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.fail_at_write.load(Ordering::SeqCst) { return Err(eio()); }
        self.inner.write_at(b, off)
    }
    fn len(&self) -> Result<u64> { self.inner.len() }
    fn set_len(&self, n: u64) -> Result<()> { self.inner.set_len(n) }
    fn sync_data(&self) -> Result<()> { self.inner.sync_data() }
    // A failing barrier returns without syncing: the frame may or may not be
    // durable, which is exactly the uncertain outcome the contract names.
    fn sync_full(&self) -> Result<()> {
        if self.fail_sync.load(Ordering::SeqCst) { return Err(eio()); }
        self.inner.sync_full()
    }
    fn sync_full_primitive(&self) -> &'static str { self.inner.sync_full_primitive() }
    fn sync_dir(&self) -> Result<()> { self.inner.sync_dir() }
}
const CACHE: usize = 64 << 10;
fn seed(p: &Path) -> PageWalStore {
    let mut s = PageWalStore::open(p, true, CACHE).unwrap();
    s.put(b"a", b"v1").unwrap();
    s.commit().unwrap();
    s
}
fn a(s: &PageWalStore) -> Vec<u8> { s.get(b"a").unwrap().unwrap() }
fn hint_file(p: &Path) -> Box<dyn FileIo> { io::open_file(&p.join(GATE), IoMode::Buffered).unwrap().0 }
fn current_hint(p: &Path) -> (Hint, [u8; 96]) {
    let mut b = [0u8; 96];
    hint_file(p).read_at(&mut b, 0).unwrap();
    (hint_decode(&b[..48]).unwrap(), b)
}

// Publication has two copy boundaries. Copy 0 failing publishes nothing:
// readers, in-process snapshots and the writer's own bookkeeping stay on the
// previous transaction. Copy 0 landing and copy 1 failing already publishes
// (a reader picks the newer valid copy), so the writer's bookkeeping swaps
// too and only the caller's `commit` reports the uncertain outcome. In both
// cases the writer is poisoned, checkpoint refuses, and rollback restores a
// coherent, fully published state from the durable files.
#[test]
fn hint_write_failure_at_each_copy_boundary_keeps_readers_and_writer_coherent() {
    for boundary in [1usize, 2] {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("db");
        let mut s = seed(&p);
        let flaky = Flaky::new(hint_file(&p).into());
        flaky.fail_at_write.store(boundary, Ordering::SeqCst);
        s.test_replace_hint_io(flaky.clone());
        s.put(b"a", b"v2").unwrap();
        assert!(s.commit().is_err(), "boundary {boundary}: the caller learns the outcome is uncertain");
        assert!(s.get(b"a").is_err(), "writer poisoned");
        assert!(s.checkpoint().is_err(), "no checkpoint without a current hint");
        let published = s.test_published_tx();
        let (h, _) = current_hint(&p);
        let r = PageWalStore::open_snapshot(&p, CACHE).unwrap();
        if boundary == 1 {
            assert_eq!((published, h.published_tx), (2, 2), "nothing published");
            assert_eq!(a(&r), b"v1");
        } else {
            assert_eq!((published, h.published_tx), (3, 3), "copy 0 published: bookkeeping agrees");
            assert_eq!(a(&r), b"v2");
        }
        drop(r);
        // Rollback re-inspects the durable files, finds the complete frame,
        // barriers it and publishes it through both copies: durable truth.
        s.rollback().unwrap();
        assert_eq!(a(&s), b"v2");
        assert_eq!(s.test_published_tx(), 3);
        let (h, both) = current_hint(&p);
        assert_eq!((h.published_tx, h.published_end as u64), (3, fs::metadata(p.join("wal")).unwrap().len()));
        assert_eq!(both[..48], both[48..], "both copies repaired");
        let r = PageWalStore::open_snapshot(&p, CACHE).unwrap();
        assert_eq!(a(&r), b"v2");
        drop(r);
        assert!(s.checkpoint().unwrap());
        let (h, _) = current_hint(&p);
        assert_eq!((h.checkpoint_tx, h.published_tx, h.published_end), (3, 3, 0));
        assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v2");
    }
}

#[test]
fn barrier_failure_after_the_frame_write_hides_the_transaction_until_rollback() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    drop(seed(&p));
    let wal: Arc<dyn FileIo> = io::open_file(&p.join("wal"), IoMode::Buffered).unwrap().0.into();
    let flaky = Flaky::new(wal);
    let hooked = flaky.clone();
    let mut s = PageWalStore::open_with(&p, false, CACHE, move |d, sync| {
        let (data, _) = io::open_file(&d.join("data"), IoMode::Buffered)?;
        Pager::from_files(data.into(), hooked, sync)
    })
    .unwrap();
    s.put(b"a", b"v2").unwrap();
    flaky.fail_sync.store(true, Ordering::SeqCst);
    assert!(s.commit().is_err());
    flaky.fail_sync.store(false, Ordering::SeqCst);
    assert!(s.get(b"a").is_err());
    let before = fs::metadata(p.join("wal")).unwrap().len();
    let r = PageWalStore::open_snapshot(&p, CACHE).unwrap();
    assert_eq!(a(&r), b"v1", "an unbarriered frame is never served");
    drop(r);
    s.rollback().unwrap();
    assert_eq!(fs::metadata(p.join("wal")).unwrap().len(), before, "a complete frame is kept, not truncated");
    assert_eq!(a(&s), b"v2");
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v2");
}

#[test]
fn damaged_stale_or_foreign_hints_refuse_live_readers_until_the_next_publication() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    let mut s = seed(&p);
    let (h, good) = current_hint(&p);
    let hint = hint_file(&p);
    let mut one = good;
    one[10] ^= 1;
    hint.write_at(&one[..48], 0).unwrap();
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v1", "one damaged copy is tolerated");
    hint.write_at(&one[..48], 48).unwrap();
    assert!(PageWalStore::open_snapshot(&p, CACHE).is_err(), "two damaged copies refuse");
    for (why, bad_hint) in [
        ("end past the WAL", Hint { published_end: h.published_end + FRAME as u32, ..h }),
        ("end not on a frame boundary", Hint { published_end: h.published_end - 1, ..h }),
        ("transaction not at the bound", Hint { published_tx: h.published_tx + 1, ..h }),
        ("another checkpoint history", Hint { checkpoint_tx: h.checkpoint_tx + 1, ..h }),
        ("unabsorbed transaction without WAL", Hint { published_end: 0, ..h }),
        ("another database", Hint { identity: [7; 16], ..h }),
    ] {
        let b = hint_encode(&bad_hint);
        hint.write_at(&b, 0).unwrap();
        hint.write_at(&b, 48).unwrap();
        assert!(PageWalStore::open_snapshot(&p, CACHE).is_err(), "{why}");
    }
    // A pair of valid copies naming different publications serves the newer.
    let older = hint_encode(&Hint { published_tx: h.published_tx - 1, published_end: h.published_end - 3 * FRAME as u32, ..h });
    hint.write_at(&good[..48], 0).unwrap();
    hint.write_at(&older, 48).unwrap();
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v1");
    // The next publication repairs both copies.
    hint.write_at(&one[..48], 0).unwrap();
    hint.write_at(&one[..48], 48).unwrap();
    s.put(b"a", b"v2").unwrap();
    s.commit().unwrap();
    let (repaired, _) = current_hint(&p);
    assert_eq!(repaired.published_tx, h.published_tx + 1);
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v2");
    // No writer alive: the hint is not consulted at all.
    drop(s);
    fs::write(p.join(GATE), b"").unwrap();
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v2");
    fs::write(p.join(GATE), b"garbage-of-any-length-and-content").unwrap();
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v2");
    // A returning writer rewrites the hint before it can be used.
    let s = PageWalStore::open(&p, false, CACHE).unwrap();
    let (h, _) = current_hint(&p);
    assert_eq!((h.published_tx, h.published_end as u64), (3, fs::metadata(p.join("wal")).unwrap().len()));
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v2");
    drop(s);
}

#[test]
fn writer_restart_rewrites_the_hint_and_keeps_pinned_readers_stable() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    let mut s = seed(&p);
    s.put(b"a", b"v2").unwrap();
    s.commit().unwrap();
    let r = PageWalStore::open_snapshot(&p, CACHE).unwrap();
    assert_eq!(a(&r), b"v2");
    drop(s);
    // A crashed writer's partial append: bytes past the last commit.
    { use std::io::Write; fs::OpenOptions::new().append(true).open(p.join("wal")).unwrap().write_all(&[9; 100]).unwrap(); }
    let mut s = PageWalStore::open(&p, false, CACHE).unwrap();
    assert_eq!(fs::metadata(p.join("wal")).unwrap().len() % FRAME as u64, 0, "only the tail was truncated");
    assert_eq!(a(&r), b"v2");
    assert!(!s.checkpoint().unwrap(), "the pinned reader defers the restarted writer's checkpoint");
    s.put(b"a", b"v3").unwrap();
    s.commit().unwrap();
    assert_eq!(a(&r), b"v2", "byte-stable for its whole life");
    let r2 = PageWalStore::open_snapshot(&p, CACHE).unwrap();
    assert_eq!(a(&r2), b"v3");
    assert_eq!(r.take_file_io_stats().map(|s| s[0].0 + s[1].0), Some(0), "readers never write");
    drop(r);
    drop(r2);
    assert!(s.checkpoint().unwrap());
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v3", "absorbed state: hint with end 0");
}

#[test]
fn quiescent_readers_derive_state_strictly_and_create_files_only_after_validation() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    let mut s = seed(&p);
    s.put(b"a", b"v2").unwrap();
    s.commit().unwrap();
    drop(s);
    for n in coordination_files() { fs::remove_file(p.join(n)).unwrap(); }
    let listing = |p: &Path| { let mut v: Vec<_> = fs::read_dir(p).unwrap().map(|e| e.unwrap().file_name()).collect(); v.sort(); v };
    let before = listing(&p);
    assert!(PageWalStore::open_snapshot_validated(&p, CACHE, |_| Err(bad("typed refusal"))).is_err());
    assert_eq!(listing(&p), before, "nothing is created before the typed check passes");
    let r1 = PageWalStore::open_snapshot(&p, CACHE).unwrap();
    for n in coordination_files() { assert!(p.join(n).exists()); }
    assert_eq!(fs::metadata(p.join(GATE)).unwrap().len(), 0, "readers never write a hint");
    let r2 = PageWalStore::open_snapshot(&p, CACHE).unwrap();
    assert_eq!((a(&r1), a(&r2)), (b"v2".to_vec(), b"v2".to_vec()));
    assert_eq!((r1.reader_slot(), r2.reader_slot()), (Some(0), Some(1)));
    // An incomplete uncommitted tail is ignored by readers, never truncated.
    { use std::io::Write; fs::OpenOptions::new().append(true).open(p.join("wal")).unwrap().write_all(&[9; 100]).unwrap(); }
    let len = fs::metadata(p.join("wal")).unwrap().len();
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v2");
    assert_eq!(fs::metadata(p.join("wal")).unwrap().len(), len);
    // A complete bad frame is corruption for a quiescent reader too.
    let mut wal = fs::read(p.join("wal")).unwrap();
    wal[100] ^= 1;
    fs::write(p.join("wal"), &wal).unwrap();
    assert!(PageWalStore::open_snapshot(&p, CACHE).is_err());
    wal[100] ^= 1;
    fs::write(p.join("wal"), &wal).unwrap();
    // A writer may open beside admitted readers; their slots defer its checkpoint.
    let mut s = PageWalStore::open(&p, false, CACHE).unwrap();
    assert!(!s.checkpoint().unwrap());
    drop((r1, r2));
    assert!(s.checkpoint().unwrap());
}

// Power loss after a durable commit can leave a valid but stale hint (the
// hint is derived and never barriered). While the next writer recovers, a
// reader that finds `writer.lock` busy takes the live path: it must wait for
// the republished truth or be refused, never serve the stale, previously
// acknowledged transaction. The opener closure runs inside that window
// (ownership claimed, `finish_open` not yet run).
#[test]
fn writer_recovery_window_never_serves_a_stale_hint_to_a_live_reader() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    let mut s = seed(&p);
    let (_, stale) = current_hint(&p);
    s.put(b"a", b"v2").unwrap();
    s.commit().unwrap();
    drop(s);
    // The crash lost the hint update but not the barriered commit frames.
    hint_file(&p).write_at(&stale, 0).unwrap();
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v2", "quiescent readers ignore the hint");
    let outcome: Arc<Mutex<Option<std::result::Result<Vec<u8>, String>>>> = Arc::new(Mutex::new(None));
    let handle: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let (dir, o, h) = (p.clone(), outcome.clone(), handle.clone());
    let mut s = PageWalStore::open_with(&p, false, CACHE, move |d, sync| {
        let (data, _) = io::open_file(&d.join("data"), IoMode::Buffered)?;
        let (wal, _) = io::open_file(&d.join("wal"), IoMode::Buffered)?;
        let pager = Pager::from_files(data.into(), wal.into(), sync)?;
        let (dir2, o2) = (dir.clone(), o.clone());
        *h.lock().unwrap() = Some(std::thread::spawn(move || {
            let r = PageWalStore::open_snapshot(&dir2, CACHE).map(|r| a(&r)).map_err(|e| format!("{e:?}"));
            *o2.lock().unwrap() = Some(r);
        }));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(o.lock().unwrap().is_none(), "a live reader was admitted inside the recovery window");
        Ok(pager)
    })
    .unwrap();
    handle.lock().unwrap().take().unwrap().join().unwrap();
    match outcome.lock().unwrap().take().unwrap() {
        Ok(v) => assert_eq!(v, b"v2", "the reader saw the recovered truth"),
        Err(e) => assert!(e.contains("WouldBlock"), "explicit unavailable, not stale: {e}"),
    }
    let (h, _) = current_hint(&p);
    assert_eq!(h.published_tx, 3, "recovery republished the durable commit");
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v2");
    s.put(b"a", b"v3").unwrap();
    s.commit().unwrap();
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v3");
}

#[test]
fn reader_bound_is_enforced_on_the_held_slot_in_both_admission_paths() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    drop(seed(&p));
    let bound = |_: &PageWalStore| Ok(Some(1usize));
    let r1 = PageWalStore::open_snapshot_validated(&p, CACHE, bound).unwrap();
    assert_eq!(r1.reader_slot(), Some(0));
    assert!(matches!(PageWalStore::open_snapshot_validated(&p, CACHE, bound), Err(Error::ResourceLimit(_))), "quiescent path");
    let s = PageWalStore::open(&p, false, CACHE).unwrap();
    assert!(matches!(PageWalStore::open_snapshot_validated(&p, CACHE, bound), Err(Error::ResourceLimit(_))), "live path");
    drop(r1);
    let r2 = PageWalStore::open_snapshot_validated(&p, CACHE, bound).unwrap();
    assert_eq!(r2.reader_slot(), Some(0), "a refused admission released its slot");
    assert!(PageWalStore::open_snapshot(&p, CACHE).is_ok(), "no bound, slot 1 admitted");
    drop((r2, s));
}

#[test]
fn runtime_limits_refuse_before_framing_and_survive_rollback() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("db");
    let mut s = seed(&p);
    assert!(s.checkpoint().unwrap());
    // tracked_pages: page 0 and the root leaf are the two distinct pages of a
    // small transaction; an overflow value needs more and is refused before
    // any of its frames could commit.
    s.set_runtime_limits(u64::MAX, u64::MAX, 2).unwrap();
    s.put(b"a", b"v2").unwrap();
    s.commit().unwrap();
    assert_eq!(s.tracked_pages(), Some(2));
    let committed = fs::metadata(p.join("wal")).unwrap().len();
    s.put(b"big", &vec![1; 8192]).unwrap();
    assert!(matches!(s.commit(), Err(Error::ResourceLimit(_))));
    s.rollback().unwrap();
    assert_eq!(fs::metadata(p.join("wal")).unwrap().len(), committed);
    assert_eq!(a(&s), b"v2");
    s.put(b"big", &vec![1; 8192]).unwrap();
    assert!(s.commit().is_err(), "limits survive rollback");
    s.rollback().unwrap();
    // wal_bytes: hold the committed prefix with a reader. Without a reader,
    // the next clean transaction may legitimately auto-fold the old WAL and
    // fit under this unchanged cap. Here only one frame of headroom remains,
    // so the multi-frame transaction must refuse at append, not cross the cap.
    let reader = PageWalStore::open_snapshot(&p, CACHE).unwrap();
    let wal_limit = committed + FRAME as u64;
    s.set_runtime_limits(u64::MAX, wal_limit, usize::MAX).unwrap();
    for value in [b"v3", b"v3-again".as_slice()] {
        s.put(b"a", value).unwrap();
        assert!(matches!(s.commit(), Err(Error::ResourceLimit("page-WAL wal_bytes allowance"))));
        assert!(fs::metadata(p.join("wal")).unwrap().len() <= wal_limit,
            "a refused append must not cross wal_bytes");
        assert_eq!(a(&reader), b"v2", "failed commit must preserve the pinned snapshot");
        s.rollback().unwrap();
        assert_eq!(fs::metadata(p.join("wal")).unwrap().len(), committed);
        assert_eq!(a(&s), b"v2");
        assert!(s.get(b"big").unwrap().is_none());
        assert_eq!(a(&reader), b"v2", "rollback must preserve the pinned snapshot");
    }
    drop(reader);
    // data_bytes: extent growth refused before the page is framed.
    let pages = s.data_bytes();
    s.set_runtime_limits(pages, u64::MAX, usize::MAX).unwrap();
    s.put(b"big", &vec![1; 8192]).unwrap();
    assert!(matches!(s.commit(), Err(Error::ResourceLimit(_))));
    s.rollback().unwrap();
    assert!(s.set_runtime_limits(1, u64::MAX, usize::MAX).is_err(), "an allowance below the existing extent is refused");
    s.put(b"a", b"v4").unwrap();
    s.commit().unwrap();
    assert_eq!(a(&PageWalStore::open_snapshot(&p, CACHE).unwrap()), b"v4");
}
