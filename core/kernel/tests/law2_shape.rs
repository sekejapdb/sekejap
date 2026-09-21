//! Law 2 as a SHAPE, not a point: writes/row and bytes/row flat across an 8x
//! ladder. Asserted on the data file's own counters, not wall clock (clock
//! moves with machine load; a reading was once discarded for that).
//! One counter-reading test per binary is no longer needed: counters are per
//! store. Sequential keys: that is the path claimed linear; random is a known,
//! separately pinned problem.

use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};

fn per_row(rows: u64) -> (f64, f64) {
    let d = tempfile::TempDir::new().unwrap();
    let cfg = Config { budget_bytes: 16 << 20, io: IoMode::Buffered, sync: SyncMode::Off };
    let mut s = Store::create(d.path(), cfg).unwrap();
    s.io_stats().unwrap().take();
    let v = vec![b'x'; 200];
    for i in 0..rows { s.put(&i.to_be_bytes(), &v).unwrap(); }
    s.commit().unwrap();
    s.checkpoint().unwrap();
    let (w, b, _) = s.io_stats().unwrap().take();
    (w as f64 / rows as f64, b as f64 / rows as f64)
}

#[test]
fn write_cost_per_row_is_flat_as_the_store_grows() {
    let ladder = [100_000u64, 200_000, 400_000, 800_000];
    let mut prev: Option<(f64, f64)> = None;
    let mut first: Option<(f64, f64)> = None;
    for n in ladder {
        let (w, b) = per_row(n);
        eprintln!("{n:>7} rows: {w:.3} writes/row {b:.1} bytes/row");
        if let Some((pw, _)) = prev {
            assert!(w < pw * 1.4 + 0.05, "writes/row rose {pw:.3} -> {w:.3} on a doubling");
        }
        prev = Some((w, b));
        first.get_or_insert((w, b));
    }
    let (fw, fb) = first.unwrap();
    let (lw, lb) = prev.unwrap();
    assert!(lw < fw * 1.5 + 0.05, "writes/row {fw:.3} -> {lw:.3} across 8x: Law 2 inverted");
    assert!(lb < fb * 1.5 + 64.0, "bytes/row {fb:.1} -> {lb:.1} across 8x: amplifying with size");
}
