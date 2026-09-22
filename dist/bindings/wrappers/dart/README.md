# sekejap for Dart

Embedded **graph-first, multi-model database** — SQL + graph traversal + vector
+ spatial in one native library, running in your process. No server, no second
process.

The binding is **`dart:ffi` over the sekejap C ABI**
([`docs/dist/C_ABI.md`](../../../../docs/dist/C_ABI.md), header
[`dist/ffi/include/sekejap.h`](../../../ffi/include/sekejap.h)): one Dart class
per C handle and one method per C function. Pure Dart — it works the same in a
Dart CLI, in a server and in a Flutter app.

- **PostgreSQL-like SQL** — `CREATE TABLE`, `CREATE INDEX`, `INSERT`,
  `SELECT … WHERE`, `$1` parameters.
- **Documents** — a row is addressed by collection and key; a document is a
  Dart `Map<String, Object?>` and comes back as one, with `_key` on it.
- **Graph** — typed edges with properties, and the rows one hop away.
- **Prepared statements** — parse once, rebind many.
- **Transactions** — many writes under one durability barrier.
- **Service mode** — one writer, parallel readers, a change feed, a statement
  timeout and a cancel.

## Install

```console
dart pub add sekejap        # Dart CLI or server
flutter pub add sekejap     # Flutter app
```

The package builds no native code. It loads a `libsekejap` that is already on
the machine.

## Getting `libsekejap`

`libsekejap` is the `sekejap-capi` crate (`dist/ffi`). Three ways to have one:

| where from | how |
|---|---|
| a release | `libsekejap-<platform>.tar.gz` is attached to every GitHub release (`lib/` + `include/`) |
| from source | `cargo build --release -p sekejap-capi` in the sekejap repository |
| a package manager | `cd dist/ffi && make install PREFIX=~/.local` puts the library and `sekejap.pc` where `pkg-config` finds them |

## How the wrapper finds it

In this order; the first that loads wins.

1. **`useSekejapLibrary(path)`** — called before the first database call.
2. **`SEKEJAP_LIBRARY`** — the full path to the file. What the tests use.
3. **The running process** — when it already carries `sekejap_version`: a
   Flutter app whose plugin bundled the library, or a host that linked
   `libsekejap.a`.
4. **The platform's plain library name** — `libsekejap.dylib`,
   `libsekejap.so`, `sekejap.dll`. The dynamic loader resolves it against
   `DYLD_LIBRARY_PATH` / `LD_LIBRARY_PATH`, the macOS app bundle, the Android
   `jniLibs` directory, or the directory of the executable.

Nothing found is a `SekejapLibraryNotFound` naming every candidate it tried.

**In Flutter**, put the platform's `libsekejap` where the platform already
looks — `android/app/src/main/jniLibs/<abi>/libsekejap.so`, the macOS/iOS
app's `Frameworks`, beside the Windows or Linux executable — and step 4 finds
it with no configuration. Nothing in this package downloads or compiles a
native library at build time.

## Use

```dart
import 'package:sekejap/sekejap.dart';

void main() {
  final db = Db.open('/tmp/warung');

  db.createCollection('dish', const [
    FieldSpec('name', FieldKind.text),
    FieldSpec('price', FieldKind.integer),
  ]);
  // A Tier-1 predicate is answered index-side, so a field a WHERE names needs
  // an index; without one the query is REFUSED rather than quietly scanned.
  db.execute('CREATE INDEX dish_price ON dish USING btree(price)');

  db.put('dish', 'd1',
      {'_key': 'd1', 'name': 'Nasi Goreng', 'price': 45000});
  db.putMany('dish', {
    'd2': {'name': 'Gado Gado', 'price': 38000},
    'd3': {'name': 'Sate Ayam', 'price': 52000},
  });

  print(db.get('dish', 'd1'));                // {_key: d1, name: …, price: …}
  print(db.query(r'SELECT name FROM dish WHERE price < $1', [40000]));

  db.close();
}
```

`example/sekejap_example.dart` is the same tour over every call.

### A walk, one page at a time

```dart
final walk = db.scan('dish', pageRows: 256);
for (final row in walk.rows()) {
  print(row['_key']);
}
walk.close();
```

`db.stream(sql, params, pageRows)` is the same shape over a statement's
answer. What paging buys is a bounded string per call and the freedom to stop
reading; the answer itself is assembled when the walk opens.

### Prepared statements

```dart
final byCeiling = db.prepare(r'SELECT name FROM dish WHERE price < $1');
print(byCeiling.rebindable);            // Rebindable.unbound, before any bind
print(byCeiling.query([20000]));
print(byCeiling.rebindable);            // Rebindable.yes — a rebind compiles nothing
byCeiling.close();
```

A WRITING statement answers `Rebindable.no`, because a write folds its
document at compile. The call says so rather than pretending otherwise.

### Graph

```dart
db.link('place', 'warung', 'serves', 'dish', 'd1', properties: {'since': 1998});
final served = db.neighbours('place', 'warung', edgeType: 'serves');
final back = db.neighbours('dish', 'd1',
    edgeType: 'serves', direction: SekejapDirection.incoming);
```

Both endpoints must exist: a missing one is `SekejapStatus.unknownRow`, never
a dangling identity. A neighbour answer is complete or refused under a bound
of 256 edges; a walk deeper than one hop is SQL's `GRAPH_TABLE`.

### Transactions

```dart
final tx = db.transaction();
try {
  tx.put('dish', 'd4', {'name': 'Soto Ayam', 'price': 41000});
  tx.execute(r'DELETE FROM dish WHERE _key = $1', ['d3']);
  tx.commit();
} catch (_) {
  tx.rollbackIfOpen();
  rethrow;
}
```

Every call outside a transaction commits before it returns: durability per
call. A transaction is the other bargain — many writes, one barrier — and the
two are the whole story. `commit` and `rollback` both free the handle;
`db.close()` rolls back a transaction that is still open, because a close is
not a commit.

### Errors

Both channels of the ABI reach Dart. A failure is a `SekejapException`
carrying the closed `SekejapStatus` and the sentence the engine wrote:

```dart
try {
  db.query('SELECT FROM WHERE');
} on SekejapException catch (e) {
  print(e.status);     // SekejapStatus.invalid
  print(e.message);    // the message sekejap_last_error wrote on this thread
}
```

A **miss is not a failure**: `db.get` answers `null` for no such row,
`db.describe` answers `null` for no such collection, and `Scan.nextPage`
answers `null` at the end of the walk.

A **refusal is never an empty answer**. `Db.openMemory`, `db.trimMemory`,
`db.compact` and `db.show` keep their names and always throw
`SekejapStatus.refused` with the reason — sekejap has no atomic underneath any
of them, and an emulation would be a fake.

### Service mode

```dart
final service = Db.openService('/tmp/warung');
final subscription = service.subscribe();
service.put('dish', 'd9', {'name': 'Rendang', 'price': 65000});
final event = service.nextChange(subscription, timeoutMs: 2000);
service.statementTimeoutMs(5000);
service.unsubscribe(subscription);
```

Every call in this family is refused by name on a handle that was not opened
with `Db.openService`: single mode has no writer to time out, no interrupt and
no change feed.

## Threading

`sekejap::Db` is `Send + Sync`, so one `Db` MAY be used from several isolates
and each reads its own error slot back. The derived handles — `Scan`,
`Statement`, `Transaction` — are each used from **one isolate at a time**, and
all of them are freed before the `Db` they borrow (`db.close()` does that for
any that are still open).

Every call is **synchronous**: a `dart:ffi` call blocks its isolate. Put long
scans and large statements on their own isolate rather than on the one serving
a UI.

## API mapping

One Dart call per C function, all 59 of them. `docs/dist/C_ABI.md` is the
contract; `lib/src/bindings.dart` is the only file that names a C symbol.

| C | Dart |
|---|---|
| `sekejap_open` | `Db.open(path)` |
| `sekejap_open_with_config` | `Db.openWithConfig(path, StoreConfig(…))` |
| `sekejap_open_service` | `Db.openService(path)` |
| `sekejap_close` | `db.close()` |
| `sekejap_version` | `Db.version` |
| `sekejap_format_version` | `Db.formatVersion` |
| `sekejap_last_error` | `Db.lastError` |
| `sekejap_last_error_code` | `Db.lastErrorCode` |
| `sekejap_string_free` | inside the wrapper, on every returned string |
| `sekejap_put` | `db.put(collection, key, document)` |
| `sekejap_put_many` | `db.putMany(collection, rows)` |
| `sekejap_get` | `db.get(collection, key)` |
| `sekejap_exists` | `db.exists(collection, key)` |
| `sekejap_delete` | `db.delete(collection, key)` |
| `sekejap_scan_open` / `_next` / `_close` | `db.scan(…)` → `Scan.nextPage()` / `pages()` / `rows()` / `close()` |
| `sekejap_execute` | `db.execute(sql, params)` |
| `sekejap_query` | `db.query(sql, params)` |
| `sekejap_explain` | `db.explain(sql, params)` |
| `sekejap_prepare` | `db.prepare(sql)` |
| `sekejap_stmt_query` | `statement.query(params)` |
| `sekejap_stmt_execute` | `statement.execute(params)` |
| `sekejap_stmt_rebindable` | `statement.rebindable` |
| `sekejap_stmt_free` | `statement.close()` |
| `sekejap_query_open` / `_next` / `_close` | `db.stream(…)` → the same `Scan` |
| `sekejap_link` / `sekejap_link_with` | `db.link(…, properties: …)` |
| `sekejap_unlink` | `db.unlink(…)` |
| `sekejap_neighbours` | `db.neighbours(…)` → `List<Neighbour>` |
| `sekejap_create_collection` | `db.createCollection(name, fields)` |
| `sekejap_drop_collection` | `db.dropCollection(name)` |
| `sekejap_collections` | `db.collections()` |
| `sekejap_describe` | `db.describe(name)` → `CollectionDescription?` |
| `sekejap_count_rows` | `db.countRows(name)` |
| `sekejap_scan_count_rows` | `db.scanCountRows(name)` |
| `sekejap_scan_count_edges` | `db.scanCountEdges()` |
| `sekejap_tx_begin` | `db.transaction()` |
| `sekejap_tx_put` / `_delete` / `_link` / `_execute` | `tx.put` / `delete` / `link` / `execute` |
| `sekejap_tx_commit` / `_rollback` | `tx.commit()` / `tx.rollback()` |
| `sekejap_checkpoint` | `db.checkpoint()` — false is DEFERRED, not failed |
| `sekejap_publish` | `db.publish()` |
| `sekejap_storage` | `db.storage()` → `StorageBytes` |
| `sekejap_statement_timeout_ms` | `db.statementTimeoutMs(ms)` |
| `sekejap_cancel` | `db.cancel()` |
| `sekejap_clear_interrupt` | `db.clearInterrupt()` |
| `sekejap_subscribe` | `db.subscribe()` |
| `sekejap_next_change` | `db.nextChange(id, timeoutMs: …)` → `ChangeEvent?` |
| `sekejap_unsubscribe` | `db.unsubscribe(id)` |
| `sekejap_open_memory` | `Db.openMemory()` — always refuses |
| `sekejap_trim_memory` | `db.trimMemory()` — always refuses |
| `sekejap_compact` | `db.compact()` — always refuses |
| `sekejap_show` | `db.show(statement)` — always refuses |

## Gaps

`ChangeEvent.collectionIds`, `ChangeEvent.edgeTypeIds` and
`ChangedKey.collectionId` are NUMERIC ids, because that is what
`sekejap_next_change` emits. The C ABI has no call that turns one into a name
(`sekejap_collections` answers names with no ids beside them), so a listener
that wants to act on a name cannot get one from the feed alone.

## Tests

```console
tool/test.sh /path/to/libsekejap.dylib
```

which is `dart pub get`, `dart analyze` and `dart test` with
`SEKEJAP_LIBRARY` set. Every test opens a real database in a temporary
directory and compares against an oracle computed in the test process.

## Licence

MIT OR Apache-2.0, as the rest of sekejap.
