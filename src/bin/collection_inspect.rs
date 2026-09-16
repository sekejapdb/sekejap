//! Read-only footprint attribution for a V2 page-WAL typed-collection
//! database; run outside benchmark timing. Admits itself as a snapshot reader
//! (one slot, which defers the writer's checkpoint while it runs) and reads
//! every page through the committed-WAL overlay, so a database whose WAL has
//! not been folded yet is attributed exactly as its readers see it.
use e4_prototype::{pagewal::PageWalStore, Result};
use kernel::page::{PageKind, PageRef};
use serde_json::json;
// Bounded depth-first traversal of the published tree; each frame holds one page.
fn occupancy(
    s: &PageWalStore,
    no: u32,
    depth: usize,
    lower: Option<u8>,
    upper: Option<u8>,
    out: &mut [[u64; 8]; 257],
    visited: &mut u64,
) -> Result<()> {
    if depth > 64 {
        return Err("tree too deep".into());
    }
    let bytes = s.page_bytes(no)?;
    let p = PageRef::open(&bytes, no)?;
    *visited += 1;
    if p.kind() == PageKind::Leaf {
        let mut tag = None;
        let mut mixed = false;
        let mut used = 0u64;
        for i in 0..p.nentries() {
            let r = p.slot(i);
            let k = if r[0] == 255 && (0x81..=0x88).contains(&r[1]) {
                &r[1..2 + (r[1] - 0x80) as usize]
            } else if cfg!(feature = "compact-cells") && r[1] & 0xf0 == 0x40 {
                let n = (u16::from_le_bytes(r[..2].try_into()?) & 0x0fff) as usize;
                r.get(2..2 + n).ok_or("compact key crosses slot")?
            } else {
                let n = u16::from_le_bytes(r[..2].try_into()?) as usize;
                &r[2..2 + n]
            };
            let t = k.first().copied();
            if tag.is_some() && tag != t {
                mixed = true;
            }
            tag = t;
            used += r.len() as u64 + 4;
        }
        let index = if p.nentries() == 0 {
            if lower == upper {
                lower.map_or(256, usize::from)
            } else {
                256
            }
        } else if mixed {
            256
        } else {
            tag.map_or(256, usize::from)
        };
        let a = &mut out[index];
        a[0] += 1;
        a[1] += p.nentries() as u64;
        a[2] += used;
        let bucket = if used == 0 {
            3
        } else if used * 4 < 4056 {
            4
        } else if used * 2 < 4056 {
            5
        } else if used * 4 < 4056 * 3 {
            6
        } else {
            7
        };
        a[bucket] += 1;
    } else if p.kind() == PageKind::Interior {
        let mut lo = lower;
        for i in 0..=p.nentries() {
            let child = if i == 0 {
                p.child0()
            } else {
                let r = p.slot(i - 1);
                u32::from_le_bytes(r[r.len() - 4..].try_into()?)
            };
            let hi = if i == p.nentries() {
                upper
            } else {
                p.slot(i).get(2).copied()
            };
            occupancy(s, child, depth + 1, lo, hi, out, visited)?;
            lo = hi;
        }
    } else {
        return Err("non-tree page".into());
    }
    Ok(())
}
fn main() -> Result<()> {
    let path = std::env::args().nth(1).ok_or("database path required")?;
    let dir = std::path::Path::new(&path);
    let store = PageWalStore::open_snapshot(dir, 1 << 20)?;
    let mut stats = [[0u64; 3]; 256];
    for row in store.range(&[])? {
        let (key, value) = row?;
        let tag = *key.first().ok_or("empty key")? as usize;
        stats[tag][0] += 1;
        stats[tag][1] += key.len() as u64;
        stats[tag][2] += value.len() as u64;
    }
    let rows:Vec<_>=stats.iter().enumerate().filter(|(_,s)|s[0]>0).map(|(tag,s)|json!({"tag":format!("{tag:02x}"),"records":s[0],"key_bytes":s[1],"value_bytes":s[2]})).collect();
    let mut density = [[0u64; 8]; 257];
    let mut reachable = 0u64;
    occupancy(&store, store.root(), 0, None, None, &mut density, &mut reachable)?;
    let records = density.iter().map(|a| a[1]).sum::<u64>();
    assert_eq!(records, stats.iter().map(|s| s[0]).sum::<u64>());
    let len = |name: &str| std::fs::metadata(dir.join(name)).map(|m| m.len()).unwrap_or(0);
    let extent = u64::from(store.page_count());
    eprintln!(
        "{}",
        json!({"backend":"page-WAL E4PWAL02","records":records,"reachable_pages":reachable,
        "committed_extent_pages":extent,"data_file_pages":len("data")/4096,
        "wal_bytes":len("wal"),"publication_hint_bytes":len("readers.lock"),
        "nonreachable_nonmeta_pages":extent.saturating_sub(2+reachable),
        "reader_slot":store.reader_slot()})
    );
    let density:Vec<_>=density.iter().enumerate().filter(|(_,a)|a[0]>0).map(|(tag,a)|json!({"tag":if tag==256 {"mixed/unknown".into()}else{format!("{tag:02x}")},"pages":a[0],"records":a[1],"live_cell_bytes":a[2],"occupancy":a[2] as f64/(a[0]*4056) as f64,"empty":a[3],"nonempty_under25":a[4],"25_to50":a[5],"50_to75":a[6],"75_to100":a[7]})).collect();
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"records":rows,"leaf_occupancy":density,"empty_attribution":"same lower and upper fence tag; otherwise unknown"})
        )?
    );
    Ok(())
}
