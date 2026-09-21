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

SwiftPM consumes packages from a **Git URL whose root has a `Package.swift`** —
so, unlike Go's subdirectory module, sekejap's Swift package needs its own
distribution repo. The plan (and remaining steps):

**Status today:** the binding builds and `swift test` passes against a locally
built `libsekejap` (the dev manifest in this folder computes the path to
`target/release`). Use it now as a local dependency:
`.package(path: "…/sekejap/wrappers/swift")`.

**To publish via SwiftPM** (`.package(url: …, from: "0.16.2")`), three steps —
best done together in an Xcode session on real hardware:

1. **Create `sekejapdb/sekejap-swift`** (a small repo whose root is a
   `Package.swift`). This is the URL SwiftPM and the
   [Swift Package Index](https://swiftpackageindex.com) resolve.
2. **Ship the native code as a prebuilt `libsekejap.xcframework`** (iOS device +
   simulator + macOS). The Release workflow already builds and attaches
   `libsekejap.xcframework.zip` + its checksum to each GitHub release; the
   distribution `Package.swift` references it as a `binaryTarget` (url + checksum).
   *Validate that `import CSekejap` resolves from the xcframework on a real Xcode
   build before tagging.*
3. **Wire the checksum:** the xcframework checksum only exists after the release
   builds it, so the `sekejap-swift` repo's `Package.swift` (url + checksum) is
   updated per release — either by hand from the release asset, or by a CI job
   that pushes the updated manifest + a matching tag after the build.

- **Optional CocoaPods:** `pod 'Sekejap'`, published with `pod trunk push`.
