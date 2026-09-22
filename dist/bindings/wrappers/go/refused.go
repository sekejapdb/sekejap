// The four calls in this file are REFUSED BY NAME (docs/dist/C_ABI.md
// §4.10): sekejap has no atomic under any of them, so each always fails
// with StatusRefused and a reason, rather than emulating one behind a
// symbol that would otherwise quietly give a wrong answer. They are wrapped
// here anyway, one to one with the C ABI, so a caller finds the SAME name
// they would reach for and gets a clear refusal instead of a missing
// symbol.
package sekejap

// #include <stdlib.h>
// #include "sekejap.h"
import "C"

import "unsafe"

// OpenMemory always fails: sekejap is disk-first and has no in-memory
// database (a temporary directory would be a fake of an ephemeral store).
// Give Open a directory instead.
func OpenMemory() (*DB, error) {
	ptr := C.sekejap_open_memory()
	if ptr == nil {
		return nil, lastError()
	}
	return newDB(ptr), nil
}

// TrimMemory always fails: sekejap keeps nothing proportional to rows in
// memory to trim -- the buffer pool is bounded by Config.BudgetBytes and
// the plan cache by its own ceilings -- so a no-op success would be a fake
// of reclaim.
func (db *DB) TrimMemory() error {
	if C.sekejap_trim_memory(db.ptr) != 0 {
		return lastError()
	}
	return nil
}

// Compact always fails: there is no payload-rewriting compaction anymore.
// Checkpoint folds the committed write-ahead log into the data file; it
// does not rewrite rows, and this symbol is kept, refusing by name, so a
// caller who reaches for the old operation is told why rather than getting
// a missing method.
func (db *DB) Compact() error {
	if C.sekejap_compact(db.ptr) != 0 {
		return lastError()
	}
	return nil
}

// Show always fails: the SHOW family has no Tier-1 spelling. Collections
// and Describe answer the same questions as data.
func (db *DB) Show(statement string) (string, error) {
	var cstmt *C.char
	if statement != "" {
		cstmt = C.CString(statement)
		defer C.free(unsafe.Pointer(cstmt))
	}
	out := C.sekejap_show(db.ptr, cstmt)
	if out == nil {
		return "", lastError()
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), nil
}
