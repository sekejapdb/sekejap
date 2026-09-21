//! Android JNI binding for sekejap.
//!
//! Exposes a minimal, benchmark-complete surface (open, execute[_params],
//! query_params, get, put_many, compact, mobile profile, close) as native
//! methods for the Kotlin `life.sekejap.SekejapNative` object. The database
//! handle is a boxed `CoreDB` pointer passed back and forth as a `jlong`.

use jni::objects::{JClass, JString};
use jni::sys::{jlong, jstring};
use jni::JNIEnv;
use sekejap::{AutoCompact, CoreDB, SyncMode};
use serde_json::Value;

// ── helpers ──────────────────────────────────────────────────────────────────

unsafe fn db<'a>(handle: jlong) -> &'a mut CoreDB {
    &mut *(handle as *mut CoreDB)
}

fn jstr(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

fn out(env: &JNIEnv, s: String) -> jstring {
    env.new_string(s).map(|o| o.into_raw()).unwrap_or(std::ptr::null_mut())
}

// ── lifecycle ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_open(
    mut env: JNIEnv,
    _class: JClass,
    path: JString,
) -> jlong {
    let path = jstr(&mut env, &path);
    match CoreDB::open(&path) {
        Ok(db) => Box::into_raw(Box::new(db)) as jlong,
        Err(_) => 0,
    }
}

/// Relaxed-durability mobile profile: WAL sync = Normal, auto-compaction = Manual.
#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_mobileProfile(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    let db = unsafe { db(handle) };
    db.set_wal_sync(SyncMode::Normal);
    db.set_auto_compact(AutoCompact::Manual);
}

#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_compact(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle != 0 {
        let _ = unsafe { db(handle) }.compact();
    }
}

#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_close(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle != 0 {
        // Reclaim the box; Drop flushes/closes.
        unsafe { drop(Box::from_raw(handle as *mut CoreDB)) };
    }
}

// ── mutations ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_execute(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    sql: JString,
) -> jlong {
    let sql = jstr(&mut env, &sql);
    match unsafe { db(handle) }.execute(&sql) {
        Ok(n) => n as jlong,
        Err(_) => -1,
    }
}

#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_executeParams(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    sql: JString,
    params_json: JString,
) -> jlong {
    let sql = jstr(&mut env, &sql);
    let params: Vec<Value> =
        serde_json::from_str(&jstr(&mut env, &params_json)).unwrap_or_default();
    match unsafe { db(handle) }.execute_params(&sql, &params) {
        Ok(n) => n as jlong,
        Err(_) => -1,
    }
}

/// Bulk insert. `rows_json` is a JSON array of `[slug, payloadObject]` pairs
/// (payload as a nested object, not a string — cheaper for callers to build) —
/// one FFI crossing, one durability barrier.
#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_putMany(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    rows_json: JString,
) -> jlong {
    let rows: Vec<(String, Value)> =
        serde_json::from_str(&jstr(&mut env, &rows_json)).unwrap_or_default();
    let payloads: Vec<(String, String)> =
        rows.into_iter().map(|(s, v)| (s, v.to_string())).collect();
    let refs: Vec<(&str, &str)> = payloads.iter().map(|(s, j)| (s.as_str(), j.as_str())).collect();
    match unsafe { db(handle) }.put_many(refs) {
        Ok(v) => v.len() as jlong,
        Err(_) => -1,
    }
}

// ── change feed (reactive .watch) ────────────────────────────────────────────

enum WatchMsg {
    Event(sekejap::ChangeEvent),
    Stop,
}

/// A live subscription. Owned as a boxed pointer (jlong). `watchNext` parks on
/// `rx`; `watchClose` unsubscribes and posts `Stop`; the caller's watch thread
/// frees the box via `watchFree` after `watchNext` returns null.
struct SekejapWatch {
    id: u64,
    tx: std::sync::mpsc::Sender<WatchMsg>,
    rx: std::sync::Mutex<std::sync::mpsc::Receiver<WatchMsg>>,
}

/// Begin watching the change feed. Returns a watch handle (jlong).
#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_watchOpen(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jlong {
    if handle == 0 {
        return 0;
    }
    let (tx, rx) = std::sync::mpsc::channel::<WatchMsg>();
    // Mutex-wrap so the listener closure is Send + Sync (CoreDB requires it).
    let listener_tx = std::sync::Mutex::new(tx.clone());
    let id = unsafe { db(handle) }.subscribe_changes(move |ev| {
        if let Ok(t) = listener_tx.lock() {
            let _ = t.send(WatchMsg::Event(ev.clone()));
        }
    });
    Box::into_raw(Box::new(SekejapWatch { id, tx, rx: std::sync::Mutex::new(rx) })) as jlong
}

/// Block until the next committed change, returning it as JSON
/// (`{"collections":[…],"keys":[…],"edge_types":[…]}`), or null when stopped.
#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_watchNext(
    env: JNIEnv,
    _class: JClass,
    watch: jlong,
) -> jstring {
    if watch == 0 {
        return std::ptr::null_mut();
    }
    let w = unsafe { &*(watch as *const SekejapWatch) };
    let msg = {
        let rx = w.rx.lock().unwrap();
        rx.recv()
    };
    match msg {
        Ok(WatchMsg::Event(ev)) => {
            let json = serde_json::json!({
                "collections": ev.collections,
                "keys": ev.keys,
                "edge_types": ev.edge_types,
            });
            out(&env, json.to_string())
        }
        _ => std::ptr::null_mut(), // Stop or channel closed
    }
}

/// Stop a watch: unsubscribe the engine listener and wake a parked `watchNext`.
/// Does NOT free the box (the watch thread frees it via `watchFree`).
#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_watchClose(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    watch: jlong,
) {
    if watch == 0 {
        return;
    }
    let w = unsafe { &*(watch as *const SekejapWatch) };
    let id = w.id; // read before signaling
    if handle != 0 {
        unsafe { db(handle) }.unsubscribe_changes(id);
    }
    let _ = w.tx.send(WatchMsg::Stop); // last box access — wakes watchNext
}

/// Free a watch box. Call only after `watchNext` has returned null.
#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_watchFree(
    _env: JNIEnv,
    _class: JClass,
    watch: jlong,
) {
    if watch != 0 {
        unsafe { drop(Box::from_raw(watch as *mut SekejapWatch)) };
    }
}

// ── reads ────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_get(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    slug: JString,
) -> jstring {
    let slug = jstr(&mut env, &slug);
    match unsafe { db(handle) }.get(&slug) {
        Some(s) => out(&env, s),
        None => std::ptr::null_mut(),
    }
}

/// Parameterised SELECT. Returns `[{"slug":..,"payload":..}, …]` as JSON.
#[no_mangle]
pub extern "system" fn Java_life_sekejap_SekejapNative_queryParams(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    sql: JString,
    params_json: JString,
) -> jstring {
    let sql = jstr(&mut env, &sql);
    let params: Vec<Value> =
        serde_json::from_str(&jstr(&mut env, &params_json)).unwrap_or_default();
    let hits = match unsafe { db(handle) }.query_params(&sql, &params) {
        Ok(q) => q.collect(),
        Err(_) => return out(&env, "[]".to_string()),
    };
    let rows: Vec<Value> = hits
        .into_iter()
        .map(|h| serde_json::json!({ "slug": h.slug, "payload": h.payload }))
        .collect();
    out(&env, serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string()))
}
