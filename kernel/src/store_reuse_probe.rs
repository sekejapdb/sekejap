//! Diagnostic only: compare each disk read with the last bytes issued for it.
use super::*;
use std::{collections::BTreeMap,sync::Mutex};
struct AuditIo { file:Arc<dyn FileIo>, writes:Mutex<BTreeMap<u64,u32>> }
impl FileIo for AuditIo {
    fn requires_alignment(&self)->bool{false}
    fn len(&self)->Result<u64>{self.file.len()}
    fn set_len(&self,n:u64)->Result<()>{self.file.set_len(n)}
    fn read_at(&self,b:&mut[u8],off:u64)->Result<()>{
        self.file.read_at(b,off)?;
        for (i,p) in b.chunks_exact(PAGE_SIZE).enumerate(){if let Some(want)=self.writes.lock().unwrap().get(&(off+i as u64*PAGE_SIZE as u64)){assert_eq!(crc32c::crc32c(p),*want,"disk differs from last issued write at {}",off+i as u64*PAGE_SIZE as u64);}}
        Ok(())
    }
    fn write_at(&self,b:&[u8],off:u64)->Result<()>{self.file.write_at(b,off)?;for (i,p) in b.chunks_exact(PAGE_SIZE).enumerate(){self.writes.lock().unwrap().insert(off+i as u64*PAGE_SIZE as u64,crc32c::crc32c(p));}Ok(())}
    fn sync_data(&self)->Result<()>{self.file.sync_data()}
    fn sync_full(&self)->Result<()>{self.file.sync_full()}
    fn sync_dir(&self)->Result<()>{self.file.sync_dir()}
    fn sync_full_primitive(&self)->&'static str{self.file.sync_full_primitive()}
}
#[test]
#[ignore = "large forensic probe; explicit scratch TMPDIR required"]
fn store_churn_audits_last_issued_page_write(){
    let d=tempfile::tempdir().unwrap();let (f,_)=crate::io::open_file(&d.path().join("data"),IoMode::Buffered).unwrap();
    let io=Arc::new(AuditIo{file:f.into(),writes:Mutex::new(BTreeMap::new())});
    let sync=if std::env::var_os("E4_PROBE_FULL").is_some(){SyncMode::Full}else{SyncMode::Off};
    let mut s=Store::create_on(d.path(),Config{budget_bytes:8<<20,io:IoMode::Buffered,sync},io).unwrap();
    let n=400000u64;
    let rounds=std::env::var("E4_PROBE_ROUNDS").unwrap_or_else(|_|"20".into()).parse::<u64>().unwrap();
    for round in 0..=rounds {
        let mut ops=0;
        for slot in 0..n {
            let value=|id:u64,version:u64|{let mut v=vec![b'a'+((id+version)%26)as u8;256];v[..8].copy_from_slice(&id.to_le_bytes());v[8..16].copy_from_slice(&version.to_le_bytes());v};
            if round==0||slot%5==0{s.put(&slot.to_be_bytes(),&value(slot,round)).unwrap();ops+=1;}
            else if slot%10==1{
                let old=if round==1{slot}else{n+(round-2)*(n/10)+slot/10};assert!(s.delete(&old.to_be_bytes()).unwrap());ops+=1;
                if ops%1000==0{s.checkpoint().unwrap();}
                s.put(&(n+(round-1)*(n/10)+slot/10).to_be_bytes(),&value(slot,round)).unwrap();ops+=1;
            }else{continue;}
            if ops%1000==0{s.checkpoint().unwrap();}
        }
        s.checkpoint().unwrap();let mut prev=None;let mut count=0;
        s.scan(&[]).unwrap().for_each_ref(|k,_|{let id=u64::from_be_bytes(k.try_into().unwrap());assert!(prev.is_none_or(|p|id>p),"round {round} prev {prev:?} id {id}");prev=Some(id);count+=1;true}).unwrap();
        assert_eq!(count,n,"round {round}");eprintln!("round {round} verified");
    }
}
