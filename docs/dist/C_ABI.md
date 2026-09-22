# The sekejap C ABI (0.17.0)

`dist/ffi` is the crate a foreign runtime links: the workspace member
`sekejap-capi`, with the lib name `sekejap`, so the file on disk is
`libsekejap.dylib`, `libsekejap.so` or `libsekejap.a`. The header is
`dist/ffi/include/sekejap.h`, generated from the `extern "C"` surface by
cbindgen on every build and committed as the fallback.

It is NOT a port of the 0.16 header. The 0.16 header
(`docs/dist/FFI_CONTRACT.md`) says which operations have to exist, what the
ownership rules are and how the packaging works; the SHAPE is sekejap's own,
over the published crate's `Db` (`docs/dist/RUST_API.md`) -- a row addressed
by collection and key rather than one slug string, `$n` parameters, and a
refusal by name wherever sekejap has no atomic.

Layer rule (`docs/LAYERS.md`): this crate depends on `sekejap` and nothing
else. It adds no execution: every function below is one call on `Db` plus a
JSON encode.

Units: bytes are bytes, timeouts are milliseconds, counts are rows or edges
and are named as such.

59 functions. The header is `#include <stdint.h>`-clean C99 with a
`__cplusplus` guard, so a C++ consumer includes the same file.

---

## 1. The rules a caller can rely on

| rule | what it means |
|---|---|
| opaque handles | `SekejapDb*`, `SekejapStmt*`, `SekejapTx*`, `SekejapScan*`. An open/begin call creates one; the matching close/free call destroys it, and the pointer is DANGLING after that. C may not inspect one. |
| strings IN are borrowed | every `const char*` parameter is null-terminated UTF-8 owned by the caller and is never freed here |
| strings OUT are owned | every `char*` RETURNED was allocated here and is freed ONCE with `sekejap_string_free` |
| the one exception | `sekejap_version` points into static program data. Passing it to `sekejap_string_free` hands `CString::from_raw` a pointer the allocator never gave out, which corrupts the allocator rather than freeing anything |
| sentinels | `NULL` for a failed pointer return, `-1` for a failed integer return |
| a miss is not a failure | `sekejap_get`, `sekejap_describe`, `sekejap_scan_next`/`sekejap_query_next` at the end of a walk and `sekejap_next_change` with nothing queued answer `NULL` with the status left `SekejapStatus_Ok`. A `NULL` with anything else is a failure |
| errors are THREAD-LOCAL | `sekejap_last_error(db)` and `sekejap_last_error_code(db)` read a slot owned by the CALLING THREAD. They take the handle for source compatibility with 0.16 and IGNORE it, which is what lets a failed `sekejap_open` -- with no handle to carry a message -- still report |
| a success clears the slot | both halves: `sekejap_last_error` answers `NULL` and `sekejap_last_error_code` answers `SekejapStatus_Ok` |
| no panic crosses | every entry point is wrapped in `catch_unwind`; a panic becomes the failure sentinel with `SekejapStatus_Unknown`, never undefined behaviour |
| a refusal is never an empty answer | a construct sekejap has no atomic for returns the sentinel with a reason, never a zero count or an empty JSON array |

### 1.1 `SekejapStatus`

A closed enumeration, so a wrapper maps a failure without parsing the
message. `int32_t` on the wire.

| value | name | what lands here |
|---|---|---|
| 0 | `SekejapStatus_Ok` | the last call succeeded, or answered a clean miss |
| 1 | `SekejapStatus_Refused` | a construct with no atomic, named with its reason: a Tier-2/Tier-3 statement, an in-memory open, a payload-rewriting compact, a service call on a single-mode handle, a read-only store |
| 2 | `SekejapStatus_Corrupt` | a page or a log failed verification. Nothing was changed |
| 3 | `SekejapStatus_Unsupported` | a format, policy or configuration this build does not implement |
| 4 | `SekejapStatus_Io` | the directory, the file or the medium refused |
| 5 | `SekejapStatus_Invalid` | the caller's arguments: a null pointer, text that is not UTF-8, JSON that does not parse, a collection that is not in the catalog, a parameter of the wrong type, a syntax error |
| 6 | `SekejapStatus_Busy` | a bound refused rather than waiting: a work budget, a statement deadline, a cancel, a second writer, a reader slot |
| 7 | `SekejapStatus_UnknownRow` | the named row is not in the collection, on a call that needs it to exist -- an edge endpoint |
| 8 | `SekejapStatus_Unknown` | nothing above classified it, including a panic caught at the boundary. The message is still there |

The mapping is total by construction: `sekejap::Error` has eight variants and
every one of them, through the four error types it wraps, lands on exactly one
of the nine (`dist/ffi/src/lib.rs`, `status_of`).

### 1.2 `SekejapDirection`

`SekejapDirection_Outgoing` (0), `_Incoming` (1), `_Both` (2). `int32_t`.

### 1.3 `SEKEJAP_REBIND_UNBOUND`

`2`. The third answer of `sekejap_stmt_rebindable`: the statement has not been
bound, so there is nothing to answer yet. Not a failure; `-1` is.

---

## 2. The JSON shapes

Documents, parameters and rows cross the boundary as JSON text. That is the
0.16 decision (`FFI_CONTRACT.md` §2: "UTF-8 in, JSON out"), kept because every
wrapper in `dist/bindings` already parses it and because a row-iteration ABI
is a much larger thing to get wrong.

| where | shape |
|---|---|
| a document IN (`sekejap_put`, `sekejap_tx_put`) | a JSON OBJECT. A `_key` member must equal the `key` argument |
| a document OUT (`sekejap_get`, `sekejap_scan_next`) | a JSON object with `_key` set to the row's external key, so a document read back is a document that can be written back. A declared `VECTOR(n)` field renders as a JSON array of `n` numbers |
| a parameter list | a JSON ARRAY. `NULL` or an empty string is no parameters; any other single JSON value is a one-element list |
| an answer (`sekejap_query`, `sekejap_stmt_query`, `sekejap_query_next`) | a JSON ARRAY of objects keyed by COLUMN NAME. A column that is MISSING in a row is OMITTED from that object, because missing is not null (`docs/lang/QL_CONTRACT.md` §5) |
| a scan page | a JSON ARRAY of documents |
| `sekejap_put_many` rows | a JSON array of `{"key": "...", "doc": { ... }}` |
| `sekejap_neighbours` | a JSON array of `{"collection": "...", "key": "...", "document": { ... }}`. The collection is named because a neighbour can be in another one |
| `sekejap_collections` | a JSON array of strings, in key order |
| `sekejap_describe` | `{"name", "timestamps", "rows", "fields": [...], "indexes": [...]}`. A field is `{"name", "kind", "dimension"?, "declared", "primary_key"}`; an index is `{"name", "field", "family", "unique", "ready"}` |
| `sekejap_create_collection` fields | a JSON array of `{"name", "kind", "dimension"?}` |
| `sekejap_open_with_config` | `{"budget_bytes": n, "io": "buffered"\|"direct", "sync": "full"\|"normal"\|"off"}`, every member optional |
| `sekejap_storage` | `{"data_bytes", "wal_bytes", "total_bytes"}` |
| `sekejap_next_change` | `{"sequence", "collections", "edge_types", "keys", "keys_total", "keys_truncated", "unnamed_writes", "rows_affected"}`; a key is `{"collection", "key", "kind": "put"\|"delete"}` |

A `kind` is one of `text`, `int`, `real`, `bool`, `json`, `geo`, `point`,
`vector`; `dimension` is present for `vector` only and is required when
declaring one. `declared` is the SQL spelling the catalog recorded where the
kind does not carry it (`TIMESTAMPTZ` and `DATE` are both `int`), or `null`.
An index `family` is one of `scalar`, `text`, `exact_vector`,
`quantized_vector`, `spatial_point`, `spatial_geometry`.

`describe`'s `rows` is the LIVE row count, or `null` where this database keeps
no record for the collection. `null` is "no record", not "no rows"; then the
number costs a walk, and `sekejap_scan_count_rows` is the call that says so in
its name.

`next_change`'s `keys` is empty and `keys_truncated` is `true` when the batch
moved more keys than the feed's per-event cap of 1,024: the list is dropped
whole rather than handed over half-true, and `collections` is still exact.

---

## 3. Threading

`sekejap::Db` is `Send + Sync` in both modes, so a `SekejapDb*` MAY be shared
across threads: two threads may call `sekejap_query` on one handle at the same
time, and each reads its own error slot back. This is the change from 0.16,
where `SekejapDb*` wrapped a single-threaded `CoreDB` and a second handle
(`SekejapEngine*`) existed for the concurrent case; 0.17 has one handle.

The DERIVED handles are not shared work. A `SekejapScan*`, a `SekejapStmt*`
and a `SekejapTx*` are each used from ONE thread at a time.

A `SekejapTx*` HOLDS the writer for its whole life. While one is open, a call
on the same `SekejapDb*` that needs the writer -- from this thread or another
-- WAITS for it. Commit or roll the transaction back before using the handle
for anything else.

Every derived handle borrows its database: free the `SekejapStmt*`, the
`SekejapScan*` and the `SekejapTx*` BEFORE `sekejap_close`.

---

## 4. Every function

Ownership column: "free" means free the returned `char*` with
`sekejap_string_free`. Error column: what a failure returns; the message is
`sekejap_last_error` and the code `sekejap_last_error_code` on the calling
thread, in every row.

### 4.1 Opening and identity

| signature | `Db` call | one line | error |
|---|---|---|---|
| `SekejapDb *sekejap_open(const char *path)` | `Db::open` | open the directory, creating it when it holds none | `NULL` |
| `SekejapDb *sekejap_open_with_config(const char *path, const char *config_json)` | `Db::open_with` | the same under a store configuration; a `NULL` config is sekejap's default, which is `SyncMode::Full` | `NULL` |
| `SekejapDb *sekejap_open_service(const char *path)` | `Db::open_service` | SERVICE mode: one writer, parallel readers on a published snapshot, the change feed, the statement timeout, the cancel (`docs/dist/OPS_CONTRACT.md` §1-§5) | `NULL` |
| `void sekejap_close(SekejapDb *db)` | `Db::close` | close and free. Null-safe. Uncommitted work is discarded: a close is not a commit | -- |
| `const char *sekejap_version(void)` | `sekejap::VERSION` | `MAJOR.MINOR.PATCH`. STATIC: do not free | -- |
| `int32_t sekejap_format_version(void)` | `sekejap::FORMAT_VERSION` | the disk format this build reads and writes: 2 | -- |

### 4.2 Errors and memory

| signature | one line | ownership |
|---|---|---|
| `char *sekejap_last_error(const SekejapDb *db)` | the message for the last failure ON THIS THREAD, or `NULL` after a success. `db` is accepted and ignored | free |
| `SekejapStatus sekejap_last_error_code(const SekejapDb *db)` | the code for the same, `SekejapStatus_Ok` after a success or a clean miss | -- |
| `void sekejap_string_free(char *s)` | free a string this library returned. Null-safe, exactly once, never `sekejap_version` | -- |

### 4.3 Documents

| signature | `Db` call | one line | answer / error |
|---|---|---|---|
| `int32_t sekejap_put(SekejapDb*, const char *collection, const char *key, const char *document_json)` | `Db::put` | write one document, committed before the call returns | `0` / `-1` |
| `long sekejap_put_many(SekejapDb*, const char *collection, const char *rows_json)` | `Db::put_many` | many documents into one collection under ONE commit; a failure stores none of the batch | rows / `-1` |
| `char *sekejap_get(SekejapDb*, const char *collection, const char *key)` | `Db::get` | one document with `_key` set | JSON (free) / `NULL` + `Ok` for a miss / `NULL` for a failure |
| `int32_t sekejap_exists(SekejapDb*, const char *collection, const char *key)` | `Db::exists` | whether the row is there | `1` / `0` / `-1` |
| `int32_t sekejap_delete(SekejapDb*, const char *collection, const char *key)` | `Db::delete` | delete one row and every edge that touches it, committed | `1` / `0` / `-1` |
| `SekejapScan *sekejap_scan_open(SekejapDb*, const char *collection, uintptr_t page_rows)` | `Db::scan` | a walk in stable id order holding at most `page_rows` rows (`0` means 256) | handle / `NULL` |
| `char *sekejap_scan_next(SekejapScan*)` | `Scan::next` | the next page | JSON array (free) / `NULL` + `Ok` at the end |
| `void sekejap_scan_close(SekejapScan*)` | drop | close the walk. Null-safe | -- |

`sekejap_put` to a collection that is not in the catalog is a failure, not an
implicit create: one document implies no column kinds for the documents after
it. Declare it with `sekejap_create_collection` or `CREATE TABLE`.

### 4.4 SQL

| signature | `Db` call | one line | answer / error |
|---|---|---|---|
| `long sekejap_execute(SekejapDb*, const char *sql, const char *params_json)` | `Db::execute` | one writing statement, committed. A statement that only raises a notice returns `0` | rows / `-1` |
| `char *sekejap_query(SekejapDb*, const char *sql, const char *params_json)` | `Db::query` | one row-returning statement | JSON array (free) / `NULL` |
| `char *sekejap_explain(SekejapDb*, const char *sql, const char *params_json)` | `Db::explain` | the plan the engine would build | text (free) / `NULL` |
| `SekejapStmt *sekejap_prepare(SekejapDb*, const char *sql)` | `Db::prepare` | PARSE now -- a syntax error is reported here -- and compile on the first bind | handle / `NULL` |
| `char *sekejap_stmt_query(SekejapStmt*, const char *params_json)` | `Statement::query_with` | run it as a row-returning statement | JSON array (free) / `NULL` |
| `long sekejap_stmt_execute(SekejapStmt*, const char *params_json)` | `Statement::execute_with` | run it as a writing statement and commit | rows / `-1` |
| `int32_t sekejap_stmt_rebindable(const SekejapStmt*)` | `Statement::rebindable` | whether a further bind compiles nothing | `1` / `0` / `SEKEJAP_REBIND_UNBOUND` / `-1` |
| `void sekejap_stmt_free(SekejapStmt*)` | drop | free the statement. Null-safe | -- |
| `SekejapScan *sekejap_query_open(SekejapDb*, const char *sql, const char *params_json, uintptr_t page_rows)` | `Db::stream` | run a row-returning statement and open a PAGED DELIVERY of its answer (`0` means 4,096) | handle / `NULL` |
| `char *sekejap_query_next(SekejapScan*)` | -- | the next page, the same operation as `sekejap_scan_next` | JSON array (free) / `NULL` + `Ok` at the end |
| `void sekejap_query_close(SekejapScan*)` | drop | the same operation as `sekejap_scan_close` | -- |

The `sql` argument is sekejap's own dialect, unchanged: this surface parses
nothing and rewrites nothing. What a C caller passes, and what it gets, is
what the same text does through `Db::execute` and `Db::query` — these run on
[the example fixture](../lang/EXAMPLE_FIXTURE.md):

```sql
-- sekejap_execute(db, sql, params_json): a writing statement, committed.
-- params: ["p13", "a new post", 130]
INSERT INTO posts (_key, title, views) VALUES ($1, $2, $3);

-- sekejap_query(db, sql, params_json): one row-returning statement.
-- params: [100]
SELECT _key, title, views FROM posts WHERE views >= $1 ORDER BY views ASC;

-- sekejap_explain(db, sql, params_json): the plan, as text.
EXPLAIN SELECT _key FROM posts WHERE views >= 100;

-- sekejap_prepare + sekejap_stmt_query: the same statement, one parse.
-- params: ["p13"]
SELECT _key, title FROM posts WHERE _key = $1
```

`params_json` is a JSON array, and it maps onto `$n` by the rule
`docs/dist/RUST_API.md` §3 states. A construct with no atomic is refused
through the same door, with `-1` or `NULL` and the reason in
`sekejap_last_error` — never an empty JSON array:

```sql refused
-- refused 0A000: JOIN
SELECT p.title FROM posts p JOIN people ON people._key = p.title
```

A cached plan is the `Db::query` door: the statement text is looked up in the
bounded least-recently-used plan cache of `docs/lang/QL_CONTRACT.md` §2, and a
HIT is a REBIND. `sekejap_prepare` is the same mechanism made explicit, with
one parse for the life of the handle.

`sekejap_stmt_rebindable` answers `0` for every WRITING statement, because a
write folds its document at compile. That saves the parse and nothing else,
and the call says so rather than pretending otherwise.

**What `sekejap_query_open` pages, stated rather than implied.** The ENGINE
pages the EXECUTION at `page_rows` rows, which is `Db::stream`. The ANSWER is
assembled at `sekejap_query_open`, because a compiled SELECT owns what its
request borrows -- the term strings, the query vector, the geometries -- and
therefore cannot be suspended between two C calls (`docs/dist/RUST_API.md`
§3). What paging buys a C caller is a bounded string per call and the freedom
to stop reading; it does not bound the answer. `sekejap_query` has the same
property with the whole answer in one string, because `Db::query` assembles a
`Rows` too.

### 4.5 Edges

| signature | `Db` call | one line | answer / error |
|---|---|---|---|
| `int32_t sekejap_link(SekejapDb*, const char *from_collection, const char *from_key, const char *edge_type, const char *to_collection, const char *to_key)` | `Db::link` | link two rows with a typed edge in the base graph context, committed | `0` / `-1` |
| `int32_t sekejap_link_with(..., const char *properties_json)` | `Db::link_with` | the same, carrying a JSON properties object | `0` / `-1` |
| `int32_t sekejap_unlink(SekejapDb*, ...)` | `Db::unlink` | remove one edge, committed | `1` / `0` / `-1` |
| `char *sekejap_neighbours(SekejapDb*, const char *collection, const char *key, const char *edge_type, SekejapDirection, uintptr_t limit)` | `Db::neighbours` | the rows one hop away, in one direction. `edge_type` may be `NULL` for every type | JSON array (free) / `NULL` |

Both endpoints must EXIST: a missing one is `SekejapStatus_UnknownRow`, never
a dangling identity (`docs/core/GRAPH_CONTRACT.md` §4).

A neighbour answer is complete or refused, under a bound of 256 edges
(`GRAPH_CONTRACT.md` §4.1). A `limit` above it is REFUSED by name, and the
reason names `GRAPH_TABLE` as the wider walk. A traversal deeper than one hop
is SQL's `GRAPH_TABLE` (`QL_CONTRACT.md` §2), not a second spelling here.

### 4.6 The catalog

| signature | `Db` call | one line | answer / error |
|---|---|---|---|
| `int32_t sekejap_create_collection(SekejapDb*, const char *name, const char *fields_json)` | `Db::create_collection` | declare a collection | `1` created / `0` already there / `-1` |
| `int32_t sekejap_drop_collection(SekejapDb*, const char *name)` | `Db::drop_collection` | remove it, its rows, its indexes and its descriptor | `1` / `0` / `-1` |
| `char *sekejap_collections(SekejapDb*)` | `Db::collections` | every collection name, in key order | JSON array (free) / `NULL` |
| `char *sekejap_describe(SekejapDb*, const char *collection)` | `Db::describe` | the declared shape of one collection | JSON object (free) / `NULL` + `Ok` for no such collection |
| `long sekejap_count_rows(SekejapDb*, const char *collection)` | `Db::count_rows` | the rows, from the LIVE record when this database keeps one and from the walk when it does not | count / `-1` |
| `long sekejap_scan_count_rows(SekejapDb*, const char *collection)` | `Db::scan_count_rows` | the rows BY WALKING them, whether or not a record exists | count / `-1` |
| `long sekejap_scan_count_edges(SekejapDb*)` | `Db::scan_count_edges` | every edge BY WALKING the primary edge keyspace | count / `-1` |

The two `scan_count_*` calls are named that way because that is what they are.
sekejap keeps no O(1) edge counter (`OPS_CONTRACT.md` §6.1), so the edge count
stays a walk under its own name. Law 4: a scan is called a scan.

The declaration is a floor and not a fence: a document may carry a field the
declaration does not name, and it is stored in the row's extras map.

### 4.7 Transactions

| signature | `Tx` call | one line | answer / error |
|---|---|---|---|
| `SekejapTx *sekejap_tx_begin(SekejapDb*)` | `Db::transaction` | take the writer for many writes under ONE barrier | handle / `NULL` |
| `int32_t sekejap_tx_put(SekejapTx*, const char *collection, const char *key, const char *document_json)` | `Tx::put` | write one document, NOT committed | `0` / `-1` |
| `int32_t sekejap_tx_delete(SekejapTx*, const char *collection, const char *key)` | `Tx::delete` | delete one row, NOT committed | `1` / `0` / `-1` |
| `int32_t sekejap_tx_link(SekejapTx*, ...)` | `Tx::link` | link two rows, NOT committed | `0` / `-1` |
| `long sekejap_tx_execute(SekejapTx*, const char *sql, const char *params_json)` | `Tx::execute` | one writing statement, NOT committed | rows / `-1` |
| `int32_t sekejap_tx_commit(SekejapTx*)` | `Tx::commit` | commit and FREE the handle | `0` / `-1` |
| `int32_t sekejap_tx_rollback(SekejapTx*)` | `Tx::rollback` | roll back and FREE the handle | `0` / `-1` |

Every plain call in §4.3-§4.6 commits before it returns: durability per call.
`SekejapTx*` is the opposite bargain -- many writes, one barrier -- and the
two are the whole story: there is no third auto-commit toggle to get wrong.

`sekejap_tx_commit` and `sekejap_tx_rollback` free the handle WHETHER OR NOT
they succeeded: the pointer is dangling after either, in both cases. A handle
freed any other way ROLLS BACK, because committing on a stray free would make
an abandoned batch durable.

### 4.8 Maintenance

| signature | `Db` call | one line | answer / error |
|---|---|---|---|
| `int32_t sekejap_checkpoint(SekejapDb*)` | `Db::checkpoint` | fold the committed write-ahead log into the data file | `1` folded / `0` DEFERRED / `-1` |
| `int32_t sekejap_publish(SekejapDb*)` | `Db::publish` | make the newest commit visible to readers now | `0` / `-1` |
| `char *sekejap_storage(SekejapDb*)` | `Db::storage` | the bytes on disk | JSON object (free) / `NULL` |

`0` from `sekejap_checkpoint` is DEFERRED, not failed: a live reader holds a
slot. In service mode that is every call, because the published read view
holds one for its whole life (`OPS_CONTRACT.md` §1). In single mode
`sekejap_publish` has no view to swap and every commit is already visible to
this handle, so it succeeds having done nothing.

### 4.9 Service mode

Every call in this section is REFUSED BY NAME on a handle that was not opened
with `sekejap_open_service`: the sentinel, `SekejapStatus_Refused`, and a
message naming the call and saying that single mode has no writer to time out,
no interrupt and no change feed.

| signature | service call | one line | answer / error |
|---|---|---|---|
| `int32_t sekejap_statement_timeout_ms(SekejapDb*, uint64_t milliseconds)` | `set_statement_timeout` / `clear_statement_timeout` | refuse a statement that runs longer. `0` milliseconds CLEARS the timeout | `0` / `-1` |
| `int32_t sekejap_cancel(SekejapDb*)` | `ServiceDatabase::cancel` | cancel the work in flight, from any thread. STICKY until cleared | `0` / `-1` |
| `int32_t sekejap_clear_interrupt(SekejapDb*)` | `clear_interrupt` | clear a cancel so the service accepts work again | `1` one was standing / `0` none was / `-1` |
| `long sekejap_subscribe(SekejapDb*)` | `subscribe_changes` | subscribe to the commit-time change feed | id / `-1` |
| `char *sekejap_next_change(SekejapDb*, long subscription, uint64_t timeout_ms)` | `Receiver::try_recv` / `recv_timeout` | the next event. `0` milliseconds polls and returns at once | JSON object (free) / `NULL` + `Ok` when none arrived |
| `int32_t sekejap_unsubscribe(SekejapDb*, long subscription)` | `unsubscribe` | close one subscription | `1` it was open / `0` it was not / `-1` |

A subscriber's queue is bounded and the service never waits for a slow one: a
subscription that falls behind loses events, and the `sequence` an event
carries is what tells a listener exactly how many
(`OPS_CONTRACT.md` §5). The feed is owned by the `SekejapDb*`, so an id is
usable from any thread and a subscription left open is closed by
`sekejap_close`.

`unnamed_writes` is the count of writes the feed could not attribute to a
collection: SQL DML and DDL, and a write run through a PREPARED statement
handle, whose compiled plan the recorded path -- which takes statement TEXT --
cannot name a collection for. A listener that sees it above zero re-runs its
query, exactly as one past the key cap does.

### 4.10 Refused by name

`docs/dist/RUST_API.md` §7 lists what sekejap does not offer, each because
there is no atomic underneath and an emulation would be a fake. The four a C
caller would otherwise reach for keep a SYMBOL, so the refusal arrives with a
name and a reason rather than as a link error or, worse, a plausible wrong
answer. All four always fail, with `SekejapStatus_Refused`.

| signature | refusal |
|---|---|
| `SekejapDb *sekejap_open_memory(void)` | sekejap is disk-first and has no in-memory store (`OPS_CONTRACT.md` Law 1); `sekejap_open` takes a directory. A temporary directory would be a fake of an ephemeral store, so this does not make one |
| `int32_t sekejap_trim_memory(SekejapDb*)` | sekejap holds nothing proportional to rows to trim: the buffer pool is bounded by `budget_bytes` and the plan cache by its three ceilings (`OPS_CONTRACT.md` §6.3). A no-op that returned success would be a fake of reclaim |
| `int32_t sekejap_compact(SekejapDb*)` | there is no payload-rewriting compaction; `sekejap_checkpoint` folds the committed write-ahead log into the data file and does not rewrite rows |
| `char *sekejap_show(SekejapDb*, const char *statement)` | the `SHOW` family has no Tier-1 spelling (`QL_CONTRACT.md` §2); `sekejap_collections` and `sekejap_describe` answer the same questions as DATA |

The rest of §7 refuses through `sekejap_execute` and `sekejap_query`, because
those constructs are SQL and `sekejap_lang` refuses them by name with their
tier: `FROM MATCH`, edge DML as SQL, `USING hash` / `USING spatial` index
methods, and every other Tier-2/Tier-3 construct. A refusal is `-1` or `NULL`
with the reason in `sekejap_last_error`, never an empty JSON array.

---

## 5. Packaging

```sh
cargo build --release -p sekejap-capi     # libsekejap.{dylib,so} and libsekejap.a
cd dist/ffi && make check                 # compiles and RUNS examples/smoke.c
cd dist/ffi && make install PREFIX=~/.local
cc app.c $(pkg-config --cflags --libs sekejap) -o app
```

| file | what |
|---|---|
| `dist/ffi/Cargo.toml` | `sekejap-capi`, lib name `sekejap`, `cdylib` + `staticlib`, depending on the published crate and `serde_json` only |
| `dist/ffi/build.rs` | regenerates the header with cbindgen on every build. Best-effort: a cbindgen failure warns and the COMMITTED header stays authoritative |
| `dist/ffi/cbindgen.toml` | the header preamble and style |
| `dist/ffi/include/sekejap.h` | the generated header, committed |
| `dist/ffi/Makefile` | `build`, `header`, `check`, `install`, `uninstall`, `clean`. Honours `CARGO_TARGET_DIR` |
| `dist/ffi/sekejap.pc.in` | the pkg-config template `make install` fills in |
| `dist/ffi/examples/smoke.c` | what `make check` compiles and runs |
| `dist/ffi/tests/abi.rs` | the ABI exercised through its C signatures, against an oracle held in the test process |

Two naming consequences of the lib target having to be called `sekejap`,
because that is `libsekejap` on disk:

- The dependency on the published crate is renamed `sekejap-rs` in
  `Cargo.toml`, with `use sekejap_rs as sekejap;` at the top of `src/lib.rs`,
  so the two do not claim one name in the same build.
- `tests/abi.rs` is compiled as the crate's own test module
  (`#[cfg(test)] #[path = "../tests/abi.rs"] mod abi;`, with
  `autotests = false`) rather than as an integration test. An integration
  test needs an `rlib` beside the C library, and an `rlib` named
  `libsekejap` collides with the published crate's. It is the same test
  either way -- the `extern "C"` entry points, called with `CString`/`CStr`
  and raw pointers -- and that the LINK works is covered by `make check`
  compiling `examples/smoke.c` with a C compiler against the built library.
  Run it with `cargo test -p sekejap-capi --lib`.

---

Version: this document describes `libsekejap` 0.17.0.
`docs/dist/FFI_CONTRACT.md` §1 maps every symbol of the 0.16 header onto it.
