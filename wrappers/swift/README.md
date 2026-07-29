# sekejap for Swift (iOS + macOS)

**Status: working** (tested). Swift binding over the C ABI (`../c`, `libsekejap`)
via a SwiftPM `systemLibrary` + module map.

## Run it

```bash
cargo build --release -p sekejap-capi     # build libsekejap (once)
cd wrappers/swift
swift test                                # → OK — sekejap 0.13.0
```

`Package.swift` computes the absolute path to `target/release` and bakes an rpath,
so `swift test`/`swift build` link and run with no env vars. The C header is
symlinked into `Sources/CSekejap/` so the module map stays in sync. Swift API:
`SekejapDB(path:)`, `execute`, `query`/`query(_:params:)`, `put`, `get`, `link`,
`contains`, `nodeCount`/`edgeCount`, `compact`, `sekejapVersion()` — throwing where
the C ABI can fail.

## Approach

Swift has first-class C interop: expose the C ABI to Swift via a **module map**,
then wrap it in a Swift-friendly class (`SekejapDB` with throwing methods, `String`
results). Two packaging paths:

1. **SwiftPM `systemLibrary`** pointing at the installed `libsekejap` + `sekejap.h`
   (dev-friendly on macOS after `make install`).
2. **SwiftPM `binaryTarget`** shipping a prebuilt **`libsekejap.xcframework`** (fat
   binary for iOS device/simulator + macOS) — the real distribution path.

Alternatively, [uniffi](https://mozilla.github.io/uniffi-rs/) can generate the Swift
wrapper from Rust (and Kotlin at the same time).

```
wrappers/swift/
├── Package.swift
├── Sources/CSekejap/         # module map exposing sekejap.h
├── Sources/Sekejap/          # Sekejap.swift: idiomatic wrapper
└── Tests/SekejapTests/       # XCTest
```

## Distribution

- **No central registry.** Swift Package Manager consumes packages from **Git URLs**.
- **Install (SwiftPM):**
  ```swift
  .package(url: "https://github.com/sekejapdb/sekejap-swift.git", from: "0.13.0")
  ```
- **Publish:** git-tag a release; the [Swift Package Index](https://swiftpackageindex.com)
  indexes it automatically. Ship the native code as a `libsekejap.xcframework` attached
  to the GitHub Release and referenced by a `binaryTarget`.
- **Optional CocoaPods:** `pod 'Sekejap'`, published with `pod trunk push`.
