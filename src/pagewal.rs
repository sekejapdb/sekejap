//! Page-image WAL store: the selected V2 storage path for typed collections
//! (docs/V2_COLLECTION_INTEGRATION.md). One writer per database, guarded by
//! `writer.lock`. A transaction is PUBLISHED only after its commit frame's
//! FULL barrier returned AND the writer recorded `(floor, tx, end)` in the
//! two-copy publication hint inside `readers.lock`; readers beside a live
//! writer are bounded to exactly that prefix and never guess, clamp or fall
//! back. With no writer alive a reader derives the committed state from the
//! files by the same strict inspection a writer runs, under a shared
//! ownership lock. Readers hold one of eight advisory slot locks
//! (`reader-N.lock`); a checkpoint runs only when it can take the admission
//! gate and every slot, so no reader's page image is rewritten or its WAL
//! frames reset underneath it. Frame, header and page formats are the
//! accepted `E4PWAL02` layout and are unchanged by this integration.
use kernel::{btree::{BTree, RangeIter}, budget::MemoryBudget, io::{self, FileIo, IoMode, Barrier},
    page::{PageMut, PageRef, PageKind}, pool::BufferPool, recover::CandidateReader, Error, Result};
use std::{cell::Cell, collections::BTreeMap, fs::File, path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::{AtomicU64, AtomicUsize, Ordering}}};

const PAGE: usize = 4096;
const FRAME: usize = PAGE + 48;
const WAL_CAP: u64 = 16 * 1024 * 1024;
const MAGIC: &[u8; 8] = b"E4PWAL02";
/// Reader admission bound, in-process and cross-process alike.
pub const READER_SLOTS: usize = 8;
/// Logical bytes of the publication hint file (`readers.lock`): two copies.
pub const HINT_FILE_BYTES: u64 = 2 * HINT_BYTES as u64;
const HINT_BYTES: usize = 48;
const HINT_MAGIC: &[u8; 8] = b"E4PWHNT1";
mod format;
use format::{create_features, disk_header, metadata_write_order, Header, COMPACT_CELLS};
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
mod current_reader;
pub use current_reader::{CurrentReaderLimits, CurrentSourceReader, SourceFingerprint};
mod repair;
pub use repair::recover_to;
fn bad(why: &'static str) -> Error { Error::CorruptWal { offset: 0, why } }
/// A reader could not be admitted right now; the state is intact and the
/// next writer publication, rollback or reopen resolves it. Reported as a
/// retryable I/O condition rather than corruption.
fn unavailable(why: &'static str) -> Error { Error::Io(std::io::Error::new(std::io::ErrorKind::WouldBlock, why)) }
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
#[cfg(test)]
#[path = "pagewal_hint_tests.rs"]
mod hint_tests;
#[cfg(test)]
#[path = "pagewal/create_feature_tests.rs"]
mod create_feature_tests;
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

// ---- coordination files -------------------------------------------------
// `readers.lock`: admission gate (shared/exclusive advisory lock) and the
// 96-byte publication hint. `reader-{i}.lock`: zero-byte presence slots. A
// writer creates them at open; a quiescent reader creates them only after the
// physical and typed checks passed; a reader beside a live writer never
// creates anything and opens read-only handles (sufficient for advisory
// locks on Linux, macOS and Windows).
const GATE: &str = "readers.lock";
fn slot_name(index:usize)->String{format!("reader-{index}.lock")}
fn coordination_files()->Vec<String>{
    let mut v=vec![GATE.to_owned()];v.extend((0..READER_SLOTS).map(slot_name));v
}
fn create_coordination(dir:&Path)->Result<()>{
    for name in coordination_files() {
        std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(name))?;
    }
    Ok(())
}
fn open_existing(dir:&Path,name:&str)->Result<File>{ Ok(File::open(dir.join(name))?) }
/// A held reader admission. Dropping it releases the slot.
struct ReaderSlot{_file:File,index:usize}
fn take_slot(dir:&Path)->Result<ReaderSlot>{
    for index in 0..READER_SLOTS {
        let f=open_existing(dir,&slot_name(index))?;
        if io::try_lock_exclusive(&f)? { return Ok(ReaderSlot{_file:f,index}); }
    }
    Err(Error::ResourceLimit("page-WAL permits at most eight snapshots"))
}
// Admission holds the gate shared while it takes a slot and reads the
// published prefix; the writer holds it exclusive across its tail
// truncation + hint (open, rollback) and across the whole checkpoint. A
// reader therefore waits for a checkpoint in flight instead of finding every
// slot transiently taken, and never observes a half-written hint pair from a
// truncation. Reads after admission do no coordination at all.
fn gate_shared(dir:&Path)->Result<File>{ let g=open_existing(dir,GATE)?;io::lock_shared(&g)?;Ok(g) }
struct CheckpointGuard{_files:Vec<File>}
fn exclude_readers(dir:&Path)->Result<Option<CheckpointGuard>>{
    let gate=open_existing(dir,GATE)?;
    if !io::try_lock_exclusive(&gate)?{return Ok(None);}
    let mut files=vec![gate];
    for index in 0..READER_SLOTS {
        let f=open_existing(dir,&slot_name(index))?;
        if !io::try_lock_exclusive(&f)?{return Ok(None);}
        files.push(f);
    }
    Ok(Some(CheckpointGuard{_files:files}))
}

// ---- publication hint ---------------------------------------------------
// 48 bytes: magic | identity(16) | checkpoint_tx u64 | published_tx u64 |
// published_end u32 | crc32c. Two copies at offsets 0 and 48, written one
// after the other without a barrier: derived state, rewritten by every writer
// incarnation at open. It binds a reader to an exact `(tx, end)` that the
// writer's barrier already made durable; a stale copy over a reset WAL fails
// validation because a reused offset can never show `tx` at `end` again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Hint { identity:[u8;16], checkpoint_tx:u64, published_tx:u64, published_end:u32 }
fn hint_encode(h:&Hint)->[u8;HINT_BYTES]{
    let mut b=[0u8;HINT_BYTES];b[..8].copy_from_slice(HINT_MAGIC);b[8..24].copy_from_slice(&h.identity);
    b[24..32].copy_from_slice(&h.checkpoint_tx.to_le_bytes());b[32..40].copy_from_slice(&h.published_tx.to_le_bytes());
    b[40..44].copy_from_slice(&h.published_end.to_le_bytes());
    let crc=crc32c::crc32c(&b[..44]);b[44..].copy_from_slice(&crc.to_le_bytes());b
}
fn hint_decode(b:&[u8])->Option<Hint>{
    if b.len()!=HINT_BYTES||&b[..8]!=HINT_MAGIC||crc32c::crc32c(&b[..44])!=u32at(b,44){return None;}
    Some(Hint{identity:b[8..24].try_into().unwrap(),checkpoint_tx:u64at(b,24),published_tx:u64at(b,32),published_end:u32at(b,40)})
}
fn read_hint(io:&dyn FileIo,identity:&[u8;16])->Result<Hint>{
    if io.len()?<HINT_FILE_BYTES {return Err(unavailable("no publication hint beside the live writer"));}
    let mut b=[0u8;2*HINT_BYTES];io.read_at(&mut b,0)?;
    let mut best:Option<Hint>=None;
    for copy in [&b[..HINT_BYTES],&b[HINT_BYTES..]] {
        if let Some(h)=hint_decode(copy).filter(|h|&h.identity==identity) {
            if best.is_none_or(|old|h.published_tx>old.published_tx) {best=Some(h);}
        }
    }
    best.ok_or_else(||unavailable("publication hint damaged or from another database"))
}
fn validate_hint(h:&Hint,floor:u64,wal_len:u64)->Result<u64>{
    if !(h.checkpoint_tx<=floor && floor<=h.published_tx) {return Err(unavailable("publication hint from another checkpoint history"));}
    let end=h.published_end as u64;
    if end%FRAME as u64!=0 || end>wal_len {return Err(unavailable("publication hint exceeds the WAL"));}
    if end==0 && floor!=h.published_tx {return Err(unavailable("publication hint names an unabsorbed transaction without WAL"));}
    Ok(end)
}

struct State {
    latest: Index, committed: Arc<Index>, end: u64, last_commit: u64,
    pages: u32, committed_pages: u32, tx: u64, tx_crc: u32, cap: u64, free_head: u32,
    identity: [u8;16], features: u64,
    /// Newest verified checkpoint metadata transaction.
    floor: u64,
    /// Both hint copies describe the current published state.
    hint_current: bool,
    /// Copy 0 of the last hint write landed: readers can already see it.
    hint_published: bool,
    // Runtime policy installed by the layer that persists it (typed collection
    // header). Independent of the persisted total cap; never written to disk here.
    data_limit: u64, wal_limit: u64, tracked_limit: usize,
}

/// Monotonic page-WAL I/O counters. Relaxed atomics / pool Cells: the writer
/// is single-threaded. Diagnostic; not on the per-key path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoCounters {
    pub wal_frames_appended: u64,
    pub commit_frames: u64,
    pub wal_fsyncs_commit: u64,
    pub wal_fsyncs_checkpoint: u64,
    pub wal_fsyncs_open: u64,
    pub checkpoint_count: u64,
    pub data_pages_written_at_checkpoint: u64,
    pub data_fsyncs_checkpoint: u64,
    pub metadata_fsyncs: u64,
    pub wal_bytes_written: u64,
    pub data_bytes_written: u64,
    pub dirty_pages_flushed: u64,
}
impl IoCounters {
    pub fn wal_fsyncs(self) -> u64 {
        self.wal_fsyncs_commit.saturating_add(self.wal_fsyncs_checkpoint).saturating_add(self.wal_fsyncs_open)
    }
    pub fn fsyncs(self) -> u64 {
        self.wal_fsyncs().saturating_add(self.data_fsyncs_checkpoint).saturating_add(self.metadata_fsyncs)
    }
    pub fn pages_written(self) -> u64 {
        self.wal_frames_appended.saturating_add(self.data_pages_written_at_checkpoint)
    }
    pub fn bytes_written(self) -> u64 {
        self.wal_bytes_written.saturating_add(self.data_bytes_written)
    }
    pub fn saturating_sub(self, prev: Self) -> Self {
        Self {
            wal_frames_appended: self.wal_frames_appended.saturating_sub(prev.wal_frames_appended),
            commit_frames: self.commit_frames.saturating_sub(prev.commit_frames),
            wal_fsyncs_commit: self.wal_fsyncs_commit.saturating_sub(prev.wal_fsyncs_commit),
            wal_fsyncs_checkpoint: self.wal_fsyncs_checkpoint.saturating_sub(prev.wal_fsyncs_checkpoint),
            wal_fsyncs_open: self.wal_fsyncs_open.saturating_sub(prev.wal_fsyncs_open),
            checkpoint_count: self.checkpoint_count.saturating_sub(prev.checkpoint_count),
            data_pages_written_at_checkpoint: self.data_pages_written_at_checkpoint.saturating_sub(prev.data_pages_written_at_checkpoint),
            data_fsyncs_checkpoint: self.data_fsyncs_checkpoint.saturating_sub(prev.data_fsyncs_checkpoint),
            metadata_fsyncs: self.metadata_fsyncs.saturating_sub(prev.metadata_fsyncs),
            wal_bytes_written: self.wal_bytes_written.saturating_sub(prev.wal_bytes_written),
            data_bytes_written: self.data_bytes_written.saturating_sub(prev.data_bytes_written),
            dirty_pages_flushed: self.dirty_pages_flushed.saturating_sub(prev.dirty_pages_flushed),
        }
    }
}
struct IoAcc {
    wal_frames_appended: AtomicU64,
    commit_frames: AtomicU64,
    wal_fsyncs_commit: AtomicU64,
    wal_fsyncs_checkpoint: AtomicU64,
    wal_fsyncs_open: AtomicU64,
    checkpoint_count: AtomicU64,
    data_pages_written_at_checkpoint: AtomicU64,
    data_fsyncs_checkpoint: AtomicU64,
    metadata_fsyncs: AtomicU64,
    wal_bytes_written: AtomicU64,
    data_bytes_written: AtomicU64,
}
impl IoAcc {
    fn new() -> Self {
        Self {
            wal_frames_appended: AtomicU64::new(0),
            commit_frames: AtomicU64::new(0),
            wal_fsyncs_commit: AtomicU64::new(0),
            wal_fsyncs_checkpoint: AtomicU64::new(0),
            wal_fsyncs_open: AtomicU64::new(0),
            checkpoint_count: AtomicU64::new(0),
            data_pages_written_at_checkpoint: AtomicU64::new(0),
            data_fsyncs_checkpoint: AtomicU64::new(0),
            metadata_fsyncs: AtomicU64::new(0),
            wal_bytes_written: AtomicU64::new(0),
            data_bytes_written: AtomicU64::new(0),
        }
    }
    fn add(a: &AtomicU64, n: u64) { a.fetch_add(n, Ordering::Relaxed); }
    fn snapshot(&self) -> IoCounters {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        IoCounters {
            wal_frames_appended: g(&self.wal_frames_appended),
            commit_frames: g(&self.commit_frames),
            wal_fsyncs_commit: g(&self.wal_fsyncs_commit),
            wal_fsyncs_checkpoint: g(&self.wal_fsyncs_checkpoint),
            wal_fsyncs_open: g(&self.wal_fsyncs_open),
            checkpoint_count: g(&self.checkpoint_count),
            data_pages_written_at_checkpoint: g(&self.data_pages_written_at_checkpoint),
            data_fsyncs_checkpoint: g(&self.data_fsyncs_checkpoint),
            metadata_fsyncs: g(&self.metadata_fsyncs),
            wal_bytes_written: g(&self.wal_bytes_written),
            data_bytes_written: g(&self.data_bytes_written),
            dirty_pages_flushed: 0,
        }
    }
}

struct Pager {
    data: Arc<dyn FileIo>, wal: Arc<dyn FileIo>, state: Mutex<State>,
    readers: Arc<AtomicUsize>,
    // Read-write handle on `readers.lock` for hint writes; set by the writer's
    // `finish_open`. Lock order: `state` before `hint`.
    hint: Mutex<Option<Arc<dyn FileIo>>>,
    io: IoAcc,
}
impl Pager {
    fn initialize(dir: &Path, features:u64) -> Result<()> {
        let mut identity = [0;16];
        getrandom::fill(&mut identity).map_err(|e| std::io::Error::other(e.to_string()))?;
        let h = Header { root:0, free:0, cap:u64::MAX, identity, tx:0, features };
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
        Ok(Self::with_state(data,wal,state))
    }
    fn with_state(data:Arc<dyn FileIo>, wal:Arc<dyn FileIo>, state:State) -> Arc<Self> {
        Arc::new(Self{data,wal,state:Mutex::new(state),readers:Arc::new(AtomicUsize::new(0)),hint:Mutex::new(None),io:IoAcc::new()})
    }
    // Only the fully validated writer opener may normalize an uncommitted
    // tail. Under the gate (`held` when the opener took it before claiming
    // ownership; otherwise taken here, on files created just now): truncate,
    // barrier the recovered prefix, then publish it through the hint. A live
    // reader cannot be admitted anywhere in this window, so a hint left
    // behind by a crash (derived, deliberately unsynced) is never served
    // beside a later durable commit it does not name.
    fn finish_open(&self, dir:&Path, held:Option<File>) -> Result<()> {
        create_coordination(dir)?;
        let gate=match held { Some(g)=>g, None=>{ let g=open_existing(dir,GATE)?;io::lock_exclusive(&g)?;g } };
        let end = self.state.lock().unwrap().last_commit;
        if self.wal.len()? != end { self.wal.set_len(end)?; }
        // A prefix recovered after a failed barrier is made durable before any
        // reader can be pointed at it.
        self.wal.sync_full()?;IoAcc::add(&self.io.wal_fsyncs_open,1);
        let (hint_io,_)=io::open_file(&dir.join(GATE),IoMode::Buffered)?;
        *self.hint.lock().unwrap()=Some(Arc::from(hint_io));
        let mut s=self.state.lock().unwrap();
        let (tx,end)=(s.tx-1,s.last_commit);
        let r=self.write_hint(&mut s,tx,end);
        drop(gate);r
    }
    // Copy 0 alone already publishes to readers (it wins a torn pair by
    // transaction number), so `hint_published` reports it separately from
    // `hint_current` (both copies landed). Callers that publish a transaction
    // must swap their bookkeeping whenever copy 0 landed, error or not.
    fn write_hint(&self,s:&mut State,published_tx:u64,published_end:u64)->Result<()>{
        let io=self.hint.lock().unwrap().clone().ok_or_else(||bad("publication hint file unavailable"))?;
        let b=hint_encode(&Hint{identity:s.identity,checkpoint_tx:s.floor,published_tx,published_end:published_end as u32});
        s.hint_current=false;s.hint_published=false;
        io.write_at(&b,0)?;
        s.hint_published=true;
        io.write_at(&b,HINT_BYTES as u64)?;
        s.hint_current=true;Ok(())
    }
    fn inspect(data:&dyn FileIo,wal:&dyn FileIo)->Result<State>{ Self::inspect_bounded(data,wal,None) }
    // Strict posture always: a complete bad frame refuses. With `bound`, only
    // `[0, bound)` is read (a reader pinned to a published prefix); without
    // it the whole file, discarding an incomplete uncommitted tail.
    fn inspect_bounded(data:&dyn FileIo,wal:&dyn FileIo,bound:Option<u64>)->Result<State>{
        let disk = disk_header(data)?;
        let len=match bound {Some(b)=>b,None=>wal.len()?};
        if len>WAL_CAP {return Err(bad("WAL exceeds pilot bounded lookup allowance"));}
        if disk.is_none() && len != 0 { return Err(bad("WAL ownership/history lacks checkpoint metadata")); }
        let data_len=data.len()?;
        let data_pages=u32::try_from(data_len/PAGE as u64).map_err(|_|Error::TooLarge)?;
        let mut committed=Index::new();let mut pending=Index::new();let mut pages=data_pages;
        let floor = disk.map_or(0, |h| h.tx);
        let identity = disk.map_or([0;16], |h| h.identity);
        let features = disk.map_or_else(create_features, |h| h.features);
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
            free_head:h.map_or(0, |h|h.free),identity,features,floor,hint_current:false,hint_published:false,
            data_limit:u64::MAX,wal_limit:u64::MAX,tracked_limit:usize::MAX})
    }
    fn append(&self,s:&mut State,b:&[u8]) -> Result<()> {
        let end=s.end.checked_add(b.len() as u64).ok_or(Error::TooLarge)?;
        if end>WAL_CAP || end+s.pages as u64*PAGE as u64>s.cap {return Err(Error::ResourceLimit("page-WAL managed-byte allowance"));}
        if end>s.wal_limit {return Err(Error::ResourceLimit("page-WAL wal_bytes allowance"));}
        self.wal.write_at(b,s.end)?;s.end=end;IoAcc::add(&self.io.wal_bytes_written,b.len() as u64);Ok(())
    }
    // Refuse extent growth before the page is framed. `pages` may still rise
    // ahead of a later append failure; rollback restores the committed extent.
    fn grow(s:&mut State,p:u32)->Result<()>{
        let pages=s.pages.max(p.checked_add(1).ok_or(Error::TooLarge)?);
        if pages as u64*PAGE as u64>s.data_limit {return Err(Error::ResourceLimit("page-WAL data_bytes allowance"));}
        s.pages=pages;Ok(())
    }
    // `tracked_pages`: distinct pages the WAL index may hold between
    // checkpoints, the page-WAL's only RAM-proportional-to-change structure.
    // Checked at the actual addition, before the frame is written.
    fn track(s:&State,p:u32)->Result<()>{
        if !s.latest.contains_key(&p) && s.latest.len()>=s.tracked_limit {
            return Err(Error::ResourceLimit("page-WAL tracked_pages allowance"));
        }
        Ok(())
    }
    // Publication order: commit frame, FULL barrier, hint, then the in-memory
    // swap. A reader in any process can only be pointed at a prefix whose
    // barrier returned. If copy 0 of the hint failed, nothing is published:
    // readers and this handle stay on the previous transaction. If copy 0
    // landed and copy 1 failed, readers can already see the transaction, so
    // this handle's bookkeeping swaps too and the caller still gets an error
    // (the writer is poisoned until rollback/reopen rewrites both copies):
    // an uncertain outcome that is coherent, never old-here-new-there.
    fn publish(&self) -> Result<()> {
        let mut s=self.state.lock().unwrap();let mut body=[0;PAGE];body[..4].copy_from_slice(&s.tx_crc.to_le_bytes());
        let next = s.tx.checked_add(1).ok_or(Error::TooLarge)?;
        let b=frame(2,u32::MAX,s.tx,s.pages,&body,&s.identity);self.append(&mut s,&b)?;IoAcc::add(&self.io.commit_frames,1);
        self.wal.sync_full()?;IoAcc::add(&self.io.wal_fsyncs_commit,1);
        let (tx,end)=(s.tx,s.end);
        let r=self.write_hint(&mut s,tx,end);
        if s.hint_published {
            s.committed=Arc::new(s.latest.clone());s.committed_pages=s.pages;s.last_commit=s.end;
            s.tx=next;s.tx_crc=0;
        }
        r
    }
    // Callers hold the reader exclusion guard (gate + every slot) for the
    // whole call, so no reader is admitted or served from the WAL while data
    // pages are overwritten or the WAL is reset.
    fn checkpoint_with_crash(&self, fault:u8) -> Result<bool> {
        if self.readers.load(Ordering::SeqCst)!=0 {return Ok(false);}
        let mut s=self.state.lock().unwrap();
        if s.end!=s.last_commit {return Err(bad("checkpoint with unpublished pages"));}
        if !s.hint_current {return Err(bad("checkpoint requires a current publication hint; rollback or reopen"));}
        if s.end==0{return Ok(true);}
        for (&p,&off) in &s.latest {
            if p == 0 { continue; }
            let b=read_indexed_frame(&*self.wal,off)?;
            if u32at(&b,12)!=p {return Err(bad("WAL lookup identity"));}
            PageRef::open(&b[32..32+PAGE],p)?;self.data.write_at(&b[32..32+PAGE],p as u64*PAGE as u64)?;
            IoAcc::add(&self.io.data_pages_written_at_checkpoint,1);IoAcc::add(&self.io.data_bytes_written,PAGE as u64);
            if fault==1 {std::process::exit(86);}
        }
        self.data.set_len(s.pages as u64*PAGE as u64)?;self.data.sync_full()?;IoAcc::add(&self.io.data_fsyncs_checkpoint,1);
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
        // verified. `metadata_write_order` replaces the damaged or older copy
        // first. That copy is FULL-synced before the other is touched, so a
        // crash never leaves zero valid headers. The second copy is written
        // and read back but not synced: a crash may leave it torn or stale,
        // which `disk_header` already accepts by selecting the newer valid
        // copy (today's window between the two metadata syncs). The WAL
        // truncate barrier is a different file and does not make copy 1
        // durable; copy 1 rides the next data-file FULL sync (the following
        // checkpoint's page copy-back). Syncing it here would be a fourth
        // barrier. WAL truncate+sync is unchanged (option 3).
        let at = *s.latest.get(&0).ok_or_else(||bad("checkpoint lacks metadata"))?;
        let b = read_indexed_frame(&*self.wal,at)?;
        let header = Header::decode(&b[32..32+PAGE],0)?;
        let order = metadata_write_order(&*self.data)?;
        for (step,no) in order.into_iter().enumerate() {
            let page = header.page(no)?;
            self.data.write_at(&page,no as u64*PAGE as u64)?;IoAcc::add(&self.io.data_bytes_written,PAGE as u64);
            if (fault==5&&step==0)||(fault==6&&step==1) {std::process::exit(86);}
            if step==0 { self.data.sync_full()?;IoAcc::add(&self.io.metadata_fsyncs,1); }
            let mut actual=[0;PAGE];self.data.read_at(&mut actual,no as u64*PAGE as u64)?;
            if actual != page { return Err(bad("checkpoint metadata read-back mismatch")); }
            Header::decode(&actual,no)?;
        }
        if fault==3 {std::process::exit(86);}
        // Fault 7 (option 3): snapshot the committed WAL, run the truncate
        // (and today's WAL FULL sync), then put the pre-truncate bytes back
        // so reopen sees case (b) — leftover frames of the just-absorbed
        // floor, not a stale earlier incarnation. When sync 4 is removed the
        // restore still runs past set_len(0).
        let mut lost_truncate=None;
        if fault==7 {
            let n=self.wal.len()?;let mut b=vec![0;n as usize];
            if n>0 {self.wal.read_at(&mut b,0)?;}
            lost_truncate=Some(b);
        }
        self.wal.set_len(0)?;
        if fault==4 {std::process::exit(86);}
        self.wal.sync_full()?;IoAcc::add(&self.io.wal_fsyncs_checkpoint,1);
        if fault==7 {
            let b=lost_truncate.unwrap();
            self.wal.set_len(b.len() as u64)?;
            if !b.is_empty() {self.wal.write_at(&b,0)?;}
            std::process::exit(86);
        }
        s.latest.clear();s.committed=Arc::new(Index::new());s.end=0;s.last_commit=0;s.tx_crc=0;s.floor=header.tx;
        // Absorbed: the data file alone is transaction `floor`; the hint says so.
        let tx=s.tx-1;self.write_hint(&mut s,tx,0)?;
        IoAcc::add(&self.io.checkpoint_count,1);
        Ok(true)
    }
    // 4 MiB, or half of whichever allowance (persisted cap, runtime WAL
    // limit) would otherwise refuse the next transaction first.
    fn checkpoint_due(&self) -> bool {
        let s=self.state.lock().unwrap();let allowance=s.cap.saturating_sub(s.pages as u64*PAGE as u64);
        s.end >= (4*1024*1024).min(allowance/2).min(s.wal_limit/2).max(FRAME as u64)
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
        Pager::track(&s,p)?;
        let mut b=[0;PAGE];read_page(&*self.data,&*self.wal,&s.latest,p,&mut b)?;
        let page=PageRef::open(&b,p)?;
        if page.kind()!=PageKind::Free || page.nentries()!=1 || page.slot(0).len()!=4{return Err(bad("free page state"));}
        let next=u32at(page.slot(0),0);
        if next==p || (next!=0&&(next<2||next>=s.pages)){return Err(bad("free next extent/cycle"));}
        // Mark taken through WAL before returning it. A corrupt cycle cannot
        // reallocate this page even while its replacement is still cached.
        let mut page=PageMut::init(&mut b,PageKind::Free,0,p);page.insert_slot(0,b"taken")?;page.finalise(0);kernel::page::seal(&mut b,1);
        let f=frame(1,p,s.tx,0,&b,&s.identity);let at=s.end;self.append(&mut s,&f)?;IoAcc::add(&self.io.wal_frames_appended,1);
        s.tx_crc=crc32c::crc32c_append(s.tx_crc,&f);s.latest.insert(p,FrameRef::new(at,&f));s.free_head=next;Ok(Some(p))
    }
    fn push_free_page(&self,p:u32)->Result<()>{
        let mut s=self.state.lock().unwrap();if p<2{return Err(bad("free page extent"));}
        Pager::track(&s,p)?;
        // A pool-owned fresh page can be retired before ever being flushed.
        Pager::grow(&mut s,p)?;
        let mut b=[0;PAGE];let mut page=PageMut::init(&mut b,PageKind::Free,0,p);
        page.insert_slot(0,&s.free_head.to_le_bytes())?;page.finalise(0);kernel::page::seal(&mut b,1);
        let f=frame(1,p,s.tx,0,&b,&s.identity);let at=s.end;self.append(&mut s,&f)?;IoAcc::add(&self.io.wal_frames_appended,1);
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
        let mut s=self.state.lock().unwrap();Pager::track(&s,p)?;Pager::grow(&mut s,p)?;
        let f=frame(1,p,s.tx,0,b.try_into().unwrap(),&s.identity);let at=s.end;
        self.append(&mut s,&f)?;IoAcc::add(&self.io.wal_frames_appended,1);s.tx_crc=crc32c::crc32c_append(s.tx_crc,&f);s.latest.insert(p,FrameRef::new(at,&f));Ok(())
    }
    fn sync_data(&self)->Result<()>{Err(bad("use explicit page-WAL publication"))}
    fn sync_full(&self)->Result<()>{Err(bad("use explicit page-WAL publication"))}
    fn sync_full_primitive(&self)->&'static str{self.wal.sync_full_primitive()}
    fn sync_dir(&self)->Result<()>{self.data.sync_dir()}
}
// A view pins its index and its pager (files); the slot lives with the handle.
struct View {data:Arc<dyn FileIo>,wal:Arc<dyn FileIo>,index:Arc<Index>,pages:u32,readers:Arc<AtomicUsize>,_pager:Arc<Pager>}
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
fn snapshot_store(pager:&Arc<Pager>,lock:Option<Arc<File>>,dir:&Path,cache:usize,slot:Option<ReaderSlot>)->Result<PageWalStore>{
    let s=pager.state.lock().unwrap();
    pager.readers.fetch_update(Ordering::SeqCst,Ordering::SeqCst,|n|if n<READER_SLOTS{Some(n+1)}else{None})
        .map_err(|_|Error::ResourceLimit("page-WAL permits at most eight snapshots"))?;
    let view:Arc<dyn FileIo>=Arc::new(View{data:pager.data.clone(),wal:pager.wal.clone(),index:s.committed.clone(),
        pages:s.committed_pages,readers:pager.readers.clone(),_pager:pager.clone()});
    drop(s);
    let pool=PageWalStore::new_pool(view,cache,false)?;
    let mut out=PageWalStore{pager:None,pool,root:0,last:Cell::new(None),hits:Cell::new(0),attempts:Cell::new(0),
        poisoned:false,dirty:false,_lock:lock,dir:dir.into(),cache,slot,limits:(u64::MAX,u64::MAX,usize::MAX),
        reader_files:Some((pager.data.clone(),pager.wal.clone()))};
    out.load_header()?;Ok(out)
}
// A persisted reader bound is enforced on the slot index once the slot is
// held, in both paths, without re-running the caller's check. Serving
// readers always occupy indices below the bound; nothing is clamped.
fn enforce_reader_bound(slot:&ReaderSlot,bound:Option<usize>)->Result<()>{
    if bound.is_some_and(|r|slot.index>=r) {return Err(Error::ResourceLimit("persisted reader bound reached"));}
    Ok(())
}
// 5.1a: no writer is alive and none can start while `owner` is held shared.
// Strict inspection of the files is the committed state a writer would
// recover; the hint is not consulted. Nothing is created before the typed
// check passed; the check's reader bound is applied after the slot is taken.
fn admit_quiescent(dir:&Path,owner:File,cache:usize,check:impl FnOnce(&PageWalStore)->Result<Option<usize>>)->Result<PageWalStore>{
    let data:Arc<dyn FileIo>=io::open_file_readonly(&dir.join("data"))?.into();
    let wal:Arc<dyn FileIo>=io::open_file_readonly(&dir.join("wal"))?.into();
    let pager=Pager::from_files(data,wal)?;
    let mut store=snapshot_store(&pager,None,dir,cache,None)?;
    let bound=check(&store)?;
    create_coordination(dir)?;
    let gate=gate_shared(dir)?;
    let slot=take_slot(dir)?;
    enforce_reader_bound(&slot,bound)?;
    store.slot=Some(slot);
    drop(gate);drop(owner);
    Ok(store)
}
// 5.1b: a live writer owns the database. Only existing coordination files
// are opened. Under the gate: slot, checkpoint floor, hint, and a strict scan
// of exactly the published prefix. Any mismatch is an explicit refusal. A
// writer inside its own open/recovery holds the gate exclusive, so admission
// waits for the republished hint rather than serving a pre-crash one.
fn admit_live(dir:&Path,cache:usize,check:impl FnOnce(&PageWalStore)->Result<Option<usize>>)->Result<PageWalStore>{
    if coordination_files().iter().any(|n|!dir.join(n).exists()) {
        return Err(unavailable("no reader table beside the live writer (it is still opening, or is an older writer)"));
    }
    let gate=gate_shared(dir)?;
    let slot=take_slot(dir)?;
    let data:Arc<dyn FileIo>=io::open_file_readonly(&dir.join("data"))?.into();
    let wal:Arc<dyn FileIo>=io::open_file_readonly(&dir.join("wal"))?.into();
    let hint_io=io::open_file_readonly(&dir.join(GATE))?;
    let disk=disk_header(&*data)?.ok_or_else(||unavailable("checkpoint metadata unreadable during admission"))?;
    let hint=read_hint(&*hint_io,&disk.identity)?;
    let end=validate_hint(&hint,disk.tx,wal.len()?)?;
    let state=Pager::inspect_bounded(&*data,&*wal,Some(end))?;
    if state.last_commit!=end || state.tx-1!=hint.published_tx {
        return Err(unavailable("publication hint does not name a commit at its bound"));
    }
    let pager=Pager::with_state(data,wal,state);
    drop(gate);
    let store=snapshot_store(&pager,None,dir,cache,Some(slot))?;
    let bound=check(&store)?;
    enforce_reader_bound(store.slot.as_ref().unwrap(),bound)?;
    Ok(store)
}
/// Read-only committed-state overlay for recovery: committed WAL frames over
/// the data file, without any truncation, checkpoint or lock on `writer.lock`.
/// Returns the reader and, when the overlay could not be applied, the reason
/// (the caller then scans the bare data file and records that limitation).
pub fn candidate_reader(source:&Path)->Result<(CandidateReader,Option<String>)>{
    let data:Arc<dyn FileIo>=io::open_recovery_source(&source.join("data"))?.into();
    if !source.join("wal").exists() || !source.join("writer.lock").exists() {
        return Ok((CandidateReader::from_file(Box::new(repair::Source::plain(data)?)),Some("not a page-WAL directory".into())));
    }
    let wal:Arc<dyn FileIo>=io::open_recovery_source(&source.join("wal"))?.into();
    match Pager::inspect(&*data,&*wal) {
        Ok(state)=>Ok((CandidateReader::from_file(Box::new(repair::Source{data,wal,index:state.committed,pages:state.committed_pages})),None)),
        Err(e)=>Ok((CandidateReader::from_file(Box::new(repair::Source::plain(data)?)),Some(format!("committed WAL not interpretable: {e:?}")))),
    }
}
/// Whether a database CREATED from now on declares compact cell encodings.
///
/// Seeded from the `compact-cells` cargo feature, which is all that feature
/// still decides: every build of this release reads AND writes both cell
/// families, and an existing database's own header — never the build — chooses
/// the encoding its new cells are written in.
pub fn create_compact_cells()->bool{ create_features() & COMPACT_CELLS != 0 }

/// Choose the cell encoding for databases created from now on, returning the
/// previous setting. Existing databases are unaffected: opening one never
/// changes its declared features.
pub fn set_create_compact_cells(on:bool)->Result<bool>{
    Ok(format::set_create_features(if on {COMPACT_CELLS} else {0})? & COMPACT_CELLS != 0)
}

pub struct PageWalStore {
    pager:Option<Arc<Pager>>,pool:BufferPool,root:u32,last:Cell<Option<u32>>,hits:Cell<u64>,attempts:Cell<u64>,
    poisoned:bool,dirty:bool,_lock:Option<Arc<File>>,dir:PathBuf,cache:usize,slot:Option<ReaderSlot>,
    limits:(u64,u64,usize),reader_files:Option<(Arc<dyn FileIo>,Arc<dyn FileIo>)>,
}
impl PageWalStore {
    pub fn open(dir:&Path,create:bool,cache:usize)->Result<Self>{
        Self::open_inner(dir,create,cache,None,Pager::open,|_|Ok(()))
    }
    /// Explicit new-database codec, without changing the process default.
    /// Rebuilds preserve their source's codec even beside unrelated creates.
    pub(crate) fn create_with_compact_cells(dir:&Path,cache:usize,compact:bool)->Result<Self>{
        Self::open_inner(dir,true,cache,Some(if compact {COMPACT_CELLS} else {0}),Pager::open,|_|Ok(()))
    }
    /// Managed bytes sufficient to create an empty tree and persist its cap:
    /// three pages, root/header/commit frames, then cap-header/commit frames.
    /// Coordination and caller-owned marker files are additional to this bound.
    pub(crate) fn creation_cap_headroom()->u64{(3*PAGE+5*FRAME) as u64}
    /// Open a writer, running `check` on the committed state BEFORE the
    /// uncommitted WAL tail is normalized or any coordination file is
    /// created. A layer above (typed collections) refuses an unsupported
    /// catalog here without any byte of the source changing; a refused open
    /// releases the writer lock untouched.
    pub fn open_validated(dir:&Path,create:bool,cache:usize,check:impl FnOnce(&Self)->Result<()>)->Result<Self>{
        Self::open_inner(dir,create,cache,None,Pager::open,check)
    }
    fn open_with(dir:&Path,create:bool,cache:usize,opener:impl FnOnce(&Path)->Result<Arc<Pager>>)->Result<Self>{
        Self::open_inner(dir,create,cache,None,opener,|_|Ok(()))
    }
    fn new_pool(file:Arc<dyn FileIo>,cache:usize,writer:bool)->Result<BufferPool>{
        let pool=BufferPool::new(file,Arc::new(MemoryBudget::new(cache)),cache/PAGE)?;
        if writer {pool.set_stamp_gen(1);pool.set_reuse_limit(u64::MAX);}
        Ok(pool)
    }
    /// Install the cell encoding the DATABASE declares, so new cells match the
    /// file rather than this build's cargo features. Snapshot readers never
    /// encode, but decode both families unconditionally, so this is only
    /// meaningful on a writer's pool.
    fn install_codec(pool:&BufferPool,features:u64){ pool.set_compact_cells(features & COMPACT_CELLS != 0); }
    fn open_inner(dir:&Path,create:bool,cache:usize,create_codec:Option<u64>,opener:impl FnOnce(&Path)->Result<Arc<Pager>>,
        check:impl FnOnce(&Self)->Result<()>)->Result<Self>{
        if create {std::fs::create_dir(dir)?;}
        else { for name in ["data","wal","writer.lock"] { std::fs::metadata(dir.join(name))?; } }
        // Recovery window: an existing gate is taken exclusively BEFORE
        // ownership is claimed and held through inspection, the caller's
        // check, tail normalization and republication, so a live reader can
        // never be admitted on a hint that predates the recovered truth. Lock
        // order gate -> writer.lock, with a non-blocking ownership claim, so a
        // quiescent admission (writer.lock shared -> gate shared) cannot
        // deadlock: this side never waits while holding the gate. Without a
        // gate file, live readers fail closed until `finish_open` created it.
        let held=if dir.join(GATE).exists() { let g=open_existing(dir,GATE)?;io::lock_exclusive(&g)?;Some(g) } else { None };
        let lock=std::fs::OpenOptions::new().read(true).write(true).create(create).truncate(false).open(dir.join("writer.lock"))?;
        // A quiescent reader admission holds this shared for one bounded
        // scan; a writer arriving in that window is refused, not queued.
        if !io::try_lock_exclusive(&lock)?{return Err(Error::WriterLocked);}
        if create { Pager::initialize(dir,create_codec.unwrap_or_else(create_features))?; }
        let pager=opener(dir)?;let file:Arc<dyn FileIo>=pager.clone();
        // Every SUPPORTED feature set is writable by every build of this
        // release; `Header::decode_slot` already refused anything outside it.
        // The writer adopts the database's declared encoding instead of
        // demanding its own, and never rewrites those bits (Law 8).
        let features=pager.state.lock().unwrap().features;
        let pool=Self::new_pool(file,cache,true)?;
        Self::install_codec(&pool,features);
        let mut s=Self{pager:Some(pager),pool,root:0,last:Cell::new(None),hits:Cell::new(0),attempts:Cell::new(0),
            poisoned:false,dirty:false,_lock:Some(Arc::new(lock)),dir:dir.into(),cache,slot:None,
            limits:(u64::MAX,u64::MAX,usize::MAX),reader_files:None};
        if create {
            if s.pool.page_count()!=2{return Err(bad("create found unexpected pages"));}
            // Creation: coordination files and hint first, then the empty
            // tree's commit publishes through them.
            s.root=BTree::create(&s.pool,1,&s.last,&s.hits,&s.attempts)?.root();s.dirty=true;
            s.pager.as_ref().unwrap().finish_open(dir,None)?;
            s.commit()?;
        } else {s.load_header()?;}
        check(&s)?;
        s.pager.as_ref().unwrap().finish_open(dir,held)?;
        Ok(s)
    }
    /// Read-only handle on the newest PUBLISHED transaction, by path, beside
    /// a writer in this or another process, or on the committed files when no
    /// writer is alive. See `admit_live` / `admit_quiescent`.
    pub fn open_snapshot(dir:&Path,cache:usize)->Result<Self>{ Self::open_snapshot_validated(dir,cache,|_|Ok(None)) }
    /// `check` runs on the admitted view before the handle is returned (on
    /// the quiescent path before any coordination file exists) and returns
    /// the persisted reader bound, if any, which is then enforced on the
    /// slot index this handle holds.
    pub fn open_snapshot_validated(dir:&Path,cache:usize,check:impl FnOnce(&Self)->Result<Option<usize>>)->Result<Self>{
        for name in ["data","wal","writer.lock"] { std::fs::metadata(dir.join(name))?; }
        let owner=open_existing(dir,"writer.lock")?;
        if io::try_lock_shared(&owner)? { admit_quiescent(dir,owner,cache,check) }
        else { drop(owner);admit_live(dir,cache,check) }
    }
    pub fn dir(&self)->&Path{&self.dir}
    pub fn is_snapshot(&self)->bool{self.pager.is_none()}
    pub fn is_dirty(&self)->bool{self.dirty}
    /// Slot index (0..8) held by a snapshot handle; lowest free slot at admission.
    pub fn reader_slot(&self)->Option<usize>{self.slot.as_ref().map(|s|s.index)}
    /// Persisted managed-byte allowance (data + WAL); `u64::MAX` means none.
    pub fn managed_cap(&self)->Option<u64>{self.pager.as_ref().map(|p|p.state.lock().unwrap().cap)}
    /// Runtime allowances checked before every page write and append: data
    /// extent, WAL bytes, and distinct pages in the WAL index between
    /// checkpoints (`tracked_pages`). Not persisted here; the owner persists
    /// and reinstalls them at every open and after rollback.
    pub fn set_runtime_limits(&mut self,data_bytes:u64,wal_bytes:u64,tracked_pages:usize)->Result<()>{
        let pager=self.pager.as_ref().ok_or(Error::ReadOnly)?;
        let mut s=pager.state.lock().unwrap();
        if s.pages as u64*PAGE as u64>data_bytes || s.end>wal_bytes || s.latest.len()>tracked_pages {
            return Err(Error::ResourceLimit("existing extent exceeds the requested allowance"));
        }
        s.data_limit=data_bytes;s.wal_limit=wal_bytes;s.tracked_limit=tracked_pages;drop(s);
        self.limits=(data_bytes,wal_bytes,tracked_pages);Ok(())
    }
    pub fn data_bytes(&self)->u64{self.pool.page_count() as u64*PAGE as u64}
    /// Distinct pages currently held by the WAL index (writer only).
    pub fn tracked_pages(&self)->Option<usize>{self.pager.as_ref().map(|p|p.state.lock().unwrap().latest.len())}
    /// Diagnostics (read-only footprint attribution): root page, extent, and
    /// one page's bytes as this handle sees them (WAL overlay included).
    pub fn root(&self)->u32{self.root}
    pub fn page_count(&self)->u32{self.pool.page_count()}
    /// Diagnostic only: buffer-pool page accesses since open.
    pub fn pool_accesses(&self)->u64{let s=self.pool.stats();s.hits+s.misses}
    pub fn page_bytes(&self,no:u32)->Result<Vec<u8>>{self.ready()?;let r=self.pool.get(no)?;Ok(r[..].to_vec())}
    /// Discard every uncommitted change in place: the files are re-inspected
    /// exactly as a reopen would, the uncommitted tail is truncated, the
    /// recovered prefix is barriered and republished through the hint, the
    /// pool is rebuilt and the header reloaded. Live readers keep their
    /// frames (all below the last commit); the writer lock is never released.
    /// A commit whose frame is complete on disk but whose barrier or hint
    /// write failed is therefore reported as committed, the same truth a
    /// reopen would find.
    pub fn rollback(&mut self)->Result<()>{
        let pager=self.pager.clone().ok_or(Error::ReadOnly)?;
        let mut fresh=Pager::inspect(&*pager.data,&*pager.wal)?;
        fresh.data_limit=self.limits.0;fresh.wal_limit=self.limits.1;fresh.tracked_limit=self.limits.2;
        {let mut s=pager.state.lock().unwrap();
         if fresh.identity!=s.identity||fresh.features!=s.features{return Err(bad("rollback identity/features changed"));}
         *s=fresh;}
        pager.finish_open(&self.dir,None)?;
        self.pool=Self::new_pool(pager.clone(),self.cache,true)?;
        Self::install_codec(&self.pool,pager.state.lock().unwrap().features);
        self.root=0;self.last.set(None);self.poisoned=true;self.dirty=false;
        self.load_header()?;self.poisoned=false;Ok(())
    }
    /// Ascending records from `from`, borrowing this handle for the cursor's
    /// life. Snapshot readers scan their published index; a writer scans its
    /// working tree.
    pub fn range(&self,from:&[u8])->Result<RangeIter<'_>>{self.ready()?;self.tree().range(from)}
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
    // Fold committed WAL before the first frame of a new transaction when the
    // next append would miss wal_bytes, or a reader-deferred checkpoint is due.
    // Admission holds `state` and has no exclusion gate, so this lives here.
    // Safe: !dirty and end==last_commit, so only published frames are folded.
    fn fold_committed_wal_if_at_cap(&mut self) -> Result<()> {
        if self.dirty { return Ok(()); }
        let Some(pager) = self.pager.as_ref() else { return Ok(()); };
        let due = pager.checkpoint_due();
        {
            let s = pager.state.lock().unwrap();
            let next_misses = s.end.saturating_add(FRAME as u64) > s.wal_limit;
            if !due && !next_misses { return Ok(()); }
            if s.end != s.last_commit { return Ok(()); }
        }
        match self.checkpoint_guarded(0) {
            Ok(_) => Ok(()),
            Err(e) => { self.poisoned = true; Err(e) }
        }
    }
    pub fn put(&mut self,k:&[u8],v:&[u8])->Result<()> {
        self.writable()?;self.fold_committed_wal_if_at_cap()?;self.dirty=true;
        let r={let mut t=self.tree();t.insert(k,v).map(|_|t.root())};
        match r {Ok(root)=>{self.root=root;Ok(())},Err(e)=>{self.poisoned=true;Err(e)}}
    }
    pub fn delete(&mut self,k:&[u8])->Result<bool>{
        self.writable()?;self.fold_committed_wal_if_at_cap()?;self.dirty=true;
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
            self.pool.finish_stable_page_epoch();self.dirty=false;
            Ok(pager.checkpoint_due())
        })();
        match result {
            Ok(true)=>{let r=self.checkpoint_guarded(0).map(|_|());if r.is_err(){self.poisoned=true;}r}
            Ok(false)=>Ok(()),
            Err(e)=>{self.poisoned=true;Err(e)}
        }
    }
    // Every checkpoint path: readers in any process defer it (Ok(false)). The
    // conservative gate is held for the whole fold and WAL reset; admission
    // can wait for a checkpoint, reads after admission never do.
    fn checkpoint_guarded(&self,fault:u8)->Result<bool>{
        let Some(_guard)=exclude_readers(&self.dir)? else {return Ok(false)};
        self.pager.as_ref().unwrap().checkpoint_with_crash(fault)
    }
    pub fn checkpoint(&mut self)->Result<bool>{self.writable()?;
        if self.dirty{return Err(bad("checkpoint requires commit"));}
        let r=self.checkpoint_guarded(0);if r.is_err(){self.poisoned=true;}r
    }
    pub fn set_cap(&mut self,bytes:u64)->Result<()> {
        self.writable()?;if self.dirty{return Err(bad("set cap requires committed state"));}
        {let mut s=self.pager.as_ref().unwrap().state.lock().unwrap();
        if s.end+s.pages as u64*PAGE as u64+2*FRAME as u64>bytes{return Err(Error::ResourceLimit("cap lacks policy commit headroom"));}s.cap=bytes;}
        self.dirty=true;self.commit()
    }
    /// Snapshot from the writer handle: its published index, a gate pass and
    /// a slot. Keeps the writer lock alive with the view, so a reopen waits
    /// for the view (`surviving_snapshot_keeps_wal_reset_ownership`).
    pub fn snapshot(&self)->Result<Self>{
        self.ready()?;
        let p=self.pager.as_ref().ok_or(Error::ReadOnly)?;
        let gate=gate_shared(&self.dir)?;let slot=take_slot(&self.dir)?;drop(gate);
        snapshot_store(p,self._lock.clone(),&self.dir,self.cache,Some(slot))
    }
    pub fn wal_bytes(&self)->u64 {self.pager.as_ref().map_or(0,|p|p.state.lock().unwrap().end)}
    /// Diagnostic: monotonic page-WAL and pool I/O counters since this handle opened.
    pub fn io_counters(&self)->IoCounters {
        let mut c=self.pager.as_ref().map(|p|p.io.snapshot()).unwrap_or_default();
        c.dirty_pages_flushed=self.pool.stats().dirty_pages_flushed;c
    }
    /// Diagnostic snapshot/reset of existing FileIo counters, data then WAL.
    /// Each tuple is (write calls, issued write bytes, read calls). Buffered
    /// calls are not physical-device I/O. Snapshot handles report their own
    /// read-only files (Law 6 counter gate); writer-derived snapshots share
    /// the writer's files and report None.
    pub fn take_file_io_stats(&self)->Option<[(u64,u64,u64);2]> {
        if let Some(p)=self.pager.as_ref() { return Some([p.data.stats()?.take(),p.wal.stats()?.take()]); }
        let (d,w)=self.reader_files.as_ref()?;
        Some([d.stats()?.take(),w.stats()?.take()])
    }
    /// Pilot fault harness only: abruptly exits inside a checkpoint stage.
    pub fn test_checkpoint_crash(&mut self,stage:u8)->Result<bool>{
        self.writable()?;assert!((1..=7).contains(&stage));assert!(!self.dirty);
        self.checkpoint_guarded(stage)
    }
    /// Test seam: route publication-hint writes through a caller's `FileIo`.
    #[cfg(test)]
    fn test_replace_hint_io(&self,io:Arc<dyn FileIo>){
        *self.pager.as_ref().unwrap().hint.lock().unwrap()=Some(io);
    }
    /// Test seam: the transaction this writer considers published.
    #[cfg(test)]
    fn test_published_tx(&self)->u64{self.pager.as_ref().unwrap().state.lock().unwrap().tx-1}
}
