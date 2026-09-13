//! Explicit comparison: E4 managed logical cap versus native SQLite WAL behavior.
use e4_prototype::pagewal::PageWalStore;
use rusqlite::{Connection,params};
use serde_json::json;
use std::{fs,path::{Path,PathBuf},time::Instant};
type R<T>=Result<T,Box<dyn std::error::Error>>;
enum Db{Page(PageWalStore),Sql(Connection)}
enum Reader{Page(PageWalStore),Sql(Connection)}
fn bytes(p:&Path)->R<u64>{let mut total=0;for e in fs::read_dir(p)?{let e=e?;if e.file_type()?.is_file(){total+=e.metadata()?.len();}}Ok(total)}
fn value(id:u64,version:u64)->Vec<u8>{let mut b=vec![b'x';256];b[..8].copy_from_slice(&id.to_le_bytes());b[8..16].copy_from_slice(&version.to_le_bytes());b}
impl Db{
    fn begin(&self)->R<()>{if let Self::Sql(c)=self{c.execute_batch("BEGIN")?;}Ok(())}
    fn put(&mut self,id:u64,v:u64)->R<()>{let k=id.to_be_bytes();let b=value(id,v);match self{
        Self::Page(s)=>s.put(&k,&b)?,Self::Sql(c)=>{c.prepare_cached("INSERT INTO kv VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v")?.execute(params![k.as_slice(),b])?;}
    }Ok(())}
    fn commit(&mut self)->R<()>{match self{Self::Page(s)=>s.commit()?,Self::Sql(c)=>c.execute_batch("COMMIT")?}Ok(())}
    fn checkpoint(&mut self)->R<()>{match self{Self::Page(s)=>{s.checkpoint()?;},Self::Sql(c)=>{c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;}}Ok(())}
    fn snapshot(&self,p:&Path)->R<Reader>{Ok(match self{Self::Page(s)=>Reader::Page(s.snapshot()?),Self::Sql(_)=>{
        let c=Connection::open(p.join("data.sqlite"))?;c.execute_batch("PRAGMA cache_size=-8192;PRAGMA mmap_size=0;BEGIN;")?;
        let _:i64=c.query_row("SELECT count(*) FROM kv",[],|r|r.get(0))?;Reader::Sql(c)
    }})}
}
fn verify(reader:&Reader,n:u64,completed:u64)->R<()>{
    let mut count=0;let mut visit=|k:&[u8],v:&[u8]|{
        let id=u64::from_be_bytes(k.try_into().unwrap());assert_eq!(id,count);assert!(id<n);
        let version=completed/n+u64::from(id<completed%n);assert_eq!(v,value(id,version));count+=1;
    };
    match reader{Reader::Page(s)=>s.scan(|k,v|{visit(k,v);true})?,Reader::Sql(c)=>{
        let mut q=c.prepare("SELECT k,v FROM kv ORDER BY k")?;let mut rs=q.query([])?;
        while let Some(r)=rs.next()?{visit(r.get_ref(0)?.as_blob()?,r.get_ref(1)?.as_blob()?);}
    }}assert_eq!(count,n);Ok(())
}
fn main()->R<()>{
    let a:Vec<String>=std::env::args().collect();if a.len()!=5{return Err("OUTPUT pagewal|sqlite N none|held|rolling".into());}
    let out=PathBuf::from(&a[1]);fs::create_dir_all(&out)?;let p=out.join("db");let n:u64=a[3].parse()?;assert!(n%1000==0&&n>=1000);
    let mode=&a[4];assert!(matches!(mode.as_str(),"none"|"held"|"rolling"|"short"));
    let mut db=match a[2].as_str(){"pagewal"=>Db::Page(PageWalStore::open(&p,true,8<<20)?),"sqlite"=>{
        fs::create_dir(&p)?;let c=Connection::open(p.join("data.sqlite"))?;
        c.execute_batch("PRAGMA page_size=4096;PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA fullfsync=ON;PRAGMA checkpoint_fullfsync=ON;PRAGMA wal_autocheckpoint=1000;PRAGMA cache_size=-8192;PRAGMA mmap_size=0;CREATE TABLE kv(k BLOB PRIMARY KEY,v BLOB NOT NULL) WITHOUT ROWID;")?;Db::Sql(c)
    },_=>return Err("engine".into())};
    let start=Instant::now();for offset in (0..n).step_by(1000){db.begin()?;for i in offset..offset+1000{db.put(i,0)?;}db.commit()?;}db.checkpoint()?;
    let load_seconds=start.elapsed().as_secs_f64();let loaded=bytes(&p)?;let cap=2*loaded;
    if let Db::Page(s)=&mut db{s.set_cap(cap)?;}
    let mut reader=if mode!="none"{Some(db.snapshot(&p)?)}else{None};let mut reader_version=0;
    let mut completed=0;let mut peak=bytes(&p)?;let mut refusal=None;let start=Instant::now();let mut verification_seconds=0.;
    for tx in 0..12*n/1000 {
        db.begin()?;
        let r=(||->R<()>{for j in 0..1000{let index=tx*1000+j;db.put(index%n,index/n+1)?;peak=peak.max(bytes(&p)?);}db.commit()})();
        peak=peak.max(bytes(&p)?);
        if let Err(e)=r{assert!(a[2]=="pagewal"&&e.to_string().contains("allowance"),"unexpected failure: {e}");refusal=Some(e.to_string());break;}
        completed+=1000;
        if (mode=="rolling"&&completed%n==0)||(mode=="short"&&completed%5000==0){
            let t=Instant::now();verify(reader.as_ref().unwrap(),n,reader_version)?;verification_seconds+=t.elapsed().as_secs_f64();
            drop(reader.take());db.checkpoint()?;reader=Some(db.snapshot(&p)?);reader_version=completed;
        }
        // SQLite has no managed data+WAL hard cap in this comparison. Stop at
        // the first transaction boundary above 2x to limit test disk consumption.
        if a[2]=="sqlite"&&bytes(&p)?>cap{refusal=Some("harness stopped after SQLite crossed 2x; not engine admission".into());break;}
    }
    let mutation_seconds=start.elapsed().as_secs_f64()-verification_seconds;
    if let Some(r)=&reader{verify(r,n,reader_version)?;}drop(reader);drop(db);
    let t=Instant::now();let reopened=if a[2]=="pagewal"{Reader::Page(PageWalStore::open(&p,false,8<<20)?)}else{Reader::Sql(Connection::open(p.join("data.sqlite"))?)};
    verify(&reopened,n,completed)?;let verification= t.elapsed().as_secs_f64();
    let mut db=match reopened{Reader::Page(s)=>Db::Page(s),Reader::Sql(c)=>Db::Sql(c)};
    let t=Instant::now();db.checkpoint()?;let checkpoint_seconds=t.elapsed().as_secs_f64();peak=peak.max(bytes(&p)?);
    if a[2]=="pagewal"{assert!(peak<=cap);}
    let report=json!({"engine":a[2],"rows":n,"payload_bytes":256,"reader":mode,"batch":1000,"requested_updates":12*n,
        "committed_updates":completed,"completed":completed==12*n,"refusal":refusal,"loaded_logical":loaded,"cap_logical":cap,
        "observed_peak_logical":peak,"final_logical":bytes(&p)?,"load_seconds":load_seconds,"mutation_seconds":mutation_seconds,
        "ending_checkpoint_seconds":checkpoint_seconds,"mutation_with_ending_checkpoint_seconds":mutation_seconds+checkpoint_seconds,
        "verification_seconds":verification+verification_seconds,"verified":true,
        "measurement":"Logical file sizes checked after each put and commit; transient internal peaks not sampled; metadata overhead is included in both arms; fs stat cost is inside mutation time. E4 enforces cap at every WAL append. SQLite has no total-WAL cap; harness stops AFTER a boundary exceeds 2x. Not a speed acceptance benchmark."});
    fs::write(out.join("report.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");Ok(())
}
