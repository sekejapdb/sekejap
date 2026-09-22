# Changelog

## 0.17.0

The wrapper is now **`dart:ffi` over the sekejap C ABI**. It is a rewrite, not
an upgrade: every call has a new spelling.

### Changed

- **The binding.** `flutter_rust_bridge` over a Rust glue crate is gone; the
  package binds `libsekejap` (`dist/ffi`, contract `docs/dist/C_ABI.md`)
  directly with `dart:ffi`. All 59 C functions are exposed, one Dart call each.
- **Pure Dart.** The package no longer depends on Flutter and is no longer a
  Flutter plugin. It works unchanged in a Dart CLI, a server and a Flutter app.
  Its one dependency is `package:ffi`.
- **A row is a collection and a key**, not one slug string: `db.put('dish',
  'd1', {…})`, `db.get('dish', 'd1')`.
- **Handles are classes.** `Db`, `Statement`, `Scan`, `Transaction`, each freed
  by its own `close()` and all of them before the `Db` they borrow.
- **Errors are exceptions.** A failure throws `SekejapException` carrying both
  channels of the ABI: the closed `SekejapStatus` and the message
  `sekejap_last_error` wrote on this thread. A miss stays `null`.

### Removed

- `flutter_rust_bridge` and the generated `lib/src/rust/` bindings, and the
  `rust/` glue crate (`sekejap_ffi`) they were generated from. The C ABI is
  the binding now, and the header is generated from `dist/ffi/src/lib.rs`.
- **cargokit** and the `android/`, `ios/`, `linux/`, `macos/`, `windows/`
  plugin folders. They existed to build the glue crate from source on a
  consumer's machine; there is no glue crate to build. Ship the platform's
  `libsekejap` with the app instead — see the README.
- `dbNew` (an in-memory database). sekejap is disk-first and has no in-memory
  store; `Db.openMemory()` keeps the name and refuses with the reason.
- **The typed model layer** — `@SekejapEntity`, `Collection`, `Query`,
  `Filter`, `Sekejap.open(schema:)` — and the `sekejap_generator` package that
  generated it. It was a second front end over the old Rust glue's API and does
  not describe the 0.17 surface. The wrapper mirrors the C functions one to
  one; a typed layer over these calls is separate work.
- `dbSetWalSync` and the mobile profile call. Store configuration is set at
  open, once: `Db.openWithConfig(path, StoreConfig(sync: SyncMode.normal))`.
- `watchChanges`. The change feed has a service behind it now:
  `Db.openService`, `subscribe`, `nextChange`, `unsubscribe`.

### Added

- Service mode: `Db.openService`, `subscribe`/`nextChange`/`unsubscribe`,
  `statementTimeoutMs`, `cancel`, `clearInterrupt`.
- Transactions: `db.transaction()` with `put`, `delete`, `link`, `execute`,
  `commit`, `rollback`.
- Paged walks: `db.scan(collection)` and `db.stream(sql, params, pageRows)`.
- The catalog: `createCollection`, `dropCollection`, `collections`,
  `describe`, `countRows`, `scanCountRows`, `scanCountEdges`.
- `checkpoint`, `publish`, `storage`, `explain`.
