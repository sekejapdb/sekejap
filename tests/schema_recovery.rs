use e4_prototype::{recovery::*, *};
use kernel::{
    io::IoMode,
    page::{PageKind, PageRef},
    store::{Config, Store, SyncMode},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn key(id: u16) -> Vec<u8> {
    [vec![0x82], id.to_be_bytes().to_vec()].concat()
}
fn catalog(id: u64, copy: u64) -> Vec<u8> {
    [vec![0, 240], (id * 3 + copy).to_be_bytes().to_vec()].concat()
}
fn layout(id: u64) -> Layout {
    let mut fields = vec![
        ("name".into(), Kind::Text),
        ("age".into(), Kind::Int),
        ("active".into(), Kind::Bool),
        ("position".into(), Kind::Point),
        ("profile".into(), Kind::Json),
    ];
    if id == 2 {
        fields.push(("created".into(), Kind::Int));
    }
    Layout { id, fields }
}
fn document(id: u16) -> Value {
    let mut d = json!({"name":format!("Person 東京 {id}"),"age":i64::from(id % 100)-10,
        "active":id%2==0,"position":{"type":"Point","coordinates":[(id%144) as f64 / 4.0,-((id%80) as f64) / 4.0]},
        "profile":{"languages":["id","en"],"nested":[null,true,{"score":id%7}]},"extra":{"large":u64::MAX}});
    if id % 7 == 0 {
        d["age"] = Value::Null;
    }
    if id % 2 == 0 {
        d["created"] = json!(1_788_888_888u64);
    }
    if id % 100 == 0 {
        d["profile"]["note"] = json!("observation-é-".repeat(600));
    }
    d
}
fn create(source: &Path, n: u16) {
    let mut s = Store::create(source, cfg()).unwrap();
    let descriptors = (1..=2).flat_map(|id| {
        (0..3).map(move |copy| (catalog(id, copy), layout(id).descriptor().unwrap()))
    });
    let rows = (1..=n).map(|id| {
        (
            key(id),
            encode_dense_v3(&layout(1 + u64::from(id % 2 == 0)), &document(id))
                .unwrap()
                .row,
        )
    });
    s.bulk_load(descriptors.chain(rows)).unwrap();
    s.checkpoint().unwrap();
}
fn source_bytes(p: &Path) -> Vec<Vec<u8>> {
    ["data", "wal", "free"]
        .iter()
        .map(|n| fs::read(p.join(n)).unwrap_or_default())
        .collect()
}

// Fixture inspection deliberately does not use the production recovery scanner.
fn cells(page: &[u8], no: u32) -> Vec<(Vec<u8>, Vec<u8>)> {
    let p = PageRef::open(page, no).unwrap();
    if p.kind() != PageKind::Leaf {
        return vec![];
    }
    (0..p.nentries())
        .map(|i| {
            let r = p.slot(i);
            if r[0] == 255 {
                let n = (r[1] - 128) as usize;
                (r[1..2 + n].to_vec(), r[2 + n..].to_vec())
            } else if cfg!(feature = "compact-cells") && r[1] & 0xf0 == 0x40 {
                let n = (u16::from_le_bytes(r[..2].try_into().unwrap()) & 0x0fff) as usize;
                (r[2..2 + n].to_vec(), r[2 + n..].to_vec())
            } else {
                let n = u16::from_le_bytes(r[..2].try_into().unwrap()) as usize;
                (r[2..2 + n].to_vec(), r[4 + n..].to_vec())
            }
        })
        .collect()
}
fn damage(source: &Path, case: &str) -> BTreeSet<u16> {
    let path = source.join("data");
    let mut bytes = fs::read(&path).unwrap();
    let mut lost_rows = BTreeSet::new();
    let mut per_layout = [0; 3];
    for (no, page) in bytes.chunks_exact_mut(4096).enumerate() {
        let p = PageRef::open(page, no as u32).unwrap();
        if p.kind() == PageKind::Interior {
            page[50] ^= 1;
            continue;
        }
        if p.kind() != PageKind::Leaf || case == "ancestors" {
            continue;
        }
        let entries = cells(page, no as u32);
        let descriptor = entries
            .iter()
            .find(|(k, _)| DenseV3.classify(k) == RecordClass::Layout);
        if let Some((_, b)) = descriptor {
            let id = Layout::from_descriptor(b).unwrap().id as usize;
            per_layout[id] += 1;
            let count = match case {
                "one-copy" => 1,
                "two-copies" => 2,
                "all-copies" => 3,
                _ => panic!("case"),
            };
            if per_layout[id] <= count {
                for (k, _) in &entries {
                    if DenseV3.classify(k) == RecordClass::Entity {
                        lost_rows.insert(u16::from_be_bytes(k[1..].try_into().unwrap()));
                    }
                }
                page[50] ^= 1;
            }
        }
    }
    fs::write(path, bytes).unwrap();
    lost_rows
}
fn case_dir(case: &str, n: u16) -> (PathBuf, Option<tempfile::TempDir>) {
    if let Ok(base) = std::env::var("E4_SCHEMA_ARTIFACTS") {
        let p = PathBuf::from(base).join(format!("{case}-{n}"));
        assert!(p.starts_with("<scratch>")
            || p.starts_with("<scratch>")
            || p.starts_with("<scratch>"));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::create_dir(&p).unwrap();
        (p, None)
    } else {
        let tmp = std::env::temp_dir();
        assert!(tmp.starts_with("<scratch>")
            || tmp.starts_with("<scratch>")
            || tmp.starts_with("<scratch>"));
        let t = tempfile::tempdir().unwrap();
        (t.path().to_path_buf(), Some(t))
    }
}
fn run(case: &str, n: u16) {
    let (d, _temp) = case_dir(case, n);
    let source = d.join("source");
    create(&source, n);
    let lost = damage(&source, case);
    let before = source_bytes(&source);
    let started = Instant::now();
    let report = recover_typed_candidates(
        &source,
        &d.join("recovery"),
        &DenseV3,
        RecoveryOptions::default(),
    )
    .unwrap();
    let elapsed = started.elapsed().as_secs_f64();
    assert_eq!(source_bytes(&source), before);
    let survivors = u64::from(n) - lost.len() as u64;
    assert_eq!(report.raw_records, survivors);
    let missing = case == "all-copies";
    assert_eq!(report.layouts, if missing { 0 } else { 2 });
    assert_eq!(
        report.missing_layout_records,
        if missing { survivors } else { 0 }
    );
    assert_eq!(report.decoded_records, if missing { 0 } else { survivors });
    assert_eq!(report.unresolved_records, 0);
    let mut found = BTreeSet::new();
    for line in fs::read_to_string(d.join("recovery/decoded.jsonl"))
        .unwrap()
        .lines()
    {
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["membership"], "candidate");
        let id = u16::from_str_radix(
            v["key_hex"].as_str().unwrap().strip_prefix("82").unwrap(),
            16,
        )
        .unwrap();
        assert!(found.insert(id));
        assert!(!lost.contains(&id));
        assert_eq!(v["document"], document(id));
    }
    if !missing {
        assert_eq!(found.len() as u64, survivors);
    }
    // Even with every descriptor lost, the archive must retain full overflow
    // values and exact typed bytes. The TEST knows the original schema; recovery
    // itself must not invent it, and exports zero decoded documents in that case.
    let raw_count = visit_raw_records(&d.join("recovery/records.raw"), 1 << 20, |r| {
        assert!(!r.overflow_marker);
        let id = u16::from_be_bytes(r.key[1..].try_into()?);
        assert!(!lost.contains(&id));
        assert_eq!(
            decode_dense_v3(&layout(1 + u64::from(id % 2 == 0)), &r.value, |_| Err(
                "no vectors".into()
            ))?,
            document(id)
        );
        Ok(())
    })
    .unwrap();
    assert_eq!(raw_count, survivors);
    let result = json!({"case":case,"input_rows":n,"source_data_bytes":before[0].len(),
        "physical_page_losses_known_by_fixture":lost,"raw_records":report.raw_records,"decoded_records":report.decoded_records,
        "missing_layout_records":report.missing_layout_records,"damaged_pages":report.damaged_pages,
        "layouts":report.layouts,"elapsed_seconds":elapsed,"source_unchanged":true});
    fs::write(
        d.join("result.json"),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
    println!("{result}");
}
#[test]
fn layouts_recover_without_btree_ancestors() {
    for n in [10_000, 40_000] {
        run("ancestors", n);
    }
}
#[test]
fn surviving_layout_replicas_decode_exact_values() {
    for n in [10_000, 40_000] {
        for case in ["one-copy", "two-copies"] {
            run(case, n);
        }
    }
}
#[test]
fn missing_layout_preserves_raw_rows_and_names_loss() {
    for n in [10_000, 40_000] {
        run("all-copies", n);
    }
}

fn change_descriptor(source: &Path, target: &[u8], change: impl FnOnce(&mut [u8])) {
    let path = source.join("data");
    let mut bytes = fs::read(&path).unwrap();
    let mut change = Some(change);
    for (no, page) in bytes.chunks_exact_mut(4096).enumerate() {
        let p = PageRef::open(page, no as u32).unwrap();
        if p.kind() != PageKind::Leaf {
            continue;
        }
        let entries = cells(page, no as u32);
        if let Some(i) = entries.iter().position(|(k, _)| k == target) {
            let record = p.slot(i);
            let offset = record.as_ptr() as usize - page.as_ptr() as usize + record.len() - 2081;
            let generation = p.lsn();
            change.take().unwrap()(&mut page[offset..offset + 2081]);
            kernel::page::seal(page, generation);
            break;
        }
    }
    assert!(change.is_none());
    fs::write(path, bytes).unwrap();
}

#[test]
fn schema_conflicting_ids_never_pick_a_winner() {
    let (d, _temp) = case_dir("conflict", 200);
    let source = d.join("source");
    create(&source, 200);
    let mut different = layout(1);
    different.fields[0].0 = "renamed".into();
    change_descriptor(&source, &catalog(1, 1), |bytes| {
        bytes.copy_from_slice(&different.descriptor().unwrap())
    });
    let before = source_bytes(&source);
    let r = recover_typed_candidates(
        &source,
        &d.join("recovery"),
        &DenseV3,
        RecoveryOptions::default(),
    )
    .unwrap();
    assert_eq!(source_bytes(&source), before);
    assert_eq!(
        (
            r.layouts,
            r.conflicting_layouts,
            r.raw_records,
            r.decoded_records,
            r.unresolved_records
        ),
        (1, 1, 200, 100, 100)
    );
    for line in fs::read_to_string(r.destination.join("decoded.jsonl"))
        .unwrap()
        .lines()
    {
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["layout_id"], 2);
    }
}

#[test]
fn schema_descriptor_checksum_is_independent_of_page_checksum() {
    let (d, _temp) = case_dir("descriptor-crc", 200);
    let source = d.join("source");
    create(&source, 200);
    for copy in 0..3 {
        change_descriptor(&source, &catalog(1, copy), |bytes| bytes[2077] ^= 1);
    }
    let before = source_bytes(&source);
    let r = recover_typed_candidates(
        &source,
        &d.join("recovery"),
        &DenseV3,
        RecoveryOptions::default(),
    )
    .unwrap();
    assert_eq!(source_bytes(&source), before);
    assert_eq!(
        (
            r.layouts,
            r.invalid_descriptors,
            r.raw_records,
            r.decoded_records,
            r.missing_layout_records
        ),
        (1, 3, 200, 100, 100)
    );
}

#[test]
fn schema_resource_limit_and_archive_validation_preserve_evidence() {
    let (d, _temp) = case_dir("limits", 200);
    let source = d.join("source");
    create(&source, 200);
    let before = source_bytes(&source);
    let r = recover_typed_candidates(
        &source,
        &d.join("recovery"),
        &DenseV3,
        RecoveryOptions {
            max_value_bytes: 512,
            layout_cache_entries: 1,
        },
    )
    .unwrap();
    assert_eq!(source_bytes(&source), before);
    assert_eq!(
        (r.raw_records, r.decoded_records, r.unresolved_records),
        (198, 198, 2)
    );
    assert_eq!(
        visit_raw_records(&r.destination.join("unresolved.raw"), 4096, |r| {
            assert!(r.overflow_marker);
            assert_eq!(r.value.len(), 12);
            Ok(())
        })
        .unwrap(),
        2
    );
    let mut archive = fs::read(r.destination.join("records.raw")).unwrap();
    archive[8 + 21 + 3] ^= 1;
    let path = d.join("bad-archive");
    fs::write(&path, &archive).unwrap();
    let mut callbacks = 0;
    assert!(visit_raw_records(&path, 512, |_| {
        callbacks += 1;
        Ok(())
    })
    .is_err());
    assert_eq!(callbacks, 0);
    archive[8 + 16..8 + 20].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&path, &archive).unwrap();
    assert!(visit_raw_records(&path, 512, |_| panic!(
        "bounds must be checked before callback"
    ))
    .is_err());
    assert!(recover_typed_candidates(
        &source,
        &r.destination,
        &DenseV3,
        RecoveryOptions::default()
    )
    .is_err());
    assert_eq!(source_bytes(&source), before);
}

#[test]
fn schema_codec_policy_is_replaceable() {
    struct CustomKeys;
    impl RecoveryCodec for CustomKeys {
        fn name(&self) -> &'static str {
            "test-custom-keyspace"
        }
        fn classify(&self, key: &[u8]) -> RecordClass {
            if key.starts_with(b"person/") {
                RecordClass::Entity
            } else {
                DenseV3.classify(key)
            }
        }
        fn layout_id(&self, row: &[u8]) -> Result<u64> {
            DenseV3.layout_id(row)
        }
        fn decode(
            &self,
            layout: &Layout,
            context: CandidateContext<'_>,
            row: &[u8],
        ) -> Result<Value> {
            assert!(context.key.starts_with(b"person/"));
            DenseV3.decode(layout, context, row)
        }
    }
    let (d, _temp) = case_dir("custom-codec", 1);
    let source = d.join("source");
    let mut s = Store::create(&source, cfg()).unwrap();
    s.put(&catalog(1, 0), &layout(1).descriptor().unwrap())
        .unwrap();
    s.put(
        b"person/1",
        &encode_dense_v3(&layout(1), &document(1)).unwrap().row,
    )
    .unwrap();
    s.commit().unwrap();
    s.checkpoint().unwrap();
    drop(s);
    let r = recover_typed_candidates(
        &source,
        &d.join("recovery"),
        &CustomKeys,
        RecoveryOptions::default(),
    )
    .unwrap();
    assert_eq!(r.decoded_records, 1);
    let value: Value = serde_json::from_str(
        fs::read_to_string(r.destination.join("decoded.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(value["document"], document(1));
}
