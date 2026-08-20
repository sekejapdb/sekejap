//! One dimension per field, enforced at the door.
//!
//! Nothing established that before, so a field could hold a 3-dimensional vector
//! and a 5-dimensional one at once. Two things followed. The dense snapshot the
//! HNSW build works from takes its width from the *first* vector and appends
//! each later one at its own length, so the buffer stops lining up with
//! `n * dim` and a read slices past the end. And a query vector of the wrong
//! width aborted the process outright — on ordinary user input, through both the
//! builder and SQL.
//!
//! The distance kernels are the reason it was an abort rather than a wrong
//! answer: they walk to `a.len()` and index `b` at the same offset with
//! unchecked SIMD loads, and the `debug_assert_eq!` guarding that was compiled
//! out of every release build.
//!
//! These go through the public surface, because that is where the input arrives.

use sekejap::CoreDB;

fn seeded(dir: &std::path::Path) -> CoreDB {
    let mut db = CoreDB::open(dir).unwrap();
    for i in 0..8u64 {
        db.put(
            &format!("p/n{i}"),
            &format!(r#"{{"_collection":"p","_key":"n{i}"}}"#),
        )
        .unwrap();
        db.put_vector(&format!("p/n{i}"), "vec", &[i as f32, 1.0, 2.0])
            .unwrap();
    }
    // `VECTOR_NEAR` answers from the HNSW graph, so without one there is nothing
    // to answer from and every query returns empty — which would make the
    // "does not abort" assertions below pass without ever reaching a distance
    // kernel, and the kernel is the thing being tested.
    db.build_hnsw_index("vec", 16, 100).expect("hnsw");
    db
}

#[test]
fn a_vector_of_the_wrong_width_is_refused_not_stored() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = seeded(dir.path());

    // The field's dimension is whatever the first vector established: three.
    assert!(
        db.put_vector("p/n0", "vec", &[1.0, 2.0]).is_err(),
        "a two-dimensional vector was accepted into a three-dimensional field"
    );
    assert!(
        db.put_vector("p/n0", "vec", &[1.0, 2.0, 3.0, 4.0]).is_err(),
        "a four-dimensional vector was accepted into a three-dimensional field"
    );
    // The right width still works, so the guard is not simply refusing everything.
    assert!(db.put_vector("p/n0", "vec", &[9.0, 9.0, 9.0]).is_ok());
}

#[test]
fn a_query_vector_of_the_wrong_width_does_not_abort() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = seeded(dir.path());

    // Whatever these answer, they must *answer*. Aborting the process is the
    // behaviour being tested against, and a test that merely checked the row
    // count would pass by never reaching the kernel.
    for q in [
        "SELECT _key FROM p WHERE VECTOR_NEAR(vec, [1.0], 3)",
        "SELECT _key FROM p WHERE VECTOR_NEAR(vec, [1.0, 2.0], 3)",
        "SELECT _key FROM p WHERE VECTOR_NEAR(vec, [1.0, 2.0, 3.0, 4.0, 5.0], 3)",
    ] {
        let _ = db.query(q).map(|s| s.collect().len());
    }

    // And the correctly-sized query still works.
    let n = db
        .query("SELECT _key FROM p WHERE VECTOR_NEAR(vec, [1.0, 1.0, 2.0], 3)")
        .unwrap()
        .collect()
        .len();
    assert!(n > 0, "a correctly-sized vector query returned nothing");
}

#[test]
fn a_mismatched_vector_survives_a_compaction_and_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = seeded(dir.path());
    let _ = db.put_vector("p/n0", "vec", &[1.0, 2.0]); // refused
    db.compact().unwrap();
    drop(db);

    let db = CoreDB::open(dir.path()).unwrap();
    let n = db
        .query("SELECT _key FROM p WHERE VECTOR_NEAR(vec, [1.0, 1.0, 2.0], 3)")
        .unwrap()
        .collect()
        .len();
    assert!(n > 0, "vector search stopped working after a refused write");
}
