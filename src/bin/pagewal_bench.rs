use e4_prototype::pagewal::PageWalStore;
use kernel::{store::{Store,Config,SyncMode},io::IoMode};
use rusqlite::{Connection,params};
use serde_json::{json,Value};
use std::{fs,path::{Path,PathBuf},time::{Instant,Duration},sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}}};
type R<T>=Result<T,Box<dyn std::error::Error>>;
enum Db{E4(Store),Page(PageWalStore),Sql(Connection)}
impl Db{
 fn new(p:&Path,engine:&str)->R<Self>{Ok(match engine{
  "e4"=>Self::E4(Store::create(p,Config{budget_bytes:8<<20,io:IoMode::Buffered,sync:SyncMode::Full})?),
  "pagewal"=>Self::Page(PageWalStore::open(p,true,8<<20)?),
  "sqlite"=>{fs::create_dir(p)?;let c=Connection::open(p.join("data.sqlite"))?;
   c.execute_batch("PRAGMA page_size=4096;PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA fullfsync=ON;PRAGMA checkpoint_fullfsync=ON;PRAGMA wal_autocheckpoint=1000;PRAGMA cache_size=-8192;PRAGMA mmap_size=0;CREATE TABLE kv(k BLOB PRIMARY KEY,v BLOB NOT NULL) WITHOUT ROWID;")?;Self::Sql(c)},_=>return Err("unknown engine".into())})}
 fn reopen(p:&Path,engine:&str)->R<Self>{Ok(match engine{
  "e4"=>Self::E4(Store::open(p,Config{budget_bytes:8<<20,io:IoMode::Buffered,sync:SyncMode::Full})?),
  "pagewal"=>Self::Page(PageWalStore::open(p,false,8<<20)?),
  _=>Self::Sql(Connection::open(p.join("data.sqlite"))?),})}
 fn begin(&self)->R<()>{if let Self::Sql(c)=self{c.execute_batch("BEGIN")?;}Ok(())}
 fn commit(&mut self)->R<()>{match self{Self::E4(s)=>s.checkpoint()?,Self::Page(s)=>s.commit()?,Self::Sql(c)=>c.execute_batch("COMMIT")?};Ok(())}
 fn checkpoint(&mut self)->R<()>{match self{Self::E4(_)=>(),Self::Page(s)=>{assert!(s.checkpoint()?);},Self::Sql(c)=>{let busy:i64=c.query_row("PRAGMA wal_checkpoint(TRUNCATE)",[],|r|r.get(0))?;assert_eq!(busy,0);}}Ok(())}
 fn put(&mut self,id:u64,v:&[u8])->R<()>{let k=id.to_be_bytes();match self{Self::E4(s)=>s.put(&k,v)?,Self::Page(s)=>s.put(&k,v)?,Self::Sql(c)=>{c.prepare_cached("INSERT INTO kv VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v")?.execute(params![k.as_slice(),v])?;}}Ok(())}
 fn del(&mut self,id:u64)->R<()>{let k=id.to_be_bytes();match self{Self::E4(s)=>assert!(s.delete(&k)?),Self::Page(s)=>assert!(s.delete(&k)?),Self::Sql(c)=>assert_eq!(c.prepare_cached("DELETE FROM kv WHERE k=?1")?.execute(params![k.as_slice()])?,1)}Ok(())}
 fn scan(&self,mut f:impl FnMut(&[u8],&[u8]))->R<()>{match self{
  Self::E4(s)=>s.scan(&[])?.for_each_ref(|k,v|{f(k,v);true})?,Self::Page(s)=>s.scan(|k,v|{f(k,v);true})?,
  Self::Sql(c)=>{let mut q=c.prepare("SELECT k,v FROM kv ORDER BY k")?;let mut rs=q.query([])?;while let Some(r)=rs.next()?{f(r.get_ref(0)?.as_blob()?,r.get_ref(1)?.as_blob()?);}}
 }Ok(())}
}
fn payload(slot:u64,version:u64,size:usize,case:&str)->Vec<u8>{
 let len=if case=="resize" {[64,256,2048,8192,256,64][version as usize%6]}else{size};
 let mut b=vec![b'a'+((slot+version)%26) as u8;len];b[..8].copy_from_slice(&slot.to_le_bytes());b[8..16].copy_from_slice(&version.to_le_bytes());b
}
fn verify(db:&Db,n:u64,cycle:u64,size:usize,case:&str)->R<Value>{
 let mixed=case=="mixed";let mut count=0;let mut crc=0;let mut prev=None;
 db.scan(|k,v|{
  assert_eq!(k.len(),8);let id=u64::from_be_bytes(k.try_into().unwrap());if let Some(p)=prev{assert!(id>p);}prev=Some(id);
  let (slot,version)=if id<n {assert!(!(mixed&&cycle>0&&id%10==1));(id,if id%5==0{cycle}else{0})}
   else {assert!(mixed&&cycle>0);assert_eq!((id-n)/(n/10)+1,cycle);((id-n)%(n/10)*10+1,cycle)};
  assert_eq!(v,payload(slot,version,size,case));crc=crc32c::crc32c_append(crc,k);crc=crc32c::crc32c_append(crc,v);count+=1;
 })?;assert_eq!(count,n);
 let mut expected=0;for slot in 0..n{if mixed&&cycle>0&&slot%10==1{continue;}
  expected=crc32c::crc32c_append(expected,&slot.to_be_bytes());expected=crc32c::crc32c_append(expected,&payload(slot,if slot%5==0{cycle}else{0},size,case));}
 if mixed&&cycle>0{for j in 0..n/10{let id=n+(cycle-1)*(n/10)+j;expected=crc32c::crc32c_append(expected,&id.to_be_bytes());expected=crc32c::crc32c_append(expected,&payload(j*10+1,cycle,size,case));}}
 assert_eq!(crc,expected);Ok(json!({"rows":count,"crc32c":crc}))
}
fn disk(p:&Path)->std::io::Result<(u64,u64)>{use std::os::unix::fs::MetadataExt;let mut out=(0,0);
 for e in fs::read_dir(p)?{let e=e?;let m=e.metadata()?;if m.is_dir(){let x=disk(&e.path())?;out.0+=x.0;out.1+=x.1;}else{out.0+=m.len();out.1+=m.blocks()*512;}}Ok(out)}
struct Monitor{end:Arc<AtomicBool>,peak:Arc<Mutex<(u64,u64,u64,u64)>>,thread:Option<std::thread::JoinHandle<()>>}
impl Monitor{fn new(p:PathBuf)->Self{let end=Arc::new(AtomicBool::new(false));let peak=Arc::new(Mutex::new((0,0,0,0)));
 let (e,x)=(end.clone(),peak.clone());let t=std::thread::spawn(move||while !e.load(Ordering::Relaxed){let r=disk(&p);let mut s=x.lock().unwrap();match r{Ok(v)=>{s.0=s.0.max(v.0);s.1=s.1.max(v.1);s.2+=1;},Err(_)=>s.3+=1};drop(s);std::thread::sleep(Duration::from_millis(1));});Self{end,peak,thread:Some(t)}}
 fn take(&self)->Value{let mut s=self.peak.lock().unwrap();let v=json!({"logical":s.0,"allocated":s.1,"samples":s.2,"errors":s.3});*s=(0,0,0,0);v}}
impl Drop for Monitor{fn drop(&mut self){self.end.store(true,Ordering::Relaxed);self.thread.take().unwrap().join().unwrap();}}
fn main()->R<()>{let a:Vec<String>=std::env::args().collect();if a.len()!=8{return Err("OUTPUT ENGINE N SIZE CYCLES CASE BATCH".into());}
 let root=PathBuf::from(&a[1]);fs::create_dir_all(&root)?;let path=root.join("db");let engine=&a[2];let n:u64=a[3].parse()?;let size:usize=a[4].parse()?;let cycles:u64=a[5].parse()?;let case=&a[6];let batch:u64=a[7].parse()?;
 assert!(n>=10&&n%10==0&&size>=16&&batch>0);assert!(matches!(case.as_str(),"load"|"updates"|"mixed"|"resize"));if case=="load"{assert_eq!(cycles,0);}
 let mut db=Db::new(&path,engine)?;let monitor=Monitor::new(path.clone());let mut phases=Vec::new();
 for cycle in 0..=cycles{monitor.take();let start=Instant::now();let mut counts=[0u64;3];let mut times=[0f64;3];let mut commit_seconds=0.;let mut ops=0;
  db.begin()?;
  for slot in 0..n{
   if cycle==0 || slot%5==0{let v=payload(slot,cycle,size,case);let t=Instant::now();db.put(slot,&v)?;let ix=if cycle==0{0}else{1};times[ix]+=t.elapsed().as_secs_f64();counts[ix]+=1;ops+=1;
   }else if case=="mixed"&&slot%10==1{
    let old=if cycle==1{slot}else{n+(cycle-2)*(n/10)+slot/10};let t=Instant::now();db.del(old)?;times[2]+=t.elapsed().as_secs_f64();counts[2]+=1;ops+=1;
    if ops%batch==0{let t=Instant::now();db.commit()?;commit_seconds+=t.elapsed().as_secs_f64();db.begin()?;}
    let fresh=n+(cycle-1)*(n/10)+slot/10;let v=payload(slot,cycle,size,case);let t=Instant::now();db.put(fresh,&v)?;times[0]+=t.elapsed().as_secs_f64();counts[0]+=1;ops+=1;
   }else{continue;}
   if ops%batch==0{let t=Instant::now();db.commit()?;commit_seconds+=t.elapsed().as_secs_f64();db.begin()?;}
  }
  let t=Instant::now();db.commit()?;commit_seconds+=t.elapsed().as_secs_f64();let t=Instant::now();db.checkpoint()?;let checkpoint_seconds=t.elapsed().as_secs_f64();let seconds=start.elapsed().as_secs_f64();
  let final_size=disk(&path)?;let peak=monitor.take();let oracle=verify(&db,n,cycle,size,case)?;
  let phase=json!({"cycle":cycle,"seconds":seconds,"create_update_delete_counts":counts,"create_update_delete_seconds":times,"commit_seconds":commit_seconds,"ending_checkpoint_seconds":checkpoint_seconds,"final_logical":final_size.0,"final_allocated":final_size.1,"peak":peak,"verification":oracle});println!("{phase}");phases.push(phase);
 }
 drop(db);let t=Instant::now();let db=Db::reopen(&path,engine)?;let reopen_seconds=t.elapsed().as_secs_f64();let reopened=verify(&db,n,cycles,size,case)?;
 let report=json!({"engine":engine,"rows":n,"payload_bytes":size,"case":case,"cycles":cycles,"batch":batch,"cache_bytes":8<<20,"sqlite_version":rusqlite::version(),"sync":"FULL native; SQLite fullfsync and checkpoint_fullfsync enabled","layer":"raw KV; no external-key mapping, schema, collection or multimodel index","phases":phases,"reopen_seconds":reopen_seconds,"reopen_verification":reopened,"sampling":"1ms logical/allocated lower bounds, not an enforced cap"});fs::write(root.join("report.json"),serde_json::to_vec_pretty(&report)?)?;Ok(())}
