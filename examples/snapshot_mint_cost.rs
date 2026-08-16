// 23b measurement harness: what does minting a snapshot cost, and how does a
// snapshot-served query compare with a direct one?
use sekejap::CoreDB;
use std::time::Instant;

fn med(mut v: Vec<std::time::Duration>) -> std::time::Duration { v.sort(); v[v.len()/2] }

fn main() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE v (_key TEXT PRIMARY KEY, cat TEXT, n INTEGER, name TEXT)").unwrap();
        for i in 0..50_000 {
            let cat = ["cafe","bar","gym"][i % 3];
            db.execute(&format!("INSERT INTO v (_key, cat, n, name) VALUES ('v{i}','{cat}',{i},'Venue {i}')")).unwrap();
        }
        db.execute("CREATE INDEX ON v USING btree (cat)").unwrap();
        db.execute("CREATE INDEX ON v USING btree (n)").unwrap();
        db.execute("CREATE INDEX ON v USING bm25 (name)").unwrap();
        db.compact().unwrap();
    }
    let mut db = CoreDB::open_paged(dir.path()).unwrap();

    let mint = |db: &CoreDB| med((0..20).map(|_| {
        let s = Instant::now(); let x = db.snapshot_db().unwrap(); let e = s.elapsed();
        std::hint::black_box(&x); e
    }).collect());

    println!("MINT COST");
    println!("  empty overlay            {:>9.1?}", mint(&db));
    for i in 0..1_000 { db.execute(&format!("INSERT INTO v (_key,cat,n,name) VALUES ('w{i}','cafe',{i},'W {i}')")).unwrap(); }
    println!("  after 1k overlay writes  {:>9.1?}", mint(&db));
    for i in 1_000..10_000 { db.execute(&format!("INSERT INTO v (_key,cat,n,name) VALUES ('w{i}','cafe',{i},'W {i}')")).unwrap(); }
    println!("  after 10k overlay writes {:>9.1?}", mint(&db));

    // Query performance: direct vs served from a snapshot.
    let q = "SELECT _key FROM v WHERE cat = 'cafe' LIMIT 50";
    let direct = med((0..200).map(|_| { let s=Instant::now(); let r=db.query(q).unwrap().collect(); let e=s.elapsed(); std::hint::black_box(r); e }).collect());
    let snap = db.snapshot_db().unwrap();
    let viasnap = med((0..200).map(|_| { let s=Instant::now(); let r=snap.query(q).unwrap().collect(); let e=s.elapsed(); std::hint::black_box(r); e }).collect());
    println!("QUERY (indexed, LIMIT 50)");
    println!("  direct                   {direct:>9.1?}");
    println!("  via snapshot             {viasnap:>9.1?}   ({:.2}x)", viasnap.as_secs_f64()/direct.as_secs_f64());

    // Amortised: one mint per N writes, the service pattern.
    let cycles = 50;
    let t = Instant::now();
    for c in 0..cycles {
        for i in 0..20 { db.execute(&format!("INSERT INTO v (_key,cat,n,name) VALUES ('c{c}_{i}','bar',{i},'C')")).unwrap(); }
        std::hint::black_box(db.snapshot_db().unwrap());
    }
    println!("SERVICE CYCLE (20 writes + 1 mint) x{cycles}: {:>9.1?} total, {:>7.1?}/cycle",
        t.elapsed(), t.elapsed()/cycles);
}
