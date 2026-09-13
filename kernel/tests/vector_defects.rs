//! DEFECT DASHBOARD (kernel, vector tier).
//!
//! `#[ignore = "DEFECT: ..."]` marks an open defect; `cargo test -p kernel --
//! --ignored` lists them. Closing one means deleting the attribute.

use kernel::graph::{Graph, Metric};
use kernel::store::{Config, Store};

const F: u64 = 1; // one vector field for these tests

fn graph() -> (tempfile::TempDir, Graph) {
    let d = tempfile::TempDir::new().unwrap();
    let g = Graph::new(Store::create(d.path(), Config::default()).unwrap()).unwrap();
    (d, g)
}

#[test]
fn a_vector_stored_with_a_lower_id_after_a_fold_is_still_found() {
    // Ids are the caller's to choose -- `set_vec` accepts any u64, and a
    // database that assigns ids by content hash, or reuses a slot, will hand
    // out a lower one at any time. The fold's watermark treats "new" as
    // "greater", so anything below it is skipped forever.
    let (_d, mut g) = graph();
    g.set_vec(F, 100, &[10.0]).unwrap();
    g.commit().unwrap();
    g.fold_nav(F).unwrap();

    g.set_vec(F, 50, &[1.0]).unwrap();
    g.commit().unwrap();
    g.fold_nav(F).unwrap();

    // [1.0] is an exact match for id 50 and far from id 100
    let hits = g.nearest_nav(F, &[1.0], 1, Metric::L2, 1).unwrap();
    assert_eq!(hits.first().map(|(id, _)| *id), Some(50),
               "the exact match must win; got {hits:?}");
}

/// PASSES at the default 2-bit width. The mis-decode the audit found is only
/// reachable on a field stored at 4 bits, which today only a bulk load
/// produces -- so this is the guard for the ordinary path, and the 4-bit
/// path stays listed as an open defect until a test can construct one.
#[test]
fn a_field_stored_at_four_bits_is_decoded_at_four_bits() {
    // The row length is computed from the field's persisted `bits`, but the
    // decode always uses the 2-bit path -- so a 4-bit field is misread and
    // half of every code is ignored, making unrelated vectors tie.
    let (_d, mut g) = graph();
    let dim = 64;
    let mk = |seed: f32| -> Vec<f32> { (0..dim).map(|i| seed + i as f32 * 0.001).collect() };
    for id in 1..=80u64 {
        g.set_vec(F, id, &mk(id as f32)).unwrap();
    }
    g.commit().unwrap();
    g.fold_nav(F).unwrap();
    let target = mk(42.0);
    let hits = g.nearest_nav(F, &target, 1, Metric::L2, 4).unwrap();
    assert_eq!(hits.first().map(|(id, _)| *id), Some(42),
               "an exact match must be found regardless of the field's bit width; got {hits:?}");
}
