# sekejap for Kotlin/Java (desktop/server JVM)

**Status: working** (tested). Kotlin/JVM binding via **Panama / Foreign Function &
Memory API** (FFM, JDK 22+) over the C ABI (`../c`, `libsekejap`) — pure JVM, no
native glue and no JNA dependency. `SekejapDB : AutoCloseable` with idiomatic methods.

## Run it

```bash
cargo build --release -p sekejap-capi     # build libsekejap (once)
cd wrappers/kotlin
gradle test                               # → BUILD SUCCESSFUL
gradle run                                # micro-benchmark (Bench.kt)
```

Test/run JVM args (`--enable-native-access=ALL-UNNAMED` + the lib dir on
`java.library.path`) are set in `build.gradle.kts`, so it loads `libsekejap` with no
install. `Ffi.java` locates the library via `-Dsekejap.lib=<path>` or
`java.library.path`.

> **JDK note:** requires **JDK 22+** at runtime (FFM was finalized in 22). Also,
> Kotlin 2.0.20 doesn't support JDK 26 (the compiler crashes), so build on JDK 22:
> `JAVA_HOME=$(brew --prefix openjdk@22 2>/dev/null || echo /path/to/jdk22) gradle test`.

API: `execute`/`executeParams`, `query`/`queryParams`, `put`, `get`, `link`/`linkMeta`/
`unlink`, `contains`, `nodeCount`/`edgeCount`, `compact`, `SekejapDB.version()`.

## Why Panama (not JNA or JNI)

We benchmarked all three JVM approaches (see [`../BENCH.md`](../BENCH.md)):
**JNI ≈ 1.15× native, Panama ≈ 1.44×, JNA ≈ 2.10×.** JNI is fastest but needs a C
shim compiled and shipped **per platform**; JNA is slowest (reflection) and adds a
~1.5 MB jar. **Panama is pure JVM** — no shim, no extra dependency, and ~1.5× faster
than JNA — so it's the cleanest modern binding. (Trade-off: JDK 22+ only. If you must
support older runtimes, JNA is the fallback, at a larger footprint.)

```
wrappers/kotlin/
├── build.gradle.kts
├── src/main/java/life/sekejap/Ffi.java   # FFM downcalls (Java: Kotlin lacks invokeExact)
├── src/main/kotlin/life/sekejap/Sekejap.kt  # SekejapDB — idiomatic Kotlin wrapper
├── src/main/kotlin/Bench.kt              # micro-benchmark
└── src/test/kotlin/…                     # JUnit test
```

For distribution you'd bundle `libsekejap` per platform (jar resources → extract to
temp → `-Dsekejap.lib`), or rely on a system-installed library.

## Distribution

- **Registry:** [Maven Central](https://central.sonatype.com) → Gradle
  `implementation("life.sekejap:sekejap:0.16.5")`
- **groupId:** **`life.sekejap`** (matches `group` in the Gradle build files).
- **Publish:** via the Sonatype **Central Portal**, using the vanniktech
  `com.vanniktech.maven.publish` Gradle plugin (handles signing + Central Portal upload).
