use e4_prototype::pagewal::PageWalStore;
use std::fs;
fn dir()->tempfile::TempDir{tempfile::tempdir().unwrap()}
fn val(i:u64,version:u8,len:usize)->Vec<u8>{let mut v=vec![version;len];v[..8].copy_from_slice(&i.to_le_bytes());v}
fn check(s:&PageWalStore,n:u64,version:u8,len:usize){
    for i in 0..n{assert_eq!(s.get(&i.to_be_bytes()).unwrap(),Some(val(i,version,len)));}
    let mut count=0;s.scan(|_,_|{count+=1;true}).unwrap();assert_eq!(count,n);
}
#[test]
fn commit_reopen_and_overflow_resize(){
    let d=dir();let p=d.path().join("db");let mut s=PageWalStore::open(&p,true,64<<10).unwrap();
    for (version,len) in [(1,256),(2,8192),(3,64)] {
        for i in (0..120u64).rev(){s.put(&i.to_be_bytes(),&val(i,version,len)).unwrap();}
        s.commit().unwrap();check(&s,120,version,len);drop(s);
        s=PageWalStore::open(&p,false,64<<10).unwrap();check(&s,120,version,len);
    }
    assert!(s.checkpoint().unwrap());drop(s);check(&PageWalStore::open(&p,false,64<<10).unwrap(),120,3,64);
}
#[test]
fn delete_reinsert_and_snapshot_versions(){
    let d=dir();let p=d.path().join("db");let mut s=PageWalStore::open(&p,true,64<<10).unwrap();
    for i in 0..120u64{s.put(&i.to_be_bytes(),&val(i,1,256)).unwrap();}s.commit().unwrap();
    let old=s.snapshot().unwrap();
    for i in 0..120u64{assert!(s.delete(&i.to_be_bytes()).unwrap());s.put(&(i+1000).to_be_bytes(),&val(i,2,256)).unwrap();}
    s.commit().unwrap();assert!(!s.checkpoint().unwrap());check(&old,120,1,256);
    let latest=s.snapshot().unwrap();for i in 0..120u64{
        assert!(latest.get(&i.to_be_bytes()).unwrap().is_none());
        assert_eq!(latest.get(&(i+1000).to_be_bytes()).unwrap(),Some(val(i,2,256)));}
    drop(latest);drop(old);assert!(s.checkpoint().unwrap());drop(s);
    let s=PageWalStore::open(&p,false,64<<10).unwrap();for i in 0..120u64{assert!(s.get(&i.to_be_bytes()).unwrap().is_none());}
}
#[test]
fn uncommitted_evictions_never_become_visible_after_reopen(){
    let d=dir();let p=d.path().join("db");let mut s=PageWalStore::open(&p,true,32<<10).unwrap();
    for i in 0..100u64{s.put(&i.to_be_bytes(),&val(i,1,256)).unwrap();}s.commit().unwrap();
    for i in 0..100u64{s.put(&i.to_be_bytes(),&val(i,2,8192)).unwrap();}drop(s);
    check(&PageWalStore::open(&p,false,32<<10).unwrap(),100,1,256);
}
#[test]
fn corrupt_wal_refuses_without_modifying_evidence(){
    let d=dir();let p=d.path().join("db");let mut s=PageWalStore::open(&p,true,64<<10).unwrap();
    s.put(b"a",b"committed").unwrap();s.commit().unwrap();drop(s);
    let wal=p.join("wal");let mut b=fs::read(&wal).unwrap();b[100]^=1;fs::write(&wal,&b).unwrap();
    assert!(PageWalStore::open(&p,false,64<<10).is_err());assert_eq!(fs::read(&wal).unwrap(),b);
}
#[test]
fn cap_refusal_preserves_published_snapshot_and_reopen(){
    let d=dir();let p=d.path().join("db");let mut s=PageWalStore::open(&p,true,32<<10).unwrap();
    for i in 0..100u64{s.put(&i.to_be_bytes(),&val(i,1,256)).unwrap();}s.commit().unwrap();s.checkpoint().unwrap();
    let old=s.snapshot().unwrap();let loaded=fs::metadata(p.join("data")).unwrap().len();let cap=loaded*2;s.set_cap(cap).unwrap();
    let mut refused=false;for i in 0..100u64{if s.put(&i.to_be_bytes(),&val(i,2,8192)).is_err(){refused=true;break;}}
    assert!(refused);assert!(fs::metadata(p.join("data")).unwrap().len()+fs::metadata(p.join("wal")).unwrap().len()<=cap);
    check(&old,100,1,256);drop(old);drop(s);check(&PageWalStore::open(&p,false,32<<10).unwrap(),100,1,256);
}
#[test]
fn surviving_snapshot_keeps_wal_reset_ownership(){
    let d=dir();let p=d.path().join("db");let mut s=PageWalStore::open(&p,true,32<<10).unwrap();
    s.put(b"a",b"old").unwrap();s.commit().unwrap();let old=s.snapshot().unwrap();drop(s);
    assert!(PageWalStore::open(&p,false,32<<10).is_err());assert_eq!(old.get(b"a").unwrap(),Some(b"old".to_vec()));
    drop(old);assert!(PageWalStore::open(&p,false,32<<10).is_ok());
}
#[test]
fn free_new_overflow_before_its_first_flush(){
    let d=dir();let p=d.path().join("db");let mut s=PageWalStore::open(&p,true,8<<20).unwrap();
    s.put(b"fresh",&vec![9;8192]).unwrap();assert!(s.delete(b"fresh").unwrap());
    s.commit().unwrap();drop(s);assert!(PageWalStore::open(&p,false,8<<20).unwrap().get(b"fresh").unwrap().is_none());
}
#[test]
fn crash_child(){
    let Ok(p)=std::env::var("E4_PAGEWAL_CRASH_PATH") else{return;};
    let stage:u8=std::env::var("E4_PAGEWAL_CRASH_STAGE").unwrap().parse().unwrap();
    let mut s=PageWalStore::open(std::path::Path::new(&p),true,64<<10).unwrap();
    for i in 0..200u64{s.put(&i.to_be_bytes(),&val(i,1,256)).unwrap();}s.commit().unwrap();s.checkpoint().unwrap();
    for i in 0..200u64{s.put(&i.to_be_bytes(),&val(i,2,256)).unwrap();}s.commit().unwrap();
    s.test_checkpoint_crash(stage).unwrap();panic!("fault did not terminate child");
}
#[test]
fn checkpoint_process_death_preserves_acknowledged_rows(){
    let d=dir();for stage in 1..=4{
        let p=d.path().join(format!("crash-{stage}"));
        let result=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","crash_child","--nocapture"])
            .env("E4_PAGEWAL_CRASH_PATH",&p).env("E4_PAGEWAL_CRASH_STAGE",stage.to_string()).status().unwrap();
        assert_eq!(result.code(),Some(86));let s=PageWalStore::open(&p,false,64<<10).unwrap();check(&s,200,2,256);
    }
}
#[test]
fn repeated_small_transactions_complete_under_twice_loaded_size(){
    let d=dir();let p=d.path().join("db");let mut s=PageWalStore::open(&p,true,64<<10).unwrap();
    for i in 0..1000u64{s.put(&i.to_be_bytes(),&val(i,1,256)).unwrap();}s.commit().unwrap();s.checkpoint().unwrap();
    let cap=2*fs::metadata(p.join("data")).unwrap().len();s.set_cap(cap).unwrap();
    for version in 2..=6{for i in 0..1000u64{s.put(&i.to_be_bytes(),&val(i,version,256)).unwrap();if i%50==49{s.commit().unwrap();}}
        s.commit().unwrap();check(&s,1000,version,256);
        assert!(fs::metadata(p.join("data")).unwrap().len()+fs::metadata(p.join("wal")).unwrap().len()<=cap);
    }
    s.checkpoint().unwrap();drop(s);check(&PageWalStore::open(&p,false,64<<10).unwrap(),1000,6,256);
}
#[test]
fn new_snapshot_during_uncommitted_changes_sees_latest_commit(){
    let d=dir();let p=d.path().join("db");let mut s=PageWalStore::open(&p,true,32<<10).unwrap();
    s.put(b"a",b"committed").unwrap();s.commit().unwrap();s.put(b"a",b"pending").unwrap();
    let old=s.snapshot().unwrap();assert_eq!(old.get(b"a").unwrap(),Some(b"committed".to_vec()));
    s.commit().unwrap();let newest=s.snapshot().unwrap();assert_eq!(newest.get(b"a").unwrap(),Some(b"pending".to_vec()));
    assert_eq!(old.get(b"a").unwrap(),Some(b"committed".to_vec()));
}

#[test]
fn managed_disk_cap_survives_reopen(){
    let d=dir();let p=d.path().join("db");let mut s=PageWalStore::open(&p,true,32<<10).unwrap();
    for i in 0..100u64{s.put(&i.to_be_bytes(),&val(i,1,256)).unwrap();}s.commit().unwrap();s.checkpoint().unwrap();
    let cap=fs::metadata(p.join("data")).unwrap().len()*2;s.set_cap(cap).unwrap();drop(s);
    let mut s=PageWalStore::open(&p,false,32<<10).unwrap();let old=s.snapshot().unwrap();
    let mut refused=false;
    for i in 0..100u64{if s.put(&i.to_be_bytes(),&val(i,2,8192)).is_err(){refused=true;break;}}
    assert!(refused,"reopen forgot the disk policy");
    assert!(fs::metadata(p.join("data")).unwrap().len()+fs::metadata(p.join("wal")).unwrap().len()<=cap);
    check(&old,100,1,256);
}

#[test]
fn snapshot_admission_is_bounded_and_released_on_drop(){
    let d=dir();let p=d.path().join("db");let s=PageWalStore::open(&p,true,32<<10).unwrap();
    let mut views=Vec::new();for _ in 0..8{views.push(s.snapshot().unwrap());}
    assert!(s.snapshot().is_err(),"unbounded per-reader cache/index allocation");
    views.pop();views.push(s.snapshot().unwrap());
}
