//! Immutable fixture generator for the VAMANA GRAPH family (keyspace `0x7D`,
//! logical feature bit `0x8000`).
//!
//! `docs/core/FORMAT_V2.md` "Extension boundary" requires an immutable
//! fixture of every family that ships. This writes one: two independently
//! captured corpora of the same logical content -- the 16-byte database
//! identity is drawn fresh for each created database, so the two are
//! captures and not copies -- at the two WAL boundaries the format has.
//!
//! The MANIFEST is the ORACLE and holds no engine readback. The vectors are
//! generated here, the expected nearest neighbours are computed here by brute
//! force from those f32 lanes, and `core/engine/tests/format_vamana_compat.rs`
//! compares the engine against those numbers.
//!
//! ```sh
//! cargo run -p sekejap-bench --release --bin vamana_format_fixture -- docs/format-v2-vamana
//! ```
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        CollectionOptions, Database, VectorMetric, SUPPORTED_LOGICAL_FEATURES,
        VAMANA_ADJACENCY, VAMANA_ALPHA_HUNDREDTHS, VAMANA_BUILD_SEARCH_LIST, VAMANA_DEGREE,
        VAMANA_ENTRY, VAMANA_FEATURE, VAMANA_GRAPH_VERSION,
    },
    internal::logical_features,
    Kind,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process,
};

const CACHE: usize = 1 << 20;
const DIM: usize = 16;
const ROWS: usize = 300;
const QUERIES: usize = 8;
const K: usize = 5;
/// The search list the manifest's expectation is stated at. Recall is a
/// property of the family, not of the fixture: the compat test asserts the
/// graph returns the exact top-k at this list over this corpus, which it does
/// because the corpus is small enough for the list to cover it.
const EF: usize = 300;

fn cfg() -> Config {
    Config {
        budget_bytes: CACHE,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn vector(&mut self) -> Vec<f32> {
        (0..DIM)
            .map(|_| ((self.next() >> 11) as f32 / (1u64 << 53) as f32) * 2.0 - 1.0)
            .collect()
    }
}

fn cosine(stored: &[f32], query: &[f32]) -> f64 {
    let wide = |v: &[f32]| v.iter().map(|l| f64::from(*l)).collect::<Vec<f64>>();
    let (s, q) = (wide(stored), wide(query));
    let dot: f64 = s.iter().zip(&q).map(|(a, b)| a * b).sum();
    let sn: f64 = s.iter().map(|a| a * a).sum();
    let qn: f64 = q.iter().map(|b| b * b).sum();
    1.0 - dot / (sn.sqrt() * qn.sqrt())
}

/// SHA-256, the same implementation `bench/src/bin/format_fixture.rs`
/// carries: this workspace has no hash dependency and a fixture index that
/// pins checksums cannot borrow one.
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

fn file_records(dir: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    names.sort();
    for path in names {
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name == "MANIFEST.json" {
            continue;
        }
        let bytes = fs::read(&path).unwrap();
        out.push(json!({"name": name, "bytes": bytes.len(), "sha256": hex(&sha256(&bytes))}));
    }
    out
}

fn write_fixture(dir: &Path, seed: u64, checkpointed: bool) -> Value {
    fs::create_dir_all(dir).unwrap();
    let mut rng = Rng(seed);
    let vectors: Vec<Vec<f32>> = (0..ROWS).map(|_| rng.vector()).collect();
    let mut db = Database::create(dir, cfg()).unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![("embedding".into(), Kind::Vector(DIM))],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut held: BTreeMap<u64, Vec<f32>> = BTreeMap::new();
    let mut keys: BTreeMap<String, u64> = BTreeMap::new();
    for (at, vector) in vectors.iter().enumerate() {
        let key = format!("p{at:05}");
        let id = db
            .put(collection, &key, &json!({"embedding": vector}))
            .unwrap();
        held.insert(id.sequence, vector.clone());
        keys.insert(key, id.sequence);
    }
    db.commit().unwrap();
    let index = db
        .create_vamana_index(collection, "embedding_vamana", "embedding")
        .unwrap();
    db.commit().unwrap();
    while !db.build_index_step(index, 64).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    let features = logical_features(&db);
    assert_ne!(features & VAMANA_FEATURE, 0, "the fixture must declare 0x8000");
    if checkpointed {
        assert!(db.checkpoint().unwrap(), "checkpoint deferred");
    }
    drop(db);

    // The oracle: brute force, here, from the lanes this generator made.
    let mut rng = Rng(seed ^ 0xa5a5_a5a5_a5a5_a5a5);
    let mut expectations = Vec::new();
    for _ in 0..QUERIES {
        let query = rng.vector();
        let mut scored: Vec<(f64, u64)> = held
            .iter()
            .map(|(seq, stored)| (cosine(stored, &query), *seq))
            .collect();
        scored.sort_by(|l, r| l.0.total_cmp(&r.0).then_with(|| l.1.cmp(&r.1)));
        expectations.push(json!({
            "query": query,
            "nearest": scored.iter().take(K).map(|(_, seq)| *seq).collect::<Vec<_>>(),
        }));
    }

    let manifest = json!({
        "format": "sekejap-disk-format-v2",
        "family": "vamana_graph",
        "keyspace_tag": VAMANA_ENTRY,
        "adjacency_keyspace_tag": VAMANA_ADJACENCY,
        "feature_bit": VAMANA_FEATURE,
        "supported_logical_features": SUPPORTED_LOGICAL_FEATURES,
        "graph": {
            "version": VAMANA_GRAPH_VERSION,
            "degree": VAMANA_DEGREE,
            "build_search_list": VAMANA_BUILD_SEARCH_LIST,
            "alpha_hundredths": VAMANA_ALPHA_HUNDREDTHS,
        },
        "generator": {"source": "bench/src/bin/vamana_format_fixture.rs", "seed": seed},
        "collection": "points",
        "field": "embedding",
        "index": "embedding_vamana",
        "dimension": DIM,
        "rows": ROWS,
        "metric": "cosine",
        "k": K,
        "ef": EF,
        "checkpointed": checkpointed,
        "declared_features": features,
        "keys": keys,
        "vectors": held,
        "expectations": expectations,
        "files": file_records(dir),
    });
    fs::write(
        dir.join("MANIFEST.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let _ = VectorMetric::Cosine;
    json!({
        "name": dir.file_name().unwrap().to_string_lossy(),
        "checkpointed": checkpointed,
        "rows": ROWS,
        "manifest_sha256": hex(&sha256(&fs::read(dir.join("MANIFEST.json")).unwrap())),
    })
}

fn run(out: &Path) {
    if out.exists() {
        fs::remove_dir_all(out).unwrap();
    }
    fs::create_dir_all(out).unwrap();
    let fixtures = vec![
        write_fixture(&out.join("checkpointed"), 0x7d80_0000_1111_2222, true),
        write_fixture(&out.join("wal-pending"), 0x7d80_0000_3333_4444, false),
    ];
    let index = json!({
        "format": "sekejap-disk-format-v2",
        "family": "vamana_graph",
        "generator": {"source": "bench/src/bin/vamana_format_fixture.rs"},
        "fixtures": fixtures,
    });
    fs::write(
        out.join("INDEX.json"),
        serde_json::to_vec_pretty(&index).unwrap(),
    )
    .unwrap();
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(out) = args.first() else {
        eprintln!("usage: vamana_format_fixture OUT_DIR");
        process::exit(2);
    };
    run(&PathBuf::from(out));
}
