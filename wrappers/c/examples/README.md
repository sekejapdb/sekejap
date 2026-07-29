# sekejap C ABI — examples

Runnable C programs that link `libsekejap` and exercise the [C ABI](../include/sekejap.h).
This is how you verify the library from C, the same way you'd test SQLite.

## Run them

```bash
make test      # build libsekejap + compile & run test.c (asserts the whole ABI)
make asan      # same, under AddressSanitizer (catches C-side memory bugs)
make server    # build with the engine feature + run server.c (4 reader threads + writer)
make clean
```

`make test` compiles `test.c` against `../include/sekejap.h`, links the release
`libsekejap`, and runs it — you should see `OK: all sekejap C ABI assertions passed`.

## Files

- **`test.c`** — open a DB, DDL + SQL insert, direct `put`/`link`, a parameterized
  (injection-safe) query, introspection, and the error path — all checked with `assert`.
- **`server.c`** — the concurrent story: one thread-safe `SekejapEngine*` shared by
  4 reader threads while the main thread writes, then a durable final count. Needs the
  `engine` feature (the Makefile builds `--features engine` and compiles with
  `-DSEKEJAP_ENGINE`).
- **`Makefile`** — builds the lib via cargo, compiles the examples, sets the loader
  path to run them; also `make install` (lib + header + pkg-config into `$(PREFIX)`).
- **`sekejap.pc.in`** — pkg-config template; `make install` fills in the prefix so
  consumers build with `cc app.c $(pkg-config --cflags --libs sekejap)`.

## Using it in your own project

After `make install` (default `PREFIX=/usr/local`):

```bash
cc my_app.c $(pkg-config --cflags --libs sekejap) -o my_app
```

Or point directly at the build tree without installing:

```bash
cc my_app.c -I path/to/wrappers/c/include -L path/to/target/release -lsekejap -o my_app
```
