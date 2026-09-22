package sekejap

// #include <stdlib.h>
// #include "sekejap.h"
import "C"

import (
	"runtime"
	"unsafe"
)

// Execute runs one writing statement ($1, $2, ... parameters) and commits.
// Returns the rows it moved; a statement that only raises a notice returns
// 0. A Tier-2/Tier-3 construct sekejap has no atomic for is REFUSED, not
// emulated, and surfaces here as an error naming it.
func (db *DB) Execute(sql string, params ...any) (int64, error) {
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	cparams, err := paramsJSON(params)
	if err != nil {
		return 0, err
	}
	if cparams != nil {
		defer C.free(unsafe.Pointer(cparams))
	}
	return count(C.sekejap_execute(db.ptr, csql, cparams))
}

// Query runs one row-returning statement, decoded into a slice of maps
// keyed by column name. A column missing from a row is omitted from its
// map, because missing is not null.
func (db *DB) Query(sql string, params ...any) ([]map[string]any, error) {
	js, err := db.QueryJSON(sql, params...)
	if err != nil {
		return nil, err
	}
	return decodeRows(js)
}

// QueryJSON is Query, returning the answer as a raw JSON array.
func (db *DB) QueryJSON(sql string, params ...any) (string, error) {
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	cparams, err := paramsJSON(params)
	if err != nil {
		return "", err
	}
	if cparams != nil {
		defer C.free(unsafe.Pointer(cparams))
	}
	out := C.sekejap_query(db.ptr, csql, cparams)
	if out == nil {
		return "", lastError()
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), nil
}

// Explain returns the plan the engine would build for one statement.
func (db *DB) Explain(sql string, params ...any) (string, error) {
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	cparams, err := paramsJSON(params)
	if err != nil {
		return "", err
	}
	if cparams != nil {
		defer C.free(unsafe.Pointer(cparams))
	}
	out := C.sekejap_explain(db.ptr, csql, cparams)
	if out == nil {
		return "", lastError()
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), nil
}

// QueryOpen runs a row-returning statement and opens a PAGED delivery of its
// answer: Scan.Next hands back at most pageRows rows per call (0 means
// 4,096), so no single call holds the whole answer in one string. This
// bounds the string per call and lets a caller stop reading early; it does
// not bound the answer itself, which the engine still executes in one pass
// under Db::stream's own paging.
func (db *DB) QueryOpen(sql string, pageRows uintptr, params ...any) (*Scan, error) {
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	cparams, err := paramsJSON(params)
	if err != nil {
		return nil, err
	}
	if cparams != nil {
		defer C.free(unsafe.Pointer(cparams))
	}
	ptr := C.sekejap_query_open(db.ptr, csql, cparams, C.uintptr_t(pageRows))
	if ptr == nil {
		return nil, lastError()
	}
	s := &Scan{ptr: ptr, fromScan: false}
	runtime.SetFinalizer(s, (*Scan).Close)
	return s, nil
}

// ---- prepared statements ---------------------------------------------

// Stmt is a prepared (compiled) statement. Create with DB.Prepare, run with
// Query/Execute, free with Close -- BEFORE closing the DB it came from.
// Reusable for the same query shape with different parameter values.
type Stmt struct {
	ptr *C.SekejapStmt
}

// Prepare parses sql (with $1, $2, ... placeholders) now -- a syntax error
// is reported here -- and compiles it on the first bind.
func (db *DB) Prepare(sql string) (*Stmt, error) {
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	ptr := C.sekejap_prepare(db.ptr, csql)
	if ptr == nil {
		return nil, lastError()
	}
	s := &Stmt{ptr: ptr}
	runtime.SetFinalizer(s, (*Stmt).Close)
	return s, nil
}

// Query runs the prepared statement as a row-returning one, decoded into a
// slice of maps.
func (s *Stmt) Query(params ...any) ([]map[string]any, error) {
	js, err := s.QueryJSON(params...)
	if err != nil {
		return nil, err
	}
	return decodeRows(js)
}

// QueryJSON is Query, returning the answer as a raw JSON array.
func (s *Stmt) QueryJSON(params ...any) (string, error) {
	cparams, err := paramsJSON(params)
	if err != nil {
		return "", err
	}
	if cparams != nil {
		defer C.free(unsafe.Pointer(cparams))
	}
	out := C.sekejap_stmt_query(s.ptr, cparams)
	if out == nil {
		return "", lastError()
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), nil
}

// Execute runs the prepared statement as a writing one and commits,
// returning the rows it moved.
func (s *Stmt) Execute(params ...any) (int64, error) {
	cparams, err := paramsJSON(params)
	if err != nil {
		return 0, err
	}
	if cparams != nil {
		defer C.free(unsafe.Pointer(cparams))
	}
	return count(C.sekejap_stmt_execute(s.ptr, cparams))
}

// Rebind is Stmt.Rebindable's answer: whether a further bind compiles
// nothing.
type Rebind int32

const (
	RebindNo      Rebind = 0 // a further bind recompiles
	RebindYes     Rebind = 1 // a further bind compiles nothing
	RebindUnbound Rebind = 2 // not bound yet (SEKEJAP_REBIND_UNBOUND) -- not a failure
)

// Rebindable reports whether a further bind of this statement compiles
// nothing. A writing statement is never rebindable -- its document is
// folded at compile -- and says so here rather than pretending otherwise.
func (s *Stmt) Rebindable() (Rebind, error) {
	r := C.sekejap_stmt_rebindable(s.ptr)
	if r == -1 {
		return 0, lastError()
	}
	return Rebind(r), nil
}

// Close frees the prepared statement. Safe to call more than once.
func (s *Stmt) Close() {
	if s.ptr != nil {
		C.sekejap_stmt_free(s.ptr)
		s.ptr = nil
		runtime.SetFinalizer(s, nil)
	}
}
