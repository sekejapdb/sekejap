/// Dart & Flutter bindings for **sekejap** — an embedded graph-first,
/// multi-model database (SQL + graph + vector + spatial) with a native Rust core.
///
/// Call [initSekejap] once before using the database:
///
/// ```dart
/// await initSekejap();
/// final db = await dbOpen(path: '/tmp/mydb');
/// await dbExecute(db: db, sql: 'CREATE TABLE t (_key TEXT PRIMARY KEY, v INT)');
/// await dbExecute(db: db, sql: "INSERT INTO t (_key, v) VALUES ('a', 42)");
/// final rows = await dbQuery(db: db, sql: 'SELECT * FROM t');
/// ```
///
/// In a Flutter app the native library is bundled and found automatically. For
/// standalone Dart (CLI, `dart test`), pass [initSekejap]'s `libraryPath` to a
/// `libsekejap_ffi.{dylib,so,dll}` you built (`cargo build -p sekejap_ffi`) or
/// downloaded.
library;

import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart'
    show ExternalLibrary;

import 'src/rust/frb_generated.dart';

export 'src/rust/api/simple.dart';
export 'src/rust/frb_generated.dart' show RustLib;

/// Initialise the native library. Safe to call more than once (later calls are
/// no-ops).
///
/// - In a **Flutter app**, omit [libraryPath] — the bundled library is located
///   automatically.
/// - In **standalone Dart / tests**, pass [libraryPath] to a built
///   `libsekejap_ffi.{dylib,so,dll}`.
Future<void> initSekejap({String? libraryPath}) async {
  if (RustLib.instance.initialized) return;
  await RustLib.init(
    externalLibrary:
        libraryPath != null ? ExternalLibrary.open(libraryPath) : null,
  );
}
