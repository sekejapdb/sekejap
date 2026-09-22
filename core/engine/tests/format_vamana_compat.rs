//! Compatibility qualification for the VAMANA GRAPH family: keyspace `0x7D`,
//! logical feature bit `0x8000`.
//!
//! `docs/format-v2-vamana/` is a PRESERVED reference corpus, written once by
//! `bench/src/bin/vamana_format_fixture.rs` and never regenerated to make a
//! later engine pass. Missing or changed fixtures fail qualification. Every
//! database operation here is on a unique temporary COPY; on macOS TMPDIR
//! must be under <scratch>
//!
//! The ORACLE is the fixture's own MANIFEST, whose expected nearest
//! neighbours were computed by brute force in the generator from the f32
//! lanes it created -- never read back from an engine.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        verification::{verify_indexed_source, VerificationLimits},
        ApproxVectorMethod, Database, Error, IndexFamily, IndexId, IndexState, VectorMetric,
        SUPPORTED_LOGICAL_FEATURES, VAMANA_ALPHA_HUNDREDTHS, VAMANA_BUILD_SEARCH_LIST,
        VAMANA_DEGREE, VAMANA_ENTRY, VAMANA_FEATURE, VAMANA_GRAPH_VERSION,
    },
    internal::{admit_logical_features, logical_features},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const FIXTURES: &str = "../../docs/format-v2-vamana";
/// Locks the preserved corpus, its per-fixture checksums and its oracles.
const INDEX_SHA256: &str = "21ed742b7d437cfdb599e2bbb1b911871ac7ed285ac63671e8aeedbcd103c1ee";

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn fixtures_root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURES);
    assert!(
        root.join("INDEX.json").is_file(),
        "required preserved vamana fixtures missing at {}; compatibility qualification cannot skip fixtures",
        root.display()
    );
    root
}

struct Fixture {
    name: String,
    dir: PathBuf,
    manifest: Value,
}

fn load() -> Vec<Fixture> {
    let root = fixtures_root();
    let raw = fs::read(root.join("INDEX.json")).unwrap();
    assert_eq!(
        hex(&sha256(&raw)),
        INDEX_SHA256,
        "docs/format-v2-vamana/INDEX.json changed; the corpus is preserved, not regenerated"
    );
    let index: Value = serde_json::from_slice(&raw).unwrap();
    let mut out = Vec::new();
    for entry in index["fixtures"].as_array().unwrap() {
        let name = entry["name"].as_str().unwrap().to_owned();
        let dir = root.join(&name);
        let manifest_bytes = fs::read(dir.join("MANIFEST.json")).unwrap();
        assert_eq!(
            hex(&sha256(&manifest_bytes)),
            entry["manifest_sha256"].as_str().unwrap(),
            "{name}: MANIFEST.json does not match the pinned INDEX"
        );
        let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
        // Every engine file, byte for byte, against the manifest's own list.
        for record in manifest["files"].as_array().unwrap() {
            let file = record["name"].as_str().unwrap();
            let bytes = fs::read(dir.join(file)).unwrap();
            assert_eq!(
                bytes.len(),
                record["bytes"].as_u64().unwrap() as usize,
                "{name}/{file}: size changed"
            );
            assert_eq!(
                hex(&sha256(&bytes)),
                record["sha256"].as_str().unwrap(),
                "{name}/{file}: sha256 changed (the fixture is not the preserved reference)"
            );
        }
        out.push(Fixture { name, dir, manifest });
    }
    assert_eq!(out.len(), 2, "the corpus is two independent captures");
    out
}

fn copy(fx: &Fixture, label: &str) -> tempfile::TempDir {
    let base = std::env::temp_dir();
    if cfg!(target_os = "macos") {
        assert!(
            base.starts_with("<scratch>"),
            "set TMPDIR under <scratch> before database tests"
        );
    }
    let dst = tempfile::Builder::new()
        .prefix(&format!("vamana-{}-{label}-", fx.name))
        .tempdir_in(base)
        .unwrap();
    for record in fx.manifest["files"].as_array().unwrap() {
        let file = record["name"].as_str().unwrap();
        fs::copy(fx.dir.join(file), dst.path().join(file)).unwrap();
    }
    dst
}

fn bytes_of(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().into_string().unwrap(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn index_of(db: &Database, manifest: &Value) -> IndexId {
    let name = manifest["index"].as_str().unwrap();
    let collection = db
        .collection(manifest["collection"].as_str().unwrap())
        .unwrap()
        .unwrap();
    let info = db
        .list_indexes(collection)
        .unwrap()
        .into_iter()
        .find(|info| info.name == name)
        .expect("the fixture's vamana index");
    assert_eq!(info.family, IndexFamily::VamanaGraph);
    assert_eq!(info.state, IndexState::Ready);
    info.id
}

/// The preserved corpus is readable by this build and answers the oracle its
/// own manifest carries.
#[test]
fn the_preserved_vamana_corpus_opens_and_answers_its_own_brute_force_oracle() {
    for fx in load() {
        // The shape the fixture was written at is the shape this build
        // implements: a graph whose degree or alpha differed would be a graph
        // this binary did not build.
        assert_eq!(fx.manifest["keyspace_tag"].as_u64().unwrap(), u64::from(VAMANA_ENTRY));
        assert_eq!(fx.manifest["feature_bit"].as_u64().unwrap(), VAMANA_FEATURE);
        assert_eq!(fx.manifest["graph"]["version"].as_u64().unwrap(), u64::from(VAMANA_GRAPH_VERSION));
        assert_eq!(fx.manifest["graph"]["degree"].as_u64().unwrap() as usize, VAMANA_DEGREE);
        assert_eq!(
            fx.manifest["graph"]["build_search_list"].as_u64().unwrap() as usize,
            VAMANA_BUILD_SEARCH_LIST
        );
        assert_eq!(
            fx.manifest["graph"]["alpha_hundredths"].as_u64().unwrap(),
            u64::from(VAMANA_ALPHA_HUNDREDTHS)
        );
        let declared = fx.manifest["declared_features"].as_u64().unwrap();
        assert_ne!(declared & VAMANA_FEATURE, 0, "{}: the fixture must declare the bit", fx.name);
        assert_eq!(
            declared & !SUPPORTED_LOGICAL_FEATURES,
            0,
            "{}: the fixture declares a bit this build does not implement",
            fx.name
        );

        let temp = copy(&fx, "open");
        let db = Database::open(temp.path(), cfg()).unwrap();
        assert_eq!(logical_features(&db), declared);
        let index = index_of(&db, &fx.manifest);
        let ef = fx.manifest["ef"].as_u64().unwrap() as usize;
        let k = fx.manifest["k"].as_u64().unwrap() as usize;
        for expectation in fx.manifest["expectations"].as_array().unwrap() {
            let query: Vec<f32> = expectation["query"]
                .as_array()
                .unwrap()
                .iter()
                .map(|lane| lane.as_f64().unwrap() as f32)
                .collect();
            let want: Vec<u64> = expectation["nearest"]
                .as_array()
                .unwrap()
                .iter()
                .map(|seq| seq.as_u64().unwrap())
                .collect();
            let result = db
                .query_vamana_vector(index, &query, VectorMetric::Cosine, k, ef, usize::MAX, || false)
                .unwrap();
            assert_eq!(result.method, ApproxVectorMethod::VamanaGraphV1);
            let got: Vec<u64> = result.hits.iter().map(|hit| hit.id.sequence).collect();
            assert_eq!(got, want, "{}: the graph answered the wrong rows", fx.name);
        }
        drop(db);
        let report = verify_indexed_source(temp.path(), VerificationLimits::default(), |issue| {
            panic!("{}: verification issue: {issue:?}", fx.name);
        })
        .unwrap();
        assert!(report.complete && report.clean, "{}: {report:?}", fx.name);
    }
}

/// Law 8. A binary whose supported mask predates `0x8000` refuses each
/// preserved fixture WHOLE, as `Unsupported` and naming the feature word, and
/// the refusal changes no byte of the source.
#[test]
fn a_build_that_predates_the_vamana_bit_refuses_the_corpus_by_name_and_changes_no_byte() {
    for fx in load() {
        let before = bytes_of(&fx.dir);
        let declared = fx.manifest["declared_features"].as_u64().unwrap();
        // This build admits it.
        admit_logical_features(declared, SUPPORTED_LOGICAL_FEATURES).unwrap();
        // The older one does not, and says which word it refused.
        let older = SUPPORTED_LOGICAL_FEATURES & !VAMANA_FEATURE;
        match admit_logical_features(declared, older) {
            Err(Error::Unsupported(message)) => assert!(
                message.contains(&format!("{declared:#x}")),
                "{}: the refusal must name the feature word: {message}",
                fx.name
            ),
            other => panic!("{}: an unimplemented bit must be Unsupported, got {other:?}", fx.name),
        }
        assert_eq!(bytes_of(&fx.dir), before, "{}: a refusal changed the source", fx.name);
    }
}

/// Writing to a COPY neither promotes nor drops a feature: the reopened
/// database declares exactly the word the fixture did.
#[test]
fn writing_to_a_copy_of_the_corpus_promotes_no_feature_and_keeps_the_graph_whole() {
    for fx in load() {
        let before = bytes_of(&fx.dir);
        let temp = copy(&fx, "write");
        let declared = fx.manifest["declared_features"].as_u64().unwrap();
        let mut db = Database::open(temp.path(), cfg()).unwrap();
        let collection = db
            .collection(fx.manifest["collection"].as_str().unwrap())
            .unwrap()
            .unwrap();
        let index = index_of(&db, &fx.manifest);
        let dimension = fx.manifest["dimension"].as_u64().unwrap() as usize;
        let added: Vec<f32> = (0..dimension).map(|at| (at as f32) * 0.01 - 0.08).collect();
        db.put(
            collection,
            "added-by-the-compat-suite",
            &serde_json::json!({"embedding": added.clone()}),
        )
        .unwrap();
        // And a delete, which is the maintenance step a graph has to survive.
        let removed = fx.manifest["keys"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        assert!(db.delete(collection, &removed).unwrap());
        db.commit().unwrap();
        assert_eq!(logical_features(&db), declared, "a write promoted a feature");
        assert!(db.checkpoint().unwrap());
        drop(db);
        let reopened = Database::open(temp.path(), cfg()).unwrap();
        assert_eq!(logical_features(&reopened), declared);
        // The row that was added is now the nearest neighbour of itself.
        let hits = reopened
            .query_vamana_vector(index, &added, VectorMetric::Cosine, 1, 200, usize::MAX, || false)
            .unwrap()
            .hits;
        assert_eq!(hits.len(), 1);
        drop(reopened);
        let report = verify_indexed_source(temp.path(), VerificationLimits::default(), |issue| {
            panic!("{}: verification issue after writes: {issue:?}", fx.name);
        })
        .unwrap();
        assert!(report.complete && report.clean, "{}: {report:?}", fx.name);
        assert_eq!(bytes_of(&fx.dir), before, "the source corpus was written to");
    }
}

#[test]
#[should_panic(expected = "required preserved vamana fixtures missing")]
fn missing_preserved_fixtures_fail_qualification() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/does-not-exist");
    assert!(
        root.join("INDEX.json").is_file(),
        "required preserved vamana fixtures missing at {}; compatibility qualification cannot skip fixtures",
        root.display()
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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