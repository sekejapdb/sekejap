# sekejap for Go

Go binding for the [sekejap](https://sekejap.life) embedded database — a
graph-first, multi-model engine (SQL + graph + spatial + vector + full-text) —
via **cgo** over the stable C ABI ([`dist/ffi`](../../../ffi/),
[`docs/dist/C_ABI.md`](../../../../docs/dist/C_ABI.md)).

Idiomatic Go surface: a `*DB` with methods, `error` returns, and
`[]map[string]any` result rows. Rows are addressed by **collection + key**,
not one slug string. Every call that crosses the boundary as JSON has two
forms: `Query`/`Get`/`Put`/… marshal into and out of idiomatic Go values, and
the `*JSON` twin (`QueryJSON`, `GetJSON`, `PutJSON`, …) hands the raw JSON
text straight through.

API: `Open`/`OpenWithConfig`/`OpenService`, `Put`/`PutMany`/`Get`/`Exists`/
`Delete`, `Scan`, `Execute`/`Query`/`Explain`, `Prepare` + `Stmt.Query`/
`Execute`/`Rebindable`, `QueryOpen` (paged answers), `Link`/`LinkWith`/
`Unlink`/`Neighbours`, `CreateCollection`/`DropCollection`/`Collections`/
`Describe`/`CountRows`/`ScanCountRows`/`ScanCountEdges`, `Begin` + `Tx.Put`/
`Delete`/`Link`/`Execute`/`Commit`/`Rollback`, `Checkpoint`/`Publish`/
`Storage`, the service-mode family (`StatementTimeout`, `Cancel`,
`ClearInterrupt`, `Subscribe`, `NextChange`, `Unsubscribe`), `Version`/
`FormatVersion`. Every C function in `sekejap.h` has a Go call site; see
"Approach" below for how the calls map.

Four calls are wrapped but always fail — `OpenMemory`, `TrimMemory`,
`Compact`, `Show` — because sekejap has no atomic under them
(`docs/dist/C_ABI.md` §4.10). They keep the symbol so a caller reaching for
the old name gets a refusal naming the reason, not a missing method;
`Checkpoint` is `Compact`'s replacement (it folds the WAL, it does not
rewrite rows).

## Install

sekejap is a cgo binding, so alongside the module you need the native
`libsekejap` on your system. Two steps:

**1. Install the native library** (once). From a clone of the repo:

```bash
make -C dist/ffi install                    # into /usr/local (may need sudo)
# or a user prefix, no sudo:
make -C dist/ffi install PREFIX="$HOME/.local"
```

This builds `libsekejap`, installs the header, and writes a `sekejap.pc` so
pkg-config can find it. (A prebuilt `libsekejap-<platform>.tar.gz`, built
from `dist/ffi`, is also attached to each GitHub release — see
`build-native-libs` / `release-native-libs` in `.github/workflows/release.yml`.)

**2. Add the module:**

```bash
go get github.com/sekejapdb/sekejap/dist/bindings/wrappers/go@latest
```

Then `import "github.com/sekejapdb/sekejap/dist/bindings/wrappers/go"` and build normally — the
default build resolves the library through pkg-config. If you installed to a
non-standard prefix, point pkg-config at it:

```bash
export PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig:$PKG_CONFIG_PATH"
```

If your `libsekejap` tarball lays the library and header flat in one
directory rather than under `lib/`/`include/` (as a locally built one might),
point `PKG_CONFIG_PATH` at a directory holding a matching `sekejap.pc` and
add the real library directory with `CGO_LDFLAGS="-L<dir> -Wl,-rpath,<dir>"`
— cgo appends `CGO_LDFLAGS` after the `#cgo pkg-config` flags, so the linker
still finds `-lsekejap` even when the `.pc` file's own `-L` does not resolve.

## Quick start

```go
package main

import (
    "fmt"
    sekejap "github.com/sekejapdb/sekejap/dist/bindings/wrappers/go"
)

func main() {
    db, _ := sekejap.Open("./data")   // a directory on disk
    defer db.Close()

    db.Execute(`CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)`)
    db.Execute(`INSERT INTO places (_key, name, area) VALUES ($1, $2, $3)`, "ubud", "Ubud", "central")

    rows, _ := db.Query(`SELECT name, area FROM places WHERE area = $1`, "central")
    fmt.Println(rows) // []map[string]any
}
```

See `examples/` for a runnable demo and `examples/tour/` for a five-model
tour (SQL, graph, spatial, vector, hybrid).

## Working in this repo (contributors)

The `sekejap_dev` build tag links against the workspace build output instead of a
system install — no `make install` needed:

```bash
cargo build --release -p sekejap-capi           # build libsekejap once (dist/ffi)
cd dist/bindings/wrappers/go
go test -tags sekejap_dev ./...                 # round-trip tests
go run  -tags sekejap_dev ./examples            # runnable demo
go run  -tags sekejap_dev ./examples/tour       # the five-model tour
```

Without a monorepo Rust build (a prebuilt `libsekejap` from
a directory such as `/path/to/libsekejap/`, or any release
tarball), skip the tag and set `PKG_CONFIG_PATH`/`CGO_LDFLAGS` per Install
above, then `go test ./...` as usual.

## Approach

cgo calls the C ABI directly (`#include "sekejap.h"`, link `libsekejap`). The link
flags live in build-tagged files: `cgo_pkgconfig.go` (default, pkg-config) and
`cgo_dev.go` (the `sekejap_dev` tag, monorepo paths into `dist/ffi`). The rest of
the package wraps the flat C functions in idiomatic Go, one file per area:
`sekejap.go` (open/close, documents, scans), `sql.go` (execute/query/explain,
prepared statements, paged query answers), `edges.go` (link/neighbours),
`catalog.go` (collections), `tx.go` (transactions), `maintenance.go`
(checkpoint/publish/storage), `service.go` (service-mode: timeout, cancel,
subscribe), `refused.go` (the four always-refused calls), `errors.go` (the
closed `Status` code and `*Error`).

## Releases

Go resolves versions from git tags. `go.mod`'s `module` line is unchanged
from 0.16 (`github.com/sekejapdb/sekejap/dist/bindings/wrappers/go`), so per Go's own
subdirectory-module convention its tag stays prefixed the same way:

```
dist/bindings/wrappers/go/v0.17.0      ← the tag Go reads for this module
```

not a bare `v0.17.0` (that versions the repo root, which isn't a Go module).
There is no CI release job to publish this module — Go has none to gate: a
consumer resolves any commit through its tag, and
[pkg.go.dev](https://pkg.go.dev/github.com/sekejapdb/sekejap/dist/bindings/wrappers/go)
indexes it automatically once the tag is pushed. `libsekejap` itself still
ships through `build-native-libs` / `release-native-libs`
(`.github/workflows/release.yml`), which this wrapper links against.

**A gap this move creates, flagged rather than silently worked around:** the
package itself now lives at `dist/bindings/wrappers/go/` (this directory),
not at `wrappers/go/` where it sat in 0.16. `go get`/the module proxy
resolve a subdirectory module by re-deriving its path from the MODULE NAME
(`github.com/sekejapdb/sekejap/dist/bindings/wrappers/go` → repo `github.com/sekejapdb/
sekejap` + subdirectory `wrappers/go`) and then looking for `go.mod` there —
which, after this move, is no longer where it looks. Keeping the module path
unchanged means `go get` of this module will not resolve
against its real location until either the module path is changed to
`.../dist/bindings/wrappers/go` (retagged to match) or the repository adds a
redirect (a `wrappers/go` stub module, or a vanity-import server) pointing
at this directory. Not fixed here: changing the module path is an API/path
decision for the project, out of scope for this wrapper.
