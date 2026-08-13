# Rust

sekejap is a Rust library first; every other binding wraps this crate.
Install from [crates.io](https://crates.io/crates/sekejap):

```bash
cargo add sekejap
```

## First query

```rust
use sekejap::CoreDB;

let mut db = CoreDB::open("./mydb")?;
db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT)")?;
db.execute("INSERT INTO places (_key, name) VALUES ('a', 'Uluwatu')")?;

for hit in db.query("SELECT * FROM places")?.collect() {
    println!("{:?}", hit.payload);
}
```

## The atomic API (Rust only)

A chain of small typed calls that builds the same query plan SQL compiles to —
no text parsing. Useful when a query is assembled from program logic at
runtime, or on hot paths where parse cost shows up (measured on an indexed
point query: 0.8 ms atomic, 1.3 ms SQL — same engine, same plan).

```rust
let picks = db.collection("dishes")
    .where_gte("protein_g", 25.0)
    .sort("price", false)          // false = ascending
    .take(10)
    .collect();
```

**Starters:** `db.collection("x")`, `db.one("x/key")`, `db.many([...])`,
`db.all()`.

**Filters:** `where_eq`, `where_neq`, `where_gt`/`where_gte`,
`where_lt`/`where_lte`, `where_between`, `where_in`, `like`, `ilike`.

**Graph moves:** `forward(edge)`, `backward(edge)`, `hops(n)`,
`hops_typed(edge, n)`, `leaves()`, `roots()`:

```rust
// Which venues did this band play at?
db.one("bands/the_vines").forward("played_at").collect();
```

**Spatial / vector:** `st_dwithin(lat, lon, metres)` (alias `near`),
`st_contains_point`, `st_within`, `st_contains`, `st_intersects`,
`vector_near(field, query, k)`.

**Combining and shaping:** `intersect` / `union` / `subtract` between two
chains; `sort` / `sort_multi`, `skip`, `take`, `select`; endings `collect`,
`count`, `first`, `exists`, `edge_collect`.

```rust
let cafes  = db.collection("venues").where_eq("category", "cafe");
let nearby = db.collection("venues").st_dwithin(-37.81, 144.96, 2000.0);
let both   = cafes.intersect(nearby).count();
```

For variable-length graph patterns and path predicates, use SQL `MATCH` — the
atomic graph moves cover fixed shapes. Both surfaces run on the same executor,
so mixing them is normal.

## Direct operations and engine control

`put` / `get` / `link` / `link_many`, bulk scopes (`begin_bulk` /
`end_bulk`), index builds (`build_bm25_index`, `build_hnsw_index_metric`, …),
`compact()`. Open modes: `open` (resident), `open_paged` (memory-mapped, fast
startup), `open_read_only`.

Feature flags: `engine` (concurrent wrapper), `serve` (HTTP adapter),
`pg` (Postgres wire adapter), `s3`.

Runnable demos: [`examples/`](../../../examples/) —
`cargo run --example sql_basics` and friends.
