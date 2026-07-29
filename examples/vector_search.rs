//! Vector similarity search with an HNSW index.
//!
//!   cargo run --example vector_search

use sekejap::CoreDB;

fn main() {
    let mut db = CoreDB::new();

    db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, name TEXT, emb VECTOR)").unwrap();
    db.execute("CREATE INDEX ON items USING hnsw (emb)").unwrap();
    for (key, name, emb) in [
        ("a", "apple", "[1.0, 0.0, 0.0]"),
        ("b", "banana", "[0.0, 1.0, 0.0]"),
        ("c", "cherry", "[0.9, 0.1, 0.0]"),
    ] {
        db.execute(&format!(
            "INSERT INTO items (_key, name, emb) VALUES ('{key}', '{name}', {emb})"
        ))
        .unwrap();
    }

    println!("2 nearest to [1,0,0] (apple-ish):");
    for hit in db
        .query("SELECT name FROM items WHERE VECTOR_NEAR(emb, [1.0, 0.0, 0.0], 2)")
        .unwrap()
        .collect()
    {
        println!("  {}", serde_json::to_string(&hit.payload.unwrap()).unwrap());
    }
}
