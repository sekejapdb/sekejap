//! # sekejap-capi — the C ABI for sekejap
//!
//! A small, stable `extern "C"` surface over [`sekejap::CoreDB`]. This is the
//! lingua franca that lets Swift, Kotlin, Dart (`dart:ffi`), Go (cgo), and plain
//! C/C++ drive the engine — the same way those languages bind to SQLite's C API.
//!
//! ## Design rules (kept deliberately boring for safety)
//!
//! - **One opaque handle.** `sekejap_open` returns a `SekejapDb*`. Every call
//!   takes it. `sekejap_close` frees it. Never inspect its fields from C.
//! - **UTF-8 in, JSON out.** Inputs are null-terminated UTF-8 `const char*`.
//!   Query results come back as a single JSON-array string — the caller parses
//!   it. This avoids a fragile row-iteration ABI; it's enough to build any
//!   higher-level binding on top.
//! - **Explicit ownership.** Any `char*` this library *returns* is heap-owned by
//!   the caller and must be freed with [`sekejap_string_free`] — exactly once.
//!   Strings the caller *passes in* are borrowed and never freed here.
//!
//!   The one exception is [`sekejap_version`], which returns a pointer into
//!   static program data and is typed `*const c_char` to say so. Passing it to
//!   `sekejap_string_free` hands `CString::from_raw` a pointer the allocator
//!   never gave out, which corrupts the allocator rather than freeing anything.
//!   Every binding in this repository already treats it as static; the rule is
//!   stated here because a rule with an unstated exception is how that stops
//!   being true.
//! - **No panic ever crosses the boundary.** Every entry point is wrapped in
//!   `catch_unwind`; a panic becomes an error return, never undefined behavior.
//! - **NULL is the failure sentinel** for pointer returns; `-1` for integer
//!   returns. After any failure, [`sekejap_last_error`] returns a human-readable
//!   message for that handle.
//!
//! See `include/sekejap.h` for the C declarations and `README.md` for examples.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_long};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::ptr;

use sekejap::CoreDB;
use serde_json::{json, Value};

/// Opaque database handle. Created by [`sekejap_open`] / [`sekejap_open_paged`],
/// destroyed by [`sekejap_close`]. Treat as a black box from C.
pub struct SekejapDb {
    inner: CoreDB,
    last_error: Option<String>,
}

impl SekejapDb {
    fn ok(&mut self) {
        self.last_error = None;
    }
    fn fail(&mut self, msg: impl Into<String>) {
        self.last_error = Some(msg.into());
    }
}

/// Borrow a C string as `&str`, or `None` if it is null or not valid UTF-8.
///
/// # Safety
/// `p` must be null or a valid pointer to a null-terminated C string that stays
/// alive for the duration of the borrow.
unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok()
}

/// Move a Rust `String` onto the heap as a C string for the caller to own.
/// Returns null if the string contains an interior NUL (cannot be a C string).
fn into_c_string(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

/// Parse a JSON string into a params vector for the `*_params` calls. A null or
/// empty pointer means "no params". A JSON array becomes its elements; any other
/// single JSON value becomes a one-element vector. `None` on invalid JSON.
///
/// # Safety
/// `json` must be null or a valid null-terminated UTF-8 C string.
unsafe fn parse_params(json: *const c_char) -> Option<Vec<Value>> {
    let s = match cstr(json) {
        None if json.is_null() => return Some(Vec::new()),
        None => return None, // non-null but bad UTF-8
        Some(s) if s.trim().is_empty() => return Some(Vec::new()),
        Some(s) => s,
    };
    match serde_json::from_str::<Value>(s).ok()? {
        Value::Array(a) => Some(a),
        other => Some(vec![other]),
    }
}

/// Run `f` on the handle, returning an integer result: the closure's value on
/// success, or `-1` on a null handle / error / panic. Sets `last_error`.
fn guard_int<F>(db: *mut SekejapDb, f: F) -> c_long
where
    F: FnOnce(&mut SekejapDb) -> Result<c_long, String>,
{
    if db.is_null() {
        return -1;
    }
    let d = unsafe { &mut *db };
    catch_unwind(AssertUnwindSafe(move || match f(d) {
        Ok(v) => {
            d.ok();
            v
        }
        Err(e) => {
            d.fail(e);
            -1
        }
    }))
    .unwrap_or(-1)
}

/// Run `f` on the handle, returning a heap C string: `Some(s)` → owned string,
/// `Ok(None)` → NULL (a clean miss, not an error), error/panic → NULL. Sets
/// `last_error` on error.
fn guard_str<F>(db: *mut SekejapDb, f: F) -> *mut c_char
where
    F: FnOnce(&mut SekejapDb) -> Result<Option<String>, String>,
{
    if db.is_null() {
        return ptr::null_mut();
    }
    let d = unsafe { &mut *db };
    catch_unwind(AssertUnwindSafe(move || match f(d) {
        Ok(Some(s)) => {
            d.ok();
            into_c_string(s)
        }
        Ok(None) => {
            d.ok();
            ptr::null_mut()
        }
        Err(e) => {
            d.fail(e);
            ptr::null_mut()
        }
    }))
    .unwrap_or(ptr::null_mut())
}

/// Collect a query `Set` into a JSON-array string (one object per row; rows
/// without a payload become `{"_slug": "..."}`).
fn collect_rows_json(set: sekejap::Set<'_>) -> Result<String, String> {
    let rows: Vec<Value> = set
        .collect()
        .into_iter()
        .map(|h| h.payload.unwrap_or_else(|| json!({ "_slug": h.slug })))
        .collect();
    serde_json::to_string(&Value::Array(rows)).map_err(|e| e.to_string())
}

// ── Lifecycle ───────────────────────────────────────────────────────────────

/// Open (or create) a database at `path`. Returns null on failure.
///
/// # Safety
/// `path` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_open(path: *const c_char) -> *mut SekejapDb {
    open_impl(path, false)
}

/// Open a database in paged (mmap) mode — fast startup regardless of size.
/// Returns null on failure.
///
/// # Safety
/// `path` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_open_paged(path: *const c_char) -> *mut SekejapDb {
    open_impl(path, true)
}

fn open_impl(path: *const c_char, paged: bool) -> *mut SekejapDb {
    catch_unwind(|| {
        let p = match unsafe { cstr(path) } {
            Some(s) => s,
            None => return ptr::null_mut(),
        };
        let opened = if paged {
            CoreDB::open_paged(Path::new(p))
        } else {
            CoreDB::open(Path::new(p))
        };
        match opened {
            Ok(inner) => Box::into_raw(Box::new(SekejapDb {
                inner,
                last_error: None,
            })),
            Err(_) => ptr::null_mut(),
        }
    })
    .unwrap_or(ptr::null_mut())
}

/// Close a database handle and free all its resources. Safe to call with null.
/// After this, the pointer is dangling — do not use it again.
///
/// # Safety
/// `db` must be null or a handle returned by `sekejap_open*` and not yet closed.
#[no_mangle]
pub unsafe extern "C" fn sekejap_close(db: *mut SekejapDb) {
    if db.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        drop(Box::from_raw(db));
    }));
}

// ── Mutations (DDL / DML) ───────────────────────────────────────────────────

/// Run a statement that changes data or schema (`CREATE`, `INSERT`, `UPDATE`,
/// `DELETE`, `ALTER`, `BEGIN`/`COMMIT`, edge inserts, …).
///
/// Returns the number of affected rows (`>= 0`), or `-1` on error — call
/// [`sekejap_last_error`] for the message.
///
/// # Safety
/// `db` must be a live handle; `sql` a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_execute(db: *mut SekejapDb, sql: *const c_char) -> c_long {
    if db.is_null() {
        return -1;
    }
    let d = &mut *db;
    catch_unwind(AssertUnwindSafe(|| {
        let s = match cstr(sql) {
            Some(s) => s,
            None => {
                d.fail("sql pointer is null or not valid UTF-8");
                return -1;
            }
        };
        match d.inner.execute(s) {
            Ok(n) => {
                d.ok();
                n as c_long
            }
            Err(e) => {
                d.fail(e.to_string());
                -1
            }
        }
    }))
    .unwrap_or_else(|_| {
        d.fail("panic while executing statement");
        -1
    })
}

// ── Queries ─────────────────────────────────────────────────────────────────

/// Run a `SELECT` (including `SELECT ... FROM MATCH`) and return the result rows
/// as a heap-allocated JSON-array string. Each element is the row's payload
/// object (or `{"_slug": "..."}` when a row has no payload).
///
/// Returns null on error — call [`sekejap_last_error`]. Free the returned string
/// with [`sekejap_string_free`].
///
/// # Safety
/// `db` must be a live handle; `sql` a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_query(db: *mut SekejapDb, sql: *const c_char) -> *mut c_char {
    guard_str(db, |d| {
        let s = cstr(sql).ok_or_else(|| "sql is null or not valid UTF-8".to_string())?;
        let set = d.inner.query(s).map_err(|e| e.to_string())?;
        collect_rows_json(set).map(Some)
    })
}

/// Fetch a single node's payload by slug (`"collection/key"`) as a heap JSON
/// string. Returns null if the node does not exist (this is *not* an error) or
/// on failure — distinguish with [`sekejap_last_error`], which is cleared on a
/// successful lookup and on a clean miss. Free with [`sekejap_string_free`].
///
/// # Safety
/// `db` must be a live handle; `slug` a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_get(db: *mut SekejapDb, slug: *const c_char) -> *mut c_char {
    if db.is_null() {
        return ptr::null_mut();
    }
    let d = &mut *db;
    catch_unwind(AssertUnwindSafe(|| {
        let s = match cstr(slug) {
            Some(s) => s,
            None => {
                d.fail("slug pointer is null or not valid UTF-8");
                return ptr::null_mut();
            }
        };
        d.ok();
        match d.inner.get(s) {
            Some(json_str) => into_c_string(json_str),
            None => ptr::null_mut(),
        }
    }))
    .unwrap_or_else(|_| {
        d.fail("panic while fetching node");
        ptr::null_mut()
    })
}

// ── Maintenance ─────────────────────────────────────────────────────────────

/// Compact the database: truncate the WAL, rewrite payloads/topology, reclaim
/// RAM. Run after a large bulk load. Returns `0` on success, `-1` on error.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_compact(db: *mut SekejapDb) -> i32 {
    if db.is_null() {
        return -1;
    }
    let d = &mut *db;
    catch_unwind(AssertUnwindSafe(|| match d.inner.compact() {
        Ok(()) => {
            d.ok();
            0
        }
        Err(e) => {
            d.fail(e.to_string());
            -1
        }
    }))
    .unwrap_or_else(|_| {
        d.fail("panic while compacting");
        -1
    })
}

/// Reclaim excess in-RAM capacity on demand (cheap; never drops data/indexes).
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_trim_memory(db: *mut SekejapDb) {
    guard_int(db, |d| {
        d.inner.trim_memory();
        Ok(0)
    });
}

/// Flush buffered writes durably to disk (fsync). `0` on success, `-1` on error.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_sync(db: *mut SekejapDb) -> i32 {
    guard_int(db, |d| d.inner.sync().map(|_| 0).map_err(|e| e.to_string())) as i32
}

// ── Extended lifecycle ──────────────────────────────────────────────────────

/// Open a database read-only (writes will error). Returns null on failure.
///
/// # Safety
/// `path` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_open_read_only(path: *const c_char) -> *mut SekejapDb {
    catch_unwind(|| {
        let p = match cstr(path) {
            Some(s) => s,
            None => return ptr::null_mut(),
        };
        match CoreDB::open_read_only(Path::new(p)) {
            Ok(inner) => Box::into_raw(Box::new(SekejapDb {
                inner,
                last_error: None,
            })),
            Err(_) => ptr::null_mut(),
        }
    })
    .unwrap_or(ptr::null_mut())
}

// ── Parameterized statements (injection-safe) ───────────────────────────────

/// Like [`sekejap_execute`] but with positional parameters (`$1`, `$2`, …) bound
/// from a JSON array — the injection-safe way to pass values. `params_json` may
/// be null/empty for none. Returns affected rows (`>= 0`) or `-1`.
///
/// # Safety
/// `db` a live handle; `sql`/`params_json` valid null-terminated UTF-8 (or null
/// for `params_json`).
#[no_mangle]
pub unsafe extern "C" fn sekejap_execute_params(
    db: *mut SekejapDb,
    sql: *const c_char,
    params_json: *const c_char,
) -> c_long {
    guard_int(db, |d| {
        let s = cstr(sql).ok_or_else(|| "sql is null or not valid UTF-8".to_string())?;
        let params = parse_params(params_json).ok_or_else(|| "params_json is not valid JSON".to_string())?;
        d.inner
            .execute_params(s, &params)
            .map(|n| n as c_long)
            .map_err(|e| e.to_string())
    })
}

/// Like [`sekejap_query`] but with positional parameters bound from a JSON array.
/// Returns a heap JSON-array string, or null on error. Free with
/// [`sekejap_string_free`].
///
/// # Safety
/// `db` a live handle; `sql`/`params_json` valid null-terminated UTF-8 (or null
/// for `params_json`).
#[no_mangle]
pub unsafe extern "C" fn sekejap_query_params(
    db: *mut SekejapDb,
    sql: *const c_char,
    params_json: *const c_char,
) -> *mut c_char {
    guard_str(db, |d| {
        let s = cstr(sql).ok_or_else(|| "sql is null or not valid UTF-8".to_string())?;
        let params = parse_params(params_json).ok_or_else(|| "params_json is not valid JSON".to_string())?;
        let set = d.inner.query_params(s, &params).map_err(|e| e.to_string())?;
        collect_rows_json(set).map(Some)
    })
}

// ── Prepared statements (PostgreSQL-style: prepare once, execute many) ───────

/// A prepared (compiled) query. Parse the SQL once with [`sekejap_prepare`], run
/// it many times with different parameters via [`sekejap_query_prepared`], and
/// free it with [`sekejap_stmt_free`]. Opaque — do not inspect.
pub struct SekejapStmt {
    inner: sekejap::PreparedQuery,
}

/// Compile `sql` into a reusable prepared statement (tokenized + validated once).
/// Use `$1`, `$2`, … placeholders for values bound at execution. Returns null on
/// a parse error (see [`sekejap_last_error`]).
///
/// # Safety
/// `db` a live handle; `sql` a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_prepare(
    db: *mut SekejapDb,
    sql: *const c_char,
) -> *mut SekejapStmt {
    if db.is_null() {
        return ptr::null_mut();
    }
    let d = &mut *db;
    catch_unwind(AssertUnwindSafe(|| {
        let s = match cstr(sql) {
            Some(s) => s,
            None => {
                d.fail("sql is null or not valid UTF-8");
                return ptr::null_mut();
            }
        };
        match d.inner.prepare(s) {
            Ok(p) => {
                d.ok();
                Box::into_raw(Box::new(SekejapStmt { inner: p }))
            }
            Err(e) => {
                d.fail(e.to_string());
                ptr::null_mut()
            }
        }
    }))
    .unwrap_or_else(|_| {
        d.fail("panic while preparing");
        ptr::null_mut()
    })
}

/// Execute a prepared statement, binding `$1`, `$2`, … from `params_json` (a JSON
/// array string, or null for none). Returns a heap JSON-array string, or null on
/// error. Free the result with [`sekejap_string_free`].
///
/// # Safety
/// `db`/`stmt` live handles; `params_json` valid null-terminated UTF-8 or null.
#[no_mangle]
pub unsafe extern "C" fn sekejap_query_prepared(
    db: *mut SekejapDb,
    stmt: *const SekejapStmt,
    params_json: *const c_char,
) -> *mut c_char {
    if stmt.is_null() {
        return ptr::null_mut();
    }
    let st = &*stmt;
    guard_str(db, |d| {
        let params = parse_params(params_json)
            .ok_or_else(|| "params_json is not valid JSON".to_string())?;
        let set = d
            .inner
            .query_prepared(&st.inner, &params)
            .map_err(|e| e.to_string())?;
        collect_rows_json(set).map(Some)
    })
}

/// Free a prepared statement. Safe with null; call once.
///
/// # Safety
/// `stmt` must be null or a handle from [`sekejap_prepare`], not yet freed.
#[no_mangle]
pub unsafe extern "C" fn sekejap_stmt_free(stmt: *mut SekejapStmt) {
    if stmt.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| drop(Box::from_raw(stmt))));
}

/// Run an introspection statement (`SHOW TABLES`, `SHOW EDGES`, `SHOW <table>`)
/// and return the rows as a heap JSON-array string, or null on error. Free with
/// [`sekejap_string_free`].
///
/// # Safety
/// `db` a live handle; `sql` a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_show(db: *mut SekejapDb, sql: *const c_char) -> *mut c_char {
    guard_str(db, |d| {
        let s = cstr(sql).ok_or_else(|| "sql is null or not valid UTF-8".to_string())?;
        let hits = d.inner.show(s).map_err(|e| e.to_string())?;
        let rows: Vec<Value> = hits
            .into_iter()
            .map(|h| h.payload.unwrap_or_else(|| json!({ "_slug": h.slug })))
            .collect();
        serde_json::to_string(&Value::Array(rows))
            .map(Some)
            .map_err(|e| e.to_string())
    })
}

// ── Direct node / edge mutations (no SQL) ───────────────────────────────────

/// Insert or replace one node by slug (`"collection/key"`) with a JSON payload.
/// `0` on success, `-1` on error.
///
/// # Safety
/// `db` a live handle; `slug`/`payload_json` valid null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn sekejap_put(
    db: *mut SekejapDb,
    slug: *const c_char,
    payload_json: *const c_char,
) -> i32 {
    guard_int(db, |d| {
        let s = cstr(slug).ok_or_else(|| "slug is null or not valid UTF-8".to_string())?;
        let j = cstr(payload_json).ok_or_else(|| "payload_json is null or not valid UTF-8".to_string())?;
        d.inner.put(s, j).map(|_| 0).map_err(|e| e.to_string())
    }) as i32
}

/// Bulk insert nodes from a JSON object mapping `slug -> payload object`. Returns
/// the number of rows inserted (`>= 0`) or `-1`. Much faster than repeated
/// [`sekejap_put`] for large loads (one WAL sync).
///
/// # Safety
/// `db` a live handle; `rows_json` a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_put_many(db: *mut SekejapDb, rows_json: *const c_char) -> c_long {
    guard_int(db, |d| {
        let j = cstr(rows_json).ok_or_else(|| "rows_json is null or not valid UTF-8".to_string())?;
        let map: serde_json::Map<String, Value> = match serde_json::from_str(j) {
            Ok(Value::Object(m)) => m,
            Ok(_) => return Err("rows_json must be a JSON object {slug: payload}".to_string()),
            Err(e) => return Err(e.to_string()),
        };
        let rows: Vec<(String, Value)> = map.into_iter().collect();
        d.inner
            .put_value_bulk(rows)
            .map(|n| n as c_long)
            .map_err(|e| e.to_string())
    })
}

/// Delete one node by slug. `0` on success (whether or not it existed), `-1` on
/// a null handle / panic.
///
/// # Safety
/// `db` a live handle; `slug` a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_remove(db: *mut SekejapDb, slug: *const c_char) -> i32 {
    guard_int(db, |d| {
        let s = cstr(slug).ok_or_else(|| "slug is null or not valid UTF-8".to_string())?;
        d.inner.remove(s);
        Ok(0)
    }) as i32
}

/// Create a plain edge `from -[edge_type]-> to` (slugs are `"collection/key"`).
/// `0` on success, `-1` on error.
///
/// # Safety
/// `db` a live handle; all string args valid null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn sekejap_link(
    db: *mut SekejapDb,
    from: *const c_char,
    to: *const c_char,
    edge_type: *const c_char,
) -> i32 {
    guard_int(db, |d| {
        let f = cstr(from).ok_or_else(|| "from is null or not valid UTF-8".to_string())?;
        let t = cstr(to).ok_or_else(|| "to is null or not valid UTF-8".to_string())?;
        let e = cstr(edge_type).ok_or_else(|| "edge_type is null or not valid UTF-8".to_string())?;
        d.inner.link(f, t, e);
        Ok(0)
    }) as i32
}

/// Create an edge carrying attributes (a JSON object; primitives are stored in
/// fast-lane columns, the rest in a JSON bag). `0` on success, `-1` on error.
///
/// # Safety
/// `db` a live handle; all string args valid null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn sekejap_link_meta(
    db: *mut SekejapDb,
    from: *const c_char,
    to: *const c_char,
    edge_type: *const c_char,
    meta_json: *const c_char,
) -> i32 {
    guard_int(db, |d| {
        let f = cstr(from).ok_or_else(|| "from is null or not valid UTF-8".to_string())?;
        let t = cstr(to).ok_or_else(|| "to is null or not valid UTF-8".to_string())?;
        let e = cstr(edge_type).ok_or_else(|| "edge_type is null or not valid UTF-8".to_string())?;
        let m = cstr(meta_json).ok_or_else(|| "meta_json is null or not valid UTF-8".to_string())?;
        d.inner.link_meta(f, t, e, m).map(|_| 0).map_err(|e| e.to_string())
    }) as i32
}

/// Remove an edge `from -[edge_type]-> to`. `0` on success, `-1` on error.
///
/// # Safety
/// `db` a live handle; all string args valid null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn sekejap_unlink(
    db: *mut SekejapDb,
    from: *const c_char,
    to: *const c_char,
    edge_type: *const c_char,
) -> i32 {
    guard_int(db, |d| {
        let f = cstr(from).ok_or_else(|| "from is null or not valid UTF-8".to_string())?;
        let t = cstr(to).ok_or_else(|| "to is null or not valid UTF-8".to_string())?;
        let e = cstr(edge_type).ok_or_else(|| "edge_type is null or not valid UTF-8".to_string())?;
        d.inner.unlink(f, t, e);
        Ok(0)
    }) as i32
}

// ── Introspection ───────────────────────────────────────────────────────────

/// Whether a node with `slug` exists. `1` = yes, `0` = no, `-1` = error.
///
/// # Safety
/// `db` a live handle; `slug` a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_contains(db: *mut SekejapDb, slug: *const c_char) -> i32 {
    guard_int(db, |d| {
        let s = cstr(slug).ok_or_else(|| "slug is null or not valid UTF-8".to_string())?;
        Ok(if d.inner.contains(s) { 1 } else { 0 })
    }) as i32
}

/// Number of nodes in the database, or `-1` on error.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_node_count(db: *mut SekejapDb) -> c_long {
    guard_int(db, |d| Ok(d.inner.node_count() as c_long))
}

/// Number of edges in the database, or `-1` on error.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_edge_count(db: *mut SekejapDb) -> c_long {
    guard_int(db, |d| Ok(d.inner.edge_count() as c_long))
}

/// All collection (table) names as a heap JSON-array string, or null on error.
/// Free with [`sekejap_string_free`].
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_collection_names(db: *mut SekejapDb) -> *mut c_char {
    guard_str(db, |d| {
        serde_json::to_string(&d.inner.collection_names())
            .map(Some)
            .map_err(|e| e.to_string())
    })
}

/// The `CREATE TABLE` DDL for a collection as a heap string, or null if the
/// collection has no schema. Free with [`sekejap_string_free`].
///
/// # Safety
/// `db` a live handle; `collection` a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_schema_ddl(
    db: *mut SekejapDb,
    collection: *const c_char,
) -> *mut c_char {
    guard_str(db, |d| {
        let c = cstr(collection).ok_or_else(|| "collection is null or not valid UTF-8".to_string())?;
        Ok(d.inner.schema_ddl(c))
    })
}

// ── Errors, memory, version ─────────────────────────────────────────────────

/// Return the last error message for this handle as a heap C string, or null if
/// the most recent call succeeded. Free with [`sekejap_string_free`].
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_last_error(db: *const SekejapDb) -> *mut c_char {
    if db.is_null() {
        return ptr::null_mut();
    }
    let d = &*db;
    match &d.last_error {
        Some(msg) => into_c_string(msg.clone()),
        None => ptr::null_mut(),
    }
}

/// Free a string returned by this library (`sekejap_query`, `sekejap_get`,
/// `sekejap_last_error`). Safe to call with null. Call exactly once per string.
///
/// # Safety
/// `s` must be null or a pointer returned by this library and not yet freed.
#[no_mangle]
pub unsafe extern "C" fn sekejap_string_free(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    drop(CString::from_raw(s));
}

/// The sekejap-capi version, as a static null-terminated string. Do NOT free.
#[no_mangle]
pub extern "C" fn sekejap_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

// ── Concurrent Engine handle (feature `engine`) ─────────────────────────────
//
// The `SekejapEngine*` handle wraps sekejap's thread-safe `Engine` (an RwLock
// over the store: parallel readers, one serialized writer). Unlike `SekejapDb*`,
// it is SAFE TO CALL FROM MULTIPLE THREADS on the same handle — the base for
// building a sekejap server in any language. Errors are thread-local (like
// `errno`): each thread reads its own last error via `sekejap_engine_last_error`.

mod engine_abi {
    use super::{cstr, into_c_string, parse_params};
    use sekejap::engine::Engine;
    use sekejap::Hit;
    use serde_json::{json, Value};
    use std::cell::RefCell;
    use std::ffi::CString;
    use std::os::raw::{c_char, c_long};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::ptr;

    /// Opaque thread-safe engine handle. `Engine` is `Send + Sync`, so the same
    /// `*mut SekejapEngine` may be used concurrently from many threads (do not,
    /// however, call `sekejap_engine_close` while other threads are still using it).
    pub struct SekejapEngine {
        inner: Engine,
    }

    thread_local! {
        static ENGINE_ERR: RefCell<Option<CString>> = const { RefCell::new(None) };
    }

    fn set_err(msg: String) {
        ENGINE_ERR.with(|e| *e.borrow_mut() = CString::new(msg).ok());
    }
    fn clear_err() {
        ENGINE_ERR.with(|e| *e.borrow_mut() = None);
    }

    /// `&self` integer op with thread-local error + panic guard.
    fn guard_int<F>(e: *const SekejapEngine, f: F) -> c_long
    where
        F: FnOnce(&Engine) -> Result<c_long, String>,
    {
        if e.is_null() {
            set_err("engine handle is null".to_string());
            return -1;
        }
        let eng = unsafe { &(*e).inner };
        catch_unwind(AssertUnwindSafe(|| match f(eng) {
            Ok(v) => {
                clear_err();
                v
            }
            Err(msg) => {
                set_err(msg);
                -1
            }
        }))
        .unwrap_or(-1)
    }

    /// `&self` string op with thread-local error + panic guard.
    fn guard_str<F>(e: *const SekejapEngine, f: F) -> *mut c_char
    where
        F: FnOnce(&Engine) -> Result<String, String>,
    {
        if e.is_null() {
            set_err("engine handle is null".to_string());
            return ptr::null_mut();
        }
        let eng = unsafe { &(*e).inner };
        catch_unwind(AssertUnwindSafe(|| match f(eng) {
            Ok(s) => {
                clear_err();
                into_c_string(s)
            }
            Err(msg) => {
                set_err(msg);
                ptr::null_mut()
            }
        }))
        .unwrap_or(ptr::null_mut())
    }

    fn hits_to_json(hits: Vec<Hit>) -> Result<String, String> {
        let rows: Vec<Value> = hits
            .into_iter()
            .map(|h| h.payload.unwrap_or_else(|| json!({ "_slug": h.slug })))
            .collect();
        serde_json::to_string(&Value::Array(rows)).map_err(|e| e.to_string())
    }

    /// Open (or create) a thread-safe engine at `path`. Returns null on failure.
    ///
    /// # Safety
    /// `path` must be a valid null-terminated UTF-8 C string.
    #[no_mangle]
    pub unsafe extern "C" fn sekejap_engine_open(path: *const c_char) -> *mut SekejapEngine {
        catch_unwind(|| {
            let p = match cstr(path) {
                Some(s) => s,
                None => return ptr::null_mut(),
            };
            match Engine::builder(p).build() {
                Ok(inner) => Box::into_raw(Box::new(SekejapEngine { inner })),
                Err(e) => {
                    set_err(e);
                    ptr::null_mut()
                }
            }
        })
        .unwrap_or(ptr::null_mut())
    }

    /// Open an in-memory (ephemeral) thread-safe engine. Never fails → non-null.
    #[no_mangle]
    pub extern "C" fn sekejap_engine_open_memory() -> *mut SekejapEngine {
        catch_unwind(|| Box::into_raw(Box::new(SekejapEngine { inner: Engine::memory() })))
            .unwrap_or(ptr::null_mut())
    }

    /// Close an engine handle and free its resources. Safe with null. Do NOT call
    /// while other threads are still using this handle.
    ///
    /// # Safety
    /// `e` must be null or a handle from `sekejap_engine_open*`, not yet closed.
    #[no_mangle]
    pub unsafe extern "C" fn sekejap_engine_close(e: *mut SekejapEngine) {
        if e.is_null() {
            return;
        }
        let _ = catch_unwind(AssertUnwindSafe(|| drop(Box::from_raw(e))));
    }

    /// Run a `SELECT` — CONCURRENT-SAFE (takes a read lock; many run in parallel).
    /// Returns a heap JSON-array string, or null on error. Free with
    /// `sekejap_string_free`.
    ///
    /// # Safety
    /// `e` a live handle; `sql` valid null-terminated UTF-8.
    #[no_mangle]
    pub unsafe extern "C" fn sekejap_engine_query(
        e: *const SekejapEngine,
        sql: *const c_char,
    ) -> *mut c_char {
        guard_str(e, |eng| {
            let s = cstr(sql).ok_or_else(|| "sql is null or not valid UTF-8".to_string())?;
            hits_to_json(eng.query(s)?)
        })
    }

    /// Parameterized `SELECT` (positional `$1` from a JSON array). Concurrent-safe.
    ///
    /// # Safety
    /// `e` a live handle; `sql`/`params_json` valid UTF-8 (params_json may be null).
    #[no_mangle]
    pub unsafe extern "C" fn sekejap_engine_query_params(
        e: *const SekejapEngine,
        sql: *const c_char,
        params_json: *const c_char,
    ) -> *mut c_char {
        guard_str(e, |eng| {
            let s = cstr(sql).ok_or_else(|| "sql is null or not valid UTF-8".to_string())?;
            let params = parse_params(params_json).ok_or_else(|| "params_json is not valid JSON".to_string())?;
            hits_to_json(eng.query_params(s, &params)?)
        })
    }

    /// Run a mutating statement — writes are SERIALIZED (one writer at a time)
    /// while reads continue in parallel. Returns affected rows, or -1.
    ///
    /// # Safety
    /// `e` a live handle; `sql` valid null-terminated UTF-8.
    #[no_mangle]
    pub unsafe extern "C" fn sekejap_engine_execute(
        e: *const SekejapEngine,
        sql: *const c_char,
    ) -> c_long {
        guard_int(e, |eng| {
            let s = cstr(sql).ok_or_else(|| "sql is null or not valid UTF-8".to_string())?;
            eng.execute(s).map(|n| n as c_long)
        })
    }

    /// Parameterized mutating statement (injection-safe). Returns affected rows, or -1.
    ///
    /// # Safety
    /// `e` a live handle; `sql`/`params_json` valid UTF-8 (params_json may be null).
    #[no_mangle]
    pub unsafe extern "C" fn sekejap_engine_execute_params(
        e: *const SekejapEngine,
        sql: *const c_char,
        params_json: *const c_char,
    ) -> c_long {
        guard_int(e, |eng| {
            let s = cstr(sql).ok_or_else(|| "sql is null or not valid UTF-8".to_string())?;
            let params = parse_params(params_json).ok_or_else(|| "params_json is not valid JSON".to_string())?;
            eng.execute_params(s, &params).map(|n| n as c_long)
        })
    }

    /// Flush the write buffer to disk under one fsync (group commit). Returns the
    /// number of buffered rows committed, or -1.
    ///
    /// # Safety
    /// `e` must be a live handle.
    #[no_mangle]
    pub unsafe extern "C" fn sekejap_engine_flush(e: *const SekejapEngine) -> c_long {
        guard_int(e, |eng| eng.flush().map(|n| n as c_long))
    }

    /// Compact the engine's store. `0` on success, `-1` on error.
    ///
    /// # Safety
    /// `e` must be a live handle.
    #[no_mangle]
    pub unsafe extern "C" fn sekejap_engine_compact(e: *const SekejapEngine) -> i32 {
        guard_int(e, |eng| eng.compact().map(|_| 0)) as i32
    }

    /// Reclaim excess in-RAM capacity (cheap; never drops data/indexes).
    ///
    /// # Safety
    /// `e` must be a live handle.
    #[no_mangle]
    pub unsafe extern "C" fn sekejap_engine_trim_memory(e: *const SekejapEngine) {
        guard_int(e, |eng| {
            eng.trim_memory();
            Ok(0)
        });
    }

    /// The calling thread's last engine error as a heap C string, or null if the
    /// last engine call on THIS thread succeeded. Free with `sekejap_string_free`.
    /// (Thread-local — like `errno` — so concurrent callers don't clobber each other.)
    #[no_mangle]
    pub extern "C" fn sekejap_engine_last_error() -> *mut c_char {
        ENGINE_ERR.with(|e| match &*e.borrow() {
            Some(c) => into_c_string(c.to_string_lossy().into_owned()),
            None => ptr::null_mut(),
        })
    }
}

pub use engine_abi::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    // Drive the C ABI the way a foreign caller would: raw pointers, C strings,
    // explicit frees. Proves the round-trip works end to end.
    #[test]
    fn open_execute_query_close_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = CString::new(dir.path().to_str().unwrap()).unwrap();

        unsafe {
            let db = sekejap_open(path.as_ptr());
            assert!(!db.is_null(), "open failed");

            let ddl = CString::new(
                "CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)",
            )
            .unwrap();
            assert!(sekejap_execute(db, ddl.as_ptr()) >= 0);

            let ins = CString::new("INSERT INTO t (_key, v) VALUES ('a', 42)").unwrap();
            assert_eq!(sekejap_execute(db, ins.as_ptr()), 1);

            let q = CString::new("SELECT v FROM t WHERE _key = 'a'").unwrap();
            let out = sekejap_query(db, q.as_ptr());
            assert!(!out.is_null(), "query returned null");
            let json = CStr::from_ptr(out).to_str().unwrap();
            assert!(json.contains("42"), "unexpected result: {json}");
            sekejap_string_free(out);

            // A bad statement sets last_error and returns -1.
            let bad = CString::new("SELECT nope FROM").unwrap();
            assert_eq!(sekejap_execute(db, bad.as_ptr()), -1);
            let err = sekejap_last_error(db);
            assert!(!err.is_null(), "expected an error message");
            sekejap_string_free(err);

            sekejap_close(db);
        }
    }

    #[test]
    fn null_inputs_are_safe() {
        unsafe {
            assert!(sekejap_open(ptr::null()).is_null());
            assert_eq!(sekejap_execute(ptr::null_mut(), ptr::null()), -1);
            assert!(sekejap_query(ptr::null_mut(), ptr::null()).is_null());
            assert!(sekejap_last_error(ptr::null()).is_null());
            assert_eq!(sekejap_put(ptr::null_mut(), ptr::null(), ptr::null()), -1);
            assert_eq!(sekejap_link(ptr::null_mut(), ptr::null(), ptr::null(), ptr::null()), -1);
            assert_eq!(sekejap_node_count(ptr::null_mut()), -1);
            assert!(sekejap_collection_names(ptr::null_mut()).is_null());
            sekejap_trim_memory(ptr::null_mut()); // no-op, no crash
            sekejap_close(ptr::null_mut()); // no-op, no crash
            sekejap_string_free(ptr::null_mut()); // no-op, no crash
        }
    }

    // Exercise the expanded surface: direct put/link, params, introspection.
    #[test]
    fn extended_surface_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = CString::new(dir.path().to_str().unwrap()).unwrap();
        unsafe {
            let db = sekejap_open(path.as_ptr());
            assert!(!db.is_null());

            // Direct node inserts (no SQL).
            let a = CString::new("t/a").unwrap();
            let pa = CString::new(r#"{"_collection":"t","_key":"a","v":1}"#).unwrap();
            let b = CString::new("t/b").unwrap();
            let pb = CString::new(r#"{"_collection":"t","_key":"b","v":2}"#).unwrap();
            assert_eq!(sekejap_put(db, a.as_ptr(), pa.as_ptr()), 0);
            assert_eq!(sekejap_put(db, b.as_ptr(), pb.as_ptr()), 0);

            // Introspection.
            assert_eq!(sekejap_node_count(db), 2);
            let acheck = CString::new("t/a").unwrap();
            assert_eq!(sekejap_contains(db, acheck.as_ptr()), 1);
            let miss = CString::new("t/zzz").unwrap();
            assert_eq!(sekejap_contains(db, miss.as_ptr()), 0);

            // Edge + edge count.
            let ty = CString::new("near").unwrap();
            assert_eq!(sekejap_link(db, a.as_ptr(), b.as_ptr(), ty.as_ptr()), 0);
            assert_eq!(sekejap_edge_count(db), 1);

            // Parameterized query (injection-safe).
            let q = CString::new("SELECT v FROM t WHERE v = $1").unwrap();
            let params = CString::new("[2]").unwrap();
            let out = sekejap_query_params(db, q.as_ptr(), params.as_ptr());
            assert!(!out.is_null());
            let js = CStr::from_ptr(out).to_str().unwrap();
            assert!(js.contains('2') && !js.contains("\"v\":1"), "params filter: {js}");
            sekejap_string_free(out);

            // collection_names as JSON.
            let cn = sekejap_collection_names(db);
            assert!(!cn.is_null());
            assert!(CStr::from_ptr(cn).to_str().unwrap().contains("t"));
            sekejap_string_free(cn);

            // trim + remove.
            sekejap_trim_memory(db);
            assert_eq!(sekejap_remove(db, a.as_ptr()), 0);
            assert_eq!(sekejap_node_count(db), 1);

            sekejap_close(db);
        }
    }

    // Prepared statement via the C ABI: prepare once, run with different params.
    #[test]
    fn prepared_statement_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = CString::new(dir.path().to_str().unwrap()).unwrap();
        unsafe {
            let db = sekejap_open(path.as_ptr());
            assert!(!db.is_null());
            let ddl = CString::new("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)").unwrap();
            assert!(sekejap_execute(db, ddl.as_ptr()) >= 0);
            for i in 0..5 {
                let ins = CString::new(format!("INSERT INTO t (_key, v) VALUES ('k{i}', {i})")).unwrap();
                assert_eq!(sekejap_execute(db, ins.as_ptr()), 1);
            }

            // Compile once.
            let sql = CString::new("SELECT _key FROM t WHERE v = $1").unwrap();
            let stmt = sekejap_prepare(db, sql.as_ptr());
            assert!(!stmt.is_null(), "prepare failed");

            // Run with different parameter values → different rows.
            for i in 0..5 {
                let params = CString::new(format!("[{i}]")).unwrap();
                let out = sekejap_query_prepared(db, stmt, params.as_ptr());
                assert!(!out.is_null());
                let js = CStr::from_ptr(out).to_str().unwrap().to_string();
                assert!(js.contains(&format!("k{i}")), "param {i}: {js}");
                sekejap_string_free(out);
            }

            sekejap_stmt_free(stmt);
            sekejap_stmt_free(std::ptr::null_mut()); // null-safe
            sekejap_close(db);
        }
    }

    // The "build your own server" proof: one thread-safe engine handle, hit
    // concurrently by many reader threads AND a writer thread at the same time —
    // exactly what a server does. No data race (the handle is Send+Sync), reads
    // stay correct, writes land, and a final flush is durable.
        #[test]
    fn engine_handle_is_concurrent_and_safe() {
        use std::sync::Arc;
        use std::thread;

        unsafe {
            let e = sekejap_engine_open_memory();
            assert!(!e.is_null());

            let ddl = CString::new("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)").unwrap();
            assert!(sekejap_engine_execute(e, ddl.as_ptr()) >= 0);
            for i in 0..50 {
                let ins = CString::new(format!("INSERT INTO t (_key, v) VALUES ('k{i}', {i})")).unwrap();
                assert_eq!(sekejap_engine_execute(e, ins.as_ptr()), 1);
            }
            assert!(sekejap_engine_flush(e) >= 0);

            // Share the handle across threads as a plain address — a `usize` is
            // Send, and each thread casts it back. This is exactly how a C server
            // shares one engine pointer among worker threads. Safe because the
            // Engine behind it is Send + Sync.
            let addr = e as usize;
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

            // 4 reader threads hammering SELECT concurrently.
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    let done = done.clone();
                    thread::spawn(move || {
                        let eng = addr as *const SekejapEngine;
                        let q = CString::new("SELECT COUNT(*) AS n FROM t").unwrap();
                        while !done.load(std::sync::atomic::Ordering::Relaxed) {
                            let out = sekejap_engine_query(eng, q.as_ptr());
                            assert!(!out.is_null(), "concurrent read must not fail");
                            let js = CStr::from_ptr(out).to_str().unwrap().to_string();
                            sekejap_string_free(out);
                            assert!(js.contains("\"n\""), "malformed result: {js}");
                        }
                    })
                })
                .collect();

            // 1 writer thread inserting 50 more rows while readers run.
            let writer = thread::spawn(move || {
                let eng = addr as *const SekejapEngine;
                for i in 50..100 {
                    let ins = CString::new(format!("INSERT INTO t (_key, v) VALUES ('k{i}', {i})")).unwrap();
                    assert_eq!(sekejap_engine_execute(eng, ins.as_ptr()), 1);
                }
                assert!(sekejap_engine_flush(eng) >= 0);
            });

            writer.join().unwrap();
            done.store(true, std::sync::atomic::Ordering::Relaxed);
            for r in readers {
                r.join().unwrap();
            }

            // After everyone's done: exactly 100 rows, durable.
            assert!(sekejap_engine_flush(e) >= 0);
            let q = CString::new("SELECT COUNT(*) AS n FROM t").unwrap();
            let out = sekejap_engine_query(e, q.as_ptr());
            let js = CStr::from_ptr(out).to_str().unwrap().to_string();
            sekejap_string_free(out);
            assert!(js.contains("100"), "expected 100 rows after concurrent load: {js}");

            sekejap_engine_close(e);
        }
    }
}
