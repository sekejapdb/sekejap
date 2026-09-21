//! Graph format admission and corruption boundaries. Database tests run on
//! authorized Linux paths; local development is compile-only on macOS.
use sekejap_core::{
    collections::{
        verification::{verify_indexed_source, VerificationLimits},
        CollectionId, Database, Direction, EdgeKey, EdgeTypeId, EntityId, Error, GraphContextId,
        NeighborRequest,
    },
    pagewal::PageWalStore,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{collections::BTreeMap, fs, io::Write, path::Path};

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn ordered(n: u64) -> Vec<u8> {
    let bytes = n.to_be_bytes();
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(7);
    let mut out = vec![0x80 + (8 - start) as u8];
    out.extend_from_slice(&bytes[start..]);
    out
}

fn edge_storage_key(tag: u8, edge: EdgeKey) -> Vec<u8> {
    fn entity(out: &mut Vec<u8>, id: EntityId) {
        out.extend(ordered(id.collection.0.into()));
        out.extend(ordered(id.sequence));
    }
    let mut out = vec![tag];
    let (first, last) = if tag == 0x71 {
        (edge.source, edge.destination)
    } else {
        (edge.destination, edge.source)
    };
    entity(&mut out, first);
    out.extend(ordered(edge.context.0));
    out.extend(ordered(edge.edge_type.0));
    entity(&mut out, last);
    out
}

fn fixture(path: &Path) -> (CollectionId, EntityId, EntityId, EdgeKey) {
    let mut db = Database::create(path, cfg()).unwrap();
    let c = db
        .create_collection("nodes", vec![], Default::default())
        .unwrap();
    let a = db.put(c, "a", &json!({})).unwrap();
    let b = db.put(c, "b", &json!({})).unwrap();
    db.enable_graph().unwrap();
    let edge = db
        .link(a, "knows", b, "", &json!({"proof":u64::MAX}))
        .unwrap();
    assert_eq!(edge.edge_type, EdgeTypeId(1));
    db.commit().unwrap();
    (c, a, b, edge)
}

fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                out.insert(format!("{name}/"), vec![]);
                visit(root, &path, out);
            } else {
                out.insert(name, fs::read(path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

fn reseal(packet: &mut [u8]) {
    assert_eq!(packet.len(), 2081);
    let end = packet.len() - 4;
    let checksum = crc32c::crc32c(&packet[..end]).to_le_bytes();
    packet[end..].copy_from_slice(&checksum);
}

fn tail_without_coordination(path: &Path) {
    fs::OpenOptions::new()
        .append(true)
        .open(path.join("wal"))
        .unwrap()
        .write_all(b"incomplete-uncommitted-tail")
        .unwrap();
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_string_lossy().starts_with("reader") {
            fs::remove_file(entry.path()).unwrap();
        }
    }
}

fn assert_refused_unchanged(path: &Path, unsupported: bool) {
    let before = files(path);
    for snapshot in [false, true] {
        let result = if snapshot {
            Database::open_snapshot(path, cfg())
        } else {
            Database::open(path, cfg())
        };
        match result {
            Err(Error::Unsupported(_)) if unsupported => {}
            Err(Error::Corrupt(_)) if !unsupported => {}
            Err(error) => panic!("wrong graph refusal, snapshot={snapshot}: {error:?}"),
            Ok(_) => panic!("invalid graph format admitted, snapshot={snapshot}"),
        }
        assert_eq!(files(path), before, "refused open changed graph source");
    }
}

#[test]
fn intact_future_graph_header_or_name_replica_refuses_before_source_mutation() {
    let temp = tempfile::tempdir().unwrap();
    for (family, copy) in (0..3u8).flat_map(|copy| [(0u8, copy), (1u8, copy)]) {
        let path = temp.path().join(format!("future-{family}-{copy}"));
        fixture(&path);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        if family == 0 {
            let key = [0x06, copy];
            let mut header = raw.get(&key).unwrap().unwrap();
            header[10..12].copy_from_slice(&2u16.to_be_bytes());
            reseal(&mut header);
            raw.put(&key, &header).unwrap();
        } else {
            let key = [0x07, 0, copy, 0x81, 1];
            let mut descriptor = raw.get(&key).unwrap().unwrap();
            assert_eq!(&descriptor[..8], b"E4GNM01\0");
            descriptor[6] = b'2';
            reseal(&mut descriptor);
            raw.put(&key, &descriptor).unwrap();
        }
        raw.commit().unwrap();
        drop(raw);
        tail_without_coordination(&path);
        assert_refused_unchanged(&path, true);
    }
}

#[test]
fn graph_rows_cannot_be_hidden_by_clearing_feature_and_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("hidden-edges");
    fixture(&path);
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    for copy in 0..3u8 {
        let logical_key = [0, 0, copy];
        let mut logical = raw.get(&logical_key).unwrap().unwrap();
        logical[18..26].copy_from_slice(&1u64.to_be_bytes());
        reseal(&mut logical);
        raw.put(&logical_key, &logical).unwrap();
        assert!(raw.delete(&[0x06, copy]).unwrap());
        assert!(raw.delete(&[0x07, 0, copy, 0x81, 1]).unwrap());
    }
    let mut lookup = vec![0x12, 0];
    lookup.extend_from_slice(b"knows");
    assert!(raw.delete(&lookup).unwrap());
    raw.commit().unwrap();
    drop(raw);
    tail_without_coordination(&path);
    assert_refused_unchanged(&path, false);
}

#[test]
fn damaged_graph_metadata_replica_falls_back_without_rewriting_source() {
    let temp = tempfile::tempdir().unwrap();
    for copy in 0..3u8 {
        let path = temp.path().join(format!("damaged-{copy}"));
        let (_, a, b, edge) = fixture(&path);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        for key in [vec![0x06, copy], vec![0x07, 0, copy, 0x81, 1]] {
            let mut value = raw.get(&key).unwrap().unwrap();
            value[10] ^= 0x40; // stale checksum: damaged, not future evidence
            raw.put(&key, &value).unwrap();
        }
        raw.commit().unwrap();
        drop(raw);
        let before = files(&path);
        let snapshot = Database::open_snapshot(&path, cfg()).unwrap();
        let found = snapshot
            .neighbors(NeighborRequest {
                entity: a,
                direction: Direction::Outgoing,
                context: GraphContextId::BASE,
                edge_type: Some(edge.edge_type),
                limit: 2,
            })
            .unwrap();
        assert_eq!(found[0].key.destination, b);
        assert_eq!(found[0].properties, json!({"proof":u64::MAX}));
        drop(snapshot);
        assert_eq!(files(&path), before, "metadata fallback rewrote source");

        let mut writer = Database::open(&path, cfg()).unwrap();
        writer
            .put_edge(
                edge.context,
                edge.source,
                edge.edge_type,
                edge.destination,
                &json!({"proof":"updated-through-fallback"}),
            )
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        let reopened = Database::open_snapshot(&path, cfg()).unwrap();
        assert_eq!(
            reopened
                .neighbors(NeighborRequest {
                    entity: a,
                    direction: Direction::Outgoing,
                    context: GraphContextId::BASE,
                    edge_type: Some(edge.edge_type),
                    limit: 2,
                })
                .unwrap()[0]
                .properties,
            json!({"proof":"updated-through-fallback"})
        );
    }
}

/// A traversal no longer cross-checks the other direction of every edge it
/// walks -- that cost one lookup per edge, to guard a state the commit
/// protocol already excludes, since both directions are written in the same
/// transaction. Damage that the read actually needs is still refused; a
/// missing reverse marker is now the verifier's finding, and it is reported.
#[test]
fn edge_damage_fails_the_read_that_needs_it_and_the_verifier_reports_the_rest() {
    let temp = tempfile::tempdir().unwrap();
    for damage in 0..3u8 {
        let path = temp.path().join(format!("edge-damage-{damage}"));
        let (_, a, b, edge) = fixture(&path);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        let primary = edge_storage_key(0x71, edge);
        let reverse = edge_storage_key(0x72, edge);
        match damage {
            0 => assert!(raw.delete(&reverse).unwrap()),
            1 => assert!(raw.delete(&primary).unwrap()),
            2 => raw.put(&primary, &[1, 0xff]).unwrap(),
            _ => unreachable!(),
        }
        raw.commit().unwrap();
        drop(raw);

        // Ordinary admission is metadata-bounded; exact edge damage is found
        // when that adjacency is read. A reverse marker has no properties from
        // which a deleted authoritative primary could be reconstructed.
        let db = Database::open_snapshot(&path, cfg()).unwrap();
        let request = NeighborRequest {
            entity: if damage == 1 { b } else { a },
            direction: if damage == 1 {
                Direction::Incoming
            } else {
                Direction::Outgoing
            },
            context: GraphContextId::BASE,
            edge_type: Some(edge.edge_type),
            limit: 2,
        };
        if damage == 0 {
            // The outgoing read never needed the reverse marker: it answers
            // from the authoritative row it scanned. The verifier owns this.
            let found = db.neighbors(request).unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].key.destination, b);
            drop(db);
            let mut issues = Vec::new();
            let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
                issues.push(issue.clone())
            })
            .unwrap();
            assert!(report.complete && !report.clean);
            assert!(
                issues
                    .iter()
                    .any(|issue| issue.message.contains("graph reverse marker missing")),
                "verifier issues: {:?}",
                issues.iter().map(|i| i.message.clone()).collect::<Vec<_>>()
            );
            continue;
        }
        assert!(matches!(db.neighbors(request), Err(Error::Corrupt(_))));
    }
}
