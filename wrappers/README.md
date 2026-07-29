# Language wrappers

Each subfolder is a binding to the sekejap engine for one language, with its own
`examples/`. Most native bindings sit on the **C ABI** (`wrappers/c`, `libsekejap`);
Python and Node use language-native FFI frameworks for better ergonomics.

## The wrappers

| Folder | Binds via | Status |
|---|---|---|
| [`python`](python/) | PyO3 + maturin (native CPython extension) | working |
| [`dart`](dart/) | flutter_rust_bridge / `dart:ffi` (Dart + Flutter) | working (Rust side) |
| [`c`](c/) | the C ABI itself (`extern "C"`, `sekejap.h`) — also serves C++ | working |
| [`node`](node/) | napi-rs (native Node addon) | scaffold |
| [`kotlin`](kotlin/) | Panama/FFM over the C ABI (JDK 22+, JVM + Android) | working |
| [`go`](go/) | cgo over the C ABI | scaffold |
| [`swift`](swift/) | SwiftPM over the C ABI (module map / xcframework) | scaffold |

Naming note: the Dart wrapper is called **`dart`**, not `flutter` — it publishes to
**pub.dev** (Dart's registry) and a `dart:ffi` package works in *both* standalone
Dart and Flutter. Calling it "flutter" would wrongly imply Flutter-only.

## Distribution channels — where each one is published

The name to claim everywhere is **`sekejap`**. Reserve it early on each registry.

| Wrapper | Registry | Install | How you publish |
|---|---|---|---|
| **Python** | [PyPI](https://pypi.org) | `pip install sekejap` | `maturin publish` (or the `maturin-action` GitHub workflow → wheels per platform) |
| **Dart/Flutter** | [pub.dev](https://pub.dev) | `dart pub add sekejap` · `flutter pub add sekejap` | `dart pub publish`. Published under the *verified publisher* **`zebflow.com`** (the umbrella domain, shared by all products) |
| **Node.js** | [npm](https://npmjs.com) | `npm install sekejap` | `npm publish`. napi-rs builds prebuilt binaries per platform in CI; ship them as `optionalDependencies` platform packages |
| **Kotlin/Java** | [Maven Central](https://central.sonatype.com) | Gradle `implementation("com.zebflow:sekejap:0.13.0")` | Publish via the Sonatype **Central Portal**. groupId **`com.zebflow`** (reverse-DNS of the `zebflow.com` umbrella; verifying the apex grants `com.zebflow.*` for every product). Verify the domain once |
| **Go** | *no registry* — Go modules via Git | `go get github.com/sekejapdb/sekejap/wrappers/go` | Just **git-tag** a release (`v0.13.0`); [pkg.go.dev](https://pkg.go.dev) indexes it automatically. Module path must match the repo URL |
| **C/C++** | *no single registry* | see below | Options: **vcpkg**, **Conan**, a **Homebrew tap** (`brew install <you>/tap/sekejap`), or prebuilt `.so`/`.a` + header + `sekejap.pc` on **GitHub Releases** |
| **Swift** | *no central registry* — SwiftPM via Git | `.package(url: "…/sekejap-swift.git", from: "0.13.0")` | **git-tag** a release; the [Swift Package Index](https://swiftpackageindex.com) indexes it. Optionally also **CocoaPods** (`pod 'Sekejap'`, `pod trunk push`). Ship the native lib as an `.xcframework` (binaryTarget) on Releases |

### Cross-cutting notes

- **Reserve the name now:** create empty/placeholder entries (or at least the
  accounts) on **PyPI, npm, pub.dev, crates.io** and pick the **Maven groupId** from a
  domain you own — before someone else takes `sekejap`.
- **Prebuilt native binaries** are the real distribution work: Python (wheels),
  Node (napi prebuilds), Swift/Kotlin (xcframework / bundled `.so`), C/C++ (release
  artifacts). A single **GitHub Actions release workflow** cross-compiling
  `libsekejap` for macOS (x86_64/arm64), Linux (x86_64, aarch64, armv7 for Raspberry
  Pi), Windows, iOS, and Android would feed all of them.
- **Registryless ≠ unpublished:** Go, Swift, and C/C++ have no upload step — you
  "publish" by tagging a Git release and (for C/C++) attaching prebuilt artifacts.
