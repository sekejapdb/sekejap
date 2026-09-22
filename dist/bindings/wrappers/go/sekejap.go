// Package sekejap is a Go binding to the sekejap embedded database -- a
// graph-first, multi-model engine (SQL + graph + spatial + vector +
// full-text) -- via cgo over the stable C ABI (dist/ffi, libsekejap).
//
// Rows are addressed by collection + key, not by one slug string. Documents,
// query parameters and result rows all cross the boundary as JSON: a method
// ending in JSON hands back (or takes) the raw string sekejap uses, and the
// method of the same name without the suffix marshals it into idiomatic Go
// -- map[string]any for a row or a document, `any` for a value to write.
//
// sekejap::Db is Send+Sync, so one *DB may be called from many goroutines at
// once. A *Stmt, a *Scan and a *Tx derived from it are each for ONE
// goroutine at a time, and must be freed before the *DB they came from is
// closed.
//
// # Build
//
// libsekejap must be built or installed first (docs/dist/C_ABI.md §5):
//
//	cargo build --release -p sekejap-capi
//
// The link flags live in build-tagged files: cgo_pkgconfig.go (default,
// resolves libsekejap via pkg-config for an installed consumer) and
// cgo_dev.go (the `sekejap_dev` build tag, links against this repo's own
// build output for in-repo work).
package sekejap

// #include <stdlib.h>
// #include "sekejap.h"
import "C"

import (
	"encoding/json"
	"runtime"
	"unsafe"
)

// DB is an open sekejap database handle. Call Close when done; every Stmt,
// Scan and Tx taken from it must be freed first.
type DB struct {
	ptr *C.SekejapDb
}

// Config is sekejap_open_with_config's store configuration. Every field is
// optional; a nil/empty field keeps sekejap's own default. IO is
// "buffered" or "direct"; Sync is "full" (sekejap's default), "normal" or
// "off".
type Config struct {
	BudgetBytes *int64 `json:"budget_bytes,omitempty"`
	IO          string `json:"io,omitempty"`
	Sync        string `json:"sync,omitempty"`
}

// Open opens (or creates) a database at the given directory path.
func Open(path string) (*DB, error) {
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))
	ptr := C.sekejap_open(cpath)
	if ptr == nil {
		return nil, lastError()
	}
	return newDB(ptr), nil
}

// OpenWithConfig opens (or creates) a database under a store configuration.
// A nil config is sekejap's own default.
func OpenWithConfig(path string, config *Config) (*DB, error) {
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))
	var cconfig *C.char
	if config != nil {
		j, err := json.Marshal(config)
		if err != nil {
			return nil, err
		}
		cconfig = C.CString(string(j))
		defer C.free(unsafe.Pointer(cconfig))
	}
	ptr := C.sekejap_open_with_config(cpath, cconfig)
	if ptr == nil {
		return nil, lastError()
	}
	return newDB(ptr), nil
}

// OpenService opens the database in SERVICE mode: one writer, parallel
// readers on a published snapshot, and the change feed, the statement
// timeout and the cancel family (docs/dist/OPS_CONTRACT.md §1-§5). Those
// calls -- StatementTimeout, Cancel, ClearInterrupt, Subscribe, NextChange,
// Unsubscribe -- answer only on a handle opened this way; on any other
// handle they are refused by name.
func OpenService(path string) (*DB, error) {
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))
	ptr := C.sekejap_open_service(cpath)
	if ptr == nil {
		return nil, lastError()
	}
	return newDB(ptr), nil
}

func newDB(ptr *C.SekejapDb) *DB {
	db := &DB{ptr: ptr}
	runtime.SetFinalizer(db, (*DB).Close)
	return db
}

// Close frees the handle. Safe to call more than once. Uncommitted work is
// discarded: a close is not a commit.
func (db *DB) Close() {
	if db.ptr != nil {
		C.sekejap_close(db.ptr)
		db.ptr = nil
		runtime.SetFinalizer(db, nil)
	}
}

// Version returns the library version, as MAJOR.MINOR.PATCH.
func Version() string { return C.GoString(C.sekejap_version()) }

// FormatVersion returns the disk format this build reads and writes.
func FormatVersion() int32 { return int32(C.sekejap_format_version()) }

// ---- documents ----------------------------------------------------------

// Put writes one document, committed before this call returns. doc is
// marshaled to JSON; if the marshaled object carries a _key member, it must
// equal key. A collection that is not in the catalog is a failure, not an
// implicit create -- declare it first with CreateCollection.
func (db *DB) Put(collection, key string, doc any) error {
	j, err := json.Marshal(doc)
	if err != nil {
		return err
	}
	return db.PutJSON(collection, key, string(j))
}

// PutJSON is Put, taking the document as a JSON object already encoded.
func (db *DB) PutJSON(collection, key, documentJSON string) error {
	ccol, ckey, cdoc := C.CString(collection), C.CString(key), C.CString(documentJSON)
	defer C.free(unsafe.Pointer(ccol))
	defer C.free(unsafe.Pointer(ckey))
	defer C.free(unsafe.Pointer(cdoc))
	if C.sekejap_put(db.ptr, ccol, ckey, cdoc) != 0 {
		return lastError()
	}
	return nil
}

// PutRow is one row of a PutMany batch.
type PutRow struct {
	Key string `json:"key"`
	Doc any    `json:"doc"`
}

// PutMany writes many documents into one collection under ONE commit,
// returning the rows written. A failure stores none of the batch.
func (db *DB) PutMany(collection string, rows []PutRow) (int64, error) {
	j, err := json.Marshal(rows)
	if err != nil {
		return 0, err
	}
	return db.PutManyJSON(collection, string(j))
}

// PutManyJSON is PutMany, taking the rows already encoded as a JSON array of
// {"key": "...", "doc": { ... }}.
func (db *DB) PutManyJSON(collection, rowsJSON string) (int64, error) {
	ccol, crows := C.CString(collection), C.CString(rowsJSON)
	defer C.free(unsafe.Pointer(ccol))
	defer C.free(unsafe.Pointer(crows))
	return count(C.sekejap_put_many(db.ptr, ccol, crows))
}

// Get reads one document, decoded into a map keyed by field name (with
// _key set to the row's external key). ok is false for a clean miss.
func (db *DB) Get(collection, key string) (map[string]any, bool, error) {
	js, ok, err := db.GetJSON(collection, key)
	if err != nil || !ok {
		return nil, ok, err
	}
	var doc map[string]any
	if err := json.Unmarshal([]byte(js), &doc); err != nil {
		return nil, false, err
	}
	return doc, true, nil
}

// GetJSON is Get, returning the document as raw JSON text.
func (db *DB) GetJSON(collection, key string) (payload string, ok bool, err error) {
	ccol, ckey := C.CString(collection), C.CString(key)
	defer C.free(unsafe.Pointer(ccol))
	defer C.free(unsafe.Pointer(ckey))
	out := C.sekejap_get(db.ptr, ccol, ckey)
	if out == nil {
		if lastErrorIsOk() {
			return "", false, nil
		}
		return "", false, lastError()
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), true, nil
}

// Exists reports whether a row with the given collection and key exists.
func (db *DB) Exists(collection, key string) (bool, error) {
	ccol, ckey := C.CString(collection), C.CString(key)
	defer C.free(unsafe.Pointer(ccol))
	defer C.free(unsafe.Pointer(ckey))
	return tribool(C.sekejap_exists(db.ptr, ccol, ckey))
}

// Delete removes one row and every edge that touches it, committed.
// Reports whether the row was there.
func (db *DB) Delete(collection, key string) (bool, error) {
	ccol, ckey := C.CString(collection), C.CString(key)
	defer C.free(unsafe.Pointer(ccol))
	defer C.free(unsafe.Pointer(ckey))
	return tribool(C.sekejap_delete(db.ptr, ccol, ckey))
}

// ---- scans ----------------------------------------------------------------

// Scan is a paged walk, of one collection (DB.Scan) or of one statement's
// answer (DB.QueryOpen). Free it with Close, before closing the DB it was
// opened on.
type Scan struct {
	ptr      *C.SekejapScan
	fromScan bool // true: sekejap_scan_open; false: sekejap_query_open
}

// Scan opens a walk of one collection in stable id order. It holds at most
// pageRows rows at a time (0 means sekejap's default of 256).
func (db *DB) Scan(collection string, pageRows uintptr) (*Scan, error) {
	ccol := C.CString(collection)
	defer C.free(unsafe.Pointer(ccol))
	ptr := C.sekejap_scan_open(db.ptr, ccol, C.uintptr_t(pageRows))
	if ptr == nil {
		return nil, lastError()
	}
	s := &Scan{ptr: ptr, fromScan: true}
	runtime.SetFinalizer(s, (*Scan).Close)
	return s, nil
}

// Next returns the next page of documents, decoded into maps. ok is false
// at the end of the walk.
func (s *Scan) Next() ([]map[string]any, bool, error) {
	js, ok, err := s.NextJSON()
	if err != nil || !ok {
		return nil, ok, err
	}
	rows, err := decodeRows(js)
	return rows, err == nil, err
}

// NextJSON is Next, returning the page as a raw JSON array.
func (s *Scan) NextJSON() (js string, ok bool, err error) {
	var out *C.char
	if s.fromScan {
		out = C.sekejap_scan_next(s.ptr)
	} else {
		out = C.sekejap_query_next(s.ptr)
	}
	if out == nil {
		if lastErrorIsOk() {
			return "", false, nil
		}
		return "", false, lastError()
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), true, nil
}

// Close closes the walk and frees it. Safe to call more than once.
func (s *Scan) Close() {
	if s.ptr != nil {
		if s.fromScan {
			C.sekejap_scan_close(s.ptr)
		} else {
			C.sekejap_query_close(s.ptr)
		}
		s.ptr = nil
		runtime.SetFinalizer(s, nil)
	}
}

// ---- shared helpers ---------------------------------------------------

func decodeRows(js string) ([]map[string]any, error) {
	var rows []map[string]any
	if err := json.Unmarshal([]byte(js), &rows); err != nil {
		return nil, err
	}
	return rows, nil
}

// paramsJSON marshals a variadic parameter list into the JSON array the C
// ABI expects, or a nil *C.char for no parameters (NULL means "no
// parameters" on the wire). The caller frees the returned pointer with
// C.free, if it is not nil.
func paramsJSON(params []any) (*C.char, error) {
	if len(params) == 0 {
		return nil, nil
	}
	j, err := json.Marshal(params)
	if err != nil {
		return nil, err
	}
	return C.CString(string(j)), nil
}
