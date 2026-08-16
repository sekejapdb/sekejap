// Diagnose which facet filters the hybrid query to zero.
//   cargo run --release --example hybrid_debug 5000
use sekejap::CoreDB;

fn count(db: &CoreDB, q: &str) -> usize { db.query(q).map(|s| s.collect().len()).unwrap_or(usize::MAX) }

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5_000);
    let dir = std::env::temp_dir().join(format!("sk-hd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cats = ["cafe", "resto", "bar", "bakery", "hotel"];
    let words = ["coffee", "brunch", "wine", "grilled", "vegan", "spicy", "seafood", "dessert"];
    let mut db = CoreDB::open(&dir).unwrap();
    db.execute("CREATE TABLE venues (category TEXT, price INTEGER, content TEXT, geometry GEO)").unwrap();
    let mut pairs = Vec::with_capacity(n);
    for i in 0..n {
        let hx = (i.wrapping_mul(2654435761) % 4000) as f64 - 2000.0;
        let hy = (i.wrapping_mul(40503) % 4000) as f64 - 2000.0;
        let lon = 106.5 + hx * 0.0001;
        let lat = -6.4 + hy * 0.0001;
        let content = format!("{} {} great place",
            words[i.wrapping_mul(3) % words.len()], words[i.wrapping_mul(5).wrapping_add(2) % words.len()]);
        pairs.push((format!("venues/v{i}"), format!(
            r#"{{"_collection":"venues","_key":"v{i}","category":"{}","price":{},"content":"{content}","geometry":{{"type":"Point","coordinates":[{lon},{lat}]}}}}"#,
            cats[i % cats.len()], i % 500)));
    }
    db.put_many(pairs.iter().map(|(s, p)| (s.as_str(), p.as_str()))).unwrap();
    db.build_field_index("venues", "category");
    db.build_spatial_index();
    db.build_bm25_index("content");
    db.execute("CREATE INDEX ON venues USING search (content)").unwrap();
    db.compact().unwrap();

    println!("total                 : {}", count(&db, "SELECT _key FROM venues"));
    println!("category='cafe'       : {}", count(&db, "SELECT _key FROM venues WHERE category = 'cafe'"));
    println!("ST_DWithin 30km       : {}", count(&db, "SELECT _key FROM venues WHERE ST_DWithin(geometry, POINT(106.5 -6.4), 30000)"));
    println!("ST_DWithin 300km      : {}", count(&db, "SELECT _key FROM venues WHERE ST_DWithin(geometry, POINT(106.5 -6.4), 300000)"));
    println!("SEARCH('coffee')      : {}", count(&db, "SELECT _key FROM venues WHERE SEARCH('coffee')"));
    println!("cafe + coffee         : {}", count(&db, "SELECT _key FROM venues WHERE category='cafe' AND SEARCH('coffee')"));
    println!("cafe + spatial        : {}", count(&db, "SELECT _key FROM venues WHERE category='cafe' AND ST_DWithin(geometry, POINT(106.5 -6.4), 30000)"));
    println!("spatial + coffee      : {}", count(&db, "SELECT _key FROM venues WHERE ST_DWithin(geometry, POINT(106.5 -6.4), 30000) AND SEARCH('coffee')"));
    println!("coffee + spatial      : {}", count(&db, "SELECT _key FROM venues WHERE SEARCH('coffee') AND ST_DWithin(geometry, POINT(106.5 -6.4), 30000)"));
    println!("all three             : {}", count(&db, "SELECT _key FROM venues WHERE category='cafe' AND ST_DWithin(geometry, POINT(106.5 -6.4), 30000) AND SEARCH('coffee')"));
    println!("all three (search 1st): {}", count(&db, "SELECT _key FROM venues WHERE SEARCH('coffee') AND category='cafe' AND ST_DWithin(geometry, POINT(106.5 -6.4), 30000)"));
    println!("-- BM25 variant --");
    println!("BM25 coffee           : {}", count(&db, "SELECT _key FROM venues WHERE BM25(content,'coffee') > 0.0"));
    println!("BM25 + cafe           : {}", count(&db, "SELECT _key FROM venues WHERE BM25(content,'coffee') > 0.0 AND category='cafe'"));
    println!("BM25 + spatial        : {}", count(&db, "SELECT _key FROM venues WHERE BM25(content,'coffee') > 0.0 AND ST_DWithin(geometry, POINT(106.5 -6.4), 30000)"));
    println!("BM25 + spatial + cafe : {}", count(&db, "SELECT _key FROM venues WHERE BM25(content,'coffee') > 0.0 AND ST_DWithin(geometry, POINT(106.5 -6.4), 30000) AND category='cafe'"));
    let _ = std::fs::remove_dir_all(&dir);
}
