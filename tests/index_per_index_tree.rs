//! Per-index B-trees: a scalar or spatial index whose entries live in a tree
//! of its own instead of sharing the primary one.
//!
//! The properties here are about EQUIVALENCE and ADMISSION, not speed. An
//! index that moved to its own tree must answer every query exactly as the
//! shared-tree index does, before and after ordinary writes and a reopen; the
//! file must announce the new layout so a binary that predates it refuses the
//! whole database rather than misreading it; and a database that never got a
//! per-index tree must stay readable and writable exactly as it was.

use e4_prototype::{
    collections::{
        verification::{verify_indexed_source, VerificationLimits},
        CollectionOptions, Database, EntityId, Error, IndexId, IndexState, ScalarPredicate,
        SpatialCandidates,
    },
    pagewal::PageWalStore,
    spatial_math::{Bounds, Point},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::path::Path;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

const ROWS: u64 = 600;

/// Deterministic rows: every field is a closed form of the row index, so the
/// two layouts are built from byte-identical input.
fn person(i: u64) -> Value {
    json!({
        "age": (i * 37 % 211) as i64,
        "home": {"type":"Point","coordinates":[
            -179.0 + (i % 359) as f64,
            -80.0 + (i % 157) as f64,
        ]},
    })
}

struct Built {
    db: Database,
    collection: e4_prototype::collections::CollectionId,
    age: IndexId,
    home: IndexId,
}

/// Seed identical rows and build both indexes, with or without their own
/// trees. The layout is set on this handle, so both layouts are built by the
/// SAME binary in the SAME process -- the comparison is between two files,
/// not between two builds of the code.
fn build(path: &Path, own_trees: bool) -> Built {
    let mut db = Database::create(path, cfg()).unwrap();
    db.set_create_index_trees(own_trees);
    let collection = db
        .create_collection(
            "people",
            vec![("age".into(), Kind::Int), ("home".into(), Kind::Point)],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..ROWS {
        db.put(collection, &format!("p{i}"), &person(i)).unwrap();
    }
    db.commit().unwrap();
    let age = db
        .create_scalar_index(collection, "age_idx", "age", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(age, 256).unwrap();
    let home = db.create_point_index(collection, "home_idx", "home").unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(home, 256).unwrap();
    assert_eq!(
        db.index_tree(age).unwrap().is_some(),
        own_trees,
        "scalar index layout does not match what was asked for"
    );
    assert_eq!(db.index_tree(home).unwrap().is_some(), own_trees);
    Built {
        db,
        collection,
        age,
        home,
    }
}

/// Every answer the two layouts must agree on, as one comparable value.
fn answers(b: &Built) -> Vec<(String, String)> {
    let db = &b.db;
    let mut out = Vec::new();
    let mut push = |what: &str, v: String| out.push((what.to_string(), v));

    for value in [0i64, 37, 74, 210, 211] {
        push(
            &format!("eq {value}"),
            format!(
                "{:?}",
                db.query_scalar(
                    b.age,
                    ScalarPredicate::Eq(json!(value)),
                    65536,
                )
                .unwrap()
            ),
        );
    }
    for (lo, hi) in [(0i64, 20i64), (50, 120), (200, 210), (100, 100)] {
        push(
            &format!("range {lo}..{hi}"),
            format!(
                "{:?}",
                db.query_scalar(
                    b.age,
                    ScalarPredicate::Range {
                        lower: Some(json!(lo)),
                        upper: Some(json!(hi)),
                    },
                    65536,
                )
                .unwrap()
            ),
        );
    }
    // Top-k: the first entities the ordered scan yields, which is what a
    // limit-bounded range answers.
    for limit in [1usize, 5, 50] {
        push(
            &format!("topk {limit}"),
            format!(
                "{:?}",
                db.query_scalar(
                    b.age,
                    ScalarPredicate::Range {
                        lower: None,
                        upper: None,
                    },
                    limit,
                )
                .unwrap()
            ),
        );
    }
    for (w, s, e, n) in [
        (-179.0, -80.0, -100.0, 0.0),
        (0.0, 0.0, 90.0, 60.0),
        (-10.0, -10.0, 10.0, 10.0),
    ] {
        let bounds = Bounds::new(w, e, s, n).unwrap();
        push(
            &format!("bbox {w},{s},{e},{n}"),
            format!(
                "{:?}",
                db.query_point_bbox(b.home, bounds, 4096, SpatialCandidates::All, 1 << 20, || false)
                    .unwrap()
            ),
        );
    }
    for radius in [50_000.0f64, 500_000.0, 5_000_000.0] {
        push(
            &format!("radius {radius}"),
            format!(
                "{:?}",
                db.query_point_radius(
                    b.home,
                    Point::new(0.0, 0.0).unwrap(),
                    radius,
                    4096,
                    SpatialCandidates::All,
                    1 << 20,
                    || false,
                )
                .unwrap()
            ),
        );
    }
    for k in [1usize, 10, 100] {
        push(
            &format!("nearest {k}"),
            format!(
                "{:?}",
                db.query_point_nearest(
                    b.home,
                    Point::new(12.0, 34.0).unwrap(),
                    k,
                    SpatialCandidates::All,
                    1 << 20,
                    || false,
                )
                .unwrap()
            ),
        );
    }
    // Combined: a spatial answer restricted to the entities a scalar range
    // selected, which walks both index trees inside one query.
    let mut ids = db
        .query_scalar(
            b.age,
            ScalarPredicate::Range {
                lower: Some(json!(0i64)),
                upper: Some(json!(60i64)),
            },
            65536,
        )
        .unwrap();
    ids.sort();
    ids.dedup();
    push(
        "combined",
        format!(
            "{:?}",
            db.query_point_nearest(
                b.home,
                Point::new(0.0, 0.0).unwrap(),
                25,
                SpatialCandidates::SortedUnique(&ids),
                1 << 20,
                || false,
            )
            .unwrap()
        ),
    );
    out
}

fn mutate(b: &mut Built) {
    for i in (0..ROWS).step_by(7) {
        b.db.put(b.collection, &format!("p{i}"), &person(i + 1000))
            .unwrap();
    }
    for i in (3..ROWS).step_by(11) {
        b.db.delete(b.collection, &format!("p{i}")).unwrap();
    }
    for i in ROWS..ROWS + 120 {
        b.db.put(b.collection, &format!("q{i}"), &person(i * 3))
            .unwrap();
    }
    b.db.commit().unwrap();
}

/// (a) The same rows, the same questions, the same answers -- in both layouts,
/// before and after ordinary writes, and again after a reopen.
#[test]
fn a_per_index_tree_answers_every_query_exactly_as_the_shared_tree_does() {
    let temp = tempfile::tempdir().unwrap();
    let shared_path = temp.path().join("shared");
    let owned_path = temp.path().join("owned");
    let mut shared = build(&shared_path, false);
    let mut owned = build(&owned_path, true);

    assert_eq!(answers(&shared), answers(&owned), "fresh build disagrees");

    mutate(&mut shared);
    mutate(&mut owned);
    assert_eq!(
        answers(&shared),
        answers(&owned),
        "insert/update/delete disagrees"
    );

    drop(shared.db);
    drop(owned.db);
    shared.db = Database::open(&shared_path, cfg()).unwrap();
    owned.db = Database::open(&owned_path, cfg()).unwrap();
    assert_eq!(answers(&shared), answers(&owned), "reopen disagrees");

    // Both files pass their own verifier: equal answers from a damaged index
    // would prove nothing.
    for path in [&shared_path, &owned_path] {
        let db = std::mem::replace(
            if std::ptr::eq(path, &shared_path) {
                &mut shared.db
            } else {
                &mut owned.db
            },
            Database::create(temp.path().join(format!("scratch{:p}", path)), cfg()).unwrap(),
        );
        drop(db);
        let report =
            verify_indexed_source(path, VerificationLimits::default(), |_| {}).unwrap();
        assert!(report.complete && report.clean, "{path:?} is not clean");
    }
}

/// (b) Same entries, different tree. The pinned digests in
/// `tests/index_build_equivalence.rs` are the primary statement of this; here
/// it is asserted directly against a shared-tree build of the same rows.
#[test]
fn the_entry_set_is_identical_and_only_the_tree_differs() {
    let temp = tempfile::tempdir().unwrap();
    let shared = build(&temp.path().join("shared"), false);
    let owned = build(&temp.path().join("owned"), true);
    for (family_tag, shared_id, owned_id) in [
        (0x70u8, shared.age, owned.age),
        (0x74, shared.home, owned.home),
    ] {
        let mut a = Vec::new();
        let mut b = Vec::new();
        let prefix = [family_tag];
        shared
            .db
            .index_for_each(shared_id, &prefix, &mut |k, v| {
                a.push((k.to_vec(), v.to_vec()))
            })
            .unwrap();
        owned
            .db
            .index_for_each(owned_id, &prefix, &mut |k, v| {
                b.push((k.to_vec(), v.to_vec()))
            })
            .unwrap();
        assert!(!a.is_empty(), "tag {family_tag:#x} built nothing");
        assert_eq!(a, b, "tag {family_tag:#x} entry set moved");
    }
    assert_eq!(shared.db.index_tree(shared.age).unwrap(), None);
    assert!(owned.db.index_tree(owned.age).unwrap().unwrap().0 >= 2);
}

fn header_features(path: &Path) -> u64 {
    let raw = PageWalStore::open(path, false, 1 << 20).unwrap();
    let header = raw.get(&[0, 0, 0]).unwrap().unwrap();
    u64::from_be_bytes(header[10 + 8..10 + 16].try_into().unwrap())
}

fn files(path: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut names = std::fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let b = std::fs::read(path.join(&n)).unwrap();
            (n, b)
        })
        .collect()
}

fn reseal(bytes: &mut [u8]) {
    let n = bytes.len();
    let crc = crc32c::crc32c(&bytes[..n - 4]).to_le_bytes();
    bytes[n - 4..].copy_from_slice(&crc);
}

/// (c) Admission. The new bit sits outside the mask every earlier binary
/// implements, so such a binary refuses the whole database before touching a
/// byte of it; a database that never got a per-index tree never declares the
/// bit and stays fully writable.
#[test]
fn the_per_index_tree_bit_is_declared_only_when_a_tree_was_created() {
    const INDEX_TREES: u64 = 0x80;
    /// Everything the release before this one implemented.
    const OLD_MASK: u64 = 0x7f;
    assert_eq!(
        INDEX_TREES & OLD_MASK,
        0,
        "the per-index-tree bit must be outside the older supported mask, or an \
         older binary would open a file it cannot read"
    );

    let temp = tempfile::tempdir().unwrap();
    let shared_path = temp.path().join("shared");
    let owned_path = temp.path().join("owned");
    let shared = build(&shared_path, false);
    let owned = build(&owned_path, true);
    let collection = shared.collection;
    drop(shared.db);
    drop(owned.db);

    assert_eq!(
        header_features(&shared_path) & INDEX_TREES,
        0,
        "a shared-tree database declared the per-index-tree bit"
    );
    assert_ne!(
        header_features(&owned_path) & INDEX_TREES,
        0,
        "a per-index-tree database did not declare its bit"
    );

    // Law 8: the shared-tree file this binary can also create is still a
    // writable, queryable database, and opening it does not adopt the new bit.
    let mut db = Database::open(&shared_path, cfg()).unwrap();
    db.put(collection, "late", &person(9)).unwrap();
    db.commit().unwrap();
    assert!(db.get(collection, "late").unwrap().is_some());
    drop(db);
    assert_eq!(header_features(&shared_path) & INDEX_TREES, 0);
}

#[test]
fn clearing_the_per_index_tree_bit_is_refused_without_changing_a_byte() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("owned");
    drop(build(&path, true).db);

    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    for copy in 0..3u8 {
        let key = [0, 0, copy];
        let mut header = raw.get(&key).unwrap().unwrap();
        let features = u64::from_be_bytes(header[10 + 8..10 + 16].try_into().unwrap());
        header[10 + 8..10 + 16].copy_from_slice(&(features & !0x80u64).to_be_bytes());
        reseal(&mut header);
        raw.put(&key, &header).unwrap();
    }
    raw.commit().unwrap();
    drop(raw);

    let before = files(&path);
    for snapshot in [false, true] {
        let result = if snapshot {
            Database::open_snapshot(&path, cfg())
        } else {
            Database::open(&path, cfg())
        };
        assert!(
            matches!(result, Err(Error::Corrupt(_)) | Err(Error::Unsupported(_))),
            "a descriptor with a tree was admitted without the feature bit"
        );
        assert_eq!(files(&path), before, "refusal changed bytes");
    }
}

/// (d) Crash consistency. The tree's pages and the descriptor that names its
/// root are frames of ONE commit, so there is no reachable state in which they
/// disagree: an uncommitted build reverts to the last committed root, and the
/// READY flip is not visible until its own commit is.
#[test]
fn a_build_that_never_commits_leaves_the_last_committed_root() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    db.set_create_index_trees(true);
    let collection = db
        .create_collection(
            "people",
            vec![("age".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..200u64 {
        db.put(collection, &format!("p{i}"), &person(i)).unwrap();
    }
    db.commit().unwrap();
    let age = db
        .create_scalar_index(collection, "age_idx", "age", false)
        .unwrap();
    db.commit().unwrap();
    assert_eq!(db.index_tree(age).unwrap().unwrap().1, 0, "fresh tree is empty");

    // Entries and the descriptor's new root are written, and then the whole
    // transaction is discarded exactly as a crash would discard it.
    assert!(!db.build_index_step(age, 64).unwrap());
    assert_ne!(db.index_tree(age).unwrap().unwrap().1, 0);
    db.rollback().unwrap();
    assert_eq!(
        db.index_tree(age).unwrap().unwrap().1,
        0,
        "an uncommitted build left a committed root behind"
    );
    assert!(matches!(
        db.index_info(age).unwrap().state,
        IndexState::Building { after: 0 }
    ));

    // A build stopped after committed groups: BUILDING, with a root that is
    // the one those groups published.
    // One committed insert group, then stop: the group is 64 chunks, so a
    // chunk of one row reaches the cap well inside 200 rows.
    db.build_index_to_ready_capped(age, 1, Some(1)).unwrap();
    let mid = db.index_tree(age).unwrap().unwrap().1;
    assert!(matches!(
        db.index_info(age).unwrap().state,
        IndexState::Building { .. }
    ));
    drop(db);

    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(
        db.index_tree(age).unwrap().unwrap().1,
        mid,
        "reopen after a kill saw a different root"
    );
    assert!(matches!(
        db.index_info(age).unwrap().state,
        IndexState::Building { .. }
    ));
    db.build_index_to_ready(age, 64).unwrap();
    assert_eq!(db.index_info(age).unwrap().state, IndexState::Ready);
    let ready_root = db.index_tree(age).unwrap().unwrap().1;
    assert_ne!(ready_root, 0);
    drop(db);

    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.index_info(age).unwrap().state, IndexState::Ready);
    assert_eq!(db.index_tree(age).unwrap().unwrap().1, ready_root);
    assert_eq!(
        db.query_scalar(age, ScalarPredicate::Eq(person(7)["age"].clone()), 64)
            .unwrap()
            .len(),
        1
    );
    drop(db);
    let report = verify_indexed_source(&path, VerificationLimits::default(), |_| {}).unwrap();
    assert!(report.complete && report.clean);
}

/// (e) The verifier reads the index's own tree, so damage there is damage it
/// reports -- and a descriptor root that does not name a page of that tree is
/// refused rather than read as an empty index.
#[test]
fn the_verifier_catches_a_damaged_index_tree_and_a_wrong_root() {
    let temp = tempfile::tempdir().unwrap();

    let damaged = temp.path().join("damaged");
    let built = build(&damaged, true);
    let (_, root) = built.db.index_tree(built.age).unwrap().unwrap();
    drop(built.db);
    {
        // Fold the WAL so the tree's pages are in `data`, then flip a byte of
        // the index tree's root page.
        let mut raw = PageWalStore::open(&damaged, false, 1 << 20).unwrap();
        raw.checkpoint().unwrap();
        drop(raw);
        let file = damaged.join("data");
        let mut bytes = std::fs::read(&file).unwrap();
        let at = root as usize * kernel::page::PAGE_SIZE + 64;
        bytes[at] ^= 0xff;
        std::fs::write(&file, bytes).unwrap();
    }
    let outcome = verify_indexed_source(&damaged, VerificationLimits::default(), |_| {});
    match outcome {
        Err(_) => {}
        Ok(report) => assert!(
            !report.clean || !report.complete,
            "a flipped index-tree page verified clean"
        ),
    }

    let wrong = temp.path().join("wrong-root");
    let built = build(&wrong, true);
    let age = built.age;
    let (tree_id, root) = built.db.index_tree(age).unwrap().unwrap();
    drop(built.db);
    {
        let mut raw = PageWalStore::open(&wrong, false, 1 << 20).unwrap();
        assert_eq!(age.0, 1, "descriptor key below assumes the first index id");
        for copy in 0..3u8 {
            // dkey(id, copy): the DESCRIPTOR tag, the replica number, then the
            // order-preserving identity, which for id 1 is [0x81, 1].
            let key = [3u8, copy, 0x81, 1];
            let mut descriptor = raw.get(&key).unwrap().unwrap();
            // The root is the last four bytes of the descriptor's fixed part,
            // which the strings follow; rewrite it to the PRIMARY tree's root,
            // which is a real page of a different tree.
            let root_at = root_offset(&descriptor, tree_id, root);
            descriptor[root_at..root_at + 4].copy_from_slice(&1u32.to_be_bytes());
            reseal(&mut descriptor);
            raw.put(&key, &descriptor).unwrap();
        }
        raw.commit().unwrap();
    }
    let outcome = verify_indexed_source(&wrong, VerificationLimits::default(), |_| {});
    match outcome {
        Err(_) => {}
        Ok(report) => assert!(
            !report.clean || !report.complete,
            "a descriptor root pointing outside its tree verified clean"
        ),
    }
}

/// Where the descriptor keeps this index's `tree_id:u16be | root:u32be`, found
/// by the pair itself rather than by re-deriving the family's byte layout.
fn root_offset(descriptor: &[u8], tree_id: u16, root: u32) -> usize {
    let mut needle = tree_id.to_be_bytes().to_vec();
    needle.extend(root.to_be_bytes());
    let at = descriptor
        .windows(6)
        .position(|w| w == needle)
        .expect("descriptor does not carry its own tree id and root");
    at + 2
}

/// (f) Dropping an index returns its tree's pages to the freelist: building
/// the same index again must not grow the file.
#[test]
fn dropping_an_index_returns_its_trees_pages() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    db.set_create_index_trees(true);
    let collection = db
        .create_collection(
            "people",
            vec![("age".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..ROWS {
        db.put(collection, &format!("p{i}"), &person(i)).unwrap();
    }
    db.commit().unwrap();

    // The data extent, read after folding the WAL: a reclaimed page is one the
    // allocator hands back instead of growing the file.
    fn pages(db: &mut Database, path: &Path) -> u32 {
        db.commit().unwrap();
        db.checkpoint().unwrap();
        let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
        raw.page_count()
    }
    let baseline = pages(&mut db, &path);

    let mut after_first = 0;
    for round in 0..3 {
        let id = db
            .create_scalar_index(collection, "age_idx", "age", false)
            .unwrap();
        db.commit().unwrap();
        db.build_index_to_ready(id, 256).unwrap();
        let grown = pages(&mut db, &path);
        assert!(grown > baseline, "the index occupied no pages");
        if round == 0 {
            after_first = grown;
        } else {
            assert_eq!(
                grown, after_first,
                "rebuilding after a drop grew the file: the dropped tree's \
                 pages were not reclaimed"
            );
        }
        db.begin_drop_index(id).unwrap();
        db.commit().unwrap();
        while !db.drop_index_step(id, 256).unwrap() {
            db.commit().unwrap();
        }
        db.commit().unwrap();
    }
}

/// (g) Cost. A packed build is page-linear: it descends nothing, so it costs
/// far fewer pool accesses than the per-key insert the shared tree needs. A
/// live insert into a dense single-family tree is cheaper too.
#[test]
fn a_packed_build_and_a_live_insert_cost_fewer_pool_accesses() {
    let temp = tempfile::tempdir().unwrap();
    let mut measured = Vec::new();
    for own in [false, true] {
        let path = temp.path().join(if own { "owned" } else { "shared" });
        let mut db = Database::create(&path, cfg()).unwrap();
        db.set_create_index_trees(own);
        let collection = db
            .create_collection(
                "people",
                vec![("age".into(), Kind::Int)],
                CollectionOptions::default(),
            )
            .unwrap();
        for i in 0..ROWS {
            db.put(collection, &format!("p{i}"), &person(i)).unwrap();
        }
        db.commit().unwrap();

        let id = db
            .create_scalar_index(collection, "age_idx", "age", false)
            .unwrap();
        db.commit().unwrap();
        let before = db.pool_accesses().unwrap();
        db.build_index_to_ready(id, 256).unwrap();
        let build = db.pool_accesses().unwrap() - before;

        let before = db.pool_accesses().unwrap();
        for i in ROWS..ROWS + 200 {
            db.put(collection, &format!("q{i}"), &person(i * 7)).unwrap();
        }
        db.commit().unwrap();
        let insert = db.pool_accesses().unwrap() - before;
        measured.push((own, build, insert));
    }
    let (_, shared_build, shared_insert) = measured[0];
    let (_, owned_build, owned_insert) = measured[1];
    eprintln!(
        "COST rows={ROWS} build shared={shared_build} owned={owned_build} \
         insert shared={shared_insert} owned={owned_insert}"
    );
    assert!(
        owned_build * 2 < shared_build,
        "a packed build cost {owned_build} accesses against {shared_build} for \
         the shared tree: the pack is not page-linear"
    );
    // MEASURED, not assumed: live inserts do NOT get cheaper here. Each write
    // now descends a second tree instead of reaching its index entry through
    // pages the row write already brought in, and the descriptor is rewritten
    // whenever the index tree grows a level. At this size that is a ~2% cost,
    // against a build that is 8x cheaper. The bound keeps it from drifting.
    assert!(
        owned_insert * 100 <= shared_insert * 105,
        "a live insert into a per-index tree cost {owned_insert} accesses \
         against {shared_insert} for the shared tree"
    );
}

/// The queries above must also work through the prepared-query drivers, which
/// walk the index cursors rather than the direct scalar/spatial entry points.
#[test]
fn prepared_query_drivers_read_the_index_tree() {
    let temp = tempfile::tempdir().unwrap();
    let shared = build(&temp.path().join("shared"), false);
    let owned = build(&temp.path().join("owned"), true);
    for b in [&shared, &owned] {
        let ids = b
            .db
            .query_scalar(
                b.age,
                ScalarPredicate::Range {
                    lower: Some(json!(10i64)),
                    upper: Some(json!(40i64)),
                },
                65536,
            )
            .unwrap();
        assert!(!ids.is_empty());
    }
    let a: Vec<EntityId> = shared
        .db
        .query_scalar(
            shared.age,
            ScalarPredicate::Range {
                lower: Some(json!(10i64)),
                upper: Some(json!(40i64)),
            },
            65536,
        )
        .unwrap();
    let b: Vec<EntityId> = owned
        .db
        .query_scalar(
            owned.age,
            ScalarPredicate::Range {
                lower: Some(json!(10i64)),
                upper: Some(json!(40i64)),
            },
            65536,
        )
        .unwrap();
    assert_eq!(a, b);
}

/// (h) The layout an UNCONFIGURED handle creates.
///
/// Every test above sets the per-index-tree switch itself, so all of them keep
/// passing whatever the default is. The default is not a private detail: a
/// tool that never touches the switch -- the Phase 2 lifecycle compatibility
/// fixture is one -- writes whatever it produces into a corpus, declares the
/// resulting feature mask in its manifest, and the Linux qualification driver
/// computes its family table and its mask-31 admission gate over that number.
///
/// When loop 4 turned per-index trees on by default it moved all three of
/// those numbers at once -- a scalar index's header mask from 1 to 129, a
/// spatial one's from 9 to 137, and both descriptors from version 1 to 2 --
/// and no test said so, because no test asked what an unconfigured handle
/// creates. Stage 4 of the final qualification found it instead.
///
/// So this pins the published contract, through a build long enough to split
/// the index's tree and move its root more than once, across the commit and
/// checkpoint and reopen that a fixture performs: what the catalog reports,
/// what the header declares, and that the entries really are in the index's
/// own tree with none left behind in the primary one.
#[test]
fn an_unconfigured_handle_creates_the_layout_the_compat_corpus_declares() {
    /// The mask a scalar-only collection must require of a reader: the base
    /// logical bit plus the per-index-tree bit.
    const SCALAR_REQUIRES: u64 = 1 | 0x80;
    /// The same for a collection that also carries a spatial index.
    const SCALAR_AND_SPATIAL_REQUIRE: u64 = 1 | 8 | 0x80;

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("default");

    // No set_create_index_trees here, deliberately: this is the handle an
    // ordinary tool gets.
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "people",
            vec![("age".into(), Kind::Int), ("home".into(), Kind::Point)],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..ROWS {
        db.put(collection, &format!("p{i}"), &person(i)).unwrap();
    }
    db.commit().unwrap();
    let age = db
        .create_scalar_index(collection, "age_idx", "age", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(age, 256).unwrap();
    db.commit().unwrap();

    // One index, one catalog entry, and the catalog agrees with itself.
    let before = db.list_indexes(collection).unwrap();
    assert_eq!(before.len(), 1, "one created index is one catalog entry");
    assert_eq!(db.index_info(age).unwrap(), before[0]);
    assert_eq!(before[0].encoding_version, 2);
    assert_eq!(before[0].state, IndexState::Ready);
    let tree = db.index_tree(age).unwrap().expect("an owned tree");
    assert!(tree.0 >= 2, "tree ids 0 and 1 are reserved");
    assert_ne!(tree.1, 0, "a built index's tree has a root");

    // 600 rows do not fit one leaf, so the build moved the root and rewrote
    // the descriptor at least twice. Take the entries as the oracle.
    let mut entries = Vec::new();
    db.index_for_each(age, &[0x70], &mut |k, v| {
        entries.push((k.to_vec(), v.to_vec()))
    })
    .unwrap();
    assert_eq!(entries.len() as u64, ROWS);

    assert!(db.checkpoint().unwrap());
    drop(db);
    assert_eq!(header_features(&path), SCALAR_REQUIRES);

    let mut db = Database::open(&path, cfg()).unwrap();
    let after = db.list_indexes(collection).unwrap();
    assert_eq!(after, before, "reopening changed the catalog");
    assert_eq!(db.index_info(age).unwrap(), after[0]);
    assert_eq!(db.index_tree(age).unwrap(), Some(tree));
    let mut reopened = Vec::new();
    db.index_for_each(age, &[0x70], &mut |k, v| {
        reopened.push((k.to_vec(), v.to_vec()))
    })
    .unwrap();
    assert_eq!(reopened, entries, "the root the descriptor kept is the wrong one");

    // A second owned-tree family on the same collection adds its own bit and
    // nothing else; the mask is the sum of what the file actually contains.
    let home = db.create_point_index(collection, "home_idx", "home").unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(home, 256).unwrap();
    db.commit().unwrap();
    assert_eq!(db.index_info(home).unwrap().encoding_version, 2);
    assert_eq!(db.list_indexes(collection).unwrap().len(), 2);
    assert!(db.checkpoint().unwrap());
    drop(db);
    assert_eq!(header_features(&path), SCALAR_AND_SPATIAL_REQUIRE);

    // An external inventory -- which is all the fixture is -- must look in the
    // index's tree. Nothing of either family is left in the primary tree, so a
    // tool that scans only the primary tree sees an empty index, not a partial
    // one.
    let raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    for tag in [0x70u8, 0x74] {
        assert!(
            raw.range(&[tag])
                .unwrap()
                .next()
                .is_none_or(|row| row.unwrap().0[0] != tag),
            "tag {tag:#x} entries were left in the primary tree"
        );
    }
    let (id, root) = tree;
    let mut in_own_tree = 0u64;
    for row in raw.tree_range(id, root, &[0x70]).unwrap().unwrap() {
        if row.unwrap().0[0] != 0x70 {
            break;
        }
        in_own_tree += 1;
    }
    assert_eq!(in_own_tree, ROWS, "the index's own tree lost entries");
}
