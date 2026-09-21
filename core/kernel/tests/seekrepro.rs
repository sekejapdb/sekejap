//! A range seek that starts BETWEEN two keys inside a grafted subtree must
//! land on the first key >= the seek target — never on a fabricated or
//! truncated record, and never on anything that sorts below the target.

fn key26(h: u64, id: u64) -> Vec<u8> {
    let mut k = vec![0x0Fu8];
    k.extend_from_slice(&7u64.to_be_bytes());
    k.push(12);
    k.extend_from_slice(&h.to_be_bytes());
    k.extend_from_slice(&id.to_be_bytes());
    k
}

#[test]
fn seek_between_grafted_keys_lands_on_the_next_key() {
    let d = tempfile::TempDir::new().unwrap();
    let cfg = kernel::store::Config { budget_bytes: 8 << 20, ..Default::default() };
    let mut st = kernel::store::Store::create(d.path(), cfg).unwrap();
    // Populate other keyspaces the way a real database is populated before a
    // late index build grafts its postings in.
    for i in 0..200u64 {
        let mut k = vec![0x01u8];
        k.extend_from_slice(&(i * 977).to_be_bytes());
        st.put(&k, format!("row {i}").as_bytes()).unwrap();
    }
    st.commit().unwrap();

    // Graft exactly two postings, like a tiny spatial build.
    let rows = [key26(13031203, 444), key26(13031545, 888)];
    let scratch = d.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    st.graft_sorted_range(
        rows.iter().map(|k| Ok((k.clone(), b"v".to_vec(), false))),
        2,
        rows[0].clone(),
        rows[1].clone(),
        &scratch,
    ).unwrap();
    st.commit().unwrap();

    // Seek between the two grafted keys.
    let mut from = vec![0x0Fu8];
    from.extend_from_slice(&7u64.to_be_bytes());
    from.push(12);
    from.extend_from_slice(&13031422u64.to_be_bytes());
    let mut first: Option<Vec<u8>> = None;
    st.scan(&from).unwrap().for_each_ref(|k, _| { first = Some(k.to_vec()); false }).unwrap();
    let got = first.expect("scan found something");
    assert!(got.as_slice() >= from.as_slice(),
        "scan(from) returned a key BELOW the seek target: len {} first bytes {:?}",
        got.len(), &got[..got.len().min(12)]);
    assert_eq!(got, rows[1], "must land on the first key >= from");
}
