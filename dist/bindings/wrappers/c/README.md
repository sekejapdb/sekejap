# sekejap-capi

A stable **C ABI** for [sekejap](../../README.md). This is the layer that lets
Swift, Kotlin, Dart, Go, and C/C++ drive the engine — the same way those
languages bind to SQLite's C API.

It exposes a handful of `extern "C"` functions over `sekejap::CoreDB`. The header
is [`include/sekejap.h`](include/sekejap.h).

## Why a C ABI

sekejap is written in Rust, but a language's ability to bind to it depends on a
flat **C ABI**, not on the source language. Rust produces exactly the same C ABI
a C library would (`extern "C"` + `cdylib`), so every ecosystem consumes this the
way it already knows how:

| Target | How it binds to this |
|---|---|
| **Swift** | C interop — a module map, then `import Sekejap` |
| **Dart / Flutter** | `dart:ffi` against `libsekejap.{so,dylib}` |
| **Kotlin / Java** | JNI wrapper (or Panama FFM); Kotlin/Native cinterop |
| **Go** | cgo `#include "sekejap.h"` |
| **C / C++** | include the header, link the library |

## Build

```bash
cargo build --release -p sekejap-capi
# produces target/release/libsekejap.{dylib,so}  (cdylib)
#      and  target/release/libsekejap.a          (staticlib)
```

Link against the library and include [`include/sekejap.h`](include/sekejap.h).

Runnable C programs (compile + run against `libsekejap`) live in
[`examples/`](examples/) — `cd examples && make test`.

## The surface

```c
// Lifecycle
SekejapDb *sekejap_open(const char *path);
SekejapDb *sekejap_open_paged(const char *path);       // mmap, fast startup
SekejapDb *sekejap_open_read_only(const char *path);
void       sekejap_close(SekejapDb *db);

// SQL (execute mutates; query/show return a JSON array string)
long  sekejap_execute       (SekejapDb *db, const char *sql);                       // rows, or -1
long  sekejap_execute_params(SekejapDb *db, const char *sql, const char *params);   // params = JSON array
char *sekejap_query         (SekejapDb *db, const char *sql);                       // JSON array, or NULL
char *sekejap_query_params  (SekejapDb *db, const char *sql, const char *params);   // injection-safe
char *sekejap_show          (SekejapDb *db, const char *sql);                       // SHOW TABLES/EDGES/...

// Prepared statements (PostgreSQL/libpq-style: prepare once, execute many)
SekejapStmt *sekejap_prepare       (SekejapDb *db, const char *sql);                          // compile ($1,$2,…)
char        *sekejap_query_prepared(SekejapDb *db, const SekejapStmt *stmt, const char *params); // JSON array
void         sekejap_stmt_free     (SekejapStmt *stmt);

// Direct node & edge mutation (no SQL)
int   sekejap_put      (SekejapDb *db, const char *slug, const char *payload_json);  // 0/-1
long  sekejap_put_many (SekejapDb *db, const char *rows_json);   // {slug: payload} object → count
int   sekejap_remove   (SekejapDb *db, const char *slug);
int   sekejap_link     (SekejapDb *db, const char *from, const char *to, const char *type);
int   sekejap_link_meta(SekejapDb *db, const char *from, const char *to, const char *type, const char *meta_json);
int   sekejap_unlink   (SekejapDb *db, const char *from, const char *to, const char *type);
char *sekejap_get      (SekejapDb *db, const char *slug);        // JSON, or NULL

// Introspection
int   sekejap_contains        (SekejapDb *db, const char *slug); // 1/0/-1
long  sekejap_node_count      (SekejapDb *db);
long  sekejap_edge_count      (SekejapDb *db);
char *sekejap_collection_names(SekejapDb *db);                   // JSON array
char *sekejap_schema_ddl      (SekejapDb *db, const char *collection);

// Maintenance
int   sekejap_compact    (SekejapDb *db);   // 0/-1
int   sekejap_sync       (SekejapDb *db);   // fsync buffered writes
void  sekejap_trim_memory(SekejapDb *db);   // reclaim RAM capacity

// Errors, memory, version
char       *sekejap_last_error (const SekejapDb *db);   // message, or NULL
void        sekejap_string_free(char *s);
const char *sekejap_version(void);                      // static; don't free
```

## Concurrent engine — build your own server

The default `SekejapDb*` handle wraps single-threaded `CoreDB`: **do not share one
handle across threads.** For a multi-threaded service, build with the `engine`
feature to get `SekejapEngine*` — a **thread-safe** handle over sekejap's `Engine`
(an `RwLock` over the store: many readers in parallel, one serialized writer). The
same `SekejapEngine*` **can be called concurrently from many threads**, which is
exactly what a server needs.

```bash
cargo build --release -p sekejap-capi --features engine
# then compile your C/Go/… server with -DSEKEJAP_ENGINE so the header exposes it
cc server.c -I sekejap-capi/include -DSEKEJAP_ENGINE -L target/release -lsekejap
```

```c
SekejapEngine *e = sekejap_engine_open("./data");    // or _open_memory()
// share `e` with all your worker threads:
char *rows = sekejap_engine_query(e, "SELECT * FROM t");   // concurrent read
long  n    = sekejap_engine_execute(e, "INSERT ...");      // serialized write
sekejap_engine_flush(e);                                   // group-commit (one fsync)
// errors are thread-local (like errno): sekejap_engine_last_error() (no db arg)
sekejap_engine_close(e);                                   // once, after all threads join
```

Engine surface (feature `engine`): `sekejap_engine_open` / `_open_memory` / `_close`,
`_query` / `_query_params`, `_execute` / `_execute_params`, `_flush`, `_compact`,
`_trim_memory`, and thread-local `sekejap_engine_last_error()`.

It's proven by `engine_handle_is_concurrent_and_safe` — 4 reader threads + 1 writer
thread pounding one handle at once, then asserting a durable, correct final count.
Run it with `cargo test -p sekejap-capi --features engine`.

## Ownership rules

- `SekejapDb*` is opaque; `sekejap_open*` creates it, `sekejap_close` frees it.
- Any `char*` **returned** by the library is yours — free it exactly once with
  `sekejap_string_free` (except `sekejap_version`, which is static).
- Strings you **pass in** are borrowed and must be null-terminated UTF-8; the
  library never frees them.
- Failure sentinels: `NULL` for pointer returns, `-1` for integer returns. After
  a failure, `sekejap_last_error(db)` returns the message.
- A Rust panic never crosses the boundary — it becomes an error return.

## Example (C)

```c
#include "sekejap.h"
#include <stdio.h>

int main(void) {
    SekejapDb *db = sekejap_open("./data");
    if (!db) { fprintf(stderr, "open failed\n"); return 1; }

    sekejap_execute(db, "CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)");
    sekejap_execute(db, "INSERT INTO t (_key, v) VALUES ('a', 42)");

    char *rows = sekejap_query(db, "SELECT v FROM t WHERE _key = 'a'");
    if (rows) { printf("%s\n", rows); sekejap_string_free(rows); }   // [{"v":42,...}]
    else      { char *e = sekejap_last_error(db);
                fprintf(stderr, "query failed: %s\n", e ? e : "?");
                sekejap_string_free(e); }

    sekejap_close(db);
    return 0;
}
```

## C++ (header-only)

For C++ there's an idiomatic RAII wrapper, [`include/sekejap.hpp`](include/sekejap.hpp)
— include it instead of `sekejap.h`. It links the same `libsekejap`, and adds:
handles that close/free themselves (move-only `Db`, `Stmt`), `std::string` in/out,
`std::optional<std::string>` for a missing node, and exceptions (`sekejap::Error`,
carrying `sekejap_last_error()`) instead of sentinel checks. Requires C++17.

```cpp
#include "sekejap.hpp"

int main() {
    sekejap::Db db = sekejap::Db::open("./data");                 // throws on failure
    db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)");
    db.executeParams("INSERT INTO t (_key, v) VALUES ($1, $2)", R"(["a",42])");

    std::string rows = db.query("SELECT v FROM t WHERE _key = 'a'");  // [{"v":42,...}]
    if (auto node = db.get("t/a")) { /* found */ }                    // std::optional

    auto stmt = db.prepare("SELECT _key FROM t WHERE v > $1");
    std::string big = db.queryPrepared(stmt, R"([10])");
    // db + stmt close/free themselves at scope end
}
```

Build (from the repo root):

```bash
clang++ -std=c++17 -Iwrappers/c/include wrappers/c/examples/cpp_tour.cpp \
  -Ltarget/release -lsekejap -Wl,-rpath,target/release -o cpp_tour && ./cpp_tour
```

The runnable tour with assertions is [`examples/cpp_tour.cpp`](examples/cpp_tour.cpp).

## Scope

The surface favors **UTF-8 in, JSON out**: query/show results come back as one
JSON array string rather than a row-iteration ABI, which keeps the boundary
small and hard to misuse — and it's enough to build any idiomatic per-language
binding on top. Not yet exposed (can be added later without breaking these
signatures): streaming result iteration, prepared statements, and the concurrent
`Engine` handle.

### Header is auto-generated (the build pipeline)

`include/sekejap.h` is **regenerated from the Rust `extern "C"` surface by
`build.rs` (via cbindgen) on every build** — it can't drift from the functions.
This is the C-ABI analog of what `maturin` does for the Python wrapper: the
artifact is produced from source, not hand-maintained. A cbindgen failure only
warns (the committed header stays as a fallback), so it never breaks `cargo
build`. Config lives in `cbindgen.toml`; you can also regenerate manually with
`cbindgen --config cbindgen.toml --output include/sekejap.h`.
