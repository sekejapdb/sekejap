//! Preserved failing fixture from the page-WAL qualification control.
//! This uses the production kernel reader, not the experimental pager.
#[cfg(all(feature = "sqlite-balance", feature = "compact-cells"))]
#[test]
#[ignore = "Known persisted scan/get disagreement; retained qualification fixture required"]
fn accepted_control_scan_matches_point_reads_after_six_mixed_rounds() {
    use kernel::{btree::BTree, budget::MemoryBudget, io::open_recovery_source,
        meta::Meta, pool::BufferPool};
    use std::{cell::Cell, path::PathBuf, sync::Arc};
    let Some(path) = std::env::var_os("E4_CONTROL_FIXTURE").map(PathBuf::from) else {
        eprintln!("skipping: set E4_CONTROL_FIXTURE to the retained qualification database directory");
        return;
    };
    let pool = BufferPool::new(open_recovery_source(&path.join("data")).unwrap().into(),
        Arc::new(MemoryBudget::new(8 << 20)), 2048).unwrap();
    let meta = Meta::read_latest(&pool).unwrap();
    let (last, hits, attempts) = (Cell::new(None), Cell::new(0), Cell::new(0));
    let tree = BTree::open(&pool, 1, meta.roots[0], &last, &hits, &attempts);
    let point = tree.get(&395080u64.to_be_bytes()).unwrap().unwrap();
    assert_eq!(u64::from_le_bytes(point[8..16].try_into().unwrap()), 6);
    let (mut rows, mut wrong, mut ordering_errors, mut previous) = (0u64, 0u64, 0u64, None);
    tree.range(&[]).unwrap().for_each_ref(|k, v| {
        let id = u64::from_be_bytes(k.try_into().unwrap());
        if previous.is_some_and(|p| id <= p) { ordering_errors += 1; }
        previous = Some(id);
        let (slot, version, allowed) = if id < 400000 {
            (id, if id % 5 == 0 { 6 } else { 0 }, id % 10 != 1)
        } else { ((id - 400000) % 40000 * 10 + 1, 6, (id - 400000) / 40000 == 5) };
        let mut expected = vec![b'a' + ((slot + version) % 26) as u8; 256];
        expected[..8].copy_from_slice(&slot.to_le_bytes());
        expected[8..16].copy_from_slice(&version.to_le_bytes());
        if !allowed || v != expected { wrong += 1; }
        rows += 1;
        rows <= 800000
    }).unwrap();
    assert_eq!((rows, wrong, ordering_errors), (400000, 0, 0),
        "persisted scan disagrees with independently generated round-6 rows");
}
