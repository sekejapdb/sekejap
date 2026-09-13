//! After a graft is installed, the PUBLISHED tree must still be structurally
//! sound — every separator inside its parent's range, every level ordered,
//! the leaf chain intact.
//!
//! The candidate subtree is verified before publication, but `install_graft`
//! then rewrites the parent path, and nothing re-read that. A boundary
//! anchored one child left of a stale separator produced a tree whose root
//! disagreed with its own leaves: full scans saw the keys, mid-range seeks
//! skipped them. This walks the whole published tree after the exact sequence
//! that produced it — build, delete the range, rebuild — and would have caught
//! that defect at its source rather than through a wrong query answer.

fn key(space: u8, h: u64, id: u64) -> Vec<u8> {
    let mut k = vec![0x0Fu8];
    k.extend_from_slice(&7u64.to_be_bytes());
    k.push(space);
    k.extend_from_slice(&h.to_be_bytes());
    k.extend_from_slice(&id.to_be_bytes());
    k
}

#[test]
fn a_regrafted_range_leaves_the_published_tree_sound() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = kernel::store::Config { budget_bytes: 8 << 20, ..Default::default() };
    let mut store = kernel::store::Store::create(dir.path(), cfg).unwrap();

    // Ordinary rows first, so the grafted range lands mid-tree.
    for i in 0..300u64 {
        let mut k = vec![0x01u8];
        k.extend_from_slice(&(i * 7919).to_be_bytes());
        store.put(&k, format!("row {i}").as_bytes()).unwrap();
    }
    store.commit().unwrap();

    let scratch = dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let rows: Vec<Vec<u8>> = (0..64u64).map(|i| key(12, 13_031_000 + i * 13, i)).collect();

    // Build, drop the whole range, then build the SAME range again — the
    // rebuild shape that stranded keys under a stale separator.
    for round in 0..2 {
        if round > 0 {
            let mut prefix = vec![0x0Fu8];
            prefix.extend_from_slice(&7u64.to_be_bytes());
            prefix.push(12);
            store.delete_prefix(&prefix).unwrap();
            store.commit().unwrap();
        }
        store.graft_sorted_range(
            rows.iter().map(|k| Ok((k.clone(), b"v".to_vec(), false))),
            rows.len() as u64,
            rows[0].clone(),
            rows[rows.len() - 1].clone(),
            &scratch,
        ).unwrap();
        store.commit().unwrap();
        store.checkpoint().unwrap();

        kernel::verify::verify_published_tree(
            &dir.path().join("data"), store.io_mode(),
            store.published_root(), store.main_tree_id(),
        ).unwrap_or_else(|e| panic!("round {round}: published tree is unsound after graft: {e}"));

        // And the structure must agree with the answers: a seek starting
        // between two grafted keys lands on the next one, not before it.
        let mut from = vec![0x0Fu8];
        from.extend_from_slice(&7u64.to_be_bytes());
        from.push(12);
        from.extend_from_slice(&(13_031_000u64 + 32 * 13 - 5).to_be_bytes());
        let mut first: Option<Vec<u8>> = None;
        store.scan(&from).unwrap().for_each_ref(|k, _| { first = Some(k.to_vec()); false }).unwrap();
        assert_eq!(first.as_deref(), Some(rows[32].as_slice()),
            "round {round}: a mid-range seek did not land on the next grafted key");
    }
}
