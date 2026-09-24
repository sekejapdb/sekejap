// swift-tools-version:5.9
// SwiftPM binding for sekejap 0.17.0 over the C ABI
// (dist/ffi, crate sekejap-capi, lib name `sekejap` -> libsekejap.{dylib,a}).
//
// This manifest never runs cargo: libsekejap comes from a prebuilt libsekejap
// directory (or, for a normal checkout, from `cargo build --release -p
// sekejap-capi`) and linked as a prebuilt library. The directory that holds
// it is resolved as:
//   1. $SEKEJAP_LIB_DIR, when set -- a flat directory of
//      libsekejap.{dylib,a} + include/sekejap.h + sekejap.pc, exactly what
//      `swift build`/`swift test` in this checkout point it at.
//   2. Otherwise <repo root>/target/release, the layout an in-tree
//      `cargo build --release -p sekejap-capi` produces.
//
// The header (Sources/CSekejap/sekejap.h) is a symlink straight into
// dist/ffi/include/, the committed, cbindgen-generated header that
// docs/dist/C_ABI.md is the contract for -- never a vendored copy that can
// drift from it.
import PackageDescription
import Foundation

let libDir: String = {
    if let override = ProcessInfo.processInfo.environment["SEKEJAP_LIB_DIR"] {
        return override
    }
    return URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()                            // .../dist/bindings/wrappers/swift
        .appendingPathComponent("../../../../target/release")   // -> <repo root>/target/release
        .standardizedFileURL.path
}()

let sekejapLinkerSettings: [LinkerSetting] = [
    .unsafeFlags(["-L\(libDir)", "-Xlinker", "-rpath", "-Xlinker", libDir])
]

let package = Package(
    name: "Sekejap",
    products: [
        .library(name: "Sekejap", targets: ["Sekejap"])
    ],
    targets: [
        // The C ABI, exposed to Swift via a module map over dist/ffi/include/sekejap.h.
        .systemLibrary(name: "CSekejap", path: "Sources/CSekejap"),

        // Idiomatic Swift wrapper: one class per handle (Db, Statement, Scan, Tx),
        // mirroring the 59 sekejap_* functions one to one (docs/dist/C_ABI.md).
        .target(
            name: "Sekejap",
            dependencies: ["CSekejap"],
            linkerSettings: sekejapLinkerSettings
        ),

        // Inherits the library target's linker settings transitively.
        .testTarget(name: "SekejapTests", dependencies: ["Sekejap"]),

        // Cross-wrapper micro-benchmark: `swift run -c release bench`.
        .executableTarget(
            name: "bench",
            dependencies: ["Sekejap"],
            linkerSettings: sekejapLinkerSettings
        ),

        // For distribution (build-swift-xcframework in .github/workflows/release.yml):
        // replace CSekejap with a prebuilt libsekejap.xcframework binaryTarget
        // (url + checksum), shipping the same dist/ffi/include/sekejap.h inside it.
    ]
)
