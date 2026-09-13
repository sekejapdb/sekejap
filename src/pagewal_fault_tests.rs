use super::*;
use std::fs;

#[derive(Default)]
struct Plan { nth:usize, count:usize, mode:u8, fired:bool, trace:Vec<String> }
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
        for name in ["data","wal"]{fs::copy(base.join(name),control.join(name)).unwrap();}
        let plan=Arc::new(Mutex::new(Plan::default()));let mut s=hooked(&control,plan.clone());
        if checkpoint{mutate(&mut s).unwrap();}
        *plan.lock().unwrap()=Plan::default();
        if checkpoint{s.checkpoint().unwrap();}else{mutate(&mut s).unwrap();}
        let trace=plan.lock().unwrap().trace.clone();drop(s);
        for nth in 1..=trace.len(){for mode in 0..4{
            let p=temp.path().join(format!("case-{checkpoint}-{nth}-{mode}"));fs::create_dir(&p).unwrap();
            for name in ["data","wal"]{fs::copy(base.join(name),p.join(name)).unwrap();}
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
