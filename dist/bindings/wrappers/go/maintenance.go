package sekejap

// #include "sekejap.h"
import "C"

import "encoding/json"

// Checkpoint folds the committed write-ahead log into the data file.
// folded is false when a live reader holds a slot and the fold is DEFERRED
// -- in service mode that is every call, because the published read view
// holds a slot for its whole life -- which is not a failure.
func (db *DB) Checkpoint() (folded bool, err error) {
	switch C.sekejap_checkpoint(db.ptr) {
	case 1:
		return true, nil
	case 0:
		return false, nil
	default:
		return false, lastError()
	}
}

// Publish makes the newest commit visible to readers now. In single mode
// there is no published view to swap and every commit is already visible
// to this handle, so this succeeds having done nothing.
func (db *DB) Publish() error {
	if C.sekejap_publish(db.ptr) != 0 {
		return lastError()
	}
	return nil
}

// Storage is the bytes sekejap.Storage reports.
type Storage struct {
	DataBytes  int64 `json:"data_bytes"`
	WalBytes   int64 `json:"wal_bytes"`
	TotalBytes int64 `json:"total_bytes"`
}

// Storage returns the bytes this database occupies on disk.
func (db *DB) Storage() (*Storage, error) {
	js, err := db.StorageJSON()
	if err != nil {
		return nil, err
	}
	var s Storage
	if err := json.Unmarshal([]byte(js), &s); err != nil {
		return nil, err
	}
	return &s, nil
}

// StorageJSON is Storage, returning the answer as raw JSON text.
func (db *DB) StorageJSON() (string, error) {
	out := C.sekejap_storage(db.ptr)
	if out == nil {
		return "", lastError()
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), nil
}
