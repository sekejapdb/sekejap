use criterion::{criterion_group, criterion_main, Criterion, black_box};
use sekejap::{CoreDB, Config, WalFormat};
use serde_json::json;
use tempfile::TempDir;

fn bench_wal_write(c: &mut Criterion) {
    let payload_small = json!({"_collection":"users","_key":"alice","name":"Alice","age":30}).to_string();
    let payload_medium = json!({
        "_collection": "articles",
        "_key": "a1",
        "title": "Introduction to Graph Databases and Multi-Model Storage",
        "body": "Lorem ipsum ".repeat(50),
        "tags": ["graph", "database", "embedded"],
        "views": 1234,
        "published": true
    }).to_string();

    let mut group = c.benchmark_group("wal_write");

    for (label, format) in [("json", WalFormat::Json), ("binary", WalFormat::Binary)] {
        group.bench_function(format!("{label}/small_put"), |b| {
            let dir = TempDir::new().unwrap();
            let mut db = CoreDB::open_with_config(dir.path(), Config {
                wal_format: format,
                ..Config::default()
            }).unwrap();
            b.iter(|| {
                db.put(black_box("users/alice"), black_box(&payload_small)).unwrap();
            });
        });

        group.bench_function(format!("{label}/medium_put"), |b| {
            let dir = TempDir::new().unwrap();
            let mut db = CoreDB::open_with_config(dir.path(), Config {
                wal_format: format,
                ..Config::default()
            }).unwrap();
            b.iter(|| {
                db.put(black_box("articles/a1"), black_box(&payload_medium)).unwrap();
            });
        });

        group.bench_function(format!("{label}/link"), |b| {
            let dir = TempDir::new().unwrap();
            let mut db = CoreDB::open_with_config(dir.path(), Config {
                wal_format: format,
                ..Config::default()
            }).unwrap();
            db.put("users/alice", &payload_small).unwrap();
            db.put("users/bob", &payload_small).unwrap();
            b.iter(|| {
                db.link(black_box("users/alice"), black_box("users/bob"), black_box("follows"));
            });
        });
    }

    group.finish();
}

fn bench_wal_replay(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_replay");

    for (label, format) in [("json", WalFormat::Json), ("binary", WalFormat::Binary)] {
        let dir = TempDir::new().unwrap();
        {
            let mut db = CoreDB::open_with_config(dir.path(), Config {
                wal_format: format,
                ..Config::default()
            }).unwrap();
            for i in 0..1000 {
                db.put(
                    &format!("items/{i}"),
                    &json!({"_collection":"items","_key":i.to_string(),"val":i}).to_string(),
                ).unwrap();
            }
            for i in 0..500 {
                db.link(&format!("items/{i}"), &format!("items/{}", i + 1), "next");
            }
        }

        group.bench_function(format!("{label}/1500_entries"), |b| {
            b.iter(|| {
                let _db = CoreDB::open_with_config(dir.path(), Config {
                    wal_format: format,
                    ..Config::default()
                }).unwrap();
            });
        });
    }

    group.finish();
}

fn bench_wal_encode_decode(c: &mut Criterion) {
    use sekejap::wal_bench::{WalEntry, binary_encode, binary_decode};

    let put_entry = WalEntry::Put {
        slug: "users/alice".into(),
        payload: json!({"_collection":"users","_key":"alice","name":"Alice","age":30}).to_string(),
    };
    let link_entry = WalEntry::Link {
        from: "users/alice".into(),
        to: "users/bob".into(),
        edge_type: "follows".into(),
        strength: 1.0,
    };
    let vector_entry = WalEntry::PutVector {
        slug: "docs/d1".into(),
        field: "embedding".into(),
        data: vec![0.1; 128],
    };

    let mut group = c.benchmark_group("wal_codec");

    for (label, entry) in [("put", &put_entry), ("link", &link_entry), ("vector_128d", &vector_entry)] {
        let json_bytes = serde_json::to_vec(entry).unwrap();
        let binary_bytes = binary_encode(entry);
        eprintln!("{label}: json={} bytes, binary={} bytes ({}% smaller)",
            json_bytes.len(), binary_bytes.len(),
            100 - (binary_bytes.len() * 100 / json_bytes.len()));

        group.bench_function(format!("json/encode/{label}"), |b| {
            b.iter(|| serde_json::to_vec(black_box(entry)).unwrap())
        });
        group.bench_function(format!("binary/encode/{label}"), |b| {
            b.iter(|| binary_encode(black_box(entry)))
        });
        group.bench_function(format!("json/decode/{label}"), |b| {
            b.iter(|| serde_json::from_slice::<WalEntry>(black_box(&json_bytes)).unwrap())
        });
        group.bench_function(format!("binary/decode/{label}"), |b| {
            b.iter(|| binary_decode(black_box(&binary_bytes)).unwrap())
        });
    }

    group.finish();
}

criterion_group!(benches, bench_wal_write, bench_wal_replay, bench_wal_encode_decode);
criterion_main!(benches);
