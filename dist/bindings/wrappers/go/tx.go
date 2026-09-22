package sekejap

// #include <stdlib.h>
// #include "sekejap.h"
import "C"

import (
	"encoding/json"
	"runtime"
	"unsafe"
)

// Tx is the writer, held across many writes under ONE commit barrier: every
// plain DB call commits per call, and Tx is the other bargain. While a Tx
// is open it HOLDS the writer -- a call on the same DB that needs the
// writer waits for it. Commit or Rollback before using the DB for anything
// else that needs the writer, and free this handle BEFORE closing the DB.
// A Tx dropped any other way (garbage collected without Commit/Rollback)
// ROLLS BACK.
type Tx struct {
	ptr *C.SekejapTx
}

// Begin takes the writer for many writes under one barrier.
func (db *DB) Begin() (*Tx, error) {
	ptr := C.sekejap_tx_begin(db.ptr)
	if ptr == nil {
		return nil, lastError()
	}
	tx := &Tx{ptr: ptr}
	runtime.SetFinalizer(tx, (*Tx).Rollback)
	return tx, nil
}

// Put writes one document inside the transaction, with NO commit.
func (tx *Tx) Put(collection, key string, doc any) error {
	j, err := json.Marshal(doc)
	if err != nil {
		return err
	}
	return tx.PutJSON(collection, key, string(j))
}

// PutJSON is Put, taking the document as a JSON object already encoded.
func (tx *Tx) PutJSON(collection, key, documentJSON string) error {
	ccol, ckey, cdoc := C.CString(collection), C.CString(key), C.CString(documentJSON)
	defer C.free(unsafe.Pointer(ccol))
	defer C.free(unsafe.Pointer(ckey))
	defer C.free(unsafe.Pointer(cdoc))
	if C.sekejap_tx_put(tx.ptr, ccol, ckey, cdoc) != 0 {
		return lastError()
	}
	return nil
}

// Delete removes one row inside the transaction, with NO commit. Reports
// whether it was there.
func (tx *Tx) Delete(collection, key string) (bool, error) {
	ccol, ckey := C.CString(collection), C.CString(key)
	defer C.free(unsafe.Pointer(ccol))
	defer C.free(unsafe.Pointer(ckey))
	return tribool(C.sekejap_tx_delete(tx.ptr, ccol, ckey))
}

// Link creates an edge inside the transaction, with NO commit.
func (tx *Tx) Link(fromCollection, fromKey, edgeType, toCollection, toKey string) error {
	cfc, cfk, ce, ctc, ctk := C.CString(fromCollection), C.CString(fromKey), C.CString(edgeType), C.CString(toCollection), C.CString(toKey)
	defer C.free(unsafe.Pointer(cfc))
	defer C.free(unsafe.Pointer(cfk))
	defer C.free(unsafe.Pointer(ce))
	defer C.free(unsafe.Pointer(ctc))
	defer C.free(unsafe.Pointer(ctk))
	if C.sekejap_tx_link(tx.ptr, cfc, cfk, ce, ctc, ctk) != 0 {
		return lastError()
	}
	return nil
}

// Execute runs one writing statement inside the transaction, with NO
// commit, returning the rows it moved.
func (tx *Tx) Execute(sql string, params ...any) (int64, error) {
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	cparams, err := paramsJSON(params)
	if err != nil {
		return 0, err
	}
	if cparams != nil {
		defer C.free(unsafe.Pointer(cparams))
	}
	return count(C.sekejap_tx_execute(tx.ptr, csql, cparams))
}

// Commit commits the transaction and FREES the handle, whether the commit
// succeeded or not: tx is unusable after this call either way.
func (tx *Tx) Commit() error {
	if tx.ptr == nil {
		return nil
	}
	runtime.SetFinalizer(tx, nil)
	r := C.sekejap_tx_commit(tx.ptr)
	tx.ptr = nil
	if r != 0 {
		return lastError()
	}
	return nil
}

// Rollback rolls the transaction back and FREES the handle. Safe to call
// more than once (a second call is a no-op).
func (tx *Tx) Rollback() error {
	if tx.ptr == nil {
		return nil
	}
	runtime.SetFinalizer(tx, nil)
	r := C.sekejap_tx_rollback(tx.ptr)
	tx.ptr = nil
	if r != 0 {
		return lastError()
	}
	return nil
}
