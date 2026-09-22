# sekejap

The compact typed embedded database, in one crate: storage kernel, engine,
query language and service wrapper behind a single `Db` handle.

The example below is the body of a function returning
`Result<(), sekejap::Error>`, and `dist/rust/tests/doc_examples.rs` runs it
exactly as written.

<!-- doc_example: crate_readme_quickstart -->
```rust
use sekejap::{Db, FieldKind};
use serde_json::json;

let dir = std::env::temp_dir().join("sekejap-crate-readme");
let _ = std::fs::remove_dir_all(&dir);

let db = Db::open(&dir)?;
db.create_collection(
    "dishes",
    &[("name", FieldKind::Text), ("price", FieldKind::Int)],
)?;
db.put(("dishes", "laksa"), &json!({ "name": "Laksa", "price": 1200 }))?;
db.execute("CREATE INDEX dishes_price ON dishes USING btree (price)", &[])?;

let rows = db.query(
    "SELECT name, price FROM dishes WHERE price < $1",
    &[json!(2000)],
)?;
for row in rows.iter() {
    println!("{:?} {:?}", row.value("name"), row.value("price"));
}
assert_eq!(rows.len(), 1);
Ok(())
```

- Documents addressed by collection and key, stored as typed positional records
- SQL with prepared statements and `$n` parameters
- Graph edges, vector similarity, spatial and full-text indexes in the same store
- Disk format v2, stable across 0.17.x

The Rust API is described in `docs/dist/RUST_API.md` in the repository.

License: MIT OR Apache-2.0.
