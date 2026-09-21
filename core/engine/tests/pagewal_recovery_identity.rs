use sekejap_core::pagewal::PageWalStore;
use kernel::page::{PageKind, PageMut, PageRef, PAGE_SIZE, seal};
use std::{fs, io::Write, path::Path};

fn bytes(p:&Path)->[Vec<u8>;3] {
    [fs::read(p.join("data")).unwrap(),fs::read(p.join("wal")).unwrap(),fs::read(p.join("writer.lock")).unwrap()]
}
fn seed(p:&Path,base:&[u8],pending:&[u8]) {
    let mut db=PageWalStore::open(p,true,32<<10).unwrap();
    db.put(b"key",base).unwrap();db.commit().unwrap();db.checkpoint().unwrap();
    db.put(b"key",pending).unwrap();db.commit().unwrap();
}
fn refuses_unchanged(p:&Path) {
    let before=bytes(p);
    assert!(PageWalStore::open(p,false,32<<10).is_err());
    assert_eq!(before,bytes(p));
}
fn rewrite_header(p:&Path,no:usize,f:impl FnOnce(&mut Vec<u8>)) {
    let mut data=fs::read(p.join("data")).unwrap();
    let b=&mut data[no*PAGE_SIZE..(no+1)*PAGE_SIZE];
    let mut h=PageRef::open(b,no as u32).unwrap().slot(0).to_vec();f(&mut h);
    let mut page=PageMut::init(b,PageKind::Meta,0,no as u32);
    page.insert_slot(0,&h).unwrap();page.finalise(0);seal(b,1);
    PageRef::open(b,no as u32).unwrap();fs::write(p.join("data"),data).unwrap();
}
#[test]
fn foreign_valid_wal_refuses_before_mutation() {
    let t=tempfile::tempdir().unwrap();let a=t.path().join("a");let b=t.path().join("b");
    seed(&a,b"a-base",b"a-pending");seed(&b,b"b-base",b"b-pending");
    fs::copy(b.join("wal"),a.join("wal")).unwrap();refuses_unchanged(&a);
}
#[test]
fn stale_valid_wal_cannot_roll_back_a_later_checkpoint() {
    let t=tempfile::tempdir().unwrap();let p=t.path().join("db");seed(&p,b"base",b"old");
    let old=fs::read(p.join("wal")).unwrap();
    let mut db=PageWalStore::open(&p,false,32<<10).unwrap();
    db.put(b"key",b"newest").unwrap();db.commit().unwrap();db.checkpoint().unwrap();drop(db);
    fs::write(p.join("wal"),old).unwrap();refuses_unchanged(&p);
}
#[test]
fn unsupported_metadata_or_features_preserve_tail_and_both_headers() {
    let t=tempfile::tempdir().unwrap();
    for no in 0..2 { for features in [false,true] {
        let p=t.path().join(format!("{no}-{features}"));seed(&p,b"base",b"pending");
        let mut db=PageWalStore::open(&p,false,32<<10).unwrap();db.checkpoint().unwrap();drop(db);
        rewrite_header(&p,no,|h|if features {h[55]|=0x80;} else {h[0]^=0x20;});
        fs::OpenOptions::new().append(true).open(p.join("wal")).unwrap().write_all(b"future-format-tail").unwrap();
        refuses_unchanged(&p);
    }}
}
#[test]
fn invalid_root_is_refused_before_tail_cleanup() {
    let t=tempfile::tempdir().unwrap();let p=t.path().join("db");seed(&p,b"base",b"pending");
    let mut db=PageWalStore::open(&p,false,32<<10).unwrap();db.checkpoint().unwrap();drop(db);
    for no in 0..2 {rewrite_header(&p,no,|h|h[8..12].copy_from_slice(&u32::MAX.to_le_bytes()));}
    fs::write(p.join("wal"),b"uncommitted-tail").unwrap();refuses_unchanged(&p);
}
#[test]
fn one_damaged_checkpoint_header_can_reopen_and_continue_writing() {
    let t=tempfile::tempdir().unwrap();
    for no in 0..2 {
        let p=t.path().join(format!("copy-{no}"));seed(&p,b"base",b"newest");
        let mut db=PageWalStore::open(&p,false,32<<10).unwrap();db.checkpoint().unwrap();drop(db);
        let mut b=fs::read(p.join("data")).unwrap();b[no*PAGE_SIZE+100]^=1;fs::write(p.join("data"),b).unwrap();
        let mut db=PageWalStore::open(&p,false,32<<10).unwrap();
        assert_eq!(db.get(b"key").unwrap().as_deref(),Some(b"newest".as_slice()));
        db.put(b"key",b"continued").unwrap();db.commit().unwrap();db.checkpoint().unwrap();drop(db);
        assert_eq!(PageWalStore::open(&p,false,32<<10).unwrap().get(b"key").unwrap().as_deref(),Some(b"continued".as_slice()));
    }
}
#[test]
fn interrupted_reset_can_reopen_write_and_reopen_again() {
    let t=tempfile::tempdir().unwrap();let p=t.path().join("db");seed(&p,b"base",b"committed");
    let before_reset=fs::read(p.join("wal")).unwrap();
    let mut db=PageWalStore::open(&p,false,32<<10).unwrap();db.checkpoint().unwrap();drop(db);
    // Equivalent durable files to a checkpoint completed before WAL reset.
    fs::write(p.join("wal"),before_reset).unwrap();
    let mut db=PageWalStore::open(&p,false,32<<10).unwrap();
    assert_eq!(db.get(b"key").unwrap().as_deref(),Some(b"committed".as_slice()));
    db.put(b"key",b"after-recovery").unwrap();db.commit().unwrap();drop(db);
    let mut db=PageWalStore::open(&p,false,32<<10).unwrap();
    assert_eq!(db.get(b"key").unwrap().as_deref(),Some(b"after-recovery".as_slice()));
    db.checkpoint().unwrap();
}
