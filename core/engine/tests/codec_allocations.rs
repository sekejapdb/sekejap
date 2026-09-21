//! Bound unnecessary row-sized scratch while preserving the stored bytes.
use sekejap_core::{decode_dense_v3, encode_dense_v3, Kind, Layout};
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout as AllocationLayout, System},
    cell::Cell,
};
thread_local! { static TRACK: Cell<bool> = const {Cell::new(false)}; static BYTES: Cell<usize> = const {Cell::new(0)}; static COUNT: Cell<usize> = const {Cell::new(0)}; }
struct Alloc;
unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: AllocationLayout) -> *mut u8 {
        TRACK
            .try_with(|t| {
                if t.get() {
                    BYTES.with(|n| n.set(n.get() + l.size()));
                    COUNT.with(|n| n.set(n.get() + 1));
                }
            })
            .ok();
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: AllocationLayout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: AllocationLayout, n: usize) -> *mut u8 {
        TRACK
            .try_with(|t| {
                if t.get() {
                    BYTES.with(|b| b.set(b.get() + n));
                    COUNT.with(|c| c.set(c.get() + 1));
                }
            })
            .ok();
        System.realloc(p, l, n)
    }
}
#[global_allocator]
static ALLOC: Alloc = Alloc;
fn measured<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let (v, bytes, _) = counted(f);
    (v, bytes)
}
/// Bytes AND the number of allocation calls. A per-row `Vec` shows up in the
/// count even when it is small, which is the shape a build must not have.
fn counted<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    BYTES.with(|b| b.set(0));
    COUNT.with(|c| c.set(0));
    TRACK.with(|t| t.set(true));
    let v = f();
    TRACK.with(|t| t.set(false));
    (v, BYTES.with(Cell::get), COUNT.with(Cell::get))
}
#[test]
fn dense_rows_do_not_rebuild_multiple_intermediate_formats() {
    let layout = Layout {
        id: 17,
        fields: vec![
            ("name".into(), Kind::Text),
            ("profile".into(), Kind::Json),
            ("number".into(), Kind::Int),
        ],
    };
    let document = json!({"name":"sensor","number":i64::MIN,"profile":{"text":"x".repeat(100_000)},"extra":{"blob":"y".repeat(100_000)}});
    let (encoded, encode_bytes) = measured(|| encode_dense_v3(&layout, &document).unwrap());
    let (decoded, decode_bytes) =
        measured(|| decode_dense_v3(&layout, &encoded.row, |_| panic!("no vectors")).unwrap());
    assert_eq!(decoded, document);
    println!(
        "row_bytes={} encode_allocated_bytes={encode_bytes} decode_allocated_bytes={decode_bytes}",
        encoded.row.len()
    );
    assert!(
        encode_bytes <= 4 * encoded.row.len() + 8192,
        "encoder still rebuilds intermediate row buffers"
    );
    assert!(
        decode_bytes <= 3 * encoded.row.len() + 8192,
        "decoder still rebuilds intermediate row buffers"
    );
}

// ---------------------------------------------------------------------------
// The late text build's per-row scratch.
// ---------------------------------------------------------------------------

use sekejap_core::collections::{Database, TextCandidates, TextMatch};
use kernel::{io::IoMode, store::SyncMode};

const DOCUMENTS: u64 = 2_000;

/// Five distinct terms a document, one of them ("river") in every document.
fn body(i: u64) -> String {
    let subject = ["harbour", "mill", "terrace", "orchard", "quarry"][(i % 5) as usize];
    let verb = ["flooded", "settled", "burned", "drained"][(i % 4) as usize];
    format!("the {subject} river {verb} again in the year {i}")
}

/// A packed late build must not allocate per document for work the document's
/// own bytes already hold.
///
/// Three per-row costs used to be paid and are not any more: the scan handed
/// the builder an owned copy of the key and an owned copy of the whole primary
/// row; every posting built a fresh key `Vec`; and every document pushed a
/// norm entry -- key `Vec` plus value `Vec` -- through the external sorter,
/// which then buffered all 2,000 of them in RAM to write them one B-tree entry
/// at a time.
///
/// What is left is genuinely proportional to the corpus and is named rather
/// than hidden: the analyzer allocates one `String` per distinct term per
/// document (its `Analysis` map is the frozen analyzer-v1 contract), and the
/// sorter owns one copy of each posting because sorting them is its job. The
/// bound below is set against those, with room for the store's own paging, and
/// it fails if a per-row `Vec` comes back.
#[test]
fn a_packed_text_build_does_not_allocate_per_row_scratch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let cfg = kernel::store::Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    };
    let mut db = Database::create(&path, cfg).unwrap();
    let docs = db
        .create_collection("docs", vec![("body".into(), Kind::Text)], Default::default())
        .unwrap();
    let mut corpus_bytes = 0usize;
    for i in 0..DOCUMENTS {
        let text = body(i);
        corpus_bytes += text.len();
        db.put(docs, &format!("d{i:05}"), &json!({ "body": text }))
            .unwrap();
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    let index = db.create_text_index(docs, "body_idx", "body").unwrap();
    db.commit().unwrap();

    let (_, bytes, count) = counted(|| db.build_index_to_ready(index, 256).unwrap());
    let per_document = count as f64 / DOCUMENTS as f64;
    println!(
        "TEXT BUILD documents={DOCUMENTS} corpus_bytes={corpus_bytes} \
         allocations={count} ({per_document:.2}/document) allocated_bytes={bytes}"
    );

    // The index still answers.
    let hits = db
        .query_text(
            index,
            "orchard river",
            TextMatch::All,
            5,
            TextCandidates::All,
            1 << 22,
            || false,
        )
        .unwrap();
    assert_eq!(hits.len(), 5);

    // Measured on this corpus: 123.95 allocations and 7,646,066 bytes a build
    // before, 52.84 and 4,375,193 after -- 2.3x fewer allocations for byte
    // identical postings. What remains is NOT free and is not claimed to be:
    // the analyzer builds one `String` per token and per distinct term, which
    // is the frozen analyzer-v1 contract, and the external sorter owns a copy
    // of every posting key and value, which is what sorting them means. Both
    // are O(documents x terms) by construction. The bounds below sit above the
    // measurement with room for machine-to-machine variation in the store's
    // paging, and below the before-figure, so a per-row `Vec` coming back
    // fails the test.
    assert!(
        per_document <= 65.0,
        "the packed build allocates {per_document:.2} times per document, against 52.84 measured and 123.95 before"
    );
    // Nothing may copy the primary row or push a norm through the sorter: 82.4
    // bytes per corpus byte before, 47.2 after.
    assert!(
        bytes <= 55 * corpus_bytes,
        "the packed build allocated {bytes} bytes for a {corpus_bytes}-byte corpus"
    );
}
