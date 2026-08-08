// Package sekejap is a Go binding to the sekejap embedded database — a graph-first,
// multi-model engine (SQL + graph + spatial + vector + full-text) — via cgo over
// the C ABI (wrappers/c, libsekejap).
//
// Build requirement: libsekejap must be built first:
//
//	cargo build --release -p sekejap-capi
//
// The cgo directives below point at the workspace build output and embed an rpath
// so binaries find the shared library at run time without LD_LIBRARY_PATH.
package sekejap

// The link flags live in build-tagged files: cgo_pkgconfig.go (default, resolves
// libsekejap via pkg-config for installed consumers) and cgo_dev.go (the
// `sekejap_dev` tag, links against the monorepo build output for in-repo work).

// #include <stdlib.h>
// #include "sekejap.h"
import "C"

import (
	"encoding/json"
	"errors"
	"runtime"
	"unsafe"
)

// DB is an open sekejap database handle. Not safe for concurrent use from
// multiple goroutines (it wraps single-threaded CoreDB); serialize access, or use
// the engine handle once exposed. Call Close when done.
type DB struct {
	ptr *C.SekejapDb
}

// Open opens (or creates) a database at the given directory path.
func Open(path string) (*DB, error) {
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))
	ptr := C.sekejap_open(cpath)
	if ptr == nil {
		return nil, errors.New("sekejap: open failed")
	}
	db := &DB{ptr: ptr}
	runtime.SetFinalizer(db, (*DB).Close)
	return db, nil
}

// OpenPaged opens a database in paged (mmap) mode — fast startup regardless of size.
func OpenPaged(path string) (*DB, error) {
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))
	ptr := C.sekejap_open_paged(cpath)
	if ptr == nil {
		return nil, errors.New("sekejap: open_paged failed")
	}
	db := &DB{ptr: ptr}
	runtime.SetFinalizer(db, (*DB).Close)
	return db, nil
}

// Close frees the handle. Safe to call more than once.
func (db *DB) Close() {
	if db.ptr != nil {
		C.sekejap_close(db.ptr)
		db.ptr = nil
		runtime.SetFinalizer(db, nil)
	}
}

// Execute runs a mutating statement (CREATE / INSERT / UPDATE / DELETE / ALTER /
// BEGIN / COMMIT / edge insert). Returns the number of affected rows.
func (db *DB) Execute(sql string) (int64, error) {
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	n := int64(C.sekejap_execute(db.ptr, csql))
	if n < 0 {
		return 0, db.lastError()
	}
	return n, nil
}

// QueryJSON runs a SELECT and returns the raw JSON-array result string.
func (db *DB) QueryJSON(sql string) (string, error) {
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	out := C.sekejap_query(db.ptr, csql)
	if out == nil {
		return "", db.lastError()
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), nil
}

// Query runs a SELECT and returns the rows decoded into a slice of maps.
func (db *DB) Query(sql string) ([]map[string]any, error) {
	js, err := db.QueryJSON(sql)
	if err != nil {
		return nil, err
	}
	return decodeRows(js)
}

// QueryParams runs a parameterized SELECT ($1, $2, …) with params bound from the
// given values — the injection-safe way to pass user input.
func (db *DB) QueryParams(sql string, params ...any) ([]map[string]any, error) {
	pj, err := json.Marshal(params)
	if err != nil {
		return nil, err
	}
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	cpj := C.CString(string(pj))
	defer C.free(unsafe.Pointer(cpj))
	out := C.sekejap_query_params(db.ptr, csql, cpj)
	if out == nil {
		return nil, db.lastError()
	}
	defer C.sekejap_string_free(out)
	return decodeRows(C.GoString(out))
}

// Stmt is a prepared (compiled) query. Create with DB.Prepare, run with
// DB.QueryPrepared, free with Close. Reusable for the same query shape with
// different parameter values.
type Stmt struct {
	ptr *C.SekejapStmt
}

// Prepare compiles sql (with $1, $2, … placeholders) into a reusable prepared
// statement — parsed once, executed many times.
func (db *DB) Prepare(sql string) (*Stmt, error) {
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	ptr := C.sekejap_prepare(db.ptr, csql)
	if ptr == nil {
		return nil, db.lastError()
	}
	s := &Stmt{ptr: ptr}
	runtime.SetFinalizer(s, (*Stmt).Close)
	return s, nil
}

// QueryPreparedJSON runs a prepared statement, binding params, and returns the raw
// JSON-array result string.
func (db *DB) QueryPreparedJSON(stmt *Stmt, params ...any) (string, error) {
	pj, err := json.Marshal(params)
	if err != nil {
		return "", err
	}
	cpj := C.CString(string(pj))
	defer C.free(unsafe.Pointer(cpj))
	out := C.sekejap_query_prepared(db.ptr, stmt.ptr, cpj)
	if out == nil {
		return "", db.lastError()
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), nil
}

// QueryPrepared runs a prepared statement and decodes the rows into a slice of maps.
func (db *DB) QueryPrepared(stmt *Stmt, params ...any) ([]map[string]any, error) {
	js, err := db.QueryPreparedJSON(stmt, params...)
	if err != nil {
		return nil, err
	}
	return decodeRows(js)
}

// Close frees the prepared statement. Safe to call more than once.
func (s *Stmt) Close() {
	if s.ptr != nil {
		C.sekejap_stmt_free(s.ptr)
		s.ptr = nil
		runtime.SetFinalizer(s, nil)
	}
}

// Put inserts or replaces one node by slug ("collection/key") with a JSON payload.
func (db *DB) Put(slug, payloadJSON string) error {
	cslug := C.CString(slug)
	defer C.free(unsafe.Pointer(cslug))
	cjson := C.CString(payloadJSON)
	defer C.free(unsafe.Pointer(cjson))
	if C.sekejap_put(db.ptr, cslug, cjson) != 0 {
		return db.lastError()
	}
	return nil
}

// Get fetches one node's payload by slug as a JSON string; ok is false if absent.
func (db *DB) Get(slug string) (payload string, ok bool, err error) {
	cslug := C.CString(slug)
	defer C.free(unsafe.Pointer(cslug))
	out := C.sekejap_get(db.ptr, cslug)
	if out == nil {
		// Clean miss clears last_error; a real error sets it.
		if e := db.lastError(); e != nil && e.Error() != "sekejap: unknown error" {
			return "", false, e
		}
		return "", false, nil
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), true, nil
}

// Link creates a plain edge from -> to of the given type (slugs are "collection/key").
func (db *DB) Link(from, to, edgeType string) error {
	cf, ct, ce := C.CString(from), C.CString(to), C.CString(edgeType)
	defer C.free(unsafe.Pointer(cf))
	defer C.free(unsafe.Pointer(ct))
	defer C.free(unsafe.Pointer(ce))
	if C.sekejap_link(db.ptr, cf, ct, ce) != 0 {
		return db.lastError()
	}
	return nil
}

// LinkMeta creates an edge carrying attributes (a JSON object).
func (db *DB) LinkMeta(from, to, edgeType, metaJSON string) error {
	cf, ct, ce, cm := C.CString(from), C.CString(to), C.CString(edgeType), C.CString(metaJSON)
	defer C.free(unsafe.Pointer(cf))
	defer C.free(unsafe.Pointer(ct))
	defer C.free(unsafe.Pointer(ce))
	defer C.free(unsafe.Pointer(cm))
	if C.sekejap_link_meta(db.ptr, cf, ct, ce, cm) != 0 {
		return db.lastError()
	}
	return nil
}

// Contains reports whether a node with the given slug exists.
func (db *DB) Contains(slug string) bool {
	cslug := C.CString(slug)
	defer C.free(unsafe.Pointer(cslug))
	return C.sekejap_contains(db.ptr, cslug) == 1
}

// NodeCount returns the number of nodes.
func (db *DB) NodeCount() int64 { return int64(C.sekejap_node_count(db.ptr)) }

// EdgeCount returns the number of edges.
func (db *DB) EdgeCount() int64 { return int64(C.sekejap_edge_count(db.ptr)) }

// Compact truncates the WAL, rewrites payloads/topology, and reclaims RAM.
func (db *DB) Compact() error {
	if C.sekejap_compact(db.ptr) != 0 {
		return db.lastError()
	}
	return nil
}

// Version returns the library version.
func Version() string {
	return C.GoString(C.sekejap_version())
}

func (db *DB) lastError() error {
	msg := C.sekejap_last_error(db.ptr)
	if msg == nil {
		return errors.New("sekejap: unknown error")
	}
	defer C.sekejap_string_free(msg)
	return errors.New("sekejap: " + C.GoString(msg))
}

func decodeRows(js string) ([]map[string]any, error) {
	var rows []map[string]any
	if err := json.Unmarshal([]byte(js), &rows); err != nil {
		return nil, err
	}
	return rows, nil
}
