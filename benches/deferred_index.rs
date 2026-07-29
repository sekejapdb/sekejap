use criterion::{criterion_group, criterion_main, Criterion, black_box};
use sekejap::CoreDB;
use tempfile::TempDir;

fn gen_payload(i: usize) -> (String, String) {
    let slug = format!("articles/a{i}");
    let json = format!(
        r#"{{"_collection":"articles","_key":"a{i}","title":"Article number {i} about rust databases","body":"This is the body of article {i}. It contains various words about graph databases, vector search, and embedded systems. Each article has unique content identified by its number {i}.","views":{views}}}"#,
        i = i,
        views = i * 10,
    );
    (slug, json)
}

fn bench_insert_with_bm25(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_with_bm25");
    group.sample_size(10);

    for n in [100, 500, 1000] {
        let payloads: Vec<(String, String)> = (0..n).map(gen_payload).collect();

        group.bench_function(format!("per_row/{n}"), |b| {
            b.iter(|| {
                let dir = TempDir::new().unwrap();
                let mut db = CoreDB::open(dir.path()).unwrap();
                db.build_bm25_index("title");
                db.build_bm25_index("body");
                for (slug, json) in &payloads {
                    db.put(black_box(slug), black_box(json)).unwrap();
                }
                black_box(db.bm25_search("title", "rust", 5).len());
            });
        });

        group.bench_function(format!("put_many/{n}"), |b| {
            b.iter(|| {
                let dir = TempDir::new().unwrap();
                let mut db = CoreDB::open(dir.path()).unwrap();
                db.build_bm25_index("title");
                db.build_bm25_index("body");
                let items: Vec<(&str, &str)> = payloads.iter()
                    .map(|(s, j)| (s.as_str(), j.as_str()))
                    .collect();
                db.put_many(black_box(items)).unwrap();
                black_box(db.bm25_search("title", "rust", 5).len());
            });
        });

        group.bench_function(format!("sql_batch/{n}"), |b| {
            b.iter(|| {
                let dir = TempDir::new().unwrap();
                let mut db = CoreDB::open(dir.path()).unwrap();
                db.execute("CREATE TABLE articles (_key TEXT PRIMARY KEY, title TEXT, body TEXT, views INTEGER)").unwrap();
                db.execute("CREATE INDEX ON articles USING bm25 (title)").unwrap();
                db.execute("CREATE INDEX ON articles USING bm25 (body)").unwrap();
                let mut sql = String::with_capacity(n * 200);
                sql.push_str("INSERT INTO articles (_key, title, body, views) VALUES ");
                for i in 0..n {
                    if i > 0 { sql.push_str(", "); }
                    sql.push_str(&format!(
                        "('a{i}', 'Article number {i} about rust databases', 'Body of article {i} about graph databases vector search embedded systems', {views})",
                        i = i, views = i * 10
                    ));
                }
                db.execute(&sql).unwrap();
                black_box(db.bm25_search("title", "rust", 5).len());
            });
        });
    }

    group.finish();
}

fn bench_insert_no_bm25(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_no_bm25");
    group.sample_size(10);

    for n in [1000, 5000] {
        let payloads: Vec<(String, String)> = (0..n).map(gen_payload).collect();

        group.bench_function(format!("per_row/{n}"), |b| {
            b.iter(|| {
                let dir = TempDir::new().unwrap();
                let mut db = CoreDB::open(dir.path()).unwrap();
                for (slug, json) in &payloads {
                    db.put(black_box(slug), black_box(json)).unwrap();
                }
            });
        });

        group.bench_function(format!("put_many/{n}"), |b| {
            b.iter(|| {
                let dir = TempDir::new().unwrap();
                let mut db = CoreDB::open(dir.path()).unwrap();
                let items: Vec<(&str, &str)> = payloads.iter()
                    .map(|(s, j)| (s.as_str(), j.as_str()))
                    .collect();
                db.put_many(black_box(items)).unwrap();
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_insert_with_bm25, bench_insert_no_bm25);
criterion_main!(benches);
