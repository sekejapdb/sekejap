# The `sekejap` Rust API (0.17.0)

`dist/rust` is the crate an application depends on: one name, one handle, one
`Result`. It is NOT a port of the 0.16 `CoreDB`. The 0.16 surface and the call
sites of the applications that used it say which operations have to exist and
how light they have to feel; the shape below is E4's own -- collection and key
instead of a slug string, `serde_json::Value` in and out, `$n` parameters, and
a refusal by name wherever E4 has no atomic.

Layer rule (`docs/LAYERS.md`): `dist/rust -> dist -> lang -> core`. Every item
in this document is a composition of calls those three layers already export;
this crate adds no execution and no second engine.

Units: bytes are bytes, durations are `std::time::Duration`, counts are rows or
edges and are named as such.

---

## 1. Opening

| item | E4 call | one line |
|---|---|---|
| `Db::open(path) -> Result<Db>` | `Database::create` when the directory holds no `data`, else `Database::open` (`core/engine/src/collections/mod.rs:878`, `:904`), wrapped in `Mutex<Database>` | `let db = Db::open("/var/lib/app")?;` |
| `Db::open_with(path, Config)` | same, with the caller's `kernel::store::Config` | `Db::open_with(dir, Config { budget_bytes: 1 << 28, ..Config::default() })?` |
| `Db::open_service(path)` | `ServiceDatabase::open` (`dist/src/service/mod.rs:173`), creating the directory first if it is empty | `let db = Db::open_service(dir)?;` |
| `Db::open_service_with(path, Config)` | same | -- |
| `Db::mode() -> Mode` | -- | `assert_eq!(db.mode(), Mode::Single);` |
| `Db::path() -> &Path` | `ServiceDatabase::path` / the remembered path | `db.path()` |
| `Db::close(self) -> Result<()>` | drop the writer; `ServiceDatabase::close` (`:504`) in service mode | `db.close()?;` |

`Db` is `Send + Sync` in both modes. `Mode::Single` is one `Mutex<Database>`:
readers and the writer share it, so a read sees the write that preceded it with
no publication step. `Mode::Service` is `docs/dist/OPS_CONTRACT.md` §1: one
writer, parallel readers on a published snapshot, a change feed. The two modes
answer every call below identically except where this document says otherwise.

There is no in-memory mode. E4 is disk-first (`OPS_CONTRACT.md` Law 1) and a
temporary directory would be a fake of one: `Db::open` is the only constructor.

## 2. Documents

A row is addressed by `(collection, key)`. `Addr<'a> { collection, key }` is the
pair as one argument, and `("posts", "p1")` converts into it, so no call takes
two bare strings that could be swapped.

| item | E4 call | one line |
|---|---|---|
| `Db::create_collection(name, &[(&str, FieldKind)]) -> Result<bool>` | `Database::create_collection` (`collections/mod.rs:1363`) | `db.create_collection("posts", &[("title", FieldKind::Text)])?;` |
| `Db::drop_collection(name) -> Result<bool>` | `begin_drop_collection` + `drop_collection_step` to completion (`collections/drop_collection.rs`) | `db.drop_collection("posts")?;` |
| `Db::put(addr, &Value) -> Result<EntityId>` | `Database::put` (`:1734`) + commit | `db.put(("posts", "p1"), &json!({"title": "Hello"}))?;` |
| `Db::put_many(collection, rows) -> Result<usize>` | N × `put`, ONE commit | `db.put_many("posts", vec![("p1".into(), doc)])?;` |
| `Db::get(addr) -> Result<Option<Value>>` | `Database::get` (`:1834`); `Entity::document`, with `_key` injected | `let doc = db.get(("posts", "p1"))?;` |
| `Db::exists(addr) -> Result<bool>` | `Database::get(..).is_some()` | `if db.exists(("posts", "p1"))? { .. }` |
| `Db::delete(addr) -> Result<bool>` | `Database::delete` (`:1920`) + commit | `db.delete(("posts", "p1"))?;` |
| `Db::scan(collection) -> Result<Scan<'_>>` | `Database::scan` (`:1939`), ONE page held at a time | `for doc in db.scan("posts")? { let doc = doc?; }` |

`put` to a collection that does not exist is `Error::UnknownCollection`, not an
implicit create: one document implies no column kinds for the documents after
it, and guessing them is what a later type conflict is made of. Declare the
collection with `create_collection` or `CREATE TABLE`.

A document is the row's fields WITHOUT the E4 internal key field, plus `_key`
set to the row's external key, so a document read back is a document that can be
written back. A declared `VECTOR(n)` field renders as a JSON array of `n`
numbers, and writing that array back writes the vector sidecar.

`Scan` is lazy: it holds at most `page_size` rows (default 256, `Scan::page_size`
sets it) and takes the read lock once per page, never for the whole walk. This
is Law 1 at the API: no call returns a collection-sized `Vec`.

```rust
pub struct Document { pub collection: String, pub key: String, pub id: EntityId, pub fields: Value }
```

## 3. SQL

The dialect is `sekejap_lang`'s, unchanged: `docs/lang/QL_CONTRACT.md` Tier 1,
with every Tier-2/Tier-3 construct REFUSED by name. This crate parses nothing
and rewrites nothing -- a statement goes to `SqlDatabase::sql` as written.

| item | E4 call | one line |
|---|---|---|
| `Db::execute(sql, &[Value]) -> Result<u64>` | `SqlDatabase::sql` (`lang/src/lib.rs`) + commit; `SqlResult::Affected(n)` as `n`, `Notice` as `0` | `db.execute("INSERT INTO posts (_key, title) VALUES ($1, $2)", &[json!("p1"), json!("Hello")])?;` |
| `Db::query(sql, &[Value]) -> Result<Rows>` | `SqlDatabase::sql` → `SqlResult::Rows` | `let rows = db.query("SELECT _key, title FROM posts", &[])?;` |
| `Db::stream(sql, &[Value], page_rows, &mut f) -> Result<u64>` | `prepare_sql` + `PreparedSql::for_each_row` (`lang/src/lib.rs:399`) | `db.stream(sql, &[], 512, &mut |row| { .. Ok(()) })?;` |
| `Db::explain(sql, &[Value]) -> Result<String>` | `SqlDatabase::sql_explain` / `explain_sql` (`lang/src/lib.rs:523`) | `println!("{}", db.explain("SELECT * FROM posts", &[])?);` |

```rust
pub struct Rows { pub columns: Arc<Vec<String>>, pub rows: Vec<Row> }
pub struct Row  { pub columns: Arc<Vec<String>>, pub id: EntityId, pub values: Vec<SqlValue> }

impl Row {
    pub fn value(&self, column: &str) -> Option<&SqlValue>;
    pub fn json(&self, column: &str) -> Option<Value>;
    pub fn to_object(&self) -> Map<String, Value>;   // MISSING columns are omitted
}
```

`SqlValue` is `sekejap_lang::SqlValue`, re-exported: `Missing`, `Null`, `Bool`,
`Int`, `Float`, `Text`, `Json`, `Id`. `Missing` is not `Null`, so `to_object`
OMITS a missing column rather than writing `null` for it.

`Db::stream` is a callback and not an iterator because a compiled SELECT owns
what its request borrows -- the term strings, the query vector, the geometries
(`lang/src/lib.rs:362`) -- so the cursor cannot outlive the call. It holds one
page at a time, like `Scan`.

Parameters are `serde_json::Value`, mapped to `sekejap_lang::Param` by a stated
rule: `Null` → `Null`, `Bool` → `Bool`, an integer number → `Int`, any other
number → `Float`, `String` → `Text`, an array whose members are all numbers →
`Vector`, anything else → `Json`.

## 4. Edges

Edges are `docs/core/GRAPH_CONTRACT.md` §4. An edge carries a type name, an
optional JSON properties object, and lives in the BASE graph context unless the
caller names another.

| item | E4 call | one line |
|---|---|---|
| `Db::link(from, edge_type, to) -> Result<()>` | resolve both `Addr`s to `EntityId`, `Database::link` (`index/graph/mod.rs:1423`) with empty properties + commit | `db.link(("people","alice"), "knows", ("people","bob"))?;` |
| `Db::link_with(from, edge_type, to, &Value)` | same, with the properties object | `db.link_with(a, "knows", b, &json!({"since": 2020}))?;` |
| `Db::unlink(from, edge_type, to) -> Result<bool>` | `Database::unlink` (`:1537`) + commit | `db.unlink(a, "knows", b)?;` |
| `Db::neighbours(of, Option<&str>, Direction, limit) -> Result<Vec<Document>>` | `Database::neighbor_ids` (`:1826`) under a stated `EdgeBudget`, then `get_by_id` | `db.neighbours(("people","alice"), Some("knows"), Direction::Outgoing, 100)?;` |

Both endpoints must exist: E4 validates them (`validate_endpoints`,
`index/graph/mod.rs:1436`). An edge to a key that has not been written is
`Error::UnknownRow`, never a dangling identity.

A traversal deeper than one hop is SQL's `GRAPH_TABLE`
(`docs/lang/QL_CONTRACT.md` §2), not a method here: the bounded walk already has
one spelling and this crate does not add a second.

## 5. Catalog

| item | E4 call | one line |
|---|---|---|
| `Db::collections() -> Result<Vec<String>>` | `Database::list_collections` (new, §8) | `for name in db.collections()? { .. }` |
| `Db::describe(collection) -> Result<Option<Collection>>` | `Database::collection` + `collection_info` (`:1474`, `:1487`) + `list_indexes` (`collections/catalog.rs:887`) | `let schema = db.describe("posts")?;` |
| `Db::scan_count_rows(collection) -> Result<u64>` | `Database::scan` walked to the end | `let n = db.scan_count_rows("posts")?;` |
| `Db::scan_count_all_rows() -> Result<u64>` | the same walk per collection | `let n = db.scan_count_all_rows()?;` |
| `Db::scan_count_edges() -> Result<u64>` | a walk of the primary edge keyspace | `let n = db.scan_count_edges()?;` |

```rust
pub struct Collection { pub name: String, pub fields: Vec<Field>, pub indexes: Vec<Index>, pub timestamps: bool }
pub struct Field { pub name: String, pub kind: FieldKind, pub declared: Option<String>, pub primary_key: bool }
pub struct Index { pub name: String, pub field: String, pub family: IndexFamily, pub unique: bool, pub ready: bool }
```

`FieldKind` is `sekejap_core::Kind`, re-exported: `Text`, `Int`, `Real`, `Bool`,
`Json`, `Geo`, `Point`, `Vector(n)`. `declared` is the SQL spelling the catalog
recorded where the `Kind` does not carry it (`TIMESTAMPTZ` and `DATE` are both
`Kind::Int`). The first `Field` of every collection is `_key`, `FieldKind::Text`,
`primary_key: true`: it is a declared field of every layout
(`collections/mod.rs:1350`) and naming it is how a caller addresses a row.

The three counts are named `scan_*` because that is what they are. E4 keeps no
O(1) row counter and no O(1) edge counter (`docs/dist/OPS_CONTRACT.md` §6.1), and
a method called `count()` would hide a full walk behind a cheap-looking name.
Law 4: a scan is called a scan.

## 6. Transactions

| item | E4 call | one line |
|---|---|---|
| `Db::transaction() -> Result<Tx<'_>>` | take the writer | `let mut tx = db.transaction()?;` |
| `Tx::put/put_many/delete/link/link_with/unlink/execute` | the same calls as §2-§4, with NO commit | `tx.put(("posts","p1"), &doc)?;` |
| `Tx::commit(self) -> Result<()>` | `Database::commit` (`collections/mod.rs:1957`) | `tx.commit()?;` |
| `Tx::rollback(self) -> Result<()>` | `Database::rollback` (`:1982`) | `tx.rollback()?;` |

Every `Db::` write in §2-§4 commits before it returns: durability per call, which
is what an application that writes one row at a time expects and what the 0.16
callers were built on. `Tx` is the opposite bargain -- many writes, one barrier --
and the two are the whole story: there is no third auto-commit toggle to get
wrong. A `Tx` dropped without `commit` ROLLS BACK, and says so in its docs.

`BEGIN`, `COMMIT` and `ROLLBACK` as SQL through `Db::execute` are refused in
service mode by the service's own barrier rule
(`dist/src/service/mod.rs:603`, `TRANSACTION_SQL_REFUSAL`); use `Tx`.

## 7. Maintenance and observation

| item | E4 call | one line |
|---|---|---|
| `Db::checkpoint() -> Result<bool>` | `Database::checkpoint` (`:1970`); `Ok(false)` = a live reader holds a slot and the fold is DEFERRED, not failed | `db.checkpoint()?;` |
| `Db::publish() -> Result<()>` | `ServiceDatabase::publish_now` (`:275`) in service mode; `Ok(())` in single mode, where a commit is already visible to this handle | `db.publish()?;` |
| `Db::storage() -> Result<Storage>` | `Database::storage_bytes` (`:1011`) | `let s = db.storage()?; s.data_bytes + s.wal_bytes` |
| `Db::service() -> Option<&ServiceDatabase>` | the handle itself, for the change feed, the interrupt and the statement timeout | `db.service().map(\|s\| s.subscribe_changes())` |

```rust
pub struct Storage { pub data_bytes: u64, pub wal_bytes: u64 }
```

In service mode the published read view holds a reader slot for its whole life,
so `checkpoint` there answers `Ok(false)` until that view is replaced. That is
`OPS_CONTRACT.md` §1's own trade and is reported, not worked around.

What this crate does NOT offer, each because E4 has no atomic for it and an
emulation would be the fake the eighth law forbids:

| asked for | refusal |
|---|---|
| an in-memory database | E4 is disk-first; `Db::open` takes a directory (`OPS_CONTRACT.md` Law 1) |
| `trim_memory` | E4 holds nothing proportional to rows to trim (`OPS_CONTRACT.md` §6.3) |
| a payload-rewriting `compact` | `checkpoint` folds the committed WAL into the data file; it does not rewrite rows (`collections/mod.rs:1970`) |
| `SHOW TABLES` / `SHOW <table>` as SQL | `QL_CONTRACT.md` §2 leaves the SHOW family to a later tier; `Db::collections` and `Db::describe` answer the same questions as data |
| `FROM MATCH ...` | not adopted; the bounded traversal is `GRAPH_TABLE` (`QL_CONTRACT.md` §2) |
| `INSERT (a)-[:t]->(b)` as SQL | edge DML has no Tier-1 spelling; `Db::link` is the call |
| `USING hash` / `USING spatial` index methods | the families are btree, gin, gist, exact and quantized (`lang/src/parser/ddl.rs:462`) |

## 8. Errors

```rust
pub enum Error {
    Sql(sekejap_lang::SqlError),            // parse, refusal by tier, compile
    Engine(sekejap_core::collections::Error),
    Query(sekejap_core::collections::QueryError),
    Service(sekejap_dist::service::ServiceError),
    Io(std::io::Error),
    UnknownCollection(String),
    UnknownRow { collection: String, key: String },
    Refused { construct: String, reason: String },
}
pub type Result<T> = std::result::Result<T, Error>;
```

`Error::Refused` is this crate's own named refusal and always carries both what
was asked for and why there is no atomic. A refusal is never an empty answer:
no call in this document returns an empty `Vec` where it means "not built".

## 9. What core had to make public

| item | why |
|---|---|
| `sekejap_core::{Config, IoMode, SyncMode}` (re-export of `kernel::store`) | `Database::create(path, Config)` is public API whose parameter type could not be NAMED without depending on `kernel`, which `dist/rust` must not do |
| `Database::list_collections() -> Result<Vec<String>>` | the catalog's name keyspace was reachable only through `raw_for_each`, which is `#[doc(hidden)]` diagnostics |

---

Version: this document describes `sekejap` 0.17.0. `docs/dist/FFI_CONTRACT.md`
maps the 0.16 C ABI onto the same three layers and is unaffected by it.
