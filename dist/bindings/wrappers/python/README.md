# sekejap for Python

An embedded, disk-first, multi-model database: documents addressed by
collection and key, SQL with `$n` parameters over the same rows, typed edges
between them, and vector and spatial fields in the one store.

This package is **pure Python**. It compiles nothing and it contains no Rust:
it loads `libsekejap` — the shared library built from
[`dist/ffi`](../../../ffi/), whose contract is
[`docs/dist/C_ABI.md`](../../../../docs/dist/C_ABI.md) — and calls its 59
entry points through `ctypes`. A platform wheel ships that library inside the
package; an install from the source distribution finds one on the machine.

```python
from sekejap import Db

with Db("./data") as db:
    db.create_collection("venues", [
        {"name": "name", "kind": "text"},
        {"name": "suburb", "kind": "text"},
        {"name": "capacity", "kind": "int"},
    ])
    db.put("venues", "the_tote", {"name": "The Tote", "suburb": "Collingwood"})

    for row in db.query("SELECT _key, name FROM venues WHERE suburb = $1",
                        ["Collingwood"]):
        print(row["_key"], row["name"])
```

## Install

```sh
pip install sekejap
```

A wheel for your platform carries the library and needs nothing else. If you
installed from the source distribution, or you want to run against a library
you built yourself:

```sh
cargo build --release -p sekejap-capi          # in the sekejap repository
export SEKEJAP_LIBRARY=/path/to/libsekejap.dylib
```

`SEKEJAP_LIBRARY` is looked at first, then `sekejap/_lib/` inside the package,
then the platform loader path (`DYLD_LIBRARY_PATH`, `LD_LIBRARY_PATH`, `PATH`,
and wherever `make install` in `dist/ffi` put it). `sekejap.library_path()`
says which one this process loaded.

## The handles

One class per opaque pointer in the C ABI. Each is a context manager, and
closing a `Db` closes every handle taken from it first, because each of them
borrows it.

| class | from | what it is |
|---|---|---|
| `Db` | `Db(path)`, `Db.open_service(path)` | the database. May be shared across threads |
| `Statement` | `db.prepare(sql)` | one statement, parsed once, compiled at its first bind |
| `Scan` | `db.scan(collection)`, `db.stream(sql)` | a paged walk. Iterating yields documents; `.pages()` yields pages |
| `Tx` | `db.transaction()` | the writer, held across many writes under one barrier |

## Errors

A failure raises. The exception carries `code`, a `Status` enumeration member,
so you branch on the code and never on the text of `message`.

```python
from sekejap import Db, Refused, Status

try:
    db.compact()
except Refused as refusal:
    print(refusal.code is Status.REFUSED, refusal.message)
```

`SekejapError` is the base; `Refused`, `Corrupt`, `Unsupported`, `IoFailure`,
`Invalid`, `Busy` and `UnknownRow` are the named ones. A **miss** is not a
failure and does not raise: `db.get` and `db.describe` answer `None`, a walk
answers `None` at its end, and `db.next_change` answers `None` when nothing
arrived.

## Every C function, and the call that reaches it

| C function | Python |
|---|---|
| `sekejap_open` | `Db(path)` / `Db.open(path)` |
| `sekejap_open_with_config` | `Db(path, config={"budget_bytes": ..., "io": ..., "sync": ...})` |
| `sekejap_open_service` | `Db.open_service(path)` / `Db(path, service=True)` |
| `sekejap_close` | `db.close()`, or leaving a `with` block |
| `sekejap_version` | `sekejap.version()` |
| `sekejap_format_version` | `sekejap.format_version()` |
| `sekejap_last_error` | `sekejap.last_error()`, and `error.message` on every exception |
| `sekejap_last_error_code` | `sekejap.last_error_code()`, and `error.code` |
| `sekejap_string_free` | inside the wrapper: every returned string is copied and freed once |
| `sekejap_put` | `db.put(collection, key, document)` |
| `sekejap_put_many` | `db.put_many(collection, rows)` — a dict, pairs, or `{"key", "doc"}` objects |
| `sekejap_get` | `db.get(collection, key)` → `dict` or `None` |
| `sekejap_exists` | `db.exists(collection, key)` |
| `sekejap_delete` | `db.delete(collection, key)` |
| `sekejap_scan_open` | `db.scan(collection, page_rows=0)` |
| `sekejap_scan_next` | `scan.next_page()`, `scan.pages()`, `iter(scan)` |
| `sekejap_scan_close` | `scan.close()` |
| `sekejap_execute` | `db.execute(sql, params)` → rows moved |
| `sekejap_query` | `db.query(sql, params)` → `list[dict]` |
| `sekejap_explain` | `db.explain(sql, params)` |
| `sekejap_prepare` | `db.prepare(sql)` |
| `sekejap_stmt_query` | `statement.query(params)` |
| `sekejap_stmt_execute` | `statement.execute(params)` |
| `sekejap_stmt_rebindable` | `statement.rebindable` → `True` / `False` / `None` (not bound yet) |
| `sekejap_stmt_free` | `statement.close()` |
| `sekejap_query_open` | `db.stream(sql, params, page_rows=0)` |
| `sekejap_query_next` | `scan.next_page()` |
| `sekejap_query_close` | `scan.close()` |
| `sekejap_link` | `db.link(from_collection, from_key, edge_type, to_collection, to_key)` |
| `sekejap_link_with` | the same call with `properties={...}` |
| `sekejap_unlink` | `db.unlink(...)` |
| `sekejap_neighbours` | `db.neighbours(collection, key, edge_type, direction, limit)` |
| `sekejap_create_collection` | `db.create_collection(name, fields)` |
| `sekejap_drop_collection` | `db.drop_collection(name)` |
| `sekejap_collections` | `db.collections()` |
| `sekejap_describe` | `db.describe(collection)` → `dict` or `None` |
| `sekejap_count_rows` | `db.count_rows(collection)` |
| `sekejap_scan_count_rows` | `db.scan_count_rows(collection)` |
| `sekejap_scan_count_edges` | `db.scan_count_edges()` |
| `sekejap_tx_begin` | `db.transaction()` |
| `sekejap_tx_put` | `tx.put(collection, key, document)` |
| `sekejap_tx_delete` | `tx.delete(collection, key)` |
| `sekejap_tx_link` | `tx.link(...)` |
| `sekejap_tx_execute` | `tx.execute(sql, params)` |
| `sekejap_tx_commit` | `tx.commit()`, or a clean exit from `with db.transaction()` |
| `sekejap_tx_rollback` | `tx.rollback()`, or an exception inside that block |
| `sekejap_checkpoint` | `db.checkpoint()` → `True` folded, `False` deferred |
| `sekejap_publish` | `db.publish()` |
| `sekejap_storage` | `db.storage()` |
| `sekejap_statement_timeout_ms` | `db.statement_timeout_ms(ms)` |
| `sekejap_cancel` | `db.cancel()` |
| `sekejap_clear_interrupt` | `db.clear_interrupt()` |
| `sekejap_subscribe` | `db.subscribe()` |
| `sekejap_next_change` | `db.next_change(subscription, timeout_ms=0)` |
| `sekejap_unsubscribe` | `db.unsubscribe(subscription)` |
| `sekejap_open_memory` | `sekejap.open_memory()` — **refused**: sekejap is disk-first |
| `sekejap_trim_memory` | `db.trim_memory()` — **refused**: nothing proportional to rows is held |
| `sekejap_compact` | `db.compact()` — **refused**: there is no payload-rewriting compaction |
| `sekejap_show` | `db.show(statement)` — **refused**: `collections()` and `describe()` answer as data |

The four refusals keep their names so the reason arrives as a sentence rather
than as an `AttributeError`.

## Service mode

`Db.open_service(path)` is one writer, parallel readers on a published
snapshot, and the commit-time change feed. Every service call on a
single-mode handle is refused by name.

```python
db = Db.open_service("./data")
subscription = db.subscribe()
db.put("venues", "the_tote", {"name": "The Tote"})
event = db.next_change(subscription, timeout_ms=1000)
print(event["sequence"], event["keys"])
```

## pandas

`db.df` is a namespace, and pandas is imported only when you touch it.

```python
frame = db.df.query("SELECT _key, name, capacity FROM venues")
db.df.put(frame, "venues")          # the index is the key
```

## A shell

```sh
python -m sekejap ./data                    # a prompt
python -m sekejap ./data "SELECT * FROM venues"
```

## Running the tests

```sh
SEKEJAP_LIBRARY=/path/to/libsekejap.dylib make test
```

## What changed from 0.16

The 0.16 package was a PyO3 extension module over `CoreDB`, and its API is
gone rather than renamed:

* `DB`, `Hit` and `EdgeHit` are gone. A row is addressed by **collection and
  key**, not by one `collection/key` slug string, and a query answers plain
  `dict` rows keyed by column name.
* `DB()` with no argument opened an in-memory database. There is none:
  `sekejap.open_memory()` exists only to refuse, by name, with the reason.
* `db.show("SHOW TABLES")` is refused; `db.collections()` and
  `db.describe(name)` answer the same questions as data.
* `trim_memory` and `memory_report` are refused: the buffer pool is bounded at
  open and there is nothing proportional to rows to give back.
* `FROM MATCH`, `PATH_*`, `SHORTEST` and the rest of the 0.16 graph dialect are
  SQL questions now, answered or refused by name by `sekejap-lang`.

Code written against 0.16 fails at import rather than silently meaning
something else.
