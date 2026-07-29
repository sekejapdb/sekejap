/*
 * sekejap.h — C ABI for sekejap (https://sekejap.zebflow.com)
 *
 * AUTO-GENERATED from sekejap-capi/src/lib.rs by cbindgen. Do not edit by hand.
 *
 * A stable extern "C" surface over the sekejap engine, for binding from
 * Swift, Kotlin/JNI, Dart (dart:ffi), Go (cgo), and C/C++.
 *
 * Ownership: SekejapDb* is opaque (open* creates, close frees). Any char* the
 * library RETURNS is yours — free it once with sekejap_string_free (except
 * sekejap_version, which is static). Strings you PASS IN are borrowed UTF-8.
 * Failure sentinels: NULL for pointers, -1 for integers; sekejap_last_error(db)
 * has the message. No Rust panic crosses the boundary.
 */


#ifndef SEKEJAP_H
#define SEKEJAP_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

// Opaque database handle. Created by [`sekejap_open`] / [`sekejap_open_paged`],
// destroyed by [`sekejap_close`]. Treat as a black box from C.
typedef struct SekejapDb SekejapDb;

#if defined(SEKEJAP_ENGINE)
// Opaque thread-safe engine handle. `Engine` is `Send + Sync`, so the same
// `*mut SekejapEngine` may be used concurrently from many threads (do not,
// however, call `sekejap_engine_close` while other threads are still using it).
typedef struct SekejapEngine SekejapEngine;
#endif

// A prepared (compiled) query. Parse the SQL once with [`sekejap_prepare`], run
// it many times with different parameters via [`sekejap_query_prepared`], and
// free it with [`sekejap_stmt_free`]. Opaque — do not inspect.
typedef struct SekejapStmt SekejapStmt;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

// Open (or create) a database at `path`. Returns null on failure.
//
// # Safety
// `path` must be a valid null-terminated UTF-8 C string.
SekejapDb *sekejap_open(const char *path);

// Open a database in paged (mmap) mode — fast startup regardless of size.
// Returns null on failure.
//
// # Safety
// `path` must be a valid null-terminated UTF-8 C string.
SekejapDb *sekejap_open_paged(const char *path);

// Close a database handle and free all its resources. Safe to call with null.
// After this, the pointer is dangling — do not use it again.
//
// # Safety
// `db` must be null or a handle returned by `sekejap_open*` and not yet closed.
void sekejap_close(SekejapDb *db);

// Run a statement that changes data or schema (`CREATE`, `INSERT`, `UPDATE`,
// `DELETE`, `ALTER`, `BEGIN`/`COMMIT`, edge inserts, …).
//
// Returns the number of affected rows (`>= 0`), or `-1` on error — call
// [`sekejap_last_error`] for the message.
//
// # Safety
// `db` must be a live handle; `sql` a valid null-terminated UTF-8 C string.
long sekejap_execute(SekejapDb *db, const char *sql);

// Run a `SELECT` (including `SELECT ... FROM MATCH`) and return the result rows
// as a heap-allocated JSON-array string. Each element is the row's payload
// object (or `{"_slug": "..."}` when a row has no payload).
//
// Returns null on error — call [`sekejap_last_error`]. Free the returned string
// with [`sekejap_string_free`].
//
// # Safety
// `db` must be a live handle; `sql` a valid null-terminated UTF-8 C string.
char *sekejap_query(SekejapDb *db, const char *sql);

// Fetch a single node's payload by slug (`"collection/key"`) as a heap JSON
// string. Returns null if the node does not exist (this is *not* an error) or
// on failure — distinguish with [`sekejap_last_error`], which is cleared on a
// successful lookup and on a clean miss. Free with [`sekejap_string_free`].
//
// # Safety
// `db` must be a live handle; `slug` a valid null-terminated UTF-8 C string.
char *sekejap_get(SekejapDb *db, const char *slug);

// Compact the database: truncate the WAL, rewrite payloads/topology, reclaim
// RAM. Run after a large bulk load. Returns `0` on success, `-1` on error.
//
// # Safety
// `db` must be a live handle.
int32_t sekejap_compact(SekejapDb *db);

// Reclaim excess in-RAM capacity on demand (cheap; never drops data/indexes).
//
// # Safety
// `db` must be a live handle.
void sekejap_trim_memory(SekejapDb *db);

// Flush buffered writes durably to disk (fsync). `0` on success, `-1` on error.
//
// # Safety
// `db` must be a live handle.
int32_t sekejap_sync(SekejapDb *db);

// Open a database read-only (writes will error). Returns null on failure.
//
// # Safety
// `path` must be a valid null-terminated UTF-8 C string.
SekejapDb *sekejap_open_read_only(const char *path);

// Like [`sekejap_execute`] but with positional parameters (`$1`, `$2`, …) bound
// from a JSON array — the injection-safe way to pass values. `params_json` may
// be null/empty for none. Returns affected rows (`>= 0`) or `-1`.
//
// # Safety
// `db` a live handle; `sql`/`params_json` valid null-terminated UTF-8 (or null
// for `params_json`).
long sekejap_execute_params(SekejapDb *db, const char *sql, const char *params_json);

// Like [`sekejap_query`] but with positional parameters bound from a JSON array.
// Returns a heap JSON-array string, or null on error. Free with
// [`sekejap_string_free`].
//
// # Safety
// `db` a live handle; `sql`/`params_json` valid null-terminated UTF-8 (or null
// for `params_json`).
char *sekejap_query_params(SekejapDb *db, const char *sql, const char *params_json);

// Compile `sql` into a reusable prepared statement (tokenized + validated once).
// Use `$1`, `$2`, … placeholders for values bound at execution. Returns null on
// a parse error (see [`sekejap_last_error`]).
//
// # Safety
// `db` a live handle; `sql` a valid null-terminated UTF-8 C string.
SekejapStmt *sekejap_prepare(SekejapDb *db, const char *sql);

// Execute a prepared statement, binding `$1`, `$2`, … from `params_json` (a JSON
// array string, or null for none). Returns a heap JSON-array string, or null on
// error. Free the result with [`sekejap_string_free`].
//
// # Safety
// `db`/`stmt` live handles; `params_json` valid null-terminated UTF-8 or null.
char *sekejap_query_prepared(SekejapDb *db, const SekejapStmt *stmt, const char *params_json);

// Free a prepared statement. Safe with null; call once.
//
// # Safety
// `stmt` must be null or a handle from [`sekejap_prepare`], not yet freed.
void sekejap_stmt_free(SekejapStmt *stmt);

// Run an introspection statement (`SHOW TABLES`, `SHOW EDGES`, `SHOW <table>`)
// and return the rows as a heap JSON-array string, or null on error. Free with
// [`sekejap_string_free`].
//
// # Safety
// `db` a live handle; `sql` a valid null-terminated UTF-8 C string.
char *sekejap_show(SekejapDb *db, const char *sql);

// Insert or replace one node by slug (`"collection/key"`) with a JSON payload.
// `0` on success, `-1` on error.
//
// # Safety
// `db` a live handle; `slug`/`payload_json` valid null-terminated UTF-8.
int32_t sekejap_put(SekejapDb *db, const char *slug, const char *payload_json);

// Bulk insert nodes from a JSON object mapping `slug -> payload object`. Returns
// the number of rows inserted (`>= 0`) or `-1`. Much faster than repeated
// [`sekejap_put`] for large loads (one WAL sync).
//
// # Safety
// `db` a live handle; `rows_json` a valid null-terminated UTF-8 C string.
long sekejap_put_many(SekejapDb *db, const char *rows_json);

// Delete one node by slug. `0` on success (whether or not it existed), `-1` on
// a null handle / panic.
//
// # Safety
// `db` a live handle; `slug` a valid null-terminated UTF-8 C string.
int32_t sekejap_remove(SekejapDb *db, const char *slug);

// Create a plain edge `from -[edge_type]-> to` (slugs are `"collection/key"`).
// `0` on success, `-1` on error.
//
// # Safety
// `db` a live handle; all string args valid null-terminated UTF-8.
int32_t sekejap_link(SekejapDb *db, const char *from, const char *to, const char *edge_type);

// Create an edge carrying attributes (a JSON object; primitives are stored in
// fast-lane columns, the rest in a JSON bag). `0` on success, `-1` on error.
//
// # Safety
// `db` a live handle; all string args valid null-terminated UTF-8.
int32_t sekejap_link_meta(SekejapDb *db,
                          const char *from,
                          const char *to,
                          const char *edge_type,
                          const char *meta_json);

// Remove an edge `from -[edge_type]-> to`. `0` on success, `-1` on error.
//
// # Safety
// `db` a live handle; all string args valid null-terminated UTF-8.
int32_t sekejap_unlink(SekejapDb *db, const char *from, const char *to, const char *edge_type);

// Whether a node with `slug` exists. `1` = yes, `0` = no, `-1` = error.
//
// # Safety
// `db` a live handle; `slug` a valid null-terminated UTF-8 C string.
int32_t sekejap_contains(SekejapDb *db, const char *slug);

// Number of nodes in the database, or `-1` on error.
//
// # Safety
// `db` must be a live handle.
long sekejap_node_count(SekejapDb *db);

// Number of edges in the database, or `-1` on error.
//
// # Safety
// `db` must be a live handle.
long sekejap_edge_count(SekejapDb *db);

// All collection (table) names as a heap JSON-array string, or null on error.
// Free with [`sekejap_string_free`].
//
// # Safety
// `db` must be a live handle.
char *sekejap_collection_names(SekejapDb *db);

// The `CREATE TABLE` DDL for a collection as a heap string, or null if the
// collection has no schema. Free with [`sekejap_string_free`].
//
// # Safety
// `db` a live handle; `collection` a valid null-terminated UTF-8 C string.
char *sekejap_schema_ddl(SekejapDb *db, const char *collection);

// Return the last error message for this handle as a heap C string, or null if
// the most recent call succeeded. Free with [`sekejap_string_free`].
//
// # Safety
// `db` must be a live handle.
char *sekejap_last_error(const SekejapDb *db);

// Free a string returned by this library (`sekejap_query`, `sekejap_get`,
// `sekejap_last_error`). Safe to call with null. Call exactly once per string.
//
// # Safety
// `s` must be null or a pointer returned by this library and not yet freed.
void sekejap_string_free(char *s);

// The sekejap-capi version, as a static null-terminated string. Do NOT free.
const char *sekejap_version(void);

#if defined(SEKEJAP_ENGINE)
// Open (or create) a thread-safe engine at `path`. Returns null on failure.
//
// # Safety
// `path` must be a valid null-terminated UTF-8 C string.
SekejapEngine *sekejap_engine_open(const char *path);
#endif

#if defined(SEKEJAP_ENGINE)
// Open an in-memory (ephemeral) thread-safe engine. Never fails → non-null.
SekejapEngine *sekejap_engine_open_memory(void);
#endif

#if defined(SEKEJAP_ENGINE)
// Close an engine handle and free its resources. Safe with null. Do NOT call
// while other threads are still using this handle.
//
// # Safety
// `e` must be null or a handle from `sekejap_engine_open*`, not yet closed.
void sekejap_engine_close(SekejapEngine *e);
#endif

#if defined(SEKEJAP_ENGINE)
// Run a `SELECT` — CONCURRENT-SAFE (takes a read lock; many run in parallel).
// Returns a heap JSON-array string, or null on error. Free with
// `sekejap_string_free`.
//
// # Safety
// `e` a live handle; `sql` valid null-terminated UTF-8.
char *sekejap_engine_query(const SekejapEngine *e, const char *sql);
#endif

#if defined(SEKEJAP_ENGINE)
// Parameterized `SELECT` (positional `$1` from a JSON array). Concurrent-safe.
//
// # Safety
// `e` a live handle; `sql`/`params_json` valid UTF-8 (params_json may be null).
char *sekejap_engine_query_params(const SekejapEngine *e, const char *sql, const char *params_json);
#endif

#if defined(SEKEJAP_ENGINE)
// Run a mutating statement — writes are SERIALIZED (one writer at a time)
// while reads continue in parallel. Returns affected rows, or -1.
//
// # Safety
// `e` a live handle; `sql` valid null-terminated UTF-8.
long sekejap_engine_execute(const SekejapEngine *e, const char *sql);
#endif

#if defined(SEKEJAP_ENGINE)
// Parameterized mutating statement (injection-safe). Returns affected rows, or -1.
//
// # Safety
// `e` a live handle; `sql`/`params_json` valid UTF-8 (params_json may be null).
long sekejap_engine_execute_params(const SekejapEngine *e,
                                   const char *sql,
                                   const char *params_json);
#endif

#if defined(SEKEJAP_ENGINE)
// Flush the write buffer to disk under one fsync (group commit). Returns the
// number of buffered rows committed, or -1.
//
// # Safety
// `e` must be a live handle.
long sekejap_engine_flush(const SekejapEngine *e);
#endif

#if defined(SEKEJAP_ENGINE)
// Compact the engine's store. `0` on success, `-1` on error.
//
// # Safety
// `e` must be a live handle.
int32_t sekejap_engine_compact(const SekejapEngine *e);
#endif

#if defined(SEKEJAP_ENGINE)
// Reclaim excess in-RAM capacity (cheap; never drops data/indexes).
//
// # Safety
// `e` must be a live handle.
void sekejap_engine_trim_memory(const SekejapEngine *e);
#endif

#if defined(SEKEJAP_ENGINE)
// The calling thread's last engine error as a heap C string, or null if the
// last engine call on THIS thread succeeded. Free with `sekejap_string_free`.
// (Thread-local — like `errno` — so concurrent callers don't clobber each other.)
char *sekejap_engine_last_error(void);
#endif

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* SEKEJAP_H */
