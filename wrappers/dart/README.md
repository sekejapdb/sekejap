# sekejap

Embedded **graph-first, multi-model database** for Dart & Flutter — SQL + graph
traversal + vector + spatial in one small native library. No server, no external
process; the whole engine runs in-process on top of a Rust core.

- **PostgreSQL-like SQL** — `CREATE TABLE`, `INSERT`, `SELECT … WHERE`, parameters (`$1`).
- **Graph** — link records and traverse edges.
- **Prepared statements** — compile once, run many with varying parameters.
- **No Rust toolchain required** — the native library is precompiled per platform
  and downloaded at build time.

## Install

```console
flutter pub add sekejap      # Flutter app
dart pub add sekejap         # standalone Dart
```

## Typed, reactive API (recommended)

Annotate a model, run `build_runner`, and get a typed collection, a fluent
query builder, and reactive queries — no SQL strings. It lowers to the same
engine; it's a typed front-end, not a second engine.

> Ships in the upcoming release. `pub.dev` `sekejap` 0.16.1 currently exposes the
> lower-level SQL/JSON API below; the typed layer and its `sekejap_generator`
> land together in the next version.

Add the codegen tooling (dev-only):

```yaml
# pubspec.yaml
dependencies:
  sekejap: ^0.16.2
dev_dependencies:
  build_runner: ^2.4.0
  sekejap_generator: ^0.1.0
```

```dart
import 'package:sekejap/sekejap.dart';
part 'dish.g.dart';

@SekejapEntity()
class Dish {
  @Key() final String id;
  @Index(IndexKind.hash) final String category;
  @Index() final int price;                // btree
  const Dish({required this.id, required this.category, required this.price});
}
// dart run build_runner build   → generates the typed layer
```

```dart
final db = await Sekejap.open('app.db', schema: [dishSchema]);
await db.dishes.put(const Dish(id: 'd1', category: 'main', price: 45000));

final cheapMains = await db.dishes
    .where((d) => d.category.eq('main') & d.price.lt(90000))
    .sortBy((d) => d.price)
    .find();                                // List<Dish>

// Reactive — a StreamBuilder rebuilds when matching data changes:
db.dishes.where((d) => d.category.eq('main')).watch();   // Stream<List<Dish>>
```

`d.price.lt('cheap')` is a compile error. Multi-model composes in one query:
`.near((d) => d.location, here, metres: 5000).matchText((d) => d.about, 'grilled')
.rankByVector((d) => d.taste, myTaste)`.

The rest of this page covers the lower-level SQL/JSON API the typed layer builds on.

## Quick start

```dart
import 'package:sekejap/sekejap.dart';

Future<void> main() async {
  // 1. Initialise the native library once (Flutter finds it automatically).
  await initSekejap();

  // 2. Open (or create) a database at a directory path.
  final db = await dbOpen(path: '/tmp/notes');

  // 3. Define a table and insert rows with SQL.
  await dbExecute(db: db, sql: '''
    CREATE TABLE note (_key TEXT PRIMARY KEY, title TEXT, pinned INTEGER)
  ''');
  await dbExecute(
    db: db,
    sql: "INSERT INTO note (_key, title, pinned) VALUES ('n1', 'Buy milk', 1)",
  );

  // 4. Insert a record directly (no SQL) as a JSON payload.
  await dbPut(db: db, slug: 'note/n2', json: '{"title":"Call Sam","pinned":0}');

  // 5. Query. Results come back as a JSON array string — decode with jsonDecode.
  final all = await dbQuery(db: db, sql: 'SELECT * FROM note');
  print(all); // [{"slug":"note/n1","payload":{...}}, ...]

  // 6. Parameterised query ($1, $2, …) — injection-safe.
  final pinned = await dbQueryParams(
    db: db,
    sql: 'SELECT title FROM note WHERE pinned = \$1',
    paramsJson: '[1]',
  );
  print(pinned);

  // 7. Prepared statement — compile once, run many.
  final byPin = await dbPrepare(db: db, sql: 'SELECT _key FROM note WHERE pinned = \$1');
  for (final p in [0, 1]) {
    print(await dbQueryPrepared(db: db, stmt: byPin, paramsJson: '[$p]'));
  }

  // 8. Graph: link two records together.
  await dbLink(db: db, from: 'note/n1', to: 'note/n2', edgeType: 'related');

  // 9. Flush + compact to disk when you're done writing a lot.
  await dbCompact(db: db);
}
```

### API at a glance

| Function | Purpose |
|---|---|
| `initSekejap({libraryPath})` | Load the native library (call once) |
| `dbOpen(path:)` / `dbNew()` | Open a persistent DB / an in-memory one |
| `dbExecute(db:, sql:)` | Run DDL/DML (`CREATE`/`INSERT`/`UPDATE`/`DELETE`) → rows affected |
| `dbExecuteParams(db:, sql:, paramsJson:)` | Same, with `$1` parameters |
| `dbQuery(db:, sql:)` | Run a `SELECT`/graph query → JSON array string |
| `dbQueryParams(db:, sql:, paramsJson:)` | Parameterised query |
| `dbPrepare(db:, sql:)` / `dbQueryPrepared(db:, stmt:, paramsJson:)` | Prepared statements |
| `dbPut` / `dbGet` / `dbContains` / `dbRemove` | Direct record access by slug |
| `dbLink` / `dbUnlink` | Create / remove graph edges |
| `dbCompact` / `dbSync` | Flush + compact / flush WAL |

Query results are a **JSON array string** of `{"slug": ..., "payload": {...}}` — decode
with `jsonDecode(...)`.

## Platform support

| Platform | Status |
|---|---|
| Android (arm64, armv7, x86-64) | ✅ |
| iOS | ✅ |
| macOS (Apple Silicon & Intel) | ✅ |
| Linux (x86-64, arm64) | ✅ |
| Windows (x86-64) | ✅ |

The native library (`libsekejap_ffi`) is precompiled in CI and fetched at build
time — **your users do not need a Rust toolchain**. (If a prebuilt binary is
unavailable for a target, the build falls back to compiling from source, which
*does* need Rust.)

## Testing

`flutter test` (plain widget/unit tests) runs on the Dart VM and does **not** bundle
or load the native library, so calls into sekejap will fail there. Two supported ways
to test against the real engine:

1. **Flutter integration test** — bundles and loads the native library like a real
   app. Put tests under `integration_test/` and run on a device/desktop target:
   ```console
   flutter test integration_test -d macos      # or -d linux / a device
   ```
2. **Standalone Dart** — build the library and point `initSekejap` at it:
   ```console
   cargo build --release -p sekejap_ffi
   ```
   ```dart
   await initSekejap(libraryPath: 'target/release/libsekejap_ffi.dylib'); // .so / .dll
   ```

See `example/integration_test/plugin_test.dart` for a working roundtrip + prepared test.

## Requirements

- Dart `>=3.4.0`, Flutter `>=3.22.0`.

## License

Dual-licensed under **MIT OR Apache-2.0** — use whichever you prefer.
