//! Read-only page occupancy diagnosis for the fixed-work benchmark.
use kernel::{
    io::open_recovery_source,
    page::{PageKind, PageRef, PAGE_SIZE},
};
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<_> = std::env::args().collect();
    if a.len() != 3 {
        return Err("pagewal|sqlite DB_DIRECTORY".into());
    }
    let path = Path::new(&a[2]);
    if a[1] == "pagewal" {
        let file = open_recovery_source(&path.join("data"))?;
        assert_eq!(file.len()? % PAGE_SIZE as u64, 0);
        assert_eq!(
            std::fs::metadata(path.join("wal"))?.len(),
            0,
            "checkpoint first"
        );
        let mut kinds = BTreeMap::<String, u64>::new();
        let mut leaf_histogram = BTreeMap::<usize, u64>::new();
        let (mut leaf_records, mut leaf_unused) = (0u64, 0u64);
        for no in 0..file.len()? / PAGE_SIZE as u64 {
            let mut b = [0; PAGE_SIZE];
            file.read_at(&mut b, no * PAGE_SIZE as u64)?;
            // The second reserved header page is allowed to be unwritten.
            if no == 1 && b.iter().all(|v| *v == 0) {
                *kinds.entry("reserved_zero".into()).or_default() += 1;
                continue;
            }
            let page = PageRef::open(&b, u32::try_from(no)?)?;
            *kinds.entry(format!("{:?}", page.kind())).or_default() += 1;
            if page.kind() == PageKind::Leaf {
                *leaf_histogram.entry(page.nentries()).or_default() += 1;
                leaf_records += page.nentries() as u64;
                // Sum actual occupied cells; fragmented free_ptr alone is insufficient.
                let used: usize = (0..page.nentries()).map(|i| page.slot(i).len() + 4).sum();
                leaf_unused += (PAGE_SIZE - kernel::page::HEADER_LEN - used) as u64;
            }
        }
        println!(
            "{}",
            json!({"engine":"pagewal","physical_pages":file.len()?/PAGE_SIZE as u64,
            "page_kinds":kinds,"leaf_records":leaf_records,"leaf_unused_bytes":leaf_unused,
            "leaf_cells_histogram":leaf_histogram,"scope":"CRC/bounds-verified physical page inventory; not a reachability proof"})
        );
    } else {
        assert_eq!(a[1], "sqlite");
        let c = Connection::open_with_flags(
            path.join("data.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let mut q=c.prepare("SELECT pagetype,ncell,count(*),sum(unused) FROM dbstat WHERE name='kv' GROUP BY pagetype,ncell ORDER BY pagetype,ncell")?;
        let mut rs = q.query([])?;
        let mut rows = Vec::new();
        while let Some(r) = rs.next()? {
            rows.push(
                json!({"kind":r.get::<_,String>(0)?,"cells":r.get::<_,u64>(1)?,
                "pages":r.get::<_,u64>(2)?,"unused_bytes":r.get::<_,u64>(3)?}),
            );
        }
        let free: u64 = c.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let pages: u64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        println!(
            "{}",
            json!({"engine":"sqlite","page_count":pages,"free_pages":free,"kv_page_histogram":rows,
            "scope":"SQLite dbstat physical occupancy; WITHOUT ROWID stores records in interior pages too"})
        );
    }
    Ok(())
}
