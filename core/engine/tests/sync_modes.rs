//! The commit barrier is the `SyncMode` the store was opened with, at every
//! one of the page-WAL's publication points, and the default is `Normal`.
//!
//! Before this, the page-WAL hard-coded `sync_full` at all six of them and
//! `collection_backend::check_config` refused any `Config` that did not name
//! `SyncMode::Full`, so a `Normal` or `Off` database could not be opened at
//! all. The kernel `Store` had honoured all three for as long as it has
//! existed; the collection backend did not, and nothing said so.
//!
//! The six sites, in the order a database meets them:
//!
//! | # | Site | `core/engine/src/store/pagewal/mod.rs` | Counter |
//! | - | ---- | -------------------------------------- | ------- |
//! | 1 | create: the two metadata copies | `Pager::initialize` (data) | -- |
//! | 2 | create: the empty WAL | `Pager::initialize` (wal) | -- |
//! | 3 | open: the recovered WAL prefix | `Pager::finish_open` | `wal_fsyncs_open` |
//! | 4 | COMMIT: the commit frame | `Pager::publish` | `wal_fsyncs_commit` |
//! | 5 | checkpoint: the copied-back data pages | `checkpoint_with_crash` | `data_fsyncs_checkpoint` |
//! | 6 | checkpoint: metadata copy 0 | `checkpoint_with_crash` | `metadata_fsyncs` |
//!
//! Sites 1 and 2 run before a handle exists, so they have no site counter of
//! their own; `open_inner` folds their two barriers into the handle that
//! opens the files they made, which is why `sync_full_calls` /
//! `sync_data_calls` exceed the sum of the site counters by exactly two on a
//! create and by zero on a reopen.
use kernel::{io::IoMode, limits::ResourceLimits, store::{Config, SyncMode}};
use sekejap_core::{collections::Database, pagewal::{IoCounters, PageWalStore}, Kind};
use serde_json::json;
use std::{fs, path::{Path, PathBuf}};

const CACHE: usize = 1 << 20;

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn cfg(sync: SyncMode) -> Config {
    Config { budget_bytes: CACHE, io: IoMode::Buffered, sync }
}

fn fields() -> Vec<(String, Kind)> {
    vec![("name".into(), Kind::Text), ("n".into(), Kind::Int)]
}

/// The barriers ONE site places, held as a number rather than read back from
/// the store: the oracle every count below is compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sites {
    create_data: u64,
    create_wal: u64,
    open: u64,
    commit: u64,
    checkpoint_data: u64,
    checkpoint_metadata: u64,
}

impl Sites {
    /// What a `create` + `put` + `commit` + `checkpoint` on one fresh handle
    /// places, computed here rather than observed. Every number is a count of
    /// CALLS, not of a mode: under `Off` each of them is skipped entirely.
    ///
    /// `open` is 2 because `open_inner` calls `finish_open` twice on a create
    /// -- once before the empty tree's own commit, once after the caller's
    /// check -- and `commit` is 2 because that empty tree's commit is a
    /// publication like any other. A reopen has one of each.
    const fn of_a_created_database() -> Self {
        Self { create_data: 1, create_wal: 1, open: 2, commit: 2, checkpoint_data: 1, checkpoint_metadata: 1 }
    }
    const fn total(self) -> u64 {
        self.create_data + self.create_wal + self.open + self.commit
            + self.checkpoint_data + self.checkpoint_metadata
    }
    /// Barriers placed at the sites that HAVE a counter (everything but the
    /// two a create issues before a handle exists).
    const fn counted(self) -> u64 {
        self.open + self.commit + self.checkpoint_data + self.checkpoint_metadata
    }
    /// The same sites, but only where `mode` places anything at all.
    const fn under(self, mode: SyncMode) -> Self {
        match mode {
            SyncMode::Full | SyncMode::Normal => self,
            SyncMode::Off => Self {
                create_data: 0, create_wal: 0, open: 0, commit: 0,
                checkpoint_data: 0, checkpoint_metadata: 0,
            },
        }
    }
}

/// Assert the counters of `c` against the oracle `want` for `mode`, site by
/// site, including which PRIMITIVE ran: `Full` must never issue a data
/// barrier and `Normal` must never issue a drive-cache barrier.
fn assert_sites(mode: SyncMode, c: IoCounters, want: Sites) {
    let want = want.under(mode);
    assert_eq!(c.wal_fsyncs_open, want.open, "{mode:?}: barriers at the open site");
    assert_eq!(c.wal_fsyncs_commit, want.commit, "{mode:?}: barriers at the COMMIT site");
    assert_eq!(c.data_fsyncs_checkpoint, want.checkpoint_data, "{mode:?}: barriers at the checkpoint data site");
    assert_eq!(c.metadata_fsyncs, want.checkpoint_metadata, "{mode:?}: barriers at the metadata-flip site");
    assert_eq!(
        c.wal_fsyncs_checkpoint, 0,
        "{mode:?}: the page-WAL truncates its log without a barrier (option 3); nothing may start placing one unannounced"
    );
    let (full, data) = match mode {
        SyncMode::Full => (want.total(), 0),
        SyncMode::Normal => (0, want.total()),
        SyncMode::Off => (0, 0),
    };
    assert_eq!(c.sync_full_calls, full, "{mode:?}: `sync_full` calls");
    assert_eq!(c.sync_data_calls, data, "{mode:?}: `sync_data` calls");
    assert_eq!(
        c.sync_full_calls + c.sync_data_calls,
        want.counted() + want.create_data + want.create_wal,
        "{mode:?}: a barrier was issued somewhere no site counter names, or the other way round"
    );
    assert_eq!(c.fsyncs(), want.counted(), "{mode:?}: the site-counter total");
}

#[test]
fn each_mode_places_exactly_the_barriers_its_six_sites_call_for() {
    for mode in [SyncMode::Full, SyncMode::Normal, SyncMode::Off] {
        let d = dir();
        let p = d.path().join("db");
        let mut s = PageWalStore::open_sync(&p, true, CACHE, mode).unwrap();
        s.put(b"k", b"v").unwrap();
        s.commit().unwrap();
        assert!(s.checkpoint().unwrap(), "{mode:?}: nothing holds a reader slot, so the fold must run");
        assert_sites(mode, s.io_counters(), Sites::of_a_created_database());
        drop(s);

        // A reopen meets one site -- the recovered prefix -- and no other
        // until something is written, so the same helper with a one-site
        // oracle catches a barrier that leaked into the open path.
        let s = PageWalStore::open_sync(&p, false, CACHE, mode).unwrap();
        assert_sites(
            mode,
            s.io_counters(),
            Sites { create_data: 0, create_wal: 0, open: 1, commit: 0, checkpoint_data: 0, checkpoint_metadata: 0 },
        );
        assert_eq!(s.get(b"k").unwrap().as_deref(), Some(&b"v"[..]), "{mode:?}: the committed row");
    }
}

#[test]
fn off_is_not_silently_the_same_thing_as_normal() {
    let counts = |mode| {
        let d = dir();
        let p = d.path().join("db");
        let mut s = PageWalStore::open_sync(&p, true, CACHE, mode).unwrap();
        s.put(b"k", b"v").unwrap();
        s.commit().unwrap();
        assert!(s.checkpoint().unwrap());
        let c = s.io_counters();
        (c.sync_full_calls, c.sync_data_calls, c.fsyncs())
    };
    let full = counts(SyncMode::Full);
    let normal = counts(SyncMode::Normal);
    let off = counts(SyncMode::Off);
    assert_eq!(full, (8, 0, 6));
    assert_eq!(normal, (0, 8, 6));
    assert_eq!(off, (0, 0, 0));
    assert_ne!(normal, off, "Off must be observably weaker than Normal, not a relabelling of it");
    assert_ne!(full, normal, "Normal must be observably weaker than Full, not a relabelling of it");
}

#[test]
fn a_typed_database_accepts_every_mode_and_refuses_only_what_it_cannot_honour() {
    for mode in [SyncMode::Full, SyncMode::Normal, SyncMode::Off] {
        let d = dir();
        let p = d.path().join("db");
        let mut db = Database::create(&p, cfg(mode)).unwrap();
        let c = db.create_collection("c", fields(), Default::default()).unwrap();
        db.put(c, "k0", &json!({"name": "a", "n": 1})).unwrap();
        db.commit().unwrap();
        let counters = db.io_counters().unwrap();
        match mode {
            SyncMode::Full => {
                assert!(counters.sync_full_calls > 0, "Full must place drive-cache barriers");
                assert_eq!(counters.sync_data_calls, 0);
            }
            SyncMode::Normal => {
                assert_eq!(counters.sync_full_calls, 0, "Normal must never place a drive-cache barrier");
                assert!(counters.sync_data_calls > 0, "Normal must place data barriers");
            }
            SyncMode::Off => {
                assert_eq!((counters.sync_full_calls, counters.sync_data_calls), (0, 0));
            }
        }
        drop(db);
        let db = Database::open(&p, cfg(mode)).unwrap();
        assert_eq!(db.get(c, "k0").unwrap().unwrap().document, json!({"name": "a", "n": 1}));
        // Every mode is also admissible on the read side, where no barrier
        // is ever placed.
        drop(db);
        let snap = Database::open_snapshot(&p, cfg(mode)).unwrap();
        assert_eq!(snap.io_counters().unwrap().fsyncs(), 0, "{mode:?}: a snapshot places no barrier");
    }

    // What `check_config` still refuses: the page-WAL has no unbuffered path
    // to fall back to, and the B-tree cannot descend and split below
    // MIN_CACHE. Both are refused before the directory exists.
    let d = dir();
    let direct = Config { io: IoMode::Direct, ..cfg(SyncMode::Normal) };
    assert!(matches!(
        Database::create(&d.path().join("direct"), direct),
        Err(sekejap_core::collections::Error::Unsupported(_))
    ));
    assert!(!d.path().join("direct").exists(), "a refused config creates nothing");
    let tiny = Config { budget_bytes: 1024, ..cfg(SyncMode::Normal) };
    assert!(matches!(
        Database::create(&d.path().join("tiny"), tiny),
        Err(sekejap_core::collections::Error::Unsupported(_))
    ));
    assert!(!d.path().join("tiny").exists());
}

/// Nothing else about a commit changes with the mode: the same rows, the
/// same catalog, the same resource policy, the same files.
#[test]
fn a_database_written_under_normal_is_byte_identical_in_everything_but_its_barriers() {
    let d = dir();
    let mut paths = Vec::new();
    for mode in [SyncMode::Full, SyncMode::Normal] {
        let p = d.path().join(format!("{mode:?}"));
        let limits = ResourceLimits {
            data_bytes: 4 << 20,
            wal_bytes: 1 << 20,
            tracked_pages: 256,
            readers: 2,
            record_bytes: 64 << 10,
            recovery_bytes: 64 << 10,
        };
        let mut db = Database::create_limited(&p, cfg(mode), limits).unwrap();
        let c = db.create_collection("c", fields(), Default::default()).unwrap();
        for n in 0..200i64 {
            db.put(c, &format!("k{n}"), &json!({"name": format!("row {n}"), "n": n})).unwrap();
        }
        db.commit().unwrap();
        db.checkpoint().unwrap();
        drop(db);
        paths.push(p);
    }
    let read = |p: &Path, name: &str| fs::read(p.join(name)).unwrap();
    // The metadata copies carry a random per-database identity, so the two
    // data files cannot be compared byte for byte; their LENGTH and every
    // row they answer can be.
    assert_eq!(read(&paths[0], "data").len(), read(&paths[1], "data").len(), "same pages written");
    assert_eq!(read(&paths[0], "wal").len(), read(&paths[1], "wal").len(), "same WAL left behind");
    for p in &paths {
        let db = Database::open(p, cfg(SyncMode::Normal)).unwrap();
        let c = db.collection("c").unwrap().unwrap();
        assert_eq!(db.scan(c, None).unwrap().count(), 200);
        for n in 0..200i64 {
            assert_eq!(db.get(c, &format!("k{n}")).unwrap().unwrap().document["n"], json!(n));
        }
    }
}

// ── recovery under Normal ────────────────────────────────────────────────
//
// What `Normal` promises is a process- and OS-crash guarantee: the bytes of
// an acknowledged commit are out of the page cache and in the drive's hands
// before `commit` returns, so a kill, a panic or a reboot cannot lose them.
// What it does NOT promise is a power-loss guarantee: a drive that
// acknowledged a write it had not yet persisted can lose recently
// acknowledged commits. That is exactly the bargain SQLite makes with
// `fullfsync` off and PostgreSQL with a plain `fsync`, and it is why
// `SyncMode::Full` stays reachable. A test process cannot cut power to a
// drive, so what is tested here is the half that is testable, which is also
// the half that changed.

/// Child: writes and commits under `Normal`, then dies without unwinding --
/// no `Drop`, no flush, exactly the shape `pagewal.rs::crash_child` uses.
#[test]
fn normal_crash_child() {
    let Ok(p) = std::env::var("E4_SYNC_CRASH_PATH") else { return };
    let path = PathBuf::from(p);
    let mut db = Database::create(&path, cfg(SyncMode::Normal)).unwrap();
    let c = db.create_collection("c", fields(), Default::default()).unwrap();
    for n in 0..300i64 {
        db.put(c, &format!("k{n}"), &json!({"name": format!("row {n}"), "n": n})).unwrap();
    }
    db.commit().unwrap();
    // Acknowledged. Everything after this line must be absent, not partial.
    db.put(c, "dangling", &json!({"name": "never", "n": -1})).unwrap();
    std::process::exit(86);
}

#[test]
fn a_process_death_under_normal_still_recovers_every_acknowledged_row() {
    let d = dir();
    let p = d.path().join("crash");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "normal_crash_child", "--nocapture"])
        .env("E4_SYNC_CRASH_PATH", &p)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(86), "the child must have died mid-write, not finished");

    let db = Database::open(&p, cfg(SyncMode::Normal)).unwrap();
    let c = db.collection("c").unwrap().unwrap();
    assert_eq!(db.scan(c, None).unwrap().count(), 300, "every acknowledged row survives a process death under Normal");
    for n in 0..300i64 {
        assert_eq!(
            db.get(c, &format!("k{n}")).unwrap().unwrap().document,
            json!({"name": format!("row {n}"), "n": n})
        );
    }
    assert!(db.get(c, "dangling").unwrap().is_none(), "the unacknowledged write must be absent, not partial");
}

/// The checkpoint crash matrix of `pagewal.rs::crash_child`, run with the
/// mode set to `Normal`: a process death at each of the seven stages of a
/// checkpoint still recovers every acknowledged row.
#[test]
fn normal_checkpoint_crash_child() {
    let Ok(p) = std::env::var("E4_SYNC_CKPT_PATH") else { return };
    let stage: u8 = std::env::var("E4_SYNC_CKPT_STAGE").unwrap().parse().unwrap();
    let mut s = PageWalStore::open_sync(Path::new(&p), true, 64 << 10, SyncMode::Normal).unwrap();
    for i in 0..200u64 {
        s.put(&i.to_be_bytes(), &vec![1u8; 256]).unwrap();
    }
    s.commit().unwrap();
    s.checkpoint().unwrap();
    for i in 0..200u64 {
        s.put(&i.to_be_bytes(), &vec![2u8; 256]).unwrap();
    }
    s.commit().unwrap();
    s.test_checkpoint_crash(stage).unwrap();
    panic!("fault did not terminate child");
}

#[test]
fn a_checkpoint_death_under_normal_preserves_every_acknowledged_row() {
    let d = dir();
    for stage in 1..=7u8 {
        let p = d.path().join(format!("ckpt-{stage}"));
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "normal_checkpoint_crash_child", "--nocapture"])
            .env("E4_SYNC_CKPT_PATH", &p)
            .env("E4_SYNC_CKPT_STAGE", stage.to_string())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86), "stage {stage}: the child must have died inside the checkpoint");
        let s = PageWalStore::open_sync(&p, false, 64 << 10, SyncMode::Normal).unwrap();
        for i in 0..200u64 {
            assert_eq!(
                s.get(&i.to_be_bytes()).unwrap(),
                Some(vec![2u8; 256]),
                "stage {stage}: row {i} was acknowledged before the crash"
            );
        }
        let mut count = 0;
        s.scan(|_, _| { count += 1; true }).unwrap();
        assert_eq!(count, 200, "stage {stage}: no row appeared or vanished");
    }
}
