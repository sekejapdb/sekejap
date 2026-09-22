package sekejap

// #include <stdlib.h>
// #include "sekejap.h"
import "C"

import (
	"encoding/json"
	"unsafe"
)

// Direction is which way an edge points, for DB.Neighbours.
type Direction int32

const (
	DirectionOutgoing Direction = 0 // edges that leave the row
	DirectionIncoming Direction = 1 // edges that arrive at the row
	DirectionBoth     Direction = 2 // both, with each neighbour reported once
)

// Link creates a plain edge from -> to of the given type in the base graph
// context, committed. Both endpoints must already exist: a missing one is
// an error (StatusUnknownRow), never a dangling identity.
func (db *DB) Link(fromCollection, fromKey, edgeType, toCollection, toKey string) error {
	cfc, cfk, ce, ctc, ctk := C.CString(fromCollection), C.CString(fromKey), C.CString(edgeType), C.CString(toCollection), C.CString(toKey)
	defer C.free(unsafe.Pointer(cfc))
	defer C.free(unsafe.Pointer(cfk))
	defer C.free(unsafe.Pointer(ce))
	defer C.free(unsafe.Pointer(ctc))
	defer C.free(unsafe.Pointer(ctk))
	if C.sekejap_link(db.ptr, cfc, cfk, ce, ctc, ctk) != 0 {
		return lastError()
	}
	return nil
}

// LinkWith is Link, carrying a properties value marshaled to a JSON object.
func (db *DB) LinkWith(fromCollection, fromKey, edgeType, toCollection, toKey string, properties any) error {
	j, err := json.Marshal(properties)
	if err != nil {
		return err
	}
	return db.LinkWithJSON(fromCollection, fromKey, edgeType, toCollection, toKey, string(j))
}

// LinkWithJSON is LinkWith, taking the properties already encoded as a JSON
// object.
func (db *DB) LinkWithJSON(fromCollection, fromKey, edgeType, toCollection, toKey, propertiesJSON string) error {
	cfc, cfk, ce, ctc, ctk, cp := C.CString(fromCollection), C.CString(fromKey), C.CString(edgeType), C.CString(toCollection), C.CString(toKey), C.CString(propertiesJSON)
	defer C.free(unsafe.Pointer(cfc))
	defer C.free(unsafe.Pointer(cfk))
	defer C.free(unsafe.Pointer(ce))
	defer C.free(unsafe.Pointer(ctc))
	defer C.free(unsafe.Pointer(ctk))
	defer C.free(unsafe.Pointer(cp))
	if C.sekejap_link_with(db.ptr, cfc, cfk, ce, ctc, ctk, cp) != 0 {
		return lastError()
	}
	return nil
}

// Unlink removes one edge, committed. Reports whether it was there.
func (db *DB) Unlink(fromCollection, fromKey, edgeType, toCollection, toKey string) (bool, error) {
	cfc, cfk, ce, ctc, ctk := C.CString(fromCollection), C.CString(fromKey), C.CString(edgeType), C.CString(toCollection), C.CString(toKey)
	defer C.free(unsafe.Pointer(cfc))
	defer C.free(unsafe.Pointer(cfk))
	defer C.free(unsafe.Pointer(ce))
	defer C.free(unsafe.Pointer(ctc))
	defer C.free(unsafe.Pointer(ctk))
	return tribool(C.sekejap_unlink(db.ptr, cfc, cfk, ce, ctc, ctk))
}

// Neighbour is one row sekejap_neighbours reports: a collection is named
// alongside the key because a neighbour can be in another collection than
// the row it is one hop from.
type Neighbour struct {
	Collection string         `json:"collection"`
	Key        string         `json:"key"`
	Document   map[string]any `json:"document"`
}

// Neighbours returns the rows one hop away from collection/key, in one
// direction, under a complete-or-error bound of at most 256 edges: a wider
// walk is SQL's GRAPH_TABLE, not a second spelling here. edgeType may be ""
// for every type.
func (db *DB) Neighbours(collection, key, edgeType string, direction Direction, limit uintptr) ([]Neighbour, error) {
	js, err := db.NeighboursJSON(collection, key, edgeType, direction, limit)
	if err != nil {
		return nil, err
	}
	var out []Neighbour
	if err := json.Unmarshal([]byte(js), &out); err != nil {
		return nil, err
	}
	return out, nil
}

// NeighboursJSON is Neighbours, returning the answer as a raw JSON array.
func (db *DB) NeighboursJSON(collection, key, edgeType string, direction Direction, limit uintptr) (string, error) {
	ccol, ckey := C.CString(collection), C.CString(key)
	defer C.free(unsafe.Pointer(ccol))
	defer C.free(unsafe.Pointer(ckey))
	var cedge *C.char
	if edgeType != "" {
		cedge = C.CString(edgeType)
		defer C.free(unsafe.Pointer(cedge))
	}
	out := C.sekejap_neighbours(db.ptr, ccol, ckey, cedge, C.SekejapDirection(direction), C.uintptr_t(limit))
	if out == nil {
		return "", lastError()
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), nil
}
