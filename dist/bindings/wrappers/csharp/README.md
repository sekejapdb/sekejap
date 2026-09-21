# sekejap for C# / .NET (and Unity)

C# binding for the [sekejap](https://sekejap.life) embedded database — a
graph-first, multi-model engine (SQL + graph + spatial + vector + full-text) —
via P/Invoke over the stable C ABI.

Targets **.NET** backends and **Unity 2021+** (netstandard2.1). Idiomatic surface:
`SekejapDb : IDisposable`, exceptions (`SekejapException`), `string?` for a missing
node, `using` for deterministic cleanup. Results are JSON strings.

> **Status:** the binding is written and compiles as a standard netstandard2.1
> library; it has **not yet been built/run here** (no .NET SDK in the authoring
> environment). Validate with `dotnet run` on the example below before publishing.

## Use

```csharp
using Sekejap;

using var db = SekejapDb.Open("./data");        // IDisposable → closes on scope exit
Console.WriteLine(SekejapDb.Version());          // "0.16.2"

db.Execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)");
db.ExecuteParams("INSERT INTO places (_key, name, area) VALUES ($1, $2, $3)",
                 "[\"ubud\",\"Ubud\",\"central\"]");

string rows = db.Query("SELECT name FROM places WHERE area = 'central'");  // JSON
string? node = db.Get("places/ubud");            // null if missing

using var stmt = db.Prepare("SELECT _key FROM places WHERE area = $1");
string south = db.QueryPrepared(stmt, "[\"south\"]");

// graph
db.Link("tourists/chloe", "places/ubud", "visited");
db.Query("SELECT p.name FROM MATCH (t:tourists)-[:visited]->(p:places) WHERE t._key='chloe'");
```

Decode the JSON with `System.Text.Json`.

## Build & run the example

The native `libsekejap` must be discoverable at run time:

```bash
cargo build --release -p sekejap-capi
cd wrappers/csharp/example
DYLD_LIBRARY_PATH=../../../target/release dotnet run    # macOS
LD_LIBRARY_PATH=../../../target/release  dotnet run     # Linux
```

## Packaging

- **NuGet:** build `libsekejap` per RID, drop the artifacts in
  `wrappers/csharp/native/<rid>/native/`, then `dotnet pack Sekejap/Sekejap.csproj`
  (the csproj packs `../native/**` under `runtimes/`). `DllImport("sekejap")`
  resolves the right `libsekejap.{so,dylib}` / `sekejap.dll` per platform.
- **Unity:** place `libsekejap` for each platform under `Assets/Plugins/<platform>/`
  and add the `Sekejap` assembly. (A LINQ query provider — `db.Places.Where(...)` —
  is a planned ergonomic layer on top of this raw binding.)

## Caveats

- **Windows `long`:** the C ABI uses C `long` for `execute`/`put_many`/counts,
  which is 64-bit on macOS/Linux (matched by C# `long`) but **32-bit on Windows**.
  Add a Windows-specific marshaling pass (or harden the C ABI to `int64_t`) before
  shipping Windows binaries.
