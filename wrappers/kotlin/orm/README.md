# sekejap for Kotlin — typed, reactive, JNI-backed

The **mobile-first** Kotlin binding: an embedded graph-first multi-model database
(SQL + graph + vector + spatial) with a typed, reactive API. Runs on **Android**
and on **JVM backend/desktop** over one JNI core.

Three layers, pick your level:

| layer | what | when |
|---|---|---|
| **Ergonomic** (this module) | `@SekejapEntity` + KSP codegen + typed builder + `Flow` | app code |
| **Raw** (`SekejapNative`) | `external fun` + string SGQL + JSON | hot paths, tooling |
| **Core** (`../rust`, `../c`) | the Rust engine / C ABI | other bindings |

## Install

**Android app** — two dependencies, that's all (the AAR bundles the native `.so`):

```kotlin
// build.gradle.kts
plugins {
  id("com.google.devtools.ksp")
}
dependencies {
  implementation("life.sekejap:sekejap-android:0.16.4")   // typed API + native library
  ksp("life.sekejap:sekejap-processor:0.16.2")            // @SekejapEntity codegen
}
```

**Desktop / server JVM** — `implementation("life.sekejap:sekejap:0.16.2")` (typed API)
+ the same `ksp(...)` line; provide the host native library via
`-Dsekejap.jni.path=…` or the Panama binding `life.sekejap:sekejap-ffm`.

> Only `sekejap-android` + `sekejap-processor` are needed for an app. The runtime
> `life.sekejap:sekejap` is pulled in transitively. (The older `sekejap-orm` /
> `sekejap-orm-processor` coordinates are deprecated — use the names above.)

## Typed, reactive usage

```kotlin
@SekejapEntity
data class Dish(
    @Key val id: String,
    @Index(IndexKind.HASH) val category: String,
    @Index val price: Int,
)
// KSP generates DishColumns, DishCollection, db.dishes, dishSchema

val db = Sekejap.open(dir, schema = listOf(dishSchema), mobileProfile = true)
db.dishes.put(Dish("d1", "main", 45000))

val cheapMains = db.dishes
    .where { it.category eq "main" and (it.price lt 90000) }
    .sortBy { it.price }.find()                       // List<Dish>

// Reactive (Compose-ready): re-emits when matching data changes.
db.dishes.where { it.category eq "main" }.watch()     // Flow<List<Dish>>
```

Multi-model in one query: `.near { it.location }`, `.matchText { it.body }`,
`.rankByVector { it.embedding }` compose with `.where { … }` — one SGQL statement.

## The stable JNI surface (`life.sekejap.SekejapNative`)

`open`, `mobileProfile`, `execute`, `executeParams`, `queryParams`, `get`,
`putMany`, `compact`, `close`, and the change feed `watchOpen`/`watchNext`/
`watchClose`/`watchFree`. The typed layer lowers to exactly these — no hidden ops.

## Building the native library

```bash
# host (for `gradle test` on this machine)
./build-native.sh host
# Android ABIs (arm64-v8a, armeabi-v7a, x86_64) → jniLibs/
ANDROID_NDK_HOME=$ANDROID_HOME/ndk/<version> ./build-native.sh android
```

## Consuming it

- **Android app**: just the two dependencies in [Install](#install) — the
  `sekejap-android` AAR already bundles `libsekejap_jni.so` for every ABI, loaded
  via `System.loadLibrary("sekejap_jni")`. No manual `jniLibs`, no Rust toolchain.
- **JVM backend / tests**: add `life.sekejap:sekejap`, and provide the host native
  library with `-Dsekejap.jni.path=/abs/libsekejap_jni.{so,dylib,dll}`.
- Building the `.so` yourself (extra ABIs, source builds) is only needed for
  contributors — see [Building the native library](#building-the-native-library).

## Test

```bash
./build-native.sh host
gradle test        # host-JVM tests load the built lib via -Dsekejap.jni.path
```

## Status

Typed CRUD, multi-model queries, typed `update`/`deleteAll`, and reactive `Flow`
`.watch()` are implemented and tested (host JVM) and benchmarked on-device (see
`eval/mobile-kotlin`). Remaining release-engineering: an Android **AAR** that
bundles the ABIs' `.so` (so consumers don't manage jniLibs), and Maven Central
publishing — mirrors the FFM desktop binding's publish setup in `../build.gradle.kts`.
