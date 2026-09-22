# sekejap for Swift (iOS + macOS)

Swift binding over the sekejap C ABI (`dist/ffi`, `libsekejap`, contract
`docs/dist/C_ABI.md`) via a SwiftPM `systemLibrary` + module map. Re-pointed
for sekejap 0.17.0: the shape is sekejap's own -- a row addressed by
**collection + key** rather than one slug string, `$n` parameters, documents
and rows as JSON, and the C functions this wrapper calls now name
`sekejap_*`, not the 0.13 `node_count`/`edge_count`/`contains` surface.

## Run it

`libsekejap` is never built by this package -- no `cargo` command runs here.
It links a **prebuilt** library, found by:

1. `$SEKEJAP_LIB_DIR`, when set: a directory holding
   `libsekejap.{dylib,a}` (`include/` and `sekejap.pc` may also be there, but
   this package reads its own copy of the header -- see below).
2. Otherwise `<repo root>/target/release`, the layout a normal in-tree
   `cargo build --release -p sekejap-capi` produces.

```bash
SEKEJAP_LIB_DIR=/path/to/libsekejap swift test    # or export it once
swift test                                        # if built at target/release
```

`Package.swift` bakes an rpath to `libDir`, so the built products run with no
further environment variable. The header,
`Sources/CSekejap/sekejap.h`, is a **symlink** into `dist/ffi/include/`, the
committed, cbindgen-generated header `docs/dist/C_ABI.md` is the contract
for -- never a copy vendored into this wrapper, so it cannot drift from the
ABI.

Swift API: `Db(path:)` / `Db.openService(path:)`, `put`/`get`/`exists`/
`delete`/`putMany`, `scan`, `execute`/`query`/`explain`/`prepare`/`stream`,
`link`/`unlink`/`neighbours`, `createCollection`/`dropCollection`/
`collections`/`describe`/`countRows`/`scanCountRows`/`scanCountEdges`,
`transaction()` returning a `Tx` with `put`/`delete`/`link`/`execute`/
`commit`/`rollback`, `checkpoint`/`publish`/`storage`, the service-mode
family (`setStatementTimeout`/`cancel`/`clearInterrupt`/`subscribe`/
`nextChange`/`unsubscribe`) -- every call throws `SekejapError` (a
`SekejapStatus` code plus the message) on failure.

## Approach

Swift has first-class C interop: expose the C ABI to Swift via a **module
map**, then wrap it in Swift classes with throwing methods and native
`[String: Any]` documents (via `JSONSerialization`, cheap and schema-free) --
one class per opaque handle:

| C handle | Swift class |
|---|---|
| `SekejapDb*` | `Db` |
| `SekejapStmt*` | `Statement` |
| `SekejapScan*` (from `sekejap_scan_open` OR `sekejap_query_open` -- the header states these are the same operation under two names) | `Scan` |
| `SekejapTx*` | `Tx` |

Every derived handle (`Statement`, `Scan`, `Tx`) holds a strong reference back
to the `Db` it came from, so Swift's own ARC enforces the ABI's ordering rule
("every derived handle must be freed before `sekejap_close`") rather than a
caller having to remember it by hand. `Db.close()`/`Statement.free()`/
`Scan.close()` are idempotent and are also run by `deinit`; a `Tx` dropped
without `commit()`/`rollback()` rolls back, matching the ABI's own rule for a
transaction handle freed any other way.

Two packaging paths for the native code:

1. **SwiftPM `systemLibrary`** pointing at a prebuilt `libsekejap` +
   `dist/ffi/include/sekejap.h` (dev-friendly; what `swift test` uses here).
2. **SwiftPM `binaryTarget`** shipping a prebuilt **`libsekejap.xcframework`**
   (fat binary for iOS device/simulator + macOS) -- the real distribution
   path, built by the `build-swift-xcframework` job in
   `.github/workflows/release.yml`.

```
wrappers/swift/
├── Package.swift
├── Sources/CSekejap/         # module map + a symlink to dist/ffi/include/sekejap.h
├── Sources/Sekejap/          # Sekejap.swift: the idiomatic wrapper
├── Sources/bench/            # cross-wrapper micro-benchmark
└── Tests/SekejapTests/       # XCTest: open, catalog, put/get, query with a
                               # parameter, scan, prepare + rebind, link +
                               # neighbours, tx commit/rollback, count_rows,
                               # an error path, service mode, close
```

## Distribution

SwiftPM consumes packages from a **Git URL whose root has a `Package.swift`**
-- so, unlike Go's subdirectory module, sekejap's Swift package needs its own
distribution repo. The plan (unchanged from before this repoint):

**To publish via SwiftPM** (`.package(url: …, from: "0.17.0")`), three steps
-- best done together in an Xcode session on real hardware:

1. **Create `sekejapdb/sekejap-swift`** (a small repo whose root is a
   `Package.swift`). This is the URL SwiftPM and the
   [Swift Package Index](https://swiftpackageindex.com) resolve.
2. **Ship the native code as a prebuilt `libsekejap.xcframework`** (iOS device
   + simulator + macOS). `build-swift-xcframework` in the release workflow
   builds it from `dist/ffi` and attaches
   `libsekejap.xcframework.zip` + its checksum to each GitHub release; the
   distribution `Package.swift` references it as a `binaryTarget` (url +
   checksum). *Validate that `import CSekejap` resolves from the xcframework
   on a real Xcode build before tagging.*
3. **Wire the checksum:** the xcframework checksum only exists after the
   release builds it, so the `sekejap-swift` repo's `Package.swift` (url +
   checksum) is updated per release -- either by hand from the release asset,
   or by a CI job that pushes the updated manifest + a matching tag after the
   build.

- **Optional CocoaPods:** `pod 'Sekejap'`, published with `pod trunk push`.
