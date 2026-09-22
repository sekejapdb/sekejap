// Service-mode calls: StatementTimeout, Cancel, ClearInterrupt, Subscribe,
// NextChange and Unsubscribe answer only on a handle opened with
// OpenService (docs/dist/OPS_CONTRACT.md §1-§5). On any other handle they
// are REFUSED BY NAME -- StatusRefused, naming the call and saying single
// mode has no writer to time out, no interrupt and no change feed.
package sekejap

// #include <stdlib.h>
// #include "sekejap.h"
import "C"

import "encoding/json"

// StatementTimeout refuses a statement that runs longer than the given
// duration. Zero milliseconds CLEARS the timeout.
func (db *DB) StatementTimeout(milliseconds uint64) error {
	if C.sekejap_statement_timeout_ms(db.ptr, C.uint64_t(milliseconds)) != 0 {
		return lastError()
	}
	return nil
}

// Cancel cancels the work in flight on this service, from any thread. The
// flag is STICKY until ClearInterrupt.
func (db *DB) Cancel() error {
	if C.sekejap_cancel(db.ptr) != 0 {
		return lastError()
	}
	return nil
}

// ClearInterrupt clears a cancel so the service accepts work again. Reports
// whether a cancel was standing.
func (db *DB) ClearInterrupt() (bool, error) {
	return tribool(C.sekejap_clear_interrupt(db.ptr))
}

// Subscribe subscribes to the commit-time change feed, returning a
// subscription id for NextChange and Unsubscribe. The subscription is
// owned by the DB, so the id is usable from any goroutine, and a
// subscription left open is closed by DB.Close.
func (db *DB) Subscribe() (int64, error) {
	return count(C.sekejap_subscribe(db.ptr))
}

// Change is one commit-time event, as sekejap_next_change reports it.
type Change struct {
	Sequence      uint64      `json:"sequence"`
	Collections   []string    `json:"collections"`
	EdgeTypes     []string    `json:"edge_types"`
	Keys          []ChangeKey `json:"keys"`
	KeysTotal     int64       `json:"keys_total"`
	KeysTruncated bool        `json:"keys_truncated"`
	UnnamedWrites int64       `json:"unnamed_writes"`
	RowsAffected  int64       `json:"rows_affected"`
}

// ChangeKey is one row a Change moved.
type ChangeKey struct {
	Collection string `json:"collection"`
	Key        string `json:"key"`
	Kind       string `json:"kind"` // "put" or "delete"
}

// NextChange returns the next event for one subscription, or ok=false when
// none arrived within timeoutMs (0 polls and returns at once; a positive
// value waits that long). Keys is empty and KeysTruncated is true when the
// batch moved more keys than the feed's per-event cap: the list is dropped
// whole rather than handed over half-true, and Collections is still exact.
func (db *DB) NextChange(subscription int64, timeoutMs uint64) (*Change, bool, error) {
	js, ok, err := db.NextChangeJSON(subscription, timeoutMs)
	if err != nil || !ok {
		return nil, ok, err
	}
	var c Change
	if err := json.Unmarshal([]byte(js), &c); err != nil {
		return nil, false, err
	}
	return &c, true, nil
}

// NextChangeJSON is NextChange, returning the event as raw JSON text.
func (db *DB) NextChangeJSON(subscription int64, timeoutMs uint64) (js string, ok bool, err error) {
	out := C.sekejap_next_change(db.ptr, C.long(subscription), C.uint64_t(timeoutMs))
	if out == nil {
		if lastErrorIsOk() {
			return "", false, nil
		}
		return "", false, lastError()
	}
	defer C.sekejap_string_free(out)
	return C.GoString(out), true, nil
}

// Unsubscribe closes one subscription. Reports whether it was open.
func (db *DB) Unsubscribe(subscription int64) (bool, error) {
	return tribool(C.sekejap_unsubscribe(db.ptr, C.long(subscription)))
}
