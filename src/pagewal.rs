//! Isolated page-image WAL experiment. Not the collection engine or its format.
//! One process writer; snapshots created from that writer. Cross-process reader
//! opening and independent salvage are deliberately not claimed by this pilot.
use kernel::{btree::BTree, budget::MemoryBudget, io::{self, FileIo, IoMode, Barrier},
    page::{PageMut, PageRef, PageKind}, pool::BufferPool, Error, Result};
use std::{cell::Cell, collections::BTreeMap, fs::File, path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}}};

const PAGE: usize = 4096;
const FRAME: usize = PAGE + 48;
const WAL_CAP: u64 = 16 * 1024 * 1024;
const MAGIC: &[u8; 8] = b"E4PWAL02";
mod format;
use format::{disk_header, metadata_write_order, Header, WRITE_FEATURES};
// WAL is capped at 16 MiB, so a 32-bit offset leaves room for the expected
// frame checksum in the same eight bytes previously used by a u64 offset.
// A valid checksum alone cannot distinguish an older image of the same page.
#[derive(Clone, Copy)]
struct FrameRef { offset:u32, checksum:u32 }
impl FrameRef {
    fn new(offset:u64,bytes:&[u8])->Self {
        assert!(offset<WAL_CAP);
        Self{offset:offset as u32,checksum:u32at(bytes,28)}
    }
}
type Index = BTreeMap<u32, FrameRef>;
mod repair;
pub use repair::recover_to;
fn bad(why: &'static str) -> Error { Error::CorruptWal { offset: 0, why } }
fn u32at(b: &[u8], p: usize) -> u32 { u32::from_le_bytes(b[p..p+4].try_into().unwrap()) }
fn u64at(b: &[u8], p: usize) -> u64 { u64::from_le_bytes(b[p..p+8].try_into().unwrap()) }
fn frame(kind: u32, no: u32, tx: u64, pages: u32, payload: &[u8; PAGE], identity: &[u8;16]) -> Vec<u8> {
    let mut b=vec![0;FRAME];b[..8].copy_from_slice(MAGIC);
    b[8..12].copy_from_slice(&kind.to_le_bytes());b[12..16].copy_from_slice(&no.to_le_bytes());
    b[16..24].copy_from_slice(&tx.to_le_bytes());b[24..28].copy_from_slice(&pages.to_le_bytes());
    b[32..32+PAGE].copy_from_slice(payload);b[32+PAGE..].copy_from_slice(identity);let crc=crc32c::crc32c(&b);
    b[28..32].copy_from_slice(&crc.to_le_bytes());b
}
#[cfg(test)]
#[path = "pagewal_fault_tests.rs"]
mod fault_tests;
fn read_frame(file: &dyn FileIo, off: u64) -> Result<Vec<u8>> {
    let mut b=vec![0;FRAME];file.read_at(&mut b,off)?;
    let want=u32at(&b,28);b[28..32].fill(0);
    if &b[..8]!=MAGIC || crc32c::crc32c(&b)!=want {return Err(bad("page WAL frame checksum/magic"));}
    b[28..32].copy_from_slice(&want.to_le_bytes());Ok(b)
}
fn read_indexed_frame(file:&dyn FileIo,reference:FrameRef)->Result<Vec<u8>> {
    let bytes=read_frame(file,reference.offset as u64)?;
    if u32at(&bytes,28)!=reference.checksum {
        return Err(bad("WAL frame differs from published version"));
    }
    Ok(bytes)
}
struct State {
    latest: Index, committed: Arc<Index>, end: u64, last_commit: u64,
    pages: u32, committed_pages: u32, tx: u64, tx_crc: u32, cap: u64, free_head: u32,
    identity: [u8;16], features: u64,
}
struct Pager {
    data: Arc<dyn FileIo>, wal: Arc<dyn FileIo>, state: Mutex<State>,
    readers: Arc<AtomicUsize>,
}
impl Pager {
    fn initialize(dir: &Path) -> Result<()> {
        let mut identity = [0;16];
        getrandom::fill(&mut identity).map_err(|e| std::io::Error::other(e.to_string()))?;
        let h = Header { root:0, free:0, cap:u64::MAX, identity, tx:0, features:WRITE_FEATURES };
        let (data,_) = io::open_file(&dir.join("data"), IoMode::Buffered)?;
        let (wal,_) = io::open_file(&dir.join("wal"), IoMode::Buffered)?;
        if data.len()? != 0 || wal.len()? != 0 { return Err(bad("initialize found existing bytes")); }
        data.write_at(&h.page(0)?, 0)?;
        data.write_at(&h.page(1)?, PAGE as u64)?;
        data.sync_full()?;
        if disk_header(&*data)? != Some(h) { return Err(bad("initial metadata verification")); }
        wal.sync_full()?;
        data.sync_dir()
    }
    fn open(dir: &Path) -> Result<Arc<Self>> {
        let (data,_)=io::open_file(&dir.join("data"),IoMode::Buffered)?;
        let (wal,_)=io::open_file(&dir.join("wal"),IoMode::Buffered)?;
        let data:Arc<dyn FileIo>=Arc::from(data);let wal:Arc<dyn FileIo>=Arc::from(wal);
        Self::from_files(data, wal)
    }
    fn from_files(data:Arc<dyn FileIo>, wal:Arc<dyn FileIo>) -> Result<Arc<Self>> {
        let state=Self::inspect(&*data,&*wal)?;
        Ok(Arc::new(Self{data,wal,state:Mutex::new(state),readers:Arc::new(AtomicUsize::new(0))}))
    }
    // Only the fully validated writer opener may normalize an uncommitted tail.
    fn finish_open(&self) -> Result<()> {
        let end = self.state.lock().unwrap().last_commit;
        if self.wal.len()? != end { self.wal.set_len(end)?; self.wal.sync_full()?; }
        Ok(())
    }
    fn inspect(data:&dyn FileIo,wal:&dyn FileIo)->Result<State>{
        let disk = disk_header(data)?;
        let len=wal.len()?;if len>WAL_CAP {return Err(bad("WAL exceeds pilot bounded lookup allowance"));}
        if disk.is_none() && len != 0 { return Err(bad("WAL ownership/history lacks checkpoint metadata")); }
        let data_len=data.len()?;
        let data_pages=u32::try_from(data_len/PAGE as u64).map_err(|_|Error::TooLarge)?;
        let mut committed=Index::new();let mut pending=Index::new();let mut pages=data_pages;
        let floor = disk.map_or(0, |h| h.tx);
        let identity = disk.map_or([0;16], |h| h.identity);
        let features = disk.map_or(WRITE_FEATURES, |h| h.features);
        let mut at=0;let mut last=0;let mut tx=floor.checked_add(1).ok_or(Error::TooLarge)?;let mut crc=0;
        let mut first_tx = None;
        let mut committed_header = None;
        let mut pending_header = None;
        let mut floor_proven = false;
        while at+FRAME as u64<=len {
            let b=read_frame(wal,at)?;
            if b[32+PAGE..] != identity { return Err(bad("WAL belongs to another database")); }
            if at == 0 {
                tx = u64at(&b,16);
                if tx == 0 || tx > floor.checked_add(1).ok_or(Error::TooLarge)? {
                    return Err(bad("WAL starts beyond checkpoint history"));
                }
                first_tx = Some(tx);
            }
            if u64at(&b,16)!=tx {return Err(bad("WAL transaction sequence"));}
            match u32at(&b,8) {
                1 => {let p=u32at(&b,12);
                    if p == 1 { return Err(bad("WAL image targets checkpoint-only metadata copy")); }
                    PageRef::open(&b[32..32+PAGE],p)?;
                    if p == 0 {
                        let h = Header::decode(&b[32..32+PAGE],0)?;
                        if h.identity != identity || h.features != features || h.tx != tx {
                            return Err(bad("WAL metadata identity/history mismatch"));
                        }
                        pending_header = Some(h);
                    }
                    pending.insert(p,FrameRef::new(at,&b));crc=crc32c::crc32c_append(crc,&b);},
                2 => {if u32at(&b,32)!=crc {return Err(bad("WAL transaction checksum"));}
                    pages=u32at(&b,24);if pending.keys().any(|p|*p>=pages){return Err(bad("page beyond committed extent"));}
                    let h = pending_header.take().ok_or_else(||bad("commit lacks transaction metadata"))?;
                    h.validate_extent(pages)?;
                    if tx == floor {
                        if Some(h) != disk { return Err(bad("WAL conflicts with checkpoint history")); }
                        floor_proven = true;
                    }
                    committed_header = Some(h);
                    committed.append(&mut pending);last=at+FRAME as u64;tx=tx.checked_add(1).ok_or(Error::TooLarge)?;crc=0;},
                _ => return Err(bad("unknown WAL frame kind")),
            }
            at+=FRAME as u64;
        }
        if first_tx.is_some_and(|first| first <= floor) && !floor_proven {
            return Err(bad("WAL predates the durable checkpoint"));
        }
        // A torn checkpoint may have extended data by only part of a page.
        // Accept missing/partial data pages only when committed WAL supplies
        // every page through its authenticated extent. No zero-filled holes.
        if data_len % PAGE as u64 != 0 || pages>data_pages {
            if last==0 || pages<=data_pages || pages-data_pages>committed.len() as u32
                || (data_pages..pages).any(|p|!committed.contains_key(&p)) {
                return Err(bad("incomplete data extent without committed WAL coverage"));
            }
        }
        // A complete bad frame refuses above. Only incomplete/uncommitted tail
        // is discarded; no acknowledged page depends on such a tail.
        let h = committed_header.or(disk);
        let next = h.map_or(0, |h| h.tx).checked_add(1).ok_or(Error::TooLarge)?;
        // Creation's root=0 and completely damaged metadata are rejected by the
        // normal opener later. Read-only salvage can still classify raw pages.
        let cap = h.map_or(u64::MAX, |h|h.cap);
        if last + pages as u64 * PAGE as u64 > cap {
            return Err(Error::ResourceLimit("persisted page-WAL cap exceeded"));
        }
        Ok(State{latest:committed.clone(),committed:Arc::new(committed),
            end:last,last_commit:last,pages,committed_pages:pages,tx:next,tx_crc:0,cap,
            free_head:h.map_or(0, |h|h.free),identity,features})
    }
    fn append(&self,s:&mut State,b:&[u8]) -> Result<()> {
        let end=s.end.checked_add(b.len() as u64).ok_or(Error::TooLarge)?;
        if end>WAL_CAP || end+s.pages as u64*PAGE as u64>s.cap {return Err(Error::ResourceLimit("page-WAL managed-byte allowance"));}
        self.wal.write_at(b,s.end)?;s.end=end;Ok(())
    }
    fn publish(&self) -> Result<()> {
        let mut s=self.state.lock().unwrap();let mut body=[0;PAGE];body[..4].copy_from_slice(&s.tx_crc.to_le_bytes());
        let next = s.tx.checked_add(1).ok_or(Error::TooLarge)?;
        let b=frame(2,u32::MAX,s.tx,s.pages,&body,&s.identity);self.append(&mut s,&b)?;
        self.wal.sync_full()?;
        s.committed=Arc::new(s.latest.clone());s.committed_pages=s.pages;s.last_commit=s.end;
        s.tx=next;s.tx_crc=0;Ok(())
    }
    fn checkpoint(&self) -> Result<bool> {
        self.checkpoint_with_crash(0)
    }
    fn checkpoint_with_crash(&self, fault:u8) -> Result<bool> {
        if self.readers.load(Ordering::SeqCst)!=0 {return Ok(false);}
        let mut s=self.state.lock().unwrap();
        if s.end!=s.last_commit {return Err(bad("checkpoint with unpublished pages"));}
        if s.end==0{return Ok(true);}
        for (&p,&off) in &s.latest {
            if p == 0 { continue; }
            let b=read_indexed_frame(&*self.wal,off)?;
            if u32at(&b,12)!=p {return Err(bad("WAL lookup identity"));}
            PageRef::open(&b[32..32+PAGE],p)?;self.data.write_at(&b[32..32+PAGE],p as u64*PAGE as u64)?;
            if fault==1 {std::process::exit(86);}
        }
        self.data.set_len(s.pages as u64*PAGE as u64)?;self.data.sync_full()?;
        if fault==2 {std::process::exit(86);}
        // Independent read-back precedes dropping the WAL. Cost is measured.
        for (&p,&off) in &s.latest {
            if p == 0 { continue; }
            let mut data=[0;PAGE];self.data.read_at(&mut data,p as u64*PAGE as u64)?;
            let b=read_indexed_frame(&*self.wal,off)?;
            if data!=b[32..32+PAGE] {return Err(bad("checkpoint read-back mismatch"));}
            PageRef::open(&data,p)?;
        }
        // Advance the checkpoint floor only after its data is durable and
        // verified. Either intact metadata copy can anchor interrupted recovery.
        let at = *s.latest.get(&0).ok_or_else(||bad("checkpoint lacks metadata"))?;
        let b = read_indexed_frame(&*self.wal,at)?;
        let header = Header::decode(&b[32..32+PAGE],0)?;
        let order = metadata_write_order(&*self.data)?;
        for (step,no) in order.into_iter().enumerate() {
            let page = header.page(no)?;
            self.data.write_at(&page,no as u64*PAGE as u64)?;
            if (fault==5&&step==0)||(fault==6&&step==1) {std::process::exit(86);}
            self.data.sync_full()?;
            let mut actual=[0;PAGE];self.data.read_at(&mut actual,no as u64*PAGE as u64)?;
            if actual != page { return Err(bad("checkpoint metadata read-back mismatch")); }
            Header::decode(&actual,no)?;
        }
        if fault==3 {std::process::exit(86);}
        self.wal.set_len(0)?;
        if fault==4 {std::process::exit(86);}
        self.wal.sync_full()?;
        s.latest.clear();s.committed=Arc::new(Index::new());s.end=0;s.last_commit=0;s.tx_crc=0;
        Ok(true)
    }
    fn auto_checkpoint(&self) -> Result<()> {
        let due={let s=self.state.lock().unwrap();let allowance=s.cap.saturating_sub(s.pages as u64*PAGE as u64);
            s.end >= (4*1024*1024).min(allowance/2).max(FRAME as u64)};
        if due {self.checkpoint()?;}Ok(())
    }
}
fn read_page(data:&dyn FileIo,wal:&dyn FileIo,index:&Index,p:u32,dst:&mut[u8]) -> Result<()> {
    if let Some(off)=index.get(&p) {
        let b=read_indexed_frame(wal,*off)?;if u32at(&b,12)!=p{return Err(bad("WAL page identity"));}
        dst.copy_from_slice(&b[32..32+PAGE]);
    } else if p == 0 {
        let h=disk_header(data)?.ok_or_else(||bad("both checkpoint metadata copies damaged"))?;
        dst.copy_from_slice(&h.page(0)?);
    } else {data.read_at(dst,p as u64*PAGE as u64)?;}
    PageRef::open(dst,p)?;Ok(())
}
impl FileIo for Pager {
    fn manages_free_pages(&self)->bool{true}
    fn pop_free_page(&self)->Result<Option<u32>>{
        let mut s=self.state.lock().unwrap();let p=s.free_head;if p==0{return Ok(None);}
        if p<2 || p>=s.pages{return Err(bad("free head extent"));}
        let mut b=[0;PAGE];read_page(&*self.data,&*self.wal,&s.latest,p,&mut b)?;
        let page=PageRef::open(&b,p)?;
        if page.kind()!=PageKind::Free || page.nentries()!=1 || page.slot(0).len()!=4{return Err(bad("free page state"));}
        let next=u32at(page.slot(0),0);
        if next==p || (next!=0&&(next<2||next>=s.pages)){return Err(bad("free next extent/cycle"));}
        // Mark taken through WAL before returning it. A corrupt cycle cannot
        // reallocate this page even while its replacement is still cached.
        let mut page=PageMut::init(&mut b,PageKind::Free,0,p);page.insert_slot(0,b"taken")?;page.finalise(0);kernel::page::seal(&mut b,1);
        let f=frame(1,p,s.tx,0,&b,&s.identity);let at=s.end;self.append(&mut s,&f)?;
        s.tx_crc=crc32c::crc32c_append(s.tx_crc,&f);s.latest.insert(p,FrameRef::new(at,&f));s.free_head=next;Ok(Some(p))
    }
    fn push_free_page(&self,p:u32)->Result<()>{
        let mut s=self.state.lock().unwrap();if p<2{return Err(bad("free page extent"));}
        // A pool-owned fresh page can be retired before ever being flushed.
        s.pages=s.pages.max(p.checked_add(1).ok_or(Error::TooLarge)?);
        let mut b=[0;PAGE];let mut page=PageMut::init(&mut b,PageKind::Free,0,p);
        page.insert_slot(0,&s.free_head.to_le_bytes())?;page.finalise(0);kernel::page::seal(&mut b,1);
        let f=frame(1,p,s.tx,0,&b,&s.identity);let at=s.end;self.append(&mut s,&f)?;
        s.tx_crc=crc32c::crc32c_append(s.tx_crc,&f);s.latest.insert(p,FrameRef::new(at,&f));s.free_head=p;Ok(())
    }
    fn requires_alignment(&self)->bool{false}
    fn len(&self)->Result<u64>{Ok(self.state.lock().unwrap().pages as u64*PAGE as u64)}
    fn set_len(&self,_:u64)->Result<()>{Err(bad("virtual pager cannot truncate through pool"))}
    fn read_at(&self,b:&mut[u8],off:u64)->Result<()> {
        if off%PAGE as u64!=0 || b.len()%PAGE!=0{return Err(bad("unaligned page read"));}
        let s=self.state.lock().unwrap();
        for (i,p) in b.chunks_mut(PAGE).enumerate(){read_page(&*self.data,&*self.wal,&s.latest,(off/PAGE as u64) as u32+i as u32,p)?;}Ok(())
    }
    fn write_at(&self,b:&[u8],off:u64)->Result<()> {
        if off%PAGE as u64!=0 || b.len()!=PAGE{return Err(bad("unaligned page write"));}
        let p=u32::try_from(off/PAGE as u64).map_err(|_|Error::TooLarge)?;PageRef::open(b,p)?;
        let mut s=self.state.lock().unwrap();s.pages=s.pages.max(p.checked_add(1).ok_or(Error::TooLarge)?);
        let f=frame(1,p,s.tx,0,b.try_into().unwrap(),&s.identity);let at=s.end;
        self.append(&mut s,&f)?;s.tx_crc=crc32c::crc32c_append(s.tx_crc,&f);s.latest.insert(p,FrameRef::new(at,&f));Ok(())
    }
    fn sync_data(&self)->Result<()>{Err(bad("use explicit page-WAL publication"))}
    fn sync_full(&self)->Result<()>{Err(bad("use explicit page-WAL publication"))}
    fn sync_full_primitive(&self)->&'static str{self.wal.sync_full_primitive()}
    fn sync_dir(&self)->Result<()>{self.data.sync_dir()}
}
struct View {data:Arc<dyn FileIo>,wal:Arc<dyn FileIo>,index:Arc<Index>,pages:u32,readers:Arc<AtomicUsize>}
impl Drop for View {fn drop(&mut self){self.readers.fetch_sub(1,Ordering::SeqCst);}}
impl FileIo for View {
    fn requires_alignment(&self)->bool{false}
    fn len(&self)->Result<u64>{Ok(self.pages as u64*PAGE as u64)}
    fn set_len(&self,_:u64)->Result<()>{Err(Error::ReadOnly)}
    fn write_at(&self,_:&[u8],_:u64)->Result<()>{Err(Error::ReadOnly)}
    fn read_at(&self,b:&mut[u8],off:u64)->Result<()> {
        if off%PAGE as u64!=0 || b.len()%PAGE!=0{return Err(bad("unaligned snapshot read"));}
        for (i,dst) in b.chunks_mut(PAGE).enumerate(){let p=(off/PAGE as u64) as u32+i as u32;
            if p>=self.pages{return Err(bad("snapshot page extent"));}read_page(&*self.data,&*self.wal,&self.index,p,dst)?;}Ok(())
    }
    fn sync_data(&self)->Result<()>{Err(Error::ReadOnly)}
    fn sync_full(&self)->Result<()>{Err(Error::ReadOnly)}
    fn sync_full_primitive(&self)->&'static str{"read only"}
    fn sync_dir(&self)->Result<()>{Err(Error::ReadOnly)}
}
pub struct PageWalStore {
    pager:Option<Arc<Pager>>,pool:BufferPool,root:u32,last:Cell<Option<u32>>,hits:Cell<u64>,attempts:Cell<u64>,
    poisoned:bool,dirty:bool,_lock:Option<Arc<File>>,dir:PathBuf,cache:usize,
}
impl PageWalStore {
    pub fn open(dir:&Path,create:bool,cache:usize)->Result<Self>{
        Self::open_with(dir,create,cache,Pager::open)
    }
    fn open_with(dir:&Path,create:bool,cache:usize,opener:impl FnOnce(&Path)->Result<Arc<Pager>>)->Result<Self>{
        if create {std::fs::create_dir(dir)?;}
        else { for name in ["data","wal","writer.lock"] { std::fs::metadata(dir.join(name))?; } }
        let lock=std::fs::OpenOptions::new().read(true).write(true).create(create).truncate(false).open(dir.join("writer.lock"))?;
        if !io::try_lock_exclusive(&lock)?{return Err(Error::WriterLocked);}
        if create { Pager::initialize(dir)?; }
        let pager=opener(dir)?;let file:Arc<dyn FileIo>=pager.clone();
        if pager.state.lock().unwrap().features != WRITE_FEATURES {
            return Err(bad("writer build cannot preserve the database feature set"));
        }
        let pool=BufferPool::new(file,Arc::new(MemoryBudget::new(cache)),cache/PAGE)?;
        pool.set_stamp_gen(1);pool.set_reuse_limit(u64::MAX);
        let mut s=Self{pager:Some(pager),pool,root:0,last:Cell::new(None),hits:Cell::new(0),attempts:Cell::new(0),
            poisoned:false,dirty:false,_lock:Some(Arc::new(lock)),dir:dir.into(),cache};
        if create {
            if s.pool.page_count()!=2{return Err(bad("create found unexpected pages"));}
            s.root=BTree::create(&s.pool,1,&s.last,&s.hits,&s.attempts)?.root();s.dirty=true;s.commit()?;
        } else {s.load_header()?;}
        s.pager.as_ref().unwrap().finish_open()?;
        Ok(s)
    }
    fn tree(&self)->BTree<'_>{BTree::open(&self.pool,1,self.root,&self.last,&self.hits,&self.attempts)}
    fn ready(&self)->Result<()>{if self.poisoned {Err(Error::StorePoisoned)}else{Ok(())}}
    fn writable(&self)->Result<()>{self.ready()?;if self.pager.is_none(){Err(Error::ReadOnly)}else{Ok(())}}
    fn load_header(&mut self)->Result<()> {
        let r=self.pool.get(0)?;let p=PageRef::open_resident(&r,0)?;
        if p.kind()!=PageKind::Meta || p.nentries()!=1 { return Err(bad("pilot header shape")); }
        let h=Header::decode_slot(p.slot(0))?;h.validate_extent(self.pool.page_count())?;
        self.root=h.root;
        if let Some(pager)=&self.pager{let mut s=pager.state.lock().unwrap();s.free_head=h.free;
            s.cap=h.cap;
            if s.end+s.pages as u64*PAGE as u64>s.cap{return Err(Error::ResourceLimit("persisted page-WAL cap exceeded"));}
        }Ok(())
    }
    pub fn get(&self,k:&[u8])->Result<Option<Vec<u8>>>{self.ready()?;self.tree().get(k)}
    pub fn put(&mut self,k:&[u8],v:&[u8])->Result<()> {
        self.writable()?;self.dirty=true;
        let r={let mut t=self.tree();t.insert(k,v).map(|_|t.root())};
        match r {Ok(root)=>{self.root=root;Ok(())},Err(e)=>{self.poisoned=true;Err(e)}}
    }
    pub fn delete(&mut self,k:&[u8])->Result<bool>{
        self.writable()?;self.dirty=true;
        let r={let mut t=self.tree();t.delete(k).map(|yes|(yes,t.root()))};
        match r {Ok((yes,root))=>{self.root=root;Ok(yes)},Err(e)=>{self.poisoned=true;Err(e)}}
    }
    pub fn scan(&self,mut f:impl FnMut(&[u8],&[u8])->bool)->Result<()> {
        self.ready()?;self.tree().range(&[])?.for_each_ref(|k,v|f(k,v))
    }
    pub fn commit(&mut self)->Result<()> {
        self.writable()?;
        let result=(||{
            let header={let s=self.pager.as_ref().unwrap().state.lock().unwrap();
                Header{root:self.root,free:s.free_head,cap:s.cap,identity:s.identity,tx:s.tx,features:s.features}};
            {let mut w=self.pool.get_mut(0)?;let mut p=PageMut::init(w.bytes_mut(),PageKind::Meta,0,0);
             p.insert_slot(0,&header.bytes())?;p.finalise(0);}
            self.pool.flush_all(Barrier::None)?;let pager=self.pager.as_ref().unwrap();pager.publish()?;
            self.pool.finish_stable_page_epoch();self.dirty=false;pager.auto_checkpoint()?;Ok(())
        })();if result.is_err(){self.poisoned=true;}result
    }
    pub fn checkpoint(&mut self)->Result<bool>{self.writable()?;
        if self.dirty{return Err(bad("checkpoint requires commit"));}
        let r=self.pager.as_ref().unwrap().checkpoint();if r.is_err(){self.poisoned=true;}r
    }
    pub fn set_cap(&mut self,bytes:u64)->Result<()> {
        self.writable()?;if self.dirty{return Err(bad("set cap requires committed state"));}
        {let mut s=self.pager.as_ref().unwrap().state.lock().unwrap();
        if s.end+s.pages as u64*PAGE as u64+2*FRAME as u64>bytes{return Err(Error::ResourceLimit("cap lacks policy commit headroom"));}s.cap=bytes;}
        self.dirty=true;self.commit()
    }
    pub fn snapshot(&self)->Result<Self>{
        self.ready()?;
        let p=self.pager.as_ref().ok_or(Error::ReadOnly)?;let s=p.state.lock().unwrap();
        p.readers.fetch_update(Ordering::SeqCst,Ordering::SeqCst,|n|if n<8{Some(n+1)}else{None})
            .map_err(|_|Error::ResourceLimit("page-WAL permits at most eight snapshots"))?;
        let view:Arc<dyn FileIo>=Arc::new(View{data:p.data.clone(),wal:p.wal.clone(),index:s.committed.clone(),pages:s.committed_pages,readers:p.readers.clone()});
        let pool=BufferPool::new(view,Arc::new(MemoryBudget::new(self.cache)),self.cache/PAGE)?;
        let mut out=Self{pager:None,pool,root:0,last:Cell::new(None),hits:Cell::new(0),attempts:Cell::new(0),poisoned:false,dirty:false,_lock:self._lock.clone(),dir:self.dir.clone(),cache:self.cache};
        drop(s);out.load_header()?;Ok(out)
    }
    pub fn wal_bytes(&self)->u64 {self.pager.as_ref().map_or(0,|p|p.state.lock().unwrap().end)}
    /// Diagnostic snapshot/reset of existing FileIo counters, data then WAL.
    /// Each tuple is (write calls, issued write bytes, read calls). Buffered
    /// calls are not physical-device I/O. Read-only snapshots return None.
    pub fn take_file_io_stats(&self)->Option<[(u64,u64,u64);2]> {
        let p=self.pager.as_ref()?;
        Some([p.data.stats()?.take(),p.wal.stats()?.take()])
    }
    /// Pilot fault harness only: abruptly exits inside a checkpoint stage.
    pub fn test_checkpoint_crash(&mut self,stage:u8)->Result<bool>{
        self.writable()?;assert!((1..=6).contains(&stage));assert!(!self.dirty);
        self.pager.as_ref().unwrap().checkpoint_with_crash(stage)
    }
}
