# Dart / Flutter

Install from [pub.dev](https://pub.dev/packages/sekejap) — the native library
is precompiled per platform and downloaded at build time:

```bash
flutter pub add sekejap      # Flutter app
dart pub add sekejap         # standalone Dart
```

## First query

```dart
import 'package:sekejap/sekejap.dart';

Future<void> main() async {
  await initSekejap();                    // once; Flutter finds the lib automatically

  final db = await dbOpen(path: './mydb');
  await dbExecute(db: db,
      sql: 'CREATE TABLE note (_key TEXT PRIMARY KEY, title TEXT)');
  await dbExecute(db: db,
      sql: "INSERT INTO note (_key, title) VALUES ('n1', 'Buy milk')");

  final rows = await dbQuery(db: db, sql: 'SELECT * FROM note');
  print(rows);
}
```

All calls are async (the engine runs off the UI thread). Parameters use `$1`
placeholders via `dbQueryParams`; prepared statements are available for
repeated queries. On device, the database is a directory in your app's
documents folder — no server process, works offline.

Full API: [`wrappers/dart/`](../../../wrappers/dart/).
