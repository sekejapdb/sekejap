# sekejap for Go

Go binding for the [sekejap](https://sekejap.life) embedded database — a
graph-first, multi-model engine (SQL + graph + spatial + vector + full-text) —
via **cgo** over the stable C ABI.

Idiomatic Go surface: a `*DB` with methods, `error` returns, and `[]map[string]any`
result rows. API: `Open`, `Execute`, `Query`/`QueryParams`, `Put`, `Get`, `Link`,
`Contains`, `NodeCount`/`EdgeCount`, `Compact`, `Version`.

## Install

sekejap is a cgo binding, so alongside the module you need the native
`libsekejap` on your system. Two steps:

**1. Install the native library** (once). From a clone of the repo:

```bash
make -C wrappers/c install           # into /usr/local (may need sudo)
# or a user prefix, no sudo:
make -C wrappers/c install PREFIX="$HOME/.local"
```

This builds `libsekejap`, installs the header, and writes a `sekejap.pc` so
pkg-config can find it. (A prebuilt `libsekejap-<platform>.tar.gz` is also
attached to each GitHub release.)

**2. Add the module:**

```bash
go get github.com/sekejapdb/sekejap/wrappers/go@latest
```

Then `import "github.com/sekejapdb/sekejap/wrappers/go"` and build normally — the
default build resolves the library through pkg-config. If you installed to a
non-standard prefix, point pkg-config at it:

```bash
export PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig:$PKG_CONFIG_PATH"
```

## Quick start

```go
package main

import (
    "fmt"
    sekejap "github.com/sekejapdb/sekejap/wrappers/go"
)

func main() {
    db, _ := sekejap.Open("./data")   // directory on disk; "" for in-memory
    defer db.Close()

    db.Execute(`CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)`)
    db.Execute(`INSERT INTO places (_key, name, area) VALUES ('ubud', 'Ubud', 'central')`)

    rows, _ := db.Query(`SELECT name, area FROM places WHERE area = 'central'`)
    fmt.Println(rows) // []map[string]any
}
```

See `examples/` for a five-model tour (SQL, graph, spatial, vector, hybrid).

## Working in this repo (contributors)

The `sekejap_dev` build tag links against the workspace build output instead of a
system install — no `make install` needed:

```bash
cargo build --release -p sekejap-capi     # build libsekejap once
cd wrappers/go
go test -tags sekejap_dev ./...           # round-trip tests
go run  -tags sekejap_dev ./examples      # runnable demo
```

## Approach

cgo calls the C ABI directly (`#include "sekejap.h"`, link `libsekejap`). The link
flags live in build-tagged files: `cgo_pkgconfig.go` (default, pkg-config) and
`cgo_dev.go` (the `sekejap_dev` tag, monorepo paths). `sekejap.go` wraps the flat
C functions in idiomatic Go.

## Releases

Go resolves versions from git tags. Because this module lives in a subdirectory,
its tags are **prefixed with the module path**:

```
wrappers/go/v0.16.2      ← the tag Go reads for this module
```

not a bare `v0.16.2` (that versions the repo root, which isn't a Go module). The
release helper `scripts/tag-release.sh` creates both from the Cargo.toml version.
[pkg.go.dev](https://pkg.go.dev/github.com/sekejapdb/sekejap/wrappers/go) indexes
it automatically once pushed.
