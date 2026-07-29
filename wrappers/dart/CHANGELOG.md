## 0.13.3

* Smaller native library — release builds now strip debug symbols and use LTO
  (link-time optimization). No changes to the Dart/Flutter API.

## 0.13.2

* Maintenance release — coordinated version bump across all sekejap language
  packages. No changes to the Dart/Flutter API since 0.13.1.

## 0.13.1

* **Fix native build in a clean Flutter app.** The published `rust/Cargo.toml` no
  longer inherits `version`/`edition` from a workspace root (which doesn't exist in
  the pub.dev package cache) and depends on the core crate from the registry — so a
  from-source build is self-contained instead of failing with "failed to find a
  workspace root". Native binaries are precompiled in CI and downloaded at build
  time, so a clean `flutter pub add sekejap` needs no Rust toolchain and no manual
  framework wiring.
* Real README with runnable examples for the full API (`initSekejap`, `dbOpen`,
  `dbExecute`, `dbPut`, `dbQuery`, `dbQueryParams`, `dbPrepare`/`dbQueryPrepared`, `dbLink`).
* Example app rewritten as a real CRUD notes app (add / list / delete backed by the DB).
* `initSekejap(libraryPath:)` — load an explicit native library for standalone Dart / `dart test`.
* Documented the testing story (Flutter `integration_test` vs standalone Dart with `libraryPath`).
* Lowered the SDK floor to Dart `>=3.4.0` / Flutter `>=3.22.0` for broader adoption.

## 0.13.0

* Initial public release of the Dart & Flutter bindings for sekejap — an embedded
  graph-first, multi-model database (SQL + graph + vector + spatial) with a native
  Rust core.
* Async API over `flutter_rust_bridge`: `dbOpen`/`dbNew`, `dbExecute`, `dbQuery`,
  `dbQueryParams`, `dbPut`/`dbGet`/`dbContains`, `dbLink`/`dbUnlink`, `dbCompact`/`dbSync`.
* Prepared statements: `dbPrepare` + `dbQueryPrepared` (compile once, run with varying `$1` params).
* Native library precompiled in CI and downloaded at build time via cargokit — no Rust
  toolchain required to use the package.
