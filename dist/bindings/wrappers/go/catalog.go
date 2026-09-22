package sekejap

// #include <stdlib.h>
// #include "sekejap.h"
import "C"

import (
	"encoding/json"
	"unsafe"
)

// Field declares one column: Kind is "text", "int", "real", "bool", "json",
// "geo", "point" or "vector"; Dimension is required for "vector" and
// rejected for every other kind.
type Field struct {
	Name      string `json:"name"`
	Kind      string `json:"kind"`
	Dimension *int   `json:"dimension,omitempty"`
}

// CreateCollection declares a collection. The declaration is a floor, not a
// fence: a document may carry a field it does not name, stored in the row's
// extras. Reports whether the collection was newly created (false: it was
// already there).
func (db *DB) CreateCollection(name string, fields []Field) (bool, error) {
	j, err := json.Marshal(fields)
	if err != nil {
		return false, err
	}
	return db.CreateCollectionJSON(name, string(j))
}

// CreateCollectionJSON is CreateCollection, taking the fields already
// encoded as a JSON array of {"name", "kind", "dimension"?}.
func (db *DB) CreateCollectionJSON(name, fieldsJSON string) (bool, error) {
	cname, cfields := C.CString(name), C.CString(fieldsJSON)
	defer C.free(unsafe.Pointer(cname))
	defer C.free(unsafe.Pointer(cfields))
	return tribool(C.sekejap_create_collection(db.ptr, cname, cfields))
}

// DropCollection removes a collection, its rows, its indexes and its
// descriptor. Reports whether it was there.
func (db *DB) DropCollection(name string) (bool, error) {
	cname := C.CString(name)
	defer C.free(unsafe.Pointer(cname))
	return tribool(C.sekejap_drop_collection(db.ptr, cname))
}

// Collections returns every collection name in the catalog, in key order.
func (db *DB) Collections() ([]string, error) {
	out := C.sekejap_collections(db.ptr)
	if out == nil {
		return nil, lastError()
	}
	defer C.sekejap_string_free(out)
	var names []string
	if err := json.Unmarshal([]byte(C.GoString(out)), &names); err != nil {
		return nil, err
	}
	return names, nil
}

// Describe returns the declared shape of one collection, decoded into a
// map: "name", "timestamps", "rows" (the live row count, or nil where this
// database keeps no record -- nil is "no record", not "no rows"), "fields"
// and "indexes". ok is false when there is no such collection.
func (db *DB) Describe(collection string) (map[string]any, bool, error) {
	js, ok, err := db.DescribeJSON(collection)
	if err != nil || !ok {
		return nil, ok, err
	}
	var out map[string]any
	if err := json.Unmarshal([]byte(js), &out); err != nil {
		return nil, false, err
	}
	return out, true, nil
}

// DescribeJSON is Describe, returning the shape as raw JSON text.
func (db *DB) DescribeJSON(collection string) (js string, ok bool, err error) {
	ccol := C.CString(collection)
	defer C.free(unsafe.Pointer(ccol))
	out := C.sekejap_describe(db.ptr, ccol)
	if out == nil {
		if lastErrorIsOk() {
			return "", false, nil
		}
		return "", false, lastError()
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), true, nil
}

// CountRows returns the rows of one collection: the LIVE record when this
// database keeps one, and a walk when it does not.
func (db *DB) CountRows(collection string) (int64, error) {
	ccol := C.CString(collection)
	defer C.free(unsafe.Pointer(ccol))
	return count(C.sekejap_count_rows(db.ptr, ccol))
}

// ScanCountRows counts the rows of one collection BY WALKING them, whether
// or not a live record exists. The explicit walk, named as one.
func (db *DB) ScanCountRows(collection string) (int64, error) {
	ccol := C.CString(collection)
	defer C.free(unsafe.Pointer(ccol))
	return count(C.sekejap_scan_count_rows(db.ptr, ccol))
}

// ScanCountEdges counts every edge BY WALKING the primary edge keyspace:
// sekejap keeps no O(1) edge counter, so this is a scan and is named as one.
func (db *DB) ScanCountEdges() (int64, error) {
	return count(C.sekejap_scan_count_edges(db.ptr))
}
