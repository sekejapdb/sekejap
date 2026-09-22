# sekejap for the JVM — Kotlin and Java

One Maven artifact, **`life.sekejap:sekejap-ffm:0.17.0`**, bound with the
**Foreign Function & Memory API** (Panama, `java.lang.foreign`, final in JDK 22)
over the C ABI `libsekejap` — [`dist/ffi/include/sekejap.h`](../../../ffi/include/sekejap.h),
contract [`docs/dist/C_ABI.md`](../../../../docs/dist/C_ABI.md). Pure JVM: no JNI
shim to compile, no JNA, and no runtime dependency beyond the JDK.

The API is sekejap's own, not a port of the 0.16 one: a row is addressed by
**collection and key**, documents and answers are **JSON text**, parameters are
`$1`, `$2`, …, and a construct sekejap has no atomic for is **refused by name**
rather than emulated.

```kotlin
import life.sekejap.*

Db.open("/var/lib/city").use { db ->
    db.createCollection("city", """[{"name":"name","kind":"text"},{"name":"people","kind":"int"}]""")
    db.execute("CREATE INDEX city_people ON city USING btree (people)")

    db.put("city", "melbourne", """{"_key":"melbourne","name":"Melbourne","people":5207145}""")
    db.get("city", "melbourne")                       // {"_key":"melbourne",...}
    db.query("SELECT name FROM city WHERE people > \$1", "[1000000]")

    db.transaction { tx ->                            // many writes, one barrier
        tx.put("city", "geelong", """{"_key":"geelong","name":"Geelong","people":289000}""")
    }
    db.link("city", "melbourne", "near", "city", "geelong")
    db.neighbours("city", "melbourne", "near", Direction.Outgoing)
}
```

The same from Java — every Kotlin default argument carries `@JvmOverloads`, and
every handle is an `AutoCloseable`:

```java
try (Db db = Db.open("/var/lib/city")) {
    db.put("city", "melbourne", "{\"_key\":\"melbourne\",\"name\":\"Melbourne\"}");
    String rows = db.query("SELECT _key FROM city WHERE people > $1", "[1000000]");
} catch (SekejapException e) {
    if (e.getStatus() == Status.Refused) { /* … */ }
}
```

## Run it

`libsekejap` is built from `dist/ffi` and is **not** built by this project:

```bash
cargo build --release -p sekejap-capi          # once, from the workspace root
cd dist/bindings/wrappers/kotlin
./gradlew test                                 # 17 tests
./gradlew run                                  # the end-to-end tour, Example.kt
```

Point the build at a library that lives somewhere else with
`-Psekejap.nativeDir=<directory>` or `SEKEJAP_NATIVE_DIR`; with neither, it looks
in the workspace's own `target/release`. At runtime the library is found through
`-Dsekejap.lib=<file>`, then `java.library.path`, then the natives bundled in the
published jar under `natives/<os>-<arch>/`.

**JDK 22 or newer** is required: FFM was finalized there. The build compiles with
`--release 22`, so the artifact runs on 22 and up whichever JDK builds it. Tests
and `run` pass `--enable-native-access=ALL-UNNAMED`, which is what keeps the
runtime from warning on every downcall.

## What is here

```
dist/bindings/wrappers/kotlin/
├── build.gradle.kts                          # one project, one published artifact
├── src/main/java/life/sekejap/Ffi.java       # the 59 FFM downcalls (Java: invokeExact)
├── src/main/kotlin/life/sekejap/Sekejap.kt   # Db, Statement, Scan, Tx, Status, Direction
├── src/main/kotlin/life/sekejap/Example.kt   # `./gradlew run`
├── src/test/kotlin/life/sekejap/             # 16 Kotlin tests + the Java interop test
└── src/test/java/life/sekejap/JavaTour.java  # the same API, called from Java source
```

`Ffi.java` is Java because `MethodHandle.invokeExact` is signature-polymorphic
and Kotlin cannot spell such a call site. It is package-private; nothing outside
`life.sekejap` sees it.

## The rules this wrapper keeps

| the ABI says | the wrapper does |
|---|---|
| `NULL` / `-1` is a failure | throws `SekejapException` carrying the message from `sekejap_last_error` and the `Status` from `sekejap_last_error_code` |
| `NULL` with `Ok` is a clean MISS | `get`, `describe`, `nextChange` and `Scan.next` answer `null`, and only a non-`Ok` code becomes an exception |
| every returned `char*` is yours | read once and freed with `sekejap_string_free` inside `Ffi.takeString`; `sekejap_version` is static and is never freed |
| the error slot is THREAD-LOCAL and the handle is ignored | `sekejap_last_error(NULL)`, so a failed open with no handle still reports |
| a derived handle is freed BEFORE the database | `Db.close()` frees every `Statement`, `Scan` and `Tx` still open on it, youngest first |
| a `Tx` freed any other way ROLLS BACK | `Tx.close()` rolls back when the transaction is still open, so `use` cannot leave the writer held |
| a refusal keeps its symbol | `compact()`, `trimMemory()`, `show()` and `Db.openMemory()` exist, call the C function, and throw with `Status.Refused` and the library's own reason |
| C `long` is 64-bit on LP64 and 32-bit on LLP64 | the layout for the ten `long` functions is chosen at class-init, so one jar is correct on Windows too |

JSON stays **text**. The JVM has no JSON type in its standard library, so parsing
it here would pick a library for every caller and pay for a tree nobody asked
for; the answer is handed over as the ABI produced it.

## Distribution

- **Registry:** [Maven Central](https://central.sonatype.com) —
  `implementation("life.sekejap:sekejap-ffm:0.17.0")`
- **groupId:** `life.sekejap`, the reverse-DNS of `sekejap.life` and the Kotlin
  package.
- **Publish:** `.github/workflows/release.yml`, job `publish-kotlin`. It stages
  `libsekejap` for five desktop targets from `build-native-libs` into
  `src/main/resources/natives/<os>-<arch>/`, runs the tests against the Linux one,
  then publishes through the Sonatype Central Portal with the
  `com.vanniktech.maven.publish` plugin.

## What e1 had and this does not

e1's Kotlin tree carried four things beside the FFM binding, all of them built on
a **JNI** glue crate, and all four are gone:

| removed | why |
|---|---|
| `rust/` (`sekejap_jni`) | a JNI shim over e1's `CoreDB` (`CoreDB::open`, `AutoCompact`, `SyncMode`). Its path dependency `sekejap = { path = "../../.." }` resolves to `dist/bindings/` in the e4 layout, so it no longer builds at all. Its replacement on the JVM is this FFM binding over the header |
| `orm/` | a KSP ORM (`@SekejapEntity` + `Flow`) whose every call went through `SekejapNative`, the object that crate defined |
| `sekejap-android/` | an AAR wrapping 62 MB of committed `libsekejap_jni.so` binaries for three ABIs, built from that crate against e1 |
| `build/`, `.gradle/`, `.kotlin/` | committed build output — jars, Gradle caches and lock files. `.gitignore` now keeps them out |

**Android is not covered by this artifact.** Android has no `java.lang.foreign`,
so the AAR lane needs a JNI shim written against the 0.17 C ABI plus an NDK
cross-build of `libsekejap`. `.github/workflows/publish-kotlin-mobile.yml` stays
gated until someone writes it.
