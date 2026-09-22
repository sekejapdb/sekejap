# The `sekejap` Rust API (0.17.0)

`dist/rust` is the crate an application depends on: one name, one handle, one
`Result`. It is NOT a port of the 0.16 `CoreDB`. The 0.16 surface and the call
sites of the applications that used it say which operations have to exist and
how light they have to feel; the shape below is sekejap's own -- collection and key
instead of a slug string, `serde_json::Value` in and out, `$n` parameters, and
a refusal by name wherever sekejap has no atomic.

Layer rule (`docs/LAYERS.md`): `dist/rust -> dist -> lang -> core`. Every item
in this document is a composition of calls those three layers already export;
this crate adds no execution and no second engine.

Units: bytes are bytes, durations are `std::time::Duration`, counts are rows or
edges and are named as such.

---

## 1. Opening

| item | engine call | one line |
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

There is no in-memory mode. sekejap is disk-first (`OPS_CONTRACT.md` Law 1) and a
temporary directory would be a fake of one: `Db::open` is the only constructor.

## 2. Documents

A row is addressed by `(collection, key)`. `Addr<'a> { collection, key }` is the
pair as one argument, and `("posts", "p1")` converts into it, so no call takes
two bare strings that could be swapped.

| item | engine call | one line |
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

A document is the row's fields WITHOUT the engine's internal key field, plus `_key`
set to the row's external key, so a document read back is a document that can be
written back. A declared `VECTOR(n)` field renders as a JSON array of `n`
numbers, and writing that array back writes the vector sidecar.

`Scan` is lazy: it holds at most `page_size` rows (default 256, `Scan::page_size`
sets it) and takes the read lock once per page, never for the whole walk. This
is Law 1 at the API: no call returns a collection-sized `Vec`.

```rust,signatures
pub struct Document { pub collection: String, pub key: String, pub id: EntityId, pub fields: Value }
```

## 3. SQL

The dialect is `sekejap_lang`'s, unchanged: `docs/lang/QL_CONTRACT.md` Tier 1,
with every Tier-2/Tier-3 construct REFUSED by name. This crate parses nothing
and rewrites nothing -- a statement goes to `SqlDatabase::sql` as written.

| item | engine call | one line |
|---|---|---|
| `Db::execute(sql, &[Value]) -> Result<u64>` | `SqlDatabase::sql` (`lang/src/lib.rs`) + commit; `SqlResult::Affected(n)` as `n`, `Notice` as `0` | `db.execute("INSERT INTO posts (_key, title) VALUES ($1, $2)", &[json!("p1"), json!("Hello")])?;` |
| `Db::query(sql, &[Value]) -> Result<Rows>` | `SqlDatabase::sql` → `SqlResult::Rows` | `let rows = db.query("SELECT _key, title FROM posts", &[])?;` |
| `Db::stream(sql, &[Value], page_rows, &mut f) -> Result<u64>` | `prepare_sql` + `PreparedSql::for_each_row` (`lang/src/lib.rs:399`) | `db.stream(sql, &[], 512, &mut |row| { .. Ok(()) })?;` |
| `Db::explain(sql, &[Value]) -> Result<String>` | `SqlDatabase::sql_explain` / `explain_sql` (`lang/src/lib.rs:523`) | `println!("{}", db.explain("SELECT * FROM posts", &[])?);` |
| `Db::prepare(sql) -> Result<Statement<'_>>` | `parse_sql` (`lang/src/lib.rs`), then `prepare_sql` on the first bind | `let mut s = db.prepare("SELECT _key FROM item WHERE bucket = $1")?;` |
| `Statement::query_with(&[Value]) -> Result<Rows>` | `PreparedSql::bind` + `PreparedSql::run` | `let rows = s.query_with(&[json!(3)])?;` |
| `Statement::execute_with(&[Value]) -> Result<u64>` | `PreparedSql::bind` + `PreparedSql::run_mut` + commit | `s.execute_with(&[json!("p1")])?;` |
| `Statement::stream_with(&[Value], page_rows, &mut f) -> Result<u64>` | `PreparedSql::bind` + `for_each_row` | `s.stream_with(&[json!(3)], 512, &mut |row| { .. Ok(()) })?;` |
| `Db::cache_stats() -> CacheStats` | the bounded plan cache of `QL_CONTRACT` §2 (`dist/rust/src/plans.rs`) | `assert_eq!(db.cache_stats().hits, 4);` |
| `Db::invalidate_plans()` | drops every cached plan | after changing the catalog through `Tx::database` |

**A `WHERE` needs an index on the column it filters.** A declared column with
no index is refused, by name, rather than answered by reading every row: Law 6
buys no silent scan. A filter on the key needs none -- the key is the tree's
own order.

A table declared through SQL usually has the index already. `CREATE TABLE`
indexes every ordinary column as it creates it (`docs/lang/INDEX_CONTRACT.md`);
what stays declared is `gin` full text and a `VECTOR` index, where there is a
real trade to make. `Db::create_collection` is the exception and creates NONE:
it takes a `FieldKind` list and not a declared SQL type, and the line that
contract draws -- `SMALLINT` yes, `VECTOR(n)` no -- is drawn over declared
types. Write the `CREATE INDEX` beside a `create_collection`, or declare the
collection with `execute("CREATE TABLE ...")` and get them.

The statements those three doors take, run on
[the example fixture](../lang/EXAMPLE_FIXTURE.md):

```sql
-- params: ["p07", "a new title"]
UPDATE posts SET title = $2 WHERE _key = $1;

-- params: [60]
SELECT _key, title, views FROM posts WHERE views > $1 ORDER BY views ASC LIMIT 4;

EXPLAIN SELECT _key FROM posts WHERE views > 60
```

A statement whose construct has no atomic is refused by name, with the tier
and the reason, and never rewritten into one that does:

```sql refused
-- refused 0A000: OFFSET
SELECT _key FROM posts ORDER BY views ASC LIMIT 5 OFFSET 5
```

### Prepared statements and the plan cache

`Db::query` and `Db::stream` look their statement text up in a bounded
least-recently-used plan cache, and a HIT is a REBIND: the compiled plan's
typed slots are refilled from the new parameters and nothing is parsed or
compiled. Three ceilings are fixed at open and exported --
`PLAN_CACHE_ENTRIES` (64), `PLAN_CACHE_BYTES` (256 KiB of statement text) and
`PLAN_CACHE_STATEMENT_BYTES` (8 KiB, the longest statement cached at all) --
so Law 1 holds whatever the workload does. `Db::cache_stats` reports entries,
bytes, hits, misses, evictions, the statements too long to cache, and those
three ceilings.

`Db::prepare` is the same mechanism made explicit. It PARSES -- a syntax error
is reported there and then -- and compiles on the first `*_with` call, because
this compiler folds at prepare and therefore needs the first parameter list
(`QL_CONTRACT` §2). `Statement::rebindable` is `None` until that first bind,
then `Some(true)` for a statement whose every `$n` landed in a typed slot and
`Some(false)` for one whose plan depends on a parameter VALUE --
`Statement::rebind_refusal` names which `$n` and what folded it, and a bind
then compiles again from the statement parsed ONCE. `Statement::counters`
gives `(binds, compiles)`; for a rebindable statement the second is 1 however
many times it runs.

A cache key carries the catalog generation, so a `CREATE`, `ALTER` or `DROP`
through `Db::execute` or `Tx::execute` empties the cache. A statement that
only changes ROWS does not: a plan holds no rows. A caller that changes the
catalog through `Tx::database` or a re-exported layer calls
`Db::invalidate_plans` itself.

```rust,signatures
pub struct Rows { pub columns: Arc<Vec<String>>, pub rows: Vec<Row> }
pub struct Row  { pub columns: Arc<Vec<String>>, pub id: EntityId, pub values: Vec<SqlValue> }

impl Row {
    pub fn value(&self, column: &str) -> Option<&SqlValue>;
    pub fn json(&self, column: &str) -> Option<Value>;
    pub fn to_object(&self) -> Map<String, Value>;   // MISSING columns are omitted
}
```

Both halves as a program — the body of a function returning
`Result<(), sekejap::Error>`, run by `dist/rust/tests/doc_examples.rs`:

<!-- doc_example: rust_api_parameters -->
```rust
use sekejap::{Db, FieldKind};
use serde_json::json;

let dir = std::env::temp_dir().join("sekejap-api-parameters");
let _ = std::fs::remove_dir_all(&dir);
let db = Db::open(&dir)?;
db.create_collection(
    "posts",
    &[("title", FieldKind::Text), ("views", FieldKind::Int)],
)?;
for n in 1..=5 {
    db.execute(
        "INSERT INTO posts (_key, title, views) VALUES ($1, $2, $3)",
        &[json!(format!("p{n}")), json!(format!("post {n}")), json!(n * 10)],
    )?;
}
db.execute("CREATE INDEX posts_views ON posts USING btree (views)", &[])?;

// One statement, two bindings. The second is a plan-cache HIT, which
// REBINDS the compiled plan rather than parsing and compiling again.
let sql = "SELECT _key, title FROM posts WHERE views >= $1 ORDER BY views DESC LIMIT 2";
let busy = db.query(sql, &[json!(30)])?;
let busier = db.query(sql, &[json!(40)])?;
assert_eq!(busy.len(), 2);
assert_eq!(busier.len(), 2);
assert_eq!(db.cache_stats().hits, 1);

// `Db::prepare` is the same mechanism made explicit: ONE parse for the
// life of the handle, and `counters()` reports (binds, compiles).
let mut statement = db.prepare("SELECT _key FROM posts WHERE views = $1")?;
for n in 1..=5 {
    assert_eq!(statement.query_with(&[json!(n * 10)])?.len(), 1);
}
assert_eq!(statement.counters(), (5, 1));
Ok(())
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

| item | engine call | one line |
|---|---|---|
| `Db::link(from, edge_type, to) -> Result<()>` | resolve both `Addr`s to `EntityId`, `Database::link` (`index/graph/mod.rs:1423`) with empty properties + commit | `db.link(("people","alice"), "knows", ("people","bob"))?;` |
| `Db::link_with(from, edge_type, to, &Value)` | same, with the properties object | `db.link_with(a, "knows", b, &json!({"since": 2020}))?;` |
| `Db::unlink(from, edge_type, to) -> Result<bool>` | `Database::unlink` (`:1537`) + commit | `db.unlink(a, "knows", b)?;` |
| `Db::neighbours(of, Option<&str>, Direction, limit) -> Result<Vec<Document>>` | `Database::neighbor_ids` (`:1826`) under a stated `EdgeBudget`, then `get_by_id` | `db.neighbours(("people","alice"), Some("knows"), Direction::Outgoing, 100)?;` |

Both endpoints must exist: sekejap validates them (`validate_endpoints`,
`index/graph/mod.rs:1436`). An edge to a key that has not been written is
`Error::UnknownRow`, never a dangling identity.

A traversal deeper than one hop is SQL's `GRAPH_TABLE`
(`docs/lang/QL_CONTRACT.md` §2), not a method here: the bounded walk already has
one spelling and this crate does not add a second.

## 5. Catalog

| item | engine call | one line |
|---|---|---|
| `Db::collections() -> Result<Vec<String>>` | `Database::list_collections` (new, §8) | `for name in db.collections()? { .. }` |
| `Db::describe(collection) -> Result<Option<Collection>>` | `Database::collection` + `collection_info` (`:1474`, `:1487`) + `list_indexes` (`collections/catalog.rs:887`) | `let schema = db.describe("posts")?;` |
| `Db::count_rows(collection) -> Result<u64>` | `Database::row_count` (the live record) and, only where there is none, the walk | `let n = db.count_rows("posts")?;` |
| `Db::scan_count_rows(collection) -> Result<u64>` | `Database::scan` walked to the end | `let n = db.scan_count_rows("posts")?;` |
| `Db::scan_count_all_rows() -> Result<u64>` | the same walk per collection | `let n = db.scan_count_all_rows()?;` |
| `Db::scan_count_edges() -> Result<u64>` | a walk of the primary edge keyspace | `let n = db.scan_count_edges()?;` |

```rust,signatures
pub struct Collection { pub name: String, pub fields: Vec<Field>, pub indexes: Vec<Index>, pub timestamps: bool, pub rows: Option<u64> }
pub struct Field { pub name: String, pub kind: FieldKind, pub declared: Option<String>, pub primary_key: bool }
pub struct Index { pub name: String, pub field: String, pub family: IndexFamily, pub unique: bool, pub ready: bool }
```

`FieldKind` is `sekejap_core::Kind`, re-exported: `Text`, `Int`, `Real`, `Bool`,
`Json`, `Geo`, `Point`, `Vector(n)`. `declared` is the SQL spelling the catalog
recorded where the `Kind` does not carry it (`TIMESTAMPTZ` and `DATE` are both
`Kind::Int`). The first `Field` of every collection is `_key`, `FieldKind::Text`,
`primary_key: true`: it is a declared field of every layout
(`collections/mod.rs:1350`) and naming it is how a caller addresses a row.

`Collection::rows` is the LIVE ROW COUNT, or `None` where the database keeps
none for that collection. It is a record per collection in its own keyspace
(`core/engine/src/collections/row_count.rs`, tag `0x08`, feature bit `0x2000`),
maintained by the write path inside the same transaction as the rows, so it is
exact across a crash and reading it is one `get`. `None` is not "no rows": it
is "no record" -- every database written before the record existed, and every
collection `Database::backfill_row_counts` has not reached.

The three `scan_*` counts are named that way because that is what they are: a
walk, whether or not a record exists. `Db::count_rows` is the one that reads
the record when there is one and walks when there is not, and it is named
without `scan_` for exactly that reason. sekejap still keeps no O(1) EDGE counter
(`docs/dist/OPS_CONTRACT.md` §6.1), so `scan_count_edges` remains a walk under
its own name. Law 4: a scan is called a scan.

## 6. Transactions

| item | engine call | one line |
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

Every write in §2-§4 goes THROUGH a `Tx`, including the ones that open and
commit one of their own, and in service mode a `Tx` writes through the
service's own writer. That is what puts them under `OPS_CONTRACT.md` §3-§5:
a put or a delete is one entry in the change feed, delivered by the commit
that made it durable; `Tx::execute` runs under the statement timeout and the
cancel, which is also where `BEGIN`, `COMMIT` and `ROLLBACK` as SQL are
refused. Two writes cannot be attributed to a collection by that feed and are
counted in its `unnamed_writes` instead: a statement, whose target collection
`lang` keeps to itself, and a `Statement::execute_with`, whose compiled plan
the recorded path -- which takes statement TEXT -- has no name for.
`Tx::database` is the unrecorded handle, and `Tx::note_unnamed_write` is how a
caller that uses it reports what it moved.

<!-- doc_example: rust_api_transaction -->
```rust
use sekejap::{Db, FieldKind};
use serde_json::json;

let dir = std::env::temp_dir().join("sekejap-api-transaction");
let _ = std::fs::remove_dir_all(&dir);
let db = Db::open(&dir)?;
db.create_collection("accounts", &[("balance", FieldKind::Int)])?;

// Many writes, ONE barrier. A `Tx` dropped without `commit` rolls back.
let mut tx = db.transaction()?;
tx.put(("accounts", "a"), &json!({ "balance": 100 }))?;
tx.put(("accounts", "b"), &json!({ "balance": 0 }))?;
tx.execute(
    "UPDATE accounts SET balance = $1 WHERE _key = $2",
    &[json!(40), json!("a")],
)?;
tx.commit()?;

assert_eq!(db.get(("accounts", "a"))?, Some(json!({ "_key": "a", "balance": 40 })));

// The other half of the bargain: nothing this transaction wrote survives.
let mut discarded = db.transaction()?;
discarded.put(("accounts", "c"), &json!({ "balance": 7 }))?;
discarded.rollback()?;
assert_eq!(db.get(("accounts", "c"))?, None);
Ok(())
```

`BEGIN`, `COMMIT` and `ROLLBACK` as SQL through `Db::execute` are refused in
service mode by the service's own barrier rule
(`dist/src/service/mod.rs:603`, `TRANSACTION_SQL_REFUSAL`); use `Tx`.

## 7. Maintenance and observation

| item | engine call | one line |
|---|---|---|
| `Db::checkpoint() -> Result<bool>` | `Database::checkpoint` (`:1970`); `Ok(false)` = a live reader holds a slot and the fold is DEFERRED, not failed | `db.checkpoint()?;` |
| `Db::publish() -> Result<()>` | `ServiceDatabase::publish_now` (`:275`) in service mode; `Ok(())` in single mode, where a commit is already visible to this handle | `db.publish()?;` |
| `Db::storage() -> Result<Storage>` | `Database::storage_bytes` (`:1011`) | `let s = db.storage()?; s.data_bytes + s.wal_bytes` |
| `Db::service() -> Option<&ServiceDatabase>` | the handle itself, for the change feed, the interrupt and the statement timeout | `db.service().map(\|s\| s.subscribe_changes())` |

```rust,signatures
pub struct Storage { pub data_bytes: u64, pub wal_bytes: u64 }
```

In service mode the published read view holds a reader slot for its whole life,
so `checkpoint` there answers `Ok(false)` until that view is replaced. That is
`OPS_CONTRACT.md` §1's own trade and is reported, not worked around.

What this crate does NOT offer, each because sekejap has no atomic for it and an
emulation would be the fake the eighth law forbids:

| asked for | refusal |
|---|---|
| an in-memory database | sekejap is disk-first; `Db::open` takes a directory (`OPS_CONTRACT.md` Law 1) |
| `trim_memory` | sekejap holds nothing proportional to rows to trim (`OPS_CONTRACT.md` §6.3) |
| a payload-rewriting `compact` | `checkpoint` folds the committed WAL into the data file; it does not rewrite rows (`collections/mod.rs:1970`) |
| `SHOW TABLES` / `SHOW <table>` as SQL | `QL_CONTRACT.md` §2 leaves the SHOW family to a later tier; `Db::collections` and `Db::describe` answer the same questions as data |
| `FROM MATCH ...` | not adopted; the bounded traversal is `GRAPH_TABLE` (`QL_CONTRACT.md` §2) |
| `INSERT (a)-[:t]->(b)` as SQL | edge DML has no Tier-1 spelling; `Db::link` is the call |
| `USING hash` / `USING spatial` index methods | the families are btree, gin, gist, exact and quantized (`lang/src/parser/ddl.rs:462`) |

## 8. Errors

```rust,signatures
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
| `sekejap_core::KernelError` (re-export of `kernel::Error`) | `collections::Error::Kernel` carries it, so a caller that must CLASSIFY every error a call can return -- the C ABI's `SekejapStatus`, `docs/dist/C_ABI.md` §1.1 -- cannot match the variant's payload without reaching past the layer it depends on |
| `Database::list_collections() -> Result<Vec<String>>` | the catalog's name keyspace was reachable only through `raw_for_each`, which is `#[doc(hidden)]` diagnostics |

---

Version: this document describes `sekejap` 0.17.0. `docs/dist/FFI_CONTRACT.md`
maps the 0.16 C ABI onto the same three layers and is unaffected by it.
