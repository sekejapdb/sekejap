//! The late-build oracle.
//!
//! A late index build is allowed to get cheaper. It is not allowed to persist
//! different bytes. This file is the guard for that: it seeds a fixed
//! people + organizations corpus, builds one index of every family that has a
//! late-build path, and folds every persisted key and value under that index's
//! key prefix into a SHA-256 digest.
//!
//! The digests are pinned constants. They were produced by the **unchanged**
//! builder on branch `pagewal-foundation` at working tree `a69838c` + the
//! Phase 2 tree, before the scalar sort, the text chunk accumulator and the
//! bounded-commit atomic policy were written. If a digest below moves, the
//! change altered persisted index content and is refused, whatever it did to
//! the clock.
//!
//! SHA-256 is implemented here from FIPS 180-4 rather than taken from the
//! engine: an oracle that shares code with the thing it checks is not one.

use e4_prototype::{
    collections::{CollectionOptions, Database, EntityId, IndexId, IndexState, ScalarPredicate},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};

// ---------------------------------------------------------------- SHA-256

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    n: usize,
    len: u64,
}

impl Sha256 {
    fn new() -> Self {
        Sha256 {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0; 64],
            n: 0,
            len: 0,
        }
    }
    fn block(&mut self) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(self.buf[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = self.h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for i in 0..8 {
            self.h[i] = self.h[i].wrapping_add(v[i]);
        }
    }
    fn update(&mut self, mut b: &[u8]) {
        self.len += b.len() as u64;
        while !b.is_empty() {
            let take = (64 - self.n).min(b.len());
            self.buf[self.n..self.n + take].copy_from_slice(&b[..take]);
            self.n += take;
            b = &b[take..];
            if self.n == 64 {
                self.block();
                self.n = 0;
            }
        }
    }
    fn hex(mut self) -> String {
        let bits = self.len * 8;
        self.update(&[0x80]);
        while self.n != 56 {
            self.update(&[0]);
        }
        self.len = 0;
        self.update(&bits.to_be_bytes());
        self.h.iter().map(|w| format!("{w:08x}")).collect()
    }
}

// ------------------------------------------------- key prefixes (from the spec)

/// `PHASE2_INDEX_FORMAT.md`: ordered integers are `0x80 + width` then the
/// minimal unsigned big-endian bytes. Written out here so the oracle does not
/// borrow the engine's codec.
fn ordered(n: u64) -> Vec<u8> {
    let b = n.to_be_bytes();
    let start = b.iter().position(|x| *x != 0).unwrap_or(7);
    let mut k = vec![0x80 + (8 - start) as u8];
    k.extend_from_slice(&b[start..]);
    k
}

fn index_prefix(tag: u8, id: IndexId) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend(ordered(id.0));
    k
}

/// Every persisted tag a family owns. 0x70 scalar posting, 0x74 spatial
/// posting, 0x75/0x76/0x77/0x78 text posting / norm / term stats / corpus,
/// 0x7a the packed posting segment the sorted builder writes instead of the
/// per-document head rows.
fn family_tags(family: &str) -> &'static [u8] {
    match family {
        "scalar" => &[0x70],
        "spatial" => &[0x74],
        "text" => &[0x75, 0x76, 0x77, 0x78, 0x7a, 0x7b],
        _ => unreachable!(),
    }
}

/// Fold every key and value the index owns, in key order, into one digest.
/// Lengths are folded too, so no pair of entries can be re-split silently.
fn digest(db: &Database, family: &str, id: IndexId) -> (String, u64) {
    let mut h = Sha256::new();
    let mut entries = 0;
    for tag in family_tags(family) {
        let p = index_prefix(*tag, id);
        entries += db
            .index_for_each(id, &p, &mut |k, v| {
                h.update(&(k.len() as u64).to_be_bytes());
                h.update(k);
                h.update(&(v.len() as u64).to_be_bytes());
                h.update(v);
            })
            .unwrap();
    }
    (h.hex(), entries)
}

// ------------------------------------------------------------------ fixture

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

const PEOPLE: u64 = 2_000;
const ORGS: u64 = 1_000;

/// Deterministic, no randomness: every value is a closed form of the row index
/// so the digests below are reproducible on any machine.
fn person(i: u64) -> Value {
    let words = [
        "flood", "levee", "river", "bridge", "survey", "harbour", "silt", "canal", "tide", "rail",
    ];
    let mut bio = String::new();
    for j in 0..(3 + i % 9) {
        if j > 0 {
            bio.push(' ');
        }
        bio.push_str(words[((i * 7 + j * 3) % 10) as usize]);
    }
    json!({
        "name": format!("person-{i:05}"),
        "age": ((i * 37) % 95) as i64,
        "active": i % 3 == 0,
        "bio": bio,
        "home": {"type":"Point","coordinates":[
            -180.0 + ((i * 131) % 3600) as f64 / 10.0,
            -85.0 + ((i * 97) % 1700) as f64 / 10.0]},
    })
}

fn org(i: u64) -> Value {
    json!({"title": format!("org-{i:04}"), "staff": ((i * 13) % 500) as i64})
}

struct Seeded {
    people: e4_prototype::collections::CollectionId,
}

fn seed(path: &std::path::Path) -> (Database, Seeded) {
    seed_with(path, None, 256)
}

/// The same corpus, optionally under a resource policy and with a chosen
/// commit cadence. Neither changes a single persisted index byte -- the rows
/// are identical and the digests below are content, not layout -- so a test
/// that needs a small `wal_bytes` still checks against the same pins.
fn seed_with(
    path: &std::path::Path,
    limits: Option<kernel::limits::ResourceLimits>,
    commit_every: u64,
) -> (Database, Seeded) {
    let mut db = match limits {
        None => Database::create(path, cfg()).unwrap(),
        Some(l) => Database::create_limited(path, cfg(), l).unwrap(),
    };
    let people = db
        .create_collection(
            "people",
            vec![
                ("name".into(), Kind::Text),
                ("age".into(), Kind::Int),
                ("active".into(), Kind::Bool),
                ("bio".into(), Kind::Text),
                ("home".into(), Kind::Point),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let orgs = db
        .create_collection(
            "organizations",
            vec![("title".into(), Kind::Text), ("staff".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..PEOPLE {
        db.put(people, &format!("p{i:05}"), &person(i)).unwrap();
        if i % commit_every == commit_every - 1 {
            db.commit().unwrap();
        }
    }
    for i in 0..ORGS {
        db.put(orgs, &format!("o{i:04}"), &org(i)).unwrap();
        if i % commit_every == commit_every - 1 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    (db, Seeded { people })
}

fn build(db: &mut Database, id: IndexId) -> u64 {
    let before = db.pool_accesses().unwrap();
    while !db.build_index_step(id, 256).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    db.pool_accesses().unwrap() - before
}

fn build_sorted(db: &mut Database, id: IndexId) -> u64 {
    let before = db.pool_accesses().unwrap();
    db.build_index_to_ready(id, 256).unwrap();
    db.pool_accesses().unwrap() - before
}

/// Pinned by a run of the unchanged builder; see the module note.
const SCALAR_AGE: &str = "6b7205dc65d3fc47285914b3e86cc0d8224722aa478d05a561c650e0b0bbdb63";
const SCALAR_ACTIVE: &str = "5349a3713fc8216b9d04df511d8397fe09b5669bcbd8e78bfbc2a3c043770d09";
/// The stepped builder (`build_index_step`) still writes the head tier, one
/// entry per `(term, document)`, and its digest is unchanged.
const TEXT_BIO: &str = "e1b15c14f345ba66dc8a2cd0e954e38de8200ecd7e8f73c2adb0d15904475dc1";
/// The sorted builder packs the same postings into per-term segments, so the
/// persisted SET is different by design and needs its own pin. The two are
/// proved to answer identically in `tests/index_text.rs`; this constant only
/// pins that the packed bytes do not drift.
///
/// Repinned when norms moved from one `0x76` row per document to one `0x7B`
/// block per 256 documents. The stepped builder's `TEXT_BIO` above is
/// deliberately untouched: the head tier's bytes did not change.
const TEXT_BIO_SEGMENTS: &str = "481deca7533220ec312574b03d400d334c21d0d830c923802132cd56f0fd8fef";
const SPATIAL_HOME: &str = "195919c4b0c7dfdbbcad2c117a6a3d861135d374b45445f72210554b1b19c863";

#[test]
fn a_late_build_persists_the_same_bytes_however_it_is_scheduled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, s) = seed(&path);

    let age = db
        .create_scalar_index(s.people, "age_idx", "age", false)
        .unwrap();
    db.commit().unwrap();
    let age_accesses = build(&mut db, age);

    let active = db
        .create_scalar_index(s.people, "active_idx", "active", false)
        .unwrap();
    db.commit().unwrap();
    let active_accesses = build(&mut db, active);

    let bio = db.create_text_index(s.people, "bio_idx", "bio").unwrap();
    db.commit().unwrap();
    let text_accesses = build(&mut db, bio);

    let home = db.create_point_index(s.people, "home_idx", "home").unwrap();
    db.commit().unwrap();
    let spatial_accesses = build(&mut db, home);

    let found = [
        ("scalar age", digest(&db, "scalar", age), SCALAR_AGE),
        (
            "scalar active",
            digest(&db, "scalar", active),
            SCALAR_ACTIVE,
        ),
        ("text bio", digest(&db, "text", bio), TEXT_BIO),
        ("spatial home", digest(&db, "spatial", home), SPATIAL_HOME),
    ];
    eprintln!(
        "ORACLE rows={PEOPLE} accesses/row age={:.3} active={:.3} text={:.3} spatial={:.3}",
        age_accesses as f64 / PEOPLE as f64,
        active_accesses as f64 / PEOPLE as f64,
        text_accesses as f64 / PEOPLE as f64,
        spatial_accesses as f64 / PEOPLE as f64,
    );
    for (what, (hex, entries), _) in &found {
        eprintln!("ORACLE {what}: {entries} entries {hex}");
        assert!(*entries > 0, "{what} built nothing");
    }
    for (what, (hex, _), pinned) in &found {
        assert_eq!(hex, pinned, "{what} index content moved");
    }

    // The same bytes must still be there after the handle is gone: a build that
    // only agrees inside its own writer proves nothing about what was published.
    db.checkpoint().unwrap();
    drop(db);
    let reopened = Database::open(&path, cfg()).unwrap();
    for (what, (hex, entries), _) in &found {
        let (again, n) = digest(
            &reopened,
            match *what {
                "text bio" => "text",
                "spatial home" => "spatial",
                _ => "scalar",
            },
            match *what {
                "scalar age" => age,
                "scalar active" => active,
                "text bio" => bio,
                _ => home,
            },
        );
        assert_eq!((&again, n), (hex, *entries), "{what} changed across reopen");
    }

    // And the index must answer, not merely exist.
    let hits = reopened
        .query_scalar(age, ScalarPredicate::Eq(json!(0i64)), 65_536)
        .unwrap();
    let expect: Vec<EntityId> = (0..PEOPLE)
        .filter(|i| (i * 37) % 95 == 0)
        .map(|i| EntityId {
            collection: s.people,
            sequence: i + 1,
        })
        .collect();
    assert_eq!(hits, expect);
}

/// The oracle checks the engine, so something has to check the oracle. FIPS
/// 180-4 test vectors plus a multi-block message.
#[test]
fn the_oracles_own_digest_is_sha256() {
    assert_eq!(
        Sha256::new().hex(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    let mut h = Sha256::new();
    h.update(b"abc");
    assert_eq!(
        h.hex(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let mut h = Sha256::new();
    for _ in 0..1000 {
        h.update(b"a");
    }
    assert_eq!(
        h.hex(),
        "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
    );
}

// --------------------------------------------- the publication policy (fix C)

/// "Atomic" was implemented as one transaction, and one transaction is bounded
/// by the page-WAL. A build big enough to matter is therefore not slow, it is
/// REFUSED. This pins both halves: the old shape fails on a small allowance,
/// and the engine's own driver finishes the same build on the same allowance.
#[test]
fn an_atomic_late_build_is_bounded_by_chunks_not_by_the_whole_index() {
    use kernel::limits::ResourceLimits;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let limits = ResourceLimits {
        data_bytes: 32 << 20,
        wal_bytes: 256 << 10,
        tracked_pages: 4096,
        readers: 4,
        record_bytes: 16384,
        recovery_bytes: 256 << 10,
    };
    let mut db = Database::create_limited(&path, cfg(), limits).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("bio".into(), Kind::Text), ("age".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    const ROWS: u64 = 16_000;
    for i in 0..ROWS {
        let doc = person(i);
        db.put(
            people,
            &format!("p{i:05}"),
            &json!({"bio": doc["bio"], "age": doc["age"]}),
        )
        .unwrap();
        if i % 128 == 127 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();

    // RED: the old policy, one transaction for the whole build.
    let first = db.create_text_index(people, "bio_idx", "bio").unwrap();
    db.commit().unwrap();
    let refused = loop {
        match db.build_index_step(first, 256) {
            Ok(true) => break None,
            Ok(false) => {}
            Err(e) => break Some(e),
        }
    };
    let refused = refused.unwrap_or_else(|| {
        panic!(
            "a whole-index transaction should exhaust the WAL allowance; it used {:?}",
            db.storage_bytes()
        )
    });
    assert!(
        format!("{refused:?}").contains("ResourceLimit"),
        "expected a WAL allowance refusal, got {refused:?}"
    );
    db.rollback().unwrap();

    // GREEN: the same build, same allowance, through the engine's driver.
    let index = db.create_text_index(people, "bio_idx2", "bio").unwrap();
    db.commit().unwrap();
    // The driver groups chunks into one transaction to save FULL barriers, and
    // a group of 16 does not fit this allowance either: finishing therefore
    // also proves the group shrank itself and carried on rather than failing.
    let chunks = db.build_index_to_ready(index, 256).unwrap();
    assert_eq!(chunks, ROWS.div_ceil(256) as usize);
    let hits = db
        .query_text(
            index,
            "flood",
            e4_prototype::collections::TextMatch::Any,
            5,
            e4_prototype::collections::TextCandidates::All,
            usize::MAX,
            || false,
        )
        .unwrap();
    assert!(!hits.is_empty(), "the finished index must answer");
}

/// Bounded chunk commits must not publish a half-built index. Visibility is the
/// descriptor's state, and a snapshot is byte-stable for its whole life.
#[test]
fn chunk_commits_publish_nothing_until_the_ready_flip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, s) = seed(&path);
    let age = db
        .create_scalar_index(s.people, "age_idx", "age", false)
        .unwrap();
    db.commit().unwrap();

    // Opened while the descriptor says BUILDING.
    let before = Database::open_snapshot(&path, cfg()).unwrap();
    assert!(before
        .query_scalar(age, ScalarPredicate::Eq(json!(0i64)), 16)
        .is_err());

    db.build_index_to_ready(age, 256).unwrap();

    // Still refused: this reader's generation never had a READY descriptor.
    assert!(before
        .query_scalar(age, ScalarPredicate::Eq(json!(0i64)), 16)
        .is_err());
    drop(before);

    let after = Database::open_snapshot(&path, cfg()).unwrap();
    let hits = after
        .query_scalar(age, ScalarPredicate::Eq(json!(0i64)), 65_536)
        .unwrap();
    assert_eq!(
        hits.len(),
        (0..PEOPLE).filter(|i| (i * 37) % 95 == 0).count()
    );
}

// ------------------------------------------- the cost property (fixes A and B)

/// THE COST PROPERTY for a late scalar build: a row's index work must not
/// include the vector lanes it never looks at, and a chunk's inserts must not
/// each be a blind descent into the middle of the scalar key space.
///
/// Page accesses, not seconds: they are exact and they do not move with the
/// machine (`sharing_a_tree_must_not_multiply_an_ascending_runs_page_work` in
/// kernel/src/btree.rs measures the same quantity for the same reason).
///
/// Measured on this fixture, 2,000 rows carrying a 64-lane vector:
///   builder before fix A    6.689 accesses per indexed row
///   builder after  fix A    3.349
/// Most of the difference is the sidecar `get` the scalar key never needed;
/// the rest is the chunk sort. The bound sits between the two numbers.
#[test]
fn a_scalar_late_build_does_not_pay_for_the_lanes_it_never_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![
                ("age".into(), Kind::Int),
                ("embed".into(), Kind::Vector(64)),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..PEOPLE {
        let lanes: Vec<f32> = (0..64).map(|j| (i as f32 + j as f32) / 64.0).collect();
        db.put(
            people,
            &format!("p{i:05}"),
            &json!({"age": ((i * 37) % 95) as i64, "embed": lanes}),
        )
        .unwrap();
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();

    let age = db
        .create_scalar_index(people, "age_idx", "age", false)
        .unwrap();
    db.commit().unwrap();
    let accesses = build_sorted(&mut db, age) as f64 / PEOPLE as f64;
    eprintln!("COST scalar build {accesses:.3} page accesses per indexed row");
    assert!(
        accesses <= 3.2,
        "a scalar build costs {accesses:.3} page accesses per row, against 3.349 after chunk sort and 6.689 before fix A"
    );
}

/// THE COST PROPERTY for a late text build. Document-at-a-time reads and
/// rewrites the one shared corpus row once per document and each term's
/// document frequency once per document per term; a chunk accumulator pays each
/// once per chunk and writes every key in ascending order.
///
/// Measured on the oracle fixture, 2,000 documents:
///   builder before fix B  106.608 accesses per indexed row
///   builder after  fix B   52.341
/// (the same build before both fixes, without the analysis carried out of the
/// scan loop, measured 109.591)
/// Retained deliberately: the per-posting absence probe, which is the Law 5
/// check that a document with no norm owns no posting. Dropping it measured
/// 31.606 -- a further 1.65x that is not taken here, because it buys speed with
/// a corruption check.
#[test]
fn a_text_late_build_pays_the_corpus_row_once_per_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, s) = seed(&path);
    db.checkpoint().unwrap();
    let bio = db.create_text_index(s.people, "bio_idx", "bio").unwrap();
    db.commit().unwrap();
    let accesses = build(&mut db, bio) as f64 / PEOPLE as f64;
    eprintln!("COST text build {accesses:.3} page accesses per indexed row");
    assert!(
        accesses <= 75.0,
        "a text build costs {accesses:.3} page accesses per row, against 106.608 before fix B"
    );
}

/// The sorted driver must persist the same bytes as the step driver. The
/// oracle constants above were pinned against the unchanged builder.
#[test]
fn a_sorted_late_build_keeps_the_pinned_digests() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, s) = seed(&path);
    let age = db
        .create_scalar_index(s.people, "age_idx", "age", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(age, 256).unwrap();
    let active = db
        .create_scalar_index(s.people, "active_idx", "active", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(active, 256).unwrap();
    let bio = db.create_text_index(s.people, "bio_idx", "bio").unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(bio, 256).unwrap();
    let home = db.create_point_index(s.people, "home_idx", "home").unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(home, 256).unwrap();
    assert_eq!(digest(&db, "scalar", age).0, SCALAR_AGE);
    assert_eq!(digest(&db, "scalar", active).0, SCALAR_ACTIVE);
    let (text, entries) = digest(&db, "text", bio);
    eprintln!("ORACLE text bio segments: {entries} entries {text}");
    assert_eq!(text, TEXT_BIO_SEGMENTS);
    assert_eq!(digest(&db, "spatial", home).0, SPATIAL_HOME);
}

/// Sort restarts; insert resumes from already-committed keys. Killing the
/// writer after one committed group, reopening, and finishing must leave
/// the oracle bytes unchanged.
#[test]
fn a_sorted_build_resumes_after_committed_groups_and_keeps_the_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, s) = seed(&path);
    let age = db
        .create_scalar_index(s.people, "age_idx", "age", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready_capped(age, 16, Some(1)).unwrap();
    assert!(matches!(
        db.index_info(age).unwrap().state,
        IndexState::Building { .. }
    ));
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    assert!(matches!(
        db.index_info(age).unwrap().state,
        IndexState::Building { .. }
    ));
    db.build_index_to_ready(age, 16).unwrap();
    assert_eq!(db.index_info(age).unwrap().state, IndexState::Ready);
    assert_eq!(digest(&db, "scalar", age).0, SCALAR_AGE);

    // Spatial oracle keys include the index identity (4 after age/active/bio).
    let _ = db
        .create_scalar_index(s.people, "active_idx", "active", false)
        .unwrap();
    db.commit().unwrap();
    let _ = db.create_text_index(s.people, "bio_idx", "bio").unwrap();
    db.commit().unwrap();
    let home = db.create_point_index(s.people, "home_idx", "home").unwrap();
    db.commit().unwrap();
    db.build_index_to_ready_capped(home, 16, Some(1)).unwrap();
    assert!(matches!(
        db.index_info(home).unwrap().state,
        IndexState::Building { .. }
    ));
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    db.build_index_to_ready(home, 16).unwrap();
    assert_eq!(digest(&db, "spatial", home).0, SPATIAL_HOME);
}

// ------------------------------------------- bounded runs (loop 5, item B1)

/// A logged page and its frame header: `FRAME` in `src/pagewal.rs`.
const WAL_FRAME_BYTES: u64 = 4096 + 48;

/// Walk one per-index tree and return `(leaf depths, live cell bytes, leaves)`.
///
/// Reads raw pages the way `src/bin/collection_inspect.rs` does rather than
/// through the engine, so the shape it reports is the shape on disk and not
/// the shape the builder believes it wrote.
fn leaf_shape(store: &e4_prototype::pagewal::PageWalStore, tree: u16, no: u32, depth: usize,
              depths: &mut Vec<usize>, used: &mut u64, leaves: &mut u64) {
    use kernel::page::{PageKind, PageRef};
    let bytes = store.page_bytes(no).unwrap();
    let p = PageRef::open(&bytes, no).unwrap();
    assert_eq!(p.tree_id(), tree, "page {no} belongs to another tree");
    assert!(depth < 64, "tree deeper than the format allows");
    match p.kind() {
        PageKind::Leaf => {
            depths.push(depth);
            *leaves += 1;
            for i in 0..p.nentries() {
                *used += p.slot(i).len() as u64 + 4;
            }
        }
        PageKind::Interior => {
            for i in 0..=p.nentries() {
                let child = if i == 0 {
                    p.child0()
                } else {
                    let r = p.slot(i - 1);
                    u32::from_le_bytes(r[r.len() - 4..].try_into().unwrap())
                };
                leaf_shape(store, tree, child, depth + 1, depths, used, leaves);
            }
        }
        _ => panic!("non-tree page {no} in a per-index tree"),
    }
}

/// An index build must complete at ANY size under the FIXED WAL allowance,
/// and completing must not change a byte of what it persists.
///
/// The old builder handed the whole index to one `tree_pack`, which is one
/// transaction, which the page-WAL refuses past its managed-byte allowance --
/// at 1,000,000 scalar rows under the fixed 16 MiB, by construction. The
/// builder now sizes its own transactions (`Database::sorted_run_budget`):
/// the first run is packed, the rest appends at the right edge of the same
/// tree, each run its own commit.
///
/// This forces at least three of those runs by shrinking the allowance instead
/// of growing the corpus, and then checks the four things that could have gone
/// wrong: the persisted ENTRY SET moved (pinned digests), the tree stopped
/// being uniformly deep (which a graft would have caused), the pages came out
/// sparse (occupancy), or some single transaction was still large enough to be
/// refused on a real database (the frame watermark).
#[test]
fn a_bounded_run_build_finishes_under_a_small_allowance_and_keeps_the_pinned_digests() {
    use e4_prototype::collections::verification::{verify_indexed_source, VerificationLimits};
    use e4_prototype::pagewal::PageWalStore;
    use kernel::limits::ResourceLimits;

    const WAL_BYTES: u64 = 256 << 10;
    let limits = ResourceLimits {
        data_bytes: 32 << 20,
        wal_bytes: WAL_BYTES,
        tracked_pages: 4096,
        readers: 4,
        record_bytes: 16384,
        recovery_bytes: 256 << 10,
    };
    // The bound the engine computes, restated here from the allowance alone so
    // the test does not read it back from the code it is checking: a quarter
    // of the allowance, in 4144-byte frames.
    let bound = WAL_BYTES / 4 / WAL_FRAME_BYTES;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, s) = seed_with(&path, Some(limits), 64);
    db.checkpoint().unwrap();
    drop(db);
    // Reopen so the I/O counters below describe the BUILDS and not the
    // seeding: they are monotonic from the moment a handle opens.
    let mut db = Database::open(&path, cfg()).unwrap();

    let before = db.io_counters().unwrap();
    let age = db
        .create_scalar_index(s.people, "age_idx", "age", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(age, 256).unwrap();
    let after = db.io_counters().unwrap();
    let runs = after.commit_frames - before.commit_frames;
    eprintln!(
        "BOUNDED wal_bytes={WAL_BYTES} bound={bound} frames runs={runs} \
         max_transaction_frames={}",
        after.max_transaction_frames
    );
    assert!(
        runs >= 3,
        "the allowance was meant to force at least three runs, it took {runs}"
    );
    assert!(
        after.max_transaction_frames <= bound,
        "a single transaction of the build wrote {} frames, over the {bound}-frame run bound",
        after.max_transaction_frames
    );

    let active = db
        .create_scalar_index(s.people, "active_idx", "active", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(active, 256).unwrap();
    let bio = db.create_text_index(s.people, "bio_idx", "bio").unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(bio, 256).unwrap();
    let home = db.create_point_index(s.people, "home_idx", "home").unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(home, 256).unwrap();

    // The entry SET is what may not move. Only the page shape may.
    assert_eq!(digest(&db, "scalar", age).0, SCALAR_AGE);
    assert_eq!(digest(&db, "scalar", active).0, SCALAR_ACTIVE);
    assert_eq!(digest(&db, "spatial", home).0, SPATIAL_HOME);
    assert_eq!(digest(&db, "text", bio).0, TEXT_BIO_SEGMENTS);
    assert_eq!(digest(&db, "scalar", age).1, PEOPLE);

    let trees = db.index_trees().unwrap();
    db.checkpoint().unwrap();
    drop(db);

    // Clean, by the engine's own two-way verifier.
    let report = verify_indexed_source(&path, VerificationLimits::default(), |_| {}).unwrap();
    assert!(
        report.clean && report.complete,
        "the verifier is not clean after a bounded-run build: {report:?}"
    );

    // Uniformly deep, and dense. A right-edge GRAFT would fail the first of
    // these: it enters as one separator, so its subtree's height is its own.
    let store = PageWalStore::open(&path, false, 1 << 20).unwrap();
    for (tree, root) in trees {
        let (mut depths, mut used, mut leaves) = (Vec::new(), 0u64, 0u64);
        leaf_shape(&store, tree, root, 0, &mut depths, &mut used, &mut leaves);
        let occupancy = used as f64 / (leaves * (4096 - 40)) as f64;
        eprintln!(
            "BOUNDED tree {tree}: {leaves} leaves depths {:?}..{:?} occupancy {occupancy:.3}",
            depths.iter().min(),
            depths.iter().max()
        );
        assert_eq!(
            depths.iter().min(),
            depths.iter().max(),
            "tree {tree} is not uniformly deep"
        );
        if leaves > 2 {
            assert!(
                occupancy >= 0.85,
                "tree {tree} packs {occupancy:.3} of its leaves, under the 0.85 floor"
            );
        }
    }
}
