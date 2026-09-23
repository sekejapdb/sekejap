//! Source-preserving raw-KV salvage. Rootless bytes are candidates, never current.
use super::*;
use kernel::{recover::{CandidateReader,LeafEvent,LeafCandidate},verify::{decode_record,DecodedRecord}};
use serde_json::{json,Value};
use std::{fs,io::Write};

#[derive(Clone)]
pub(super) struct Source {pub(super) data:Arc<dyn FileIo>,pub(super) wal:Arc<dyn FileIo>,pub(super) index:Arc<Index>,pub(super) pages:u32}
impl Source {
    /// The bare data file with no committed-WAL overlay (typed recovery fallback).
    pub(super) fn plain(data:Arc<dyn FileIo>)->Result<Self>{
        let pages=u32::try_from(data.len()?/PAGE as u64).map_err(|_|Error::TooLarge)?;
        Ok(Self{wal:data.clone(),data,index:Arc::new(Index::new()),pages})
    }
}
impl FileIo for Source {
    fn requires_alignment(&self)->bool{false}
    fn len(&self)->Result<u64>{Ok(self.pages as u64*PAGE as u64)}
    fn read_at(&self,b:&mut[u8],off:u64)->Result<()>{
        if b.len()!=PAGE||off%PAGE as u64!=0||off/PAGE as u64>=self.pages as u64{return Err(bad("repair page bounds"));}
        // Let CandidateReader classify a damaged data page after this raw read.
        let p=(off/PAGE as u64) as u32;
        if let Some(at)=self.index.get(&p){let f=read_indexed_frame(&*self.wal,*at)?;if u32at(&f,12)!=p{return Err(bad("repair WAL identity"));}b.copy_from_slice(&f[32..32+PAGE]);Ok(())}
        else{self.data.read_at(b,off)}
    }
    fn write_at(&self,_:&[u8],_:u64)->Result<()>{Err(Error::ReadOnly)}
    fn set_len(&self,_:u64)->Result<()>{Err(Error::ReadOnly)}
    fn sync_data(&self)->Result<()>{Err(Error::ReadOnly)}
    fn sync_full(&self)->Result<()>{Err(Error::ReadOnly)}
    fn sync_dir(&self)->Result<()>{Err(Error::ReadOnly)}
    fn sync_full_primitive(&self)->&'static str{"read only"}
}
fn fingerprint(f:&dyn FileIo)->Result<(u64,u32)>{
    let len=f.len()?;let mut at=0;let mut crc=0;let mut b=[0;65536];
    while at<len{let n=(len-at).min(b.len() as u64) as usize;f.read_at(&mut b[..n],at)?;crc=crc32c::crc32c_append(crc,&b[..n]);at+=n as u64;}Ok((len,crc))
}
fn page(source:&Source,no:u32)->Result<[u8;PAGE]>{let mut b=[0;PAGE];source.read_at(&mut b,no as u64*PAGE as u64)?;PageRef::open(&b,no)?;Ok(b)}
fn root(source:&Source)->Result<u32>{
    let h=if source.index.contains_key(&0){Header::decode(&page(source,0)?,0)?}
        else{disk_header(&*source.data)?.ok_or_else(||bad("repair metadata unavailable"))?};
    h.validate_extent(source.pages)?;Ok(h.root)
}
// Bound descent to 64 pages. Validate all separators/keys before routing;
// a leaf independently scanned elsewhere is current only if this path names it.
fn current_leaf(source:&Source,mut no:u32,key:&[u8])->Result<u32>{
    let mut lower:Option<Vec<u8>>=None;let mut upper:Option<Vec<u8>>=None;
    for _ in 0..64{
        if no<2||no>=source.pages{return Err(bad("repair child bounds"));}
        let b=page(source,no)?;let p=PageRef::open(&b,no)?;
        if p.tree_id()!=1||!matches!(p.kind(),PageKind::Leaf|PageKind::Interior){return Err(bad("repair tree identity"));}
        let mut prev:Option<&[u8]>=None;let mut next=p.child0();let mut lo=lower.clone();let mut hi=upper.clone();let mut found_upper=false;
        for i in 0..p.nentries(){
            let r=decode_record(p.slot(i),no,p.kind())?;
            let (k,child)=match r{DecodedRecord::Leaf{key,..}=>(key,None),DecodedRecord::Interior{key,child}=>(key,Some(child))};
            if prev.is_some_and(|v|k<=v)||lower.as_ref().is_some_and(|v|k<v.as_slice())||upper.as_ref().is_some_and(|v|k>=v.as_slice()) {return Err(bad("repair key ordering/bounds"));}
            prev=Some(k);
            if let Some(child)=child{if k<=key{next=child;lo=Some(k.to_vec());}else if !found_upper{hi=Some(k.to_vec());found_upper=true;}}
        }
        if p.kind()==PageKind::Leaf{return Ok(no);}no=next;lower=lo;upper=hi;
    }Err(bad("repair depth/cycle bound"))
}
fn hex(b:&[u8])->String{const H:&[u8]=b"0123456789abcdef";let mut out=String::with_capacity(b.len()*2);for &c in b{out.push(H[(c>>4) as usize] as char);out.push(H[(c&15) as usize] as char);}out}
fn event(file:&mut File,v:Value)->Result<()>{writeln!(file,"{v}")?;Ok(())}
fn value_for(source:&Source,reader:&CandidateReader,root:u32,key:&[u8],limit:usize)->Result<Vec<u8>>{
    let no=current_leaf(source,root,key)?;let b=page(source,no)?;let p=PageRef::open(&b,no)?;
    for slot in 0..p.nentries(){if let DecodedRecord::Leaf{key:k,value,overflow}=decode_record(p.slot(slot),no,PageKind::Leaf)?{if k==key{
        return reader.read_value(LeafCandidate{page_no:no,generation:p.lsn(),slot,key:k,stored_value:value,overflow},limit);
    }}}Err(bad("repair verification source key missing"))
}

/// Repair to a fresh directory. `max_value_bytes` bounds individual materialization;
/// fixed destination cache is 64 KiB, WAL/index <=16 MiB, loss reports stream.
/// Requires a quiescent source; owns its existing writer lock for the whole run.
/// Does not promote candidates or replace source. Complete corrupt WAL currently
/// refuses conservatively; later-region WAL salvage remains a separate open gate.
pub fn recover_to(source:&Path,destination:&Path,max_value_bytes:usize)->Result<Value>{
    if max_value_bytes==0||max_value_bytes>16<<20{return Err(Error::ResourceLimit("repair value allowance must be 1..16MiB"));}
    let source=fs::canonicalize(source)?;
    let parent=fs::canonicalize(destination.parent().ok_or_else(||bad("repair destination parent"))?)?;
    let destination=parent.join(destination.file_name().ok_or_else(||bad("repair destination name"))?);
    if destination.starts_with(&source)||source.starts_with(&destination){return Err(bad("repair source/destination overlap"));}
    if destination.exists(){return Err(bad("repair destination already exists"));}
    let lock=File::open(source.join("writer.lock"))?;
    if !io::try_lock_exclusive(&lock)?{return Err(Error::WriterLocked);}
    let _lock=io::Locked::held(lock);
    let data:Arc<dyn FileIo>=io::open_recovery_source(&source.join("data"))?.into();
    let wal:Arc<dyn FileIo>=io::open_recovery_source(&source.join("wal"))?.into();
    let before=[fingerprint(&*data)?,fingerprint(&*wal)?];
    // No writer opener, truncation, checkpoint, root lookup or allocator walk.
    let state=Pager::inspect(&*data,&*wal)?;
    let ignored_tail=wal.len()?.saturating_sub(state.last_commit);
    let view=Source{data,wal,index:state.committed,pages:state.committed_pages};
    let root=root(&view).ok();let reader=CandidateReader::from_file(Box::new(view.clone()));
    fs::create_dir(&destination)?;
    let mut losses=fs::OpenOptions::new().write(true).create_new(true).open(destination.join("losses.jsonl"))?;
    let mut candidates=fs::OpenOptions::new().write(true).create_new(true).open(destination.join("candidates.bin"))?;
    let mut out=PageWalStore::open(&destination.join("current"),true,64<<10)?;
    let (mut current,mut uncertain,mut known)=(0u64,0u64,0u64);
    let scan=reader.scan::<Error>(1,|e|{
        match e {
            LeafEvent::DamagedPage{page_no}=>event(&mut losses,json!({"class":"unknown_extent","page":page_no}))?,
            LeafEvent::MalformedCell{page_no,slot}=>event(&mut losses,json!({"class":"unknown_extent","page":page_no,"slot":slot}))?,
            LeafEvent::Record(r)=>{
                let is_current=root.and_then(|root|current_leaf(&view,root,r.key).ok())==Some(r.page_no);
                let value=match reader.read_value(r,max_value_bytes){Ok(v)=>v,Err(e)=>{
                    known+=1;event(&mut losses,json!({"class":"known_affected_key","key_hex":hex(r.key),"page":r.page_no,"membership":if is_current{"current"}else{"uncertain"},"reason":format!("{e:?}")}))?;return Ok(());
                }};
                if is_current{
                    if out.get(r.key)?.is_some(){return Err(bad("duplicate current recovery key"));}
                    out.put(r.key,&value)?;current+=1;if current%100==0{out.commit()?;}
                }else{
                    // Preserve all candidates with physical identity; never dedup
                    // conflicting versions into an authoritative replacement.
                    candidates.write_all(&r.page_no.to_le_bytes())?;candidates.write_all(&(r.slot as u32).to_le_bytes())?;
                    candidates.write_all(&(r.key.len() as u32).to_le_bytes())?;candidates.write_all(&(value.len() as u32).to_le_bytes())?;
                    candidates.write_all(r.key)?;candidates.write_all(&value)?;uncertain+=1;
                }
            }
        }Ok(())
    })?;
    out.commit()?;out.checkpoint()?;drop(out);
    let check=PageWalStore::open(&destination.join("current"),false,64<<10)?;
    let mut seen=0;let mut failure=None;
    check.scan(|k,v|{match root.ok_or_else(||bad("current rows lack root")).and_then(|root|value_for(&view,&reader,root,k,max_value_bytes)){
        Ok(want) if want==v=>{seen+=1;true},other=>{failure=Some(format!("independent destination verification: {other:?}"));false}
    }})?;
    if failure.is_some()||seen!=current{return Err(bad("repair independent verification failed"));}
    // Reread candidate spool without retaining it; verify framing and source bytes.
    candidates.sync_all()?;drop(candidates);
    let mut spool=File::open(destination.join("candidates.bin"))?;let mut checked_candidates=0;
    use std::io::Read;
    while spool.metadata()?.len()>0 {
        let mut head=[0;16];let mut first=[0];if spool.read(&mut first)?==0{break;}head[0]=first[0];spool.read_exact(&mut head[1..])?;
        let no=u32at(&head,0);let slot=u32at(&head,4) as usize;let kl=u32at(&head,8) as usize;let vl=u32at(&head,12) as usize;
        if kl>PAGE||vl>max_value_bytes{return Err(bad("candidate spool bounds"));}
        let mut k=vec![0;kl];let mut v=vec![0;vl];spool.read_exact(&mut k)?;spool.read_exact(&mut v)?;
        let b=page(&view,no)?;let p=PageRef::open(&b,no)?;if slot>=p.nentries(){return Err(bad("candidate slot bounds"));}
        match decode_record(p.slot(slot),no,PageKind::Leaf)?{DecodedRecord::Leaf{key,value,overflow}=>{
            let want=reader.read_value(LeafCandidate{page_no:no,generation:p.lsn(),slot,key,stored_value:value,overflow},max_value_bytes)?;
            if key!=k||want!=v{return Err(bad("candidate verification mismatch"));}
        },_=>return Err(bad("candidate kind"))}checked_candidates+=1;
    }
    if checked_candidates!=uncertain{return Err(bad("candidate count mismatch"));}
    losses.sync_all()?;
    if before!=[fingerprint(&*view.data)?,fingerprint(&*view.wal)?]{return Err(bad("source changed during repair"));}
    let report=json!({"version":1,"format":"page-WAL raw KV","source":source,"destination":destination,
        "completion":"verified salvage; source not replaced","current_rows":current,"candidate_rows":uncertain,
        "known_affected_keys":known,"unknown_page_extents":scan.damaged_pages,"malformed_cells":scan.malformed_cells,
        "wal_uncommitted_tail_bytes":ignored_tail,"source_length_crc32c":before,"source_unchanged":true,
        "verified_current_rows":seen,"verified_candidate_rows":checked_candidates,"max_value_bytes":max_value_bytes,
        "limitations":"No rootless current-membership reconstruction; corrupt-WAL salvage refuses; no aggregate repair disk cap or typed schema decoding"});
    let marker=destination.join("COMPLETE.json");let mut f=fs::OpenOptions::new().write(true).create_new(true).open(&marker)?;
    f.write_all(serde_json::to_string_pretty(&report).map_err(|_|bad("repair report serialization"))?.as_bytes())?;f.sync_all()?;
    let (marker_io,_)=io::open_file(&marker,IoMode::Buffered)?;marker_io.sync_dir()?;
    Ok(report)
}
