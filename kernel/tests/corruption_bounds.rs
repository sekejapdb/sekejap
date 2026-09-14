use kernel::graph::Graph;
use kernel::keys;
use kernel::page::{self, PageKind, PAGE_SIZE};
use kernel::store::{Config, Store};
use kernel::Error;

fn forge_oversized_record_length(dir: &std::path::Path) {
    let path = dir.join("data");
    let mut bytes = std::fs::read(&path).unwrap();
    let mut forged = false;
    for page_no in 2..bytes.len() / PAGE_SIZE {
        let base = page_no * PAGE_SIZE;
        let page = &mut bytes[base..base + PAGE_SIZE];
        let kind = u16::from_le_bytes(page[6..8].try_into().unwrap());
        let tree_id = u16::from_le_bytes(page[8..10].try_into().unwrap());
        let n = u16::from_le_bytes(page[10..12].try_into().unwrap());
        if kind != PageKind::Leaf as u16 || tree_id != 1 || n == 0 {
            continue;
        }
        let off = u16::from_le_bytes(page[40..42].try_into().unwrap()) as usize;
        let klen = u16::from_le_bytes(page[off..off + 2].try_into().unwrap()) as usize;
        if cfg!(feature = "compact-cells") && klen & 0xf000 == 0x4000 {
            // Compact values end at the slot boundary. Forge the remaining
            // length field (key length) while retaining the compact tag.
            page[off..off + 2].copy_from_slice(&0x4fffu16.to_le_bytes());
        } else {
            let vlen_at = off + 2 + klen;
            page[vlen_at..vlen_at + 2].copy_from_slice(&((PAGE_SIZE - 1) as u16).to_le_bytes());
        }
        let generation = u64::from_le_bytes(page[24..32].try_into().unwrap());
        page::seal(page, generation);
        forged = true;
        break;
    }
    assert!(forged, "fixture must contain a non-empty data leaf");
    std::fs::write(path, bytes).unwrap();
}

fn damaged_record_store() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut store = Store::create(dir.path(), Config::default()).unwrap();
        store.put(b"key", b"value").unwrap();
        store.commit().unwrap();
        store.checkpoint().unwrap();
    }
    forge_oversized_record_length(dir.path());
    dir
}

#[test]
fn an_oversized_record_length_returns_corruption_on_normal_read() {
    let dir = damaged_record_store();
    let store = Store::open(dir.path(), Config::default()).unwrap();
    assert!(matches!(
        store.get(b"key"),
        Err(Error::Corrupt { .. })
    ));
}

#[test]
fn an_oversized_record_length_returns_corruption_during_recovery() {
    let dir = damaged_record_store();
    assert!(matches!(
        kernel::recover::recover(dir.path(), Config::default()),
        Err(Error::Corrupt { .. })
    ));
}

#[test]
fn a_missing_id_counter_cannot_reuse_an_existing_id() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut graph = Graph::new(Store::create(dir.path(), Config::default()).unwrap()).unwrap();
        assert_eq!(graph.add_node(None, 7, b"original").unwrap(), 1);
        graph.commit().unwrap();
        graph.checkpoint().unwrap();
    }
    let mut store = Store::open(dir.path(), Config::default()).unwrap();
    assert!(store.delete(&keys::catalog(1)).unwrap());
    store.commit().unwrap();
    store.checkpoint().unwrap();

    let mut graph = Graph::new(store).unwrap();
    assert!(matches!(
        graph.add_node(None, 9, b"replacement"),
        Err(Error::Corrupt { .. })
    ));
    let existing = graph.store_ref().get(&keys::node(1)).unwrap().unwrap();
    assert_eq!(&existing[..8], &7u64.to_be_bytes());
    assert_eq!(&existing[8..], b"original", "refusing allocation must leave the live id untouched");
}

#[test]
fn a_malformed_id_counter_refuses_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::create(dir.path(), Config::default()).unwrap();
    store.put(&keys::catalog(1), b"seven").unwrap();
    store.commit().unwrap();
    store.checkpoint().unwrap();
    drop(store);

    let mut graph = Graph::new(Store::open(dir.path(), Config::default()).unwrap()).unwrap();
    assert!(matches!(
        graph.add_node(None, 9, b"must not allocate"),
        Err(Error::Corrupt { .. })
    ));
}

#[test]
fn a_short_node_row_returns_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = Graph::new(Store::create(dir.path(), Config::default()).unwrap()).unwrap();
    graph.store().put(&keys::node(7), b"short").unwrap();

    assert!(matches!(graph.get_node(7), Err(Error::Corrupt { .. })),
            "a node row shorter than its eight-byte label must be corruption");
}

#[test]
fn a_number_list_with_trailing_bytes_returns_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = Graph::new(Store::create(dir.path(), Config::default()).unwrap()).unwrap();
    graph.set_vec(3, 7, &[1.0]).unwrap();
    graph.store().put(&keys::vec_key(3, 7), &[0, 0, 0, 0, 0]).unwrap();

    assert!(matches!(graph.get_vec(3, 7), Err(Error::Corrupt { .. })),
            "a stored f32 list must not silently discard a trailing byte");
}

#[test]
fn a_short_vector_code_row_returns_corruption_during_scan() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = Graph::new(Store::create(dir.path(), Config::default()).unwrap()).unwrap();
    graph.set_vec(3, 7, &[1.0, 2.0, 3.0, 4.0]).unwrap();
    graph.store().put(&keys::vcode_key(3, 7), b"bad").unwrap();

    assert!(matches!(
        graph.nearest(3, &[1.0, 2.0, 3.0, 4.0], 1, kernel::graph::Metric::L2, 1),
        Err(Error::Corrupt { .. })
    ), "a short vector-code row must not look like the end of its field");
}

#[test]
fn a_partial_data_page_refuses_store_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut store = Store::create(dir.path(), Config::default()).unwrap();
        store.put(b"key", b"value").unwrap();
        store.commit().unwrap();
        store.checkpoint().unwrap();
    }
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(dir.path().join("data"))
        .unwrap()
        .write_all(b"x")
        .unwrap();

    assert!(matches!(
        Store::open(dir.path(), Config::default()),
        Err(Error::Corrupt { page_no: 0, .. })
    ));
}
