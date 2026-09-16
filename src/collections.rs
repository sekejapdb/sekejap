//! Typed collections on the V2 page-WAL store. No JSON text is persisted.
//! Keys and immutable layouts are documented in docs/COLLECTIONS.md; the
//! storage path, its limits and its deviations in docs/V2_COLLECTION_INTEGRATION.md.
use crate::collection_backend::{check_config, check_limits, Backend};
use crate::pagewal::PageWalStore;
use crate::{decode_dense_v3, encode_dense_v3, Kind, Layout};
use kernel::{btree::RangeIter, limits::ResourceLimits, store::Config};
use serde_json::Value;
use std::{
    cell::RefCell,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
pub enum Error {
    InvalidInput(String),
    NotFound(&'static str),
    AlreadyExists,
    ReadOnly,
    Failed,
    Corrupt(String),
    /// A format, policy or configuration this binary does not implement.
    /// Raised before any byte of the source changes.
    Unsupported(String),
    Kernel(kernel::Error),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<kernel::Error> for Error {
    fn from(e: kernel::Error) -> Self {
        Self::Kernel(e)
    }
}
pub type Result<T> = std::result::Result<T, Error>;
fn invalid(e: impl fmt::Display) -> Error {
    Error::InvalidInput(e.to_string())
}
fn corrupt(e: impl fmt::Display) -> Error {
    Error::Corrupt(e.to_string())
}
const KEY_FIELD: &str = "__e4_key";
const CREATED: &str = "_created_unix";
const UPDATED: &str = "_updated_unix";
const PAD: usize = 2081;
const HEADER_MAGIC: &[u8; 8] = b"E4COLL1\0";
const CATALOG_MAGIC: &[u8; 8] = b"E4CAT01\0";
const COUNTER_MAGIC: &[u8; 8] = b"E4SEQ01\0";
/// Header payload: next collection, next layout (8 bytes, every ordinary
/// database, byte-identical to the inherited encoding), optionally followed
/// by the kernel's 56-byte `E4LIMIT1` policy record (only databases created
/// through `create_limited`). Other lengths are refused before any write.
const HEADER_PLAIN: usize = 8;
const HEADER_LIMITED: usize = 8 + 56;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CollectionId(pub u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntityId {
    pub collection: CollectionId,
    pub sequence: u64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Entity {
    pub id: EntityId,
    pub key: String,
    pub document: Value,
}
type VectorCells = Vec<(usize, Vec<u8>)>;
struct LoadedEntity {
    entity: Entity,
    vectors: VectorCells,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct CollectionOptions {
    pub timestamps: bool,
}
#[derive(Clone, Debug)]
pub struct CollectionInfo {
    pub id: CollectionId,
    pub name: String,
    pub layout: Layout,
    pub timestamps: bool,
}
pub trait Clock: Send + Sync {
    fn unix_seconds(&self) -> i64;
}
struct SystemClock;
impl Clock for SystemClock {
    fn unix_seconds(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs().min(i64::MAX as u64) as i64)
    }
}
#[derive(Clone, Debug, PartialEq)]
struct Catalog {
    id: CollectionId,
    name: String,
    layout: u32,
    timestamps: bool,
}
#[derive(Clone, Copy, Debug, PartialEq)]
struct HeaderInfo {
    next_collection: u32,
    next_layout: u32,
    limits: Option<ResourceLimits>,
}
// E3 db.rs MembershipBatch uses the same fixed one-collection accumulator:
// switching collections flushes it; memory never grows with collection count.
struct Sequence {
    collection: CollectionId,
    next: u64,
}
pub struct Database {
    store: Backend,
    path: PathBuf,
    read_only: bool,
    failed: bool,
    sequence: Option<Sequence>,
    catalog_cache: RefCell<Option<Catalog>>,
    layout_cache: RefCell<Option<Arc<Layout>>>,
    clock: Arc<dyn Clock>,
    limits: Option<ResourceLimits>,
}

fn ordered(n: u64) -> Vec<u8> {
    let bytes = n.to_be_bytes();
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(7);
    let mut key = vec![0x80 + (8 - start) as u8];
    key.extend_from_slice(&bytes[start..]);
    key
}
fn read_ordered(b: &[u8], at: &mut usize) -> Result<u64> {
    let width = usize::from(*b.get(*at).ok_or_else(|| corrupt("truncated integer key"))?)
        .checked_sub(0x80)
        .ok_or_else(|| corrupt("integer key tag"))?;
    if !(1..=8).contains(&width) {
        return Err(corrupt("integer key width"));
    }
    *at += 1;
    let bytes = b
        .get(*at..*at + width)
        .ok_or_else(|| corrupt("truncated integer key"))?;
    if width > 1 && bytes[0] == 0 {
        return Err(corrupt("noncanonical integer key"));
    }
    let mut out = [0u8; 8];
    out[8 - width..].copy_from_slice(bytes);
    *at += width;
    Ok(u64::from_be_bytes(out))
}
fn prefix(tag: u8, c: CollectionId) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend(ordered(c.0 as u64));
    k
}
fn row_key(id: EntityId) -> Vec<u8> {
    let mut k = prefix(0x40, id.collection);
    k.extend(ordered(id.sequence));
    k
}
fn row_id(k: &[u8]) -> Result<EntityId> {
    if k.first() != Some(&0x40) {
        return Err(corrupt("entity key tag"));
    }
    let mut at = 1;
    let c = u32::try_from(read_ordered(k, &mut at)?).map_err(corrupt)?;
    let sequence = read_ordered(k, &mut at)?;
    if at != k.len() || c == 0 || sequence == 0 {
        return Err(corrupt("entity identity"));
    }
    Ok(EntityId {
        collection: CollectionId(c),
        sequence,
    })
}
fn mapping_key(c: CollectionId, key: &str) -> Vec<u8> {
    let mut k = prefix(0x20, c);
    k.extend_from_slice(key.as_bytes());
    k
}
fn name_key(name: &str) -> Vec<u8> {
    let mut k = vec![0x10];
    k.extend_from_slice(name.as_bytes());
    k
}
fn replica_key(tag: u8, id: u32, copy: u8) -> Vec<u8> {
    let mut k = vec![tag, copy];
    k.extend(ordered(id as u64));
    k
}
fn layout_key(id: u32, copy: u8) -> Vec<u8> {
    let mut k = vec![0, 240];
    k.extend_from_slice(&(u64::from(id) * 3 + u64::from(copy)).to_be_bytes());
    k
}
fn vector_key(id: EntityId, field: usize) -> Vec<u8> {
    let mut k = row_key(id);
    k[0] = 0x60;
    k.extend(ordered(field as u64));
    k
}
fn packet(magic: &[u8; 8], payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > PAD - 14 {
        return Err(invalid("metadata too large"));
    }
    let mut b = magic.to_vec();
    b.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    b.extend_from_slice(payload);
    b.resize(PAD - 4, 0);
    b.extend_from_slice(&crc32c::crc32c(&b).to_le_bytes());
    Ok(b)
}
fn unpack<'a>(b: &'a [u8], magic: &[u8; 8]) -> Result<&'a [u8]> {
    if b.len() != PAD
        || &b[..8] != magic
        || crc32c::crc32c(&b[..PAD - 4]) != u32::from_le_bytes(b[PAD - 4..].try_into().unwrap())
    {
        return Err(corrupt("metadata checksum/magic/size"));
    }
    let n = u16::from_be_bytes(b[8..10].try_into().unwrap()) as usize;
    if n > PAD - 14 || b[10 + n..PAD - 4].iter().any(|b| *b != 0) {
        return Err(corrupt("metadata length/padding"));
    }
    Ok(&b[10..10 + n])
}
fn catalog_bytes(c: &Catalog) -> Result<Vec<u8>> {
    let mut b = c.id.0.to_be_bytes().to_vec();
    b.extend_from_slice(&c.layout.to_be_bytes());
    b.push(u8::from(c.timestamps));
    b.extend_from_slice(c.name.as_bytes());
    packet(CATALOG_MAGIC, &b)
}
fn parse_catalog(b: &[u8]) -> Result<Catalog> {
    let b = unpack(b, CATALOG_MAGIC)?;
    if b.len() < 10 || b[8] > 1 {
        return Err(corrupt("catalog fields"));
    }
    let id = u32::from_be_bytes(b[..4].try_into().unwrap());
    let layout = u32::from_be_bytes(b[4..8].try_into().unwrap());
    let name = std::str::from_utf8(&b[9..]).map_err(corrupt)?.to_owned();
    if id == 0 || layout == 0 || name.len() > 255 {
        return Err(corrupt("catalog domain"));
    }
    Ok(Catalog {
        id: CollectionId(id),
        name,
        layout,
        timestamps: b[8] == 1,
    })
}
/// The kernel's own `E4LIMIT1` record: a damaged one is corruption, a valid
/// one this page-WAL release cannot honour (e.g. more readers than slots) is
/// refused as unsupported, not silently narrowed.
fn decode_limits(b: &[u8]) -> Result<ResourceLimits> {
    let l = ResourceLimits::decode(b)
        .map_err(|e| corrupt(format!("persisted resource policy: {e:?}")))?;
    check_limits(l).map_err(|e| Error::Unsupported(format!("persisted resource policy: {e:?}")))
}
fn header_bytes(h: HeaderInfo) -> Result<Vec<u8>> {
    let mut payload = h.next_collection.to_be_bytes().to_vec();
    payload.extend_from_slice(&h.next_layout.to_be_bytes());
    if let Some(l) = h.limits {
        payload.extend(l.encode());
    }
    packet(HEADER_MAGIC, &payload)
}
fn parse_header(b: &[u8]) -> Result<HeaderInfo> {
    if b.len() == PAD && b.starts_with(b"E4COLL") && &b[..8] != HEADER_MAGIC {
        // An unknown version is authoritative only inside an intact packet.
        // A damaged magic byte must still allow an independent replica to win.
        unpack(b, b[..8].try_into().unwrap())?;
        return Err(Error::Unsupported(format!(
            "typed-collection header version {:?} is newer than this binary",
            String::from_utf8_lossy(&b[..7])
        )));
    }
    let b = unpack(b, HEADER_MAGIC)?;
    if b.len() != HEADER_PLAIN && b.len() != HEADER_LIMITED {
        return Err(Error::Unsupported(format!(
            "typed-collection header payload of {} bytes is not implemented by this binary",
            b.len()
        )));
    }
    let a = u32::from_be_bytes(b[..4].try_into().unwrap());
    let z = u32::from_be_bytes(b[4..8].try_into().unwrap());
    if a == 0 || z == 0 {
        return Err(corrupt("header counters"));
    }
    Ok(HeaderInfo {
        next_collection: a,
        next_layout: z,
        limits: (b.len() == HEADER_LIMITED)
            .then(|| decode_limits(&b[8..]))
            .transpose()?,
    })
}
/// Agreeing intact copies win; a damaged page or descriptor loses one copy,
/// not all metadata. Conflicting valid copies are an error, never a vote. An
/// unsupported-but-intact copy is reported as such rather than as damage.
fn replicas<T: PartialEq>(
    get: impl Fn(&[u8]) -> Result<Option<Vec<u8>>>,
    keys: impl Fn(u8) -> Vec<u8>,
    parse: impl Fn(&[u8]) -> Result<T>,
) -> Result<T> {
    let mut good = None;
    for copy in 0..3 {
        match get(&keys(copy)) {
            Ok(Some(b)) => match parse(&b) {
                Ok(value) => {
                    if good.as_ref().is_some_and(|old| old != &value) {
                        return Err(corrupt("conflicting metadata copies"));
                    }
                    good = Some(value);
                }
                Err(e @ Error::Unsupported(_)) => return Err(e),
                Err(_) => {}
            },
            Ok(None) | Err(Error::Kernel(kernel::Error::Corrupt { .. })) => {}
            Err(e) => return Err(e),
        }
    }
    good.ok_or_else(|| corrupt("all metadata copies missing or damaged"))
}
fn read_header(s: &PageWalStore) -> Result<HeaderInfo> {
    replicas(|k| s.get(k).map_err(Error::from), |i| vec![0, 0, i], parse_header)
}
/// The typed refusal the page-WAL runs before it normalizes or creates
/// anything. The precise collection error is kept in `detail`; the kernel
/// sees an opaque refusal.
fn typed_check(s: &PageWalStore, detail: &RefCell<Option<Error>>) -> kernel::Result<()> {
    match read_header(s) {
        Ok(_) => Ok(()),
        Err(e) => {
            *detail.borrow_mut() = Some(e);
            Err(kernel::Error::Corrupt {
                page_no: 0,
                why: "typed-collection catalog refused before open",
            })
        }
    }
}

impl Database {
    pub fn create(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        Self::create_inner(path.as_ref(), config, None)
    }
    /// Create with a persisted resource policy. Every field is enforced on
    /// this path or the policy is refused before the directory exists; see
    /// `collection_backend::check_limits`.
    pub fn create_limited(
        path: impl AsRef<Path>,
        config: Config,
        limits: ResourceLimits,
    ) -> Result<Self> {
        Self::create_inner(path.as_ref(), config, Some(limits))
    }
    fn create_inner(path: &Path, config: Config, limits: Option<ResourceLimits>) -> Result<Self> {
        let cache = check_config(&config).map_err(Error::Unsupported)?;
        let limits = limits.map(check_limits).transpose()?;
        let store = Backend::create(path, cache, limits)?;
        let mut db = Self::wrap(store, false, limits);
        db.write_header(1, 1)?;
        db.commit()?;
        Ok(db)
    }
    /// Open the single writer. The catalog header is validated before the
    /// page-WAL normalizes anything: an unsupported or damaged catalog, an
    /// unknown storage feature, or a non-page-WAL directory is refused with
    /// the source byte-for-byte untouched.
    pub fn open(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        let path = path.as_ref();
        let cache = check_config(&config).map_err(Error::Unsupported)?;
        if path.join("data").exists() && !path.join("writer.lock").exists() {
            return Err(Error::Unsupported(
                "not a V2 page-WAL typed-collection database (no writer.lock)".into(),
            ));
        }
        let detail = RefCell::new(None);
        let store = Backend::open(path, cache, |s| typed_check(s, &detail));
        let store = match store {
            Ok(s) => s,
            Err(k) => return Err(detail.into_inner().unwrap_or(Error::Kernel(k))),
        };
        let h = read_header(store.store())?;
        let mut db = Self::wrap(store, false, h.limits);
        if let Some(l) = h.limits {
            db.store.install_limits(l)?;
        }
        Ok(db)
    }
    /// Read-only view of the newest PUBLISHED transaction, beside a writer in
    /// this or another process, or of the committed files when no writer is
    /// alive. The typed catalog check runs inside admission, before any
    /// coordination file is created. Holds a reader slot for its life, which
    /// defers (never blocks) the writer's checkpoint; a persisted `readers`
    /// bound is enforced by the slot index.
    pub fn open_snapshot(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        let cache = check_config(&config).map_err(Error::Unsupported)?;
        let detail = RefCell::new(None);
        let limits = std::cell::Cell::new(None);
        // The typed check also hands the persisted reader bound to the
        // page-WAL, which enforces it on the slot this handle holds.
        let store = Backend::open_snapshot(path.as_ref(), cache, |s| match read_header(s) {
            Ok(h) => {
                limits.set(h.limits);
                Ok(h.limits.map(|l| l.readers as usize))
            }
            Err(e) => {
                *detail.borrow_mut() = Some(e);
                Err(kernel::Error::Corrupt {
                    page_no: 0,
                    why: "typed-collection catalog refused before open",
                })
            }
        });
        let store = match store {
            Ok(s) => s,
            Err(k) => return Err(detail.into_inner().unwrap_or(Error::Kernel(k))),
        };
        Ok(Self::wrap(store, true, limits.get()))
    }
    fn wrap(store: Backend, read_only: bool, limits: Option<ResourceLimits>) -> Self {
        Self {
            path: store.dir().to_owned(),
            store,
            read_only,
            failed: false,
            sequence: None,
            catalog_cache: RefCell::new(None),
            layout_cache: RefCell::new(None),
            clock: Arc::new(SystemClock),
            limits,
        }
    }
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }
    /// The persisted resource policy, if the database was created with one.
    pub fn limits(&self) -> Option<ResourceLimits> {
        self.limits
    }
    /// Current (data extent, WAL) bytes of the underlying page-WAL store.
    /// The 96-byte publication hint (`readers.lock`) is not included.
    pub fn storage_bytes(&self) -> Result<(u64, u64)> {
        let s = self.store()?;
        Ok((s.data_bytes(), s.wal_bytes()))
    }
    /// Distinct pages held by the WAL index since the last checkpoint: what a
    /// persisted `tracked_pages` policy bounds. `None` for snapshots.
    pub fn tracked_pages(&self) -> Result<Option<usize>> {
        Ok(self.store()?.tracked_pages())
    }
    fn store(&self) -> Result<&Backend> {
        if self.failed {
            return Err(Error::Failed);
        }
        Ok(&self.store)
    }
    fn writer(&mut self) -> Result<&mut Backend> {
        self.ready_write()?;
        Ok(&mut self.store)
    }
    fn ready_write(&self) -> Result<()> {
        self.store()?;
        if self.read_only {
            Err(Error::ReadOnly)
        } else {
            Ok(())
        }
    }
    fn finish<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn replicas<T: PartialEq>(
        &self,
        keys: impl Fn(u8) -> Vec<u8>,
        parse: impl Fn(&[u8]) -> Result<T>,
    ) -> Result<T> {
        let store = self.store()?;
        replicas(|k| store.get(k).map_err(Error::from), keys, parse)
    }
    fn header(&self) -> Result<(u32, u32)> {
        let h = self.replicas(|i| vec![0, 0, i], parse_header)?;
        Ok((h.next_collection, h.next_layout))
    }
    fn write_header(&mut self, collection: u32, layout: u32) -> Result<()> {
        let b = header_bytes(HeaderInfo {
            next_collection: collection,
            next_layout: layout,
            limits: self.limits,
        })?;
        for i in 0..3 {
            self.writer()?.put(&[0, 0, i], &b)?;
        }
        Ok(())
    }
    fn catalog(&self, id: CollectionId) -> Result<Catalog> {
        self.store()?;
        if let Some(c) = self.catalog_cache.borrow().as_ref().filter(|c| c.id == id) {
            return Ok(c.clone());
        }
        let c = self.replicas(
            |i| replica_key(1, id.0, i),
            |b| {
                let c = parse_catalog(b)?;
                if c.id != id {
                    return Err(corrupt("catalog identity"));
                }
                Ok(c)
            },
        )?;
        *self.catalog_cache.borrow_mut() = Some(c.clone());
        Ok(c)
    }
    fn layout(&self, id: u32) -> Result<Arc<Layout>> {
        self.store()?;
        if let Some(l) = self
            .layout_cache
            .borrow()
            .as_ref()
            .filter(|l| l.id == u64::from(id))
        {
            return Ok(l.clone());
        }
        let l = self.replicas(
            |i| layout_key(id, i),
            |b| {
                let l = Layout::from_descriptor(b).map_err(corrupt)?;
                if l.id != u64::from(id)
                    || l.fields.first() != Some(&(KEY_FIELD.to_owned(), Kind::Text))
                {
                    return Err(corrupt("layout identity/key slot"));
                }
                Ok(l)
            },
        )?;
        let l = Arc::new(l);
        *self.layout_cache.borrow_mut() = Some(l.clone());
        Ok(l)
    }
    fn persist_catalog(&mut self, c: &Catalog) -> Result<()> {
        let b = catalog_bytes(c)?;
        for i in 0..3 {
            self.writer()?.put(&replica_key(1, c.id.0, i), &b)?;
        }
        *self.catalog_cache.borrow_mut() = Some(c.clone());
        Ok(())
    }
    fn persist_layout(&mut self, l: &Layout) -> Result<()> {
        let b = l.descriptor().map_err(invalid)?;
        for i in 0..3 {
            self.writer()?.put(&layout_key(l.id as u32, i), &b)?;
        }
        *self.layout_cache.borrow_mut() = Some(Arc::new(l.clone()));
        Ok(())
    }
    fn make_layout(id: u32, fields: Vec<(String, Kind)>, timestamps: bool) -> Result<Layout> {
        for (name, _) in &fields {
            if reserved(name) || (timestamps && matches!(name.as_str(), CREATED | UPDATED)) {
                return Err(invalid("managed/reserved field declaration"));
            }
        }
        let mut out = vec![(KEY_FIELD.into(), Kind::Text)];
        out.extend(fields);
        if timestamps {
            out.push((CREATED.into(), Kind::Int));
            out.push((UPDATED.into(), Kind::Int));
        }
        let layout = Layout {
            id: u64::from(id),
            fields: out,
        };
        layout.descriptor().map_err(invalid)?;
        Ok(layout)
    }
    pub fn create_collection(
        &mut self,
        name: &str,
        fields: Vec<(String, Kind)>,
        options: CollectionOptions,
    ) -> Result<CollectionId> {
        self.ready_write()?;
        if name.is_empty() || name.len() > 255 {
            return Err(invalid("collection name must contain 1..255 UTF-8 bytes"));
        }
        if self.collection(name)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        let (cid, lid) = self.header()?;
        let next_c = cid
            .checked_add(1)
            .ok_or_else(|| invalid("collection IDs exhausted"))?;
        let next_l = lid
            .checked_add(1)
            .ok_or_else(|| invalid("layout IDs exhausted"))?;
        let layout = Self::make_layout(lid, fields, options.timestamps)?;
        let c = Catalog {
            id: CollectionId(cid),
            name: name.into(),
            layout: lid,
            timestamps: options.timestamps,
        };
        let result = (|| {
            self.persist_layout(&layout)?;
            self.persist_catalog(&c)?;
            self.write_sequence(c.id, 1)?;
            self.writer()?.put(&name_key(name), &cid.to_be_bytes())?;
            self.write_header(next_c, next_l)?;
            Ok(c.id)
        })();
        self.finish(result)
    }
    pub fn collection(&self, name: &str) -> Result<Option<CollectionId>> {
        let Some(b) = self.store()?.get(&name_key(name))? else {
            return Ok(None);
        };
        if b.len() != 4 {
            return Err(corrupt("collection-name mapping"));
        }
        let id = CollectionId(u32::from_be_bytes(b.try_into().unwrap()));
        if self.catalog(id)?.name != name {
            return Err(corrupt("collection-name identity"));
        }
        Ok(Some(id))
    }
    pub fn collection_info(&self, id: CollectionId) -> Result<CollectionInfo> {
        let c = self.catalog(id)?;
        let mut layout = self.layout(c.layout)?.as_ref().clone();
        layout.fields.remove(0);
        Ok(CollectionInfo {
            id,
            name: c.name,
            layout,
            timestamps: c.timestamps,
        })
    }
    pub fn alter_collection(
        &mut self,
        id: CollectionId,
        fields: Vec<(String, Kind)>,
    ) -> Result<u64> {
        self.ready_write()?;
        let mut c = self.catalog(id)?;
        let (next_c, next_l) = self.header()?;
        let layout = Self::make_layout(next_l, fields, c.timestamps)?;
        let after = next_l
            .checked_add(1)
            .ok_or_else(|| invalid("layout IDs exhausted"))?;
        c.layout = next_l;
        let result = (|| {
            self.persist_layout(&layout)?;
            self.persist_catalog(&c)?;
            self.write_header(next_c, after)?;
            Ok(u64::from(next_l))
        })();
        self.finish(result)
    }
    fn write_sequence(&mut self, c: CollectionId, next: u64) -> Result<()> {
        let b = packet(COUNTER_MAGIC, &next.to_be_bytes())?;
        for i in 0..3 {
            self.writer()?.put(&replica_key(2, c.0, i), &b)?;
        }
        Ok(())
    }
    fn flush_sequence(&mut self) -> Result<()> {
        if let Some(s) = self.sequence.take() {
            self.write_sequence(s.collection, s.next)?;
        }
        Ok(())
    }
    fn allocate(&mut self, c: CollectionId) -> Result<EntityId> {
        if !self.sequence.as_ref().is_some_and(|s| s.collection == c) {
            self.flush_sequence()?;
            let next = self.replicas(
                |i| replica_key(2, c.0, i),
                |b| {
                    let b = unpack(b, COUNTER_MAGIC)?;
                    if b.len() != 8 {
                        return Err(corrupt("sequence length"));
                    }
                    let n = u64::from_be_bytes(b.try_into().unwrap());
                    if n == 0 {
                        return Err(corrupt("zero sequence"));
                    }
                    Ok(n)
                },
            )?;
            self.sequence = Some(Sequence {
                collection: c,
                next,
            });
        }
        let s = self.sequence.as_mut().unwrap();
        let sequence = s.next;
        s.next = sequence
            .checked_add(1)
            .ok_or_else(|| invalid("entity sequence exhausted"))?;
        Ok(EntityId {
            collection: c,
            sequence,
        })
    }
    fn validate_document(&self, c: &Catalog, key: &str, doc: &Value) -> Result<()> {
        if key.is_empty() || key.len() > 1024 {
            return Err(invalid("external key must contain 1..1024 UTF-8 bytes"));
        }
        let obj = doc
            .as_object()
            .ok_or_else(|| invalid("document/patch must be an object"))?;
        if obj
            .keys()
            .any(|k| reserved(k) || (c.timestamps && matches!(k.as_str(), CREATED | UPDATED)))
        {
            return Err(invalid("caller supplied managed/reserved field"));
        }
        Ok(())
    }
    pub fn put(&mut self, c: CollectionId, key: &str, doc: &Value) -> Result<EntityId> {
        self.ready_write()?;
        let catalog = self.catalog(c)?;
        self.validate_document(&catalog, key, doc)?;
        let old = self.load_entity(c, key, true, false)?;
        self.write_entity(&catalog, key, doc.clone(), old)
    }
    pub fn update(&mut self, c: CollectionId, key: &str, patch: &Value) -> Result<EntityId> {
        self.ready_write()?;
        let catalog = self.catalog(c)?;
        self.validate_document(&catalog, key, patch)?;
        let old = self
            .load_entity(c, key, true, true)?
            .ok_or(Error::NotFound("entity"))?;
        let mut doc = old.entity.document.clone();
        for (k, v) in patch.as_object().unwrap() {
            doc.as_object_mut().unwrap().insert(k.clone(), v.clone());
        }
        self.write_entity(&catalog, key, doc, Some(old))
    }
    fn write_entity(
        &mut self,
        c: &Catalog,
        key: &str,
        mut doc: Value,
        old: Option<LoadedEntity>,
    ) -> Result<EntityId> {
        let layout = self.layout(c.layout)?;
        if c.timestamps {
            let now = self.clock.unix_seconds();
            if now < 0 {
                return Err(invalid("clock predates Unix epoch"));
            }
            let created = old.as_ref().map_or(Ok(now), |e| {
                e.entity.document[CREATED]
                    .as_i64()
                    .ok_or_else(|| corrupt("managed creation time"))
            })?;
            let previous = old.as_ref().map_or(Ok(now), |e| {
                e.entity.document[UPDATED]
                    .as_i64()
                    .ok_or_else(|| corrupt("managed update time"))
            })?;
            doc[CREATED] = Value::from(created);
            doc[UPDATED] = Value::from(now.max(created).max(previous));
        }
        doc[KEY_FIELD] = Value::from(key);
        let encoded = encode_dense_v3(&layout, &doc).map_err(invalid)?;
        if let Some(l) = self.limits {
            if encoded.row.len() + 32 > l.record_bytes as usize
                || encoded
                    .vectors
                    .iter()
                    .any(|(_, v)| v.len() + 40 > l.record_bytes as usize)
            {
                return Err(invalid("encoded entity exceeds a configured record limit"));
            }
        }
        let result = (|| {
            let id = if let Some(e) = &old {
                e.entity.id
            } else {
                self.allocate(c.id)?
            };
            // Sidecar identity is the physical field ordinal. Compare exact
            // encoded bytes, including float sign bits, across layout versions.
            // Only removed slots need deletes; replacements are ordinary puts.
            if let Some(e) = &old {
                for (field, _) in &e.vectors {
                    if !encoded.vectors.iter().any(|(f, _)| f == field) {
                        self.writer()?.delete(&vector_key(id, *field))?;
                    }
                }
            }
            for (field, bytes) in &encoded.vectors {
                let unchanged = old
                    .as_ref()
                    .is_some_and(|e| e.vectors.iter().any(|(f, b)| f == field && b == bytes));
                if !unchanged {
                    self.writer()?.put(&vector_key(id, *field), bytes)?;
                }
            }
            self.writer()?.put(&row_key(id), &encoded.row)?;
            if old.is_none() {
                self.writer()?
                    .put(&mapping_key(c.id, key), &ordered(id.sequence))?;
            }
            Ok(id)
        })();
        self.finish(result)
    }
    pub fn get(&self, c: CollectionId, key: &str) -> Result<Option<Entity>> {
        Ok(self.load_entity(c, key, false, true)?.map(|e| e.entity))
    }
    fn load_entity(
        &self,
        c: CollectionId,
        key: &str,
        capture_vectors: bool,
        render_vectors: bool,
    ) -> Result<Option<LoadedEntity>> {
        self.catalog(c)?;
        let Some(b) = self.store()?.get(&mapping_key(c, key))? else {
            return Ok(None);
        };
        let mut at = 0;
        let sequence = read_ordered(&b, &mut at)?;
        if at != b.len() || sequence == 0 {
            return Err(corrupt("external-key mapping"));
        }
        let id = EntityId {
            collection: c,
            sequence,
        };
        let row = self
            .store()?
            .get(&row_key(id))?
            .ok_or_else(|| corrupt("external key points to missing entity"))?;
        let mut vectors = Vec::new();
        let e = self.decode_with_vectors(
            id,
            &row,
            capture_vectors.then_some(&mut vectors),
            render_vectors,
        )?;
        if e.key != key {
            return Err(corrupt("external-key identity mismatch"));
        }
        Ok(Some(LoadedEntity { entity: e, vectors }))
    }
    pub fn get_by_id(&self, id: EntityId) -> Result<Option<Entity>> {
        self.catalog(id.collection)?;
        self.store()?
            .get(&row_key(id))?
            .map(|b| self.decode_entity(id, &b))
            .transpose()
    }
    fn decode_entity(&self, id: EntityId, b: &[u8]) -> Result<Entity> {
        self.decode_with_vectors(id, b, None, true)
    }
    fn decode_with_vectors(
        &self,
        id: EntityId,
        b: &[u8],
        mut vectors: Option<&mut VectorCells>,
        render_vectors: bool,
    ) -> Result<Entity> {
        let l = self.layout(layout_id(b)?)?;
        let mut doc = crate::dense_v3::decode_with_vector_values(&l, b, |field, dim| {
            let bytes = self
                .store()?
                .get(&vector_key(id, field))?
                .ok_or("missing vector sidecar")?;
            let value = if render_vectors {
                Some(crate::vector_json(&bytes, dim)?)
            } else {
                crate::visit_vector(&bytes, dim, |_| {})?;
                None
            };
            if let Some(out) = vectors.as_mut() {
                out.push((field, bytes));
            }
            Ok(value)
        })
        .map_err(corrupt)?;
        let key = doc
            .as_object_mut()
            .ok_or_else(|| corrupt("entity object"))?
            .remove(KEY_FIELD)
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or_else(|| corrupt("missing external key"))?;
        Ok(Entity {
            id,
            key,
            document: doc,
        })
    }
    pub fn delete(&mut self, c: CollectionId, key: &str) -> Result<bool> {
        self.ready_write()?;
        let Some(e) = self.load_entity(c, key, true, false)? else {
            return Ok(false);
        };
        let result = (|| {
            for (field, _) in e.vectors {
                self.writer()?.delete(&vector_key(e.entity.id, field))?;
            }
            self.writer()?.delete(&row_key(e.entity.id))?;
            self.writer()?.delete(&mapping_key(c, key))?;
            Ok(true)
        })();
        self.finish(result)
    }
    /// Stable ID order, exclusive cursor. Holds no collection-sized row vector.
    pub fn scan(&self, c: CollectionId, after: Option<EntityId>) -> Result<Scan<'_>> {
        self.catalog(c)?;
        if after.is_some_and(|id| id.collection != c) {
            return Err(invalid("cursor belongs to another collection"));
        }
        let start = after.map(row_key).unwrap_or_else(|| prefix(0x40, c));
        Ok(Scan {
            db: self,
            inner: self.store()?.range(&start)?,
            prefix: prefix(0x40, c),
            after,
            done: false,
        })
    }
    /// One atomic transaction, durable with a FULL barrier and published to
    /// every snapshot opened afterwards. The committed WAL is folded into the
    /// data file by `checkpoint`, automatically once it reaches 4 MiB (or half
    /// the remaining allowance) and no reader holds a slot.
    pub fn commit(&mut self) -> Result<()> {
        self.ready_write()?;
        let r = (|| {
            self.flush_sequence()?;
            self.writer()?.commit()?;
            Ok(())
        })();
        self.finish(r)
    }
    /// Fold committed pages into the data file and reset the WAL. Requires a
    /// committed handle. `Ok(false)` means a live reader (any process) holds a
    /// slot and the fold is deferred; committed data is unaffected either way.
    pub fn checkpoint(&mut self) -> Result<bool> {
        self.ready_write()?;
        if self.sequence.is_some() || self.store.is_dirty() {
            return Err(invalid("checkpoint requires commit"));
        }
        let r = self.writer()?.checkpoint();
        self.finish(r.map_err(Error::from))
    }
    /// Discard the uncommitted working tree in place by re-inspecting the
    /// durable files, exactly as a reopen would. Live snapshots are untouched.
    /// After an I/O error, publication may be uncertain: a commit frame that
    /// became durable before its barrier failed is reported as committed.
    pub fn rollback(&mut self) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        self.sequence = None;
        *self.catalog_cache.borrow_mut() = None;
        *self.layout_cache.borrow_mut() = None;
        self.failed = true;
        self.store.rollback()?;
        if let Some(l) = self.limits {
            self.store.install_limits(l)?;
        }
        self.failed = false;
        let validation = self.header().map(|_| ());
        self.finish(validation)
    }
}
fn reserved(name: &str) -> bool {
    matches!(name, KEY_FIELD | "_id" | "_key" | "_collection")
}
fn layout_id(row: &[u8]) -> Result<u32> {
    let mut r = crate::Read { b: row, p: 0 };
    u32::try_from(r.uv().map_err(corrupt)? >> 2).map_err(corrupt)
}

pub struct Scan<'a> {
    db: &'a Database,
    inner: RangeIter<'a>,
    prefix: Vec<u8>,
    after: Option<EntityId>,
    done: bool,
}
impl Iterator for Scan<'_> {
    type Item = Result<Entity>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            match self.inner.next()? {
                Err(e) => {
                    self.done = true;
                    return Some(Err(e.into()));
                }
                Ok((k, v)) => {
                    if !k.starts_with(&self.prefix) {
                        self.done = true;
                        return None;
                    }
                    let id = match row_id(&k) {
                        Ok(id) => id,
                        Err(e) => {
                            self.done = true;
                            return Some(Err(e));
                        }
                    };
                    if self.after.is_some_and(|after| id <= after) {
                        continue;
                    }
                    let row = self.db.decode_entity(id, &v);
                    if row.is_err() {
                        self.done = true;
                    }
                    return Some(row);
                }
            }
        }
    }
}

/// Root-independent candidate decoding reuses the existing R2 recovery engine.
/// Catalog names/policies remain separate evidence; this reports numeric identity.
/// Vector-bearing rows need a candidate-aware sidecar resolver and remain raw
/// if it is unavailable, rather than fabricating a vector or current membership.
pub struct CollectionRecovery;
impl crate::recovery::RecoveryCodec for CollectionRecovery {
    fn name(&self) -> &'static str {
        "collections-dense-v3"
    }
    fn classify(&self, key: &[u8]) -> crate::recovery::RecordClass {
        use crate::recovery::RecordClass;
        if key.len() == 10 && key[..2] == [0, 240] {
            RecordClass::Layout
        } else if row_id(key).is_ok() {
            RecordClass::Entity
        } else {
            RecordClass::Other
        }
    }
    fn layout_id(&self, row: &[u8]) -> crate::Result<u64> {
        Ok(u64::from(layout_id(row)?))
    }
    fn decode(
        &self,
        layout: &Layout,
        context: crate::recovery::CandidateContext<'_>,
        row: &[u8],
    ) -> crate::Result<Value> {
        let id = row_id(context.key)?;
        let mut document = decode_dense_v3(layout, row, |_| {
            Err("candidate vector sidecar not resolved".into())
        })?;
        let key = document
            .as_object_mut()
            .ok_or("entity object")?
            .remove(KEY_FIELD)
            .ok_or("external key slot")?;
        Ok(
            serde_json::json!({"collection_id":id.collection.0,"sequence":id.sequence,"key":key,"document":document}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::{
        io::IoMode,
        page::{PageKind, PageRef},
        store::SyncMode,
    };
    use serde_json::json;
    fn cfg() -> Config {
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        }
    }
    fn setup() -> (tempfile::TempDir, Database, CollectionId, EntityId) {
        assert!(
            (std::env::temp_dir().starts_with("<scratch>")
                || std::env::temp_dir()
                    .starts_with("<scratch>")
                || std::env::temp_dir()
                    .starts_with("<scratch>")
                || std::env::temp_dir().starts_with("<scratch>"))
        );
        let t = tempfile::tempdir().unwrap();
        let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
        let c = db
            .create_collection("c", vec![("v".into(), Kind::Vector(2))], Default::default())
            .unwrap();
        let id = db.put(c, "base", &json!({"v":[1.0,2.0],"n":1})).unwrap();
        db.commit().unwrap();
        (t, db, c, id)
    }
    #[test]
    fn scalar_update_spends_only_one_store_write() {
        let (t, mut db, c, id) = setup();
        let old = Database::open_snapshot(t.path().join("db"), cfg()).unwrap();
        db.store().unwrap().arm_write_fault(1);
        assert_eq!(db.update(c, "base", &json!({"n":2})).unwrap(), id);
        db.commit().unwrap();
        assert_eq!(
            old.get_by_id(id).unwrap().unwrap().document,
            json!({"v":[1.0,2.0],"n":1})
        );
        // The path-based reader holds only its slot: the writer can be closed
        // and reopened beside it, and it keeps serving its committed state.
        drop(db);
        let db = Database::open(t.path().join("db"), cfg()).unwrap();
        assert_eq!(
            db.get_by_id(id).unwrap().unwrap().document,
            json!({"v":[1.0,2.0],"n":2})
        );
        assert_eq!(
            old.get_by_id(id).unwrap().unwrap().document,
            json!({"v":[1.0,2.0],"n":1})
        );
    }
    #[test]
    fn replacement_compares_vector_bits_and_layout_ordinals() {
        let (t, mut db, c, id) = setup();
        db.alter_collection(
            c,
            vec![("w".into(), Kind::Vector(2)), ("v".into(), Kind::Vector(2))],
        )
        .unwrap();
        db.put(c, "base", &json!({"w":[3.0,4.0],"v":[1.0,2.0]}))
            .unwrap();
        db.commit().unwrap();
        let old = Database::open_snapshot(t.path().join("db"), cfg()).unwrap();
        db.alter_collection(
            c,
            vec![("v".into(), Kind::Vector(2)), ("w".into(), Kind::Vector(2))],
        )
        .unwrap();
        db.put(c, "base", &json!({"v":[-0.0,2.0],"w":[3.0,4.0]}))
            .unwrap();
        db.commit().unwrap();
        assert_eq!(
            old.get_by_id(id).unwrap().unwrap().document,
            json!({"w":[3.0,4.0],"v":[1.0,2.0]})
        );
        assert_eq!(
            &db.store()
                .unwrap()
                .get(&vector_key(id, 1))
                .unwrap()
                .unwrap()[..4],
            &(-0.0f32).to_le_bytes()
        );
        db.put(c, "base", &json!({"v":[0.0,2.0],"w":null})).unwrap();
        assert_eq!(
            &db.store()
                .unwrap()
                .get(&vector_key(id, 1))
                .unwrap()
                .unwrap()[..4],
            &0.0f32.to_le_bytes()
        );
        assert!(db
            .store()
            .unwrap()
            .get(&vector_key(id, 2))
            .unwrap()
            .is_none());
        db.alter_collection(c, vec![]).unwrap();
        db.put(c, "base", &json!({"n":3})).unwrap();
        assert!(db
            .store()
            .unwrap()
            .get(&vector_key(id, 1))
            .unwrap()
            .is_none());
        db.commit().unwrap();
        drop(db);
        let db = Database::open(t.path().join("db"), cfg()).unwrap();
        assert_eq!(db.get_by_id(id).unwrap().unwrap().document, json!({"n":3}));
        assert_eq!(
            old.get_by_id(id).unwrap().unwrap().document,
            json!({"w":[3.0,4.0],"v":[1.0,2.0]})
        );
    }
    #[test]
    fn missing_vector_cannot_be_hidden_by_replacement() {
        let (_t, mut db, c, id) = setup();
        db.writer().unwrap().delete(&vector_key(id, 1)).unwrap();
        assert!(db.put(c, "base", &json!({"v":[1.0,2.0],"n":2})).is_err());
    }
    #[test]
    fn mutation_vector_validation_matches_public_reads() {
        for bytes in [
            vec![],
            vec![0; 7],
            vec![0; 9],
            [f32::NAN.to_le_bytes(), 1.0f32.to_le_bytes()].concat(),
            [f32::INFINITY.to_le_bytes(), 1.0f32.to_le_bytes()].concat(),
            [f32::NEG_INFINITY.to_le_bytes(), 1.0f32.to_le_bytes()].concat(),
        ] {
            let (_t, mut db, c, id) = setup();
            db.writer()
                .unwrap()
                .put(&vector_key(id, 1), &bytes)
                .unwrap();
            let before = db.store().unwrap().get(&row_key(id)).unwrap();
            assert!(db.get(c, "base").is_err());
            assert!(db.put(c, "base", &json!({"v":[1.0,2.0]})).is_err());
            assert!(db.delete(c, "base").is_err());
            assert_eq!(db.store().unwrap().get(&row_key(id)).unwrap(), before);
        }
    }
    #[test]
    fn write_faults_never_publish_partial_multikey_changes() {
        for operation in [
            "insert",
            "replace",
            "delete",
            "alter",
            "create",
            "sequence-commit",
        ] {
            let steps = match operation {
                "insert" => 3,
                "replace" => 2,
                "delete" => 3,
                "alter" => 9,
                "create" => 13,
                _ => 3,
            };
            for fail_at in 0..steps {
                let (t, mut db, c, id) = setup();
                if operation == "sequence-commit" {
                    db.put(c, "new", &json!({"v":[3.0,4.0]})).unwrap();
                }
                db.store().unwrap().arm_write_fault(fail_at);
                let r = match operation {
                    "insert" => db.put(c, "new", &json!({"v":[3.0,4.0]})).map(|_| ()),
                    "replace" => db.put(c, "base", &json!({"v":[3.0,4.0]})).map(|_| ()),
                    "delete" => db.delete(c, "base").map(|_| ()),
                    "alter" => db.alter_collection(c, vec![]).map(|_| ()),
                    "create" => db
                        .create_collection("new", vec![], Default::default())
                        .map(|_| ()),
                    _ => db.commit(),
                };
                assert!(r.is_err(), "{operation} position {fail_at}");
                assert!(db.commit().is_err());
                assert!(db.get_by_id(id).is_err());
                let old = Database::open_snapshot(t.path().join("db"), cfg()).unwrap();
                assert_eq!(
                    old.get_by_id(id).unwrap().unwrap().document,
                    json!({"v":[1.0,2.0],"n":1})
                );
                // In-place rollback beside a live reader: the reader's frames
                // are all below the last commit and stay byte-stable.
                db.rollback().unwrap();
                assert_eq!(db.scan(c, None).unwrap().count(), 1);
                assert!(db.collection("new").unwrap().is_none());
                assert_eq!(
                    db.get_by_id(id).unwrap().unwrap().document,
                    json!({"v":[1.0,2.0],"n":1})
                );
                db.put(c, "usable", &json!({})).unwrap();
                db.commit().unwrap();
                assert_eq!(old.scan(c, None).unwrap().count(), 1);
                drop(old);
            }
        }
    }
    #[test]
    fn metadata_replica_loss_and_conflict_fail_safely() {
        for family in 0..4 {
            for losses in 1..=3 {
                let (t, db, c, _) = setup();
                let path = db.path.clone();
                drop(db);
                let mut s = PageWalStore::open(&path, false, 1 << 20).unwrap();
                for copy in 0..losses {
                    let k = match family {
                        0 => vec![0, 0, copy],
                        1 => replica_key(1, c.0, copy),
                        2 => layout_key(1, copy),
                        _ => replica_key(2, c.0, copy),
                    };
                    let mut b = s.get(&k).unwrap().unwrap();
                    b[PAD - 1] ^= 1;
                    s.put(&k, &b).unwrap();
                }
                s.commit().unwrap();
                drop(s);
                let result = (|| -> Result<()> {
                    let mut db = Database::open(&path, cfg())?;
                    db.get(c, "base")?;
                    db.put(c, "new", &json!({}))?;
                    Ok(())
                })();
                assert_eq!(
                    result.is_ok(),
                    losses < 3,
                    "family {family}, losses {losses}"
                );
                drop(t);
            }
        }
        let (_t, db, c, _) = setup();
        let path = db.path.clone();
        let mut cat = db.catalog(c).unwrap();
        drop(db);
        cat.name = "conflict".into();
        let mut s = PageWalStore::open(&path, false, 1 << 20).unwrap();
        s.put(&replica_key(1, c.0, 1), &catalog_bytes(&cat).unwrap())
            .unwrap();
        s.commit().unwrap();
        drop(s);
        assert!(Database::open(&path, cfg())
            .unwrap()
            .get(c, "base")
            .is_err());
    }
    #[test]
    fn alter_preserves_raw_rows_and_replacement_removes_old_vectors() {
        let (_t, mut db, c, id) = setup();
        let row = db.store().unwrap().get(&row_key(id)).unwrap().unwrap();
        db.alter_collection(c, vec![]).unwrap();
        db.commit().unwrap();
        assert_eq!(db.store().unwrap().get(&row_key(id)).unwrap().unwrap(), row);
        assert!(db
            .store()
            .unwrap()
            .get(&vector_key(id, 1))
            .unwrap()
            .is_some());
        db.put(c, "base", &json!({"n":2})).unwrap();
        db.commit().unwrap();
        assert!(db
            .store()
            .unwrap()
            .get(&vector_key(id, 1))
            .unwrap()
            .is_none());
    }
    #[test]
    fn failed_rollback_validation_keeps_handle_failed() {
        let (_t, mut db, c, id) = setup();
        for i in 0..3 {
            db.writer().unwrap().put(&[0, 0, i], b"broken").unwrap();
        }
        db.writer().unwrap().commit().unwrap();
        assert!(db.rollback().is_err());
        assert!(matches!(db.get_by_id(id), Err(Error::Failed)));
        assert!(db.put(c, "bad", &json!({})).is_err());
    }
    #[test]
    fn plain_header_payload_is_the_eight_byte_form_and_future_versions_refuse() {
        let (_t, db, _, _) = setup();
        let raw = db.store().unwrap().get(&[0, 0, 0]).unwrap().unwrap();
        let payload = unpack(&raw, HEADER_MAGIC).unwrap();
        assert_eq!(payload.len(), HEADER_PLAIN);
        let mut future = raw.clone();
        future[6] = b'9';
        let crc = crc32c::crc32c(&future[..PAD - 4]).to_le_bytes();
        future[PAD - 4..].copy_from_slice(&crc);
        assert!(matches!(parse_header(&future), Err(Error::Unsupported(_))));
        let mut longer = packet(HEADER_MAGIC, &[1; 20]).unwrap();
        assert!(matches!(parse_header(&longer), Err(Error::Unsupported(_))));
        longer[PAD - 1] ^= 1;
        assert!(matches!(parse_header(&longer), Err(Error::Corrupt(_))));
    }
    #[test]
    fn rootless_collection_recovery_preserves_source_and_reports_vector_limit() {
        let (t, mut db, c, _) = setup();
        db.put(c, "scalar", &json!({"n":2})).unwrap();
        db.commit().unwrap();
        assert!(db.checkpoint().unwrap());
        // Committed after the checkpoint: this row exists only in the WAL and
        // must still be exported through the committed-WAL overlay.
        db.put(c, "tail", &json!({"n":3})).unwrap();
        db.commit().unwrap();
        let path = db.path.clone();
        drop(db);
        let file = path.join("data");
        let mut bytes = std::fs::read(&file).unwrap();
        let mut metadata_pages =
            std::collections::BTreeMap::<u8, std::collections::BTreeSet<usize>>::new();
        for (no, page) in bytes.chunks_exact_mut(4096).enumerate() {
            let p = PageRef::open(page, no as u32).unwrap();
            if p.kind() == PageKind::Interior {
                page[50] ^= 1;
                continue;
            }
            if p.kind() != PageKind::Leaf {
                continue;
            }
            for i in 0..p.nentries() {
                let cell = p.slot(i);
                let kernel::verify::DecodedRecord::Leaf { key: k, .. } =
                    kernel::verify::decode_record(cell, no as u32, PageKind::Leaf).unwrap()
                else { panic!("leaf cell decoded as interior") };
                if k.starts_with(&[0, 0]) {
                    metadata_pages.entry(0).or_default().insert(no);
                }
                if k.starts_with(&[0, 240]) {
                    metadata_pages.entry(3).or_default().insert(no);
                }
                if matches!(k.first(), Some(1 | 2)) {
                    metadata_pages.entry(k[0]).or_default().insert(no);
                }
            }
        }
        for family in 0..4 {
            assert!(
                metadata_pages[&family].len() >= 3,
                "replicas share leaf for {family}"
            );
        }
        std::fs::write(&file, &bytes).unwrap();
        let before = ["data", "wal", "writer.lock"]
            .map(|name| std::fs::read(path.join(name)).unwrap_or_default());
        let out = t.path().join("recovered");
        let r = crate::recovery::recover_typed_candidates(
            &path,
            &out,
            &CollectionRecovery,
            Default::default(),
        )
        .unwrap();
        assert_eq!(r.decoded_records + r.unresolved_records, r.raw_records);
        assert!(r.decoded_records >= 2);
        assert!(r.unresolved_records >= 1);
        for name in ["data", "wal", "writer.lock"].into_iter().enumerate() {
            assert_eq!(
                std::fs::read(path.join(name.1)).unwrap_or_default(),
                before[name.0]
            );
        }
        let lines = std::fs::read_to_string(out.join("decoded.jsonl")).unwrap();
        let mut keys = std::collections::BTreeSet::new();
        for line in lines.lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["membership"], "candidate");
            let key = v["document"]["key"].as_str().unwrap().to_owned();
            let n = if key == "scalar" { 2 } else { 3 };
            assert_eq!(v["document"]["document"], json!({"n":n}));
            keys.insert(key);
        }
        assert_eq!(keys, ["scalar", "tail"].map(str::to_owned).into());
        let issues = std::fs::read_to_string(out.join("issues.jsonl")).unwrap();
        assert!(issues.contains("candidate vector sidecar not resolved"));
        assert!(!issues.contains("committed_wal_overlay_skipped"));
    }
}
