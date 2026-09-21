//! Lean sekejap disk format v2 compatibility suite. Fixtures in
//! `docs/format-v2-fixtures/` and `docs/format-v2-baseline/` are mandatory
//! preserved reference files; this file never regenerates either corpus.
//!
//! Missing or changed fixtures fail qualification. Database operations use only
//! temporary copies; on macOS TMPDIR must be under <scratch>
//!
//! The contract is `docs/core/FORMAT_V2.md`.

use sekejap_core::collections::{CollectionId, Database, EntityId};
use kernel::{
    io::IoMode,
    page::{seal, PageKind, PageMut, PageRef, PAGE_SIZE},
    store::{Config, SyncMode},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

// The preserved corpora live at the REPOSITORY root, one level above the
// engine crate: `CARGO_MANIFEST_DIR` is `core/engine/` since the restructure.
const FIXTURES: &str = "../../docs/format-v2-fixtures";
// Locks the preserved corpus, including its independent expected-result manifests.
const INDEX_SHA256: &str = "6b7a47df653383aacf025951ede47d33a8aeba43ebb12f20d338d60af36900d8";
const BASELINE_FIXTURES: &str = "../../docs/format-v2-baseline";
// A second, independent capture: the 16-byte database identity is drawn fresh
// for every created database, so two runs of the same generator are two
// corpora, not two copies of one. Preserve it independently of the first.
const BASELINE_INDEX_SHA256: &str = "9b87de6805c4f6d4f8f4816673db6a0818e6d3e9b7b0f134781a49c30d08e54e";
const FEATURES: usize = 48;
// Page header bytes 18-19: the disk-format stamp (`kernel::FORMAT_VERSION`).
const STAMP_AT: usize = 18;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn fixtures_root() -> PathBuf {
    require_fixtures_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURES))
}

fn require_fixtures_root(root: PathBuf) -> PathBuf {
    assert!(
        root.join("INDEX.json").is_file(),
        "required preserved format-v2 fixtures missing at {}; compatibility qualification cannot skip fixtures",
        root.display()
    );
    root
}

#[test]
#[should_panic(expected = "required preserved format-v2 fixtures missing")]
fn missing_preserved_fixtures_fail_qualification() {
    // A regular file cannot contain INDEX.json; no temporary database is needed.
    require_fixtures_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"));
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut padded = data.to_vec();
    let bit_len = (data.len() as u64).saturating_mul(8);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h;
        for i in 0..64 {
            let s1 = a[4].rotate_right(6) ^ a[4].rotate_right(11) ^ a[4].rotate_right(25);
            let ch = (a[4] & a[5]) ^ ((!a[4]) & a[6]);
            let t1 = a[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a[0].rotate_right(2) ^ a[0].rotate_right(13) ^ a[0].rotate_right(22);
            let maj = (a[0] & a[1]) ^ (a[0] & a[2]) ^ (a[1] & a[2]);
            let t2 = s0.wrapping_add(maj);
            a[7] = a[6];
            a[6] = a[5];
            a[5] = a[4];
            a[4] = a[3].wrapping_add(t1);
            a[3] = a[2];
            a[2] = a[1];
            a[1] = a[0];
            a[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(a[i]);
        }
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct Fixture {
    name: String,
    dir: PathBuf,
    manifest: Value,
    manifest_sha256: String,
}

fn load_fixtures(root: &Path) -> Vec<Fixture> {
    let mut fixtures = load_fixture_corpus(root, INDEX_SHA256);
    let baseline =
        require_fixtures_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(BASELINE_FIXTURES));
    fixtures.extend(load_fixture_corpus(&baseline, BASELINE_INDEX_SHA256));
    fixtures
}

fn load_fixture_corpus(root: &Path, index_sha256: &str) -> Vec<Fixture> {
    let bytes = fs::read(root.join("INDEX.json")).expect("read required fixture INDEX.json");
    assert_eq!(
        hex(&sha256(&bytes)),
        index_sha256,
        "preserved fixture index changed at {}",
        root.display()
    );
    let index: Value = serde_json::from_slice(&bytes).unwrap();
    let mut out = Vec::new();
    for f in index["fixtures"].as_array().expect("INDEX.json fixtures") {
        let name = f["name"].as_str().expect("fixture name").to_string();
        let dir = root.join(&name);
        let bytes = fs::read(dir.join("MANIFEST.json")).unwrap();
        let manifest_sha256 = f["manifest_sha256"]
            .as_str()
            .expect("manifest sha256")
            .to_owned();
        assert_eq!(
            hex(&sha256(&bytes)),
            manifest_sha256,
            "{name}: preserved manifest changed"
        );
        let manifest: Value = serde_json::from_slice(&bytes).unwrap();
        out.push(Fixture {
            name: format!("{}--{name}", root.file_name().unwrap().to_string_lossy()),
            dir,
            manifest,
            manifest_sha256,
        });
    }
    assert!(!out.is_empty(), "INDEX.json lists no fixtures");
    out
}

fn verify_manifest_hashes(fx: &Fixture) {
    assert_eq!(
        hex(&sha256(&fs::read(fx.dir.join("MANIFEST.json")).unwrap())),
        fx.manifest_sha256,
        "{}: preserved manifest changed",
        fx.name
    );
    let files = fx.manifest["files"]
        .as_object()
        .expect("MANIFEST files object");
    let actual: BTreeSet<_> = fs::read_dir(&fx.dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    let expected: BTreeSet<_> = files
        .keys()
        .cloned()
        .chain(std::iter::once("MANIFEST.json".to_owned()))
        .collect();
    assert_eq!(
        actual, expected,
        "{}: preserved file inventory changed",
        fx.name
    );
    for (name, rec) in files {
        let path = fx.dir.join(name);
        let bytes = fs::read(&path).unwrap_or_else(|e| panic!("{}: read {name}: {e}", fx.name));
        let got = hex(&sha256(&bytes));
        let want = rec["sha256"].as_str().expect("file sha256");
        assert_eq!(
            got, want,
            "{}: {name} sha256 changed (fixture is not the preserved reference)",
            fx.name
        );
        let want_len = rec["bytes"].as_u64().expect("file bytes") as usize;
        assert_eq!(bytes.len(), want_len, "{}: {name} size changed", fx.name);
    }
}

fn copy_fixture(fx: &Fixture, label: &str) -> tempfile::TempDir {
    let base = std::env::temp_dir();
    if cfg!(target_os = "macos") {
        assert!(
            base.starts_with("<scratch>"),
            "set TMPDIR under <scratch> before database tests"
        );
    }
    let dst = tempfile::Builder::new()
        .prefix(&format!("format-v2-{}-{label}-", fx.name))
        .tempdir_in(base)
        .unwrap();
    for name in fx.manifest["files"].as_object().unwrap().keys() {
        fs::copy(fx.dir.join(name), dst.path().join(name)).unwrap();
    }
    dst
}

fn file_bytes(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            assert!(
                entry.file_type().unwrap().is_file(),
                "unexpected database directory entry"
            );
            (
                entry.file_name().into_string().unwrap(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

/// Independent expectations for this pinned corpus, from the original
/// format_fixture::populate insertion order, NOT engine reads or regenerated
/// fixtures. People allocate 1..=200, delete slot 13 (ID 14), then delete and
/// reinsert slot 7 as ID 201. Events and blobs have separate counters starting
/// at 1. The compatibility writer's next new person must therefore be ID 202.
fn expected_entity_id(collection: &str, key: &str) -> EntityId {
    assert_eq!(key.len(), 9, "unexpected fixture key {key}");
    let slot: u64 = key
        .strip_prefix('p')
        .expect("fixture key prefix")
        .parse()
        .unwrap();
    let (collection, sequence) = match (collection, slot) {
        ("people", 7) => (1, 201),
        ("people", 999) => (1, 202),
        ("people", 0..=199) => (1, slot + 1),
        ("events", 0..=79) => (2, slot + 1),
        ("blobs", 0..=3) => (3, slot + 1),
        _ => panic!("unexpected fixture identity {collection}/{key}"),
    };
    EntityId {
        collection: CollectionId(collection),
        sequence,
    }
}

fn features_on_disk(dir: &Path) -> [u64; 2] {
    let data = fs::read(dir.join("data")).unwrap();
    let mut out = [0; 2];
    for no in 0..2 {
        let b = &data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE];
        let slot = PageRef::open(b, no as u32).unwrap().slot(0).to_vec();
        out[no] = u64::from_le_bytes(slot[FEATURES..FEATURES + 8].try_into().unwrap());
    }
    out
}

fn rewrite_header(dir: &Path, no: usize, f: impl FnOnce(&mut Vec<u8>)) {
    let mut data = fs::read(dir.join("data")).unwrap();
    let b = &mut data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE];
    let mut h = PageRef::open(b, no as u32).unwrap().slot(0).to_vec();
    f(&mut h);
    let mut page = PageMut::init(b, PageKind::Meta, 0, no as u32);
    page.insert_slot(0, &h).unwrap();
    page.finalise(0);
    seal(b, 1);
    PageRef::open(b, no as u32).unwrap();
    fs::write(dir.join("data"), data).unwrap();
}

fn check_expected(db: &Database, manifest: &Value, label: &str) {
    let collections = manifest["collections"].as_array().unwrap();
    for c in collections {
        let name = c["name"].as_str().unwrap();
        let id = db
            .collection(name)
            .unwrap()
            .unwrap_or_else(|| panic!("{label}: collection {name} missing"));
        let expected_id = c["id"].as_u64().unwrap() as u32;
        assert_eq!(id.0, expected_id, "{label}: collection id for {name}");
        let info = db.collection_info(id).unwrap();
        assert_eq!(
            info.timestamps,
            c["timestamps"].as_bool().unwrap(),
            "{label}: timestamps for {name}"
        );
        assert_eq!(
            info.layout.id,
            c["layout_id"].as_u64().unwrap(),
            "{label}: layout id for {name}"
        );
        let expected_fields: Vec<(String, String)> = c["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["name"].as_str().unwrap().to_string(),
                    f["kind"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let actual: Vec<(String, String)> = info
            .layout
            .fields
            .iter()
            .map(|(n, k)| (n.clone(), format!("{k:?}")))
            .collect();
        assert_eq!(actual, expected_fields, "{label}: fields for {name}");
    }
    let entities = manifest["entities"].as_array().unwrap();
    for e in entities {
        let coll = e["collection"].as_str().unwrap();
        let key = e["key"].as_str().unwrap();
        let id = db.collection(coll).unwrap().unwrap();
        let found = db
            .get(id, key)
            .unwrap()
            .unwrap_or_else(|| panic!("{label}: missing {coll}/{key}"));
        let expected_id = expected_entity_id(coll, key);
        assert_eq!(found.id, expected_id, "{label}: identity {coll}/{key}");
        assert_eq!(found.key, key, "{label}: external key {coll}/{key}");
        let by_id = db
            .get_by_id(expected_id)
            .unwrap()
            .unwrap_or_else(|| panic!("{label}: missing numeric identity {expected_id:?}"));
        assert_eq!(by_id, found, "{label}: numeric lookup {coll}/{key}");
        assert_eq!(
            &found.document, &e["document"],
            "{label}: document {coll}/{key}"
        );
    }
    for d in manifest["deleted"].as_array().unwrap() {
        let coll = d["collection"].as_str().unwrap();
        let key = d["key"].as_str().unwrap();
        let id = db.collection(coll).unwrap().unwrap();
        assert!(
            db.get(id, key).unwrap().is_none(),
            "{label}: expected {coll}/{key} absent"
        );
        assert!(
            db.get_by_id(expected_entity_id(coll, key))
                .unwrap()
                .is_none(),
            "{label}: deleted numeric identity for {coll}/{key} remains"
        );
    }
    assert!(
        db.get_by_id(EntityId {
            collection: CollectionId(1),
            sequence: 8
        })
        .unwrap()
        .is_none(),
        "{label}: retired pre-reinsert identity remains"
    );
    let mut expected_rows = BTreeMap::new();
    for e in entities {
        let key = (
            e["collection"].as_str().unwrap().to_owned(),
            e["key"].as_str().unwrap().to_owned(),
        );
        assert!(
            expected_rows.insert(key, e["document"].clone()).is_none(),
            "{label}: duplicate manifest entity"
        );
    }
    let mut actual_rows = BTreeMap::new();
    for c in collections {
        let name = c["name"].as_str().unwrap();
        let id = db.collection(name).unwrap().unwrap();
        for row in db.scan(id, None).unwrap() {
            let row = row.unwrap();
            assert_eq!(
                row.id,
                expected_entity_id(name, &row.key),
                "{label}: scan identity"
            );
            assert!(
                actual_rows
                    .insert((name.to_owned(), row.key), row.document)
                    .is_none(),
                "{label}: duplicate scan entity"
            );
        }
    }
    let expected = manifest["counts"]["entities"].as_u64().unwrap() as usize;
    assert_eq!(
        actual_rows, expected_rows,
        "{label}: exact scanned entities"
    );
    assert_eq!(actual_rows.len(), expected, "{label}: live entity count");
    assert_eq!(
        entities.len(),
        expected,
        "{label}: manifest entity list vs counts.entities"
    );
}

/// Preserved fixture bytes must still match the MANIFEST written beside them.
#[test]
fn every_fixture_file_matches_its_manifest_sha256() {
    let root = fixtures_root();
    for fx in load_fixtures(&root) {
        verify_manifest_hashes(&fx);
    }
}

/// A current snapshot reader serves every expected entity and count from a
/// copy of the preserved fixture, never from the original files.
#[test]
fn a_snapshot_of_a_copied_fixture_serves_every_expected_entity_and_count() {
    let root = fixtures_root();
    for fx in load_fixtures(&root) {
        verify_manifest_hashes(&fx);
        let work = copy_fixture(&fx, "snapshot");
        let copy = work.path();
        let db = Database::open_snapshot(&copy, cfg())
            .unwrap_or_else(|e| panic!("{}: snapshot open refused: {e}", fx.name));
        check_expected(&db, &fx.manifest, &fx.name);
        drop(db);
        verify_manifest_hashes(&fx);
    }
}

/// A routine update/insert/delete/commit/checkpoint/reopen must not change
/// the database's declared required-feature bits.
#[test]
fn a_writer_update_insert_delete_commit_checkpoint_reopen_preserves_declared_feature_bits() {
    let root = fixtures_root();
    for fx in load_fixtures(&root) {
        verify_manifest_hashes(&fx);
        let work = copy_fixture(&fx, "writer");
        let copy = work.path();
        let declared = features_on_disk(&copy);
        assert_eq!(
            declared[0], declared[1],
            "{}: metadata copies disagree",
            fx.name
        );
        let want = if fx.manifest["compact_cells"].as_bool().unwrap() {
            1
        } else {
            0
        };
        assert_eq!(
            declared, [want; 2],
            "{}: declared compact-cells bit",
            fx.name
        );

        let mut db = Database::open(&copy, cfg()).unwrap();
        check_expected(&db, &fx.manifest, &format!("{}:pre-write", fx.name));
        assert_eq!(features_on_disk(&copy), declared, "{}: after open", fx.name);

        let people = db.collection("people").unwrap().unwrap();
        let updated_id = db
            .update(
                people,
                "p00000000",
                &serde_json::json!({"note": "compat-updated"}),
            )
            .unwrap();
        assert_eq!(
            updated_id,
            expected_entity_id("people", "p00000000"),
            "{}: update must preserve identity",
            fx.name
        );
        assert_eq!(
            features_on_disk(&copy),
            declared,
            "{}: after update",
            fx.name
        );
        let inserted = serde_json::json!({
            "fullname": "Compat Insert",
            "born": 2000,
            "income": 1.25,
            "location": {"type": "Point", "coordinates": [0.0, 0.0]},
            "profile": {},
            "note": "inserted",
        });
        let inserted_id = db.put(people, "p00000999", &inserted).unwrap();
        assert_eq!(
            inserted_id,
            expected_entity_id("people", "p00000999"),
            "{}: insert must preserve the persisted identity counter",
            fx.name
        );
        assert_eq!(
            features_on_disk(&copy),
            declared,
            "{}: after insert",
            fx.name
        );
        assert!(
            db.delete(people, "p00000001").unwrap(),
            "{}: delete",
            fx.name
        );
        assert_eq!(
            features_on_disk(&copy),
            declared,
            "{}: after delete",
            fx.name
        );
        db.commit().unwrap();
        assert_eq!(
            features_on_disk(&copy),
            declared,
            "{}: after commit",
            fx.name
        );
        assert!(
            db.checkpoint().unwrap(),
            "{}: checkpoint must complete (no live snapshot)",
            fx.name
        );
        assert_eq!(
            features_on_disk(&copy),
            declared,
            "{}: after checkpoint",
            fx.name
        );
        drop(db);

        // Derive the post-write oracle from the preserved manifest and the
        // requested mutations, never from engine output.
        let mut expected = fx.manifest.clone();
        let entities = expected["entities"].as_array_mut().unwrap();
        entities.retain(|e| !(e["collection"] == "people" && e["key"] == "p00000001"));
        let updated = entities
            .iter_mut()
            .find(|e| e["collection"] == "people" && e["key"] == "p00000000")
            .unwrap();
        updated["document"]["note"] = serde_json::json!("compat-updated");
        entities.push(
            serde_json::json!({"collection": "people", "key": "p00000999", "document": inserted}),
        );
        expected["deleted"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"collection": "people", "key": "p00000001"}));
        let db = Database::open(&copy, cfg()).unwrap();
        check_expected(&db, &expected, &format!("{}:post-write", fx.name));
        drop(db);
        assert_eq!(
            features_on_disk(&copy),
            declared,
            "{}: after reopen",
            fx.name
        );
        verify_manifest_hashes(&fx);
    }
}

/// An unknown required feature bit is refused before any data or WAL byte
/// changes, including coordination files or inventory. Both metadata copies are rewritten and resealed so the opener
/// judges the bit, not a checksum failure.
#[test]
fn an_unknown_required_feature_bit_is_refused_and_leaves_all_files_unchanged() {
    let root = fixtures_root();
    for fx in load_fixtures(&root) {
        verify_manifest_hashes(&fx);
        let work = copy_fixture(&fx, "unknown-bit");
        let copy = work.path();
        for no in 0..2 {
            rewrite_header(&copy, no, |h| h[FEATURES + 7] |= 0x80);
        }
        let after_edit = file_bytes(&copy);
        assert!(
            Database::open(&copy, cfg()).is_err(),
            "{}: unknown required bit must be refused",
            fx.name
        );
        assert_eq!(
            file_bytes(&copy),
            after_edit,
            "{}: refusal must not change any file bytes or inventory",
            fx.name
        );
        assert!(
            Database::open_snapshot(&copy, cfg()).is_err(),
            "{}: snapshot must also refuse an unknown required bit",
            fx.name
        );
        assert_eq!(
            file_bytes(&copy),
            after_edit,
            "{}: snapshot refusal must not change any file bytes or inventory",
            fx.name
        );
        verify_manifest_hashes(&fx);
    }
}

/// A WAL taken from a different fixture (different database identity) beside
/// this fixture's data file is refused, and every file and the directory inventory stay identical.
#[test]
fn a_foreign_wal_beside_a_data_file_is_refused_and_leaves_all_files_unchanged() {
    let root = fixtures_root();
    let fixtures = load_fixtures(&root);
    assert!(
        fixtures.len() >= 2,
        "foreign-WAL check needs at least two fixtures"
    );
    let donors: Vec<&Fixture> = fixtures
        .iter()
        .filter(|f| {
            fs::metadata(f.dir.join("wal"))
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        })
        .collect();
    assert!(
        !donors.is_empty(),
        "need a wal-pending fixture as the foreign WAL donor"
    );
    for fx in &fixtures {
        let donor = donors
            .iter()
            .find(|d| d.name != fx.name)
            .expect("need a foreign WAL from a different fixture");
        verify_manifest_hashes(fx);
        verify_manifest_hashes(donor);
        let work = copy_fixture(fx, "foreign-wal");
        let copy = work.path();
        fs::copy(donor.dir.join("wal"), copy.join("wal")).unwrap();
        let after_edit = file_bytes(&copy);
        assert_ne!(
            after_edit["wal"],
            fs::read(fx.dir.join("wal")).unwrap(),
            "{}: foreign WAL must differ from the original",
            fx.name
        );
        assert!(
            Database::open(&copy, cfg()).is_err(),
            "{}: foreign WAL must be refused",
            fx.name
        );
        assert_eq!(
            file_bytes(&copy),
            after_edit,
            "{}: foreign-WAL refusal must not change any file bytes or inventory",
            fx.name
        );
        verify_manifest_hashes(fx);
        verify_manifest_hashes(donor);
    }
}

/// A checksum-valid unknown physical version cannot hide behind a supported
/// sibling metadata page or committed WAL. Test either copy and both copies.
#[test]
fn an_unknown_physical_page_version_is_refused_without_changing_any_file() {
    let root = fixtures_root();
    for fx in load_fixtures(&root) {
        verify_manifest_hashes(&fx);
        for pages in [&[0usize][..], &[1usize][..], &[0usize, 1][..]] {
            let work = copy_fixture(&fx, "unknown-page-version");
            let copy = work.path();
            let mut data = fs::read(copy.join("data")).unwrap();
            for &no in pages {
                let page = &mut data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE];
                PageRef::open(page, no as u32).unwrap();
                page[4..6].copy_from_slice(&u16::MAX.to_le_bytes());
                let crc = kernel::page::checksum(page);
                page[36..40].copy_from_slice(&crc.to_le_bytes());
                assert!(PageRef::open(page, no as u32).is_err());
            }
            fs::write(copy.join("data"), data).unwrap();
            let after_edit = file_bytes(copy);
            let error = Database::open(copy, cfg()).err().unwrap_or_else(|| {
                panic!(
                    "{}: writer accepted unknown page version in {pages:?}",
                    fx.name
                )
            });
            assert!(
                error.to_string().contains("unknown format version"),
                "{}: writer refused for an unrelated reason: {error}",
                fx.name
            );
            assert_eq!(
                file_bytes(copy),
                after_edit,
                "{}: writer refusal modified file bytes or inventory",
                fx.name
            );
            let error = Database::open_snapshot(copy, cfg())
                .err()
                .unwrap_or_else(|| {
                    panic!(
                        "{}: snapshot accepted unknown page version in {pages:?}",
                        fx.name
                    )
                });
            assert!(
                error.to_string().contains("unknown format version"),
                "{}: snapshot refused for an unrelated reason: {error}",
                fx.name
            );
            assert_eq!(
                file_bytes(copy),
                after_edit,
                "{}: snapshot refusal modified file bytes or inventory",
                fx.name
            );
            verify_manifest_hashes(&fx);
        }
    }
}

// ---------------------------------------------------------------------------
// The disk-format stamp. `docs/core/FORMAT_V2.md`, "The stamp".
// ---------------------------------------------------------------------------

/// A page-WAL frame: 32-byte header, 4096-byte page image, 16-byte identity.
const FRAME: usize = PAGE_SIZE + 48;
const FRAME_PAYLOAD_AT: usize = 32;

fn stamp_of(page: &[u8]) -> u16 {
    u16::from_le_bytes(page[STAMP_AT..STAMP_AT + 2].try_into().unwrap())
}

/// Rewrite bytes 18-19 of one metadata copy and restore its page checksum, so
/// the opener judges the CLAIM rather than a damaged page. Deliberately not
/// `rewrite_header`: that one rebuilds the page through `PageMut::init`, which
/// would stamp it 2 again.
fn restamp_metadata(dir: &Path, no: usize, claim: u16) {
    let mut data = fs::read(dir.join("data")).unwrap();
    let page = &mut data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE];
    assert_eq!(stamp_of(page), kernel::FORMAT_VERSION, "fixture was not v2");
    page[STAMP_AT..STAMP_AT + 2].copy_from_slice(&claim.to_le_bytes());
    let crc = kernel::page::checksum(page);
    page[36..40].copy_from_slice(&crc.to_le_bytes());
    fs::write(dir.join("data"), data).unwrap();
}

/// Every page of every preserved fixture carries disk format 2: both
/// checkpoint metadata copies, every data page, and every page image a
/// committed WAL frame carries.
#[test]
fn every_page_and_every_wal_frame_image_of_the_corpus_carries_the_disk_format_stamp() {
    assert_eq!(kernel::FORMAT_VERSION, 2);
    assert_eq!(sekejap_core::FORMAT_VERSION, kernel::FORMAT_VERSION);
    let root = fixtures_root();
    let mut frames_seen = 0usize;
    for fx in load_fixtures(&root) {
        verify_manifest_hashes(&fx);
        let data = fs::read(fx.dir.join("data")).unwrap();
        assert_eq!(data.len() % PAGE_SIZE, 0, "{}: data is whole pages", fx.name);
        assert!(data.len() / PAGE_SIZE >= 2, "{}: two metadata copies", fx.name);
        for no in 0..data.len() / PAGE_SIZE {
            let page = &data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE];
            assert_eq!(
                stamp_of(page),
                kernel::FORMAT_VERSION,
                "{}: data page {no} must carry disk format 2",
                fx.name
            );
            // The metadata copies and every data page also OPEN, which is the
            // same judgement taken through the reader rather than by hand.
            PageRef::open(page, no as u32)
                .unwrap_or_else(|e| panic!("{}: page {no} did not open: {e}", fx.name));
        }
        let wal = fs::read(fx.dir.join("wal")).unwrap();
        assert_eq!(wal.len() % FRAME, 0, "{}: WAL is whole frames", fx.name);
        for (i, frame) in wal.chunks_exact(FRAME).enumerate() {
            let kind = u32::from_le_bytes(frame[8..12].try_into().unwrap());
            if kind != 1 {
                continue; // a commit frame carries a CRC, not a page image
            }
            let page_no = u32::from_le_bytes(frame[12..16].try_into().unwrap());
            let image = &frame[FRAME_PAYLOAD_AT..FRAME_PAYLOAD_AT + PAGE_SIZE];
            assert_eq!(
                stamp_of(image),
                kernel::FORMAT_VERSION,
                "{}: WAL frame {i} carries a page image without the stamp",
                fx.name
            );
            PageRef::open(image, page_no)
                .unwrap_or_else(|e| panic!("{}: WAL frame {i} image did not open: {e}", fx.name));
            frames_seen += 1;
        }
    }
    assert!(
        frames_seen > 0,
        "the corpus must include wal-pending fixtures, or the frame claim is untested"
    );
}

/// A database this build CREATES carries the stamp on both metadata copies
/// and on its data pages -- the corpus proves what was captured, this proves
/// what the binary in hand writes.
#[test]
fn a_database_this_build_creates_carries_the_stamp_on_pages_zero_and_one_and_on_a_data_page() {
    let base = std::env::temp_dir();
    if cfg!(target_os = "macos") {
        assert!(
            base.starts_with("<scratch>"),
            "set TMPDIR under <scratch> before database tests"
        );
    }
    let work = tempfile::Builder::new()
        .prefix("format-v2-created-")
        .tempdir_in(base)
        .unwrap();
    let dir = work.path().join("db");
    let mut db = Database::create(&dir, cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("name".into(), sekejap_core::Kind::Text)],
            sekejap_core::collections::CollectionOptions { timestamps: false },
        )
        .unwrap();
    for i in 0..400u32 {
        db.put(people, &format!("k{i:06}"), &serde_json::json!({"name": "x".repeat(64)}))
            .unwrap();
    }
    db.commit().unwrap();
    assert!(db.checkpoint().unwrap(), "checkpoint must complete");
    drop(db);

    let data = fs::read(dir.join("data")).unwrap();
    let pages = data.len() / PAGE_SIZE;
    assert!(pages >= 3, "need both metadata copies and a data page; got {pages}");
    for no in [0usize, 1, 2] {
        assert_eq!(
            stamp_of(&data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE]),
            kernel::FORMAT_VERSION,
            "page {no} of a freshly created database"
        );
    }
    for no in 0..pages {
        assert_eq!(
            stamp_of(&data[no * PAGE_SIZE..(no + 1) * PAGE_SIZE]),
            kernel::FORMAT_VERSION,
            "page {no} of a freshly created database"
        );
    }
}

/// A checksum-valid disk-format stamp that is not 2, on either metadata copy
/// or on both, is refused by name before anything is read or written, and
/// every file byte and the directory inventory survive the attempt.
#[test]
fn a_disk_format_stamp_that_is_not_two_is_refused_by_name_without_changing_any_file() {
    let root = fixtures_root();
    for fx in load_fixtures(&root) {
        verify_manifest_hashes(&fx);
        for claim in [1u16, 3] {
            for copies in [&[0usize][..], &[1usize][..], &[0usize, 1][..]] {
                let work = copy_fixture(&fx, "wrong-stamp");
                let copy = work.path();
                for &no in copies {
                    restamp_metadata(copy, no, claim);
                }
                let after_edit = file_bytes(copy);
                let want = format!("sekejap disk format {claim}; this build reads v2");

                let error = Database::open(copy, cfg()).err().unwrap_or_else(|| {
                    panic!("{}: writer accepted disk format {claim} on {copies:?}", fx.name)
                });
                assert!(
                    error.to_string().contains(&want),
                    "{}: writer refused for an unrelated reason: {error}",
                    fx.name
                );
                assert!(
                    matches!(error, sekejap_core::collections::Error::Unsupported(_)),
                    "{}: a foreign disk format is Unsupported, not {error:?}",
                    fx.name
                );
                assert_eq!(
                    file_bytes(copy),
                    after_edit,
                    "{}: writer refusal modified file bytes or inventory",
                    fx.name
                );

                let error = Database::open_snapshot(copy, cfg()).err().unwrap_or_else(|| {
                    panic!("{}: snapshot accepted disk format {claim} on {copies:?}", fx.name)
                });
                assert!(
                    error.to_string().contains(&want),
                    "{}: snapshot refused for an unrelated reason: {error}",
                    fx.name
                );
                assert_eq!(
                    file_bytes(copy),
                    after_edit,
                    "{}: snapshot refusal modified file bytes or inventory",
                    fx.name
                );
            }
        }
        verify_manifest_hashes(&fx);
    }
}
