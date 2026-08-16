// Which index makes a single INSERT expensive?
use sekejap::CoreDB;
use std::time::Instant;

fn probe(label: &str, indexes: &[&str], rows: usize) {
    let d = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(d.path()).unwrap();
    db.execute("CREATE TABLE v (_key TEXT PRIMARY KEY, cat TEXT, n INTEGER, name TEXT)").unwrap();
    for i in 0..rows {
        let cat = ["cafe","bar","gym"][i % 3];
        db.execute(&format!("INSERT INTO v (_key,cat,n,name) VALUES ('v{i}','{cat}',{i},'Venue {i}')")).unwrap();
    }
    for ix in indexes { db.execute(ix).unwrap(); }
    db.compact().unwrap();

    let n = 50;
    let t = Instant::now();
    for i in 0..n {
        db.execute(&format!("INSERT INTO v (_key,cat,n,name) VALUES ('x{i}','bar',{i},'X {i}')")).unwrap();
    }
    println!("  {label:24} @{rows:>6} rows : {:>9.1?}/write", t.elapsed() / n);
}

fn main() {
    println!("SINGLE-INSERT COST vs index set");
    for rows in [5_000usize, 20_000] {
        probe("none", &[], rows);
        probe("btree", &["CREATE INDEX ON v USING btree (cat)"], rows);
        probe("bm25", &["CREATE INDEX ON v USING bm25 (name)"], rows);
        probe("search", &["CREATE INDEX ON v USING search (name)"], rows);
        println!();
    }
}
