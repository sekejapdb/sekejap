//! WHAT ONE RELATIONSHIP COSTS, counted rather than timed.
//!
//! Writing an edge is two puts into the shared tree. Everything else it does
//! -- proving both endpoint rows exist, probing for a forward/reverse pair it
//! is about to overwrite -- is a read, and a read that already knows its own
//! answer is the read-before-write the contract forbids.
//!
//! The handle knows the answer for every identity IT allocated: sequences are
//! dense, monotonic and never reused, so such a row did not exist in any
//! committed state at open and nothing but this handle can have touched it
//! since. These tests pin that knowledge to the identity, not to a fixed-size
//! recency window, because a window is a cost that grows with N: the 1M
//! multimodel run wrote 3,000,000 relationships of which 93% named endpoints
//! the window had already forgotten, and paid four extra page descents each.
use e4_prototype::{
    collections::{CollectionId, CollectionOptions, Database, EdgeTypeId, EntityId, Error,
        GraphContextId},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout as AllocationLayout, System},
    cell::Cell,
};

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}
struct Alloc;
unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: AllocationLayout) -> *mut u8 {
        TRACK
            .try_with(|t| {
                if t.get() {
                    COUNT.with(|n| n.set(n.get() + 1));
                }
            })
            .ok();
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: AllocationLayout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: AllocationLayout, n: usize) -> *mut u8 {
        TRACK
            .try_with(|t| {
                if t.get() {
                    COUNT.with(|c| c.set(c.get() + 1));
                }
            })
            .ok();
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static ALLOC: Alloc = Alloc;
fn allocations<T>(f: impl FnOnce() -> T) -> (T, usize) {
    COUNT.with(|c| c.set(0));
    TRACK.with(|t| t.set(true));
    let value = f();
    TRACK.with(|t| t.set(false));
    (value, COUNT.with(Cell::get))
}

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// `rows` identities allocated by one handle, all committed, plus one edge
/// type. `rows` is chosen by each test; what matters is whether it is past the
/// recency window the old fresh-identity map used.
fn population(rows: u64) -> (tempfile::TempDir, Database, CollectionId, EdgeTypeId) {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    db.commit().unwrap();
    for i in 0..rows {
        db.put(people, &format!("p{i:08}"), &json!({"name":"x"}))
            .unwrap();
        if (i + 1) % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    (dir, db, people, knows)
}

fn entity(people: CollectionId, i: u64) -> EntityId {
    EntityId {
        collection: people,
        sequence: i,
    }
}

/// The window bound the fresh-identity map used, plus room to clear it.
const PAST_THE_WINDOW: u64 = (1 << 16) + 4_000;

/// Page accesses for `count` edges written over the OLDEST identities the
/// handle allocated -- the ones a recency window has long since dropped.
fn pages_per_edge(db: &mut Database, people: CollectionId, knows: EdgeTypeId, count: u64) -> f64 {
    let before = db.pool_accesses().unwrap();
    for i in 0..count {
        db.put_edge(
            GraphContextId::BASE,
            entity(people, i + 1),
            knows,
            entity(people, i + 2),
            &json!({}),
        )
        .unwrap();
    }
    db.commit().unwrap();
    (db.pool_accesses().unwrap() - before) as f64 / count as f64
}

/// THE FINDING. An edge between two rows this handle wrote costs two puts.
/// Both endpoint reads and both halves of the forward/reverse probe are reads
/// whose answer the allocator already fixed, and none of them may come back
/// just because the identity is no longer recent.
#[test]
fn an_edge_over_old_handle_allocated_rows_costs_no_extra_descent() {
    let (_dir, mut db, people, knows) = population(PAST_THE_WINDOW);
    let pages = pages_per_edge(&mut db, people, knows, 2_000);
    assert!(
        pages < 12.0,
        "an edge over two rows this handle allocated took {pages:.2} page accesses; \
         two puts are about 8.7 and the four reads they do not need are about 16"
    );
}

/// The same edges over identities the window still held. This is the cost the
/// test above must match: the point is that the two are the SAME number, so
/// the per-edge cost stops growing with how much was loaded first.
#[test]
fn the_cost_of_an_edge_does_not_grow_with_the_rows_loaded_before_it() {
    let (_dir, mut db_small, small, small_knows) = population(2_000);
    let near = pages_per_edge(&mut db_small, small, small_knows, 1_000);
    let (_dir2, mut db_big, big, big_knows) = population(PAST_THE_WINDOW);
    let far = pages_per_edge(&mut db_big, big, big_knows, 1_000);
    assert!(
        far < near * 1.35,
        "the same edge cost {near:.2} page accesses after 2,000 rows and {far:.2} after \
         {PAST_THE_WINDOW}; the second number must not grow with the first"
    );
}

/// Key building and property encoding are the rest of the per-edge constant.
/// An edge key is a handful of integers into one buffer and `{}` has a frozen
/// three-byte encoding, so neither is a reason to reach for the allocator
/// once per component.
#[test]
fn writing_one_edge_allocates_a_bounded_number_of_times() {
    let (_dir, mut db, people, knows) = population(PAST_THE_WINDOW);
    // Warm every cache the first edge would otherwise fill.
    db.put_edge(
        GraphContextId::BASE,
        entity(people, 1),
        knows,
        entity(people, 2),
        &json!({}),
    )
    .unwrap();
    let properties = json!({});
    let (result, count) = allocations(|| {
        db.put_edge(
            GraphContextId::BASE,
            entity(people, 3),
            knows,
            entity(people, 4),
            &properties,
        )
    });
    result.unwrap();
    assert!(
        count <= 8,
        "writing one relationship allocated {count} times; the two keys and the \
         encoded properties are three buffers, not one per integer in them"
    );
}

/// The guard on the claim. A row the handle allocated AND THEN DELETED is not
/// live, and an edge naming it must still be refused -- the cheap proof may
/// only ever shrink, never assume.
#[test]
fn an_edge_to_a_row_this_handle_deleted_is_still_refused() {
    let (_dir, mut db, people, knows) = population(PAST_THE_WINDOW);
    db.delete(people, "p00000009").unwrap();
    let err = db
        .put_edge(
            GraphContextId::BASE,
            entity(people, 1),
            knows,
            entity(people, 10),
            &json!({}),
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::NotFound(_)),
        "an edge to a deleted row was accepted: {err:?}"
    );
    // Its neighbours on either side are untouched and still linkable.
    db.put_edge(
        GraphContextId::BASE,
        entity(people, 1),
        knows,
        entity(people, 9),
        &json!({}),
    )
    .unwrap();
    db.put_edge(
        GraphContextId::BASE,
        entity(people, 1),
        knows,
        entity(people, 11),
        &json!({}),
    )
    .unwrap();
    db.commit().unwrap();
}

/// The other guard. An identity past the allocator's cursor was never handed
/// out, so nothing wrote its row, and "this handle allocated it" must not be
/// read off a range check alone.
#[test]
fn an_edge_to_an_identity_never_handed_out_is_refused() {
    let (_dir, mut db, people, knows) = population(2_000);
    let err = db
        .put_edge(
            GraphContextId::BASE,
            entity(people, 1),
            knows,
            entity(people, 9_999),
            &json!({}),
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::NotFound(_)),
        "an edge to an identity that was never allocated was accepted: {err:?}"
    );
}

/// A reopened handle allocated none of the rows on disk, so it proves nothing
/// about them and must read before it writes. The cheap path is a claim about
/// THIS handle's allocations only.
#[test]
fn a_reopened_handle_proves_nothing_about_rows_it_did_not_allocate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (people, knows) = {
        let mut db = Database::create(&path, cfg()).unwrap();
        let people = db
            .create_collection(
                "people",
                vec![("name".into(), Kind::Text)],
                CollectionOptions::default(),
            )
            .unwrap();
        db.enable_graph().unwrap();
        let knows = db.create_edge_type("knows").unwrap();
        for i in 0..100u64 {
            db.put(people, &format!("p{i:08}"), &json!({"name":"x"}))
                .unwrap();
        }
        db.commit().unwrap();
        (people, knows)
    };
    let mut db = Database::open(&path, cfg()).unwrap();
    let err = db
        .put_edge(
            GraphContextId::BASE,
            entity(people, 1),
            knows,
            entity(people, 4_000),
            &json!({}),
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::NotFound(_)),
        "a reopened handle accepted an edge to a row that is not there: {err:?}"
    );
    db.put_edge(
        GraphContextId::BASE,
        entity(people, 1),
        knows,
        entity(people, 2),
        &json!({}),
    )
    .unwrap();
    db.commit().unwrap();
}
