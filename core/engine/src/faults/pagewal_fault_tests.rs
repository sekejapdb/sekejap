use super::*;
use std::fs;

#[derive(Default)]
struct Plan { nth:usize, count:usize, mode:u8, fired:bool, trace:Vec<String>, silent_data_page:Option<u32> }
struct FaultFile { inner:Arc<dyn FileIo>, name:&'static str, plan:Arc<Mutex<Plan>>, path:PathBuf }
impl FaultFile {
    fn hit(&self, op:&str)->Option<u8>{
        let mut p=self.plan.lock().unwrap();p.count+=1;
        p.trace.push(format!("{}:{op}",self.name));
        if p.nth==p.count {p.fired=true;Some(p.mode)}else{None}
    }
    fn err(mode:u8)->Error {std::io::Error::from_raw_os_error(if mode==3 {28}else{5}).into()}
    fn simple<T>(&self,op:&str,f:impl FnOnce()->Result<T>)->Result<T>{
        let hit=self.hit(op);if matches!(hit,Some(0|2|3)){return Err(Self::err(hit.unwrap()));}
        let r=f()?;if let Some(mode)=hit{return Err(Self::err(mode));}Ok(r)
    }
}
impl FileIo for FaultFile {
    fn requires_alignment(&self)->bool{false}
    fn read_at(&self,b:&mut[u8],off:u64)->Result<()>{self.simple("read",||self.inner.read_at(b,off))}
    fn write_at(&self,b:&[u8],off:u64)->Result<()>{
        {let mut p=self.plan.lock().unwrap();if self.name=="data"&&p.silent_data_page==Some((off/PAGE as u64)as u32){p.fired=true;return Ok(());}}
        let hit=self.hit("write");match hit {
            Some(0)=>Err(Self::err(0)),
            Some(mode @ (2|3))=>{self.inner.write_at(&b[..b.len()/2+1],off)?;Err(Self::err(mode))},
            Some(mode)=>{self.inner.write_at(b,off)?;Err(Self::err(mode))},
            None=>self.inner.write_at(b,off),
        }
    }
    fn len(&self)->Result<u64>{self.simple("len",||self.inner.len())}
    fn set_len(&self,n:u64)->Result<()>{self.simple("truncate",||self.inner.set_len(n))}
    fn sync_data(&self)->Result<()>{self.simple("sync_data",||self.inner.sync_data())}
    fn sync_full(&self)->Result<()>{self.simple("sync_full",||{
        self.inner.sync_full()?;
        fs::copy(&self.path,self.path.with_extension("durable"))?;Ok(())
    })}
    fn sync_dir(&self)->Result<()>{self.simple("sync_dir",||self.inner.sync_dir())}
    fn sync_full_primitive(&self)->&'static str{self.inner.sync_full_primitive()}
}
fn hooked(p:&Path,plan:Arc<Mutex<Plan>>)->PageWalStore {
    PageWalStore::open_with(p,false,32<<10,|p|{
        let wrap=|name|->Arc<dyn FileIo>{let (inner,_)=io::open_file(&p.join(name),IoMode::Buffered).unwrap();
            fs::copy(p.join(name),p.join(name).with_extension("durable")).unwrap();
            Arc::new(FaultFile{inner:inner.into(),name,plan:plan.clone(),path:p.join(name)})};
        Pager::from_files(wrap("data"),wrap("wal"))
    }).unwrap()
}
fn value(i:u64,v:u8)->Vec<u8>{let mut b=vec![v;256];b[..8].copy_from_slice(&i.to_le_bytes());b}
fn seed(p:&Path){
    let mut s=PageWalStore::open(p,true,32<<10).unwrap();
    for i in 0..120u64{s.put(&i.to_be_bytes(),&value(i,1)).unwrap();}s.commit().unwrap();s.checkpoint().unwrap();
}
fn mutate(s:&mut PageWalStore)->Result<()> {
    for i in 0..120u64{
        if i%3==0 {s.delete(&i.to_be_bytes())?;s.put(&(i+1000).to_be_bytes(),&value(i,2))?;}
        else{s.put(&i.to_be_bytes(),&value(i,2))?;}
    }s.commit()
}
fn verify(p:&Path,required:Option<u8>){
    let s=PageWalStore::open(p,false,32<<10).unwrap_or_else(|e|panic!("reopen {}: {e:?}",p.display()));
    let version=s.get(&1u64.to_be_bytes()).unwrap().unwrap()[8];
    assert!(version==1||version==2);if let Some(v)=required{assert_eq!(version,v);}
    for i in 0..120u64{
        let key=if version==2&&i%3==0{i+1000}else{i};
        assert_eq!(s.get(&key.to_be_bytes()).unwrap(),Some(value(i,version)),"key={key}");
        let absent=if version==2{i}else{i+1000};
        if i%3==0{assert!(s.get(&absent.to_be_bytes()).unwrap().is_none());}
    }
    let mut count=0;s.scan(|_,_|{count+=1;true}).unwrap();assert_eq!(count,120);
}

#[test]
fn io_failure_at_each_commit_and_checkpoint_boundary(){
    let temp=tempfile::tempdir().unwrap();let base=temp.path().join("base");seed(&base);
    let mut cases=0;
    for checkpoint in [false,true]{
        let control=temp.path().join(format!("control-{checkpoint}"));fs::create_dir(&control).unwrap();
        for name in ["data","wal","writer.lock"]{fs::copy(base.join(name),control.join(name)).unwrap();}
        let plan=Arc::new(Mutex::new(Plan::default()));let mut s=hooked(&control,plan.clone());
        if checkpoint{mutate(&mut s).unwrap();}
        *plan.lock().unwrap()=Plan::default();
        if checkpoint{s.checkpoint().unwrap();}else{mutate(&mut s).unwrap();}
        let trace=plan.lock().unwrap().trace.clone();drop(s);
        for nth in 1..=trace.len(){for mode in 0..4{
            let p=temp.path().join(format!("case-{checkpoint}-{nth}-{mode}"));fs::create_dir(&p).unwrap();
            for name in ["data","wal","writer.lock"]{fs::copy(base.join(name),p.join(name)).unwrap();}
            let plan=Arc::new(Mutex::new(Plan::default()));let mut s=hooked(&p,plan.clone());
            if checkpoint{mutate(&mut s).unwrap();}
            *plan.lock().unwrap()=Plan{nth,mode,..Plan::default()};
            let r=if checkpoint{s.checkpoint().map(|_|())}else{mutate(&mut s)};
            assert!(plan.lock().unwrap().fired,"unreached {checkpoint}/{nth}/{mode}");
            assert!(r.is_err(),"swallowed {}",trace[nth-1]);
            assert!(s.get(b"x").is_err(),"writer not poisoned");drop(s);
            eprintln!("checking checkpoint={checkpoint} nth={nth} mode={mode} op={}",trace[nth-1]);
            verify(&p,if checkpoint{Some(2)}else{None});
            // Simulated power loss: discard every write/truncate that did not
            // precede a successful FULL barrier. This is not a hardware cut.
            for name in ["data","wal"]{fs::copy(p.join(name).with_extension("durable"),p.join(name)).unwrap();}
            verify(&p,if checkpoint{Some(2)}else{None});cases+=1;
        }}
    }
    println!("IO_FAILURE_CASES={cases}; REOPEN_CHECKS={}",cases*2);
}

#[test]
fn partial_checkpoint_extension_can_reopen_from_committed_wal(){
    let temp=tempfile::tempdir().unwrap();let p=temp.path().join("db");seed(&p);
    let mut s=PageWalStore::open(&p,false,32<<10).unwrap();
    for i in 120..160u64{s.put(&i.to_be_bytes(),&value(i,2)).unwrap();}s.commit().unwrap();drop(s);
    // A real partial pwrite extending the data file, with committed WAL intact.
    let file=fs::OpenOptions::new().write(true).open(p.join("data")).unwrap();
    file.set_len(file.metadata().unwrap().len()+17).unwrap();drop(file);
    let mut s=PageWalStore::open(&p,false,32<<10).unwrap();
    for i in 0..160u64{assert_eq!(s.get(&i.to_be_bytes()).unwrap(),Some(value(i,if i<120{1}else{2})));}
    s.checkpoint().unwrap();assert_eq!(fs::metadata(p.join("data")).unwrap().len()%PAGE as u64,0);
}

#[test]
fn successful_but_dropped_data_write_preserves_committed_wal(){
    let temp=tempfile::tempdir().unwrap();let p=temp.path().join("db");seed(&p);
    let before=fs::read(p.join("data")).unwrap();
    let no=before.chunks_exact(PAGE).enumerate().skip(2).find(|(n,b)|PageRef::open(b,*n as u32).is_ok_and(|p|p.kind()==PageKind::Leaf)).unwrap().0 as u32;
    let plan=Arc::new(Mutex::new(Plan::default()));let mut s=hooked(&p,plan.clone());mutate(&mut s).unwrap();
    let wal=fs::read(p.join("wal")).unwrap();
    plan.lock().unwrap().silent_data_page=Some(no);
    assert!(s.checkpoint().is_err(),"a successful syscall cannot substitute for read-back verification");
    assert!(plan.lock().unwrap().fired);assert!(s.get(b"x").is_err());
    assert_eq!(fs::read(p.join("wal")).unwrap(),wal,"verification must precede WAL deletion");
    drop(s);verify(&p,Some(2));
}

#[test]
fn checksum_valid_stale_wal_frame_cannot_serve_a_snapshot_or_checkpoint(){
    let temp=tempfile::tempdir().unwrap();let p=temp.path().join("db");seed(&p);
    let original_data=fs::read(p.join("data")).unwrap();
    let mut s=PageWalStore::open(&p,false,32<<10).unwrap();
    s.put(&1u64.to_be_bytes(),&value(1,2)).unwrap();s.commit().unwrap();
    let good=fs::read(p.join("wal")).unwrap();
    let (off,no)=good.chunks_exact(FRAME).enumerate().find_map(|(i,b)|{
        if u32at(b,8)!=1{return None;}let no=u32at(b,12);let page=PageRef::open(&b[32..32+PAGE],no).ok()?;
        (page.kind()==PageKind::Leaf).then_some((i*FRAME,no))
    }).unwrap();
    let old:[u8;PAGE]=original_data[no as usize*PAGE..(no as usize+1)*PAGE].try_into().unwrap();
    let identity=good[off+32+PAGE..off+FRAME].try_into().unwrap();
    let stale=frame(1,no,u64at(&good[off..],16),0,&old,&identity);
    assert_ne!(stale,&good[off..off+FRAME]);PageRef::open(&stale[32..32+PAGE],no).unwrap();
    let snapshot=s.snapshot().unwrap();
    let (wal,_)=io::open_file(&p.join("wal"),IoMode::Buffered).unwrap();wal.write_at(&stale,off as u64).unwrap();wal.sync_full().unwrap();
    assert!(snapshot.get(&1u64.to_be_bytes()).is_err(),"valid old bytes are not the published page version");
    drop(snapshot);let damaged=fs::read(p.join("wal")).unwrap();
    assert!(s.checkpoint().is_err());assert!(s.get(b"x").is_err());drop(s);
    assert_eq!(fs::read(p.join("wal")).unwrap(),damaged);
    assert!(PageWalStore::open(&p,false,32<<10).is_err());
    assert_eq!(fs::read(p.join("wal")).unwrap(),damaged);
    // Restore the test's injected byte substitution, then verify acknowledged data.
    wal.write_at(&good[off..off+FRAME],off as u64).unwrap();wal.sync_full().unwrap();
    let recovered=PageWalStore::open(&p,false,32<<10).unwrap();
    assert_eq!(recovered.get(&1u64.to_be_bytes()).unwrap(),Some(value(1,2)));
}

#[test]
fn checkpoint_preserves_a_valid_header_when_the_other_copy_starts_damaged(){
    let temp=tempfile::tempdir().unwrap();
    let mut cases=0;
    for damaged in 0..2usize {
        let base=temp.path().join(format!("base-{damaged}"));seed(&base);
        let mut data=fs::read(base.join("data")).unwrap();
        data[damaged*PAGE+100]^=1;fs::write(base.join("data"),data).unwrap();

        let control=temp.path().join(format!("control-{damaged}"));fs::create_dir(&control).unwrap();
        for name in ["data","wal","writer.lock"]{fs::copy(base.join(name),control.join(name)).unwrap();}
        let plan=Arc::new(Mutex::new(Plan::default()));let mut s=hooked(&control,plan.clone());
        mutate(&mut s).unwrap();*plan.lock().unwrap()=Plan::default();s.checkpoint().unwrap();
        let trace=plan.lock().unwrap().trace.clone();drop(s);

        for nth in 1..=trace.len() { for mode in 0..4 {
            let p=temp.path().join(format!("case-{damaged}-{nth}-{mode}"));fs::create_dir(&p).unwrap();
            for name in ["data","wal","writer.lock"]{fs::copy(base.join(name),p.join(name)).unwrap();}
            let plan=Arc::new(Mutex::new(Plan::default()));let mut s=hooked(&p,plan.clone());
            mutate(&mut s).unwrap();*plan.lock().unwrap()=Plan{nth,mode,..Plan::default()};
            let r=s.checkpoint();assert!(plan.lock().unwrap().fired,"unreached {damaged}/{nth}/{mode}");
            assert!(r.is_err(),"swallowed {}",trace[nth-1]);drop(s);
            verify(&p,Some(2));
            for name in ["data","wal"]{fs::copy(p.join(name).with_extension("durable"),p.join(name)).unwrap();}
            verify(&p,Some(2));
            cases+=1;
        }}
    }
    println!("DAMAGED_HEADER_CASES={cases}; REOPEN_CHECKS={}",cases*2);
}

#[test]
fn lost_truncate_child() {
    let Ok(p)=std::env::var("E4_PAGEWAL_LOST_TRUNCATE") else { return; };
    let p=std::path::Path::new(&p);
    seed(p);
    let mut s=PageWalStore::open(p,false,32<<10).unwrap();
    mutate(&mut s).unwrap();
    s.test_checkpoint_crash(7).unwrap();
    panic!("fault 7 did not terminate child");
}

#[test]
fn lost_wal_truncate_keeps_checkpoint_floor_and_refuses_foreign_identity() {
    let temp=tempfile::tempdir().unwrap();let p=temp.path().join("db");
    let result=std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact","store::pagewal::fault_tests::lost_truncate_child","--nocapture"])
        .env("E4_PAGEWAL_LOST_TRUNCATE",&p).status().unwrap();
    assert_eq!(result.code(),Some(86));
    let mut s=PageWalStore::open(&p,false,32<<10).unwrap();
    for i in 0..120u64{
        let key=if i%3==0{i+1000}else{i};
        assert_eq!(s.get(&key.to_be_bytes()).unwrap(),Some(value(i,2)));
        if i%3==0{assert!(s.get(&i.to_be_bytes()).unwrap().is_none());}
    }
    let snap=s.snapshot().unwrap();
    for i in 0..120u64{
        let key=if i%3==0{i+1000}else{i};
        assert_eq!(snap.get(&key.to_be_bytes()).unwrap(),Some(value(i,2)));
    }
    drop(snap);
    for i in 0..120u64{s.put(&i.to_be_bytes(),&value(i,3)).unwrap();}s.commit().unwrap();drop(s);
    let s=PageWalStore::open(&p,false,32<<10).unwrap();
    for i in 0..120u64{assert_eq!(s.get(&i.to_be_bytes()).unwrap(),Some(value(i,3)));}
    drop(s);
    let foreign=temp.path().join("foreign");
    let mut other=PageWalStore::open(&foreign,true,32<<10).unwrap();
    other.put(b"x",b"y").unwrap();other.commit().unwrap();drop(other);
    fs::copy(foreign.join("wal"),p.join("wal")).unwrap();
    let before=[fs::read(p.join("data")).unwrap(),fs::read(p.join("wal")).unwrap(),fs::read(p.join("writer.lock")).unwrap()];
    assert!(PageWalStore::open(&p,false,32<<10).is_err(),"foreign identity WAL must refuse (64b6663)");
    assert_eq!(before,[fs::read(p.join("data")).unwrap(),fs::read(p.join("wal")).unwrap(),fs::read(p.join("writer.lock")).unwrap()]);
}
