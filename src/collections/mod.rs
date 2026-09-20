//! Typed collections on the V2 page-WAL store. No JSON text is persisted.
//! Keys and immutable layouts are documented in docs/COLLECTIONS.md; the
//! storage path, its limits and its deviations in docs/V2_COLLECTION_INTEGRATION.md.
use crate::collection_backend::{check_config, check_limits, Backend};
use crate::pagewal::PageWalStore;
use crate::{decode_dense_v3, encode_dense_v3, Kind, Layout};
use kernel::{btree::RangeIter, limits::ResourceLimits, store::Config};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
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
    /// The caller's cancellation callback stopped a bounded exact query.
    Cancelled,
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
pub(crate) fn invalid(e: impl fmt::Display) -> Error {
    Error::InvalidInput(e.to_string())
}
pub(crate) fn corrupt(e: impl fmt::Display) -> Error {
    Error::Corrupt(e.to_string())
}
pub(crate) const KEY_FIELD: &str = "__e4_key";
const CREATED: &str = "_created_unix";
const UPDATED: &str = "_updated_unix";
pub(crate) const PAD: usize = 2081;
const HEADER_MAGIC: &[u8; 8] = b"E4COLL1\0";
const INDEX_HEADER_MAGIC: &[u8; 8] = b"E4COLL2\0";
pub(crate) mod catalog;
pub mod rebuild;
mod sort;
pub mod verification;
// The index families and the query engine live in `crate::index` and
// `crate::query`; every name this module has ever exported still leaves the
// crate through `e4_prototype::collections`.
pub use crate::index::graph::{
    BfsRequest, Direction, Edge, EdgeBudget, EdgeKey, EdgeTypeId, GraphContextId, NeighborRequest,
    TraversalNode, TraversalResult,
};
pub use crate::index::spatial::point::{SpatialCandidates, SpatialHit};
pub use crate::index::text::{TextCandidates, TextHit, TextMatch};
pub use crate::index::vector::exact::{VectorCandidates, VectorHit, VectorMetric};
pub use crate::index::vector::quantized::{
    ApproxVectorMethod, ApproxVectorResult, QuantizedVectorCandidates,
};
pub use crate::query::{
    ApproximationDiagnostics, CandidateDriver, Geom, GeometryFilter, OrderValue, OwnedScalarValue,
    PointFilter, PreparedQuery, ProjectedValue, Projection, QueryBudget, QueryDriver, QueryError,
    QueryFilter, QueryOrder, QueryPage, QueryRequest, QueryResult, QueryRow, QueryWork,
    ScalarFilter, ScalarValue, ScoreExpr, SortDirection, WorkResource,
};
pub use catalog::{
    create_index_trees, set_create_index_trees, IndexFamily, IndexId, IndexInfo, IndexState,
    IndexTree, ScalarPredicate,
};
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct IndexHeader {
    pub(crate) features: u64,
    pub(crate) next: u64,
    pub(crate) count: u32,
}
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
pub(crate) type VectorCells = Vec<(usize, Vec<u8>)>;
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
pub(crate) struct Catalog {
    id: CollectionId,
    name: String,
    pub(crate) layout: u32,
    timestamps: bool,
}
#[derive(Clone, Copy, Debug, PartialEq)]
struct HeaderInfo {
    next_collection: u32,
    next_layout: u32,
    limits: Option<ResourceLimits>,
    indexes: Option<IndexHeader>,
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
    pub(crate) limits: Option<ResourceLimits>,
    pub(crate) index_header: Option<IndexHeader>,
    /// The graph dictionary header, cached. It is three replica reads, it is
    /// read once per relationship written, and this handle is the only thing
    /// that can change it, so re-reading it per edge is three descents bought
    /// for nothing.
    pub(crate) graph_header_cache: Cell<Option<crate::index::graph::GraphHeader>>,
    /// Index descriptors this handle has already read, by id.
    ///
    /// `index_info` is four B-tree point-gets -- one registry probe and three
    /// descriptor replicas -- and a query pays it once per index it names,
    /// before it has looked at a single row. Measured on the 20,000-row
    /// fixture: preparing a query with ONE scalar index cost 5.5 us against
    /// 0.38 us for the same query with no index, so 5.1 us of a 8.1 us
    /// empty-result query was descriptor reads.
    ///
    /// INVALIDATION. The cache is per handle and lives only as long as the
    /// handle, so it cannot survive a reopen, and a snapshot handle opens its
    /// own. Within a handle it is emptied by [`Database::writer`] -- the one
    /// gate every mutation passes through, including the descriptor rewrites
    /// that an index build, a tree-root move, a create and a drop all perform
    /// -- and by [`Database::rollback`], which re-reads the durable files.
    /// Nothing else can change a descriptor under a live handle: the store is
    /// held exclusively, which is the same argument `graph_header_cache`
    /// above rests on.
    index_cache: RefCell<Vec<(catalog::IndexId, catalog::IndexInfo)>>,
    /// Identities this handle allocated since it opened, one entry per
    /// collection. Sequences are dense, monotonic and never reused (D13) and
    /// the allocator counter rides the commit, so a sequence handed out after
    /// open did not exist in any committed state: its row was written here,
    /// and no edge on disk can name it. That makes both the endpoint
    /// existence read and the forward/reverse probe before writing an edge on
    /// such an endpoint provably pointless -- the read-before-write D10
    /// forbids.
    ///
    /// This is a RANGE per collection, not a list of identities. A list has a
    /// bound, and past that bound the proof lapses and every write pays the
    /// four descents again -- a per-edge cost that grows with how much was
    /// loaded first, which is the shape the 1M multimodel run was paying.
    allocated: BTreeMap<CollectionId, Allocated>,
    /// Whether scalar and spatial indexes created through this handle get
    /// their own B-tree. Copied from the process-wide default at open/create.
    create_index_trees: bool,
    /// Whether the open transaction holds work the CALLER asked for, as
    /// opposed to work the engine did on the caller's behalf.
    ///
    /// A late build may have to roll back -- an allowance refusal is how it
    /// finds the transaction size the page-WAL admits -- and `rollback`
    /// discards everything uncommitted, not the build's share of it. Two
    /// different things can be sitting in that transaction:
    ///
    /// * INDEX CATALOG work: this index's own CREATE, or the READY flip of a
    ///   build the caller ran a moment ago and has not committed yet. Rolling
    ///   that away is how a refused build used to LOSE the index it was
    ///   building (`index_info` then answered `NotFound("index")`). The build
    ///   commits it before it starts -- it is the engine's own work, and
    ///   committing a finished index or a fresh empty one changes nothing a
    ///   reader can object to.
    /// * USER writes: rows, edges, collections, graph metadata. Committing
    ///   those decides something that is not the engine's to decide, and
    ///   discarding them is worse. The build refuses instead and says so.
    ///
    /// This flag is what tells the two apart. It is set by every public write
    /// entry point and cleared by `commit` and `rollback`; the engine's own
    /// index create/build/drop steps deliberately do not set it, which is what
    /// lets `create A, build A, create B, build B, commit` work -- a
    /// reasonable caller pattern that a plain "is the handle dirty" test
    /// refuses on the second build.
    user_writes_pending: bool,
}

/// One collection's worth of identities this handle handed out.
///
/// `live_from..next` is the range it allocated and has not given up on;
/// `deleted` names the ones inside that range it has since removed. A row is
/// provably live when it is in the range and not in that set.
struct Allocated {
    /// Lowest sequence still provable. It only ever rises, and only when
    /// `deleted` runs out of room.
    live_from: u64,
    /// One past the highest sequence handed out. An identity at or above this
    /// was never allocated, so the range says nothing about it.
    next: u64,
    /// Handle-allocated identities deleted since, named one by one while they
    /// fit.
    deleted: BTreeSet<u64>,
}

/// Deleted identities named individually, per collection.
///
/// SACRIFICE (Law 4): past this many deletes in one collection the handle
/// stops naming them and raises `live_from` above the highest instead, which
/// costs every older row the cheap path. It is the cost this code used to pay
/// unconditionally, so the fallback is never worse than the behaviour it
/// replaces, and it is what keeps the structure's RAM bounded (~3 MiB at the
/// bound, against ~4 MiB for the identity list it replaces) and proportional
/// to change rather than to the store.
const DELETED_IDENTITIES: usize = 1 << 17;

pub(crate) fn ordered(n: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(9);
    ordered_into(&mut key, n);
    key
}
/// The same frozen encoding, appended to a buffer the caller reuses. A late
/// build writes one of these per posting; allocating a `Vec` for each is a
/// per-row cost with nothing to show for it.
pub(crate) fn ordered_into(out: &mut Vec<u8>, n: u64) {
    let bytes = n.to_be_bytes();
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(7);
    out.push(0x80 + (8 - start) as u8);
    out.extend_from_slice(&bytes[start..]);
}
#[inline]
pub(crate) fn read_ordered(b: &[u8], at: &mut usize) -> Result<u64> {
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
    // A `copy_from_slice` of a RUNTIME width compiles to a `memcpy` call, and
    // the call costs more than the at-most-eight shifts it is standing in for.
    let mut out = 0u64;
    for b in bytes {
        out = (out << 8) | u64::from(*b);
    }
    *at += width;
    Ok(out)
}
/// Room for a tag and two widest integers, so a keyed lookup is one
/// allocation rather than one per component plus a regrow between them.
const TAGGED_PAIR_BYTES: usize = 1 + 9 + 9;
pub(crate) fn prefix(tag: u8, c: CollectionId) -> Vec<u8> {
    let mut k = Vec::with_capacity(TAGGED_PAIR_BYTES);
    k.push(tag);
    ordered_into(&mut k, c.0 as u64);
    k
}
pub(crate) fn row_key(id: EntityId) -> Vec<u8> {
    let mut k = prefix(0x40, id.collection);
    ordered_into(&mut k, id.sequence);
    k
}
/// True when `key` begins with `prefix`. The prefixes a scan matches per row
/// are a tag and one width-tagged integer -- three or four bytes -- and
/// `starts_with` hands those to the platform's `memcmp`, whose call overhead
/// is most of what a four-byte comparison costs. Counted on a 5M-row key-only
/// enumeration it was 4.6% of the whole walk.
#[inline]
pub(crate) fn has_prefix(key: &[u8], prefix: &[u8]) -> bool {
    if key.len() < prefix.len() {
        return false;
    }
    for (a, b) in key.iter().zip(prefix) {
        if a != b {
            return false;
        }
    }
    true
}
/// The entity id of a primary key whose `0x40 || collection` prefix the caller
/// has ALREADY matched byte for byte against a prefix built from
/// `collection`. The collection half is then not a question -- the bytes that
/// answer it are the bytes that were just compared -- so only the sequence is
/// decoded. `row_id` re-read the tag, re-decoded the collection width, and
/// re-checked a range that the prefix match had already settled, once per row.
#[inline]
pub(crate) fn row_id_after_prefix(k: &[u8], prefix_len: usize, collection: CollectionId) -> Result<EntityId> {
    let mut at = prefix_len;
    let sequence = read_ordered(k, &mut at)?;
    if at != k.len() || sequence == 0 {
        return Err(corrupt("entity identity"));
    }
    Ok(EntityId {
        collection,
        sequence,
    })
}
pub(crate) fn row_id(k: &[u8]) -> Result<EntityId> {
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
pub(crate) fn mapping_key(c: CollectionId, key: &str) -> Vec<u8> {
    let mut k = prefix(0x20, c);
    k.extend_from_slice(key.as_bytes());
    k
}
pub(crate) fn name_key(name: &str) -> Vec<u8> {
    let mut k = vec![0x10];
    k.extend_from_slice(name.as_bytes());
    k
}
fn replica_key(tag: u8, id: u32, copy: u8) -> Vec<u8> {
    let mut k = vec![tag, copy];
    k.extend(ordered(id as u64));
    k
}
pub(crate) fn layout_key(id: u32, copy: u8) -> Vec<u8> {
    let mut k = vec![0, 240];
    k.extend_from_slice(&(u64::from(id) * 3 + u64::from(copy)).to_be_bytes());
    k
}
pub(crate) fn vector_key(id: EntityId, field: usize) -> Vec<u8> {
    let mut k = row_key(id);
    k[0] = 0x60;
    k.extend(ordered(field as u64));
    k
}
pub(crate) fn packet(magic: &[u8; 8], payload: &[u8]) -> Result<Vec<u8>> {
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
pub(crate) fn unpack<'a>(b: &'a [u8], magic: &[u8; 8]) -> Result<&'a [u8]> {
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
/// Every logical index feature bit this binary implements, in one place.
///
/// The collection header carries the set of logical index features the file
/// actually uses. This binary opens a file only when that set is a subset of
/// this mask; a file that declares anything outside it is refused whole,
/// before a byte of it is read (Law 8). Bit 0 is the always-set "typed indexes
/// exist" marker; the rest are one bit per index family. It is `pub` because
/// the compatibility fixture binaries report it in their `--version` JSON, and
/// the fixtures must never be able to disagree with the engine about what this
/// build accepts.
pub const SUPPORTED_LOGICAL_FEATURES: u64 = 1
    | crate::index::graph::GRAPH_FEATURE
    | crate::index::vector::exact::VECTOR_FEATURE
    | crate::index::spatial::point::SPATIAL_FEATURE
    | crate::index::text::TEXT_FEATURE
    | crate::index::text::segments::SEGMENT_FEATURE
    | crate::index::vector::quantized::QUANTIZED_VECTOR_FEATURE
    | catalog::INDEX_TREE_FEATURE
    | crate::index::spatial::geometry_index::GEOMETRY_FEATURE;
fn header_bytes(h: HeaderInfo) -> Result<Vec<u8>> {
    let mut payload = h.next_collection.to_be_bytes().to_vec();
    payload.extend_from_slice(&h.next_layout.to_be_bytes());
    if let Some(i) = h.indexes {
        payload.extend_from_slice(&i.features.to_be_bytes());
        payload.extend_from_slice(&i.next.to_be_bytes());
        payload.extend_from_slice(&i.count.to_be_bytes());
    }
    if let Some(l) = h.limits {
        payload.extend(l.encode());
    }
    packet(
        if h.indexes.is_some() {
            INDEX_HEADER_MAGIC
        } else {
            HEADER_MAGIC
        },
        &payload,
    )
}
fn parse_header(b: &[u8]) -> Result<HeaderInfo> {
    if b.len() == PAD
        && b.starts_with(b"E4COLL")
        && &b[..8] != HEADER_MAGIC
        && &b[..8] != INDEX_HEADER_MAGIC
    {
        // An unknown version is authoritative only inside an intact packet.
        // A damaged magic byte must still allow an independent replica to win.
        unpack(b, b[..8].try_into().unwrap())?;
        return Err(Error::Unsupported(format!(
            "typed-collection header version {:?} is newer than this binary",
            String::from_utf8_lossy(&b[..7])
        )));
    }
    let indexed = b.get(..8) == Some(INDEX_HEADER_MAGIC.as_slice());
    let b = unpack(
        b,
        if indexed {
            INDEX_HEADER_MAGIC
        } else {
            HEADER_MAGIC
        },
    )?;
    let base = if indexed { 28 } else { HEADER_PLAIN };
    if b.len() != base && b.len() != base + (HEADER_LIMITED - HEADER_PLAIN) {
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
        limits: (b.len() != base)
            .then(|| decode_limits(&b[base..]))
            .transpose()?,
        indexes: if indexed {
            let features = u64::from_be_bytes(b[8..16].try_into().unwrap());
            if features & 1 == 0 || features & !SUPPORTED_LOGICAL_FEATURES != 0 {
                return Err(Error::Unsupported(format!(
                    "logical index features {features:#x}"
                )));
            }
            let next = u64::from_be_bytes(b[16..24].try_into().unwrap());
            let count = u32::from_be_bytes(b[24..28].try_into().unwrap());
            if next == 0 || u64::from(count) >= next {
                return Err(corrupt("index allocator/count"));
            }
            Some(IndexHeader {
                features,
                next,
                count,
            })
        } else {
            None
        },
    })
}
/// Agreeing intact copies win; a damaged page or descriptor loses one copy,
/// not all metadata. Conflicting valid copies are an error, never a vote. An
/// unsupported-but-intact copy is reported as such rather than as damage.
pub(crate) fn replicas<T: PartialEq>(
    mut get: impl FnMut(&[u8]) -> Result<Option<Vec<u8>>>,
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
    replicas(
        |k| s.get(k).map_err(Error::from),
        |i| vec![0, 0, i],
        parse_header,
    )
}
fn validate_features(s: &PageWalStore, header: Option<IndexHeader>) -> Result<()> {
    catalog::validate_catalog(s, header)?;
    crate::index::graph::validate_graph(
        s,
        header.is_some_and(|h| h.features & crate::index::graph::GRAPH_FEATURE != 0),
    )
}
/// The typed refusal the page-WAL runs before it normalizes or creates
/// anything. The precise collection error is kept in `detail`; the kernel
/// sees an opaque refusal.
fn typed_check(s: &PageWalStore, detail: &RefCell<Option<Error>>) -> kernel::Result<()> {
    match read_header(s).and_then(|h| validate_features(s, h.indexes)) {
        Ok(()) => Ok(()),
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
        let mut db = Self::wrap(store, false, limits, None);
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
        let mut db = Self::wrap(store, false, h.limits, h.indexes);
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
        let index_header = std::cell::Cell::new(None);
        // The typed check also hands the persisted reader bound to the
        // page-WAL, which enforces it on the slot this handle holds.
        let store = Backend::open_snapshot(path.as_ref(), cache, |s| {
            match read_header(s).and_then(|h| {
                validate_features(s, h.indexes)?;
                Ok(h)
            }) {
                Ok(h) => {
                    index_header.set(h.indexes);
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
            }
        });
        let store = match store {
            Ok(s) => s,
            Err(k) => return Err(detail.into_inner().unwrap_or(Error::Kernel(k))),
        };
        Ok(Self::wrap(store, true, limits.get(), index_header.get()))
    }
    fn wrap(
        store: Backend,
        read_only: bool,
        limits: Option<ResourceLimits>,
        index_header: Option<IndexHeader>,
    ) -> Self {
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
            index_header,
            graph_header_cache: Cell::new(None),
            index_cache: RefCell::new(Vec::new()),
            allocated: BTreeMap::new(),
            create_index_trees: catalog::create_index_trees(),
            user_writes_pending: false,
        }
    }
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }
    /// Whether scalar and spatial indexes created through this handle get
    /// their own B-tree.
    pub fn create_index_trees(&self) -> bool {
        self.create_index_trees
    }
    /// Choose whether scalar and spatial indexes created from now on through
    /// this handle own a tree. Existing indexes are unaffected.
    pub fn set_create_index_trees(&mut self, on: bool) {
        self.create_index_trees = on;
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
    /// Diagnostic only. Stream every persisted key/value under `prefix` in key
    /// order and return how many entries were seen. Derived-index equivalence
    /// tests need the exact persisted bytes; Law 1 refuses to hand them a `Vec`
    /// of a whole index, so the caller folds each entry as it arrives.
    #[doc(hidden)]
    pub fn raw_for_each(&self, prefix: &[u8], f: &mut dyn FnMut(&[u8], &[u8])) -> Result<u64> {
        let mut seen = 0;
        for row in self.store()?.range(prefix)? {
            let (k, v) = row?;
            if !k.starts_with(prefix) {
                break;
            }
            f(&k, &v);
            seen += 1;
        }
        Ok(seen)
    }
    /// Diagnostic only: every entry of ONE index under `prefix`, wherever that
    /// index keeps them.
    ///
    /// `raw_for_each` walks the primary tree, which is where a version-1 index
    /// lives. A version-2 index's entries are the same keys and the same
    /// values in its own tree, so this resolves the descriptor and scans
    /// there. It exists so a test can compare the two layouts' ENTRY SETS
    /// directly: same digest, different tree.
    #[doc(hidden)]
    pub fn index_for_each(
        &self,
        id: catalog::IndexId,
        prefix: &[u8],
        f: &mut dyn FnMut(&[u8], &[u8]),
    ) -> Result<u64> {
        let i = match self.index_info(id) {
            Ok(i) => i,
            Err(Error::NotFound(_)) => return self.raw_for_each(prefix, f),
            Err(e) => return Err(e),
        };
        let mut seen = 0;
        for row in self.index_range(&i, prefix)?.into_iter().flatten() {
            let (k, v) = row?;
            if !k.starts_with(prefix) {
                break;
            }
            f(&k, &v);
            seen += 1;
        }
        Ok(seen)
    }
    /// Diagnostic only: the tree id and root page of an index that owns a
    /// tree, or `None` when its entries live in the primary tree.
    #[doc(hidden)]
    pub fn index_tree(&self, id: catalog::IndexId) -> Result<Option<(u16, u32)>> {
        Ok(self.index_info(id)?.tree.map(|t| (t.id, t.root)))
    }
    /// Diagnostic only: `(tree_id, root)` for every index in this database
    /// that owns a tree. Empty on a database whose indexes all live in the
    /// primary tree, which is every database written before this format.
    #[doc(hidden)]
    pub fn index_trees(&self) -> Result<Vec<(u16, u32)>> {
        let mut out = Vec::new();
        let store = self.store()?;
        let mut ids = Vec::new();
        for row in store.range(&[catalog::REGISTRY])? {
            let (k, _) = row?;
            if k.first() != Some(&catalog::REGISTRY) {
                break;
            }
            let mut at = 1;
            ids.push(catalog::IndexId(read_ordered(&k, &mut at)?));
        }
        for id in ids {
            if let Some(t) = self.index_info(id)?.tree {
                out.push((t.id, t.root));
            }
        }
        Ok(out)
    }
    /// Diagnostic only: buffer-pool page accesses (hits + misses) since this
    /// handle opened. This is the unit the repo measures write cost in; it is
    /// exact and does not move with the machine.
    #[doc(hidden)]
    pub fn pool_accesses(&self) -> Result<u64> {
        Ok(self.store()?.store().pool_accesses())
    }
    /// Diagnostic only: buffer-pool (hits, misses, evictions, clock sweep
    /// steps). `pool_accesses` is their first two summed; a scan budget needs
    /// them apart, because a MISS is a pread and a checksum and a hit is a
    /// hash lookup.
    #[doc(hidden)]
    pub fn pool_counters(&self) -> Result<(u64, u64, u64, u64)> {
        Ok(self.store()?.store().pool_counters())
    }
    /// Diagnostic only: count the primary rows of one collection through the
    /// kernel's callback walk -- one pin and one validation per leaf, no
    /// allocation and no engine work at all. The floor a key-only page is
    /// measured against.
    #[doc(hidden)]
    pub fn diag_scan_for_each_ref(&self, c: CollectionId) -> Result<u64> {
        let prefix = prefix(0x40, c);
        let mut seen = 0u64;
        self.store()?.range(&prefix)?.for_each_ref(|key, _| {
            if !key.starts_with(&prefix) {
                return false;
            }
            seen += 1;
            true
        })?;
        Ok(seen)
    }
    /// Diagnostic only: the same rows through the PULL cursor the entity
    /// driver uses -- peek, decode the id, step -- with nothing else on top.
    /// The difference from `diag_scan_for_each_ref` is what the pull shape
    /// itself costs; the difference from a key-only page is the engine.
    #[doc(hidden)]
    pub fn diag_scan_pull(&self, c: CollectionId, decode_id: bool) -> Result<u64> {
        let prefix = prefix(0x40, c);
        let mut iter = self.store()?.range(&prefix)?;
        let mut seen = 0u64;
        loop {
            let ok = {
                let Some((key, _)) = iter.peek_ref()? else { break };
                if !key.starts_with(&prefix) {
                    break;
                }
                if decode_id {
                    row_id(key)?;
                }
                true
            };
            if !ok {
                break;
            }
            iter.step();
            seen += 1;
        }
        Ok(seen)
    }
    /// Diagnostic only: (hits, attempts) of the per-keyspace append hints.
    #[doc(hidden)]
    pub fn tag_hint_stats(&self) -> Result<(u64, u64, u64)> {
        Ok(self.store()?.store().tag_hint_stats())
    }
    /// Diagnostic only: monotonic page-WAL I/O counters (frames, fsyncs, bytes).
    #[doc(hidden)]
    pub fn io_counters(&self) -> Result<crate::pagewal::IoCounters> {
        Ok(self.store()?.store().io_counters())
    }
    pub(crate) fn store(&self) -> Result<&Backend> {
        if self.failed {
            return Err(Error::Failed);
        }
        Ok(&self.store)
    }
    pub(crate) fn writer(&mut self) -> Result<&mut Backend> {
        self.ready_write()?;
        // Every mutation this handle makes passes here, and a descriptor
        // rewrite is one of them. See `index_cache`.
        self.index_cache.borrow_mut().clear();
        Ok(&mut self.store)
    }
    /// [`Database::index_info`] through the per-handle descriptor cache.
    pub(crate) fn index_info_cached(&self, id: catalog::IndexId) -> Result<catalog::IndexInfo> {
        if let Some((_, info)) = self
            .index_cache
            .borrow()
            .iter()
            .find(|(cached, _)| *cached == id)
        {
            return Ok(info.clone());
        }
        let info = self.index_info(id)?;
        let mut cache = self.index_cache.borrow_mut();
        // One collection may hold `MAX_INDEXES`; beyond that a handle is
        // touching more indexes than one database is allowed to have, so the
        // cache starts again rather than growing without a bound.
        if cache.len() >= catalog::MAX_INDEXES {
            cache.clear();
        }
        cache.push((id, info.clone()));
        Ok(info)
    }
    /// A public write entry point: the caller is changing the database, not
    /// the engine. Everything `ready_write` refuses is still refused; what this
    /// adds is the record that the transaction now holds work only the caller
    /// can decide the fate of, which is what stops a late build from
    /// committing or rolling back someone else's rows. The engine's own index
    /// create/build/drop steps call `ready_write` and deliberately not this.
    pub(crate) fn user_write(&mut self) -> Result<()> {
        self.ready_write()?;
        self.user_writes_pending = true;
        Ok(())
    }
    pub(crate) fn ready_write(&self) -> Result<()> {
        self.store()?;
        if self.read_only {
            Err(Error::ReadOnly)
        } else {
            Ok(())
        }
    }
    pub(crate) fn finish<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub(crate) fn replicas<T: PartialEq>(
        &self,
        keys: impl Fn(u8) -> Vec<u8>,
        parse: impl Fn(&[u8]) -> Result<T>,
    ) -> Result<T> {
        let store = self.store()?;
        replicas(|k| store.get(k).map_err(Error::from), keys, parse)
    }
    pub(crate) fn header(&self) -> Result<(u32, u32)> {
        let h = self.replicas(|i| vec![0, 0, i], parse_header)?;
        Ok((h.next_collection, h.next_layout))
    }
    pub(crate) fn write_header(&mut self, collection: u32, layout: u32) -> Result<()> {
        let b = header_bytes(HeaderInfo {
            next_collection: collection,
            next_layout: layout,
            limits: self.limits,
            indexes: self.index_header,
        })?;
        for i in 0..3 {
            self.writer()?.put(&[0, 0, i], &b)?;
        }
        Ok(())
    }
    pub(crate) fn catalog(&self, id: CollectionId) -> Result<Catalog> {
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
    pub(crate) fn layout(&self, id: u32) -> Result<Arc<Layout>> {
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
        self.user_write()?;
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
        self.validate_indexed_layout(id, &layout)?;
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
        let id = EntityId {
            collection: c,
            sequence,
        };
        self.note_allocated(id);
        Ok(id)
    }
    /// One past the highest sequence this collection has ever handed out,
    /// read without allocating one. Every entity the collection has ever
    /// held -- live or since deleted -- has a sequence below this, so it
    /// bounds a per-sequence structure (a query's range bitmap) sized before
    /// any row is read, with no scan.
    ///
    /// The durable counter (`write_sequence`) is the source of truth across
    /// handles; a handle with its own uncommitted allocation in flight for
    /// this collection (`self.sequence`) is ahead of that counter until its
    /// next commit, so it is checked first, matching what `allocate` itself
    /// would hand out next.
    pub(super) fn collection_span(&self, c: CollectionId) -> Result<u64> {
        if let Some(s) = self.sequence.as_ref().filter(|s| s.collection == c) {
            return Ok(s.next);
        }
        self.replicas(
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
        )
    }
    /// Extend this collection's allocated range to cover a just-handed-out
    /// identity. `allocate` is the only place sequences are handed out and it
    /// hands them out in order, so the range never needs to grow downwards.
    fn note_allocated(&mut self, id: EntityId) {
        let e = self.allocated.entry(id.collection).or_insert(Allocated {
            live_from: id.sequence,
            next: id.sequence,
            deleted: BTreeSet::new(),
        });
        e.next = e.next.max(id.sequence.saturating_add(1));
    }
    /// Take a deleted row out of the provable set. This is the ONLY row-delete
    /// site in the engine, and forgetting here is what lets
    /// `endpoint_known_live` read the range as "the row is there".
    fn note_deleted(&mut self, id: EntityId) {
        let Some(e) = self.allocated.get_mut(&id.collection) else {
            return;
        };
        if id.sequence < e.live_from || id.sequence >= e.next {
            return;
        }
        e.deleted.insert(id.sequence);
        if e.deleted.len() > DELETED_IDENTITIES {
            let highest = e.deleted.iter().next_back().copied().unwrap_or(e.live_from);
            e.live_from = highest.saturating_add(1);
            e.deleted.clear();
        }
    }
    /// True when this handle wrote the endpoint's row and has not deleted it.
    /// `allocate` records the identity, `write_entity` writes its row in the
    /// same poisoned-on-error closure, `delete` records the removal, and a
    /// rollback clears the map -- so a sequence inside the live range means
    /// the row is on the tree and a `get` to confirm it is a read that already
    /// knows its own answer.
    ///
    /// The upper bound is not decoration. A sequence at or above `next` was
    /// never handed out, so nothing wrote its row, and reading the range
    /// without it would report a row that is not there as live.
    pub(super) fn endpoint_known_live(&self, id: EntityId) -> bool {
        self.allocated.get(&id.collection).is_some_and(|e| {
            (e.live_from..e.next).contains(&id.sequence)
                // A handle that has deleted nothing asks nothing. The probe is
                // for the exceptions, and most writers have none.
                && (e.deleted.is_empty() || !e.deleted.contains(&id.sequence))
        })
    }
    /// True when the forward/reverse probe before writing this edge cannot
    /// report anything, because one of its endpoints is a row this handle
    /// allocated.
    ///
    /// SACRIFICE (Law 4), widened from "and no edge with this key written on
    /// it since": the probe's whole product is a corruption report about a
    /// pair this call is on its way to overwrite, and on a handle-allocated
    /// endpoint the only bytes it could read are ones this same handle wrote
    /// in this session, already checksummed on the way in (Law 5). Tracking
    /// which keys those were cost a list per identity and a bound on how many
    /// identities could be tracked at all; the report it bought back is one
    /// `index_verifier` still produces. What is lost is an early warning on
    /// this engine's own fresh writes, not a repair and not a refusal.
    pub(super) fn edge_provably_absent(&self, key: EdgeKey) -> bool {
        self.endpoint_known_live(key.source) || self.endpoint_known_live(key.destination)
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
        self.user_write()?;
        let catalog = self.catalog(c)?;
        self.validate_document(&catalog, key, doc)?;
        let old = self.load_entity(c, key, true, false)?;
        self.write_entity(&catalog, key, doc.clone(), old)
    }
    pub fn update(&mut self, c: CollectionId, key: &str, patch: &Value) -> Result<EntityId> {
        self.user_write()?;
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
            self.maintain_indexes(
                id,
                old.as_ref().map(|e| &e.entity.document),
                Some(&doc),
                Some((&layout, &encoded.vectors)),
            )?;
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
        self.user_write()?;
        let Some(e) = self.load_entity(c, key, true, false)? else {
            return Ok(false);
        };
        let result = (|| {
            self.cascade_graph_delete(e.entity.id)?;
            self.maintain_indexes(e.entity.id, Some(&e.entity.document), None, None)?;
            for (field, _) in e.vectors {
                self.writer()?.delete(&vector_key(e.entity.id, field))?;
            }
            self.writer()?.delete(&row_key(e.entity.id))?;
            self.writer()?.delete(&mapping_key(c, key))?;
            self.note_deleted(e.entity.id);
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
        self.user_writes_pending = false;
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
        self.graph_header_cache.set(None);
        self.index_cache.borrow_mut().clear();
        // A rollback rewinds the allocator, so a sequence handed out before it
        // can be handed out again. Everything the map claims about those ids
        // was learned in the discarded transaction; drop the lot.
        self.allocated.clear();
        self.user_writes_pending = false;
        self.failed = true;
        self.store.rollback()?;
        if let Some(l) = self.limits {
            self.store.install_limits(l)?;
        }
        self.failed = false;
        let validation = read_header(self.store.store()).and_then(|h| {
            validate_features(self.store.store(), h.indexes)?;
            self.index_header = h.indexes;
            Ok(())
        });
        self.finish(validation)
    }
}
pub(crate) fn reserved(name: &str) -> bool {
    matches!(name, KEY_FIELD | "_id" | "_key" | "_collection")
}
pub(crate) fn layout_id(row: &[u8]) -> Result<u32> {
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
                else {
                    panic!("leaf cell decoded as interior")
                };
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
    /// The one number the fixture binaries and the compatibility driver both
    /// quote. Bits 0x40 (packed text posting segments) and 0x80 (per-index
    /// B-trees) were added after the mask had stood at 63 for five families,
    /// and the fixtures kept reporting 63 until the replay caught it. Naming
    /// the mask once and asserting its value here is what stops that drift:
    /// a new family bit fails this test until every reporter is updated.
    #[test]
    fn supported_logical_feature_mask_is_the_only_definition() {
        assert_eq!(SUPPORTED_LOGICAL_FEATURES, 0x1ff);
        let header = |features| {
            header_bytes(HeaderInfo {
                next_collection: 1,
                next_layout: 1,
                limits: None,
                indexes: Some(IndexHeader {
                    features,
                    next: 1,
                    count: 0,
                }),
            })
            .unwrap()
        };
        // A file that declares exactly what this binary implements opens.
        assert_eq!(
            parse_header(&header(SUPPORTED_LOGICAL_FEATURES))
                .unwrap()
                .indexes
                .unwrap()
                .features,
            0x1ff
        );
        // One bit past the mask is a future family: refused whole, and as
        // Unsupported rather than corruption, because the bytes are intact.
        assert!(matches!(
            parse_header(&header(SUPPORTED_LOGICAL_FEATURES | 0x200)),
            Err(Error::Unsupported(m)) if m.contains("0x3ff")
        ));
    }
}
