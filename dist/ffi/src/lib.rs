//! `sekejap-capi` -- the C ABI of sekejap, as `libsekejap`.
//!
//! One `extern "C"` surface over the published crate
//! (`docs/dist/RUST_API.md`), so Swift, Kotlin/JNI, Dart (`dart:ffi`), Go
//! (cgo) and plain C/C++ can drive the same handle a Rust application holds.
//! The contract is `docs/dist/C_ABI.md`; the header it generates is
//! `include/sekejap.h`.
//!
//! This layer adds NO execution. Every function below is one call on
//! `sekejap::Db` plus a JSON encode, and a construct sekejap has no atomic
//! for is REFUSED by name here exactly as it is there
//! (`docs/dist/RUST_API.md` §7), never emulated.
//!
//! # The rules a C caller can rely on
//!
//! - **Opaque handles.** `SekejapDb*`, `SekejapStmt*`, `SekejapTx*` and
//!   `SekejapScan*` are pointers to types C may not inspect. An open/begin
//!   call creates one; the matching close/free call destroys it. A handle is
//!   dangling after that call and must not be used again.
//! - **Strings in are borrowed.** Every `const char*` parameter is
//!   null-terminated UTF-8 owned by the caller and is never freed here.
//! - **Strings out are owned.** Every `char*` this library RETURNS was
//!   allocated here and is freed once with `sekejap_string_free`. The one
//!   exception is `sekejap_version`, which points into static program data
//!   and is typed `*const c_char` to say so: passing it to
//!   `sekejap_string_free` hands `CString::from_raw` a pointer the allocator
//!   never gave out, which corrupts the allocator rather than freeing
//!   anything.
//! - **Sentinels.** `NULL` for a failed pointer return, `-1` for a failed
//!   integer return. A few calls answer `NULL` for a clean MISS and not an
//!   error (`sekejap_get`, `sekejap_describe`, `sekejap_scan_next` at
//!   the end of a walk, `sekejap_next_change` with nothing queued);
//!   `sekejap_last_error_code` tells the two apart without parsing text,
//!   because a miss leaves it `SekejapStatus_Ok`.
//! - **Errors are thread-local.** `sekejap_last_error` and
//!   `sekejap_last_error_code` read a slot owned by the CALLING THREAD, so
//!   a failed `sekejap_open` -- which has no handle to carry a message --
//!   still reports. They take the handle for source compatibility with e1
//!   and ignore it.
//! - **No panic crosses the boundary.** Every entry point is wrapped in
//!   `catch_unwind`; a panic becomes the failure sentinel with
//!   `SekejapStatus_Unknown`, never undefined behaviour.
//!
//! # Threading
//!
//! `sekejap::Db` is `Send + Sync` in both modes, so `SekejapDb*` MAY be
//! shared across threads: two threads may call `sekejap_query` on one
//! handle at the same time. The derived handles are not shared work: a
//! `SekejapScan*`, a `SekejapStmt*` and a `SekejapTx*` are each used from
//! ONE thread at a time.
//!
//! A `SekejapTx*` holds the writer for its whole life. While one is open, no
//! other call on the same `SekejapDb*` -- from this thread or another -- can
//! take the writer, and a call that needs it WAITS. Commit or roll the
//! transaction back before using the handle for anything else.
//!
//! Every derived handle borrows its database: free the `SekejapStmt*`,
//! `SekejapScan*` and `SekejapTx*` BEFORE `sekejap_close`.

// The published crate, under its own name. The dependency is renamed in
// `Cargo.toml` so that it and this crate's `libsekejap` lib target do not
// claim one name in the test binaries; nothing below has to know that.
use sekejap_rs as sekejap;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_long};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::Mutex;
use std::time::Duration;

use sekejap::core::collections::Error as CoreError;
use sekejap::core::{Config, IoMode, SyncMode};
use sekejap::dist::service::{ChangeKind, Receiver, SubscriptionId};
use sekejap::{Db, Direction, Error, FieldKind, IndexFamily, Scan, Statement, Tx};
use sekejap::{SqlError, Storage};
use serde_json::{json, Map, Value};

// ── Status, direction ───────────────────────────────────────────────────────

/// Why the last call on this thread failed, as a small closed enumeration a
/// wrapper can map without parsing the message text.
///
/// `SekejapStatus_Ok` is also what a clean MISS leaves behind: a `NULL`
/// from `sekejap_get` with `Ok` is "no such row", and a `NULL` with
/// anything else is a failure whose sentence `sekejap_last_error` carries.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SekejapStatus {
    /// The last call succeeded, or answered a clean miss.
    Ok = 0,
    /// A construct sekejap has no atomic for, named with its reason: a
    /// Tier-2/Tier-3 statement, an in-memory open, a payload-rewriting
    /// compact, a service call on a single-mode handle.
    Refused = 1,
    /// A page or a log failed verification. Nothing was changed.
    Corrupt = 2,
    /// A format, policy or configuration this build does not implement.
    Unsupported = 3,
    /// The directory, the file or the medium refused.
    Io = 4,
    /// The caller's arguments are wrong: a null pointer, text that is not
    /// UTF-8, JSON that does not parse, a collection that is not in the
    /// catalog, a parameter of the wrong type.
    Invalid = 5,
    /// A bound refused rather than waiting: a work budget, a statement
    /// deadline, a cancel, a second writer, a reader slot.
    Busy = 6,
    /// The named row is not in the collection, on a call that needs it to
    /// exist -- an edge endpoint, for instance.
    UnknownRow = 7,
    /// Nothing above classified it, including a panic caught at the
    /// boundary. The message is still in `sekejap_last_error`.
    Unknown = 8,
}

/// Which way an edge points, for `sekejap_neighbours`.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SekejapDirection {
    /// Edges that leave the row.
    Outgoing = 0,
    /// Edges that arrive at the row.
    Incoming = 1,
    /// Both, with each neighbour reported once.
    Both = 2,
}

/// `sekejap_stmt_rebindable`: the statement has not been bound yet, so
/// there is nothing to answer. It is compiled by its first
/// `sekejap_stmt_query` or `sekejap_stmt_execute`, and rebindable is
/// known from then on. Not a failure: `-1` is.
pub const SEKEJAP_REBIND_UNBOUND: i32 = 2;

// ── The error slot ──────────────────────────────────────────────────────────

struct Slot {
    message: Option<String>,
    code: SekejapStatus,
}

thread_local! {
    static LAST: std::cell::RefCell<Slot> = const {
        std::cell::RefCell::new(Slot { message: None, code: SekejapStatus::Ok })
    };
}

fn clear_error() {
    LAST.with(|slot| {
        let mut slot = slot.borrow_mut();
        slot.message = None;
        slot.code = SekejapStatus::Ok;
    });
}

fn set_error(code: SekejapStatus, message: impl Into<String>) {
    LAST.with(|slot| {
        let mut slot = slot.borrow_mut();
        slot.message = Some(message.into());
        slot.code = code;
    });
}

fn fail(error: &Error) {
    set_error(status_of(error), error.to_string());
}

/// A refusal raised by this layer rather than by the crate under it. Both
/// halves are required, exactly as `sekejap::Error::Refused` requires them.
fn refuse(construct: &str, reason: &str) {
    set_error(
        SekejapStatus::Refused,
        format!("{construct} refused: {reason}"),
    );
}

/// The caller's own mistake: a null pointer, text that is not UTF-8, JSON
/// that does not parse.
fn invalid(message: impl Into<String>) {
    set_error(SekejapStatus::Invalid, message);
}

/// One `sekejap::Error` as one status code. Total by construction: every
/// variant of every error the published crate can return lands on exactly
/// one of the nine.
fn status_of(error: &Error) -> SekejapStatus {
    match error {
        Error::Refused { .. } => SekejapStatus::Refused,
        Error::Sql(e) => status_of_sql(e),
        Error::Engine(e) => status_of_core(e),
        Error::Query(e) => match e {
            sekejap::core::collections::QueryError::Database(e) => status_of_core(e),
            _ => SekejapStatus::Busy,
        },
        Error::Service(e) => match e {
            sekejap::dist::service::ServiceError::Core(e) => status_of_core(e),
            sekejap::dist::service::ServiceError::Sql(e) => status_of_sql(e),
            sekejap::dist::service::ServiceError::Query(q) => match q {
                sekejap::core::collections::QueryError::Database(e) => status_of_core(e),
                _ => SekejapStatus::Busy,
            },
            sekejap::dist::service::ServiceError::Refused(_) => SekejapStatus::Refused,
        },
        Error::Io(_) => SekejapStatus::Io,
        Error::UnknownCollection(_) => SekejapStatus::Invalid,
        Error::UnknownRow { .. } => SekejapStatus::UnknownRow,
    }
}

fn status_of_sql(error: &SqlError) -> SekejapStatus {
    match error {
        SqlError::Refused { .. } => SekejapStatus::Refused,
        SqlError::Syntax { .. } | SqlError::Parameter(_) => SekejapStatus::Invalid,
        SqlError::Unsupported(_) => SekejapStatus::Unsupported,
        SqlError::Engine(_) => SekejapStatus::Invalid,
    }
}

fn status_of_core(error: &CoreError) -> SekejapStatus {
    match error {
        CoreError::InvalidInput(_) | CoreError::NotFound(_) | CoreError::AlreadyExists => {
            SekejapStatus::Invalid
        }
        CoreError::ReadOnly => SekejapStatus::Refused,
        CoreError::Corrupt(_) => SekejapStatus::Corrupt,
        CoreError::Unsupported(_) => SekejapStatus::Unsupported,
        CoreError::Cancelled | CoreError::BudgetExceeded { .. } => SekejapStatus::Busy,
        CoreError::Failed => SekejapStatus::Unknown,
        CoreError::Kernel(e) => status_of_kernel(e),
    }
}

fn status_of_kernel(error: &sekejap::core::KernelError) -> SekejapStatus {
    use sekejap::core::KernelError as K;
    match error {
        K::Io(_) => SekejapStatus::Io,
        K::Corrupt { .. } | K::CorruptWal { .. } | K::StorePoisoned => SekejapStatus::Corrupt,
        K::ReadOnly => SekejapStatus::Refused,
        K::WriterLocked | K::OutOfBudget | K::ResourceLimit(_) => SekejapStatus::Busy,
        K::TooLarge | K::DuplicateKey | K::RangeNotEmpty => SekejapStatus::Invalid,
        _ => SekejapStatus::Unknown,
    }
}

// ── Borrowing and lending strings ───────────────────────────────────────────

/// Borrow a C string as `&str`. `None` for a null pointer or bytes that are
/// not UTF-8; the caller decides which of the two it was.
///
/// # Safety
/// `p` must be null or point at a null-terminated C string that outlives the
/// borrow.
unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok()
}

/// A required `const char*` argument, or an `Invalid` error naming it.
///
/// # Safety
/// As `cstr`.
unsafe fn required<'a>(p: *const c_char, name: &str) -> Option<&'a str> {
    match cstr(p) {
        Some(s) => Some(s),
        None if p.is_null() => {
            invalid(format!("`{name}` is NULL; it is required"));
            None
        }
        None => {
            invalid(format!("`{name}` is not valid UTF-8"));
            None
        }
    }
}

/// Move a Rust `String` onto the heap as a C string the caller owns. `NULL`
/// if it holds an interior NUL, which cannot be a C string.
fn lend(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => {
            invalid("the answer holds an interior NUL byte and cannot be a C string");
            ptr::null_mut()
        }
    }
}

/// One JSON answer as the `Ok(Some(..))` a string guard wants. The encode
/// can only fail on a non-finite float, which `serde_json` reports rather
/// than writing as a token no JSON reader accepts.
fn encode(value: &Value) -> Result<Option<String>, ()> {
    serde_json::to_string(value).map(Some).map_err(|e| {
        set_error(SekejapStatus::Unknown, e.to_string());
    })
}

/// The parameter list of a statement: a JSON ARRAY. A null or empty pointer
/// is no parameters. Any other single JSON value is a one-element list,
/// which is the rule e1 stated and every wrapper already follows.
///
/// # Safety
/// As `cstr`.
unsafe fn params(p: *const c_char) -> Option<Vec<Value>> {
    let text = match cstr(p) {
        None if p.is_null() => return Some(Vec::new()),
        None => {
            invalid("`params` is not valid UTF-8");
            return None;
        }
        Some(s) if s.trim().is_empty() => return Some(Vec::new()),
        Some(s) => s,
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(items)) => Some(items),
        Ok(other) => Some(vec![other]),
        Err(e) => {
            invalid(format!("`params` is not JSON: {e}"));
            None
        }
    }
}

/// A required JSON argument, parsed.
///
/// # Safety
/// As `cstr`.
unsafe fn json_arg(p: *const c_char, name: &str) -> Option<Value> {
    let text = required(p, name)?;
    match serde_json::from_str::<Value>(text) {
        Ok(value) => Some(value),
        Err(e) => {
            invalid(format!("`{name}` is not JSON: {e}"));
            None
        }
    }
}

// ── The guards ──────────────────────────────────────────────────────────────

/// Run `body` for an integer answer. `-1` on failure, and the thread-local
/// slot carries why.
fn guard_int(body: impl FnOnce() -> Result<c_long, ()>) -> c_long {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(value)) => value,
        Ok(Err(())) => -1,
        Err(_) => {
            set_error(SekejapStatus::Unknown, "a panic was caught at the C boundary");
            -1
        }
    }
}

/// Run `body` for a small integer answer (a boolean, a status).
fn guard_i32(body: impl FnOnce() -> Result<i32, ()>) -> i32 {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(value)) => value,
        Ok(Err(())) => -1,
        Err(_) => {
            set_error(SekejapStatus::Unknown, "a panic was caught at the C boundary");
            -1
        }
    }
}

/// Run `body` for a string answer. `Ok(None)` is a clean MISS: `NULL` with
/// the slot left `SekejapStatus_Ok`.
fn guard_str(body: impl FnOnce() -> Result<Option<String>, ()>) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(Some(text))) => lend(text),
        Ok(Ok(None)) => ptr::null_mut(),
        Ok(Err(())) => ptr::null_mut(),
        Err(_) => {
            set_error(SekejapStatus::Unknown, "a panic was caught at the C boundary");
            ptr::null_mut()
        }
    }
}

/// Run `body` for a handle answer. `NULL` on failure.
fn guard_ptr<T>(body: impl FnOnce() -> Result<*mut T, ()>) -> *mut T {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(value)) => value,
        Ok(Err(())) => ptr::null_mut(),
        Err(_) => {
            set_error(SekejapStatus::Unknown, "a panic was caught at the C boundary");
            ptr::null_mut()
        }
    }
}

/// `Result` from the published crate, with the error recorded.
fn record<T>(out: sekejap::Result<T>) -> Result<T, ()> {
    out.map_err(|e| fail(&e))
}

// ── The handles ─────────────────────────────────────────────────────────────

/// An open database. Created by `sekejap_open`,
/// `sekejap_open_with_config` or `sekejap_open_service`; destroyed by
/// `sekejap_close`. `Send + Sync`: it MAY be shared across threads.
pub struct SekejapDb {
    db: Db,
    /// The change-feed subscriptions this handle holds open, by the id
    /// `sekejap_subscribe` returned. A `Receiver` is owned by one reader,
    /// so the handle owns them and hands out ids instead of pointers.
    subscriptions: Mutex<BTreeMap<u64, Receiver>>,
}

/// One statement, parsed once at `sekejap_prepare` and compiled by its
/// first bind. Freed with `sekejap_stmt_free`, which must happen BEFORE
/// `sekejap_close` of the database it was prepared on.
pub struct SekejapStmt {
    inner: Statement<'static>,
}

/// The writer, held across many writes. Created by `sekejap_tx_begin` and
/// consumed by `sekejap_tx_commit` or `sekejap_tx_rollback`; a handle
/// dropped any other way ROLLS BACK.
pub struct SekejapTx {
    inner: Option<Tx<'static>>,
}

/// A paged walk: of one collection (`sekejap_scan_open`) or of one
/// statement's answer (`sekejap_query_open`). Freed with
/// `sekejap_scan_close`, which `sekejap_query_close` is another name
/// for.
pub struct SekejapScan {
    inner: Walk,
    page_rows: usize,
}

enum Walk {
    /// The lazy collection walk: at most `page_rows` rows are held.
    Collection(Scan<'static>),
    /// The rows a statement answered, encoded once at
    /// `sekejap_query_open` and handed over `page_rows` at a time.
    Answer(VecDeque<Value>),
}

// ── §1 opening ──────────────────────────────────────────────────────────────

/// Open the database in `path`, creating it when the directory holds none.
/// `NULL` on failure, with `sekejap_last_error` set on this thread.
///
/// # Safety
/// `path` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_open(path: *const c_char) -> *mut SekejapDb {
    guard_ptr(|| {
        clear_error();
        let path = required(path, "path").ok_or(())?;
        let db = record(Db::open(path))?;
        Ok(boxed(db))
    })
}

/// `sekejap_open` under a store configuration, given as a JSON object:
/// `{"budget_bytes": 268435456, "io": "buffered"|"direct",
/// "sync": "full"|"normal"|"off"}`. Every member is optional and an absent
/// one keeps sekejap's own default.
///
/// The default `sync` is `"normal"`: every acknowledged commit is pushed out
/// of the page cache with a data barrier (`fdatasync` on Linux, plain
/// `fsync` on macOS), so it survives a process crash and an operating-system
/// panic. It does NOT flush the drive's own write cache, so a power cut can
/// lose recently acknowledged commits. This is the guarantee SQLite gives
/// with `fullfsync` off and PostgreSQL with a plain `fsync`.
///
/// `"sync": "full"` asks for the drive-cache barrier instead
/// (`fcntl(F_FULLFSYNC)` on macOS, `fsync` elsewhere), which survives a power
/// cut as well and costs 11.9 ms against 1.45 ms for `"normal"` on the
/// volume the write-path measurements were taken on. `"sync": "off"` issues
/// no barrier at all.
///
/// # Safety
/// Both pointers must be valid null-terminated UTF-8 C strings, or
/// `config_json` may be NULL for the defaults.
#[no_mangle]
pub unsafe extern "C" fn sekejap_open_with_config(
    path: *const c_char,
    config_json: *const c_char,
) -> *mut SekejapDb {
    guard_ptr(|| {
        clear_error();
        let path = required(path, "path").ok_or(())?;
        let config = config_of(config_json)?;
        let db = record(Db::open_with(path, config))?;
        Ok(boxed(db))
    })
}

/// Open in SERVICE mode: one writer, parallel readers on a published
/// snapshot, and the change feed, the statement timeout and the cancel of
/// `docs/dist/OPS_CONTRACT.md` §1-§5. The `sekejap_subscribe`,
/// `sekejap_cancel` and `sekejap_statement_timeout_ms` family answers only
/// on a handle opened this way.
///
/// # Safety
/// `path` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_open_service(path: *const c_char) -> *mut SekejapDb {
    guard_ptr(|| {
        clear_error();
        let path = required(path, "path").ok_or(())?;
        let db = record(Db::open_service(path))?;
        Ok(boxed(db))
    })
}

/// Close the handle and free it. Null-safe. Uncommitted work is discarded:
/// a close is not a commit. Every `SekejapStmt*`, `SekejapScan*` and
/// `SekejapTx*` taken from this handle must be freed FIRST.
///
/// # Safety
/// `db` must be null or a handle from an open call that has not been closed.
#[no_mangle]
pub unsafe extern "C" fn sekejap_close(db: *mut SekejapDb) {
    if db.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let handle = Box::from_raw(db);
        // Drop the subscriptions before the database they are fed by.
        drop(handle.subscriptions.into_inner().unwrap_or_else(|e| e.into_inner()));
        let _ = handle.db.close();
    }));
}

/// The library version, as `MAJOR.MINOR.PATCH`. STATIC: do not pass it to
/// `sekejap_string_free`.
#[no_mangle]
pub extern "C" fn sekejap_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// The sekejap disk format this build reads and writes: 2. A file that
/// carries anything else is refused by name with nothing changed
/// (`docs/core/FORMAT_V2.md`).
#[no_mangle]
pub extern "C" fn sekejap_format_version() -> i32 {
    sekejap::FORMAT_VERSION as i32
}

fn boxed(db: Db) -> *mut SekejapDb {
    Box::into_raw(Box::new(SekejapDb {
        db,
        subscriptions: Mutex::new(BTreeMap::new()),
    }))
}

/// # Safety
/// As `cstr`.
unsafe fn config_of(config_json: *const c_char) -> Result<Config, ()> {
    let mut config = Db::config();
    if config_json.is_null() {
        return Ok(config);
    }
    let Some(value) = json_arg(config_json, "config") else {
        return Err(());
    };
    let Value::Object(members) = value else {
        invalid("`config` must be a JSON object");
        return Err(());
    };
    for (name, value) in &members {
        match name.as_str() {
            "budget_bytes" => match value.as_u64() {
                Some(bytes) => config.budget_bytes = bytes as usize,
                None => {
                    invalid("`config.budget_bytes` must be a whole number of bytes");
                    return Err(());
                }
            },
            "io" => match value.as_str() {
                Some("buffered") => config.io = IoMode::Buffered,
                Some("direct") => config.io = IoMode::Direct,
                _ => {
                    invalid("`config.io` must be \"buffered\" or \"direct\"");
                    return Err(());
                }
            },
            "sync" => match value.as_str() {
                Some("full") => config.sync = SyncMode::Full,
                Some("normal") => config.sync = SyncMode::Normal,
                Some("off") => config.sync = SyncMode::Off,
                _ => {
                    invalid("`config.sync` must be \"full\", \"normal\" or \"off\"");
                    return Err(());
                }
            },
            other => {
                invalid(format!(
                    "`config.{other}` is not a store setting; the three are budget_bytes, io and sync"
                ));
                return Err(());
            }
        }
    }
    Ok(config)
}

// ── §2 errors ───────────────────────────────────────────────────────────────

/// The message for the last failure ON THIS THREAD, as a heap C string the
/// caller frees with `sekejap_string_free`. `NULL` when the last call
/// succeeded. The handle is accepted and ignored: the slot is thread-local,
/// so a failed open with no handle still reports.
///
/// # Safety
/// `db` may be NULL; when it is not, it must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_last_error(db: *const SekejapDb) -> *mut c_char {
    let _ = db;
    match catch_unwind(|| LAST.with(|slot| slot.borrow().message.clone())) {
        Ok(Some(message)) => match CString::new(message) {
            Ok(c) => c.into_raw(),
            Err(_) => ptr::null_mut(),
        },
        _ => ptr::null_mut(),
    }
}

/// The code for the last failure ON THIS THREAD, for a wrapper that maps
/// without parsing the message. `SekejapStatus_Ok` after a success and
/// after a clean miss.
///
/// # Safety
/// `db` may be NULL; when it is not, it must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_last_error_code(db: *const SekejapDb) -> SekejapStatus {
    let _ = db;
    catch_unwind(|| LAST.with(|slot| slot.borrow().code)).unwrap_or(SekejapStatus::Unknown)
}

/// Free a string this library returned. Null-safe, and exactly once.
/// NEVER pass `sekejap_version`, which is static.
///
/// # Safety
/// `s` must be null or a pointer this library returned and that has not been
/// freed.
#[no_mangle]
pub unsafe extern "C" fn sekejap_string_free(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    drop(CString::from_raw(s));
}

// ── §3 documents ────────────────────────────────────────────────────────────

/// # Safety
/// `db` must be a live handle.
unsafe fn handle<'a>(db: *const SekejapDb) -> Option<&'a SekejapDb> {
    if db.is_null() {
        invalid("`db` is NULL");
        return None;
    }
    Some(&*db)
}

/// Write one document, committed before this call returns. `0` on success,
/// `-1` on failure. `document_json` must be a JSON object; a `_key` member
/// in it must equal `key`.
///
/// # Safety
/// Every pointer must be a valid null-terminated UTF-8 C string and `db` a
/// live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_put(
    db: *mut SekejapDb,
    collection: *const c_char,
    key: *const c_char,
    document_json: *const c_char,
) -> i32 {
    guard_i32(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let collection = required(collection, "collection").ok_or(())?;
        let key = required(key, "key").ok_or(())?;
        let document = json_arg(document_json, "document").ok_or(())?;
        record(handle.db.put((collection, key), &document))?;
        Ok(0)
    })
}

/// Write many documents into one collection under ONE commit. `rows_json`
/// is a JSON array of `{"key": "...", "doc": { ... }}`. Returns the rows
/// written, or `-1`; a failure stores NONE of the batch.
///
/// # Safety
/// As `sekejap_put`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_put_many(
    db: *mut SekejapDb,
    collection: *const c_char,
    rows_json: *const c_char,
) -> c_long {
    guard_int(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let collection = required(collection, "collection").ok_or(())?;
        let rows = json_arg(rows_json, "rows").ok_or(())?;
        let Value::Array(items) = rows else {
            invalid("`rows` must be a JSON array of {\"key\": ..., \"doc\": ...}");
            return Err(());
        };
        let mut batch: Vec<(String, Value)> = Vec::with_capacity(items.len());
        for (at, item) in items.into_iter().enumerate() {
            let Some(object) = item.as_object() else {
                invalid(format!("`rows[{at}]` must be a JSON object"));
                return Err(());
            };
            let Some(Value::String(key)) = object.get("key") else {
                invalid(format!("`rows[{at}].key` must be a string"));
                return Err(());
            };
            let Some(doc) = object.get("doc") else {
                invalid(format!("`rows[{at}].doc` is missing"));
                return Err(());
            };
            batch.push((key.clone(), doc.clone()));
        }
        let written = record(handle.db.put_many(collection, batch))?;
        Ok(written as c_long)
    })
}

/// Read one document, with `_key` set, as a heap JSON object the caller
/// frees. `NULL` with `SekejapStatus_Ok` is a MISS; `NULL` with anything
/// else is a failure.
///
/// # Safety
/// As `sekejap_put`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_get(
    db: *mut SekejapDb,
    collection: *const c_char,
    key: *const c_char,
) -> *mut c_char {
    guard_str(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let collection = required(collection, "collection").ok_or(())?;
        let key = required(key, "key").ok_or(())?;
        match record(handle.db.get((collection, key)))? {
            Some(document) => encode(&document),
            None => Ok(None),
        }
    })
}

/// Whether the row is there: `1`, `0`, or `-1` on failure.
///
/// # Safety
/// As `sekejap_put`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_exists(
    db: *mut SekejapDb,
    collection: *const c_char,
    key: *const c_char,
) -> i32 {
    guard_i32(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let collection = required(collection, "collection").ok_or(())?;
        let key = required(key, "key").ok_or(())?;
        Ok(i32::from(record(handle.db.exists((collection, key)))?))
    })
}

/// Delete one row and every edge that touches it, committed. `1` if it was
/// there, `0` if it was not, `-1` on failure.
///
/// # Safety
/// As `sekejap_put`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_delete(
    db: *mut SekejapDb,
    collection: *const c_char,
    key: *const c_char,
) -> i32 {
    guard_i32(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let collection = required(collection, "collection").ok_or(())?;
        let key = required(key, "key").ok_or(())?;
        Ok(i32::from(record(handle.db.delete((collection, key)))?))
    })
}

/// Open a walk of one collection in stable id order. The walk holds at most
/// `page_rows` rows at a time (`0` means sekejap's default of 256), and
/// takes the read lock once per page rather than once for the collection.
/// `NULL` on failure. Free with `sekejap_scan_close`, BEFORE
/// `sekejap_close`.
///
/// # Safety
/// `db` must be a live handle and `collection` a valid C string. The
/// returned handle borrows `db`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_scan_open(
    db: *mut SekejapDb,
    collection: *const c_char,
    page_rows: usize,
) -> *mut SekejapScan {
    guard_ptr(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let collection = required(collection, "collection").ok_or(())?;
        let page_rows = if page_rows == 0 {
            sekejap::SCAN_PAGE
        } else {
            page_rows
        };
        let scan = record(handle.db.scan(collection))?.page_size(page_rows);
        // The walk borrows the `Db` inside the box the caller holds. The C
        // rule that the scan is closed before the database is what keeps
        // that borrow true, and is stated in the header.
        let scan: Scan<'static> = std::mem::transmute::<Scan<'_>, Scan<'static>>(scan);
        Ok(Box::into_raw(Box::new(SekejapScan {
            inner: Walk::Collection(scan),
            page_rows,
        })))
    })
}

/// The next page of a walk, as a heap JSON array of documents (each with
/// `_key`). `NULL` with `SekejapStatus_Ok` is the END of the walk;
/// `NULL` with anything else is a failure. Free the string with
/// `sekejap_string_free`.
///
/// # Safety
/// `scan` must be a live handle from `sekejap_scan_open` or
/// `sekejap_query_open`, used from one thread at a time.
#[no_mangle]
pub unsafe extern "C" fn sekejap_scan_next(scan: *mut SekejapScan) -> *mut c_char {
    next_page(scan)
}

/// Close a walk and free it. Null-safe.
///
/// # Safety
/// `scan` must be null or a live handle that has not been closed.
#[no_mangle]
pub unsafe extern "C" fn sekejap_scan_close(scan: *mut SekejapScan) {
    if scan.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| drop(Box::from_raw(scan))));
}

/// # Safety
/// As `sekejap_scan_next`.
unsafe fn next_page(scan: *mut SekejapScan) -> *mut c_char {
    guard_str(|| {
        clear_error();
        if scan.is_null() {
            invalid("`scan` is NULL");
            return Err(());
        }
        let scan = &mut *scan;
        let page_rows = scan.page_rows;
        let mut page: Vec<Value> = Vec::new();
        match &mut scan.inner {
            Walk::Collection(walk) => {
                for row in walk.by_ref().take(page_rows) {
                    let row = record(row)?;
                    page.push(row.fields);
                }
            }
            Walk::Answer(rows) => {
                while page.len() < page_rows {
                    match rows.pop_front() {
                        Some(row) => page.push(row),
                        None => break,
                    }
                }
            }
        }
        if page.is_empty() {
            return Ok(None);
        }
        encode(&Value::Array(page))
    })
}

// ── §4 SQL ──────────────────────────────────────────────────────────────────

/// Run one writing statement and commit. Returns the rows it moved; a
/// statement that only raises a notice returns `0`. `-1` on failure, which
/// includes a Tier-2/Tier-3 construct REFUSED by name.
///
/// # Safety
/// `db` must be a live handle, `sql` a valid C string, `params_json` NULL or
/// a JSON array.
#[no_mangle]
pub unsafe extern "C" fn sekejap_execute(
    db: *mut SekejapDb,
    sql: *const c_char,
    params_json: *const c_char,
) -> c_long {
    guard_int(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let sql = required(sql, "sql").ok_or(())?;
        let params = params(params_json).ok_or(())?;
        Ok(record(handle.db.execute(sql, &params))? as c_long)
    })
}

/// Run one row-returning statement. The answer is a heap JSON ARRAY of
/// objects keyed by COLUMN NAME; a column that is MISSING in a row is
/// omitted from that object, because missing is not null. `NULL` on
/// failure. Free with `sekejap_string_free`.
///
/// # Safety
/// As `sekejap_execute`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_query(
    db: *mut SekejapDb,
    sql: *const c_char,
    params_json: *const c_char,
) -> *mut c_char {
    guard_str(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let sql = required(sql, "sql").ok_or(())?;
        let params = params(params_json).ok_or(())?;
        let rows = record(handle.db.query(sql, &params))?;
        let answer: Vec<Value> = rows.iter().map(|r| Value::Object(r.to_object())).collect();
        encode(&Value::Array(answer))
    })
}

/// The plan the engine would build for one statement, as heap text. `NULL`
/// on failure.
///
/// # Safety
/// As `sekejap_execute`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_explain(
    db: *mut SekejapDb,
    sql: *const c_char,
    params_json: *const c_char,
) -> *mut c_char {
    guard_str(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let sql = required(sql, "sql").ok_or(())?;
        let params = params(params_json).ok_or(())?;
        Ok(Some(record(handle.db.explain(sql, &params))?))
    })
}

/// Prepare one statement. It is PARSED here -- a syntax error is reported
/// now -- and compiled by its first bind. `NULL` on failure. Free with
/// `sekejap_stmt_free`, BEFORE `sekejap_close`.
///
/// # Safety
/// `db` must be a live handle. The returned handle borrows it.
#[no_mangle]
pub unsafe extern "C" fn sekejap_prepare(
    db: *mut SekejapDb,
    sql: *const c_char,
) -> *mut SekejapStmt {
    guard_ptr(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let sql = required(sql, "sql").ok_or(())?;
        let statement = record(handle.db.prepare(sql))?;
        let statement: Statement<'static> =
            std::mem::transmute::<Statement<'_>, Statement<'static>>(statement);
        Ok(Box::into_raw(Box::new(SekejapStmt { inner: statement })))
    })
}

/// Run a prepared statement as a row-returning one. Same JSON shape as
/// `sekejap_query`. `NULL` on failure.
///
/// # Safety
/// `stmt` must be a live handle from `sekejap_prepare` whose database is
/// still open, used from one thread at a time.
#[no_mangle]
pub unsafe extern "C" fn sekejap_stmt_query(
    stmt: *mut SekejapStmt,
    params_json: *const c_char,
) -> *mut c_char {
    guard_str(|| {
        clear_error();
        if stmt.is_null() {
            invalid("`stmt` is NULL");
            return Err(());
        }
        let stmt = &mut *stmt;
        let params = params(params_json).ok_or(())?;
        let rows = record(stmt.inner.query_with(&params))?;
        let answer: Vec<Value> = rows.iter().map(|r| Value::Object(r.to_object())).collect();
        encode(&Value::Array(answer))
    })
}

/// Run a prepared statement as a writing one and commit. Returns the rows it
/// moved, or `-1`.
///
/// # Safety
/// As `sekejap_stmt_query`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_stmt_execute(
    stmt: *mut SekejapStmt,
    params_json: *const c_char,
) -> c_long {
    guard_int(|| {
        clear_error();
        if stmt.is_null() {
            invalid("`stmt` is NULL");
            return Err(());
        }
        let stmt = &mut *stmt;
        let params = params(params_json).ok_or(())?;
        Ok(record(stmt.inner.execute_with(&params))? as c_long)
    })
}

/// Whether a further bind of this statement compiles nothing: `1` yes,
/// `0` no, `SEKEJAP_REBIND_UNBOUND` (`2`) not bound yet, `-1` on failure.
///
/// A writing statement is never rebindable -- its document is folded at
/// compile -- and says so here rather than pretending.
///
/// # Safety
/// As `sekejap_stmt_query`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_stmt_rebindable(stmt: *const SekejapStmt) -> i32 {
    guard_i32(|| {
        clear_error();
        if stmt.is_null() {
            invalid("`stmt` is NULL");
            return Err(());
        }
        Ok(match (*stmt).inner.rebindable() {
            Some(true) => 1,
            Some(false) => 0,
            None => SEKEJAP_REBIND_UNBOUND,
        })
    })
}

/// Free a prepared statement. Null-safe.
///
/// # Safety
/// `stmt` must be null or a live handle that has not been freed.
#[no_mangle]
pub unsafe extern "C" fn sekejap_stmt_free(stmt: *mut SekejapStmt) {
    if stmt.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| drop(Box::from_raw(stmt))));
}

/// Run a row-returning statement and open a PAGED DELIVERY of its answer:
/// `sekejap_query_next` hands back at most `page_rows` rows per call
/// (`0` means 4,096), so no single `char*` holds the whole answer.
///
/// What is paged and what is not, stated rather than implied: the ENGINE
/// pages the execution at `page_rows` rows, which is `Db::stream`; the
/// ANSWER is assembled here, because a compiled SELECT owns what its request
/// borrows and therefore cannot be suspended between two C calls
/// (`docs/dist/RUST_API.md` §3). This bounds the string per call and lets a
/// caller stop reading; it does not bound the answer.
///
/// `NULL` on failure. Free with `sekejap_query_close`.
///
/// # Safety
/// As `sekejap_execute`. The returned handle borrows `db`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_query_open(
    db: *mut SekejapDb,
    sql: *const c_char,
    params_json: *const c_char,
    page_rows: usize,
) -> *mut SekejapScan {
    guard_ptr(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let sql = required(sql, "sql").ok_or(())?;
        let params = params(params_json).ok_or(())?;
        let page_rows = if page_rows == 0 {
            sekejap::QUERY_PAGE
        } else {
            page_rows
        };
        let mut rows: VecDeque<Value> = VecDeque::new();
        record(
            handle
                .db
                .stream(sql, &params, page_rows, &mut |row| {
                    rows.push_back(Value::Object(row.to_object()));
                    Ok(())
                }),
        )?;
        Ok(Box::into_raw(Box::new(SekejapScan {
            inner: Walk::Answer(rows),
            page_rows,
        })))
    })
}

/// The next page of a statement's answer: a heap JSON array of objects
/// keyed by column name, or `NULL` with `SekejapStatus_Ok` at the end.
/// The same operation as `sekejap_scan_next`, under the name that matches
/// `sekejap_query_open`.
///
/// # Safety
/// As `sekejap_scan_next`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_query_next(scan: *mut SekejapScan) -> *mut c_char {
    next_page(scan)
}

/// Close a paged answer and free it. Null-safe. The same operation as
/// `sekejap_scan_close`.
///
/// # Safety
/// As `sekejap_scan_close`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_query_close(scan: *mut SekejapScan) {
    sekejap_scan_close(scan)
}

// ── §5 edges ────────────────────────────────────────────────────────────────

/// Link two rows with a typed edge in the base graph context, committed.
/// `0` on success, `-1` on failure. BOTH endpoints must already exist: a
/// missing one is an error, never a dangling identity.
///
/// # Safety
/// Every pointer must be a valid null-terminated UTF-8 C string and `db` a
/// live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_link(
    db: *mut SekejapDb,
    from_collection: *const c_char,
    from_key: *const c_char,
    edge_type: *const c_char,
    to_collection: *const c_char,
    to_key: *const c_char,
) -> i32 {
    guard_i32(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let (from, edge_type, to) =
            edge_args(from_collection, from_key, edge_type, to_collection, to_key)?;
        record(handle.db.link(from, edge_type, to))?;
        Ok(0)
    })
}

/// `sekejap_link` carrying a JSON properties object.
///
/// # Safety
/// As `sekejap_link`, with `properties_json` a JSON object.
#[no_mangle]
pub unsafe extern "C" fn sekejap_link_with(
    db: *mut SekejapDb,
    from_collection: *const c_char,
    from_key: *const c_char,
    edge_type: *const c_char,
    to_collection: *const c_char,
    to_key: *const c_char,
    properties_json: *const c_char,
) -> i32 {
    guard_i32(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let (from, edge_type, to) =
            edge_args(from_collection, from_key, edge_type, to_collection, to_key)?;
        let properties = json_arg(properties_json, "properties").ok_or(())?;
        record(handle.db.link_with(from, edge_type, to, &properties))?;
        Ok(0)
    })
}

/// Remove one edge, committed. `1` if it was there, `0` if it was not, `-1`
/// on failure.
///
/// # Safety
/// As `sekejap_link`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_unlink(
    db: *mut SekejapDb,
    from_collection: *const c_char,
    from_key: *const c_char,
    edge_type: *const c_char,
    to_collection: *const c_char,
    to_key: *const c_char,
) -> i32 {
    guard_i32(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let (from, edge_type, to) =
            edge_args(from_collection, from_key, edge_type, to_collection, to_key)?;
        Ok(i32::from(record(handle.db.unlink(from, edge_type, to))?))
    })
}

/// The rows one hop away, in one direction, under a complete-or-error bound
/// of at most 256 edges: a wider walk is `GRAPH_TABLE` in SQL and is
/// REFUSED here by name. `edge_type` may be NULL for every type.
///
/// The answer is a heap JSON array of
/// `{"collection": "...", "key": "...", "document": { ... }}`, because a
/// neighbour can be in another collection and its name is part of the
/// answer. `NULL` on failure.
///
/// # Safety
/// As `sekejap_link`; `edge_type` may be NULL.
#[no_mangle]
pub unsafe extern "C" fn sekejap_neighbours(
    db: *mut SekejapDb,
    collection: *const c_char,
    key: *const c_char,
    edge_type: *const c_char,
    direction: SekejapDirection,
    limit: usize,
) -> *mut c_char {
    guard_str(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let collection = required(collection, "collection").ok_or(())?;
        let key = required(key, "key").ok_or(())?;
        let edge_type = match edge_type.is_null() {
            true => None,
            false => Some(required(edge_type, "edge_type").ok_or(())?),
        };
        let direction = match direction {
            SekejapDirection::Outgoing => Direction::Outgoing,
            SekejapDirection::Incoming => Direction::Incoming,
            SekejapDirection::Both => Direction::Both,
        };
        let found = record(
            handle
                .db
                .neighbours((collection, key), edge_type, direction, limit),
        )?;
        let answer: Vec<Value> = found
            .into_iter()
            .map(|document| {
                json!({
                    "collection": document.collection,
                    "key": document.key,
                    "document": document.fields,
                })
            })
            .collect();
        encode(&Value::Array(answer))
    })
}

/// # Safety
/// As `sekejap_link`.
#[allow(clippy::type_complexity)]
unsafe fn edge_args<'a>(
    from_collection: *const c_char,
    from_key: *const c_char,
    edge_type: *const c_char,
    to_collection: *const c_char,
    to_key: *const c_char,
) -> Result<((&'a str, &'a str), &'a str, (&'a str, &'a str)), ()> {
    let from_collection = required(from_collection, "from_collection").ok_or(())?;
    let from_key = required(from_key, "from_key").ok_or(())?;
    let edge_type = required(edge_type, "edge_type").ok_or(())?;
    let to_collection = required(to_collection, "to_collection").ok_or(())?;
    let to_key = required(to_key, "to_key").ok_or(())?;
    Ok((
        (from_collection, from_key),
        edge_type,
        (to_collection, to_key),
    ))
}

// ── §6 catalog ──────────────────────────────────────────────────────────────

/// Declare a collection. `fields_json` is a JSON array of
/// `{"name": "...", "kind": "text"|"int"|"real"|"bool"|"json"|"geo"|
/// "point"|"vector", "dimension": n}`, where `dimension` is required for
/// `vector` and rejected for every other kind. `1` if it was created, `0` if
/// it was already there, `-1` on failure.
///
/// The declaration is a floor, not a fence: a document may carry a field the
/// declaration does not name, and it is stored in the row's extras.
///
/// # Safety
/// `db` must be a live handle and both strings valid C strings.
#[no_mangle]
pub unsafe extern "C" fn sekejap_create_collection(
    db: *mut SekejapDb,
    name: *const c_char,
    fields_json: *const c_char,
) -> i32 {
    guard_i32(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let name = required(name, "name").ok_or(())?;
        let fields = json_arg(fields_json, "fields").ok_or(())?;
        let Value::Array(items) = fields else {
            invalid("`fields` must be a JSON array of {\"name\": ..., \"kind\": ...}");
            return Err(());
        };
        let mut declared: Vec<(String, FieldKind)> = Vec::with_capacity(items.len());
        for (at, item) in items.iter().enumerate() {
            declared.push(field_of(item, at)?);
        }
        let borrowed: Vec<(&str, FieldKind)> = declared
            .iter()
            .map(|(name, kind)| (name.as_str(), kind.clone()))
            .collect();
        Ok(i32::from(record(
            handle.db.create_collection(name, &borrowed),
        )?))
    })
}

fn field_of(item: &Value, at: usize) -> Result<(String, FieldKind), ()> {
    let Some(object) = item.as_object() else {
        invalid(format!("`fields[{at}]` must be a JSON object"));
        return Err(());
    };
    let Some(Value::String(name)) = object.get("name") else {
        invalid(format!("`fields[{at}].name` must be a string"));
        return Err(());
    };
    let Some(Value::String(kind)) = object.get("kind") else {
        invalid(format!("`fields[{at}].kind` must be a string"));
        return Err(());
    };
    let dimension = object.get("dimension").and_then(Value::as_u64);
    let kind = match kind.as_str() {
        "text" => FieldKind::Text,
        "int" => FieldKind::Int,
        "real" => FieldKind::Real,
        "bool" => FieldKind::Bool,
        "json" => FieldKind::Json,
        "geo" => FieldKind::Geo,
        "point" => FieldKind::Point,
        "vector" => match dimension {
            Some(n) if n > 0 => FieldKind::Vector(n as usize),
            _ => {
                invalid(format!(
                    "`fields[{at}].dimension` must be a positive whole number for a vector field"
                ));
                return Err(());
            }
        },
        other => {
            invalid(format!(
                "`fields[{at}].kind` is `{other}`; the kinds are text, int, real, bool, json, geo, point and vector"
            ));
            return Err(());
        }
    };
    if dimension.is_some() && !matches!(kind, FieldKind::Vector(_)) {
        invalid(format!(
            "`fields[{at}].dimension` belongs to a vector field only"
        ));
        return Err(());
    }
    Ok((name.clone(), kind))
}

/// Remove a collection, its rows, its indexes and its descriptor. `1` if it
/// was there, `0` if it was not, `-1` on failure.
///
/// # Safety
/// `db` must be a live handle and `name` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_drop_collection(
    db: *mut SekejapDb,
    name: *const c_char,
) -> i32 {
    guard_i32(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let name = required(name, "name").ok_or(())?;
        Ok(i32::from(record(handle.db.drop_collection(name))?))
    })
}

/// Every collection name in the catalog, in key order, as a heap JSON array
/// of strings. `NULL` on failure.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_collections(db: *mut SekejapDb) -> *mut c_char {
    guard_str(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let names = record(handle.db.collections())?;
        encode(&Value::Array(names.into_iter().map(Value::String).collect()))
    })
}

/// The declared shape of one collection, as a heap JSON object:
/// `{"name", "timestamps", "rows", "fields": [...], "indexes": [...]}`.
/// `rows` is the LIVE row count or `null` where this database keeps no
/// record for the collection -- `null` is "no record", not "no rows".
/// `NULL` with `SekejapStatus_Ok` means there is no such collection.
///
/// # Safety
/// `db` must be a live handle and `collection` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_describe(
    db: *mut SekejapDb,
    collection: *const c_char,
) -> *mut c_char {
    guard_str(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let collection = required(collection, "collection").ok_or(())?;
        let Some(found) = record(handle.db.describe(collection))? else {
            return Ok(None);
        };
        let fields: Vec<Value> = found
            .fields
            .iter()
            .map(|field| {
                let mut object = Map::new();
                object.insert("name".into(), Value::String(field.name.clone()));
                object.insert("kind".into(), Value::String(kind_name(&field.kind).into()));
                if let FieldKind::Vector(n) = field.kind {
                    object.insert("dimension".into(), Value::from(n as u64));
                }
                object.insert(
                    "declared".into(),
                    match &field.declared {
                        Some(text) => Value::String(text.clone()),
                        None => Value::Null,
                    },
                );
                object.insert("primary_key".into(), Value::Bool(field.primary_key));
                Value::Object(object)
            })
            .collect();
        let indexes: Vec<Value> = found
            .indexes
            .iter()
            .map(|index| {
                json!({
                    "name": index.name,
                    "field": index.field,
                    "family": family_name(index.family),
                    "unique": index.unique,
                    "ready": index.ready,
                })
            })
            .collect();
        let answer = json!({
            "name": found.name,
            "timestamps": found.timestamps,
            "rows": match found.rows { Some(rows) => Value::from(rows), None => Value::Null },
            "fields": fields,
            "indexes": indexes,
        });
        encode(&answer)
    })
}

fn kind_name(kind: &FieldKind) -> &'static str {
    match kind {
        FieldKind::Text => "text",
        FieldKind::Int => "int",
        FieldKind::Real => "real",
        FieldKind::Bool => "bool",
        FieldKind::Json => "json",
        FieldKind::Geo => "geo",
        FieldKind::Point => "point",
        FieldKind::Vector(_) => "vector",
    }
}

fn family_name(family: IndexFamily) -> &'static str {
    match family {
        IndexFamily::Scalar => "scalar",
        IndexFamily::Text => "text",
        IndexFamily::ExactVector => "exact_vector",
        IndexFamily::QuantizedVector => "quantized_vector",
        IndexFamily::SpatialPoint => "spatial_point",
        IndexFamily::SpatialGeometry => "spatial_geometry",
        IndexFamily::VamanaGraph => "vamana_graph",
    }
}

/// The rows of one collection, from the LIVE record when this database keeps
/// one and from the walk when it does not. `-1` on failure, which includes a
/// collection that is not in the catalog.
///
/// # Safety
/// `db` must be a live handle and `collection` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_count_rows(
    db: *mut SekejapDb,
    collection: *const c_char,
) -> c_long {
    guard_int(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let collection = required(collection, "collection").ok_or(())?;
        Ok(record(handle.db.count_rows(collection))? as c_long)
    })
}

/// Count the rows of one collection BY WALKING them, whether or not a live
/// record exists. The explicit walk, named as one. `-1` on failure.
///
/// # Safety
/// As `sekejap_count_rows`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_scan_count_rows(
    db: *mut SekejapDb,
    collection: *const c_char,
) -> c_long {
    guard_int(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let collection = required(collection, "collection").ok_or(())?;
        Ok(record(handle.db.scan_count_rows(collection))? as c_long)
    })
}

/// Count every edge BY WALKING the primary edge keyspace. sekejap keeps no
/// O(1) edge counter, so this is a scan and is named as one. `-1` on
/// failure.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_scan_count_edges(db: *mut SekejapDb) -> c_long {
    guard_int(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        Ok(record(handle.db.scan_count_edges())? as c_long)
    })
}

// ── §7 transactions ─────────────────────────────────────────────────────────

/// Take the writer for many writes under ONE barrier. Every plain call
/// commits per call, as the Rust API does; this is the other bargain.
///
/// While the transaction is open it HOLDS the writer: a call on the same
/// `SekejapDb*` that needs the writer waits for it. Commit or roll back
/// before using the handle for anything else, and free this handle BEFORE
/// `sekejap_close`. A handle freed any other way ROLLS BACK.
///
/// `NULL` on failure.
///
/// # Safety
/// `db` must be a live handle. The returned handle borrows it.
#[no_mangle]
pub unsafe extern "C" fn sekejap_tx_begin(db: *mut SekejapDb) -> *mut SekejapTx {
    guard_ptr(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let tx = record(handle.db.transaction())?;
        let tx: Tx<'static> = std::mem::transmute::<Tx<'_>, Tx<'static>>(tx);
        Ok(Box::into_raw(Box::new(SekejapTx { inner: Some(tx) })))
    })
}

/// # Safety
/// `tx` must be a live handle from `sekejap_tx_begin`.
unsafe fn tx_of<'a>(tx: *mut SekejapTx) -> Option<&'a mut Tx<'static>> {
    if tx.is_null() {
        invalid("`tx` is NULL");
        return None;
    }
    match (*tx).inner.as_mut() {
        Some(inner) => Some(inner),
        None => {
            invalid("this transaction has already been committed or rolled back");
            None
        }
    }
}

/// Write one document inside the transaction, with NO commit. `0` or `-1`.
///
/// # Safety
/// `tx` must be a live handle and every string a valid C string.
#[no_mangle]
pub unsafe extern "C" fn sekejap_tx_put(
    tx: *mut SekejapTx,
    collection: *const c_char,
    key: *const c_char,
    document_json: *const c_char,
) -> i32 {
    guard_i32(|| {
        clear_error();
        let tx = tx_of(tx).ok_or(())?;
        let collection = required(collection, "collection").ok_or(())?;
        let key = required(key, "key").ok_or(())?;
        let document = json_arg(document_json, "document").ok_or(())?;
        record(tx.put((collection, key), &document))?;
        Ok(0)
    })
}

/// Delete one row inside the transaction, with NO commit. `1` if it was
/// there, `0` if it was not, `-1` on failure.
///
/// # Safety
/// As `sekejap_tx_put`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_tx_delete(
    tx: *mut SekejapTx,
    collection: *const c_char,
    key: *const c_char,
) -> i32 {
    guard_i32(|| {
        clear_error();
        let tx = tx_of(tx).ok_or(())?;
        let collection = required(collection, "collection").ok_or(())?;
        let key = required(key, "key").ok_or(())?;
        Ok(i32::from(record(tx.delete((collection, key)))?))
    })
}

/// Link two rows inside the transaction, with NO commit. `0` or `-1`.
///
/// # Safety
/// As `sekejap_tx_put`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_tx_link(
    tx: *mut SekejapTx,
    from_collection: *const c_char,
    from_key: *const c_char,
    edge_type: *const c_char,
    to_collection: *const c_char,
    to_key: *const c_char,
) -> i32 {
    guard_i32(|| {
        clear_error();
        let tx = tx_of(tx).ok_or(())?;
        let (from, edge_type, to) =
            edge_args(from_collection, from_key, edge_type, to_collection, to_key)?;
        record(tx.link(from, edge_type, to))?;
        Ok(0)
    })
}

/// Run one writing statement inside the transaction, with NO commit.
/// Returns the rows it moved, or `-1`.
///
/// # Safety
/// As `sekejap_tx_put`, with `params_json` NULL or a JSON array.
#[no_mangle]
pub unsafe extern "C" fn sekejap_tx_execute(
    tx: *mut SekejapTx,
    sql: *const c_char,
    params_json: *const c_char,
) -> c_long {
    guard_int(|| {
        clear_error();
        let tx = tx_of(tx).ok_or(())?;
        let sql = required(sql, "sql").ok_or(())?;
        let params = params(params_json).ok_or(())?;
        Ok(record(tx.execute(sql, &params))? as c_long)
    })
}

/// Commit the transaction and FREE the handle, whether the commit succeeded
/// or not. `0` on success, `-1` on failure. The pointer is dangling after
/// this call in both cases.
///
/// # Safety
/// `tx` must be a live handle that has not been committed or rolled back.
#[no_mangle]
pub unsafe extern "C" fn sekejap_tx_commit(tx: *mut SekejapTx) -> i32 {
    finish(tx, true)
}

/// Roll the transaction back and FREE the handle. `0` on success, `-1` on
/// failure. The pointer is dangling after this call in both cases.
///
/// # Safety
/// As `sekejap_tx_commit`.
#[no_mangle]
pub unsafe extern "C" fn sekejap_tx_rollback(tx: *mut SekejapTx) -> i32 {
    finish(tx, false)
}

/// # Safety
/// As `sekejap_tx_commit`.
unsafe fn finish(tx: *mut SekejapTx, commit: bool) -> i32 {
    guard_i32(|| {
        clear_error();
        if tx.is_null() {
            invalid("`tx` is NULL");
            return Err(());
        }
        let mut handle = Box::from_raw(tx);
        let Some(inner) = handle.inner.take() else {
            invalid("this transaction has already been committed or rolled back");
            return Err(());
        };
        let out = if commit { inner.commit() } else { inner.rollback() };
        record(out)?;
        Ok(0)
    })
}

// ── §8 maintenance ──────────────────────────────────────────────────────────

/// Fold the committed write-ahead log into the data file. `1` when it
/// folded, `0` when a live reader holds a slot and the fold is DEFERRED
/// (which in service mode is every call, because the published read view
/// holds one for its whole life), `-1` on failure. Deferred is not a
/// failure.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_checkpoint(db: *mut SekejapDb) -> i32 {
    guard_i32(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        Ok(i32::from(record(handle.db.checkpoint())?))
    })
}

/// Make the newest commit visible to readers now. In single mode there is no
/// published view to swap and every commit is already visible to this
/// handle, so this succeeds having done nothing. `0` or `-1`.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_publish(db: *mut SekejapDb) -> i32 {
    guard_i32(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        record(handle.db.publish())?;
        Ok(0)
    })
}

/// The bytes on disk, as a heap JSON object
/// `{"data_bytes": n, "wal_bytes": n, "total_bytes": n}`. `NULL` on failure.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_storage(db: *mut SekejapDb) -> *mut c_char {
    guard_str(|| {
        clear_error();
        let handle = handle(db).ok_or(())?;
        let Storage {
            data_bytes,
            wal_bytes,
        } = record(handle.db.storage())?;
        encode(&json!({
            "data_bytes": data_bytes,
            "wal_bytes": wal_bytes,
            "total_bytes": data_bytes.saturating_add(wal_bytes),
        }))
    })
}

// ── §9 service mode ─────────────────────────────────────────────────────────

/// # Safety
/// `db` must be a live handle.
unsafe fn service<'a>(
    db: *const SekejapDb,
    call: &str,
) -> Option<(&'a SekejapDb, &'a sekejap::dist::service::ServiceDatabase)> {
    let handle = handle(db)?;
    match handle.db.service() {
        Some(service) => Some((handle, service)),
        None => {
            refuse(
                call,
                "this handle was opened in single mode, which has no writer to time out, \
                 no interrupt and no change feed; open it with sekejap_open_service",
            );
            None
        }
    }
}

/// Refuse a statement that runs longer than `milliseconds`. `0` clears the
/// timeout. `0` on success, `-1` on failure -- which includes a handle that
/// is not in service mode, REFUSED by name.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_statement_timeout_ms(
    db: *mut SekejapDb,
    milliseconds: u64,
) -> i32 {
    guard_i32(|| {
        clear_error();
        let (_, service) = service(db, "sekejap_statement_timeout_ms").ok_or(())?;
        if milliseconds == 0 {
            service.clear_statement_timeout();
        } else {
            service.set_statement_timeout(Duration::from_millis(milliseconds));
        }
        Ok(0)
    })
}

/// Cancel the work in flight on this service, from any thread. The flag is
/// STICKY until `sekejap_clear_interrupt`. `0` on success, `-1` on failure
/// -- which includes a handle that is not in service mode.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_cancel(db: *mut SekejapDb) -> i32 {
    guard_i32(|| {
        clear_error();
        let (_, service) = service(db, "sekejap_cancel").ok_or(())?;
        service.cancel();
        Ok(0)
    })
}

/// Clear a cancel so the service accepts work again. `1` when a cancel was
/// standing, `0` when none was, `-1` on failure.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_clear_interrupt(db: *mut SekejapDb) -> i32 {
    guard_i32(|| {
        clear_error();
        let (_, service) = service(db, "sekejap_clear_interrupt").ok_or(())?;
        Ok(i32::from(service.clear_interrupt()))
    })
}

/// Subscribe to the commit-time change feed. Returns a subscription id to
/// pass to `sekejap_next_change` and `sekejap_unsubscribe`, or `-1` on
/// failure -- which includes a handle that is not in service mode.
///
/// The subscription is owned by the database handle, so an id is valid from
/// any thread; a subscription left open is closed by `sekejap_close`.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_subscribe(db: *mut SekejapDb) -> c_long {
    guard_int(|| {
        clear_error();
        let (handle, service) = service(db, "sekejap_subscribe").ok_or(())?;
        let receiver = service.subscribe_changes();
        let id = receiver.id().0;
        handle
            .subscriptions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, receiver);
        Ok(id as c_long)
    })
}

/// The next change event for one subscription, as a heap JSON object, or
/// `NULL` with `SekejapStatus_Ok` when none arrived. `timeout_ms` of `0`
/// polls and returns at once; a positive value waits that long.
///
/// The object is
/// `{"sequence", "collections", "edge_types", "keys", "keys_total",
/// "keys_truncated", "unnamed_writes", "rows_affected"}`. `keys` is empty
/// and `keys_truncated` true when the batch moved more keys than the feed's
/// per-event cap: the list is dropped whole rather than handed over
/// half-true, and `collections` is still exact.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_next_change(
    db: *mut SekejapDb,
    subscription: c_long,
    timeout_ms: u64,
) -> *mut c_char {
    guard_str(|| {
        clear_error();
        let (handle, _) = service(db, "sekejap_next_change").ok_or(())?;
        if subscription < 0 {
            invalid("`subscription` is not an id sekejap_subscribe returned");
            return Err(());
        }
        let mut open = handle
            .subscriptions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(receiver) = open.get_mut(&(subscription as u64)) else {
            invalid(format!(
                "subscription {subscription} is not open on this handle"
            ));
            return Err(());
        };
        let event = if timeout_ms == 0 {
            receiver.try_recv()
        } else {
            receiver.recv_timeout(Duration::from_millis(timeout_ms))
        };
        let Some(event) = event else {
            return Ok(None);
        };
        let keys: Vec<Value> = event
            .keys
            .iter()
            .map(|changed| {
                json!({
                    "collection": changed.collection.0,
                    "key": changed.key,
                    "kind": match changed.kind {
                        ChangeKind::Put => "put",
                        ChangeKind::Delete => "delete",
                    },
                })
            })
            .collect();
        let answer = json!({
            "sequence": event.sequence,
            "collections": event.collections.iter().map(|c| c.0).collect::<Vec<u32>>(),
            "edge_types": event.edge_types.iter().map(|t| t.0).collect::<Vec<u64>>(),
            "keys": keys,
            "keys_total": event.keys_total,
            "keys_truncated": event.keys_truncated,
            "unnamed_writes": event.unnamed_writes,
            "rows_affected": event.rows_affected,
        });
        encode(&answer)
    })
}

/// Close one subscription. `1` when it was open on the service, `0` when it
/// was not, `-1` on failure.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_unsubscribe(db: *mut SekejapDb, subscription: c_long) -> i32 {
    guard_i32(|| {
        clear_error();
        let (handle, service) = service(db, "sekejap_unsubscribe").ok_or(())?;
        if subscription < 0 {
            invalid("`subscription` is not an id sekejap_subscribe returned");
            return Err(());
        }
        let id = subscription as u64;
        handle
            .subscriptions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        Ok(i32::from(service.unsubscribe(SubscriptionId(id))))
    })
}

// ── §10 refused by name ─────────────────────────────────────────────────────
//
// `docs/dist/RUST_API.md` §7 is a list of things sekejap does NOT offer, each
// because there is no atomic underneath and an emulation would be a fake.
// The four that a C caller would otherwise reach for keep a SYMBOL here, so
// the refusal arrives with a name and a reason instead of a link error or,
// worse, a plausible wrong answer.

/// REFUSED: sekejap is disk-first and has no in-memory database. Always
/// `NULL`, with the reason in `sekejap_last_error` and
/// `SekejapStatus_Refused` in `sekejap_last_error_code`.
///
/// A temporary directory would be a fake of an ephemeral store, so this does
/// not make one. Give `sekejap_open` a directory.
#[no_mangle]
pub extern "C" fn sekejap_open_memory() -> *mut SekejapDb {
    guard_ptr(|| {
        clear_error();
        refuse(
            "sekejap_open_memory",
            "sekejap is disk-first and has no in-memory store \
             (docs/dist/OPS_CONTRACT.md Law 1); sekejap_open takes a directory",
        );
        Err(())
    })
}

/// REFUSED: there is nothing proportional to rows held in memory to trim.
/// Always `-1`, with the reason in `sekejap_last_error`.
///
/// The caches sekejap keeps are bounded at open -- the buffer pool by
/// `budget_bytes`, the plan cache by its three ceilings -- so a trim call
/// would have nothing to give back, and a no-op that returned success would
/// be a fake of reclaim.
///
/// # Safety
/// `db` may be NULL; when it is not, it must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_trim_memory(db: *mut SekejapDb) -> i32 {
    let _ = db;
    guard_i32(|| {
        clear_error();
        refuse(
            "sekejap_trim_memory",
            "sekejap holds nothing proportional to rows to trim: the buffer pool is bounded \
             by budget_bytes and the plan cache by its three ceilings \
             (docs/dist/OPS_CONTRACT.md §6.3)",
        );
        Err(())
    })
}

/// REFUSED: there is no payload-rewriting compaction. Always `-1`.
/// `sekejap_checkpoint` folds the committed write-ahead log into the data
/// file; it does not rewrite rows, and naming that `compact` would promise
/// something else.
///
/// # Safety
/// `db` may be NULL; when it is not, it must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn sekejap_compact(db: *mut SekejapDb) -> i32 {
    let _ = db;
    guard_i32(|| {
        clear_error();
        refuse(
            "sekejap_compact",
            "sekejap has no payload-rewriting compaction; sekejap_checkpoint folds the \
             committed write-ahead log into the data file (docs/dist/RUST_API.md §7)",
        );
        Err(())
    })
}

/// REFUSED: the `SHOW` family is not in this dialect. Always `NULL`.
/// `sekejap_collections` and `sekejap_describe` answer the same
/// questions as DATA rather than as a result set.
///
/// # Safety
/// `db` may be NULL; `statement` may be NULL.
#[no_mangle]
pub unsafe extern "C" fn sekejap_show(
    db: *mut SekejapDb,
    statement: *const c_char,
) -> *mut c_char {
    let _ = (db, statement);
    guard_str(|| {
        clear_error();
        refuse(
            "sekejap_show",
            "the SHOW family has no Tier-1 spelling (docs/lang/QL_CONTRACT.md §2); \
             sekejap_collections and sekejap_describe answer the same questions as data",
        );
        Err(())
    })
}

// ── the tests ───────────────────────────────────────────────────────────────

/// The ABI exercised through its C signatures, in `dist/ffi/tests/abi.rs`.
///
/// It is this crate's own test module rather than an integration test
/// because an integration test would need an `rlib` beside the C library,
/// and an `rlib` named `libsekejap` collides with the published crate's own.
/// What the module tests is the same thing either way: the `extern "C"`
/// entry points, called with `CString`/`CStr` and raw pointers. That the
/// LINK works is tested where it belongs, by `make check` compiling
/// `examples/smoke.c` against the built library with a C compiler.
#[cfg(test)]
#[path = "../tests/abi.rs"]
mod abi;
