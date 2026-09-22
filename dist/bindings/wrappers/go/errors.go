package sekejap

// #include "sekejap.h"
import "C"

// Status is sekejap's closed failure code (SekejapStatus on the C side), for
// a caller that wants to branch on the KIND of failure without parsing the
// message. StatusOk is also what a clean miss leaves behind: a NULL from Get
// (or Describe, or a Scan/Stmt at the end of a walk) with StatusOk is "no
// such row", not a failure.
type Status int32

// The nine values of SekejapStatus (dist/ffi/include/sekejap.h).
const (
	StatusOk          Status = 0
	StatusRefused     Status = 1
	StatusCorrupt     Status = 2
	StatusUnsupported Status = 3
	StatusIo          Status = 4
	StatusInvalid     Status = 5
	StatusBusy        Status = 6
	StatusUnknownRow  Status = 7
	StatusUnknown     Status = 8
)

func (s Status) String() string {
	switch s {
	case StatusOk:
		return "ok"
	case StatusRefused:
		return "refused"
	case StatusCorrupt:
		return "corrupt"
	case StatusUnsupported:
		return "unsupported"
	case StatusIo:
		return "io"
	case StatusInvalid:
		return "invalid"
	case StatusBusy:
		return "busy"
	case StatusUnknownRow:
		return "unknown_row"
	case StatusUnknown:
		return "unknown"
	default:
		return "status(?)"
	}
}

// Error is a failure surfaced from the C ABI: the message sekejap gave, plus
// the closed Status a caller can branch on without parsing it.
type Error struct {
	Code    Status
	Message string
}

func (e *Error) Error() string { return "sekejap: " + e.Message }

// lastError reads the calling thread's last-error slot. Every C signature
// that reports one takes a `const SekejapDb *db`, but the header documents
// it as accepted and IGNORED -- the slot is thread-local, which is what lets
// a failed open with no handle still report -- so every caller in this
// package passes nil.
func lastError() error {
	code := Status(C.sekejap_last_error_code(nil))
	msg := C.sekejap_last_error(nil)
	if msg == nil {
		return &Error{Code: code, Message: "unknown error"}
	}
	defer C.sekejap_string_free(msg)
	return &Error{Code: code, Message: C.GoString(msg)}
}

// lastErrorIsOk reports whether the calling thread's last-error slot is
// clear, i.e. a NULL return the caller just saw was a clean miss (Get,
// Describe, the end of a Scan/Stmt walk, NextChange with nothing queued)
// rather than a failure.
func lastErrorIsOk() bool {
	return Status(C.sekejap_last_error_code(nil)) == StatusOk
}

// tribool decodes the `1` / `0` / `-1` sentinel a handful of C functions
// return (Exists, Delete, Unlink, CreateCollection, DropCollection, ...)
// into a Go bool and error.
func tribool(r C.int32_t) (bool, error) {
	switch r {
	case 1:
		return true, nil
	case 0:
		return false, nil
	default:
		return false, lastError()
	}
}

// count decodes the "rows, or -1" sentinel a handful of C functions return
// (Execute, PutMany, CountRows, ScanCountRows, ScanCountEdges, Subscribe,
// Tx.Execute) into a Go int64 and error.
func count(r C.long) (int64, error) {
	if r < 0 {
		return 0, lastError()
	}
	return int64(r), nil
}
