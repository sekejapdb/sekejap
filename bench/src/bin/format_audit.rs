//! Negative disk-format probes. Exit 1 means an upgrade-safety gap was found.
//! Run only on disposable copies under the authorized native artifact roots.
use sekejap_core::pagewal::PageWalStore;
use kernel::page::{PageKind, PageMut, PageRef, PAGE_SIZE};
use serde_json::json;
use std::{fs, io::Write, path::{Path, PathBuf}};
type R<T> = Result<T, Box<dyn std::error::Error>>;

fn copy_db(source: &Path, target: &Path) -> R<()> {
    fs::create_dir(target)?;
    for name in ["data", "wal", "writer.lock"] { fs::copy(source.join(name), target.join(name))?; }
    Ok(())
}
fn seed(path: &Path, base: &[u8], pending: &[u8]) -> R<()> {
    let mut db = PageWalStore::open(path, true, 64 << 10)?;
    db.put(b"key", base)?; db.commit()?; db.checkpoint()?;
    db.put(b"key", pending)?; db.commit()?;
    assert_eq!(db.get(b"key")?.as_deref(), Some(pending));
    Ok(())
}
fn observed(path: &Path) -> R<(bool, Option<Vec<u8>>, Option<String>)> {
    match PageWalStore::open(path, false, 64 << 10) {
        Ok(db) => match db.get(b"key") {
            Ok(value) => Ok((false, value, None)),
            Err(e) => Ok((true, None, Some(format!("{e:?}")))),
        },
        Err(e) => Ok((true, None, Some(format!("{e:?}")))),
    }
}
fn main() -> R<()> {
    let root = PathBuf::from(std::env::args().nth(1).ok_or("OUTPUT required")?);
    let parent = fs::canonicalize(root.parent().ok_or("parent required")?)?;
    assert!(parent.starts_with("<scratch>")
        || parent.starts_with("<scratch>"));
    fs::create_dir(&root)?;
    let a = root.join("source-a"); let b = root.join("source-b");
    seed(&a, b"a-checkpoint", b"a-committed-wal")?;
    seed(&b, b"b-checkpoint", b"b-committed-wal")?;
    let a_wal = fs::read(a.join("wal"))?;
    let b_wal = fs::read(b.join("wal"))?;
    assert!(!a_wal.is_empty() && !b_wal.is_empty() && a_wal != b_wal);
    let foreign = root.join("foreign-wal"); copy_db(&a, &foreign)?;
    fs::write(foreign.join("wal"), &b_wal)?;
    let foreign_before = [fs::read(foreign.join("data"))?, fs::read(foreign.join("wal"))?];
    let (foreign_refused, foreign_value, foreign_error) = observed(&foreign)?;
    let foreign_unchanged = foreign_before == [fs::read(foreign.join("data"))?, fs::read(foreign.join("wal"))?];

    let mut db = PageWalStore::open(&a, false, 64 << 10)?;
    db.put(b"key", b"a-newer-checkpoint")?; db.commit()?; db.checkpoint()?; drop(db);
    assert!(fs::read(a.join("wal"))?.is_empty());
    assert_eq!(observed(&a)?.1.as_deref(), Some(b"a-newer-checkpoint".as_slice()));
    let stale = root.join("stale-wal"); copy_db(&a, &stale)?;
    fs::write(stale.join("wal"), &a_wal)?;
    let (stale_refused, stale_value, stale_error) = observed(&stale)?;
    let stale_safe = stale_refused || stale_value.as_deref() == Some(b"a-newer-checkpoint".as_slice());

    let unknown = root.join("unknown-header"); copy_db(&a, &unknown)?;
    let mut data = fs::read(unknown.join("data"))?;
    let page = PageRef::open(&data[..PAGE_SIZE], 0)?;
    let mut header = page.slot(0).to_vec(); header[0] ^= 0x20;
    let mut page = PageMut::init(&mut data[..PAGE_SIZE], PageKind::Meta, 0, 0);
    page.insert_slot(0, &header)?; page.finalise(0);
    // finalise stamps metadata only; seal the altered disk fixture explicitly.
    // Assert validity here so a checksum failure cannot masquerade as a
    // format-version refusal in this diagnostic.
    let crc = crc32c::crc32c_append(crc32c::crc32c(&data[..36]), &data[40..PAGE_SIZE]);
    data[36..40].copy_from_slice(&crc.to_le_bytes());
    let checked = PageRef::open(&data[..PAGE_SIZE], 0)?;
    assert_eq!(checked.slot(0), header);
    fs::write(unknown.join("data"), &data)?;
    // An unknown format's tail belongs to that format; do not classify it as
    // discardable pilot WAL before establishing the file's supported version.
    let tail = b"future-format-tail";
    let mut file = fs::OpenOptions::new().append(true).open(unknown.join("wal"))?;
    file.write_all(tail)?; file.sync_all()?; drop(file);
    let (unknown_refused, _, unknown_error) = observed(&unknown)?;
    let unknown_unchanged = fs::read(unknown.join("data"))? == data
        && fs::read(unknown.join("wal"))? == tail;
    let safe = foreign_refused && foreign_unchanged && stale_safe && unknown_refused && unknown_unchanged;
    let report = json!({"diagnostic":"pre-freeze negative format probes", "safe":safe,
        "foreign_wal":{"refused":foreign_refused,"value":foreign_value,"error":foreign_error,"source_unchanged":foreign_unchanged},
        "stale_wal":{"refused":stale_refused,"value":stale_value,"error":stale_error,"preserves_newer_commit_or_refuses":stale_safe},
        "unknown_header":{"refused":unknown_refused,"source_unchanged":unknown_unchanged,"error":unknown_error},
        "scope":"tiny disposable copies; not crash, production, or full upgrade qualification"});
    fs::write(root.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    if !safe { std::process::exit(1); }
    Ok(())
}
