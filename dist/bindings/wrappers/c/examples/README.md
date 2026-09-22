# C examples

Runnable C programs that link `libsekejap` and drive the C ABI. The header
they include is [`dist/ffi/include/sekejap.h`](../../../../ffi/include/sekejap.h)
and the contract it carries is
[`docs/dist/C_ABI.md`](../../../../../docs/dist/C_ABI.md).

```sh
make tour       # build libsekejap, compile tour.c against it, run it
make clean
```

## Files

- **`tour.c`** -- the five stops of the surface in one program: documents and
  the paged scan, SQL with `$n` parameters and a prepared statement, the graph,
  a transaction that commits and one that rolls back, and the catalog. Every
  step is checked, and a failed check is a non-zero exit.
- **`Makefile`** -- builds the library with cargo and compiles against
  `dist/ffi/include`, setting the loader path so nothing has to be installed
  first.

The shortest program, the one `cd dist/ffi && make check` compiles and runs on
every build, is `dist/ffi/examples/smoke.c`.

## Using it in your own project

After `cd dist/ffi && make install` (default `PREFIX=/usr/local`):

```sh
cc my_app.c $(pkg-config --cflags --libs sekejap) -o my_app
```

Or point straight at the build tree:

```sh
cc my_app.c -I dist/ffi/include -L target/release -lsekejap -o my_app
```
