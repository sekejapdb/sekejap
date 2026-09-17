//! A refused build must never lose the index it was building.
//!
//! This file began as the reproduction for the 1,000,000-row atomic-arm
//! failure (`phase2_multimodel_bench e4 1000000 32 <dir>/db atomic none` exited
//! 1 with `Error: NotFound("index")` and no stdout.json, server job
//! e4-p2-1m-20260918, commit aeeae13). The sequence it drives is the bench's:
//!
//! ```text
//!   create_scalar_index(...)     // writes the REGISTRY row, does NOT commit
//!   build_index_to_ready(...)    // no commit in between
//! ```
//!
//! `create_index_with_tree` does not commit, so the REGISTRY row
//! (`ikey(REGISTRY, id)`) stayed in the same open transaction as whatever the
//! build did next. For a Scalar-family index that was a single one-shot
//! `tree_pack` of the WHOLE index. Refuse that pack for want of WAL allowance
//! and the driver calls `Database::rollback`, which discards the uncommitted
//! working tree by re-inspecting the durable files -- taking the never-
//! committed CREATE with it. The retry's first act is `index_info(id)`, which
//! found no registry row and returned `NotFound("index")`: not a
//! `ResourceLimit`, so neither the driver's allowance retry nor the bench's own
//! typed-refusal handler in `main` caught it. The index was gone for good.
//!
//! Two things had to change, and this file holds both properties:
//!
//! 1. A build refuses to start while the CALLER has uncommitted writes,
//!    because its own rollback would discard them. Uncommitted INDEX CATALOG
//!    work -- this index's create, or the READY flip of a build run a moment
//!    ago -- is the engine's own and is committed before the build starts,
//!    which is what keeps a refused chunk from rolling the create away and
//!    what lets `create A, build A, create B, build B, commit` work.
//! 2. The build no longer offers the whole index to one transaction. It packs
//!    a first run sized to the allowance and appends the rest, so the refusal
//!    this test forces is not even reached at the sizes that used to force it.
//!
//! The allowance here is small enough that the OLD whole-index pack could not
//! have fitted, so a completed build is evidence for both.

use e4_prototype::{
    collections::{CollectionOptions, Database, IndexState, ScalarPredicate},
    Kind,
};
use kernel::{
    io::IoMode,
    limits::ResourceLimits,
    store::{Config, SyncMode},
};
use serde_json::json;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// Same allowance shape as the engine's own
/// `an_atomic_late_build_is_bounded_by_chunks_not_by_the_whole_index` test
/// (`tests/index_build_equivalence.rs`), which seeds 16,000 rows under this
/// exact `wal_bytes` with no trouble: it is the *single-shot whole-index
/// pack* that cannot fit, not ordinary seeding commits.
const WAL_BYTES: u64 = 256 << 10;
/// The WAL is written in 4144-byte frames (`FRAME` in `src/pagewal.rs`);
/// 256 KiB is about 63 page-frames. A scalar entry is on the order of
/// 20-30 bytes, so a 90%-full 4096-byte leaf holds on the order of 130-150
/// of them -- roughly 8,000-9,500 rows fill 63 leaves. ROWS is chosen well
/// above that estimate so a whole-index pack could not fit, and well under
/// the 20,000-row ceiling for an untimed correctness check on this machine.
const ROWS: u64 = 15_000;

fn limited_db(path: &std::path::Path) -> Database {
    let limits = ResourceLimits {
        data_bytes: 16 << 20,
        wal_bytes: WAL_BYTES,
        tracked_pages: 4096,
        readers: 4,
        record_bytes: 16384,
        recovery_bytes: 256 << 10,
    };
    Database::create_limited(path, cfg(), limits).unwrap()
}

fn seed_people(db: &mut Database) -> e4_prototype::collections::CollectionId {
    let people = db
        .create_collection(
            "people",
            vec![
                ("age".into(), Kind::Int),
                ("parity".into(), Kind::Int),
                ("note".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..ROWS {
        db.put(
            people,
            &format!("p{i:06}"),
            &json!({"age": (i % 95) as i64, "parity": (i % 2) as i64,
                    "note": if i % 3 == 0 { "levee survey" } else { "river silt" }}),
        )
        .unwrap();
        if i % 128 == 127 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    people
}

/// Every row is indexed, under the id the index really got, and the index
/// answers rather than merely existing.
fn assert_whole(db: &Database, id: e4_prototype::collections::IndexId) {
    assert_eq!(db.index_info(id).unwrap().state, IndexState::Ready);
    let mut entries = 0u64;
    for age in 0..95i64 {
        entries += db
            .query_scalar(id, ScalarPredicate::Eq(json!(age)), 65_536)
            .unwrap()
            .len() as u64;
    }
    assert_eq!(entries, ROWS, "the index does not hold every row");
}

/// Control: the calling convention every existing engine test uses --
/// `db.commit()` right after `create_scalar_index`, before any build step.
/// The REGISTRY row is durable before any packing is attempted, so a refused
/// transaction can only roll back the build's own work.
#[test]
fn control_create_then_commit_then_build_survives_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = limited_db(&dir.path().join("db"));
    let people = seed_people(&mut db);

    let age = db
        .create_scalar_index(people, "age_idx", "age", false)
        .unwrap();
    db.commit().unwrap(); // <-- the commit the bench's atomic arm skipped

    let result = db.build_index_to_ready(age, 256);
    eprintln!("control (create + commit + build): {result:?}");
    assert!(
        result.is_ok(),
        "expected the committed-create path to finish under a bounded allowance, got {result:?}"
    );
    assert!(
        db.index_info(age).is_ok(),
        "index vanished after a committed create"
    );
    assert_whole(&db, age);
}

/// The bench's actual atomic-arm sequence: create with NO commit before the
/// build (`src/bin/phase2_multimodel_bench.rs:909-911`, mirrored by every
/// `finish_build` call for vector/spatial/text at atomic policy).
///
/// Before the fix this returned `Err(NotFound("index"))` and a following
/// `index_info(age)` returned `NotFound` too -- the index was gone, not slow.
/// The build now commits that create (and only that create) before it starts,
/// so the registry row is durable before anything can roll back.
#[test]
fn a_build_started_on_an_uncommitted_create_keeps_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = limited_db(&dir.path().join("db"));
    let people = seed_people(&mut db);

    let age = db
        .create_scalar_index(people, "age_idx", "age", false)
        .unwrap();
    // NOTE: no db.commit() here -- this is the bench's exact sequence.

    let result = db.build_index_to_ready(age, 256);
    eprintln!("no intervening commit (create, then build): {result:?}");
    assert!(
        result.is_ok(),
        "a build started on its own uncommitted create must finish, got {result:?}"
    );
    assert!(
        db.index_info(age).is_ok(),
        "the index was lost by the build that was meant to create it"
    );
    assert_whole(&db, age);
}

/// The exception is exactly one create and nothing else. A handle carrying
/// other uncommitted work is refused with the sentence that fixes it, because
/// the build's own rollback would otherwise discard that work -- and the
/// refusal costs nothing: committing and asking again just works.
#[test]
fn a_build_refuses_a_handle_that_carries_other_uncommitted_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = limited_db(&dir.path().join("db"));
    let people = seed_people(&mut db);

    let age = db
        .create_scalar_index(people, "age_idx", "age", false)
        .unwrap();
    // One ordinary write after the create: the transaction is no longer just
    // this index's creation, so committing it is not the engine's call.
    db.put(
        people,
        "p999999",
        &json!({"age": 42, "parity": 1, "note": "harbour tide"}),
    )
    .unwrap();

    let refused = db.build_index_to_ready(age, 256).unwrap_err();
    let text = format!("{refused:?}");
    eprintln!("dirty handle: {text}");
    assert!(
        text.contains("commit pending writes before building an index"),
        "expected the refusal to say what to do, got {text}"
    );
    // Nothing was committed and nothing was discarded: the row is still
    // pending and the index still exists.
    db.commit().unwrap();
    assert!(db.index_info(age).is_ok());
    assert!(db.get(people, "p999999").unwrap().is_some());
    db.build_index_to_ready(age, 256).unwrap();
    assert_eq!(db.index_info(age).unwrap().state, IndexState::Ready);

}

/// Several indexes built back to back under ONE caller commit: the pattern
/// `tests/query_candidate_budget.rs` and any fixture that wants its indexes
/// published together writes. After the first build the handle is dirty with
/// that build's own READY flip, so a guard that asked only "is the handle
/// dirty" refused the second build. The engine's own catalog work is not the
/// caller's pending work, and is committed rather than refused.
#[test]
fn builds_run_back_to_back_share_one_caller_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = limited_db(&path);
    let people = seed_people(&mut db);

    let age = db
        .create_scalar_index(people, "age_idx", "age", false)
        .unwrap();
    db.build_index_to_ready(age, 256).unwrap();
    let parity = db
        .create_scalar_index(people, "parity_idx", "parity", false)
        .unwrap();
    db.build_index_to_ready(parity, 256).unwrap();
    // A third, of another family: `create_text_index` writes more than the
    // registry row, so this is the build that met a dirty handle in practice
    // (`tests/query_candidate_budget.rs:148`).
    let note = db.create_text_index(people, "note_text", "note").unwrap();
    db.build_index_to_ready(note, 256).unwrap();
    db.commit().unwrap();

    for id in [age, parity, note] {
        assert_eq!(db.index_info(id).unwrap().state, IndexState::Ready);
    }
    db.checkpoint().unwrap();
    drop(db);

    // Published, not merely present in the writer that made them.
    let reopened = Database::open(&path, cfg()).unwrap();
    assert_eq!(reopened.index_info(age).unwrap().state, IndexState::Ready);
    assert_eq!(reopened.index_info(parity).unwrap().state, IndexState::Ready);
    assert_eq!(reopened.index_info(note).unwrap().state, IndexState::Ready);
    assert_whole(&reopened, age);
    let evens = reopened
        .query_scalar(parity, ScalarPredicate::Eq(json!(0i64)), 65_536)
        .unwrap()
        .len() as u64;
    assert_eq!(evens, ROWS.div_ceil(2), "the second index does not answer");
}
