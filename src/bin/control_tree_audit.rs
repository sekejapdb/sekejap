//! Read-only physical tree audit; bypasses the pool and range iterator.
use kernel::{
    io::{open_recovery_source, FileIo},
    page::{PageKind, PageRef, PAGE_SIZE},
    verify::{decode_record, DecodedRecord},
};
use serde_json::{json, Value};
use std::path::Path;
fn walk(
    file: &dyn FileIo,
    no: u32,
    lo: Option<Vec<u8>>,
    hi: Option<Vec<u8>>,
    path: &mut Vec<(u32, usize)>,
    out: &mut Vec<Value>,
    rows: &mut u64,
    bad_rows: &mut u64,
    remaining: &mut u64,
) -> Result<(), Box<dyn std::error::Error>> {
    if path.len() >= 64 || *remaining == 0 {
        return Err(kernel::Error::Corrupt {
            page_no: no,
            why: "audit traversal exceeds depth or file-page allowance",
        }
        .into());
    }
    *remaining -= 1;
    let mut b = [0; PAGE_SIZE];
    file.read_at(&mut b, no as u64 * PAGE_SIZE as u64)?;
    let p = PageRef::open(&b, no)?;
    if p.kind() == PageKind::Leaf {
        let mut previous: Option<Vec<u8>> = None;
        for i in 0..p.nentries() {
            let DecodedRecord::Leaf { key, value, .. } = decode_record(p.slot(i), no, p.kind())?
            else {
                panic!()
            };
            *rows += 1;
            let id = u64::from_be_bytes(key.try_into()?);
            let bad = lo.as_ref().is_some_and(|l| key < l.as_slice())
                || hi.as_ref().is_some_and(|h| key >= h.as_slice())
                || previous.as_ref().is_some_and(|l| key <= l.as_slice());
            if bad {
                *bad_rows += 1;
            }
            if out.len() < 64 && (bad || (395060..395110).contains(&id)) {
                out.push(json!({"page":no,"slot":i,"key":id,"version":u64::from_le_bytes(value.get(8..16).ok_or("audit expects raw versioned values")?.try_into()?),"bad_bounds":bad,"path":path,"lo":lo,"hi":hi,"generation":p.lsn()}));
            }
            previous = Some(key.to_vec());
        }
    } else {
        if p.kind() != PageKind::Interior {
            return Err(kernel::Error::Corrupt {
                page_no: no,
                why: "audit reached non-tree page",
            }
            .into());
        }
        let mut children = vec![p.child0()];
        let mut keys = Vec::new();
        for i in 0..p.nentries() {
            let DecodedRecord::Interior { key, child } = decode_record(p.slot(i), no, p.kind())?
            else {
                panic!()
            };
            keys.push(key.to_vec());
            children.push(child);
        }
        for (i, child) in children.into_iter().enumerate() {
            path.push((no, i));
            walk(
                file,
                child,
                if i == 0 {
                    lo.clone()
                } else {
                    Some(keys[i - 1].clone())
                },
                if i == keys.len() {
                    hi.clone()
                } else {
                    Some(keys[i].clone())
                },
                path,
                out,
                rows,
                bad_rows,
                remaining,
            )?;
            path.pop();
        }
    }
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<_> = std::env::args().collect();
    let f = open_recovery_source(&Path::new(&a[1]).join("data"))?;
    let root = a[2].parse()?;
    let mut out = Vec::new();
    let mut rows = 0;
    let mut bad_rows = 0;
    let mut remaining = f.len()? / PAGE_SIZE as u64;
    walk(
        &*f,
        root,
        None,
        None,
        &mut Vec::new(),
        &mut out,
        &mut rows,
        &mut bad_rows,
        &mut remaining,
    )?;
    println!(
        "{}",
        json!({"rows":rows,"bad_bound_rows":bad_rows,"findings":out,"preview_limit":64})
    );
    Ok(())
}
