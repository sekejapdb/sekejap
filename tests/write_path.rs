use e4_prototype::{collections::{CollectionOptions, Database}, Kind};
use kernel::{io::IoMode, limits::ResourceLimits, store::{Config, SyncMode}};
use serde_json::json;

#[test]
fn unchanged_embeddings_complete_under_two_times_loaded_allowance_with_reader() {
    let tmp = std::env::temp_dir();
    assert!(tmp.starts_with("<scratch>") || tmp.starts_with("<scratch>"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let config = Config { budget_bytes: 1<<20, io: IoMode::Buffered, sync: SyncMode::Full };
    let limits = ResourceLimits { data_bytes: 14<<20, wal_bytes: 1<<20, tracked_pages: 1024,
        readers: 4, record_bytes: 16384, recovery_bytes: 256<<10 };
    let mut db = Database::create_limited(&path, config, limits).unwrap();
    let c = db.create_collection("embeddings", vec![("v".into(), Kind::Vector(1536)), ("revision".into(), Kind::Int)], CollectionOptions::default()).unwrap();
    let mut doc = json!({"v":vec![0.25f32;1536],"revision":0});
    for i in 0..1024 {
        db.put(c, &format!("p{i:04}"), &doc).unwrap();
        if i%64==63 {db.commit().unwrap();}
    }
    let size = || -> u64 {
        std::fs::read_dir(&path).unwrap().map(|p| {
            let m=p.unwrap().metadata().unwrap(); if m.is_file() {m.len()} else {0}
        }).sum()
    };
    let loaded = size();
    assert!(limits.total_bytes().unwrap() < loaded*2);
    let old = Database::open_snapshot(&path, config).unwrap();
    for cycle in 1..=12 {
        doc["revision"] = json!(cycle);
        for i in 0..1024 {
            db.put(c, &format!("p{i:04}"), &doc).unwrap();
            if i%64==63 {db.commit().unwrap(); assert!(size() <= limits.total_bytes().unwrap());}
        }
        assert_eq!(old.get(c,"p0000").unwrap().unwrap().document["revision"], 0);
    }
    assert_eq!(old.scan(c,None).unwrap().count(),1024);
    drop(db);
    let db = Database::open(&path, config).unwrap();
    for row in db.scan(c,None).unwrap() {
        let row=row.unwrap(); assert_eq!(row.document,doc);
    }
    assert_eq!(db.scan(c,None).unwrap().count(),1024);
}
