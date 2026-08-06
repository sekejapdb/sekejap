//! Change-notification primitive: one event per committed mutation.
use sekejap::{CoreDB, ChangeEvent};
use std::sync::{Arc, Mutex};

fn recorder() -> (Arc<Mutex<Vec<ChangeEvent>>>, impl FnMut(&ChangeEvent) + Send + 'static) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let l2 = log.clone();
    (log, move |ev: &ChangeEvent| l2.lock().unwrap().push(ev.clone()))
}

#[test]
fn single_put_emits_one_event() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
    let (log, cb) = recorder();
    db.subscribe_changes(cb);
    db.execute("INSERT INTO items (_key, name) VALUES ('a', 'A')").unwrap();
    let ev = log.lock().unwrap();
    assert_eq!(ev.len(), 1, "one insert = one event");
    assert!(ev[0].collections.contains(&"items".to_string()));
    assert!(ev[0].keys.iter().any(|k| k.contains("a")));
}

#[test]
fn transaction_emits_once_at_commit() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
    let (log, cb) = recorder();
    db.subscribe_changes(cb);
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO items (_key, name) VALUES ('a','A')").unwrap();
    db.execute("INSERT INTO items (_key, name) VALUES ('b','B')").unwrap();
    assert_eq!(log.lock().unwrap().len(), 0, "no event mid-transaction");
    db.execute("COMMIT").unwrap();
    let ev = log.lock().unwrap();
    assert_eq!(ev.len(), 1, "whole transaction = one event");
    assert!(ev[0].keys.len() >= 2, "both keys in the one event");
}

#[test]
fn rollback_emits_nothing() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
    let (log, cb) = recorder();
    db.subscribe_changes(cb);
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO items (_key, name) VALUES ('a','A')").unwrap();
    db.execute("ROLLBACK").unwrap();
    assert_eq!(log.lock().unwrap().len(), 0, "rollback emits nothing");
    // and the discarded change must not leak into the next mutation
    db.execute("INSERT INTO items (_key, name) VALUES ('b','B')").unwrap();
    let ev = log.lock().unwrap();
    assert_eq!(ev.len(), 1);
    assert!(!ev[0].keys.iter().any(|k| k.contains("a")), "aborted key must not leak");
}

#[test]
fn update_delete_and_edges_emit() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, v INTEGER)").unwrap();
    db.execute("INSERT INTO items (_key, v) VALUES ('a', 1)").unwrap();
    let (log, cb) = recorder();
    db.subscribe_changes(cb);
    db.execute("UPDATE items SET v = 2 WHERE _key = 'a'").unwrap();
    db.execute("DELETE FROM items WHERE _key = 'a'").unwrap();
    db.link("items/a", "items/b", "rel");
    db.unlink("items/a", "items/b", "rel");
    let ev = log.lock().unwrap();
    assert_eq!(ev.len(), 4, "update, delete, link, unlink = 4 events");
    assert!(ev[2].edge_types.contains(&"rel".to_string()), "link edge type recorded");
}

#[test]
fn unsubscribe_stops_events() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY)").unwrap();
    let (log, cb) = recorder();
    let id = db.subscribe_changes(cb);
    db.execute("INSERT INTO items (_key) VALUES ('a')").unwrap();
    db.unsubscribe_changes(id);
    db.execute("INSERT INTO items (_key) VALUES ('b')").unwrap();
    assert_eq!(log.lock().unwrap().len(), 1, "no events after unsubscribe");
}
