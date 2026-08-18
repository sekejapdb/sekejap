//! Approximate vector search against exact search.
//!
//! `VECTOR_NEAR` answers from an HNSW graph when one has been built and from a
//! brute-force scan when one has not. The graph is an *approximation*, so the two
//! are not required to be identical in general — but on a few hundred vectors
//! with ordinary parameters they should agree completely, and a graph that has
//! stopped finding neighbours degrades silently: the query still returns k rows,
//! they are simply the wrong ones.
//!
//! There was no test of that at all. It matters more than usual just now, because
//! the heap ordering behind the search was changed in this branch — `partial_cmp`
//! collapsed a NaN distance to "equal", which is not a consistent `Ord` and
//! breaks the invariant `BinaryHeap` relies on. Swapping it for `total_cmp` is
//! the sort of change that could quietly cost recall, and nothing would have
//! said so.

use sekejap::CoreDB;
use serde_json::json;

/// Deterministic pseudo-random vectors — no dev-dependency, reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 { (self.next() % 10_000) as f32 / 10_000.0 }
}

const DIM: usize = 16;
const ROWS: usize = 300;
const K: usize = 10;

fn fixture(dir: &std::path::Path, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    let mut db = CoreDB::open(dir).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
    let mut vectors = Vec::with_capacity(ROWS);
    for i in 0..ROWS {
        db.put(&format!("p/n{i}"),
               &json!({"_collection": "p", "_key": format!("n{i}"), "n": i as i64}).to_string())
          .unwrap();
        let v: Vec<f32> = (0..DIM).map(|_| rng.unit()).collect();
        db.put_vector(&format!("p/n{i}"), "emb", &v).unwrap();
        vectors.push(v);
    }
    db.compact().unwrap();
    vectors
}

fn near(db: &CoreDB, q: &[f32], k: usize) -> Vec<String> {
    let list = q.iter().map(|f| format!("{f:.6}")).collect::<Vec<_>>().join(",");
    let sql = format!("SELECT _key FROM p WHERE VECTOR_NEAR(emb, [{list}], {k})");
    db.query(&sql)
        .unwrap_or_else(|e| panic!("`{sql}` did not run: {e:?}"))
        .collect()
        .iter()
        .map(|h| h.slug.clone())
        .collect()
}

#[test]
fn hnsw_finds_what_the_exact_scan_finds() {
    let seed = 0x5EED_1234;
    let dir_plain = tempfile::TempDir::new().unwrap();
    let dir_hnsw = tempfile::TempDir::new().unwrap();
    let vectors = fixture(dir_plain.path(), seed);
    let _ = fixture(dir_hnsw.path(), seed);        // identical data, same seed

    let plain = CoreDB::open(dir_plain.path()).unwrap();
    let mut indexed = CoreDB::open(dir_hnsw.path()).unwrap();
    indexed.build_hnsw_index("emb", 16, 200).expect("building the HNSW graph failed");

    // Otherwise both sides are brute force and this compares a scan with itself.
    assert_eq!(indexed.stats().hnsw_indexes, 1,
               "no HNSW graph exists on the indexed side, so the comparison is                 between two exact scans and proves nothing");
    assert_eq!(plain.stats().hnsw_indexes, 0,
               "the plain side has an HNSW graph, so it is not the exact scan this                 is meant to compare against");

    let mut rng = Rng(seed ^ 0xABCD);
    let mut checked = 0;
    for round in 0..25 {
        // Query with a stored vector and with a fresh one: an exact hit and a
        // point that is not in the set are different cases for a graph walk.
        let q: Vec<f32> = if round % 2 == 0 {
            vectors[(rng.next() as usize) % ROWS].clone()
        } else {
            (0..DIM).map(|_| rng.unit()).collect()
        };

        let want = near(&plain, &q, K);
        let got = near(&indexed, &q, K);
        assert_eq!(want.len(), K, "the exact scan returned {} rows, not {K}", want.len());
        assert_eq!(got.len(), K, "the graph returned {} rows, not {K}", got.len());
        assert_eq!(got, want,
            "round {round}: the HNSW graph and the exact scan disagree on the {K} \
             nearest.\n  exact = {want:?}\n  graph = {got:?}");
        checked += 1;
    }
    assert_eq!(checked, 25, "the loop did not run");
}

/// A vector holding NaN must not take the search down with it. L2, dot and L1
/// propagate NaN straight through, and the heap ordering used to compare a NaN
/// distance as equal to everything — a cyclic `Ord`, which breaks the heap
/// itself rather than merely misordering the answer.
#[test]
fn a_nan_vector_does_not_break_the_search() {
    let dir = tempfile::TempDir::new().unwrap();
    let vectors = fixture(dir.path(), 0xBEEF);
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.put_vector("p/n0", "emb", &vec![f32::NAN; DIM]).unwrap();
    db.build_hnsw_index("emb", 16, 200).expect("building over a NaN vector failed");

    // The query must terminate, return k rows, and never panic.
    let got = near(&db, &vectors[7], K);
    assert_eq!(got.len(), K,
               "a single NaN vector left the search returning {} rows instead of {K}",
               got.len());
    // And the healthy rows are still findable: querying a stored vector should
    // return that row itself.
    assert!(got.contains(&"p/n7".to_string()),
            "querying a row's own vector did not return that row once a NaN vector \
             was present: {got:?}");
}

/// **A field holds one dimension, and a query of another width is refused.**
///
/// Nothing established a field's dimension, so a 3-dimensional field accepted a
/// 5-dimensional vector without complaint. The dense buffer the search reads is
/// `n * dim` wide and the SIMD kernels walk one slice's length while indexing the
/// other, so a query of the wrong width **aborted the process** — from
/// `vector_near` and from `WHERE VECTOR_NEAR(..)` alike, on ordinary user input.
///
/// Three surfaces, three answers, none of them a crash:
/// * `put_vector` refuses the mismatched write, naming both widths
/// * SQL refuses the query, naming both widths
/// * the builder API returns no rows — it hands back rows rather than a `Result`,
///   and nothing can match a vector of a different width
#[test]
fn a_vector_of_the_wrong_width_is_refused_rather_than_fatal() {
    use sekejap::SqlError;

    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY)").unwrap();
    for i in 0..8 {
        db.put(&format!("p/n{i}"),
               &json!({"_collection": "p", "_key": format!("n{i}")}).to_string()).unwrap();
        db.put_vector(&format!("p/n{i}"), "vec", &[i as f32, 1.0, 2.0]).unwrap();
    }

    // The write.
    let err = db.put_vector("p/n0", "vec", &[1.0, 2.0, 3.0, 4.0, 5.0])
        .expect_err("a 5-dimensional vector must not join a 3-dimensional field");
    assert!(err.to_string().contains('3') && err.to_string().contains('5'),
        "the error must name both widths: {err}");

    db.build_hnsw_index("vec", 16, 100).expect("hnsw");

    // The right width still answers, on both surfaces.
    assert_eq!(db.collection("p").vector_near("vec", vec![1.0, 1.0, 2.0], 5).collect().len(), 5);
    assert_eq!(db.query("SELECT _key FROM p WHERE VECTOR_NEAR(vec, [1.0,1.0,2.0], 5)")
                   .unwrap().collect().len(), 5);

    // The wrong width: refused by SQL, empty from the builder, fatal from neither.
    match db.query("SELECT _key FROM p WHERE VECTOR_NEAR(vec, [1.0,1.0,2.0,3.0,4.0], 5)") {
        Err(SqlError::VectorDimensionMismatch { field_dim, query_dim, .. }) => {
            assert_eq!((field_dim, query_dim), (3, 5));
        }
        Err(other) => panic!("wrong error for a mismatched vector: {other:?}"),
        Ok(set) => panic!("a 5-dimensional query matched {} row(s) in a \
                          3-dimensional field", set.collect().len()),
    }
    assert_eq!(db.collection("p").vector_near("vec", vec![1.0, 1.0, 2.0, 3.0, 4.0], 5)
                   .collect().len(), 0,
        "nothing can match a vector of a different width");
}
