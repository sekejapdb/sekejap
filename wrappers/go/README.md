# sekejap for Go

**Status: working** (tested end-to-end). Go binding via **cgo** over the C ABI
(`../c`, `libsekejap`).

## Run it

```bash
cargo build --release -p sekejap-capi     # build libsekejap (once)
cd wrappers/go
go test ./...                             # round-trip test — passes
go run ./examples                         # runnable demo
```

The cgo directives embed an rpath to the workspace `target/release`, so binaries
find `libsekejap` at run time with no `LD_LIBRARY_PATH` needed. API: `Open`,
`Execute`, `Query`/`QueryParams` (→ `[]map[string]any`), `Put`, `Get`, `Link`,
`Contains`, `NodeCount`/`EdgeCount`, `Compact`, `Version`.

## Approach

cgo lets Go call the C ABI directly — `#include "sekejap.h"` and link `libsekejap`.
A thin `sekejap.go` wraps the flat C functions in idiomatic Go types (a `*DB` with
methods, `error` returns, `[]byte`/`string` for JSON).

```
wrappers/go/
├── go.mod
├── sekejap.go        # cgo: #include "sekejap.h"; wrap the C ABI in a Go API
└── examples/         # *.go usage + a runnable main
```

The cgo preamble points at the C ABI header + built library, e.g.:

```go
// #cgo CFLAGS: -I${SRCDIR}/../c/include
// #cgo LDFLAGS: -L${SRCDIR}/../../target/release -lsekejap
// #include "sekejap.h"
import "C"
```

(For distribution you'd instead resolve the header/lib via pkg-config —
`#cgo pkg-config: sekejap` — once `make install` has placed `sekejap.pc`.)

## Distribution

- **No upload registry.** Go modules are distributed straight from a Git repo.
- **Install:** `go get github.com/sekejapdb/sekejap/wrappers/go`
- **Publish:** git-tag a release (`git tag v0.13.0 && git push --tags`);
  [pkg.go.dev](https://pkg.go.dev) indexes it automatically. The module path in
  `go.mod` must match the repository URL.
- Consumers need `libsekejap` available at build/run time (system-installed via
  `make install`, or vendored). Document this in the module README.
