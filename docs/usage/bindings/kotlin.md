# Kotlin / Java

Install from
[Maven Central](https://central.sonatype.com/artifact/life.sekejap/sekejap) —
the jar bundles the native library per platform, so there is nothing else to
install:

```kotlin
// build.gradle.kts
implementation("life.sekejap:sekejap:0.17.0")
```

Requires **JDK 22+** at runtime (the binding uses the Java FFM API, finalized
in JDK 22; pure JVM, no JNI glue, no JNA dependency). This serves **desktop and
server JVMs**; Android's runtime does not provide the FFM API — on Android, use
the [Flutter binding](dart.md) instead.

## First query

```kotlin
import life.sekejap.SekejapDB

SekejapDB.open("./mydb").use { db ->
    db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT)")
    db.execute("INSERT INTO places (_key, name) VALUES ('a', 'Uluwatu')")
    val rows = db.query("SELECT * FROM places")   // JSON string
    println(rows)
}
```

`SekejapDB` is `AutoCloseable` — `use { }` (Kotlin) or try-with-resources
(Java) closes it cleanly. API: `execute` / `executeParams`,
`query` / `queryParams`, `put` / `get`, `link` / `linkMeta` / `unlink`,
`contains`, `nodeCount` / `edgeCount`, `compact`, `SekejapDB.version()`.

Full build notes: [`wrappers/kotlin/`](../../../wrappers/kotlin/).
