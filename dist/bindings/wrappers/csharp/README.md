# sekejap for C# / .NET (and Unity)

C# binding for the [sekejap](https://sekejap.life) embedded database --
collections addressed by name + key, JSON documents, SQL with `$n`
parameters, scans, prepared statements, transactions, links and bounded
graph neighbours, the catalog, and (service mode) parallel readers with a
change feed -- via P/Invoke over the stable C ABI (`dist/ffi`,
`docs/dist/C_ABI.md`).

Targets **.NET** backends and **Unity 2021+** (`netstandard2.1`). Idiomatic
surface: `SekejapDb : IDisposable`, `SekejapScan`/`SekejapStatement`/`SekejapTx`
each `IDisposable`, exceptions (`SekejapException`, carrying the closed
`SekejapStatus` code alongside the message), `string?` for a clean miss,
`using` for deterministic cleanup. Results are JSON strings; decode with
`System.Text.Json`.

> **Status:** this is a 0.17.0 port written directly against
> `dist/ffi/include/sekejap.h` (the header, not the old 0.16 one) and
> `docs/dist/C_ABI.md`. It compiles as a standard `netstandard2.1` library by
> inspection, but **it has not been built or run in the authoring
> environment: there is no .NET SDK installed there (`dotnet` is not on
> `PATH`)**. Nothing here has executed against `libsekejap`. Before
> publishing, build and run `example/Program.cs` yourself (see "Build & run
> the example" below) and treat that as the first real compile.

## Use

```csharp
using Sekejap;

using var db = SekejapDb.Open("./data");         // IDisposable -> closes on scope exit
Console.WriteLine(SekejapDb.Version());           // "0.17.0"

db.CreateCollection("places",
    "[{\"name\":\"name\",\"kind\":\"text\"},{\"name\":\"area\",\"kind\":\"text\"}]");
db.Put("places", "ubud", "{\"_key\":\"ubud\",\"name\":\"Ubud\",\"area\":\"central\"}");

string rows = db.Query("SELECT name FROM places WHERE area = $1", "[\"central\"]");  // JSON
string? doc = db.Get("places", "ubud");           // null if missing, not an exception

using var stmt = db.Prepare("SELECT _key FROM places WHERE area = $1");
string south = stmt.Query("[\"south\"]");
stmt.Query("[\"central\"]");                      // a rebind of the same compiled plan

db.Link("tourists", "chloe", "visited", "places", "ubud");
string neighbours = db.Neighbours("tourists", "chloe", "visited", SekejapDirection.Outgoing, limit: 10);

using (var tx = db.TxBegin())
{
    tx.Put("places", "sanur", "{\"_key\":\"sanur\",\"name\":\"Sanur\",\"area\":\"south\"}");
    tx.Commit();                                  // or tx.Rollback(); Dispose() rolls back if neither ran
}

long n = db.CountRows("places");
```

`example/Program.cs` is a longer, runnable tour of the same surface: open,
declare a collection, put, get, a parameterised query, a paged scan, prepare
+ rebind, link + neighbours, a transaction committed and one rolled back,
`CountRows`, and an error path that catches `SekejapException` and reads its
`Status`/`Message`.

## What changed from the 0.16 wrapper (e1)

`e1`'s wrapper (also under this path, and on branch `e1`) was the packaging,
build-script and example INSPIRATION, not the API: 0.17 addresses a row by
COLLECTION + KEY rather than one slug string, and the API is reshaped around
`docs/dist/RUST_API.md` end to end. Concretely, against `Native.cs`:

- `sekejap_open_paged` / `sekejap_open_read_only` are gone. `sekejap_open`,
  `sekejap_open_with_config` (a JSON store config) and `sekejap_open_service`
  (parallel readers, change feed, statement timeout, cancel) replace them.
- `put` / `get` / `remove` (now `delete`) / `contains` (now `exists`) /
  `link` (now five arguments: `from_collection, from_key, edge_type,
  to_collection, to_key`) all take a COLLECTION and a KEY where 0.16 took one
  slug.
- `execute`/`execute_params` and `query`/`query_params` collapsed into single
  `sekejap_execute(db, sql, params_json)` / `sekejap_query(db, sql,
  params_json)` calls, with `params_json` nullable for "no parameters" --
  mirrored here as an optional `paramsJson` argument rather than two
  overloaded native entry points.
- `sekejap_query_prepared(db, stmt, params)` became
  `sekejap_stmt_query(stmt, params)` (no `db` argument -- the statement
  already borrows it) plus `sekejap_stmt_execute` for a writing prepared
  statement and `sekejap_stmt_rebindable` to ask whether a further bind
  compiles anything.
- `sekejap_node_count` / `sekejap_edge_count` became `sekejap_count_rows` (a
  named collection, from the live record) and `sekejap_scan_count_edges` (an
  explicit walk -- sekejap keeps no O(1) edge counter). There is no longer a
  single all-collections node count: `sekejap_scan_count_rows` and
  `sekejap_count_rows` both take a collection name.
- `sekejap_collection_names` became `sekejap_collections`; `sekejap_describe`
  is new (the declared shape of one collection, `null` for no such
  collection -- a clean miss, not a failure).
- Brand new in this wrapper, with no 0.16 predecessor: `sekejap_put_many`,
  `sekejap_scan_open`/`sekejap_scan_next`/`sekejap_scan_close` (a paged
  collection walk), `sekejap_explain`, `sekejap_query_open` (a paged
  statement answer, over the same `SekejapScan` type as a collection scan),
  `sekejap_link_with`, `sekejap_unlink`, `sekejap_neighbours`,
  `sekejap_create_collection`/`sekejap_drop_collection`, the whole
  `sekejap_tx_*` family (`SekejapTx`), `sekejap_checkpoint`/`sekejap_publish`/
  `sekejap_storage`, the service-mode family
  (`sekejap_statement_timeout_ms`/`sekejap_cancel`/`sekejap_clear_interrupt`/
  `sekejap_subscribe`/`sekejap_next_change`/`sekejap_unsubscribe`), and the
  REFUSED-by-name stubs `sekejap_open_memory`/`sekejap_trim_memory`/
  `sekejap_show` (`sekejap_compact` already existed in 0.16 and is REFUSED
  here instead -- see `docs/dist/C_ABI.md` §4.8).
- `sekejap_last_error_code`, absent in 0.16, is now read on every failure
  path and carried on `SekejapException.Status` as a `SekejapStatus` enum, so
  a caller can branch (`UnknownRow`, `Busy`, `Refused`, ...) without parsing
  the message.
- The single-file `Native.cs` + `SekejapDb.cs` pair grew into
  `Native.cs` (still one P/Invoke declaration per C function),
  `SekejapEnums.cs` (`SekejapStatus`, `SekejapDirection`, `SekejapRebind`,
  `SekejapCheckpointResult`), `SekejapException.cs`, `Interop.cs` (the shared
  C-string-ownership and error-translation helpers), and one file per handle
  type (`SekejapDb.cs`, `SekejapScan.cs`, `SekejapStatement.cs`,
  `SekejapTx.cs`) -- an organisational change only; the SDK-style `.csproj`
  globs `**/*.cs`, so nothing needed updating there for the split.

Nothing was removed as "no longer applicable": every 0.16 call this wrapper
exposed maps onto a 0.17 one under a new name and signature, so there was no
e1 glue to delete outright (contrast the Rust-backed wrappers in
`dist/bindings/README.md`, several of which drop a whole Rust glue crate).

## Build & run the example

The native `libsekejap` must be discoverable at run time. It is NOT built by
this wrapper or by anything under `dist/bindings/wrappers/csharp/` -- build
it once from `dist/ffi` (outside this wrapper's scope):

```bash
cargo build --release -p sekejap-capi     # produces libsekejap.{dylib,so,a}
cd dist/bindings/wrappers/csharp/example
DYLD_LIBRARY_PATH=<path-to-target>/release dotnet run    # macOS
LD_LIBRARY_PATH=<path-to-target>/release  dotnet run     # Linux
```

This wrapper has not been built or run with `dotnet` yet -- neither
`dotnet build` nor `dotnet run` has ever executed against this source. Treat every signature above as reviewed against
`dist/ffi/include/sekejap.h` by hand, not as compiler-checked.

## Packaging

- **NuGet:** build `libsekejap` per RID, drop the artifacts in
  `wrappers/csharp/native/<rid>/native/`, then `dotnet pack Sekejap/Sekejap.csproj`
  (the csproj packs `../native/**` under `runtimes/`). `DllImport("sekejap")`
  resolves the right `libsekejap.{so,dylib}` / `sekejap.dll` per platform.
  `PackageId` is `Sekejap`, `Version` is `0.17.0` in step with the workspace.
- **Unity:** place `libsekejap` for each platform under `Assets/Plugins/<platform>/`
  and add the `Sekejap` assembly.
- **CI / release job:** this wrapper's publish job in
  `.github/workflows/release.yml` is intentionally left as-is by this
  change -- see "Publish job" below.

## Publish job

`.github/workflows/release.yml` was not touched by this port (out of scope
for this wrapper). There is currently no `publish-csharp` (or
`dotnet`/NuGet) job in that workflow to gate or un-gate. Separately, even
where the general rule is to un-gate a wrapper's publish job once its glue
compiles against the header, this wrapper is an exception: it has not been
built or run with `dotnet` yet, so nothing here has actually compiled, and a
publish job -- gated or not -- must
not be added or enabled on the strength of an unverified port. Whoever adds
a `publish-csharp` job should gate it (`if: false`, with a comment pointing
at this README's Status section) until `dotnet build`/`dotnet run` on
`example/` have actually passed on a machine with the .NET SDK.

## Caveats

- **Not compiled here.** See "Status" above -- this is a hand-reviewed port,
  not a build-verified one.
- **Windows `long`:** the C ABI uses C `long` for `execute`/`put_many`/
  `stmt_execute`/the count and subscription calls, which is 64-bit on
  macOS/Linux (matched by C# `long`) but **32-bit on Windows**. Add a
  Windows-specific marshaling pass (or harden the C ABI to `int64_t`) before
  shipping Windows binaries.
- **JSON stays JSON.** Every document, parameter list, row set and catalog
  answer crosses the boundary -- and this wrapper's public surface -- as a
  raw JSON string, matching `docs/dist/C_ABI.md` §2. There is no typed model
  layer (a `Places` collection with `.Where(...)`) here; decode with
  `System.Text.Json` at the call site.
