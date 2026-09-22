/*
 * sekejap.h -- the C ABI of sekejap (https://sekejap.life)
 *
 * AUTO-GENERATED from dist/ffi/src/lib.rs by cbindgen. Do not edit by hand.
 * The contract this header carries is docs/dist/C_ABI.md.
 *
 * One extern "C" surface over the published `sekejap` crate
 * (docs/dist/RUST_API.md), for binding from Swift, Kotlin/JNI, Dart
 * (dart:ffi), Go (cgo) and plain C/C++.
 *
 * Ownership: SekejapDb*, SekejapStmt*, SekejapTx* and SekejapScan* are opaque
 * (an open/begin call creates one, the matching close/free call destroys it).
 * Any char* the library RETURNS is yours -- free it once with
 * sekejap_string_free, except sekejap_version, which points into static
 * program data. Strings you PASS IN are borrowed UTF-8 and are never freed
 * here.
 *
 * Errors: NULL for a failed pointer return, -1 for a failed integer return.
 * sekejap_last_error(db) is the message and sekejap_last_error_code(db) is
 * the closed enum a wrapper maps without parsing text; both read a
 * THREAD-LOCAL slot, so a failed open with no handle still reports.
 * No Rust panic crosses the boundary.
 *
 * Documents, parameters and rows are JSON text: a document is a JSON object,
 * a parameter list is a JSON array, and an answer is a JSON array of objects
 * keyed by column name.
 */


#ifndef SEKEJAP_H
#define SEKEJAP_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

// `sekejap_stmt_rebindable`: the statement has not been bound yet, so
// there is nothing to answer. It is compiled by its first
// `sekejap_stmt_query` or `sekejap_stmt_execute`, and rebindable is
// known from then on. Not a failure: `-1` is.
#define SEKEJAP_REBIND_UNBOUND 2

// Which way an edge points, for `sekejap_neighbours`.
enum SekejapDirection
#ifdef __cplusplus
  : int32_t
#endif // __cplusplus
 {
    // Edges that leave the row.
    SekejapDirection_Outgoing = 0,
    // Edges that arrive at the row.
    SekejapDirection_Incoming = 1,
    // Both, with each neighbour reported once.
    SekejapDirection_Both = 2,
};
#ifndef __cplusplus
typedef int32_t SekejapDirection;
#endif // __cplusplus

// Why the last call on this thread failed, as a small closed enumeration a
// wrapper can map without parsing the message text.
//
// `SekejapStatus_Ok` is also what a clean MISS leaves behind: a `NULL`
// from `sekejap_get` with `Ok` is "no such row", and a `NULL` with
// anything else is a failure whose sentence `sekejap_last_error` carries.
enum SekejapStatus
#ifdef __cplusplus
  : int32_t
#endif // __cplusplus
 {
    // The last call succeeded, or answered a clean miss.
    SekejapStatus_Ok = 0,
    // A construct sekejap has no atomic for, named with its reason: a
    // Tier-2/Tier-3 statement, an in-memory open, a payload-rewriting
    // compact, a service call on a single-mode handle.
    SekejapStatus_Refused = 1,
    // A page or a log failed verification. Nothing was changed.
    SekejapStatus_Corrupt = 2,
    // A format, policy or configuration this build does not implement.
    SekejapStatus_Unsupported = 3,
    // The directory, the file or the medium refused.
    SekejapStatus_Io = 4,
    // The caller's arguments are wrong: a null pointer, text that is not
    // UTF-8, JSON that does not parse, a collection that is not in the
    // catalog, a parameter of the wrong type.
    SekejapStatus_Invalid = 5,
    // A bound refused rather than waiting: a work budget, a statement
    // deadline, a cancel, a second writer, a reader slot.
    SekejapStatus_Busy = 6,
    // The named row is not in the collection, on a call that needs it to
    // exist -- an edge endpoint, for instance.
    SekejapStatus_UnknownRow = 7,
    // Nothing above classified it, including a panic caught at the
    // boundary. The message is still in `sekejap_last_error`.
    SekejapStatus_Unknown = 8,
};
#ifndef __cplusplus
typedef int32_t SekejapStatus;
#endif // __cplusplus

// An open database. Created by `sekejap_open`,
// `sekejap_open_with_config` or `sekejap_open_service`; destroyed by
// `sekejap_close`. `Send + Sync`: it MAY be shared across threads.
typedef struct SekejapDb SekejapDb;

// A paged walk: of one collection (`sekejap_scan_open`) or of one
// statement's answer (`sekejap_query_open`). Freed with
// `sekejap_scan_close`, which `sekejap_query_close` is another name
// for.
typedef struct SekejapScan SekejapScan;

// One statement, parsed once at `sekejap_prepare` and compiled by its
// first bind. Freed with `sekejap_stmt_free`, which must happen BEFORE
// `sekejap_close` of the database it was prepared on.
typedef struct SekejapStmt SekejapStmt;

// The writer, held across many writes. Created by `sekejap_tx_begin` and
// consumed by `sekejap_tx_commit` or `sekejap_tx_rollback`; a handle
// dropped any other way ROLLS BACK.
typedef struct SekejapTx SekejapTx;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

// Open the database in `path`, creating it when the directory holds none.
// `NULL` on failure, with `sekejap_last_error` set on this thread.
//
// # Safety
// `path` must be a valid null-terminated UTF-8 C string.
SekejapDb *sekejap_open(const char *path);

// `sekejap_open` under a store configuration, given as a JSON object:
// `{"budget_bytes": 268435456, "io": "buffered"|"direct",
// "sync": "full"|"normal"|"off"}`. Every member is optional and an absent
// one keeps sekejap's own default.
//
// The default `sync` is `"normal"`: every acknowledged commit is pushed out
// of the page cache with a data barrier (`fdatasync` on Linux, plain
// `fsync` on macOS), so it survives a process crash and an operating-system
// panic. It does NOT flush the drive's own write cache, so a power cut can
// lose recently acknowledged commits. This is the guarantee SQLite gives
// with `fullfsync` off and PostgreSQL with a plain `fsync`.
//
// `"sync": "full"` asks for the drive-cache barrier instead
// (`fcntl(F_FULLFSYNC)` on macOS, `fsync` elsewhere), which survives a power
// cut as well and costs 11.9 ms against 1.45 ms for `"normal"` on the
// volume the write-path measurements were taken on. `"sync": "off"` issues
// no barrier at all.
//
// # Safety
// Both pointers must be valid null-terminated UTF-8 C strings, or
// `config_json` may be NULL for the defaults.
SekejapDb *sekejap_open_with_config(const char *path, const char *config_json);

// Open in SERVICE mode: one writer, parallel readers on a published
// snapshot, and the change feed, the statement timeout and the cancel of
// `docs/dist/OPS_CONTRACT.md` §1-§5. The `sekejap_subscribe`,
// `sekejap_cancel` and `sekejap_statement_timeout_ms` family answers only
// on a handle opened this way.
//
// # Safety
// `path` must be a valid null-terminated UTF-8 C string.
SekejapDb *sekejap_open_service(const char *path);

// Close the handle and free it. Null-safe. Uncommitted work is discarded:
// a close is not a commit. Every `SekejapStmt*`, `SekejapScan*` and
// `SekejapTx*` taken from this handle must be freed FIRST.
//
// # Safety
// `db` must be null or a handle from an open call that has not been closed.
void sekejap_close(SekejapDb *db);

// The library version, as `MAJOR.MINOR.PATCH`. STATIC: do not pass it to
// `sekejap_string_free`.
const char *sekejap_version(void);

// The sekejap disk format this build reads and writes: 2. A file that
// carries anything else is refused by name with nothing changed
// (`docs/core/FORMAT_V2.md`).
int32_t sekejap_format_version(void);

// The message for the last failure ON THIS THREAD, as a heap C string the
// caller frees with `sekejap_string_free`. `NULL` when the last call
// succeeded. The handle is accepted and ignored: the slot is thread-local,
// so a failed open with no handle still reports.
//
// # Safety
// `db` may be NULL; when it is not, it must be a live handle.
char *sekejap_last_error(const SekejapDb *db);

// The code for the last failure ON THIS THREAD, for a wrapper that maps
// without parsing the message. `SekejapStatus_Ok` after a success and
// after a clean miss.
//
// # Safety
// `db` may be NULL; when it is not, it must be a live handle.
SekejapStatus sekejap_last_error_code(const SekejapDb *db);

// Free a string this library returned. Null-safe, and exactly once.
// NEVER pass `sekejap_version`, which is static.
//
// # Safety
// `s` must be null or a pointer this library returned and that has not been
// freed.
void sekejap_string_free(char *s);

// Write one document, committed before this call returns. `0` on success,
// `-1` on failure. `document_json` must be a JSON object; a `_key` member
// in it must equal `key`.
//
// # Safety
// Every pointer must be a valid null-terminated UTF-8 C string and `db` a
// live handle.
int32_t sekejap_put(SekejapDb *db,
                    const char *collection,
                    const char *key,
                    const char *document_json);

// Write many documents into one collection under ONE commit. `rows_json`
// is a JSON array of `{"key": "...", "doc": { ... }}`. Returns the rows
// written, or `-1`; a failure stores NONE of the batch.
//
// # Safety
// As `sekejap_put`.
long sekejap_put_many(SekejapDb *db, const char *collection, const char *rows_json);

// Read one document, with `_key` set, as a heap JSON object the caller
// frees. `NULL` with `SekejapStatus_Ok` is a MISS; `NULL` with anything
// else is a failure.
//
// # Safety
// As `sekejap_put`.
char *sekejap_get(SekejapDb *db, const char *collection, const char *key);

// Whether the row is there: `1`, `0`, or `-1` on failure.
//
// # Safety
// As `sekejap_put`.
int32_t sekejap_exists(SekejapDb *db, const char *collection, const char *key);

// Delete one row and every edge that touches it, committed. `1` if it was
// there, `0` if it was not, `-1` on failure.
//
// # Safety
// As `sekejap_put`.
int32_t sekejap_delete(SekejapDb *db, const char *collection, const char *key);

// Open a walk of one collection in stable id order. The walk holds at most
// `page_rows` rows at a time (`0` means sekejap's default of 256), and
// takes the read lock once per page rather than once for the collection.
// `NULL` on failure. Free with `sekejap_scan_close`, BEFORE
// `sekejap_close`.
//
// # Safety
// `db` must be a live handle and `collection` a valid C string. The
// returned handle borrows `db`.
SekejapScan *sekejap_scan_open(SekejapDb *db, const char *collection, uintptr_t page_rows);

// The next page of a walk, as a heap JSON array of documents (each with
// `_key`). `NULL` with `SekejapStatus_Ok` is the END of the walk;
// `NULL` with anything else is a failure. Free the string with
// `sekejap_string_free`.
//
// # Safety
// `scan` must be a live handle from `sekejap_scan_open` or
// `sekejap_query_open`, used from one thread at a time.
char *sekejap_scan_next(SekejapScan *scan);

// Close a walk and free it. Null-safe.
//
// # Safety
// `scan` must be null or a live handle that has not been closed.
void sekejap_scan_close(SekejapScan *scan);

// Run one writing statement and commit. Returns the rows it moved; a
// statement that only raises a notice returns `0`. `-1` on failure, which
// includes a Tier-2/Tier-3 construct REFUSED by name.
//
// # Safety
// `db` must be a live handle, `sql` a valid C string, `params_json` NULL or
// a JSON array.
long sekejap_execute(SekejapDb *db, const char *sql, const char *params_json);

// Run one row-returning statement. The answer is a heap JSON ARRAY of
// objects keyed by COLUMN NAME; a column that is MISSING in a row is
// omitted from that object, because missing is not null. `NULL` on
// failure. Free with `sekejap_string_free`.
//
// # Safety
// As `sekejap_execute`.
char *sekejap_query(SekejapDb *db, const char *sql, const char *params_json);

// The plan the engine would build for one statement, as heap text. `NULL`
// on failure.
//
// # Safety
// As `sekejap_execute`.
char *sekejap_explain(SekejapDb *db, const char *sql, const char *params_json);

// Prepare one statement. It is PARSED here -- a syntax error is reported
// now -- and compiled by its first bind. `NULL` on failure. Free with
// `sekejap_stmt_free`, BEFORE `sekejap_close`.
//
// # Safety
// `db` must be a live handle. The returned handle borrows it.
SekejapStmt *sekejap_prepare(SekejapDb *db, const char *sql);

// Run a prepared statement as a row-returning one. Same JSON shape as
// `sekejap_query`. `NULL` on failure.
//
// # Safety
// `stmt` must be a live handle from `sekejap_prepare` whose database is
// still open, used from one thread at a time.
char *sekejap_stmt_query(SekejapStmt *stmt, const char *params_json);

// Run a prepared statement as a writing one and commit. Returns the rows it
// moved, or `-1`.
//
// # Safety
// As `sekejap_stmt_query`.
long sekejap_stmt_execute(SekejapStmt *stmt, const char *params_json);

// Whether a further bind of this statement compiles nothing: `1` yes,
// `0` no, `SEKEJAP_REBIND_UNBOUND` (`2`) not bound yet, `-1` on failure.
//
// A writing statement is never rebindable -- its document is folded at
// compile -- and says so here rather than pretending.
//
// # Safety
// As `sekejap_stmt_query`.
int32_t sekejap_stmt_rebindable(const SekejapStmt *stmt);

// Free a prepared statement. Null-safe.
//
// # Safety
// `stmt` must be null or a live handle that has not been freed.
void sekejap_stmt_free(SekejapStmt *stmt);

// Run a row-returning statement and open a PAGED DELIVERY of its answer:
// `sekejap_query_next` hands back at most `page_rows` rows per call
// (`0` means 4,096), so no single `char*` holds the whole answer.
//
// What is paged and what is not, stated rather than implied: the ENGINE
// pages the execution at `page_rows` rows, which is `Db::stream`; the
// ANSWER is assembled here, because a compiled SELECT owns what its request
// borrows and therefore cannot be suspended between two C calls
// (`docs/dist/RUST_API.md` §3). This bounds the string per call and lets a
// caller stop reading; it does not bound the answer.
//
// `NULL` on failure. Free with `sekejap_query_close`.
//
// # Safety
// As `sekejap_execute`. The returned handle borrows `db`.
SekejapScan *sekejap_query_open(SekejapDb *db,
                                const char *sql,
                                const char *params_json,
                                uintptr_t page_rows);

// The next page of a statement's answer: a heap JSON array of objects
// keyed by column name, or `NULL` with `SekejapStatus_Ok` at the end.
// The same operation as `sekejap_scan_next`, under the name that matches
// `sekejap_query_open`.
//
// # Safety
// As `sekejap_scan_next`.
char *sekejap_query_next(SekejapScan *scan);

// Close a paged answer and free it. Null-safe. The same operation as
// `sekejap_scan_close`.
//
// # Safety
// As `sekejap_scan_close`.
void sekejap_query_close(SekejapScan *scan);

// Link two rows with a typed edge in the base graph context, committed.
// `0` on success, `-1` on failure. BOTH endpoints must already exist: a
// missing one is an error, never a dangling identity.
//
// # Safety
// Every pointer must be a valid null-terminated UTF-8 C string and `db` a
// live handle.
int32_t sekejap_link(SekejapDb *db,
                     const char *from_collection,
                     const char *from_key,
                     const char *edge_type,
                     const char *to_collection,
                     const char *to_key);

// `sekejap_link` carrying a JSON properties object.
//
// # Safety
// As `sekejap_link`, with `properties_json` a JSON object.
int32_t sekejap_link_with(SekejapDb *db,
                          const char *from_collection,
                          const char *from_key,
                          const char *edge_type,
                          const char *to_collection,
                          const char *to_key,
                          const char *properties_json);

// Remove one edge, committed. `1` if it was there, `0` if it was not, `-1`
// on failure.
//
// # Safety
// As `sekejap_link`.
int32_t sekejap_unlink(SekejapDb *db,
                       const char *from_collection,
                       const char *from_key,
                       const char *edge_type,
                       const char *to_collection,
                       const char *to_key);

// The rows one hop away, in one direction, under a complete-or-error bound
// of at most 256 edges: a wider walk is `GRAPH_TABLE` in SQL and is
// REFUSED here by name. `edge_type` may be NULL for every type.
//
// The answer is a heap JSON array of
// `{"collection": "...", "key": "...", "document": { ... }}`, because a
// neighbour can be in another collection and its name is part of the
// answer. `NULL` on failure.
//
// # Safety
// As `sekejap_link`; `edge_type` may be NULL.
char *sekejap_neighbours(SekejapDb *db,
                         const char *collection,
                         const char *key,
                         const char *edge_type,
                         SekejapDirection direction,
                         uintptr_t limit);

// Declare a collection. `fields_json` is a JSON array of
// `{"name": "...", "kind": "text"|"int"|"real"|"bool"|"json"|"geo"|
// "point"|"vector", "dimension": n}`, where `dimension` is required for
// `vector` and rejected for every other kind. `1` if it was created, `0` if
// it was already there, `-1` on failure.
//
// The declaration is a floor, not a fence: a document may carry a field the
// declaration does not name, and it is stored in the row's extras.
//
// # Safety
// `db` must be a live handle and both strings valid C strings.
int32_t sekejap_create_collection(SekejapDb *db, const char *name, const char *fields_json);

// Remove a collection, its rows, its indexes and its descriptor. `1` if it
// was there, `0` if it was not, `-1` on failure.
//
// # Safety
// `db` must be a live handle and `name` a valid C string.
int32_t sekejap_drop_collection(SekejapDb *db, const char *name);

// Every collection name in the catalog, in key order, as a heap JSON array
// of strings. `NULL` on failure.
//
// # Safety
// `db` must be a live handle.
char *sekejap_collections(SekejapDb *db);

// The declared shape of one collection, as a heap JSON object:
// `{"name", "timestamps", "rows", "fields": [...], "indexes": [...]}`.
// `rows` is the LIVE row count or `null` where this database keeps no
// record for the collection -- `null` is "no record", not "no rows".
// `NULL` with `SekejapStatus_Ok` means there is no such collection.
//
// # Safety
// `db` must be a live handle and `collection` a valid C string.
char *sekejap_describe(SekejapDb *db, const char *collection);

// The rows of one collection, from the LIVE record when this database keeps
// one and from the walk when it does not. `-1` on failure, which includes a
// collection that is not in the catalog.
//
// # Safety
// `db` must be a live handle and `collection` a valid C string.
long sekejap_count_rows(SekejapDb *db, const char *collection);

// Count the rows of one collection BY WALKING them, whether or not a live
// record exists. The explicit walk, named as one. `-1` on failure.
//
// # Safety
// As `sekejap_count_rows`.
long sekejap_scan_count_rows(SekejapDb *db, const char *collection);

// Count every edge BY WALKING the primary edge keyspace. sekejap keeps no
// O(1) edge counter, so this is a scan and is named as one. `-1` on
// failure.
//
// # Safety
// `db` must be a live handle.
long sekejap_scan_count_edges(SekejapDb *db);

// Take the writer for many writes under ONE barrier. Every plain call
// commits per call, as the Rust API does; this is the other bargain.
//
// While the transaction is open it HOLDS the writer: a call on the same
// `SekejapDb*` that needs the writer waits for it. Commit or roll back
// before using the handle for anything else, and free this handle BEFORE
// `sekejap_close`. A handle freed any other way ROLLS BACK.
//
// `NULL` on failure.
//
// # Safety
// `db` must be a live handle. The returned handle borrows it.
SekejapTx *sekejap_tx_begin(SekejapDb *db);

// Write one document inside the transaction, with NO commit. `0` or `-1`.
//
// # Safety
// `tx` must be a live handle and every string a valid C string.
int32_t sekejap_tx_put(SekejapTx *tx,
                       const char *collection,
                       const char *key,
                       const char *document_json);

// Delete one row inside the transaction, with NO commit. `1` if it was
// there, `0` if it was not, `-1` on failure.
//
// # Safety
// As `sekejap_tx_put`.
int32_t sekejap_tx_delete(SekejapTx *tx, const char *collection, const char *key);

// Link two rows inside the transaction, with NO commit. `0` or `-1`.
//
// # Safety
// As `sekejap_tx_put`.
int32_t sekejap_tx_link(SekejapTx *tx,
                        const char *from_collection,
                        const char *from_key,
                        const char *edge_type,
                        const char *to_collection,
                        const char *to_key);

// Run one writing statement inside the transaction, with NO commit.
// Returns the rows it moved, or `-1`.
//
// # Safety
// As `sekejap_tx_put`, with `params_json` NULL or a JSON array.
long sekejap_tx_execute(SekejapTx *tx, const char *sql, const char *params_json);

// Commit the transaction and FREE the handle, whether the commit succeeded
// or not. `0` on success, `-1` on failure. The pointer is dangling after
// this call in both cases.
//
// # Safety
// `tx` must be a live handle that has not been committed or rolled back.
int32_t sekejap_tx_commit(SekejapTx *tx);

// Roll the transaction back and FREE the handle. `0` on success, `-1` on
// failure. The pointer is dangling after this call in both cases.
//
// # Safety
// As `sekejap_tx_commit`.
int32_t sekejap_tx_rollback(SekejapTx *tx);

// Fold the committed write-ahead log into the data file. `1` when it
// folded, `0` when a live reader holds a slot and the fold is DEFERRED
// (which in service mode is every call, because the published read view
// holds one for its whole life), `-1` on failure. Deferred is not a
// failure.
//
// # Safety
// `db` must be a live handle.
int32_t sekejap_checkpoint(SekejapDb *db);

// Make the newest commit visible to readers now. In single mode there is no
// published view to swap and every commit is already visible to this
// handle, so this succeeds having done nothing. `0` or `-1`.
//
// # Safety
// `db` must be a live handle.
int32_t sekejap_publish(SekejapDb *db);

// The bytes on disk, as a heap JSON object
// `{"data_bytes": n, "wal_bytes": n, "total_bytes": n}`. `NULL` on failure.
//
// # Safety
// `db` must be a live handle.
char *sekejap_storage(SekejapDb *db);

// Refuse a statement that runs longer than `milliseconds`. `0` clears the
// timeout. `0` on success, `-1` on failure -- which includes a handle that
// is not in service mode, REFUSED by name.
//
// # Safety
// `db` must be a live handle.
int32_t sekejap_statement_timeout_ms(SekejapDb *db, uint64_t milliseconds);

// Cancel the work in flight on this service, from any thread. The flag is
// STICKY until `sekejap_clear_interrupt`. `0` on success, `-1` on failure
// -- which includes a handle that is not in service mode.
//
// # Safety
// `db` must be a live handle.
int32_t sekejap_cancel(SekejapDb *db);

// Clear a cancel so the service accepts work again. `1` when a cancel was
// standing, `0` when none was, `-1` on failure.
//
// # Safety
// `db` must be a live handle.
int32_t sekejap_clear_interrupt(SekejapDb *db);

// Subscribe to the commit-time change feed. Returns a subscription id to
// pass to `sekejap_next_change` and `sekejap_unsubscribe`, or `-1` on
// failure -- which includes a handle that is not in service mode.
//
// The subscription is owned by the database handle, so an id is valid from
// any thread; a subscription left open is closed by `sekejap_close`.
//
// # Safety
// `db` must be a live handle.
long sekejap_subscribe(SekejapDb *db);

// The next change event for one subscription, as a heap JSON object, or
// `NULL` with `SekejapStatus_Ok` when none arrived. `timeout_ms` of `0`
// polls and returns at once; a positive value waits that long.
//
// The object is
// `{"sequence", "collections", "edge_types", "keys", "keys_total",
// "keys_truncated", "unnamed_writes", "rows_affected"}`. `keys` is empty
// and `keys_truncated` true when the batch moved more keys than the feed's
// per-event cap: the list is dropped whole rather than handed over
// half-true, and `collections` is still exact.
//
// # Safety
// `db` must be a live handle.
char *sekejap_next_change(SekejapDb *db, long subscription, uint64_t timeout_ms);

// Close one subscription. `1` when it was open on the service, `0` when it
// was not, `-1` on failure.
//
// # Safety
// `db` must be a live handle.
int32_t sekejap_unsubscribe(SekejapDb *db, long subscription);

// REFUSED: sekejap is disk-first and has no in-memory database. Always
// `NULL`, with the reason in `sekejap_last_error` and
// `SekejapStatus_Refused` in `sekejap_last_error_code`.
//
// A temporary directory would be a fake of an ephemeral store, so this does
// not make one. Give `sekejap_open` a directory.
SekejapDb *sekejap_open_memory(void);

// REFUSED: there is nothing proportional to rows held in memory to trim.
// Always `-1`, with the reason in `sekejap_last_error`.
//
// The caches sekejap keeps are bounded at open -- the buffer pool by
// `budget_bytes`, the plan cache by its three ceilings -- so a trim call
// would have nothing to give back, and a no-op that returned success would
// be a fake of reclaim.
//
// # Safety
// `db` may be NULL; when it is not, it must be a live handle.
int32_t sekejap_trim_memory(SekejapDb *db);

// REFUSED: there is no payload-rewriting compaction. Always `-1`.
// `sekejap_checkpoint` folds the committed write-ahead log into the data
// file; it does not rewrite rows, and naming that `compact` would promise
// something else.
//
// # Safety
// `db` may be NULL; when it is not, it must be a live handle.
int32_t sekejap_compact(SekejapDb *db);

// REFUSED: the `SHOW` family is not in this dialect. Always `NULL`.
// `sekejap_collections` and `sekejap_describe` answer the same
// questions as DATA rather than as a result set.
//
// # Safety
// `db` may be NULL; `statement` may be NULL.
char *sekejap_show(SekejapDb *db, const char *statement);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* SEKEJAP_H */
