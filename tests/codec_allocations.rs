//! Bound unnecessary row-sized scratch while preserving the stored bytes.
use e4_prototype::{decode_dense_v3, encode_dense_v3, Kind, Layout};
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout as AllocationLayout, System},
    cell::Cell,
};
thread_local! { static TRACK: Cell<bool> = const {Cell::new(false)}; static BYTES: Cell<usize> = const {Cell::new(0)}; }
struct Alloc;
unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: AllocationLayout) -> *mut u8 {
        TRACK
            .try_with(|t| {
                if t.get() {
                    BYTES.with(|n| n.set(n.get() + l.size()))
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
                    BYTES.with(|b| b.set(b.get() + n))
                }
            })
            .ok();
        System.realloc(p, l, n)
    }
}
#[global_allocator]
static ALLOC: Alloc = Alloc;
fn measured<T>(f: impl FnOnce() -> T) -> (T, usize) {
    BYTES.with(|b| b.set(0));
    TRACK.with(|t| t.set(true));
    let v = f();
    TRACK.with(|t| t.set(false));
    (v, BYTES.with(Cell::get))
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
