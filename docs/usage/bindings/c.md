# C / C++

The stable C ABI is the layer every other binding stands on — the same way
languages bind to SQLite's C API. Build it from source:

```bash
cargo build --release -p sekejap-capi
# → target/release/libsekejap.{dylib,so}   (shared)
#   target/release/libsekejap.a            (static)
```

Include [`wrappers/c/include/sekejap.h`](../../../wrappers/c/include/sekejap.h)
and link the library.

## First query

```c
#include "sekejap.h"

SekejapDb *db = sekejap_open("./mydb");
sekejap_execute(db, "CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT)");
sekejap_execute(db, "INSERT INTO places (_key, name) VALUES ('a', 'Uluwatu')");

char *rows = sekejap_query(db, "SELECT * FROM places");  /* JSON string */
printf("%s\n", rows);
sekejap_string_free(rows);
sekejap_close(db);
```

Open modes mirror the Rust API: `sekejap_open`, `sekejap_open_paged`
(memory-mapped, fast startup), `sekejap_open_read_only`. Strings returned by
the library are freed with `sekejap_string_free`.

Runnable examples: [`wrappers/c/examples/`](../../../wrappers/c/examples/)
(`make test`; `make tour` runs the five-stop tour). This ABI is also the bridge for any language not listed in
these pages — anything with a C FFI can drive the engine.
