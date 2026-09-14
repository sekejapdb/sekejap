//! Read a COPY of a refused fixed-work fixture; opening discards uncommitted WAL.
use e4_prototype::pagewal::PageWalStore;
use std::{path::Path, error::Error};
fn main() -> Result<(), Box<dyn Error>> {
    let a: Vec<String> = std::env::args().collect();
    assert_eq!(a.len(), 3);
    let n: u64 = a[2].parse()?;
    let db = PageWalStore::open(Path::new(&a[1]), false, 8 << 20)?;
    let mut count = 0;
    db.scan(|key, value| {
        let id = 2 * count;
        assert_eq!(key, &u64::to_be_bytes(id));
        let mut expected = [b'a' + (id % 26) as u8; 256];
        expected[..8].copy_from_slice(&id.to_le_bytes());
        expected[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(value, &expected);
        count += 1;
        true
    })?;
    assert_eq!(count, n);
    println!("{{\"rows\":{count},\"exact_loaded_state_after_refusal\":true}}");
    Ok(())
}
