//go:build !sekejap_dev

package sekejap

// Default build: resolve libsekejap's header and library via pkg-config, so an
// installed consumer needs no hardcoded paths. Install the library + its
// sekejap.pc first (see the README): a prebuilt release tarball, or
// `make -C wrappers/c install`.

// #cgo pkg-config: sekejap
import "C"
