// swift-tools-version:5.9
// SwiftPM binding for sekejap over the C ABI (../c, libsekejap).
//
// Build libsekejap first: `cargo build --release -p sekejap-capi`, then
// `swift test` / `swift build` from this directory.
import PackageDescription
import Foundation

// Absolute path to the workspace's release build dir, so the linker finds
// libsekejap and bakes an rpath — `swift test` works with no env vars.
let libDir = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()                       // wrappers/swift
    .appendingPathComponent("../../target/release")     // → <root>/target/release
    .standardizedFileURL.path

let package = Package(
    name: "Sekejap",
    products: [
        .library(name: "Sekejap", targets: ["Sekejap"])
    ],
    targets: [
        // The C ABI, exposed to Swift via a module map (Sources/CSekejap).
        .systemLibrary(name: "CSekejap", path: "Sources/CSekejap"),

        // Idiomatic Swift wrapper.
        .target(
            name: "Sekejap",
            dependencies: ["CSekejap"],
            linkerSettings: [
                .unsafeFlags(["-L\(libDir)", "-Xlinker", "-rpath", "-Xlinker", libDir])
            ]
        ),

        .testTarget(name: "SekejapTests", dependencies: ["Sekejap"]),

        // Micro-benchmark executable: `swift run -c release bench`.
        .executableTarget(
            name: "bench",
            dependencies: ["Sekejap"],
            linkerSettings: [
                .unsafeFlags(["-L\(libDir)", "-Xlinker", "-rpath", "-Xlinker", libDir])
            ]
        ),

        // For distribution, replace CSekejap with a prebuilt xcframework:
        // .binaryTarget(name: "CSekejap",
        //     url: "https://github.com/sekejapdb/sekejap/releases/download/v0.13.0/libsekejap.xcframework.zip",
        //     checksum: "…"),
    ]
)
