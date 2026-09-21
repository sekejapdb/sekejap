# FFI contract — e1 `sekejap.h` mapped onto e4

Read-only map of the e1 C ABI (`wrappers/c/include/sekejap.h`, generated from
`wrappers/c/src/lib.rs` by cbindgen) onto e4 HEAD. The C header is 41 `extern
"C"` functions (counted from the declarations in `sekejap.h:47-328`), not ~72.
Nothing in this document is an implementation.

Layers: `dist` is the foreign-runtime surface
(`docs/LAYERS.md:53-57`, `dist/src/lib.rs:1-13`). `lang` turns text into engine
calls (`docs/LAYERS.md:41-51`). `core` owns `Database`
(`core/engine/src/collections/mod.rs:797`). The registry name is `sekejap`
(`wrappers/README.md:25`).

Status key used in §1:

| status | meaning |
|---|---|
| READY NOW | e4 call exists at HEAD; the C function can be a thin wrapper |
| READY AFTER D1 (DML / SERVICE / SCHEMA) | blocked on that D1 worker |
| LATER (contract row) | named in `QL_CONTRACT` or `OPS_CONTRACT`, not in D1 |
| REFUSED (reason) | the C symbol stays; the caller gets the failure sentinel and a message, never a substitute answer |

Failure sentinels are the e1 header's: `NULL` for pointers, `-1` for integers
(`sekejap.h:12-13`, `wrappers/c/src/lib.rs:28-30`).

---

## 0. Shared ownership and error rules

Copied from the e1 header and `lib.rs` design comment. e4's FFI must keep them
so existing C callers stay valid.

| rule | e1 | e4 FFI |
|---|---|---|
| Opaque handles | `SekejapDb*`, `SekejapEngine*`, `SekejapStmt*` (`sekejap.h:27-37`) | same typedefs; inner types below |
| Strings IN | borrowed UTF-8 `const char*`; library does not free (`sekejap.h:11`, `lib.rs:17-18`) | same |
| Strings OUT | heap `char*` owned by caller; free once with `sekejap_string_free` (`sekejap.h:9-11`, `lib.rs:15-17`, `lib.rs:748-754`) | same; `CString::into_raw` / `from_raw` |
| Exception | `sekejap_version` is static; do not free (`sekejap.h:258-259`, `lib.rs:19-25`, `lib.rs:756-760`) | same |
| `SekejapDb` errors | handle-local `last_error: Option<String>` (`lib.rs:45-57`); `sekejap_last_error(db)` clones it to a heap C string (`lib.rs:732-741`) | handle-local on the `SekejapDb` box. **Not** thread-local. |
| `SekejapEngine` errors | thread-local `ENGINE_ERR` (`lib.rs:788-797`); `sekejap_engine_last_error` has no handle (`sekejap.h:325-328`, `lib.rs:996-1000`) | thread-local, like `errno` |
| Panic | every entry is `catch_unwind`; a panic is an error return (`lib.rs:26-27`, `lib.rs:99-120`) | same |
| Null handle | integer ops return `-1`; string ops return `NULL` (`lib.rs:105-107`, `lib.rs:129-131`) | same |
| Params JSON | null or empty → no params; a JSON array is the list; any other JSON value is a one-element list (`lib.rs:80-97`) | parse into `Vec<Param>` (`lang/src/lib.rs:240-248`) with the same JSON rules |

`SqlDatabase::sql` takes `&mut self` because writes go through `put` / `delete`
/ `create_*` (`lang/src/lib.rs:522-531`). e4 `put` writes the working tree;
`commit` is the durability barrier (`collections/mod.rs:1580-1586`,
`:1796-1808`). e1 `put` / `execute` append the WAL before mutating
(`src/lib.rs:5316-5328`, `:9411-9413`). To keep the C ABI, the FFI handle
must auto-commit after a mutating call unless the caller has an open
`BEGIN`…`COMMIT` (e4 `BEGIN` is a notice that the writer is already in a
transaction; `COMMIT` calls `Database::commit` — `lang/src/compile/plan.rs:837-845`).

Slug convention on the C side is `"collection/key"` (`sekejap.h:84`, `:166`).
e4 splits that into `Database::collection(name)` + `put`/`get`/`delete(c, key)`
(`collections/mod.rs:1346-1357`, `:1580`, `:1677`, `:1763`). The internal key
field is `__e4_key` (`collections/mod.rs:83`); SQL spells it `_key`
(`lang/src/lib.rs:109-112`).

---

## 1. Every function in `sekejap.h`

### 1.1 Lifecycle (`SekejapDb`)

| # | C signature (`sekejap.h`) | what e1 does | e4 call(s) | status | ownership / error |
|---|---|---|---|---|---|
| 1 | `SekejapDb *sekejap_open(const char *path)` `:47` | `CoreDB::open` (`src/lib.rs:2547-2549`, via `open_impl` `lib.rs:168-201`) | If the directory has no `data`/`writer.lock`, `Database::create(path, Config)`; else `Database::open(path, Config)` (`collections/mod.rs:798-844`). `Config` is `kernel::store::Config { budget_bytes, io, sync }` (`core/kernel/src/store.rs:67-68`). | READY NOW | returns boxed handle or `NULL`. Open failure does not set `last_error` in e1 (`lib.rs:193-199` drops the `io::Error`). e4 FFI should set a thread-unrelated open-error string **or** keep e1's silent `NULL`; pick one and document it. Recommendation: set a process-level last-open-error readable via a static, because `sekejap_last_error` needs a handle (`sekejap.h:249`). |
| 2 | `SekejapDb *sekejap_open_paged(const char *path)` `:54` | `CoreDB::open_paged` — mmap topology (`src/lib.rs:2559-2561`, `lib.rs:178-180`) | e4 has one store: the page-WAL. `Database::open` / `create` (`collections/mod.rs:798-844`). There is no second "resident" layout. | READY NOW | same as `sekejap_open`. Not a fake of paged mode: e4's only mode is paged. |
| 3 | `void sekejap_close(SekejapDb *db)` `:61` | `Box::from_raw`; null-safe (`lib.rs:210-217`) | `drop(Database)` | READY NOW | null-safe; pointer dangling after. |
| 4 | `SekejapDb *sekejap_open_read_only(const char *path)` `:116` | `CoreDB::open_read_only` (`src/lib.rs:2578-2583`, `lib.rs:370-385`); writes error | `Database::open_snapshot(path, Config)` (`collections/mod.rs:852-869`; OPS §1 ingredient `OPS_CONTRACT.md:56-61`). Writes return `Error::ReadOnly` (`collections/mod.rs:23`). | READY NOW | `NULL` on failure. Snapshot holds a reader slot and defers checkpoint (`collections/mod.rs:846-851`). |

### 1.2 SQL on `SekejapDb`

| # | C signature | what e1 does | e4 call(s) | status | ownership / error |
|---|---|---|---|---|---|
| 5 | `long sekejap_execute(SekejapDb *db, const char *sql)` `:71` | `CoreDB::execute` (`src/lib.rs:9411-9413`, `lib.rs:230-258`); DDL/DML/BEGIN/COMMIT/edges | `SqlDatabase::sql(&mut db, sql, &[])` (`lang/src/lib.rs:531-554`). T1 writes: `INSERT`/`UPDATE … WHERE _key =`/`DELETE … WHERE _key =`/`CREATE TABLE`/`CREATE INDEX`/`DROP`/`BEGIN`/`COMMIT`/`ROLLBACK` (`lang/src/lib.rs:20-21`, `:71-86`; `compile/plan.rs:735-849`). Returns `SqlResult::Affected(n)` as `n`, `Notice` as `0`. | READY NOW for the T1 list. Predicate `UPDATE`/`DELETE`/`FROM ALL`: READY AFTER D1 (DML) (`QL_CONTRACT.md:41-42`, brief-dml). `ALTER TABLE`, column `DEFAULT`/`NOT NULL`: READY AFTER D1 (SCHEMA) (`QL_CONTRACT.md:49-53`, brief-schema). `COMPACT` as SQL: LATER (`QL_CONTRACT.md:55`). `SHOW` via execute: LATER (`QL_CONTRACT.md:62-64`). `FROM MATCH`: REFUSED — spelling not adopted (`QL_CONTRACT.md:263-270`, `E3_PARITY.md:112`). | `-1` + handle `last_error`. A T2/T3 construct is `SqlError::Refused` (`lang/src/lib.rs:148-154`, `refuse.rs:7-8`); that is an error code, not a scan. Auto-commit per §0. |
| 6 | `char *sekejap_query(SekejapDb *db, const char *sql)` `:82` | `CoreDB::query` then `collect_rows_json` (`src/lib.rs:8717-8731`, `lib.rs:272-278`, `:150-159`) | `SqlDatabase::sql` → `SqlResult::Rows { columns, rows }` (`lang/src/lib.rs:272-276`, `:569-575`) encoded as a JSON array (see §2). Paging: `PreparedSql::for_each_row` / `PreparedQuery::next_page` (`lang/src/lib.rs:388-408`, `query/page.rs:942-947`) under `QueryBudget::unlimited()` (`query/mod.rs:517-533`). | READY NOW for T1 `SELECT` (`QL_CONTRACT.md:35-36`, `lang/src/lib.rs:23-24`). `FROM ALL`: READY AFTER D1 (DML). `FROM MATCH`: REFUSED (`QL_CONTRACT.md:263-270`). | heap JSON; free with `sekejap_string_free`. `NULL` + `last_error` on error. |
| 7 | `long sekejap_execute_params(db, sql, params_json)` `:125` | `CoreDB::execute_params` (`src/lib.rs:9432-9437`, `lib.rs:397-410`) | `sql(text, &params)` with `parse_params` → `Vec<Param>` (`lib.rs:80-97`, `lang/src/lib.rs:240-248`) | READY NOW (same statement set as #5) | `-1` + `last_error`. `params_json` may be null. |
| 8 | `char *sekejap_query_params(db, sql, params_json)` `:134` | `CoreDB::query_params` (`src/lib.rs:8879-8882`, `lib.rs:420-431`) | `sql(text, &params)` as #6 | READY NOW (same as #6) | heap JSON; free; `NULL` + `last_error`. |
| 9 | `SekejapStmt *sekejap_prepare(db, sql)` `:142` | `CoreDB::prepare` → `sql::PreparedQuery` (`src/lib.rs:8848-8850`, `lib.rs:449-480`) | `SqlDatabase::sql_prepare` / `prepare_sql` (`lang/src/lib.rs:545-546`, `:475-477`) → boxed `PreparedSql` (`lang/src/lib.rs:287-290`) | READY NOW for T1. T2/T3 refuse at prepare (`refuse.rs:7-8`). | `NULL` + `last_error` on parse/refuse. Stmt is independent of later SQL text; it still needs a live `db` to run (`lang/src/lib.rs:351-359` borrows `&Database`). |
| 10 | `char *sekejap_query_prepared(db, stmt, params_json)` `:150` | `CoreDB::query_prepared` (`src/lib.rs:8853-8860`, `lib.rs:489-507`) | `PreparedSql::rows` / `for_each_row` (`lang/src/lib.rs:388-456`) | READY NOW | heap JSON; free. Null `stmt` → `NULL` (`lib.rs:494-496`). |
| 11 | `void sekejap_stmt_free(SekejapStmt *stmt)` `:156` | `Box::from_raw`; null-safe (`lib.rs:514-519`) | `drop(PreparedSql)` | READY NOW | null-safe. |
| 12 | `char *sekejap_show(db, sql)` `:164` | `CoreDB::show` (`src/lib.rs:9130-9141`, `lib.rs:528-540`); `SHOW TABLES`/`EDGES`/`<table>` | Sugar over `db_*` catalog rows (`QL_CONTRACT.md:62-64`); `SHOW STATUS`/`STORAGE` are OPS §6 (`OPS_CONTRACT.md:272-321`). Not built: `dist/src/service/mod.rs:4-7`, lang grammar has no `SHOW` (`lang/src/lib.rs:20-21`). | LATER (`QL_CONTRACT.md:62-64` SHOW family; `OPS_CONTRACT.md:36` §6; `E3_PARITY.md:46`, `:119`, `:138-140`) | heap JSON; free. Until built: `NULL` + `last_error` naming the contract row. Never return an empty array as success. |

### 1.3 Direct node / edge (no SQL)

| # | C signature | what e1 does | e4 call(s) | status | ownership / error |
|---|---|---|---|---|---|
| 13 | `char *sekejap_get(db, slug)` `:91` | `CoreDB::get` payload JSON; miss is not an error (`src/lib.rs:7664-7674`, `lib.rs:288-311`, `sekejap.h:84-87`) | Split slug; `Database::get(c, key)` (`collections/mod.rs:1677-1678`) → `Entity { document }` (`:148-152`) serialised as JSON. Miss: `Ok(None)` → `NULL`, `last_error` cleared. | READY NOW | heap JSON or `NULL`. Distinguish miss vs error via `sekejap_last_error` (`sekejap.h:86-87`). |
| 14 | `int32_t sekejap_put(db, slug, payload_json)` `:171` | `CoreDB::put` (`src/lib.rs:5316-5328`, `lib.rs:550-560`) | Split slug; `collection(name)` must exist (e4 has no implicit collection); `Database::put(c, key, &Value)` (`collections/mod.rs:1580-1586`); FFI auto-commit. | READY NOW | `0` / `-1`. Payload must be a JSON object (`collections/mod.rs:1570-1571`). |
| 15 | `long sekejap_put_many(db, rows_json)` `:179` | JSON object `{slug: payload}` → `put_value_bulk` (`src/lib.rs:5453-5457`, `lib.rs:569-583`) | Parse object; N × `put` then one `commit` (OPS §7: e4 is already "many puts, then one commit" — `OPS_CONTRACT.md:369-373`). Named `begin_bulk`/`end_bulk` scope: READY AFTER D1 (DML) (`OPS_CONTRACT.md:375-376`, brief-dml C). | READY NOW as N puts + one commit. The D1 bulk **scope** is DML. | count or `-1`. Failed batch: e4 `rollback` (`collections/mod.rs:1825-1826`); stronger than e1 leaving prefix stored (`OPS_CONTRACT.md:385-390`). |
| 16 | `int32_t sekejap_remove(db, slug)` `:186` | `CoreDB::remove`; success whether or not it existed (`src/lib.rs:5791`, `lib.rs:591-597`) | `Database::delete(c, key)` returns `bool` (`collections/mod.rs:1763-1766`); C still returns `0` on miss. Graph 6.1 RESTRICT may refuse (`QL_CONTRACT.md:39`). | READY NOW | `0` / `-1`. Predicate delete: READY AFTER D1 (DML). |
| 17 | `int32_t sekejap_link(db, from, to, edge_type)` `:193` | `CoreDB::link` by slug hashes; endpoints need not exist (`src/lib.rs:5811-5825`, `lib.rs:605-618`) | Resolve both slugs to `EntityId` via `get`; `Database::link(source, edge_type, dest, "", &json!({}))` (`index/graph/mod.rs:1423-1430`). Endpoints **must** exist (`validate_endpoints`, `:1436`). | READY NOW | `0` / `-1`. Missing endpoint is an error (e4), not a dangling hash (e1). State that deviation. SQL `INSERT INTO GRAPH … EDGE`: LATER (`QL_CONTRACT.md:43`). |
| 18 | `int32_t sekejap_link_meta(db, from, to, edge_type, meta_json)` `:200` | `CoreDB::link_meta` (`src/lib.rs:5829-5845`, `lib.rs:626-640`) | Same as #17 with `properties = serde_json::from_str(meta_json)` | READY NOW | `0` / `-1`. Invalid JSON → `-1`. |
| 19 | `int32_t sekejap_unlink(db, from, to, edge_type)` `:210` | `CoreDB::unlink` (`src/lib.rs:5849`, `lib.rs:647-660`) | Resolve slugs; interned type id; `Database::delete_edge(EdgeKey {…})` (`index/graph/mod.rs:1510-1522`) | READY NOW | `0` / `-1`. Miss of the edge is `Ok(false)` in e4 (`:1518-1522`); C returns `0`. |

### 1.4 Introspection

| # | C signature | what e1 does | e4 call(s) | status | ownership / error |
|---|---|---|---|---|---|
| 20 | `int32_t sekejap_contains(db, slug)` `:216` | `CoreDB::contains` (`src/lib.rs:8116-8118`, `lib.rs:669-674`) | `get(c, key)?.is_some()` | READY NOW | `1` / `0` / `-1` |
| 21 | `long sekejap_node_count(db)` `:222` | `CoreDB::node_count` overlay+base (`src/lib.rs:8184-8192`, `lib.rs:681-683`) | No O(1) counter. OPS §6.1: node count is a scan, optional (`OPS_CONTRACT.md:288-293`, `E3_PARITY.md:138`). Implementation: walk collections (`header` next id `collections/mod.rs:1153-1155`) and `scan` each (`:1781-1794`). | LATER (`OPS_CONTRACT.md:36` §6.1) if the C function must stay cheap. READY NOW only as a labelled scan. Recommendation: implement as scan, `EXPLAIN` of the equivalent `count(*)` is a scan (`QL_CONTRACT.md:283`). | count or `-1` |
| 22 | `long sekejap_edge_count(db)` `:228` | `CoreDB::edge_count` (`src/lib.rs:8205-8211`, `lib.rs:690-692`) | Same as #21 for the edge keyspace. | LATER (`OPS_CONTRACT.md:36` §6.1) / scan | count or `-1` |
| 23 | `char *sekejap_collection_names(db)` `:235` | `CoreDB::collection_names` schemas ∪ live rows (`src/lib.rs:8217-8258`, `lib.rs:700-706`) | Walk name keyspace prefix `0x10` (`name_key` `collections/mod.rs:442-446`) via `raw_for_each` (`:944-955`); JSON-array of names. | READY NOW (composition; `raw_for_each` is `doc(hidden)` so the FFI crate in `dist` needs a public `list_collections` or a lang `SHOW TABLES` later) | heap JSON array; free |
| 24 | `char *sekejap_schema_ddl(db, collection)` `:242` | `CoreDB::schema_ddl` from declared schema (`src/lib.rs:8262-8274`, `lib.rs:714-722`); `NULL` if no schema | `collection_info` (`collections/mod.rs:1359-1369`) → render `CREATE TABLE` from `layout.fields` + `declared`. `NULL` if the name is unknown. SQL `SHOW CREATE TABLE`: LATER (`QL_CONTRACT.md:62`). | READY NOW as a renderer over `CollectionInfo` | heap string or `NULL` (clean miss, `last_error` cleared — same as e1 `Ok(d.inner.schema_ddl(c))` `lib.rs:720`) |

### 1.5 Maintenance on `SekejapDb`

| # | C signature | what e1 does | e4 call(s) | status | ownership / error |
|---|---|---|---|---|---|
| 25 | `int32_t sekejap_compact(db)` `:98` | `CoreDB::compact` rewrite snapshot, truncate WAL (`src/lib.rs:5980-5982`, `lib.rs:321-340`) | `Database::checkpoint` (`collections/mod.rs:1813-1819`; `QL_CONTRACT.md:55`). `Ok(false)` while a reader holds a slot: C should return `0` and not wait (deferred is success of the call, matching "never waits"). SQL `COMPACT`: LATER (`QL_CONTRACT.md:55`). | READY NOW as `checkpoint` | `0` / `-1`. Deviation vs e1: does not rewrite payloads; folds committed WAL (`collections/mod.rs:1810-1812`). |
| 26 | `void sekejap_trim_memory(db)` `:104` | `CoreDB::trim_memory` shrink maps, return heap (`src/lib.rs:6701-6705`, `lib.rs:347-352`) | `Database::memory_report` / `trim_memory` not built (`OPS_CONTRACT.md:323-355`, `E3_PARITY.md:161`). | LATER (`OPS_CONTRACT.md:36` §6.3; `E3_PARITY.md:161` row 87) | void. Until built: no-op is a fake — **refuse by leaving the symbol and doing nothing only if the header says it cannot fail**. e1 cannot fail (`void`). Keep the no-op **and** document it as "caches already bounded; nothing to trim" (OPS §6.3: e4 holds nothing proportional to rows — `OPS_CONTRACT.md:347-353`). That is not a fake of reclaim; it is the honest empty report. |
| 27 | `int32_t sekejap_sync(db)` `:110` | `CoreDB::sync` fsync WAL (`src/lib.rs:7358-7362`, `lib.rs:359-361`) | `Database::commit` is the FULL barrier (`collections/mod.rs:1796-1808`). If auto-commit already ran, a second `commit` on a clean handle is the e4 "nothing pending" path. | READY NOW as `commit` | `0` / `-1` |

### 1.6 Errors, memory, version

| # | C signature | what e1 does | e4 call(s) | status | ownership / error |
|---|---|---|---|---|---|
| 28 | `char *sekejap_last_error(const SekejapDb *db)` `:249` | clone handle `last_error` (`lib.rs:732-741`) | same field on the FFI box | READY NOW | heap C string or `NULL`; **handle-local, not thread-local**. Free with `sekejap_string_free`. |
| 29 | `void sekejap_string_free(char *s)` `:256` | `CString::from_raw`; null-safe (`lib.rs:749-754`) | same | READY NOW | never pass `sekejap_version` (`lib.rs:19-25`) |
| 30 | `const char *sekejap_version(void)` `:259` | `CARGO_PKG_VERSION` static (`lib.rs:758-760`) | `env!("CARGO_PKG_VERSION")` of the dist/ffi crate | READY NOW | static; do not free |

### 1.7 Concurrent engine (`SekejapEngine`)

e1: `SekejapEngine` wraps `sekejap::engine::Engine` (`lib.rs:770-786`,
`src/engine/mod.rs:1-13`): `Send + Sync`, parallel readers, one writer,
thread-local errors (`sekejap.h:29-31`, `lib.rs:764-768`).

e4: `ServiceDatabase` is OPS §1, **not built** (`dist/src/service/mod.rs:4-7`,
`OPS_CONTRACT.md:43-78`, `E3_PARITY.md:138` row 74). D1 worker SERVICE
(`brief-service.md`) builds it in `dist/src/service/` over
`Mutex<Database>` + `RwLock<Arc<Database>>` snapshots.

| # | C signature | what e1 does | e4 call(s) | status | ownership / error |
|---|---|---|---|---|---|
| 31 | `SekejapEngine *sekejap_engine_open(const char *path)` `:265` | `Engine::builder(p).build()` (`src/engine/mod.rs:249-254`, `:919`; `lib.rs:858-873`) | `ServiceDatabase::open(path, config)` (OPS §1 atomic `OPS_CONTRACT.md:63-67`; brief-service §1) | READY AFTER D1 (SERVICE) | `NULL` + thread-local error. Until SERVICE: `NULL` + `"ServiceDatabase not built (OPS_CONTRACT §1)"`. |
| 32 | `SekejapEngine *sekejap_engine_open_memory(void)` `:268` | `Engine::memory()` over `CoreDB::new()` (`src/engine/mod.rs:297-309`, `src/lib.rs:2465-2466`, `lib.rs:877-880`). Header: "Never fails → non-null" (`sekejap.h:267`). | e4 `Database` is path-backed (`create`/`open` `collections/mod.rs:798-844`). No in-memory store. OPS does not name one. Law 1 is disk-first (`OPS_CONTRACT.md:21-25`). Creating a temp directory would be a fake of "ephemeral memory". | REFUSED (e4 has no in-memory engine; disk-first, no `Database::new`) | Header claims non-null. Honour the **sentinel contract** over the comment: return `NULL`, set thread-local `"sekejap_engine_open_memory refused: e4 has no in-memory store (OPS_CONTRACT / Law 1)"`. Do not tempdir. |
| 33 | `void sekejap_engine_close(SekejapEngine *e)` `:275` | `Box::from_raw`; do not call while other threads use it (`lib.rs:888-893`, `sekejap.h:270-271`) | `ServiceDatabase::close` (brief-service §1) | READY AFTER D1 (SERVICE) | null-safe |
| 34 | `char *sekejap_engine_query(const SekejapEngine *e, sql)` `:283` | `Engine::query` read lock / snapshot (`src/engine/mod.rs:323-334`, `lib.rs:902-910`) | `reader()` snapshot + `prepare_sql` / `PreparedSql::rows` on `&Database` (`lang/src/lib.rs:529-530`, `:475`). Reads must not take the writer mutex (`OPS_CONTRACT.md:69-70`). | READY AFTER D1 (SERVICE) | heap JSON; free; thread-local error |
| 35 | `char *sekejap_engine_query_params(e, sql, params_json)` `:289` | `Engine::query_params` (`src/engine/mod.rs:340-350`, `lib.rs:917-927`) | same as #34 with `Param`s | READY AFTER D1 (SERVICE) | heap JSON; free |
| 36 | `long sekejap_engine_execute(e, sql)` `:296` | `Engine::execute` exclusive write; may buffer (`src/engine/mod.rs:497-520`, `lib.rs:935-943`) | `writer()` guard + `SqlDatabase::sql` (`lang/src/lib.rs:531`); SERVICE `publish` after commit (OPS §2 `OPS_CONTRACT.md:111-115`) | READY AFTER D1 (SERVICE) | `-1` + thread-local. Statement set as #5. |
| 37 | `long sekejap_engine_execute_params(e, sql, params_json)` `:302` | `Engine::execute_params` bypasses buffer (`src/engine/mod.rs:530-538`, `lib.rs:950-960`) | same as #36 with params | READY AFTER D1 (SERVICE) | `-1` + thread-local |
| 38 | `long sekejap_engine_flush(e)` `:311` | drain write buffer, apply, maybe compact (`src/engine/mod.rs:597-602`, `lib.rs:968-970`); `Ok(0)` if no buffer | e4 has no statement buffer. Map to "commit unpublished writer work + `publish`". Empty pending → `0`, matching e1 empty buffer (`src/engine/mod.rs:600-602`). | READY AFTER D1 (SERVICE) | count or `-1`. Do not invent a SQL buffer. |
| 39 | `int32_t sekejap_engine_compact(e)` `:317` | `Engine::compact` exclusive (`src/engine/mod.rs:707-714`, `lib.rs:977-979`) | writer `checkpoint` (`collections/mod.rs:1813`) | READY AFTER D1 (SERVICE) (needs the writer half) | `0` / `-1` |
| 40 | `void sekejap_engine_trim_memory(e)` `:323` | `Engine::trim_memory` (`src/engine/mod.rs:694-696`, `lib.rs:986-991`) | OPS §6.3 | LATER (`OPS_CONTRACT.md:36` §6.3) | void; same honest no-op as #26 until built |
| 41 | `char *sekejap_engine_last_error(void)` `:328` | thread-local `ENGINE_ERR` (`lib.rs:788-797`, `:996-1000`) | thread-local `RefCell<Option<CString>>` | READY NOW (can exist before SERVICE; open/query will set it) | heap copy; free with `sekejap_string_free`. **Thread-local.** |

Count: 41 functions. `sekejap.hpp` (`wrappers/c/include/sekejap.hpp`) is a C++
RAII wrapper over a subset; it is not additional C ABI.

---

## 2. Result-set encoding

### 2.1 What e1 hands to C

Not a cursor, not columnar, not a row iterator.

Query path (`wrappers/c/src/lib.rs:150-159`, `:272-278`, `:420-431`, `:489-507`;
header `sekejap.h:73-78`):

1. Run SQL → `sekejap::Set`.
2. `collect()` every hit into RAM.
3. Each hit becomes its **payload JSON object**, or `{"_slug": "<slug>"}` if
   the payload is missing.
4. `serde_json::to_string` of a JSON **array**. One heap `char*`.

`sekejap_show` uses the same array-of-payload-objects shape (`lib.rs:528-539`).
`sekejap_get` returns one payload JSON object, not an array (`lib.rs:288-305`,
`sekejap.h:84-87`). `sekejap_collection_names` is a JSON array of strings
(`lib.rs:700-706`).

Design comment: "UTF-8 in, JSON out … This avoids a fragile row-iteration ABI"
(`lib.rs:11-14`).

Dart's FRB path is a **different** JSON shape: `[{"slug":...,"payload":...}]`
(`wrappers/dart/rust/src/api/simple.rs:187-198`). That is not the C ABI.
Python returns `Hit` objects, not JSON (`wrappers/python/src/lib.rs:13-27`,
`:346`). Go/Swift/Kotlin/C/Lua parse the C JSON array
(`wrappers/go/sekejap.go:82-101`, `wrappers/swift/.../Sekejap.swift:36-40`,
`wrappers/kotlin/.../Sekejap.kt:40-41`).

### 2.2 What e4 should do (keep the same header)

e1 already uses JSON text, so JSON text is the encoding
(brief: "rows as JSON text is acceptable only if e1 does that"). Do **not**
replace `char *sekejap_query` with a binary cursor. The header stays.

e4's native answer is columnar, not a JSON bag:

```text
SqlResult::Rows { columns: Vec<String>, rows: Vec<SqlRow> }   lang/src/lib.rs:272-276
SqlRow { id: EntityId, values: Vec<SqlValue> }                 lang/src/lib.rs:266-269
SqlValue = Missing | Null | Bool | Int | Float | Text | Json | Id   lang/src/lib.rs:252-263
```

Paging already exists: `PreparedQuery::next_page(page_size, budget, cancelled)`
(`query/page.rs:942-947`, `QueryPage` `query/mod.rs:656-662`) and
`PreparedSql::for_each_row` (`lang/src/lib.rs:388-408`). The C function still
assembles one JSON array so callers of the header do not change.

**Envelope (same as e1):** a JSON array string.

**Object per row (the one documented change inside the envelope):**

| e1 C ABI | e4 C ABI (same `char*`) |
|---|---|
| payload object as stored, or `{"_slug": "..."}` (`lib.rs:150-157`) | object whose keys are `SqlResult` column names, values are `SqlValue` as JSON |

`SqlValue` → JSON:

| `SqlValue` (`lang/src/lib.rs:252-263`) | JSON |
|---|---|
| `Missing` | omit the key (e4 Missing ≠ Null; omitting matches "field not in this row") |
| `Null` | `null` |
| `Bool` | `true`/`false` |
| `Int` | number |
| `Float` | number |
| `Text` | string |
| `Json` | embedded JSON value |
| `Id(EntityId)` | string `"<collection_id>:<sequence>"` or a JSON object `{"collection":u32,"sequence":u64}` — pick object; it is lossless |

`sekejap_get` stays one JSON **object** (the `Entity.document`, with `_key`
injected if the caller expects a payload bag). Do not wrap get in an array.

`sekejap_show` stays an array of objects once SHOW exists.

Do not ship a binary cursor in `sekejap.h`. If a later wire/FFI wants one, it
is `QL_CONTRACT.md:59` `DECLARE … BINARY CURSOR` (T2, p3-wire), a new header,
not a silent change to `sekejap_query`.

---

## 3. Threading: `engine_*` vs `ServiceDatabase`

### 3.1 e1

| handle | concurrency | error slot | close |
|---|---|---|---|
| `SekejapDb*` | not concurrent; wraps `CoreDB` (`sekejap.hpp` comment `sekejap.hpp` is C++; Go: `wrappers/go/sekejap.go:28-30`; Swift: `Sekejap.swift:12-13`; Kotlin: `Sekejap.kt:15-16`) | handle-local (`lib.rs:45-47`) | `sekejap_close` |
| `SekejapEngine*` | `Engine` is `Send + Sync`; many threads may call the same pointer (`sekejap.h:29-31`, `lib.rs:781-783`, `src/engine/mod.rs:5-7`) | thread-local (`lib.rs:788-797`, `sekejap.h:325-328`) | `sekejap_engine_close` **not** while others still use it (`sekejap.h:270-271`) |

Reads on `Engine` take a shared lock or a published snapshot
(`src/engine/mod.rs:323-328`). Writes serialise (`src/engine/mod.rs:497-520`).

### 3.2 e4

OPS §1 (`OPS_CONTRACT.md:43-84`): `ServiceDatabase` owns one `Mutex<Database>`
writer and one `RwLock<Arc<Database>>` snapshot over `Database::open_snapshot`
(`collections/mod.rs:852`). Laws: L6 — a reader opened before a commit does
not see it; no read takes the writer lock; minting a snapshot past the
`readers` bound **refuses**, never blocks (`OPS_CONTRACT.md:69-76`). A second
writer process is T3 (`OPS_CONTRACT.md:80-83`).

`WorkMeter` already threads a cancel closure through every charge
(`query/mod.rs:664-696`, `OPS_CONTRACT.md:189-199`). SERVICE adds
`InterruptHandle` and a deadline (`OPS_CONTRACT.md:131-212`, brief-service §3-4).

`SqlDatabase::sql` needs `&mut self` (`lang/src/lib.rs:525-531`); snapshot
reads use `prepare_sql` + `with_query` on `&Database` (`:529-530`).

### 3.3 What the C side must guarantee

1. **`SekejapDb*`:** one thread at a time, or the caller serialises. The FFI
   will not put a mutex on it (e1 did not). Handle-local `last_error` would
   race if they share it.
2. **`SekejapEngine*`:** may be shared across threads after SERVICE. Callers
   still must not `sekejap_engine_close` concurrently with in-flight calls
   (`sekejap.h:270-271`).
3. **Do not** use `SekejapDb*` as a server handle. That is what `engine_*` is
   for (`lib.rs:764-768`; OPS §1 `OPS_CONTRACT.md:53-54`).
4. Cancellation from another thread is SERVICE (`OPS_CONTRACT.md:176-187`);
   the C ABI has no interrupt symbol today. Do not add one without a header
   bump. Postgres `CancelRequest` is OPS §9.2 (`OPS_CONTRACT.md:448-456`).
5. `sekejap_engine_last_error` is per calling thread. `sekejap_last_error(db)`
   is per handle. Do not mix them.

---

## 4. Proposed `dist/src/ffi/` layout and cbindgen

`dist` already names the foreign surface and has empty `service` / `pg`
modules (`dist/src/lib.rs:8-16`, `dist/src/service/mod.rs:1-7`,
`dist/src/pg/mod.rs:1-6`). Wrappers are listed, not built
(`dist/bindings/README.md:1-19`).

### 4.1 Files

```text
dist/
  cbindgen.toml              # listing below; drives include/sekejap.h
  include/
    sekejap.h                # generated; commit the artifact as e1 does
  src/
    lib.rs                   # already: pub mod pg; pub mod service;
    ffi/
      mod.rs                 # cdylib surface: re-export no_mangle fns
      db.rs                  # SekejapDb { inner: Database, last_error, in_txn }
      engine.rs              # SekejapEngine { inner: ServiceDatabase }
      stmt.rs                # SekejapStmt { inner: PreparedSql }
      json.rs                # parse_params, rows_to_json, sqlvalue_to_json, slug_split
      error.rs               # handle last_error + thread_local ENGINE_ERR
      guard.rs               # catch_unwind helpers (e1 lib.rs:99-148)
    service/mod.rs           # SERVICE worker fills this; engine.rs calls it
    pg/mod.rs                # unchanged placeholder
    cli/                     # operator bins; not linked into libsekejap
```

`dist/Cargo.toml` today is `sekejap-dist` rlib + bins (`dist/Cargo.toml:8-43`).
Add a second crate **or** a `crate-type` on a `dist/ffi` package so the CLI
and the cdylib do not share one artifact:

```toml
# dist/ffi/Cargo.toml  (preferred: keep sekejap-dist as bins)
[package]
name = "sekejap-capi"
# library file name libsekejap.{dylib,so,a} — e1 wrappers/c/Cargo.toml:10-13
[lib]
name = "sekejap"
crate-type = ["cdylib", "staticlib"]
path = "src/lib.rs"   # or ../src/ffi/mod.rs via a thin crate

[dependencies]
sekejap-dist = { path = ".." }      # ServiceDatabase after SERVICE
sekejap-lang = { path = "../../lang" }
sekejap-core = { path = "../../core/engine" }
serde_json = { version = "1", features = ["float_roundtrip"] }
```

e1's crate is already named `sekejap-capi` with `lib.name = "sekejap"`
(`wrappers/c/Cargo.toml:2-13`). Keep both names.

`mod.rs` sketch:

```rust
//! C ABI. Header: dist/include/sekejap.h. Same symbols as e1 sekejap.h.
mod db;
mod engine;
mod error;
mod guard;
mod json;
mod stmt;
pub use db::*;
pub use engine::*;
pub use stmt::*;
```

`json.rs` slug split: first `/` separates collection and key (header
`sekejap.h:84`). No slash → error `"slug must be collection/key"`.

### 4.2 `cbindgen.toml` (listing)

Taken from e1 `wrappers/c/cbindgen.toml:1-43`, with the engine ifdef removed
because the committed e1 header already emits `engine_*` unconditionally
(`sekejap.h:261-328` has no `#if SEKEJAP_ENGINE`).

```toml
# cbindgen configuration for sekejap-capi (e4 dist/ffi).
# Drives build.rs (`cbindgen --output include/sekejap.h`).

language = "C"
include_guard = "SEKEJAP_H"
tab_width = 4
style = "type"
cpp_compat = true
documentation = true
documentation_style = "c99"

header = """
/*
 * sekejap.h — C ABI for sekejap (https://sekejap.life)
 *
 * AUTO-GENERATED from dist/src/ffi by cbindgen. Do not edit by hand.
 *
 * Ownership: SekejapDb* is opaque (open* creates, close frees). Any char* the
 * library RETURNS is yours — free it once with sekejap_string_free (except
 * sekejap_version, which is static). Strings you PASS IN are borrowed UTF-8.
 * Failure sentinels: NULL for pointers, -1 for integers; sekejap_last_error(db)
 * has the message. Engine errors are thread-local (sekejap_engine_last_error).
 * No Rust panic crosses the boundary.
 */
"""

[export]
prefix = ""

[parse]
parse_deps = false

[enum]
prefix_with_name = true
```

Do **not** keep e1's `[defines] "feature = engine" = "SEKEJAP_ENGINE"`
(`wrappers/c/cbindgen.toml:36-39`): wrapping `engine_*` in `#ifdef` would
break Go/Swift/Kotlin that include the header as-is.

`build.rs` can be copied from e1 `wrappers/c/build.rs:1-34` (best-effort
cbindgen; committed header is the fallback).

### 4.3 Wrapper-by-wrapper porting

e1 consumption (`wrappers/README.md:3-17`): most native bindings sit on
`libsekejap`; Python and Node use language-native FFI.

| wrapper | binds via | against e4 `libsekejap` with the **same header** | glue to re-point |
|---|---|---|---|
| C (`wrappers/c`) | the ABI itself | **unchanged** | none |
| C++ (`sekejap.hpp:1-16`) | includes `sekejap.h` | **unchanged** (no `engine_*` in the C++ class — `sekejap.hpp:64-187`) | none |
| Go (`wrappers/go/sekejap.go:17-19`) | cgo `#include "sekejap.h"` | **unchanged** | none. Symbols used: `open`, `open_paged`, `close`, `execute`, `query`, `query_params`, `prepare`, `query_prepared`, `stmt_free`, `put`, `get`, `link`, `link_meta`, `contains`, `node_count`, `edge_count`, `compact`, `version`, `last_error`, `string_free` (`sekejap.go` grep). Does not call `engine_*`. |
| Swift (`Sources/CSekejap/sekejap.h` + `Sekejap.swift:1-40`) | SwiftPM module map over C | **unchanged** | none. Subset: open/execute/query/query_params/put/get/link/contains/counts/compact/prepare/query_prepared/version. |
| Kotlin Panama (`Ffi.java:89-110`, `Sekejap.kt:21-98`) | JDK 22 FFM downcalls | **unchanged** | none. Subset as in `Ffi.java` MethodHandles. |
| Lua (`wrappers/lua/sekejap.c:1-12`) | Lua C module over `sekejap.h` | **unchanged** | none |
| C# (`Native.cs:5-16`, `SekejapDb.cs:36-38`) | P/Invoke `Lib = "sekejap"` | **unchanged** (same `long` vs Windows `C long` caveat, `Native.cs:9-12`) | none |
| React Native JSI (`react-native/cpp/sekejap-jsi.h:1-11`) | JSI HostObject over C ABI | **unchanged** once the scaffold links | none (scaffold) |
| Dart (`wrappers/dart/rust`) | flutter_rust_bridge, **not** `dart:ffi` against `sekejap.h` (`simple.rs:1-5`, `wrappers/README.md:12`) | **must re-point Rust glue** | see list below |
| Python (`wrappers/python/src/lib.rs:1-9`) | PyO3 on `sekejap::CoreDB` | **must re-point** | see list below |
| Node (`wrappers/node/src/lib.rs:1-16`) | napi-rs on `CoreDB` | **must re-point** | see list below |

#### Dart FRB glue (`wrappers/dart/rust/src/api/simple.rs`)

Today: `SekejapDb(Mutex<CoreDB>)` (`simple.rs:21-22`). Re-point `CoreDB` →
`sekejap_core::collections::Database` + `sekejap_lang::SqlDatabase`, or call
the C ABI from Dart (would drop FRB). If keeping FRB, rewrite these functions
(`simple.rs` `pub fn` list):

| function | line | e4 target |
|---|---|---|
| `init_app` | 11 | keep FRB init |
| `db_open` | 27 | `Database::open` / `create` |
| `db_new` | 34 | REFUSED (no in-memory) or tempdir — same as `engine_open_memory` |
| `db_execute` | 42 | `SqlDatabase::sql` |
| `db_put` | 50 | `Database::put` |
| `db_put_many` | 58 | N puts + commit |
| `db_set_wal_sync` | 68 | `Config.sync` at open only (`store.rs:65-68`); live change is LATER |
| `db_mobile_profile` | 82 | LATER (no `AutoCompact` / live `SyncMode` on `Database`) |
| `db_remove` | 89 | `Database::delete` |
| `db_link` / `db_unlink` | 94, 99 | `link` / `delete_edge` |
| `db_watch_open` / `stream` / `close` | 141-183 | OPS §5 change feed — LATER (`OPS_CONTRACT.md:214-268`); not in `sekejap.h` |
| `db_query` / `db_query_params` | 189, 203 | `sql`; JSON shape today includes `slug`+`payload` (`simple.rs:194-196`) — Dart-specific, not C ABI |
| `db_prepare` / `db_query_prepared` | 226, 235 | `sql_prepare` / `for_each_row` |
| `db_execute_params` | 255 | `sql` with params |
| `db_get` / `db_contains` | 264, 269 | `get` |
| `db_show` | 274 | LATER SHOW |
| `db_compact` / `db_sync` | 287, 292 | `checkpoint` / `commit` |

#### Python PyO3 glue (`wrappers/python/src/lib.rs`)

`PyDB { inner: Option<CoreDB> }` (`lib.rs:136-138`). Re-point to `Database`.
Methods that exist on `CoreDB` but **not** on `sekejap.h` stay Python-only
and follow the same READY/LATER/REFUSED map:

| method | line | in `sekejap.h`? | e4 |
|---|---|---|---|
| `new` / `open_paged` / `open_read_only` | 149, 161, 167 | yes (`open`, `open_paged`, `open_read_only`) | §1.1 |
| `put` `get` `remove` `contains` `put_many` | 174-197 | yes | §1.3 |
| `begin_bulk` `end_bulk` | 203-210 | no | READY AFTER D1 (DML) OPS §7 |
| `put_vector` `get_vector` | 214-220 | no | `Kind::Vector` field on `put`/`get` document — READY NOW as JSON field, not a sidecar API |
| `link` `link_meta` `unlink` `link_many` | 226-248 | link/meta/unlink yes; `link_many` no | §1.3; `link_many` = loop + one commit |
| `unlink_where` `update_edge` | 253-260 | no | LATER (`QL_CONTRACT.md:44-45` edge UPDATE/DELETE) |
| `edges_from` `edges_to` `edges_between` | 265-276 | no | graph neighbor APIs in core; not C ABI |
| `query` `prepare` `query_prepared` `execute` | 346-417 | yes | §1.2 |
| `explain` | 400 | no (C uses `query("EXPLAIN …")`) | `sql_explain` READY NOW (`lang/src/lib.rs:503-511`, `:547-548`; E3_PARITY row 19 DONE `:48`) even though `QL_CONTRACT.md:60` still says EXPLAIN T2 |
| `show` | 437 | yes | LATER |
| `collection_names` `schema_ddl` `node_count` `edge_count` | 445-468 | yes | §1.4 |
| `all_slugs` | 471 | no | scan; not C ABI |
| `bm25_search` | 477 | no | SQL `ORDER BY bm25(col, q)` T1 (`QL_CONTRACT.md:226`) |
| `compact` `trim_memory` `memory_report` | 488-499 | compact/trim yes; report no | §1.5 / OPS §6.3 |
| `set_hnsw_ef_search` | 506 | no | `SET LOCAL ef_search` (`QL_CONTRACT.md:203`) |
| `close` | 512 | `sekejap_close` | drop |

#### Node napi glue (`wrappers/node/src/lib.rs`)

`Db { inner: Mutex<Option<CoreDB>> }` (`lib.rs:15-17`). Same re-point as
Python. napi methods: `open` `execute` `query` `query_params` `execute_params`
`watch` `unwatch` `prepare` `query_prepared` `put` `link` `node_count`
`edge_count` `compact` `open_paged` `open_read_only` `get` `contains` `remove`
`put_many` `begin_bulk` `end_bulk` `put_vector` `get_vector` `link_meta`
`link_many` `unlink` `unlink_where` `update_edge` `edges_from` `edges_to`
`edges_between` `collection_names` `all_slugs` `schema_ddl` `bm25_search`
`explain` `show` `trim_memory` `memory_report` `set_hnsw_ef_search` `close`
`version` (`lib.rs` `#[napi]` list). `watch`/`unwatch` = OPS §5 LATER.

---

## 5. What e4 must not reproduce, and names to keep

### 5.1 Keep (the C ABI identity)

| name | where |
|---|---|
| registry / lib / crate lib name `sekejap` | `wrappers/README.md:25`; `wrappers/c/Cargo.toml:12` `name = "sekejap"` → `libsekejap` |
| crate package `sekejap-capi` | `wrappers/c/Cargo.toml:2` |
| header guard `SEKEJAP_H` | `sekejap.h:17-18`, `cbindgen.toml:6` |
| types `SekejapDb`, `SekejapEngine`, `SekejapStmt` | `sekejap.h:27-37` |
| every `sekejap_*` symbol in §1 | `sekejap.h:47-328` |
| sentinels `NULL` / `-1` | `sekejap.h:12-13` |
| `sekejap_string_free` ownership | `sekejap.h:9-11` |
| JSON-array `char*` query results | `sekejap.h:73-78`, `lib.rs:11-14` |
| `long` for execute/counts (not `int64_t`) | `sekejap.h:71`; Windows caveat `Native.cs:9-12` — keep `long` to not break the header |
| Maven `life.sekejap`, pub.dev/PyPI/npm `sekejap` | `wrappers/README.md:27-35` |

### 5.2 Do not reproduce in the C header

| item | reason |
|---|---|
| A binary / columnar cursor replacing `char *sekejap_query` | e1 is JSON (`lib.rs:150-159`); changing the header breaks Go/Swift/Kotlin/C/Lua/C# |
| `#ifdef SEKEJAP_ENGINE` around `engine_*` | committed e1 header emits them unconditionally (`sekejap.h:261-328`) |
| `FROM MATCH` as a working spelling | not adopted (`QL_CONTRACT.md:263-270`); C `query` must refuse, not rewrite silently |
| An in-memory success path for `sekejap_engine_open_memory` via tempdir | fake of `Engine::memory()` (`src/engine/mod.rs:297`); REFUSED in §1 #32 |
| Python/Node/Dart extras as C symbols (`put_vector`, `watch`, `begin_bulk`, `edges_from`, `memory_report`, `set_hnsw_ef_search`, `db_new`, `db_mobile_profile`) | not in `sekejap.h`; adding them is a new ABI. They stay language glue |
| e1 payload-bag `{"_slug":...}` as the only row shape | e4 rows are projected columns (`lang/src/lib.rs:272-276`); keep the JSON **array** envelope, not the bag |
| `sekejap.hpp` inside `sekejap.h` | C++ wrapper (`sekejap.hpp:1-16`), a consumer |
| Emitting `0` affected / empty JSON array for a T2/T3 statement | eighth law: refuse with a named reason (`lang/src/lib.rs:11-15`, `refuse.rs:7-8`; brief: "never a fake") |
| Per-write fsync that e1 WAL-appends implied, implemented as a weakened e4 commit | OPS §7: same FULL barrier; weakened bulk durability is T3 (`OPS_CONTRACT.md:380-383`) |
| `DECLARE … BINARY CURSOR` symbols in this header | T2 p3-wire (`QL_CONTRACT.md:59`); a later header |

### 5.3 Header comments e4 should edit (same symbols)

The comment on `sekejap_query` that each element is "the row's payload object
(or `{"_slug": "..."}`)" (`sekejap.h:73-75`) is e1-specific. After e4, the
comment should say: JSON array of objects keyed by the SELECT list, values as
in §2. The **signature** does not change.

The comment on `sekejap_engine_open_memory` "Never fails → non-null"
(`sekejap.h:267`) is false on e4. Keep the symbol; document the `NULL` +
thread-local error. That is a comment fix, not a new ABI.

---

## 6. Status counts (the 41)

| status | functions |
|---|---|
| READY NOW | open, open_paged, close, open_read_only, execute, query, execute_params, query_params, prepare, query_prepared, stmt_free, get, put, put_many (as N puts+commit), remove, link, link_meta, unlink, contains, collection_names (via name-key walk), schema_ddl (renderer), compact (checkpoint), sync (commit), last_error, string_free, version, engine_last_error (slot only) |
| READY AFTER D1 (SERVICE) | engine_open, engine_close, engine_query, engine_query_params, engine_execute, engine_execute_params, engine_flush, engine_compact |
| READY AFTER D1 (DML) | execute/query of predicate `UPDATE`/`DELETE`/`FROM ALL`; named bulk scope (put_many already composable) |
| READY AFTER D1 (SCHEMA) | execute of `ALTER TABLE` / `DEFAULT` / `NOT NULL` |
| LATER | show (QL §2 SHOW / OPS §6); node_count, edge_count (OPS §6.1 as O(1)); trim_memory, engine_trim_memory (OPS §6.3); SQL `COMPACT` as a statement |
| REFUSED | engine_open_memory (no in-memory store); `FROM MATCH` inside query/execute (spelling not adopted) |

D1 workers: DML `brief-dml.md`, SERVICE `brief-service.md`, SCHEMA
`brief-schema.md`. SHOW, trim_memory, write_trace, change feed, interrupt
are OPS order-of-work items 4-8 (`OPS_CONTRACT.md:508-521`), not D1.
