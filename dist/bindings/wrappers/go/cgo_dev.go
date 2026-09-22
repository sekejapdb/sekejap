//go:build sekejap_dev

package sekejap

// In-repo development build (`go test -tags sekejap_dev ./...`): link directly
// against the workspace build output, and embed an rpath so binaries find the
// shared library at run time without pkg-config or a system install. Build the
// library first with `cargo build --release -p sekejap-capi` (dist/ffi),
// which puts the header at dist/ffi/include/sekejap.h and the library in the
// workspace's own target/release, four directories above this wrapper.

// #cgo CFLAGS: -I${SRCDIR}/../../../ffi/include
// #cgo LDFLAGS: -L${SRCDIR}/../../../../target/release -lsekejap -Wl,-rpath,${SRCDIR}/../../../../target/release
import "C"
